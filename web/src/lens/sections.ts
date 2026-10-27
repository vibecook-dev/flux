/**
 * Spaces, filters, sorts and sections: the shell searches, filters and sorts once,
 * and every lens receives the result. Pure functions, shared with tests.
 */
import { foldForSearch, titleSortKey } from '../api/format';
import type { ItemSummary, LibrarySort } from '../api/types';
import { averageOfHash, fallbackPalette } from './palette';

/** What the island's "Show" chips filter by. */
export const SHOWS = ['all', 'unwatched', 'progress', 'new', '4k', 'hdr'] as const;
export type Show = (typeof SHOWS)[number];

export const SHOW_LABELS: Record<Show, string> = {
  all: 'All',
  unwatched: 'Unwatched',
  progress: 'In progress',
  new: 'New',
  '4k': '4K',
  hdr: 'HDR',
};

/** Sorts the island offers: the server's, plus colour (client-side, from the poster palettes). */
export const SORTS = ['title', '-year', 'year', '-added', 'colour'] as const;
export type Sort = (typeof SORTS)[number];

export const SORT_LABELS: Record<Sort, string> = {
  title: 'Title',
  '-year': 'Newest',
  year: 'Oldest',
  '-added': 'Added',
  colour: 'Colour',
};

/** The server-side sort a space list is loaded in. */
export function serverSort(sort: Sort): LibrarySort {
  return sort === 'colour' ? 'title' : sort;
}

export function parseSort(v: string | null): Sort {
  return (SORTS as readonly string[]).includes(v ?? '') ? (v as Sort) : 'title';
}

export function parseShow(v: string | null): Show {
  return (SHOWS as readonly string[]).includes(v ?? '') ? (v as Show) : 'all';
}

const NEW_MS = 14 * 24 * 3600 * 1000;

export function isNew(item: ItemSummary, now = Date.now()): boolean {
  return now - item.added_at < NEW_MS;
}

export function progressFraction(item: ItemSummary): number | null {
  const p = item.progress;
  if (!p || p.watched || !p.duration_ms || p.position_ms <= 0) return null;
  return Math.min(1, p.position_ms / p.duration_ms);
}

export function isWatchedItem(item: ItemSummary): boolean {
  if (item.kind === 'show' || item.kind === 'season') return item.unwatched_count === 0;
  return item.progress?.watched ?? false;
}

const HDR = new Set(['HDR10', 'HDR10+', 'DV', 'HLG']);

export function matchesShow(item: ItemSummary, show: Show, now = Date.now()): boolean {
  switch (show) {
    case 'all':
      return true;
    case 'unwatched':
      return !isWatchedItem(item);
    case 'progress':
      return progressFraction(item) !== null;
    case 'new':
      return isNew(item, now);
    case '4k':
      return item.badges.includes('4K');
    case 'hdr':
      return item.badges.some((b) => HDR.has(b));
  }
}

// What search looks through, per title: built once per summary, then reused on every keystroke.
const haystacks = new WeakMap<ItemSummary, string>();

/** Search as you type: every word must appear in the title (accents and case ignored). */
export function matchesText(item: ItemSummary, folded: string[]): boolean {
  if (folded.length === 0) return true;
  let hay = haystacks.get(item);
  if (hay === undefined) {
    hay = foldForSearch(`${item.title} ${item.parent_title ?? ''} ${item.year ?? ''}`);
    haystacks.set(item, hay);
  }
  return folded.every((w) => hay.includes(w));
}

export function foldWords(text: string): string[] {
  return foldForSearch(text.trim())
    .split(/\s+/)
    .filter(Boolean);
}

/** Colour order: colours round the hue circle, then greys from light to dark. */
export function colourKey(item: ItemSummary): number {
  const p = averageOfHash(item.poster?.blurhash ?? item.backdrop?.blurhash) ?? fallbackPalette(item.title);
  if (p.sat < 0.16) return 1000 + (1 - p.lum) * 100;
  return (p.hue + 330) % 360;
}

/** Apply show filter, text and the client-side part of the sort. */
export function visibleItems(items: readonly ItemSummary[], { show, text, sort }: { show: Show; text: string; sort: Sort }): ItemSummary[] {
  const words = foldWords(text);
  const now = Date.now();
  const out = show === 'all' && words.length === 0 ? [...items] : items.filter((it) => matchesShow(it, show, now) && matchesText(it, words));
  if (sort === 'colour') {
    const keyed = out.map((it) => [colourKey(it), it] as const);
    keyed.sort((a, b) => a[0] - b[0]);
    return keyed.map(([, it]) => it);
  }
  return out;
}

const DAY = 24 * 3600 * 1000;

/** The section a title belongs to under a sort; lenses that show sections use it. */
export function sectionOf(item: ItemSummary, sort: Sort, now = Date.now()): string {
  switch (sort) {
    case 'title': {
      const c = titleSortKey(item.title).charAt(0).toUpperCase();
      return c >= 'A' && c <= 'Z' ? c : '#';
    }
    case 'year':
    case '-year':
      return item.year ? `${Math.floor(item.year / 10) * 10}s` : 'Undated';
    case '-added': {
      const age = now - item.added_at;
      if (age < 7 * DAY) return 'This week';
      if (age < 31 * DAY) return 'This month';
      if (age < 365 * DAY) return 'This year';
      return 'Earlier';
    }
    case 'colour':
      return '';
  }
}
