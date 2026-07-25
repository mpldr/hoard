//! # `hoardd` — el servicio local dueño del motor (ADR 0021, Parte A)
//!
//! Hasta el Slice 4 el motor de sync (`hoard_agent::agent::spawn`) iba embebido
//! en dos binarios —el desktop y el daemon CLI—, y el árbitro para que no
//! corrieran dos a la vez era un pidfile con tres fallos de diseño: carrera en el
//! arranque, cero reclaim (parabas la CLI y el desktop no retomaba el motor), y
//! el ciclo de vida del sync atado a una UI. Este crate es el motor promovido a
//! **proceso propio de vida larga**: exactamente uno por usuario, que sobrevive
//! al cierre de la app y con el que desktop y CLI hablan por un socket local.
//!
//! ## Qué hay y qué no (tras el Slice 4c)
//!
//! **Hay:** el binario, la IPC (socket con permisos solo-usuario + handshake
//! versionado + journal con cursor y push en vivo), el arranque y la supervisión
//! del motor, "spawn if absent" idempotente, y **el único rotador del refresh
//! token** — que además lo presta por IPC a quien lo necesite
//! (`Request::CloudToken`). Desde el 4b el desktop no tiene motor; desde el 4c
//! tampoco la CLI: `hoard sync` asegura este servicio y se engancha a su journal.
//!
//! **No hay todavía:** borrar `instance.rs` es 4d (aquí sólo se sigue *tomando*
//! el pidfile, para que un frontend de una versión anterior no arranque un motor
//! contra nosotros); el empaquetado (systemd user unit / launchd / Task Scheduler
//! apuntando a este binario, y el sidecar del bundle) y las notificaciones
//! nativas cierran el Slice 4.
//!
//! ## Mapa
//!
//! - [`endpoint`] — dónde escucha (socket por usuario / named pipe).
//! - [`transport`] — bind exclusivo (el socket **es** el árbitro), permisos,
//!   accept y connect.
//! - [`codec`] — tramas sobre el socket (el formato vive en `hoard_core::ipc`).
//! - [`journal`] — journal con cursor + push; guarda transiciones y acciones, no
//!   reposos repetidos.
//! - [`engine`] — arranque, supervisión y bomba de eventos del motor.
//! - [`server`] — handshake y despacho.
//! - [`client`] — cliente + "spawn if absent" (lo que usan el desktop y la CLI).
//!
//! La sesión (resolución, refresher y préstamo del token) vive en
//! `hoard_agent::session`: el Slice 4a la tenía duplicada aquí para no tocar la
//! CLI, y el 4c dejó una sola implementación compartida.

pub mod client;
pub mod codec;
pub mod endpoint;
pub mod engine;
pub mod journal;
pub mod server;
pub mod transport;

#[cfg(windows)]
pub mod winsec;

use std::sync::Arc;

use anyhow::Result;
use hoard_agent::agent::AgentEvent;
use hoard_agent::supervisor;
use tokio::sync::mpsc;

use crate::endpoint::Endpoint;
use crate::engine::Engine;
use crate::journal::EventLog;
use crate::server::Daemon;
use crate::transport::{BindError, Listener};

/// Eventos del motor en cola antes de que la bomba los drene. Generoso: perder
/// un evento es perder una fila del journal, y el journal es lo que sostiene el
/// catch-up de los clientes.
const EVENT_CHANNEL: usize = 256;

/// Cómo arrancar el daemon.
pub struct Options {
    /// Endpoint explícito. `None` = el del usuario (o el de `HOARDD_SOCKET`).
    pub endpoint: Option<Endpoint>,
    /// Arrancar el motor. `false` sirve la IPC y nada más: diagnóstico y tests
    /// (un test **no** puede levantar el motor de verdad — sincronizaría los
    /// saves de quien lo ejecuta).
    pub with_engine: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            endpoint: None,
            with_engine: true,
        }
    }
}

/// Cómo acabó el proceso.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Servimos hasta que nos pidieron parar.
    Served,
    /// Ya había un daemon: no hacía falta este proceso. **No es un error** — es
    /// la mitad "el que pierde el bind" de "spawn if absent".
    AlreadyRunning,
}

