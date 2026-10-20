# flux-catalog: prototype report

**Crate:** `crates/flux-catalog`. **Contract:** `flux_core::{media, names, api}`.
**Research:** research/02 §4 (identity), §7 (SQLite), §8.2 (file states), §9 (crate sketch); initial-discussion.md §7 (Work/Edition/Asset/Location/UserState); research/03 §11 (playback facts at scan time).

## Summary

flux-catalog is the SQLite catalog (rusqlite 0.40.2, bundled SQLite 3.53.2) behind flux's API. It has:

- **A schema with `user_version` migrations.** The tables are library, root, dir, file, file_meta, asset, asset_item, item, art, user, user_state and job, plus an FTS5 index.
- **Grouping rules.** These turn a `NameGuess` (plus the path) into movie, show, season, episode and extra items. Versions and editions become assets of one item, multi-part files are ordered by part, a multi-episode file links to several episodes, extras attach to their parent, samples are ignored, and unknown files become "unmatched" movies.
- **A single writer thread.** It reads a bounded queue and runs each command in its own savepoint. Commands are group-committed in transactions of ≤ 1000 rows or ≤ 50 ms. Replies are sent only after the commit, so a caller can immediately read its own write.
- **A pool of `query_only` readers.** It works from both sync and async code (`spawn_blocking`). The query functions return `flux_core::api` DTOs: libraries, a paged item list, item detail, children, search, home, and progress.
- **Scanner support.** This covers root snapshots for the in-memory diff and an atomic batch apply that maintains assets, items, availability, tombstones and the search index in the same transaction. Marking a root unavailable never deletes anything.

### Headline numbers

The numbers come from the last full `catalog-bench` run. Load average was 28–47, with 8 agents compiling on the M1 Max and the database on the USB SSD. Results are in `$FLUX_WORK/bench/results/flux-catalog-*.json`.

| Measure | Result | Reference |
|---|---|---|
| Apply the 16.7k-file library (14,760 media files, full grouping) | **591 ms median, 25.0k files/s** (best 27.0k/s; 5 runs: 546–775 ms) | Jellyfin cold scan of the same library: 1,047–1,063 s (benchmarks.md, 2026-10-20; first quoted as 1,442 s from the loaded 2026-10-18 baseline), but that includes ffprobe on every file and local metadata |
| Grouping vs ground truth (perfect-parser guesses) | **Exact:** 2,998 movies, 300 shows, 1,176 seasons, 11,263 episodes, 94 extras, 261 samples ignored | Jellyfin counted 3,149 "movies" |
| 100k synthetic files (80% episodes, 20% movies in 2 versions) | **5.57 s median, 17.9k files/s** (best 19.1k/s); 129k SQL rows/s | research/02 §7.1: 110–270k plain rows/s |
| Catalog cost of a zero-change rescan, 14.7k files | **8.6 ms snapshot + 1.3 ms diff; 0 writer commands, 0 transactions** | Jellyfin unchanged rescan: 18.8–20.8 s (benchmarks.md) |
| `load_root_snapshot`, 100k rows | **62–65 ms** (min 62.0); diff vs enumeration 11.7 ms | research: 25–32 ms for 4-column rows |
| Reader point query (file by id) while the writer applies 100k files | **p50 17.5 µs, p99 0.52 ms** (idle: p99 0.16 ms) | research: p99 0.11–0.92 ms |
| `item_detail` (≈10 queries plus probe JSON) during that write | **p50 135 µs, p99 1.77 ms** (idle: p99 0.69 ms) | target: API p99 < 50 ms |
| Search, 3–6-char prefixes of real titles (library) | **p50 0.48 ms, p99 1.5 ms** | — |
| Search, deliberately broad prefixes over 200k synthetic items during writes | p50 3.0 ms, **p99 22.9 ms** | — |
| Database size | **14.6 MB per 10k files** (library: 21.6 MB for 14,760 files) | Jellyfin's `jellyfin.db` in its baseline state dir: 85 MB (may also hold the tiny run; providers were part of that run) |

These numbers are noisy. Between full runs, the 100k insert median ranged from 5.6 s to 12 s, the snapshot from 62 to 343 ms, and even the pure in-memory HashMap diff went from 12 to 67 ms. That variance comes from CPU contention by the other agents, not from the catalog. Every number is re-runnable with one command (below); the lead's quiet-machine re-run should be authoritative.

## Public API

```rust
use flux_catalog::{Catalog, CatalogConfig, read, scan_support, ItemsQuery, ItemSort, ScanBatch, FileUpsert,
                   FileFacts, ProbeUpdate, FileMove, FileRow, FileState};

let cat = Catalog::open("/data/flux/catalog.db", CatalogConfig::default())?; // migrates, starts the writer
let w = cat.writer();                         // Writer: Clone + Send + Sync

// Every write returns a Request<T>: `.await` (async), `.wait()` (thread),
// or `.submit()?` → Reply<T> to pipeline commands and collect replies later.
let lib = w.create_library(CreateLibrary { name, kind: LibraryKind::Movies, roots }).await?;
let out: ApplyOutcome = w.apply(ScanBatch { root_id, upserts, moves, missing, dirs, removed_dirs, cursor }).await?;
w.set_root_availability(root_id, false).await?;   // never deletes; assets report available=false
w.purge_tombstones(now_ms).await?;                // the caller schedules this (e.g. daily)
w.put_progress(user_id, item_id, Some(asset_id), pos_ms, dur_ms).await?;  // ≥90% → watched
w.set_watched(user_id, show_or_item_id, true).await?;
w.ensure_user(identity).await?;                   // tailnet login → user id
w.update_item_metadata(item_id, ItemMetadata { .. }).await?;   // flux-meta; respects 'locked'
w.add_art(item_id, NewArt { .. }).await?;
w.put_asset_index(asset_id, version, bytes).await?;            // flux-hls playback index cache
w.claim_jobs("match".into(), 32).await?;  w.finish_job(id, Ok(()), retry, max).await?;
w.exec(|conn| { /* arbitrary write in its own savepoint */ Ok(x) }).await?;
w.flush().await?;  w.rebuild_search_index().await?;  w.delete_library(id).await?;

// Reads: plain functions over &Connection, run on the pool.
let page = cat.read(move |c| read::items_page(c, &ItemsQuery { library_id: Some(id), sort: ItemSort::Title,
                                                               cursor, limit: 100, ..Default::default() })).await?;
read::item_detail(c, item_id, Some(user_id))  -> Option<api::ItemDetail>   // assets + probe + ids + evidence
read::children(c, item_id, user)              -> Vec<api::ItemSummary>     // seasons (Specials last) / episodes / extras
read::search(c, "matr", 20, user)             -> Vec<api::ItemSummary>
read::home(c, Some(user_id), 20)              -> api::Home                 // continue watching, recently added, per-library rows
read::list_libraries(c) / read::library(c, id) -> api::Library
read::progress(c, user, item), read::user_by_login(c, login), read::asset_location(c, asset_id),
read::asset_play_index(c, asset_id), read::art(c, art_id), read::item_assets(c, item_id)

// Scanner side (flux-scan): diff in memory, send only the changes.
let snap: HashMap<String, FileRow> = cat.read_blocking(|c| scan_support::load_root_snapshot(c, root_id))?;
row.same_stat(&facts)   // (dev, ino, size, mtime) unchanged → no fingerprint, no probe, no write
scan_support::{list_roots, root_info, load_dirs, find_by_fp128, find_by_dev_ino}
```

