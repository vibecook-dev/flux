"""The scan benchmark protocol shared by flux_scan_bench.py and jellyfin_scan_bench.py.

One run, for either server. Every measured phase (marked *) is preceded by
the gate (QUIET absent and load1 < 10; see benchlib.gate) and records the load
at its start and end:

  1. fresh APFS clone of the corpus under the scratch dir (both servers scan a
     clone, so the corpus under media/ is never written and the page-cache
     state is the same for both: a fresh clone has no cached pages of its own);
  2. * first start: fresh (empty) state dir, server spawned at `nice -n 19`;
     spawn -> public endpoint and spawn -> API ready; lsof must show 127.0.0.1
     only; RSS right after ready and 2 s later;
  3. * cold scan of Movies/ + TV/ (time to first items visible, wall,
     server-side time, peak RSS/threads/children, CPU incl. reaped children);
  4. * R unchanged rescans;
  5. * R x (add N new movie folders -> rescan) — waits until the movie count
     has grown by N;
  6. * R x (touch N episodes -> rescan);
  7. 10 s idle -> RSS / threads after the scans; DB and state size; stop
     (lifetime CPU incl. children from wait4);
  8. * K restarts on the populated state dir (the catalog of step 3-6):
     spawn -> public endpoint and spawn -> API usable (libraries listed), RSS
     3 s later, stop.

Each scan phase (3-6) has two end points: `wall_s`, when the server reports
the scan done (what a user sees), and `quiescent_s`, when the server has
been idle for 1 s (no CPU, no child process, DB/-wal unchanged), which
includes work a server defers past "done" (flux fingerprints new and changed
files in a background pass after committing them). CPU, disk and peak RSS
are given for both windows (`usage`, `usage_until_quiescent`).

Each scan phase (3-6) also carries a write probe (benchlib.WriteProbe), started
before the phase's mutation: (size, mtime_ns) of the DB, -wal and -shm before,
after and after a sync; every changed state file; device-level bytes with an
idle baseline. The rusage write count is kept as `disk_written_mb_lower_bound`.
flux also reports what each scan job did (media files opened, writer
commands) from its own counters; Jellyfin has no such counter.

Each scan request covers every library ("refresh all"); completion is polled
every 5 ms for the first second, then every 50 ms (same for both servers).
Process samples (RSS, footprint, threads, children) are taken every 50 ms.
"""
from __future__ import annotations

import time
from pathlib import Path

import benchlib as B

POLL_FAST_S = 0.005  # first second of a scan: rescans can take a few ms
POLL_S = 0.05


def poll_interval(elapsed_s: float) -> float:
    return POLL_FAST_S if elapsed_s < 1.0 else POLL_S


class Adapter:
    """What the protocol needs from a server. See FluxAdapter / JellyfinAdapter."""

    tool = "?"
    pid: int
    state: Path

    def start(self) -> dict: ...  # first start on an empty state dir
    def restart(self) -> dict: ...  # start on the populated state dir; returns timings, then stays up
    def prepare(self, movies: Path, tv: Path) -> None: ...
    def begin_scan(self, cold: bool, movies: Path, tv: Path): ...
    def poll_done(self, token) -> dict | None: ...
    def any_items(self) -> bool: ...
    def counts(self) -> dict: ...
    def movie_count(self, counts: dict) -> int: ...
    def db_path(self) -> Path | None: ...
    def stop(self) -> dict: ...  # idempotent: {} when nothing is running

    def scan_mark(self):
        """Opaque position before a scan phase, for scan_counters()."""
        return None

    def scan_counters(self, mark) -> dict:
        """What the server's own scan jobs did since `mark` (files opened,
        writer commands), or how this was established instead."""
        return {"available": False}

    def finish_run(self) -> dict:
        """Run-level evidence collected after the last stop (e.g. Jellyfin's extraction check)."""
        return {}

    def data_dirs(self) -> list[Path]:
        """The server's persistent state (logs excluded)."""
        return [d for d in self.state.iterdir() if d.is_dir() and d.name not in ("log", "logs")]


