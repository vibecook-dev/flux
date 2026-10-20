/**
 * MSW handlers implementing flux's HTTP API (crates/flux-core/src/api.rs)
 * against the in-memory mock catalog, plus the web-only route this client
 * proposes (/invites). Shared by the browser worker (browser.ts) and the
 * Node test server (vitest).
 */
import { delay, http, HttpResponse, sse } from 'msw';
import type {
  AudioRef,
  CreateInvite,
  CreateLibrary,
  FluxEvent,
  Id,
  Invite,
  ItemFilter,
  ItemSummary,
  Library,
  LibrarySort,
  MarkWatched,
  Page,
  PlanPreview,
  PlaybackPlan,
  PlayRequest,
  PlayResponse,
  ProbeResult,
  ProgressReport,
  ServerInfo,
  SessionDiagnostics,
  TailnetConnect,
  TailnetInfo,
} from '../api/types';
import { ITEM_FILTERS, LIBRARY_SORTS } from '../api/types';
import { channelLayout, codecShort, planLabel } from '../api/explain';
import { addOtherAudioTracks, predictPlan } from '../plan/predict';
import { hash32, posterSvg, wideSvg } from './art';
import type { MockDb } from './db';
import { mediaFor, type MockMedia } from './media';

export interface MockOptions {
  /** Where playable media comes from (local dev middleware or public streams). */
  media: MockMedia;
  /** Simulated server latency in ms for JSON routes (0 in tests). */
  latencyMs?: number;
  /** Initial tailnet state; `running` skips onboarding. */
  tailnet?: 'off' | 'running';
  /** How the browser reached the server (ServerInfo.path). */
  path?: string;
  /** Prefix for handler paths; tests in Node use an absolute origin. */
  origin?: string;
}

interface Session {
  id: string;
  itemId: Id;
  assetId: Id;
  response: PlayResponse;
  started: number;
  last: ProgressReport | null;
}

type SseClient = { send: (msg: { data: string }) => void };

