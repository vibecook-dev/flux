/**
 * What both Galleries share (the DOM one in gallery.ts, the WebGPU one in
 * gpu-gallery.ts): how the show is hung, where each piece's frame, mat and label
 * go, and the walk itself (the camera, its glides, input, the tour and reflows).
 * A renderer draws the corridor around `pose` and answers a few hooks.
 */
import { formatCount } from '../../api/format';
import type { Id, ItemSummary } from '../../api/types';
import { clamp, el, lerp } from '../dom';
import { paletteOf } from '../palette';
import { isNew, isWatchedItem, progressFraction } from '../sections';
import type { Dir, LensContext, LensView } from '../types';
import styles from './gallery.module.css';

// World units are CSS px on the picture plane. The eye sits at y = 0 (y grows
// downward) and the corridor runs away from you: d is the distance along it, z = -d.
export const W = 1000; // corridor width
export const FLOOR = 330;
export const CEIL = -370;
export const H = FLOOR - CEIL;
export const STEP = 470; // between consecutive works; they alternate walls
export const ROOM = 760; // the stretch that opens a room: a doorway and the room's title
export const ROOM_SMALL = 420; // a room of one or two works: title on the wall, no doorway
export const END = 560;
export const PORTAL_AT = 230;
export const JAMB = 72;
export const LINTEL = 124;
export const DEPTH = 48; // doorway thickness
export const PW = 780; // a "piece": one wall plane holding the light pool, the frame and the label
export const PH = 660;
export const PY = CEIL + 20;
export const POSTER_H = 316;
export const WIDE_W = 430;
export const MAT = 30;
export const MAT_B = 40; // mats are weighted at the bottom, as framers do
export const FRAME = 7;
export const LABEL_W = 200;
export const FRAME_SHIFT = 78; // frames sit left of the piece centre to leave room for the label
export const AHEAD = 5600;
export const BEHIND = 520;

const WALK = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="13" cy="4.6" r="1.6"/><path d="m9.5 21 2.3-6.2L14.5 17v4M11.8 14.8l.8-5.6 2.8 2.6 2.6.7M12.6 9.2 9.6 10.6 8.5 13.5"/></svg>';

// ── the show (pure, exported for tests) ──

export interface WorkSlot {
  type: 'work';
  key: string;
  item: ItemSummary;
  /** Index in the show (and in the lens's items). */
  i: number;
  side: 1 | -1;
  d0: number;
  d1: number;
  /** Centre of the work's stretch. */
  d: number;
}

export interface RoomSlot {
  type: 'room';
  key: string;
  d0: number;
  d1: number;
  portal: boolean;
  label: string;
  sub: string;
  by: string;
}

export interface EndSlot {
  type: 'end';
  key: string;
  d0: number;
  d1: number;
  count: number;
  /** Titles still arriving (the library is loading). */
  pending: number;
}

export type Slot = WorkSlot | RoomSlot | EndSlot;

export interface Show {
  slots: Slot[];
  works: WorkSlot[];
}

export const plural = (n: number, one: string, many: string) => `${formatCount(n)} ${n === 1 ? one : many}`;

/** What a room holds, like an exhibition room's introduction: "52 films, 1990–1999". */
export function roomByline(items: readonly ItemSummary[]): string {
  let films = 0;
  let shows = 0;
  let other = 0;
  let lo = Infinity;
  let hi = -Infinity;
  for (const it of items) {
    if (it.kind === 'movie') films++;
    else if (it.kind === 'show') shows++;
    else other++;
    if (it.year) {
      lo = Math.min(lo, it.year);
      hi = Math.max(hi, it.year);
    }
  }
  const parts = [films ? plural(films, 'film', 'films') : '', shows ? plural(shows, 'show', 'shows') : '', other ? plural(other, 'title', 'titles') : ''].filter(Boolean);
  const kinds = parts.length > 1 ? `${parts.slice(0, -1).join(', ')} and ${parts[parts.length - 1]}` : (parts[0] ?? '');
  const span = lo === Infinity ? '' : lo === hi ? String(lo) : `${lo}–${hi}`;
  return [kinds, span].filter(Boolean).join(', ');
}

