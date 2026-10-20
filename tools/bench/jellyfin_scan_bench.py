#!/usr/bin/env python3
"""Benchmark Jellyfin's library scan on a given corpus — the baseline flux must beat.

Starts a fresh, isolated Jellyfin (all state on the given state dir, `nice -n 19`)
on 127.0.0.1 only: config/network.xml is written before the first start (own
port, LocalNetworkAddresses 127.0.0.1, LocalNetworkSubnets 127.0.0.1/32,
auto-discovery, UPnP and remote access off), and lsof must show nothing but
127.0.0.1 after every start. The setup wizard runs over the API with a random
admin password (never a fixed one). Movies/ and TV/ libraries are added with
every network and image/analysis feature disabled, for every item type the
scan creates (Movie, Video, Trailer / Series, Season, Episode, Video: extras
and stacked parts are plain `Video` items, which Jellyfin would otherwise
frame-grab posters for), and every scheduled-task trigger is cleared (chapter
images, trickplay, …; library scans are requested explicitly). At the end the
run checks that no image extraction happened: generated images in
jellyfin.db by item type, chapter images, trickplay and keyframe rows, the
files under data/metadata, and which scheduled tasks ran.

Then it runs the phases in scan_protocol.py — the same module
flux_scan_bench.py uses: first start, cold scan, unchanged rescans, rescans
after adding N new movie folders, rescans after touching N episodes, idle
footprint, restarts on the populated state.

The corpus is only read: Jellyfin scans a fresh APFS clone under the scratch
dir. Results are written as JSON.

Usage:
  tools/bench/jellyfin_scan_bench.py --library "$FLUX_TEST_MEDIA/library"
"""
from __future__ import annotations

import argparse
import json
import re
import secrets
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import benchlib as B  # noqa: E402
import scan_protocol  # noqa: E402

WORK = B.WORK
DEFAULT_JELLYFIN = str(WORK / "tools/jellyfin/jellyfin/jellyfin")
AUTH_BASE = 'MediaBrowser Client="flux-bench", Device="bench", DeviceId="flux-bench-1", Version="1.0"'
MOVIE_TYPES = ["Movie", "Video", "Trailer"]
TV_TYPES = ["Series", "Season", "Episode", "Video"]
IMAGE_EXTS = {".jpg", ".jpeg", ".png", ".webp", ".bmp", ".gif"}


class Jellyfin:
    def __init__(self, base: str):
        self.base = base.rstrip("/")
        self.token: str | None = None

    def req(self, method: str, path: str, body=None, query: dict | None = None, timeout=60):
        url = self.base + path
        if query:
            url += "?" + urllib.parse.urlencode(query, doseq=True)
        data = json.dumps(body).encode() if body is not None else None
        auth = AUTH_BASE + (f', Token="{self.token}"' if self.token else "")
        r = urllib.request.Request(url, data=data, method=method, headers={
            "Content-Type": "application/json", "Authorization": auth})
        with urllib.request.urlopen(r, timeout=timeout) as resp:
            raw = resp.read()
            return json.loads(raw) if raw and resp.headers.get_content_type() == "application/json" else None


def rss_mb(pid: int) -> float:
    s = B.proc_stats(pid)
    return s["rss_mb"] if s else 0.0


def lib_options(path: str, item_types: list[str]) -> dict:
    """Library options with every network, image and analysis feature off.
    TypeOptions must name every item type the scan creates: a type without an
    entry gets Jellyfin's default image fetchers (Screen Grabber for Video)."""
    return {"LibraryOptions": {
        "Enabled": True,
        "EnableRealtimeMonitor": False,
        "EnableInternetProviders": False,
        "EnableChapterImageExtraction": False,
        "ExtractChapterImagesDuringLibraryScan": False,
        "EnableTrickplayImageExtraction": False,
        "ExtractTrickplayImagesDuringLibraryScan": False,
        "EnableLUFSScan": False,
        "SaveLocalMetadata": False,
        "MetadataSavers": [],
        "DisabledMediaSegmentProviders": [],
        "MediaSegmentProviderOrder": [],
        "PathInfos": [{"Path": path}],
        "TypeOptions": [{"Type": t, "MetadataFetchers": [], "MetadataFetcherOrder": [],
                          "ImageFetchers": [], "ImageFetcherOrder": []} for t in item_types],
    }}


