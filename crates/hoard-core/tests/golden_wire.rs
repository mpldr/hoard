//! # Test golden de round-trip del wire (ADR 0021, C.6 — Slice 3)
//!
//! Las formas de `wire` compilan a la vez en cliente y server, pero **eso sólo
//! garantiza coherencia dentro de un build**: cliente y server se despliegan por
//! separado (un self-hoster corre un server de hace tres versiones contra un
//! desktop de ayer; Hoard Cloud actualiza el server sin tocar los clientes
//! instalados). Un cambio que compile puede romper igual el contrato.
//!
//! Los ficheros de `tests/golden/` son el JSON **byte a byte de la última
//! release** (v1.0.4). Cada uno se deserializa con el tipo de hoy y se vuelve a
//! serializar: si el resultado no es el mismo objeto, el cambio rompe compat y
//! el test cae. Renombrar un campo, quitarlo, cambiarle el tipo o dejar de
//! emitirlo se caza aquí, no en producción.
//!
//! **Al añadir un campo**: se añade al tipo con `#[serde(default)]` y NO se toca
//! el fixture (el JSON viejo debe seguir cargando). Sólo se añade un fixture
//! nuevo cuando se quiere fijar además la forma nueva.

use std::path::PathBuf;

use hoard_core::wire::{
    CreateSaveRequest, Game, Health, LogBatch, LogIngestResponse, MaxVersionsBody,
    MaxVersionsResponse, Save, Snapshot, SnapshotDetail, Whoami,
};
use serde::{de::DeserializeOwned, Serialize};

fn golden(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.json"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("leyendo {}: {e}", path.display()))
}

/// Deserializa el golden con el tipo de hoy, lo vuelve a serializar y exige que
/// salga **el mismo objeto JSON**: ni un campo perdido, ni uno renombrado, ni un
/// valor movido.
fn round_trip<T: DeserializeOwned + Serialize>(name: &str) {
    let raw = golden(name);
    let parsed: T = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("{name}.json no deserializa con el tipo de hoy: {e}"));
    let before: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let after = serde_json::to_value(&parsed).unwrap();
    assert_eq!(
        before, after,
        "{name}.json no sobrevive el round-trip: el wire cambió respecto a la release"
    );
}

/// Sólo comprueba que el JSON de la release **entra**. Para las formas que el
/// tipo compartido normaliza al re-emitir (ver `cloud_rename_response_parses`).
fn parses<T: DeserializeOwned>(name: &str) -> T {
    let raw = golden(name);
    serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("{name}.json no deserializa con el tipo de hoy: {e}"))
}

#[test]
fn health_round_trips() {
    round_trip::<Health>("health");
}

/// Cloud emite su propio `HealthBody` (en `server::cloud::run`, fuera del
/// alcance de este slice) y **no** lleva `uptime_secs`. El cliente ramifica el
/// protocolo entero con este payload, así que lo que importa es que entre.
#[test]
fn cloud_health_parses() {
    let h: Health = parses("health_cloud");
    assert_eq!(h.mode.as_deref(), Some("cloud"));
    assert_eq!(h.log_min_level.as_deref(), Some("warn"));
    assert_eq!(h.uptime_secs, 0, "ausente → default, no error");
}

#[test]
fn whoami_round_trips() {
    round_trip::<Whoami>("whoami");
}

#[test]
fn max_versions_round_trips() {
    round_trip::<MaxVersionsBody>("max_versions_body");
    round_trip::<MaxVersionsResponse>("max_versions_response");
}

#[test]
fn game_round_trips() {
    round_trip::<Game>("game");
}

#[test]
fn save_round_trips() {
    round_trip::<Save>("save");
    round_trip::<CreateSaveRequest>("create_save_request");
}

#[test]
fn snapshot_round_trips() {
    round_trip::<Snapshot>("snapshot");
    round_trip::<SnapshotDetail>("snapshot_detail");
}

#[test]
fn logs_round_trip() {
    round_trip::<LogBatch>("log_batch");
    round_trip::<LogIngestResponse>("log_ingest_response");
}

/// La respuesta del rename cloud (`SaveSummary`, que sigue definida aparte en
/// `server::cloud::routes::saves`) tiene que seguir entrando en el `Save`
/// compartido: omite los campos opcionales y emite el offset como `+00:00` en
/// vez de `Z`. Round-trip no aplica —al re-emitir se normaliza a `Z`, que es la
/// misma instante— pero el parseo sí, y es lo que hace el cliente.
#[test]
fn cloud_rename_response_parses() {
    let save: Save = parses("save_cloud_rename");
    assert_eq!(save.game_slug.as_str(), "stardew-valley");
    assert_eq!(save.label, "granja");
    assert!(
        save.snapshot_count.is_none(),
        "cloud no calcula el agregado"
    );
    assert_eq!(save.created_at.offset(), time::UtcOffset::UTC);
}

/// Los valores que cruzan el wire pasan por la puerta de `ids`, así que el
/// golden también fija que los ids de la release **siguen siendo válidos** hoy.
/// Si alguien endurece un `parse` de más, esto cae antes que un usuario.
#[test]
fn release_values_still_pass_the_gate() {
    let save: Save = parses("save");
    assert_eq!(save.id.as_str(), "3f2504e0-4f89-41d3-9a0c-0305e82c3301");
    assert_eq!(save.game_slug.as_str(), "stardew-valley");

    let whoami: Whoami = parses("whoami");
    assert_eq!(whoami.username.as_str(), "jacka");

    let detail: SnapshotDetail = parses("snapshot_detail");
    assert_eq!(detail.files.len(), 2);
    assert_eq!(detail.files[0].sha256.as_ref().unwrap().as_str().len(), 64);

    let batch: LogBatch = parses("log_batch");
    assert!(batch.device.fingerprint.is_some());
    assert_eq!(batch.entries.len(), 2);
    assert!(batch.entries[1].fields.is_none());
}
