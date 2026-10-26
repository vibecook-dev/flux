#!/usr/bin/env node
// The Wall (the default lens) at 10k titles, plus cold Home load, in real Chrome.
//
//   pnpm build && pnpm preview --port 4199 &
//   BASE=http://localhost:4199 node scripts/bench-grid.mjs
//
// Scrolls the 10,000-title "Film Archive" with wheel input (fast fling, then a
// slow read) and the arrow keys (the TV-remote path), recording every animation
// frame and every long task / long animation frame. Writes $RESULTS, default
// $FLUX_WORK/bench/results/flux-web-grid-scroll.json.
import { execSync } from 'node:child_process';
import { mkdirSync, writeFileSync } from 'node:fs';
import { chromium } from 'playwright-core';
import { resultFile } from './work.mjs';

const BASE = process.env.BASE ?? 'http://localhost:4199';
// Titles on the wall.
const TILE = '[role="grid"] a[href^="/item/"]';
const OUT = process.env.RESULTS ?? resultFile('flux-web-grid-scroll.json');
const RUNS = Number(process.env.RUNS ?? 3);

const pct = (xs, p) => {
  if (!xs.length) return null;
  const s = [...xs].sort((a, b) => a - b);
  return Math.round(s[Math.min(s.length - 1, Math.floor((p / 100) * s.length))] * 10) / 10;
};
const median = (xs) => pct(xs, 50);

