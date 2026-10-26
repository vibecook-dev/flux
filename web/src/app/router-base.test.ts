import { describe, expect, it, vi } from 'vitest';
import { appHref, appPath, basePrefix } from './base';

describe('base paths', () => {
  it('turn a Vite base into a prefix', () => {
    expect(basePrefix('/')).toBe('');
    expect(basePrefix('/demo/')).toBe('/demo');
    expect(basePrefix('/flux/demo/')).toBe('/flux/demo');
    expect(basePrefix('/demo')).toBe('/demo');
    // A relative base can't place deep links, so it counts as the root.
    expect(basePrefix('./')).toBe('');
    expect(basePrefix(undefined)).toBe('');
  });

  it('add the base to app paths and strip it from URL paths', () => {
    expect(appHref('/item/42?t=1', '/demo')).toBe('/demo/item/42?t=1');
    expect(appHref('/', '/demo')).toBe('/demo/');
    expect(appHref('/item/42', '')).toBe('/item/42');
    expect(appPath('/demo/item/42', '/demo')).toBe('/item/42');
    expect(appPath('/demo/', '/demo')).toBe('/');
    expect(appPath('/demo', '/demo')).toBe('/');
    expect(appPath('/flux/demo/library/1', '/flux/demo')).toBe('/library/1');
    // Not below the base (and not a lookalike prefix): left alone, so no screen matches it.
    expect(appPath('/demolition', '/demo')).toBe('/demolition');
    expect(appPath('/item/42', '')).toBe('/item/42');
  });
});

describe('the router below a sub-path', () => {
  it('reads app paths from the address bar and pushes URLs with the base', async () => {
    vi.stubEnv('BASE_URL', '/demo/');
    const pushed: string[] = [];
    const replaced: string[] = [];
    const loc = { pathname: '/demo/library/3', search: '?sort=-year', origin: 'http://flux.test' };
    vi.stubGlobal('location', loc);
    vi.stubGlobal('history', {
      state: null,
      pushState: (_s: unknown, _t: string, url: string) => pushed.push(url),
      replaceState: (_s: unknown, _t: string, url: string) => replaced.push(url),
    });
    vi.stubGlobal('window', { scrollY: 0, addEventListener: () => undefined });
    vi.resetModules();
    const { currentLocation, defineRoutes, matchRoute, navigate, setQuery } = await import('./router');
    const { appHref: href } = await import('./base');
    defineRoutes([
      { name: 'library', path: '/library/:id', layer: 'space', load: () => Promise.resolve({ default: () => null }) },
      { name: 'item', path: '/item/:id', layer: 'overlay', load: () => Promise.resolve({ default: () => null }) },
    ]);

    expect(currentLocation().pathname).toBe('/library/3');
    expect(matchRoute(currentLocation().pathname)?.params).toEqual({ id: '3' });
    expect(href('/item/7')).toBe('/demo/item/7');

    navigate('/item/7?from=wall');
    expect(pushed).toEqual(['/demo/item/7?from=wall']);
    expect(currentLocation()).toMatchObject({ pathname: '/item/7', search: '?from=wall' });

    // The query of the page you're on (filters, sort) keeps its URL path.
    loc.pathname = '/demo/library/3';
    loc.search = '';
    setQuery({ sort: 'year' });
    expect(replaced).toEqual(['/demo/library/3?sort=year']);
    expect(currentLocation()).toMatchObject({ pathname: '/library/3', search: '?sort=year' });
    vi.unstubAllEnvs();
  });
});
