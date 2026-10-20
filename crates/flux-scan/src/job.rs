//! Scan jobs: what was asked (library or directories), its priority, its
//! cancellation flag, live progress counters and the completion slot that
//! every [`crate::ScanHandle`] clone waits on.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use flux_core::api::{Id, ScanState, ScanStatus};
use mille_core::CancelFlag;

use crate::{Priority, ScanError, ScanMode, ScanSummary};

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn prio_u8(p: Priority) -> u8 {
    match p {
        Priority::Deep => 0,
        Priority::Scheduled => 1,
        Priority::Watcher => 2,
        Priority::Interactive => 3,
    }
}

/// What a job scans.
#[derive(Debug, Clone)]
pub(crate) enum JobKind {
    Library(ScanMode),
    /// Absolute directory paths, rescanned one level deep (plus any
    /// directory below them the catalog has never seen).
    Dirs(Vec<PathBuf>),
}

/// A cloneable stand-in for [`ScanError`] (several handles may wait on one
/// job; `CatalogError` is not `Clone`).
#[derive(Debug, Clone)]
pub(crate) enum ErrRepr {
    UnknownLibrary(Id),
    Cancelled,
    ShuttingDown,
    Catalog(String),
    Other(String),
}

impl ErrRepr {
    pub(crate) fn to_error(&self) -> ScanError {
        match self {
            ErrRepr::UnknownLibrary(id) => ScanError::UnknownLibrary(*id),
            ErrRepr::Cancelled => ScanError::Cancelled,
            ErrRepr::ShuttingDown => ScanError::ShuttingDown,
            ErrRepr::Catalog(m) => ScanError::Other(format!("catalog: {m}")),
            ErrRepr::Other(m) => ScanError::Other(m.clone()),
        }
    }
}

impl From<flux_catalog::CatalogError> for ErrRepr {
    fn from(e: flux_catalog::CatalogError) -> Self {
        ErrRepr::Catalog(e.to_string())
    }
}

pub(crate) type JobResult = Result<ScanSummary, ErrRepr>;

/// Where finished results land.
pub(crate) struct Completion {
    slot: Mutex<Option<JobResult>>,
    cv: Condvar,
    tx: tokio::sync::watch::Sender<bool>,
}

impl Completion {
    fn new() -> Self {
        Completion {
            slot: Mutex::new(None),
            cv: Condvar::new(),
            tx: tokio::sync::watch::channel(false).0,
        }
    }

    pub(crate) fn set(&self, r: JobResult) {
        let mut s = lock(&self.slot);
        if s.is_none() {
            *s = Some(r);
        }
        drop(s);
        self.cv.notify_all();
        self.tx.send_replace(true);
    }

    pub(crate) fn wait(&self) -> JobResult {
        let mut s = lock(&self.slot);
        loop {
            if let Some(r) = s.as_ref() {
                return r.clone();
            }
            s = self.cv.wait(s).unwrap_or_else(|e| e.into_inner());
        }
    }

    pub(crate) async fn finished(&self) -> JobResult {
        let mut rx = self.tx.subscribe();
        // `wait_for` checks the current value first, so a finished job
        // returns at once.
        let _ = rx.wait_for(|done| *done).await;
        lock(&self.slot).clone().unwrap_or(Err(ErrRepr::ShuttingDown))
    }

    pub(crate) fn is_done(&self) -> bool {
        lock(&self.slot).is_some()
    }
}

