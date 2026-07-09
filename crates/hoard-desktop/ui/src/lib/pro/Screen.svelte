<script module lang="ts">
  // Escena viva del overlay, guardada a nivel de MÓDULO (no de instancia) para
  // que sobreviva al desmontaje del componente: al cambiar de Screen a otra
  // vista y volver, el proceso overlay nativo sigue corriendo, así que sus
  // paneles deben reaparecer sin tener que entrar en modo editor para que el
  // overlay reenvíe la escena. `onMount` la rehidrata cuando `screen_is_open`,
  // `onDestroy` la vuelca antes de irse. No se persiste a disco: al cerrar la
  // app el overlay muere y `screen_is_open` pasa a false.
  export type Panel = {
    id: string;
    windowId: string;
    label: string;
    x: number;
    y: number;
    w: number;
    h: number;
    crop: { top: number; right: number; bottom: number; left: number };
    scale: "fill" | "fit";
    z: number;
    // En modo Juego la ventana es click-through (los clics pasan al juego). Con
    // contenido de vídeo protegido/acelerado eso puede dejarla en negro; poner
    // `passthrough: false` conserva el estilo original de la ventana (el vídeo
    // compone normal) a cambio de que los clics sobre ese panel vayan a la app.
    passthrough: boolean;
    // Modo compatibilidad Chromium: en lugar de colocar la ventana real
    // recortada (cuyo recorte deja gris en Brave/Discord), captura la ventana
    // y compone el recorte pixel-perfect; la ventana real se aparca fuera de
    // pantalla. Puede dejar el vídeo en negro al reproducir (oclusión Chromium).
    compat: boolean;
    // Radio (px) de la lente circular click-through que sigue al cursor cuando
    // `passthrough` está ON: dentro del círculo ves/clicas lo que hay detrás del
    // panel; fuera, el panel se ve normal.
    passthroughRadius: number;
    // Which screen this panel lives on. `mirror` = drawn on every monitor;
    // otherwise it's drawn only on `monitorId`. Rects are monitor-local.
    monitorId: number;
    mirror: boolean;
  };

  let savedPanels: Panel[] = [];
</script>

