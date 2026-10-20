#!/usr/bin/env python3
"""Benchmark Jellyfin playback start and seek latency — the baseline flux must beat.

Starts an isolated Jellyfin (`nice -n 19`, 127.0.0.1 only, random admin
password, scheduled-task triggers cleared; see jellyfin_scan_bench.py) with
one library holding just the files under test, then:

  0. a throwaway warm-up play of WARMUP_FILE (not in the measured set): it
     absorbs the server's first-request cost (about +100 ms PlaybackInfo and
     +100 ms TTFS on Jellyfin) and is reported on its own as `warmup`;
  1. for each test file, in an order rotated per run (`--rotate k` starts at
     file k mod n, so over n runs every file is played in every position),
     after the gate (QUIET absent, load < 10):
     a. POST /Items/{id}/PlaybackInfo with a browser-like device profile
        (fMP4 HLS, H.264/HEVC/AV1 video, AAC/Opus audio, no MKV/TS direct
        play), so MKV and TS sources take Jellyfin's remux ("direct stream")
        or transcode path;
     b. time-to-first-segment (hls_walk.py): GET master.m3u8 -> variant
        playlist -> init -> segment 0 (server-side proxy for time-to-first-frame);
     c. seek: GET the segment that covers each seek target (forces Jellyfin
        to restart ffmpeg at that position), timing each fetch.

flux_playback_bench.py runs the same walk against flux-server with equivalent
client capabilities. The library is a folder of APFS clones of the files
(tmp/bench/play-lib, shared with the flux harness so both servers read the
same inodes and see the same page cache; seek_2h_360p.ts is a stream copy of
long_2h_360p.mkv made there once, as media/features has no long TS). Results
are written as JSON, including what Jellyfin actually did (the ffmpeg codec
arguments from its transcode log: copy or encode, per stream).

Usage:
  tools/bench/jellyfin_playback_bench.py --media "$FLUX_TEST_MEDIA/features" --rotate 2
"""
from __future__ import annotations

import argparse
import json
import re
import sys
import time
import urllib.error
import urllib.parse
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import benchlib as B  # noqa: E402
import hls_walk  # noqa: E402
from jellyfin_scan_bench import (  # noqa: E402
    DEFAULT_JELLYFIN, MOVIE_TYPES, Jellyfin, clear_task_triggers, extraction_evidence, launch, lib_options,
    task_runs, tasks_ran_since, wait_ready, wait_scan, wizard)

DEFAULT_FILES = [
    "h264_multi_audio_subs.mkv",   # remux MKV -> fMP4, AAC passthrough
    "h264_truehd71.mkv",           # video copy + TrueHD -> AAC
    "hevc_main10_hdr10_eac3.mkv",  # HEVC HDR10 remux (+ E-AC-3 -> AAC)
    "hevc_4k_60mbps.mkv",          # high-bitrate remux
    "long_2h_360p.mkv",            # seek-latency file
    "h264_aac.m2ts",               # M2TS remux (H.264 + AAC, 30 s); flux remuxes TS in-process
    "seek_2h_360p.ts",             # MPEG-TS seek file: stream copy of long_2h_360p.mkv (B.DERIVED_PLAY_FILES)
]
WARMUP_FILE = "intro_shared.mkv"  # H.264 + AAC, 20 s: the throwaway first play
SEEK_TARGETS_S = hls_walk.SEEK_TARGETS_S

BROWSER_PROFILE = {
    "MaxStreamingBitrate": 200_000_000,
    "MaxStaticBitrate": 200_000_000,
    "MusicStreamingTranscodingBitrate": 384000,
    "DirectPlayProfiles": [
        {"Container": "mp4,m4v", "Type": "Video", "VideoCodec": "h264,hevc,av1", "AudioCodec": "aac,mp3,opus,flac,ac3,eac3"},
        {"Container": "webm", "Type": "Video", "VideoCodec": "vp9,av1", "AudioCodec": "opus"},
    ],
    "TranscodingProfiles": [
        {"Container": "mp4", "Type": "Video", "AudioCodec": "aac,ac3,eac3,opus,flac", "VideoCodec": "h264,hevc,av1",
         "Context": "Streaming", "Protocol": "hls", "MaxAudioChannels": "6", "MinSegments": "1",
         "BreakOnNonKeyFrames": True},
    ],
    "ContainerProfiles": [],
    "CodecProfiles": [],
    "SubtitleProfiles": [{"Format": "vtt", "Method": "External"}, {"Format": "srt", "Method": "External"},
                         {"Format": "ass", "Method": "External"}],
}


