"""Shared helpers for the flux-vs-Jellyfin head-to-head harnesses (macOS).

Both servers are measured with exactly the same code: the same gate, the same
process sampler, the same write probe, the same scratch clones and the same
file mutations. That way a difference in the numbers is a difference between
the servers, not between two harnesses.

House rules baked in (shared machine, see docs/prototype/benchmarks.md):
- `gate()` runs before EVERY measured phase (a start, a scan, a play). It
  waits while $FLUX_WORK/QUIET exists and until the
  1-minute load is below 10. After 30 min of waiting on the load it accepts
  a load below 15 and flags the phase (`load_flagged`). The load at the
  start and the end of each phase is recorded;
- every server and helper process runs at `nice -n 19`;
- media under media/ is only read: mutations happen on APFS clones under the
  scratch dir (`cp -c`, no data copied), which are deleted afterwards;
- servers listen on 127.0.0.1 only; `assert_loopback()` stops a server that
  listens anywhere else;
- result files are timestamped and never overwritten (`write_result`).

Environment: FLUX_WORK (required) is the scratch directory: results go to
$FLUX_WORK/bench/results, scratch clones to $FLUX_WORK/tmp/bench, and a file
named $FLUX_WORK/QUIET pauses every measured phase. FLUX_TEST_MEDIA is the
test corpus (default $FLUX_WORK/media; see CONTRIBUTING.md).

Overrides (for a private scratch area, e.g. a smoke test):
FLUX_BENCH_SCRATCH, FLUX_BENCH_RESULTS, FLUX_BENCH_FLUX_PORT,
FLUX_BENCH_JELLYFIN_PORT, and FLUX_BENCH_PREFIX (result file names start with
it; default `flux-bench`, e.g. `flux-e-bench` for a phase's own set).
"""
from __future__ import annotations

import ctypes
import json
import os
import plistlib
import re
import shutil
import statistics
import subprocess
import sys
import threading
import time
from datetime import datetime, timezone
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]


def _flux_work() -> Path:
    """$FLUX_WORK, the scratch directory. There is no default: the harnesses
    write results, scratch clones and server state under it."""
    v = os.environ.get("FLUX_WORK")
    if v:
        return Path(v)
    if any(a in ("-h", "--help") for a in sys.argv[1:]):
        return Path("$FLUX_WORK")  # only so that --help can print the defaults
    sys.exit("FLUX_WORK is not set. Point it at a scratch directory (results go to "
             "$FLUX_WORK/bench/results, scratch clones to $FLUX_WORK/tmp); see CONTRIBUTING.md.")


WORK = _flux_work()
# The test corpus: features/, library/, library-tiny/, … (see CONTRIBUTING.md).
MEDIA = Path(os.environ.get("FLUX_TEST_MEDIA") or WORK / "media")
QUIET = WORK / "QUIET"
RESULTS = Path(os.environ.get("FLUX_BENCH_RESULTS") or WORK / "bench/results")
SCRATCH = Path(os.environ.get("FLUX_BENCH_SCRATCH") or WORK / "tmp/bench")
FLUX_PORT = int(os.environ.get("FLUX_BENCH_FLUX_PORT") or 18596)
JELLYFIN_PORT = int(os.environ.get("FLUX_BENCH_JELLYFIN_PORT") or 18696)
RESULT_PREFIX = os.environ.get("FLUX_BENCH_PREFIX") or "flux-bench"
MEDIA_EXTS = {".mkv", ".mp4", ".m4v", ".avi", ".ts", ".m2ts", ".mov", ".webm", ".wmv", ".mpg", ".flv"}

LOAD_OK = 10.0  # a phase starts when load1 < LOAD_OK ...
LOAD_FALLBACK = 15.0  # ... or, after LOAD_WAIT_S of waiting, when load1 < LOAD_FALLBACK (flagged)
LOAD_WAIT_S = 1800.0


def load1() -> float:
    return round(os.getloadavg()[0], 2)


