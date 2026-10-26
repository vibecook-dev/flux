/**
 * Bias lighting: the page background takes on the colour of whatever artwork
 * has focus, like the light a screen throws onto a dark wall. The colour is the
 * blurhash's DC term (the image's average), so it costs nothing to compute.
 *
 * The CSS variable is set on the ambient layer element only (registered with
 * `inherits: false`), so the 900 ms transition never restyles the document.
 */
import { averageColor } from '../components/blurhash';

let el: HTMLElement | null = null;
let timer: ReturnType<typeof setTimeout> | null = null;
let last = '';
let watcher: ((color: string) => void) | null = null;

export function registerAmbientElement(node: HTMLElement | null): void {
  el = node;
  if (el && last) el.style.setProperty('--ambient', last);
}

/** Light the room with this colour (debounced so sweeping a row doesn't flicker). */
export function setAmbient(color: string | null, immediate = false): void {
  if (!color || color === last) return;
  if (timer) clearTimeout(timer);
  const run = () => {
    last = color;
    el?.style.setProperty('--ambient', color);
    watcher?.(color);
  };
  if (immediate) run();
  else timer = setTimeout(run, 70);
}

/** Light the room from a blurhash string. */
export function setAmbientFromHash(hash: string | null | undefined, immediate = false): void {
  if (hash) setAmbient(averageColor(hash), immediate);
}

/** The colour the room is lit with now, or null before any artwork had focus. */
export function ambientColor(): string | null {
  return last || null;
}

/**
 * Follow the room light as it is applied, after the debounce (one watcher; null
 * stops). Only the live demo's bridge watches (src/mock/demo-bridge.ts), so the
 * product site's glow can take the colour of what a visitor browses.
 */
export function watchAmbient(fn: ((color: string) => void) | null): void {
  watcher = fn;
}
