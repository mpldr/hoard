//! Empuje Cloud de baja latencia para el motor headless (`hoard daemon`) — lo
//! mismo que la app de escritorio consigue con `cloud_pull` + `cloud_realtime`,
//! pero sin Tauri y operando directo sobre el [`AgentHandle`].
//!
//! Dos mitades que se complementan:
//! - **Realtime** (`realtime_loop`): un WebSocket a Supabase Realtime suscrito a
//!   la tabla `saves` (RLS acota a tu usuario). En cuanto otro dispositivo hace
//!   commit, la transacción sube `saves.latest_version_num` y Supabase empuja un
//!   `UPDATE`; lo convertimos en un pull inmediato (~1s en vez de esperar al
//!   poll). El servidor Hoard ni se entera: el mensajero es Supabase.
//! - **Poll de respaldo** (`poll_loop`): pega a `/v1/cloud/sync` cada
//!   `poll_interval` por si el socket se cae o el push se pierde. El manifest
//!   va excluido de la cuota de banda, así que es gratis en dinero y bytes.
//!
//! Ambas desembocan en lo mismo: se alimenta la caché de versiones del agente
//! (`set_cloud_versions`) y, con sync global, se pide `force_restore` de los
//! saves que avanzaron en el servidor. El version-gate y los vetos mid-session
//! viven dentro del agente, así que pedir de más nunca pisa datos.
//!
//! Todo es best-effort: si el socket falla se reconecta con backoff, y si el
//! poll falla se reintenta al siguiente tick. El daemon nunca se cae por esto.

use std::collections::HashMap;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep, Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message;

use crate::agent::AgentHandle;
use crate::api::ApiClient;
use crate::cloud_auth;

/// Cadencia de heartbeat: Supabase Realtime cierra sockets ociosos ~30s sin
/// latido en el topic `phoenix`.
const HEARTBEAT_SECS: u64 = 25;

/// Vida máxima de una conexión. El JWT de Supabase dura ~1h; reciclamos el
/// socket bien dentro de esa ventana para reconectar con un token fresco de
/// disco en vez de rastrear su expiración exacta.
const CONNECTION_MAX_SECS: u64 = 45 * 60;

/// Límites del backoff de reconexión.
const BACKOFF_MIN_SECS: u64 = 2;
const BACKOFF_MAX_SECS: u64 = 60;

/// Cadencia del modo aparcado: sin sesión utilizable, el realtime solo relee el
/// fichero de sesión esperando un login nuevo. Espeja el recheck del refresher
/// periódico del daemon (session.rs::RELOGIN_RECHECK_EVERY) — mismo evento, no
/// tiene sentido enterarse a ritmos distintos.
const RELOGIN_RECHECK_SECS: u64 = 5 * 60;

/// Ajustes del empuje Cloud.
pub struct Config {
    /// Periodo del poll de respaldo a `/v1/cloud/sync`.
    pub poll_interval: Duration,
    /// Sync global: cuando un save avanza en el servidor, forzar su restore ya
    /// (no solo alimentar la caché de versiones). Espejo de `Prefs::global_sync`.
    pub global_sync: bool,
}

/// Arranca poll + realtime y devuelve sus tasks. El daemon las guarda para que
/// vivan tanto como él; abortan solo si se las aborta explícitamente (o al morir
/// el proceso).
pub fn spawn(client: ApiClient, handle: AgentHandle, cfg: Config) -> Vec<JoinHandle<()>> {
    // Canal de "kick" de capacidad 1: un push del realtime pide un pull fuera de
    // cadencia. Si ya hay uno pendiente, el `try_send` lo descarta → una ráfaga
    // de cambios colapsa en un solo pull extra.
    let (kick_tx, kick_rx) = mpsc::channel::<()>(1);

    let poll = tokio::spawn(poll_loop(client, handle, cfg, kick_rx));
    let realtime = tokio::spawn(realtime_loop(kick_tx));
    vec![poll, realtime]
}

