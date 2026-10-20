#!/usr/bin/env python3
"""Benchmark flux-server's library scan through its HTTP API — the mirror of
jellyfin_scan_bench.py (same protocol module, same corpus clone, same phases).

Starts a fresh, isolated flux-server (`nice -n 19`, 127.0.0.1 only, tailnet
off, background jobs off: no matching and no frame posters, as Jellyfin runs
with metadata and image fetchers off; filesystem watcher off, as Jellyfin's
real-time monitor is off), creates Movies (movies) and TV (shows) libraries
over `POST /api/v1/libraries` on a fresh APFS clone of the corpus, and times
the phases in scan_protocol.py. Scan completion is read from
`GET /api/v1/libraries` (every library not scanning and finished after the
request).

What each scan job did comes from flux-scan's own counters: the server runs
with `--log info,flux_scan::engine=debug`, which logs one `scan finished`
line per job with its ScanSummary (media_files_opened, writer_commands, …);
the harness reads the lines each phase added to server.log.

Usage:
  tools/bench/flux_scan_bench.py --library "$FLUX_TEST_MEDIA/library-tiny" --runs 3

The server binary is $FLUX_SERVER, else $CARGO_TARGET_DIR/release/flux-server,
else target/release/flux-server in the repo (or pass --server-bin).
"""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import benchlib as B  # noqa: E402
import scan_protocol  # noqa: E402

DEFAULT_BIN = Path(os.environ.get("FLUX_SERVER")
                   or Path(os.environ.get("CARGO_TARGET_DIR") or B.REPO / "target") / "release/flux-server")
LOG_FILTER = "info,flux_scan::engine=debug"  # adds one `scan finished` line (ScanSummary) per scan job
_WARMED: set[str] = set()
SUMMARY_KEYS = ("library_id", "files_seen", "media_files", "unchanged", "added", "changed", "moved", "missing",
                "media_files_opened", "writer_commands", "fingerprints_deferred", "probe_failures",
                "ffprobe_fallbacks", "total_ms")


class Flux:
    def __init__(self, base: str):
        self.base = base.rstrip("/")

    def req(self, method: str, path: str, body=None, timeout=60):
        data = json.dumps(body).encode() if body is not None else None
        # flux refuses unsafe /api/ calls without X-Flux-Request: 1 (403,
        # the cross-site guard; flux_core::api::REQUEST_HEADER).
        headers = {"X-Flux-Request": "1"}
        if data:
            headers["Content-Type"] = "application/json"
        r = urllib.request.Request(self.base + path, data=data, method=method, headers=headers)
        with urllib.request.urlopen(r, timeout=timeout) as resp:
            raw = resp.read()
            return json.loads(raw) if raw and resp.headers.get_content_type() == "application/json" else None


def parse_scan_finished(text: str) -> list[dict]:
    """The ScanSummary of every `scan finished` line in a flux-server log chunk."""
    out = []
    for line in text.splitlines():
        if "scan finished" not in line or "ScanSummary {" not in line:
            continue
        body = line.split("ScanSummary {", 1)[1]
        nums = dict(re.findall(r"\b(\w+): (\d+)(?=[,\s}])", body))
        out.append({k: int(nums[k]) for k in SUMMARY_KEYS if k in nums})
    return out


