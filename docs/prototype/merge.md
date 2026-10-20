# Phase C merge

*2026-10-19. Merged `phase-c/{catalog,hls,scan,server}` into `main` and wired the seams. `CARGO_TARGET_DIR=$FLUX_WORK/target/c-merge`. Everything heavy ran at `nice -n 19`, `-j 4`. The machine is shared with another project: the 1-minute load average was 2.6–12.4 during the runs. All timings below are indicative only.*

## Summary

- **The merge had no textual conflicts.** All four branches merged with `git merge --no-ff` in the order catalog → hls → scan → server, and Cargo.lock merged automatically.
- **Only flux-server broke, with 6 compile errors.** It had been written against the phase-B catalog API (`claim_jobs(kind, n)`, `finish_job(id, result, …)`, a `(version, bytes)` play-index tuple). Every other crate built unchanged.
- **Every `TODO(phase-c-merge)` is resolved; none are left in the tree.** The server now uses:
  - the catalog's six sorts and per-user filters;
  - the play-index BLOB in flux-hls's binary encoding, with the path rewritten after a rename;
  - the durable job queue for match and art jobs, and an automatic merge when two works match the same provider ids;
  - per-user progress through `set_progress`;
  - art rows;
  - the real flux-scan `Scanner`. Its socket test is no longer ignored.
  - `plan_from_probe` for `POST /plan` and for the fallback when a file has no index.
- **The full gate passes:**

  | Check | Result |
  |---|---|
  | build | 0 warnings |
  | clippy | 0 warnings |
  | tests | 447 passed, 0 failed, 4 ignored |
  | fmt | clean |
  | rustdoc (`-D warnings`) | clean |
  | web (typecheck, lint, test, build) | 89 tests pass |
  | golden-rs | compiles |
  | flux-scan on library-tiny | 393/393 files placed as the ground truth says |
  | server smoke run | 30/30 checks pass |

## 1. Merge

| Branch | Commits | Uncommitted work in its worktree | Conflicts |
|---|---|---|---|
| `phase-c/catalog` | 9 | none (`git status` clean, no ignored files) | none |
| `phase-c/hls` | 11 | none | none |
| `phase-c/scan` | 11 | none | none; Cargo.lock merged automatically |
| `phase-c/server` | 6 | none | none; Cargo.lock merged automatically (clap) |

Merge commits: `0152681` (catalog), `f19004c` (hls), `b65ac21` (scan), `5d70692` (server). Fixes are on top: `a4da476` (seams) and `a6531a6` (follow-ups), plus this report.

The first build after the merges failed only in flux-server:

| Where | What changed | Error |
|---|---|---|
| `jobs.rs` | `claim_jobs` and `finish_job` signatures | E0277, E0061 ×2 |
| `play.rs` | `asset_play_index` now returns `CachedPlayIndex`; `put_asset_index` now takes a `PlayIndex` | E0308 ×2, E0061 |
| `tests/play.rs` | the same tuple | 1 |

The test count reconciles exactly:

| | Tests |
|---|---|
| main before the merge | 354 |
| + catalog | 26 |
| + scan | 30 |
| + hls | 12 |
| + server | 21 |
| + the formerly ignored flux-scan socket test | 1 |
| + new in this merge | 3 |
| **Total** | **447** |

## 2. Seams wired

