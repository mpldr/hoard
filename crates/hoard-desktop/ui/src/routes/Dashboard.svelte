<script lang="ts">
  /**
   * Dashboard — live view of every tracked save.
   *
   * Hydrates from `list_tracked_saves` and then reactively renders status
   * pills driven by the agent activity store (which subscribes to
   * `agent://*` events at boot time).
   */
  import { onMount } from "svelte";
  import { push } from "svelte-spa-router";
  import { tilt } from "../lib/actions/tilt";
  import {
    LogOut,
    PlayCircle,
    Clock,
    UploadCloud,
    Check,
    AlertTriangle,
    CircleDot,
    RefreshCw,
    History,
    PauseCircle,
    Cloud,
    Layers,
  } from "lucide-svelte";
  import { _ } from "svelte-i18n";

  import Button from "../lib/components/Button.svelte";
  import Card from "../lib/components/Card.svelte";
  import Cover from "../lib/components/Cover.svelte";
  import Modal from "../lib/components/Modal.svelte";
  import QuotaBar from "../lib/components/QuotaBar.svelte";
  import * as api from "../lib/api";
  import type { TrackedSave } from "../lib/api";
  import { auth, refreshQuota, signOut } from "../lib/stores/auth";
  import { activity, status } from "../lib/stores/agent";
  import { toastError, toastSuccess } from "../lib/stores/toasts";

  let saves = $state<TrackedSave[]>([]);
  let loading = $state(true);
  let signingOut = $state(false);
  let now = $state(Date.now());

  // Panel ordering. "recent" (default) = newest last backup first;
  // "size" = biggest cloud footprint first. Every size in this view is the
  // SERVER-side one (`total_size_bytes`) — the panel never shows local sizes,
  // that's the Library's job.
  let sortBy = $state<"recent" | "size">("recent");

  const sortedSaves = $derived.by(() => {
    const arr = [...saves];
    if (sortBy === "size") {
      arr.sort((a, b) => (b.total_size_bytes ?? 0) - (a.total_size_bytes ?? 0));
    } else {
      const t = (s: TrackedSave) =>
        s.last_backup_at ? new Date(s.last_backup_at).getTime() : 0;
      arr.sort((a, b) => t(b) - t(a));
    }
    return arr;
  });

  // Per-user "max versions per save" cap. `null` = unlimited. Edited right
  // here in the panel (explicit user request: not in Settings). A numeric
  // input's bind:value yields `undefined` while empty/invalid, so normalise
  // through `?? null` everywhere.
  let maxVersions = $state<number | null>(null);
  let maxVersionsInput = $state<number | null>(null);
  let savingMaxVersions = $state(false);

  const maxVersionsDirty = $derived((maxVersionsInput ?? null) !== maxVersions);

  // When applying a cap would delete versions, we stop and ask first. Set to
  // the pending {cap, count} while the confirmation modal is open.
  let confirmPrune = $state<{ cap: number; count: number } | null>(null);

  async function applyMaxVersions() {
    const next = maxVersionsInput ?? null;
    if (next != null && (!Number.isInteger(next) || next < 1 || next > 10000)) {
      toastError($_("dashboard.max_versions_invalid"));
      return;
    }
    savingMaxVersions = true;
    try {
      if (next != null) {
        // Dry-run first: if this cap would prune stored versions, ask before
        // touching anything. Clearing the cap never prunes — no dialog.
        const count = await api.previewMaxVersions(next);
        if (count > 0) {
          confirmPrune = { cap: next, count };
          return;
        }
      }
      await commitMaxVersions(next);
    } catch (e) {
      toastError(typeof e === "string" ? e : (e as Error).message);
    } finally {
      savingMaxVersions = false;
    }
  }

  async function confirmPruneAndApply() {
    if (!confirmPrune) return;
    const cap = confirmPrune.cap;
    savingMaxVersions = true;
    try {
      await commitMaxVersions(cap);
      confirmPrune = null;
    } catch (e) {
      toastError(typeof e === "string" ? e : (e as Error).message);
    } finally {
      savingMaxVersions = false;
    }
  }

  async function commitMaxVersions(next: number | null) {
    await api.setMaxVersions(next);
    maxVersions = next;
    toastSuccess($_("dashboard.max_versions_saved"));
    // Pruning frees server space right away — reflect it on the bar.
    refreshQuota().catch(() => {});
  }

  function fmtBytes(n: number): string {
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
    if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
    return `${(n / (1024 * 1024 * 1024)).toFixed(2)} GB`;
  }

  // Tick once a second so "next backup in 28s" countdowns animate. Skip the
  // state write while the window is hidden (minimised / in the tray): nobody
  // can see the countdown, and the write would force a pointless re-render.
  $effect(() => {
    const id = setInterval(() => {
      if (!document.hidden) now = Date.now();
    }, 1000);
    return () => clearInterval(id);
  });

  // Poll the storage quota every 30s while the dashboard is open so the
  // QuotaBar tracks reality after backups land. The first poll runs
  // immediately on mount via `hydrateAuth`, so this just keeps it warm.
  // Hidden window → skip the round-trip; the next visible tick refreshes it.
  $effect(() => {
    const id = setInterval(() => {
      if (!document.hidden) refreshQuota().catch(() => {});
    }, 30_000);
    return () => clearInterval(id);
  });

  onMount(async () => {
    api
      .getMaxVersions()
      .then((n) => {
        maxVersions = n;
        maxVersionsInput = n;
      })
      .catch(() => {});
    try {
      saves = await api.listTrackedSaves();
    } catch (e) {
      toastError(typeof e === "string" ? e : (e as Error).message);
    } finally {
      loading = false;
    }
  });

  async function handleLogout() {
    signingOut = true;
    try {
      await signOut();
      toastSuccess($_("dashboard.signed_out"));
    } catch (e) {
      toastError(typeof e === "string" ? e : (e as Error).message);
    } finally {
      signingOut = false;
    }
  }

  async function backupNow(saveId: string) {
    try {
      await api.backupNow(saveId);
      toastSuccess($_("dashboard.backup_queued"));
    } catch (e) {
      toastError(typeof e === "string" ? e : (e as Error).message);
    }
  }

  /**
   * The version **this device** holds: the backend's local cursor, moved
   * forward by anything the live session has since confirmed (an upload that
   * committed, or an auto-restore that landed). Never the cloud head.
   *
   * Keeping the two apart is the whole point: the panel used to label the
   * *cloud* head "Guardado (v138)" while this machine sat at v120 with a dead
   * poller, so the user believed they had versions they'd never downloaded
   * (ADR 0021 D.10).
   */
  function localVersion(save: TrackedSave): number | null {
    const live = $activity[save.save_id]?.last_version;
    const stored = save.local_version_num;
    if (live == null) return stored;
    if (stored == null) return live;
    return Math.max(live, stored);
  }

  /** `true` when the cloud holds a version this device doesn't have yet. */
  function cloudAhead(save: TrackedSave): boolean {
    if (save.cloud_version_num == null) return false;
    const local = localVersion(save);
    return local == null || save.cloud_version_num > local;
  }

  /** Tooltip for the cloud-version chip: always spells out both numbers, so
   *  "the cloud has vN" can never be read as "this device has vN". */
  function cloudTitle(save: TrackedSave): string {
    const cloud = save.cloud_version_num;
    const local = localVersion(save);
    if (!cloudAhead(save)) return $_("dashboard.cloud_version_title");
    if (local == null) {
      return $_("dashboard.cloud_ahead_no_local_title", { values: { cloud } });
    }
    return $_("dashboard.cloud_ahead_title", { values: { cloud, local } });
  }

  /** The status pill. Reflects **local** state only — what this machine has
   *  on disk and what the agent is doing about it. The cloud head gets its
   *  own chip so the two are never conflated. */
  function pillFor(save: TrackedSave) {
    const a = $activity[save.save_id];
    const local = localVersion(save);
    // Live activity always wins — if the agent reports anything, the pill
    // reflects *that* (it's the freshest signal on screen). Falls through
    // to the server-side history check only when we have no in-memory state
    // for this save, which is the case on a cold app launch.
    if (!a) {
      return idlePill(save, local);
    }
    switch (a.state) {
      case "running":
        return {
          label: $_("dashboard.pill_running"),
          icon: PlayCircle,
          klass: "text-sky-400",
          rail: "bg-sky-500",
          tint: "bg-sky-500/[0.05]",
        };
      case "scheduled": {
        const secs = Math.max(
          0,
          Math.round(((a.next_backup_at ?? now) - now) / 1000),
        );
        return {
          label: $_("dashboard.pill_scheduled", { values: { seconds: secs } }),
          icon: Clock,
          klass: "text-amber-400",
          rail: "bg-amber-500",
          tint: "bg-amber-500/[0.04]",
        };
      }
      case "uploading":
        return {
          label: $_("dashboard.pill_uploading"),
          icon: UploadCloud,
          klass: "text-amber-400",
          rail: "bg-amber-500",
          tint: "bg-amber-500/[0.05]",
        };
      case "ok":
        return {
          label: local
            ? $_("dashboard.pill_saved_local_v", { values: { version: local } })
            : $_("dashboard.pill_saved"),
          icon: Check,
          klass: "text-emerald-400",
          rail: "bg-emerald-500",
          tint: "bg-emerald-500/[0.04]",
        };
      case "partial":
        return {
          label: $_("dashboard.pill_partial"),
          icon: AlertTriangle,
          klass: "text-amber-400",
          rail: "bg-amber-500",
          tint: "bg-amber-500/[0.04]",
        };
      case "failed":
        return {
          label: a.will_retry ? $_("dashboard.pill_failed_retry") : $_("dashboard.pill_failed"),
          icon: AlertTriangle,
          klass: "text-red-400",
          rail: "bg-red-500",
          tint: "bg-red-500/[0.05]",
        };
      default:
        return idlePill(save, local);
    }
  }

  /** Resting pill (no live activity): what this device holds, or — when it
   *  holds nothing — an explicit "only in the cloud" instead of a version
   *  number the user would read as their own. */
  function idlePill(save: TrackedSave, local: number | null) {
    if (local != null) {
      return {
        label: $_("dashboard.pill_saved_local_v", { values: { version: local } }),
        icon: Check,
        klass: "text-emerald-400",
        rail: "bg-emerald-500",
        tint: "bg-emerald-500/[0.04]",
      };
    }
    if (save.cloud_version_num != null) {
      return {
        label: $_("dashboard.pill_cloud_only_v", {
          values: { version: save.cloud_version_num },
        }),
        icon: Cloud,
        klass: "text-zinc-400",
        rail: "bg-zinc-600",
        tint: "",
      };
    }
    return {
      label: $_("dashboard.pill_no_backup"),
      icon: CircleDot,
      klass: "text-zinc-400",
      rail: "bg-zinc-600",
      tint: "",
    };
  }
