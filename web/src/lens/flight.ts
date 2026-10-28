/**
 * Shared-element flights: a title's poster flies from where one place shows it
 * (a lens, the detail page) to where the next one shows it, so nothing opens out
 * of nowhere and switching lens keeps your place.
 *
 * Seamless means the poster is never on screen twice and never missing:
 *  - a ghost of it flies above everything, showing from its first frame an image
 *    the browser already has (what the lens drew), and the sharper one for where
 *    it lands once that has decoded;
 *  - both ends hide their own copy in the frame the ghost appears;
 *  - the far end shows its copy again before the ghost goes (it fades over it).
 * The ghost maps onto each end's four corners, so a poster seen in perspective
 * (a painting on the Gallery's wall, a cover turned on the Shelf) is matched too.
 */
import { artUrl, artWidthBucket } from '../api/client';
import type { ArtRef, Id, ItemSummary } from '../api/types';
import { averageColor, blurhashDataUrl } from '../components/blurhash';
import { reducedMotion } from './dom';
import styles from './flight.module.css';
import { soft } from './gpu/motion';
import type { LensView, Pt, Quad, Spot } from './types';

export type { Pt, Quad, Spot };

// ── geometry ──

export function rectQuad(r: { left: number; top: number; width: number; height: number }): Quad {
  return [
    { x: r.left, y: r.top },
    { x: r.left + r.width, y: r.top },
    { x: r.left + r.width, y: r.top + r.height },
    { x: r.left, y: r.top + r.height },
  ];
}

/** Corners as the GPU lenses project them (top-left, top-right, bottom-left, bottom-right), in client px. */
export function projectedQuad(c: readonly ({ x: number; y: number } | null)[], dx = 0, dy = 0): Quad | null {
  if (c.length < 4 || c.some((p) => !p)) return null;
  const [tl, tr, bl, br] = c as { x: number; y: number }[];
  const q: Quad = [
    { x: tl.x + dx, y: tl.y + dy },
    { x: tr.x + dx, y: tr.y + dy },
    { x: br.x + dx, y: br.y + dy },
    { x: bl.x + dx, y: bl.y + dy },
  ];
  // Seen from behind (or edge on), it isn't a poster you could see.
  return quadArea(q) > 1 ? q : null;
}

/** Signed area (positive when the corners go clockwise on screen, as a poster facing you does). */
export function quadArea(q: Quad): number {
  let a = 0;
  for (let i = 0; i < 4; i++) {
    const p = q[i];
    const n = q[(i + 1) % 4];
    a += p.x * n.y - n.x * p.y;
  }
  return a / 2;
}

export function quadBox(q: Quad): { x: number; y: number; w: number; h: number } {
  const xs = q.map((p) => p.x);
  const ys = q.map((p) => p.y);
  const x = Math.min(...xs);
  const y = Math.min(...ys);
  return { x, y, w: Math.max(...xs) - x, h: Math.max(...ys) - y };
}

export function quadCentre(q: Quad): Pt {
  return { x: (q[0].x + q[1].x + q[2].x + q[3].x) / 4, y: (q[0].y + q[1].y + q[2].y + q[3].y) / 4 };
}

/** An upright rectangle (within `tol` px), which plain translate and scale can reach. */
export function isFlat(q: Quad, tol = 0.75): boolean {
  return Math.abs(q[0].y - q[1].y) <= tol && Math.abs(q[3].y - q[2].y) <= tol && Math.abs(q[0].x - q[3].x) <= tol && Math.abs(q[1].x - q[2].x) <= tol;
}

export function lerpQuad(a: Quad, b: Quad, t: number): Quad {
  const l = (p: Pt, q: Pt): Pt => ({ x: p.x + (q.x - p.x) * t, y: p.y + (q.y - p.y) * t });
  return [l(a[0], b[0]), l(a[1], b[1]), l(a[2], b[2]), l(a[3], b[3])];
}

/** Whether any of it is in the window. */
export function onScreen(q: Quad, w = window.innerWidth, h = window.innerHeight): boolean {
  const b = quadBox(q);
  return b.w > 0 && b.h > 0 && b.x + b.w > 0 && b.y + b.h > 0 && b.x < w && b.y < h;
}