export interface WallLines {
  title: string;
  year: string;
  /** What it is: "Film", "Show · 3 seasons", "Severance · S01E03". */
  what: string;
  /** The museum's medium line, from what flux knows about the file: "4K · HDR10 · Atmos". */
  medium: string;
  status: string;
  tone: 'new' | 'offline' | '';
}

/** The wall label beside a work. */
export function wallLines(item: ItemSummary, now = Date.now()): WallLines {
  let what: string;
  switch (item.kind) {
    case 'show':
      what = item.child_count ? `Show · ${plural(item.child_count, 'season', 'seasons')}` : 'Show';
      break;
    case 'episode':
      what = [item.parent_title, item.subtitle].filter(Boolean).join(' · ') || 'Episode';
      break;
    case 'season':
      what = item.parent_title ? `${item.parent_title} · season` : 'Season';
      break;
    case 'extra':
      what = item.parent_title ? `Extra · ${item.parent_title}` : 'Extra';
      break;
    default:
      what = 'Film';
  }
  const prog = progressFraction(item);
  let status = '';
  let tone: WallLines['tone'] = '';
  if (!item.available) {
    status = 'On loan: its drive is offline';
    tone = 'offline';
  } else if (prog !== null) status = `Partly viewed, ${Math.round(prog * 100)}%`;
  else if (isNew(item, now)) {
    status = 'Recent acquisition';
    tone = 'new';
  } else if (isWatchedItem(item)) status = 'Viewed';
  return { title: item.title, year: item.year ? String(item.year) : '', what, medium: item.badges.slice(0, 4).join(' · '), status, tone };
}

/** Everything a work's planes show, so a re-hang keeps planes whose content didn't change. */
function workKey(it: ItemSummary, d0: number, side: number): string {
  const l = wallLines(it);
  return `w${it.id}@${d0}:${side}|${l.title}|${l.year}|${l.what}|${l.medium}|${l.status}|${it.poster?.url ?? ''}`;
}

/** Hang the show: rooms per section (contiguous runs of `sectionOf`), works alternating walls, then the end wall. */
export function hangShow(items: readonly ItemSummary[], sectionOf: (it: ItemSummary) => string, total = items.length): Show {
  const slots: Slot[] = [];
  const works: WorkSlot[] = [];
  const sections = items.map((it) => sectionOf(it) || '');
  const grouped = sections.some((s) => s !== '');
  const loading = total > items.length;
  let d = 0;
  let room = 0;
  for (let i = 0; i < items.length; ) {
    const g = grouped ? sections[i] : '';
    let end = i + 1;
    while (end < items.length && (grouped ? sections[end] : '') === g) end++;
    room++;
    const n = end - i;
    // Big rooms get a doorway; a room of one or two works is announced on the wall only.
    // The last room of a library still arriving may yet grow: hang it at full length, so
    // nothing already on the walls moves when the next page comes in.
    const portal = n >= 3 || !grouped || (loading && end === items.length);
    const len = portal ? ROOM : ROOM_SMALL;
    const label = grouped ? g : 'The collection';
    const sub = `Room ${room} · ${plural(n, 'work', 'works')}`;
    const by = roomByline(items.slice(i, end));
    slots.push({ type: 'room', key: `r${label}@${d}:${portal ? 1 : 0}|${sub}|${by}`, d0: d, d1: d + len, portal, label, sub, by });
    d += len;
    // Every room starts on the right wall.
    let side: 1 | -1 = 1;
    for (let k = i; k < end; k++) {
      const w: WorkSlot = { type: 'work', key: workKey(items[k], d, side), item: items[k], i: k, side, d0: d, d1: d + STEP, d: d + STEP / 2 };
      slots.push(w);
      works.push(w);
      d += STEP;
      side = side === 1 ? -1 : 1;
    }
    i = end;
  }
  if (!items.length) {
    // An empty doorway. While a library is still arriving it says so; when nothing matches,
    // the shell explains, so the room stays quiet.
    const label = total > 0 ? 'Hanging' : '';
    const by = total > 0 ? 'The collection is still arriving.' : '';
    slots.push({ type: 'room', key: `r-empty|${label}`, d0: 0, d1: ROOM, portal: true, label, sub: total > 0 ? `${plural(total, 'work', 'works')} on the way` : '', by });
    d = ROOM;
  }
  const pending = Math.max(0, total - items.length);
  slots.push({ type: 'end', key: `e@${d}:${items.length}:${pending}`, d0: d, d1: d + END, count: items.length, pending });
  return { slots, works };
}

