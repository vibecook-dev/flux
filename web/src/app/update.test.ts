import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ServerInfo } from '../api/types';
import { isStaleCode, isUpdate, mayReloadOnItsOwn, noteServer, pageBuild, RELOAD_GAP_MS, reloadNow, updateState } from './update';

const info = (web_build: string | null, version = '0.1.0-alpha.6'): ServerInfo => ({
  name: 'flux',
  version,
  you: null,
  path: 'localhost',
  tailnet_name: null,
  metadata: { source: 'none' },
  https_url: null,
  web_build,
});

const doc = (content: string | null) => ({
  querySelector: () => (content === null ? null : { content }),
});

function memory(): Pick<Storage, 'getItem' | 'setItem'> & { data: Map<string, string> } {
  const data = new Map<string, string>();
  return { data, getItem: (k) => data.get(k) ?? null, setItem: (k, v) => void data.set(k, v) };
}

afterEach(() => {
  noteServer(info(null), null);
  vi.unstubAllGlobals();
});

describe('the page’s build', () => {
  it('comes from its meta tag, when it is an id', () => {
    expect(pageBuild(doc('30a0a9ba12da36ba') as unknown as Document)).toBe('30a0a9ba12da36ba');
    expect(pageBuild(doc(' 30a0a9ba ') as unknown as Document)).toBe('30a0a9ba');
    expect(pageBuild(doc(null) as unknown as Document)).toBeNull();
    expect(pageBuild(doc('') as unknown as Document)).toBeNull();
    expect(pageBuild(doc('<b>') as unknown as Document)).toBeNull();
    expect(pageBuild(null)).toBeNull();
  });
});

describe('noteServer', () => {
  it('is an update only when both builds are known and differ', () => {
    expect(isUpdate(info('b'), 'a')).toBe(true);
    expect(isUpdate(info('a'), 'a')).toBe(false);
    expect(isUpdate(info(null), 'a')).toBe(false);
    expect(isUpdate(info('b'), null)).toBe(false);
    expect(isUpdate(info('NOT AN ID'), 'a')).toBe(false);
    // A server older than the build id leaves the field out altogether.
    expect(isUpdate({} as Pick<ServerInfo, 'web_build'>, 'a')).toBe(false);
  });

  it('says what changed, and passes the answer on', () => {
    const answer = info('b');
    expect(noteServer(answer, 'a')).toBe(answer);
    expect(updateState()).toEqual({ ready: true, build: 'b', version: '0.1.0-alpha.6' });
    // The server going back to this page's build (a rollback) clears it.
    noteServer(info('a'), 'a');
    expect(updateState()).toEqual({ ready: false, build: null, version: null });
  });

  it('asks the service worker to fetch the new client', async () => {
    const update = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal('navigator', { serviceWorker: { getRegistration: vi.fn().mockResolvedValue({ update }) } });
    noteServer(info('c'), 'a');
    await vi.waitFor(() => expect(update).toHaveBeenCalledOnce());
    // Hearing the same again asks nothing more.
    noteServer(info('c'), 'a');
    await Promise.resolve();
    expect(update).toHaveBeenCalledOnce();
  });
});

describe('reloading', () => {
  it('reloads on its own at most once in RELOAD_GAP_MS', () => {
    const store = memory();
    const reload = vi.fn();
    vi.stubGlobal('location', { reload });
    expect(mayReloadOnItsOwn(Date.now(), store)).toBe(true);
    reloadNow(store);
    expect(reload).toHaveBeenCalledOnce();
    const at = Number(store.data.get('flux.reloaded-at'));
    expect(mayReloadOnItsOwn(at + 1000, store)).toBe(false);
    expect(mayReloadOnItsOwn(at + RELOAD_GAP_MS, store)).toBe(true);
    // A clock that went backwards doesn't block it forever.
    expect(mayReloadOnItsOwn(at - 60_000, store)).toBe(true);
    // No storage (private mode): it may.
    expect(mayReloadOnItsOwn(Date.now(), null)).toBe(true);
  });

  it('still reloads when storage refuses', () => {
    const reload = vi.fn();
    vi.stubGlobal('location', { reload });
    reloadNow({
      getItem: () => null,
      setItem: () => {
        throw new Error('QuotaExceededError');
      },
    });
    expect(reload).toHaveBeenCalledOnce();
  });
});

describe('isStaleCode', () => {
  it('knows each browser’s words for a module that couldn’t be fetched', () => {
    expect(isStaleCode(new TypeError('Failed to fetch dynamically imported module: https://x/assets/Connect-BM4E1TBp.js'))).toBe(true);
    expect(isStaleCode(new TypeError('error loading dynamically imported module: https://x/assets/Item-Cwk9FdSe.js'))).toBe(true);
    expect(isStaleCode(new TypeError('Importing a module script failed.'))).toBe(true);
    expect(isStaleCode(new Error('Unable to preload CSS for /assets/Player-C-5XPQEM.css'))).toBe(true);
    expect(isStaleCode('Failed to fetch dynamically imported module: x')).toBe(true);
  });

  it('is nothing else', () => {
    expect(isStaleCode(new TypeError('Cannot read properties of undefined (reading "title")'))).toBe(false);
    expect(isStaleCode(new TypeError('Failed to fetch'))).toBe(false);
    expect(isStaleCode(null)).toBe(false);
    expect(isStaleCode({ message: 'Failed to fetch dynamically imported module' })).toBe(false);
  });
});
