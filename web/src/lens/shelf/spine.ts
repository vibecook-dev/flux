/**
 * Canvas 2D painting for the GPU Shelf's sprites, following the DOM Shelf
 * (shelf.module.css) rule for rule: a tape's spine (the treatment's colours and
 * rules, the cap with year, № or OFFLINE, the title set sideways in condensed
 * Archivo and shrunk to fit, a box set's season count, the flux VIDEO mark,
 * stickers and the progress stripe), its back cover, and the shelf fittings
 * (board, lip, bookends, the light on the wall, the shade on a tape's sides).
 *
 * Painters draw in CSS pixels on a context scaled to device pixels.
 *
 * A spine's alpha is not coverage (the renderer rounds its corners itself) but how
 * glossy it is, for the renderer's lighting (scene3d.ts, `MAT3D.spine`): 1 is the
 * sleeve's card, less is shinier: a clamshell's plastic, a sticker.
 */
import type { ItemSummary } from '../../api/types';
import { rand01 } from '../dom';
import { hex, rgb } from '../palette';
import { isNew, progressFraction } from '../sections';
import { collectorNo, spineTitleParts, type SpineLook } from './geometry';

export interface ShelfFonts {
  /** CSS font-family lists. */
  mono: string;
  cond: string;
  body: string;
}

export interface SpineSpec {
  /** Spine width (box sets are wider), tape height and the plain tape's width, CSS pixels. */
  tw: number;
  H: number;
  W: number;
}

/** What a spine shows, as a key: it is painted again only when one of these changes. */
export function spineKey(item: ItemSummary, look: SpineLook, s: SpineSpec, dpr: number, fontGen = 0): string {
  const p = progressFraction(item);
  return [
    's',
    item.id,
    item.title,
    item.parent_title ?? '',
    item.year ?? '',
    item.kind,
    item.child_count ?? '',
    item.available ? 1 : 0,
    isNew(item) ? 1 : 0,
    p === null ? '' : p.toFixed(3),
    look.style,
    look.bg,
    look.acc,
    look.ink,
    s.tw,
    s.H,
    dpr,
    fontGen,
  ].join('|');
}

/** CSS `grayscale(0.9)` on a colour (offline spines); their dimming is the renderer's tint. */
export function grayHex(c: string, on = true): string {
  if (!on) return c;
  const [r, g, b] = rgb(c);
  const l = 0.2126 * r + 0.7152 * g + 0.0722 * b;
  return hex([r + (l - r) * 0.9, g + (l - g) * 0.9, b + (l - b) * 0.9]);
}

const toLinear = (v: number) => {
  const c = v / 255;
  return c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
};
const toSrgb = (c: number) => 255 * (c <= 0.0031308 ? c * 12.92 : 1.055 * c ** (1 / 2.4) - 0.055);

/**
 * `color-mix(in oklab, c k, #000)`: in Oklab, mixing with black scales L, a and b by
 * `k`, which is the linear-light colour scaled by k³.
 */
export function mixBlackOklab(c: string, k: number): string {
  const f = k * k * k;
  const [r, g, b] = rgb(c);
  return hex([toSrgb(toLinear(r) * f), toSrgb(toLinear(g) * f), toSrgb(toLinear(b) * f)]);
}

/** CSS `grayscale(0.9) brightness(0.62)` on a colour: an offline tape's print, as its spine is filtered. */
export function offlineHex(c: string): string {
  const [r, g, b] = rgb(grayHex(c));
  return hex([r * 0.62, g * 0.62, b * 0.62]);
}

/**
 * The band across the top of a cover, as shelf.module.css prints it (`.cover`): the
 * spine's colours carried round the corner, as a sleeve's print wraps, ruled off as
 * the spine's cap is. Offline, greyed and dimmed like the spine.
 */
export function bandColors(look: SpineLook, off: boolean): { fill: string; ink: string; rule: string | null } {
  const c = off ? offlineHex : (x: string) => x;
  switch (look.style) {
    case 'band':
      // The spine's cap: its accent, dark type.
      return { fill: c(look.acc), ink: c('#111113'), rule: null };
    case 'clamw':
      // A clamshell's printed insert.
      return { fill: c('#faf8f2'), ink: c('#151518'), rule: null };
    case 'clamb':
      return { fill: c(look.acc), ink: c(look.ink), rule: null };
    default:
      return { fill: c(look.bg), ink: c(look.ink), rule: c(look.acc) };
  }
}

