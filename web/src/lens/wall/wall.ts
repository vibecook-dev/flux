/**
 * Wall: the whole library at once, like a photo library. Pinch (or + / −) moves
 * between four densities, from a mosaic of decades to posters with captions,
 * anchored where you point. Virtualised: only the rows on screen are in the DOM,
 * so it stays smooth at 10,000+ titles, and titles still loading show as
 * placeholders at their final size.
 */
import type { Id, ItemSummary } from '../../api/types';
import { artElement, artLabel, loadArt, resizeArt } from '../art';
import artStyles from '../art.module.css';
import { clamp, el, flip, prefs, reducedMotion } from '../dom';
import type { Dir, LensContext, LensModule, LensView } from '../types';
import { LEVELS, layoutWall, rowAt, type Row } from './layout';
import styles from './wall.module.css';

interface Tile {
  el: HTMLElement;
  art: HTMLElement;
  index: number;
  id: Id;
}

const OVERSCAN = 400;
const FAST_PX_PER_MS = 3.5;

const ZOOM_OUT = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" aria-hidden="true"><path d="M5.5 12h13"/></svg>';
const ZOOM_IN = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" aria-hidden="true"><path d="M12 5.5v13M5.5 12h13"/></svg>';

export class WallView implements LensView {
  private root: HTMLElement;
  private ctx: LensContext;
  private mini: boolean;
  private scroller: HTMLDivElement;
  private content: HTMLDivElement;
  private tip: HTMLDivElement | null = null;
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
  private tiles = new Map<Id, Tile>();
  private heads = new Map<string, HTMLElement>();
  private skels = new Map<number, HTMLElement>();
  private focusIndex = -1;
  /** The focus came from the keyboard (arrows), so it shows a ring and follows tiles as they are drawn. */
  private keyed = false;
  /** When the last arrow press moved the focus (presses close together scroll instantly). */
  private lastKeyAt = 0;
  private fast = false;
  private lastY = 0;
  private lastT = 0;
  private raf = 0;
  private idle: ReturnType<typeof setTimeout> | null = null;
  private hoverT: ReturnType<typeof setTimeout> | null = null;
  private ro: ResizeObserver;
  private zoomOut: HTMLButtonElement | null = null;
  private zoomIn: HTMLButtonElement | null = null;
  private zoomLabel: HTMLSpanElement | null = null;
  private cleanup: (() => void)[] = [];
  /** Scroll position we set ourselves (zoom, reveal): not a reason to tuck the bars away. */
  private quietUntil = 0;

  constructor(root: HTMLElement, ctx: LensContext) {
    this.root = root;
    this.ctx = ctx;
    this.mini = ctx.env.mini;
    this.items = ctx.items;
    this.total = ctx.total;
    this.level = clamp(Math.round(prefs.get('wall.level', 2)), 0, LEVELS.length - 1);
    root.classList.add(styles.wall);
    this.scroller = el('div', styles.scroller);
    this.content = el('div', styles.content, { role: 'grid', 'aria-label': 'Library' });
    this.scroller.append(this.content);
    root.append(this.scroller);
    if (!this.mini) {
      this.tip = el('div', `${styles.tip} glass glass-clear`, { 'aria-hidden': 'true' });
      root.append(this.tip);
      this.bind();
      this.buildAccessory();
    }
    this.layout();
    this.render();
    this.ro = new ResizeObserver(() => {
      this.layout();
      this.render();
    });
    this.ro.observe(root);
    // Arriving from another lens (or back from detail): keep your place.
    if (ctx.focus !== null) this.reveal(ctx.focus, false);
  }

  // ── layout model ──

  private layout(): void {
    const L = LEVELS[this.level];
    const W = this.root.clientWidth || window.innerWidth;
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
    this.content.style.height = `${this.height}px`;
    this.root.dataset.level = String(this.level);
    if (this.zoomLabel && this.zoomOut && this.zoomIn) {
      this.zoomLabel.textContent = L.name;
      this.zoomOut.disabled = this.level === 0;
      this.zoomIn.disabled = this.level === LEVELS.length - 1;
    }
  }

  private rowAt(y: number): number {
    return rowAt(this.rows, y);
  }

  // ── rendering ──

