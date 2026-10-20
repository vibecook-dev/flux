#!/usr/bin/env python3
"""Benchmark flux-server playback start and seek latency — the mirror of
jellyfin_playback_bench.py: same files, same library folder (APFS clones in
tmp/bench/play-lib), same client walk (hls_walk.py), same seek targets, same
warm-up play and the same per-run rotation of the file order.

Starts an isolated flux-server (`nice -n 19`, 127.0.0.1 only, tailnet off,
jobs off, watcher off) with one Movies library holding just the files under
test, plays WARMUP_FILE once (throwaway, reported as `warmup`), then for each
file in the rotated order, after the gate:

  1. POST /api/v1/play (for that file's asset) with ClientCaps equivalent to
     Jellyfin's BROWSER_PROFILE (below): progressive MP4/WebM only, so MKV and
     TS go over HLS fMP4; H.264, HEVC and AV1 (VP9 for WebM) with no profile,
     level, size or HDR limits (the profile has no CodecProfiles); AAC, MP3,
     Opus, FLAC, AC-3, E-AC-3 up to 6 channels (the transcoding profile's
     MaxAudioChannels); 200 Mb/s; audio track 1 (Jellyfin's AudioStreamIndex 1).
     This is Jellyfin's PlaybackInfo step. flux's /play also starts producing
     segment 0 (warm-up), which is part of its design, so `play_plus_ttfs_s`
     (both steps) is the fair end-to-end number.
  2. TTFS = master -> variant -> init -> segment 0, then the default demuxed
     audio rendition's playlist -> init -> segment 0 (sequential).
  3. Cold seeks to the segments covering 600/3600/5100/30 s (video + audio).

Usage:
  tools/bench/flux_playback_bench.py --media "$FLUX_TEST_MEDIA/features" --rotate 2
"""
from __future__ import annotations

import argparse
import json
import shutil
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import benchlib as B  # noqa: E402
import hls_walk  # noqa: E402
from flux_scan_bench import DEFAULT_BIN, FluxAdapter  # noqa: E402
from jellyfin_playback_bench import DEFAULT_FILES, WARMUP_FILE  # noqa: E402

AUDIO_CHANNELS = 6  # BROWSER_PROFILE TranscodingProfiles[0].MaxAudioChannels


def video_cap(codec: str) -> dict:
    return {"codec": codec, "profiles": [], "max_level": None, "max_bit_depth": None, "max_width": None,
            "max_height": None, "hdr": ["hdr10", "hdr10_plus", "hlg", "dolby_vision"], "dovi_profiles": [5, 7, 8]}


# Jellyfin's BROWSER_PROFILE (jellyfin_playback_bench.py), as flux ClientCaps.
BROWSER_CAPS = {
    "client": "bench/jellyfin-browser-profile",
    "delivery": ["progressive_file", "hls_fmp4"],
    "progressive_containers": ["mp4", "web_m"],
    "video": [video_cap(c) for c in ("h264", "hevc", "av1", "vp9")],
    "audio": [{"codec": c, "max_channels": AUDIO_CHANNELS} for c in ("aac", "mp3", "opus", "flac", "ac3", "eac3")],
    "display_hdr": False,
    "subtitles": ["web_vtt", "subrip", "ass"],
    "needs_demuxed_audio": False,
    "max_bitrate": 200_000_000,
}