`ScanBatch` semantics:

- **`upserts`** are keyed by `(root, rel_path)`, so the same call handles both added and changed files.
  - `ProbeUpdate::{Keep, Probed, Failed}`.
  - `guess: None` keeps the stored guess.
  - A new path whose `fp128 + size` matches a tombstoned file **re-attaches** to that file. This covers a cross-root or cross-volume move, a copy, or a restore, and it keeps the asset, the item and the watch state.
- **`moves`** come from the scanner's `(dev, ino)` detection. They keep the file id. A move onto an occupied path evicts the stale row.
- **`missing`** holds file ids. Each one is tombstoned for `WriterConfig::tombstone_grace` (default 30 days). Its items are hidden but kept, and the purge never runs on an unavailable root.
- **`cursor`** is persisted in the same transaction.
- **Empty and large batches:**
  - An empty batch never reaches the writer, which is how "zero writes on an unchanged rescan" holds.
  - A batch larger than `max_files_per_job` (default 128) is split into consecutive atomic jobs, so each transaction stays near 1000 rows. The cursor rides on the last part and is dropped if any part failed. The error in that case is `CatalogError::BatchFailed`.

## Design notes and the measurements behind them

- **Domain model** (initial-discussion §7):
  - **Location and FileObject:** `file` holds `(root, rel_path)`, `(dev, ino, btime)`, and `fp128`/`oshash`.
  - **Encoding:** `asset` holds one playable encoding, with edition and part.
  - **Work:** `item` is the work, as movie, show, season, episode or extra.
  - **User state:** `user_state(user, item, asset)` keeps the asset with the position, so editions are distinguishable later.
  - **Asset links:** `asset_item` links assets to items. It generalises the spec's `asset_episode`: every asset links to its primary item, and a multi-episode file links to each of its episodes.
- **Playback facts are stored at scan time** (research/03 §11):
  - The asset row carries container, duration, codec, width/height, HDR, audio summary, a label ("4K HEVC HDR10 · TrueHD Atmos 7.1") and badges (4K/1080p/720p/SD, DV/HDR10+/HDR10/HLG, Atmos, 7.1/5.1).
  - The full `ProbeResult` is kept for the planner, so lists never parse probe JSON.
  - Item badges are merged by the writer: best resolution, union of HDR formats, Atmos, best channel layout.
- **Availability is derived and denormalised** by the writer as `item.state`, which is one of `available`, `unavailable` or `missing`. Only rows that change are written:
  - leaves are recomputed from their files;
  - seasons and shows are recomputed from their children.
  - Browsing hides `missing` items and shows `unavailable` ones; their assets have `available=false`.
- **Grouping:**
  - **Movies:** an explicit tmdb, tvdb or imdb id wins. Otherwise the key is the normalised title (folded diacritics, `&`→and, punctuation collapsed) plus the year.
  - **Year rules:** a guess without a year joins the only same-titled item. A dated guess adopts a yearless item. Two candidates mean ambiguity, so a new item is created: remakes stay separate, and `Show/Season 01` joins `Show (2008)`.
  - **Extras:** they attach structurally, to the movie or episode files in the extra's parent folder (skipping `Featurettes/`, `Extras/` and similar). Failing that, they attach by the guess's title. An extra that arrives before its movie stays `pending` and is retried after each batch that places a movie or episode.
  - **Unknown files:** they become `unmatched` movies titled by the cleaned filename, and the matcher gets them first through the job queue.
- **Writer:**
  - Commands run in a `SAVEPOINT`, so a failing or panicking command rolls back alone.
  - Commits use group commit. The writer drains the queue until the queue is empty, the transaction reaches 1000 rows, or 50 ms pass.
  - When idle, the writer runs a passive checkpoint, plus `PRAGMA optimize` at most every 10 minutes and on shutdown.
