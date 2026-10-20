/**
 * Query keys and fetchers for each screen, plus the item-summary memory that
 * lets a detail page render its header instantly from the card that was
 * clicked (optimistic navigation) while the full ItemDetail loads.
 */
import { api } from './client';
import { prefetchQuery, useQuery } from './query';
import type { Id, ItemDetail, ItemSummary } from './types';

export const keys = {
  server: 'server',
  libraries: 'libraries',
  home: 'home',
  item: (id: Id) => `item:${id}`,
  children: (id: Id) => `children:${id}`,
  tailnet: 'tailnet',
  invites: 'invites',
} as const;

export const useServer = () => useQuery(keys.server, api.server, { staleMs: 60_000 });
export const useLibraries = () => useQuery(keys.libraries, api.libraries, { staleMs: 10_000 });
export const useHome = () => useQuery(keys.home, api.home, { staleMs: 15_000 });
export const useItem = (id: Id) => useQuery(keys.item(id), () => api.item(id));
export const useChildren = (id: Id | null) =>
  useQuery(keys.children(id ?? -1), () => api.children(id ?? -1), { enabled: id !== null });
export const useTailnet = () => useQuery(keys.tailnet, api.tailnet, { staleMs: 5_000 });
export const useInvites = () => useQuery(keys.invites, api.invites);

const summaries = new Map<Id, ItemSummary>();

/** Remember summaries seen in lists so detail pages can paint before fetching. */
export function rememberSummaries(items: Iterable<ItemSummary | undefined>): void {
  for (const it of items) if (it) summaries.set(it.id, it);
}

export function recallSummary(id: Id): ItemSummary | undefined {
  return summaries.get(id);
}

/** Hover/focus intent on a card: warm the detail (and a show's seasons). */
export function prefetchItem(item: ItemSummary): void {
  summaries.set(item.id, item);
  prefetchQuery<ItemDetail>(keys.item(item.id), () => api.item(item.id));
  if (item.kind === 'show' || item.kind === 'season') {
    prefetchQuery<ItemSummary[]>(keys.children(item.id), () => api.children(item.id));
  }
}

/** Show title for an episode or season (`ItemSummary.parent_title`): no lookups needed. */
export function parentTitle(item: ItemSummary | undefined): string | null {
  return item && (item.kind === 'episode' || item.kind === 'season') ? item.parent_title : null;
}