def now_utc() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


# ── the gate: before every measured phase ───────────────────────────────────

def gate(label: str, *, poll_s: float = 30.0, load_ok: float = LOAD_OK, load_fallback: float = LOAD_FALLBACK,
         max_load_wait_s: float = LOAD_WAIT_S) -> dict:
    """Waits while QUIET exists (no limit) and until load1 < load_ok. After
    max_load_wait_s of load waiting it runs once load1 < load_fallback, and
    flags the phase. QUIET is re-checked after every wait."""
    t0 = time.monotonic()
    quiet_wait = 0.0
    load_wait = 0.0
    flagged = False
    last_note = -1e9
    while True:
        if QUIET.exists():
            print(f"[gate] QUIET is set; waiting before {label}", flush=True)
            q0 = time.monotonic()
            while QUIET.exists():
                time.sleep(poll_s)
            quiet_wait += time.monotonic() - q0
            print(f"[gate] QUIET cleared after {quiet_wait:.0f} s", flush=True)
            continue
        load = load1()
        if load < load_ok:
            break
        if load_wait >= max_load_wait_s and load < load_fallback:
            flagged = True
            print(f"[gate] load {load} after {load_wait:.0f} s of waiting: running {label} anyway (flagged)",
                  flush=True)
            break
        if time.monotonic() - last_note > 300:
            print(f"[gate] load {load} >= {load_ok}; waiting before {label}", flush=True)
            last_note = time.monotonic()
        l0 = time.monotonic()
        time.sleep(poll_s)
        load_wait += time.monotonic() - l0
    return {"label": label, "quiet_wait_s": round(quiet_wait), "load_wait_s": round(load_wait),
            "load1_at_start": load1(), "load_limit": load_fallback if flagged else load_ok,
            "load_flagged": flagged, "gate_s": round(time.monotonic() - t0)}


def phase_end() -> dict:
    """What the gate cannot see: the load at the end and QUIET appearing mid-phase."""
    return {"load1_at_end": load1(), "quiet_at_end": QUIET.exists()}


# ── process facts through libproc (no `ps` spawns while sampling) ────────────

_libproc = ctypes.CDLL("/usr/lib/libproc.dylib")
_libc = ctypes.CDLL("/usr/lib/libSystem.dylib")


class _Timebase(ctypes.Structure):
    _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]


_tb = _Timebase()
_libc.mach_timebase_info(ctypes.byref(_tb))
_TICK_NS = _tb.numer / _tb.denom  # rusage_info times are mach ticks on Apple silicon

PROC_PIDTASKINFO = 4
RUSAGE_INFO_V4 = 4


def _taskinfo(pid: int):
    buf = (ctypes.c_uint8 * 96)()
    n = _libproc.proc_pidinfo(pid, PROC_PIDTASKINFO, ctypes.c_uint64(0), buf, 96)
    if n < 96:
        return None
    raw = bytes(buf)
    resident = int.from_bytes(raw[8:16], "little")
    threads = int.from_bytes(raw[84:88], "little")
    return resident, threads


def _rusage(pid: int):
    buf = (ctypes.c_uint64 * 64)()
    if _libproc.proc_pid_rusage(pid, RUSAGE_INFO_V4, buf) != 0:
        return None
    return list(buf)


def proc_stats(pid: int) -> dict | None:
    """RSS / footprint (MB), threads, CPU s (own and reaped children), disk bytes.

    `disk_written_mb` is rusage ri_diskio_byteswritten: only the I/O the process
    issued itself (its own fsyncs, large cluster writes). Delayed writes that
    the kernel flushes later are never charged to it, so it is a LOWER BOUND
    (it reads 0 for a phase that commits small SQLite transactions)."""
    ti = _taskinfo(pid)
    ru = _rusage(pid)
    if ti is None or ru is None:
        return None
    resident, threads = ti
    t = lambda ticks: ticks * _TICK_NS / 1e9  # noqa: E731
    return {
        "rss_mb": round(resident / 1048576, 1),
        "footprint_mb": round(ru[9] / 1048576, 1),
        "threads": threads,
        "cpu_s": round(t(ru[2] + ru[3]), 3),
        "child_cpu_s": round(t(ru[12] + ru[13]), 3),
        "disk_read_mb": round(ru[18] / 1048576, 1),
        "disk_written_mb": round(ru[19] / 1048576, 1),
        "lifetime_max_footprint_mb": round(ru[30] / 1048576, 1),
    }


