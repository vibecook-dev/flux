/**
 * Mock art, served the way a flux-server serves art: JPEG, at the width the
 * `?w=` hint asks for. Workers draw and encode it (art-worker.ts) and a
 * size-capped cache keeps it, so the page's main thread never rasterises art:
 * the browser decodes a JPEG off the main thread, where an SVG would be drawn on
 * it. Where workers can't draw (Node tests, no OffscreenCanvas), art stays SVG.
 *
 * Through MSW, each art request also crosses the service worker and back and is
 * matched against the handlers on the main thread (every handler reads the
 * cookies and parses the URL), which costs more than the picture. The GPU lenses
 * fetch hundreds of posters as they zoom and scroll, so `installArtFetch`
 * answers the page's own art fetches in the page.
 */
import { ART_SIZE, posterSvg, wideSvg, type ArtDraw } from './art';
import type { MockDb } from './db';

/** The art an id names (`p-12` poster, `b-12` backdrop, `s-40` episode still), or null. */
export function artDrawFor(db: MockDb, artId: string): ArtDraw | null {
  const [kind, idStr] = artId.split('-');
  const rec = db.get(Number(idStr));
  if (!rec) return null;
  const owner = rec.showId !== null && kind !== 's' ? db.get(rec.showId) ?? rec : rec;
  const seedKey = `${owner.title}|${owner.year}`;
  if (kind === 's') return { kind: 'wide', spec: { seed: `${seedKey}|S${rec.season}E${rec.episodes[0]}`, title: '', year: null, source: 'provider' } };
  if (kind === 'b') return { kind: 'wide', spec: { seed: seedKey, title: '', year: null, source: owner.posterSource === 'generated' ? 'generated' : 'provider' } };
  const seed = rec.kind === 'season' ? `${seedKey}|season${rec.season}` : seedKey;
  const title = rec.kind === 'season' ? `${owner.title} · ${rec.title}` : owner.title;
  return { kind: 'poster', spec: { seed, title, year: owner.year, source: owner.posterSource ?? 'provider' } };
}

/** Pixel width to draw: the `?w=` hint, as the server resizes to it, up to twice the native size. */
function widthFor(art: ArtDraw, hint: string | null): number {
  const native = ART_SIZE[art.kind][0];
  const w = Number(hint);
  return w > 0 ? Math.round(Math.min(Math.max(w, 64), native * 2)) : native;
}

// ───────────── the workers ─────────────

interface Job {
  key: string;
  art: ArtDraw;
  width: number;
  /** Requests waiting on it; a queued job nobody waits for any more is dropped. */
  waiters: number;
  done: (jpeg: Blob | null) => void;
}

interface Slot {
  worker: Worker;
  busy: number;
}

/** In flight per worker: one drawing while the last one encodes. */
const PER_WORKER = 2;
const CACHE_BYTES = 48 * 1024 * 1024;

let broken = typeof Worker === 'undefined' || typeof OffscreenCanvas === 'undefined' || !('convertToBlob' in OffscreenCanvas.prototype);
let slots: Slot[] | null = null;
/** Jobs not yet given to a worker, oldest first (the lenses ask nearest first). */
const queued = new Map<string, Job>();
const running = new Map<number, Job>();
let nextId = 1;
/** Finished and pending JPEGs by key, least recently used first. */
const cache = new Map<string, Promise<Blob | null>>();
const sizes = new Map<string, number>();
let cachedBytes = 0;

/** Stop drawing: everything waiting, and everything after, gets SVG. */
function giveUp(): void {
  broken = true;
  for (const job of [...queued.values(), ...running.values()]) job.done(null);
  queued.clear();
  running.clear();
  cache.clear();
  sizes.clear();
  cachedBytes = 0;
}

function pool(): Slot[] | null {
  if (slots || broken) return slots;
  try {
    const n = Math.max(1, Math.min(3, (navigator.hardwareConcurrency || 4) - 2));
    slots = Array.from({ length: n }, () => {
      const slot: Slot = { worker: new Worker(new URL('./art-worker.ts', import.meta.url), { type: 'module' }), busy: 0 };
      slot.worker.onmessage = (e: MessageEvent<{ id: number; blob?: Blob; error?: string }>) => {
        slot.busy--;
        const job = running.get(e.data.id);
        running.delete(e.data.id);
        job?.done(e.data.blob ?? null);
        if (e.data.blob) pump();
        else giveUp();
      };
      slot.worker.onerror = giveUp;
      return slot;
    });
  } catch {
    giveUp();
  }
  return slots;
}

