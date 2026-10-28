import { describe, expect, it, vi } from 'vitest';
import type { RouteProps } from './router';

// router.ts reads the location at import; give it a minimal one (vitest runs in node).
vi.stubGlobal('history', { state: null });
vi.stubGlobal('location', { pathname: '/', search: '', origin: 'http://flux.test' });
const { defineRoutes, matchRoute, navigateWhenLoaded, preloadRoute, routeLoaded } = await import('./router');

function Fake(_props: RouteProps) {
  return null;
}

describe('route components', () => {
  it('render the loaded component directly once its code is in (no lazy suspend, no ~300 ms reveal throttle on Play)', async () => {
    let calls = 0;
    defineRoutes([
      {
        name: 'play',
        path: '/play/:id',
        bare: true,
        load: () => {
          calls += 1;
          return Promise.resolve({ default: Fake });
        },
      },
    ]);
    const m = matchRoute('/play/7');
    expect(m?.params).toEqual({ id: '7' });
    // Before the code is loaded: the lazy wrapper.
    expect(m?.route.Component).not.toBe(Fake);
    expect(routeLoaded('/play/1')).toBe(false);
    void preloadRoute('/play/0');
    await new Promise((r) => setTimeout(r, 0));
    expect(calls).toBe(1);
    expect(routeLoaded('/play/1')).toBe(true);
    expect(m?.route.Component).toBe(Fake);
    expect(matchRoute('/play/8')?.route.Component).toBe(Fake);
  });

  it('navigates only once the destination code is in', async () => {
    const pushed: string[] = [];
    vi.stubGlobal('history', { state: null, pushState: (_s: unknown, _t: string, url: string) => pushed.push(url), replaceState: () => undefined });
    vi.stubGlobal('window', { scrollY: 0 });
    let release: (m: { default: typeof Fake }) => void = () => undefined;
    defineRoutes([{ name: 'library', path: '/library/:id', load: () => new Promise((r) => (release = r)) }]);
    navigateWhenLoaded('/library/3');
    await new Promise((r) => setTimeout(r, 0));
    expect(pushed).toEqual([]);
    release({ default: Fake });
    await new Promise((r) => setTimeout(r, 0));
    expect(pushed).toEqual(['/library/3']);
    // Already loaded: synchronous.
    navigateWhenLoaded('/library/4');
    expect(pushed).toEqual(['/library/3', '/library/4']);
  });
});

describe('preloadRoute', () => {
  it('settles when the code can’t be fetched (flux updated under the page): only an intent', async () => {
    defineRoutes([
      {
        name: 'connect',
        path: '/connect',
        load: () => Promise.reject(new TypeError('Failed to fetch dynamically imported module: http://flux.test/assets/Connect-old.js')),
      },
    ]);
    await expect(preloadRoute('/connect')).resolves.toBeUndefined();
    expect(routeLoaded('/connect')).toBe(false);
  });
});
