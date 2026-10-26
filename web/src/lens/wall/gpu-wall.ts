/**
 * Wall, drawn with WebGPU. The same layout, marks and behaviour as the DOM Wall
 * (wall.ts, still used where WebGPU isn't available), but a frame is one draw
 * call however many titles are on screen: scrolling changes one number, a zoom
 * morphs every tile on the GPU, and a pinch follows your fingers live before it
 * settles on the nearest density.
 *
 * The DOM keeps what it is good at: native scrolling (momentum, scroll chaining,
 * keyboard scrolling), one focusable link that carries keyboard focus for screen
 * readers, and one under the pointer so middle-click and "open in new tab" work.
 */
import type { Id, ItemSummary } from '../../api/types';
import { sourceLabel } from '../../components/art-label';
import { artLabel } from '../art';
import { clamp, el, prefs, reducedMotion } from '../dom';
import type { Gpu } from '../gpu/device';
import { lerp, lerpRect, soft, spring, Tween, type Rect } from '../gpu/motion';
import { classFor, posterCache, type PosterCache } from '../gpu/posters';
import { clock, FLAG, KIND, prepareQuads, QuadBatch, QuadRenderer, type RGBA } from '../gpu/quads';
import { textAtlas, wrap, type Glyph, type TextAtlas, type TextStyle } from '../gpu/text';
import { isNew, isWatchedItem, progressFraction } from '../sections';
import type { Dir, LensContext, LensModule, LensView } from '../types';
import { LEVELS, layoutWall, rowAt, type Row } from './layout';
import styles from './wall.module.css';
import { cssColor, dcColor, paperOf } from '../gpu/colors';

/** Pixels above and below the window that are still drawn (hover lifts, zooms). */
const OVERDRAW = 80;
const FAST_PX_PER_MS = 3.5;
const ZOOM_S = 0.56;
const REFLOW_S = 0.48;
const VANISH_S = 0.24;
const HOVER_S = 0.5;

const ZOOM_OUT = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" aria-hidden="true"><path d="M5.5 12h13"/></svg>';
const ZOOM_IN = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" aria-hidden="true"><path d="M12 5.5v13M5.5 12h13"/></svg>';

const WHITE: RGBA = [1, 1, 1, 1];
const INK: RGBA = [0.914, 0.902, 0.875, 1];
const PILL: RGBA = [8 / 255, 8 / 255, 10 / 255, 1];
const BLACK: RGBA = [0, 0, 0, 1];

interface Theme {
  text: RGBA;
  text3: RGBA;
  focus: RGBA;
  surface: RGBA;
  surface2: RGBA;
  body: string;
  mono: string;
}

/** Where the titles were: enough of a layout to say where any title sat on screen. */
interface Geometry {
  level: number;
  rows: Row[];
  px: Float64Array;
  py: Float64Array;
  tileW: number;
  tileH: number;
  padL: number;
  scroll: number;
}

interface TextItem {
  glyph: Glyph;
  x: number;
  y: number;
  color: RGBA;
}

interface Morph {
  kind: 'zoom' | 'reflow';
  start: number;
  dur: number;
  /** Zoom: the titles to draw while it runs (on screen before or after). */
  draw: number[] | null;
  /** Where titles start, by id, in content coordinates at the current scroll. */
  from: Map<Id, Rect>;
  /** Titles that left (a narrower search): they shrink away where they were. */
  ghosts: { item: ItemSummary; rect: Rect }[];
  fromLevel: number;
  /** The previous level's section labels, fading out. */
  heads: TextItem[];
  headsGen: number;
}

interface Look {
  marks: number;
  caps: number;
  radius: number;
  shadow: number;
}

function radiusOf(level: number): number {
  return level === 0 ? 3 : level === 1 ? 6 : 8;
}


export class GpuWallView implements LensView {
  private root: HTMLElement;
  private ctx: LensContext;
  private mini: boolean;
  private canvas: HTMLCanvasElement;
  private scroller: HTMLDivElement;
  private grid: HTMLDivElement;
  private focusLink: HTMLAnchorElement;
  /** The titles on screen as invisible links, and the sections as row headers (see syncMirror). */
  private mirror = new Map<Id, HTMLAnchorElement>();
  private mirrorHeads = new Map<string, HTMLElement>();
  private mirrorAt = 0;
  private twin: HTMLAnchorElement | null = null;
  private tip: HTMLDivElement | null = null;
  private renderer: QuadRenderer;
  private posters: PosterCache;
  private text: TextAtlas;
  private batch = new QuadBatch(2048);
  private theme: Theme;

  private items: readonly ItemSummary[];
  private total: number;
  private level: number;
  private rows: Row[] = [];
  private px: Float64Array = new Float64Array(0);
  private py: Float64Array = new Float64Array(0);
  private rowOf: Int32Array = new Int32Array(0);
  private tileW = 180;
  private tileH = 270;
  private padL = 16;
  private padX = 16;
  private height = 0;
  private viewW = 0;
  private viewH = 0;
  private dpr = 1;

  private focusIndex = -1;
  private keyed = false;
  private lastKeyAt = 0;
  private hoverIndex = -1;
  private hovers = new Map<Id, Tween>();
  private pointer: { x: number; y: number } | null = null;
  private hiddenId: Id | null = null;
  private morph: Morph | null = null;
  private pinch = { active: false, scale: 1, fx: 0, fy: 0, settle: new Tween(soft, 1) };
  private fast = false;
  private paused = false;
  private destroyed = false;
  private shimmer = false;
  private lastY = 0;
  private lastT = 0;
  private quietUntil = 0;
  private lastTrim = 0;
  private raf = 0;
  private idle: ReturnType<typeof setTimeout> | null = null;
  private hoverT: ReturnType<typeof setTimeout> | null = null;
  private pinchT: ReturnType<typeof setTimeout> | null = null;
  private ro: ResizeObserver;
  private zoomOut: HTMLButtonElement | null = null;
  private zoomIn: HTMLButtonElement | null = null;
  private zoomLabel: HTMLSpanElement | null = null;
  private cleanup: (() => void)[] = [];
  /** After a lost GPU: the DOM Wall, which this view then forwards to. */
  private dom: LensView | null = null;

  constructor(root: HTMLElement, ctx: LensContext, gpu: Gpu) {
    this.root = root;
    this.ctx = ctx;
    this.mini = ctx.env.mini;
    this.items = ctx.items;
    this.total = ctx.total;
    this.level = clamp(Math.round(prefs.get('wall.level', 2)), 0, LEVELS.length - 1);
    root.classList.add(styles.wall, styles.gpu);
    this.canvas = el('canvas', styles.canvas, { 'aria-hidden': 'true' });
    this.scroller = el('div', styles.scroller, { 'data-scroller': '' });
    this.grid = el('div', styles.content, { role: 'grid', 'aria-label': 'Library' });
    // Carries keyboard focus (and the screen reader's attention) to the focused title, wherever it is.
    this.focusLink = el('a', `${styles.link} ${styles.mirror}`, { role: 'gridcell', tabindex: '-1', draggable: 'false' });
    this.focusLink.hidden = true;
    this.grid.append(this.focusLink);
    this.scroller.append(this.grid);
    root.append(this.canvas, this.scroller);
    this.renderer = new QuadRenderer(gpu, this.canvas);
    this.posters = posterCache(gpu);
    this.text = textAtlas(gpu);
    this.theme = this.readTheme();
    this.measure();
    // Holding the poster cache keeps its textures while any GPU lens is mounted.
    this.cleanup.push(this.posters.retain(), this.posters.onReady(this.invalidate), this.text.onChange(this.invalidate), gpu.onLost(() => this.fallBack()));
    if (!this.mini) {
      this.tip = el('div', `${styles.tip} glass glass-clear`, { 'aria-hidden': 'true' });
      root.append(this.tip);
      this.bind();
      this.buildAccessory();
    }
    this.watchTheme();
    this.layout();
    this.ro = new ResizeObserver(() => {
      this.measure();
      this.layout();
      this.invalidate();
    });
    this.ro.observe(root);
    // Arriving from another lens (or back from detail): keep your place.
    if (ctx.focus !== null) this.reveal(ctx.focus, false);
    this.invalidate();
  }

