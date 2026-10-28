/**
 * The service worker's decisions, apart from the worker (sw.ts) so they can be
 * tested: which requests it answers, what it keeps, and when it steps aside.
 *
 * - **Pages** (navigations) come from the network, so a page always opens on the
 *   client the server has now. The last page flux served is kept, so when flux
 *   can't be reached the app still opens, and says so, instead of the browser's
 *   error page.
 * - **The client's files** (`/assets/*`, named by their content, so a name never
 *   changes meaning) come from the cache, else the network. A new build's files
 *   are fetched when its worker installs, and the previous build's stay: a page
 *   left open across an upgrade keeps finding the code it was built with.
 * - **Everything else** (the API, media, art, the worker itself) is never touched.
 *
 * When the address stops serving flux (another program on the same port), the
 * worker removes itself and its caches.
 */

/** Caches of one build's files: `flux-app-<build>`. */
export const CACHE_PREFIX = 'flux-app-';
/** The kept page and the list of kept builds. */
export const META_CACHE = 'flux-meta';
/** Builds whose files stay: this one and the one before. */
export const KEEP_BUILDS = 2;

export type Route = 'page' | 'asset' | 'skip';

/** What the worker does with a request, for a worker whose scope is `scope` (an absolute URL ending in `/`). */
export function routeOf(req: { url: string; method: string; mode: string }, scope: string): Route {
  if (req.method !== 'GET') return 'skip';
  let url: URL;
  let base: URL;
  try {
    url = new URL(req.url);
    base = new URL(scope);
  } catch {
    return 'skip';
  }
  if (url.origin !== base.origin || !url.pathname.startsWith(base.pathname)) return 'skip';
  const path = `/${url.pathname.slice(base.pathname.length)}`;
  if (/^\/(api|media)(\/|$)/.test(path) || path.startsWith('/.well-known/')) return 'skip';
  if (req.mode === 'navigate') return 'page';
  if (path.startsWith('/assets/')) return 'asset';
  return 'skip';
}

/** The cache holding `build`'s files. */
export function cacheName(build: string): string {
  return CACHE_PREFIX + build;
}

/** The builds kept once `build` is the current one, oldest first. */
export function keptBuilds(previous: unknown, build: string): string[] {
  const list = Array.isArray(previous) ? previous.filter((b): b is string => typeof b === 'string' && b !== build) : [];
  return [...list, build].slice(-KEEP_BUILDS);
}

/** flux's own caches that hold builds no longer kept (other caches on the origin are left alone). */
export function staleCaches(names: readonly string[], keep: readonly string[]): string[] {
  const wanted = new Set(keep.map(cacheName));
  return names.filter((n) => n.startsWith(CACHE_PREFIX) && !wanted.has(n));
}

/** The build a page says it is (`<meta name="flux-build" content="…">`, scripts/pwa-plugin.ts), or null. */
export function buildOfHtml(html: string): string | null {
  const m = /<meta\s+name="flux-build"\s+content="([0-9a-z]{1,64})"\s*\/?>/i.exec(html);
  return m ? m[1] : null;
}

/** What a page from the network means for the kept copy: keep it, leave things as they are, or (not flux any more) remove the worker. */
export function verdictOnPage(res: { ok: boolean; status: number; contentType: string | null }, html: string | null): 'keep' | 'ignore' | 'forget' {
  if (!res.ok || res.status !== 200 || !(res.contentType ?? '').toLowerCase().startsWith('text/html') || html === null) return 'ignore';
  return buildOfHtml(html) ? 'keep' : 'forget';
}
