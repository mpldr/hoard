<script lang="ts">
  /**
   * Floating bottom-right panel listing recent `agent://*` activity.
   *
   * Toggled from the sidebar (📜 button) and persisted via
   * `prefs.live_activity_visible`. Read-only — the entries come from
   * `live.ts`'s `activityFeed` circular buffer.
   *
   * We render at most 50 visible rows. The buffer holds more (80) so
   * scrolling history isn't truncated mid-session; the cap above is a
   * cheap CSS overflow cutoff.
   */
  import { _ } from "svelte-i18n";
  import { fly } from "svelte/transition";
  import {
    Eye,
    Play,
    Square,
    Clock,
    UploadCloud,
    CheckCircle2,
    XCircle,
    Download,
    RefreshCcw,
    Ban,
    WifiOff,
    Scissors,
    Trash2,
    AlertTriangle,
    Lock,
    LockOpen,
    X,
  } from "lucide-svelte";

  import { activityFeed, type FeedEntry } from "../stores/live";
  import * as api from "../api";
  import { formatBytes } from "../utils/format";

  /** Hide affordance. Persisted via prefs so the next boot remembers. */
  let { onClose }: { onClose: () => void } = $props();

  const ICONS = {
    watcher_armed: Eye,
    game_started: Play,
    game_stopped: Square,
    throttled: Clock,
    upload_started: UploadCloud,
    upload_completed: CheckCircle2,
    upload_failed: XCircle,
    bandwidth_throttled: Clock,
    auto_restored: Download,
    cloud_pull: RefreshCcw,
    quota_reached: Ban,
    offline: WifiOff,
    online: CheckCircle2,
    backup_too_large: XCircle,
    backup_trimmed: Scissors,
    auto_restore_failed: XCircle,
    storage_purging: Trash2,
    storage_full: AlertTriangle,
    gate_locked: Lock,
    gate_unlocked: LockOpen,
  } as const;

  const TINTS = {
    watcher_armed: "text-emerald-400",
    game_started: "text-emerald-300",
    game_stopped: "text-zinc-400",
    throttled: "text-amber-300",
    upload_started: "text-emerald-300",
    upload_completed: "text-emerald-400",
    upload_failed: "text-rose-400",
    bandwidth_throttled: "text-amber-300",
    auto_restored: "text-sky-300",
    cloud_pull: "text-emerald-300",
    quota_reached: "text-amber-400",
    offline: "text-rose-400",
    online: "text-emerald-400",
    backup_too_large: "text-rose-400",
    backup_trimmed: "text-amber-300",
    auto_restore_failed: "text-rose-400",
    storage_purging: "text-amber-400",
    storage_full: "text-rose-400",
    gate_locked: "text-rose-400",
    gate_unlocked: "text-emerald-400",
  } as const;

  // Alert rows get a tinted "card" so plan-limit / storage-pressure events
  // read at a glance: amber for reversible pressure (trimming / purging),
  // red for a hard stop (over-cap upload, restore failure, storage full).
  const ROW_ACCENT: Partial<Record<FeedEntry["kind"], string>> = {
    backup_too_large: "my-1 rounded-md border border-rose-500/60 bg-rose-500/10",
    auto_restore_failed:
      "my-1 rounded-md border border-rose-500/60 bg-rose-500/10",
    storage_full: "my-1 rounded-md border border-rose-500/60 bg-rose-500/10",
    backup_trimmed: "my-1 rounded-md border border-amber-500/60 bg-amber-500/10",
    storage_purging: "my-1 rounded-md border border-amber-500/60 bg-amber-500/10",
  };

  function relativeTime(at: number): string {
    const seconds = Math.round((Date.now() - at) / 1000);
    if (seconds < 5) return $_("activity.time_just_now");
    if (seconds < 60)
      return $_("activity.time_seconds_ago", { values: { count: seconds } });
    const minutes = Math.round(seconds / 60);
    if (minutes < 60)
      return $_("activity.time_minutes_ago", { values: { count: minutes } });
    const hours = Math.round(minutes / 60);
    return $_("activity.time_hours_ago", { values: { count: hours } });
  }

  function summary(e: FeedEntry): string {
    const name = e.game_slug ?? e.save_id?.slice(0, 8) ?? "—";
    switch (e.kind) {
      case "watcher_armed":
        return $_("activity.watcher_armed", { values: { name } });
      case "game_started":
        return $_("activity.game_started", { values: { name } });
      case "game_stopped":
        return $_("activity.game_stopped", { values: { name } });
      case "throttled":
        return $_("activity.throttled", { values: { name } });
      case "upload_started":
        return $_("activity.upload_started", { values: { name } });
      case "upload_completed":
        return $_("activity.upload_completed", {
          values: {
            name,
            version: e.version ?? 0,
            size: formatBytes(e.bytes ?? 0),
          },
        });
      case "upload_failed":
        return $_("activity.upload_failed", {
          values: { name, error: e.error ?? "" },
        });
      case "bandwidth_throttled":
        return $_("activity.bandwidth_throttled", {
          values: { name, seconds: e.retry_in ?? 60 },
        });
      case "auto_restored":
        return $_("activity.auto_restored", {
          values: { name, version: e.version ?? 0 },
        });
      case "cloud_pull":
        return $_("activity.cloud_pull", {
          values: {
            count: e.new_versions ?? 0,
            size: formatBytes(e.bytes ?? 0),
          },
        });
      case "quota_reached":
        return $_("activity.quota_reached", {
          values: { plan: e.plan ?? "free", seconds: e.retry_in ?? 60 },
        });
      case "offline":
        return $_("activity.offline");
      case "online":
        return $_("activity.online");
      case "backup_too_large":
        return $_("activity.backup_too_large", {
          values: {
            name,
            size: formatBytes(e.bytes ?? 0),
            limit: formatBytes(e.limit_bytes ?? 0),
          },
        });
      case "backup_trimmed":
        return $_("activity.backup_trimmed", {
          values: {
            name,
            count: e.count ?? 0,
            size: formatBytes(e.bytes ?? 0),
          },
        });
      case "auto_restore_failed":
        return $_("activity.auto_restore_failed", {
          values: { name, error: e.error ?? "" },
        });
      case "storage_purging":
        return $_("activity.storage_purging");
      case "storage_full":
        return $_("activity.storage_full");
      case "gate_locked":
        return $_("activity.gate_locked", {
          values: {
            reason: $_(e.reason_key ?? "activity.gate_reason_fetch_failed"),
          },
        });
      case "gate_unlocked":
        return $_("activity.gate_unlocked", {
          values: {
            reason: $_(e.reason_key ?? "activity.gate_reason_pro"),
          },
        });
    }
  }

  async function hidePanel() {
    try {
      await api.setLiveActivityVisible(false);
    } catch (err) {
      console.warn("setLiveActivityVisible failed:", err);
    }
    onClose();
  }

  // Tick once a second so relative timestamps stay honest. Wrapped in a
  // counter so Svelte 5 notices the change. Skipped while the window is hidden
  // — no visible timestamps to keep honest, so don't force re-renders.
  let tick = $state(0);
  let timer: ReturnType<typeof setInterval>;
  $effect(() => {
    timer = setInterval(() => {
      if (!document.hidden) tick = tick + 1;
    }, 1000);
    return () => clearInterval(timer);
  });
  const _tickRef = $derived(tick); // touched in summary() via relativeTime call below