/** Slots overlapping [lo, hi), by binary search (slots are hung in order along the corridor). */
export function slotRange(slots: readonly Slot[], lo: number, hi: number): [number, number] {
  let a = 0;
  let b = slots.length;
  while (a < b) {
    const m = (a + b) >> 1;
    if (slots[m].d1 > lo) b = m;
    else a = m + 1;
  }
  let end = a;
  while (end < slots.length && slots[end].d0 < hi) end++;
  return [a, end];
}

/** True when `next` only adds titles after `prev` (a page of a loading library arrived). */
export function isGrowth(prev: readonly ItemSummary[], next: readonly ItemSummary[]): boolean {
  if (next.length < prev.length) return false;
  for (let i = 0; i < prev.length; i++) if (prev[i].id !== next[i].id) return false;
  return true;
}

/** Where a piece's framed print and label go, in the piece's own px (0..PW, 0..PH). */
export interface PieceLayout {
  /** A still wider than tall (an episode, a home video): it keeps its width. */
  wide: boolean;
  /** The art. */
  pw: number;
  ph: number;
  /** The frame's box. */
  fl: number;
  ft: number;
  fw: number;
  fh: number;
  /** The label's box: beside the frame, or under it on a phone. */
  lx: number;
  ly: number;
  lw: number;
}

export function pieceLayout(item: ItemSummary, portrait: boolean): PieceLayout {
  const ref = item.poster;
  const ratio = ref?.width && ref.height ? ref.width / ref.height : item.kind === 'episode' ? 16 / 9 : 2 / 3;
  // Posters keep their height; a wide still (an episode, a home video) keeps its width.
  const wide = ratio > 1.05;
  const pw = wide ? WIDE_W : Math.round(POSTER_H * clamp(ratio, 0.6, 0.8));
  const ph = wide ? Math.round(WIDE_W / clamp(ratio, 1.2, 2.4)) : POSTER_H;
  const fw = pw + 2 * MAT + 2 * FRAME;
  const fh = ph + MAT + MAT_B + 2 * FRAME;
  const fx = PW / 2 - FRAME_SHIFT; // frame centre
  const fyc = -14 - PY; // frame centre at eye level
  const fl = Math.round(fx - fw / 2);
  const ft = Math.round(fyc - fh / 2);
  // Beside the frame, as museums hang them; under it on a phone, where the frame fills the view.
  const label = portrait ? { lx: fl, ly: ft + fh + 28, lw: fw + 40 } : { lx: fl + fw + 40, ly: ft + fh - 128, lw: LABEL_W };
  return { wide, pw, ph, fw, fh, fl, ft, ...label };
}

const ease = (x: number) => (x < 0.5 ? 4 * x * x * x : 1 - Math.pow(-2 * x + 2, 3) / 2);

export interface Pose {
  d: number;
  x: number;
  y: number;
  /** Degrees (CSS rotateY). */
  yaw: number;
}

interface CameraRig {
  STAND: number;
  XOFF: number;
  YAW: number;
  VIEWX: number;
}

interface Tween {
  from: Pose;
  to: Pose;
  t0: number;
  dur: number;
  done?: () => void;
}

/**
 * The walk: where the camera is and how it moves. A renderer extends it and draws
 * the corridor at `pose` (the world seen through CSS `perspective: P` and
 * `translateZ(P) rotateY(yaw) translate3d(-x, -y, d)`).
 */