function rgba(c: string, a: number): string {
  const [r, g, b] = rgb(c);
  return `rgb(${r} ${g} ${b} / ${a})`;
}

const MARK_A = 'M2 26 L20 2 L27 2 L9 26 Z';
const MARK_B = 'M16 26 L28 10 L33 10 L21 26 Z';
let markA: Path2D | null = null;
let markB: Path2D | null = null;

interface Run {
  text: string;
  /** Of the title's size. */
  size: number;
  weight: number;
  alpha: number;
  /** Margins before and after, in em of this run. */
  before: number;
  after: number;
}

type Stretch = 'normal' | 'condensed' | 'extra-condensed';

/**
 * Canvas sets widths by keyword only (extra-condensed 62.5%, condensed 75%); the
 * CSS asks for 78% and 70% in places, so those are drawn condensed and scaled.
 */
const WIDE78 = 78 / 75;
const NARROW70 = 70 / 75;

function setFont(ctx: CanvasRenderingContext2D, family: string, size: number, weight: number, stretch: Stretch, spacing: number): void {
  ctx.font = `${weight} ${size}px ${family}`;
  ctx.fontStretch = stretch;
  ctx.letterSpacing = `${spacing * size}px`;
}

/**
 * The baseline that centres a line of `size` text on `y` the way a CSS line box
 * does: half the font's ascent less its descent below the middle (DM Mono 0.34,
 * Archivo 0.335, Jost 0.345 of the size).
 */
const centred = (y: number, size: number, mid = 0.34) => y + size * mid;
/** The spine inherits the page's line-height (base.css). */
const LINE = 1.55;

/** Gloss, as alpha: a clamshell's plastic all over, a sticker's paper. */
export const GLOSS_PLASTIC = 0.78;
export const GLOSS_STICKER = 0.8;

/** A tile of paper grain (mid-grey specks), made once; drawn over a spine with `overlay`. */
let grainTile: OffscreenCanvas | HTMLCanvasElement | null = null;

function grainPattern(ctx: CanvasRenderingContext2D, seed: number): CanvasPattern | null {
  if (!grainTile) {
    const N = 64;
    const tile = typeof OffscreenCanvas !== 'undefined' ? new OffscreenCanvas(N, N) : Object.assign(document.createElement('canvas'), { width: N, height: N });
    const g = tile.getContext('2d');
    if (!g) return null;
    const img = g.createImageData(N, N);
    let s = 0x9e3779b9;
    for (let i = 0; i < N * N; i++) {
      s ^= s << 13;
      s ^= s >>> 17;
      s ^= s << 5;
      const v = 128 + ((s >>> 0) % 61) - 30;
      img.data[i * 4] = v;
      img.data[i * 4 + 1] = v;
      img.data[i * 4 + 2] = v;
      img.data[i * 4 + 3] = 255;
    }
    g.putImageData(img, 0, 0);
    grainTile = tile;
  }
  const p = ctx.createPattern(grainTile, 'repeat');
  // Each tape's grain starts somewhere else in the tile.
  p?.setTransform(new DOMMatrix([1, 0, 0, 1, Math.floor(seed * 64), Math.floor(((seed * 7.31) % 1) * 64)]));
  return p;
}

/**
 * Handling: the print on a card sleeve's edges and corners rubs through to the white
 * card, here and there, on a few tapes much more than most (the same marks every time).
 */
