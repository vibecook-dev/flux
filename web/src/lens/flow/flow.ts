/**
 * Flow: Cover Flow for a film and TV library. One poster faces you; the rest
 * stand in two receding stacks you flick through. The room takes its light from
 * the poster in front, and the floor carries soft reflections.
 *
 * Only the covers near the position are in the DOM (enough to reach past the
 * window's edges), so a library of 13,000 titles flicks as smoothly as one of
 * fifty, and pages that arrive while you browse slide into place.
 */
import { formatCount } from '../../api/format';
import type { Id, ItemSummary } from '../../api/types';
import { artElement, artLabel, resizeArt } from '../art';
import { clamp, el } from '../dom';
import type { Dir, LensContext, LensModule, LensView } from '../types';
import styles from './flow.module.css';

/** How far the stacked covers turn away, in degrees. */
export const ANGLE = 70;
/** Spring stiffness and damping: a whisker under critical, so it settles with a breath of overshoot. */
const K = 230;
const DAMP = 2 * Math.sqrt(K) * 0.9;
const H_STEP = 1 / 240;

/** Covers swing away quickly as they leave the centre, like the original. */
const ease = (t: number) => t * (2 - t);

export interface Metrics {
  W: number;
  H: number;
  /** Cover height and width. */
  ch: number;
  cw: number;
  /** y of the floor line (the covers' bottom edge). */
  floor: number;
  /** Distance from the centre to the first cover of a stack. */
  gap1: number;
  /** Distance between stacked covers. */
  spacing: number;
  /** How far the stacks sit behind the centre. */
  depth: number;
  /** How far the centre cover comes forward. */
  lift: number;
  persp: number;
  /** Covers rendered on each side of the position. */
  R: number;
  /** Drag distance per cover. */
  unit: number;
  reflGap: number;
}

export interface Pose {
  x: number;
  z: number;
  ry: number;
}

/** Where a cover stands when it is `d` places from the position (negative = left stack). */
export function pose(d: number, m: Pick<Metrics, 'gap1' | 'spacing' | 'depth' | 'lift'>): Pose {
  const ad = Math.abs(d);
  const s = d < 0 ? -1 : 1;
  if (ad < 1) {
    const t = ease(ad);
    return { x: s * t * m.gap1, z: (1 - t) * m.lift - t * m.depth, ry: -s * t * ANGLE };
  }
  return { x: s * (m.gap1 + (ad - 1) * m.spacing), z: -m.depth, ry: -s * ANGLE };
}

/** The window of indices to render around a position. */
export function windowAround(pos: number, n: number, R: number): [number, number] {
  if (n <= 0) return [0, -1];
  return [Math.max(0, Math.floor(pos) - R), Math.min(n - 1, Math.ceil(pos) + R)];
}

/** The quiet line under the title: year, seasons or episode code, and the first badges. */
export function metaLine(it: ItemSummary): string {
  const parts: string[] = [];
  if (it.kind === 'episode' && it.subtitle) parts.push(it.subtitle);
  else if (it.year) parts.push(String(it.year));
  if (it.kind === 'show' && it.child_count) parts.push(`${it.child_count} season${it.child_count === 1 ? '' : 's'}`);
  parts.push(...it.badges.slice(0, 2));
  if (!it.available) parts.push('offline');
  return parts.join(' · ');
}

export function titleLine(it: ItemSummary): string {
  return it.kind === 'episode' && it.parent_title ? `${it.parent_title}: ${it.title}` : it.title;
}

