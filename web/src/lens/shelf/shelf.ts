/**
 * Shelf: your library as VHS tapes on wall shelves, seen spine-on, the way a video
 * shop kept them. Point at a tape and it slides out; click and it turns to show
 * its sleeve, which opens as the detail. Shelves fill left to right in bays that
 * scroll sideways.
 *
 * Windowed: the whole shelf is planned (positions are just numbers), but only the
 * tapes in and near the visible bays are in the DOM, so a 13,000-title library
 * scrolls as smoothly as fifty tapes. Spines are painted from each title's
 * palette (its blurhash), so no poster is downloaded until a tape is pulled out.
 */
import type { Id, ItemSummary } from '../../api/types';
import { artElement, artLabel, loadArt } from '../art';
import { activeLens } from '../bridge';
import { clamp, el, rand01, reducedMotion } from '../dom';
import { deepen, luminance, mixHex, paletteOf, type Palette } from '../palette';
import { isNew, progressFraction } from '../sections';
import type { Dir, LensContext, LensModule, LensView } from '../types';
import styles from './shelf.module.css';

const SOFT = 'cubic-bezier(.25,.85,.3,1)';
const MINI_COUNT = 14;
const MARK =
  '<svg viewBox="0 0 35 28" aria-hidden="true"><path d="M2 26 L20 2 L27 2 L9 26 Z" fill="currentColor"/><path d="M16 26 L28 10 L33 10 L21 26 Z" fill="currentColor" fill-opacity=".45"/></svg>';
const CONDENSED = '800 20px "Archivo Variable"';

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

// ── the lens ──

interface Metrics {
  vw: number;
  vh: number;
  phone: boolean;
  top: number;
  availH: number;
  rows: number;
  H: number;
  W: number;
  D: number;
  bh: number;
  head: number;
  boxW: number;
  gap: number;
  secGap: number;
  padX: number;
  fullW: number;
  bayW: number;
  bayGap: number;
  over: number;
  ew: number;
  endGap: number;
  lip: number;
  bd: number;
}

interface Tape {
  id: Id;
  item: ItemSummary;
  slot: HTMLDivElement;
  spine: HTMLDivElement;
  sleeve: HTMLDivElement;
  tbox: HTMLDivElement;
  tspan: HTMLSpanElement;
  /** The transform it was last placed with. */
  t: string;
}

class ShelfView implements LensView {
  private root: HTMLElement;
  private ctx: LensContext;
  private mini: boolean;
  private scene: HTMLDivElement;
  private world: HTMLDivElement;
  private items: readonly ItemSummary[] = [];
  /** Section of each item, for the items array it was computed for. */
  private groups: string[] = [];
  private groupsFor: readonly ItemSummary[] | null = null;
  private index = new Map<Id, number>();
  private m!: Metrics;
  private p!: ShelfPlan;
  private X = new Float64Array(0);
  private Y = new Float64Array(0);
  private widths = new Float64Array(0);
  private x0 = 0;
  private stride = 1;
  private pitch = 1;
  private top0 = 0;
  private tapes = new Map<Id, Tape>();
  private extras = new Map<string, HTMLElement>();
  private scrollX = 0;
  private targetX = 0;
  private maxX = 0;
  /** scrollX the window was last drawn for. */
  private drawnAt = Number.NaN;
  private focusId: Id | null = null;
  private openId: Id | null = null;
  private raf = 0;
  private hoverT: ReturnType<typeof setTimeout> | null = null;
  private timers: ReturnType<typeof setTimeout>[] = [];
  private suppressClick = false;
  private dead = false;
  private ro: ResizeObserver;
  private cleanup: (() => void)[] = [];

