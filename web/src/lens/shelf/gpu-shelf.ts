/**
 * Shelf, drawn with WebGPU: the same shelves, tapes and behaviour as the DOM
 * Shelf (shelf.ts, still used where WebGPU isn't available), drawn as perspective
 * quads (gpu/scene3d.ts) in one draw call however many tapes are in view.
 *
 * - Each spine is painted once with Canvas 2D (spine.ts) into a sprite sheet, a few
 *   per frame as tapes come into view; until then a tape shows its colours.
 * - There is no depth buffer, so a frame is drawn back to front: the light on the
 *   wall and the boards, the tapes' sides and tops in the order they hide each
 *   other, the spines, the boards' lips and labels, and last the tapes that are off
 *   the shelf (the part of their sides still inside the shelf goes with the shelf).
 * - The DOM keeps what it is good at: an invisible copy of the tapes in view, with
 *   the DOM Shelf's roles and labels, for screen readers, and one element that
 *   carries keyboard focus.
 */
import { decode } from 'blurhash';
import type { Id, ItemSummary } from '../../api/types';
import { sourceLabel } from '../../components/art-label';
import { artLabel } from '../art';
import { activeLens } from '../bridge';
import { clamp, el, rand01, reducedMotion } from '../dom';
import type { Gpu } from '../gpu/device';
import { bezier, lerp, soft, spring } from '../gpu/motion';
import { classFor, posterCache, type PosterCache } from '../gpu/posters';
import { clock } from '../gpu/quads';
import { corners, FLAG3D, inside, KIND3D, prepareScene3D, Quad3DBatch, Scene3DRenderer } from '../gpu/scene3d';
import { SpriteSheet, type Sprite } from '../gpu/sprites';
import { textAtlas, type Glyph, type TextAtlas, type TextStyle } from '../gpu/text';
import { paletteOf } from '../palette';
import { isNew, isWatchedItem, progressFraction } from '../sections';
import type { Dir, LensContext, LensModule, LensView } from '../types';
import { planShelf, shelfLayout, shelfMetrics, spineLook, type Metrics, type ShelfLayout, type ShelfPlan, type SpineLook } from './geometry';
import styles from './shelf.module.css';
import { backKey, grayHex, mixBlackOklab, paintBack, paintEdge, paintGlow, paintRamp, paintSheen, paintSpine, spineKey, type ShelfFonts } from './spine';
import { facing, I3, place, REST, rotX, rotY, splitAtDepth, tapeFaces, type M3, type Placed, type TapeFaces, type TapeMove } from './transform';

type RGBA = [number, number, number, number];
type Span = [number, number];
type Stage = 'rest' | 'pull' | 'out' | 'open';

const MINI_COUNT = 14;
const CONDENSED = '800 20px "Archivo Variable"';

const WHITE: RGBA = [1, 1, 1, 1];
const BLACK: RGBA = [0, 0, 0, 1];
const INK: RGBA = [0.914, 0.902, 0.875, 1];
const PILL: RGBA = [8 / 255, 8 / 255, 10 / 255, 1];

// Timings (seconds) and curves, as shelf.module.css and shelf.ts have them.
const PULL_S = 0.62;
const OUT_S = 0.36;
const OPEN_S = 0.72;
const LIT_S = 0.3;
const SHADE_S = 0.5;
const SHEEN_S = 0.6;
const SHEEN_DELAY = 0.2;
const SLIDE_S = 0.52;
const ENTER_S = 0.42;
const ENTER_STAGGER = 0.018;
const SINK_S = 0.26;
const EXTRA_S = 0.5;
const OUT_CURVE = bezier(0.3, 0.8, 0.3, 1);
const EASE_OUT = bezier(0.2, 0.8, 0.2, 1);
const EASE_IN = bezier(0.42, 0, 1, 1);
/** `--spring-soft` (tokens.css): boards, bookends and labels sliding to a new layout. */
const SPRING_SOFT = [0, 0.032, 0.106, 0.201, 0.303, 0.403, 0.496, 0.579, 0.652, 0.715, 0.768, 0.813, 0.849, 0.88, 0.904, 0.924, 0.94, 0.953, 0.963, 0.971, 0.978, 0.983, 0.987, 0.99, 0.992, 0.994, 0.995, 0.996, 0.997, 0.998, 1];

function springSoft(p: number): number {
  if (p <= 0) return 0;
  if (p >= 1) return 1;
  const f = p * (SPRING_SOFT.length - 1);
  const i = Math.floor(f);
  return SPRING_SOFT[i] + (SPRING_SOFT[i + 1] - SPRING_SOFT[i]) * (f - i);
}

/** The plane of the spines on the shelf: what is in front of it is out of the shelf. */
const Z0 = 0;
const RY_RIGHT = rotY(Math.PI / 2);
const RY_LEFT = rotY(-Math.PI / 2);
const RX_TOP = rotX(Math.PI / 2);

/** A value easing to a target, with an optional delay (CSS `transition-delay`). */
class Ease {
  from: number;
  to: number;
  start = 0;
  dur = 0;
  private curve: (p: number) => number;

  constructor(curve: (p: number) => number, value: number) {
    this.curve = curve;
    this.from = value;
    this.to = value;
  }

  value(now: number): number {
    if (this.dur <= 0 || now >= this.start + this.dur) return this.to;
    if (now <= this.start) return this.from;
    return lerp(this.from, this.to, this.curve((now - this.start) / this.dur));
  }

  done(now: number): boolean {
    return this.dur <= 0 || now >= this.start + this.dur;
  }

  set(to: number, now: number, dur: number, delay = 0): void {
    if (to === this.to) return;
    this.from = this.value(now);
    this.to = to;
    this.start = now + delay;
    this.dur = dur;
  }
}

/** A tape off its resting place: where it is going and how (the DOM Shelf's classes, as numbers). */
interface Anim {
  stage: Stage;
  from: TapeMove;
  to: TapeMove;
  start: number;
  dur: number;
  curve: (p: number) => number;
  /** Spine brightness (hover, keyboard focus), the shade on its sides, the light across a turned sleeve. */
  lit: Ease;
  shadeL: Ease;
  shadeR: Ease;
  sheen: Ease;
}

function poseAt(a: Anim, now: number): TapeMove {
  if (a.dur <= 0 || now >= a.start + a.dur) return a.to;
  const e = a.curve(clamp((now - a.start) / a.dur, 0, 1));
  const f = a.from;
  const t = a.to;
  return { ty: lerp(f.ty, t.ty, e), tz: lerp(f.tz, t.tz, e), rx: lerp(f.rx, t.rx, e), ry: lerp(f.ry, t.ry, e), s: lerp(f.s, t.s, e) };
}

/** Per title: its spine look and colours, worked out once. */
interface TapeLook {
  item: ItemSummary;
  look: SpineLook;
  /** The spine until its sprite is painted (offline: greyed). */
  bg: RGBA;
  bg2: RGBA;
  /** The back cover's gradient and the top of the box. */
  fbg: RGBA;
  fbg2: RGBA;
  top: RGBA;
  topDark: RGBA;
  /** The sleeve until its poster is in: the art's average colour, or the title's two colours. */
  paper: RGBA;
  paper2: RGBA | null;
  /** How far back it sits on the shelf (a little uneven). */
  z: number;
  key: string;
  keyFor: string;
  /** The sprite last drawn, kept while a new one is painted (a font arriving). */
  shown: string | null;
  back: string;
  backFor: string;
}

/** A tape off the shelf this frame: its placed faces (null: turned away) and looks. */
interface Active {
  i: number;
  id: Id;
  l: TapeLook;
  tw: number;
  a: number;
  depth: number;
  lit: number;
  shadeL: number;
  shadeR: number;
  sheen: number;
  ring: boolean;
  spine: Placed | null;
  top: Placed | null;
  right: Placed | null;
  left: Placed | null;
  splitR: { front: Span | null; back: Span | null } | null;
  splitL: { front: Span | null; back: Span | null } | null;
}

/** A row of one bay this frame: its board (and light), bookends and tapes. */
interface ShelfRow {
  b: number;
  r: number;
  /** Board: left edge and width (world), the board's top (tapes stand on it). */
  x: number;
  w: number;
  by: number;
  /** Top of the tapes, the light's top and height. */
  top: number;
  endL: number | null;
  endR: number | null;
  endY: number;
  i0: number;
  i1: number;
}

interface Ext {
  x: number;
  y: number;
  w: number;
}

interface Ghost {
  l: TapeLook;
  tw: number;
  x: number;
  y: number;
  z: number;
  start: number;
}

interface Theme {
  focus: RGBA;
  tagBg: RGBA;
  tagInk: RGBA;
  light: string;
  top1: string;
  top2: string;
  edge1: string;
  edge2: string;
  hi: string;
  end1: string;
  end2: string;
  shade: string;
  shade1: string;
  fonts: ShelfFonts;
}

function hexColor(h: string): RGBA {
  const n = parseInt(h.slice(1, 7), 16);
  return [((n >> 16) & 255) / 255, ((n >> 8) & 255) / 255, (n & 255) / 255, 1];
}

let probe: CanvasRenderingContext2D | null = null;