/** Fit the stage to the window: cover size, the floor, the stacks and how many covers to draw. */
export function measure(W: number, H: number, safe: { top: number; bottom: number }, phone: boolean): Metrics {
  const avail = Math.max(120, H - safe.top - safe.bottom);
  let ch = Math.min(H * 0.54, 500, avail * 0.6, W * (phone ? 0.5 : 0.4) * 1.5);
  ch = Math.max(ch, 90);
  const cw = (ch * 2) / 3;
  const titleH = 60;
  const reflGap = ch * 0.25;
  const block = ch + reflGap + titleH;
  const top = safe.top + Math.max(0, (avail - block) / 2);
  const floor = top + ch;
  const gap1 = cw * (phone ? 0.94 : 1.1);
  const spacing = cw * (phone ? 0.11 : 0.19);
  const persp = Math.max(phone ? 640 : 820, cw * 3.2);
  // Enough covers to reach past the edges of the window, and no more.
  const reach = Math.ceil((W / 2 - gap1 * 0.7) / (spacing * 0.7)) + 2;
  return { W, H, ch, cw, floor, gap1, spacing, depth: cw * 0.95, lift: cw * 0.06, persp, R: clamp(reach, 4, 14), unit: cw * 0.42, reflGap };
}

const CHEVRON = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M14.5 5.5 8 12l6.5 6.5"/></svg>';

interface Cover {
  id: Id;
  el: HTMLDivElement;
  art: HTMLDivElement;
  sheen: HTMLSpanElement;
  /** Visual index (springs toward the real index when results change). */
  vi: number;
  vv: number;
  /** Last written stacking, shade and opacity: style is only touched when they change. */
  z: number;
  sh: number;
  op: string;
}

interface Down {
  id: number;
  x: number;
  pos: number;
  cover: HTMLElement | null;
  samples: { t: number; p: number }[];
}

class FlowView implements LensView {
  private root: HTMLElement;
  private ctx: LensContext;
  private mini: boolean;
  private reduced: boolean;
  private items: readonly ItemSummary[];
  private index = new Map<Id, number>();
  private covers = new Map<Id, Cover>();
  private pos = 0;
  private vel = 0;
  private target = 0;
  /** The index in front (−1 when empty). */
  private ci = -1;
  private tilt = { x: 0, y: 0, tx: 0, ty: 0 };
  private raf = 0;
  private last = 0;
  private paused = false;
  private dragging = false;
  private m: Metrics;
  private stage: HTMLDivElement;
  private contact: HTMLDivElement;
  private title: HTMLDivElement | null = null;
  private tA: HTMLDivElement | null = null;
  private tB: HTMLDivElement | null = null;
  private front: HTMLDivElement | null = null;
  private track: HTMLDivElement | null = null;
  private trackRo: ResizeObserver | null = null;
  /** The track's width, measured on resize (reading it every frame forced a layout each frame). */
  private trackW = 140;
  /** The title the slider last announced (its ARIA values change only when that does). */
  private scrubAt = -2;
  private thumb: HTMLSpanElement | null = null;
  private count: HTMLSpanElement | null = null;
  private intentT: ReturnType<typeof setTimeout> | null = null;
  private snapT: ReturnType<typeof setTimeout> | null = null;
  private ro: ResizeObserver;
  private cleanup: (() => void)[] = [];

  constructor(root: HTMLElement, ctx: LensContext) {
    this.root = root;
    this.ctx = ctx;
    this.mini = ctx.env.mini;
    this.reduced = ctx.env.reducedMotion;
    this.items = ctx.items;
    root.classList.add(styles.flow);
    if (this.mini) root.classList.add(styles.mini);
    root.setAttribute('role', 'listbox');
    root.setAttribute('aria-roledescription', 'cover flow');
    root.setAttribute('aria-label', 'Library');
    this.stage = el('div', styles.stage);
    const floor = el('div', styles.floor, { 'aria-hidden': 'true' });
    this.contact = el('div', styles.contact, { 'aria-hidden': 'true' });
    const mist = el('div', styles.mist, { 'aria-hidden': 'true' });
    this.stage.append(floor, this.contact, mist);
    root.append(this.stage);

    this.title = el('div', styles.title, { 'aria-live': this.mini ? null : 'polite' });
    this.tA = el('div', styles.t);
    this.tB = el('div', styles.t);
    this.title.append(this.tA, this.tB);
    root.append(this.title);
    if (!this.mini) {
      this.bind();
      this.buildAccessory();
    }

    this.reindex();
    const fi = ctx.focus !== null ? this.index.get(ctx.focus) : undefined;
    // A preview with nothing focused opens a few covers in, so both stacks show.
    this.pos = this.target = fi ?? (this.mini ? Math.min(Math.max(0, this.items.length - 1), 6) : 0);
    this.m = this.measure();
    this.render();
    this.centre(true);
    this.syncScrub();
    this.ro = new ResizeObserver(() => {
      const cw = this.m.cw;
      this.m = this.measure();
      if (Math.abs(cw - this.m.cw) > 1) for (const c of this.covers.values()) resizeArt(c.art, this.m.cw);
      this.render();
      this.syncScrub();
    });
    this.ro.observe(root);
  }

