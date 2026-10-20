#!/usr/bin/env node
// Playback through the whole stack: release flux-server + the production web
// client in the system Google Chrome, per plan type.
//
// 1. Server-side time to first segment (TTFS), no browser: POST /api/v1/play
//    with the caps this Chrome reports, then the prefetch URLs the client would
//    fetch (video init + segment, audio init + segment), or the first MiB of
//    the file for direct play. Run 1 is cold (nothing prepared), runs 2+ warm.
// 2. Click → first frame (requestVideoFrameCallback) and seek → first frame,
//    RUNS fresh browser contexts per case × SEEKS seeks, measured both by the
//    app's PlaybackMetrics and by an independent rVFC in the harness.
//
// Device variants are made with init scripts: "no Matroska" (Safari-like, so
// MKV is repackaged over HLS) and "no AV1" (M1-like, so AV1 is transcoded).
//
//   cd web && pnpm e2e:playback        (after pnpm e2e:bench, which leaves bench-data)
//
// Env: RUNS (5), SEEKS (10), CASES (comma list), TTFS_RUNS (5), E2E_PORT.
// Writes bench/results/flux-e2e-playback.json.
import { existsSync } from 'node:fs';
import { basename, join } from 'node:path';
import { NO_AV1_INIT, WORK, api, launchChrome, load1, machine, quietGate, sampleRss, startServer, summary, watchPage, writeResult } from './e2e-lib.mjs';

const PORT = Number(process.env.E2E_PORT ?? 18498);
const RUNS = Number(process.env.RUNS ?? 5);
const SEEKS = Number(process.env.SEEKS ?? 10);
const TTFS_RUNS = Number(process.env.TTFS_RUNS ?? 5);
const DATA = join(WORK, 'bench-data');

const NO_MKV_INIT = `(() => {
  const cpt = HTMLMediaElement.prototype.canPlayType;
  HTMLMediaElement.prototype.canPlayType = function (t) { return /matroska/i.test(String(t)) ? '' : cpt.call(this, t); };
})();`;

const ALL_CASES = [
  { name: 'direct-mp4', file: 'h264_aac_faststart.mp4', type: 'Direct play' },
  { name: 'direct-mkv', file: 'h264_long_gop.mkv', type: 'Direct play' },
  { name: 'remux-mkv-h264-flac', file: 'h264_flac.mkv', type: 'Direct stream', device: 'no-mkv' },
  { name: 'remux-hevc10-10min', file: 'synth_hevc10_opengop_10min.mkv', type: 'Direct stream', device: 'no-mkv' },
  { name: 'audio-hevc-hdr10-4k-eac3', file: 'hevc_main10_hdr10_eac3.mkv', type: 'Audio conversion', note: '4K HEVC Main10 HDR10 copied over HLS' },
  { name: 'audio-truehd', file: 'h264_truehd71.mkv', type: 'Audio conversion' },
  { name: 'audio-dts', file: 'h264_dts51.mkv', type: 'Audio conversion' },
  { name: 'transcode-mpeg2-ts', file: 'mpeg2_ac3.ts', type: 'Transcode' },
  { name: 'transcode-avi', file: 'mpeg4_mp3.avi', type: 'Transcode' },
  { name: 'transcode-av1', file: 'av1_opus.mkv', type: 'Transcode', device: 'no-av1' },
  { name: 'transcode-h264-m2ts', file: 'h264_aac.m2ts', type: 'Transcode' },
];
const CASES = process.env.CASES ? ALL_CASES.filter((c) => process.env.CASES.split(',').includes(c.name)) : ALL_CASES;
const DEVICE_INIT = { 'no-mkv': NO_MKV_INIT, 'no-av1': NO_AV1_INIT };

/** PlaybackPlan::label(). */
function planLabel(plan) {
  if (plan.delivery === 'progressive_file') return 'Direct play';
  if (plan.video.op === 'transcode') return 'Transcode';
  if (plan.audio.some((a) => a.op.op === 'transcode')) return 'Audio conversion';
  return 'Direct stream';
}

/** The caps a device variant sends, derived from what this Chrome sent. */
function capsFor(base, device) {
  const c = structuredClone(base);
  if (device === 'no-mkv') c.progressive_containers = c.progressive_containers.filter((x) => x !== 'matroska');
  if (device === 'no-av1') c.video = c.video.filter((v) => v.codec !== 'av1');
  return c;
}