function paintWear(ctx: CanvasRenderingContext2D, id: string | number, tw: number, H: number): void {
  const wear = rand01(`${id}:worn`) ** 2;
  if (wear < 0.06) return;
  const r = (k: string) => rand01(`${id}:${k}`);
  const n = Math.round(3 + wear * 22);
  ctx.save();
  ctx.fillStyle = 'rgb(232 228 220)';
  for (let j = 0; j < n; j++) {
    const kind = r(`wk${j}`);
    const at = r(`wa${j}`);
    const len = 1.5 + r(`wl${j}`) * (2.5 + wear * 12);
    ctx.globalAlpha = (0.1 + 0.4 * wear) * (0.45 + 0.55 * r(`wo${j}`));
    if (kind < 0.42) {
      // Along a long edge: a thin rub.
      const x = r(`ws${j}`) < 0.5 ? 0 : tw - 0.7;
      ctx.fillRect(x, at * (H - len), 0.7, len);
    } else if (kind < 0.7) {
      // Along the top or the foot.
      const y = r(`ws${j}`) < 0.55 ? 0 : H - 0.7;
      ctx.fillRect(at * Math.max(0, tw - len), y, Math.min(len, tw), 0.7);
    } else {
      // A corner, rounded off.
      const cx = r(`ws${j}`) < 0.5 ? 0 : tw;
      const cy = at < 0.5 ? 0 : H;
      ctx.beginPath();
      ctx.arc(cx, cy, 0.8 + r(`wl${j}`) * (1 + wear * 2), 0, Math.PI * 2);
      ctx.fill();
    }
  }
  ctx.restore();
}

/** Make what is painted inside `path` glossy: its alpha × `a` (alpha 1 reads as card). */
function glaze(ctx: CanvasRenderingContext2D, path: () => void, a: number): void {
  ctx.save();
  ctx.beginPath();
  path();
  ctx.clip();
  ctx.globalCompositeOperation = 'destination-in';
  ctx.globalAlpha = 1;
  ctx.fillStyle = `rgb(0 0 0 / ${a})`;
  ctx.fillRect(-1e4, -1e4, 2e4, 2e4);
  ctx.restore();
}

