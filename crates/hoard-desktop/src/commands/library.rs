//! Game-library commands: detection, tracking, and listing tracked saves.
//!
//! `scan_library` runs the auto-detection sweep (filesystem heuristic +
//! Steam library scan) against the catalog. Long scans emit
//! `library://scan-progress` events so the UI can show a progress bar.
//!
//! `add_game_to_tracking` records that the user wants Hoard to back up a
//! given game/path pair. It creates a Save row on the server and stores the
//! local path in the agent's on-disk state file.
//!
//! `list_tracked_saves` returns the saves the current user has registered,
//! enriched with the local path from state so the UI doesn't need a second
//! round-trip.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use hoard_agent::api::{ApiClient, ApiError};
use hoard_agent::correlation::CorrelationStore;
use hoard_agent::credentials;
use hoard_agent::detection::{
    self, DetectedGame, DetectionReport, DetectionSource, DetectionTrace,
};
use hoard_agent::library;
use hoard_agent::manifest::Os;
use hoard_agent::state::CliState;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use time::OffsetDateTime;

use super::agent::{attach_save_if_running, detach_save_if_running};
use super::auth::pretty_error;
use super::error::AppError;
use crate::state::AppState;

// Persisted detection snapshot + its disk I/O live in `hoard_agent::library`
// (the CLI reuses the same cache). Re-exported so the rest of this module and
// `AppState` keep referring to them under `commands::library::…`.
pub use hoard_agent::library::{
    load_detection_from_disk, save_detection_to_disk_atomic, CachedDetection,
};

/// In-memory cache of the most recent scan, so the Library page can
/// re-render instantly when the user navigates back without forcing
/// another sweep. Wrapped in a `Mutex<Option<…>>` and stored on
/// `AppState`. Hydrated at boot from `detection.json` next to `state.json`;
/// `scan_library` re-writes both memory and disk atomically.
#[derive(Default)]
pub struct DetectionCache {
    pub last: Mutex<Option<CachedDetection>>,
}

/// Maximum age before the background scheduler triggers an automatic
/// rescan. 24 hours mirrors the catalog-refresh cadence so the two stay
/// loosely in step.
const STALE_AFTER_SECS: i64 = 60 * 60 * 24;
/// How often the background scheduler wakes to check the cache age.
/// 30 minutes is a good middle ground: long enough to be near-free,
/// short enough that the user doesn't have to wait a full sleep period
/// after the OS clock jumps forward.
const POLL_INTERVAL_SECS: u64 = 60 * 30;

/// Wire shape sent to the frontend per progress tick.
#[derive(Debug, Clone, Serialize)]
pub struct ScanProgress {
    pub done: usize,
    pub total: usize,
}

// Wire types for tracking live in `hoard_agent::library` so the CLI shares
// them. Re-exported here so the Tauri commands and `automatic.rs` keep the
// `commands::library::…` paths.
pub use hoard_agent::library::{AddGameArgs, AdoptArgs, TrackedSave};

