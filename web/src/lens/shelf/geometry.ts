/**
 * The Shelf as numbers, shared by the DOM Shelf (shelf.ts) and the GPU Shelf
 * (gpu-shelf.ts): how big a tape is on this screen, which spine treatment each
 * title gets, and where every tape goes on the shelves. Pure functions.
 */
import type { ItemSummary } from '../../api/types';
import { clamp, rand01 } from '../dom';
import { deepen, luminance, mixHex, type Palette } from '../palette';

// ── spine looks ──

export type SpineStyle = 'studio' | 'band' | 'number' | 'clamw' | 'clamb' | 'box';

export interface SpineLook {
  style: SpineStyle;
  bg: string;
  bg2: string;
  ink: string;
  acc: string;
  top: string;
}

/** Lift a colour until it is at least this bright (accents on dark spines). */
function lift(c: string, min: number): string {
  let x = c;
  for (let k = 0; k < 12 && luminance(x) < min; k++) x = mixHex(x, '#ffffff', 0.16);
  return x;
}

/** Each tape gets one of a handful of real-world spine treatments, chosen from its id. */
export function spineLook(item: Pick<ItemSummary, 'id' | 'kind' | 'year'>, p: Palette): SpineLook {
  const r = rand01(`${item.id}:spine`);
  const acc = p.c3 || p.c2 || p.c1;
  // Black-and-white art (a grey palette) from before colour TV may come in a clamshell.
  const mono = p.sat < 0.12;
  let style: SpineStyle;
  if (item.kind === 'show') style = 'box';
  else if (mono && (item.year ?? 2000) < 1965 && r < 0.4) style = r < 0.17 ? 'clamw' : 'clamb';
  else if (r < 0.66) style = 'studio';
  else if (r < 0.86) style = 'band';
  else style = 'number';
  let bg: string;
  let bg2: string;
  let ink: string;
  let accent: string;
  if (style === 'studio' || style === 'box') {
    bg = deepen(p.c1, 0.045);
    bg2 = mixHex(bg, '#000000', 0.5);
    ink = mixHex(lift(acc, 0.45), '#fbf7ee', 0.6);
    accent = lift(acc, 0.2);
  } else if (style === 'band') {
    bg = '#131315';
    bg2 = '#09090a';
    accent = lift(p.c1, 0.3);
    ink = accent;
  } else if (style === 'number') {
    bg = '#101012';
    bg2 = '#08080a';
    ink = '#efece4';
    accent = lift(acc, 0.32);
  } else if (style === 'clamw') {
    bg = '#e7e4dd';
    bg2 = '#cfcbc2';
    ink = '#151518';
    accent = deepen(p.c1, 0.08);
  } else {
    bg = '#1c1c1f';
    bg2 = '#0f0f11';
    ink = '#f3f0e8';
    accent = deepen(lift(p.c1, 0.12), 0.07);
  }
  const top = style === 'clamw' ? '#f2f0ea' : mixHex(bg, '#ffffff', 0.14);
  return { style, bg, bg2, ink, acc: accent, top };
}

/** "The General" → a small "THE" and a big "GENERAL"; a subtitle after a colon is set smaller. */
export function spineTitleParts(title: string): { article: string | null; main: string; sub: string | null } {
  let t = title.toUpperCase().trim();
  let article: string | null = null;
  const a = /^(THE|A|AN)\s+/.exec(t);
  if (a && t.length > a[0].length) {
    article = a[1];
    t = t.slice(a[0].length);
  }
  const colon = t.indexOf(':');
  if (colon > 0 && colon < t.length - 1) return { article, main: t.slice(0, colon), sub: t.slice(colon + 1).trim() };
  return { article, main: t, sub: null };
}

// ── sizes ──

export interface Metrics {
  vw: number;
  vh: number;
  phone: boolean;
  top: number;
  availH: number;
  rows: number;
  /** A tape: height, spine width, depth (a VHS sleeve's proportions). */
  H: number;
  W: number;
  D: number;
  /** Shelf board thickness, the room above each row, a box set's spine. */
  bh: number;
  head: number;
  boxW: number;
  gap: number;
  secGap: number;
  padX: number;
  fullW: number;
  bayW: number;
  bayGap: number;
  /** How far a board runs past its tapes, bookend width, the gap before a bookend. */
  over: number;
  ew: number;
  endGap: number;
  /** The board's front lip and depth. */
  lip: number;
  bd: number;
  /** CSS perspective of the scene. */
  persp: number;
}

export interface MetricsInput {
  vw: number;
  vh: number;
  safeTop: number;
  safeBottom: number;
  mini: boolean;
  /** Shrinks the tapes a touch when the shelf nearly fits on one bay. */
  scale?: number;
}

