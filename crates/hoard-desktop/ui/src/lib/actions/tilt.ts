/**
 * 3D tilt action — the element leans toward the cursor on hover and a soft
 * directional glow follows the pointer. Applied to cards, panels, and covers
 * so the UI feels physical rather than flat.
 *
 * The transform + glow are driven by CSS variables this action sets
 * (--tilt-rx/--tilt-ry and --tilt-glow-x/--tilt-glow-y); the `.tilt` class in
 * app.css consumes them, so the action itself stays transform-free (cheap to
 * attach/teardown). The CSS's global reduced-motion rule flattens transitions
 * automatically when the OS asks for it — we don't skip the listeners here
 * because the glow (opacity-only) is still useful even without the 3D tilt.
 *
 * Anti-flicker: the CSS transform skews the bounding box so `mouseleave`
 * fires when the cursor is still visually on the card. We listen for
 * `pointermove` on the document and only reset when the pointer truly
 * exits the element's original (un-transformed) rect.
 */
export function tilt(node: HTMLElement, opts: { max?: number } = {}) {
  const max = opts.max ?? 8;

  let raf = 0;
  let active = false;

  /** Original (un-transformed) bounding rect, captured on enter. */
  let rect: DOMRect | null = null;

  function onMove(e: PointerEvent) {
    if (!active || !rect) return;
    // Use the original rect captured at enter so the transform doesn't
    // shift the "inside" boundary while we're computing tilt values.
    const px = (e.clientX - rect.left) / rect.width; // 0..1
    const py = (e.clientY - rect.top) / rect.height;
    cancelAnimationFrame(raf);
    raf = requestAnimationFrame(() => {
      node.style.setProperty("--tilt-ry", `${(px - 0.5) * 2 * max}deg`);
      node.style.setProperty("--tilt-rx", `${(0.5 - py) * 2 * max}deg`);
      node.style.setProperty("--tilt-glow-x", `${(px * 100).toFixed(1)}%`);
      node.style.setProperty("--tilt-glow-y", `${(py * 100).toFixed(1)}%`);
    });
  }

  function reset() {
    active = false;
    rect = null;
    cancelAnimationFrame(raf);
    node.style.setProperty("--tilt-ry", "0deg");
    node.style.setProperty("--tilt-rx", "0deg");
  }

  function onEnter(e: PointerEvent) {
    // Capture the rect BEFORE the transform kicks in by reading it on the
    // very first enter. Subsequent enters after a reset re-capture it.
    rect = node.getBoundingClientRect();
    active = true;
    onMove(e);
  }

  function onDocMove(e: PointerEvent) {
    if (!active || !rect) return;
    // Check against the ORIGINAL rect — ignore the transformed bbox.
    const inBounds =
      e.clientX >= rect.left &&
      e.clientX <= rect.right &&
      e.clientY >= rect.top &&
      e.clientY <= rect.bottom;
    if (inBounds) {
      onMove(e);
    } else {
      reset();
    }
  }

  function onDocUp() {
    // After a click, re-capture the rect on next move so stale bounds
    // from before a layout shift don't linger.
    if (active) rect = node.getBoundingClientRect();
  }

  node.addEventListener("pointerenter", onEnter);
  document.addEventListener("pointermove", onDocMove);
  document.addEventListener("pointerup", onDocUp);
  return {
    destroy() {
      node.removeEventListener("pointerenter", onEnter);
      document.removeEventListener("pointermove", onDocMove);
      document.removeEventListener("pointerup", onDocUp);
      cancelAnimationFrame(raf);
    },
  };
}
