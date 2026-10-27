/**
 * Shelf, drawn with WebGPU: the same shelves, tapes and behaviour as the DOM
 * Shelf (shelf.ts, still used where WebGPU isn't available), drawn as perspective
 * quads (gpu/scene3d.ts) in one draw call however many tapes are in view.
 *
 * - Each spine is painted once with Canvas 2D (spine.ts) into a sprite sheet, a few
 *   per frame as tapes come into view; until then a tape shows its colours.
 * - Everything is lit (scene3d.ts materials): card sleeves and plastic clamshells,
 *   wooden boards and painted bookends, under a lamp above the shelf and a light
 *   whose highlight follows the pointer, with shade where the shelf hides things
 *   and soft shadows on the wall, on the boards and behind a tape you take out.
 *   When only the light moves, the frame redraws without rebuilding the scene.
 * - Pointing at a tape lifts it; resting on it brings it out and turns its cover to
 *   you (peek.ts: springs, so it reverses smoothly from wherever it is). Its
 *   neighbours ease aside. Click opens it; on touch the first tap shows the cover
 *   and the second opens; keyboard focus shows the cover.
 * - There is no depth buffer, so a frame is drawn back to front: the wall and its
 *   shadows, the boards, the tapes' sides and tops in the order they hide each
 *   other, the spines, the shadows of tapes taken out, the boards' lips and labels,
 *   and last the tapes that are off the shelf (the part of their sides still inside
 *   the shelf goes with the shelf).
 * - The DOM keeps what it is good at: an invisible copy of the tapes in view, with
 *   the DOM Shelf's roles and labels, for screen readers, and one element that
 *   carries keyboard focus (over the cover while it shows).
 */
import { decode } from 'blurhash';
import type { Id, ItemSummary } from '../../api/types';
import { sourceLabel } from '../../components/art-label';
import { artLabel } from '../art';
import { activeLens } from '../bridge';
import { clamp, el, rand01, reducedMotion } from '../dom';
import type { Gpu } from '../gpu/device';
import { bezier, lerp, soft } from '../gpu/motion';
import { classFor, posterCache, type PosterCache } from '../gpu/posters';
import { clock } from '../gpu/quads';
import { corners, FLAG3D, inside, KIND3D, MAT3D, prepareScene3D, Quad3DBatch, Scene3DRenderer, type Camera } from '../gpu/scene3d';
import { SpriteSheet, type Sprite } from '../gpu/sprites';
import { textAtlas, type Glyph, type TextAtlas, type TextStyle } from '../gpu/text';
import { paletteOf } from '../palette';
import { isNew, isWatchedItem, progressFraction } from '../sections';
import type { Dir, LensContext, LensModule, LensView } from '../types';
import { planShelf, shelfLayout, shelfMetrics, spineLook, type Metrics, type ShelfLayout, type ShelfPlan, type SpineLook } from './geometry';
import { asideFalloff, BACK_C, BACK_K, LIFT_C, LIFT_K, PEEK_C, PEEK_K, peekPath, peekWait, spring0, stepSpring, tapePose, type PoseDims, type Spring } from './peek';
import styles from './shelf.module.css';
import { backKey, grayHex, mixBlackOklab, paintBack, paintEdge, paintGlow, paintRamp, paintSpine, spineKey, type ShelfFonts } from './spine';
import { boxCorners, facing, I3, place, REST, rotX, rotY, splitAtDepth, tapeFaces, type M3, type Placed, type TapeFaces, type TapeMove } from './transform';
import { cssColor, dcColor, hexColor } from '../gpu/colors';

type RGBA = readonly [number, number, number, number];
type Span = [number, number];
/** Who asked a tape to show its cover: the pointer resting on it, the keyboard, a tap, or opening it. */
type PeekBy = 'pointer' | 'key' | 'touch' | 'open';

const MINI_COUNT = 14;
const CONDENSED = '800 20px "Archivo Variable"';

const WHITE: RGBA = [1, 1, 1, 1];
const BLACK: RGBA = [0, 0, 0, 1];
const INK: RGBA = [0.914, 0.902, 0.875, 1];
const PILL: RGBA = [8 / 255, 8 / 255, 10 / 255, 1];

// Timings (seconds) and curves, as shelf.module.css and shelf.ts have them.
const SLIDE_S = 0.52;
const ENTER_S = 0.42;
const ENTER_STAGGER = 0.018;
const SINK_S = 0.26;
const EXTRA_S = 0.5;
/** Reduced motion: the cover fades in where the tape stands, this fast. */
const FADE_S = 0.15;
const EASE_IN = bezier(0.42, 0, 1, 1);
/** `--spring-soft` (tokens.css): boards, bookends and labels sliding to a new layout. */
const SPRING_SOFT = [0, 0.032, 0.106, 0.201, 0.303, 0.403, 0.496, 0.579, 0.652, 0.715, 0.768, 0.813, 0.849, 0.88, 0.904, 0.924, 0.94, 0.953, 0.963, 0.971, 0.978, 0.983, 0.987, 0.99, 0.992, 0.994, 0.995, 0.996, 0.997, 0.998, 1];
/** A pointer peek waits this long after the pointer leaves the tapes before going back (a brief excursion keeps it). */
const LEAVE_MS = 120;
/** How far along its peek a tape waits for a neighbour's cover to turn away (out of the shelf, not turning: peek.ts). */
const WAIT_AT = 0.2;
/** Card: satin; clamshell plastic: glossy (spines carry their own gloss, spine.ts). */
const GLOSS_CARD = 0.5;
const GLOSS_PLASTIC = 0.82;
const GLOSS_WOOD = 0.32;
const GLOSS_PAINT = 0.28;

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

/** A value easing to a target over a fixed time (reduced motion's fade). */
class Ease {
  from: number;
  to: number;
  start = 0;
  dur = 0;

  constructor(value: number) {
    this.from = value;
    this.to = value;
  }

  value(now: number): number {
    if (this.dur <= 0 || now >= this.start + this.dur) return this.to;
    if (now <= this.start) return this.from;
    return lerp(this.from, this.to, (now - this.start) / this.dur);
  }

  done(now: number): boolean {
    return this.dur <= 0 || now >= this.start + this.dur;
  }

  set(to: number, now: number, dur: number): void {
    if (to === this.to) return;
    this.from = this.value(now);
    this.to = to;
    this.start = now;
    this.dur = dur;
  }
}

/** A tape off its resting place: its lift (pointed at) and its turn to show its cover, as springs, and where that puts it. */
interface Anim {
  lift: Spring;
  peek: Spring;
  /** Reduced motion: the turned tape fades in over the one on the shelf instead (0–1). */
  fade: Ease;
  pose: TapeMove;
}

