//! Admin-only operational endpoints for self-hosted instances.
//!
//! These routes are mounted **only** by `run_self_hosted` in `main.rs`,
//! never by the cloud router (`cloud/run.rs`), so they don't exist on the
//! managed Fly.io instance. They sit behind the same `require_auth`
//! middleware as everything else and additionally require
//! `AuthUser.is_admin`.
//!
//! See ADR 0017 for the full design of remote-triggered server upgrades.

use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::auth::AuthUser;
use crate::routes::health::ServerState;
use crate::routes::session::SESSION_DEVICE_NAME;

/// Filename of the upgrade marker, dropped in the server's writable
/// `data_dir`. A root systemd path-unit (`hoard-upgrade.path`) watches for
/// it and runs the privileged upgrade. Keep in sync with
/// `deploy/systemd/hoard-upgrade.path`.
pub const UPGRADE_MARKER: &str = ".upgrade-requested";

#[derive(Serialize)]
pub struct UpgradeAck {
    pub status: &'static str,
}

/// `POST /v1/admin/upgrade` — request a self-upgrade of this server.
///
/// The web process is sandboxed (see `deploy/systemd/hoard-server.service`)
/// and deliberately cannot touch its own binary. All it does here is drop a
/// marker file in `data_dir`; the root oneshot does the download + signature
/// check + binary swap + restart. We **ignore any request body**: the
/// privileged side always installs the latest *signed* canonical release, so
/// even a forged request can't choose what gets installed.
pub async fn upgrade(
    Extension(user): Extension<AuthUser>,
    State(state): State<Arc<ServerState>>,
) -> Result<(StatusCode, Json<UpgradeAck>), (StatusCode, String)> {
    if !user.is_admin {
        return Err((
            StatusCode::FORBIDDEN,
            "remote upgrade requires an admin token".to_string(),
        ));
    }
    if !state.config.server.allow_remote_upgrade {
        return Err((
            StatusCode::FORBIDDEN,
            "remote upgrade is disabled on this server (server.allow_remote_upgrade = false)"
                .to_string(),
        ));
    }

    let marker = state.config.storage.data_dir.join(UPGRADE_MARKER);
    // Content is informational only — the oneshot reads nothing from it.
    let body = format!(
        "requested_by={}\nrequested_at={}\n",
        user.username,
        now_unix(),
    );
    tokio::fs::write(&marker, body).await.map_err(|e| {
        tracing::error!(error = %e, path = %marker.display(), "failed to write upgrade marker");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not schedule the upgrade".to_string(),
        )
    })?;

    tracing::warn!(
        requested_by = %user.username,
        marker = %marker.display(),
        "remote upgrade scheduled; root oneshot will pick it up"
    );

    Ok((
        StatusCode::ACCEPTED,
        Json(UpgradeAck {
            status: "scheduled",
        }),
    ))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Operator views behind the panel's "server" section.
//
// Everything below is `is_admin`-only and self-hosted-only. The gate lives in
// each handler rather than in a layer because these hang off the same authed
// router as the rest, and a second middleware stack that could drift out of
// sync with this one is a worse trade than five explicit checks.
//
// What is deliberately NOT here: deleting a user, migrating storage backends,
// verifying every object. Those are `hoard-admin` subcommands and they stay
// there — each one is long-running or irreversible, and both properties are
// better served by a terminal that can print progress and refuse to be closed
// than by a browser tab.
// ---------------------------------------------------------------------------

type ApiError = (StatusCode, Json<serde_json::Value>);