  private reindex(): void {
    const map = new Map<Id, number>();
    this.items.forEach((it, i) => map.set(it.id, i));
    this.index = map;
  }

  // ── geometry ──

  private measure(): Metrics {
    const m = measure(this.root.clientWidth || window.innerWidth, this.root.clientHeight || window.innerHeight, this.ctx.safe, this.ctx.env.phone);
    const s = this.root.style;
    s.setProperty('--cw', `${m.cw.toFixed(1)}px`);
    s.setProperty('--ch', `${m.ch.toFixed(1)}px`);
    s.setProperty('--floor', `${m.floor.toFixed(1)}px`);
    s.setProperty('--title-y', `${(m.floor + m.reflGap).toFixed(1)}px`);
    this.stage.style.perspective = `${m.persp.toFixed(0)}px`;
    this.stage.style.perspectiveOrigin = `50% ${(m.floor - m.ch / 2).toFixed(1)}px`;
    return m;
  }

  // ── drawing ──

  private make(it: ItemSummary, i: number, fadeIn: boolean): Cover {
    const node = el('div', styles.cover, { role: 'option', 'aria-label': artLabel(it), 'aria-selected': 'false', 'data-id': it.id });
    // Mid-flick (or far out in the stacks) the poster's average colour is placeholder enough; it costs nothing to work out.
    const near = Math.abs(i - this.pos) < 4 && Math.abs(this.vel) < 6;
    const art = artElement(it, { width: this.m.cw, eager: Math.abs(i - this.pos) < 3, placeholder: near ? 'blur' : 'flat', className: styles.art });
    const sheen = el('span', styles.sheen, { 'aria-hidden': 'true' });
    node.append(art, sheen);
    this.stage.append(node);
    const c: Cover = { id: it.id, el: node, art, sheen, vi: i, vv: 0, z: NaN, sh: NaN, op: '' };
    this.covers.set(it.id, c);
    if (i === this.ci) {
      node.classList.add(styles.centre);
      node.setAttribute('aria-selected', 'true');
    }
    if (fadeIn && !this.reduced) node.animate([{ opacity: 0 }, { opacity: 1 }], { duration: 360, easing: 'ease-out' });
    return c;
  }

  private render(fadeIn = false): void {
    const n = this.items.length;
    const [lo, hi] = windowAround(this.pos, n, this.m.R);
    for (const [id, c] of this.covers) {
      const i = this.index.get(id);
      if (i === undefined || i < lo || i > hi) {
        c.el.remove();
        this.covers.delete(id);
      }
    }
    for (let i = lo; i <= hi; i++) {
      const it = this.items[i];
      const c = this.covers.get(it.id) ?? this.make(it, i, fadeIn);
      this.place(c);
    }
    // A soft contact shadow under the poster in front, fading while it is between two.
    const off = Math.abs(this.pos - Math.round(this.pos));
    this.contact.style.opacity = n ? String(clamp(1 - off * 2.4, 0, 1)) : '0';
  }