| Seam | Before (branch) | After (main) |
|---|---|---|
| **Sorts** (`GET /libraries/{id}/items`) | `year`, `-year`, `added` and `-title` returned 400 | `ItemsParams.sort` goes straight to the catalog (`read::ItemSort` *is* `api::ItemSort`). A cursor from another sort gets 400 (`CatalogError::Invalid`). The router test pages through all six sorts and checks the year order, with unknown years last. |
| **Filters** | passed through | Unchanged. They are per user, because `ItemsQuery.user_id` is the caller. |
| **Play index** | JSON in the catalog BLOB | `PlayIndex { format: INDEX_FORMAT_BINARY ("fxix1"), version: INDEX_VERSION, source_size, source_mtime_ns, bytes: idx.to_bytes() }`. On a hit: format and version are checked, then `from_bytes(..).with_path(cached.path)` and `prepare_with_index`. If anything fails, `clear_asset_index` runs and the index is rebuilt. Both writes use `submit_async`, so playback never waits for the commit. Tests check the binary format, that the cached index is reused after a `FileMove` (not rebuilt), and that it is served with the new path. |
| **Prepared-asset cache** | server-side LRU, 32 entries | Still the server's own LRU, because it layers the catalog's index cache under the engine. It is now also limited by bytes: `Asset::approx_bytes`, budgeted by `EngineConfig::asset_cache_bytes` (256 MB), up to 64 entries. `HlsEngine::prepare` isn't used: it can't consult the catalog cache, and the server's 10 s stat recheck avoids opening the file for every segment. |
| **Prefetch for transcoded video** | `v/{n}.m4s` with n = remux segment | `v/{engine.segment_at(Video, t)}.m4s`. Audio keeps remux segment n, as before. |
| **`POST /plan`** | built the index (`plan_for`) | Uses the prepared asset if it is in memory (exact). Otherwise `flux_hls::plan_from_probe(stored probe)`, which reads no file. Otherwise it prepares the asset. |
| **No-index fallback** | `flux_hls::plan(probe)`, direct play or fail | `plan_from_probe(probe)`, direct play or 501. Fragmented MP4, QuickTime AC-3 and AVI now get an index in flux-hls, so this path is rare. |
| **Progress** | `put_progress` | `set_progress`: at 90 % the item is marked watched, along with every episode of a multi-episode file. Track choices are still not stored per user (gap). |
| **Durable job queue** | match: phase-B API; art: an in-process SQL scan for items without art | Both run from the catalog's `job` table. See "Jobs" below. |
| **Merging works** | none | After an accepted match, the server calls `merge::works_sharing_ids`. Every other work with the same ids is merged into the oldest one (`merge_items`), and `RekeyOutcome::event()` is sent over SSE. |
| **Scanner** | flux-scan stub detected with `catch_unwind`; "scanning unavailable" | Always `flux_scan::Scanner::new(catalog, ScanConfig::default())`. At startup the server calls `watch(lib)` for each library, and the first watch catches up with a Quick scan at Scheduled priority. Creating a library or `POST /scan` still runs `scan_library(Quick, Interactive)` and then `watch`. `startup_scan_with_flux_scan` is no longer ignored: it waits for the scan to finish and compares `item_count` with the browse page. |
| **Raw SQL in the server** | `items_needing_art`, `library_of` | Gone, and the `rusqlite` dependency was dropped. The new catalog reads are `read::item_library` and `read::show_episodes_art`. |

**Jobs.** The executor is a loop in `flux-server/src/jobs.rs`. It works like this:

- **Match jobs** are claimed 4 at a time (5-minute lease) and finished `Done`, or `Retry(e)` with the catalog's backoff.
- **Without a metadata source** (no TMDB key, no offline exports), match jobs are failed at once (`Fail("no metadata source …")`). Art jobs wait on the match (`after_id`), so failing it lets them run. At the next start that has a source, `requeue_failed_jobs("match")` queues them again.
- **Art jobs** are claimed one at a time (2-minute lease, extended after every image). Each one handles one movie or show:
  - It makes a frame poster and backdrop for whichever of the two is missing; a show's frames come from its first episode.
  - For a show, it then makes a thumbnail for every episode that has none.
  - Playback or a scan pauses it: the rest is handed to a fresh art job (`art_key(item, now)`), and the current one finishes.
  - A movie with no decodable frame gets `Fail`. "No reachable file" gets `Retry`.
- **When idle**, the loop sleeps until `next_job_due` (at most 30 s) or until new items arrive. If a job is due but none can be claimed (an art job waiting on a match in backoff), the sleep backs off from 0.2 s to 25.6 s.
- The `jobs::frame_posters_become_art_rows_and_resize` test now runs through this queue.

## 3. Small cross-crate changes applied

| Crate | Change | Asked for by |
|---|---|---|
| flux-hls | `index::INDEX_FORMAT_BINARY` (`"fxix1"`, tested to follow the layout byte) and `PlaybackIndex::with_path` | catalog |
| flux-catalog | `read::item_library` and `read::show_episodes_art`, so the server needs no SQL; test `show_episodes_art_lists_episodes_in_order_with_their_thumbs` | server |
| flux-core | `ItemSort` doc says all six sorts are implemented. The API conventions now say that error bodies are plain text and that anonymous callers share one guest user. Docs only; golden-rs compiles. | catalog, server |
| flux-scan (`examples/pipeline_smoke.rs`) | The work dir comes from `FLUX_SMOKE_WORK` and the result-file prefix from `FLUX_SMOKE_PREFIX`. The `cargo test` run uses a per-process database and deletes it afterwards. | catalog (concurrent runs clobbered each other) |
| flux-server (`examples/probe_vs_index.rs`) | The dry-run column uses `plan_from_probe`, and the test asserts 0 differences from the index plan across 6 presets × the feature clips (was 23/144). | hls |
| web `player/engine.ts` | hls.js `progressive: false`. With it on, a 4K first frame took 1.7 s against 0.59 s, and seeks 1–3 s against 0.09–0.28 s (hls.md). | hls |
| web `plan/predict.ts` | MP3 isn't packageable in fMP4, so it becomes AAC. Matroska and MP4 never get `no_index`. Both mirror the Rust planner; 2 new tests. Proposal (a), the stereo AAC fallback, doesn't apply, because the sketch models a single audio rendition. | hls |
| docs | `server.md` gets notes pointing here where it described the stub and the in-process art scan | — |