  private render(): void {
    const top = this.scroller.scrollTop - OVERSCAN;
    const bottom = this.scroller.scrollTop + (this.scroller.clientHeight || window.innerHeight) + OVERSCAN;
    const L = LEVELS[this.level];
    const seenTiles = new Set<Id>();
    const seenHeads = new Set<string>();
    const seenSkels = new Set<number>();
    for (let r = this.rowAt(top); r < this.rows.length && this.rows[r].y <= bottom; r++) {
      const row = this.rows[r];
      if (row.kind === 'items') {
        for (let i = row.start; i < row.end; i++) {
          const item = this.items[i];
          let t = this.tiles.get(item.id);
          if (!t) t = this.makeTile(item, i);
          t.index = i;
          this.place(t.el, this.px[i], this.py[i]);
          seenTiles.add(item.id);
        }
      } else if (row.kind === 'head') {
        const key = `${row.section}@${row.start}`;
        let h = this.heads.get(key);
        if (!h) {
          h = el('div', styles.head, { role: 'rowheader' }, el('span', styles.label, null, row.section), el('span', `${styles.count} mono`, null, String(row.count)));
          this.heads.set(key, h);
          this.content.append(h);
        }
        // A section keeps its header while pages load; its count grows.
        const count = h.lastElementChild;
        if (count && count.textContent !== String(row.count)) count.textContent = String(row.count);
        h.style.transform = `translate(0, ${row.y}px)`;
        h.style.height = `${row.h}px`;
        h.style.left = `${this.padL}px`;
        h.style.width = `${(this.root.clientWidth || window.innerWidth) - this.padL - this.padX}px`;
        seenHeads.add(key);
      } else {
        for (let s = row.start; s < row.end; s++) {
          let sk = this.skels.get(s);
          if (!sk) {
            sk = el('div', `${styles.skel} skeleton`, { 'aria-hidden': 'true' });
            this.skels.set(s, sk);
            this.content.append(sk);
          }
          const col = s - row.start;
          sk.style.transform = `translate(${this.padL + col * (this.tileW + L.gap)}px, ${row.y}px)`;
          sk.style.width = `${this.tileW}px`;
          sk.style.height = `${this.tileW * 1.5}px`;
          seenSkels.add(s);
        }
      }
    }
    // The key-focused title always keeps its tile (focus lives on it), even before the view catches up.
    const fi = this.focusIndex;
    if (this.keyed && fi >= 0 && fi < this.items.length && !seenTiles.has(this.items[fi].id)) {
      const item = this.items[fi];
      const t = this.tiles.get(item.id) ?? this.makeTile(item, fi);
      t.index = fi;
      this.place(t.el, this.px[fi], this.py[fi]);
      seenTiles.add(item.id);
    }
    for (const [id, t] of this.tiles) {
      if (!seenTiles.has(id)) {
        t.el.remove();
        this.tiles.delete(id);
      }
    }
    for (const [k, h] of this.heads) {
      if (!seenHeads.has(k)) {
        h.remove();
        this.heads.delete(k);
      }
    }
    for (const [k, s] of this.skels) {
      if (!seenSkels.has(k)) {
        s.remove();
        this.skels.delete(k);
      }
    }
  }

  private place(node: HTMLElement, x: number, y: number): void {
    node.style.transform = `translate(${x}px, ${y}px)`;
    node.style.width = `${this.tileW}px`;
  }

  private makeTile(item: ItemSummary, index: number): Tile {
    // A real link: middle-click and "open in new tab" work; a plain click flies the poster instead.
    // It carries the build's base ('/' unless built for a sub-path, src/app/base.ts).
    const node = el('a', styles.tile, { href: `${import.meta.env.BASE_URL}item/${item.id}`, role: 'gridcell', tabindex: '-1', 'data-id': item.id, 'aria-label': artLabel(item), draggable: 'false' });
    const art = artElement(item, { width: this.tileW, defer: this.fast, placeholder: this.fast ? 'flat' : 'blur', bare: this.level === 0, className: styles.art });
    node.append(art);
    const cap = el('div', styles.cap);
    cap.append(el('b', null, null, item.parent_title && item.kind === 'episode' ? `${item.parent_title}: ${item.title}` : item.title));
    const edge = [item.year ? String(item.year) : '', item.kind === 'show' && item.child_count ? `${item.child_count} season${item.child_count === 1 ? '' : 's'}` : '', ...item.badges.slice(0, 2)].filter(Boolean).join(' · ');
    cap.append(el('small', 'mono', null, edge));
    node.append(cap);
    const keyedHere = this.keyed && this.focusIndex === index;
    if (keyedHere) node.classList.add(styles.kf);
    this.content.append(node);
    const t: Tile = { el: node, art, index, id: item.id };
    this.tiles.set(item.id, t);
    // Held arrow keys outrun the smooth scroll: the focused title takes focus as soon as it is drawn
    // (unless focus has moved somewhere else, like the search field).
    if (keyedHere && !this.mini) {
      const a = document.activeElement;
      if (!a || a === document.body || this.root.contains(a)) node.focus({ preventScroll: true });
    }
    return t;
  }

