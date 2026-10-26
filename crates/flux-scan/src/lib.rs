//! Library scan orchestrator: Mille walk → diff vs catalog → fingerprint →
//! probe + parse → batched catalog writes, plus the change watcher.
//!
//! This file is the **contract** that `flux-server` codes against. The
//! implementation may add items, but must keep these signatures and semantics
//! (or change them together with flux-server).
//!
//! ```text
//! scan_library / scan_dirs ─► per-device admission (priority queue)
//!   ─► Mille walk_scan (getattrlistbulk / statx), ReadBoundary per device
//!   ─► diff vs catalog snapshot on (dev, ino, size, mtime): unchanged → nothing
//!   ─► new/changed: flux-probe → flux-names (siblings); fp128 up front only
//!      for a possible move, else by the background pass after the commit
//!   ─► catalog Writer::apply (≤ batch_rows or ≤ batch_interval per transaction)
//!   ─► Event::ScanProgress / ItemsChanged / SourceAvailability on `subscribe()`
//! ```
//!
//! Invariants (docs/ARCHITECTURE.md):
//! - A zero-change rescan opens zero media files and sends zero writer commands.
//! - Items become browsable batch by batch, not at the end of the scan.
//! - An unreachable root or an unread directory never deletes anything: the
//!   root is marked unavailable / files under unread dirs keep their state.
//!   The same holds below the root: a link whose target is offline, or an
//!   unmounted volume's empty mount point, is unread.
//! - Symlinks follow one deterministic policy (docs/prototype/scan.md):
//!   aliases inside the library's roots are never followed.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use flux_catalog::Catalog;
use flux_core::api::{Event, Id, ScanStatus};
use serde::Serialize;

mod device;
mod engine;
mod ffprobe;
pub mod fingerprint;
mod fp_later;
mod job;
mod paths;
mod pool;
mod process;
mod resume;
mod scan_root;
mod watch;

pub use device::{classify as classify_fstype, fs_type, DeviceClass, FsInfo};

use job::{JobKind, JobShared};

/// Tuning knobs. `Default` is right for a desktop with an SSD.
#[derive(Debug, Clone)]
pub struct ScanConfig {
    /// Threads that fingerprint, probe and parse new or changed files.
    pub workers: usize,
    /// In-flight directory reads per device on SSD/NVMe.
    pub ssd_concurrency: usize,
    /// In-flight directory reads per device on HDDs and network volumes.
    pub slow_concurrency: usize,
    /// A catalog transaction closes after this many files…
    pub batch_rows: usize,
    /// …or after this long, whichever comes first.
    pub batch_interval: Duration,
    /// Compute fp128 (xxh3-128 over size + first and last 64 KiB) for move and
    /// re-attach detection.
    pub fingerprint: bool,
    /// Fingerprint new and changed files in a background pass after they
    /// are committed (and browsable), instead of before: the 64 KiB tail
    /// read is most of a cold scan's I/O. A file that may be a move inside
    /// the root (its size matches a fingerprinted file of the root) is
    /// still fingerprinted up front, so the move is found by the same scan;
    /// any other match with a tombstone is merged when the background pass
    /// reaches it (`Writer::attach_fingerprints`), keeping the tombstone's
    /// identity and watch state. Ignored when `fingerprint` is off.
    pub fingerprint_later: bool,
    /// Follow symbolic links whose target is outside every root of the
    /// library: a folder link to another disk, a debrid/rclone mount, a
    /// file link into one. Their content is cataloged under the link's
    /// path, and a *new* file reached that way is added only if it probes
    /// as media. Links that stay inside the library's roots are never
    /// followed (the real path is cataloged). See docs/prototype/scan.md
    /// "Symlinks".
    pub follow_external_links: bool,
    /// Run ffprobe for files flux-probe can't read (`UnknownFormat`) or
    /// finds malformed (`ProbeError::wants_ffprobe`).
    pub ffprobe_fallback: bool,
    /// The ffprobe binary for that fallback. The server passes the one it
    /// resolved (`--ffprobe`, its config, Homebrew); the default is
    /// `$FLUX_FFPROBE`, else `ffprobe` on `PATH`.
    pub ffprobe: PathBuf,
    pub ffprobe_timeout: Duration,
    /// Concurrent ffprobe processes at most.
    pub ffprobe_workers: usize,
    /// Directory read deadlines: first answer (lets a sleeping disk spin up)
    /// and stall between answers (Mille `ReadBoundaryOptions::media()`).
    pub first_response: Duration,
    pub stall: Duration,
}

