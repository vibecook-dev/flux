import { afterEach, describe, expect, it, vi } from 'vitest';
import { reachable, secureTarget } from './secure';

const here = (url: string) => {
  const u = new URL(url);
  return { protocol: u.protocol, pathname: u.pathname, search: u.search, hash: u.hash };
};
const server = { https_url: 'https://flux.tail1234.ts.net:8096', path: 'tailnet-direct' };

describe('secureTarget', () => {
  it('keeps the route and moves to the HTTPS address', () => {
    const t = secureTarget(server, here('http://100.64.0.1:8096/library/3?q=blade#top'), false);
    expect(t?.href).toBe('https://flux.tail1234.ts.net:8096/library/3?q=blade#top');
    expect(secureTarget({ ...server, path: 'tailnet' }, here('http://flux.tail1234.ts.net:8096/'), false)?.href).toBe('https://flux.tail1234.ts.net:8096/');
  });

  it('stays on a secure page, off the tailnet, or without HTTPS', () => {
    expect(secureTarget(server, here('http://localhost:8096/'), true)).toBeNull();
    expect(secureTarget(server, here('https://flux.tail1234.ts.net:8096/'), true)).toBeNull();
    expect(secureTarget({ ...server, path: 'lan' }, here('http://192.168.1.5:8096/'), false)).toBeNull();
    expect(secureTarget({ ...server, path: 'localhost' }, here('http://127.0.0.1:8096/'), false)).toBeNull();
    expect(secureTarget({ ...server, https_url: null }, here('http://100.64.0.1:8096/'), false)).toBeNull();
    expect(secureTarget({ ...server, https_url: 'http://flux.tail1234.ts.net:8096' }, here('http://100.64.0.1:8096/'), false)).toBeNull();
    expect(secureTarget({ ...server, https_url: 'not a url' }, here('http://100.64.0.1:8096/'), false)).toBeNull();
  });
});

describe('reachable', () => {
  afterEach(() => vi.unstubAllGlobals());

  it('is an opaque answer from the HTTPS address, and false when it fails or hangs', async () => {
    const fetch = vi.fn().mockResolvedValue(new Response(null));
    vi.stubGlobal('fetch', fetch);
    expect(await reachable('https://flux.tail1234.ts.net:8096')).toBe(true);
    const [url, init] = fetch.mock.calls[0] as [URL, RequestInit];
    expect(url.href).toBe('https://flux.tail1234.ts.net:8096/api/v1/server');
    expect(init.mode).toBe('no-cors');

    vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new TypeError('Failed to fetch')));
    expect(await reachable('https://flux.tail1234.ts.net:8096')).toBe(false);

    const hang = vi.fn((_: URL, init: RequestInit) => new Promise((_, reject) => init.signal?.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError')))));
    vi.stubGlobal('fetch', hang);
    expect(await reachable('https://flux.tail1234.ts.net:8096', 20)).toBe(false);
  });
});
