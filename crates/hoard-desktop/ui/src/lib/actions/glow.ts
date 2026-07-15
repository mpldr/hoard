/**
 * Glow action — a cursor-following highlight without the 3D tilt. Used by
 * buttons and small interactive elements where a perspective rotation would
 * feel wrong, but the light that follows the pointer is still a nice touch.
 *
 * Sets the same --tilt-glow-x/--tilt-glow-y CSS variables as `tilt`; the
 * `.glow` class in app.css consumes them. Cheaper than tilt (no transform,
 * no perspective) so it's safe to attach to every button.
 */
export function glow(node: HTMLElement) {
  let raf = 0;

  function onMove(e: MouseEvent) {
    const r = node.getBoundingClientRect();
    const px = (e.clientX - r.left) / r.width;
    const py = (e.clientY - r.top) / r.height;
    cancelAnimationFrame(raf);
    raf = requestAnimationFrame(() => {
      node.style.setProperty("--tilt-glow-x", `${(px * 100).toFixed(1)}%`);
      node.style.setProperty("--tilt-glow-y", `${(py * 100).toFixed(1)}%`);
    });
  }

  node.addEventListener("mousemove", onMove);
  return {
    destroy() {
      node.removeEventListener("mousemove", onMove);
      cancelAnimationFrame(raf);
    },
  };
}
