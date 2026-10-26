/**
 * Where the app lives on its origin. flux serves the client at `/`, so an app
 * path ('/library/1', '/item/42', '/changes') is also its URL path. A build made
 * with `vite build --base /demo/` (the product site's live demo) lives below a
 * sub-path instead: the address bar and every link's href carry the base, while
 * the app keeps using its own paths. The router adds the base and strips it;
 * nothing else needs to know.
 *
 * Only an absolute base works: a relative one (`--base ./`) can't say where the
 * app starts once you are on a deep link, so it counts as the root.
 */

/** A Vite base as a prefix without its trailing slash: '' for '/', '/demo' for '/demo/'. */
export function basePrefix(baseUrl: string | undefined): string {
  if (!baseUrl?.startsWith('/')) return '';
  return baseUrl.replace(/\/+$/, '');
}

/** This build's base: '' unless it was built for a sub-path. */
export const APP_BASE = basePrefix(import.meta.env.BASE_URL);

/** An app path ('/item/42?t=1') as the URL path that opens it ('/demo/item/42?t=1'). */
export function appHref(path: string, base = APP_BASE): string {
  return base && path.startsWith('/') ? base + path : path;
}

/** A URL path on this origin as an app path. A path outside the base stays as it is (no screen matches it). */
export function appPath(pathname: string, base = APP_BASE): string {
  if (!base) return pathname;
  if (pathname === base) return '/';
  return pathname.startsWith(`${base}/`) ? pathname.slice(base.length) : pathname;
}
