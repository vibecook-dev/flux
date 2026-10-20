#!/usr/bin/env node
// End-to-end product test: a real release flux-server on the real corpus, the
// production web client (web/dist built with VITE_FLUX_API=/), the system
// Google Chrome. It walks the product the way a person would — first run,
// browsing, playback of every plan type, seeks, track switches, resume, mark
// watched, the stats overlay, frame posters, an unavailable drive — records
// what worked, timings, console/HTTP errors, and screenshots of key screens.
//
//   cd web && VITE_FLUX_API=/ pnpm build && pnpm e2e:product
//
// Env: FLUX_SERVER (binary), E2E_WORK (scratch dir, wiped: data/ + scratch/),
// E2E_SHOTS, E2E_PORT, HEADFUL=1, ONLY=step,step (debugging; the first-run
// steps always run).
// Output: bench/results/flux-e2e-product.json and $E2E_SHOTS/*.png.
import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, renameSync, rmSync } from 'node:fs';
import { basename, join } from 'node:path';
import {
  MEDIA,
  NO_AV1_INIT,
  SHOTS,
  WORK,
  api,
  launchChrome,
  load1,
  machine,
  quietGate,
  sampleRss,
  sleep,
  startServer,
  summary,
  waitScansIdle,
  watchPage,
  writeResult,
} from './e2e-lib.mjs';

const PORT = Number(process.env.E2E_PORT ?? 18496);
const DATA = join(WORK, 'data');
const SCRATCH = join(WORK, 'scratch');
const CLONE = join(SCRATCH, 'features-clone');
const ONLY = process.env.ONLY ? new Set(process.env.ONLY.split(',')) : null;

const LIBRARIES = [
  { name: 'Movies', kind: 'Films', path: `${MEDIA}/library/Movies` },
  { name: 'TV', kind: 'TV shows', path: `${MEDIA}/library/TV` },
  { name: 'Features', kind: 'Films', path: `${MEDIA}/features` },
  { name: 'Research', kind: 'Films', path: `${MEDIA}/playback-research` },
];

const steps = [];
const errors = [];
let errSeen = 0;
let shotN = 0;
const t00 = Date.now();
let quietWaitMs = 0;

function newErrors() {
  const e = errors.slice(errSeen);
  errSeen = errors.length;
  return e;
}

async function step(name, fn, { always = false } = {}) {
  if (ONLY && !always && !ONLY.has(name)) return null;
  const t0 = performance.now();
  const l0 = load1();
  console.log(`\n[e2e] ── ${name}`);
  let rec;
  try {
    const r = (await fn()) ?? {};
    rec = { name, ok: r.ok ?? true, ms: Math.round(performance.now() - t0), load1: [l0, load1()], ...r };
  } catch (e) {
    rec = { name, ok: false, ms: Math.round(performance.now() - t0), load1: [l0, load1()], error: String(e?.stack ?? e).slice(0, 1500) };
  }
  const errs = newErrors();
  if (errs.length) rec.page_errors = errs;
  steps.push(rec);
  console.log(`[e2e] ${rec.ok ? 'ok  ' : 'FAIL'} ${name} (${rec.ms} ms)${rec.error ? `\n${rec.error}` : ''}${errs.length ? `\n  page errors: ${JSON.stringify(errs).slice(0, 800)}` : ''}`);
  return rec;
}

async function shot(page, name, opts = {}) {
  shotN += 1;
  const file = join(SHOTS, `${String(shotN).padStart(2, '0')}-${name}.png`);
  await page.screenshot({ path: file, ...opts });
  return file;
}

// ── catalog lookups through the API ────────────────────────────────────────

async function allItems(base, libId) {
  const out = [];
  let cursor = null;
  for (;;) {
    const q = new URLSearchParams({ limit: '500' });
    if (cursor) q.set('cursor', cursor);
    const page = await api(base, `/api/v1/libraries/${libId}/items?${q}`);
    out.push(...page.items);
    if (!page.next) return out;
    cursor = page.next;
  }
}

/** file name → {item, asset} for a small library. */
async function fileMap(base, libId) {
  const map = new Map();
  for (const it of await allItems(base, libId)) {
    const d = await api(base, `/api/v1/items/${it.id}`);
    for (const a of d.assets) map.set(basename(a.path), { item: it.id, asset: a.id, title: d.title, assets: d.assets.length });
  }
  return map;
}

// ── page helpers ────────────────────────────────────────────────────────────

/** Records SSE events the server sends, from inside the page (a second EventSource). */
async function recordEvents(page) {
  await page.evaluate(() => {
    if (window.__sse) return;
    window.__sse = [];
    const es = new EventSource('/api/v1/events');
    es.onmessage = (m) => {
      try {
        window.__sse.push({ t: performance.now(), wall: Date.now(), e: JSON.parse(m.data) });
      } catch {
        /* comment */
      }
    };
  });
}

async function gotoItem(page, base, id) {
  await page.goto(`${base}/item/${id}`, { waitUntil: 'domcontentloaded' });
  await page.locator('h1').first().waitFor();
  await page.waitForLoadState('networkidle').catch(() => undefined);
}

/** Install an independent first-frame probe and the click clock, like bench-playback.mjs. */
async function armFirstFrameProbe(page) {
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
}

/** Opens the stats overlay and reads the plan pill + explain sentences. */
async function readStats(page) {
  const overlay = page.locator('section[aria-label="Playback details"]');
  if (!(await overlay.isVisible().catch(() => false))) {
    await page.mouse.move(500, 300);
    await page.keyboard.press('i');
  }
  await overlay.waitFor({ timeout: 5000 });
  await page.waitForTimeout(700); // diagnostics poll
  const pill = (await overlay.locator('header span').first().textContent())?.trim();
  const reasons = (await overlay.locator('ul li').allTextContents()).map((s) => s.trim());
  const readout = await overlay.locator('dl').first().innerText();
  return { label: pill, reasons, readout: readout.replace(/\n+/g, ' | ').slice(0, 900) };
}

async function seekOnce(page, target) {
  return page.evaluate(async (target) => {
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
    return { target, harness: h === null ? null : Math.round(h), app: window.__fluxPlayback.metrics().lastSeekMs, landed: v.currentTime, error: v.error ? v.error.message || String(v.error.code) : null };
  }, target);
}

const SEEK_FRACTIONS = [0.5, 0.15, 0.82, 0.33, 0.95, 0.05, 0.62, 0.25, 0.71, 0.42];

/**
 * From an item page: hover Play, click, wait for the first frame; then N seeks,
 * read the stats overlay, screenshot. Returns the measurements.
 */