  private place(c: Cover): void {
    const d = c.vi - this.pos;
    const ad = Math.abs(d);
    const p = pose(d, this.m);
    let ry = p.ry;
    let rx = 0;
    if (ad < 0.5 && !this.dragging) {
      ry += this.tilt.y * (1 - ad * 2);
      rx = this.tilt.x * (1 - ad * 2);
    }
    const st = c.el.style;
    st.transform = `translate3d(${p.x.toFixed(2)}px,0,${p.z.toFixed(2)}px) rotateY(${ry.toFixed(2)}deg) rotateX(${rx.toFixed(2)}deg)`;
    // Stacking only needs the order (eighth steps keep the crossing in the middle right).
    const z = 1000 - Math.round(ad * 8);
    if (z !== c.z) {
      c.z = z;
      st.zIndex = String(z);
    }
    const sh = Math.round(Math.min(0.62, ad * 0.36) * 100) / 100;
    if (sh !== c.sh) {
      c.sh = sh;
      st.setProperty('--sh', String(sh));
    }
    const R = this.m.R;
    const op = ad > R - 1.5 ? clamp(R - 0.5 - ad, 0, 1).toFixed(2) : '';
    if (op !== c.op) {
      c.op = op;
      st.opacity = op;
    }
  }

  // ── motion ──

  private kick(): void {
    const n = this.items.length;
    if (this.reduced) {
      if (!this.dragging) {
        this.pos = this.target = Math.round(clamp(this.target, 0, Math.max(0, n - 1)));
        this.vel = 0;
      }
      for (const c of this.covers.values()) {
        c.vi = this.index.get(c.id) ?? c.vi;
        c.vv = 0;
      }
      this.render();
      this.centre();
      this.syncScrub();
      return;
    }
    if (this.raf || this.paused) return;
    this.last = performance.now();
    this.raf = requestAnimationFrame(this.tick);
  }

  private tick = (now: number): void => {
    this.raf = 0;
    const dt = Math.min(0.05, Math.max(0.001, (now - this.last) / 1000));
    this.last = now;
    let moving = this.dragging;
    for (let t = 0; t < dt - 1e-9; t += H_STEP) {
      const h = Math.min(H_STEP, dt - t);
      if (!this.dragging) {
        const a = K * (this.target - this.pos) - DAMP * this.vel;
        this.vel += a * h;
        this.pos += this.vel * h;
      }
      for (const c of this.covers.values()) {
        const i = this.index.get(c.id);
        if (i === undefined || (c.vi === i && c.vv === 0)) continue;
        const a = K * (i - c.vi) - DAMP * c.vv;
        c.vv += a * h;
        c.vi += c.vv * h;
      }
    }
    if (!this.dragging) {
      if (Math.abs(this.target - this.pos) < 0.0006 && Math.abs(this.vel) < 0.002) {
        this.pos = this.target;
        this.vel = 0;
      } else moving = true;
    }
    for (const c of this.covers.values()) {
      const i = this.index.get(c.id);
      if (i === undefined) continue;
      if (Math.abs(i - c.vi) < 0.0006 && Math.abs(c.vv) < 0.002) {
        c.vi = i;
        c.vv = 0;
      } else moving = true;
    }
    const tl = this.tilt;
    tl.x += (tl.tx - tl.x) * Math.min(1, dt * 10);
    tl.y += (tl.ty - tl.y) * Math.min(1, dt * 10);
    if (Math.abs(tl.tx - tl.x) > 0.02 || Math.abs(tl.ty - tl.y) > 0.02) moving = true;
    else {
      tl.x = tl.tx;
      tl.y = tl.ty;
    }
    this.render();
    this.centre();
    this.syncScrub();
    if (moving && !this.paused) this.raf = requestAnimationFrame(this.tick);
  };

  private go(i: number): void {
    const n = this.items.length;
    if (!n) return;
    this.target = clamp(Math.round(i), 0, n - 1);
    // A jump across the library lands near its target instead of sweeping past thousands of covers.
    if (Math.abs(this.target - this.pos) > 40) {
      this.pos = this.target - Math.sign(this.target - this.pos) * 6;
      this.vel = 0;
      for (const c of this.covers.values()) {
        c.vi = this.index.get(c.id) ?? c.vi;
        c.vv = 0;
      }
    }
    this.kick();
  }

