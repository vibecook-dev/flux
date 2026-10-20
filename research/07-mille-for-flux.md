# 07 — Mille as flux's scanner foundation: deep code review + measurements

**Date:** 2026-10-18 · **Subject:** `vibecook-dev/mille` @ `c8160d5` (main, 2026-10-07, workspace version 0.3.4), a local checkout (read-only; verified `git status` unchanged after all work)
**Method:** I read the full walker, read boundary, watcher, store/snapshot, resume and fs modules plus the binding's watch runtime, explorer, stream and event bus, and the mille-truffle and protocol TS. I then wrote and ran a standalone benchmark harness (`$FLUX_WORK/bench/mille-walkbench`, built with `CARGO_TARGET_DIR=$FLUX_WORK/target`). The harness links `mille-core` by path and compares it with walkdir, jwalk, ignore, a plain std walk, and a raw `getattrlistbulk(2)` walker I wrote for this report. I also extracted the 0.3.4 release's jwalk-based walker with `git archive` for an old-vs-new comparison.

**Confidence legend:** **[V]** verified, either by reading code at the cited line or by measuring it here. **[I]** my inference from verified facts. **[U]** unverified or third-party claim.

> **Measurement caveat (important).** The dev machine (M1 Max, 8P+2E cores, 64 GB, macOS 26.5.2) was shared with many other agents during this run. The 1-minute load average ranged from **10 to 140** on 10 cores. I tagged every result with the load at the time and report medians of 3 runs on the two stable trees. Absolute numbers are noisy (±30–50% run to run). The *ratios* between walkers were stable across runs and loads. All runs are page-cache warm (no `sudo purge`). The 2M-entry tree is effectively **vnode-cold**, because it exceeds `kern.maxvnodes = 263,168` (measured with `sysctl`).

---

## 0. Executive summary

1. **mille-core is genuinely napi-free and usable from pure Rust today [V].** That covers the walker (`walk`, `walk_with_ignore`, `walk_batched`, `walk_with_ignore_batched`, plus the more useful `walk_bounded` and `walk_bounded_batched`), the `ReadBoundary` stall-bounded read pool, the notify-based `Watcher`, `coalesce_events`, `RenamePairer`, `EntryStore` and `StoreSnapshot`, nucleo `search`, and the bincode crash-resume snapshot. `RealFs` is **still a stub that returns `Unsupported`** (`crates/mille-core/src/fs.rs:47-86`).
2. **The walker's per-entry record is not a media-scanner record [V].** `WalkedEntry` carries path, parent path, name, kind, size, `mtime_ms`, `ctime_ms` and some flags. It has **no inode, no device, no birthtime, and no nanosecond times**. Its `ctime_ms` is the Unix *status-change* time, not the creation time (`walker.rs:77-96`, `913-924`). Non-UTF-8 names are **silently dropped**, but their subdirectories are still walked (`walker.rs:602-607, 707`).
3. **The watcher cannot resume after a restart [V].** It wraps notify 6.1.1 and notify-debouncer-full 0.3.2 with `NoCache`, a 75 ms debounce, and unbounded `std::sync::mpsc` channels. Both notify 6.1.1 and today's notify `main` (9.0.0-rc.6) **hard-code `kFSEventStreamEventIdSinceNow` and discard event IDs**. Mille's own "resume" (`events_since`) is a **full re-walk** diffed on path + mtime + size (`resume.rs:297-370`).
4. **Two quadratic hot spots sit in the watch path [V].**
   - `coalesce_events` is O(n²). Measured: 1k events take 22 ms, 10k take **7.4 s**, and 40k take **86 s** (`watcher.rs:605-610`).
   - The binding's reconciler re-lists the parent directory **once per event** (`watch_runtime.rs:138-162, 369-386`). It also enumerates the whole known subtree via `paths_under` on each reconcile (`store.rs:456-478`).
5. **Throughput: Mille is middle-of-the-pack [V].** Parallel `getattrlistbulk`, which I prototyped in about 150 lines, is **~2.6× faster than Mille (8 threads)** on both stable trees, and **~4.6× faster on the 2M-entry tree**. jwalk and `ignore` are 1.6–2.5× faster than Mille. Serial Mille is **~1.5–1.9× slower than a naive serial `read_dir + lstat`**. That is the cost of a cross-thread handoff per directory: Mille uses ~15× the user CPU of a std walk.
6. **Memory [V].**
   - `walk()` materializes ~420–485 B/entry (path, parent path and name are each separately heap-allocated).
   - `EntryStore` adds **~685–841 B/entry** of RSS.
   - The binding's `maxCachedEntries` option is parsed and never enforced.
   - Batched walking is cheap: 14 MB for 79.5k entries and 72 MB for 2M.
7. **No OS fast paths [V].** There is no `getattrlistbulk`, `statx`-via-dirfd, `io_uring`, Windows bulk directory info, or FSEvents `sinceWhen`. Every child gets a full-path `lstat` (`read_boundary.rs:1015`), plus a second `stat` for each symlink (`:1022-1026`).
8. **The good parts are worth keeping [V].** The M10 **ReadBoundary** (stall deadline, stuck cap, macOS dataless-folder policy, `unread` reporting) is valuable for NAS and iCloud libraries. Its 2 s default deadline is too short for spun-down HDDs, and plain `walk()` discards the `unread` report (`walker.rs:125-127`).
9. **The watch runtime is the piece flux would most want, and it is NAPI-coupled only by types [V].** It has zero `napi` references, but it emits `*Js` `#[napi(object)]` structs through the napi `EventBus` (`watch_runtime.rs:23-24`). Extracting it means swapping those for core event types plus a sink trait. That is about 1–2 days of work [I].
10. **mille-truffle is Node/TS-only [V].** It has a framed stream (`MLLE` magic, 20-byte header, 32 MiB frames, 4 MiB JSON metadata, 8/32 MiB soft/hard outbound), a **16 MiB per-request file cap**, port 9451, and export roots resolved server-side. There is no Rust host. For flux's "browse a remote device's folders to add a library" flow, the cheapest path is a tiny Rust "list directory" RPC over Truffle that reuses `walk_bounded(max_depth=1)` and Mille's export and policy *ideas*, not its TS protocol.

**Bottom line:** use `mille-core`'s walker, ReadBoundary and IgnoreMatcher now. Do **not** adopt `EntryStore`, the snapshot and projection layer, ICU sort or nucleo for flux's catalog (flux has SQLite). Before relying on Mille's watcher, land the P0 fixes in §8: extract a NAPI-free scan runtime, add a media-grade scan record with identity, fix the two quadratic paths, and make deadlines configurable with an explicit `unread` result. Add a `getattrlistbulk` fast path and FSEvents resume as P1.

---

## 1. Inventory

| Crate / package | LOC (src) | Role | NAPI? |
|---|---:|---|---|
| `crates/mille-core` | ~14.9k (`walker.rs` 1,795; `read_boundary.rs` 1,645; `watcher.rs` 1,774; `snapshot.rs` 2,050; `store.rs` 2,637) | walker, bounded reads, watcher, store/snapshot, search, resume | **No** (`lib.rs:1-10`) |
| `crates/mille-binding` | 9,477 (`explorer.rs` 4,491; `watch_runtime.rs` 1,012) | cdylib: `FileExplorer` class, watch runtime, undo journal, TSFN event bus, file stream | Yes |
| `crates/mille-git` | ~6k | gix reads + git CLI writes | No (and must not depend on mille-core, `Cargo.toml` comment) |
| `crates/mille-bench` | 151 + 4 benches | criterion: walker, watcher, search, path_index | No |
| `packages/mille` | TS | host/renderer protocol, framed stream, WebSocket transport | — |
| `packages/mille-truffle` | ~1.6k TS | `serveMille` / `connectMille` over Truffle `mesh.net` | — |

**Locked dependency versions** (`Cargo.lock`) [V]: notify 6.1.1, notify-debouncer-full 0.3.2, inotify 0.9.6, fsevent-sys 4.1.0, ignore 0.4.25, nucleo 0.5.0, arc-swap 1.9.1, dashmap 6.2.1, napi 3.8.5, bincode 2.0.1 (pinned `=`), tokio 1.52.1.

**Dead weight in mille-core [V].** `tokio` (with `"full"`), `tokio-util`, `rmp-serde`, `blake3`, `memmap2`, `crossbeam-channel` and `globset` are declared in `crates/mille-core/Cargo.toml` but never referenced in `mille-core/src`. I checked this by grepping for `crate::` and `use`. `icu_collator`, used only for sibling sort in `sort.rs`, and `nucleo` are used, but a scanner does not need them. `VolatileTracker` (`volatile.rs`) and `advise_budget` (`inotify_limits.rs`) are exported but **never used by the binding**: grep finds no references in `mille-binding/src`.

**Recent history matters [V].** The custom bounded walker (`97ec3c6`) and the structurally shared `IdMap` snapshot (`cf284e6`) both landed on **2026-10-07**, *after* the 0.3.4 release (`18ea13a`, 2026-09-07). `initial-discussion.md` reviewed `18ea13a`. Its statements that "the snapshot implementation clones its map structures" and that the walker is jwalk-based are now outdated: clones are O(1) and the walker is Mille's own loop.

---

## 2. What is usable from pure Rust today (mille-core)

### 2.1 Walker API (all re-exported from `lib.rs:47-52`)