def children(pid: int) -> list[int]:
    """Live child processes (libproc; no subprocess spawned)."""
    buf = (ctypes.c_int32 * 4096)()
    n = _libproc.proc_listchildpids(pid, buf, ctypes.sizeof(buf))
    return [int(x) for x in buf[:max(0, n)] if x]


class Sampler:
    """Samples a server process every `every_s`: peak RSS/footprint/threads and
    the peak number of live child processes (ffprobe/ffmpeg)."""

    def __init__(self, pid: int, every_s: float = 0.05, count_children: bool = True):
        self.pid = pid
        self.every_s = every_s
        self.count_children = count_children
        self.peak = {"rss_mb": 0.0, "footprint_mb": 0.0, "threads": 0, "children": 0}
        self.samples = 0
        self._stop = threading.Event()
        self._t = threading.Thread(target=self._run, daemon=True)
        self._t.start()

    def _run(self):
        while not self._stop.is_set():
            s = proc_stats(self.pid)
            if s:
                self.samples += 1
                for k in ("rss_mb", "footprint_mb", "threads"):
                    self.peak[k] = max(self.peak[k], s[k])
            if self.count_children:
                self.peak["children"] = max(self.peak["children"], len(children(self.pid)))
            self._stop.wait(self.every_s)

    def stop(self) -> dict:
        self._stop.set()
        self._t.join()
        s = proc_stats(self.pid)  # one last sample at the end of the phase
        if s:
            self.samples += 1
            for k in ("rss_mb", "footprint_mb", "threads"):
                self.peak[k] = max(self.peak[k], s[k])
        return {**self.peak, "samples": self.samples, "every_s": self.every_s}


def delta(before: dict | None, after: dict | None) -> dict:
    """CPU and disk spent between two proc_stats() samples. The rusage write
    count is a lower bound (see proc_stats); the write probe has the evidence."""
    if not before or not after:
        return {}
    return {
        "cpu_s": round(after["cpu_s"] - before["cpu_s"], 3),
        "child_cpu_s": round(after["child_cpu_s"] - before["child_cpu_s"], 3),
        "cpu_total_s": round(after["cpu_s"] + after["child_cpu_s"] - before["cpu_s"] - before["child_cpu_s"], 3),
        "disk_read_mb": round(after["disk_read_mb"] - before["disk_read_mb"], 1),
        "disk_written_mb_lower_bound": round(after["disk_written_mb"] - before["disk_written_mb"], 1),
    }


# ── listening sockets ───────────────────────────────────────────────────────

def listening(pid: int) -> list[str]:
    """Every TCP listener and UDP socket of `pid`, as lsof prints them."""
    tcp = subprocess.run(["lsof", "-nP", "-a", "-p", str(pid), "-iTCP", "-sTCP:LISTEN"],
                         capture_output=True, text=True).stdout
    udp = subprocess.run(["lsof", "-nP", "-a", "-p", str(pid), "-iUDP"], capture_output=True, text=True).stdout
    return (["TCP " + line.split()[-2] for line in tcp.splitlines()[1:]]
            + ["UDP " + line.split()[-1] for line in udp.splitlines()[1:]])


