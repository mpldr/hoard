//! Comandos del motor de sync — ahora **cliente del servicio**, no dueño.
//!
//! Hasta el Slice 4b el desktop embebía el motor: `start_agent` hacía
//! `agent::spawn`, se quedaba el `AgentHandle` en `AppState` y reenviaba los
//! `AgentEvent` a la UI. Eso ataba el sync a la ventana (cerrar la app paraba el
//! sync salvo que la CLI tuviera el pidfile) y obligaba a un árbitro entre los
//! dos motores. Desde este slice el motor vive en `hoardd` —uno por usuario, que
//! sobrevive a la app— y estos comandos son lo que la ADR 0021 pide: mandarle
//! peticiones por la IPC y pintar lo que reporte.
//!
//! Lo que cambia y lo que no:
//!
//! - **No cambia** la superficie que ve la UI: mismos `#[tauri::command]`,
//!   mismos nombres de evento `agent://*`, mismo `AgentStatus`. La restricción
//!   dura de D.3 es que los stores TS no se enteren del cambio de backend.
//! - **Cambia** quién hace el trabajo: el conjunto vigilado, la persistencia de
//!   `state.json` y la presencia son del servicio. El cliente **avisa** de los
//!   cambios ([`hoard_core::ipc::Request::Reload`]), no manda listas de saves.
//! - **Desaparece** el pidfile del lado del desktop: ya no hay motor que
//!   arbitrar aquí. `hoard_agent::instance` sigue existiendo mientras la CLI
//!   embeba el suyo (el daemon lo respeta); se borra en el 4d.

use std::collections::HashSet;
use std::sync::OnceLock;

use hoard_agent::agent::WatchedSave;
use hoard_agent::prefs::Prefs;
use hoard_core::ipc::{AgentSlotStatus, Request};
use tauri::{AppHandle, Manager, State};

use crate::daemon::{self, AgentStatus};
use crate::state::AppState;

/// Serializa los arranques concurrentes. El rehidratado de arranque lo dispara
/// desde dos sitios (el scheduler de Modo Automático y el login cloud), y con un
/// `await` de por medio los dos podían pasar la comprobación de "¿ya está?" y
/// duplicar el trabajo. Sigue mereciendo la pena aunque ahora arrancar sea
/// idempotente: evita dos `ensure_running` a la vez, que lanzarían dos daemons
/// (uno saldría solo, pero el log queda más limpio así).
fn agent_start_gate() -> &'static tokio::sync::Mutex<()> {
    static GATE: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    GATE.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Asegura que el servicio está arriba y devuelve el estado de su motor.
/// Idempotente. El relevo de eventos lo enciende [`attach_agent_events`].
#[tauri::command]
pub async fn start_agent(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<AgentStatus, String> {
    let _start = agent_start_gate().lock().await;

    // Levanta el servicio si no lo hay (la conexión de comandos hace "spawn if
    // absent") y pregunta cómo está. El relevo de eventos **no** se enciende
    // aquí: lo enciende la UI cuando ya tiene sus oyentes puestos, porque a esta
    // función también la llama el escaneo de Modo Automático desde Rust y puede
    // ganarle al montaje del webview.
    let status = state
        .daemon
        .status()
        .await
        .map_err(|e| format!("Couldn't reach the Hoard service: {e:#}"))?;

    // Dot del watcher: lo que el servicio dice estar vigilando de verdad, no lo
    // que nosotros creemos que debería.
    let mut seen = HashSet::new();
    daemon::announce_slots(&app, &status.slots, &mut seen);

    if let Some(pid) = status.engine.blocked_by_pid {
        // Convivencia 4b–4c: `hoard sync` sigue embebiendo su motor y tiene el
        // pidfile. El servicio no arranca un segundo motor y lo dice; el sync
        // funciona, sólo que lo lleva la CLI.
        tracing::warn!(
            pid,
            "another Hoard agent owns the engine; the service is serving without one"
        );
    } else if !status.engine.running {
        tracing::info!(
            reason = status.engine.last_error.as_deref().unwrap_or("starting"),
            "the Hoard service has no engine yet"
        );
    }

    // Push servidor→app del self-hosted (SSE). Cloud lo recibe por Supabase
    // Realtime, así que sólo se levanta con una sesión self-hosted viva. Se
    // decide con lo que ya hay en memoria: sondear `/v1/health` sólo para esto
    // era una petición de red en el camino de arranque.
    let selfhosted = state
        .user
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|u| !u.is_cloud_server);
    if selfhosted {
        crate::commands::selfhosted_events::start(&app);
    }

    let running = status.engine.running;
    let watched_count = status.slots.len().max(status.engine.watched);
    let reported = AgentStatus {
        running,
        watched_count,
    };
    daemon::emit_status(&app, &reported);
    Ok(reported)
}