| Function | Signature (abridged) | Returns | Notes |
|---|---|---|---|
| `walk` | `fn walk(root: &Path, options: WalkOptions) -> Result<Vec<WalkedEntry>, FxError>` (`walker.rs:125`) | full `Vec` | Uses the process-wide `ReadBoundary::shared_default()` and **drops `WalkReport.unread`** (`:126`). A stalled subtree just vanishes. |
| `walk_with_ignore` | `(root, options, ignore: &IgnoreMatcher)` (`:141-147`) | full `Vec` | Prunes ignored dirs *before* descending, including symlinked `node_modules` (`:653-664`). Nested `.gitignore`/`.ignore`/`.rgignore` files stack dynamically (`:571-582`). Also drops `unread`. |
| `walk_batched` | `<C: FnMut()->bool, F: FnMut(Vec<WalkedEntry>)->Result<(),FxError>>(root, options, batch_size, cancelled, on_batch)` (`:152-173`) | `()` | Streams batches of `batch_size` and calls `thread::yield_now()` per batch (`:251`). Drops `unread`. |
| `walk_with_ignore_batched` | same, plus `ignore` (`:176-198`) | `()` | Drops `unread`. |
| **`walk_bounded`** | `(root, options, ignore: Option<&IgnoreMatcher>, reads: &ReadBoundary) -> Result<WalkReport, FxError>` (`:207-220`) | `WalkReport { entries, unread, ignore_files }` (`:111-123`) | **The one flux should use.** It takes a caller-owned boundary (deadline, stuck cap, dataless policy) and returns what could not be read. |
| **`walk_bounded_batched`** | `(root, options, ignore, reads, batch_size, cancelled, on_batch) -> Result<WalkReport, FxError>` (`:224-263`) | report without entries | Streaming plus `unread`. This is the right primitive for a library scan. |
| `populate_store[_with_provenance]` | `(store, root, walked: &[WalkedEntry], ignore…) -> Result<Vec<EntryId>>` (`:795-889`) | ids | Sorts by (depth, path), then `insert_batch` per depth level. |

`WalkOptions` (`:50-75`): `max_depth: Option<usize>` (default unlimited), `follow_symlinks: SymlinkPolicy` (default `Smart`), `include_hidden: bool` (default **true**), `include_root: bool` (false), `parallelism: usize` (default **0**, which means one directory read at a time).

Output order is **deterministic depth-first with children byte-sorted** (`read_boundary.rs:1044`, `walker.rs:12-15`). The cost of that guarantee is that only one thread can emit.

### 2.2 What each `WalkedEntry` carries (`walker.rs:77-96`)

| Field | Type | Source | flux relevance |
|---|---|---|---|
| `path` | `PathBuf` | `dir.join(name)` (`:602`) | full path; separate heap allocation |
| `parent_path` | `Option<PathBuf>` | `path.parent().to_path_buf()` (`:715`) | **second full path copy** per entry |
| `name` | `String` | `OsString::to_str()?.to_string()` (`:707`) | **a non-UTF-8 name returns `None` and the entry is dropped**, yet `descend` is computed independently (`:603-607`), so its children are still emitted with a `parent_path` that was never emitted [V] |
| `kind` | `EntryKind` (File/Directory/Symlink/Unknown/Unavailable) (`entry.rs:45-56`) | listing `file_type` + metadata | ✓ |
| `size` | `u64` | `metadata.len()` **only for `File`**, 0 otherwise (`:752-756`) | ✓ logical size |
| `mtime_ms` | `i64` | `modified()` in **milliseconds** (`:757-761`) | APFS/ext4 keep ns, so equal-ms rewrites are invisible [I] |
| `ctime_ms` | `i64` | **Unix `st_ctime` (inode status change)** on unix, `created()` only on Windows (`:913-924`) | misleading name. **No birthtime on macOS/Linux.** |
| `is_symlink`, `symlink_target_is_dir` | bool, `Option<bool>` | an extra `stat()` through every symlink (`read_boundary.rs:1016-1027`) | ✓ |
| `is_readonly` | bool | `permissions().readonly()` | — |
| `is_hidden` | bool | **dot-prefix only** (`:708`); macOS `UF_HIDDEN` is ignored | — |
| `depth` | `u16` | — | — |
| **missing** | — | — | **`dev`, `ino` (file id), birthtime, ns mtime, `st_flags` (dataless), volume id.** The data *is* fetched: `ListedChild.metadata` is a full `std::fs::Metadata` (`read_boundary.rs:314-316`), but `entry_from_metadata` throws it away. |

`size_of::<WalkedEntry>() = 104` bytes inline, plus three heap buffers (measured).

### 2.3 Symlink handling [V]

- `SymlinkPolicy::Never`: links are listed, never descended. `symlink_target_is_dir` is still resolved with one `stat`.
- `SymlinkPolicy::Always`: links are followed. Cycles are cut by comparing each followed link's **canonical** target (`fs::canonicalize` per directory link, `read_boundary.rs:1005`) with the ancestor chain (`walker.rs:636-651`). Broken links are dropped entirely (`:594-596`).
- `SymlinkPolicy::Smart` is the default and is "behaviorally equivalent to Never" (`walker.rs:44-47`). The `(dev, inode)` dedup it promises is unimplemented.
- **Remote exports force `followSymlinks: false`** (`mille-truffle/src/exports.ts:48-54`, SEC-003).

**For flux:** media libraries frequently use symlinks or hardlinks (Sonarr/Radarr hardlinks, `/mnt/media` → NAS). flux needs `Always` semantics with **(dev, ino) dedup**, and must not depend on `canonicalize` per link [I].

### 2.4 ReadBoundary: Mille's best idea, with flux-hostile defaults [V]

`read_boundary.rs:1-39` explains the design. Every `readdir` and `lstat` runs on a helper thread pool. The caller waits on a ticket with a **stall deadline**: no progress for `timeout` returns `TimedOut`. Progress ticks per entry, so huge but moving directories are never cut off. Other properties:

- **Stuck cap:** `max_stuck`, default 16.
- **No pile-up:** a request below a stuck path returns `Stuck` at once.
- **Joined reads:** identical in-flight requests share one kernel call.
- **`close()` never waits on the kernel.**
- **macOS dataless policy:** helpers call `setiopolicy_np(IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES, …OFF)` (`:1110-1166`), so iCloud-evicted folders answer `EDEADLK`, which maps to `ReadFailure::Dataless`, instead of downloading.

| Constant | Value | Line |
|---|---|---|
| `DEFAULT_READ_TIMEOUT` | **2,000 ms** | `:59` |
| `DEFAULT_MAX_STUCK_READS` | 16 | `:63` |
| `DEFAULT_CONCURRENCY` | 8 | `:66` |
| helper idle keepalive | 10 s | `:69` |
| spin-before-park | 50 µs | `:72` |
| max helper threads | `concurrency + max_stuck` = **24** | `:526` |

**Why flux cares:**

- A spun-down HDD or a sleeping NAS can block the **first** `readdir` for multiple seconds with zero progress ticks. A 2 s deadline would then return `ETIMEDOUT` for the whole root (`walker.rs:336-341, 393`) [I]. The binding marks such a root `Unavailable` (`explorer.rs:3406` `root_read_failed`). In flux that must never translate into "media missing". I could not find datasheet spin-up figures, so treat the exact seconds as [U]; measure SMART attribute 3 (Spin_Up_Time) on target hardware.
- Plain `walk*` drops `unread`. Only `walk_bounded*` exposes it. A scanner that uses `walk()` cannot tell "folder empty" from "folder didn't answer".
- I specifically tested for silent loss under load. On the 2.05M-entry tree, `unread = 0` at both 2 s and 60 s deadlines, even at load ~136. My early observation of "missing entries" turned out to be the live tree shrinking between runs. **No measured data loss** [V].

### 2.5 Watcher (`watcher.rs`) [V]

| Aspect | Finding | Evidence |
|---|---|---|
| Backend | `notify::RecommendedWatcher` (FSEvents, inotify, ReadDirectoryChangesW), 2 s poll interval used only by the poll fallback | `:270-295` |
| Debounce | `notify-debouncer-full` 0.3.2, **default 75 ms**, tick ≈ timeout/4 | `:60-67, 297-338` |
| File-id cache | **`NoCache`**: deliberately avoids debouncer-full's `FileIdMap`, which does a synchronous WalkDir of the whole root at `watch()` | `:308-317` |
| Consequence of `NoCache` | debouncer-full can only stitch renames by notify *tracker* (inotify cookie), never by file id (`notify-debouncer-full-0.3.2/src/lib.rs:316-375`). FSEvents has no tracker, so on macOS no rename is stitched upstream. | registry source |
| Rename info thrown away | A stitched `Modify(Name(Both))` with `[from, to]` is split into two independent `RawEvent::Any` (`:427-434`). Mille's own `RenamePairer` then re-pairs them heuristically. | `:427-434` |
| Rename pairing | **macOS:** stat the path. If it exists, record `(dev, ino)` and treat it as the "to" half. If gone, it is a "from" half. The oldest complementary half wins (`:871-914`). The from-half never has an inode, so pairing is effectively "first gone + first existing". **Linux/Windows:** pure arrival order (`:921-934`). Unpaired halves degrade after 1 s (`watch_runtime.rs:61`). | — |
| Overflow | `EventKind::Other`, which carries notify's `Flag::Rescan` for FSEvents `MustScanSubDirs` and inotify `Q_OVERFLOW`, becomes `RawEvent::Overflow`, then `Coarse`, then a full subtree re-walk | `:415-420`; notify `fsevent.rs:115`, `inotify.rs:208-209` |
| Channels | `std::sync::mpsc::channel()` (**unbounded**) from notify to the forwarder thread (`:277, :307`). debouncer-full also keeps an unbounded `HashMap<PathBuf, Queue>`. The forwarder polls with `recv_timeout(100 ms)`. | `:343-406` |
| Modes | **Recursive only.** `recursive=false` returns `Unsupported` | `:165-171` |
| Coalescer | per-path state machine (create+delete cancels, delete+create becomes create, …) | `:559-717` |
| **Coalescer complexity** | final pass does `first_seen.iter().find(|p| path_slot[*p] == idx)` for every slot, which is **O(n²)** | `:605-610`; measured in §5.5 |
| Resume | none. FSEvents `sinceWhen` is unused; notify does not expose it (§6.3) | — |
| Volatile-dir throttling | implemented (`volatile.rs`), not wired | grep |