  // ── updates ──

  update(items: readonly ItemSummary[], total: number): void {
    const before = [...this.tiles.values()].map((t) => t.el);
    const f = flip(before, { duration: 480 });
    const prevFocusId = this.focusIndex >= 0 ? this.items[this.focusIndex]?.id : undefined;
    const keep = new Set(items.map((i) => i.id));
    for (const [id, t] of this.tiles) {
      if (!keep.has(id)) {
        this.vanish(t.el);
        this.tiles.delete(id);
      }
    }
    this.items = items;
    this.total = total;
    this.focusIndex = prevFocusId !== undefined ? items.findIndex((i) => i.id === prevFocusId) : -1;
    this.layout();
    // Results shorter than the scroll position: back to the top.
    if (this.scroller.scrollTop > this.height - this.scroller.clientHeight) this.scroller.scrollTop = Math.max(0, this.height - this.scroller.clientHeight);
    this.render();
    f.play([...this.tiles.values()].map((t) => t.el));
  }

  private vanish(node: HTMLElement): void {
    const r = node.getBoundingClientRect();
    if (reducedMotion() || !r.width || r.bottom < 0 || r.top > window.innerHeight) {
      node.remove();
      return;
    }
    const sr = this.root.getBoundingClientRect();
    node.style.transform = `translate(${r.left - sr.left}px, ${r.top - sr.top}px)`;
    node.style.zIndex = '3';
    node.style.pointerEvents = 'none';
    this.root.append(node);
    node
      .animate([{ opacity: 1, scale: '1' }, { opacity: 0, scale: '.86' }], { duration: 240, easing: 'ease-in', fill: 'forwards' })
      .finished.then(
        () => node.remove(),
        () => node.remove(),
      );
  }

  // ── zoom ──

  private setLevel(n: number, point?: { x: number; y: number }): void {
    n = clamp(n, 0, LEVELS.length - 1);
    if (n === this.level) return;
    const anchor = this.anchorIndex(point);
    const sr = this.scroller.getBoundingClientRect();
    const p = point ?? (anchor >= 0 ? { x: sr.left + this.px[anchor] + this.tileW / 2, y: sr.top + this.py[anchor] - this.scroller.scrollTop + this.tileH / 2 } : null);
    const u = anchor >= 0 && p ? clamp((p.y - (sr.top + this.py[anchor] - this.scroller.scrollTop)) / this.tileH, 0, 1) : 0.5;
    const f = flip([...this.tiles.values()].map((t) => t.el), { duration: 560 });
    this.level = n;
    prefs.set('wall.level', n);
    this.layout();
    if (anchor >= 0 && p) {
      this.quietUntil = performance.now() + 200;
      this.scroller.scrollTop = this.py[anchor] + u * this.tileH - (p.y - sr.top);
    }
    // Tiles keep their art; only the size hint and the mosaic's bareness change.
    for (const t of this.tiles.values()) {
      resizeArt(t.art, this.tileW);
      t.art.classList.toggle(artStyles.bare, n === 0);
    }
    this.render();
    f.play([...this.tiles.values()].map((t) => t.el));
    this.hideTip();
  }

  private anchorIndex(point?: { x: number; y: number }): number {
    const sr = this.scroller.getBoundingClientRect();
    if (point) {
      const y = point.y - sr.top + this.scroller.scrollTop;
      const r = this.rows[this.rowAt(y)];
      if (r && r.kind === 'items') {
        const col = clamp(Math.floor((point.x - sr.left - this.padL) / (this.tileW + LEVELS[this.level].gap)), 0, r.end - r.start - 1);
        return r.start + col;
      }
    }
    if (this.focusIndex >= 0) return this.focusIndex;
    // The middle of the view, or the nearest row of titles to it (the middle may be a heading).
    const mid = this.rowAt(this.scroller.scrollTop + this.scroller.clientHeight / 2);
    for (let d = 0; d < this.rows.length; d++) {
      for (const k of [mid + d, mid - d]) {
        const r = this.rows[k];
        if (r?.kind === 'items') return r.start + Math.floor((r.end - r.start) / 2);
      }
    }
    return -1;
  }

