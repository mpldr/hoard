<script lang="ts">
  /**
   * Game cover thumbnail. Shows the Steam capsule (served from the on-device
   * cache) when one exists, otherwise a tinted square with the game's initial.
   * The image is loaded lazily via the `covers` store so the same app id is
   * fetched at most once per session.
   *
   * Users can override the cover with a custom local image. On hover a pencil
   * icon appears; clicking it opens a file picker. If a custom cover is set,
   * the pencil changes to a "restore" icon to revert to the Steam capsule.
   */
  import { Pencil, RotateCcw } from "lucide-svelte";
  import { open as openDialog } from "@tauri-apps/plugin-dialog";
  import {
    coverUrl,
    steamIdForSlug,
    hasCustomCover,
    setCustomCover,
    removeCustomCover,
  } from "../stores/covers";
  import { tilt } from "../actions/tilt";

  let {
    appId = null,
    /** Game slug, used to recover the Steam app id from the catalog when
     *  `appId` is null (e.g. a save tracked on another device). */
    slug = null,
    name = "",
    /** Tailwind size + radius classes for the outer box. */
    class: klass = "h-10 w-10 rounded-lg",
    /** Font-size class for the fallback initial. */
    initialClass = "text-sm",
  }: {
    appId?: number | null;
    slug?: string | null;
    name?: string;
    class?: string;
    initialClass?: string;
  } = $props();

  const initial = $derived((name.trim().charAt(0) || "?").toUpperCase());
  let url = $state<string | null>(null);
  let hovered = $state(false);
  let isCustom = $state(false);
  let resolvedAppId = $state<number | null>(null);

  $effect(() => {
    url = null;
    isCustom = false;
    const directId = appId;
    const s = slug;
    let alive = true;
    (async () => {
      // Prefer the id detection already resolved; otherwise recover it from the
      // catalog by slug so cross-device saves still get a cover.
      const id = directId ?? (s ? await steamIdForSlug(s) : null);
      if (id == null || !alive) return;
      resolvedAppId = id;
      const [u, custom] = await Promise.all([
        coverUrl(id),
        hasCustomCover(id),
      ]);
      if (alive) {
        url = u;
        isCustom = custom;
      }
    })();
    return () => {
      alive = false;
    };
  });

  async function pickCover(e: MouseEvent) {
    e.stopPropagation();
    if (resolvedAppId == null) return;
    try {
      const file = await openDialog({
        multiple: false,
        filters: [
          {
            name: "Images",
            extensions: ["jpg", "jpeg", "png", "webp", "gif", "bmp"],
          },
        ],
      });
      if (typeof file === "string" && file.length > 0) {
        await setCustomCover(resolvedAppId, file);
        // Reload the cover.
        url = null;
        const u = await coverUrl(resolvedAppId);
        url = u;
        isCustom = true;
      }
    } catch {
      // User cancelled or file read error — ignore silently.
    }
  }

  async function restoreOriginal(e: MouseEvent) {
    e.stopPropagation();
    if (resolvedAppId == null) return;
    await removeCustomCover(resolvedAppId);
    // Reload the cover.
    url = null;
    const u = await coverUrl(resolvedAppId);
    url = u;
    isCustom = false;
  }
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div
  class={`tilt group relative shrink-0 overflow-hidden border border-white/[0.08] bg-zinc-800 ${klass}`}
  use:tilt
  onmouseenter={() => (hovered = true)}
  onmouseleave={() => (hovered = false)}
>
  {#if url}
    <img src={url} alt={name} class="h-full w-full object-cover" draggable="false" />
  {:else}
    <div
      class={`flex h-full w-full items-center justify-center bg-gradient-to-br from-emerald-600/40 to-emerald-900/40 font-semibold text-emerald-100 ${initialClass}`}
    >
      {initial}
    </div>
  {/if}

  {#if resolvedAppId != null && hovered}
    {#if isCustom}
      <button
        type="button"
        onclick={restoreOriginal}
        title="Restaurar imagen original"
        class="absolute inset-0 flex items-center justify-center bg-black/50 text-white transition-opacity"
      >
        <RotateCcw class="h-3.5 w-3.5" />
      </button>
    {:else}
      <button
        type="button"
        onclick={pickCover}
        title="Cambiar imagen"
        class="absolute inset-0 flex items-center justify-center bg-black/50 text-white transition-opacity"
      >
        <Pencil class="h-3.5 w-3.5" />
      </button>
    {/if}
  {/if}
</div>