if (!existsSync(join(DATA, 'catalog.db'))) {
  console.error(`[playback] ${DATA} has no catalog; run scripts/e2e-bench.mjs first`);
  process.exit(2);
}
const out = { name: 'flux-e2e-playback', generated_at: new Date().toISOString(), machine: machine(), quiet_wait_ms: 0, runs: RUNS, seeks: SEEKS };
out.quiet_wait_ms += await quietGate('playback bench');
let server = await startServer({ dataDir: DATA, port: PORT, log: join(WORK, 'server-playback.log') });
const base = server.base;
let rss = sampleRss(server.pid, 500);
out.load1_start = load1();

const libs = await api(base, '/api/v1/libraries');
const files = new Map();
for (const name of ['Features', 'Research']) {
  const L = libs.find((l) => l.name === name);
  const page = await api(base, `/api/v1/libraries/${L.id}/items?limit=500`);
  for (const it of page.items) {
    const d = await api(base, `/api/v1/items/${it.id}`);
    for (const a of d.assets) files.set(basename(a.path), { item: it.id, asset: a.id, duration_ms: a.duration_ms });
  }
}

const browser = await launchChrome();
const errors = [];

// The caps this Chrome sends (captured from the app's own POST /play).
let chromeCaps;
{
  const ctx = await browser.newContext();
  const page = await ctx.newPage();
  const f = files.get('h264_aac_faststart.mp4');
  const req = page.waitForRequest((r) => r.url().endsWith('/api/v1/play') && r.method() === 'POST', { timeout: 30_000 });
  await page.goto(`${base}/play/${f.item}?start=0`, { waitUntil: 'domcontentloaded' });
  chromeCaps = JSON.parse((await req).postData()).caps;
  await page.waitForTimeout(500);
  await ctx.close();
}
out.chrome_caps = chromeCaps;