def wait_quiescent(pid: int, db: Path | None, t0: float, t_done: float, *, window_s: float = 1.0,
                   every_s: float = 0.1, busy_cpu_frac: float = 0.05, timeout_s: float = 600.0) -> dict:
    """After a scan reports done, waits until the server has been idle for
    `window_s`: in every sample its CPU (own + reaped children) grew by less
    than `busy_cpu_frac` of one core, it had no child process, and the DB and
    -wal kept their (size, mtime_ns). Returns the time of its last activity,
    from t0. This catches work a server does after it reports a scan done
    (flux fingerprints new and changed files in a background pass after they
    are committed and browsable). A server idle from the start gives the
    done time, t_done."""
    stamps = lambda: {k: v for k, v in B.db_stamps(db).items() if k != "shm"}  # noqa: E731
    cpu = lambda s: (s["cpu_s"] + s["child_cpu_s"]) if s else 0.0  # noqa: E731
    prev, prev_db, t_prev = B.proc_stats(pid), stamps(), time.monotonic()
    t_start, t_active = t_prev, t_done
    timed_out = False
    while True:
        time.sleep(every_s)
        now = time.monotonic()
        st, db_now, kids = B.proc_stats(pid), stamps(), len(B.children(pid))
        if cpu(st) - cpu(prev) > busy_cpu_frac * (now - t_prev) or kids or db_now != prev_db:
            t_active = now
        prev, prev_db, t_prev = st, db_now, now
        if now - t_active >= window_s and now - t_start >= window_s:  # a full window observed idle
            break
        if now - t_start > timeout_s:
            timed_out = True
            break
    return {"quiescent_s": round(t_active - t0, 3), "quiescent_window_s": window_s,
            "quiescent_busy_cpu_frac": busy_cpu_frac, "quiescent_timed_out": timed_out}


def timed_scan(ad: Adapter, *, cold: bool, lib: Path, gate: dict, probe: B.WriteProbe,
               expect_movies: int | None = None, first_items: bool = False,
               timeout_s: float = 4 * 3600) -> dict:
    l0 = B.load1()
    mark = ad.scan_mark()
    st0 = B.proc_stats(ad.pid)
    sampler = B.Sampler(ad.pid)
    t0 = time.monotonic()
    token = ad.begin_scan(cold, lib / "Movies", lib / "TV")
    first = None
    res = None
    while time.monotonic() - t0 < timeout_s:
        if first_items and first is None and ad.any_items():
            first = round(time.monotonic() - t0, 3)
        res = ad.poll_done(token)
        if res is not None:
            break
        time.sleep(poll_interval(time.monotonic() - t0))
    if res is None:
        raise TimeoutError("scan did not finish")
    wall = round(time.monotonic() - t0, 3)
    counts = ad.counts()
    expect_ok = None
    if expect_movies is not None:
        # The scan has finished; give the count up to 60 s to catch up.
        t1 = time.monotonic()
        while ad.movie_count(counts) < expect_movies and time.monotonic() - t1 < 60:
            time.sleep(POLL_S)
            counts = ad.counts()
        expect_ok = ad.movie_count(counts) >= expect_movies
        res["count_wait_s"] = round(time.monotonic() - t1, 3)
        res["wall_until_count_s"] = round(time.monotonic() - t0, 3)
    st1 = B.proc_stats(ad.pid)
    quiet = wait_quiescent(ad.pid, ad.db_path(), t0, t0 + wall)
    st2 = B.proc_stats(ad.pid)
    peak = sampler.stop()
    end = B.phase_end()
    out = {"wall_s": wall, **res, "first_items_s": first, "counts": counts, **quiet,
           "background_after_done_s": round(quiet["quiescent_s"] - wall, 3), "peak": peak,
           "usage": B.delta(st0, st1), "usage_until_quiescent": B.delta(st0, st2),
           "load1": [l0, end["load1_at_end"]], "gate": gate, "end": end,
           "scan_counters": ad.scan_counters(mark)}
    out["writes"] = probe.finish()
    if expect_movies is not None:
        out["expected_movies"] = expect_movies
        out["expected_ok"] = expect_ok
    return out