impl Default for ScanConfig {
    fn default() -> Self {
        ScanConfig {
            workers: std::thread::available_parallelism()
                .map(|n| n.get().min(8))
                .unwrap_or(4),
            ssd_concurrency: 16,
            slow_concurrency: 4,
            batch_rows: 500,
            batch_interval: Duration::from_millis(50),
            fingerprint: true,
            fingerprint_later: true,
            follow_external_links: true,
            ffprobe_fallback: true,
            ffprobe: PathBuf::from(flux_probe::ffprobe::ffprobe_bin()),
            ffprobe_timeout: Duration::from_secs(20),
            ffprobe_workers: 2,
            first_response: Duration::from_secs(30),
            stall: Duration::from_secs(2),
        }
    }
}

/// Who asked. Higher runs first when work competes for a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Background re-probe after a probe/parser version bump.
    Deep,
    /// Startup and periodic scans.
    Scheduled,
    /// Filesystem change events.
    Watcher,
    /// A user is waiting (play pressed on a file that changed, "Scan now").
    Interactive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanMode {
    /// Stat diff; only new or changed files are opened. Files whose probe
    /// failed are retried once their backoff has passed (at once for an
    /// `Interactive` request).
    Quick,
    /// A Quick scan that also brings every file up to the current
    /// versions: files probed by an older flux-probe (or whose probe
    /// failed) are re-probed; files parsed by an older flux-names are
    /// re-parsed (no I/O). Up-to-date files are left alone.
    Deep,
}

/// What one finished scan did. Also written to
/// `bench/results/flux-scan-*.json` by the bench binary.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ScanSummary {
    pub library_id: Id,
    /// Files the walk delivered (video and sidecar extensions, after the
    /// hidden / NAS-litter / `.plexignore` filters).
    pub files_seen: u64,
    /// Video files among them (what the catalog tracks).
    pub media_files: u64,
    /// Same `(dev, ino, size, mtime)` as the catalog: not opened, not written.
    pub unchanged: u64,
    /// Catalog outcome counts (`ApplyOutcome`): new rows, rows updated in
    /// place (re-probed, stat refresh, revived tombstones), moves, tombstones.
    pub added: u64,
    pub changed: u64,
    pub moved: u64,
    pub missing: u64,
    /// New paths the catalog re-attached to a tombstoned file by `fp128`
    /// (moved or copied across roots or volumes; asset and watch state kept).
    pub reattached: u64,
    pub probe_failures: u64,
    pub ffprobe_fallbacks: u64,
    /// Names that are not valid UTF-8, skipped (with a warning) rather than
    /// stored lossily (two such names could alias one catalog path).
    pub non_utf8_skipped: u64,
    /// Media files skipped because their name starts with `.` (each is
    /// logged at info). Hidden files are never cataloged; Windows doesn't
    /// hide such a name, so this tells a user where a file went. OS litter
    /// (`.DS_Store`, AppleDouble `._*`) is not counted.
    pub hidden_skipped: u64,
    /// Directories that didn't answer before their deadline (nothing under
    /// them was marked missing). Also: a symlink whose target does not
    /// resolve (an offline disk), and an empty directory whose files lived
    /// on another device (an unmounted volume below the root).
    pub unread_dirs: Vec<String>,
    /// Roots that could not be reached at all (marked unavailable).
    pub unavailable_roots: Vec<String>,
    pub items_added: u64,
    pub items_updated: u64,
    pub items_removed: u64,
    /// Media files opened (fingerprint/probe) plus ffprobe runs.
    pub media_files_opened: u64,
    /// Catalog writer requests this scan sent (each `apply`, availability
    /// change and scan record counts once).
    pub writer_commands: u64,
    /// Wall time inside `walk_scan` (includes waiting on back-pressure when
    /// workers are the bottleneck).
    pub walk_ms: u64,
    /// Snapshot load + post-walk reconciliation.
    pub diff_ms: u64,
    /// First to last worker activity.
    pub process_ms: u64,
    /// Time blocked on catalog commits.
    pub apply_ms: u64,
    /// Job wall time, including waiting for device admission.
    pub total_ms: u64,
    /// Where worker time went (summed over workers, so it can exceed the
    /// wall time).
    pub stages: StageTimes,
    /// Symlinks leaving the library's roots that were followed: directory
    /// links walked, and file links whose target is outside every root.
    pub links_external: u64,
    /// Symlinks not followed: an alias of something inside the library's
    /// roots (the real path is cataloged instead), a second link to a
    /// folder already walked, a link to a folder that contains a root, or
    /// any link leaving the roots with `follow_external_links` off.
    pub links_skipped: u64,
    /// New files reached through a link that leaves the library's roots
    /// that did not probe as media, and so were not cataloged (a
    /// `Leak (2021).mkv -> ~/.ssh/id_ed25519` link never becomes an item).
    pub external_not_media: u64,
    /// Files with a video extension that are not media at all (text: a
    /// TypeScript `.ts`). Cataloged as such, so they never become items
    /// and no later scan opens them again until they change.
    pub not_media: u64,
    /// Files left to the background fingerprint pass
    /// ([`ScanConfig::fingerprint_later`]); see [`Scanner::wait_fingerprints`].
    pub fingerprints_deferred: u64,
    /// Failed probes retried by this scan (backoff passed, an Interactive
    /// request, or a Deep scan).
    pub probe_retries: u64,
    /// Follow-up passes run because a request joined this scan after it had
    /// started (each re-walks the library once more, so nothing the request
    /// was about is missed).
    pub follow_up_passes: u64,
}