/**
 * The CSS transform (with `transform-origin: 0 0`) that puts a `w`×`h` box onto
 * quad `q`: a translate and scale for an upright rectangle, else the projective
 * map of the box's corners onto the quad's (Heckbert's square-to-quad).
 */
export function quadTransform(w: number, h: number, q: Quad): string {
  if (isFlat(q)) {
    const b = quadBox(q);
    return `translate(${b.x}px, ${b.y}px) scale(${b.w / w}, ${b.h / h})`;
  }
  const [p0, p1, p2, p3] = q;
  const sx = p0.x - p1.x + p2.x - p3.x;
  const sy = p0.y - p1.y + p2.y - p3.y;
  let a: number;
  let b: number;
  let c: number;
  let d: number;
  let g = 0;
  let k = 0;
  if (Math.abs(sx) < 1e-6 && Math.abs(sy) < 1e-6) {
    a = p1.x - p0.x;
    b = p1.y - p0.y;
    c = p3.x - p0.x;
    d = p3.y - p0.y;
  } else {
    const dx1 = p1.x - p2.x;
    const dx2 = p3.x - p2.x;
    const dy1 = p1.y - p2.y;
    const dy2 = p3.y - p2.y;
    const det = dx1 * dy2 - dx2 * dy1;
    g = (sx * dy2 - dx2 * sy) / det;
    k = (dx1 * sy - sx * dy1) / det;
    a = p1.x - p0.x + g * p1.x;
    b = p1.y - p0.y + g * p1.y;
    c = p3.x - p0.x + k * p3.x;
    d = p3.y - p0.y + k * p3.y;
  }
  const m = [a / w, b / w, 0, g / w, c / h, d / h, 0, k / h, 0, 0, 1, 0, p0.x, p0.y, 0, 1];
  // Full precision: the perspective terms are tiny (CSS reads 1.2e-5 too).
  return `matrix3d(${m.map((v) => +v.toPrecision(12)).join(', ')})`;
}

// ── where a title is ──

/** A title's spot from its element (flat: its box), the image it shows, and its corner radius. */
export function spotOfElement(el: Element, lift = 0.25): Spot | null {
  const r = el.getBoundingClientRect();
  if (!r.width || !r.height) return null;
  const img = el instanceof HTMLImageElement ? el : el.querySelector('img');
  const src = img && img.complete && img.naturalWidth > 0 ? img.currentSrc || img.src : null;
  const radius = parseFloat(getComputedStyle(el).borderTopLeftRadius) || 0;
  return { quad: rectQuad(r), radius, src, lift };
}

/** Where a lens shows a title right now, if any of it is in the window. */
export function spotIn(view: LensView | null | undefined, id: Id): Spot | null {
  if (!view) return null;
  try {
    let s = view.spotOf?.(id) ?? null;
    if (!s) {
      const el = view.artOf?.(id) ?? null;
      const r = view.rectOf(id);
      if (!r || !r.width) return null;
      const fromEl = el ? spotOfElement(el) : null;
      s = { quad: rectQuad(r), radius: fromEl?.radius ?? 6, src: fromEl?.src ?? null, lift: 0.25 };
    }
    return onScreen(s.quad) ? s : null;
  } catch {
    return null;
  }
}

/** Titles hidden in each lens, and how many flights are hiding each (two can overlap: a switch still landing as its title opens). */
const hiddenIn = new WeakMap<LensView, Map<Id, number>>();
const hiddenEls = new WeakMap<HTMLElement, number>();

/**
 * Hide a title in a lens while its ghost flies (the lens's own `hide`, else its
 * element); returns the undo. Counted: it shows again once every flight hiding it
 * has undone (a lens hides one title at a time: the latest still hidden wins).
 */
