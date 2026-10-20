/**
 * Loads a whole library through the cursor-paged items endpoint: a small first
 * page so the grid paints immediately, then large pages in the background.
 * `total` sizes the grid up front, so the scrollbar is right from the start
 * and unloaded cells render as skeletons.
 *
 * The cursor is opaque (api.rs `Page.next`), so pages load in order; random
 * access ("jump to Z") waits for the pages before it. A server-side
 * offset/letter index is listed as a proposed core change.
 */
import { useSyncExternalStore } from 'react';
import { api } from './client';
import { rememberSummaries } from './queries';
import type { Id, ItemSummary, LibrarySort } from './types';

const FIRST_PAGE = 120;
const NEXT_PAGE = 1000;

export class LibraryList {
  readonly libraryId: Id;
  readonly sort: LibrarySort;
  items: ItemSummary[] = [];
  total: number | null = null;
  done = false;
  error: unknown = null;
  /** Bumped on every change; the React snapshot. */
  version = 0;
  private listeners = new Set<() => void>();
  private abort: AbortController | null = null;

  constructor(libraryId: Id, sort: LibrarySort) {
    this.libraryId = libraryId;
    this.sort = sort;
  }

  subscribe = (cb: () => void) => {
    this.listeners.add(cb);
    return () => this.listeners.delete(cb);
  };

  private emit() {
    this.version++;
    for (const l of this.listeners) l();
  }

  /**
   * Start (or restart) loading from the first page. With `keep`, the current
   * items stay on screen until the first fresh page replaces them (live updates).
   */
  load(keep = false): void {
    this.abort?.abort();
    const ctl = new AbortController();
    this.abort = ctl;
    if (!keep) this.items = [];
    this.done = false;
    this.error = null;
    this.emit();
    void this.run(ctl.signal);
  }

  private async run(signal: AbortSignal) {
    let cursor: string | null = null;
    let limit = FIRST_PAGE;
    let firstPage = true;
    try {
      do {
        const page = await api.libraryItems(this.libraryId, { cursor, limit, sort: this.sort }, signal);
        if (signal.aborted) return;
        this.items = firstPage ? page.items : this.items.concat(page.items);
        firstPage = false;
        this.total = page.total ?? (page.next ? null : this.items.length);
        rememberSummaries(page.items);
        cursor = page.next;
        limit = NEXT_PAGE;
        this.done = cursor === null;
        this.emit();
      } while (cursor);
    } catch (err) {
      if (signal.aborted) return;
      this.error = err;
      this.emit();
    }
  }

  dispose(): void {
    this.abort?.abort();
  }
}

const lists = new Map<string, LibraryList>();

/** Shared list per (library, sort); kept for instant back-navigation. */
export function getLibraryList(libraryId: Id, sort: LibrarySort): LibraryList {
  const key = `${libraryId}|${sort}`;
  let list = lists.get(key);
  if (!list) {
    list = new LibraryList(libraryId, sort);
    lists.set(key, list);
    list.load();
    // Keep memory bounded: at most four lists (10k summaries is ~4 MB).
    if (lists.size > 4) {
      const oldest = lists.keys().next().value;
      if (oldest !== undefined && oldest !== key) {
        lists.get(oldest)?.dispose();
        lists.delete(oldest);
      }
    }
  }
  return list;
}

/** Reload every cached list of a library (after `items_changed`), or of every library. */
export function refreshLibrary(libraryId?: Id): void {
  for (const list of lists.values()) if (libraryId === undefined || list.libraryId === libraryId) list.load(true);
}

export function useLibraryList(libraryId: Id, sort: LibrarySort): LibraryList {
  const list = getLibraryList(libraryId, sort);
  useSyncExternalStore(list.subscribe, () => list.version, () => list.version);
  return list;
}
