/**
 * Live-agent store.
 *
 * The Rust live agent emits events through the `agent://*` Tauri event
 * channels. This store subscribes to them once at app boot, keeps a
 * per-save activity status in memory, and lets pages reactively render
 * "backing up…", "next in 30s", "failed (will retry)", etc.
 *
 * Activity is keyed by `save_id`; pages look up their saves by id.
 *
 * The store is also responsible for two side effects that need to react to
 * the same event stream:
 *
 *  1. **Tray colouring** — we collapse all per-save states into a single
 *     "global state" and push it down to Rust so the tray icon recolours.
 *  2. **Desktop notifications** — backup successes/failures fire a system
 *     notification, gated by the user's prefs.
 *
 * Doing this in the same place avoids a separate listener subscription per
 * concern, and keeps the priority logic (failures beat successes beat
 * uploads…) in one obvious spot.
 */
import { derived, get, writable, type Writable } from "svelte/store";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";
import { _ as i18n } from "svelte-i18n";

import type {
  AgentEvent,
  AgentStatus,
  BackupReason,
  TrayStateName,
} from "../api";
import * as api from "../api";
import { prefs } from "./prefs";
import { seedWatcher } from "./live";
import { toastSuccess } from "./toasts";

export type SaveActivity = {
  state:
    | "idle"
    | "running" // game running
    | "scheduled"
    | "uploading"
    | "ok"
    | "partial" // backed up, but the plan's per-save cap dropped old files
    | "failed";
  /** When the next backup is expected to fire (epoch ms), if scheduled. */
  next_backup_at?: number;
  /** When the current game session started (epoch ms), if running. Drives
   *  the elapsed-time counter in the Eye panel. */
  running_since?: number;
  reason?: BackupReason;
  /** The version **this device** now holds — set by an upload that committed
   *  and by an auto-restore that landed. Never the cloud head: the panel keeps
   *  those apart on purpose (ADR 0021 D.10). */
  last_version?: number;
  last_bytes?: number;
  error?: string;
  will_retry?: boolean;
};

type ActivityMap = Record<string, SaveActivity>;

export const activity: Writable<ActivityMap> = writable({});

/** A save whose cloud auto-restore keeps failing on the same version. */
export type StuckRestore = {
  /** Consecutive failures, so the badge can say "3×" and read as a pattern. */
  failures: number;
  /** Last error chain — the badge's tooltip, so the user sees *why*. */
  error: string;
};

/** Saves currently stuck on a failing cloud restore, keyed by `save_id`.
 *
 * Deliberately separate from `activity`: that map tracks the *current* backup
 * lifecycle and gets overwritten by the next event, whereas this is a sticky
 * condition that must outlive unrelated activity. It's the persistent half of
 * the July-2026 fix — the transient toast is what let a save silently fail to
 * sync for eight days. Cleared only by a recovery (success or a new cloud
 * version), never by time passing. */
export const restoreStuck: Writable<Record<string, StuckRestore>> = writable(
  {},
);
export const status: Writable<AgentStatus> = writable({
  running: false,
  watched_count: 0,
});

/** Reduce per-save activity into a single tray-coloring state. Priority is
 * "anything broken first": a failure dominates an in-flight upload, which
 * dominates a running game, which dominates idle. Mirrors the precedence
 * users expect from a status indicator. */
export const trayState = derived<
  [Writable<ActivityMap>, Writable<AgentStatus>],
  TrayStateName
>([activity, status], ([$activity, $status]) => {
  if (!$status.running) return "offline";
  const states = Object.values($activity).map((a) => a.state);
  if (states.some((s) => s === "failed")) return "error";
  if (states.some((s) => s === "uploading")) return "uploading";
  if (states.some((s) => s === "scheduled")) return "uploading";
  if (states.some((s) => s === "running")) return "running";
  // "partial" still means the last sync succeeded — the tray stays green so we
  // don't cry wolf; the plan-limit warning lives on the save row + toast.
  if (states.some((s) => s === "ok" || s === "partial")) return "ok";
  return "idle";
});

let unlisteners: UnlistenFn[] = [];
let lastTrayState: TrayStateName | null = null;
let trayUnsub: (() => void) | null = null;
let notificationsAllowed = false;

async function ensureNotificationPermission() {
  // Ask once on startup. If the user denies, future toasts no-op silently —
  // we don't want to nag, and Tauri's plugin already handles the system
  // dialog gracefully.
  try {
    notificationsAllowed = await isPermissionGranted();
    if (!notificationsAllowed) {
      const decision = await requestPermission();
      notificationsAllowed = decision === "granted";
    }
  } catch (e) {
    console.warn("notification permission probe failed:", e);
  }
}

