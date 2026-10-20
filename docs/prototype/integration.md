# flux workspace integration check

*2026-10-19, after the eight component agents finished.* Every command uses `CARGO_TARGET_DIR=$FLUX_WORK/target/integrate`. The machine load average was 4.6–9 during the runs (quieter than the component runs, but other projects were still compiling).

## Summary

- **The workspace integrated cleanly as delivered.**
  - `cargo build --workspace --all-targets` and `cargo clippy --workspace --all-targets`: 0 warnings, 0 errors.
  - `cargo test --workspace`: 332 passed, 0 failed, 5 ignored. Every crate's count matches its agent's report.
  - There were no dependency conflicts, and the build did not rewrite Cargo.lock.
  - The web client typechecks and builds.
- **Fixes I made:**
  - 2 rustdoc warnings (one each in flux-meta and flux-net).
  - rustfmt on flux-core (6 hunks from the original skeleton).
  - 1 real cross-crate bug, found by the new checks below. A file whose H.264 profile flux-probe reports as "Constrained Baseline" was **video-transcoded on every client** when planned from the catalog's probe, because flux-hls calls the same profile "Baseline". The fix is in the flux-hls planner, with a unit test. On the corpus, plans now agree in 144/144 file × preset cases (6 differed before).
- **Two new cross-crate checks.** Both run on the real corpus under `cargo test --workspace` and write JSON results.
  - `flux-scan/examples/pipeline_smoke.rs`:
    - **What it runs:** Mille `walk_scan` → in-memory diff → flux-names + flux-probe → flux-catalog.
    - **library-tiny:** 393/393 media files are placed exactly as the ground truth says.
    - **Full 16.7k-file library:** **5.5 s end to end, 131 MB peak RSS**. Jellyfin's cold scan takes 1,047–1,063 s and peaks at 539–549 MiB (benchmarks.md, 2026-10-20; first quoted here as 1,442 s at 520 MB, from the loaded 2026-10-18 baseline). The comparison is caveated: warm page cache, no fingerprinting, no metadata fetch.
    - **Unchanged rescan:** 71 ms with zero writes (Jellyfin: 18.8–20.8 s, benchmarks.md).
    - **One real grouping bug:** "Doctor Who" (no year) and "Doctor Who (1963)" are merged into one show.
  - `flux-server/examples/probe_vs_index.rs`: flux-probe's stored `ProbeResult` against flux-hls's `PlaybackIndex`, across 24 corpus files plus 16 edge-case fixtures.
    - Track `(kind, index)` lists agree, so `PlayRequest.audio_track` is safe to pass through.
    - It found the profile bug above, plus five playback-side mismatches (§4.2).
- **Core changes:**
  - **Applied:** docs for behaviour the components already implement, plus one new self-contained type (`api::ItemsParams` / `api::ItemSort`).
  - **Deferred:** every new field on an existing struct and every new enum variant. `web/scripts/golden-rs` builds every flux-core struct literally and matches every enum exhaustively, by design. Each such change also breaks struct literals in four or more crates. They should land together as one coordinated "core v2" change with the web mirror (§3).

## 1. Build / test / clippy per crate

The baseline is the components as delivered. "After" is the state after my edits. Ignored tests are opt-in by design: deep fuzz, live TMDB, live Ollama, live tailscaled, and one doc snippet.

| crate | build | clippy (all targets) | tests passed (baseline → after) | ignored | notes |
|---|---|---|---|---|---|
| flux-core | ok | 0 warnings | 0 → **2** | 0 | new serde tests for `ItemSort` / `ItemsParams` |
| flux-probe | ok | 0 | 66 → 66 | 1 | deep fuzz (`--ignored`) |
| flux-names | ok | 0 (also `--no-default-features`) | 48 → 48 | 0 | |
| flux-catalog | ok | 0 | 57 → 57 | 0 | |
| flux-hls | ok | 0 (also `--no-default-features`) | 55 → **56** | 0 | +`ffprobe_profile_names_satisfy_base_profiles`; debug build, real-media tests 6.5 s |
| flux-meta | ok | 0 | 78 → 78 | 2 | live TMDB, live Ollama |
| flux-net | ok | 0 (also `--no-default-features`) | 28 → 28 | 2 → **1** | live tailscaled. The illustrative ```` ```ignore ```` doc block became ```` ```text ````, so it no longer counts as an ignored doctest. |
| flux-scan | ok | 0 | 0 → **1** | 0 | `pipeline_smoke` example (`test = true`): library-tiny end to end |
| flux-server | ok | 0 | 0 → **1** | 0 | `probe_vs_index` example (`test = true`): feature clips |
| **workspace** | **ok** (1m07s cold, `jobs = 4`) | **0** | **332 → 337** | 5 → 4 | |

