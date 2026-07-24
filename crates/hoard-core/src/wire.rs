//! # Tipos de wire compartidos (ADR 0021, C.6 — Slice 3)
//!
//! Las formas que `hoard_agent::api` y `hoard_server::routes` mantenían **a
//! mano, por duplicado**. Ahora hay una sola definición y el drift entre
//! cliente y server es un error de compilación en vez de un 422 en producción.
//!
//! Ya había drift real cuando esto se escribió: el cliente enviaba
//! `LogEntry.target`/`ts` como campos obligatorios y el server los declaraba
//! `Option`, y `SaveResponse` (server) y `Save` (cliente) llevaban desde 2025
//! subconjuntos distintos de los mismos campos.
//!
//! ## Alcance
//!
//! El contrato **self-hosted**: `/v1/health`, `/v1/auth/whoami`,
//! `/v1/me/max-versions`, `/v1/games`, `/v1/saves`, `/v1/saves/*/snapshots` y
//! `/v1/logs` (que el namespace cloud reusa tal cual). Los DTO exclusivos de
//! cloud (`Cloud*` en `agent::api` ↔ `server::cloud::routes`) siguen duplicados;
//! son el siguiente incremento natural, no parte de este slice.
//!
//! ## Disciplina de compatibilidad — leer antes de tocar nada
//!
//! Que compile no basta: **cliente y server se despliegan por separado**. Un
//! usuario self-hosted corre un server de hace tres versiones contra un desktop
//! recién actualizado, y Hoard Cloud actualiza el server sin que nadie toque los
//! clientes instalados. Por eso:
//!
//! - **Append-only.** Se añaden campos; no se quitan.
//! - **Todo campo nuevo lleva `#[serde(default)]`** (y `Option` si no hay
//!   default sensato), para que el JSON de una versión vieja siga
//!   deserializando.
//! - **Nunca se repurposea un campo.** Cambiar el significado (o el tipo) de uno
//!   existente es indistinguible de corrupción para la otra punta. Si el
//!   significado cambia, es un campo nuevo.
//! - **`#[serde(skip_serializing_if)]` sólo donde el campo hoy no se emite.** El
//!   test golden (`tests/golden_wire.rs`) fija los bytes de la última release;
//!   si un cambio los mueve, el test cae y hay que justificarlo.
//!
//! ## Estado persistido ≠ dato nuevo
//!
//! Los newtypes de [`crate::ids`] llevan la puerta estricta en `serde`, así que
//! un valor envenenado **no entra por el wire**. Pero el server construye estas
//! respuestas leyendo su propia DB, que es estado persistido y puede llevar
//! veneno de años: ahí se usa la puerta indulgente
//! ([`crate::ids::GameSlug::repair`]), nunca `parse`, o una fila mala tumbaría
//! el listado entero de un usuario. Misma regla que en `state.json` (ADR C.3).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::{GameSlug, MachineId, SaveId, Sha256, Username};

/// RFC3339, la misma representación que ya cruzaba el wire.
///
/// El server self-hosted guarda los timestamps en SQLite con
/// `strftime('%Y-%m-%dT%H:%M:%SZ','now')` y los devolvía como `String` sin
/// tocarlos; el cliente los parseaba a `OffsetDateTime` con este mismo
/// serializador. Unificar el tipo no mueve un byte: `time` emite `Z` para offset
/// cero, que es exactamente lo que escribe SQLite, y acepta al parsear tanto esa
/// forma como el `+00:00` que emite el namespace cloud.
use time::serde::rfc3339 as ts;

// ---------------------------------------------------------------------------
// GET /v1/health
// ---------------------------------------------------------------------------