def refresh_task(jf: Jellyfin) -> dict | None:
    tasks = jf.req("GET", "/ScheduledTasks", query={"isHidden": "false"}) or []
    return next((t for t in tasks if t.get("Key") == "RefreshLibrary"), None)


def finished_since(task: dict | None, started_utc: datetime) -> dict | None:
    """The RefreshLibrary task's last run if it began after `started_utc` and is over."""
    if not task or task.get("State") != "Idle":
        return None
    last = task.get("LastExecutionResult") or {}
    end = last.get("EndTimeUtc")
    if not end:
        return None
    end_dt = datetime.fromisoformat(end.replace("Z", "+00:00"))
    if end_dt < started_utc:
        return None
    start_dt = datetime.fromisoformat(last["StartTimeUtc"].replace("Z", "+00:00"))
    return {"server_s": round((end_dt - start_dt).total_seconds(), 3), "status": last.get("Status")}


def wait_scan(jf: Jellyfin, pid: int, started_utc: datetime, timeout_s: float) -> dict:
    """Poll the RefreshLibrary task until it has finished a run that began after started_utc."""
    t0 = time.monotonic()
    sampler = B.Sampler(pid)
    try:
        while time.monotonic() - t0 < timeout_s:
            done = finished_since(refresh_task(jf), started_utc)
            if done:
                return {"wall_s": round(time.monotonic() - t0, 3), "task_s": done["server_s"],
                        "status": done["status"], "peak": sampler.peak}
            time.sleep(0.25)
    finally:
        sampler.stop()
    raise TimeoutError("scan did not finish in time")


def _poll(fn, proc: subprocess.Popen, timeout_s: float, every_s: float = 0.01) -> None:
    t0 = time.monotonic()
    while time.monotonic() - t0 < timeout_s:
        try:
            if fn():
                return
        except urllib.error.HTTPError as e:
            if e.code not in (503, 502):
                raise
        except (urllib.error.URLError, ConnectionError, TimeoutError, OSError):
            pass  # not listening yet, or the startup placeholder server is swapped for the real one
        if proc.poll() is not None:
            raise RuntimeError("Jellyfin exited early")
        time.sleep(every_s)
    raise RuntimeError("Jellyfin did not start")


def wait_ready(jf: Jellyfin, proc: subprocess.Popen, t_launch: float) -> dict:
    """First start: public info, then the rest of the API (503 until startup is done)."""
    out: dict = {}
    info: dict = {}
    _poll(lambda: info.update(jf.req("GET", "/System/Info/Public", timeout=2) or {}) or True, proc, 120)
    out["startup_to_public_s"] = round(time.monotonic() - t_launch, 3)
    out["version"] = info.get("Version") or info.get("version")
    _poll(lambda: jf.req("GET", "/Startup/Configuration", timeout=2) or True, proc, 180)
    out["startup_to_ready_s"] = round(time.monotonic() - t_launch, 3)
    out["ready_definition"] = "GET /Startup/Configuration stops answering 503 (no user exists yet)"
    return out


def wait_restart(jf: Jellyfin, proc: subprocess.Popen, t_launch: float) -> dict:
    """Restart on a populated state: public info, then an authenticated API call."""
    out: dict = {}
    _poll(lambda: jf.req("GET", "/System/Info/Public", timeout=2) is not None, proc, 120)
    out["startup_to_public_s"] = round(time.monotonic() - t_launch, 3)
    _poll(lambda: jf.req("GET", "/Library/VirtualFolders", timeout=5) is not None, proc, 180)
    out["startup_to_ready_s"] = round(time.monotonic() - t_launch, 3)
    out["ready_definition"] = "authenticated GET /Library/VirtualFolders answers 200"
    return out


def network_xml(port: int) -> str:
    """Loopback only: Kestrel binds 127.0.0.1, no UDP auto-discovery, no UPnP,
    no remote access. Without this file Jellyfin binds 0.0.0.0 (every LAN and
    tailnet address) with remote access on, and always uses port 8096."""
    return f"""<?xml version="1.0" encoding="utf-8"?>
<NetworkConfiguration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <InternalHttpPort>{port}</InternalHttpPort>
  <PublicHttpPort>{port}</PublicHttpPort>
  <EnableHttps>false</EnableHttps>
  <AutoDiscovery>false</AutoDiscovery>
  <EnableUPnP>false</EnableUPnP>
  <EnableIPv4>true</EnableIPv4>
  <EnableIPv6>false</EnableIPv6>
  <EnableRemoteAccess>false</EnableRemoteAccess>
  <LocalNetworkSubnets><string>127.0.0.1/32</string></LocalNetworkSubnets>
  <LocalNetworkAddresses><string>127.0.0.1</string></LocalNetworkAddresses>
  <IgnoreVirtualInterfaces>true</IgnoreVirtualInterfaces>
</NetworkConfiguration>
"""


