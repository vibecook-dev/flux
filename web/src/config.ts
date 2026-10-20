/**
 * Backend selection, decided at build time from `VITE_FLUX_API`:
 *
 * - unset or `mock` → the in-browser mock backend (MSW, `src/mock/`).
 * - `/`             → a real flux-server on the same origin (it serves `web/dist`).
 * - `http://host:port` → a real flux-server elsewhere. Under `pnpm dev` the Vite
 *   dev server proxies `/api` and `/media` there (no CORS needed). A production
 *   build would call it cross-origin, which flux refuses by design: it sends no
 *   CORS headers, so the `X-Flux-Request` header its CSRF guard wants can't be
 *   added from another origin. Serve the build from flux itself (`/`).
 *
 * Because this is a build-time constant, the mock chunk is dropped from builds
 * that target a real server.
 */
const raw = import.meta.env.VITE_FLUX_API;

/**
 * True when the in-browser mock backend answers every API call. Written so the
 * bundler can constant-fold it (Vite inlines VITE_* values), which removes the
 * mock chunk from real-server builds.
 */
export const MOCK = !import.meta.env.VITE_FLUX_API || import.meta.env.VITE_FLUX_API === 'mock';

function apiBase(): string {
  if (MOCK || raw === undefined || raw === '/') return '';
  if (import.meta.env.DEV && /^https?:\/\//.test(raw)) return ''; // proxied by vite.config.ts
  return raw.replace(/\/+$/, '');
}

/** Prefix for every API and media path; '' means same origin. */
export const API_BASE = apiBase();

/** Resolve a server-relative URL (`/media/…`, `/api/v1/art/…`) against the API base. */
export function resolveUrl(url: string): string {
  if (/^(https?:|data:|blob:)/.test(url)) return url;
  return API_BASE + url;
}
