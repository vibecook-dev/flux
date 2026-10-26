/**
 * A space's titles for the lenses: one library, or every film and show library
 * merged into "All" (videos libraries are browsed by folder, so they stay out).
 * Built on LibraryList's paged loading; the merged list keeps the server's sort
 * by merging the per-library lists with the same order.
 */
import { useMemo, useSyncExternalStore } from 'react';
import { titleSortKey } from './format';
import { getCollectionList, getLibraryList, type LibraryList } from './library-list';
import type { ItemSummary, Library, LibrarySort } from './types';

/** `all`, a library id, or a collection (`c3`). */
export type SpaceId = 'all' | number | `c${number}`;

/** The collection a space is, if it is one. */
export function collectionOf(id: SpaceId): number | null {
  return typeof id === 'string' && id.startsWith('c') ? Number(id.slice(1)) || null : null;
}

export interface SpaceSnapshot {
  items: readonly ItemSummary[];
  /** Titles in the space once known (sizes skeletons while pages arrive). */
  total: number | null;
  done: boolean;
  error: unknown;
  /** Changes whenever any of the above does. */
  version: string;
  reload: () => void;
}

/** Libraries that make up "All". */
export function allLibraries(libraries: readonly Library[]): Library[] {
  return libraries.filter((l) => l.kind !== 'videos');
}

function compare(sort: LibrarySort): (a: ItemSummary, b: ItemSummary) => number {
  const byTitle = (a: ItemSummary, b: ItemSummary) => titleSortKey(a.title).localeCompare(titleSortKey(b.title));
  switch (sort) {
    case 'title':
      return byTitle;
    case '-title':
      return (a, b) => byTitle(b, a);
    case 'year':
      return (a, b) => (a.year ?? 9999) - (b.year ?? 9999) || byTitle(a, b);
    case '-year':
      return (a, b) => (b.year ?? -1) - (a.year ?? -1) || byTitle(a, b);
    case 'added':
      return (a, b) => a.added_at - b.added_at || byTitle(a, b);
    case '-added':
      return (a, b) => b.added_at - a.added_at || byTitle(a, b);
  }
}

/**
 * The part of a merged list that can no longer change: everything up to the
 * smallest "last loaded" title among the lists still loading. Showing only this
 * prefix (and placeholders for the rest) keeps titles from shifting while other
 * libraries' pages arrive.
 */
export function settledPrefix(merged: readonly ItemSummary[], lists: readonly { items: readonly ItemSummary[]; done: boolean }[], sort: LibrarySort): readonly ItemSummary[] {
  const open = lists.filter((l) => !l.done);
  if (open.length === 0) return merged;
  if (open.some((l) => l.items.length === 0)) return [];
  const cmp = compare(sort);
  const frontier = open.map((l) => l.items[l.items.length - 1]).reduce((a, b) => (cmp(a, b) <= 0 ? a : b));
  let lo = 0;
  let hi = merged.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (cmp(merged[mid], frontier) <= 0) lo = mid + 1;
    else hi = mid;
  }
  return lo === merged.length ? merged : merged.slice(0, lo);
}

/** Merge already-sorted lists (k-way, stable by list order). */
export function mergeSorted(lists: readonly (readonly ItemSummary[])[], sort: LibrarySort): ItemSummary[] {
  const cmp = compare(sort);
  const out: ItemSummary[] = [];
  const idx = lists.map(() => 0);
  const total = lists.reduce((n, l) => n + l.length, 0);
  while (out.length < total) {
    let best = -1;
    for (let k = 0; k < lists.length; k++) {
      if (idx[k] >= lists[k].length) continue;
      if (best < 0 || cmp(lists[k][idx[k]], lists[best][idx[best]]) < 0) best = k;
    }
    out.push(lists[best][idx[best]++]);
  }
  return out;
}

function snapshotOf(lists: readonly LibraryList[], sort: LibrarySort, cache: { key: string; items: readonly ItemSummary[] }): SpaceSnapshot {
  const version = lists.map((l) => `${l.key}:${l.version}`).join(',');
  let items = cache.items;
  if (cache.key !== version) {
    items = lists.length === 1 ? lists[0].items : settledPrefix(mergeSorted(lists.map((l) => l.items), sort), lists, sort);
    cache.key = version;
    cache.items = items;
  }
  const totals = lists.map((l) => l.total);
  return {
    items,
    total: totals.some((t) => t === null) ? null : totals.reduce<number>((n, t) => n + (t ?? 0), 0),
    done: lists.every((l) => l.done),
    error: lists.find((l) => l.error !== null)?.error ?? null,
    version,
    reload: () => {
      for (const l of lists) l.load();
    },
  };
}

/**
 * Subscribe to a space's titles. `libraries` is the server's list (needed for
 * "All"); while it is still loading, "All" is empty and not done.
 */
export function useSpaceList(space: SpaceId, sort: LibrarySort, libraries: readonly Library[] | undefined): SpaceSnapshot {
  const collection = collectionOf(space);
  const ids = useMemo(() => {
    if (typeof space === 'number') return [space];
    if (space !== 'all') return [];
    return libraries ? allLibraries(libraries).map((l) => l.id) : [];
  }, [space, libraries]);
  const key = `${collection ?? ids.join(',')}|${sort}`;
  const lists = useMemo(
    () => (collection !== null ? [getCollectionList(collection, sort)] : ids.map((id) => getLibraryList(id, sort))),
    [collection, ids, sort],
  );
  const cache = useMemo(() => ({ key: '', items: [] as readonly ItemSummary[] }), [key]); // eslint-disable-line react-hooks/exhaustive-deps
  const subscribe = useMemo(
    () => (cb: () => void) => {
      const offs = lists.map((l) => l.subscribe(cb));
      return () => offs.forEach((off) => off());
    },
    [lists],
  );
  const version = useSyncExternalStore(
    subscribe,
    () => lists.map((l) => l.version).join(','),
    () => lists.map((l) => l.version).join(','),
  );
  const pending = space === 'all' && libraries === undefined;
  return useMemo(() => {
    if (pending) return { items: [], total: null, done: false, error: null, version: 'pending', reload: () => undefined };
    return snapshotOf(lists, sort, cache);
    // `version` changes whenever a list changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [lists, sort, cache, version, pending]);
}