// ── 1. browser: click → first frame, seeks (run 1 = the file's first play on this server) ─────────────────────────────────
const SEEK_FRACTIONS = [0.5, 0.15, 0.82, 0.33, 0.95, 0.05, 0.62, 0.25, 0.71, 0.42, 0.88, 0.1];
const cases = {};
for (const c of CASES) {
  out.quiet_wait_ms += await quietGate(`browser ${c.name}`);
  const f = files.get(c.file);
  const ttffApp = [];
  const ttffHarness = [];
  const seekApp = [];
  const seekHarness = [];
  const seekFirstRun = [];
  const labels = new Set();
  const loads = [load1()];
  for (let run = 0; run < RUNS; run++) {
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 } });
    if (c.device) await ctx.addInitScript(DEVICE_INIT[c.device]);
    const page = await ctx.newPage();
    watchPage(page, errors);
    await page.goto(`${base}/item/${f.item}`, { waitUntil: 'networkidle' });
    // Items with several versions (Main10: an HDR10 E-AC-3 MKV and an AAC MP4) list
    // them in scan order: pick this case's file explicitly.
    const radios = page.locator('input[name="version"]');
    if ((await radios.count()) > 1) {
      const d = await api(base, `/api/v1/items/${f.item}`);
      const idx = d.assets.findIndex((a) => a.id === f.asset);
      if (idx >= 0) await radios.nth(idx).check({ force: true });
    }
    // Every run starts at 0 (earlier runs leave a resume point).
    const restart = page.locator('button', { hasText: 'From the start' });
    const button = (await restart.count()) > 0 ? restart.first() : page.locator('button', { hasText: /^(Resume|Play)/ }).first();
    await button.hover();
    await page.waitForTimeout(200);
    await page.evaluate(() => {
      const w = window;
      w.__h = { t0: 0, first: null };
      new MutationObserver((_, obs) => {
        const v = document.querySelector('video');
        if (!v) return;
        obs.disconnect();
        v.requestVideoFrameCallback((now, meta) => (w.__h.first = (meta.presentationTime ?? now) - w.__h.t0));
      }).observe(document.body, { childList: true, subtree: true });
      document.addEventListener('click', () => (w.__h.t0 = performance.now()), { capture: true, once: true });
    });
    await button.click();
    await page.waitForFunction(() => window.__fluxPlayback?.metrics().ttffMs != null && window.__h?.first != null, null, { timeout: 45_000 });
    const first = await page.evaluate(() => ({ app: window.__fluxPlayback.metrics().ttffMs, harness: window.__h.first, plan: window.__fluxPlayback.plan }));
    ttffApp.push(first.app);
    ttffHarness.push(Math.round(first.harness));
    labels.add(planLabel(first.plan));
    await page.waitForTimeout(800);
    const duration = await page.evaluate(() => document.querySelector('video').duration);
    for (let k = 0; k < SEEKS; k++) {
      const frac = SEEK_FRACTIONS[(k + run * 3) % SEEK_FRACTIONS.length];
      const target = Math.max(1, Math.min(duration - 2.5, frac * duration));
      const r = await page.evaluate(async (target) => {
        const v = document.querySelector('video');
        const before = window.__fluxPlayback.metrics().seekSamples.length;
        const t0 = performance.now();
        const harness = new Promise((resolve) => {
          v.addEventListener('seeked', () => v.requestVideoFrameCallback((now, meta) => resolve((meta.presentationTime ?? now) - t0)), { once: true });
        });
        v.currentTime = target;
        const h = await Promise.race([harness, new Promise((r) => setTimeout(() => r(null), 15_000))]);
        const start = performance.now();
        while (window.__fluxPlayback.metrics().seekSamples.length === before && performance.now() - start < 3000) await new Promise((r) => setTimeout(r, 10));
        return { harness: h === null ? null : Math.round(h), app: window.__fluxPlayback.metrics().lastSeekMs };
      }, target);
      if (r.harness !== null) (run === 0 ? seekFirstRun : seekHarness).push(r.harness);
      if (r.app !== null) seekApp.push(r.app);
      await page.waitForTimeout(300);
    }
    await ctx.close();
  }
  loads.push(load1());
  cases[c.name] = {
    file: c.file,
    device: c.device ?? 'chrome',
    expected_type: c.type,
    labels: [...labels],
    note: c.note,
    first_play: { ttff_ms: ttffHarness[0], seek_ms: summary(seekFirstRun), seek_samples: seekFirstRun, note: 'run 1: first play of this file on this server (index built, nothing transcoded yet)' },
    ttff_ms: { app: summary(ttffApp.slice(1)), harness: summary(ttffHarness.slice(1)), samples: ttffHarness.slice(1), runs: 'runs 2+' },
    seek_ms: { app: summary(seekApp), harness: summary(seekHarness), samples: seekHarness, runs: 'runs 2+' },
    load1: loads,
  };
  console.log(
    `[playback] ${c.name}: ${[...labels].join('/')} first play: TTFF ${ttffHarness[0]} seek p50 ${cases[c.name].first_play.seek_ms.p50} p95 ${cases[c.name].first_play.seek_ms.p95}; runs 2+: TTFF p50 ${cases[c.name].ttff_ms.harness.p50} p95 ${cases[c.name].ttff_ms.harness.p95}, seek p50 ${cases[c.name].seek_ms.harness.p50} p95 ${cases[c.name].seek_ms.harness.p95} (load ${loads.join('→')})`,
  );
}
out.browser = cases;

// ── 2. server-side TTFS, after a restart (cold = first request; the catalog still has the play index)
out.server_rss_peak_mb_sampled_browser_phase = rss.stop().peak_mb;
out.server_rss_after_browser_phase_mb = server.rss();
out.server_peak_rss_with_children_mb_browser_phase = (await server.stop()).peakMb;
server = await startServer({ dataDir: DATA, port: PORT, log: join(WORK, 'server-playback.log') });
rss = sampleRss(server.pid, 500);
async function timedGet(url, headers = {}) {
  const t0 = performance.now();
  const r = await fetch(url, { headers });
  const buf = await r.arrayBuffer();
  return { ms: performance.now() - t0, status: r.status, bytes: buf.byteLength, flux_ms: Number(r.headers.get('x-flux-ms')) || null };
}