/// Run a full auto-detection sweep against the **bundled** catalog (no
/// server round-trips). Emits `library://scan-progress` events
/// (`{ done, total }`) as it churns through the catalog. The completed
/// `DetectionReport` is also stored on the app state so re-renders are free.
#[tauri::command]
pub async fn scan_library(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<DetectionReport, String> {
    let os = Os::current();

    let app_for_progress = app.clone();
    let progress = move |done: usize, total: usize| {
        // Best-effort: a missing window means the user closed it mid-scan,
        // and we just stop reporting. Any other emit error is noise.
        let _ = app_for_progress.emit("library://scan-progress", ScanProgress { done, total });
    };

    // Load CliState so detect_all sees the user's manual_paths overrides.
    // Failures here mean state.json is unreadable; we surface that instead
    // of silently scanning without overrides, otherwise the user sees their
    // manual picks vanish from the Library card on every re-scan.
    let (cli_state, _) = CliState::load_default().map_err(|e| e.to_string())?;

    let mut report = detection::detect_all(os, &cli_state, progress)
        .await
        .map_err(pretty_error)?;

    // Drop user-blacklisted slugs *after* detection finishes so the walker
    // still benefits from their install dirs when cross-referencing other
    // games on the same volume. The filter is purely a UI-edge concern.
    report.games.retain(|g| !cli_state.is_ignored(&g.slug));

    persist_scan(&state, report.clone());
    Ok(report)
}

/// Forced re-scan that ignores the in-memory cache. Functionally identical
/// to `scan_library` today (both always do a fresh sweep), but kept as a
/// distinct entry point so the UI's "Re-escanear" button maps to an
/// unambiguous intent — and so future caching layers don't accidentally
/// short-circuit the user's manual request.
#[tauri::command]
pub async fn rescan_library(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<DetectionReport, String> {
    scan_library(app, state).await
}

/// Deep, user-triggered detection sweep — the Library "deep scan" tile. Runs
/// [`detection::detect_all_deep`], which on top of the normal pipeline looks
/// at the expensive places the periodic scan skips: arbitrary Wine prefixes
/// (Heroic/CrossOver/Flatpak/mounted media), Flatpak/Snap/EmuDeck save roots,
/// deeper directory walks and a relaxed precision gate. Slow by design, so
/// it's never on the automatic tick. Emits the same `library://scan-progress`
/// events and persists the merged report like a normal scan.
#[tauri::command]
pub async fn deep_scan_library(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<DetectionReport, String> {
    let os = Os::current();

    let app_for_progress = app.clone();
    let progress = move |done: usize, total: usize| {
        let _ = app_for_progress.emit("library://scan-progress", ScanProgress { done, total });
    };

    let (cli_state, _) = CliState::load_default().map_err(|e| e.to_string())?;

    let mut report = detection::detect_all_deep(os, &cli_state, progress)
        .await
        .map_err(pretty_error)?;

    report.games.retain(|g| !cli_state.is_ignored(&g.slug));

    persist_scan(&state, report.clone());
    Ok(report)
}

/// Ludusavi-style "add from folder": scan ONE user-chosen folder and return
/// the games detected inside it, for the folder-picker button next to "Manual
/// track". Unlike `scan_library` this never touches the catalog or Steam and
/// never persists into the library cache — it's a one-off lookup whose results
/// the UI shows as "found <Game> here — track it?".
///
/// Runs on the blocking pool: the walk is synchronous filesystem I/O bounded by
/// the deep per-root timeout, so keeping it off the async runtime avoids
/// stalling the UI event loop on a slow/large folder.
#[tauri::command]
pub async fn scan_folder(path: String) -> Result<Vec<DetectedGame>, String> {
    let root = PathBuf::from(&path);
    if !root.is_dir() {
        return Err(format!("{} isn't a folder.", root.display()));
    }

    // Skip folders already covered by a tracked save so the list only shows
    // new candidates, not games the user already added.
    let (cli_state, _) = CliState::load_default().map_err(|e| e.to_string())?;
    let known: HashSet<PathBuf> = cli_state
        .saves
        .values()
        .map(|s| s.local_path.clone())
        .collect();

    tokio::task::spawn_blocking(move || {
        // Correlation store, best-effort (empty if absent) — same as detect_all,
        // so a folder the agent has seen a game write to grades higher.
        let store = CorrelationStore::default_path()
            .ok()
            .map(|p| CorrelationStore::load(&p))
            .unwrap_or_default();
        detection::discover_in_folder(&root, &store, &known)
            .into_iter()
            .map(|a| DetectedGame {
                slug: a.slug,
                display_name: a.display_name,
                found_paths: vec![a.path],
                path_confidences: vec![a.confidence],
                confidence: a.confidence,
                source: DetectionSource::FilesystemHeuristic,
                steam_app_id: None,
                install_dir: None,
            })
            .collect()
    })
    .await
    .map_err(|e| e.to_string())
}

/// Return the previous scan if one is in memory. Used by the Library page
/// to render quickly on navigation; if `None`, the UI triggers `scan_library`.
#[tauri::command]
pub fn cached_detection(state: State<'_, AppState>) -> Option<DetectionReport> {
    state
        .detection_cache
        .last
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.report.clone())
}

/// What local detection already knows about one slug, so "Vincular a esta
/// máquina" can offer the detected folders as one-click options instead of
/// sending the user hunting through a folder picker.
///
/// Reads the in-memory cache (fresher than disk: `scan_library` writes it
/// first). A `scanned_at: None` result means nobody ever scanned here — the UI
/// offers a scan rather than claiming there's nothing.
#[tauri::command]
pub fn detected_paths_for_game(
    game_slug: String,
    state: State<'_, AppState>,
) -> library::LocalDetection {
    let guard = state.detection_cache.last.lock().unwrap();
    library::local_detection(guard.as_ref(), &game_slug)
}

/// Update both the in-memory cache and the on-disk copy. Disk failures are
/// logged at WARN — the user still has a working session, we just lose the
/// cold-start optimisation on next launch.
fn persist_scan(state: &State<'_, AppState>, report: DetectionReport) {
    let cached = CachedDetection {
        report,
        scanned_at: OffsetDateTime::now_utc(),
    };
    *state.detection_cache.last.lock().unwrap() = Some(cached.clone());
    if let Err(e) = save_detection_to_disk_atomic(&cached) {
        tracing::warn!(error = %e, "couldn't persist detection cache");
    }
}

/// Begin tracking a detected game. Creates a Save on the server and writes
/// the local path into the agent's state file. Returns the resulting
/// `TrackedSave` so the UI can append it to the list without a re-fetch. All
/// the business logic (cloud vs self-hosted, dedup, 409 re-link) lives in
/// `hoard_agent::library`; this wrapper just talks to the live agent and
/// prettifies errors.
#[tauri::command]
pub async fn add_game_to_tracking(
    args: AddGameArgs,
    state: State<'_, AppState>,
) -> Result<TrackedSave, String> {
    let client = current_client(&state)?;
    let outcome = library::add_to_tracking(&client, args)
        .await
        .map_err(pretty_error)?;
    // Attach to the running agent so it starts watching immediately.
    attach_save_if_running(&state, outcome.watched).await;
    Ok(outcome.tracked)
}

/// Adopt (vincular) a cloud save that belongs to another machine: associate a
/// local folder on THIS machine with the existing `save_id` instead of minting
/// a new save. In sync mode the agent auto-restores the latest snapshot on add
/// (the version-gate is left open); in backup-only mode it just watches the
/// folder. Core of cross-device sync. Logic in `hoard_agent::library::adopt`.
#[tauri::command]
pub async fn adopt_save(
    args: AdoptArgs,
    state: State<'_, AppState>,
) -> Result<TrackedSave, String> {
    let client = current_client(&state)?;
    let outcome = library::adopt(&client, args).await.map_err(pretty_error)?;
    attach_save_if_running(&state, outcome.watched).await;
    Ok(outcome.tracked)
}

/// List the saves Hoard is tracking for the logged-in user. Server-side data is
/// the source of truth for `latest_version_num`; the local path comes from
/// `CliState`. Logic (dedup self-heal, orphan detection, local sizes) lives in
/// `hoard_agent::library::list_tracked`; this wrapper detaches any duplicate
/// rows the self-heal pruned from the live agent.
#[tauri::command]
pub async fn list_tracked_saves(state: State<'_, AppState>) -> Result<Vec<TrackedSave>, String> {
    let client = current_client(&state)?;
    let (out, detached) = library::list_tracked(&client).await.map_err(pretty_error)?;
    for id in detached {
        detach_save_if_running(&state, id).await;
    }
    Ok(out)
}

/// Rename a save's label both on the server and in local state. On a 409
/// (another save in the same game already uses that label) we surface a
/// distinguishable error string so the UI can show the localized "label
/// already exists" message instead of a generic toast.
#[tauri::command]
pub async fn rename_save_label(
    save_id: String,
    new_label: String,
    state: State<'_, AppState>,
) -> Result<TrackedSave, String> {
    let client = current_client(&state)?;
    let (tracked, watched) = library::rename_label(&client, &save_id, &new_label)
        .await
        .map_err(|e| {
            // A 409 (another save in the same game already uses that label)
            // gets tagged so the JS side can match it without parsing the
            // server's English error text.
            let is_conflict = e
                .downcast_ref::<ApiError>()
                .map(|api| matches!(api, ApiError::Conflict(_)))
                .unwrap_or(false);
            if is_conflict {
                "conflict:label_collision".to_string()
            } else {
                pretty_error(e)
            }
        })?;

    // Re-attach to the running agent so its in-memory WatchedSave picks up the
    // new label (the watcher uses label as part of the upload key).
    if let Some(watched) = watched {
        attach_save_if_running(&state, watched).await;
    }
    Ok(tracked)
}

/// Validate a user-picked override path: non-empty, exists, is a directory.
/// Pulled out of the Tauri command so the failure messages can be unit-tested
/// without spinning up Tauri's `State` machinery.
fn validate_override_path(path: &str) -> Result<PathBuf, String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("Path can't be empty.".to_string());
    }
    let buf = PathBuf::from(trimmed);
    if !buf.exists() {
        return Err(format!("{} doesn't exist.", buf.display()));
    }
    if !buf.is_dir() {
        return Err(format!("{} isn't a folder.", buf.display()));
    }
    Ok(buf)
}

