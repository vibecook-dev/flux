/**
 * When to ask the server which client it serves (update.ts says what for):
 * when the event stream comes back (flux restarted, maybe as a new version),
 * when the page comes back into view after a while, every half hour while it
 * is in view, and when a screen's code fails to load.
 */
import { api } from '../api/client';
import { type EventsState, watchEventsState } from '../api/events';
import { keys } from '../api/queries';
import { setQueryData } from '../api/query';
import type { ServerInfo } from '../api/types';
import { heardAt, mayReloadOnItsOwn, noteServer, reloadNow, updateState } from './update';

/** Ask again this often while the page is in view. */
const EVERY_MS = 30 * 60_000;
/** Back in view after at least this long: ask. */
const AWAY_MS = 60_000;
/** The server said which client it serves this recently: no need to ask again. */
const JUST_HEARD_MS = 2000;

let askedAt = 0;

/** What the server says about itself now (also refreshes the shared query). */
export async function askServer(): Promise<ServerInfo> {
  askedAt = Date.now();
  const info = noteServer(await api.server());
  setQueryData(keys.server, info);
  return info;
}

const quietly = () => void askServer().catch(() => undefined);

/** Starts asking; returns the stop function. */
export function startUpdateChecks(): () => void {
  // The stream opening may be flux back from a restart (or up at last, for a page that
  // opened while it was down): ask, unless the server has only just said.
  const unwatch = watchEventsState((s: EventsState) => {
    if (s === 'open' && Date.now() - heardAt() > JUST_HEARD_MS) quietly();
  });
  const onVisible = () => {
    if (document.visibilityState === 'visible' && Date.now() - askedAt >= AWAY_MS) quietly();
  };
  document.addEventListener('visibilitychange', onVisible);
  const timer = setInterval(() => {
    if (document.visibilityState === 'visible') quietly();
  }, EVERY_MS);
  // Vite's word that a screen's code (or its CSS) didn't load: likely a new version.
  window.addEventListener('vite:preloadError', quietly);
  return () => {
    unwatch();
    document.removeEventListener('visibilitychange', onVisible);
    clearInterval(timer);
    window.removeEventListener('vite:preloadError', quietly);
  };
}

export type Recovery = 'reloading' | 'offline' | 'stuck';

/**
 * A screen's code didn't arrive. When flux was updated, reload onto the new
 * version, unless flux just did that (a loop): says what happened.
 */
export async function recoverFromStaleCode(): Promise<Recovery> {
  try {
    await askServer();
  } catch {
    return 'offline';
  }
  if (updateState().ready && mayReloadOnItsOwn()) {
    reloadNow();
    return 'reloading';
  }
  return 'stuck';
}
