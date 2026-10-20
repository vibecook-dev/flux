# Phase D re-verification

*2026-10-20. Adversarial re-verification of every merged fix on `main`, starting at `16469af`. For each of the 37 findings I reran the reviewer's original repro against merged `main`, then tried to break the fix with a variant the regression test doesn't cover (other containers, edge sizes, concurrency, a different order). `CARGO_TARGET_DIR=$FLUX_WORK/target/c-e2e`; every heavy step ran at `nice -n 19 -j 3`.*

Source of the fixes and their regression tests: [review.md](review.md) and [fixes.md](fixes.md). The raw findings are `$FLUX_WORK/review/findings-d1.md`; the reviewers' repros are in the `wt/review-*` worktrees and the reusable ones were copied to `$FLUX_WORK/review/repros/`.

## Outcome

- **32 of 37 held** unchanged on merged `main`: the original repro no longer reproduces, and no variant broke the fix.
- **5 were hardened** by this stage, each with a regression test that fails before the new change (commits below). One of them (**SEC-2**) was a genuine reopen: the merged fix did not hold against a variant.
- The web e2e walk hit one **stale test expectation** (not a product bug), now corrected.

| Finding | Severity | Original repro on main | Variant tried | Verdict |
|---|---|---|---|---|
| SEC-1 | critical | forged `Host` on GET/POST, rebinding host → **421**; no library created | `localhost.evil`, `127.0.0.1.nip.io`, numeric/hex IPs, `[::ffff:7f00:1]`, two `Host` headers, tab-folded value, abs-form target, HTTP/1.0 no-Host, `/media` + web fallback + SSE | **held** |
| SEC-2 | medium | re-point a link to a secret (same size+mtime) → `/media` 404, HLS segments 500/404 | **in-process re-point while the HLS source is open; re-point + rescan (watcher / Scan now)** | **reopened → fixed now** (2 commits) |
| SEC-3 | medium | 245 SSE from one caller under `ulimit -n 256` → 32 streams, 214×503, no EMFILE, recovers at once | 100 stalled `/media` readers (SEC-6 combined) | **held** |
| SEC-4 | medium | 300 varied-bitrate `/play` → 1 plan file | 6,000 plays over 30 `max_level` × 13 `max_width` × 13 `max_height` → 540 distinct plans, 540 files (far below the 4096 cap) | **held** |
| SEC-5 | medium | 20,000 forged ids on GET diagnostics → RSS +0.1 MB, flat latency | 20,000 forged ids on POST `/progress` (the rebuild path) → RSS +0.5 MB, flat | **held** |
| SEC-6 | medium | 100 stalled readers → dropped at 60 s (fds 83→27) | holding past 80 s; freed at the write timeout, not at FIN | **held** |
| SEC-7 | low | body-less cross-site POST `/scan` (Origin evil, Sec-Fetch cross-site) → **403** | form-urlencoded, `Origin: null`, path spellings (`//api`, `/%61pi`, `/API`, `/x/../api`), `PUT`, extension method, OPTIONS preflight (405, no CORS) | **held** |
| ROB-1 | high | hung ffprobe (H.264 TS) during `/play`, SIGTERM → exit 0.5 s, no child | **3 concurrent plays over AVI + MPEG-2 TS with a hung ffprobe, then SIGTERM** → each `/play` 500 at the 20 s stall deadline, exit 5.05 s, no child | **held** |
| ROB-2 | high | `stsz` 20–60 M / `ctts` u32::MAX (HLS caps) → clean error / ≤2 ms | **`stsz` size 1 × file-length samples**: indexes, but a segment cost ~250 MB/request; `stts`/`stsc`/`stco`/`co64` bombs | **hardened** (1 commit) |
| ROB-3 | medium | launchd PATH, no `--ffprobe` → FLV/MPEG-PS probed | `--ffmpeg` at a non-standard dir (sibling ffprobe); `FLUX_FFPROBE` only | **held** |
| ROB-4 | low | SIGTERM during a scan with a hung ffprobe → exit 0.02 s, no orphan | hung scan-time **and** play-time ffprobe at once, **SIGINT** → exit 5.04 s, both killed | **held** |
| ROB-5 | low | truncated MP4 (64 KiB) → 500 with a complete body, no Content-Length mismatch | **file shrinks after the asset is prepared** (overwrite in place) → body failed mid-stream after a 200 | **hardened** (1 commit) |
| ROB-V1 | medium | chmod-000 then 644: healed on Scan now | **watcher alone** heals after the backoff (no restart, no Scan now); a fix within the first minute waits for the next pass, as designed | **held** |
| ROB-V2 | medium | 128 MiB sparse `moof` size 0 → index error | 2 GiB sparse size 0; `moof` size (64 MiB + 8) | **held** |
| PB-1 | high | `vfr_section.mkv`: 288/288 frames, 0 colliding PTS | the same over chrome/appletv/legacy (hls-validate) | **held** |
| PB-2 | high | `vfr_hevc_short.mkv` 288/288 on the transcode path; every piece starts on an IDR | hls-validate over hevc30_nodd, vfr_hevc, the AVI | **held** |
| PB-3 | medium | file replaced at the same path → the new file's cues served (not stale) | end-to-end server (`server_exp.sh`): vtt 1 `00:00:01`, vtt 2 `00:00:06`, differ | **held** |
| PB-4 | medium | source closes 30 s idle / at once on delete | fd of an external-link source closed after 31 s idle, measured with `lsof` | **held** |
| PB-5 | medium | 3 abandoned seeks → 0 ffmpeg children, next piece not delayed (513 vs 523 ms) | **two viewers on one piece, one aborts** → the other still gets its piece (200, 1.9 MB) | **held** |
| PB-6 | low | `warm()` of a 3×TrueHD disc produces the default audio + lookahead only | — | **held** |
| PB-7 | medium | subtitle read 0.1 % of a cue-indexed MKV | **ffmpeg-muxed MKV** (cues) 0.1 %; **cue-less** MKV 15.3 % (the documented gap) | **held** |
| PB-9 | medium | rotated phone video upright for copy and transcode | 90°/180°/270° H.264 and HEVC `.mov`; transcode SSIM 0.87 vs the upright source, −0.003 vs the 180°-turned one | **held** |
| PB-10 | low | `truehd_x3.mkv` BANDWIDTH 6.48 vs 6.19 Mb/s sent (video + default audio), not the sum of all tracks | appletv (E-AC-3 + AAC): 6.93 vs 6.64 | **held** |
| PB-11 | low | audio transcode byte-identical on demand vs prefetched; video transcode weak ETag | — (video byte-difference is expected; the ETag is `W/`) | **held** |
| PB-12 | low | 1080p60 transcode → level 4.2, `avc1.64002a` (matches the SPS) | 720p60 → 4.1; 24 fps → 4.1 | **held** |
| PB-NEW-1 | medium | `start1_hevc.mkv` 144/144 frames, piece 0 at source frame 0 | `start10_hevc.mkv` (offset 10 s), `start1_hevc.mp4` → 144/144 each | **held** |
| PB-NEW-2 | low | hls-validate fails a transcode missing frames | 60/60 clips: packet counts equal the source's; VFR transcodes 288/288 | **held** |
| PB-NEW-3 | low | `--help`, unknown flag, unknown preset, missing file, existing results without `--force` all error; no shared file overwritten | `hls-serve --help`/`--bogus` print usage and exit without binding | **held** |
| INT-1 | high | title-fixing rename carries the resume point and pin to the new item; old item merged away | show-folder retitle (every episode's watched flag); move between libraries | **held** |
| INT-2 | medium | dangling file/folder link, empty mount point with files on another device → unread, `missing=0` | `RUN_MOUNT=1` with `hdiutil` is sandbox-denied; the device rule is covered by rewriting the catalog's devices | **held** |
| INT-3 | medium | Scan-now joining a running scan → a follow-up pass sees the change | watcher-overflow join | **held** |
| INT-4 | medium | a concurrent watcher move is not tombstoned by a stale snapshot (`ScanBatch.expect`) | catalog-level `missing` by id with a guard | **held** |
| INT-5 | medium | in-root alias: 1 asset, 0 writes across 20 unchanged rescans | **two-hop alias** (`root/Alias -> /hop -> root/Real`): 1 asset, 0 writes | **held** |
| INT-6 | low | transient open failure retried, facts kept | — | **held** |
| INT-7 | low | overlapping roots → 400 | **same folder spelled through a symlink, with `..`, or in another letter case** → accepted, files cataloged twice | **hardened** (1 commit) |
| INT-8 | low | merge keeps the user's pin when the target has none | — | **held** |
| INT-V1 | low | edit-in-place + rename → re-probed, facts match a deep scan | **edit-in-place + move to another folder** → re-probed, facts match a deep scan | **held** |

## Fixes made during the re-verification

Each is on `main` with a regression test that fails before it.

1. **`d4476a8` — SEC-2: a reopened source must be the inode it was opened as.** PB-4 made an idle HLS source close its descriptor and reopen by path, checking only size and mtime. An external link re-pointed in place (while the source was closed) at another file with the same size and mtime was then read by the reopened source: HLS segments of an MP4 carried the other file's bytes. `Source` now records its first inode and refuses a reopen that finds another; `Asset::checked` reopens before the response is built; the server's 10 s re-check compares the inode too. Tests: `source::tests::a_reopen_through_a_repointed_link_refuses_another_inode`, `flux-server play::a_reopened_hls_source_does_not_follow_a_repointed_link`.

2. **`da8801f` — SEC-2 reopen: a file outside the library is served only while it is media.** The serve-time identity check trusts the catalog's recorded `(ino, size, mtime)`. But the scanner follows a re-pointed external link and *re-records* its new target's identity: re-pointing `Movies/Ext/Ext.mkv -> /secret` and rescanning (the watcher, or Scan now) updated the row to the secret's identity (state `probe_failed`, facts kept), after which `/media/{asset}/file` streamed the secret. Proven end to end (a 562-byte non-media secret served verbatim, and HLS segments of a same-size secret). Fix: a file resolved outside every library root is served only when its catalog state is `present` (`Recorded::media`), so a link re-pointed at a non-media file is refused however the scan re-records it. Files inside the roots are unaffected. Tests: `play::a_repointed_external_link_to_a_non_media_file_is_not_served`; `files_outside_the_library_are_not_served` now uses real media for its served external link.

3. **`25db4bc` — ROB-2: tighter MP4 sample-table bounds.** The merged fix bounds a track's sample count by the file length (≥1 byte/sample) and 32 M. A crafted `stsz` of 1-byte samples, as many as the file has bytes, still indexed and cost ~140 bytes of memory per sample per segment request (a 1.6 MB file → +250 MB/request). Now a per-sample-expanded track must average ≥16 bytes/sample of the file, and `MAX_SAMPLES` is 8 M (one segment request bounded at ~1.1 GB). PCM (per-chunk) keeps the 1-byte rule. Test: `phase_d::mp4_sample_table_bombs` gains the 1-byte `stsz` file.

4. **`a111235` — ROB-5: a file that shrank since it was opened fails before the response.** A segment's ranges are checked against the length recorded when the source opened. A file overwritten in place and shortened while its descriptor is open (within the 10 s re-check) passed that check, so the body failed mid-stream after a 200 — the ROB-5 symptom. `Source::ensure_open` now also refuses a descriptor whose file shrank below the recorded length. Test: `phase_d::a_file_that_shrinks_after_prepare_fails_before_the_response`.

5. **`f6b08cf` — INT-7: refuse overlapping roots by where they really are.** `CatalogError::OverlappingRoots` compares roots as written. The same folder spelled through a symlink, with `..`, or in another letter case was accepted and every file below it cataloged twice. `POST /api/v1/libraries` now canonicalizes the requested roots before the catalog's lexical check. Test: `seams::overlapping_roots_are_a_bad_request` gains the three spellings.

## flux-hls browser harness (subset)

Chrome 155 (headless, playwright-core) + hls.js 1.7.3, against the re-built release `hls-serve`, over a subset that covers the new TS remux cases and the VFR, rotation and anamorphic fixes: **10/10 ok**, 0 fatal hls.js errors, 0 `video.error`, no MSE exception.

| File | Plan | Notes |
|---|---|---|
| `anamorphic.mkv`, `anamorphic.mp4` | Direct stream | SAR carried; play and seeks fine |
| `h264_aac.m2ts` | Direct stream | in-process TS remux (Phase D) |
| `i576.ts` | Audio conversion | interlaced 576i H.264 TS |
| `mpeg2_ac3.ts` | Transcode | MPEG-2 video (ffprobe-indexed) |
| `opengop_hevc_20s.ts` | Direct stream | open-GOP HEVC over the stdin input |
| `rot90_h264.mov`, `rot90_hevc.mov`, `rot270_hevc.mov` | Direct stream | rotation honoured in the player |
| `vfr_section.mkv` | Direct stream | VFR frames keep unique PTS |

## Web e2e walk (`pnpm e2e`)

Release server per the web scripts (Chrome via playwright-core, scratch under `$FLUX_WORK/tmp/d-reverify/e2e/`). The first full run was **35/36**; the one failure, `play-ts-h264-m2ts`, was a **stale expectation**, not a product bug: the case still expected `Transcode` with the note "flux-hls never remuxes TS in-process", but Phase D's hls-perf added in-process TS/M2TS remux, so `h264_aac.m2ts` now correctly does **Direct stream** (video+audio copied; the browser harness agrees, and every playback signal — TTFF 137 ms in that run, 10 seeks landed, frames advanced — was healthy; *audit:* the re-run's `flux-e2e-product.json` replaced that run's record and shows 133 ms, so the 137 ms can't be traced). Corrected the expectation in `web/scripts/e2e-product.mjs`; the step then passes, and the bench and playback walks (which `&&`-chain after the product walk) run through. `flux-e2e-product.json` holds the per-step record.

## Full gate (on `main` after the fixes)

| Check | Result |
|---|---|
| `cargo build --workspace --all-targets` | ok |
| `cargo clippy --workspace --all-targets` | 0 warnings |
| `cargo test --workspace` | **603 passed**, 0 failed, 4 ignored (was 599; +4 regression tests) |
| `cargo fmt --all --check` | clean |
| web: typecheck / lint / test / build | ok; vitest **130/130**; build ok |
| web e2e (`pnpm e2e`) | product **36/36**, bench and playback walks ok |
| flux-hls browser harness (subset) | **10/10 ok** |

## Notes

- Everything ran on `127.0.0.1` with tailnet off; every server, `hls-serve`, ffmpeg and Chrome started was killed (`pgrep` clean at each stage).
- Test media under `media/` was only read or APFS-cloned. Scratch is under `$FLUX_WORK/tmp/d-reverify/`.
- The reviewers' ported tests ran in temporary files (removed after) with a temporary `rusqlite` dev-dependency on `flux-scan` (reverted); the five fixes above carry their regression tests in the committed suites.
