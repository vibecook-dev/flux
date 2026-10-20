# Audit of published numbers

*2026-10-20, on `main` from `5c08ab3`.* This is an adversarial check of every number in `README.md` and `docs/prototype/benchmarks.md`, and of the headline numbers in `e2e.md`, `fixes.md`, `reverify.md`, `hls.md`, `scan.md` and `server.md`, before the maintainer quotes them publicly. **No new benchmark was run.** Every number was settled from existing result files and logs.

## Method

- **Trace.** Each number was matched to a result file in `$FLUX_WORK/bench/results/` (paths below are relative to it unless they start with `tmp/` or `media/`), or to a documented command output. For benchmarks.md this means the summary `flux-e-bench-summary-20261020-043247.json` (**S** below) and its 17 run files. Every derived figure was recomputed from the run files, not the summary: the pooled first starts, the pooled far seeks, per-run cells and load ranges.
- **Arithmetic.** Every ratio was recomputed as Jellyfin's median ÷ flux's median from the unrounded values, and every range from its samples.
- **Caveats.** For each number: is the machine, load, corpus and warm/cold state stated where the number is?
- **Flags.** Comparative claims with no measured baseline (Plex), "faster than" claims that rest on a configuration difference, and statistics that present a best case as typical.

**Status:**
- **ok:** traced and correct.
- **fixed:** corrected in the doc.
- **marked:** can't be traced; kept, but flagged in the doc with the reason.
- **removed:** no source exists, so it was taken out.

Commits: `bd50a32`, `5d0b258`, `91734c8`, `943d517`, `1e8f787`, `ed5220d`, `0a0dc7f`, `823925b`, `be45e81` (plus this file).

## What mattered

1. **Jellyfin's cold scan moves by up to 15 % between sessions** at similar load: 900 s on 2026-10-19, 994 s in the review, 1,047–1,063 s here. The headline 326× uses the slowest session. Against the earlier ones it is ≈ 280–310×. Now stated in the README and in benchmarks.md's "Read this first".
2. **The README's TTFS row hid the long-TS case.** Its 67× top ratio is also the TS file's TTFS ratio, but that file's 133 ms index build sits in `/play`. The README now also gives play + first segment (1.5–50×).
3. **"Cold seek" meant a segment the server had not produced before, on a warm page cache.** That is not a cold disk. Both the README and benchmarks.md now say so.
4. **Stale Jellyfin baselines were still printed as current** in server.md's summary table, hls.md §6 and scan.md's results table. They came from the loaded 2026-10-18 single run with a fixed file order (n = 1; h264_multi was the first play after start). They are replaced or labelled.
5. **Result files were overwritten**, so some figures lost their source:
   - e2e.md's bench and playback JSONs were overwritten by the reverify session's `pnpm e2e` run.
   - hls.md's Phase C `flux-hls-validate.json` was overwritten.
   - The logs behind scan.md's three earlier Phase C runs are gone.

   Figures that still trace to logs were re-pointed. The rest are marked.
6. **Configuration difference:** e2e.md set flux's *direct-play* browser click (50 ms) beside Jellyfin's *server-side remux* TTFS. It also labelled flux's public-endpoint restart (12.6 ms) as "API ready" against Jellyfin's authenticated 5.02 s. Both are fixed.
7. **Pooled or mixed statistics:**
   - hls.md's "1.1 ms p50 / 2.1 ms p99 per segment" pools 1,229 segments, 1,199 of them from one 360p file. The 4K file's segments took 90 ms p50.
   - Its transcoded-audio range mixed two runs.
   - Its Phase D table dropped a seek that ran against the claim.
   - scan.md's "2–4× sooner" was 1.6–4.6×.
8. **Plex:** no number anywhere compares with Plex. The README's only Plex lines are the stated aim and the "No Plex baseline" caveat (ok).

## README.md

