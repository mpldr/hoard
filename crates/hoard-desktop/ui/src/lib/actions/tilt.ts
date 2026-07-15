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
 */
export function tilt(node: HTMLElement, opts: { max?: number } = {}) {
  const max = opts.max ?? 8;

  let raf = 0;

  function onMove(e: MouseEvent) {
    const r = node.getBoundingClientRect();
    const px = (e.clientX - r.left) / r.width; // 0..1
    const py = (e.clientY - r.top) / r.height;
    cancelAnimationFrame(raf);
    raf = requestAnimationFrame(() => {
      node.style.setProperty("--tilt-ry", `${(px - 0.5) * 2 * max}deg`);
      node.style.setProperty("--tilt-rx", `${(0.5 - py) * 2 * max}deg`);
      node.style.setProperty("--tilt-glow-x", `${(px * 100).toFixed(1)}%`);
      node.style.setProperty("--tilt-glow-y", `${(py * 100).toFixed(1)}%`);
    });
  }

  function reset() {
    cancelAnimationFrame(raf);
    node.style.setProperty("--tilt-ry", "0deg");
    node.style.setProperty("--tilt-rx", "0deg");
  }

  node.addEventListener("mousemove", onMove);
  node.addEventListener("mouseleave", reset);
  return {
    destroy() {
      node.removeEventListener("mousemove", onMove);
      node.removeEventListener("mouseleave", reset);
      cancelAnimationFrame(raf);
    },
  };
}
