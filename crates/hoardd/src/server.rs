//! El servidor IPC: handshake, despacho de peticiones y push de eventos.
//!
//! Una task por conexión, y dentro de ella dos mitades: el lector de peticiones
//! y un escritor único alimentado por un canal. Que **todo** lo que sale pase por
//! un solo escritor es lo que permite que una respuesta y un push del journal no
//! se entrelacen a media trama.
//!
//! ## Qué pasa si una conexión se cae (o entra en pánico)
//!
//! Se cae **esa** conexión y nada más. El bucle de accept va bajo
//! `supervisor::supervise` (regla de D.12: si vive más que una petición, va
//! supervisado), y el `panic hook` del daemon manda cualquier pánico al log, así
//! que una conexión que muere deja rastro. No se supervisa *cada conexión*
//! porque reiniciar el cuerpo de una conexión cuyo socket ya no existe no
//! significa nada: el cliente reconecta y, gracias al cursor del journal,
//! recupera lo que se perdió sin agujeros.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use hoard_agent::session::LendError;
use hoard_core::ipc::{
    ClientFrame, DaemonStatus, Hello, IpcError, JournalEntry, Payload, Rejected, Reply, Request,
    ServerFrame, Welcome, PROTOCOL_VERSION,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{broadcast, mpsc};

use crate::codec::{read_frame, write_frame};
use crate::engine::{self, Engine};
use crate::journal::EventLog;
use crate::transport::Listener;

/// Tramas de salida en cola por conexión. Un cliente que no lee se corta cuando
/// se llena, en vez de hacer crecer la memoria del daemon.
const OUTBOX: usize = 512;

/// Estado compartido que ve cada conexión.
pub struct Daemon {
    pub version: String,
    pub pid: u32,
    /// Identidad de esta ejecución: los `seq` del journal sólo son comparables
    /// dentro de un mismo epoch.
    pub epoch: String,
    pub started: Instant,
    pub log: Arc<EventLog>,
    pub engine: Engine,
    /// Se dispara con `Request::Shutdown`; `main` lo espera.
    shutdown: tokio::sync::Notify,
}

impl Daemon {
    pub fn new(log: Arc<EventLog>, engine: Engine) -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
            epoch: uuid::Uuid::new_v4().to_string(),
            started: Instant::now(),
            log,
            engine,
            shutdown: tokio::sync::Notify::new(),
        }
    }

    /// Espera la orden de apagado.
    pub async fn wait_for_shutdown(&self) {
        self.shutdown.notified().await;
    }

    fn welcome(&self) -> Welcome {
        Welcome {
            protocol: PROTOCOL_VERSION,
            daemon_version: self.version.clone(),
            pid: self.pid,
            epoch: self.epoch.clone(),
            cursor: self.log.cursor(),
        }
    }

    async fn status(&self) -> DaemonStatus {
        let mut engine_status = self.engine.status();
        // Los slots son la verdad viva del motor, así que se preguntan en vez de
        // servir un contador guardado que puede haber quedado atrás.
        let slots = engine::slot_status(&self.engine).await;
        if engine_status.running {
            engine_status.watched = slots.len();
        }
        DaemonStatus {
            daemon_version: self.version.clone(),
            protocol: PROTOCOL_VERSION,
            pid: self.pid,
            epoch: self.epoch.clone(),
            uptime_secs: self.started.elapsed().as_secs(),
            cursor: self.log.cursor(),
            engine: engine_status,
            slots,
        }
    }

    /// Despacha todo menos `Subscribe` (que necesita la conexión) y `Shutdown`
    /// (que la dispara). Cada comando del motor es un envío al `AgentHandle`: lo
    /// que pasa después llega por el journal, no por la respuesta.
    async fn dispatch(&self, request: Request) -> Reply {
        match request {
            Request::Ping => Reply::Ok(Payload::Pong {
                daemon_version: self.version.clone(),
                pid: self.pid,
            }),
            Request::Status => Reply::Ok(Payload::Status(self.status().await)),
            Request::Reload => match engine::reload(&self.engine).await {
                Ok(_) => Reply::Ok(Payload::Ack),
                Err(err) => self.engine_error(err),
            },
            Request::SetProbeCandidates { dirs } => {
                let dirs: Vec<std::path::PathBuf> =
                    dirs.into_iter().map(std::path::PathBuf::from).collect();
                self.with_engine(|h| async move { h.set_probe_candidates(dirs).await })
                    .await
            }
            // Tampoco pasa por `with_engine`, y es load-bearing: el rotador del
            // token es **del daemon**, no del motor. Un motor caído por falta de
            // sesión o por un bache de red no puede dejar al desktop sin poder
            // hablar con la nube — y menos aún empujarle a rotar por su cuenta,
            // que es justo lo que este slice viene a matar.
            Request::CloudToken { rejected } => {
                match hoard_agent::session::lend_token(rejected.as_deref()).await {
                    Ok(token) => {
                        if token.rotated {
                            tracing::info!("hoardd: rotated the Cloud token for a client");
                        }
                        Reply::Ok(Payload::CloudToken(token))
                    }
                    Err(LendError::Gone(reason)) => {
                        tracing::warn!(reason = %reason, "hoardd: no Cloud token to lend");
                        Reply::Error(IpcError::CloudSessionExpired { reason })
                    }
                    Err(LendError::Transient(err)) => {
                        let message = format!("{err:#}");
                        tracing::warn!(error = %message, "hoardd: couldn't lend a Cloud token");
                        Reply::Error(IpcError::Internal { message })
                    }
                }
            }
            // No pasa por `with_engine`: reiniciar un motor caído es
            // precisamente lo que puede hacer que vuelva (el keeper resuelve la
            // sesión otra vez), así que un `EngineDown` aquí sería contestar
            // "no puedo arreglarlo porque está roto".
            Request::RestartEngine => {
                self.engine
                    .request_restart("a client reported a session change");
                Reply::Ok(Payload::Ack)
            }
            Request::BackupNow { save_id } => {
                self.with_engine(|h| async move { h.backup_now(save_id).await })
                    .await
            }
            Request::SweepAll { window_secs } => {
                self.with_engine(|h| async move { h.sweep_all(window_secs).await })
                    .await
            }
            Request::ForceRestore { save_id } => {
                self.with_engine(|h| async move { h.force_restore(save_id).await })
                    .await
            }
            Request::SetAutoRestore { enabled } => {
                self.with_engine(|h| async move { h.set_auto_restore(enabled).await })
                    .await
            }
            Request::SetGlobalSync { enabled } => {
                self.with_engine(|h| async move { h.set_global_sync(enabled).await })
                    .await
            }
            // Las dos que no llegan aquí.
            Request::Subscribe { .. } | Request::Shutdown => Reply::Error(IpcError::Internal {
                message: "handled by the connection loop".to_string(),
            }),
        }
    }

    async fn with_engine<F, Fut>(&self, f: F) -> Reply
    where
        F: FnOnce(hoard_agent::agent::AgentHandle) -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<()>>,
    {
        let Some(handle) = self.engine.handle() else {
            return Reply::Error(IpcError::EngineDown {
                reason: self.engine.down_reason(),
            });
        };
        match f(handle).await {
            Ok(()) => Reply::Ok(Payload::Ack),
            Err(err) => self.engine_error(err),
        }
    }

    /// Un comando que no llega al motor casi siempre significa que el motor se
    /// fue (canal cerrado), así que se reporta como `EngineDown` con el motivo
    /// que el keeper haya registrado, no como un `Internal` opaco.
    fn engine_error(&self, err: anyhow::Error) -> Reply {
        tracing::warn!(error = %format!("{err:#}"), "hoardd: a request failed");
        if self.engine.handle().is_none() {
            return Reply::Error(IpcError::EngineDown {
                reason: self.engine.down_reason(),
            });
        }
        Reply::Error(IpcError::Internal {
            message: format!("{err:#}"),
        })
    }
}