  /** Name the poster in front, light the room with it and tell the shell. */
  private centre(force = false): void {
    const n = this.items.length;
    if (!n) {
      this.ci = -1;
      if (this.title) this.title.hidden = true;
      return;
    }
    const ci = clamp(Math.round(this.pos), 0, n - 1);
    if (ci === this.ci && !force) return;
    const prev = this.ci;
    this.ci = ci;
    const it = this.items[ci];
    for (const c of this.covers.values()) {
      const on = c.id === it.id;
      c.el.setAttribute('aria-selected', String(on));
      c.el.classList.toggle(styles.centre, on);
    }
    if (!this.mini) {
      this.ctx.setFocus(it.id);
      // Warm the detail only once you stop on a poster, not for every one you flick past.
      if (this.intentT) clearTimeout(this.intentT);
      this.intentT = setTimeout(() => {
        const cur = this.items[this.ci];
        if (cur) this.ctx.intent(cur);
      }, 180);
    }
    if (!this.title || !this.tA || !this.tB) return;
    this.title.hidden = false;
    const body = [el('b', null, null, titleLine(it)), el('small', 'mono', null, metaLine(it))];
    if (force || prev < 0 || !this.front) {
      this.tA.replaceChildren(...body);
      this.tA.classList.add(styles.on);
      this.tB.classList.remove(styles.on);
      this.front = this.tA;
      return;
    }
    const next = this.front === this.tA ? this.tB : this.tA;
    next.replaceChildren(...body);
    next.classList.add(styles.on);
    this.front.classList.remove(styles.on);
    this.front = next;
  }

  // ── input ──

  private on<K extends keyof HTMLElementEventMap>(t: HTMLElement, type: K, fn: (e: HTMLElementEventMap[K]) => void, opts?: AddEventListenerOptions): void {
    t.addEventListener(type, fn as EventListener, opts);
    this.cleanup.push(() => t.removeEventListener(type, fn as EventListener, opts));
  }

  private bind(): void {
    const root = this.root;
    this.on(
      root,
      'wheel',
      (e) => {
        e.preventDefault();
        if (e.ctrlKey || !this.items.length) return;
        const horiz = Math.abs(e.deltaX) > Math.abs(e.deltaY);
        let d = horiz ? e.deltaX : e.deltaY;
        if (e.deltaMode === 1) d *= 32;
        else if (e.deltaMode === 2) d *= 600;
        // A mouse wheel clicks in whole notches: one notch, one poster.
        const notch = e.deltaMode !== 0 || (!horiz && Math.abs(d) >= 60 && Math.abs(e.deltaX) < 0.5 && Number.isInteger(e.deltaY));
        const n = this.items.length;
        if (notch) {
          this.target = clamp(Math.round(this.target) + Math.sign(d), 0, n - 1);
        } else {
          this.target = clamp(this.target + d / 70, -0.45, n - 1 + 0.45);
          if (this.snapT) clearTimeout(this.snapT);
          this.snapT = setTimeout(() => this.go(this.target + this.vel * 0.06), 110);
        }
        this.kick();
      },
      { passive: false },
    );

    let down: Down | null = null;
    this.on(root, 'pointerdown', (e) => {
      if (e.button !== 0 || !this.items.length) return;
      const target = e.target as HTMLElement;
      down = { id: e.pointerId, x: e.clientX, pos: this.pos, cover: target.closest<HTMLElement>(`.${styles.cover}`), samples: [{ t: e.timeStamp, p: this.pos }] };
      root.setPointerCapture(e.pointerId);
    });
    this.on(root, 'pointermove', (e) => {
      if (down && e.pointerId === down.id) {
        const dx = e.clientX - down.x;
        if (!this.dragging && Math.abs(dx) > 6) {
          this.dragging = true;
          root.classList.add(styles.dragging);
          this.tilt.tx = this.tilt.ty = 0;
        }
        if (this.dragging) {
          const n = this.items.length;
          let p = down.pos - dx / this.m.unit;
          // Rubber band at the ends.
          if (p < 0) p *= 0.35;
          else if (p > n - 1) p = n - 1 + (p - (n - 1)) * 0.35;
          this.pos = p;
          this.target = p;
          this.vel = 0;
          down.samples.push({ t: e.timeStamp, p });
          if (down.samples.length > 6) down.samples.shift();
          this.kick();
        }
        return;
      }
      if (e.pointerType !== 'mouse' || this.dragging) return;
      this.hover(e);
    });
    const up = (e: PointerEvent) => {
      if (!down || e.pointerId !== down.id) return;
      const d = down;
      down = null;
      if (this.dragging) {
        this.dragging = false;
        root.classList.remove(styles.dragging);
        const s = d.samples;
        const a = s[0];
        const b = s[s.length - 1];
        const v = b.t > a.t ? ((b.p - a.p) / (b.t - a.t)) * 1000 : 0;
        this.vel = clamp(v, -40, 40);
        this.go(this.pos + this.vel * 0.2);
        return;
      }
      if (e.type === 'pointercancel' || !d.cover) return;
      const i = this.index.get(Number(d.cover.dataset.id));
      if (i === undefined) return;
      if (i === this.ci && Math.abs(this.pos - i) < 0.35) this.openCentre();
      else this.go(i);
    };
    this.on(root, 'pointerup', up);
    this.on(root, 'pointercancel', up);
    this.on(root, 'pointerleave', () => this.untilt());
  }