class FluxAdapter(scan_protocol.Adapter):
    tool = "flux"

    def __init__(self, bin_path: Path, state: Path, port: int, watch: bool = False):
        self.bin = bin_path
        self.state = state
        self.port = port
        self.watch = watch
        self.api = Flux(f"http://127.0.0.1:{port}")
        self.libs: dict[str, int] = {}
        self.proc: subprocess.Popen | None = None

    @property
    def log_path(self) -> Path:
        return self.state / "server.log"

    def _spawn(self, append: bool) -> float:
        if str(self.bin) not in _WARMED:
            # A newly built binary's first exec is slow on macOS (code-signature
            # assessment, ~1 s); do it once outside any measurement.
            subprocess.run([str(self.bin), "--help"], capture_output=True)
            _WARMED.add(str(self.bin))
        B.wait_port_free(self.port)
        log = open(self.log_path, "a" if append else "w")
        args = [str(self.bin), "--data-dir", str(self.state / "data"), "--port", str(self.port),
                "--bind", "127.0.0.1", "--tailnet", "off", "--no-jobs", "--log", LOG_FILTER]
        if not self.watch:
            args.append("--no-watch")
        t0 = time.monotonic()
        # cwd = the state dir: no ./web/dist there, so no web UI is served
        # (Jellyfin runs with --nowebclient).
        self.proc = subprocess.Popen(["nice", "-n", "19", *args], cwd=self.state, stdout=log,
                                     stderr=subprocess.STDOUT, start_new_session=True,
                                     env={**os.environ, "NO_COLOR": "1"})
        log.close()
        self.pid = self.proc.pid
        return t0

    def _poll(self, fn, t0: float, timeout_s: float = 120.0, every_s: float = 0.002) -> float:
        while time.monotonic() - t0 < timeout_s:
            try:
                if fn():
                    return round(time.monotonic() - t0, 4)
            except (urllib.error.URLError, ConnectionError, TimeoutError, OSError):
                pass
            if self.proc.poll() is not None:
                raise RuntimeError("flux-server exited early")
            time.sleep(every_s)
        raise RuntimeError("flux-server did not start")

    def start(self) -> dict:
        if self.state.exists():
            shutil.rmtree(self.state)
        self.state.mkdir(parents=True)
        t0 = self._spawn(append=False)
        info: dict = {}
        ready = self._poll(lambda: info.update(self.api.req("GET", "/api/v1/server", timeout=2) or {}) or True, t0)
        out = {"startup_to_public_s": ready, "startup_to_ready_s": ready,
               "ready_definition": "first 200 from GET /api/v1/server (empty state: no libraries to list)",
               "version": info.get("version"), "server_bin": str(self.bin),
               "server_bin_bytes": self.bin.stat().st_size, "watch": self.watch, "log_filter": LOG_FILTER}
        out["listening"] = B.assert_loopback(self.pid, self.stop, "flux")
        return out

    def restart(self) -> dict:
        t0 = self._spawn(append=True)
        public = self._poll(lambda: self.api.req("GET", "/api/v1/server", timeout=2) is not None, t0)
        want = set(self.libs.values())
        ready = self._poll(lambda: want <= {x["id"] for x in self.libraries()}, t0)
        return {"startup_to_public_s": public, "startup_to_ready_s": ready,
                "ready_definition": "GET /api/v1/libraries lists every library",
                "listening": B.assert_loopback(self.pid, self.stop, "flux")}

    def prepare(self, movies: Path, tv: Path) -> None:
        pass  # creating a library starts its scan, so that happens in begin_scan(cold=True)

    def libraries(self) -> list[dict]:
        return self.api.req("GET", "/api/v1/libraries") or []

    def begin_scan(self, cold: bool, movies: Path, tv: Path):
        t_ms = time.time() * 1000
        if cold:
            for name, kind, path in (("Movies", "movies", movies), ("TV", "shows", tv)):
                lib = self.api.req("POST", "/api/v1/libraries", {"name": name, "kind": kind, "roots": [str(path)]})
                self.libs[name] = lib["id"]
        else:
            for lib_id in self.libs.values():
                self.api.req("POST", f"/api/v1/libraries/{lib_id}/scan")
        return t_ms

    def poll_done(self, t_ms):
        libs = [lib for lib in self.libraries() if lib["id"] in self.libs.values()]
        if len(libs) < len(self.libs):
            return None
        for lib in libs:
            s = lib["scan"]
            # Our request's scan (or a later one) has started and nothing is queued or running.
            if s["state"] == "scanning" or s["finished_at"] is None or (s["started_at"] or 0) < t_ms - 2:
                return None
        started = min(lib["scan"]["started_at"] for lib in libs)
        finished = max(lib["scan"]["finished_at"] for lib in libs)
        return {"server_s": round((finished - started) / 1000, 3),
                "libraries": {lib["name"]: {k: lib["scan"][k] for k in (
                    "state", "files_seen", "files_total", "files_probed", "items_added", "items_updated",
                    "items_removed", "started_at", "finished_at", "last_error", "dirs_unavailable")}
                    | {"item_count": lib["item_count"]} for lib in libs}}

    def scan_mark(self):
        try:
            return self.log_path.stat().st_size
        except FileNotFoundError:
            return 0

    def scan_counters(self, mark) -> dict:
        """Sums the ScanSummary of every job that finished since `mark`. The
        line is logged just after the job's status turns to finished, so wait
        (up to 10 s) for one line per library."""
        jobs: list[dict] = []
        t0 = time.monotonic()
        while time.monotonic() - t0 < 10:
            with open(self.log_path, "rb") as fh:
                fh.seek(mark or 0)
                jobs = parse_scan_finished(fh.read().decode(errors="replace"))
            if len(jobs) >= len(self.libs):
                break
            time.sleep(0.05)
        out = {"available": True, "source": f"server.log `scan finished` lines ({LOG_FILTER})",
               "jobs": len(jobs), "per_job": jobs}
        for k in ("media_files_opened", "writer_commands", "added", "changed", "moved", "missing", "unchanged"):
            out[k] = sum(j.get(k, 0) for j in jobs) if jobs else None
        return out

    def any_items(self) -> bool:
        return any(lib["item_count"] > 0 for lib in self.libraries())

    def counts(self) -> dict:
        return {lib["name"]: lib["item_count"] for lib in self.libraries()}

    def movie_count(self, counts: dict) -> int:
        return counts.get("Movies", 0)

    def db_path(self) -> Path | None:
        return self.state / "data" / "catalog.db"

    def stop(self) -> dict:
        proc, self.proc = self.proc, None
        return B.stop_process(proc) if proc else {}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--library", required=True, help="dir containing Movies/ and TV/ (only read; a clone is scanned)")
    ap.add_argument("--server-bin", default=str(DEFAULT_BIN))
    ap.add_argument("--state", default=str(B.SCRATCH / "flux-scan-state"))
    ap.add_argument("--clone", default=str(B.SCRATCH / "scan-clone"))
    ap.add_argument("--port", type=int, default=B.FLUX_PORT)
    ap.add_argument("--runs", type=int, default=1)
    ap.add_argument("--rescans", type=int, default=3)
    ap.add_argument("--add-files", type=int, default=50)
    ap.add_argument("--touch-files", type=int, default=50)
    ap.add_argument("--restarts", type=int, default=3, help="restarts on the populated state after the scans")
    ap.add_argument("--watch", action="store_true", help="leave flux's filesystem watcher on (default: off, "
                                                         "like Jellyfin's real-time monitor)")
    ap.add_argument("--tag", default="", help="suffix for the result file name")
    ap.add_argument("--out", default=str(B.RESULTS))
    args = ap.parse_args()

    lib = Path(args.library)
    for _ in range(args.runs):
        ad = FluxAdapter(Path(args.server_bin), Path(args.state), args.port, watch=args.watch)
        r = scan_protocol.run(ad, library=lib, clone=Path(args.clone) / lib.name, rescans=args.rescans,
                              add_files=args.add_files, touch_files=args.touch_files, restarts=args.restarts)
        B.write_result(f"{B.RESULT_PREFIX}-scan-flux-{lib.name}{args.tag}", r, Path(args.out))
        print(json.dumps(r["summary"], indent=1), flush=True)


if __name__ == "__main__":
    main()
