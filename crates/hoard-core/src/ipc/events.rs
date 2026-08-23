//! Contrato de eventos y estado del motor — **el protocolo de eventos por
//! cable** (ADR 0021, Parte A + C.6).
//!
//! Estos tres tipos vivían en `hoard_agent::agent`, donde eran el canal
//! in-process `events_tx` que el desktop y el daemon CLI consumían. Con el
//! Slice 4 el motor se muda a su propio proceso (`hoardd`) y ese canal pasa a
//! cruzar un socket: la ADR lo dice literalmente — «el `AgentEvent` pasa a ser
//! el protocolo de eventos por cable». Un tipo que cruza el cable pertenece al
//! kernel leaf, no al crate del motor, o el cliente tendría que depender del
//! motor entero para leer un evento.
//!
//! El movimiento fue **verbatim**: mismas variantes, mismos campos, mismos
//! atributos `serde`. `hoard_agent::agent` los re-exporta, así que ni el
//! desktop ni la CLI notan el cambio, y [`super::events_wire_shape_is_frozen`]
//! fija la forma del JSON para que nadie la mueva por accidente.
//!
//! ## Disciplina de compatibilidad
//!
//! Igual que [`crate::wire`], pero con una diferencia: aquí las dos puntas son
//! **el daemon y sus clientes**, que se actualizan por separado (el usuario
//! actualiza la app y el servicio de usuario sigue siendo el viejo hasta que
//! reinicia sesión). Por tanto: append-only, `#[serde(default)]` en todo campo
//! nuevo, nunca repurposear un campo, y la versión del protocolo
//! ([`super::PROTOCOL_VERSION`]) sube cuando un cambio no es compatible.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Who refused an upload for being too big — and therefore what the user has to
/// change. See [`AgentEvent::BackupTooLarge`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TooLargeKind {
    /// Hoard Cloud's per-save plan cap. The user upgrades or trims the folder.
    PlanCap,
    /// A self-hosted Hoard's `storage.max_snapshot_size_mb`. The user edits
    /// their server's `config.toml`.
    ServerLimit,
    /// Not Hoard at all: the reply carried no `code`, so it was written by a
    /// reverse proxy or tunnel in front of the server. Nothing in Hoard's
    /// settings will fix it.
    Proxy,
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
        /// **No se subió nada: el contenido ya era la cabeza del server** (ADR
        /// 0021 D.8.3). Pasa cuando el daemon se reinicia con una subida en
        /// vuelo que sí llegó a comprometerse: el `in_flight` en memoria se
        /// perdió, pero los bytes están arriba, y el chequeo content-addressed
        /// lo detecta en vez de crear una versión duplicada.
        ///
        /// Sigue siendo un `BackupSuccess` porque el hecho que le importa a
        /// quien mira es el mismo —"está guardado en la versión N"— y porque de
        /// él cuelga la persistencia de `state.json`. `total_bytes` es 0: no
        /// viajó ni un byte. Campo nuevo con `default`, así que un cliente
        /// anterior lo lee como `false` (ADR 0021 C.6: append-only).
        #[serde(default)]
        already_landed: bool,
        /// La pidió una persona: el botón "copiar ahora" o la red de seguridad
        /// previa a un restore ([`hoard_core::wire::VersionOrigin::is_deliberate`]).
        ///
        /// Existe para el aviso: `notify_on_success` está apagado por defecto a
        /// propósito, porque el motor narrando cada autoguardado es ruido — pero
        /// esa preferencia nunca quiso decir "no contestes cuando pulse un
        /// botón". Sin esto, pulsar "copiar ahora" no daba señal ninguna: ni
        /// aviso, ni error, sólo una fila en el feed que hay que ir a mirar.
        ///
        /// Campo nuevo con `default`, así que un cliente anterior lo lee como
        /// `false` y se comporta como hasta ahora (ADR 0021 C.6: append-only).
        #[serde(default)]
        deliberate: bool,
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
    /// A 413: the upload can never succeed as-is, so retrying is pointless and
    /// would just spam the feed every time the folder changes. Its own event
    /// (not a generic `BackupFailed`) so the UI can show an actionable message
    /// built from the structured fields and mark the save terminal rather than
    /// "reintentando".
    ///
    /// **Three different things answer 413 and the fix differs for each**, so
    /// `kind` decides which sentence the UI shows. Getting this wrong sends the
    /// user to the wrong knob: a self-hoster spent days looking for a Hoard
    /// limit when the answer was nginx's `client_max_body_size` (2026-08-07).
    BackupTooLarge {
        save_id: String,
        game_slug: String,
        label: String,
        kind: TooLargeKind,
        /// Cloud only: the plan whose cap was hit.
        plan: String,
        /// The cap itself. `0` only when the responder wasn't Hoard.
        limit_bytes: u64,
        /// Cloud only: the save's real size, known up front.
        actual_bytes: u64,
        /// Self-hosted only: bytes taken in before the server gave up. A floor,
        /// not the total — it aborts mid-stream and never learns the size.
        received_bytes: u64,
    },
    /// The *account* is out of storage (402 `quota_exceeded`), so this upload —
    /// and every other one — will keep failing until the user frees space or
    /// upgrades. Its own event rather than a `BackupFailed` for two reasons:
    /// nothing about this save is wrong (a red "factorio falló" blames the
    /// wrong thing, and the raw 402 JSON was what actually reached the feed in
    /// ago-2026), and it's account-wide, so the UI collapses every save's
    /// report into one actionable banner that opens "liberar espacio".
    ///
    /// The save keeps its pending changes and re-arms on a long park — the
    /// bytes are still only on disk, so the slot must stay vetoed from restores
    /// until they land.
    BackupQuotaFull {
        save_id: String,
        game_slug: String,
        label: String,
        plan: String,
        used_bytes: u64,
        limit_bytes: u64,
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
    /// The snapshot went up **without some of its files**: their bytes couldn't
    /// be read, so the upload left them out rather than losing the backup of
    /// everything else.
    ///
    /// Exists because the alternative was worse in both directions. Failing the
    /// whole snapshot on one unreadable file is what actually happened until
    /// ago-2026 — a OneDrive Files On-Demand placeholder ("the cloud file
    /// provider is not running") inside a GTA San Andreas save meant 3,934
    /// attempts across 13 days and not one version uploaded — and quietly
    /// shipping an incomplete version is the failure nobody would ever notice
    /// until a restore came back missing a file. So: upload what can be read,
    /// and say out loud what was left behind.
    ///
    /// Fired right after the `BackupSuccess` for the same upload, so the UI's
    /// amber "partial" state wins over the green "ok" — same ordering contract
    /// as [`AgentEvent::BackupTrimmed`]. When `uploaded` is false nothing went
    /// up at all (not a single file was readable) and there is no companion
    /// success event.
    BackupFilesUnreadable {
        save_id: String,
        game_slug: String,
        label: String,
        /// How many files were left out.
        count: u64,
        /// How many did travel. `0` means nothing was uploaded.
        kept_files: u64,
        /// One of the offending paths, relative to the save folder.
        sample_path: String,
        /// The OS error behind it, verbatim. This is the only part that tells
        /// the user whether to start their cloud-files provider, fix a
        /// permission, or worry about the disk.
        sample_error: String,
        /// `false` = no version was created; the save is not backed up at all.
        uploaded: bool,
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
    /// Auto-restore has failed `AUTO_RESTORE_STUCK_AFTER` times in a row on
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
        /// `true` when this save has **never** produced a snapshot, which
        /// makes an empty folder a wrong tracked path far more often than a
        /// real state change (the native folder tracked while the game runs
        /// under Proton, a container tracked instead of its `remote/`, a
        /// phase-4 guess). The UI escalates that case into "check the folder"
        /// instead of the ordinary "nothing to back up" notice.
        ///
        /// A save that has backed up before and is empty *now* is a state
        /// change, not a detection error — R.E.P.O. wipes its own directory
        /// at the menu, which is why it ships a preset — so it stays quiet.
        #[serde(default)]
        likely_wrong_path: bool,
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
        /// What to call it in the toast ("Detectado posible juego: …"): the
        /// catalog title when the manifest recognises the executable, the raw
        /// process name otherwise. The agent log always carries the raw name.
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
    /// The upload is re-armed on a long backoff
    /// ([`crate::kernel::reconcile::BACKUP_FAILURE_BACKOFF_SECS`]) so there's a
    /// way back without waiting on a new fs event. See
    /// `AgentCommand::RetryBackupAfterFailure`.
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
