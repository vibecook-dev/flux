/**
 * Shelf: your library as VHS tapes on wall shelves, seen spine-on, the way a video
 * shop kept them. Point at a tape and it lifts; rest on it and it comes out and
 * turns to show its cover; click and the cover opens as the detail. On touch the
 * first tap shows the cover and the second opens it; keyboard focus shows it too.
 * Shelves fill left to right in bays that scroll sideways.
 *
 * Windowed: the whole shelf is planned (positions are just numbers), but only the
 * tapes in and near the visible bays are in the DOM, so a 13,000-title library
 * scrolls as smoothly as fifty tapes. Spines are painted from each title's
 * palette (its blurhash), so no poster is downloaded until a tape is pointed at.
 *
 * This is the Shelf where WebGPU isn't available (gpu-shelf.ts draws it otherwise);
 * the turn is CSS: `translate` and `rotate` on their own springs, so a tape is out
 * of the shelf before it turns (peek.ts says why).
 */
import type { Id, ItemSummary } from '../../api/types';
import { artElement, artLabel, loadArt } from '../art';
import { activeLens } from '../bridge';
import { clamp, el, rand01, reducedMotion } from '../dom';
import { paletteOf } from '../palette';
import { isNew, progressFraction } from '../sections';
import type { Dir, LensContext, LensModule, LensView } from '../types';
import { planShelf, shelfMetrics, spineLook, spineTitleParts, type Metrics, type ShelfPlan } from './geometry';
import { asideFalloff, peekDepth, peekWait } from './peek';
import styles from './shelf.module.css';

export { planShelf, spineLook, spineTitleParts } from './geometry';
export type { ShelfPlan, ShelfPlanInput, SpineLook, SpineStyle } from './geometry';

const SOFT = 'cubic-bezier(.25,.85,.3,1)';
const MINI_COUNT = 14;
const MARK =
  '<svg viewBox="0 0 35 28" aria-hidden="true"><path d="M2 26 L20 2 L27 2 L9 26 Z" fill="currentColor"/><path d="M16 26 L28 10 L33 10 L21 26 Z" fill="currentColor" fill-opacity=".45"/></svg>';
const CONDENSED = '800 20px "Archivo Variable"';
/** The turn takes this long to show the cover (shelf.module.css `.peek`): an opened tape hands over to the detail then. */
const TURN_MS = 560;
/** A pointer's cover goes back this long after the pointer leaves the tapes. */
const LEAVE_MS = 120;