async function playFromItem(page, base, { item, asset = null, seeks = 10, name, expect, shotName }) {
  await gotoItem(page, base, item);
  if (asset) {
    const radio = page.locator('input[name="version"]');
    if ((await radio.count()) > 1) {
      // Pick the version whose file matches (FilesPanel order = assets order = picker order).
      const d = await api(base, `/api/v1/items/${item}`);
      const idx = d.assets.findIndex((a) => a.id === asset);
      // The radio is visually hidden inside its label card.
      if (idx >= 0) await radio.nth(idx).check({ force: true });
    }
  }
  const button = page.locator('button', { hasText: /^(Resume|Play)/ }).first();
  await button.waitFor();
  // "Expected on this device" (server /plan) — compared with what Play does.
  const expectedEl = page.locator('p[data-tone] strong').first();
  await expectedEl.waitFor({ timeout: 5000 }).catch(() => undefined);
  const expectedLabel = (await expectedEl.textContent().catch(() => null))?.trim() ?? null;
  await button.hover();
  await page.waitForTimeout(200);
  await armFirstFrameProbe(page);
  await button.click();
  await page.waitForFunction(() => window.__fluxPlayback?.metrics().ttffMs != null && window.__h?.first != null, null, { timeout: 45_000 });
  const first = await page.evaluate(() => ({
    app: window.__fluxPlayback.metrics().ttffMs,
    harness: Math.round(window.__h.first),
    plan: window.__fluxPlayback.plan,
    session: window.__fluxPlayback.sessionId,
  }));
  await page.waitForTimeout(800);
  const duration = await page.evaluate(() => document.querySelector('video').duration);
  const seekRes = [];
  for (let i = 0; i < seeks; i++) {
    const target = Math.max(1, Math.min(duration - 2.5, SEEK_FRACTIONS[i % SEEK_FRACTIONS.length] * duration));
    seekRes.push(await seekOnce(page, Math.round(target * 10) / 10));
    await page.waitForTimeout(250);
  }
  const playing = await page.evaluate(async () => {
    const v = document.querySelector('video');
    const a = v.currentTime;
    await new Promise((r) => setTimeout(r, 1200));
    return { advanced: v.currentTime - a, paused: v.paused, error: v.error?.code ?? null, w: v.videoWidth, h: v.videoHeight };
  });
  const stats = await readStats(page);
  const shotFile = await shot(page, shotName ?? `play-${name}`);
  const ok =
    playing.advanced > 0.3 &&
    !playing.error &&
    (expect ? stats.label === expect : true) &&
    expectedLabel === stats.label &&
    seekRes.every((s) => s.harness !== null && !s.error);
  return {
    ok,
    expect,
    expected_on_item_page: expectedLabel,
    label: stats.label,
    reasons: stats.reasons,
    readout: stats.readout,
    delivery: first.plan?.delivery,
    ttff_ms: { app: first.app, harness: first.harness },
    seeks: seekRes,
    seek_ms: summary(seekRes.map((s) => s.harness)),
    after_seeks: playing,
    shot: shotFile,
    session: first.session,
  };
}

async function backToDetails(page) {
  await page.mouse.move(500, 300);
  const b = page.getByRole('button', { name: 'Back to details' }).first();
  if (await b.isVisible().catch(() => false)) await b.click();
  else await page.keyboard.press('Escape');
  await page.waitForTimeout(600);
}

// ── the walk ────────────────────────────────────────────────────────────────

quietWaitMs += await quietGate('the product walk');
console.log(`[e2e] fresh data dir ${DATA}, scratch clone ${CLONE}`);
rmSync(DATA, { recursive: true, force: true });
rmSync(SCRATCH, { recursive: true, force: true });
mkdirSync(SCRATCH, { recursive: true });
// An APFS clone (instant, shares blocks) — the only thing this test renames.
execFileSync('cp', ['-cR', `${MEDIA}/features`, CLONE]);

const server = await startServer({ dataDir: DATA, port: PORT, log: join(WORK, 'server-product.log') });
const base = server.base;
console.log(`[e2e] server ready in ${server.readyMs.toFixed(1)} ms (pid ${server.pid})`);
const rssSamples = [{ at: 'startup', mb: server.rss() }];
const rssSampler = sampleRss(server.pid, 500);

const browser = await launchChrome();
const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: 'dark' });
const page = await ctx.newPage();
watchPage(page, errors);
const libIds = {};
let features = new Map();
let research = new Map();
const playResults = {};
const ctxState = {};

await step(
  'first-run-empty-home',
  async () => {
    await page.goto(base + '/', { waitUntil: 'networkidle' });
    await page.getByText('Your library is empty').waitFor({ timeout: 10_000 });
    const s = await shot(page, 'first-run-empty-home');
    return { shot: s, server_ready_ms: Math.round(server.readyMs) };
  },
  { always: true },
);

await step(
  'first-run-add-libraries-ui',
  async () => {
    await page.getByRole('link', { name: 'Add a library' }).click();
    await page.getByRole('heading', { name: /Libraries/ }).waitFor();
    await recordEvents(page);
    await page.locator('main header p[data-state="open"]').waitFor({ timeout: 10_000 });
    const perLib = [];
    for (const lib of LIBRARIES) {
      await page.getByRole('button', { name: 'Add library' }).click();
      const dlg = page.locator('dialog[open]');
      await dlg.getByLabel('Name').fill(lib.name);
      await dlg.getByLabel(lib.kind, { exact: true }).check();
      await dlg.getByLabel('Folder 1').fill(lib.path);
      const evBefore = await page.evaluate(() => window.__sse.length);
      const tClick = await page.evaluate(() => performance.now());
      await dlg.getByRole('button', { name: 'Add and scan' }).click();
      await dlg.waitFor({ state: 'detached' }).catch(() => undefined);
      await page.waitForFunction(() => !document.querySelector('dialog[open]'));
      // The library card appears and goes live.
      const card = page.locator('article', { has: page.getByRole('link', { name: lib.name, exact: true }) });
      await card.waitFor();
      let midShot = null;
      let sawScanningInUi = false;
      let uiCounters = null;
      const deadline = Date.now() + 300_000;
      for (;;) {
        const st = await card.getAttribute('data-state');
        if (st === 'scanning') {
          sawScanningInUi = true;
          uiCounters = (await card.locator('dl').innerText().catch(() => '')).replace(/\n+/g, ' ');
          if (!midShot && (lib.name === 'Movies' || lib.name === 'TV')) midShot = await shot(page, `first-run-scanning-${lib.name.toLowerCase()}`);
        }
        if (st === 'watching' || st === 'idle' || st === 'error' || st === 'unavailable') {
          const txt = await card.innerText();
          if (st !== 'idle' || /Last scan/.test(txt)) break;
        }
        if (Date.now() > deadline) throw new Error(`${lib.name}: scan did not finish (state ${st})`);
        await page.waitForTimeout(150);
      }
      const ev = await page.evaluate(
        ([from, t]) =>
          window.__sse.slice(from).map((x) => ({ dt: Math.round(x.t - t), type: x.e.type, lib: x.e.library_id, state: x.e.status?.state, seen: x.e.status?.files_seen, total: x.e.status?.files_total, added: x.e.added?.length })),
        [evBefore, tClick],
      );
      const libs = await api(base, '/api/v1/libraries');
      const L = libs.find((l) => l.name === lib.name);
      libIds[lib.name] = L.id;
      const mine = ev.filter((e) => e.lib === L.id);
      const progress = mine.filter((e) => e.type === 'scan_progress');
      const items = mine.filter((e) => e.type === 'items_changed');
      perLib.push({
        library: lib.name,
        id: L.id,
        item_count: L.item_count,
        scan: L.scan,
        server_scan_ms: L.scan.finished_at - L.scan.started_at,
        click_to_first_progress_ms: progress[0]?.dt ?? null,
        click_to_first_items_ms: items[0]?.dt ?? null,
        click_to_done_event_ms: progress.find((e) => e.state && e.state !== 'scanning')?.dt ?? null,
        sse_scan_progress: progress.length,
        sse_items_changed: items.length,
        ui_showed_scanning: sawScanningInUi,
        ui_counters_last: uiCounters,
        card_text: (await card.innerText()).replace(/\n+/g, ' | ').slice(0, 400),
        shot: midShot,
      });
    }
    // Each card shows its library's real item count without a reload (it said
    // "0 items" until the page was revisited: the refresh after a scan was lost).
    await page.waitForTimeout(1500);
    const libsNow = await api(base, '/api/v1/libraries');
    const cardCounts = {};
    for (const L of libsNow) {
      const edge = await page.locator('article', { has: page.getByRole('link', { name: L.name, exact: true }) }).locator('p.edge').first().innerText();
      cardCounts[L.name] = { card: edge.replace(/\s+/g, ' '), api_items: L.item_count };
    }
    const countsOk = libsNow.every((L) => cardCounts[L.name].card.toLowerCase().includes(`${L.item_count.toLocaleString()} items`));
    const s = await shot(page, 'first-run-admin-done');
    return { ok: countsOk, libraries: perLib, card_counts: cardCounts, shot: s };
  },
  { always: true },
);