export abstract class GalleryCore implements LensView {
  protected root: HTMLElement;
  protected ctx: LensContext;
  protected mini: boolean;
  /** The haze where the corridor vanishes, and the vignette. */
  protected fog: HTMLDivElement;
  protected items: readonly ItemSummary[];
  protected total: number;
  protected slots: Slot[] = [];
  protected works: WorkSlot[] = [];
  protected pose: Pose = { d: 0, x: 0, y: 0, yaw: 0 };
  /** Position along the show in works (fractional while gliding); -1 is the entrance. */
  protected t = -1;
  protected tv = -1;
  protected mode: 'walk' | 'view' = 'walk';
  protected tween: Tween | null = null;
  protected focusIdx = -2;
  protected raf = 0;
  protected paused = false;
  /** Inside the animation frame: a renderer can draw now rather than ask for a frame. */
  protected ticking = false;
  /** CSS perspective, and the view's size. */
  protected P = 800;
  protected vw = 1200;
  protected vh = 800;
  protected portrait: boolean | null = null;
  protected cleanup: (() => void)[] = [];
  private C: CameraRig = { STAND: 660, XOFF: 150, YAW: 30, VIEWX: 170 };
  private lastInput = 0;
  private phase = 0;
  private last = 0;
  private wheeling = false;
  private snapT: ReturnType<typeof setTimeout> | null = null;
  private touring = false;
  private tourTimer: ReturnType<typeof setInterval> | null = null;
  private tourBtn: HTMLButtonElement | null = null;
  private suppressClick = false;
  private entranceD = -620;
  private ro: ResizeObserver | null = null;
  private stopped = false;

  constructor(root: HTMLElement, ctx: LensContext) {
    this.root = root;
    this.ctx = ctx;
    this.mini = ctx.env.mini;
    this.items = ctx.items;
    this.total = ctx.total;
    root.classList.add(styles.gal);
    root.setAttribute('role', 'region');
    root.setAttribute('aria-label', 'Gallery');
    this.fog = el('div', styles.fog, { 'aria-hidden': 'true' });
  }

  /** After the renderer has built its surface (put in the root before the fog): hang the show and start walking. */
  protected start(): void {
    this.root.append(this.fog);
    this.measure();
    this.hang(this.items);
    const i = this.ctx.focus !== null ? this.works.findIndex((w) => w.item.id === this.ctx.focus) : -1;
    // Start at the focused work, or at the first one; walk back for the entrance view.
    this.t = this.tv = this.mini ? -0.35 : i >= 0 ? i : this.works.length ? 0 : -1;
    this.snapPose();
    this.showChanged(true);
    this.apply();
    this.emitFocus();
    if (!this.mini) {
      this.bind();
      this.buildAccessory();
    }
    this.ro = new ResizeObserver(() => {
      this.measure();
      if (this.mode === 'walk' && !this.tween) this.snapPose();
      this.apply();
    });
    this.ro.observe(this.root);
  }

  protected get rm(): boolean {
    return this.ctx.env.reducedMotion;
  }

  // ── what a renderer does ──

  /** The element that fades for reduced-motion cuts and reflows. */
  protected abstract surface(): HTMLElement;
  /** The view changed size (P, vw, vh are new); `flipped` when it turned between portrait and landscape. */
  protected abstract resized(flipped: boolean): void;
  /** Draw the corridor at `pose` (called for every move). */
  protected abstract render(): void;
  /** The show was hung again (`force`: rebuild what is on the walls). */
  protected abstract showChanged(force: boolean): void;
  /** Work `idx` is the one you stand at now. */
  protected abstract focused(idx: number): void;
  /** The work under a click, or -1. */
  protected abstract workAt(e: MouseEvent): number;
  abstract rectOf(id: Id): DOMRect | null;
  abstract artOf(id: Id): HTMLElement | null;

  // ── camera ──

