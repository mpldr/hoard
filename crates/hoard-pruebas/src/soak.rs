//! El escenario largo: juegos falsos jugando contra el servicio de verdad.
//!
//! `bench` mide el motor en línea recta; esto lo mide con el mundo encima —
//! watchers, correlación, veto de sesión, planificador, el servicio con sus
//! reinicios. Es donde salen los fallos que solo aparecen "a ratos", porque
//! dependen de en qué orden llegaron dos cosas.
//!
//! Todo lo que crea lleva el prefijo `pruebas-`, y `hoard-pruebas purgar` lo
//! borra aunque este mando muera a medias: un save de banco olvidado en el
//! `state.json` del usuario sigue sincronizando para siempre.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hoard_core::ipc::{AgentEvent, Request};
use hoardd::client::{Client, Push};

use crate::fixture::{self, Shape};
use crate::report::fmt_ms;
use crate::{fakegame, session};

pub struct SoakArgs {
    pub juegos: usize,
    pub duracion: u64,
    pub sesion: u64,
    pub escribe_cada: u64,
    pub shape: Shape,
    pub scale: f64,
    pub workdir: PathBuf,
    pub server: Option<String>,
    pub token: Option<String>,
    pub keep: bool,
}

/// Lo que el banco vio pasar, por save.
#[derive(Debug, Default)]
struct Cuenta {
    arrancados: usize,
    parados: usize,
    backups_ok: usize,
    backups_fallidos: usize,
    restores_ok: usize,
    restores_fallidos: usize,
    aplazados: usize,
    throttled: usize,
    /// De `GameStopped` a `BackupSuccess`: cuánto tarda en quedar guardado lo
    /// que acabas de jugar. Es la métrica que un usuario nota.
    latencias_ms: Vec<u64>,
}

