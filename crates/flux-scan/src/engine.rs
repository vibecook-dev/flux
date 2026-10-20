//! The scanner's shared state: job registry (coalescing), the job threads,
//! per-library status, events, and root reachability.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use flux_catalog::{scan_support, Catalog, RootInfo, RootState};
use flux_core::api::{Event, Id, ScanState, ScanStatus};
use mille_core::{ReadBoundary, ReadBoundaryOptions};
use tokio::sync::broadcast;

use crate::device::Devices;
use crate::job::{lock, ErrRepr, JobKind, JobResult, JobShared, Progress};
use crate::pool::WorkerPool;
use crate::process::FfprobePool;
use crate::scan_root::{scan_root, RootResult, Scope};
use crate::{paths, Priority, ScanConfig, ScannerOptions};

#[derive(Default)]
struct Registry {
    active: HashMap<Id, Vec<Arc<JobShared>>>,
    /// Status of the last finished scan per library.
    last: HashMap<Id, ScanStatus>,
    threads: Vec<JoinHandle<()>>,
}

pub(crate) struct Inner {
    pub catalog: Arc<Catalog>,
    pub cfg: ScanConfig,
    pub opts: ScannerOptions,
    pub events: broadcast::Sender<Event>,
    pub pool: WorkerPool,
    pub ffprobe: FfprobePool,
    pub devices: Devices,
    /// Bounded stats of roots (reachability, `st_dev`) before admission.
    pub root_stat: ReadBoundary,
    reg: Mutex<Registry>,
    pub shutting_down: AtomicBool,
    next_job: AtomicU64,
    pub watches: Mutex<HashMap<Id, crate::watch::LibraryWatch>>,
    /// The background fingerprint pass ([`crate::ScanConfig::fingerprint_later`]).
    pub fingerprints: crate::fp_later::Fingerprinter,
    /// When each root last answered a scan: root id → (library, path, ms).
    roots_seen: Mutex<HashMap<Id, (Id, String, i64)>>,
    /// New files behind links leaving the roots that did not probe as
    /// media: `(root, rel_path, size, mtime_ns)`. Not opened again while
    /// unchanged (they are never cataloged, so the catalog can't remember).
    pub rejected: Mutex<HashSet<(Id, String, u64, i64)>>,
}

impl Inner {
    pub(crate) fn new(catalog: Arc<Catalog>, cfg: ScanConfig, opts: ScannerOptions) -> Arc<Inner> {
        let (events, _) = broadcast::channel(1024);
        let pool = WorkerPool::new(cfg.workers);
        let ffprobe = FfprobePool::new(cfg.ffprobe_workers);
        let root_stat = ReadBoundary::new(ReadBoundaryOptions {
            timeout: cfg.stall,
            first_response: Some(cfg.first_response),
            concurrency: 4,
            hook: opts.read_hook.clone(),
            ..ReadBoundaryOptions::media()
        });
        Arc::new(Inner {
            catalog,
            cfg,
            opts,
            events,
            pool,
            ffprobe,
            devices: Devices::default(),
            root_stat,
            reg: Mutex::new(Registry::default()),
            shutting_down: AtomicBool::new(false),
            next_job: AtomicU64::new(1),
            watches: Mutex::new(HashMap::new()),
            fingerprints: crate::fp_later::Fingerprinter::default(),
            roots_seen: Mutex::new(HashMap::new()),
            rejected: Mutex::new(HashSet::new()),
        })
    }

    pub(crate) fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    // ---- requests ------------------------------------------------------------