  private measure(): void {
    const w = this.root.clientWidth || window.innerWidth;
    const h = this.root.clientHeight || window.innerHeight;
    const portrait = h > w * 1.08;
    const flipped = this.portrait !== null && this.portrait !== portrait;
    this.portrait = portrait;
    // Landscape keeps an 80° horizontal view; portrait (phones) keeps a 68° vertical one and
    // turns further toward each work, so the phone becomes a viewfinder.
    this.P = portrait ? h / 2 / Math.tan((34 * Math.PI) / 180) : w / 2 / Math.tan((40 * Math.PI) / 180);
    this.C = portrait ? { STAND: 600, XOFF: 230, YAW: 50, VIEWX: 90 } : { STAND: 660, XOFF: 150, YAW: 30, VIEWX: 170 };
    this.vw = w;
    this.vh = h;
    this.entranceD = -this.C.STAND + 40;
    this.resized(flipped);
  }

  /** Where along the corridor the frame of work w hangs. */
  protected frameD(w: WorkSlot): number {
    return w.side < 0 ? w.d - FRAME_SHIFT : w.d + FRAME_SHIFT;
  }

  private standD(k: number): number {
    const n = this.works.length;
    if (!n || k <= -1) return this.entranceD;
    const i0 = Math.floor(k);
    const i1 = Math.min(i0 + 1, n - 1);
    const a = i0 < 0 ? this.entranceD : this.frameD(this.works[Math.min(i0, n - 1)]) - this.C.STAND;
    const b = this.frameD(this.works[i1]) - this.C.STAND;
    return lerp(a, b, clamp(k - i0, 0, 1));
  }

  /** The resting pose for a position: standing back from the work, turned toward its wall. */
  private restPose(k: number, settle = 1): Pose {
    const n = this.works.length;
    const w = n && k > -0.5 ? this.works[clamp(Math.round(k), 0, n - 1)] : null;
    const s = w ? w.side : 0;
    return { d: this.standD(k), x: -s * this.C.XOFF * settle, y: 0, yaw: s * this.C.YAW * settle };
  }

  private snapPose(): void {
    this.pose = this.restPose(this.t);
  }

  /** The camera moved: keep the haze on the vanishing point, and draw. */
  protected apply(): void {
    const p = this.pose;
    const vpx = 50 - ((this.P * Math.tan((p.yaw * Math.PI) / 180)) / this.vw) * 100;
    this.fog.style.setProperty('--vpx', `${clamp(vpx, -60, 160).toFixed(1)}%`);
    this.fog.style.opacity = String(clamp(1 - Math.abs(p.yaw) / 80, 0.15, 1));
    this.render();
  }

  /** Still moving (a glide, the walk settling, a breath of sway). */
  protected get moving(): boolean {
    return this.raf !== 0;
  }

  protected kick(): void {
    if (this.raf || this.mini || this.paused || this.stopped) return;
    this.last = performance.now();
    this.raf = requestAnimationFrame((t) => this.tick(t));
  }