## 4. Gate

```sh
cd /path/to/flux
export CARGO_TARGET_DIR=$FLUX_WORK/target/c-merge TMPDIR=$FLUX_WORK/tmp/merge/tmp
nice -n 19 cargo build --workspace --all-targets -j 4          # ok, 0 warnings
nice -n 19 cargo clippy --workspace --all-targets -j 4         # 0 warnings
nice -n 19 cargo test --workspace --no-fail-fast -j 4          # 447 passed, 0 failed, 4 ignored (1:57)
cargo fmt --all --check                                        # clean
RUSTDOCFLAGS="-D warnings" nice -n 19 cargo doc --workspace --no-deps -j 4   # clean
for c in flux-net flux-names flux-hls; do nice -n 19 cargo clippy -p $c --no-default-features --all-targets -j 4; done  # 0 each
cd web && pnpm typecheck && pnpm lint && pnpm test && pnpm build   # ok; vitest 89/89; 315 modules
cd web/scripts/golden-rs && cargo check --locked --offline         # ok
```

The 4 ignored tests are opt-in by design:

- live Ollama;
- live TMDB;
- this machine's tailscaled (WhoIs);
- the probe's deep header fuzz.

**flux-scan on library-tiny.** `flux-scan-bench tiny` (release build), saved to `bench/results/flux-merge-scan-library-tiny.json`. The bench always writes `flux-scan-library-tiny.json`, so I moved the new file to this name and restored the scan agent's original. Load average was 4.8–5.3.

- **Placement:**
  - Movies 50/50 files, 40 movies.
  - TV 343/343 files: 5 shows, 31 seasons, 345 episodes.
  - Every count equals the ground truth, on the original corpus and on a fresh clone.
- **Scan times:**

  | Scan | Time | Media files opened | Writer commands |
  |---|---|---|---|
  | cold | 185 ms | | |
  | cold, warm cache | 48 ms | | |
  | cold, no fingerprinting | 43 ms | | |
  | unchanged rescan | 5 ms | 0 | 0 |
  | rescan after adding 50 files | 35 ms | 50 | |
  | rescan after touching 50 files | 18 ms | 50 | |

- **Watcher, from a file change until it is committed:** add, median 180 ms; delete, median 170 ms.

The workspace test `library_tiny_end_to_end_and_zero_touch_rescan` asserts the same placement.

## 5. Server smoke run

The server was the debug binary at `a6531a6`, run as:

```sh
flux-server --data-dir $FLUX_WORK/tmp/merge/smoke-data --port 18396 --bind 127.0.0.1 --tailnet off --web-dir web/dist
```

The data dir was empty to start with. The smoke-test client was a stdlib Python script, not committed. Results are in `bench/results/flux-merge-smoke.json`. Load average was 4.2–7.9.

