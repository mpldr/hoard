<script lang="ts">
  /**
   * "Libera espacio" dialog — the black-box escape hatch.
   *
   * Shown when the account's live saves exceed the plan limit and purging old
   * versions can't bring them under (the Pro→Free case). It lists the games by
   * weight and offers three ways out:
   *   - green, top     → upgrade to Pro (nothing gets archived)
   *   - black, bottom-l → download the saves first (reuses the account export)
   *   - red,   bottom-r → Continuar: archive the ticked games. Archiving frees
   *                       the quota now, freezes the cloud copy for 7 days
   *                       (downloadable), then it's purged. The LOCAL save is
   *                       never touched and it's reversible by reactivating
   *                       after upgrading.
   *
   * Two things the first version got wrong, both discovered on a real stuck
   * account (ago-2026):
   *
   * 1. **It chose for you.** A greedy "archive the heaviest until it fits" is
   *    the wrong default when the thing being thrown out is someone's 200-hour
   *    campaign. The picker now starts on that suggestion but every row is a
   *    checkbox, with a live meter showing where the selection lands.
   * 2. **It couldn't see shared bytes.** Blobs referenced by two saves are
   *    exclusive to neither, so they show up in nobody's `freeable_bytes`:
   *    1.25 GB (60% of a Free quota) was invisible, and archiving either game
   *    alone freed exactly zero. `shared_groups` makes those bytes real — they
   *    count as soon as every save sharing them is ticked, and the pair is
   *    called out as the duplicate it almost always is.
   */
  import { _ } from "svelte-i18n";
  import { push } from "svelte-spa-router";
  import Modal from "./Modal.svelte";
  import { Crown, Download, Archive, Link2 } from "@lucide/svelte";
  import {
    storageGamesCloud,
    archiveSaveCloud,
    type StorageGame,
    type SharedGroup,
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
  let usedBytes = $state(0);
  let limitBytes = $state(0);
  let overBytes = $state(0);
  let games = $state<StorageGame[]>([]);
  let sharedGroups = $state<SharedGroup[]>([]);
  let selected = $state<Set<string>>(new Set());
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
      usedBytes = data.used_bytes;
      limitBytes = data.limit_bytes;
      overBytes = data.over_bytes;
      sharedGroups = data.shared_groups ?? [];
      // Archived saves are already out of the quota; everything else is a
      // candidate — including games whose own bytes are all shared
      // (`freeable_bytes === 0`), which the old filter dropped and which are
      // precisely the ones holding a duplicate hostage.
      games = data.games.filter((g) => !g.archived);
      selected = suggestion();
    } catch (e) {
      loadError = String(e);
    } finally {
      loading = false;
    }
  }

  /** Bytes reclaimed by archiving `ids`: every selected game's exclusive bytes,
   *  plus each shared group whose *whole* set is selected. */
  function freedBy(ids: Set<string>): number {
    let total = 0;
    for (const g of games) {
      if (ids.has(g.save_id)) total += g.freeable_bytes;
    }
    for (const grp of sharedGroups) {
      if (grp.save_ids.every((id) => ids.has(id))) total += grp.bytes;
    }
    return total;
  }

  /** Opening selection: heaviest-first until the overage is covered, counting
   *  shared bytes so a duplicate pair gets picked together instead of one half
   *  being ticked for no gain. */
  function suggestion(): Set<string> {
    const sel = new Set<string>();
    if (overBytes <= 0) return sel;
    // Weight = own bytes + a share of every group it belongs to, so a game
    // whose bytes are all shared isn't ranked as worthless.
    const weight = (g: StorageGame) =>
      g.freeable_bytes +
      sharedGroups
        .filter((grp) => grp.save_ids.includes(g.save_id))
        .reduce((n, grp) => n + grp.bytes / grp.save_ids.length, 0);
    const ordered = [...games].sort((a, b) => weight(b) - weight(a));
    for (const g of ordered) {
      if (freedBy(sel) >= overBytes) break;
      sel.add(g.save_id);
      // Pull in the rest of any group this game belongs to: half a duplicate
      // frees nothing, so ticking it alone would be a lie in the meter.
      for (const grp of sharedGroups) {
        if (grp.save_ids.includes(g.save_id)) {
          for (const id of grp.save_ids) sel.add(id);
        }
      }
    }
    return sel;
  }

  function toggle(saveId: string) {
    const next = new Set(selected);
    if (next.has(saveId)) next.delete(saveId);
    else next.add(saveId);
    selected = next;
  }

  const freed = $derived(freedBy(selected));
  const remaining = $derived(Math.max(0, usedBytes - freed));
  const fits = $derived(limitBytes > 0 && remaining <= limitBytes);
  /** Everything selectable, for the "not even archiving it all fits" check. */
  const maxFreeable = $derived(
    freedBy(new Set(games.map((g) => g.save_id))),
  );
  const hopeless = $derived(
    !loading &&
      games.length > 0 &&
      limitBytes > 0 &&
      usedBytes - maxFreeable > limitBytes,
  );

  /** Games sharing bytes with the given one — the "this is the same save twice"
   *  hint. */
  function partners(saveId: string): StorageGame[] {
    const ids = new Set<string>();
    for (const grp of sharedGroups) {
      if (!grp.save_ids.includes(saveId)) continue;
      for (const id of grp.save_ids) if (id !== saveId) ids.add(id);
    }
    return games.filter((g) => ids.has(g.save_id));
  }

  /** Bytes of this game that only come back if its partners go too. */
  function sharedBytesOf(saveId: string): number {
    return sharedGroups
      .filter((grp) => grp.save_ids.includes(saveId))
      .reduce((n, grp) => n + grp.bytes, 0);
  }

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
      .filter((g) => selected.has(g.save_id))
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

  // A la pantalla Pro, no al navegador. Este diálogo salta cuando la cuota se
  // llena —o sea, en mitad de otra cosa—, así que abrir una pestaña encima es
  // el peor momento posible para hacerlo.
  function goPro() {
    onClose();
    push("/pro");
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

    {#if hopeless}
      <p
        class="rounded-lg border border-rose-500/40 bg-rose-500/10 p-2.5 text-xs text-rose-200/90"
      >
        {$_("liberate.hopeless", {
          values: { limit: formatBytes(limitBytes) },
        })}
      </p>
    {/if}

    <div>
      <p class="mb-2 text-xs font-medium uppercase tracking-wide text-zinc-500">
        {$_("liberate.pick")}
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
        <ul class="max-h-64 space-y-1.5 overflow-y-auto pr-1">
          {#each games as g (g.save_id)}
            {@const willArchive = selected.has(g.save_id)}
            {@const twins = partners(g.save_id)}
            {@const shared = sharedBytesOf(g.save_id)}
            <li>
              <label
                class="flex cursor-pointer items-center justify-between gap-3 rounded-lg border px-3 py-2 text-sm {willArchive
                  ? 'border-rose-500/40 bg-rose-500/10'
                  : 'border-white/[0.08] bg-zinc-950/40'}"
              >
                <span class="flex min-w-0 items-center gap-2.5">
                  <input
                    type="checkbox"
                    checked={willArchive}
                    disabled={busy}
                    onchange={() => toggle(g.save_id)}
                    class="size-3.5 shrink-0 accent-rose-500"
                  />
                  <span class="flex min-w-0 flex-col">
                    <span class="truncate text-zinc-200">{gameName(g)}</span>
                    {#if saveLabel(g)}
                      <span class="truncate text-xs text-zinc-500"
                        >{saveLabel(g)}</span
                      >
                    {/if}
                    {#if twins.length > 0}
                      <!-- Casi siempre no son dos juegos: es el mismo save
                           trackeado dos veces. Decirlo aquí evita que el usuario
                           archive uno, no libere nada y crea que esto no va. -->
                      <span
                        class="mt-0.5 flex items-center gap-1 text-[11px] text-amber-300/90"
                      >
                        <Link2 size={11} />
                        {$_("liberate.shared_with", {
                          values: {
                            names: twins.map(gameName).join(", "),
                            size: formatBytes(shared),
                          },
                        })}
                      </span>
                    {/if}
                  </span>
                </span>
                <span class="shrink-0 font-mono text-xs text-zinc-400">
                  {formatBytes(g.freeable_bytes)}
                </span>
              </label>
            </li>
          {/each}
        </ul>

        <!-- Medidor: dónde deja la selección a la cuenta. -->
        <div class="mt-3 rounded-lg border border-white/[0.08] bg-zinc-950/40 p-2.5">
          <div class="flex items-baseline justify-between text-xs">
            <span class="text-zinc-400">{$_("liberate.after")}</span>
            <span class="font-mono {fits ? 'text-emerald-300' : 'text-rose-300'}">
              {formatBytes(remaining)} / {formatBytes(limitBytes)}
            </span>
          </div>
          <div class="mt-1.5 h-1.5 w-full overflow-hidden rounded-full bg-zinc-800">
            <div
              class="h-full rounded-full {fits ? 'bg-emerald-500' : 'bg-rose-500'}"
              style="width: {limitBytes > 0
                ? Math.min(100, (remaining / limitBytes) * 100)
                : 0}%"
            ></div>
          </div>
          <p class="mt-1.5 text-[11px] {fits ? 'text-emerald-300/90' : 'text-rose-300/90'}">
            {fits
              ? $_("liberate.fits")
              : $_("liberate.still_over", {
                  values: {
                    size: formatBytes(Math.max(0, remaining - limitBytes)),
                  },
                })}
          </p>
        </div>
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
        disabled={busy || loading || selected.size === 0}
        class="flex items-center gap-2 rounded-lg bg-red-600 px-4 py-2 text-sm font-semibold text-white transition-colors hover:bg-red-500 disabled:opacity-50"
      >
        <Archive size={15} />
        {busy
          ? $_("liberate.archiving")
          : $_("liberate.continue_n", { values: { count: selected.size } })}
      </button>
    </div>
  {/snippet}
</Modal>
