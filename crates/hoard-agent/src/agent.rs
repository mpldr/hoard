//! Long-running "live agent" that watches tracked saves and backs them up.
//!
//! Three independent loops cooperate inside one Tokio task:
//!
//! 1. **Filesystem watcher** — `notify-debouncer-mini` aggregates raw inotify
//!    events into a debounced stream. When a save folder settles for
//!    `debounce_secs`, we enqueue a backup.
//! 2. **Process watcher** — a periodic `sysinfo` poll asks "is any tracked
//!    game's executable running?" and emits `GameStarted` / `GameStopped`
//!    transitions. On stop we also enqueue an immediate backup, since the
//!    user just finished playing.
//! 3. **Backup scheduler** — drains the queue, runs `upload_directory` per
//!    entry, and applies exponential backoff (`2 ** retry` seconds, capped)
//!    on failure up to `max_retries`.
//!
//! Everything outside the agent talks to it through two channels:
//! - `AgentCommand` (mpsc, in)  — add/remove watched saves, shut down.
//! - `AgentEvent` (mpsc, out) — fire-and-forget notifications the desktop UI
//!   surfaces as Tauri events.
//!
//! The agent never panics on a missing path or a failed upload; those become
//! events the UI can show. Loss-of-network is the common case and we want it
//! to look like "we'll retry in a bit", not a crash.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use hoard_core::kernel;
use hoard_core::kernel::correlation::accept_correlation_signals;
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, Debouncer};
use serde::{Deserialize, Serialize};
use sysinfo::{
    Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, RefreshKind, System, UpdateKind,
};
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant as TokioInstant;

use crate::api::{ApiClient, ApiError};
use crate::backup::{upload_directory_checked, BackupResult};

/// Configuration for the live agent. Defaults are tuned for v0.3's
/// "instant feel" priority:
///
/// - **5 s debounce**: short enough that auto-backup feels immediate
///   after a save, long enough to coalesce torn writes (Bethesda games,
///   Souls games re-write the save file mid-burst). v0.2's 30 s default
///   was much more conservative; product call to match the user's ask.
/// - **2 s process poll** *while a game is running*: catches "I quit the
///   game" within seconds. When idle the poll backs off to
///   `poll_secs * IDLE_POLL_MULT` (the common case is no game running, so
///   this keeps `/proc` scans — the agent's dominant idle cost — rare). The
///   refresh itself is name+exe only, never the full per-process snapshot.
/// - **5 retries** with exponential backoff covers "wifi blipped"
///   without pestering the user forever.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub debounce_secs: u64,
    pub poll_secs: u64,
    pub max_retries: u32,
    /// Mirror of `Prefs::auto_restore`. When `true`, every save the agent
    /// adopts (initial seed or live `AddSave`) is checked against the
    /// server: if the local path is missing or empty *and* the server has
    /// at least one snapshot, we restore the latest snapshot in the
    /// background and emit `AgentEvent::SaveAutoRestored`. Off by default
    /// because silently writing files under the user's `~` is the sort
    /// of side-effect that needs explicit opt-in.
    pub auto_restore: bool,
    /// Root directory the agent uses to park the *local* copy of a file
    /// before letting a newer remote version overwrite it (ADR 0014). The
    /// final path is `<conflict_root>/<save_id>/<rfc3339_ts>/<rel>`. When
    /// `None`, the agent falls back to 1.5.4 behaviour: a conflict where
    /// the remote is newer is *not* applied (we keep local) so data is
    /// never destroyed silently.
    pub conflict_root: Option<PathBuf>,
    /// Days to keep conflict backups under `conflict_root` before the
    /// per-tick sweep removes them. Mirrors `Prefs::conflict_retention_days`.
    pub conflict_retention_days: u32,
    /// Minimum seconds between two successful backups of the *same* save
    /// (ADR 0018, eje A — "ahorro de datos"). After a backup succeeds, the
    /// agent won't start another for this save until the interval elapses;
    /// intermediate writes coalesce into the next one (the final state is
    /// always uploaded). Kills the "one version per minute" cadence of games
    /// that autosave every few seconds (OpenTTD). `0` disables the floor
    /// (every settle backs up — sin espera). The desktop derives this from
    /// `Prefs::data_saving` via `min_snapshot_interval_for`: la franja baja del
    /// deslizador (incluido el default) da `0`; sólo al empujar hacia "ahorro"
    /// aparece un suelo, hasta 600 s.
    pub min_snapshot_interval_secs: u64,
    /// Mirror of `Prefs::global_sync`. Distinct from [`Self::auto_restore`]:
    /// it opts every save into restore (same effect as `auto_restore` on the
    /// eligibility floor) *and* unlocks the low-latency pull paths — the
    /// poller/SSE `ForceRestore` push and the pre-launch sync barrier on
    /// `GameStarted`. The version-gate inside `run_auto_restore`
    /// (`known >= latest`) still holds, so it never re-downloads a save the
    /// device is already current on. Backup-only presets
    /// (`policy.auto_restore == Some(false)`) still opt out.
    ///
    /// It does **not** bypass the "user is mid-session" guards (`is_running`,
    /// `has_pending`, recent-fs-event, recent-mtime). It used to — "pull the
    /// moment it's outdated, even while playing" — and on a single device
    /// that raced the user's own backup: the pull re-applied the last
    /// *uploaded* version over progress the debounced backup hadn't flushed
    /// yet, so intermediate sessions never got versioned (REPO data-loss
    /// incident, 2026-07-05). A mid-session pull is never needed for
    /// correctness: if another device genuinely advanced the save, our next
    /// upload gets a 409 non-fast-forward and the reconcile path merges the
    /// remote head in before retrying. So outdated-while-playing now defers
    /// to the reconciliation sweep, which catches up as soon as the session
    /// settles — and a deferred `ForceRestore` that finds un-flushed local
    /// changes flushes them immediately (bypassing the min-interval queue),
    /// so live progress becomes a cloud version within seconds instead of
    /// waiting out the debounce window. The only guarded-path exception is
    /// the pre-launch barrier (see `process_poll`), which is launch-scoped
    /// by construction.
    pub global_sync: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            debounce_secs: 5,
            poll_secs: 2,
            max_retries: 5,
            auto_restore: false,
            conflict_root: None,
            conflict_retention_days: 14,
            min_snapshot_interval_secs: 0,
            global_sync: false,
        }
    }
}

/// Umbral del deslizador "Ahorro de datos" por debajo del cual NO se impone
/// suelo entre snapshots: el cambio sube en cuanto se asienta el debounce, sin
/// "en cola — esperando". Cubre el default de fábrica (`data_saving = 0.3`) para
/// que el usuario nunca vea una subida en espera salvo que pida ahorrar a
/// propósito.
const DATA_SAVING_NO_FLOOR_UPTO: f64 = 0.4;

/// Map the user's `data_saving` knob (0..=1) to a minimum snapshot interval in
/// seconds (ADR 0018, Decisión 4). La franja baja (`k ≤ DATA_SAVING_NO_FLOOR_UPTO`,
/// incluido el default) devuelve `0`: sin espera, la subida es inmediata tras el
/// debounce. Por encima del umbral el suelo crece linealmente hasta 600 s
/// (`k = 1`, "máximo ahorro" ≈ 10 min entre snapshots). Los presets con suelo
/// explícito (`short_session` 30 s, `data_saver` 600 s) siguen mandando por save.
pub fn min_snapshot_interval_for(data_saving: f64) -> u64 {
    let k = data_saving.clamp(0.0, 1.0);
    if k <= DATA_SAVING_NO_FLOOR_UPTO {
        return 0;
    }
    let t = (k - DATA_SAVING_NO_FLOOR_UPTO) / (1.0 - DATA_SAVING_NO_FLOOR_UPTO);
    (600.0 * t).round() as u64
}

/// The *minimal* process-refresh set the agent actually consumes. The process
/// poll reads each process's `name()` (always populated, no flag), its `exe()`
/// for the legacy install-dir fallback, and its `cpu_usage()` to spot a
/// just-launched untracked game (see `process_poll`). Everything else
/// `ProcessRefreshKind::everything()` pulls — memory, disk I/O, environ,
/// cmdline, cwd, root, user — is dead weight re-read from `/proc/<pid>/*` for
/// every process on the box on every tick, and was the bulk of the agent's
/// idle CPU. `OnlyIfNotSet` reads each `exe` path exactly once per PID (it never
/// changes); `with_cpu` adds no per-process file read — utime/stime come from
/// the same `/proc/<pid>/stat` already parsed for the name, plus a single
/// global `/proc/stat` read per tick — so steady-state ticks stay cheap.
///
/// `Process::status()` (see [`is_defunct`]) needs no flag of its own and adds
/// no cost: `ProcessRefreshKind` has no switch for it because sysinfo always
/// populates it from the state field of that same already-parsed
/// `/proc/<pid>/stat` (macOS/Windows fill it from the process snapshot they
/// already walk). The zombie filter is free.
fn proc_refresh_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::new()
        .with_exe(UpdateKind::OnlyIfNotSet)
        .with_cpu()
}

/// Is this process listed by the OS but no longer able to run code? A zombie
/// has already exited and only lingers because its parent hasn't reaped it —
/// which under Proton is routine: the game quits, the wine supervisor leaves
/// the .exe defunct, and the entry can sit there until the prefix tears down
/// (on a Steam Deck, often not before the next reboot).
///
/// It matters because a defunct entry keeps its name and its exe path, so every
/// strong matcher in [`process_poll`] (name, identity token, open handles,
/// install dir) went on matching it and the slot stayed `is_running` for good.
/// That pinned the mid-session veto open and a save pushed from another device
/// never landed. A zombie cannot be writing a save file, so it cannot be
/// evidence that the user is playing.
fn is_defunct(status: ProcessStatus) -> bool {
    matches!(status, ProcessStatus::Zombie | ProcessStatus::Dead)
}

/// One save the agent is responsible for backing up.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchedSave {
    pub save_id: String,
    pub game_slug: String,
    pub display_name: String,
    pub label: String,
    pub local_path: PathBuf,
    /// Optional install directory (e.g. Steam's `steamapps/common/<game>`).
    /// Kept for the UI and for legacy install-dir-prefix matching as a
    /// fallback when [`Self::processes`] is empty.
    pub steam_install_dir: Option<PathBuf>,
    /// Process executable file names (case-insensitive, with extension on
    /// Windows). The agent's process poll matches against these to fire
    /// `GameStarted` / `GameStopped` transitions. Rarely populated (only the
    /// curated `builtin_processes_for` list — Minecraft, emuladores…); the
    /// TOML catalog that fed it se quitó en 1.5.0. Con la lista vacía el match
    /// NO se queda en `steam_install_dir`: el poll también casa por identidad
    /// genérica (nombre/carpeta del proceso vs slug del juego, list-free y
    /// multiplataforma — ver `game_identity_tokens` / `process_identity_candidates`).
    #[serde(default)]
    pub processes: Vec<String>,
    /// Resolved per-save sync overrides (from the save's preset). Empty by
    /// default = inherit every global `AgentConfig` setting. The agent reads
    /// `policy.<field>.unwrap_or(config.<field>)` at each decision point. See
    /// [`crate::presets`].
    #[serde(default)]
    pub policy: crate::presets::SavePolicy,
    /// Cloud version this device last committed or restored, read from
    /// `state.json` (`last_version_num`). Seeds the slot's `known_version`
    /// so the reconciliation sweep's version-gate is armed from the first
    /// tick after a restart: without it every restart re-downloads every
    /// snapshot to diff and drains the bandwidth quota. `None` for a
    /// freshly tracked save (nothing committed yet) is correct — the gate
    /// stays open so an empty/new device still pulls.
    #[serde(default)]
    pub known_version: Option<i64>,
    /// Skip-by-set-hash signature of the last successful upload, read from
    /// `state.json` (`set_hash`). Seeds the slot's `last_set_hash` so the
    /// first backup sweep after a restart can compare against it and skip a
    /// no-op upload instead of re-pushing an identical snapshot. `None` for a
    /// freshly tracked save (nothing committed yet). Without this every app
    /// restart re-uploads every save as a new identical version.
    #[serde(default)]
    pub set_hash: Option<String>,
    /// PLAYTIME-ONLY tracking: this entry is here purely to count hours played
    /// for the recap (hoard-wrapple), never to back up a save. A `track_only`
    /// slot arms no fs watcher and is skipped by every backup/restore/sweep
    /// path; the process poll still matches it (by `processes` / install dir)
    /// so [`crate::playtime`] accrues its time. Used for always-online games
    /// with no local save worth syncing (Fortnite, Rust, Valorant…). Surfaced
    /// in amber in the UI. `default` keeps older `state.json` files loading.
    #[serde(default)]
    pub track_only: bool,
}

/// Out-of-agent notifications. Frontend listens to these to drive the
/// dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    GameStarted {
        save_id: String,
        game_slug: String,
    },
    GameStopped {
        save_id: String,
        game_slug: String,
    },
    /// A backup will run after `delay_ms` unless cancelled. Used by the UI
    /// to show "next backup in 30s" pills.
    BackupScheduled {
        save_id: String,
        delay_ms: u64,
        reason: BackupReason,
    },
    BackupStarted {
        save_id: String,
        game_slug: String,
        /// Human label for the save (the partida name). Lets the UI show
        /// "Subiendo Factorio…" instead of the raw uuid.
        label: String,
    },
    BackupSuccess {
        save_id: String,
        version_num: i64,
        total_bytes: u64,
        /// Composite skip-by-set-hash signature of the snapshot just uploaded
        /// (`"<cheap>:<content>"`). The desktop persists it into
        /// `state.json` so the next session can skip a no-op re-upload of the
        /// same bytes. `None` only if the agent couldn't compute it.
        set_hash: Option<String>,
    },
    BackupFailed {
        save_id: String,
        /// Slug so the feed can show "factorio falló" instead of a raw uuid.
        game_slug: String,
        error: String,
        will_retry: bool,
    },
    /// The upload was deferred by the server's rolling bandwidth limit (429).
    /// Not a failure — the agent is waiting `retry_after_secs` for the window
    /// to slide and will retry automatically. The UI shows an amber
    /// "en espera, reintento en Xs" entry instead of a red "falló", so a
    /// first-time onboarding burst that briefly exceeds the window reads as
    /// throttled rather than broken.
    BackupThrottled {
        save_id: String,
        game_slug: String,
        label: String,
        retry_after_secs: u32,
    },
    /// The save is larger than the plan's per-save cap (413 `save_too_large`),
    /// so the upload can never succeed as-is — retrying is pointless and would
    /// just spam the feed every time the folder changes. Surfaced as its own
    /// event (not a generic `BackupFailed`) so the UI can show an actionable
    /// "supera el límite de tu plan, sube a Pro" message built from the
    /// structured fields instead of a cryptic raw 413, and mark the save
    /// terminal rather than "reintentando". `limit_bytes`/`actual_bytes` are
    /// `0` for a self-hosted 413 with no structured body.
    BackupTooLarge {
        save_id: String,
        game_slug: String,
        label: String,
        plan: String,
        limit_bytes: u64,
        actual_bytes: u64,
    },
    /// The save was bigger than the plan's per-save cap, so the agent uploaded
    /// only the newest files that fit and dropped the oldest (generic recency
    /// trim — no per-game knowledge). The backup **succeeded** (a
    /// `BackupSuccess` fires alongside), but it's *partial*: the UI surfaces an
    /// amber "tu plan no llega, sube a Pro" state rather than a plain green
    /// "ok", so a Free user knows their older saves aren't in the cloud even
    /// though sync is working. `omitted_*` count what was left out.
    BackupTrimmed {
        save_id: String,
        game_slug: String,
        label: String,
        kept_files: u64,
        omitted_files: u64,
        omitted_bytes: u64,
        plan: String,
        limit_bytes: u64,
    },
    /// The agent detected that the save's local folder was missing or empty
    /// on add and `Prefs::auto_restore` was enabled, so it downloaded the
    /// latest server snapshot into the folder. The UI uses this to toast
    /// "We restored your save from the cloud" and to nudge the dashboard
    /// pill back to a synced state.
    SaveAutoRestored {
        save_id: String,
        game_slug: String,
        version_num: i64,
        files_extracted: u64,
        bytes_extracted: u64,
    },
    /// Auto-restore was attempted but failed (network error, sha mismatch,
    /// permission denied writing to the local path). Surfaced separately
    /// from `BackupFailed` because the user-visible message is different:
    /// the save is left untouched and we want the UI to suggest "restore
    /// manually" rather than "we'll try again".
    ///
    /// A retry *is* scheduled (the reconciliation sweep re-fires once the
    /// slot's backoff elapses) — the doc here used to claim otherwise, which
    /// was wrong in a load-bearing way: it made the every-minute retry loop
    /// look intentional. Each occurrence is a transient toast; a save that
    /// keeps failing escalates to [`AgentEvent::SaveAutoRestoreStuck`].
    SaveAutoRestoreFailed {
        save_id: String,
        game_slug: String,
        error: String,
    },
    /// Auto-restore has failed [`AUTO_RESTORE_STUCK_AFTER`] times in a row on
    /// the same cloud version: this save is not syncing and won't fix itself.
    ///
    /// Exists because the July-2026 re-download incident was *silent*. Every
    /// individual failure emitted `SaveAutoRestoreFailed`, which the desktop
    /// renders as a toast — a notification the user dismisses, or never sees
    /// because it appeared while they were in-game. Eight days of a save
    /// silently not syncing (and re-downloading gigabytes to fail again) is
    /// the thing a toast structurally cannot tell you. This event is what the
    /// frontends turn into a *persistent* state: it stays on the save's card
    /// until the save actually recovers.
    ///
    /// One-shot per (save, version), throttled the way `RestoreDeferred` is:
    /// the sweep keeps retrying and keeps failing, but the user is told once.
    /// Cleared — and followed by [`AgentEvent::SaveAutoRestoreRecovered`] — on
    /// a successful attempt or when the cloud version changes.
    SaveAutoRestoreStuck {
        save_id: String,
        game_slug: String,
        /// Consecutive failures on this version at the moment we gave up
        /// treating it as transient. Shown to the user ("3×") so the state
        /// reads as a pattern rather than a one-off.
        failures: u32,
        /// The last error chain, so the card/log line says *why* rather than
        /// just "it's broken".
        error: String,
    },
    /// A save that had emitted [`AgentEvent::SaveAutoRestoreStuck`] restored
    /// successfully (or the cloud moved to a new version, giving it a fresh
    /// reason to try). Lets the frontends drop the persistent warning instead
    /// of leaving a stale "this is broken" badge on a save that now works —
    /// a warning that can't clear itself trains the user to ignore warnings.
    SaveAutoRestoreRecovered {
        save_id: String,
        game_slug: String,
    },
    /// A scheduled backup landed but the local folder was empty (or gone)
    /// at upload time. We deliberately do **not** push an empty snapshot —
    /// that would silently destroy the user's last good save on the server
    /// the next time they look at History. Instead we surface this event so
    /// the UI can toast "we skipped backup because the folder is empty; turn
    /// on auto-restore in Settings if you wanted it pulled back".
    ///
    /// Since 1.4.3. Pairs with `SaveAutoRestored` when `auto_restore` is on:
    /// in that case the agent fires the restore *instead* of this event.
    BackupSkippedEmpty {
        save_id: String,
        game_slug: String,
    },
    /// The diff-based auto-restore found N files where the remote snapshot
    /// was newer than the local copy (ADR 0014). Before overwriting, the
    /// agent moved each local version into `conflict_dir`. The UI surfaces
    /// a toast so the user can recover manually if mtime decided wrong.
    SaveConflictsBackedUp {
        save_id: String,
        game_slug: String,
        count: u64,
        conflict_dir: PathBuf,
    },
    /// The process poll spotted a heavy-CPU process that looks like a game
    /// (`correlation::is_game_like`) but matches no tracked save's process
    /// name — most likely a just-launched, not-yet-tracked game. The desktop
    /// reacts by firing an immediate detection scan instead of waiting out the
    /// periodic timer, so a new game lands in the Library within seconds of
    /// launch. Emitted at most once per PID until that process exits, so a
    /// game running for hours triggers a single scan, not one per tick.
    HeavyProcessDetected {
        /// Process name, for the toast ("Detectado posible juego: …") and logs.
        name: String,
    },
    /// An update from another device is ready to pull, but the save's
    /// mid-session guards vetoed it: the game is still running, or the folder
    /// has changes this device hasn't versioned yet. The agent remembers it
    /// (`SaveSlot::pull_pending`) and pulls the moment the game closes.
    ///
    /// The wait is worth surfacing because it can be long and looks like
    /// nothing happening: a Proton game that leaves its process behind holds
    /// the veto until the leftover is reaped. "Waiting for the game to close"
    /// is the difference between a user seeing sync work and a user reloading
    /// Steam to force it.
    ///
    /// Emitted once per waiting update (the sweep re-checks every tick), and
    /// again if a new session defers it anew.
    RestoreDeferred {
        save_id: String,
        game_slug: String,
        /// The guard that vetoed, straight from `mid_session_reason`.
        reason: String,
    },
}

/// Why we scheduled a backup. Useful in the UI to explain "the game just
/// closed, so I'm backing it up now" vs "the save folder changed".
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupReason {
    FilesystemSettled,
    GameStopped,
    Manual,
    /// One save inside a staggered "backup sweep" (Modo Automático's hourly
    /// hash pass). Spaced out across an effective window so disk I/O doesn't
    /// burst. Kept quiet in the activity feed — unlike a filesystem-settled
    /// backup there's no user-visible trigger, and N queued rows every hour
    /// would be noise — but the resulting upload still announces normally.
    SweepStaggered,
    /// A previous attempt burned its whole retry budget and failed for real.
    /// The upload is re-armed on a long backoff ([`BACKUP_RETRY_BACKOFF`]) so
    /// there's a way back without waiting on a new fs event. See
    /// [`AgentCommand::RetryBackupAfterFailure`].
    RetryAfterFailure,
}

/// Per-slot diagnostic snapshot. Surfaced by the hidden Settings diagnostics
/// panel so a user can verify the watcher actually armed and is seeing fs
/// events. Serializable so the desktop can hand it straight to the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSlotStatus {
    pub save_id: String,
    pub display_name: String,
    pub path: PathBuf,
    pub watcher_armed: bool,
    pub process_running: bool,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_fs_event_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub next_scheduled_backup_at: Option<OffsetDateTime>,
}

/// How a spawned auto-restore attempt ended. Drives how the slot's
/// `next_auto_restore_at` is re-armed and whether the consecutive-failure
/// counter moves, so the three error classes stay visibly distinct:
/// a 404 is permanent-ish, a 401 isn't the save's fault, and everything else
/// is the transient-or-chronic case the escalating backoff exists for.
#[derive(Debug, Clone)]
enum AutoRestoreDisposition {
    /// The attempt finished without error (restored, or nothing to pull).
    /// Resets the failure counter and any stuck state.
    Ok,
    /// 404: the save has no record/snapshot on the backend we're talking to
    /// (carried over from another account, stale state, remote purged). Parks
    /// the slot on the long not-found backoff. Not a "failure" for backoff
    /// purposes — retrying can't conjure a snapshot that doesn't exist, and
    /// this arm already paces itself.
    NotOnServer,
    /// 401: session-wide, not this save's problem — the stored cloud JWT is
    /// expired and the refresh hasn't landed in this client yet. Swallowed and
    /// left on the normal short cooldown so it retries as soon as the token is
    /// back. Deliberately does *not* touch the failure counter: counting a
    /// token blip toward "this save is stuck" would let one expired session
    /// mark every tracked save as broken.
    Unauthorized,
    /// 429: the server's rolling bandwidth limiter deferred this download. Like
    /// [`Self::Unauthorized`] it isn't this save's fault and must *not* touch the
    /// failure counter — counting a throttle toward "stuck" is exactly what made
    /// a busy reconciliation sweep spam "keeps failing to restore (3×)". Carries
    /// the server's `retry_after_secs` so the slot re-arms on the exact window
    /// slide instead of the generic 60s cooldown. Swallowed (no failure toast).
    Throttled { retry_after_secs: u32 },
    /// Any other error (network, sha mismatch, permission denied, timeout).
    /// Carries the formatted error chain for the event. Escalates the
    /// consecutive-failure counter and the backoff.
    Failed(String),
}

/// Commands the host (Tauri command handlers, tests) sends to the agent.
enum AgentCommand {
    // Boxed: `WatchedSave` is much larger than the other variants, so keeping
    // it inline made every `AgentCommand` value as big as a `WatchedSave`.
    AddSave(Box<WatchedSave>),
    RemoveSave(String),
    BackupNow(String),
    /// Staggered "backup sweep": re-hash every tracked save to catch changes
    /// the fs-watcher missed, but spread the per-save work over time so disk
    /// use doesn't spike. `window_secs` is the nominal sweep interval (the
    /// hourly cadence); the agent grows it into a longer *effective* window
    /// when the total save footprint is large, and schedules each save at a
    /// size-proportional offset within it. Saves already queued for backup
    /// (fs event or a still-running previous sweep) are skipped so ticks
    /// don't pile up. Fired by the desktop "Modo Automático" backup
    /// scheduler.
    SweepAll {
        window_secs: u64,
    },
    /// Internal: an auto-restore task finished writing files into a slot's
    /// local path. The slot's fs watcher was either never armed (path was
    /// missing on AddSave) or armed against an empty directory — either
    /// way we re-arm it now so the freshly-restored save is being watched.
    /// Not exposed through `AgentHandle` because only the auto-restore
    /// task ever fires it.
    RearmWatcher(String),
    /// Internal: a spawned auto-restore task finished (success or failure).
    /// Clears `slot.restoring` so the reconciliation sweep can try again
    /// next tick. `outcome` decides how the slot is re-armed — see
    /// [`AutoRestoreDisposition`].
    AutoRestoreFinished {
        id: String,
        disposition: AutoRestoreDisposition,
        /// The cloud version this slot is now synced to (the latest the restore
        /// observed), so the slot can remember it and the reconciliation sweep
        /// skips re-downloading the same version next tick. `None` when the
        /// attempt didn't reach a known version (404, transient failure).
        synced_version: Option<i64>,
        /// Post-merge set signature to adopt as the slot's `last_set_hash`.
        /// Only `Some` when the merged tree equals head exactly (no local
        /// divergence): adopting it makes the fs events the merge triggered
        /// settle as a `Skipped` no-op instead of firing a redundant upload of
        /// content already on the server. `None` leaves `last_set_hash` alone
        /// so a genuinely-diverged tree still uploads.
        post_restore_set_hash: Option<String>,
        /// Whether this attempt actually wrote files into the folder (restored
        /// or conflict-backed-up ≥1 file), as opposed to a no-op "already
        /// synced" pass. Only a real write bumps the folder mtime / echoes fs
        /// events, so only then do we stamp `last_restore_at` to keep the
        /// restore from vetoing the next pull (see `mid_session_reason`).
        wrote_files: bool,
    },
    /// Live-toggle `config.auto_restore` so the user's Settings change
    /// reaches the running agent without a restart. When flipped from
    /// `false → true` the agent also kicks an immediate reconciliation
    /// sweep so any tracked save with an empty local folder gets restored
    /// right away.
    SetAutoRestore(bool),
    /// Live-toggle `config.global_sync` (sync global). Distinct from
    /// `SetAutoRestore`: when flipped `false → true` the agent kicks an
    /// immediate sweep so every outdated-but-idle save catches up right away.
    /// See [`AgentConfig::global_sync`].
    SetGlobalSync(bool),
    /// Sync global, ruta de baja latencia: el poller `cloud_pull` (o el SSE
    /// self-hosted) detectó que un save concreto avanzó de versión y pide
    /// bajarlo ya, saltándose el cooldown del sweep. Respeta el flag
    /// `restoring` (no solapa restores), el opt-out backup-only por preset y
    /// los guards de sesión viva (`is_running`/`has_pending`/actividad
    /// reciente): con la partida abierta el pull NO se descarta, se anota en
    /// `SaveSlot::pull_pending` y se ejecuta al cerrarse el juego. El
    /// version-gate dentro de `run_auto_restore` evita la descarga si ya
    /// estamos al día.
    ForceRestore(String),
    /// DETECCIÓN (fase 3, ADR 0020): lista de carpetas candidatas detectadas
    /// pero AÚN NO rastreadas, que el escaneo del desktop quiere "sondear".
    /// El agente las vigila por mtime en cada tick de proceso: si una se
    /// reescribe mientras un juego está vivo, registra la correlación
    /// proceso↔escritura — la misma señal +0.50 que hoy sólo obtenían los
    /// saves ya rastreados. Rompe el huevo-y-gallina: jugar un juego no
    /// rastreado deja por fin rastro, y el siguiente escaneo lo asciende a
    /// `High` y lo auto-rastrea. Reemplaza el set entero en cada llamada.
    SetProbeCandidates(Vec<PathBuf>),
    /// Internal: a backup task exhausted its retry budget and failed for real.
    /// Sent by `run_backup_with_retry` instead of just giving up, because
    /// giving up wedged the slot: no `BackupDone` is emitted on this path (the
    /// local changes are still un-versioned, so `has_pending` must stay set to
    /// keep every restore off them) and `has_pending` is itself a
    /// `mid_session_reason` veto — so the save could neither be uploaded nor
    /// pulled until the user happened to write the folder again. The handler
    /// re-arms the upload on [`BACKUP_RETRY_BACKOFF`], the recovery path that
    /// doesn't depend on a new fs event.
    RetryBackupAfterFailure(String),
    /// Latest known cloud version per save id, as last seen by the `cloud_pull`
    /// poller's manifest. The poller already fetches the full manifest once per
    /// tick, so it hands the map to the agent and the reconciliation sweep can
    /// version-gate locally instead of each `run_auto_restore` re-fetching the
    /// same manifest (cloud) / hitting `get_save` per candidate (the old N+1).
    /// Replaces the whole map each call. Only populated on cloud; self-hosted
    /// and headless CLI leave it empty and fall back to the network fetch.
    SetCloudVersions(HashMap<String, i64>),
    QueryStatus(oneshot::Sender<Vec<AgentSlotStatus>>),
    Shutdown,
}

/// Handle returned by `spawn`. Cheap to clone (channel-cloning).
#[derive(Debug, Clone)]
pub struct AgentHandle {
    tx: mpsc::Sender<AgentCommand>,
}

impl AgentHandle {
    pub async fn add_save(&self, save: WatchedSave) -> Result<()> {
        self.tx.send(AgentCommand::AddSave(Box::new(save))).await?;
        Ok(())
    }

    pub async fn remove_save(&self, save_id: impl Into<String>) -> Result<()> {
        self.tx
            .send(AgentCommand::RemoveSave(save_id.into()))
            .await?;
        Ok(())
    }

    /// Force an immediate backup attempt for `save_id`, bypassing debounce.
    /// Used by the "Back up now" button.
    pub async fn backup_now(&self, save_id: impl Into<String>) -> Result<()> {
        self.tx
            .send(AgentCommand::BackupNow(save_id.into()))
            .await?;
        Ok(())
    }