def assert_loopback(pid: int, stop, label: str = "server") -> list[str]:
    """Logs what lsof shows `pid` listening on; stops it (and raises) if any
    socket is bound beyond 127.0.0.1."""
    lst = listening(pid)
    print(f"[{label}] listening: {lst}", flush=True)
    bad = [x for x in lst if "127.0.0.1:" not in x]
    if bad or not lst:
        stop()
        raise RuntimeError(f"{label} must listen on 127.0.0.1 only; lsof shows {lst}")
    return lst


# ── zero-write evidence: state files and device counters ────────────────────

def file_stamp(p: Path) -> list[int] | None:
    """[size, mtime_ns], or None if the file does not exist."""
    try:
        st = os.stat(p)
    except FileNotFoundError:
        return None
    return [st.st_size, st.st_mtime_ns]


def db_stamps(db: Path | None) -> dict:
    """(size, mtime_ns) of a SQLite DB, its -wal and its -shm."""
    if db is None:
        return {}
    return {name: file_stamp(Path(str(db) + sfx)) for name, sfx in (("db", ""), ("wal", "-wal"), ("shm", "-shm"))}


def tree_stamps(root: Path | None) -> dict:
    """(size, mtime_ns) of every file under a server's state dir, logs excluded."""
    out: dict = {}
    if not root or not root.exists():
        return out
    for dp, dns, fns in os.walk(root):
        dns[:] = [d for d in dns if d not in ("log", "logs")]
        for f in fns:
            if f.endswith(".log"):
                continue
            p = os.path.join(dp, f)
            try:
                st = os.lstat(p)
            except FileNotFoundError:
                continue
            out[os.path.relpath(p, root)] = (st.st_size, st.st_mtime_ns)
    return out


def tree_diff(a: dict, b: dict) -> list[str]:
    """Files added or changed (size or mtime) from a to b; '-name' for removed ones."""
    return sorted(k for k in b if a.get(k) != b[k]) + ["-" + k for k in sorted(a) if k not in b]


_DISK: dict = {}


def device_disk(path: Path = WORK) -> str | None:
    """Whole-disk BSD name under `path` (for APFS: the physical store's disk)."""
    if "name" in _DISK:
        return _DISK["name"]
    name = None
    try:
        df = subprocess.run(["df", "-P", str(path)], capture_output=True, text=True).stdout.splitlines()
        mount = df[-1].split(None, 5)[5]
        info = plistlib.loads(subprocess.run(["diskutil", "info", "-plist", mount], capture_output=True).stdout)
        stores = info.get("APFSPhysicalStores") or []
        dev = stores[0]["APFSPhysicalStore"] if stores else info.get("ParentWholeDisk")
        name = re.sub(r"s\d+$", "", dev) if dev else None
    except (OSError, IndexError, KeyError, ValueError, plistlib.InvalidFileException):
        name = None
    _DISK["name"] = name
    return name


def _io_stats(node, disk: str, parent=None):
    stats = node.get("Statistics")
    if stats and "Bytes (Write)" in stats:
        parent = stats
    if node.get("BSD Name") == disk and parent is not None:
        return parent
    for ch in node.get("IORegistryEntryChildren") or []:
        r = _io_stats(ch, disk, parent)
        if r is not None:
            return r
    return None


def device_io() -> dict | None:
    """IOBlockStorageDriver byte counters of the disk holding WORK (no sudo).
    They count every byte that reached the device, whoever issued it and
    whenever the kernel flushed it. System-wide for that disk: other writers
    show up too, hence the idle baseline the write probe takes."""
    disk = device_disk()
    if not disk:
        return None
    try:
        raw = subprocess.run(["ioreg", "-a", "-c", "IOBlockStorageDriver", "-r", "-l", "-w0"],
                             capture_output=True).stdout
        roots = plistlib.loads(raw)
    except (OSError, ValueError, plistlib.InvalidFileException):
        return None
    for root in roots if isinstance(roots, list) else [roots]:
        st = _io_stats(root, disk)
        if st is not None:
            return {"read": int(st["Bytes (Read)"]), "write": int(st["Bytes (Write)"]),
                    "write_ops": int(st.get("Operations (Write)", 0)), "t": time.monotonic()}
    return None


