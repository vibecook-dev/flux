#!/usr/bin/env node
// Audio switch latency in real Chrome: a new session (restart) vs. switching
// HLS renditions inside the session (hls.js setAudioOption).
//
//   ONLY=multi pnpm mock-media      # once: the demuxed multi-audio variant
//   pnpm build && nice -n 19 pnpm preview --host 127.0.0.1 --port 4199 &
//   BASE=http://127.0.0.1:4199 SWITCHES=10 nice -n 19 node scripts/bench-audio-switch.mjs
//
// Spirited Away (h264_multi_audio_subs.mkv) starts as direct play (English
// AAC). Picking Japanese needs a new session (direct play carries only the
// file's default track); after that, English ↔ Japanese are renditions of
// the same HLS session. Timings are the app's own (window.__fluxPlayback):
// restart = click → first frame of the new session; in-session = click →
// hls.js AUDIO_TRACK_SWITCHED (the new track's audio is buffered). The
// harness also checks the session id stays the same and playback keeps going.
// Against the mock the bytes come from the Vite preview server, so this
// measures the client pipeline, not flux-server's audio transcode.
// Writes $RESULTS, default $FLUX_WORK/bench/results/flux-web-audio-switch.json.
import { execSync } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { chromium } from 'playwright-core';
import { resultFile } from './work.mjs';

const BASE = process.env.BASE ?? 'http://127.0.0.1:4199';
const OUT = process.env.RESULTS ?? resultFile('flux-web-audio-switch.json');
const RUNS = Number(process.env.RUNS ?? 3);
const SWITCHES = Number(process.env.SWITCHES ?? 10);

const fx = JSON.parse(readFileSync(new URL('../src/mock/fixtures.json', import.meta.url), 'utf8'));
const ITEM = Number(process.env.ITEM ?? 10_000 + fx.movies.findIndex((m) => m[0] === 'Spirited Away'));

const pct = (xs, p) => {
  if (!xs.length) return null;
  const s = [...xs].sort((a, b) => a - b);
  return Math.round(s[Math.min(s.length - 1, Math.floor((p / 100) * s.length))]);
};

async function pickAudio(page, label) {
  await page.mouse.move(600, 400);
  if (!(await page.locator('[role="menu"][aria-label="Audio"]').isVisible().catch(() => false))) await page.getByRole('button', { name: 'Audio' }).click();
  await page.locator('[role="menu"][aria-label="Audio"] [role="menuitemradio"]', { hasText: label }).first().click();
}

const switches = () => window.__fluxPlayback.audio().switches.length;

const browser = await chromium.launch({ channel: 'chrome', headless: true, args: ['--autoplay-policy=no-user-gesture-required'] });
const restart = [];
const inSession = [];
const checks = [];
for (let run = 0; run < RUNS; run++) {
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 } });
  const page = await ctx.newPage();
  // Independent evidence from the network: POST /play count, and which audio rendition's segments load.
  const net = { plays: 0, audio: [] };
  page.on('request', (r) => {
    if (r.method() === 'POST' && r.url().endsWith('/api/v1/play')) net.plays++;
    const m = /\/a\/([^/]+)\/(?:\d+\.m4s|init\.mp4)$/.exec(new URL(r.url()).pathname);
    if (m) net.audio.push({ at: Date.now(), rendition: m[1] });
  });
  await page.goto(`${BASE}/item/${ITEM}?mock_latency=0`, { waitUntil: 'networkidle' });
  const button = page.locator('button', { hasText: /^(Resume|Play)/ }).first();
  await button.hover();
  await page.waitForTimeout(150);
  await button.click();
  await page.waitForFunction(() => window.__fluxPlayback?.metrics().ttffMs != null, null, { timeout: 30_000 });
  const direct = await page.evaluate(() => window.__fluxPlayback.plan.delivery);

  // 1. Direct play → Japanese: a new session.
  const s0 = await page.evaluate(() => window.__fluxPlayback.sessionId);
  await pickAudio(page, /Japanese/);
  await page.waitForFunction((s) => window.__fluxPlayback.sessionId !== s && window.__fluxPlayback.audio().switches.length > 0, s0, { timeout: 30_000 });
  const first = await page.evaluate(() => ({ s: window.__fluxPlayback.sessionId, sw: window.__fluxPlayback.audio().switches.at(-1), delivery: window.__fluxPlayback.plan.delivery }));
  restart.push(first.sw.ms);
  await page.waitForTimeout(1500);

  // 2. English ↔ Japanese inside that session.
  for (let k = 0; k < SWITCHES; k++) {
    const before = await page.evaluate(switches);
    const plays = net.plays;
    const clickedAt = Date.now();
    await pickAudio(page, k % 2 === 0 ? /English/ : /Japanese/);
    await page.waitForFunction((n) => window.__fluxPlayback.audio().switches.length > n, before, { timeout: 30_000 });
    const r = await page.evaluate(async () => {
      const v = document.querySelector('video');
      const a = v.currentTime;
      await new Promise((res) => setTimeout(res, 1200));
      return { sw: window.__fluxPlayback.audio().switches.at(-1), session: window.__fluxPlayback.sessionId, advanced: v.currentTime - a, rendition: window.__fluxPlayback.audio().rendition };
    });
    if (r.sw.mode === 'in-session') inSession.push(r.sw.ms);
    const fetched = [...new Set(net.audio.filter((a) => a.at >= clickedAt).map((a) => a.rendition))];
    checks.push({
      run,
      k,
      mode: r.sw.mode,
      ms: r.sw.ms,
      same_session: r.session === first.s,
      new_play_requests: net.plays - plays,
      audio_segments_fetched_for: fetched,
      advanced_s: Math.round(r.advanced * 100) / 100,
      rendition: r.rendition,
    });
  }
  checks.push({ run, start_delivery: direct, after_restart_delivery: first.delivery });
  await ctx.close();
}
await browser.close();

const switchesChecked = checks.filter((c) => c.mode);
const out = {
  name: 'flux-web-audio-switch',
  generated_at: new Date().toISOString(),
  base: BASE,
  browser: 'Google Chrome (headless, playwright-core channel=chrome)',
  backend: 'mock (MSW) + demuxed renditions of h264_multi_audio_subs.mkv served by Vite preview',
  measured_from: 'menu click → first frame of the new session (restart) / hls.js AUDIO_TRACK_SWITCHED (in session)',
  load_average: execSync('uptime').toString().trim(),
  restart_ms: { p50: pct(restart, 50), p95: pct(restart, 95), n: restart.length, samples: restart },
  in_session_ms: { p50: pct(inSession, 50), p95: pct(inSession, 95), n: inSession.length, samples: inSession },
  all_in_session: switchesChecked.every((c) => c.mode === 'in-session' && c.same_session && c.new_play_requests === 0),
  target_rendition_loaded: switchesChecked.every((c) => c.audio_segments_fetched_for.includes(c.rendition)),
  playback_kept_going: switchesChecked.every((c) => c.advanced_s > 0.5),
  checks,
};
mkdirSync(OUT.replace(/\/[^/]+$/, ''), { recursive: true });
writeFileSync(OUT, JSON.stringify(out, null, 2));
console.log(
  JSON.stringify({ restart_ms: out.restart_ms, in_session_ms: out.in_session_ms, all_in_session: out.all_in_session, target_rendition_loaded: out.target_rendition_loaded, playback_kept_going: out.playback_kept_going }),
);
console.log(`wrote ${OUT}`);