  private untilt(): void {
    if (!this.tilt.tx && !this.tilt.ty) return;
    this.tilt.tx = this.tilt.ty = 0;
    this.root.classList.remove(styles.tilting);
    this.kick();
  }

  /** The poster in front leans toward the pointer and catches a highlight, like a focused poster on a TV. */
  private hover(e: PointerEvent): void {
    const it = this.items[this.ci];
    const c = it ? this.covers.get(it.id) : undefined;
    if (!c) return;
    const r = c.art.getBoundingClientRect();
    const inside = e.clientX >= r.left && e.clientX <= r.right && e.clientY >= r.top && e.clientY <= r.bottom;
    if (!inside || this.reduced) {
      this.untilt();
      return;
    }
    const u = (e.clientX - r.left) / r.width - 0.5;
    const v = (e.clientY - r.top) / r.height - 0.5;
    this.tilt.ty = u * 9;
    this.tilt.tx = -v * 7;
    c.sheen.style.setProperty('--mx', `${((u + 0.5) * 100).toFixed(1)}%`);
    c.sheen.style.setProperty('--my', `${((v + 0.5) * 100).toFixed(1)}%`);
    this.root.classList.add(styles.tilting);
    this.kick();
  }

  private openCentre(): void {
    const it = this.items[this.ci];
    if (!it) return;
    this.ctx.open(it.id, this.covers.get(it.id)?.art ?? null);
  }

  // ── accessory: the scroll bar Cover Flow always had, made of glass ──