export function hideIn(view: LensView | null | undefined, id: Id): () => void {
  if (!view) return () => undefined;
  let done = false;
  if (view.hide) {
    let ids = hiddenIn.get(view);
    if (!ids) hiddenIn.set(view, (ids = new Map<Id, number>()));
    const held = ids;
    // Counted, and moved to the end: the latest hidden is the one a lens keeps hidden.
    const count = (held.get(id) ?? 0) + 1;
    held.delete(id);
    held.set(id, count);
    view.hide(id);
    return () => {
      if (done) return;
      done = true;
      const n = (held.get(id) ?? 1) - 1;
      if (n > 0) return void held.set(id, n);
      held.delete(id);
      view.hide?.([...held.keys()].pop() ?? null);
    };
  }
  const el = view.artOf?.(id) ?? null;
  if (!el) return () => undefined;
  hiddenEls.set(el, (hiddenEls.get(el) ?? 0) + 1);
  el.style.visibility = 'hidden';
  return () => {
    if (done) return;
    done = true;
    const n = (hiddenEls.get(el) ?? 1) - 1;
    if (n > 0) return void hiddenEls.set(el, n);
    hiddenEls.delete(el);
    el.style.visibility = '';
  };
}

// ── images ──

/** The URL an `<img>` with art.ts's `srcset` picks at `cssWidth` on this screen. */
export function srcFor(art: ArtRef | null | undefined, cssWidth: number): string | null {
  if (!art) return null;
  const one = artWidthBucket(cssWidth, 1);
  const two = artWidthBucket(cssWidth, 2);
  const dpr = typeof window === 'undefined' ? 1 : window.devicePixelRatio || 1;
  // The smallest candidate dense enough for the screen; past both, the larger.
  return artUrl(art, dpr <= one / cssWidth || one === two ? one : two);
}

const warmed = new Map<string, HTMLImageElement>();

/** Fetch and decode an image ahead of a flight (kept for the next few). Resolves when it can paint at once. */
export function warm(src: string | null | undefined): Promise<void> {
  if (!src) return Promise.resolve();
  let img = warmed.get(src);
  if (!img) {
    img = new Image();
    img.decoding = 'async';
    img.src = src;
    warmed.set(src, img);
    // A handful is plenty: the last few titles you pointed at.
    if (warmed.size > 8) warmed.delete(warmed.keys().next().value as string);
  }
  return img.decode().catch(() => undefined);
}

// ── the flight ──

export interface FlyOptions {
  item: ItemSummary;
  from: Spot;
  /** Where it lands (asked again just before take-off: it may have settled since). */
  to: Spot | (() => Spot | null);
  /** The art for where it lands, sharper than the source's (faded in once decoded). */
  hi?: string | null;
  duration?: number;
  /** Longest wait for the ghost's first image before flying anyway (ms). */
  wait?: number;
  /** Hide both ends' own copies: called in the frame the ghost appears (not at all if it never flies). */
  start?: () => void;
  /** Show the far end's copy again (may wait, e.g. for its image); the ghost fades over it after. Always called once, flown or not, unless cancelled. */
  land?: () => void | Promise<void>;
}

export interface Flight {
  /** Resolves once the ghost is gone (landed, cancelled, or never flew). */
  done: Promise<void>;
  /** Whether the ghost is in the air (it took off and hasn't handed over to the far end). */
  readonly airborne: boolean;
  /** Whether it took off (false while it waits for its image, or if it never flew). */
  readonly started: boolean;
  /** Still on its first course, to where it was sent (not cancelled, not redirected). */
  readonly onCourse: boolean;
  /**
   * Change course mid-air (another lens was picked while it flew): fly on from
   * where it is to `to`, and land there instead (the old landing never runs).
   * False when it isn't in the air.
   */
  retarget(to: Spot, o?: { duration?: number; land?: () => void | Promise<void> }): boolean;
  /** Remove the ghost now; nothing is shown or hidden (the caller tidies its ends). */
  cancel(): void;
}

const STEPS_PER_SECOND = 60;
const SOFT_CSS = 'cubic-bezier(.25,.85,.3,1)';

/** Keyframes that take a `w`×`h` box from quad `a` to quad `b`. */
function frames(w: number, h: number, a: Quad, b: Quad, ms: number): { kf: Keyframe[]; easing: string } {
  if (isFlat(a) && isFlat(b)) {
    // An upright rectangle all the way: two frames, eased on the compositor.
    return { kf: [{ transform: quadTransform(w, h, a) }, { transform: quadTransform(w, h, b) }], easing: SOFT_CSS };
  }
  // In perspective: the corners move along straight lines, each frame mapped exactly.
  const n = Math.max(8, Math.round((ms / 1000) * STEPS_PER_SECOND));
  const kf: Keyframe[] = [];
  for (let i = 0; i <= n; i++) {
    const t = i / n;
    kf.push({ offset: t, transform: quadTransform(w, h, lerpQuad(a, b, soft(t))) });
  }
  return { kf, easing: 'linear' };
}