### 2.6 Store and snapshot [V]

- **`EntryStore`** (`store.rs:27-39`) has an `ArcSwap<StoreSnapshot>`, two `DashMap`s for path↔id (`Arc<Path>` keys), an `AtomicU64` id counter, and a writer mutex. Every mutation clones the snapshot, mutates it, fixes ancestor summaries, and publishes.
- **`StoreSnapshot`** (`snapshot.rs:64-89`) holds `IdMap`s for entries, children, child counts, visible counts and total sizes, plus an `IdSet` of loaded directories.
- **`IdMap`** (`id_map.rs:1-30`) is a 32-way bitmap-compressed persistent radix trie. `clone()` is O(1) and a mutation is O(depth). The header records why: before this change, 22k entries took 15 s and a 43k home folder took 44 s and 494 MB, because each publication was O(n).
- `insert_batch` merges each parent's new children into the sorted sibling list linearly (`store.rs:781-806`). It still walks every new entry's ancestor chain (O(depth) each) to update summaries (`:750-778`).
- `EntryId` is session-scoped, monotonic, and capped at 2^53 for JS (`entry.rs:11-34`). **It is not durable identity.**
- `paths_under(dir)` walks the **entire known subtree**, not just depth 1 (`store.rs:456-478`). The watch runtime calls it on every reconcile (`watch_runtime.rs:453, 657`).
- The binding's `maxCachedEntries` (default 500,000, `explorer.rs:150, 177, 275`) is **never read** after parsing. There is no eviction.

### 2.7 Search (`search.rs:70-121`) [V]

One-shot nucleo fuzzy match over **every entry's name**. It allocates a `Utf32String` per entry per query (`:99`), sorts all hits, and truncates to `limit` (default 100). There is no index and no incremental matcher. It is fine for a file tree and irrelevant for flux, which will use SQLite FTS5 or Tantivy for titles [I].

### 2.8 Crash resume (`resume.rs`) [V]

- `write_snapshot` serializes the whole snapshot with bincode 2 and writes it atomically: tmp file, `fsync`, rename, then `fsync` of the parent directory (`:126-196`). The format version is 2 (`:29`).
- `events_since` (`:297-370`) **re-walks every root in full** with `walk()` and diffs `HashMap<PathBuf,(mtime_ms,size)>`. It emits `Appeared/Disappeared/Modified/RootDisappeared`.
- It has no identity, so a rename shows up as disappear plus appear. It also has no FSEvents journal.
- The restart cost therefore equals a full scan.

### 2.9 The `Fs` abstraction [V]

`trait Fs` (`fs.rs:31-45`) has an `InMemoryFs` test implementation. **`RealFs` returns `FxError::Unsupported("RealFs implemented in Phase 2")` for all 13 methods** (`fs.rs:47-86`). The repo's own `CLAUDE.md` confirms "`Fs`/`RealFs` … is aspirational". Real I/O goes through `std::fs` in `read_boundary.rs` and through `tokio::fs`/`std::fs` in the binding.

---

## 3. What lives only in mille-binding, and how hard it is to extract

### 3.1 Coupling map [V]

Per-file counts of `napi`, `tokio`, and napi-type tokens (ThreadsafeFunction/Buffer/…):

| File | LOC | napi | tokio | napi types | What it is | Extraction |
|---|---:|---:|---:|---:|---|---|
| `watch_runtime.rs` | 1,012 | **0** | 1 | 0 | OS events → coalesce → pair → **reconcile against bounded disk walk** → ChangeSet + public events | **Easy**: replace `EventBus` + `*Js` (`:23-24`) with a sink trait + core types |
| `bounded.rs` | 55 | 0 | 0 | 0 | `seed_ignore_files`, `probe_exists`, `probe_is_dir` over ReadBoundary | trivial (move to core) |
| `events.rs` | 564 | 10 | 0 | 9 | TSFN fan-out, 8 channels + native listeners | replace with `trait EventSink` / `tokio::broadcast` |
| `types.rs` | 326 | 17 | 0 | 0 | `#[napi(object)]` mirrors (`EntryJs`, `FileSystemEventJs`, `ChangeNoticeJs`…) | define napi-free twins in core; keep `From` impls in binding |
| `explorer.rs` | 4,491 | 279 | 75 | 20 | `FileExplorer` class: lifecycle, populate/resync/lazy listing, **copy/move/delete recursion**, undo, policy gate | **Hard as a whole**; flux needs only populate/resync/start-watching (~600 LOC) |
| `journal.rs` | 739 | 3 | 2 | 0 | undo journal with real `(dev, ino)` file identity (`:29-42, 617-663`) | not needed by flux; the identity helpers are reusable |
| `stream.rs` | 112 | 11 | 8 | 6 | `FileReadStream`: tokio task, 64 KiB chunks, `mpsc(4)`, **copies every chunk into a napi `Buffer`** (`:78`) | irrelevant: flux streams media through its own HTTP range server |
| `io.rs`, `mutations.rs`, `cancel.rs`, `bulk.rs` | ~500 | some | some | — | async file ops, cancellation tokens | not needed |

### 3.2 Dependency graph (as is, and proposed)

```
today:                                    proposed for flux:
 mille-core (walker, read_boundary,        mille-walk   (walker + read_boundary + ignore; deps: ignore, parking_lot, libc)
   watcher, store, snapshot, search,         ▲
   resume, sort[icu], nucleo, notify,      mille-watch  (watcher + coalesce + pairer + FSEvents resume; deps: notify|fsevent-sys)
   + unused tokio/blake3/memmap2/…)          ▲
        ▲                                  mille-runtime (watch_runtime reconcile + ChangeSet + EventSink trait; no napi)
 mille-binding (napi cdylib)                 ▲            ▲
   explorer.rs ── watch_runtime.rs         mille-core (store/snapshot/projection/search: FileTree only)
        │             │                      ▲
     events.rs ◄──────┘ (EventBus, *Js)    mille-binding (napi: FileExplorer = mille-runtime + marshaling)
     types.rs  (#[napi(object)])                         flux-scanner ──► mille-walk + mille-watch (+ optionally mille-runtime)
```

### 3.3 Extraction effort [I]

- **Step 1, ~0.5 day:** move `bounded.rs` and `watch_runtime.rs` into a new `mille-runtime` crate. Introduce `pub enum FsEvent` and `struct ChangeNotice`, with `From` impls into the `*Js` types left in the binding, plus `trait EventSink { fn event(&self,&FsEvent); fn batch(&self,&[FsEvent]); fn change(&self,&ChangeNotice); fn warning(..); fn error(..) }`.
- **Step 2, ~1 day:** lift the ~600 LOC of `populate_from_roots`, `resync`, `resync_progressive`, `refresh_workspace_roots` and `ensure_watcher` out of `explorer.rs` into a `ScanRuntime` struct. `policy_gate` stays as `Arc<Mutex<()>>`. Note that `populate_from_roots` is an `async fn` that runs a **fully synchronous walk while holding a `parking_lot` mutex on a tokio worker** (`explorer.rs:565-679`). A Rust host should call it from `spawn_blocking`.
- **Step 3:** the binding re-implements `FileExplorer` as `ScanRuntime` plus marshaling. The existing JS test suite (which imports from `dist/`) guards behavior.
- **Risk:** low. `watch_runtime.rs` already has zero napi references, and the M10 boundary already threads `ReadBoundary` through `WatchConfig` (`watch_runtime.rs:27-38`).

---

## 4. Performance characteristics from the code

### 4.1 Parallelism model [V]

- **No rayon, no jwalk** since `97ec3c6`. One **consumer thread** runs `walk_core` (`walker.rs:325-435`). It plans every listing (`plan()`, `:566-634`), builds every `WalkedEntry`, and calls the visitor.
- Directory reads are submitted to the `ReadBoundary` helper pool, with up to `parallelism` reads in flight (`read_ahead`, `:376`) and at most `LISTED_AHEAD_MAX = 1024` listed-but-unemitted directories (`:267`).
- Each directory read costs several things:
  - `Flight` `Arc` allocation and a `HashMap<(Op, PathBuf)>` insert, which clones the path (`read_boundary.rs:632-673`).
  - A scan over all in-flight flights for stuck checks (`:643-656`).
  - A condvar handoff to a helper, then on the waiter side a 50 µs spin and a park/wake (`:782-805, 838-853`).
  - A `ReadSignal` notify.
- With `parallelism = 0` (the default), every directory is a synchronous round trip to another thread. That is why serial Mille is slower than a serial std walk, and why it degrades badly under CPU contention: 77–108 s on the 2.3M tree at load 60–100 (§5.3).
- **Concurrency defaults:** the binding uses `min(available_parallelism, 8)` (`explorer.rs:4487-4491`), which is 8 on this M1 Max. The core `walk()` defaults to 0.