def measured(ad: Adapter, label: str, mutate=None, **kw) -> dict:
    """gate -> write probe (idle baseline, state snapshot) -> mutation -> scan."""
    g = B.gate(f"{ad.tool} {label}")
    probe = B.WriteProbe(ad.state, ad.db_path())
    if mutate:
        mutate()
    return timed_scan(ad, gate=g, probe=probe, **kw)


def run(ad: Adapter, *, library: Path, clone: Path, rescans: int, add_files: int, touch_files: int,
        restarts: int = 3, settle_s: float = 2.0) -> dict:
    result: dict = {"tool": ad.tool, "library": str(library), "clone": str(clone), "started": B.now_utc(),
                    "machine": B.machine(), "protocol": __doc__.strip()}
    result["gate"] = B.gate(f"{ad.tool} clone of {library.name}")
    result["clone_s"] = B.clone_tree(library, clone)
    episodes = B.media_files(clone / "TV")
    result["corpus_files"] = {"movies_dir": len(B.media_files(clone / "Movies")), "tv_dir": len(episodes)}
    try:
        g = B.gate(f"{ad.tool} first start")
        first = ad.start()
        first["gate"] = g
        first["end"] = B.phase_end()
        result["first_start"] = first
        result["after_start"] = B.proc_stats(ad.pid)
        time.sleep(2)
        result["idle_2s"] = B.proc_stats(ad.pid)
        ad.prepare(clone / "Movies", clone / "TV")

        phases: dict = {}
        phases["cold_scan"] = measured(ad, "cold scan", cold=True, lib=clone, first_items=True)
        print(ad.tool, "cold_scan", phases["cold_scan"]["wall_s"], "s", phases["cold_scan"]["counts"], flush=True)
        time.sleep(settle_s)
        phases["unchanged_rescan"] = []
        for k in range(rescans):
            r = measured(ad, f"unchanged rescan {k + 1}", cold=False, lib=clone)
            phases["unchanged_rescan"].append(r)
            print(ad.tool, "unchanged_rescan", r["wall_s"], "s; db+wal unchanged:",
                  r["writes"]["db_and_wal_unchanged"], r["scan_counters"], flush=True)
            time.sleep(settle_s)
        phases["add_rescan"] = []
        for k in range(rescans):
            before = ad.movie_count(ad.counts())
            r = measured(ad, f"add rescan {k + 1}", mutate=lambda k=k: B.add_movies(clone, k, add_files),
                         cold=False, lib=clone, expect_movies=before + add_files)
            phases["add_rescan"].append(r)
            print(ad.tool, "add_rescan", r["wall_s"], "s", "ok" if r.get("expected_ok") else "COUNT MISMATCH",
                  r["scan_counters"], flush=True)
            time.sleep(settle_s)
        phases["touch_rescan"] = []
        for k in range(rescans):
            r = measured(ad, f"touch rescan {k + 1}",
                         mutate=lambda k=k: B.touch_episodes(clone, k, touch_files, episodes), cold=False, lib=clone)
            phases["touch_rescan"].append(r)
            print(ad.tool, "touch_rescan", r["wall_s"], "s", r["scan_counters"], flush=True)
            time.sleep(settle_s)
        result["phases"] = phases
        time.sleep(10)
        result["idle_after_scans"] = B.proc_stats(ad.pid)
        result["idle_after_scans"]["children_alive"] = len(B.children(ad.pid))
        result["db"] = B.sqlite_bytes(ad.db_path())
        result["state_bytes"] = sum(B.dir_bytes(d) for d in ad.data_dirs())
        result["stop"] = ad.stop()

        result["restarts"] = []
        for k in range(restarts):
            time.sleep(1)
            g = B.gate(f"{ad.tool} restart {k + 1}")
            r = ad.restart()
            time.sleep(3)
            st = B.proc_stats(ad.pid) or {}
            r.update({"rss_3s_mb": st.get("rss_mb"), "footprint_3s_mb": st.get("footprint_mb"),
                      "threads_3s": st.get("threads"), "gate": g, "end": B.phase_end()})
            r["stop"] = ad.stop()
            result["restarts"].append(r)
            print(ad.tool, "restart", r.get("startup_to_public_s"), r.get("startup_to_ready_s"), "s", flush=True)
    finally:
        ad.stop()  # a no-op unless a phase failed with the server up
        B.remove_tree(clone)
    result["evidence"] = ad.finish_run()
    result["finished"] = B.now_utc()
    result["load1_end"] = B.load1()
    result["summary"] = summarize(result)
    return result


