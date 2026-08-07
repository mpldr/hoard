//! El guion de medición: sube, restaura frío, restaura caliente, muta y
//! vuelve a medir.
//!
//! Llama a `hoard_agent` directamente en vez de lanzar la CLI: lo que se
//! quiere medir es el motor, y un `Command::spawn` metería por medio el
//! arranque del proceso, el handshake IPC y el pintado de la barra de
//! progreso. El servicio se prueba en `soak`, que es otra pregunta.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};

use hoard_agent::api::ApiClient;
use hoard_agent::backup::upload_directory;
use hoard_agent::restore::{download_snapshot, RestoreOptions, RestoreOutcome};

use crate::fixture::{self, Mutation, Shape};
use crate::report::{fmt_bytes, fmt_ms, Failure, Report, Sample, Step};
use crate::session;

pub struct BenchArgs {
    pub shapes: Vec<Shape>,
    pub scale: f64,
    pub rounds: usize,
    pub seed: u64,
    pub workdir: PathBuf,
    pub concurrency: Vec<usize>,
    pub server: Option<String>,
    pub token: Option<String>,
    pub keep: bool,
    pub json: Option<PathBuf>,
}

pub async fn run(args: BenchArgs) -> Result<()> {
    let active = session::resolve(args.server.clone(), args.token.clone()).await?;
    let client = &active.client;
    println!("servidor: {}", active.server);
    println!(
        "formas: {} · vueltas: {} · escala: {} · concurrencia: {:?}",
        args.shapes
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        args.rounds,
        args.scale,
        args.concurrency
    );

    let mut report = Report {
        server: active.server.clone(),
        started_at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        ..Default::default()
    };

    std::fs::create_dir_all(&args.workdir)
        .with_context(|| format!("creando {}", args.workdir.display()))?;

    // Los saves creados se apuntan según se crean, no al final: si el banco
    // muere a medias, la lista sigue en el informe y se pueden borrar a mano
    // en vez de quedar colgados ocupando cuota.
    let mut created: Vec<String> = Vec::new();

    for &conc in &args.concurrency {
        // El motor lee el fan-out del entorno en cada restore, así que barrer
        // el rango es ponerlo aquí y volver a llamar.
        std::env::set_var("HOARD_RESTORE_CONCURRENCY", conc.to_string());

        for &shape in &args.shapes {
            for round in 0..args.rounds {
                // Contenido ÚNICO por (concurrencia, vuelta). Reutilizar el
                // mismo corpus parecía el ahorro obvio y arruina la medida: el
                // almacenamiento es content-addressed, así que la segunda vuelta
                // encuentra sus blobs ya subidos y el "backup frío" pasa de 4,6 s
                // a 630 ms sin haber movido un byte. Medía el dedup y lo
                // etiquetaba como subida.
                let semilla = args
                    .seed
                    .wrapping_add((conc as u64).wrapping_mul(1_000_003))
                    .wrapping_add(round as u64);
                let src =
                    args.workdir
                        .join(format!("origen-{}-c{}-r{}", shape.as_str(), conc, round));
                print!(
                    "generando {} (escala {}, vuelta {round})… ",
                    shape.as_str(),
                    args.scale
                );
                use std::io::Write;
                std::io::stdout().flush().ok();
                let fx = fixture::generate(shape, args.scale, semilla, &src)?;
                println!("{} ficheros, {}", fx.files, fmt_bytes(fx.bytes));

                let res = round_once(
                    client,
                    shape,
                    &src,
                    &args,
                    conc,
                    round,
                    &mut report,
                    &mut created,
                )
                .await;
                if let Err(e) = res {
                    // Una vuelta rota no puede llevarse el barrido entero: lo
                    // caro ya está gastado (generar y subir).
                    eprintln!("  vuelta {round} abortada: {e:#}");
                    report.failures.push(Failure {
                        shape: shape.as_str().into(),
                        step: Step::BackupCold,
                        concurrency: conc,
                        round,
                        error: format!("{e:#}"),
                    });
                }
                if !args.keep {
                    // Un corpus por vuelta multiplica el disco por el número de
                    // vueltas; se tira en cuanto deja de hacer falta.
                    let _ = std::fs::remove_dir_all(&src);
                }
            }
        }
    }

    if !args.keep {
        limpiar(client, &created).await;
    } else if !created.is_empty() {
        println!(
            "\nsaves dejados en el servidor (--keep): {}",
            created.join(", ")
        );
    }

    report.print_table();
    if let Some(path) = &args.json {
        report.write_json(path)?;
        println!("\ninforme: {}", path.display());
    }
    Ok(())
}