/// Record a manual save-folder override for `slug`. The path must already
/// exist and be a directory — we validate up-front so an obvious typo in
/// the picker doesn't get persisted and silently fail the next re-scan.
///
/// After the override lands in `state.json`, we kick a background re-scan
/// so the Library card flips to source=manual without the user clicking
/// "Rescan". Disk failures inside the background scan are logged; the
/// command itself returns as soon as the override is durable.
#[tauri::command]
pub async fn set_manual_path(
    app: AppHandle,
    state: State<'_, AppState>,
    slug: String,
    path: String,
) -> Result<(), String> {
    let path_buf = validate_override_path(&path)?;

    let (mut cli_state, state_path) = CliState::load_default().map_err(|e| e.to_string())?;
    cli_state.set_manual_path(&slug, path_buf);
    cli_state.save(&state_path).map_err(|e| e.to_string())?;

    // Refresh the detection cache so subsequent renders see source=manual
    // without forcing the user to click "Rescan". A short-circuit failure
    // here is harmless — the next scheduled rescan will pick up the
    // override either way.
    let app_for_progress = app.clone();
    let progress = move |done: usize, total: usize| {
        let _ = app_for_progress.emit("library://scan-progress", ScanProgress { done, total });
    };
    match detection::detect_all(Os::current(), &cli_state, progress).await {
        Ok(mut report) => {
            report.games.retain(|g| !cli_state.is_ignored(&g.slug));
            persist_scan(&state, report);
        }
        Err(e) => tracing::warn!(error = %e, "post-override detection refresh failed"),
    }
    Ok(())
}