def launch(jellyfin: str, state: Path, ffmpeg: str, port: int, fresh: bool = True) -> subprocess.Popen:
    """fresh: wipe the state dir and write network.xml before the first start;
    otherwise start on the existing state (a restart)."""
    if fresh:
        if state.exists():
            shutil.rmtree(state)
        for d in ("data", "cache", "config", "log"):
            (state / d).mkdir(parents=True)
    # Rewritten on every start: the port and the loopback binding never come from an earlier run.
    (state / "config" / "network.xml").write_text(network_xml(port))
    log = open(state / "stdout.log", "w" if fresh else "a")
    proc = subprocess.Popen(["nice", "-n", "19", jellyfin, "--datadir", str(state / "data"),
                             "--cachedir", str(state / "cache"), "--configdir", str(state / "config"),
                             "--logdir", str(state / "log"), "--ffmpeg", ffmpeg, "--nowebclient"],
                            cwd=Path(jellyfin).parent, stdout=log, stderr=subprocess.STDOUT,
                            start_new_session=True)
    log.close()
    return proc


def wizard(jf: Jellyfin) -> dict:
    """First-run setup: a random admin password (never a fixed one) and remote access off."""
    pw = secrets.token_urlsafe(18)
    jf.req("POST", "/Startup/Configuration", {"ServerName": "bench", "UICulture": "en-US",
                                              "MetadataCountryCode": "US", "PreferredMetadataLanguage": "en"})
    jf.req("GET", "/Startup/User")
    jf.req("POST", "/Startup/User", {"Name": "bench", "Password": pw})
    jf.req("POST", "/Startup/RemoteAccess", {"EnableRemoteAccess": False, "EnableAutomaticPortMapping": False})
    jf.req("POST", "/Startup/Complete")
    auth = jf.req("POST", "/Users/AuthenticateByName", {"Username": "bench", "Pw": pw})
    jf.token = auth["AccessToken"]
    return auth


def scheduled_tasks(jf: Jellyfin) -> list[dict]:
    return jf.req("GET", "/ScheduledTasks", query={"isHidden": "false"}) or []


def clear_task_triggers(jf: Jellyfin) -> list[dict]:
    """Removes every scheduled-task trigger (daily chapter-image and trickplay
    extraction, startup plugin updates, interval library scans, …). The
    benchmark requests its scans explicitly; nothing else may run."""
    out = []
    for t in scheduled_tasks(jf):
        trig = t.get("Triggers") or []
        if trig:
            jf.req("POST", f"/ScheduledTasks/{t['Id']}/Triggers", [])
        out.append({"key": t.get("Key"), "name": t.get("Name"),
                    "triggers_cleared": [x.get("Type") for x in trig]})
    left = {t.get("Key"): len(t.get("Triggers") or []) for t in scheduled_tasks(jf)}
    for x in out:
        x["triggers_left"] = left.get(x["key"])
    return out


def task_runs(jf: Jellyfin) -> list[dict]:
    """Last execution of every scheduled task (what ran, and when)."""
    out = []
    for t in scheduled_tasks(jf):
        last = t.get("LastExecutionResult") or {}
        out.append({"key": t.get("Key"), "name": t.get("Name"), "state": t.get("State"),
                    "last_start_utc": last.get("StartTimeUtc"), "last_status": last.get("Status")})
    return out