### 4.2 Allocation per entry (current walker) [V]

| # | Allocation | Where |
|---|---|---|
| 1 | `OsString` name from `DirEntry::file_name()` | `read_boundary.rs:988` |
| 2 | `PathBuf` `dir.join(name)` for the `lstat` | `read_boundary.rs:989` |
| 3 | `PathBuf` `dir_path.join(name)` again in `plan()` | `walker.rs:602` |
| 4 | `String` name (UTF-8 copy) | `walker.rs:707` |
| 5 | `PathBuf` path copy into `WalkedEntry` | `walker.rs:714` |
| 6 | `PathBuf` parent path copy | `walker.rs:715` |
| +dirs | `Box<Dir>` + `Arc<Flight>` + `HashMap` key clone + `Ticket` + sorted `Vec<ListedChild>` (each `ListedChild` embeds a full `std::fs::Metadata`) | `walker.rs:438-441`, `read_boundary.rs:661-673, 1044` |

Six heap allocations per file and roughly 10 per directory. Measured user CPU on the 245k-entry p008 tree: **mille ~1.8–2.0 s vs std walk 0.13 s vs getattrlistbulk 0.02 s** (§5.2).

### 4.3 Syscalls per entry [V]

- `readdir` (std `ReadDir`) plus **`lstat(full_path)`** per child (`read_boundary.rs:1015`). This is not std's dirfd-relative `DirEntry::metadata()`, which on Apple and glibc Linux uses the directory handle (Rust std `sys/fs/unix.rs`: `cfg_select!` "Use directory handle where possible … `target_vendor = "apple"`"; [source](https://raw.githubusercontent.com/rust-lang/rust/master/library/std/src/sys/fs/unix.rs)).
- Plus one **`stat()`** through every symlink (`:1022-1026`), and `canonicalize` for followed directory links (`:1005`).
- With an ignore matcher, `read_to_string` for each `.gitignore`/`.ignore`/`.rgignore` (`:1030-1041`).

### 4.4 Memory [V, measured §5.4]

| Structure | Bytes/entry (RSS) | Notes |
|---|---:|---|
| `Vec<WalkedEntry>` from `walk()` | **~420–485** | 104 B inline + path + parent path + name + Vec doubling slack |
| `EntryStore` after `populate_store` | **~685–841** | `Arc<Entry>` (104 B + name) + `IdMap` nodes ×5 + 2 `DashMap` entries with `Arc<Path>` + allocator retention |
| `walk_bounded_batched` (batch 4096) | ~O(batch + open dirs) | 14 MB @ 79.5k, 28 MB @ 246k, 72 MB @ 2.05M entries |

Back-of-envelope for flux [I]: a 100k-file library costs ~45 MB as a `Vec` and ~80 MB as an `EntryStore`. That is fine for one library. It is wasteful to keep resident per library when flux's catalog lives in SQLite anyway.

### 4.5 Unbounded queues [V]

- notify → forwarder: `std::sync::mpsc::channel()` (unbounded, `watcher.rs:277, 307`).
- debouncer-full internal per-path queues (`HashMap<PathBuf, Queue>`, notify-debouncer-full `lib.rs:184-187`).
- `EventBus` TSFNs (napi default queue; binding only).

The watcher callback `process_batch` runs **synchronously on the forwarder thread under `policy_gate`** (`watch_runtime.rs:110`). Slow reconciles therefore back up an unbounded queue rather than shedding to a `Coarse` rescan [I].

### 4.6 Watch-path complexity [V by code, partially measured]

For each event in a debounced batch (`watch_runtime.rs:138-162`), `reconcile_event` runs. For `Created(file)`, `Modified`, `Unknown` and `RenameDegraded` that means `reconcile_nearest_parent` (`:369-386`):

1. `probe_is_dir` (a bounded `stat`).
2. `walk_bounded(parent, max_depth=1)`: a full listing plus `lstat` of every sibling.
3. `paths_under(parent)`: the whole known subtree (`store.rs:456-478`).
4. A per-entry `store.get_by_path` + `update`/`insert`, each publishing a new snapshot (`:472-521`).

There is **no per-batch dedup of reconcile scopes**. Copying 5,000 new files into one flat `Movies/` folder produces about 5,000 × (listing of up to 5,000 entries), i.e. **O(n²) `lstat` calls**, plus the O(n²) coalescer. A season folder of 24 episodes is harmless, but a flat movies folder, a big download or unpack directory, or a music dump is not [I].

`normalize_path` calls `fs::canonicalize(root)` **per event per root** (`watch_runtime.rs:222-236`).

---

## 5. Measurements

Harness: `$FLUX_WORK/bench/mille-walkbench/src/main.rs` (release, thin LTO, `codegen-units=1`). Every mode walks without following symlinks, includes hidden entries, excludes the root, and obtains kind + size + mtime for every entry. All modes agreed **exactly** on entry, file, dir, link and byte counts on every static tree, which is a correctness cross-check of my getattrlistbulk parser.

- **`gab`** is my serial `getattrlistbulk(2)` walker. It requests `RETURNED_ATTRS | NAME | ERROR | OBJTYPE | CRTIME | MODTIME | FLAGS | FILEID` plus `ATTR_FILE_DATALENGTH` into a 128 KiB buffer. That gives **name, kind, size, ns mtime, birthtime, inode and `st_flags` in one syscall batch**, a strict superset of `WalkedEntry`.
- **`gab-par`** is the same walker with an 8-thread work queue.
- `mille` is `walk()` and `mille-bounded` is `walk_bounded()` with an explicit boundary. They are the same code path; `bounded` also reports `unread`.

Datasets:

| Dataset | Where | Entries | Files | Dirs | Symlinks |
|---|---|---:|---:|---:|---:|
| **media fixture** (generated: 2,000 movie folders × {mkv, nfo, srt, poster, fanart}; 300 shows × 6 seasons × 12 eps × {mkv, nfo, srt}; mkv files sparse 1.2–21 GB) | USB APFS SSD `$FLUX_WORK/media/lib1` | 79,502 | 75,400 | 4,102 | 0 |
| **p008** (Truffle checkout incl. node_modules; static) | internal APFS NVMe | 245,785 | 200,423 | 32,893 | 12,469 |
| **project100** (live tree; shrank 2.32M→2.05M during testing) | internal APFS NVMe | ~2.05M | ~1.74M | ~258k | ~56k |

### 5.1 Media fixture: 79.5k entries, load ≈ 15, warm, median of 3

| Walker | Threads | Wall (s) | Entries/s | Peak RSS (MB) | vs mille p8 |
|---|---:|---:|---:|---:|---:|
| mille `walk` | 0 (default) | 0.44 | 181k | 36 | 0.41× |
| mille `walk` | 8 | **0.18** | 444k | 38.5 | 1.00× |
| mille `walk_batched` | 8 | 0.18 | 460k | **14** | 1.00× |
| std `read_dir` + `lstat(path)` | 1 | 0.29 | 279k | 6.5 | 0.62× |
| std `read_dir` + `DirEntry::metadata` (fstatat) | 1 | 0.27 | 305k | 6.4 | 0.67× |
| walkdir 2.5 | 1 | 0.28 | 285k | 6.2 | 0.64× |
| jwalk 0.8.1 | 8 | 0.13 | 620k | 13 | 1.4× |
| ignore 0.4.25 `WalkParallel` | 8 | 0.11 | 720k | 8.3 | 1.6× |
| **getattrlistbulk serial** | 1 | 0.16 | 495k | 6.6 | 1.1× |
| **getattrlistbulk parallel** | 8 | **0.07** | **1.18M** | 7.0 | **2.6×** |
| `find -ls` (BSD) | 1 | 0.39 | — | 2.2 | — |

### 5.2 p008: 245.8k entries, load 18–31, warm, median of 3

| Walker | Threads | Wall (s) | Entries/s | User CPU (s) | Peak RSS (MB) | vs mille p8 |
|---|---:|---:|---:|---:|---:|---:|
| mille `walk` | 0 | 3.30 | 74.6k | ~1.9 | 116 | 0.42× |
| mille `walk` | 8 | **1.37** | 180k | ~1.1 | 119 | 1.00× |
| mille `walk_batched` | 8 | 1.71 | 144k | ~1.2 | **28** | 0.80× |
| std `read_dir` + `lstat` | 1 | 1.78 | 138k | 0.13 | 7.3 | 0.77× |
| std `read_dir` + fstatat | 1 | 1.77 | 139k | 0.10 | 7.1 | 0.77× |
| walkdir | 1 | 1.98 | 124k | 0.13 | 7.2 | 0.69× |
| jwalk | 8 | 0.87 | 283k | 0.36 | 16.6 | 1.6× |
| ignore `WalkParallel` | 8 | 0.76 | 323k | 0.28 | 15 | 1.8× |
| **getattrlistbulk serial** | 1 | 1.11 | 221k | **0.02** | 7.2 | 1.2× |
| **getattrlistbulk parallel** | 8 | **0.53** | **464k** | 0.06 | 8.4 | **2.6×** |
| `find -ls` | 1 | 2.63 | — | — | — | — |

### 5.3 project100: ~2.05M entries, vnode-cold (> `kern.maxvnodes` 263,168), single runs

