/**
 * The typed client end to end against the mock backend's MSW handlers (the
 * same handlers the browser uses), with the catalog built from the real test
 * library manifest.
 */
import { readFileSync } from 'node:fs';
import { setupServer } from 'msw/node';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { createMockDb, type Fixtures } from '../mock/db';
import { createHandlers } from '../mock/handlers';
import { apiErrorMessage, createFluxClient, FluxApiError, artSrcSet, artUrl, artWidthBucket } from './client';
import { planAudioSwitch, renditionKey, startingRendition } from '../player/audio';
import type { ClientCaps, ItemSummary } from './types';

const ORIGIN = 'http://flux.test';
const fixtures = JSON.parse(readFileSync(new URL('../mock/fixtures.json', import.meta.url), 'utf8')) as Fixtures;
const db = createMockDb(fixtures, Date.UTC(2026, 9, 18));
const server = setupServer(...createHandlers(db, { media: { mode: 'remote' }, origin: ORIGIN, tailnet: 'off' }));
const client = createFluxClient({ baseUrl: ORIGIN });

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }));
afterAll(() => server.close());

const chromeCaps: ClientCaps = {
  client: 'web/chrome-141/macos',
  delivery: ['progressive_file', 'hls_fmp4', 'hls_ts'],
  progressive_containers: ['mp4', 'web_m'],
  video: [{ codec: 'h264', profiles: ['high', 'main'], max_level: 51, max_bit_depth: 8, max_width: 3840, max_height: 2160, hdr: [], dovi_profiles: [] }],
  audio: [{ codec: 'aac', max_channels: 8 }],
  subtitles: ['web_vtt'],
  display_hdr: false,
  needs_demuxed_audio: false,
  max_bitrate: null,
};