rssSamples.push({ at: 'after first-run scan', mb: server.rss() });
if (!libIds.Features) {
  // The UI step failed; carry on with libraries made through the API so the rest still runs.
  for (const lib of LIBRARIES) {
    const have = (await api(base, '/api/v1/libraries')).find((l) => l.name === lib.name);
    const L = have ?? (await api(base, '/api/v1/libraries', { method: 'POST', body: JSON.stringify({ name: lib.name, kind: lib.kind === 'TV shows' ? 'shows' : 'movies', roots: [lib.path] }) }));
    libIds[lib.name] = L.id;
  }
  await waitScansIdle(base);
}
features = await fileMap(base, libIds.Features);
research = await fileMap(base, libIds.Research);

await step('home', async () => {
  await page.goto(base + '/', { waitUntil: 'networkidle' });
  await page.locator('#hero-title').waitFor();
  const rows = await page.locator('section h2').allTextContents();
  const cards = await page.locator('main a[href^="/item/"]').count();
  // No typeset fallback title behind the hero's own heading (it overlapped the meta line).
  const heroGhost = await page.locator('section[aria-labelledby="hero-title"] [role="img"] span, section[aria-labelledby="hero-title"] [aria-hidden="true"] span').count();
  const s = await shot(page, 'home');
  return { ok: heroGhost === 0, rows, cards, hero_fallback_titles: heroGhost, shot: s };
});

await step('navigation-latency', async () => {
  // Card click → the item's title painted (from the clicked card's summary), → its detail loaded.
  const samples = [];
  for (let i = 0; i < 6; i++) {
    await page.goto(`${base}/library/${libIds.Movies}?sort=-year`, { waitUntil: 'networkidle' });
    const card = page.locator('[role="grid"] a[href^="/item/"]').nth(i * 3);
    const href = await card.getAttribute('href');
    const title = (await card.getAttribute('aria-label')).split(',')[0];
    await card.hover();
    await page.waitForTimeout(250);
    const r = await page.evaluate(
      ([href, title]) =>
        new Promise((resolve) => {
          const a = document.querySelector(`[role="grid"] a[href="${href}"]`);
          const t0 = performance.now();
          let titleAt = null;
          const tick = () => {
            const h1 = document.querySelector('main h1');
            if (titleAt === null && location.pathname === href && h1?.textContent === title) titleAt = performance.now() - t0;
            if (titleAt !== null && document.querySelector('#files')) return resolve({ title_ms: Math.round(titleAt), detail_ms: Math.round(performance.now() - t0) });
            if (performance.now() - t0 > 5000) return resolve({ title_ms: titleAt, detail_ms: null });
            requestAnimationFrame(tick);
          };
          a.click();
          requestAnimationFrame(tick);
        }),
      [href, title],
    );
    samples.push(r);
  }
  return {
    ok: samples.every((x) => x.detail_ms !== null),
    title_painted_ms: summary(samples.map((x) => x.title_ms)),
    detail_loaded_ms: summary(samples.map((x) => x.detail_ms)),
    samples,
    how: 'hover 250 ms (prefetch), click; polled each animation frame',
  };
});

await step('library-grid-movies', async () => {
  const t0 = Date.now();
  await page.getByRole('navigation', { name: 'Main' }).first().getByRole('link', { name: /^Movies/ }).click();
  await page.locator('[role="grid"] a[href^="/item/"]').first().waitFor();
  const firstCellMs = Date.now() - t0;
  await page.waitForFunction(() => !/loaded/.test(document.querySelector('header p')?.textContent ?? ''), null, { timeout: 30_000 }).catch(() => undefined);
  const header = await page.locator('main header p').first().textContent();
  const top = await shot(page, 'library-movies');
  // Fling through the whole grid and measure frame times (rAF deltas) + long tasks.
  const perf = await page.evaluate(async () => {
    const frames = [];
    let last = performance.now();
    let run = true;
    const loop = (t) => {
      frames.push(t - last);
      last = t;
      if (run) requestAnimationFrame(loop);
    };
    requestAnimationFrame(loop);
    const longTasks = [];
    const po = new PerformanceObserver((l) => longTasks.push(...l.getEntries().map((e) => Math.round(e.duration))));
    try {
      po.observe({ type: 'longtask', buffered: false });
    } catch {
      /* unsupported */
    }
    const h = document.documentElement.scrollHeight;
    for (let y = 0; y < h; y += 1400) {
      window.scrollTo(0, y);
      await new Promise((r) => setTimeout(r, 16));
    }
    for (let y = h; y > 0; y -= 900) {
      window.scrollTo(0, y);
      await new Promise((r) => setTimeout(r, 16));
    }
    await new Promise((r) => setTimeout(r, 300));
    run = false;
    po.disconnect();
    const s = [...frames].sort((a, b) => a - b);
    const q = (p) => Math.round(s[Math.min(s.length - 1, Math.floor(p * s.length))] * 10) / 10;
    return {
      frames: frames.length,
      p50: q(0.5),
      p95: q(0.95),
      p99: q(0.99),
      janky_over_25ms: frames.filter((f) => f > 25).length,
      long_tasks: longTasks,
      cards_in_dom: document.querySelectorAll('[role="grid"] a[href^="/item/"]').length,
      dom_nodes: document.getElementsByTagName('*').length,
      heap_mb: performance.memory ? Math.round(performance.memory.usedJSHeapSize / 1048576) : null,
      scroll_height: h,
    };
  });
  await page.mouse.wheel(0, 9000);
  await page.waitForTimeout(800);
  const mid = await shot(page, 'library-movies-scrolled');
  return { header, first_cell_ms: firstCellMs, scroll: perf, shots: [top, mid] };
});

await step('library-grid-tv-sorted', async () => {
  await page.goto(`${base}/library/${libIds.TV}?sort=-added`, { waitUntil: 'networkidle' });
  await page.locator('[role="grid"] a[href^="/item/"]').first().waitFor();
  const header = await page.locator('main header p').first().textContent();
  const s = await shot(page, 'library-tv-recent');
  // Every sort answers through the UI.
  const sorts = {};
  for (const v of ['title', '-title', 'year', '-year', 'added', '-added']) {
    await page.locator('main select').selectOption(v);
    await page.waitForTimeout(400);
    sorts[v] = (await page.locator('[role="grid"] a[href^="/item/"]').first().getAttribute('aria-label')) ?? null;
  }
  return { header, first_card_by_sort: sorts, shot: s };
});