const nextFrame = () => new Promise<void>((r) => requestAnimationFrame(() => r()));
const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

function image(src: string, cls: string): HTMLImageElement {
  const img = new Image();
  img.decoding = 'async';
  img.alt = '';
  img.draggable = false;
  img.className = cls;
  img.src = src;
  return img;
}

/** The longer of a quad's top and bottom edges, and of its sides: its size as a flat box. */
export function quadSize(q: Quad): { w: number; h: number } {
  if (isFlat(q)) {
    const b = quadBox(q);
    return { w: b.w, h: b.h };
  }
  const d = (p: Pt, r: Pt) => Math.hypot(r.x - p.x, r.y - p.y);
  return { w: Math.max(d(q[0], q[1]), d(q[3], q[2])), h: Math.max(d(q[0], q[3]), d(q[1], q[2])) };
}

/** The art's own size at a spot: its box when upright, else its longer side and its shape (a poster's by default). */
function artSize(s: Spot): { w: number; h: number } {
  const q = quadSize(s.quad);
  if (isFlat(s.quad)) return q;
  const a = s.aspect ?? 2 / 3;
  const h = Math.max(q.h, q.w / a);
  return { w: h * a, h };
}

/** Whether two quads are the same to within a pixel. */
function sameQuad(a: Quad, b: Quad): boolean {
  return a.every((p, i) => Math.abs(p.x - b[i].x) < 1 && Math.abs(p.y - b[i].y) < 1);
}

class GhostFlight implements Flight {
  readonly done: Promise<void>;
  private ghost: HTMLDivElement | null = null;
  /** The ghost's face (it clips the images), and what it turns into where it lands (a spine), if anything. */
  private faceEl: HTMLDivElement | null = null;
  private into: HTMLCanvasElement | null = null;
  private anim: Animation | null = null;
  private box = { w: 0, h: 0 };
  private toQuad: Quad | null = null;
  /** Where it lands, asked again at the end of the leg (it may have moved); none once it changes course. */
  private follow: (() => Spot | null) | null = null;
  private landing: (() => void | Promise<void>) | undefined;
  /** Bumps when the flight changes course: a landing armed before it doesn't run. */
  private leg = 0;
  /** Course corrections for a landing spot that moved while it flew (the page laid out again). */
  private corrections = 0;
  private tookOff = false;
  private inAir = false;
  private cancelled = false;
  private ended = false;
  private resolve!: () => void;
  private o: FlyOptions;

  constructor(o: FlyOptions) {
    this.o = o;
    this.landing = o.land;
    this.done = new Promise<void>((r) => (this.resolve = r));
    this.run().catch(() => this.fail());
  }

  get airborne(): boolean {
    return this.inAir;
  }

  get started(): boolean {
    return this.tookOff;
  }

  get onCourse(): boolean {
    return !this.cancelled && this.leg === 0;
  }

  private finish(): void {
    if (this.ended) return;
    this.ended = true;
    this.inAir = false;
    this.resolve();
  }

  /** Something went wrong: no ghost, and the far end shows its own copy (unless cancelled). */
  private fail(): void {
    this.anim?.cancel();
    this.ghost?.remove();
    this.ghost = null;
    if (this.cancelled) return this.finish();
    const land = this.landing;
    this.landing = undefined;
    Promise.resolve()
      .then(() => land?.())
      .catch(() => undefined)
      .finally(() => this.finish());
  }