/// Drop the manual override for `slug` so the next re-scan goes back to
/// whatever the heuristics produce. Mirrors `set_manual_path` and also
/// refreshes the detection cache in-line so the UI re-paints without an
/// explicit rescan click.
#[tauri::command]
pub async fn clear_manual_path(
    app: AppHandle,
    state: State<'_, AppState>,
    slug: String,
) -> Result<(), String> {
    let (mut cli_state, state_path) = CliState::load_default().map_err(|e| e.to_string())?;
    cli_state.clear_manual_path(&slug);
    cli_state.save(&state_path).map_err(|e| e.to_string())?;

    let app_for_progress = app.clone();
    let progress = move |done: usize, total: usize| {
        let _ = app_for_progress.emit("library://scan-progress", ScanProgress { done, total });
    };
    match detection::detect_all(Os::current(), &cli_state, progress).await {
        Ok(mut report) => {
            report.games.retain(|g| !cli_state.is_ignored(&g.slug));
            persist_scan(&state, report);
        }
        Err(e) => tracing::warn!(error = %e, "post-clear detection refresh failed"),
    }
    Ok(())
}

/// Persistently blacklist a detected game from the Library page. Subsequent
/// scans run the full detection pipeline but the matching slug is filtered
/// out before the report reaches the UI. Reversible via
/// [`unignore_detected_game`]. Idempotent.
#[tauri::command]
pub async fn ignore_detected_game(slug: String) -> Result<(), AppError> {
    let trimmed = slug.trim();
    if trimmed.is_empty() {
        return Err(AppError::plain("Slug is empty."));
    }
    let (mut cli_state, path) =
        CliState::load_default().map_err(|e| AppError::plain(e.to_string()))?;
    cli_state.add_ignored_slug(trimmed.to_string());
    cli_state
        .save(&path)
        .map_err(|e| AppError::plain(e.to_string()))?;
    Ok(())
}

/// Drop the blacklist entry for `slug` so the next scan re-surfaces it in
/// the Library. Mirror of [`ignore_detected_game`]. Idempotent.
#[tauri::command]
pub async fn unignore_detected_game(slug: String) -> Result<(), AppError> {
    let trimmed = slug.trim();
    if trimmed.is_empty() {
        return Err(AppError::plain("Slug is empty."));
    }
    let (mut cli_state, path) =
        CliState::load_default().map_err(|e| AppError::plain(e.to_string()))?;
    cli_state.remove_ignored_slug(trimmed);
    cli_state
        .save(&path)
        .map_err(|e| AppError::plain(e.to_string()))?;
    Ok(())
}