| Number | Source | Status | Fix |
|---|---|---|---|
| 16,671 files, 14,760 media | `media/library.manifest.jsonl` (10,949 episodes + 3,456 movies + 261 samples + 94 extras; 1,911 sidecars) | ok | |
| "7 seed clips (6 s each), cloned 16.7k times" | `media/seeds/` (ffprobe: 6.00–6.04 s), manifest | fixed | Only the 14,760 media files are clones; the other 1,911 are sidecars |
| Cold scan → browsable 3.24 s / 1,055 s / 326× | S `scan.library.*.cold_scan_wall_s` (3.238, 1054.90); 325.8× | ok | Caveat added: 900 / 994 s in earlier sessions → ≈ 280–310× |
| Cold scan → all work finished 10.19 s / 1,055 s / 104× | S `cold_scan_quiescent_s` (10.187, 1055.14); 103.6× | ok | |
| Unchanged rescan 142 ms / 19.7 s / 138×; 0 files opened, catalog untouched | S `unchanged_rescan_wall_s`; run files `scan_counters.media_files_opened` = 0 ×9, `db_and_wal_unchanged` 9/9 | ok | |
| Add 50 → listed 122 ms / 21.4 s / 175× | S `add_rescan_wall_s` (= `wall_until_count_s`) | ok | |
| Restart → API usable 20.7 ms / 5.02 s / 243× | S `restart_to_ready_s` (0.0207, 5.022); 242.6× | ok | |
| TTFS remux, 6 files: 2.8–17.9 ms / 176–617 ms / 25–67× | S `playback.*.per_file.*.ttfs_s`, per-file ratios 24.7–67.4 | fixed | Labelled "after the play request". New row: play + first segment 4.1–136 / 187–634 ms, 1.5–50×. The 67× top is reached by the TS file, whose cost sits in `/play` |
| Far seek in a 2 h file 1.3 ms / 624 ms / 480× | Pooled from the 6 playback run files: 18 + 18 seeks, medians 1.3 / 623.6 ms, 479.7× | fixed | The label said "Cold seek". It now says the segment was not produced before and the page cache was warm |
| Peak RSS 100 / 544 MiB, 5.5× | S `lifetime_peak_rss_mb` (99.6, 543.95) | ok | |
| Idle 67 / 22 vs 480 / 353 MiB, 7.2× / 16× | S `idle_rss_after_scans_mb`, `idle_footprint_after_scans_mb` | ok | |
| Install 23.7 + 1.2 MiB vs 238 MiB, 9.6× | S `footprint` bytes (24,803,832 stripped; 1,305,030; 249,887,929) | fixed | Added 7.0× as built (32.6 MiB unstripped: the release profile keeps `debug = 1`) |
| TrueHD 113 vs 395 ms (3.5×); long TS 136 vs 205 ms (1.5×) | S per_file `h264_truehd71`, `seek_2h_360p.ts` | ok | |
| M1 Max 64 GB; load 3.4–11.2 | S `machine`; every load field in the 17 run files: 3.42–11.21 | ok | Caveat added: playback was page-cache-warm |
| 3 flux / 2 Jellyfin full runs; n = 3 per file | S `runs` | ok | |
| "Both ran with metadata, jobs and watching off" | benchmarks.md §3–§4, harness flags | added | The configuration was stated in benchmarks.md, not in the README |
| 12,000 titles; 36/36 steps | e2e.md; `flux-e2e-product-final.json` (36/36; also 36/36 in the reverify run) | ok | The 12k grid is synthetic clones (e2e.md says so) |
| 37 defects, all fixed, 5 hardened | review.md (7 + 7 + 14 + 9), reverify.md (32 held + 5 hardened) | ok | |
| rust-version 1.85, developed with 1.97 | `Cargo.toml`; `rustc 1.97.1` | ok | |
| ffmpeg 9.0.2; pnpm 11.13.1; port 8096; 127.0.0.1 | `ffmpeg -version`; `web/package.json`; `flux-server/src/config.rs` | ok | |
| TMDB 6-month cache limit | research/09 row 4 (terms of use, downloaded) | ok | |
| Plex | none | ok | No Plex number exists; the caveat says so |

## benchmarks.md