/** Paint the spine into a `tw`×`H` (CSS pixels) area at the context's origin. */
export function paintSpine(ctx: CanvasRenderingContext2D, item: ItemSummary, look: SpineLook, s: SpineSpec, dpr: number, fonts: ShelfFonts): void {
  const { tw, H, W } = s;
  const off = !item.available;
  const bg = grayHex(look.bg, off);
  const bg2 = grayHex(look.bg2, off);
  const ink = grayHex(look.ink, off);
  const acc = grayHex(look.acc, off);
  ctx.scale(dpr, dpr);
  ctx.textBaseline = 'alphabetic';
  const clam = look.style === 'clamw' || look.style === 'clamb';
  ctx.beginPath();
  ctx.roundRect(0, 0, tw, H, clam ? 2.5 : 1.5);
  ctx.clip();
  // Body.
  const body = ctx.createLinearGradient(0, 0, 0, H);
  body.addColorStop(0, bg);
  body.addColorStop(1, bg2);
  ctx.fillStyle = body;
  ctx.fillRect(0, 0, tw, H);
  if (clam) {
    // Ribbed plastic.
    ctx.fillStyle = look.style === 'clamw' ? 'rgb(0 0 0 / 0.045)' : 'rgb(255 255 255 / 0.04)';
    for (let y = 0; y < H; y += 4) ctx.fillRect(0, y, tw, 1);
  }
  // Inset edges: light on the left and top, shade on the right.
  ctx.fillStyle = 'rgb(255 255 255 / 0.09)';
  ctx.fillRect(0, 0, 1, H);
  ctx.fillStyle = 'rgb(0 0 0 / 0.42)';
  ctx.fillRect(tw - 1, 0, 1, H);
  ctx.fillStyle = 'rgb(255 255 255 / 0.14)';
  ctx.fillRect(0, 0, tw, 1);

  const mono = fonts.mono;
  const yr = Math.max(6.5, W * 0.235);
  const band = look.style === 'band';
  const capH = H * (band ? 0.2 : 0.115);
  const footH = H * 0.135;
  const titleTop = capH;
  const titleH = H - capH - footH;

  // Cap: the year (a band of the accent colour, or a collector's number), or OFFLINE.
  if (band) {
    ctx.fillStyle = acc;
    ctx.fillRect(0, 0, tw, capH);
  }
  let capInk = ink;
  if (band) capInk = '#111113';
  else if (look.style === 'number' || look.style === 'clamw') capInk = acc;
  // A band's year sits at the bottom of its block (padding-bottom W·0.18).
  const capY = band ? capH - W * 0.18 - (yr * LINE) / 2 : capH / 2;
  ctx.fillStyle = capInk;
  ctx.textAlign = 'center';
  if (off) {
    const size = Math.max(5, W * 0.16);
    setFont(ctx, mono, size, band ? 500 : 400, 'normal', 0.02);
    ctx.globalAlpha = 0.85;
    ctx.fillText('OFFLINE', tw / 2, centred(capY, size));
    ctx.globalAlpha = 1;
  } else if (look.style === 'number') {
    const no = String(collectorNo(item.id));
    setFont(ctx, mono, yr, 400, 'normal', 0.01);
    const numW = ctx.measureText(no).width;
    setFont(ctx, mono, yr * 0.8, 400, 'normal', 0.01);
    const signW = ctx.measureText('№').width + 1;
    const x0 = tw / 2 - (numW + signW) / 2;
    const base = centred(capY, yr);
    ctx.textAlign = 'left';
    ctx.globalAlpha = 0.7;
    ctx.fillText('№', x0, base);
    ctx.globalAlpha = 1;
    setFont(ctx, mono, yr, 400, 'normal', 0.01);
    ctx.fillText(no, x0 + signW, base);
    ctx.textAlign = 'center';
  } else {
    setFont(ctx, mono, yr, band ? 500 : 400, 'normal', 0.01);
    ctx.fillText(item.year ? String(item.year) : '—', tw / 2, centred(capY, yr));
  }
  if (look.style === 'studio' || look.style === 'box') {
    // Inset box shadows on the cap's foot and the foot's head, on whole pixels as Blink paints boxes.
    const capB = Math.round(capH);
    ctx.fillStyle = acc;
    ctx.fillRect(0, capB - 2, tw, 2);
    if (look.style === 'box') ctx.fillRect(0, capB - 6, tw, 1);
    ctx.fillRect(0, Math.round(H - footH), tw, 2);
  }

  // The title, set sideways (read top to bottom), shrunk to the spine; a box set's season count beside it.
  const boxW = clam ? tw * 0.72 : tw;
  const boxX = (tw - boxW) / 2;
  if (clam) {
    ctx.fillStyle = look.style === 'clamw' ? '#faf8f2' : acc;
    ctx.beginPath();
    ctx.roundRect(boxX, titleTop, boxW, titleH, 1);
    ctx.fill();
    ctx.strokeStyle = look.style === 'clamw' ? 'rgb(0 0 0 / 0.14)' : 'rgb(255 255 255 / 0.14)';
    ctx.lineWidth = 0.5;
    ctx.stroke();
  }
  const titleInk = look.style === 'clamw' ? '#151518' : ink;
  const parts = spineTitleParts(item.kind === 'episode' && item.parent_title ? item.parent_title : item.title);
  const numbered = look.style === 'number';
  const stretch: Stretch = numbered ? 'condensed' : 'extra-condensed';
  const sx = numbered ? WIDE78 : 1;
  const weight = numbered ? 600 : 800;
  const spacing = numbered ? 0.08 : 0.012;
  let fs = W * 0.52 * (numbered ? 0.78 : 1);
  const runs: Run[] = [];
  if (parts.article) runs.push({ text: parts.article, size: 0.6, weight: 700, alpha: 0.9, before: 0, after: 0.3 });
  runs.push({ text: parts.main, size: 1, weight, alpha: 1, before: 0, after: 0 });
  if (parts.sub) runs.push({ text: parts.sub, size: 0.56, weight: 700, alpha: 0.85, before: 0.4, after: 0 });
  // Measured once: a run's width (letter-spacing included, in em) scales with the size.
  const fs0 = fs;
  const widths = runs.map((r) => {
    setFont(ctx, fonts.cond, fs0 * r.size, r.weight, stretch, spacing);
    return ctx.measureText(r.text).width * sx;
  });
  const length = (size: number) => runs.reduce((L, r, k) => L + (widths[k] * size) / fs0 + (r.before + r.after) * size * r.size, 0);
  let L = length(fs);
  const room = titleH - 10;
  if (L > room && L > 0 && room > 0) {
    fs = Math.max(6.5, (fs * room) / L);
    L = length(fs);
  }
  const chap = item.kind === 'show' ? (item.child_count ? `${item.child_count} SEASON${item.child_count === 1 ? '' : 'S'}` : 'COMPLETE SERIES') : null;
  const chapSize = Math.max(6, W * 0.2);
  const gap = W * 0.08;
  const chapW = chap ? chapSize * LINE : 0;
  const groupW = fs + (chap ? gap + chapW : 0);
  const titleX = tw / 2 + groupW / 2 - fs / 2;
  const midY = titleTop + titleH / 2;
  ctx.save();
  // The title box clips what doesn't fit (overflow: hidden).
  ctx.beginPath();
  ctx.rect(boxX, titleTop, boxW, titleH);
  ctx.clip();
  ctx.save();
  ctx.translate(titleX, midY);
  ctx.rotate(Math.PI / 2);
  ctx.textAlign = 'left';
  ctx.fillStyle = titleInk;
  let x = -L / 2;
  runs.forEach((r, k) => {
    const size = fs * r.size;
    setFont(ctx, fonts.cond, size, r.weight, stretch, spacing);
    x += r.before * size;
    ctx.globalAlpha = r.alpha;
    if (sx === 1) ctx.fillText(r.text, x, centred(0, size, 0.335));
    else {
      ctx.save();
      ctx.translate(x, 0);
      ctx.scale(sx, 1);
      ctx.fillText(r.text, 0, centred(0, size, 0.335));
      ctx.restore();
    }
    x += (widths[k] * fs) / fs0 + r.after * size;
  });
  ctx.restore();
  if (chap) {
    ctx.translate(titleX - fs / 2 - gap - chapW / 2, midY);
    ctx.rotate(Math.PI / 2);
    setFont(ctx, mono, chapSize, 400, 'normal', 0.08);
    ctx.textAlign = 'center';
    ctx.globalAlpha = 0.7;
    ctx.fillStyle = titleInk;
    ctx.fillText(chap, 0, centred(0, chapSize));
  }
  ctx.restore();
  ctx.globalAlpha = 1;

  // Foot: the flux mark over VIDEO.
  const markW = W * 0.5;
  const markH = (markW * 28) / 35;
  const lblSize = Math.max(5, W * 0.15);
  const lblH = lblSize * LINE;
  const footY = H - footH + (footH - (markH + 2 + lblH)) / 2;
  markA ??= new Path2D(MARK_A);
  markB ??= new Path2D(MARK_B);
  ctx.save();
  ctx.translate(tw / 2 - markW / 2, footY);
  ctx.scale(markW / 35, markH / 28);
  ctx.fillStyle = ink;
  ctx.globalAlpha = 0.78;
  ctx.fill(markA);
  ctx.globalAlpha = 0.78 * 0.45;
  ctx.fill(markB);
  ctx.restore();
  setFont(ctx, mono, lblSize, 400, 'normal', 0.14);
  ctx.textAlign = 'center';
  ctx.globalAlpha = 0.55;
  ctx.fillStyle = ink;
  ctx.fillText('VIDEO', tw / 2, centred(footY + markH + 2 + lblH / 2, lblSize));
  ctx.globalAlpha = 1;

  // The light down the box (the shelf's light is above it), as the DOM Shelf paints it;
  // its highlights are the renderer's now (they move with the light), so the painted
  // streak down the box is only a breath.
  const sheenV = ctx.createLinearGradient(0, 0, 0, H);
  sheenV.addColorStop(0, 'rgb(255 255 255 / 0.1)');
  sheenV.addColorStop(0.26, 'rgb(255 255 255 / 0)');
  sheenV.addColorStop(0.68, 'rgb(0 0 0 / 0)');
  sheenV.addColorStop(1, 'rgb(0 0 0 / 0.3)');
  ctx.fillStyle = sheenV;
  ctx.fillRect(0, 0, tw, H);
  const sheenH = ctx.createLinearGradient(0, 0, tw, 0);
  sheenH.addColorStop(0.06, 'rgb(255 255 255 / 0)');
  sheenH.addColorStop(0.2, 'rgb(255 255 255 / 0.04)');
  sheenH.addColorStop(0.34, 'rgb(255 255 255 / 0)');
  sheenH.addColorStop(0.76, 'rgb(0 0 0 / 0)');
  sheenH.addColorStop(1, 'rgb(0 0 0 / 0.2)');
  ctx.fillStyle = sheenH;
  ctx.fillRect(0, 0, tw, H);

  // The print's paper grain (under a clamshell's plastic, hardly any), and on a card
  // sleeve, wear at its edges. (Before the stickers, which are smooth, glossy paper.)
  const grain = grainPattern(ctx, rand01(`${item.id}:grain`));
  if (grain) {
    ctx.save();
    ctx.globalCompositeOperation = 'overlay';
    ctx.globalAlpha = clam ? 0.08 : 0.2;
    ctx.fillStyle = grain;
    ctx.fillRect(0, 0, tw, H);
    ctx.restore();
  }
  if (!clam) paintWear(ctx, item.id, tw, H);

  // Stickers: NEW on recent additions; now and then a price or a plea to rewind.
  const r = (k: string) => rand01(`${item.id}:${k}`);
  const sticker = (o: { top: number; w: number | null; rot: number; bg: string; fg: string; lines: string[]; family: string; size: number; weight: number; stretch: Stretch; sx?: number; spacing: number; lineH: number; padTop: number; padBottom: number; padX: number; radius: number }) => {
    setFont(ctx, o.family, o.size, o.weight, o.stretch, o.spacing);
    const ssx = o.sx ?? 1;
    const textW = Math.max(...o.lines.map((l) => ctx.measureText(l).width)) * ssx;
    const bw = o.w ?? textW + o.padX * 2;
    const bh = o.lines.length * o.size * o.lineH + o.padTop + o.padBottom;
    ctx.save();
    ctx.translate(tw / 2, o.top + bh / 2);
    ctx.rotate((o.rot * Math.PI) / 180);
    ctx.shadowColor = 'rgb(0 0 0 / 0.38)';
    ctx.shadowBlur = 1.5 * dpr;
    ctx.shadowOffsetY = dpr;
    ctx.fillStyle = grayHex(o.bg, off);
    ctx.beginPath();
    ctx.roundRect(-bw / 2, -bh / 2, bw, bh, o.radius);
    ctx.fill();
    ctx.shadowColor = 'transparent';
    ctx.fillStyle = grayHex(o.fg, off);
    ctx.textAlign = 'center';
    const mid = o.family === mono ? 0.34 : 0.335;
    ctx.save();
    ctx.scale(ssx, 1);
    o.lines.forEach((l, k) => ctx.fillText(l, 0, centred(-bh / 2 + o.padTop + o.size * o.lineH * (k + 0.5), o.size, mid)));
    ctx.restore();
    // A sticker is glossy paper (its shadow on the box is not); a clamshell is glossy anyway.
    if (!clam) glaze(ctx, () => ctx.roundRect(-bw / 2, -bh / 2, bw, bh, o.radius), GLOSS_STICKER);
    ctx.restore();
  };
  if (isNew(item)) {
    sticker({ top: H * 0.13, w: null, rot: r('nr') * 10 - 6, bg: '#ffd84a', fg: '#141414', lines: ['NEW'], family: fonts.cond, size: Math.max(7, W * 0.24), weight: 800, stretch: 'condensed', spacing: 0.04, lineH: 1, padTop: 2, padBottom: 1, padX: 3, radius: 2 });
  }
  // Price and rewind stickers only where the title leaves room for them.
  const short = item.title.replace(/^(the|a|an)\s+/i, '').length <= 13;
  if (short && item.available) {
    if (r('price') < 0.12) {
      const size = Math.max(6, W * 0.2);
      const bh = size * 1.1 + 3;
      const price = ['$2.99', '$1.99', '99¢', '$3.49'][Math.floor(r('pv') * 4)];
      sticker({ top: H - H * 0.15 - bh, w: W * 0.86, rot: r('pr') * 8 - 4, bg: '#fbfaf6', fg: '#c6261c', lines: [price], family: mono, size, weight: 500, stretch: 'normal', spacing: 0, lineH: 1.1, padTop: 2, padBottom: 1, padX: 0, radius: 1.5 });
    } else if (r('rewind') < 0.1) {
      sticker({ top: H * 0.71, w: W * 0.92, rot: r('rr') * 10 - 5, bg: '#ffffff', fg: '#cf261b', lines: ['BE KIND', 'REWIND'], family: fonts.cond, size: Math.max(5, W * 0.15), weight: 800, stretch: 'condensed', sx: NARROW70, spacing: 0, lineH: 1.05, padTop: 2, padBottom: 2, padX: 0, radius: 1.5 });
    }
  }

  // How far you got: a thin bright stripe down the right edge.
  const prog = progressFraction(item);
  if (prog !== null) {
    ctx.save();
    ctx.globalAlpha = 0.9;
    ctx.shadowColor = rgba(ink, 0.6);
    ctx.shadowBlur = 6 * dpr;
    ctx.fillStyle = ink;
    ctx.beginPath();
    ctx.roundRect(tw - 1.5 - 2, 0, 2, prog * H, [0, 0, 2, 2]);
    ctx.fill();
    ctx.restore();
  }

  // A clamshell is glossy plastic all over (stickers on it no glossier).
  if (clam) glaze(ctx, () => ctx.rect(0, 0, tw, H), GLOSS_PLASTIC);
}

