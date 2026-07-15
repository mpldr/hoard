<script lang="ts">
  /**
   * "Libera espacio" dialog — the black-box escape hatch.
   *
   * Shown when a user's live saves (heads) exceed their plan limit and purging
   * old versions can't bring them under (e.g. a Pro→Free downgrade). It lists
   * the heaviest games and offers three ways out:
   *   - green, top     → upgrade to Pro (nothing gets archived)
   *   - black, bottom-l → download the saves first (reuses the account export)
   *   - red,   bottom-r → Continuar: archive the heaviest games until the
   *                       footprint fits. Archiving frees the quota now, freezes
   *                       the cloud copy for 7 days (downloadable), then it's
   *                       purged. The LOCAL save is never touched and it's
   *                       reversible by reactivating after upgrading.
   */
  import { _ } from "svelte-i18n";
  import Modal from "./Modal.svelte";
  import { Crown, Download, Archive } from "lucide-svelte";
  import {
    storageGamesCloud,
    archiveSaveCloud,
    openUpgradePage,
    type StorageGame,
  } from "../stores/cloud";
  import { toastError, toastSuccess } from "../stores/toasts";
  import { formatBytes } from "../utils/format";

  type Props = {
    open: boolean;
    onClose: () => void;
    /** Reuse the Account page's export flow to grab a copy first. */
    onDownload: () => void;
    /** Called after a successful archive so the parent can refresh the account
     *  (storage bar / status). */
    onDone: () => void;
  };

  let { open, onClose, onDownload, onDone }: Props = $props();

  let loading = $state(false);
  let busy = $state(false);
  let overBytes = $state(0);
  let games = $state<StorageGame[]>([]);
  let loadError = $state<string | null>(null);

  // Load the per-game footprint whenever the dialog opens.
  $effect(() => {
    if (open) void load();
  });

  async function load() {
    loading = true;
    loadError = null;
    try {
      const data = await storageGamesCloud();
      overBytes = data.over_bytes;
      // Only non-archived games with something to free are archivable
      // candidates; the server already orders them heaviest-first.
      games = data.games.filter((g) => !g.archived && g.freeable_bytes > 0);
    } catch (e) {
      loadError = String(e);
    } finally {
      loading = false;
    }
  }

  // The heaviest games we'd archive to get back under the limit: accumulate
  // freeable bytes until they cover the overage.
  const selectedIds = $derived.by(() => {
    const sel = new Set<string>();
    let acc = 0;
    for (const g of games) {
      if (acc >= overBytes) break;
      sel.add(g.save_id);
      acc += g.freeable_bytes;
    }
    return sel;
  });

  // The list is one row per save; `game_slug` is the game and `label` is the
  // save slot (almost always the default "main"). Show the game name — turn the
  // slug into a title — and only surface the label when it disambiguates.
  const ROMAN = new Set([
    "ii", "iii", "iv", "v", "vi", "vii", "viii", "ix",
    "x", "xi", "xii", "xiii", "xiv", "xv",
  ]);
  function prettifyGame(slug: string): string {
    return slug
      .split("-")
      .map((w) =>
        !w
          ? w
          : ROMAN.has(w)
            ? w.toUpperCase()
            : w[0].toUpperCase() + w.slice(1),
      )
      .join(" ");
  }
  const gameName = (g: StorageGame) => prettifyGame(g.game_slug);
  const saveLabel = (g: StorageGame) =>
    g.label && g.label.toLowerCase() !== "main" ? g.label : null;

  async function handleContinue() {
    const ids = games
      .filter((g) => selectedIds.has(g.save_id))
      .map((g) => g.save_id);
    if (ids.length === 0) {
      onClose();
      return;
    }
    busy = true;
    try {
      for (const id of ids) {
        await archiveSaveCloud(id);
      }
      toastSuccess($_("liberate.done"));
      onDone();
      onClose();
    } catch (e) {
      toastError(String(e));
    } finally {
      busy = false;
    }
  }

  function goPro() {
    void openUpgradePage();
  }