| Number | Source | Status | Fix |
|---|---|---|---|
| macOS 26.5, M1 Max 64 GB | S `machine` (26.5.2) | ok | |
| Load 3.4–11.2; every phase started < 10 | All load fields of the 17 run files | ok | |
| "Four phase-end readings were 10.3–11.2" | `…scan-jellyfin-library-20261020-034634.json`: `add_rescan[0]` 11.21, `add_rescan[1]` 10.31 | fixed | Two readings. Each is stored twice (`load1[1]` and `end.load1_at_end`), which made four |
| Load gate waited once, 30 s | S `gate.load_wait_s` = 30 (Jellyfin full) | ok | |
| 03:20–04:32 local | run files `started` / `finished` | ok | |
| Jellyfin cold scan 900 / 994 s in earlier sessions (new caveat) | `flux-bench-scan-jellyfin-library-20261019-082205.json` (899.6 s; cold-scan load 3.3→5.8), `flux-review-scan-jellyfin-library-20261019-203948.json` (994.3 s; 5.6→7.5) | added | ≈ 280–310× to browsable against those |
| §1, every row of the summary table except the three below | S; per-run cells checked against the 5 full + 6 tiny + 6 playback run files | ok | Includes the CPU split 782 + 503 s (766.8 + 501.6, 797.7 + 503.8), disk 131.5×, Jellyfin rescans 657–681 MiB, DB 26.7 + 0.34 vs 82.0 + 18.2–36.7 MiB |
| First start 21 ms (20–66, 6) vs 6.27 s (6.02–7.58, 5), 294× | Recomputed: flux 19.5, 19.7, 20.4, 22.2, 59.1, 66.4 → 21.3 ms; Jellyfin 6.022, 6.115, 6.265, 7.133, 7.581 → 6.265 s | ok | |
| Far seeks 1.3 ms (0.6–1.7, 18) vs 624 ms (459–762, 18), 480× | Recomputed from the 6 playback run files | ok | Row label now defines "cold" (segment not produced before; page cache warm) |
| library-tiny touch 14 ms vs 3.85 s, 275× | S `touch_rescan_wall_s`, `touch_rescan_quiescent_s` 1.09 s | fixed | Added "3.5× to quiescent", as the full-library row already does |
| Install 9.6× | S footprint | fixed | Added 7.0× as built |
| Long TS read "455 MiB" (×2) | `stat` of `play-lib/seek_2h_360p.ts`: 454.4 MiB | fixed | 454 MiB |
| "≈ 1 s on a 500 MB/s SATA SSD" | Arithmetic: 476 MB ÷ 500 MB/s | ok | Labelled as arithmetic, not measured |
| Jellyfin m2ts 30 s seek: 404 in 3 of 3 runs | S `seek_errors` | ok | |
| Research targets table | S | ok | The watcher figure "181–193 ms" was fixed: it rested on scan.md runs whose logs are gone; now 193 ms (full) and 179 ms (`flux-d-scancat-library-tiny.json`) |
| §2 corpus: 16,671 / 14,760; 956 / 657 / 289 / 9 junk; seeds 1.39–1.81 MB, 6 s | manifest; `ls -l media/seeds`; ffprobe | ok | "cloned 16.7k times" fixed as in the README |
| Clone vs real copy: 0.181/0.189 vs 0.170/0.188 s; Movies 1.958/1.915 vs 1.973/1.975 s | `tmp/r-bench/results/SLOT-SUMMARY.md` (Movies from step logs 29/31–33) | ok | |
| Jellyfin per file: 94–101 ms (real) vs "74 ms" (clips) | SLOT-SUMMARY: 4.59 / 4.95 s for 49 files; 29.12 / 34.25 s for 393 | fixed | The clips cost 74–87 ms per file, not just the faster run's 74 |
| §3: 47–56 writer commands; 503 s ffprobe CPU ≈ 34 ms/file; 2,998/300 vs 3,149/300/10,949; 8 Jellyfin runs with no extraction; 3 chapter / 6 keyframe rows; 14 of 18 tasks with triggers | run files (`scan_counters`, `usage`, `evidence.extraction`, `scheduled_tasks`) | ok | |
| "Everything else off" 26.2/31.5 vs 29.1/34.3 s | SLOT-SUMMARY | ok | |
| §4: rusage 0 vs device 0.5–1.0 MiB (tiny unchanged); 87 vs 127–132 MiB | run files `writes.device`, `usage` | ok | |
| "The write baselines were 0 in every phase" | `…scan-flux-library-tiny-20261020-041916.json` cold scan: `idle_2s_before.write_mb` 3.1 (546 ops) | fixed | 0 in every phase but that one; its 3.2 MiB may include another writer |
| Foreign reader ≈ 18 MiB/s (34–38 MiB per 2 s) | `idle_2s_before.read_mb` 32.5–38.1 when active | ok | |
| §5.1 run list, start times, load ranges | run files | ok | |
| §5.2, every per-run cell (≈ 110) | the 5 full-library run files | ok | Add rescans use `wall_until_count_s` (88 ms, not the 87 ms `wall_s`), as the doc says |
| §5.2 notes: 3.1 vs 0.8 GiB read; 4.5 → 6.5 s CPU; ~1 s / ~10 ms touch re-fingerprint; 657–704 MiB per Jellyfin rescan | run files `usage`, `usage_until_quiescent` | ok | |
| §5.3, every cell | the 6 tiny run files | ok | |
| §5.4, every cell (TTFS, play, ratios, first-play table, seeks 370–780× / 390–520×, 409×, CPU 1.2–1.5 s in children, footprints 55–58 / 299–306) | the 6 playback run files | ok | |
| TrueHD video leg 3.2 ms, audio 104–113 ms server time | `audio_seg0_server_ms` 104.1 / 112.8 / 109.4 | ok | |
| §5.5, every cell; first start bimodal 5 of 9 / 4 of 9 | run files | ok | |
| Jellyfin migrations ≈ 2.5 s, ffmpeg probes ≈ 3.5 s | `review/findings-bench.md` checked_ok | ok | |
| §5.6 | S footprint, run files | ok | |
| §6: 22.8 ms / 285× → 44.5 ms / 146×; 160× → 132×; 73×; 628× / 119×; 6.86 / 6.97 / 899.6 s; 127–132 MB idle; 1,442 / 19.6 / 17.3 s; 11.6 s | findings-bench.md BENCH-2/3/5/6; `flux-bench-summary-20261019-083926.json` | ok | |
| §6: "176 frame grabs, about 1–2 % of Jellyfin's scan" | BENCH-7 ("not measured … estimated") | fixed | Labelled as an estimate |
| §6: 2026-10-19 Jellyfin "at load 2.5–3.7" | `flux-bench-scan-jellyfin-library-20261019-082205.json`: 2.49–5.83 | fixed | 2.5–5.8 (3.3 → 5.8 over the cold scan). The review's and this run's loads are now given over the cold scan too |
| §7: about 75 min; about 24 min per Jellyfin full run | 03:20–04:32 (72 min); 23.6 min | ok | |
| §8: fingerprint pass about 7 s after a 3 s scan | 10.19 − 3.24 | ok | |

