/**
 * A small palette per title, read from its blurhash (no image download needed):
 * c1 is the colour the art is "about", c2 its shadow, c3 an accent, avg the mean.
 * Lenses paint with it (tape spines, gallery light), the shell lights the room
 * with it and tints the Play button. Cached per hash.
 */
import { decode } from 'blurhash';
import type { ItemSummary } from '../api/types';

export interface Palette {
  c1: string;
  c2: string;
  c3: string;
  avg: string;
  /** Of c1: hue in degrees, saturation 0–1. */
  hue: number;
  sat: number;
  /** Relative luminance of the average, 0–1. */
  lum: number;
}

type RGB = [number, number, number];

export const hex = ([r, g, b]: RGB): string => '#' + [r, g, b].map((v) => Math.round(Math.max(0, Math.min(255, v))).toString(16).padStart(2, '0')).join('');

export function rgb(h: string): RGB {
  const n = parseInt(h.slice(1), 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

export function mixHex(a: string, b: string, t: number): string {
  const x = rgb(a);
  const y = rgb(b);
  return hex([x[0] + (y[0] - x[0]) * t, x[1] + (y[1] - x[1]) * t, x[2] + (y[2] - x[2]) * t]);
}

export function luminance(h: string): number {
  const [r, g, b] = rgb(h).map((v) => {
    const c = v / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

/** [hue 0–360, saturation 0–1, lightness 0–1] */
export function hsl([r0, g0, b0]: RGB): [number, number, number] {
  const r = r0 / 255;
  const g = g0 / 255;
  const b = b0 / 255;
  const mx = Math.max(r, g, b);
  const mn = Math.min(r, g, b);
  const l = (mx + mn) / 2;
  if (mx === mn) return [0, 0, l];
  const d = mx - mn;
  const s = l > 0.5 ? d / (2 - mx - mn) : d / (mx + mn);
  let h = mx === r ? (g - b) / d + (g < b ? 6 : 0) : mx === g ? (b - r) / d + 2 : (r - g) / d + 4;
  h *= 60;
  return [h, s, l];
}

function hslHex(h: number, s: number, l: number): string {
  const k = (n: number) => (n + h / 30) % 12;
  const a = s * Math.min(l, 1 - l);
  const f = (n: number) => l - a * Math.max(-1, Math.min(k(n) - 3, Math.min(9 - k(n), 1)));
  return hex([f(0) * 255, f(8) * 255, f(4) * 255]);
}

/** A version dark enough to carry white text (tinted glass, tape spines). */
export function deepen(h: string, target = 0.09): string {
  let c = h;
  for (let i = 0; i < 14 && luminance(c) > target; i++) c = mixHex(c, '#000000', 0.16);
  return c;
}

/** Palette from pixels (exported for tests): k-means with k = 4, seeded across brightness. */
export function paletteFromPixels(px: RGB[]): Palette {
  const avgRgb = px.reduce<RGB>((a, p) => [a[0] + p[0], a[1] + p[1], a[2] + p[2]], [0, 0, 0]).map((v) => v / px.length) as RGB;
  const sorted = [...px].sort((a, b) => a[0] + a[1] + a[2] - (b[0] + b[1] + b[2]));
  let cents: RGB[] = [0.08, 0.38, 0.66, 0.92].map((q) => [...sorted[Math.floor(q * (sorted.length - 1))]] as RGB);
  const assign = new Array<number>(px.length).fill(0);
  for (let it = 0; it < 8; it++) {
    for (let i = 0; i < px.length; i++) {
      let best = 0;
      let bd = Infinity;
      for (let k = 0; k < cents.length; k++) {
        const dr = px[i][0] - cents[k][0];
        const dg = px[i][1] - cents[k][1];
        const db = px[i][2] - cents[k][2];
        const dd = dr * dr * 0.3 + dg * dg * 0.59 + db * db * 0.11;
        if (dd < bd) {
          bd = dd;
          best = k;
        }
      }
      assign[i] = best;
    }
    const sums = cents.map(() => [0, 0, 0, 0]);
    for (let i = 0; i < px.length; i++) {
      const s = sums[assign[i]];
      s[0] += px[i][0];
      s[1] += px[i][1];
      s[2] += px[i][2];
      s[3]++;
    }
    cents = sums.map((s, k) => (s[3] ? ([s[0] / s[3], s[1] / s[3], s[2] / s[3]] as RGB) : cents[k]));
  }
  const info = cents
    .map((c, k) => {
      const n = assign.filter((a) => a === k).length / px.length;
      const [, s, l] = hsl(c);
      return { c, n, s, l };
    })
    .filter((x) => x.n > 0.02);
  const score = (x: (typeof info)[number]) => x.n * (0.25 + x.s * 1.6) * (x.l < 0.1 || x.l > 0.92 ? 0.25 : 1);
  const c1 = [...info].sort((a, b) => score(b) - score(a))[0];
  const c2 = [...info].sort((a, b) => a.l - b.l)[0];
  const c3 = [...info].filter((x) => x !== c1).sort((a, b) => b.s * Math.sqrt(b.n) - a.s * Math.sqrt(a.n))[0] ?? c1;
  const [hue, sat] = hsl(c1.c);
  const avg = hex(avgRgb);
  return { c1: hex(c1.c), c2: hex(c2.c), c3: hex(c3.c), avg, hue, sat, lum: luminance(avg) };
}

/** A calm colour for titles with no art, stable per title. */
export function fallbackPalette(key: string): Palette {
  let h = 2166136261;
  for (let i = 0; i < key.length; i++) {
    h ^= key.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  const hue = (h >>> 0) % 360;
  const c1 = hslHex(hue, 0.34, 0.34);
  const c2 = hslHex((hue + 30) % 360, 0.28, 0.13);
  const c3 = hslHex(hue, 0.45, 0.6);
  return { c1, c2, c3, avg: mixHex(c1, c2, 0.5), hue, sat: 0.34, lum: luminance(mixHex(c1, c2, 0.5)) };
}

const cache = new Map<string, Palette>();

export function paletteOfHash(hash: string | null | undefined, fallbackKey: string): Palette {
  const key = hash ?? `title:${fallbackKey}`;
  const hit = cache.get(key);
  if (hit) return hit;
  let p: Palette;
  if (hash) {
    try {
      const data = decode(hash, 6, 9);
      const px: RGB[] = [];
      for (let i = 0; i < data.length; i += 4) px.push([data[i], data[i + 1], data[i + 2]]);
      p = paletteFromPixels(px);
    } catch {
      p = fallbackPalette(fallbackKey);
    }
  } else p = fallbackPalette(fallbackKey);
  if (cache.size > 20000) cache.clear();
  cache.set(key, p);
  return p;
}

/** The palette of a title's poster (or backdrop, or a stable fallback from its title). */
export function paletteOf(item: Pick<ItemSummary, 'poster' | 'backdrop' | 'title'>): Palette {
  return paletteOfHash(item.poster?.blurhash ?? item.backdrop?.blurhash, item.title);
}

const B83 = '0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz#$%*+,-.:;=?@[]^_{|}~';

/**
 * The poster's average colour from its blurhash's DC term (four characters, no
 * decoding): cheap enough to sort 10,000 titles by colour in a frame.
 */
export function averageOfHash(hash: string | null | undefined): { hue: number; sat: number; lum: number } | null {
  if (!hash || hash.length < 6) return null;
  let v = 0;
  for (const ch of hash.slice(2, 6)) {
    const d = B83.indexOf(ch);
    if (d < 0) return null;
    v = v * 83 + d;
  }
  const c: RGB = [v >> 16, (v >> 8) & 255, v & 255];
  const [h, sat] = hsl(c);
  return { hue: h, sat, lum: luminance(hex(c)) };
}