/// Worker time per stage, summed over all workers.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct StageTimes {
    /// Open + fp128 (head and tail reads).
    pub fingerprint_ms: f64,
    /// flux-probe through the same page cache (often no new read).
    pub probe_ms: f64,
    pub parse_ms: f64,
    /// Cross-root move lookups (one indexed query per new file).
    pub cross_root_ms: f64,
    /// Real `pread` calls and bytes for fingerprint + probe.
    pub read_calls: u64,
    pub read_bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("library {0} not found")]
    UnknownLibrary(Id),
    #[error("scan cancelled")]
    Cancelled,
    #[error("scanner is shutting down")]
    ShuttingDown,
    #[error("catalog: {0}")]
    Catalog(#[from] flux_catalog::CatalogError),
    #[error("{0}")]
    Other(String),
}

/// How `watch()` catches up with changes made while flux wasn't watching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupCatchUp {
    /// A Quick scan of the library (walk + stat diff; opens only changed
    /// files).
    QuickScan,
    /// Only the parents of paths changed while flux was down are rescanned,
    /// found from, in order:
    /// 1. on macOS, the volume's FSEvents history since the token saved
    ///    with the last complete full scan or catch-up (Mille's
    ///    `replay_since`; O(changes), 18–29 ms for up to 100 changed
    ///    folders of the 16.7k-file library);
    /// 2. Mille's resume snapshot + `events_since` (re-walks the root, so no
    ///    faster than a Quick scan);
    /// 3. a Quick scan, also when more than 256 folders changed or the
    ///    history can't answer (another volume, a lost journal, too old).
    ///
    /// Writes the snapshot (unsynced) and the token after each complete
    /// full scan, and the next token once a catch-up is applied. Not the
    /// default yet: tokens are not refreshed while watching, so a restart
    /// after a long run replays the volume's whole history since the last
    /// start (about 0.5 s per million volume events; capped, then a Quick
    /// scan).
    MilleResume,
    /// Nothing (the caller schedules scans itself).
    Nothing,
}

/// Scanner options beyond [`ScanConfig`] (kept separate so `ScanConfig`
/// literals in callers stay valid).
#[derive(Clone)]
pub struct ScannerOptions {
    /// Mille's watcher debounce window (notify-debouncer-full); zero (the
    /// default) uses notify's raw event stream and only flux's own settle
    /// window below. The debounced backend used to drop, on macOS, the
    /// deletion of a file whose FSEvents history also says "created"; Mille
    /// fixed that on `flux/dogfood` (Phase D), so either works. Events are
    /// coalesced statefully either way: a path created and deleted in one
    /// window is reported, never dropped (`CoalesceOptions::stateful`).
    pub watch_debounce: Duration,
    /// After the last event of a burst, wait this long before rescanning
    /// (bursts are capped at 4× this, minimum 1 s).
    pub watch_settle: Duration,
    pub startup: StartupCatchUp,
    /// How often a watched library's unavailable roots are tried again (a
    /// bounded stat of those roots only; a Quick scan once one answers). A
    /// root renamed and renamed back, a drive plugged in again or a NAS
    /// that came back then returns by itself instead of waiting for "Scan
    /// now". A root that answers but still can't be scanned (an empty mount
    /// point) is tried at doubling intervals, up to an hour. Zero turns it
    /// off.
    pub recheck_unavailable: Duration,
    /// Test seam: a Mille read hook installed on every device's
    /// `ReadBoundary` (e.g. to park a directory past its deadline).
    #[doc(hidden)]
    pub read_hook: Option<Arc<dyn mille_core::ReadHook>>,
}