Other checks:

- **`cargo fmt --all --check`:** before my edits, only flux-core failed (6 hunks); now clean.
- **`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`:** failed on flux-meta's bare URL and flux-net's unparsable `ignore` block; both are now fixed and the build is clean.
- **Real-media tests really ran.** The SSD was mounted, and `--nocapture` runs of the probe, hls and catalog corpus tests show no "skipping" lines.
- **Cargo.lock:** gained 60 packages, because mille-core is now used in this workspace for the first time (flux-scan dev-dependency).
  - **Version drift:** Cargo resolved them fresh rather than from Mille's own lock. 92 shared packages have different (newer, semver-compatible) patch versions than Mille's lock, and 151 differ from Truffle's. Mille's 320 and Truffle's 422 tests ran on their own locks, not this one.
  - **Old duplicates:** Mille brings notify 6, mio 0.8, bitflags 1 and windows-sys 0.48 alongside the newer versions. The footprint is small, but worth a look when Mille bumps notify.
- **Not mine:** `.cargo/config.toml` has an uncommitted `jobs = 4`. Its own comment says to remove it after about 01:15 on 2026-10-19. I left it alone; the lead should drop it before committing.

## 2. What I changed

| file | change |
|---|---|
| `crates/flux-hls/src/planner.rs` | `profile_satisfies()`: Constrained Baseline satisfies Baseline/Main/High; "… Intra" satisfies its base profile; "High 4:4:4 Predictive" = "High 4:4:4" (H.264 Annex A). Plus a unit test. Sensitivity: with the old exact-match code, `probe_vs_index`'s test fails. |
| `crates/flux-meta/src/art/encode.rs` | rustdoc: bare URL → `<…>`. |
| `crates/flux-net/src/conn.rs` | rustdoc: illustrative block with `…` placeholders: `ignore` → `text`. |
| `crates/flux-core/**` | Docs and new types (§3), the `serde_json` dev-dependency for tests, rustfmt. |
| `crates/flux-scan/Cargo.toml`, `examples/pipeline_smoke.rs` | dev-dependencies (mille-core, probe, names, catalog) and the pipeline smoke run. **The `flux-scan` library is untouched.** |
| `crates/flux-server/Cargo.toml`, `examples/probe_vs_index.rs` | dev-dependencies (probe, hls, tokio) and the agreement check. **`main.rs` is untouched.** |
| `Cargo.lock` | mille-core's dependency graph (above). |

Nothing in `web/` or the Mille/Truffle worktrees was touched. golden-rs still compiles against the new flux-core: `cargo check --locked` passes and its lockfile is unchanged.

## 3. Proposed core changes: applied and deferred

**Rule used.** Apply only what is additive and source-compatible for every consumer: documentation, and brand-new types that no existing type references. Any new field on an existing struct, or any new enum variant, breaks `web/scripts/golden-rs`, which builds 30+ core structs literally and matches every enum exhaustively, by design, so that the TS mirror can't drift. Those changes also break the struct literals in probe, hls, catalog and meta. So they are deferred and should land as one batch with the web owner.

### Applied

