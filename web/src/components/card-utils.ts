import type { ItemKind, ItemSummary } from '../api/types';

/** "2008 · 4K · HDR10" — the small edge line under a card title. */
export function cardEdge(item: ItemSummary): string {
  const parts: string[] = [];
  if (item.kind === 'episode' && item.subtitle) parts.push(item.subtitle);
  else if (item.year) parts.push(String(item.year));
  if (item.kind === 'show' && item.child_count) parts.push(`${item.child_count} season${item.child_count === 1 ? '' : 's'}`);
  if (item.kind === 'season' && item.child_count) parts.push(`${item.child_count} episodes`);
  parts.push(...item.badges.slice(0, 2));
  return parts.join(' · ');
}

/**
 * Whether a detail page has a poster column. An episode's art is a wide still
 * (it is the page's backdrop), so an episode page has none; it used to keep an
 * empty column on the left (e2e P2 #9).
 */
export function hasPosterColumn(kind: ItemKind): boolean {
  return kind !== 'episode';
}

export function itemHref(item: ItemSummary): string {
  return `/item/${item.id}`;
}

/** Watched, for the "Mark watched" toggle: a show or season when no episode is left. */
export function isWatched(item: ItemSummary): boolean {
  if (item.kind === 'show' || item.kind === 'season') return item.unwatched_count === 0;
  return item.progress?.watched ?? false;
}

/**
 * Resumable when a position is stored. A finished viewing stores 0, so a
 * watched item with a position is a rewatch in progress: Continue watching
 * lists it and the server resumes it, so the detail page offers Resume too.
 */
export function isResumable(item: ItemSummary): boolean {
  return (item.progress?.position_ms ?? 0) > 0;
}

const KIND_WORD: Record<ItemKind, string> = { movie: 'Film', show: 'Show', season: 'Season', episode: 'Episode', extra: 'Extra' };

/** The edge line of a search result: "Film · 1982 · 720P", "Episode · Breaking Bad · S01E01" (every non-show was "Film"). */
export function resultEdge(item: ItemSummary): string {
  const parts = [KIND_WORD[item.kind]];
  if ((item.kind === 'episode' || item.kind === 'season' || item.kind === 'extra') && item.parent_title) parts.push(item.parent_title);
  const edge = cardEdge(item);
  if (edge) parts.push(edge);
  return parts.join(' · ');
}