  /** Where the arrows start when nothing has key focus: the title you point at, else the first one in view. */
  private startIndex(): number {
    const f = this.ctx.focus;
    const t = f !== null ? this.tiles.get(f) : undefined;
    const top = this.scroller.scrollTop + this.ctx.safe.top;
    const bottom = this.scroller.scrollTop + this.scroller.clientHeight - this.ctx.safe.bottom;
    if (t && this.py[t.index] + this.tileH > top && this.py[t.index] < bottom) return t.index;
    for (let r = this.rowAt(top); r < this.rows.length && this.rows[r].y < bottom; r++) {
      const row = this.rows[r];
      if (row.kind === 'items' && row.y + this.tileH / 2 > top) return row.start;
    }
    return this.anchorIndex();
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
        if (!this.raf)
          this.raf = requestAnimationFrame(() => {
            this.raf = 0;
            this.render();
          });
        if (this.idle) clearTimeout(this.idle);
        this.idle = setTimeout(() => {
          this.fast = false;
          for (const t of this.tiles.values()) loadArt(t.art);
        }, 140);
        this.hideTip();
      },
      { passive: true },
    );

    // Trackpad pinch arrives as ctrl+wheel in Chromium and Firefox.
    let acc = 0;
    let lock = 0;
    on(
      sc,
      'wheel',
      (e) => {
        if (!e.ctrlKey) return;
        e.preventDefault();
        const now = performance.now();
        if (now < lock) return;
        acc += e.deltaY;
        if (Math.abs(acc) > 34) {
          this.setLevel(this.level + (acc < 0 ? 1 : -1), { x: e.clientX, y: e.clientY });
          acc = 0;
          lock = now + 380;
        }
      },
      { passive: false },
    );
    // Safari's trackpad pinch.
    let gs = 1;
    type GestureEvent = UIEvent & { scale: number; clientX: number; clientY: number };
    const gstart = (e: Event) => {
      e.preventDefault();
      gs = 1;
    };
    const gchange = (e: Event) => {
      e.preventDefault();
      const g = e as GestureEvent;
      if (g.scale / gs > 1.28) {
        this.setLevel(this.level + 1, { x: g.clientX, y: g.clientY });
        gs = g.scale;
      } else if (g.scale / gs < 0.78) {
        this.setLevel(this.level - 1, { x: g.clientX, y: g.clientY });
        gs = g.scale;
      }
    };
    sc.addEventListener('gesturestart', gstart);
    sc.addEventListener('gesturechange', gchange);
    this.cleanup.push(() => {
      sc.removeEventListener('gesturestart', gstart);
      sc.removeEventListener('gesturechange', gchange);
    });
    // Two-finger pinch on touch screens.
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
      if (!pts.has(e.pointerId)) return;
      pts.set(e.pointerId, { x: e.clientX, y: e.clientY });
      if (pts.size === 2 && base) {
        const d = dist();
        const [a, b] = [...pts.values()];
        const mid = { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 };
        if (d / base > 1.3) {
          this.setLevel(this.level + 1, mid);
          base = d;
        } else if (d / base < 0.76) {
          this.setLevel(this.level - 1, mid);
          base = d;
        }
      }
    });
    const up = (e: PointerEvent) => {
      pts.delete(e.pointerId);
      if (pts.size < 2) base = 0;
    };
    on(sc, 'pointerup', up);
    on(sc, 'pointercancel', up);

    // Hover lights the room, warms the detail and names the poster; click opens it.
    on(this.content, 'pointerover', (e) => {
      if (e.pointerType === 'touch') return;
      const tile = (e.target as HTMLElement).closest<HTMLElement>(`.${styles.tile}`);
      if (!tile) return;
      const t = this.tiles.get(Number(tile.dataset.id));
      if (!t) return;
      if (this.hoverT) clearTimeout(this.hoverT);
      this.hoverT = setTimeout(() => {
        this.ctx.setFocus(t.id);
        this.ctx.intent(this.items[t.index]);
      }, 110);
      this.showTip(t);
    });
    on(this.content, 'pointerleave', () => this.hideTip());
    on(this.content, 'click', (e) => {
      const tile = (e.target as HTMLElement).closest<HTMLElement>(`.${styles.tile}`);
      if (!tile || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
      e.preventDefault();
      const t = this.tiles.get(Number(tile.dataset.id));
      if (!t) return;
      if (this.focusIndex >= 0) this.tiles.get(this.items[this.focusIndex]?.id)?.el.classList.remove(styles.kf);
      this.focusIndex = t.index;
      this.keyed = false;
      this.ctx.setFocus(t.id);
      this.ctx.open(t.id, t.art);
    });
  }

  private showTip(t: Tile): void {
    if (!this.tip || LEVELS[this.level].cap > 0) return this.hideTip();
    const item = this.items[t.index];
    if (!item) return;
    this.tip.replaceChildren(el('b', null, null, item.title), el('span', 'mono', null, item.year ? String(item.year) : ''));
    const r = t.art.getBoundingClientRect();
    const sr = this.root.getBoundingClientRect();
    this.tip.classList.add(styles.on);
    const tw = this.tip.offsetWidth;
    const x = clamp(r.left - sr.left + r.width / 2 - tw / 2, 10, sr.width - tw - 10);
    const above = r.top - sr.top - 40;
    const y = above < this.ctx.safe.top - 10 ? r.bottom - sr.top + 8 : above;
    this.tip.style.transform = `translate(${x}px, ${y}px)`;
  }

  private hideTip(): void {
    this.tip?.classList.remove(styles.on);
  }

  move(dir: Dir): boolean {
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

  private setKeyFocus(i: number, scroll = true): void {
    const prev = this.focusIndex >= 0 ? this.tiles.get(this.items[this.focusIndex]?.id) : undefined;
    prev?.el.classList.remove(styles.kf);
    this.focusIndex = i;
    this.keyed = true;
    // A held key repeats faster than a smooth scroll settles: jump, so the view keeps up.
    const now = performance.now();
    const quick = now - this.lastKeyAt < 160;
    this.lastKeyAt = now;
    const item = this.items[i];
    this.ctx.setFocus(item.id);
    this.ctx.intent(item);
    if (scroll) this.scrollToIndex(i, 'nearest', quick);
    const t = this.tiles.get(item.id);
    if (t) {
      t.el.classList.add(styles.kf);
      t.el.focus({ preventScroll: true });
      this.showTip(t);
    }
  }

  private scrollToIndex(i: number, align: 'nearest' | 'center', instant = false): void {
    const safe = this.ctx.safe;
    const top = this.py[i];
    const viewTop = this.scroller.scrollTop + safe.top + 12;
    const viewBottom = this.scroller.scrollTop + this.scroller.clientHeight - safe.bottom - 12;
    let target: number | null = null;
    if (align === 'center') target = top + this.tileH / 2 - this.scroller.clientHeight / 2;
    else if (top < viewTop) target = top - safe.top - 12;
    else if (top + this.tileH > viewBottom) target = top + this.tileH - this.scroller.clientHeight + safe.bottom + 12;
    if (target === null) return;
    this.scroller.scrollTo({ top: Math.max(0, target), behavior: instant || reducedMotion() || align === 'center' ? 'auto' : 'smooth' });
    this.render();
  }

  key(e: KeyboardEvent): boolean {
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
      if (item) this.ctx.open(item.id, this.tiles.get(item.id)?.art ?? null);
      return true;
    }
    return false;
  }

  rectOf(id: Id): DOMRect | null {
    const t = this.tiles.get(id);
    if (t) return t.art.getBoundingClientRect();
    const i = this.items.findIndex((x) => x.id === id);
    if (i < 0) return null;
    const sr = this.scroller.getBoundingClientRect();
    return new DOMRect(sr.left + this.px[i], sr.top + this.py[i] - this.scroller.scrollTop, this.tileW, this.tileW * 1.5);
  }

  artOf(id: Id): HTMLElement | null {
    return this.tiles.get(id)?.art ?? null;
  }

  reveal(id: Id, mark = true): void {
    const i = this.items.findIndex((x) => x.id === id);
    if (i < 0) return;
    const y = this.py[i];
    const visible = y >= this.scroller.scrollTop + this.ctx.safe.top && y + this.tileH <= this.scroller.scrollTop + this.scroller.clientHeight - this.ctx.safe.bottom;
    if (!visible) {
      this.quietUntil = performance.now() + 200;
      this.scroller.scrollTop = Math.max(0, y + this.tileH / 2 - this.scroller.clientHeight / 2);
      this.render();
    }
    if (mark) this.setKeyFocus(i, false);
    else {
      this.focusIndex = i;
      this.keyed = false;
    }
  }

  destroy(): void {
    this.ro.disconnect();
    for (const c of this.cleanup) c();
    if (this.raf) cancelAnimationFrame(this.raf);
    if (this.idle) clearTimeout(this.idle);
    if (this.hoverT) clearTimeout(this.hoverT);
    this.tip?.remove();
  }
}

export const wall: LensModule = {
  mount(root, ctx) {
    return new WallView(root, ctx);
  },
};

