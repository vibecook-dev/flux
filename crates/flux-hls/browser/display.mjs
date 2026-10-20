#!/usr/bin/env node
// Display geometry check (Phase D: anamorphic `pasp`, rotation matrix):
// what size Chrome (hls.js/MSE) and AVPlayer present for each case.
// Chrome reports `video.videoWidth/videoHeight` (the natural size, after
// the pixel aspect and rotation it honours); AVPlayer `presentationSize`
// (bench/playback-remux-probe/avcheck/check).
//
//   node crates/flux-hls/browser/display.mjs [--serve-bin PATH] [--port 18400] [--out FILE] [--no-avplayer]
//
// Inputs: the Phase D clips flux-test-media makes ($FLUX_TEST_CLIPS; run
// `cargo test -p flux-hls --test phase_d` first). See env.mjs.
// Results: $FLUX_BENCH_RESULTS/flux-hls-display-<ts>.json

import { spawn, execFileSync } from "node:child_process";
import { createServer } from "node:http";
import { readFileSync, mkdirSync } from "node:fs";
import { createRequire } from "node:module";
import os from "node:os";
import path from "node:path";

import { AVCHECK, CLIPS, NODE_MODULES as NM, TARGET, TMP, resultsPath, writeResults } from "./env.mjs";

const require = createRequire(path.join(NM, "noop.js"));
const pw = require("playwright-core");
const args = Object.fromEntries(
  process.argv.slice(2).reduce((acc, a, i, all) => {
    if (a.startsWith("--")) acc.push([a.slice(2), all[i + 1] && !all[i + 1].startsWith("--") ? all[i + 1] : "1"]);
    return acc;
  }, []),
);
const port = Number(args.port || 18400);
const serveBin = args["serve-bin"] || path.join(TARGET, "release/hls-serve");
const scratch = CLIPS;
const ts = new Date().toISOString().replace(/[-:]/g, "").replace(/\..*/, "").replace("T", "-");
const out = resultsPath(`flux-hls-display-${ts}.json`, args.out);
const checker = AVCHECK;
if (!checker) args["no-avplayer"] = "1";

// [file, chrome preset, avplayer preset, coded size, display size expected]
const CASES = [
  ["anamorphic.mkv", "chrome", "safari", "720x576", "1024x576", "anamorphic 64:45, copy (pasp)"],
  ["anamorphic.mp4", "chrome", "safari", "720x576", "1024x576", "anamorphic 64:45 MP4, copy (pasp)"],
  ["rot90_h264.mov", "chrome", "safari", "640x360", "360x640", "tkhd 90° CCW, copy (matrix)"],
  ["rot90_hevc.mov", "legacy", "legacy", "640x360", "360x640", "tkhd 90° CCW, transcode (upright pixels)"],
];

const serve = spawn("nice", ["-n", "19", serveBin, scratch, "--port", String(port), "--preset", "chrome"], {
  stdio: ["ignore", "ignore", "inherit"],
});
process.on("exit", () => serve.kill("SIGTERM"));
async function waitHttp(url, ms) {
  const t0 = Date.now();
  while (Date.now() - t0 < ms) {
    try {
      if ((await fetch(url)).ok) return true;
    } catch (_) {}
    await new Promise((r) => setTimeout(r, 100));
  }
  return false;
}
if (!(await waitHttp(`http://127.0.0.1:${port}/`, 20000))) {
  console.error("hls-serve did not start");
  process.exit(2);
}
// The page: hls.js from the web client's node_modules, same origin as hls-serve
// is not needed (hls-serve sends CORS headers).
const pagePort = port + 1;
const hlsjs = readFileSync(path.join(NM, "hls.js/dist/hls.js"));
const page = `<!doctype html><meta charset=utf-8><video id=v muted playsinline></video><script src="/hls.js"></script>
<script>
window.measure = (url) => new Promise((resolve) => {
  const v = document.getElementById("v");
  const hls = new Hls({ enableWorker: true });
  const done = (r) => { try { hls.destroy(); } catch (_) {} resolve(r); };
  hls.on(Hls.Events.ERROR, (_, d) => { if (d.fatal) done({ error: d.details }); });
  v.addEventListener("loadeddata", () => {
    v.requestVideoFrameCallback((_, m) => done({ videoWidth: v.videoWidth, videoHeight: v.videoHeight, frameWidth: m.width, frameHeight: m.height }));
    v.play().catch(() => {});
  }, { once: true });
  setTimeout(() => done({ error: "timeout" }), 20000);
  hls.loadSource(url);
  hls.attachMedia(v);
});
</script>`;
const srv = createServer((req, res) => {
  if (req.url === "/hls.js") {
    res.writeHead(200, { "content-type": "text/javascript" });
    return res.end(hlsjs);
  }
  res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
  res.end(page);
});
await new Promise((r) => srv.listen(pagePort, "127.0.0.1", r));

const tmp = path.join(TMP, "flux-hls-display-browser");
mkdirSync(tmp, { recursive: true });
const ctx = await pw.chromium.launchPersistentContext(path.join(tmp, "profile"), {
  channel: "chrome",
  headless: true,
  args: ["--mute-audio", "--autoplay-policy=no-user-gesture-required"],
});
const browserVersion = ctx.browser()?.version() ?? "chrome";
const rows = [];
for (const [file, preset, avPreset, coded, expected, what] of CASES) {
  const p = await ctx.newPage();
  await p.goto(`http://127.0.0.1:${pagePort}/`);
  const url = `http://127.0.0.1:${port}/media/${file}/${preset}/master.m3u8`;
  const chrome = await p.evaluate((u) => window.measure(u), url);
  await p.close();
  let avplayer = null;
  if (!args["no-avplayer"]) {
    const avUrl = `http://127.0.0.1:${port}/media/${file}/${avPreset}/master.m3u8`;
    try {
      const text = execFileSync("nice", ["-n", "19", checker, avUrl, "0"], { encoding: "utf8", timeout: 40000 });
      const m = text.match(/presentationSize: \(([\d.]+), ([\d.]+)\)/);
      avplayer = m ? `${Math.round(+m[1])}x${Math.round(+m[2])}` : text.trim().split("\n").pop();
    } catch (e) {
      avplayer = `error: ${e.message.split("\n")[0]}`;
    }
  }
  const row = { file, what, coded, expected, chrome_preset: preset, chrome, avplayer_preset: avPreset, avplayer };
  console.log(JSON.stringify(row));
  rows.push(row);
}
await ctx.close();
srv.close();
writeResults(out, { tool: "flux-hls display", browser: browserVersion, load: os.loadavg(), rows });
if (out) console.error(`wrote ${out}`);
process.exit(0);