- **Four measured fixes, each found by profiling** (`sample` on the writer thread):
  1. **`wal_autocheckpoint = 16384` pages instead of SQLite's default of 1000.**
     - In WAL mode with `synchronous=NORMAL`, commits don't fsync, but each autocheckpoint fsyncs both the WAL and the database.
     - On the USB SSD the writer spent most of a 100k insert inside `fsync`. It got worse as the database grew, because each transaction dirties more scattered index pages.
     - Effect: 100k insert went from **21.5 s to 5.8 s** (same build, back to back). `journal_size_limit` caps the WAL file at 64 MB.
  2. **FTS5 is synced explicitly by the writer** (`fts.rs`), not by triggers.
     - Any trigger makes an INSERT a multi-write statement, so SQLite opened and truncated a statement journal on every item insert.
     - The trigger's `kind IN (...)` also built an ephemeral b-tree per row.
     - One Rust predicate decides what is indexed: movies, shows, and episodes with a real title. `rebuild_search_index` repairs the index.
  3. **The cold JSON (probe, guess) lives in a 1:1 `file_meta` table.**
     - With the JSON inline, `file` rows were about 1 KB, so a snapshot scanned about 10× more pages.
     - Snapshot of 100k rows: **114–400 ms → 62–65 ms**.
     - This deviates from the spec, which puts `probe`/`guess` on `file`. `probe_version`, `parse_version` and `probe_error` stay on `file`.
  4. **Stored JSON drops `null` members** with a single-pass byte filter.
     - serde reads a missing `Option` field as `None`, so the round trip is exact; a test checks this against a `Value`-tree reference implementation.
     - Probe JSON shrank from 905 to 587 bytes. The `Value`-tree version had cost about 15% of writer time.
- **Search runs in two phases.**
  1. FTS5 picks the 300 best hits by its weighted bm25 `rank`: title 10, original title 5, overview 1.
  2. Those hits are re-ranked: exact title, then title prefix, then movies/shows before episodes, then bm25.
  - Ranking every match instead cost p99 112–154 ms for broad prefixes over 200k items.
  - User input becomes quoted prefix terms, so the query is injection-safe.
- **Robustness:**
  - A corrupt stored guess or probe is logged and treated as absent. Before this fix, one bad pending extra failed every later batch for its root; a test now covers this.
  - Read paths parse JSON leniently.
  - Mutexes recover from poisoning.
  - There is no `unwrap` on library paths. The one `expect` is the `PooledConn` deref invariant.

## How to run

```sh
export CARGO_TARGET_DIR=$FLUX_WORK/target/agent-catalog
cargo test   -p flux-catalog          # 57 tests: unit + integration (+ real corpus when the SSD is mounted)
cargo clippy -p flux-catalog --all-targets
cargo run --release -p flux-catalog --bin catalog-bench -- --runs 5      # ≈ 1 min; writes flux-catalog-*.json
#   --quick (10k rows, 1 run) · --rows N · --batch N · --autocheckpoint PAGES · --out DIR · --db-dir DIR
```

**Tests (57).** Integration tests use temporary database files with WAL, the real writer and the real pool.

- **Schema:** migrate, idempotent, refuses a newer schema, FTS5 present.
- **Grouping:**
  - versions and editions;
  - remakes and year rules;
  - tmdb id beats title;
  - multi-part ordering and runtime;
  - show/season/episode with multi-episode files and Specials last;
  - extras that arrive before their movie;
  - show extras;
  - samples;
  - unmatched files;
  - a rename that regroups an item but keeps its asset.
