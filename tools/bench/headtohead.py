#!/usr/bin/env python3
"""flux vs Jellyfin 12.2, head to head: same machine, same corpus, same harness
code, run back to back and interleaved.

  run        all phases below, then `summarize`
  summarize  rebuild <prefix>-summary-*.json from result files (--files or --since);
             the prefix is FLUX_BENCH_PREFIX, default flux-bench

Phases (every measured phase inside each harness is gated on QUIET and on
load < 10; see benchlib.gate):
  1. full library scans:  flux, Jellyfin, flux by default (--full-order FJFJF for
     3 + 2; Jellyfin's cold scan is ~15 min)
     each run ends with restarts on its populated (full-catalog) state dir
  2. playback: n runs per server (n = number of files), ABBA order
     F J J F F J J F …; run k of each server starts its file order at file k,
     so every file is played in every position; every run starts with a
     throwaway warm-up play (page cache warmed once first)
  3. library-tiny scans, 3 each: F J J F F J
  4. footprint: binary / install sizes (no server run)

Statistics: n, the true median (statistics.median), the min–max range and the
nearest-rank p95. Ratios (Jellyfin / flux) are ratios of medians. The summary
reads every input once (same path, or the same run under another name).

Usage:
  tools/bench/headtohead.py run [--skip full,play,tiny] [--full-order FJF] [--play-runs N] [--tiny-order FJJFFJ]
  tools/bench/headtohead.py summarize --since 20261019-0800
"""
from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path

HERE = Path(__file__).parent
sys.path.insert(0, str(HERE))
import benchlib as B  # noqa: E402
from flux_scan_bench import DEFAULT_BIN  # noqa: E402
from jellyfin_playback_bench import DEFAULT_FILES, WARMUP_FILE  # noqa: E402

JELLYFIN_DIR = B.WORK / "tools/jellyfin/jellyfin"
LIB_FULL = B.MEDIA / "library"
LIB_TINY = B.MEDIA / "library-tiny"
FEATURES = B.MEDIA / "features"


def harness(script: str, *args: str) -> list[str]:
    """Runs one harness at nice 19; returns the result files it wrote."""
    cmd = ["nice", "-n", "19", sys.executable, "-u", str(HERE / script), *args]
    print(f"\n[h2h] {datetime.now():%H:%M:%S} load {B.load1()} $ {' '.join([script, *args])}", flush=True)
    t0 = time.monotonic()
    p = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    print(p.stdout[-6000:], flush=True)
    print(f"[h2h] exit {p.returncode} after {time.monotonic() - t0:.0f} s", flush=True)
    if p.returncode != 0:
        raise SystemExit(f"{script} failed")
    return re.findall(r"^wrote (\S+)$", p.stdout, re.M)


def footprint() -> dict:
    flux_bin = Path(DEFAULT_BIN)
    stripped = B.SCRATCH / "flux-server.stripped"
    B.SCRATCH.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(flux_bin, stripped)
    subprocess.run(["strip", "-S", "-x", str(stripped)], check=True)
    web = HERE.parent.parent / "web/dist"
    jf_web = JELLYFIN_DIR / "jellyfin-web"
    total = B.dir_bytes(JELLYFIN_DIR)
    out = {
        "flux_server_bin_bytes": flux_bin.stat().st_size,
        "flux_server_bin_stripped_bytes": stripped.stat().st_size,
        "flux_server_bin_note": "release profile keeps debug = 1 (symbols for profiling); the stripped copy is `strip -S -x`",
        "flux_web_dist_bytes": B.dir_bytes(web) if web.exists() else None,
        "jellyfin_install_bytes": total,
        "jellyfin_web_bytes": B.dir_bytes(jf_web) if jf_web.exists() else None,
        "jellyfin_server_bytes": total - (B.dir_bytes(jf_web) if jf_web.exists() else 0),
        "jellyfin_note": "self-contained .NET build (runtime included); jellyfin-web is the bundled web client",
        "ffmpeg": subprocess.run(["/opt/homebrew/bin/ffmpeg", "-version"], capture_output=True, text=True).stdout.splitlines()[0],
        "ffmpeg_note": "both servers use the same Homebrew ffmpeg/ffprobe (not counted above)",
    }
    stripped.unlink()
    return out