</script>

<div class="mx-auto max-w-5xl px-8 py-8">
  <header class="mb-7 flex items-start justify-between gap-4">
    <div class="min-w-0">
      <h1 class="font-display text-[28px] leading-tight font-semibold tracking-[-0.02em] text-zinc-50">
        {#if $auth.user}
          {$_("dashboard.welcome_back", { values: { username: $auth.user.username } })}
        {:else}
          {$_("dashboard.title")}
        {/if}
      </h1>
      <p class="mt-2 flex items-center gap-2.5 text-sm text-zinc-400">
        <span class="relative inline-flex h-3 w-3 shrink-0">
          {#if $status.running}
            <span
              class="absolute inline-flex h-full w-full animate-ping rounded-full bg-emerald-400/70"
            ></span>
          {/if}
          <span
            class="relative inline-flex h-3 w-3 rounded-full {$status.running
              ? 'bg-emerald-400 shadow-[0_0_8px_2px_rgba(16,185,129,0.5)]'
              : 'bg-zinc-600'}"
          ></span>
        </span>
        <span>
          {$status.running ? $_("dashboard.agent_watching") : $_("dashboard.agent_offline")}
          {#if $status.running}
            <span class="text-zinc-600">·</span>
            {$_("dashboard.tracked_count", { values: { count: saves.length } })}
          {/if}
        </span>
      </p>
    </div>
    <Button
      variant="ghost"
      onclick={handleLogout}
      loading={signingOut}
      aria-label={$_("dashboard.sign_out")}
    >
      <LogOut size={16} />
      {$_("dashboard.sign_out")}
    </Button>
  </header>

  {#if $auth.user}
    <div class="mb-4">
      <QuotaBar user={$auth.user} />
    </div>
  {/if}

  {#if !loading && saves.length > 0}
    <div
      class="mb-4 flex flex-wrap items-center justify-between gap-x-4 gap-y-2"
    >
      <label class="flex items-center gap-2 text-xs text-zinc-400">
        <span class="text-zinc-500">{$_("dashboard.sort_label")}</span>
        <select
          class="rounded-md border border-white/[0.08] bg-zinc-900 px-2 py-1.5 text-xs text-zinc-200 focus:border-emerald-500/40 focus:outline-none"
          bind:value={sortBy}
        >
          <option value="recent">{$_("dashboard.sort_recent")}</option>
          <option value="size">{$_("dashboard.sort_size")}</option>
        </select>
      </label>

      <!-- Max stored versions per game. Server-side, per-user; lowering it
           prunes the oldest versions immediately. -->
      <label
        class="flex items-center gap-2 text-xs text-zinc-400"
        title={$_("dashboard.max_versions_hint")}
      >
        <Layers size={13} class="text-zinc-500" />
        <span class="text-zinc-500">{$_("dashboard.max_versions_label")}</span>
        <input
          type="number"
          min="1"
          max="10000"
          placeholder="∞"
          bind:value={maxVersionsInput}
          disabled={savingMaxVersions}
          class="w-16 rounded-md border border-white/[0.08] bg-zinc-900 px-2 py-1.5 text-xs text-zinc-200 [appearance:textfield] focus:border-emerald-500/40 focus:outline-none disabled:opacity-50 [&::-webkit-inner-spin-button]:appearance-none [&::-webkit-outer-spin-button]:appearance-none"
        />
        {#if maxVersionsDirty}
          <Button
            variant="secondary"
            class="!px-2.5 !py-1.5 !text-xs"
            onclick={applyMaxVersions}
            loading={savingMaxVersions}
          >
            {$_("dashboard.max_versions_apply")}
          </Button>
        {/if}
      </label>
    </div>
  {/if}

  {#if loading}
    <Card>
      <div class="shimmer py-12 text-center text-sm text-zinc-400">{$_("common.loading")}</div>
    </Card>
  {:else if saves.length === 0}
    <Card>
      <div class="py-16 text-center">
        <div
          class="mx-auto mb-4 flex h-12 w-12 items-center justify-center rounded-full bg-emerald-500/10 text-emerald-400 ring-1 ring-emerald-500/30"
        >
          <RefreshCw size={20} />
        </div>
        <h2 class="text-base font-medium text-zinc-100">
          {$_("dashboard.no_saves_title")}
        </h2>
        <p class="mx-auto mt-2 max-w-md text-sm text-zinc-400">
          {$_("dashboard.no_saves_body")}
        </p>
      </div>
    </Card>
  {:else}
    <div class="space-y-2.5">
      {#each sortedSaves as save (save.save_id)}
        {@const pill = pillFor(save)}
        <div
          class="tilt group relative flex items-center gap-4 overflow-hidden rounded-xl border border-white/[0.08] {pill.tint} p-4 pl-5 shadow-[inset_0_1px_0_0_rgba(255,255,255,0.03)] transition-all duration-150 hover:border-white/[0.12] hover:bg-zinc-900/50"
          use:tilt
        >
          <!-- Status rail: a 3px vertical bar on the left edge, coloured by
               the save's live state. Makes the list scanable at a glance —
               green=ok, amber=scheduled, sky=running, red=failed. -->
          <span
            class="absolute inset-y-0 left-0 w-[3px] {pill.rail}"
            aria-hidden="true"
          ></span>
          <Cover
            slug={save.game_slug}
            name={save.game_slug}
            class="h-14 w-14 shrink-0 rounded-2xl"
            initialClass="text-xl"
          />
          <div class="min-w-0 flex-1">
            <div class="flex items-center gap-2">
              <span class="truncate text-[15px] font-medium text-zinc-100">
                {save.game_slug}
              </span>
              <span
                class="shrink-0 rounded-md bg-white/[0.05] px-1.5 py-0.5 text-[11px] text-zinc-400 ring-1 ring-inset ring-white/[0.06]"
              >
                {save.label}
              </span>
              {#if save.paused}
                <span
                  class="inline-flex shrink-0 items-center gap-1 rounded-md bg-amber-500/10 px-1.5 py-0.5 text-[11px] text-amber-400 ring-1 ring-inset ring-amber-500/30"
                >
                  <PauseCircle size={11} /> {$_("dashboard.paused")}
                </span>
              {/if}
            </div>
            <p
              class="mt-1 flex items-center gap-1.5 truncate text-xs text-zinc-600"
              title={save.local_path}
            >
              <span class="inline-block h-1 w-1 shrink-0 rounded-full bg-zinc-700"></span>
              <span class="truncate font-mono">{save.local_path}</span>
            </p>
          </div>
          {#if save.cloud_version_num != null}
            <!-- The CLOUD's head version, always labelled as such and never
                 merged into the status pill (which is this device's). Amber
                 when the cloud is ahead: same "there's something newer waiting"
                 semantics as the update-available badge. -->
            <span
              class="inline-flex shrink-0 items-center gap-1 rounded-md px-2 py-1 text-[11px] tabular-nums ring-1 ring-inset {cloudAhead(
                save,
              )
                ? 'bg-amber-500/10 text-amber-400 ring-amber-500/30'
                : 'bg-white/[0.04] text-zinc-300 ring-white/[0.06]'}"
              title={cloudTitle(save)}
            >
              <Cloud size={11} class={cloudAhead(save) ? "" : "text-zinc-500"} />
              {$_("dashboard.cloud_version_badge", {
                values: { version: save.cloud_version_num },
              })}
            </span>
          {/if}
          {#if save.total_size_bytes > 0}
            <!-- Cloud footprint only — the panel never shows local sizes. -->
            <span
              class="inline-flex shrink-0 items-center gap-1 rounded-md bg-white/[0.04] px-2 py-1 text-[11px] tabular-nums text-zinc-300 ring-1 ring-inset ring-white/[0.06]"
              title={$_("dashboard.cloud_size_title")}
            >
              <Cloud size={11} class="text-zinc-500" />
              {fmtBytes(save.total_size_bytes)}
            </span>
          {/if}
          <div class="flex shrink-0 items-center gap-1.5 text-xs font-medium {pill.klass}">
            {#if pill.klass.includes("sky") || pill.klass.includes("amber")}
              <span class="relative flex h-2 w-2">
                <span class="absolute inline-flex h-full w-full animate-ping rounded-full {pill.rail} opacity-60"></span>
                <span class="relative inline-flex h-2 w-2 rounded-full {pill.rail}"></span>
              </span>
            {:else}
              <pill.icon size={14} />
            {/if}
            <span class="whitespace-nowrap">{pill.label}</span>
          </div>
          <div class="flex shrink-0 items-center gap-1">
            <Button
              variant="ghost"
              size="md"
              onclick={() => push(`/history/${save.save_id}`)}
              title={$_("dashboard.history_title")}
              aria-label={$_("dashboard.history")}
            >
              <History size={14} />
              <span class="hidden lg:inline">{$_("dashboard.history")}</span>
            </Button>
            <Button
              variant="secondary"
              size="md"
              onclick={() => backupNow(save.save_id)}
              disabled={!$status.running}
              title={!$status.running
                ? $_("dashboard.tooltip_offline")
                : save.paused
                  ? $_("dashboard.tooltip_force_paused")
                  : $_("dashboard.tooltip_force")}
            >
              <UploadCloud size={14} />
              <span class="hidden lg:inline">{$_("dashboard.back_up")}</span>
            </Button>
          </div>
        </div>
      {/each}
    </div>
  {/if}
</div>

<!-- Lowering the cap below the stored history is destructive — the server
     prunes immediately. The dry-run count feeds this confirmation, so the
     user sees exactly how many versions are about to go. -->
<Modal
  open={!!confirmPrune}
  title={$_("dashboard.max_versions_confirm_title")}
  dismissible={!savingMaxVersions}
  onClose={() => {
    if (!savingMaxVersions) confirmPrune = null;
  }}
>
  {#if confirmPrune}
    <div class="space-y-3 text-sm text-zinc-300">
      <p>
        {$_("dashboard.max_versions_confirm_body", {
          values: { cap: confirmPrune.cap, count: confirmPrune.count },
        })}
      </p>
      <div
        class="flex items-start gap-2 rounded-md border border-amber-500/20 bg-amber-500/5 p-3 text-xs text-amber-200"
      >
        <AlertTriangle size={14} class="mt-0.5 shrink-0" />
        <span>{$_("dashboard.max_versions_confirm_note")}</span>
      </div>
    </div>
  {/if}
  {#snippet footer()}
    <Button
      variant="secondary"
      onclick={() => (confirmPrune = null)}
      disabled={savingMaxVersions}
    >
      {$_("common.cancel")}
    </Button>
    <Button
      variant="danger"
      onclick={confirmPruneAndApply}
      loading={savingMaxVersions}
    >
      {$_("dashboard.max_versions_confirm_apply")}
    </Button>
  {/snippet}
</Modal>