  private tick(now: number): void {
    this.raf = 0;
    const dt = Math.min(0.05, Math.max(0, (now - this.last) / 1000));
    this.last = now;
    let busy = false;
    if (this.tween) {
      const tw = this.tween;
      const f = tw.dur ? clamp((now - tw.t0) / tw.dur, 0, 1) : 1;
      const e = ease(f);
      this.pose = { d: lerp(tw.from.d, tw.to.d, e), x: lerp(tw.from.x, tw.to.x, e), y: lerp(tw.from.y, tw.to.y, e), yaw: lerp(tw.from.yaw, tw.to.yaw, e) };
      if (f >= 1) {
        this.tween = null;
        tw.done?.();
      } else busy = true;
    } else if (this.mode === 'walk') {
      const k = this.rm ? 1 : 1 - Math.exp(-dt * 6.2);
      const before = this.t;
      this.t += (this.tv - this.t) * k;
      if (Math.abs(this.tv - this.t) < 0.0005) this.t = this.tv;
      const scrolling = now - this.lastInput < 200 && this.wheeling;
      // Turn toward the work you are arriving at; look ahead while you travel.
      const arrive = scrolling ? 0 : clamp(1 - Math.abs(this.tv - this.t) * 1.1, 0, 1);
      const target = this.restPose(this.tv, arrive);
      const k2 = this.rm ? 1 : 1 - Math.exp(-dt * 3.4);
      this.pose.d = this.standD(this.t);
      this.pose.yaw += (target.yaw - this.pose.yaw) * k2;
      this.pose.x += (target.x - this.pose.x) * k2;
      // A breath of sway while walking, never with reduced motion.
      const speed = Math.abs(this.t - before) / Math.max(dt, 0.001);
      if (!this.rm) {
        this.phase += Math.abs(this.t - before) * 5.2;
        const amp = clamp(speed * 1.6, 0, 3.2);
        this.pose.y += (Math.sin(this.phase) * amp - this.pose.y) * 0.2;
      }
      busy = this.t !== this.tv || Math.abs(target.yaw - this.pose.yaw) > 0.02 || Math.abs(target.x - this.pose.x) > 0.05 || Math.abs(this.pose.y) > 0.05 || scrolling;
      if (!busy) this.pose.y = 0;
      this.emitFocus();
    }
    if (busy) this.kick();
    this.ticking = true;
    try {
      this.apply();
    } finally {
      this.ticking = false;
    }
    if (!busy) this.settled();
  }

  /** The camera came to rest (renderers sync what they keep in the page). */
  protected settled(): void {}

  private tweenTo(to: Pose, dur: number, done?: () => void): void {
    this.tween = { from: { ...this.pose }, to, t0: performance.now(), dur: this.rm ? 0 : dur, done };
    if (this.rm) {
      // Reduced motion: cut instead of gliding, with a short fade.
      this.surface().animate([{ opacity: 0.25 }, { opacity: 1 }], { duration: 220, easing: 'ease-out' });
    }
    if (this.paused || this.mini || this.rm) {
      this.tick(performance.now() + 1e6);
      return;
    }
    this.kick();
  }

  private emitFocus(): void {
    const n = this.works.length;
    if (!n) return;
    const idx = clamp(Math.round(this.t), 0, n - 1);
    if (idx === this.focusIdx) return;
    this.focusIdx = idx;
    const it = this.works[idx].item;
    if (!this.mini) {
      this.ctx.setFocus(it.id);
      this.ctx.intent(it);
    }
    this.root.style.setProperty('--gal-tint', paletteOf(it).c1);
    this.focused(idx);
  }

  // ── the show ──

  private hang(items: readonly ItemSummary[]): void {
    this.items = items;
    const show = hangShow(items, (it) => this.ctx.sectionOf(it), this.total);
    this.slots = show.slots;
    this.works = show.works;
  }

  // ── input ──

  protected on<K extends keyof HTMLElementEventMap>(type: K, fn: (e: HTMLElementEventMap[K]) => void, opts?: AddEventListenerOptions): void {
    this.root.addEventListener(type, fn as EventListener, opts);
    this.cleanup.push(() => this.root.removeEventListener(type, fn as EventListener, opts));
  }