| # | change | from | notes |
|---|---|---|---|
| A1 | `api.rs` route table: audio renditions are `a/{rendition}/…` with `{rendition}` = `{track}` (copy), `{track}-aac2` or `{track}-eac36`; subtitles are `s/{track}.m3u8` → `s/{track}.vtt` | hls #1 | Checked against `flux_hls::playlist` (`HlsResource::parse`). |
| A2 | `ServerInfo.path` doc lists `"tailnet"` (path unknown) and points to `flux_net::NetPath::as_str()` | net, web | doc only. The web client already accepts it. |
| A3 | `Identity` doc: per-listener semantics (localhost synthetic Owner, configurable; LAN `None`; tailnet WhoIs + role order) | net | doc only |
| A4 | `NameGuess` doc: conventions for episodes, anime absolute numbering, season packs, extras (title = parent work, `episode_title` = the extra's name) and versions | names #4, catalog #5 | **Verified against real flux-names output** from the full-library run: 1,085 anime files have `season = None` and `episodes = []`; extras and editions are as documented. A resolution label (`- 2160p`) goes to `resolution`, not `edition`, so the doc says that. |
| A5 | `BitstreamFix::StripInbandParameterSets` doc: engine-decided from the index; the planner never emits it | hls #4 | doc only (removing the variant would break golden-rs) |
| A6 | **New** `api::ItemsParams { cursor, limit, sort }` and `api::ItemSort` (`title`, `-title`, `year`, `-year`, `added`, `-added`) | catalog #3, web #2 | The wire values are exactly the web's `LIBRARY_SORTS` (serde test). flux-catalog implements `title` and `-added` only (§4.3). Catalog keeps its own `ItemSort`; the server maps between them. |

### Deferred (batch into one "core v2" change with the web mirror)

> **Update:** the wire-DTO items of this table landed as one change: see [core-v2.md](core-v2.md). Still deferred: `Track.codec_config`/`reorder_depth`/`start_offset_ms`, `Attachment.data_offset`, disposition flags, Matroska `CodecDelay`, the names items (`alt_titles`, `label`, `GuessKind::Personal`, structured evidence), `ProviderIds.wikidata`, `ProbeResult.keyframes_ms`, `MatchDecision`, `ClientCaps` presets in core, and invites.

| proposal | from | why deferred / what to do |
|---|---|---|
| `Track.codec_config`, `reorder_depth`, `sample_aspect_ratio`, `start_offset_ms`; `Attachment.data_offset`; more disposition flags | probe | New fields: golden-rs plus Track literals in probe, hls, catalog (synth) and meta. Not needed to wire the server: flux-hls reads its own index at play time. Highest value of the batch: `sample_aspect_ratio` (anamorphic SD displays wrong today). |
| Matroska `CodecDelay` + exact frame duration on Track | hls #3 | Same. |
| `Reason::CodecNotPackageable` | hls #2 | New variant: golden-rs exhaustive match, plus the web's 14 golden explain sentences. |
| `NameGuess.alt_titles`, `NameGuess.label`; `GuessKind::Personal`; structured `Evidence` | names | New field or variant, or a type change. `GuessKind::Personal` also needs a catalog grouping rule. |
| `ProviderIds.wikidata`; `ProbeResult.keyframes_ms`; `ArtRef.generator` / `attribution`; shared `MatchDecision` | meta, web | New fields. `MatchDecision` should wait for the catalog's match-decision table design (today `ItemMetadata` carries confidence and evidence). |
| `ItemSummary.parent_title` / `available` / `unwatched_count`; `Library.roots` → `RootStatus`; `ClientCaps.display_hdr`; `PlayResponse.audio`; `ScanStatus.files_total` / `dirs_unavailable`; `Identity.tags` | web, catalog, mille, net | New fields on wire DTOs. These are the most user-visible: `parent_title` saves two requests per "Continue watching" card, and `dirs_unavailable` lets the UI say "N folders didn't respond". Do them first in v2. |
| `#[serde(skip_serializing_if = "Option::is_none")]` on Option fields | catalog #1 | **Changes the wire format.** The web mirror declares `field: T \| null` (required keys), so omitted keys become `undefined` at runtime. Keep the catalog's storage-only null stripping. |
| Scan-diff types into `flux_core::scan`; shared `FileStat` | catalog #2, mille | **Not needed:** let flux-scan depend on flux-catalog (§5.1). flux-scan needs catalog *functions* anyway (snapshot, writer). The smoke run shows `FileFacts` built straight from `mille_core::ScanEntry` is enough. |
| `Reason::explain()` display names (HEVC, TrueHD, MKV) | web #6 | Changes the golden sentences. Coordinate with the web's `humanizeExplain`. |
| `ClientCaps` presets into core | hls #5 | Moves code. The web derives caps from runtime probes anyway. |
| `PlayResponse.prefetch` from `HlsEngine::warm` | hls #6 | No core change needed; this is server wiring (§5.2). |
| Tailnet onboarding DTO, invites, `POST /api/v1/plan` | web #3, #4, #8 | New types, but they need server design first. Several web fields have no producer yet (`approve_url`, `key_expiry`, `system_tailscale`), and invites aren't implemented anywhere. A dry-run plan from the probe alone is wrong in 23/144 corpus cases (§4.2). |

## 4. Integration findings (with evidence)

### 4.1 Scan pipeline (Mille → names → probe → catalog)

`cargo run --release -p flux-scan --example pipeline_smoke -- full` → `bench/results/flux-integrate-pipeline-full.json`. Two libraries are set up the way a user would: `Movies/` as Movies and `TV/` as Shows. Load average was 4.7 and the page cache warm.

| | Movies | TV |
|---|---|---|
| files walked / media | 5,648 / 3,811 | 11,014 / 10,949 |
| walk (Mille `walk_scan`, 8 in flight) | 242 ms | 136 ms |
| names + probe (8 threads, wall) | 1,188 ms | 3,145 ms |
| probe reads / KiB per file | 1.29 / 113 | 1.25 / 102 |
| catalog apply (batches of 500) | 164 ms | 475 ms |
| files placed as the ground truth says | **3,811 / 3,811** (3,456 movies, 94 extras, 261 samples ignored) | **10,182 / 10,949**; all 767 misses are anime absolute numbering (by design, §5.1 #10) |
| items: expected → catalog | movie 2,998 → 2,998; extra 94 → 94 | show 300 → **299**; season 1,176 → **1,094**; episode 11,263 → **11,255** |
| unchanged rescan | 43 ms, 0 changed, 0 writer commands | 28 ms, 0 changed, 0 writer commands |

**Totals:** 5.48 s for the whole library, peak RSS 131 MB. **library-tiny:** 393/393 correct, counts exact, rescan 0/0 (also asserted by `cargo test -p flux-scan`).

Findings:

1. **Doctor Who merge (catalog grouping; fix in flux-catalog).**
   - flux-names correctly returns year `None` for `TV/Doctor Who/S01/…` and 1963 for `TV/Doctor Who (1963)/Season 1/…`.
   - The catalog's year rule is "a dated guess adopts the only yearless same-titled item, a yearless guess joins it". It merges the two *different show folders* into one show, so the 1963 S01E01–E08 become extra versions of the 2005 episodes. That is the 299/300 shows and −8 episodes.
   - The catalog's tests never saw this, because its ground-truth guesses carry the generator's year for both.
   - Suggested rule: for shows, a different top-level show folder is a different show unless provider ids agree. Keep the adopt and join behaviour within one folder.
2. **Anime absolute numbering is filed as season 1, episode N** (`group::place`). That accounts for the remaining missing seasons.
   - When flux-meta maps absolute numbers to SxxEyy from TMDB episode groups, there is **no writer command to re-key an episode**. `ItemMetadata` has no season or episode fields.
   - Needs: `Writer::rekey_episode(asset, season, episodes)` or a "mapped guess" re-apply.
3. **Three different media-extension lists.**
   - flux-probe `MEDIA_EXTENSIONS` has `mk3d/qt/m2t/tp/trp/divx`.
   - flux-names `VIDEO_EXTS` has `wmv/mpg/vob/ogv/flv/3gp/iso/rmvb/strm` but no `mk3d/qt/m2t/tp/trp`.
   - Mille's suggested walk filter has no `qt/m2t/tp/trp/divx/3gp/rmvb/strm`.
   - So a `.m2t` or `.tp` capture that the probe reads would be dropped by `flux_names::is_video_file`.
   - One list should own this (flux-names, or a flux-core constant). Formats the probe can't read (`iso`, `strm`, `wmv`, `flv`, `rmvb`) should route to the ffprobe fallback worker.
4. **Identity edge cases flux-scan must handle.**
   - Mille reports `dev = ino = 0` on Windows; the catalog's "unknown" is `None` (the smoke run maps it).
   - `ScanEntry.name` is an `OsStr` (non-UTF-8 kept), but `FileFacts.rel_path` is a `String`. Lossy conversion could alias two files. Skip and warn until there's a policy.
5. **`ScanReport.unread`** (directories that didn't answer) has no home in `ScanStatus` (deferred core change). Until then, never tombstone under an unread directory, and surface it through `last_error` or `SourceAvailability`.
6. **Extras arrive before their movie:** 40 were `files_pending` mid-run. All were resolved by the end, as designed.

### 4.2 Catalog probe vs playback index (flux-probe vs flux-hls)

`cargo run --release -p flux-server --example probe_vs_index` → `bench/results/flux-integrate-probe-vs-index.json`. It covers 24 files: 20 feature clips and 4 playback-research files. I also ran it by hand on the 16 probe-fixture files. Each file is planned for 6 client presets, three ways: `HlsEngine::plan` (index), `finalize_plan(plan(flux-probe result))` (what `POST /play` can do from the catalog), and the raw probe plan (a dry run).

| check | result |
|---|---|
| Track `(kind, index)` lists | **identical on 24/24 corpus files**. 2 fixtures differ only by an extra MP4 text/chapter track, which flux-hls lists as a Subtitle at the end. |
| Plans, index vs catalog probe finalized | **0/144 differ** after the profile fix (6/144 before: every preset on `long_2h_360p.mkv`) |
| Plans, index vs dry run from the probe alone | 23/144 differ (expected, see 4 below) |

Findings:

1. **Fixed: H.264 profile naming.**
   - flux-probe uses ffprobe's names, "Constrained Baseline" (which its 0-mismatch ffprobe differential depends on); flux-hls's index says "Baseline".
   - The planner compared the strings exactly, so planning from the catalog meant a video transcode on every client.
   - The fix is subset matching in the planner. Note that ffprobe also names "High 10 Intra", "High 4:4:4 Predictive" and so on, which the presets don't list.
2. **`has_index` means different things.**
   - **Probe:** "Cues/stss are cheap to load".
   - **Planner:** `!has_index` → `NoIndex` → transcode.
   - **flux-hls:** it remuxes Cue-less MKV through `ClusterScan`.
   - So `live_unknown_size.mkv` plans "Transcode" from the catalog probe but "Direct stream" from the index (4 presets).
   - Either the planner should not emit `NoIndex` for Matroska/MP4, or `POST /play` must plan from the index (recommended anyway, §5.2 #2).
3. **Files that probe fine but can't be indexed, so HLS fails:**
   - `quicktime_ac3.mov` ("moov: missing box: dac3"; mp4-atom needs `dac3`, QuickTime AC-3 doesn't carry it);
   - `fragmented.mp4` ("no keyframe index": fragmented MP4 isn't indexed);
   - `h264_ac3.avi` ("no keyframe index").
   - These should fall back to the ffmpeg transcode path instead of erroring. Owner: flux-hls.
4. **Engine-only limits aren't visible to a probe-only plan.**
   - Opus/Vorbis/PCM can't be transcoded from MKV/MP4 yet, so the AAC rendition is dropped.
   - TS/M2TS/AVI audio is never copied (always AAC).
   - The `finalize_plan` step fixes these, but needs the asset. A dry-run endpoint should either prepare the asset (2–9 ms for MKV; 100–200 ms ffprobe scan for TS/AVI) or flux-hls should expose a probe-only `finalize` (a pure function of container and codec).
5. **Smaller mismatches:**
   - `Track.id` is the PID in flux-probe but the ffprobe stream index in flux-hls for TS/M2TS, so never key anything on `id` across crates.
   - The probe's language "und" vs `None` in the index.
   - Audio `bit_depth` is `None` in the probe vs 32 (decoder sample format) in the index.
   - TS/AVI `default` is false vs true.
   - flux-hls's own ffprobe codec table lacks `mjpeg` ("can't decode Other video"). The architecture allows hls → probe, so reuse `flux_probe::ffprobe::codec_of`.

### 4.3 API seams the server will hit

- **Items sorting.**
  - The web sends `title|-title|year|-year|added|-added`.
  - flux-catalog's keyset pagination supports `Title` and `AddedDesc` only.
  - Either the catalog adds the other four (they need their own keyset cursors), or the web restricts its sort menu.
- **Index cache.** The cache holds the absolute path: `PlaybackIndex.source = {path, size, mtime}`.
  - The catalog keeps the asset across renames (`FileMove`), so a catalog-cached index (`put_asset_index` / `asset_play_index`) fails `Asset::with_index` after a move.
  - The server should overwrite `index.source.path` with `asset_location().path` before `prepare_with_index` (size and mtime still guard the content), or rebuild on error.
  - Long-MP4 index JSON can be about 12 MB (expanded sample tables), so consider a binary encoding before storing it in SQLite.
- **Two job systems.**
  - The catalog has a durable `job` table (`claim_jobs` / `finish_job`, `JOB_MATCH`).
  - flux-meta has an in-memory `JobQueue` (lanes, budgets, idempotency keys).
  - Proposal: the catalog is the durable queue; meta's `JobQueue` is the executor, fed by `claim_jobs`.
- **Two index caches.** flux-hls `EngineConfig::index_cache_dir` (JSON files) vs the catalog `asset.play_index` BLOB. Pick the catalog and leave `index_cache_dir = None` in the server.
- **Tailnet status.**
  - `flux_net::TailnetStatus` is `{Off, Starting, NeedsLogin{auth_url}, NeedsApproval, Running(TailnetInfo), Unavailable{reason}, Failed{error}}`.
  - The web expects a flat `{state: off|starting|needs_login|needs_approval|running|unavailable|error, auth_url, approve_url, tailnet_name, owner_login, key_expiry, system_tailscale, last_error}`.
  - `approve_url`, `key_expiry` and `system_tailscale` have no producer yet.
- **Matching to catalog.** `flux_meta::matcher::MatchResult` → `flux_catalog::ItemMetadata` (ids, confidence, evidence) is a straight mapping. Art: `FramePoster.to_art_ref()` → `NewArt { source: Frame, … }` → `add_art`. The server resizes for `/api/v1/art/{id}?w=` (meta already has `image`).

## 5. Next phase, prioritized

### 5.1 flux-scan orchestrator

1. **Decide the layering:** flux-scan → flux-catalog.
   - `examples/pipeline_smoke.rs` is the working shape. It uses `FileFacts` from `ScanEntry`, `load_root_snapshot` + `FileRow::same_stat` for the diff, and `Writer::apply` in batches of ≤ 500.
   - Update ARCHITECTURE.md's arrow (`server → {scan, …} → {probe, names} → core` becomes `scan → catalog`).
   - Moving the types to core instead buys nothing: scan still needs the catalog's functions.
2. **Turn the smoke loop into the real pipeline:**
   - per-root `walk_scan` with one `ReadBoundary` per device (concurrency 16 on SSD/NVMe, 4 on HDD/NAS; Mille's advice);
   - group by directory for siblings;
   - a bounded channel into a parse+probe worker pool;
   - a batcher;
   - `Event::ScanProgress` / `ItemsChanged` from `ApplyOutcome`.
3. **Fingerprints:** compute `fp128` (xxh3 of size + head 64 KiB + tail 64 KiB) with a `PageCache` shared with `flux_probe::probe_cached`, so the probe's first read costs no I/O. The catalog's re-attach-by-fingerprint feature depends on this. The smoke run sends `None`.
4. **Moves and missing:**
   - same `(dev, ino)` at a new path → `FileMove`;
   - absent and not under `ScanReport::unread` → `missing`;
   - root error (`ETIMEDOUT/ENOENT/EACCES/EDATALESS`) → `set_root_availability(false)`, never delete.
5. **Probe fallback:** `ProbeError::UnknownFormat` → a bounded ffprobe worker with a timeout (the probe's fallback has none).
6. **One media-extension list** (§4.1 #3).
7. **Windows `dev`/`ino` = 0** → `None`; non-UTF-8 names → skip and warn (§4.1 #4).
8. **I/O admission and priorities:** play-triggered > watcher > scheduled > deep work (ARCHITECTURE).
9. **Watcher:** Mille's coalesced events grouped by parent → `walk_scan(max_depth = 1)` per directory.
10. **With catalog and meta owners:**
    - the Doctor Who show-folder rule (§4.1 #1);
    - an episode re-key command for anime absolute mapping (§4.1 #2);
    - season packs surfaced as "needs review";
    - enqueue `JOB_MATCH` after apply.

### 5.2 flux-server wiring

1. **Serving.**
   - Build the axum `Router` per `flux_core::api` and serve it with `flux_net::FluxNet::start(cfg, router)`; don't use `axum::serve`, which can't set half-close.
   - Handlers take `Extension<Option<Identity>>` and `Extension<ConnInfo>`.
   - `ServerInfo.path = conn.path.as_str()`; identity → `Writer::ensure_user` → user id for progress.
   - Embedded mode needs `TRUFFLE_SIDECAR_PATH=$FLUX_WORK/bin/sidecar-slim` (branch build).
2. **`POST /play`.** Plan from the **index**, not the stored probe (§4.2 #2, #4):
   - catalog `asset_location` (409 if unavailable);
   - `engine.prepare_with_index` (catalog cache, with the path rewritten, §4.3) or `engine.prepare` followed by `put_asset_index`;
   - `engine.plan(&asset, &req.caps, PlanPrefs { audio_track, subtitle_track, .. })` (track indices agree, §4.2);
   - `engine.warm(&asset, &plan, start_segment)` → `PlayResponse.prefetch`;
   - a `plan_id` registry mapping `plan_id` → `(asset, plan)`. Segments are pure functions of `(asset, recipe, rendition, n)`, so the registry can be an LRU keyed by a hash of the recipe.
3. **`GET /media/{plan_id}/{*rest}`** → `HlsResource::parse` → `engine.serve`. Stream `SegmentBody::into_stream` with `Content-Length` and the engine's `Cache-Control`. `/media/{asset}/file` serves Range (flux-net's `bench_throughput` example has a zero-copy Range handler).
4. **Browse routes** are plain `cat.read(...)` calls: `items_page` (map `api::ItemsParams` → `flux_catalog::ItemsQuery`; 400 for sorts the catalog lacks), `item_detail`, `children`, `search`, `home`, `list_libraries`.
5. **SSE `/api/v1/events`** from the scan orchestrator and availability changes.
6. **Jobs:** the catalog job table + the flux-meta `JobQueue` executor (§4.3). Frame posters → `add_art`; serve `/api/v1/art/{id}?w=`.
7. **Onboarding endpoints:** map `flux_net::TailnetStatus` to the web's DTO, then add the type to flux-core in v2.
8. **Static web UI** (`web/dist`) with `VITE_FLUX_API=/`. A strict CSP needs the hash of the inline theme script.
9. **Then the "core v2" batch** (§3 deferred), coordinated with `web/src/api/types.ts` and `pnpm golden`.

## 6. Web client (report only)

From `cd web && pnpm install --frozen-lockfile=false && pnpm typecheck && pnpm build`:

- **Install:** "Already up to date"; the hashes of `pnpm-lock.yaml` and `package.json` were unchanged.
- **Typecheck:** `tsc -b` clean in 3.4 s.
- **Build:** `vite build` succeeds with 315 modules and no warnings.
- **Toolchain:** Node 26.5.0, pnpm 11.13.1.
- **Largest chunks:** hls.js 575 kB, the MSW mock 471 kB (lazy, mock builds only) and the app 239 kB (raw, not gzipped).

golden-rs (`web/scripts/golden-rs`) still compiles against the updated flux-core (`cargo check --locked --offline`). Its output is unchanged, because §3's applied changes add no fields or variants.

## 7. Re-running

```sh
cd /path/to/flux
export CARGO_TARGET_DIR=$FLUX_WORK/target/integrate
cargo build --workspace --all-targets && cargo clippy --workspace --all-targets && cargo test --workspace
cargo fmt --all --check && RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
for c in flux-net flux-names flux-hls; do cargo clippy -p $c --no-default-features --all-targets; done

# Cross-crate checks (JSON in $FLUX_WORK/bench/results/flux-integrate-*.json)
cargo run --release -p flux-scan   --example pipeline_smoke -- full [--threads 8]   # or: tiny
cargo run --release -p flux-server --example probe_vs_index [-- FILE...]
cargo run --release -p flux-server --example probe_vs_index -- $FLUX_WORK/media/probe-fixtures/*.{mkv,mp4,mov,webm,ts,m2ts,avi}
```

The numbers above are from a shared machine at load 4.6–9 with a warm page cache. Re-run `pipeline_smoke full` after `sudo purge` on a quiet machine for a cold-scan number that compares fairly with Jellyfin's 1,047–1,063 s (benchmarks.md, 2026-10-20). That run also needs fingerprinting added to be complete.