def abba(n_each: int) -> str:
    """F J J F F J J F … with n_each runs per server."""
    seq = ("FJJF" * n_each)[: 2 * n_each]
    return seq


def run(skip: set[str], *, full_order: str, play_runs: int, tiny_order: str, scan_args: list[str]) -> list[str]:
    files: list[str] = []
    # A newly built binary's first exec is slow on macOS (assessment); do it once outside any measurement.
    subprocess.run([str(DEFAULT_BIN), "--help"], capture_output=True)
    if "full" not in skip:
        for tool in full_order:
            files += harness("flux_scan_bench.py" if tool == "F" else "jellyfin_scan_bench.py",
                             "--library", str(LIB_FULL), *scan_args)
    if "play" not in skip:
        B.gate("playback library and page-cache warm-up")
        lib = B.prepare_play_lib(FEATURES, DEFAULT_FILES + [WARMUP_FILE], B.SCRATCH / "play-lib")
        print(f"[h2h] warmed playback files in {B.warm_files([lib / f for f in DEFAULT_FILES + [WARMUP_FILE]])} s",
              flush=True)
        k = {"F": 0, "J": 0}
        for tool in abba(play_runs):
            script = "flux_playback_bench.py" if tool == "F" else "jellyfin_playback_bench.py"
            files += harness(script, "--rotate", str(k[tool]))
            k[tool] += 1
    if "tiny" not in skip:
        for tool in tiny_order:
            files += harness("flux_scan_bench.py" if tool == "F" else "jellyfin_scan_bench.py",
                             "--library", str(LIB_TINY), *scan_args)
    return files


# ── summary ─────────────────────────────────────────────────────────────────

def vals(runs: list[dict], f) -> list:
    out = []
    for r in runs:
        try:
            v = f(r)
        except (KeyError, TypeError, IndexError, AttributeError):
            v = None
        if isinstance(v, list):
            out += [x for x in v if isinstance(x, (int, float)) and not isinstance(x, bool)]
        elif isinstance(v, (int, float)) and not isinstance(v, bool):
            out.append(v)
    return out


def flags(runs: list[dict]) -> dict:
    """Load and gate facts of a group of runs: the load range over every
    phase start/end, QUIET waits, and phases flagged by the gate."""
    def gates(r):
        out = [r.get("gate") or {}, (r.get("first_start") or {}).get("gate") or {}]
        out += [x.get("gate") or {} for x in r.get("restarts") or []]
        for ph in (r.get("phases") or {}).values():
            out += [x.get("gate") or {} for x in (ph if isinstance(ph, list) else [ph])]
        out += [x.get("gate") or {} for x in (r.get("files") or {}).values()]
        out.append((r.get("warmup") or {}).get("gate") or {})
        uniq = {json.dumps(g, sort_keys=True): g for g in out if g}  # a gate stored twice counts once
        return list(uniq.values())
    loads = vals(runs, lambda r: [g.get("load1_at_start") for g in gates(r)] + [r.get("load1_end")])
    return {
        "load1_range": [min(loads), max(loads)] if loads else None,
        "quiet_wait_s": sum(vals(runs, lambda r: [g.get("quiet_wait_s") for g in gates(r)])),
        "load_wait_s": sum(vals(runs, lambda r: [g.get("load_wait_s") for g in gates(r)])),
        "flagged": [(r.get("_file"), g.get("label")) for r in runs for g in gates(r) if g.get("load_flagged")],
    }


