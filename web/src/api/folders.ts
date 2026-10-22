/**
 * Browse-by-folder helpers for videos libraries: where a folder lives in the
 * app's URL, its breadcrumbs, and the sort orders the folder view offers.
 */
import { FOLDER_SORTS, type FolderRef, type FolderSort, type Id } from './types';

export const FOLDER_SORT_LABELS: Record<FolderSort, string> = {
  title: 'Name A–Z',
  '-title': 'Name Z–A',
  '-date': 'Newest first',
  date: 'Oldest first',
};

/** The `fsort` query value as a sort (name order when missing or unknown). */
export function parseFolderSort(s: string | null): FolderSort {
  return s !== null && (FOLDER_SORTS as readonly string[]).includes(s) ? (s as FolderSort) : 'title';
}

/** `/library/{id}?folder=…&fsort=…`: a folder of a videos library (the top when `folder` is empty). */
export function folderHref(libraryId: Id, folder: string, sort: FolderSort = 'title'): string {
  const p = new URLSearchParams();
  if (folder) p.set('folder', folder);
  if (sort !== 'title') p.set('fsort', sort);
  const qs = p.toString();
  return `/library/${libraryId}${qs ? `?${qs}` : ''}`;
}

export interface Crumb {
  ref: FolderRef;
  /** The folder on screen (not a link). */
  current: boolean;
}

/** The way down from the library's top to this folder, for a breadcrumb trail. */
export function breadcrumbs(parents: FolderRef[], at: FolderRef | null): Crumb[] {
  const out = parents.map((ref) => ({ ref, current: false }));
  if (at) out.push({ ref: at, current: true });
  return out;
}
