/** Colours the GPU lenses share: poster placeholders, and CSS colours as straight RGBA 0–1. */
import type { ItemSummary } from '../../api/types';
import { paletteOf } from '../palette';
import type { RGBA } from './quads';

export function hexColor(h: string): RGBA {
  const n = parseInt(h.slice(1, 7), 16);
  return [((n >> 16) & 255) / 255, ((n >> 8) & 255) / 255, (n & 255) / 255, 1];
}

let probe: CanvasRenderingContext2D | null = null;

/** Any CSS colour as straight RGBA 0–1 (the canvas parses it); `fallback` when it isn't one. */
export function cssColor(value: string, fallback: RGBA): RGBA {
  probe ??= document.createElement('canvas').getContext('2d');
  if (!probe || !value.trim()) return fallback;
  probe.fillStyle = '#000';
  probe.fillStyle = value.trim();
  const v = String(probe.fillStyle);
  if (v.startsWith('#')) return hexColor(v);
  const m = /rgba?\(([^)]+)\)/.exec(v);
  if (!m) return fallback;
  const p = m[1].split(',').map((s) => Number(s.trim()));
  return [p[0] / 255, p[1] / 255, p[2] / 255, Number.isFinite(p[3]) ? p[3] : 1];
}

/** CSS `color-mix(a, b t)`: `t` of b, mixed premultiplied as CSS does (sRGB, not oklab). */
export function mixColor(a: RGBA, b: RGBA, t: number): RGBA {
  const al = a[3] * (1 - t) + b[3] * t;
  if (al <= 0) return [0, 0, 0, 0];
  const ch = (i: number) => (a[i] * a[3] * (1 - t) + b[i] * b[3] * t) / al;
  return [ch(0), ch(1), ch(2), al];
}

const B83 = '0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz#$%*+,-.:;=?@[]^_{|}~';
const dcCache = new Map<string, RGBA | null>();

/** The poster's average colour (the blurhash DC term): the placeholder until it loads. */
export function dcColor(hash: string): RGBA | null {
  let c = dcCache.get(hash);
  if (c !== undefined) return c;
  c = null;
  if (hash.length >= 6) {
    let v = 0;
    for (const ch of hash.slice(2, 6)) {
      const d = B83.indexOf(ch);
      if (d < 0) {
        v = -1;
        break;
      }
      v = v * 83 + d;
    }
    if (v >= 0) c = [((v >> 16) & 255) / 255, ((v >> 8) & 255) / 255, (v & 255) / 255, 1];
  }
  if (dcCache.size > 20000) dcCache.clear();
  dcCache.set(hash, c);
  return c;
}

const paperCache = new Map<string, [RGBA, RGBA]>();

/** A title without art: its stable fallback gradient (art.ts draws the same). */
export function paperOf(item: ItemSummary): [RGBA, RGBA] {
  let p = paperCache.get(item.title);
  if (!p) {
    const pal = paletteOf(item);
    p = [hexColor(pal.c1), hexColor(pal.c2)];
    if (paperCache.size > 20000) paperCache.clear();
    paperCache.set(item.title, p);
  }
  return p;
}
