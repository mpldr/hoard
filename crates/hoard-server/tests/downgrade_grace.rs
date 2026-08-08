//! The Pro→Free downgrade, end to end against a real Postgres.
//!
//! This is the rehearsal for the incident of ago-2026: a Pro account holding
//! 6.8 GB dropped to Free (2 GB) and the server shrank the limit the same
//! second, so the auto-purge deleted the user's version history with no notice
//! and every later upload bounced off a 402. The grace window meant to prevent
//! exactly that was dead code — `settle_storage_on_active` sized "how much room
//! do you have today" with the plan being moved *to*, so Pro→Free resolved to
//! 2 GB on both sides and never scheduled anything.
//!
//! It runs against a throwaway database instead of a paid subscription because
//! nothing here is Polar's decision: Polar only says "this subscription ended".
//! Everything that matters — grant, deadline, what the limit resolves to while
//! the plan column already says `free` — is [`quota::settle_storage_limit`] and
//! [`plans::resolved_storage_limit`], and both are reachable from a test.
//!
//! Skipped unless `HOARD_PG_TEST_URL` is set, like the S3 one. To run it:
//!
//! ```sh
//! docker run -d --name hoard-pg -p 55432:5432 \
//!   -e POSTGRES_PASSWORD=hoard -e POSTGRES_DB=hoard postgres:17
//! export HOARD_PG_TEST_URL=postgres://postgres:hoard@localhost:55432/hoard
//! cargo test -p hoard-server --features cloud --test downgrade_grace -- --nocapture
//! ```
//!
//! **Never point it at production.** It creates and deletes its own profile
//! rows, but the migrations run on whatever database it's given.

#![cfg(feature = "cloud")]

use hoard_server::cloud::plans::Plan;
use hoard_server::cloud::quota::{self, SettleOutcome};
use sqlx::PgPool;
use uuid::Uuid;

const GB: i64 = 1024 * 1024 * 1024;
const GRACE_DAYS: i64 = 30;

/// Connect + migrate, or `None` when the env var isn't set (CI's no-op path).
async fn pool() -> Option<PgPool> {
    let url = std::env::var("HOARD_PG_TEST_URL").ok()?;
    let pool = hoard_server::cloud::db::connect(&url, 5)
        .await
        .expect("connect to the test database");
    // The migrations assume Supabase: an `auth.users` table to hang the
    // `profiles` FK off (0013), an `auth.uid()` for the RLS policies, and the
    // `anon` / `authenticated` roles the admin-metrics grants name (0030). A
    // bare Postgres has none of them, so stand up the shapes they reference.
    // Nothing here authenticates anything — RLS is never the path the server
    // takes (it connects as the owner); the objects just have to be creatable.
    for role in ["anon", "authenticated", "service_role"] {
        // No IF NOT EXISTS for roles before PG 16's syntax, and re-running the
        // suite must stay idempotent, so swallow the duplicate.
        let _ = sqlx::query(&format!("CREATE ROLE {role} NOLOGIN"))
            .execute(&pool)
            .await;
    }
    sqlx::query("CREATE SCHEMA IF NOT EXISTS auth")
        .execute(&pool)
        .await
        .expect("auth schema");
    sqlx::query("CREATE TABLE IF NOT EXISTS auth.users (id UUID PRIMARY KEY)")
        .execute(&pool)
        .await
        .expect("auth.users");
    sqlx::query(
        "CREATE OR REPLACE FUNCTION auth.uid() RETURNS UUID LANGUAGE sql STABLE AS $$ SELECT NULL::uuid $$",
    )
    .execute(&pool)
    .await
    .expect("auth.uid()");
    hoard_server::cloud::db::run_migrations(&pool)
        .await
        .expect("migrations");
    Some(pool)
}

/// A Pro profile storing `used` bytes, on the base tier (no bought override).
async fn seed_pro(pool: &PgPool, used: i64) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO auth.users (id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(id)
        .execute(pool)
        .await
        .expect("auth user");
    sqlx::query(
        "INSERT INTO profiles (user_id, email, plan, storage_bytes) VALUES ($1, $2, 'pro', $3)",
    )
    .bind(id)
    .bind(format!("{id}@test.invalid"))
    .bind(used)
    .execute(pool)
    .await
    .expect("profile");
    id
}

/// What the webhook does after settling: flip the plan column.
async fn set_plan(pool: &PgPool, id: Uuid, plan: &str) {
    sqlx::query("UPDATE profiles SET plan = $1 WHERE user_id = $2")
        .bind(plan)
        .bind(id)
        .execute(pool)
        .await
        .expect("plan flip");
}

async fn enforced_limit(pool: &PgPool, id: Uuid) -> u64 {
    let (limits, _info) = quota::load(pool, id)
        .await
        .expect("quota load")
        .expect("profile exists");
    limits.storage_bytes
}

