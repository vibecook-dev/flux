/**
 * Typed fetch client for every route in flux-core's api.rs table, plus the
 * web-only routes this client proposes (invites; see types.ts).
 *
 * `createFluxClient` is pure (base URL + fetch injected) so tests can run it
 * against the MSW mock in Node; the app uses the `api` singleton.
 */
import { API_BASE, resolveUrl } from '../config';
import type {
  ArtRef,
  CreateInvite,
  CreateLibrary,
  FolderPage,
  FolderSort,
  Home,
  Id,
  Invite,
  ItemDetail,
  ItemFilter,
  ItemSummary,
  Library,
  LibrarySort,
  MarkWatched,
  Page,
  PlanPreview,
  PlayRequest,
  PlayResponse,
  ProgressReport,
  ScanStatus,
  ServerInfo,
  SessionDiagnostics,
  TailnetConnect,
  TailnetInfo,
  TailnetMode,
} from './types';

/**
 * Sent on every fetch to `/api/`. flux rejects POST/PUT/PATCH/DELETE under
 * `/api/` without it (403), and never answers CORS preflights, so a page on
 * another site can't add it: this is flux's CSRF guard (contract in
 * docs/prototype/web.md, "Phase D"). GETs carry it too; that is harmless.
 */
export const FLUX_REQUEST_HEADER = 'x-flux-request';

/** A non-2xx response, with the server's message when it sent one. */
export class FluxApiError extends Error {
  readonly status: number;
  readonly path: string;

  constructor(status: number, path: string, message: string) {
    super(message);
    this.name = 'FluxApiError';
    this.status = status;
    this.path = path;
  }

  /** 421: the server doesn't answer to the host name this page used (DNS-rebinding guard). */
  get misdirected(): boolean {
    return this.status === 421;
  }
}

