/**
 * flux's service worker. What it does and why is in core.ts.
 *
 * scripts/pwa-plugin.ts builds this into dist/sw.js and puts this build's id
 * and file list in front of it (`self.__FLUX_SW__`); app/install.ts registers
 * it on secure pages of a real-server build (never the mock or the dev server).
 * It runs as a classic worker script: no imports survive the build.
 */
import { CACHE_PREFIX, cacheName, keptBuilds, META_CACHE, routeOf, staleCaches, verdictOnPage } from './core';

interface ExtendableEvent extends Event {
  waitUntil(p: Promise<unknown>): void;
}
interface InstallEvent extends ExtendableEvent {
  /** Chrome's Static Routing API: requests these rules send to the network never wake the worker. */
  addRoutes?: (rules: unknown[]) => Promise<void>;
}
interface FetchEvent extends ExtendableEvent {
  readonly request: Request;
  readonly preloadResponse: Promise<Response | undefined>;
  respondWith(r: Response | Promise<Response>): void;
}
interface WorkerScope {
  readonly registration: ServiceWorkerRegistration;
  readonly clients: { claim(): Promise<void> };
  readonly __FLUX_SW__: { build: string; precache: string[] };
  skipWaiting(): Promise<void>;
  addEventListener(type: 'install', fn: (e: InstallEvent) => void): void;
  addEventListener(type: 'activate', fn: (e: ExtendableEvent) => void): void;
  addEventListener(type: 'fetch', fn: (e: FetchEvent) => void): void;
}

const sw = self as unknown as WorkerScope;
const { build: BUILD, precache: PRECACHE } = sw.__FLUX_SW__;
const at = (path: string) => new URL(path, sw.registration.scope).href;
/** The kept page and the list of kept builds, in META_CACHE. */
const PAGE_KEY = () => at('__flux/page');
const BUILDS_KEY = () => at('__flux/builds');
/** A page waits this long for the network before the kept copy opens instead. */
const PAGE_WAIT_MS = 4000;
/** Headers that describe the bytes on the wire, not the page: a kept copy must not carry them. */
const WIRE_HEADERS = ['content-encoding', 'content-length', 'transfer-encoding', 'connection', 'keep-alive', 'set-cookie'];

sw.addEventListener('install', (event) => {
  routeAround(event);
  event.waitUntil(install());
});

sw.addEventListener('activate', (event) => {
  event.waitUntil(activate());
});

sw.addEventListener('fetch', (event) => {
  const route = routeOf(event.request, sw.registration.scope);
  if (route === 'page') event.respondWith(page(event));
  else if (route === 'asset') event.respondWith(asset(event));
});

/** The API and media go straight to the network, without waking the worker (where the browser can do that). */
function routeAround(event: InstallEvent) {
  if (!event.addRoutes || typeof URLPattern === 'undefined') return;
  const base = new URL(sw.registration.scope).pathname;
  try {
    const rules = ['api', 'media'].map((p) => ({ condition: { urlPattern: new URLPattern({ pathname: `${base}${p}/*` }) }, source: 'network' }));
    event.waitUntil(event.addRoutes(rules).catch(() => undefined));
  } catch {
    // An older form of the API: the fetch handler steps aside for these anyway.
  }
}

/** This build's files, then take over at once: the previous build's files stay for pages still running it. */
async function install() {
  const cache = await caches.open(cacheName(BUILD));
  await cache.addAll(PRECACHE.map(at));
  await sw.skipWaiting();
}

async function activate() {
  const meta = await caches.open(META_CACHE);
  const before: unknown = await meta
    .match(BUILDS_KEY())
    .then((r) => r?.json())
    .catch(() => null);
  const keep = keptBuilds(before, BUILD);
  await meta.put(BUILDS_KEY(), new Response(JSON.stringify(keep), { headers: { 'content-type': 'application/json' } }));
  await Promise.all(staleCaches(await caches.keys(), keep).map((n) => caches.delete(n)));
  await sw.registration.navigationPreload?.enable().catch(() => undefined);
  await sw.clients.claim();
}

/** A page: the network's, else (offline, or no answer in PAGE_WAIT_MS) the kept copy. */
async function page(event: FetchEvent): Promise<Response> {
  const network = (async () => (await event.preloadResponse) ?? (await fetch(event.request)))();
  // Look at what the network says (even after the kept copy has answered), on a copy the page doesn't read.
  event.waitUntil(network.then((res) => look(res.clone()), () => undefined));
  try {
    return await withTimeout(network, PAGE_WAIT_MS);
  } catch {
    return (await keptPage()) ?? network;
  }
}

/** Keep a page flux served; a page that isn't flux means the address serves something else now: step aside. */
async function look(res: Response) {
  const type = res.headers.get('content-type');
  const html = res.ok && (type ?? '').toLowerCase().startsWith('text/html') ? await res.text() : null;
  const verdict = verdictOnPage({ ok: res.ok, status: res.status, contentType: type }, html);
  if (verdict === 'keep' && html !== null) {
    const headers = new Headers(res.headers);
    for (const h of WIRE_HEADERS) headers.delete(h);
    await (await caches.open(META_CACHE)).put(PAGE_KEY(), new Response(html, { status: 200, headers }));
  } else if (verdict === 'forget') {
    const names = await caches.keys();
    await Promise.all(names.filter((n) => n.startsWith(CACHE_PREFIX) || n === META_CACHE).map((n) => caches.delete(n)));
    await sw.registration.unregister();
  }
}

async function keptPage(): Promise<Response | undefined> {
  return (await caches.open(META_CACHE)).match(PAGE_KEY());
}

/** A client file: from whichever kept build has it, else the network (and then kept with this build). */
async function asset(event: FetchEvent): Promise<Response> {
  const req = event.request;
  const hit = await caches.match(req);
  if (hit) return hit;
  const res = await fetch(req);
  if (res.status === 200) {
    const copy = res.clone();
    event.waitUntil(caches.open(cacheName(BUILD)).then((c) => c.put(req, copy)));
  }
  return res;
}

function withTimeout<T>(p: Promise<T>, ms: number): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const t = setTimeout(() => reject(new Error('timeout')), ms);
    p.then(
      (v) => {
        clearTimeout(t);
        resolve(v);
      },
      (e: unknown) => {
        clearTimeout(t);
        reject(e instanceof Error ? e : new Error(String(e)));
      },
    );
  });
}