    /// Kick a staggered backup sweep across every tracked save. `window_secs`
    /// is the nominal sweep interval; the agent spreads each save's re-hash
    /// across an effective window (grown when there are tens of GB of saves)
    /// so disk I/O stays spread out. Replaces the frontend's old "loop
    /// `backup_now` over every save" burst.
    pub async fn sweep_all(&self, window_secs: u64) -> Result<()> {
        self.tx.send(AgentCommand::SweepAll { window_secs }).await?;
        Ok(())
    }

    /// Diagnostic snapshot of every tracked slot. Backs the hidden Settings
    /// "agent diagnostics" panel — surfaces the same internal state we'd
    /// otherwise only see in `tracing` logs (watcher armed, last fs event,
    /// next scheduled backup).
    pub async fn status(&self) -> Result<Vec<AgentSlotStatus>> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.tx.send(AgentCommand::QueryStatus(resp_tx)).await?;
        Ok(resp_rx.await?)
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.tx.send(AgentCommand::Shutdown).await?;
        Ok(())
    }

    /// Push a new `auto_restore` preference into the running agent. The
    /// agent loop applies it to its own copy of `AgentConfig` and, on a
    /// `false → true` flip, immediately re-scans every slot so any
    /// already-empty folder is restored right away (instead of waiting
    /// for the next fs event / process tick).
    pub async fn set_auto_restore(&self, enabled: bool) -> Result<()> {
        self.tx.send(AgentCommand::SetAutoRestore(enabled)).await?;
        Ok(())
    }

    /// Push a new `global_sync` preference into the running agent. On a
    /// `false → true` flip the agent immediately sweeps every slot, pulling
    /// any outdated save that isn't mid-session (the version-gate keeps it
    /// free when the device is already current). See
    /// [`AgentConfig::global_sync`].
    pub async fn set_global_sync(&self, enabled: bool) -> Result<()> {
        self.tx.send(AgentCommand::SetGlobalSync(enabled)).await?;
        Ok(())
    }

    /// Ask the agent to pull a specific save's latest cloud version right now,
    /// bypassing the sweep cooldown. Used by the `cloud_pull` poller when sync
    /// global is on and it spots a save that advanced server-side, so the
    /// download starts within the poll interval instead of up to a cooldown
    /// later. No-op on the agent side if the save is unknown or already
    /// restoring; deferred to the sweep if the save is mid-session.
    /// See [`AgentCommand::ForceRestore`].
    pub async fn force_restore(&self, save_id: String) -> Result<()> {
        self.tx.send(AgentCommand::ForceRestore(save_id)).await?;
        Ok(())
    }

    /// Hand the agent the latest set of untracked candidate folders to probe
    /// for process↔write correlation (ADR 0020 fase 3). The desktop calls
    /// this after each automatic scan with the detected-but-untracked dirs.
    pub async fn set_probe_candidates(&self, dirs: Vec<PathBuf>) -> Result<()> {
        self.tx.send(AgentCommand::SetProbeCandidates(dirs)).await?;
        Ok(())
    }

    /// Feed the agent the latest cloud version per save id, as observed by the
    /// `cloud_pull` poller's manifest. Lets the reconciliation sweep skip the
    /// per-save metadata fetch it would otherwise make. See
    /// [`AgentCommand::SetCloudVersions`].
    pub async fn set_cloud_versions(&self, versions: HashMap<String, i64>) -> Result<()> {
        self.tx
            .send(AgentCommand::SetCloudVersions(versions))
            .await?;
        Ok(())
    }
}

/// Spawn the live agent. Returns a handle for sending commands and a task
/// handle the caller can `.abort()` for hard shutdown.
pub fn spawn(
    api: ApiClient,
    config: AgentConfig,
    initial_saves: Vec<WatchedSave>,
    events_tx: mpsc::Sender<AgentEvent>,
) -> (AgentHandle, JoinHandle<()>) {
    let (cmd_tx, cmd_rx) = mpsc::channel::<AgentCommand>(64);

    // Pre-seed the agent with already-tracked saves so the desktop app can
    // start watching as soon as login completes.
    let cmd_tx_seed = cmd_tx.clone();
    if !initial_saves.is_empty() {
        tokio::spawn(async move {
            for s in initial_saves {
                let _ = cmd_tx_seed.send(AgentCommand::AddSave(Box::new(s))).await;
            }
        });
    }

    // The agent loop needs its own clone of `cmd_tx` so background tasks
    // it spawns (auto-restore is the only one today) can post commands
    // back to it — e.g. `RearmWatcher` after files land on disk.
    let cmd_tx_loop = cmd_tx.clone();
    let task = tokio::spawn(run_agent(api, config, cmd_rx, cmd_tx_loop, events_tx));
    (AgentHandle { tx: cmd_tx }, task)
}

/// Signal from a finished backup task back to the agent loop.
struct BackupDone {
    save_id: String,
    /// `Some` when a new snapshot was uploaded — carries the fresh set
    /// signature to cache on the slot. `None` when the backup was skipped
    /// (unchanged) or the folder was empty, so the slot keeps its previous
    /// signature.
    new_set_hash: Option<String>,
    /// `true` only when a real snapshot reached the server. The min-interval
    /// throttle anchors on `last_backup_at`, which must advance **only** on a
    /// genuine upload. A skip (unchanged bytes) or an empty/missing folder is
    /// not a backup: if it bumped the anchor, the next real change would be
    /// throttled a full `min_snapshot_interval_secs` out — and with
    /// auto-restore re-emptying the folder each cycle, the anchor would keep
    /// advancing on phantom "backups" and a short play session would never
    /// flush its progress before the game closed (R.E.P.O. regression).
    committed: bool,
    /// Version number of the snapshot just uploaded (`Some` only when
    /// `committed`). The agent advances the slot's `known_version` to this so
    /// the reconciliation sweep won't re-download a version this device itself
    /// just produced. `None` on skip/empty.
    version_num: Option<i64>,
}

/// Internal per-save bookkeeping.
struct SaveSlot {
    save: WatchedSave,
    /// Active fs debouncer. Armed in `handle_add` so the agent reacts to
    /// save-folder changes whether or not a game process is running. The
    /// pre-1.4 design built this lazily on `GameStarted`, which silently
    /// broke autobackup for any save without a matching process name
    /// (most non-Steam installs and most manifest entries without a
    /// `processes` field). See ADR / version1-5 §P1.4.0-0.
    watcher: Option<Debouncer<notify::RecommendedWatcher>>,
    /// Tokio task that fires the debounced backup. Cancelled and recreated
    /// on every fs event so the timer effectively resets.
    pending: Option<tokio::task::JoinHandle<()>>,
    /// Currently-running guess from the last process poll. Drives
    /// GameStarted/Stopped transitions.
    is_running: bool,
    /// La sesión en curso arrancó SOLO por señal débil (correlación
    /// carpeta→proceso) y ninguna señal fuerte la ha corroborado después. Si
    /// además termina sin una sola escritura en la carpeta, fue una sesión
    /// fantasma: el proceso correlacionado no era el juego, y se le pasa un
    /// strike a la observación ([`CorrelationStore::strike_phantom`]) para que
    /// una atribución envenenada (task horario, residente) se auto-descarte en
    /// vez de vetar el sync "mid-session" para siempre (caso MOUSE jul-2026).
    weak_session: bool,
    /// Last poll at which this slot's process was seen running. Powers the
    /// stop-debounce (`RUNNING_STICKY_SECS`): a correlation match is CPU-gated,
    /// so a Paradox game idling in a menu or on a loading screen can dip below
    /// the threshold for a tick and drop out of the running set. Without a
    /// grace window that flaps GameStarted/Stopped (and its final-flush backup)
    /// every few seconds. We keep the slot "running" until this is older than
    /// the grace. `None` until first seen running.
    last_running_seen: Option<TokioInstant>,
    /// Has the save folder changed since the last successful backup?
    /// Drives the v0.3 "final-flush-only-if-pending" rule on `GameStopped`
    /// — no point re-uploading an unchanged save just because the user
    /// quit. Set on every fs event; cleared on backup success.
    has_pending: bool,
    /// Most recent debounced fs event observed for this slot. Surfaced via
    /// `AgentSlotStatus` so the diagnostics panel can prove the watcher
    /// is actually seeing writes.
    last_fs_event_at: Option<OffsetDateTime>,
    /// When our own auto-restore last *wrote files* into this slot's folder
    /// (UTC). A restore bumps the folder mtime and echoes fs events, which would
    /// otherwise trip the `mid_session_reason` "folder touched recently" /
    /// "fs event observed recently" vetoes and throttle the NEXT cross-device
    /// pull for a whole `RECENT_SAVE_GRACE` — so back-to-back saves from another
    /// device landed at most one per window on the receiver. This lets the veto
    /// tell our own restore writes apart from the user's. Only set when files
    /// were actually applied (not on a no-op "already synced" pass).
    last_restore_at: Option<OffsetDateTime>,
    /// When the currently-pending backup will fire (UTC). `None` if no
    /// backup is scheduled. Recomputed in `schedule_backup`.
    next_scheduled_backup_at: Option<OffsetDateTime>,
    /// When the *oldest* un-flushed change in the current debounce window
    /// arrived (UTC). The notify debounce resets `next_scheduled_backup_at`
    /// on every write, so a game that autosaves every second (OpenTTD,
    /// factory builders) would reset the timer forever and never flush.
    /// This anchor lets the fs handler cap the total wait: once it's older
    /// than `MAX_BACKUP_WAIT_SECS`, we stop resetting and back up now.
    /// `None` when there are no pending changes; cleared on backup success.
    first_pending_event_at: Option<OffsetDateTime>,
    /// When this save was last successfully backed up (UTC). Anchors the
    /// `min_snapshot_interval_secs` floor (ADR 0018, eje A): a new backup is
    /// never scheduled to fire before `last_backup_at + interval`. `None`
    /// until the first success this session.
    last_backup_at: Option<OffsetDateTime>,
    /// `true` while a background auto-restore task is downloading into
    /// this slot's local path. Prevents the reconciliation sweep from
    /// firing the same restore twice. Cleared by
    /// `AgentCommand::AutoRestoreFinished` when the task ends (success
    /// or failure).
    restoring: bool,
    /// Earliest moment the reconciliation sweep is allowed to fire
    /// another auto-restore for this slot. Used as a 60-second cooldown
    /// after a failed attempt so a misbehaving server doesn't burn rate
    /// limits in a tight loop, and stretched by `auto_restore_failures`
    /// once a save keeps failing. `None` means "no cooldown active".
    next_auto_restore_at: Option<TokioInstant>,
    /// Consecutive auto-restore failures and the cloud version they're counted
    /// against (the "every other error" arm only — 404 and 401 don't count; see
    /// [`AutoRestoreDisposition`]). Drives [`auto_restore_backoff`], so the
    /// retry pacing escalates instead of re-downloading a multi-GB save every
    /// 60 s forever, and gates the one-shot stuck event.
    ///
    /// The counter is per *version*, not per save: a new version on the server
    /// is genuinely new content and a fresh reason to try now, so it resets the
    /// escalation rather than inheriting the old version's penalty. Comparing
    /// versions rather than elapsed time is what keeps that honest — a save
    /// stuck on v7 for an hour is still stuck on v7, no matter how long it's
    /// been.
    restore_failures: AutoRestoreFailures,
    /// Skip-by-set-hash signature of the last successful upload this session
    /// (ADR 0019). Compared against the freshly-walked signature before each
    /// backup; an unchanged signature means the watcher fired on a no-op
    /// settle, so the upload is skipped. In-memory only — cross-restart
    /// persistence is the CLI/desktop's job via `state.json`.
    last_set_hash: Option<String>,
    /// Cloud version this slot is known to be synced to — advanced on a genuine
    /// upload commit and after a successful auto-restore. The reconciliation
    /// sweep passes it to `run_auto_restore`, which skips the download-to-diff
    /// when the server's latest version isn't newer than this. `None` until the
    /// first commit/restore this session (the first sweep then downloads once to
    /// establish the baseline). This is what stops the every-tick re-download
    /// that used to burn the cloud bandwidth quota: a real cross-device update
    /// (another device committed a higher version) still pulls; our own folder
    /// churn no longer does.
    known_version: Option<i64>,
    /// A cross-device update is waiting to land in this slot, but a pull was
    /// vetoed by [`mid_session_reason`]. Set instead of dropping the
    /// `ForceRestore` outright: "the sweep re-runs every tick, so it lands as
    /// soon as the session settles" assumed the session ends. On a Steam Deck it
    /// often doesn't — suspend/resume keeps the game alive across days, and
    /// Proton regularly leaves the process behind after the user quits — so the
    /// veto held forever and a save made on another device only showed up after
    /// a Steam restart. Consumed by the `GameStopped` transition (see
    /// [`deferred_pull_ready`]), the first moment the folder is provably quiet.
    pull_pending: bool,
    /// The `GameStopped` final flush is in flight and owes this slot a deferred
    /// pull once its `BackupDone` lands. A one-shot licence, consumed by the
    /// next `BackupDone` whether or not the pull ends up firing: it scopes the
    /// recency-guard skip to the stop transition, so no other backup (a
    /// mid-session flush, a manual run) can license a pull into a folder that
    /// was just written.
    pull_after_flush: bool,
    /// Has [`AgentEvent::RestoreDeferred`] already gone out for the update
    /// currently waiting? The sweep re-evaluates the veto every tick, so without
    /// this the feed would take one "waiting" line per save per tick. Cleared
    /// when the game starts (a new session earns a new notice) and when the
    /// deferred pull finally fires.
    deferred_notified: bool,
}

async fn run_agent(
    api: ApiClient,
    mut config: AgentConfig,
    mut cmd_rx: mpsc::Receiver<AgentCommand>,
    cmd_tx: mpsc::Sender<AgentCommand>,
    events_tx: mpsc::Sender<AgentEvent>,
) {
    let mut slots: HashMap<String, SaveSlot> = HashMap::new();

    // Latest cloud version per save id, fed by the `cloud_pull` poller via
    // `SetCloudVersions`. Lets the reconciliation sweep version-gate locally
    // instead of having each `run_auto_restore` re-fetch the manifest. Empty
    // on self-hosted / headless CLI (no poller), where we fall back to the
    // per-save network fetch.
    let mut latest_versions: HashMap<String, i64> = HashMap::new();

    // Channel used by every fs watcher — debounced events all funnel here
    // and we route them by path. mpsc::unbounded would be fine since the
    // debouncer already throttles, but we cap at 256 to be defensive.
    let (fs_tx, mut fs_rx) = mpsc::channel::<PathBuf>(256);

    // Backup tasks signal "save_id of save just successfully backed up"
    // so the agent loop can clear `has_pending`. Cap matches `cmd_rx`.
    let (done_tx, mut done_rx) = mpsc::channel::<BackupDone>(64);

    // Process watcher: periodic poll. We refresh only the bits we care
    // about (process names + exe paths) to keep CPU near zero when idle.
    let mut sys =
        System::new_with_specifics(RefreshKind::new().with_processes(proc_refresh_kind()));
    let active_poll = Duration::from_secs(config.poll_secs.max(1));
    let idle_poll = active_poll.saturating_mul(IDLE_POLL_MULT);
    let mut poll = tokio::time::interval(active_poll);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Start fast so a game already open at launch is caught on the first tick;
    // `process_poll`'s return value drives the fast↔idle cadence thereafter.
    let mut polling_fast = true;

    // DETECCIÓN (fase 3, ADR 0020): store de correlación proceso↔escritura.
    // Cuando un save vigilado se reescribe, registramos qué proceso de juego
    // estaba vivo. Hoy alimenta atribución/aprendizaje sobre saves ya
    // rastreados; el observador sobre los roots amplios de `roots.rs` (para
    // DESCUBRIR carpetas nuevas) es el paso siguiente, más pesado, y queda
    // fuera de este cableado.
    let corr_path = crate::correlation::CorrelationStore::default_path().ok();
    let mut corr_store = corr_path
        .as_deref()
        .map(crate::correlation::CorrelationStore::load)
        .unwrap_or_default();

    // PLAYTIME: horas reales por día local. Se alimenta en cada tick de poll
    // con los saves cuyo proceso de juego sigue vivo (ver `process_poll`).
    // Adopta una vez el fichero legacy global al contexto activo antes de
    // cargar, para que la cuenta principal conserve su histórico y el resto
    // arranque vacío (el store se resuelve por contexto de sync).
    if let Err(e) = crate::playtime::PlaytimeStore::migrate_legacy_into_current_context() {
        tracing::debug!(error = %e, "agent: legacy playtime migration skipped");
    }
    let playtime_path = crate::playtime::PlaytimeStore::default_path().ok();
    let mut playtime = playtime_path
        .as_deref()
        .map(crate::playtime::PlaytimeStore::load)
        .unwrap_or_default();

    // DETECCIÓN (fase 3, ADR 0020): sonda de candidatos no-rastreados. Mapea
    // cada carpeta candidata → su última mtime-máxima observada. Cuando una
    // sube (escritura nueva) y hay un juego vivo, registra la correlación. El
    // baseline `None` se siembra en el primer tick sin registrar nada (así no
    // confundimos un fichero pre-existente reciente con una escritura recién
    // observada).
    let mut probes: HashMap<PathBuf, Option<std::time::SystemTime>> = HashMap::new();

    // PIDs we've already flagged as heavy untracked games this session (see
    // `AgentEvent::HeavyProcessDetected`). Keeps the immediate-scan trigger to
    // one event per process; `process_poll` prunes exited PIDs each tick so a
    // relaunch re-triggers.
    let mut reported_heavy: HashSet<Pid> = HashSet::new();
    // Estado cross-tick de la detección por correlación (señal DÉBIL), que ahora
    // es transición de PID en vez de presencia+CPU: `prev_pids` es la foto de
    // PIDs vivos del tick anterior (para saber cuáles NACIERON este tick) y
    // `corr_running` mapea `save_id → (pid, start_time)` del proceso que hoy
    // mantiene ese slot "corriendo". Un residente (Discord desde el boot) nunca
    // es nuevo, así que jamás dispara "arrancó"; el slot para cuando su PID muere.
    let mut prev_pids: HashSet<Pid> = HashSet::new();
    let mut corr_running: HashMap<String, (Pid, u64)> = HashMap::new();

    // PLAYTIME "solo lo que juegas": índice `carpeta Steam → slug` de la
    // biblioteca instalada. El poll atribuye horas a cualquier juego de Steam
    // que se ejecute, esté o no rastreado. Se reconstruye con TTL (ver
    // `playtime_index`); vacío hasta el primer `refresh_if_stale`.
    let mut steam_index = crate::playtime_index::SteamPlaytimeIndex::new();

    tracing::info!(
        debounce_secs = config.debounce_secs,
        poll_secs = config.poll_secs,
        max_retries = config.max_retries,
        "agent: started"
    );

    loop {
        tokio::select! {
            // ----- Commands from the host -----
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(AgentCommand::AddSave(save)) => {
                        let added_id = save.save_id.clone();
                        handle_add(
                            &mut slots, *save, &fs_tx, &api, &events_tx, &cmd_tx, &config,
                        );
                        // Baseline a freshly-added save that already holds
                        // content. When auto-restore is in flight (`restoring`)
                        // this no-ops and the post-restore path below handles
                        // it instead, so we never upload pre-restore content.
                        maybe_schedule_initial_backup(
                            &mut slots, &added_id, &api, &events_tx, &config, &done_tx, &cmd_tx,
                        );
                    }
                    Some(AgentCommand::RearmWatcher(id)) => {
                        // Auto-restore created files where there were none —
                        // the watcher we built (or skipped) on AddSave needs
                        // to be rebuilt against the now-existing directory.
                        if let Some(slot) = slots.get_mut(&id) {
                            arm_watcher(slot, &fs_tx);
                        }
                    }
                    Some(AgentCommand::AutoRestoreFinished { id, disposition, synced_version, post_restore_set_hash, wrote_files }) => {
                        // The background restore task signalled completion
                        // (success or failure). Clear the in-flight flag so
                        // the reconciliation sweep can try again once the
                        // cooldown expires — `next_auto_restore_at` was set
                        // when we spawned, so we don't reset it here…
                        let latest = latest_versions.get(&id).copied();
                        let mut stuck: Option<(String, u32, String)> = None;
                        let mut recovered: Option<(String, String)> = None;
                        if let Some(slot) = slots.get_mut(&id) {
                            slot.restoring = false;
                            // Remember the version we just synced to so the next
                            // sweep can skip the expensive download-to-diff when
                            // nothing newer has landed from another device.
                            if synced_version.is_some() {
                                slot.known_version = synced_version;
                            }
                            // Adopt the post-merge signature when the tree now
                            // equals head — stops the merge's own fs writes from
                            // triggering a redundant re-upload of head's content.
                            if let Some(h) = post_restore_set_hash {
                                slot.last_set_hash = Some(h);
                            }
                            // Stamp the restore so its own folder-touch / fs-event
                            // echo doesn't veto the NEXT pull (see
                            // `mid_session_reason`). Only when it actually wrote —
                            // a no-op "already synced" pass bumped nothing.
                            if wrote_files {
                                slot.last_restore_at = Some(OffsetDateTime::now_utc());
                            }
                            match disposition {
                                // …unless the save simply isn't on the server
                                // (404). Retrying every 60s can't conjure a
                                // snapshot that doesn't exist; park it on a long
                                // backoff so we check ~hourly instead of spamming.
                                AutoRestoreDisposition::NotOnServer => {
                                    slot.next_auto_restore_at = Some(
                                        TokioInstant::now()
                                            + Duration::from_secs(
                                                AUTO_RESTORE_NOT_FOUND_BACKOFF_SECS,
                                            ),
                                    );
                                }
                                // A token blip isn't this save's fault: leave the
                                // failure state untouched (neither escalate nor
                                // reset) and let the short cooldown retry once the
                                // refresh lands.
                                AutoRestoreDisposition::Unauthorized => {}
                                // Throttled: wait the server's exact window slide
                                // (clamped so a bogus value can't park the slot
                                // forever) and leave the failure counter alone —
                                // a bandwidth 429 isn't a broken save.
                                AutoRestoreDisposition::Throttled { retry_after_secs } => {
                                    let wait = (u64::from(retry_after_secs)).clamp(1, 300) + 2;
                                    // Per-save jitter so a sweep that 429'd a dozen
                                    // saves at once doesn't re-fire them all on the
                                    // same tick and stampede the window again. A
                                    // cheap deterministic hash of the id spreads
                                    // them across an extra few seconds.
                                    let jitter = {
                                        let mut h = std::collections::hash_map::DefaultHasher::new();
                                        std::hash::Hash::hash(&id, &mut h);
                                        std::hash::Hasher::finish(&h) % 6
                                    };
                                    slot.next_auto_restore_at = Some(
                                        TokioInstant::now() + Duration::from_secs(wait + jitter),
                                    );
                                }
                                AutoRestoreDisposition::Ok => {
                                    // The save works. Drop the escalation so the
                                    // next hiccup starts from 60s again, and take
                                    // down any persistent warning we raised.
                                    if slot.restore_failures.clear() {
                                        recovered =
                                            Some((id.clone(), slot.save.game_slug.clone()));
                                    }
                                }
                                AutoRestoreDisposition::Failed(err) => {
                                    let (delay, emit_stuck) =
                                        slot.restore_failures.record_failure(latest);
                                    slot.next_auto_restore_at = Some(TokioInstant::now() + delay);
                                    tracing::debug!(
                                        save_id = %id,
                                        failures = slot.restore_failures.consecutive,
                                        version = ?latest,
                                        backoff_secs = delay.as_secs(),
                                        "agent: auto-restore failed — escalating backoff"
                                    );
                                    // Escalation alone would make a chronically
                                    // broken save *quieter* over time. Surface it
                                    // once instead, so backing off doesn't turn
                                    // into hiding.
                                    if emit_stuck {
                                        stuck = Some((
                                            slot.save.game_slug.clone(),
                                            slot.restore_failures.consecutive,
                                            err,
                                        ));
                                    }
                                }
                            }
                        }
                        if let Some((game_slug, failures, error)) = stuck {
                            tracing::warn!(
                                save_id = %id,
                                game_slug = %game_slug,
                                failures,
                                error = %error,
                                "agent: auto-restore stuck — repeated failures on the same version"
                            );
                            let _ = events_tx.try_send(AgentEvent::SaveAutoRestoreStuck {
                                save_id: id.clone(),
                                game_slug,
                                failures,
                                error,
                            });
                        }
                        if let Some((save_id, game_slug)) = recovered {
                            tracing::info!(
                                save_id = %save_id,
                                "agent: auto-restore recovered after repeated failures"
                            );
                            let _ = events_tx
                                .try_send(AgentEvent::SaveAutoRestoreRecovered { save_id, game_slug });
                        }
                        // Restore has settled: if the slot still has no baseline
                        // (server was empty / 404 and the local folder holds
                        // content the watcher never saw written — the manual
                        // emulator-save case), take the first snapshot now
                        // instead of waiting on an fs write or the hourly sweep.
                        maybe_schedule_initial_backup(
                            &mut slots, &id, &api, &events_tx, &config, &done_tx, &cmd_tx,
                        );
                    }
                    Some(AgentCommand::SetAutoRestore(enabled)) => {
                        let was = config.auto_restore;
                        config.auto_restore = enabled;
                        tracing::info!(
                            auto_restore = enabled,
                            "agent: auto_restore preference updated"
                        );
                        // Flipping from off → on is the user's cue that they
                        // want any already-empty folder pulled back right
                        // now. Don't wait for the next poll tick.
                        if !was && enabled {
                            sweep_for_auto_restore(
                                &mut slots, &api, &events_tx, &cmd_tx, &config, &latest_versions,
                            );
                        }
                    }
                    Some(AgentCommand::SetGlobalSync(enabled)) => {
                        let was = config.global_sync;
                        config.global_sync = enabled;
                        tracing::info!(
                            global_sync = enabled,
                            "agent: global_sync preference updated"
                        );
                        // Flipping on means "catch me up now". Kick an
                        // immediate sweep; the version-gate keeps it free when
                        // already current, and the mid-session guards defer
                        // any save with a live game until it settles.
                        if !was && enabled {
                            sweep_for_auto_restore(
                                &mut slots, &api, &events_tx, &cmd_tx, &config, &latest_versions,
                            );
                        }
                    }
                    Some(AgentCommand::ForceRestore(id)) => {
                        // Low-latency pull requested by the cloud_pull poller
                        // (or the self-hosted SSE stream). Honour the
                        // backup-only opt-out and the in-flight guard, but
                        // ignore the cooldown — the poller already confirmed a
                        // version bump, so this is never spurious.
                        //
                        // The *failure* backoff is different from the cooldown and
                        // is not bypassed. "The poller confirmed a bump" is only a
                        // reason to skip pacing if the bump is news to us: the
                        // poller re-reports the same latest version every tick, so
                        // a save failing on v7 would get a fresh kick every poll
                        // interval, which is exactly the every-minute multi-GB
                        // retry loop the backoff exists to stop — the kick would
                        // walk straight around it. So a kick for the version that
                        // keeps failing waits out the backoff, while a kick for a
                        // newer version resets it and proceeds (the reset happens
                        // in `SetCloudVersions`, which both pollers send *before*
                        // this command, so by the time we get here `failing_version`
                        // is already cleared for a genuine bump).
                        //
                        // Only an equal, *known* version blocks: when we have no
                        // cloud version for the save (self-hosted SSE, which sends
                        // `ForceRestore` without populating the cache, or the window
                        // before the first poll) `failing_version` is `None` and we
                        // can't prove the kick is a repeat — let it through, since
                        // the kick is itself evidence something changed.
                        let backoff_held = slots.get(&id).is_some_and(|slot| {
                            slot.restore_failures.version.is_some()
                                && slot.restore_failures.version == latest_versions.get(&id).copied()
                                && slot
                                    .next_auto_restore_at
                                    .is_some_and(|t| TokioInstant::now() < t)
                        });
                        if backoff_held {
                            tracing::debug!(
                                save_id = %id,
                                version = ?latest_versions.get(&id).copied(),
                                "agent: force-restore ignored — same version is on the failure backoff"
                            );
                        }
                        let eligible = !backoff_held && slots.get(&id).is_some_and(|slot| {
                            !slot.save.track_only
                                && slot
                                    .save
                                    .policy
                                    .auto_restore
                                    .unwrap_or(config.auto_restore || config.global_sync)
                                && !slot.restoring
                        });
                        // Mid-session veto (REPO data-loss, 2026-07-05): a
                        // push-triggered pull used to land between the game's
                        // save write and the debounced backup, re-applying the
                        // last uploaded version over progress that hadn't been
                        // flushed yet — on a single device, the poller's echo
                        // of our own upload could erase a live session. Never
                        // pull into a folder the user is actively playing in.
                        //
                        // Deferring it, however, is not the same as dropping it.
                        // This used to drop the command on the theory that the
                        // sweep re-runs every tick with the same guards and
                        // would pick the delta up "as soon as the session
                        // settles". On a Steam Deck the session doesn't settle:
                        // suspend/resume keeps the process alive for days and
                        // Proton leaves it behind after the user quits, so the
                        // veto never lifted and the other device's save never
                        // arrived (the user's fix was reloading Steam). We now
                        // remember the pull and run it the moment the game
                        // closes — see `SaveSlot::pull_pending`.
                        let mid_session = if eligible {
                            slots.get(&id).and_then(mid_session_reason)
                        } else {
                            None
                        };
                        if let Some(reason) = mid_session {
                            // Deferring the pull must not leave the session's
                            // progress sitting un-versioned in the debounce /
                            // min-interval queue while the remote moves: flush
                            // it now. The upload either commits cleanly (this
                            // device becomes head and the deferred pull turns
                            // into a no-op) or hits the 409 non-fast-forward
                            // reconcile, which merges the newer remote head and
                            // versions both sides — either way the progress
                            // exists as a cloud version within seconds, even if
                            // it never ends up being the version the user keeps.
                            // 2s mirrors the GameStopped final flush; skipping
                            // the ADR 0018 min-interval floor here is deliberate
                            // (correctness beats data saving under contention).
                            //
                            // The poller only sends this after confirming a
                            // version bump, so an update really is waiting: mark
                            // the slot and tell the user it's queued.
                            let notify = slots
                                .get_mut(&id)
                                .is_some_and(note_deferred_pull);
                            if notify {
                                if let Some(slot) = slots.get(&id) {
                                    let _ = events_tx.try_send(AgentEvent::RestoreDeferred {
                                        save_id: id.clone(),
                                        game_slug: slot.save.game_slug.clone(),
                                        reason: reason.to_string(),
                                    });
                                }
                            }
                            let flush_pending =
                                slots.get(&id).is_some_and(|s| s.has_pending);
                            if flush_pending {
                                tracing::info!(
                                    save_id = %id,
                                    reason,
                                    "agent: force-restore deferred — flushing pending local changes so they're versioned first"
                                );
                                schedule_backup(
                                    &mut slots, &id, BackupReason::FilesystemSettled,
                                    Duration::from_secs(2),
                                    &api, &events_tx, &config, &done_tx, &cmd_tx,
                                );
                            } else {
                                tracing::info!(
                                    save_id = %id,
                                    reason,
                                    "agent: force-restore deferred — user is mid-session; the pull runs when the game closes"
                                );
                            }
                        } else if eligible {
                            let known_version =
                                slots.get(&id).and_then(|s| s.known_version);
                            let save = slots.get(&id).map(|s| s.save.clone());
                            if let Some(slot) = slots.get_mut(&id) {
                                slot.restoring = true;
                                slot.next_auto_restore_at = Some(
                                    TokioInstant::now()
                                        + Duration::from_secs(AUTO_RESTORE_COOLDOWN_SECS),
                                );
                            }
                            if let Some(save) = save {
                                tracing::info!(
                                    save_id = %id,
                                    "agent: force-restore (sync global, cloud delta)"
                                );
                                spawn_auto_restore(
                                    save,
                                    api.clone(),
                                    events_tx.clone(),
                                    cmd_tx.clone(),
                                    config.conflict_root.clone(),
                                    config.conflict_retention_days,
                                    known_version,
                                    // Authoritative path: the poller already
                                    // confirmed a bump, so fetch the real latest
                                    // rather than trusting a possibly-stale cache.
                                    // One save per command — no batch to share a
                                    // manifest with.
                                    None,
                                    None,
                                );
                            }
                        }
                    }
                    Some(AgentCommand::SetCloudVersions(map)) => {
                        tracing::debug!(
                            count = map.len(),
                            "agent: cloud version cache updated from poller"
                        );
                        // A save parked on the escalating failure backoff must not
                        // sit out a *new* version: the backoff is a statement about
                        // the version that kept failing, and the server just moved
                        // past it. Clear the escalation here — the single place the
                        // agent learns the cloud advanced — so the next sweep tick
                        // retries immediately instead of honouring a penalty of up
                        // to an hour that no longer applies to anything. Without
                        // this, only the sync-global `ForceRestore` kick would
                        // notice, and a save with restore-on/sync-global-off would
                        // stay stale for the rest of the backoff.
                        let mut recovered: Vec<(String, String)> = Vec::new();
                        for (id, slot) in slots.iter_mut() {
                            if !slot.restore_failures.is_active() {
                                continue;
                            }
                            if slot.restore_failures.version == map.get(id).copied() {
                                continue;
                            }
                            if slot.restore_failures.clear() {
                                recovered.push((id.clone(), slot.save.game_slug.clone()));
                            }
                            slot.next_auto_restore_at = None;
                        }
                        for (save_id, game_slug) in recovered {
                            tracing::info!(
                                save_id = %save_id,
                                "agent: cloud version advanced past the failing one — clearing stuck state"
                            );
                            let _ = events_tx
                                .try_send(AgentEvent::SaveAutoRestoreRecovered { save_id, game_slug });
                        }
                        latest_versions = map;
                    }
                    Some(AgentCommand::SetProbeCandidates(dirs)) => {
                        // Reemplaza el set conservando los baselines de los que
                        // siguen; los nuevos arrancan en `None` (se siembran en
                        // el próximo tick). Drop de los que ya no son candidatos
                        // (se rastrearon o dejaron de detectarse).
                        let mut next: HashMap<PathBuf, Option<std::time::SystemTime>> =
                            HashMap::with_capacity(dirs.len());
                        for d in dirs {
                            let baseline = probes.get(&d).copied().flatten();
                            next.insert(d, baseline);
                        }
                        tracing::debug!(count = next.len(), "agent: probe candidates updated");
                        probes = next;
                    }
                    Some(AgentCommand::RemoveSave(id)) => {
                        if let Some(slot) = slots.remove(&id) {
                            if let Some(p) = slot.pending {
                                p.abort();
                            }
                            // watcher dropped here, releasing inotify handle.
                        }
                    }
                    Some(AgentCommand::BackupNow(id)) => {
                        if slots.contains_key(&id) {
                            schedule_backup(
                                &mut slots, &id, BackupReason::Manual,
                                Duration::ZERO, &api, &events_tx, &config, &done_tx, &cmd_tx,
                            );
                        }
                    }
                    Some(AgentCommand::RetryBackupAfterFailure(id)) => {
                        if slots.contains_key(&id) {
                            // Clear the stale "a backup is queued" stamp before
                            // re-arming. `done_rx` never ran for the attempt
                            // that just died, so it still points at that dead
                            // schedule — and `schedule_backup` reads it as "a
                            // window is already open", takes the retry for a
                            // debounce reset and stays silent. The user would
                            // see the failure and no sign of the recovery.
                            if let Some(slot) = slots.get_mut(&id) {
                                slot.next_scheduled_backup_at = None;
                            }
                            tracing::info!(
                                save_id = %id,
                                backoff_secs = BACKUP_RETRY_BACKOFF.as_secs(),
                                "agent: backup retries exhausted — re-arming on the long backoff"
                            );
                            schedule_backup(
                                &mut slots, &id, BackupReason::RetryAfterFailure,
                                BACKUP_RETRY_BACKOFF,
                                &api, &events_tx, &config, &done_tx, &cmd_tx,
                            );
                        }
                    }
                    Some(AgentCommand::SweepAll { window_secs }) => {
                        sweep_all(
                            &mut slots, window_secs, &api, &events_tx,
                            &config, &done_tx, &cmd_tx,
                        );
                    }
                    Some(AgentCommand::QueryStatus(resp)) => {
                        let snapshot: Vec<AgentSlotStatus> = slots
                            .values()
                            .map(|s| AgentSlotStatus {
                                save_id: s.save.save_id.clone(),
                                display_name: s.save.display_name.clone(),
                                path: s.save.local_path.clone(),
                                watcher_armed: s.watcher.is_some(),
                                process_running: s.is_running,
                                last_fs_event_at: s.last_fs_event_at,
                                next_scheduled_backup_at: s.next_scheduled_backup_at,
                            })
                            .collect();
                        let _ = resp.send(snapshot);
                    }
                    Some(AgentCommand::Shutdown) | None => {
                        tracing::info!("agent: shutting down");
                        break;
                    }
                }
            }

            // ----- Filesystem debounce hits -----
            Some(path) = fs_rx.recv() => {
                if let Some(save_id) = match_save_for_path(&slots, &path) {
                    let now = OffsetDateTime::now_utc();
                    // Per-save preset overrides win over the global config.
                    let debounce_secs = slots
                        .get(&save_id)
                        .and_then(|s| s.save.policy.debounce_secs)
                        .unwrap_or(config.debounce_secs);
                    let min_interval_secs = slots
                        .get(&save_id)
                        .and_then(|s| s.save.policy.min_snapshot_interval_secs)
                        .unwrap_or(config.min_snapshot_interval_secs);
                    let mut delay = Duration::from_secs(debounce_secs);
                    if let Some(slot) = slots.get_mut(&save_id) {
                        slot.has_pending = true;
                        slot.last_fs_event_at = Some(now);
                        // Anti-starvation cap. Each fs event resets the
                        // debounce, so a game writing every second would
                        // never settle and never flush ("se quedó todo en
                        // cola"). Anchor the oldest un-flushed change; once
                        // it has waited MAX_BACKUP_WAIT_SECS, stop resetting
                        // and back up now even though writes keep arriving.
                        let waited_since = *slot.first_pending_event_at.get_or_insert(now);
                        if (now - waited_since).whole_seconds() >= MAX_BACKUP_WAIT_SECS {
                            delay = Duration::ZERO;
                            slot.first_pending_event_at = Some(now);
                        }
                        // Minimum-interval floor (ADR 0018, eje A). Never start
                        // a new backup sooner than `min_snapshot_interval_secs`
                        // after the last successful one — coalesce the burst
                        // into the next allowed slot. The anchor is the fixed
                        // `last_backup_at`, so repeated writes converge on the
                        // same fire time instead of drifting. Wins over the
                        // anti-starvation `delay = ZERO` above: we deliberately
                        // wait, and always upload the final state when we do.
                        if min_interval_secs > 0 {
                            if let Some(last) = slot.last_backup_at {
                                let earliest = last
                                    + Duration::from_secs(min_interval_secs);
                                if now + delay < earliest {
                                    delay = (earliest - now).unsigned_abs();
                                }
                            }
                        }
                    }
                    tracing::info!(
                        save_id = %save_id,
                        path = %path.display(),
                        delay_ms = delay.as_millis() as u64,
                        "agent: fs event observed; scheduling backup"
                    );
                    schedule_backup(
                        &mut slots, &save_id, BackupReason::FilesystemSettled,
                        delay,
                        &api, &events_tx, &config, &done_tx, &cmd_tx,
                    );

                    // DETECCIÓN (fase 3, ADR 0020): la carpeta se reescribió;
                    // muestrea los procesos de juego vivos y registra la
                    // correlación proceso↔escritura. Alimenta atribución y la
                    // señal +0.50 del scoring para descubrimientos futuros.
                    sys.refresh_processes_specifics(
                        ProcessesToUpdate::All,
                        true,
                        proc_refresh_kind(),
                    );
                    let games = crate::correlation::sample_game_processes(&sys);
                    if !games.is_empty() {
                        let dir = slots
                            .get(&save_id)
                            .map(|s| s.save.local_path.clone())
                            .unwrap_or_else(|| path.clone());
                        corr_store.record(&dir, &games);
                        if let Some(p) = &corr_path {
                            if let Err(e) = corr_store.save(p) {
                                tracing::debug!(error = %e, "agent: failed to persist correlation store");
                            }
                        }
                    }
                }
            }

            // ----- Process poll tick -----
            _ = poll.tick() => {
                // Refresca el índice de Steam si el TTL expiró (barato en estado
                // estable) antes de que el poll atribuya horas por carpeta.
                steam_index.refresh_if_stale();
                let any_running = process_poll(
                    &mut sys, &mut slots, &events_tx, &api, &config, &done_tx, &cmd_tx,
                    &mut playtime, playtime_path.as_deref(), &mut reported_heavy,
                    &mut corr_store, corr_path.as_deref(), &steam_index, &mut prev_pids,
                    &mut corr_running, &latest_versions,
                );
                // Watcher self-healing: a slot whose folder didn't exist when
                // the game was tracked (freshly installed, save dir created on
                // first save) never armed its watcher, and nothing rearms it
                // short of an auto-restore or an app restart. Every tick,
                // (re)arm any slot that has no watcher but whose folder now
                // exists. Cheap (a stat per tracked save) and silent for the
                // common already-armed case.
                for slot in slots.values_mut() {
                    if slot.save.track_only {
                        continue;
                    }
                    if slot.watcher.is_none() && slot.save.local_path.is_dir() {
                        tracing::info!(
                            save_id = %slot.save.save_id,
                            path = %slot.save.local_path.display(),
                            "agent: save folder now present; rearming fs watcher"
                        );
                        arm_watcher(slot, &fs_tx);
                    }
                }
                // Reconciliation backstop: every tick, look for tracked
                // saves whose local folder is empty and (a) restore is enabled
                // for that save (global default or per-save preset), (b) we're
                // not already restoring, and (c) the cooldown has elapsed.
                // Catches the cases the event-driven paths miss — uninstall
                // while Hoard was closed, network came back online after a
                // failed attempt, user just turned auto_restore on with several
                // stale slots. The per-slot filter inside resolves the
                // effective preference, so we always call (a backup-only save
                // is filtered out there, not here).
                sweep_for_auto_restore(
                    &mut slots, &api, &events_tx, &cmd_tx, &config, &latest_versions,
                );

                // DETECCIÓN (fase 3, ADR 0020): sonda de candidatos. `sys` ya
                // viene refrescado por `process_poll`. Para cada candidato no
                // rastreado, si su carpeta se reescribió desde el último tick
                // y hay un juego vivo, registra la correlación. Esto es lo que
                // rompe el huevo-y-gallina: el siguiente escaneo verá el bonus
                // +0.50 y ascenderá el candidato a `High`.
                if !probes.is_empty() {
                    probe_candidates(&mut probes, &sys, &mut corr_store, corr_path.as_deref());
                }

                // Adapt the poll cadence to whether anything is running. Only
                // rebuild the interval on an actual transition so steady state
                // never churns the timer.
                if any_running != polling_fast {
                    polling_fast = any_running;
                    let period = if any_running { active_poll } else { idle_poll };
                    poll = tokio::time::interval_at(TokioInstant::now() + period, period);
                    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                }
            }

            // ----- Backup success notifications -----
            Some(done) = done_rx.recv() => {
                // Was this the `GameStopped` final flush a deferred pull is
                // waiting on? Taken here so the licence can't outlive the one
                // backup it was granted for.
                let mut owed_pull = false;
                if let Some(slot) = slots.get_mut(&done.save_id) {
                    slot.has_pending = false;
                    slot.next_scheduled_backup_at = None;
                    slot.first_pending_event_at = None;
                    owed_pull = std::mem::take(&mut slot.pull_after_flush);
                    // Advance the throttle anchor only on a real upload — a
                    // skip/empty must not push the next change a full
                    // min-interval into the future.
                    if done.committed {
                        slot.last_backup_at = Some(OffsetDateTime::now_utc());
                        // Remember the version we just produced so the sweep
                        // won't re-download our own upload to diff it.
                        if done.version_num.is_some() {
                            slot.known_version = done.version_num;
                        }
                    }
                    if let Some(h) = done.new_set_hash {
                        slot.last_set_hash = Some(h);
                    }
                }
                // The exit save is versioned and the folder has gone quiet:
                // run the pull the session deferred. A no-op unless the game
                // really is closed and nothing else is pending — a relaunch
                // during the flush's 2s window defers it again, and
                // `pull_pending` keeps it owed until the next quiet moment.
                if owed_pull {
                    run_deferred_pull(
                        &mut slots, &done.save_id, &api, &events_tx, &cmd_tx,
                        &config, &latest_versions,
                    );
                }
            }
        }
    }
}

