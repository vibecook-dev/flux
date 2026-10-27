/**
 * Text for GPU lenses. Each string is drawn once with Canvas 2D (so it uses the
 * page's own fonts and kerning), packed into one glyph texture, and drawn as a
 * quad tinted in the shader, on whole device pixels so it stays crisp. Icons
 * (the watched check, the AI spark, the flux mark) live in the same atlas.
 *
 * When the atlas fills up it starts over (`gen` bumps); a lens rebuilds its frame
 * if that happened while it was building one.
 */
import { TEXTURE, type Gpu } from './device';

export interface TextStyle {
  /** CSS font-family list. */
  family: string;
  /** CSS pixels. */
  size: number;
  weight: number;
  /** Letter spacing, in em. */
  spacing?: number;
  upper?: boolean;
}

export interface Glyph {
  u0: number;
  v0: number;
  u1: number;
  v1: number;
  /** Quad size in CSS pixels (padding included). */
  w: number;
  h: number;
  /** From the quad's top to the baseline, CSS pixels. */
  baseline: number;
  /** Padding around the ink, CSS pixels. */
  pad: number;
}

export type IconName = 'check' | 'spark' | 'flux';

const SIZE = 2048;
const PAD = 2;

/** The longest prefix of `text` that fits `max` with an ellipsis (by code point). */
export function ellipsize(text: string, max: number, measure: (s: string) => number): string {
  if (measure(text) <= max) return text;
  const cps = Array.from(text);
  let lo = 0;
  let hi = cps.length;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (measure(cps.slice(0, mid).join('').trimEnd() + '…') <= max) lo = mid;
    else hi = mid - 1;
  }
  return lo === 0 ? '…' : cps.slice(0, lo).join('').trimEnd() + '…';
}

/** Greedy word wrap into at most `lines` lines (the last one ellipsised). */
export function wrap(text: string, max: number, lines: number, measure: (s: string) => number): string[] {
  const words = text.split(/\s+/).filter(Boolean);
  const out: string[] = [];
  let cur = '';
  for (let i = 0; i < words.length; i++) {
    const next = cur ? `${cur} ${words[i]}` : words[i];
    if (measure(next) <= max || !cur) {
      cur = next;
      continue;
    }
    out.push(cur);
    cur = words[i];
    if (out.length === lines - 1) {
      cur = words.slice(i).join(' ');
      break;
    }
  }
  if (cur) out.push(cur);
  return out.slice(0, lines).map((l, i, a) => (i === a.length - 1 ? ellipsize(l, max, measure) : l));
}

export class TextAtlas {
  readonly view: GPUTextureView;
  /** Bumps when the atlas starts over (glyphs handed out before are gone). */
  gen = 0;
  private gpu: Gpu;
  private tex: GPUTexture;
  private canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D;
  private entries = new Map<string, Glyph>();
  private shelves: { y: number; h: number; x: number }[] = [];
  private nextY = 0;
  private dirty: [number, number, number, number] | null = null;
  private listeners = new Set<() => void>();

  constructor(gpu: Gpu) {
    this.gpu = gpu;
    this.tex = gpu.device.createTexture({ label: 'flux glyphs', size: [SIZE, SIZE], format: 'r8unorm', usage: TEXTURE.TEXTURE_BINDING | TEXTURE.COPY_DST | TEXTURE.RENDER_ATTACHMENT });
    this.view = this.tex.createView();
    this.canvas = document.createElement('canvas');
    this.canvas.width = SIZE;
    this.canvas.height = SIZE;
    // Canvas text takes its font smoothing from the canvas element's style, and a canvas
    // outside the page gets the platform default (heavier on macOS than the page's
    // antialiased text): keep it in the page, hidden.
    this.canvas.hidden = true;
    this.canvas.setAttribute('aria-hidden', 'true');
    document.body.append(this.canvas);
    const ctx = this.canvas.getContext('2d');
    if (!ctx) throw new Error('No 2D canvas for text.');
    this.ctx = ctx;
    // Web fonts arriving late: draw everything again with the real faces.
    document.fonts?.addEventListener('loadingdone', () => this.clear());
  }