/// Una vuelta completa sobre una forma.
#[allow(clippy::too_many_arguments)]
async fn round_once(
    client: &ApiClient,
    shape: Shape,
    src: &Path,
    args: &BenchArgs,
    conc: usize,
    round: usize,
    report: &mut Report,
    created: &mut Vec<String>,
) -> Result<()> {
    let label = format!(
        "pruebas-{}-c{}-r{}-{}",
        shape.as_str(),
        conc,
        round,
        time::OffsetDateTime::now_utc().unix_timestamp()
    );
    // Los dos servidores materializan un save de forma distinta y no hay un
    // camino común: Cloud no tiene `POST /v1/saves` (contesta 404) porque su
    // fila nace en el primer upload, con UPSERT sobre `(user_id, game_slug,
    // label)`; el cliente sólo minta el id. Self-hosted sí lo crea, y necesita
    // `display_name` para que el self-heal de la tabla `games` acepte un juego
    // inventado que no está en el catálogo Ludusavi (si no, 422 "game not
    // found"). Es el mismo reparto que hace `library::add_to_tracking`.
    let save_id = if client.is_cloud().await {
        uuid::Uuid::new_v4().to_string()
    } else {
        client
            .create_save_with_meta(
                shape.slug(),
                &label,
                Some(&format!("Pruebas {}", shape.as_str())),
                None,
            )
            .await
            .context("creando el save de pruebas en el servidor")?
            .id
            .to_string()
    };
    created.push(save_id.clone());

    let dest = args
        .workdir
        .join(format!("destino-{}-c{}-r{}", shape.as_str(), conc, round));
    let _ = std::fs::remove_dir_all(&dest);

    // 1. Subida en frío: el servidor no tiene nada de este save.
    let up = medir_backup(
        client, &save_id, shape, &label, src, None, conc, round, report,
    )
    .await?;

    // 2. Restore a carpeta vacía: nada que reutilizar, todo viaja. Es el techo
    //    del tiempo — lo que cuesta traerlo entero de la nube.
    medir_restore(
        client,
        &save_id,
        shape,
        &dest,
        Step::RestoreCold,
        false,
        conc,
        round,
        report,
    )
    .await?;

    // 3. Restore encima de sí mismo: el disco ya tiene exactamente esos bytes.
    //    Si esto no es casi instantáneo, el dedup de D.13 no está haciendo su
    //    trabajo.
    medir_restore(
        client,
        &save_id,
        shape,
        &dest,
        Step::RestoreWarm,
        true,
        conc,
        round,
        report,
    )
    .await?;

    // 4. El juego juega: rota un autosave.
    let cambio = fixture::mutate(src, Mutation::Rotate, 10, args.seed ^ round as u64)?;
    tracing::debug!(%cambio, "mutación aplicada");

    // 5. Subida incremental: el server ya tiene casi todos los blobs.
    medir_backup(
        client,
        &save_id,
        shape,
        &label,
        src,
        Some(up),
        conc,
        round,
        report,
    )
    .await?;

    // 6. Y el restore que de verdad hace un usuario: la versión nueva sobre la
    //    carpeta que tiene la vieja.
    medir_restore(
        client,
        &save_id,
        shape,
        &dest,
        Step::RestoreIncremental,
        true,
        conc,
        round,
        report,
    )
    .await?;

    if !args.keep {
        let _ = std::fs::remove_dir_all(&dest);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn medir_backup(
    client: &ApiClient,
    save_id: &str,
    shape: Shape,
    label: &str,
    src: &Path,
    base_version: Option<i64>,
    conc: usize,
    round: usize,
    report: &mut Report,
) -> Result<i64> {
    let step = if base_version.is_some() {
        Step::BackupIncremental
    } else {
        Step::BackupCold
    };
    // El label tiene que ser el único de la vuelta, no uno por forma: en Cloud
    // la fila del save se materializa con un UPSERT sobre
    // `(user_id, game_slug, label)`, así que dos vueltas con el mismo label
    // aterrizan en la MISMA fila con ids distintos — la segunda sube bien y
    // luego no encuentra su propio snapshot ("save has no snapshots yet"),
    // porque el id que publica el manifiesto es el de la primera.
    let seen = std::sync::Arc::new(AtomicU64::new(0));
    let seen_cb = seen.clone();

    let t0 = Instant::now();
    let outcome = upload_directory(
        client,
        save_id,
        shape.slug(),
        label,
        src,
        base_version,
        None,
        move |done, _total| {
            seen_cb.store(done, Ordering::Relaxed);
        },
    )
    .await;
    let ms = t0.elapsed().as_millis() as u64;

    match outcome {
        Ok(out) => {
            println!(
                "  {} · {} · {} ficheros · {} · {}",
                shape.as_str(),
                step.label(),
                out.file_count,
                fmt_bytes(out.total_bytes),
                fmt_ms(ms)
            );
            report.samples.push(Sample {
                shape: shape.as_str().into(),
                step,
                concurrency: conc,
                round,
                ms,
                files: out.file_count as u64,
                bytes: out.total_bytes,
                bytes_reused: None,
                files_reused: None,
                manifest_ms: None,
                index_ms: None,
                transfer_ms: None,
            });
            Ok(out.snapshot.version_num)
        }
        Err(e) => {
            report.failures.push(Failure {
                shape: shape.as_str().into(),
                step,
                concurrency: conc,
                round,
                error: format!("{e:#}"),
            });
            Err(e).context("backup")
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn medir_restore(
    client: &ApiClient,
    save_id: &str,
    shape: Shape,
    dest: &Path,
    step: Step,
    dedup: bool,
    conc: usize,
    round: usize,
    report: &mut Report,
) -> Result<()> {
    let version = hoard_agent::restore::resolve_version(client, save_id, None).await?;
    let options = RestoreOptions {
        skip_verify: false,
        // Un restore frío entra en carpeta vacía; los otros dos pisan lo que
        // haya, que es justo lo que hace el motor de verdad.
        force: true,
        reuse_from: dedup.then(|| dest.to_path_buf()),
    };

    let t0 = Instant::now();
    let outcome: Result<RestoreOutcome> =
        download_snapshot(client, save_id, version, dest, options, |_, _| {}).await;
    let ms = t0.elapsed().as_millis() as u64;

    match outcome {
        Ok(out) => {
            println!(
                "  {} · {} · {} ficheros · {} ({} reutilizado) · {}{}",
                shape.as_str(),
                step.label(),
                out.files_extracted,
                fmt_bytes(out.bytes_extracted),
                fmt_bytes(out.bytes_reused),
                fmt_ms(ms),
                fases(&out),
            );
            report.samples.push(Sample {
                shape: shape.as_str().into(),
                step,
                concurrency: conc,
                round,
                ms,
                files: out.files_extracted as u64,
                bytes: out.bytes_extracted,
                bytes_reused: Some(out.bytes_reused),
                files_reused: Some(out.files_reused as u64),
                manifest_ms: Some(out.timings.manifest_ms),
                index_ms: Some(out.timings.index_ms),
                transfer_ms: Some(out.timings.transfer_ms),
            });
            Ok(())
        }
        Err(e) => {
            report.failures.push(Failure {
                shape: shape.as_str().into(),
                step,
                concurrency: conc,
                round,
                error: format!("{e:#}"),
            });
            Err(e).context("restore")
        }
    }
}

fn fases(out: &RestoreOutcome) -> String {
    let t = &out.timings;
    if t.transfer_ms == 0 && t.index_ms == 0 && t.manifest_ms == 0 {
        return String::new();
    }
    format!(
        " (manifiesto {} · índice {} · transfer {})",
        fmt_ms(t.manifest_ms),
        fmt_ms(t.index_ms),
        fmt_ms(t.transfer_ms)
    )
}

/// Borra del servidor lo que el banco creó. Best-effort y ruidoso: un save de
/// pruebas olvidado ocupa cuota real del usuario, así que si no se puede
/// borrar hay que decirlo, no tragárselo.
async fn limpiar(client: &ApiClient, ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    print!("limpiando {} saves de pruebas… ", ids.len());
    use std::io::Write;
    std::io::stdout().flush().ok();
    let mut fallidos = Vec::new();
    for id in ids {
        let borrado = if client.is_cloud().await {
            client.cloud_save_delete(id).await
        } else {
            client.delete_save(id).await
        };
        if let Err(e) = borrado {
            fallidos.push(format!("{id}: {e:#}"));
        }
    }
    if fallidos.is_empty() {
        println!("hecho");
    } else {
        println!("{} sin borrar", fallidos.len());
        for f in &fallidos {
            eprintln!("  {f}");
        }
    }
}
