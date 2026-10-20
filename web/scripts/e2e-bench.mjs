#!/usr/bin/env node
// Server-side numbers through the real HTTP API of a release flux-server on
// the full 16.7k-file library (no browser):
//
// - startup → API ready (spawn → first 200 on /api/v1/server), empty and full catalog;
// - cold scan of Movies + TV created over POST /api/v1/libraries, on the corpus
//   as found (page cache warm-ish) and on a fresh APFS clone (closer to cold);
// - browse API latency while that scan runs, hammered by 4 concurrent clients,
//   and the same hammer at idle;
// - unchanged rescans over POST /libraries/{id}/scan;
// - RSS: idle after startup, during and after the scan, peak (sampled).
//
//   cd web && pnpm e2e:bench          (after building the release server)
//
// Env: FLUX_SERVER, E2E_WORK, E2E_PORT, HAMMER_CLIENTS (4), RESCANS (3), STARTS (5), SKIP_CLONE=1.
// Writes bench/results/flux-e2e-bench.json. Leaves $E2E_WORK/bench-data with all
// four libraries scanned, for e2e-playback.mjs.
import { execFileSync } from 'node:child_process';
import { existsSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { MEDIA, WORK, api, load1, machine, quietGate, sampleRss, sleep, startServer, summary, waitScansIdle, writeResult } from './e2e-lib.mjs';

const PORT = Number(process.env.E2E_PORT ?? 18497);
const CLIENTS = Number(process.env.HAMMER_CLIENTS ?? 4);
const RESCANS = Number(process.env.RESCANS ?? 3);
const STARTS = Number(process.env.STARTS ?? 5);
const BENCH_DATA = join(WORK, 'bench-data');
const CLONE_DATA = join(WORK, 'bench-clone-data');
const CLONE = join(WORK, 'scratch', 'library-clone');
const out = { name: 'flux-e2e-bench', generated_at: new Date().toISOString(), machine: machine(), quiet_wait_ms: 0, phases: {} };
const gate = async (what) => (out.quiet_wait_ms += await quietGate(what));

const SEARCH_TERMS = ['bla', 'the', 'matr', 'star', 'alien', 'doctor', 'break', 'love', 'night', 'war', 'sum', 'ame'];

/** Hammers the browse endpoints with `clients` concurrent loops until `until()` is true. */
async function hammer(base, libs, until, clients = CLIENTS) {
  const lat = {};
  const errors = {};
  let known = [];
  const routes = [
    ['libraries', () => '/api/v1/libraries'],
    ['home', () => '/api/v1/home'],
    ['items_page_title', () => `/api/v1/libraries/${libs.Movies}/items?limit=100`],
    ['items_page_recent', () => `/api/v1/libraries/${libs.Movies}/items?limit=100&sort=-added`],
    ['items_page_tv', () => `/api/v1/libraries/${libs.TV}/items?limit=100&sort=title`],
    ['search', (i) => `/api/v1/search?q=${SEARCH_TERMS[i % SEARCH_TERMS.length]}`],
    ['item_detail', (i) => (known.length ? `/api/v1/items/${known[i % known.length]}` : '/api/v1/libraries')],
  ];
  const loop = async (w) => {
    let i = w;
    while (!until()) {
      const [name, path] = routes[i % routes.length];
      const t0 = performance.now();
      try {
        const r = await fetch(base + path(i), { headers: { 'x-flux-request': '1' } });
        const body = await r.text();
        const ms = performance.now() - t0;
        if (!r.ok) errors[name] = (errors[name] ?? 0) + 1;
        (lat[name] ??= []).push(ms);
        if (name === 'items_page_title' && r.ok) {
          const ids = JSON.parse(body).items.map((x) => x.id);
          if (ids.length) known = ids;
        }
      } catch {
        errors[name] = (errors[name] ?? 0) + 1;
      }
      i += 1;
    }
  };
  const t0 = Date.now();
  await Promise.all(Array.from({ length: clients }, (_, w) => loop(w)));
  const wall = (Date.now() - t0) / 1000;
  const all = Object.values(lat).flat();
  return {
    clients,
    seconds: Math.round(wall * 10) / 10,
    requests: all.length,
    rps: Math.round(all.length / wall),
    all_ms: summary(all.map((x) => Math.round(x * 100) / 100)),
    by_endpoint_ms: Object.fromEntries(Object.entries(lat).map(([k, v]) => [k, summary(v.map((x) => Math.round(x * 100) / 100))])),
    errors,
  };
}

/** Creates libraries over the API and times the scan; hammers the API meanwhile. */
async function coldScan(label, dataDir, roots) {
  await gate(`cold scan (${label})`);
  rmSync(dataDir, { recursive: true, force: true });
  const s = await startServer({ dataDir, port: PORT, log: join(WORK, `server-bench-${label}.log`) });
  const rssStart = s.rss();
  const rss = sampleRss(s.pid, 200);
  const l0 = load1();
  const t0 = performance.now();
  const libs = {};
  for (const [name, kind, path] of roots) libs[name] = (await api(s.base, '/api/v1/libraries', { method: 'POST', body: JSON.stringify({ name, kind, roots: [path] }) })).id;
  let firstItemsMs = null;
  let done = false;
  let doneMs = null;
  const watcher = (async () => {
    while (!done) {
      const L = await api(s.base, '/api/v1/libraries');
      if (firstItemsMs === null && L.some((l) => l.item_count > 0)) firstItemsMs = performance.now() - t0;
      if (L.every((l) => l.scan.state !== 'scanning' && l.scan.finished_at !== null)) {
        doneMs = performance.now() - t0;
        done = true;
      } else await sleep(25);
    }
  })();
  const during = await hammer(s.base, libs, () => done);
  await watcher;
  const libsAfter = await api(s.base, '/api/v1/libraries');
  const l1 = load1();
  const scanPeak = rss.stop();
  await sleep(3000);
  const idleAfter = s.rss();
  const idle = await hammer(s.base, libs, (() => {
    const until = Date.now() + 8000;
    return () => Date.now() > until;
  })());
  // Unchanged rescans, through the API.
  const rescans = [];
  for (let r = 0; r < RESCANS; r++) {
    for (const [name] of roots) {
      const id = libs[name];
      const before = (await api(s.base, '/api/v1/libraries')).find((l) => l.id === id).scan;
      const t = performance.now();
      await api(s.base, `/api/v1/libraries/${id}/scan`, { method: 'POST' });
      let after;
      for (;;) {
        after = (await api(s.base, '/api/v1/libraries')).find((l) => l.id === id).scan;
        if (after.state !== 'scanning' && after.finished_at !== null && after.finished_at !== before.finished_at) break;
        await sleep(5);
      }
      rescans.push({
        library: name,
        api_wall_ms: Math.round(performance.now() - t),
        server_ms: after.finished_at - after.started_at,
        files_seen: after.files_seen,
        changed: after.items_added + after.items_updated + after.items_removed,
        files_probed: after.files_probed,
      });
    }
  }
  const stopped = await s.stop();
  return {
    label,
    load1: [l0, l1],
    startup_ready_ms: Math.round(s.readyMs),
    cold_scan: {
      wall_ms: Math.round(doneMs),
      first_items_browsable_ms: firstItemsMs === null ? null : Math.round(firstItemsMs),
      libraries: libsAfter.map((l) => ({ name: l.name, items: l.item_count, files: l.scan.files_seen, server_scan_ms: l.scan.finished_at - l.scan.started_at, state: l.scan.state, error: l.scan.last_error })),
      note: 'Both libraries are on one device, so flux-scan admits them one after the other; wall = POST of the first → both finished (polled every 25 ms).',
    },
    api_during_scan: during,
    api_idle: idle,
    unchanged_rescans: rescans,
    rescan_summary: Object.fromEntries(roots.map(([n]) => [n, summary(rescans.filter((r) => r.library === n).map((r) => r.server_ms))])),
    rss_mb: { startup: rssStart, peak_during_scan_sampled: scanPeak.peak_mb, idle_3s_after_scan: idleAfter, peak_with_children_time_l: stopped.peakMb },
    shutdown_ms: stopped.shutdownMs,
  };
}

// ── 1. cold scans ───────────────────────────────────────────────────────────
const LIB = `${MEDIA}/library`;
out.phases.scan_original = await coldScan('original', BENCH_DATA, [
  ['Movies', 'movies', `${LIB}/Movies`],
  ['TV', 'shows', `${LIB}/TV`],
]);
console.log('[bench] scan (original):', JSON.stringify(out.phases.scan_original.cold_scan), 'rescans', JSON.stringify(out.phases.scan_original.rescan_summary));

if (!process.env.SKIP_CLONE) {
  await gate('APFS clone');
  rmSync(CLONE, { recursive: true, force: true });
  const t = Date.now();
  execFileSync('nice', ['-n', '19', 'cp', '-cR', LIB, CLONE]);
  const cloneMs = Date.now() - t;
  out.phases.scan_fresh_clone = await coldScan('clone', CLONE_DATA, [
    ['Movies', 'movies', `${CLONE}/Movies`],
    ['TV', 'shows', `${CLONE}/TV`],
  ]);
  out.phases.scan_fresh_clone.clone_ms = cloneMs;
  out.phases.scan_fresh_clone.note =
    'A fresh APFS clone shares blocks on disk but not page-cache pages (macOS caches per file), so this is closer to an OS-cold scan; no purge without sudo.';
  console.log('[bench] scan (clone):', JSON.stringify(out.phases.scan_fresh_clone.cold_scan));
  rmSync(CLONE, { recursive: true, force: true });
  rmSync(CLONE_DATA, { recursive: true, force: true });
}

// ── 2. the other two libraries (for e2e-playback.mjs), then startup ─────────
await gate('startup runs');
{
  const s = await startServer({ dataDir: BENCH_DATA, port: PORT, log: join(WORK, 'server-bench-startup.log') });
  for (const [name, kind, path] of [
    ['Features', 'movies', `${MEDIA}/features`],
    ['Research', 'movies', `${MEDIA}/playback-research`],
  ]) {
    const have = (await api(s.base, '/api/v1/libraries')).some((l) => l.name === name);
    if (!have) await api(s.base, '/api/v1/libraries', { method: 'POST', body: JSON.stringify({ name, kind, roots: [path] }) });
  }
  await waitScansIdle(s.base);
  await s.stop();
}
const starts = { full_catalog: [], empty_data_dir: [] };
for (let i = 0; i < STARTS; i++) {
  const s = await startServer({ dataDir: BENCH_DATA, port: PORT, log: join(WORK, 'server-bench-startup.log') });
  const ready = s.readyMs;
  await sleep(3000); // jobs start 10 s after startup; the catch-up Quick scans run now
  const idle = s.rss();
  const st = await s.stop();
  starts.full_catalog.push({ ready_ms: Math.round(ready * 10) / 10, idle_rss_mb_3s: idle, shutdown_ms: st.shutdownMs });
}
for (let i = 0; i < 3; i++) {
  const dir = join(WORK, `empty-data-${i}`);
  rmSync(dir, { recursive: true, force: true });
  const s = await startServer({ dataDir: dir, port: PORT, log: join(WORK, 'server-bench-startup.log') });
  const ready = s.readyMs;
  await sleep(1000);
  const idle = s.rss();
  await s.stop();
  rmSync(dir, { recursive: true, force: true });
  starts.empty_data_dir.push({ ready_ms: Math.round(ready * 10) / 10, idle_rss_mb: idle });
}
out.phases.startup = {
  ...starts,
  full_catalog_ready_ms: summary(starts.full_catalog.map((x) => x.ready_ms)),
  empty_ready_ms: summary(starts.empty_data_dir.map((x) => x.ready_ms)),
  full_catalog_idle_rss_mb: summary(starts.full_catalog.map((x) => x.idle_rss_mb_3s)),
  note: 'spawn of `nice -n 19 flux-server` → first 200 on GET /api/v1/server (polled every 2 ms); the full catalog is the 16.7k-file library plus features and research (≈14.8k media files).',
};
console.log('[bench] startup:', JSON.stringify({ full: out.phases.startup.full_catalog_ready_ms, empty: out.phases.startup.empty_ready_ms, rss: out.phases.startup.full_catalog_idle_rss_mb }));
out.load1_end = load1();
out.bench_data = existsSync(BENCH_DATA) ? BENCH_DATA : null;
writeResult('bench', out);