/// Respuesta de `/v1/health`. El cliente ramifica el protocolo entero con
/// [`Health::mode`] (`"cloud"` vs self-hosted), así que este es el tipo más
/// load-bearing del contrato.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Health {
    /// `"ok"` o `"db_error"`.
    pub status: String,
    /// Versión del binario del server.
    pub version: String,
    #[serde(default)]
    pub uptime_secs: u64,
    /// Nivel mínimo de log que este server acepta en el ingest de logs de
    /// cliente. Ausente en servers pre-ingest: el cliente lo lee como "este
    /// server no recibe logs" y desactiva el envío.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_min_level: Option<String>,
    /// `"cloud"` en el despliegue SaaS, ausente self-hosted. Selecciona el
    /// namespace (`/v1/cloud/*` vs `/v1/saves`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

// ---------------------------------------------------------------------------
// GET /v1/auth/whoami  ·  PUT /v1/me/max-versions
// ---------------------------------------------------------------------------

/// Identidad + cuota del usuario autenticado.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Whoami {
    pub user_id: String,
    pub username: Username,
    pub is_admin: bool,
    /// Bytes almacenados ahora mismo. Los servers pre-v0.3 lo omiten → 0.
    #[serde(default)]
    pub storage_used_bytes: i64,
    /// Cuota total. Los servers pre-v0.3 lo omiten → 0, que la UI lee como
    /// "cuota desconocida".
    #[serde(default)]
    pub storage_quota_bytes: i64,
    /// Tope de versiones almacenadas por save. `None` = ilimitado (y lo que
    /// reporta un server sin la feature).
    #[serde(default)]
    pub max_versions: Option<i64>,
}

/// Cuerpo de `PUT /v1/me/max-versions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaxVersionsBody {
    /// `null` quita el tope (ilimitado).
    pub max_versions: Option<i64>,
    /// En seco: no escribe ni borra nada, sólo cuenta cuántos snapshots
    /// *tiraría* el tope. El cliente lo enseña en la confirmación.
    #[serde(default)]
    pub dry_run: bool,
}

/// Respuesta de `PUT /v1/me/max-versions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaxVersionsResponse {
    pub max_versions: Option<i64>,
    /// Snapshots tirados a la papelera por pasarse del tope nuevo (o, en
    /// `dry_run`, los que se tirarían).
    #[serde(default)]
    pub pruned: u64,
}

// ---------------------------------------------------------------------------
// GET /v1/games
// ---------------------------------------------------------------------------

/// Una entrada del catálogo de juegos.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Game {
    pub slug: GameSlug,
    pub display_name: String,
    pub engine: Option<String>,
    #[serde(default)]
    pub save_paths_json: Option<String>,
}

// ---------------------------------------------------------------------------
// /v1/saves
// ---------------------------------------------------------------------------

/// Cuerpo de `POST /v1/saves`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSaveRequest {
    pub game_slug: GameSlug,
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_os: Option<String>,
    /// Metadatos opcionales de clientes nuevos. Cuando la tabla `games` del
    /// server no conoce el slug (server sembrado con un catálogo Ludusavi más
    /// viejo), con esto hace upsert de una fila stub en vez de devolver 422.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steam_app_id: Option<i64>,
}

/// Cuerpo de `PATCH /v1/saves/{id}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PatchSaveRequest {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub local_path_hint: Option<String>,
    #[serde(default)]
    pub client_os: Option<String>,
}

/// Un save. Unión de lo que emitía `server::routes::saves::SaveResponse` y lo
/// que esperaba `agent::api::Save`; los campos que sólo tenía una punta van
/// `Option` + `default` para que ninguna versión desplegada se rompa.
///
/// Los campos agregados (`snapshot_count`, `total_size_bytes`) son `Option`
/// porque cloud no los calcula: `None` es "no lo sé", distinto de `Some(0)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Save {
    pub id: SaveId,
    /// Sólo lo emite cloud. Se omite al serializar cuando falta, para que el
    /// self-hosted siga emitiendo exactamente el JSON de la release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    pub game_slug: GameSlug,
    pub label: String,
    #[serde(default)]
    pub local_path_hint: Option<String>,
    #[serde(default)]
    pub client_os: Option<String>,
    #[serde(default)]
    pub latest_version_num: Option<i64>,
    #[serde(default)]
    pub snapshot_count: Option<i64>,
    #[serde(default)]
    pub total_size_bytes: Option<i64>,
    #[serde(with = "ts")]
    pub created_at: OffsetDateTime,
    #[serde(with = "ts")]
    pub updated_at: OffsetDateTime,
}

