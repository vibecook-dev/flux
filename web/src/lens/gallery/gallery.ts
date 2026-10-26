/**
 * Gallery: walk an exhibition of your own library. Posters hang as framed prints
 * on two walls, each section of the current sort becomes a room behind a
 * doorway, and the camera glides from work to work, turning toward the one you
 * stop at (the walk is core.ts; this is its DOM renderer, CSS 3D planes, and the
 * fallback for gpu-gallery.ts). Only the stretch of corridor around you is in the
 * DOM, and planes are keyed by what they show, so a library still arriving in
 * pages (or a search narrowing it) never rebuilds what is already on the walls.
 */
import { formatCount } from '../../api/format';
import type { Id } from '../../api/types';
import { artElement, artLabel } from '../art';
import { el } from '../dom';
import type { LensContext, LensModule } from '../types';
import { AHEAD, BEHIND, CEIL, DEPTH, FLOOR, FRAME_SHIFT, GalleryCore, H, JAMB, LINTEL, PH, PORTAL_AT, PW, PY, W, pieceLayout, plural, slotRange, wallLines, type EndSlot, type RoomSlot, type Slot, type WorkSlot } from './core';
import styles from './gallery.module.css';

interface Rec {
  slot: Slot;
  els: HTMLElement[];
  piece?: HTMLElement;
  art?: HTMLElement;
}

class GalleryView extends GalleryCore {
  private view: HTMLDivElement;
  private world: HTMLDivElement;
  private live = new Map<string, Rec>();
  private windowD: number | null = null;

  constructor(root: HTMLElement, ctx: LensContext) {
    super(root, ctx);
    this.view = el('div', styles.view);
    this.world = el('div', styles.world);
    this.view.append(this.world);
    root.append(this.view);
    this.start();
  }

  protected surface(): HTMLElement {
    return this.view;
  }

  protected resized(flipped: boolean): void {
    this.view.style.perspective = `${this.P.toFixed(1)}px`;
    // Labels sit beside frames in landscape and under them in portrait: rebuild what's hung.
    if (flipped) {
      this.clear();
      this.ensureWindow(true);
    }
  }

  protected render(): void {
    const p = this.pose;
    this.world.style.transform = `translateZ(${this.P.toFixed(1)}px) rotateY(${p.yaw.toFixed(3)}deg) translate3d(${(-p.x).toFixed(2)}px, ${(-p.y).toFixed(2)}px, ${p.d.toFixed(2)}px)`;
    if (this.windowD === null || Math.abs(p.d - this.windowD) > 140) this.ensureWindow();
  }

  protected showChanged(force: boolean): void {
    this.ensureWindow(force);
  }

  protected focused(idx: number): void {
    for (const rec of this.live.values()) if (rec.piece) rec.piece.classList.toggle(styles.on, rec.slot.type === 'work' && rec.slot.i === idx);
  }

  protected workAt(e: MouseEvent): number {
    const f = (e.target as HTMLElement).closest<HTMLElement>('[data-work]');
    if (!f) return -1;
    const id = Number(f.dataset.work);
    return this.works.findIndex((w) => w.item.id === id);
  }

  // ── the planes ──

  private clear(): void {
    for (const rec of this.live.values()) for (const e of rec.els) e.remove();
    this.live.clear();
    this.windowD = null;
  }

  /** Keep the planes for the stretch around the camera; reuse any whose content is unchanged. */
  private ensureWindow(force = false): void {
    const dc = this.pose.d;
    if (!force && this.windowD !== null && Math.abs(dc - this.windowD) < 140) return;
    this.windowD = dc;
    const [a, b] = slotRange(this.slots, dc - BEHIND, dc + (this.mini ? 3600 : AHEAD));
    const want = new Set<string>();
    for (let s = a; s < b; s++) {
      const sl = this.slots[s];
      want.add(sl.key);
      if (!this.live.has(sl.key)) this.live.set(sl.key, this.build(sl));
      else (this.live.get(sl.key) as Rec).slot = sl;
    }
    for (const [key, rec] of this.live) {
      if (want.has(key)) continue;
      for (const e of rec.els) e.remove();
      this.live.delete(key);
    }
  }