await step('search', async () => {
  await page.goto(base + '/', { waitUntil: 'networkidle' });
  await page.keyboard.press('/');
  const dlg = page.locator('dialog[open]');
  await dlg.waitFor();
  const t0 = Date.now();
  await page.keyboard.type('blade run', { delay: 40 });
  await dlg.locator('a[href^="/item/"], [role="option"]').first().waitFor({ timeout: 10_000 });
  const firstResultMs = Date.now() - t0;
  const results = (await dlg.locator('a[href^="/item/"], [role="option"]').allInnerTexts()).map((s) => s.replace(/\n+/g, ' · ')).slice(0, 8);
  // Thumbnails stay thumbnail-sized without art (they were 156 px wide with the title stacked letter by letter).
  const thumbs = await dlg.locator('[role="option"] [role="img"]').evaluateAll((els) => els.map((e) => Math.round(e.getBoundingClientRect().width)));
  const s = await shot(page, 'search');
  await page.keyboard.press('Enter');
  await page.waitForURL(/\/item\/\d+/, { timeout: 10_000 });
  await page.locator('h1').first().waitFor();
  const landed = await page.locator('h1').first().textContent();
  ctxState.searchItem = Number(page.url().match(/item\/(\d+)/)[1]);
  return { ok: thumbs.every((w) => w <= 48), first_result_ms: firstResultMs, results, thumb_widths: thumbs, landed, shot: s };
});

await step('item-movie', async () => {
  const id = ctxState.searchItem;
  await gotoItem(page, base, id);
  await page.waitForTimeout(400);
  await page.locator('p[data-tone] strong').first().waitFor({ timeout: 5000 }).catch(() => undefined);
  const expected = await page.locator('p[data-tone]').first().innerText().catch(() => null);
  const s = await shot(page, 'item-movie', { fullPage: true });
  return { id, expected_on_device: expected, shot: s };
});

await step('item-show-season-episode', async () => {
  const shows = await allItems(base, libIds.TV);
  const show = shows.find((s) => s.title === 'Breaking Bad') ?? shows.find((s) => (s.child_count ?? 0) > 2);
  await gotoItem(page, base, show.id);
  await page.locator('[role="tablist"] [role="tab"]').first().waitFor();
  const tabs = await page.locator('[role="tablist"] [role="tab"]').allInnerTexts();
  const sShow = await shot(page, 'item-show', { fullPage: true });
  const seasons = await api(base, `/api/v1/items/${show.id}/children`);
  const season = seasons[Math.min(1, seasons.length - 1)];
  await page.goto(`${base}/item/${season.id}`, { waitUntil: 'networkidle' });
  await page.waitForURL(new RegExp(`/item/${show.id}\\?season=${season.id}`), { timeout: 10_000 });
  const selected = await page.locator('[role="tab"][aria-selected="true"]').innerText();
  const sSeason = await shot(page, 'item-season');
  const eps = await api(base, `/api/v1/items/${season.id}/children`);
  await page.locator(`a[href="/item/${eps[0].id}"]`).first().click();
  await page.waitForURL(new RegExp(`/item/${eps[0].id}`));
  await page.locator('h1').first().waitFor();
  // SPA navigation: the load state is already 'networkidle'; wait for the detail itself.
  await page.locator('#files').waitFor({ timeout: 10_000 });
  const backLink = await page.locator('a', { hasText: show.title }).first().isVisible();
  const sEp = await shot(page, 'item-episode', { fullPage: true });
  ctxState.episode = eps[0].id;
  ctxState.show = show.id;
  return { show: show.title, season_tabs: tabs, season_redirect_selected: selected, episode: eps[0].title, back_to_show_link: backLink, shots: [sShow, sSeason, sEp] };
});

await step('item-unmatched', async () => {
  // No TMDB key and no exports: nothing is matched. Pick the messiest parse.
  const f = features.get('h264_truehd71.mkv');
  await gotoItem(page, base, f.item);
  const d = await api(base, `/api/v1/items/${f.item}`);
  const matchPanel = await page.locator('#matched-as').count();
  const s = await shot(page, 'item-unmatched', { fullPage: true });
  return { title: d.title, match_confidence: d.match_confidence, match_evidence: d.match_evidence, match_panel_shown: matchPanel > 0, shot: s };
});

// ── playback ────────────────────────────────────────────────────────────────

const PLAYS = [
  { name: 'direct-mp4', file: 'h264_aac_faststart.mp4', expect: 'Direct play' },
  { name: 'direct-mkv', file: 'h264_long_gop.mkv', expect: 'Direct play' },
  { name: 'hls-remux-hevc-hdr10-4k', file: 'hevc_main10_hdr10_eac3.mkv', expect: 'Audio conversion', note: '4K HEVC Main10 HDR10 video copied over HLS; E-AC-3 → AAC' },
  { name: 'hls-remux-4k-60mbps', file: 'hevc_4k_60mbps.mkv', expect: 'Audio conversion', note: 'HEVC level 5.2 (was a 4K→1080p transcode)' },
  { name: 'audio-truehd', file: 'h264_truehd71.mkv', expect: 'Audio conversion' },
  { name: 'audio-dts', file: 'h264_dts51.mkv', expect: 'Audio conversion' },
  { name: 'remux-flac', file: 'h264_flac.mkv', expect: null },
  { name: 'direct-mp4-moov-end', file: 'h264_aac_moov_end.mp4', expect: 'Direct play' },
  { name: 'ts-h264-m2ts', file: 'h264_aac.m2ts', expect: 'Direct stream', note: 'H.264/AAC in M2TS: remuxed in process (Phase D TS path), video and audio copied' },
  { name: 'transcode-mpeg2-ts', file: 'mpeg2_ac3.ts', expect: 'Transcode' },
  { name: 'transcode-avi', file: 'mpeg4_mp3.avi', expect: 'Transcode' },
  { name: 'long-2h', file: 'long_2h_360p.mkv', expect: 'Direct play', note: 'Constrained Baseline H.264 (was transcoded)' },
];
const RESEARCH_PLAYS = [
  { name: 'research-h264-10min', file: 'synth_h264_10min.mkv', expect: null },
  { name: 'research-hevc10-opengop-10min', file: 'synth_hevc10_opengop_10min.mkv', expect: null },
];

for (const p of [...PLAYS, ...RESEARCH_PLAYS]) {
  await step(`play-${p.name}`, async () => {
    quietWaitMs += await quietGate(`play-${p.name}`);
    const f = features.get(p.file) ?? research.get(p.file);
    if (!f) throw new Error(`no item for ${p.file}`);
    const r = await playFromItem(page, base, { item: f.item, asset: f.asset, name: p.name, expect: p.expect });
    playResults[p.name] = r;
    await backToDetails(page);
    return { file: p.file, note: p.note, ...r };
  });
}

await step('play-transcode-av1-no-av1-device', async () => {
  // A second context whose Chrome reports no AV1 decoder (MSE, canPlayType, mediaCapabilities).
  const c2 = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: 'dark' });
  await c2.addInitScript(NO_AV1_INIT);
  const p2 = await c2.newPage();
  watchPage(p2, errors);
  const f = features.get('av1_opus.mkv');
  const r = await playFromItem(p2, base, { item: f.item, asset: f.asset, name: 'transcode-av1', expect: 'Transcode' });
  playResults['transcode-av1'] = r;
  await c2.close();
  return { file: 'av1_opus.mkv', ...r };
});

