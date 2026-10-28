/**
 * Knowing that flux was updated under an open page.
 *
 * Every build of the client carries an id (`<meta name="flux-build">`, stamped
 * by scripts/pwa-plugin.ts) and the server reports the one it serves
 * (ServerInfo.web_build). When they differ, this page runs code the server no
 * longer has: the shell offers a reload ("flux was updated"), and a screen
 * whose code can't be fetched any more reloads onto the new version by itself
 * (Crash.tsx). When to ask the server is update-checks.ts.
 *
 * Builds without the meta tag (the dev server) never compare.
 */
import { useSyncExternalStore } from 'react';
import type { ServerInfo } from '../api/types';

const BUILD_ID = /^[0-9a-z]{1,64}$/;

/** The build a document says it is, or null (the dev server's page says nothing). */
export function pageBuild(doc: Pick<Document, 'querySelector'> | null = typeof document === 'undefined' ? null : document): string | null {
  const v = doc?.querySelector<HTMLMetaElement>('meta[name="flux-build"]')?.content.trim() ?? '';
  return BUILD_ID.test(v) ? v : null;
}

/** This page's build. */
export const PAGE_BUILD = pageBuild();

export interface UpdateState {
  /** The server serves a client other than the one this page runs. */
  ready: boolean;
  /** That client's build id, and the server's version, while `ready`. */
  build: string | null;
  version: string | null;
}

const IDLE: UpdateState = { ready: false, build: null, version: null };
let state: UpdateState = IDLE;
const listeners = new Set<() => void>();
/** When the server last said which client it serves (ms since the epoch; 0: not yet). */
let heard = 0;

/** When the server last said which client it serves (0: not yet). */
export function heardAt(): number {
  return heard;
}

/** Whether the server's client differs from the page's (both known). */
export function isUpdate(info: Pick<ServerInfo, 'web_build'>, page: string | null): boolean {
  return page !== null && typeof info.web_build === 'string' && BUILD_ID.test(info.web_build) && info.web_build !== page;
}

/** Takes in what the server said about itself; returns it, for chaining onto a fetch. */
export function noteServer(info: ServerInfo, page: string | null = PAGE_BUILD): ServerInfo {
  heard = Date.now();
  const next: UpdateState = isUpdate(info, page) ? { ready: true, build: info.web_build ?? null, version: info.version } : IDLE;
  if (next.ready !== state.ready || next.build !== state.build || next.version !== state.version) {
    state = next;
    for (const l of listeners) l();
    // Let the service worker fetch the new client now, so the reload is instant.
    if (next.ready && typeof navigator !== 'undefined') void navigator.serviceWorker?.getRegistration().then((r) => r?.update()).catch(() => undefined);
  }
  return info;
}

export function updateState(): UpdateState {
  return state;
}

export function useUpdate(): UpdateState {
  return useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    () => state,
    () => state,
  );
}

// ── reloading ──

const RELOADED_AT = 'flux.reloaded-at';
/** A reload flux did on its own counts this long: a second one inside it would be a loop. */
export const RELOAD_GAP_MS = 30_000;

type Store = Pick<Storage, 'getItem' | 'setItem'>;

function session(): Store | null {
  try {
    return sessionStorage;
  } catch {
    return null;
  }
}

/** Whether flux may reload the page on its own now (not again within RELOAD_GAP_MS of the last time). */
export function mayReloadOnItsOwn(now = Date.now(), store: Store | null = session()): boolean {
  const at = Number(store?.getItem(RELOADED_AT) ?? 0);
  return !(at > 0 && now - at >= 0 && now - at < RELOAD_GAP_MS);
}

/** Reload onto the client the server has now (the service worker answers pages from the network). */
export function reloadNow(store: Store | null = session()): void {
  try {
    store?.setItem(RELOADED_AT, String(Date.now()));
  } catch {
    // private mode: no loop guard, which only matters for reloads flux starts itself
  }
  location.reload();
}

// ── code that isn't there any more ──

/** Browsers' words for "this module couldn't be fetched" (Chrome, Firefox, Safari, and Vite's CSS preload). */
const STALE_CODE = /dynamically imported module|Importing a module script failed|error loading dynamically imported module|Unable to preload CSS/i;

/** Whether an error is a screen's code that couldn't be fetched (typically: flux was updated, its old files are gone). */
export function isStaleCode(err: unknown): boolean {
  const text = err instanceof Error ? `${err.name}: ${err.message}` : typeof err === 'string' ? err : '';
  return STALE_CODE.test(text);
}