/// Poll de respaldo + consumidor de kicks. Corre un pull en cada tick del timer
/// y en cada empujón del realtime, serializados por el `select!` (nunca dos a la
/// vez). Mantiene el mapa `save_id → version_num` visto para detectar avances.
async fn poll_loop(
    client: ApiClient,
    handle: AgentHandle,
    cfg: Config,
    mut kick_rx: mpsc::Receiver<()>,
) {
    tracing::info!(
        poll_secs = cfg.poll_interval.as_secs(),
        global_sync = cfg.global_sync,
        "cloud-live: empuje Cloud arrancado"
    );

    // El mapa vive en memoria: una sesión nueva parte de cero y la primera
    // pasada solo fija la línea base (nada cuenta como "avanzado"), igual que el
    // desktop, para no forzar restores masivos justo al arrancar.
    let mut seen: HashMap<String, i64> = HashMap::new();

    // El primer tick de `interval` es inmediato: sembramos la línea base nada
    // más arrancar. `Skip` evita ráfagas si el sistema se congela un rato.
    let mut ticker = interval(cfg.poll_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            k = kick_rx.recv() => {
                if k.is_none() {
                    // El emisor murió (no debería mientras corre el realtime).
                    return;
                }
            }
        }
        // Vacía kicks acumulados: varios cambios seguidos = un solo pull.
        while kick_rx.try_recv().is_ok() {}

        run_pull(&client, &handle, cfg.global_sync, &mut seen).await;
    }
}

/// Un pull del manifest: alimenta la caché de versiones del agente y, con sync
/// global, fuerza el restore de los saves que avanzaron desde la última pasada.
async fn run_pull(
    client: &ApiClient,
    handle: &AgentHandle,
    global_sync: bool,
    seen: &mut HashMap<String, i64>,
) {
    let manifest = match client.cloud_sync().await {
        Ok(m) => m,
        Err(e) => {
            // Un 401 transitorio (token en el filo) o un corte de red se
            // recupera solo en el siguiente tick; el refresco periódico del
            // daemon mantiene el token del cliente al día.
            tracing::debug!(error = %format!("{e:#}"), "cloud-live: pull falló");
            return;
        }
    };

    let mut latest: HashMap<String, i64> = HashMap::with_capacity(manifest.saves.len());
    let mut advanced: Vec<String> = Vec::new();
    for e in &manifest.saves {
        latest.insert(e.save_id.clone(), e.latest_version_num);
        // Solo cuenta como avance si ya teníamos una versión previa y subió.
        // Los que vemos por primera vez (`None`) solo fijan línea base.
        if let Some(prev) = seen.get(&e.save_id) {
            if e.latest_version_num > *prev {
                advanced.push(e.save_id.clone());
            }
        }
    }
    *seen = latest.clone();

    // Alimenta la caché de versiones del agente en cada pasada (no solo en
    // deltas) para que el sweep de reconciliación gatee por versión sin re-pedir
    // el manifest por cada save.
    if let Err(e) = handle.set_cloud_versions(latest).await {
        tracing::warn!(error = %format!("{e:#}"), "cloud-live: no pude alimentar la caché de versiones");
    }

    // Sync global: pide el pull inmediato de lo que avanzó. El agente gatea por
    // versión y respeta los vetos mid-session, así que esto nunca pisa datos.
    if global_sync {
        for id in advanced {
            if let Err(e) = handle.force_restore(id).await {
                tracing::warn!(error = %format!("{e:#}"), "cloud-live: no pude pedir force-restore");
            }
        }
    }
}

/// Bucle externo de reconexión del WebSocket. Nunca termina por sí solo: si la
/// sesión Cloud desaparece o GoTrue revoca la familia de tokens, en vez de
/// morir se aparca vigilando el fichero de sesión (sin red) hasta que un
/// `hoard login` — aquí o en el desktop, comparten fichero — deje una sesión
/// nueva, y entonces reconecta. Antes retornaba: el refresher periódico sí
/// readoptaba el re-login (Expired→Normal) pero el realtime ya no existía, y
/// el daemon se quedaba en latencia de poll (≤60s) hasta reiniciarlo.
async fn realtime_loop(kick_tx: mpsc::Sender<()>) {
    let mut backoff = BACKOFF_MIN_SECS;
    loop {
        match connect_once(&kick_tx).await {
            Ok(true) => {
                // Fin de ciclo limpio (tope de vida): reconecta ya con el token
                // fresco que ha quedado en disco.
                backoff = BACKOFF_MIN_SECS;
            }
            Ok(false) => {
                // Sin sesión utilizable (ausente o revocada). Vigila el disco
                // sin tocar red: replayar un token revocado cada pocos minutos
                // contra GoTrue es justo el ruido que se le quitó al refresher.
                let dead = cloud_auth::load_session().ok().flatten().map(|s| s.refresh);
                tracing::info!("cloud-live: realtime en pausa — esperando un login nuevo");
                loop {
                    sleep(Duration::from_secs(RELOGIN_RECHECK_SECS)).await;
                    let disk = cloud_auth::load_session().ok().flatten();
                    if session_renewed(dead.as_deref(), disk.as_ref()) {
                        break;
                    }
                }
                tracing::info!("cloud-live: sesión nueva en disco — realtime reconecta");
                backoff = BACKOFF_MIN_SECS;
            }
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), "cloud-live: conexión realtime cortada, reintento");
            }
        }
        sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(BACKOFF_MAX_SECS);
    }
}