<script lang="ts">
  // hoard-screen launcher + editor. Drives the NATIVE overlay process
  // (`hoard-screen` sidecar) — not a Tauri window anymore.
  //
  // Model (v1): the overlay only composites panels and stays click-through
  // (View). Arranging happens here, in the main window, on a scaled preview of
  // the monitor; every change is pushed to the overlay as a `set_scene` line so
  // the result shows live over the desktop/game. Direct manipulation on the
  // overlay itself (grabbing the mouse there) is a later function.
  import { onMount, onDestroy } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { primaryMonitor } from "@tauri-apps/api/window";
  import {
    MonitorPlay,
    SquarePlus,
    SquareX,
    RefreshCw,
    Trash2,
    TriangleAlert,
    Maximize,
    Proportions,
    ArrowUp,
    ArrowDown,
    Gamepad2,
    Pencil,
    Crop,
    RotateCcw,
    MousePointerClick,
  } from "lucide-svelte";
  import { tr } from "./lib";

  type WinInfo = { id: string; title: string; app: string; protected: boolean };
  type Monitor = {
    id: number;
    name: string;
    x: number;
    y: number;
    w: number;
    h: number;
    primary: boolean;
  };
  // `type Panel` vive en el `<script module>` de arriba (se comparte con el
  // buffer `savedPanels` que sobrevive al desmontaje).

  // px of the on-screen preview; overlay maps to monitor. Capped to the
  // rendered width of the preview column so a narrow window (min 800px wide)
  // doesn't force a fixed 560px box wider than the space actually available.
  let previewColWidth = $state(560);
  const PREVIEW_W = $derived(Math.min(560, previewColWidth));

  let open = $state(false);
  let busy = $state(false);
  let error = $state<string | null>(null);
  let windows = $state<WinInfo[]>([]);
  let panels = $state<Panel[]>([]);
  let selectedId = $state<string | null>(null);
  // Game (click-through) vs Edit (overlay grabs input; panels draggable on the
  // overlay itself). Mirrors the overlay; Ctrl+O / Esc flip it there too.
  let editing = $state(false);

  // Physical monitors (from the native overlay's enumeration). The overlay
  // draws one click-through window per monitor; a panel's rect is local to its
  // target monitor's top-left. `activeMonId` is the screen currently shown in
  // the preview / arranged here.
  let monitors = $state<Monitor[]>([]);
  let activeMonId = $state(0);

  const fallbackMon: Monitor = { id: 0, name: "", x: 0, y: 0, w: 1920, h: 1080, primary: true };
  const activeMon = $derived(
    monitors.find((m) => m.id === activeMonId) ?? monitors[0] ?? fallbackMon,
  );
  // The preview's coordinate space is the active monitor's pixel size.
  const mon = $derived({ w: activeMon.w, h: activeMon.h });
  // Panels drawn on the active screen: its own plus any mirrored everywhere.
  const visiblePanels = $derived(
    panels.filter((p) => p.mirror || p.monitorId === activeMonId),
  );

  const previewScale = $derived(PREVIEW_W / mon.w);
  const previewH = $derived(Math.round(mon.h * previewScale));
  const selected = $derived(panels.find((p) => p.id === selectedId) ?? null);

  function monLabel(m: Monitor) {
    const n = monitors.findIndex((x) => x.id === m.id) + 1;
    return `${tr({ es: "Pantalla", en: "Screen" })} ${n}${m.primary ? " ★" : ""}`;
  }

  function sceneJson() {
    return {
      panels: panels.map((p) => ({
        id: p.id,
        source: { kind: "window", id: p.windowId },
        rect: { x: p.x, y: p.y, w: p.w, h: p.h },
        crop: p.crop,
        scale: p.scale,
        z: p.z,
        passthrough: p.passthrough,
        compat: p.compat,
        passthrough_radius: p.passthroughRadius,
        monitor: p.mirror ? { kind: "all" } : { kind: "monitor", id: p.monitorId },
      })),
    };
  }

  async function pushScene() {
    if (!open) return;
    try {
      await invoke("screen_send", {
        line: JSON.stringify({ type: "set_scene", scene: sceneJson() }),
      });
    } catch (e) {
      error = String(e);
    }
  }

  async function loadWindows() {
    try {
      const raw = await invoke<string>("screen_list_windows");
      const parsed = JSON.parse(raw || "[]");
      windows = Array.isArray(parsed) ? parsed : [];
    } catch (e) {
      error = String(e);
      windows = [];
    }
  }

  async function setEditor(on: boolean) {
    editing = on;
    if (!open) return;
    try {
      await invoke("screen_send", {
        line: JSON.stringify({ type: "set_editor", editor: on }),
      });
    } catch (e) {
      error = String(e);
    }
  }

  // Scene pushed back by the overlay after an in-overlay drag/resize: adopt the
  // new geometry/z, keeping our human labels. Skipped while dragging here.
  function applyIncomingScene(scene: any) {
    if (drag) return;
    const sp = scene?.panels;
    if (!Array.isArray(sp)) return;
    panels = sp.map((p: any) => {
      const prev = panels.find((q) => q.id === p.id);
      const t = p.monitor;
      return {
        id: p.id,
        windowId: p.source?.id ?? "",
        label: prev?.label ?? p.source?.id ?? p.id,
        x: p.rect.x,
        y: p.rect.y,
        w: p.rect.w,
        h: p.rect.h,
        crop: p.crop ?? { top: 0, right: 0, bottom: 0, left: 0 },
        scale: p.scale ?? "fill",
        z: p.z ?? 0,
        passthrough: p.passthrough ?? true,
        compat: p.compat ?? false,
        passthroughRadius: p.passthrough_radius ?? 90,
        mirror: t?.kind === "all",
        monitorId: t?.kind === "monitor" ? (t.id ?? 0) : (prev?.monitorId ?? 0),
      };
    });
  }

  async function loadMonitors() {
    try {
      const raw = await invoke<string>("screen_list_monitors");
      const parsed = JSON.parse(raw || "[]");
      if (Array.isArray(parsed) && parsed.length) {
        monitors = parsed;
      } else {
        // Platform without native enumeration: a single screen from the web API.
        const m = await primaryMonitor();
        monitors = [
          { id: 0, name: "", x: 0, y: 0, w: m?.size?.width ?? 1920, h: m?.size?.height ?? 1080, primary: true },
        ];
      }
      if (!monitors.find((m) => m.id === activeMonId)) {
        activeMonId = monitors[0]?.id ?? 0;
      }
    } catch (e) {
      error = String(e);
    }
  }

  let unlisten: UnlistenFn | null = null;
  let pollTimer: ReturnType<typeof setInterval> | null = null;

  function startPolling() {
    stopPolling();
    pollTimer = setInterval(loadWindows, 2500);
  }
  function stopPolling() {
    if (pollTimer) clearInterval(pollTimer);
    pollTimer = null;
  }

  async function openOverlay() {
    if (busy) return;
    busy = true;
    error = null;
    try {
      await loadMonitors();
      await invoke("screen_open");
      // Start in Game mode (click-through); the user flips to Edit when ready.
      editing = false;
      await invoke("screen_send", {
        line: JSON.stringify({ type: "set_editor", editor: false }),
      });
      open = true;
      await loadWindows();
      await pushScene();
      startPolling();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function closeOverlay() {
    stopPolling();
    try {
      await invoke("screen_close");
    } catch (e) {
      error = String(e);
    }
    open = false;
    editing = false;
  }

  function addPanel(win: WinInfo) {
    const w = Math.round(mon.w / 3);
    const h = Math.round(mon.h / 3);
    const id = `p${Date.now().toString(36)}`;
    const z = panels.reduce((m, p) => Math.max(m, p.z), 0) + 1;
    panels.push({
      id,
      windowId: win.id,
      label: win.app || win.title || win.id,
      x: Math.round((mon.w - w) / 2),
      y: Math.round((mon.h - h) / 2),
      w,
      h,
      crop: { top: 0, right: 0, bottom: 0, left: 0 },
      scale: "fill",
      z,
      passthrough: true,
      compat: false,
      passthroughRadius: 90,
      monitorId: activeMonId,
      mirror: false,
    });
    selectedId = id;
    pushScene();
  }

  function setPanelTarget(p: Panel, value: string) {
    if (value === "all") {
      p.mirror = true;
    } else {
      p.mirror = false;
      p.monitorId = Number(value);
    }
    pushScene();
  }

  function removePanel(id: string) {
    panels = panels.filter((p) => p.id !== id);
    if (selectedId === id) selectedId = null;
    pushScene();
  }

  function bumpZ(p: Panel, dir: 1 | -1) {
    p.z += dir;
    pushScene();
  }

  // --- drag / resize on the preview ---------------------------------------
  const MIN_W = 80;
  const MIN_H = 60;
  const SNAP = 10; // imán en px de preview
  // Un panel puede sobresalir del monitor (la ventana real se coloca off-screen
  // sin problema). Dejamos al menos VIS px dentro para poder volver a agarrarlo
  // en el editor, y permitimos hasta un monitor de desbordamiento por lado.
  const VIS = 48;
  const lo = (size: number, _span: number) => Math.min(VIS - size, 0);
  const hi = (span: number) => span - VIS;

  type Handle = "move" | "n" | "s" | "e" | "w" | "ne" | "nw" | "se" | "sw";

  // Tiradores de redimensión: el de arrastre completo ("move") es el propio
  // cuerpo del panel; estos son los 8 bordes/esquinas que se pintan al
  // seleccionar.
  const HANDLES: { h: Handle; cls: string }[] = [
    { h: "nw", cls: "-left-1 -top-1 cursor-nwse-resize" },
    { h: "n", cls: "left-1/2 -top-1 -translate-x-1/2 cursor-ns-resize" },
    { h: "ne", cls: "-right-1 -top-1 cursor-nesw-resize" },
    { h: "e", cls: "-right-1 top-1/2 -translate-y-1/2 cursor-ew-resize" },
    { h: "se", cls: "-right-1 -bottom-1 cursor-nwse-resize" },
    { h: "s", cls: "left-1/2 -bottom-1 -translate-x-1/2 cursor-ns-resize" },
    { h: "sw", cls: "-left-1 -bottom-1 cursor-nesw-resize" },
    { h: "w", cls: "-left-1 top-1/2 -translate-y-1/2 cursor-ew-resize" },
  ];

  const clamp = (v: number, a: number, b: number) => Math.max(a, Math.min(b, v));

  // Imanta un valor a uno de varios objetivos si queda dentro del umbral
  // (expresado en px de monitor, por eso dividimos SNAP por la escala).
  function snap(v: number, targets: number[]): number {
    const tol = SNAP / previewScale;
    for (const t of targets) if (Math.abs(v - t) <= tol) return t;
    return v;
  }

  let drag: {
    id: string;
    handle: Handle;
    sx: number;
    sy: number;
    ox: number;
    oy: number;
    ow: number;
    oh: number;
  } | null = null;

  function onPointerDown(e: PointerEvent, p: Panel, handle: Handle) {
    e.preventDefault();
    e.stopPropagation();
    selectedId = p.id;
    drag = { id: p.id, handle, sx: e.clientX, sy: e.clientY, ox: p.x, oy: p.y, ow: p.w, oh: p.h };
    window.addEventListener("pointermove", onPointerMove);
    window.addEventListener("pointerup", onPointerUp);
  }

  function onPointerMove(e: PointerEvent) {
    if (!drag) return;
    const p = panels.find((q) => q.id === drag!.id);
    if (!p) return;
    const dx = (e.clientX - drag.sx) / previewScale;
    const dy = (e.clientY - drag.sy) / previewScale;
    const free = e.shiftKey; // Shift desactiva el imán

    if (drag.handle === "move") {
      let nx = drag.ox + dx;
      let ny = drag.oy + dy;
      if (!free) {
        nx = snap(nx, [0, (mon.w - p.w) / 2, mon.w - p.w]);
        ny = snap(ny, [0, (mon.h - p.h) / 2, mon.h - p.h]);
      }
      p.x = clamp(Math.round(nx), lo(p.w, mon.w), hi(mon.w));
      p.y = clamp(Math.round(ny), lo(p.h, mon.h), hi(mon.h));
    } else {
      const h = drag.handle;
      let left = drag.ox;
      let top = drag.oy;
      let right = drag.ox + drag.ow;
      let bottom = drag.oy + drag.oh;
      if (h.includes("e")) right = drag.ox + drag.ow + dx;
      if (h.includes("w")) left = drag.ox + dx;
      if (h.includes("s")) bottom = drag.oy + drag.oh + dy;
      if (h.includes("n")) top = drag.oy + dy;
      if (!free) {
        if (h.includes("e")) right = snap(right, [mon.w, mon.w / 2]);
        if (h.includes("w")) left = snap(left, [0, mon.w / 2]);
        if (h.includes("s")) bottom = snap(bottom, [mon.h, mon.h / 2]);
        if (h.includes("n")) top = snap(top, [0, mon.h / 2]);
      }
      // Permite que los bordes salgan del monitor (hasta un monitor por lado).
      left = clamp(left, -mon.w, 2 * mon.w);
      right = clamp(right, -mon.w, 2 * mon.w);
      top = clamp(top, -mon.h, 2 * mon.h);
      bottom = clamp(bottom, -mon.h, 2 * mon.h);
      // Respeta el tamaño mínimo moviendo solo el borde que se arrastra.
      if (h.includes("w")) left = Math.min(left, right - MIN_W);
      else right = Math.max(right, left + MIN_W);
      if (h.includes("n")) top = Math.min(top, bottom - MIN_H);
      else bottom = Math.max(bottom, top + MIN_H);
      p.x = Math.round(left);
      p.y = Math.round(top);
      p.w = Math.round(right - left);
      p.h = Math.round(bottom - top);
    }
    pushScene();
  }

  function onPointerUp() {
    drag = null;
    window.removeEventListener("pointermove", onPointerMove);
    window.removeEventListener("pointerup", onPointerUp);
  }

  // Mover/redimensionar el panel seleccionado con el teclado (flechas; Shift =
  // paso de 10; Alt = redimensiona en vez de mover).
  function onKeydown(e: KeyboardEvent) {
    const s = selected;
    if (!s || cropMode) return;
    const tag = (e.target as HTMLElement | null)?.tagName;
    if (tag === "INPUT" || tag === "SELECT" || tag === "TEXTAREA") return;
    if (!e.key.startsWith("Arrow")) return;
    const step = e.shiftKey ? 10 : 1;
    if (e.altKey) {
      if (e.key === "ArrowRight") s.w = Math.max(MIN_W, s.w + step);
      else if (e.key === "ArrowLeft") s.w = Math.max(MIN_W, s.w - step);
      else if (e.key === "ArrowDown") s.h = Math.max(MIN_H, s.h + step);
      else s.h = Math.max(MIN_H, s.h - step);
    } else {
      if (e.key === "ArrowRight") s.x = clamp(s.x + step, lo(s.w, mon.w), hi(mon.w));
      else if (e.key === "ArrowLeft") s.x = clamp(s.x - step, lo(s.w, mon.w), hi(mon.w));
      else if (e.key === "ArrowDown") s.y = clamp(s.y + step, lo(s.h, mon.h), hi(mon.h));
      else s.y = clamp(s.y - step, lo(s.h, mon.h), hi(mon.h));
    }
    e.preventDefault();
    pushScene();
  }

  // Edición numérica directa de la caja del panel.
  function setRect(p: Panel, key: "x" | "y" | "w" | "h", v: number) {
    if (!Number.isFinite(v)) return;
    v = Math.round(v);
    if (key === "w") p.w = Math.max(MIN_W, v);
    else if (key === "h") p.h = Math.max(MIN_H, v);
    else if (key === "x") p.x = clamp(v, lo(p.w, mon.w), hi(mon.w));
    else p.y = clamp(v, lo(p.h, mon.h), hi(mon.h));
    pushScene();
  }

  // --- recorte -------------------------------------------------------------
  // Modo recorte visual del panel seleccionado: se ve la ventana entera y lo
  // que cae fuera del recorte se atenúa, con tiradores arrastrables por borde.
  let cropId = $state<string | null>(null);
  const cropMode = $derived(cropId === selectedId && !!selected);
  function toggleCrop() {
    cropId = cropMode ? null : selectedId;
  }

  function setCrop(p: Panel, edge: keyof Panel["crop"], v: number) {
    // El recorte no puede tragarse el borde opuesto: deja al menos un 10% vivo.
    const opp =
      edge === "top" ? "bottom" : edge === "bottom" ? "top" : edge === "left" ? "right" : "left";
    const max = Math.max(0, 0.9 - p.crop[opp]);
    p.crop[edge] = clamp(v, 0, max);
    pushScene();
  }

  function resetCrop(p: Panel) {
    p.crop = { top: 0, right: 0, bottom: 0, left: 0 };
    pushScene();
  }

  let cropDrag: {
    id: string;
    edge: keyof Panel["crop"];
    sx: number;
    sy: number;
    start: number;
  } | null = null;

  function onCropDown(e: PointerEvent, p: Panel, edge: keyof Panel["crop"]) {
    e.preventDefault();
    e.stopPropagation();
    cropDrag = { id: p.id, edge, sx: e.clientX, sy: e.clientY, start: p.crop[edge] };
    window.addEventListener("pointermove", onCropMove);
    window.addEventListener("pointerup", onCropUp);
  }

  function onCropMove(e: PointerEvent) {
    if (!cropDrag) return;
    const p = panels.find((q) => q.id === cropDrag!.id);
    if (!p) return;
    const pxW = p.w * previewScale;
    const pxH = p.h * previewScale;
    const d = cropDrag;
    let v = d.start;
    if (d.edge === "left") v = d.start + (e.clientX - d.sx) / pxW;
    else if (d.edge === "right") v = d.start - (e.clientX - d.sx) / pxW;
    else if (d.edge === "top") v = d.start + (e.clientY - d.sy) / pxH;
    else v = d.start - (e.clientY - d.sy) / pxH;
    setCrop(p, d.edge, v);
  }

  function onCropUp() {
    cropDrag = null;
    window.removeEventListener("pointermove", onCropMove);
    window.removeEventListener("pointerup", onCropUp);
  }

  const pct = (v: number) => `${Math.round(v * 100)}%`;

  onMount(async () => {
    window.addEventListener("keydown", onKeydown);
    try {
      unlisten = await listen<string>("screen://event", (ev) => {
        try {
          const msg = JSON.parse(ev.payload);
          if (msg.type === "mode") editing = !!msg.editor;
          else if (msg.type === "scene") applyIncomingScene(msg.scene);
        } catch {
          /* ignore malformed */
        }
      });
    } catch {
      /* no event bus (shouldn't happen) */
    }
    try {
      open = await invoke<boolean>("screen_is_open");
      if (open) {
        // El overlay sigue vivo de un montaje anterior: recupera su escena para
        // que los paneles se pinten en el preview sin tener que pasar por editar.
        panels = savedPanels;
        await loadMonitors();
        await loadWindows();
        startPolling();
      }
    } catch {
      /* community build: command missing → stays closed */
    }
  });

  onDestroy(() => {
    // Preserva la escena viva para el próximo montaje (el overlay no se cierra
    // al cambiar de vista, solo se desmonta este panel de control).
    savedPanels = panels;
    stopPolling();
    unlisten?.();
    window.removeEventListener("keydown", onKeydown);
    window.removeEventListener("pointermove", onPointerMove);
    window.removeEventListener("pointerup", onPointerUp);
    window.removeEventListener("pointermove", onCropMove);
    window.removeEventListener("pointerup", onCropUp);
  });
</script>

<div class="mx-auto max-w-5xl px-6 py-8">
  <div class="mb-6 flex items-center gap-3">
    <div
      class="grid h-12 w-12 place-items-center rounded-xl bg-emerald-500/10 ring-1 ring-emerald-500/30"
    >
      <MonitorPlay size={26} class="text-emerald-400" />
    </div>
    <div>
      <h1 class="text-xl font-semibold text-zinc-50">Hoard Screen</h1>
      <p class="text-sm text-zinc-400">
        {tr({
          es: "Una capa nativa sobre el juego: captura ventanas de otras apps y colócalas flotando encima. Edítalo desde aquí; el overlay refleja los cambios en vivo.",
          en: "A native layer over your game: capture other apps' windows and float them on top. Edit here; the overlay reflects changes live.",
        })}
      </p>
    </div>
  </div>

  {#if error}
    <div
      class="mb-4 rounded-lg border border-red-500/40 bg-red-500/10 px-3 py-2 text-xs text-red-200"
    >
      {error}
    </div>
  {/if}

  {#if !open}
    <button
      type="button"
      onclick={openOverlay}
      disabled={busy}
      class="flex items-center gap-2 rounded-lg bg-emerald-600 px-4 py-2.5 text-sm font-medium text-white hover:bg-emerald-500 disabled:opacity-60"
    >
      <SquarePlus size={18} />
      {tr({ es: "Abrir overlay", en: "Open overlay" })}
    </button>
  {:else}
    <div class="grid gap-6 md:grid-cols-[1fr_18rem]">
      <!-- preview -->
      <div bind:clientWidth={previewColWidth}>
        <div class="mb-2 flex flex-wrap items-center gap-2">
          <button
            type="button"
            onclick={closeOverlay}
            class="flex items-center gap-2 rounded-lg border border-zinc-700 px-3 py-1.5 text-sm text-zinc-300 hover:bg-zinc-800"
          >
            <SquareX size={15} />
            {tr({ es: "Ocultar overlay", en: "Hide overlay" })}
          </button>
          <div class="inline-flex rounded-md border border-zinc-700 p-0.5">
            <button
              type="button"
              onclick={() => setEditor(false)}
              class="flex items-center gap-1 rounded px-2.5 py-1 text-xs {!editing
                ? 'bg-emerald-600 text-white'
                : 'text-zinc-300'}"
              ><Gamepad2 size={14} /> {tr({ es: "Juego", en: "Game" })}</button
            >
            <button
              type="button"
              onclick={() => setEditor(true)}
              class="flex items-center gap-1 rounded px-2.5 py-1 text-xs {editing
                ? 'bg-emerald-600 text-white'
                : 'text-zinc-300'}"
              ><Pencil size={14} /> {tr({ es: "Editar", en: "Edit" })}</button
            >
          </div>
          <span class="text-xs text-zinc-500">{mon.w}×{mon.h}</span>
        </div>
        <p class="mb-2 flex items-start gap-1 text-[11px] text-zinc-500">
          <TriangleAlert size={12} class="mt-0.5 shrink-0 text-zinc-600" />
          <span
            >{tr({
              es: "El juego debe ir en ventana sin bordes (borderless), no en pantalla completa exclusiva, o no se verá nada encima.",
              en: "Run the game in borderless windowed mode, not exclusive fullscreen, or nothing will show on top.",
            })}</span
          >
        </p>
        {#if monitors.length > 1}
          <div class="mb-2 flex flex-wrap items-center gap-1">
            {#each monitors as m (m.id)}
              <button
                type="button"
                onclick={() => (activeMonId = m.id)}
                class="rounded-md border px-2.5 py-1 text-xs {activeMonId === m.id
                  ? 'border-emerald-500/60 bg-emerald-600/20 text-emerald-200'
                  : 'border-zinc-700 text-zinc-300 hover:bg-zinc-800'}"
                title="{m.w}×{m.h}">{monLabel(m)}</button
              >
            {/each}
            <span class="ml-1 text-[11px] text-zinc-500"
              >{tr({
                es: "Arrastra cada app en la pantalla elegida; usa el selector del panel para moverla a otra o ponerla en espejo.",
                en: "Arrange each app on the chosen screen; use the panel selector to move it to another or mirror it.",
              })}</span
            >
          </div>
        {/if}
        <p class="mb-2 text-[11px] text-zinc-500">
          {tr({
            es: "Modo Editar: cada app se vuelve una ventana normal que mueves y redimensionas por cualquier borde. Al volver a Juego, la captura queda donde la dejaste. Ctrl+O o Esc cambian de modo.",
            en: "Edit mode: each app becomes a normal window you move and resize from any edge. Back in Game, the capture stays where you left it. Ctrl+O or Esc switch modes.",
          })}
        </p>
        <div
          class="relative overflow-hidden rounded-lg border border-zinc-700 bg-zinc-900"
          style="width:{PREVIEW_W}px;height:{previewH}px"
          role="presentation"
          onpointerdown={() => (selectedId = null)}
        >
          {#each visiblePanels as p (p.id)}
            {@const isSel = selectedId === p.id}
            {@const inCrop = isSel && cropMode}
            <div
              role="button"
              tabindex="0"
              class="absolute select-none rounded-sm border text-[10px] {isSel
                ? 'border-emerald-400 bg-emerald-500/20'
                : 'border-zinc-500/70 bg-zinc-700/40'} {inCrop ? 'cursor-default' : 'cursor-move'}"
              style="left:{p.x * previewScale}px;top:{p.y *
                previewScale}px;width:{p.w * previewScale}px;height:{p.h *
                previewScale}px;z-index:{p.z}"
              onpointerdown={(e) => !inCrop && onPointerDown(e, p, "move")}
            >
              <span
                class="pointer-events-none absolute inset-x-0 top-0 truncate px-1 py-0.5 text-zinc-100"
                >{p.label}</span
              >

              {#if inCrop}
                <!-- Capa de recorte: lo que cae fuera se atenúa; los bordes del
                     área retenida se arrastran para recortar. -->
                <div
                  class="pointer-events-none absolute inset-x-0 top-0 bg-zinc-950/70"
                  style="height:{pct(p.crop.top)}"
                ></div>
                <div
                  class="pointer-events-none absolute inset-x-0 bottom-0 bg-zinc-950/70"
                  style="height:{pct(p.crop.bottom)}"
                ></div>
                <div
                  class="pointer-events-none absolute left-0 bg-zinc-950/70"
                  style="top:{pct(p.crop.top)};bottom:{pct(p.crop.bottom)};width:{pct(p.crop.left)}"
                ></div>
                <div
                  class="pointer-events-none absolute right-0 bg-zinc-950/70"
                  style="top:{pct(p.crop.top)};bottom:{pct(p.crop.bottom)};width:{pct(p.crop.right)}"
                ></div>
                <div
                  class="pointer-events-none absolute border border-dashed border-emerald-300"
                  style="left:{pct(p.crop.left)};right:{pct(p.crop.right)};top:{pct(
                    p.crop.top,
                  )};bottom:{pct(p.crop.bottom)}"
                ></div>
                <div
                  role="button"
                  tabindex="0"
                  class="absolute z-10 h-1.5 w-6 -translate-x-1/2 -translate-y-1/2 cursor-ns-resize rounded-full bg-emerald-400"
                  style="left:{pct((p.crop.left + 1 - p.crop.right) / 2)};top:{pct(p.crop.top)}"
                  onpointerdown={(e) => onCropDown(e, p, "top")}
                ></div>
                <div
                  role="button"
                  tabindex="0"
                  class="absolute z-10 h-1.5 w-6 -translate-x-1/2 translate-y-1/2 cursor-ns-resize rounded-full bg-emerald-400"
                  style="left:{pct((p.crop.left + 1 - p.crop.right) / 2)};bottom:{pct(p.crop.bottom)}"
                  onpointerdown={(e) => onCropDown(e, p, "bottom")}
                ></div>
                <div
                  role="button"
                  tabindex="0"
                  class="absolute z-10 h-6 w-1.5 -translate-x-1/2 -translate-y-1/2 cursor-ew-resize rounded-full bg-emerald-400"
                  style="top:{pct((p.crop.top + 1 - p.crop.bottom) / 2)};left:{pct(p.crop.left)}"
                  onpointerdown={(e) => onCropDown(e, p, "left")}
                ></div>
                <div
                  role="button"
                  tabindex="0"
                  class="absolute z-10 h-6 w-1.5 translate-x-1/2 -translate-y-1/2 cursor-ew-resize rounded-full bg-emerald-400"
                  style="top:{pct((p.crop.top + 1 - p.crop.bottom) / 2)};right:{pct(p.crop.right)}"
                  onpointerdown={(e) => onCropDown(e, p, "right")}
                ></div>
              {:else if isSel}
                <!-- tiradores de redimensión -->
                {#each HANDLES as hd (hd.h)}
                  <div
                    role="button"
                    tabindex="0"
                    class="absolute z-10 h-2 w-2 rounded-sm border border-emerald-200 bg-emerald-400 {hd.cls}"
                    onpointerdown={(e) => onPointerDown(e, p, hd.h)}
                  ></div>
                {/each}
              {/if}
            </div>
          {/each}
        </div>

        {#if selected}
          {@const s = selected}
          <div class="mt-3 space-y-3 rounded-lg border border-zinc-700 bg-zinc-800/40 p-3">
            <div class="flex items-center justify-between">
              <span class="truncate text-sm font-medium text-zinc-100">{s.label}</span>
              <div class="flex items-center gap-1">
                <button
                  type="button"
                  title={tr({ es: "Subir capa", en: "Raise layer" })}
                  onclick={() => bumpZ(s, 1)}
                  class="rounded p-1 text-zinc-300 hover:bg-zinc-700"><ArrowUp size={14} /></button
                >
                <button
                  type="button"
                  title={tr({ es: "Bajar capa", en: "Lower layer" })}
                  onclick={() => bumpZ(s, -1)}
                  class="rounded p-1 text-zinc-300 hover:bg-zinc-700"
                  ><ArrowDown size={14} /></button
                >
                <button
                  type="button"
                  onclick={() => removePanel(s.id)}
                  class="rounded p-1 text-red-300 hover:bg-red-500/20"><Trash2 size={14} /></button
                >
              </div>
            </div>
            <div class="grid grid-cols-4 gap-1.5">
              {#each [["x", "X"], ["y", "Y"], ["w", tr({ es: "An", en: "W" })], ["h", tr({ es: "Al", en: "H" })]] as [key, lbl]}
                <label class="flex flex-col gap-0.5 text-[10px] text-zinc-500">
                  <span>{lbl}</span>
                  <input
                    type="number"
                    value={s[key as "x" | "y" | "w" | "h"]}
                    onchange={(e) =>
                      setRect(s, key as "x" | "y" | "w" | "h", +e.currentTarget.value)}
                    class="w-full rounded border border-zinc-700 bg-zinc-900 px-1.5 py-1 text-xs text-zinc-200"
                  />
                </label>
              {/each}
            </div>
            {#if monitors.length > 1}
              <label class="flex items-center gap-2 text-xs text-zinc-400">
                <span>{tr({ es: "Pantalla", en: "Screen" })}</span>
                <select
                  value={s.mirror ? "all" : String(s.monitorId)}
                  onchange={(e) => setPanelTarget(s, e.currentTarget.value)}
                  class="flex-1 rounded border border-zinc-700 bg-zinc-900 px-2 py-1 text-zinc-200"
                >
                  {#each monitors as m (m.id)}
                    <option value={String(m.id)}>{monLabel(m)} ({m.w}×{m.h})</option>
                  {/each}
                  <option value="all">{tr({ es: "Espejo (todas)", en: "Mirror (all)" })}</option>
                </select>
              </label>
            {/if}
            <div class="inline-flex rounded-md border border-zinc-700 p-0.5">
              <button
                type="button"
                onclick={() => {
                  s.scale = "fit";
                  pushScene();
                }}
                class="flex items-center gap-1 rounded px-2 py-1 text-xs {s.scale === 'fit'
                  ? 'bg-emerald-600 text-white'
                  : 'text-zinc-300'}"><Proportions size={13} /> {tr({ es: "Ajustar", en: "Fit" })}</button
              >
              <button
                type="button"
                onclick={() => {
                  s.scale = "fill";
                  pushScene();
                }}
                class="flex items-center gap-1 rounded px-2 py-1 text-xs {s.scale === 'fill'
                  ? 'bg-emerald-600 text-white'
                  : 'text-zinc-300'}"><Maximize size={13} /> {tr({ es: "Llenar", en: "Fill" })}</button
              >
            </div>
            <div class="flex items-center justify-between">
              <button
                type="button"
                onclick={toggleCrop}
                class="flex items-center gap-1 rounded px-2 py-1 text-xs {cropMode
                  ? 'bg-emerald-600 text-white'
                  : 'border border-zinc-700 text-zinc-300 hover:bg-zinc-700'}"
                ><Crop size={13} />
                {tr({ es: "Recortar", en: "Crop" })}</button
              >
              <button
                type="button"
                title={tr({ es: "Quitar recorte", en: "Reset crop" })}
                onclick={() => resetCrop(s)}
                class="rounded p-1 text-zinc-400 hover:bg-zinc-700 hover:text-white"
                ><RotateCcw size={13} /></button
              >
            </div>
            <div class="grid grid-cols-2 gap-2 text-xs text-zinc-400">
              {#each [["top", "↑"], ["bottom", "↓"], ["left", "←"], ["right", "→"]] as [edge, sym]}
                <label class="flex items-center gap-2">
                  <span class="w-3">{sym}</span>
                  <input
                    type="range"
                    min="0"
                    max="0.9"
                    step="0.01"
                    value={s.crop[edge as keyof Panel["crop"]]}
                    oninput={(e) =>
                      setCrop(s, edge as keyof Panel["crop"], +e.currentTarget.value)}
                    class="flex-1"
                  />
                  <span class="w-8 text-right tabular-nums text-zinc-500"
                    >{pct(s.crop[edge as keyof Panel["crop"]])}</span
                  >
                </label>
              {/each}
            </div>
            <p class="text-[11px] text-zinc-500">
              {cropMode
                ? tr({
                    es: "Arrastra los bordes en la vista previa; lo atenuado se recorta.",
                    en: "Drag the edges in the preview; the dimmed area is cropped.",
                  })
                : tr({ es: "Recorte (fracción del origen)", en: "Crop (fraction of source)" })}
            </p>
            <div class="mt-1 border-t border-zinc-800 pt-2">
              <button
                type="button"
                onclick={() => {
                  s.passthrough = !s.passthrough;
                  pushScene();
                }}
                class="flex w-full items-center justify-between gap-1 rounded px-2 py-1 text-xs {s.passthrough
                  ? 'text-zinc-300'
                  : 'bg-amber-600/20 text-amber-300'}"
              >
                <span class="flex items-center gap-1"
                  ><MousePointerClick size={13} />
                  {tr({ es: "Clics pasan al juego", en: "Clicks go to game" })}</span
                >
                <span class="tabular-nums">{s.passthrough ? "ON" : "OFF"}</span>
              </button>
              <p class="px-2 text-[11px] text-zinc-500">
                {tr({
                  es: "Desactívalo solo si este vídeo sale en negro (Prime/Netflix). Entonces los clics sobre este panel irán a la app, no al juego.",
                  en: "Turn off only if this video shows black (Prime/Netflix). Then clicks on this panel go to the app, not the game.",
                })}
              </p>
              {#if s.passthrough}
                <div class="mt-1 flex items-center gap-2 px-2">
                  <span class="text-xs text-zinc-300">
                    {tr({ es: "Lente click-through", en: "Click-through lens" })}
                  </span>
                  <input
                    type="range"
                    min="20"
                    max="400"
                    step="5"
                    bind:value={s.passthroughRadius}
                    oninput={() => pushScene()}
                    class="flex-1 accent-zinc-400"
                  />
                  <span class="w-10 text-right text-xs tabular-nums text-zinc-400"
                    >{s.passthroughRadius}px</span
                  >
                </div>
                <p class="px-2 text-[11px] text-zinc-500">
                  {tr({
                    es: "Círculo que sigue al cursor: dentro ves y clicas lo que hay detrás del panel.",
                    en: "Circle that follows the cursor: inside it you see and click whatever is behind the panel.",
                  })}
                </p>
              {/if}
            </div>
            <div class="mt-1 border-t border-zinc-800 pt-2">
              <button
                type="button"
                onclick={() => {
                  s.compat = !s.compat;
                  pushScene();
                }}
                class="flex w-full items-center justify-between gap-1 rounded px-2 py-1 text-xs {s.compat
                  ? 'bg-emerald-600/20 text-emerald-300'
                  : 'text-zinc-300'}"
              >
                <span class="flex items-center gap-1"
                  ><Crop size={13} />
                  {tr({ es: "Modo compatibilidad Chromium", en: "Chromium compatibility mode" })}</span
                >
                <span class="tabular-nums">{s.compat ? "ON" : "OFF"}</span>
              </button>
              <p class="px-2 text-[11px] text-zinc-500">
                {tr({
                  es: "Recorte limpio en Brave/Discord (Chromium) y permite encoger el panel por debajo del mínimo de la ventana. Para clicar el panel (pausar, etc.), apaga «Clics pasan al juego» en él: los clics se reenvían a la ventana. Es posible que este modo cause que la ventana se quede en negro al reproducir.",
                  en: "Clean crop on Brave/Discord (Chromium), and lets the panel shrink below the window's minimum size. To click the panel (pause, etc.), turn off \"Clicks pass to game\" on it: clicks are forwarded to the window. This mode may make the window go black while playing video.",
                })}
              </p>
            </div>
          </div>
        {/if}
      </div>

      <!-- window picker -->
      <div>
        <div class="mb-2 flex items-center justify-between">
          <span class="text-sm font-medium text-zinc-200"
            >{tr({ es: "Apps", en: "Apps" })}</span
          >
          <button
            type="button"
            onclick={loadWindows}
            class="rounded p-1 text-zinc-400 hover:bg-zinc-700 hover:text-white"
            title={tr({ es: "Actualizar", en: "Refresh" })}><RefreshCw size={15} /></button
          >
        </div>
        <div class="space-y-1">
          {#each windows as w (w.id)}
            <button
              type="button"
              onclick={() => addPanel(w)}
              class="flex w-full items-center gap-2 rounded-md border border-zinc-700 bg-zinc-800/40 px-2 py-1.5 text-left text-xs text-zinc-200 hover:border-emerald-500/50 hover:bg-zinc-800"
            >
              <SquarePlus size={14} class="shrink-0 text-emerald-400" />
              <span class="min-w-0 flex-1 truncate"
                >{#if w.app}<span class="font-medium text-zinc-100">{w.app}</span
                  >{#if w.title}<span class="text-zinc-400"> — {w.title}</span>{/if}{:else}{w.title ||
                    w.id}{/if}</span
              >
              {#if w.protected}
                <TriangleAlert
                  size={13}
                  class="shrink-0 text-amber-400"
                  aria-label={tr({ es: "Contenido protegido", en: "Protected content" })}
                />
              {/if}
            </button>
          {:else}
            <p class="text-xs text-zinc-500">
              {tr({
                es: "No hay ventanas. Abre una app y pulsa actualizar.",
                en: "No windows. Open an app and refresh.",
              })}
            </p>
          {/each}
        </div>
      </div>
    </div>
  {/if}
</div>