await step('play-av1-capable-device', async () => {
  const f = features.get('av1_opus.mkv');
  const r = await playFromItem(page, base, { item: f.item, asset: f.asset, name: 'av1-native', seeks: 3 });
  await backToDetails(page);
  return { file: 'av1_opus.mkv', note: 'same file, Chrome with its AV1 decoder', ...r };
});

await step('play-tv-episode', async () => {
  const r = await playFromItem(page, base, { item: ctxState.episode, name: 'tv-episode', seeks: 3 });
  await backToDetails(page);
  return r;
});

await step('audio-track-switch', async () => {
  const f = features.get('h264_multi_audio_subs.mkv');
  const r = await playFromItem(page, base, { item: f.item, asset: f.asset, name: 'multi-audio', seeks: 2, shotName: 'play-multi-audio' });
  const before = await page.evaluate(() => ({ s: window.__fluxPlayback.sessionId, a: window.__fluxPlayback.plan.audio.map((x) => x.track) }));
  await page.mouse.move(600, 400);
  await page.getByRole('button', { name: 'Audio' }).click();
  const options = await page.locator('[role="menu"][aria-label="Audio"] [role="menuitemradio"]').allInnerTexts();
  const t0 = Date.now();
  await page.locator('[role="menu"][aria-label="Audio"] [role="menuitemradio"][aria-checked="false"]').first().click();
  await page.waitForFunction((s) => window.__fluxPlayback?.sessionId !== s && window.__fluxPlayback.metrics().ttffMs != null, before.s, { timeout: 30_000 });
  const switchMs = Date.now() - t0;
  const after = await page.evaluate(() => ({ s: window.__fluxPlayback.sessionId, a: window.__fluxPlayback.plan.audio.map((x) => x.track), t: document.querySelector('video').currentTime, ttff: window.__fluxPlayback.metrics().ttffMs }));
  await page.waitForTimeout(1500);
  const advancing = await page.evaluate(async () => {
    const v = document.querySelector('video');
    const a = v.currentTime;
    await new Promise((r) => setTimeout(r, 1000));
    return v.currentTime - a;
  });
  const stats = await readStats(page);
  const s = await shot(page, 'audio-switched');
  // And back: the first track is now a rendition of this HLS session, so the
  // player switches in place (no new /play; app time = click → hls.js
  // AUDIO_TRACK_SWITCHED).
  const nBefore = await page.evaluate(() => window.__fluxPlayback.audio().switches.length);
  await page.mouse.move(600, 400);
  await page.getByRole('button', { name: 'Audio' }).click();
  const t1 = Date.now();
  await page.locator('[role="menu"][aria-label="Audio"] [role="menuitemradio"][aria-checked="false"]').first().click();
  await page.waitForFunction((n) => window.__fluxPlayback.audio().switches.length > n, nBefore, { timeout: 30_000 });
  const backWallMs = Date.now() - t1;
  const back = await page.evaluate(async () => {
    const v = document.querySelector('video');
    const a = v.currentTime;
    await new Promise((r) => setTimeout(r, 1000));
    return { s: window.__fluxPlayback.sessionId, sw: window.__fluxPlayback.audio().switches.at(-1), rendition: window.__fluxPlayback.audio().rendition, advanced: v.currentTime - a };
  });
  await backToDetails(page);
  const switchBack = { same_session: back.s === after.s, mode: back.sw.mode, app_ms: back.sw.ms, wall_ms: backWallMs, rendition: back.rendition, advanced_s: back.advanced };
  return {
    ok: after.a[0] !== before.a[0] && advancing > 0.3 && switchBack.same_session && switchBack.mode === 'in-session' && back.advanced > 0.3,
    options,
    before,
    after,
    switch_to_first_frame_ms: switchMs,
    switch_back_in_session: switchBack,
    label_after: stats.label,
    reasons_after: stats.reasons,
    first_play: { label: r.label, ttff_ms: r.ttff_ms },
    shot: s,
  };
});

await step('subtitles', async () => {
  const f = features.get('h264_multi_audio_subs.mkv');
  await gotoItem(page, base, f.item);
  const button = page.locator('button', { hasText: /^(Resume|Play)/ }).first();
  await button.hover();
  await button.click();
  await page.waitForFunction(() => window.__fluxPlayback?.metrics().ttffMs != null, null, { timeout: 30_000 });
  const res = [];
  await page.mouse.move(600, 400);
  await page.getByRole('button', { name: 'Subtitles' }).click();
  const options = await page.locator('[role="menu"][aria-label="Subtitles"] [role="menuitemradio"]').allInnerTexts();
  for (let i = 1; i < options.length; i++) {
    await page.mouse.move(600, 400);
    if (!(await page.locator('[role="menu"][aria-label="Subtitles"]').isVisible().catch(() => false))) await page.getByRole('button', { name: 'Subtitles' }).click();
    await page.locator('[role="menu"][aria-label="Subtitles"] [role="menuitemradio"]').nth(i).click();
    const r = await page.evaluate(async () => {
      const v = document.querySelector('video');
      const t0 = performance.now();
      let tt = null;
      while (performance.now() - t0 < 8000) {
        tt = [...v.textTracks].find((t) => t.mode === 'showing');
        if (tt && tt.cues && tt.cues.length > 0) break;
        await new Promise((r) => setTimeout(r, 50));
      }
      if (!tt || !tt.cues || tt.cues.length === 0) return { showing: tt ? tt.label : null, cues: 0 };
      const cue = tt.cues[Math.min(2, tt.cues.length - 1)];
      v.currentTime = cue.startTime + 0.2;
      await new Promise((r) => v.addEventListener('seeked', r, { once: true }));
      await new Promise((r) => setTimeout(r, 400));
      return { showing: tt.label, lang: tt.language, cues: tt.cues.length, cue_start: cue.startTime, active: [...(tt.activeCues ?? [])].map((c) => c.text), loaded_ms: Math.round(performance.now() - t0) };
    });
    await page.mouse.move(10, 10);
    await page.waitForTimeout(3200); // controls auto-hide, so the cue is unobstructed
    r.shot = await shot(page, `subtitles-${i}`);
    res.push({ option: options[i], ...r });
  }
  await backToDetails(page);
  return { ok: res.length > 0 && res.every((r) => r.cues > 0 && r.active.length > 0), options, tracks: res };
});