/// Live counters. Everything a `ScanSummary` and a `ScanStatus` need.
#[derive(Default)]
pub(crate) struct Progress {
    pub files_seen: AtomicU64,
    pub media_files: AtomicU64,
    /// Media files whose fate is decided (unchanged, or committed).
    pub files_done: AtomicU64,
    /// `u64::MAX` = not known yet (walk still running).
    pub files_total: AtomicU64,
    pub files_probed: AtomicU64,
    pub unchanged: AtomicU64,
    pub added: AtomicU64,
    pub changed: AtomicU64,
    pub moved: AtomicU64,
    pub missing: AtomicU64,
    pub reattached: AtomicU64,
    pub probe_failures: AtomicU64,
    pub ffprobe_fallbacks: AtomicU64,
    pub items_added: AtomicU64,
    pub items_updated: AtomicU64,
    pub items_removed: AtomicU64,
    pub opened: AtomicU64,
    pub writer_commands: AtomicU64,
    pub non_utf8: AtomicU64,
    pub walk_ns: AtomicU64,
    pub diff_ns: AtomicU64,
    pub process_ns: AtomicU64,
    pub apply_ns: AtomicU64,
    pub fingerprint_ns: AtomicU64,
    pub probe_ns: AtomicU64,
    pub parse_ns: AtomicU64,
    pub cross_root_ns: AtomicU64,
    pub read_calls: AtomicU64,
    pub read_bytes: AtomicU64,
    pub links_external: AtomicU64,
    pub links_skipped: AtomicU64,
    pub external_not_media: AtomicU64,
    pub fingerprints_deferred: AtomicU64,
    pub probe_retries: AtomicU64,
    pub follow_up_passes: AtomicU64,
    pub started_at: AtomicI64,
    pub dirs_unavailable: AtomicU32,
    pub unread: Mutex<Vec<String>>,
    pub unavailable_roots: Mutex<Vec<String>>,
    pub last_error: Mutex<Option<String>>,
}

impl Progress {
    pub(crate) fn add(c: &AtomicU64, n: u64) {
        c.fetch_add(n, Ordering::Relaxed);
    }

    fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }

    pub(crate) fn summary(&self, library_id: Id, total_ms: u64) -> ScanSummary {
        let g = Self::get;
        ScanSummary {
            library_id,
            files_seen: g(&self.files_seen),
            media_files: g(&self.media_files),
            unchanged: g(&self.unchanged),
            added: g(&self.added),
            changed: g(&self.changed),
            moved: g(&self.moved),
            missing: g(&self.missing),
            reattached: g(&self.reattached),
            probe_failures: g(&self.probe_failures),
            ffprobe_fallbacks: g(&self.ffprobe_fallbacks),
            non_utf8_skipped: g(&self.non_utf8),
            unread_dirs: lock(&self.unread).clone(),
            unavailable_roots: lock(&self.unavailable_roots).clone(),
            items_added: g(&self.items_added),
            items_updated: g(&self.items_updated),
            items_removed: g(&self.items_removed),
            media_files_opened: g(&self.opened),
            writer_commands: g(&self.writer_commands),
            walk_ms: g(&self.walk_ns) / 1_000_000,
            diff_ms: g(&self.diff_ns) / 1_000_000,
            process_ms: g(&self.process_ns) / 1_000_000,
            apply_ms: g(&self.apply_ns) / 1_000_000,
            total_ms,
            stages: crate::StageTimes {
                fingerprint_ms: g(&self.fingerprint_ns) as f64 / 1e6,
                probe_ms: g(&self.probe_ns) as f64 / 1e6,
                parse_ms: g(&self.parse_ns) as f64 / 1e6,
                cross_root_ms: g(&self.cross_root_ns) as f64 / 1e6,
                read_calls: g(&self.read_calls),
                read_bytes: g(&self.read_bytes),
            },
            links_external: g(&self.links_external),
            links_skipped: g(&self.links_skipped),
            external_not_media: g(&self.external_not_media),
            fingerprints_deferred: g(&self.fingerprints_deferred),
            probe_retries: g(&self.probe_retries),
            follow_up_passes: g(&self.follow_up_passes),
        }
    }

    pub(crate) fn status(&self, state: ScanState) -> ScanStatus {
        let g = Self::get;
        let total = g(&self.files_total);
        let started = self.started_at.load(Ordering::Relaxed);
        ScanStatus {
            state,
            files_seen: g(&self.files_done),
            files_total: (total != u64::MAX).then_some(total),
            files_probed: g(&self.files_probed),
            items_added: g(&self.items_added),
            items_updated: g(&self.items_updated),
            items_removed: g(&self.items_removed),
            started_at: (started != 0).then_some(started),
            finished_at: None,
            last_error: lock(&self.last_error).clone(),
            dirs_unavailable: self.dirs_unavailable.load(Ordering::Relaxed),
        }
    }
}

