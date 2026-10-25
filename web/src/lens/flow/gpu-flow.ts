/**
 * Flow on the GPU: the same Cover Flow as flow.ts (the same spring, input, title
 * and scroll bar, from core.ts), drawn with WebGPU perspective quads (scene3d.ts)
 * placed with CSS's own 3D maths. Covers, their floor reflections, marks and the
 * room's haze are one draw call a frame, so a flick through 13,000 titles costs a
 * fraction of a millisecond of main thread instead of restyling and compositing
 * two dozen 3D layers with reflections.
 *
 * The DOM keeps what doesn't move with the covers: the floor's pool of light, the
 * mist over the reflections, the title, the scroll bar, and an invisible list of
 * the covers in view (role=option) with a focusable stand-in for the one in front.
 */
import type { Id, ItemSummary } from '../../api/types';
import { sourceLabel } from '../../components/art-label';
import { artLabel } from '../art';
import { clamp, el } from '../dom';
import type { Gpu } from '../gpu/device';
import { bezier, soft, Tween } from '../gpu/motion';
import { classFor, posterCache, type PosterCache, type PosterHit } from '../gpu/posters';
import { clock, type RGBA } from '../gpu/quads';
import { corners, FLAG3D, inside, KIND3D, prepareScene3D, Quad3DBatch, Scene3DRenderer, type Camera } from '../gpu/scene3d';
import { SpriteSheet, type Sprite } from '../gpu/sprites';
import { textAtlas, wrap, type Glyph, type TextAtlas, type TextStyle } from '../gpu/text';
import { isNew, isWatchedItem, progressFraction } from '../sections';
import type { Dir, LensContext, LensModule, LensView } from '../types';
import { coverPose, edgeOpacity, FlowCore, shadeOf, type CoverState, type Metrics } from './core';
import styles from './flow.module.css';
import { cssColor, dcColor, paperOf } from '../gpu/colors';

const DEG = Math.PI / 180;
/** Corner radius of a cover (`--flow-r`). */
const RADIUS = 7;
/** `-webkit-box-reflect: below 2px`. */
const REFL_GAP = 2;
/** New results fade in (CSS: 360 ms ease-out); ones that left fade out (240 ms ease-in). */
const APPEAR_S = 0.36;
const VANISH_S = 0.24;
const easeOut = bezier(0, 0, 0.58, 1);
const easeIn = bezier(0.42, 0, 1, 1);
/**
 * The sheen on the poster in front while it leans toward the pointer: a soft light
 * that follows the pointer (CSS: `radial-gradient(70% 55% at x y, white .26,
 * transparent 70%)`, soft-light) and a faint diagonal band. Soft light can't be
 * blended here; these strengths brighten a poster's mid-tones about as much.
 */
const GLOW = { rx: 0.7, ry: 0.55, alpha: 0.12 };
const BAND = 0.015;
/** The glow sprite: a radial gradient `GLOW_PX` across, white to clear at 70%. */
const GLOW_PX = 128;
/** The contact shadow's blur (CSS `filter: blur(5px)`), and the margin the blur needs. */
const CONTACT_BLUR = 5;
const CONTACT_PAD = 15;

const WHITE: RGBA = [1, 1, 1, 1];
const INK: RGBA = [0.914, 0.902, 0.875, 1];
const PILL: RGBA = [8 / 255, 8 / 255, 10 / 255, 1];
const BLACK: RGBA = [0, 0, 0, 1];

/** A cover as drawn: offsets from the stage's centre and rotations (degrees), and its distance from the position. */
export interface Placed {
  x: number;
  z: number;
  rx: number;
  ry: number;
  ad: number;
}

/** A cover's perspective quad (scene3d): centred on the stage, its bottom edge on the floor, radians. */
export interface CoverQuad {
  x: number;
  y: number;
  z: number;
  w: number;
  h: number;
  rx: number;
  ry: number;
}

export function coverQuad(p: Placed, m: Metrics): CoverQuad {
  return { x: m.W / 2 + p.x, y: m.floor - m.ch / 2, z: p.z, w: m.cw, h: m.ch, rx: p.rx * DEG, ry: p.ry * DEG };
}

/** The stage's camera: CSS `perspective` with `perspective-origin: 50% (floor − ch/2)`. */
export function flowCamera(m: Metrics): Pick<Camera, 'persp' | 'originX' | 'originY'> {
  return { persp: m.persp, originX: m.W / 2, originY: m.floor - m.ch / 2 };
}

/** A cover's box on the stage (the bounds of its projected corners); null when a corner is behind the camera. */
export function coverBox(p: Placed, m: Metrics): { x: number; y: number; w: number; h: number } | null {
  let l = Infinity;
  let t = Infinity;
  let r = -Infinity;
  let b = -Infinity;
  for (const v of corners(coverQuad(p, m), flowCamera(m))) {
    if (!v) return null;
    l = Math.min(l, v.x);
    t = Math.min(t, v.y);
    r = Math.max(r, v.x);
    b = Math.max(b, v.y);
  }
  return { x: l, y: t, w: r - l, h: b - t };
}

