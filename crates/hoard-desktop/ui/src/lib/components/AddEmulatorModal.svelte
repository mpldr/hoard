<script lang="ts">
  /**
   * "Add emulator" dialog (Hoard free).
   *
   * Emulators have no storefront/Steam/manifest entry, so Hoard can't
   * auto-detect their save folder or guess "is the user playing". This dialog
   * collects both by hand — folder + the emulator's executable(s) — and tracks
   * the result through the normal `addGameToTracking` with `processes` and
   * `backup_only` pinned. From there backup/sync/restore and the playtime/date
   * signal (which hoard-pro's recap later consumes) work like any other save.
   *
   * Kept as its own component so none of this touches the existing detection
   * flow in Library.svelte.
   */
  import { open as openDialog } from "@tauri-apps/plugin-dialog";
  import { FolderOpen, Plus, X, Gamepad2, Cpu } from "lucide-svelte";
  import { _ } from "svelte-i18n";

  import Modal from "./Modal.svelte";
  import Button from "./Button.svelte";
  import Input from "./Input.svelte";
  import {
    listEmulatorPresets,
    listRunningProcesses,
    addGameToTracking,
    type EmulatorPreset,
    type RunningProcess,
    type TrackedSave,
  } from "../api";
  import { toastError, toastSuccess } from "../stores/toasts";

  type Props = {
    open: boolean;
    onClose: () => void;
    onAdded: (save: TrackedSave) => void;
  };
  let { open, onClose, onAdded }: Props = $props();

  let presets = $state<EmulatorPreset[]>([]);
  let loadingPresets = $state(false);
  // "" = nothing chosen yet, "custom" = an emulator not in the catalog.
  let selectedId = $state("");
  let customName = $state("");
  let folder = $state("");
  let procs = $state<string[]>([]);
  let manualProc = $state("");
  let backupOnly = $state(true);

  let running = $state<RunningProcess[]>([]);
  let detected = $state(false);
  let detecting = $state(false);
  let submitting = $state(false);

  const selectedPreset = $derived(
    presets.find((p) => p.id === selectedId) ?? null,
  );
  const isCustom = $derived(selectedId === "custom");
  const canSubmit = $derived(
    selectedId.length > 0 &&
      folder.trim().length > 0 &&
      procs.length > 0 &&
      (!isCustom || customName.trim().length > 0),
  );

  // Lazy-load the catalog the first time the dialog opens.
  $effect(() => {
    if (open && presets.length === 0 && !loadingPresets) {
      void loadPresets();
    }
  });

  async function loadPresets() {
    loadingPresets = true;
    try {
      presets = await listEmulatorPresets();
    } catch (e) {
      toastError(typeof e === "string" ? e : (e as Error).message);
    } finally {
      loadingPresets = false;
    }
  }

  /** Pre-fill folder + processes from the chosen catalog entry. */
  function onSelect() {
    if (selectedPreset) {
      procs = [...selectedPreset.processes];
      folder = selectedPreset.save_paths[0] ?? "";
    } else {
      // "Otro…" — start blank, let the user fill everything in.
      procs = [];
      folder = "";
    }
    running = [];
    detected = false;
  }

  async function pickFolder() {
    try {
      const result = await openDialog({
        directory: true,
        multiple: false,
        defaultPath: folder || undefined,
        title: $_("emulators.folder_label"),
      });
      if (typeof result === "string" && result.length > 0) folder = result;
    } catch (e) {
      toastError(typeof e === "string" ? e : (e as Error).message);
    }
  }

  async function detect() {
    detecting = true;
    try {
      running = await listRunningProcesses();
      detected = true;
    } catch (e) {
      toastError(typeof e === "string" ? e : (e as Error).message);
    } finally {
      detecting = false;
    }
  }

  function addProc(name: string) {
    const n = name.trim();
    if (n.length === 0) return;
    if (procs.some((p) => p.toLowerCase() === n.toLowerCase())) return;
    procs = [...procs, n];
  }

  function addManualProc() {
    addProc(manualProc);
    manualProc = "";
  }

  function removeProc(name: string) {
    procs = procs.filter((p) => p !== name);
  }

  function slugify(s: string): string {
    return s
      .trim()
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-+|-+$/g, "");
  }

  function reset() {
    selectedId = "";
    customName = "";
    folder = "";
    procs = [];
    manualProc = "";
    backupOnly = true;
    running = [];
    detected = false;
  }

  function close() {
    reset();
    onClose();
  }

  async function submit() {
    if (!canSubmit || submitting) return;
    submitting = true;
    try {
      const slug = isCustom
        ? `emu-${slugify(customName)}`
        : `emu-${selectedId}`;
      const display = isCustom
        ? customName.trim()
        : (selectedPreset?.display_name ?? selectedId);
      const saved = await addGameToTracking({
        game_slug: slug,
        local_path: folder.trim(),
        display_name: display,
        preset: backupOnly ? "backup_only" : undefined,
        processes: procs,
      });
      onAdded(saved);
      toastSuccess($_("emulators.added", { values: { name: display } }));
      reset();
      onClose();
    } catch (e) {
      toastError(typeof e === "string" ? e : (e as Error).message);
    } finally {
      submitting = false;
    }
  }
</script>

<Modal
  {open}
  title={$_("emulators.title")}
  description={$_("emulators.description")}
  onClose={close}
