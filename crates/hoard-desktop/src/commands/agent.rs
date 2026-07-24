//! Live-agent commands: start/stop the watcher and forward its events.
//!
//! The agent runs in the background as long as the desktop app is open. It
//! watches the user's tracked-save folders, detects when games launch and
//! quit, and uploads snapshots after a debounce.
//!
//! `start_agent` is invoked once per session — usually right after login.
//! It does three things:
//!
//! 1. Hydrate the list of tracked saves from the local state file plus the
//!    server's known-paths data (so we know each game's Steam install dir
//!    for process matching).
//! 2. Spawn the live agent (`hoard_agent::agent::spawn`) with that list.
//! 3. Stand up a forwarder task that re-emits every `AgentEvent` as a
//!    Tauri event named `agent://<event_type>`. The frontend subscribes
//!    with `listen()`.
//!
//! `stop_agent` cleanly shuts the agent down on logout. `backup_now`
//! exposes the manual-trigger button on the dashboard.

use std::sync::{Mutex, OnceLock};

use hoard_agent::agent::{self, AgentConfig, AgentEvent, AgentSlotStatus, WatchedSave};
use hoard_agent::api::ApiClient;
use hoard_agent::config::CliConfig;
use hoard_agent::prefs::Prefs;
use hoard_agent::state::{CliState, SaveState};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use time::OffsetDateTime;
use tokio::sync::mpsc;

use super::library::current_client;
use crate::state::AppState;

/// Summary of the running agent that the UI can display in Settings.
#[derive(Debug, Clone, Serialize)]
pub struct AgentStatus {
    pub running: bool,
    pub watched_count: usize,
}

/// Clone of the live agent's `ApiClient`, kept so the cloud token-refresh path
/// can push a fresh Supabase JWT into the long-lived agent without rebuilding
/// it. The agent is spawned once per session with a single client; for a Hoard
/// Cloud session that client's JWT expires after ~1h, and nothing else refreshes
/// it. Before this hook the auto-restore sweep kept firing with the stale token
/// and 401'd every tick ("no se pudo restaurar …"). `ApiClient` shares its token
/// cell across clones, so calling `set_token` here updates the agent's copy too.
static AGENT_CLIENT: OnceLock<Mutex<Option<ApiClient>>> = OnceLock::new();

fn agent_client_slot() -> &'static Mutex<Option<ApiClient>> {
    AGENT_CLIENT.get_or_init(|| Mutex::new(None))
}

/// Remember the running agent's client so token refreshes can reach it.
pub fn register_agent_client(client: ApiClient) {
    *agent_client_slot().lock().unwrap() = Some(client);
}

/// Serialises concurrent `start_agent` calls. Boot rehydrate fires it from two
/// paths (the automatic-scheduler rehydrate and the cloud-login rehydrate), and
/// the `.await`s inside `start_agent` (the `is_cloud` probe, save hydration)
/// open a TOCTOU window between the `state.agent` "already running?" check and
/// where the handle is finally stored. Without this gate both callers pass the
/// check and spawn *two* agents, each with its own `ApiClient` token cell; only
/// the last-registered one gets `update_agent_token`, so the orphan agent's
/// uploads and presence heartbeats 401 forever on a stale JWT while the other
/// keeps working (observed 2026-07-21: factorio restored fine while its backup
/// 401'd seconds later). Held across the whole `start_agent` body so the second
/// caller observes the first's stored handle and no-ops.
fn agent_start_gate() -> &'static tokio::sync::Mutex<()> {
    static GATE: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    GATE.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Forget the agent's client (logout / agent stop).
pub fn clear_agent_client() {
    *agent_client_slot().lock().unwrap() = None;
}

/// Serializes read-modify-write of `state.json` so two backup/restore events
/// landing back-to-back don't clobber each other (`CliState::load + save` is
/// not atomic). The forwarder task is already single-threaded, but other Tauri
/// commands write `state.json` too; the lock keeps the agent's writes consistent.
static STATE_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn state_write_lock() -> &'static Mutex<()> {
    STATE_WRITE_LOCK.get_or_init(|| Mutex::new(()))
}

/// Apply `f` to the `state.json` row for `save_id` and persist, under the
/// process-wide write lock (BUG 1). Best-effort: a load/save failure or a
/// missing row (e.g. a cloud-orphan save backed up before adoption wrote a
/// local row) is logged and skipped — a persistence hiccup must not take the
/// event forwarder down.
fn update_save_state(save_id: &str, f: impl FnOnce(&mut SaveState)) {
    let _guard = state_write_lock().lock().unwrap();
    let (mut state, path) = match CliState::load_default() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "persist: couldn't load state.json");
            return;
        }
    };
    let Some(entry) = state.saves.get_mut(save_id) else {
        return;
    };
    f(entry);
    if let Err(e) = state.save(&path) {
        tracing::warn!(error = %e, "persist: couldn't write state.json");
    }
}