- **Browse:**
  - 250-item keyset pagination by title (natural number order) and by date added;
  - kind and library filters;
  - search (prefix, diacritics, ranking, placeholder titles excluded, injection);
  - progress, continue watching and the 90% rule (counted once);
  - set_watched for a whole show;
  - metadata and art flow (an episode falls back to its show's poster);
  - delete library;
  - async API.
- **Availability:**
  - an unavailable root keeps its items; a batch for that root brings it back;
  - a tombstoned file re-attaches by fingerprint and keeps its watch state;
  - a move onto an occupied path evicts the old row;
  - purge, which is paused while the root is unavailable;
  - a re-probe updates the badges;
  - a probe failure leaves the file browsable;
  - batch splitting with injected failure (temp trigger): the cursor is held and the other parts commit;
  - corrupt-JSON resilience.
- **Writer:**
  - a failing or panicking job rolls back alone;
  - replies arrive after commit;
  - 500 jobs share transactions.
- **Real media:**
  - `library-tiny`: the 424 real files are stat'ed, named from the manifest, and given seed probes. The resulting counts must match the ground truth exactly, no movie item may merge two films, a re-stat produces a zero diff, and the FTS index passes its integrity check.
  - The features manifest's ffprobe facts produce the expected labels and badges.

## Known gaps

- **Not yet integrated with sibling crates.**
  - Guesses come from the manifest ground truth (a perfect parser) and probes from the 7 seed files via ffprobe JSON (`synth.rs`).
  - Grouping quality with real `flux-names` output is untested here; flux-names reports 100% on this manifest.
  - HDR10 that lives only in SEI (`hevc_main10_hdr10_eac3.mkv`) shows no badge until flux-probe supplies it.
- **Not implemented:**
  - Remux/re-encode identity: matching by probe signature to give a re-encoded file the same edition with a new asset (research/02 §4.2 step 3).
  - "Next up" on home.
  - Show- and season-level badges and unwatched counts.
  - Edition-aware resume. The asset is stored, but there is no duration-mismatch rule yet.
  - NFC/NFD path normalisation for SMB.
- **Matching state:** a user lock (`match_state='locked'`) is honoured but nothing sets it yet.
- **Scheduling:** purge and `PRAGMA optimize` beyond the idle hook are the caller's job.
- **Search is prefix + AND only:** no typo tolerance. Broad prefixes over very large libraries cost about 10–23 ms p99.
- **Size:** about 1 KB per file today, mostly probe JSON.
  - A real Blu-ray remux with 20–30 tracks will have 5–10 KB of probe JSON.
  - Follow-ups: zstd with a trained dictionary, or a per-track table.
- **Snapshot speed:** 62 ms per 100k rows is about 2× the research's 4-column figure. The 12 columns and path `String`s cost that. An index-only or borrowed-key variant is possible if it matters.
- **Split batches:** if an intermediate part's commit fails, the batch reports `BatchFailed`, but parts in other transactions may already have committed. Upserts are idempotent, so a rescan heals this.
- **Purge loses history:** purging a tombstoned item cascades to its `user_state`, so history is lost after the grace period.
- **Migrations:** V1 was edited in place during the prototype; there are no deployed databases. Future changes must append migrations.
- **Pending extras:** an extra with neither a parent folder nor a parent title stays `pending`. It is counted in `ApplyOutcome.files_pending` but not shown anywhere.

## Proposed flux-core changes

1. **`#[serde(skip_serializing_if = "Option::is_none")]` (with `default`) on the `Option` fields** of `ProbeResult`, `Track`, `NameGuess` and `ItemSummary`.
   - This gives smaller JSON on disk and on the wire.
   - The catalog could then drop its null-stripping pass.
2. **Scan-diff types in core.** The architecture lists `flux-scan` and `flux-catalog` as siblings, but flux-scan needs the diff types.
   - Either allow `flux-scan → flux-catalog`, or move `FileFacts`, `FileRow`, `FileState`, `ScanBatch`, `FileUpsert`, `ProbeUpdate`, `FileMove`, `DirRow` and `ApplyOutcome` (all plain data) into a `flux_core::scan` module.
3. **`api::ItemSort` / items-query parameters** for `GET /libraries/{id}/items`: `kinds`, `sort=title|added`, `cursor`, `limit`. These are defined locally today as `flux_catalog::{ItemsQuery, ItemSort}`.
4. **An `ItemSummary.available: bool`** so offline items can be greyed out without loading assets. Also an **`unwatched_count`** for shows and seasons.
5. **A `NameGuess` contract for extras:** `title`/`year` name the *parent* work, and `episode_title` is the extra's own title. The catalog relies on this, with the folder structure as a fallback.

## Phase C

*2026-10-19, branch `phase-c/catalog`.* Everything in this section ran on a **shared machine** (M1 Max; another project's dev server and three other agents were running). The 1-minute load average is recorded in every result file; it was 3.5–16 during these runs. The database and WAL lived on the shared USB SSD. Checkpoint `fsync` there dominates run-to-run variance, so compare numbers only at similar load.

### What changed

Schema v3 is one appended migration, with an in-place upgrade from v2. A v1 → latest upgrade test still passes.

1. **Show folders (integration §4.1 #1).**
   - **Rule:** a different top-level show folder is a different show, unless provider ids agree. Adopt/join year rules still apply within one folder.
   - **Show folder:** the episode's directory with trailing season folders removed. Removed folders are `Season 1`, `S01`, `Specials`, `Series 2`, `Staffel 3`, `Disc 1`, pure numbers such as `2024`, and season packs such as `Show.S01.1080p-GRP`. It is lowercased and root-relative (`group::show_folder`, `is_season_dir`).
   - **Ownership:** a show owns the folders it has episodes in (`item_folder`). The same folder name on another root is the same show.
   - **No folder:** files at the root, and shows from pre-v3 catalogs, keep the old title/year rules.
   - **Moves:** a moved episode file prefers its current show when the new folder has no same-titled owner. Renaming `Dark/` to `Dark (2017)/` therefore keeps the show, adopts the year, and keeps everyone's watch state.
   - **Ids agreeing later:** when ids only agree after matching (`The Wire/` and `The Wire (2002)/` both matched to TMDB 1438), `merge::works_sharing_ids(item)` finds the pair and `Writer::merge_items(from, into)` joins them.
     - Children merge by season, episode and extra identity; the rest moves over.
     - Assets, art (the target's pin is kept), folder claims, absolute-episode maps and user state come along.
     - The outcome feeds `ItemsChanged`.
   - **Tests:** `tests/show_folders.rs` has 9 tests:
     - `Show/` vs `Show (1963)/`, both orders and the same batch;
     - `Show (2005)/` vs `Show (1963)/`;
     - one folder with mixed yeared and yearless guesses, both orders;
     - two roots;
     - ids in folder names;
     - a folder rename with watch state;
     - flat files;
     - merge after matching;
     - the season-folder classifier.
   - **End to end:** `pipeline_smoke full` now gives **shows 300/300 and episodes 11,263/11,263** (before: 299 and 11,255). Seasons are 1,095/1,176: the 767 anime files with absolute numbering still sit in season 1 by design, until flux-meta re-keys them (item 2). Movies are 3,811/3,811 placed, and the unchanged rescan does 0 writer commands. Result: `flux-catalog-c-pipeline-full.json`.

2. **Episode re-key (§4.1 #2).**
   - **Finding work:** `rekey::absolute_episodes(c, show)` lists the show's assets that were parsed by absolute number only. flux-meta maps them.
   - **Applying:** `Writer::rekey_episodes(Vec<EpisodeRekey { asset_id, season, episodes, absolute }>)` applies the whole list atomically:
     - it re-links each asset to the target episodes, creating them if needed (double episodes are allowed);
     - it carries every user's state to the targets: watched wins, the newer position wins, and play counts take the max;
     - it deletes old placeholder episodes, and seasons, that are left empty;
     - it records `absolute → (season, episodes)` in `episode_map`.
   - **Later files:** `apply` consults that map for new files and re-parsed ones with the same absolute number, so they land in the right place straight away.
   - **Outcome:** `RekeyOutcome::event()` gives the `Event::ItemsChanged`.
   - **Tests:** `tests/rekey.rs` (2).

3. **Sorts and filters.**
   - **Sorts:** `items_page` implements every `api::ItemSort` (`title`, `-title`, `year`, `-year`, `added`, `-added`) with keyset cursors.
     - Ties break by sort title, then id. Unknown years sort last in both directions.
     - A cursor names its sort, so a cursor from another order is refused.
     - `flux_catalog::ItemSort` is now a re-export of `flux_core::api::ItemSort`, so the server passes the sort straight through.
   - **Indexes:** every sort of a library's top level walks a partial index, with no sort step (a test asserts the query plans): `item_top_title`, `item_top_lib_added`, and `item_top_year` / `item_top_year_desc` (expression indexes).
   - **Total:** `Page.total` is set on every page and counts the same filtered set.
   - **Per-user filters:**
     - `in_progress` is the user's started items plus their seasons and shows, computed once per query.
     - `unwatched` is a leaf the user hasn't finished, or a show or season with such an episode.
     - Season and show badges are now merged from their episodes, so `4k`/`hdr` read each row's own badges.
   - **List summaries are built in a handful of set queries per page** (ancestors, art with parent fallback, progress, unwatched counts via `json_each`) instead of 4+ point queries per row.
   - **Unwatched counts:**
     - Seasons and shows carry a writer-maintained `episode_count`.
     - A page whose shows hold more than 3,000 episodes subtracts the user's finished episodes, counted in one pass over the user's own state rows.
     - Smaller pages check each episode.
     - A unit test checks that both strategies agree.

4. **Playback index cache (§4.3).**
   - **Storage:** `Writer::put_asset_index(asset, PlayIndex { format, version, source_size, source_mtime_ns, bytes })`.
     - The bytes are opaque; the format tag is a free string (`INDEX_FORMAT_JSON = "json"`, or flux-hls's binary tag).
     - The index lives in its own `asset_index` table, so asset rows stay narrow. v3 drops `asset.play_index*`.
   - **Reading:** `read::asset_play_index` serves the index only while the file's catalog `(size, mtime_ns)` equals the stat it was built from. It returns `CachedPlayIndex { path, size, mtime_ns, available, … }` with the file's **current** path, so the server rewrites `index.source.path` after renames.
   - **Invalidation:** a scan that sees the file change deletes the row. `clear_asset_index` drops one by hand.
   - **Test:** `tests/art_index.rs`.

5. **One durable job queue (§4.3 "Two job systems").** The `job` table is rebuilt and old rows are migrated (`jobs.rs`).
   - **Idempotency:** `(kind, key)` is unique. A key that exists in any state is a no-op (`Enqueued::Duplicate`); re-enqueueing with a higher priority bumps a queued job.
   - **Supersede:** there is at most one queued job per `(kind, item)`. A new key rewrites the queued one, and finished records of older keys are dropped.
   - **Priorities:** `jobs::priority` (interactive 0, unmatched 10, match 20, art 40, background 60); FIFO within a priority.
   - **Dependencies:** with `after`, an item's art job waits for its match job.
   - **Leases:** `claim_jobs(ClaimJobs { worker, kinds, limit, lease })` leases jobs; `extend_job_lease` is the heartbeat. An expired lease counts as a failed attempt.
   - **Finishing:** `finish_job(id, attempt, Done | Retry(e) | Fail(e))` returns `Finished::{Done, RetryAt, Failed, LeaseLost}`.
     - A stale worker gets `LeaseLost`.
     - Retries back off exponentially: base 30 s, ×2 per attempt, cap 6 h, plus up to 12.5 % deterministic jitter, until `max_attempts` (default 5).
   - **Admin and polling:** `requeue_failed_jobs`; `jobs::{job_counts, next_job_due, job, item_jobs}` (the executor sleeps until `next_job_due`).
   - **On apply:**
     - Every new top-level work gets `match` + `art` (art after match).
     - A show that gains episodes re-arms its single art job; that one job covers posters and episode thumbs.
     - A work whose parse identity changes (adopted year, learned id) gets a fresh match, unless it is `matched` or `locked`.
     - `WriterConfig::enqueue_jobs = false` turns this off.
   - **Why one art job per work, not per episode:** per-episode art jobs added 11k rows to the library apply, and pushed the WAL over the checkpoint threshold (profiled: the writer sat in checkpoint `fsync`).
   - **Tests:** `tests/jobs.rs` (7): enqueue on apply without duplicates on rescans, dedupe/supersede, priorities and kind filters, lease expiry plus a stale finish, backoff to max attempts and requeue, match re-queue on year adoption, and art re-arm.

6. **User state per user.**
   - **Existing pieces:** `ensure_user` (Identity login), `set_progress` (the 90 % rule; `put_progress` is kept as an alias) and `set_watched` (show and season mark every episode).
   - **Multi-episode files:** finishing one marks every episode it holds.
   - **`read::continue_watching(user, n)`:**
     - It lists the most recent activity first, one entry per movie or show.
     - Anything with a resume point appears, including a rewatch.
     - After the user *finishes* an episode, the next unfinished episode appears: S1E3 → S1E4, a season's last → the next season's first. Specials only follow specials.
     - Finished movies and shows drop out.
     - `home()` uses it.
   - **`unwatched_count`** is per user for shows and seasons.
   - **Tests:** `tests/user_state.rs` (4).

7. **Art.**
   - **Rows:** each item has rows with `source`, `attribution` and `generator` (core v2 `ArtRef`).
   - **Selection:** the best image is the **user's pin, else provider > local > frame > generated**, newest first within a source.
     - `pin_art(art_id)`, `unpin_art(item, kind)`, `remove_art`, `read::item_art` (picker, best first) and `read::best_art`.
     - Adding art no longer selects it.
     - `add_art` is idempotent per (item, kind, source, url/path).
   - **Tests:** `tests/art_index.rs`.

8. **Bench:** `catalog-bench --phase c` (below).

**Tests:**

- `cargo test -p flux-catalog`: **85 passed** (57 before).
- Workspace gate: `cargo build --workspace --all-targets` and `cargo clippy --workspace --all-targets` with 0 warnings, `cargo test --workspace` **380 passed / 0 failed / 4 ignored**, and `cargo fmt --all --check`.
- `RUSTDOCFLAGS="-D warnings" cargo doc -p flux-catalog --no-deps` is clean.

### Numbers

Bench workload:

- **Users:** 1,000 users with about 200 state rows each (198k rows). Each user has binge runs of watched episodes with the next one in progress, plus single movies.
- **Library:** the 16.7k-file library (14,760 media files) with manifest names, seed probes and real stat.
- **100k:** 100k synthetic files (80k episodes in 1,000 shows, 20k movie files, half of them 4K HDR10).
- **Browse requests:** first page = 120 items, as the web client asks; page 2 = 500 via the cursor (the catalog clamps the web's 1,000). Each request uses a random user.
- **Samples:** 100 per sort × filter per page; that is 6 × 5 = 30 combinations.

Results JSON: `$FLUX_WORK/bench/results/flux-catalog-c-*.json`.

| Measure | Library (3.3k works) | 100k synthetic (11k works) |
|---|---|---|
| Apply, with jobs (median of 3) | **596 ms, 24.8k files/s** (590–789) | **7.4 s, 13.5k files/s** (best 5.8 s, 17.3k/s) |
| Apply, jobs off | 611 ms (594–725); earlier runs put jobs at +8–20 % | — |
| Browse page 1 (120), idle: p50 median / worst p50 / p99 median / worst p99 | 1.7 / 3.8 / 2.8 / 13.9 ms | 3.7 / 7.4 / 4.3 / 16.9 ms |
| Browse page 2 (500), idle: p99 median / worst | 4.2 / 18.1 ms | 5.0 / 20.5 ms |
| Browse page 1 while the writer applies into another library (7.3k / 8.5k files/s) | p50 ≤ 4.7 ms; p99 median 4.8, worst 20.5 ms | p50 ≤ 9.3 ms; p99 median 8.8, worst 58.3 ms |
| Browse page 2 under writes: p99 median / worst | 9.9 / 22.8 ms | 11.2 / 74.9 ms |
| Continue watching (20), p50/p99 idle; under writes | 1.16/1.42; 1.23/3.45 ms | 1.39/1.64; 1.51/2.52 ms |
| Home (20), p50/p99 idle; under writes | 1.47/1.83; 1.74/4.91 ms | 1.47/1.79; 1.69/4.08 ms |
| Show summary with per-user unwatched count, p50 | 32 µs | 48 µs |
| Database after apply | 22.7 MB = **15.4 MB / 10k files** (phase B: 14.7) | 150.1 MB = **15.0 MB / 10k** (phase B: 14.6) |
| 1k users' state | user_state 7.3 MB + `user_state_recent` 3.8 MB (≈ 56 B/row) | same |

- **Worst cases:**
  - The worst browse p99s under writes are single outliers among 100 samples, at load 8–10.
  - The slowest idle combinations are `unwatched` on 100k: 7.0–7.4 ms page-1 p50. The filter evaluates every show for `total`.
- **Optimisation steps:**
  - Library, worst page-1 p50 (smoke runs with 50 users and 3 samples): per-row summaries 16.6 ms → batched summaries 3.8 ms.
  - 100k, idle, worst page-1 p50 / worst page-2 p99:
    - batched summaries: 20.6 / 68.7 ms (500-show pages did 40k per-episode state lookups);
    - plus `episode_count` and the adaptive unwatched strategy: 8.1 / 14.2 ms;
    - final run: 7.4 / 20.5 ms.
- **Job queue** (20k jobs, `flux-catalog-c-jobs.json`):
  - enqueue 253k/s in batches of 1,000;
  - re-enqueueing all 20k as duplicates: 18 ms;
  - **claim 32: p50 0.20 ms, p99 0.36 ms**. It was 4.6 ms p50 before `job_ready` became per-kind: the planner had used the `(kind, key)` index and sorted;
  - finish p50 27 µs;
  - one client claiming and finishing sequentially drains 23.9k jobs/s.
- **Write cost vs the pre-change build** (`flux-catalog-c-apply-ab.json`; the phase-B bench built from `main` in a scratch export, interleaved):
  - **SQL rows:** +4.9 % (library) and +3.2 % (100k).
  - **Library apply, best of 3:** 552 ms vs 612 ms at load 7–16.
  - **With automatic checkpoints disabled** (`--autocheckpoint 1000000`), the builds are indistinguishable:
    - library: 655–825 ms vs 649–906 ms;
    - 100k: 5.0–8.1 s vs 4.7–8.9 s.
  - **Conclusion:** the remaining wall-time gap is checkpoint `fsync` on the shared SSD.
  - **pipeline_smoke A/B at load 9–10:**
    - Movies apply: 213–309 vs 182–216 ms;
    - TV apply: 755–1,144 vs 946–1,100 ms.

Re-run:

```sh
cd $FLUX_WORK/wt/flux-catalog        # or the repo once merged
export CARGO_TARGET_DIR=$FLUX_WORK/target/c-catalog
export TMPDIR=$FLUX_WORK/tmp/catalog/tmp
nice -n 19 cargo test -p flux-catalog -j 4
nice -n 19 cargo run --release -p flux-catalog --bin catalog-bench -j 4 -- --phase c --runs 3 --samples 100
#   --users N --rows-per-user N --rows N (synthetic size) --db-dir DIR (default tmp/catalog/db) --out DIR
nice -n 19 cargo run --release -p flux-scan --example pipeline_smoke -j 4 -- full
#   writes bench/results/flux-integrate-pipeline-full.json (the example's fixed name)
```

### Server wiring (what the new API expects)

- **`GET …/items`:** map `api::ItemsParams` straight to `ItemsQuery { sort, filter, cursor, limit, user_id }`. Every sort is supported, so there is no need for 400 replies.
- **Login and progress:**
  - On login: `ensure_user(identity)`.
  - Progress reports: `set_progress`.
  - `MarkWatched`: `set_watched`.
  - Home: `read::home(user)`.
- **Job executor loop:**
  1. `claim_jobs(ClaimJobs::new(worker, &[JOB_MATCH, JOB_ART], n, lease))`.
  2. For long jobs, `extend_job_lease`.
  3. `finish_job(id, attempt, result)`.
  4. Sleep until `jobs::next_job_due`.
- **Match results:**
  - `update_item_metadata`; then, if `merge::works_sharing_ids` is non-empty, `merge_items` → `ItemsChanged`.
  - For anime: `rekey::absolute_episodes(show)` → TMDB episode groups → `rekey_episodes` → `RekeyOutcome::event()`.
- **Art job semantics:**
  - one job per movie or show, covering show, season and episode-thumb art;
  - provider art if matched, else frames;
  - `add_art` is safe to repeat.
- **Playback:**
  - `read::asset_play_index(asset)`. On a hit, set `index.source.path = cached.path` and call `prepare_with_index`; if that fails, call `clear_asset_index`.
  - On a miss, `prepare`, then `put_asset_index(asset, PlayIndex { format, version, source_size, source_mtime_ns, bytes })`.
  - Leave `EngineConfig::index_cache_dir = None`.

### Known gaps

- **Folder claims:**
  - Claims never shrink. After a folder rename, the old folder name still belongs to the show, so a *different* same-titled show added later under the old name would join it.
  - Pre-v3 shows have no claims until their files are re-placed, so an existing Doctor Who merge in an old catalog is not split. There are no deployed catalogs.
- **Root-level season packs** (`Show.S01.1080p/` directly in the root) have no show folder; title and year rules decide.
- **Merging and re-keying limits:**
  - Merging is explicit (`merge_items`); nothing merges automatically on a match, and there is no split.
  - A manual re-key of a *non*-absolute episode holds only until the file is re-parsed. Absolute numbers persist through `episode_map`.
- **Filters:**
  - `4k`/`hdr` are `LIKE` on the badges JSON, not indexed. A filter that matches nothing scans every top-level item: ~2.5 ms on the library, ~4 ms on 100k.
  - `unwatched` on shows uses `EXISTS` over episodes, so fully watched long shows scan all their episodes.
- **Queue and history scans:**
  - `next_job_due` scans the queued jobs of the given kinds.
  - Done jobs stay as idempotency records; they are deleted with their item, or when a new key supersedes them.
  - Continue watching looks at the user's 500 most recent state rows.
- **Index staleness:** the index cache is guarded by `(size, mtime)`, as flux-hls's own `SourceId` is. An in-place rewrite that keeps both would serve a stale index.
- **Browse tail latency:** browse p99 under concurrent writes is 20–75 ms worst case on this shared machine. The causes are WAL growth and `fsync` on the USB SSD, not query plans (every plan is an index walk).

### Proposed changes outside flux-catalog

1. **flux-core `api::ItemSort` doc.** It says the catalog implements only `title` and `-added` and that the server answers 400 for the others. All six are implemented now; drop that sentence. `flux_catalog::ItemSort` is a re-export of the core type.
2. **flux-core route for pinning art** (the picker): `POST /api/v1/items/{id}/art/{art_id}/pin` and `DELETE /api/v1/items/{id}/art/{kind}/pin` → 204. The catalog side (`pin_art`, `unpin_art`, `read::item_art`) exists.
3. **flux-scan `examples/pipeline_smoke.rs` writes to fixed locations:**
   - its database goes to `WORK = $FLUX_WORK/tmp/integrate`;
   - results go to `flux-integrate-pipeline-<lib>.json`.

   Concurrent agents (and `cargo test --workspace`, which runs the `library-tiny` test with the same `pipeline-tiny-test.db`) can clobber each other. Take the work directory from an env var or `TMPDIR`, and allow a result-name prefix. Running the task's verification command overwrote `flux-integrate-pipeline-full.json`; a copy of the final run is in `flux-catalog-c-pipeline-full.json`.
4. **flux-meta.** Its `JobQueue` becomes the executor of the catalog queue:
   - lane = job kind;
   - the idempotency key is the catalog key;
   - budgets stay in meta.

   The art executor must make episode thumbs for a show's episodes without art.
5. **flux-hls.** Expose the binary index format tag as a constant the server passes to `PlayIndex.format`, and a `PlaybackIndex::with_path(path)` to rewrite the cached path.

## Phase D

*2026-10-19, branch `phase-d/scancat`.* The catalog halves of the phase-D review fixes (scanner side: [scan.md](scan.md#phase-d)) and the footprint work. Shared machine, load 10–180, recorded in the result files. Schema **v4** is one appended migration (two `file` columns, two partial indexes); v1 → v4 upgrades in place.

### What changed

1. **INT-1: a work's state follows its file.** A rename or re-parse that moves a work's *last* file to another work used to leave the user's resume point, watched flags, pinned art and match on the old work, which was then hidden. `Apply::link` now calls `merge::carry_emptied(from, to)` for every old item left without assets:
   - **Same library and kind** (movie → movie, episode → episode, extra → extra): merged as in `merge_items`. Children merge by identity, every user's state is merged with what `to` has (watched wins, the newer position wins, play counts take the max), art moves with the target's pin winning, ids `to` lacks are filled in, folder claims and absolute maps move, and `from` is deleted (`items_removed`).
   - When `from`'s top-level work has no file left either, it follows into `to`'s: a show folder renamed to another title carries the show's pin, ids and folder claims, not just the episodes' state.
   - A **`locked`** match (a user decision) is carried when `to` has none. An automatic match is not: the rename may be fixing it, and `to`'s own match job decides.
   - **Another library or kind:** every user's state is copied onto `to`; `from` stays (hidden), as before.
   - A multi-episode file maps old episodes to new ones by episode number.
   - The same path serves `attach_fingerprints` (below) and cross-root moves.
2. **INT-4: guarded tombstones and moves.** `ScanBatch.expect: Vec<ExpectedRow { file_id, rel_path, size, mtime_ns }>`. A `missing` id or `FileMove` with a guard applies only while the row is still at that path with that stat; otherwise it is skipped (debug log). Unguarded ids behave as before. A guarded move may come from **another root** (same inode: the same file, live or a tombstone there); it counts as `files_reattached`.
3. **INT-6: a failed re-probe keeps the facts.** `ProbeUpdate::Failed` on an existing file keeps the stored probe JSON and the asset's playback facts, clears `probe_version`, sets `probe_error`, and schedules a retry: `probe_attempts` + 1, `probe_retry_at` = now + 1 min × 2^(attempts − 1), capped at 1 day. Exposed as `FileRow::probe_retry_at` in snapshots. A successful probe resets both. A stat change without a fresh fingerprint clears `fp128`/`oshash`, which described the old content.
4. **INT-7: overlapping roots.** `create_library` and `add_root` refuse a root that equals or contains (by whole path components) another root of the same library: `CatalogError::OverlappingRoots { root, other }`. `/m/Movies2` is not inside `/m/Movies`. Roots of different libraries may still overlap.
5. **INT-8:** `merge_items` keeps the source's pin unless the target has its own pin of that kind.
6. **Deferred fingerprints:** `Writer::attach_fingerprints(root, Vec<FileFingerprint>)`. Each row is found by `(root, rel_path)` and updated only while its size and mtime are the fingerprinted ones. A file whose content matches a tombstone is merged into it: the tombstone takes the new path, stat, probe and guess, keeps its id and asset, and is re-placed. The fresh row and its asset are deleted; a work the fresh file had created is carried (INT-1 rules).
7. **Re-attach tie-break.** Among tombstones with the same `fp128 + size`, the same file name wins, then the same folder name, then the most recent tombstone (flux-scan's rule for moves). Lookups use a new partial index `file_fp_missing(fp128, size) WHERE state = 'missing'`. The plain `fp128` index also holds every live copy, thousands per content in the benchmark corpus.
8. **Subtree snapshots:** `scan_support::load_subtree_snapshot(c, root, rel_prefix)` and `load_subtrees_snapshot(c, root, &[prefix])`: one range of the `(root_id, rel_path)` index (`prefix/` ≤ path < `prefix0`).
9. **Caches and the WAL.**
   - Defaults are now writer **16 MiB**, readers **4 MiB** (were 64 and 16).
   - `Writer::begin_bulk` / `end_bulk` (nested, counted on the writer thread, handled between transactions): the writer gets `WriterConfig::bulk_cache_kib` (64 MiB) while a scan writes.
   - When the last bulk writer ends, the cache goes back to 16 MiB, `PRAGMA shrink_memory` runs, then `wal_checkpoint(TRUNCATE)` with a 50 ms busy timeout. The WAL file goes back to 0 bytes unless a reader holds an old snapshot.
   - `release_free_memory()` asks the allocator to return freed pages (works with glibc; a no-op on macOS 26, see scan.md).
   - flux-scan enters bulk mode for library scans and large fingerprint batches, not for watcher batches.
10. **Cached library counts:** `FileCounts` caches each library's (files, probed files, visible items) until the writer commits something that may change `file` or `item` rows.
    - Every typed command that can (apply, attach, availability, purge, merge, rekey, create/delete library, generic `exec`) invalidates it after its commit.
    - User state, art, jobs and metadata don't.
    - Use it through `Catalog::list_libraries().await`, `Catalog::library(id).await`, or `read::list_libraries_cached(c, catalog.file_counts())`. `read::list_libraries` still counts.
11. **Art jobs, newest first.** `claim_jobs` orders art jobs by `priority, id DESC` (new partial index `job_ready_art`); other kinds stay FIFO. `Writer::bump_art(item)` moves the art job of the item's work (an episode's show), and the match it waits for, to interactive priority. It returns `false` when no art job is queued.
12. **`AssetLocation`** carries the recorded `dev`, `ino`, `mtime_ns`, the `root_path` and the `probe_error`, for the server's serve-time identity check (SEC-2) and for showing why a probe failed (ROB-3/ROB-V1).

### Tests

`tests/phase_d.rs` (21; the review's catalog repros are ported): INT-1 (movie retitle with pin, show retitle with every watched episode, a move onto an existing work merging both states, a move to another library), a guarded cross-root move, INT-4 (stale missing and stale move skipped, a matching guard applies), INT-6 (facts kept, backoff doubling, reset on success), INT-7, INT-8 (both pin cases), `attach_fingerprints` (merge into a tombstone, plain store, carry into a renamed work), the re-attach tie-break, subtree snapshots, cached counts (files and merges), newest-first art and `bump_art` (movie, and an episode bumping its show), bulk mode (cache sizes, nesting, WAL truncated), and `asset_location`'s identity. With each fix disabled, 10 of them fail.

`cargo test -p flux-catalog`: **107 passed**. The workspace gate (build, clippy with 0 warnings, test, fmt) is in the final report.

### Numbers

From the flux-scan bench on the full library (`bench/results/flux-d-scancat-scan-20261019-220926.json`, load 25–84):

| | |
|---|---|
| Library listing, uncached (counts every file) | p50 3.8–4.6 ms per call |
| Library listing, cached | **p50 0.006 ms**; the first call after a commit recounts (~4 ms) |
| Whole-root snapshot (TV, 10,949 rows) vs one show folder (24 rows) | 6.2–7.1 ms vs **0.014 ms** |
| WAL after a cold scan | **0 B** (was 64 MB) |
| Idle footprint after a cold scan, flux-server | 84–87 MB (base 80–81 MB): allocator retention, not SQLite caches (scan.md) |

### Known gaps (new)

- **Carrying is by emptiness.** A work keeps its state while it still has *any* asset, including tombstoned ones. A renamed version whose sibling version was deleted earlier leaves the state on the hidden old work (purge would then drop it).
- **Episode carry by number:** a file renumbered within one show (S01E02 renamed S01E03) carries E02's state onto E03. Like `rekey_episodes`, the state follows the file.
- **An automatic match is not carried;** ids the target lacks are filled in (as in `merge_items`) and the target's own match job runs.
- **`FileCounts` is per process:** another process writing the database isn't seen until this one commits.
- **Bulk-mode truncation** gives up after 50 ms if a reader holds an old snapshot; the idle passive checkpoint and `journal_size_limit` still bound the WAL.

### Server wiring for Phase D

See scan.md's "Proposed changes elsewhere (Phase D)". In short: 400 for `OverlappingRoots`, `Catalog::list_libraries`/`library` for `GET /libraries`, `bump_art` when the UI asks for missing art, and the `AssetLocation` identity check in `media.rs`.