/// Desengancha la app del servicio (logout, cierre).
///
/// **El servicio sigue vivo**: ése es el punto del Slice 4. Cerrar la app o
/// cerrar sesión no puede parar el sync; para eso está `hoard sync stop`, que es
/// una orden explícita. Aquí sólo se sueltan las conexiones y las tareas de esta
/// ventana.
#[tauri::command]
pub async fn stop_agent(app: AppHandle, _state: State<'_, AppState>) -> Result<(), String> {
    // El subscriptor SSE se para en el mismo paso: no tiene a quién despachar y,
    // en un logout, las credenciales que lee están a punto de desaparecer.
    crate::commands::selfhosted_events::stop(&app);
    daemon::detach(&app);
    daemon::emit_status(&app, &AgentStatus::down());
    Ok(())
}

/// La UI ya tiene puestos sus `listen()` de `agent://*`: empieza a relevarle los
/// eventos del servicio (backlog desde el cursor + push en vivo).
///
/// Va aparte de `start_agent` a propósito — ver [`crate::daemon`]: quien enciende
/// el relevo tiene que ser quien escucha, o el primer backlog se emite al vacío.
#[tauri::command]
pub async fn attach_agent_events(app: AppHandle) -> Result<(), String> {
    daemon::attach(&app);
    Ok(())
}

/// La UI deja de escuchar (logout, recarga). Para el relevo; el servicio sigue.
#[tauri::command]
pub async fn detach_agent_events(app: AppHandle) -> Result<(), String> {
    daemon::detach(&app);
    Ok(())
}

/// Fuerza un backup ya, saltándose el debounce.
#[tauri::command]
pub async fn backup_now(save_id: String, state: State<'_, AppState>) -> Result<(), String> {
    state
        .daemon
        .request(Request::BackupNow { save_id })
        .await
        .map(|_| ())
        .map_err(|e| format!("{e:#}"))
}

/// Barrido escalonado de backups sobre todos los saves rastreados. Lo dispara el
/// tick de Modo Automático. No es un error que el servicio no tenga motor: el
/// siguiente tick barrerá.
#[tauri::command]
pub async fn sweep_backups(state: State<'_, AppState>) -> Result<(), String> {
    let window_secs = Prefs::load_default()
        .map(|(p, _)| p.automatic_backup_interval_secs)
        .unwrap_or(3600);
    state
        .daemon
        .tell("run a backup sweep", Request::SweepAll { window_secs })
        .await;
    Ok(())
}

/// Foto de diagnóstico de cada slot vigilado. Alimenta el panel oculto de
/// Ajustes. Vacío = el servicio no tiene motor (la UI muestra "agente parado").
#[tauri::command]
pub async fn agent_status(state: State<'_, AppState>) -> Result<Vec<AgentSlotStatus>, String> {
    match state.daemon.status().await {
        Ok(status) => Ok(status.slots),
        Err(e) => Err(format!("{e:#}")),
    }
}

// ---- pegamento con el resto de comandos --------------------------------