/** Per title: its spine look and colours, worked out once. */
interface TapeLook {
  item: ItemSummary;
  look: SpineLook;
  /** A clamshell (plastic) rather than a card sleeve. */
  clam: boolean;
  /** Its own grain and wear. */
  seed: number;
  /** The spine until its sprite is painted (offline: greyed). */
  bg: RGBA;
  bg2: RGBA;
  /** The back cover's gradient and the top of the box (the lighting shades its back). */
  fbg: RGBA;
  fbg2: RGBA;
  top: RGBA;
  topBack: RGBA;
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
  pose: TapeMove;
  /** Pointed at (0–1): a brighter spine. */
  lit: number;
  /** How much of its foot still stands on the board (shade there). */
  foot: number;
  ring: boolean;
  /** Reduced motion: a turned copy over the tape, which stays on the shelf. */
  copy: boolean;
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
  shadow: RGBA;
  dark: boolean;
  fonts: ShelfFonts;
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
  private faces = new Map<number, TapeFaces>();
  private X = new Float64Array(0);
  private Y = new Float64Array(0);
  private widths = new Float64Array(0);
  /** How far each tape has eased aside for a neighbour showing its cover (px), and which are. */
  private aside = new Float64Array(0);
  private asideSet: number[] = [];
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
  /** Poster size for a sleeve on the shelf; `coverCls` for the cover that's out and turned to you (sharp, crop and all). */
  private posterCls = 1;
  private coverCls = 1;

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
  private hoverSince = 0;
  /** The tape showing its cover (or turning to), and who asked. */
  private peekId: Id | null = null;
  private peekBy: PeekBy | null = null;
  private openId: Id | null = null;
  /** Opened, turning: the detail view takes over once its cover faces you. */
  private opening: Id | null = null;
  private hiddenId: Id | null = null;
  private eagerId: Id | null = null;
  private wanted = new Set<Id>();
  private anims = new Map<Id, Anim>();
  /** Springs moved this frame (the scene changes): spines aren't snapped to pixels mid-move. */
  private springing = false;
  private rm = false;
  private slides = new Map<Id, { x: number; y: number; start: number }>();
  private enters = new Map<Id, number>();
  private ghosts: Ghost[] = [];
  private extrasNow = new Map<string, Ext>();
  private extraFrom: Map<string, Ext> | null = null;
  private extraStart = 0;

  /** The lamp above the shelf; the highlight that follows the pointer (where it lands, how strong). */
  private lamp: [number, number, number, number] = [0, 0, 0, 1];
  private glint: [number, number, number, number] = [0, 0, 0, 0];
  private room: [number, number, number, number] = [0.5, 0.85, 1, 100];
  private gx = spring0();
  private gy = spring0();
  private gOn = spring0();