function pump(): void {
  const all = pool();
  if (!all) return;
  for (const job of queued.values()) {
    let slot = all[0];
    for (const s of all) if (s.busy < slot.busy) slot = s;
    if (slot.busy >= PER_WORKER) return;
    queued.delete(job.key);
    const id = nextId++;
    running.set(id, job);
    slot.busy++;
    slot.worker.postMessage({ id, art: job.art, width: job.width });
  }
}

function keep(key: string, p: Promise<Blob | null>, jpeg: Blob | null): void {
  if (cache.get(key) !== p) return;
  if (!jpeg) {
    cache.delete(key);
    return;
  }
  sizes.set(key, jpeg.size);
  cachedBytes += jpeg.size;
  for (const k of cache.keys()) {
    if (cachedBytes <= CACHE_BYTES) break;
    const size = sizes.get(k);
    if (size === undefined) continue; // still drawing
    cachedBytes -= size;
    sizes.delete(k);
    cache.delete(k);
  }
}

/** The art as JPEG, drawn by a worker (or remembered), or null where workers can't draw it. */
function artJpeg(key: string, art: ArtDraw, width: number, signal?: AbortSignal | null): Promise<Blob | null> {
  if (broken) return Promise.resolve(null);
  let p = cache.get(key);
  if (p) {
    cache.delete(key);
    cache.set(key, p);
  } else {
    const made: Promise<Blob | null> = new Promise((done) => queued.set(key, { key, art, width, waiters: 0, done }));
    p = made;
    cache.set(key, made);
    void made.then((jpeg) => keep(key, made, jpeg));
    pump();
  }
  const job = queued.get(key);
  if (!job) return p;
  job.waiters++;
  if (!signal) return p;
  const shared = p;
  return new Promise((resolve, reject) => {
    const abort = () => {
      // Nobody else wants it and no worker has started it: drop it.
      if (queued.get(key) === job && --job.waiters === 0) {
        queued.delete(key);
        cache.delete(key);
        job.done(null);
      }
      reject(signal.reason instanceof Error ? signal.reason : new DOMException('The art request was aborted.', 'AbortError'));
    };
    if (signal.aborted) return abort();
    signal.addEventListener('abort', abort, { once: true });
    void shared.then((jpeg) => {
      signal.removeEventListener('abort', abort);
      resolve(jpeg);
    });
  });
}

const HEADERS = { 'cache-control': 'public, max-age=31536000, immutable' };

/** The answer to `GET /api/v1/art/{artId}?w=…`, or null when there is no such art. */
export async function artResponse(db: MockDb, artId: string, w: string | null, signal?: AbortSignal | null): Promise<Response | null> {
  const art = artDrawFor(db, artId);
  if (!art) return null;
  const width = widthFor(art, w);
  const jpeg = await artJpeg(`${artId}@${width}`, art, width, signal);
  if (jpeg) return new Response(jpeg, { headers: { 'content-type': 'image/jpeg', ...HEADERS } });
  return new Response(art.kind === 'wide' ? wideSvg(art.spec) : posterSvg(art.spec), { headers: { 'content-type': 'image/svg+xml', ...HEADERS } });
}

const ART_PATH = '/api/v1/art/';

/** Answer the page's own `fetch()`es of art here, not through the service worker. */
export function installArtFetch(db: MockDb): void {
  const network = window.fetch.bind(window);
  window.fetch = (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    const req = input instanceof Request ? input : null;
    if ((init?.method ?? req?.method ?? 'GET').toUpperCase() === 'GET') {
      const url = new URL(input instanceof Request ? input.url : input instanceof URL ? input.href : input, location.href);
      if (url.origin === location.origin && url.pathname.startsWith(ART_PATH)) {
        const id = decodeURIComponent(url.pathname.slice(ART_PATH.length));
        return artResponse(db, id, url.searchParams.get('w'), init?.signal ?? req?.signal).then((res) => res ?? new Response('Art not found', { status: 404 }));
      }
    }
    return network(input, init);
  };
}