// ── the lens ──

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
  /** The tape under the pointer (by its place on the shelf), since when; the one showing its cover, since when, and who asked. */
  private hoverId: Id | null = null;
  private hoverSince = 0;
  private peekId: Id | null = null;
  private peekSince = 0;
  private peekBy: 'pointer' | 'key' | 'touch' | 'open' | null = null;
  private pointer: { x: number; y: number; t: number } | null = null;
  private speed = 0;
  private lastType = 'mouse';
  private sceneRect: DOMRect | null = null;
  /** Neighbours eased aside for the cover showing. */
  private aside: HTMLElement[] = [];
  private raf = 0;
  private intentT: ReturnType<typeof setTimeout> | null = null;
  private leaveT: ReturnType<typeof setTimeout> | null = null;
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
    const m = shelfMetrics({
      vw: this.root.clientWidth || window.innerWidth,
      vh: this.root.clientHeight || window.innerHeight,
      safeTop: this.ctx.safe.top,
      safeBottom: this.ctx.safe.bottom,
      mini: this.mini,
      scale,
    });
    const { H, W, D, bh } = m;
    const s = this.world.style;
    s.setProperty('--h', `${H}px`);
    s.setProperty('--w', `${W}px`);
    s.setProperty('--d', `${D}px`);
    s.setProperty('--bh', `${bh}px`);
    s.setProperty('--lip', `${m.lip}px`);
    s.setProperty('--bd', `${m.bd}px`);
    s.setProperty('--pull', `${Math.round(W * 0.9)}px`);
    // Out far enough to turn clear of its neighbours (a box set's is the widest turn).
    s.setProperty('--peek-z', `${peekDepth({ H, W, D, tw: m.boxW, gap: m.gap })}px`);
    s.setProperty('--lift', `${Math.round(-H * 0.05)}px`);
    s.setProperty('--hov-y', `${Math.round(-H * 0.055)}px`);
    s.setProperty('--fs0', `${(W * 0.52).toFixed(1)}px`);
    s.setProperty('--ew', `${m.ew}px`);
    s.setProperty('--yr', `${Math.max(6.5, W * 0.235).toFixed(1)}px`);
    this.scene.style.setProperty('--persp', `${m.persp}px`);
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
    this.sceneRect = null;
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
      if (this.peekId !== null) this.tapes.get(this.peekId)?.slot.classList.add(styles.peek);
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
        this.setHover(null);
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
    // A cover the pointer or a tap turned goes back when the shelf moves.
    const putBack = () => {
      if (this.peekBy === 'pointer' || this.peekBy === 'touch') this.peek(null, 'pointer');
    };
    on('pointerdown', (e) => {
      this.lastType = e.pointerType;
    });
    on('pointermove', (e) => {
      if (down?.drag) {
        this.setHover(null);
        putBack();
        return;
      }
      if (e.pointerType === 'touch') return;
      this.pointerMoved(e.clientX, e.clientY);
    });
    on('pointerleave', (e) => {
      if (e.pointerType === 'touch' || down?.drag) return;
      this.pointer = null;
      this.setHover(null);
    });
    on('wheel', putBack, { passive: true });
    on('click', (e) => {
      if (this.suppressClick) return;
      // What you see under the pointer: a cover out of the shelf, else the tape whose place it is.
      const s = slotOf(e.target);
      const id = s ? Number(s.dataset.id) : this.slotAt(e.clientX, e.clientY);
      const touch = this.lastType === 'touch';
      if (id === null) {
        if (touch) putBack();
        return;
      }
      // A tap shows the cover first; tapping it again opens it.
      if (touch && id !== this.peekId) {
        this.peek(id, 'touch');
        return;
      }
      this.openTape(id);
    });
    on('focusin', (e) => {
      // Keyboard focus moving to a tape shows its cover (focus handed back to the same one doesn't).
      const s = slotOf(e.target);
      if (!s || Number(s.dataset.id) === this.focusId) return;
      this.setKeyFocus(Number(s.dataset.id));
      this.peek(Number(s.dataset.id), 'key');
    });
  }

  /** The tape whose place on the shelf is under a point (client coordinates), with half the gap either side. */
  private slotAt(cx: number, cy: number): Id | null {
    const r = (this.sceneRect ??= this.scene.getBoundingClientRect());
    const x = cx - r.left + this.scrollX;
    const y = cy - r.top;
    const pad = this.m.gap / 2 + 0.25;
    const held = this.hoverId !== null ? this.index.get(this.hoverId) : undefined;
    for (const t of this.tapes.values()) {
      const i = this.index.get(t.id);
      if (i === undefined) continue;
      const grace = i === held ? 3 : 0;
      if (x >= this.X[i] - pad - grace && x < this.X[i] + this.widths[i] + pad + grace && y >= this.Y[i] - this.m.H * 0.06 && y < this.Y[i] + this.m.H) return t.id;
    }
    return null;
  }

  private pointerMoved(cx: number, cy: number): void {
    const now = performance.now();
    const p = this.pointer;
    if (p) {
      const dt = Math.max(1, now - p.t);
      const v = Math.hypot(cx - p.x, cy - p.y) / dt;
      this.speed = dt > 60 ? v : this.speed * 0.55 + v * 0.45;
    }
    this.pointer = { x: cx, y: cy, t: now };
    const id = this.slotAt(cx, cy);
    if (id === this.hoverId) {
      if (id !== null && !this.intentT) this.armIntent();
    } else this.setHover(id);
  }

  private setHover(id: Id | null): void {
    if (id === this.hoverId) return;
    this.hoverId = id;
    this.hoverSince = performance.now();
    if (this.intentT) clearTimeout(this.intentT);
    this.intentT = null;
    if (id === null) {
      // Off the tapes: a cover the pointer turned goes back after a moment (a brief excursion keeps it).
      if (this.peekBy === 'pointer' && this.peekId !== null && !this.leaveT) {
        this.leaveT = setTimeout(() => {
          this.leaveT = null;
          if (this.hoverId === null && this.peekBy === 'pointer') this.peek(null, 'pointer');
        }, LEAVE_MS);
      }
      return;
    }
    if (this.leaveT) clearTimeout(this.leaveT);
    this.leaveT = null;
    // Its poster starts loading now, so the cover is there when it turns.
    const tape = this.tapes.get(id);
    if (tape) loadArt(tape.sleeve);
    this.armIntent();
  }

  /** When the pointer rests (or moves slowly) on a tape, turn it to show its cover; a fast sweep turns none. */
  private armIntent(): void {
    if (this.intentT) clearTimeout(this.intentT);
    this.intentT = null;
    if (this.hoverId === null || this.hoverId === this.peekId) return;
    this.intentT = setTimeout(this.intent, this.intentWait());
  }

  private intentWait(): number {
    const now = performance.now();
    const resting = !this.pointer || now - this.pointer.t > 45;
    const wait = peekWait(now - this.hoverSince, resting ? 0 : this.speed);
    return Number.isFinite(wait) ? Math.max(0, wait) : 50;
  }

  private intent = (): void => {
    this.intentT = null;
    const id = this.hoverId;
    if (id === null || id === this.peekId || this.dead) return;
    const wait = this.intentWait();
    if (wait > 0) {
      this.intentT = setTimeout(this.intent, Math.max(16, wait));
      return;
    }
    this.peek(id, 'pointer');
  };

  /** Turn this tape's cover to you (null: none); the one showing goes back as it does, and its neighbours ease aside. */
  private peek(id: Id | null, by: 'pointer' | 'key' | 'touch' | 'open'): void {
    if (this.leaveT) clearTimeout(this.leaveT);
    this.leaveT = null;
    this.peekBy = id === null ? null : by;
    if (id === this.peekId) return;
    const prev = this.peekId !== null ? this.tapes.get(this.peekId) : undefined;
    prev?.slot.classList.remove(styles.peek);
    for (const n of this.aside) n.style.translate = '';
    this.aside = [];
    this.peekId = id;
    this.peekSince = performance.now();
    if (id === null) return;
    const i = this.index.get(id);
    const tape = this.tapes.get(id);
    if (i === undefined || !tape) return;
    loadArt(tape.sleeve, true);
    tape.slot.classList.add(styles.peek);
    this.ctx.setFocus(id);
    this.ctx.intent(this.items[i]);
    if (reducedMotion()) return;
    // The tapes beside it ease aside a little (each gap closing at most to nothing).
    const { p, m } = this;
    const same = (j: number) => j >= 0 && j < p.n && p.bay[j] === p.bay[i] && p.row[j] === p.row[i];
    for (const dir of [1, -1]) {
      const gaps: number[] = [];
      for (let j = i + dir; same(j) && same(j + dir) && gaps.length < 6; j += dir) {
        const a = dir > 0 ? j : j + dir;
        gaps.push(this.X[a + 1] - this.X[a] - this.widths[a]);
      }
      asideFalloff(m.gap * 3, gaps).forEach((off, k) => {
        const j = i + dir * (k + 1);
        const n = same(j) ? this.tapes.get(this.items[j].id)?.slot : undefined;
        if (!n) return;
        n.style.translate = `${(dir * off).toFixed(2)}px 0`;
        this.aside.push(n);
      });
    }
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
    const shown = this.peekId === id ? performance.now() - this.peekSince : 0;
    this.peek(id, 'open');
    this.openId = id;
    if (reducedMotion()) {
      this.ctx.open(id, tape.sleeve);
      return;
    }
    // The cover turns to you (if it isn't already), then becomes the detail view.
    // Skip the hand-off if this lens has stopped being the one on screen (switched lens mid-turn).
    const live = () => !this.dead && this.openId === id && activeLens()?.view === this;
    this.timers.push(
      setTimeout(
        () => {
          if (live()) this.ctx.open(id, tape.sleeve);
        },
        Math.max(0, TURN_MS - shown),
      ),
    );
  }

  private closeTape(id: Id): void {
    if (this.openId === id) this.openId = null;
    if (this.peekId === id) this.peek(null, 'pointer');
  }

  // ── contract ──

  update(items: readonly ItemSummary[]): void {
    this.setItems(items);
    this.relayout(true);
  }

  /** Keyboard focus to a tape, which shows its cover. */
  private keyTo(id: Id): void {
    this.setKeyFocus(id);
    this.peek(id, 'key');
  }

  move(dir: Dir): boolean {
    const n = this.items.length;
    if (!n) return false;
    const cur = this.focusId !== null ? this.index.get(this.focusId) : undefined;
    if (cur === undefined) {
      // The first tape on screen.
      let first = 0;
      while (first < n - 1 && this.X[first] - this.scrollX < this.m.padX) first++;
      this.keyTo(this.items[first].id);
      return true;
    }
    if (dir === 'left' || dir === 'right') {
      this.keyTo(this.items[clamp(cur + (dir === 'right' ? 1 : -1), 0, n - 1)].id);
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
    if (best >= 0) this.keyTo(this.items[best].id);
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
    if (tape) return (this.openId === id || this.peekId === id ? tape.sleeve : tape.spine).getBoundingClientRect();
    // Off the window: where its spine would be (flat, ignoring perspective).
    const i = this.index.get(id);
    if (i === undefined) return null;
    const r = this.root.getBoundingClientRect();
    return new DOMRect(r.left + this.X[i] - this.scrollX, r.top + this.Y[i], this.widths[i], this.m.H);
  }

  artOf(id: Id): HTMLElement | null {
    const tape = this.tapes.get(id);
    if (!tape) return null;
    return this.openId === id || this.peekId === id ? tape.sleeve : tape.spine;
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
    if (paused && this.intentT) {
      clearTimeout(this.intentT);
      this.intentT = null;
    }
    if (!paused && this.openId !== null) this.closeTape(this.openId);
  }

  destroy(): void {
    this.dead = true;
    this.ro.disconnect();
    for (const c of this.cleanup) c();
    if (this.raf) cancelAnimationFrame(this.raf);
    if (this.intentT) clearTimeout(this.intentT);
    if (this.leaveT) clearTimeout(this.leaveT);
    for (const t of this.timers) clearTimeout(t);
  }
}

export const shelf: LensModule = {
  mount(root, ctx) {
    return new ShelfView(root, ctx);
  },
};