def device_delta(a: dict | None, b: dict | None) -> dict | None:
    if not a or not b:
        return None
    return {"read_mb": round((b["read"] - a["read"]) / 1048576, 1),
            "write_mb": round((b["write"] - a["write"]) / 1048576, 1),
            "write_ops": b["write_ops"] - a["write_ops"], "s": round(b["t"] - a["t"], 2)}


class WriteProbe:
    """Zero-write evidence for one phase. Start it BEFORE any mutation (flux's
    watcher, when on, reacts within ~150 ms) and finish it after the phase.

    - `db`: (size, mtime_ns) of the DB, -wal and -shm before, right after, and
      after a `sync` + settle; `db_and_wal_unchanged` compares all three.
    - `state_files_changed`: every state-dir file (logs excluded) whose size or
      mtime changed by the after-sync look.
    - `device`: bytes that reached the disk during the phase and after the
      sync, plus a 2 s idle baseline taken just before the phase (other
      writers on the machine show up there)."""

    def __init__(self, state_root: Path | None, db: Path | None, idle_s: float = 2.0, settle_s: float = 3.0):
        self.state_root = state_root
        self.db = db
        self.settle_s = settle_s
        d0 = device_io()
        time.sleep(idle_s)
        self.dev0 = device_io()
        self.idle = device_delta(d0, self.dev0)
        self.db0 = db_stamps(db)
        self.tree0 = tree_stamps(state_root)

    def finish(self) -> dict:
        db1 = db_stamps(self.db)
        dev_phase = device_delta(self.dev0, device_io())
        subprocess.run(["sync"])
        time.sleep(self.settle_s)
        dev_sync = device_delta(self.dev0, device_io())
        db2 = db_stamps(self.db)
        tree2 = tree_stamps(self.state_root)
        same = all(self.db0.get(k) == db1.get(k) == db2.get(k) for k in ("db", "wal"))
        return {
            "db": {"before": self.db0, "after": db1, "after_sync": db2},
            "db_and_wal_unchanged": same if self.db0.get("db") else None,
            "shm_unchanged": (self.db0.get("shm") == db2.get("shm")) if self.db0.get("db") else None,
            "state_files_changed": tree_diff(self.tree0, tree2)[:40],
            "device": None if dev_phase is None else {
                "disk": device_disk(), "idle_2s_before": self.idle, "phase": dev_phase,
                "after_sync": dev_sync, "settle_s": self.settle_s},
        }


# ── scratch corpus: APFS clones and the mutations both harnesses apply ───────

def clone_tree(src: Path, dst: Path) -> float:
    """`cp -cR` (APFS clonefile: no data copied). Returns seconds."""
    if dst.exists():
        shutil.rmtree(dst)
    dst.parent.mkdir(parents=True, exist_ok=True)
    t0 = time.monotonic()
    subprocess.run(["nice", "-n", "19", "cp", "-cR", str(src), str(dst)], check=True)
    return round(time.monotonic() - t0, 2)


def remove_tree(p: Path) -> None:
    if p.exists():
        subprocess.run(["nice", "-n", "19", "rm", "-rf", str(p)], check=False)


def media_files(root: Path) -> list[Path]:
    return sorted(p for p in root.rglob("*") if p.suffix.lower() in MEDIA_EXTS and p.is_file()
                  and not p.name.startswith("."))


def seed_movie(lib: Path) -> Path:
    """The file the add phase copies: the first .mkv under Movies/ by path (deterministic)."""
    return next(p for p in media_files(lib / "Movies") if p.suffix == ".mkv")