const browser = await chromium.launch({ channel: 'chrome', headless: true });
const runs = [];
for (let run = 0; run < RUNS; run++) {
  const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: 'dark' });
  // The first-run hint counts as seen (it floats over the wall).
  await ctx.addInitScript(() => localStorage.setItem('flux.coach.done', 'true'));
  const page = await ctx.newPage();

  // Cold load of Home (the "All" space): LCP and time until the first posters are on the wall.
  const t0 = Date.now();
  await page.goto(`${BASE}/?mock_latency=0`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector(TILE, { timeout: 30_000 });
  const homeReadyMs = Date.now() - t0;
  const lcp = await page.evaluate(
    () =>
      new Promise((resolve) => {
        new PerformanceObserver((list) => {
          const e = list.getEntries().at(-1);
          resolve(e ? e.startTime : null);
        }).observe({ type: 'largest-contentful-paint', buffered: true });
        setTimeout(() => resolve(null), 1500);
      }),
  );

  // 10k library, fully loaded: its last items page has arrived. (Home's "All"
  // loads that library too: let it finish, so its pages can't be mistaken for these.)
  await page.waitForLoadState('networkidle');
  const lastPage = page.waitForResponse(async (r) => new URL(r.url()).pathname === '/api/v1/libraries/3/items' && (await r.json().catch(() => ({}))).next === null, { timeout: 60_000 });
  await page.goto(`${BASE}/library/3`, { waitUntil: 'networkidle' });
  await lastPage;
  await page.waitForSelector(TILE);
  await page.waitForTimeout(500);

  await page.evaluate(() => {
    const w = window;
    w.__frames = [];
    w.__long = [];
    w.__loaf = [];
    const tick = (t) => {
      w.__frames.push(t);
      if (w.__frames.length < 100000) requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
    new PerformanceObserver((l) => l.getEntries().forEach((e) => w.__long.push(e.duration))).observe({ type: 'longtask' });
    try {
      new PerformanceObserver((l) => l.getEntries().forEach((e) => w.__loaf.push(e.duration))).observe({ type: 'long-animation-frame' });
    } catch {
      /* LoAF not supported */
    }
  });

  await page.mouse.move(700, 500);
  const scrollStart = Date.now();
  // Fast fling: 120 wheel events of 600 px (~72,000 px ≈ 160 rows).
  for (let i = 0; i < 120; i++) {
    await page.mouse.wheel(0, 600);
    await page.waitForTimeout(16);
  }
  // Slow read back up: 120 events of 80 px.
  for (let i = 0; i < 120; i++) {
    await page.mouse.wheel(0, -80);
    await page.waitForTimeout(16);
  }
  // The arrow keys through the wall (TV remote path): the first press focuses the
  // first title on screen, each later one moves a row and the wall follows.
  for (let i = 0; i < 25; i++) {
    await page.keyboard.press('ArrowDown');
    await page.waitForTimeout(40);
  }
  const scrollMs = Date.now() - scrollStart;
  await page.waitForTimeout(300);

  const r = await page.evaluate(() => {
    const w = window;
    const f = w.__frames;
    const deltas = f.slice(1).map((t, i) => t - f[i]);
    return {
      deltas,
      long: w.__long,
      loaf: w.__loaf,
      gridCells: document.querySelectorAll('[role="gridcell"]').length,
      domNodes: document.getElementsByTagName('*').length,
      // The wall's full (virtual) height: every title has its place, few are drawn.
      wallHeight: document.querySelector('[role="grid"]')?.offsetHeight ?? 0,
      heapMB: performance.memory ? Math.round(performance.memory.usedJSHeapSize / 1e6) : null,
      focusedId: document.activeElement?.closest('[role="grid"]') ? document.activeElement.getAttribute('data-id') : null,
    };
  });
  // Where keyboard focus landed, as a position in the library (title order).
  const focusedIndex = await page.evaluate(async (id) => {
    if (id === null) return -1;
    let cursor = null;
    let at = 0;
    do {
      const q = new URLSearchParams({ limit: '1000', sort: 'title' });
      if (cursor) q.set('cursor', cursor);
      const p = await (await fetch(`/api/v1/libraries/3/items?${q}`, { headers: { 'x-flux-request': '1' } })).json();
      const k = p.items.findIndex((it) => String(it.id) === id);
      if (k >= 0) return at + k;
      at += p.items.length;
      cursor = p.next;
    } while (cursor);
    return -1;
  }, r.focusedId);
  const frameBudget = 1000 / 60;
  runs.push({
    home_ready_ms: homeReadyMs,
    home_lcp_ms: lcp === null ? null : Math.round(lcp),
    scroll_ms: scrollMs,
    frames: r.deltas.length,
    frame_ms_p50: median(r.deltas),
    frame_ms_p95: pct(r.deltas, 95),
    frame_ms_p99: pct(r.deltas, 99),
    frame_ms_max: Math.round(Math.max(...r.deltas) * 10) / 10,
    janky_frames: r.deltas.filter((d) => d > frameBudget * 1.5).length,
    long_tasks: r.long.length,
    long_task_ms_total: Math.round(r.long.reduce((a, b) => a + b, 0)),
    long_animation_frames: r.loaf.length,
    grid_cells_in_dom: r.gridCells,
    dom_nodes: r.domNodes,
    wall_height_px: r.wallHeight,
    js_heap_mb: r.heapMB,
    keyboard_focus_index_after_25_arrow_downs: focusedIndex,
  });
  console.log(JSON.stringify(runs.at(-1)));
  await ctx.close();
}
await browser.close();

const summary = {};
for (const k of Object.keys(runs[0])) {
  const vals = runs.map((r) => r[k]).filter((v) => typeof v === 'number');
  summary[k] = vals.length ? median(vals) : null;
}
const result = {
  name: 'flux-web-grid-scroll',
  generated_at: new Date().toISOString(),
  base: BASE,
  browser: 'Google Chrome (headless, playwright-core channel=chrome)',
  library_items: 10_000,
  backend: 'mock (MSW in-browser); poster SVGs generated per request',
  load_average: execSync('uptime').toString().trim(),
  runs: runs.length,
  median: summary,
  all: runs,
};
mkdirSync(OUT.replace(/\/[^/]+$/, ''), { recursive: true });
writeFileSync(OUT, JSON.stringify(result, null, 2));
console.log(`\nmedian of ${runs.length}: ${JSON.stringify(summary)}\nwrote ${OUT}`);