    pub(crate) fn request(
        self: &Arc<Self>,
        library_id: Id,
        kind: JobKind,
        priority: Priority,
    ) -> Arc<JobShared> {
        let mut reg = lock(&self.reg);
        if self.shutting_down.load(Ordering::Relaxed) {
            let j = JobShared::new(0, library_id, kind, priority);
            j.done.set(Err(ErrRepr::ShuttingDown));
            return j;
        }
        let active = reg.active.entry(library_id).or_default();
        active.retain(|j| !j.done.is_done());
        // Coalesce with a job that already covers this request.
        let joined = match &kind {
            JobKind::Library(mode) => active.iter().find(|j| j.is_library() && j.join_library(*mode)),
            JobKind::Dirs(dirs) => active.iter().find(|j| !j.is_sealed() && j.join_dirs(dirs)),
        };
        if let Some(j) = joined {
            j.raise(priority);
            let j = j.clone();
            drop(reg);
            for d in self.devices.snapshot() {
                d.admission.poke();
            }
            return j;
        }
        let id = self.next_job.fetch_add(1, Ordering::Relaxed);
        let job = JobShared::new(id, library_id, kind, priority);
        active.push(job.clone());
        let inner = self.clone();
        let j2 = job.clone();
        match std::thread::Builder::new()
            .name(format!("flux-scan-job-{id}"))
            .spawn(move || inner.run_job(j2))
        {
            Ok(h) => {
                reg.threads.retain(|t| !t.is_finished());
                reg.threads.push(h);
            }
            Err(e) => job.done.set(Err(ErrRepr::Other(format!("spawn: {e}")))),
        }
        job
    }

    // ---- the job thread --------------------------------------------------------

    fn run_job(self: Arc<Self>, job: Arc<JobShared>) {
        let t0 = Instant::now();
        let mut res = self.run_job_inner(&job, false);
        let res: JobResult = loop {
            // A library request that joined after this scan started gets one
            // more (Quick) pass, which starts after it. Decided under the
            // registry lock: a request either joins before this check and
            // is covered by the next pass, or finds the job gone and starts
            // a new scan.
            let mut reg = lock(&self.reg);
            if res.is_ok()
                && !job.is_cancelled()
                && !self.shutting_down.load(Ordering::Relaxed)
                && job.take_rescan()
            {
                drop(reg);
                Progress::add(&job.progress.follow_up_passes, 1);
                res = self.run_job_inner(&job, true);
                continue;
            }
            let total_ms = t0.elapsed().as_millis() as u64;
            let res: JobResult = res.map(|()| job.progress.summary(job.library_id, total_ms));
            let status = self.final_status(&job, &res);
            reg.last.insert(job.library_id, status);
            if let Some(v) = reg.active.get_mut(&job.library_id) {
                v.retain(|j| j.id != job.id);
            }
            break res;
        };
        // Back to the small idle cache, and the WAL truncated, once the
        // last writing scan is done.
        if job.bulk.load(Ordering::Relaxed) {
            if let Err(e) = self.catalog.writer().end_bulk().wait() {
                tracing::debug!(error = %e, "end_bulk failed");
            }
        }
        if let Ok(s) = &res {
            tracing::debug!(library = job.library_id, job = job.id, ?s, "scan finished");
        }
        if job.bulk.load(Ordering::Relaxed) {
            // The scan's snapshot, walk sets and page buffers are gone now.
            flux_catalog::release_free_memory();
        }
        job.done.set(res);
        self.emit(Event::ScanProgress {
            library_id: job.library_id,
            status: self.status(job.library_id),
        });
    }

    fn run_job_inner(self: &Arc<Self>, job: &Arc<JobShared>, follow_up: bool) -> Result<(), ErrRepr> {
        let lib = job.library_id;
        let roots: Vec<RootInfo> = self
            .catalog
            .read_blocking(scan_support::list_roots)?
            .into_iter()
            .filter(|r| r.library_id == lib)
            .collect();
        if roots.is_empty() {
            let exists = self
                .catalog
                .read_blocking(move |c| flux_catalog::read::library(c, lib))?
                .is_some();
            job.seal();
            return if exists {
                Ok(())
            } else {
                Err(ErrRepr::UnknownLibrary(lib))
            };
        }
        let want_resume = self.opts.startup == crate::StartupCatchUp::MilleResume;
        let mut done: HashSet<Id> = HashSet::new();
        let mut complete = true;
        let mut resume = Vec::new();
        loop {
            if job.is_cancelled() {
                return Err(ErrRepr::Cancelled);
            }
            // Roots this job (still) needs; a directory job may grow until
            // its first admission seals it.
            let kind = job.kind();
            let next = roots.iter().find(|r| {
                !done.contains(&r.id)
                    && match &kind {
                        JobKind::Library(_) => true,
                        JobKind::Dirs(dirs) => !dirs_under(dirs, &r.path).is_empty(),
                    }
            });
            let Some(root) = next else { break };
            done.insert(root.id);
            let full = matches!(kind, JobKind::Library(_));
            let r: RootResult = scan_root(self, job, root, want_resume && full, follow_up)?;
            complete &= r.complete;
            if let Some(entries) = r.resume {
                resume.push(entries);
            }
        }
        if !job.is_sealed() {
            job.seal();
        }
        if want_resume && complete && matches!(job.kind(), JobKind::Library(_)) {
            for r in resume {
                crate::resume::save(self, &r);
            }
        }
        Ok(())
    }