def scan_summary(runs: list[dict]) -> dict:
    def ph(name, key="wall_s"):
        return lambda r: [x.get(key) for x in r["phases"][name]]

    def phase_list(name):
        out = []
        for r in runs:
            v = (r.get("phases") or {}).get(name)
            out += v if isinstance(v, list) else [v] if v else []
        return out

    def writes(name):
        xs = phase_list(name)
        dev = [((x.get("writes") or {}).get("device") or {}).get("after_sync", {}).get("write_mb") for x in xs]
        idle = [((x.get("writes") or {}).get("device") or {}).get("idle_2s_before", {}).get("write_mb") for x in xs]
        return {
            "db_and_wal_unchanged": [x.get("writes", {}).get("db_and_wal_unchanged") for x in xs],
            "state_files_changed": [len(x.get("writes", {}).get("state_files_changed") or []) for x in xs],
            "device_written_mb_after_sync": B.summary(dev),
            "device_idle_2s_written_mb": B.summary(idle),
            "rusage_written_mb_lower_bound": B.summary([x.get("usage", {}).get("disk_written_mb_lower_bound")
                                                       for x in xs]),
            "media_files_opened": [x.get("scan_counters", {}).get("media_files_opened") for x in xs],
            "writer_commands": [x.get("scan_counters", {}).get("writer_commands") for x in xs],
        }

    return {
        "runs": len(runs),
        "files": [r.get("_file") for r in runs],
        "gate": flags(runs),
        "first_start_to_public_s": B.summary(vals(runs, lambda r: r["first_start"]["startup_to_public_s"])),
        "first_start_to_ready_s": B.summary(vals(runs, lambda r: r["first_start"]["startup_to_ready_s"])),
        "restart_to_public_s": B.summary(vals(runs, lambda r: [x["startup_to_public_s"] for x in r["restarts"]])),
        "restart_to_ready_s": B.summary(vals(runs, lambda r: [x["startup_to_ready_s"] for x in r["restarts"]])),
        "restart_rss_3s_mb": B.summary(vals(runs, lambda r: [x.get("rss_3s_mb") for x in r["restarts"]])),
        "idle_rss_after_start_mb": B.summary(vals(runs, lambda r: r["idle_2s"]["rss_mb"])),
        "idle_threads_after_start": B.summary(vals(runs, lambda r: r["idle_2s"]["threads"])),
        "cold_scan_wall_s": B.summary(vals(runs, lambda r: r["phases"]["cold_scan"]["wall_s"])),
        "cold_scan_server_s": B.summary(vals(runs, lambda r: r["phases"]["cold_scan"]["server_s"])),
        "cold_scan_quiescent_s": B.summary(vals(runs, lambda r: r["phases"]["cold_scan"]["quiescent_s"])),
        "cold_scan_first_items_s": B.summary(vals(runs, lambda r: r["phases"]["cold_scan"]["first_items_s"])),
        "cold_scan_peak_rss_mb_sampled": B.summary(vals(runs, lambda r: r["phases"]["cold_scan"]["peak"]["rss_mb"])),
        "cold_scan_peak_threads": B.summary(vals(runs, lambda r: r["phases"]["cold_scan"]["peak"]["threads"])),
        "cold_scan_peak_children": B.summary(vals(runs, lambda r: r["phases"]["cold_scan"]["peak"]["children"])),
        "cold_scan_cpu_total_s": B.summary(vals(runs, lambda r: r["phases"]["cold_scan"]["usage"]["cpu_total_s"])),
        "cold_scan_cpu_total_until_quiescent_s": B.summary(vals(runs, lambda r: r["phases"]["cold_scan"][
            "usage_until_quiescent"]["cpu_total_s"])),
        "cold_scan_disk_read_mb_until_quiescent": B.summary(vals(runs, lambda r: r["phases"]["cold_scan"][
            "usage_until_quiescent"]["disk_read_mb"])),
        "cold_scan_writes": writes("cold_scan"),
        "cold_scan_device_written_mb_after_sync": B.summary(vals(runs, lambda r: r["phases"]["cold_scan"]["writes"][
            "device"]["after_sync"]["write_mb"])),
        "cold_scan_counts": [r["phases"]["cold_scan"]["counts"] for r in runs if "phases" in r],
        "unchanged_rescan_wall_s": B.summary(vals(runs, ph("unchanged_rescan"))),
        "unchanged_rescan_server_s": B.summary(vals(runs, ph("unchanged_rescan", "server_s"))),
        "unchanged_rescan_quiescent_s": B.summary(vals(runs, ph("unchanged_rescan", "quiescent_s"))),
        "unchanged_rescan_cpu_s": B.summary(vals(runs, lambda r: [x["usage"]["cpu_total_s"]
                                                                 for x in r["phases"]["unchanged_rescan"]])),
        "unchanged_rescan_writes": writes("unchanged_rescan"),
        "add_rescan_wall_s": B.summary(vals(runs, ph("add_rescan", "wall_until_count_s"))),
        "add_rescan_server_s": B.summary(vals(runs, ph("add_rescan", "server_s"))),
        "add_rescan_quiescent_s": B.summary(vals(runs, ph("add_rescan", "quiescent_s"))),
        "add_rescan_count_ok": [x.get("expected_ok") for x in phase_list("add_rescan")],
        "add_rescan_writes": writes("add_rescan"),
        "touch_rescan_wall_s": B.summary(vals(runs, ph("touch_rescan"))),
        "touch_rescan_server_s": B.summary(vals(runs, ph("touch_rescan", "server_s"))),
        "touch_rescan_quiescent_s": B.summary(vals(runs, ph("touch_rescan", "quiescent_s"))),
        "touch_rescan_writes": writes("touch_rescan"),
        "idle_rss_after_scans_mb": B.summary(vals(runs, lambda r: r["idle_after_scans"]["rss_mb"])),
        "idle_footprint_after_scans_mb": B.summary(vals(runs, lambda r: r["idle_after_scans"]["footprint_mb"])),
        "idle_threads_after_scans": B.summary(vals(runs, lambda r: r["idle_after_scans"]["threads"])),
        "lifetime_peak_rss_mb": B.summary(vals(runs, lambda r: r["stop"]["lifetime_peak_rss_mb_incl_children"])),
        "lifetime_cpu_s_incl_children": B.summary(vals(runs, lambda r: r["stop"]["lifetime_cpu_s_incl_children"])),
        "db_bytes": B.summary(vals(runs, lambda r: r["db"]["db_bytes"])),
        "db_wal_bytes": B.summary(vals(runs, lambda r: r["db"]["wal_bytes"])),
        "state_bytes": B.summary(vals(runs, lambda r: r["state_bytes"])),
        "extraction": [(r.get("evidence") or {}).get("extraction", {}).get("no_extraction") for r in runs
                       if (r.get("evidence") or {}).get("extraction")],
    }


