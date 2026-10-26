/**
 * The Wall's layout as numbers: rows of section headers, tiles and placeholders,
 * and each title's position. Pure, so it is cheap to recompute on every keystroke
 * and easy to test; the Wall renders only the rows on screen from it.
 */

/** The Wall's four densities, from a mosaic of decades to posters with captions. */
export interface Level {
  name: string;
  /** Target tile width. */
  w: number;
  gap: number;
  /** Caption height under the poster. */
  cap: number;
  /** Section header height (0 = labels sit in the margin). */
  head: number;
}

export const LEVELS: readonly Level[] = [
  { name: 'Overview', w: 62, gap: 3, cap: 0, head: 0 },
  { name: 'Grid', w: 112, gap: 8, cap: 0, head: 50 },
  { name: 'Posters', w: 180, gap: 16, cap: 0, head: 64 },
  { name: 'Details', w: 300, gap: 28, cap: 58, head: 92 },
];

export interface Row {
  kind: 'head' | 'items' | 'skel';
  y: number;
  h: number;
  /** items: [start, end) into the titles; skel: [start, end) of virtual slots after them. */
  start: number;
  end: number;
  section: string;
  count: number;
}

export interface WallLayout {
  rows: Row[];
  px: Float64Array;
  py: Float64Array;
  rowOf: Int32Array;
  cols: number;
  tileW: number;
  tileH: number;
  /** Content height, including the bottom inset. */
  height: number;
}

export interface LayoutInput {
  /** Titles loaded so far. */
  count: number;
  /** Titles in the space (the rest show as placeholders). */
  total: number;
  sectionAt: (i: number) => string;
  /** Usable width between the margins. */
  inner: number;
  padL: number;
  /** Target tile width and gap. */
  w: number;
  gap: number;
  /** Caption height under each poster. */
  cap: number;
  /** Section header height; 0 = labels sit in the margin (a small spacer between sections). */
  head: number;
  top: number;
  bottom: number;
}

export function layoutWall(o: LayoutInput): WallLayout {
  const cols = Math.max(1, Math.round((o.inner + o.gap) / (o.w + o.gap)));
  const tileW = (o.inner - o.gap * (cols - 1)) / cols;
  const tileH = tileW * 1.5 + o.cap;
  const n = o.count;
  const px = new Float64Array(n);
  const py = new Float64Array(n);
  const rowOf = new Int32Array(n);
  const rows: Row[] = [];
  let y = o.top;
  let i = 0;
  while (i < n) {
    const section = o.sectionAt(i);
    let end = i + 1;
    while (end < n && o.sectionAt(end) === section) end++;
    if (section) {
      const h = o.head > 0 ? o.head : rows.length ? 12 : 0;
      rows.push({ kind: 'head', y, h, start: i, end, section, count: end - i });
      y += h;
    }
    for (let s = i; s < end; s += cols) {
      const e = Math.min(end, s + cols);
      rows.push({ kind: 'items', y, h: tileH + o.gap, start: s, end: e, section, count: e - s });
      for (let k = s; k < e; k++) {
        px[k] = o.padL + (k - s) * (tileW + o.gap);
        py[k] = y;
        rowOf[k] = rows.length - 1;
      }
      y += tileH + o.gap;
    }
    i = end;
  }
  const pending = Math.max(0, o.total - n);
  for (let s = 0; s < pending; s += cols) {
    const e = Math.min(pending, s + cols);
    rows.push({ kind: 'skel', y, h: tileH + o.gap, start: s, end: e, section: '', count: e - s });
    y += tileH + o.gap;
  }
  return { rows, px, py, rowOf, cols, tileW, tileH, height: y + o.bottom };
}

/** First row whose bottom is at or below `y` (rows are sorted by y). */
export function rowAt(rows: readonly Row[], y: number): number {
  let lo = 0;
  let hi = rows.length - 1;
  let ans = rows.length;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    const r = rows[mid];
    if (r.y + r.h >= y) {
      ans = mid;
      hi = mid - 1;
    } else lo = mid + 1;
  }
  return ans;
}