/** The back cover's words: title, a line, and the year, kind and badges. */
export function backLines(item: ItemSummary): { title: string; line: string; meta: string } {
  const meta = [item.year ? String(item.year) : '', item.kind === 'show' ? 'SERIES' : item.kind === 'movie' ? 'FEATURE' : '', ...item.badges.slice(0, 3)].filter(Boolean).join(' · ');
  return { title: item.title, line: item.parent_title && item.kind !== 'show' ? item.parent_title : 'flux video', meta };
}

export function backKey(item: ItemSummary, look: SpineLook, D: number, H: number, dpr: number, fontGen = 0): string {
  const b = backLines(item);
  return ['b', item.id, b.title, b.line, b.meta, look.ink, D, H, dpr, fontGen].join('|');
}

function wrapWords(ctx: CanvasRenderingContext2D, text: string, max: number): string[] {
  const words = text.split(/\s+/).filter(Boolean);
  const out: string[] = [];
  let cur = '';
  for (const w of words) {
    const next = cur ? `${cur} ${w}` : w;
    if (!cur || ctx.measureText(next).width <= max) cur = next;
    else {
      out.push(cur);
      cur = w;
    }
  }
  if (cur) out.push(cur);
  return out;
}

/**
 * The back cover's text (no background: the renderer draws the face's gradient
 * under it), over a whole D×H face squeezed into `w`×`h` device pixels; the face
 * is only ever seen from the side, so it needs less width than it has.
 */
