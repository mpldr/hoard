//! Censo de dispositivos y presencia en vivo, self-hosted.
//!
//! # Qué contesta esto
//!
//! Tres preguntas que un self-hoster con más de una máquina se hace y que hasta
//! la 1.1.2 sólo podía contestar Hoard Cloud:
//!
//! - **De qué máquina salió esta versión.** Esa ya la contestaba la columna
//!   `snapshots.device_name`, y el historial la pinta desde entonces. No hace
//!   falta nada de aquí para eso.
//! - **Qué máquinas hay en esta cuenta.** El censo: `GET /v1/devices`.
//! - **Cuáles están encendidas ahora y jugando a qué.** La presencia:
//!   `POST /v1/presence/heartbeat` cada ~30 s desde cada máquina.
//!
//! # Nada sale de tu servidor
//!
//! Esto es lo contrario de un servicio externo: el censo vive en **tu** SQLite,
//! lo escriben tus propias máquinas contra tu propio servidor, y no hay ninguna
//! ruta que lo mande a ningún sitio. Es la razón de que esta pieza sí encaje en
//! self-hosted mientras que los broadcasts del operador no: aquellos son Hoard
//! hablándole a tus clientes, esto son tus clientes hablando entre ellos a
//! través de tu servidor.
//!
//! # Identidad
//!
//! Una máquina se identifica con la cabecera `x-hoard-device-fp`, una huella
//! estable que calcula el cliente (`hoard_agent::logship::device_identity`).
//! Estable de verdad: reinstalar la app no crea un dispositivo nuevo. Un
//! cliente que no la mande —builds anteriores— no registra nada y no es un
//! error; simplemente no aparece en la lista.
//!
//! # `online` se calcula al leer
//!
//! No hay columna que decir. Un dispositivo está encendido mientras su último
//! latido sea más joven que [`ONLINE_WINDOW_SECS`] y no haya mandado el latido
//! de cierre. Guardarlo obligaría a que alguien lo apagara, y una máquina que
//! se va por un corte de luz no apaga nada: se quedaría encendida para siempre.

use axum::{
    extract::{Extension, Path, State},
    http::{HeaderMap, StatusCode},
    response::Json,
};
use hoard_core::wire::{DeviceListOut, DeviceOut, DevicePlaying, Heartbeat};
use sqlx::Row;
use std::sync::Arc;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::routes::health::ServerState;
use crate::routes::snapshots::internal_logged;

type ApiError = (StatusCode, Json<serde_json::Value>);

/// Un dispositivo cuenta como encendido mientras su último latido sea más joven
/// que esto. El agente late cada 30 s, así que 90 s son tres latidos perdidos:
/// lo bastante corto para que se note en vivo, lo bastante largo para aguantar
/// un tropiezo de red. Si tocas esto, toca también `KEEPALIVE_SECS` en
/// `hoard_agent::presence`.
pub const ONLINE_WINDOW_SECS: i64 = 90;

/// Topes de lo que un latido puede declarar. La presencia es cosmética y sólo
/// la ve el dueño de la cuenta, así que esto no defiende de nada grave: evita
/// que un cliente roto meta megas en una fila.
const MAX_PLAYING_GAMES: usize = 8;
const MAX_SLUG_CHARS: usize = 128;

/// Marca de tiempo en el mismo formato que escriben las migraciones
/// (`strftime('%Y-%m-%dT%H:%M:%SZ')`), para que las comparaciones de texto en
/// SQLite ordenen bien.
fn stamp(at: OffsetDateTime) -> String {
    let d = at.date();
    let t = at.time();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        d.year(),
        u8::from(d.month()),
        d.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

fn header<'a>(headers: &'a HeaderMap, key: &str) -> Option<&'a str> {
    headers
        .get(key)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Alta o refresco de la máquina que pregunta. Sin huella no se registra nada y