>
  <div class="space-y-4">
    <!-- Emulator picker -->
    <div>
      <label
        for="emu-select"
        class="mb-1.5 block text-xs font-medium text-zinc-400"
      >
        {$_("emulators.choose")}
      </label>
      <select
        id="emu-select"
        bind:value={selectedId}
        onchange={onSelect}
        disabled={loadingPresets}
        class="w-full rounded-md border border-zinc-800 bg-zinc-950/60 px-3 py-2 text-sm text-zinc-100 focus:border-emerald-600 focus:outline-none focus:ring-1 focus:ring-emerald-600 disabled:opacity-50"
      >
        <option value="" disabled>{$_("emulators.choose_placeholder")}</option>
        {#each presets as p (p.id)}
          <option value={p.id}>{p.display_name} — {p.system}</option>
        {/each}
        <option value="custom">{$_("emulators.custom")}</option>
      </select>
    </div>

    {#if isCustom}
      <div>
        <label
          for="emu-custom-name"
          class="mb-1.5 block text-xs font-medium text-zinc-400"
        >
          {$_("emulators.custom_name_label")}
        </label>
        <Input
          id="emu-custom-name"
          bind:value={customName}
          placeholder={$_("emulators.custom_name_placeholder")}
        />
      </div>
    {/if}

    {#if selectedId.length > 0}
      <!-- Save folder -->
      <div>
        <label
          for="emu-folder"
          class="mb-1.5 block text-xs font-medium text-zinc-400"
        >
          {$_("emulators.folder_label")}
        </label>
        <div class="flex gap-2">
          <Input
            id="emu-folder"
            class="flex-1"
            bind:value={folder}
            placeholder={$_("emulators.folder_placeholder")}
          />
          <Button variant="secondary" onclick={pickFolder}>
            <FolderOpen size={14} />
            {$_("emulators.browse")}
          </Button>
        </div>
        <p class="mt-1.5 text-xs text-zinc-500">
          {$_("emulators.folder_hint")}
        </p>
      </div>

      <!-- Process(es) -->
      <div>
        <span class="mb-1.5 block text-xs font-medium text-zinc-400">
          {$_("emulators.processes_label")}
        </span>

        {#if procs.length > 0}
          <div class="mb-2 flex flex-wrap gap-1.5">
            {#each procs as p (p)}
              <span
                class="inline-flex items-center gap-1 rounded-md bg-emerald-600/15 px-2 py-1 font-mono text-xs text-emerald-300 ring-1 ring-inset ring-emerald-600/30"
              >
                {p}
                <button
                  type="button"
                  onclick={() => removeProc(p)}
                  aria-label={$_("emulators.remove_proc")}
                  class="rounded-sm text-emerald-400/70 hover:text-emerald-200"
                >
                  <X size={12} />
                </button>
              </span>
            {/each}
          </div>
        {/if}

        <div class="flex gap-2">
          <Input
            class="flex-1"
            bind:value={manualProc}
            placeholder={$_("emulators.process_placeholder")}
            onkeydown={(e: KeyboardEvent) => {
              if (e.key === "Enter") {
                e.preventDefault();
                addManualProc();
              }
            }}
          />
          <Button
            variant="secondary"
            onclick={addManualProc}
            disabled={manualProc.trim().length === 0}
          >
            <Plus size={14} />
            {$_("emulators.add_proc")}
          </Button>
        </div>

        <div class="mt-2">
          <Button variant="ghost" onclick={detect} loading={detecting}>
            <Cpu size={14} />
            {detecting ? $_("emulators.detecting") : $_("emulators.detect")}
          </Button>
        </div>

        {#if detected && running.length === 0}
          <p class="mt-2 text-xs text-zinc-500">
            {$_("emulators.no_processes_found")}
          </p>
        {/if}
        {#if running.length > 0}
          <ul
            class="mt-2 max-h-40 divide-y divide-zinc-800 overflow-y-auto rounded-md border border-zinc-800 bg-zinc-950/40"
          >
            {#each running as proc (proc.name)}
              <li class="flex items-center justify-between gap-2 px-3 py-1.5">
                <span class="flex min-w-0 items-center gap-2">
                  <Gamepad2 size={13} class="shrink-0 text-zinc-500" />
                  <span class="truncate font-mono text-xs text-zinc-300"
                    >{proc.name}</span
                  >
                  <span class="shrink-0 tabular-nums text-[10px] text-zinc-600"
                    >{proc.cpu.toFixed(0)}%</span
                  >
                </span>
                <button
                  type="button"
                  onclick={() => addProc(proc.name)}
                  disabled={procs.some(
                    (p) => p.toLowerCase() === proc.name.toLowerCase(),
                  )}
                  class="shrink-0 rounded-md px-2 py-0.5 text-xs text-emerald-400 hover:bg-emerald-600/10 disabled:text-zinc-600 disabled:hover:bg-transparent"
                >
                  <Plus size={13} />
                </button>
              </li>
            {/each}
          </ul>
        {/if}
        <p class="mt-1.5 text-xs text-zinc-500">
          {$_("emulators.processes_hint")}
        </p>
      </div>

      <!-- Backup-only toggle -->
      <label class="flex items-start gap-2.5 text-sm text-zinc-300">
        <input
          type="checkbox"
          bind:checked={backupOnly}
          class="mt-0.5 h-4 w-4 rounded border-zinc-700 bg-zinc-950 text-emerald-600 focus:ring-emerald-600"
        />
        <span>
          {$_("emulators.backup_only_label")}
          <span class="mt-0.5 block text-xs text-zinc-500">
            {$_("emulators.backup_only_hint")}
          </span>
        </span>
      </label>
    {/if}
  </div>

  {#snippet footer()}
    <Button variant="ghost" onclick={close}>
      {$_("common.cancel")}
    </Button>
    <Button onclick={submit} loading={submitting} disabled={!canSubmit}>
      <Plus size={14} />
      {submitting ? $_("emulators.submitting") : $_("emulators.submit")}
    </Button>
  {/snippet}
</Modal>