impl Default for ScannerOptions {
    fn default() -> Self {
        ScannerOptions {
            watch_debounce: Duration::ZERO,
            watch_settle: Duration::from_millis(150),
            // Measured (docs/prototype/scan.md): `events_since` re-walks the
            // whole root, so it is no faster than a Quick scan, and keeping
            // its snapshot costs a write after every full scan.
            startup: StartupCatchUp::QuickScan,
            recheck_unavailable: Duration::from_secs(30),
            read_hook: None,
        }
    }
}

impl std::fmt::Debug for ScannerOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScannerOptions")
            .field("watch_debounce", &self.watch_debounce)
            .field("watch_settle", &self.watch_settle)
            .field("startup", &self.startup)
            .field("recheck_unavailable", &self.recheck_unavailable)
            .field("read_hook", &self.read_hook.is_some())
            .finish()
    }
}

/// The scan orchestrator. Cheap to clone; all clones share one set of workers.
#[derive(Clone)]
pub struct Scanner {
    inner: Arc<engine::Inner>,
}

/// A hold on a library's watcher ([`Scanner::hold_watch`]); released when
/// dropped.
pub struct WatchHold {
    inner: std::sync::Weak<engine::Inner>,
    library_id: Id,
}

impl Drop for WatchHold {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            let mut holds = job::lock(&inner.holds);
            if let Some(n) = holds.get_mut(&self.library_id) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    holds.remove(&self.library_id);
                }
            }
        }
    }
}

impl Scanner {
    /// Starts the worker pool. Must be called inside a Tokio runtime only if
    /// the implementation needs one; callers may call it from sync code.
    /// (This implementation uses std threads only; no runtime is needed.)
    pub fn new(catalog: Arc<Catalog>, cfg: ScanConfig) -> Scanner {
        Scanner::with_options(catalog, cfg, ScannerOptions::default())
    }

    /// [`Scanner::new`] with [`ScannerOptions`].
    pub fn with_options(catalog: Arc<Catalog>, cfg: ScanConfig, opts: ScannerOptions) -> Scanner {
        Scanner {
            inner: engine::Inner::new(catalog, cfg, opts),
        }
    }

    /// Scan progress, item changes and source availability, for the SSE
    /// endpoint. Lagging receivers lose old events (broadcast semantics).
    /// Hold the watcher of `library_id` until the hold is dropped: what it
    /// sees is kept, and the rescan it asks for waits. For flux moving files
    /// itself (a file plan): the catalog follows the files first, so the
    /// rescan finds nothing to do instead of files gone and new ones. (On
    /// Windows the scan can't follow a file by its inode.)
    pub fn hold_watch(&self, library_id: Id) -> WatchHold {
        *job::lock(&self.inner.holds).entry(library_id).or_insert(0) += 1;
        WatchHold {
            inner: Arc::downgrade(&self.inner),
            library_id,
        }
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// The library's current scan status (`Idle` with zero counts if it was
    /// never scanned by this process). `files_total` is `None` until the
    /// walk has finished and then the number of files the scan will look at;
    /// `dirs_unavailable` is the length of the last scan's
    /// [`ScanSummary::unread_dirs`].
    ///
    /// While scanning, `files_seen` counts media files whose fate is
    /// decided (unchanged, or committed to the catalog), so
    /// `files_seen / files_total` is the progress fraction.
    pub fn status(&self, library_id: Id) -> ScanStatus {
        self.inner.status(library_id)
    }

    /// Starts a scan of every root of the library and returns at once. A
    /// request for a library that is already scanning joins that scan (the
    /// returned handle refers to it); its priority is raised if higher.
    /// When the scan has already started, joining also queues one follow-up
    /// Quick pass of the library after the current one (later joins share
    /// it), so the handle completes only after a pass that began after the
    /// request: "Scan now" or a watcher overflow never loses a change in a
    /// directory the running pass had already listed.
    ///
    /// A `Deep` request joins a queued scan (upgrading it) but not a running
    /// `Quick` one, which can't re-probe what it already passed: it queues
    /// a new scan instead.
    pub fn scan_library(&self, library_id: Id, mode: ScanMode, priority: Priority) -> ScanHandle {
        let job = self.inner.request(library_id, JobKind::Library(mode), priority);
        ScanHandle { id: job.id, job }
    }

    /// Rescans the given directories one level deep (watcher batches,
    /// play-triggered checks). Directories outside the library's roots are
    /// ignored.
    ///
    /// "One level deep" plus every subdirectory the catalog has never seen
    /// (a new or renamed folder is walked whole). Files under a listed
    /// directory that are gone are tombstoned; files under subdirectories
    /// that still exist are left alone. Requests merge into a directory or
    /// library scan of the same library that has not started yet.
    pub fn scan_dirs(&self, library_id: Id, dirs: Vec<PathBuf>, priority: Priority) -> ScanHandle {
        let job = self.inner.request(library_id, JobKind::Dirs(dirs), priority);
        ScanHandle { id: job.id, job }
    }

    /// Watches every root of the library and feeds coalesced change events
    /// into [`Scanner::scan_dirs`] with [`Priority::Watcher`]. Idempotent.
    ///
    /// The first call per library also catches up with changes made while
    /// nothing was watching ([`ScannerOptions::startup`]).
    pub fn watch(&self, library_id: Id) -> Result<(), ScanError> {
        watch::watch(&self.inner, library_id, true)
    }

    /// [`Scanner::watch`] without the startup catch-up, for a caller that
    /// starts a full scan of the library right after: started once the
    /// watcher is up, that scan sees whatever changed while nothing
    /// watched, and changes after that are the watcher's. The catch-up
    /// would only join the running scan as a second, redundant pass whose
    /// counts add to its summary (a library of 28 files reporting 56).
    pub fn watch_without_catch_up(&self, library_id: Id) -> Result<(), ScanError> {
        watch::watch(&self.inner, library_id, false)
    }

    pub fn unwatch(&self, library_id: Id) {
        watch::unwatch(&self.inner, library_id);
    }

    /// Cancels running scans and stops watchers; waits up to `timeout` for
    /// in-flight catalog batches to commit.
    pub fn shutdown(&self, timeout: Duration) {
        self.inner.shutdown(timeout);
    }

    /// The storage devices scanned so far and how each was classified.
    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.inner.devices_info()
    }

