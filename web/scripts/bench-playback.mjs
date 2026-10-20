#!/usr/bin/env node
// Client-observed time-to-first-frame and seek latency in real Chrome
// (research/03 §13: "Web: Playwright click → first requestVideoFrameCallback";
// seek: `seeking` → first rVFC after `seeked`).
//
//   pnpm build && pnpm preview --port 4199 &
//   BASE=http://localhost:4199 RUNS=10 SEEKS=8 node scripts/bench-playback.mjs
//
// Each run: fresh browser context → item page → click Play → TTFF (measured
// twice: by the app's own PlaybackMetrics and by an independent rVFC in the
// harness) → N random seeks. Against the mock backend the bytes come from the
// local feature clips via the Vite preview server, so this measures the web
// client's pipeline (code split, POST /play, prefetch, hls.js/MSE, decode), not
// flux-server. Point BASE at a flux-server serving web/dist to measure it.
// Writes $RESULTS, default $FLUX_WORK/bench/results/flux-web-playback.json.
import { execSync } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { chromium } from 'playwright-core';
import { resultFile } from './work.mjs';

const BASE = process.env.BASE ?? 'http://localhost:4199';
const OUT = process.env.RESULTS ?? resultFile('flux-web-playback.json');
const RUNS = Number(process.env.RUNS ?? 10);
const SEEKS = Number(process.env.SEEKS ?? 8);

const fx = JSON.parse(readFileSync(new URL('../src/mock/fixtures.json', import.meta.url), 'utf8'));
const movie = (t) => 10_000 + fx.movies.findIndex((m) => m[0] === t);

// Cases cover each delivery path the mock can serve with real bytes.
const CASES = (process.env.CASES ?? 'direct-mp4,direct-mkv,hls-hevc-4k,hls-h264-2h').split(',');
const CASE_ITEMS = {
  'direct-mp4': { item: movie('Mad Max: Fury Road'), note: 'progressive MP4 (h264_aac_faststart.mp4, faststart)' },
  'direct-mkv': { item: movie('Spirited Away'), note: 'progressive MKV in Chrome (h264_multi_audio_subs.mkv)' },
  'hls-hevc-4k': { item: movie('Blade Runner 2049'), note: 'HLS fMP4 via hls.js, 4K HEVC Main10 HDR10 (audio conversion plan)' },
  'hls-h264-2h': { item: movie('Dune: Part Two'), note: 'HLS fMP4 via hls.js, 2 h H.264 360p (transcode plan; stand-in bytes)' },
};

const pct = (xs, p) => {
  if (!xs.length) return null;
  const s = [...xs].sort((a, b) => a - b);
  return Math.round(s[Math.min(s.length - 1, Math.floor((p / 100) * s.length))]);
};

const browser = await chromium.launch({ channel: 'chrome', headless: true, args: ['--autoplay-policy=no-user-gesture-required'] });
const results = {};
for (const name of CASES) {
  const c = CASE_ITEMS[name];
  if (!c) continue;
  const ttffApp = [];
  const ttffHarness = [];
  const seeksApp = [];
  const seeksHarness = [];
  let plan = null;
  for (let run = 0; run < RUNS; run++) {
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 } });
    const page = await ctx.newPage();
    await page.goto(`${BASE}/item/${c.item}?mock_latency=0`, { waitUntil: 'networkidle' });
    await page.waitForTimeout(300);
    const button = page.locator('button', { hasText: /^(Resume|Play)/ }).first();
    await button.hover(); // Play intent: preloads the player route + hls.js, like a real pointer would
    await page.waitForTimeout(150);
    // Harness clock + independent first-frame probe on whatever <video> appears.
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
    await page.waitForFunction(() => window.__fluxPlayback?.metrics().ttffMs != null && window.__h.first != null, null, { timeout: 30_000 });
    const first = await page.evaluate(() => ({ app: window.__fluxPlayback.metrics().ttffMs, harness: window.__h.first, plan: window.__fluxPlayback.plan }));
    ttffApp.push(first.app);
    ttffHarness.push(Math.round(first.harness));
    plan = first.plan;
    await page.waitForTimeout(1200);

    for (let s = 0; s < SEEKS; s++) {
      const r = await page.evaluate(async (k) => {
        const v = document.querySelector('video');
        const before = window.__fluxPlayback.metrics().seekSamples.length;
        const dur = Number.isFinite(v.duration) ? v.duration : 60;
        // Deterministic spread of targets across the whole file.
        const target = Math.max(1, Math.min(dur - 3, ((k * 0.37 + 0.11) % 1) * dur));
        const t0 = performance.now();
        const harness = new Promise((resolve) => {
          v.addEventListener('seeked', () => v.requestVideoFrameCallback((now, meta) => resolve((meta.presentationTime ?? now) - t0)), { once: true });
        });
        v.currentTime = target;
        const h = await Promise.race([harness, new Promise((r) => setTimeout(() => r(null), 10_000))]);
        const start = performance.now();
        while (window.__fluxPlayback.metrics().seekSamples.length === before && performance.now() - start < 2000) await new Promise((r) => setTimeout(r, 10));
        const app = window.__fluxPlayback.metrics().lastSeekMs;
        return { harness: h === null ? null : Math.round(h), app };
      }, s + run * SEEKS);
      if (r.harness !== null) seeksHarness.push(r.harness);
      if (r.app !== null) seeksApp.push(r.app);
      await page.waitForTimeout(400);
    }
    await ctx.close();
  }
  results[name] = {
    note: c.note,
    plan_delivery: plan?.delivery,
    runs: RUNS,
    ttff_ms: { app_p50: pct(ttffApp, 50), app_p95: pct(ttffApp, 95), harness_p50: pct(ttffHarness, 50), harness_p95: pct(ttffHarness, 95), samples_app: ttffApp, samples_harness: ttffHarness },
    seek_ms: { app_p50: pct(seeksApp, 50), app_p95: pct(seeksApp, 95), harness_p50: pct(seeksHarness, 50), harness_p95: pct(seeksHarness, 95), n: seeksHarness.length },
  };
  console.log(name, JSON.stringify({ ttff: results[name].ttff_ms.app_p50, ttff_p95: results[name].ttff_ms.app_p95, seek: results[name].seek_ms.app_p50, seek_p95: results[name].seek_ms.app_p95 }));
}
await browser.close();

const out = {
  name: 'flux-web-playback',
  generated_at: new Date().toISOString(),
  base: BASE,
  browser: 'Google Chrome (headless, playwright-core channel=chrome)',
  backend: BASE.includes('4199') || BASE.includes('5199') ? 'mock (MSW) + local feature clips served by Vite preview' : 'flux-server',
  measured_from: 'Play click (TTFF) / currentTime assignment (seek) → first requestVideoFrameCallback presentationTime',
  load_average: execSync('uptime').toString().trim(),
  jellyfin_baseline: 'Jellyfin 12.2 on this machine: time-to-first-segment 0.19–0.91 s, seeks 0.32–0.84 s (bench/results/jellyfin-playback-*.json; server-side, not first frame)',
  cases: results,
};
mkdirSync(OUT.replace(/\/[^/]+$/, ''), { recursive: true });
writeFileSync(OUT, JSON.stringify(out, null, 2));
console.log(`wrote ${OUT}`);