/// Register a save with the agent and arm its fs watcher immediately.
///
/// Pre-1.4 this deferred the watcher to `GameStarted`, which silently broke
/// autobackup for saves whose Ludusavi manifest entry had no `processes`
/// and that weren't a Steam install — the process poll never matched, the
/// watcher never armed, no events fired, the Dashboard pill stayed
/// "Inactivo" forever. Arming up front trades one inotify slot per tracked
/// save for end-to-end reliability; `process_poll` still emits
/// `GameStarted`/`GameStopped` for UI signalling but no longer gates the
/// fs subsystem.
///
/// Since 1.4.2: if `config.auto_restore` is on and the local folder is
/// missing or empty, kick off a background restore of the latest server
/// snapshot. Files land on disk, the agent loop receives `RearmWatcher`,
/// and the slot ends up watching the restored folder for the rest of
/// the session.
fn handle_add(
    slots: &mut HashMap<String, SaveSlot>,
    save: WatchedSave,
    fs_tx: &mpsc::Sender<PathBuf>,
    api: &ApiClient,
    events_tx: &mpsc::Sender<AgentEvent>,
    cmd_tx: &mpsc::Sender<AgentCommand>,
    config: &AgentConfig,
) {
    let save_for_restore = save.clone();
    let save_id = save.save_id.clone();
    let known_version = save.known_version;
    let last_set_hash = save.set_hash.clone();
    let mut slot = SaveSlot {
        save,
        watcher: None,
        pending: None,
        is_running: false,
        weak_session: false,
        last_running_seen: None,
        has_pending: false,
        last_fs_event_at: None,
        last_restore_at: None,
        next_scheduled_backup_at: None,
        first_pending_event_at: None,
        last_backup_at: None,
        restoring: false,
        next_auto_restore_at: None,
        restore_failures: AutoRestoreFailures::default(),
        last_set_hash,
        known_version,
        pull_pending: false,
        pull_after_flush: false,
        deferred_notified: false,
    };
    // Playtime-only entries exist purely to be matched by the process poll
    // so their hours accrue for the recap. They own no save folder, so we
    // never arm a watcher or run any restore/backup logic for them.
    if slot.save.track_only {
        slots.insert(save_id.clone(), slot);
        return;
    }
    arm_watcher(&mut slot, fs_tx);
    slots.insert(save_id.clone(), slot);

    // Since 1.5.4 auto-restore is diff-based and non-destructive: it always
    // runs when `auto_restore` is on, and decides per-file whether to copy.
    // If nothing's missing the task ends with `restored == 0` and no event
    // is emitted, so this is cheap even on a fully-populated slot.
    //
    // Since 1.5.5 (ADR 0014) the same "user is playing" guard from the
    // sweep applies here too: if the folder was just touched, the user is
    // likely mid-session — let the next sweep handle it once mtime
    // stabilises. Since 1.7.x this is unconditional (no longer gated on
    // `processes.is_empty()`): a game whose process name doesn't match the
    // manifest leaves `is_running` false *and* `processes` non-empty, so
    // the old gate skipped this guard and an auto-restore could fire
    // mid-session, resurrecting rotated-out autosaves. The recent-touch
    // check is the reliable "user is playing" signal regardless of process
    // detection.
    if config.auto_restore || config.global_sync {
        // The recent-touch defer applies to sync global too. It used to be
        // skipped there ("catch up immediately even if the folder was just
        // written"), but AddSave fires for every save at app start — before
        // the first process poll, so `is_running` can't veto yet — and this
        // pull is un-gated (`known_version = None`, it always downloads to
        // diff). Starting the app mid-game could therefore merge the last
        // uploaded version into a live folder. A just-written folder means
        // the user is (or was seconds ago) here; let the sweep catch up once
        // mtime stabilises.
        let recently_touched =
            is_path_recently_touched(&save_for_restore.local_path, RECENT_SAVE_GRACE);
        if recently_touched {
            tracing::debug!(
                save_id = %save_id,
                path = %save_for_restore.local_path.display(),
                "agent: handle_add auto-restore deferred — folder touched recently"
            );
        } else {
            if let Some(slot) = slots.get_mut(&save_id) {
                slot.restoring = true;
                slot.next_auto_restore_at =
                    Some(TokioInstant::now() + Duration::from_secs(AUTO_RESTORE_COOLDOWN_SECS));
            }
            spawn_auto_restore(
                save_for_restore,
                api.clone(),
                events_tx.clone(),
                cmd_tx.clone(),
                config.conflict_root.clone(),
                config.conflict_retention_days,
                // Fresh add: no known baseline yet, so this first pull downloads
                // once to establish it.
                None,
                // No poller cache to consult on a brand-new add either.
                None,
                // Single save — no sweep batch to share a manifest with.
                None,
            );
        }
    }
}

/// Ensure a freshly-added save with existing on-disk content gets a first
/// snapshot without waiting on a filesystem write or the hourly backup sweep.
///
/// Motivating case: emulator saves. The user points Hoard at a folder that
/// already holds a `.sav` the agent never observed being written, so no fs
/// event ever fires (`has_pending` stays false, the GameStopped path skips),
/// and the hourly sweep — the only other path to a first backup — may not run
/// for a long time (it restarts whenever the app relaunches). The result is a
/// tracked save that never reaches the cloud (`last_backup_at = None`).
///
/// Fires only when the slot has no baseline yet (never backed up, no known
/// set-hash, nothing already queued) and the folder isn't empty, so it's a
/// one-shot for the add and a no-op for every established save. Reuses
/// `SweepStaggered` so the queued row stays quiet in the feed; the resulting
/// upload still announces normally. Skipped while `restoring` so we never
/// upload pre-restore content — the post-restore caller re-checks once the
/// restore has settled.
#[allow(clippy::too_many_arguments)]
fn maybe_schedule_initial_backup(
    slots: &mut HashMap<String, SaveSlot>,
    save_id: &str,
    api: &ApiClient,
    events_tx: &mpsc::Sender<AgentEvent>,
    config: &AgentConfig,
    done_tx: &mpsc::Sender<BackupDone>,
    cmd_tx: &mpsc::Sender<AgentCommand>,
) {
    let needs = match slots.get(save_id) {
        Some(slot) => {
            !slot.save.track_only
                && !slot.restoring
                && slot.last_backup_at.is_none()
                && slot.last_set_hash.is_none()
                && slot.next_scheduled_backup_at.is_none()
                && !is_path_empty_or_missing(&slot.save.local_path)
        }
        None => false,
    };
    if !needs {
        return;
    }
    tracing::info!(
        save_id = %save_id,
        "agent: scheduling initial baseline backup for freshly-added save with existing content"
    );
    schedule_backup(
        slots,
        save_id,
        BackupReason::SweepStaggered,
        Duration::from_secs(2),
        api,
        events_tx,
        config,
        done_tx,
        cmd_tx,
    );
}

/// Minimum interval between successive auto-restore attempts for the
/// same save. Applied to both successful and failed attempts so a server
/// that's flapping ("snapshot available", "snapshot gone", "snapshot
/// available" — possible during a GC race) doesn't get hammered by the
/// reconciliation sweep.
const AUTO_RESTORE_COOLDOWN_SECS: u64 = 60;

/// Escalating pacing for an auto-restore that keeps failing on the *same*
/// cloud version: 60 s → 5 min → 15 min → 60 min, then 60 min forever.
///
/// The flat 60 s cooldown above is right for a one-off failure and wrong for a
/// chronic one. A restore that fails is not free: it fails *after* pulling the
/// snapshot, so each retry costs a full download. A Windows client hit exactly
/// this in July 2026 — auto-restores failing on the pre-1.0.3 60 s download
/// timeout, a failed restore recording no synced version, and the sweep
/// retrying at full download cost every minute. It re-downloaded the same 13
/// saves (~3.7 GB a burst) for eight days, ~60 GB/day, and nothing stopped it:
/// the bandwidth quota was working as designed (each burst fit inside the
/// window) and the only user-visible trace was a transient error toast.
///
/// Escalating cuts the steady-state cost by ~60× (1440 attempts/day → 24)
/// without dulling the common case: the first retry is still at 60 s, so a
/// transient blip recovers exactly as fast as it did before. We only slow down
/// once the save has *proved* that retrying doesn't work. The 60 min cap is
/// deliberately not the hour-scale parking of `AUTO_RESTORE_NOT_FOUND_BACKOFF_SECS`:
/// a 404 is a statement of fact ("this isn't here"), while these errors are
/// usually environmental (network, disk, permissions) and do fix themselves, so
/// the ceiling stays inside a typical play session and repair stays unattended.
const AUTO_RESTORE_FAILURE_BACKOFF_SECS: [u64; 4] = [60, 5 * 60, 15 * 60, 60 * 60];

/// Consecutive failures on the same version before the agent stops treating the
/// problem as transient and emits [`AgentEvent::SaveAutoRestoreStuck`].
///
/// Three is the point where the escalation has already spent 60 s + 5 min and is
/// about to stretch to 15 min and beyond: the retries have gone from "you won't
/// notice" to "this save is effectively not syncing", which is precisely when a
/// toast the user may have missed stops being adequate and the state has to
/// become persistent.
const AUTO_RESTORE_STUCK_AFTER: u32 = 3;

/// How long to wait before the next auto-restore attempt, given how many
/// consecutive failures this save has hit on the current cloud version.
///
/// `failures` is 1-based (1 = the attempt that just failed). Saturates at the
/// last step of [`AUTO_RESTORE_FAILURE_BACKOFF_SECS`] rather than growing
/// without bound — an unbounded backoff eventually parks a recoverable save for
/// days, which is the opposite failure of the one we're fixing.
fn auto_restore_backoff(failures: u32) -> Duration {
    let idx = (failures.max(1) as usize - 1).min(AUTO_RESTORE_FAILURE_BACKOFF_SECS.len() - 1);
    Duration::from_secs(AUTO_RESTORE_FAILURE_BACKOFF_SECS[idx])
}

/// Per-save auto-restore failure state: how many consecutive failures, on which
/// cloud version, and whether the user has already been told.
///
/// Kept as its own type rather than three loose fields on [`SaveSlot`] because
/// the three only make sense together — a count without the version it counts
/// for is what made the original bug possible ("retry forever" is what you get
/// when nothing remembers *what* kept failing). Bundling them also makes the
/// state machine unit-testable without standing up an agent loop.
#[derive(Debug, Default, Clone)]
struct AutoRestoreFailures {
    /// Consecutive failures against `version`.
    consecutive: u32,
    /// The cloud version those failures are counted for. `None` = unknown
    /// (self-hosted, or before the first poll).
    version: Option<i64>,
    /// Whether `SaveAutoRestoreStuck` has already been emitted for
    /// (this save, `version`).
    stuck_notified: bool,
}

impl AutoRestoreFailures {
    /// Record one failed attempt against the cloud's current `latest` version.
    ///
    /// Returns the delay to re-arm `next_auto_restore_at` with, and whether the
    /// caller should emit [`AgentEvent::SaveAutoRestoreStuck`] — true exactly
    /// once per (save, version), on the [`AUTO_RESTORE_STUCK_AFTER`]th failure.
    fn record_failure(&mut self, latest: Option<i64>) -> (Duration, bool) {
        // A different version is a fresh reason to try: start the escalation
        // over instead of inheriting the old version's penalty.
        if self.version != latest {
            self.version = latest;
            self.consecutive = 0;
            self.stuck_notified = false;
        }
        self.consecutive = self.consecutive.saturating_add(1);
        let emit_stuck = self.consecutive >= AUTO_RESTORE_STUCK_AFTER && !self.stuck_notified;
        if emit_stuck {
            self.stuck_notified = true;
        }
        (auto_restore_backoff(self.consecutive), emit_stuck)
    }

    /// Clear the failure state after a successful attempt. Returns whether a
    /// persistent warning was up (so the caller emits `...Recovered` and the UI
    /// can drop the badge).
    fn clear(&mut self) -> bool {
        let was_stuck = self.stuck_notified;
        *self = Self::default();
        was_stuck
    }

    /// Whether this save is currently carrying failure state worth clearing
    /// when the cloud version moves on.
    fn is_active(&self) -> bool {
        self.consecutive > 0 || self.stuck_notified
    }
}

/// How long to wait before re-arming a backup whose in-task retry budget
/// (`max_retries`, seconds-scale exponential backoff) is spent. Ten minutes is
/// deliberately far slower than that budget: what survives it isn't a flaky
/// packet but a real outage — server down, no network, disk unreadable, token
/// expired — and those resolve on the scale of minutes to hours. Long enough
/// that a dead backend isn't hammered (and the feed gets one row per ten
/// minutes, not a scroll of red), short enough that recovery is unattended.
const BACKUP_RETRY_BACKOFF: Duration = Duration::from_secs(10 * 60);

/// Idle process-poll slowdown factor. When no tracked game is running the agent
/// polls the process table every `poll_secs * IDLE_POLL_MULT` instead of every
/// `poll_secs`. Scanning every process on the box is the agent's dominant idle
/// cost, and while idle there's nothing to detect "stopping" — only launches,
/// whose detection just gains up to one idle interval of latency (absorbed by
/// the conflict-aware pre-launch barrier). The first running game snaps the
/// cadence back to `poll_secs`.
const IDLE_POLL_MULT: u32 = 4;

/// CPU floor (sysinfo `cpu_usage()`, where 100.0 = one fully-used core) above
/// which a *game-like, untracked* process is treated as a just-launched game
/// worth an immediate detection scan (`AgentEvent::HeavyProcessDetected`). Set
/// low enough to catch lightweight indie titles, high enough that idle helper
/// processes that slip past `correlation::is_game_like` don't keep firing. A
/// false positive only costs one cheap metadata scan (debounced desktop-side),
/// so we bias toward catching games.
const HEAVY_PROCESS_CPU_PCT: f32 = 25.0;

/// CPU floor (sysinfo `cpu_usage()`, donde 100.0 = un núcleo al máximo) para
/// que un match por CORRELACIÓN cuente como "el juego está corriendo". Los
/// process-names declarados por manifest cuentan haya o no CPU (un juego en
/// pausa sigue "corriendo"), pero la atribución carpeta→proceso de la
/// correlación es ruidosa: una utilidad de fondo (RTSS, ctfmon, taskhostw,
/// RadeonSoftware…) que toca una carpeta de save queda correlacionada y, en
/// reposo a ~0%, dispararía un "arrancó" falso y un barrier de auto-restore
/// falso. Exigir CPU real separa un juego fuera de catálogo en juego activo de
/// un helper en reposo. Por debajo de `HEAVY_PROCESS_CPU_PCT` para que un juego
/// moderadamente activo siga contando.
const CORRELATION_MIN_CPU_PCT: f32 = 5.0;

/// Grace window (in *poll ticks*) before a slot that dropped out of the running
/// set is declared stopped. A correlation match is CPU-gated
/// (`CORRELATION_MIN_CPU_PCT`), so a game idling in a menu or grinding a loading
/// screen can dip under the floor for a tick and momentarily look stopped;
/// without this it flaps GameStarted/Stopped (and a final-flush backup) every
/// few seconds. We keep the slot "running" for this many consecutive
/// not-seen polls (converted to seconds via `poll_secs`, floored at
/// [`STRONG_STOP_GRACE_FLOOR_SECS`]) before firing GameStopped. A genuine quit
/// still resolves within the grace.
const RUNNING_STICKY_POLLS: u64 = 3;

/// Floor for the strong-signal stop grace (see the `sticky` computation below).
/// It only has to swallow a rare 1-tick process-table refresh race, so a handful
/// of seconds is plenty. Was 90 s — badly over-provisioned: because the
/// [`mid_session_reason`] veto keys on `is_running`, that 90 s got tacked onto
/// *every* GameStopped, inflating both close-detection latency ("2 min to notice
/// I quit") and cross-device restore latency (the receiver keeps vetoing pulls
/// for exactly this long after the game quits). 6 s ≈ RUNNING_STICKY_POLLS ticks
/// at the default 2 s poll, still comfortably above any real refresh hiccup.
const STRONG_STOP_GRACE_FLOOR_SECS: u64 = 6;

/// Backoff applied when an auto-restore fails with a 404: the save is tracked
/// locally but has no record/snapshot on the backend we're talking to (e.g.
/// saves carried over from another account, or a stale `state.json` entry).
/// Retrying on the normal 60s cooldown floods the log with WARNs forever, so
/// we space these out to roughly hourly — still self-heals if the user later
/// uploads the save, without the spam.
const AUTO_RESTORE_NOT_FOUND_BACKOFF_SECS: u64 = 60 * 60;

/// Hard ceiling on how long a continuously-writing save can defer its
/// backup. The notify debounce resets the timer on every write, so a game
/// that autosaves every second would never settle and never flush. Once
/// the oldest un-backed-up change has waited this long, the fs handler
/// forces the backup with a zero delay even though writes keep arriving.
/// Kept comfortably above the default 5 s debounce so normal saves still
/// coalesce; only pathological writers ever hit it.
const MAX_BACKUP_WAIT_SECS: i64 = 30;

/// True if `path` doesn't exist on disk, or exists as a directory that
/// contains no entries. Anything else (file, broken symlink, populated
/// directory) returns `false`. Errors reading the directory are treated
/// conservatively as "not empty" so we never wipe a user's save folder
/// just because we couldn't enumerate it (NFS hiccup, etc).
fn is_path_empty_or_missing(path: &Path) -> bool {
    if !path.exists() {
        return true;
    }
    if !path.is_dir() {
        return false;
    }
    match std::fs::read_dir(path) {
        Ok(mut it) => it.next().is_none(),
        Err(_) => false,
    }
}

