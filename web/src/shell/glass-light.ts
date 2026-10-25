/**
 * The light that moves across the glass rims. It follows the pointer (or drifts
 * slowly on touch screens), and each glass element reads it in its own
 * coordinates through --lx / --ly.
 */
const clamp = (v: number, a: number, b: number) => Math.max(a, Math.min(b, v));

let px = 0;
let py = -80;
let queued = false;
let started = false;

function update() {
  queued = false;
  for (const g of document.querySelectorAll<HTMLElement>('.glass')) {
    const r = g.getBoundingClientRect();
    if (!r.width || r.bottom < 0 || r.top > window.innerHeight) continue;
    g.style.setProperty('--lx', `${clamp(((px - r.left) / r.width) * 100, -40, 140).toFixed(1)}%`);
    g.style.setProperty('--ly', `${clamp(((py - r.top) / r.height) * 100, -120, 220).toFixed(1)}%`);
  }
}

/** Ask for a refresh after a glass shape moved or changed size. */
export function refreshGlass(): void {
  if (queued || typeof window === 'undefined') return;
  queued = true;
  requestAnimationFrame(update);
}

export function startGlassLight(): () => void {
  if (started) return () => undefined;
  started = true;
  px = window.innerWidth * 0.3;
  const onMove = (e: PointerEvent) => {
    if (e.pointerType === 'touch') return;
    px = e.clientX;
    py = e.clientY;
    refreshGlass();
  };
  window.addEventListener('pointermove', onMove, { passive: true });
  window.addEventListener('resize', refreshGlass);
  // Shapes morph over ~0.7 s; a slow heartbeat keeps rims right without watching every mutation.
  const beat = setInterval(refreshGlass, 400);
  let t = 0;
  const reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
  const drift = reduced
    ? null
    : setInterval(() => {
        if (!matchMedia('(hover: none)').matches) return;
        t += 0.05;
        px = window.innerWidth * (0.5 + 0.35 * Math.sin(t));
        py = -60 + 40 * Math.cos(t * 0.7);
        refreshGlass();
      }, 120);
  refreshGlass();
  return () => {
    started = false;
    window.removeEventListener('pointermove', onMove);
    window.removeEventListener('resize', refreshGlass);
    clearInterval(beat);
    if (drift) clearInterval(drift);
  };
}