/// Push a freshly-rotated bearer token into the running agent's client, if any.
/// Called from the cloud token-refresh path so the agent's long-lived client
/// keeps working across JWT expiry. No-op when no agent is running.
pub fn update_agent_token(token: &str) {
    if let Some(client) = agent_client_slot().lock().unwrap().as_ref() {
        client.set_token(token);
    }
}

/// Boot the live agent and start forwarding events. No-op if it's already
/// running (returns the existing status).
#[tauri::command]
pub async fn start_agent(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<AgentStatus, String> {
    // Serialise starts: two concurrent boot-rehydrate callers must not both pass
    // the `already running?` check below and spawn a second, orphaned agent.
    // Held for the whole body (see `agent_start_gate`).
    let _start = agent_start_gate().lock().await;

    if state.agent.lock().unwrap().is_some() {
        return Ok(AgentStatus {
            running: true,
            watched_count: 0, // unknown without round-tripping the agent
        });
    }

    // One agent per machine: if a `hoard sync` daemon already holds the shared
    // lock, don't spin up ours too — two agents fight over each save and rotate
    // the shared Cloud refresh token against each other (reuse-detection → 401).
    // Skip quietly (not an error): the GUI still works, sync just runs in the
    // daemon. When the daemon stops the next scan boots our agent.
    if let Some(pid) = hoard_agent::instance::live_owner() {
        tracing::warn!(
            pid,
            "another Hoard agent (CLI sync) is running; not starting the desktop agent"
        );
        return Ok(AgentStatus {
            running: false,
            watched_count: 0,
        });
    }

    let client = current_client(&state)?;
    // Keep a handle on the agent's client so the cloud token-refresh path can
    // swap in a fresh JWT as it rotates (see `update_agent_token`). `ApiClient`
    // shares its token cell across clones, so this clone stays in lock-step.
    register_agent_client(client.clone());
    // Probe once now (before `client` is moved into the agent) whether this is
    // a self-hosted server. Self-hosted gets server→app push via the `/v1/events`
    // SSE stream; cloud uses Supabase Realtime instead, so we skip it there.
    let server_is_cloud = client.is_cloud().await;
    let saves = hydrate_watched_saves(&state).map_err(|e| e.to_string())?;
    let watched_count = saves.len();

    // Pull the user's auto-restore preference + conflict retention so the
    // agent knows whether to pull missing snapshots on attach and how long
    // to keep conflict backups under `<state_dir>/conflicts/`.
    let prefs_loaded = Prefs::load_default().ok();
    let auto_restore = prefs_loaded
        .as_ref()
        .map(|(p, _)| p.auto_restore)
        .unwrap_or(false);
    let global_sync = prefs_loaded
        .as_ref()
        .map(|(p, _)| p.global_sync)
        .unwrap_or(false);
    let conflict_retention_days = prefs_loaded
        .as_ref()
        .map(|(p, _)| p.conflict_retention_days)
        .unwrap_or(14);
    // "Ahorro de datos" knob → minimum interval between snapshots per save
    // (ADR 0018 eje A). Maps `data_saving ∈ [0,1]` to 5s..600s via the
    // shared lerp helper so the cadence floor matches the server-side
    // retention scaling.
    let min_snapshot_interval_secs = prefs_loaded
        .as_ref()
        .map(|(p, _)| agent::min_snapshot_interval_for(p.data_saving))
        .unwrap_or_else(|| agent::min_snapshot_interval_for(0.3));
    // state_dir resolution can fail on locked-down hosts (no $HOME etc).
    // When it does, fall back to None — the agent then keeps the legacy
    // "never destroy local" behaviour for conflicts.
    let conflict_root = CliConfig::state_dir().ok().map(|d| d.join("conflicts"));
    let config = AgentConfig {
        auto_restore,
        global_sync,
        conflict_root,
        conflict_retention_days,
        min_snapshot_interval_secs,
        ..AgentConfig::default()
    };

    let (events_tx, mut events_rx) = mpsc::channel::<AgentEvent>(256);
    // Capture per-save metadata before moving `saves` into the agent so we
    // can fire `agent://watcher-armed` for each slot right after spawn.
    // The frontend uses these events to flip the LiveStatus indicator to
    // "watching" without having to round-trip `agent_status`.
    let armed: Vec<WatcherArmed> = saves
        .iter()
        .map(|s| WatcherArmed {
            save_id: s.save_id.clone(),
            game_slug: s.game_slug.clone(),
        })
        .collect();
    // Capturado antes de mover `config` al agente (lo usa el forwarder para no
    // marcar como "esperando" el debounce rutinario).
    let debounce_ms = config.debounce_secs.saturating_mul(1000);
    // Presence heartbeater (Eye panel): same client (shared token cell keeps
    // it authenticated across JWT rotations), cloned before `client` moves
    // into the agent. It gates itself to Cloud servers internally.
    let (presence, _presence_task) = hoard_agent::presence::spawn(client.clone());
    let (handle, _task) = agent::spawn(client, config, saves, events_tx);

    // Fan out the synthetic watcher-armed events. Done after `spawn` so the
    // frontend's "armed" count never exceeds what the agent actually
    // tracks. Best-effort — emit failures fall through.
    for entry in &armed {
        let _ = app.emit("agent://watcher-armed", entry);
    }

    // Forwarder task — translate AgentEvent into Tauri events the UI can
    // subscribe to. `agent://*` is our private event namespace; the
    // dashboard listens with a single `listen("agent://...")` per type.
    //
    // A handful of events are also re-emitted under second, UX-friendlier
    // topics so the new LiveStatus + ActivityFeed surface can subscribe to
    // semantic names ("upload", "throttled") without forcing every legacy
    // listener to rename. The original `agent://backup-*` topics stay live.
    let app_for_emit = app.clone();
    let presence_for_events = presence.clone();
    // `debounce_ms` (capturado arriba, antes de mover `config`) distingue una
    // espera REAL por throttle —el min-interval aplazó la subida por encima del
    // debounce— del debounce rutinario de cada autosave. Sin esto, "en cola —
    // esperando…" salía en cada guardado aunque el suelo por defecto sea 0.
    tokio::spawn(async move {
        while let Some(ev) = events_rx.recv().await {
            // Presence: mirror game transitions to the heartbeater so the
            // other devices' Eye panels flip to "jugando" within a beat.
            match &ev {
                AgentEvent::GameStarted { game_slug, .. } => {
                    presence_for_events.game_started(game_slug.clone());
                }
                AgentEvent::GameStopped { game_slug, .. } => {
                    presence_for_events.game_stopped(game_slug.clone());
                }
                _ => {}
            }
            // The event variant name doubles as the Tauri channel suffix.
            // Serializing the whole enum gives the frontend a tagged
            // payload that's easy to discriminate on.
            let topic = match &ev {
                AgentEvent::GameStarted { .. } => "agent://game-started",
                AgentEvent::GameStopped { .. } => "agent://game-stopped",
                AgentEvent::BackupScheduled { .. } => "agent://backup-scheduled",
                AgentEvent::BackupStarted { .. } => "agent://backup-started",
                AgentEvent::BackupSuccess { .. } => "agent://backup-success",
                AgentEvent::BackupFailed { .. } => "agent://backup-failed",
                AgentEvent::BackupThrottled { .. } => "agent://backup-throttled",
                AgentEvent::BackupTooLarge { .. } => "agent://backup-too-large",
                AgentEvent::BackupTrimmed { .. } => "agent://backup-trimmed",
                AgentEvent::SaveAutoRestored { .. } => "agent://save-auto-restored",
                AgentEvent::SaveAutoRestoreFailed { .. } => "agent://save-auto-restore-failed",
                AgentEvent::BackupSkippedEmpty { .. } => "agent://backup-skipped-empty",
                AgentEvent::SaveConflictsBackedUp { .. } => "agent://save-conflicts-backed-up",
                AgentEvent::HeavyProcessDetected { .. } => "agent://heavy-process-detected",
                // Emitted for parity with the CLI feed; no UI listens yet.
                AgentEvent::RestoreDeferred { .. } => "agent://restore-deferred",
                AgentEvent::SaveAutoRestoreStuck { .. } => "agent://save-auto-restore-stuck",
                AgentEvent::SaveAutoRestoreRecovered { .. } => {
                    "agent://save-auto-restore-recovered"
                }
            };
            let _ = app_for_emit.emit(topic, &ev);

            // A heavy untracked game just appeared — kick off a detection scan
            // now instead of waiting for the periodic timer. `request_scan`
            // no-ops unless Modo Automático is on and debounces bursts.
            if let AgentEvent::HeavyProcessDetected { name } = &ev {
                tracing::info!(process = %name, "desktop: heavy untracked game suspected; requesting immediate scan");
                crate::commands::automatic::request_scan(app_for_emit.clone());
            }

            // Persist the anti-reupload signature + version cursor so the next
            // session doesn't re-push identical snapshots or re-download to
            // diff (BUG 1). The agent keeps these in memory only; making them
            // survive a restart is the desktop's job via `state.json`.
            match &ev {
                AgentEvent::BackupSuccess {
                    save_id,
                    version_num,
                    set_hash,
                    ..
                } => {
                    let version_num = *version_num;
                    let set_hash = set_hash.clone();
                    update_save_state(save_id, move |s| {
                        s.last_version_num = Some(version_num);
                        s.last_backup_at = Some(OffsetDateTime::now_utc());
                        if let Some(h) = set_hash {
                            s.set_hash = Some(h);
                        }
                    });
                }
                AgentEvent::SaveAutoRestored {
                    save_id,
                    version_num,
                    ..
                } => {
                    // The slot is now synced to this cloud version; remember it
                    // so the version-gate survives a restart and we don't
                    // re-download to diff every launch.
                    let version_num = *version_num;
                    update_save_state(save_id, move |s| {
                        s.last_version_num = Some(version_num);
                    });
                }
                _ => {}
            }

            // Aliases for the new UX surface. Same payload, friendlier
            // channel name. `throttled` only fires for the filesystem-
            // settled flavour of BackupScheduled — the "game stopped"
            // and "manual" reasons aren't throttling, they're triggers.
            match &ev {
                AgentEvent::BackupStarted { .. } => {
                    let _ = app_for_emit.emit("agent://upload-started", &ev);
                }
                AgentEvent::BackupSuccess { .. } => {
                    let _ = app_for_emit.emit("agent://upload-completed", &ev);
                }
                AgentEvent::BackupScheduled {
                    reason: hoard_agent::agent::BackupReason::FilesystemSettled,
                    delay_ms,
                    ..
                } if *delay_ms > debounce_ms => {
                    // Solo cuando el min-interval aplazó la subida más allá del
                    // debounce hay una espera de verdad que mostrar. El debounce
                    // rutinario (delay == debounce) no es "en cola — esperando".
                    let _ = app_for_emit.emit("agent://throttled", &ev);
                }
                _ => {}
            }
        }
    });

    *state.agent.lock().unwrap() = Some(handle);
    *state.presence.lock().unwrap() = Some(presence);

    // Hand the fresh agent whatever cloud heads the poller already pulled. The
    // poller's first tick fires immediately on sign-in and can easily beat this
    // spawn; before ADR 0021 D.10 that manifest was dropped silently and the
    // agent ran on an empty version cache — `cloud_ahead = false` for every
    // save, so cross-device updates never landed. No-op when nothing has been
    // pulled yet (self-hosted, or poller not started).
    crate::commands::cloud_pull::feed_agent_versions(&app).await;
    // Hold the cross-process lock for as long as our agent lives, so a `hoard
    // sync` daemon started afterwards backs off instead of double-running.
    *state.agent_lock.lock().unwrap() = Some(hoard_agent::instance::AgentLock::acquire());

    // Self-hosted server→app push. Rides alongside the agent: on each pushed
    // save it force-restores under sync global (the agent sweep is the airbag
    // for plain auto-restore). Cloud sessions get this from Supabase Realtime,
    // so only start it for self-hosted. Best-effort — never blocks agent boot.
    if !server_is_cloud {
        crate::commands::selfhosted_events::start(&app);
    }

    Ok(AgentStatus {
        running: true,
        watched_count,
    })
}

#[derive(Debug, Clone, Serialize)]
struct WatcherArmed {
    save_id: String,
    game_slug: String,
}

/// Tear the agent down. Used on logout and on app exit (best-effort).
#[tauri::command]
pub async fn stop_agent(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    // Stop the self-hosted SSE subscriber in lock-step with the agent — it has
    // nothing to dispatch to once the handle is gone, and on logout the creds
    // it reads are about to be wiped. No-op when it wasn't running.
    crate::commands::selfhosted_events::stop(&app);
    let handle = state.agent.lock().unwrap().take();
    // Final presence beat first, while the client's token is still valid (on
    // logout the creds are about to be wiped): flips this device's dot to
    // grey on the other machines' Eye panels immediately. Bounded wait inside.
    let presence = state.presence.lock().unwrap().take();
    if let Some(p) = presence {
        p.closing().await;
    }
    clear_agent_client();
    // Release the shared agent lock so a `hoard sync` daemon can take over.
    *state.agent_lock.lock().unwrap() = None;
    if let Some(h) = handle {
        h.shutdown().await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Force a backup right now for the given save. Bypasses the debounce.
#[tauri::command]
pub async fn backup_now(save_id: String, state: State<'_, AppState>) -> Result<(), String> {
    let handle = state
        .agent
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "Agent isn't running. Sign in first.".to_string())?;
    handle
        .backup_now(save_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Kick a staggered backup sweep across every tracked save. Backs the Modo
/// Automático backup tick (driven from Rust by `automatic::run_backup_sweep`):
/// instead of the old "loop `backup_now`
/// over every save" burst, the agent spreads each save's re-hash across an
/// effective window (grown when there are tens of GB of saves) so sustained
/// disk use stays low. The nominal window is the persisted
/// `automatic_backup_interval_secs`. No-op (not an error) when the agent isn't
/// running — the next login boots it and the next tick sweeps.
#[tauri::command]
pub async fn sweep_backups(state: State<'_, AppState>) -> Result<(), String> {
    let handle = state.agent.lock().unwrap().clone();
    let Some(h) = handle else {
        return Ok(());
    };
    let window_secs = Prefs::load_default()
        .map(|(p, _)| p.automatic_backup_interval_secs)
        .unwrap_or(3600);
    h.sweep_all(window_secs).await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Diagnostic snapshot of every slot the agent is currently tracking.
/// Powers the hidden "agent diagnostics" panel in Settings — the only
/// non-trace surface that reveals whether each slot's fs watcher actually
/// armed and what it's seen. Returns an empty vec when the agent is not
/// running (no error: the UI shows "agent stopped").
#[tauri::command]
pub async fn agent_status(state: State<'_, AppState>) -> Result<Vec<AgentSlotStatus>, String> {
    let handle = state.agent.lock().unwrap().clone();
    let Some(h) = handle else {
        return Ok(Vec::new());
    };
    h.status().await.map_err(|e| e.to_string())
}

// ---- helpers ----------------------------------------------------------

/// Build the initial watch list from local state. The server-side save row
/// gives us the slug; the local state file gives us the path on this
/// machine. We try to enrich each entry with its Steam install dir so the
/// process watcher has something to match against.
fn hydrate_watched_saves(_state: &State<'_, AppState>) -> anyhow::Result<Vec<WatchedSave>> {
    // Business logic lives in the agent so the CLI daemon builds the exact same
    // watch list (Steam-enriched saves + playtime-only slots). This wrapper is
    // just the desktop's entry point.
    let (cli_state, _) = CliState::load_default()?;
    Ok(hoard_agent::library::watched_saves_from_state(&cli_state))
}

/// Glue used by `add_game_to_tracking` so newly tracked saves auto-attach
/// to the running agent without forcing a full restart.
pub(crate) async fn attach_save_if_running(state: &State<'_, AppState>, save: WatchedSave) {
    let handle = state.agent.lock().unwrap().clone();
    if let Some(h) = handle {
        if let Err(e) = h.add_save(save).await {
            tracing::warn!(error = %e, "couldn't attach new save to live agent");
        }
    }
}

/// Apply the live-agent side effect that a save-settings change
/// (`hoard_agent::library::set_paused`/`set_preset`/`set_local_path`) asks for.
/// Keeps the desktop's attach/detach glue in one place; the CLI ignores the
/// reseat entirely (a separate daemon picks the change up on restart).
pub(crate) async fn apply_reseat(
    state: &State<'_, AppState>,
    reseat: hoard_agent::library::LiveReseat,
) {
    use hoard_agent::library::LiveReseat;
    match reseat {
        LiveReseat::Detach(id) => detach_save_if_running(state, id).await,
        LiveReseat::Attach(w) => attach_save_if_running(state, *w).await,
        LiveReseat::Reseat(id, w) => {
            detach_save_if_running(state, id).await;
            attach_save_if_running(state, *w).await;
        }
        LiveReseat::Noop => {}
    }
}

/// Glue used by `untrack_save` so removing a save also stops watching it.
pub(crate) async fn detach_save_if_running(state: &State<'_, AppState>, save_id: String) {
    let handle = state.agent.lock().unwrap().clone();
    if let Some(h) = handle {
        if let Err(e) = h.remove_save(save_id).await {
            tracing::warn!(error = %e, "couldn't detach save from live agent");
        }
    }
}

#[cfg(test)]
mod tests {
    use hoard_agent::library::name_matches;

    #[test]
    fn name_match_kebab_vs_titlecase() {
        assert!(name_matches("Stardew Valley", "stardew-valley"));
        assert!(name_matches("Hollow Knight", "hollow-knight"));
        assert!(name_matches(
            "Subnautica: Below Zero",
            "subnautica-below-zero"
        ));
        assert!(!name_matches("Stardew Valley", "stardew-vallei"));
        assert!(!name_matches("", ""));
    }
}