/** The host the browser used to reach the API, for messages ("flux.local:8096"). */
function apiHost(base: string): string | null {
  try {
    if (/^https?:\/\//.test(base)) return new URL(base).host;
    return typeof location === 'undefined' ? null : location.host;
  } catch {
    return null;
  }
}

/**
 * The message for a failed API call. 421 and 403 come from flux's request
 * guards, not from a missing title, so they say what to do instead of
 * showing the server's terse plain-text reason.
 */
export function apiErrorMessage(status: number, method: string, path: string, serverText: string, host: string | null): string {
  const text = serverText.length > 300 ? `${serverText.slice(0, 300)}…` : serverText;
  if (status === 421) {
    const where = host ? `the address ${host}` : 'this address';
    return `flux doesn’t answer to ${where}. Open it at localhost (on the server itself), at the server’s own name or IP, or at its Tailscale name. To use another name, start flux with --allow-host.`;
  }
  if (status === 403) {
    // The cross-site guard (missing X-Flux-Request, or a foreign Origin) vs. a role check.
    if (/x-flux-request|origin|cross-site|csrf/i.test(text)) {
      return `flux refused this request because it didn’t come from flux’s own page (${text}). Reload flux from its own address and try again.`;
    }
    return text ? `Not allowed: ${text}` : 'flux refused this request (HTTP 403): your account may not be allowed to do this.';
  }
  return text || `${method} ${path} failed with HTTP ${status}`;
}

/**
 * The message to show instead of "can't reach the server" when flux answered
 * but its request guard refused the page: a 421 (unknown host name) or a 403
 * from the cross-site check. `null` for anything else.
 */
export function guardRefusal(err: unknown): string | null {
  return err instanceof FluxApiError && (err.status === 421 || err.status === 403) ? err.message : null;
}

export interface FluxClientOptions {
  /** '' for same origin, or e.g. "http://flux.tail1234.ts.net:8096". */
  baseUrl: string;
  fetch?: typeof fetch;
  /** Awaited before every request (the mock backend uses it to finish booting). */
  ready?: () => Promise<void>;
}

export interface ItemsQuery {
  cursor?: string | null;
  limit?: number;
  sort?: LibrarySort;
  filter?: ItemFilter | null;
}

export interface FolderQuery {
  /** A `FolderRef.folder`; omitted or empty = the library's top. */
  folder?: string | null;
  cursor?: string | null;
  limit?: number;
  sort?: FolderSort;
}

/** Pick an image width bucket so `?w=` URLs repeat (and cache) across screens. */
export function artWidthBucket(cssPx: number, dpr = 1): number {
  const want = cssPx * dpr;
  const buckets = [160, 240, 320, 480, 640, 960, 1280, 1920, 2560];
  return buckets.find((b) => b >= want) ?? buckets[buckets.length - 1];
}

/** `ArtRef.url` with the `?w=` resize hint the server understands. */
export function artUrl(ref: ArtRef, width: number): string {
  const url = resolveUrl(ref.url);
  return `${url}${url.includes('?') ? '&' : '?'}w=${width}`;
}

/** `srcset` for an image displayed at `cssPx` wide (1× and 2× buckets). */
export function artSrcSet(ref: ArtRef, cssPx: number): string {
  const one = artWidthBucket(cssPx, 1);
  const two = artWidthBucket(cssPx, 2);
  return one === two ? `${artUrl(ref, one)} ${one}w` : `${artUrl(ref, one)} ${one}w, ${artUrl(ref, two)} ${two}w`;
}

export function createFluxClient(opts: FluxClientOptions) {
  const doFetch = opts.fetch ?? ((input: RequestInfo | URL, init?: RequestInit) => fetch(input, init));
  const base = opts.baseUrl.replace(/\/+$/, '');

  async function request<T>(method: string, path: string, body?: unknown, init?: RequestInit): Promise<T> {
    if (opts.ready) await opts.ready();
    const headers: Record<string, string> = { accept: 'application/json', [FLUX_REQUEST_HEADER]: '1' };
    if (body !== undefined) headers['content-type'] = 'application/json';
    const res = await doFetch(base + path, {
      ...init,
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    if (!res.ok) {
      let text = '';
      try {
        text = (await res.text()).trim();
      } catch {
        // keep the generic message
      }
      throw new FluxApiError(res.status, path, apiErrorMessage(res.status, method, path, text, apiHost(base)));
    }
    if (res.status === 204) return undefined as T;
    return (await res.json()) as T;
  }

  const get = <T>(path: string, signal?: AbortSignal) => request<T>('GET', path, undefined, { signal });
  const post = <T>(path: string, body?: unknown, init?: RequestInit) => request<T>('POST', path, body, init);

  return {
    /** GET /api/v1/server */
    server: () => get<ServerInfo>('/api/v1/server'),
    /** GET /api/v1/libraries */
    libraries: () => get<Library[]>('/api/v1/libraries'),
    /** POST /api/v1/libraries */
    createLibrary: (body: CreateLibrary) => post<Library>('/api/v1/libraries', body),
    /** POST /api/v1/libraries/{id}/scan */
    scanLibrary: (id: Id) => post<ScanStatus>(`/api/v1/libraries/${id}/scan`),
    /** GET /api/v1/libraries/{id}/items?cursor=&limit=&sort=&filter= */
    libraryItems: (id: Id, q: ItemsQuery = {}, signal?: AbortSignal) => {
      const params = new URLSearchParams();
      if (q.cursor) params.set('cursor', q.cursor);
      if (q.limit !== undefined) params.set('limit', String(q.limit));
      if (q.sort) params.set('sort', q.sort);
      if (q.filter) params.set('filter', q.filter);
      const qs = params.toString();
      return get<Page<ItemSummary>>(`/api/v1/libraries/${id}/items${qs ? `?${qs}` : ''}`, signal);
    },
    /** GET /api/v1/collections/{id}/items?cursor=&limit=&sort=&filter= (a collection's titles, across libraries). */
    collectionItems: (id: Id, q: ItemsQuery = {}, signal?: AbortSignal) => {
      const params = new URLSearchParams();
      if (q.cursor) params.set('cursor', q.cursor);
      if (q.limit !== undefined) params.set('limit', String(q.limit));
      if (q.sort) params.set('sort', q.sort);
      if (q.filter) params.set('filter', q.filter);
      const qs = params.toString();
      return get<Page<ItemSummary>>(`/api/v1/collections/${id}/items${qs ? `?${qs}` : ''}`, signal);
    },
    /** GET /api/v1/libraries/{id}/folders?folder=&cursor=&limit=&sort= (a videos library's folders). */
    libraryFolders: (id: Id, q: FolderQuery = {}, signal?: AbortSignal) => {
      const params = new URLSearchParams();
      if (q.folder) params.set('folder', q.folder);
      if (q.cursor) params.set('cursor', q.cursor);
      if (q.limit !== undefined) params.set('limit', String(q.limit));
      if (q.sort) params.set('sort', q.sort);
      const qs = params.toString();
      return get<FolderPage>(`/api/v1/libraries/${id}/folders${qs ? `?${qs}` : ''}`, signal);
    },
    /** GET /api/v1/home */
    home: () => get<Home>('/api/v1/home'),
    /** GET /api/v1/items/{id} */
    item: (id: Id, signal?: AbortSignal) => get<ItemDetail>(`/api/v1/items/${id}`, signal),
    /** GET /api/v1/items/{id}/children */
    children: (id: Id, signal?: AbortSignal) => get<ItemSummary[]>(`/api/v1/items/${id}/children`, signal),
    /** POST /api/v1/items/{id}/watched → 204 (a show or season marks every episode). */
    markWatched: (id: Id, watched: boolean) => post<void>(`/api/v1/items/${id}/watched`, { watched } satisfies MarkWatched),
    /** GET /api/v1/search?q= */
    search: (q: string, signal?: AbortSignal) => get<ItemSummary[]>(`/api/v1/search?q=${encodeURIComponent(q)}`, signal),
    /** POST /api/v1/play */
    play: (req: PlayRequest) => post<PlayResponse>('/api/v1/play', req),
    /** POST /api/v1/plan: what /play would do for these caps (no session). */
    plan: (req: PlayRequest) => post<PlanPreview>('/api/v1/plan', req),
    /**
     * POST /api/v1/sessions/{id}/progress → 204. `keepalive` lets the final
     * report survive the page or player being torn down.
     */
    reportProgress: (sessionId: string, report: ProgressReport, keepalive = false) =>
      post<void>(`/api/v1/sessions/${encodeURIComponent(sessionId)}/progress`, report, { keepalive }),
    /** GET /api/v1/sessions/{id}/diagnostics */
    diagnostics: (sessionId: string) =>
      get<SessionDiagnostics>(`/api/v1/sessions/${encodeURIComponent(sessionId)}/diagnostics`),
    /** URL of GET /api/v1/events (Server-Sent Events); see events.ts. */
    eventsUrl: () => `${base}/api/v1/events`,

    /** GET /api/v1/tailnet */
    tailnet: () => get<TailnetInfo>('/api/v1/tailnet'),
    /**
     * POST /api/v1/tailnet/connect: `embedded` starts flux's own node (then
     * `needs_login` with an auth URL), `system` uses this machine's
     * tailscaled, `off` disconnects.
     */
    connectTailnet: (mode: TailnetMode = 'embedded', https?: boolean) =>
      post<TailnetInfo>('/api/v1/tailnet/connect', (https === undefined ? { mode } : { mode, https }) satisfies TailnetConnect),

    // Proposed (not in flux-core yet).
    /** GET /api/v1/invites */
    invites: () => get<Invite[]>('/api/v1/invites'),
    /** POST /api/v1/invites */
    createInvite: (body: CreateInvite) => post<Invite>('/api/v1/invites', body),
  };
}

export type FluxClient = ReturnType<typeof createFluxClient>;

let readyGate: Promise<void> | null = null;

/** The mock backend registers its boot promise here so early requests wait for it. */
export function setRequestGate(p: Promise<void>): void {
  readyGate = p;
}

/** Resolves once the backend can take requests (immediately for a real server). */
export function whenReady(): Promise<void> {
  return readyGate ?? Promise.resolve();
}

/** The app-wide client. */
export const api: FluxClient = createFluxClient({ baseUrl: API_BASE, ready: whenReady });