/// Background task: resolve the latest snapshot for `save`, download it
/// into the local path, emit `SaveAutoRestored` on success or
/// `SaveAutoRestoreFailed` otherwise, and ping the agent loop to re-arm
/// the watcher against the now-populated folder.
#[allow(clippy::too_many_arguments)]
fn spawn_auto_restore(
    save: WatchedSave,
    api: ApiClient,
    events_tx: mpsc::Sender<AgentEvent>,
    cmd_tx: mpsc::Sender<AgentCommand>,
    conflict_root: Option<PathBuf>,
    conflict_retention_days: u32,
    known_version: Option<i64>,
    // Latest cloud version the `cloud_pull` poller last reported for this save,
    // when known. Lets `run_auto_restore` version-gate without its own metadata
    // fetch. `None` falls back to the per-save network call (self-hosted,
    // headless CLI, fresh add, or the authoritative force/barrier paths).
    cached_latest: Option<i64>,
    // One manifest fetch shared by every restore of the same sweep: when
    // `cached_latest` is `None` on a cold start (the poller hasn't filled the
    // cache yet) the first task pulls `/v1/cloud/sync` once and the rest
    // reuse it, instead of N tasks fetching the identical manifest (the
    // startup burst that tripped the server's poll guard).
    shared_manifest: Option<Arc<tokio::sync::OnceCell<HashMap<String, i64>>>>,
) {
    tokio::spawn(async move {
        tracing::debug!(
            save_id = %save.save_id,
            game_slug = %save.game_slug,
            path = %save.local_path.display(),
            "agent: auto-restore diff — checking server snapshot against local"
        );
        let retention = Duration::from_secs(u64::from(conflict_retention_days) * 86_400);
        let mut disposition = AutoRestoreDisposition::Ok;
        let mut synced_version: Option<i64> = None;
        // Adopted as the slot's `last_set_hash` only when the merge left the
        // tree equal to head (no divergence), so the writes the merge made
        // don't bounce back as a redundant upload. Stays `None` on a diverged
        // tree so the genuinely-new local content still uploads.
        let mut post_restore_set_hash: Option<String> = None;
        // True once we've actually written pulled files into the folder — used
        // to stamp `last_restore_at` so our own writes don't veto the next pull.
        let mut wrote_files = false;
        match run_auto_restore(
            &api,
            &save,
            conflict_root.as_deref(),
            retention,
            known_version,
            cached_latest,
            shared_manifest,
        )
        .await
        {
            Ok(Some(outcome)) => {
                // We downloaded and diffed against this version; remember it so
                // the next sweep can short-circuit.
                synced_version = Some(outcome.version_num);
                // When the merged tree equals head, adopt its signature so the
                // restore's own writes don't trigger a redundant re-upload.
                if !outcome.local_diverged {
                    post_restore_set_hash = outcome.disk_set_hash.clone();
                }
                let touched = outcome.files_restored + outcome.conflicts_backed_up;
                wrote_files = touched > 0;
                if touched > 0 {
                    tracing::info!(
                        save_id = %save.save_id,
                        version_num = outcome.version_num,
                        restored = outcome.files_restored,
                        backed_up = outcome.conflicts_backed_up,
                        local_wins = outcome.conflicts_local_wins,
                        bytes = outcome.bytes_extracted,
                        "auto-restore diff: applied {} files (incl. {} conflict-backups), {} kept local",
                        touched,
                        outcome.conflicts_backed_up,
                        outcome.conflicts_local_wins
                    );
                    let _ = events_tx
                        .send(AgentEvent::SaveAutoRestored {
                            save_id: save.save_id.clone(),
                            game_slug: save.game_slug.clone(),
                            version_num: outcome.version_num,
                            files_extracted: touched,
                            bytes_extracted: outcome.bytes_extracted,
                        })
                        .await;
                    if outcome.conflicts_backed_up > 0 {
                        if let Some(dir) = outcome.conflict_dir.clone() {
                            let _ = events_tx
                                .send(AgentEvent::SaveConflictsBackedUp {
                                    save_id: save.save_id.clone(),
                                    game_slug: save.game_slug.clone(),
                                    count: outcome.conflicts_backed_up,
                                    conflict_dir: dir,
                                })
                                .await;
                        }
                    }
                    // Tell the agent loop to rebuild the fs watcher now that
                    // the directory actually has contents. Safe to send even
                    // if it was already armed — `arm_watcher` overwrites.
                    let _ = cmd_tx
                        .send(AgentCommand::RearmWatcher(save.save_id.clone()))
                        .await;
                } else if outcome.conflicts_local_wins > 0 {
                    tracing::debug!(
                        save_id = %save.save_id,
                        local_wins = outcome.conflicts_local_wins,
                        "auto-restore diff: nothing copied; {} files newer locally",
                        outcome.conflicts_local_wins
                    );
                }
                // else: every file present and identical — silent no-op.
            }
            Ok(None) => {
                // Nothing to pull. `run_auto_restore` already logged which
                // case it was (already synced vs. no snapshots on the server)
                // — don't second-guess it here, the old unconditional "no
                // snapshots yet" line contradicted the up-to-date path.
            }
            Err(e) => {
                // A 404 means the save has no record/snapshot on the backend
                // (carried over from another account, stale state, or the
                // remote was purged). It's not a transient failure — don't
                // raise it to the user as an error and don't keep retrying on
                // the short cooldown; park it on a long backoff (below).
                let api_err = e.downcast_ref::<ApiError>();
                let not_on_server = matches!(api_err, Some(ApiError::NotFound));
                // A 401 is session-wide, not per-save: at launch the stored
                // cloud JWT can be expired and the desktop's refresh path
                // hasn't pushed a fresh token into this client yet, so the
                // startup reconciliation sweep would emit one
                // `SaveAutoRestoreFailed` per tracked save — a burst of "no se
                // pudo restaurar" popups. Swallow it (the global cloud status
                // already reflects the session problem) and let the normal
                // short cooldown retry once the token is refreshed.
                let unauthorized = matches!(api_err, Some(ApiError::Unauthorized));
                // A 429 is the rolling bandwidth limiter, not a per-save failure:
                // during a reconciliation sweep every tracked save races for the
                // same window, so one over-quota moment 429s a dozen restores at
                // once. Treated as a failure it burned the escalation budget and
                // fired "keeps failing to restore (3×)" for saves that were never
                // broken. Honour the server's retry_after and don't count it.
                let throttled = match api_err {
                    Some(ApiError::RateLimited { retry_after_seconds, .. }) => {
                        Some(*retry_after_seconds)
                    }
                    _ => None,
                };
                if not_on_server {
                    disposition = AutoRestoreDisposition::NotOnServer;
                    tracing::debug!(
                        save_id = %save.save_id,
                        "agent: auto-restore — save not on server (404); backing off"
                    );
                } else if let Some(retry_after_secs) = throttled {
                    disposition = AutoRestoreDisposition::Throttled { retry_after_secs };
                    tracing::debug!(
                        save_id = %save.save_id,
                        retry_after_secs,
                        "agent: auto-restore throttled (429); waiting the server window"
                    );
                } else if unauthorized {
                    disposition = AutoRestoreDisposition::Unauthorized;
                    tracing::debug!(
                        save_id = %save.save_id,
                        "agent: auto-restore deferred — session unauthorized (token refresh pending)"
                    );
                } else {
                    let chain = format!("{e:#}");
                    tracing::warn!(
                        save_id = %save.save_id,
                        error = %chain,
                        "agent: auto-restore failed"
                    );
                    let _ = events_tx
                        .send(AgentEvent::SaveAutoRestoreFailed {
                            save_id: save.save_id.clone(),
                            game_slug: save.game_slug.clone(),
                            error: chain.clone(),
                        })
                        .await;
                    disposition = AutoRestoreDisposition::Failed(chain);
                }
            }
        }
        // Always clear the slot's `restoring` flag, even on failure — the
        // reconciliation sweep is responsible for retrying once the
        // cooldown (or, on repeated failures, the escalating backoff the
        // handler arms from `outcome`) expires; we just need to mark this
        // attempt as done.
        let _ = cmd_tx
            .send(AgentCommand::AutoRestoreFinished {
                id: save.save_id.clone(),
                disposition,
                synced_version,
                post_restore_set_hash,
                wrote_files,
            })
            .await;
    });
}

/// Reconciliation sweep: every tick, schedule a diff-based auto-restore for
/// any save not already being restored and outside its cooldown window. The
/// restore task itself decides whether anything actually needs copying —
/// since 1.5.4 a populated local folder no longer skips the attempt at
/// this stage; it skips inside `restore_files_into` once we've compared
/// the snapshot against what's on disk.
///
/// Guards apply *before* spawning to avoid stomping on a save the user is
/// actively touching:
/// 1. `slot.is_running` → game is open, skip.
/// 2. `slot.has_pending` → un-flushed local changes queued, skip.
/// 3. `last_fs_event_at` within `RECENT_SAVE_GRACE` → the watcher saw a
///    write recently, skip.
/// 4. Disk mtime within `RECENT_SAVE_GRACE` → fallback for the startup
///    window before the agent has fs history of its own.
///
/// Since 1.7.x the activity guards (2, 3) drive the decision and the mtime
/// check is only a fallback. The earlier version gated solely on
/// `is_running` + dir mtime, both of which miss real-world cases: a game
/// whose process name doesn't match its manifest never sets `is_running`,
/// and autosavers that truncate-and-overwrite the same file in place don't
/// bump the *directory* mtime — so the sweep auto-restored mid-session,
/// re-downloading autosaves the game had already rotated away and failing
/// uploads as the restore mutated the folder under them. The agent's own
/// inotify stream catches both. The guards are unconditional: `global_sync`
/// no longer bypasses them (see [`AgentConfig::global_sync`] for the
/// data-loss incident that ended that).
///
/// Cheap: per-slot work here is just a `restoring` flag check and a
/// timer compare. The network/disk cost happens inside the spawned task,
/// which dedupes via `restoring` so the next sweep doesn't pile up.
fn sweep_for_auto_restore(
    slots: &mut HashMap<String, SaveSlot>,
    api: &ApiClient,
    events_tx: &mpsc::Sender<AgentEvent>,
    cmd_tx: &mpsc::Sender<AgentCommand>,
    config: &AgentConfig,
    latest_versions: &HashMap<String, i64>,
) {
    let now = TokioInstant::now();
    // Slots the mid-session guards vetoed while the poller's cache says the
    // cloud is ahead: an update really is waiting on them. Recorded here and
    // applied after the scan (the filter only holds `&SaveSlot`), so the
    // `GameStopped` transition can run the pull the moment the game closes
    // instead of leaving it to a sweep that a stuck process may veto forever.
    let mut deferred: Vec<(String, &'static str)> = Vec::new();
    // Collect candidate save_ids first to keep the borrow checker happy
    // (we mutate the slot afterwards, then spawn a task that holds a
    // clone of `WatchedSave`).
    let candidates: Vec<(String, WatchedSave)> = slots
        .iter()
        .filter(|(id, slot)| {
            // Playtime-only entries have no save folder to restore into.
            if slot.save.track_only {
                return false;
            }
            // Per-save preset can opt out of restore (backup-only) or opt in
            // even when the global default is off. `global_sync` raises the
            // floor: it counts as a global opt-in for restore, but a save
            // explicitly marked backup-only (`Some(false)`) still wins.
            if !slot
                .save
                .policy
                .auto_restore
                .unwrap_or(config.auto_restore || config.global_sync)
            {
                return false;
            }
            if slot.restoring {
                return false;
            }
            if let Some(t) = slot.next_auto_restore_at {
                if now < t {
                    return false;
                }
            }
            // Mid-session guards. These apply to sync global too: it used to
            // bypass them ("pull en el momento, even mid-session"), which on
            // a single device let the pull race the user's own debounced
            // backup and re-apply the last uploaded version over un-flushed
            // progress (REPO data-loss, 2026-07-05). The sweep re-runs every
            // tick and the version-gate keeps every retry free — but a tick
            // that keeps being vetoed never lands, so a veto with the cloud
            // ahead is also recorded on the slot and honoured at `GameStopped`.
            if let Some(reason) = mid_session_reason(slot) {
                tracing::debug!(save_id = %id, reason, "sweep: skipping — user is mid-session");
                if cloud_ahead(slot, latest_versions) {
                    deferred.push((id.to_string(), reason));
                }
                return false;
            }
            true
        })
        .map(|(id, slot)| (id.clone(), slot.save.clone()))
        .collect();

    for (id, reason) in deferred {
        let notify = slots.get_mut(&id).is_some_and(note_deferred_pull);
        if notify {
            if let Some(slot) = slots.get(&id) {
                let _ = events_tx.try_send(AgentEvent::RestoreDeferred {
                    save_id: id.clone(),
                    game_slug: slot.save.game_slug.clone(),
                    reason: reason.to_string(),
                });
            }
        }
    }

    // One manifest fetch for the whole batch when the poller cache is cold
    // (e.g. a cold start scheduling several restores at once): the first
    // spawned task that needs it pulls `/v1/cloud/sync` once and the rest
    // reuse the result instead of each fetching the identical manifest.
    let shared_manifest = Arc::new(tokio::sync::OnceCell::new());
    for (id, save) in candidates {
        let known_version = slots.get(&id).and_then(|s| s.known_version);
        let cached_latest = latest_versions.get(&id).copied();
        if let Some(slot) = slots.get_mut(&id) {
            slot.restoring = true;
            slot.next_auto_restore_at = Some(now + Duration::from_secs(AUTO_RESTORE_COOLDOWN_SECS));
        }
        tracing::debug!(
            save_id = %id,
            "agent: reconciliation sweep — scheduling diff auto-restore"
        );
        spawn_auto_restore(
            save,
            api.clone(),
            events_tx.clone(),
            cmd_tx.clone(),
            config.conflict_root.clone(),
            config.conflict_retention_days,
            known_version,
            cached_latest,
            Some(shared_manifest.clone()),
        );
    }
}

/// Grace window for the "save touched recently" heuristic in sweep guards.
/// Five minutes matches the ADR 0014 acceptance: while playing, the
/// process poll will normally mark the slot `is_running`; this catches the
/// case where the slot has no process match in the catalog.
const RECENT_SAVE_GRACE: Duration = Duration::from_secs(5 * 60);

/// The "user is mid-session" test shared by the reconciliation sweep and the
/// push-triggered `ForceRestore` path: the game's process is running, there
/// are un-flushed local changes queued for backup, the watcher saw a write
/// within [`RECENT_SAVE_GRACE`], or (fallback for the startup window before
/// the agent has fs history) the folder's disk mtime is that recent. Any of
/// these means a pull could overwrite progress the backup hasn't captured
/// yet, so restores must wait until the save settles. Returns the first
/// tripped guard for logging, `None` when the slot is quiet.
///
/// The watcher signals matter because inotify catches in-place file rewrites
/// that don't bump the directory's mtime (OpenTTD and other autosavers
/// truncate-and-overwrite the same .sav): never auto-restore into a folder
/// the user is actively writing, or the restore and the backup fight over
/// the same files.
fn mid_session_reason(slot: &SaveSlot) -> Option<&'static str> {
    // Sans-IO boundary (ADR 0021, Slice 1): the decision lives in the kernel;
    // this shell samples the non-determinism (`now`) and the world
    // (`folder_mtime`) and hands it in as data. The kernel's `veto_reason`
    // makes the same choice the old in-place logic did, returning the same
    // `&'static str` reasons — behaviour is identical.
    // Los campos que el kernel creció en el Slice 2 no los mira el veto de
    // sesión; el shell sólo rellena los que `veto_reason` consulta y deja el
    // resto en su default. `run_agent` no cambia.
    let state = kernel::State {
        is_running: slot.is_running,
        has_pending: slot.has_pending,
        last_fs_event_at: slot.last_fs_event_at,
        last_restore_at: slot.last_restore_at,
        ..Default::default()
    };
    let obs = kernel::Observation {
        folder_mtime: folder_own_mtime(&slot.save.local_path),
        ..Default::default()
    };
    let world = kernel::World {
        now: OffsetDateTime::now_utc(),
        seed: 0,
    };
    kernel::session::veto_reason(&state, &obs, &world)
}

/// Does the poller's version cache say this save moved past what this device
/// last committed or restored? A cached version with no `known_version` counts
/// as ahead: this device has never synced the save, so whatever the cloud holds
/// is news. No cache entry means we simply don't know (self-hosted, headless
/// CLI, or the poller hasn't reported yet) — never claim ahead on a guess, the
/// callers use this to decide whether an update is *waiting*, and the pull
/// itself re-checks the real head anyway.
fn cloud_ahead(slot: &SaveSlot, latest_versions: &HashMap<String, i64>) -> bool {
    match latest_versions.get(&slot.save.save_id) {
        Some(latest) => slot.known_version.is_none_or(|known| *latest > known),
        None => false,
    }
}

/// Remember that a pull for this slot was vetoed mid-session so the
/// `GameStopped` transition can honour it once the folder goes quiet. Returns
/// `true` when the caller should emit [`AgentEvent::RestoreDeferred`] — once
/// per waiting update, not once per sweep tick.
fn note_deferred_pull(slot: &mut SaveSlot) -> bool {
    slot.pull_pending = true;
    let first = !slot.deferred_notified;
    slot.deferred_notified = true;
    first
}

/// May the pull deferred by a mid-session veto run now? Asked only from the
/// `GameStopped` transition — either straight away, or once the final flush's
/// `BackupDone` lands (`SaveSlot::pull_after_flush`).
///
/// This path drops **only** the recency guards of [`mid_session_reason`]
/// (`last_fs_event_at` and the disk-mtime fallback). Both are guaranteed to
/// trip here and both are describing the same write: the save the game made on
/// its way out, which the final flush has just uploaded. Waiting out their
/// 5-minute grace is what left the update stranded — nothing re-fires once it
/// expires except the sweep, and on a Deck the sweep is usually still vetoed by
/// a leftover Proton process. The restore is conflict-aware by design
/// (local-newer files win, conflicts are backed up), so the exiting write can't
/// be lost even if the grace was hiding a real race.
///
/// The live-session guards stay exactly as they were (REPO data-loss,
/// 2026-07-05): `is_running` means the user is playing *right now*,
/// `has_pending` means local changes aren't versioned yet, and `restoring`
/// means a pull is already writing into the folder. Any of them defers again —
/// `pull_pending` survives, so the next quiet moment gets another chance.
fn deferred_pull_ready(
    slot: &SaveSlot,
    config: &AgentConfig,
    latest_versions: &HashMap<String, i64>,
) -> bool {
    // Playtime-only entries have no save folder to pull into.
    if slot.save.track_only {
        return false;
    }
    // Nothing is waiting: no veto recorded one, and the poller's cache doesn't
    // show the cloud ahead of us either.
    if !slot.pull_pending && !cloud_ahead(slot, latest_versions) {
        return false;
    }
    // Per-save preset can opt out of restore (backup-only) or opt in when the
    // global default is off; `global_sync` counts as a global opt-in.
    if !slot
        .save
        .policy
        .auto_restore
        .unwrap_or(config.auto_restore || config.global_sync)
    {
        return false;
    }
    !(slot.is_running || slot.has_pending || slot.restoring)
}

/// Run the pull a mid-session veto deferred, if the slot is finally ready for
/// it. Called from the `GameStopped` transition and from the `BackupDone` of
/// its final flush; a no-op for every slot with nothing waiting.
fn run_deferred_pull(
    slots: &mut HashMap<String, SaveSlot>,
    id: &str,
    api: &ApiClient,
    events_tx: &mpsc::Sender<AgentEvent>,
    cmd_tx: &mpsc::Sender<AgentCommand>,
    config: &AgentConfig,
    latest_versions: &HashMap<String, i64>,
) {
    let Some(slot) = slots.get(id) else {
        return;
    };
    if !deferred_pull_ready(slot, config, latest_versions) {
        return;
    }
    let save = slot.save.clone();
    let known_version = slot.known_version;
    if let Some(slot) = slots.get_mut(id) {
        slot.pull_pending = false;
        slot.deferred_notified = false;
        slot.restoring = true;
        slot.next_auto_restore_at =
            Some(TokioInstant::now() + Duration::from_secs(AUTO_RESTORE_COOLDOWN_SECS));
    }
    tracing::info!(
        save_id = %id,
        "agent: game closed — running the pull that was deferred mid-session"
    );
    spawn_auto_restore(
        save,
        api.clone(),
        events_tx.clone(),
        cmd_tx.clone(),
        config.conflict_root.clone(),
        config.conflict_retention_days,
        known_version,
        // Authoritative, like the force/barrier paths: this is the user's
        // cross-device hand-off finally landing, so fetch the real head instead
        // of trusting a cache that may be a tick stale. The version-gate inside
        // still makes it free when we're already current.
        None,
        // Single save — no sweep batch to share a manifest with.
        None,
    );
}

/// True if `path` exists and has been modified within `grace`. Conservative
/// on errors: an unreadable path returns `false` so we don't deadlock the
/// auto-restore against a slot we can't stat.
fn is_path_recently_touched(path: &Path, grace: Duration) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    match std::time::SystemTime::now().duration_since(mtime) {
        Ok(age) => age < grace,
        Err(_) => false,
    }
}

/// The save folder's own mtime (its inode, not recursive), as an
/// `OffsetDateTime`, or `None` if it can't be stat'd. This is the sampled
/// [`kernel::Observation::folder_mtime`] the sans-IO session veto consumes:
/// same source as [`is_path_recently_touched`], but the recency comparison
/// now lives in the kernel against an injected `now`.
fn folder_own_mtime(path: &Path) -> Option<OffsetDateTime> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .map(OffsetDateTime::from)
}

/// Mayor mtime entre la propia carpeta y sus ficheros inmediatos (no
/// recursivo — barato y suficiente: un save que se escribe deja un fichero
/// nuevo/tocado en el primer nivel, p.ej. el `.zip` de Factorio en `saves/`).
/// `None` si la carpeta no se puede leer.
fn dir_max_mtime(dir: &Path) -> Option<std::time::SystemTime> {
    let mut max = std::fs::metadata(dir).ok().and_then(|m| m.modified().ok());
    if let Ok(read) = std::fs::read_dir(dir) {
        for entry in read.flatten() {
            if let Ok(m) = entry.metadata() {
                if let Ok(t) = m.modified() {
                    max = Some(match max {
                        Some(cur) if cur >= t => cur,
                        _ => t,
                    });
                }
            }
        }
    }
    max
}

/// Recorre las carpetas candidatas sondeadas, actualiza sus baselines de
/// mtime y devuelve aquellas reescritas desde el último tick. El baseline
/// `None` (primer avistamiento) sólo se siembra, sin reportar — evita
/// atribuir un fichero pre-existente reciente a una escritura no presenciada.
/// Pura (sin I/O de procesos ni persistencia) para poder testearla.
fn probe_detect_writes(
    probes: &mut HashMap<PathBuf, Option<std::time::SystemTime>>,
) -> Vec<PathBuf> {
    let mut written = Vec::new();
    for (dir, baseline) in probes.iter_mut() {
        let Some(current) = dir_max_mtime(dir) else {
            continue;
        };
        let is_write = matches!(*baseline, Some(prev) if current > prev);
        *baseline = Some(current);
        if is_write {
            written.push(dir.clone());
        }
    }
    written
}

/// DETECCIÓN (fase 3, ADR 0020): sondea los candidatos y, para los reescritos
/// desde el último tick, si hay un juego vivo registra la correlación
/// proceso↔escritura y persiste el store. Es lo que rompe el huevo-y-gallina:
/// jugar un juego no rastreado deja por fin el rastro +0.50 que el siguiente
/// escaneo necesita para ascenderlo a `High`.
fn probe_candidates(
    probes: &mut HashMap<PathBuf, Option<std::time::SystemTime>>,
    sys: &System,
    corr_store: &mut crate::correlation::CorrelationStore,
    corr_path: Option<&Path>,
) {
    let written = probe_detect_writes(probes);
    if written.is_empty() {
        return;
    }
    // Sólo muestreamos procesos cuando de verdad hubo una escritura (perezoso).
    let games = crate::correlation::sample_game_processes(sys);
    if games.is_empty() {
        return;
    }
    for dir in &written {
        tracing::info!(
            dir = %dir.display(),
            process = %games[0].name,
            "agent: probe write correlated to live game; recording"
        );
        corr_store.record(dir, &games);
    }
    if let Some(p) = corr_path {
        if let Err(e) = corr_store.save(p) {
            tracing::debug!(error = %e, "agent: failed to persist correlation store (probe)");
        }
    }
}

/// Internal restore primitive returning the outcome summary or `None` if
/// the server has no snapshots for this save (in which case auto-restore
/// is a no-op, not a failure).
struct AutoRestoreOutcome {
    version_num: i64,
    /// Files copied from the remote snapshot into the local folder (those
    /// that were missing locally). Bytes equal between staging and local
    /// don't count.
    files_restored: u64,
    /// Files where the local copy was preserved because its mtime was
    /// newer than the remote (or `conflict_root` was unset — see
    /// `restore_files_into` for the fallback path).
    conflicts_local_wins: u64,
    /// Files where the local copy was moved into the conflict backup dir
    /// before being overwritten by the remote version (ADR 0014).
    conflicts_backed_up: u64,
    /// Where the local versions were parked, if any. `None` when
    /// `conflicts_backed_up == 0`.
    conflict_dir: Option<PathBuf>,
    /// Total bytes copied. Sum of `restored` + `conflicts_resolved_remote`
    /// file sizes.
    bytes_extracted: u64,
    /// True when the merged local tree is strictly ahead of the head we pulled:
    /// some local file was newer (kept on mtime) or local-only. The conflict
    /// reconcile path uses this to decide whether the follow-up upload carries
    /// real data (`true` → push it, fast-forwarding from the new head) or would
    /// just mint a redundant copy of head (`false` → settle without uploading).
    local_diverged: bool,
    /// Cheap set signature (`"<paths+sizes+mtimes>:"`, content half empty) of
    /// the local folder *after* the merge, in the exact format
    /// `upload_directory_checked` compares against. When the tree matches head
    /// (`!local_diverged`) the caller stores this as the slot's
    /// `last_set_hash`, so the fs events the merge itself triggered settle as a
    /// no-op `Skipped` instead of firing a redundant upload. `None` if the
    /// post-merge walk failed (best-effort; we just skip the optimisation).
    disk_set_hash: Option<String>,
}

/// Per-file outcome accounting for diff-based restore. Returned by
/// `restore_files_into` and embedded into `AutoRestoreOutcome`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RestoreStats {
    /// Files copied from `source` into `target` because they were missing
    /// from `target`.
    pub restored: usize,
    /// Files present in both `source` and `target` with identical bytes.
    /// Left as-is.
    pub skipped: usize,
    /// Files where bytes differ and the *remote* won by mtime, so we
    /// overwrote the local copy with the staged remote version.
    pub conflicts_resolved_remote: usize,
    /// Files where bytes differ and the *local* won by mtime, so we left
    /// the local copy alone. Also incremented as a safety fallback when
    /// `conflict_backup_dir` is `None` and the remote would have won.
    pub conflicts_resolved_local: usize,
    /// Files where the local copy was moved into the conflict backup dir
    /// before being replaced by the remote version (subset of
    /// `conflicts_resolved_remote`).
    pub conflicts_backed_up: usize,
    /// Total bytes copied across `restored` + `conflicts_resolved_remote`.
    /// Useful for the `SaveAutoRestored` event payload.
    pub bytes_restored: u64,
    /// Files present locally (`target`) but absent from the remote snapshot
    /// (`source`) — local-only content the merge left untouched. Together with
    /// `conflicts_resolved_local` this tells the caller whether the merged tree
    /// genuinely diverges from the head (a follow-up upload carries real data)
    /// or matches it exactly (re-uploading would only mint a redundant no-op
    /// version). Counted with the same recursive walk as the restore itself.
    pub target_only: usize,
}

async fn run_auto_restore(
    api: &ApiClient,
    save: &WatchedSave,
    conflict_root: Option<&Path>,
    retention: Duration,
    known_version: Option<i64>,
    cached_latest: Option<i64>,
    shared_manifest: Option<Arc<tokio::sync::OnceCell<HashMap<String, i64>>>>,
) -> Result<Option<AutoRestoreOutcome>> {
    // Prefer the version the cloud_pull poller already learned this tick: it
    // fetched the whole manifest once, so reusing it spares us a per-save
    // `cloud_sync`/`get_save` round-trip (the old sweep N+1). When the poller
    // cache is cold (cold start), the sweep's `shared_manifest` cell fills
    // that role: one fetch for the whole batch of restores. Only the
    // authoritative force-restore / pre-launch barrier paths (which pass
    // neither) and self-hosted / headless one-offs hit the network per save.
    let latest = match cached_latest {
        Some(v) => Some(v),
        None => {
            if api.is_cloud().await {
                match &shared_manifest {
                    Some(cell) => cell
                        .get_or_try_init(|| async {
                            Ok::<_, anyhow::Error>(
                                api.cloud_sync()
                                    .await?
                                    .saves
                                    .into_iter()
                                    .map(|e| (e.save_id, e.latest_version_num))
                                    .collect::<HashMap<String, i64>>(),
                            )
                        })
                        .await?
                        .get(&save.save_id)
                        .copied(),
                    None => api
                        .cloud_sync()
                        .await?
                        .saves
                        .into_iter()
                        .find(|e| e.save_id == save.save_id)
                        .map(|e| e.latest_version_num),
                }
            } else {
                api.get_save(&save.save_id).await?.latest_version_num
            }
        }
    };
    // Version gate: if we're already synced to the server's latest version,
    // there's nothing newer from another device to pull, so skip the expensive
    // download-to-diff entirely. This is the fix for the bandwidth blowout —
    // the sweep used to re-download the full snapshot every ~50s just to diff
    // it against a folder that hadn't changed, exhausting the 15-min cloud
    // quota (429 storm) and starving real uploads. A genuine cross-device
    // update bumps the server version above `known_version` and still pulls.
    // A locally empty/missing folder is the one case where "already on the
    // latest version" is a lie worth pulling for: the user wiped the save
    // (manual cleanup, uninstall, deleted folder) and the cloud copy is the
    // only one left. Restoring it is exactly what they want, so don't let the
    // version gate short-circuit an empty folder — fall through to the download
    // even when `known >= v`.
    if let (Some(v), Some(known)) = (latest, known_version) {
        if known >= v && !is_path_empty_or_missing(&save.local_path) {
            tracing::debug!(
                save_id = %save.save_id,
                version = v,
                "agent: auto-restore — already synced to latest version; skipping download"
            );
            if let Some(root) = conflict_root {
                if let Err(e) = cleanup_old_conflicts(root, retention).await {
                    tracing::debug!(error = %e, "cleanup_old_conflicts failed (up-to-date path)");
                }
            }
            return Ok(None);
        }
    }
    let Some(version) = latest else {
        tracing::debug!(
            save_id = %save.save_id,
            "agent: auto-restore — server has no snapshots yet; nothing to restore"
        );
        // Still sweep TTL before bailing — keeps the conflict dir bounded
        // even for saves whose remote has been purged.
        if let Some(root) = conflict_root {
            if let Err(e) = cleanup_old_conflicts(root, retention).await {
                tracing::debug!(error = %e, "cleanup_old_conflicts failed (no-snapshot path)");
            }
        }
        return Ok(None);
    };
    // Stage the snapshot in a unique temp dir so we never overwrite the
    // user's local files during extraction. The staging dir is empty by
    // construction, so `download_snapshot` extracts into it cleanly even
    // with `force=false`. Cleanup happens in `cleanup_staging` at the end.
    let staging = staging_dir_for(&save.save_id);
    tokio::fs::create_dir_all(&staging)
        .await
        .with_context(|| format!("creating staging dir {}", staging.display()))?;

    let download_result = crate::restore::download_snapshot(
        api,
        &save.save_id,
        version,
        &staging,
        crate::restore::RestoreOptions {
            skip_verify: false,
            force: false,
        },
        |_, _| {},
    )
    .await;

    let outcome = match download_result {
        Ok(o) => o,
        Err(e) => {
            cleanup_staging(&staging).await;
            return Err(e);
        }
    };
    let _ = outcome; // we walk the staging dir directly for the diff

    // Per-attempt timestamped subdir so concurrent restores never collide
    // and the TTL sweep can drop the whole subtree in one shot. We compute
    // it lazily *only if* a conflict_root is configured — `restore_files_into`
    // treats `None` as the safe legacy fallback.
    let conflict_backup_dir: Option<PathBuf> = conflict_root.map(|root| {
        let ts = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "unknown-ts".to_string())
            // Colons aren't legal in Windows paths and look weird everywhere.
            .replace(':', "-");
        root.join(&save.save_id).join(ts)
    });

    let copy_result =
        restore_files_into(&save.local_path, &staging, conflict_backup_dir.as_deref()).await;
    cleanup_staging(&staging).await;

    // Best-effort TTL sweep regardless of the per-file outcome — we want
    // bounded disk usage even when the current restore had no conflicts.
    if let Some(root) = conflict_root {
        if let Err(e) = cleanup_old_conflicts(root, retention).await {
            tracing::debug!(error = %e, "cleanup_old_conflicts failed");
        }
    }

    let stats = copy_result?;
    let dir_used = if stats.conflicts_backed_up > 0 {
        conflict_backup_dir
    } else {
        None
    };

    // Did anything local survive the merge that head doesn't have? A newer
    // local file (kept on mtime) or a local-only file means the merged tree is
    // ahead of head; otherwise the tree now equals head exactly.
    let local_diverged = stats.conflicts_resolved_local > 0 || stats.target_only > 0;
    // Cheap (no byte reads) signature of the merged folder, in the composite
    // `"<cheap>:"` shape `upload_directory_checked` splits on — the empty
    // content half is fine because the fast-path skip only compares the cheap
    // half. Best-effort: a walk error just drops the redundant-upload
    // optimisation, never blocks the restore.
    let disk_set_hash = crate::backup::walk_source(&save.local_path)
        .ok()
        .map(|files| format!("{}:", crate::backup::compute_set_signature(&files)));

    Ok(Some(AutoRestoreOutcome {
        version_num: version,
        files_restored: stats.restored as u64,
        conflicts_local_wins: stats.conflicts_resolved_local as u64,
        conflicts_backed_up: stats.conflicts_backed_up as u64,
        conflict_dir: dir_used,
        bytes_extracted: stats.bytes_restored,
        local_diverged,
        disk_set_hash,
    }))
}