await step('resume-continue-watching', async () => {
  const f = features.get('long_2h_360p.mkv');
  await gotoItem(page, base, f.item);
  await page.locator('button', { hasText: /^(Resume|Play)/ }).first().click();
  await page.waitForFunction(() => window.__fluxPlayback?.metrics().ttffMs != null, null, { timeout: 30_000 });
  const target = 1834;
  await seekOnce(page, target);
  await page.waitForTimeout(4000);
  const left = await page.evaluate(() => document.querySelector('video').currentTime);
  await backToDetails(page); // unmount reports progress with keepalive
  await page.waitForTimeout(800);
  // The position stored is where we left (the unmount report), not the last seek (1834 s).
  const stored = (await api(base, `/api/v1/items/${f.item}`)).progress;
  await page.goto(base + '/', { waitUntil: 'networkidle' });
  const heroTitle = await page.locator('#hero-title').textContent();
  const eyebrow = await page.locator('section[aria-labelledby="hero-title"] p.edge').first().textContent();
  const sHome = await shot(page, 'home-continue-watching');
  const resume = page.locator('section[aria-labelledby="hero-title"] button', { hasText: 'Resume' });
  await resume.hover();
  await armFirstFrameProbe(page);
  await resume.click();
  await page.waitForFunction(() => window.__fluxPlayback?.metrics().ttffMs != null && window.__h?.first != null, null, { timeout: 30_000 });
  const r = await page.evaluate(() => ({ t: document.querySelector('video').currentTime, ttff: window.__fluxPlayback.metrics().ttffMs, harness: Math.round(window.__h.first) }));
  const sPlay = await shot(page, 'resumed');
  await backToDetails(page);
  return {
    ok: Math.abs(r.t - left) < 15 && /Continue watching/.test(eyebrow ?? '') && stored.position_ms >= (left - 1.5) * 1000,
    left_at_s: Math.round(left * 10) / 10,
    stored_on_leave_s: stored.position_ms / 1000,
    resumed_at_s: Math.round(r.t * 10) / 10,
    hero: heroTitle,
    eyebrow,
    resume_ttff_ms: { app: r.ttff, harness: r.harness },
    shots: [sHome, sPlay],
  };
});

await step('mark-watched', async () => {
  const f = features.get('long_2h_360p.mkv');
  await gotoItem(page, base, f.item);
  const before = (await api(base, `/api/v1/items/${f.item}`)).progress;
  // Earlier steps seeked past 90 %, so this film may already be watched (a rewatch in progress).
  const reset = page.getByRole('button', { name: /^Mark (as )?unwatched/i });
  if ((await reset.count()) > 0) {
    await reset.first().click();
    await page.getByRole('button', { name: /^Mark (as )?watched/i }).waitFor({ timeout: 5000 });
  }
  const btn = page.getByRole('button', { name: /^Mark (as )?watched/i });
  if ((await btn.count()) === 0) return { ok: false, error: 'no "Mark watched" control in the item page' };
  await btn.first().click();
  const unmark = page.getByRole('button', { name: /^Mark (as )?unwatched/i });
  await unmark.waitFor({ timeout: 5000 });
  const d = await api(base, `/api/v1/items/${f.item}`);
  const resumeGone = (await page.locator('button', { hasText: /^Resume/ }).count()) === 0;
  const s = await shot(page, 'mark-watched');
  const home = await api(base, '/api/v1/home');
  const stillContinue = home.continue_watching.some((i) => i.id === f.item);
  // The card in its library shows the watched mark.
  await page.goto(`${base}/library/${libIds.Features}`, { waitUntil: 'networkidle' });
  const card = page.locator(`[role="grid"] a[href="/item/${f.item}"]`).first();
  const cardLabel = await card.getAttribute('aria-label');
  // And back.
  await gotoItem(page, base, f.item);
  await page.getByRole('button', { name: /^Mark (as )?unwatched/i }).click();
  await page.getByRole('button', { name: /^Mark (as )?watched/i }).waitFor({ timeout: 5000 });
  const after = (await api(base, `/api/v1/items/${f.item}`)).progress;
  return {
    ok: Boolean(d.progress?.watched) && !stillContinue && resumeGone && /watched/.test(cardLabel ?? '') && after?.watched === false,
    before,
    progress: d.progress,
    resume_button_gone: resumeGone,
    still_in_continue_watching: stillContinue,
    card_label: cardLabel,
    after_unmark: after,
    shot: s,
  };
});

await step('stats-overlay-explain', async () => {
  // Collected from the plays above: every explain sentence per plan.
  const byCase = Object.fromEntries(Object.entries(playResults).map(([k, v]) => [k, { label: v.label, reasons: v.reasons }]));
  const debugNames = /\b(TrueHd|Eac3|Ac3|Hevc|Matroska|Mpeg2|DolbyVision|Mp4|MpegTs|Avi)\b/;
  const raw = Object.entries(byCase).flatMap(([k, v]) => v.reasons.filter((r) => debugNames.test(r)).map((r) => `${k}: ${r}`));
  return { ok: raw.length === 0, by_case: byCase, debug_names_in_sentences: raw };
});

await step('frame-posters', async () => {
  // Jobs pause during playback (30 s) and scans; give the art queue a minute.
  const count = async () => {
    const libs = {};
    for (const [name, id] of Object.entries(libIds)) {
      const items = await allItems(base, id);
      libs[name] = { items: items.length, frame_posters: items.filter((i) => i.poster?.source === 'frame').length };
    }
    return libs;
  };
  const before = await count();
  const t0 = Date.now();
  await page.goto(base + '/', { waitUntil: 'networkidle' });
  let after = before;
  while (Date.now() - t0 < 120_000) {
    await sleep(10_000);
    after = await count();
    if (Object.values(after).reduce((a, b) => a + b.frame_posters, 0) >= 60) break;
  }
  await page.goto(`${base}/library/${libIds.Movies}?sort=added`, { waitUntil: 'networkidle' });
  await page.waitForTimeout(1500);
  const s1 = await shot(page, 'frame-posters-movies');
  const labels = await page.locator('[role="grid"] img').count();
  await page.goto(base + '/', { waitUntil: 'networkidle' });
  await page.waitForTimeout(1500);
  const s2 = await shot(page, 'frame-posters-home');
  const total = Object.values(after).reduce((a, b) => a + b.frame_posters, 0);
  const av1 = await (async () => {
    // AV1 titles (libdav1d + -skip_frame nokey gave no frame before the fix).
    const items = await allItems(base, libIds.Movies);
    const withArt = items.filter((i) => i.poster?.source === 'frame').map((i) => i.id);
    let n = 0;
    for (const id of withArt.slice(0, 200)) {
      const d = await api(base, `/api/v1/items/${id}`);
      if (d.assets.some((a) => a.probe?.tracks.some((t) => t.kind === 'video' && t.codec === 'av1'))) n++;
    }
    return n;
  })();
  return { ok: total > 0 && av1 > 0, before, after, av1_titles_with_frame_posters: av1, images_in_grid: labels, waited_ms: Date.now() - t0, shots: [s1, s2] };
});

