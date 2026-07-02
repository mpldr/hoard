<script lang="ts">
  /**
   * Compact storage bar for the sidebar footer (replaces the old
   * watcher/cloud LiveStatus line).
   *
   * Same reading as the dashboard's QuotaBar — used / quota with the bar
   * escalating emerald → amber (≥80%) → red (≥95%) — just shrunk to fit
   * the rail. Self-hosted servers (and quota-less accounts) have no cap,
   * so they get the plain "X usado" line with no bar.
   *
   * The sidebar outlives every route, so this component owns its own 30s
   * refresh; the dashboard's poll writes to the same auth store, whoever
   * fires first wins.
   */
  import { onMount } from "svelte";
  import { _ } from "svelte-i18n";
  import { auth, refreshQuota } from "../stores/auth";

  function fmtBytes(n: number): string {
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
    if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
    return `${(n / (1024 * 1024 * 1024)).toFixed(2)} GB`;
  }

  onMount(() => {
    const timer = setInterval(() => {
      if (!document.hidden) refreshQuota().catch(() => {});
    }, 30_000);
    return () => clearInterval(timer);
  });

  const used = $derived($auth.user?.storage_used_bytes ?? 0);
  const quota = $derived($auth.user?.storage_quota_bytes ?? 0);
  const capped = $derived(
    !($auth.user?.is_local_server ?? false) && quota > 0,
  );

  const pct = $derived(
    capped ? Math.min(100, Math.max(0, (used / quota) * 100)) : 0,
  );

  const barClass = $derived(
    pct >= 95 ? "bg-red-500" : pct >= 80 ? "bg-amber-400" : "bg-emerald-500",
  );
  const pctClass = $derived(
    pct >= 95 ? "text-red-400" : pct >= 80 ? "text-amber-400" : "text-zinc-300",
  );
</script>

{#if $auth.user}
  <div class="space-y-1.5 px-1" title={$_("quota.label")}>
    <div class="flex items-center justify-between gap-2 text-[11px] text-zinc-400">
      <span class="truncate">
        {#if capped}
          {$_("quota.used_of", {
            values: { used: fmtBytes(used), quota: fmtBytes(quota) },
          })}
        {:else}
          {$_("quota.used", { values: { size: fmtBytes(used) } })}
        {/if}
      </span>
      {#if capped}
        <span class="shrink-0 font-semibold tabular-nums {pctClass}">
          {pct.toFixed(1)}%
        </span>
      {/if}
    </div>
    {#if capped}
      <div class="h-1.5 w-full overflow-hidden rounded-full bg-zinc-800">
        <div
          class="h-full rounded-full transition-[width] duration-500 ease-out {barClass}"
          style:width="{pct}%"
        ></div>
      </div>
    {/if}
  </div>
{/if}