    /// What `scan_root` should scan of `root` for this (sealed) job. A
    /// follow-up pass is a Quick scan of the whole root.
    pub(crate) fn scope_for(&self, kind: &JobKind, root: &RootInfo, follow_up: bool) -> Scope {
        match kind {
            JobKind::Library(_) if follow_up => Scope::Full(crate::ScanMode::Quick),
            JobKind::Library(mode) => Scope::Full(*mode),
            JobKind::Dirs(dirs) => Scope::Dirs(dirs_under(dirs, &root.path)),
        }
    }

    /// A root answered a scan: remember when (no catalog write).
    pub(crate) fn root_seen(&self, root: &RootInfo) {
        lock(&self.roots_seen).insert(
            root.id,
            (root.library_id, root.path.clone(), flux_catalog::now_ms()),
        );
    }

    pub(crate) fn root_last_seen(&self, library_id: Id) -> Vec<(String, i64)> {
        let mut v: Vec<(String, i64)> = lock(&self.roots_seen)
            .values()
            .filter(|(lib, _, _)| *lib == library_id)
            .map(|(_, path, t)| (path.clone(), *t))
            .collect();
        v.sort();
        v
    }

    fn final_status(&self, job: &JobShared, res: &JobResult) -> ScanStatus {
        let p = &job.progress;
        let mut s = p.status(ScanState::Idle);
        s.finished_at = Some(flux_catalog::now_ms());
        if s.files_total.is_none() {
            s.files_total = Some(s.files_seen);
        }
        match res {
            Err(ErrRepr::Cancelled) => {}
            Err(e) => s.last_error = Some(e.to_error().to_string()),
            Ok(_) => {}
        }
        if !lock(&p.unavailable_roots).is_empty() {
            s.state = ScanState::Unavailable;
        } else if s.last_error.is_some() {
            s.state = ScanState::Error;
        }
        s
    }

    // ---- status ----------------------------------------------------------------

    pub(crate) fn status(&self, library_id: Id) -> ScanStatus {
        let reg = lock(&self.reg);
        let active: Vec<Arc<JobShared>> = reg
            .active
            .get(&library_id)
            .map(|v| v.iter().filter(|j| !j.done.is_done()).cloned().collect())
            .unwrap_or_default();
        // The library scan's progress if one runs, else a directory job's.
        if let Some(j) = active
            .iter()
            .find(|j| j.is_library() && j.is_sealed())
            .or_else(|| active.iter().find(|j| j.is_sealed()))
            .or_else(|| active.first())
        {
            return j.progress.status(ScanState::Scanning);
        }
        let watching = lock(&self.watches).contains_key(&library_id);
        match reg.last.get(&library_id) {
            Some(s) => {
                let mut s = s.clone();
                if s.state == ScanState::Idle && watching {
                    s.state = ScanState::Watching;
                }
                s
            }
            None => {
                let mut s = flux_catalog::read::idle_status();
                if watching {
                    s.state = ScanState::Watching;
                }
                s
            }
        }
    }

    pub(crate) fn progress_event(&self, job: &JobShared) {
        self.emit(Event::ScanProgress {
            library_id: job.library_id,
            status: job.progress.status(ScanState::Scanning),
        });
    }

    // ---- roots -------------------------------------------------------------------

