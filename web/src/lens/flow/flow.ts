/**
 * Flow: Cover Flow for a film and TV library. One poster faces you; the rest
 * stand in two receding stacks you flick through. The room takes its light from
 * the poster in front, and the floor carries soft reflections.
 *
 * This is the DOM renderer (CSS 3D), used where WebGPU isn't available; the
 * behaviour lives in core.ts and gpu-flow.ts draws the same scene on the GPU.
 * Only the covers near the position are in the DOM (enough to reach past the
 * window's edges), so a library of 13,000 titles flicks as smoothly as one of
 * fifty, and pages that arrive while you browse slide into place.
 */
import type { Id, ItemSummary } from '../../api/types';
import { artElement, artLabel, resizeArt } from '../art';
import { clamp, el } from '../dom';
import type { LensContext, LensModule } from '../types';
import { coverPose, edgeOpacity, FlowCore, shadeOf, type CoverState, type Metrics } from './core';
import styles from './flow.module.css';

export { ANGLE, measure, metaLine, pose, titleLine, windowAround, type Metrics, type Pose } from './core';

interface Cover extends CoverState {
  el: HTMLDivElement;
  art: HTMLDivElement;
  sheen: HTMLSpanElement;
  /** Last written stacking, shade and opacity: style is only touched when they change. */
  z: number;
  sh: number;
  op: string;
}

class FlowView extends FlowCore<Cover> {
  private stage: HTMLDivElement;
  private contact: HTMLDivElement;

  constructor(root: HTMLElement, ctx: LensContext) {
    super(root, ctx);
    this.stage = el('div', styles.stage);
    const floor = el('div', styles.floor, { 'aria-hidden': 'true' });
    this.contact = el('div', styles.contact, { 'aria-hidden': 'true' });
    const mist = el('div', styles.mist, { 'aria-hidden': 'true' });
    this.stage.append(floor, this.contact, mist);
    root.append(this.stage);
    this.start();
  }

  // ── geometry ──

  protected applyMetrics(m: Metrics): void {
    const s = this.root.style;
    s.setProperty('--cw', `${m.cw.toFixed(1)}px`);
    s.setProperty('--ch', `${m.ch.toFixed(1)}px`);
    s.setProperty('--floor', `${m.floor.toFixed(1)}px`);
    s.setProperty('--title-y', `${(m.floor + m.reflGap).toFixed(1)}px`);
    this.stage.style.perspective = `${m.persp.toFixed(0)}px`;
    this.stage.style.perspectiveOrigin = `50% ${(m.floor - m.ch / 2).toFixed(1)}px`;
  }

  protected resized(): void {
    const cw = this.m.cw;
    this.m = this.measure();
    if (Math.abs(cw - this.m.cw) > 1) for (const c of this.covers.values()) resizeArt(c.art, this.m.cw);
    this.render();
    this.syncScrub();
  }

  // ── drawing ──

  protected make(it: ItemSummary, i: number, fadeIn: boolean): Cover {
    const node = el('div', styles.cover, { role: 'option', 'aria-label': artLabel(it), 'aria-selected': 'false', 'data-id': it.id });
    // Mid-flick (or far out in the stacks) the poster's average colour is placeholder enough; it costs nothing to work out.
    const near = Math.abs(i - this.pos) < 4 && Math.abs(this.vel) < 6;
    const art = artElement(it, { width: this.m.cw, eager: Math.abs(i - this.pos) < 3, placeholder: near ? 'blur' : 'flat', className: styles.art });
    const sheen = el('span', styles.sheen, { 'aria-hidden': 'true' });
    node.append(art, sheen);
    this.stage.append(node);
    const c: Cover = { id: it.id, el: node, art, sheen, vi: i, vv: 0, z: NaN, sh: NaN, op: '' };
    if (i === this.ci) {
      node.classList.add(styles.centre);
      node.setAttribute('aria-selected', 'true');
    }
    if (fadeIn && !this.reduced) node.animate([{ opacity: 0 }, { opacity: 1 }], { duration: 360, easing: 'ease-out' });
    return c;
  }

  protected drop(c: Cover): void {
    c.el.remove();
  }

  protected retire(c: Cover): void {
    if (this.reduced) {
      c.el.remove();
      return;
    }
    c.el.style.pointerEvents = 'none';
    c.el
      .animate([{ opacity: c.el.style.opacity || '1' }, { opacity: '0' }], { duration: 240, easing: 'ease-in' })
      .finished.then(
        () => c.el.remove(),
        () => c.el.remove(),
      );
  }

  protected paint(): void {
    for (const c of this.covers.values()) this.place(c);
    // A soft contact shadow under the poster in front, fading while it is between two.
    const off = Math.abs(this.pos - Math.round(this.pos));
    this.contact.style.opacity = this.items.length ? String(clamp(1 - off * 2.4, 0, 1)) : '0';
  }

  private place(c: Cover): void {
    const p = coverPose(c, this.pos, this.m, this.tilt, this.dragging);
    const st = c.el.style;
    st.transform = `translate3d(${p.x.toFixed(2)}px,0,${p.z.toFixed(2)}px) rotateY(${p.ry.toFixed(2)}deg) rotateX(${p.rx.toFixed(2)}deg)`;
    // Stacking only needs the order (eighth steps keep the crossing in the middle right).
    const z = 1000 - Math.round(p.ad * 8);
    if (z !== c.z) {
      c.z = z;
      st.zIndex = String(z);
    }
    const sh = shadeOf(p.ad);
    if (sh !== c.sh) {
      c.sh = sh;
      st.setProperty('--sh', String(sh));
    }
    const R = this.m.R;
    const op = p.ad > R - 1.5 ? edgeOpacity(p.ad, R).toFixed(2) : '';
    if (op !== c.op) {
      c.op = op;
      st.opacity = op;
    }
  }

  protected selected(id: Id): void {
    for (const c of this.covers.values()) {
      const on = c.id === id;
      c.el.setAttribute('aria-selected', String(on));
      c.el.classList.toggle(styles.centre, on);
    }
  }

  // ── pointer ──

  protected coverAt(e: PointerEvent): Id | null {
    const cover = (e.target as HTMLElement).closest<HTMLElement>(`.${styles.cover}`);
    return cover ? Number(cover.dataset.id) : null;
  }

  protected centreRect(): DOMRect | null {
    const it = this.items[this.ci];
    const c = it ? this.covers.get(it.id) : undefined;
    return c ? c.art.getBoundingClientRect() : null;
  }

  protected sheenAt(u: number, v: number): void {
    const it = this.items[this.ci];
    const c = it ? this.covers.get(it.id) : undefined;
    if (!c) return;
    c.sheen.style.setProperty('--mx', `${((u + 0.5) * 100).toFixed(1)}%`);
    c.sheen.style.setProperty('--my', `${((v + 0.5) * 100).toFixed(1)}%`);
  }

  protected artFor(id: Id): Element | null {
    return this.covers.get(id)?.art ?? null;
  }

  // ── contract ──

  rectOf(id: Id): DOMRect | null {
    return this.covers.get(id)?.art.getBoundingClientRect() ?? null;
  }

  artOf(id: Id): HTMLElement | null {
    return this.covers.get(id)?.art ?? null;
  }
}

export const flow: LensModule = {
  mount(root, ctx) {
    return new FlowView(root, ctx);
  },
};