  // ── setup ──

  private readTheme(): Theme {
    const cs = getComputedStyle(this.root);
    const v = (k: string) => cs.getPropertyValue(k);
    return {
      text: cssColor(v('--text'), [0.95, 0.95, 0.94, 1]),
      text3: cssColor(v('--text-3'), [0.95, 0.95, 0.94, 0.46]),
      focus: cssColor(v('--focus'), WHITE),
      surface: cssColor(v('--surface'), [0.09, 0.09, 0.11, 1]),
      surface2: cssColor(v('--surface-2'), [0.125, 0.125, 0.153, 1]),
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

  /** Canvas size and the device pixel ratio (a preview in the lens gallery is scaled down: fewer pixels). */
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
  }

  private layout(): void {
    const L = LEVELS[this.level];
    const W = this.viewW;
    const safe = this.ctx.safe;
    this.padX = this.level === 0 ? 14 : clamp(W * 0.035, 16, 56);
    this.padL = this.level === 0 && !this.mini ? clamp(W * 0.11, 84, 168) : this.padX;
    const items = this.items;
    const out = layoutWall({
      count: items.length,
      total: this.total,
      sectionAt: (i) => this.ctx.sectionOf(items[i]),
      inner: W - this.padL - this.padX,
      padL: this.padL,
      w: L.w,
      gap: L.gap,
      cap: L.cap,
      head: L.head,
      top: this.mini ? 24 : safe.top + (this.level === 0 ? 8 : 14),
      bottom: this.mini ? 24 : safe.bottom + 48,
    });
    ({ rows: this.rows, px: this.px, py: this.py, rowOf: this.rowOf, tileW: this.tileW, tileH: this.tileH, height: this.height } = out);
    this.grid.style.height = `${this.height}px`;
    this.root.dataset.level = String(this.level);
    if (this.zoomLabel && this.zoomOut && this.zoomIn) {
      this.zoomLabel.textContent = L.name;
      this.zoomOut.disabled = this.level === 0;
      this.zoomIn.disabled = this.level === LEVELS.length - 1;
    }
    if (this.focusIndex >= 0) this.placeLink(this.focusLink, this.focusIndex);
    this.syncMirror();
  }

  private geometry(): Geometry {
    return { level: this.level, rows: this.rows, px: this.px, py: this.py, tileW: this.tileW, tileH: this.tileH, padL: this.padL, scroll: this.scroller.scrollTop };
  }

  private buildAccessory(): void {
    const box = el('div', styles.acc);
    this.zoomOut = el('button', styles.accBtn, { type: 'button', 'aria-label': 'Zoom out', title: 'Zoom out (−)' });
    this.zoomOut.innerHTML = ZOOM_OUT;
    this.zoomIn = el('button', styles.accBtn, { type: 'button', 'aria-label': 'Zoom in', title: 'Zoom in (+)' });
    this.zoomIn.innerHTML = ZOOM_IN;
    this.zoomLabel = el('span', `${styles.accLabel} mono`, { 'aria-live': 'polite' });
    this.zoomOut.addEventListener('click', () => this.setLevel(this.level - 1));
    this.zoomIn.addEventListener('click', () => this.setLevel(this.level + 1));
    box.append(this.zoomOut, this.zoomLabel, this.zoomIn);
    this.ctx.setAccessory(box);
    this.layout();
  }

  // ── frames ──

  private invalidate = (): void => {
    if (!this.raf && !this.destroyed && !this.dom) this.raf = requestAnimationFrame(this.frame);
  };

  private frame = (): void => {
    this.raf = 0;
    if (this.destroyed || this.dom) return;
    const now = clock();
    if (Math.min(window.devicePixelRatio || 1, 3) !== this.dpr && !this.mini) this.measure();
    const posters = this.posters;
    // Upload first: a slot drawn in the last frame is still "in use" and can't be evicted for a new one.
    posters.upload();
    posters.begin();
    const gen = this.text.gen;
    this.build(now);
    // The glyph atlas started over while building: build again with fresh glyphs.
    if (this.text.gen !== gen) this.build(now);
    this.text.flush();
    const pinchScale = this.pinch.active ? this.pinch.scale : this.pinch.settle.value(now);
    this.renderer.draw(
      this.batch,
      {
        width: this.viewW,
        height: this.viewH,
        scrollX: 0,
        scrollY: this.scroller.scrollTop,
        focalX: this.pinch.fx,
        focalY: this.pinch.fy,
        scale: pinchScale,
        dpr: this.dpr,
        time: now,
        t: 1,
      },
      { posters: posters.views(), glyphs: this.text.view, key: `${posters.version}` },
    );
    if (!this.mini && !this.fast) this.prefetch();
    posters.pump();
    if (performance.now() - this.lastTrim > 5000) {
      this.lastTrim = performance.now();
      posters.trim();
    }
    if (this.morph && now >= this.morph.start + this.morph.dur) this.morph = null;
    for (const [id, tw] of this.hovers) if (tw.done(now) && tw.to === 0) this.hovers.delete(id);
    const moving = this.morph !== null || this.hovers.size > 0 || !this.pinch.settle.done(now) || posters.pending || now < posters.fadeUntil || this.shimmer;
    if (moving && !this.paused) this.invalidate();
  };

  // ── scene ──

  private look(e: number): Look {
    const from = this.morph?.kind === 'zoom' ? this.morph.fromLevel : this.level;
    const to = this.level;
    return {
      marks: lerp(from >= 1 ? 1 : 0, to >= 1 ? 1 : 0, e),
      caps: lerp(LEVELS[from].cap > 0 ? 1 : 0, LEVELS[to].cap > 0 ? 1 : 0, e),
      radius: lerp(radiusOf(from), radiusOf(to), e),
      shadow: lerp(from >= 1 ? 1 : 0, to >= 1 ? 1 : 0, e),
    };
  }

  private progress(now: number): number {
    const m = this.morph;
    if (!m) return 1;
    return soft(clamp((now - m.start) / m.dur, 0, 1));
  }

  /** Where title `i` is drawn now (mid-morph: between where it was and where it goes). */
  private rectAt(i: number, e: number): Rect {
    const to = { x: this.px[i], y: this.py[i], w: this.tileW, h: this.tileW * 1.5 };
    const f = this.morph?.from.get(this.items[i].id);
    return f ? lerpRect(f, to, e) : to;
  }

  private build(now: number): void {
    const b = this.batch;
    b.reset();
    this.shimmer = false;
    if (!this.items.length && !this.total) return;
    const scroll = this.scroller.scrollTop;
    const top = scroll - OVERDRAW;
    const bottom = scroll + this.viewH + OVERDRAW;
    const m = this.morph;
    const e = this.progress(now);
    const look = this.look(e);
    const headAlpha = m?.kind === 'zoom' ? e : 1;
    // Section labels and placeholders.
    for (let r = rowAt(this.rows, top); r < this.rows.length && this.rows[r].y <= bottom; r++) {
      const row = this.rows[r];
      if (row.kind === 'head') for (const t of this.headText(row, this.level, this.padL)) this.drawText(t, headAlpha);
      else if (row.kind === 'skel') this.skeletons(row);
    }
    if (m?.kind === 'zoom' && m.headsGen === this.text.gen) for (const t of m.heads) this.drawText(t, 1 - e);
    // Titles.
    const hovered = this.hoverIndex;
    if (m?.draw) {
      for (const i of m.draw) if (i !== hovered) this.tileAt(i, now, e, look, 1, top, bottom);
    } else {
      for (let r = rowAt(this.rows, top); r < this.rows.length && this.rows[r].y <= bottom; r++) {
        const row = this.rows[r];
        if (row.kind !== 'items') continue;
        for (let i = row.start; i < row.end; i++) if (i !== hovered) this.tileAt(i, now, e, look, 1, top, bottom);
      }
    }
    if (m) {
      const v = clamp((now - m.start) / VANISH_S, 0, 1);
      if (v < 1) {
        for (const g of m.ghosts) {
          const s = 1 - 0.14 * v * v;
          const r = { x: g.rect.x + (g.rect.w * (1 - s)) / 2, y: g.rect.y + (g.rect.h * (1 - s)) / 2, w: g.rect.w * s, h: g.rect.h * s };
          this.tile(g.item, -1, r, r, (1 - v) * (1 - v), look, 0);
        }
      }
    }
    // The pointed-at title last, so it lifts over its neighbours.
    if (hovered >= 0 && hovered < this.items.length) this.tileAt(hovered, now, e, look, 1, top, bottom);
  }

  private tileAt(i: number, now: number, e: number, look: Look, alpha: number, top: number, bottom: number): void {
    const item = this.items[i];
    if (!item || item.id === this.hiddenId) return;
    let base = this.rectAt(i, e);
    const m = this.morph;
    let a = alpha;
    if (m?.kind === 'reflow' && !m.from.has(item.id)) {
      // New in view: fades in, settling from a little smaller.
      const p = soft(clamp((now - m.start - 0.06) / (m.dur * 0.8), 0, 1));
      a *= p;
      const s = 0.94 + 0.06 * p;
      base = { x: base.x + (base.w * (1 - s)) / 2, y: base.y + (base.h * (1 - s)) / 2, w: base.w * s, h: base.h * s };
    }
    const cap = LEVELS[this.level].cap * look.caps;
    if (base.y > bottom || base.y + base.h + cap < top || a <= 0.002) return;
    const hv = this.hovers.get(item.id)?.value(now) ?? 0;
    let r = base;
    if (hv !== 0) {
      const s = 1 + hv * (this.level === 0 ? 0.4 : 0.025);
      const lift = this.level === 0 ? 0 : 3 * hv;
      r = { x: base.x - (base.w * (s - 1)) / 2, y: base.y - (base.h * (s - 1)) / 2 - lift, w: base.w * s, h: base.h * s };
    }
    this.tile(item, i, r, base, a, look, hv);
  }

  private tile(item: ItemSummary, i: number, r: Rect, base: Rect, a: number, look: Look, hv: number): void {
    const b = this.batch;
    const level0Hover = this.level === 0 && hv > 0;
    const radius = level0Hover ? lerp(look.radius, 5, Math.min(1, hv)) : look.radius;
    // Shadow: 0 10px 24px -14px rgb(0 0 0 / .6), deeper when lifted; the mosaic only casts one when lifted.
    const sh = level0Hover ? Math.min(1, hv) : look.shadow;
    if (sh > 0.01) {
      const oy = level0Hover ? 12 : lerp(10, 22, hv);
      const blur = level0Hover ? 28 : lerp(24, 44, hv);
      const spread = level0Hover ? -6 : lerp(-14, -18, hv);
      const alpha = (level0Hover ? 0.8 : lerp(0.6, 0.75, hv)) * sh * a;
      const sx = r.x - spread;
      const sw = r.w + spread * 2;
      const sy = r.y + oy - spread;
      const shh = r.h + spread * 2;
      if (sw > 0 && shh > 0) {
        const o = b.add(sx - blur, sy - blur, sw + blur * 2, shh + blur * 2, KIND.shadow, radius);
        b.color(o, BLACK, alpha);
        b.extra(o, blur);
      }
    }
    // The poster: its texture once loaded, its average colour (or the title's gradient) until then.
    const art = item.poster;
    const o = b.add(r.x, r.y, r.w, r.h, KIND.poster, radius);
    b.opacity(o, a);
    let flags = FLAG.hairline;
    if (!item.available) flags |= FLAG.gray;
    const dc = art?.blurhash ? dcColor(art.blurhash) : null;
    if (dc) b.color(o, dc);
    else {
      const [c1, c2] = paperOf(item);
      b.color(o, c1);
      b.uv(o, c2[0], c2[1], c2[2], c2[3]);
      flags |= FLAG.gradient;
    }
    if (art) {
      const cls = classFor(this.tileW * this.dpr);
      const mid = this.scroller.scrollTop + this.viewH / 2;
      const prio = Math.abs(base.y + base.h / 2 - mid) - (i === this.focusIndex ? 1e6 : 0);
      const hit = this.posters.get(art, cls, prio, !this.fast && !this.paused);
      if (hit) {
        b.uv(o, 0, 0, 1, 1);
        b.texture(o, hit.code, hit.readyAt, hit.texWidth);
        flags &= ~FLAG.gradient;
      }
    } else if (r.w >= 72) this.typeset(item, r, a);
    b.flags(o, flags);
    if (look.marks > 0.01) this.marks(item, r, a * look.marks);
    if (look.caps > 0.01) this.caption(item, base, a * look.caps);
    if (this.keyed && i === this.focusIndex && i >= 0 && !this.mini) {
      const ring = b.add(r.x - 5, r.y - 5, r.w + 10, r.h + 10, KIND.ring, radius + 5);
      b.color(ring, this.theme.focus, a);
      b.extra(ring, 2);
    }
  }

  /** Progress, watched, new and offline marks, and the frame/AI label: as art.ts draws them. */
  private marks(item: ItemSummary, r: Rect, a: number): void {
    const b = this.batch;
    const dpr = this.dpr;
    const mono: TextStyle = { family: this.theme.mono, size: 9.28, weight: 500, spacing: 0.08, upper: true };
    const pillH = 15.6;
    const label = item.poster ? sourceLabel(item.poster) : null;
    if (label) {
      const ai = label.tone === 'ai';
      const g = this.text.get(label.short, mono, 200, dpr);
      const icon = ai ? this.text.icon('spark', 9, dpr) : null;
      if (g) {
        const ink = g.w - g.pad * 2;
        const w = 12 + ink + (icon ? 13 : 0);
        const x = r.x + 6;
        const y = r.y + 6;
        const p = b.add(x, y, w, pillH, KIND.solid, 4);
        b.color(p, PILL, (ai ? 0.86 : 0.72) * a);
        if (ai) {
          const ring = b.add(x, y, w, pillH, KIND.ring, 4);
          b.color(ring, WHITE, 0.55 * a);
          b.extra(ring, 1);
        }
        let tx = x + 6;
        if (icon) {
          this.glyphAt(icon, tx - icon.pad, y + (pillH - icon.h) / 2, WHITE, a);
          tx += 13;
        }
        this.glyphAt(g, tx - g.pad, y + pillH / 2 + 3.2 - g.baseline, ai ? WHITE : INK, a);
      }
    }
    const prog = progressFraction(item);
    if (prog !== null) {
      const tx = r.x + r.w * 0.08;
      const tw = r.w * 0.84;
      const ty = r.y + r.h - 10;
      const track = b.add(tx, ty, tw, 3, KIND.solid, 1.5);
      b.color(track, WHITE, 0.3 * a);
      const fill = b.add(tx, ty, tw * Math.max(0.03, prog), 3, KIND.solid, 1.5);
      b.color(fill, WHITE, a);
    } else if (isWatchedItem(item)) {
      const x = r.x + r.w - 26;
      const y = r.y + 6;
      const c = b.add(x, y, 20, 20, KIND.solid, 10);
      b.color(c, PILL, 0.6 * a);
      const icon = this.text.icon('check', 11, dpr);
      if (icon) this.glyphAt(icon, x + 10 - icon.w / 2, y + 10 - icon.h / 2, WHITE, a);
    } else if (isNew(item)) {
      const x = r.x + r.w - 16;
      const y = r.y + 8;
      const glow = b.add(x - 12, y - 12, 32, 32, KIND.shadow, 4);
      b.color(glow, WHITE, 0.8 * a);
      b.extra(glow, 12);
      const ring = b.add(x - 3, y - 3, 14, 14, KIND.solid, 7);
      b.color(ring, BLACK, 0.25 * a);
      const dot = b.add(x, y, 8, 8, KIND.solid, 4);
      b.color(dot, WHITE, a);
    }
    if (!item.available) {
      const g = this.text.get('Offline', { ...mono, weight: 400 }, 200, dpr);
      if (g) {
        const w = 12 + g.w - g.pad * 2;
        const x = r.x + 6;
        const y = r.y + r.h - 6 - pillH;
        const p = b.add(x, y, w, pillH, KIND.solid, 4);
        b.color(p, PILL, 0.75 * a);
        this.glyphAt(g, x + 6 - g.pad, y + pillH / 2 + 3.2 - g.baseline, INK, a);
      }
    }
  }

  /** A title without art: its name set on the poster, bottom-aligned (art.ts `typeset`). */
  private typeset(item: ItemSummary, r: Rect, a: number): void {
    // Sized from the layout, not the animated rect: each title is drawn into the atlas once.
    const w = this.tileW;
    const size = Math.round(clamp(w * 0.11, 12.8, 25.6) * 4) / 4;
    const margin = w * 0.11;
    const max = Math.round(w - margin * 2);
    const style: TextStyle = { family: this.theme.body, size, weight: 500 };
    const lines = wrap(item.title, max, 4, (s) => this.text.measure(s, style));
    const lh = size * 1.05;
    let y = r.y + r.h - margin - lines.length * lh;
    for (const line of lines) {
      const g = this.text.get(line, style, max, this.dpr);
      if (g) this.glyphAt(g, r.x + margin - g.pad, y + lh * 0.82 - g.baseline, WHITE, a);
      y += lh;
    }
  }

  /** Details: the title and its edge line under the poster. */
  private caption(item: ItemSummary, r: Rect, a: number): void {
    const t = this.theme;
    const name = item.parent_title && item.kind === 'episode' ? `${item.parent_title}: ${item.title}` : item.title;
    const max = Math.round(this.tileW - 4);
    const title = this.text.get(name, { family: t.body, size: 15, weight: 500 }, max, this.dpr);
    const top = r.y + r.h + 10;
    if (title) this.glyphAt(title, r.x + 2 - title.pad, top + 14.5 - title.baseline, t.text, a);
    const edge = [item.year ? String(item.year) : '', item.kind === 'show' && item.child_count ? `${item.child_count} season${item.child_count === 1 ? '' : 's'}` : '', ...item.badges.slice(0, 2)].filter(Boolean).join(' · ');
    if (edge) {
      const g = this.text.get(edge, { family: t.mono, size: 11, weight: 400, spacing: 0.04 }, max, this.dpr);
      if (g) this.glyphAt(g, r.x + 2 - g.pad, top + 20.75 + 10.5 - g.baseline, t.text3, a);
    }
  }

  /** A section's label and count, laid out as the DOM Wall's headers are. */
  private headText(row: Row, level: number, padL: number): TextItem[] {
    const t = this.theme;
    const vw = this.viewW;
    const out: TextItem[] = [];
    const countStyle: TextStyle = { family: t.mono, size: 11, weight: 400 };
    if (level === 0) {
      if (this.mini) return out;
      const size = Math.round(clamp(14.4 + vw * 0.016, 24, 40));
      const label = this.text.get(row.section, { family: t.body, size, weight: 300, spacing: -0.01 }, Math.round(padL - 24), this.dpr);
      if (label) out.push({ glyph: label, x: padL - 18 - (label.w - label.pad), y: row.y + 14 + size * 0.84 - label.baseline, color: t.text });
      const count = this.text.get(String(row.count), countStyle, 120, this.dpr);
      if (count) out.push({ glyph: count, x: padL - 19 - (count.w - count.pad), y: row.y + 54 + 11.5 - count.baseline, color: t.text3 });
      return out;
    }
    const size = level === 1 ? 18 : level === 2 ? 24 : clamp(19.2 + vw * 0.026, 32, 56);
    const baseline = row.y + row.h - 14 - size * 0.2;
    const label = this.text.get(row.section, { family: t.body, size: Math.round(size), weight: 300, spacing: -0.01 }, Math.round(vw - padL * 2), this.dpr);
    let x = padL;
    if (label) {
      out.push({ glyph: label, x: x - label.pad, y: baseline - label.baseline, color: t.text });
      x += label.w - label.pad * 2 + 10;
    }
    const count = this.text.get(String(row.count), countStyle, 120, this.dpr);
    if (count) out.push({ glyph: count, x: x - count.pad, y: row.y + row.h - 14 - 4.5 - count.baseline, color: t.text3 });
    return out;
  }

  private skeletons(row: Row): void {
    const b = this.batch;
    const L = LEVELS[this.level];
    const t = this.theme;
    for (let s = row.start; s < row.end; s++) {
      const o = b.add(this.padL + (s - row.start) * (this.tileW + L.gap), row.y, this.tileW, this.tileW * 1.5, KIND.skeleton, 8);
      b.color(o, t.surface);
      b.uv(o, t.surface2[0], t.surface2[1], t.surface2[2], 1);
      b.opacity(o, 0.6);
    }
    this.shimmer = true;
  }

  private glyphAt(g: Glyph, x: number, y: number, color: RGBA, a: number): void {
    const o = this.batch.add(x, y, g.w, g.h, KIND.text);
    this.batch.color(o, color, a);
    this.batch.uv(o, g.u0, g.v0, g.u1, g.v1);
  }

  private drawText(t: TextItem, a: number): void {
    if (a > 0.002) this.glyphAt(t.glyph, t.x, t.y, t.color, a);
  }

  /** Posters just off screen, so they are there when you scroll to them. */
  private prefetch(): void {
    const scroll = this.scroller.scrollTop;
    const H = this.viewH;
    const cls = classFor(this.tileW * this.dpr);
    let n = 0;
    for (const [from, to] of [
      [scroll + H + OVERDRAW, scroll + H * 2],
      [scroll - H, scroll - OVERDRAW],
    ]) {
      for (let r = rowAt(this.rows, from); r < this.rows.length && this.rows[r].y <= to && n < 80; r++) {
        const row = this.rows[r];
        if (row.kind !== 'items') continue;
        for (let i = row.start; i < row.end; i++) {
          const art = this.items[i]?.poster;
          if (!art) continue;
          this.posters.want(art, cls, 5000 + Math.abs(this.py[i] - scroll - H / 2));
          n++;
        }
      }
    }
  }

  // ── links (keyboard focus, and the pointer's middle-click target) ──

  private placeLink(link: HTMLAnchorElement, i: number): void {
    const item = this.items[i];
    if (!item) {
      link.hidden = true;
      link.removeAttribute('href');
      return;
    }
    link.hidden = false;
    // Below the build's base ('/' unless built for a sub-path, src/app/base.ts).
    link.href = `${import.meta.env.BASE_URL}item/${item.id}`;
    link.dataset.id = String(item.id);
    link.setAttribute('aria-label', artLabel(item));
    link.style.transform = `translate(${this.px[i]}px, ${this.py[i]}px)`;
    link.style.width = `${this.tileW}px`;
    link.style.height = `${this.tileH}px`;
    this.syncTwin();
  }

  /**
   * The titles on screen as invisible links (and the sections as row headers):
   * screen readers can browse the wall as they could the DOM one, middle-click and
   * "open in new tab" work, and tests find titles. Synced when the layout changes
   * and every so often while scrolling, never every frame.
   */
  private syncMirror(): void {
    if (this.mini || this.destroyed || this.dom) return;
    this.mirrorAt = performance.now();
    const scroll = this.scroller.scrollTop;
    const top = scroll - 60;
    const bottom = scroll + this.viewH + 60;
    const seen = new Set<Id>();
    const seenHeads = new Set<string>();
    for (let r = rowAt(this.rows, top); r < this.rows.length && this.rows[r].y <= bottom; r++) {
      const row = this.rows[r];
      if (row.kind === 'head') {
        const key = `${row.section}@${row.start}`;
        let h = this.mirrorHeads.get(key);
        if (!h) {
          h = el('div', styles.mirrorHead, { role: 'rowheader' }, el('span', null, null, row.section), el('span', null, null, ''));
          this.mirrorHeads.set(key, h);
          this.grid.append(h);
        }
        const count = h.lastElementChild;
        if (count && count.textContent !== String(row.count)) count.textContent = String(row.count);
        h.style.transform = `translate(${this.padL}px, ${row.y}px)`;
        seenHeads.add(key);
      } else if (row.kind === 'items') {
        for (let i = row.start; i < row.end; i++) {
          const item = this.items[i];
          let a = this.mirror.get(item.id);
          if (!a) {
            a = el('a', `${styles.link} ${styles.mirror}`, { role: 'gridcell', tabindex: '-1', href: `${import.meta.env.BASE_URL}item/${item.id}`, draggable: 'false' });
            a.dataset.id = String(item.id);
            this.mirror.set(item.id, a);
            this.grid.append(a);
          }
          const label = artLabel(item);
          if (a.getAttribute('aria-label') !== label) a.setAttribute('aria-label', label);
          a.dataset.art = item.poster?.source ?? 'none';
          a.style.transform = `translate(${this.px[i]}px, ${this.py[i]}px)`;
          a.style.width = `${this.tileW}px`;
          a.style.height = `${this.tileH}px`;
          seen.add(item.id);
        }
      }
    }
    for (const [id, a] of this.mirror) {
      if (!seen.has(id)) {
        a.remove();
        this.mirror.delete(id);
      }
    }
    for (const [k, h] of this.mirrorHeads) {
      if (!seenHeads.has(k)) {
        h.remove();
        this.mirrorHeads.delete(k);
      }
    }
    this.syncTwin();
  }

  /** While the focus link stands on a title, that title's mirror link steps aside (one title, one link). */
  private syncTwin(): void {
    const id = this.focusLink.hidden ? null : Number(this.focusLink.dataset.id);
    const twin = id !== null ? (this.mirror.get(id) ?? null) : null;
    if (twin === this.twin) return;
    this.twin?.removeAttribute('aria-hidden');
    twin?.setAttribute('aria-hidden', 'true');
    this.twin = twin;
  }

  // ── updates ──

  update(items: readonly ItemSummary[], total: number): void {
    if (this.dom) return this.dom.update(items, total);
    const now = clock();
    const before = this.mini || reducedMotion() ? null : this.onScreen(now);
    const prevFocusId = this.focusIndex >= 0 ? this.items[this.focusIndex]?.id : undefined;
    const prevHoverId = this.hoverIndex >= 0 ? this.items[this.hoverIndex]?.id : undefined;
    this.items = items;
    this.total = total;
    const at = new Map<Id, number>();
    for (let i = 0; i < items.length; i++) at.set(items[i].id, i);
    this.focusIndex = prevFocusId !== undefined ? (at.get(prevFocusId) ?? -1) : -1;
    this.hoverIndex = prevHoverId !== undefined ? (at.get(prevHoverId) ?? -1) : -1;
    this.layout();
    const max = Math.max(0, this.height - this.scroller.clientHeight);
    if (this.scroller.scrollTop > max) this.scroller.scrollTop = max;
    if (before && before.size) {
      const from = new Map<Id, Rect>();
      const ghosts: Morph['ghosts'] = [];
      let moved = false;
      for (const [id, v] of before) {
        const i = at.get(id);
        if (i === undefined) {
          ghosts.push(v);
          continue;
        }
        from.set(id, v.rect);
        if (Math.abs(v.rect.x - this.px[i]) > 0.5 || Math.abs(v.rect.y - this.py[i]) > 0.5 || Math.abs(v.rect.w - this.tileW) > 0.5) moved = true;
      }
      // Anything new on screen fades in; nothing changed on screen: no animation at all.
      const scroll = this.scroller.scrollTop;
      let appeared = false;
      for (let r = rowAt(this.rows, scroll); r < this.rows.length && this.rows[r].y <= scroll + this.viewH && !appeared; r++) {
        const row = this.rows[r];
        if (row.kind !== 'items') continue;
        for (let i = row.start; i < row.end; i++) {
          if (!from.has(items[i].id)) {
            appeared = true;
            break;
          }
        }
      }
      if (moved || ghosts.length || appeared) this.morph = { kind: 'reflow', start: now, dur: REFLOW_S, draw: null, from, ghosts, fromLevel: this.level, heads: [], headsGen: this.text.gen };
    }
    this.invalidate();
  }

  /** Titles on screen now and where they are drawn (mid-morph positions included), content coordinates. */
  private onScreen(now: number): Map<Id, { item: ItemSummary; rect: Rect }> {
    const out = new Map<Id, { item: ItemSummary; rect: Rect }>();
    const scroll = this.scroller.scrollTop;
    const e = this.progress(now);
    const top = scroll - OVERDRAW;
    const bottom = scroll + this.viewH + OVERDRAW;
    const add = (i: number) => {
      const item = this.items[i];
      if (!item) return;
      const rect = this.rectAt(i, e);
      if (rect.y > bottom || rect.y + rect.h < top) return;
      out.set(item.id, { item, rect });
    };
    if (this.morph?.draw) for (const i of this.morph.draw) add(i);
    else
      for (let r = rowAt(this.rows, top); r < this.rows.length && this.rows[r].y <= bottom; r++) {
        const row = this.rows[r];
        if (row.kind === 'items') for (let i = row.start; i < row.end; i++) add(i);
      }
    return out;
  }

  // ── zoom ──

  private setLevel(n: number, point?: { x: number; y: number }): void {
    n = clamp(n, 0, LEVELS.length - 1);
    if (n === this.level) {
      this.settlePinch();
      return;
    }
    const now = clock();
    const rr = this.root.getBoundingClientRect();
    const old = this.geometry();
    const anchor = this.anchorIndex(point);
    const p = point ?? (anchor >= 0 ? { x: rr.left + old.px[anchor] + old.tileW / 2, y: rr.top + old.py[anchor] - old.scroll + old.tileH / 2 } : null);
    const u = anchor >= 0 && p ? clamp((p.y - (rr.top + old.py[anchor] - old.scroll)) / old.tileH, 0, 1) : 0.5;
    // Where every title on screen is drawn right now: mid-morph and mid-pinch positions included.
    const was = this.onScreen(now);
    const ps = this.pinch.active ? this.pinch.scale : this.pinch.settle.value(now);
    const fx = this.pinch.fx;
    const fy = this.pinch.fy;
    const oldHeads: TextItem[] = [];
    for (let r = rowAt(old.rows, old.scroll); r < old.rows.length && old.rows[r].y <= old.scroll + this.viewH; r++) {
      if (old.rows[r].kind === 'head') oldHeads.push(...this.headText(old.rows[r], old.level, old.padL));
    }
    this.level = n;
    prefs.set('wall.level', n);
    this.layout();
    if (anchor >= 0 && p) {
      this.quietUntil = performance.now() + 250;
      this.scroller.scrollTop = this.py[anchor] + u * this.tileH - (p.y - rr.top);
    }
    const scroll = this.scroller.scrollTop;
    this.syncMirror();
    const shift = scroll - old.scroll;
    // Into the new layout's content space, through the pinch that was showing.
    const place = (r: Rect): Rect => {
      if (ps === 1) return { x: r.x, y: r.y + shift, w: r.w, h: r.h };
      const sx = fx + (r.x - fx) * ps;
      const sy = fy + (r.y - old.scroll - fy) * ps;
      return { x: sx, y: sy + scroll, w: r.w * ps, h: r.h * ps };
    };
    const from = new Map<Id, Rect>();
    const draw = new Set<number>();
    for (const [id, v] of was) from.set(id, place(v.rect));
    const index = new Map<Id, number>();
    for (let i = 0; i < this.items.length; i++) index.set(this.items[i].id, i);
    for (const id of from.keys()) {
      const i = index.get(id);
      if (i !== undefined) draw.add(i);
    }
    // Titles arriving on screen come from where the old layout had them.
    for (let r = rowAt(this.rows, scroll - OVERDRAW); r < this.rows.length && this.rows[r].y <= scroll + this.viewH + OVERDRAW; r++) {
      const row = this.rows[r];
      if (row.kind !== 'items') continue;
      for (let i = row.start; i < row.end; i++) {
        draw.add(i);
        const id = this.items[i].id;
        if (!from.has(id) && i < old.px.length) from.set(id, place({ x: old.px[i], y: old.py[i], w: old.tileW, h: old.tileW * 1.5 }));
      }
    }
    this.pinch.active = false;
    this.pinch.scale = 1;
    this.pinch.settle = new Tween(soft, 1);
    const heads = oldHeads.map((t) => {
      const r = place({ x: t.x, y: t.y, w: t.glyph.w, h: t.glyph.h });
      return { ...t, x: r.x, y: r.y };
    });
    this.morph = reducedMotion() ? null : { kind: 'zoom', start: now, dur: ZOOM_S, draw: [...draw].sort((a, b) => a - b), from, ghosts: [], fromLevel: old.level, heads, headsGen: this.text.gen };
    this.setHover(-1);
    this.hideTip();
    this.invalidate();
  }

  private anchorIndex(point?: { x: number; y: number }): number {
    const scroll = this.scroller.scrollTop;
    if (point) {
      const i = this.indexAt(point.x, point.y, true);
      if (i >= 0) return i;
    }
    if (this.focusIndex >= 0) return this.focusIndex;
    const mid = rowAt(this.rows, scroll + this.viewH / 2);
    for (let d = 0; d < this.rows.length; d++) {
      for (const k of [mid + d, mid - d]) {
        const r = this.rows[k];
        if (r?.kind === 'items') return r.start + Math.floor((r.end - r.start) / 2);
      }
    }
    return -1;
  }

  /** The title under a viewport point; with `near`, the nearest one in that row (zoom anchors). */
  private indexAt(clientX: number, clientY: number, near = false): number {
    const rr = this.root.getBoundingClientRect();
    const x = clientX - rr.left;
    const y = clientY - rr.top + this.scroller.scrollTop;
    const row = this.rows[rowAt(this.rows, y)];
    if (!row || row.kind !== 'items' || y < row.y) return -1;
    const L = LEVELS[this.level];
    const col = Math.floor((x - this.padL) / (this.tileW + L.gap));
    if (near) return row.start + clamp(col, 0, row.end - row.start - 1);
    if (col < 0 || col >= row.end - row.start) return -1;
    const i = row.start + col;
    if (x > this.px[i] + this.tileW || y > this.py[i] + this.tileH) return -1;
    return i;
  }

  /** A pinch let go between densities: spring back to this one. */
  private settlePinch(): void {
    const now = clock();
    const from = this.pinch.active ? this.pinch.scale : this.pinch.settle.value(now);
    this.pinch.active = false;
    this.pinch.scale = 1;
    this.pinch.settle = new Tween(soft, from);
    this.pinch.settle.set(1, now, reducedMotion() ? 0 : 0.32);
    this.invalidate();
  }

  /** Live pinch: scale everything about the focal point; settle on a density when it stops. */
  private pinchTo(scale: number, clientX: number, clientY: number): void {
    const rr = this.root.getBoundingClientRect();
    if (!this.pinch.active) {
      this.pinch.active = true;
      this.pinch.scale = this.pinch.settle.value(clock());
      this.setHover(-1);
      this.hideTip();
    }
    // A little give past the ends.
    const lo = this.level === 0 ? 0.82 : 0.32;
    const hi = this.level === LEVELS.length - 1 ? 1.22 : 2.6;
    this.pinch.scale = clamp(scale, lo, hi);
    this.pinch.fx = clientX - rr.left;
    this.pinch.fy = clientY - rr.top;
    this.invalidate();
    if (this.pinchT) clearTimeout(this.pinchT);
    this.pinchT = setTimeout(() => this.commitPinch(), 140);
  }

  private commitPinch(): void {
    if (!this.pinch.active) return;
    const s = this.pinch.scale;
    const want = LEVELS[this.level].w * s;
    let best = this.level;
    for (let l = 0; l < LEVELS.length; l++) if (Math.abs(Math.log(LEVELS[l].w / want)) < Math.abs(Math.log(LEVELS[best].w / want))) best = l;
    // Commit only past the midpoint to the next density (in log space).
    if (best !== this.level) {
      const rr = this.root.getBoundingClientRect();
      this.setLevel(best, { x: rr.left + this.pinch.fx, y: rr.top + this.pinch.fy });
    } else this.settlePinch();
  }

  // ── input ──

  private bind(): void {
    const sc = this.scroller;
    const on = <K extends keyof HTMLElementEventMap>(t: HTMLElement, type: K, fn: (e: HTMLElementEventMap[K]) => void, opts?: AddEventListenerOptions) => {
      t.addEventListener(type, fn as EventListener, opts);
      this.cleanup.push(() => t.removeEventListener(type, fn as EventListener, opts));
    };
    on(
      sc,
      'scroll',
      () => {
        const y = sc.scrollTop;
        const now = performance.now();
        const speed = Math.abs(y - this.lastY) / Math.max(1, now - this.lastT);
        if (y < 40) this.ctx.scrolled('top');
        else if (now < this.quietUntil) {
          // our own scroll
        } else if (y > this.lastY + 6) this.ctx.scrolled('down');
        else if (y < this.lastY - 6) this.ctx.scrolled('up');
        if (speed > FAST_PX_PER_MS) this.fast = true;
        this.lastY = y;
        this.lastT = now;
        if (this.hoverIndex >= 0) this.setHover(-1);
        this.hideTip();
        this.invalidate();
        if (now - this.mirrorAt > 150) this.syncMirror();
        if (this.idle) clearTimeout(this.idle);
        this.idle = setTimeout(() => {
          this.fast = false;
          this.syncMirror();
          // The pointer rests over whatever scrolled under it.
          if (this.pointer) this.hoverAt(this.pointer.x, this.pointer.y);
          this.invalidate();
        }, 140);
      },
      { passive: true },
    );

    // Trackpad pinch arrives as ctrl+wheel in Chromium and Firefox: follow it live.
    on(
      sc,
      'wheel',
      (e) => {
        if (!e.ctrlKey) return;
        e.preventDefault();
        const d = clamp(e.deltaMode === 1 ? e.deltaY * 16 : e.deltaY, -40, 40);
        const cur = this.pinch.active ? this.pinch.scale : this.pinch.settle.value(clock());
        this.pinchTo(cur * Math.exp(-d * 0.011), e.clientX, e.clientY);
      },
      { passive: false },
    );
    // Safari's trackpad pinch.
    type GestureEvent = UIEvent & { scale: number; clientX: number; clientY: number };
    const gstart = (e: Event) => e.preventDefault();
    const gchange = (e: Event) => {
      e.preventDefault();
      const g = e as GestureEvent;
      this.pinchTo(g.scale, g.clientX, g.clientY);
    };
    sc.addEventListener('gesturestart', gstart);
    sc.addEventListener('gesturechange', gchange);
    this.cleanup.push(() => {
      sc.removeEventListener('gesturestart', gstart);
      sc.removeEventListener('gesturechange', gchange);
    });
    // Two fingers on a touch screen.
    const pts = new Map<number, { x: number; y: number }>();
    let base = 0;
    const dist = () => {
      const [a, b] = [...pts.values()];
      return Math.hypot(a.x - b.x, a.y - b.y);
    };
    on(sc, 'pointerdown', (e) => {
      if (e.pointerType !== 'touch') return;
      pts.set(e.pointerId, { x: e.clientX, y: e.clientY });
      if (pts.size === 2) base = dist();
    });
    on(sc, 'pointermove', (e) => {
      if (e.pointerType !== 'touch') {
        this.pointer = { x: e.clientX, y: e.clientY };
        this.hoverAt(e.clientX, e.clientY);
        return;
      }
      if (!pts.has(e.pointerId)) return;
      pts.set(e.pointerId, { x: e.clientX, y: e.clientY });
      if (pts.size === 2 && base) {
        const [a, b] = [...pts.values()];
        this.pinchTo(dist() / base, (a.x + b.x) / 2, (a.y + b.y) / 2);
      }
    });
    const up = (e: PointerEvent) => {
      pts.delete(e.pointerId);
      if (pts.size < 2 && base) {
        base = 0;
        this.commitPinch();
      }
    };
    on(sc, 'pointerup', up);
    on(sc, 'pointercancel', up);
    on(sc, 'pointerleave', (e) => {
      if (e.pointerType === 'touch') return;
      this.pointer = null;
      this.setHover(-1);
      this.hideTip();
    });
    on(sc, 'click', (e) => {
      if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
      const i = this.indexAt(e.clientX, e.clientY);
      if (i < 0) return;
      e.preventDefault();
      const item = this.items[i];
      this.focusIndex = i;
      this.keyed = false;
      this.ctx.setFocus(item.id);
      this.ctx.open(item.id, null);
    });
  }

  private hoverAt(clientX: number, clientY: number): void {
    if (this.pinch.active || this.morph?.kind === 'zoom') return;
    this.setHover(this.indexAt(clientX, clientY));
  }

  private setHover(i: number): void {
    if (i === this.hoverIndex) return;
    const now = clock();
    const prev = this.hoverIndex >= 0 ? this.items[this.hoverIndex] : undefined;
    if (prev) this.tweenHover(prev.id, 0, now);
    this.hoverIndex = i;
    if (this.hoverT) clearTimeout(this.hoverT);
    if (i < 0) {
      this.hideTip();
      this.invalidate();
      return;
    }
    const item = this.items[i];
    this.tweenHover(item.id, 1, now);
    this.showTip(i);
    // Hover lights the room and warms the detail (a beat later, so passing over doesn't).
    this.hoverT = setTimeout(() => {
      this.ctx.setFocus(item.id);
      this.ctx.intent(item);
    }, 110);
    this.invalidate();
  }

  private tweenHover(id: Id, to: number, now: number): void {
    let t = this.hovers.get(id);
    if (!t) {
      t = new Tween(spring, 0);
      this.hovers.set(id, t);
    }
    t.set(to, now, reducedMotion() ? 0 : HOVER_S);
  }

  private showTip(i: number): void {
    if (!this.tip || LEVELS[this.level].cap > 0) return this.hideTip();
    const item = this.items[i];
    if (!item) return;
    this.tip.replaceChildren(el('b', null, null, item.title), el('span', 'mono', null, item.year ? String(item.year) : ''));
    this.tip.classList.add(styles.on);
    const tw = this.tip.offsetWidth;
    const scroll = this.scroller.scrollTop;
    const x = clamp(this.px[i] + this.tileW / 2 - tw / 2, 10, this.viewW - tw - 10);
    const above = this.py[i] - scroll - 40;
    const y = above < this.ctx.safe.top - 10 ? this.py[i] - scroll + this.tileW * 1.5 + 8 : above;
    this.tip.style.transform = `translate(${x}px, ${y}px)`;
  }

  private hideTip(): void {
    this.tip?.classList.remove(styles.on);
  }

  // ── keyboard ──

  move(dir: Dir): boolean {
    if (this.dom) return this.dom.move(dir);
    const n = this.items.length;
    if (!n) return false;
    let i = this.focusIndex;
    // The first press picks a title to start from; the next ones move.
    if (i < 0) i = this.startIndex();
    else if (dir === 'left') i = Math.max(0, i - 1);
    else if (dir === 'right') i = Math.min(n - 1, i + 1);
    else {
      const step = dir === 'up' ? -1 : 1;
      let r = this.rowOf[i] + step;
      while (r >= 0 && r < this.rows.length && this.rows[r].kind !== 'items') r += step;
      const row = this.rows[r];
      if (!row) return true;
      const x = this.px[i];
      let best = row.start;
      for (let k = row.start; k < row.end; k++) if (Math.abs(this.px[k] - x) < Math.abs(this.px[best] - x)) best = k;
      i = best;
    }
    if (i < 0) return false;
    this.setKeyFocus(i);
    return true;
  }

  /** Where the arrows start: the title you point at, else the first one in view. */
  private startIndex(): number {
    const scroll = this.scroller.scrollTop;
    const top = scroll + this.ctx.safe.top;
    const bottom = scroll + this.viewH - this.ctx.safe.bottom;
    const f = this.ctx.focus;
    if (f !== null) {
      const i = this.items.findIndex((x) => x.id === f);
      if (i >= 0 && this.py[i] + this.tileH > top && this.py[i] < bottom) return i;
    }
    for (let r = rowAt(this.rows, top); r < this.rows.length && this.rows[r].y < bottom; r++) {
      const row = this.rows[r];
      if (row.kind === 'items' && row.y + this.tileH / 2 > top) return row.start;
    }
    return this.anchorIndex();
  }

  private setKeyFocus(i: number, scroll = true): void {
    this.focusIndex = i;
    this.keyed = true;
    const now = performance.now();
    const quick = now - this.lastKeyAt < 160;
    this.lastKeyAt = now;
    const item = this.items[i];
    this.ctx.setFocus(item.id);
    this.ctx.intent(item);
    if (scroll) this.scrollToIndex(i, 'nearest', quick);
    this.placeLink(this.focusLink, i);
    if (!this.mini) {
      const a = document.activeElement;
      if (!a || a === document.body || this.root.contains(a)) this.focusLink.focus({ preventScroll: true });
    }
    this.showTip(i);
    this.invalidate();
  }

  private scrollToIndex(i: number, align: 'nearest' | 'center', instant = false): void {
    const safe = this.ctx.safe;
    const top = this.py[i];
    const scroll = this.scroller.scrollTop;
    const viewTop = scroll + safe.top + 12;
    const viewBottom = scroll + this.viewH - safe.bottom - 12;
    let target: number | null = null;
    if (align === 'center') target = top + this.tileH / 2 - this.viewH / 2;
    else if (top < viewTop) target = top - safe.top - 12;
    else if (top + this.tileH > viewBottom) target = top + this.tileH - this.viewH + safe.bottom + 12;
    if (target === null) return;
    this.scroller.scrollTo({ top: Math.max(0, target), behavior: instant || reducedMotion() || align === 'center' ? 'auto' : 'smooth' });
    this.invalidate();
  }

  key(e: KeyboardEvent): boolean {
    if (this.dom) return this.dom.key?.(e) ?? false;
    if (e.key === '+' || e.key === '=') {
      this.setLevel(this.level + 1);
      return true;
    }
    if (e.key === '-' || e.key === '_') {
      this.setLevel(this.level - 1);
      return true;
    }
    if (e.key === 'Enter' && this.focusIndex >= 0) {
      const item = this.items[this.focusIndex];
      if (item) this.ctx.open(item.id, null);
      return true;
    }
    return false;
  }

  // ── the shell's questions ──

  rectOf(id: Id): DOMRect | null {
    if (this.dom) return this.dom.rectOf(id);
    const i = this.items.findIndex((x) => x.id === id);
    if (i < 0) return null;
    const rr = this.root.getBoundingClientRect();
    return new DOMRect(rr.left + this.px[i], rr.top + this.py[i] - this.scroller.scrollTop, this.tileW, this.tileW * 1.5);
  }

  /** The focus link, moved onto the title (the shell gives it keyboard focus back after an overlay). */
  artOf(id: Id): HTMLElement | null {
    if (this.dom) return this.dom.artOf?.(id) ?? null;
    const i = this.items.findIndex((x) => x.id === id);
    if (i < 0) return null;
    this.placeLink(this.focusLink, i);
    return this.focusLink;
  }

  hide(id: Id | null): void {
    if (this.dom) return this.dom.hide?.(id);
    this.hiddenId = id;
    this.invalidate();
  }

  reveal(id: Id, mark = true): void {
    if (this.dom) return this.dom.reveal?.(id);
    const i = this.items.findIndex((x) => x.id === id);
    if (i < 0) return;
    const y = this.py[i];
    const scroll = this.scroller.scrollTop;
    const visible = y >= scroll + this.ctx.safe.top && y + this.tileH <= scroll + this.viewH - this.ctx.safe.bottom;
    if (!visible) {
      this.quietUntil = performance.now() + 200;
      this.scroller.scrollTop = Math.max(0, y + this.tileH / 2 - this.viewH / 2);
    }
    if (mark) this.setKeyFocus(i, false);
    else {
      this.focusIndex = i;
      this.keyed = false;
      this.placeLink(this.focusLink, i);
    }
    this.invalidate();
  }

  pause(paused: boolean): void {
    if (this.dom) return this.dom.pause?.(paused);
    this.paused = paused;
    if (!paused) this.invalidate();
  }

  // ── teardown ──

  private teardown(): void {
    this.ro.disconnect();
    for (const c of this.cleanup) c();
    this.cleanup = [];
    if (this.raf) cancelAnimationFrame(this.raf);
    this.raf = 0;
    for (const t of [this.idle, this.hoverT, this.pinchT]) if (t) clearTimeout(t);
    this.renderer.destroy();
    this.canvas.remove();
    this.scroller.remove();
    this.tip?.remove();
    this.root.classList.remove(styles.gpu);
  }

  /** The GPU went away (driver reset, GPU process crash): carry on as the DOM Wall. */
  private fallBack(): void {
    if (this.dom || this.destroyed) return;
    this.teardown();
    this.dom = { update: () => undefined, move: () => false, rectOf: () => null, destroy: () => undefined };
    void import('./wall').then(({ wall }) => {
      if (this.destroyed) return;
      this.dom = wall.mount(this.root, this.ctx);
    });
  }

  destroy(): void {
    if (this.destroyed) return;
    this.destroyed = true;
    if (this.dom) {
      this.dom.destroy();
      return;
    }
    this.teardown();
  }
}

/** The Wall on the GPU, once its pipeline compiles here (null: use the DOM Wall). */
export async function gpuWall(gpu: Gpu): Promise<LensModule | null> {
  const [quads, mips] = await Promise.all([prepareQuads(gpu), posterCache(gpu).prepare()]);
  if (!quads || !mips) return null;
  return {
    mount(root, ctx) {
      if (gpu.lost) throw new Error('The GPU device is gone.');
      return new GpuWallView(root, ctx, gpu);
    },
  };
}