/// Build a unique staging directory under the system temp dir. We embed
/// the save_id (sanitised to alphanumeric+dash) and a monotonic nanosecond
/// counter so concurrent restores for the same save never collide.
fn staging_dir_for(save_id: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let safe_id: String = save_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    std::env::temp_dir().join(format!(
        "hoard-restore-{safe_id}-{n}-{}",
        std::process::id()
    ))
}

/// Best-effort tempdir cleanup. We log but never propagate the error: a
/// leaked staging dir is annoying but not user-visible, and the OS will
/// reap `/tmp` on reboot anyway.
async fn cleanup_staging(staging: &Path) {
    if let Err(e) = tokio::fs::remove_dir_all(staging).await {
        tracing::debug!(
            staging = %staging.display(),
            error = %e,
            "agent: failed to clean up restore staging dir"
        );
    }
}

/// Walk `conflict_root` two levels deep (`<save_id>/<timestamp>/`) and
/// remove every timestamp dir whose mtime is older than `now - retention`.
/// No-op when the root doesn't exist (typical fresh install). Errors are
/// logged but never propagated — a stuck conflict dir is much better than
/// killing the auto-restore tick.
pub(crate) async fn cleanup_old_conflicts(conflict_root: &Path, retention: Duration) -> Result<()> {
    if !conflict_root.exists() {
        return Ok(());
    }
    let cutoff = std::time::SystemTime::now()
        .checked_sub(retention)
        .unwrap_or(std::time::UNIX_EPOCH);
    let mut save_entries = tokio::fs::read_dir(conflict_root)
        .await
        .with_context(|| format!("reading conflict root {}", conflict_root.display()))?;
    while let Some(save_entry) = save_entries.next_entry().await? {
        if !save_entry.file_type().await?.is_dir() {
            continue;
        }
        let save_dir = save_entry.path();
        let mut ts_entries = match tokio::fs::read_dir(&save_dir).await {
            Ok(it) => it,
            Err(e) => {
                tracing::debug!(
                    dir = %save_dir.display(),
                    error = %e,
                    "agent: skipping unreadable conflict save dir"
                );
                continue;
            }
        };
        while let Some(ts_entry) = ts_entries.next_entry().await? {
            if !ts_entry.file_type().await?.is_dir() {
                continue;
            }
            let ts_dir = ts_entry.path();
            let mtime = match ts_entry.metadata().await.and_then(|m| m.modified()) {
                Ok(t) => t,
                Err(e) => {
                    tracing::debug!(
                        dir = %ts_dir.display(),
                        error = %e,
                        "agent: couldn't read conflict ts mtime; leaving it alone"
                    );
                    continue;
                }
            };
            if mtime < cutoff {
                match tokio::fs::remove_dir_all(&ts_dir).await {
                    Ok(()) => tracing::info!(
                        dir = %ts_dir.display(),
                        "agent: removed expired conflict backup"
                    ),
                    Err(e) => tracing::warn!(
                        dir = %ts_dir.display(),
                        error = %e,
                        "agent: failed to remove expired conflict backup"
                    ),
                }
            }
        }
    }
    Ok(())
}

/// Copy files from `source` into `target` non-destructively, resolving
/// per-file conflicts via mtime (ADR 0014).
///
/// Walks `source` recursively. For each file:
///
/// - `target/rel` missing → copy; bump `restored`.
/// - `target/rel` exists with identical bytes → skip; bump `skipped`.
/// - `target/rel` exists with different bytes:
///   - `local_mtime > remote_mtime + 1s` → local wins, untouched; bump
///     `conflicts_resolved_local`.
///   - Otherwise (remote newer, or within ±1s tolerance) → remote wins.
///     If `conflict_backup_dir` is `Some(dir)`, move `target/rel` to
///     `dir/rel` (creating parents) and bump `conflicts_backed_up`, then
///     copy `source/rel` over and bump `conflicts_resolved_remote`. If
///     `conflict_backup_dir` is `None`, *do not* overwrite — bump
///     `conflicts_resolved_local` as a safety fallback (legacy 1.5.4
///     behaviour) and log a warn.
///
/// Errors propagate only for I/O failures we can't classify (e.g.
/// permission denied reading a file we just listed).
pub(crate) async fn restore_files_into(
    target: &Path,
    source: &Path,
    conflict_backup_dir: Option<&Path>,
) -> Result<RestoreStats> {
    let mut stats = RestoreStats::default();
    let mut stack: Vec<PathBuf> = vec![source.to_path_buf()];
    // Relative paths seen in the remote snapshot. Used after the merge to spot
    // local-only files (in `target`, not in `source`) → `stats.target_only`.
    let mut source_rels: HashSet<PathBuf> = HashSet::new();

    while let Some(dir) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&dir)
            .await
            .with_context(|| format!("reading staging dir {}", dir.display()))?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let file_type = entry.file_type().await?;
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                // Skip symlinks, devices etc — they shouldn't appear in
                // a hoard snapshot but we'd rather no-op than crash.
                continue;
            }
            let rel = path
                .strip_prefix(source)
                .with_context(|| format!("path {} not under source", path.display()))?;
            source_rels.insert(rel.to_path_buf());
            let dest = target.join(rel);
            if dest.exists() {
                if files_have_equal_bytes(&path, &dest).await? {
                    stats.skipped += 1;
                    continue;
                }
                // Bytes differ — the resolution policy is the kernel's; this
                // shell samples the mtime winner and executes the chosen
                // branch. 1s tolerance covers FAT32 and friends; remote ties
                // take the local side so a close call doesn't trash data.
                let local_wins = local_mtime_wins(&dest, &path).await;
                let backup_root = match kernel::restore_merge::resolve_conflict(
                    local_wins,
                    conflict_backup_dir.is_some(),
                ) {
                    kernel::restore_merge::ConflictResolution::KeepLocal => {
                        if local_wins {
                            tracing::debug!(
                                rel = %rel.display(),
                                "auto-restore diff: local wins on mtime"
                            );
                        } else {
                            // Remote looked newer but there's no
                            // conflict_backup_dir (legacy fallback): never
                            // destroy local data.
                            tracing::warn!(
                                rel = %rel.display(),
                                "auto-restore diff: remote appears newer but no conflict_backup_dir; keeping local"
                            );
                        }
                        stats.conflicts_resolved_local += 1;
                        continue;
                    }
                    kernel::restore_merge::ConflictResolution::BackupThenTakeRemote => {
                        conflict_backup_dir
                            .expect("BackupThenTakeRemote is only chosen when a backup dir exists")
                    }
                };
                let backup_dest = backup_root.join(rel);
                if let Some(parent) = backup_dest.parent() {
                    tokio::fs::create_dir_all(parent).await.with_context(|| {
                        format!("creating conflict backup parent dir {}", parent.display())
                    })?;
                }
                // `rename` first (cheap, atomic). Fall back to copy+remove
                // when the conflict root is on a different filesystem
                // (typical when state_dir lives on the system disk and the
                // save folder is on a different volume).
                if let Err(e) = tokio::fs::rename(&dest, &backup_dest).await {
                    tracing::debug!(
                        rel = %rel.display(),
                        error = %e,
                        "auto-restore diff: rename across filesystems failed, falling back to copy"
                    );
                    tokio::fs::copy(&dest, &backup_dest)
                        .await
                        .with_context(|| {
                            format!(
                                "copying {} → {} for conflict backup",
                                dest.display(),
                                backup_dest.display()
                            )
                        })?;
                    tokio::fs::remove_file(&dest).await.with_context(|| {
                        format!("removing local {} after conflict backup", dest.display())
                    })?;
                }
                stats.conflicts_backed_up += 1;
                let copied = tokio::fs::copy(&path, &dest)
                    .await
                    .with_context(|| format!("copying {} → {}", path.display(), dest.display()))?;
                preserve_staging_mtime(&path, &dest).await;
                stats.conflicts_resolved_remote += 1;
                stats.bytes_restored += copied;
                continue;
            }
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await.with_context(|| {
                    format!("creating parent dir {} for restore", parent.display())
                })?;
            }
            let copied = tokio::fs::copy(&path, &dest)
                .await
                .with_context(|| format!("copying {} → {}", path.display(), dest.display()))?;
            preserve_staging_mtime(&path, &dest).await;
            stats.restored += 1;
            stats.bytes_restored += copied;
        }
    }

    // Second pass over `target`: count files the snapshot didn't carry. These
    // are local-only and survive the merge, so the merged tree is strictly
    // ahead of head and a follow-up upload is real, not redundant. We don't
    // filter transient lock files here — a stray lock counting as divergence
    // only costs one extra upload (the safe direction), never a skipped one.
    let mut tstack: Vec<PathBuf> = vec![target.to_path_buf()];
    while let Some(dir) = tstack.pop() {
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            // The folder can be empty or vanish mid-walk; treat as no extras.
            Err(_) => continue,
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let file_type = entry.file_type().await?;
            if file_type.is_dir() {
                tstack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Ok(rel) = path.strip_prefix(target) else {
                continue;
            };
            if !source_rels.contains(rel) {
                stats.target_only += 1;
            }
        }
    }

    Ok(stats)
}

/// Re-stamp `dest` with `src`'s mtime after a copy. `fs::copy` writes the
/// destination with mtime=now, but the staging tree carries the snapshot's
/// original mtimes (restore.rs re-applies them on extraction) and they must
/// survive into the live folder: a game that picks "continue" by
/// most-recent file would otherwise see every restored save as brand-new
/// and load the wrong one, and the follow-up merged-tree upload would
/// record the inflated mtimes server-side, poisoning future merges on
/// other devices. Best-effort: a failure only degrades ordering, never data.
async fn preserve_staging_mtime(src: &Path, dest: &Path) {
    let mtime = match tokio::fs::metadata(src).await.and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                src = %src.display(),
                error = %e,
                "restore: couldn't read staging mtime; destination keeps mtime=now"
            );
            return;
        }
    };
    if let Err(e) = filetime::set_file_mtime(dest, filetime::FileTime::from_system_time(mtime)) {
        tracing::warn!(
            dest = %dest.display(),
            error = %e,
            "restore: couldn't re-apply snapshot mtime; destination keeps mtime=now"
        );
    }
}

/// True when the local file's mtime is more than 1s newer than the remote
/// file's. Conservative on errors: if we can't read either mtime, we treat
/// the remote as the winner — the snapshot's authority comes from the
/// server's committed timestamps, which are more reliable than a local
/// filesystem with quirks (FAT32 2s rounding, network share clock skew).
async fn local_mtime_wins(local: &Path, remote: &Path) -> bool {
    // Sans-IO boundary: this shell samples both mtimes; the kernel decides.
    // An unreadable file → `None` → the kernel hands the tie to the remote,
    // exactly as the old early-return `false` did.
    let local_mtime = tokio::fs::metadata(local).await.and_then(|m| m.modified()).ok();
    let remote_mtime = tokio::fs::metadata(remote).await.and_then(|m| m.modified()).ok();
    kernel::restore_merge::local_wins_on_mtime(local_mtime, remote_mtime)
}

/// Cheap bytes-equal: size first (saves the read for the common
/// different-sized case), then a single shot read of each file and a
/// linear compare. Files in tracked saves are small enough that
/// chunk-streaming would only matter for pathological archives — the
/// per-file alloc cost is much smaller than the network/zstd cost we
/// already paid to land them in staging.
async fn files_have_equal_bytes(a: &Path, b: &Path) -> Result<bool> {
    let meta_a = tokio::fs::metadata(a).await?;
    let meta_b = tokio::fs::metadata(b).await?;
    if meta_a.len() != meta_b.len() {
        return Ok(false);
    }
    let bytes_a = tokio::fs::read(a).await?;
    let bytes_b = tokio::fs::read(b).await?;
    Ok(bytes_a == bytes_b)
}

/// Try to attach an fs debouncer to `slot`. Tolerant: a missing folder or
/// an inotify error logs and leaves `slot.watcher == None` so the agent
/// keeps running for the other slots. Re-arming later is fine — we just
/// overwrite the field.
fn arm_watcher(slot: &mut SaveSlot, fs_tx: &mpsc::Sender<PathBuf>) {
    let path = slot.save.local_path.clone();
    if !path.is_dir() {
        tracing::info!(
            save_id = %slot.save.save_id,
            path = %path.display(),
            "agent: save path missing on add; fs watcher not armed"
        );
        slot.watcher = None;
        return;
    }
    match build_watcher(&path, fs_tx.clone()) {
        Ok(w) => {
            tracing::info!(
                save_id = %slot.save.save_id,
                path = %path.display(),
                "agent: fs watcher armed"
            );
            slot.watcher = Some(w);
        }
        Err(e) => {
            tracing::warn!(
                save_id = %slot.save.save_id,
                path = %path.display(),
                error = %e,
                "agent: couldn't arm fs watcher"
            );
            slot.watcher = None;
        }
    }
}

fn build_watcher(
    path: &Path,
    fs_tx: mpsc::Sender<PathBuf>,
) -> Result<Debouncer<notify::RecommendedWatcher>> {
    let watch_root = path.to_path_buf();
    let mut debouncer = new_debouncer(
        // Internal aggregation window for notify-debouncer-mini. We use a
        // small value (2 s) here and apply our larger product debounce by
        // resetting the schedule timer on each event. That way we still see
        // bursts as a single "settled" signal upstream.
        Duration::from_secs(2),
        move |res: DebounceEventResult| {
            if let Ok(events) = res {
                if !events.is_empty() {
                    let _ = fs_tx.try_send(watch_root.clone());
                }
            }
        },
    )?;
    debouncer
        .watcher()
        .watch(path, notify::RecursiveMode::Recursive)?;
    Ok(debouncer)
}

/// Find which save a path event belongs to. The fs watcher emits the root
/// it was registered for, so this is a direct lookup by canonical prefix.
fn match_save_for_path(slots: &HashMap<String, SaveSlot>, path: &Path) -> Option<String> {
    for slot in slots.values() {
        // Playtime-only slots own no folder; their sentinel path is empty,
        // which `starts_with` would treat as a prefix of *every* path.
        if slot.save.track_only {
            continue;
        }
        if slot.save.local_path == path || path.starts_with(&slot.save.local_path) {
            return Some(slot.save.save_id.clone());
        }
    }
    None
}

/// Cancel any in-flight pending backup, then schedule a new one to run
/// after `delay`. The pending task does the wait *and* the upload, so we
/// can abort the wait cleanly when a new event resets the timer.
#[allow(clippy::too_many_arguments)]
fn schedule_backup(
    slots: &mut HashMap<String, SaveSlot>,
    save_id: &str,
    reason: BackupReason,
    delay: Duration,
    api: &ApiClient,
    events_tx: &mpsc::Sender<AgentEvent>,
    config: &AgentConfig,
    done_tx: &mpsc::Sender<BackupDone>,
    cmd_tx: &mpsc::Sender<AgentCommand>,
) {
    let Some(slot) = slots.get_mut(save_id) else {
        return;
    };
    // Was a backup already scheduled for this slot? If so, this call is
    // just resetting the debounce timer inside an in-progress window — the
    // feed already shows a "queued" row for it. Re-announcing on every fs
    // event is what flooded the activity feed with orphaned "en cola"
    // entries when a game autosaves every second. Only announce on the
    // leading edge; the row resolves when the upload completes (which
    // clears `next_scheduled_backup_at` via `done_rx`).
    let already_scheduled = slot.next_scheduled_backup_at.is_some();
    if let Some(p) = slot.pending.take() {
        p.abort();
    }

    slot.next_scheduled_backup_at = Some(OffsetDateTime::now_utc() + delay);

    tracing::info!(
        save_id = %save_id,
        delay_ms = delay.as_millis() as u64,
        reason = ?reason,
        "agent: backup scheduled"
    );

    // Don't announce zero-delay backups (manual / forced flush) — they'd
    // add noise — nor re-announce a window that's already queued, nor the
    // staggered sweep entries (there's no user-visible trigger and one row
    // per save every hour would flood the feed; the resulting upload still
    // announces normally when it runs).
    if delay > Duration::ZERO
        && !already_scheduled
        && !matches!(reason, BackupReason::SweepStaggered)
    {
        let _ = events_tx.try_send(AgentEvent::BackupScheduled {
            save_id: save_id.to_string(),
            delay_ms: delay.as_millis() as u64,
            reason,
        });
    }

    let api = api.clone();
    let events_tx = events_tx.clone();
    let done_tx = done_tx.clone();
    let cmd_tx = cmd_tx.clone();
    let save = slot.save.clone();
    let prev_set_hash = slot.last_set_hash.clone();
    // Fast-forward base for the upload: the version this device believes is the
    // server head. Sending it lets the server reject (409 non-fast-forward) when
    // another device advanced the save since we last synced, instead of burying
    // their version under our stale content. Captured at schedule time; if it's
    // gone stale by the time the debounced upload runs, the 409 path reconciles
    // and retries with the fresh head. `None` only for a brand-new save the
    // device has never synced (no head to fast-forward against yet).
    let base_version = slot.known_version;
    let max_retries = config.max_retries;
    // Per-save preset can force backup-only (`Some(false)`) or force restore
    // (`Some(true)`) regardless of the global default.
    let auto_restore = slot
        .save
        .policy
        .auto_restore
        .unwrap_or(config.auto_restore || config.global_sync);
    let conflict_root = config.conflict_root.clone();
    let conflict_retention_days = config.conflict_retention_days;

    slot.pending = Some(tokio::spawn(async move {
        if delay > Duration::ZERO {
            tokio::time::sleep(delay).await;
        }
        run_backup_with_retry(
            api,
            save,
            prev_set_hash,
            base_version,
            events_tx,
            done_tx,
            cmd_tx,
            max_retries,
            auto_restore,
            conflict_root,
            conflict_retention_days,
        )
        .await;
    }));
}

/// Nominal hash-throughput budget for the staggered sweep: how many bytes of
/// save data each second of the *effective* window covers. Calibrated so
/// ~20 GiB of saves stretches the window to ~2h (≈6 min per GiB), keeping
/// sustained disk reads thin. Below this footprint the configured interval
/// dominates and the window stays at its nominal length.
const SWEEP_BYTES_PER_WINDOW_SEC: f64 = 20.0 * 1024.0 * 1024.0 * 1024.0 / 7200.0;

/// Floor on the gap between consecutive saves in a staggered sweep, so even a
/// pile of tiny saves gets a visible beat between each re-hash instead of
/// firing back-to-back.
const SWEEP_MIN_GAP_SECS: f64 = 15.0;

/// Staggered backup sweep (see `AgentCommand::SweepAll`). Walks each tracked
/// save's folder for its byte footprint (metadata only — no file contents are
/// read here), then schedules a re-hash for each at a size-proportional offset
/// inside an effective window. The window is
/// `max(window_secs, total / SWEEP_BYTES_PER_WINDOW_SEC)`, so a small set
/// finishes within the nominal interval while tens of GB stretch it out. Saves
/// already queued for backup (live fs event, or a still-running previous
/// sweep) are left alone so repeated ticks don't reset the stagger or pile up
/// concurrent hashes.
#[allow(clippy::too_many_arguments)]
fn sweep_all(
    slots: &mut HashMap<String, SaveSlot>,
    window_secs: u64,
    api: &ApiClient,
    events_tx: &mpsc::Sender<AgentEvent>,
    config: &AgentConfig,
    done_tx: &mpsc::Sender<BackupDone>,
    cmd_tx: &mpsc::Sender<AgentCommand>,
) {
    // Snapshot (id, path, already-queued) up front: scheduling borrows `slots`
    // mutably, so we can't hold an iterator over it while calling
    // `schedule_backup` below.
    let entries: Vec<(String, PathBuf, bool)> = slots
        .values()
        // Playtime-only entries never back up.
        .filter(|s| !s.save.track_only)
        .map(|s| {
            (
                s.save.save_id.clone(),
                s.save.local_path.clone(),
                s.next_scheduled_backup_at.is_some(),
            )
        })
        .collect();
    if entries.is_empty() {
        return;
    }

    // Byte footprint per save (metadata walk). Missing/unreadable folders
    // count as zero — they're handled (or skipped-empty) when their turn to
    // back up comes.
    let sized: Vec<(String, bool, u64)> = entries
        .into_iter()
        .map(|(id, path, queued)| (id, queued, dir_size_bytes(&path)))
        .collect();
    let total_bytes: u64 = sized.iter().map(|(_, _, b)| *b).sum();
    let n = sized.len() as f64;

    // Effective window: grows past the nominal interval once the footprint is
    // large enough that spreading it thin needs more time.
    let window = window_secs.max(1) as f64;
    let effective_window = if total_bytes > 0 {
        window.max(total_bytes as f64 / SWEEP_BYTES_PER_WINDOW_SEC)
    } else {
        window
    };

    tracing::info!(
        saves = sized.len(),
        total_mib = (total_bytes / (1024 * 1024)),
        window_secs,
        effective_window_secs = effective_window as u64,
        "agent: starting staggered backup sweep"
    );

    let mut offset = 0.0_f64;
    for (id, already_queued, bytes) in sized {
        // Per-save slice of the window: size-proportional when we have a
        // total, an even split otherwise, floored so tiny saves still space
        // out.
        let slice = if total_bytes > 0 {
            (effective_window * (bytes as f64 / total_bytes as f64)).max(SWEEP_MIN_GAP_SECS)
        } else {
            (effective_window / n).max(SWEEP_MIN_GAP_SECS)
        };
        // Skip saves already on the schedule (live fs change, or a previous
        // sweep that hasn't run yet): don't reset their timer. We still
        // advance `offset` by their slice so the remaining saves keep their
        // size-proportional spacing — and so a long sweep that overruns into
        // the next tick finishes instead of restarting.
        if !already_queued {
            schedule_backup(
                slots,
                &id,
                BackupReason::SweepStaggered,
                Duration::from_secs_f64(offset),
                api,
                events_tx,
                config,
                done_tx,
                cmd_tx,
            );
        }
        offset += slice;
    }
}

/// Sum the byte size of every regular file under `root`, recursively. Reads
/// directory entries + file metadata only — never opens a file — so it's the
/// cheap way to learn a save's footprint for sweep staggering. Unreadable
/// dirs/entries are skipped rather than erroring; a best-effort estimate is
/// all the scheduler needs.
pub fn dir_size_bytes(root: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                if let Ok(meta) = entry.metadata() {
                    total = total.saturating_add(meta.len());
                }
            }
            // symlinks ignored, mirroring walk_source.
        }
    }
    total
}

