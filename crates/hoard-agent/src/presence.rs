//! Presencia en vivo para el Eye panel: heartbeats a
//! `POST /v1/presence/heartbeat`.
//!
//! Una única implementación que comparten los dos frontends (desktop y
//! daemon CLI): se hace `spawn` junto al agente, se le reenvían los
//! `AgentEvent::GameStarted/GameStopped` que ya consumen sus event loops, y
//! el resto es autónomo:
//!
//! - **Latido inmediato** cuando cambia el juego reportado — arrancar una
//!   partida en esta máquina se pinta en el Eye panel de las demás en ~1-2s
//!   (el server empuja el UPDATE de `devices` por Supabase Realtime).
//! - **Keepalive cada 30s** mientras el agente vive; el server marca offline
//!   a los 90s sin latido (3 perdidos), así que un crash envejece solo.
//! - **Latido final `closing`** en el shutdown ordenado, para que el dot se
//!   apague al instante en vez de esperar el umbral.
//!
//! Todo best-effort: un latido fallido se loguea en debug y el siguiente tick
//! reintenta. El gate es la capacidad que anuncia el server
//! ([`ApiClient::has_presence`], sobre el probe cacheado de `/v1/health`), no el
//! despliegue: desde la 1.1.3 un server self-hosted también lleva el censo, y
//! contra uno anterior no se emite nada.
//!
//! En self-hosted esto no habla con Hoard ni con nadie de fuera: los latidos van
//! al servidor del propio usuario y sólo los ve él.

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{interval, Instant, MissedTickBehavior};

use crate::api::ApiClient;

/// Cadencia del keepalive. El umbral server-side de "online" es 90s
/// (`PRESENCE_ONLINE_WINDOW_SECS` en cloud/routes/me.rs) = 3 latidos
/// perdidos; si tocas uno, toca el otro.
const KEEPALIVE_SECS: u64 = 30;

/// Máximo que un `closing()` espera por su latido antes de rendirse: el quit
/// de la app nunca debe colgarse de una red muerta.
const CLOSING_TIMEOUT_SECS: u64 = 3;

enum Cmd {
    Started { slug: String },
    Stopped { slug: String },
    Closing { done: oneshot::Sender<()> },
}

/// Handle barato de clonar. Los frontends lo llaman desde sus event loops.
#[derive(Clone)]
pub struct PresenceHandle {
    tx: mpsc::Sender<Cmd>,
}

impl PresenceHandle {
    /// Un juego rastreado arrancó. `try_send` a propósito: si el canal está
    /// lleno perdemos un beat, nunca bloqueamos el event loop del frontend.
    pub fn game_started(&self, slug: impl Into<String>) {
        let _ = self.tx.try_send(Cmd::Started { slug: slug.into() });
    }

    /// Un juego rastreado paró.
    pub fn game_stopped(&self, slug: impl Into<String>) {
        let _ = self.tx.try_send(Cmd::Stopped { slug: slug.into() });
    }

    /// Latido final en el shutdown ordenado: marca este device offline ya.
    /// Espera acotada ([`CLOSING_TIMEOUT_SECS`]) — llamable desde el path de
    /// quit sin miedo a colgarlo.
    pub async fn closing(&self) {
        let (done_tx, done_rx) = oneshot::channel();
        if self.tx.send(Cmd::Closing { done: done_tx }).await.is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(CLOSING_TIMEOUT_SECS), done_rx).await;
        }
    }
}

/// Arranca la tarea de presencia sobre el mismo `ApiClient` del agente (el
/// token compartido tras el `RwLock` la mantiene autenticada cuando el
/// desktop rota el JWT). La tarea muere sola cuando todos los handles caen, o
/// inmediatamente tras el beat de un `closing()`.
pub fn spawn(api: ApiClient) -> (PresenceHandle, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(64);
    let task = tokio::spawn(run(api, rx));
    (PresenceHandle { tx }, task)
}

async fn run(api: ApiClient, mut rx: mpsc::Receiver<Cmd>) {
    // slug → (refcount, arranque). Refcount porque dos saves rastreados del
    // mismo juego disparan dos GameStarted; el juego sigue "vivo" hasta que
    // caen ambos. Se reporta la lista COMPLETA en cada latido — jugar a dos
    // juegos a la vez pinta dos filas en el Eye panel de las otras máquinas.
    let mut running: HashMap<String, (u32, Instant)> = HashMap::new();

    let mut tick = interval(Duration::from_secs(KEEPALIVE_SECS));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                None => break,
                Some(Cmd::Started { slug }) => {
                    let entry = running.entry(slug).or_insert((0, Instant::now()));
                    entry.0 += 1;
                    // Latido inmediato solo cuando el SET visible cambia (el
                    // juego acaba de aparecer); el segundo save del mismo
                    // juego no cambia nada que se vea.
                    if entry.0 == 1 {
                        beat(&api, &running, false).await;
                    }
                }
                Some(Cmd::Stopped { slug }) => {
                    if let Some(entry) = running.get_mut(&slug) {
                        entry.0 = entry.0.saturating_sub(1);
                        if entry.0 == 0 {
                            running.remove(&slug);
                            beat(&api, &running, false).await;
                        }
                    }
                }
                Some(Cmd::Closing { done }) => {
                    running.clear();
                    beat(&api, &running, true).await;
                    let _ = done.send(());
                    break;
                }
            },
            _ = tick.tick() => {
                beat(&api, &running, false).await;
            }
        }
    }
}

async fn beat(api: &ApiClient, running: &HashMap<String, (u32, Instant)>, closing: bool) {
    // Gate por capacidad, no por despliegue: cloud siempre la tiene y
    // self-hosted desde la 1.1.3, que es cuando su server empezó a llevar censo
    // de dispositivos. El probe se cachea al primer éxito, así que en régimen
    // esto no cuesta red; un server viejo (o un probe fallido) → silencio.
    if !api.has_presence().await {
        return;
    }
    // Lista completa, la más reciente primero — el mismo orden en que el
    // server la guarda y el Eye panel la pinta.
    let mut games: Vec<(&String, &Instant)> = running.iter().map(|(s, (_, at))| (s, at)).collect();
    games.sort_by_key(|(_, started)| std::cmp::Reverse(**started));
    let playing: Vec<crate::api::PlayingBeat> = games
        .into_iter()
        .map(|(slug, started)| crate::api::PlayingBeat {
            slug: slug.clone(),
            for_secs: started.elapsed().as_secs(),
        })
        .collect();
    if let Err(e) = api.presence_heartbeat(&playing, closing).await {
        tracing::debug!(error = %e, "presence: heartbeat failed (retry on next tick)");
    }
}