B_SEEKS = [600, 3600, 5100, 30]


def play_summary(runs: list[dict]) -> dict:
    files: dict = {}
    names = list(dict.fromkeys(n for r in runs for n in r["files"]))
    pooled_seeks = []
    for n in names:
        es = [r["files"][n] for r in runs if n in r["files"] and "ttfs_s" in r["files"][n]]
        if not es:
            files[n] = {"note": next((r["files"][n].get("note") or r["files"][n].get("error")
                                      for r in runs if n in r["files"]), None)}
            continue
        seeks = {}
        seek_errors = {}
        for t in B_SEEKS:
            lat = [s["latency_s"] for e in es for s in e["seeks"] if s["target_s"] == t and "latency_s" in s]
            errs = [s["error"] for e in es for s in e["seeks"] if s["target_s"] == t and "error" in s]
            if lat:
                seeks[str(t)] = B.summary(lat)
                pooled_seeks += lat
            if errs:
                seek_errors[str(t)] = errs
        first = es[0]
        files[n] = {
            "runs": len(es),
            "positions": sorted(e.get("position") for e in es if e.get("position") is not None),
            "decision": first.get("decision"),
            "ffmpeg": {k: (first.get("ffmpeg") or {}).get(k) for k in ("video", "audio")} if first.get("ffmpeg") else None,
            "ffmpeg_starts": B.summary([(e.get("ffmpeg") or {}).get("ffmpeg_starts") for e in es]) if first.get("ffmpeg") else None,
            "playbackinfo_s": B.summary([e["playbackinfo_s"] for e in es]),
            "ttfs_video_s": B.summary([e["ttfs_video_s"] for e in es]),
            "ttfs_s": B.summary([e["ttfs_s"] for e in es]),
            "play_plus_ttfs_s": B.summary([e["play_plus_ttfs_s"] for e in es]),
            "seg0_bytes": first.get("seg0_bytes"),
            "segments": first.get("segments"),
            "seeks_s": seeks,
            "seek_errors": seek_errors,
        }
    warm = [r["warmup"] for r in runs if r.get("warmup") and "ttfs_s" in r["warmup"]]
    first_measured = [e for r in runs for e in r["files"].values() if e.get("position") == 0 and "ttfs_s" in e]
    return {
        "runs": len(runs),
        "files_used": [r.get("_file") for r in runs],
        "rotations": sorted(r.get("rotate") for r in runs if r.get("rotate") is not None),
        "gate": flags(runs),
        "per_file": files,
        "first_play": {
            "note": "the throwaway warm-up play right after the server started (not in per_file or the ratios)",
            "file": next((r.get("warmup_file") for r in runs if r.get("warmup_file")), None),
            "playbackinfo_s": B.summary([w["playbackinfo_s"] for w in warm]),
            "ttfs_s": B.summary([w["ttfs_s"] for w in warm]),
            "play_plus_ttfs_s": B.summary([w["play_plus_ttfs_s"] for w in warm]),
        },
        "first_measured_play_ttfs_s": B.summary([e["ttfs_s"] for e in first_measured]),
        "pooled_seek_s": B.summary(pooled_seeks),
        "per_file_ttfs_medians_s": B.summary([files[n]["ttfs_s"]["median"] for n in files if "ttfs_s" in files[n]]),
        "first_start_to_ready_s": B.summary(vals(runs, lambda r: r["first_start"]["startup_to_ready_s"])),
        "idle_rss_after_start_mb": B.summary(vals(runs, lambda r: r["idle_after_start"]["rss_mb"])),
        "rss_after_playback_mb": B.summary(vals(runs, lambda r: r["after_playback"]["rss_mb"])),
        "threads_after_playback": B.summary(vals(runs, lambda r: r["after_playback"]["threads"])),
        "playback_peak_rss_mb_sampled": B.summary(vals(runs, lambda r: r["playback_peak"]["rss_mb"])),
        "lifetime_peak_rss_mb_incl_children": B.summary(vals(runs, lambda r: r["stop"]["lifetime_peak_rss_mb_incl_children"])),
        "playback_cpu_total_s": B.summary(vals(runs, lambda r: r["playback_usage"]["cpu_total_s"])),
        "extraction": [(r.get("evidence") or {}).get("extraction", {}).get("no_extraction") for r in runs
                       if (r.get("evidence") or {}).get("extraction")],
    }