    /// Wait until the background fingerprint pass
    /// ([`ScanConfig::fingerprint_later`]) has nothing queued or in flight,
    /// at most `timeout`. Returns whether it is idle.
    pub fn wait_fingerprints(&self, timeout: Duration) -> bool {
        self.inner.fingerprints.wait_idle(timeout)
    }

    /// When each root of the library last answered a scan in this process
    /// (Unix ms), by root path. A scan that changes nothing writes nothing
    /// (not even its scan record), so the catalog's `RootStatus.last_seen`
    /// lags on an untouched library; this is the live value.
    pub fn root_last_seen(&self, library_id: Id) -> Vec<(String, i64)> {
        self.inner.root_last_seen(library_id)
    }

    /// Overlay [`Scanner::root_last_seen`] on a catalog `Library` (for
    /// `GET /libraries`): each root's `last_seen` becomes the later of the
    /// two.
    pub fn overlay_roots(&self, lib: &mut flux_core::api::Library) {
        let seen = self.root_last_seen(lib.id);
        for r in &mut lib.roots {
            if let Some((_, t)) = seen.iter().find(|(p, _)| *p == r.path) {
                r.last_seen = Some(r.last_seen.map_or(*t, |c| c.max(*t)));
            }
        }
    }

    /// The directories a [`StartupCatchUp::MilleResume`] catch-up would
    /// rescan (`None`: no usable snapshot, a Quick scan would run). For the
    /// bench; `watch()` does this itself.
    #[doc(hidden)]
    pub fn resume_dirs(&self, library_id: Id) -> Option<Vec<PathBuf>> {
        resume::catch_up(&self.inner, library_id).map(|c| c.dirs)
    }
}

/// A device as the scanner classified it (`statfs`; see [`DeviceClass`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceInfo {
    pub dev: u64,
    pub class: DeviceClass,
    pub fstype: String,
    /// Directory reads (and file reads) in flight on this device.
    pub concurrency: usize,
}

/// A running (or finished) scan. Clones refer to the same scan.
#[derive(Clone)]
pub struct ScanHandle {
    id: u64,
    job: Arc<JobShared>,
}

impl ScanHandle {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Requests cancellation; batches already committed stay committed.
    pub fn cancel(&self) {
        self.job.cancel();
    }

    /// Blocks until the scan finishes.
    pub fn wait(&self) -> Result<ScanSummary, ScanError> {
        self.job.done.wait().map_err(|e| e.to_error())
    }

    /// Async version of [`ScanHandle::wait`].
    pub async fn finished(&self) -> Result<ScanSummary, ScanError> {
        self.job.done.finished().await.map_err(|e| e.to_error())
    }

    /// The scan has finished (or failed, or was cancelled).
    pub fn is_finished(&self) -> bool {
        self.job.done.is_done()
    }
}

impl std::fmt::Debug for ScanHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanHandle").field("id", &self.id).finish()
    }
}
