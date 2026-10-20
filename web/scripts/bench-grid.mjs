#!/usr/bin/env node
// Library grid smoothness at 10k items, plus cold Home load, in real Chrome.
//
//   pnpm build && pnpm preview --port 4199 &
//   BASE=http://localhost:4199 node scripts/bench-grid.mjs
//
// Scrolls the 10,000-title "Film Archive" with wheel input (fast fling, then a
// slow read) and keyboard paging, recording every animation frame and every
// long task / long animation frame. Writes $RESULTS, default
// $FLUX_WORK/bench/results/flux-web-grid-scroll.json.
import { execSync } from 'node:child_process';
import { mkdirSync, writeFileSync } from 'node:fs';
import { chromium } from 'playwright-core';
import { resultFile } from './work.mjs';

const BASE = process.env.BASE ?? 'http://localhost:4199';
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
  const page = await ctx.newPage();

  // Cold load of Home: LCP and time until the first poster row is in the DOM.
  const t0 = Date.now();
  await page.goto(`${BASE}/?mock_latency=0`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('[data-nav-row] a', { timeout: 30_000 });
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

  // 10k library, fully loaded.
  await page.goto(`${BASE}/library/3`, { waitUntil: 'networkidle' });
  await page.waitForFunction(() => !/loaded \d/.test(document.querySelector('main header .edge')?.textContent ?? ''), null, { timeout: 60_000 });
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
  // Keyboard paging through the grid (TV remote path).
  await page.locator('[role="grid"] a[tabindex="0"]').first().focus();
  for (let i = 0; i < 25; i++) {
    await page.keyboard.press('PageDown');
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
      rowcount: Number(document.querySelector('[role="grid"]')?.getAttribute('aria-rowcount') ?? 0),
      heapMB: performance.memory ? Math.round(performance.memory.usedJSHeapSize / 1e6) : null,
      focusedIndex: Number(document.activeElement?.getAttribute('data-index') ?? -1),
    };
  });
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
    grid_rows: r.rowcount,
    js_heap_mb: r.heapMB,
    keyboard_focus_index_after_25_pagedowns: r.focusedIndex,
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
