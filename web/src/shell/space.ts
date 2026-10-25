/**
 * The space under everything: "All" at `/`, one library at `/library/:id`. Its
 * filter (`show`), sort and search text live in the query string, so Back and
 * Forward and shared links keep them. While an overlay (detail, settings) is
 * open, the space is the last one you were in.
 */
import { matchRoute, navigate, setQuery } from '../app/router';
import type { SpaceId } from '../api/space-list';
import { parseShow, parseSort, type Show, type Sort } from '../lens/sections';

export interface SpaceState {
  id: SpaceId;
  show: Show;
  sort: Sort;
  text: string;
  /** Where this space lives (for Back from an overlay). */
  href: string;
}

let last = { pathname: '/', search: '' };

/** The space for a location: its own when it is a space route, else the last space visited. */
export function spaceFor(pathname: string, search: string): SpaceState {
  const m = matchRoute(pathname);
  if (!m || m.route.layer === 'space') last = { pathname, search };
  const route = matchRoute(last.pathname);
  const id: SpaceId = route?.route.name === 'library' ? Number(route.params.id) : 'all';
  const q = new URLSearchParams(last.search);
  return { id, show: parseShow(q.get('show')), sort: parseSort(q.get('sort')), text: q.get('q') ?? '', href: last.pathname + last.search };
}

export function spacePath(id: SpaceId): string {
  return id === 'all' ? '/' : `/library/${id}`;
}

/** The key lens choices are remembered under. */
export function spaceKey(id: SpaceId): string {
  return id === 'all' ? 'all' : `lib${id}`;
}

function onSpaceRoute(): boolean {
  const m = matchRoute(location.pathname);
  return !m || m.route.layer === 'space';
}

/** Change the space's query (filter, sort, text) without a new history entry. */
export function setSpaceQuery(update: Record<string, string | null>): void {
  if (onSpaceRoute()) {
    setQuery(update);
    return;
  }
  // From an overlay: go back to the space with the change applied.
  const q = new URLSearchParams(last.search);
  for (const [k, v] of Object.entries(update)) {
    if (v === null || v === '') q.delete(k);
    else q.set(k, v);
  }
  const qs = q.toString();
  navigate(`${last.pathname}${qs ? `?${qs}` : ''}`);
}

/** Switch space, keeping the sort; the filter and search reset. */
export function goToSpace(id: SpaceId, sort: Sort): void {
  const qs = sort !== 'title' ? `?sort=${encodeURIComponent(sort)}` : '';
  navigate(`${spacePath(id)}${qs}`);
}