  private async run(): Promise<void> {
    const o = this.o;
    if (reducedMotion()) return this.ground();
    const art = o.item.poster;
    const lo = o.from.src ?? null;
    const hi = o.hi && o.hi !== lo ? o.hi : null;
    // The first image: what the source shows (the browser has it), else the sharp one.
    const first = lo ? image(lo, styles.img) : hi ? image(hi, styles.img) : null;
    const sharp = lo && hi ? image(hi, `${styles.img} ${styles.hi}`) : null;
    if (first) await Promise.race([first.decode().catch(() => undefined), sleep(o.wait ?? 90)]);
    if (this.cancelled) return;
    const to = typeof o.to === 'function' ? o.to() : o.to;
    if (!to || !onScreen(o.from.quad) || !onScreen(to.quad)) return this.ground();
    // The ghost is shaped like the poster: as it is where it lands (a quad in perspective
    // is a poster seen at an angle: its own shape, not the quad's), or where it starts when
    // it lands on something else (a spine); as big as the larger end, so it rasterises sharp.
    const shape = to.face ? o.from : to;
    const ss = artSize(shape);
    const k = Math.max(1, artSize(shape === to ? o.from : to).w / Math.max(1, ss.w));
    this.box = { w: Math.round(ss.w * k), h: Math.round(ss.h * k) };
    this.toQuad = to.quad;
    this.follow = typeof o.to === 'function' ? o.to : null;

    const g = document.createElement('div');
    g.className = styles.ghost;
    g.setAttribute('aria-hidden', 'true');
    g.style.width = `${this.box.w}px`;
    g.style.height = `${this.box.h}px`;
    const face = document.createElement('div');
    face.className = styles.face;
    face.style.borderRadius = `${shape.radius * k}px`;
    const hash = art?.blurhash;
    const avg = hash ? averageColor(hash) : null;
    if (avg) face.style.backgroundColor = avg;
    const ph = hash ? blurhashDataUrl(hash, 8, 12) : null;
    if (ph) face.style.backgroundImage = `url(${ph})`;
    // Starting from something that isn't the poster (a spine), the poster comes in over it.
    const turn = o.from.face ?? null;
    const layer = turn ? document.createElement('div') : face;
    if (turn) {
      turn.className = styles.turn;
      layer.className = styles.layer;
      face.append(turn, layer);
    }
    if (first) layer.append(first);
    if (sharp) {
      layer.append(sharp);
      sharp.decode().then(
        () => sharp.classList.add(styles.on),
        () => undefined,
      );
    }
    // Landing on something that isn't the poster (a tape's spine), it turns into that on the way.
    const into = to.face ?? null;
    if (into) {
      into.className = styles.turn;
      face.append(into);
    }
    this.faceEl = face;
    this.into = into;
    const shade = document.createElement('div');
    shade.className = styles.shade;
    shade.style.borderRadius = face.style.borderRadius;
    g.append(shade, face);

    const ms = o.duration ?? 560;
    const { kf, easing } = frames(this.box.w, this.box.h, o.from.quad, to.quad, ms);
    g.style.transform = String(kf[0].transform);
    document.body.append(g);
    this.ghost = g;
    this.tookOff = true;
    this.inAir = true;
    // Both ends hide in this frame, as the ghost appears over them.
    o.start?.();
    this.anim = g.animate(kf, { duration: ms, easing, fill: 'forwards' });
    // The spine turns into the poster early on (by the time the shape has mostly changed).
    if (turn) layer.animate([{ opacity: 0 }, { opacity: 0, offset: 0.03 }, { opacity: 1, offset: 0.3 }, { opacity: 1 }], { duration: ms, fill: 'forwards' });
    if (into) into.animate([{ opacity: 0 }, { opacity: 0, offset: 0.22 }, { opacity: 1, offset: 0.62 }, { opacity: 1 }], { duration: ms, fill: 'forwards' });
    shade.animate([{ opacity: o.from.lift ?? 0.25 }, { opacity: 1, offset: 0.45 }, { opacity: to.lift ?? 0.25 }], { duration: ms, easing: 'ease-in-out', fill: 'forwards' });
    void this.arrive(this.leg);
  }

  /** It never flew: the far end shows its own copy. */
  private async ground(): Promise<void> {
    try {
      if (!this.cancelled) await this.landing?.();
    } finally {
      this.finish();
    }
  }

