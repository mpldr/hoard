//! Cloud-pull poller — the live "is anything new?" loop.
//!
//! Pairs with `commands::automatic`. The automatic scheduler does the heavy
//! lifting (scan-library + backup-stale sweep) on the hourly scale; this
//! poller hits `/v1/cloud/sync` every [`CLOUD_POLL_INTERVAL_SECS`] as the
//! airbag for the Supabase Realtime push (`cloud_realtime`), which is the
//! primary near-instant trigger. Immediacy comes from `kick()`; the timed
//! tick only catches a missed push.
//!
//! Decoupling the two cadences was an ADR-0016 call. The manifest endpoint
//! returns <5 KB and is explicitly excluded from the bandwidth quota
//! (`hoard-server::cloud::routes::sync` — no `bandwidth::check` call). The
//! cadence itself stopped being a pref: it has no user-visible effect with
//! Realtime as the primary trigger, and the old knob let a hand-edited
//! `prefs.json` hammer the server.
//!
//! What this poller deliberately does **not** do: it never overwrites a
//! local save file. The "remote is newer, pull it" pathway still goes
//! through the agent's auto-restore sweep (triggered by the automatic
//! scheduler on its hourly tick, or by flipping the toggle off→on). The
//! poller's role is to make the UI honest about server state — not to
//! race the user's keyboard. Forcing pulls from a 10-second loop would
//! risk overwriting an active edit; ADR 0016 spells out the trade.
//!
//! Events emitted (Tauri):
//! - `agent://cloud-pull-started`  — fired right before the HTTP GET.
//! - `agent://cloud-pull-completed { count, new_versions, bytes }`
//!   `count` = total saves in manifest, `new_versions` = number where the
//!   remote version_num is strictly greater than the last manifest seen
//!   in-memory, `bytes` = sum of `latest_size_bytes` of the new ones
//!   (informational only — nothing was downloaded).
//! - `agent://quota-reached { reset_in_seconds, plan }` — emitted on a
//!   429 response from the manifest endpoint. Today the manifest is
//!   excluded from the limiter so this should be a no-op in practice;
//!   we keep the branch in case the policy changes.
//! - `agent://offline` — emitted on transport errors (DNS, TCP, TLS).
//!   The LiveStatus widget downgrades to the red "Server unreachable"
//!   dot until the next successful pull.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use hoard_agent::agent::AgentHandle;
use hoard_agent::prefs::CLOUD_POLL_INTERVAL_SECS;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tokio::task::JoinHandle;
use tokio::time::interval;

/// Lock a mutex, recovering from poisoning instead of panicking.
///
/// Every lock in this module guards *derived, rebuildable* state — the seen-map
/// reseeds from the next manifest, the gate reseeds from the next tick. A panic
/// while one was held used to poison it, and the next `.lock().unwrap()` killed
/// the poller task with no log line at all: one of the two candidate causes of
/// the silent death in ADR 0021 D.10. Taking the value anyway degrades a
/// poisoned lock into "stale but usable data", which for this data is exactly
/// right and, unlike dying, is visible.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| {
        tracing::warn!("cloud-pull: recovering from a poisoned lock");
        poisoned.into_inner()
    })
}

/// Managed singleton holding the currently-active poller task, if any.
/// Mirrors `AutomaticScheduler`. We mutate the inner `Option<JoinHandle>`
/// to swap tasks on prefs changes.
#[derive(Default)]
pub struct CloudPullScheduler {
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// Latest seen `(save_id → version_num)` map. Used to detect deltas
    /// between polls. Lives in the scheduler (not on disk) because a
    /// fresh session is correct: the first poll just emits "0 new" and
    /// subsequent polls show real deltas.
    seen: Arc<Mutex<Vec<ManifestSeenEntry>>>,
    /// Single-flight coalescing gate shared by the timed poller and every
    /// Realtime `kick()`. Without it, a catch-up backup sweep that touches
    /// N saves makes Supabase push N near-simultaneous `saves` UPDATEs, and
    /// each one used to spawn its own `/v1/cloud/sync` — N concurrent pulls
    /// that race on token refresh, so a single transient timeout among them
    /// emitted `agent://offline` and the LiveStatus dot flapped to "agente
    /// apagado". With the gate, at most one pull runs at a time and a burst
    /// of kicks collapses into a single follow-up pull.
    gate: Arc<Mutex<PullGate>>,
}