def extraction_evidence(state: Path) -> dict:
    """After Jellyfin has stopped: did it extract any image or analysis data?

    Reads a clone of jellyfin.db (+ WAL; `cp -c`, the original is untouched)
    and lists the image files under data/metadata. Images of the root folders
    are collages Jellyfin draws from its children (no extraction); every other
    generated image would be a frame grab or an embedded-image extraction."""
    out: dict = {}
    meta = state / "data" / "metadata"
    files = [p for p in meta.rglob("*") if p.is_file() and p.suffix.lower() in IMAGE_EXTS] if meta.exists() else []
    out["metadata_image_files"] = len(files)
    out["metadata_image_examples"] = [str(p.relative_to(state)) for p in files[:10]]
    db = state / "data" / "data" / "jellyfin.db"
    if not db.exists():
        out["db"] = "jellyfin.db not found"
        return out
    B.SCRATCH.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=B.SCRATCH) as td:
        for sfx in ("", "-wal"):
            src = Path(str(db) + sfx)
            if src.exists():
                subprocess.run(["cp", "-c", str(src), str(Path(td) / ("jellyfin.db" + sfx))], check=True)
        c = sqlite3.connect(str(Path(td) / "jellyfin.db"))
        try:
            q = lambda sql: c.execute(sql).fetchall()  # noqa: E731
            gen = q("SELECT b.Type, count(*) FROM BaseItemImageInfos i JOIN BaseItems b ON b.Id = i.ItemId "
                    "WHERE i.Path LIKE '%/metadata/%' GROUP BY b.Type")
            out["generated_images_by_type"] = {t.rsplit(".", 1)[-1]: n for t, n in gen}
            out["generated_images_non_folder"] = sum(n for t, n in gen if not t.endswith("Folder"))
            for name, sql in (("chapter_images", "SELECT count(*) FROM Chapters WHERE ImagePath IS NOT NULL"),
                              ("chapters", "SELECT count(*) FROM Chapters"),
                              ("trickplay_rows", "SELECT count(*) FROM TrickplayInfos"),
                              ("keyframe_rows", "SELECT count(*) FROM KeyframeData"),
                              ("media_segment_rows", "SELECT count(*) FROM MediaSegments")):
                try:
                    out[name] = q(sql)[0][0]
                except sqlite3.Error as e:
                    out[name] = f"n/a ({e})"
        finally:
            c.close()
    out["no_extraction"] = (out.get("generated_images_non_folder") == 0 and out.get("chapter_images") == 0
                            and out.get("trickplay_rows") == 0)
    return out


EXTRACTION_TASK_HINTS = ("chapter", "trickplay", "keyframe", "segment", "normaliz", "image", "lufs")


def _utc(s: str | None) -> datetime | None:
    """Parses Jellyfin's UTC stamps (`…T03:39:07.5382791Z`: up to 7 fraction digits)."""
    if not s:
        return None
    m = re.match(r"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)(?:\.(\d+))?", s)
    if not m:
        return None
    frac = (m.group(2) or "0")[:6].ljust(6, "0")
    return datetime.fromisoformat(f"{m.group(1)}.{frac}+00:00")


def tasks_ran_since(runs: list[dict], since_utc: str | None) -> list[dict]:
    """Scheduled tasks other than the library scan whose last run started after `since_utc`."""
    since = datetime.fromisoformat(since_utc) if since_utc else None
    out = []
    for t in runs:
        start = _utc(t.get("last_start_utc"))
        if start and t.get("key") != "RefreshLibrary" and (since is None or start >= since):
            hint = f"{t.get('key')} {t.get('name')}".lower()
            out.append({**t, "extraction": any(h in hint for h in EXTRACTION_TASK_HINTS)})
    return out


