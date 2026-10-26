/**
 * Lenses you added, kept on this device (prefs key `lenses.mine`, read by
 * registry.ts `yourLenses`). A lens is a small JSON file, so it can also be
 * shared and added on another device.
 */
import { prefs } from '../dom';
import type { StoredLens } from '../registry';
import type { KitSpec } from './runtime';

const KEY = 'lenses.mine';
const MAX = 24;

export function storedLenses(): StoredLens[] {
  const v = prefs.get<StoredLens[]>(KEY, []);
  return Array.isArray(v) ? v.filter((l) => l && typeof l.id === 'string' && typeof l.name === 'string') : [];
}

/** Keep a checked lens (replacing one with the same id). */
export function keepLens(spec: KitSpec): StoredLens {
  const entry: StoredLens = { id: spec.id, name: spec.name, blurb: spec.blurb, spec };
  const rest = storedLenses().filter((l) => l.id !== spec.id);
  prefs.set(KEY, [...rest, entry].slice(-MAX));
  return entry;
}

export function removeLens(id: string): void {
  prefs.set(
    KEY,
    storedLenses().filter((l) => l.id !== id),
  );
}

/** The lens as a file to share: the same JSON an agent writes. */
export function lensFile(spec: KitSpec): string {
  const { name, blurb, input, step, window, animated, css, scene, item, layout } = spec;
  return JSON.stringify({ name, blurb, input, step, window, animated, css, scene, item, layout }, null, 2);
}