/// Coalescing state for [`CloudPullScheduler::gate`].
#[derive(Default)]
struct PullGate {
    /// A pull is currently executing.
    running: bool,
    /// A pull was requested while one was already running; run exactly one
    /// more pass when the current one finishes (collapses any number of
    /// concurrent kicks into a single re-run).
    rerun: bool,
    /// When the current holder took the gate. Only used to tell a normal
    /// coalesced kick from a holder that is *stuck* — see [`STUCK_GATE_SECS`].
    since: Option<Instant>,
}

/// How long the gate may stay held before a blocked kick escalates to a
/// warning. A pull is one ≤15 s HTTP request plus a version feed, so anything
/// past this is a hung task, not coalescing. The RAII guard makes the old
/// permanent wedge impossible; this catches whatever wedges next, out loud.
const STUCK_GATE_SECS: u64 = 5 * 60;

/// RAII release for [`CloudPullScheduler::gate`].
///
/// **This is the D.10 fix.** The gate used to be released by hand at the end of
/// `guarded_pull`, so a task that got aborted (`start()` aborts the previous
/// poller) or panicked mid-pull left `running = true` forever, and every later
/// tick returned through `if g.running { … return; }` — silently, which is why
/// a dead poller looked exactly like a healthy one. Dropping is the one thing
/// that happens on *every* exit path (return, panic-unwind, and abort, which
/// drops the future's locals), so the gate now cannot leak.
struct GateGuard {
    gate: Arc<Mutex<PullGate>>,
}

impl GateGuard {
    /// Take the gate, or `None` if another pull holds it (the caller's request
    /// is folded into that pull's re-run flag).
    fn acquire(gate: &Arc<Mutex<PullGate>>) -> Option<Self> {
        let mut g = lock(gate);
        if g.running {
            // Someone is already pulling; ask them to do one more pass with
            // the freshest server state and bail.
            g.rerun = true;
            let held = g.since.map(|t| t.elapsed()).unwrap_or_default();
            if held.as_secs() >= STUCK_GATE_SECS {
                tracing::warn!(
                    held_secs = held.as_secs(),
                    "cloud-pull: gate busy — the in-flight pull looks stuck; cloud versions are going stale"
                );
            } else {
                tracing::debug!(
                    held_secs = held.as_secs(),
                    "cloud-pull: gate busy — coalescing into the in-flight pull"
                );
            }
            return None;
        }
        g.running = true;
        g.since = Some(Instant::now());
        Some(Self { gate: gate.clone() })
    }

    /// Drain the re-run flag: `true` when kicks arrived mid-flight and one more
    /// pass is owed.
    fn take_rerun(&self) -> bool {
        let mut g = lock(&self.gate);
        std::mem::take(&mut g.rerun)
    }
}

impl Drop for GateGuard {
    fn drop(&mut self) {
        let mut g = lock(&self.gate);
        g.running = false;
        g.since = None;
    }
}

#[derive(Debug, Clone)]
struct ManifestSeenEntry {
    save_id: String,
    version_num: i64,
}

#[derive(Debug, Deserialize)]
struct ManifestEntry {
    save_id: String,
    #[allow(dead_code)]
    game_slug: String,
    #[allow(dead_code)]
    label: String,
    latest_version_num: i64,
    latest_size_bytes: i64,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    saves: Vec<ManifestEntry>,
}

#[derive(Debug, Serialize, Clone)]
struct CloudPullCompleted {
    /// Total saves in the manifest. The watcher_count for cloud.
    count: usize,
    /// How many of those have a `latest_version_num` strictly greater
    /// than what we saw last time. First poll always reports 0.
    new_versions: usize,
    /// Sum of `latest_size_bytes` across the newly-versioned saves —
    /// informational only, nothing was downloaded.
    bytes: i64,
}

#[derive(Debug, Serialize, Clone)]
struct QuotaReached {
    reset_in_seconds: u32,
    plan: String,
}

