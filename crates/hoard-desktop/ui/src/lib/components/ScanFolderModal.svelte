<script lang="ts">
  /**
   * "Scan folder" dialog (Hoard free) — Ludusavi-style add-from-folder.
   *
   * The user points Hoard at one folder; we run the catalog-free discovery
   * walk scoped to it (`scan_folder`) and list every save-like dir found
   * inside, attributed to a game name, as "found <Game> here — track it?".
   * Each row tracks through the normal `addGameToTracking`. Folders already
   * covered by a tracked save are filtered out backend-side, so the list only
   * ever shows new candidates.
   */
  import { open as openDialog } from "@tauri-apps/plugin-dialog";
  import { FolderOpen, FolderSearch, Check, Plus } from "lucide-svelte";
  import { _ } from "svelte-i18n";

  import Modal from "./Modal.svelte";
  import Button from "./Button.svelte";
  import { scanFolder, addGameToTracking, type DetectedGame } from "../api";
  import { toastError, toastSuccess } from "../stores/toasts";

  type Props = {
    open: boolean;
    onClose: () => void;
    onAdded: (save: import("../api").TrackedSave) => void;
  };
  let { open, onClose, onAdded }: Props = $props();

  let folder = $state("");
  let scanning = $state(false);
  let scanned = $state(false);
  let results = $state<DetectedGame[]>([]);
  // Slugs currently being tracked / already tracked this session, so a row's
  // button shows progress and can't be double-submitted.
  let trackingSlug = $state<string | null>(null);
  let addedSlugs = $state<string[]>([]);

  async function pickFolder() {
    try {
      const result = await openDialog({
        directory: true,
        multiple: false,
        defaultPath: folder || undefined,
        title: $_("scan_folder.pick"),
      });
      if (typeof result === "string" && result.length > 0) {
        folder = result;
        await runScan();
      }
    } catch (e) {
      toastError(typeof e === "string" ? e : (e as Error).message);
    }
  }

  async function runScan() {
    if (folder.trim().length === 0 || scanning) return;
    scanning = true;
    scanned = false;
    results = [];
    addedSlugs = [];
    try {
      results = await scanFolder(folder.trim());
      scanned = true;
    } catch (e) {
      toastError(typeof e === "string" ? e : (e as Error).message);
    } finally {
      scanning = false;
    }
  }

  async function track(game: DetectedGame) {
    if (trackingSlug !== null) return;
    const path = game.found_paths[0];
    if (!path) return;
    trackingSlug = game.slug;
    try {
      const saved = await addGameToTracking({
        game_slug: game.slug,
        local_path: path,
        display_name: game.display_name,
      });
      onAdded(saved);
      addedSlugs = [...addedSlugs, game.slug];
      toastSuccess(
        $_("manual.added_game", { values: { name: game.display_name } }),
      );
    } catch (e) {
      toastError(typeof e === "string" ? e : (e as Error).message);
    } finally {
      trackingSlug = null;
    }
  }

  function reset() {
    folder = "";
    scanning = false;
    scanned = false;
    results = [];
    trackingSlug = null;
    addedSlugs = [];
  }

  function close() {
    reset();
    onClose();
  }
</script>

<Modal
  {open}
  title={$_("scan_folder.title")}
  description={$_("scan_folder.description")}
  onClose={close}
>
  <div class="space-y-4">
    <div class="flex flex-wrap gap-2">
      <Button variant="secondary" onclick={pickFolder} loading={scanning}>
        <FolderOpen size={14} />
        {folder ? $_("scan_folder.change") : $_("scan_folder.pick")}
      </Button>
      {#if folder}
        <span
          class="flex min-w-0 flex-1 items-center truncate rounded-md bg-zinc-950/60 px-3 font-mono text-xs text-zinc-400 ring-1 ring-inset ring-zinc-800"
          title={folder}
        >
          {folder}
        </span>
      {/if}
    </div>

    {#if scanning}
      <p class="py-6 text-center text-sm text-zinc-400">
        {$_("scan_folder.scanning")}
      </p>
    {:else if scanned && results.length === 0}
      <div class="py-8 text-center">
        <FolderSearch size={28} class="mx-auto mb-2 text-zinc-600" />
        <p class="text-sm text-zinc-400">{$_("scan_folder.empty")}</p>
      </div>
    {:else if results.length > 0}
      <div>
        <p class="mb-2 text-xs uppercase tracking-wide text-zinc-500">
          {$_("scan_folder.found", { values: { count: results.length } })}
        </p>
        <ul
          class="max-h-72 divide-y divide-zinc-800 overflow-y-auto rounded-md border border-zinc-800 bg-zinc-950/40"
        >
          {#each results as game (game.slug + game.found_paths[0])}
            {@const isAdded = addedSlugs.includes(game.slug)}
            <li class="flex items-center justify-between gap-3 px-3 py-2.5">
              <div class="min-w-0">
                <p class="truncate text-sm font-medium text-zinc-100">
                  {game.display_name}
                </p>
                <p class="truncate font-mono text-[11px] text-zinc-500">
                  {game.found_paths[0]}
                </p>
              </div>
              {#if isAdded}
                <span
                  class="flex shrink-0 items-center gap-1 text-xs text-emerald-400"
                >
                  <Check size={14} />
                  {$_("scan_folder.tracked")}
                </span>
              {:else}
                <Button
                  variant="secondary"
                  onclick={() => track(game)}
                  loading={trackingSlug === game.slug}
                  disabled={trackingSlug !== null}
                >
                  <Plus size={13} />
                  {$_("scan_folder.track")}
                </Button>
              {/if}
            </li>
          {/each}
        </ul>
      </div>
    {:else}
      <p class="py-6 text-center text-sm text-zinc-500">
        {$_("scan_folder.hint")}
      </p>
    {/if}
  </div>

  {#snippet footer()}
    <Button variant="ghost" onclick={close}>
      {$_("common.close")}
    </Button>
  {/snippet}
</Modal>