/// `(id, user_id, username, device_name, created_at, last_used_at, expires_at,
/// revoked_at)` straight from the join.
type TokenRecord = (
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// `(at, username, level, target, message, device_name, device_os,
/// app_version, fields)`.
type LogRecord = (
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn err(status: StatusCode, code: &str) -> ApiError {
    (status, Json(serde_json::json!({ "error": code })))
}

fn db_error(e: sqlx::Error, what: &str) -> ApiError {
    tracing::error!(error = %e, "{what} failed");
    err(StatusCode::INTERNAL_SERVER_ERROR, "internal")
}

fn require_admin(user: &AuthUser) -> Result<(), ApiError> {
    if user.is_admin {
        Ok(())
    } else {
        Err(err(StatusCode::FORBIDDEN, "admin_only"))
    }
}

#[derive(Serialize)]
pub struct AdminOverview {
    pub totals: Totals,
    pub users: Vec<UserRow>,
}

#[derive(Serialize)]
pub struct Totals {
    pub users: i64,
    pub admins: i64,
    pub saves: i64,
    pub versions: i64,
    pub trashed_versions: i64,
    pub logical_bytes: i64,
    pub stored_bytes: i64,
    pub trash_bytes: i64,
    /// Blobs and chunks nothing references any more. They are already spent
    /// disk; the hourly cleanup collects them. A number that only grows means
    /// the sweep is failing, which is the kind of thing an operator can only
    /// notice if someone shows it to them.
    pub orphan_objects: i64,
    pub orphan_bytes: i64,
    pub objects: i64,
    /// Size of the SQLite file plus its write-ahead log, or `null` when the
    /// database is not a local file we can stat (a Postgres cloud deployment
    /// never mounts these routes, so in practice: an unusual URL).
    pub db_bytes: Option<i64>,
    pub client_logs: i64,
    pub oldest_snapshot_at: Option<String>,
}

#[derive(Serialize)]
pub struct UserRow {
    pub id: String,
    pub username: String,
    pub is_admin: bool,
    pub used_bytes: i64,
    pub quota_bytes: i64,
    pub stored_bytes: i64,
    pub saves: i64,
    pub versions: i64,
    pub devices: i64,
    pub last_seen_at: Option<String>,
    pub created_at: String,
}

/// `GET /v1/admin/overview`
pub async fn overview(
    Extension(user): Extension<AuthUser>,
    State(state): State<Arc<ServerState>>,
) -> Result<Json<AdminOverview>, ApiError> {
    require_admin(&user)?;
    let pool = &state.pool;

    let (users, admins): (i64, i64) =
        sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(is_admin),0) FROM users")
            .fetch_one(pool)
            .await
            .map_err(|e| db_error(e, "admin user counts"))?;

    let (saves,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM saves")
        .fetch_one(pool)
        .await
        .map_err(|e| db_error(e, "admin save count"))?;

    let (versions, logical_bytes): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(total_size_bytes),0) FROM snapshots \
         WHERE deleted_at IS NULL",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| db_error(e, "admin snapshot totals"))?;

    let (trashed_versions, trash_bytes): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(total_size_bytes),0) FROM snapshots \
         WHERE deleted_at IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| db_error(e, "admin trash totals"))?;

    let (objects, stored_bytes, orphan_objects, orphan_bytes): (i64, i64, i64, i64) =
        sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM blobs) + (SELECT COUNT(*) FROM chunks), \
                    (SELECT COALESCE(SUM(size_bytes),0) FROM blobs) \
                  + (SELECT COALESCE(SUM(size_bytes),0) FROM chunks), \
                    (SELECT COUNT(*) FROM blobs WHERE refcount <= 0) \
                  + (SELECT COUNT(*) FROM chunks WHERE refcount <= 0), \
                    (SELECT COALESCE(SUM(size_bytes),0) FROM blobs WHERE refcount <= 0) \
                  + (SELECT COALESCE(SUM(size_bytes),0) FROM chunks WHERE refcount <= 0)",
        )
        .fetch_one(pool)
        .await
        .map_err(|e| db_error(e, "admin object totals"))?;

    let (client_logs,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM client_logs")
        .fetch_one(pool)
        .await
        .map_err(|e| db_error(e, "admin log count"))?;

    let (oldest_snapshot_at,): (Option<String>,) =
        sqlx::query_as("SELECT MIN(created_at) FROM snapshots")
            .fetch_one(pool)
            .await
            .map_err(|e| db_error(e, "admin oldest snapshot"))?;

    let rows: Vec<(String, String, i64, i64, i64, String)> = sqlx::query_as(
        "SELECT id, username, is_admin, storage_used_bytes, storage_quota_bytes, created_at \
         FROM users ORDER BY username COLLATE NOCASE",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| db_error(e, "admin user list"))?;

    let mut users_out = Vec::with_capacity(rows.len());
    for (id, username, admin_flag, used, quota, created_at) in rows {
        // Per-user rollups one query at a time. A self-hosted instance has a
        // handful of users, and the readable version wins over a five-way join
        // that has to fake outer-join semantics for users with no saves yet.
        let (saves, versions): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(DISTINCT s.id), \
                    COUNT(CASE WHEN sn.deleted_at IS NULL THEN sn.id END) \
             FROM saves s LEFT JOIN snapshots sn ON sn.save_id = s.id \
             WHERE s.user_id = ?",
        )
        .bind(&id)
        .fetch_one(pool)
        .await
        .map_err(|e| db_error(e, "admin per-user saves"))?;

        let (stored_bytes,): (i64,) = sqlx::query_as(
            "SELECT (SELECT COALESCE(SUM(size_bytes),0) FROM blobs WHERE user_id = ?1) \
                  + (SELECT COALESCE(SUM(size_bytes),0) FROM chunks WHERE user_id = ?1)",
        )
        .bind(&id)
        .fetch_one(pool)
        .await
        .map_err(|e| db_error(e, "admin per-user bytes"))?;

        let (devices, last_seen_at): (i64, Option<String>) =
            sqlx::query_as("SELECT COUNT(*), MAX(last_seen_at) FROM devices WHERE user_id = ?")
                .bind(&id)
                .fetch_one(pool)
                .await
                .map_err(|e| db_error(e, "admin per-user devices"))?;

        users_out.push(UserRow {
            id,
            username,
            is_admin: admin_flag != 0,
            used_bytes: used,
            quota_bytes: quota,
            stored_bytes,
            saves,
            versions,
            devices,
            last_seen_at,
            created_at,
        });
    }

    Ok(Json(AdminOverview {
        totals: Totals {
            users,
            admins,
            saves,
            versions,
            trashed_versions,
            logical_bytes,
            stored_bytes,
            trash_bytes,
            orphan_objects,
            orphan_bytes,
            objects,
            db_bytes: db_file_bytes(&state.config.database.url),
            client_logs,
            oldest_snapshot_at,
        },
        users: users_out,
    }))
}