pub async fn run(args: SoakArgs) -> Result<()> {
    let active = session::resolve(args.server.clone(), args.token.clone()).await?;
    println!("servidor: {}", active.server);

    // El servicio tiene que estar levantado: este escenario mide justo eso.
    let endpoint = hoardd::endpoint::Endpoint::resolve()?;
    let mut ctrl = Client::ensure_running(&endpoint, "hoard-pruebas")
        .await
        .context("no pude hablar con el servicio (hoardd)")?;
    let estado = ctrl.status().await?;
    println!(
        "servicio: v{} · motor {} · {} slots · notificaciones nativas: {}",
        ctrl.welcome().daemon_version,
        if estado.engine.running {
            "vivo".to_string()
        } else {
            // Un motor caído invalida el escenario entero: sin él no hay quien
            // haga los backups que vamos a contar. Se dice con el motivo, que
            // es justo lo que D.11/D.12 costó tener.
            format!(
                "PARADO ({})",
                estado.engine.last_error.as_deref().unwrap_or("sin motivo")
            )
        },
        estado.slots.len(),
        estado.notifications
    );

    // ---- montar los juegos --------------------------------------------
    let nombres = fakegame::nombres(args.juegos);
    let bindir = args.workdir.join("bin");
    let mut saves: Vec<(String, String, PathBuf)> = Vec::new(); // (save_id, nombre, carpeta)

    for nombre in &nombres {
        let carpeta = args.workdir.join("partidas").join(nombre);
        fixture::generate(args.shape, args.scale, hash_semilla(nombre), &carpeta)?;
        let slug = format!("pruebas-{nombre}");
        let outcome = hoard_agent::library::add_to_tracking(
            &active.client,
            hoard_agent::library::AddGameArgs {
                slot: None,
                repoint: false,
                game_slug: slug.clone(),
                label: Some(format!("pruebas-{nombre}")),
                local_path: carpeta.to_string_lossy().to_string(),
                display_name: Some(format!("Pruebas {nombre}")),
                steam_app_id: None,
                preset: None,
                shared_processes: false,
                // Atar el save al nombre del proceso: sin esto dependeríamos
                // de que la correlación adivine, que es otra prueba distinta.
                processes: Some(vec![nombre.clone()]),
            },
        )
        .await
        .with_context(|| format!("registrando {nombre}"))?;
        saves.push((
            outcome.tracked.save_id.clone(),
            nombre.clone(),
            carpeta.clone(),
        ));
        fakegame::prepare_binary(&bindir, nombre)?;
    }
    println!(
        "{} juegos montados en {}",
        saves.len(),
        args.workdir.display()
    );

    // El daemon es el dueño del estado: se le avisa, no se le manda la lista.
    ctrl.request(Request::Reload).await?;

    // ---- escuchar el journal ------------------------------------------
    // Desde el cursor actual: lo de antes es historia del usuario, no del
    // banco, y contarla falsearía los totales.
    let mut oyente = Client::connect(&endpoint, "hoard-pruebas-journal").await?;
    let backlog = oyente.subscribe(None).await?;
    let desde = backlog.cursor;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let escucha = tokio::spawn(async move {
        loop {
            match oyente.next_push().await {
                Ok(Some(Push::Event(entry))) => {
                    if entry.seq > desde && tx.send(entry).is_err() {
                        break;
                    }
                }
                Ok(Some(Push::Resync { dropped, .. })) => {
                    tracing::warn!(dropped, "el journal descartó filas: el banco va detrás");
                }
                Ok(Some(Push::Goodbye { reason })) => {
                    tracing::warn!(%reason, "el servicio se despidió");
                    break;
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "se cortó el journal");
                    break;
                }
            }
        }
    });

    // ---- jugar ---------------------------------------------------------
    let hasta = Instant::now() + Duration::from_secs(args.duracion);
    let mut cuentas: HashMap<String, Cuenta> = HashMap::new();
    let mut parados_en: HashMap<String, Instant> = HashMap::new();
    let mut vuelta = 0usize;
    let mut incidencias: Vec<String> = Vec::new();

    while Instant::now() < hasta {
        vuelta += 1;
        println!(
            "\n— vuelta {vuelta} · quedan {} —",
            fmt_ms(hasta.saturating_duration_since(Instant::now()).as_millis() as u64)
        );

        // Todos a la vez: la concurrencia es parte de lo que se prueba.
        let mut hijos = Vec::new();
        for (_, nombre, carpeta) in &saves {
            match fakegame::spawn(&bindir, nombre, carpeta, args.sesion, args.escribe_cada) {
                Ok(child) => hijos.push((nombre.clone(), child)),
                Err(e) => incidencias.push(format!("no arrancó {nombre}: {e:#}")),
            }
        }
        println!("  {} juegos jugando {}s", hijos.len(), args.sesion);

        // Mientras juegan, ir vaciando el journal: si se deja para el final el
        // canal crece y las latencias salen mentidas.
        let fin_sesion = Instant::now() + Duration::from_secs(args.sesion + 2);
        while Instant::now() < fin_sesion {
            match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
                Ok(Some(entry)) => anotar(
                    &entry.event,
                    &mut cuentas,
                    &mut parados_en,
                    &mut incidencias,
                ),
                Ok(None) => break,
                Err(_) => {}
            }
        }

        for (nombre, mut child) in hijos {
            if let Err(e) = child.wait() {
                incidencias.push(format!("esperando a {nombre}: {e}"));
            }
        }

        // Ventana de calma: aquí es donde el motor tiene que hacer su trabajo
        // (asentarse el filesystem, disparar el backup por `GameStopped`).
        let calma = Instant::now() + Duration::from_secs(20);
        while Instant::now() < calma {
            match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
                Ok(Some(entry)) => anotar(
                    &entry.event,
                    &mut cuentas,
                    &mut parados_en,
                    &mut incidencias,
                ),
                Ok(None) => break,
                Err(_) => {}
            }
        }
    }

    // Un último barrido del canal.
    while let Ok(entry) = rx.try_recv() {
        anotar(
            &entry.event,
            &mut cuentas,
            &mut parados_en,
            &mut incidencias,
        );
    }
    escucha.abort();

    // ---- informe -------------------------------------------------------
    resumen(&saves, &cuentas, &incidencias);

    if !args.keep {
        limpiar(&active.client, &saves).await;
        ctrl.request(Request::Reload).await.ok();
    } else {
        println!(
            "\n--keep: los saves de pruebas siguen registrados. `hoard-pruebas purgar` los quita."
        );
    }
    Ok(())
}