  private buildAccessory(): void {
    const row = el('div', styles.acc);
    const phone = this.ctx.env.phone;
    const prev = el('button', styles.accBtn, { type: 'button', 'aria-label': 'Previous', title: 'Previous (←)' });
    prev.innerHTML = CHEVRON;
    const next = el('button', `${styles.accBtn} ${styles.next}`, { type: 'button', 'aria-label': 'Next', title: 'Next (→)' });
    next.innerHTML = CHEVRON;
    prev.addEventListener('click', () => this.go(Math.round(this.target) - 1));
    next.addEventListener('click', () => this.go(Math.round(this.target) + 1));
    const track = el('div', styles.track, { role: 'slider', tabindex: '0', 'aria-label': 'Position in the library', 'aria-valuemin': '1' });
    const thumb = el('span', styles.thumb);
    track.append(thumb);
    const count = el('span', `${styles.accLabel} mono`);
    if (!phone) row.append(prev);
    row.append(track);
    if (!phone) row.append(count, next);
    this.track = track;
    // Its width once the lens bar shows it, and whenever it changes.
    this.trackRo = new ResizeObserver(() => {
      this.measureTrack();
      this.syncScrub();
    });
    this.trackRo.observe(track);
    this.thumb = thumb;
    this.count = count;
    this.ctx.setAccessory(row);

    let scrubbing: number | null = null;
    const at = (e: PointerEvent) => {
      const r = track.getBoundingClientRect();
      const f = clamp((e.clientX - r.left - 13) / Math.max(1, r.width - 26), 0, 1);
      return f * Math.max(0, this.items.length - 1);
    };
    track.addEventListener('pointerdown', (e) => {
      if (!this.items.length) return;
      scrubbing = e.pointerId;
      track.setPointerCapture(e.pointerId);
      track.classList.add(styles.scrubbing);
      this.target = at(e);
      this.kick();
    });
    track.addEventListener('pointermove', (e) => {
      if (scrubbing !== e.pointerId) return;
      this.target = at(e);
      this.kick();
    });
    const end = (e: PointerEvent) => {
      if (scrubbing !== e.pointerId) return;
      scrubbing = null;
      track.classList.remove(styles.scrubbing);
      this.go(this.target);
    };
    track.addEventListener('pointerup', end);
    track.addEventListener('pointercancel', end);
    // The slider takes arrows too, when it has focus.
    track.addEventListener('keydown', (e) => {
      if (e.key === 'ArrowLeft' || e.key === 'ArrowRight') {
        e.preventDefault();
        e.stopPropagation();
        this.go(Math.round(this.target) + (e.key === 'ArrowRight' ? 1 : -1));
      }
    });
  }

  private syncScrub(): void {
    if (!this.track || !this.thumb || !this.count) return;
    const n = this.items.length;
    const w = this.trackW;
    const f = n > 1 ? clamp(this.pos / (n - 1), 0, 1) : 0;
    this.thumb.style.transform = `translateX(${(f * (w - 26)).toFixed(1)}px)`;
    const ci = n ? clamp(Math.round(this.pos), 0, n - 1) : -1;
    // Text and ARIA only when the centred title (or the count) changes, not every frame.
    const key = ci * 1e7 + n;
    if (key === this.scrubAt) return;
    this.scrubAt = key;
    const txt = n ? `${formatCount(ci + 1)} / ${formatCount(n)}` : '0';
    if (this.count.textContent !== txt) this.count.textContent = txt;
    this.track.setAttribute('aria-valuemax', String(Math.max(1, n)));
    this.track.setAttribute('aria-valuenow', String(ci + 1));
    this.track.setAttribute('aria-valuetext', n && this.items[ci] ? `${titleLine(this.items[ci])}, ${ci + 1} of ${n}` : 'Empty');
  }

  /** The track's width, read on resize instead of every frame. */
  private measureTrack(): void {
    if (this.track) this.trackW = this.track.clientWidth || 140;
    this.scrubAt = -2;
  }

  // ── contract ──

