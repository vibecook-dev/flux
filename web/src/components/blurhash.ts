/**
 * Blurhash placeholders. Decoding is cheap at 8×12 pixels; the result is cached
 * as a data URL so a 10k-item grid decodes each hash once.
 */
import { decode } from 'blurhash';

const B83 = '0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz#$%*+,-.:;=?@[]^_{|}~';

function decode83(s: string): number {
  let v = 0;
  for (const ch of s) v = v * 83 + B83.indexOf(ch);
  return v;
}

/** The hash's DC component: the image's average colour, as `rgb(r g b)`. */
export function averageColor(hash: string): string | null {
  if (hash.length < 6) return null;
  const v = decode83(hash.slice(2, 6));
  return `rgb(${v >> 16} ${(v >> 8) & 255} ${v & 255})`;
}

const cache = new Map<string, string>();
let canvas: HTMLCanvasElement | null = null;

/** A tiny PNG data URL of the decoded hash (stretched with CSS, it blurs itself). */
export function blurhashDataUrl(hash: string, w = 8, h = 12): string | null {
  const key = `${hash}|${w}x${h}`;
  const hit = cache.get(key);
  if (hit) return hit;
  try {
    const pixels = decode(hash, w, h);
    canvas ??= document.createElement('canvas');
    canvas.width = w;
    canvas.height = h;
    const ctx = canvas.getContext('2d');
    if (!ctx) return null;
    const img = ctx.createImageData(w, h);
    img.data.set(pixels);
    ctx.putImageData(img, 0, 0);
    const url = canvas.toDataURL('image/png');
    // Enough for a 10,000-title library (each entry is a few hundred bytes).
    if (cache.size > 12000) cache.clear();
    cache.set(key, url);
    return url;
  } catch {
    return null; // malformed hash: fall back to the plain surface colour
  }
}
