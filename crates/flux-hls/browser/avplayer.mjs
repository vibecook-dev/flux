#!/usr/bin/env node
// AVPlayer check (macOS native HLS): plays each case through the compiled
// bench/playback-remux-probe/avcheck/check (AVPlayer, muted, exact seek)
// against hls-serve and records ready / first advance / seek completion /
// errorLog.
//
//   node crates/flux-hls/browser/avplayer.mjs [--serve-bin PATH] [--port 18300] [--out FILE]
//
// Media from $FLUX_TEST_MEDIA, the checker from $FLUX_AVCHECK (see env.mjs).
// Results: $FLUX_BENCH_RESULTS/flux-hls-avplayer.json

import { spawn, execFileSync } from "node:child_process";
import os from "node:os";
import path from "node:path";

import { AVCHECK, TARGET, TMP, requireMedia, resultsPath, writeResults } from "./env.mjs";

const MEDIA = requireMedia();
if (!AVCHECK) {
  console.error("skipping: FLUX_AVCHECK (the AVPlayer checker) is not set");
  process.exit(0);
}
const args = Object.fromEntries(
  process.argv.slice(2).reduce((acc, a, i, all) => {
    if (a.startsWith("--")) acc.push([a.slice(2), all[i + 1]]);
    return acc;
  }, []),
);
const port = Number(args.port || 18300);
const serveBin = args["serve-bin"] || path.join(TARGET, "release/hls-serve");
const checker = AVCHECK;
const out = resultsPath("flux-hls-avplayer.json", args.out);
const roots = [path.join(MEDIA, "features"), path.join(MEDIA, "playback-research"), path.join(TMP, "hls/media")];

// [file, preset, seek seconds, what it exercises]
const CASES = [
  ["hevc_main10_hdr10_eac3.mkv", "safari", 20, "4K HDR10 remux + E-AC-3 copy"],
  ["hevc_main10_hdr10_hvc1.mp4", "safari", 20, "4K HDR10 MP4 remux"],
  ["hevc_4k_60mbps.mkv", "safari", 40, "60 Mb/s HEVC remux"],
  ["synth_hevc10_opengop_10min.mkv", "safari", 255.5, "open-GOP HEVC remux"],
  ["h264_multi_audio_subs.mkv", "safari", 45, "multi-audio + WebVTT"],
  ["h264_truehd71.mkv", "safari", 20, "TrueHD → E-AC-3/AAC"],
  ["h264_dts51.mkv", "safari", 20, "DTS → E-AC-3/AAC"],
  ["long_2h_360p.mkv", "safari", 5100, "2 h remux"],
  ["synth_h264_10min.mkv", "safari", 255.5, "H.264 remux"],
  ["h264_aac_moov_end.mp4", "safari", 40, "MP4 moov at end"],
  ["av1_opus.mkv", "safari", 20, "AV1 → H.264 transcode (pieces)"],
  ["hevc_main10_hdr10_eac3.mkv", "legacy", 20, "HDR → SDR transcode (pieces)"],
  ["mpeg2_ac3.ts", "safari", 20, "TS: ffprobe index + transcode (pieces)"],
  // Phase C fallbacks and audio transcodes
  ["frag_h264_aac.mp4", "safari", 40, "fragmented MP4 (moof/trun index)"],
  ["frag_hevc_hdr10.mp4", "safari", 20, "fragmented HEVC HDR10 MP4"],
  ["qt_h264_ac3.mov", "safari", 40, "QuickTime AC-3 without dac3 (copy)"],
  ["avi_h264_ac3.avi", "safari", 40, "AVI with B-frames: transcode"],
  ["h264_opus.mkv", "legacy", 40, "Opus → AAC"],
  ["h264_vorbis.mkv", "safari", 40, "Vorbis → AAC"],
  ["h264_pcm.mov", "safari", 40, "MOV PCM → AAC"],
  ["h264_pcm.mkv", "safari", 40, "MKV PCM → AAC"],
  ["h264_mp3.mkv", "safari", 40, "MP3 → AAC"],
];

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

const serve = spawn("nice", ["-n", "19", serveBin, ...roots, "--port", String(port), "--preset", "safari"], {
  stdio: ["ignore", "ignore", "inherit"],
});
process.on("exit", () => serve.kill("SIGTERM"));
if (!(await waitHttp(`http://127.0.0.1:${port}/`, 20000))) {
  console.error("hls-serve did not start");
  process.exit(2);
}
const load0 = os.loadavg()[0];
const rows = [];
for (const [file, preset, seek, what] of CASES) {
  const url = `http://127.0.0.1:${port}/media/${file}/${preset}/master.m3u8`;
  let text = "";
  try {
    text = execFileSync("nice", ["-n", "19", checker, url, String(seek), "exact"], { encoding: "utf8", timeout: 60000 });
  } catch (e) {
    text = String(e.stdout || "") + String(e.stderr || "") + ` (exit ${e.status})`;
  }
  const num = (re) => {
    const m = re.exec(text);
    return m ? Number(m[1]) : null;
  };
  const errors = (/errors: (\[.*?\])/.exec(text) || [])[1] || null;
  const row = {
    file,
    preset,
    what,
    seek_target_s: seek,
    ready_s: num(/ready=([\d.-]+)s/),
    first_advance_s: num(/firstAdvance=([\d.-]+)s/),
    seek_completion_s: num(/completion after ([\d.]+)s/),
    position_2s_later: num(/position 2s later=([\d.-]+)/),
    errors,
  };
  row.ok =
    !/FAILED/.test(text) &&
    row.first_advance_s > 0 &&
    row.seek_completion_s != null &&
    row.position_2s_later >= seek + 1 &&
    errors === "[]";
  if (!row.ok) row.output = text.slice(0, 800);
  rows.push(row);
  console.error(
    `${row.ok ? "ok  " : "FAIL"} ${preset.padEnd(7)} ${file.padEnd(34)} ready=${row.ready_s} first=${row.first_advance_s} seek=${row.seek_completion_s} pos=${row.position_2s_later} ${errors}`,
  );
}
serve.kill("SIGTERM");
writeResults(out, {
  tool: "flux-hls",
  kind: "avplayer",
  checker: "bench/playback-remux-probe/avcheck/check (AVPlayer, macOS, muted, exact seeks)",
  server: "hls-serve (release)",
  machine: `${os.cpus()[0]?.model ?? "?"}, ${os.platform()} (shared machines skew timings)`,
  load_avg_1m_start: +load0.toFixed(2),
  load_avg_1m_end: +os.loadavg()[0].toFixed(2),
  ok: rows.filter((r) => r.ok).length,
  cases: rows.length,
  rows,
});
console.error(`${rows.filter((r) => r.ok).length}/${rows.length} ok → ${out}`);
process.exit(0);