// ---------------------------------------------------------------------------
// /v1/saves/{id}/snapshots
// ---------------------------------------------------------------------------

/// Resumen de un snapshot (una versión de un save).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_id: Option<SaveId>,
    pub version_num: i64,
    /// Padre en el DAG (`None` = raíz). La arista que hace detectable la
    /// divergencia. Los servers viejos lo omiten → `None`.
    #[serde(default)]
    pub parent_version: Option<i64>,
    #[serde(default)]
    pub device_name: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    pub total_size_bytes: i64,
    pub file_count: i64,
    pub is_pinned: bool,
    #[serde(default, with = "ts::option")]
    pub deleted_at: Option<OffsetDateTime>,
    #[serde(with = "ts")]
    pub created_at: OffsetDateTime,
}

/// Snapshot + su listado de ficheros (`GET /v1/saves/{id}/snapshots/{n}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotDetail {
    #[serde(flatten)]
    pub snapshot: Snapshot,
    pub files: Vec<SnapshotFile>,
}

/// Un fichero dentro de un snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotFile {
    pub relative_path: String,
    pub size_bytes: i64,
    /// `None` = **no se conoce**, no "hash malo". Pasa con las versiones legacy
    /// de archivo entero, donde el listado se sintetiza leyendo el tar y no hay
    /// digest por fichero. En el JSON eso es la cadena vacía, que es lo que
    /// emitía la release: ver [`sha_opt`].
    #[serde(default, with = "sha_opt")]
    pub sha256: Option<Sha256>,
}

/// `Option<Sha256>` con `""` como forma de `None` en el JSON.
///
/// La release ya usaba la cadena vacía para "sin digest", así que relajar la
/// puerta de [`Sha256`] para dejarla pasar habría sido meter un valor imposible
/// dentro del tipo. El sitio correcto para "no aplica" es el `Option`, y este
/// módulo es el traductor entre las dos representaciones — sin tocar los bytes
/// que cruzan la red.
mod sha_opt {
    use super::Sha256;
    use serde::{de::Error as _, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Option<Sha256>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(value.as_ref().map(Sha256::as_str).unwrap_or(""))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Sha256>, D::Error> {
        let raw = Option::<String>::deserialize(d)?;
        match raw.as_deref() {
            None | Some("") => Ok(None),
            Some(s) => Sha256::parse(s).map(Some).map_err(D::Error::custom),
        }
    }
}

// ---------------------------------------------------------------------------
// POST /v1/logs  (el namespace cloud monta este mismo cuerpo)
// ---------------------------------------------------------------------------

/// Metadatos del dispositivo, una vez por lote.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceMeta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub os: Option<String>,
    /// Huella de la máquina. Es la misma que usa el alta de dispositivos, así
    /// que una fila de log y el contador de dispositivos casan por este campo.
    #[serde(default)]
    pub fingerprint: Option<MachineId>,
    #[serde(default)]
    pub app_version: Option<String>,
}

/// Una línea de log enviada por un cliente.
///
/// `target` y `ts` son `Option` porque así los declaraba el server: el cliente
/// los mandaba siempre, pero el contrato tolera su ausencia y romper eso
/// rechazaría lotes de clientes viejos.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub level: String,
    #[serde(default)]
    pub target: Option<String>,
    pub message: String,
    /// Campos estructurados como objeto JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<serde_json::Value>,
    /// Timestamp del cliente (RFC3339). Se queda como `String`: es dato de
    /// diagnóstico de una punta que puede tener el reloj torcido, y el server
    /// lo guarda tal cual sin interpretarlo.
    #[serde(default)]
    pub ts: Option<String>,
}