/// Arranca el servicio y sirve hasta que se pida el apagado (por IPC o señal).
pub async fn run(options: Options) -> Result<Outcome> {
    let endpoint = match options.endpoint {
        Some(ep) => ep,
        None => Endpoint::resolve()?,
    };

    let listener = match Listener::bind(&endpoint) {
        Ok(listener) => listener,
        Err(BindError::AlreadyRunning { address }) => {
            tracing::info!(address = %address, "hoardd: another daemon already owns the socket; exiting");
            return Ok(Outcome::AlreadyRunning);
        }
        Err(BindError::Failed(err)) => return Err(err),
    };
    tracing::info!(
        address = %endpoint,
        pid = std::process::id(),
        version = env!("CARGO_PKG_VERSION"),
        engine = options.with_engine,
        "hoardd: listening"
    );

    let log = Arc::new(EventLog::new());
    let engine = Engine::new();
    let daemon = Arc::new(Daemon::new(log.clone(), engine.clone()));

    // El canal de eventos es del daemon, no del motor: se crea una vez y cada
    // arranque del motor recibe un clon del emisor. Así un motor reiniciado sigue
    // escribiendo en el mismo journal y los cursores de los clientes no se rompen.
    let (events_tx, events_rx) = mpsc::channel::<AgentEvent>(EVENT_CHANNEL);
    let events_rx = Arc::new(tokio::sync::Mutex::new(events_rx));

    // Regla de D.12: todo lo que vive más que una petición va supervisado. Un
    // pánico aquí es un incidente logueado y un reinicio con backoff, no un
    // silencio de 36 minutos.
    let mut tasks = Vec::new();
    tasks.push(tokio::spawn({
        let engine = engine.clone();
        let log = log.clone();
        let events_rx = events_rx.clone();
        supervisor::supervise("hoardd event pump", move || {
            engine::pump(engine.clone(), log.clone(), events_rx.clone())
        })
    }));
    if options.with_engine {
        tasks.push(tokio::spawn({
            let engine = engine.clone();
            let events_tx = events_tx.clone();
            supervisor::supervise("hoardd engine keeper", move || {
                engine::keeper(engine.clone(), events_tx.clone())
            })
        }));
    } else {
        engine.disable("the daemon was started with --no-engine");
    }

    let listener = Arc::new(tokio::sync::Mutex::new(listener));
    tasks.push(tokio::spawn({
        let listener = listener.clone();
        let daemon = daemon.clone();
        supervisor::supervise("hoardd ipc", move || {
            server::accept_loop(listener.clone(), daemon.clone())
        })
    }));

    tokio::select! {
        _ = daemon.wait_for_shutdown() => tracing::info!("hoardd: stopping on request"),
        _ = shutdown_signal() => tracing::info!("hoardd: stopping on signal"),
    }

    // Orden del apagado: primero el motor (su último latido de presencia necesita
    // el token vivo y su `shutdown` es limpio), luego las tareas, luego el socket.
    engine.shutdown().await;
    for task in &tasks {
        task.abort();
    }
    for task in tasks {
        let _ = task.await;
    }
    // Suelta el socket y el lock: el fichero no queda para que el siguiente
    // arranque tenga que barrerlo.
    drop(listener);
    Ok(Outcome::Served)
}

/// Se resuelve cuando el SO nos pide parar: Ctrl-C, más SIGTERM en unix — la
/// señal que manda `systemctl --user stop` / `launchctl bootout`. Sin la rama de
/// SIGTERM el gestor de servicios tendría que mandarnos SIGKILL, saltándose el
/// apagado limpio del motor.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Manda todo pánico al log, no sólo a stderr.
///
/// Misma razón que en el desktop (ADR 0021 D.12): un servicio de usuario no tiene
/// dónde imprimir —stderr es `/dev/null` bajo systemd sin journal, o la nada bajo
/// Task Scheduler—, así que una task que muere de pánico no dejaba **ni un
/// rastro**. Eso fue exactamente el poller de D.11/D.12.
pub fn install_panic_logger() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("<unnamed>").to_string();
        tracing::error!(
            location = %location,
            thread = %thread,
            "PANIC — a task or thread died"
        );
        previous(info);
    }));
}