| Check | Result |
|---|---|
| startup | API ready in 26 ms (cold catalog), 4.6 ms on restart; tailnet mode `off`, state `off`; `/api/v1/server` path `localhost`, role owner |
| libraries | created over `POST /api/v1/libraries` (which starts the scans): Movies and TV from library-tiny, and Features (media/features, Movies kind), then `POST /libraries/{id}/scan` |
| scan via the API | all three finished in 795 ms wall time: Movies 50 files → 40 items, TV 343 files → 5 shows, Features 20 files → 16 items; state `watching`, no errors |
| browse | all six sorts × three libraries page through every item exactly once (limit 25); the four filters return 200; show → 6 seasons → episodes; search; home (20 recently added, 3 rows) |
| **direct play** `h264_aac_faststart.mp4` (Chrome caps) | `/play` 4.4 ms. `Range: bytes=1000-1999` → 206, `bytes 1000-1999/46189102`, bytes equal the file's; tail range 206 |
| **remux** `h264_flac.mkv` | "Direct stream". Segments 0–1 in 2.6 and 3.5 ms; a seek to the middle 13.8 ms; FLAC audio rendition copied; all 6 prefetch URLs 200 |
| **audio conversion** `h264_truehd71.mkv` | "Audio conversion" (TrueHD → `1-aac2`). Video segments 5 ms; first AAC segment 66 ms |
| **video transcode** `hevc_main10_hdr10_hvc1.mp4` (SDR-only caps: tone map) | "Transcode". Segment 0 in 2 ms (`warm` had already produced it at `/play`); segment 1 in 791 ms; a cold mid-file seek 1.17 s (VideoToolbox, loaded machine) |
| **video transcode** `mpeg4_mp3.avi` | "Transcode". `/play` 67 ms (ffprobe index); segment 1 in 132 ms; seek 433 ms |
| `POST /plan` | same label as `/play` in all 5 cases, 0.6–1.3 ms |
| progress | `POST /sessions/{id}/progress` → 204; the item appears in Continue watching; diagnostics count the segments served |
| SSE | `/api/v1/events`: 14 `scan_progress`, 5 `items_changed` |
| jobs | match jobs failed at once (no metadata source) → art ran in queue order: 31 movies got frame posters (`?w=300` resize returns a JPEG). 9 failed: those library-tiny clones have no decodable frame. A show got its poster, backdrop and 86 episode thumbnails from one art job. The five files played had their playback indexes cached as `fxix1` v2 (2.0–22.5 KB each). |
| restart over the same data dir | catch-up Quick scan: 0 files probed, 0 items changed. `/play` reused the catalog's cached index (row `created_at` unchanged, `/play` 1.8 ms). Resume position 4000 ms kept. |
| web UI | `/` and SPA routes 200 `text/html` with the strict CSP (inline-script hash) |

## 6. Remaining gaps and proposals not applied

Things the merge leaves open:

- **Anime re-key is not wired.** flux-meta has no TMDB episode-group client, so `rekey::absolute_episodes → rekey_episodes` has nothing to feed it. Needs: a flux-meta episode-groups API, then a server step after a show's match.
- **Automatic merging is untested end to end.** It only follows an *accepted* match, and offline exports only produce Review (no year or runtime in the export). With no TMDB key, nothing in this environment can trigger it. The catalog tests `merge_items`; the server wiring is straightforward but unexercised.
- **Track choices aren't stored per user.** The catalog has no API for them.
- **flux-meta's `JobQueue` is not the executor.** The server's own loop runs jobs: one at a time, paused during playback and scans. Moving the lanes and budgets into flux-meta is a flux-meta change.
- **No backfill of art jobs for pre-v3 catalogs.** There are no deployed catalogs.
- **The AVI plan explains contradictory reasons.** It says "repackages the streams (no quality loss)" and then "transcodes it". The flux-hls planner pushes `ContainerUnsupported` before it knows the video will be transcoded. The web sketch already avoids this.
- **The smoke run used a debug server binary.** Segment timings for remux and audio include debug-build overhead. Transcode time is ffmpeg's.

Proposals deferred (they need an owner's decision or a coordinated core/web change):

- **flux-catalog:**
  - `reattach_candidate` should prefer a tombstone with the same file name, then the same folder. This is a tie-break among byte-identical tombstones and needs its own test.
  - `attach_fingerprint`, which would allow deferred fp128.
  - `load_subtree_snapshot`.
  - `RootStatus.last_seen` from the overlay.
  - Store Review match candidates.
- **flux-core and web:**
  - Art-picker routes (pin/unpin). The catalog side exists, but a picker also needs a list DTO, so this is a core-v3 and web change.
  - `PlayResponse` presentation offset D.
  - Keyframe kind (open GOP) on `Track`.
  - `ClientCaps` presets in core.
  - `ServerInfo.scanning_available`.
- **flux-net:** `set_tailnet` without restarting every listener.
- **flux-meta:** a priority option in `Tools`; `MatchQuery::from_probe`.
- **Mille:** the debounced FSEvents backend drops deletions; `coalesce_events` hides a create followed by a delete; FSEvents `sinceWhen` resume; `write_snapshot_from`.
- **flux-scan:** a cheap "is any library scanning?" accessor.

## 7. Worktrees

After the final commit, `git worktree remove` removed `$FLUX_WORK/wt/flux-{catalog,scan,hls,server}`. All four were clean, with no untracked or ignored files. The branches `phase-c/*` remain.