export function paintBack(ctx: CanvasRenderingContext2D, item: ItemSummary, look: SpineLook, D: number, H: number, w: number, h: number, fonts: ShelfFonts): void {
  ctx.scale(w / D, h / H);
  ctx.beginPath();
  ctx.rect(0, 0, D, H);
  ctx.clip();
  const { title, line, meta } = backLines(item);
  const fs = D * 0.05;
  const left = D * 0.1;
  const max = D * 0.8;
  let y = H * 0.08;
  ctx.fillStyle = look.ink;
  ctx.textAlign = 'left';
  ctx.textBaseline = 'alphabetic';
  // The title: condensed caps, line-height 1.
  const ts = fs * 1.7;
  setFont(ctx, fonts.cond, ts, 800, 'condensed', 0);
  ctx.globalAlpha = 0.78;
  for (const l of wrapWords(ctx, title.toUpperCase(), max / NARROW70)) {
    ctx.save();
    ctx.translate(left, 0);
    ctx.scale(NARROW70, 1);
    ctx.fillText(l, 0, centred(y + ts / 2, ts, 0.335));
    ctx.restore();
    y += ts;
  }
  y += ts * 0.6;
  // A line in the body face, then the details in mono (line-height 1.35).
  setFont(ctx, fonts.body, fs, 400, 'normal', 0);
  for (const l of wrapWords(ctx, line, max)) {
    ctx.fillText(l, left, centred(y + (fs * 1.35) / 2, fs, 0.345));
    y += fs * 1.35;
  }
  y += fs * 0.9;
  if (meta) {
    const ms = fs * 0.85;
    setFont(ctx, fonts.mono, ms, 400, 'normal', 0);
    ctx.globalAlpha = 0.78 * 0.7;
    for (const l of wrapWords(ctx, meta, max)) {
      ctx.fillText(l, left, centred(y + (fs * 1.35) / 2, ms));
      y += fs * 1.35;
    }
  }
  ctx.globalAlpha = 1;
}