  update(items: readonly ItemSummary[]): void {
    const prevId = this.ci >= 0 ? this.items[this.ci]?.id : undefined;
    const old = this.index;
    this.items = items;
    this.reindex();
    const n = items.length;
    // Covers that left the list fade where they stand.
    for (const [id, c] of this.covers) {
      if (this.index.has(id)) continue;
      this.covers.delete(id);
      if (this.reduced) {
        c.el.remove();
        continue;
      }
      c.el.style.pointerEvents = 'none';
      c.el
        .animate([{ opacity: c.el.style.opacity || '1' }, { opacity: '0' }], { duration: 240, easing: 'ease-in' })
        .finished.then(
          () => c.el.remove(),
          () => c.el.remove(),
        );
    }
    if (!n) {
      this.centre();
      this.syncScrub();
      return;
    }
    // Keep the poster in front where it is: shift everything by its change of index, then let the
    // others slide to their new places. If it left, land on the nearest poster with a short glide
    // instead of sweeping across the library. A new page arriving at the end moves nothing.
    const oldPos = this.pos;
    let newPos: number;
    const was = prevId !== undefined ? old.get(prevId) : undefined;
    const now = prevId !== undefined ? this.index.get(prevId) : undefined;
    if (was !== undefined && now !== undefined) {
      newPos = oldPos + (now - was);
      this.target = now + (this.target - was);
      this.target = clamp(Math.round(this.target), 0, n - 1);
    } else {
      // Nothing was in front yet (the first page just landed): honour the shared focus if it is here now.
      const fi = prevId === undefined && this.ctx.focus !== null ? this.index.get(this.ctx.focus) : undefined;
      this.target = fi ?? clamp(Math.round(oldPos), 0, n - 1);
      newPos = fi !== undefined ? fi : Math.abs(oldPos - this.target) > 1.2 ? this.target + Math.sign(oldPos - this.target) * 0.6 : oldPos;
      this.vel = 0;
    }
    this.pos = newPos;
    for (const c of this.covers.values()) c.vi = newPos + (c.vi - oldPos);
    if (was !== undefined && now !== undefined) {
      // Same poster in front, at a new index: nothing to announce.
      this.ci = clamp(Math.round(this.pos), 0, n - 1);
      this.render(true);
    } else {
      this.ci = -1;
      this.render(true);
      this.centre(true);
    }
    this.syncScrub();
    this.kick();
  }

  move(dir: Dir): boolean {
    if (!this.items.length) return false;
    if (dir === 'left') {
      this.go(Math.round(this.target) - 1);
      return true;
    }
    if (dir === 'right') {
      this.go(Math.round(this.target) + 1);
      return true;
    }
    return false;
  }

  key(e: KeyboardEvent): boolean {
    if (!this.items.length) return false;
    switch (e.key) {
      case 'Enter':
        this.openCentre();
        return true;
      case 'Home':
        this.go(0);
        return true;
      case 'End':
        this.go(this.items.length - 1);
        return true;
      case 'PageUp':
        this.go(Math.round(this.target) - 5);
        return true;
      case 'PageDown':
        this.go(Math.round(this.target) + 5);
        return true;
      default:
        return false;
    }
  }

  rectOf(id: Id): DOMRect | null {
    return this.covers.get(id)?.art.getBoundingClientRect() ?? null;
  }

  artOf(id: Id): HTMLElement | null {
    return this.covers.get(id)?.art ?? null;
  }

  reveal(id: Id): void {
    const i = this.index.get(id);
    if (i === undefined) return;
    if (Math.abs(i - this.pos) > 8 || this.reduced) {
      // Far away: be there, rather than flick past everything in between.
      this.pos = this.target = i;
      this.vel = 0;
      for (const c of this.covers.values()) {
        c.vi = this.index.get(c.id) ?? c.vi;
        c.vv = 0;
      }
      this.render();
      this.centre();
      this.syncScrub();
      return;
    }
    this.go(i);
  }

  pause(on: boolean): void {
    this.paused = on;
    if (on) {
      if (this.raf) cancelAnimationFrame(this.raf);
      this.raf = 0;
      this.untiltQuietly();
    } else this.kick();
  }

  private untiltQuietly(): void {
    this.tilt.tx = this.tilt.ty = this.tilt.x = this.tilt.y = 0;
    this.root.classList.remove(styles.tilting);
  }

  destroy(): void {
    if (this.raf) cancelAnimationFrame(this.raf);
    this.raf = 0;
    this.paused = true;
    if (this.intentT) clearTimeout(this.intentT);
    if (this.snapT) clearTimeout(this.snapT);
    this.ro.disconnect();
    this.trackRo?.disconnect();
    for (const c of this.cleanup) c();
  }
}

export const flow: LensModule = {
  mount(root, ctx) {
    return new FlowView(root, ctx);
  },
};