    /// Bounded stat of a root: `(st_dev, mtime_ms, canonical path)`, or why
    /// it can't be reached.
    pub(crate) fn stat_root(&self, root: &Path) -> Result<RootStatOk, String> {
        let out = self
            .root_stat
            .stat(root, true)
            .map_err(|f| f.into_fx(root).to_string())?;
        let st = out.as_stat().ok_or("bounded stat returned no metadata")?;
        if st.is_dataless() {
            return Err(format!(
                "{}: dataless placeholder (not on this machine)",
                root.display()
            ));
        }
        let meta = st.target.as_ref().unwrap_or(&st.metadata);
        if !meta.is_dir() {
            return Err(format!("{}: not a directory", root.display()));
        }
        Ok(RootStatOk {
            dev: dev_of(meta),
            mtime_ms: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis() as i64),
            canonical: st.canonical.clone().unwrap_or_else(|| root.to_path_buf()),
        })
    }

    /// Never delete: mark the root unavailable (a writer command only when
    /// its state changes) and say so.
    pub(crate) fn root_unreachable(&self, job: &JobShared, root: &RootInfo, why: &str) {
        tracing::warn!(root = %root.path, reason = why, "root unreachable; marking unavailable");
        let p = &job.progress;
        lock(&p.unavailable_roots).push(root.path.clone());
        *lock(&p.last_error) = Some(format!("root unreachable: {why}"));
        if root.state == RootState::Available {
            Progress::add(&p.writer_commands, 2);
            match self.catalog.writer().set_root_availability(root.id, false).wait() {
                Ok(c) if c.changed => self.emit(Event::SourceAvailability {
                    library_id: root.library_id,
                    root: root.path.clone(),
                    available: false,
                }),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "set_root_availability(false) failed"),
            }
            let rec = flux_catalog::ScanRecord {
                started_at: None,
                finished_at: None,
                error: Some(format!("root unreachable: {why}")),
            };
            let _ = self.catalog.writer().record_scan(root.id, rec).wait();
        }
    }

    pub(crate) fn root_reachable(&self, job: &JobShared, root: &RootInfo) {
        if root.state == RootState::Unavailable {
            Progress::add(&job.progress.writer_commands, 1);
            match self.catalog.writer().set_root_availability(root.id, true).wait() {
                Ok(_) => self.emit(Event::SourceAvailability {
                    library_id: root.library_id,
                    root: root.path.clone(),
                    available: true,
                }),
                Err(e) => tracing::warn!(error = %e, "set_root_availability(true) failed"),
            }
        }
    }

    // ---- shutdown ------------------------------------------------------------------

    pub(crate) fn shutdown(&self, timeout: Duration) {
        self.shutting_down.store(true, Ordering::Relaxed);
        crate::watch::unwatch_all(self);
        let (jobs, threads) = {
            let mut reg = lock(&self.reg);
            let jobs: Vec<Arc<JobShared>> = reg.active.values().flatten().cloned().collect();
            (jobs, std::mem::take(&mut reg.threads))
        };
        for j in &jobs {
            j.cancel();
        }
        let deadline = Instant::now() + timeout;
        for t in threads {
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
        for j in &jobs {
            if !j.done.is_done() {
                j.done.set(Err(ErrRepr::ShuttingDown));
            }
        }
        // The background fingerprint pass stops between files.
        self.fingerprints.stop(deadline);
        // Workers finish (cancelled tasks return at once), then exit. An
        // in-flight ffprobe watches its job's cancel flag and is killed
        // within ~10 ms, so this join honours the grace too.
        self.pool.stop();
        self.ffprobe.stop();
    }

    pub(crate) fn devices_info(&self) -> Vec<crate::DeviceInfo> {
        let mut v: Vec<crate::DeviceInfo> = self
            .devices
            .snapshot()
            .iter()
            .map(|d| crate::DeviceInfo {
                dev: d.dev,
                class: d.class,
                fstype: d.fstype.clone(),
                concurrency: d.concurrency,
            })
            .collect();
        v.sort_by_key(|d| d.dev);
        v
    }
}

pub(crate) struct RootStatOk {
    pub dev: u64,
    pub mtime_ms: i64,
    pub canonical: PathBuf,
}

#[cfg(unix)]
fn dev_of(m: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.dev()
}

#[cfg(not(unix))]
fn dev_of(_m: &std::fs::Metadata) -> u64 {
    0
}

/// The requested directories inside `root` (absolute, deduplicated).
pub(crate) fn dirs_under(dirs: &[PathBuf], root: &str) -> Vec<PathBuf> {
    let root = Path::new(root);
    let mut out: Vec<PathBuf> = dirs
        .iter()
        .filter(|d| paths::rel_of(root, d).is_some())
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}