struct KindState {
    kind: JobKind,
    sealed: bool,
}

/// One scan. Shared by its thread, its handles and the registry.
pub(crate) struct JobShared {
    pub id: u64,
    pub library_id: Id,
    pub prio: Arc<AtomicU8>,
    kind: Mutex<KindState>,
    pub cancel: CancelFlag,
    cancelled: AtomicBool,
    /// A request joined after the scan started: run one more pass.
    rescan: AtomicBool,
    /// The job put the catalog writer in bulk mode (ended when it finishes).
    pub bulk: AtomicBool,
    pub progress: Progress,
    pub done: Completion,
}

impl JobShared {
    pub(crate) fn new(id: u64, library_id: Id, kind: JobKind, prio: Priority) -> Arc<JobShared> {
        let p = Progress {
            files_total: AtomicU64::new(u64::MAX),
            ..Default::default()
        };
        Arc::new(JobShared {
            id,
            library_id,
            prio: Arc::new(AtomicU8::new(prio_u8(prio))),
            kind: Mutex::new(KindState { kind, sealed: false }),
            cancel: CancelFlag::new(),
            cancelled: AtomicBool::new(false),
            rescan: AtomicBool::new(false),
            bulk: AtomicBool::new(false),
            progress: p,
            done: Completion::new(),
        })
    }

    pub(crate) fn prio(&self) -> u8 {
        self.prio.load(Ordering::Relaxed)
    }

    pub(crate) fn raise(&self, p: Priority) {
        self.prio.fetch_max(prio_u8(p), Ordering::Relaxed);
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.cancel.cancel();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub(crate) fn is_library(&self) -> bool {
        matches!(lock(&self.kind).kind, JobKind::Library(_))
    }

    pub(crate) fn is_sealed(&self) -> bool {
        lock(&self.kind).sealed
    }

    /// Freeze the request (no more merging); returns what to scan.
    pub(crate) fn seal(&self) -> JobKind {
        let mut k = lock(&self.kind);
        k.sealed = true;
        k.kind.clone()
    }

    /// The current request (it may still grow until sealed).
    pub(crate) fn kind(&self) -> JobKind {
        lock(&self.kind).kind.clone()
    }

    /// Join a library request: unsealed jobs upgrade to `Deep` if asked; a
    /// running `Quick` scan can't become `Deep` (`false`). Joining a running
    /// scan queues a follow-up pass: it may have listed the directory the
    /// request is about before the change happened.
    pub(crate) fn join_library(&self, mode: ScanMode) -> bool {
        let mut k = lock(&self.kind);
        let sealed = k.sealed;
        match &mut k.kind {
            JobKind::Library(m) if !sealed => {
                if mode == ScanMode::Deep {
                    *m = ScanMode::Deep;
                }
                true
            }
            JobKind::Library(m) => {
                let joins = mode == ScanMode::Quick || *m == ScanMode::Deep;
                if joins {
                    self.rescan.store(true, Ordering::Relaxed);
                }
                joins
            }
            JobKind::Dirs(_) => false,
        }
    }

    /// Take the follow-up request (see [`JobShared::join_library`]).
    pub(crate) fn take_rescan(&self) -> bool {
        self.rescan.swap(false, Ordering::Relaxed)
    }

    /// Merge directories into an unsealed directory job (or let an unsealed
    /// library scan cover them).
    pub(crate) fn join_dirs(&self, dirs: &[PathBuf]) -> bool {
        let mut k = lock(&self.kind);
        if k.sealed {
            return false;
        }
        match &mut k.kind {
            JobKind::Library(_) => true,
            JobKind::Dirs(have) => {
                for d in dirs {
                    if !have.contains(d) {
                        have.push(d.clone());
                    }
                }
                true
            }
        }
    }
}
