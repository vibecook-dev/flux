/**
 * The current lens, for the overlays that need it: the detail page asks where
 * a poster is so it can fly back into place, and pauses the lens underneath.
 */
import type { Id } from '../api/types';
import type { LensDef, LensView } from './types';

interface Active {
  def: LensDef;
  view: LensView;
}

let active: Active | null = null;
let focus: Id | null = null;
const focusListeners = new Set<(id: Id | null) => void>();

export function setActiveLens(next: Active | null): void {
  active = next;
}

export function activeLens(): Active | null {
  return active;
}

export function rectInLens(id: Id): DOMRect | null {
  try {
    const r = active?.view.rectOf(id) ?? null;
    if (!r || !r.width || r.bottom < 0 || r.top > window.innerHeight || r.right < 0 || r.left > window.innerWidth) return null;
    return r;
  } catch {
    return null;
  }
}

/** The focused title, shared by every lens (not React state: hover moves it often). */
export function lensFocus(): Id | null {
  return focus;
}

export function setLensFocus(id: Id | null): void {
  if (focus === id) return;
  focus = id;
  for (const l of focusListeners) l(id);
}

export function onLensFocus(fn: (id: Id | null) => void): () => void {
  focusListeners.add(fn);
  return () => focusListeners.delete(fn);
}