/** Any CSS colour as straight RGBA 0–1 (the canvas parses it). */
function cssColor(value: string, fallback: RGBA): RGBA {
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

const B83 = '0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz#$%*+,-.:;=?@[]^_{|}~';

/** The poster's average colour (the blurhash DC term). */
function dcColor(hash: string): RGBA | null {
  if (hash.length < 6) return null;
  let v = 0;
  for (const ch of hash.slice(2, 6)) {
    const d = B83.indexOf(ch);
    if (d < 0) return null;
    v = v * 83 + d;
  }
  return [((v >> 16) & 255) / 255, ((v >> 8) & 255) / 255, (v & 255) / 255, 1];
}

const edgeCache = new Map<string, [RGBA, RGBA] | null>();

/**
 * The sleeve before its poster loads shows the blurred poster (art.ts); what shows
 * of it on the shelf is its front edge: that column's colours, top and bottom.
 */
function edgeColors(hash: string, crop0: number): [RGBA, RGBA] | null {
  const key = `${hash}|${crop0.toFixed(2)}`;
  let c = edgeCache.get(key);
  if (c !== undefined) return c;
  try {
    // A coarse decode is enough for one edge (its first column sits about where the sleeve's front edge does).
    const W = 4;
    const H = 6;
    const px = decode(hash, W, H);
    const col = Math.min(W - 1, Math.max(0, Math.floor(crop0 * W)));
    const avg = (r0: number, r1: number): RGBA => {
      const t = [0, 0, 0];
      for (let y = r0; y < r1; y++) {
        const o = (y * W + col) * 4;
        t[0] += px[o];
        t[1] += px[o + 1];
        t[2] += px[o + 2];
      }
      const n = (r1 - r0) * 255;
      return [t[0] / n, t[1] / n, t[2] / n, 1];
    };
    c = [avg(0, 2), avg(4, 6)];
  } catch {
    c = null;
  }
  if (edgeCache.size > 20000) edgeCache.clear();
  edgeCache.set(key, c);
  return c;
}

/** A face at rest, written into `out` (no allocation per face per frame). */
function setFace(out: Placed, x: number, y: number, z: number, w: number, h: number, rx: number, ry: number, m: M3): Placed {
  out.x = x;
  out.y = y;
  out.z = z;
  out.w = w;
  out.h = h;
  out.rx = rx;
  out.ry = ry;
  out.rz = 0;
  out.m = m;
  out.s = 1;
  return out;
}

const blank = (): Placed => ({ x: 0, y: 0, z: 0, w: 0, h: 0, rx: 0, ry: 0, rz: 0, m: I3, s: 1 });

export class GpuShelfView implements LensView {
  private root: HTMLElement;
  private ctx: LensContext;
  private gpu: Gpu;
  private mini: boolean;
  private canvas: HTMLCanvasElement;
  /** The tapes in view, invisible, for screen readers (see syncMirror). */
  private group: HTMLDivElement | null = null;
  private track: HTMLDivElement | null = null;
  private focusEl: HTMLDivElement | null = null;
  private mirror = new Map<Id, HTMLDivElement>();
  private twin: HTMLDivElement | null = null;
  private mirrorAt = 0;
  private renderer: Scene3DRenderer;
  private posters: PosterCache;
  private text: TextAtlas;
  private sheet: SpriteSheet | null = null;
  private sheetFor = '';
  private sheetSerial = 0;
  private sheetMax = 64;
  private sheetCanvas: HTMLCanvasElement | null = null;
  private batch = new Quad3DBatch(1024);
  private theme: Theme;
  private fontGen = 0;
  private fontsOk = false;

  private items: readonly ItemSummary[] = [];
  private groups: string[] = [];
  private groupsFor: readonly ItemSummary[] | null = null;
  private index = new Map<Id, number>();
  private looks = new Map<Id, TapeLook>();
  private m!: Metrics;
  private p!: ShelfPlan;
  private lay!: ShelfLayout;
  private poses!: Record<Stage, TapeMove>;
  private faces = new Map<number, TapeFaces>();
  private X = new Float64Array(0);
  private Y = new Float64Array(0);
  private widths = new Float64Array(0);
  private rowFirst = new Int32Array(1);
  /** Where each tape was drawn in the last frame (view coordinates), and how opaque. */
  private dx = new Float64Array(0);
  private dy = new Float64Array(0);
  private dz = new Float64Array(0);
  private da = new Float64Array(0);
  private drawn: number[] = [];
  private act: Active[] = [];
  private actById = new Map<Id, Active>();
  private crop0 = 0;
  private crop = 1;
  private posterCls = 1;

  private viewW = 0;
  private viewH = 0;
  private dpr = 1;
  private ox = 0;
  private oy = 0;
  private scrollX = 0;
  private targetX = 0;
  private lastT = 0;
  private mountedAt = 0;
  private windowAt = Number.NaN;
  private windowIds = new Set<Id>();

  private focusId: Id | null = null;
  private hoverId: Id | null = null;
  private openId: Id | null = null;
  private hiddenId: Id | null = null;
  private eagerId: Id | null = null;
  private outs = new Set<Id>();
  private opened = new Set<Id>();
  private wanted = new Set<Id>();
  private anims = new Map<Id, Anim>();
  private slides = new Map<Id, { x: number; y: number; start: number }>();
  private enters = new Map<Id, number>();
  private ghosts: Ghost[] = [];
  private extrasNow = new Map<string, Ext>();
  private extraFrom: Map<string, Ext> | null = null;
  private extraStart = 0;

  private paintPending = false;
  private paintedNow = 0;
  private budget = 8;
  private raf = 0;
  private paused = false;
  private dead = false;
  private lastTrim = 0;
  private pointer: { x: number; y: number } | null = null;
  private down: { id: number; x: number; s: number; lx: number; lt: number; v: number; drag: boolean } | null = null;
  private suppressClick = false;
  private hoverT: ReturnType<typeof setTimeout> | null = null;
  private settleT: ReturnType<typeof setTimeout> | null = null;
  private timers = new Set<ReturnType<typeof setTimeout>>();
  private ro: ResizeObserver;
  private cleanup: (() => void)[] = [];
  /** After a lost GPU: the DOM Shelf, which this view then forwards to. */
  private dom: LensView | null = null;
  // Scratch faces for tapes at rest.
  private fSpine = blank();
  private fTop = blank();
  private fSide = blank();
  private tint: RGBA = [1, 1, 1, 1];
  private paintL: TapeLook | null = null;
  private paintTW = 0;
  private paintK = 1;

  constructor(root: HTMLElement, ctx: LensContext, gpu: Gpu) {
    this.root = root;
    this.ctx = ctx;
    this.gpu = gpu;
    this.mini = ctx.env.mini;
    root.classList.add(styles.shelf, styles.gpu);
    if (this.mini) root.classList.add(styles.mini);
    this.canvas = el('canvas', styles.canvas, { 'aria-hidden': 'true' });
    root.append(this.canvas);
    if (!this.mini) {
      this.group = el('div', styles.mirror, { role: 'group', 'aria-label': 'Shelf' });
      this.track = el('div', styles.track);
      // Carries keyboard focus (and the screen reader's attention) to the focused tape, wherever it is.
      this.focusEl = el('div', styles.ghost, { role: 'button', tabindex: '-1' });
      this.focusEl.hidden = true;
      this.track.append(this.focusEl);
      this.group.append(this.track);
      root.append(this.group);
    }
    this.renderer = new Scene3DRenderer(gpu, this.canvas);
    this.posters = posterCache(gpu);
    this.text = textAtlas(gpu);
    this.theme = this.readTheme();
    this.mountedAt = clock();
    // Hold the shared poster cache while drawing from it (freed a while after the last lens lets go).
    this.cleanup.push(this.posters.retain(), this.posters.onReady(this.invalidate), this.text.onChange(this.invalidate), gpu.onLost(() => this.fallBack()));
    this.watchTheme();
    this.watchFonts();
    this.measure();
    this.setItems(ctx.items);
    this.relayout(null);
    ctx.setAccessory(null);
    if (!this.mini) {
      this.bind();
      // The shelf scrolls sideways: nothing to tuck the bars away for.
      ctx.scrolled('top');
    }
    this.ro = new ResizeObserver(() => {
      if (this.dom || this.dead) return;
      this.measure();
      this.relayout(null);
    });
    this.ro.observe(root);
    // Arriving from another lens (or back from detail): keep your place on the shelf.
    if (!this.mini && ctx.focus !== null && this.index.has(ctx.focus)) this.reveal(ctx.focus, true);
    this.invalidate();
  }

  // ── setup ──

  private readTheme(): Theme {
    const cs = getComputedStyle(this.root);
    const v = (k: string, d: string) => cs.getPropertyValue(k).trim() || d;
    return {
      focus: cssColor(v('--focus', '#fff'), WHITE),
      tagBg: cssColor(v('--sh-tag-bg', '#f2f0ea'), WHITE),
      tagInk: cssColor(v('--sh-tag-ink', '#141416'), BLACK),
      light: v('--sh-light', 'rgb(255 236 212 / 0.085)'),
      top1: v('--sh-board-top-1', '#36363d'),
      top2: v('--sh-board-top-2', '#141417'),
      edge1: v('--sh-board-edge-1', '#2c2c32'),
      edge2: v('--sh-board-edge-2', '#19191d'),
      hi: v('--sh-board-hi', 'rgb(255 255 255 / 0.18)'),
      end1: v('--sh-end-1', '#4b4c53'),
      end2: v('--sh-end-2', '#222329'),
      shade: v('--sh-shade', 'rgb(0 0 0 / 0.8)'),
      shade1: v('--sh-shade-1', 'rgb(0 0 0 / 0.62)'),
      fonts: {
        mono: v('--font-mono', 'ui-monospace, monospace'),
        cond: v('--font-cond', '"Archivo Variable", sans-serif'),
        body: v('--font-body', 'system-ui, sans-serif'),
      },
    };
  }

  private watchTheme(): void {
    const refresh = () => {
      this.theme = this.readTheme();
      this.invalidate();
    };
    const mo = new MutationObserver(refresh);
    mo.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme', 'class'] });
    const mq = window.matchMedia('(prefers-color-scheme: dark)');
    mq.addEventListener('change', refresh);
    this.cleanup.push(
      () => mo.disconnect(),
      () => mq.removeEventListener('change', refresh),
    );
  }

  /** Spines are set in condensed Archivo: paint them again once the faces are in. */
  private watchFonts(): void {
    const fonts = document.fonts as FontFaceSet | undefined;
    if (!fonts) return;
    const f = this.theme.fonts;
    const check = () => {
      let ok = true;
      try {
        ok = fonts.check(`800 20px ${f.cond}`) && fonts.check(`400 12px ${f.mono}`) && fonts.check(`400 12px ${f.body}`);
      } catch {
        // an unparsable family list: paint with what there is
      }
      if (ok === this.fontsOk) return;
      this.fontsOk = ok;
      this.fontGen++;
      this.invalidate();
    };
    this.fontsOk = true;
    check();
    fonts.addEventListener('loadingdone', check);
    this.cleanup.push(() => fonts.removeEventListener('loadingdone', check));
    for (const spec of [CONDENSED, `400 12px ${f.mono}`, `400 12px ${f.body}`]) fonts.load(spec).then(check, () => undefined);
  }

  /** Canvas size and pixel ratio (a preview in the lens gallery is scaled down: fewer pixels). */
  private measure(): void {
    this.viewW = this.root.clientWidth || window.innerWidth;
    this.viewH = this.root.clientHeight || window.innerHeight;
    let dpr = Math.min(window.devicePixelRatio || 1, 3);
    if (this.mini) {
      const shown = this.root.getBoundingClientRect().width;
      dpr = Math.min(dpr, Math.max(0.5, (dpr * shown) / Math.max(1, this.viewW)) * 1.25);
    }
    this.dpr = dpr;
    this.renderer.resize(this.viewW * dpr, this.viewH * dpr);
    this.ox = this.viewW / 2;
    this.oy = this.viewH * 0.15;
  }

  private setItems(items: readonly ItemSummary[]): void {
    this.items = this.mini ? items.slice(0, MINI_COUNT) : items;
    this.index = new Map(this.items.map((it, i) => [it.id, i]));
  }

  // ── layout (shelf.ts, as numbers) ──

  private metricsFor(scale: number): Metrics {
    return shelfMetrics({ vw: this.viewW, vh: this.viewH, safeTop: this.ctx.safe.top, safeBottom: this.ctx.safe.bottom, mini: this.mini, scale });
  }

  /** Section labels; when a page of a loading library was appended, only the new titles are labelled. */
  private sections(): string[] {
    const items = this.items;
    if (this.groupsFor === items) return this.groups;
    const prev = this.groupsFor;
    let keep = 0;
    if (prev && !this.mini && prev.length > 0 && prev.length <= items.length) {
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

  /**
   * Plan the shelf for the window and the items. `before`: where the tapes were
   * drawn before a search or filter changed them, so they slide to their new places.
   */
  private relayout(before: Map<Id, Ghost> | null): void {
    const prevH = this.m?.H;
    let m = this.metricsFor(1);
    let p = this.plan(m);
    // Nearly fits on one bay? Make the tapes a touch smaller rather than send a few off screen.
    if (!p.oneBay && p.total <= (p.rows * m.fullW) / 0.86) {
      m = this.metricsFor(clamp((p.rows * m.fullW * 0.95) / p.total, 0.8, 1));
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
    if (prevH !== m.H) this.faces.clear();
    const W = m.W;
    this.poses = {
      rest: REST,
      pull: { ty: Math.round(-m.H * 0.055), tz: Math.round(W * 0.9), rx: (-5 * Math.PI) / 180, ry: 0, s: 1 },
      out: { ty: Math.round(-m.H * 0.05), tz: Math.round(m.D / 2 + W / 2 + 28), rx: 0, ry: 0, s: 1 },
      open: { ty: Math.round(-m.H * 0.05), tz: Math.round(m.D / 2 + W / 2 + 28), rx: 0, ry: -Math.PI / 2, s: 1.05 },
    };
    for (const a of this.anims.values()) a.to = this.poses[a.stage];
    // The sleeve is a 2:3 poster cut to the box's proportions (object-fit: cover).
    this.crop = Math.min(1, (m.D / m.H) * 1.5);
    this.crop0 = (1 - this.crop) / 2;
    this.posterCls = classFor(m.D * 1.1 * this.dpr);
    this.ensureSheet();
    if (this.focusId !== null && !this.index.has(this.focusId)) {
      this.focusId = null;
      if (this.focusEl) this.focusEl.hidden = true;
    }
    if (this.openId !== null && !this.index.has(this.openId)) this.openId = null;
    if (this.hoverId !== null && !this.index.has(this.hoverId)) this.setHover(null);
    const now = clock();
    const motion = before !== null && before.size > 0 && !reducedMotion();
    this.syncWindow(true);
    if (motion) {
      // Tapes still here slide to their new places; tapes new to the view drop in; tapes gone sink back.
      let k = 0;
      for (const id of this.windowIds) {
        const i = this.index.get(id);
        if (i === undefined) continue;
        const was = before.get(id);
        if (was) {
          if (Math.abs(was.x - this.X[i]) > 0.5 || Math.abs(was.y - this.Y[i]) > 0.5) this.slides.set(id, { x: was.x, y: was.y, start: now });
        } else {
          this.enters.set(id, now + Math.min(k, 12) * ENTER_STAGGER);
          k++;
        }
      }
      for (const [id, g] of before) if (!this.index.has(id)) this.ghosts.push({ ...g, start: now });
      this.extraFrom = new Map(this.extrasNow);
      this.extraStart = now;
    }
    this.scrollTo(this.targetX, !motion);
    if (this.focusId !== null) this.ensureVisible(this.focusId);
    if (this.focusId !== null) this.placeFocus(this.focusId);
    this.syncMirror();
    this.invalidate();
  }

  /** World coordinates of every tape (numbers only). */
  private position(): void {
    const { m, p } = this;
    this.lay = shelfLayout(m, p);
    const n = p.n;
    if (this.X.length !== n) {
      this.X = new Float64Array(n);
      this.Y = new Float64Array(n);
      this.dx = new Float64Array(n);
      this.dy = new Float64Array(n);
      this.dz = new Float64Array(n);
      this.da = new Float64Array(n);
    }
    for (let i = 0; i < n; i++) {
      this.X[i] = this.lay.x0 + p.bay[i] * this.lay.stride + p.x[i];
      this.Y[i] = this.tapeTop(p.row[i]);
    }
    // The first tape of each row of each bay (tapes fill rows, then bays, in order).
    const R = p.rows;
    const total = p.bays * R;
    this.rowFirst = new Int32Array(total + 1);
    let k = 0;
    for (let key = 0; key <= total; key++) {
      while (k < n && p.bay[k] * R + p.row[k] < key) k++;
      this.rowFirst[key] = k;
    }
    this.targetX = clamp(this.targetX, 0, this.lay.maxX);
    this.scrollX = clamp(this.scrollX, 0, this.lay.maxX);
  }

  private tapeTop(r: number): number {
    return this.lay.top0 + r * this.lay.pitch + this.m.head;
  }

  private facesFor(tw: number): TapeFaces {
    let f = this.faces.get(tw);
    if (!f) {
      f = tapeFaces(tw, this.m.H, this.m.D);
      this.faces.set(tw, f);
    }
    return f;
  }

  /**
   * One sprite sheet for everything painted: cells the size of a spine (a box set's
   * when there are box sets) at this pixel ratio; back covers and the shelf's
   * gradients use a cell each.
   */
  private ensureSheet(): void {
    const m = this.m;
    const boxes = this.items.some((it) => it.kind === 'show');
    const cw = Math.ceil((boxes ? m.boxW : m.W) * this.dpr);
    const ch = Math.ceil(m.H * this.dpr);
    // Room for every spine on screen with some to spare (the sheet grows by doubling up to this).
    const onScreen = this.p.rows * Math.ceil(this.viewW / (m.W + m.gap)) + 16;
    let max = 64;
    while (max < onScreen * 1.6 && max < 512) max *= 2;
    const key = `${cw}x${ch}x${max}`;
    if (this.sheet && this.sheetFor === key) return;
    this.sheet?.destroy();
    this.sheetCanvas?.remove();
    this.sheetCanvas = null;
    this.sheet = new SpriteSheet(this.gpu, cw, ch, max);
    this.sheetFor = key;
    this.sheetMax = max;
    this.sheetSerial++;
    for (const l of this.looks.values()) l.shown = null;
  }

  // ── looks ──

  private lookOf(item: ItemSummary): TapeLook {
    let l = this.looks.get(item.id);
    if (l && l.item === item) return l;
    const pal = paletteOf(item);
    const look = spineLook(item, pal);
    const off = !item.available;
    const hash = item.poster?.blurhash;
    const edge = hash ? edgeColors(hash, this.crop0) : null;
    const dc = hash && !edge ? dcColor(hash) : null;
    l = {
      item,
      look,
      bg: hexColor(grayHex(look.bg, off)),
      bg2: hexColor(grayHex(look.bg2, off)),
      fbg: hexColor(look.bg),
      fbg2: hexColor(look.bg2),
      top: hexColor(look.top),
      topDark: hexColor(mixBlackOklab(look.top, 0.55)),
      paper: edge ? edge[0] : (dc ?? hexColor(pal.c1)),
      paper2: edge ? edge[1] : dc ? null : hexColor(pal.c2),
      z: -rand01(`${item.id}:z`) * 3,
      key: '',
      keyFor: '',
      shown: null,
      back: '',
      backFor: '',
    };
    if (this.looks.size > 8000) this.looks.clear();
    this.looks.set(item.id, l);
    return l;
  }

  /**
   * Canvas text takes its font smoothing from the canvas element's style, and a
   * canvas outside the document gets the platform default (heavier on macOS than
   * the page's antialiased text): keep the sheet's canvas, hidden, in the lens.
   */
  private adopt(c: CanvasRenderingContext2D): void {
    const cv = c.canvas;
    if (!(cv instanceof HTMLCanvasElement) || cv.parentNode === this.root) return;
    cv.hidden = true;
    cv.style.display = 'none';
    cv.setAttribute('aria-hidden', 'true');
    this.root.append(cv);
    this.sheetCanvas = cv;
  }

  private paintSpineNow = (c: CanvasRenderingContext2D): void => {
    const l = this.paintL;
    if (!l) return;
    this.adopt(c);
    this.paintedNow++;
    paintSpine(c, l.item, l.look, { tw: this.paintTW, H: this.m.H, W: this.m.W }, this.paintK, this.theme.fonts);
  };

  private paintBackNow = (c: CanvasRenderingContext2D, w: number, h: number): void => {
    const l = this.paintL;
    if (!l) return;
    this.adopt(c);
    this.paintedNow++;
    paintBack(c, l.item, l.look, this.m.D, this.m.H, w, h, this.theme.fonts);
  };

  /**
   * The spine's sprite, painting it if this frame's budget allows (null: draw its
   * colours). It is painted at the size it shows on the shelf (a tape set back a
   * little is a hair smaller), so at rest one texel is one device pixel.
   */
  private spineSprite(l: TapeLook, tw: number, budget: number): Sprite | null {
    const sheet = this.sheet;
    if (!sheet) return null;
    const H = this.m.H;
    const persp = this.m.persp;
    const k = Math.round(((this.dpr * persp) / (persp - l.z)) * 1e4) / 1e4;
    const f = `${tw}|${H}|${k}|${this.fontGen}`;
    if (l.keyFor !== f) {
      l.key = spineKey(l.item, l.look, { tw, H, W: this.m.W }, k, this.fontGen);
      l.keyFor = f;
    }
    this.paintL = l;
    this.paintTW = tw;
    this.paintK = k;
    const w = tw * k;
    const h = H * k;
    const sp = sheet.get(l.key, this.paintSpineNow, w, h, budget);
    if (sp) {
      l.shown = l.key;
      return sp;
    }
    if (budget > 0) this.paintPending = true;
    if (l.shown) {
      const old = sheet.get(l.shown, this.paintSpineNow, w, h, 0);
      if (old) return old;
      l.shown = null;
    }
    return null;
  }

  private backSprite(l: TapeLook): Sprite | null {
    const sheet = this.sheet;
    if (!sheet) return null;
    const f = `${this.m.D}|${this.m.H}|${this.dpr}|${this.fontGen}`;
    if (l.backFor !== f) {
      l.back = backKey(l.item, l.look, this.m.D, this.m.H, this.dpr, this.fontGen);
      l.backFor = f;
    }
    this.paintL = l;
    const sp = sheet.get(l.back, this.paintBackNow, sheet.cellW, sheet.cellH, 2);
    if (!sp) this.paintPending = true;
    return sp;
  }

  /** A gradient of the shelf's (board, lip, bookends, light, shade): painted once per theme and size. */
  private fx(key: string, paint: (c: CanvasRenderingContext2D, w: number, h: number) => void, w?: number, h?: number): Sprite | null {
    const sp = this.sheet?.get(key, paint, w, h, 64) ?? null;
    if (!sp) this.paintPending = true;
    return sp;
  }

  private shadeSprite(): Sprite | null {
    const t = this.theme;
    return this.fx(`fx|shade|${t.shade1}|${t.shade}`, (c, w, h) =>
      paintRamp(c, w, h, 'h', [
        [0, t.shade1],
        [1, t.shade],
      ]),
    );
  }

  // ── frames ──

  private invalidate = (): void => {
    if (!this.raf && !this.dead && !this.dom) this.raf = requestAnimationFrame(this.frame);
  };

  private frame = (): void => {
    this.raf = 0;
    if (this.dead || this.dom || !this.sheet) return;
    const now = clock();
    const dt = clamp(now - this.lastT, 0, 0.1);
    this.lastT = now;
    if (!this.mini && Math.min(window.devicePixelRatio || 1, 3) !== this.dpr) {
      this.measure();
      this.relayout(null);
    }
    const scrolling = this.stepScroll(dt);
    const posters = this.posters;
    // Upload first: a slot drawn in the last frame is still "in use" and can't be evicted for a new one.
    posters.upload();
    posters.begin();
    this.sheet.begin();
    this.paintedNow = 0;
    // Spines come in fast on a fresh shelf, a few per frame while it scrolls.
    this.budget = now - this.mountedAt < 0.8 ? 24 : 8;
    const gen = this.text.gen;
    this.build(now);
    if (this.text.gen !== gen) this.build(now);
    if (scrolling || now - this.mountedAt < 1.5) this.prefetch();
    this.text.flush();
    const sheet = this.sheet;
    this.renderer.draw(
      this.batch,
      { width: this.viewW, height: this.viewH, persp: this.m.persp, originX: this.ox, originY: this.oy, dpr: this.dpr, time: now },
      { posters: posters.views(), glyphs: this.text.view, sprites: sheet.view, key: `${posters.version}|${this.sheetSerial}.${sheet.version}` },
    );
    posters.pump();
    if (performance.now() - this.lastTrim > 5000) {
      this.lastTrim = performance.now();
      posters.trim();
    }
    const moving = this.settle(now);
    // Painting that ran out of budget goes on next frame (unless nothing could be painted at all).
    const painting = this.paintPending && this.paintedNow > 0;
    if ((scrolling || moving || painting || posters.pending || now < posters.fadeUntil) && !this.paused) this.invalidate();
  };

  /** Smooth scrolling towards the target (shelf.ts: 15% of the way per frame at 120 Hz). */
  private stepScroll(dt: number): boolean {
    const d = this.targetX - this.scrollX;
    if (d === 0) return false;
    if (Math.abs(d) < 0.25) this.scrollX = this.targetX;
    else this.scrollX += d * (1 - Math.pow(0.85, dt * 120));
    this.scrolled();
    return this.scrollX !== this.targetX;
  }

  /** Drop finished animations; true while any is still running. */
  private settle(now: number): boolean {
    let moving = false;
    for (const [id, a] of this.anims) {
      const done = (a.dur <= 0 || now >= a.start + a.dur) && a.lit.done(now) && a.shadeL.done(now) && a.shadeR.done(now) && a.sheen.done(now);
      if (!done) moving = true;
      else if (a.stage === 'rest' && a.lit.to === 0) this.anims.delete(id);
    }
    for (const [id, s] of this.slides) {
      if (now >= s.start + SLIDE_S) this.slides.delete(id);
      else moving = true;
    }
    for (const [id, start] of this.enters) {
      if (now >= start + ENTER_S) this.enters.delete(id);
      else moving = true;
    }
    if (this.ghosts.length) {
      this.ghosts = this.ghosts.filter((g) => now < g.start + SINK_S);
      if (this.ghosts.length) moving = true;
    }
    if (this.extraFrom) {
      if (now >= this.extraStart + EXTRA_S) this.extraFrom = null;
      else moving = true;
    }
    return moving;
  }

  // ── the scene ──

  /** Bays with anything between world x `lo` and `hi`. */
  private baysIn(lo: number, hi: number): [number, number] {
    const { lay, p } = this;
    const first = clamp(Math.floor((lo - lay.x0) / lay.stride), 0, p.bays - 1);
    const last = clamp(Math.floor((hi - lay.x0) / lay.stride), 0, p.bays - 1);
    return [first, last];
  }

  /** Where tape `i` is drawn now (a search slides it, a new one drops in), into dx/dy/dz/da. */
  private placeTape(i: number, l: TapeLook, now: number): void {
    const id = l.item.id;
    let x = this.X[i];
    let y = this.Y[i];
    let z = l.z;
    let a = 1;
    const s = this.slides.get(id);
    if (s) {
      const e = soft(clamp((now - s.start) / SLIDE_S, 0, 1));
      x = lerp(s.x, x, e);
      y = lerp(s.y, y, e);
    }
    const start = this.enters.get(id);
    if (start !== undefined) {
      const e = soft(clamp((now - start) / ENTER_S, 0, 1));
      y -= 18 * (1 - e);
      z -= 40 * (1 - e);
      a = e;
    }
    this.dx[i] = x - this.scrollX;
    this.dy[i] = y;
    this.dz[i] = z;
    this.da[i] = a;
  }

  /** An extra (board, bookend, label) sliding to a new layout after a search. */
  private ext(key: string, x: number, y: number, w: number, e: number): Ext {
    const f = this.extraFrom?.get(key);
    const v = f ? { x: lerp(f.x, x, e), y: lerp(f.y, y, e), w: lerp(f.w, w, e) } : { x, y, w };
    this.extrasNow.set(key, v);
    return v;
  }

  private shelfRows(fb: number, lb: number, now: number): ShelfRow[] {
    const { m, p, lay } = this;
    const e = this.extraFrom ? springSoft(clamp((now - this.extraStart) / EXTRA_S, 0, 1)) : 1;
    const minLen = this.mini ? 0 : m.W * 7;
    const out: ShelfRow[] = [];
    for (let b = fb; b <= lb; b++) {
      const bx = lay.x0 + b * lay.stride;
      const used = p.oneBay ? p.bayLen[0] : b < p.bays - 1 ? m.bayW : Math.max(p.bayLen[b], p.bays > 1 ? m.bayW : 0);
      const len = Math.max(used, minLen);
      const bxb = bx - (len - used) / 2;
      for (let r = 0; r < p.rows; r++) {
        const top = this.tapeTop(r);
        const by = top + m.H;
        const board = this.ext(`b${b}r${r}`, bxb - m.over, by, len + 2 * m.over, e);
        const rl = p.rowLen[b]?.[r];
        const has = rl !== undefined && rl > 0;
        const endL = has ? this.ext(`eL${b}r${r}`, bx - m.endGap - m.ew, by - m.H * 0.58, 0, e) : null;
        const endR = has ? this.ext(`eR${b}r${r}`, bx + rl + m.endGap, by - m.H * 0.58, 0, e) : null;
        const key = b * p.rows + r;
        out.push({
          b,
          r,
          x: board.x,
          w: board.w,
          by: board.y,
          top: board.y - m.H,
          endL: endL ? endL.x : null,
          endR: endR ? endR.x : null,
          endY: endL ? endL.y : by - m.H * 0.58,
          i0: this.rowFirst[key] ?? 0,
          i1: this.rowFirst[key + 1] ?? 0,
        });
      }
    }
    return out;
  }

  private build(now: number): void {
    const b = this.batch;
    b.reset();
    this.paintPending = false;
    this.extrasNow.clear();
    const { m, oy } = this;
    if (!this.sheet) return;
    const sx = this.scrollX;
    const vw = this.viewW;
    // What is drawn: the tapes on screen and a little beyond (their sides reach in from the edges).
    const reflow = this.slides.size > 0 || this.enters.size > 0;
    const spill = reflow ? Math.max(vw * 0.35, m.W * 10) : m.D * 0.7 + m.W * 2;
    const lo = sx - spill;
    const hi = sx + vw + spill;
    const [fb, lb] = this.baysIn(lo, hi);
    const rows = this.shelfRows(fb, lb, now);

    // Where every tape in range is, and the tapes that are off the shelf.
    const drawn = this.drawn;
    drawn.length = 0;
    for (const row of rows) {
      for (let i = row.i0; i < row.i1; i++) {
        if (this.X[i] + this.widths[i] < lo || this.X[i] > hi) continue;
        this.placeTape(i, this.lookOf(this.items[i]), now);
        drawn.push(i);
      }
    }
    this.collectActive(now, lo, hi);

    // 1. The light on the wall, the boards' tops (seen from above).
    const glow = this.fx(`fx|glow|${this.theme.light}`, (c, w, h) => paintGlow(c, w, h, this.theme.light));
    for (const row of rows) {
      if (glow) {
        const gh = m.head + m.H * 0.7;
        const o = b.add(row.x + row.w / 2 - sx, row.top - m.head + gh / 2, -m.D - 8, row.w, gh, KIND3D.sprite);
        this.sprite(o, glow, 1);
      }
    }
    for (const row of rows) if (oy < row.by) this.boardTop(row, sx);
    // 2. Tapes leaving the results, sinking back into the shelf.
    for (const g of this.ghosts) this.ghost(g, now);
    // 3. Sides and tops, row by row, in the order they hide each other: left of the
    //    eye from left to right (each tape hides the one before's right side), right of it from right to left.
    for (const row of rows) this.sides(row, lo, hi);
    // 4. The spines on the shelf and the bookends' faces. A still shelf puts each spine
    //    on whole device pixels at exactly its size, so its sprite is drawn texel for pixel.
    const still = this.targetX === this.scrollX && !this.down?.drag && !reflow;
    const { ox, dpr } = this;
    const persp = m.persp;
    for (const row of rows) {
      for (let i = row.i0; i < row.i1; i++) {
        if (this.X[i] + this.widths[i] < lo || this.X[i] > hi) continue;
        const id = this.items[i].id;
        if (this.actById.has(id) || id === this.hiddenId) continue;
        const l = this.lookOf(this.items[i]);
        const tw = this.widths[i];
        const z = this.dz[i];
        let x = this.dx[i];
        let y = this.dy[i];
        if (still) {
          // Its top-left corner on a whole device pixel (the same spot to a fraction of one).
          const w = (persp - z) / persp;
          x = ox + (Math.round((ox + (x - ox) / w) * dpr) / dpr - ox) * w;
          y = oy + (Math.round((oy + (y - oy) / w) * dpr) / dpr - oy) * w;
        }
        const f = setFace(this.fSpine, x + tw / 2, y + m.H / 2, z, tw, m.H, 0, 0, I3);
        this.spine(l, f, tw, this.da[i], l.item.available ? 1 : 0.62, false);
      }
      this.endFaces(row, sx);
    }
    // 5. The boards' lips and the section labels on them.
    for (const row of rows) this.lip(row, sx);
    this.tags(fb, lb, now);
    // 6. Tapes off the shelf, back to front.
    for (const t of this.act) this.activeTape(t);
  }

  /** Placed faces of the tapes that are off the shelf (or easing back onto it), sorted back to front. */
  private collectActive(now: number, lo: number, hi: number): void {
    const act = this.act;
    act.length = 0;
    this.actById.clear();
    const { m, ox, oy } = this;
    const persp = m.persp;
    for (const [id, a] of this.anims) {
      const i = this.index.get(id);
      if (i === undefined) continue;
      const tw = this.widths[i];
      if (this.X[i] + tw < lo - m.D || this.X[i] > hi + m.D) continue;
      const l = this.lookOf(this.items[i]);
      this.placeTape(i, l, now);
      const pose = poseAt(a, now);
      const F = this.facesFor(tw);
      const at: [number, number, number] = [this.dx[i], this.dy[i], this.dz[i]];
      const put = (f: TapeFaces['spine']) => {
        const q = place(f, at, pose, F.origin);
        return facing(q, ox, oy, persp) ? q : null;
      };
      const right = put(F.right);
      const left = put(F.left);
      const t: Active = {
        i,
        id,
        l,
        tw,
        a: this.da[i],
        depth: this.dz[i] + pose.tz,
        lit: a.lit.value(now),
        shadeL: a.shadeL.value(now),
        shadeR: a.shadeR.value(now),
        sheen: a.sheen.value(now),
        ring: id === this.focusId && !this.mini,
        spine: put(F.spine),
        top: put(F.top),
        right,
        left,
        splitR: right ? splitAtDepth(right, m.D, m.H, Z0) : null,
        splitL: left ? splitAtDepth(left, m.D, m.H, Z0) : null,
      };
      act.push(t);
      this.actById.set(id, t);
    }
    act.sort((p, q) => p.depth - q.depth);
  }

  /** Sides and tops of one row (see build, step 3), and its bookends' sides. */
  private sides(row: ShelfRow, lo: number, hi: number): void {
    const { m, ox } = this;
    let a = row.i0;
    let z = row.i1;
    while (a < z && this.X[a] + this.widths[a] < lo) a++;
    while (z > a && this.X[z - 1] > hi) z--;
    // Left of the eye: right sides show, left to right.
    let k = a;
    for (; k < z; k++) {
      if (this.dx[k] + this.widths[k] > ox) break;
      this.sideAndTop(k, 'right', k === row.i1 - 1 || this.X[k + 1] - this.X[k] - this.widths[k] > m.gap + 0.5);
    }
    if (row.endR !== null && row.endR + m.ew - this.scrollX < ox) this.endSide(row, 'right');
    // Right of the eye: left sides show, right to left.
    let j = z - 1;
    for (; j >= k; j--) {
      if (this.dx[j] < ox) break;
      this.sideAndTop(j, 'left', j === row.i0 || this.X[j] - this.X[j - 1] - this.widths[j - 1] > m.gap + 0.5);
    }
    // Straddling the eye: neither side shows.
    for (let q = k; q <= j; q++) this.sideAndTop(q, null, false);
    if (row.endL !== null && row.endL - this.scrollX > ox) this.endSide(row, 'left');
  }

  private sideAndTop(i: number, side: 'right' | 'left' | null, exposed: boolean): void {
    const { m, oy } = this;
    const item = this.items[i];
    const l = this.lookOf(item);
    const a = this.da[i];
    const t = this.actById.get(item.id);
    if (t) {
      // Off the shelf: the part of its sides still inside the shelf goes in with the shelf.
      if (t.right && t.splitR?.back) this.sleeve(t.l, t.right, t.splitR.back, t.a, t.shadeR, 0, true);
      if (t.left && t.splitL?.back) this.backCover(t.l, t.left, t.splitL.back, t.a, t.shadeL, true);
      return;
    }
    const tw = this.widths[i];
    const x = this.dx[i];
    const y = this.dy[i];
    const z = this.dz[i];
    const half = m.D / 2;
    if (side === 'right') {
      const f = setFace(this.fSide, x + tw, y + m.H / 2, z - half, m.D, m.H, 0, Math.PI / 2, RY_RIGHT);
      this.sleeve(l, f, [-half, half], a, 1, 0, exposed);
    } else if (side === 'left') {
      const f = setFace(this.fSide, x, y + m.H / 2, z - half, m.D, m.H, 0, -Math.PI / 2, RY_LEFT);
      this.backCover(l, f, [-half, half], a, 1, exposed);
    }
    if (oy < y) this.top(l, setFace(this.fTop, x + tw / 2, y, z - half, tw, m.D, Math.PI / 2, 0, RX_TOP), tw, a);
  }

  /** The tape that is off the shelf: the front of its sides, its top and its spine (back faces culled). */
  private activeTape(t: Active): void {
    const off = !t.l.item.available;
    if (t.right && t.splitR?.front) this.sleeve(t.l, t.right, t.splitR.front, t.a, t.shadeR, t.sheen, true);
    if (t.left && t.splitL?.front) this.backCover(t.l, t.left, t.splitL.front, t.a, t.shadeL, true);
    if (t.top) this.top(t.l, t.top, t.tw, t.a);
    const hidden = this.hiddenId === t.id && this.openId !== t.id;
    if (t.spine && !hidden) this.spine(t.l, t.spine, t.tw, t.a, off ? lerp(0.62, 0.8, t.lit) : lerp(1, 1.14, t.lit), t.ring);
  }

  // ── faces ──

  /** A quad on a placed face: centred at (u, v) from the face's centre, w×h, turned with the face. */
  private on(f: Placed, u: number, v: number, w: number, h: number, kind: number, radius = 0): number {
    const s = f.s;
    let x = f.x;
    let y = f.y;
    let z = f.z;
    if (u !== 0 || v !== 0) {
      const q = f.m;
      x += (q[0] * u + q[1] * v) * s;
      y += (q[3] * u + q[4] * v) * s;
      z += (q[6] * u + q[7] * v) * s;
    }
    const o = this.batch.add(x, y, z, w * s, h * s, kind, radius * s);
    if (f.rx !== 0 || f.ry !== 0 || f.rz !== 0) this.batch.rotate(o, f.rx, f.ry, f.rz);
    return o;
  }

  private sprite(o: number, sp: Sprite, a: number, u0 = 0, u1 = 1): void {
    const b = this.batch;
    b.color(o, WHITE);
    b.opacity(o, a);
    b.uv(o, u0 * sp.u1, 0, u1 * sp.u1, sp.v1);
    b.texture(o, sp.layer, 0, 0);
  }

  private spine(l: TapeLook, f: Placed, tw: number, a: number, tint: number, ring: boolean): void {
    const b = this.batch;
    const H = this.m.H;
    const radius = l.look.style === 'clamw' || l.look.style === 'clamb' ? 2.5 : 1.5;
    const sp = this.spineSprite(l, tw, this.budget);
    const o = this.on(f, 0, 0, tw, H, sp ? KIND3D.sprite : KIND3D.poster, radius);
    b.opacity(o, a);
    if (sp) {
      const c = this.tint;
      c[0] = tint;
      c[1] = tint;
      c[2] = tint;
      b.color(o, c);
      b.uv(o, 0, 0, sp.u1, sp.v1);
      b.texture(o, sp.layer, 0, 0);
    } else {
      // Its colours until the sprite is painted.
      b.color(o, [l.bg[0] * tint, l.bg[1] * tint, l.bg[2] * tint, 1]);
      b.uv(o, l.bg2[0] * tint, l.bg2[1] * tint, l.bg2[2] * tint, 1);
      b.flags(o, FLAG3D.gradient);
    }
    if (ring) {
      // outline: 2px solid (focus at 60%), outline-offset: 2px
      const r = this.on(f, 0, 0, tw + 8, H + 8, KIND3D.ring, radius + 4);
      b.color(r, this.theme.focus, 0.6 * a);
      b.extra(r, 2);
    }
  }

  private top(l: TapeLook, f: Placed, tw: number, a: number): void {
    const b = this.batch;
    const o = this.on(f, 0, 0, tw, this.m.D, KIND3D.poster, 1.5);
    b.opacity(o, a);
    // linear-gradient(to top, top, 55% top in black): light at the front edge.
    b.color(o, l.topDark);
    b.uv(o, l.top[0], l.top[1], l.top[2], 1);
    b.flags(o, FLAG3D.gradient);
  }

  /** The sleeve (the tape's right side): its poster once wanted, marks, the shade, and the light when turned. */
  private sleeve(l: TapeLook, f: Placed, span: Span, a: number, shade: number, sheen: number, full: boolean): void {
    const b = this.batch;
    const { D, H } = this.m;
    const [ua, ub] = span;
    const whole = ua <= -D / 2 + 0.01 && ub >= D / 2 - 0.01;
    // The two parts of a split face overlap a pixel, so no seam shows between them.
    const fa = whole ? ua : ua - 0.75;
    const fb = whole ? ub : ub + 0.75;
    const uc = (fa + fb) / 2;
    const sw = fb - fa;
    const t0 = clamp((fa + D / 2) / D, 0, 1);
    const t1 = clamp((fb + D / 2) / D, 0, 1);
    const item = l.item;
    const id = item.id;
    const o = this.on(f, uc, 0, sw, H, KIND3D.poster, whole ? 1.5 : 0);
    b.opacity(o, a);
    const hidden = this.hiddenId === id && this.openId === id;
    let flags = whole && !hidden ? FLAG3D.hairline : 0;
    if (hidden) {
      // Its poster is flying to the detail page: the bare box.
      b.color(o, l.fbg2);
    } else {
      const art = item.poster;
      const hit = art && this.wanted.has(id) ? this.posters.get(art, this.posterCls, id === this.eagerId ? -1e6 : id === this.hoverId || id === this.focusId ? 0 : 1000, true) : null;
      b.color(o, l.paper);
      if (hit) {
        b.uv(o, this.crop0 + t0 * this.crop, 0, this.crop0 + t1 * this.crop, 1);
        b.texture(o, hit.code, hit.texWidth, hit.readyAt);
        if (!item.available) flags |= FLAG3D.gray;
      } else if (l.paper2) {
        b.uv(o, l.paper2[0], l.paper2[1], l.paper2[2], 1);
        flags |= FLAG3D.gradient;
      }
    }
    b.flags(o, flags);
    if (full && !hidden) this.marks(item, f, ua, ub, a);
    if (shade > 0.002) {
      const s = this.shadeSprite();
      if (s) this.sprite(this.on(f, uc, 0, sw, H, KIND3D.sprite), s, shade * a, t0, t1);
    }
    if (sheen > 0.002) {
      const s = this.fx(`fx|sheen|${D}|${H}`, (c, w, h) => paintSheen(c, w, h, D, H));
      if (s) this.sprite(this.on(f, uc, 0, sw, H, KIND3D.sprite), s, sheen * a, t0, t1);
    }
  }

  /** The back cover (the tape's left side): its colours, the words on it where it shows, and the shade. */
  private backCover(l: TapeLook, f: Placed, span: Span, a: number, shade: number, full: boolean): void {
    const b = this.batch;
    const { D, H } = this.m;
    const [ua, ub] = span;
    const whole = ua <= -D / 2 + 0.01 && ub >= D / 2 - 0.01;
    const fa = whole ? ua : ua - 0.75;
    const fb = whole ? ub : ub + 0.75;
    const uc = (fa + fb) / 2;
    const sw = fb - fa;
    const t0 = clamp((fa + D / 2) / D, 0, 1);
    const t1 = clamp((fb + D / 2) / D, 0, 1);
    const o = this.on(f, uc, 0, sw, H, KIND3D.poster, whole ? 1.5 : 0);
    b.opacity(o, a);
    b.color(o, l.fbg);
    b.uv(o, l.fbg2[0], l.fbg2[1], l.fbg2[2], 1);
    b.flags(o, FLAG3D.gradient);
    if (full) {
      const s = this.backSprite(l);
      if (s) this.sprite(this.on(f, uc, 0, sw, H, KIND3D.sprite), s, a, t0, t1);
    }
    if (shade > 0.002) {
      const s = this.shadeSprite();
      if (s) this.sprite(this.on(f, uc, 0, sw, H, KIND3D.sprite), s, shade * a, t0, t1);
    }
  }

  private glyphOn(f: Placed, g: Glyph, x: number, y: number, color: RGBA, a: number): void {
    // (x, y): the glyph quad's top-left, from the face's top-left corner.
    const { D, H } = this.m;
    const o = this.on(f, x + g.w / 2 - D / 2, y + g.h / 2 - H / 2, g.w, g.h, KIND3D.text);
    this.batch.color(o, color, a);
    this.batch.uv(o, g.u0, g.v0, g.u1, g.v1);
  }

  /** A box on the sleeve, from the face's top-left corner (CSS-style). */
  private boxOn(f: Placed, x: number, y: number, w: number, h: number, kind: number, radius: number): number {
    const { D, H } = this.m;
    return this.on(f, x + w / 2 - D / 2, y + h / 2 - H / 2, w, h, kind, radius);
  }

  /** Progress, watched, new and offline marks and the frame/AI label, as art.ts puts them on the sleeve. */
  private marks(item: ItemSummary, f: Placed, ua: number, ub: number, a: number): void {
    const b = this.batch;
    const dpr = this.dpr;
    const { D, H } = this.m;
    // Only what lies in this part of the face (u from its centre).
    const here = (x: number) => x - D / 2 >= ua && x - D / 2 < ub;
    const mono: TextStyle = { family: this.theme.fonts.mono, size: 9.28, weight: 500, spacing: 0.08, upper: true };
    const pillH = 15.6;
    const label = item.poster ? sourceLabel(item.poster) : null;
    if (label) {
      const ai = label.tone === 'ai';
      const g = this.text.get(label.short, mono, 200, dpr);
      const icon = ai ? this.text.icon('spark', 9, dpr) : null;
      if (g) {
        const w = 12 + g.w - g.pad * 2 + (icon ? 13 : 0);
        const x = 6;
        const y = 6;
        if (here(x + w / 2)) {
          const p = this.boxOn(f, x, y, w, pillH, KIND3D.solid, 4);
          b.color(p, PILL, (ai ? 0.86 : 0.72) * a);
          if (ai) {
            const ring = this.boxOn(f, x, y, w, pillH, KIND3D.ring, 4);
            b.color(ring, WHITE, 0.55 * a);
            b.extra(ring, 1);
          }
          let tx = x + 6;
          if (icon) {
            this.glyphOn(f, icon, tx - icon.pad, y + (pillH - icon.h) / 2, WHITE, a);
            tx += 13;
          }
          this.glyphOn(f, g, tx - g.pad, y + pillH / 2 + 3.2 - g.baseline, ai ? WHITE : INK, a);
        }
      }
    }
    const prog = progressFraction(item);
    if (prog !== null) {
      const tx = D * 0.08;
      const tw = D * 0.84;
      const ty = H - 10;
      if (here(tx + tw / 2)) {
        const track = this.boxOn(f, tx, ty, tw, 3, KIND3D.solid, 1.5);
        b.color(track, WHITE, 0.3 * a);
        const fw = tw * Math.max(0.03, prog);
        const fill = this.boxOn(f, tx, ty, fw, 3, KIND3D.solid, 1.5);
        b.color(fill, WHITE, a);
      }
    } else if (isWatchedItem(item)) {
      const x = D - 26;
      const y = 6;
      if (here(x + 10)) {
        const c = this.boxOn(f, x, y, 20, 20, KIND3D.solid, 10);
        b.color(c, PILL, 0.6 * a);
        const icon = this.text.icon('check', 11, dpr);
        if (icon) this.glyphOn(f, icon, x + 10 - icon.w / 2, y + 10 - icon.h / 2, WHITE, a);
      }
    } else if (isNew(item)) {
      const x = D - 16;
      const y = 8;
      if (here(x + 4)) {
        const glow = this.boxOn(f, x - 12, y - 12, 32, 32, KIND3D.shadow, 4);
        b.color(glow, WHITE, 0.8 * a);
        b.extra(glow, 12);
        const ring = this.boxOn(f, x - 3, y - 3, 14, 14, KIND3D.solid, 7);
        b.color(ring, BLACK, 0.25 * a);
        const dot = this.boxOn(f, x, y, 8, 8, KIND3D.solid, 4);
        b.color(dot, WHITE, a);
      }
    }
    if (!item.available) {
      const g = this.text.get('Offline', { ...mono, weight: 400 }, 200, dpr);
      if (g) {
        const w = 12 + g.w - g.pad * 2;
        const x = 6;
        const y = H - 6 - pillH;
        if (here(x + w / 2)) {
          const p = this.boxOn(f, x, y, w, pillH, KIND3D.solid, 4);
          b.color(p, PILL, 0.75 * a);
          this.glyphOn(f, g, x + 6 - g.pad, y + pillH / 2 + 3.2 - g.baseline, INK, a);
        }
      }
    }
  }

  // ── the shelf ──

  private boardTop(row: ShelfRow, sx: number): void {
    const { m, theme: t } = this;
    const s = this.fx(`fx|top|${t.top1}|${t.top2}`, (c, w, h) =>
      paintRamp(c, w, h, 'v', [
        [0, t.top2],
        [0.12, t.top2],
        [1, t.top1],
      ]),
    );
    if (!s) return;
    const o = this.batch.add(row.x + row.w / 2 - sx, row.by, m.lip - m.bd / 2, row.w, m.bd, KIND3D.sprite);
    this.batch.rotate(o, Math.PI / 2, 0, 0);
    this.sprite(o, s, 1);
  }

  private lip(row: ShelfRow, sx: number): void {
    const { m, theme: t, sheet } = this;
    if (!sheet) return;
    const len = Math.max(1, Math.round(m.bh * this.dpr));
    const s = this.fx(`fx|edge|${t.hi}|${t.edge1}|${t.edge2}|${len}`, (c) => paintEdge(c, sheet.cellW, sheet.cellH, 'v', len, this.dpr, t.hi, t.edge1, t.edge2), sheet.cellW, len);
    if (!s) return;
    const o = this.batch.add(row.x + row.w / 2 - sx, row.by + m.bh / 2, m.lip, row.w, m.bh, KIND3D.sprite, 1.5);
    this.sprite(o, s, 1);
  }

  private endFaces(row: ShelfRow, sx: number): void {
    const { m, theme: t, sheet } = this;
    if (!sheet || row.endL === null || row.endR === null) return;
    const len = Math.max(1, Math.round(m.ew * this.dpr));
    const s = this.fx(`fx|endf|${t.hi}|${t.end1}|${t.end2}|${len}`, (c) => paintEdge(c, sheet.cellW, sheet.cellH, 'h', len, this.dpr, t.hi, t.end1, t.end2), len, sheet.cellH);
    if (!s) return;
    const h = m.H * 0.58;
    for (const x of [row.endL, row.endR]) {
      const o = this.batch.add(x + m.ew / 2 - sx, row.endY + h / 2, 0, m.ew, h, KIND3D.sprite, 1);
      this.sprite(o, s, 1);
    }
  }

  private endSide(row: ShelfRow, side: 'left' | 'right'): void {
    const { m, theme: t } = this;
    const s = this.fx(`fx|ends|${t.end1}|${t.end2}`, (c, w, h) =>
      paintRamp(c, w, h, 'h', [
        [0, t.end1],
        [1, t.end2],
      ]),
    );
    if (!s) return;
    const h = m.H * 0.58;
    const d = m.D * 0.78;
    const x = side === 'left' ? (row.endL ?? 0) : (row.endR ?? 0) + m.ew;
    const o = this.batch.add(x - this.scrollX, row.endY + h / 2, -d / 2, d, h, KIND3D.sprite);
    this.batch.rotate(o, 0, side === 'left' ? -Math.PI / 2 : Math.PI / 2, 0);
    this.sprite(o, s, 1);
  }

  /** Section labels on the boards' lips. */
  private tags(fb: number, lb: number, now: number): void {
    const { m, p, lay, theme: t } = this;
    if (!p.tags.length) return;
    const b = this.batch;
    const e = this.extraFrom ? springSoft(clamp((now - this.extraStart) / EXTRA_S, 0, 1)) : 1;
    const fs = Math.max(9, m.W * 0.26);
    const style: TextStyle = { family: t.fonts.mono, size: Math.round(fs * 4) / 4, weight: 400, spacing: 0.03 };
    const z = m.lip + 1;
    for (const tag of p.tags) {
      if (tag.bay < fb || tag.bay > lb) continue;
      const pos = this.ext(`t:${tag.label}@${tag.bay}.${tag.row}`, lay.x0 + tag.bay * lay.stride + tag.x, this.tapeTop(tag.row) + m.H + 3, 0, e);
      const g = this.text.get(tag.label, style, 400, this.dpr);
      if (!g) continue;
      const w = g.w - g.pad * 2 + 11;
      const h = fs * 1.25 + 3;
      const cx = pos.x - this.scrollX + w / 2;
      const cy = pos.y + h / 2;
      // box-shadow: 0 0 0 .5px rgb(0 0 0 / .14), 0 1px 2px rgb(0 0 0 / .28)
      const sh = b.add(cx, cy + 1, z, w + 4, h + 4, KIND3D.shadow, 2);
      b.color(sh, BLACK, 0.28);
      b.extra(sh, 2);
      const ring = b.add(cx, cy, z, w + 1, h + 1, KIND3D.ring, 2.5);
      b.color(ring, BLACK, 0.14);
      b.extra(ring, 0.5);
      const pill = b.add(cx, cy, z, w, h, KIND3D.solid, 2);
      b.color(pill, t.tagBg);
      const base = pos.y + 2 + fs * 0.625 + fs * 0.35;
      const go = b.add(pos.x - this.scrollX + 5 - g.pad + g.w / 2, base - g.baseline + g.h / 2, z, g.w, g.h, KIND3D.text);
      b.color(go, t.tagInk);
      b.uv(go, g.u0, g.v0, g.u1, g.v1);
    }
  }

  private ghost(g: Ghost, now: number): void {
    const { m, oy } = this;
    const e = EASE_IN(clamp((now - g.start) / SINK_S, 0, 1));
    const a = 1 - e;
    if (a <= 0.002) return;
    const x = g.x - this.scrollX;
    const z = g.z - 46 * e;
    if (oy < g.y) this.top(g.l, setFace(this.fTop, x + g.tw / 2, g.y, z - m.D / 2, g.tw, m.D, Math.PI / 2, 0, RX_TOP), g.tw, a);
    const k = (this.dpr * m.persp) / (m.persp - g.l.z);
    const sp = this.sheet?.get(g.l.shown ?? g.l.key, this.paintSpineNow, g.tw * k, m.H * k, 0) ?? null;
    const f = setFace(this.fSpine, x + g.tw / 2, g.y + m.H / 2, z, g.tw, m.H, 0, 0, I3);
    const o = this.on(f, 0, 0, g.tw, m.H, sp ? KIND3D.sprite : KIND3D.poster, 1.5);
    const tint = g.l.item.available ? 1 : 0.62;
    if (sp) {
      this.sprite(o, sp, a);
      this.batch.color(o, [tint, tint, tint, 1]);
    } else {
      this.batch.opacity(o, a);
      this.batch.color(o, g.l.bg);
      this.batch.uv(o, g.l.bg2[0], g.l.bg2[1], g.l.bg2[2], 1);
      this.batch.flags(o, FLAG3D.gradient);
    }
  }

  /** Spines just past the edge of the screen in the direction of travel, painted ahead of time. */
  private prefetch(): void {
    const sheet = this.sheet;
    if (!sheet || this.mini || this.paintedNow >= 3) return;
    const { m, p } = this;
    const margin = Math.max(this.viewW * 0.35, m.W * 10);
    const ahead = this.targetX >= this.scrollX;
    const lo = ahead ? this.scrollX + this.viewW : this.scrollX - margin;
    const hi = ahead ? this.scrollX + this.viewW + margin : this.scrollX;
    const [fb, lb] = this.baysIn(lo, hi);
    let n = 0;
    const cap = Math.max(0, this.sheetMax - this.drawn.length - 24);
    for (let bb = 0; bb <= lb - fb; bb++) {
      const bay = ahead ? fb + bb : lb - bb;
      const from = p.bayStart[bay];
      const to = p.bayStart[bay + 1];
      for (let k = 0; k < to - from; k++) {
        const i = ahead ? from + k : to - 1 - k;
        if (this.X[i] + this.widths[i] < lo || this.X[i] > hi) continue;
        if (++n > cap || this.paintedNow >= 3) return;
        this.spineSprite(this.lookOf(this.items[i]), this.widths[i], 3);
      }
    }
  }

  // ── the window: which tapes are near the view (state, the mirror) ──

  /** The shelf moved: the window follows; the mirror every so often, and once it comes to rest. */
  private scrolled(): void {
    this.syncWindow(false);
    if (performance.now() - this.mirrorAt > 300) this.syncMirror();
    if (this.settleT) clearTimeout(this.settleT);
    this.settleT = setTimeout(() => {
      this.settleT = null;
      if (this.dead || this.dom) return;
      this.syncMirror();
      // The pointer rests over whatever scrolled under it.
      if (this.pointer && !this.down?.drag) this.hoverAt(this.pointer.x, this.pointer.y);
    }, 140);
  }

  /** The tapes near the view, as the DOM Shelf keeps them; state of tapes far away is let go. */
  private syncWindow(force: boolean): void {
    const m = this.m;
    if (!force && Math.abs(this.scrollX - this.windowAt) < m.W * 4) return;
    this.windowAt = this.scrollX;
    const margin = Math.max(this.viewW * 0.35, m.W * 10);
    const lo = this.scrollX - margin;
    const hi = this.scrollX + this.viewW + margin;
    const [fb, lb] = this.baysIn(lo, hi);
    const ids = new Set<Id>();
    for (let b = fb; b <= lb; b++) {
      for (let i = this.p.bayStart[b]; i < this.p.bayStart[b + 1]; i++) {
        if (this.X[i] + this.widths[i] < lo || this.X[i] > hi) continue;
        ids.add(this.items[i].id);
      }
    }
    this.windowIds = ids;
    const keep = (id: Id) => ids.has(id) || id === this.openId || id === this.focusId || id === this.hoverId;
    for (const id of this.wanted) if (!keep(id)) this.wanted.delete(id);
    for (const id of this.outs) if (!keep(id)) this.outs.delete(id);
    for (const id of this.opened) if (!keep(id)) this.opened.delete(id);
    for (const id of this.anims.keys()) if (!keep(id)) this.anims.delete(id);
  }

  /**
   * The tapes in view as invisible buttons with the DOM Shelf's roles and labels:
   * screen readers can browse the shelf as they could the DOM one, and tests find
   * titles. Synced when the layout changes and every so often while scrolling, never every frame.
   */
  private syncMirror(): void {
    const track = this.track;
    if (this.mini || !track || this.dead || this.dom) return;
    this.mirrorAt = performance.now();
    const m = this.m;
    const lo = this.scrollX - m.W * 2;
    const hi = this.scrollX + this.viewW + m.W * 2;
    const [fb, lb] = this.baysIn(lo, hi);
    const seen = new Set<Id>();
    let prev: Element = this.focusEl ?? track.firstElementChild ?? track;
    for (let b = fb; b <= lb; b++) {
      for (let i = this.p.bayStart[b]; i < this.p.bayStart[b + 1]; i++) {
        if (this.X[i] + this.widths[i] < lo || this.X[i] > hi) continue;
        const item = this.items[i];
        let n = this.mirror.get(item.id);
        if (!n) {
          n = el('div', styles.ghost, { role: 'button', tabindex: '-1', 'data-id': String(item.id) });
          this.mirror.set(item.id, n);
        }
        const label = artLabel(item);
        if (n.getAttribute('aria-label') !== label) n.setAttribute('aria-label', label);
        n.style.transform = `translate(${this.X[i]}px, ${this.Y[i]}px)`;
        n.style.width = `${this.widths[i]}px`;
        n.style.height = `${m.H}px`;
        // In shelf order (the order a screen reader reads them in).
        if (prev === track ? track.firstElementChild !== n : prev.nextElementSibling !== n) {
          if (prev === track) track.prepend(n);
          else prev.after(n);
        }
        prev = n;
        seen.add(item.id);
      }
    }
    for (const [id, n] of this.mirror) {
      if (seen.has(id)) continue;
      n.remove();
      this.mirror.delete(id);
    }
    track.style.transform = `translate3d(${(-this.scrollX).toFixed(1)}px, 0, 0)`;
    this.syncTwin();
  }

  /** While the focus element stands on a tape, that tape's mirror steps aside (one tape, one button). */
  private syncTwin(): void {
    const f = this.focusEl;
    const id = f && !f.hidden ? Number(f.dataset.id) : null;
    const twin = id !== null ? (this.mirror.get(id) ?? null) : null;
    if (twin === this.twin) return;
    this.twin?.removeAttribute('aria-hidden');
    twin?.setAttribute('aria-hidden', 'true');
    this.twin = twin;
  }

  private placeFocus(id: Id): void {
    const f = this.focusEl;
    const i = this.index.get(id);
    if (!f || i === undefined) return;
    const item = this.items[i];
    f.hidden = false;
    f.dataset.id = String(id);
    f.setAttribute('aria-label', artLabel(item));
    f.style.transform = `translate(${this.X[i]}px, ${this.Y[i]}px)`;
    f.style.width = `${this.widths[i]}px`;
    f.style.height = `${this.m.H}px`;
    this.syncTwin();
  }

  // ── scrolling ──

  private scrollTo(x: number, immediate = false): void {
    this.targetX = clamp(x, 0, this.lay.maxX);
    if ((immediate || reducedMotion()) && this.scrollX !== this.targetX) {
      this.scrollX = this.targetX;
      this.scrolled();
    }
    this.invalidate();
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

  // ── tape states ──

  /** Move a tape to the pose its state asks for, the way the DOM Shelf's CSS transitions would. */
  private refresh(id: Id, instant = false): void {
    const open = this.opened.has(id);
    const out = this.outs.has(id);
    const hover = id === this.hoverId;
    const kf = id === this.focusId && !this.mini;
    const lit = hover || kf;
    // CSS: a hovered tape keeps its hover pose over `.out` (more specific), but not over `.out.open`.
    const stage: Stage = open ? 'open' : hover ? 'pull' : out ? 'out' : kf ? 'pull' : 'rest';
    let a = this.anims.get(id);
    if (!a) {
      if (stage === 'rest' && !lit) return;
      a = { stage: 'rest', from: REST, to: REST, start: 0, dur: 0, curve: spring, lit: new Ease(EASE_OUT, 0), shadeL: new Ease(EASE_OUT, 1), shadeR: new Ease(EASE_OUT, 1), sheen: new Ease(EASE_OUT, 0) };
      this.anims.set(id, a);
    }
    const now = clock();
    const still = instant || this.mini || reducedMotion();
    if (a.stage !== stage) {
      a.from = poseAt(a, now);
      a.to = this.poses[stage];
      a.start = now;
      // The transition is the one the new classes carry (shelf.module.css).
      a.dur = still ? 0 : open ? OPEN_S : out ? OUT_S : PULL_S;
      a.curve = !open && out ? OUT_CURVE : spring;
      a.stage = stage;
    }
    a.lit.set(lit ? 1 : 0, now, still ? 0 : LIT_S);
    a.shadeL.set(lit ? 0.55 : 1, now, still ? 0 : SHADE_S);
    // A hovered sleeve keeps its shade even turned (`.slot:hover .right::after` outranks `.open .right::after`).
    a.shadeR.set(hover ? 0.55 : open ? 0 : kf ? 0.55 : 1, now, still ? 0 : SHADE_S);
    a.sheen.set(open ? 1 : 0, now, still ? 0 : SHEEN_S, still ? 0 : SHEEN_DELAY);
    this.invalidate();
  }

  private setHover(id: Id | null): void {
    if (id === this.hoverId) return;
    const prev = this.hoverId;
    this.hoverId = id;
    if (prev !== null) this.refresh(prev);
    if (id !== null) this.refresh(id);
    this.canvas.classList.toggle(styles.pointing, id !== null);
    if (this.hoverT) clearTimeout(this.hoverT);
    this.hoverT = null;
    if (id === null) return;
    // A beat later (so passing over doesn't): light the room, warm the detail, load the sleeve.
    this.hoverT = setTimeout(() => {
      this.hoverT = null;
      const i = this.index.get(id);
      if (i === undefined || this.dead) return;
      this.ctx.setFocus(id);
      this.ctx.intent(this.items[i]);
      this.wanted.add(id);
      this.invalidate();
    }, 90);
  }

  private setKeyFocus(id: Id, instant = false): void {
    const i = this.index.get(id);
    if (i === undefined) return;
    const prev = this.focusId;
    this.focusId = id;
    this.ensureVisible(id, instant);
    if (prev !== null && prev !== id) this.refresh(prev);
    this.refresh(id);
    this.ctx.setFocus(id);
    this.ctx.intent(this.items[i]);
    this.wanted.add(id);
    this.placeFocus(id);
    const f = this.focusEl;
    if (f && document.activeElement !== f) f.focus({ preventScroll: true });
    this.invalidate();
  }

  private later(fn: () => void, ms: number): void {
    const t = setTimeout(() => {
      this.timers.delete(t);
      fn();
    }, ms);
    this.timers.add(t);
  }

  private openTape(id: Id): void {
    if (!this.index.has(id)) return;
    if (this.openId !== null && this.openId !== id) this.closeTape(this.openId);
    this.setKeyFocus(id);
    this.wanted.add(id);
    this.eagerId = id;
    this.openId = id;
    this.outs.add(id);
    this.refresh(id);
    if (reducedMotion()) {
      this.opened.add(id);
      this.refresh(id);
      this.ctx.open(id, null);
      return;
    }
    // Out first, then the turn, then the sleeve becomes the detail view.
    // Skip the hand-off if this lens has stopped being the one on screen (switched lens mid-turn).
    const live = () => !this.dead && this.openId === id && activeLens()?.view === this;
    this.later(() => {
      if (!live()) return;
      this.opened.add(id);
      this.refresh(id);
    }, 160);
    this.later(() => {
      if (live()) this.ctx.open(id, null);
    }, 640);
  }

  private closeTape(id: Id): void {
    if (this.openId === id) this.openId = null;
    this.opened.delete(id);
    this.refresh(id);
    this.later(
      () => {
        if (this.openId === id) return;
        this.outs.delete(id);
        this.refresh(id);
      },
      reducedMotion() ? 0 : 320,
    );
  }

  // ── input ──

  /** The tape under a point (view coordinates): tapes off the shelf first, then spines, then sides and tops. */
  private tapeAt(px: number, py: number): number {
    const { m, ox, oy } = this;
    const persp = m.persp;
    const cam = { persp, originX: ox, originY: oy };
    for (let k = this.act.length - 1; k >= 0; k--) {
      const t = this.act[k];
      for (const f of [t.spine, t.right, t.left, t.top]) {
        if (f && inside(px, py, corners(f, cam))) return t.i;
      }
    }
    let side = -1;
    for (const i of this.drawn) {
      if (this.actById.has(this.items[i].id)) continue;
      const x = this.dx[i];
      const y = this.dy[i];
      const z = this.dz[i];
      const tw = this.widths[i];
      const w = (persp - z) / persp;
      const x0 = ox + (x - ox) / w;
      const x1 = ox + (x + tw - ox) / w;
      const y0 = oy + (y - oy) / w;
      const y1 = oy + (y + m.H - oy) / w;
      if (px >= x0 && px < x1 && py >= y0 && py < y1) return i;
      if (side >= 0) continue;
      // The top (seen from above) and the side facing the eye.
      if (oy < y && py < y0 && py > oy) {
        const ww = (y - oy) / (py - oy);
        const zz = persp * (1 - ww);
        const xx = ox + (px - ox) * ww;
        if (zz <= z && zz >= z - m.D && xx >= x && xx < x + tw) side = i;
      }
      const xs = x + tw <= ox ? x + tw : x >= ox ? x : null;
      if (xs !== null && side < 0 && px !== ox) {
        const ww = (xs - ox) / (px - ox);
        const zz = persp * (1 - ww);
        const yy = oy + (py - oy) * ww;
        if (ww > 0 && zz <= z && zz >= z - m.D && yy >= y && yy < y + m.H) side = i;
      }
    }
    return side;
  }

  private hoverAt(px: number, py: number): void {
    if (this.down?.drag) return;
    const i = this.tapeAt(px, py);
    this.setHover(i >= 0 ? this.items[i].id : null);
  }

  private bind(): void {
    const c = this.canvas;
    const on = <K extends keyof HTMLElementEventMap>(t: HTMLElement, type: K, fn: (e: HTMLElementEventMap[K]) => void, opts?: AddEventListenerOptions) => {
      t.addEventListener(type, fn as EventListener, opts);
      this.cleanup.push(() => t.removeEventListener(type, fn as EventListener, opts));
    };
    on(
      c,
      'wheel',
      (e) => {
        if (e.ctrlKey || this.lay.maxX <= 0) return;
        const d = Math.abs(e.deltaX) > Math.abs(e.deltaY) ? e.deltaX : e.deltaY;
        if (!d) return;
        e.preventDefault();
        this.setHover(null);
        this.scrollTo(this.targetX + d * (e.deltaMode === 1 ? 32 : 1));
      },
      { passive: false },
    );
    on(c, 'pointerdown', (e) => {
      if (e.button !== 0) return;
      this.down = { id: e.pointerId, x: e.clientX, s: this.targetX, lx: e.clientX, lt: performance.now(), v: 0, drag: false };
    });
    on(c, 'pointermove', (e) => {
      const d = this.down;
      if (d && e.pointerId === d.id) {
        const dx = e.clientX - d.x;
        if (!d.drag && Math.abs(dx) > 7 && this.lay.maxX > 0) {
          d.drag = true;
          c.setPointerCapture(e.pointerId);
          c.classList.add(styles.dragging);
          this.setHover(null);
        }
        if (d.drag) {
          const now = performance.now();
          d.v = (e.clientX - d.lx) / Math.max(1, now - d.lt);
          d.lx = e.clientX;
          d.lt = now;
          this.scrollTo(d.s - dx, true);
          return;
        }
      }
      if (e.pointerType === 'touch') return;
      this.pointer = { x: e.offsetX, y: e.offsetY };
      this.hoverAt(e.offsetX, e.offsetY);
    });
    const up = (e: PointerEvent) => {
      const d = this.down;
      if (!d || e.pointerId !== d.id) return;
      if (d.drag) {
        this.suppressClick = true;
        this.later(() => (this.suppressClick = false), 0);
        c.classList.remove(styles.dragging);
        // A flick keeps going a little.
        this.scrollTo(this.targetX - d.v * 280);
      }
      this.down = null;
    };
    on(c, 'pointerup', up);
    on(c, 'pointercancel', up);
    on(c, 'pointerleave', (e) => {
      if (e.pointerType === 'touch' || this.down?.drag) return;
      this.pointer = null;
      this.setHover(null);
    });
    on(c, 'click', (e) => {
      if (this.suppressClick) return;
      const i = this.tapeAt(e.offsetX, e.offsetY);
      if (i >= 0) this.openTape(this.items[i].id);
    });
    // The invisible buttons: screen readers focus and press them.
    const g = this.group;
    if (g) {
      const idOf = (t: EventTarget | null) => (t instanceof HTMLElement && t.dataset.id ? Number(t.dataset.id) : null);
      on(g, 'click', (e) => {
        const id = idOf(e.target);
        if (id !== null) this.openTape(id);
      });
      on(g, 'focusin', (e) => {
        const id = idOf(e.target);
        if (id !== null && id !== this.focusId) this.setKeyFocus(id);
      });
    }
  }

  // ── contract ──

  update(items: readonly ItemSummary[], total: number): void {
    if (this.dom) return this.dom.update(items, total);
    // Where the tapes near the view are drawn now, for the slide to their new places.
    const now = clock();
    const before = new Map<Id, Ghost>();
    if (!this.mini) {
      for (const id of this.windowIds) {
        const i = this.index.get(id);
        if (i === undefined) continue;
        const l = this.lookOf(this.items[i]);
        this.placeTape(i, l, now);
        before.set(id, { l, tw: this.widths[i], x: this.dx[i] + this.scrollX, y: this.dy[i], z: this.dz[i], start: now });
      }
    }
    this.setItems(items);
    this.relayout(before);
  }

  move(dir: Dir): boolean {
    if (this.dom) return this.dom.move(dir);
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
    if (this.dom) return this.dom.key?.(e) ?? false;
    if ((e.key === 'Enter' || e.key === ' ') && this.focusId !== null) {
      this.openTape(this.focusId);
      return true;
    }
    return false;
  }

  /** The sleeve when the tape is turned, else the spine, as projected on screen (client coordinates). */
  rectOf(id: Id): DOMRect | null {
    if (this.dom) return this.dom.rectOf(id);
    const i = this.index.get(id);
    if (i === undefined) return null;
    const r = this.root.getBoundingClientRect();
    const { m } = this;
    const tw = this.widths[i];
    const sx = this.X[i] - this.scrollX;
    if (sx + tw < -m.D || sx > this.viewW + m.D) return new DOMRect(r.left + sx, r.top + this.Y[i], tw, m.H);
    const now = clock();
    const a = this.anims.get(id);
    const l = this.lookOf(this.items[i]);
    this.placeTape(i, l, now);
    const F = this.facesFor(tw);
    const f = place(this.openId === id ? F.right : F.spine, [this.dx[i], this.dy[i], this.dz[i]], a ? poseAt(a, now) : REST, F.origin);
    const c = corners(f, { persp: m.persp, originX: this.ox, originY: this.oy });
    if (c.some((q) => !q)) return null;
    const pts = c as { x: number; y: number }[];
    const xs = pts.map((q) => q.x);
    const ys = pts.map((q) => q.y);
    const x0 = Math.min(...xs);
    const y0 = Math.min(...ys);
    return new DOMRect(r.left + x0, r.top + y0, Math.max(...xs) - x0, Math.max(...ys) - y0);
  }

  /** The focus element, moved onto the tape (the shell gives it keyboard focus back after an overlay). */
  artOf(id: Id): HTMLElement | null {
    if (this.dom) return this.dom.artOf?.(id) ?? null;
    if (!this.focusEl || !this.index.has(id)) return null;
    this.placeFocus(id);
    return this.focusEl;
  }

  hide(id: Id | null): void {
    if (this.dom) return this.dom.hide?.(id);
    this.hiddenId = id;
    this.invalidate();
  }

  reveal(id: Id, instant = false): void {
    if (this.dom) return this.dom.reveal?.(id);
    const i = this.index.get(id);
    if (i === undefined) return;
    this.scrollTo(this.X[i] + this.widths[i] / 2 - this.m.vw / 2, true);
    const prev = this.focusId;
    this.focusId = id;
    if (prev !== null && prev !== id) this.refresh(prev);
    this.refresh(id, instant);
    this.placeFocus(id);
    this.syncWindow(true);
    this.syncMirror();
    this.invalidate();
  }

  /** Covered by the detail view or the player; when it lifts, the tape goes back on the shelf. */
  pause(paused: boolean): void {
    if (this.dom) return this.dom.pause?.(paused);
    this.paused = paused;
    if (!paused && this.openId !== null) this.closeTape(this.openId);
    if (!paused) this.invalidate();
  }

  // ── teardown ──

  private teardown(): void {
    this.ro.disconnect();
    for (const c of this.cleanup) c();
    this.cleanup = [];
    if (this.raf) cancelAnimationFrame(this.raf);
    this.raf = 0;
    if (this.hoverT) clearTimeout(this.hoverT);
    if (this.settleT) clearTimeout(this.settleT);
    for (const t of this.timers) clearTimeout(t);
    this.timers.clear();
    this.renderer.destroy();
    this.sheet?.destroy();
    this.sheet = null;
    this.sheetCanvas?.remove();
    this.canvas.remove();
    this.group?.remove();
    this.root.classList.remove(styles.gpu);
  }

  /** The GPU went away (driver reset, GPU process crash): carry on as the DOM Shelf. */
  private fallBack(): void {
    if (this.dom || this.dead) return;
    this.teardown();
    this.dom = { update: () => undefined, move: () => false, rectOf: () => null, destroy: () => undefined };
    void import('./shelf').then(({ shelf }) => {
      if (this.dead) return;
      this.dom = shelf.mount(this.root, this.ctx);
    });
  }

  destroy(): void {
    if (this.dead) return;
    this.dead = true;
    if (this.dom) {
      this.dom.destroy();
      return;
    }
    this.teardown();
  }
}

/** Waits (a little) for the faces the spines are set in, so the first paint is the real one. */
async function fontsReady(): Promise<void> {
  const fonts = typeof document !== 'undefined' ? (document.fonts as FontFaceSet | undefined) : undefined;
  if (!fonts) return;
  const load = Promise.all([fonts.load(CONDENSED), fonts.load('400 12px "DM Mono"'), fonts.load('400 12px "Jost Variable"')]).then(
    () => undefined,
    () => undefined,
  );
  await Promise.race([load, new Promise<void>((r) => setTimeout(r, 600))]);
}

/** The Shelf on the GPU, once its pipeline compiles here (null: use the DOM Shelf). */
export async function gpuShelf(gpu: Gpu): Promise<LensModule | null> {
  const [scene, mips] = await Promise.all([prepareScene3D(gpu), posterCache(gpu).prepare(), fontsReady()]);
  if (!scene || !mips) return null;
  return {
    mount(root, ctx) {
      if (gpu.lost) throw new Error('The GPU device is gone.');
      return new GpuShelfView(root, ctx, gpu);
    },
  };
}
