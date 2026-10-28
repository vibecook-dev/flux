/**
 * A ~150-line router: History API, route-level code splitting with preload on
 * hover/focus, transitions so the current screen stays up until the next one's
 * code is ready (no blank flash), and per-entry scroll restoration.
 * The <Link> component lives in Link.tsx.
 *
 * Paths here are app paths ('/item/42'). A build served below a sub-path
 * (base.ts) keeps them: the router strips the base from the address bar and
 * puts it back when it pushes a URL.
 */
import { type ComponentType, type LazyExoticComponent, lazy, startTransition, useSyncExternalStore } from 'react';
import { appHref, appPath } from './base';

export interface RouteDef {
  name: string;
  /** Pattern like "/item/:id". */
  path: string;
  load: () => Promise<{ default: ComponentType<RouteProps> }>;
  /** Render without the app chrome (player). */
  bare?: boolean;
  /**
   * Where the screen lives in the shell: `space` routes are the library itself
   * (drawn by the shell's lens), `overlay` routes open over the library, which
   * stays mounted underneath (detail, settings sheets).
   */
  layer?: 'space' | 'overlay';
}

export interface RouteProps {
  params: Record<string, string>;
  query: URLSearchParams;
}

interface Compiled extends RouteDef {
  re: RegExp;
  keys: string[];
  /**
   * The loaded component itself once its code is in, else a `lazy` one that
   * suspends while it loads.
   *
   * Rendering a never-rendered `lazy` component suspends even when its chunk
   * is already cached, and the player is a bare route, so it mounts a fresh
   * Suspense boundary whose reveal React throttles: every Play click waited
   * ~300 ms before the <video> existed, after POST /play had answered in 4 ms
   * and the first segments were already prefetched (found by the e2e test).
   */
  readonly Component: ComponentType<RouteProps> | LazyExoticComponent<ComponentType<RouteProps>>;
  /** The component once its code has loaded. */
  loaded: ComponentType<RouteProps> | null;
}

export interface Location {
  pathname: string;
  search: string;
  key: string;
}

let compiled: Compiled[] = [];

export function defineRoutes(defs: RouteDef[]): void {
  compiled = defs.map((d) => {
    const keys: string[] = [];
    const re = new RegExp(
      `^${d.path.replace(/\/:([a-z]+)/gi, (_, k: string) => {
        keys.push(k);
        return '/([^/]+)';
      })}/?$`,
    );
    // Every load (preload on intent, or React's first render) records the module.
    const load = () =>
      d.load().then((m) => {
        route.loaded = m.default;
        return m;
      });
    const lazyComponent = lazy(load);
    const route: Compiled = {
      ...d,
      load,
      re,
      keys,
      loaded: null,
      get Component() {
        return route.loaded ?? lazyComponent;
      },
    };
    return route;
  });
}

export function matchRoute(pathname: string): { route: Compiled; params: Record<string, string> } | null {
  for (const route of compiled) {
    const m = route.re.exec(pathname);
    if (m) {
      const params: Record<string, string> = {};
      route.keys.forEach((k, i) => (params[k] = decodeURIComponent(m[i + 1])));
      return { route, params };
    }
  }
  return null;
}

/** True once a route's code has loaded (rendering it won't suspend). */
export function routeLoaded(href: string): boolean {
  return matchRoute(new URL(href, location.origin).pathname)?.route.loaded != null;
}

/** Start downloading a route's code (hover/focus intent); resolves once it is in. */
export function preloadRoute(href: string): Promise<void> {
  const m = matchRoute(new URL(href, location.origin).pathname);
  // A preload that fails (its code gone after an update) is only an intent: opening the
  // route shows why (app/Crash.tsx) and reloads onto the new version.
  return m
    ? m.route.load().then(
        () => undefined,
        () => undefined,
      )
    : Promise.resolve();
}

// ── location store ──

const listeners = new Set<() => void>();
const newKey = () => Math.random().toString(36).slice(2, 10);

function readLocation(): Location {
  const state = history.state as { key?: string } | null;
  return { pathname: appPath(location.pathname), search: location.search, key: state?.key ?? 'initial' };
}

let current = readLocation();
const scrollPositions = new Map<string, number>();
let pendingRestore: number | null = null;

function emit(next: Location) {
  current = next;
  for (const l of listeners) l();
}

if (typeof window !== 'undefined') {
  history.scrollRestoration = 'manual';
  window.addEventListener('popstate', () => {
    scrollPositions.set(current.key, window.scrollY);
    const next = readLocation();
    pendingRestore = scrollPositions.get(next.key) ?? 0;
    startTransition(() => emit(next));
  });
}

export function navigate(to: string, opts: { replace?: boolean } = {}): void {
  const url = new URL(to, location.origin);
  if (url.pathname + url.search === current.pathname + current.search && !opts.replace) return;
  scrollPositions.set(current.key, window.scrollY);
  const key = newKey();
  const href = appHref(url.pathname) + url.search + url.hash;
  if (opts.replace) history.replaceState({ key }, '', href);
  else history.pushState({ key }, '', href);
  pendingRestore = opts.replace ? null : 0;
  startTransition(() => emit({ pathname: url.pathname, search: url.search, key }));
}

/**
 * Navigate once the destination's code is in (synchronously when it already
 * is), so the transition never suspends on `lazy`: the current screen stays
 * up meanwhile, as before, but a suspended transition committed late (a
 * library grid showed its first cell ~250 ms after its first page arrived
 * when the link was clicked without a long hover; found by the e2e test).
 */
export function navigateWhenLoaded(to: string, opts: { replace?: boolean } = {}): void {
  if (routeLoaded(to)) return navigate(to, opts);
  const go = () => navigate(to, opts);
  preloadRoute(to).then(go, go);
}

export function back(fallback = '/'): void {
  if (history.state && (history.state as { key?: string }).key && history.length > 1) history.back();
  else navigate(fallback, { replace: true });
}

/** Called by the outlet after a route commits. */
export function consumeScrollRestore(): void {
  if (pendingRestore === null) return;
  const y = pendingRestore;
  pendingRestore = null;
  // Twice: once now, once after layout-affecting data (virtual grids) settles.
  window.scrollTo(0, y);
  requestAnimationFrame(() => window.scrollTo(0, y));
}

/** The location now, outside React. */
export function currentLocation(): Location {
  return current;
}

/** Follow the location outside React (the live demo's bridge, src/mock/demo-bridge.ts). */
export function watchLocation(fn: (loc: Location) => void): () => void {
  const listener = () => fn(current);
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function useLocation(): Location {
  return useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    () => current,
    () => current,
  );
}

/** Update the query string without a new history entry (filters, sort). */
export function setQuery(update: Record<string, string | null>): void {
  const params = new URLSearchParams(location.search);
  for (const [k, v] of Object.entries(update)) {
    if (v === null || v === '') params.delete(k);
    else params.set(k, v);
  }
  const qs = params.toString();
  const key = current.key;
  history.replaceState({ key }, '', `${location.pathname}${qs ? `?${qs}` : ''}`);
  emit({ pathname: appPath(location.pathname), search: qs ? `?${qs}` : '', key });
}