/// ¿Lo que hay en disco ya no es la sesión que murió? Solo entonces merece
/// reconectar: un refresh token distinto (o una sesión donde no había ninguna)
/// es un login nuevo; la misma sesión muerta seguiría rebotando en el join.
fn session_renewed(dead: Option<&str>, disk: Option<&cloud_auth::Session>) -> bool {
    let Some(s) = disk else { return false };
    if s.refresh.trim().is_empty() {
        return false;
    }
    dead != Some(s.refresh.as_str())
}

/// Un ciclo de conexión: conecta, se une al canal `saves` y bombea heartbeats y
/// cambios hasta que el socket muere o vence el tope de vida. `Ok(true)` = fin
/// limpio (reconecta), `Ok(false)` = sin sesión utilizable, ausente o revocada
/// (el caller se aparca a esperar un login nuevo).
async fn connect_once(kick_tx: &mpsc::Sender<()>) -> anyhow::Result<bool> {
    let sess = match cloud_auth::load_session()? {
        Some(s) => s,
        None => return Ok(false),
    };

    // wss://<project>.supabase.co/realtime/v1/websocket?apikey=<anon>&vsn=1.0.0
    let base = cloud_auth::supabase_url();
    let ws_base = base
        .strip_prefix("https://")
        .map(|h| format!("wss://{h}"))
        .or_else(|| base.strip_prefix("http://").map(|h| format!("ws://{h}")))
        .unwrap_or_else(|| base.clone());
    let anon = cloud_auth::supabase_anon_key();
    let url = format!("{ws_base}/realtime/v1/websocket?apikey={anon}&vsn=1.0.0");

    let (ws, _resp) = tokio_tungstenite::connect_async(&url).await?;
    let (mut write, mut read) = ws.split();

    // Únete al canal suscribiendo UPDATE/INSERT de public.saves. La RLS del token
    // garantiza que solo llegan filas propias — sin filtro de user_id en cliente.
    let join = json!({
        "topic": "realtime:hoard",
        "event": "phx_join",
        "payload": {
            "config": {
                "broadcast": { "ack": false },
                "presence": { "key": "" },
                "private": false,
                "postgres_changes": [
                    { "event": "UPDATE", "schema": "public", "table": "saves" },
                    { "event": "INSERT", "schema": "public", "table": "saves" }
                ]
            },
            "access_token": sess.access
        },
        "ref": "1"
    });
    write.send(Message::Text(join.to_string())).await?;

    let mut hb = interval(Duration::from_secs(HEARTBEAT_SECS));
    hb.tick().await; // consume el primer tick inmediato
    let mut hb_ref: u64 = 2;
    let deadline = Instant::now() + Duration::from_secs(CONNECTION_MAX_SECS);

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                // Tope de vida: recicla para tomar un token fresco.
                let _ = write.send(Message::Close(None)).await;
                return Ok(true);
            }
            _ = hb.tick() => {
                let beat = json!({
                    "topic": "phoenix",
                    "event": "heartbeat",
                    "payload": {},
                    "ref": hb_ref.to_string()
                });
                hb_ref += 1;
                if write.send(Message::Text(beat.to_string())).await.is_err() {
                    anyhow::bail!("fallo al enviar heartbeat");
                }
            }
            msg = read.next() => {
                let Some(msg) = msg else { return Ok(true) };
                match msg? {
                    Message::Text(txt) => match classify(&txt) {
                        Some(Action::Change) => {
                            tracing::debug!("cloud-live: cambio en saves → pull");
                            let _ = kick_tx.try_send(());
                        }
                        Some(Action::Resubscribed) => {
                            // Recién (re)unidos: lo que cambió con el socket caído
                            // no generó `postgres_changes`, así que un pull de
                            // recuperación cierra ese hueco.
                            tracing::debug!("cloud-live: (re)suscrito → pull de recuperación");
                            let _ = kick_tx.try_send(());
                        }
                        Some(Action::TokenError) => {
                            // JWT rechazado al unirse: refresca (queda en disco) y
                            // reconecta con el token rotado. Si el refresh está
                            // terminalmente caducado, deja de intentar.
                            //
                            // Vía `refresh_freshest`, nunca con la `sess` de esta
                            // conexión: se capturó al conectar y para cuando llega
                            // un TokenError puede tener hasta CONNECTION_MAX_SECS,
                            // tiempo de sobra para que el refresher periódico la
                            // haya rotado. Replayarla sería reuse-detection, y GoTrue
                            // responde revocando la familia entera de tokens.
                            tracing::debug!("cloud-live: token rechazado, refresco");
                            match cloud_auth::refresh_freshest().await {
                                Ok(_) => anyhow::bail!("refresh forzó reconexión"),
                                Err(e) if e.downcast_ref::<cloud_auth::RefreshTokenStale>().is_some() => {
                                    tracing::info!("cloud-live: refresh token revocado — realtime se aparca hasta un login nuevo");
                                    return Ok(false);
                                }
                                Err(_) => anyhow::bail!("refresh forzó reconexión"),
                            }
                        }
                        None => {}
                    },
                    Message::Ping(p) => {
                        let _ = write.send(Message::Pong(p)).await;
                    }
                    Message::Close(_) => return Ok(true),
                    _ => {}
                }
            }
        }
    }
}