</script>

<Modal
  {open}
  title={$_("liberate.title")}
  dismissible={!busy}
  onClose={busy ? () => {} : onClose}
>
  <div class="space-y-4">
    <p class="text-sm text-zinc-300">{$_("liberate.intro")}</p>

    <!-- Pasar a Pro — green, top, full width -->
    <button
      type="button"
      onclick={goPro}
      disabled={busy}
      class="flex w-full items-center justify-center gap-2 rounded-lg bg-emerald-600 px-4 py-2.5 text-sm font-semibold text-white transition-colors hover:bg-emerald-500 disabled:opacity-50"
    >
      <Crown size={16} />
      {$_("liberate.pro")}
    </button>

    <div>
      <p class="mb-2 text-xs font-medium uppercase tracking-wide text-zinc-500">
        {$_("liberate.heaviest")}
      </p>

      {#if loading}
        <p class="text-sm text-zinc-500">{$_("liberate.loading")}</p>
      {:else if loadError}
        <div class="flex items-center justify-between gap-3">
          <p class="text-sm text-rose-400">{$_("liberate.load_error")}</p>
          <button
            type="button"
            onclick={() => void load()}
            class="shrink-0 rounded-lg border border-white/10 bg-zinc-900 px-3 py-1.5 text-xs font-medium text-zinc-200 transition-colors hover:bg-zinc-800"
          >
            {$_("liberate.retry")}
          </button>
        </div>
      {:else if games.length === 0}
        <p class="text-sm text-zinc-500">{$_("liberate.nothing")}</p>
      {:else}
        <ul class="space-y-1.5">
          {#each games as g (g.save_id)}
            {@const willArchive = selectedIds.has(g.save_id)}
            <li
              class="flex items-center justify-between gap-3 rounded-lg border px-3 py-2 text-sm {willArchive
                ? 'border-rose-500/40 bg-rose-500/10'
                : 'border-white/[0.08] bg-zinc-950/40'}"
            >
              <span class="flex min-w-0 flex-col">
                <span class="truncate text-zinc-200">{gameName(g)}</span>
                {#if saveLabel(g)}
                  <span class="truncate text-xs text-zinc-500">{saveLabel(g)}</span>
                {/if}
              </span>
              <span class="flex shrink-0 items-center gap-2">
                {#if willArchive}
                  <span
                    class="rounded-full bg-rose-500/20 px-2 py-0.5 text-[11px] font-medium text-rose-300"
                  >
                    {$_("liberate.will_archive")}
                  </span>
                {/if}
                <span class="font-mono text-xs text-zinc-400"
                  >{formatBytes(g.freeable_bytes)}</span
                >
              </span>
            </li>
          {/each}
        </ul>
      {/if}
    </div>

    <p class="rounded-lg border border-amber-500/40 bg-amber-500/10 p-2.5 text-xs text-amber-200/90">
      {$_("liberate.explain_archive")}
    </p>
  </div>

  {#snippet footer()}
    <div class="flex w-full items-center justify-between gap-3">
      <!-- Descargar saves — black, bottom-left -->
      <button
        type="button"
        onclick={onDownload}
        disabled={busy}
        class="flex items-center gap-2 rounded-lg border border-white/10 bg-zinc-900 px-4 py-2 text-sm font-medium text-zinc-200 transition-colors hover:bg-zinc-800 disabled:opacity-50"
      >
        <Download size={15} />
        {$_("liberate.download")}
      </button>

      <!-- Continuar (= archivar) — red, bottom-right -->
      <button
        type="button"
        onclick={handleContinue}
        disabled={busy || loading || games.length === 0}
        class="flex items-center gap-2 rounded-lg bg-red-600 px-4 py-2 text-sm font-semibold text-white transition-colors hover:bg-red-500 disabled:opacity-50"
      >
        <Archive size={15} />
        {busy ? $_("liberate.archiving") : $_("liberate.continue")}
      </button>
    </div>
  {/snippet}
</Modal>