await step('unavailable-root', async () => {
  // A scratch clone of features is its own library; rename its folder away and back.
  await page.goto(base + '/admin', { waitUntil: 'networkidle' });
  await recordEvents(page);
  await page.getByRole('button', { name: 'Add library' }).click();
  const dlg = page.locator('dialog[open]');
  await dlg.getByLabel('Name').fill('Scratch');
  await dlg.getByLabel('Films', { exact: true }).check();
  await dlg.getByLabel('Folder 1').fill(CLONE);
  await dlg.getByRole('button', { name: 'Add and scan' }).click();
  const libs0 = await waitScansIdle(base, { timeoutMs: 120_000 });
  const lib = libs0.find((l) => l.name === 'Scratch');
  libIds.Scratch = lib.id;
  const scratchItems = await allItems(base, lib.id);
  const victim = scratchItems.find((i) => i.title === 'Faststart') ?? scratchItems[0];
  const away = `${CLONE}.away`;
  const tMove = Date.now();
  renameSync(CLONE, away);
  // Does the watcher notice by itself?
  let byWatcher = false;
  for (let i = 0; i < 40; i++) {
    const L = (await api(base, '/api/v1/libraries')).find((l) => l.id === lib.id);
    if (L.roots.some((r) => !r.available) || L.scan.state === 'unavailable') {
      byWatcher = true;
      break;
    }
    await sleep(250);
  }
  const watcherMs = byWatcher ? Date.now() - tMove : null;
  if (!byWatcher) {
    const card = page.locator('article', { has: page.getByRole('link', { name: 'Scratch', exact: true }) });
    await card.getByRole('button', { name: /Scan now/ }).click();
    await waitScansIdle(base, { timeoutMs: 60_000, ids: [lib.id] }).catch(() => undefined);
    await sleep(500);
  }
  const L1 = (await api(base, '/api/v1/libraries')).find((l) => l.id === lib.id);
  await page.waitForTimeout(800);
  await page.locator('article', { has: page.getByRole('link', { name: 'Scratch', exact: true }) }).scrollIntoViewIfNeeded();
  const cardAway = await page.locator('article', { has: page.getByRole('link', { name: 'Scratch', exact: true }) }).innerText();
  const sseAway = await page.evaluate(() => (window.__sse ?? []).filter((x) => x.e.type === 'source_availability').map((x) => x.e));
  const sAdmin = await shot(page, 'unavailable-admin');
  await page.goto(`${base}/library/${lib.id}`, { waitUntil: 'networkidle' });
  const notice = await page.locator('[role="note"]').innerText().catch(() => null);
  const sLib = await shot(page, 'unavailable-library');
  await gotoItem(page, base, victim.id);
  const offlineText = await page.getByText(/Offline/).first().innerText().catch(() => null);
  const playDisabled = await page.locator('button', { hasText: /^(Resume|Play)/ }).first().isDisabled();
  const sItem = await shot(page, 'unavailable-item');
  const playStatus = await fetch(`${base}/api/v1/play`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'x-flux-request': '1' },
    body: JSON.stringify({
      item_id: victim.id,
      asset_id: null,
      caps: { client: 'e2e', delivery: ['progressive_file', 'hls_fmp4'], progressive_containers: ['mp4'], video: [], audio: [], subtitles: ['web_vtt'], display_hdr: false, needs_demuxed_audio: false, max_bitrate: null },
      audio_track: null,
      subtitle_track: null,
      start_ms: null,
    }),
  }).then(async (r) => ({ status: r.status, body: (await r.text()).slice(0, 200) }));
  const itemsWhileAway = (await allItems(base, lib.id)).length;
  // And back.
  renameSync(away, CLONE);
  const tBack = Date.now();
  let backByWatcher = false;
  for (let i = 0; i < 40; i++) {
    const L = (await api(base, '/api/v1/libraries')).find((l) => l.id === lib.id);
    if (L.roots.every((r) => r.available) && L.scan.state !== 'unavailable') {
      backByWatcher = true;
      break;
    }
    await sleep(250);
  }
  if (!backByWatcher) {
    await page.goto(base + '/admin', { waitUntil: 'networkidle' });
    const card = page.locator('article', { has: page.getByRole('link', { name: 'Scratch', exact: true }) });
    await card.getByRole('button', { name: /Scan now/ }).click();
    await waitScansIdle(base, { timeoutMs: 60_000, ids: [lib.id] }).catch(() => undefined);
  }
  const L2 = (await api(base, '/api/v1/libraries')).find((l) => l.id === lib.id);
  const itemsBack = (await allItems(base, lib.id)).length;
  await gotoItem(page, base, victim.id);
  const playEnabled = !(await page.locator('button', { hasText: /^(Resume|Play)/ }).first().isDisabled());
  const sBack = await shot(page, 'available-again');
  return {
    ok: !L1.roots[0].available && itemsWhileAway === scratchItems.length && playStatus.status === 409 && L2.roots[0].available && itemsBack === scratchItems.length && playEnabled,
    detected_by_watcher: byWatcher,
    watcher_ms: watcherMs,
    back_detected_by_watcher: backByWatcher,
    back_ms: backByWatcher ? Date.now() - tBack : null,
    while_away: { admin_card: cardAway.replace(/\n+/g, ' | '), roots: L1.roots, scan_state: L1.scan.state, last_error: L1.scan.last_error, items: itemsWhileAway, library_notice: notice, item_offline_text: offlineText, play_button_disabled: playDisabled, play_api: playStatus },
    after_back: { roots: L2.roots, scan_state: L2.scan.state, items: itemsBack, play_enabled: playEnabled },
    sse_source_availability_while_away: sseAway,
    shots: [sAdmin, sLib, sItem, sBack],
  };
});