</script>

<aside
  class="pointer-events-auto fixed bottom-4 right-4 z-40 flex w-[min(22rem,calc(100vw-2rem))] flex-col rounded-lg border border-zinc-800 bg-zinc-950/95 shadow-xl backdrop-blur"
  aria-label={$_("activity.panel_label")}
  transition:fly={{ y: 12, duration: 180 }}
>
  <header
    class="flex items-center justify-between border-b border-zinc-800 px-3 py-2"
  >
    <div class="flex items-center gap-2 text-xs font-medium text-zinc-200">
      <span aria-hidden="true">
        <RefreshCcw size={12} />
      </span>
      <span>{$_("activity.panel_title")}</span>
    </div>
    <button
      type="button"
      class="rounded p-1 text-zinc-400 hover:text-zinc-100 focus:outline-none focus-visible:ring-2 focus-visible:ring-zinc-500"
      aria-label={$_("activity.hide_panel")}
      title={$_("activity.hide_panel")}
      onclick={hidePanel}
    >
      <X size={14} />
    </button>
  </header>

  {#if $activityFeed.length === 0}
    <p class="px-3 py-6 text-center text-xs text-zinc-500">
      {$_("activity.empty")}
    </p>
  {:else}
    <ul class="max-h-[min(60vh,28rem)] divide-y divide-zinc-800/60 overflow-y-auto">
      {#each $activityFeed.slice(0, 50) as entry (entry.id)}
        {@const Icon = ICONS[entry.kind]}
        <li class="flex items-start gap-3 px-3 py-2 {ROW_ACCENT[entry.kind] ?? ''}">
          <span
            class="mt-0.5 shrink-0 {TINTS[entry.kind]}"
            aria-hidden="true"
          >
            <Icon size={14} />
          </span>
          <div class="min-w-0 flex-1 text-xs leading-snug">
            <p class="truncate text-zinc-200">{summary(entry)}</p>
            <p class="text-[10px] text-zinc-500">
              {_tickRef >= 0 ? relativeTime(entry.at) : ""}
            </p>
          </div>
        </li>
      {/each}
    </ul>
  {/if}
</aside>