/** The front-most cover under a point on the stage: the one nearest the position wins, as they are drawn. */
export function coverUnder<T>(covers: Iterable<{ id: T; at: Placed }>, x: number, y: number, m: Metrics): T | null {
  const cam = flowCamera(m);
  let best: T | null = null;
  let bestAd = Infinity;
  for (const c of covers) {
    const p = c.at;
    if (p.ad >= bestAd || edgeOpacity(p.ad, m.R) <= 0.05) continue;
    if (inside(x, y, corners(coverQuad(p, m), cam))) {
      best = c.id;
      bestAd = p.ad;
    }
  }
  return best;
}

/**
 * Where to draw a cover's reflection with the shader's floor mirror so it lands where
 * `-webkit-box-reflect: below <gap>` puts it. CSS reflects a cover in its own plane,
 * about its bottom edge; that is a floor mirror only while the cover stands upright,
 * so the mirror image of a tilted cover is turned back and moved to match. Returns
 * the quad's centre and rotation (radians) and the mirror line.
 */
export function reflectionPose(q: Pick<CoverQuad, 'x' | 'y' | 'z' | 'h' | 'rx' | 'ry'>, gap: number): { x: number; y: number; z: number; rx: number; ry: number; line: number } {
  const a = q.h + gap;
  const s = Math.sin(q.rx);
  return { x: q.x + a * s * Math.sin(q.ry), y: q.y + a * (1 - Math.cos(q.rx)), z: q.z + a * s * Math.cos(q.ry), rx: -q.rx, ry: q.ry, line: q.y + a / 2 };
}

/**
 * Where a mark or a line of text `oy` below a cover's centre (in its plane) shows in the
 * cover's reflection: CSS reflects in the cover's own plane, so it is the same distance
 * below the reflected edge, upside down (draw it there with its texture flipped).
 */
export function reflectedOffset(oy: number, h: number, gap: number): number {
  return h + gap - oy;
}

/** How strongly a reflection shows at height `f` of the cover (0 its top, 1 the edge on the floor), as the shader fades it. */
export function reflectionAt(f: number): number {
  const t = clamp((f - 0.64) / 0.36, 0, 1);
  return t * t * (3 - 2 * t);
}

/**
 * CSS fades a cover as one group (`opacity` over its poster, its marks and the haze on
 * top). Drawn as separate layers over whatever is behind, the ones under the haze need a
 * little more alpha to come out the same: `under` for the poster and its marks, `haze`
 * for the haze.
 */
export function groupAlphas(op: number, haze: number): { under: number; haze: number } {
  const hz = op * haze;
  return { under: hz >= 1 ? 0 : (op * (1 - haze)) / (1 - hz), haze: hz };
}

/**
 * Texture coordinates that centre a radial sprite on (fu, fv) of a quad, its radius
 * `rx`×`ry` of the quad's size; the sprite's centre and radius are (cu, cv) and (ru, rv)
 * in texture coordinates. Beyond its painted part the sprite is clear.
 */
export function glowUV(fu: number, fv: number, rx: number, ry: number, cu: number, cv: number, ru: number, rv: number): [number, number, number, number] {
  const ku = ru / rx;
  const kv = rv / ry;
  return [cu - fu * ku, cv - fv * kv, cu + (1 - fu) * ku, cv + (1 - fv) * kv];
}

interface GCover extends CoverState {
  item: ItemSummary;
  /** When it arrived with new results (it fades in); −1 when it was already there. */
  appear: number;
  /** Where it was drawn last. */
  at: Placed;
}

/** A cover whose title left the results, fading where it stood. */
interface Ghost {
  item: ItemSummary;
  at: Placed;
  start: number;
}

interface Theme {
  bg: RGBA;
  contact: RGBA;
  refl: number;
  shade: number;
  body: string;
  mono: string;
}

/** The pointer's light, white: a radial gradient that is clear from 70% of its radius. */
function paintGlow(ctx: CanvasRenderingContext2D, w: number): void {
  const r = w / 2;
  const g = ctx.createRadialGradient(r, r, 0, r, r, r);
  g.addColorStop(0, 'rgb(255 255 255)');
  g.addColorStop(0.7, 'rgb(255 255 255 / 0)');
  ctx.fillStyle = g;
  ctx.fillRect(0, 0, w, w);
}

/** The contact shadow, white (tinted when drawn): CSS's `radial-gradient(closest-side)` ellipse, blurred. */
function paintContact(ctx: CanvasRenderingContext2D, w: number, h: number): void {
  const shape = document.createElement('canvas');
  shape.width = w;
  shape.height = h;
  const s = shape.getContext('2d');
  if (!s) return;
  s.translate(w / 2, h / 2);
  s.scale(w / 2 - CONTACT_PAD, h / 2 - CONTACT_PAD);
  const g = s.createRadialGradient(0, 0, 0, 0, 0, 1);
  g.addColorStop(0, 'rgb(255 255 255)');
  g.addColorStop(1, 'rgb(255 255 255 / 0)');
  s.fillStyle = g;
  s.beginPath();
  s.arc(0, 0, 1, 0, Math.PI * 2);
  s.fill();
  ctx.filter = `blur(${CONTACT_BLUR}px)`;
  ctx.drawImage(shape, 0, 0);
}

/** Flow's sprites (the pointer's light and the contact shadow), one sheet per device. */
const sheets = new WeakMap<GPUDevice, SpriteSheet>();

