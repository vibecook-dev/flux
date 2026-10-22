/**
 * The mock server's `GET /libraries/{id}/folders`: flux-catalog's folder view
 * (read::folder_page) over the mock's file paths, for one-folder libraries.
 * Folder tokens are `{root}/{dir}` as the server's are.
 */
import type { FolderEntry, FolderRef, FolderSort, Id } from '../api/types';

export interface MockFolderFile {
  /** The item the file belongs to (top-level). */
  id: Id;
  /** Path relative to the library's folder, `/`-separated. */
  rel: string;
  /** Unix ms. */
  mtime: number;
  /** The item's sort key (title order). */
  sortKey: string;
}

export interface MockFolderPage {
  at: FolderRef;
  parents: FolderRef[];
  folders: FolderEntry[];
  /** The page's item ids, in order. */
  ids: Id[];
  next: string | null;
  total: number;
}

const natural = (a: string, b: string) => a.localeCompare(b, undefined, { numeric: true, sensitivity: 'base' });

/** `null` when the folder isn't there (bad root, nothing in it). */
export function mockFolderPage(
  library: { name: string; root: Id },
  files: MockFolderFile[],
  folder: string,
  sort: FolderSort,
  offset: number,
  limit: number,
): MockFolderPage | null {
  const token = folder.replace(/^\/+|\/+$/g, '');
  let dir = '';
  if (token) {
    const [root, ...rest] = token.split('/');
    if (Number(root) !== library.root) return null;
    dir = rest.join('/');
  }
  const prefix = dir ? `${dir}/` : '';
  const here = new Map<Id, MockFolderFile>();
  const below = new Map<string, Set<Id>>();
  for (const f of files) {
    if (!f.rel.startsWith(prefix)) continue;
    const rest = f.rel.slice(prefix.length);
    const slash = rest.indexOf('/');
    if (slash < 0) {
      const seen = here.get(f.id);
      if (!seen || f.mtime < seen.mtime) here.set(f.id, f);
    } else {
      const name = rest.slice(0, slash);
      if (!below.has(name)) below.set(name, new Set());
      below.get(name)?.add(f.id);
    }
  }
  if (dir && here.size === 0 && below.size === 0) return null;
  const tokenOf = (d: string) => (d ? `${library.root}/${d}` : String(library.root));
  const folders: FolderEntry[] = [...below.entries()]
    .sort(([a], [b]) => natural(a, b))
    .map(([name, ids]) => ({ folder: tokenOf(prefix + name), name, item_count: ids.size }));
  const rows = [...here.values()];
  const byTitle = (a: MockFolderFile, b: MockFolderFile) => a.sortKey.localeCompare(b.sortKey) || a.id - b.id;
  const byDate = (a: MockFolderFile, b: MockFolderFile) => a.mtime - b.mtime || byTitle(a, b);
  rows.sort(sort === 'title' || sort === '-title' ? byTitle : byDate);
  if (sort === '-title' || sort === '-date') rows.reverse();
  const top: FolderRef = { folder: '', name: library.name };
  const parents: FolderRef[] = [];
  let at = top;
  if (dir) {
    parents.push(top);
    const parts = dir.split('/');
    parts.forEach((name, i) => {
      const ref = { folder: tokenOf(parts.slice(0, i + 1).join('/')), name };
      if (i === parts.length - 1) at = ref;
      else parents.push(ref);
    });
  }
  return {
    at,
    parents,
    folders,
    ids: rows.slice(offset, offset + limit).map((r) => r.id),
    next: offset + limit < rows.length ? `o${offset + limit}` : null,
    total: rows.length,
  };
}