  private plane(cls: string, w: number, h: number, transform: string, els: HTMLElement[]): HTMLDivElement {
    const p = el('div', `${styles.plane} ${cls}`);
    p.style.width = `${w}px`;
    p.style.height = `${h}px`;
    p.style.transform = transform;
    this.world.append(p);
    els.push(p);
    return p;
  }

  private wallT(side: number, d0: number, len: number, y: number, inset = 0, extra = ''): string {
    return side < 0 ? `translate3d(${-W / 2 + inset}px, ${y}px, ${-d0}px) rotateY(90deg)${extra}` : `translate3d(${W / 2 - inset}px, ${y}px, ${-(d0 + len)}px) rotateY(-90deg)${extra}`;
  }

  private build(sl: Slot): Rec {
    const els: HTMLElement[] = [];
    const rec: Rec = { slot: sl, els };
    const len = sl.d1 - sl.d0 + 2; // a little overlap hides the seams between panels
    const d0 = sl.d0 - 1;
    this.plane(styles.wall, len, H, this.wallT(-1, d0, len, CEIL), els);
    this.plane(styles.wall, len, H, this.wallT(1, d0, len, CEIL), els);
    this.plane(styles.floor, W, len, `translate3d(${-W / 2}px, ${FLOOR}px, ${-(d0 + len)}px) rotateX(90deg)`, els);
    const ceil = this.plane(styles.ceil, W, len, `translate3d(${-W / 2}px, ${CEIL}px, ${-d0}px) rotateX(-90deg)`, els);
    if (sl.type === 'work') {
      // The track light that throws this work's pool of light.
      ceil.style.setProperty('--fix-x', `${W / 2 + sl.side * (W / 2 - 170)}px`);
      ceil.style.setProperty('--fix-y', `${this.frameD(sl) - d0}px`);
      this.buildWork(sl, rec);
    } else if (sl.type === 'room') this.buildRoom(sl, els);
    else this.buildEnd(sl, els);
    return rec;
  }

  private buildWork(sl: WorkSlot, rec: Rec): void {
    const it = sl.item;
    const piece = el('div', `${styles.plane} ${styles.piece}`);
    piece.style.width = `${PW}px`;
    piece.style.height = `${PH}px`;
    const L = pieceLayout(it, this.portrait === true);
    piece.style.setProperty('--fx', `${(((PW / 2 - FRAME_SHIFT) / PW) * 100).toFixed(1)}%`);
    piece.style.setProperty('--fy', `${(((L.ft - 40) / PH) * 100).toFixed(1)}%`);

    const spot = el('div', styles.spot);
    const frame = el('button', styles.frame, { type: 'button', tabindex: '-1', 'data-work': it.id, 'aria-label': artLabel(it) });
    Object.assign(frame.style, { left: `${L.fl}px`, top: `${L.ft}px`, width: `${L.fw}px`, height: `${L.fh}px` });
    const mat = el('div', styles.mat);
    const near = this.focusIdx >= 0 && Math.abs(sl.i - this.focusIdx) <= 2;
    const art = artElement(it, { width: L.pw, shape: L.wide ? 'wide' : 'poster', eager: near, className: styles.art });
    art.style.width = `${L.pw}px`;
    art.style.height = `${L.ph}px`;
    mat.append(art);
    frame.append(mat, el('span', styles.glaze, { 'aria-hidden': 'true' }));

    const l = wallLines(it);
    const lab = el('div', styles.label);
    lab.append(el('b', null, null, l.title));
    if (l.year) lab.append(el('span', styles.yr, null, l.year));
    lab.append(el('p', null, null, l.what));
    if (l.medium) lab.append(el('p', styles.med, null, l.medium));
    if (l.status) lab.append(el('p', `${styles.st} ${l.tone === 'new' ? styles.fresh : l.tone === 'offline' ? styles.away : ''}`, null, l.status));
    Object.assign(lab.style, { left: `${L.lx}px`, top: `${L.ly}px`, width: `${L.lw}px` });
    piece.append(spot, frame, lab);
    piece.style.transform = this.wallT(sl.side, sl.d - PW / 2, PW, PY, 2);
    piece.classList.toggle(styles.on, sl.i === this.focusIdx);
    this.world.append(piece);
    rec.els.push(piece);
    rec.piece = piece;
    rec.art = art;

    // The polished floor gives back a dim reflection of each work.
    if (!this.mini || sl.i < 6) {
      const mirror = piece.cloneNode(true) as HTMLElement;
      mirror.classList.add(styles.mirror);
      mirror.classList.remove(styles.on);
      mirror.setAttribute('aria-hidden', 'true');
      mirror.querySelector(`.${styles.label}`)?.remove();
      for (const b of mirror.querySelectorAll('[data-work], [data-id]')) {
        b.removeAttribute('data-work');
        b.removeAttribute('data-id');
      }
      for (const b of mirror.querySelectorAll('button')) b.setAttribute('disabled', '');
      mirror.style.transform = this.wallT(sl.side, sl.d - PW / 2, PW, 2 * FLOOR - PY, 3, ' scaleY(-1)');
      this.world.append(mirror);
      rec.els.push(mirror);
    }
  }