/// Cancel any in-flight poller and start a fresh one ticking every
/// [`CLOUD_POLL_INTERVAL_SECS`]. Safe to call repeatedly. The poller reads
/// the cloud session from disk on each tick (cheap) so a logout / login
/// round-trip without a restart picks up the new token transparently.
pub fn start(app: &AppHandle) {
    let scheduler = app.state::<CloudPullScheduler>();
    {
        let mut slot = lock(&scheduler.handle);
        if let Some(prev) = slot.take() {
            // The aborted task may have been mid-pull. Its `GateGuard` is
            // dropped with the future, so the gate reopens on its own — the
            // hand-rolled release this replaced did not, and that stuck gate
            // is what silenced the poller for a whole session (D.10).
            prev.abort();
        }
    }

    let secs = CLOUD_POLL_INTERVAL_SECS as u64;
    let period = Duration::from_secs(secs);
    let app_for_task = app.clone();
    let seen = scheduler.seen.clone();
    let gate = scheduler.gate.clone();
    let new_handle = tokio::task::spawn(async move {
        supervise(app_for_task, seen, gate, period).await;
    });

    let mut slot = lock(&scheduler.handle);
    *slot = Some(new_handle);
}

/// Backoff between poller restarts: a loop that dies on its first line must not
/// spin, and one that dies of something transient must come back fast.
const RESTART_BACKOFF_MIN_SECS: u64 = 5;
const RESTART_BACKOFF_MAX_SECS: u64 = 5 * 60;

/// A run this long counts as healthy, so the next death restarts from the
/// bottom of the backoff instead of inheriting an old escalation.
const HEALTHY_RUN_SECS: u64 = 10 * 60;

/// Keep [`poll_loop`] alive: catch its death, say so out loud, restart with
/// backoff (ADR 0021 D.12).
///
/// The loop is not supposed to be able to return — and it didn't; it *vanished*.
/// A `tokio::spawn` that panics is reaped by the runtime and its `JoinHandle`
/// silently holds the error nobody joins, so the poller stopped after two ticks
/// and nothing in the log said so: no `gate busy`, no second `started`, no
/// `stopped`. Chasing that one panic (a `state::<CloudFeed>()` on unmanaged
/// state, fixed at its source) would only defer the next one. A background task
/// that dies in silence is a bug by itself, so this supervisor treats **any**
/// exit — panic or return — as an incident: logged at error and retried.
///
/// `catch_unwind` rather than a nested task on purpose: `start()`/`stop()` abort
/// the handle they hold, and a nested `tokio::spawn` would outlive that abort as
/// an orphan — two pollers racing, which is precisely the shape of bug this
/// module keeps producing. One task means abort still kills everything.
async fn supervise(
    app: AppHandle,
    seen: Arc<Mutex<Vec<ManifestSeenEntry>>>,
    gate: Arc<Mutex<PullGate>>,
    period: Duration,
) {
    use futures::FutureExt;

    let mut backoff = Duration::from_secs(RESTART_BACKOFF_MIN_SECS);
    loop {
        let started = Instant::now();
        let outcome = std::panic::AssertUnwindSafe(poll_loop(&app, &seen, &gate, period))
            .catch_unwind()
            .await;
        // The gate is RAII (`GateGuard`), so it was released by the unwind; the
        // seen-map is derived state that the next pull replaces wholesale. There
        // is nothing to roll back before going again.
        match outcome {
            Ok(()) => tracing::error!(
                ran_secs = started.elapsed().as_secs(),
                "cloud-pull poller: loop returned unexpectedly — restarting"
            ),
            Err(payload) => tracing::error!(
                ran_secs = started.elapsed().as_secs(),
                panic = %panic_text(&payload),
                "cloud-pull poller: task panicked — restarting"
            ),
        }
        if started.elapsed() >= Duration::from_secs(HEALTHY_RUN_SECS) {
            backoff = Duration::from_secs(RESTART_BACKOFF_MIN_SECS);
        }
        tracing::info!(
            restart_in_secs = backoff.as_secs(),
            "cloud-pull poller: restarting after backoff"
        );
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(RESTART_BACKOFF_MAX_SECS));
    }
}