## e2e.md (headline table, §4)

| Number | Source | Status | Fix |
|---|---|---|---|
| Direct-play click → first frame 50–51 ms vs "server-side TTFS 176–617 ms on remux files" | `tmp/e2e/final-run2.log` | fixed | Configuration difference: a direct-play click vs a server-side remux. The comparison is removed from that row |
| HLS click → first frame 157–161 ms | final-run2.log | ok | Jellyfin cell now says the browser was not measured |
| Transcode first play 0.52–0.83 s | final-run2.log | ok | |
| Seek p50 / p95 97–113 / 146–316 ms | final-run2.log (runs 2–5 pooled) | ok | Labelled browser vs Jellyfin's server-side figures |
| Transcode first-play seek p95 1.2–1.7 s | final-run2.log | ok | Added the AVI case (96 ms) the range left out |
| Cold scan "10.2–11.4 s (8 runs)" | `tmp/e2e/{bench-run2,bench-run3,final-run,final-run2}.log`: 10.357–11.352 s | fixed | 10.4–11.4 s; no scan below 10.36 s exists |
| First items 87–126 ms | same logs | ok | |
| Unchanged rescan "29–176 ms" | BENCH-2 read 32–176 ms from the final run's JSON before it was overwritten | fixed | 32–176 ms (final run) |
| Browse API p95 33–38 ms, idle 10–24 ms | was in `flux-e2e-bench.json`, overwritten by the reverify run of `da8801f` | marked | † note. The overwriting run measured 45.8–46.4 ms p95 during its scan |
| "Spawn → API ready 12.6 ms" | final-run2.log (`/api/v1/server`, p50 12.6, max 15.1) | fixed | It is the public endpoint. Relabelled; the like-for-like pair 20.7 ms vs 5.02 s is added |
| RSS "15 / 119–160 / 285 MB", restart 53 MB | `flux-e2e-product-final.json` (14.9 / 160.1 / 284.8 MB); logs' restart RSS p50 51.9–53.3 MB | fixed / removed | "119" has no source: removed. 15 / 160 / 285 MB; restart 52–53 MB |
| Before/after the Play fix: 323–324 → 50–51; 427–446 → 157–161; 343–431 → 70–158 ms | `tmp/e2e/playback-run3.log` and final-run2.log | ok | The logs give 344–430; the difference is ±1 ms rounding |
| §4.1 per-case and server-side TTFS tables | final-run2.log | ok | "Files:" re-pointed from the overwritten JSON to the log |
| §4.2 API rows, per-endpoint table, clone rescans, RSS rows | overwritten JSON | marked | † note |
| §4.2 wall 10.6 / 10.7 s, first items 121 / 91 ms, as-found rescans, startup | final-run2.log | ok | |