function notify(title: string, body: string) {
  if (!notificationsAllowed) return;
  try {
    sendNotification({ title, body });
  } catch (e) {
    console.warn("sendNotification failed:", e);
  }
}

function patch(save_id: string, partial: Partial<SaveActivity>) {
  activity.update((m) => {
    const prev = m[save_id] ?? { state: "idle" };
    return { ...m, [save_id]: { ...prev, ...partial } };
  });
}

function applyEvent(ev: AgentEvent) {
  switch (ev.type) {
    case "game_started":
      patch(ev.save_id, { state: "running", running_since: Date.now() });
      break;
    case "game_stopped":
      patch(ev.save_id, { state: "idle", running_since: undefined });
      break;
    case "backup_scheduled":
      patch(ev.save_id, {
        state: "scheduled",
        next_backup_at: Date.now() + ev.delay_ms,
        reason: ev.reason,
      });
      break;
    case "backup_started":
      patch(ev.save_id, { state: "uploading", next_backup_at: undefined });
      break;
    case "backup_success": {
      patch(ev.save_id, {
        state: "ok",
        last_version: ev.version_num,
        last_bytes: ev.total_bytes,
        error: undefined,
        will_retry: undefined,
      });
      const $prefs = get(prefs);
      if ($prefs?.notify_on_success) {
        notify(
          "Backup saved",
          `${ev.save_id.slice(0, 8)} · v${ev.version_num} (${formatBytes(
            ev.total_bytes,
          )})`,
        );
      }
      break;
    }
    case "backup_failed": {
      patch(ev.save_id, {
        state: "failed",
        error: ev.error,
        will_retry: ev.will_retry,
      });
      const $prefs = get(prefs);
      if ($prefs?.notify_on_failure) {
        notify(
          ev.will_retry ? "Backup failed (retrying)" : "Backup failed",
          ev.error,
        );
      }
      break;
    }
    case "save_auto_restored": {
      // A restore that landed moves *this device's* version forward, so record
      // it: without this the dashboard keeps flagging the cloud as ahead until
      // the next full `list_tracked_saves`. Deliberately doesn't touch `state`
      // — a restore isn't a backup lifecycle transition.
      patch(ev.save_id, { last_version: ev.version_num });
      // Beyond that it's a one-shot adoption side-effect, but we do want to
      // surface a toast so the user notices files appeared under `~`. The
      // `game_slug` is what we have to hand; resolving to display_name would
      // require another store dependency just for cosmetics.
      const t = get(i18n);
      toastSuccess(
        t("library.auto_restored_toast", {
          values: {
            name: ev.game_slug,
            version: ev.version_num,
            count: ev.files_extracted,
          },
        }),
      );
      break;
    }
    case "save_auto_restore_failed": {
      // Surfaced in the activity feed (see live.ts), not as a toast. A stale
      // session token at launch made this fire once per tracked save — a burst
      // of popups every start. The agent now suppresses the transient 401 at
      // source; genuine per-save failures still land in the feed.
      break;
    }
    case "save_auto_restore_stuck": {
      // The agent only sends this once per (save, version), after repeated
      // failures — so it's rare enough to deserve a notification, and the
      // Library card keeps an amber badge until it recovers.
      restoreStuck.update((m) => ({
        ...m,
        [ev.save_id]: { failures: ev.failures, error: ev.error },
      }));
      // Honour the same pref as backup failures: the card badge is the part
      // the user can't miss, so a silenced notification loses nothing.
      const $prefs = get(prefs);
      if ($prefs?.notify_on_failure) {
        const t = get(i18n);
        notify(
          t("library.restore_stuck_notify_title"),
          t("library.restore_stuck_notify_body", {
            values: { name: ev.game_slug, count: ev.failures },
          }),
        );
      }
      break;
    }
    case "save_auto_restore_recovered": {
      restoreStuck.update((m) => {
        if (!(ev.save_id in m)) return m;
        const next = { ...m };
        delete next[ev.save_id];
        return next;
      });
      break;
    }
    case "backup_too_large": {
      // The save is bigger than the plan's per-save cap, so it can never
      // upload as-is. Not a transient failure and not "retrying" — show an
      // actionable message (with the real limit/size) once and mark the row
      // failed-without-retry so it stops spamming every folder change.
      const msg =
        ev.limit_bytes > 0
          ? get(i18n)("library.backup_too_large_toast", {
              values: {
                name: ev.game_slug,
                limit: formatBytes(ev.limit_bytes),
                size: formatBytes(ev.actual_bytes),
              },
            })
          : get(i18n)("library.backup_too_large_generic_toast", {
              values: { name: ev.game_slug },
            });
      patch(ev.save_id, { state: "failed", error: msg, will_retry: false });
      // No toast: the activity feed carries this now (see live.ts). Native
      // notification stays behind the user's opt-in `notify_on_failure`.
      if (get(prefs)?.notify_on_failure) notify("Backup failed", msg);
      break;
    }
    case "backup_trimmed": {
      // The backup succeeded but the plan's per-save cap forced us to upload
      // only the newest files. Not an error (a backup_success already fired),
      // but the user must know their plan isn't enough for this save: mark the
      // row amber "partial" and warn once.
      const msg = get(i18n)("library.backup_trimmed_toast", {
        values: {
          name: ev.game_slug,
          omitted: ev.omitted_files,
          size: formatBytes(ev.omitted_bytes),
        },
      });
      patch(ev.save_id, { state: "partial", error: msg, will_retry: false });
      // No toast — surfaced in the activity feed (see live.ts).
      break;
    }
    case "backup_skipped_empty": {
      // The folder resolved to empty/missing so we did *not* push an empty
      // snapshot (that would silently overwrite the user's last good copy on
      // the server). This is a benign, recurring condition — the
      // reconciliation sweep re-checks the folder every cycle, and a
      // mis-detected/empty tracked folder (or a game that never wrote a save
      // yet) would fire this on every tick. So don't toast or notify; just
      // reflect the save as idle. Users who want the cloud copy pulled into an
      // empty folder enable "Restore from cloud when a save folder is empty"
      // in Settings.
      patch(ev.save_id, { state: "idle", error: undefined, will_retry: undefined });
      break;
    }
  }
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(0)} MB`;
  return `${(n / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}

/** Subscribe to all `agent://*` channels. Idempotent — safe to call from
 * `onMount` more than once (we tear down previous listeners first). */
export async function subscribeAgent() {
  await unsubscribeAgent();
  const topics = [
    "agent://game-started",
    "agent://game-stopped",
    "agent://backup-scheduled",
    "agent://backup-started",
    "agent://backup-success",
    "agent://backup-failed",
    "agent://backup-too-large",
    "agent://backup-trimmed",
    "agent://save-auto-restored",
    "agent://save-auto-restore-failed",
    "agent://save-auto-restore-stuck",
    "agent://save-auto-restore-recovered",
    "agent://backup-skipped-empty",
  ];
  unlisteners = await Promise.all(
    topics.map((t) =>
      listen<AgentEvent>(t, (event) => applyEvent(event.payload)),
    ),
  );

  // Mirror the derived tray state into Rust whenever it changes. We only
  // push when the value actually flips so the tray doesn't repaint every
  // tick of the FS-event firehose.
  trayUnsub = trayState.subscribe((s) => {
    if (s === lastTrayState) return;
    lastTrayState = s;
    api.setTrayState(s).catch((e) => console.warn("setTrayState failed:", e));
  });
}

export async function unsubscribeAgent() {
  for (const u of unlisteners) {
    try {
      u();
    } catch {
      /* ignore */
    }
  }
  unlisteners = [];
  if (trayUnsub) {
    trayUnsub();
    trayUnsub = null;
  }
  lastTrayState = null;
}

/** In-flight `bootAgent` promise. Both `hydrateAuth` (self-hosted) and
 *  `hydrateCloud` (cloud) can race to boot the agent on a cold start where the
 *  user has both sessions; `start_agent` guards against a double spawn but has
 *  an `await` between its "already running?" check and setting the handle, so
 *  two concurrent calls could slip through. Deduping here closes that window. */
let bootInFlight: Promise<void> | null = null;

/** Start the agent and subscribe. Idempotent and safe to call from every login
 *  path (self-hosted or cloud) and from the cold-start hydrate; concurrent
 *  calls share a single boot. */
export async function bootAgent() {
  if (bootInFlight) return bootInFlight;
  bootInFlight = (async () => {
    await ensureNotificationPermission();
    const s = await api.startAgent();
    status.set(s);
    // Seed the live header's watcher dot from the boot status. The
    // `agent://watcher-armed` events fire inside `start_agent` before
    // `subscribeLive()` is wired, so without this the header stays on
    // "watcher off" for the whole session even though saves are watched.
    seedWatcher(s.watched_count);
    await subscribeAgent();
  })();
  try {
    await bootInFlight;
  } finally {
    bootInFlight = null;
  }
}

/** Stop the agent and clear state. Called on logout. */
export async function shutdownAgent() {
  await unsubscribeAgent();
  try {
    await api.stopAgent();
  } catch {
    /* logout should never fail because the agent didn't */
  }
  activity.set({});
  status.set({ running: false, watched_count: 0 });
  // Drop the tray back to "offline" so the icon doesn't lie.
  api.setTrayState("offline").catch(() => {});
}
