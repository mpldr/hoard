<script lang="ts">
  /**
   * Eye dropdown — live overview of every device in the account.
   *
   * Each device shows: OS logo (emoji) + name + online dot + running game with
   * elapsed time. This machine is always first; other devices from the same
   * account appear below when the server exposes a live device list (see
   * vista.md for the backend spec).
   *
   * Super clean: one row per device, no noise.
   */
  import { _ } from "svelte-i18n";
  import { activity, status } from "../stores/agent";
  import { auth } from "../stores/auth";
  import { cloud } from "../stores/cloud";
  import { Gamepad2, Server } from "@lucide/svelte";
  import OsLogo from "./OsLogo.svelte";

  let { now }: { now: number } = $props();

  // --- OS detection -------------------------------------------------------
  // Read from the <html> class set by App.svelte on boot. Returns the OS key
  // for OsLogo + a display name.
  type OsInfo = { key: "windows" | "linux" | "macos" | "unknown"; name: string };

  const thisOs = $derived.by<OsInfo>(() => {
    const html = document.documentElement;
    if (html.classList.contains("is-linux")) return { key: "linux", name: "Linux" };
    if (html.classList.contains("is-macos")) return { key: "macos", name: "macOS" };
    if (html.classList.contains("is-windows")) return { key: "windows", name: "Windows" };
    return { key: "unknown", name: "" };
  });

  // --- This machine's running games ---------------------------------------
  const runningGames = $derived(
    Object.entries($activity)
      .filter(([, a]) => a.state === "running")
      .map(([saveId, a]) => ({
        saveId,
        slug: saveId,
        since: a.running_since ?? now,
      })),
  );

  function prettySlug(slug: string): string {
    const parts = slug.split(/[-_]+/).filter(Boolean);
    if (parts.length === 0) return slug;
    if (parts.length > 1 && parts[parts.length - 1] === "main") parts.pop();
    return parts.map((w) => w.charAt(0).toUpperCase() + w.slice(1)).join(" ");
  }

  function fmtElapsed(since: number): string {
    const secs = Math.max(0, Math.floor((now - since) / 1000));
    if (secs < 60) return `${secs}s`;
    const m = Math.floor(secs / 60);
    if (m < 60) return `${m}m ${secs % 60}s`;
    const h = Math.floor(m / 60);
    return `${h}h ${m % 60}m`;
  }

  // --- Device model -------------------------------------------------------
  // This machine is always present. Other devices from the same account would
  // come from a server endpoint (see vista.md). The panel is structured so
  // they slot in below this machine — when the API lands, just populate the
  // `otherDevices` array.
  type Device = {
    name: string;
    os: OsInfo;
    online: boolean;
    playing?: string;
    playingSince?: number;
  };

  const thisDevice = $derived<Device>({
    name: $_("eye.this_machine"),
    os: thisOs,
    online: $status.running,
    playing: runningGames.length > 0 ? prettySlug(runningGames[0].slug) : undefined,
    playingSince: runningGames[0]?.since,
  });

  // Other devices: NOT available yet. The server doesn't expose a live device
  // list — see vista.md for the spec of what needs to be added backend-side.
  // When it lands, poll or listen for a Tauri event and populate this array.
  const otherDevices: Device[] = $derived([]);

  const allDevices = $derived([thisDevice, ...otherDevices]);
</script>

<div class="space-y-1 px-3 py-3">
  <!-- Device rows -->
  {#each allDevices as d, i (i)}
    <div class="flex items-center gap-2.5 px-1 py-1.5">
      <!-- Online dot (green pulse when online + playing, green solid when
           online idle, grey when offline) -->
      <span class="relative flex h-2.5 w-2.5 shrink-0">
        {#if d.online && d.playing}
          <span class="absolute inline-flex h-full w-full animate-ping rounded-full bg-sky-400/60"></span>
        {/if}
        <span class="relative inline-flex h-2.5 w-2.5 rounded-full {d.online
          ? d.playing ? 'bg-sky-400' : 'bg-emerald-400'
          : 'bg-zinc-600'}"></span>
      </span>
      <!-- OS logo — real SVG, tinted green when online, grey when offline -->
      <span class="shrink-0 {d.online ? 'text-emerald-400' : 'text-zinc-600'}">
        <OsLogo os={d.os.key} size={16} />
      </span>
      <!-- Device name -->
      <span class="flex-1 truncate text-xs font-medium text-zinc-200">
        {d.name}
        {#if d.os.name}<span class="text-zinc-500">· {d.os.name}</span>{/if}
      </span>
      <!-- Status: playing (sky) or online (green) or offline (grey) -->
      {#if d.playing}
        <span class="shrink-0 font-mono text-[11px] tabular-nums text-sky-400">
          {fmtElapsed(d.playingSince ?? now)}
        </span>
      {:else if d.online}
        <span class="shrink-0 text-[10px] font-medium uppercase tracking-wide text-emerald-400">
          {$_("eye.online")}
        </span>
      {:else}
        <span class="shrink-0 text-[10px] font-medium uppercase tracking-wide text-zinc-500">
          {$_("eye.offline")}
        </span>
      {/if}
    </div>
    <!-- Running game indented under the device -->
    {#if d.playing}
      <div class="flex items-center gap-2.5 pl-9 pr-1 py-0.5">
        <Gamepad2 size={12} class="shrink-0 text-sky-400" />
        <span class="flex-1 truncate text-[11px] text-zinc-400">{d.playing}</span>
      </div>
    {/if}
  {/each}

  <!-- Self-hosted server (if connected, separate section) -->
  {#if $auth.user?.server_url}
    <div class="mt-2 flex items-center gap-2.5 border-t border-white/[0.06] px-1 pt-2.5">
      <span class="relative flex h-2.5 w-2.5 shrink-0">
        <span class="relative inline-flex h-2.5 w-2.5 rounded-full bg-emerald-400"></span>
      </span>
      <Server size={14} class="shrink-0 text-zinc-400" />
      <span class="flex-1 truncate text-xs font-medium text-zinc-200">
        {$auth.user.server_url.replace(/^https?:\/\//, "").replace(/\/.*$/, "")}
      </span>
      <span class="shrink-0 text-[10px] font-medium uppercase tracking-wide text-emerald-400">
        {$_("eye.online")}
      </span>
    </div>
  {/if}

  <!-- If this machine is online but nothing is running, a quiet hint -->
  {#if runningGames.length === 0 && $status.running}
    <p class="px-1 py-1 text-[11px] text-zinc-600">
      {$_("eye.nothing_playing")}
    </p>
  {/if}

  <!-- Cloud account device count (informational — shows how many devices are
       linked, even though we can't list them individually yet). -->
  {#if $cloud.account && $cloud.account.devices_used > 1}
    <p class="mt-1 px-1 text-[10px] text-zinc-600">
      {$cloud.account.devices_used} {$_("eye.devices_linked")}
    </p>
  {/if}
</div>