| Walker | Threads | Load | Wall (s) | Entries/s | User / Sys (s) | Peak RSS (MB) |
|---|---:|---:|---:|---:|---|---:|
| getattrlistbulk parallel | 8 | 23 | **14.5** | 142k | 0.5 / 37 | 10 |
| ignore `WalkParallel` | 8 | 27 | 23.2 | 88k | 2.8 / 59 | 37 |
| jwalk | 8 | 34 | 26.9 | 76k | 3.7 / 59 | 38 |
| mille `walk_batched` | 8 | 24 | 40.4 | 51k | 12.7 / 52 | 72.5 |
| mille `walk_bounded` (Vec) | 8 | 33 | 67.1 | 31k | 13.6 / 54 | **831** |
| getattrlistbulk serial | 1 | 17 | 41.9 | 49k | 0.3 / 19 | 8.6 |
| std `read_dir` + `lstat` | 1 | 12 | 63.1 | 33k | 1.5 / 31 | 8.5 |
| *(earlier, load 60–136)* mille `walk` | 0 | 60–100 | 77–108 | 21–30k | 18 / 37–44 | 910–926 |
| *(earlier, load 48–136)* mille `walk_bounded` | 8 | 48–136 | 48–81 | 25–43k | 13 / 51–52 | 830 |

Takeaways:

- **Parallel getattrlistbulk is 2.8–4.6× faster than Mille at 8 threads**, and uses ~25× less user CPU.
- On a tree larger than the vnode cache, `getattrlistbulk` also cuts *system* time: 19 s serial vs 31 s for `lstat`.
- When a filesystem lacks `VNOP_GETATTRLISTBULK`, XNU's fallback is a per-entry `namei()` ([xnu `vfs_attrlist.c`](https://raw.githubusercontent.com/apple-oss-distributions/xnu/main/bsd/vfs/vfs_attrlist.c)), which implies native filesystems (APFS) can serve bulk attributes more cheaply [I].

### 5.4 Store build and memory (`populate_store`, 8-thread walk)

| Tree | Walk (s) | populate_store (s) | µs/entry | RSS after walk | RSS after populate | **Store Δ/entry** |
|---|---:|---:|---:|---:|---:|---:|
| media 79.5k | 0.12–0.15 | 0.20–0.21 | 2.5–2.6 | 40–41 MB | 106–108 MB | **834–855 B** |
| p008 245.8k | 2.23 | 0.92 | 3.7 | 125 MB | 294 MB | **685 B** |

### 5.5 `coalesce_events` scaling (unique paths, one batch; load 75–100)

| Raw events | Time | µs/event |
|---:|---:|---:|
| 1,000 | 22 ms | 22 |
| 5,000 | 1.20 s | 241 |
| 10,000 | **7.42 s** | 742 |
| 20,000 | **31.8 s** | 1,588 |
| 40,000 | **86.1 s** | 2,152 |

This is clearly quadratic (`watcher.rs:605-610`). Load inflates the constants, not the exponent. It runs on the forwarder thread **while holding `policy_gate`**, so every explorer mutation and listing stalls behind it.

### 5.6 0.3.4 (jwalk-based) vs main (ReadBoundary walker), interleaved, load 10–16

| Tree | Mode | 0.3.4 median (s) | main median (s) | 0.3.4 RSS | main RSS | User CPU 0.3.4 → main |
|---|---|---:|---:|---:|---:|---|
| p008 | walk p0 | 2.62 | 3.34 | 142 MB | 115 MB | 0.23 → 1.9 s |
| p008 | walk p8 | 2.75 | 2.34 | 200 MB | 120 MB | 0.39 → 1.4 s |
| p008 | batched p8 | **1.37** | 2.19 | 66 MB | **27 MB** | 0.38 → 1.3 s |
| media | walk p0 | 0.37 | 0.50 | 38 MB | 36 MB | 0.05 → 0.27 s |
| media | walk p8 | 0.26 | 0.19 | 60 MB | 38 MB | 0.08 → 0.19 s |
| media | batched p8 | 0.24 | 0.22 | 33 MB | 14 MB | 0.08 → 0.19 s |

The M10 rewrite bought hang-safety and lower memory. It left wall-clock time roughly unchanged (±30%, within noise here) at **3–8× the user CPU**. Neither version approaches `getattrlistbulk`.

### 5.7 mille-bench (criterion) itself

`crates/mille-bench` measures:

- `walker.rs`: serial and 4-thread walks of a ~1,500-entry tree; `populate_store`; visible-row and viewport queries; path lookup; a pnpm-style 10k-repo/100k-store ignore walk.
- `watcher.rs`: `coalesce_events` on 10 and 1,000 events; `RenamePairer`; `IntentCache`; `VolatileTracker`.
- `search.rs` and `path_index.rs`: 1.5k-entry fixtures.

The fixtures (`mille-bench/src/lib.rs:17-63`) are 220–8,590 entries of tiny files in `TempDir`. They are **too small to expose the issues above** (the coalescer bench stops at 1,000 events, where it costs 22 ms). Criterion numbers from a run with `TMPDIR` and the target dir on the SSD are in Appendix B.

---

## 6. OS fast paths: what Mille uses and what flux should

### 6.1 macOS `getattrlistbulk(2)`

