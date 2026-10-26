/** Small DOM, motion and geometry helpers shared by lenses. */

type Attrs = Record<string, string | number | boolean | null | undefined>;

/** `el('div', 'a b', { role: 'button' }, child, 'text')` */
export function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls?: string | null, attrs?: Attrs | null, ...kids: (Node | string | null | undefined)[]): HTMLElementTagNameMap[K] {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (attrs) {
    for (const [k, v] of Object.entries(attrs)) {
      if (v === null || v === undefined || v === false) continue;
      n.setAttribute(k, v === true ? '' : String(v));
    }
  }
  for (const k of kids) if (k !== null && k !== undefined) n.append(k);
  return n;
}

export const clamp = (v: number, a: number, b: number) => Math.max(a, Math.min(b, v));
export const lerp = (a: number, b: number, t: number) => a + (b - a) * t;

export function reducedMotion(): boolean {
  return typeof matchMedia !== 'undefined' && matchMedia('(prefers-reduced-motion: reduce)').matches;
}

/** The soft spring used for flights and re-layouts (WAAPI easing). */
export const SOFT = 'cubic-bezier(.25,.85,.3,1)';

/**
 * FLIP: measure before a DOM change, `play()` after it. Elements that are new
 * (or were off screen) fade in; others slide from where they were.
 */
export function flip(els: Iterable<Element>, { duration = 540, easing = SOFT, fadeIn = true } = {}) {
  const first = new Map<Element, DOMRect>();
  for (const e of els) first.set(e, e.getBoundingClientRect());
  return {
    play(after: Iterable<Element>) {
      if (reducedMotion()) return;
      const vh = window.innerHeight;
      for (const e of after) {
        const b = e.getBoundingClientRect();
        if (!b.width && !b.height) continue;
        const a = first.get(e);
        if (!a || (!a.width && !a.height)) {
          if (fadeIn && b.bottom > 0 && b.top < vh) e.animate([{ opacity: 0, transform: 'scale(.94)' }, { opacity: 1, transform: 'none' }], { duration: duration * 0.8, easing, delay: 60 });
          continue;
        }
        const dx = a.left - b.left;
        const dy = a.top - b.top;
        const sx = b.width > 1 ? a.width / b.width : 1;
        const sy = b.height > 1 ? a.height / b.height : 1;
        if (Math.abs(dx) < 0.5 && Math.abs(dy) < 0.5 && Math.abs(sx - 1) < 0.01 && Math.abs(sy - 1) < 0.01) continue;
        const off = (r: DOMRect) => r.bottom < -200 || r.top > vh + 200;
        if (off(a) && off(b)) continue;
        e.animate(
          [
            { transformOrigin: '0 0', transform: `translate(${dx}px, ${dy}px) scale(${sx}, ${sy})` },
            { transformOrigin: '0 0', transform: 'none' },
          ],
          { duration, easing },
        );
      }
    },
  };
}

export interface Candidate<T> {
  value: T;
  rect: DOMRect;
}

/** Spatial navigation: the nearest candidate in a direction, favouring alignment. */
export function nearestInDirection<T>(from: DOMRect, cands: Iterable<Candidate<T>>, dir: 'left' | 'right' | 'up' | 'down'): Candidate<T> | null {
  const cx = from.left + from.width / 2;
  const cy = from.top + from.height / 2;
  let best: Candidate<T> | null = null;
  let bestScore = Infinity;
  for (const c of cands) {
    const x = c.rect.left + c.rect.width / 2;
    const y = c.rect.top + c.rect.height / 2;
    const dx = x - cx;
    const dy = y - cy;
    let main: number;
    let cross: number;
    if (dir === 'left') {
      if (dx > -4) continue;
      main = -dx;
      cross = Math.abs(dy);
    } else if (dir === 'right') {
      if (dx < 4) continue;
      main = dx;
      cross = Math.abs(dy);
    } else if (dir === 'up') {
      if (dy > -4) continue;
      main = -dy;
      cross = Math.abs(dx);
    } else {
      if (dy < 4) continue;
      main = dy;
      cross = Math.abs(dx);
    }
    const score = main + cross * 2.2;
    if (score < bestScore) {
      bestScore = score;
      best = c;
    }
  }
  return best;
}

/** Stable pseudo-random number in [0, 1) for a string (FNV-1a). */
export function rand01(key: string): number {
  let h = 2166136261;
  for (let i = 0; i < key.length; i++) {
    h ^= key.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return ((h >>> 0) % 100000) / 100000;
}

/** Per-viewer conveniences (zoom level, last lens); never required. */
/** Whether a stored value has the fallback's shape (array, object, or the same primitive type). */
function sameShape(v: unknown, like: unknown): boolean {
  if (like === null || like === undefined) return true;
  if (Array.isArray(like)) return Array.isArray(v);
  if (typeof like === 'object') return typeof v === 'object' && v !== null && !Array.isArray(v);
  return typeof v === typeof like;
}

export const prefs = {
  get<T>(key: string, fallback: T): T {
    try {
      const v = localStorage.getItem(`flux.${key}`);
      if (v === null) return fallback;
      const parsed: unknown = JSON.parse(v);
      // A value of another shape (an older version, a hand edit) falls back instead of breaking the page.
      return sameShape(parsed, fallback) ? (parsed as T) : fallback;
    } catch {
      return fallback;
    }
  },
  set(key: string, value: unknown): void {
    try {
      localStorage.setItem(`flux.${key}`, JSON.stringify(value));
    } catch {
      // storage blocked: the choice lasts for this page only
    }
  },
};
