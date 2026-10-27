/**
 * The Gallery on the GPU: the same walk (core.ts) through the same corridor as
 * gallery.ts, drawn with WebGPU (room3d.ts) instead of a hundred CSS 3D layers
 * the browser sorts and composites every frame.
 *
 * - **One instance buffer, three draws.** Walls, floor, light pools, frames, mats,
 *   posters, labels and their reflections are instanced quads in world space; the
 *   camera is a uniform, so walking rewrites no geometry.
 * - **No z-fighting.** A depth buffer (reversed-Z, float32) orders the
 *   architecture. Each side wall, the floor and the ceiling are one continuous
 *   quad (the DOM version overlaps a panel per slot by 2 px, coplanar), and
 *   everything hung on a wall sits a fixed distance in front of it (`OFF`), far
 *   more than float32 depth needs at the end of the corridor (room3d `depthMargin`,
 *   checked in the tests). Where two surfaces of one colour meet, they overlap a
 *   little instead of leaving a seam.
 * - **Text drawn for its size on screen.** Labels are rasterised at a resolution
 *   that follows their projected size (in √2 steps, with hysteresis), so they stay
 *   crisp up close and don't shimmer far away, where they fade out.
 *
 * The DOM keeps the haze and vignette (core.ts), the Tour button, and invisible
 * buttons over the works in view (screen readers, keyboard focus, the item-page flight).
 */
import type { Id, ItemSummary } from '../../api/types';
import { sourceLabel } from '../../components/art-label';
import { artLabel } from '../art';
import { clamp, el } from '../dom';
import { projectedQuad } from '../flight';
import { cssColor, dcColor, hexColor, mixColor, paperOf } from '../gpu/colors';
import type { Gpu } from '../gpu/device';
import { classFor, posterCache, type PosterCache, type PosterHit } from '../gpu/posters';
import { clock, type RGBA } from '../gpu/quads';
import { FLAGR, KINDR, packGradients, prepareRoom3D, Room3DRenderer, roomCorners, type RoomCamera, type RoomPasses } from '../gpu/room3d';
import { inside, Quad3DBatch } from '../gpu/scene3d';
import { textAtlas, wrap, type Glyph, type TextAtlas, type TextStyle } from '../gpu/text';
import { paletteOf } from '../palette';
import { isNew, isWatchedItem, progressFraction } from '../sections';
import type { LensContext, LensModule, LensView, Spot } from '../types';
import { AHEAD, BEHIND, CEIL, DEPTH, FLOOR, FRAME, FRAME_SHIFT, GalleryCore, H, JAMB, LINTEL, MAT, PH, PORTAL_AT, PW, PY, W, pieceLayout, plural, slotRange, wallLines, type EndSlot, type PieceLayout, type RoomSlot, type WorkSlot } from './core';
import styles from './gallery.module.css';

const DEG = Math.PI / 180;
/** The near plane, px from the eye (nothing comes closer; reversed-Z keeps precision far away). */
export const NEAR = 8;
/** Farthest a surface can be from the eye: the corridor ahead, across, and the end wall. */
export const FARTHEST = AHEAD + 2 * W;

/**
 * How far in front of its surface each decal layer sits, px. Within a piece the
 * layers are also drawn in this order; against the wall it is the depth test
 * that keeps them in front, at any distance in view.
 */
/** A frame's backing board, seen while its print is away (flying). */
const BACKING: RGBA = [0.13, 0.125, 0.12, 1];

export const OFF = {
  spot: 1,
  shadow: 1.5,
  frame: 2,
  label: 2,
  mat: 2.5,
  matShade: 2.6,
  bevelLine: 2.7,
  bevel: 2.8,
  art: 3,
  marks: 3.2,
  glaze: 3.6,
  /** On a doorway's lintel, the end wall, the ceiling. */
  face: 0.75,
  door: 1,
} as const;

/** How far the eye is from a work it stands at (the walk's STAND, and its turn toward the wall), roughly. */
const STAND_DIST = 640;

/** The smallest of the gaps above: the one that must survive float32 depth at FARTHEST. */
export const MIN_GAP = Math.min(OFF.spot, OFF.face);

const WHITE: RGBA = [1, 1, 1, 1];
const CLEAR: RGBA = [0, 0, 0, 0];
const INK: RGBA = [0.914, 0.902, 0.875, 1];
const PILL: RGBA = [8 / 255, 8 / 255, 10 / 255, 1];
const BLACK: RGBA = [0, 0, 0, 1];

/** Raster scales for world text, √2 apart. */
const LODS = [0.35, 0.5, 0.71, 1, 1.41, 2, 2.83, 4];

/**
 * The raster scale for text needing `need` device px per world px: the smallest
 * step at or above it, kept while it still fits (`prev`), so walking doesn't redraw
 * labels at every step.
 */
export function textLod(need: number, prev = 0): number {
  if (prev && need <= prev * 1.15 && need >= prev * 0.55) return prev;
  for (const l of LODS) if (l >= need * 0.95) return l;
  return LODS[LODS.length - 1];
}

/** A wall rect's quad: the plane runs from `dStart` along the corridor for `len` (CSS `wallT`). */
export function wallQuad(side: number, dStart: number, len: number, lx: number, top: number, w: number, h: number, off: number): { x: number; y: number; z: number; w: number; h: number; ry: number } {
  const cx = lx + w / 2;
  const d = side < 0 ? dStart + cx : dStart + len - cx;
  return { x: side < 0 ? -W / 2 + off : W / 2 - off, y: top + h / 2, z: -d, w, h, ry: side < 0 ? Math.PI / 2 : -Math.PI / 2 };
}

type GradName = 'wall' | 'ceil' | 'stripe' | 'sheen' | 'board' | 'pool' | 'poolTop' | 'fixture' | 'fixtureGlow' | 'frame' | 'glaze' | 'jamb' | 'jambSide' | 'end' | 'door';

interface Theme {
  floor: RGBA;
  mat: RGBA;
  shadow: RGBA;
  bevel: RGBA;
  ink: RGBA;
  ink2: RGBA;
  ink3: RGBA;
  away: RGBA;
  jamb: RGBA;
  jambSide: RGBA;
  door: RGBA;
  doorGlow: RGBA;
  body: string;
  mono: string;
  /** Colours the gradients are made of (the door's end follows the room's tint). */
  stops: Record<string, RGBA>;
}

/** A label's lines, laid out in its box (px from its top-left). */
interface LabelLine {
  text: string;
  style: TextStyle;
  color: 'ink' | 'ink2' | 'ink3' | 'away';
  x: number;
  top: number;
  lh: number;
  dot?: 'plain' | 'fresh' | 'away';
}

interface Lod {
  r: number;
}

class GpuGalleryView extends GalleryCore {
  private canvas: HTMLCanvasElement;
  private renderer: Room3DRenderer;
  private posters: PosterCache;
  private text: TextAtlas;
  private batch = new Quad3DBatch(1024);
  private passes: RoomPasses = { reflections: 0, architecture: 0, decals: 0 };
  private theme: Theme;
  private grad: Record<GradName, number> = {} as Record<GradName, number>;
  private gradKey = '';
  private tint = '#3a3836';
  private dpr = 1;
  private paintRaf = 0;
  private lastPaint = 0;
  private lastTrim = 0;
  private gone = false;
  private dom: LensView | null = null;
  private hidden: Id | null = null;
  private hover = -1;
  private spot = new Map<number, number>();
  private lods = new Map<string, Lod>();
  private layouts = new Map<string, LabelLine[]>();
  private layoutGen = -1;
  private opts = new Map<Id, HTMLButtonElement>();
  private eye: [number, number, number] = [0, 0, 0];
  /** The batch is out of date (content, focus, hover, art arriving); else only the camera moves. */
  private needBuild = true;
  /** What the last batch was built for (see `signature`). */
  private builtFor = 0;
  private spotsMoving = false;
  /** Wrapped lines of texts that don't change (room introductions, titles set on paper). */
  private wraps = new Map<string, string[]>();
  private wrapGen = -1;
  /** Each work's frame and label boxes (they depend on its art's shape and the orientation). */
  private layoutCache = new Map<WorkSlot, PieceLayout>();
  private layoutPortrait: boolean | null = null;
  /** A label's glyphs at its raster step (looked up once, not at every build). */
  private labelGlyphs = new Map<string, (Glyph | null)[]>();