/// Acepta conexiones para siempre. No retorna (no puede producir un `Finished`),
/// así que `supervise` sólo lo reinicia por pánico y `main` lo mata abortando.
pub async fn accept_loop(
    listener: Arc<tokio::sync::Mutex<Listener>>,
    daemon: Arc<Daemon>,
) -> hoard_agent::supervisor::Finished {
    loop {
        let accepted = {
            let mut guard = listener.lock().await;
            guard.accept().await
        };
        match accepted {
            Ok(stream) => {
                let daemon = daemon.clone();
                tokio::spawn(async move {
                    if let Err(err) = serve_connection(stream, daemon).await {
                        tracing::debug!(error = %format!("{err:#}"), "hoardd: connection ended");
                    }
                });
            }
            Err(err) => {
                tracing::warn!(error = %format!("{err:#}"), "hoardd: accept failed");
                // Un accept que falla en bucle (fd agotados) no debe quemar CPU.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

/// Atiende una conexión: handshake, luego peticiones hasta que el cliente cierre.
pub async fn serve_connection<S>(stream: S, daemon: Arc<Daemon>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (out_tx, mut out_rx) = mpsc::channel::<ServerFrame>(OUTBOX);

    // Escritor único: respuestas y pushes salen por aquí, nunca en paralelo.
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if let Err(err) = write_frame(&mut writer, &frame).await {
                tracing::debug!(error = %format!("{err:#}"), "hoardd: write failed; dropping the client");
                return;
            }
        }
    });

    let result = handshake_and_serve(&mut reader, &out_tx, &daemon).await;
    drop(out_tx);
    let _ = writer_task.await;
    result
}

async fn handshake_and_serve<R>(
    reader: &mut R,
    out: &mpsc::Sender<ServerFrame>,
    daemon: &Arc<Daemon>,
) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    let first: Option<ClientFrame> = read_frame(reader).await?;
    let Some(ClientFrame::Hello(hello)) = first else {
        // Sin handshake no se atiende nada: el protocolo empieza por decir quién
        // eres y qué versión hablas.
        let _ = out
            .send(ServerFrame::Rejected(Rejected {
                reason: "the first frame must be a hello".to_string(),
                daemon_protocol: PROTOCOL_VERSION,
                daemon_version: daemon.version.clone(),
            }))
            .await;
        return Ok(());
    };
    if !accepts(&hello) {
        tracing::warn!(
            client = %hello.client,
            client_protocol = hello.protocol,
            daemon_protocol = PROTOCOL_VERSION,
            "hoardd: rejected a client speaking another protocol"
        );
        let _ = out
            .send(ServerFrame::Rejected(Rejected {
                reason: format!(
                    "this daemon speaks protocol {PROTOCOL_VERSION}, the client speaks {}",
                    hello.protocol
                ),
                daemon_protocol: PROTOCOL_VERSION,
                daemon_version: daemon.version.clone(),
            }))
            .await;
        return Ok(());
    }
    tracing::info!(client = %hello.client, protocol = hello.protocol, "hoardd: client connected");
    out.send(ServerFrame::Welcome(daemon.welcome()))
        .await
        .context("sending the welcome")?;

    // Alta en el push: se guarda para cuando llegue el `Subscribe`. `None` hasta
    // entonces — un cliente que sólo manda comandos no paga el coste.
    let mut pusher: Option<tokio::task::JoinHandle<()>> = None;

    while let Some(frame) = read_frame::<_, ClientFrame>(reader).await? {
        let ClientFrame::Request { id, request } = frame else {
            // Un segundo hello es ruido; ignorarlo es más amable que cortar.
            continue;
        };
        match request {
            Request::Shutdown => {
                tracing::info!("hoardd: shutdown requested over IPC");
                let _ = out
                    .send(ServerFrame::Reply {
                        id,
                        reply: Reply::Ok(Payload::Ack),
                    })
                    .await;
                daemon.shutdown.notify_one();
                break;
            }
            Request::Subscribe { since } => {
                // El orden importa: primero se abre el canal de push, luego se
                // lee el backlog. Al revés, un evento que ocurriese entre las dos
                // cosas no aparecería ni en el backlog ni en el push — el hueco
                // silencioso que este diseño existe para cerrar.
                let rx = daemon.log.subscribe();
                let backlog = daemon.log.since(since.unwrap_or(0));
                let cursor = backlog.cursor;
                if backlog.gap {
                    tracing::info!(
                        requested = since.unwrap_or(0),
                        cursor,
                        "hoardd: a client asked for journal rows we no longer have"
                    );
                }
                let _ = out
                    .send(ServerFrame::Reply {
                        id,
                        reply: Reply::Ok(Payload::Backlog(backlog)),
                    })
                    .await;
                if let Some(old) = pusher.replace(tokio::spawn(push_loop(
                    rx,
                    out.clone(),
                    cursor,
                    daemon.log.clone(),
                ))) {
                    // Re-suscribirse reemplaza la suscripción anterior; dos
                    // pushers por conexión duplicarían cada evento.
                    old.abort();
                }
            }
            other => {
                let reply = daemon.dispatch(other).await;
                if out.send(ServerFrame::Reply { id, reply }).await.is_err() {
                    break;
                }
            }
        }
    }

    if let Some(task) = pusher {
        task.abort();
    }
    Ok(())
}

/// ¿Hablamos el mismo protocolo? Hoy es igualdad estricta. Cuando haya una
/// versión 2 este es el sitio donde decidir qué versiones viejas se siguen
/// atendiendo — el handshake existe precisamente para poder hacerlo sin que el
/// cliente vea un error de parseo.
fn accepts(hello: &Hello) -> bool {
    hello.protocol == PROTOCOL_VERSION
}

/// Reenvía filas nuevas del journal al cliente. Salta lo que ya iba en el
/// backlog (por `seq`) y, si el cliente se retrasa tanto que el canal descarta
/// filas, le manda un `Resync` en vez de dejarle un hueco invisible.
async fn push_loop(
    mut rx: broadcast::Receiver<JournalEntry>,
    out: mpsc::Sender<ServerFrame>,
    mut cursor: u64,
    log: Arc<EventLog>,
) {
    loop {
        match rx.recv().await {
            Ok(entry) => {
                if entry.seq <= cursor {
                    continue;
                }
                cursor = entry.seq;
                if out.send(ServerFrame::Event(entry)).await.is_err() {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(dropped)) => {
                let _ = out
                    .send(ServerFrame::Resync {
                        cursor: log.cursor(),
                        dropped,
                    })
                    .await;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}