# <prefix>-(scan|playback)-<tool>[-<corpus>]-<stamp>.json, for any prefix ending in `bench`
# (flux-bench, flux-e-bench, …).
NAME_RE = re.compile(r"^[\w-]*?bench-(scan|playback)-(flux|jellyfin)-?(.*?)-\d{8}-\d{6}[a-z]?\.json$")


def load_inputs(paths: list[Path]) -> tuple[dict[str, list[dict]], list[str], list[str]]:
    """Groups result files by (kind, tool, corpus). Each input is read once:
    the same path twice, or the same run under another name (same tool,
    start time and corpus), counts once."""
    groups: dict[str, list[dict]] = {}
    used: list[str] = []
    dropped: list[str] = []
    seen_runs: set = set()
    for p in dict.fromkeys(Path(p).resolve() for p in paths):
        m = NAME_RE.match(p.name)
        if not m:
            dropped.append(f"{p.name} (not a result file)")
            continue
        r = json.loads(p.read_text())
        ident = (r.get("tool"), r.get("started"), r.get("library") or r.get("lib_dir"))
        if ident in seen_runs:
            dropped.append(f"{p.name} (duplicate of a run already read)")
            continue
        seen_runs.add(ident)
        r["_file"] = p.name
        kind, tool, rest = m.groups()
        key = f"{kind}:{tool}:{rest or 'features'}" if kind == "scan" else f"{kind}:{tool}"
        groups.setdefault(key, []).append(r)
        used.append(p.name)
    return groups, sorted(used), dropped