describe('flux client against the mock server', () => {
  it('GET /server', async () => {
    const s = await client.server();
    expect(s.tailnet_name).toBeNull();
    expect(s.path).toBe('localhost');
    expect(s.metadata.source).toBe('tmdb');
  });

  it('GET /libraries', async () => {
    const libs = await client.libraries();
    expect(libs.map((l) => l.name)).toEqual(['Movies', 'TV Shows', 'Film Archive']);
    expect(libs[0].item_count).toBe(2998);
    expect(libs[2].scan.state).toBe('unavailable');
  });

  it('pages a 10k library through opaque cursors', async () => {
    const seen: ItemSummary[] = [];
    let cursor: string | null = null;
    let total: number | null | undefined;
    do {
      const page = await client.libraryItems(3, { cursor, limit: 1000, sort: 'title' });
      seen.push(...page.items);
      cursor = page.next;
      total = page.total;
    } while (cursor);
    expect(total).toBe(10_000);
    expect(seen).toHaveLength(10_000);
    expect(new Set(seen.map((i) => i.id)).size).toBe(10_000);
  });

  it('sorts by the requested key', async () => {
    const page = await client.libraryItems(1, { limit: 50, sort: '-year' });
    const years = page.items.map((i) => i.year ?? 0);
    expect([...years].sort((a, b) => b - a)).toEqual(years);
  });

  it('GET /items/{id} is a flattened ItemDetail with probed assets', async () => {
    const page = await client.search('Spirited Away');
    const d = await client.item(page[0].id);
    expect(d.title).toBe('Spirited Away');
    expect(d.assets).toHaveLength(2); // cd1 + cd2 in the manifest
    expect(d.assets[0].probe?.tracks.filter((t) => t.kind === 'audio')).toHaveLength(2);
    expect(d.match_confidence).toBeGreaterThan(0.9);
    expect(d.external_ids).toEqual([['tmdb', '129']]);
  });

  it('walks show → seasons → episodes', async () => {
    const [show] = await client.search('Severance');
    expect(show.kind).toBe('show');
    const seasons = await client.children(show.id);
    expect(seasons.map((s) => s.title)).toEqual(['Season 1', 'Season 2']);
    const eps = await client.children(seasons[0].id);
    expect(eps).toHaveLength(9);
    expect(eps[0].subtitle).toBe('S01E01');
  });

  it('search folds accents', async () => {
    const r = await client.search('amelie');
    expect(r.map((i) => i.title)).toContain('Amélie');
  });

  it('POST /play explains the plan; progress and diagnostics round-trip', async () => {
    const [dune] = await client.search('Dune: Part Two');
    const res = await client.play({ item_id: dune.id, asset_id: null, caps: chromeCaps, audio_track: null, subtitle_track: null, start_ms: 0 });
    expect(res.plan.video.op).toBe('transcode'); // MPEG-2 in TS
    expect(res.explain.some((e) => e.includes('MPEG-2'))).toBe(true);
    // HLS audio renditions as flux-hls names them; the first is the default.
    expect(res.audio.length).toBeGreaterThan(0);
    for (const a of res.audio) expect(a.rendition).toMatch(/^\d+(-(aac|eac3)\d)?$/);
    expect(res.audio.filter((a) => a.default)).toEqual([res.audio[0]]);
    await client.reportProgress(res.session_id, { position_ms: 5000, paused: false, ttff_ms: 420, last_seek_ms: null, stalls: 0, dropped_frames: 0 });
    const diag = await client.diagnostics(res.session_id);
    expect(diag.client?.ttff_ms).toBe(420);
    expect(diag.plan).toEqual(res.plan);
  });

  it('POST /play lists every audio track as a rendition, so audio switches stay in the session', async () => {
    const [spirited] = await client.search('Spirited Away');
    const res = await client.play({ item_id: spirited.id, asset_id: null, caps: chromeCaps, audio_track: 2, subtitle_track: null, start_ms: 0 });
    expect(res.plan.delivery).not.toBe('progressive_file');
    expect(res.audio.map((a) => a.rendition)).toEqual(['2-aac6', '1']);
    expect(res.plan.audio.map(renditionKey)).toEqual(res.audio.map((a) => a.rendition));
    // Back to English: a rendition of this session, so no new /play.
    expect(planAudioSwitch(res, startingRendition(res), 1, 'hls.js')).toEqual({ kind: 'in-session', ref: res.audio[1] });
  });

  it('refuses to play a file whose drive is offline (409, item kept)', async () => {
    const page = await client.libraryItems(3, { limit: 30, sort: 'added' });
    const offline = [];
    for (const it of page.items) {
      const d = await client.item(it.id);
      if (!d.assets[0].available) offline.push(d);
    }
    expect(offline.length).toBeGreaterThan(0);
    await expect(client.play({ item_id: offline[0].id, asset_id: null, caps: chromeCaps, audio_track: null, subtitle_track: null, start_ms: null })).rejects.toMatchObject({ status: 409 });
  });

  it('turns HTTP errors into FluxApiError', async () => {
    const err = await client.item(42).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(FluxApiError);
    expect((err as FluxApiError).status).toBe(404);
  });

  it('creates a library and starts scanning it', async () => {
    const lib = await client.createLibrary({ name: 'Kids', kind: 'movies', roots: ['/Volumes/Media/Kids'] });
    expect(lib.scan.state).toBe('scanning');
    const again = await client.scanLibrary(lib.id);
    expect(again.state).toBe('scanning');
  });

  it('core v2: parent titles, availability, unwatched counts and root status', async () => {
    const [show] = await client.search('Severance');
    expect(show.parent_title).toBeNull();
    expect(show.available).toBe(true);
    expect(show.unwatched_count).toBeGreaterThan(0);
    const [season] = await client.children(show.id);
    expect(season.parent_title).toBe('Severance');
    const [ep] = await client.children(season.id);
    expect(ep.parent_title).toBe('Severance');
    expect(ep.unwatched_count).toBeNull();
    const libs = await client.libraries();
    const archive = libs[2].roots;
    expect(archive.filter((r) => !r.available)).toHaveLength(1);
    expect(archive.every((r) => r.last_seen !== null)).toBe(true);
    expect(libs[0].scan.files_total).toBe(libs[0].scan.files_seen);
  });

  it('POST /items/{id}/watched marks a season and recounts the show', async () => {
    const [show] = await client.search('Severance');
    const [season] = await client.children(show.id);
    const before = (await client.search('Severance'))[0].unwatched_count ?? 0;
    await client.markWatched(season.id, true);
    const eps = await client.children(season.id);
    expect(eps.every((e) => e.progress?.watched)).toBe(true);
    expect((await client.search('Severance'))[0].unwatched_count).toBe(before - eps.length);
    await client.markWatched(season.id, false);
    expect((await client.search('Severance'))[0].unwatched_count).toBe(before);
  });

  it('filters library pages on the server', async () => {
    const inProgress = await client.libraryItems(1, { limit: 500, filter: 'in_progress' });
    expect(inProgress.items.length).toBeGreaterThan(0);
    expect(inProgress.total).toBe(inProgress.items.length);
    for (const i of inProgress.items) expect(i.progress?.watched).toBe(false);
    const all = await client.libraryItems(1, { limit: 1 });
    expect((await client.libraryItems(1, { limit: 1, filter: '4k' })).total).toBeLessThan(all.total ?? 0);
  });

  it('GET /libraries/{id}/folders browses by folder', async () => {
    const top = await client.libraryFolders(1);
    expect(top.at).toEqual({ folder: '', name: 'Movies' });
    expect(top.parents).toEqual([]);
    const counted = top.folders.reduce((n, f) => n + f.item_count, 0) + top.total;
    expect(counted).toBeGreaterThan(0);
    const first = top.folders[0];
    if (first) {
      const inside = await client.libraryFolders(1, { folder: first.folder, sort: '-date', limit: 1 });
      expect(inside.at).toEqual({ folder: first.folder, name: first.name });
      expect(inside.parents).toEqual([top.at]);
      expect(inside.items.length).toBeLessThanOrEqual(1);
    }
    await expect(client.libraryFolders(1, { folder: '1/No such folder' })).rejects.toMatchObject({ status: 404 });
  });

  it('POST /plan previews /play without a session', async () => {
    const [dune] = await client.search('Dune: Part Two');
    const req = { item_id: dune.id, asset_id: null, caps: chromeCaps, audio_track: null, subtitle_track: null, start_ms: null };
    const preview = await client.plan(req);
    expect(preview.label).toBe('Transcode');
    expect(preview.explain.length).toBe(preview.plan.reasons.length);
  });

  it('tailnet onboarding + invites', async () => {
    const t = await client.connectTailnet('embedded');
    expect(t.state).toBe('needs_login');
    expect(t.mode).toBe('embedded');
    expect(t.auth_url).toMatch(/^https:\/\/login\.tailscale\.com\/a\//);
    await expect(client.createInvite({ kind: 'share', role: 'viewer', note: null })).rejects.toMatchObject({ status: 409 });
  });
});

describe('CSRF guard: X-Flux-Request (SEC-1 / SEC-7 contract §2–3)', () => {
  it('the client sends X-Flux-Request: 1 on every API call, GETs and POSTs', async () => {
    const seen: { method: string; path: string; header: string | null }[] = [];
    const spy = createFluxClient({
      baseUrl: ORIGIN,
      fetch: (input, init) => {
        const req = new Request(input, init);
        seen.push({ method: req.method, path: new URL(req.url).pathname, header: req.headers.get('x-flux-request') });
        return fetch(req);
      },
    });
    await spy.server();
    const [dune] = await spy.search('Dune: Part Two');
    const req = { item_id: dune.id, asset_id: null, caps: chromeCaps, audio_track: null, subtitle_track: null, start_ms: null };
    await spy.plan(req);
    const res = await spy.play(req);
    await spy.reportProgress(res.session_id, { position_ms: 1000, paused: true, ttff_ms: null, last_seek_ms: null, stalls: 0, dropped_frames: 0 }, true);
    await spy.diagnostics(res.session_id);
    await spy.markWatched(42, true).catch(() => undefined); // 404: no item 42, but past the guard
    expect(seen.map((s) => s.method)).toEqual(['GET', 'GET', 'POST', 'POST', 'POST', 'GET', 'POST']);
    expect(seen.filter((s) => s.header !== '1')).toEqual([]);
  });

  it('the mock refuses an API call without it (403), as flux does for unsafe methods', async () => {
    const without = await fetch(`${ORIGIN}/api/v1/items/42/watched`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{"watched":true}' });
    expect(without.status).toBe(403);
    expect(await without.text()).toMatch(/X-Flux-Request/);
    const withIt = await fetch(`${ORIGIN}/api/v1/items/42/watched`, { method: 'POST', headers: { 'content-type': 'application/json', 'x-flux-request': '1' }, body: '{"watched":true}' });
    expect(withIt.status).toBe(404); // through the guard to the route
  });

  it('the mock refuses a foreign Origin on unsafe methods even with the header', async () => {
    const post = (origin: string) =>
      fetch(`${ORIGIN}/api/v1/items/42/watched`, { method: 'POST', headers: { 'content-type': 'application/json', 'x-flux-request': '1', origin }, body: '{"watched":true}' });
    expect((await post('https://evil.example')).status).toBe(403);
    expect((await post(ORIGIN)).status).toBe(404);
  });

  it('a request that loses the header fails with a message that says why', async () => {
    const stripped = createFluxClient({
      baseUrl: ORIGIN,
      fetch: (input, init) => {
        const headers = new Headers(init?.headers);
        headers.delete('x-flux-request');
        return fetch(input, { ...init, headers });
      },
    });
    const err = await stripped.markWatched(42, true).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(FluxApiError);
    expect((err as FluxApiError).status).toBe(403);
    expect((err as FluxApiError).message).toMatch(/didn’t come from flux’s own page/);
  });

  it('421 Misdirected Request names the host and the way out', async () => {
    const rebound = createFluxClient({
      baseUrl: 'http://rebind.example:8096',
      fetch: () => Promise.resolve(new Response('Misdirected Request: host not allowed', { status: 421 })),
    });
    const err = await rebound.server().catch((e: unknown) => e);
    expect(err).toBeInstanceOf(FluxApiError);
    expect((err as FluxApiError).misdirected).toBe(true);
    expect((err as FluxApiError).message).toMatch(/rebind\.example:8096/);
    expect((err as FluxApiError).message).toMatch(/--allow-host/);
  });

  it('a role refusal reads as one, not as a cross-site block', () => {
    expect(apiErrorMessage(403, 'POST', '/api/v1/libraries/1/scan', 'admin only', null)).toBe('Not allowed: admin only');
    expect(apiErrorMessage(500, 'GET', '/api/v1/home', '', null)).toBe('GET /api/v1/home failed with HTTP 500');
  });
});

describe('art URL helpers', () => {
  const ref = { url: '/api/v1/art/p-1', width: 400, height: 600, blurhash: null, source: 'provider' as const, attribution: 'TMDB', generator: null };
  it('appends ?w= width hints from fixed buckets', () => {
    expect(artWidthBucket(150)).toBe(160);
    expect(artWidthBucket(150, 2)).toBe(320);
    expect(artUrl(ref, 320)).toBe('/api/v1/art/p-1?w=320');
    expect(artUrl({ ...ref, url: '/art?id=1' }, 160)).toBe('/art?id=1&w=160');
    expect(artSrcSet(ref, 150)).toBe('/api/v1/art/p-1?w=160 160w, /api/v1/art/p-1?w=320 320w');
  });
});