/// Return every slug the user has blacklisted. Sorted alphabetically so the
/// Settings page renders a stable order across refreshes.
#[tauri::command]
pub async fn list_ignored_slugs() -> Result<Vec<String>, AppError> {
    let (cli_state, _) = CliState::load_default().map_err(|e| AppError::plain(e.to_string()))?;
    let mut slugs: Vec<String> = cli_state.ignored_slugs.iter().cloned().collect();
    slugs.sort();
    Ok(slugs)
}

/// Replay the detection pipeline for a single slug and return a trace
/// explaining what every step kept / dropped. Read-only — does not write
/// to the detection cache or `state.json`. Backs the hidden
/// `/diagnostics` route unlocked by the 5-click sidebar gesture.
#[tauri::command]
pub async fn detection_diagnostics(slug: String) -> Result<DetectionTrace, String> {
    if slug.trim().is_empty() {
        return Err("Slug is empty.".into());
    }
    let (cli_state, _path) = CliState::load_default().map_err(|e| e.to_string())?;
    Ok(detection::diagnose(slug.trim(), Os::current(), &cli_state).await)
}

/// Stop tracking a save. Removes the local-state row but leaves server data
/// intact (delete from the History view if you want that gone too).
#[tauri::command]
pub async fn untrack_save(save_id: String, state: State<'_, AppState>) -> Result<(), String> {
    library::untrack(&save_id).map_err(|e| e.to_string())?;
    detach_save_if_running(&state, save_id).await;
    Ok(())
}

/// Hard-delete a save: drop the row + every snapshot on the server **and**
/// purge any local CliState that referenced it. Sibling of `untrack_save`,
/// but destructive on purpose — it's what the user clicks when a save was
/// tracked against a wrong path and the only way out is to start over
/// (otherwise `add_game_to_tracking` swallows the next 409 and re-links to
/// the bad row). The matching `manual_paths` override is also cleared so a
/// re-add doesn't immediately bounce back to the same wrong folder.
#[tauri::command]
pub async fn delete_save_completely(
    save_id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let client = current_client(&state)?;
    library::delete_completely(&client, &save_id)
        .await
        .map_err(pretty_error)?;
    detach_save_if_running(&state, save_id).await;
    Ok(())
}

// ---- helpers ----------------------------------------------------------

/// Build an `ApiClient` from the credentials on disk. We don't reuse a
/// long-lived client on `AppState` because the user can log out at any time
/// and we want fresh creds per command — the cost is negligible (`reqwest`
/// connections are pooled internally).
pub(crate) fn current_client(state: &State<'_, AppState>) -> Result<ApiClient, String> {
    // Prefer the self-hosted session: the cached UserInfo gives us the URL and
    // the keychain holds the bearer token.
    let self_hosted = state.user.lock().unwrap().clone();
    if let Some(user) = self_hosted {
        let creds = credentials::load()
            .map_err(|e| format!("Couldn't load credentials: {e}"))?
            .ok_or_else(|| "Saved credentials are missing. Sign in again.".to_string())?;
        return ApiClient::new(user.server_url, creds.token).map_err(|e| e.to_string());
    }
    // Fall back to a Hoard Cloud session (Gmail login). The cloud API exposes
    // the same `/v1/...` surface and accepts the Supabase JWT as a bearer
    // token, so the agent and every library/history command can talk to it
    // exactly like a self-hosted server. Without this branch a cloud-only user
    // hit "Not logged in" on every monitor/backup action even though the
    // sidebar showed "Nube conectada".
    //
    // Note: the JWT is short-lived (~1h). The cloud-pull poller refreshes the
    // on-disk token on a 401, so per-command clients built here stay fresh;
    // the long-lived agent client created at `start_agent` is the one case that
    // can outlive a token and is handled separately.
    if let Some(cloud) = crate::commands::cloud::load_active_creds()
        .map_err(|e| format!("Couldn't load cloud credentials: {e}"))?
    {
        return ApiClient::new(cloud.server_url, cloud.access_token).map_err(|e| e.to_string());
    }
    Err("Not logged in. Sign in again to continue.".to_string())
}

