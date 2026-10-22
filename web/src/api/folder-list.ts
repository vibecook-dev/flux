/**
 * Loads one folder of a videos library through the cursor-paged folders
 * endpoint (`GET /libraries/{id}/folders`): the first page brings the folder's
 * place (breadcrumbs), its subfolders and a first page of videos; the rest of
 * the videos load in the background, as LibraryList does for a whole library.
 */
import { useSyncExternalStore } from 'react';
import { api } from './client';
import { rememberSummaries } from './queries';
import type { FolderEntry, FolderRef, FolderSort, Id, ItemSummary } from './types';

const FIRST_PAGE = 120;
const NEXT_PAGE = 1000;

export class FolderList {
  readonly libraryId: Id;
  readonly folder: string;
  readonly sort: FolderSort;
  at: FolderRef | null = null;
  parents: FolderRef[] = [];
  folders: FolderEntry[] = [];
  items: ItemSummary[] = [];
  total: number | null = null;
  done = false;
  error: unknown = null;
  /** Bumped on every change; the React snapshot. */
  version = 0;
  private listeners = new Set<() => void>();
  private abort: AbortController | null = null;

  constructor(libraryId: Id, folder: string, sort: FolderSort) {
    this.libraryId = libraryId;
    this.folder = folder;
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

  /** Start (or restart) loading. With `keep`, what is on screen stays until the first fresh page. */
  load(keep = false): void {
    this.abort?.abort();
    const ctl = new AbortController();
    this.abort = ctl;
    if (!keep) {
      this.items = [];
      this.folders = [];
    }
    this.done = false;
    this.error = null;
    this.emit();
    void this.run(ctl.signal);
  }

  private async run(signal: AbortSignal) {
    let cursor: string | null = null;
    let limit = FIRST_PAGE;
    let first = true;
    try {
      do {
        const page = await api.libraryFolders(this.libraryId, { folder: this.folder, cursor, limit, sort: this.sort }, signal);
        if (signal.aborted) return;
        if (first) {
          this.at = page.at;
          this.parents = page.parents;
          this.folders = page.folders;
        }
        this.items = first ? page.items : this.items.concat(page.items);
        first = false;
        this.total = page.total;
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

const lists = new Map<string, FolderList>();

/** Shared list per (library, folder, sort); kept for instant back-navigation. */
export function getFolderList(libraryId: Id, folder: string, sort: FolderSort): FolderList {
  const key = `${libraryId}|${sort}|${folder}`;
  let list = lists.get(key);
  if (!list) {
    list = new FolderList(libraryId, folder, sort);
    lists.set(key, list);
    list.load();
    // Up and down a tree: keep a few folders, not every one ever opened.
    if (lists.size > 12) {
      const oldest = lists.keys().next().value;
      if (oldest !== undefined && oldest !== key) {
        lists.get(oldest)?.dispose();
        lists.delete(oldest);
      }
    }
  }
  return list;
}

/**
 * Reload every cached folder of a library (after `items_changed`, a scan, a
 * watched toggle), or of every library: kept for back-navigation, a folder
 * would otherwise show what it held when it was first opened.
 */
export function refreshFolders(libraryId?: Id): void {
  for (const list of lists.values()) if (libraryId === undefined || list.libraryId === libraryId) list.load(true);
}

export function useFolderList(libraryId: Id, folder: string, sort: FolderSort): FolderList {
  const list = getFolderList(libraryId, folder, sort);
  useSyncExternalStore(list.subscribe, () => list.version, () => list.version);
  return list;
}