// ── fittings: gradients the renderer stretches over a quad ──

/** Colour stops along x (`h`) or y (`v`) of the painted area. */
export function paintRamp(ctx: CanvasRenderingContext2D, w: number, h: number, dir: 'h' | 'v', stops: readonly (readonly [number, string])[]): void {
  const g = dir === 'h' ? ctx.createLinearGradient(0, 0, w, 0) : ctx.createLinearGradient(0, 0, 0, h);
  for (const [at, c] of stops) g.addColorStop(Math.max(0, Math.min(1, at)), c);
  ctx.fillStyle = g;
  ctx.fillRect(0, 0, w, h);
}

/**
 * A lit edge `lead` device pixels deep in `hi`, then a ramp from `a` to `b` over
 * `size` device pixels; the rest of the cell is `b`, so sampling its border doesn't bleed.
 */
export function paintEdge(ctx: CanvasRenderingContext2D, cellW: number, cellH: number, dir: 'h' | 'v', size: number, lead: number, hi: string, a: string, b: string): void {
  const len = Math.max(1, size);
  const f = Math.min(1, lead / len);
  const stops: [number, string][] = [
    [0, hi],
    [f, hi],
    [f, a],
    [1, b],
  ];
  ctx.fillStyle = b;
  ctx.fillRect(0, 0, cellW, cellH);
  if (dir === 'v') paintRamp(ctx, cellW, len, 'v', stops);
  else paintRamp(ctx, len, cellH, 'h', stops);
}

