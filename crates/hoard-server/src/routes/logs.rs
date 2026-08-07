//! `POST /v1/logs` — ingest of client diagnostic logs (self-hosted).
//!
//! Connected apps (desktop/CLI) ship batches of their `tracing` events here.
//! Self-hosted accepts *every* level; the wire shape and the batch caps are
//! shared with the cloud route (`cloud::routes::logs`), which additionally
//! filters to INFO+.

use axum::{
    extract::{Extension, State},
    http::StatusCode,
    response::Json,
};
use hoard_core::wire::{LogBatch, LogIngestResponse};
use std::sync::Arc;

use crate::auth::AuthUser;
use crate::routes::health::ServerState;

/// Max log entries accepted in a single request. Pairs with the per-route
/// body-size limit to bound abuse.
pub const MAX_BATCH_ENTRIES: usize = 500;
/// Per-request body cap for the logs endpoint (~256 KiB).
pub const MAX_BATCH_BYTES: usize = 256 * 1024;

// El cuerpo (`LogBatch` / `LogEntry` / `DeviceMeta`) y la respuesta viven en
// `hoard_core::wire` (ADR 0021 C.6). Este par era drift real: el cliente
// declaraba `target` y `ts` obligatorios y el server los tenía `Option`.

// El orden de niveles y la regla de qué se guarda viven en `hoard_core::wire`
// (`level_rank` / `ships_at` / `CLOUD_MIN_RANK`), compartidos con el cliente:
// estaban escritos tres veces —aquí, en el namespace cloud y en el enviador del
// agente— y una regla duplicada es una fuga en silencio esperando su turno. Si
// el cliente filtra a un nivel y el server a otro, o se manda lo que el server
// tira o se tira lo que el cliente manda, y nadie se entera.

pub async fn ingest(
    Extension(user): Extension<AuthUser>,
    State(state): State<Arc<ServerState>>,
    Json(batch): Json<LogBatch>,
) -> Result<(StatusCode, Json<LogIngestResponse>), StatusCode> {
    if batch.entries.len() > MAX_BATCH_ENTRIES {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }

    let user_id = user.user_id.to_string();
    let mut accepted = 0usize;

    for entry in &batch.entries {
        let id = uuid::Uuid::new_v4().to_string();
        let level = entry.level.trim().to_ascii_lowercase();
        let fields_json = entry.fields.as_ref().map(|v| v.to_string());

        let res = sqlx::query(
            "INSERT INTO client_logs
                (id, user_id, device_name, device_os, device_fingerprint,
                 app_version, level, target, message, fields, client_ts)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&user_id)
        .bind(&batch.device.name)
        .bind(&batch.device.os)
        .bind(batch.device.fingerprint.as_ref().map(|f| f.as_str()))
        .bind(&batch.device.app_version)
        .bind(&level)
        .bind(&entry.target)
        .bind(&entry.message)
        .bind(&fields_json)
        .bind(&entry.ts)
        .execute(&state.pool)
        .await;

        match res {
            Ok(_) => accepted += 1,
            Err(e) => {
                tracing::error!(error = %e, "client log insert failed");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }

    Ok((StatusCode::OK, Json(LogIngestResponse { accepted })))
}

// La matriz de la regla se testea una sola vez, donde vive: `hoard_core::wire`
// (`one_rule_decides_what_travels_and_what_is_stored`).
