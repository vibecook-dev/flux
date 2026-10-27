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

/**
 * Opaque RGBA pixels as a 24-bit BMP data URL. Written byte by byte: a canvas
 * `toDataURL` costs about a millisecond of main thread per placeholder (canvas
 * reset, PNG deflate), and search results and scrolled-in tiles ask for dozens
 * at a time.
 */
function bmpDataUrl(rgba: Uint8ClampedArray, w: number, h: number): string {
  const stride = (w * 3 + 3) & ~3;
  const size = 54 + stride * h;
  const b = new Uint8Array(size);
  const v = new DataView(b.buffer);
  b[0] = 0x42; // 'BM'
  b[1] = 0x4d;
  v.setUint32(2, size, true);
  v.setUint32(10, 54, true); // pixels follow the two headers
  v.setUint32(14, 40, true); // BITMAPINFOHEADER
  v.setInt32(18, w, true);
  v.setInt32(22, h, true); // positive: rows bottom-up
  v.setUint16(26, 1, true);
  v.setUint16(28, 24, true);
  v.setUint32(34, stride * h, true);
  for (let y = 0; y < h; y++) {
    let o = 54 + (h - 1 - y) * stride;
    for (let x = 0; x < w; x++) {
      const i = (y * w + x) * 4;
      b[o++] = rgba[i + 2];
      b[o++] = rgba[i + 1];
      b[o++] = rgba[i];
    }
  }
  let s = '';
  for (let i = 0; i < size; i++) s += String.fromCharCode(b[i]);
  return `data:image/bmp;base64,${btoa(s)}`;
}

/** A tiny data URL image of the decoded hash (stretched with CSS, it blurs itself). */
export function blurhashDataUrl(hash: string, w = 8, h = 12): string | null {
  const key = `${hash}|${w}x${h}`;
  const hit = cache.get(key);
  if (hit) return hit;
  try {
    const url = bmpDataUrl(decode(hash, w, h), w, h);
    // Enough for a 10,000-title library (each entry is a few hundred bytes).
    if (cache.size > 12000) cache.clear();
    cache.set(key, url);
    return url;
  } catch {
    return null; // malformed hash: fall back to the plain surface colour
  }
}