/// Best-effort text of a caught panic payload, for the supervisor's log line.
fn panic_text(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// The poller proper: pull the manifest every `period`, and keep the devices +
/// bell feeds eventually honest. Never returns; [`supervise`] owns its lifetime.
async fn poll_loop(
    app: &AppHandle,
    seen: &Arc<Mutex<Vec<ManifestSeenEntry>>>,
    gate: &Arc<Mutex<PullGate>>,
    period: Duration,
) {
    tracing::info!(
        interval_secs = period.as_secs(),
        "cloud-pull poller: started"
    );

    // First tick fires immediately so the user sees activity on
    // sign-in without waiting a full interval. The built-in
    // zero-delay first tick of `tokio::time::interval` is the right
    // shape — we don't manually emit before the loop.
    let mut ticker = interval(period);
    // Fallback refresh for the Eye-panel devices + bell feeds when the
    // Realtime socket is down. Immediacy comes from Realtime; this only
    // needs to keep them *eventually* honest, so it's throttled to at
    // most once per `FALLBACK_MIN_SECS`.
    // Initialized in the past so the first tick primes both feeds.
    let mut last_feed = tokio::time::Instant::now()
        - Duration::from_secs(crate::commands::cloud_feed::FALLBACK_MIN_SECS);
    loop {
        ticker.tick().await;
        guarded_pull(app, seen, gate, "timer").await;
        if last_feed.elapsed().as_secs() >= crate::commands::cloud_feed::FALLBACK_MIN_SECS {
            last_feed = tokio::time::Instant::now();
            crate::commands::cloud_feed::kick_all(app);
        }
    }
}

/// Fire a single manifest pull immediately, off the regular cadence.
///
/// Used by the Realtime push (`cloud_realtime`): when another device commits
/// a new save version, Supabase pushes a `saves` UPDATE and we refresh state
/// in ~1 s instead of waiting for the next timed tick. Reuses the
/// scheduler's `seen` map so delta detection stays consistent with the timed
/// poll. No-op when signed out (`run_one_pull` bails on missing creds).
pub fn kick(app: &AppHandle) {
    let scheduler = app.state::<CloudPullScheduler>();
    let seen = scheduler.seen.clone();
    let gate = scheduler.gate.clone();
    let app = app.clone();
    tokio::task::spawn(async move {
        guarded_pull(&app, &seen, &gate, "kick").await;
    });
}

/// Run a pull behind the single-flight [`PullGate`]. If a pull is already in
/// flight this only flags a re-run and returns immediately; the in-flight
/// caller drains that flag with exactly one extra pass when it finishes. A
/// burst of kicks (e.g. a backup sweep touching every save) therefore costs
/// at most two `/v1/cloud/sync` requests instead of one per save.
/// The gate is released by [`GateGuard`]'s `Drop`, so an abort or a panic can
/// no longer wedge the poller shut (ADR 0021 D.10).
async fn guarded_pull(
    app: &AppHandle,
    seen: &Arc<Mutex<Vec<ManifestSeenEntry>>>,
    gate: &Arc<Mutex<PullGate>>,
    trigger: &'static str,
) {
    let Some(guard) = GateGuard::acquire(gate) else {
        return;
    };
    // One line per pull attempt, unconditionally. The whole reason D.10 took a
    // day to find is that a poller that stops pulling and a poller that pulls
    // fine looked identical in the log; from here on, silence in this stream
    // *is* the symptom.
    tracing::debug!(trigger, "cloud-pull: tick");
    loop {
        run_one_pull(app, seen).await;
        // Honour the kick(s) that arrived mid-flight, then let `guard` drop.
        if !guard.take_rerun() {
            return;
        }
        tracing::debug!(trigger, "cloud-pull: re-running for kicks that arrived mid-pull");
    }
}

/// Abort the running poller. No-op when nothing is scheduled.
pub fn stop(app: &AppHandle) {
    let scheduler = app.state::<CloudPullScheduler>();
    let mut slot = lock(&scheduler.handle);
    if let Some(prev) = slot.take() {
        prev.abort();
        tracing::info!("cloud-pull poller: stopped");
    }
    // Clear the seen-map too: a fresh login starts from 0 known versions.
    lock(&scheduler.seen).clear();
}

/// Boot-time rehydration: if a cloud session is present on disk, start
/// the poller. Called from `lib.rs::setup`.
pub async fn restart_if_enabled(app: &AppHandle) -> anyhow::Result<()> {
    let signed_in = {
        let st = app.state::<crate::state::AppState>();
        let has = lock(&st.cloud_account).is_some();
        has
    };
    if !signed_in {
        return Ok(());
    }
    start(app);
    Ok(())
}

/// The running agent's handle, or `None` when it hasn't been spawned yet.
fn agent_handle(app: &AppHandle) -> Option<AgentHandle> {
    app.try_state::<crate::state::AppState>()
        .and_then(|s| lock(&s.agent).clone())
}

/// Push the freshest cloud head map the poller knows into the running agent.
///
/// The reconciliation sweep version-gates locally off this cache instead of
/// each candidate re-fetching the same manifest — collapsing the old "poller
/// GET + N sweep GETs" per interval down to the single GET the pull already
/// did. Sent every tick (not just on deltas) so the cache never goes stale and
/// a save we already pulled stays correctly gated; since ADR 0021 D.10 the
/// agent also timestamps each feed, so a poller that stops feeding degrades to
/// a visible `Hold{"cloud state stale"}` instead of a silent `converged`.
///
/// Called from two places on purpose: after every pull, and from `start_agent`
/// once the handle exists. Without the second call a pull that lands before the
/// agent is spawned — the normal cold-start order, since the poller's first
/// tick fires immediately — dropped its whole manifest on the floor without a
/// word, and the agent then ran on an empty version cache until the next tick.
pub async fn feed_agent_versions(app: &AppHandle) {
    let Some(scheduler) = app.try_state::<CloudPullScheduler>() else {
        return;
    };
    let versions: HashMap<String, i64> = lock(&scheduler.seen)
        .iter()
        .map(|e| (e.save_id.clone(), e.version_num))
        .collect();
    if versions.is_empty() {
        // Nothing pulled yet (or signed out): the next pull feeds the agent.
        return;
    }
    let Some(h) = agent_handle(app) else {
        tracing::debug!(
            count = versions.len(),
            "cloud-pull: no agent handle yet — version feed deferred to agent start"
        );
        return;
    };
    let count = versions.len();
    match h.set_cloud_versions(versions).await {
        Ok(()) => tracing::debug!(count, "cloud-pull: fed cloud version cache to agent"),
        Err(e) => tracing::warn!(error = %e, "cloud-pull: couldn't feed version cache to agent"),
    }
}

async fn run_one_pull(app: &AppHandle, seen: &Arc<Mutex<Vec<ManifestSeenEntry>>>) {
    let _ = app.emit("agent://cloud-pull-started", ());

    let creds = match crate::commands::cloud::load_active_creds() {
        Ok(Some(c)) => c,
        Ok(None) => {
            // Session disappeared between polls (user logged out). The logout
            // path called `stop()` separately, so this is expected — but it
            // still ends a tick without pulling, and every silent way to end a
            // tick is a place D.10 could hide again.
            tracing::debug!("cloud-pull: no cloud session on disk — nothing to pull");
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "cloud-pull: couldn't read session");
            let _ = app.emit("agent://offline", ());
            return;
        }
    };

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(concat!("hoard-desktop/", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "cloud-pull: HTTP client build failed");
            return;
        }
    };

    let url = format!("{}/v1/cloud/sync", creds.server_url);
    let mut access_token = creds.access_token.clone();
    let mut resp = match client.get(&url).bearer_auth(&access_token).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "cloud-pull: network error");
            let _ = app.emit("agent://offline", ());
            return;
        }
    };

    // The access token is a short-lived Supabase JWT. When it expires the
    // sync endpoint answers 401 — which previously surfaced as a permanent
    // "server down" dot. Renew it with the refresh token and retry once so the
    // poller keeps working across the token's lifetime (and across restarts).
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        match crate::commands::cloud::refresh_active_session().await {
            Ok(fresh) => {
                access_token = fresh.access_token;
                match client.get(&url).bearer_auth(&access_token).send().await {
                    Ok(r) => resp = r,
                    Err(e) => {
                        tracing::debug!(error = %e, "cloud-pull: network error after refresh");
                        let _ = app.emit("agent://offline", ());
                        return;
                    }
                }
            }
            Err(e) => {
                // Distinguish a transient refresh failure (network, GoTrue
                // hiccup → keep polling, dot flaps offline and recovers) from a
                // terminal one: Supabase revoked the whole refresh-token family,
                // so every future refresh 401s and the old code looped on
                // "Servidor no disponible" forever. On terminal expiry, sign out
                // cleanly and tell the UI to prompt re-login instead of spinning.
                if crate::commands::cloud::is_session_expired(&e) {
                    tracing::warn!(
                        "cloud-pull: refresh token revoked — signing out (session expired)"
                    );
                    crate::commands::cloud::handle_session_expired(app);
                    return;
                }
                tracing::warn!(error = %e, "cloud-pull: token refresh failed");
                let _ = app.emit("agent://offline", ());
                return;
            }
        }
    }

    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(60);
        let plan = creds.plan.clone().unwrap_or_else(|| "free".to_string());
        let _ = app.emit(
            "agent://quota-reached",
            QuotaReached {
                reset_in_seconds: retry_after,
                plan,
            },
        );
        return;
    }
    if !status.is_success() {
        tracing::warn!(status = %status, "cloud-pull: non-2xx response");
        let _ = app.emit("agent://offline", ());
        return;
    }

    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "cloud-pull: couldn't read response body");
            return;
        }
    };
    let manifest: Manifest = match serde_json::from_str(&body) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "cloud-pull: couldn't parse manifest");
            return;
        }
    };

    let (new_versions, new_bytes, advanced_ids) = {
        let mut seen_guard = lock(seen);
        let mut new_versions: usize = 0;
        let mut new_bytes: i64 = 0;
        let mut advanced_ids: Vec<String> = Vec::new();
        for entry in &manifest.saves {
            let prev_version = seen_guard
                .iter()
                .find(|s| s.save_id == entry.save_id)
                .map(|s| s.version_num);
            match prev_version {
                Some(prev) if entry.latest_version_num > prev => {
                    new_versions += 1;
                    new_bytes += entry.latest_size_bytes.max(0);
                    advanced_ids.push(entry.save_id.clone());
                }
                None => {
                    // First time we see this save in this session. We
                    // intentionally do *not* count it as "new" — the
                    // user might have logged in to a populated account
                    // and reporting "247 new versions!" right after
                    // sign-in is noisy. The seed pass just records the
                    // baseline; deltas from the next poll onward are
                    // what surface to LiveStatus.
                }
                _ => {}
            }
        }
        // Replace the seen-map wholesale: saves removed server-side
        // (other-device deletes) shouldn't linger.
        seen_guard.clear();
        for entry in &manifest.saves {
            seen_guard.push(ManifestSeenEntry {
                save_id: entry.save_id.clone(),
                version_num: entry.latest_version_num,
            });
        }
        (new_versions, new_bytes, advanced_ids)
    };

    let _ = app.emit(
        "agent://cloud-pull-completed",
        CloudPullCompleted {
            count: manifest.saves.len(),
            new_versions,
            bytes: new_bytes,
        },
    );

    // Keep the agent's long-lived client token in lock-step with the JWT we
    // just proved works (this request succeeded). The targeted
    // refresh→`update_agent_token` push can miss — the agent client may be
    // registered after a refresh already ran, or `refresh_active_session` can
    // return via its reuse-window / healed-disk short-circuits without touching
    // the agent — and the agent never re-reads creds from disk on its own. When
    // that happens it 401s on *every* auto-restore/backup/CAS until the app is
    // restarted, even though the poller (which reloads disk each tick) keeps
    // working. This per-tick resync caps that drift at one interval. Cheap: it
    // just swaps a shared `RwLock<String>`.
    crate::commands::agent::update_agent_token(&access_token);

    // Feed the agent the full latest-version map this manifest just gave us
    // (reads it back out of the seen-map we just replaced, so the deferred
    // re-feed below shares one code path with the live one).
    feed_agent_versions(app).await;

    // Grab the agent handle to force-restore advanced saves (sync global).
    let handle = agent_handle(app);

    // Sync global: the poller is the cheap "is this device outdated?" detector.
    // When it's on and a save advanced server-side, ask the agent to pull it
    // right now — "en el momento", even if the game is running. This is the
    // low-latency complement to the agent's own sweep (which would catch the
    // same delta within its cooldown). Off by default and version-gated on the
    // agent side, so this is a no-op cost when nothing actually changed.
    if !advanced_ids.is_empty() {
        let global_sync = hoard_agent::prefs::Prefs::load_default()
            .map(|(p, _)| p.global_sync)
            .unwrap_or(false);
        if global_sync {
            if let Some(h) = &handle {
                for id in advanced_ids {
                    if let Err(e) = h.force_restore(id).await {
                        tracing::warn!(
                            error = %e,
                            "cloud-pull: couldn't request force-restore for sync global"
                        );
                    }
                }
            }
        }
    }
}