  constructor(root: HTMLElement, ctx: LensContext, gpu: Gpu) {
    super(root, ctx);
    this.canvas = el('canvas', styles.canvas, { 'aria-hidden': 'true' });
    root.append(this.canvas);
    this.renderer = new Room3DRenderer(gpu, this.canvas);
    this.posters = posterCache(gpu);
    this.text = textAtlas(gpu);
    this.theme = this.readTheme();
    // Holding the poster cache keeps it from being freed while the Gallery draws from it.
    this.cleanup.push(this.posters.retain(), this.posters.onReady(this.dirty), this.text.onChange(this.dirty), gpu.onLost(() => this.fallBack()));
    this.watchTheme();
    if (!this.mini) this.on('pointermove', (e) => this.pointer(e));
    this.start();
  }

  // ── the theme ──

  private readTheme(): Theme {
    const cs = getComputedStyle(this.root);
    const v = (k: string) => cs.getPropertyValue(k);
    const c = (k: string, d: RGBA) => cssColor(v(k), d);
    const stops: Record<string, RGBA> = {
      wall: c('--gal-wall', [0.125, 0.118, 0.106, 1]),
      hi: c('--gal-wall-hi', [0.16, 0.15, 0.137, 1]),
      lo: c('--gal-wall-lo', [0.082, 0.078, 0.071, 1]),
      skirt: c('--gal-skirt', [0.035, 0.031, 0.027, 1]),
      board: c('--gal-board', [1, 1, 1, 0.028]),
      sheen: c('--gal-sheen', [1, 0.91, 0.78, 0.07]),
      ceil: c('--gal-ceil', [0.047, 0.043, 0.043, 1]),
      ceilLight: c('--gal-ceil-light', [1, 0.925, 0.82, 0.2]),
      fixture: c('--gal-fixture', [1, 0.93, 0.84, 0.95]),
      fixtureGlow: c('--gal-fixture-glow', [1, 0.886, 0.73, 0.12]),
      spot: c('--gal-spot', [1, 0.878, 0.714, 0.3]),
      spot2: c('--gal-spot-2', [1, 0.878, 0.714, 0.07]),
      frameA: c('--gal-frame-a', [0.118, 0.118, 0.118, 1]),
      frameB: c('--gal-frame-b', [0.024, 0.024, 0.024, 1]),
      jamb: c('--gal-jamb', [0.149, 0.141, 0.129, 1]),
      jambSide: c('--gal-jamb-side', [0.078, 0.075, 0.071, 1]),
      door: c('--gal-door', [1, 0.953, 0.878, 1]),
    };
    return {
      floor: c('--gal-floor', [0.075, 0.071, 0.063, 0.9]),
      mat: c('--gal-mat', [0.94, 0.93, 0.9, 1]),
      shadow: c('--gal-shadow', [0, 0, 0, 0.78]),
      bevel: c('--gal-bevel', [0.984, 0.98, 0.965, 1]),
      ink: c('--gal-ink', [0.93, 0.906, 0.867, 1]),
      ink2: c('--gal-ink-2', [0.93, 0.906, 0.867, 0.66]),
      ink3: c('--gal-ink-3', [0.93, 0.906, 0.867, 0.42]),
      away: c('--gal-away', [1, 0.54, 0.46, 1]),
      jamb: stops.jamb,
      jambSide: stops.jambSide,
      door: stops.door,
      doorGlow: c('--gal-door-glow', [1, 0.894, 0.745, 0.32]),
      body: v('--font-body').trim() || 'system-ui, sans-serif',
      mono: v('--font-mono').trim() || 'ui-monospace, monospace',
      stops,
    };
  }

  /** The gradient table: the theme's, and the door's end, which takes the room's tint. */
  private gradients(): void {
    const key = `${this.tint}`;
    if (key === this.gradKey && this.grad.wall !== undefined) return;
    this.gradKey = key;
    const s = this.theme.stops;
    const tint = hexColor(this.tint.startsWith('#') ? this.tint : '#3a3836');
    const list: [GradName, [number, RGBA][]][] = [
      ['wall', [[0, s.lo], [0.16, s.wall], [0.44, s.hi], [0.78, s.wall], [0.972, s.lo], [0.972, s.skirt], [1, s.skirt]]],
      ['ceil', [[0, s.ceil], [0.5, mixColor(s.ceil, s.ceilLight, 0.18)], [1, s.ceil]]],
      // The light strip along the ceiling: 47.2–52.8% of its width, full from 49.4 to 50.6%.
      ['stripe', [[0, CLEAR], [0.393, s.ceilLight], [0.607, s.ceilLight], [1, CLEAR]]],
      ['sheen', [[0.08, CLEAR], [0.5, s.sheen], [0.92, CLEAR]]],
      ['board', [[0, s.board]]],
      ['pool', [[0, s.spot], [0.55, s.spot2], [0.82, CLEAR]]],
      ['poolTop', [[0, mixColor(s.spot, WHITE, 0.3)], [1, CLEAR]]],
      ['fixture', [[0, s.fixture], [1, CLEAR]]],
      ['fixtureGlow', [[0, s.fixtureGlow], [1, CLEAR]]],
      ['frame', [[0, s.frameA], [0.6, s.frameB]]],
      ['glaze', [[0.3, CLEAR], [0.41, [1, 1, 1, 0.1]], [0.52, [1, 1, 1, 0.02]], [0.6, CLEAR]]],
      ['jamb', [[0, s.jamb], [0.972, s.jamb], [0.972, s.skirt], [1, s.skirt]]],
      ['jambSide', [[0, s.jambSide], [0.972, s.jambSide], [0.972, s.skirt], [1, s.skirt]]],
      ['end', [[0, s.lo], [0.3, s.wall], [0.972, s.wall], [0.972, s.skirt], [1, s.skirt]]],
      ['door', [[0, s.door], [1, mixColor(s.door, tint, 0.12)]]],
    ];
    const packed = packGradients(list.map(([, g]) => g));
    list.forEach(([name], i) => (this.grad[name] = packed.offsets[i]));
    this.renderer.setGradients(packed.data);
  }