def _phase_list(r: dict) -> list[dict]:
    ph = r.get("phases", {})
    out = [ph["cold_scan"]] if "cold_scan" in ph else []
    for name in ("unchanged_rescan", "add_rescan", "touch_rescan"):
        out += ph.get(name, [])
    return out


def flagged_phases(r: dict) -> list[str]:
    """Phases that ran above the normal load limit, or saw QUIET appear mid-phase."""
    out = []
    items = [("first_start", r.get("first_start") or {})]
    items += [(f"restart {i + 1}", x) for i, x in enumerate(r.get("restarts") or [])]
    items += [(p.get("gate", {}).get("label", "?"), p) for p in _phase_list(r)]
    for name, x in items:
        g, e = x.get("gate") or {}, x.get("end") or {}
        if g.get("load_flagged") or e.get("quiet_at_end"):
            out.append(name)
    return out


def summarize(r: dict) -> dict:
    ph = r.get("phases", {})

    def s(name, key="wall_s"):
        return B.summary([x.get(key) for x in ph.get(name) or []])

    cold = ph.get("cold_scan", {})
    first = r.get("first_start") or {}
    rs = r.get("restarts") or []
    unchanged = ph.get("unchanged_rescan") or []
    return {
        "first_start_to_public_s": first.get("startup_to_public_s"),
        "first_start_to_ready_s": first.get("startup_to_ready_s"),
        "restart_to_public_s": B.summary([x.get("startup_to_public_s") for x in rs]),
        "restart_to_ready_s": B.summary([x.get("startup_to_ready_s") for x in rs]),
        "cold_scan_wall_s": cold.get("wall_s"),
        "cold_scan_quiescent_s": cold.get("quiescent_s"),
        "cold_scan_first_items_s": cold.get("first_items_s"),
        "cold_scan_peak_rss_mb": cold.get("peak", {}).get("rss_mb"),
        "cold_scan_peak_threads": cold.get("peak", {}).get("threads"),
        "cold_scan_cpu_total_s": cold.get("usage", {}).get("cpu_total_s"),
        "cold_scan_cpu_total_until_quiescent_s": cold.get("usage_until_quiescent", {}).get("cpu_total_s"),
        "cold_scan_device_written_mb_after_sync": ((cold.get("writes") or {}).get("device") or {}).get(
            "after_sync", {}).get("write_mb"),
        "cold_scan_disk_written_mb_lower_bound": cold.get("usage", {}).get("disk_written_mb_lower_bound"),
        "unchanged_rescan_wall_s": s("unchanged_rescan"),
        "unchanged_rescan_db_and_wal_unchanged": [x.get("writes", {}).get("db_and_wal_unchanged") for x in unchanged],
        "unchanged_rescan_media_files_opened": [x.get("scan_counters", {}).get("media_files_opened")
                                                for x in unchanged],
        "unchanged_rescan_writer_commands": [x.get("scan_counters", {}).get("writer_commands") for x in unchanged],
        "add_rescan_wall_s": s("add_rescan", "wall_until_count_s"),
        "touch_rescan_wall_s": s("touch_rescan"),
        "idle_rss_after_start_mb": (r.get("idle_2s") or {}).get("rss_mb"),
        "idle_rss_after_scans_mb": (r.get("idle_after_scans") or {}).get("rss_mb"),
        "idle_threads_after_scans": (r.get("idle_after_scans") or {}).get("threads"),
        "db_bytes": (r.get("db") or {}).get("db_bytes"),
        "state_bytes": r.get("state_bytes"),
        "lifetime_cpu_s_incl_children": (r.get("stop") or {}).get("lifetime_cpu_s_incl_children"),
        "lifetime_peak_rss_mb": (r.get("stop") or {}).get("lifetime_peak_rss_mb_incl_children"),
        "cold_scan_disk_read_mb": cold.get("usage", {}).get("disk_read_mb"),
        "cold_scan_peak_children": cold.get("peak", {}).get("children"),
        "flagged_phases": flagged_phases(r),
    }