function spriteSheet(gpu: Gpu): SpriteSheet {
  let s = sheets.get(gpu.device);
  if (!s) {
    // Wide enough for the contact shadow under the largest cover (cw ≤ 334px).
    s = new SpriteSheet(gpu, 352, GLOW_PX, 4);
    sheets.set(gpu.device, s);
  }
  return s;
}

const byDistance = (a: { at: Placed }, b: { at: Placed }) => b.at.ad - a.at.ad;

export class GpuFlowView extends FlowCore<GCover> {
  private canvas: HTMLCanvasElement;
  private floorEl: HTMLDivElement;
  private mistEl: HTMLDivElement;
  private renderer: Scene3DRenderer;
  private posters: PosterCache;
  private text: TextAtlas;
  private sprites: SpriteSheet;
  private batch = new Quad3DBatch(512);
  private theme: Theme;
  private dpr = 1;
  /** Poster sizes: for the covers by the centre, and for the stacks. */
  private nearClass = 2;
  private farClass = 1;
  private paintRaf = 0;
  private lastTrim = 0;
  private ghosts: Ghost[] = [];
  private list: (GCover | Ghost)[] = [];
  /** The sheen's strength (0–1), and where the pointer is on the poster in front (0–1). */
  private glow = new Tween(soft, 0);
  private glowAt: [number, number] = [0.5, 0.2];
  private contactA = 0;
  private contactWant = 0;
  private contactT = 0;
  private hidden: Id | null = null;
  private gone = false;
  /** After a lost GPU: the DOM Flow, which this view then forwards to. */
  private dom: LensView | null = null;
  /** The covers in view as invisible options (screen readers), synced when things settle. */
  private opts = new Map<Id, HTMLDivElement>();
  private optsT: ReturnType<typeof setTimeout> | null = null;
  /** Stands in for the poster in front: keyboard focus, and what the shell focuses after an overlay. */
  private focusEl: HTMLAnchorElement | null = null;
  private twin: HTMLDivElement | null = null;
  private cursor = '';
  /** Titles set on posters without art, by size and title (measuring is slow). */
  private typeCache = new Map<string, string[]>();
  private typeGen = -1;
  // The cover being drawn (fields, so drawing allocates nothing per quad).
  private fx = 0;
  private fy = 0;
  private fz = 0;
  private frx = 0;
  private fry = 0;
  private fop = 1;
  private fhaze = 0;
  private funder = 1;
  private fmir = false;

  constructor(root: HTMLElement, ctx: LensContext, gpu: Gpu) {
    super(root, ctx);
    root.classList.add(styles.gpu);
    this.floorEl = el('div', styles.floor, { 'aria-hidden': 'true' });
    this.canvas = el('canvas', styles.canvas, { 'aria-hidden': 'true' });
    this.mistEl = el('div', styles.mist, { 'aria-hidden': 'true' });
    root.append(this.floorEl, this.canvas, this.mistEl);
    this.renderer = new Scene3DRenderer(gpu, this.canvas);
    this.posters = posterCache(gpu);
    this.text = textAtlas(gpu);
    this.sprites = spriteSheet(gpu);
    this.theme = this.readTheme();
    if (!this.mini) {
      this.focusEl = el('a', styles.option, { role: 'option', 'aria-selected': 'true', tabindex: '-1', draggable: 'false' });
      this.focusEl.hidden = true;
      root.append(this.focusEl);
      this.on(root, 'pointermove', (e) => this.pointerCursor(e));
    }
    // Holding the poster cache keeps it from being freed while Flow draws from it.
    this.cleanup.push(this.posters.retain(), this.posters.onReady(this.invalidate), this.text.onChange(this.invalidate), gpu.onLost(() => this.fallBack()));
    this.watchTheme();
    this.start();
  }

  // ── setup ──

