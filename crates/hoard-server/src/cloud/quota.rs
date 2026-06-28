//! Quota enforcement.
//!
//! Two consumers:
//!
//! 1. `check_storage` — called from upload handlers *before* presigning a
//!    PUT URL. Returns `Err(QuotaResponse)` (which serializes to 402) if
//!    the user would exceed their plan, otherwise `Ok(())`.
//! 2. `check_devices` — called from `/v1/profiles/sync` and similar
//!    bootstrap endpoints. Same shape.

use crate::cloud::errors::CloudError;
use crate::cloud::plans::{Plan, PlanLimits};
use crate::cloud::state::CloudState;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize)]
pub struct QuotaInfo {
    pub plan: &'static str,
    pub used_bytes: u64,
    pub limit_bytes: u64,
    pub devices_used: u32,
    pub devices_limit: u32,
}

#[derive(Debug, Serialize)]
pub struct QuotaResponse {
    pub error: &'static str,
    pub code: &'static str,
    pub plan: &'static str,
    pub used_bytes: u64,
    pub limit_bytes: u64,
    pub requested_bytes: u64,
    pub upgrade_url: String,
}

impl IntoResponse for QuotaResponse {
    fn into_response(self) -> Response {
        (StatusCode::PAYMENT_REQUIRED, Json(self)).into_response()
    }
}

/// Fetch plan + storage_bytes for a user. Returns None if no profile row
/// exists yet — callers should treat that as a 404 / "log in first".
pub async fn load(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Option<(PlanLimits, QuotaInfo)>, CloudError> {
    let row: Option<(String, i64, i32, Option<i64>)> = sqlx::query_as(
        "SELECT plan, storage_bytes, devices_count, storage_limit_bytes \
         FROM profiles WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    let Some((plan_s, used, devs, limit_override)) = row else {
        return Ok(None);
    };
    let plan = Plan::from_str(&plan_s).unwrap_or(Plan::Free);
    let mut limits = plan.limits();
    // Apply the per-user storage tier (Pro xN). Overriding here means every
    // downstream consumer — including `check_storage`, which reads
    // `limits.storage_bytes` directly — sees the bought limit.
    limits.storage_bytes = super::plans::effective_storage_limit(plan, limit_override);
    let info = QuotaInfo {
        plan: plan.as_str(),
        used_bytes: used.max(0) as u64,
        limit_bytes: limits.storage_bytes,
        devices_used: devs.max(0) as u32,
        devices_limit: limits.devices,
    };
    Ok(Some((limits, info)))
}

/// Settle a user's storage limit when a subscription becomes active, given the
/// tier size the active product grants (`new_override`, bytes; `None` = the
/// plan base). Called from the Polar webhook.
///
/// - Upgrade, or a downgrade that still fits the current footprint → apply
///   immediately and clear any pending change.
/// - Downgrade *below* the current footprint → don't shrink now; stash the new
///   size in `pending_storage_limit_bytes` with a `storage_limit_change_at`
///   deadline `STORAGE_DOWNGRADE_GRACE_DAYS` out. The user keeps the larger
///   limit (no purging) until then and `/v1/me` advertises the change.
pub async fn settle_storage_on_active(
    pool: &PgPool,
    user_id: Uuid,
    plan: Plan,
    new_override: Option<i64>,
    grace_days: i64,
) -> Result<(), CloudError> {
    use crate::cloud::plans::effective_storage_limit;

    let row: Option<(i64, Option<i64>)> = sqlx::query_as(
        "SELECT storage_bytes, storage_limit_bytes FROM profiles WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    let Some((used, current_override)) = row else {
        return Ok(());
    };
    let used = used.max(0);
    let current_eff = effective_storage_limit(plan, current_override) as i64;
    let new_eff = effective_storage_limit(plan, new_override) as i64;

    if new_eff >= current_eff || used <= new_eff {
        sqlx::query(
            "UPDATE profiles SET storage_limit_bytes = $1, pending_storage_limit_bytes = NULL, \
             storage_limit_change_at = NULL, updated_at = now() WHERE user_id = $2",
        )
        .bind(new_override)
        .bind(user_id)
        .execute(pool)
        .await?;
    } else {
        sqlx::query(
            "UPDATE profiles SET pending_storage_limit_bytes = $1, \
             storage_limit_change_at = now() + ($2::int * interval '1 day'), \
             updated_at = now() WHERE user_id = $3",
        )
        .bind(new_override)
        .bind(grace_days.max(0) as i32)
        .bind(user_id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Promote a pending downgrade whose grace window has elapsed into the live
/// limit. Idempotent and cheap — call before reading/enforcing the limit
/// (`get_me`, `maybe_purge`). Rows without a due change are untouched.
pub async fn apply_due_downgrade(pool: &PgPool, user_id: Uuid) -> Result<(), CloudError> {
    sqlx::query(
        "UPDATE profiles SET storage_limit_bytes = pending_storage_limit_bytes, \
         pending_storage_limit_bytes = NULL, storage_limit_change_at = NULL, updated_at = now() \
         WHERE user_id = $1 AND storage_limit_change_at IS NOT NULL \
         AND storage_limit_change_at <= now()",
    )
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// True if `used + extra` would exceed `limit`. Saturating add to avoid
/// pathological u64 overflow on a malformed request.
pub fn would_exceed(used: u64, extra: u64, limit: u64) -> bool {
    used.saturating_add(extra) > limit
}

pub fn quota_response(info: &QuotaInfo, requested: u64, upgrade_url: String) -> QuotaResponse {
    QuotaResponse {
        error: "storage quota exceeded",
        code: "quota_exceeded",
        plan: info.plan,
        used_bytes: info.used_bytes,
        limit_bytes: info.limit_bytes,
        requested_bytes: requested,
        upgrade_url,
    }
}

/// Helper used by upload handlers: load + check + return a quota-shaped
/// 402 response if exceeded.
pub async fn check_storage(
    state: &CloudState,
    user_id: Uuid,
    requested: u64,
) -> Result<QuotaInfo, Response> {
    let (limits, info) = match load(&state.pool, user_id).await {
        Ok(Some(x)) => x,
        Ok(None) => return Err(CloudError::NotFound("no profile").into_response()),
        Err(e) => return Err(e.into_response()),
    };
    if would_exceed(info.used_bytes, requested, limits.storage_bytes) {
        let url = state
            .config
            .cloud
            .as_ref()
            .map(|c| c.upgrade_url.clone())
            .unwrap_or_else(|| "https://hoard.services/upgrade".to_string());
        return Err(quota_response(&info, requested, url).into_response());
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn would_exceed_basic() {
        assert!(would_exceed(100, 10, 100));
        assert!(!would_exceed(100, 0, 100));
        assert!(!would_exceed(99, 1, 100));
        assert!(would_exceed(99, 2, 100));
    }

    #[test]
    fn would_exceed_overflow_safe() {
        // saturating_add(u64::MAX, 1) == u64::MAX; that should compare to
        // limit and exceed it.
        assert!(would_exceed(u64::MAX, 1, 100));
    }

    #[test]
    fn quota_response_body_shape() {
        let info = QuotaInfo {
            plan: "free",
            used_bytes: 400 * 1024 * 1024,
            limit_bytes: 500 * 1024 * 1024,
            devices_used: 1,
            devices_limit: 1,
        };
        let resp = quota_response(&info, 200 * 1024 * 1024, "https://x/upgrade".into());
        assert_eq!(resp.code, "quota_exceeded");
        assert_eq!(resp.plan, "free");
        assert_eq!(resp.requested_bytes, 200 * 1024 * 1024);
    }
}
