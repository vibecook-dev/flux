#!/usr/bin/env node
// flux-hls browser validation: plays every corpus file through hls.js (MSE)
// in the system Google Chrome (and Firefox via WebDriver BiDi when present)
// against `hls-serve`, and records what a real player sees.
//
//   node crates/flux-hls/browser/run.mjs --browser chrome [--files a.mkv,b.mp4]
//        [--presets chrome] [--serve-bin PATH] [--port 18090] [--out FILE]
//
// Nothing is installed: playwright-core and hls.js are taken from the web
// client's node_modules (FLUX_WEB_NODE_MODULES overrides; see env.mjs).
// Per case (file × preset): TTFF, play past segment 0, 5 seeks (one backwards,
// one mid-segment, long files far apart), every audio rendition switched to
// and back, the first WebVTT track's cues at their source times, hls.js ERROR
// events, video.error, MSE exceptions, dropped/decoded frames, and A/V drift
// (requestVideoFrameCallback mediaTime vs the playback clock).
//
// Media: $FLUX_TEST_MEDIA/{features,playback-research} unless --dirs.
// Results: $FLUX_BENCH_RESULTS/flux-hls-browser-<browser>-<ts>.json

import { spawn, execFileSync } from "node:child_process";
import { createServer } from "node:http";
import { readFileSync, writeFileSync, mkdirSync, readdirSync, existsSync } from "node:fs";
import { createRequire } from "node:module";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { MEDIA, NODE_MODULES as NM, TARGET, TMP, resultsPath } from "./env.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const HLS_JS = path.join(NM, "hls.js/dist/hls.js");
const require = createRequire(path.join(NM, "noop.js"));
const pw = require("playwright-core");

// ---------------------------------------------------------------- args
const args = Object.fromEntries(
  process.argv.slice(2).reduce((acc, a, i, all) => {
    if (a.startsWith("--")) acc.push([a.slice(2), all[i + 1] && !all[i + 1].startsWith("--") ? all[i + 1] : "1"]);
    return acc;
  }, []),
);
const browserName = args.browser || "chrome";
const defaultPreset = { chrome: "chrome", firefox: "firefox", safari: "safari" }[browserName] || "chrome";
const presetList = (args.presets || defaultPreset).split(",");
const port = Number(args.port || 18090);
const pagePort = port + 1;
const serveBin = args["serve-bin"] || path.join(TARGET, "release/hls-serve");
if (!args.dirs && !MEDIA) {
  console.error("skipping: no media (pass --dirs or set FLUX_TEST_MEDIA)");
  process.exit(0);
}
const dirs = (args.dirs || `${path.join(MEDIA, "features")},${path.join(MEDIA, "playback-research")}`).split(",");
const ts = new Date().toISOString().replace(/[-:]/g, "").replace(/\..*/, "").replace("T", "-");
const out = resultsPath(`flux-hls-browser-${browserName}-${ts}.json`, args.out);
const tmp = path.join(TMP, `flux-hls-browser-${browserName}`);
const ffprobe = process.env.FLUX_FFPROBE || process.env.FFPROBE || "ffprobe";
const MEDIA_EXT = /\.(mkv|webm|mp4|m4v|mov|ts|m2ts|avi)$/i;

let files = [];
for (const d of dirs) {
  if (!existsSync(d)) continue;
  for (const f of readdirSync(d).sort()) if (MEDIA_EXT.test(f)) files.push(path.join(d, f));
}
if (args.files) {
  const want = args.files.split(",");
  files = want.map((w) => files.find((f) => path.basename(f) === w) || w).filter((f) => existsSync(f));
}

// ---------------------------------------------------------------- servers
function startServe() {
  const roots = [...new Set(files.map((f) => (dirs.some((d) => f.startsWith(d + "/")) ? path.dirname(f) : f)))];
  const child = spawn("nice", ["-n", "19", serveBin, ...roots, "--port", String(port), "--preset", presetList[0]], {
    stdio: ["ignore", "inherit", "inherit"],
    env: { ...process.env, RUST_LOG: process.env.RUST_LOG || "warn" },
  });
  return child;
}

async function waitHttp(url, ms) {
  const t0 = Date.now();
  while (Date.now() - t0 < ms) {
    try {
      const r = await fetch(url);
      if (r.ok) return true;
    } catch (_) {}
    await new Promise((r) => setTimeout(r, 100));
  }
  return false;
}

function startPageServer() {
  const page = readFileSync(path.join(here, "player.html"));
  const hlsjs = readFileSync(HLS_JS);
  const srv = createServer((req, res) => {
    if (req.url === "/" || req.url.startsWith("/player.html")) {
      res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
      res.end(page);
    } else if (req.url === "/hls.js") {
      res.writeHead(200, { "content-type": "text/javascript" });
      res.end(hlsjs);
    } else if (req.url === "/favicon.ico") {
      res.writeHead(204);
      res.end();
    } else {
      res.writeHead(404);
      res.end();
    }
  });
  return new Promise((resolve) => srv.listen(pagePort, "127.0.0.1", () => resolve(srv)));
}