  /** The batch needs building again (anything but the lights or the time changed). */
  private dirty = true;
  private paintPending = false;
  private paintedNow = 0;
  private budget = 8;
  private raf = 0;
  private paused = false;
  private dead = false;
  private lastTrim = 0;
  private pointer: { x: number; y: number; t: number } | null = null;
  /** The pointer's speed, px/ms, smoothed. */
  private speed = 0;
  private lastType = 'mouse';
  private down: { id: number; x: number; s: number; lx: number; lt: number; v: number; drag: boolean } | null = null;
  private suppressClick = false;
  private intentT: ReturnType<typeof setTimeout> | null = null;
  private leaveT: ReturnType<typeof setTimeout> | null = null;
  private settleT: ReturnType<typeof setTimeout> | null = null;
  private timers = new Set<ReturnType<typeof setTimeout>>();
  private ro: ResizeObserver;
  private cleanup: (() => void)[] = [];
  /** After a lost GPU: the DOM Shelf, which this view then forwards to. */
  private dom: LensView | null = null;
  /** Where the focus element was put (over the slot, or over the cover while it shows). */
  private focusOnCover = false;
  // Scratch faces for tapes at rest.
  private fSpine = blank();
  private fTop = blank();
  private fSide = blank();
  private tint: [number, number, number, number] = [1, 1, 1, 1];
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
    this.rm = reducedMotion();
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
    const top1 = v('--sh-board-top-1', '#36363d');
    const t = cssColor(top1, BLACK);
    return {
      focus: cssColor(v('--focus', '#fff'), WHITE),
      tagBg: cssColor(v('--sh-tag-bg', '#f2f0ea'), WHITE),
      tagInk: cssColor(v('--sh-tag-ink', '#141416'), BLACK),
      light: v('--sh-light', 'rgb(255 236 212 / 0.085)'),
      top1,
      top2: v('--sh-board-top-2', '#141417'),
      edge1: v('--sh-board-edge-1', '#2c2c32'),
      edge2: v('--sh-board-edge-2', '#19191d'),
      hi: v('--sh-board-hi', 'rgb(255 255 255 / 0.18)'),
      end1: v('--sh-end-1', '#4b4c53'),
      end2: v('--sh-end-2', '#222329'),
      shadow: cssColor(v('--sh-shadow', 'rgb(0 0 0 / 0.62)'), [0, 0, 0, 0.62]),
      // Dark boards: the dark room (the light theme's room is brighter, its highlights gentler).
      dark: 0.2126 * t[0] + 0.7152 * t[1] + 0.0722 * t[2] < 0.5,
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
    const motion = window.matchMedia('(prefers-reduced-motion: reduce)');
    const still = () => {
      this.rm = motion.matches;
      this.invalidate();
    };
    motion.addEventListener('change', still);
    this.cleanup.push(
      () => mo.disconnect(),
      () => mq.removeEventListener('change', refresh),
      () => motion.removeEventListener('change', still),
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
    // The glint rests where the lamp's own highlight would be, until a pointer moves it.
    if (!this.pointer) {
      this.gx.x = this.gx.to = this.ox;
      this.gy.x = this.gy.to = this.oy;
    }
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
    // The sleeve is a 2:3 poster cut to the box's proportions (object-fit: cover).
    this.crop = Math.min(1, (m.D / m.H) * 1.5);
    this.crop0 = (1 - this.crop) / 2;
    // Sharp when the cover turns to you (out in front of the shelf, a little larger):
    // only the cropped middle of the poster spans the cover, so that one asks for more.
    this.posterCls = classFor(m.D * 1.15 * this.dpr);
    this.coverCls = classFor((m.D * 1.15 * this.dpr) / this.crop);
    this.ensureSheet();
    if (this.focusId !== null && !this.index.has(this.focusId)) {
      this.focusId = null;
      if (this.focusEl) this.focusEl.hidden = true;
    }
    if (this.openId !== null && !this.index.has(this.openId)) this.openId = null;
    if (this.peekId !== null && !this.index.has(this.peekId)) this.peekAt(null, 'pointer');
    if (this.hoverId !== null && !this.index.has(this.hoverId)) this.setHover(null);
    const now = clock();
    const motion = before !== null && before.size > 0 && !this.rm;
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
      this.aside = new Float64Array(n);
      this.asideSet = [];
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

  private dims(i: number): PoseDims {
    const m = this.m;
    return { H: m.H, W: m.W, D: m.D, tw: this.widths[i], gap: m.gap };
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
      clam: look.style === 'clamw' || look.style === 'clamb',
      seed: rand01(`${item.id}:wear`),
      bg: hexColor(grayHex(look.bg, off)),
      bg2: hexColor(grayHex(look.bg2, off)),
      fbg: hexColor(look.bg),
      fbg2: hexColor(look.bg2),
      top: hexColor(look.top),
      topBack: hexColor(mixBlackOklab(look.top, 0.8)),
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

  /** A gradient of the shelf's (board, lip, bookends, light): painted once per theme and size. */
  private fx(key: string, paint: (c: CanvasRenderingContext2D, w: number, h: number) => void, w?: number, h?: number): Sprite | null {
    const sp = this.sheet?.get(key, paint, w, h, 64) ?? null;
    if (!sp) this.paintPending = true;
    return sp;
  }

  // ── frames ──

  /** Something in the scene changed: build it again next frame. */
  private invalidate = (): void => {
    this.dirty = true;
    this.relight();
  };

  /** Only the lights or the time moved: draw the same scene again. */
  private relight = (): void => {
    if (!this.raf && !this.dead && !this.dom) this.raf = requestAnimationFrame(this.frame);
  };

  private frame = (): void => {
    this.raf = 0;
    if (this.dead || this.dom || !this.sheet) return;
    const now = clock();
    // After a rest the first frame steps as one frame, not the whole rest.
    const gap = now - this.lastT;
    const dt = gap > 0.05 ? 1 / 120 : Math.max(0, gap);
    this.lastT = now;
    if (!this.mini && Math.min(window.devicePixelRatio || 1, 3) !== this.dpr) {
      this.measure();
      this.relayout(null);
    }
    const scrolling = this.stepScroll(dt);
    this.springing = this.stepSprings(dt, now);
    const glinting = this.stepLights(dt);
    const posters = this.posters;
    const sheet = this.sheet;
    const fresh = this.dirty || scrolling || this.springing || posters.pending;
    if (fresh) {
      this.dirty = false;
      this.asideNow();
      // Upload first: a slot drawn in the last frame is still "in use" and can't be evicted for a new one.
      posters.upload();
      posters.begin();
      sheet.begin();
      this.paintedNow = 0;
      // Spines come in fast on a fresh shelf, a few per frame while it scrolls.
      this.budget = now - this.mountedAt < 0.8 ? 24 : 8;
      const gen = this.text.gen;
      this.build(now);
      if (this.text.gen !== gen) this.build(now);
      if (scrolling || now - this.mountedAt < 1.5) this.prefetch();
      this.text.flush();
    }
    this.renderer.draw(this.batch, this.camera(now), { posters: posters.views(), glyphs: this.text.view, sprites: sheet.view, key: `${posters.version}|${this.sheetSerial}.${sheet.version}` }, fresh);
    if (fresh) {
      posters.pump();
      if (performance.now() - this.lastTrim > 5000) {
        this.lastTrim = performance.now();
        posters.trim();
      }
    }
    const moving = this.settle(now);
    // Painting that ran out of budget goes on next frame (unless nothing could be painted at all).
    const painting = fresh && this.paintPending && this.paintedNow > 0;
    if (this.paused) return;
    if (scrolling || this.springing || moving || painting || posters.pending) this.invalidate();
    else if (glinting || now < posters.fadeUntil) this.relight();
  };

  private camera(now: number): Camera {
    return { width: this.viewW, height: this.viewH, persp: this.m.persp, originX: this.ox, originY: this.oy, dpr: this.dpr, time: now, light: this.lamp, glint: this.glint, room: this.room };
  }

  /**
   * The lamp hangs above the shelf, a little in front; the glint is a second, small
   * light placed so its highlight on the spines lands just above the pointer (it
   * follows on a soft spring, and fades when the pointer leaves). Still under reduced
   * motion. True while the glint is moving.
   */
  private stepLights(dt: number): boolean {
    const { m, ox, oy, viewW, viewH } = this;
    const persp = m.persp;
    this.lamp[0] = ox + viewW * 0.08;
    this.lamp[1] = -viewH * 0.35;
    this.lamp[2] = persp * 0.6;
    this.lamp[3] = 1;
    const dark = this.theme.dark;
    this.room[0] = dark ? 0.5 : 0.66;
    this.room[1] = dark ? 1 : 0.55;
    this.room[2] = dark ? 1 : 0.6;
    this.room[3] = m.D;
    const on = !this.mini && !this.rm && this.pointer !== null ? 1 : 0;
    this.gOn.to = on;
    let moving = false;
    if (this.rm || this.mini) {
      this.gOn.x = this.gOn.to;
      this.gOn.v = 0;
    } else {
      moving = stepSpring(this.gx, dt, 90, 19) || moving;
      moving = stepSpring(this.gy, dt, 90, 19) || moving;
      moving = stepSpring(this.gOn, dt, 60, 15.5) || moving;
    }
    // Where its highlight should land, and where that puts the light (mirrored in the plane of the spines).
    const gz = persp * 0.75;
    const k = (persp + gz) / persp;
    this.glint[0] = ox + (this.gx.x - ox) * k;
    this.glint[1] = oy + (this.gy.x - 40 - oy) * k;
    this.glint[2] = gz;
    this.glint[3] = (dark ? 1.5 : 1.1) * Math.max(0, this.gOn.x);
    return moving;
  }

  /** Smooth scrolling towards the target (shelf.ts: 15% of the way per frame at 120 Hz). */
  private stepScroll(dt: number): boolean {
    const d = this.targetX - this.scrollX;
    if (d === 0) return false;
    if (Math.abs(d) < 0.25) this.scrollX = this.targetX;
    else this.scrollX += d * (1 - Math.pow(0.85, dt * 120));
    this.scrolled();
    return this.scrollX !== this.targetX;
  }

  /**
   * Lifts and turns, one step (reduced motion: no motion, the cover fades in over the
   * tape instead). Hands an opened tape to the detail view once its cover faces you.
   * True while any spring is still moving.
   */
  private stepSprings(dt: number, now: number): boolean {
    let moving = false;
    let out = 0;
    // A cover still turned towards you, going back: the next tape waits partway out
    // until it has turned away (so the two never meet), and it hurries.
    const going: number[] = [];
    for (const [id, a] of this.anims) {
      if (a.peek.x > 0.02) out++;
      const i = this.index.get(id);
      if (i !== undefined && a.peek.to === 0 && peekPath(a.peek.x).turn > 0.2) going.push(i);
    }
    const blocked = (i: number) => going.some((j) => j !== i && this.p.row[j] === this.p.row[i] && this.p.bay[j] === this.p.bay[i] && Math.abs(this.X[j] - this.X[i]) < this.m.D);
    for (const [id, a] of this.anims) {
      const i = this.index.get(id);
      if (i === undefined) {
        this.anims.delete(id);
        continue;
      }
      let busy: boolean;
      if (this.rm || this.mini) {
        a.lift.x = a.lift.to;
        a.peek.x = a.peek.to;
        a.lift.v = 0;
        a.peek.v = 0;
        busy = !a.fade.done(now);
      } else {
        busy = stepSpring(a.lift, dt, LIFT_K, LIFT_C);
        const back = a.peek.to < a.peek.x;
        // Another tape on its way out (or a third tape out): the ones going back hurry.
        const hurry = back && (out > 1 || this.peekId !== null) ? 2.5 : 1;
        const wait = a.peek.to === 1 && going.length > 0 && blocked(i);
        if (wait) a.peek.to = WAIT_AT;
        if (stepSpring(a.peek, dt, (back ? BACK_K : PEEK_K) * hurry, (back ? BACK_C : PEEK_C) * Math.sqrt(hurry))) busy = true;
        if (wait) {
          a.peek.to = 1;
          busy = true;
        }
        // Going back, it seats: no overshoot into the wall.
        if (a.peek.x < 0) {
          a.peek.x = 0;
          a.peek.v = Math.max(0, a.peek.v);
        }
        if (a.lift.x < 0) {
          a.lift.x = 0;
          a.lift.v = Math.max(0, a.lift.v);
        }
      }
      tapePose(this.rm ? 0 : a.lift.x, this.rm ? 0 : a.peek.x, this.dims(i), a.pose);
      if (busy) moving = true;
      if (this.opening === id && (this.rm || peekPath(a.peek.x).turn >= 0.97)) this.handOff(id);
      if (!busy && a.lift.to === 0 && a.peek.to === 0 && a.lift.x === 0 && a.peek.x === 0 && a.fade.value(now) === 0) this.anims.delete(id);
    }
    this.placeFocusOnCover();
    return moving;
  }

  /** Tapes beside one showing its cover ease aside (never into each other). */
  private asideNow(): void {
    const set = this.asideSet;
    for (const i of set) this.aside[i] = 0;
    set.length = 0;
    if (this.rm || this.mini) return;
    const { p, m } = this;
    const n = p.n;
    const amount = m.gap * 3;
    for (const [id, a] of this.anims) {
      if (a.peek.x <= 0.001) continue;
      const i = this.index.get(id);
      if (i === undefined) continue;
      const s = amount * Math.min(1, peekPath(a.peek.x).out);
      const same = (j: number) => j >= 0 && j < n && p.bay[j] === p.bay[i] && p.row[j] === p.row[i];
      for (const dir of [1, -1]) {
        const gaps: number[] = [];
        for (let j = i + dir; same(j) && same(j + dir) && gaps.length < 6; j += dir) {
          const a0 = dir > 0 ? j : j + dir;
          gaps.push(this.X[a0 + 1] - this.X[a0] - this.widths[a0]);
        }
        const offs = asideFalloff(s, gaps);
        for (let k = 0; k < offs.length; k++) {
          const j = i + dir * (k + 1);
          if (!same(j)) break;
          this.aside[j] += dir * offs[k];
          set.push(j);
        }
      }
    }
  }

  /** Drop finished animations; true while any is still running. */
  private settle(now: number): boolean {
    let moving = false;
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

  /** Where tape `i` is drawn now (a search slides it, a new one drops in, a neighbour nudges it), into dx/dy/dz/da. */
  private placeTape(i: number, l: TapeLook, now: number): void {
    const id = l.item.id;
    let x = this.X[i] + this.aside[i];
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
    this.wantNear();

    // 1. The wall: the light on it, the boards' shadows; the boards' tops (seen from above) and the shade where tapes stand on them.
    const glow = this.fx(`fx|glow|${this.theme.light}`, (c, w, h) => paintGlow(c, w, h, this.theme.light));
    for (const row of rows) {
      if (glow) {
        const gh = m.head + m.H * 0.7;
        const o = b.add(row.x + row.w / 2 - sx, row.top - m.head + gh / 2, -m.D - 8, row.w, gh, KIND3D.sprite);
        this.sprite(o, glow, 1);
      }
    }
    for (const row of rows) this.wallShadow(row, sx);
    for (const row of rows) {
      if (oy >= row.by) continue;
      this.boardTop(row, sx);
      this.footShade(row);
    }
    // 2. Tapes leaving the results, sinking back into the shelf.
    for (const g of this.ghosts) this.ghost(g, now);
    // 3. Sides and tops, row by row, in the order they hide each other: left of the
    //    eye from left to right (each tape hides the one before's right side), right of it from right to left.
    for (const row of rows) this.sides(row, lo, hi);
    // 4. The spines on the shelf and the bookends' faces. A still shelf puts each spine
    //    on whole device pixels at exactly its size, so its sprite is drawn texel for pixel.
    const still = this.targetX === this.scrollX && !this.down?.drag && !reflow && !this.springing;
    const { ox, dpr } = this;
    const persp = m.persp;
    for (const row of rows) {
      for (let i = row.i0; i < row.i1; i++) {
        if (this.X[i] + this.widths[i] < lo || this.X[i] > hi) continue;
        const id = this.items[i].id;
        const t = this.actById.get(id);
        if ((t && !t.copy) || id === this.hiddenId) continue;
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
        const lit = this.anims.get(id)?.lift.x ?? 0;
        // (Under a turned copy, the copy carries the focus ring.)
        const ring = id === this.focusId && !this.mini && !t;
        this.spine(l, f, tw, this.da[i], l.item.available ? lerp(1, 1.08, lit) : lerp(0.62, 0.8, lit), ring, 1);
      }
      this.endFaces(row, sx);
    }
    // 4b. Tapes off the shelf cast soft shadows on it, from the lamp.
    for (const t of this.act) if (!t.copy) this.dropShadow(t);
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
      let pose = a.pose;
      let alpha = this.da[i];
      const copy = this.rm || this.mini;
      if (copy) {
        // Reduced motion: the tape stays put; its cover fades in over it, turned to you.
        const f = a.fade.value(now);
        if (f <= 0.002) continue;
        pose = tapePose(0, 1, this.dims(i));
        alpha *= f;
      } else if (pose.tz === 0 && pose.ty === 0 && pose.ry === 0 && pose.rx === 0) continue;
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
        a: alpha,
        depth: this.dz[i] + pose.tz,
        pose,
        lit: copy ? 0 : a.lift.x,
        foot: clamp(1 - pose.tz / 14, 0, 1) * clamp(1 + pose.ty / 6, 0, 1),
        ring: id === this.focusId && !this.mini,
        copy,
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

  /** Posters of the tape under the pointer (or focused) and its neighbours load ahead, so a cover is there when it turns. */
  private wantNear(): void {
    const id = this.hoverId ?? this.focusId;
    if (id === null || this.mini) return;
    const i = this.index.get(id);
    if (i === undefined) return;
    for (let j = Math.max(0, i - 2); j <= Math.min(this.items.length - 1, i + 2); j++) {
      const art = this.items[j].poster;
      if (art) this.posters.want(art, this.coverCls, j === i ? 1 : 200 + Math.abs(j - i));
    }
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
    if (t && !t.copy) {
      // Off the shelf: the part of its sides still inside the shelf goes in with the shelf.
      if (t.right && t.splitR?.back) this.sleeve(t.l, t.right, t.splitR.back, t.a, 1, true);
      if (t.left && t.splitL?.back) this.backCover(t.l, t.left, t.splitL.back, t.a, 1, true);
      return;
    }
    const tw = this.widths[i];
    const x = this.dx[i];
    const y = this.dy[i];
    const z = this.dz[i];
    const half = m.D / 2;
    // Beside a neighbour a side is in deep shade; at the end of a row, less.
    const shade = exposed ? 0.3 : 1;
    if (side === 'right') {
      const f = setFace(this.fSide, x + tw, y + m.H / 2, z - half, m.D, m.H, 0, Math.PI / 2, RY_RIGHT);
      this.sleeve(l, f, [-half, half], a, shade, exposed);
    } else if (side === 'left') {
      const f = setFace(this.fSide, x, y + m.H / 2, z - half, m.D, m.H, 0, -Math.PI / 2, RY_LEFT);
      this.backCover(l, f, [-half, half], a, shade, exposed);
    }
    if (oy < y) this.top(l, setFace(this.fTop, x + tw / 2, y, z - half, tw, m.D, Math.PI / 2, 0, RX_TOP), tw, a);
  }

  /** The tape that is off the shelf: the front of its sides, its top and its spine (back faces culled). */
  private activeTape(t: Active): void {
    const off = !t.l.item.available;
    if (t.right && t.splitR?.front) this.sleeve(t.l, t.right, t.splitR.front, t.a, 1, true);
    if (t.left && t.splitL?.front) this.backCover(t.l, t.left, t.splitL.front, t.a, 1, true);
    if (t.top) this.top(t.l, t.top, t.tw, t.a);
    const hidden = this.hiddenId === t.id && this.openId !== t.id;
    if (t.spine && !hidden) this.spine(t.l, t.spine, t.tw, t.a, off ? lerp(0.62, 0.8, t.lit) : lerp(1, 1.08, t.lit), t.ring, t.foot);
  }

  // ── faces ──

  /**
   * A quad on a placed face: centred at (u, v) from the face's centre, w×h, turned
   * with the face; `out` lifts it off the face along its normal (a decal on it).
   */
  private on(f: Placed, u: number, v: number, w: number, h: number, kind: number, radius = 0, out = 0): number {
    const s = f.s;
    let x = f.x;
    let y = f.y;
    let z = f.z;
    const q = f.m;
    if (u !== 0 || v !== 0) {
      x += (q[0] * u + q[1] * v) * s;
      y += (q[3] * u + q[4] * v) * s;
      z += (q[6] * u + q[7] * v) * s;
    }
    if (out !== 0) {
      x += q[2] * out;
      y += q[5] * out;
      z += q[8] * out;
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

  /** The material of a tape's box: card or a clamshell's plastic. */
  private box(o: number, l: TapeLook, shade: number): void {
    this.batch.material(o, l.clam ? MAT3D.plastic : MAT3D.print, l.clam ? GLOSS_PLASTIC : GLOSS_CARD, l.seed, shade);
  }

  /** `foot`: how much of it stands on the board (the shade at its foot and beside its neighbours). */
  private spine(l: TapeLook, f: Placed, tw: number, a: number, tint: number, ring: boolean, foot: number): void {
    const b = this.batch;
    const H = this.m.H;
    const radius = l.clam ? 2.5 : 1.5;
    const sp = this.spineSprite(l, tw, this.budget);
    const o = this.on(f, 0, 0, tw, H, sp ? KIND3D.sprite : KIND3D.poster, radius);
    b.opacity(o, a);
    // Card is satin; the sprite's alpha makes plastic and stickers glossy.
    b.material(o, MAT3D.spine, GLOSS_CARD * 0.9, l.seed, foot);
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
      const r = this.on(f, 0, 0, tw + 8, H + 8, KIND3D.ring, radius + 4, 0.75);
      b.color(r, this.theme.focus, 0.6 * a);
      b.extra(r, 2);
    }
  }

  private top(l: TapeLook, f: Placed, tw: number, a: number): void {
    const b = this.batch;
    const o = this.on(f, 0, 0, tw, this.m.D, KIND3D.poster, 1.5);
    b.opacity(o, a);
    // Its colour, a little darker at the back; the light and the shade under the next board do the rest.
    b.color(o, l.topBack);
    b.uv(o, l.top[0], l.top[1], l.top[2], 1);
    b.flags(o, FLAG3D.gradient);
    this.box(o, l, 0);
  }

  /**
   * The sleeve (the tape's right side): its poster once wanted, and its marks where it
   * shows whole. `shade`: how deep in shade it is beside its neighbour (0–1).
   */
  private sleeve(l: TapeLook, f: Placed, span: Span, a: number, shade: number, full: boolean): void {
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
    this.box(o, l, shade);
    const hidden = this.hiddenId === id && this.openId === id;
    let flags = whole && !hidden ? FLAG3D.hairline : 0;
    if (hidden) {
      // Its poster is flying to the detail page: the bare box.
      b.color(o, l.fbg2);
    } else {
      const art = item.poster;
      const near = id === this.eagerId || id === this.peekId || id === this.hoverId || id === this.focusId;
      const hit = art && this.wanted.has(id) ? this.posters.get(art, near ? this.coverCls : this.posterCls, id === this.eagerId ? -1e6 : near ? 0 : 1000, true) : null;
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
  }

  /** The back cover (the tape's left side): its colours and, where it shows, the words on it. */
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
    this.box(o, l, shade);
    if (full) {
      // The words are printed on it: lit and shaded with it.
      const s = this.backSprite(l);
      if (s) {
        const w = this.on(f, uc, 0, sw, H, KIND3D.sprite, whole ? 1.5 : 0);
        this.sprite(w, s, a, t0, t1);
        this.box(w, l, shade);
      }
    }
  }

  private glyphOn(f: Placed, g: Glyph, x: number, y: number, color: RGBA, a: number): void {
    // (x, y): the glyph quad's top-left, from the face's top-left corner.
    const { D, H } = this.m;
    const o = this.on(f, x + g.w / 2 - D / 2, y + g.h / 2 - H / 2, g.w, g.h, KIND3D.text, 0, 0.75);
    this.batch.color(o, color, a);
    this.batch.uv(o, g.u0, g.v0, g.u1, g.v1);
  }

  /** A box on the sleeve, from the face's top-left corner (CSS-style), lifted off it a hair. */
  private boxOn(f: Placed, x: number, y: number, w: number, h: number, kind: number, radius: number): number {
    const { D, H } = this.m;
    return this.on(f, x + w / 2 - D / 2, y + h / 2 - H / 2, w, h, kind, radius, 0.75);
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

  /**
   * The board's shadow on the wall under it: the lamp is above, so the wall just below a
   * board is in its shade, fading lower down. Drawn on the wall's plane, where it lands
   * under the board's lip as seen from the eye.
   */
  private wallShadow(row: ShelfRow, sx: number): void {
    const { m, ox, oy, theme: t } = this;
    const persp = m.persp;
    const zw = -m.D - 7;
    // The lip's foot on screen, and the point on the wall behind it.
    const kLip = persp / (persp - m.lip);
    const kWall = (persp - zw) / persp;
    const toWall = (x: number, y: number): [number, number] => [ox + (ox + (x - ox) * kLip - ox) * kWall, oy + (oy + (y - oy) * kLip - oy) * kWall];
    const [x0, y0] = toWall(row.x - sx, row.by + m.bh);
    const [x1] = toWall(row.x + row.w - sx, row.by + m.bh);
    const blur = Math.max(10, m.head * 0.95);
    const o = this.batch.add((x0 + x1) / 2, y0, zw, x1 - x0 + blur * 0.5, blur * 2, KIND3D.shadow, 4);
    this.batch.color(o, t.shadow, t.dark ? 0.62 : 0.55);
    this.batch.extra(o, blur);
  }

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
    this.batch.material(o, MAT3D.wood, GLOSS_WOOD, rand01(`board:${row.b}:${row.r}`));
  }

  /**
   * Where the tapes stand on the board, a soft shade along their feet (ambient light
   * can't reach under them): one soft strip per run of tapes, a hair above the board.
   */
  private footShade(row: ShelfRow): void {
    const { m, theme: t } = this;
    const b = this.batch;
    const blur = Math.max(4, m.W * 0.16);
    let start = -1;
    const flush = (i0: number, i1: number) => {
      const x0 = this.dx[i0];
      const x1 = this.dx[i1] + this.widths[i1];
      const o = b.add((x0 + x1) / 2, row.by - 0.75, 0, x1 - x0 + blur, blur * 2, KIND3D.shadow, 2);
      b.rotate(o, Math.PI / 2, 0, 0);
      b.color(o, BLACK, t.dark ? 0.7 : 0.32);
      b.extra(o, blur);
    };
    let prev = -1;
    for (const i of this.drawn) {
      if (i < row.i0 || i >= row.i1) continue;
      if (start < 0) start = i;
      else if (this.dx[i] - (this.dx[prev] + this.widths[prev]) > m.gap * 2.5 + 0.5) {
        flush(start, prev);
        start = i;
      }
      prev = i;
    }
    if (start >= 0) flush(start, prev);
  }

  private lip(row: ShelfRow, sx: number): void {
    const { m, theme: t, sheet } = this;
    if (!sheet) return;
    const len = Math.max(1, Math.round(m.bh * this.dpr));
    const s = this.fx(`fx|edge|${t.hi}|${t.edge1}|${t.edge2}|${len}`, (c) => paintEdge(c, sheet.cellW, sheet.cellH, 'v', len, this.dpr, t.hi, t.edge1, t.edge2), sheet.cellW, len);
    if (!s) return;
    const o = this.batch.add(row.x + row.w / 2 - sx, row.by + m.bh / 2, m.lip, row.w, m.bh, KIND3D.sprite, 1.5);
    this.sprite(o, s, 1);
    this.batch.material(o, MAT3D.wood, GLOSS_WOOD, rand01(`board:${row.b}:${row.r}`));
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
      this.batch.material(o, MAT3D.paint, GLOSS_PAINT);
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
    this.batch.material(o, MAT3D.paint, GLOSS_PAINT);
  }

  /**
   * A tape off the shelf casts a soft shadow on the tapes behind it: its box, seen from
   * the lamp, on the plane just in front of the spines; softer and lighter the further out.
   */
  private dropShadow(t: Active): void {
    const pose = t.pose;
    if (pose.tz < 2) return;
    const { m, lamp } = this;
    const zs = 1;
    const F = this.facesFor(t.tw);
    const pts = boxCorners([this.dx[t.i], this.dy[t.i], this.dz[t.i]], pose, F.origin, t.tw, m.H, m.D);
    let x0 = Infinity;
    let x1 = -Infinity;
    let y0 = Infinity;
    let y1 = -Infinity;
    let lift = 0;
    let n = 0;
    for (const p of pts) {
      // Only what is out in front of the shelf casts a shadow on it.
      if (p[2] <= zs) continue;
      const k = (lamp[2] - zs) / Math.max(1, lamp[2] - p[2]);
      const x = lamp[0] + (p[0] - lamp[0]) * k;
      const y = lamp[1] + (p[1] - lamp[1]) * k;
      x0 = Math.min(x0, x);
      x1 = Math.max(x1, x);
      y0 = Math.min(y0, y);
      y1 = Math.max(y1, y);
      lift += p[2] - zs;
      n++;
    }
    if (!n) return;
    lift /= n;
    const blur = 3 + lift * 0.16;
    const strength = clamp(lift / 24, 0, 1) * (this.theme.dark ? 0.5 : 0.28) * t.a;
    if (strength < 0.01) return;
    const o = this.batch.add((x0 + x1) / 2, (y0 + y1) / 2, zs, x1 - x0 + blur, y1 - y0 + blur, KIND3D.shadow, 4 + blur * 0.3);
    this.batch.color(o, BLACK, strength);
    this.batch.extra(o, blur);
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
    this.batch.material(o, MAT3D.spine, GLOSS_CARD * 0.9, g.l.seed, 1);
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
    const keep = (id: Id) => ids.has(id) || id === this.openId || id === this.focusId || id === this.hoverId || id === this.peekId;
    for (const id of this.wanted) if (!keep(id)) this.wanted.delete(id);
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
    const r = this.focusOnCover ? this.coverRect(i) : null;
    if (r) {
      // Over the cover as it shows (track coordinates), where a screen reader draws its outline.
      f.style.transform = `translate(${(r.x + this.scrollX).toFixed(1)}px, ${r.y.toFixed(1)}px)`;
      f.style.width = `${r.width.toFixed(1)}px`;
      f.style.height = `${r.height.toFixed(1)}px`;
    } else {
      f.style.transform = `translate(${this.X[i]}px, ${this.Y[i]}px)`;
      f.style.width = `${this.widths[i]}px`;
      f.style.height = `${this.m.H}px`;
    }
    this.syncTwin();
  }

  /** The focus element follows the focused tape's cover out and back (only when it turns past halfway, not every frame). */
  private placeFocusOnCover(): void {
    const id = this.focusId;
    if (id === null || !this.focusEl) return;
    const a = this.anims.get(id);
    const shown = !!a && (this.rm ? a.fade.to > 0 : peekPath(a.peek.x).turn > 0.5);
    if (shown === this.focusOnCover) return;
    this.focusOnCover = shown;
    this.placeFocus(id);
  }

  /** The sleeve of tape `i` turned to you (its pose as it ends up), in view coordinates. */
  private coverRect(i: number): DOMRect | null {
    const { m } = this;
    const tw = this.widths[i];
    const F = this.facesFor(tw);
    const pose = tapePose(0, 1, this.dims(i));
    const f = place(F.right, [this.X[i] - this.scrollX, this.Y[i], this.lookOf(this.items[i]).z], pose, F.origin);
    const c = corners(f, { persp: m.persp, originX: this.ox, originY: this.oy });
    if (c.some((q) => !q)) return null;
    const pts = c as { x: number; y: number }[];
    const xs = pts.map((q) => q.x);
    const ys = pts.map((q) => q.y);
    const x0 = Math.min(...xs);
    const y0 = Math.min(...ys);
    return new DOMRect(x0, y0, Math.max(...xs) - x0, Math.max(...ys) - y0);
  }

  // ── scrolling ──

  private scrollTo(x: number, immediate = false): void {
    this.targetX = clamp(x, 0, this.lay.maxX);
    if ((immediate || this.rm) && this.scrollX !== this.targetX) {
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

  /** Point a tape's springs where its state asks: lifted while pointed at or focused, turned while it shows its cover. */
  private refresh(id: Id, instant = false): void {
    const lift = id === this.hoverId || (id === this.focusId && !this.mini) ? 1 : 0;
    const peek = id === this.peekId || id === this.openId ? 1 : 0;
    let a = this.anims.get(id);
    if (!a) {
      if (!lift && !peek) return;
      a = { lift: spring0(), peek: spring0(), fade: new Ease(0), pose: { ...REST } };
      this.anims.set(id, a);
    }
    a.lift.to = lift;
    a.peek.to = peek;
    if (instant) {
      a.lift.x = lift;
      a.lift.v = 0;
      a.peek.x = peek;
      a.peek.v = 0;
    }
    a.fade.set(peek, clock(), instant ? 0 : FADE_S);
    this.invalidate();
  }

  private setHover(id: Id | null): void {
    if (id === this.hoverId) return;
    const prev = this.hoverId;
    this.hoverId = id;
    this.hoverSince = performance.now();
    if (prev !== null) this.refresh(prev);
    if (id !== null) this.refresh(id);
    this.canvas.classList.toggle(styles.pointing, id !== null);
    if (id === null) {
      if (this.intentT) clearTimeout(this.intentT);
      this.intentT = null;
      // Off the tapes: a cover the pointer turned goes back after a moment (a brief excursion keeps it).
      if (this.peekBy === 'pointer' && this.peekId !== null && !this.leaveT) {
        this.leaveT = setTimeout(() => {
          this.leaveT = null;
          if (this.hoverId === null && this.peekBy === 'pointer') this.peekAt(null, 'pointer');
        }, LEAVE_MS);
      }
      return;
    }
    if (this.leaveT) clearTimeout(this.leaveT);
    this.leaveT = null;
    this.wanted.add(id);
    this.armIntent();
  }

  /** When the pointer rests (or moves slowly) on the tape it points at, turn it to show its cover. */
  private armIntent(): void {
    if (this.intentT) clearTimeout(this.intentT);
    this.intentT = null;
    const id = this.hoverId;
    if (id === null || id === this.peekId || this.paused || this.mini) return;
    this.intentT = setTimeout(this.intent, this.intentWait());
  }

  private intentWait(): number {
    const now = performance.now();
    // A pointer that has stopped sends no more moves: its speed is nil.
    const resting = !this.pointer || now - this.pointer.t > 45;
    const wait = peekWait(now - this.hoverSince, resting ? 0 : this.speed);
    return Number.isFinite(wait) ? Math.max(0, wait) : 50;
  }

  private intent = (): void => {
    this.intentT = null;
    const id = this.hoverId;
    if (id === null || id === this.peekId || this.dead || this.dom || this.paused) return;
    const wait = this.intentWait();
    if (wait > 0) {
      this.intentT = setTimeout(this.intent, Math.max(16, wait));
      return;
    }
    this.peekAt(id, 'pointer');
  };

  /** Turn this tape's cover to you (null: none); the one showing goes back as it does. */
  private peekAt(id: Id | null, by: PeekBy): void {
    if (this.leaveT) clearTimeout(this.leaveT);
    this.leaveT = null;
    const prev = this.peekId;
    this.peekBy = id === null ? null : by;
    if (id === prev) return;
    this.peekId = id;
    if (prev !== null) this.refresh(prev);
    if (id === null) return;
    const i = this.index.get(id);
    if (i !== undefined) {
      this.ctx.setFocus(id);
      this.ctx.intent(this.items[i]);
    }
    this.wanted.add(id);
    this.refresh(id);
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
    this.focusOnCover = false;
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

  /** Whether this lens is still the one on screen and this tape still the one opening (a lens switch mid-turn skips the hand-off). */
  private live(id: Id): boolean {
    return !this.dead && this.openId === id && activeLens()?.view === this;
  }

  private openTape(id: Id): void {
    if (!this.index.has(id)) return;
    if (this.openId !== null && this.openId !== id) this.closeTape(this.openId);
    this.setKeyFocus(id);
    this.wanted.add(id);
    this.eagerId = id;
    this.openId = id;
    this.peekAt(id, 'open');
    this.refresh(id);
    // The cover turns to you (if it isn't already), then becomes the detail view (stepSprings hands it over).
    this.opening = id;
    this.later(() => {
      if (this.opening === id) this.handOff(id);
    }, 900);
    this.invalidate();
  }

  private handOff(id: Id): void {
    this.opening = null;
    if (this.live(id)) this.ctx.open(id, null);
  }

  private closeTape(id: Id): void {
    if (this.openId === id) this.openId = null;
    if (this.opening === id) this.opening = null;
    if (this.peekId === id) this.peekAt(null, 'pointer');
    this.refresh(id);
  }

  // ── input ──

  /** The tape off the shelf under a point (view coordinates), front first: what you see there. */
  private activeAt(px: number, py: number): number {
    const cam = { persp: this.m.persp, originX: this.ox, originY: this.oy };
    for (let k = this.act.length - 1; k >= 0; k--) {
      const t = this.act[k];
      for (const f of [t.spine, t.right, t.left, t.top]) {
        if (f && inside(px, py, corners(f, cam))) return t.i;
      }
    }
    return -1;
  }

  /**
   * The tape whose place on the shelf is under a point (view coordinates): its spine
   * (with half the gap either side, so nothing flickers between tapes), else the top or
   * side you see of it. Tapes out of the shelf count by their place, so sweeping across a
   * cover moves on tape by tape; the tape pointed at keeps a few pixels' grace.
   */
  private slotAt(px: number, py: number): number {
    const { m, ox, oy } = this;
    const persp = m.persp;
    const pad = m.gap / 2 + 0.25;
    let side = -1;
    const held = this.hoverId !== null ? this.index.get(this.hoverId) : undefined;
    for (const i of this.drawn) {
      const x = this.dx[i];
      const y = this.dy[i];
      const z = this.dz[i];
      const tw = this.widths[i];
      const w = (persp - z) / persp;
      const grace = i === held ? 3 : 0;
      const x0 = ox + (x - pad - ox) / w - grace;
      const x1 = ox + (x + tw + pad - ox) / w + grace;
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
    const i = this.slotAt(px, py);
    const id = i >= 0 ? this.items[i].id : null;
    if (id === this.hoverId) {
      if (id !== null && !this.intentT) this.armIntent();
    } else this.setHover(id);
  }

  private pointerMoved(x: number, y: number): void {
    const now = performance.now();
    const p = this.pointer;
    if (p) {
      const dt = Math.max(1, now - p.t);
      const v = Math.hypot(x - p.x, y - p.y) / dt;
      // Smoothed over a few moves; a pause between moves counts as slow.
      this.speed = dt > 60 ? v : this.speed * 0.55 + v * 0.45;
    }
    this.pointer = { x, y, t: now };
    this.gx.to = x;
    this.gy.to = y;
    this.relight();
    this.hoverAt(x, y);
  }

  private bind(): void {
    const c = this.canvas;
    const on = <K extends keyof HTMLElementEventMap>(t: HTMLElement, type: K, fn: (e: HTMLElementEventMap[K]) => void, opts?: AddEventListenerOptions) => {
      t.addEventListener(type, fn as EventListener, opts);
      this.cleanup.push(() => t.removeEventListener(type, fn as EventListener, opts));
    };
    // A cover the pointer or a tap turned goes back when the shelf moves.
    const putBack = () => {
      if (this.peekBy === 'pointer' || this.peekBy === 'touch') this.peekAt(null, 'pointer');
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
        putBack();
        this.scrollTo(this.targetX + d * (e.deltaMode === 1 ? 32 : 1));
      },
      { passive: false },
    );
    on(c, 'pointerdown', (e) => {
      this.lastType = e.pointerType;
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
          putBack();
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
      this.pointerMoved(e.offsetX, e.offsetY);
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
      this.relight();
      this.setHover(null);
    });
    on(c, 'click', (e) => {
      if (this.suppressClick) return;
      // What you see under the pointer: a cover that's out, else the tape whose place it is.
      let i = this.activeAt(e.offsetX, e.offsetY);
      if (i < 0) i = this.slotAt(e.offsetX, e.offsetY);
      const touch = this.lastType === 'touch';
      if (i < 0) {
        if (touch) putBack();
        return;
      }
      const id = this.items[i].id;
      // A tap shows the cover first; tapping it again opens it.
      if (touch && id !== this.peekId) {
        this.peekAt(id, 'touch');
        return;
      }
      this.openTape(id);
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
        // A screen reader moving to another tape shows its cover (focus handed back to the same one doesn't).
        const id = idOf(e.target);
        if (id === null || id === this.focusId) return;
        this.setKeyFocus(id);
        this.peekAt(id, 'key');
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
    let next = -1;
    if (cur === undefined) {
      // The first tape on screen.
      let first = 0;
      while (first < n - 1 && this.X[first] - this.scrollX < this.m.padX) first++;
      next = first;
    } else if (dir === 'left' || dir === 'right') {
      next = clamp(cur + (dir === 'right' ? 1 : -1), 0, n - 1);
    } else {
      const { p } = this;
      const bay = p.bay[cur];
      const row = p.row[cur] + (dir === 'down' ? 1 : -1);
      const mid = p.x[cur] + this.widths[cur] / 2;
      let bd = Infinity;
      for (let i = p.bayStart[bay]; i < p.bayStart[bay + 1]; i++) {
        if (p.row[i] !== row) continue;
        const d = Math.abs(p.x[i] + this.widths[i] / 2 - mid);
        if (d < bd) {
          bd = d;
          next = i;
        }
      }
    }
    if (next >= 0) {
      const id = this.items[next].id;
      this.setKeyFocus(id);
      // Keyboard focus shows the cover.
      this.peekAt(id, 'key');
    }
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

  /** The sleeve when the tape is turned towards you, else the spine, as projected on screen (client coordinates). */
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
    const fade = a ? a.fade.value(now) : 0;
    const pose = !a ? REST : this.rm || this.mini ? (fade > 0.5 ? tapePose(0, 1, this.dims(i)) : REST) : a.pose;
    const turned = pose.ry < -Math.PI / 4;
    const f = place(turned ? F.right : F.spine, [this.dx[i], this.dy[i], this.dz[i]], pose, F.origin);
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
    if (paused) {
      if (this.intentT) clearTimeout(this.intentT);
      this.intentT = null;
    }
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
    if (this.intentT) clearTimeout(this.intentT);
    if (this.leaveT) clearTimeout(this.leaveT);
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