  /** Lenses redraw when the atlas starts over. */
  onChange(fn: () => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  private font(style: TextStyle, dpr: number): void {
    const px = style.size * dpr;
    this.ctx.font = `${style.weight} ${px}px ${style.family}`;
    this.ctx.letterSpacing = style.spacing ? `${style.spacing * px}px` : '0px';
  }

  /** Width of `text` in `style`, CSS pixels. */
  measure(text: string, style: TextStyle): number {
    this.font(style, 1);
    return this.ctx.measureText(style.upper ? text.toUpperCase() : text).width;
  }

  /** `text` in `style`, ellipsised to `maxWidth` CSS pixels. Null only if the atlas can't hold it. */
  get(text: string, style: TextStyle, maxWidth: number, dpr: number): Glyph | null {
    const key = `${style.family}|${style.size}|${style.weight}|${style.spacing ?? 0}|${style.upper ? 1 : 0}|${dpr}|${Math.round(maxWidth)}|${text}`;
    const hit = this.entries.get(key);
    if (hit) return hit;
    const ctx = this.ctx;
    this.font(style, dpr);
    const max = Math.max(1, maxWidth * dpr);
    let s = style.upper ? text.toUpperCase() : text;
    let m = ctx.measureText(s);
    if (m.width > max) {
      s = ellipsize(s, max, (x) => ctx.measureText(x).width);
      m = ctx.measureText(s);
    }
    const px = style.size * dpr;
    const ascent = Math.ceil(m.fontBoundingBoxAscent || px * 0.8);
    const descent = Math.ceil(m.fontBoundingBoxDescent || px * 0.25);
    const w = Math.ceil(Math.min(m.width, max)) + PAD * 2;
    const h = ascent + descent + PAD * 2;
    const spot = this.place(w, h);
    if (!spot) return null;
    ctx.clearRect(spot.x, spot.y, w, h);
    ctx.fillStyle = '#fff';
    ctx.textBaseline = 'alphabetic';
    ctx.fillText(s, spot.x + PAD, spot.y + PAD + ascent);
    this.mark(spot.x, spot.y, w, h);
    const g = this.glyph(spot.x, spot.y, w, h, dpr, (PAD + ascent) / dpr);
    this.entries.set(key, g);
    return g;
  }

  /** A small icon, `size` CSS pixels square. */
  icon(name: IconName, size: number, dpr: number): Glyph | null {
    const key = `icon|${name}|${size}|${dpr}`;
    const hit = this.entries.get(key);
    if (hit) return hit;
    const px = Math.ceil(size * dpr);
    const w = px + PAD * 2;
    const spot = this.place(w, w);
    if (!spot) return null;
    const ctx = this.ctx;
    ctx.clearRect(spot.x, spot.y, w, w);
    ctx.save();
    ctx.translate(spot.x + PAD, spot.y + PAD);
    ctx.fillStyle = '#fff';
    ctx.strokeStyle = '#fff';
    if (name === 'check') {
      ctx.scale(px / 16, px / 16);
      ctx.lineWidth = 2;
      ctx.lineCap = 'round';
      ctx.lineJoin = 'round';
      ctx.stroke(new Path2D('M3.5 8.5l3 3 6-7'));
    } else if (name === 'flux') {
      // The flux mark (spine.ts), centred: its second stroke at 45%, as the spine paints it.
      ctx.scale(px / 35, px / 35);
      ctx.translate(0, 3.5);
      ctx.fill(new Path2D('M2 26 L20 2 L27 2 L9 26 Z'));
      ctx.globalAlpha = 0.45;
      ctx.fill(new Path2D('M16 26 L28 10 L33 10 L21 26 Z'));
    } else {
      ctx.scale(px / 12, px / 12);
      ctx.fill(new Path2D('M6 1l1.1 3.4L10.5 5.5 7.1 6.6 6 10 4.9 6.6 1.5 5.5l3.4-1.1z'));
    }
    ctx.restore();
    this.mark(spot.x, spot.y, w, w);
    const g = this.glyph(spot.x, spot.y, w, w, dpr, w / dpr);
    this.entries.set(key, g);
    return g;
  }

  private glyph(x: number, y: number, w: number, h: number, dpr: number, baseline: number): Glyph {
    return { u0: x / SIZE, v0: y / SIZE, u1: (x + w) / SIZE, v1: (y + h) / SIZE, w: w / dpr, h: h / dpr, baseline, pad: PAD / dpr };
  }

  private place(w: number, h: number): { x: number; y: number } | null {
    const spot = this.fit(w, h);
    if (spot) return spot;
    this.clear();
    return this.fit(w, h);
  }

  private fit(w: number, h: number): { x: number; y: number } | null {
    if (w > SIZE || h > SIZE) return null;
    const rh = Math.ceil(h / 4) * 4;
    for (const s of this.shelves) {
      if (s.h >= rh && s.h <= rh * 1.35 && SIZE - s.x >= w) {
        const spot = { x: s.x, y: s.y };
        s.x += w + 1;
        return spot;
      }
    }
    if (this.nextY + rh > SIZE) return null;
    const shelf = { y: this.nextY, h: rh, x: w + 1 };
    this.shelves.push(shelf);
    this.nextY += rh + 1;
    return { x: 0, y: shelf.y };
  }

  private mark(x: number, y: number, w: number, h: number): void {
    const d = this.dirty;
    if (!d) this.dirty = [x, y, x + w, y + h];
    else {
      d[0] = Math.min(d[0], x);
      d[1] = Math.min(d[1], y);
      d[2] = Math.max(d[2], x + w);
      d[3] = Math.max(d[3], y + h);
    }
  }

  /** Start over: every glyph is drawn again as it is asked for. */
  clear(): void {
    this.entries.clear();
    this.shelves = [];
    this.nextY = 0;
    this.ctx.clearRect(0, 0, SIZE, SIZE);
    this.dirty = [0, 0, SIZE, SIZE];
    this.gen++;
    for (const l of this.listeners) l();
  }

  /** Send what was drawn since the last frame to the GPU. */
  flush(): void {
    const d = this.dirty;
    if (!d) return;
    this.dirty = null;
    const w = Math.min(SIZE, d[2]) - d[0];
    const h = Math.min(SIZE, d[3]) - d[1];
    if (w <= 0 || h <= 0) return;
    this.gpu.device.queue.copyExternalImageToTexture({ source: this.canvas, origin: [d[0], d[1]] }, { texture: this.tex, origin: [d[0], d[1]], premultipliedAlpha: true }, [w, h]);
  }
}

const atlases = new WeakMap<GPUDevice, TextAtlas>();

/** The glyph atlas for this device (one per app). */
export function textAtlas(gpu: Gpu): TextAtlas {
  let a = atlases.get(gpu.device);
  if (!a) {
    a = new TextAtlas(gpu);
    atlases.set(gpu.device, a);
  }
  return a;
}