def summarize(paths: list[Path], foot: dict | None) -> dict:
    groups, used, dropped = load_inputs(paths)
    out: dict = {"generated": B.now_utc(), "machine": B.machine(), "inputs": used, "inputs_dropped": dropped,
                 "statistics": "n, median (statistics.median), min-max range, nearest-rank p95; "
                               "ratios are Jellyfin median / flux median",
                 "footprint": foot, "scan": {}, "playback": {}}
    for key, runs in sorted(groups.items()):
        kind, tool, *rest = key.split(":")
        if kind == "scan":
            out["scan"].setdefault(rest[0], {})[tool] = scan_summary(runs)
        else:
            out["playback"][tool] = play_summary(runs)
    # Ratios (Jellyfin / flux) of the medians.
    rat: dict = {}
    for corpus, by in out["scan"].items():
        if "flux" in by and "jellyfin" in by:
            f, j = by["flux"], by["jellyfin"]
            rat[corpus] = {k: B.ratio(j[k]["median"], f[k]["median"]) for k in (
                "first_start_to_ready_s", "restart_to_public_s", "restart_to_ready_s", "cold_scan_wall_s",
                "cold_scan_quiescent_s", "cold_scan_cpu_total_until_quiescent_s",
                "cold_scan_first_items_s", "unchanged_rescan_wall_s", "unchanged_rescan_server_s",
                "add_rescan_wall_s", "touch_rescan_wall_s", "cold_scan_peak_rss_mb_sampled", "lifetime_peak_rss_mb",
                "idle_rss_after_start_mb", "idle_rss_after_scans_mb", "cold_scan_cpu_total_s",
                "cold_scan_device_written_mb_after_sync", "db_bytes", "state_bytes")}
    if "flux" in out["playback"] and "jellyfin" in out["playback"]:
        fp, jp = out["playback"]["flux"], out["playback"]["jellyfin"]
        f, j = fp["per_file"], jp["per_file"]
        rat["playback"] = {n: {k: B.ratio(j[n][k]["median"], f[n][k]["median"]) for k in ("ttfs_s", "play_plus_ttfs_s")}
                           for n in f if n in j and "ttfs_s" in f[n] and "ttfs_s" in j[n]}
        rat["playback"]["pooled_seek_median"] = B.ratio(jp["pooled_seek_s"]["median"], fp["pooled_seek_s"]["median"])
        rat["playback"]["first_play_ttfs_median"] = B.ratio(jp["first_play"]["ttfs_s"]["median"],
                                                           fp["first_play"]["ttfs_s"]["median"])
    out["ratios_jellyfin_over_flux"] = rat
    return out


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cmd", choices=["run", "summarize"])
    ap.add_argument("--skip", default="", help="comma list of full,play,tiny")
    ap.add_argument("--full-order", default="FJF", help="full-library runs, F = flux, J = Jellyfin")
    ap.add_argument("--play-runs", type=int, default=len(DEFAULT_FILES), help="playback runs per server")
    ap.add_argument("--tiny-order", default="FJJFFJ", help="library-tiny runs, F = flux, J = Jellyfin")
    ap.add_argument("--rescans", type=int, help="passed to the scan harnesses")
    ap.add_argument("--restarts", type=int, help="passed to the scan harnesses")
    ap.add_argument("--since", help="summarize: result files in the results dir stamped at or after YYYYMMDD-HHMM")
    ap.add_argument("--files", nargs="*", help="summarize: explicit result files")
    ap.add_argument("--out", default=str(B.RESULTS))
    args = ap.parse_args()
    if args.cmd == "run":
        scan_args = []
        if args.rescans is not None:
            scan_args += ["--rescans", str(args.rescans)]
        if args.restarts is not None:
            scan_args += ["--restarts", str(args.restarts)]
        t0 = time.monotonic()
        written = [Path(p) for p in run(set(filter(None, args.skip.split(","))), full_order=args.full_order,
                                        play_runs=args.play_runs, tiny_order=args.tiny_order,
                                        scan_args=scan_args)]
        print(f"[h2h] all runs done in {time.monotonic() - t0:.0f} s", flush=True)
    elif args.files:
        written = [Path(p) for p in args.files]
    else:
        since = args.since or "0"
        written = [p for p in sorted(B.RESULTS.glob(f"{B.RESULT_PREFIX}-*-2*.json"))
                   if not p.name.startswith(f"{B.RESULT_PREFIX}-summary")
                   and (m := B.RESULT_STAMP_RE.search(p.name)) and m.group(1) >= since]
    summary = summarize(written, footprint())
    B.write_result(f"{B.RESULT_PREFIX}-summary", summary, Path(args.out))
    print(json.dumps(summary["ratios_jellyfin_over_flux"], indent=1))
    if summary["inputs_dropped"]:
        print("dropped:", summary["inputs_dropped"])


if __name__ == "__main__":
    main()