/// Upload + retry. Backoff is `2 ** attempt` seconds, capped at 5 min.
/// `max_retries == 0` means "try once and give up on failure".
///
/// Since 1.4.3 there's a pre-check: if the local folder is missing or empty
/// at upload time, we never push an empty snapshot. The user can wipe a
/// save folder for any number of reasons (uninstall, manual cleanup,
/// crashed mod) and shipping an empty backup would silently overwrite the
/// last good copy on the server with nothing. Instead:
///
/// - `auto_restore = true`  → spawn a restore task to repopulate the
///   folder from the latest server snapshot and emit `SaveAutoRestored`.
/// - `auto_restore = false` → emit `BackupSkippedEmpty` and bail. The UI
///   surfaces a toast pointing the user at the Settings toggle.
#[allow(clippy::too_many_arguments)]
async fn run_backup_with_retry(
    api: ApiClient,
    save: WatchedSave,
    prev_set_hash: Option<String>,
    // The version this device believes is the server head. Sent as the upload's
    // fast-forward base so the server rejects (409 non-fast-forward) when another
    // device advanced the save since we last synced — see the `ApiError::Conflict`
    // arm below, which reconciles and retries instead of burying their version.
    // `None` only for a save never synced from this device (no head yet) and the
    // empty/missing-folder restore path, which never uploads.
    mut base_version: Option<i64>,
    events_tx: mpsc::Sender<AgentEvent>,
    done_tx: mpsc::Sender<BackupDone>,
    cmd_tx: mpsc::Sender<AgentCommand>,
    max_retries: u32,
    auto_restore: bool,
    conflict_root: Option<PathBuf>,
    conflict_retention_days: u32,
) {
    if is_path_empty_or_missing(&save.local_path) {
        tracing::info!(
            save_id = %save.save_id,
            path = %save.local_path.display(),
            auto_restore,
            "agent: backup skipped — local folder is empty/missing"
        );
        // Always clear has_pending so a future fs event isn't blocked.
        let _ = done_tx.try_send(BackupDone {
            save_id: save.save_id.clone(),
            new_set_hash: None,
            committed: false,
            version_num: None,
        });
        if auto_restore {
            spawn_auto_restore(
                save.clone(),
                api.clone(),
                events_tx.clone(),
                cmd_tx,
                conflict_root,
                conflict_retention_days,
                // Empty/missing folder: we genuinely want the save back, so
                // don't version-gate this pull.
                None,
                None,
                // Single save — no sweep batch to share a manifest with.
                None,
            );
        } else {
            let _ = events_tx
                .send(AgentEvent::BackupSkippedEmpty {
                    save_id: save.save_id.clone(),
                    game_slug: save.game_slug.clone(),
                })
                .await;
        }
        return;
    }
    let mut attempt = 0u32;
    // Bandwidth-limit (429) waits are tracked separately from real-failure
    // retries: a throttle isn't a failure, so it shouldn't eat the small
    // exponential-backoff budget meant for flaky network. We honour the
    // server's `retry_after_secs` and cap how many times we'll sit out a
    // window so a user genuinely parked over quota eventually surfaces.
    let mut throttle_waits = 0u32;
    const MAX_THROTTLE_WAITS: u32 = 5;
    // Fast-forward conflicts (409) are reconciled-then-retried, not backed off.
    // Cap the reconcile loop so a head that keeps advancing under us (a very
    // chatty sibling device) can't spin forever — after this many we surface
    // the conflict as a failure and let the next scheduled backup try fresh.
    let mut conflict_reconciles = 0u32;
    const MAX_CONFLICT_RECONCILES: u32 = 3;
    loop {
        let outcome = upload_directory_checked(
            &api,
            &save.save_id,
            &save.game_slug,
            &save.label,
            &save.local_path,
            prev_set_hash.as_deref(),
            // Fast-forward base: the version this device last synced. The server
            // rejects with 409 (non-fast-forward) if the head moved past it,
            // which the `ApiError::Conflict` arm below catches to reconcile +
            // retry instead of clobbering the newer remote version.
            base_version,
            |_, _| {},
            // Emit "uploading…" only once the signature checks have decided a
            // real upload is happening — a Skipped/Unchanged settle stays
            // quiet in the feed (BUG 2). Only on the first attempt: retries
            // re-firing it filled the feed with "Subiendo… / falló" pairs.
            || {
                if attempt == 0 {
                    let _ = events_tx.try_send(AgentEvent::BackupStarted {
                        save_id: save.save_id.clone(),
                        game_slug: save.game_slug.clone(),
                        label: save.label.clone(),
                    });
                }
            },
        )
        .await;

        match outcome {
            Ok(BackupResult::Skipped) => {
                // The save's cheap set signature is unchanged since the last
                // upload: the watcher fired on a settle that didn't actually
                // write anything. Skip the no-op snapshot, clear has_pending.
                tracing::info!(
                    save_id = %save.save_id,
                    "agent: backup skipped — no content change since last upload"
                );
                let _ = done_tx.try_send(BackupDone {
                    save_id: save.save_id.clone(),
                    new_set_hash: None,
                    committed: false,
                    version_num: None,
                });
                return;
            }
            Ok(BackupResult::Unchanged { signature }) => {
                // The cheap signature drifted (mtime bump) but the bytes are
                // identical to the last upload. No snapshot, but cache the
                // refreshed composite so the next check hits the fast path
                // instead of re-reading every file.
                tracing::info!(
                    save_id = %save.save_id,
                    "agent: backup skipped — bytes unchanged despite mtime drift"
                );
                let _ = done_tx.try_send(BackupDone {
                    save_id: save.save_id.clone(),
                    new_set_hash: Some(signature),
                    committed: false,
                    version_num: None,
                });
                return;
            }
            Ok(BackupResult::Uploaded {
                outcome: o,
                signature,
            }) => {
                let _ = events_tx
                    .send(AgentEvent::BackupSuccess {
                        save_id: save.save_id.clone(),
                        version_num: o.snapshot.version_num,
                        total_bytes: o.total_bytes,
                        set_hash: Some(signature.clone()),
                    })
                    .await;
                // Partial upload: the save was over the plan's per-save cap so
                // only the newest files went up. Fire a second event *after*
                // success so the UI's amber "plan too small" state wins over the
                // green "ok" — the backup worked, but the user must know Free
                // isn't enough for this save.
                if let Some(t) = &o.trimmed {
                    let _ = events_tx
                        .send(AgentEvent::BackupTrimmed {
                            save_id: save.save_id.clone(),
                            game_slug: save.game_slug.clone(),
                            label: save.label.clone(),
                            kept_files: t.kept_files as u64,
                            omitted_files: t.omitted_files as u64,
                            omitted_bytes: t.omitted_bytes,
                            plan: t.plan.clone(),
                            limit_bytes: t.limit_bytes,
                        })
                        .await;
                }
                // Tell the agent loop to clear has_pending and cache the new
                // signature. If the channel is full or the agent is shutting
                // down we just drop the signal — worst case we re-upload an
                // unchanged snapshot on the next GameStopped, a soft failure.
                let _ = done_tx.try_send(BackupDone {
                    save_id: save.save_id.clone(),
                    new_set_hash: Some(signature),
                    committed: true,
                    version_num: Some(o.snapshot.version_num),
                });
                return;
            }
            Err(e) => {
                // Fast-forward conflict (409 non_fast_forward): another device
                // advanced this save past our `base_version`. Re-pushing our
                // stale content with a backoff is exactly how a behind device
                // used to bury a sibling's version. Instead reconcile — pull the
                // remote head with the conflict-aware merge (local-newer files
                // survive, remote-newer overwrite with a backup) — then retry the
                // upload fast-forwarding from the new head. So on a newer remote
                // head, restore wins; only genuinely-newer-or-additional local
                // content goes up afterwards (a purely-stale device matches head
                // and settles). Bounded by MAX_CONFLICT_RECONCILES.
                // Only a *non-fast-forward* 409 means "you're behind, reconcile
                // first" — that's the single 409 the upload path emits today
                // (`init_upload`/`cas_init`), and the server tags it in the body
                // (`code: "non_fast_forward"`, surfaced here as the message
                // "non-fast-forward: …"). Gate on that text so a hypothetical
                // future 409 on this path doesn't get silently reconciled +
                // retried as if it were a version conflict; it falls through to
                // the normal failure handling instead.
                let is_nff = e.chain().any(|c| {
                    matches!(
                        c.downcast_ref::<crate::api::ApiError>(),
                        Some(crate::api::ApiError::Conflict(m)) if m.contains("non-fast-forward")
                    )
                });
                if is_nff {
                    if conflict_reconciles >= MAX_CONFLICT_RECONCILES {
                        let chain = format!("{e:#}");
                        tracing::warn!(
                            save_id = %save.save_id,
                            game_slug = %save.game_slug,
                            conflict_reconciles,
                            error = %chain,
                            "agent: backup conflict — remote head kept moving; giving up after reconcile retries"
                        );
                        let _ = events_tx
                            .send(AgentEvent::BackupFailed {
                                save_id: save.save_id.clone(),
                                game_slug: save.game_slug.clone(),
                                error: chain,
                                will_retry: false,
                            })
                            .await;
                        return;
                    }
                    conflict_reconciles += 1;
                    tracing::info!(
                        save_id = %save.save_id,
                        game_slug = %save.game_slug,
                        base_version = ?base_version,
                        conflict_reconciles,
                        "agent: backup rejected (non-fast-forward) — reconciling remote head before retry"
                    );
                    let retention =
                        Duration::from_secs(u64::from(conflict_retention_days) * 86_400);
                    // Pass our stale `base_version` as known_version (so the
                    // version-gate won't trip — remote is strictly ahead) and
                    // `None` cached_latest / shared_manifest so we fetch the
                    // authoritative head rather than trust a cache that may
                    // itself be stale.
                    match run_auto_restore(
                        &api,
                        &save,
                        conflict_root.as_deref(),
                        retention,
                        base_version,
                        None,
                        None,
                    )
                    .await
                    {
                        Ok(Some(outcome)) => {
                            let touched = outcome.files_restored + outcome.conflicts_backed_up;
                            if touched > 0 {
                                let _ = events_tx
                                    .send(AgentEvent::SaveAutoRestored {
                                        save_id: save.save_id.clone(),
                                        game_slug: save.game_slug.clone(),
                                        version_num: outcome.version_num,
                                        files_extracted: touched,
                                        bytes_extracted: outcome.bytes_extracted,
                                    })
                                    .await;
                                if outcome.conflicts_backed_up > 0 {
                                    if let Some(dir) = outcome.conflict_dir.clone() {
                                        let _ = events_tx
                                            .send(AgentEvent::SaveConflictsBackedUp {
                                                save_id: save.save_id.clone(),
                                                game_slug: save.game_slug.clone(),
                                                count: outcome.conflicts_backed_up,
                                                conflict_dir: dir,
                                            })
                                            .await;
                                    }
                                }
                                // The merge wrote into the live folder — re-arm
                                // the watcher so the slot tracks the new state.
                                let _ = cmd_tx
                                    .send(AgentCommand::RearmWatcher(save.save_id.clone()))
                                    .await;
                            }
                            if !outcome.local_diverged {
                                // The merged tree equals the head we just pulled:
                                // re-uploading would only mint head+1 with
                                // identical bytes (and fan a no-op realtime push
                                // out to every other device). Settle instead —
                                // advance known_version, adopt the post-merge
                                // signature so the merge's own fs writes don't
                                // bounce back as an upload, and clear has_pending.
                                tracing::info!(
                                    save_id = %save.save_id,
                                    game_slug = %save.game_slug,
                                    version_num = outcome.version_num,
                                    "agent: backup conflict reconciled to head with no local divergence — settled without re-upload"
                                );
                                let _ = cmd_tx
                                    .send(AgentCommand::AutoRestoreFinished {
                                        id: save.save_id.clone(),
                                        disposition: AutoRestoreDisposition::Ok,
                                        synced_version: Some(outcome.version_num),
                                        post_restore_set_hash: outcome.disk_set_hash.clone(),
                                        // The reconcile merge wrote head's files
                                        // into the folder — treat like a restore.
                                        wrote_files: true,
                                    })
                                    .await;
                                let _ = done_tx
                                    .send(BackupDone {
                                        save_id: save.save_id.clone(),
                                        new_set_hash: None,
                                        committed: false,
                                        version_num: None,
                                    })
                                    .await;
                                return;
                            }
                            // Local content survived the merge that head lacks —
                            // fast-forward from the head we just reconciled to and
                            // retry so that genuinely-new local data goes up.
                            // Advance the slot's known_version (and the sweep's
                            // gate); leave last_set_hash stale so the retry's
                            // signature check still sees the divergence to upload.
                            base_version = Some(outcome.version_num);
                            let _ = cmd_tx
                                .send(AgentCommand::AutoRestoreFinished {
                                    id: save.save_id.clone(),
                                    disposition: AutoRestoreDisposition::Ok,
                                    synced_version: Some(outcome.version_num),
                                    post_restore_set_hash: None,
                                    // The reconcile merge wrote into the folder.
                                    wrote_files: true,
                                })
                                .await;
                            continue;
                        }
                        Ok(None) => {
                            // 409 said we're behind, yet the reconcile found
                            // nothing newer to pull (remote purged or the head
                            // raced backwards). We can't pick a safe new base, so
                            // surface the conflict rather than risk a loop.
                            let chain = format!("{e:#}");
                            tracing::warn!(
                                save_id = %save.save_id,
                                error = %chain,
                                "agent: backup conflict but reconcile found nothing to pull — surfacing"
                            );
                            let _ = events_tx
                                .send(AgentEvent::BackupFailed {
                                    save_id: save.save_id.clone(),
                                    game_slug: save.game_slug.clone(),
                                    error: chain,
                                    will_retry: false,
                                })
                                .await;
                            return;
                        }
                        Err(re) => {
                            let chain = format!("{re:#}");
                            tracing::warn!(
                                save_id = %save.save_id,
                                error = %chain,
                                "agent: backup conflict — reconcile failed; surfacing"
                            );
                            let _ = events_tx
                                .send(AgentEvent::BackupFailed {
                                    save_id: save.save_id.clone(),
                                    game_slug: save.game_slug.clone(),
                                    error: chain,
                                    will_retry: false,
                                })
                                .await;
                            return;
                        }
                    }
                }
                // Empty source (no regular files to upload): not a failure.
                // Pushing an empty snapshot would clobber the last good server
                // copy, so we skip exactly like the up-front empty-folder guard.
                // Reached when the folder holds only subdirs / no files (e.g. an
                // empty `Repo/saves`). Clear has_pending so a later write isn't
                // blocked, and settle without a red "falló".
                if e.chain().any(|c| c.is::<crate::backup::EmptySource>()) {
                    tracing::info!(
                        save_id = %save.save_id,
                        game_slug = %save.game_slug,
                        "agent: backup skipped — source has no files to upload"
                    );
                    let _ = done_tx.try_send(BackupDone {
                        save_id: save.save_id.clone(),
                        new_set_hash: None,
                        committed: false,
                        version_num: None,
                    });
                    let _ = events_tx
                        .send(AgentEvent::BackupSkippedEmpty {
                            save_id: save.save_id.clone(),
                            game_slug: save.game_slug.clone(),
                        })
                        .await;
                    return;
                }
                // Archived game (403 `save_archived`): the user parked this save
                // in the server-side "caja negra". Re-uploading would revive its
                // frozen blobs and undo the quota it freed, so never retry —
                // settle quietly (clear has_pending, no red "falló"). The local
                // save stays put; the desktop learns the archived state from
                // `/v1/cloud/storage/games` and surfaces it there.
                let is_archived = e.chain().any(|c| {
                    matches!(
                        c.downcast_ref::<crate::api::ApiError>(),
                        Some(crate::api::ApiError::Archived)
                    )
                });
                if is_archived {
                    tracing::info!(
                        save_id = %save.save_id,
                        game_slug = %save.game_slug,
                        "agent: backup skipped — game is archived on the server (caja negra)"
                    );
                    let _ = done_tx.try_send(BackupDone {
                        save_id: save.save_id.clone(),
                        new_set_hash: None,
                        committed: false,
                        version_num: None,
                    });
                    return;
                }
                // Per-save size cap (413 `save_too_large`): the upload can never
                // succeed as-is, so retrying just burns the budget and spams the
                // feed. Emit a dedicated, actionable event and settle (clear
                // has_pending) so it doesn't re-fire until the folder actually
                // changes — no red "falló", no retry loop.
                let too_large = e
                    .chain()
                    .find_map(|c| c.downcast_ref::<crate::api::ApiError>())
                    .and_then(|api_err| match api_err {
                        crate::api::ApiError::TooLarge(d) => Some(d.clone()),
                        _ => None,
                    });
                if let Some(detail) = too_large {
                    tracing::warn!(
                        save_id = %save.save_id,
                        game_slug = %save.game_slug,
                        plan = %detail.plan,
                        limit_bytes = detail.limit_bytes,
                        actual_bytes = detail.actual_bytes,
                        "agent: backup rejected — save exceeds the plan's per-save size cap"
                    );
                    let _ = done_tx.try_send(BackupDone {
                        save_id: save.save_id.clone(),
                        new_set_hash: None,
                        committed: false,
                        version_num: None,
                    });
                    let _ = events_tx
                        .send(AgentEvent::BackupTooLarge {
                            save_id: save.save_id.clone(),
                            game_slug: save.game_slug.clone(),
                            label: save.label.clone(),
                            plan: detail.plan.clone(),
                            limit_bytes: detail.limit_bytes,
                            actual_bytes: detail.actual_bytes,
                        })
                        .await;
                    return;
                }
                // Bandwidth throttle (429): wait the server's exact
                // window-slide time and retry without consuming the
                // network-flake budget. Kept out of the "falló" feed path —
                // we emit an amber "en espera" entry instead.
                let retry_after = e
                    .chain()
                    .find_map(|c| c.downcast_ref::<crate::api::ApiError>())
                    .and_then(|api_err| match api_err {
                        crate::api::ApiError::RateLimited {
                            retry_after_seconds,
                            ..
                        } => Some(*retry_after_seconds),
                        _ => None,
                    });
                if let Some(retry_after) = retry_after {
                    if throttle_waits < MAX_THROTTLE_WAITS {
                        // Cap the wait so a bogus huge `retry_after` can't park
                        // the upload forever; +2s jitter avoids a thundering
                        // herd of saves all retrying on the same tick.
                        let wait = (u64::from(retry_after)).clamp(1, 300) + 2;
                        tracing::info!(
                            save_id = %save.save_id,
                            game_slug = %save.game_slug,
                            throttle_waits,
                            retry_after,
                            wait,
                            "agent: backup throttled by bandwidth limit — waiting to retry"
                        );
                        let _ = events_tx
                            .send(AgentEvent::BackupThrottled {
                                save_id: save.save_id.clone(),
                                game_slug: save.game_slug.clone(),
                                label: save.label.clone(),
                                retry_after_secs: retry_after,
                            })
                            .await;
                        tokio::time::sleep(Duration::from_secs(wait)).await;
                        throttle_waits += 1;
                        continue;
                    }
                    // Exhausted our patience for the window — fall through and
                    // surface it as a normal failure below.
                }
                let will_retry = attempt < max_retries;
                // `{:#}` renders the whole anyhow context chain — `.to_string()`
                // alone collapses it to the outermost label ("cloud cas init"),
                // which is what made this failure undiagnosable from the feed.
                let chain = format!("{e:#}");
                let backoff_secs = (1u64 << attempt.min(8)).min(300);
                tracing::warn!(
                    save_id = %save.save_id,
                    game_slug = %save.game_slug,
                    attempt,
                    max_retries,
                    will_retry,
                    backoff_secs = if will_retry { backoff_secs } else { 0 },
                    error = %chain,
                    "agent: backup attempt failed"
                );
                // Feed-visible failure only when the retries are exhausted —
                // intermediate attempts stay in the log, otherwise one flaky
                // burst paints the feed with a dozen "falló" rows.
                if !will_retry {
                    let _ = events_tx
                        .send(AgentEvent::BackupFailed {
                            save_id: save.save_id.clone(),
                            game_slug: save.game_slug.clone(),
                            error: chain,
                            will_retry,
                        })
                        .await;
                    // Out of retries, but the slot must not be left wedged. We
                    // deliberately send no `BackupDone`: the local changes never
                    // made it to a version, so `has_pending` has to stay set or
                    // a later restore would overwrite them. That also means the
                    // slot is now vetoed from every pull *and* has nothing left
                    // that would re-fire the upload — until this returned, only
                    // a fresh fs event could break the deadlock, so a save whose
                    // game was already closed just sat there. Hand the retry
                    // back to the agent loop instead.
                    let _ = cmd_tx
                        .send(AgentCommand::RetryBackupAfterFailure(save.save_id.clone()))
                        .await;
                    return;
                }
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                attempt += 1;
            }
        }
    }
}

/// `accept_correlation_signals` (el filtro anti horas-fantasma) se movió al
/// kernel leaf en el Slice 1 (ADR 0021): vive en
/// [`hoard_core::kernel::correlation`] y se importa arriba. Era ya una función
/// pura, así que su sitio natural es el kernel.

/// Longitud mínima de un token de identidad para que cuente en el match
/// genérico. Por debajo (`gta`, `ori`, `ff`) es demasiado corto y colisiona
/// con carpetas o nombres de proceso cualesquiera.
const MIN_IDENTITY_TOKEN_LEN: usize = 4;

/// Token canónico de identidad de un juego/proceso: solo alfanuméricos ASCII en
/// minúscula, sin separadores ni extensión. Unifica las tres formas en las que
/// el mismo juego aparece — slug (`victoria-3`), nombre visible (`Victoria 3`) y
/// ejecutable (`victoria3.exe` → `victoria3`) — en una sola clave comparable.
fn canon_token(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

/// Tokens VETADOS en el match genérico de identidad: componentes del perfil de
/// usuario y de la fontanería de instalación. Un slug degenerado igual a uno de
/// estos convierte procesos cualesquiera en señal fuerte de "estás jugando" —
/// caso real jul-2026: el save de `GSE Saves` quedó rastreado con slug =
/// nombre de usuario de Windows ("jacka"), y como el username es componente de
/// ruta de TODO exe bajo `C:\Users\<user>\...`, cualquier app del perfil
/// disparaba GameStarted (y el guard "un juego a la vez" apagaba de rebote los
/// juegos reales). La lista estática cubre la fontanería común; los
/// componentes del home real (username incluido) se añaden dinámicamente.
fn is_generic_identity_token(tok: &str) -> bool {
    const GENERIC: &[&str] = &[
        "users",
        "home",
        "appdata",
        "roaming",
        "local",
        "locallow",
        "documents",
        "savedgames",
        "mygames",
        "saves",
        "games",
        "programfiles",
        "programfilesx86",
        "steamapps",
        "common",
        "compatdata",
        "drivec",
        "windows",
        "desktop",
        "downloads",
    ];
    static HOME_TOKENS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let home = HOME_TOKENS.get_or_init(|| {
        directories::UserDirs::new()
            .map(|u| {
                u.home_dir()
                    .components()
                    .filter_map(|c| match c {
                        std::path::Component::Normal(s) => s.to_str().map(canon_token),
                        _ => None,
                    })
                    .filter(|t| t.len() >= MIN_IDENTITY_TOKEN_LEN)
                    .collect()
            })
            .unwrap_or_default()
    });
    GENERIC.contains(&tok) || home.iter().any(|h| h == tok)
}

/// Tokens de identidad de un save rastreado, derivados de datos que ya tenemos
/// (slug + nombre visible) — SIN lista curada. Son las claves contra las que se
/// compara cada proceso vivo. Los tokens genéricos/de perfil se vetan
/// ([`is_generic_identity_token`]): un juego así de mal nombrado pierde el
/// match por token (le quedan las otras señales) antes que casar con todo.
fn game_identity_tokens(slug: &str, display: &str) -> Vec<String> {
    let mut v: Vec<String> = Vec::with_capacity(2);
    for raw in [slug, display] {
        let t = canon_token(raw);
        if t.len() >= MIN_IDENTITY_TOKEN_LEN
            && !is_generic_identity_token(&t)
            && !v.contains(&t)
        {
            v.push(t);
        }
    }
    v
}

/// Candidatos de identidad de un proceso vivo, list-free y multiplataforma: el
/// basename del ejecutable (`.../Stellaris/stellaris` → `stellaris`), el nombre
/// del proceso, y cada componente de la RUTA del ejecutable — porque la carpeta
/// de instalación casi siempre lleva el nombre del juego (`steamapps/common/The
/// Witcher 3 Wild Hunt/...`, `GOG Games/...`, el `.app` de macOS). Con esto un
/// juego cuyo exe está abreviado (`witcher3.exe`) casa igual por su carpeta. La
/// comparación es igualdad exacta de tokens canónicos, así que un componente
/// genérico (`common`, `bin`, `x64`) no colisiona con un slug real.
fn process_identity_candidates(name: &str, exe: Option<&Path>) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    let push = |s: &str, v: &mut Vec<String>| {
        let t = canon_token(s);
        if t.len() >= MIN_IDENTITY_TOKEN_LEN
            && !is_generic_identity_token(&t)
            && !v.contains(&t)
        {
            v.push(t);
        }
    };
    push(name, &mut v);
    if let Some(exe) = exe {
        if let Some(base) = exe.file_stem().and_then(|s| s.to_str()) {
            push(base, &mut v);
        }
        for comp in exe.components() {
            if let std::path::Component::Normal(c) = comp {
                if let Some(s) = c.to_str() {
                    push(s, &mut v);
                }
            }
        }
    }
    v
}

/// Rutas abiertas (fd + cwd) por el proceso `pid` que caen DENTRO de alguna de
/// `folders`. Señal de arranque agnóstica del instalador y del nombre del exe:
/// si un proceso de juego tiene abierto un fichero de la carpeta de save (o su
/// cwd está ahí), ese proceso es el juego de ese save — sin catálogo, sin Steam
/// y sin esperar a que escriba (basta con que lo tenga abierto, p. ej. al listar
/// partidas en el menú de carga o al mapear el save en memoria). Devuelve los
/// `save_id` casados.
///
/// Hoy solo Linux/SteamOS (vía `/proc/<pid>/fd` y `/proc/<pid>/cwd`, que no
/// requieren privilegios para procesos propios). Windows/macOS devuelven vacío
/// por ahora — su equivalente (enumerar handles / `proc_pidfdinfo`) queda
/// pendiente; ahí la detección se apoya en nombre/carpeta y correlación.
#[cfg(target_os = "linux")]
fn open_paths_matching(pid: Pid, folders: &[(&str, &Path)]) -> Vec<String> {
    let mut hits: Vec<String> = Vec::new();
    if folders.is_empty() {
        return hits;
    }
    let check = |p: &Path, hits: &mut Vec<String>| {
        for (id, folder) in folders {
            if p.starts_with(folder) && !hits.iter().any(|h| h == id) {
                hits.push((*id).to_string());
            }
        }
    };
    let base = std::path::PathBuf::from(format!("/proc/{pid}"));
    if let Ok(cwd) = std::fs::read_link(base.join("cwd")) {
        check(&cwd, &mut hits);
    }
    if let Ok(entries) = std::fs::read_dir(base.join("fd")) {
        for entry in entries.flatten() {
            if hits.len() == folders.len() {
                break; // ya casaron todos; no sigas leyendo fds
            }
            if let Ok(target) = std::fs::read_link(entry.path()) {
                check(&target, &mut hits);
            }
        }
    }
    hits
}

#[cfg(not(target_os = "linux"))]
fn open_paths_matching(_pid: Pid, _folders: &[(&str, &Path)]) -> Vec<String> {
    Vec::new()
}

