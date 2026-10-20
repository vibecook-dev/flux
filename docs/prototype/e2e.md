# End-to-end product test: flux-server + web client in Chrome on the real corpus

*2026-10-19, after the Phase C merge (`79d20dd`). `CARGO_TARGET_DIR=$FLUX_WORK/target/c-e2e`.*

**Shared machine.** All numbers come from the M1 Max, which another project's dev server was also using. The 1-minute load average was **2.2–14.6** during the runs and is recorded per step and per phase in the result files. The server, Chrome, ffmpeg and cargo all ran at `nice -n 19`, and the server ran with tailnet **off**. The page cache was never purged (no sudo). Treat absolute timings as indicative; ratios within one run are the honest comparison.

## Summary

- **The product works end to end on the real 16.7k-file library.** A release flux-server serves the production web client (`VITE_FLUX_API=/`), driven through the system Google Chrome 155.
  - The final product walk passes **36/36 steps** with **0 console errors, 0 failed requests and 0 HTTP errors**.
  - It covers: first run with live SSE progress; home; grids of 3k and 12k films; search; movie, show, season, episode and unmatched pages; 17 plays covering every plan type, with 10 seeks each; audio switch; subtitles; resume; mark watched; the stats overlay; frame posters; an offline drive going away and coming back; phone and light screens.