  /**
   * At the end of the current leg: if where it lands has moved meanwhile (the page laid
   * out again), a short correction first; then the far end shows its copy, and the ghost
   * fades over it. It counts as in the air until then (closing now can still turn it round).
   */
  private async arrive(leg: number): Promise<void> {
    const a = this.anim;
    if (!a) return;
    try {
      await a.finished;
    } catch {
      // cancelled, or the course changed
      return;
    }
    if (this.cancelled || leg !== this.leg) return;
    const follow = this.follow;
    if (follow && this.toQuad && this.corrections < 2) {
      const now = follow();
      if (now && !sameQuad(now.quad, this.toQuad) && onScreen(now.quad)) {
        this.corrections++;
        this.steer(now, 200, this.landing);
        this.follow = follow;
        return;
      }
    }
    try {
      await this.landing?.();
      // The far end draws its copy in the next frame; the ghost fades once it has.
      await nextFrame();
      await nextFrame();
      const g = this.ghost;
      if (this.cancelled || leg !== this.leg || !g) return;
      this.inAir = false;
      await g
        .animate([{ opacity: 1 }, { opacity: 0 }], { duration: 140, easing: 'ease-out', fill: 'forwards' })
        .finished.catch(() => undefined);
    } finally {
      // Landed (or the landing failed): no ghost left behind either way, unless it changed course.
      if (leg === this.leg) {
        this.ghost?.remove();
        this.ghost = null;
        this.finish();
      }
    }
  }

  retarget(to: Spot, o: { duration?: number; land?: () => void | Promise<void> } = {}): boolean {
    return this.steer(to, o.duration ?? 520, o.land);
  }

  /** A new leg from where the ghost is now to `to` (a new course: what it followed before is dropped). */
  private steer(to: Spot, ms: number, land: (() => void | Promise<void>) | undefined): boolean {
    const g = this.ghost;
    if (!g || !this.inAir || this.cancelled || !onScreen(to.quad)) return false;
    // Where it is this frame (the running animation's value), as the new leg's first frame.
    const now = getComputedStyle(g).transform;
    this.leg++;
    this.anim?.cancel();
    this.landing = land;
    this.toQuad = to.quad;
    this.follow = null;
    const end = quadTransform(this.box.w, this.box.h, to.quad);
    g.style.transform = now;
    this.anim = g.animate([{ transform: now }, { transform: end }], { duration: ms, easing: SOFT_CSS, fill: 'forwards' });
    // What it was turning into goes; what it lands on now (a spine) comes in on the way.
    const was = this.into;
    const next = to.face ?? null;
    if (was && was !== next) void was.animate([{ opacity: getComputedStyle(was).opacity }, { opacity: 0 }], { duration: 180, fill: 'forwards' }).finished.then(() => was.remove(), () => undefined);
    this.into = next;
    if (next && next !== was && this.faceEl) {
      next.className = styles.turn;
      this.faceEl.append(next);
      next.animate([{ opacity: 0 }, { opacity: 0, offset: 0.22 }, { opacity: 1, offset: 0.62 }, { opacity: 1 }], { duration: ms, fill: 'forwards' });
    }
    void this.arrive(this.leg);
    return true;
  }

  cancel(): void {
    if (this.cancelled) return;
    this.cancelled = true;
    this.anim?.cancel();
    this.ghost?.remove();
    this.ghost = null;
    this.finish();
  }
}

/**
 * Fly a ghost of `item`'s poster from one spot to another. It waits (briefly) for
 * an image it can show at once, takes off in one frame with both ends hidden
 * (`start`), and once the far end is showing its own copy again (`land`), fades.
 */
export function fly(o: FlyOptions): Flight {
  return new GhostFlight(o);
}

// ── into the detail page ──

interface Takeoff {
  item: ItemSummary;
  from: Spot;
  view: LensView | null;
  at: number;
}

const takeoffs = new Map<Id, Takeoff>();

/** A lens opens a title: remember where its poster is, so the detail page can fly it from there. */
export function takeoff(item: ItemSummary, from: Spot | null, view: LensView | null): void {
  takeoffs.clear();
  if (!from) return;
  takeoffs.set(item.id, { item, from, view, at: performance.now() });
  void warm(from.src);
}

/**
 * The detail page picks it up, if it is fresh. It stays until the poster lands
 * (`landed`): a page mounted twice (React's strict mode) flies it the second time.
 */
export function takeTakeoff(id: Id): Takeoff | null {
  const t = takeoffs.get(id);
  if (!t || performance.now() - t.at > 3000) return null;
  return t;
}

/** The poster from this take-off has landed: it isn't flown again. */
export function landed(id: Id): void {
  takeoffs.delete(id);
}