/// Un save nuevo empieza a vigilarse sin reiniciar nada.
///
/// No manda el `WatchedSave` por el cable a propósito: el dueño del conjunto
/// vigilado es el servicio, así que el cliente le dice que `state.json` cambió y
/// él re-hidrata (D.15). Mandar el save sería el cliente decidiendo qué vigila el
/// motor.
pub(crate) async fn attach_save_if_running(state: &State<'_, AppState>, _save: WatchedSave) {
    state.daemon.notify_reload().await;
}

/// Un save que deja de rastrearse deja de vigilarse. Mismo aviso: el servicio
/// compara lo que vigila con lo que hay en disco.
pub(crate) async fn detach_save_if_running(state: &State<'_, AppState>, _save_id: String) {
    state.daemon.notify_reload().await;
}

/// Aplica el efecto en vivo de un cambio de ajustes de un save
/// (`hoard_agent::library::set_paused`/`set_preset`/`set_local_path`). Attach,
/// detach y reseat son la misma cosa vistos desde aquí: el disco cambió.
pub(crate) async fn apply_reseat(
    state: &State<'_, AppState>,
    reseat: hoard_agent::library::LiveReseat,
) {
    if matches!(reseat, hoard_agent::library::LiveReseat::Noop) {
        return;
    }
    state.daemon.notify_reload().await;
}

/// Avisa al servicio de que la sesión en disco cambió (login, logout, cambio de
/// cuenta): que tire el motor y lo levante resolviendo credenciales de nuevo.
///
/// Fire-and-forget: quien cierra sesión no debe esperar a un socket, y el keeper
/// del daemon reintenta por su cuenta.
pub(crate) fn notify_session_changed(app: &AppHandle) {
    let app = app.clone();
    tokio::spawn(async move {
        app.state::<AppState>()
            .daemon
            .tell(
                "tell the service the session changed",
                Request::RestartEngine,
            )
            .await;
    });
}

/// Pasa al motor las carpetas candidatas del último escaneo, para que sondee la
/// correlación proceso↔escritura. Es lo único que el cliente sí manda como
/// lista: la detección vive aquí hasta el Slice 8.
pub(crate) async fn set_probe_candidates(app: &AppHandle, dirs: Vec<std::path::PathBuf>) {
    let count = dirs.len();
    // El cable es JSON: una ruta que no sea UTF-8 no cabe, y se dice aquí, que
    // es donde se sabe cuál era.
    let mut sendable = Vec::with_capacity(dirs.len());
    for dir in dirs {
        match dir.into_os_string().into_string() {
            Ok(text) => sendable.push(text),
            Err(bad) => tracing::warn!(
                path = %std::path::Path::new(&bad).display(),
                "automatic scan: dropping a probe candidate whose path isn't UTF-8"
            ),
        }
    }
    app.state::<AppState>()
        .daemon
        .tell(
            "send the probe candidates",
            Request::SetProbeCandidates { dirs: sendable },
        )
        .await;
    tracing::debug!(
        count,
        "automatic scan: probe candidates sent to the service"
    );
}

/// Empuja una preferencia al motor. Las prefs ya están guardadas en disco cuando
/// esto corre, así que un fallo aquí es cosmético: el motor la leerá igual en su
/// siguiente arranque.
pub(crate) async fn push_pref(state: &State<'_, AppState>, request: Request) {
    state.daemon.tell("push a preference", request).await;
}

#[cfg(test)]
mod tests {
    use hoard_agent::library::name_matches;

    #[test]
    fn name_match_kebab_vs_titlecase() {
        assert!(name_matches("Stardew Valley", "stardew-valley"));
        assert!(name_matches("Hollow Knight", "hollow-knight"));
        assert!(name_matches(
            "Subnautica: Below Zero",
            "subnautica-below-zero"
        ));
        assert!(!name_matches("Stardew Valley", "stardew-vallei"));
        assert!(!name_matches("", ""));
    }
}
