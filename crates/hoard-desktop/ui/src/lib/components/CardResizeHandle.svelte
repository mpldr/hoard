<script lang="ts">
  /**
   * Drag-to-resize handle for Library grid cards.
   *
   * Place inside a card element. On mousedown the user can drag to change the
   * minimum column width for the entire section. The parent grid must use
   * `grid-template-columns: repeat(auto-fill, minmax(var(--card-w), 1fr))`
   * and read from the `cardSizes` store.
   *
   * Usage:
   *   <CardResizeHandle section="tracked" />
   */
  import type { SectionKey } from "../stores/cardSizes.svelte";
  import { setCardWidth, cardWidth } from "../stores/cardSizes.svelte";

  let { section }: { section: SectionKey } = $props();

  let dragging = $state(false);
  let startX = $state(0);
  let startW = $state(0);

  function onPointerDown(e: PointerEvent) {
    e.preventDefault();
    e.stopPropagation();
    dragging = true;
    startX = e.clientX;
    startW = cardWidth(section);
    document.body.style.cursor = "nwse-resize";
    document.body.style.userSelect = "none";
    window.addEventListener("pointermove", onPointerMove);
    window.addEventListener("pointerup", onPointerUp);
  }

  function onPointerMove(e: PointerEvent) {
    if (!dragging) return;
    const dx = e.clientX - startX;
    // Each px of drag = 1px of card width change.
    setCardWidth(section, startW + dx);
  }

  function onPointerUp() {
    dragging = false;
    document.body.style.cursor = "";
    document.body.style.userSelect = "";
    window.removeEventListener("pointermove", onPointerMove);
    window.removeEventListener("pointerup", onPointerUp);
  }
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<span
  class="absolute bottom-0 right-0 z-10 h-4 w-4 cursor-nwse-resize opacity-0 transition-opacity group-hover:opacity-100"
  onpointerdown={onPointerDown}
>
  <!-- Three diagonal lines indicating resize -->
  <svg
    viewBox="0 0 16 16"
    class="h-full w-full text-zinc-500 group-hover:text-zinc-400"
    fill="none"
    stroke="currentColor"
    stroke-width="1.5"
  >
    <line x1="11" y1="5" x2="5" y2="11" />
    <line x1="14" y1="8" x2="8" y2="14" />
    <line x1="14" y1="11" x2="11" y2="14" />
  </svg>
</span>