fn anotar(
    event: &AgentEvent,
    cuentas: &mut HashMap<String, Cuenta>,
    parados_en: &mut HashMap<String, Instant>,
    incidencias: &mut Vec<String>,
) {
    match event {
        AgentEvent::GameStarted { save_id, .. } => {
            cuentas.entry(save_id.clone()).or_default().arrancados += 1;
        }
        AgentEvent::GameStopped { save_id, .. } => {
            cuentas.entry(save_id.clone()).or_default().parados += 1;
            parados_en.insert(save_id.clone(), Instant::now());
        }
        AgentEvent::BackupSuccess { save_id, .. } => {
            let c = cuentas.entry(save_id.clone()).or_default();
            c.backups_ok += 1;
            if let Some(t) = parados_en.remove(save_id) {
                c.latencias_ms.push(t.elapsed().as_millis() as u64);
            }
        }
        AgentEvent::BackupFailed {
            save_id,
            error,
            game_slug,
            ..
        } => {
            cuentas.entry(save_id.clone()).or_default().backups_fallidos += 1;
            incidencias.push(format!("backup falló ({game_slug}): {error}"));
        }
        AgentEvent::BackupThrottled { save_id, .. } => {
            cuentas.entry(save_id.clone()).or_default().throttled += 1;
        }
        AgentEvent::SaveAutoRestored { save_id, .. } => {
            cuentas.entry(save_id.clone()).or_default().restores_ok += 1;
        }
        AgentEvent::SaveAutoRestoreFailed {
            save_id,
            error,
            game_slug,
        } => {
            cuentas
                .entry(save_id.clone())
                .or_default()
                .restores_fallidos += 1;
            incidencias.push(format!("restore falló ({game_slug}): {error}"));
        }
        AgentEvent::SaveAutoRestoreStuck { game_slug, .. } => {
            incidencias.push(format!("restore atascado: {game_slug}"));
        }
        AgentEvent::RestoreDeferred { save_id, .. } => {
            cuentas.entry(save_id.clone()).or_default().aplazados += 1;
        }
        AgentEvent::BackupTooLarge {
            game_slug, plan, ..
        } => {
            incidencias.push(format!("{game_slug}: no cabe en el plan {plan}"));
        }
        _ => {}
    }
}

fn resumen(
    saves: &[(String, String, PathBuf)],
    cuentas: &HashMap<String, Cuenta>,
    incidencias: &[String],
) {
    println!("\n=== resumen ===");
    println!(
        "{:<16} {:>7} {:>7} {:>8} {:>8} {:>9} {:>10}",
        "juego", "start", "stop", "backups", "fallos", "aplazado", "latencia p50"
    );
    println!("{}", "-".repeat(70));
    let vacia = Cuenta::default();
    let mut sin_backup = Vec::new();
    for (save_id, nombre, _) in saves {
        let c = cuentas.get(save_id).unwrap_or(&vacia);
        let mut lat = c.latencias_ms.clone();
        lat.sort_unstable();
        let p50 = lat.get(lat.len() / 2).copied().unwrap_or(0);
        println!(
            "{:<16} {:>7} {:>7} {:>8} {:>8} {:>9} {:>10}",
            nombre,
            c.arrancados,
            c.parados,
            c.backups_ok,
            c.backups_fallidos + c.restores_fallidos,
            c.aplazados,
            if p50 == 0 { "—".into() } else { fmt_ms(p50) }
        );
        if c.backups_ok == 0 {
            sin_backup.push(nombre.clone());
        }
    }

    if !sin_backup.is_empty() {
        // El hallazgo más valioso del escenario: un juego que jugó y del que
        // no quedó ni una copia. Va aparte porque en la tabla se pierde.
        println!("\n⚠ jugaron y NO se guardó nada: {}", sin_backup.join(", "));
    }
    if !incidencias.is_empty() {
        println!("\nincidencias ({}):", incidencias.len());
        // Agrupadas: veinte veces el mismo 429 es una incidencia, no veinte.
        let mut vistas: HashMap<&str, usize> = HashMap::new();
        for i in incidencias {
            *vistas.entry(i.as_str()).or_default() += 1;
        }
        let mut filas: Vec<_> = vistas.into_iter().collect();
        filas.sort_by_key(|f| std::cmp::Reverse(f.1));
        for (texto, veces) in filas.iter().take(20) {
            println!("  ×{veces:<4} {texto}");
        }
    }
}

async fn limpiar(client: &hoard_agent::api::ApiClient, saves: &[(String, String, PathBuf)]) {
    print!("\nlimpiando {} saves de pruebas… ", saves.len());
    use std::io::Write;
    std::io::stdout().flush().ok();
    let mut fallos = 0;
    for (save_id, _, _) in saves {
        if hoard_agent::library::delete_completely(client, save_id)
            .await
            .is_err()
        {
            // Si el server dice que no, al menos que no siga vigilándolo.
            if hoard_agent::library::untrack(save_id).is_err() {
                fallos += 1;
            }
        }
    }
    if fallos == 0 {
        println!("hecho");
    } else {
        println!("{fallos} sin limpiar — `hoard-pruebas purgar`");
    }
}

fn hash_semilla(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x1000_0000_01b3)
    })
}