def describe_plan(plan: dict) -> dict:
    v = plan.get("video") or {}
    return {"delivery": plan.get("delivery"),
            "video": "copy" if v.get("op") == "copy" else f"transcode {v.get('codec')} {v.get('width')}x{v.get('height')}",
            "audio": [f"{a['track']}:{'copy' if a['op'].get('op') == 'copy' else 'transcode ' + str(a['op'].get('codec')) + ' ' + str(a['op'].get('channels'))}"
                      for a in plan.get("audio") or []],
            "demuxed_audio": plan.get("demuxed_audio"), "est_cpu_ms_per_min": plan.get("est_cpu_ms_per_min")}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--media", default=str(B.MEDIA / "features"))
    ap.add_argument("--files", nargs="*", default=DEFAULT_FILES)
    ap.add_argument("--rotate", type=int, default=0, help="start the file order at index k mod n (run k)")
    ap.add_argument("--warmup", default=WARMUP_FILE, help="throwaway first play ('' for none)")
    ap.add_argument("--lib-dir", default=str(B.SCRATCH / "play-lib"), help="APFS clones of the files (made if missing)")
    ap.add_argument("--server-bin", default=str(DEFAULT_BIN))
    ap.add_argument("--state", default=str(B.SCRATCH / "flux-play-state"))
    ap.add_argument("--ffmpeg", default="/opt/homebrew/bin/ffmpeg", help="for making derived play files only")
    ap.add_argument("--port", type=int, default=B.FLUX_PORT)
    ap.add_argument("--tag", default="", help="suffix for the result file name")
    ap.add_argument("--out", default=str(B.RESULTS))
    args = ap.parse_args()

    order = B.rotated(list(args.files), args.rotate)
    B.gate("playback library")  # may stream-copy a 0.5 GB file
    lib = B.prepare_play_lib(Path(args.media), order + ([args.warmup] if args.warmup else []), Path(args.lib_dir),
                             args.ffmpeg)
    gate = B.gate("flux playback: first start")
    ad = FluxAdapter(Path(args.server_bin), Path(args.state), args.port)
    result: dict = {"tool": "flux", "started": datetime.now(timezone.utc).isoformat(), "gate": gate,
                    "machine": B.machine(), "lib_dir": str(lib), "rotate": args.rotate, "order": order,
                    "warmup_file": args.warmup or None, "caps": BROWSER_CAPS, "files": {},
                    "how": hls_walk.__doc__.strip()}
    try:
        first = ad.start()
        result["first_start"] = {**first, "gate": gate, "end": B.phase_end()}
        result["listening"] = first.get("listening")
        result["idle_after_start"] = B.proc_stats(ad.pid)
        t = time.monotonic()
        lib_id = ad.api.req("POST", "/api/v1/libraries", {"name": "Bench", "kind": "movies", "roots": [str(lib)]})["id"]
        ad.libs = {"Bench": lib_id}
        while True:
            L = next(x for x in ad.libraries() if x["id"] == lib_id)
            if L["scan"]["state"] != "scanning" and L["scan"]["finished_at"] is not None:
                break
            time.sleep(0.01)
        result["library_scan"] = {"wall_s": round(time.monotonic() - t, 3), "items": L["item_count"]}

        by_file: dict[str, tuple[int, int]] = {}
        page = ad.api.req("GET", f"/api/v1/libraries/{lib_id}/items?limit=100")
        for it in page["items"]:
            d = ad.api.req("GET", f"/api/v1/items/{it['id']}")
            for a in d.get("assets") or []:
                by_file[Path(a["path"]).name] = (it["id"], a["id"])
        base = ad.api.base

        def play(f: str) -> dict:
            if f not in by_file:
                return {"error": "not in library"}
            item_id, asset_id = by_file[f]
            entry: dict = {"load1": B.load1()}
            t = time.perf_counter()
            res = ad.api.req("POST", "/api/v1/play", {"item_id": item_id, "asset_id": asset_id, "caps": BROWSER_CAPS,
                                                      "audio_track": 1, "subtitle_track": None, "start_ms": 0})
            entry["playbackinfo_s"] = round(time.perf_counter() - t, 4)
            if res.get("asset_id") != asset_id:
                entry["error"] = f"flux played asset {res.get('asset_id')}, not {asset_id}"
                return entry
            plan = res["plan"]
            entry["decision"] = describe_plan(plan)
            entry["explain"] = res.get("explain")
            entry["audio_renditions"] = [(a["rendition"], a["codec"], a["channels"], a["default"])
                                         for a in res.get("audio") or []]
            if plan.get("delivery") == "progressive_file":
                entry["note"] = "direct play chosen; no HLS session to time"
                return entry
            entry.update(hls_walk.walk(base + res["url"], {}))
            entry["play_plus_ttfs_s"] = round(entry["playbackinfo_s"] + entry["ttfs_s"], 4)
            entry["server_rss_mb"] = B.proc_stats(ad.pid)["rss_mb"]
            return entry

        result["load1_before_files"] = B.load1()
        sampler = B.Sampler(ad.pid)
        st0 = B.proc_stats(ad.pid)
        if args.warmup:
            g = B.gate("flux warm-up play")
            result["warmup"] = {**play(args.warmup), "gate": g, "end": B.phase_end()}
            print("warm-up", args.warmup, json.dumps({k: result["warmup"].get(k) for k in (
                "playbackinfo_s", "ttfs_s", "play_plus_ttfs_s")}), flush=True)
        for pos, f in enumerate(order):
            g = B.gate(f"flux play {f}")
            entry = {**play(f), "position": pos, "gate": g, "end": B.phase_end()}
            result["files"][f] = entry
            print(f, json.dumps({k: entry.get(k) for k in ("playbackinfo_s", "ttfs_video_s", "ttfs_s", "error")}),
                  [s.get("latency_s", s.get("error")) for s in entry.get("seeks", [])], (entry.get("decision") or {}).get("video"),
                  (entry.get("decision") or {}).get("audio"), flush=True)
        result["playback_peak"] = sampler.stop()
        result["playback_usage"] = B.delta(st0, B.proc_stats(ad.pid))
        result["after_playback"] = B.proc_stats(ad.pid)
    finally:
        result["stop"] = ad.stop()
        shutil.rmtree(ad.state, ignore_errors=True)
    result["load1_end"] = B.load1()
    B.write_result(f"{B.RESULT_PREFIX}-playback-flux{args.tag}", result, Path(args.out))


if __name__ == "__main__":
    main()