  private watchTheme(): void {
    const refresh = () => {
      this.theme = this.readTheme();
      this.gradKey = '';
      this.dirty();
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

  // ── the walk's hooks ──

  protected surface(): HTMLElement {
    return this.canvas;
  }

  protected resized(flipped: boolean): void {
    this.dpr = this.pixelRatio();
    this.renderer.resize(this.vw * this.dpr, this.vh * this.dpr);
    if (flipped) this.layouts.clear();
  }

  private pixelRatio(): number {
    let dpr = Math.min(window.devicePixelRatio || 1, 3);
    // A preview in the lens gallery is shown scaled down: draw only the pixels it shows.
    if (this.mini) {
      const shown = this.root.getBoundingClientRect().width;
      dpr = Math.min(dpr, Math.max(0.5, (dpr * shown) / Math.max(1, this.vw)) * 1.25);
    }
    return dpr;
  }

  protected render(): void {
    if (this.ticking) this.paint();
    else this.invalidate();
  }

  protected showChanged(): void {
    // Slots were hung anew: works may have moved to other indices.
    this.labelGlyphs.clear();
    this.dirty();
    if (!this.moving) this.syncOptions();
  }

  protected focused(idx: number): void {
    const w = this.works[idx];
    if (w) this.tint = paletteOf(w.item).c1;
    this.dirty();
  }

  protected settled(): void {
    this.syncOptions();
  }

  /** Something on the walls changed: build the batch again at the next frame. */
  private dirty = (): void => {
    this.needBuild = true;
    this.invalidate();
  };

  private invalidate = (): void => {
    if (this.paintRaf || this.gone || this.dom) return;
    this.paintRaf = requestAnimationFrame(() => {
      this.paintRaf = 0;
      // The walk paints this frame anyway.
      if (!this.moving) this.paint();
    });
  };

  // ── a frame ──

  private camera(time: number): RoomCamera {
    const p = this.pose;
    return { width: this.vw, height: this.vh, centerX: this.vw / 2, centerY: this.vh / 2, persp: this.P, yaw: p.yaw * DEG, eyeX: -p.x, eyeY: -p.y, eyeZ: p.d, near: NEAR, dpr: this.dpr, time };
  }

  /**
   * A frame. The batch is in world space, so a step of the walk only sends the
   * camera; it is built again when what is on the walls changes, or when the walk
   * changes what the batch depends on (`signature`): the stretch in view, the size
   * text and posters are drawn at, a label fading, the light pools easing.
   */
  private paint(): void {
    if (this.gone || this.dom) return;
    if (Math.abs(this.pixelRatio() - this.dpr) > 0.01) {
      this.resized(false);
      this.needBuild = true;
    }
    const now = clock();
    const dt = this.lastPaint ? Math.min(0.1, now - this.lastPaint) : 1 / 60;
    this.lastPaint = now;
    const posters = this.posters;
    // Upload first: a slot drawn in the last frame is still "in use" and can't be evicted for a new one.
    posters.upload();
    this.gradients();
    const sig = this.signature();
    const fresh = this.needBuild || this.spotsMoving || sig !== this.builtFor;
    if (fresh) {
      this.needBuild = false;
      this.builtFor = sig;
      posters.begin();
      const gen = this.text.gen;
      this.build(dt);
      // The glyph atlas started over while building: build again with fresh glyphs.
      if (this.text.gen !== gen) this.build(0);
      this.text.flush();
    }
    this.renderer.draw(this.batch, this.passes, this.camera(now), { posters: posters.views(), glyphs: this.text.view, key: String(posters.version) }, fresh);
    posters.pump();
    if (performance.now() - this.lastTrim > 5000) {
      this.lastTrim = performance.now();
      posters.trim();
    }
    const busy = this.spotsMoving || posters.pending || now < posters.fadeUntil;
    if (busy && !this.paused) this.invalidate();
    else this.lastPaint = 0;
  }

  private layoutOf(w: WorkSlot): PieceLayout {
    if (this.layoutPortrait !== this.portrait || this.layoutCache.size > 3000) {
      this.layoutCache.clear();
      this.labelGlyphs.clear();
      this.layoutPortrait = this.portrait;
    }
    let L = this.layoutCache.get(w);
    if (!L) {
      L = pieceLayout(w.item, this.portrait === true);
      this.layoutCache.set(w, L);
    }
    return L;
  }

  /** The eye's distance to a point `lx` along work w's piece, at world height `y`, `off` from the wall. */
  private onPieceDist(w: WorkSlot, lx: number, y: number, off: number): number {
    const d = w.side < 0 ? w.d - PW / 2 + lx : w.d + PW / 2 - lx;
    const x = w.side < 0 ? -W / 2 + off : W / 2 - off;
    return Math.max(this.dist(x, y, -d), NEAR);
  }

  /**
   * The poster size to draw work w with: for the works around you, the size it has
   * when you stand at it (fetched once, as it comes near); further away, its size
   * now, at most the third class. A poster walked towards is fetched twice, not at
   * every size on the way.
   */
  private posterClass(w: WorkSlot, L: PieceLayout, dist: number): number {
    const now = classFor((L.pw * this.P * this.dpr) / dist);
    if (Math.abs(w.i - this.focusIdx) > 2) return Math.min(now, 2);
    return Math.max(now, classFor((L.pw * this.P * this.dpr) / STAND_DIST));
  }

  /** Where work w's art is, and how far from the eye. */
  private artDist(w: WorkSlot, L: PieceLayout): number {
    return this.onPieceDist(w, L.fl + FRAME + MAT + L.pw / 2, PY + L.ft + FRAME + MAT + L.ph / 2, OFF.art);
  }

  private labelDist(w: WorkSlot, L: PieceLayout): number {
    return this.onPieceDist(w, L.lx + L.lw / 2, PY + L.ly + 40, OFF.label);
  }

  /** The text raster step a block would get now (as `lodFor` decides it), for the signature. */
  private lodStep(key: string, scale: number): number {
    return Math.round(textLod(scale * this.dpr, this.lods.get(key)?.r) * 100);
  }

  /**
   * A hash of what the batch depends on besides content: the slots in view, a coarse
   * position (the decals' order, now and then), and for each thing in view the text
   * and poster sizes it is drawn at and its label's fade. Cheap: a few distances.
   */
  private signature(): number {
    const p = this.pose;
    this.eye = [p.x, p.y, -p.d];
    const [a, z] = slotRange(this.slots, p.d - BEHIND, p.d + (this.mini ? 3600 : AHEAD));
    let h = (a * 7919 + z) | 0;
    h = (h * 31 + Math.round(p.d / 140)) | 0;
    h = (h * 31 + (this.portrait ? 1 : 0)) | 0;
    for (let s = a; s < z; s++) {
      const sl = this.slots[s];
      if (sl.type === 'work') {
        const L = this.layoutOf(sl);
        const ad = this.artDist(sl, L);
        const ls = this.P / this.labelDist(sl, L);
        h = (h * 31 + (sl.item.poster ? this.posterClass(sl, L, ad) : 9)) | 0;
        h = (h * 31 + this.lodStep(`m${sl.i}`, this.P / ad)) | 0;
        h = (h * 31 + this.lodStep(`l${sl.i}`, ls)) | 0;
        h = (h * 31 + Math.round(clamp((17 * ls - 2.5) / 2.5, 0, 1) * 8)) | 0;
      } else if (sl.type === 'room') {
        const dp = sl.d0 + PORTAL_AT;
        if (sl.portal) h = (h * 31 + this.lodStep(`n${sl.d0}`, this.P / Math.max(this.dist(0, CEIL + LINTEL / 2, -dp), NEAR))) | 0;
        const at = this.roomTextAt(sl);
        h = (h * 31 + this.lodStep(`r${sl.d0}`, this.P / Math.max(this.dist(-W / 2, -65, -(at + 270)), NEAR))) | 0;
      } else {
        h = (h * 31 + this.lodStep(`e${sl.d0}`, this.P / Math.max(this.dist(0, FLOOR - 469, -sl.d1), NEAR))) | 0;
      }
    }
    return h;
  }

  /** Lines of `text` wrapped to `max` in `style`, kept (fonts redrawn: the atlas's gen). */
  private wrapped(text: string, max: number, lines: number, style: TextStyle): string[] {
    if (this.wrapGen !== this.text.gen || this.wraps.size > 400) {
      this.wraps.clear();
      this.wrapGen = this.text.gen;
    }
    const key = `${style.family}|${style.size}|${style.weight}|${max}|${lines}|${text}`;
    let out = this.wraps.get(key);
    if (!out) {
      out = wrap(text, max, lines, (s) => this.text.measure(s, style));
      this.wraps.set(key, out);
    }
    return out;
  }

  /** The eye's distance to a world point. */
  private dist(x: number, y: number, z: number): number {
    const e = this.eye;
    const dx = x - e[0];
    const dy = y - e[1];
    const dz = z - e[2];
    return Math.sqrt(dx * dx + dy * dy + dz * dz);
  }

  private build(dt: number): void {
    const b = this.batch;
    b.reset();
    const p = this.pose;
    this.eye = [p.x, p.y, -p.d];
    const [a, z] = slotRange(this.slots, p.d - BEHIND, p.d + (this.mini ? 3600 : AHEAD));
    if (a >= z) {
      this.passes = { reflections: 0, architecture: 0, decals: 0 };
      return;
    }
    const works: WorkSlot[] = [];
    for (let s = a; s < z; s++) {
      const sl = this.slots[s];
      if (sl.type === 'work') works.push(sl);
    }
    this.stepSpots(works, dt);

    // 1. Reflections, under the floor: far to near.
    const reflected = works.filter((w) => !this.mini || w.i < 6);
    this.sortGroups(reflected.map((w) => ({ dist: this.pieceDist(w), draw: () => this.piece(w, true) })));
    const n0 = b.n;

    // 2. Architecture: the floor over the reflections, the ceiling, the walls, then doorways and the end wall.
    const dA = this.slots[a].d0 - 2;
    const dB = this.slots[z - 1].d1 + 2;
    this.shell(dA, dB);
    const extras: { dist: number; draw: () => void }[] = [];
    for (let s = a; s < z; s++) {
      const sl = this.slots[s];
      if (sl.type === 'room' && sl.portal) extras.push({ dist: this.dist(0, 0, -(sl.d0 + PORTAL_AT)), draw: () => this.doorway(sl) });
      else if (sl.type === 'end') extras.push({ dist: this.dist(0, 0, -sl.d1), draw: () => this.endWall(sl) });
    }
    this.sortGroups(extras);
    const n1 = b.n;

    // 3. Decals, far to near: light pools, frames, prints, labels, room texts, the end's text and door, ceiling lights.
    const decals: { dist: number; draw: () => void }[] = [];
    decals.push({ dist: Infinity, draw: () => this.stripe(dA, dB) });
    for (const w of works) {
      decals.push({ dist: this.pieceDist(w), draw: () => this.piece(w, false) });
      const fx = w.side * (W / 2 - 170);
      const fd = this.frameD(w);
      decals.push({ dist: this.dist(fx, CEIL, -fd), draw: () => this.fixture(fx, fd) });
    }
    for (let s = a; s < z; s++) {
      const sl = this.slots[s];
      if (sl.type === 'room') {
        if (sl.portal && sl.label) decals.push({ dist: this.dist(0, CEIL + LINTEL / 2, -(sl.d0 + PORTAL_AT)), draw: () => this.lintelText(sl) });
        const at = this.roomTextAt(sl);
        if (sl.label) decals.push({ dist: this.dist(-W / 2, -65, -(at + 270)), draw: () => this.roomText(sl, at) });
      } else if (sl.type === 'end') decals.push({ dist: this.dist(0, 0, -sl.d1), draw: () => this.endText(sl) });
    }
    this.sortGroups(decals);
    this.passes = { reflections: n0, architecture: n1 - n0, decals: b.n - n1 };
  }

  private sortGroups(groups: { dist: number; draw: () => void }[]): void {
    groups.sort((x, y) => y.dist - x.dist);
    for (const g of groups) g.draw();
  }

  private pieceDist(w: WorkSlot): number {
    const q = wallQuad(w.side, w.d - PW / 2, PW, PW / 2, PY, 0, PH, 0);
    return this.dist(q.x, q.y, q.z);
  }

  /** The light pools ease between 0.72 and 1 as the focus moves (CSS: 0.9 s). */
  private stepSpots(works: WorkSlot[], dt: number): void {
    const k = this.rm ? 1 : 1 - Math.exp(-Math.max(dt, 1 / 120) / 0.3);
    const keep = new Map<number, number>();
    let moving = false;
    for (const w of works) {
      const target = w.i === this.focusIdx ? 1 : 0.72;
      const prev = this.spot.get(w.i);
      let cur = prev === undefined ? target : prev + (target - prev) * k;
      if (Math.abs(target - cur) < 0.004) cur = target;
      else moving = true;
      keep.set(w.i, cur);
    }
    this.spot = keep;
    this.spotsMoving = moving;
  }

  // ── the building ──

  /** Floor, ceiling and both walls: one quad each over the stretch in view (no seams to fight over). */
  private shell(dA: number, dB: number): void {
    const b = this.batch;
    const len = dB - dA;
    const zc = -(dA + dB) / 2;
    const t = this.theme;
    // The floor reaches under the walls, so no gap can open at the corner.
    let o = b.add(0, FLOOR, zc, W + 4, len, KINDR.floor);
    b.rotate(o, Math.PI / 2, 0);
    b.color(o, t.floor);
    b.uv(o, 125, 2, 2, 0);
    b.texture(o, this.grad.sheen, this.grad.board, -1);
    b.flags(o, FLAGR.noaa);
    o = b.add(0, CEIL, zc, W + 4, len, KINDR.linear);
    b.rotate(o, -Math.PI / 2, 0);
    b.color(o, WHITE);
    b.uv(o, 90 * DEG, 0, 0, 0);
    b.texture(o, this.grad.ceil, 0, -1);
    b.flags(o, FLAGR.noaa);
    for (const side of [-1, 1]) {
      const q = wallQuad(side, dA, len, 0, CEIL, len, H, 0);
      o = b.add(q.x, q.y, q.z, q.w, q.h, KINDR.linear);
      b.rotate(o, 0, q.ry);
      b.color(o, WHITE);
      b.uv(o, 180 * DEG, 0, 0, 0);
      b.texture(o, this.grad.wall, 0, -1);
      b.flags(o, FLAGR.noaa);
    }
  }

  /** The light strip along the ceiling. */
  private stripe(dA: number, dB: number): void {
    const b = this.batch;
    const o = b.add(0, CEIL + OFF.face, -(dA + dB) / 2, 56, dB - dA, KINDR.linear);
    b.rotate(o, -Math.PI / 2, 0);
    b.color(o, WHITE);
    b.uv(o, 90 * DEG, 0, 0, 0);
    b.texture(o, this.grad.stripe, 0, -1);
  }

  /** A track light on the ceiling: the fixture and its glow. */
  private fixture(x: number, d: number): void {
    const b = this.batch;
    for (const [w, h, g] of [
      [300, 220, this.grad.fixtureGlow],
      [44, 30, this.grad.fixture],
    ] as const) {
      const o = b.add(x, CEIL + OFF.face, -d, w, h, KINDR.radial);
      b.rotate(o, -Math.PI / 2, 0);
      b.color(o, WHITE);
      b.uv(o, 0.5, 0.5, 0.5, 0.5);
      b.texture(o, g, 0, -1);
    }
  }

  private doorway(sl: RoomSlot): void {
    const b = this.batch;
    const dp = sl.d0 + PORTAL_AT;
    const t = this.theme;
    // The jambs' inner sides and the soffit first: the face, drawn last, antialiases
    // its opening over them. Their own edges all meet other surfaces, so they need none.
    for (const side of [-1, 1]) {
      const q = wallQuad(side, dp, DEPTH, 0, CEIL, DEPTH, H, JAMB);
      const o = b.add(q.x, q.y, q.z, q.w, q.h, KINDR.linear);
      b.rotate(o, 0, q.ry);
      b.color(o, WHITE);
      b.uv(o, 180 * DEG, 0, 0, 0);
      b.texture(o, this.grad.jambSide, 0, -1);
      b.flags(o, FLAGR.noaa);
    }
    let o = b.add(0, CEIL + LINTEL, -(dp + DEPTH / 2), W - 2 * JAMB, DEPTH, KINDR.solid);
    b.rotate(o, -Math.PI / 2, 0);
    b.color(o, t.jambSide);
    b.flags(o, FLAGR.noaa);
    // The face: jambs and lintel as one quad with the opening cut out, reaching 2 px
    // into the walls, the ceiling and the floor, where the depth test hides it.
    o = b.add(0, CEIL + H / 2, -dp, W + 4, H + 4, KINDR.linear);
    b.color(o, WHITE);
    b.uv(o, 180 * DEG, W / 2 - JAMB, LINTEL + 2 - (H + 4) / 2, 0);
    b.texture(o, this.grad.jamb, 0, -1);
    b.flags(o, FLAGR.noaa | FLAGR.cut);
  }

  private endWall(sl: EndSlot): void {
    const b = this.batch;
    // Reaching into the walls, ceiling and floor (hidden there), so no edge shows.
    const o = b.add(0, CEIL + H / 2, -sl.d1, W + 4, H + 4, KINDR.linear);
    b.color(o, WHITE);
    b.uv(o, 180 * DEG, 0, 0, 0);
    b.texture(o, this.grad.end, 0, -1);
    b.flags(o, FLAGR.noaa);
  }

  // ── what hangs ──

  /** A rect on work w's wall in the piece's px, `off` in front of the wall (or its reflection). */
  private onPiece(w: WorkSlot, lx: number, ly: number, ww: number, hh: number, off: number, kind: number, radius: number, mirror: boolean): number {
    const q = wallQuad(w.side, w.d - PW / 2, PW, lx, PY + ly, ww, hh, off);
    const b = this.batch;
    const o = b.add(q.x, q.y, q.z, q.w, q.h, kind, radius);
    b.rotate(o, 0, q.ry);
    if (mirror) b.mirror(o, FLOOR, 0.5);
    return o;
  }

  private piece(w: WorkSlot, mirror: boolean): void {
    const b = this.batch;
    const it = w.item;
    const L = this.layoutOf(w);
    const t = this.theme;
    const lift = !mirror && w.i === this.hover ? 0.08 : 0;
    const fx = (PW / 2 - FRAME_SHIFT) / PW;
    // The light pool, and where the light strikes the wall above it.
    const spot = mirror ? 0.4 : (this.spot.get(w.i) ?? 0.72);
    for (const [g, uv] of [
      [this.grad.pool, [fx, 0.46, 0.33, 0.52]],
      [this.grad.poolTop, [fx, 0, 0.3, 0.07]],
    ] as const) {
      const o = this.onPiece(w, 0, 0, PW, PH, OFF.spot, KINDR.radial, 0, mirror);
      b.color(o, WHITE);
      b.uv(o, uv[0], uv[1], uv[2], uv[3]);
      b.texture(o, g, 0, -1);
      b.opacity(o, spot);
    }
    const { fl, ft, fw, fh } = L;
    if (!mirror) {
      // The frame's two shadows (CSS: 0 44px 46px -34px, 0 10px 20px -8px).
      for (const [oy, blur, spread, z] of [
        [44, 46, -34, 0],
        [10, 20, -8, 0.05],
      ] as const) {
        const sw = fw + 2 * spread + 2 * blur;
        const sh = fh + 2 * spread + 2 * blur;
        const o = this.onPiece(w, fl + fw / 2 - sw / 2, ft + fh / 2 + oy - sh / 2, sw, sh, OFF.shadow + z, KINDR.shadow, 1, false);
        b.color(o, t.shadow);
        b.extra(o, blur);
      }
    }
    let o = this.onPiece(w, fl, ft, fw, fh, OFF.frame, KINDR.linear, 1, mirror);
    b.color(o, WHITE);
    b.uv(o, 150 * DEG, 0, 0, 0);
    b.texture(o, this.grad.frame, 0, -1);
    b.flags(o, FLAGR.hairline | (mirror ? FLAGR.mirror : 0));
    b.sheen(o, lift);
    const ml = fl + FRAME;
    const mt = ft + FRAME;
    const mw = fw - 2 * FRAME;
    const mh = fh - 2 * FRAME;
    o = this.onPiece(w, ml, mt, mw, mh, OFF.mat, KINDR.solid, 0, mirror);
    b.color(o, t.mat);
    b.sheen(o, lift);
    if (!mirror) {
      o = this.onPiece(w, ml, mt + 1, mw, mh, OFF.matShade, KINDR.shadow, 0, false);
      b.color(o, [0, 0, 0, 0.22]);
      b.extra(o, 4);
      b.flags(o, FLAGR.inset);
      o = this.onPiece(w, ml, mt, mw, mh, OFF.matShade + 0.02, KINDR.ring, 0, false);
      b.color(o, [0, 0, 0, 0.05]);
      b.extra(o, 1);
    }
    const al = ml + MAT;
    const at = mt + MAT;
    // The bevel: a 3 px white edge, and a faint line outside it.
    if (!mirror) {
      o = this.onPiece(w, al - 4, at - 4, L.pw + 8, L.ph + 8, OFF.bevelLine, KINDR.ring, 0, false);
      b.color(o, [0, 0, 0, 0.1]);
      b.extra(o, 1);
    }
    o = this.onPiece(w, al - 3, at - 3, L.pw + 6, L.ph + 6, OFF.bevel, KINDR.solid, 0, mirror);
    b.color(o, t.bevel);
    b.sheen(o, lift);
    if (it.id !== this.hidden) this.print(w, L, al, at, mirror, lift);
    else {
      // Its print is flying (to the detail page, or to another lens): the frame's backing
      // board shows where it was, dark (the bare bevel would be a bright white sheet).
      o = this.onPiece(w, al, at, L.pw, L.ph, OFF.art, KINDR.solid, 0, mirror);
      b.color(o, BACKING);
      b.sheen(o, lift);
    }
    o = this.onPiece(w, fl, ft, fw, fh, OFF.glaze, KINDR.linear, 1, mirror);
    b.color(o, WHITE);
    b.uv(o, 116 * DEG, 0, 0, 0);
    b.texture(o, this.grad.glaze, 0, -1);
    if (!mirror) this.label(w, L);
  }

  /** The print: its poster (or placeholder), and the marks art.ts puts on it. */
  private print(w: WorkSlot, L: PieceLayout, al: number, at: number, mirror: boolean, lift: number): void {
    const b = this.batch;
    const it = w.item;
    const q = wallQuad(w.side, w.d - PW / 2, PW, al, PY + at, L.pw, L.ph, OFF.art);
    const dist = this.dist(q.x, q.y, q.z);
    let hit: PosterHit | null = null;
    if (it.poster) hit = this.posters.get(it.poster, this.posterClass(w, L, Math.max(dist, NEAR)), dist, !this.paused && (!this.mini || w.i < 8));
    const o = this.onPiece(w, al, at, L.pw, L.ph, OFF.art, KINDR.poster, 0, mirror);
    let flags = mirror ? FLAGR.mirror : 0;
    if (!it.available) flags |= FLAGR.gray;
    const dc = it.poster?.blurhash ? dcColor(it.poster.blurhash) : null;
    if (dc) b.color(o, dc);
    else {
      const [c1, c2] = paperOf(it);
      b.color(o, c1);
      b.uv(o, c2[0], c2[1], c2[2], c2[3]);
      flags |= FLAGR.gradient;
    }
    if (hit) {
      b.uv(o, 0, 0, 1, 1);
      b.texture(o, hit.code, hit.texWidth, hit.readyAt);
      flags &= ~FLAGR.gradient;
    }
    b.flags(o, flags);
    b.sheen(o, lift);
    if (mirror) return;
    const scale = this.P / Math.max(dist, NEAR);
    this.marks(w, it, al, at, L.pw, L.ph, scale);
    if (!it.poster && L.pw >= 72) this.typeset(w, it, al, at, L.pw, L.ph, scale);
  }

  /** Text for a block whose letters are `scale` screen px per world px: its raster scale (and fade). */
  private lodFor(key: string, scale: number): { r: number; alpha: number } {
    const need = scale * this.dpr;
    const prev = this.lods.get(key);
    const r = textLod(need, prev?.r);
    if (!prev || prev.r !== r) this.lods.set(key, { r });
    if (this.lods.size > 600) this.lods.clear();
    return { r, alpha: 1 };
  }

  /** A glyph at (left, top) in the piece's px. */
  private glyphOnPiece(w: WorkSlot, g: Glyph, left: number, top: number, off: number, color: RGBA, alpha: number): void {
    const o = this.onPiece(w, left, top, g.w, g.h, off, KINDR.text, 0, false);
    this.batch.color(o, color, alpha);
    this.batch.uv(o, g.u0, g.v0, g.u1, g.v1);
  }

  /** Progress, watched, new and offline marks, and the frame/AI label: as art.ts draws them. */
  private marks(w: WorkSlot, it: ItemSummary, al: number, at: number, AW: number, AH: number, scale: number): void {
    const b = this.batch;
    const { r } = this.lodFor(`m${w.i}`, scale);
    const mono: TextStyle = { family: this.theme.mono, size: 9.28, weight: 500, spacing: 0.08, upper: true };
    const pillH = 15.6;
    let z = OFF.marks;
    const rect = (left: number, top: number, ww: number, hh: number, kind: number, radius: number, color: RGBA, alpha: number) => {
      z += 0.01;
      const o = this.onPiece(w, al + left, at + top, ww, hh, z, kind, radius, false);
      b.color(o, color, alpha);
      return o;
    };
    const glyph = (g: Glyph, left: number, top: number, color: RGBA) => {
      z += 0.01;
      this.glyphOnPiece(w, g, al + left, at + top, z, color, 1);
    };
    // Frame and AI art is always labelled.
    const label = it.poster ? sourceLabel(it.poster) : null;
    if (label) {
      const ai = label.tone === 'ai';
      const g = this.text.get(label.short, mono, 200, r);
      const icon = ai ? this.text.icon('spark', 9, r) : null;
      if (g) {
        const ww = 12 + (g.w - g.pad * 2) + (icon ? 13 : 0);
        rect(6, 6, ww, pillH, KINDR.solid, 4, PILL, ai ? 0.86 : 0.72);
        if (ai) b.extra(rect(6, 6, ww, pillH, KINDR.ring, 4, WHITE, 0.55), 1);
        let tx = 12;
        if (icon) {
          glyph(icon, tx - icon.pad, 6 + (pillH - icon.h) / 2, WHITE);
          tx += 13;
        }
        glyph(g, tx - g.pad, 6 + pillH / 2 + 3.2 - g.baseline, ai ? WHITE : INK);
      }
    }
    const prog = progressFraction(it);
    if (prog !== null) {
      rect(AW * 0.08, AH - 10, AW * 0.84, 3, KINDR.solid, 1.5, WHITE, 0.3);
      rect(AW * 0.08, AH - 10, AW * 0.84 * Math.max(0.03, prog), 3, KINDR.solid, 1.5, WHITE, 1);
    } else if (isWatchedItem(it)) {
      rect(AW - 26, 6, 20, 20, KINDR.solid, 10, PILL, 0.6);
      const icon = this.text.icon('check', 11, r);
      if (icon) glyph(icon, AW - 16 - icon.w / 2, 16 - icon.h / 2, WHITE);
    } else if (isNew(it)) {
      b.extra(rect(AW - 28, -4, 32, 32, KINDR.shadow, 4, WHITE, 0.8), 12);
      rect(AW - 19, 5, 14, 14, KINDR.solid, 7, BLACK, 0.25);
      rect(AW - 16, 8, 8, 8, KINDR.solid, 4, WHITE, 1);
    }
    if (!it.available) {
      const g = this.text.get('Offline', { ...mono, weight: 400 }, 200, r);
      if (g) {
        const ww = 12 + g.w - g.pad * 2;
        rect(6, AH - 6 - pillH, ww, pillH, KINDR.solid, 4, PILL, 0.75);
        glyph(g, 12 - g.pad, AH - 6 - pillH + pillH / 2 + 3.2 - g.baseline, INK);
      }
    }
  }

  /** A title without art: its name set on the paper, bottom-aligned (art.ts `typeset`). */
  private typeset(w: WorkSlot, it: ItemSummary, al: number, at: number, AW: number, AH: number, scale: number): void {
    const size = Math.round(clamp(AW * 0.11, 12.8, 25.6) * 4) / 4;
    const margin = AW * 0.11;
    const max = Math.round(AW - margin * 2);
    const style: TextStyle = { family: this.theme.body, size, weight: 500 };
    const lines = this.wrapped(it.title, max, 4, style);
    const { r } = this.lodFor(`m${w.i}`, scale);
    const lh = size * 1.05;
    let top = AH - margin - lines.length * lh;
    for (const line of lines) {
      const g = this.text.get(line, style, max, r);
      if (g) this.glyphOnPiece(w, g, al + margin - g.pad, at + top + lh * 0.82 - g.baseline, OFF.marks, WHITE, 1);
      top += lh;
    }
  }

  // ── labels ──

  private styles() {
    const t = this.theme;
    return {
      title: { family: t.body, size: 17, weight: 500, spacing: 0.004 } as TextStyle,
      year: { family: t.body, size: 15, weight: 400 } as TextStyle,
      what: { family: t.body, size: 13, weight: 400 } as TextStyle,
      medium: { family: t.mono, size: 10.5, weight: 400, spacing: 0.04 } as TextStyle,
      status: { family: t.body, size: 11.5, weight: 400 } as TextStyle,
    };
  }

  /** A wall label's lines in its box, as the CSS lays them out (line-height 1.4). */
  private labelLayout(w: WorkSlot, lw: number): LabelLine[] {
    if (this.layoutGen !== this.text.gen) {
      this.layouts.clear();
      this.layoutGen = this.text.gen;
    }
    const key = `${w.key}|${lw}`;
    const hit = this.layouts.get(key);
    if (hit) return hit;
    const st = this.styles();
    const l = wallLines(w.item);
    const out: LabelLine[] = [];
    const measure = (s: string, style: TextStyle) => this.text.measure(s, style);
    let top = 0;
    const tlh = 17 * 1.4;
    const titleLines = wrap(l.title, lw, 3, (s) => measure(s, st.title));
    titleLines.forEach((text, i) => {
      out.push({ text, style: st.title, color: 'ink', x: 0, top, lh: tlh });
      if (i < titleLines.length - 1) top += tlh;
    });
    if (l.year) {
      const year = `, ${l.year}`;
      const last = titleLines[titleLines.length - 1] ?? '';
      const x = measure(last, st.title);
      if (x + measure(year, st.year) <= lw) out.push({ text: year, style: st.year, color: 'ink2', x, top, lh: tlh });
      else {
        top += tlh;
        out.push({ text: l.year, style: st.year, color: 'ink2', x: 0, top, lh: tlh });
      }
    }
    top += tlh;
    top += 3;
    out.push({ text: l.what, style: st.what, color: 'ink2', x: 0, top, lh: 13 * 1.4 });
    top += 13 * 1.4;
    if (l.medium) {
      top += 8;
      out.push({ text: l.medium, style: st.medium, color: 'ink3', x: 0, top, lh: 10.5 * 1.4 });
      top += 10.5 * 1.4;
    }
    if (l.status) {
      top += 10;
      const dot = l.tone === 'new' ? 'fresh' : l.tone === 'offline' ? 'away' : 'plain';
      out.push({ text: l.status, style: st.status, color: l.tone === 'offline' ? 'away' : 'ink2', x: 13, top, lh: 11.5 * 1.4, dot });
    }
    if (this.layouts.size > 400) this.layouts.clear();
    this.layouts.set(key, out);
    return out;
  }

  private label(w: WorkSlot, L: PieceLayout): void {
    const scale = this.P / this.labelDist(w, L);
    // Too small to read: let it go rather than shimmer.
    const alpha = clamp((17 * scale - 2.5) / 2.5, 0, 1);
    if (alpha <= 0) return;
    const { r } = this.lodFor(`l${w.i}`, scale);
    const t = this.theme;
    const colors = { ink: t.ink, ink2: t.ink2, ink3: t.ink3, away: t.away };
    const lines = this.labelLayout(w, L.lw);
    // The glyphs for this label at this raster step, looked up once.
    const gk = `${w.i}|${r}|${this.text.gen}`;
    let glyphs = this.labelGlyphs.get(gk);
    if (!glyphs || glyphs.length !== lines.length) {
      glyphs = lines.map((line) => this.text.get(line.text, line.style, L.lw - line.x, r));
      if (glyphs.every(Boolean)) this.labelGlyphs.set(gk, glyphs);
      if (this.labelGlyphs.size > 600) this.labelGlyphs.clear();
    }
    for (let i = 0; i < lines.length; i++) {
      const line = lines[i];
      const g = glyphs[i];
      if (!g) continue;
      const ink = g.h - 2 * g.pad;
      const baseline = line.top + (line.lh - ink) / 2 + (g.baseline - g.pad);
      this.glyphOnPiece(w, g, L.lx + line.x - g.pad, L.ly + baseline - g.baseline, OFF.label, colors[line.color], alpha);
      if (line.dot) {
        const c = line.dot === 'away' ? t.away : line.dot === 'fresh' ? t.ink : t.ink2;
        const cy = L.ly + line.top + line.lh / 2;
        if (line.dot === 'fresh') {
          const o = this.onPiece(w, L.lx - 8, cy - 11, 22, 22, OFF.label, KINDR.shadow, 11, false);
          this.batch.color(o, t.ink, 0.85 * alpha);
          this.batch.extra(o, 8);
        }
        const o = this.onPiece(w, L.lx, cy - 3, 6, 6, OFF.label + 0.01, KINDR.solid, 3, false);
        this.batch.color(o, c, (line.dot === 'plain' ? 0.6 : 1) * alpha);
      }
    }
  }

  // ── rooms and the end ──

  private roomTextAt(sl: RoomSlot): number {
    return sl.portal ? sl.d0 + PORTAL_AT + DEPTH + 210 : sl.d0 + 30;
  }

  /** Text on a wall plane (CSS `wallT`): `top` is world y. */
  private wallGlyph(side: number, dStart: number, len: number, g: Glyph, left: number, top: number, off: number, color: RGBA, alpha = 1): void {
    const q = wallQuad(side, dStart, len, left, top, g.w, g.h, off);
    const b = this.batch;
    const o = b.add(q.x, q.y, q.z, q.w, q.h, KINDR.text);
    b.rotate(o, 0, q.ry);
    b.color(o, color, alpha);
    b.uv(o, g.u0, g.v0, g.u1, g.v1);
  }

  /** Text on a face toward the corridor (a lintel, the end wall), its top-left at (x, y), `d` along. */
  private faceGlyph(g: Glyph, x: number, y: number, d: number, off: number, color: RGBA): void {
    const b = this.batch;
    const o = b.add(x + g.w / 2, y + g.h / 2, -d + off, g.w, g.h, KINDR.text);
    b.color(o, color);
    b.uv(o, g.u0, g.v0, g.u1, g.v1);
  }

  /** The block's baseline for a line box: CSS half-leading. */
  private baselineIn(g: Glyph, top: number, lh: number): number {
    const ink = g.h - 2 * g.pad;
    return top + (lh - ink) / 2 + (g.baseline - g.pad);
  }

  /** The room's title on the left wall: name, room number and what hangs there (bottom-aligned, 30 px in). */
  private roomText(sl: RoomSlot, at: number): void {
    const t = this.theme;
    const scale = this.P / Math.max(this.dist(-W / 2, -65, -(at + 270)), NEAR);
    const { r } = this.lodFor(`r${sl.d0}`, scale);
    const size = Math.min(104, Math.round(1700 / Math.max(5, sl.label.length)));
    const title: TextStyle = { family: t.body, size, weight: 300, spacing: -0.03 };
    const small: TextStyle = { family: t.mono, size: 13, weight: 400, spacing: 0.12, upper: true };
    const body: TextStyle = { family: t.body, size: 19, weight: 400 };
    const by = sl.by ? this.wrapped(sl.by, 420, 4, body) : [];
    const blocks: { g: Glyph | null; lh: number; color: RGBA }[] = [];
    blocks.push({ g: this.text.get(sl.label, title, 2000, r), lh: size * 0.9, color: t.ink });
    blocks.push({ g: this.text.get(sl.sub, small, 2000, r), lh: 13 * 1.2, color: t.ink3 });
    const lines = by.map((line) => ({ g: this.text.get(line, body, 420, r), lh: 19 * 1.45, color: t.ink2 }));
    // Bottom-aligned with a 10 px padding, 16 px between blocks; the plane spans y -230..100.
    const total = blocks[0].lh + 16 + blocks[1].lh + (lines.length ? 16 + lines.reduce((s, l) => s + l.lh, 0) : 0);
    let top = -230 + 330 - 10 - total;
    const put = (blk: { g: Glyph | null; lh: number; color: RGBA }) => {
      if (blk.g) this.wallGlyph(-1, at, 540, blk.g, 30 - blk.g.pad, this.baselineIn(blk.g, top, blk.lh) - blk.g.baseline, OFF.label, blk.color);
      top += blk.lh;
    };
    put(blocks[0]);
    top += 16;
    put(blocks[1]);
    if (lines.length) {
      top += 16;
      for (const l of lines) put(l);
    }
  }

  /** The room's name on the doorway's lintel, centred, with its room number under it. */
  private lintelText(sl: RoomSlot): void {
    const t = this.theme;
    const dp = sl.d0 + PORTAL_AT;
    const scale = this.P / Math.max(this.dist(0, CEIL + LINTEL / 2, -dp), NEAR);
    const { r } = this.lodFor(`n${sl.d0}`, scale);
    const size = Math.min(64, Math.round(1300 / Math.max(6, sl.label.length)));
    const name = this.text.get(sl.label, { family: t.body, size, weight: 300, spacing: -0.02 }, 2000, r);
    const sub = this.text.get(sl.sub, { family: t.mono, size: 13, weight: 400, spacing: 0.12, upper: true }, 2000, r);
    const lh1 = size * 0.95;
    const lh2 = 13 * 1.2;
    let top = CEIL + (LINTEL - (lh1 + 12 + lh2)) / 2;
    if (name) this.faceGlyph(name, -(name.w - 2 * name.pad) / 2 - name.pad, this.baselineIn(name, top, lh1) - name.baseline, dp, OFF.face, t.ink);
    top += lh1 + 12;
    if (sub) this.faceGlyph(sub, -(sub.w - 2 * sub.pad) / 2 - sub.pad, this.baselineIn(sub, top, lh2) - sub.baseline, dp, OFF.face, t.ink3);
  }

  /** The end of the show: a line about it, and a door to daylight with its glow. */
  private endText(sl: EndSlot): void {
    const b = this.batch;
    const t = this.theme;
    const d = sl.d1;
    const doorTop = FLOOR - 19 - 450;
    // The glow (CSS box-shadow 0 0 90px 30px), then the door.
    let o = b.add(0, doorTop + 225, -d + OFF.face, 250 + 60 + 180, 450 + 60 + 180, KINDR.shadow, 0);
    b.color(o, t.doorGlow);
    b.extra(o, 90);
    o = b.add(0, doorTop + 225, -d + OFF.door, 250, 450, KINDR.linear, 0);
    b.color(o, WHITE);
    b.uv(o, 180 * DEG, 0, 0, 0);
    b.texture(o, this.grad.door, 0, -1);
    const line = sl.pending > 0 ? 'More works are being hung' : sl.count > 0 ? 'End of the exhibition' : '';
    if (!line) return;
    const scale = this.P / Math.max(this.dist(0, doorTop, -d), NEAR);
    const { r } = this.lodFor(`e${sl.d0}`, scale);
    const sub = sl.pending > 0 ? `${sl.count.toLocaleString()} of ${(sl.count + sl.pending).toLocaleString()} so far` : plural(sl.count, 'work', 'works');
    const g1 = this.text.get(line, { family: t.body, size: 22, weight: 300 }, W, r);
    const g2 = this.text.get(sub, { family: t.mono, size: 12, weight: 400, spacing: 0.12, upper: true }, W, r);
    const lh1 = 22 * 1.2;
    const lh2 = 12 * 1.2;
    let top = doorTop - 34 - (lh1 + 8 + lh2);
    if (g1) this.faceGlyph(g1, -(g1.w - 2 * g1.pad) / 2 - g1.pad, this.baselineIn(g1, top, lh1) - g1.baseline, d, OFF.face, t.ink2);
    top += lh1 + 8;
    if (g2) this.faceGlyph(g2, -(g2.w - 2 * g2.pad) / 2 - g2.pad, this.baselineIn(g2, top, lh2) - g2.baseline, d, OFF.face, t.ink3);
  }

  // ── finding works on screen ──

  private cam() {
    const p = this.pose;
    return { persp: this.P, yaw: p.yaw * DEG, eyeX: -p.x, eyeY: -p.y, eyeZ: p.d, centerX: this.vw / 2, centerY: this.vh / 2, near: NEAR };
  }

  /** A rect of work w's piece on screen: its corners, or null when part of it is behind the eye. */
  private corners(w: WorkSlot, lx: number, ly: number, ww: number, hh: number, off: number): ({ x: number; y: number } | null)[] {
    const q = wallQuad(w.side, w.d - PW / 2, PW, lx, PY + ly, ww, hh, off);
    return roomCorners({ x: q.x, y: q.y, z: q.z, w: q.w, h: q.h, ry: q.ry }, this.cam());
  }

  private boxOf(c: ({ x: number; y: number } | null)[]): DOMRect | null {
    if (c.some((p) => !p)) return null;
    const pts = c as { x: number; y: number }[];
    const xs = pts.map((p) => p.x);
    const ys = pts.map((p) => p.y);
    const l = Math.min(...xs);
    const t = Math.min(...ys);
    return new DOMRect(l, t, Math.max(...xs) - l, Math.max(...ys) - t);
  }

  /** The nearest work whose frame is under a point of the canvas (CSS px), or -1. */
  private workAtPoint(px: number, py: number): number {
    const [a, z] = slotRange(this.slots, this.pose.d - BEHIND, this.pose.d + AHEAD);
    let best = -1;
    let bestDist = Infinity;
    for (let s = a; s < z; s++) {
      const sl = this.slots[s];
      if (sl.type !== 'work') continue;
      const L = this.layoutOf(sl);
      const c = this.corners(sl, L.fl, L.ft, L.fw, L.fh, OFF.frame);
      if (!inside(px, py, c)) continue;
      const dist = this.pieceDist(sl);
      if (dist < bestDist) {
        bestDist = dist;
        best = sl.i;
      }
    }
    return best;
  }

  protected workAt(e: MouseEvent): number {
    const rr = this.root.getBoundingClientRect();
    return this.workAtPoint(e.clientX - rr.left, e.clientY - rr.top);
  }

  private pointer(e: PointerEvent): void {
    if (e.pointerType !== 'mouse') return;
    const i = this.dragging ? -1 : this.workAt(e);
    if (i === this.hover) return;
    this.hover = i;
    this.root.classList.toggle(styles.overFrame, i >= 0);
    this.dirty();
  }

  /** The art of a work on screen (viewport px), or null. */
  private artRect(id: Id): DOMRect | null {
    const w = this.works.find((x) => x.item.id === id);
    if (!w) return null;
    const L = this.layoutOf(w);
    const box = this.boxOf(this.corners(w, L.fl + FRAME + MAT, L.ft + FRAME + MAT, L.pw, L.ph, OFF.art));
    if (!box) return null;
    const rr = this.root.getBoundingClientRect();
    return new DOMRect(rr.left + box.x, rr.top + box.y, box.width, box.height);
  }

  /** The works in view as invisible buttons over their art: screen readers, keyboard focus, the flight. */
  private syncOptions(): void {
    if (this.mini || this.gone || this.dom) return;
    const keep = new Set<Id>();
    const n = this.works.length;
    const lo = clamp(this.focusIdx - 4, 0, n);
    const hi = clamp(this.focusIdx + 5, 0, n);
    for (let i = lo; i < hi; i++) {
      const w = this.works[i];
      if (this.place(w.item.id)) keep.add(w.item.id);
    }
    for (const [id, o] of this.opts) {
      if (keep.has(id) || o === document.activeElement) continue;
      o.remove();
      this.opts.delete(id);
    }
  }

  /** Put the button for work `id` over its art; false when it isn't on screen. */
  private place(id: Id): boolean {
    const w = this.works.find((x) => x.item.id === id);
    if (!w) return false;
    const L = this.layoutOf(w);
    const box = this.boxOf(this.corners(w, L.fl + FRAME + MAT, L.ft + FRAME + MAT, L.pw, L.ph, OFF.art));
    if (!box || box.right < 0 || box.left > this.vw || box.bottom < 0 || box.top > this.vh) return false;
    let o = this.opts.get(id);
    if (!o) {
      o = el('button', styles.option, { type: 'button', tabindex: '-1', 'data-work': id });
      o.addEventListener('click', () => this.openAt(this.works.findIndex((x) => x.item.id === id)));
      this.opts.set(id, o);
      this.root.append(o);
    }
    const label = artLabel(w.item);
    if (o.getAttribute('aria-label') !== label) o.setAttribute('aria-label', label);
    o.style.transform = `translate(${box.x.toFixed(1)}px, ${box.y.toFixed(1)}px)`;
    o.style.width = `${box.width.toFixed(1)}px`;
    o.style.height = `${box.height.toFixed(1)}px`;
    return true;
  }

  // ── contract (forwarded to the DOM Gallery once the GPU is gone) ──

  rectOf(id: Id): DOMRect | null {
    if (this.dom) return this.dom.rectOf(id);
    return this.artRect(id);
  }

  /** A work's art on its wall as you see it (in perspective), for flights. */
  spotOf(id: Id): Spot | null {
    if (this.dom) return this.dom.spotOf?.(id) ?? null;
    const w = this.works.find((x) => x.item.id === id);
    if (!w) return null;
    const L = this.layoutOf(w);
    const rr = this.root.getBoundingClientRect();
    const quad = projectedQuad(this.corners(w, L.fl + FRAME + MAT, L.ft + FRAME + MAT, L.pw, L.ph, OFF.art), rr.left, rr.top);
    if (!quad) return null;
    return { quad, radius: 0, src: w.item.poster ? this.posters.readyUrl(w.item.poster) : null, lift: 0.15, aspect: L.pw / L.ph };
  }

  artOf(id: Id): HTMLElement | null {
    if (this.dom) return this.dom.artOf?.(id) ?? null;
    if (this.mini) return null;
    return this.place(id) ? (this.opts.get(id) ?? null) : null;
  }

  hide(id: Id | null): void {
    if (this.dom) return this.dom.hide?.(id);
    this.hidden = id;
    this.dirty();
  }

  update(items: readonly ItemSummary[], total: number): void {
    if (this.dom) return this.dom.update(items, total);
    super.update(items, total);
  }

  move(dir: Parameters<LensView['move']>[0]): boolean {
    if (this.dom) return this.dom.move(dir);
    return super.move(dir);
  }

  key(e: KeyboardEvent): boolean {
    if (this.dom) return this.dom.key?.(e) ?? false;
    return super.key(e);
  }

  reveal(id: Id): void {
    if (this.dom) return this.dom.reveal?.(id);
    super.reveal(id);
  }

  pause(on: boolean): void {
    if (this.dom) return this.dom.pause?.(on);
    super.pause(on);
    if (on && this.paintRaf) {
      cancelAnimationFrame(this.paintRaf);
      this.paintRaf = 0;
    } else if (!on) this.dirty();
  }

  private teardown(): void {
    this.stop();
    if (this.paintRaf) cancelAnimationFrame(this.paintRaf);
    this.paintRaf = 0;
    this.renderer.destroy();
    this.canvas.remove();
    for (const o of this.opts.values()) o.remove();
    this.opts.clear();
    this.root.classList.remove(styles.overFrame);
  }

  /** The GPU went away (driver reset, GPU process crash): carry on as the DOM Gallery. */
  private fallBack(): void {
    if (this.dom || this.gone) return;
    this.teardown();
    this.dom = { update: () => undefined, move: () => false, rectOf: () => null, destroy: () => undefined };
    void import('./gallery').then(({ gallery }) => {
      if (this.gone) return;
      this.dom = gallery.mount(this.root, this.ctx);
    });
  }

  destroy(): void {
    if (this.gone) return;
    this.gone = true;
    if (this.dom) {
      this.dom.destroy();
      return;
    }
    this.teardown();
  }
}

/** The Gallery on the GPU, once its pipelines compile here (null: use the DOM Gallery). */
export async function gpuGallery(gpu: Gpu): Promise<LensModule | null> {
  const [room, mips] = await Promise.all([prepareRoom3D(gpu), posterCache(gpu).prepare()]);
  if (!room || !mips) return null;
  return {
    mount(root, ctx) {
      if (gpu.lost) throw new Error('The GPU device is gone.');
      return new GpuGalleryView(root, ctx, gpu);
    },
  };
}