enum Action {
    /// Cambió una fila relevante de `saves` — refrescar.
    Change,
    /// El join tuvo éxito: (re)suscrito, disparar pull de recuperación.
    Resubscribed,
    /// El token fue rechazado; refrescar y reconectar.
    TokenError,
}

/// Interpreta un frame de Realtime. `None` para los que no accionamos
/// (heartbeat, presence, status del sistema…).
fn classify(txt: &str) -> Option<Action> {
    let v: Value = serde_json::from_str(txt).ok()?;
    let event = v.get("event").and_then(|e| e.as_str()).unwrap_or("");
    match event {
        "postgres_changes" => {
            let table = v
                .pointer("/payload/data/table")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            (table == "saves").then_some(Action::Change)
        }
        "phx_reply" | "system" => {
            let status = v
                .pointer("/payload/status")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            if status == "error" {
                let reason = v
                    .pointer("/payload/response")
                    .map(|r| r.to_string())
                    .unwrap_or_default()
                    .to_lowercase();
                let token_ish = reason.contains("token")
                    || reason.contains("jwt")
                    || reason.contains("unauthorized");
                return token_ish.then_some(Action::TokenError);
            }
            // El join lleva `ref: "1"`; su reply "ok" significa (re)suscrito. Los
            // acks de heartbeat reusan `phx_reply` con ref 2,3,… así que gatear
            // en ref "1" dispara exactamente una vez por (re)conexión.
            if event == "phx_reply" && status == "ok" {
                let join_ref = v.get("ref").and_then(|r| r.as_str()).unwrap_or("");
                if join_ref == "1" {
                    return Some(Action::Resubscribed);
                }
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud_auth::Session;

    fn disk(refresh: &str) -> Session {
        Session {
            server_url: "https://api.hoard.services".into(),
            access: "jwt".into(),
            refresh: refresh.into(),
        }
    }

    /// El aparcado solo despierta ante una sesión que NO sea la muerta: la
    /// misma que reventó el join no puede hacer nada mejor la segunda vez.
    #[test]
    fn session_renewed_ignores_the_dead_session_and_wakes_on_a_new_one() {
        // Sin nada en disco, o con un refresh vacío: sigue esperando.
        assert!(!session_renewed(Some("rt-dead"), None));
        assert!(!session_renewed(Some("rt-dead"), Some(&disk("  "))));
        // La misma sesión muerta sigue en disco: sigue esperando.
        assert!(!session_renewed(Some("rt-dead"), Some(&disk("rt-dead"))));
        // Un login nuevo (refresh distinto) despierta.
        assert!(session_renewed(Some("rt-dead"), Some(&disk("rt-new"))));
    }

    /// Caso "no había sesión al aparcar" (logout en caliente): cualquier
    /// sesión con refresh no vacío cuenta como login nuevo.
    #[test]
    fn session_renewed_wakes_on_any_session_when_none_was_dead() {
        assert!(!session_renewed(None, None));
        assert!(!session_renewed(None, Some(&disk(""))));
        assert!(session_renewed(None, Some(&disk("rt-fresh"))));
    }
}
