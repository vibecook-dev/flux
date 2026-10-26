/**
 * Artwork elements for lenses. Every lens builds its posters here, so palettes,
 * blurhash placeholders, width-hinted `srcset`, progress, watched and new marks,
 * offline state and the frame/AI source labels look the same in every lens.
 * Art that flux didn't get from a provider is always labelled (research/01 §8.2.6).
 */
import { artSrcSet, artUrl, artWidthBucket } from '../api/client';
import type { ArtRef, ItemSummary } from '../api/types';
import { sourceLabel } from '../components/art-label';
import { averageColor, blurhashDataUrl } from '../components/blurhash';
import styles from './art.module.css';
import { el } from './dom';
import { paletteOf } from './palette';
import { isNew, isWatchedItem, progressFraction } from './sections';

export interface ArtOptions {
  /** CSS width the art is shown at (the `?w=` hint and `sizes`). */
  width: number;
  /** 2:3 poster or 16:9 still. */
  shape?: 'poster' | 'wide';
  /** Which artwork: the poster (default) or the backdrop still. */
  which?: 'poster' | 'backdrop';
  /** Progress, watched, new and offline marks (default true). */
  marks?: boolean;
  /** The frame/AI source label (default true). Turn off only where the label is shown elsewhere. */
  label?: boolean;
  /** Load now with high priority (the focused title in a 3D lens). */
  eager?: boolean;
  /** Paint the placeholder only; call `loadArt` later (fast scrolling). */
  defer?: boolean;
  /**
   * The placeholder until the image arrives: the blurred poster (default), or just its average
   * colour, which costs nothing to work out (tiles made mid-fling, many per frame).
   */
  placeholder?: 'blur' | 'flat';
  /** Tiny art: colour only, no marks. */
  bare?: boolean;
  className?: string;
}

const CHECK = '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="m3.5 8.5 3 3 6-7" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg>';
const SPARK = '<svg viewBox="0 0 12 12" aria-hidden="true"><path d="M6 1l1.1 3.4L10.5 5.5 7.1 6.6 6 10 4.9 6.6 1.5 5.5l3.4-1.1z" fill="currentColor"/></svg>';

export function artOf(item: ItemSummary, which: 'poster' | 'backdrop' = 'poster'): ArtRef | null {
  if (which === 'backdrop') return item.backdrop ?? (item.kind === 'episode' ? item.poster : null);
  return item.poster;
}

export function artElement(item: ItemSummary, o: ArtOptions): HTMLDivElement {
  const art = artOf(item, o.which);
  const shape = o.shape ?? (item.kind === 'episode' && o.which !== 'poster' ? 'wide' : 'poster');
  const a = el('div', [styles.art, shape === 'wide' ? styles.wide : styles.poster, 'art', o.bare ? styles.bare : '', o.className ?? ''].filter(Boolean).join(' '));
  a.dataset.id = String(item.id);
  const avg = art?.blurhash ? averageColor(art.blurhash) : null;
  if (art?.blurhash && avg) {
    // Under the blurred poster (or as the flat placeholder) the average colour is all that shows.
    a.style.setProperty('--c1', avg);
    a.style.setProperty('--c2', avg);
    if (o.placeholder !== 'flat') {
      const ph = blurhashDataUrl(art.blurhash, shape === 'poster' ? 8 : 16, shape === 'poster' ? 12 : 9);
      if (ph) a.style.backgroundImage = `url(${ph})`;
    }
  } else {
    // No hash: a calm gradient from the title's stable fallback colours.
    const p = paletteOf(item);
    a.style.setProperty('--c1', p.c1);
    a.style.setProperty('--c2', p.c2);
  }
  if (art) {
    a.dataset.src = art.url;
    a.dataset.w = String(Math.round(o.width));
    if (!o.defer) loadArt(a, o.eager);
    if (o.label !== false) {
      const l = sourceLabel(art);
      if (l) {
        const tag = el('span', `${styles.label} ${l.tone === 'ai' ? styles.labelAi : ''}`, { title: l.long });
        tag.innerHTML = (l.tone === 'ai' ? SPARK : '') + l.short;
        a.append(tag);
      }
    }
  } else {
    a.classList.add(styles.typeset);
    a.append(el('span', null, null, item.title));
  }
  if (o.marks !== false) {
    const prog = progressFraction(item);
    if (prog !== null) {
      const bar = el('span', styles.prog, { 'aria-hidden': 'true' });
      const fill = el('i');
      fill.style.width = `${Math.max(3, prog * 100)}%`;
      bar.append(fill);
      a.append(bar);
    } else if (isWatchedItem(item)) {
      const w = el('span', styles.watched, { 'aria-hidden': 'true', title: 'Watched' });
      w.innerHTML = CHECK;
      a.append(w);
    } else if (isNew(item)) {
      a.append(el('span', styles.newDot, { 'aria-hidden': 'true', title: 'New' }));
    }
    if (!item.available) {
      a.classList.add(styles.offline);
      a.append(el('span', styles.offlineTag, null, 'Offline'));
    }
  }
  return a;
}

/** Start loading the image of an element made with `defer`. */
export function loadArt(a: HTMLElement, eager = false): void {
  if (a.querySelector('img') || !a.dataset.src) return;
  const width = Number(a.dataset.w) || 240;
  const ref = { url: a.dataset.src } as ArtRef;
  const img = new Image();
  img.className = styles.img;
  img.alt = '';
  img.decoding = 'async';
  img.draggable = false;
  if (!eager) img.loading = 'lazy';
  else img.fetchPriority = 'high';
  img.sizes = `${Math.round(width)}px`;
  img.srcset = artSrcSet(ref, width);
  img.src = artUrl(ref, artWidthBucket(width, 1));
  const done = () => img.classList.add(styles.loaded);
  if (img.complete && img.naturalWidth) done();
  else img.addEventListener('load', done, { once: true });
  a.prepend(img);
}

/** After a zoom: ask for a sharper (or smaller) image at the new size. */
export function resizeArt(a: HTMLElement, width: number): void {
  a.dataset.w = String(Math.round(width));
  const img = a.querySelector('img');
  if (!img || !a.dataset.src) return;
  const ref = { url: a.dataset.src } as ArtRef;
  img.sizes = `${Math.round(width)}px`;
  img.srcset = artSrcSet(ref, width);
}

/** "Metropolis, 1927, watched" for screen readers. */
export function artLabel(item: ItemSummary): string {
  const parts = [item.parent_title && item.kind !== 'show' ? `${item.parent_title}: ${item.title}` : item.title];
  if (item.year) parts.push(String(item.year));
  if (isWatchedItem(item)) parts.push('watched');
  else if (progressFraction(item) !== null) parts.push('in progress');
  if (!item.available) parts.push('offline');
  return parts.join(', ');
}
