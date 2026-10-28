import { describe, expect, it } from 'vitest';
import { buildOfHtml, CACHE_PREFIX, cacheName, keptBuilds, META_CACHE, routeOf, staleCaches, verdictOnPage } from './core';

const SCOPE = 'https://flux.tail1234.ts.net:8096/';
const get = (url: string, mode = 'cors') => ({ url, method: 'GET', mode });

describe('routeOf', () => {
  it('answers pages and the client’s files, nothing else', () => {
    expect(routeOf(get(`${SCOPE}`, 'navigate'), SCOPE)).toBe('page');
    expect(routeOf(get(`${SCOPE}item/42?t=1`, 'navigate'), SCOPE)).toBe('page');
    expect(routeOf(get(`${SCOPE}assets/index-BNz0hwrt.js`), SCOPE)).toBe('asset');
    expect(routeOf(get(`${SCOPE}assets/jost-latin-wght-normal-ObQm3Zd1.woff2`), SCOPE)).toBe('asset');
  });

  it('never touches the API, media, art or anything that isn’t a GET', () => {
    for (const path of ['api/v1/server', 'api/v1/art/12/poster?w=300', 'api', 'media/7/master.m3u8', 'media/7/seg-1.m4s', 'media', '.well-known/oauth-authorization-server']) {
      expect(routeOf(get(`${SCOPE}${path}`), SCOPE), path).toBe('skip');
      // Opening one of them in the address bar is still the server's to answer.
      expect(routeOf(get(`${SCOPE}${path}`, 'navigate'), SCOPE), path).toBe('skip');
    }
    expect(routeOf({ url: `${SCOPE}assets/a.js`, method: 'POST', mode: 'cors' }, SCOPE)).toBe('skip');
    expect(routeOf({ url: `${SCOPE}item/1`, method: 'HEAD', mode: 'navigate' }, SCOPE)).toBe('skip');
  });

  it('leaves the worker’s own files, the manifest and icons to the network', () => {
    for (const path of ['sw.js', 'manifest.webmanifest', 'favicon.svg', 'icons/icon-192.png']) expect(routeOf(get(`${SCOPE}${path}`), SCOPE), path).toBe('skip');
  });

  it('stays inside its origin and scope', () => {
    expect(routeOf(get('https://image.tmdb.org/t/p/w500/x.jpg'), SCOPE)).toBe('skip');
    expect(routeOf(get('http://flux.tail1234.ts.net:8096/assets/a.js'), SCOPE)).toBe('skip');
    expect(routeOf(get('https://flux.tail1234.ts.net:8096/elsewhere/assets/a.js'), `${SCOPE}demo/`)).toBe('skip');
    expect(routeOf(get('https://flux.tail1234.ts.net:8096/demo/assets/a.js'), `${SCOPE}demo/`)).toBe('asset');
    expect(routeOf(get('https://flux.tail1234.ts.net:8096/demo/api/v1/server'), `${SCOPE}demo/`)).toBe('skip');
    expect(routeOf(get('not a url'), SCOPE)).toBe('skip');
  });
});

describe('the kept builds', () => {
  it('keeps this build and the one before, newest last', () => {
    expect(keptBuilds(null, 'a')).toEqual(['a']);
    expect(keptBuilds(['a'], 'b')).toEqual(['a', 'b']);
    expect(keptBuilds(['a', 'b'], 'c')).toEqual(['b', 'c']);
    // The same build activating again (a reinstall) doesn't push the previous one out.
    expect(keptBuilds(['a', 'b'], 'b')).toEqual(['a', 'b']);
    // Whatever was stored, only build ids count.
    expect(keptBuilds({ nope: true }, 'a')).toEqual(['a']);
    expect(keptBuilds(['a', 3, null], 'b')).toEqual(['a', 'b']);
  });

  it('deletes only flux’s own caches of builds no longer kept', () => {
    const names = [cacheName('a'), cacheName('b'), cacheName('c'), META_CACHE, 'someone-elses-cache'];
    expect(staleCaches(names, ['b', 'c'])).toEqual([`${CACHE_PREFIX}a`]);
    expect(staleCaches(names, ['c'])).toEqual([`${CACHE_PREFIX}a`, `${CACHE_PREFIX}b`]);
  });
});

describe('pages', () => {
  const page = (build: string) => `<!doctype html><html><head><title>flux</title>\n  <meta name="flux-build" content="${build}" />\n  </head><body></body></html>`;

  it('reads the build a page says it is', () => {
    expect(buildOfHtml(page('30a0a9ba12da36ba'))).toBe('30a0a9ba12da36ba');
    expect(buildOfHtml('<html><head><title>Jellyfin</title></head></html>')).toBeNull();
    expect(buildOfHtml('<meta name="flux-build" content="">')).toBeNull();
    expect(buildOfHtml('<meta name="flux-build" content="<script>">')).toBeNull();
  });

  it('keeps flux’s pages, ignores failures, and steps aside for another program’s', () => {
    const html = { ok: true, status: 200, contentType: 'text/html; charset=utf-8' };
    expect(verdictOnPage(html, page('abc'))).toBe('keep');
    expect(verdictOnPage(html, '<html><title>Jellyfin</title></html>')).toBe('forget');
    // An error page (a proxy's 502, flux restarting) says nothing about who lives here.
    expect(verdictOnPage({ ok: false, status: 502, contentType: 'text/html' }, '<h1>Bad Gateway</h1>')).toBe('ignore');
    expect(verdictOnPage({ ok: true, status: 200, contentType: 'application/json' }, null)).toBe('ignore');
    expect(verdictOnPage({ ok: true, status: 206, contentType: 'text/html' }, page('abc'))).toBe('ignore');
    expect(verdictOnPage({ ok: true, status: 200, contentType: null }, null)).toBe('ignore');
  });
});
