# flux vs Jellyfin 12.2: head-to-head benchmark

*2026-10-20, Phase E. flux: release build of main `b935d2d` (Phase E memory work merged: jemalloc, idle memory release; `CARGO_TARGET_DIR=$FLUX_WORK/target/c-bench`). Harness: `tools/bench` at `b935d2d` for the full-library runs and `3e6adc0` for playback and library-tiny (the only change records a failed seek instead of aborting the run; the server crates are identical). Jellyfin 12.2.0 (macOS arm64 build). Both servers use Homebrew ffmpeg/ffprobe 9.0.2.*

**Summary file:** `bench/results/flux-e-bench-summary-20261020-043247.json`. It covers 17 runs, all listed in §5.1. All run files are `bench/results/flux-e-bench-*.json`.

## Read this first

- **Shared machine.** All numbers come from one M1 Max (64 GB, macOS 26.5). Another project's dev server was running the whole time, at normal priority.
  - Both servers, and the harnesses, ran at `nice -n 19`.
  - The 1-minute load average was **3.4–11.2**. Every measured phase started below 10, because the harness gate waits for that. Two phase-end readings were above 10 (11.2 and 10.3), both in the add rescans of Jellyfin's first full-library run.
  - The load gate waited once, for 30 s, before a Jellyfin phase.
  - The runs took 03:20–04:32 local time (10:20–11:32 UTC).
- **No page-cache purge** (no sudo).
  - Every scan runs on a fresh APFS clone of the corpus. A clone has no cached pages of its own, so both servers start equally close to cold.
  - The SSD's own cache may still hold the shared blocks.
  - Playback is deliberately page-cache-warm for both servers.
  - None of this is a cold-HDD or NAS number.
- **macOS only.** Jellyfin is mostly deployed on Linux/Docker. Nothing here was measured on Linux, an N100, a Pi, SMB or ZFS.
- **Synthetic corpus.** It is 7 short seed clips cloned into 14,760 media files under messy names, plus 1,911 sidecars (§2). File sizes and probe costs are not those of real films.
- **flux reports a scan "done" before all its work is finished.** Since Phase D, flux commits new files without a content fingerprint, so they are browsable at once. It then fingerprints them in a background pass. This report gives both end points:
  - **done:** what a user waits for;
  - **quiescent:** all work finished.

  On the full library the gap is 3.2 s vs 10.2 s, so **the cold-scan ratio is 326× to browsable and 104× to all work finished.** Quote the one that fits the claim.
- **Different work, by design.** Jellyfin runs ffprobe on every file and reads NFOs and local artwork. flux probes headers in-process and reads none of those (§3).
- **Small samples.**
  - Full library: flux 3 runs, Jellyfin 2.
  - library-tiny: 3 each.
  - Playback: 3 runs each, so n = 3 per file.
  - Seeks: one per target per run.
  - Every figure below is a true median with its min–max range and n. **Ratios are Jellyfin's median ÷ flux's median.**
- **Jellyfin's cold scan moves between sessions.** On this machine it took 900 s on 2026-10-19 (`flux-bench-scan-jellyfin-library-20261019-082205.json`) and 994 s in the review's re-run (`flux-review-scan-jellyfin-library-20261019-203948.json`). Here it took 1,047–1,063 s. The 1-minute load over those cold scans was similar each time (3.3 → 5.8, 5.6 → 7.5, 4.6–5.8). Against 900–994 s, flux's 3.24 s gives **≈ 280–310×** to browsable, not 326×. Quote a range, or say which session.
- **Server-side only.** Everything is timed at the HTTP level on loopback. For browser click → first frame for flux, see e2e.md. Jellyfin's web client was not driven.
- **Units.** Memory and disk figures are MiB (2^20 bytes), as the harness records them. Earlier reports labelled the same quantities "MB".

## 1. Summary

All 17 runs completed. The full-library runs went F J F J F, playback F J J F F J, and library-tiny F J J F F J.
- Every "add 50 movies" rescan was confirmed by the server's movie count: 18 of 18 for flux, 15 of 15 for Jellyfin.
- Every server start passed the loopback check (lsof: 127.0.0.1 only).
- Every Jellyfin run passed the no-extraction check.