  private buildRoom(sl: RoomSlot, els: HTMLElement[]): void {
    let textAt = sl.d0 + 30;
    if (sl.portal) {
      const dp = sl.d0 + PORTAL_AT;
      const face = (x: number, y: number, d: number) => `translate3d(${x}px, ${y}px, ${-d}px)`;
      // A doorway into the room: two jambs and a lintel carrying the room's name.
      this.plane(styles.jamb, JAMB, H, face(-W / 2, CEIL, dp), els);
      this.plane(styles.jamb, JAMB, H, face(W / 2 - JAMB, CEIL, dp), els);
      this.plane(styles.jambSide, DEPTH, H, this.wallT(-1, dp, DEPTH, CEIL, JAMB), els);
      this.plane(styles.jambSide, DEPTH, H, this.wallT(1, dp, DEPTH, CEIL, JAMB), els);
      this.plane(styles.soffit, W - 2 * JAMB, DEPTH, `translate3d(${-W / 2 + JAMB}px, ${CEIL + LINTEL}px, ${-dp}px) rotateX(-90deg)`, els);
      const lintel = this.plane(styles.lintel, W - 2 * JAMB, LINTEL, face(-W / 2 + JAMB, CEIL, dp), els);
      if (sl.label) {
        const name = el('b', null, null, sl.label);
        name.style.fontSize = `${Math.min(64, Math.round(1300 / Math.max(6, sl.label.length)))}px`;
        lintel.append(name, el('small', null, null, sl.sub));
      }
      textAt = dp + DEPTH + 210;
    }
    if (!sl.label) return;
    // Room text on the left wall: the room's title and what hangs in it.
    const text = this.plane(styles.roomtext, 540, 330, this.wallT(-1, textAt, 540, -230, 2), els);
    const title = el('b', null, null, sl.label);
    title.style.fontSize = `${Math.min(104, Math.round(1700 / Math.max(5, sl.label.length)))}px`;
    text.append(title, el('small', null, null, sl.sub));
    if (sl.by) text.append(el('p', null, null, sl.by));
  }

  private buildEnd(sl: EndSlot, els: HTMLElement[]): void {
    const end = this.plane(styles.end, W, H, `translate3d(${-W / 2}px, ${CEIL}px, ${-sl.d1}px)`, els);
    const p = el('p');
    if (sl.pending > 0) p.append('More works are being hung', el('span', null, null, `${formatCount(sl.count)} of ${formatCount(sl.count + sl.pending)} so far`));
    else if (sl.count > 0) p.append('End of the exhibition', el('span', null, null, plural(sl.count, 'work', 'works')));
    end.append(p, el('i', styles.door, { 'aria-hidden': 'true' }));
  }

  // ── contract ──

  rectOf(id: Id): DOMRect | null {
    return this.artOf(id)?.getBoundingClientRect() ?? null;
  }

  artOf(id: Id): HTMLElement | null {
    for (const rec of this.live.values()) if (rec.slot.type === 'work' && rec.slot.item.id === id) return rec.art ?? null;
    return null;
  }

  destroy(): void {
    this.stop();
    this.clear();
  }
}

export const gallery: LensModule = {
  mount(root, ctx) {
    return new GalleryView(root, ctx);
  },
};