// ---------------------------------------------------------------- browsers
async function launch() {
  mkdirSync(tmp, { recursive: true });
  const profile = path.join(tmp, "profile");
  if (browserName === "chrome") {
    return pw.chromium.launchPersistentContext(profile, {
      channel: "chrome",
      headless: args.headed ? false : true,
      args: ["--mute-audio", "--autoplay-policy=no-user-gesture-required", "--disable-background-media-suspend"],
      viewport: { width: 800, height: 600 },
    });
  }
  if (browserName === "firefox") {
    // The "moz-firefox" channel drives the stock Firefox over WebDriver BiDi
    // (no patched Playwright build needed).
    return pw.firefox.launchPersistentContext(profile, {
      channel: "moz-firefox",
      executablePath: "/Applications/Firefox.app/Contents/MacOS/firefox",
      headless: args.headed ? false : true,
      firefoxUserPrefs: {
        "media.autoplay.default": 0,
        "media.autoplay.blocking_policy": 0,
        "media.volume_scale": "0.0",
      },
      viewport: { width: 800, height: 600 },
    });
  }
  throw new Error(`unknown browser ${browserName}`);
}

// The flux web client's hls.js settings (web/src/player/engine.ts), so the
// check exercises what users run.
const WEB_CLIENT_HLS_CONFIG = {
  testBandwidth: false,
  startFragPrefetch: true,
  progressive: true,
  enableWorker: true,
  lowLatencyMode: false,
  maxBufferLength: 30,
  maxMaxBufferLength: 60,
  backBufferLength: 30,
};

// ---------------------------------------------------------------- cases
function seeksFor(dur, seg0) {
  if (dur > 3000) return [600, 3600, 5100, 30, 1234.5];
  const lo = Math.min(seg0 + 1, dur * 0.3);
  const c = (x) => +Math.max(lo, Math.min(dur - 2, x)).toFixed(3);
  if (dur < 20) return [c(dur * 0.6), c(dur * 0.25), c(dur * 0.8), c(dur * 0.45), c(dur * 0.1)];
  // forward, backwards, far forward, mid-segment (not on a keyframe), back near the start
  return [c(dur * 0.5), c(dur * 0.2), c(dur * 0.8), c(dur * 0.5 + 3.3), c(seg0 + 1.7)];
}

function firstCue(file, streamIndex) {
  try {
    const out = execFileSync(ffprobe, ["-v", "error", "-select_streams", String(streamIndex), "-show_entries",
      "packet=pts_time", "-of", "csv=p=0", "-read_intervals", "%+#1", file], { encoding: "utf8" });
    const t = parseFloat(out.trim().split("\n")[0]);
    return Number.isFinite(t) ? t : null;
  } catch (_) {
    return null;
  }
}

async function runCase(ctx, file, preset) {
  const name = path.basename(file);
  const base = `http://127.0.0.1:${port}/media/${encodeURIComponent(name)}/${preset}`;
  const result = { file: name, preset, browser: browserName };
  const prepT0 = Date.now();
  let prep;
  try {
    const r = await fetch(`${base}/prepare`);
    if (!r.ok) throw new Error(`prepare HTTP ${r.status}: ${await r.text()}`);
    prep = await r.json();
  } catch (e) {
    return { ...result, ok: false, problems: [`prepare: ${e.message}`] };
  }
  result.prepareMs = Date.now() - prepT0;
  result.label = prep.label;
  result.timelineOffsetMs = +((prep.presentation_offset_s || 0) * 1000).toFixed(3);
  result.explain = prep.explain;
  result.segments = prep.segments;
  result.durationS = prep.duration_s;
  const durs = prep.segment_durations || [];
  const seg0 = durs[0] ?? 6;
  const plan = prep.plan;
  const sub = (plan.subtitles || []).find((s) => s.op === "web_vtt" || s.op?.op === "web_vtt");
  let subtitle = null;
  if (sub) {
    const t = firstCue(file, sub.track);
    // Player time = source time + D (every track is shifted by the same
    // presentation offset so composition offsets stay ≥ 0), so a cue is in
    // sync with the video when it appears at source time + D.
    if (t != null) subtitle = { expectFirstCue: t + (prep.presentation_offset_s || 0), sourceFirstCue: t };
  }
  const page = await ctx.newPage();
  const consoleMsgs = [];
  page.on("console", (m) => {
    if (args.debug || m.type() === "error" || m.type() === "warning") consoleMsgs.push(`${m.type()}: ${m.text()}`.slice(0, 400));
  });
  page.on("pageerror", (e) => consoleMsgs.push(`pageerror: ${e.message}`.slice(0, 300)));
  try {
    await page.goto(`http://127.0.0.1:${pagePort}/player.html`);
    const frameDur = prep.frame_duration_s || 0.042;
    const cfg = {
      url: `${base}/master.m3u8`,
      mode: browserName === "safari" ? "native" : "hlsjs",
      seeks: seeksFor(prep.duration_s, seg0),
      seg0,
      duration: prep.duration_s,
      frameDur,
      subtitle,
      hlsConfig: Object.assign({}, WEB_CLIENT_HLS_CONFIG, args["hls-config"] ? JSON.parse(args["hls-config"]) : {}),
    };
    const startEpoch = Date.now();
    const r = await Promise.race([
      page.evaluate((c) => window.fluxRun(c), cfg),
      new Promise((_, rej) => setTimeout(() => rej(new Error("case timed out after 300 s")), 300_000)),
    ]);
    Object.assign(result, r);
    if (r.ttffMs != null) result.ttffFromPrepareMs = startEpoch - prepT0 + r.ttffMs;
  } catch (e) {
    // Keep what the page measured before the time-out.
    const partial = await page.evaluate(() => window.__fluxRes || null).catch(() => null);
    if (partial) Object.assign(result, partial);
    result.ok = false;
    result.problems = [...(result.problems || []), `harness: ${e.message}`];
  } finally {
    result.console = args.debug ? consoleMsgs : consoleMsgs.slice(0, 40);
    await page.close().catch(() => {});
  }
  return result;
}