def add_movies(lib: Path, batch: int, n: int) -> list[Path]:
    """Adds `n` new movie folders (APFS clones of one seed .mkv) to lib/Movies.
    Names differ per batch: 'Bench Added Movie A000 (2026)/…mkv'."""
    seed = seed_movie(lib)
    added = []
    for i in range(n):
        name = f"Bench Added Movie {chr(65 + batch)}{i:03d} (2026)"
        p = lib / "Movies" / name / f"{name}.mkv"
        p.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(["cp", "-c", str(seed), str(p)], check=True)
        added.append(p)
    return added


def touch_episodes(lib: Path, batch: int, n: int, episodes: list[Path] | None = None) -> list[Path]:
    """Sets mtime = now on `n` TV files spread across the library (a different
    set per batch). Content is unchanged: this is a 'touched' file. A library
    without TV files touches nothing (the phase is then an unchanged rescan)."""
    eps = episodes if episodes is not None else media_files(lib / "TV")
    if not eps:
        return []
    step = max(1, len(eps) // n)
    picked = [eps[(i * step + batch) % len(eps)] for i in range(n)]
    t = time.time()
    for p in picked:
        os.utime(p, (t, t))
    return picked


# Playback files that media/features does not have, derived from one that it
# does (ffmpeg stream copy into the play-lib, never into media/). The name
# differs from the source's, so neither server files the two as versions of
# one title.
DERIVED_PLAY_FILES = {
    "seek_2h_360p.ts": ("long_2h_360p.mkv", ["-map", "0", "-c", "copy", "-f", "mpegts"]),
}


def prepare_play_lib(media: Path, files: list[str], dst: Path, ffmpeg: str = "/opt/homebrew/bin/ffmpeg") -> Path:
    """A folder of the playback test files, shared by both playback harnesses
    (same inodes, so the same page cache for both servers): APFS clones of
    media/features files, plus stream-copied derivatives (DERIVED_PLAY_FILES).
    Made once; kept between runs."""
    dst.mkdir(parents=True, exist_ok=True)
    for f in files:
        p = dst / f
        if f in DERIVED_PLAY_FILES and not (media / f).exists():
            if p.exists() and p.stat().st_size > 0:
                continue
            src, args = DERIVED_PLAY_FILES[f]
            tmp = dst / f".{f}.part"
            subprocess.run(["nice", "-n", "19", ffmpeg, "-nostdin", "-loglevel", "error", "-y", "-i",
                            str(media / src), *args, str(tmp)], check=True)
            tmp.rename(p)
            continue
        src = media / f
        if not p.exists() or p.stat().st_size != src.stat().st_size:
            if p.exists():
                p.unlink()
            subprocess.run(["cp", "-c", str(src), str(p)], check=True)
    return dst


def rotated(files: list[str], k: int) -> list[str]:
    """`files` starting at index k mod n (run k plays a different file first)."""
    if not files:
        return []
    k %= len(files)
    return files[k:] + files[:k]


def warm_files(paths: list[Path]) -> float:
    """Reads each file once (nice 19) so every run starts from a warm page cache."""
    t0 = time.monotonic()
    for p in paths:
        subprocess.run(["nice", "-n", "19", "cat", str(p)], stdout=subprocess.DEVNULL, check=True)
    return round(time.monotonic() - t0, 2)


def dir_bytes(p: Path) -> int:
    total = 0
    for root, _dirs, files in os.walk(p):
        for f in files:
            try:
                total += os.lstat(os.path.join(root, f)).st_size
            except OSError:
                pass
    return total


def sqlite_bytes(db: Path | None) -> dict:
    if not db or not db.exists():
        return {"db_bytes": None}
    wal = Path(str(db) + "-wal")
    shm = Path(str(db) + "-shm")
    return {"db_path": str(db), "db_bytes": db.stat().st_size,
            "wal_bytes": wal.stat().st_size if wal.exists() else 0,
            "shm_bytes": shm.stat().st_size if shm.exists() else 0}


# ── statistics and results ──────────────────────────────────────────────────

def _nums(xs) -> list[float]:
    return [x for x in xs if isinstance(x, (int, float)) and not isinstance(x, bool)]


def pct(xs: list[float], p: float) -> float | None:
    """Nearest-rank percentile (p95 of 5 samples is the maximum)."""
    v = sorted(_nums(xs))
    if not v:
        return None
    k = max(0, min(len(v) - 1, int(-(-p * len(v) // 100)) - 1))
    return v[k]


def median(xs) -> float | None:
    """statistics.median: the mean of the two middle values for an even n."""
    v = _nums(xs)
    return round(statistics.median(v), 6) if v else None


def summary(xs) -> dict:
    """n, true median, min–max range and the nearest-rank p95 (the maximum for
    n < 20). Headline numbers and ratios use the median, with the range and n."""
    v = sorted(_nums(xs))
    if not v:
        return {"n": 0, "median": None, "min": None, "max": None, "p95_nearest_rank": None}
    return {"n": len(v), "median": median(v), "min": v[0], "max": v[-1], "p95_nearest_rank": pct(v, 95)}


def ratio(j, f) -> float | None:
    """Jellyfin / flux, from two medians."""
    try:
        return round(j / f, 1) if j and f else None
    except TypeError:
        return None


def machine() -> dict:
    def sh(*a):
        try:
            return subprocess.run(a, capture_output=True, text=True).stdout.strip()
        except OSError:
            return None
    return {"host": "M1 Max, 64 GB, macOS (SHARED with another project; numbers are indicative)",
            "macos": sh("sw_vers", "-productVersion"), "cpu": sh("sysctl", "-n", "machdep.cpu.brand_string"),
            "ncpu": os.cpu_count(), "load1": load1()}


RESULT_STAMP_RE = re.compile(r"-(\d{8}-\d{6})[a-z]?\.json$")


def write_result(prefix: str, obj: dict, out: Path = RESULTS) -> Path:
    """Writes `<prefix>-YYYYMMDD-HHMMSS.json` and never overwrites: the file is
    created with O_EXCL, and a name already taken (two runs finishing within
    one second) gets a letter, `…-HHMMSSb.json`."""
    out.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    data = json.dumps(obj, indent=2).encode()
    for letter in ["", *"bcdefghijklmnopqrstuvwxyz"]:
        path = out / f"{prefix}-{stamp}{letter}.json"
        try:
            fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
        except FileExistsError:
            continue
        with os.fdopen(fd, "wb") as fh:
            fh.write(data)
        print(f"wrote {path}", flush=True)
        return path
    raise RuntimeError(f"no free result name for {prefix}-{stamp}")


def wait_port_free(port: int, timeout_s: float = 30) -> None:
    t0 = time.monotonic()
    while subprocess.run(["lsof", "-nP", f"-iTCP:{port}", "-sTCP:LISTEN"], capture_output=True).stdout:
        if time.monotonic() - t0 > timeout_s:
            raise RuntimeError(f"port {port} is busy")
        time.sleep(0.5)


def stop_process(proc: subprocess.Popen, timeout_s: float = 20) -> dict:
    """SIGTERM the server's process group; returns shutdown time and the
    lifetime CPU of the server plus every child it reaped (wait4)."""
    import signal
    if proc.returncode is not None:
        return {"already_stopped": True}
    t0 = time.monotonic()
    last = proc_stats(proc.pid)
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    ru = None
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        pid, status, ru = os.wait4(proc.pid, os.WNOHANG)
        if pid:
            break
        time.sleep(0.01)
    else:
        os.killpg(proc.pid, signal.SIGKILL)
        _, _, ru = os.wait4(proc.pid, 0)
    proc.returncode = 0
    return {"shutdown_s": round(time.monotonic() - t0, 3),
            "lifetime_cpu_s_incl_children": round(ru.ru_utime + ru.ru_stime, 2) if ru else None,
            "lifetime_peak_rss_mb_incl_children": round(ru.ru_maxrss / 1048576, 1) if ru else None,
            "last_sample": last}
