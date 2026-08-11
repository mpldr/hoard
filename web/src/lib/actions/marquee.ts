// Continuous ticker for the fact strip.
//
// The CSS-animation version had two problems this one doesn't: the site's
// global prefers-reduced-motion rule clamped it to a standstill, and a
// keyframe loop always lands back on its first frame, so the wrap read as a
// restart. Here the offset wraps modulo one copy of the track, which is
// exactly where the duplicate copy sits, so the seam never shows.
//
// The node is expected to hold the same list twice.
export type MarqueeOptions = {
  /** Scroll speed in CSS pixels per second. */
  speed?: number;
};

export function marquee(node: HTMLElement, opts: MarqueeOptions = {}) {
  let speed = opts.speed ?? 52;

  if (typeof window === 'undefined') return {};

  const container = node.parentElement ?? node;
  let offset = 0;
  let copy = 0; // width of one of the two copies
  let last = 0;
  let frame = 0;
  let hovering = false;
  let dragging = false;
  let dragPointer = -1;
  let dragStartX = 0;
  let dragStartOffset = 0;

  const measure = () => {
    copy = node.scrollWidth / 2;
  };

  const wrap = () => {
    if (copy <= 0) return;
    // Keep the offset in (-copy, 0]; both ends render identically.
    offset = ((offset % copy) + copy) % copy;
    if (offset > 0) offset -= copy;
  };

  const paint = () => {
    node.style.transform = `translate3d(${offset}px, 0, 0)`;
  };

  const tick = (now: number) => {
    frame = requestAnimationFrame(tick);
    const dt = last ? Math.min((now - last) / 1000, 0.1) : 0;
    last = now;
    if (hovering || dragging || copy <= 0) return;
    offset -= speed * dt;
    wrap();
    paint();
  };

  const onPointerDown = (e: PointerEvent) => {
    if (e.button !== 0) return;
    dragging = true;
    dragPointer = e.pointerId;
    dragStartX = e.clientX;
    dragStartOffset = offset;
    container.setPointerCapture(e.pointerId);
  };

  const onPointerMove = (e: PointerEvent) => {
    if (!dragging || e.pointerId !== dragPointer) return;
    e.preventDefault();
    offset = dragStartOffset + (e.clientX - dragStartX);
    wrap();
    paint();
  };

  const endDrag = (e: PointerEvent) => {
    if (!dragging || e.pointerId !== dragPointer) return;
    dragging = false;
    dragPointer = -1;
    if (container.hasPointerCapture(e.pointerId)) container.releasePointerCapture(e.pointerId);
  };

  const onEnter = () => (hovering = true);
  const onLeave = () => (hovering = false);

  measure();
  paint();
  frame = requestAnimationFrame(tick);

  const ro = new ResizeObserver(() => {
    // Fonts landing or the window resizing change the track width; re-measure
    // so the wrap point stays exactly one copy.
    measure();
    wrap();
    paint();
  });
  ro.observe(node);

  container.addEventListener('pointerenter', onEnter);
  container.addEventListener('pointerleave', onLeave);
  container.addEventListener('focusin', onEnter);
  container.addEventListener('focusout', onLeave);
  container.addEventListener('pointerdown', onPointerDown);
  container.addEventListener('pointermove', onPointerMove);
  container.addEventListener('pointerup', endDrag);
  container.addEventListener('pointercancel', endDrag);

  return {
    update(next: MarqueeOptions) {
      speed = next.speed ?? 52;
    },
    destroy() {
      cancelAnimationFrame(frame);
      ro.disconnect();
      container.removeEventListener('pointerenter', onEnter);
      container.removeEventListener('pointerleave', onLeave);
      container.removeEventListener('focusin', onEnter);
      container.removeEventListener('focusout', onLeave);
      container.removeEventListener('pointerdown', onPointerDown);
      container.removeEventListener('pointermove', onPointerMove);
      container.removeEventListener('pointerup', endDrag);
      container.removeEventListener('pointercancel', endDrag);
    }
  };
}