- **Mille: not used [V]** (no reference in the repo; `list_dir` is `read_dir` + `lstat`, `read_boundary.rs:979-1046`).
- **Requirements [V]** (local `man 2 getattrlistbulk`): it requires `ATTR_CMN_NAME` and `ATTR_CMN_RETURNED_ATTRS`. `ATTR_CMN_ERROR` is placed right after `RETURNED_ATTRS`. Groups are 8-byte aligned and attributes 4-byte aligned. Entry order is unspecified, and mixing it with `readdir` on the same fd is undefined.
- **XNU fallback [V]:** `readdirattr()` does `namei()` per entry when the filesystem lacks the native VNOP. It also falls back when `ATTR_CMN_OBJTYPE` is not requested ([xnu source](https://raw.githubusercontent.com/apple-oss-distributions/xnu/main/bsd/vfs/vfs_attrlist.c)).
- **Third-party evidence [U]:**
  - healeycodes' `dumac` (Rust + tokio + getattrlistbulk) measured **521 ms vs 3.33 s for `du -sh`** on 409,500 files on an M1 Pro ([post](https://healeycodes.com/maybe-the-fastest-disk-usage-program-on-macos)).
  - The `getattrlistbulk` crate v0.1.0 claims ~4× on NVMe and "up to 50×" on network storage ([docs.rs](https://docs.rs/getattrlistbulk)).
  - Thomas Tempelmann's 2019 tests were more mixed ([mjtsai summary](https://mjtsai.com/blog/2019/04/22/performance-considerations-when-reading-directories-on-macos)).
  - My measurements: **1.6–2.7× serial and 2.6–4.6× parallel vs Mille**.
- **Caveats for flux [V]:**
  - An Apple DTS thread documents `getattrlistbulk` **looping forever on SMB under macOS 15** when recursion keeps multiple directory iterations open (FB15497909, unresolved). DTS recommends a **flat worklist with one open directory per thread** and buffers of at least 32 KB ([Apple forums](https://developer.apple.com/forums/thread/766035)).
  - Mille's `list_dir` already reads a whole directory and then closes it, which is the right shape. My `gab-par` prototype does the same.
  - flux should cap per-share concurrency and detect "listing exceeds the directory's link count" as a loop guard [I].

### 6.2 Linux `statx` / `io_uring`

- **Mille: neither [V].** On glibc Linux, `std::fs::symlink_metadata` already calls `statx` with `STATX_BTIME` internally, so birthtime *is* available through `Metadata::created()` (Rust std `try_statx`). Mille just never reads it, and uses full-path lookups rather than dirfd-relative `fstatat`/`statx(dirfd, name)` (std `DirEntry::metadata` does the latter).
- **`io_uring`:** the current uapi enum has `IORING_OP_STATX` (21) but **no getdents opcode**; the 2021 `IORING_OP_GETDENTS` series was never merged ([io_uring.h](https://raw.githubusercontent.com/torvalds/linux/master/include/uapi/linux/io_uring.h), [LKML v3](https://lkml.iu.edu/hypermail/linux/kernel/2102.2/02298.html)) [V].
- An io_uring scanner would batch `statx` per directory after a synchronous `getdents64`. That is a plausible but unproven win, probably smaller than parallelism plus dirfd-relative `statx` [I].

### 6.3 FSEvents event-ID resume (`sinceWhen`)

- **Mille: no [V].** notify 6.1.1 sets `since_when: kFSEventStreamEventIdSinceNow` (`notify-6.1.1/src/fsevent.rs:274`). notify `main` (9.0.0-rc.6, 2026-10-17) does the same, and its callback **ignores `_event_ids`**. There is no `FSEventStreamCreateRelativeToDevice` and no config knob, though rc.5 added `with_fsevent_latency` ([notify fsevent.rs](https://raw.githubusercontent.com/notify-rs/notify/main/notify/src/fsevent.rs), [releases](https://github.com/notify-rs/notify/releases)).
- **Apple's contract [V]** ([FSEvents guide](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/FSEvents_ProgGuide/UsingtheFSEventsFramework/UsingtheFSEventsFramework.html)):
  - Store the last event ID **plus the device UUID** (`FSEventsCopyUUIDForDevice`).
  - Resume with `sinceWhen` via `FSEventStreamCreateRelativeToDevice`.
  - Treat a UUID mismatch, or a stored ID above `FSEventsGetCurrentEventId()`, as "full rescan".
  - Handle `MustScanSubDirs` (which covers user and kernel drops), `RootChanged` and `EventIdsWrapped`.
  - "Start monitoring the directory before you start scanning it."
- **Prior art [V]:** `@parcel/watcher` stores `FSEventsGetCurrentEventId()` in its snapshot file and replays with `sinceWhen` until `HistoryDone` ([FSEventsBackend.cc](https://github.com/parcel-bundler/watcher/blob/master/src/macos/FSEventsBackend.cc)).
- **Implication [I]:** flux restart-without-rescan on macOS needs either (a) a small custom FSEvents backend via `fsevent-sys` 4.1 (already in the lock graph) or `objc2-core-services`, or (b) an upstream notify PR exposing `since_when` and event IDs. Fallback is a directory-mtime-pruned rescan.
- **Linux equivalent [I]:** none persistent. `fanotify` directory events (FAN_CREATE/DELETE/MOVE since Linux 5.1, [fanotify(7)](https://man7.org/linux/man-pages/man7/fanotify.7.html)) with a filesystem-wide mark avoid inotify's per-directory watch limit. Since the 2020 change, inotify's `max_user_watches` defaults to ~1% of RAM, clamped to [8192, 1,048,576] ([commit 92890123](https://code.amethyst.name/public-mirrors/linux/commit/92890123749bafc317bbfacbe0a62ce08d78efb7)).

### 6.4 Windows [I, lower priority]

Rust std's `read_dir` on Windows already returns size and times from the find data, but no file ID. `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)` returns size, 4 timestamps, attributes and a 128-bit `FileId` in bulk ([docs](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_id_extd_dir_info)). The binding already queries file IDs through handles for undo (`journal.rs:658-715`).

---

## 7. mille-truffle and the "browse a remote device's folders to add a library" flow

### 7.1 What exists [V]

**Transport.** Node/TS only. `serveMille(mesh, options)` listens on Truffle `mesh.net` (TCP over tsnet), **default port 9451** (`mille-truffle/src/server.ts:67-74`). It authorizes on the socket's **verified Tailscale node id** before any host or filesystem work (`server.ts:3-14`, `types.ts:53-60`). The base package has no Truffle import, so the framed channel works over any Duplex, including WebSocket (`packages/mille/src/websocket/host.ts`).

**Framed stream** (`packages/mille/src/stream/limits.ts`):

| Limit | Default | Line |
|---|---|---|
| magic / version | `"MLLE"`, wire major 1, minor 0 | `:9-15` |
| header | 20 bytes (magic, major, minor, flags u16, 3× u32 lengths) | `:17-18` |
| `maxMetadataBytes` (UTF-8 JSON per frame) | **4 MiB** | `:44` |
| `maxAttachments` (binary views per frame) | 32 | `:45` |
| `maxFrameBytes` | **32 MiB** | `:46` |
| outbound soft / hard watermark | **8 MiB / 32 MiB** (hard closes channel) | `:47-48` |
| per-request file payload (`maxFileBytes`) | **16 MiB** ("control stream is not a bulk transfer channel") | `mille-truffle/src/exports.ts:20-21` |
| sessions per export / per peer | 16 / 4 | `exports.ts:22`, `server.ts:69` |
| heartbeat / idle / open timeouts | 20 s / 60 s / 10 s | `server.ts:67-74` |

**Protocol.** `{v, type, body}` with `PROTOCOL_VERSION = 1` and an exact major match (`protocol.ts:24`):

- Client → host: `handshake`, `setExpanded`, `setViewport`, `mutate` (create/rename/move/delete/copy/writeFile/readFile/readText), `call`, `ack`, `decorations`, `dispose`.
- Host → client: `snapshot`, `delta` (bincode `ClientEntry` buffers + packed child-id lists), `event`, `directoryLoad` (`loading|complete|error|cancelled`), `mutateResult`, `callResult`, `warning`, `error` (`protocol.ts:62-352`).
- Only rows in the viewport ship full records; child **id lists** ship whole.

**Remote open handshake.** `mille.remote` v1 `open {exportId, requestedAccess, credential?}` → `accepted {sessionId, export, limits}` | `rejected` (`packages/mille/src/remote/protocol.ts`). **Clients name an export id and never send a path.** Exports are roots canonicalized once at startup, with symlink following forced off (`exports.ts:36-125`). There is no QUIC content stream yet; it is deferred (`draft/mille-truffle-spec.md:98, 1419`).

### 7.2 What flux's flow needs vs. what Mille gives

| Need (flux "Add library → pick a folder on device X") | Mille today | Gap |
|---|---|---|
| Discover devices and auth by tailnet identity | Truffle + node-id authorization (`authorize.ts`) | ✓ reuse the *pattern* |
| Start browsing at volume roots (`/Volumes/*`, `/mnt`, `/media`, `$HOME`, Windows drive letters) | Exports are fixed server-configured roots | flux must auto-declare a read-only, admin-only "browse" export of mount roots, or add a `listVolumes` RPC |
| Lazy depth-1 listing with kind, size, mtime and child counts; stall-safe on NAS or iCloud | `resync_progressive` / `directoryLoad` (TS host) on top of `walk_bounded_batched(max_depth=1)` + ReadBoundary | ✓ engine-level. ✗ no Rust host: the host is `packages/mille/src/host.ts` driving the NAPI `FileExplorer` |
| Show "looks like media" hints (counts of video files, detected `Movie (Year)` / `Season NN` patterns) | none | flux-specific `call` method or a separate RPC |
| Bounded payloads | 4/32 MiB frames, viewport virtualization | browse payloads are tiny (one directory) |
| Rust-only daemon on headless NAS or Linux boxes | **Requires Node + the `.node` addon** | **Blocking for flux's Rust-native agent** |

**Recommendation [I].** Do not embed the Mille TS host in flux's Rust daemon. Instead:

- Expose `GET /v1/fs/volumes` and `GET /v1/fs/list?path=…&cursor=…` (depth 1) through Truffle's HTTP proxy. Implement them with `ReadBoundary` + `walk_bounded_batched(max_depth: Some(1))` (or the getattrlistbulk walker), with a 2–10 s per-request stall deadline surfaced to the UI as "this drive is waking up / not responding".
- Reuse Mille's *policy model*: read-only, admin-gated, no symlink escapes, and authorization on the node id rather than a client-claimed id.
- Optionally render with `@vibecook/mille-ui` `FileTree` in the flux web UI via a thin adapter, if that saves UI work.
- Keep mille-truffle for Electron or desktop dogfooding.

---

## 8. Prioritized Mille improvements flux needs

Effort: S ≤ 1 day, M 2–4 days, L ≥ 1 week [all estimates I].

| # | Pri | Improvement | Effort |
|---|---|---|---|
| 1 | **P0** | Extract a NAPI-free `mille-runtime` (watch_runtime + bounded probes + scan orchestration) with an `EventSink` trait | M |
| 2 | **P0** | Media-grade scan record + streaming visitor (`ScanEntry` with dev, ino, birthtime, ns mtime, flags; no path triple) + extension/predicate pushdown | M |
| 3 | **P0** | Fix O(n²) `coalesce_events`; dedupe reconcile scopes per batch (one listing per directory per batch); depth-limited `paths_under` | S–M |
| 4 | **P0** | Make `unread` impossible to ignore + configurable deadlines (separate first-byte vs stall deadline; HDD/NAS profile) | S |
| 5 | **P1** | macOS `getattrlistbulk` fast path in `list_dir` (+ dirfd-relative `fstatat`/`statx` fallback; Linux `statx` btime) | M |
| 6 | **P1** | Worker-owned traversal (workers expand subdirectories themselves) + unordered mode; remove per-directory handoff | M |
| 7 | **P1** | FSEvents resume: persist `(volume UUID, last event id)`; replay with `sinceWhen`; rescan on mismatch | M–L |
| 8 | **P1** | Stable file identity + identity-based rename pairing (keep debouncer's stitched pairs; FSEvents extended file-id / inotify cookie) | M |
| 9 | **P1** | Bounded watcher pipeline with overflow → `Coarse` (backpressure instead of unbounded queues) | S |
| 10 | P2 | Trim mille-core dependencies; feature-gate store/snapshot/search/ICU so a scanner links only walk + watch | S |
| 11 | P2 | Non-UTF-8 names: carry `OsString`, never drop entries | S |
| 12 | P2 | Symlink `Smart` = follow with (dev, ino) dedup; skip `canonicalize` per link | S |
| 13 | P2 | Polling watcher mode per root (SMB/NFS shares, where native events are unreliable) + directory-mtime pruned rescan | M |
| 14 | P2 | Rust implementation of a minimal remote "browse" service (or of the framed codec) for Rust-only agents | M |

### Details

**1. Extract `mille-runtime` (P0, M).**

- *Problem:* the reconcile pipeline flux would reuse is trapped in the cdylib. `watch_runtime.rs` has 0 napi references but imports `crate::events::{EventBus, Channel}` and five `#[napi(object)]` types (`watch_runtime.rs:23-24`). Scan orchestration lives inside the 4,491-line `explorer.rs` (`populate_from_roots` `:565-679`, `resync*` `:916-1060`).
- *Change:* new crate with `ScanRuntime { store, reads, watcher, roots, policy_gate }`, `enum FsEvent`, `struct ChangeNotice`, `trait EventSink`. The binding keeps `From<FsEvent> for FileSystemEventJs` and a TSFN-backed sink.
- *Impact:* flux can embed Mille's live watching and reconciliation in a tokio service without Node. Mille also gets a cleaner test surface (Rust tests for the runtime, today only exercised via JS).
- *Evidence of feasibility:* `bounded.rs` and `watch_runtime.rs` are already napi-free (§3.1).

**2. Media-grade scan record and streaming visitor (P0, M).**

- *Problem:*
  - No identity or birthtime.
  - `ctime_ms` is status-change time (`walker.rs:913-924`).
  - ms precision.
  - Six allocations per entry, including two full-path copies (`walker.rs:602, 707, 714-715`).
  - Measured ~420–485 B/entry for `Vec<WalkedEntry>`.
  - The full `Metadata` is fetched and discarded (`read_boundary.rs:314-316`).
- *Change:*
  - `walk_scan(root, opts, reads, |dir: &DirCtx, e: &ScanEntry| -> Visit)`.
  - `ScanEntry { name: &OsStr, kind, size, mtime_ns, btime_ns: Option, dev, ino, flags, nlink }`.
  - `DirCtx` carries the interned parent path and its id, so entries reference the parent instead of copying it.
  - `Visit::{Continue, SkipChildren}` plus `WalkOptions::file_filter: Option<Arc<dyn Fn(&OsStr) -> bool>>`, so non-media files can skip the per-file `lstat` on the readdir path (d_type suffices).
  - Keep `WalkedEntry` as an adapter.
- *Impact:* flux diffs scans against SQLite on `(dev, ino, size, mtime_ns)` and detects moves without re-probing. My harness's `GabEntry` shows the record costs nothing extra on macOS. Memory per in-flight entry drops from ~450 B to well under 100 B for streaming consumers [I].

**3. Fix the two quadratic watch paths (P0, S–M).**

- *Problem:*
  - `coalesce_events` final pass is O(n²): 10k events take 7.4 s and 40k take 86 s, measured (`watcher.rs:605-610`).
  - `process_batch` reconciles each event independently: one depth-1 parent listing per event, plus `paths_under` enumerating the whole known subtree (`watch_runtime.rs:138-162, 369-386, 453`; `store.rs:456-478`).
  - All of this happens under `policy_gate` on the forwarder thread (`watch_runtime.rs:110`).
- *Change:*
  - Store the final state in `path_slot` order (a `Vec<(PathBuf, PathState)>` + `HashMap<PathBuf, usize>`). That makes coalescing O(n).
  - In `process_batch`, group events by nearest known directory and reconcile each directory once with `depth=1`, plus one subtree reconcile per created or moved directory.
  - Make `paths_under` accept a depth limit.
  - Use `insert_batch` in `reconcile_directory` instead of per-entry `insert` (`:506, 515`).
- *Impact:* copying a 5,000-file folder into a flat library becomes ~1 listing instead of 5,000. The coalescer drops from seconds to milliseconds. This is critical for "drop a season pack" or "Sonarr import storm" workloads [I].

**4. Explicit `unread` and configurable deadlines (P0, S).**

- *Problem:* `walk`, `walk_with_ignore` and the batched variants silently drop `WalkReport.unread` (`walker.rs:125-198`). The default 2 s stall deadline (`read_boundary.rs:59`) also applies to the *first* response, which is exactly when a sleeping HDD or NAS spins up. A root that does not answer fails the whole walk (`walker.rs:393`). The binding then marks the root `Unavailable` and evicts its descendants (`entry.rs:50-55`).
- *Change:*
  - Deprecate the report-dropping wrappers, or make them return `Err(Incomplete{unread})` when `unread` is non-empty.
  - Split `ReadBoundaryOptions.timeout` into `first_response` (default 30 s for "media" profile) and `stall` (2 s).
  - Expose `ReadProgress` to callers for UI ("waking drive…").
- *Impact:* prevents a flux library scan from interpreting "drive asleep" as "all media deleted". This is the single most damaging possible scanner bug [I].

**5. getattrlistbulk fast path, plus Linux statx (P1, M).**

- *Problem:* each entry costs a full-path `lstat`, plus a `stat` per symlink (`read_boundary.rs:1015, 1022`). Measured gap to parallel getattrlistbulk: **2.6× (79.5k media, 246k tree) and 4.6× (2.05M tree)**. getattrlistbulk uses ~18–95× less user CPU and less system time on vnode-cold trees.
- *Change:*
  - On macOS, implement `list_dir` with `open(O_DIRECTORY)` + `getattrlistbulk` (128 KiB buffer, ≥ 32 KiB per Apple DTS) inside the existing `ReadBoundary` helper. Tick `ReadProgress` per returned batch. Keep the SF_DATALESS logic by reading `ATTR_CMN_FLAGS`. Fall back to `read_dir` + `DirEntry::metadata()` (dirfd-relative `fstatat`) on `ENOTSUP`/`EINVAL`.
  - On Linux, use `rustix` `RawDir` (getdents64) + `statx(dirfd, name, AT_SYMLINK_NOFOLLOW, STATX_BASIC_STATS|STATX_BTIME)`.
  - Guard SMB with one open directory per worker plus a loop detector.
- *Impact:* 2.6–4.6× faster full scans on macOS libraries, plus free inode and birthtime, which serves item 2.
- *Evidence:* §5.1–5.3. The harness's `gab_list` (`$FLUX_WORK/bench/mille-walkbench/src/main.rs`) is a working starting point.

**6. Worker-owned traversal / unordered mode (P1, M).**

- *Problem:* a single consumer thread plans and emits everything, and every directory is a submit → park → wake round trip (`walker.rs:396-433`, `read_boundary.rs:626-687, 769-806`).
  - Serial Mille runs at **0.42× the speed of 8-thread Mille and 0.54× a naive serial std walk** (p008: 3.30 s vs 1.78 s).
  - Under CPU contention it collapsed to 77–108 s on the 2.3M tree.
  - Its user CPU is 8–15× jwalk's.
- *Change:*
  - Let helper workers list a directory, plan its children and *push subdirectories onto a shared work queue themselves*, as `gab-par` does, with stall accounting per worker.
  - Offer `ordered: bool`. Media scans don't need byte-sorted DFS output, and the store re-sorts anyway (`walker.rs:822-823`).
- *Impact:* closes the 1.6–1.8× gap to `ignore`/jwalk even before item 5, and removes contention collapse.

**7. FSEvents resume (P1, M–L).**

- *Problem:* restart means a full rescan (`resume.rs:297-370`). Neither notify 6.1.1 nor notify 9.0.0-rc.6 can set `sinceWhen` or report event IDs (§6.3).
- *Change:*
  - Add a `mille-watch` FSEvents backend (`fsevent-sys` 4.1 is already in the lock graph) using `FSEventStreamCreateRelativeToDevice` with `kFSEventStreamCreateFlagFileEvents | WatchRoot` (and `UseExtendedData` for inode, [U] verify).
  - Persist `(FSEventsCopyUUIDForDevice, lastEventId)` per volume.
  - On start, replay until `HistoryDone`, coalesce, and reconcile.
  - On UUID mismatch, `id > FSEventsGetCurrentEventId()`, `EventIdsWrapped` or `MustScanSubDirs`, fall back to a scoped rescan.
  - On Linux, persist nothing: do a startup rescan pruned by directory mtime (item 13).
- *Impact:* a macOS flux server restarts in O(changes) instead of O(library). Plex and Jellyfin both rescan, so this is a measurable differentiator [I].

**8. Stable file identity and identity-based renames (P1, M).**

- *Problem:*
  - `EntryId` is session-scoped (`entry.rs:14-34`).
  - macOS pairing matches the first gone half with the first existing half, because the from-half never has an inode (`watcher.rs:871-914`).
  - Linux and Windows pair by arrival order (`:921-934`).
  - The debouncer's cookie-stitched `Name(Both)` pairs are split apart (`:427-434`).
  - A mis-pair would move an id, and in flux's catalog that means moving watch state and metadata, onto the wrong file.
- *Change:*
  - Expose `(dev, ino)`, plus birthtime as a tiebreaker on filesystems with inode reuse, on every scan entry.
  - Keep `Name(Both)` as a paired rename.
  - Pair by inode where available (FSEvents extended data [U], inotify cookie via notify `tracker()`, Windows `FileId`).
  - When in doubt, degrade to delete + create and let flux's catalog re-match on `(dev, ino, size)`.
- *Impact:* renames and moves in Sonarr/Radarr-managed libraries keep play state with no re-probe.

**9. Bounded watcher pipeline (P1, S).**

- *Problem:* unbounded `mpsc` (`watcher.rs:277, 307`), an unbounded debouncer queue, and a synchronous slow consumer under `policy_gate`.
- *Change:* `sync_channel(N)`. When full, set a "dirty roots" bit and emit one `Coarse{root}` instead of queueing more events. Also measure queue depth.
- *Impact:* bounded RAM during `rsync` or unpack storms, plus a deterministic recovery path.

**10. Dependency diet (P2, S).** Remove unused `tokio`/`tokio-util`/`rmp-serde`/`blake3`/`memmap2`/`crossbeam-channel`/`globset` from mille-core (§1). Put `store`, `snapshot`, `search` and `sort` (ICU) behind a default `tree` feature, so a scanner links only `ignore`, `notify` and `parking_lot`. *Impact:* faster flux builds and a smaller binary.

**11. Non-UTF-8 names (P2, S).** Today the entry is dropped but its subtree is still walked (`walker.rs:602-607, 707`), which yields orphans. *Change:* carry an `OsString` name and let consumers decide. *Impact:* correctness on Linux shares with legacy encodings.

**12. Symlink `Smart` (P2, S).** Implement the promised (dev, ino) dedup (`walker.rs:44-47`) and drop the per-link `canonicalize` (`read_boundary.rs:1005`). *Impact:* hardlink- and symlink-heavy *arr layouts are walked once, quickly.

**13. Polling and pruned rescans for network shares (P2, M).** FSEvents and inotify do not reliably report changes made by *other* hosts on SMB or NFS. The notify docs warn that network filesystems may not emit events [U]. `Watcher` always uses `RecommendedWatcher` (`watcher.rs:270-338`). *Change:* add a per-root `WatchMode::{Native, Poll{interval}, Off}` and a "pruned rescan" that skips subtrees whose directory mtime (and child count) is unchanged since the last scan. Directory mtime changes on entry add, remove or rename but not on file content changes; that suits media libraries [I]. *Impact:* NAS libraries stay fresh without full rescans.

**14. Rust remote browse (P2, M).** The remote protocol exists only in TS (§7). *Change:* either a Rust implementation of the `MLLE` framed codec + `mille.remote` handshake, or (preferred for flux) a minimal JSON or bincode "list directory" service. *Impact:* the flux agent on a headless Linux NAS can offer folder browsing without Node.

### What flux should *not* take from Mille [I]

`EntryStore`, `StoreSnapshot`, projection (compact folders, file nesting, visible rows), ICU sibling sort, nucleo search, and the bincode resume snapshot are all built for a UI file tree. flux's catalog is SQLite, with durable `SourceId`/`AssetId` and `(dev, ino, size, mtime)` diffing. Keeping a resident EntryStore per library costs ~700–850 B/entry for no catalog benefit (§5.4).

---

## 9. Recommendations for flux (how to use Mille, starting now)

1. **Phase 0, now:** depend on `mille-core` (git rev pin) and use **`walk_bounded_batched`** with a flux-owned `ReadBoundary` (`timeout` ≥ 10 s for HDD/NAS roots; `materialize_dataless: false`). Always inspect `WalkReport.unread` and treat those directories as *unknown*, never *empty*.
   - Use `IgnoreMatcher::add_from_string` to support `.plexignore`-style and `.ignore` files. Mille's built-in names are `.gitignore`/`.ignore`/`.rgignore` (`ignore.rs:17`), so flux should seed `.plexignore` and `.flux-ignore` itself.
   - Stream batches into SQLite and a bounded ffprobe queue.
2. **Phase 0:** do **not** use `walk()`/`walk_batched()` (they drop `unread`), `EntryStore`, or `events_since` for the catalog.
3. **Phase 1:** for live updates, use mille-core's `Watcher::new_batch_with_options` + `coalesce_events` only after fix #3 lands, or wrap notify directly. Implement reconciliation in flux: per batch, group by directory, re-list each once, and diff against SQLite by `(dev, ino, size, mtime_ns)`.
4. **Upstream into Mille, in this order:** #4 → #3 → #1 → #2 → #5/#6 → #7/#8. Items #4 and #3 are small, isolated and fix real hazards for Mille's IDE users too (an npm install storm triggers the same O(n²) paths).
5. **Remote folder picker:** build the Truffle-proxied `/v1/fs/{volumes,list}` endpoints in Rust (§7.2) on top of `walk_bounded(max_depth=1)`, the getattrlistbulk lister, and Mille's node-id authorization pattern. Keep mille-truffle for an Electron admin app if one materializes.

---

## 10. Prototype suggestions

1. **`flux-scan-bench` (extend this harness):** add Linux (`statx` via `rustix`), an SMB share and a USB HDD to the matrix. Also add a cold-cache protocol: drop caches on Linux with `echo 3 > /proc/sys/vm/drop_caches`; on macOS, use a fresh mount or reboot. *Metric:* entries/s and p95 directory-listing latency per filesystem. Ship `gab-par` only if it is ≥ 2× faster than `ignore` on APFS **and** never loops on SMB.
2. **`ScanEntry` streaming walker on ReadBoundary (fix #2 + #5 + #6):** fork `walker.rs` into a worker-owned traversal that emits `ScanEntry` via callback. *Metric:* ≥ 2.5× mille p8 on the 79.5k media fixture (≤ 70 ms warm), ≤ 16 MB peak RSS for 2M entries, identical counts to `std-lstat`.
3. **Watch storm test:** a Rust test that creates 20,000 files in one directory under a watched root and measures time to quiescence and peak RSS. Run it before and after fix #3. *Metric:* < 2 s to consistent state, O(n) syscalls (count `lstat` via a `ReadHook`, `read_boundary.rs:82-84`).
4. **FSEvents resume spike:** about 300 LOC using `fsevent-sys`. Persist `(uuid, id)`; kill the process; add, rename and delete 1,000 files; restart. *Metric:* every change is recovered with no full walk, within 1 s for 1,000 changes, and a forced UUID mismatch falls back to a rescan.
5. **Remote browse endpoint over Truffle:** `GET /fs/list` serving depth-1 listings from a NAS with a slept disk. *Metric:* first byte within the first-response deadline, a "waking" state surfaced, and p50 < 50 ms per warm listing over the tailnet.

---

## 11. Open questions

- Does APFS's native `VNOP_GETATTRLISTBULK` avoid per-child vnode instantiation? Measured lower sys time suggests it, but this is unverified in source. And how does `getattrlistbulk` behave on macOS SMB and NFS mounts of typical NAS units (Synology, Unraid, TrueNAS), including the macOS 15 SMB looping bug (FB15497909)?
- What first-response latency do spun-down HDDs and NAS devices in flux's target audience actually show? This sets the default deadline.
- Does `kFSEventStreamCreateFlagUseExtendedData` deliver inode or file-id per event on current macOS (26.x)? It would enable item 8 on macOS without stat races.
- Will notify 9.0 (rc.6 as of 2026-10-17) accept an upstream `since_when`/event-id API, or should flux own a minimal FSEvents backend?
- How should flux treat volume identity for removable drives (macOS device IDs change across reboots; Apple says to compare volume UUIDs)?
- Does the maintainer want Mille's EntryStore kept as the UI tree for flux's admin folder picker, or should flux's picker be stateless depth-1 calls?

---

## Appendix A: Reproduction

```bash
# harness (outside the read-only checkout)
cd $FLUX_WORK/bench/mille-walkbench
CARGO_TARGET_DIR=$FLUX_WORK/target cargo build --release --offline
B=$FLUX_WORK/target/release/mille-walkbench
$B gen-media $FLUX_WORK/media/lib1 2000 300 6 12   # 79,500 entries, sparse mkv
$B mille-bounded <root> 8        # walk_bounded + unread report (MILLE_TIMEOUT_MS overrides 2 s)
$B mille-batched <root> 8 ; $B mille-store <root> 8
$B std-lstat <root> ; $B std-fstatat <root> ; $B walkdir <root>
$B jwalk <root> 8 ; $B ignore-par <root> 8 ; $B gab <root> ; $B gab-par <root> 8
$B coalesce 1000 5000 10000 20000 40000
# old walker: $FLUX_WORK/bench/mille-walkbench-034 (git archive 18ea13a)
```

Raw outputs are in the session scratchpad: `media-clean.txt`, `p008-clean.txt`, `p100-lowload.txt`, `old-vs-new.txt`. The criterion run used `cargo bench --locked --offline -p mille-bench --bench walker` with `TMPDIR` and `CARGO_TARGET_DIR` on the SSD.

## Appendix B: mille-bench criterion (walker suite)

Command: `cargo bench --locked --offline -p mille-bench --bench walker -- --warm-up-time 1 --measurement-time 3 --noplot`, with fixtures in `TMPDIR` on the USB SSD. The load average rose from ~12 to ~290 during the run, so treat these as upper bounds. Names follow `criterion_group!` order (`benches/walker.rs:270-285`). Targets come from the file header (`walker.rs:3-11`).

| Bench | Fixture | Median | SPEC target | Met? |
|---|---|---:|---|---|
| `walker_serial_medium` | ~1.5k entries | 23.8 ms | < 20 ms | ✗ (load) |
| `walker_parallel4_medium` | ~1.5k | 13.2 ms | — | — |
| `populate_store_medium` | ~1.5k | 2.90 ms | < 30 ms | ✓ |
| `visible_row_count_all_expanded_medium` | ~1.5k | 139.6 µs | < 50 µs | ✗ |
| `visible_rows_100_slice_medium` | ~1.5k | 15.2 µs | < 200 µs | ✓ |
| `visible_rows_unlimited_medium` | ~1.5k | 279 µs | — | — |
| `visible_row_ids_full_medium` | ~1.5k | 289 µs | faster than full rows | ≈ |
| `visible_row_index_last_medium` | ~1.5k | 141.7 µs | < 200 µs | ✓ |
| `visible_row_index_last_large_8590` | 8,590 | 1.19 ms | — | — |
| `indexed_path_lookup_last_medium` | ~1.5k | 458 ns | < 10 µs | ✓ |
| `visible_prefix_full_miss_medium` | ~1.5k | 407 µs | < 200 µs | ✗ |
| `visible_prefix_full_miss_large_8590` | 8,590 | 7.0 ms | — | — |
| `walker_pnpm_style_ignored_10k_repo_100k_store` | 10k + 100k (ignored) | 54.4 ms | O(repo) | ✓ (store not descended) |

These benches exercise the UI projection layer far more than the scanner path. None of them covers watcher storms above 1,000 events, trees above 8.6k entries, or real media layouts. That is why §5 uses a dedicated harness.