/** `radial-gradient(56% 100% at 50% 0%, light, transparent 74%)` over the whole area. */
export function paintGlow(ctx: CanvasRenderingContext2D, w: number, h: number, light: string): void {
  ctx.setTransform(w * 0.56, 0, 0, h, w / 2, 0);
  const g = ctx.createRadialGradient(0, 0, 0, 0, 0, 1);
  g.addColorStop(0, light);
  g.addColorStop(0.74, 'rgb(0 0 0 / 0)');
  ctx.fillStyle = g;
  ctx.fillRect(-1 / 0.56, 0, 2 / 0.56, 1);
  ctx.setTransform(1, 0, 0, 1, 0, 0);
}

/**
 * The light that crosses a turned sleeve, `linear-gradient(115deg, transparent 28%,
 * rgb(255 255 255 / .13) 44%, transparent 58%)` on a D×H face, over the whole area.
 */
export function paintSheen(ctx: CanvasRenderingContext2D, w: number, h: number, D: number, H: number): void {
  ctx.scale(w / D, h / H);
  const a = (115 * Math.PI) / 180;
  const dx = Math.sin(a);
  const dy = -Math.cos(a);
  const len = Math.abs(D * dx) + Math.abs(H * dy);
  const cx = D / 2;
  const cy = H / 2;
  const g = ctx.createLinearGradient(cx - (dx * len) / 2, cy - (dy * len) / 2, cx + (dx * len) / 2, cy + (dy * len) / 2);
  g.addColorStop(0.28, 'rgb(255 255 255 / 0)');
  g.addColorStop(0.44, 'rgb(255 255 255 / 0.13)');
  g.addColorStop(0.58, 'rgb(255 255 255 / 0)');
  ctx.fillStyle = g;
  ctx.fillRect(0, 0, D, H);
}
