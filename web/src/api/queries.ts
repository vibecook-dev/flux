/**
 * Query keys and fetchers for each screen, plus the item-summary memory that
 * lets a detail page render its header instantly from the card that was
 * clicked (optimistic navigation) while the full ItemDetail loads.
 */
import { noteServer } from '../app/update';
import { manifest, op, type AgentItemDetail, type ChangesListOutput, type Collections, type Lenses } from './agent';
import { api } from './client';
import { prefetchQuery, useQuery } from './query';
import type { Id, ItemDetail, ItemSummary } from './types';

export const keys = {
  server: 'server',
  update: 'update',
  devices: 'devices',
  mediaTools: 'media-tools',
  addresses: 'addresses',
  libraries: 'libraries',
  home: 'home',
  item: (id: Id) => `item:${id}`,
  children: (id: Id) => `children:${id}`,
  tailnet: 'tailnet',
  invites: 'invites',
  /** Agents' and people's changesets (`changes.list`). */
  changes: 'changes',
  /** One item as the operations see it (`item.get`): details and where each came from. */
  facts: (id: Id) => `facts:${id}`,
  /** Lenses kept on the server (`lenses.list`). */
  serverLenses: 'server-lenses',
  /** `collections.list`. */
  collections: 'collections',
  /** `GET /api/v1/agent`. */
  manifest: 'agent-manifest',
} as const;

// Every answer also tells the page whether flux was updated under it (app/update.ts).
const serverInfo = () => api.server().then((info) => noteServer(info));
export const useServer = () => useQuery(keys.server, serverInfo, { staleMs: 60_000 });
/** flux's own updates (Owners and Admins); `update` events keep it current. */
export const useUpdateInfo = (enabled = true) => useQuery(keys.update, api.updateInfo, { staleMs: 60_000, enabled });
/** Owner devices (Owners and Admins). */
export const useDevices = (enabled = true) => useQuery(keys.devices, api.devices, { staleMs: 10_000, enabled });
/** What ffmpeg can do on the server (Owners and Admins). */
export const useMediaTools = (enabled = true) => useQuery(keys.mediaTools, api.mediaTools, { staleMs: 30_000, enabled });
/** Where other devices reach flux (Owners and Admins). */
export const useAddresses = (enabled = true) => useQuery(keys.addresses, api.addresses, { staleMs: 30_000, enabled });
export const useLibraries = () => useQuery(keys.libraries, api.libraries, { staleMs: 10_000 });
export const useHome = () => useQuery(keys.home, api.home, { staleMs: 15_000 });
export const useItem = (id: Id) => useQuery(keys.item(id), () => api.item(id));
export const useChildren = (id: Id | null) =>
  useQuery(keys.children(id ?? -1), () => api.children(id ?? -1), { enabled: id !== null });
export const useTailnet = () => useQuery(keys.tailnet, api.tailnet, { staleMs: 5_000 });
export const useInvites = () => useQuery(keys.invites, api.invites);
export const useChanges = (enabled = true) =>
  useQuery(keys.changes, () => op<ChangesListOutput>('changes.list', { limit: 50 }), { staleMs: 5_000, enabled });
export const useFacts = (id: Id) => useQuery(keys.facts(id), () => op<AgentItemDetail>('item.get', { item: id }));
export const useServerLenses = (enabled = true) =>
  useQuery(keys.serverLenses, () => op<Lenses>('lenses.list'), { staleMs: 30_000, enabled });
export const useManifest = () => useQuery(keys.manifest, manifest, { staleMs: 300_000 });
export const useCollections = () => useQuery(keys.collections, () => op<Collections>('collections.list'), { staleMs: 30_000 });

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