async fn cleanup(pool: &PgPool, id: Uuid) {
    let _ = sqlx::query("DELETE FROM profiles WHERE user_id = $1")
        .bind(id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await;
}

/// The regression itself: a Pro account over Free's limit keeps its old room
/// until the deadline, *including after the plan column says `free`*, and only
/// then collapses.
#[tokio::test]
async fn pro_to_free_over_footprint_gets_the_window() {
    let Some(pool) = pool().await else {
        eprintln!("HOARD_PG_TEST_URL unset — skipping");
        return;
    };
    // The real account: 6.8 GB stored, dropping to a 2 GB plan.
    let id = seed_pro(&pool, 6_800_000_000).await;

    let outcome = quota::settle_storage_limit(&pool, id, Plan::Free, None, GRACE_DAYS)
        .await
        .expect("settle");
    assert_eq!(
        outcome,
        SettleOutcome::Scheduled {
            grant_bytes: 100 * GB,
            target_bytes: 2 * GB,
        },
        "a downgrade below the footprint schedules, it doesn't apply"
    );

    // The webhook flips the plan right after. This is the exact moment the old
    // code lost: `plan` says free, so a limit derived from the plan alone is
    // 2 GB and the purge starts eating history.
    set_plan(&pool, id, "free").await;
    assert_eq!(
        enforced_limit(&pool, id).await,
        100 * GB as u64,
        "inside the window the old limit still rules, plan column notwithstanding"
    );

    // Webhook retries / the `/v1/me` expiry sweep must not push the deadline.
    let before: Option<time::OffsetDateTime> =
        sqlx::query_scalar("SELECT storage_limit_change_at FROM profiles WHERE user_id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("deadline");
    let again = quota::settle_storage_limit(&pool, id, Plan::Free, None, GRACE_DAYS)
        .await
        .expect("settle again");
    assert_eq!(
        again,
        SettleOutcome::AlreadyScheduled {
            target_bytes: 2 * GB
        },
    );
    let after: Option<time::OffsetDateTime> =
        sqlx::query_scalar("SELECT storage_limit_change_at FROM profiles WHERE user_id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("deadline");
    assert_eq!(before, after, "a second event can't extend the window");

    // Wind the clock past the deadline: now — and only now — it shrinks.
    sqlx::query("UPDATE profiles SET storage_limit_change_at = now() - interval '1 minute' WHERE user_id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .expect("rewind");
    quota::apply_due_downgrade(&pool, id).await.expect("promote");
    assert_eq!(
        enforced_limit(&pool, id).await,
        2 * GB as u64,
        "past the deadline the Free limit applies"
    );
    let leftovers: (Option<i64>, Option<time::OffsetDateTime>) = sqlx::query_as(
        "SELECT pending_storage_limit_bytes, storage_limit_change_at FROM profiles WHERE user_id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .expect("columns");
    assert_eq!(leftovers, (None, None), "the window clears itself");

    cleanup(&pool, id).await;
}

/// A downgrade the account already fits in is not a downgrade to warn about:
/// it applies at once, with no window and nothing to count down to.
#[tokio::test]
async fn pro_to_free_within_the_limit_applies_immediately() {
    let Some(pool) = pool().await else {
        eprintln!("HOARD_PG_TEST_URL unset — skipping");
        return;
    };
    let id = seed_pro(&pool, 500 * 1024 * 1024).await; // 500 MB, fits in Free

    let outcome = quota::settle_storage_limit(&pool, id, Plan::Free, None, GRACE_DAYS)
        .await
        .expect("settle");
    assert_eq!(outcome, SettleOutcome::Applied { limit_bytes: 2 * GB });
    set_plan(&pool, id, "free").await;
    assert_eq!(enforced_limit(&pool, id).await, 2 * GB as u64);

    cleanup(&pool, id).await;
}

/// Coming back to Pro during the window cancels it outright — no lingering
/// deadline waiting to shrink a paying account.
#[tokio::test]
async fn resubscribing_cancels_a_pending_downgrade() {
    let Some(pool) = pool().await else {
        eprintln!("HOARD_PG_TEST_URL unset — skipping");
        return;
    };
    let id = seed_pro(&pool, 6_800_000_000).await;
    quota::settle_storage_limit(&pool, id, Plan::Free, None, GRACE_DAYS)
        .await
        .expect("settle down");
    set_plan(&pool, id, "free").await;

    // Polar says active again. Settle *then* flip, the order the webhook uses.
    let outcome = quota::settle_storage_limit(&pool, id, Plan::Pro, None, GRACE_DAYS)
        .await
        .expect("settle up");
    assert_eq!(
        outcome,
        SettleOutcome::Applied {
            limit_bytes: 100 * GB
        }
    );
    set_plan(&pool, id, "pro").await;
    assert_eq!(enforced_limit(&pool, id).await, 100 * GB as u64);
    let pending: (Option<i64>, Option<time::OffsetDateTime>) = sqlx::query_as(
        "SELECT pending_storage_limit_bytes, storage_limit_change_at FROM profiles WHERE user_id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .expect("columns");
    assert_eq!(pending, (None, None), "no downgrade left pending");

    cleanup(&pool, id).await;
}
