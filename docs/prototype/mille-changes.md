# Mille changes for flux (`flux/dogfood`)

**Worktree:** `$FLUX_WORK/wt/mille`, branch `flux/dogfood`, based on `c8160d5` (Mille main, v0.3.4). Not pushed; the maintainer reviews and merges.
**Scope:** research 07 §8 items P0 #2, P0 #3, P0 #4 and P1 #5, plus two read-pool fixes that the new fast path exposed.
**Crate touched:** `mille-core`, plus two one-line changes in `mille-binding`: `first_response: None` in `explorer.rs`, and an explicit discard of a provably empty report in `watch_runtime.rs`. The binding compiles and is clippy-clean.

## Commits

| Commit | What it does |
|---|---|
| `57ee0f6` | **feat(core):** a read's first answer gets its own deadline. `ReadBoundaryOptions.first_response: Option<Duration>` (None = `timeout`, so existing callers are unchanged). `ReadBoundaryOptions::media()` = 30 s first response, 2 s stall. A spun-down disk is no longer reported as stalled. (P0 #4) |
| `deef2f6` | **perf(core):** `coalesce_events` is one O(n) pass instead of O(n²). The old code is kept in the tests as an oracle; a proptest compares the two, and an 80k-event test pins linearity. (P0 #3) |
| `dd246b0` | **fix(core):** `walk`, `walk_with_ignore`, `walk_batched` and `walk_with_ignore_batched` now fail with the new `FxError::Incomplete { unread }` instead of silently dropping unread subtrees. `WalkReport::into_complete()` and `#[must_use]` on `WalkReport`. `events_since` no longer reports a stalled subtree as `Disappeared`. (P0 #4) |
| `bf764f1` | **feat(core):** `walk_scan` + `ScanEntry`/`DirCtx`/`Visit`/`ScanOptions`/`ScanReport`, the portable lister, and the Linux `getdents64` + `statx(STATX_BTIME)` lister via rustix. (P0 #2, P1 #5 Linux, P2 #11, P2 #12) |
| `3440965` | **perf(core):** the macOS `getattrlistbulk` lister: 128 KiB buffer, dataless policy kept, `ENOTSUP`/`EINVAL` fallback, and a loop guard on network volumes. (P1 #5) |
| `f53f3c1` | **fix(core):** a bounded read that already finished is never joined. A latent race let a new request receive the previous read's answer. |
| `963e3f9` | **perf(core):** an idle read helper spins 50 µs for the next read before parking. This saves about 25 µs per directory on a serial walk. |
| `5f421a9` | **perf(core):** at most one helper spins at a time, which halves a parallel scan's user CPU. |
| `d109e02` | **docs(core):** a compiled `no_run` example on `walk_scan`. |
| `1a96400` | **refactor(core):** walk_scan refills in-flight reads after each listing it resolves (`Scan::fill`). No measurable change under load. |

Tests: `cargo test -p mille-core` gives 293 unit tests, plus 26 integration tests and 1 doctest, all green. That is 291 before this work; 28 new tests cover scan, deadlines, coalescing and unread. Clippy runs with `-D warnings` and is clean for:

- `mille-core`, `mille-binding` and `mille-bench` on macOS;
- `mille-core` on `x86_64-unknown-linux-gnu`, on `aarch64-unknown-linux-gnu` (with `--features blake3/pure`, because blake3's C build script needs a cross compiler) and on `x86_64-pc-windows-msvc` (same flag).

Reproduce:

```bash
cd $FLUX_WORK/wt/mille
export CARGO_TARGET_DIR=$FLUX_WORK/target/agent-mille
cargo test -p mille-core                                   # includes the real-media test on library-tiny
cargo clippy -p mille-core -p mille-binding --all-targets -- -D warnings
cargo clippy -p mille-core --target x86_64-unknown-linux-gnu --all-targets -- -D warnings

# benchmarks (before = c8160d5, after = worktree); results -> $FLUX_WORK/bench/results
export CARGO_TARGET_DIR=$FLUX_WORK/target/agent-mille-bench
(cd $FLUX_WORK/bench/mille-scanbench && cargo build --release)
(cd $FLUX_WORK/bench/mille-scanbench-base && cargo build --release)
cd $FLUX_WORK/bench/mille-scanbench
python3 -I run.py library; python3 -I run.py media; python3 -I run.py coalesce; python3 -I run.py project100
```

## What changed

### `walk_scan`: a media-grade streaming scan (P0 #2)

```rust
pub fn walk_scan<F>(root: &Path, options: &ScanOptions, reads: &ReadBoundary, visit: F)
    -> Result<ScanReport, FxError>
where F: FnMut(&DirCtx<'_>, &ScanEntry<'_>) -> Visit;

pub struct ScanEntry<'a> {
    pub name: &'a OsStr,      // verbatim, non-UTF-8 included (never dropped)
    pub kind: EntryKind,      // File | Directory | Symlink (unfollowed/dangling) | Unknown
    pub is_symlink: bool,     // the entry itself is a link (kind = target's when followed)
    pub size: u64,            // files only, 0 otherwise
    pub mtime_ns: i64,
    pub btime_ns: Option<i64>,// APFS/HFS+ always; Linux statx where the fs records it
    pub dev: u64, pub ino: u64,  // identity (0 on Windows)
    pub flags: u32,           // macOS st_flags / Linux STATX_ATTR_* / Windows FILE_ATTRIBUTE_*
    pub nlink: u64,           // files: st_nlink (>1 = hardlinked); dirs: always 1
}
pub struct DirCtx<'a> { pub path: &'a Path, pub depth: usize, pub id: u64, pub parent: Option<u64>, pub dev: u64, pub ino: u64 }
pub enum Visit { Continue, SkipChildren }
```

- **Fields match `lstat` exactly.** Tests compare `dev`, `ino`, `btime`, `mtime_ns`, `st_flags`, `nlink` and `size` with `std::fs::symlink_metadata` for both listers, on temp trees and on the 424-file `library-tiny`.
- **Allocations.** Each directory's path is built once, and each listing keeps its names in one arena. There is no per-entry allocation unless the visitor makes one. The old record cost about 450 B and 6 allocations per entry.
- **Unordered and streaming.** Up to `parallelism` directory reads are in flight at once. A directory's children arrive together, after the directory's own entry. A change of `dir.id` means the previous directory is complete. `SkipChildren` means the directory is never opened.
- **`ScanOptions`:**
  - `max_depth`.
  - `follow_symlinks`:
    - `Never` (the default).
    - `Smart`: follow, and walk each linked directory's `(dev, ino)` once. Real directories are always walked. There is no per-link `canonicalize`.
    - `Always`: follow, and cut ancestor cycles by `(dev, ino)`.
  - `include_hidden`.
  - `default_ignores`: `.DS_Store`, `._*`, `@eaDir`, `#recycle`, `#snapshot`, `@Recycle`, `.Trash-*`, `Thumbs.db`, `desktop.ini`, `$RECYCLE.BIN`, `lost+found`, …. These are also published as the gitignore text `DEFAULT_IGNORE_PATTERNS`, and a test keeps the fast name matcher and `IgnoreMatcher` in agreement.
  - `ignore: Option<Arc<IgnoreMatcher>>`.
  - `ignore_file_names`, e.g. `.plexignore`. Their text is read inside the bounded read.
  - `file_filter`: `FileFilter::extensions([...])` or `FileFilter::new(fn)`. It runs on the read helper before the per-entry `stat`.
  - `parallelism` (default 8), `cancel: Option<CancelFlag>`, and `lister: Auto | Portable`.
- **`ScanOptions::media()`** sets `Smart` links, hides dot-names, applies the default ignores and honours `.plexignore` and `.ignore`.
- **`ScanReport`** is `#[must_use]`:
  - `unread: Vec<UnreadDirectory>`: every directory below the root whose children are unknown. The read stalled, was refused, hit a dataless placeholder, or failed with an OS error such as `EACCES`. These directories are unknown, not empty.
  - Counters: `dirs_listed`, `entries`, `skipped`, `vanished`, `links_not_walked`.
  - `ignore_files`.
  - `listers { bulk, statx, portable }`.
  - A root that cannot be read is an error with its own code (`ENOENT`, `ENOTDIR`, `EACCES`, `ETIMEDOUT`, `EDATALESS`).

### Platform listers inside `ReadBoundary` (P1 #5)

All three listers run as a new `Op::Scan` on the existing helper pool, so the following apply exactly as for `list_dir`:

- stall and first-response deadlines;
- the stuck cap;
- joined reads;
- the dataless policy.

The three listers:

- **macOS `getattrlistbulk`.**
  - Requests `NAME | ERROR | DEVID | OBJTYPE | CRTIME | MODTIME | FLAGS | FILEID` plus `FILE_LINKCOUNT | FILE_DATALENGTH` into a reused 128 KiB per-thread buffer, and ticks progress once per batch.
  - `SF_DATALESS` on a child directory still short-circuits its read, and `open` of a dataless directory still answers `EDEADLK` (reported as `Dataless`).
  - `ENOTSUP`/`EINVAL` on the first call falls back to the portable lister.
  - On non-`MNT_LOCAL` volumes, a name returned twice restarts the directory with `readdir`. This guards against the SMB looping bug (FB15497909).
  - Every buffer read is bounds-checked, and malformed data is an `InvalidData` error, not a panic.
- **Linux `getdents64` + `statx(dirfd, name, AT_SYMLINK_NOFOLLOW, BASIC_STATS|BTIME)`** via rustix 1.1. `ENOSYS`/`EPERM` fall back to the portable lister. **This is compile-checked only:** no Linux host or running Docker daemon was available.
- **Portable `read_dir` + `DirEntry::metadata`** (dirfd-relative `fstatat`), used everywhere else and as the fallback.

### O(n) coalescer (P0 #3)

Each output slot now records its path's index, and the lookup map borrows keys from the batch. The semantics are unchanged, as the proptest against the old implementation shows.

**Not done:** the per-batch directory dedupe in reconcile. Reconcile lives only in the binding (`mille-binding/src/watch_runtime.rs`, `process_batch` → `reconcile_nearest_parent`), not in mille-core. It still re-lists the parent once per event. flux should do its own reconcile: per batch, group events by parent directory and run `walk_scan(dir, max_depth: Some(1))` once per directory.

### Explicit `unread`, split deadlines (P0 #4)

`first_response` vs `timeout` (the stall deadline) is described in the first commit above.

The report-dropping walks now return `FxError::Incomplete { unread }`, whose code is the first directory's own reason. `events_since` inherits this, so resuming over a sleeping drive is an error, not a mass `Disappeared`.

`#[must_use]` on `WalkReport` flagged one discard in the binding. That discard is a provably empty depth-1 report, and it is now explicit.

## How flux-scan should call `walk_scan`

`flux/Cargo.toml` already has `mille-core = { path = ".../wt/mille/crates/mille-core" }`. Add `mille-core.workspace = true` to `crates/flux-scan/Cargo.toml`.

```rust
use std::path::{Path, PathBuf};
use std::sync::mpsc::SyncSender;

use mille_core::{
    walk_scan, CancelFlag, ErrorCode, FileFilter, FxError, ReadBoundary, ReadBoundaryOptions,
    ScanOptions, ScanReport, Visit,
};

/// What the diff stage needs per file. (flux-core has no scan-record type; this
/// is flux-scan's own.)
pub struct SeenFile {
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i64,
    pub btime_ns: Option<i64>,
    pub nlink: u64,
}

/// Video, subtitle, NFO and art sidecars. Names only: the probe sniffs magic
/// bytes, so a `.ts` that is TypeScript is rejected there.
const EXTENSIONS: &[&str] = &[
    "mkv", "mk3d", "webm", "mp4", "m4v", "mov", "avi", "ts", "m2ts", "mts", "mpg", "mpeg",
    "vob", "wmv", "flv", "ogv", "iso", "srt", "ass", "ssa", "vtt", "sub", "idx", "sup",
    "nfo", "jpg", "jpeg", "png", "webp",
];

/// One boundary per *device*, shared by every root on it: deadlines, the stuck
/// cap and the helper pool are per device, so a sleeping NAS cannot starve the
/// internal SSD.
pub fn boundary_for(rotational_or_network: bool) -> ReadBoundary {
    ReadBoundary::new(ReadBoundaryOptions {
        // 2–4 for a spinning disk or a NAS share, 16 for SSD/NVMe.
        concurrency: if rotational_or_network { 4 } else { 16 },
        // 30 s to wake a sleeping disk, 2 s per stall after it answers, never
        // download iCloud-evicted folders.
        ..ReadBoundaryOptions::media()
    })
}

/// Blocking: call from `tokio::task::spawn_blocking`.
pub fn scan_root(
    root: &Path,
    reads: &ReadBoundary,
    parallelism: usize, // = the boundary's concurrency
    cancel: CancelFlag,
    out: &SyncSender<Vec<SeenFile>>, // bounded: the diff/probe stage applies back-pressure
) -> Result<ScanReport, FxError> {
    let options = ScanOptions {
        parallelism,
        file_filter: Some(FileFilter::extensions(EXTENSIONS)),
        cancel: Some(cancel),
        // Smart links ((dev, ino)-deduped), no dot-names, NAS/OS litter skipped,
        // .plexignore / .ignore honoured.
        ..ScanOptions::media()
    };
    let mut batch = Vec::with_capacity(1024);
    let report = walk_scan(root, &options, reads, |dir, e| {
        if e.is_file() {
            batch.push(SeenFile {
                path: dir.join(e.name), // the only per-file allocation
                dev: e.dev,
                ino: e.ino,
                size: e.size,
                mtime_ns: e.mtime_ns,
                btime_ns: e.btime_ns,
                nlink: e.nlink,
            });
            if batch.len() >= 1024 {
                // Blocking here is fine: in-flight reads keep their own deadlines.
                let _ = out.send(std::mem::replace(&mut batch, Vec::with_capacity(1024)));
            }
        }
        Visit::Continue
    })?;
    if !batch.is_empty() {
        let _ = out.send(batch);
    }
    Ok(report)
}

// Caller, after the diff stage has consumed every batch:
// match scan_root(..) {
//     Ok(report) => {
//         // Diff on (dev, ino, size, mtime_ns); btime breaks ties after inode reuse.
//         // Same (dev, ino) at a new path = move/rename: keep the item, no re-probe.
//         // nlink > 1 and the same (dev, ino) at two paths = one asset, probe once.
//         // Remove only what is absent AND not under an unread directory:
//         let unknown: Vec<&Path> = report.unread.iter().map(|u| u.path.as_path()).collect();
//         // catalog.remove_missing(root, &seen, |p| unknown.iter().any(|u| p.starts_with(u)));
//         // and mark those subtrees "unavailable" in the UI.
//     }
//     Err(FxError::Cancelled) => {}
//     // The root itself did not answer or is gone: never delete. ScanState::Unavailable.
//     Err(e) if matches!(e.code(), ErrorCode::ETIMEDOUT | ErrorCode::EDATALESS
//                                | ErrorCode::ENOENT | ErrorCode::EACCES) => { /* keep items */ }
//     Err(e) => { /* ScanState::Error, last_error = e.to_string() */ }
// }
```

Notes for flux-scan:

- **Zero-change rescans.** `walk_scan` opens directories only, never files, so a zero-change rescan opens zero media files (ARCHITECTURE invariant). Compare `(dev, ino, size, mtime_ns)` and skip unchanged files before any probe.
- **Directory-at-a-time grouping** (for flux-names' siblings) needs no extra pass: entries arrive grouped by `dir.id`.
- **Watcher reconcile.** Group the coalesced events of a batch by parent directory. Then call `walk_scan(parent, &ScanOptions { max_depth: Some(1), ..ScanOptions::media() }, reads, …)` once per directory, rather than once per event.
- **Remote "browse a folder" endpoint.** Use the same call with `max_depth: Some(1)`, a 2–10 s `first_response`, and `report.unread`/root errors surfaced as "drive waking up / not responding".

## Before/after numbers

Harness: `$FLUX_WORK/bench/mille-scanbench`. `run.py` and `README.md` live there; the source is one `main.rs`, built twice:

- **before:** against `c8160d5`, archived to `../mille-base-c8160d5`;
- **after:** against the worktree.

Every walker mode walks without following symlinks, includes hidden entries, applies no ignore rules, and reads kind + size + mtime for every entry. The modes report identical entry/file/dir/link counts. `scan_media` is the flux-shaped run: `ScanOptions::media()`, a video-extension filter, and one `PathBuf` per kept file. `reference.gab_par` is the research-07 raw `getattrlistbulk` prototype (no `ReadBoundary`, no record).

Results JSON:

- `$FLUX_WORK/bench/results/flux-mille-walk-{library,media,project100}.json`
- `$FLUX_WORK/bench/results/flux-mille-coalesce.json`

> **Caveat.** These numbers were taken on the dev machine (M1 Max, 10 cores) while 8 agents were compiling. The 1-minute load average was **25–120** during these runs. Per-run spread is ±50–300% (all runs are listed), and medians are the honest summary. Re-run `python3 -I run.py <set>` on a quiet machine before quoting absolute numbers.

**library** — `$FLUX_WORK/media/library`: 21,011 entries (16,671 files, 4,340 dirs); load avg (1/5/15 min) before [35.43, 50.8, 48.12], after [34.51, 50.36, 47.98]; median of 5 interleaved runs.

| mode | median s | files/s | × before | user s | sys s | peak RSS MB | all runs (s) |
|---|---:|---:|---:|---:|---:|---:|---|
| `before.walk_bounded.p8` | 0.108 | 154,504 | 1.00 | 0.09 | 0.26 | 19.7 | 0.142, 0.096, 0.083, 0.108, 0.131 |
| `before.walk_batched.p8` | 0.101 | 165,223 | 1.07 | 0.08 | 0.24 | 13.9 | 0.095, 0.085, 0.133, 0.105, 0.101 |
| `after.walk_bounded.p8` | 0.119 | 140,092 | 0.91 | 0.10 | 0.28 | 19.8 | 0.093, 0.119, 0.159, 0.103, 0.305 |
| `after.scan.p8` | 0.093 | 179,066 | 1.16 | 0.06 | 0.22 | 11.4 | 0.076, 0.075, 0.094, 0.093, 0.125 |
| `after.scan.p16` | 0.084 | 197,524 | 1.28 | 0.05 | 0.21 | 13.1 | 0.106, 0.062, 0.084, 0.088, 0.082 |
| `after.scan_portable.p8` | 0.118 | 140,802 | 0.91 | 0.08 | 0.26 | 9.1 | 0.122, 0.100, 0.118, 0.090, 0.452 |
| `after.scan.p1` | 0.186 | 89,581 | 0.58 | 0.15 | 0.14 | 8.5 | 0.228, 0.186, 0.172, 0.145, 0.273 |
| `after.scan_media.p8` | 0.085 | 174,057 | 1.27 | 0.06 | 0.22 | 14.8 | 0.098, 0.100, 0.079, 0.085, 0.074 |
| `reference.gab_par.p8` | 0.045 | 370,467 | 2.40 | 0.01 | 0.24 | 7.2 | 0.060, 0.044, 0.045, 0.046, 0.044 |
| `reference.gab_par.p16` | 0.041 | 406,610 | 2.63 | 0.01 | 0.26 | 7.5 | 0.041, 0.040, 0.057, 0.040, 0.041 |

**media** — `$FLUX_WORK/media`: 101,085 entries (92,557 files, 8,528 dirs); load avg (1/5/15 min) before [34.51, 50.36, 47.98], after [28.41, 47.93, 47.16]; median of 5 interleaved runs.

| mode | median s | files/s | × before | user s | sys s | peak RSS MB | all runs (s) |
|---|---:|---:|---:|---:|---:|---:|---|
| `before.walk_bounded.p8` | 0.500 | 185,003 | 1.00 | 0.35 | 0.70 | 47.5 | 1.39, 0.546, 0.471, 0.500, 0.437 |
| `before.walk_batched.p8` | 0.511 | 181,094 | 0.98 | 0.35 | 0.67 | 16.7 | 0.511, 0.539, 0.553, 0.455, 0.482 |
| `after.walk_bounded.p8` | 0.475 | 194,652 | 1.05 | 0.41 | 0.70 | 48.0 | 0.475, 0.526, 0.492, 0.465, 0.453 |
| `after.scan.p8` | 0.161 | 575,961 | 3.11 | 0.14 | 0.64 | 12.2 | 0.161, 0.163, 0.172, 0.142, 0.150 |
| `after.scan.p16` | 0.150 | 617,047 | 3.34 | 0.11 | 0.66 | 14.6 | 0.238, 0.145, 0.188, 0.150, 0.139 |
| `after.scan_portable.p8` | 0.226 | 409,544 | 2.21 | 0.23 | 0.93 | 10.7 | 0.213, 0.261, 0.226, 0.242, 0.204 |
| `after.scan.p1` | 0.373 | 247,943 | 1.34 | 0.34 | 0.30 | 8.9 | 0.376, 0.396, 0.373, 0.351, 0.347 |
| `after.scan_media.p8` | 0.180 | 215,441 | 2.78 | 0.16 | 0.65 | 19.8 | 0.193, 0.180, 0.173, 0.195, 0.153 |
| `reference.gab_par.p8` | 0.102 | 907,422 | 4.90 | 0.01 | 0.61 | 7.7 | 0.138, 0.102, 0.102, 0.099, 0.105 |
| `reference.gab_par.p16` | 0.099 | 934,919 | 5.05 | 0.01 | 0.70 | 8.0 | 0.104, 0.096, 0.099, 0.096, 0.120 |

**project100** (a large tree of source checkouts on the internal disk): 2,067,716 entries (1,750,294 files, 260,469 dirs); load avg (1/5/15 min) before [28.41, 47.93, 47.16], after [36.49, 49.13, 49.1]; median of 2 interleaved runs.

| mode | median s | files/s | × before | user s | sys s | peak RSS MB | all runs (s) |
|---|---:|---:|---:|---:|---:|---:|---|
| `before.walk_bounded.p8` | 49.012 | 35,711 | 1.00 | 12.07 | 50.83 | 829.2 | 40.19, 57.83 |
| `before.walk_batched.p8` | 63.461 | 27,581 | 0.77 | 11.79 | 50.06 | 69.5 | 60.05, 66.88 |
| `after.scan.p8` | 18.179 | 96,283 | 2.70 | 7.22 | 40.73 | 24.1 | 24.58, 11.77 |
| `after.scan.p16` | 13.090 | 133,710 | 3.74 | 4.78 | 49.56 | 25.4 | 17.45, 8.74 |
| `after.scan_portable.p8` | 25.075 | 69,802 | 1.95 | 10.36 | 64.05 | 19.4 | 35.99, 14.16 |
| `after.scan_media_nofollow.p8` | 7.321 | 19,211 | 6.70 | 2.55 | 14.69 | 44.0 | 11.23, 3.41 |
| `reference.gab_par.p8` | 9.773 | 179,095 | 5.02 | 0.49 | 39.45 | 10.2 | 10.28, 9.27 |
| `reference.gab_par.p16` | 8.501 | 205,893 | 5.77 | 0.54 | 50.59 | 11.7 | 8.89, 8.11 |

`coalesce_events` (n unique `Created` events, one batch):

| events | before (c8160d5) | after |
|---:|---:|---:|
| 1,000 | 28.8 ms | 1.7 ms |
| 5,000 | 657.9 ms | 1.8 ms |
| 10,000 | 2,508.6 ms | 2.4 ms |
| 20,000 | 13,601.4 ms | 5.0 ms |
| 40,000 | — | 10.1 ms |
| 100,000 | — | 23.4 ms |
| 1,000,000 | — | 920.0 ms |

Load before/after: [76.59, 41.3, 35.02] / [75.78, 43.49, 35.96]. Research 07 measured 40k events at 86 s before (load 75–100).

### Reading the numbers

The main result: **`scan` p8 is 2.7–3.1× faster than the old `walk_bounded` p8, and p16 is 3.3–3.7× faster, on the 101k- and 2.07M-entry trees, at a fraction of the memory.** Peak RSS is 12 MB vs 48 MB on the media tree, and 24 MB vs 829 MB on project100. This is the research's "≥ 2.5× on the media fixture" target. The scan uses less user CPU than the old walker and no more system CPU, and every entry carries `(dev, ino)`, birth time and ns mtime.

The small-directory library is the weak spot: 4.8 entries per directory, and only 1.16× (p8) to 1.28× (p16) faster.
- There the per-directory consumer↔helper handoff dominates, not the per-entry `lstat` that `getattrlistbulk` removes.
- The raw parallel prototype, which has no handoff, is 2.4–2.6× faster than the old walker on the same tree.
- See *Known gaps*.

**`scan_portable`** (`read_dir` + dirfd-relative `fstatat`, the Linux-like path) is already 1.9–2.2× faster than the old walker on the bigger trees. Most of that comes from the record and the allocation savings, not from the syscall.

**`scan_media`** applies `ScanOptions::media()` and a video filter. On media it takes 0.18 s for 101k entries and keeps 38.8k video files. On project100 it is measured without link following: there, `Smart` follows links out of the tree (see *Known gaps*). The figure for the 2M-entry code tree is 7.3 s median (3.4 s in the quieter run).

**`coalesce_events`:**
- 20k events took 13.6 s before and 5 ms after; 40k took 10 ms.
- One million events take 0.9 s; the old code was quadratic.
- Per-event cost stays flat at about 0.25 µs.

These benchmarks ran at `d109e02`. `1a96400` (the refill refactor) showed no difference outside noise in an interleaved A/B.

## Known gaps

- **The per-directory handoff (research P1 #6) is not done.** `walk_scan` keeps Mille's architecture: one consumer thread dispatches directory reads to the boundary's helper pool, and each directory costs a submit → wake → finish → notify round trip. That round trip is why `walk_scan` stays behind the self-feeding raw `getattrlistbulk` prototype: 1.6–2.1× at 8 in flight in these runs (library 2.1×, media 1.6×, project100 1.9×), and up to ~5× in single runs at load ~75. The gap is worst on trees of small directories. Worker-owned traversal would close it, but it conflicts with `Visit::SkipChildren`'s "never opened" guarantee and with consumer-side ignore rules. It needs a design decision from the maintainer.
- **The default `parallelism` is 8,** which matches the boundary's default `concurrency` and is safe for HDDs. On SSD/NVMe, 16 is consistently faster here, because more reads in flight hide the handoff latency. flux should set both per device.
- **`Smart` link following can leave the root.** Like Plex and Jellyfin, it follows links wherever they point. It dedups by `(dev, ino)`, so there are no loops, but it does not stop at the root or device. On project100 the media profile followed 5,444 directory links, including links into `$HOME` and onto the busy USB SSD (cargo `target/` trees). That took 28–137 s (four runs) instead of 3.4–11 s with `Never`. A `same_device`/root-confinement option is the obvious next step; it is not implemented.
- **Linux statx lister:** compile-checked and clippy-clean for x86_64 and aarch64, but **never executed**. No Linux host or running Docker daemon was available. The Linux runs of the tests (including the non-UTF-8 name test, which APFS cannot exercise because it refuses non-UTF-8 names with `EILSEQ`) are still owed.
- **Windows** is compile-checked only. There, `dev` and `ino` are 0, because stable std exposes no file index.
- **The getattrlistbulk loop guard** (non-local volumes) is untested against a real SMB share.
- **Mount points inside a scanned tree.** `getattrlistbulk` reports a mount point directory's *covered* `DEVID`/`FILEID` (man page), while `fstatat` reports the mounted root's. The two listers disagree on that single entry. Entries inside the mount are correct in both.
- **Reconcile dedupe** (P0 #3, second half) lives in the binding and is untouched. Mille's watcher still has no FSEvents resume (P1 #7), uses unbounded queues (P1 #9), and pairs renames heuristically (P1 #8).
- **The new helper spin** costs at most 50 µs of one core per finished read while a walk is running. It cut serial `walk_scan` time by about 25% (0.48 s → 0.36 s on the 101k-entry tree). A boundary with nothing to do still parks.
- **`FxError::Incomplete` is a new variant.** The binding maps it generically (`code()` → ETIMEDOUT/EDATALESS/…), with no TS-side change. Callers of `walk()` that relied on partial results now get an error instead, which is the point.

## Phase D: review follow-up (`flux/dogfood-d`)

> **Merged.** `flux/dogfood` was fast-forwarded to `flux/dogfood-d` (`2c064e8`) by the Phase D merge, and flux-scan adopted steps 1–3 below on `main` (`8882c8e`): stateful coalescing, the store-free unsynced snapshot, and FSEvents history first in `StartupCatchUp::MilleResume`. `QuickScan` stays the default until step 3f (refreshing the token while watching) exists. INT-5 needed no flux change. See [fixes.md](fixes.md).

**Worktree:** `$FLUX_WORK/wt/mille-d`, branch `flux/dogfood-d`, based on `1a96400` (the tip of `flux/dogfood`). Not pushed. The merge agent fast-forwards `flux/dogfood` to it.
**Scope:** five items. Two are bugs flux hit: the debounced watcher dropping deletions, and INT-5's order-dependent Smart links. Three are APIs flux asked for in `scan.md` → "Proposed changes elsewhere": a coalesce mode that never drops a path, FSEvents `sinceWhen` resume, and a store-free snapshot writer.
**Crates touched:** `mille-core` and one new example in `mille-bench`. Every change is additive. flux at `1fbcb63` compiles against the branch unchanged, and `cargo test -p flux-scan` passes there. No dependency or `Cargo.lock` change: the FSEvents FFI is declared in `history.rs`.

### Commits

| Commit | What it does |
|---|---|
| `bf778a6` | **fix(core):** Smart directory links are decided independently of listing order (INT-5). Links are held until nothing is pending or in flight, then decided in path order against everything walked, round by round. Adds `ScanReport::links_skipped`. |
| `0777350` | **fix(core):** the debounced watcher delivers deletions that FSEvents reports with an accumulated create flag. On macOS it also delivers replacements reported the same way. |
| `c5eb2a1` | **feat(core):** `coalesce_events_with(raw, CoalesceOptions)`. `CreateThenDelete::Report` (`CoalesceOptions::stateful()`) reports a created-then-deleted path by its state on disk instead of dropping it. |
| `6c75111` | **perf(core):** `write_snapshot_from(&[WalkedEntry], &[RootStat], path)`, with no `EntryStore`. Adds `write_snapshot_from_with(.., Durability::Unsynced)`. |
| `a152172` | **feat(core):** module `history`, a replay of a root's changes since a saved FSEvents event id. Adds the `mille-bench` example `flux_resume`, which produces the measurements below. |
| `2c064e8` | **test(core):** the live history test starts from a quiet point, because fseventsd numbers events late under load. |

### 1. The debounced backend dropped deletions on macOS (fixed)

**Cause.**
- FSEvents reports each path's *accumulated* flags, and notify splits one record into one event per flag, creates first.
- So a file created a while ago and deleted now arrives as `Create` + `Remove`. notify-debouncer-full (`push_remove_event`: queue `was_created` → drop it) cancels that pair as a transient file.
- A file removed and re-created arrives the same way, and was lost too.

**Fix.**
- On macOS the debouncer now sits on `FsEventsWatcher`: notify's watcher with `FsEventsHistoryFilter` in front of the handler.
- The filter lets the disk decide which half is history. It drops a `Create` for a path that is gone, and a `Remove` for a path that exists, before the debouncer pairs them.
- A truly transient file still cancels when both of its records land inside the window, because its `Create` was queued while it existed. Otherwise the consumer gets a `Remove` for a path it never saw, which is harmless.
- Mount and unmount events pass through untouched. The raw backend and the other platforms are unchanged.

**Tests.**
- `debouncer_delivers_deletions_and_replacements_reported_with_accumulated_flags` drives the real debouncer through a hand-fed watcher. It covers deletion, replacement and a transient file.
- `only_the_stale_half_of_accumulated_flags_is_dropped` checks the filter's decisions.
- `debounced_watcher_reports_deleting_a_file_cloned_in_earlier` is flux's repro on real FSEvents: `cp -c` in, wait 300 ms, `rm`.
- With the filter disabled, the first and the last test fail. The FSEvents test then gets no `Deleted` at all, which confirms the finding.

### 2. `coalesce_events` cancels a create+delete (new mode)

- `coalesce_events_with(raw, CoalesceOptions { create_then_delete })`:
  - `CreateThenDelete::Cancel` is the default. It is `coalesce_events` unchanged, and still pure.
  - `CreateThenDelete::Report` (`CoalesceOptions::stateful()`) handles a path whose events net out to created-then-deleted with one `lstat` at coalescing time. A gone path is reported `Deleted`; a path that exists is reported `Created`.
  - `Unknown` was rejected: it is the `RenamePairer`'s input channel, and would pair a deleted file with an unrelated one into a bogus `Renamed`.
- **Tests.** One test covers the gone and the re-created cases. A proptest checks that `Report` changes nothing but the paths the default cancels, and reports every one of them.

### 3. FSEvents `sinceWhen` resume (research/07 P1 #7)

**Module `history`.** It is macOS only; elsewhere it returns `FxError::Unsupported`, so the caller walks.

- **`HistoryToken::capture(root)`** records:
  - the volume's FSEvents journal UUID (`FSEventsCopyUUIDForDevice`);
  - `FSEventsGetCurrentEventId()`, read first;
  - the root's inode;
  - the time.

  `write` and `read` keep the token in a small versioned text file, atomically, and any root name round-trips.
- **`replay_since(&token, &ReplayOptions)`** runs an FSEvents stream from `sinceWhen = token.event_id` on a thread of its own. It runs until `HistoryDone`, then calls `FSEventStreamFlushSync`, and returns `ReplayReport { replay, next, elapsed }`.
  - **`Replay::Changes(HistoryChanges)`** carries:
    - `changed`: each path once, with flags merged (`created`, `removed`, `renamed`, `modified`, `metadata`, `is_file`, `is_dir`, `is_symlink`), mapped back under the caller's root path;
    - `dirs_to_rescan(root)`: the parents to list again;
    - `rescan_subtrees`: directories where FSEvents dropped history (`MustScanSubDirs`, mounts), to walk whole.
  - **`Replay::Rescan(reason)`** means walk the root. The reasons are:
    - `VolumeChanged`: the UUID differs.
    - `IdAhead`: the saved id is above the current one.
    - `IdsWrapped`.
    - `RootReplaced`: the root's inode changed.
    - `RootChanged`.
    - `HistoryIncomplete`: history was lost at or above the root.
    - `TooOld`: older than `max_age`, 30 days by default.
    - `TooManyEvents`: more than `max_events`, 100k by default.
    - `TimedOut`: no `HistoryDone` within `timeout`, 30 s by default.
  - **`next`** is captured before the replay starts. Save it only once the catch-up has been applied: a crash before that replays the same changes again, which is harmless.
  - A missing root returns its own stat error, never a change set.
- **A stale token is always safe.** An older token replays a superset of the changes. A resume snapshot is not safe that way: see the gaps.

**Tests.**
- The token file round-trips, including a root name containing `\nroot=`. Another format version gives `Unsupported`; a damaged file gives `InvalidInput`.
- Record interpretation (merging, mapping back to the root, subtrees, `Rescan` reasons) is tested on every platform.
- On real FSEvents, a replay names exactly the files that were created, rewritten, deleted and renamed, and the new directory, and nothing untouched. `dirs_to_rescan` is exact. A replay from the next token is empty, and a later single change is the only change.
- Every `Rescan` reason the test can provoke is checked: `VolumeChanged`, `IdAhead`, `RootReplaced`, `TooOld` and `TooManyEvents`, plus a missing root giving `ENOENT`.

### 4. `write_snapshot_from` (no `EntryStore`)

- **`write_snapshot_from(&[WalkedEntry], &[RootStat], path)`** writes exactly what `populate_store` into a fresh `EntryStore` followed by `write_snapshot` writes:
  - ids in `(depth, path)` order;
  - children in the store's default sibling order;
  - child counts, visible counts and subtree sizes, from one bottom-up pass.

  Only `created_ms` differs, and a test compares the two field by field.
- **Roots.** A root missing from the walk is made up from its `RootStat` (flux builds none).
- **Rejected input.** Orphans, duplicate paths, roots without a `RootStat` and entries whose depth is not below their parent's are `InvalidInput`.
- **The fsync floor.** On this USB SSD, two `fsync`s dominate what remains: 65–125 ms for a 1.2–1.9 MB snapshot, up to 540 ms under heavier load, against about 1 ms to write and rename it unsynced.
- **`write_snapshot_from_with(.., Durability::Unsynced)`** skips them, for a snapshot that only saves work. It is atomic against a process crash, but not against a power loss: after one, the file may be the previous snapshot or one that `read_snapshot` rejects.

### 5. INT-5: Smart link decisions depend on listing order (fixed)

**Cause.**
- Under `SymlinkPolicy::Smart`, each link was decided as soon as its own listing landed. Within one listing, links already came after real directories, but not across siblings.
- With `Alias/X -> Real/X`, X was walked twice when `Alias/` answered first: the link, then the real directory, which is always walked. Otherwise X was walked once.

**Fix.**
- Links are now held until nothing is pending or in flight. Then they are decided in path order against the set of everything walked.
- That set covers everything reachable without crossing a link, then, round by round, everything reachable through the links chosen so far.
- So a link to a directory of the same tree is never walked. Of several links to one outside directory, the first by path is walked.
- Trees without links walk exactly as before.
- `ScanReport::links_skipped` lists the links that were not walked.

**Tests.**
- `smart_link_decisions_do_not_depend_on_which_listing_answers_first` forces both orders with a `ReadHook` gate. Without the fix the two orders deliver different trees.
- In flux, against this branch with flux unchanged:
  - The reviewer's `directory_symlink_alias_flip_flops_between_scans` now passes: 0 writer commands on the four unchanged rescans, and one asset.
  - The verifier's no-delay variant shows 0 of 19 rescans writing.
- Against the old `flux/dogfood` the same tests show the bug:
  - the reviewer's test fails with 2 writer commands on every rescan, and the movie flips to 2 assets;
  - the verifier's variant shows 2 of 19 rescans writing.

### Numbers

All runs used the release build of the example `flux_resume`, on the shared M1 Max, with the USB SSD busy from other agents' builds. The 1-minute load average is in each result line; it was 12–48. The commands are in `crates/mille-bench/examples/flux_resume.rs`.

**Snapshot write.** The media walk is the one flux keeps: `ScanOptions::media()` and its extension filter, with no root entry. Medians of 9 runs at load 12:

| tree | entries | EntryStore path (populate_store + write_snapshot) | `populate_store` alone | `write_snapshot_from` | `.._with(Unsynced)` | fsync floor (same bytes) |
|---|---:|---:|---:|---:|---:|---:|
| `library/TV` | 12,304 | 108 ms | 29 ms | 88 ms | **20 ms** | 74 ms |
| `library` | 21,000 | 149 ms | 59 ms | 103 ms | **40 ms** | 82 ms |

- Earlier runs at load 22–25 gave 217–508 ms for the store path and 149–413 ms for `write_snapshot_from`; the fsync floor ran 88–542 ms.
- flux's "220–330 ms" is most likely mostly that fsync.
- Unsynced, the snapshot costs 20–40 ms on this tree.

**Startup catch-up (FSEvents replay).**
- **Setup.** An APFS clone of the 16.7k-file library: 21,012 entries, at `tmp/d-mille/lib`, removed afterwards.
- **Each run:**
  1. capture a token;
  2. create n files in n different directories;
  3. wait 300 ms;
  4. `replay_since`, then `walk_scan(max_depth 1)` of each `dirs_to_rescan` directory;
  5. against that, a full media walk (p16) and `events_since` of a `write_snapshot_from` snapshot (flux's current `MilleResume` path).
- Every created file was found in every run.

Second run, 7 runs, medians, load 15–30:

| n changed | dirs rescanned | replay | + rescans = catch-up | full walk | `events_since` |
|---:|---:|---:|---:|---:|---:|
| 0 | 0 | 25 ms | **25 ms** | 81 ms | 279 ms |
| 1 | 1 | 22 ms | **22 ms** | 108 ms | 249 ms |
| 10 | 10 | 18 ms | **18 ms** | 104 ms | 253 ms |
| 100 | 100 | 22 ms | **29 ms** | 106 ms | 279 ms |
| 1,000 | 1,000 | 50 ms | 346 ms | **208 ms** | 707 ms |

- A first run of 5 runs at load 22–34 gave catch-up 41–50 ms for n ≤ 100, against 162–310 ms for the walk and 249–692 ms for `events_since`. At n = 1,000 it gave 251 ms against 111 ms.
- **The replay itself is bimodal: 18–40 ms or 150–200 ms.** Instrumented, all of it is the wait for fseventsd's `HistoryDone`; the stream's own work is microseconds.
- **The replay's cost follows the volume's activity since the token, not the tree.** Tokens moved back by k event ids, on this volume:

| k | replay | records under the root |
|---:|---:|---:|
| 10^4 | 20 ms | 0 |
| 10^5 | 23 ms | 60 |
| 10^6 | 470 ms | 44,996 (the clone itself) |
| 10^7 | 1.58 s | 44,996 |

- **Reading these.**
  - On a warm 21k-entry tree, catch-up is 4–6× faster than a walk for up to 100 changed directories, and 9–14× faster than `events_since`.
  - Past a few hundred changed directories, one walk wins. flux should fall back to a Quick scan above a threshold (the prototype uses 256).
  - The walk is cheap here only because the tree is cached. A replay costs O(changes) whatever the library size, device or cache state. A walk costs O(tree): 13–18 s for the 2M-entry project100 tree (`mille-changes.md`), and minutes on a NAS.

### Tests and gate

Tests:

- `mille-core`:
  - 307 unit tests, up from 293. The 14 new ones: 1 scan, 5 watcher, 3 resume and 5 history.
  - 26 integration tests and 1 doctest.
- `mille-bench`: 2.

Gate commands:

| Command | Result |
|---|---|
| `cargo build --workspace --all-targets --exclude mille-binding` | clean |
| `cargo build -p mille-binding` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warnings |
| `cargo clippy -p mille-core --target x86_64-unknown-linux-gnu --all-targets -- -D warnings` | clean |
| `cargo clippy -p mille-core --target x86_64-pc-windows-msvc --all-targets --features blake3/pure -- -D warnings` | clean |
| `cargo test --workspace --exclude mille-binding` | 450 passed, 0 failed (final run; see the timing flakes below) |
| `cargo fmt --all --check` | clean |

- **`mille-binding`.** Mille's own CLAUDE.md makes excluding it from the workspace test mandatory: its test target cannot link the NAPI symbols outside Node. The workspace build of all targets fails on that same link for the same reason, which is why it excludes the binding too.
- **Timing tests are flaky on this loaded machine, before and after this work.**
  - The `hydration_scaling` ratio tests failed at load average ~220, then passed in isolation. They test `EntryStore` hydration, which this work does not touch.
  - A `read_boundary` test with a 100 ms deadline failed in 1 of 5 runs.
  - The base commit `1a96400`, run the same way at the same time, failed 2 of 8 runs, on the watcher's FSEvents timing tests.
- **flux at `1fbcb63`** built against `wt/mille-d` (a scratch detached worktree whose root `Cargo.toml` path was edited, never committed):
  - `cargo test -p flux-scan`: 13 unit + 13 scan + 4 watch + 1 example, all green;
  - `cargo clippy -p flux-scan --all-targets`: 0 warnings;
  - flux's `Cargo.lock` is unchanged by the Mille changes.
- **Flakiness.** The new FSEvents-based tests poll for up to 5 s to absorb fseventsd's numbering latency. The live history test starts from a quiet point (commit `2c064e8`). After that change it passed 5 of 5 full lib runs at load 12–77.

Reproduce:

```bash
cd $FLUX_WORK/wt/mille-d
export CARGO_TARGET_DIR=$FLUX_WORK/target/d-mille
cargo test --workspace --exclude mille-binding
cargo build --release -p mille-bench --example flux_resume
B=$CARGO_TARGET_DIR/release/examples/flux_resume
$B snapshot $FLUX_WORK/media/library/TV <scratch-dir> 9
cp -cR $FLUX_WORK/media/library <scratch>/lib
$B catchup <scratch>/lib 7 0,1,10,100,1000
$B window <scratch>/lib 3
```

### How flux-scan should adopt these

A validated prototype of steps 1–3 is at `$FLUX_WORK/review/mille-phase-d-flux-adoption.patch`. It applies to `1fbcb63`. On that patch, flux-scan's tests stay green 3 times in a row, and a new test, `restart_catches_up_from_fsevents_history_alone`, passes 3 of 3: with the snapshot deleted, the catch-up rescans 20 of 62 media files and saves the next token.

1. **Coalescing** (`watch.rs`). In the watcher callback, replace `coalesce_events(&batch)` plus the raw-path hint workaround with `coalesce_events_with(&batch, CoalesceOptions::stateful())`. Update the module doc.
2. **Snapshot** (`resume.rs::save`). Drop the `EntryStore`/`populate_store`/`write_snapshot` path and call:

   ```rust
   mille_core::write_snapshot_from_with(
       &data.entries,
       &[RootStat { path: data.root.clone(), mtime_ms: data.root_mtime_ms }],
       &path,
       Durability::Unsynced,
   )
   ```

   - Build no root `WalkedEntry`.
   - A torn or stale snapshot already falls back to a Quick scan, because `read_snapshot(..).ok()?` gives `None`.
3. **History resume** (macOS):
   - **(a) Capture.** In `scan_root`, capture `HistoryToken::capture(&root_abs).ok()` before the walk when the scope is full and `want_resume` is set. Carry it in `ResumeData.history`.
   - **(b) Save after full scans.** In `resume::save`, which runs only after a complete full library scan, also write the token to `<db>.scan-resume-root-<id>.bin.history`.
   - **(c) Catch up.** In `resume::catch_up`, try each root's token first:
     - `replay_since(&token, &ReplayOptions::default())`;
     - on `Replay::Changes(c)` with empty `c.rescan_subtrees`, the dirs are the parents of the `c.changed` paths that `relevant()` keeps;
     - otherwise, or on any error, fall back to the snapshot and then to a Quick scan;
     - above 256 dirs, run a Quick scan.

     It returns `CatchUp { dirs, next: Vec<(token_path, ReplayReport::next)> }`.
   - **(d) Save after catch-up.** `watch::catch_up` requests `JobKind::Dirs(dirs)` at `Priority::Watcher`, waits with `job.done.wait()`, and only on `Ok` writes the `next` tokens (`resume::save_tokens`).
   - **(e) Documentation.** Update `StartupCatchUp::MilleResume`'s doc: history first on macOS, then the snapshot, then a Quick scan. Consider making it the default on macOS.
   - **(f) Not in the prototype: refresh the token while watching.** Without a refresh, a restart replays everything since the last full scan or catch-up; that is correct, but costs about 0.5 s per 10^6 volume event ids here. The protocol:
     - in `debounce_loop`, capture a token per root when a flush starts;
     - after that flush's `scan_dirs` job completes `Ok`, write the token captured at the *previous* flush (one batch behind), so a token is never ahead of an applied change;
     - rate-limit to one write a minute. `HistoryToken::write` fsyncs, which costs 65–540 ms on this SSD.
4. **INT-5.** No flux change is needed: with the order-independent decision, flux's zero-write invariant holds as is. Keep inserting link directories into `listed` at visit time: rows that an older Mille cataloged under an in-root alias are then tombstoned once, and stay so. `ScanReport::links_skipped` is there for logging, or for a caller that wants "listed" to mean "actually read".
5. **Debounced watcher.** flux may keep `watch_debounce = 0`, the raw stream. The documented reason for avoiding the debouncer is fixed; update `ScannerOptions::watch_debounce`'s doc.

### Known gaps

- **flux's `MilleResume` snapshot is stale by design.** It is written only after full scans, so changes the watcher applied later are missing from it. A file the watcher added, then deleted while flux was down, is in neither the snapshot nor the disk, so `events_since` reports nothing and the catalog keeps the file. The history token has no such failure: a stale token replays a superset. Prefer history on macOS, and treat the snapshot as the non-macOS fallback.
- **The live token refresh (3f) is not implemented.** It is described above, not prototyped.
- **fseventsd numbers events late under load.** This test run saw more than 800 ms of lag, and ids out of order across paths. It is safe: an event is never numbered at or below a token captured before it was numbered, so a late event only makes a later replay a superset. But it means "capture the token, then scan" sometimes replays a few events the scan already saw.
- **The replay's cost is volume-wide.** fseventsd reads its journal from the token on, so other activity on the volume adds to the cost: 1.6 s for 10^7 ids here. Refresh the token, and keep `timeout` and `max_events` as the backstop.
- **Some FSEvents behaviour is unverified.** That it delivers `RootChanged` or `MustScanSubDirs` *in history* has not been checked. Neither has whether a journal purge always changes the UUID; `max_age` (30 days) is the backstop. A root renamed away and back keeps its inode and is not detected.
- **The filter adds an `lstat` per event.** `FsEventsHistoryFilter` stats every Create and Remove event the debounced backend sees, on notify's thread.
- **Case-insensitive APFS edge.** Deleting `Foo` and creating `foo` in one window drops the `Remove(Foo)`, because `lstat("Foo")` finds `foo`. A consumer that re-lists the parent recovers.
- **The binding's JS (pnpm) suite was not run.** The binding's Rust is unchanged. Its macOS debounced watcher now delivers these deletions and replacements.
- **Smart links now wait for the rest of the walk.** They are walked after everything reachable without links. That changes the order, not the I/O. On a slow device, linked content arrives last.
- **Linux and Windows** are clippy-checked only. On both, `history` returns `Unsupported`.