  constructor(root: HTMLElement, ctx: LensContext) {
    this.root = root;
    this.ctx = ctx;
    this.mini = ctx.env.mini;
    root.classList.add(styles.shelf);
    if (this.mini) root.classList.add(styles.mini);
    this.scene = el('div', styles.scene);
    this.world = el('div', styles.world, { role: this.mini ? null : 'group', 'aria-label': this.mini ? null : 'Shelf' });
    this.scene.append(this.world);
    root.append(this.scene);
    this.setItems(ctx.items);
    this.relayout(false);
    ctx.setAccessory(null);
    if (!this.mini) {
      this.bind();
      // The shelf scrolls sideways: nothing to tuck the bars away for.
      ctx.scrolled('top');
    }
    this.ro = new ResizeObserver(() => this.relayout(false));
    this.ro.observe(root);
    // Spine titles are set in condensed Archivo; size them again once the face is in.
    const fonts = document.fonts as FontFaceSet | undefined;
    if (fonts) {
      fonts.load(CONDENSED).then(
        () => {
          if (!this.dead) this.fitTitles();
        },
        () => undefined,
      );
    }
    // Arriving from another lens (or back from detail): keep your place on the shelf.
    if (!this.mini && ctx.focus !== null && this.index.has(ctx.focus)) this.reveal(ctx.focus);
  }

  private setItems(items: readonly ItemSummary[]): void {
    this.items = this.mini ? items.slice(0, MINI_COUNT) : items;
    this.index = new Map(this.items.map((it, i) => [it.id, i]));
  }

  // ── measurements ──