## scan.md

| Number | Source | Status | Fix |
|---|---|---|---|
| Cold scan "7.7–23.4 s (4 runs)" | Only the latest run has a file (`flux-scan-library-20261019-025811.json`: 23.4 s; clone 12.4 s). The three earlier runs' logs are not under `tmp/scan/` | fixed / marked | The headline now leads with the traceable run; the earlier runs are marked † |
| 7.2 s without fp128; "0.8–1.4 s in 3 quieter experiments" | latest file (7,175 ms); the earlier figures have no source | ok / marked | |
| First items 65–526 ms; unchanged 128–151; add 96–127; touch 88–132; watcher 181–193 (p90 ≤ 491) | latest file: 71–253; 128; 109; 132; 193 (max 220) | fixed / marked | Latest values given; the earlier runs marked |
| Peak RSS 97–114 MB | latest file (97.0–113.4; 113.4 is the no-fingerprint phase) | ok | |
| "fp128 makes a full cold scan 2–9× slower" | latest file: 23.4 or 15.8 s vs 7.2 s | fixed | 2–3× traceable; the 9× end rested on the lost logs |
| Results table: "Jellyfin: 520 / 362 MB", "Jellyfin 85 MB" | `jellyfin_baseline` in the latest file (the 2026-10-18 loaded run) | fixed | benchmarks.md: 539–549 MiB peak, 82 MiB DB |
| Phase D: "the scan finishes 2–4× sooner" | `flux-d-scancat-scan-20261019-220926.json`: base 7.9 s vs new 4.8 / 1.7 s | fixed | 1.6–4.6× |
| Phase D: 10.9/7.9/6.7, 4.8/10.4/1.7, 10.0/8.7/2.5 s; fingerprints 14.9/21.2/8.2 s; RSS 136–144 vs 109–113 MB; first items 71–140 vs 85–104 ms | same file | ok | |
| Phase D watcher 179 / 169 ms | `flux-d-scancat-library-tiny.json` | ok | |

## server.md

| Number | Source | Status | Fix |
|---|---|---|---|
| flux `/play`, TTFS, TTFS + audio, seeks (summary table) | `flux-server-playback-20261019-094039.json` | ok | |
| Jellyfin TTFS 327/521/304/913/191 ms; seeks 318/840/12–701 ms | `jellyfin-playback-20261018-224518.json` (one loaded run, fixed order) | fixed | benchmarks.md §5.4's medians (189/395/199/617/195 ms; 131 / 2,027 / 487–698 ms); the old figures are noted |
| Startup 15 ms (run 1: 277 ms), restart 10.3 ms, RSS 13 / 79.9 MB | same file `startup` | ok | |
| Phase E table (every cell) | `flux-e-memory-summary-20261020-030649.json` | ok | |
| Phase E result "45 MB … 8 MB (target ≤ 50)" | same | fixed | These are phys footprint. RSS is 85 / 51 MB; the target is met by footprint only (as benchmarks.md says) |

## hls.md

