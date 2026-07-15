<script lang="ts">
  /**
   * "Vincular a esta máquina" for a cloud orphan — a save that lives in the
   * cloud from ANOTHER machine, with no local folder here.
   *
   * The folder picker used to be the only way through, even when detection
   * already knew where the game saves on this machine. So detected folders
   * come first as one-click options and the picker stays as the fallback.
   *
   * Cold cache (never scanned — the case for anyone who never turned on Modo
   * Automático) is deliberately NOT rendered as "nothing found": we don't
   * know, so we offer the scan.
   *
   * Adopting is the parent's job (`onPick`): Library.svelte owns the toast and
   * the tracked-list refresh, and this modal shouldn't fork that.
   */
  import { open as openDialog } from "@tauri-apps/plugin-dialog";
  import { FolderOpen, Radar, Link } from "lucide-svelte";
  import { _ } from "svelte-i18n";

  import Modal from "./Modal.svelte";
  import Button from "./Button.svelte";
  import {
    detectedPathsForGame,
    scanLibrary,
    type Confidence,
    type DetectionReport,
    type LocalDetection,
    type TrackedSave,
  } from "../api";
  import { toastError } from "../stores/toasts";

  type Props = {
    open: boolean;
    orphan: TrackedSave | null;
    onClose: () => void;
    onPick: (path: string) => void;
    /** Lets Library.svelte adopt the report from an in-modal scan instead of
     *  leaving its own cards stale. */
    onScanned?: (report: DetectionReport) => void;
  };
  let { open, orphan, onClose, onPick, onScanned }: Props = $props();

  let detection = $state<LocalDetection | null>(null);
  let loading = $state(false);
  let scanning = $state(false);

  // `null` scanned_at = no cache at all. Distinct from "scanned and found
  // nothing", which is a real answer and only earns the picker.
  const neverScanned = $derived(detection !== null && detection.scanned_at === null);
  const paths = $derived(detection?.paths ?? []);

  // Re-query whenever a different orphan opens the modal. Cheap: the command
  // is an in-memory cache lookup, no scan.
  $effect(() => {
    if (open && orphan) {
      void load(orphan.game_slug);
    } else if (!open) {
      detection = null;
    }
  });

  async function load(slug: string) {
    loading = true;
    try {
      detection = await detectedPathsForGame(slug);
    } catch (e) {
      toastError(typeof e === "string" ? e : (e as Error).message);
      detection = null;
    } finally {
      loading = false;
    }
  }

  /** Cold cache: run the normal sweep, then re-read what it found for us. */
  async function scan() {
    if (!orphan || scanning) return;
    scanning = true;
    try {
      const report = await scanLibrary();
      onScanned?.(report);
      detection = await detectedPathsForGame(orphan.game_slug);
    } catch (e) {
      toastError(typeof e === "string" ? e : (e as Error).message);
    } finally {
      scanning = false;
    }
  }

  async function pickOther() {
    if (!orphan) return;
    try {
      const result = await openDialog({
        directory: true,
        multiple: false,
        // Land on the strongest detected folder when we have one, so "otra
        // carpeta" starts next door instead of at Documents.
        defaultPath: paths[0]?.path || undefined,
        title: $_("library.pick_folder_title", {
          values: { name: orphan.game_slug },
        }),
      });
      if (typeof result === "string" && result.length > 0) onPick(result);
    } catch (e) {
      toastError(typeof e === "string" ? e : (e as Error).message);
    }
  }

  function confidenceLabel(c: Confidence): string {
    return c === "high"
      ? $_("library.high")
      : c === "medium"
        ? $_("library.medium")
        : $_("library.low");
  }

  function confidenceClass(c: Confidence): string {
    return c === "high"
      ? "bg-emerald-500/10 text-emerald-300 ring-emerald-500/30"
      : c === "medium"
        ? "bg-sky-500/10 text-sky-300 ring-sky-500/30"
        : "bg-zinc-500/10 text-zinc-400 ring-zinc-500/30";
  }
</script>

<Modal
  {open}
  title={$_("library.link_to_machine")}
  description={orphan
    ? $_("library.link_modal_desc", { values: { name: orphan.game_slug } })
    : undefined}
  {onClose}
>
  <div class="space-y-4">
    {#if loading}
      <p class="text-sm text-zinc-500">{$_("library.link_loading")}</p>
    {:else}
      {#if paths.length > 0}
        <div>
          <span class="mb-1.5 block text-xs font-medium text-zinc-400">
            {$_("library.link_detected_heading")}
          </span>
          <ul class="space-y-1.5">
            {#each paths as p (p.path)}
              <li>
                <button
                  type="button"
                  onclick={() => onPick(p.path)}
                  class="group flex w-full items-center gap-2.5 rounded-lg border border-white/[0.08] bg-zinc-950/60 px-3 py-2.5 text-left transition-colors hover:border-emerald-600/40 hover:bg-emerald-600/10"
                >
                  <Link
                    size={14}
                    class="shrink-0 text-zinc-500 group-hover:text-emerald-300"
                  />
                  <span
                    class="min-w-0 flex-1 truncate font-mono text-xs text-zinc-300 group-hover:text-zinc-100"
                    title={p.path}
                  >
                    {p.path}
                  </span>
                  <span
                    class="shrink-0 rounded px-1.5 py-0.5 text-[10px] font-medium ring-1 ring-inset {confidenceClass(
                      p.confidence,
                    )}"
                  >
                    {confidenceLabel(p.confidence)}
                  </span>
                </button>
              </li>
            {/each}
          </ul>
          <p class="mt-1.5 text-xs text-zinc-500">
            {$_("library.link_detected_hint")}
          </p>
        </div>
      {:else if neverScanned}
        <div class="rounded-lg border border-white/[0.08] bg-zinc-950/60 p-3">
          <p class="text-sm text-zinc-300">{$_("library.link_never_scanned")}</p>
          <div class="mt-2.5">
            <Button variant="secondary" onclick={scan} loading={scanning}>
              <Radar size={14} />
              {scanning
                ? $_("library.link_scanning")
                : $_("library.link_scan_button")}
            </Button>
          </div>
        </div>
      {:else}
        <p class="text-sm text-zinc-500">{$_("library.link_no_detection")}</p>
      {/if}
    {/if}
  </div>

  {#snippet footer()}
    <Button variant="ghost" onclick={onClose}>
      {$_("common.cancel")}
    </Button>
    <!-- With detected folders on offer THEY are the primary action (emerald on
         hover) and the picker is the escape hatch; with none, the picker is the
         only way forward and takes the emerald. -->
    <Button
      variant={paths.length > 0 ? "secondary" : "primary"}
      onclick={pickOther}
    >
      <FolderOpen size={14} />
      {$_("library.link_other_folder")}
    </Button>
  {/snippet}
</Modal>