/// Un lote de logs (`POST /v1/logs`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogBatch {
    pub device: DeviceMeta,
    pub entries: Vec<LogEntry>,
}

/// Respuesta del ingest de logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogIngestResponse {
    pub accepted: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Los timestamps salen con el sufijo `Z` de la release, y vuelven a entrar
    /// tanto en esa forma como en la que emite el namespace cloud (`+00:00`).
    #[test]
    fn timestamps_keep_the_release_shape() {
        let json = r#"{
            "id": "3f2504e0-4f89-41d3-9a0c-0305e82c3301",
            "game_slug": "stardew-valley",
            "label": "default",
            "local_path_hint": null,
            "client_os": null,
            "latest_version_num": 7,
            "snapshot_count": 3,
            "total_size_bytes": 1024,
            "created_at": "2026-07-24T12:34:56Z",
            "updated_at": "2026-07-24T12:34:56+00:00"
        }"#;
        let save: Save = serde_json::from_str(json).unwrap();
        let out = serde_json::to_value(&save).unwrap();
        assert_eq!(out["created_at"], "2026-07-24T12:34:56Z");
        assert_eq!(out["updated_at"], "2026-07-24T12:34:56Z");
        // `user_id` ausente no se emite: los bytes self-hosted no cambian.
        assert!(out.get("user_id").is_none());
    }

    /// Un save con el slug envenenado no entra por el wire. Es la mitad
    /// "estricta" de la ADR C.3: los datos nuevos pasan por la puerta.
    #[test]
    fn poisoned_slug_never_crosses_the_wire() {
        let json = r#"{
            "id": "3f2504e0-4f89-41d3-9a0c-0305e82c3301",
            "game_slug": "GSE Saves",
            "label": "default",
            "created_at": "2026-07-24T12:34:56Z",
            "updated_at": "2026-07-24T12:34:56Z"
        }"#;
        assert!(serde_json::from_str::<Save>(json).is_err());
    }

    /// Un JSON de una versión vieja (sin los campos que se añadieron después)
    /// sigue deserializando. Es la disciplina append-only en forma de test.
    #[test]
    fn older_payloads_still_deserialize() {
        let old_health = r#"{"status":"ok","version":"0.2.0"}"#;
        let h: Health = serde_json::from_str(old_health).unwrap();
        assert_eq!(h.uptime_secs, 0);
        assert!(h.mode.is_none());

        let old_whoami = r#"{"user_id":"u1","username":"jacka","is_admin":false}"#;
        let w: Whoami = serde_json::from_str(old_whoami).unwrap();
        assert_eq!(w.storage_quota_bytes, 0);
        assert!(w.max_versions.is_none());

        let old_snapshot = r#"{
            "id":"s1","version_num":1,"total_size_bytes":10,"file_count":2,
            "is_pinned":false,"created_at":"2026-07-24T12:34:56Z"
        }"#;
        let s: Snapshot = serde_json::from_str(old_snapshot).unwrap();
        assert!(s.parent_version.is_none());
        assert!(s.deleted_at.is_none());
        assert!(s.save_id.is_none());
    }

    /// `SnapshotDetail` aplana el resumen: los campos del snapshot viven al
    /// mismo nivel que `files`, como en la release.
    #[test]
    fn snapshot_detail_stays_flat() {
        let json = r#"{
            "id":"s1","version_num":1,"total_size_bytes":10,"file_count":1,
            "is_pinned":false,"created_at":"2026-07-24T12:34:56Z",
            "files":[{"relative_path":"a.sav","size_bytes":10,
                      "sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]
        }"#;
        let d: SnapshotDetail = serde_json::from_str(json).unwrap();
        assert_eq!(d.files.len(), 1);
        let out = serde_json::to_value(&d).unwrap();
        assert_eq!(out["version_num"], 1);
        assert!(out["files"].is_array());
    }
}