  private bind(): void {
    const r = this.root;
    this.on(
      'wheel',
      (e) => {
        e.preventDefault();
        if (e.ctrlKey) return;
        this.touch();
        const unit = e.deltaMode === 1 ? 16 : e.deltaMode === 2 ? window.innerHeight : 1;
        const dy = (Math.abs(e.deltaX) > Math.abs(e.deltaY) ? e.deltaX : e.deltaY) * unit;
        this.wheeling = true;
        this.tv = clamp(this.tv + dy / 360, -1, this.works.length - 1);
        if (this.snapT) clearTimeout(this.snapT);
        this.snapT = setTimeout(() => {
          this.wheeling = false;
          this.tv = clamp(Math.round(this.tv), -1, this.works.length - 1);
          this.kick();
        }, 170);
        this.kick();
      },
      { passive: false },
    );

    let drag: { x: number; y: number; tv: number; moved: boolean; id: number; hist: [number, number][] } | null = null;
    this.on('pointerdown', (e) => {
      if (e.button !== 0 && e.pointerType === 'mouse') return;
      drag = { x: e.clientX, y: e.clientY, tv: this.tv, moved: false, id: e.pointerId, hist: [[performance.now(), 0]] };
    });
    this.on('pointermove', (e) => {
      if (!drag || e.pointerId !== drag.id) return;
      const dx = e.clientX - drag.x;
      const dy = e.clientY - drag.y;
      const along = Math.abs(dx) > Math.abs(dy) ? dx : dy;
      if (!drag.moved && Math.hypot(dx, dy) > 7) {
        drag.moved = true;
        this.touch();
        try {
          r.setPointerCapture(e.pointerId);
        } catch {
          // synthetic or finished pointer
        }
        r.classList.add(styles.dragging);
      }
      if (!drag.moved) return;
      const unit = Math.max(180, Math.min(320, this.vw * 0.22));
      this.tv = clamp(drag.tv - along / unit, -1, this.works.length - 1);
      drag.hist.push([performance.now(), along / unit]);
      if (drag.hist.length > 6) drag.hist.shift();
      this.lastInput = performance.now();
      this.kick();
    });
    const up = (e: PointerEvent) => {
      if (!drag || e.pointerId !== drag.id) return;
      if (drag.moved) {
        const h = drag.hist;
        const [t0, a0] = h[0];
        const [t1, a1] = h[h.length - 1];
        const v = t1 > t0 ? (a1 - a0) / ((t1 - t0) / 1000) : 0; // works per second
        this.tv = clamp(Math.round(this.tv - clamp(v * 0.18, -3, 3)), -1, this.works.length - 1);
        this.suppressClick = true;
        setTimeout(() => (this.suppressClick = false), 0);
        r.classList.remove(styles.dragging);
        this.kick();
      }
      drag = null;
    };
    this.on('pointerup', up);
    this.on('pointercancel', up);

    this.on('click', (e) => {
      if (this.suppressClick) return;
      const i = this.workAt(e);
      if (i >= 0) this.openAt(i);
    });
  }

  /** Being dragged (renderers keep hover effects off meanwhile). */
  protected get dragging(): boolean {
    return this.root.classList.contains(styles.dragging);
  }

  /** Any touch of the controls ends the tour and leaves a close-up. */
  private touch(): void {
    this.lastInput = performance.now();
    if (this.touring) this.stopTour();
    if (this.mode === 'view' || this.tween) {
      this.tween = null;
      this.mode = 'walk';
    }
  }

  protected openAt(i: number): void {
    const w = this.works[i];
    if (!w) return;
    this.touch();
    this.t = this.tv = i;
    this.emitFocus();
    const to: Pose = { d: this.frameD(w), x: -w.side * this.C.VIEWX, y: 0, yaw: w.side * 90 };
    const dist = Math.abs(to.d - this.pose.d) + Math.abs(to.yaw - this.pose.yaw) * 6;
    this.mode = 'view';
    this.tweenTo(to, clamp(560 + dist * 0.28, 640, 1500), () => {
      this.ctx.open(w.item.id, this.artOf(w.item.id));
    });
  }

  private leaveView(): void {
    if (this.mode !== 'view') return;
    this.mode = 'walk';
    this.tweenTo(this.restPose(this.t), 760, () => this.kick());
  }

  move(dir: Dir): boolean {
    const n = this.works.length;
    if (!n) return false;
    this.touch();
    const step = dir === 'right' || dir === 'up' ? 1 : -1;
    this.tv = clamp(Math.round(this.tv) + step, -1, n - 1);
    this.wheeling = false;
    this.kick();
    return true;
  }

  key(e: KeyboardEvent): boolean {
    if (e.key === 'Enter' && this.works.length) {
      this.openAt(clamp(Math.round(this.t), 0, this.works.length - 1));
      return true;
    }
    if (e.key === 'Home') {
      this.touch();
      this.tv = -1;
      this.kick();
      return true;
    }
    if (e.key === 'End') {
      this.touch();
      this.tv = this.works.length - 1;
      this.kick();
      return true;
    }
    return false;
  }