/** How big everything is on this screen (the DOM Shelf turns these into CSS variables). */
export function shelfMetrics(o: MetricsInput): Metrics {
  const { vw, vh, mini } = o;
  const scale = o.scale ?? 1;
  const phone = vw <= 640;
  const top = mini ? 14 : o.safeTop + 12;
  const bottom = mini ? 18 : o.safeBottom + 20;
  const availH = Math.max(120, vh - top - bottom);
  const bh = mini ? 7 : clamp(Math.round(vh * 0.016), 9, 15);
  const headK = mini ? 0.1 : 0.17;
  let rows: number;
  let H: number;
  if (mini) {
    rows = 1;
    H = clamp(availH / (1 + headK) - bh, 80, 230);
  } else {
    rows = availH >= 1020 ? 3 : availH >= 470 ? 2 : 1;
    H = clamp((availH - rows * bh) / (rows * (1 + headK)), 130, phone ? 250 : 300);
  }
  H = Math.round(H * scale);
  // A VHS sleeve: 187 mm tall, 103 mm deep, 25 mm thick.
  const W = Math.round((H * 25) / 187);
  const D = Math.round((H * 103) / 187);
  const padX = mini ? 18 : clamp(Math.round(vw * 0.045), 18, 72);
  const peek = mini ? 0 : phone ? 30 : 56;
  return {
    vw,
    vh,
    phone,
    top,
    availH,
    rows,
    H,
    W,
    D,
    bh,
    head: Math.round(H * headK),
    boxW: Math.round(W * 1.85),
    gap: Math.max(1, Math.round(W * 0.05)),
    secGap: Math.round(W * 0.5),
    padX,
    fullW: vw - 2 * padX,
    bayW: Math.max(W * 4, vw - 2 * padX - peek),
    bayGap: mini ? 24 : Math.round(W * 2.4),
    over: Math.round(W * 0.95),
    ew: Math.max(4, Math.round(W * 0.16)),
    endGap: 1.5,
    lip: Math.max(4, Math.round(W * 0.3)),
    bd: D + Math.round(W * 0.7),
    persp: mini ? 760 : phone ? 1100 : 1400,
  };
}

// ── the plan: where every tape goes ──

export interface ShelfPlanInput {
  widths: ArrayLike<number>;
  groups: readonly string[];
  rows: number;
  /** Shelf length that fits on screen. */
  fullW: number;
  /** Length of one bay when the shelf is longer than the screen. */
  bayW: number;
  W: number;
  gap: number;
  secGap: number;
}

export interface ShelfPlan {
  n: number;
  bay: Int32Array;
  row: Int32Array;
  /** x within its bay. */
  x: Float64Array;
  tags: { bay: number; row: number; x: number; label: string }[];
  rows: number;
  bays: number;
  /** Longest row of each bay. */
  bayLen: number[];
  /** Length of each row of each bay (undefined: empty row). */
  rowLen: (number | undefined)[][];
  oneBay: boolean;
  /** Length of everything on one line. */
  total: number;
  /** First tape of each bay, then n. */
  bayStart: number[];
}

/** Shelves fill left to right, then the next shelf down, then the next bay; a new section leaves a gap and gets a tag. */
export function planShelf(inp: ShelfPlanInput): ShelfPlan {
  const n = inp.widths.length;
  const { groups, gap, secGap } = inp;
  const newSection = (i: number) => i > 0 && groups[i] !== '' && groups[i] !== groups[i - 1];
  let total = 0;
  for (let i = 0; i < n; i++) total += inp.widths[i] + (i ? gap + (newSection(i) ? secGap : 0) : 0);
  let rows = inp.rows;
  const oneBay = total <= rows * inp.fullW * 0.97;
  if (oneBay && total < inp.fullW * 0.5) rows = 1;
  const target = oneBay ? Math.min(inp.fullW, total / rows + inp.W * 0.6) : inp.bayW;
  const bay = new Int32Array(n);
  const row = new Int32Array(n);
  const x = new Float64Array(n);
  const tags: ShelfPlan['tags'] = [];
  const rowLen: (number | undefined)[][] = [];
  const bayStart: number[] = [0];
  let b = 0;
  let r = 0;
  let cx = 0;
  for (let i = 0; i < n; i++) {
    const w = inp.widths[i];
    let lead = cx > 0 ? gap + (newSection(i) ? secGap : 0) : 0;
    if (cx > 0 && cx + lead + w > target) {
      r++;
      cx = 0;
      lead = 0;
      if (r >= rows) {
        b++;
        r = 0;
        bayStart.push(i);
      }
    }
    cx += lead;
    bay[i] = b;
    row[i] = r;
    x[i] = cx;
    if (groups[i] && (i === 0 || groups[i] !== groups[i - 1])) tags.push({ bay: b, row: r, x: cx, label: groups[i] });
    cx += w;
    (rowLen[b] ??= [])[r] = cx;
  }
  const bays = Math.max(1, rowLen.length);
  while (bayStart.length < bays) bayStart.push(n);
  bayStart.push(n);
  const bayLen = Array.from({ length: bays }, (_, k) => Math.max(0, ...(rowLen[k] ?? []).filter((v): v is number => v !== undefined)));
  return { n, bay, row, x, tags, rows, bays, bayLen, rowLen, oneBay, total, bayStart };
}

/** Where the bays start and end on screen, and how far the shelf scrolls. */
export interface ShelfLayout {
  pitch: number;
  top0: number;
  stride: number;
  x0: number;
  maxX: number;
}

export function shelfLayout(m: Metrics, p: ShelfPlan): ShelfLayout {
  const pitch = m.H + m.head + m.bh;
  const totalH = p.rows * pitch;
  const top0 = m.top + Math.max(0, (m.availH - totalH) / 2);
  const stride = (p.oneBay ? m.fullW : m.bayW) + m.bayGap;
  const contentW = p.oneBay ? p.bayLen[0] : (p.bays - 1) * stride + p.bayLen[p.bays - 1];
  const x0 = contentW <= m.fullW ? (m.vw - contentW) / 2 : m.padX;
  const maxX = Math.max(0, x0 + contentW + m.padX - m.vw);
  return { pitch, top0, stride, x0, maxX };
}