#[derive(Deserialize)]
pub struct UserPatch {
    pub is_admin: Option<bool>,
    pub storage_quota_bytes: Option<i64>,
}

/// `PATCH /v1/admin/users/:id` — flip the admin bit or move a quota.
///
/// Both are reversible from this same screen, which is the line for what the
/// panel is allowed to do. The one irreversible move it refuses is removing
/// the last admin: the flag guards its own route, so a server with zero admins
/// cannot promote anyone back without `hoard-admin` and a shell on the box.
pub async fn patch_user(
    Extension(user): Extension<AuthUser>,
    State(state): State<Arc<ServerState>>,
    Path(target_id): Path<String>,
    Json(body): Json<UserPatch>,
) -> Result<StatusCode, ApiError> {
    require_admin(&user)?;

    let existing: Option<(String, i64)> =
        sqlx::query_as("SELECT username, is_admin FROM users WHERE id = ?")
            .bind(&target_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(|e| db_error(e, "admin patch lookup"))?;
    let Some((target_name, was_admin)) = existing else {
        return Err(err(StatusCode::NOT_FOUND, "no_such_user"));
    };

    if let Some(quota) = body.storage_quota_bytes {
        if quota < 0 {
            return Err(err(StatusCode::BAD_REQUEST, "bad_quota"));
        }
        sqlx::query("UPDATE users SET storage_quota_bytes = ? WHERE id = ?")
            .bind(quota)
            .bind(&target_id)
            .execute(&state.pool)
            .await
            .map_err(|e| db_error(e, "admin quota update"))?;
        tracing::info!(actor = %user.username, target = %target_name, quota, "admin: quota set");
    }

    if let Some(make_admin) = body.is_admin {
        if !make_admin && was_admin != 0 {
            let (admins,): (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM users WHERE is_admin <> 0")
                    .fetch_one(&state.pool)
                    .await
                    .map_err(|e| db_error(e, "admin count"))?;
            if admins <= 1 {
                return Err(err(StatusCode::CONFLICT, "last_admin"));
            }
        }
        sqlx::query("UPDATE users SET is_admin = ? WHERE id = ?")
            .bind(make_admin as i64)
            .bind(&target_id)
            .execute(&state.pool)
            .await
            .map_err(|e| db_error(e, "admin flag update"))?;
        tracing::info!(actor = %user.username, target = %target_name, make_admin, "admin: role set");
    }

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct TokenRow {
    pub id: String,
    pub user_id: String,
    pub username: String,
    pub device_name: Option<String>,
    /// True when this row is a browser session rather than a device's token.
    pub is_session: bool,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
}

#[derive(Deserialize)]
pub struct TokenQuery {
    pub user_id: Option<String>,
    /// Revoked tokens are hidden by default; they are audit trail, not state.
    #[serde(default)]
    pub include_revoked: bool,
}

/// `GET /v1/admin/tokens`
pub async fn tokens(
    Extension(user): Extension<AuthUser>,
    State(state): State<Arc<ServerState>>,
    Query(q): Query<TokenQuery>,
) -> Result<Json<Vec<TokenRow>>, ApiError> {
    require_admin(&user)?;

    let rows: Vec<TokenRecord> = sqlx::query_as(
        "SELECT t.id, t.user_id, u.username, t.device_name, t.created_at, \
                t.last_used_at, t.expires_at, t.revoked_at \
         FROM api_tokens t JOIN users u ON u.id = t.user_id \
         WHERE (?1 IS NULL OR t.user_id = ?1) \
           AND (?2 = 1 OR t.revoked_at IS NULL) \
         ORDER BY t.created_at DESC",
    )
    .bind(&q.user_id)
    .bind(q.include_revoked as i64)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| db_error(e, "admin token list"))?;

    Ok(Json(
        rows.into_iter()
            .map(
                |(id, user_id, username, device_name, created_at, last, exp, rev)| TokenRow {
                    is_session: device_name.as_deref() == Some(SESSION_DEVICE_NAME),
                    id,
                    user_id,
                    username,
                    device_name,
                    created_at,
                    last_used_at: last,
                    expires_at: exp,
                    revoked_at: rev,
                },
            )
            .collect(),
    ))
}

/// `POST /v1/admin/tokens/:id/revoke`
///
/// The token itself is never readable here — only its SHA-256 is stored — so
/// revocation goes by row id. Revoking your own session is allowed and logs you
/// out on the next request, which is the correct behaviour for "I clicked the
/// wrong row".
pub async fn revoke_token(
    Extension(user): Extension<AuthUser>,
    State(state): State<Arc<ServerState>>,
    Path(token_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_admin(&user)?;

    let affected = sqlx::query(
        "UPDATE api_tokens SET revoked_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') \
         WHERE id = ? AND revoked_at IS NULL",
    )
    .bind(&token_id)
    .execute(&state.pool)
    .await
    .map_err(|e| db_error(e, "admin token revoke"))?
    .rows_affected();

    if affected == 0 {
        return Err(err(StatusCode::NOT_FOUND, "no_such_token"));
    }
    tracing::info!(actor = %user.username, token_id, "admin: token revoked");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct LogRow {
    pub at: String,
    pub username: String,
    pub level: String,
    pub target: Option<String>,
    pub message: String,
    pub device_name: Option<String>,
    pub device_os: Option<String>,
    pub app_version: Option<String>,
    pub fields: Option<String>,
}

#[derive(Deserialize)]
pub struct LogQuery {
    pub user_id: Option<String>,
    pub level: Option<String>,
    pub q: Option<String>,
    pub limit: Option<i64>,
}

/// `GET /v1/admin/logs`
///
/// First reader `client_logs` has ever had. Clients have been shipping their
/// diagnostics to the server since migration 0012 and the only code that
/// touched the table was the retention sweep deleting them — an operator
/// debugging a device had to open SQLite by hand.
pub async fn logs(
    Extension(user): Extension<AuthUser>,
    State(state): State<Arc<ServerState>>,
    Query(q): Query<LogQuery>,
) -> Result<Json<Vec<LogRow>>, ApiError> {
    require_admin(&user)?;
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    // Substring search, escaped so a user's `%` or `_` matches itself instead
    // of turning into a wildcard.
    let needle = q.q.as_ref().map(|raw| {
        format!(
            "%{}%",
            raw.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        )
    });

    let rows: Vec<LogRecord> = sqlx::query_as(
        "SELECT COALESCE(l.client_ts, l.received_at), u.username, l.level, l.target, \
                l.message, l.device_name, l.device_os, l.app_version, l.fields \
         FROM client_logs l JOIN users u ON u.id = l.user_id \
         WHERE (?1 IS NULL OR l.user_id = ?1) \
           AND (?2 IS NULL OR l.level = ?2) \
           AND (?3 IS NULL OR l.message LIKE ?3 ESCAPE '\\' OR l.target LIKE ?3 ESCAPE '\\') \
         ORDER BY l.received_at DESC LIMIT ?4",
    )
    .bind(&q.user_id)
    .bind(&q.level)
    .bind(&needle)
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| db_error(e, "admin logs"))?;

    Ok(Json(
        rows.into_iter()
            .map(
                |(at, username, level, target, message, dev, os, ver, fields)| LogRow {
                    at,
                    username,
                    level,
                    target,
                    message,
                    device_name: dev,
                    device_os: os,
                    app_version: ver,
                    fields,
                },
            )
            .collect(),
    ))
}

/// Size of the SQLite file and its `-wal` sidecar. The WAL is included because
/// on a busy instance it is routinely the larger of the two, and an operator
/// looking at "database" wants the number that explains their disk.
fn db_file_bytes(url: &str) -> Option<i64> {
    let path = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))?
        .split('?')
        .next()?;
    if path.is_empty() || path == ":memory:" {
        return None;
    }
    let main = std::fs::metadata(path).ok()?.len() as i64;
    let wal = std::fs::metadata(format!("{path}-wal"))
        .map(|m| m.len() as i64)
        .unwrap_or(0);
    Some(main + wal)
}

#[cfg(test)]
mod db_path_tests {
    use super::db_file_bytes;

    #[test]
    fn unusual_urls_report_nothing_rather_than_a_wrong_number() {
        assert!(db_file_bytes("sqlite::memory:").is_none());
        assert!(db_file_bytes("postgres://localhost/hoard").is_none());
        assert!(db_file_bytes("sqlite:///nonexistent/hoard.db").is_none());
    }
}