  // ── tour ──

  private buildAccessory(): void {
    const b = el('button', styles.tour, { type: 'button', 'aria-pressed': 'false', title: 'Walk the exhibition on its own' });
    b.innerHTML = WALK;
    b.append(el('span', null, null, 'Tour'));
    b.addEventListener('click', () => (this.touring ? this.stopTour() : this.startTour()));
    this.tourBtn = b;
    this.ctx.setAccessory(b);
  }

  private setTourLabel(on: boolean): void {
    if (!this.tourBtn) return;
    this.tourBtn.setAttribute('aria-pressed', String(on));
    const span = this.tourBtn.querySelector('span');
    if (span) span.textContent = on ? 'Stop tour' : 'Tour';
  }

  private startTour(): void {
    if (!this.works.length) return;
    this.touring = true;
    this.setTourLabel(true);
    const step = () => {
      if (!this.touring || this.paused) return;
      const i = Math.round(this.tv);
      if (i >= this.works.length - 1) {
        // Back to the entrance, through a quick fade.
        this.surface().animate([{ opacity: 1 }, { opacity: 0 }, { opacity: 1 }], { duration: 900 });
        setTimeout(() => {
          this.t = this.tv = -1;
          this.snapPose();
          this.apply();
          this.emitFocus();
        }, 450);
      } else {
        this.tv = i + 1;
        this.wheeling = false;
        this.kick();
      }
    };
    step();
    this.tourTimer = setInterval(step, 4200);
  }

  private stopTour(): void {
    this.touring = false;
    if (this.tourTimer) clearInterval(this.tourTimer);
    this.tourTimer = null;
    this.setTourLabel(false);
  }

  // ── contract ──

  update(items: readonly ItemSummary[], total: number): void {
    const grew = isGrowth(this.items, items);
    this.total = total;
    if (grew) {
      // A page of a loading library: everything hung so far stays where it is.
      const before = this.works.length;
      this.hang(items);
      if (before === 0 && this.works.length && !this.mini && this.t < 0 && this.tv < 0) {
        this.t = this.tv = 0;
        this.snapPose();
      }
      this.showChanged(true);
      this.apply();
      this.emitFocus();
      return;
    }
    const n0 = this.works.length;
    const keep = n0 ? this.works[clamp(Math.round(this.t), 0, n0 - 1)].item.id : null;
    this.tween = null;
    this.mode = 'walk';
    this.hang(items);
    const i = keep !== null ? this.works.findIndex((w) => w.item.id === keep) : -1;
    this.t = this.tv = i >= 0 ? i : this.works.length ? clamp(Math.round(this.t), 0, this.works.length - 1) : -1;
    this.focusIdx = -2;
    this.snapPose();
    this.showChanged(true);
    this.apply();
    this.emitFocus();
    if (!this.rm) this.surface().animate([{ opacity: 0.35 }, { opacity: 1 }], { duration: 320, easing: 'ease-out' });
  }

  reveal(id: Id): void {
    const i = this.works.findIndex((w) => w.item.id === id);
    if (i < 0) return;
    this.touch();
    this.t = this.tv = i;
    this.snapPose();
    this.showChanged(true);
    this.apply();
    this.emitFocus();
  }

  pause(on: boolean): void {
    this.paused = on;
    if (on) {
      cancelAnimationFrame(this.raf);
      this.raf = 0;
      if (this.touring) this.stopTour();
    } else if (this.mode === 'view') this.leaveView();
    else this.kick();
  }

  /** Stop the walk and its listeners (a renderer removes its own surface). */
  protected stop(): void {
    this.stopped = true;
    cancelAnimationFrame(this.raf);
    this.raf = 0;
    this.stopTour();
    if (this.snapT) clearTimeout(this.snapT);
    this.ro?.disconnect();
    for (const c of this.cleanup) c();
    this.cleanup = [];
    this.fog.remove();
  }

  destroy(): void {
    this.stop();
  }
}