/// **no es un error**: un cliente viejo debe seguir sincronizando igual, sólo
/// que sin salir en la lista.
pub async fn register(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    headers: &HeaderMap,
) -> Result<(), sqlx::Error> {
    let Some(fingerprint) = header(headers, "x-hoard-device-fp") else {
        return Ok(());
    };
    let name = header(headers, "x-hoard-device-name").unwrap_or("Unknown device");
    let os = header(headers, "x-hoard-device-os");
    let app_version = header(headers, "x-hoard-app-version");
    let now = stamp(OffsetDateTime::now_utc());

    // `closed_at = NULL` al reaparecer: una máquina que se despidió y vuelve
    // está encendida otra vez. `device_kind` se queda en 'desktop' — hoy el
    // único cliente que late es la app de escritorio (o el daemon en la misma
    // máquina); el día que haya otro, esto es lo que cambia.
    sqlx::query(
        "INSERT INTO devices (id, user_id, device_name, device_kind, os, app_version,
                              fingerprint, last_seen_at, closed_at)
         VALUES (?,?,?,'desktop',?,?,?,?,NULL)
         ON CONFLICT(user_id, fingerprint) DO UPDATE SET
             last_seen_at = excluded.last_seen_at,
             closed_at    = NULL,
             device_name  = excluded.device_name,
             os           = COALESCE(excluded.os, devices.os),
             app_version  = COALESCE(excluded.app_version, devices.app_version)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(user_id)
    .bind(name)
    .bind(os)
    .bind(app_version)
    .bind(fingerprint)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

// ─── GET /v1/devices ────────────────────────────────────────────────────────

pub async fn list(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    headers: HeaderMap,
) -> Result<Json<DeviceListOut>, ApiError> {
    let user_id = user.user_id.to_string();
    // Preguntar también cuenta como estar vivo: si no, la máquina que tiene el
    // panel abierto sería la única que aparece apagada.
    if let Err(e) = register(&state.pool, &user_id, &headers).await {
        tracing::warn!(error = %e, "devices: register on list failed");
    }
    let caller_fp = header(&headers, "x-hoard-device-fp").unwrap_or("");
    let cutoff = stamp(OffsetDateTime::now_utc() - time::Duration::seconds(ONLINE_WINDOW_SECS));

    let rows = sqlx::query(
        "SELECT id, device_name, device_kind, os, fingerprint, playing,
                last_seen_at, created_at, closed_at
           FROM devices WHERE user_id = ?
          ORDER BY last_seen_at DESC",
    )
    .bind(&user_id)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| internal_logged("listing devices", e))?;

    let devices = rows
        .into_iter()
        .map(|r| {
            let last_seen: String = r.get("last_seen_at");
            let closed: Option<String> = r.get("closed_at");
            let online = closed.is_none() && last_seen.as_str() > cutoff.as_str();
            let fp: String = r.get("fingerprint");
            // El listado de juegos sólo se sirve si la máquina está encendida:
            // "Fulano jugando a X" con el equipo apagado desde ayer es peor que
            // no decir nada.
            let playing: Vec<DevicePlaying> = if online {
                r.get::<Option<String>, _>("playing")
                    .as_deref()
                    .and_then(|j| serde_json::from_str(j).ok())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            DeviceOut {
                id: r.get("id"),
                device_name: r.get("device_name"),
                device_kind: r.get("device_kind"),
                os: r.get("os"),
                last_seen_at: Some(last_seen),
                created_at: r.get("created_at"),
                online,
                playing,
                this_device: !caller_fp.is_empty() && fp == caller_fp,
            }
        })
        .collect();

    Ok(Json(DeviceListOut { devices }))
}

// ─── DELETE /v1/devices/:id ─────────────────────────────────────────────────

/// Olvidar una máquina. Sólo borra el censo: las versiones que subió siguen
/// donde están y su `device_name` en el historial no se toca, porque es un dato
/// de lo que pasó, no del dispositivo.
pub async fn delete(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    Path(device_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<DeviceListOut>, ApiError> {
    let user_id = user.user_id.to_string();
    sqlx::query("DELETE FROM devices WHERE id = ? AND user_id = ?")
        .bind(&device_id)
        .bind(&user_id)
        .execute(&state.pool)
        .await
        .map_err(|e| internal_logged("deleting a device", e))?;
    list(State(state), Extension(user), headers).await
}

// ─── POST /v1/presence/heartbeat ────────────────────────────────────────────

pub async fn heartbeat(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    headers: HeaderMap,
    Json(body): Json<Heartbeat>,
) -> Result<StatusCode, ApiError> {
    let user_id = user.user_id.to_string();
    let Some(fp) = header(&headers, "x-hoard-device-fp") else {
        // Cliente sin huella: no hay a quién apuntar el latido. Silencio, no
        // error — un cliente viejo debe seguir funcionando.
        return Ok(StatusCode::NO_CONTENT);
    };
    let now = OffsetDateTime::now_utc();

    if body.closing {
        sqlx::query(
            "UPDATE devices SET last_seen_at = ?, closed_at = ?, playing = NULL
              WHERE user_id = ? AND fingerprint = ?",
        )
        .bind(stamp(now))
        .bind(stamp(now))
        .bind(&user_id)
        .bind(fp)
        .execute(&state.pool)
        .await
        .map_err(|e| internal_logged("recording a closing beat", e))?;
        return Ok(StatusCode::NO_CONTENT);
    }

    // Primer contacto de esta máquina (o vuelta tras un cierre): darla de alta
    // con las mismas cabeceras.
    register(&state.pool, &user_id, &headers)
        .await
        .map_err(|e| internal_logged("registering a device", e))?;

    let stored: Option<String> =
        sqlx::query_scalar("SELECT playing FROM devices WHERE user_id = ? AND fingerprint = ?")
            .bind(&user_id)
            .bind(fp)
            .fetch_optional(&state.pool)
            .await
            .map_err(|e| internal_logged("reading presence", e))?
            .flatten();
    let old: Vec<DevicePlaying> = stored
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();

    let playing = merge_playing(&old, &body.playing, now);
    let encoded = serde_json::to_string(&playing).unwrap_or_else(|_| "[]".into());

    sqlx::query(
        "UPDATE devices SET last_seen_at = ?, closed_at = NULL, playing = ?
          WHERE user_id = ? AND fingerprint = ?",
    )
    .bind(stamp(now))
    .bind(&encoded)
    .bind(&user_id)
    .bind(fp)
    .execute(&state.pool)
    .await
    .map_err(|e| internal_logged("recording a heartbeat", e))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Anclar el `since` de cada juego **una vez**, en el latido donde su slug
/// aparece por primera vez.
///
/// Pura para poder probarla. Sin esto el "lleva 40 min" del panel saltaría en
/// cada latido, porque cada uno traería un `for_secs` recalculado por el
/// cliente. El ancla la pone el reloj del server: un cliente con la hora
/// desviada no puede decir que lleva jugando desde el futuro.
fn merge_playing(
    old: &[DevicePlaying],
    beat: &[hoard_core::wire::PlayingBeat],
    now: OffsetDateTime,
) -> Vec<DevicePlaying> {
    beat.iter()
        .filter(|g| !g.slug.is_empty() && g.slug.chars().count() <= MAX_SLUG_CHARS)
        .take(MAX_PLAYING_GAMES)
        .map(|g| {
            let since = old
                .iter()
                .find(|o| o.slug == g.slug)
                .and_then(|o| o.since.clone())
                .unwrap_or_else(|| {
                    // Primera vez que se ve: anclarlo a lo que el cliente dice
                    // que lleva, acotado para que un valor absurdo no invente
                    // una sesión de años.
                    let secs = g.for_secs.min(60 * 60 * 24 * 7) as i64;
                    stamp(now - time::Duration::seconds(secs))
                });
            DevicePlaying {
                slug: g.slug.clone(),
                since: Some(since),
            }
        })
        .collect()
}

/// Olvidar máquinas que llevan `max_age_days` sin dar señales. Sin esto el
/// censo acumula para siempre cada portátil que alguien usó una tarde.
pub async fn prune_stale(pool: &sqlx::SqlitePool, max_age_days: i64) -> Result<u64, sqlx::Error> {
    let cutoff = stamp(OffsetDateTime::now_utc() - time::Duration::days(max_age_days));
    let done = sqlx::query("DELETE FROM devices WHERE last_seen_at < ?")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(done.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hoard_core::wire::PlayingBeat;

    fn beat(slug: &str, for_secs: u64) -> PlayingBeat {
        PlayingBeat {
            slug: slug.into(),
            for_secs,
        }
    }

    /// El ancla de una sesión se pone una vez y no se mueve mientras el juego
    /// siga en la lista. Es lo que hace que el "lleva 40 min" del panel avance
    /// en vez de temblar con cada latido.
    #[test]
    fn a_running_session_keeps_its_anchor_across_beats() {
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let first = merge_playing(&[], &[beat("factorio", 600)], now);
        assert_eq!(first.len(), 1);
        let anchor = first[0].since.clone().unwrap();
        assert_eq!(anchor, stamp(now - time::Duration::seconds(600)));

        // Latido siguiente, medio minuto después y con el contador del cliente
        // avanzado: el ancla no se toca.
        let later = now + time::Duration::seconds(30);
        let second = merge_playing(&first, &[beat("factorio", 630)], later);
        assert_eq!(second[0].since.as_deref(), Some(anchor.as_str()));

        // Un juego nuevo sí estrena ancla.
        let third = merge_playing(&second, &[beat("factorio", 660), beat("stardew", 0)], later);
        assert_eq!(third.len(), 2);
        assert_eq!(third[0].since.as_deref(), Some(anchor.as_str()));
        assert_eq!(third[1].since, Some(stamp(later)));

        // Y dejar de jugar lo saca de la lista.
        assert!(merge_playing(&third, &[], later).is_empty());
    }

    /// Un cliente roto no puede meter basura ni inventarse una sesión eterna.
    #[test]
    fn a_beat_cannot_claim_nonsense() {
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let many: Vec<PlayingBeat> = (0..20).map(|i| beat(&format!("g{i}"), 0)).collect();
        assert_eq!(merge_playing(&[], &many, now).len(), MAX_PLAYING_GAMES);

        let long = "x".repeat(MAX_SLUG_CHARS + 1);
        assert!(merge_playing(&[], &[beat(&long, 0), beat("", 0)], now).is_empty());

        // Diez años de sesión se acotan a una semana en vez de anclar la
        // sesión en 2016.
        let absurd = merge_playing(&[], &[beat("factorio", 60 * 60 * 24 * 3650)], now);
        assert_eq!(absurd[0].since, Some(stamp(now - time::Duration::days(7))));
    }
}
