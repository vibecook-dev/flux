/**
 * Motion helpers shared by every section: reduced-motion, headline words that
 * arrive one by one, reveals on entering the view, and one scroll loop that
 * runs scenes (scroll-linked transforms) only while something moves.
 */

const reduceQuery = matchMedia('(prefers-reduced-motion: reduce)');
export const reducedMotion = (): boolean => reduceQuery.matches;

/** Split the text of [data-split] elements into words that rise in turn. Words keep counting across one heading. */
export function splitHeadings(root: ParentNode = document): void {
  const done = new Set<Element>();
  for (const el of root.querySelectorAll<HTMLElement>('[data-split]')) {
    const heading = el.closest('h1, h2, h3') ?? el;
    if (done.has(heading)) continue;
    done.add(heading);
    let i = 0;
    for (const part of heading.querySelectorAll<HTMLElement>('[data-split]')) {
      const text = part.textContent ?? '';
      part.dataset.text = text;
      part.textContent = '';
      const words = text.split(/(\s+)/);
      for (const w of words) {
        if (!w) continue;
        if (/^\s+$/.test(w)) {
          part.append(document.createTextNode(' '));
          continue;
        }
        const span = document.createElement('span');
        span.className = 'w';
        span.textContent = w;
        span.style.setProperty('--wd', `${(Number(part.dataset.delay ?? 0) + i * 0.055).toFixed(3)}s`);
        part.append(span);
        i++;
      }
      part.setAttribute('data-split-done', '');
    }
  }
}

/**
 * Once a split headline has arrived, put its text back together: the words no
 * longer need boxes of their own, and a gradient then runs across the whole
 * phrase instead of starting again in every word.
 */
function settle(part: HTMLElement) {
  const words = part.querySelectorAll<HTMLElement>('.w');
  const last = words[words.length - 1];
  let settled = false;
  const done = () => {
    if (settled) return;
    settled = true;
    if (part.dataset.text !== undefined) part.textContent = part.dataset.text;
  };
  last?.addEventListener('transitionend', (e) => e.propertyName === 'transform' && done());
  setTimeout(done, 3000);
}

const reveal = (el: Element) => {
  el.classList.add('in');
  el.dispatchEvent(new CustomEvent('reveal'));
  if (el instanceof HTMLElement && el.hasAttribute('data-split')) settle(el);
};

/** Add `.in` to [data-reveal] and [data-split] elements when they come into view. */
export function observeReveals(root: ParentNode = document): void {
  const els = root.querySelectorAll<HTMLElement>('[data-reveal], [data-split]');
  if (!('IntersectionObserver' in window)) {
    els.forEach(reveal);
    return;
  }
  const io = new IntersectionObserver(
    (entries) => {
      for (const e of entries) {
        if (!e.isIntersecting) continue;
        reveal(e.target);
        io.unobserve(e.target);
      }
    },
    { rootMargin: '0px 0px -9% 0px', threshold: 0.12 },
  );
  els.forEach((el) => io.observe(el));
}

/** Call `cb` once when `el` first comes into view (or at once if it can't be observed). */
export function whenVisible(el: Element, cb: () => void, margin = '0px 0px -12% 0px'): void {
  if (!('IntersectionObserver' in window)) return cb();
  const io = new IntersectionObserver(
    (entries) => {
      if (entries.some((e) => e.isIntersecting)) {
        io.disconnect();
        cb();
      }
    },
    { rootMargin: margin, threshold: 0.2 },
  );
  io.observe(el);
}

/** Track whether `el` is (nearly) on screen; `cb(true|false)` on each change. */
export function onVisibility(el: Element, cb: (visible: boolean) => void, margin = '120px 0px'): () => void {
  if (!('IntersectionObserver' in window)) {
    cb(true);
    return () => {};
  }
  const io = new IntersectionObserver((entries) => cb(entries[entries.length - 1].isIntersecting), { rootMargin: margin });
  io.observe(el);
  return () => io.disconnect();
}

// ── Scroll scenes ──

export interface Scene {
  /** Called on scroll and resize with the page's scroll position and the viewport height. */
  update(scrollY: number, vh: number): void;
  /** Called on resize, before update, to cache geometry (layout reads happen here only). */
  measure?(scrollY: number): void;
}

const scenes: Scene[] = [];
let queued = false;

function frame() {
  queued = false;
  const y = window.scrollY;
  const vh = window.innerHeight;
  for (const s of scenes) s.update(y, vh);
}

function queue() {
  if (queued) return;
  queued = true;
  requestAnimationFrame(frame);
}

function remeasure() {
  const y = window.scrollY;
  for (const s of scenes) s.measure?.(y);
  queue();
}

let started = false;

export function addScene(scene: Scene): void {
  scenes.push(scene);
  scene.measure?.(window.scrollY);
  if (!started) {
    started = true;
    window.addEventListener('scroll', queue, { passive: true });
    window.addEventListener('resize', remeasure);
    // Fonts and late layout move things: measure again once they settle.
    document.fonts?.ready.then(remeasure);
    window.addEventListener('load', remeasure);
    new ResizeObserver(remeasure).observe(document.body);
  }
  queue();
}

/** Where `y` sits between `a` and `b`, clamped to 0..1. */
export const progress = (y: number, a: number, b: number): number => Math.min(1, Math.max(0, (y - a) / (b - a)));

export const easeOut = (t: number): number => 1 - Math.pow(1 - t, 3);
export const easeInOut = (t: number): number => (t < 0.5 ? 4 * t * t * t : 1 - Math.pow(-2 * t + 2, 3) / 2);

/** The page offset of an element's top (a layout read: call from measure()). */
export function pageTop(el: Element): number {
  return el.getBoundingClientRect().top + window.scrollY;
}

/** Count a number up from 0 when its card reveals ("3.24", "1,055", "20.7"). */
export function countUp(el: HTMLElement, ms = 1100): void {
  const target = el.textContent ?? '';
  const n = Number(target.replace(/,/g, ''));
  if (!Number.isFinite(n) || reducedMotion()) return;
  const decimals = (target.split('.')[1] ?? '').length;
  const comma = target.includes(',');
  const fmt = (v: number) => {
    const s = v.toFixed(decimals);
    return comma ? Number(s).toLocaleString('en-US', { minimumFractionDigits: decimals, maximumFractionDigits: decimals }) : s;
  };
  const t0 = performance.now();
  el.textContent = fmt(0);
  const step = (now: number) => {
    const t = Math.min(1, (now - t0) / ms);
    el.textContent = t < 1 ? fmt(n * easeOut(t)) : target;
    if (t < 1) requestAnimationFrame(step);
  };
  requestAnimationFrame(step);
}
