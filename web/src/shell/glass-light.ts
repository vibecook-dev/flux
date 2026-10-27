/**
 * The light that moves across the glass rims. It follows the pointer (or drifts
 * slowly on touch screens), and each glass element reads it in its own
 * coordinates through --lx / --ly.
 *
 * It costs nothing at rest: a refresh is asked for when the pointer moves, the
 * page scrolls or resizes, or a glass shape settles after a morph (the shell
 * calls refreshGlass, and every transition or animation end asks too). A refresh
 * measures all the glass first and then writes only the values that changed, so
 * it never forces a layout per element.
 */
const clamp = (v: number, a: number, b: number) => Math.max(a, Math.min(b, v));

let px = 0;
let py = -80;
let queued = false;
let started = false;
/** What each element was last given, so an unchanged light writes (and restyles) nothing. */
const written = new WeakMap<HTMLElement, string>();
/** Glass that changes size without a transition (content, a morph cut short) asks for a refresh too. */
let sized: ResizeObserver | null = null;
let watched = new WeakSet<HTMLElement>();

function update() {
  queued = false;
  const glass = document.querySelectorAll<HTMLElement>('.glass');
  const rects = Array.from(glass, (g) => g.getBoundingClientRect());
  const h = window.innerHeight;
  glass.forEach((g, i) => {
    if (sized && !watched.has(g)) {
      watched.add(g);
      sized.observe(g);
    }
    const r = rects[i];
    if (!r.width || r.bottom < 0 || r.top > h) return;
    const lx = clamp(((px - r.left) / r.width) * 100, -40, 140).toFixed(1);
    const ly = clamp(((py - r.top) / r.height) * 100, -120, 220).toFixed(1);
    const key = `${lx} ${ly}`;
    if (written.get(g) === key) return;
    written.set(g, key);
    g.style.setProperty('--lx', `${lx}%`);
    g.style.setProperty('--ly', `${ly}%`);
  });
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
  sized = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(refreshGlass);
  watched = new WeakSet();
  const passive = { passive: true, capture: true };
  window.addEventListener('pointermove', onMove, { passive: true });
  window.addEventListener('resize', refreshGlass);
  // Glass that scrolls, and shapes that settle after a morph or an entrance.
  document.addEventListener('scroll', refreshGlass, passive);
  document.addEventListener('transitionend', refreshGlass, passive);
  document.addEventListener('animationend', refreshGlass, passive);

  // Without a pointer the light drifts on its own, slowly (not with reduced motion).
  const touch = matchMedia('(hover: none)');
  const reduced = matchMedia('(prefers-reduced-motion: reduce)');
  let drift: ReturnType<typeof setInterval> | null = null;
  let t = 0;
  const syncDrift = () => {
    const want = touch.matches && !reduced.matches;
    if (want && !drift) {
      drift = setInterval(() => {
        if (document.hidden) return;
        t += 0.05;
        px = window.innerWidth * (0.5 + 0.35 * Math.sin(t));
        py = -60 + 40 * Math.cos(t * 0.7);
        refreshGlass();
      }, 120);
    } else if (!want && drift) {
      clearInterval(drift);
      drift = null;
    }
  };
  syncDrift();
  touch.addEventListener('change', syncDrift);
  reduced.addEventListener('change', syncDrift);
  refreshGlass();
  return () => {
    started = false;
    window.removeEventListener('pointermove', onMove);
    window.removeEventListener('resize', refreshGlass);
    document.removeEventListener('scroll', refreshGlass, passive);
    document.removeEventListener('transitionend', refreshGlass, passive);
    document.removeEventListener('animationend', refreshGlass, passive);
    touch.removeEventListener('change', syncDrift);
    reduced.removeEventListener('change', syncDrift);
    if (drift) clearInterval(drift);
    sized?.disconnect();
    sized = null;
  };
}