  private metrics(scale = 1): Metrics {
    const vw = this.root.clientWidth || window.innerWidth;
    const vh = this.root.clientHeight || window.innerHeight;
    const safe = this.ctx.safe;
    const phone = vw <= 640;
    const top = this.mini ? 14 : safe.top + 12;
    const bottom = this.mini ? 18 : safe.bottom + 20;
    const availH = Math.max(120, vh - top - bottom);
    const bh = this.mini ? 7 : clamp(Math.round(vh * 0.016), 9, 15);
    const headK = this.mini ? 0.1 : 0.17;
    let rows: number;
    let H: number;
    if (this.mini) {
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
    const padX = this.mini ? 18 : clamp(Math.round(vw * 0.045), 18, 72);
    const peek = this.mini ? 0 : phone ? 30 : 56;
    const m: Metrics = {
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
      bayGap: this.mini ? 24 : Math.round(W * 2.4),
      over: Math.round(W * 0.95),
      ew: Math.max(4, Math.round(W * 0.16)),
      endGap: 1.5,
      lip: Math.max(4, Math.round(W * 0.3)),
      bd: D + Math.round(W * 0.7),
    };
    const s = this.world.style;
    s.setProperty('--h', `${H}px`);
    s.setProperty('--w', `${W}px`);
    s.setProperty('--d', `${D}px`);
    s.setProperty('--bh', `${bh}px`);
    s.setProperty('--lip', `${m.lip}px`);
    s.setProperty('--bd', `${m.bd}px`);
    s.setProperty('--pull', `${Math.round(W * 0.9)}px`);
    s.setProperty('--out', `${Math.round(D / 2 + W / 2 + 28)}px`);
    s.setProperty('--lift', `${Math.round(-H * 0.05)}px`);
    s.setProperty('--hov-y', `${Math.round(-H * 0.055)}px`);
    s.setProperty('--fs0', `${(W * 0.52).toFixed(1)}px`);
    s.setProperty('--ew', `${m.ew}px`);
    s.setProperty('--yr', `${Math.max(6.5, W * 0.235).toFixed(1)}px`);
    this.scene.style.setProperty('--persp', `${this.mini ? 760 : phone ? 1100 : 1400}px`);
    return m;
  }

  /** Section labels; when a page of a loading library was appended, only the new titles are labelled. */
  private sections(): string[] {
    const items = this.items;
    if (this.groupsFor === items) return this.groups;
    const prev = this.groupsFor;
    let keep = 0;
    if (prev && !this.mini && prev.length > 0 && prev.length <= items.length) {
      // The same titles in the same order up front (sampled): reuse their labels.
      const k = prev.length;
      const same = [0, k >> 3, k >> 2, k >> 1, (3 * k) >> 2, k - 1].every((i) => items[i] === prev[i]);
      if (same) keep = k;
    }
    const groups = this.groups.slice(0, keep);
    for (let i = keep; i < items.length; i++) groups.push(this.mini ? '' : this.ctx.sectionOf(items[i]));
    this.groups = groups;
    this.groupsFor = items;
    return groups;
  }

  private plan(m: Metrics): ShelfPlan {
    this.sections();
    const n = this.items.length;
    if (this.widths.length !== n) this.widths = new Float64Array(n);
    for (let i = 0; i < n; i++) this.widths[i] = this.items[i].kind === 'show' ? m.boxW : m.W;
    return planShelf({ widths: this.widths, groups: this.groups, rows: m.rows, fullW: m.fullW, bayW: m.bayW, W: m.W, gap: m.gap, secGap: m.secGap });
  }

  private relayout(animate: boolean): void {
    const prevH = this.m?.H;
    let m = this.metrics();
    let p = this.plan(m);
    // Nearly fits on one bay? Make the tapes a touch smaller rather than send a few off screen.
    if (!p.oneBay && p.total <= (p.rows * m.fullW) / 0.86) {
      m = this.metrics(clamp((p.rows * m.fullW * 0.95) / p.total, 0.8, 1));
      p = this.plan(m);
    }
    // A preview card shows one tidy shelf: whatever fits on it.
    if (this.mini && !p.oneBay) {
      let fit = 0;
      while (fit < p.n && p.bay[fit] === 0 && p.row[fit] === 0) fit++;
      this.setItems(this.items.slice(0, Math.max(1, fit)));
      p = this.plan(m);
    }
    this.m = m;
    this.p = p;
    this.position();
    this.draw(animate, true);
    // Tapes changed size (window, or more titles than fit): every title is set again for the new spine.
    if (prevH !== undefined && prevH !== m.H) this.fitTitles();
  }

  /** World coordinates of every tape (numbers only). */
  private position(): void {
    const { m, p } = this;
    this.pitch = m.H + m.head + m.bh;
    const totalH = p.rows * this.pitch;
    this.top0 = m.top + Math.max(0, (m.availH - totalH) / 2);
    this.stride = (p.oneBay ? m.fullW : m.bayW) + m.bayGap;
    const contentW = p.oneBay ? p.bayLen[0] : (p.bays - 1) * this.stride + p.bayLen[p.bays - 1];
    this.x0 = contentW <= m.fullW ? (m.vw - contentW) / 2 : m.padX;
    this.maxX = Math.max(0, this.x0 + contentW + m.padX - m.vw);
    const n = p.n;
    if (this.X.length !== n) {
      this.X = new Float64Array(n);
      this.Y = new Float64Array(n);
    }
    for (let i = 0; i < n; i++) {
      this.X[i] = this.x0 + p.bay[i] * this.stride + p.x[i];
      this.Y[i] = this.tapeTop(p.row[i]);
    }
    this.targetX = clamp(this.targetX, 0, this.maxX);
    this.scrollX = clamp(this.scrollX, 0, this.maxX);
  }

  private tapeTop(r: number): number {
    return this.top0 + r * this.pitch + this.m.head;
  }

  private transformOf(i: number): string {
    const z = -(rand01(`${this.items[i].id}:z`) * 3);
    return `translate3d(${this.X[i].toFixed(1)}px, ${this.Y[i].toFixed(1)}px, ${z.toFixed(2)}px)`;
  }

  // ── drawing (windowed) ──

  /** Bays near the visible part of the shelf. */
  private bayRange(): [number, number] {
    const margin = Math.max(this.m.vw * 0.35, this.m.W * 10);
    const a = this.scrollX - margin;
    const b = this.scrollX + this.m.vw + margin;
    const first = clamp(Math.floor((a - this.x0) / this.stride), 0, this.p.bays - 1);
    const last = clamp(Math.floor((b - this.x0) / this.stride), 0, this.p.bays - 1);
    return [first, last];
  }

  /**
   * Put the tapes near the view in the DOM (and only those), with the shelves under
   * them. `animate`: a search or filter moved tapes, so they slide to their new places.
   */
  private draw(animate: boolean, all = false): void {
    if (!this.m) return;
    const motion = animate && !reducedMotion();
    const { m, p } = this;
    const margin = Math.max(m.vw * 0.35, m.W * 10);
    const lo = this.scrollX - margin;
    const hi = this.scrollX + m.vw + margin;
    const [firstBay, lastBay] = this.bayRange();
    const keep = new Set<Id>();
    const fresh: Tape[] = [];
    const frag = document.createDocumentFragment();
    for (let b = firstBay; b <= lastBay; b++) {
      for (let i = p.bayStart[b]; i < p.bayStart[b + 1]; i++) {
        if (this.X[i] + this.widths[i] < lo || this.X[i] > hi) continue;
        const item = this.items[i];
        keep.add(item.id);
        const t = this.transformOf(i);
        let tape = this.tapes.get(item.id);
        if (!tape) {
          tape = this.makeTape(item);
          tape.slot.style.transform = t;
          tape.t = t;
          fresh.push(tape);
          frag.append(tape.slot);
          continue;
        }
        if (tape.t !== t) {
          if (motion) tape.slot.animate([{ transform: tape.t }, { transform: t }], { duration: 520, easing: SOFT });
          tape.slot.style.transform = t;
          tape.t = t;
        }
      }
    }
    // The tape you opened stays while its turn plays, wherever the window went.
    if (this.openId !== null && this.tapes.has(this.openId) && this.index.has(this.openId)) keep.add(this.openId);
    for (const [id, tape] of this.tapes) {
      if (keep.has(id)) continue;
      this.tapes.delete(id);
      if (id === this.openId) this.openId = null;
      // Gone from the results (not just scrolled away): it sinks back into the shelf.
      if (motion && !this.index.has(id)) {
        tape.slot.style.pointerEvents = 'none';
        tape.slot
          .animate([{ transform: tape.t, opacity: 1 }, { transform: `${tape.t} translateZ(-46px)`, opacity: 0 }], { duration: 260, easing: 'ease-in', fill: 'forwards' })
          .finished.then(
            () => tape.slot.remove(),
            () => tape.slot.remove(),
          );
      } else tape.slot.remove();
    }
    if (this.focusId !== null && !this.index.has(this.focusId)) this.focusId = null;
    this.drawExtras(firstBay, lastBay);
    this.world.append(frag);
    if (fresh.length) {
      this.fitTitles(fresh);
      if (this.focusId !== null) this.tapes.get(this.focusId)?.slot.classList.add(styles.kf);
      if (motion) {
        fresh.forEach((tape, k) => {
          tape.slot.animate([{ transform: `${tape.t} translate3d(0,-18px,-40px)`, opacity: 0 }, { transform: tape.t, opacity: 1 }], {
            duration: 420,
            delay: Math.min(k, 12) * 18,
            easing: SOFT,
            fill: 'backwards',
          });
        });
      }
    }
    this.drawnAt = this.scrollX;
    if (all) {
      this.scrollTo(this.targetX, !animate);
      if (this.focusId !== null) this.ensureVisible(this.focusId);
    }
  }

  /** Shelf boards, their light on the wall, bookends and section tags, for the bays in view. */
  private drawExtras(firstBay: number, lastBay: number): void {
    const { m, p } = this;
    const keep = new Set<string>();
    const put = (key: string, build: () => HTMLElement): HTMLElement => {
      keep.add(key);
      let n = this.extras.get(key);
      if (!n) {
        n = build();
        this.extras.set(key, n);
        // Extras sit behind the tapes in paint order.
        this.world.prepend(n);
      }
      return n;
    };
    // A handful of tapes still gets a proper length of shelf, with the tapes in the middle.
    const minLen = this.mini ? 0 : m.W * 7;
    for (let b = firstBay; b <= lastBay; b++) {
      const bx = this.x0 + b * this.stride;
      const used = p.oneBay ? p.bayLen[0] : b < p.bays - 1 ? m.bayW : Math.max(p.bayLen[b], p.bays > 1 ? m.bayW : 0);
      const len = Math.max(used, minLen);
      const bxb = bx - (len - used) / 2;
      for (let r = 0; r < p.rows; r++) {
        const by = this.tapeTop(r) + m.H;
        const board = put(`b${b}r${r}`, () => {
          const n = el('div', styles.board, { 'aria-hidden': 'true' });
          n.append(el('div', styles.boardTop), el('div', styles.boardEdge));
          return n;
        });
        board.style.width = `${len + 2 * m.over}px`;
        board.style.transform = `translate3d(${(bxb - m.over).toFixed(1)}px, ${by.toFixed(1)}px, 0)`;
        const glow = put(`g${b}r${r}`, () => el('div', styles.glow, { 'aria-hidden': 'true' }));
        glow.style.width = `${len + 2 * m.over}px`;
        glow.style.height = `${m.head + m.H * 0.7}px`;
        glow.style.transform = `translate3d(${(bxb - m.over).toFixed(1)}px, ${(this.tapeTop(r) - m.head).toFixed(1)}px, ${-m.D - 8}px)`;
        const rl = p.rowLen[b]?.[r];
        if (rl !== undefined && rl > 0) {
          const endL = put(`eL${b}r${r}`, () => this.makeEnd(styles.endL));
          const endR = put(`eR${b}r${r}`, () => this.makeEnd(styles.endR));
          endL.style.transform = `translate3d(${(bx - m.endGap - m.ew).toFixed(1)}px, ${(by - m.H * 0.58).toFixed(1)}px, 0)`;
          endR.style.transform = `translate3d(${(bx + rl + m.endGap).toFixed(1)}px, ${(by - m.H * 0.58).toFixed(1)}px, 0)`;
        }
      }
    }
    for (const t of p.tags) {
      if (t.bay < firstBay || t.bay > lastBay) continue;
      const tag = put(`t:${t.label}@${t.bay}.${t.row}`, () => el('div', styles.tag, { 'aria-hidden': 'true' }, t.label));
      tag.style.transform = `translate3d(${(this.x0 + t.bay * this.stride + t.x).toFixed(1)}px, ${(this.tapeTop(t.row) + m.H + 3).toFixed(1)}px, ${m.lip + 1}px)`;
    }
    for (const [k, n] of this.extras) {
      if (keep.has(k)) continue;
      n.remove();
      this.extras.delete(k);
    }
  }

  private makeEnd(side: string): HTMLDivElement {
    const n = el('div', `${styles.end} ${side}`, { 'aria-hidden': 'true' });
    n.append(el('div', styles.endFace), el('div', styles.endSide));
    return n;
  }

  private makeTape(item: ItemSummary): Tape {
    const p = paletteOf(item);
    const look = spineLook(item, p);
    const slot = el('div', [styles.slot, item.available ? '' : styles.offline].filter(Boolean).join(' '), {
      role: this.mini ? null : 'button',
      tabindex: this.mini ? null : '-1',
      'aria-label': this.mini ? null : artLabel(item),
      'data-id': item.id,
    });
    if (item.kind === 'show') slot.style.setProperty('--tw', 'calc(var(--w) * 1.85)');
    slot.style.setProperty('--sp-bg', look.bg);
    slot.style.setProperty('--sp-bg2', look.bg2);
    slot.style.setProperty('--sp-ink', look.ink);
    slot.style.setProperty('--sp-acc', look.acc);
    slot.style.setProperty('--sp-top', look.top);
    const tape = el('div', styles.tape);

    // Spine
    const spine = el('div', `${styles.face} ${styles.spine} ${styles[look.style]}`);
    const cap = el('div', styles.cap);
    if (!item.available) {
      cap.classList.add(styles.capOff);
      cap.textContent = 'OFFLINE';
    }
    else if (look.style === 'number') cap.append(el('span', styles.num, null, '№'), String((Math.floor(rand01(`${item.id}:no`) * 880) + 101).toString()));
    else cap.textContent = item.year ? String(item.year) : '—';
    const tbox = el('div', styles.title);
    const parts = spineTitleParts(item.kind === 'episode' && item.parent_title ? item.parent_title : item.title);
    const tspan = el('span', null, null, parts.article ? el('small', styles.a, null, parts.article) : null, parts.main, parts.sub ? el('small', styles.sub, null, parts.sub) : null);
    tbox.append(tspan);
    if (item.kind === 'show') tbox.append(el('span', styles.chap, null, item.child_count ? `${item.child_count} SEASON${item.child_count === 1 ? '' : 'S'}` : 'COMPLETE SERIES'));
    const foot = el('div', styles.foot);
    foot.innerHTML = MARK;
    foot.append(el('span', styles.lbl, null, 'VIDEO'));
    spine.append(cap, tbox, foot);
    this.stickers(item, spine);
    const prog = progressFraction(item);
    if (prog !== null) {
      const pr = el('i', styles.prog);
      pr.style.setProperty('--p', String(prog));
      spine.append(pr);
    }

    // The top of the box; the sleeve (front cover) on its right side, the back cover on its left.
    const topFace = el('div', `${styles.face} ${styles.top}`);
    const right = el('div', `${styles.face} ${styles.right}`);
    // The poster only loads when the tape comes off the shelf; until then the blurhash paints it.
    const sleeve = artElement(item, { width: this.m.D, defer: true, className: styles.sleeve });
    right.append(sleeve);
    const left = el('div', `${styles.face} ${styles.left}`);
    const meta = [item.year ? String(item.year) : '', item.kind === 'show' ? 'SERIES' : item.kind === 'movie' ? 'FEATURE' : '', ...item.badges.slice(0, 3)].filter(Boolean).join(' · ');
    left.append(el('div', styles.back, null, el('b', null, null, item.title), el('p', null, null, item.parent_title && item.kind !== 'show' ? item.parent_title : 'flux video'), el('span', null, null, meta)));
    tape.append(left, right, topFace, spine);
    slot.append(tape);
    const rec: Tape = { id: item.id, item, slot, spine, sleeve, tbox, tspan, t: '' };
    this.tapes.set(item.id, rec);
    return rec;
  }

  /** Rare, stable stickers: NEW on recent additions, and now and then a price or a plea to rewind. */
  private stickers(item: ItemSummary, spine: HTMLElement): void {
    const r = (k: string) => rand01(`${item.id}:${k}`);
    const sticker = (cls: string, rot: number, ...kids: (string | Node)[]) => {
      const s = el('span', `${styles.st} ${cls}`, { 'aria-hidden': 'true' }, ...kids);
      s.style.setProperty('--rot', `${rot.toFixed(1)}deg`);
      spine.append(s);
    };
    if (isNew(item)) sticker(styles.stNew, r('nr') * 10 - 6, 'NEW');
    // Price and rewind stickers only where the title leaves room for them.
    const short = item.title.replace(/^(the|a|an)\s+/i, '').length <= 13;
    if (!short || !item.available) return;
    if (r('price') < 0.12) sticker(styles.stPrice, r('pr') * 8 - 4, ['$2.99', '$1.99', '99¢', '$3.49'][Math.floor(r('pv') * 4)]);
    else if (r('rewind') < 0.1) sticker(styles.stRewind, r('rr') * 10 - 5, 'BE KIND', el('br'), 'REWIND');
  }

  /** Shrink long titles so each runs the length of its spine and no further (read everything, then write). */
  private fitTitles(recs: Tape[] = [...this.tapes.values()]): void {
    if (!recs.length) return;
    for (const r of recs) r.tspan.style.fontSize = '';
    const reads = recs.map((r) => [Math.max(r.tspan.offsetHeight, r.tspan.scrollHeight), r.tbox.clientHeight, parseFloat(getComputedStyle(r.tspan).fontSize) || 16] as const);
    recs.forEach((r, i) => {
      const [len, avail, own] = reads[i];
      const room = avail - 10;
      if (len > room && len > 0 && room > 0) r.tspan.style.fontSize = `${Math.max(6.5, (own * room) / len).toFixed(2)}px`;
    });
  }

  // ── scrolling ──

  private scrollTo(x: number, immediate = false): void {
    this.targetX = clamp(x, 0, this.maxX);
    if (immediate || reducedMotion()) {
      this.scrollX = this.targetX;
      this.apply();
      return;
    }
    if (!this.raf) this.raf = requestAnimationFrame(this.tick);
  }

  private tick = () => {
    const d = this.targetX - this.scrollX;
    if (Math.abs(d) < 0.25) {
      this.scrollX = this.targetX;
      this.raf = 0;
      this.apply();
      return;
    }
    this.scrollX += d * 0.15;
    this.apply();
    this.raf = requestAnimationFrame(this.tick);
  };

  private apply(): void {
    this.world.style.transform = `translate3d(${(-this.scrollX).toFixed(2)}px, 0, 0)`;
    // Bring in the tapes ahead before they come into view.
    if (!(Math.abs(this.scrollX - this.drawnAt) < this.m.W * 4)) this.draw(false);
  }

  private ensureVisible(id: Id, instant = false): void {
    const i = this.index.get(id);
    if (i === undefined) return;
    const m = this.m;
    const left = this.X[i] - this.targetX;
    const right = left + this.widths[i];
    const lo = m.padX + m.W * 1.5;
    const hi = m.vw - m.padX - m.W * 1.5;
    if (left < lo) this.scrollTo(this.targetX - (lo - left), instant);
    else if (right > hi) this.scrollTo(this.targetX + (right - hi), instant);
  }

  // ── input ──

  private bind(): void {
    const sc = this.scene;
    const on = <K extends keyof HTMLElementEventMap>(type: K, fn: (e: HTMLElementEventMap[K]) => void, opts?: AddEventListenerOptions) => {
      sc.addEventListener(type, fn as EventListener, opts);
      this.cleanup.push(() => sc.removeEventListener(type, fn as EventListener, opts));
    };
    on(
      'wheel',
      (e) => {
        if (e.ctrlKey || this.maxX <= 0) return;
        const d = Math.abs(e.deltaX) > Math.abs(e.deltaY) ? e.deltaX : e.deltaY;
        if (!d) return;
        e.preventDefault();
        this.scrollTo(this.targetX + d * (e.deltaMode === 1 ? 32 : 1));
      },
      { passive: false },
    );

    let down: { id: number; x: number; s: number; lx: number; lt: number; v: number; drag: boolean } | null = null;
    on('pointerdown', (e) => {
      if (e.button !== 0) return;
      down = { id: e.pointerId, x: e.clientX, s: this.targetX, lx: e.clientX, lt: performance.now(), v: 0, drag: false };
    });
    on('pointermove', (e) => {
      if (!down || e.pointerId !== down.id) return;
      const dx = e.clientX - down.x;
      if (!down.drag && Math.abs(dx) > 7 && this.maxX > 0) {
        down.drag = true;
        sc.setPointerCapture(e.pointerId);
        sc.classList.add(styles.dragging);
      }
      if (!down.drag) return;
      const now = performance.now();
      down.v = (e.clientX - down.lx) / Math.max(1, now - down.lt);
      down.lx = e.clientX;
      down.lt = now;
      this.scrollTo(down.s - dx, true);
    });
    const up = (e: PointerEvent) => {
      if (!down || e.pointerId !== down.id) return;
      if (down.drag) {
        this.suppressClick = true;
        this.timers.push(setTimeout(() => (this.suppressClick = false), 0));
        sc.classList.remove(styles.dragging);
        // A flick keeps going a little.
        this.scrollTo(this.targetX - down.v * 280);
      }
      down = null;
    };
    on('pointerup', up);
    on('pointercancel', up);

    const slotOf = (t: EventTarget | null) => (t instanceof Element ? t.closest<HTMLElement>(`.${styles.slot}`) : null);
    on('pointerover', (e) => {
      const s = slotOf(e.target);
      if (!s || e.pointerType === 'touch') return;
      const id = Number(s.dataset.id);
      if (this.hoverT) clearTimeout(this.hoverT);
      this.hoverT = setTimeout(() => {
        const i = this.index.get(id);
        if (i === undefined) return;
        this.ctx.setFocus(id);
        this.ctx.intent(this.items[i]);
        const tape = this.tapes.get(id);
        if (tape) loadArt(tape.sleeve);
      }, 90);
    });
    on('click', (e) => {
      if (this.suppressClick) return;
      const s = slotOf(e.target);
      if (s) this.openTape(Number(s.dataset.id));
    });
    on('focusin', (e) => {
      const s = slotOf(e.target);
      if (s && Number(s.dataset.id) !== this.focusId) this.setKeyFocus(Number(s.dataset.id));
    });
  }

  private markFocus(id: Id | null): void {
    for (const t of this.tapes.values()) t.slot.classList.toggle(styles.kf, t.id === id);
  }

  private setKeyFocus(id: Id, instant = false): void {
    const i = this.index.get(id);
    if (i === undefined) return;
    this.focusId = id;
    this.ensureVisible(id, instant);
    if (!this.tapes.has(id)) this.draw(false);
    this.markFocus(id);
    this.ctx.setFocus(id);
    this.ctx.intent(this.items[i]);
    const tape = this.tapes.get(id);
    if (tape) {
      loadArt(tape.sleeve);
      if (document.activeElement !== tape.slot) tape.slot.focus({ preventScroll: true });
    }
  }

  private openTape(id: Id): void {
    if (!this.index.has(id)) return;
    if (this.openId !== null && this.openId !== id) this.closeTape(this.openId);
    this.setKeyFocus(id);
    const tape = this.tapes.get(id);
    if (!tape) return;
    loadArt(tape.sleeve, true);
    this.openId = id;
    tape.slot.classList.add(styles.out);
    if (reducedMotion()) {
      tape.slot.classList.add(styles.open);
      this.ctx.open(id, tape.sleeve);
      return;
    }
    // Out first, then the turn, then the sleeve becomes the detail view.
    // Skip the hand-off if this lens has stopped being the one on screen (switched lens mid-turn).
    const live = () => !this.dead && this.openId === id && activeLens()?.view === this;
    this.timers.push(
      setTimeout(() => {
        if (live()) tape.slot.classList.add(styles.open);
      }, 160),
      setTimeout(() => {
        if (live()) this.ctx.open(id, tape.sleeve);
      }, 640),
    );
  }

  private closeTape(id: Id): void {
    const tape = this.tapes.get(id);
    if (this.openId === id) this.openId = null;
    if (!tape) return;
    tape.slot.classList.remove(styles.open);
    this.timers.push(
      setTimeout(
        () => {
          if (this.openId !== id) tape.slot.classList.remove(styles.out);
        },
        reducedMotion() ? 0 : 320,
      ),
    );
  }

  // ── contract ──

  update(items: readonly ItemSummary[]): void {
    this.setItems(items);
    this.relayout(true);
  }

  move(dir: Dir): boolean {
    const n = this.items.length;
    if (!n) return false;
    const cur = this.focusId !== null ? this.index.get(this.focusId) : undefined;
    if (cur === undefined) {
      // The first tape on screen.
      let first = 0;
      while (first < n - 1 && this.X[first] - this.scrollX < this.m.padX) first++;
      this.setKeyFocus(this.items[first].id);
      return true;
    }
    if (dir === 'left' || dir === 'right') {
      this.setKeyFocus(this.items[clamp(cur + (dir === 'right' ? 1 : -1), 0, n - 1)].id);
      return true;
    }
    const { p } = this;
    const bay = p.bay[cur];
    const row = p.row[cur] + (dir === 'down' ? 1 : -1);
    const mid = p.x[cur] + this.widths[cur] / 2;
    let best = -1;
    let bd = Infinity;
    for (let i = p.bayStart[bay]; i < p.bayStart[bay + 1]; i++) {
      if (p.row[i] !== row) continue;
      const d = Math.abs(p.x[i] + this.widths[i] / 2 - mid);
      if (d < bd) {
        bd = d;
        best = i;
      }
    }
    if (best >= 0) this.setKeyFocus(this.items[best].id);
    return true;
  }

  key(e: KeyboardEvent): boolean {
    if ((e.key === 'Enter' || e.key === ' ') && this.focusId !== null) {
      this.openTape(this.focusId);
      return true;
    }
    return false;
  }

  rectOf(id: Id): DOMRect | null {
    const tape = this.tapes.get(id);
    if (tape) return (this.openId === id ? tape.sleeve : tape.spine).getBoundingClientRect();
    // Off the window: where its spine would be (flat, ignoring perspective).
    const i = this.index.get(id);
    if (i === undefined) return null;
    const r = this.root.getBoundingClientRect();
    return new DOMRect(r.left + this.X[i] - this.scrollX, r.top + this.Y[i], this.widths[i], this.m.H);
  }

  artOf(id: Id): HTMLElement | null {
    const tape = this.tapes.get(id);
    if (!tape) return null;
    return this.openId === id ? tape.sleeve : tape.spine;
  }

  reveal(id: Id): void {
    const i = this.index.get(id);
    if (i === undefined) return;
    this.scrollTo(this.X[i] + this.widths[i] / 2 - this.m.vw / 2, true);
    this.focusId = id;
    this.draw(false);
    this.markFocus(id);
  }

  /** Covered by the detail view or the player; when it lifts, the tape goes back on the shelf. */
  pause(paused: boolean): void {
    if (!paused && this.openId !== null) this.closeTape(this.openId);
  }

  destroy(): void {
    this.dead = true;
    this.ro.disconnect();
    for (const c of this.cleanup) c();
    if (this.raf) cancelAnimationFrame(this.raf);
    if (this.hoverT) clearTimeout(this.hoverT);
    for (const t of this.timers) clearTimeout(t);
  }
}

export const shelf: LensModule = {
  mount(root, ctx) {
    return new ShelfView(root, ctx);
  },
};