export function createHandlers(db: MockDb, opts: MockOptions) {
  const latency = opts.latencyMs ?? 0;
  const u = (path: string) => `${opts.origin ?? ''}${path}`;
  const wait = () => (latency > 0 ? delay(latency) : Promise.resolve());
  const sessions = new Map<string, Session>();
  const sseClients = new Set<SseClient>();
  const invites: Invite[] = [];
  const scanTimers = new Map<Id, ReturnType<typeof setInterval>>();

  const DNS_NAME = 'flux.tail4e2b1.ts.net';
  const running = opts.tailnet === 'running';
  const tailnet: TailnetInfo = {
    mode: running ? 'embedded' : 'off',
    state: running ? 'running' : 'off',
    auth_url: null,
    approve_url: null,
    tailnet_name: running ? 'tail4e2b1.ts.net' : null,
    dns_name: running ? DNS_NAME : null,
    url: running ? `http://${DNS_NAME}:8096` : null,
    owner_login: running ? 'james@example.com' : null,
    key_expiry: running ? db.now + 179 * 86_400_000 : null,
    system_tailscale: true,
    last_error: null,
  };

  const broadcast = (evt: FluxEvent) => {
    const data = JSON.stringify(evt);
    for (const c of sseClients) {
      try {
        c.send({ data });
      } catch {
        sseClients.delete(c); // the EventSource went away
      }
    }
  };

  const serverInfo = (): ServerInfo => ({
    name: 'flux on studio-mac',
    version: '0.1.0-prototype (mock)',
    you:
      tailnet.state === 'running'
        ? { login_name: 'james@example.com', display_name: 'James', profile_pic_url: null, node_name: 'macbook', role: 'owner', tags: [] }
        : null,
    path: opts.path ?? 'localhost',
    // ServerInfo.tailnet_name is the server's MagicDNS name.
    tailnet_name: tailnet.dns_name,
  });

  const tailnetChanged = () => broadcast({ type: 'tailnet', info: { ...tailnet } });

  /**
   * `PlayResponse.audio` as flux-hls names its renditions: `{track}` copied,
   * `{track}-aac2` / `{track}-eac36` transcoded; the label is the playlist NAME.
   */
  const audioRefs = (plan: PlaybackPlan, probe: ProbeResult): AudioRef[] => {
    if (plan.delivery === 'progressive_file') return [];
    return plan.audio.flatMap((a, i) => {
      const t = probe.tracks.find((x) => x.index === a.track);
      if (!t) return [];
      const lang = t.title ?? (t.language === 'eng' ? 'English' : t.language ?? 'Unknown');
      const copy = a.op.op === 'copy';
      const codec = a.op.op === 'copy' ? t.codec : a.op.codec;
      const channels = a.op.op === 'copy' ? (t.channels ?? 2) : a.op.channels;
      const rendition = copy ? String(a.track) : `${a.track}-${codec === 'eac3' ? 'eac3' : 'aac'}${channels}`;
      return [{ rendition, track: a.track, label: `${lang} (${codecShort(codec)} ${channelLayout(channels)})`, language: t.language, codec, channels, default: i === 0 }];
    });
  };

  /**
   * Scan simulation. A rescan of an unchanged library walks every file but opens
   * none (flux's zero-change rescan, ARCHITECTURE.md §flux-scan); a new library
   * probes everything and items become browsable as they are found.
   */
  const startScan = (lib: Library) => {
    if (lib.scan.state === 'scanning') return lib.scan;
    const fresh = lib.item_count === 0;
    const total = fresh ? 180 : Math.max(lib.scan.files_seen, lib.item_count);
    const unavailableRoots = lib.roots.filter((r) => db.rootAvailable.get(r.path) === false).map((r) => r.path);
    const reachable = unavailableRoots.length ? Math.round(total * 0.66) : total;
    const changed = fresh ? reachable : Math.max(3, Math.round(reachable * 0.002));
    lib.scan = { state: 'scanning', files_seen: 0, files_total: null, files_probed: 0, items_added: 0, items_updated: 0, items_removed: 0, started_at: Date.now(), finished_at: null, last_error: null, dirs_unavailable: 0 };
    broadcast({ type: 'scan_progress', library_id: lib.id, status: { ...lib.scan } });
    const walkPerTick = Math.max(40, Math.round(reachable / 18));
    const probePerTick = fresh ? 9 : 2;
    const timer = setInterval(() => {
      const s = lib.scan;
      s.files_seen = Math.min(reachable, s.files_seen + walkPerTick);
      // The total is known once the walk is done.
      if (s.files_seen >= reachable) s.files_total = reachable;
      const probeTarget = Math.min(changed, Math.round((s.files_seen / reachable) * changed) + probePerTick);
      const newlyProbed = Math.max(0, Math.min(probeTarget, changed) - s.files_probed);
      s.files_probed += newlyProbed;
      if (fresh && newlyProbed > 0) {
        const added = db.addItems(lib.id, newlyProbed);
        s.items_added += added.length;
        broadcast({ type: 'items_changed', library_id: lib.id, added, updated: [], removed: [] });
      } else if (!fresh) {
        s.items_updated = s.files_probed;
      }
      const done = s.files_seen >= reachable && s.files_probed >= changed;
      if (done) {
        clearInterval(timer);
        scanTimers.delete(lib.id);
        s.finished_at = Date.now();
        s.state = unavailableRoots.length ? 'unavailable' : 'watching';
        s.last_error = unavailableRoots.length ? `${unavailableRoots.join(', ')} is not responding (NAS asleep?). Its items are kept.` : null;
        for (const r of lib.roots) {
          r.available = !unavailableRoots.includes(r.path);
          if (r.available) r.last_seen = s.finished_at;
        }
        for (const root of unavailableRoots) broadcast({ type: 'source_availability', library_id: lib.id, root, available: false });
      }
      broadcast({ type: 'scan_progress', library_id: lib.id, status: { ...s } });
    }, 160);
    scanTimers.set(lib.id, timer);
    return lib.scan;
  };

  const notFound = (what: string) => new HttpResponse(`${what} not found`, { status: 404 });

  /** The item and asset a play request refers to (shows and seasons play their next episode), or the error response. */
  const resolvePlayable = (req: PlayRequest) => {
    let rec = db.get(req.item_id);
    while (rec && (rec.kind === 'show' || rec.kind === 'season')) {
      const kids = rec.children.map((c) => db.get(c)).filter((c) => c !== undefined);
      rec = kids.find((k) => k.kind === 'episode' && !k.progress?.watched) ?? kids.find((k) => k.children.length > 0) ?? kids[0];
    }
    const detail = rec ? db.detail(rec.id) : undefined;
    if (!rec || !detail) return notFound('Item');
    const asset = req.asset_id !== null ? detail.assets.find((a) => a.id === req.asset_id) : detail.assets.find((a) => a.available) ?? detail.assets[0];
    if (!asset) return notFound('Asset');
    if (!asset.available) {
      return new HttpResponse('This file is on a drive that is not responding. flux kept the item; try again when the drive is back.', { status: 409 });
    }
    if (!asset.probe) return new HttpResponse('File has not been probed yet', { status: 409 });
    return { rec, asset: { ...asset, probe: asset.probe }, probe: asset.probe };
  };

  return [
    // flux's request guard (docs/prototype/web.md, Phase D contract): unsafe
    // methods under /api/ need `X-Flux-Request: 1` and, when the browser sends
    // an Origin, flux's own. The mock is stricter than flux and wants the header
    // on every API fetch, GETs too, so a call that forgets it fails in tests
    // instead of only on a real server's POSTs. EventSource and <img> can't add
    // headers, so /events and /art are exempt (flux needs nothing on GETs).
    http.all(u('/api/*'), ({ request }) => {
      const url = new URL(request.url);
      if (url.pathname === '/api/v1/events' || url.pathname.startsWith('/api/v1/art/')) return undefined;
      if (request.headers.get('x-flux-request') !== '1') {
        return new HttpResponse('Missing X-Flux-Request header: flux only takes API calls from its own web page', { status: 403 });
      }
      const origin = request.headers.get('origin');
      if (request.method !== 'GET' && request.method !== 'HEAD' && origin !== null && origin !== url.origin) {
        return new HttpResponse(`Cross-site request from Origin ${origin} refused`, { status: 403 });
      }
      return undefined; // on to the route's own handler
    }),

    http.get(u('/api/v1/server'), async () => {
      await wait();
      return HttpResponse.json(serverInfo());
    }),

    http.get(u('/api/v1/libraries'), async () => {
      await wait();
      return HttpResponse.json(db.libraries);
    }),

    http.post(u('/api/v1/libraries'), async ({ request }) => {
      await wait();
      const body = (await request.json()) as CreateLibrary;
      if (!body.name?.trim() || !Array.isArray(body.roots) || body.roots.length === 0) {
        return new HttpResponse('A library needs a name and at least one folder', { status: 400 });
      }
      const lib = db.createLibrary(body.name.trim(), body.kind, body.roots);
      startScan(lib);
      return HttpResponse.json(lib, { status: 201 });
    }),

    http.post(u('/api/v1/libraries/:id/scan'), async ({ params }) => {
      await wait();
      const lib = db.libraries.find((l) => l.id === Number(params.id));
      if (!lib) return notFound('Library');
      return HttpResponse.json(startScan(lib));
    }),

    http.get(u('/api/v1/libraries/:id/items'), async ({ params, request }) => {
      await wait();
      const lib = db.libraries.find((l) => l.id === Number(params.id));
      if (!lib) return notFound('Library');
      const url = new URL(request.url);
      const sortParam = url.searchParams.get('sort') ?? 'title';
      const sort: LibrarySort = (LIBRARY_SORTS as readonly string[]).includes(sortParam) ? (sortParam as LibrarySort) : 'title';
      const limit = Math.min(1000, Math.max(1, Number(url.searchParams.get('limit') ?? 100)));
      const cursor = url.searchParams.get('cursor');
      const offset = cursor ? Number(cursor.replace(/^o/, '')) || 0 : 0;
      const filterParam = url.searchParams.get('filter');
      if (filterParam !== null && !(ITEM_FILTERS as readonly string[]).includes(filterParam)) {
        return new HttpResponse(`Unknown filter ${filterParam}`, { status: 400 });
      }
      const filter = filterParam as ItemFilter | null;
      const sorted = db.libraryIds(lib.id, sort);
      const ids = filter ? sorted.filter((id) => db.matchesFilter(id, filter)) : sorted;
      const page: Page<ItemSummary> = {
        items: ids.slice(offset, offset + limit).flatMap((id) => db.summary(id) ?? []),
        next: offset + limit < ids.length ? `o${offset + limit}` : null,
        total: ids.length,
      };
      return HttpResponse.json(page);
    }),

    http.get(u('/api/v1/home'), async () => {
      await wait();
      return HttpResponse.json(db.home());
    }),

    http.get(u('/api/v1/items/:id'), async ({ params }) => {
      await wait();
      const d = db.detail(Number(params.id));
      return d ? HttpResponse.json(d) : notFound('Item');
    }),

    http.get(u('/api/v1/items/:id/children'), async ({ params }) => {
      await wait();
      const id = Number(params.id);
      return db.get(id) ? HttpResponse.json(db.children(id)) : notFound('Item');
    }),

    http.post(u('/api/v1/items/:id/watched'), async ({ params, request }) => {
      await wait();
      const id = Number(params.id);
      if (!db.get(id)) return notFound('Item');
      const body = (await request.json()) as MarkWatched;
      db.setWatched(id, body.watched);
      return new HttpResponse(null, { status: 204 });
    }),

    http.get(u('/api/v1/search'), async ({ request }) => {
      await wait();
      const q = new URL(request.url).searchParams.get('q') ?? '';
      return HttpResponse.json(db.search(q));
    }),

    // Dry run of /play: what the server would do, no session.
    http.post(u('/api/v1/plan'), async ({ request }) => {
      await wait();
      const req = (await request.json()) as PlayRequest;
      const found = resolvePlayable(req);
      if (found instanceof HttpResponse) return found;
      const { plan, explain } = predictPlan({ probe: found.probe, caps: req.caps, audioTrack: req.audio_track, subtitleTrack: req.subtitle_track });
      addOtherAudioTracks(plan, found.probe, req.caps);
      const preview: PlanPreview = { asset_id: found.asset.id, plan, explain, label: planLabel(plan) };
      return HttpResponse.json(preview);
    }),

    http.post(u('/api/v1/play'), async ({ request }) => {
      await wait();
      const req = (await request.json()) as PlayRequest;
      const found = resolvePlayable(req);
      if (found instanceof HttpResponse) return found;
      const { rec, asset } = found;
      // Without local media the mock can only hand out an HLS stream, so it plans
      // as if progressive files were impossible and says so in the explanation.
      const remote = opts.media.mode === 'remote';
      const caps = remote ? { ...req.caps, progressive_containers: [] } : req.caps;
      const { plan, explain } = predictPlan({ probe: asset.probe, caps, audioTrack: req.audio_track, subtitleTrack: req.subtitle_track });
      // Every audio track as a rendition, as flux-hls does: the player switches between them in the session.
      addOtherAudioTracks(plan, asset.probe, caps);
      if (remote) {
        const note = 'Mock server without local media: playing a public test stream instead of this file';
        plan.reasons.push({ reason: 'client_quirk', note });
        explain.push(note);
      }
      const sessionId = `s${(hash32(`${rec.id}${Date.now()}${Math.random()}`) >>> 0).toString(36)}`;
      const fileIndex = asset.id - rec.id * 10;
      const media = mediaFor(opts.media, plan, asset.probe, db.profileFile(rec.id, fileIndex));
      const textSubs = asset.probe.tracks.filter((t) => t.kind === 'subtitle' && ['subrip', 'ass', 'ssa', 'web_vtt', 'mov_text'].includes(t.codec));
      // As flux-server: a stored position resumes (a finished viewing stores 0; a rewatch resumes).
      const resume = rec.progress ? rec.progress.position_ms : 0;
      let start = req.start_ms ?? resume;
      // The stand-in bytes can be shorter than the item; don't start past their end.
      if (media.durationMs !== null && start >= media.durationMs - 5000) start = 0;
      const response: PlayResponse = {
        session_id: sessionId,
        asset_id: asset.id,
        plan,
        url: media.url,
        explain,
        start_ms: start,
        duration_ms: media.durationMs ?? asset.duration_ms,
        prefetch: media.prefetch,
        audio: audioRefs(plan, asset.probe),
        subtitles: textSubs.map((t) => ({
          track: t.index,
          label: t.title ?? (t.language === 'eng' ? 'English' : t.language ?? `Track ${t.index}`),
          language: t.language,
          url: `/media/${sessionId}/s/${t.index}.vtt`,
          forced: t.forced,
        })),
      };
      sessions.set(sessionId, { id: sessionId, itemId: rec.id, assetId: asset.id, response, started: Date.now(), last: null });
      return HttpResponse.json(response);
    }),

    http.post(u('/api/v1/sessions/:id/progress'), async ({ params, request }) => {
      const s = sessions.get(String(params.id));
      if (!s) return notFound('Session');
      const report = (await request.json()) as ProgressReport;
      s.last = report;
      const dur = s.response.duration_ms ?? 0;
      if (dur > 0) db.setProgress(s.itemId, report.position_ms, dur);
      return new HttpResponse(null, { status: 204 });
    }),

    http.get(u('/api/v1/sessions/:id/diagnostics'), async ({ params }) => {
      await wait();
      const s = sessions.get(String(params.id));
      if (!s) return notFound('Session');
      const elapsed = (Date.now() - s.started) / 1000;
      const segs = 2 + Math.floor(elapsed / 6);
      const transcode = s.response.plan.video.op === 'transcode';
      const audioConv = s.response.plan.audio.some((a) => a.op.op !== 'copy');
      const p50 = s.response.plan.delivery === 'progressive_file' ? null : transcode ? 410 : audioConv ? 14 : 1.9;
      const diag: SessionDiagnostics = {
        session_id: s.id,
        plan: s.response.plan,
        explain: s.response.explain,
        path: opts.path ?? 'localhost',
        segments_served: s.response.plan.delivery === 'progressive_file' ? 0 : segs,
        bytes_served: Math.round(segs * 6 * 1.4e6),
        segment_ms_p50: p50,
        segment_ms_p95: p50 === null ? null : Math.round(p50 * 2.6 * 10) / 10,
        client: s.last,
      };
      return HttpResponse.json(diag);
    }),

    // MSW's SSE support needs a global EventSource (absent in Node without a flag).
    ...(typeof EventSource === 'undefined'
      ? []
      : [
          sse(u('/api/v1/events'), ({ client }) => {
            sseClients.add(client);
          }),
        ]),

    http.get(u('/api/v1/art/:artId'), ({ params }) => {
      const art = String(params.artId);
      const [kind, idStr] = art.split('-');
      const id = Number(idStr);
      const rec = db.get(id);
      if (!rec) return notFound('Art');
      const owner = rec.showId !== null && kind !== 's' ? db.get(rec.showId) ?? rec : rec;
      const seedKey = `${owner.title}|${owner.year}`;
      let svg: string;
      if (kind === 's') {
        svg = wideSvg({ seed: `${seedKey}|S${rec.season}E${rec.episodes[0]}`, source: 'provider' });
      } else if (kind === 'b') {
        const src = owner.posterSource === 'generated' ? 'generated' : 'provider';
        svg = wideSvg({ seed: seedKey, source: src });
      } else {
        const seed = rec.kind === 'season' ? `${seedKey}|season${rec.season}` : seedKey;
        const title = rec.kind === 'season' ? `${owner.title} · ${rec.title}` : owner.title;
        svg = posterSvg({ seed, title, year: owner.year, source: owner.posterSource ?? 'provider' });
      }
      return new HttpResponse(svg, { headers: { 'content-type': 'image/svg+xml', 'cache-control': 'public, max-age=31536000, immutable' } });
    }),

    // WebVTT sidecars for text subtitle tracks.
    http.get(u('/media/:session/s/:file'), ({ params }) => {
      const s = sessions.get(String(params.session));
      const track = Number(String(params.file).replace(/\.vtt$/, ''));
      const asset = s ? db.detail(s.itemId)?.assets.find((a) => a.id === s.assetId) : undefined;
      const t = asset?.probe?.tracks.find((x) => x.index === track);
      if (!s || !t) return notFound('Subtitle');
      const label = t.title ?? 'Subtitles';
      const cues: string[] = ['WEBVTT', ''];
      const dur = Math.min(7200, Math.round((s.response.duration_ms ?? 600_000) / 1000));
      for (let at = 2; at < dur; at += 8) {
        const ts = (sec: number) => new Date(sec * 1000).toISOString().slice(11, 23);
        cues.push(`${ts(at)} --> ${ts(at + 3)}`, `[${label} · mock cue at ${Math.floor(at / 60)}:${String(at % 60).padStart(2, '0')}]`, '');
      }
      return new HttpResponse(cues.join('\n'), { headers: { 'content-type': 'text/vtt; charset=utf-8' } });
    }),

    http.get(u('/api/v1/tailnet'), async () => {
      await wait();
      return HttpResponse.json(tailnet);
    }),

    http.post(u('/api/v1/tailnet/connect'), async ({ request }) => {
      await wait();
      const body = (await request.json().catch(() => ({ mode: 'embedded' }))) as TailnetConnect;
      if (body.mode === 'off') {
        Object.assign(tailnet, { mode: 'off', state: 'off', auth_url: null, tailnet_name: null, dns_name: null, url: null, key_expiry: null });
      } else if (tailnet.state === 'off' || tailnet.state === 'error') {
        tailnet.mode = body.mode;
        tailnet.state = 'needs_login';
        const code = (hash32(String(Date.now())) >>> 0).toString(16).padStart(8, '0');
        tailnet.auth_url = `https://login.tailscale.com/a/${code}${code.slice(0, 4)}`;
      }
      tailnetChanged();
      return HttpResponse.json(tailnet);
    }),

    // Mock-only: pretend the owner finished signing in on login.tailscale.com.
    http.post(u('/__mock/tailnet/sign-in'), () => {
      tailnet.state = 'running';
      tailnet.auth_url = null;
      tailnet.tailnet_name = 'tail4e2b1.ts.net';
      tailnet.dns_name = DNS_NAME;
      tailnet.url = `http://${DNS_NAME}:8096`;
      tailnet.owner_login = 'james@example.com';
      tailnet.key_expiry = Date.now() + 180 * 86_400_000;
      tailnetChanged();
      return HttpResponse.json(tailnet);
    }),

    // ── proposed routes (not in flux-core yet) ──

    http.get(u('/api/v1/invites'), async () => {
      await wait();
      return HttpResponse.json(invites);
    }),

    http.post(u('/api/v1/invites'), async ({ request }) => {
      await wait();
      if (tailnet.state !== 'running') return new HttpResponse('Connect flux to Tailscale before inviting people', { status: 409 });
      const body = (await request.json()) as CreateInvite;
      const code = (hash32(`${Date.now()}${invites.length}`) >>> 0).toString(36);
      const inv: Invite = {
        id: code,
        kind: body.kind,
        role: body.role,
        note: body.note,
        url: body.kind === 'share' ? `https://login.tailscale.com/admin/invite/${code}` : `https://login.tailscale.com/uinv/${code}`,
        created_at: Date.now(),
        expires_at: Date.now() + 30 * 86_400_000,
        accepted: 0,
      };
      invites.unshift(inv);
      return HttpResponse.json(inv, { status: 201 });
    }),
  ];
}