// ---------------------------------------------------------------- main
const serve = startServe();
process.on("exit", () => serve.kill("SIGTERM"));
const pageSrv = await startPageServer();
if (!(await waitHttp(`http://127.0.0.1:${port}/`, 20000))) {
  console.error("hls-serve did not start");
  process.exit(2);
}
const load = os.loadavg();
const ctx = await launch();
const probePage = await ctx.newPage();
await probePage.goto(`http://127.0.0.1:${pagePort}/player.html`);
const support = await probePage.evaluate(() => window.fluxCodecSupport());
const ua = await probePage.evaluate(() => navigator.userAgent);
await probePage.close();
console.error(`${browserName}: ${ua}`);

const cases = [];
for (const preset of presetList) {
  for (const f of files) {
    const t0 = Date.now();
    const r = await runCase(ctx, f, preset);
    cases.push(r);
    const seekMs = (r.seeks || []).map((s) => s.firstFrameMs).filter((x) => x != null);
    console.error(
      `${r.ok ? "ok  " : "FAIL"} ${preset.padEnd(8)} ${r.file.padEnd(36)} ${String(r.label || "").padEnd(18)} ttff=${r.ttffMs ?? "-"}ms ` +
        `seeks=${seekMs.join("/")}ms drift_p50=${r.drift ? r.drift.p50.toFixed(1) : "-"}ms dropped=${r.quality?.final?.dropped ?? "-"}/${r.quality?.final?.decoded ?? "-"} ` +
        `${((Date.now() - t0) / 1000).toFixed(1)}s ${r.ok ? "" : JSON.stringify(r.problems)}${(r.warnings || []).length ? " warn=" + JSON.stringify(r.warnings) : ""}`,
    );
  }
}
await ctx.close();
pageSrv.close();
serve.kill("SIGTERM");

const summary = {
  cases: cases.length,
  ok: cases.filter((c) => c.ok).length,
  failed: cases.filter((c) => !c.ok).map((c) => `${c.file}/${c.preset}: ${(c.problems || []).join("; ")}`),
  warnings: cases.filter((c) => (c.warnings || []).length).map((c) => `${c.file}/${c.preset}: ${c.warnings.join("; ")}`),
  hls_errors_nonfatal: cases.reduce((a, c) => a + (c.hlsErrors || []).filter((e) => !e.fatal).length, 0),
  ttff_ms: cases.map((c) => c.ttffMs).filter((x) => x != null),
  seek_first_frame_ms: cases.flatMap((c) => (c.seeks || []).map((s) => s.firstFrameMs)).filter((x) => x != null),
};
const doc = {
  component: "flux-hls",
  name: "browser",
  browser: browserName,
  user_agent: ua,
  hlsjs: "1.7.3",
  presets: presetList,
  machine: "M1 Max 64 GB, shared with another project (numbers are from a shared machine)",
  load_avg_1m_start: +load[0].toFixed(2),
  load_avg_1m_end: +os.loadavg()[0].toFixed(2),
  codec_support: support,
  summary,
  cases,
};
if (out) {
  mkdirSync(path.dirname(out), { recursive: true });
  writeFileSync(out, JSON.stringify(doc, null, 2));
} else {
  console.log(JSON.stringify(doc, null, 2));
}
console.error(`${summary.ok}/${summary.cases} ok → ${out ?? "stdout"}`);
process.exit(0);