class JellyfinAdapter(scan_protocol.Adapter):
    tool = "jellyfin"

    def __init__(self, jellyfin: str, state: Path, ffmpeg: str, port: int):
        self.jellyfin = jellyfin
        self.state = state
        self.ffmpeg = ffmpeg
        self.port = port
        self.jf = Jellyfin(f"http://127.0.0.1:{port}")
        self.proc: subprocess.Popen | None = None
        self.since_utc: str | None = None
        self.tasks_after_run: list[dict] = []

    def start(self) -> dict:
        B.wait_port_free(self.port)
        self.since_utc = datetime.now(timezone.utc).isoformat()
        t0 = time.monotonic()
        self.proc = launch(self.jellyfin, self.state, self.ffmpeg, self.port)
        self.pid = self.proc.pid
        out = wait_ready(self.jf, self.proc, t0)
        out["listening"] = B.assert_loopback(self.pid, self.stop, "jellyfin")
        wizard(self.jf)
        out["scheduled_tasks"] = clear_task_triggers(self.jf)
        out["jellyfin_dir"] = str(Path(self.jellyfin).parent)
        return out

    def restart(self) -> dict:
        B.wait_port_free(self.port)
        t0 = time.monotonic()
        self.proc = launch(self.jellyfin, self.state, self.ffmpeg, self.port, fresh=False)
        self.pid = self.proc.pid
        out = wait_restart(self.jf, self.proc, t0)
        out["listening"] = B.assert_loopback(self.pid, self.stop, "jellyfin")
        return out

    def prepare(self, movies: Path, tv: Path) -> None:
        self.jf.req("POST", "/Library/VirtualFolders", lib_options(str(movies), MOVIE_TYPES),
                    query={"name": "Movies", "collectionType": "movies", "refreshLibrary": "false"})
        self.jf.req("POST", "/Library/VirtualFolders", lib_options(str(tv), TV_TYPES),
                    query={"name": "TV", "collectionType": "tvshows", "refreshLibrary": "false"})
        time.sleep(2)  # let any automatic post-add work settle

    def begin_scan(self, cold: bool, movies: Path, tv: Path):
        started = datetime.now(timezone.utc)
        self.jf.req("POST", "/Library/Refresh")
        return started

    def poll_done(self, started):
        return finished_since(refresh_task(self.jf), started)

    def scan_counters(self, mark) -> dict:
        return {"available": False,
                "method": "Jellyfin exposes no per-scan file-open or write counter; the write evidence is "
                          "jellyfin.db/-wal (size, mtime_ns) and the device bytes in `writes`"}

    def any_items(self) -> bool:
        # ItemCount also counts folders; a browsable title is a movie, series or episode.
        c = self.counts()
        return sum(c.get(k) or 0 for k in ("MovieCount", "SeriesCount", "EpisodeCount")) > 0

    def counts(self) -> dict:
        return self.jf.req("GET", "/Items/Counts") or {}

    def movie_count(self, counts: dict) -> int:
        return counts.get("MovieCount") or 0

    def db_path(self) -> Path | None:
        db = self.state / "data" / "data" / "jellyfin.db"
        return db if db.exists() else next(self.state.rglob("jellyfin.db"), None)

    def stop(self) -> dict:
        proc, self.proc = self.proc, None
        if not proc:
            return {}
        try:
            self.tasks_after_run = task_runs(self.jf)
        except (urllib.error.URLError, ConnectionError, TimeoutError, OSError):
            pass
        return B.stop_process(proc)

    def finish_run(self) -> dict:
        ev = extraction_evidence(self.state)
        ev["scheduled_tasks_last_run"] = self.tasks_after_run
        ran = tasks_ran_since(self.tasks_after_run, self.since_utc)
        ev["tasks_run_during_benchmark"] = ran
        ev["extraction_tasks_run"] = [t["key"] for t in ran if t["extraction"]]
        ev["no_extraction"] = bool(ev.get("no_extraction")) and not ev["extraction_tasks_run"]
        print("[jellyfin] extraction check:", json.dumps({k: ev.get(k) for k in (
            "no_extraction", "generated_images_by_type", "chapter_images", "trickplay_rows",
            "metadata_image_files", "extraction_tasks_run")}), flush=True)
        return {"extraction": ev}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--library", required=True, help="dir containing Movies/ and TV/ (only read; a clone is scanned)")
    ap.add_argument("--jellyfin", default=DEFAULT_JELLYFIN)
    ap.add_argument("--state", default=str(B.SCRATCH / "jellyfin-scan-state"))
    ap.add_argument("--clone", default=str(B.SCRATCH / "scan-clone"))
    ap.add_argument("--ffmpeg", default="/opt/homebrew/bin/ffmpeg")
    ap.add_argument("--port", type=int, default=B.JELLYFIN_PORT)
    ap.add_argument("--runs", type=int, default=1)
    ap.add_argument("--rescans", type=int, default=3)
    ap.add_argument("--add-files", type=int, default=50, help="movie folders added before each add rescan")
    ap.add_argument("--touch-files", type=int, default=50, help="episodes touched before each touch rescan")
    ap.add_argument("--restarts", type=int, default=3, help="restarts on the populated state after the scans")
    ap.add_argument("--tag", default="", help="suffix for the result file name")
    ap.add_argument("--out", default=str(B.RESULTS))
    args = ap.parse_args()

    lib = Path(args.library)
    for _ in range(args.runs):
        ad = JellyfinAdapter(args.jellyfin, Path(args.state), args.ffmpeg, args.port)
        r = scan_protocol.run(ad, library=lib, clone=Path(args.clone) / lib.name, rescans=args.rescans,
                              add_files=args.add_files, touch_files=args.touch_files, restarts=args.restarts)
        B.write_result(f"{B.RESULT_PREFIX}-scan-jellyfin-{lib.name}{args.tag}", r, Path(args.out))
        print(json.dumps(r["summary"], indent=1), flush=True)


if __name__ == "__main__":
    main()
