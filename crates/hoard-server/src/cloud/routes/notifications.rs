//! Operator broadcast messages for the desktop's bell panel.
//!
//! `GET /v1/notifications` lists them; `POST /v1/notifications/:id/dismiss`
//! records a per-user dismissal. Rows are inserted exclusively via direct SQL
//! with the service role (`tools/send-notification.sh`), never through HTTP,
//! so the only possible sender is the operator. The `notifications` table has
//! no user_id column — broadcasts by construction (see migration 0032) — but
//! DELIVERY is per-user: the list handler filters against
//! `notification_dismissals` (migration 0033) and the caller's
//! `profiles.created_at`, so a user only ever sees broadcasts sent after they
//! signed up and never one they dismissed (on any device — the dismissal is
//! server-side, the client's localStorage tombstones are just an optimistic
//! cache).
//!
//! Delivery is poll + push: the agent polls this endpoint on its cloud tick
//! (keeping a `since` cursor so nothing is re-delivered), and Supabase
//! Realtime pushes the INSERT so open apps ring the bell within seconds. The
//! server-side dismiss/created_at filter applies to both paths identically.

use axum::{
    extract::{Path, Query, State},
    response::{Json, StatusCode},
    Extension,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::cloud::auth::CloudUser;
use crate::cloud::errors::CloudError;
use crate::cloud::state::CloudState;

/// Cap per response. Broadcasts are rare (a few per month); a fresh install
/// only ever needs the handful of still-valid ones, not the full history.
const MAX_NOTIFICATIONS: i64 = 10;

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// RFC3339 cursor: only return rows created strictly after this instant.
    /// Absent on first run — the client then gets the current (non-expired)
    /// broadcasts once and starts its cursor from the newest.
    pub since: Option<String>,
}

/// Wire shape matches the UI's `ServerNotification` (stores/notifications.ts)
/// plus `created_at` for the client's cursor.
#[derive(Debug, Serialize)]
pub struct NotificationOut {
    pub id: Uuid,
    pub title: String,
    pub body: String,
    pub priority: String,
    pub action_url: Option<String>,
    pub action_label: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct NotificationListOut {
    pub notifications: Vec<NotificationOut>,
}

pub async fn list(
    State(state): State<CloudState>,
    Extension(user): Extension<CloudUser>,
    Query(q): Query<ListQuery>,
) -> Result<Json<NotificationListOut>, CloudError> {
    let since = match q.since.as_deref() {
        Some(s) => Some(
            OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                .map_err(|_| CloudError::BadRequest("invalid `since` (want RFC3339)".into()))?,
        ),
        None => None,
    };

    let rows: Vec<(
        Uuid,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        OffsetDateTime,
    )> = sqlx::query_as(
        // Per-user delivery filter (migration 0033):
        //   1. `created_at >= profiles.created_at` — a user only sees
        //      broadcasts created AFTER their signup, so a fresh account isn't
        //      greeted by months of historical operator messages.
        //   2. `NOT EXISTS (... notification_dismissals ...)` — a broadcast
        //      the user dismissed is never re-delivered, on any device. The
        //      dismissal is server-side; the client's localStorage tombstones
        //      are an optimistic cache only.
        // Both scope by the caller's `user_id`; the `since` cursor and the
        // LIMIT cap are unchanged.
        "SELECT id, title, body, priority, action_url, action_label, created_at
           FROM notifications
          WHERE (expires_at IS NULL OR expires_at > now())
            AND created_at >= (SELECT created_at FROM profiles WHERE user_id = $1)
            AND NOT EXISTS (
                SELECT 1 FROM notification_dismissals d
                 WHERE d.user_id = $1 AND d.notification_id = notifications.id
            )
            AND ($2::timestamptz IS NULL OR created_at > $2)
          ORDER BY created_at DESC
          LIMIT $3",
    )
    .bind(user.user_id)
    .bind(since)
    .bind(MAX_NOTIFICATIONS)
    .fetch_all(&state.pool)
    .await?;

    let notifications = rows
        .into_iter()
        .map(
            |(id, title, body, priority, action_url, action_label, created_at)| NotificationOut {
                id,
                title,
                body,
                priority,
                action_url,
                action_label,
                created_at: created_at
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
            },
        )
        .collect();
    Ok(Json(NotificationListOut { notifications }))
}

/// `POST /v1/notifications/:id/dismiss` — record a per-user dismissal so the
/// broadcast is never re-delivered to that user (on any device or after a
/// reinstall). Idempotent: `ON CONFLICT DO NOTHING` makes re-dismissing the
/// same broadcast a no-op. A `notification_id` that doesn't exist trips the
/// FK; we swallow that as 204 too — the client only ever dismisses ids the
/// server just delivered, so a FK miss is a benign race (the broadcast
/// expired/was deleted between fetch and dismiss) and the user's intent
/// ("don't show this again") is already satisfied. Responds `204 No Content`.
pub async fn dismiss(
    State(state): State<CloudState>,
    Extension(user): Extension<CloudUser>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, CloudError> {
    let res = sqlx::query(
        "INSERT INTO notification_dismissals (user_id, notification_id)
         VALUES ($1, $2)
         ON CONFLICT (user_id, notification_id) DO NOTHING",
    )
    .bind(user.user_id)
    .bind(id)
    .execute(&state.pool)
    .await;
    match res {
        Ok(_) => Ok(StatusCode::NO_CONTENT),
        // 23503 = foreign_key_violation: the notification_id no longer exists.
        // No-op (see handler doc) — the dismissal is moot.
        Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some("23503") => {
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(CloudError::from(e)),
    }
}