| | flux, median (range, n) | Jellyfin 12.2, median (range, n) | Jellyfin ÷ flux |
|---|---|---|---|
| **Full library** (16,671 files, 14,760 media; Movies + TV) | 3 runs | 2 runs | |
| Cold scan → everything browsable ("done") | **3.24 s** (3.10–3.78, 3) | 1,055 s (1,047–1,063, 2) | **326×** |
| Cold scan → all work finished ("quiescent") | **10.19 s** (9.94–11.30, 3) | 1,055 s (1,047–1,063, 2) | **104×** |
| First titles browsable during the cold scan | **87 ms** (72–96, 3) | 1.16 s (1.15–1.16, 2) | 13× |
| CPU for the cold scan, until quiescent, incl. child processes | **6.5 s** (6.4–6.5, 3) | 1,285 s (1,268–1,302, 2): 782 s own + 503 s ffprobe | 198× |
| Disk written by the cold scan (device, after sync) | **127 MiB** (127–132, 3) | 16,745 MiB (16,738–16,753, 2) | 132× |
| Unchanged rescan, wall | **142 ms** (82–161, 9) | 19.7 s (18.8–20.8, 6) | **138×** |
| Unchanged rescan: files opened / writer commands / DB+WAL | **0 / 0 / unchanged** (9 of 9) | n/a / n/a / rewritten, 662 MiB to the device (657–681, 6) | — |
| Add 50 movie folders → rescan → all 50 listed | **122 ms** (88–135, 9) | 21.4 s (20.9–22.4, 6) | 175× |
| Touch 50 episodes → rescan done | **167 ms** (163–182, 9); re-fingerprints finished at 1.25 s | 23.3 s (22.8–23.8, 6) | 140× (19× to quiescent) |
| Peak RSS over the run (`wait4` maxrss) | **100 MiB** (93–108, 3) | 544 MiB (539–549, 2) | 5.5× |
| Idle after the scans (10 s): RSS / phys footprint | **67 / 22 MiB** (RSS 65–81; footprint 22.0–22.4) | 480 / 353 MiB (476–483; 349–357) | 7.2× / 16× |
| Catalog DB + WAL after the scans | **26.7 + 0.3 MiB** | 82.0 + 18–37 MiB | 3.1× (state dir 4.0×) |
| **library-tiny** (424 files, 393 media) | 3 runs | 3 runs | |
| Cold scan: done / quiescent | **75 ms** (73–77) / 294 ms (290–299) | 27.6 s (26.2–31.9) / 27.9 s | 368× / 95× |
| Unchanged / add 50 / touch 50 rescan (n = 9 each) | **10 / 36 / 14 ms**; touch re-fingerprints finished at 1.09 s | 374 ms / 3.69 s / 3.85 s | 37× / 103× / 275× (touch: 3.5× to quiescent) |
| Peak RSS (`wait4`) / idle RSS after the scans | **50 / 33 MiB** | 414 / 411 MiB | 8.2× / 12.5× |
| **Startup** | | | |
| Restart on the full catalog → API usable (libraries listed) | **20.7 ms** (19.3–24.0, 9) | 5.02 s (4.77–5.16, 6) | **243×** |
| Restart on the full catalog → public endpoint | **13.6 ms** (12.7–16.7, 9) | 1.44 s (1.42–1.46, 6) | 106× |
| First start, empty data dir → API ready (scan runs, both corpora) | **21 ms** (20–66, 6; bimodal) | 6.27 s (6.02–7.58, 5) | 294× (≈100× against flux's slow cluster) |
| RSS 2 s after a first start | **15.8 MiB** (15.8–15.9, 3) | 344 MiB (344–345, 2) | 22× |
| **Playback** (3 runs each, 7 files, rotated order, warm-up play excluded) | | | |
| Time to first segment (TTFS), remux, 6 files | **2.8–17.9 ms** | 176–617 ms | 25–67× |
| TTFS, TrueHD 7.1 → 6-channel audio transcode | **113 ms** (108–117) | 395 ms (383–399) | 3.5× |
| Play request + TTFS (what a click costs), all 7 files | **4.1–136 ms** | 187–634 ms | 1.5–50× |
| Cold seek far into a 2 h file (600/3,600/5,100 s; MKV + TS). "Cold" = a segment not produced before in the session; the page cache is warm | **1.3 ms** (0.6–1.7, 18) | 624 ms (459–762, 18) | **480×** |
| Cold seek, 4K HEVC 60 Mb/s at 30 s (42 MB segment) | **19 ms** (15–19, 3) | 2.03 s (1.88–2.09, 3) | 106× |
| First play after a server start (the warm-up play): plan + TTFS | 9.8 ms (4.0–9.9, 3) | 410 ms (407–410, 3) | 42× |
| CPU for the 7-file session, incl. ffmpeg/ffprobe | **1.6 s** (1.5–1.8) | 9.4 s (9.3–9.5) | 5.7× |
| RSS after playback | **71 MiB** (70–74; footprint 55–58) | 388 MiB (382–389; footprint 299–306) | 5.5× |
| **Install** | 32.6 MiB binary (23.7 MiB stripped) + 1.2 MiB web UI | 238 MiB (183 MiB server and .NET runtime + 55 MiB web UI) | 9.6× (stripped binary + web UI); 7.0× as built |

Two results that are not wins:
- **flux's first play of a long MPEG-TS** costs 133 ms (132–136) in `/play`. TS has no index, so flux reads the whole file once (454 MiB here, from the page cache) to build one. Play + TTFS is 136 ms against Jellyfin's 205 ms, only 1.5×. From a cold disk, that read runs at disk speed: by arithmetic, not measured, about 1 s for this file on a 500 MB/s SATA SSD, and several seconds on an HDD or NAS. Neither server was measured from a cold disk.
- **Jellyfin often cannot serve the last segment of `h264_aac.m2ts`.** Its playlist lists 6 segments, and its first ffmpeg run finishes the 30 s file before the seek asks for the 6th. The seek to 30 s returned HTTP 404 in all 3 runs used here; its log reads "transcoding quit before we got there". In the aborted first set (§5.1), one run served it in 142 ms by restarting ffmpeg and the next got the 404, which aborted that set. flux's playlist lists 5 segments, so it has no 30 s target. These seeks are recorded (`seek_errors`) and are in no ratio.

### Against the research targets (research/00-synthesis.md)

| Target | Result |
|---|---|
| Cold scan ≥ 10× faster than Jellyfin | **met**: 326× to browsable, 104× to all work finished |
| Unchanged rescan ≤ 1 s, with 0 file opens and 0 DB writes | **met**: 142 ms. flux's own counters show 0 files opened and 0 writer commands, and catalog.db/-wal size and mtime were unchanged in 9 of 9 rescans. |
| ≤ 5 s from a new file to browsable | **met**: 122 ms with an explicit rescan. The watcher was off here. scan.md measured a median of 193 ms on the full library (Phase C, `flux-scan-library-20261019-025811.json`) and 179 ms on library-tiny (Phase D, `flux-d-scancat-library-tiny.json`). |
| Startup ≤ 0.5 s | **met**: 21 ms first start; 21 ms restart on the full catalog |
| Peak memory ≤ 300 MiB | **met**: 100 MiB (93–108) |
| Idle memory ≤ 50 MiB | **met at start** (15.8 MiB RSS). **After a full scan, met by phys footprint (22 MiB) but not by RSS (65–81 MiB).** RSS − footprint is clean resident memory: it includes the binary's text and SQLite's memory-mapped reads, and was not broken down further. |
| Server-side TTFS ≤ 0.1 s | **met for remux** (2.8–18 ms). **Missed for TrueHD** (113 ms, the ffmpeg audio transcode) and, counting `/play`, for the first play of a long TS (136 ms). |
| Cold seek ≤ 50 ms | **met**: 0.6–19 ms. The maximum is the 42 MB 4K segment. |

## 2. The corpus

- **`media/library`:** 16,671 files, of which 14,760 are media files, in `Movies/` and `TV/`. `tools/corpus/gen-library.py` generated it, and `media/library.manifest.jsonl` is its ground truth.
  - Every media file is an APFS clone of one of **7 seed clips** in `media/seeds/`: 1.39–1.81 MB, **6 s each**.
    - Six are 1280×720: H.264 MKV, H.264 MP4 (twice, moov at the front and at the end), H.264 TS, HEVC MKV and AV1 MKV.
    - The seventh is MPEG-4 Part 2 AVI at 720×400.
  - **Each clone gets a unique 16-byte trailer** in a container-legal form: an EBML Void in MKV, a `free` box in MP4, a null packet in TS, `JUNK` in AVI.
    - So every file's head+tail fingerprint (fp128) is distinct.
    - The only byte-identical files are the ones the benchmark's own add phase creates: 50 clones of one movie per add rescan.
    - The 2026-10-19 report said "every fp128 is shared by about 2,000–4,000 files", and called the corpus a worst case for fingerprint logic. That was false (BENCH-4). This corpus does not exercise flux's fingerprint de-duplication paths.
  - The other ~1,910 files are sidecars: 956 NFOs, 657 subtitles (`.srt`), 289 artwork images, and a few junk files (`.DS_Store`, `Thumbs.db`, `@eaDir`, a `readme.txt`, a `.partial~` download). Jellyfin reads the NFOs and artwork; flux ignores them. Extras and samples are media files, and are counted among the 14,760.
- **Clone vs real copy makes no difference to flux** (review measurement, 2026-10-19). Real copies with distinct physical blocks scanned in 0.181/0.189 s against 0.170/0.188 s for clones (library-tiny), and in 1.958/1.915 s against 1.973/1.975 s (Movies, 3,811 media files).
  - On 49 distinct real-size files (3.3 GB), Jellyfin cost 94–101 ms per file (4.59 / 4.95 s), against 74–87 ms per file on the clips (the review's two library-tiny runs: 29.1 / 34.3 s for 393 files). So the clips do not flatter Jellyfin's per-file cost. Both figures include each scan's fixed overhead. Source: the review's `SLOT-SUMMARY.md` and `flux-review-scan-jellyfin-{distinct-lib-distinct,library-tiny}-*.json`.
- **`media/library-tiny`:** 424 files, 393 media, generated the same way.
- **Playback files** (`media/features`) are real encodes, not clips (§5.4).

## 3. What each server does during a scan

Both servers run with network metadata off and with no background jobs. Beyond that, they do different work.

| | flux | Jellyfin 12.2 |
|---|---|---|
| Directory walk | Mille `walk_scan` (getattrlistbulk) | .NET directory enumeration |
| Per new or changed media file | open; read the header pages; probe in-process (flux-probe); parse the name (flux-names); commit in batches through one writer (47–56 writer commands for 14,760 files) | one **ffprobe child process** per file (2 at a time; 503 s of child CPU per cold scan, ≈ 34 ms per file); item creation; season entries |
| Content fingerprint | fp128 (64 KiB head + 64 KiB tail), **deferred**: computed by a background pass after the scan reports done (`fingerprints_deferred` = 14,760 on a cold scan). This is the done → quiescent gap. | none |
| ffprobe | only as a fallback for formats flux cannot index (0 on this corpus) | every file |
| NFO files, local artwork, embedded titles | not read | read (956 NFOs and 289 images in the full library) |
| Images | none: frame posters are a background job, off here (`--no-jobs`) | **none extracted.** It draws collages for its 3 root folders from child images. No poster, chapter image, trickplay or keyframe extraction was found in any run (checked per run; see below). |
| Metadata matching | off (`--no-jobs`) | internet providers and every metadata/image fetcher off |
| Filesystem watcher | off (`--no-watch`) | real-time monitor off |
| Scheduled tasks | none | every trigger cleared after the wizard (14 of its 18 tasks had triggers). Two startup tasks (Clean Transcode Directory, Update Plugins) still run once, at the first start. |
| What the DB holds afterwards | 2,998 movies and 300 shows. Extras are filed under their film and samples are ignored; episodes are per item. | 3,149 "movies" (extras and samples count as movies), 300 series, 10,949 episodes |

**The no-extraction check (BENCH-7).** In the 2026-10-19 runs, Jellyfin's TypeOptions named only Movie and Series/Season/Episode. Extras and stacked parts (`… - cd2.ts`) are plain `Video` items, so Jellyfin applied its default Screen Grabber to them and frame-grabbed 176 posters in the full library. The harness now gives every item type the scan creates (Movie, Video and Trailer; Series, Season, Episode and Video) TypeOptions with no fetchers.

After every Jellyfin run, a clone of jellyfin.db is checked:
- generated images by item type;
- chapter-image, trickplay, keyframe and media-segment rows;
- the image files under `data/metadata`;
- the scheduled tasks that ran.

All 8 Jellyfin runs passed: 2 full, 3 tiny and 3 playback.
- The only generated images were the 3 root-folder collages, and the playback runs had none.
- No run had a chapter image or a trickplay row, and no extraction task ran.
- The scan runs had 0 chapter and 0 keyframe rows. Each playback run had 3 chapter rows (markers read from the files; no images) and 6 rows in Jellyfin's KeyframeData table.

The review's "everything else off" variant turned off photos, the NFO reader, embedded titles, series grouping, collections and embedded subtitles. It scanned library-tiny in 26.2/31.5 s, against 29.1/34.3 s with the standard harness configuration in the same session. Jellyfin's local readers are not what makes it slow.

## 4. Methodology

### Harnesses (`tools/bench/`)

| File | Role |
|---|---|
| `benchlib.py` | Shared by everything below:<br>• the gate (below);<br>• process facts through libproc: RSS, phys footprint, threads, CPU of the server plus its reaped children, rusage disk bytes;<br>• a 50 ms sampler;<br>• the write probe;<br>• the lsof loopback check;<br>• APFS clones and the add and touch mutations;<br>• the playback library;<br>• statistics;<br>• result files that are never overwritten. |
| `scan_protocol.py` | The scan protocol: one function, used for both servers. |
| `flux_scan_bench.py`, `jellyfin_scan_bench.py` | Thin adapters, one per server API: first start, restart, libraries, scan request, completion, counts, scan counters. The Jellyfin one also writes its network config, runs the wizard, clears task triggers and runs the extraction check. |
| `hls_walk.py` | The HLS client walk, shared by both servers: TTFS and seeks. |
| `flux_playback_bench.py`, `jellyfin_playback_bench.py` | One playback run each: a fresh server, a one-folder library, a warm-up play, then 7 files in rotated order. |
| `headtohead.py` | `run` interleaves everything; `summarize` writes the summary: medians, ranges, n, and ratios of medians. |

These runs used:
- `FLUX_BENCH_PREFIX=flux-e-bench`, so result names start with it;
- `FLUX_BENCH_SCRATCH=$FLUX_WORK/tmp/e-bench/scratch`;
- `headtohead.py run --full-order FJFJF --play-runs 3 --tiny-order FJJFFJ`.

### The gate (shared machine)

Before **every measured phase**, the gate waits while the pause file `$FLUX_WORK/QUIET` exists, then until the 1-minute load is below 10. Measured phases are each start and restart, each scan phase (before its mutation) and each play, including the warm-up.
- After 30 min of waiting it accepts a load below 15, and flags the phase. No phase was flagged.
- Each phase records the load at its start and at its end, plus whether the pause file had appeared by its end.

### Isolation

- **flux:** `--bind 127.0.0.1 --tailnet off --no-jobs --no-watch`, with no web UI.
- **Jellyfin:** `config/network.xml` is written before every start. It sets the harness's own port, `LocalNetworkAddresses` 127.0.0.1 and `LocalNetworkSubnets` 127.0.0.1/32, and turns off auto-discovery, UPnP, IPv6 and remote access.
  - The wizard sets a random admin password and posts remote access off.
  - Jellyfin's log confirms it: `Kestrel is listening on "127.0.0.1"`, `Bind Addresses ["127.0.0.1"]`.
  - In the 2026-10-19 runs, Jellyfin was on 0.0.0.0 with remote access on and the admin account `bench`/`bench` (BENCH-1).
- After every start of either server, lsof lists its TCP listeners and UDP sockets. Anything beyond 127.0.0.1 stops the run. Every start in this report showed only `TCP 127.0.0.1:<port>`.

### Scan protocol (identical for both servers)

1. A fresh APFS clone of the corpus is made under the scratch dir (`cp -cR`), so `media/` is never written.
2. **First start**, on an empty state dir. Two times are taken from spawn:
   - **public endpoint:** flux `GET /api/v1/server`; Jellyfin `GET /System/Info/Public`;
   - **API ready:** flux, the same request; Jellyfin, `/Startup/Configuration` stops answering 503.

   flux's binary is run once with `--help` before any timing, because macOS assesses a new binary on its first exec.
3. **Cold scan of Movies/ + TV/.**
   - flux: `POST /api/v1/libraries` ×2.
   - Jellyfin: the libraries are created with `refreshLibrary=false`, followed by a 2 s settle, then `POST /Library/Refresh`.
   - **Done:**
     - flux: every library is out of `scanning`, and its last scan started after the request;
     - Jellyfin: the RefreshLibrary task is idle with an end time after the request.

     Completion is polled every 5 ms for the first second, then every 50 ms.
   - **Quiescent:** the time of the server's last activity before a full second of idle.
     - Idle means: CPU (own + children) under 5% of a core, no child process, and the DB and -wal keep their (size, mtime).
     - It is sampled every 100 ms and capped at 10 min. No phase hit the cap.
   - **First titles browsable:**
     - flux: any library's `item_count > 0`;
     - Jellyfin: movies + series + episodes > 0.
4. **3 unchanged rescans.** flux: `POST /libraries/{id}/scan` for both libraries. Jellyfin: `POST /Library/Refresh`.
5. **3 add rescans.** Each adds 50 movie folders (APFS clones of the first `.mkv` under Movies/) and rescans. The phase waits until the movie count has grown by 50.
6. **3 touch rescans.** Each sets mtime = now on 50 TV files spread across the library, a different set each time, and rescans.
7. **Teardown:**
   - 10 s idle, then RSS, footprint and threads;
   - DB, WAL and state-dir sizes;
   - SIGTERM; `wait4` gives the lifetime CPU and maxrss, including children.
8. **3 restarts** on the populated state dir. A full-library run therefore restarts on the full catalog, for both servers.
   - Each restart is timed from spawn to the public endpoint (as in step 2) and to **API usable**:
     - flux: `GET /api/v1/libraries` lists every library;
     - Jellyfin: an authenticated `GET /Library/VirtualFolders` answers 200.
   - RSS is read 3 s later.

Each scan phase runs: gate → write probe (2 s idle baseline, state snapshot) → mutation → scan request. Phases are 2 s apart, and every phase ends quiescent.

### Zero-write evidence (BENCH-3)

The old evidence for "0 bytes written" was the process's rusage `ri_diskio_byteswritten`. That counter only sees I/O the process issues itself, such as its own fsyncs and large cluster writes. Delayed writes that the kernel flushes later are never charged to it, so a phase that commits small SQLite transactions reads 0.
- In these runs, rusage says Jellyfin's library-tiny unchanged rescans wrote 0 MiB. In fact jellyfin.db and its WAL changed every time, and the device saw 0.5–1.0 MiB.
- On flux's full cold scan, rusage says 87 MiB; the device saw 127–132 MiB.

The evidence now, per scan phase:
- **State files:** (size, mtime_ns) of the catalog DB, -wal and -shm before the phase, right after it, and after a `sync` + 3 s, plus every changed file in the state dir (logs excluded). "DB and WAL unchanged" is the proof of no catalog write.
- **flux's own counters:** one `scan finished` line per scan job (`--log info,flux_scan::engine=debug`) carries `media_files_opened`, `writer_commands`, added/changed/moved/missing and `fingerprints_deferred`. The harness reads the lines each phase added. The ScanStatus API does not expose these yet.
- **Device bytes:** IOBlockStorageDriver counters of the SSD (disk6), during the phase and after the sync, with a 2 s idle baseline just before.
  - They are **system-wide**: any other writer on the disk shows up.
  - The write baselines were 0 in every phase but one. flux's first library-tiny cold scan had 3.1 MiB (546 ops) written in its 2 s baseline, so its 3.2 MiB, the low end of the tiny cold-scan range, may include another writer. The full-library cold scans and every Jellyfin total are clean. A small number (the 0.3–1.1 MiB in flux's unchanged rescans) is not attributed to a file.
  - The read counters were **not usable**: another process read about 18 MiB/s from the same SSD during much of the run (idle baselines of 34–38 MiB per 2 s).
- **rusage** writes are kept as `disk_written_mb_lower_bound`.
- **Jellyfin** has no per-scan counter of files opened or writes. Its evidence is the DB/WAL stamps and the device bytes.

### Playback protocol (identical walk for both servers)

**Library.** Seven test files plus a warm-up file, in `tmp/e-bench/scratch/play-lib/`. **Both servers read the same inodes**, so both see the same page cache. Every file is read once before the runs, so every run is page-cache-warm. Each run starts a fresh server with a fresh state dir.
- Six are APFS clones from `media/features`.
- `seek_2h_360p.ts` (454 MiB) is a stream copy of `long_2h_360p.mkv` (`ffmpeg -map 0 -c copy -f mpegts`), made once in the play-lib, because `media/features` has no long TS.

**Order (BENCH-6).** Run k of each server starts its file list at file k (rotations 0, 1 and 2), so each file is played in 3 different positions. Before the measured files, every run plays `intro_shared.mkv` (H.264 + AAC, 20 s) through the same walk and reports it separately as the **first play** (`warmup`). It absorbs the first-request cost after a server start, which the 2026-10-19 runs charged to whichever file came first.

**Per file:**
1. **Plan:**
   - Jellyfin: `POST /Items/{id}/PlaybackInfo` with a browser-like profile: fMP4 HLS, H.264/HEVC/AV1, AAC/AC-3/E-AC-3/Opus/FLAC up to 6 channels, no MKV/TS direct play.
   - flux: `POST /api/v1/play` with equivalent `ClientCaps`.
2. **TTFS:** master → variant → init → segment 0. If the master names a default demuxed audio rendition (flux), its playlist → init → segment 0 follows, sequentially, so flux's audio cost is added in full. Jellyfin muxes audio into its segments.
3. **Cold seeks** to the segments covering 600, 3,600, 5,100 and then 30 s; a target past the playlist's end is skipped. Every request is a fresh connection on loopback. An HTTP error is recorded as `seek_errors` and is not timed.

**What each server did per file** (flux's plan; Jellyfin's ffmpeg logs):

| File | flux | Jellyfin |
|---|---|---|
| h264_multi_audio_subs.mkv (H.264 + AAC, 60 s) | in-process remux: video copy, AAC copy, plus an AC-3 copy rendition | ffmpeg remux, copy/copy |
| h264_truehd71.mkv (H.264 + TrueHD 7.1, 30 s) | video copy; TrueHD → E-AC-3 5.1 by ffmpeg per segment, plus an AAC stereo rendition | ffmpeg: video copy; TrueHD → `aac_at` 6 channels |
| hevc_main10_hdr10_eac3.mkv (4K HDR10 + E-AC-3, 30 s) | video copy, E-AC-3 copy, plus an AAC stereo rendition | ffmpeg remux, copy/copy |
| hevc_4k_60mbps.mkv (4K HEVC 60 Mb/s + E-AC-3, 60 s; 42 MB segments) | video copy, E-AC-3 copy, plus an AAC stereo rendition | ffmpeg remux, copy/copy |
| long_2h_360p.mkv (H.264 + AAC, 2 h) | video copy, AAC copy | ffmpeg remux; **4 ffmpeg processes per session**: the first, plus one restart per far seek |
| h264_aac.m2ts (H.264 + AAC, 30 s) | in-process TS remux | ffmpeg remux; its playlist's 6th segment answered HTTP 404 in 3 of 3 runs |
| seek_2h_360p.ts (the 2 h file as MPEG-TS) | in-process TS remux. The first `/play` reads the whole file to build the index. | ffmpeg remux; 4 ffmpeg processes per session |

Neither server transcodes video. The work is the same for both: a remux on six files and a 6-channel audio transcode on the TrueHD one.

### Statistics and result files

- **Medians:** `statistics.median`, which is the mean of the two middle values for an even n. Each comes with its min–max range and n. **Ratios are ratios of medians.** The nearest-rank p95 is in the JSON as `p95_nearest_rank`; with n < 20 it is the maximum.
- **Result files** are `<prefix>-<kind>-<tool>-<corpus>-YYYYMMDD-HHMMSS.json`. Each is created with O_EXCL and never overwritten.
- **`summarize`** reads each input once.

## 5. Results in detail

### 5.1 Run files and conditions

| Run | File (`bench/results/`) | Local start | Load (phase starts → ends) |
|---|---|---|---|
| flux, full 1 | `flux-e-bench-scan-flux-library-20261020-032256.json` | 03:20 | 4.2–6.3 |
| Jellyfin, full 1 | `flux-e-bench-scan-jellyfin-library-20261020-034634.json` | 03:23 | 4.5–11.2 |
| flux, full 2 | `flux-e-bench-scan-flux-library-20261020-034846.json` | 03:46 | 4.0–6.0 |
| Jellyfin, full 2 | `flux-e-bench-scan-jellyfin-library-20261020-041208.json` | 03:49 | 4.3–6.3 |
| flux, full 3 | `flux-e-bench-scan-flux-library-20261020-041420.json` | 04:12 | 3.8–5.7 |
| Playback F J J F F J | `flux-e-bench-playback-{flux-041617, jellyfin-041638, jellyfin-041659, flux-041700, flux-041701, jellyfin-041722}.json` | 04:16–04:17 | 5.4–6.3 |
| library-tiny F J J F F J | `flux-e-bench-scan-{flux,jellyfin}-library-tiny-20261020-04{1916,2226,2533,2727,2921,3238}.json` | 04:17–04:32 | 3.4–9.0 |

**Not used:** `flux-e-bench-playback-flux-20261020-041426.json` and `…-jellyfin-…-041444.json`. These are the first playback set, which stopped when Jellyfin's 404 on the `h264_aac.m2ts` seek aborted its second run. The harness now records such a seek instead of aborting, and the playback set was re-run in full.

### 5.2 Full library, per run

| | flux 1 | flux 2 | flux 3 | Jellyfin 1 | Jellyfin 2 |
|---|---|---|---|---|---|
| First start: public / ready | 22 / 22 ms | 66 / 66 ms | 59 / 59 ms | 3.72 / 7.58 s | 3.45 / 7.13 s |
| Cold scan: done / quiescent | 3.78 / 11.30 s | 3.10 / 9.94 s | 3.24 / 10.19 s | 1,047.3 / 1,047.5 s | 1,062.5 / 1,062.8 s |
| First titles browsable | 72 ms | 96 ms | 87 ms | 1.15 s | 1.16 s |
| Cold-scan CPU until done / until quiescent | 4.5 / 6.5 s | 4.5 / 6.5 s | 4.4 / 6.4 s | 767 + 502 s ffprobe | 798 + 504 s ffprobe |
| Cold-scan files opened / writer commands | 14,760 / 56 | 14,760 / 47 | 14,760 / 48 | n/a | n/a |
| Cold-scan disk written: device after sync (rusage) | 132.1 (87.4) MiB | 127.3 (87.3) MiB | 127.2 (87.4) MiB | 16,738 (16,637) MiB | 16,753 (16,680) MiB |
| Cold-scan peak RSS / footprint (sampled); threads; child processes | 108 / 66 MiB; 47; 0 | 100 / 70 MiB; 46; 0 | 87 / 65 MiB; 45; 0 | 537 / 446 MiB; 29; 2 | 549 / 459 MiB; 30; 2 |
| Unchanged rescan ×3, wall | 161, 141, 155 ms | 151, 82, 84 ms | 158, 142, 137 ms | 20.79, 20.57, 19.19 s | 20.12, 18.98, 18.82 s |
| Unchanged rescan: CPU each | 0.54–0.72 s | 0.42–0.93 s | 0.53–0.71 s | 24.4–26.5 s | 24.4–24.5 s |
| Unchanged rescan: DB + WAL / device written | unchanged ×3 / 0.6–1.1 MiB | unchanged ×3 / 0.4 MiB | unchanged ×3 / 0.3–0.4 MiB | rewritten / 660–679 MiB | rewritten / 657–681 MiB |
| Add 50 ×3 (until +50 listed) | 125, 130, 135 ms | 88, 95, 119 ms | 122, 121, 128 ms | 20.94, 21.38, 21.34 s | 21.14, 21.69, 22.38 s |
| Touch 50 ×3: done (quiescent) | 163, 180, 167 ms (1.22–1.26 s) | 176, 166, 166 ms (1.25 s) | 182, 181, 164 ms (1.24–1.26 s) | 23.24, 23.78, 22.81 s | 23.13, 23.39, 23.42 s |
| Add / touch: device written each | 2.6–2.7 / 6.9–7.0 MiB | 2.4–2.5 / 6.9–7.3 MiB | 2.3–2.4 / 6.5–6.8 MiB | 693–697 / 674–680 MiB | 694–704 / 672–682 MiB |
| Idle after the scans: RSS / footprint / threads | 81.1 / 22.4 MiB / 24 | 66.7 / 22.2 MiB / 24 | 65.3 / 22.0 MiB / 24 | 483.2 / 357.3 MiB / 23 | 475.8 / 348.8 MiB / 24 |
| DB / WAL / state dir | 26.8 / 0.34 / 27.3 MiB | 26.7 / 0.35 / 27.2 MiB | 26.7 / 0.34 / 27.2 MiB | 82.1 / 36.7 / 119.0 MiB | 81.9 / 18.2 / 100.2 MiB |
| Run peak RSS (`wait4`) / lifetime CPU incl. children | 107.7 MiB / 12.1 s | 99.6 MiB / 11.7 s | 92.6 MiB / 12.0 s | 539.3 MiB / 1,530 s | 548.6 MiB / 1,561 s |
| Restarts ×3: public / API usable | 16.4, 15.5, 13.6 / 23.6, 23.3, 20.7 ms | 16.7, 13.9, 13.5 / 24.0, 21.4, 20.2 ms | 13.2, 12.7, 13.3 / 19.4, 19.3, 19.6 ms | 1.44, 1.46, 1.44 / 5.03, 4.84, 5.03 s | 1.45, 1.43, 1.42 / 5.02, 4.77, 5.16 s |
| Restart: RSS / footprint 3 s later | 70–72 / 23–24 MiB | 70–72 / 23–24 MiB | 69–71 / 22–24 MiB | 298–302 / 219–223 MiB | 298–302 / 219–223 MiB |

Notes:
- **flux's done → quiescent gap is its background fingerprint pass.** On a cold scan it reads about 2.3 GiB of fingerprint heads and tails (3.1 GiB until quiescent vs 0.8 GiB until done, rusage) and takes 4.5 → 6.5 s of CPU. On a touch rescan it re-fingerprints the 50 changed files about 1 s after the scan, for about 10 ms of CPU.
- **Writer commands are batches**, not files: the cold scan's 14,760 files were committed in 47–56 transactions.
- **flux's restart starts a Quick scan of every library** (its startup catch-up; the server log shows one `scan finished` per library right after ready). The RSS 3 s after a restart is read after that scan, which takes about as long as an unchanged rescan.
- **Jellyfin's rescans rewrite its DB every time.** Each one writes 657–704 MiB to the device, and the first rescan after the cold scan also redraws the root-folder collages.
- **Movie counts differ by model** (§3). Both servers saw every file.

### 5.3 library-tiny (3 runs each)

| | flux, median (range) | Jellyfin, median (range) | Ratio |
|---|---|---|---|
| First start: ready | 19.7 ms (19.5–20.4) | 6.12 s (6.02–6.27); public 2.50 s | 310× |
| Restart: API usable (n = 9) | 14.4 ms (13.6–15.4) | 4.93 s (4.66–5.17); public 1.43 s | 342× |
| Cold scan: done / quiescent | 75 ms (73–77) / 294 ms (290–299) | 27.60 s (26.21–31.94) / 27.93 s | 368× / 95× |
| First titles browsable | 19 ms (18–20) | 165 ms (147–165) | 8.7× |
| Cold-scan CPU until quiescent | 0.17 s | 35.6 s (34.4–40.5) | 207× |
| Cold-scan disk written (device after sync) | 3.7 MiB (3.2–3.7) | 251 MiB (250.5–251.4) | 68× |
| Unchanged rescan: wall (server) | 10 ms (8–12) (3 ms) | 374 ms (330–396) (340 ms) | 37× (113×) |
| Unchanged rescan: DB + WAL; device written | unchanged 9 of 9; 0.2 MiB (0.1–0.6) | rewritten 9 of 9; 0.7 MiB (0.5–1.0), while rusage said 0 | |
| Add 50 → +50 listed | 36 ms (32–41) | 3.69 s (3.49–4.39) | 103× |
| Touch 50: done (quiescent) | 14 ms (11–18) (1.09 s) | 3.85 s (3.57–4.51) | 275× (3.5×) |
| Peak RSS (`wait4`) / sampled during the cold scan | 50 / 35 MiB | 414 / 403 MiB | 8.2× |
| Idle after the scans: RSS / footprint | 32.9 / 17.3 MiB | 410.7 / 324.7 MiB | 12.5× / 19× |
| Threads: idle / cold-scan peak / after | 24 / 53 / 24 | 28 / 29 / 25 | flux has more |
| DB + WAL | 1.25 + 0.07 MiB | 3.48 + 4.3 MiB | 2.8× (state dir 5.8×) |
| Items | 40 movies, 5 shows (ground truth) | 44 movies, 5 series, 343 episodes | |

### 5.4 Playback

Medians of 3 runs, with the range in brackets. The warm-up play is not included.

| File | flux play | flux TTFS | flux play + TTFS | Jellyfin PlaybackInfo | Jellyfin TTFS | Jellyfin + TTFS | TTFS ratio | play + TTFS ratio |
|---|---|---|---|---|---|---|---|---|
| h264_multi_audio_subs | 1.3 ms (1.2–4.5) | **2.8 ms** (2.5–2.9) | 4.1 ms (3.7–7.4) | 15 ms (14–24) | 189 ms (177–195) | 204 ms (191–219) | 67× | 50× |
| h264_truehd71 | 1.3 ms | **113 ms** (108–117) | 115 ms (110–119) | 17 ms (11–20) | 395 ms (383–399) | 413 ms (394–419) | 3.5× | 3.6× |
| hevc_main10_hdr10_eac3 | 2.0 ms | **7.0 ms** (6.8–7.5) | 9.2 ms (8.8–9.2) | 18 ms (17–20) | 199 ms (193–207) | 219 ms (210–226) | 28× | 24× |
| hevc_4k_60mbps | 2.9 ms | **17.9 ms** (16.6–20.6) | 20.4 ms (19.8–23.5) | 17 ms (17–18) | 617 ms (615–717) | 634 ms (631–735) | 34× | 31× |
| long_2h_360p.mkv | 1.8 ms | **5.1 ms** (2.7–5.3) | 6.9 ms (4.2–8.5) | 12 ms (12–13) | 195 ms (186–200) | 207 ms (198–213) | 38× | 30× |
| h264_aac.m2ts | 9.6 ms (8.1–10.2) | **7.1 ms** (6.8–8.0) | 17.0 ms (15.2–17.6) | 12 ms (11–12) | 176 ms (174–199) | 187 ms (185–212) | 25× | 11× |
| seek_2h_360p.ts | **133 ms** (132–136) | 2.9 ms (2.8–3.5) | **136 ms** (135–139) | 10 ms (10–11) | 196 ms (189–200) | 205 ms (199–209) | 67× | **1.5×** |

- **TrueHD:** flux's video leg takes 3.2 ms. The rest is the first E-AC-3 audio segment from ffmpeg, which flux reports at 104–113 ms server time.
- **TS `/play`:** 133 ms is the full read that builds the TS index (§1). On M2TS (23 MiB) the same step costs 8–10 ms.

**First play vs warm play (BENCH-6).** In the 2026-10-19 runs, the first file of each Jellyfin run was always `h264_multi_audio_subs.mkv`, so its "73×" included Jellyfin's first-request cost. Now that cost lands on the warm-up play, which is reported on its own:

| | flux | Jellyfin |
|---|---|---|
| First play after start (`intro_shared.mkv`): plan | 6.2 ms (1.4–6.3) | 115 ms (115–117) |
| First play after start: TTFS | 3.5 ms (2.6–3.7) | 295 ms (290–295) |
| A later play of the same kind of file (`h264_multi`, H.264 + AAC remux): plan / TTFS | 1.3 / 2.8 ms | 15 / 189 ms |
| First *measured* play (position 0, after the warm-up): TTFS | 7.5 ms (2.5–117) | 199 ms (195–399) |

Jellyfin's first play after a start costs about +100 ms in PlaybackInfo and +100 ms in TTFS. flux's costs up to +5 ms in `/play`.

**Cold seeks** (video + audio for flux, one muxed segment for Jellyfin):

| File, target | flux | Jellyfin | Ratio |
|---|---|---|---|
| long_2h_360p.mkv, 600 / 3,600 / 5,100 s | 1.3 / 0.9 / 1.1 ms | 487 / 698 / 642 ms | 370–780× |
| seek_2h_360p.ts, 600 / 3,600 / 5,100 s | 1.3 / 1.4 / 1.3 ms | 508 / 724 / 623 ms | 390–520× |
| **Far seeks pooled, both 2 h files (n = 18 each)** | **1.3 ms** (0.6–1.7) | **624 ms** (459–762) | **480×** |
| hevc_4k_60mbps, 30 s (42 MB segment) | 19 ms (15–19) | 2,027 ms (1,880–2,089) | 106× |
| h264_multi_audio_subs, 30 s | 2.8 ms (2.2–3.1) | 131 ms (118–218) | 47× |
| long_2h / seek_2h, back to 30 s | 1.2 / 1.3 ms | 17 / 11 ms\* | — |
| hevc_main10_hdr10_eac3, 30 s | — (flux's playlist ends at 29.99 s) | 531 ms (529–631) | not like-for-like |
| h264_aac.m2ts, 30 s | — (flux's playlist ends before 30 s) | HTTP 404 in 3 of 3 runs | not like-for-like |

\* Not cold for Jellyfin: that segment already exists from the session's first ffmpeg run.

The summary's `pooled_seek_median` ratio (409×) pools every timed target, including the non-cold ones. The like-for-like figures are the rows above.

**Server cost of the 7-file session (plus warm-up):**

| | flux | Jellyfin |
|---|---|---|
| CPU, including ffmpeg/ffprobe children | 1.6 s (1.5–1.8; 1.2–1.5 s of it in ffmpeg child processes, the audio transcodes) | 9.4 s (9.3–9.5) |
| RSS at start → after playback | 15.9 → 71 MiB (70–74) | 313 → 388 MiB (382–389) |
| Peak RSS sampled (server) / `wait4` (server or a child) | 84 / 107 MiB | 388 / 396 MiB |
| Threads after playback | 33 | 29 |

### 5.5 Startup

| | flux | Jellyfin | Ratio |
|---|---|---|---|
| Restart on the full catalog: API usable (n = 9 / 6) | **20.7 ms** (19.3–24.0) | 5.02 s (4.77–5.16) | 243× |
| Restart on the full catalog: public endpoint | 13.6 ms (12.7–16.7) | 1.44 s (1.42–1.46) | 106× |
| Restart on library-tiny: API usable (n = 9 / 9) | 14.4 ms (13.6–15.4) | 4.93 s (4.66–5.17) | 342× |
| First start, empty state: API ready (scan runs, n = 6 / 5) | 21 ms (19.5–66.4) | 6.27 s (6.02–7.58) | 294× |
| First start: public endpoint | (same request) | 2.56 s (2.43–3.72) | |
| First start in the playback runs (n = 3 / 3) | 56 ms (20–56) | 6.27 s (6.18–6.33) | |
| RSS 2 s after a first start / 3 s after a full-catalog restart | 15.8 / 71 MiB | 344 / 301 MiB | 22× / 4.2× |

- **flux's first start is bimodal:** 5 of its 9 first starts took 19.5–22.2 ms and 4 took 56–66 ms. It was the same in the 2026-10-19 runs. The cause was not investigated.
- **Jellyfin's first start includes** its database migrations and ffmpeg capability probes; the review measured about 2.5 s and 3.5 s of them. Its "ready" on a first start is the end of the startup 503s, before any user exists. On a restart, "API usable" is an authenticated call.

### 5.6 Footprint

| | flux | Jellyfin |
|---|---|---|
| Install | `flux-server` 32.6 MiB as built (the release profile keeps `debug = 1`); **23.7 MiB** stripped (`strip -S -x`); web UI 1.2 MiB | 238.3 MiB self-contained .NET build: 183.3 MiB server + runtime, 55.1 MiB `jellyfin-web` |
| External tools | ffmpeg/ffprobe, only for fallbacks and audio/video transcodes | ffprobe on every scanned file; ffmpeg for every stream |
| RSS: 2 s after start / idle after a full scan / run peak | 15.8 / 65–81 / 93–108 MiB | 344 / 476–483 / 539–549 MiB |
| Phys footprint: idle after a full scan | 22.0–22.4 MiB | 349–357 MiB |
| Threads: idle / after a full scan | 24 / 24 | 27–28 / 23–24 |

flux's idle numbers come from Phase E's memory work: jemalloc for Rust and SQLite, and thread caches released on idle (server.md, "Phase E: memory"). The small WAL (0.34 MiB, against 64 MiB before) comes from Phase D's checkpoint changes (`e7d65e9`).

## 6. Changes since the 2026-10-19 report

The 2026-10-19 report (flux `23f390a`, harness `89828b5`) had these figures wrong or misleading.

| 2026-10-19 figure | Now | What was wrong |
|---|---|---|
| Startup "22.8 ms, 285×" | First start 21 ms (20–66, n = 6) vs 6.27 s; restart on the full catalog 20.7 ms vs 5.02 s (243×) | That "p50" was nearest-rank, which takes the lower middle value of a bimodal sample of 10. The true median was 44.5 ms, ≈ 146× (BENCH-5). First start and restart were not separated, and the report had no restart measurement. (The review measured Jellyfin's restart on library-tiny only.) |
| "0 files opened, 0 bytes written" on unchanged rescans | Still true. Now shown by DB/WAL stamps (9 of 9 unchanged), 0 writer commands and 0 files opened. | The evidence was rusage write bytes, which cannot see small writes. The claim held; the proof did not (BENCH-3). |
| Disk written: 102–104 MB, "160×" | 127–132 MiB at the device, 132× | rusage undercounted flux's own cold-scan writes (87 against 127 MiB here). |
| TTFS remux "26–73×" | 25–67× (and the warm-up play separately) | The 73× was the first play after a Jellyfin start (fixed file order). The file that went first carried about +100 ms of PlaybackInfo and +100 ms of TTFS (BENCH-6). The 67× now is h264_multi as a warm play, against flux's 2.8 ms. |
| Pooled cold seeks "628× / 119×" | Far seeks 1.3 ms vs 624 ms (480×); 4K 30 s 19 ms vs 2.03 s (106×) | The pooled sets differed: Jellyfin had a 30 s seek on hevc_main10 that flux skips, and non-cold "back to 30 s" seeks were included. |
| "Image extraction off" for Jellyfin | No extraction, verified in every run | False then. 176 extras and parts were frame-grabbed by the default Screen Grabber (BENCH-7). Their cost was not measured; BENCH-7 estimated about 1–2% of Jellyfin's scan. |
| Corpus: "about 6 distinct files cloned 14.7k times", "every fp128 shared by 2,000–4,000 files" | 7 seed clips of 6 s with a unique per-file trailer; every fp128 distinct | The generator's unique trailer was overlooked (BENCH-4). |
| Jellyfin bound to 0.0.0.0, remote access on, admin `bench`/`bench` | 127.0.0.1 only, random password, checked by lsof | The earlier runs exposed an admin account on the LAN and the tailnet (BENCH-1). No number depends on it. |
| flux cold scan 6.86/6.97 s, Jellyfin 899.6 s (131×) | flux 3.24 s done / 10.19 s quiescent; Jellyfin 1,055 s (2 runs). 326× / 104×. | Not wrong, but not comparable. flux now defers fingerprinting (Phase D), and "quiescent" is new. Jellyfin was measured once. Its result file records load 2.5–5.8 (3.3 → 5.8 over the cold scan), not the 2.5–3.7 that report gave. The review got 994 s (cold-scan load 5.6 → 7.5), and this run 1,047–1,063 s (cold-scan load 4.6–5.8). |
| flux idle RSS after a scan 127–132 MB (footprint 66–67 MB), WAL 64 MB | 65–81 MiB (footprint 22 MiB), WAL 0.34 MiB | Code changes, not errors: Phase D checkpoints and Phase E memory. |
| flux's watcher "cannot be disabled" (left on) | Off (`--no-watch`), like Jellyfin's monitor | The flag now exists. The review saw the watcher pre-empt some requested rescans in its own runs (not in the original ones). |
| Results overwritten in place (e.g. `flux-e2e-bench.json`); one summary counted a run twice | Timestamped, O_EXCL; inputs read once | |

Older still: research/00-synthesis.md's Jellyfin baseline (cold scan 1,442 s, unchanged rescan 19.6 s, startup 17.3 s, 319/520 MB) came from a 2026-10-18 run on a machine loaded by research agents. Its 17.3 s "startup" was a first start to the end of the startup 503s on that loaded machine. e2e.md's 11.6 s came from the same run, and was its public endpoint. This report replaces that baseline for every comparison.

## 7. Re-running

```sh
cd /path/to/flux
export CARGO_TARGET_DIR=$FLUX_WORK/target/c-bench TMPDIR=$FLUX_WORK/tmp/e-bench/tmp
nice -n 19 cargo build --release -p flux-server --bin flux-server -j 3
export FLUX_BENCH_PREFIX=flux-e-bench FLUX_BENCH_SCRATCH=$FLUX_WORK/tmp/e-bench/scratch
nice -n 19 python3 -u tools/bench/headtohead.py run --full-order FJFJF --play-runs 3 --tiny-order FJJFFJ
#   about 75 min: each Jellyfin full-library run takes about 24 min; --skip full,play,tiny leaves phases out
python3 tools/bench/headtohead.py summarize --files $FLUX_WORK/bench/results/flux-e-bench-{scan,playback}-*.json
```

- **Each harness gates itself:** no pause file and load < 10 before every measured phase; after 30 min it accepts load < 15 and flags the phase.
- **It never writes under `media/`.**
- **Single runs:** `flux_scan_bench.py --library …`, `jellyfin_scan_bench.py --library …`, and `{flux,jellyfin}_playback_bench.py --rotate k`.
- **This report uses `flux-e-bench-summary-20261020-043247.json`.**
- The harness's own summary, `…-043239`, covers only the re-run playback and tiny sets.

## 8. Gaps

- **A quiet machine with `sudo purge`** between cold scans, and **Linux/Docker** runs: Jellyfin's home turf, and flux's NAS target.
- **Cold-disk playback.** flux's first `/play` of a long TS reads the whole file. From a cold HDD or NAS that is seconds, and it should be measured. An incremental TS index is the fix to consider (flux-hls).
- **flux's background fingerprint pass** takes about 7 s on the full library, after a 3 s scan. Whether that matters depends on what waits for it: copy-then-delete moves are matched by fingerprint (scan.md), so a move made during that window is seen later.
- **A real-file corpus**: distinct encodes, long TS/M2TS, Blu-ray rips. It would change Jellyfin's ffprobe cost and flux's fingerprint cost.
- **More samples.** Jellyfin full library n = 2, playback n = 3 per file, one seek per target per run.
- **The device read counters were polluted** by another reader on the SSD, so read totals are rusage only (the server's own reads).
- **flux's per-scan counters come from debug log lines.** Exposing `media_files_opened` and `writer_commands` in `ScanStatus` (flux-core, flux-scan, flux-catalog) would make the evidence an API.
- **flux's bimodal first start** (20 ms or 56–66 ms) is unexplained.
- **Browser-level TTFF and seeks for Jellyfin's own web client**, to set beside e2e.md's flux numbers.
- **Plex.** It needs an account claim to run (research/09).