/// Recompute and install the active sync context from the current session,
/// mirroring [`current_client`]'s self-hosted-wins-else-cloud selection. Call
/// after any login/logout (and once at boot) so `CliState` reads and writes the
/// per-context `saves` file that belongs to the session actually in use.
/// Returns the installed context id, or `None` when fully signed out.
pub(crate) fn sync_active_context(state: &AppState) -> Option<String> {
    let ctx = if let Some(user) = state.user.lock().unwrap().clone() {
        Some(hoard_agent::state::selfhosted_context(&user.server_url))
    } else if let Ok(Some(cloud)) = crate::commands::cloud::load_active_creds() {
        cloud
            .user_id
            .as_deref()
            .map(hoard_agent::state::cloud_context)
    } else {
        None
    };
    hoard_agent::state::set_active_context(ctx.clone());
    ctx
}

/// Spawn a long-lived background task that re-scans the catalog whenever the
/// persisted cache turns 24 hours old. Wakes every 30 minutes; cheap on a
/// schedule clock that's "behind" because we just check timestamps. Errors
/// are logged and swallowed — a transient detection failure must not crash
/// the app loop.
pub fn spawn_periodic_rescan(app: AppHandle) {
    // `tauri::async_runtime::spawn`, not `tokio::spawn`: this is called from
    // `setup()`, which runs before Tauri enters its event loop, so there is
    // no ambient Tokio runtime yet. Using `tokio::spawn` here panics with
    // "there is no reactor running" the instant the app starts — that's how
    // 1.4.0 shipped, which is why the binary refused to launch on every
    // platform after the upgrade until the user reopened it from a terminal
    // and saw the stack trace.
    tauri::async_runtime::spawn(async move {
        use std::time::Duration;
        loop {
            tokio::time::sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
            let needs_rescan = {
                let state = app.state::<AppState>();
                let guard = state.detection_cache.last.lock().unwrap();
                match guard.as_ref() {
                    Some(c) => {
                        // Use unix timestamps so we don't depend on time
                        // crate's Duration arithmetic — it's just a subtraction.
                        let age_secs = OffsetDateTime::now_utc().unix_timestamp()
                            - c.scanned_at.unix_timestamp();
                        age_secs >= STALE_AFTER_SECS
                    }
                    // Nothing cached yet — leave it for the user's first
                    // explicit scan rather than spinning up detection
                    // silently on a fresh install.
                    None => false,
                }
            };
            if !needs_rescan {
                continue;
            }
            tracing::info!("detection cache older than 24h, refreshing in background");
            let os = Os::current();
            // Reload state on each background pass so manual_paths overrides
            // that landed since the previous scan are honoured. Cheap (one
            // small JSON file read) and skipping it would let manual picks
            // silently disappear from the auto-refreshed report.
            let cli_state = match CliState::load_default() {
                Ok((s, _)) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "couldn't load state for background rescan");
                    continue;
                }
            };
            // No progress emit on the background path — the UI isn't
            // listening, and repainting a progress bar while the user is
            // on another page would be noise.
            let mut report = match detection::detect_all(os, &cli_state, |_, _| {}).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "background detection refresh failed");
                    continue;
                }
            };
            // Honour the user's blacklist on the background path too —
            // otherwise the cache flips back to including ignored slugs
            // every time the 24h scheduler fires.
            report.games.retain(|g| !cli_state.is_ignored(&g.slug));
            let cached = CachedDetection {
                report,
                scanned_at: OffsetDateTime::now_utc(),
            };
            {
                let state = app.state::<AppState>();
                *state.detection_cache.last.lock().unwrap() = Some(cached.clone());
            }
            if let Err(e) = save_detection_to_disk_atomic(&cached) {
                tracing::warn!(error = %e, "couldn't persist refreshed detection cache");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_override_path_rejects_empty() {
        let err = validate_override_path("   ").unwrap_err();
        assert!(err.contains("empty"), "expected legible error, got {err:?}");
    }

    #[test]
    fn validate_override_path_rejects_missing() {
        let err = validate_override_path("/definitely/not/a/real/path/zzz").unwrap_err();
        assert!(
            err.contains("doesn't exist"),
            "expected legible error, got {err:?}"
        );
    }

    #[test]
    fn validate_override_path_rejects_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let err = validate_override_path(tmp.path().to_str().unwrap()).unwrap_err();
        assert!(
            err.contains("isn't a folder"),
            "expected legible error, got {err:?}"
        );
    }

    #[test]
    fn validate_override_path_accepts_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let buf = validate_override_path(tmp.path().to_str().unwrap()).unwrap();
        assert_eq!(buf, tmp.path());
    }
}
