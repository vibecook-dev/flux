/**
 * Shared-element flights: a poster flies from where it is in one place (a lens)
 * to where it lands in another (the next lens, the detail page), so nothing
 * opens out of nowhere. Also the hand-off of "where did this poster start" from
 * the lens to the detail overlay.
 */
import type { Id, ItemSummary } from '../api/types';
import { artElement } from './art';
import { reducedMotion, SOFT } from './dom';

const origins = new Map<Id, { rect: DOMRect; at: number }>();

/** The lens remembers where a poster was when it was opened. */
export function rememberOrigin(id: Id, rect: DOMRect | null | undefined): void {
  if (rect && rect.width > 0) origins.set(id, { rect, at: performance.now() });
}

/** The detail overlay picks it up (once, and only if it is fresh). */
export function takeOrigin(id: Id): DOMRect | null {
  const o = origins.get(id);
  origins.delete(id);
  if (!o || performance.now() - o.at > 3000) return null;
  return o.rect;
}

/** A ghost of the poster flies from `from` to `to`, then fades (the real one is already there). */
export function fly(item: ItemSummary, from: DOMRect, to: DOMRect, { duration = 640, fade = true, radius = 8 } = {}): Promise<void> {
  if (reducedMotion() || !from.width || !to.width) return Promise.resolve();
  const ghost = artElement(item, { width: to.width, eager: true, marks: false, label: false });
  Object.assign(ghost.style, {
    position: 'fixed',
    left: `${to.left}px`,
    top: `${to.top}px`,
    width: `${to.width}px`,
    height: `${to.height}px`,
    zIndex: '24',
    pointerEvents: 'none',
    borderRadius: `${radius}px`,
    transformOrigin: '0 0',
    boxShadow: '0 30px 60px -20px rgb(0 0 0 / 0.5)',
  });
  document.body.append(ghost);
  const sx = from.width / to.width;
  const sy = from.height / to.height;
  const a = ghost.animate(
    [
      { transform: `translate(${from.left - to.left}px, ${from.top - to.top}px) scale(${sx}, ${sy})`, opacity: 1 },
      { transform: 'none', opacity: 1, offset: 0.82 },
      { transform: 'none', opacity: fade ? 0 : 1 },
    ],
    { duration, easing: SOFT, fill: 'forwards' },
  );
  return a.finished.then(
    () => ghost.remove(),
    () => ghost.remove(),
  );
}

/**
 * Move an element itself: from `from` to where it is now (arrival), or from where
 * it is now to `to` (departure, held at the end). Resolves when done.
 */
export function flyElement(node: HTMLElement, { from, to }: { from?: DOMRect | null; to?: DOMRect | null }, duration = 600): Promise<void> {
  if (reducedMotion()) return Promise.resolve();
  const here = node.getBoundingClientRect();
  if (!here.width) return Promise.resolve();
  const t = (r: DOMRect) => `translate(${r.left - here.left}px, ${r.top - here.top}px) scale(${r.width / here.width}, ${r.height / here.height})`;
  node.style.transformOrigin = '0 0';
  const frames = from ? [{ transform: t(from) }, { transform: 'none' }] : to ? [{ transform: 'none' }, { transform: t(to) }] : null;
  if (!frames) return Promise.resolve();
  return node.animate(frames, { duration, easing: SOFT, fill: to ? 'forwards' : 'none' }).finished.then(
    () => undefined,
    () => undefined,
  );
}
