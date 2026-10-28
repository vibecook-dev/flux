import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ServerInfo } from '../api/types';

vi.mock(import('../api/client'), async (importOriginal) => {
  const actual = await importOriginal();
  return { ...actual, api: { ...actual.api, server: vi.fn() } };
});

const info = (web_build: string | null): ServerInfo => ({
  name: 'flux',
  version: '0.1.0-alpha.6',
  you: null,
  path: 'localhost',
  tailnet_name: null,
  metadata: { source: 'none' },
  https_url: null,
  web_build,
});

/** Fresh modules for a page of build `page`, with `reload` in place of location.reload. */
async function load(page: string | null) {
  vi.resetModules();
  vi.stubGlobal('document', { querySelector: () => (page ? { content: page } : null) });
  const reload = vi.fn();
  vi.stubGlobal('location', { reload });
  const { api } = await import('../api/client');
  const checks = await import('./update-checks');
  return { server: vi.mocked(api.server), reload, checks };
}

afterEach(() => vi.unstubAllGlobals());

describe('recoverFromStaleCode', () => {
  it('reloads onto the new version when flux was updated', async () => {
    const { server, reload, checks } = await load('aaaa');
    server.mockResolvedValue(info('bbbb'));
    expect(await checks.recoverFromStaleCode()).toBe('reloading');
    expect(reload).toHaveBeenCalledOnce();
  });

  it('doesn’t reload into a loop', async () => {
    const { server, reload, checks } = await load('aaaa');
    vi.stubGlobal('sessionStorage', { getItem: () => String(Date.now() - 2000), setItem: vi.fn() });
    server.mockResolvedValue(info('bbbb'));
    expect(await checks.recoverFromStaleCode()).toBe('stuck');
    expect(reload).not.toHaveBeenCalled();
  });

  it('says so when the code is missing but flux wasn’t updated', async () => {
    const { server, reload, checks } = await load('aaaa');
    server.mockResolvedValue(info('aaaa'));
    expect(await checks.recoverFromStaleCode()).toBe('stuck');
    expect(reload).not.toHaveBeenCalled();
  });

  it('says so when flux doesn’t answer', async () => {
    const { server, reload, checks } = await load('aaaa');
    server.mockRejectedValue(new TypeError('Failed to fetch'));
    expect(await checks.recoverFromStaleCode()).toBe('offline');
    expect(reload).not.toHaveBeenCalled();
  });

  it('never reloads a page that doesn’t know its own build (the dev server)', async () => {
    const { server, reload, checks } = await load(null);
    server.mockResolvedValue(info('bbbb'));
    expect(await checks.recoverFromStaleCode()).toBe('stuck');
    expect(reload).not.toHaveBeenCalled();
  });
});