def ffmpeg_ops(log_dir: Path, after: float, source: Path) -> dict | None:
    """What Jellyfin's ffmpeg did for `source` since `after`: one transcode log
    per ffmpeg process (so a seek that restarts ffmpeg adds one), with the
    codec arguments of each (`copy`, or the encoder)."""
    runs = []
    for p in sorted(log_dir.glob("FFmpeg.*.log"), key=lambda p: p.stat().st_mtime):
        if p.stat().st_mtime < after:
            continue
        text = p.read_text(errors="replace")
        if f'"Path":"{source}"' not in text and str(source) not in text:
            continue
        cmd = next((l for l in text.splitlines() if "ffmpeg" in l and " -i " in l), "")
        runs.append({"log": p.name, "seek": (re.findall(r"-ss (\S+)", cmd) or [None])[0],
                     "video": re.findall(r"-c(?:odec)?:v(?::\d+)?\s+(\S+)", cmd),
                     "audio": re.findall(r"-c(?:odec)?:a(?::\d+)?\s+(\S+)", cmd),
                     "audio_args": " ".join(re.findall(r"-(?:ac|ab) \S+", cmd))})
    if not runs:
        return None
    return {"ffmpeg_starts": len(runs), "video": runs[0]["video"], "audio": runs[0]["audio"], "runs": runs}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--media", default=str(B.MEDIA / "features"))
    ap.add_argument("--files", nargs="*", default=DEFAULT_FILES)
    ap.add_argument("--rotate", type=int, default=0, help="start the file order at index k mod n (run k)")
    ap.add_argument("--warmup", default=WARMUP_FILE, help="throwaway first play ('' for none)")
    ap.add_argument("--lib-dir", default=str(B.SCRATCH / "play-lib"), help="APFS clones of the files (made if missing)")
    ap.add_argument("--jellyfin", default=DEFAULT_JELLYFIN)
    ap.add_argument("--state", default=str(B.SCRATCH / "jellyfin-play-state"))
    ap.add_argument("--ffmpeg", default="/opt/homebrew/bin/ffmpeg")
    ap.add_argument("--port", type=int, default=B.JELLYFIN_PORT)
    ap.add_argument("--tag", default="", help="suffix for the result file name")
    ap.add_argument("--out", default=str(B.RESULTS))
    args = ap.parse_args()

    order = B.rotated(list(args.files), args.rotate)
    B.gate("playback library")  # may stream-copy a 0.5 GB file
    lib = B.prepare_play_lib(Path(args.media), order + ([args.warmup] if args.warmup else []), Path(args.lib_dir),
                             args.ffmpeg)
    gate = B.gate("jellyfin playback: first start")
    B.wait_port_free(args.port)
    state = Path(args.state)
    since = datetime.now(timezone.utc).isoformat()
    t0 = time.monotonic()
    proc = launch(args.jellyfin, state, args.ffmpeg, args.port)
    jf = Jellyfin(f"http://127.0.0.1:{args.port}")
    result: dict = {"tool": "jellyfin", "started": datetime.now(timezone.utc).isoformat(), "gate": gate,
                    "machine": B.machine(), "lib_dir": str(lib), "rotate": args.rotate, "order": order,
                    "warmup_file": args.warmup or None, "files": {}, "how": hls_walk.__doc__.strip()}
    stop = None
    tasks: list[dict] = []
    try:
        result["first_start"] = {**wait_ready(jf, proc, t0), "gate": gate, "end": B.phase_end()}
        result["listening"] = B.assert_loopback(proc.pid, lambda: B.stop_process(proc), "jellyfin")
        result["idle_after_start"] = B.proc_stats(proc.pid)
        auth = wizard(jf)
        result["scheduled_tasks"] = clear_task_triggers(jf)
        user_id = auth["User"]["Id"]
        jf.req("POST", "/Library/VirtualFolders", lib_options(str(lib), MOVIE_TYPES),
               query={"name": "Bench", "collectionType": "movies", "refreshLibrary": "false"})
        started = datetime.now(timezone.utc)
        jf.req("POST", "/Library/Refresh")
        result["library_scan"] = {k: v for k, v in wait_scan(jf, proc.pid, started, 600).items() if k != "peak"}

        items = jf.req("GET", "/Items", query={"userId": user_id, "recursive": "true", "includeItemTypes": "Movie",
                                               "fields": "Path"})["Items"]
        by_file = {Path(i["Path"]).name: i for i in items if i.get("Path")}
        headers = {"Authorization": f'MediaBrowser Client="flux-bench", Device="bench", DeviceId="flux-bench-1", '
                                    f'Version="1.0", Token="{jf.token}"'}
        base = jf.base

        def play(f: str) -> dict:
            item = by_file.get(f)
            if not item:
                return {"error": "not in library"}
            entry: dict = {"load1": B.load1()}
            t_file = time.time()
            t = time.perf_counter()
            info = jf.req("POST", f"/Items/{item['Id']}/PlaybackInfo",
                          {"UserId": user_id, "DeviceProfile": BROWSER_PROFILE, "MaxStreamingBitrate": 200_000_000,
                           "EnableDirectPlay": True, "EnableDirectStream": True, "EnableTranscoding": True,
                           "AutoOpenLiveStream": True, "AudioStreamIndex": 1},
                          query={"userId": user_id})
            entry["playbackinfo_s"] = round(time.perf_counter() - t, 4)
            src = info["MediaSources"][0]
            entry["source_path"] = src.get("Path")
            entry["decision"] = {k: src.get(k) for k in ("SupportsDirectPlay", "SupportsDirectStream",
                                                       "SupportsTranscoding", "TranscodingSubProtocol",
                                                       "TranscodingContainer")}
            url = src.get("TranscodingUrl")
            if not url:
                entry["note"] = "direct play chosen; no HLS session to time"
                return entry
            q = urllib.parse.parse_qs(urllib.parse.urlsplit(url).query)
            entry["transcode_reasons"] = re.findall(r"TranscodeReasons=([^&]+)", url)
            entry["decision"]["url_params"] = {k: q[k][0] for k in q if k.lower() in (
                "videocodec", "audiocodec", "allowvideostreamcopy", "allowaudiostreamcopy", "segmentcontainer",
                "transcodingmaxaudiochannels", "audiostreamindex")}
            entry.update(hls_walk.walk(base + url, headers))
            entry["play_plus_ttfs_s"] = round(entry["playbackinfo_s"] + entry["ttfs_s"], 4)
            entry["ffmpeg"] = ffmpeg_ops(state / "log", t_file, lib / f)
            entry["server_rss_mb"] = B.proc_stats(proc.pid)["rss_mb"]
            # Stop the transcode session before the next file.
            play_session = re.search(r"PlaySessionId=([^&]+)", url)
            try:
                jf.req("DELETE", "/Videos/ActiveEncodings",
                       query={"deviceId": "flux-bench-1", "playSessionId": play_session.group(1) if play_session else ""})
            except urllib.error.HTTPError:
                pass
            return entry

        result["load1_before_files"] = B.load1()
        sampler = B.Sampler(proc.pid)
        st0 = B.proc_stats(proc.pid)
        if args.warmup:
            g = B.gate("jellyfin warm-up play")
            result["warmup"] = {**play(args.warmup), "gate": g, "end": B.phase_end()}
            print("warm-up", args.warmup, json.dumps({k: result["warmup"].get(k) for k in (
                "playbackinfo_s", "ttfs_s", "play_plus_ttfs_s")}), flush=True)
        for pos, f in enumerate(order):
            g = B.gate(f"jellyfin play {f}")
            entry = {**play(f), "position": pos, "gate": g, "end": B.phase_end()}
            result["files"][f] = entry
            print(f, json.dumps({k: entry.get(k) for k in ("playbackinfo_s", "ttfs_s", "transcode_reasons")}),
                  [s.get("latency_s", s.get("error")) for s in entry.get("seeks", [])], (entry.get("ffmpeg") or {}).get("video"),
                  (entry.get("ffmpeg") or {}).get("audio"), flush=True)
        result["playback_peak"] = sampler.stop()
        result["playback_usage"] = B.delta(st0, B.proc_stats(proc.pid))
        result["after_playback"] = B.proc_stats(proc.pid)
        tasks = task_runs(jf)
    finally:
        stop = B.stop_process(proc)
    result["stop"] = stop
    ev = extraction_evidence(state)
    ev["tasks_run_during_benchmark"] = tasks_ran_since(tasks, since)
    ev["extraction_tasks_run"] = [t["key"] for t in ev["tasks_run_during_benchmark"] if t["extraction"]]
    ev["no_extraction"] = bool(ev.get("no_extraction")) and not ev["extraction_tasks_run"]
    result["evidence"] = {"extraction": ev}
    result["load1_end"] = B.load1()
    B.write_result(f"{B.RESULT_PREFIX}-playback-jellyfin{args.tag}", result, Path(args.out))


if __name__ == "__main__":
    main()