await step('library-grid-12k', async () => {
  // The corpus's biggest grid is 2,998 films (episodes group into 300 shows
  // whatever the library kind), so build a 12,000-film archive from APFS
  // clones of one seed (clonefile: instant, no extra disk) in the scratch dir.
  const dir = join(SCRATCH, 'archive-12k');
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(dir, { recursive: true });
  const A = ['Silent', 'Golden', 'Hidden', 'Broken', 'Electric', 'Midnight', 'Distant', 'Crimson', 'Northern', 'Paper', 'Velvet', 'Iron', 'Glass', 'Wild', 'Quiet', 'Burning', 'Frozen', 'Hollow', 'Last', 'Lost'];
  const B = ['River', 'Harbor', 'Engine', 'Garden', 'Signal', 'Empire', 'Summit', 'Lantern', 'Orchard', 'Station', 'Canal', 'Mirror', 'Desert', 'Frontier', 'Circuit', 'Archive', 'Meridian', 'Republic', 'Tide', 'Harvest'];
  const C = ['', ' Returns', ' Rising', ' of the North', ' in Winter', ' at Dawn', ' Forever', ' Reborn', ' Protocol', ' Diaries', ' Requiem', ' Overture', ' Chronicles', ' Paradox', ' Uprising'];
  const seed = `${MEDIA}/seeds/seed_h264_mp4.mp4`;
  const names = [];
  outer: for (const c of C)
    for (const a of A)
      for (const b of B)
        for (const year of [1951, 1977, 1999, 2013, 2024, 1968, 1986, 2005, 2019, 1993]) {
          if (names.length >= 12_000) break outer;
          names.push(`${a} ${b}${c} (${year}).mp4`);
        }
  const t0 = Date.now();
  // clonefile(2) via ctypes: Node's COPYFILE_FICLONE_FORCE is ENOSYS on macOS, and a
  // plain copy would write 19 GB. A clone shares the seed's blocks; the seed is only read.
  execFileSync(
    'python3',
    [
      '-I',
      '-c',
      'import ctypes,sys,os\nlibc=ctypes.CDLL(None,use_errno=True)\nsrc,d=sys.argv[1],sys.argv[2]\nfor n in sys.stdin.read().split("\\n"):\n  if n and libc.clonefile(src.encode(),os.path.join(d,n).encode(),0)!=0: raise OSError(ctypes.get_errno(),n)',
      seed,
      dir,
    ],
    { input: names.join('\n') },
  );
  const n = names.length;
  const cloneMs = Date.now() - t0;
  const L = await api(base, '/api/v1/libraries', { method: 'POST', body: JSON.stringify({ name: 'Archive', kind: 'movies', roots: [dir] }) });
  libIds.Archive = L.id;
  const libs = await waitScansIdle(base, { ids: [L.id], timeoutMs: 300_000 });
  const lib = libs.find((l) => l.id === L.id);
  await page.goto(`${base}/`, { waitUntil: 'networkidle' });
  const tNav = Date.now();
  await page.getByRole('navigation', { name: 'Main' }).first().getByRole('link', { name: /^Archive/ }).click();
  await page.locator('[role="grid"] a[href^="/item/"]').first().waitFor();
  const firstCellMs = Date.now() - tNav;
  await page.waitForFunction(() => !/loaded/.test(document.querySelector('main header p')?.textContent ?? ''), null, { timeout: 30_000 });
  const allLoadedMs = Date.now() - tNav;
  const header = await page.locator('main header p').first().textContent();
  const perf = await page.evaluate(async () => {
    const frames = [];
    let last = performance.now();
    let run = true;
    const loop = (t) => {
      frames.push(t - last);
      last = t;
      if (run) requestAnimationFrame(loop);
    };
    requestAnimationFrame(loop);
    const longTasks = [];
    const po = new PerformanceObserver((l) => longTasks.push(...l.getEntries().map((e) => Math.round(e.duration))));
    try {
      po.observe({ type: 'longtask', buffered: false });
    } catch {
      /* unsupported */
    }
    const h = document.documentElement.scrollHeight;
    for (let y = 0; y < h; y += 2400) {
      window.scrollTo(0, y);
      await new Promise((r) => setTimeout(r, 16));
    }
    await new Promise((r) => setTimeout(r, 300));
    run = false;
    po.disconnect();
    const s = [...frames].sort((a, b) => a - b);
    const q = (p) => Math.round(s[Math.min(s.length - 1, Math.floor(p * s.length))] * 10) / 10;
    return {
      frames: frames.length,
      p50: q(0.5),
      p95: q(0.95),
      p99: q(0.99),
      janky_over_25ms: frames.filter((f) => f > 25).length,
      long_tasks: longTasks,
      cards_in_dom: document.querySelectorAll('[role="grid"] a[href^="/item/"]').length,
      dom_nodes: document.getElementsByTagName('*').length,
      heap_mb: performance.memory ? Math.round(performance.memory.usedJSHeapSize / 1048576) : null,
      scroll_height: h,
    };
  });
  // A–Z rail and the instant filter.
  await page.evaluate(() => window.scrollTo(0, 0));
  const tJump = Date.now();
  await page.getByRole('button', { name: 'Jump to M' }).click();
  await page.waitForFunction(() => [...document.querySelectorAll('[role="grid"] a[href^="/item/"]')].some((a) => /^Midnight/.test(a.getAttribute('aria-label') ?? '')), null, { timeout: 5000 });
  const jumpMs = Date.now() - tJump;
  const sJump = await shot(page, 'library-12k-jump-m');
  await page.evaluate(() => window.scrollTo(0, 0));
  const tFilter = Date.now();
  await page.getByPlaceholder(/Filter/).fill('velvet harbor');
  await page.getByRole('status').filter({ hasText: /of/ }).first().waitFor();
  const filterMs = Date.now() - tFilter;
  const filterStatus = await page.getByRole('status').filter({ hasText: /of/ }).first().textContent();
  const sFilter = await shot(page, 'library-12k-filter');
  // Again from Home, now that the scan's aftermath (art jobs queued, WAL) has settled.
  await page.goto(`${base}/`, { waitUntil: 'networkidle' });
  await page.waitForTimeout(1000);
  const tNav2 = Date.now();
  await page.getByRole('navigation', { name: 'Main' }).first().getByRole('link', { name: /^Archive/ }).click();
  await page.locator('[role="grid"] a[href^="/item/"]').first().waitFor();
  const firstCellAgainMs = Date.now() - tNav2;
  const pageApi = await (async () => {
    const xs = [];
    for (let i = 0; i < 5; i++) {
      const t = performance.now();
      await api(base, `/api/v1/libraries/${L.id}/items?limit=120`);
      xs.push(Math.round((performance.now() - t) * 10) / 10);
    }
    return xs;
  })();
  return {
    first_cell_again_ms: firstCellAgainMs,
    first_page_api_ms: pageApi,
    ok: lib.item_count >= 10_000 && perf.p95 < 25,
    files_cloned: n,
    clone_ms: cloneMs,
    scan_ms: lib.scan.finished_at - lib.scan.started_at,
    items: lib.item_count,
    header,
    first_cell_ms: firstCellMs,
    all_pages_loaded_ms: allLoadedMs,
    scroll: perf,
    az_jump_ms: jumpMs,
    filter_ms: filterMs,
    filter_status: filterStatus,
    shots: [sJump, sFilter],
  };
});

await step('phone-and-light-screens', async () => {
  const shots = [];
  const phone = await browser.newContext({ viewport: { width: 390, height: 844 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true, colorScheme: 'dark' });
  const pp = await phone.newPage();
  watchPage(pp, errors);
  await pp.goto(base + '/', { waitUntil: 'networkidle' });
  shots.push(await shot(pp, 'phone-home'));
  await pp.goto(`${base}/library/${libIds.Movies}`, { waitUntil: 'networkidle' });
  await pp.waitForTimeout(800);
  shots.push(await shot(pp, 'phone-library'));
  await gotoItem(pp, base, features.get('h264_multi_audio_subs.mkv').item);
  shots.push(await shot(pp, 'phone-item', { fullPage: true }));
  await gotoItem(pp, base, ctxState.show);
  shots.push(await shot(pp, 'phone-show'));
  await phone.close();
  const light = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: 'light' });
  const lp = await light.newPage();
  watchPage(lp, errors);
  await lp.goto(base + '/', { waitUntil: 'networkidle' });
  shots.push(await shot(lp, 'light-home'));
  await gotoItem(lp, base, features.get('hevc_main10_hdr10_eac3.mkv').item);
  shots.push(await shot(lp, 'light-item'));
  await lp.goto(base + '/admin', { waitUntil: 'networkidle' });
  shots.push(await shot(lp, 'light-admin'));
  await light.close();
  return { shots };
});

rssSamples.push({ at: 'end of walk', mb: server.rss() });
await browser.close();
await sleep(1000);
rssSamples.push({ at: 'idle after browser closed', mb: server.rss() });
const sampled = rssSampler.stop();
const stopped = await server.stop();

const failed = steps.filter((s) => !s.ok).map((s) => s.name);
const out = {
  name: 'flux-e2e-product',
  generated_at: new Date().toISOString(),
  machine: machine(),
  load1_end: load1(),
  quiet_wait_ms: quietWaitMs,
  wall_ms: Date.now() - t00,
  server: {
    base,
    ready_ms: Math.round(server.readyMs),
    rss: rssSamples,
    peak_rss_mb_sampled: sampled.peak_mb,
    rss_sampling: `ps every ${sampled.every_ms} ms, ${sampled.samples} samples`,
    peak_rss_mb_with_children: stopped.peakMb,
    peak_rss_note: 'with_children = /usr/bin/time -l, i.e. the largest of flux-server and every ffmpeg/ffprobe it ran',
    shutdown_ms: stopped.shutdownMs,
    log: server.logPath,
  },
  summary: { steps: steps.length, ok: steps.length - failed.length, failed },
  steps,
};
writeResult('product', out);
console.log(`\n[e2e] ${steps.length - failed.length}/${steps.length} steps ok${failed.length ? `; failed: ${failed.join(', ')}` : ''}`);
if (existsSync(`${CLONE}.away`)) renameSync(`${CLONE}.away`, CLONE);
process.exit(failed.length ? 1 : 0);