  private readTheme(): Theme {
    const cs = getComputedStyle(this.root);
    const v = (k: string) => cs.getPropertyValue(k);
    const num = (k: string, d: number) => {
      const n = parseFloat(v(k));
      return Number.isFinite(n) ? n : d;
    };
    return {
      bg: cssColor(v('--bg'), [0.035, 0.035, 0.043, 1]),
      contact: cssColor(v('--flow-contact'), [0, 0, 0, 0.7]),
      refl: num('--flow-refl', 0.34),
      shade: num('--flow-shade', 0.55),
      body: v('--font-body').trim() || 'system-ui, sans-serif',
      mono: v('--font-mono').trim() || 'ui-monospace, monospace',
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

  protected applyMetrics(m: Metrics): void {
    const s = this.root.style;
    s.setProperty('--cw', `${m.cw.toFixed(1)}px`);
    s.setProperty('--ch', `${m.ch.toFixed(1)}px`);
    s.setProperty('--floor', `${m.floor.toFixed(1)}px`);
    s.setProperty('--title-y', `${(m.floor + m.reflGap).toFixed(1)}px`);
    let dpr = Math.min(window.devicePixelRatio || 1, 3);
    // A preview in the lens gallery is shown scaled down: draw only the pixels it shows.
    if (this.mini) {
      const shown = this.root.getBoundingClientRect().width;
      dpr = Math.min(dpr, Math.max(0.5, (dpr * shown) / Math.max(1, m.W)) * 1.25);
    }
    this.dpr = dpr;
    // The poster in front comes forward a little; the stacks are seen edge-on (half the width is plenty).
    this.nearClass = classFor((m.cw * dpr * m.persp) / (m.persp - m.lift));
    this.farClass = classFor(m.cw * dpr * 0.5);
    this.renderer.resize(m.W * dpr, m.H * dpr);
  }

  protected resized(): void {
    super.resized();
    this.optionsSoon();
  }

  private camera(): Camera {
    return { ...flowCamera(this.m), width: this.m.W, height: this.m.H, dpr: this.dpr, time: clock() };
  }

  // ── covers ──

  private placed(c: CoverState): Placed {
    const p = coverPose(c, this.pos, this.m, this.tilt, this.dragging);
    return { x: p.x, z: p.z, rx: p.rx, ry: p.ry, ad: p.ad };
  }

  protected make(it: ItemSummary, i: number, fadeIn: boolean): GCover {
    this.optionsSoon();
    const c: GCover = { id: it.id, item: it, vi: i, vv: 0, appear: fadeIn && !this.reduced ? clock() : -1, at: { x: 0, z: 0, rx: 0, ry: 0, ad: 0 } };
    c.at = this.placed(c);
    return c;
  }

  protected drop(): void {
    this.optionsSoon();
  }

  protected retire(c: GCover): void {
    this.optionsSoon();
    if (!this.reduced) this.ghosts.push({ item: c.item, at: c.at, start: clock() });
  }

  /** The title a cover shows now (results may have changed it since the cover was made). */
  private itemOf(c: GCover): ItemSummary {
    const i = this.index.get(c.id);
    return i !== undefined ? (this.items[i] ?? c.item) : c.item;
  }

  protected selected(id: Id): void {
    // Its name now; where it stands once the flow settles (it is invisible, and the poster is still moving).
    this.placeFocus(id, false);
    if (!this.raf) this.optionsSoon();
    this.invalidate();
  }

  // ── frames ──

  private invalidate = (): void => {
    if (this.paintRaf || this.gone || this.dom) return;
    this.paintRaf = requestAnimationFrame(() => {
      this.paintRaf = 0;
      // The spring paints this frame anyway.
      if (!this.raf) this.paint();
    });
  };

  protected paint(): void {
    if (this.gone || this.dom) return;
    if (!this.mini && Math.min(window.devicePixelRatio || 1, 3) !== this.dpr) this.applyMetrics(this.m);
    const now = clock();
    const posters = this.posters;
    // Upload first: a slot drawn in the last frame is still "in use" and can't be evicted for a new one.
    posters.upload();
    posters.begin();
    this.sprites.begin();
    const gen = this.text.gen;
    this.build(now);
    // The glyph atlas started over while building: build again with fresh glyphs.
    if (this.text.gen !== gen) this.build(now);
    this.text.flush();
    this.renderer.draw(this.batch, this.camera(), { posters: posters.views(), glyphs: this.text.view, sprites: this.sprites.view, key: `${posters.version}|${this.sprites.version}` });
    posters.pump();
    if (performance.now() - this.lastTrim > 5000) {
      this.lastTrim = performance.now();
      posters.trim();
    }
    if (this.ghosts.length) this.ghosts = this.ghosts.filter((g) => now - g.start < VANISH_S);
    let busy = this.ghosts.length > 0 || !this.glow.done(now) || this.contactA !== this.contactWant || posters.pending || now < posters.fadeUntil;
    if (!busy) for (const c of this.covers.values()) if (c.appear >= 0) busy = true;
    if (busy && !this.paused) this.invalidate();
  }

  private build(now: number): void {
    const b = this.batch;
    b.reset();
    this.contactShadow(now);
    const list = this.list;
    list.length = 0;
    for (const c of this.covers.values()) {
      c.at = this.placed(c);
      list.push(c);
    }
    for (const g of this.ghosts) list.push(g);
    // Back to front: the far ends of the stacks first, the poster in front last.
    list.sort(byDistance);
    const centreId = this.ci >= 0 ? this.items[this.ci]?.id : undefined;
    for (const e of list) {
      if ('start' in e) {
        const t = clamp((now - e.start) / VANISH_S, 0, 1);
        this.cover(e.item, e.at, 1 - easeIn(t), now, false);
        continue;
      }
      if (e.id === this.hidden) continue;
      let a = 1;
      if (e.appear >= 0) {
        const t = clamp((now - e.appear) / APPEAR_S, 0, 1);
        a = easeOut(t);
        if (t >= 1) e.appear = -1;
      }
      this.cover(this.itemOf(e), e.at, a, now, e.id === centreId);
    }
  }

  /** A soft contact shadow under the poster in front, fading while it is between two. */
  private contactShadow(now: number): void {
    const m = this.m;
    const off = Math.abs(this.pos - Math.round(this.pos));
    const want = this.items.length ? clamp(1 - off * 2.4, 0, 1) : 0;
    // CSS eases it with a 0.2 s transition restarted every frame: a lag that keeps flicks from flickering it.
    const dt = this.contactT ? Math.min(0.05, now - this.contactT) : 1;
    this.contactT = now;
    this.contactWant = want;
    this.contactA = this.reduced ? want : this.contactA + (want - this.contactA) * Math.min(1, dt / 0.2);
    if (Math.abs(want - this.contactA) < 0.004) this.contactA = want;
    if (this.contactA <= 0.004) return;
    const w = Math.round(m.cw * 0.96) + CONTACT_PAD * 2;
    const h = 18 + CONTACT_PAD * 2;
    const s = this.sprites.get(`flow-contact|${w}`, (ctx, sw, sh) => paintContact(ctx, sw, sh), w, h);
    if (!s) return;
    const b = this.batch;
    const o = b.add(m.W / 2, m.floor, 0, w, h, KIND3D.sprite);
    b.color(o, this.theme.contact);
    b.opacity(o, this.contactA);
    b.uv(o, 0, 0, s.u1, s.v1);
    b.texture(o, s.layer, 0, 0);
  }

  /** One cover: its reflection on the floor (poster, marks, haze, sheen), then its face. */
  private cover(item: ItemSummary, p: Placed, alpha: number, now: number, centre: boolean): void {
    const m = this.m;
    const th = this.theme;
    const op = edgeOpacity(p.ad, m.R) * alpha;
    if (op <= 0.003) return;
    const haze = shadeOf(p.ad) * th.shade * 1.6;
    let hit: PosterHit | null = null;
    if (item.poster) {
      // Mid-flick only the covers about to pass the centre load, and at the stacks' size (it shows for a
      // moment, and a poster flicked past costs one decode, not two); the sharp one once it slows.
      const fast = Math.abs(this.vel) >= 8;
      const cls = p.ad < 1.5 && !fast ? this.nearClass : this.farClass;
      hit = this.posters.get(item.poster, cls, p.ad * 1000, !this.paused && (p.ad < 3 || !fast));
    }
    const glow = centre ? this.glow.value(now) : 0;
    const light = glow > 0.003 ? this.sprites.get('flow-glow', (ctx, w) => paintGlow(ctx, w), GLOW_PX, GLOW_PX) : null;
    const q = coverQuad(p, m);
    if (th.refl > 0.003) {
      const r = reflectionPose(q, REFL_GAP);
      // Exact where the reflection meets the floor (where it shows most).
      this.poster(item, hit, r.x, r.y, r.z, r.rx, r.ry, groupAlphas(op * th.refl, haze).under / th.refl, glow * BAND, r.line);
      this.frame(q, op, haze, true);
      this.overlays(item);
      if (haze > 0.004) this.fill(r.x, r.y, r.z, r.rx, r.ry, th.bg, haze, op, r.line);
      if (light) this.light(light, r.x, r.y, r.z, r.rx, r.ry, glow, op, r.line);
    }
    this.poster(item, hit, q.x, q.y, q.z, q.rx, q.ry, groupAlphas(op, haze).under, glow * BAND, null);
    this.frame(q, op, haze, false);
    this.overlays(item);
    // Distance haze: covers in the stacks sink into the room's own colour.
    if (haze > 0.004) this.fill(q.x, q.y, q.z, q.rx, q.ry, th.bg, haze, op, null);
    if (light) this.light(light, q.x, q.y, q.z, q.rx, q.ry, glow, op, null);
  }

  /** The poster: its texture once loaded, its average colour (or the title's gradient) until then. */
  private poster(item: ItemSummary, hit: PosterHit | null, x: number, y: number, z: number, rx: number, ry: number, opacity: number, sheen: number, mirror: number | null): void {
    const b = this.batch;
    const o = b.add(x, y, z, this.m.cw, this.m.ch, KIND3D.poster, RADIUS);
    b.rotate(o, rx, ry);
    b.opacity(o, opacity);
    let flags = FLAG3D.hairline;
    if (!item.available) flags |= FLAG3D.gray;
    const dc = item.poster?.blurhash ? dcColor(item.poster.blurhash) : null;
    if (dc) b.color(o, dc);
    else {
      const [c1, c2] = paperOf(item);
      b.color(o, c1);
      b.uv(o, c2[0], c2[1], c2[2], c2[3]);
      flags |= FLAG3D.gradient;
    }
    if (hit) {
      b.uv(o, 0, 0, 1, 1);
      b.texture(o, hit.code, hit.texWidth, hit.readyAt);
      flags &= ~FLAG3D.gradient;
    }
    b.flags(o, flags);
    if (mirror !== null) b.mirror(o, mirror, this.theme.refl);
    if (sheen > 0) b.sheen(o, sheen);
  }

  /** A flat colour over the whole cover (the haze), `alpha` of it, the cover at `op`. */
  private fill(x: number, y: number, z: number, rx: number, ry: number, color: RGBA, alpha: number, op: number, mirror: number | null): void {
    const b = this.batch;
    const o = b.add(x, y, z, this.m.cw, this.m.ch, KIND3D.solid, RADIUS);
    b.rotate(o, rx, ry);
    b.color(o, color, alpha);
    b.opacity(o, op);
    if (mirror !== null) b.mirror(o, mirror, this.theme.refl);
  }

  /** The pointer's light on the poster in front. */
  private light(s: Sprite, x: number, y: number, z: number, rx: number, ry: number, glow: number, op: number, mirror: number | null): void {
    const b = this.batch;
    const o = b.add(x, y, z, this.m.cw, this.m.ch, KIND3D.sprite, RADIUS);
    b.rotate(o, rx, ry);
    b.color(o, WHITE, GLOW.alpha * glow);
    b.opacity(o, op);
    const cu = s.u1 / 2;
    const cv = s.v1 / 2;
    const [u0, v0, u1, v1] = glowUV(this.glowAt[0], this.glowAt[1], GLOW.rx, GLOW.ry, cu, cv, cu, cv);
    b.uv(o, u0, v0, u1, v1);
    b.texture(o, s.layer, 0, 0);
    if (mirror !== null) b.mirror(o, mirror, this.theme.refl);
  }

  /** Marks and typeset titles are drawn in the plane of this cover (its reflection: mirrored in that plane, as CSS does). */
  private frame(q: CoverQuad, op: number, haze: number, mirrored: boolean): void {
    this.fx = q.x;
    this.fy = q.y;
    this.fz = q.z;
    this.frx = q.rx;
    this.fry = q.ry;
    this.fop = op;
    this.fhaze = haze;
    this.funder = groupAlphas(op, haze).under;
    this.fmir = mirrored;
  }

  /** A quad in the current cover's plane, `(ox, oy)` from its centre; −1 when it doesn't show. */
  private inPlane(ox: number, oy: number, w: number, h: number, kind: number, radius: number, alpha: number): number {
    const m = this.m;
    let a = alpha;
    let y = oy;
    if (this.fmir) {
      const M = this.fop * this.theme.refl * reflectionAt((oy + m.ch / 2) / m.ch);
      if (M <= 0.003) return -1;
      a *= groupAlphas(M, this.fhaze).under;
      y = reflectedOffset(oy, m.ch, REFL_GAP);
    } else a *= this.funder;
    if (a <= 0.003) return -1;
    const b = this.batch;
    const o = b.add(this.fx + ox, this.fy + y, this.fz, w, h, kind, radius);
    b.origin(o, -ox, -y, 0);
    b.rotate(o, this.frx, this.fry);
    b.opacity(o, a);
    return o;
  }

  /** A glyph in the cover's plane, its top-left at (left, top) from the cover's top-left. */
  private glyphAt(g: Glyph, left: number, top: number, color: RGBA, alpha: number): void {
    const m = this.m;
    const o = this.inPlane(left + g.w / 2 - m.cw / 2, top + g.h / 2 - m.ch / 2, g.w, g.h, KIND3D.text, 0, alpha);
    if (o < 0) return;
    this.batch.color(o, color);
    // A reflection shows it upside down.
    if (this.fmir) this.batch.uv(o, g.u0, g.v1, g.u1, g.v0);
    else this.batch.uv(o, g.u0, g.v0, g.u1, g.v1);
  }

  /** A rectangle in the cover's plane, its top-left at (left, top) from the cover's top-left. */
  private rectAt(left: number, top: number, w: number, h: number, kind: number, radius: number, color: RGBA, alpha: number): number {
    const m = this.m;
    const o = this.inPlane(left + w / 2 - m.cw / 2, top + h / 2 - m.ch / 2, w, h, kind, radius, alpha);
    if (o >= 0) this.batch.color(o, color);
    return o;
  }

  private overlays(item: ItemSummary): void {
    this.marks(item);
    if (!item.poster && this.m.cw >= 72) this.typeset(item);
  }

  /** Progress, watched, new and offline marks, and the frame/AI label: as art.ts draws them. */
  private marks(item: ItemSummary): void {
    const m = this.m;
    const W = m.cw;
    const H = m.ch;
    const dpr = this.dpr;
    const mono: TextStyle = { family: this.theme.mono, size: 9.28, weight: 500, spacing: 0.08, upper: true };
    const pillH = 15.6;
    // The top of a cover never shows in its reflection.
    const top = !this.fmir;
    const label = top && item.poster ? sourceLabel(item.poster) : null;
    if (label) {
      const ai = label.tone === 'ai';
      const g = this.text.get(label.short, mono, 200, dpr);
      const icon = ai ? this.text.icon('spark', 9, dpr) : null;
      if (g) {
        const w = 12 + (g.w - g.pad * 2) + (icon ? 13 : 0);
        this.rectAt(6, 6, w, pillH, KIND3D.solid, 4, PILL, ai ? 0.86 : 0.72);
        if (ai) {
          const ring = this.rectAt(6, 6, w, pillH, KIND3D.ring, 4, WHITE, 0.55);
          if (ring >= 0) this.batch.extra(ring, 1);
        }
        let tx = 12;
        if (icon) {
          this.glyphAt(icon, tx - icon.pad, 6 + (pillH - icon.h) / 2, WHITE, 1);
          tx += 13;
        }
        this.glyphAt(g, tx - g.pad, 6 + pillH / 2 + 3.2 - g.baseline, ai ? WHITE : INK, 1);
      }
    }
    const prog = progressFraction(item);
    if (prog !== null) {
      this.rectAt(W * 0.08, H - 10, W * 0.84, 3, KIND3D.solid, 1.5, WHITE, 0.3);
      this.rectAt(W * 0.08, H - 10, W * 0.84 * Math.max(0.03, prog), 3, KIND3D.solid, 1.5, WHITE, 1);
    } else if (top && isWatchedItem(item)) {
      this.rectAt(W - 26, 6, 20, 20, KIND3D.solid, 10, PILL, 0.6);
      const icon = this.text.icon('check', 11, dpr);
      if (icon) this.glyphAt(icon, W - 16 - icon.w / 2, 16 - icon.h / 2, WHITE, 1);
    } else if (top && isNew(item)) {
      const glow = this.rectAt(W - 28, -4, 32, 32, KIND3D.shadow, 4, WHITE, 0.8);
      if (glow >= 0) this.batch.extra(glow, 12);
      this.rectAt(W - 19, 5, 14, 14, KIND3D.solid, 7, BLACK, 0.25);
      this.rectAt(W - 16, 8, 8, 8, KIND3D.solid, 4, WHITE, 1);
    }
    if (!item.available) {
      const g = this.text.get('Offline', { ...mono, weight: 400 }, 200, dpr);
      if (g) {
        const w = 12 + g.w - g.pad * 2;
        this.rectAt(6, H - 6 - pillH, w, pillH, KIND3D.solid, 4, PILL, 0.75);
        this.glyphAt(g, 12 - g.pad, H - 6 - pillH + pillH / 2 + 3.2 - g.baseline, INK, 1);
      }
    }
  }

  /** A title without art: its name set on the poster, bottom-aligned (art.ts `typeset`). */
  private typeset(item: ItemSummary): void {
    const w = this.m.cw;
    const size = Math.round(clamp(w * 0.11, 12.8, 25.6) * 4) / 4;
    const margin = w * 0.11;
    const max = Math.round(w - margin * 2);
    const style: TextStyle = { family: this.theme.body, size, weight: 500 };
    if (this.typeGen !== this.text.gen || this.typeCache.size > 500) {
      this.typeCache.clear();
      this.typeGen = this.text.gen;
    }
    const key = `${max}|${size}|${item.title}`;
    let lines = this.typeCache.get(key);
    if (!lines) {
      lines = wrap(item.title, max, 4, (s) => this.text.measure(s, style));
      this.typeCache.set(key, lines);
    }
    const lh = size * 1.05;
    let top = this.m.ch - margin - lines.length * lh;
    for (const line of lines) {
      const g = this.text.get(line, style, max, this.dpr);
      if (g) this.glyphAt(g, margin - g.pad, top + lh * 0.82 - g.baseline, WHITE, 1);
      top += lh;
    }
  }

  // ── pointer ──

  /** The stage point under a viewport point. */
  private local(clientX: number, clientY: number): { x: number; y: number } {
    const rr = this.root.getBoundingClientRect();
    return { x: clientX - rr.left, y: clientY - rr.top };
  }

  protected coverAt(e: PointerEvent): Id | null {
    const p = this.local(e.clientX, e.clientY);
    return coverUnder(this.covers.values(), p.x, p.y, this.m);
  }

  /** A cover's box on screen (viewport coordinates), as the shell's flights expect. */
  private screenBox(id: Id): DOMRect | null {
    const c = this.covers.get(id);
    const box = c ? coverBox(c.at, this.m) : null;
    if (!box) return null;
    const rr = this.root.getBoundingClientRect();
    return new DOMRect(rr.left + box.x, rr.top + box.y, box.w, box.h);
  }

  protected centreRect(): DOMRect | null {
    const it = this.items[this.ci];
    return it ? this.screenBox(it.id) : null;
  }

  protected artFor(): Element | null {
    return null;
  }

  protected sheenAt(u: number, v: number): void {
    this.glowAt[0] = u + 0.5;
    this.glowAt[1] = v + 0.5;
  }

  protected tiltChanged(on: boolean): void {
    this.glow.set(on ? 1 : 0, clock(), 0.35);
    this.invalidate();
  }

  /** The pointer's shape: zoom in over the poster in front, a hand over the others. */
  private pointerCursor(e: PointerEvent): void {
    if (e.pointerType !== 'mouse' || this.dragging) return;
    const id = this.coverAt(e);
    const centreId = this.ci >= 0 ? this.items[this.ci]?.id : undefined;
    const cursor = id === null ? '' : id === centreId ? styles.overCentre : styles.over;
    if (cursor === this.cursor) return;
    if (this.cursor) this.root.classList.remove(this.cursor);
    if (cursor) this.root.classList.add(cursor);
    this.cursor = cursor;
  }

  // ── the covers in view, for screen readers and keyboard focus ──

  /** Place an invisible element over a cover. */
  private placeOver(node: HTMLElement, c: GCover): void {
    const box = coverBox(c.at, this.m);
    if (!box) return;
    node.style.transform = `translate(${box.x.toFixed(1)}px, ${box.y.toFixed(1)}px)`;
    node.style.width = `${box.w.toFixed(1)}px`;
    node.style.height = `${box.h.toFixed(1)}px`;
  }

  /** The focus stand-in moves onto the poster in front (and that poster's option steps aside: one title, one option). */
  private placeFocus(id: Id | null, place = true): void {
    const f = this.focusEl;
    if (!f) return;
    const c = id !== null ? this.covers.get(id) : undefined;
    if (!c) {
      f.hidden = true;
      this.setTwin(null);
      return;
    }
    const item = this.itemOf(c);
    f.hidden = false;
    if (f.dataset.id !== String(c.id)) {
      f.dataset.id = String(c.id);
      f.href = `/item/${c.id}`;
    }
    const label = artLabel(item);
    if (f.getAttribute('aria-label') !== label) f.setAttribute('aria-label', label);
    if (place) this.placeOver(f, c);
    this.setTwin(this.opts.get(c.id) ?? null);
  }

  private setTwin(twin: HTMLDivElement | null): void {
    if (twin === this.twin) return;
    this.twin?.removeAttribute('aria-hidden');
    twin?.setAttribute('aria-hidden', 'true');
    this.twin = twin;
  }

  /** Sync the invisible options once the covers stop changing (not during a flick: `settled` does it when it ends). */
  private optionsSoon(): void {
    if (this.mini) return;
    if (this.optsT) clearTimeout(this.optsT);
    this.optsT = setTimeout(() => {
      this.optsT = null;
      if (!this.raf) this.syncOptions();
    }, 120);
  }

  protected settled(): void {
    this.syncOptions();
  }

  private syncOptions(): void {
    if (this.mini || this.gone || this.dom) return;
    if (this.optsT) {
      clearTimeout(this.optsT);
      this.optsT = null;
    }
    for (const [id, o] of this.opts) {
      if (!this.covers.has(id)) {
        if (o === this.twin) this.twin = null;
        o.remove();
        this.opts.delete(id);
      }
    }
    for (const c of this.covers.values()) {
      let o = this.opts.get(c.id);
      if (!o) {
        o = el('div', styles.option, { role: 'option', 'aria-selected': 'false', 'data-id': c.id });
        this.opts.set(c.id, o);
        this.root.append(o);
      }
      const label = artLabel(this.itemOf(c));
      if (o.getAttribute('aria-label') !== label) o.setAttribute('aria-label', label);
      this.placeOver(o, c);
    }
    this.placeFocus(this.ci >= 0 ? (this.items[this.ci]?.id ?? null) : null);
  }

  /** After a key moved the flow: keyboard focus (and the screen reader) rides the poster in front. */
  private focusFront(): void {
    const f = this.focusEl;
    if (!f || f.hidden) return;
    const a = document.activeElement;
    if (!a || a === document.body || this.root.contains(a)) f.focus({ preventScroll: true });
  }

  // ── contract (forwarded to the DOM Flow once the GPU is gone) ──

  update(items: readonly ItemSummary[]): void {
    if (this.dom) return this.dom.update(items, items.length);
    super.update(items);
    if (!items.length) this.placeFocus(null);
    this.optionsSoon();
    this.invalidate();
  }

  move(dir: Dir): boolean {
    if (this.dom) return this.dom.move(dir);
    const done = super.move(dir);
    if (done) this.focusFront();
    return done;
  }

  key(e: KeyboardEvent): boolean {
    if (this.dom) return this.dom.key?.(e) ?? false;
    const done = super.key(e);
    if (done && e.key !== 'Enter') this.focusFront();
    return done;
  }

  rectOf(id: Id): DOMRect | null {
    if (this.dom) return this.dom.rectOf(id);
    return this.screenBox(id);
  }

  /** The focusable stand-in for the poster in front (where the shell puts keyboard focus back), or a cover's option. */
  artOf(id: Id): HTMLElement | null {
    if (this.dom) return this.dom.artOf?.(id) ?? null;
    const c = this.covers.get(id);
    if (this.mini || !c) return null;
    const centreId = this.ci >= 0 ? this.items[this.ci]?.id : undefined;
    if (id === centreId && this.focusEl) {
      this.placeFocus(id);
      return this.focusEl;
    }
    if (!this.opts.has(id)) this.syncOptions();
    const o = this.opts.get(id) ?? null;
    if (o) this.placeOver(o, c);
    return o;
  }

  hide(id: Id | null): void {
    if (this.dom) return this.dom.hide?.(id);
    this.hidden = id;
    this.invalidate();
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
    } else if (!on) this.invalidate();
  }

  private teardown(): void {
    this.stop();
    if (this.paintRaf) cancelAnimationFrame(this.paintRaf);
    this.paintRaf = 0;
    if (this.optsT) clearTimeout(this.optsT);
    this.optsT = null;
    this.renderer.destroy();
    this.canvas.remove();
    this.floorEl.remove();
    this.mistEl.remove();
    this.focusEl?.remove();
    for (const o of this.opts.values()) o.remove();
    this.opts.clear();
    this.removeTitle();
    this.root.classList.remove(styles.gpu);
    if (this.cursor) this.root.classList.remove(this.cursor);
  }

  /** The GPU went away (driver reset, GPU process crash): carry on as the DOM Flow. */
  private fallBack(): void {
    if (this.dom || this.gone) return;
    this.teardown();
    this.dom = { update: () => undefined, move: () => false, rectOf: () => null, destroy: () => undefined };
    void import('./flow').then(({ flow }) => {
      if (this.gone) return;
      this.dom = flow.mount(this.root, this.ctx);
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

/** Flow on the GPU, once its pipeline compiles here (null: use the DOM Flow). */
export async function gpuFlow(gpu: Gpu): Promise<LensModule | null> {
  const [scene, mips] = await Promise.all([prepareScene3D(gpu), posterCache(gpu).prepare()]);
  if (!scene || !mips) return null;
  return {
    mount(root, ctx) {
      if (gpu.lost) throw new Error('The GPU device is gone.');
      return new GpuFlowView(root, ctx, gpu);
    },
  };
}