const ttfs = {};
for (const c of CASES) {
  const f = files.get(c.file);
  const caps = capsFor(chromeCaps, c.device);
  const runs = [];
  for (let i = 0; i < TTFS_RUNS; i++) {
    const t0 = performance.now();
    const res = await api(base, '/api/v1/play', {
      method: 'POST',
      body: JSON.stringify({ item_id: f.item, asset_id: f.asset, caps, audio_track: null, subtitle_track: null, start_ms: 0 }),
    });
    const playMs = performance.now() - t0;
    let video;
    let audio = null;
    if (res.plan.delivery === 'progressive_file') {
      video = await timedGet(base + res.url, { range: 'bytes=0-1048575' });
    } else {
      const v = res.prefetch.filter((u) => u.includes('/v/') && !u.endsWith('.m3u8'));
      const a = res.prefetch.filter((u) => u.includes('/a/') && !u.endsWith('.m3u8'));
      const [vr, ar] = await Promise.all([
        Promise.all(v.map((u) => timedGet(base + u))),
        Promise.all(a.map((u) => timedGet(base + u))),
      ]);
      video = { ms: Math.max(...vr.map((x) => x.ms)), status: Math.max(...vr.map((x) => x.status)), bytes: vr.reduce((s, x) => s + x.bytes, 0), flux_ms: Math.max(...vr.map((x) => x.flux_ms ?? 0)) };
      if (ar.length) audio = { ms: Math.max(...ar.map((x) => x.ms)), status: Math.max(...ar.map((x) => x.status)), flux_ms: Math.max(...ar.map((x) => x.flux_ms ?? 0)) };
    }
    runs.push({
      play_ms: Math.round(playMs * 10) / 10,
      ttfs_ms: Math.round((playMs + video.ms) * 10) / 10,
      ttfs_with_audio_ms: Math.round((playMs + Math.max(video.ms, audio?.ms ?? 0)) * 10) / 10,
      video_status: video.status,
      server_segment_ms: video.flux_ms,
      label: planLabel(res.plan),
    });
  }
  ttfs[c.name] = {
    file: c.file,
    label: runs[0].label,
    cold: runs[0],
    warm_ttfs_ms: summary(runs.slice(1).map((r) => r.ttfs_ms)),
    warm_ttfs_with_audio_ms: summary(runs.slice(1).map((r) => r.ttfs_with_audio_ms)),
    play_ms: summary(runs.map((r) => r.play_ms)),
  };
  console.log(`[playback] TTFS ${c.name}: ${runs[0].label}, cold ${runs[0].ttfs_ms} ms (+audio ${runs[0].ttfs_with_audio_ms}), warm p50 ${ttfs[c.name].warm_ttfs_ms.p50} ms`);
}
out.server_ttfs = ttfs;

// Per plan type, pooling the cases of that type.
const byType = {};
for (const c of CASES) {
  const r = cases[c.name];
  const t = r.labels.length === 1 ? r.labels[0] : r.labels.join('/');
  byType[t] ??= { cases: [], ttff: [], seek: [], firstTtff: [], firstSeek: [] };
  byType[t].cases.push(c.name);
  byType[t].ttff.push(...r.ttff_ms.samples);
  byType[t].firstTtff.push(r.first_play.ttff_ms);
  byType[t].firstSeek.push(...r.first_play.seek_samples);
}
for (const c of CASES) {
  const r = cases[c.name];
  const t = r.labels.length === 1 ? r.labels[0] : r.labels.join('/');
  byType[t].seek.push(...r.seek_ms.samples);
}
out.by_plan_type = Object.fromEntries(
  Object.entries(byType).map(([t, v]) => [
    t,
    {
      cases: v.cases,
      first_play_ttff_ms: v.firstTtff,
      first_play_seek_ms: summary(v.firstSeek),
      ttff_ms: summary(v.ttff),
      seek_ms: summary(v.seek),
    },
  ]),
);
out.page_errors = errors;
await browser.close();
out.server_rss_peak_mb_sampled_ttfs_phase = rss.stop().peak_mb;
await server.stop();
out.load1_end = load1();
out.how =
  'TTFF: Play click → first requestVideoFrameCallback presentationTime; seek: currentTime assignment → first rVFC after seeked. Fresh browser context per run (cold HTTP cache, but the server keeps its prepared assets and transcoded pieces between runs). Server and Chrome at nice 19 on a shared machine.';
writeResult('playback', out);
console.log(JSON.stringify(out.by_plan_type, null, 1));