| Number | Source | Status | Fix |
|---|---|---|---|
| Phase C: Chrome 51/52, Firefox 52/52 | `flux-hls-browser-{chrome,firefox}-20261019-043118.json` | ok | |
| Phase C: transcode TTFS 0.99–1.66 → 0.55–0.68 s; seeks 1.96–3.66 → 0.83–1.20 s | `flux-hls-vtranscode-ttff.json` | ok | |
| Binary index 25 % of JSON, decode 6.5× faster | `flux-hls-index-encoding.json` (24.1 / 3.70 ms) | ok | |
| hls-validate 72/72 (first prototype), 123/123 (Phase C) | `flux-hls-validate.json`, since overwritten (2 rows) | marked | Noted; superseded by 183/183 and 213/213 |
| §1 / §6 flux TTFS, prepare, seeks; 0.98 ms CPU/MB; 973 MB; 69 MB peak RSS | `flux-hls-playback-20261019-074407.json` | ok | |
| §1 / §6 Jellyfin column (327/521/304/913/191 ms; seeks; ≈ 370 / 361–383 MB) | 2026-10-18 single loaded run, fixed order | fixed | Labelled, with benchmarks.md's like-for-like medians |
| Video segment p50 1.1 / p99 2.1 ms | same file, `sweep` pooled over 1,229 segments (1,199 from the 360p file) | fixed | Per file p50 is 0.6–2.1 ms, except the 4K file's 41 MB segments (90 / 119 ms) |
| Transcoded audio "87–136 ms p50, 125–341 ms p99" | mixes `-074407` (112–121 / 187–341) with `-115615` (88–137 / 123–339) | fixed | The cited run's values; the other run named |
| Phase D: TTFS 0.34–1.30 s → 0.7–13 ms; seeks 0.40–1.43 s → 0.5–12 ms; index 116–1016 → 6–108 ms | `flux-d-hls-perf-ttfs-20261019-220437.json` (all 5 rows) | ok / fixed | The table left out the short files' 25 s seek, which was faster before (0.4–29 ms, an already-produced piece) than after (4.1–10.3 ms). Now stated |
| 2 h TS index: 27 s first read (≈ 213 MB/s), 0.86–1.01 s cached, ffprobe 4.61 s | `flux-d-hls-perf-index-20261020.json` | ok | The file notes that the first read was "partly cached" |

## fixes.md and reverify.md

| Number | Source | Status | Fix |
|---|---|---|---|
| 599 tests (451 + 45 + 32 + 30 + 29 + 12) | gate command output; arithmetic checks | ok | |
| hls-validate 213/213, 71 files × 3 presets, 18 min at load 9–13, 1,836,498 frames, 141 I-frame playlists, 45 TS cases, index methods (Cues 84, Moov 48, TsScan 42, FfprobeScan 27, Fragments 9, ClusterScan 3) | `flux-hls-validate-20261020-061447.json` | ok | |
| 183/183 | `flux-hls-validate-20261020-044028.json` | ok | |
| Smoke 27/27, restart 23/23 | `flux-merge-d-smoke.json` | ok | |
| Mille 450 tests; Truffle 756 + 99 | agent reports (other repositories) | ok | Documented output, not re-run |
| reverify: 32 held + 5 hardened = 37; 603 = 599 + 4; product 36/36 | reverify.md; `flux-e2e-product.json` (36/36, `da8801f`) | ok | |
| reverify: M2TS "TTFF 137 ms" | that run's record was replaced; the re-run's `flux-e2e-product.json` shows 133 ms | marked | Noted |

## Flags

**No measured baseline.** Plex has none, and nothing claims one. "Lean, fast and private alternative to Plex and Jellyfin" (README) is an aim, not a measurement. docs/ARCHITECTURE.md's "aims to beat Plex" is the same.

**Configuration differences behind a "faster than":**
- Direct play vs remux (e2e.md): fixed.
- Public endpoint vs authenticated API (e2e.md): fixed.
- Stripped vs as-built binary (install ratio): both now given.
- flux's scan "done" excludes the deferred fingerprint pass: already stated, with both end points.
- Jellyfin probes every file with ffprobe and reads NFOs/artwork: stated, and the review's "everything else off" run shows it is not the cause.
- Both servers ran with metadata, jobs and watching off: now in the README too.

**Cherry-picks:**
- The clip per-file cost used the faster run: fixed.
- The README showed TTFS without the plan request: fixed.
- hls.md's pooled per-segment time: fixed.
- hls.md's dropped seek: fixed.
- Jellyfin's slowest-session cold scan behind 326×: caveated.

None was a best run presented as the median. Every benchmarks.md figure is a true median, with its range and n.

## Out of scope, still stale

- research/00-synthesis.md and research/09-fact-check-and-gaps.md still quote the 2026-10-18 Jellyfin baseline (BENCH-2). benchmarks.md §6 says it is replaced.
- catalog.md, probe.md and integration.md were already corrected in `0e77f76`.

## Environment

- No benchmark, server, Jellyfin or browser was started. The only heavy step was the closing gate (`cargo build`/`clippy`/`test` at `nice -n 19 -j 3` in `target/e-audit`, `cargo fmt --check`).
- The light reads were `ffprobe` of the 7 seed clips and the JSON/log reads.