- **Bugs found and fixed: 15**, in three crates (flux-hls, flux-meta, flux-server) and the web client. Each has a test (and for the visual ones, an e2e assertion). The ones users would feel most:
  - **Every Play click waited ~300 ms** before the `<video>` existed. Cause: React's `lazy()` suspends and its reveal is throttled. Click → first frame went from **324 → 51 ms** for direct play and from **~430 → ~158 ms** over HLS. The same fix brought the first grid cell of the 12k library from 837 to 132 ms.
  - **Unneeded video transcodes in Chrome:**
    - Constrained Baseline H.264 was transcoded, because the index lost the constraint flag.
    - 4K 60 Mb/s HEVC was transcoded 4K → 1080p, because the client only probed HEVC level 5.1.
    - "Expected on this device" came from a client-side sketch, which disagreed with Play on every TS file and on the Baseline film.
  - **Resume from Continue watching started at 0** for a film that had been watched before. Also, the last progress report on leaving the player was never sent.
  - **No frame posters for any AV1 file** (1,974 of the library's files): libdav1d outputs nothing with `-skip_frame nokey`.
  - **Visual bugs:**
    - search thumbnails were 156 px wide, with the title stacked letter by letter;
    - a second, ghost title sat behind the Home hero;
    - the Admin screen showed "0 items" after adding a library, and the raw `io error (ENOENT)` for an offline drive;
    - server timings printed as "196.53346 ms".
  - **Missing control:** there was no "Mark watched" in the UI.
- **Headline numbers.** The playback and bench tables below are from the final run, after the fixes.
- *2026-10-20 note:* the Jellyfin column now comes from benchmarks.md's same-harness head-to-head. It first quoted the 2026-10-18 baseline, a run on a machine loaded by research agents: cold scan 1,442 s, unchanged rescan 19.6 s, 319/520 MB.
  - Its "11.6 s" in the startup row was that run's **public endpoint** (`/System/Info/Public`) on a first start, not API ready (its ready time was 17.3 s). Against flux's 12.6 ms full-catalog restart, it read as ≈ 920×. Like for like (restart on the full catalog, API usable), the figures are 20.7 ms vs 5.02 s, ≈ 240×.
  - The flux column is still this report's own (Phase C, load 8.5–9.2, 4 API clients); current flux scan numbers are in benchmarks.md.

| | flux (final runs) | Jellyfin 12.2 on this machine (benchmarks.md, 2026-10-20, same harness) |
|---|---|---|
| Click → first frame, direct play (p50, 5 fresh contexts) | **50–51 ms** | not measured. A direct-play click can't be set against Jellyfin's server-side remux TTFS: the work differs. |
| Click → first frame over HLS: remux / audio conversion (p50) | **157–161 ms** | browser not measured; server-side TTFS alone is 176–617 ms on remux files |
| Click → first frame, first-ever play of a video transcode | 0.52–0.83 s | |
| Seek → first frame, p50 / p95, copied video (runs 2–5) | **97–113 / 146–316 ms** (browser) | 459–762 ms per far seek in a 2 h file; 1.9–2.1 s for a 42 MB 4K segment (server side only, so not like for like) |
| Seek → first frame, p95, first play of a transcode (cold pieces) | 1.2–1.7 s (AVI: 96 ms) | |
| Cold scan of the full library through the API (Movies + TV, 14,760 media files) | **10.4–11.4 s** (8 scans in 4 bench runs, as found and on fresh clones); first items browsable at 87–126 ms | 1,047–1,063 s (2 runs) |
| Unchanged rescan through the API (0 files opened) | **32–176 ms** per library (final run) | 18.8–20.8 s (both libraries) |
| Browse API p95 while that scan runs, 4 clients hammering | 33–38 ms (idle: 10–24 ms)† | |
| Spawn → public endpoint (`GET /api/v1/server`) | **12.6 ms** p50 (restart, full catalog), 20 ms (empty data dir) | 1.44 s (restart, full catalog); first start: 6.3 s to ready. Like for like to an authenticated API answer: flux 20.7 ms vs Jellyfin 5.02 s (benchmarks.md) |
| RSS: idle / after the first-run scan / peak in the product walk | **15 / 160 / 285 MB** (restart on the full catalog: 52–53 MB p50) | 344 MiB 2 s after start, 476–483 MiB idle after a full scan, 539–549 MiB peak (scan runs, not a product walk) |

† Not traceable. The bench and playback JSONs this report cited (`bench/results/flux-e2e-bench.json`, `flux-e2e-playback.json`) were overwritten on 2026-10-20 by the reverify session's `pnpm e2e` run of `da8801f`. That run measured 45.8–46.4 ms p95 during a 6.6–6.7 s cold scan, and 16.8–20.6 ms idle. The scan walls, first items, as-found rescans, startup and playback figures here survive in the run logs `tmp/e2e/{bench-run2,bench-run3,final-run,final-run2}.log`. The final run is `final-run2.log`, and the product walk is `bench/results/flux-e2e-product-final.json`. BENCH-2 read the final run's 10.647/10.749 s and 32–176 ms from the JSON before it was overwritten.

## 1. How it runs

```sh
cd /path/to/flux
export CARGO_TARGET_DIR=$FLUX_WORK/target/c-e2e
nice -n 19 cargo build --release -p flux-server --bin flux-server -j 4
cd web && VITE_FLUX_API=/ pnpm build
pnpm e2e            # = e2e:product, then e2e:bench, then e2e:playback (all nice -n 19)
```

| Script | What it does | Output |
|---|---|---|
| `scripts/e2e-product.mjs` | Fresh data dir, server on :18496. Walks the product in Chrome (36 steps). | `bench/results/flux-e2e-product.json`; screenshots in `bench/e2e-shots/` |
| `scripts/e2e-bench.mjs` | Server-only measurements through the API. Leaves `tmp/e2e/bench-data` for the next script. | `flux-e2e-bench.json` |
| `scripts/e2e-playback.mjs` | Per-plan-type TTFF and seeks in fresh browser contexts, then server TTFS after a restart. | `flux-e2e-playback.json` |
| `scripts/e2e-lib.mjs` | Shared helpers, listed below. | |

`e2e-lib.mjs` provides:

- the server spawn: `/usr/bin/time -l nice -n 19 flux-server --tailnet off`, plus `ps` RSS sampling;
- a pause before every heavy step while the pause file `$FLUX_WORK/QUIET` exists;
- `TMPDIR` and Chrome's caches under `tmp/e2e/`;
- console, page, failed-request and HTTP ≥ 400 capture.

Rules the harness keeps:

- Media under `media/` is only read.
- The two things the test renames or creates are APFS clones in `tmp/e2e/scratch/`:
  - a clone of `features/` (for the offline-drive test);
  - 12,000 clones of one seed file, made with `clonefile(2)`, so no data is copied (for the 12k grid).
- Device variants come from Chrome init scripts:
  - "no Matroska" (Safari-like), so MKV is repackaged over HLS;
  - "no AV1" (M1-like), so AV1 is transcoded.

## 2. What works (final product walk, 36/36)

| Area | Result |
|---|---|
| **First run** | Empty Home says "Your library is empty", which links to Admin. Four libraries were added through the **Add library** dialog. The card goes live over SSE: Movies 37 progress and 34 items events, TV 60 and 57. First progress arrives 38–47 ms after "Add and scan"; first items are browsable at 38–123 ms. Movies scanned in 2.1 s and TV in 5.2 s (3,811 and 10,949 files). Every card then shows its real count without a reload. |
| **Home** | Hero ("Just added", later "Continue watching") and five rows; no ghost title. |
| **Library grids** | **Movies (2,998 films):** first cell 68 ms after the click; scroll frame time p50/p95/p99 16.7/16.7/16.8 ms; 0 janky frames; 0 long tasks; 48 cards in the DOM; 13 MB JS heap. **TV:** all six sorts work from the UI. **12,000-film archive** (clones): first cell 121–132 ms, all 12 pages loaded by 1.2 s, scroll p95 16.7 ms with 0 janky frames, 30 cards in the DOM, A–Z jump 70 ms, instant filter 22 ms. |
| **Navigation** | Card click → title painted and detail loaded: p50 6 ms, p95 11 ms (after a hover prefetch). |
| **Search** | `/` then "blade run", typed at 40 ms per key: the first result shows 421 ms after the first keystroke. Enter opens the film. |
| **Item pages** | Film, show (62 episodes; Breaking Bad is absolute-numbered in the corpus), season (redirects into the show with the tab selected), episode (back-link to the show, "More from season 1"), and an unmatched title ("Matched as" at 40 %, Low, with the parser's evidence). "Expected on this device" now matches what Play did in **17/17** plays. |
| **Playback** | 17 plays, each with 10 seeks. Every plan label matches the expectation and the item page. Details in §4. |
| **Audio switch** | Multi-audio MKV: English AAC (direct play) → Japanese AC-3 5.1 (audio conversion over HLS). New first frame 174 ms after the click, at the same position. |
| **Subtitles** | SRT and ASS (forced, "Signs & Songs") both load as WebVTT in ~420 ms, and the right cue is active at its time. The ASS track shows as plain text, a known gap. |
| **Resume** | Played the 2 h film, seeked to 1834 s and left after 4 s. The stored position is 1838.07 s (the report sent on leaving). Home's hero shows it under "Continue watching"; Resume restarts at **1838.1 s** with a first frame 32 ms after the click. |
| **Mark watched** | New toggle. Marking watched clears Resume and removes the film from Continue watching; the grid card reads "Long 2h, watched". Unmark restores it. |
| **Stats overlay** | Plan pill, sentences, path, engine, video/audio/subtitles in → out, timings, server segments. No Rust debug names, and no contradictory "repackaged with no quality loss" sentence on transcodes. |
| **Frame posters** | Art jobs start 10 s after startup and pause during playback and scans. 68 Movies titles had frame art 4–5 minutes into the walk, **8 of them AV1** (0 before the fix). The "first added" grid shows the FRAME labels. |
| **Offline drive** | The scratch clone of `features/` (its own library) was renamed away. The **watcher noticed in 266 ms**: root unavailable, all 16 items kept, Admin says "Offline" with a plain sentence, the library page shows the notice, the item page says "Offline · … plays again when the drive is back", Play is disabled and `POST /play` answers 409. Renamed back: noticed in 1.16 s, available again, Play enabled. |
| **Phone and light** | Home, library, item and show at 390 × 844 (dark), and Home, item and Admin in the light theme. |

## 3. What broke, and how it was fixed

| # | Symptom (how the test found it) | Cause | Fix | Test | Commit |
|---|---|---|---|---|---|
| 1 | Every Play click: `<video>` mounted ~303 ms after the click. `POST /play` had answered in 4 ms and the prefetched segments were in by ~130 ms (traced with resource timing + rVFC). | A never-rendered `React.lazy` suspends even when its chunk is cached. The player is a bare route, so it mounts a fresh Suspense boundary, whose reveal React throttles. | Routes remember their loaded component and render it directly (`Component` getter: `loaded ?? lazy`). `startPlay` navigates once the player code is in. | `app/router.test.ts` | `64bd6e9` |
| 2 | Clicking a library while art jobs ran: first grid cell 365–837 ms, although the first page came back in ~110 ms. | The same suspension, on links clicked before their chunk loaded. | `navigateWhenLoaded()` for `Link` and Play: navigate synchronously if the code is in, else once it is. | `app/router.test.ts` | `08619ca` |
| 3 | `long_2h_360p.mkv` was **Transcoded** in Chrome ("can't decode H.264 Baseline"), while `/plan` said Direct play. | flux-hls named profile_idc 66 "Baseline" and dropped constraint_set1. The web lists Main/High. | `avc_profile_name(idc, flags)` → "Constrained Baseline"; `INDEX_VERSION` 3. | `avc_profile_names_keep_the_constraint_flag`, `constrained_baseline_is_copied_for_main_high_clients` (real file) | `c2ff303` |
| 4 | Transcode explanations contradicted themselves: "can't open MPEG-TS … repackages the streams (no quality loss)" next to "… transcodes it". | The planner added `ContainerUnsupported` before it knew the video would be transcoded. | Added only when the video is copied; `predict.ts` mirrors this. | 2 planner unit tests; `predict.test.ts` | `c2ff303` |
| 5 | `hevc_4k_60mbps.mkv` was transcoded 4K → 1080p ("HEVC level 156 is above … 153"). TTFF was 1.0 s and seek p95 2.4 s. | The caps only probed HEVC level 5.1. | HEVC level 5.2 candidates, claimed only when `decodingInfo` accepts 4K60 at 60 Mb/s. Now a 4K copy: TTFF 295 ms, seek p95 359 ms. | `detect.test.ts` (both ways) | `5e75583` |
| 6 | Resume from Continue watching started at **0:00**. | A seek past 90 % had marked the film watched. A later partial viewing keeps `watched` and stores the position (catalog semantics), and Home lists it, but `/play` ignored the position of a watched item. | `/play` resumes any stored position (a finished viewing stores 0). The Item page uses `isResumable`; the mock follows. | `play_truehd_mkv_over_hls_with_sessions` (rewatch), `card-utils.test.ts` | `a7d7757`, `5e75583` |
| 7 | Leaving the player didn't record where you were; only the last seek or 10 s report counted. | React clears refs before unmount cleanups, so `buildReport()` saw no `<video>`. | The cleanup passes the element it captured. | e2e: stored position = position at leave (1838.07 s vs the seek's 1834 s) | `5e75583` |
| 8 | No frame posters for any AV1 title ("no decodable frame found"). | ffmpeg's libdav1d outputs nothing with `-skip_frame nokey` when the only keyframe is before the seek point. | An empty result is retried without `-skip_frame`; a file that needed this skips the first attempt for its other candidates. | `av1_with_one_keyframe_still_gets_a_poster` (fails without the fix: `NoFrames`) | `dc11ff2` |
| 9 | "Expected on this device" said Transcode with the wrong reason for TS files, and Direct play for the Baseline film that Play transcoded. | The Item page used the client sketch (`predict.ts`). | It now uses the server's `POST /plan`; the sketch is only a fallback. | e2e: expected == played in 17/17 | `5e75583` |
| 10 | No way to mark something watched. | The client had the route but no control. | "Mark watched / unwatched" toggle (show or season: every episode). | `card-utils.test.ts`, e2e `mark-watched` | `5e75583` |
| 11 | Admin: a new library showed "0 items" until a reload. An offline drive showed "Partly offline", the raw `io error (ENOENT)` and "nothing changed, no files opened". | The SSE effect depended on the library list, so every progress event re-subscribed and cancelled the pending refresh. The copy was the raw scan state. | The effect subscribes once; a scan's end refreshes the list. `admin-state.ts` words: "Offline", a plain sentence (the raw error moves to a tooltip), "Last checked … nothing was removed". | `admin-state.test.ts`, e2e card counts | `5e75583` |
| 12 | Search thumbnails 156 px wide with the title stacked letter by letter. A ghost title behind the Home hero's heading. | `.typeset { padding: 12% }` resolves against the parent's width. | The art is a size container and the inset uses `cqi`; the title is hidden under 96 px; the hero's art is decorative. | e2e: thumbs 40 px, 0 hero fallback titles | `5e75583` |
| 13 | Console CSP violation during playback: `Loading media from 'data:,WEBVTT'`. | hls.js creates `<track>` elements with that src, and `media-src` blocked it. | `media-src 'self' blob: data:`. | `csp_hashes_exactly_the_inline_scripts` | `a7d7757` |
| 14 | Stats overlay read "196.53346 ms p50 / 926.43884 ms p95"; runtimes read "0m" (6 s clip) and "1m" (30 s). | Raw floats; minute rounding. | `formatMs`; `formatRuntime` gives seconds under a minute. | `format.test.ts` | `5e75583` |
| 15 | Search results: episodes, seasons and extras would read "Film". | The overlay labelled every non-show result "Film". | `resultEdge()`: "Episode · Breaking Bad · S01E01". | `card-utils.test.ts` | `4996b79` |

Test counts: `cargo test --workspace` **451 passed**, 0 failed, 4 ignored (447 before, plus 4 new; two existing tests gained assertions). Web vitest **102** (89 before).

## 4. Numbers

### 4.1 Click → first frame and seeks, per plan type

`pnpm e2e:playback` (final run). Each case has 5 fresh browser contexts with 10 seeks each. **Run 1 is the file's first-ever play on this server**: the index is built and nothing is transcoded yet. Runs 2–5 are reported separately, because the server keeps prepared assets and transcoded pieces. TTFF is the Play click → first `requestVideoFrameCallback`. A seek is the `currentTime` assignment → first rVFC after `seeked`. TTFF from the app's own metrics and from the harness's independent probe agree within 1 ms. Seek p50s agree within 3 ms, except the 10-min HEVC case (147 vs 163 ms). Runs 2–5 give 4 TTFF samples per case, so their p95 is the maximum. Load average was 3.0–11.4.

**By plan type** (cases pooled):

| Plan type | Cases | First play: TTFF | First play: seek p50 / p95 | Runs 2–5: TTFF p50 / p95 | Runs 2–5: seek p50 / p95 |
|---|---|---|---|---|---|
| Direct play | MP4, MKV | 51, 52 ms | 95 / 164 ms | **50 / 52 ms** | 98 / 172 ms |
| Direct stream (HLS remux) | FLAC MKV and 10-min HEVC Main10 MKV on a no-Matroska device | 157, 159 ms | 113 / 209 ms | **158 / 162 ms** | 113 / 316 ms |
| Audio conversion | 4K HEVC HDR10 + E-AC-3, TrueHD 7.1, DTS 5.1 | 197, 145, 123 ms | 97 / 146 ms | **158 / 161 ms** | 97 / 146 ms |
| Transcode | MPEG-2 TS, MPEG-4 AVI, AV1 on a no-AV1 device, H.264 M2TS | 825, 522, 542, 794 ms | 81 / **1,233 ms** | 143 / 162 ms | 80 / 98 ms |

**Per case:**

| Case | Plan | Device | First play: TTFF | First play: seek p50 / p95 | Runs 2–5: TTFF p50 / p95 | Runs 2–5: seek p50 / p95 |
|---|---|---|---|---|---|---|
| h264_aac_faststart.mp4 | Direct play | Chrome | 51 | 80 / 116 | 50 / 50 | 82 / 113 |
| h264_long_gop.mkv | Direct play | Chrome | 52 | 131 / 181 | 51 / 52 | 130 / 179 |
| h264_flac.mkv | Direct stream | no MKV | 157 | 96 / 114 | 158 / 160 | 96 / 114 |
| synth_hevc10_opengop_10min.mkv | Direct stream | no MKV | 159 | 196 / 330 | 161 / 162 | 163 / 365 |
| hevc_main10_hdr10_eac3.mkv (4K HDR10) | Audio conversion | Chrome | 197 | 113 / 148 | 157 / 161 | 114 / 147 |
| h264_truehd71.mkv | Audio conversion | Chrome | 145 | 96 / 113 | 158 / 161 | 96 / 112 |
| h264_dts51.mkv | Audio conversion | Chrome | 123 | 94 / 111 | 158 / 159 | 97 / 114 |
| mpeg2_ac3.ts | Transcode | Chrome | 825 | 80 / 1,711 | 158 / 162 | 64 / 81 |
| mpeg4_mp3.avi | Transcode | Chrome | 522 | 81 / 96 | 70 / 76 | 81 / 98 |
| av1_opus.mkv | Transcode | no AV1 | 542 | 80 / 1,301 | 141 / 143 | 87 / 99 |
| h264_aac.m2ts | Transcode | Chrome | 794 | 81 / 1,233 | 158 / 161 | 80 / 98 |

**The product walk** saw the same picture on its single runs, which include the cases above plus:

| Case | Plan | TTFF | Seek p50 / p95 |
|---|---|---|---|
| hevc_4k_60mbps.mkv (now a 4K copy) | Audio conversion | 295 ms | 163 / 359 ms |
| long_2h_360p.mkv (now copied) | Direct play | 30 ms | 96 / 115 ms |
| h264_aac_moov_end.mp4 | Direct play | 42 ms | 80 / 109 ms |
| synth_h264_10min.mkv | Direct play | 53 ms | 126 / 196 ms |
| synth_hevc10_opengop_10min.mkv | Direct play | 36 ms | 163 / 182 ms |
| AV1 on Chrome's own decoder | Direct play | 81 ms | 97 / 127 ms |
| TV episode (seed MP4) | Direct play | 36 ms | 97 / 108 ms |
| Resume of the 2 h film | Direct play | 32 ms | |

The first play of a session in a fresh browser was 281 ms. That is Chrome's media pipeline starting cold; later plays in the same tab were 30–50 ms.

**Before and after the Play fix** (same harness, earlier run):

| | Before | After |
|---|---|---|
| Direct play TTFF p50 | 323–324 ms | 50–51 ms |
| HLS remux / audio conversion TTFF p50 | 427–446 ms | 157–161 ms |
| Warm transcode TTFF p50 | 343–431 ms | 70–158 ms |

The remaining ~110 ms over HLS is hls.js: the master and variant playlists, the inits, and the MSE append.

**Server-side TTFS** (no browser, `POST /play` → the prefetch URLs the client fetches; a server restart first, so "cold" means the first request after a restart, with the play index still cached in the catalog):

| Case | Plan | `/play` | TTFS cold | TTFS + audio, cold | TTFS warm p50 / p95 |
|---|---|---|---|---|---|
| direct MP4 (first MiB) | Direct play | 3.2 ms | 6.3 ms | — | 5.2 / 5.9 ms |
| direct MKV | Direct play | 5.2 ms | 9.5 ms | — | 6.0 / 6.6 ms |
| FLAC MKV (no MKV) | Direct stream | 3.0 ms | 9.6 ms | 9.6 ms | 7.4 / 11.4 ms |
| 10-min HEVC Main10 (no MKV) | Direct stream | 1.6 ms | 10.1 ms | 10.1 ms | 13.4 / 16.5 ms |
| 4K HDR10 + E-AC-3 | Audio conversion | 2.6 ms | 12.8 ms | **120 ms** | 13.5 / 23.3 ms |
| TrueHD 7.1 | Audio conversion | 3.5 ms | 9.1 ms | **98 ms** | 6.6 / 8.0 ms |
| DTS 5.1 | Audio conversion | 3.9 ms | 8.8 ms | **97 ms** | 7.5 / 8.0 ms |
| MPEG-2 TS | Transcode | 3.4 ms | **518 ms** | 518 ms | 4.8 / 5.6 ms |
| MPEG-4 AVI | Transcode | 1.3 ms | **1,054 ms** | 1,054 ms | 2.9 / 4.8 ms |
| AV1 (no AV1) | Transcode | 2.8 ms | **460 ms** | 460 ms | 4.4 / 5.4 ms |
| H.264 M2TS | Transcode | 2.2 ms | **935 ms** | 935 ms | 3.6 / 4.1 ms |

Notes:

- Cold "TTFS + audio" for audio conversion is the first AAC segment from ffmpeg.
- Cold TS/AVI includes the ffprobe packet scan, because those indexes come from ffprobe.
- Warm numbers are cache hits.
- Jellyfin on this machine: TTFS 176–617 ms on remux files (395 ms with a TrueHD audio transcode), and 459–762 ms per far seek in a 2 h file (benchmarks.md, 2026-10-20). *The 191–913 ms TTFS and 318–840 ms seeks first quoted here came from the loaded 2026-10-18 baseline, with a fixed file order whose first play carried Jellyfin's start-up cost.*

Files: `bench/results/flux-e2e-playback.json` held every sample, the caps this Chrome sent, and the load per case. The reverify session's run of `da8801f` overwrote it on 2026-10-20. The figures above survive in `tmp/e2e/final-run2.log` (per case: TTFF, seek p50/p95, load; the server-side TTFS lines) and match it.

### 4.2 Scan, API under load, startup, RSS

`pnpm e2e:bench` (final run, load 8.5–9.2). Movies and TV are created over `POST /api/v1/libraries` on a fresh data dir. Four clients hammer seven browse endpoints the whole time.

| | Corpus as found | Fresh APFS clone (closer to OS-cold) |
|---|---|---|
| Cold scan wall: first POST → both libraries finished | **10.6 s** (Movies 2.0 s, TV 8.6 s; one device, so they run one after the other) | 10.7 s (2.0 / 8.7 s) |
| First items browsable | 121 ms | 91 ms |
| Unchanged rescan (3 runs, server time; files opened = 0, changes = 0) | Movies 59 ms p50 / 115 ms max; TV 42 / 73 ms | 88 / 176; 38 / 146 ms |
| API during the scan: all endpoints p50 / p95 / max | 3.8 / **35.9** / 68.5 ms (457 req/s) | 3.9 / 38.1 / 72.1 ms (422 req/s) |
| API idle, same hammer: p50 / p95 / max | 4.2 / **16.6** / 94.5 ms (727 req/s) | 4.4 / 23.6 / 109 ms |
| RSS: start / peak during scan / 3 s after | 15 / 148 / 126 MB | 15 / 145 / 139 MB |

Per endpoint during the scan (p50 / p95 ms; corpus as found):

| Endpoint | p50 / p95 |
|---|---|
| `/libraries` | 6.4 / 41 |
| items page, title | 7.2 / 45 |
| items page, `-added` | 6.3 / 45 |
| TV items page | 4.1 / 29 |
| `/home` | 2.9 / 11 |
| search | 2.3 / 7.5 |
| item detail | 1.8 / 6.1 |

At idle, every endpoint's p95 is 3–26 ms. Across all four bench runs: cold scan 10.4–11.4 s (`tmp/e2e/{bench-run2,bench-run3,final-run,final-run2}.log`), p95 during the scan 33–38 ms†, idle p95 10–24 ms†.

† The API latency figures (this table's API rows, the per-endpoint table, the clone column's rescans and both RSS rows) are not traceable. They were in `flux-e2e-bench.json`, which the reverify run of `da8801f` overwrote on 2026-10-20, and the logs don't print them. The logs keep the cold-scan walls, first items, the as-found rescans and startup. The research target is API p99 < 50 ms. I measured p95, not p99: 10–24 ms at idle and 33–38 ms during a cold scan, with occasional maxima of 58–109 ms on this shared machine.

**Startup** (spawn of `nice -n 19 flux-server` → first 200 on `/api/v1/server`, polled every 2 ms):

- full catalog (≈14.8k media files), 5 runs: p50 **12.6 ms**, max 15.1 ms;
- empty data dir, 3 runs: 19.6–21.7 ms;
- idle RSS 3 s after starting on the full catalog: 52–54 MB (the catch-up Quick scans have run by then).

The very first launch of a newly built binary took ~1.1 s (macOS assessing a new executable; also seen in server.md).

**RSS in the product walk** (`ps` every 500 ms):

| Point | RSS |
|---|---|
| Start | 15 MB |
| After the first-run scan | 160 MB |
| End of the walk | 244 MB |
| Peak | 285 MB |

`/usr/bin/time -l` reports 294 MB, but that figure is the largest of the server and every ffmpeg it ran.

The memory after a scan is mostly the catalog's own SQLite caches: the writer's 64 MiB, 4 readers × 16 MiB, and touched mmap pages, plus the prepared-asset cache (256 MB budget). A restart on the same catalog sits at 53 MB. It is not a leak, but it is the footprint to tune (§5).

Files: `tmp/e2e/final-run2.log` (this run's scan, rescans and startup; `bench/results/flux-e2e-bench.json` now holds the reverify run of `da8801f`), and `bench/results/flux-e2e-product-final.json` (the product walk, `4996b79`).

## 5. Remaining gaps, by priority

**P1: worth doing next**

1. **H.264 in MPEG-TS/M2TS (and AVI) is always video-transcoded.** flux-hls never remuxes these containers in-process: `can_index` is false, so `NoIndex` → transcode.
   - 1,774 of the library's files are H.264 TS; real libraries have Blu-ray M2TS rips and DVR recordings.
   - Cost: first frame 0.79–0.83 s and cold seeks p95 1.2–1.7 s. The planner estimates ~12.3 s of CPU per minute of video, against ~60 ms for a remux.
   - Owner: flux-hls. Copy H.264/HEVC from TS into fMP4 using the ffprobe index's keyframes.
2. **Art arrives in scan order, not where the user looks.**
   - On a first run the queue works through all 2,998 films (about 0.45 s each, at nice 19, paused during playback and scans) before any TV show, Features or Research title.
   - Home's hero and "Recently added" stay artless for an estimated ~25 minutes (2,998 × ~0.45 s, plus pauses). In the walk, 68 films had posters after 4–5 minutes (jobs pause during the many plays).
   - Proposal (catalog/server): claim art jobs newest-added first, and bump an item when the UI requests its art.
3. **Server memory.**
   - Idle at 15 MB, 120–160 MB after a cold scan, ~245 MB at the end of the 5-minute product walk, peak 285 MB. The sources are SQLite caches (writer 64 MiB + 4 × 16 MiB + mmap) and the 256 MB asset-cache budget.
   - Still below Jellyfin's idle (344 MiB after start, 476–483 MiB after a full scan; benchmarks.md), but "lean" wants smaller defaults: e.g. a writer cache of 16 MiB and readers of 4 MiB, with `cache_size` restored after a scan.
   - *2026-10-20:* after Phase D's checkpoints and Phase E's memory work (jemalloc, idle release), flux idles at 65–81 MiB RSS, 22 MiB phys footprint, after a full scan and 9 rescans (benchmarks.md §5.2).
   - Owner: catalog/server; needs a quiet-machine measurement.

**P2**

4. **Audio switching restarts the session.**
   - It takes a new `/play` (~170 ms to the new first frame), and a direct-play file moves to HLS when you pick a track Chrome can't decode.
   - `PlayResponse.audio` already lists the HLS renditions, so hls.js could switch within the session (web).
5. **ASS subtitles show as plain WebVTT** (no styling); PGS needs burn-in. This was a known web gap and is confirmed here.
6. **Scan counters read oddly on a first scan.** TV: "12,658 added, 465 updated" for 10,949 files: items include seasons and shows, and "updated" counts parents touched by later batches. Copy or counter semantics (scan/web).
7. **Version picker names.** Two 2160p versions both read "2160p" (Main10: E-AC-3 MKV vs AAC MP4). The container or audio should be part of the name (web).
8. **`GET /libraries` is the most-called endpoint and the slowest small one** (p50 ~6 ms). It counts every file of each library on each call. It is fine at 15k files and linear beyond; a cached count would do (catalog).
9. **Episode pages without art** keep an empty poster column on the left (layout), until thumbnails arrive.

**P3 / test coverage**

10. **HTTP/1.1 only.** Chrome opens 6 connections per host, and art resizes, API calls and media share them. I suspected this while chasing the slow grid (fix #2), but the trace didn't show it: API requests waited ≤ 1 ms for a connection while art jobs ran. HTTP/2 on the local listener would remove the risk (flux-net).
11. **The corpus has no 10k+ grid.** flux-names files episodes under their show whatever the library kind (a "TV as films" library still has 300 items). The 12k grid was measured on synthetic clones, all with one seed's art and metadata.
12. **Chrome only, headless.** Safari's native HLS and Firefox were not driven end to end here (flux-hls's own harness covers Firefox). There is no real TV browser, and no tailnet path (rules).
13. **No TMDB key**, so nothing is matched. "Matched as" shows only parser evidence (40–50 %, "Low"), and provider art is never exercised.
14. **Fresh-context TTFF excludes the hover-less cold start.** A real first visit pays the Player chunk, hls.js and Chrome's media pipeline: 281 ms for the walk's first play.

**Proposed changes for other owners** (not made here):

- **flux-hls:** an in-process TS/M2TS remux for H.264/HEVC (P1 #1).
- **flux-catalog / flux-server:** art jobs newest-first plus a UI-request bump (#2); smaller default SQLite caches (#3); a cached per-library file count for `/libraries` (#8).
- **flux-net:** HTTP/2 on the local listener (#10).
- **web:** in-session audio rendition switching (#4), ASS rendering (#5), and version names (#7).