/// One sweep of the process table. Emits transitions + schedules a
/// post-game backup when a watched game stops running.
///
/// Since 1.4 this no longer touches the fs watcher — the watcher is armed
/// in `handle_add` and lives for the slot's lifetime. `process_poll` is
/// pure UI signal (Dashboard pill, "the game just closed → flush" hint).
///
/// Returns whether any tracked game is currently running, so the caller can
/// throttle the poll cadence (fast while a game is up, slow when idle).
#[allow(clippy::too_many_arguments)]
fn process_poll(
    sys: &mut System,
    slots: &mut HashMap<String, SaveSlot>,
    events_tx: &mpsc::Sender<AgentEvent>,
    api: &ApiClient,
    config: &AgentConfig,
    done_tx: &mpsc::Sender<BackupDone>,
    cmd_tx: &mpsc::Sender<AgentCommand>,
    playtime: &mut crate::playtime::PlaytimeStore,
    playtime_path: Option<&std::path::Path>,
    reported_heavy: &mut HashSet<Pid>,
    // Mutable: las transiciones de parada pasan strikes de sesión fantasma a
    // las observaciones de correlación (y las descartan al llegar al tope).
    corr_store: &mut crate::correlation::CorrelationStore,
    corr_path: Option<&std::path::Path>,
    steam_index: &crate::playtime_index::SteamPlaytimeIndex,
    prev_pids: &mut HashSet<Pid>,
    corr_running: &mut HashMap<String, (Pid, u64)>,
    // Poller's per-save cloud version cache. Read only on the `GameStopped`
    // transition, to spot a save the cloud has moved past while the user was
    // playing (see `run_deferred_pull`).
    latest_versions: &HashMap<String, i64>,
) -> bool {
    // Refresh every process. The `true` flag asks sysinfo to remove
    // entries for processes that have exited since the last refresh,
    // which is exactly what we need to detect "game stopped".
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, proc_refresh_kind());

    // Build a set of "currently running" save_ids. Two matchers cooperate:
    // process-name match (manifest-driven, storefront-agnostic) takes
    // precedence; install-dir match is the legacy v0.2 fallback for
    // saves registered without a manifest.
    //
    // Single pass over the process table: invert the slots into a name→ids
    // index up front so the scan is O(procs + slots) instead of O(procs ×
    // slots) — the old nested loop re-scanned every process for every slot and
    // rebuilt a HashSet per slot per tick, which got worse now that playtime-
    // only games add up to ~16 extra slots.
    let mut name_index: HashMap<String, Vec<&str>> = HashMap::new();
    // Índice genérico de identidad (slug/nombre → save_ids), list-free y
    // multiplataforma. Es la vía que arregla los juegos sin procesos
    // configurados (Stellaris, Victoria…): antes solo casaban por correlación
    // fría o por `steam_install_dir`, así que la primera sesión no disparaba
    // "arrancó" ni auto-restore aunque el save sí se detectara. Ahora casan por
    // su propio nombre/carpeta sin depender de Steam ni de una lista curada.
    let mut token_index: HashMap<String, Vec<&str>> = HashMap::new();
    // Carpetas de save rastreadas `(save_id, local_path)`. Las usa la detección
    // por HANDLES ABIERTOS: un proceso de juego que tiene un fichero abierto
    // dentro de una de estas carpetas ES el juego de ese save — agnóstico del
    // instalador y del nombre del exe (resuelve exes en clave como EU5 sin
    // catálogo ni Steam). Se saltan los `track_only` (no tienen save real).
    let mut save_folders: Vec<(&str, &Path)> = Vec::new();
    let mut dir_slots: Vec<(&str, &Path)> = Vec::new();
    // Señales de correlación candidatas `(proc_name_lower, save_id, game_slug)`,
    // recogidas aparte para vetar las ambiguas ANTES de que cuenten horas (ver
    // `accept_correlation_signals`).
    let mut corr_candidates: Vec<(String, &str, &str)> = Vec::new();
    for slot in slots.values() {
        // Identidad genérica: vale para TODOS los slots (con o sin procesos
        // configurados, `track_only` incluido). Es aditivo sobre un HashSet, así
        // que solaparse con `name_index` es inocuo.
        for tok in game_identity_tokens(&slot.save.game_slug, &slot.save.display_name) {
            token_index
                .entry(tok)
                .or_default()
                .push(slot.save.save_id.as_str());
        }
        if !slot.save.track_only {
            save_folders.push((slot.save.save_id.as_str(), slot.save.local_path.as_path()));
        }
        if slot.save.processes.is_empty() {
            // Correlation-learned launch signal (ADR 0020, storefront- y
            // juego-agnóstico): si Hoard ya observó algún proceso de JUEGO
            // escribiendo en la carpeta de este save, ese proceso es la señal
            // de "estás jugando". Sin esto, un juego fuera de la lista (p. ej.
            // EU5 bajo Proton, cuyo exe no cae bajo `steam_install_dir`) nunca
            // entra en `running` y no suma horas. PERO la atribución
            // carpeta→proceso es ruidosa: si algo de fondo (Steam Cloud) reescribe
            // la carpeta de save de OTRO juego mientras corre éste, esa carpeta
            // queda correlacionada con este proceso. Para detección eso es
            // inocuo (revisable); para PLAYTIME acumularía horas fantasma. Por
            // eso aquí sólo recogemos candidatos y filtramos abajo.
            if let Some(obs) = corr_store.signal_for(&slot.save.local_path) {
                // Re-valida la observación contra las reglas ACTUALES y exige
                // un exe en disco: blinda contra entradas basura grabadas por
                // versiones previas con filtros más laxos (p. ej. el worker de
                // kernel `ib_srv_wkr-2`, sin exe, que vive 24/7 y acumularía
                // horas para siempre). En cuanto se re-grabe la correlación
                // durante una sesión real, queda corregida y se confía en ella.
                if obs.exe.is_some()
                    && crate::correlation::is_game_like(&obs.process_name, obs.exe.as_deref())
                {
                    corr_candidates.push((
                        obs.process_name.to_lowercase(),
                        slot.save.save_id.as_str(),
                        slot.save.game_slug.as_str(),
                    ));
                }
            }
            // Legacy fallback only when no process names are configured.
            if let Some(dir) = slot.save.steam_install_dir.as_deref() {
                dir_slots.push((slot.save.save_id.as_str(), dir));
            }
            continue;
        }
        for p in &slot.save.processes {
            name_index
                .entry(p.to_lowercase())
                .or_default()
                .push(slot.save.save_id.as_str());
        }
    }

    // Las señales de correlación NO se mezclan con los process-names de
    // manifest: van a un índice aparte porque un match por correlación sólo
    // cuenta como "jugando" si el proceso tiene CPU real en este tick (ver
    // `CORRELATION_MIN_CPU_PCT`). Sólo se inyectan las que sobreviven al filtro
    // anti horas-fantasma (los process-names configurados son de juegos con
    // manifest).
    let configured: HashSet<String> = name_index.keys().cloned().collect();
    let mut corr_index: HashMap<String, Vec<&str>> = HashMap::new();
    for (pname, save_id) in accept_correlation_signals(&corr_candidates, &configured) {
        corr_index.entry(pname).or_default().push(save_id);
    }

    // Señal DÉBIL = transición de PID, no presencia+CPU. `first_tick` (no había
    // foto previa) marca el arranque del agente: en él NO disparamos "arrancó"
    // por correlación —adoptamos lo que ya corría como pre-existente— para no
    // confundir un residente vivo desde el boot con un lanzamiento. `cur_pids`
    // se convierte en la foto del próximo tick.
    let first_tick = prev_pids.is_empty();
    let mut cur_pids: HashSet<Pid> = HashSet::with_capacity(sys.processes().len());

    // Señales FUERTES: el proceso lleva el nombre/identidad del juego, corre
    // desde su carpeta de instalación o tiene un fichero de su save abierto.
    // Todas exigen que el ejecutable real del juego EXISTA ahora mismo y que el
    // proceso siga VIVO (`is_defunct`): que exista el exe no bastaba — un juego
    // de Proton que muere mal deja un zombi con su mismo nombre y exe, y eso
    // mantenía el juego "corriendo" indefinidamente.
    let mut running: HashSet<String> = HashSet::new();
    // Señales DÉBILES (correlación carpeta→proceso): no exigen el exe real del
    // juego, solo que "algún proceso de juego" tocara su carpeta alguna vez. Un
    // proceso de fondo mal atribuido puede mantenerlas vivas indefinidamente
    // (caso offworld: 35 min sin cerrarse). Se resuelven aparte para poder
    // aplicarles el guard "un juego a la vez" más abajo.
    let mut weak_running: HashSet<String> = HashSet::new();
    // PLAYTIME "solo lo que juegas": slugs de juegos de Steam que corren pero
    // no están rastreados por ningún slot (ni save real ni catálogo). Se cuentan
    // igual para el Wrapped; ver `steam_index` y la atribución más abajo.
    let mut steam_running: HashSet<String> = HashSet::new();
    for (pid, proc) in sys.processes() {
        let name = proc.name().to_string_lossy().to_lowercase();
        // Un proceso difunto conserva nombre, exe y `start_time`, así que casaría
        // con las señales FUERTES igual que uno vivo y mantendría el slot
        // "corriendo" para siempre (ver `is_defunct`). No puede estar escribiendo
        // un save: queda fuera de las cuatro. La DÉBIL tampoco lo quiere: aunque
        // sólo ARRANCA con un PID que nace, su arm de "mismo PID sigue vivo"
        // aceptaba al zombi tick tras tick (un juego Proton que muere mal deja el
        // zombi con el mismo `rpid`/`rst`), y el slot no salía de "corriendo"
        // hasta el reboot — al zombi no se le puede matar y el force quit no
        // genera transición. Ver incidente PoP 2008 jul-2026.
        let defunct = is_defunct(proc.status());
        // Name match — works on every storefront on Windows, and on
        // Proton/Wine where the wineprefix process keeps the .exe name.
        if !defunct && !name_index.is_empty() {
            if let Some(ids) = name_index.get(&name) {
                running.extend(ids.iter().map(|id| id.to_string()));
            }
        }
        // Match genérico por identidad (list-free): el proceso lleva el nombre
        // del juego o corre desde su carpeta de instalación. Sin gate de CPU —
        // igualdad exacta con el slug/nombre del juego es señal fuerte por sí
        // sola, y así un juego pausado (menús de Paradox, a 0% de CPU) sigue
        // contando como "corriendo". `is_game_like` descarta sistema/launchers.
        if !defunct
            && !token_index.is_empty()
            && crate::correlation::is_game_like(&name, proc.exe())
        {
            for cand in process_identity_candidates(&name, proc.exe()) {
                if let Some(ids) = token_index.get(&cand) {
                    running.extend(ids.iter().map(|id| id.to_string()));
                }
            }
        }
        // Match por HANDLES ABIERTOS (agnóstico del instalador y del nombre del
        // exe): si un proceso de juego tiene abierto un fichero de la carpeta de
        // save, es el juego de ese save. Resuelve los exes en clave/abreviados
        // (EU5 → `eu5.exe`) que ni el nombre ni la carpeta delatan, sin catálogo
        // ni Steam. Sólo para procesos con pinta de juego, para acotar el coste
        // de leer `/proc/<pid>/fd`.
        if !defunct
            && !save_folders.is_empty()
            && crate::correlation::is_game_like(&name, proc.exe())
        {
            for id in open_paths_matching(*pid, &save_folders) {
                running.insert(id);
            }
        }
        cur_pids.insert(*pid);
        // Match por correlación (señal DÉBIL) por TRANSICIÓN DE PID: el slot
        // corre mientras viva el PID que lo arrancó, y sólo arranca cuando un PID
        // que casa su nombre NACE este tick (no estaba el tick anterior). Sin
        // gate de CPU: un residente correlacionado por error (Discord) nunca
        // "aparece", así que ningún pico de CPU puede dispararlo. Va a
        // `weak_running`; el guard "un juego a la vez" aún lo descarta si otro
        // juego corre por señal fuerte (ver más abajo).
        if !defunct && !corr_index.is_empty() {
            if let Some(ids) = corr_index.get(&name) {
                let st = proc.start_time();
                for id in ids {
                    match corr_running.get(*id) {
                        // Es el PID que ya mantenía vivo este slot y sigue vivo.
                        Some((rpid, rst)) if *rpid == *pid && *rst == st => {
                            weak_running.insert(id.to_string());
                        }
                        // PID distinto (o slot parado): sólo cuenta si acaba de
                        // nacer. En el primer tick tras arrancar el agente nada
                        // es "nuevo" — un juego ya abierto se detecta igual por
                        // señal fuerte; la correlación lo recupera al relanzar.
                        _ => {
                            if !first_tick && !prev_pids.contains(pid) {
                                weak_running.insert(id.to_string());
                                corr_running.insert(id.to_string(), (*pid, st));
                            }
                        }
                    }
                }
            }
        }
        // Legacy install-dir fallback for slots without process names.
        if !defunct && !dir_slots.is_empty() {
            if let Some(exe) = proc.exe() {
                for (id, dir) in &dir_slots {
                    if exe.starts_with(dir) {
                        running.insert(id.to_string());
                    }
                }
            }
        }

        // PLAYTIME "solo lo que juegas" (recap, Steam): cuenta horas de juegos
        // de Steam aunque no estén rastreados. Es SOLO para el Wrapped, no para
        // detectar arranque — la detección de "corriendo" es agnóstica del
        // instalador (nombre/carpeta + handles abiertos + correlación), sin
        // tocar Steam. Exigimos CPU real, no-hilo y pinta de juego para no
        // sumar herramientas de fondo bajo `steamapps/common`.
        if !steam_index.is_empty()
            && proc.thread_kind().is_none()
            && proc.cpu_usage() >= CORRELATION_MIN_CPU_PCT
        {
            if let Some(exe) = proc.exe() {
                if crate::correlation::is_game_like(&name, Some(exe)) {
                    if let Some(slug) = steam_index.slug_for_exe(exe) {
                        steam_running.insert(slug.to_string());
                    }
                }
            }
        }

        // Immediate-scan trigger: a process burning real CPU that looks like a
        // game but matches no tracked save's process name is probably a
        // just-launched, not-yet-tracked game. Flag it once (deduped by PID) so
        // the desktop fires a detection scan now instead of waiting out the
        // 10-min timer. Cheap: `cpu_usage` and `name` come from the same
        // `/proc/<pid>/stat` already parsed above. Tracked games are skipped
        // via `name_index` (their launch is already handled by the barrier).
        if proc.cpu_usage() >= HEAVY_PROCESS_CPU_PCT
            && !name_index.contains_key(&name)
            && !corr_index.contains_key(&name)
            && !reported_heavy.contains(pid)
            && crate::correlation::is_game_like(&name, proc.exe())
        {
            tracing::info!(
                process = %name,
                cpu = proc.cpu_usage(),
                "agent: heavy untracked game-like process; requesting detection scan"
            );
            let _ = events_tx.try_send(AgentEvent::HeavyProcessDetected {
                name: proc.name().to_string_lossy().into_owned(),
            });
            reported_heavy.insert(*pid);
        }
    }
    // Forget PIDs that have exited so a relaunch of the same game re-triggers.
    reported_heavy.retain(|pid| sys.processes().contains_key(pid));
    // Suelta la atribución débil de slots cuyo PID ya no vive, y guarda la foto
    // de PIDs para que el próximo tick sepa cuáles nacieron.
    corr_running.retain(|_, (pid, _)| cur_pids.contains(pid));
    *prev_pids = cur_pids;

    // Stop-debounce SÓLO para señales FUERTES: un match por nombre/handle puede
    // caerse un tick por una carrera del refresco de procesos. Las señales
    // DÉBILES ya son exactas (transición de PID: su "parado" es la muerte del
    // PID), así que NO entran en el sticky — sin ellas aquí desaparece el ciclo
    // de 90 s y el "35 min sin cerrarse". Refresca el stamp de los slots fuertes
    // vivos y re-añade los que cayeron dentro de la ventana de gracia.
    let now_inst = TokioInstant::now();
    for id in running.iter() {
        if let Some(slot) = slots.get_mut(id) {
            slot.last_running_seen = Some(now_inst);
            // Una señal fuerte corrobora la sesión: ya no es "solo débil".
            slot.weak_session = false;
        }
    }
    // Foto de los ids con señal FUERTE este tick, ANTES de mezclar débiles y
    // sticky: las transiciones de abajo la usan para saber si un arranque fue
    // solo-correlación (candidato a sesión fantasma).
    let strong_now: HashSet<String> = running.iter().cloned().collect();
    let sticky = Duration::from_secs(
        config
            .poll_secs
            .saturating_mul(RUNNING_STICKY_POLLS)
            .max(STRONG_STOP_GRACE_FLOOR_SECS),
    );
    let readd: Vec<String> = slots
        .iter()
        .filter(|(id, slot)| {
            slot.is_running
                && !strong_now.contains(id.as_str())
                && slot
                    .last_running_seen
                    .is_some_and(|seen| now_inst.duration_since(seen) < sticky)
        })
        .map(|(id, _)| id.clone())
        .collect();

    // Guard "un juego a la vez": las señales fuertes (`running`) exigen que el
    // exe real del juego exista, así que sus slugs son juegos que corren de
    // verdad AHORA. Casi nadie juega a dos a la vez, y una correlación pegada a
    // un proceso de fondo puede mantener un juego ya cerrado "arrancado" para
    // siempre (offworld). Por eso, si algún juego corre por señal fuerte,
    // descartamos las señales DÉBILES (correlación) y las re-añadidas por
    // sticky de OTROS juegos: al arrancar otro juego, el fantasma se apaga y se
    // queda apagado mientras juegas. Sin ningún juego fuerte, la correlación y
    // el sticky siguen valiendo (juegos que SOLO casan así se detectan igual).
    let strong_slugs: HashSet<String> = running
        .iter()
        .filter_map(|id| slots.get(id).map(|s| s.save.game_slug.clone()))
        .collect();
    let survives_guard = |id: &str| -> bool {
        strong_slugs.is_empty()
            || slots
                .get(id)
                .is_some_and(|s| strong_slugs.contains(&s.save.game_slug))
    };
    for id in weak_running.into_iter().chain(readd) {
        if survives_guard(&id) {
            running.insert(id);
        } else {
            tracing::debug!(
                save_id = %id,
                "agent: señal débil/sticky descartada — otro juego corre por señal fuerte"
            );
        }
    }

    // PLAYTIME: atribuye el intervalo de este tick a los juegos vivos. El cap
    // es 4× el poll (mín. 30 s) para no contar un suspend/resume como juego.
    let mut running_games: Vec<(String, String)> = running
        .iter()
        .filter_map(|id| {
            slots
                .get(id)
                .map(|s| (id.clone(), s.save.game_slug.clone()))
        })
        .collect();
    // Suma los juegos de Steam jugados-pero-no-rastreados que ningún slot ya
    // cuenta (evita doble conteo por slug). El `save_id` sintético es estable
    // entre ticks —clave de ancla en `PlaytimeStore::accrue`— y su prefijo lo
    // hace obvio en logs.
    if !steam_running.is_empty() {
        let counted: HashSet<String> = running_games.iter().map(|(_, s)| s.clone()).collect();
        for slug in &steam_running {
            if !counted.contains(slug) {
                running_games.push((format!("playtime:steam:{slug}"), slug.clone()));
            }
        }
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let max_step = config.poll_secs.saturating_mul(4).max(30);
    playtime.accrue(&running_games, now_ms, max_step);

    // Diff against previous tick to fire transition events.
    // We collect first, then mutate, to keep the borrow-checker happy.
    let transitions: Vec<(String, bool)> = slots
        .keys()
        .map(|id| (id.clone(), running.contains(id)))
        .filter(|(id, now)| slots.get(id).map(|s| s.is_running != *now).unwrap_or(false))
        .collect();
    // Persist eagerly when a game just stopped (fresh recap on quit); otherwise
    // throttle to avoid writing the JSON on every poll.
    let any_stop = transitions.iter().any(|(_, now)| !*now);

    for (id, now_running) in transitions {
        let (game_slug, local_path, had_pending) = {
            let slot = match slots.get(&id) {
                Some(s) => s,
                None => continue,
            };
            (
                slot.save.game_slug.clone(),
                slot.save.local_path.clone(),
                slot.has_pending,
            )
        };

        if now_running {
            // Decide the pre-launch sync barrier *before* flipping
            // `is_running` — the sweep skips running slots, but the whole
            // point of the barrier is to pull on launch. We still honour the
            // other "user is here" guards so we never clobber an active local
            // session: un-flushed changes, a recent fs event, a recently
            // touched folder, an in-flight restore, or an unexpired cooldown
            // all veto the pull. The restore itself is conflict-aware
            // (local-newer files win, conflicts are backed up), so even when
            // it does fire it can't lose newer local progress.
            let barrier_save: Option<WatchedSave> = {
                slots.get(&id).and_then(|slot| {
                    // Playtime-only entries have no save folder to pull into.
                    if slot.save.track_only {
                        return None;
                    }
                    // Per-save preset can disable (backup-only) or enable the
                    // pull barrier regardless of the global default. `global_sync`
                    // counts as a global opt-in.
                    if !slot
                        .save
                        .policy
                        .auto_restore
                        .unwrap_or(config.auto_restore || config.global_sync)
                    {
                        return None;
                    }
                    if slot.restoring {
                        return None;
                    }
                    if let Some(t) = slot.next_auto_restore_at {
                        if TokioInstant::now() < t {
                            return None;
                        }
                    }
                    // The user-is-here guards apply under sync global too. The
                    // old bypass ("pull at launch even if the folder was just
                    // written") meant a quick relaunch — or a process-poll flap
                    // re-firing GameStarted mid-session — could merge the cloud
                    // head over changes the backup hadn't flushed yet. A
                    // genuine cross-device hand-off passes these guards anyway
                    // (this device wasn't the one just writing the folder), so
                    // the barrier still fires exactly when it's wanted.
                    // `is_running` is still false here — it flips after the
                    // barrier decision — so the launch itself never vetoes.
                    if mid_session_reason(slot).is_some() {
                        return None;
                    }
                    Some(slot.save.clone())
                })
            };

            // ¿Arranque solo-débil? Ninguna señal fuerte lo corrobora este
            // tick: candidato a sesión fantasma (ver `SaveSlot::weak_session`).
            let weak_start = !strong_now.contains(id.as_str());
            if let Some(slot) = slots.get_mut(&id) {
                slot.is_running = true;
                slot.weak_session = weak_start;
                // A new session earns a new "update waiting" notice if a pull
                // gets deferred again. `pull_pending` itself survives: an
                // update that arrived while the game was closed but couldn't
                // land (a restore in flight, un-flushed changes) is still owed.
                slot.deferred_notified = false;
            }
            // El nombre del proceso correlacionado va al log: sin él, un
            // GameStarted fantasma es indiagnosticable (caso MOUSE jul-2026:
            // días de arranques horarios sin saber qué proceso los causaba).
            let corr_process = if weak_start {
                corr_store.attributed_name(&local_path)
            } else {
                None
            };
            tracing::info!(
                save_id = %id,
                game_slug = %game_slug,
                path = %local_path.display(),
                signal = if weak_start { "correlation" } else { "strong" },
                corr_process = %corr_process.as_deref().unwrap_or("-"),
                "agent: GameStarted"
            );
            let _ = events_tx.try_send(AgentEvent::GameStarted {
                save_id: id.clone(),
                game_slug,
            });

            // Pre-launch sync barrier (Fase 1): the instant a game launches,
            // pull the latest remote snapshot so a cross-device hand-off feels
            // immediate — play on the tablet, sit down at the PC, launch, and
            // the tablet's progress is already there. Reuses the same
            // conflict-aware restore as the reconciliation sweep.
            if let Some(save) = barrier_save {
                let known_version = slots.get(&id).and_then(|s| s.known_version);
                if let Some(slot) = slots.get_mut(&id) {
                    slot.restoring = true;
                    slot.next_auto_restore_at =
                        Some(TokioInstant::now() + Duration::from_secs(AUTO_RESTORE_COOLDOWN_SECS));
                }
                tracing::info!(
                    save_id = %id,
                    "agent: GameStarted — pre-launch sync barrier, pulling latest snapshot"
                );
                spawn_auto_restore(
                    save,
                    api.clone(),
                    events_tx.clone(),
                    cmd_tx.clone(),
                    config.conflict_root.clone(),
                    config.conflict_retention_days,
                    known_version,
                    // Pre-launch barrier wants the freshest truth before play
                    // starts — fetch it rather than trust the poller cache.
                    None,
                    // Single save — no sweep batch to share a manifest with.
                    None,
                );
            }
        } else {
            let was_weak_session = slots
                .get(&id)
                .map(|s| s.weak_session)
                .unwrap_or(false);
            if let Some(slot) = slots.get_mut(&id) {
                slot.is_running = false;
                slot.weak_session = false;
            }
            // Sesión fantasma: arrancó solo por correlación y murió sin UNA
            // escritura en la carpeta. Un juego real escribe al jugar (y cada
            // escritura re-graba la observación y la absuelve), así que esto
            // solo acumula sobre atribuciones envenenadas — el task horario
            // que tuvo a MOUSE "mid-session" durante días. Al segundo strike
            // la observación cae y la señal débil muere con ella.
            if was_weak_session && !had_pending {
                match corr_store.strike_phantom(&local_path) {
                    Some(true) => {
                        tracing::warn!(
                            save_id = %id,
                            game_slug = %game_slug,
                            "agent: observación de correlación descartada — \
                             sesiones fantasma repetidas sin escrituras"
                        );
                        if let Some(p) = corr_path {
                            if let Err(e) = corr_store.save(p) {
                                tracing::debug!(error = %e, "agent: failed to persist correlation store");
                            }
                        }
                    }
                    Some(false) => {
                        tracing::info!(
                            save_id = %id,
                            game_slug = %game_slug,
                            "agent: sesión fantasma (correlación sin escrituras) — strike a la observación"
                        );
                    }
                    None => {}
                }
            } else if had_pending {
                // La sesión escribió: la atribución es legítima; borra strikes.
                corr_store.absolve(&local_path);
            }
            tracing::info!(
                save_id = %id,
                game_slug = %game_slug,
                had_pending,
                "agent: GameStopped"
            );
            let _ = events_tx.try_send(AgentEvent::GameStopped {
                save_id: id.clone(),
                game_slug,
            });
            // Final flush on GameStopped *only* if something changed since
            // the last successful backup — avoids re-uploading an identical
            // snapshot every time the user quits.
            if had_pending {
                schedule_backup(
                    slots,
                    &id,
                    BackupReason::GameStopped,
                    Duration::from_secs(2),
                    api,
                    events_tx,
                    config,
                    done_tx,
                    cmd_tx,
                );
                // Any pull deferred during the session waits for this flush's
                // `BackupDone`: pulling now would fight the upload over the
                // same files, and `has_pending` would veto it anyway. The
                // licence is one-shot — `done_rx` takes it whether or not the
                // pull fires.
                if let Some(slot) = slots.get_mut(&id) {
                    slot.pull_after_flush = true;
                }
            } else {
                tracing::debug!(
                    save_id = %id,
                    "agent: GameStopped with no pending changes; skipping backup"
                );
                // Nothing to flush, so the folder is already quiet: an update
                // that arrived mid-session can land right now.
                run_deferred_pull(slots, &id, api, events_tx, cmd_tx, config, latest_versions);
            }
        }
    }

    // PLAYTIME: vuelca a disco (inmediato al parar un juego, throttled si no).
    if any_stop {
        playtime.flush(playtime_path, now_ms);
    } else {
        playtime.flush_if_due(playtime_path, now_ms);
    }

    // Un juego de Steam no rastreado que corre también mantiene la cadencia
    // rápida: si no, el intervalo idle podría superar el cap de `accrue` y
    // subcontar sus horas.
    !running.is_empty() || !steam_running.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The escalation the July-2026 incident needed: 60s → 5min → 15min → 60min,
    /// then flat. The first step must stay at the old flat cooldown so a
    /// one-off blip recovers exactly as fast as it did before.
    #[test]
    fn auto_restore_backoff_escalates_then_caps() {
        assert_eq!(auto_restore_backoff(1), Duration::from_secs(60));
        assert_eq!(auto_restore_backoff(2), Duration::from_secs(5 * 60));
        assert_eq!(auto_restore_backoff(3), Duration::from_secs(15 * 60));
        assert_eq!(auto_restore_backoff(4), Duration::from_secs(60 * 60));
        // Capped, not unbounded: a save that fails all day is retried hourly,
        // never parked for days.
        assert_eq!(auto_restore_backoff(5), Duration::from_secs(60 * 60));
        assert_eq!(auto_restore_backoff(99), Duration::from_secs(60 * 60));
        // 0 shouldn't happen (the counter is 1-based) but must not panic on
        // the index maths.
        assert_eq!(auto_restore_backoff(0), Duration::from_secs(60));
    }

    /// The whole point of the fix: repeated failures on the same version pace
    /// themselves instead of re-downloading gigabytes every minute forever.
    #[test]
    fn record_failure_escalates_on_same_version() {
        let mut f = AutoRestoreFailures::default();
        let delays: Vec<u64> = (0..5)
            .map(|_| f.record_failure(Some(7)).0.as_secs())
            .collect();
        assert_eq!(delays, vec![60, 300, 900, 3600, 3600]);
        assert_eq!(f.consecutive, 5);
        assert_eq!(f.version, Some(7));
    }

    /// A successful attempt wipes the slate: the next unrelated hiccup starts
    /// from 60s again rather than inheriting an hour-long penalty.
    #[test]
    fn success_resets_the_escalation() {
        let mut f = AutoRestoreFailures::default();
        f.record_failure(Some(7));
        f.record_failure(Some(7));
        assert_eq!(f.consecutive, 2);

        assert!(!f.clear(), "no warning was up, so nothing to recover from");
        assert_eq!(f.consecutive, 0);
        assert_eq!(f.version, None);
        assert!(!f.is_active());

        // Back to the bottom of the ladder.
        assert_eq!(f.record_failure(Some(7)).0, Duration::from_secs(60));
    }

    /// A new cloud version is a fresh reason to try now: it must not inherit the
    /// old version's backoff. This is compared by *version*, not elapsed time —
    /// a save stuck on v7 is still stuck on v7 an hour later.
    #[test]
    fn new_version_resets_the_escalation() {
        let mut f = AutoRestoreFailures::default();
        for _ in 0..4 {
            f.record_failure(Some(7));
        }
        assert_eq!(f.consecutive, 4);

        // v8 landed: start over at 60s instead of the 60min v7 had earned.
        let (delay, _) = f.record_failure(Some(8));
        assert_eq!(delay, Duration::from_secs(60));
        assert_eq!(f.consecutive, 1);
        assert_eq!(f.version, Some(8));
    }

    /// The stuck warning fires once per (save, version) — the sweep keeps
    /// retrying and re-failing, but the user is told a single time. A warning
    /// re-emitted on every retry is just the toast spam this replaces.
    #[test]
    fn stuck_event_is_one_shot_per_version() {
        let mut f = AutoRestoreFailures::default();
        // Below the threshold: still plausibly transient, stay quiet.
        assert!(!f.record_failure(Some(7)).1);
        assert!(!f.record_failure(Some(7)).1);
        // Third strike on the same version: surface it.
        assert!(f.record_failure(Some(7)).1);
        // …and never again for this version, however long it keeps failing.
        for _ in 0..10 {
            assert!(!f.record_failure(Some(7)).1);
        }
        assert!(f.stuck_notified);
    }

    /// A new version re-arms the one-shot: if v8 also fails three times, that's
    /// a new fact about a new version and worth saying again.
    #[test]
    fn stuck_event_rearms_on_new_version() {
        let mut f = AutoRestoreFailures::default();
        for _ in 0..3 {
            f.record_failure(Some(7));
        }
        assert!(f.stuck_notified);

        // v8: the warning clears and the count restarts.
        assert!(!f.record_failure(Some(8)).1);
        assert!(!f.stuck_notified);
        assert!(!f.record_failure(Some(8)).1);
        assert!(f.record_failure(Some(8)).1);
    }

    /// Recovering from a stuck state reports it, so the frontends can drop the
    /// persistent badge. A warning that can't clear itself trains users to
    /// ignore warnings.
    #[test]
    fn clear_reports_whether_a_warning_was_up() {
        let mut f = AutoRestoreFailures::default();
        for _ in 0..AUTO_RESTORE_STUCK_AFTER {
            f.record_failure(Some(7));
        }
        assert!(f.is_active());
        assert!(f.clear(), "was stuck → the UI has a badge to take down");
        assert!(!f.is_active());
        assert!(!f.clear(), "already clean → nothing to announce");
    }

    /// Self-hosted (and the window before the first poll) has no cloud version
    /// cache, so every attempt reports `None`. That must still escalate — the
    /// unknown version is a stable key, not a reason to reset every time.
    #[test]
    fn unknown_version_still_escalates() {
        let mut f = AutoRestoreFailures::default();
        assert_eq!(f.record_failure(None).0, Duration::from_secs(60));
        assert_eq!(f.record_failure(None).0, Duration::from_secs(300));
        assert_eq!(f.consecutive, 2);
    }

    #[test]
    fn canon_token_unifies_slug_name_and_exe() {
        // Las tres formas del mismo juego colapsan a un token.
        assert_eq!(canon_token("victoria-3"), "victoria3");
        assert_eq!(canon_token("Victoria 3"), "victoria3");
        assert_eq!(canon_token("victoria3.exe"), "victoria3exe");
        assert_eq!(canon_token("stellaris"), "stellaris");
    }

    #[test]
    fn game_tokens_drop_short_and_dedup() {
        // Slug y nombre visible que colapsan al mismo token → uno solo.
        assert_eq!(
            game_identity_tokens("stellaris", "Stellaris"),
            ["stellaris"]
        );
        // Token demasiado corto se descarta (colisiona con cualquier carpeta).
        assert!(game_identity_tokens("gta", "GTA").is_empty());
    }

    #[test]
    fn process_matches_game_by_exe_basename() {
        // Caso Stellaris/Victoria: el exe lleva el nombre del juego.
        let cands = process_identity_candidates(
            "victoria3",
            Some(Path::new(
                "/home/u/.steam/steamapps/common/Victoria 3/binaries/victoria3",
            )),
        );
        assert!(cands.contains(&"victoria3".to_string()));
    }

    #[test]
    fn process_matches_game_by_install_folder() {
        // Exe abreviado (`witcher3`) pero la CARPETA lleva el nombre completo:
        // el slug casa por el componente de ruta, no por el basename.
        let cands = process_identity_candidates(
            "witcher3",
            Some(Path::new(
                "/games/GOG Games/The Witcher 3 Wild Hunt/bin/x64/witcher3.exe",
            )),
        );
        assert!(cands.contains(&canon_token("the-witcher-3-wild-hunt")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn open_handle_detects_process_holding_a_save_file() {
        // Un fichero abierto dentro de la carpeta de save delata al proceso
        // dueño, sin depender del nombre del exe (caso EU5).
        let dir = tempfile::tempdir().unwrap();
        let save = dir.path().join("Save Games");
        std::fs::create_dir_all(&save).unwrap();
        let f = std::fs::File::create(save.join("autosave.sav")).unwrap();
        let pid = Pid::from_u32(std::process::id());
        let folders: Vec<(&str, &Path)> = vec![("save-eu5", save.as_path())];
        let hits = open_paths_matching(pid, &folders);
        assert!(hits.contains(&"save-eu5".to_string()));
        drop(f);
        // Cerrado el fichero, ya no casa.
        let hits2 = open_paths_matching(pid, &folders);
        assert!(!hits2.contains(&"save-eu5".to_string()));
    }

    #[test]
    fn generic_identity_ignores_unrelated_process() {
        // Un proceso sin relación no produce el token del juego.
        let cands =
            process_identity_candidates("firefox", Some(Path::new("/usr/lib/firefox/firefox")));
        assert!(!cands.contains(&"stellaris".to_string()));
    }

    #[test]
    fn generic_and_profile_tokens_are_vetoed() {
        // Caso real jul-2026: un save quedó rastreado con slug = username de
        // Windows ("jacka"). El username es componente de ruta de TODO exe del
        // perfil, así que cualquier app disparaba "estás jugando". Los tokens
        // de fontanería no pueden ser identidad ni de juego ni de proceso.
        for t in ["users", "appdata", "roaming", "locallow", "savedgames", "games"] {
            assert!(is_generic_identity_token(t), "{t} debería vetarse");
        }
        assert!(!is_generic_identity_token("eldenring"));
        assert!(!is_generic_identity_token("mousepiforhire"));
        // Del lado del juego: un slug degenerado no produce tokens…
        assert!(game_identity_tokens("games", "Saved Games").is_empty());
        // …y del lado del proceso, los componentes del perfil no salen como
        // candidatos (el exe y su carpeta de instalación sí). Ruta con
        // separador nativo: los componentes sólo se extraen así.
        let cands = process_identity_candidates(
            "game.exe",
            Some(Path::new("/Users/bob/AppData/Roaming/GSE Saves/game.exe")),
        );
        assert!(!cands.iter().any(|c| c == "users" || c == "appdata" || c == "roaming"));
        assert!(cands.contains(&"gsesaves".to_string()));
        // Un juego normal conserva su identidad por carpeta de instalación.
        let cands = process_identity_candidates(
            "witcher3",
            Some(Path::new(
                "/games/GOG Games/The Witcher 3 Wild Hunt/bin/x64/witcher3.exe",
            )),
        );
        assert!(cands.contains(&canon_token("the-witcher-3-wild-hunt")));
    }

    #[test]
    fn config_defaults_are_sane() {
        let c = AgentConfig::default();
        assert!(c.debounce_secs >= 5, "too eager");
        assert!(c.debounce_secs <= 120, "too sleepy");
        assert!(c.poll_secs >= 1);
        assert!(c.max_retries >= 1);
    }

    #[test]
    fn probe_seeds_baseline_then_reports_writes() {
        let dir = tempfile::tempdir().unwrap();
        let cand = dir.path().to_path_buf();
        std::fs::write(cand.join("save1.zip"), b"a").unwrap();

        let mut probes: HashMap<PathBuf, Option<std::time::SystemTime>> = HashMap::new();
        probes.insert(cand.clone(), None);

        // Primer tick: sólo siembra el baseline, no reporta nada.
        assert!(probe_detect_writes(&mut probes).is_empty());
        assert!(probes[&cand].is_some());

        // Una escritura posterior (mtime mayor) sí se reporta.
        let later = std::time::SystemTime::now() + Duration::from_secs(120);
        filetime::set_file_mtime(
            cand.join("save1.zip"),
            filetime::FileTime::from_system_time(later),
        )
        .unwrap();
        let written = probe_detect_writes(&mut probes);
        assert_eq!(written, vec![cand.clone()]);

        // Sin nuevos cambios: silencio.
        assert!(probe_detect_writes(&mut probes).is_empty());
    }

    // Los tests de `accept_correlation_signals` (incluida la regresión de
    // horas-fantasma del corpus D.4) se movieron con la función al kernel
    // leaf: `hoard_core::kernel::correlation::tests`.

    #[test]
    fn match_save_for_path_finds_exact_and_subpath() {
        let save = WatchedSave {
            save_id: "abc".into(),
            game_slug: "stardew-valley".into(),
            display_name: "Stardew Valley".into(),
            label: "main".into(),
            local_path: PathBuf::from("/tmp/saves/stardew"),
            steam_install_dir: None,
            processes: vec![],
            policy: Default::default(),
            known_version: None,
            set_hash: None,
            track_only: false,
        };
        let mut slots = HashMap::new();
        slots.insert(
            "abc".to_string(),
            SaveSlot {
                save,
                watcher: None,
                pending: None,
                is_running: false,
                weak_session: false,
                last_running_seen: None,
                has_pending: false,
                last_fs_event_at: None,
                last_restore_at: None,
                next_scheduled_backup_at: None,
                first_pending_event_at: None,
                last_backup_at: None,
                restoring: false,
                next_auto_restore_at: None,
                restore_failures: AutoRestoreFailures::default(),
                last_set_hash: None,
                known_version: None,
                pull_pending: false,
                pull_after_flush: false,
                deferred_notified: false,
            },
        );

        assert_eq!(
            match_save_for_path(&slots, Path::new("/tmp/saves/stardew")),
            Some("abc".into())
        );
        assert_eq!(
            match_save_for_path(&slots, Path::new("/tmp/saves/stardew/farm")),
            Some("abc".into())
        );
        assert_eq!(
            match_save_for_path(&slots, Path::new("/tmp/saves/other")),
            None
        );
    }

    /// Quiet slot over `path`: no process, nothing pending, no fs history.
    /// Whether it reads as mid-session then depends only on the disk-mtime
    /// fallback (i.e. on `path`'s own mtime).
    fn quiet_slot(path: &Path) -> SaveSlot {
        SaveSlot {
            save: WatchedSave {
                save_id: "mid-session-test".into(),
                game_slug: "r-e-p-o".into(),
                display_name: "R.E.P.O.".into(),
                label: "main".into(),
                local_path: path.to_path_buf(),
                steam_install_dir: None,
                processes: vec![],
                policy: Default::default(),
                known_version: None,
                set_hash: None,
                track_only: false,
            },
            watcher: None,
            pending: None,
            is_running: false,
            weak_session: false,
            last_running_seen: None,
            has_pending: false,
            last_fs_event_at: None,
            last_restore_at: None,
            next_scheduled_backup_at: None,
            first_pending_event_at: None,
            last_backup_at: None,
            restoring: false,
            next_auto_restore_at: None,
            restore_failures: AutoRestoreFailures::default(),
            last_set_hash: None,
            known_version: None,
            pull_pending: false,
            pull_after_flush: false,
            deferred_notified: false,
        }
    }

    /// The guard shared by the sweep, the push force-restore and the launch
    /// barrier (REPO data-loss regression, 2026-07-05): any live-session
    /// signal must veto a pull; a genuinely quiet slot must not.
    #[test]
    fn mid_session_reason_flags_live_session_signals() {
        let tmp = tempfile::tempdir().unwrap();
        // Age the folder so the disk-mtime fallback doesn't trip by itself.
        let old = std::time::SystemTime::now() - Duration::from_secs(3600);
        filetime::set_file_mtime(tmp.path(), filetime::FileTime::from_system_time(old)).unwrap();

        let mut slot = quiet_slot(tmp.path());
        assert_eq!(
            mid_session_reason(&slot),
            None,
            "quiet slot must be pullable"
        );

        slot.is_running = true;
        assert!(
            mid_session_reason(&slot).is_some(),
            "running game must veto"
        );
        slot.is_running = false;

        slot.has_pending = true;
        assert!(
            mid_session_reason(&slot).is_some(),
            "un-flushed changes must veto"
        );
        slot.has_pending = false;

        slot.last_fs_event_at = Some(OffsetDateTime::now_utc());
        assert!(
            mid_session_reason(&slot).is_some(),
            "recent fs event must veto"
        );
        slot.last_fs_event_at = Some(OffsetDateTime::now_utc() - time::Duration::hours(1));
        assert_eq!(
            mid_session_reason(&slot),
            None,
            "an hour-old fs event is outside the grace window"
        );
    }

    #[test]
    fn mid_session_reason_ignores_own_restore_touch() {
        // A freshly-created tempdir has mtime ≈ now, so it reads as
        // "save folder touched recently" — the veto a restore trips on itself.
        let tmp = tempfile::tempdir().unwrap();
        let mut slot = quiet_slot(tmp.path());
        assert_eq!(
            mid_session_reason(&slot),
            Some("save folder touched recently"),
            "a just-touched folder vetoes by default"
        );
        // Stamp a recent restore: the touch is ours, so the next pull isn't vetoed.
        slot.last_restore_at = Some(OffsetDateTime::now_utc());
        assert_eq!(
            mid_session_reason(&slot),
            None,
            "our own recent restore must not veto the next pull"
        );
        // A genuine un-flushed user change still wins (checked before the gate).
        slot.has_pending = true;
        assert_eq!(
            mid_session_reason(&slot),
            Some("un-flushed local changes pending"),
            "a real pending change still vetoes despite the restore stamp"
        );
        slot.has_pending = false;
        // A stale restore stamp no longer suppresses the touch veto.
        slot.last_restore_at = Some(OffsetDateTime::now_utc() - time::Duration::hours(1));
        assert_eq!(
            mid_session_reason(&slot),
            Some("save folder touched recently"),
            "a restore older than the grace window stops covering the touch"
        );
    }

    /// A Proton game that dies badly leaves its .exe defunct, keeping the name
    /// and exe path every strong matcher keys on. Nothing about a zombie says
    /// "the user is playing" — it can't write a save file — so it must never
    /// hold a slot `is_running`, which is what pinned the mid-session veto open
    /// and stranded cross-device updates on the Deck.
    #[test]
    fn defunct_processes_are_not_evidence_of_a_live_session() {
        assert!(is_defunct(ProcessStatus::Zombie), "exited, not yet reaped");
        assert!(is_defunct(ProcessStatus::Dead));

        assert!(!is_defunct(ProcessStatus::Run));
        assert!(!is_defunct(ProcessStatus::Sleep));
        // A Paradox game sitting in a menu burns no CPU and reads as Idle —
        // very much a live session.
        assert!(!is_defunct(ProcessStatus::Idle));
        // SIGSTOP'd (or suspended): it can resume and write at any moment.
        assert!(!is_defunct(ProcessStatus::Stop));
    }

    /// Pins the assumption `is_defunct` rests on: our minimal
    /// `proc_refresh_kind` really does populate `status()`, and a genuine
    /// unreaped child really does read as defunct through it. If sysinfo ever
    /// puts `status` behind a refresh flag, the zombie filter would silently
    /// go back to matching leftovers — this fails instead.
    #[cfg(target_os = "linux")]
    #[test]
    fn sysinfo_reports_an_unreaped_child_as_defunct() {
        // Exits immediately; we're its parent and don't reap until the end, so
        // it lingers in the process table exactly like Proton's leftovers.
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn a short-lived child");
        let pid = Pid::from_u32(child.id());
        let mut sys =
            System::new_with_specifics(RefreshKind::new().with_processes(proc_refresh_kind()));

        let mut saw_defunct = false;
        for _ in 0..50 {
            sys.refresh_processes_specifics(ProcessesToUpdate::All, true, proc_refresh_kind());
            if let Some(p) = sys.process(pid) {
                if is_defunct(p.status()) {
                    saw_defunct = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let _ = child.wait();
        assert!(
            saw_defunct,
            "an unreaped exited child must read as defunct through the agent's refresh kind"
        );
    }

    /// Restore-enabled config, matching a device with sync global on.
    fn restore_config() -> AgentConfig {
        AgentConfig {
            auto_restore: true,
            ..Default::default()
        }
    }

    /// The Steam Deck bug (a save from device A never appearing on device B):
    /// a pull vetoed mid-session must be remembered, must stay vetoed while the
    /// user is actually playing, and must land the moment the game closes —
    /// without waiting out the recency guards, which the exiting save's own
    /// write always trips.
    #[test]
    fn deferred_pull_survives_the_veto_and_lands_when_the_game_closes() {
        let tmp = tempfile::tempdir().unwrap();
        let config = restore_config();
        let latest = HashMap::new();
        let mut slot = quiet_slot(tmp.path());

        // The poller confirmed a bump while the game is running: veto, and
        // remember it instead of dropping it.
        slot.is_running = true;
        assert!(
            mid_session_reason(&slot).is_some(),
            "a live game must veto the pull"
        );
        assert!(note_deferred_pull(&mut slot), "the first veto notifies");
        assert!(
            !note_deferred_pull(&mut slot),
            "later ticks must not re-notify — the sweep re-checks every tick"
        );
        assert!(slot.pull_pending);
        assert!(
            !deferred_pull_ready(&slot, &config, &latest),
            "never pull into a folder the user is playing in"
        );

        // The game closed but its final flush is still queued: the changes
        // aren't versioned yet, so a pull could still overwrite them.
        slot.is_running = false;
        slot.has_pending = true;
        assert!(!deferred_pull_ready(&slot, &config, &latest));

        // Flush landed. The fs event and the folder's mtime are seconds old —
        // that write *is* the exit save, already uploaded, and skipping those
        // two guards here is the whole point: nothing else would re-fire.
        slot.has_pending = false;
        slot.last_fs_event_at = Some(OffsetDateTime::now_utc());
        assert!(
            mid_session_reason(&slot).is_some(),
            "the recency guards still veto the sweep"
        );
        assert!(
            deferred_pull_ready(&slot, &config, &latest),
            "the stop transition pulls anyway"
        );

        // A restore already writing into the folder defers again.
        slot.restoring = true;
        assert!(!deferred_pull_ready(&slot, &config, &latest));
        slot.restoring = false;

        // Nothing waiting, nothing to do.
        slot.pull_pending = false;
        assert!(!deferred_pull_ready(&slot, &config, &latest));
    }

    /// Two more ways in: the poller's cache showing the cloud ahead arms the
    /// stop transition on its own (no veto need have been recorded), and a
    /// backup-only save never pulls no matter what's waiting.
    #[test]
    fn deferred_pull_reads_the_version_cache_and_honours_backup_only() {
        let tmp = tempfile::tempdir().unwrap();
        let config = restore_config();
        let mut slot = quiet_slot(tmp.path());
        let id = slot.save.save_id.clone();
        slot.known_version = Some(4);

        let mut latest = HashMap::new();
        assert!(
            !deferred_pull_ready(&slot, &config, &latest),
            "no cache entry is not evidence the cloud moved"
        );

        latest.insert(id.clone(), 4);
        assert!(
            !deferred_pull_ready(&slot, &config, &latest),
            "same version — nothing to pull"
        );

        latest.insert(id, 5);
        assert!(
            deferred_pull_ready(&slot, &config, &latest),
            "another device committed v5 while we sat at v4"
        );

        slot.save.policy.auto_restore = Some(false);
        assert!(
            !deferred_pull_ready(&slot, &config, &latest),
            "a backup-only save must never be pulled into"
        );
    }

    #[test]
    fn mid_session_reason_falls_back_to_disk_mtime() {
        // Fresh tempdir → dir mtime = now → the startup-window fallback trips
        // even with no fs history of our own.
        let tmp = tempfile::tempdir().unwrap();
        let slot = quiet_slot(tmp.path());
        assert!(mid_session_reason(&slot).is_some());
    }

    /// Regression for the "watcher only arms on GameStarted" bug.
    /// A save with no `processes` and no `steam_install_dir` should still
    /// trigger a debounced backup when its folder changes — even with no
    /// game process running. Today this fails: `handle_add` doesn't arm
    /// the watcher and `process_poll` never finds a matching process, so
    /// the fs event is never observed.
    #[tokio::test(flavor = "current_thread")]
    async fn fs_event_triggers_backup_without_game_running() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let save_path = tmp.path().to_path_buf();

        let api = ApiClient::new("http://127.0.0.1:1", "fake").expect("fake api client");
        let (events_tx, mut events_rx) = mpsc::channel::<AgentEvent>(64);

        let save = WatchedSave {
            save_id: "watcher-bug-1".into(),
            game_slug: "fake-game".into(),
            display_name: "Fake Game".into(),
            label: "main".into(),
            local_path: save_path.clone(),
            steam_install_dir: None,
            processes: vec![],
            policy: Default::default(),
            known_version: None,
            set_hash: None,
            track_only: false,
        };

        // Short debounce so the test completes well under the 10s timeout.
        let config = AgentConfig {
            debounce_secs: 1,
            poll_secs: 2,
            max_retries: 0,
            auto_restore: false,
            global_sync: false,
            conflict_root: None,
            conflict_retention_days: 14,
            min_snapshot_interval_secs: 0,
        };

        let (handle, task) = spawn(api, config, vec![save], events_tx);

        // Give the agent a beat to register the save before we touch the
        // folder — otherwise the fs event could land before `AddSave` is
        // processed.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Touch a file inside the watched directory.
        let mut f = std::fs::File::create(save_path.join("save.dat")).expect("create save file");
        f.write_all(b"hello").expect("write save file");
        f.sync_all().expect("sync save file");
        drop(f);

        // Wait for BackupScheduled within 10s. If the bug is present this
        // times out because no watcher is ever armed.
        let scheduled = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(evt) = events_rx.recv().await {
                if let AgentEvent::BackupScheduled { save_id, .. } = evt {
                    return save_id;
                }
            }
            "<channel closed>".to_string()
        })
        .await;

        // Best-effort teardown before asserting so the task doesn't leak.
        let _ = handle.shutdown().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

        let save_id = scheduled.expect(
            "timed out waiting for BackupScheduled — the fs watcher never armed for an idle save",
        );
        assert_eq!(save_id, "watcher-bug-1");
    }

    /// A backup that burns its whole retry budget used to leave the slot in a
    /// corner it could never climb out of: no `BackupDone` (correctly — the
    /// changes never reached a version, so `has_pending` must stay set to keep
    /// restores off them), but also nothing that would ever try the upload
    /// again. `has_pending` is itself a mid-session veto, so the save could
    /// neither be pushed nor pulled until the user happened to write the folder
    /// again. The task must hand a retry back to the agent loop.
    #[tokio::test(flavor = "current_thread")]
    async fn exhausted_backup_retries_hand_back_a_retry_and_keep_changes_pending() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(&tmp.path().join("save.dat"), b"unversioned progress");

        // Port 1 refuses instantly: a real failure, not a throttle or a 413.
        let api = ApiClient::new("http://127.0.0.1:1", "fake").expect("fake api client");
        let (events_tx, mut events_rx) = mpsc::channel::<AgentEvent>(64);
        let (done_tx, mut done_rx) = mpsc::channel::<BackupDone>(8);
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<AgentCommand>(8);

        let save = WatchedSave {
            save_id: "wedged-1".into(),
            game_slug: "fake-game".into(),
            display_name: "Fake Game".into(),
            label: "main".into(),
            local_path: tmp.path().to_path_buf(),
            steam_install_dir: None,
            processes: vec![],
            policy: Default::default(),
            known_version: None,
            set_hash: None,
            track_only: false,
        };

        // `max_retries: 0` → the first failure is already the last.
        run_backup_with_retry(
            api, save, None, None, events_tx, done_tx, cmd_tx, 0, false, None, 14,
        )
        .await;

        let mut failed = false;
        while let Ok(ev) = events_rx.try_recv() {
            if let AgentEvent::BackupFailed { will_retry, .. } = ev {
                assert!(!will_retry, "the budget is spent");
                failed = true;
            }
        }
        assert!(failed, "a real failure must reach the feed");

        assert!(
            done_rx.try_recv().is_err(),
            "no BackupDone: clearing has_pending would let a restore overwrite \
             changes that were never versioned"
        );
        assert!(
            matches!(
                cmd_rx.try_recv(),
                Ok(AgentCommand::RetryBackupAfterFailure(id)) if id == "wedged-1"
            ),
            "the slot must get a retry path that doesn't depend on a new fs event"
        );
    }

    /// Helper for the diff-restore tests: write `contents` to `path`
    /// creating parent dirs as needed.
    fn write_file(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    /// Source has A, B, C. Target has only A (identical to source). The
    /// diff restore copies B and C and leaves A alone.
    #[tokio::test(flavor = "current_thread")]
    async fn restore_files_into_copies_missing_files() {
        let source_tmp = tempfile::tempdir().unwrap();
        let target_tmp = tempfile::tempdir().unwrap();
        let source = source_tmp.path();
        let target = target_tmp.path();

        write_file(&source.join("a.dat"), b"alpha");
        write_file(&source.join("b.dat"), b"beta");
        write_file(&source.join("nested/c.dat"), b"gamma");
        write_file(&target.join("a.dat"), b"alpha");

        let stats = restore_files_into(target, source, None).await.unwrap();

        assert_eq!(stats.restored, 2, "B and C should be copied");
        assert_eq!(stats.skipped, 1, "A is identical, skipped silently");
        assert_eq!(stats.conflicts_resolved_remote, 0);
        assert_eq!(stats.conflicts_resolved_local, 0);
        assert_eq!(stats.conflicts_backed_up, 0);
        assert_eq!(
            stats.bytes_restored,
            (b"beta".len() + b"gamma".len()) as u64
        );

        // Local A untouched.
        assert_eq!(std::fs::read(target.join("a.dat")).unwrap(), b"alpha");
        // B and C now present locally with source contents.
        assert_eq!(std::fs::read(target.join("b.dat")).unwrap(), b"beta");
        assert_eq!(
            std::fs::read(target.join("nested/c.dat")).unwrap(),
            b"gamma"
        );
    }

    /// Local-only files: a file present in the target but absent from the
    /// remote snapshot is left untouched and counted under `target_only`.
    /// This is the divergence signal the conflict/auto-restore path keys on
    /// to decide it must re-upload rather than settle — getting it wrong
    /// would silently drop local data, so pin the count explicitly.
    #[tokio::test(flavor = "current_thread")]
    async fn restore_files_into_counts_local_only_files() {
        let source_tmp = tempfile::tempdir().unwrap();
        let target_tmp = tempfile::tempdir().unwrap();
        let source = source_tmp.path();
        let target = target_tmp.path();

        // Remote snapshot has a.dat; target has the same a.dat plus two files
        // the snapshot knows nothing about (one nested).
        write_file(&source.join("a.dat"), b"alpha");
        write_file(&target.join("a.dat"), b"alpha");
        write_file(&target.join("local-only.sav"), b"unsynced");
        write_file(&target.join("nested/also-local.sav"), b"more");

        let stats = restore_files_into(target, source, None).await.unwrap();

        assert_eq!(stats.restored, 0, "a.dat is identical, nothing copied");
        assert_eq!(stats.skipped, 1, "a.dat skipped");
        assert_eq!(
            stats.target_only, 2,
            "two files exist locally but not in the snapshot"
        );
        assert_eq!(stats.conflicts_resolved_local, 0);
        assert_eq!(stats.conflicts_resolved_remote, 0);
        // Local-only files are never deleted by a restore.
        assert_eq!(
            std::fs::read(target.join("local-only.sav")).unwrap(),
            b"unsynced"
        );
        assert_eq!(
            std::fs::read(target.join("nested/also-local.sav")).unwrap(),
            b"more"
        );
    }

    /// Mirror image: when the target is a strict subset of the snapshot
    /// (everything local also exists remotely), `target_only` is zero — the
    /// signal that a purely-behind device can settle without re-uploading.
    #[tokio::test(flavor = "current_thread")]
    async fn restore_files_into_no_local_only_when_target_is_subset() {
        let source_tmp = tempfile::tempdir().unwrap();
        let target_tmp = tempfile::tempdir().unwrap();
        let source = source_tmp.path();
        let target = target_tmp.path();

        write_file(&source.join("a.dat"), b"alpha");
        write_file(&source.join("sub/b.dat"), b"beta");
        // Target only has a.dat (subset); b.dat will be copied in.
        write_file(&target.join("a.dat"), b"alpha");

        let stats = restore_files_into(target, source, None).await.unwrap();

        assert_eq!(stats.restored, 1, "b.dat copied");
        assert_eq!(stats.skipped, 1, "a.dat identical");
        assert_eq!(stats.target_only, 0, "no file exists only locally");
    }

    /// Conflict case: A exists in both source and target but bytes differ.
    /// The local copy wins — bytes on disk stay as the target's version
    /// and the conflict is reported in stats.
    #[tokio::test(flavor = "current_thread")]
    async fn restore_files_into_preserves_local_on_conflict() {
        let source_tmp = tempfile::tempdir().unwrap();
        let target_tmp = tempfile::tempdir().unwrap();
        let source = source_tmp.path();
        let target = target_tmp.path();

        write_file(&source.join("a.dat"), b"remote-version");
        write_file(&target.join("a.dat"), b"LOCAL-WORK");

        let stats = restore_files_into(target, source, None).await.unwrap();

        assert_eq!(stats.restored, 0, "nothing copied — A is a conflict");
        assert_eq!(stats.skipped, 0);
        // No conflict_backup_dir → fallback to "keep local" regardless of
        // mtime, accounted under `conflicts_resolved_local`.
        assert_eq!(stats.conflicts_resolved_local, 1);
        assert_eq!(stats.conflicts_resolved_remote, 0);
        assert_eq!(stats.conflicts_backed_up, 0);
        assert_eq!(stats.bytes_restored, 0);
        // Local content preserved verbatim.
        assert_eq!(std::fs::read(target.join("a.dat")).unwrap(), b"LOCAL-WORK");
    }

    /// Everything identical between source and target: zero restores, zero
    /// conflicts, just `skipped` accounting. The agent uses
    /// `restored == 0 && conflicts == 0` to keep the activity feed quiet.
    #[tokio::test(flavor = "current_thread")]
    async fn restore_files_into_silent_when_all_identical() {
        let source_tmp = tempfile::tempdir().unwrap();
        let target_tmp = tempfile::tempdir().unwrap();
        let source = source_tmp.path();
        let target = target_tmp.path();

        write_file(&source.join("a.dat"), b"alpha");
        write_file(&source.join("sub/b.dat"), b"beta");
        write_file(&target.join("a.dat"), b"alpha");
        write_file(&target.join("sub/b.dat"), b"beta");

        let stats = restore_files_into(target, source, None).await.unwrap();

        assert_eq!(stats.restored, 0);
        assert_eq!(stats.skipped, 2);
        assert_eq!(stats.conflicts_resolved_remote, 0);
        assert_eq!(stats.conflicts_resolved_local, 0);
        assert_eq!(stats.conflicts_backed_up, 0);
        assert_eq!(stats.bytes_restored, 0);
    }

    /// Empty target dir: every file in source gets copied, no conflicts.
    /// Mirrors the "agent boots, save folder was wiped" scenario.
    #[tokio::test(flavor = "current_thread")]
    async fn restore_files_into_full_restore_when_target_empty() {
        let source_tmp = tempfile::tempdir().unwrap();
        let target_tmp = tempfile::tempdir().unwrap();
        let source = source_tmp.path();
        let target = target_tmp.path();

        write_file(&source.join("a.dat"), b"alpha");
        write_file(&source.join("b.dat"), b"beta-bytes");
        write_file(&source.join("deep/nested/c.dat"), b"gamma!");

        let stats = restore_files_into(target, source, None).await.unwrap();

        assert_eq!(stats.restored, 3);
        assert_eq!(stats.skipped, 0);
        assert_eq!(stats.conflicts_resolved_remote, 0);
        assert_eq!(stats.conflicts_resolved_local, 0);
        assert_eq!(stats.conflicts_backed_up, 0);
        assert_eq!(
            stats.bytes_restored,
            (b"alpha".len() + b"beta-bytes".len() + b"gamma!".len()) as u64
        );
        assert_eq!(std::fs::read(target.join("a.dat")).unwrap(), b"alpha");
        assert_eq!(
            std::fs::read(target.join("deep/nested/c.dat")).unwrap(),
            b"gamma!"
        );
    }

    /// Helper: set both file mtimes deterministically so the mtime branch
    /// is exercised without relying on test runtime ordering.
    fn set_mtime(path: &Path, mtime: std::time::SystemTime) {
        let ft = filetime::FileTime::from_system_time(mtime);
        filetime::set_file_mtime(path, ft).expect("set mtime");
    }

    /// Remote newer than local + a conflict_backup_dir → remote wins. The
    /// previous local bytes land in `conflict_backup_dir/<rel>` before
    /// being overwritten by the staged remote version.
    #[tokio::test(flavor = "current_thread")]
    async fn restore_files_into_remote_wins_when_remote_newer() {
        let source_tmp = tempfile::tempdir().unwrap();
        let target_tmp = tempfile::tempdir().unwrap();
        let backup_tmp = tempfile::tempdir().unwrap();
        let source = source_tmp.path();
        let target = target_tmp.path();
        let backup = backup_tmp.path();

        write_file(&source.join("a.dat"), b"remote-new");
        write_file(&target.join("a.dat"), b"local-old");
        // local mtime = T-10s, remote mtime = T+10s (clearly newer).
        let now = std::time::SystemTime::now();
        set_mtime(&target.join("a.dat"), now - Duration::from_secs(10));
        set_mtime(&source.join("a.dat"), now + Duration::from_secs(10));

        let stats = restore_files_into(target, source, Some(backup))
            .await
            .unwrap();

        assert_eq!(stats.conflicts_resolved_remote, 1);
        assert_eq!(stats.conflicts_backed_up, 1);
        assert_eq!(stats.conflicts_resolved_local, 0);
        assert_eq!(stats.restored, 0);
        assert_eq!(stats.bytes_restored, b"remote-new".len() as u64);
        // Target now has the remote version.
        assert_eq!(std::fs::read(target.join("a.dat")).unwrap(), b"remote-new");
        // The previous local bytes were parked in the backup root.
        assert_eq!(std::fs::read(backup.join("a.dat")).unwrap(), b"local-old");
    }

    /// Local newer than remote (well past the 1s tolerance) → local wins.
    /// The remote file is *not* applied and no conflict backup is taken.
    #[tokio::test(flavor = "current_thread")]
    async fn restore_files_into_local_wins_when_local_newer() {
        let source_tmp = tempfile::tempdir().unwrap();
        let target_tmp = tempfile::tempdir().unwrap();
        let backup_tmp = tempfile::tempdir().unwrap();
        let source = source_tmp.path();
        let target = target_tmp.path();
        let backup = backup_tmp.path();

        write_file(&source.join("a.dat"), b"remote-old");
        write_file(&target.join("a.dat"), b"LOCAL-WORK");
        let now = std::time::SystemTime::now();
        set_mtime(&source.join("a.dat"), now - Duration::from_secs(60));
        set_mtime(&target.join("a.dat"), now);

        let stats = restore_files_into(target, source, Some(backup))
            .await
            .unwrap();

        assert_eq!(stats.conflicts_resolved_local, 1);
        assert_eq!(stats.conflicts_resolved_remote, 0);
        assert_eq!(stats.conflicts_backed_up, 0);
        assert_eq!(stats.bytes_restored, 0);
        assert_eq!(std::fs::read(target.join("a.dat")).unwrap(), b"LOCAL-WORK");
        // No backup was created — `backup` is still empty.
        assert!(std::fs::read_dir(backup).unwrap().next().is_none());
    }

    /// Files written by the merge must keep the snapshot's mtime, not the
    /// time of the restore. `fs::copy` alone stamps mtime=now, which made
    /// every restored save look brand-new: games that pick "continue" by
    /// most-recent file loaded the wrong save, and the follow-up upload
    /// pushed the inflated mtimes to the server.
    #[tokio::test(flavor = "current_thread")]
    async fn restore_files_into_preserves_snapshot_mtimes() {
        let source_tmp = tempfile::tempdir().unwrap();
        let target_tmp = tempfile::tempdir().unwrap();
        let backup_tmp = tempfile::tempdir().unwrap();
        let source = source_tmp.path();
        let target = target_tmp.path();
        let backup = backup_tmp.path();

        let old = std::time::SystemTime::now() - Duration::from_secs(30 * 24 * 3600);
        // Plain restore: file missing locally.
        write_file(&source.join("fresh.dat"), b"from-cloud");
        set_mtime(&source.join("fresh.dat"), old);
        // Conflict the remote wins: overwrite path.
        write_file(&source.join("clash.dat"), b"remote-new");
        write_file(&target.join("clash.dat"), b"local-old");
        set_mtime(&source.join("clash.dat"), old + Duration::from_secs(20));
        set_mtime(&target.join("clash.dat"), old);

        let stats = restore_files_into(target, source, Some(backup))
            .await
            .unwrap();
        assert_eq!(stats.restored, 1);
        assert_eq!(stats.conflicts_resolved_remote, 1);

        let mtime_of = |p: PathBuf| std::fs::metadata(p).unwrap().modified().unwrap();
        let close = |a: std::time::SystemTime, b: std::time::SystemTime| {
            let d = a.duration_since(b).unwrap_or_else(|e| e.duration());
            d < Duration::from_secs(1)
        };
        assert!(
            close(mtime_of(target.join("fresh.dat")), old),
            "restored file must carry the snapshot mtime, not now()"
        );
        assert!(
            close(
                mtime_of(target.join("clash.dat")),
                old + Duration::from_secs(20)
            ),
            "conflict-overwritten file must carry the snapshot mtime, not now()"
        );
    }

    /// Even with the remote winning by mtime, when `conflict_backup_dir`
    /// is `None` the agent must never overwrite local data. This is the
    /// 1.5.4 fallback for hosts where the conflict root couldn't be
    /// resolved (state_dir missing, permission denied, etc).
    #[tokio::test(flavor = "current_thread")]
    async fn restore_files_into_skips_when_no_backup_dir_provided() {
        let source_tmp = tempfile::tempdir().unwrap();
        let target_tmp = tempfile::tempdir().unwrap();
        let source = source_tmp.path();
        let target = target_tmp.path();

        write_file(&source.join("a.dat"), b"remote-new");
        write_file(&target.join("a.dat"), b"local-old");
        let now = std::time::SystemTime::now();
        set_mtime(&target.join("a.dat"), now - Duration::from_secs(10));
        set_mtime(&source.join("a.dat"), now + Duration::from_secs(10));

        let stats = restore_files_into(target, source, None).await.unwrap();

        assert_eq!(stats.conflicts_resolved_local, 1);
        assert_eq!(stats.conflicts_resolved_remote, 0);
        assert_eq!(stats.conflicts_backed_up, 0);
        // Local content was preserved.
        assert_eq!(std::fs::read(target.join("a.dat")).unwrap(), b"local-old");
    }

    /// `cleanup_old_conflicts` walks two levels deep and removes only the
    /// timestamp dirs older than the retention window. The save_id parent
    /// is left in place even after its children disappear.
    #[tokio::test(flavor = "current_thread")]
    async fn cleanup_old_conflicts_respects_ttl() {
        let root_tmp = tempfile::tempdir().unwrap();
        let root = root_tmp.path();

        let old_dir = root.join("save-A").join("2026-04-01T00-00-00Z");
        let fresh_dir = root.join("save-A").join("2026-05-20T00-00-00Z");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::create_dir_all(&fresh_dir).unwrap();
        std::fs::write(old_dir.join("dummy.txt"), b"x").unwrap();
        std::fs::write(fresh_dir.join("dummy.txt"), b"x").unwrap();

        let now = std::time::SystemTime::now();
        set_mtime(&old_dir, now - Duration::from_secs(30 * 86_400));
        set_mtime(&fresh_dir, now - Duration::from_secs(60));

        cleanup_old_conflicts(root, Duration::from_secs(14 * 86_400))
            .await
            .expect("cleanup ok");

        assert!(
            !old_dir.exists(),
            "old conflict dir should have been pruned"
        );
        assert!(fresh_dir.exists(), "fresh conflict dir must survive");
        assert!(root.join("save-A").exists(), "save_id parent stays");
    }

    /// `cleanup_old_conflicts` on a non-existent root is a no-op, not an
    /// error. Mirrors the fresh-install case where the conflict dir hasn't
    /// been touched yet.
    #[tokio::test(flavor = "current_thread")]
    async fn cleanup_old_conflicts_handles_missing_dir() {
        let parent = tempfile::tempdir().unwrap();
        let missing = parent.path().join("does-not-exist");
        assert!(!missing.exists());
        cleanup_old_conflicts(&missing, Duration::from_secs(14 * 86_400))
            .await
            .expect("missing root must be no-op");
    }
}
