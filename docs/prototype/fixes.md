# Phase D merge

*2026-10-20. Merged the four Phase D fix branches into `main` (which already had the web agent's commits), fast-forwarded the Mille and Truffle dogfood branches, and wired the seams between them. `CARGO_TARGET_DIR=$FLUX_WORK/target/c-e2e`. Every heavy step ran at `nice -n 19`, `-j 3`, on the shared M1 Max (1-minute load 7–17 during the runs).*

The findings themselves, with their final status and regression tests, are in [review.md](review.md).

## Summary

- **All four branches merged** with `git merge --no-ff` in the order scancat → hls-correct → hls-perf → server. Only the two flux-hls branches conflicted, at the expected hook points (`INDEX_VERSION`, the ffprobe fallback, `hls.md`). Two struct literals and one exhaustive match needed the other branch's new fields.
- **Mille and Truffle:** `flux/dogfood` fast-forwarded to `flux/dogfood-d` in both (`2c064e8`, `0d2415c`); their `-d` worktrees are removed, the branches kept.
- **Seams wired:** every item the task listed, plus the proposals that were contained (§2): the SEC-2 policy conflict between the scan and server branches, ROB-1's shutdown wiring, streamed WebVTT (PB-7), weak ETags (PB-11), MPEG-TS transcodes fed from the keyframe, Truffle protocol 10 in flux-net, Mille's stateful coalescing and FSEvents resume in flux-scan.
- **One pre-existing bug found by the smoke run and fixed:** transcoded pieces of ffprobe-indexed sources (AVI) didn't start on their IDR (§3).
- **Gate:**

  | Check | Result |
  |---|---|
  | `cargo build --workspace --all-targets` | ok, 0 warnings |
  | `cargo clippy --workspace --all-targets` | 0 warnings |
  | `cargo clippy --no-default-features` for flux-net, flux-names, flux-hls | 0 warnings each |
  | `cargo test --workspace` | **599 passed**, 0 failed, 4 ignored |
  | `cargo fmt --all --check` | clean |
  | `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` | clean (after one link fix) |
  | web: typecheck, lint, test, build | ok; vitest 130/130; 317 modules |
  | golden-rs `cargo check --locked` | ok |
  | server smoke run, release build | 27/27 checks, then 23/23 after a restart |
  | hls-validate, 61 corpus + 10 TS files × 3 presets | **213/213 ok**, packet counts equal the source's in every case |

## 1. Merge

| Branch | Commits | Worktree state | Conflicts |
|---|---|---|---|
| `phase-d/scancat` | 8 | clean | none |
| `phase-d/hls-correct` | 5 | clean | none |
| `phase-d/hls-perf` | 6 | clean | `index/mod.rs` ×2, `docs/prototype/hls.md` |
| `phase-d/server` | 3 | clean | none |

Merge commits: `da4e2bb` (scancat), `3602984` (hls-correct), `861410f` (hls-perf), `7a05750` (server). No branch was skipped: every agent reported "complete".

**Resolutions** (in the hls-perf merge commit):

- **`INDEX_VERSION`:** both branches bumped 3 → 4 independently (hls-correct: `TrackExtra`, measured rates, `cues_pos`; hls-perf: `Layout::Ts`). Now **5**, so every index cached by either branch's build is rebuilt once.
- **The ffprobe fallback** (`PlaybackIndex::build_with_fallback_limits`): hls-correct's bounded `external::build_with_limits` plus hls-perf's `NO_REMUX` warning, which the planner reads.
- **`hls.md`:** both "Phase D" sections kept, correctness first.
- **Compile fixes:** the TS index's `IndexTrack` literal gained `extra`; it is filled (the SPS sample aspect ratio, and the video end, which for TS is the duration). `Asset::video_frame_ticks` (hls-correct) gained the `Layout::Ts` arm (hls-perf): TS frames are predicted from the grid, like ffprobe-indexed sources. The other auto-merged hunks in `asset.rs` and `planner.rs` were read and needed nothing.

**The test count reconciles exactly:**

| | Tests |
|---|---|
| `main` before the merge (`baa81bb`, the web agent's gate) | 451 |
| + scancat / hls-correct / hls-perf / server | +45 / +32 / +30 / +29 |
| + merge seams: `flux-server/tests/seams.rs` 5, `flux-hls/tests/merge_seams.rs` 4, flux-net 1, flux-scan `watch.rs` 1 (Mille's patch), `vtranscode` unit 1 | +12 |
| **Total** | **599** |

The 4 ignored tests are opt-in by design (live Ollama, live TMDB, this machine's tailscaled, the probe's deep fuzz).

## 2. Seams wired

| Seam | Asked for by | What `main` does now | Test |
|---|---|---|---|
| **ffprobe for the scan** (ROB-3) | scancat | `scan::start` builds `ScanConfig { ffprobe: cfg.ffprobe, .. }`: the binary the server resolved. | `seams::the_scan_fallback_runs_the_servers_ffprobe` (a fake ffprobe's 12.5 s, not the real 2.0 s) |
| **Overlapping roots** (INT-7) | scancat | `CatalogError::OverlappingRoots` → 400 with a sentence naming both folders. | `seams::overlapping_roots_are_a_bad_request` |
| **Art on demand** | scancat | `GET /items/{id}` of an item without a poster (or a movie/show without a backdrop) calls `Writer::bump_art` (fire and forget) and wakes the job loop (`Activity::wake_jobs`). | `seams::opening_an_item_without_art_moves_its_art_job_up`; smoke: art job priority 40 → 0, its match 20 → 0 |
| **Cached library counts** | scancat | `GET /libraries` uses `read::list_libraries_cached` inside the same read that snapshots the live scan status first, and `/libraries/{id}/scan` uses `Catalog::library`. The writer invalidates the counts after its commit and before the scan can report "finished", so the server branch's ordering (EXTRA-libraries-live-status-first) still holds. | `guard::libraries_read_the_live_status_before_the_counts` (unchanged, passes) |
| **Live `last_seen`** | scancat | `ScanService::overlay_roots` (default no-op; `FluxScanner` calls `Scanner::overlay_roots`) on `GET /libraries` and every live-status response. | smoke: "roots carry last_seen" |
| **SEC-2 policy** | scancat vs server | The two branches disagreed: server = strictly in-root; scancat = external links are supported, check the recorded identity instead. Now: inside the library → served as the server branch does (fd identity vs the resolved path); outside → served only if the opened file *is* the one the scan recorded (inode, size, ns mtime; not the device, which a NAS remount renumbers). Applies to `/media/{asset}/file` and to `play::location` (play, plan, HLS loads). | `play::files_outside_the_library_are_not_served` gains an external link: served while it points at the scanned file, 404 once re-pointed at the secret |
| **ROB-1 shutdown** | hls-correct | `HlsEngine::shutdown` calls `external::kill_running()`; `EngineConfig::index_scan_limits` (used by `prepare` and by the server's own index build); `main` ends with `rt.shutdown_timeout(2 s)`. | `merge_seams::engine_shutdown_kills_a_hung_index_scan`; smoke: SIGTERM with a hung play-time ffprobe exits in 5.1 s, no child left (was > 35 s, still running) |
| **Streamed WebVTT** (PB-7) | hls-correct | `HlsEngine::webvtt_body` → `Whole` (cached, sent with a length) or `Streaming` (header, then each cue as its block is read; chunked); the completed document is cached. `plan_webvtt_body` checks the plan's rendition. Extraction holds a read permit. | `merge_seams::webvtt_streams_cue_by_cue_then_comes_from_the_cache` (same bytes as the sorted whole-document path), `seams::subtitle_sidecars_stream_then_come_with_a_length` |
| **Body errors logged** (ROB-5) | hls-correct | A segment body that fails mid-stream is logged with its resource. | — |
| **Weak ETags** (PB-11) | hls-perf | `ETag: W/"plan/rest"` when `!HlsEngine::is_deterministic` (a video transcode's init and segments); `If-None-Match` compares weakly. | `seams::video_transcodes_get_weak_etags`; smoke: 304 on revalidation |
| **TS transcode input** | hls-perf | A run over an MPEG-TS source reads `-f mpegts -i pipe:0`, fed from `ts::video_input` from its first segment's keyframe through the segment after its last (open-GOP leading pictures come after the next keyframe in decode order), instead of `-ss` into the file, which lands on any PES. | `merge_seams::ts_hevc_transcode_keeps_every_frame`: the 4K HDR10 M2TS 719/719 (with `-ss`: "encoder stopped after 2 of 3 pieces") |
| **Truffle protocol 10** | truffle | `TailnetInfo::{key_expiry, control_server}`; `TailnetStatus::NeedsApproval { approve_url }` (the admin-console link on Tailscale's service, none on Headscale; seeded from `TS_CONTROL_URL` for older sidecars); `KeyExpiryKnown` updates a running status (`Shared::update_running`); `SidecarExited` → `Failed` with a sentence. The DTO's `key_expiry` finally has a producer. | `embedded::tests::network_events_set_approval_link_key_expiry_and_failure`, `to_api` mappings |
| **Mille adoption** | mille | Its validated patch applied cleanly on scancat's flux-scan: stateful coalescing (no raw-path hint workaround), the unsynced store-free snapshot, FSEvents history first in `StartupCatchUp::MilleResume` (then the snapshot, then a Quick scan; Quick above 256 folders), the next token saved once the catch-up is applied. `QuickScan` stays the default: tokens aren't refreshed while watching yet (Mille's step 3f). | `watch::restart_catches_up_from_fsevents_history_alone` (20 of 62 files walked) |
| **CSRF header** | server, web | `flux_core::api::REQUEST_HEADER`, re-exported by the guard; the 421/403 contract in the API docs; `tools/bench/flux_scan_bench.py` (and so `flux_playback_bench.py`) sends the header. | — |
| **CSP** | web | Nothing to change: the web agent kept ASS as WebVTT (ASS.js would need a raw-ASS route first, and no CSP change then). | — |
| **Docs** | all | Mille's, Truffle's and the web agent's Phase D reports are appended to `mille-changes.md`, `truffle-changes.md` and `web.md`, each with a merge note. `ARCHITECTURE.md` gains the Phase D invariants. | — |

## 3. Found during the merge

- **Transcoded pieces of ffprobe-indexed sources didn't start on their IDR** (pre-existing). The smoke run's server logged "transcoded piece does not start with a sync sample" for `mpeg4_mp3.avi`. Its source lacks a frame at 0.04 s, so the frame-grid counts of its first run's pieces were one too many, and each IDR forced at a piece boundary was the *last* sample of the previous piece: pieces 1–3 began on a P frame, so a seek into them decoded garbage for up to 2 s. Main had the same bytes (checked with the pre-Phase-D release build: first VCL NAL per piece `[5, 1, 1, 1, 5, …]`); it hid the problem by marking every piece's first sample sync. Fix (`2496475`): where piece counts are only predicted (ffprobe-indexed sources and MPEG-TS), a piece ends at the sync sample within 3 frames of its predicted count, and a run's last piece leaves out a frame read past the run's end. Now every piece starts on an IDR and the frame count matches the source (750/750, was 751). Tests: `vtranscode::tests::predicted_pieces_end_at_the_forced_idr_nearby`, `merge_seams::predicted_pieces_start_on_their_idr`.
- **rustdoc:** `SubtitleSource`'s doc (hls-correct) linked a private function; no branch had run `cargo doc` over flux-hls with `-D warnings`. Fixed in `f41c29d`.
- **Debug log:** a video run now logs its ffmpeg arguments at debug level; that is how the bug above was traced.

## 4. Dogfood branches

| Repo | Before | After | Gate the agent reported |
|---|---|---|---|
| Mille (`wt/mille`) | `1a96400` | `2c064e8` (6 commits) | 450 tests passed, clippy `-D warnings` clean (also Linux and Windows targets), fmt clean |
| Truffle (`wt/truffle`) | `ca1dd8e` | `0d2415c` (2 commits) | Rust 756 passed, Go 99 passed; clippy has 4 warnings, all pre-existing in truffle-napi files the agent didn't touch |

Both were `git merge --ff-only flux/dogfood-d`. `wt/mille-d` and `wt/truffle-d` were removed with `git worktree remove` (both clean); the branches `flux/dogfood-d` remain. Their scratch flux worktrees were already gone. flux's `Cargo.lock` is unchanged by both. truffle-core's build script builds its test sidecar into the Truffle worktree on each check, as before.

## 5. Smoke run

Release `flux-server` at `2496475`, `--bind 127.0.0.1 --tailnet off --web-dir web/dist` (built with `VITE_FLUX_API=/`), fresh data dir under `tmp/d-merge/`, port 18496. Library: APFS clones of `library-tiny/{Movies,TV}` and the `features` clips. Client: a stdlib Python script (`tmp/d-merge/scripts/smoke.py`, not committed) plus curl. Results: `bench/results/flux-merge-d-smoke.json`. Load average 9.9–13.2 (an hls-validate run was going).

**Cross-site contract (curl):**

| Request | Result |
|---|---|
| `GET /api/v1/server`, `Host: evil.example:18496` | **421**, `text/plain`, `nosniff`, a sentence naming the host and `--allow-host` |
| SEC-1 repro: `POST /api/v1/libraries` with a rebinding Host | **421**; no library created |
| `POST /api/v1/libraries` without `X-Flux-Request` | **403** |
| SEC-7 repro: body-less `POST /libraries/1/scan`, `Origin: https://evil.example`, `Sec-Fetch-Site: cross-site` | **403** |
| foreign `Origin` with the header; `Origin: null` with the header | **403**, **403** |
| correct request (`Host: 127.0.0.1:18496`, header, same-origin `Origin`) | **200** |
| `Host: localhost:18496`, `Host: [::1]:18496`; a GET without the header | 200, 200, 200 |

**Product checks** (27/27 on a fresh data dir):

| Check | Result |
|---|---|
| startup | ready in 12.4 ms (empty data dir); tailnet `off`; localhost caller = owner |
| libraries | Movies, TV, Features created over the API; all scanned in 0.17 s (warm cache): 40 movies, 5 shows, 16 features; state `watching`; every root has a live `last_seen`. Nested roots → 400 with the folder names |
| browse | all six sorts × three libraries page through every item exactly once; home, search |
| direct play `h264_aac_faststart.mp4` | `/play` 8.8 ms; `Range: bytes=1000-1999` → 206 `bytes 1000-1999/46189102` |
| remux `h264_flac.mkv` | "repackages the streams"; segments 0, 1 and a mid-file seek: 16–17 ms each |
| **TS remuxed, not transcoded** `h264_aac.m2ts` | video **copy** ("repackages the streams (no quality loss)"); `/play` 70 ms cold (in-process TS index), first and last segments 4.4 / 4.5 ms. `mpeg2_ac3.ts` (MPEG-2 video) still transcodes, as it should |
| audio conversion `h264_truehd71.mkv` | TrueHD → `1-aac2`; AAC segment 195 ms |
| **audio switch** `h264_multi_audio_subs.mkv` | 2 renditions in one plan (`1` copy, `2-aac2` conversion); switching inside the session = the other rendition's playlist, init and a segment, all 200: 29 ms for the copy, 125 ms for the conversion's first segment. `/play` with `audio_track` = the second track makes it the default |
| subtitles (PB-7) | first request streamed (no Content-Length), the second from the cache with its length, same bytes |
| video transcode `hevc_main10_hdr10_hvc1.mp4`, SDR display | tone-mapped transcode; piece 0 3 ms (warmed at `/play`), piece 1 1.18 s cold; **weak ETag** `W/"…/v/0.m4s"`, revalidates with 304 |
| AVI `mpeg4_mp3.avi` | ffprobe-indexed (bounded); `/play` 110 ms; piece 1 0.83 s; every piece starts on an IDR, 750/750 frames |
| `POST /plan`, progress, diagnostics | 0.5 ms; 204 then the item is in Continue watching; 200 |
| SSE | `scan_progress` events on `/api/v1/events` |
| jobs | no metadata source: the 61 match jobs fail at once, art jobs run (21 done within 2 min, the rest queued behind the 30 s playback pause). Opening an item without art moved its art job from priority 40 to 0 |
| web UI | `/` 200 `text/html` with the strict CSP |
| SIGTERM | 0.09 s with nothing in flight |

**Restart over the same data dir** (23/23): ready in 11.3 ms; the catch-up Quick scans probed 0 files and changed nothing; all 8 cached playback indexes reused (rows untouched); progress kept.

**ROB-1 end to end:** a second server with `--ffprobe` pointing at a script whose `sleep` child hangs; `/play` of the AVI in flight, then SIGTERM: the process exited after 5.1 s (the grace for in-flight requests) and no `sleep` was left. Before the fix: > 35 s, never exited, ffprobe left running.

## 6. hls-validate

hls-correct regenerated the corpus results on its branch: `bench/results/flux-hls-validate-20261020-044028.json`, 183/183 ok (61 files × chrome, appletv, legacy). The merge re-ran the same corpus plus hls-perf's ten scratch TS/M2TS files on the merged build, so the in-process TS path and the new TS transcode input are covered too:

- **Result: 213/213 ok** (71 files × 3 presets), in 18 min at load 9–13: `bench/results/flux-hls-validate-20261020-061447.json`.
- 0 ffmpeg errors; 1,836,498 video frames, and **every case's video packet count equals the source's** (hls-correct's validator still allows one surplus frame per real source hole; none was needed: `mpeg4_mp3.avi` is now 750/750 on all three presets); max A/V offset change 0.51 ms; 141 I-frame playlists.
- **MPEG-TS/M2TS:** 45 cases. 42 are indexed in process (`TsScan`): 19 remuxes ("Direct stream"), 20 audio conversions and 3 transcodes (legacy), among them the 4K HDR10 M2TS 719/719 and the 10-minute open-GOP HEVC TS 14,386/14,386 through the new stdin input, which hls-perf's report had seen fail with RPS errors under `-ss`. The other 3 (MPEG-2 video) still go through the ffprobe scan and transcode.
- `vfr_section.ts`, which failed on hls-perf's branch because the old validator's ffmpeg check failed the source itself, passes with hls-correct's 90 kHz decode.
- Index methods across the run: Cues 84, Moov 48, TsScan 42, FfprobeScan 27, Fragments 9, ClusterScan 3.

## 7. Open items

Proposals the agents made that the merge did not apply, with why:

- **flux-core `AssetInfo.probe_error`** (scancat; ROB-3/ROB-V1 visibility). A DTO change: web types and the golden fixtures move with it. The catalog stores the reason and `AssetLocation` exposes it.
- **A "Refresh" action for Deep scans** (scancat). Deep is version-selective now; the server still never issues it.
- **TS playback index at scan time** (hls-perf). A cold 5.8 GB TS takes 27 s to index on first play. Needs a post-scan job writing `put_asset_index`.
- **A global allocator that returns memory** (scancat). macOS keeps a cold scan's freed memory (idle footprint 80–88 MB, 12 MB live). Measure mimalloc/jemalloc before adopting.
- **Mille step 3f** (refreshing the FSEvents token while watching), then `MilleResume` as the macOS default.
- **A raw ASS route** (web) for styled subtitles with ASS.js, and keeping `\i1`/`\an8` as WebVTT tags/`line:0` (flux-hls `subtitle.rs`).
- **`browser/run.mjs --tmp`** and a source baseline in hls-validate for VFR TS (hls-perf): the validator fails `vfr_section.ts` because ffmpeg's null muxer errors on the source too.
- **Quantizing caps inside the planner** (server) instead of in `flux-server::play::quantize_caps`; **serving `/media/{asset}/file` by catalog identity alone** (no containment) was rejected in favour of §2's combined rule.
- **Truffle:** `NodeBuilder::control_url`, sidecar auto-restart (flux now shows `Failed`; turning remote access off and on rebuilds the node), NAPI/Tauri exposure.

Known gaps carried over (details in review.md and each component report):

- SEC-2: HLS re-checks containment when an asset is (re)loaded, not per segment.
- SEC-3/SEC-6: a local flood can hold loopback's 64 slots until the header timeout; a slow-but-reading client is bounded only by the per-IP cap; dead SSE streams count until a keep-alive write fails.
- SEC-5: session nonces aren't HMAC-authenticated.
- PB-2: predicted pieces (ffprobe-indexed sources, TS) are cut by count snapped to the forced IDR, not by encoder timestamps; VFR TS falls back to one piece per segment.
- PB-7: a Cue-less Matroska still walks block headers (15 % of the file) to complete the track, though the first cue now goes out at once.
- PB-9: Matroska rotation isn't read.
- INT-2: unmounted volumes are recognised only by a device mismatch on an empty listing; the real-mount test needs `RUN_MOUNT=1`.
- A file cataloged by a cold scan and moved to another volume before the background fingerprint pass can't be re-attached.
- `external::kill_running` is process-wide: an engine's shutdown also ends another engine's scans in the same process (tests only; the server has one engine).

## 8. Worktrees and scratch

- `git worktree remove` removed `$FLUX_WORK/wt/fix-{scancat,hls-correct,hls-perf,server}` (all clean). The `phase-d/*` branches remain. The `wt/review-*` worktrees are left for the next stage.
- The merge's scratch is under `$FLUX_WORK/tmp/d-merge/` (smoke libraries are APFS clones; the validation segments were deleted after the run). Other agents' scratch is left as they reported it, notably `tmp/d-hls-correct/validate-corpus` (11 GB).
- No server, ffmpeg or ffprobe started by the merge is left running.

## How to re-run

```sh
cd /path/to/flux
export CARGO_TARGET_DIR=$FLUX_WORK/target/c-e2e TMPDIR=$FLUX_WORK/tmp/d-merge/tmp
nice -n 19 cargo build --workspace --all-targets -j 3
nice -n 19 cargo clippy --workspace --all-targets -j 3
for c in flux-net flux-names flux-hls; do nice -n 19 cargo clippy -p $c --no-default-features --all-targets -j 3; done
nice -n 19 cargo test --workspace --no-fail-fast -j 3
cargo fmt --all --check
RUSTDOCFLAGS="-D warnings" nice -n 19 cargo doc --workspace --no-deps -j 3
(cd web && pnpm typecheck && pnpm lint && pnpm test && pnpm build)
(cd web/scripts/golden-rs && cargo check --locked --offline)
nice -n 19 cargo test -p flux-hls --test merge_seams -j 3 -- --nocapture      # the merge's flux-hls seams
nice -n 19 cargo test -p flux-server --test seams -j 3                         # the merge's server seams
```
