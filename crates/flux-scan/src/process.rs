//! Per-file work on the shared worker pool: open once, probe (and, when it
//! decides identity now, fingerprint) through one page cache, parse the name
//! with its siblings, hand the upsert to the batcher. Unknown containers go
//! to the bounded ffprobe pool.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossbeam_channel::{bounded, Sender};
use flux_catalog::{scan_support, ExpectedRow, FileFacts, FileMove, FileUpsert, ProbeUpdate};
use flux_core::api::Id;
use flux_core::LibraryKind;
use flux_probe::{ContainerHint, FileSource, FrameSeiMode, PageCache, ProbeError, ProbeOptions};
use mille_core::ErrorCode;

use crate::device::Device;
use crate::engine::Inner;
use crate::fingerprint::fingerprint;
use crate::fp_later::LaterFp;
use crate::job::{lock, JobShared, Progress};
use crate::pool::Pending;

/// Why a file is being processed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Work {
    /// A path the catalog doesn't know. May turn out to be a move (its
    /// fingerprint matches a file of this root): then it is deferred.
    New,
    /// New, and already ruled out as a move: never deferred.
    NewFinal,
    /// Size or mtime changed, or the path now holds another inode.
    Changed,
    /// The stored probe failed; its backoff has passed (or the user asked).
    Retry,
    /// Deep scan: the stored probe is from an older flux-probe.
    Reprobe,
}

impl Work {
    fn is_new(self) -> bool {
        matches!(self, Work::New | Work::NewFinal)
    }
}

/// One media file to process.
pub(crate) struct WorkItem {
    pub facts: FileFacts,
    pub abs: PathBuf,
    pub name: String,
    pub siblings: Arc<Vec<String>>,
    pub work: Work,
    /// The catalog has no fingerprint of this content.
    pub need_fp: bool,
    /// Reached through a symlink that leaves the library's roots: a new
    /// file is cataloged only if it probes as media (SEC-2).
    pub external: bool,
}

/// A new file whose fingerprint matches a file of this root: a move if that
/// file is gone by the end of the walk, else a copy.
pub(crate) struct FpCandidate {
    pub upsert: FileUpsert,
}

/// What the batcher collects into catalog transactions.
pub(crate) enum Out {
    Upsert(FileUpsert),
    /// A file of another root of the same volume moved here (same inode).
    Move(FileMove, ExpectedRow),
}

/// What workers share for one root scan.
pub(crate) struct RootCtx {
    pub inner: Arc<Inner>,
    pub job: Arc<JobShared>,
    pub root_id: Id,
    pub kind: LibraryKind,
    pub device: Arc<Device>,
    /// `(fp128, size)` of this root's live files (move detection).
    pub fp_index: HashSet<(u128, u64)>,
    /// Sizes of those files: a new file of such a size may be one of them
    /// moved, so it is fingerprinted up front even when fingerprints are
    /// otherwise left to the background pass.
    pub fp_sizes: HashSet<u64>,
    /// Every other root: id → path (cross-root move detection).
    pub other_roots: HashMap<Id, String>,
    pub tx: Mutex<Option<Sender<Out>>>,
    pub deferred: Mutex<Vec<FpCandidate>>,
    /// Files to fingerprint after they are committed.
    pub later: Mutex<Vec<LaterFp>>,
    pub pending: Pending,
    /// First and last worker activity, for `process_ms`.
    pub busy: Mutex<Option<(Instant, Instant)>>,
}

impl RootCtx {
    pub(crate) fn send(&self, u: FileUpsert) {
        self.send_out(Out::Upsert(u));
    }

    fn send_out(&self, o: Out) {
        let tx = lock(&self.tx).clone();
        if let Some(tx) = tx {
            let _ = tx.send(o);
        }
    }

    fn mark_busy(&self, start: Instant) {
        let now = Instant::now();
        let mut b = lock(&self.busy);
        *b = Some(match *b {
            None => (start, now),
            Some((s, e)) => (s.min(start), e.max(now)),
        });
    }

    /// Fingerprint this file after it is committed.
    pub(crate) fn fingerprint_later(&self, f: &FileFacts, abs: &Path) {
        Progress::add(&self.job.progress.fingerprints_deferred, 1);
        lock(&self.later).push(LaterFp {
            rel: f.rel_path.clone(),
            abs: abs.to_path_buf(),
            size: f.size,
            mtime_ns: f.mtime_ns,
        });
    }

    /// Queue `item` on the shared pool at the job's current priority.
    pub(crate) fn submit(self: &Arc<Self>, item: WorkItem) {
        self.pending.add(1);
        let ctx = self.clone();
        let ok = self.inner.pool.submit(self.job.prio(), move || {
            run(&ctx, item);
            ctx.pending.done();
        });
        if !ok {
            self.pending.done();
        }
    }
}

enum Outcome {
    Done(FileUpsert),
    Defer(FileUpsert),
    /// Another root's file, moved here: re-pointed, nothing read.
    Move(FileMove, ExpectedRow),
    /// flux-probe doesn't know the container: ffprobe it on the bounded
    /// pool (then `defer` as for `Defer`).
    Ffprobe(Box<FfprobeTask>),
    Skip,
}

struct FfprobeTask {
    facts: FileFacts,
    siblings: Arc<Vec<String>>,
    name: String,
    abs: PathBuf,
    work: Work,
    defer: bool,
    later: bool,
    external: bool,
}

fn run(ctx: &Arc<RootCtx>, item: WorkItem) {
    if ctx.job.is_cancelled() {
        return;
    }
    let start = Instant::now();
    let out = process(ctx, item);
    ctx.mark_busy(start);
    match out {
        Outcome::Done(u) => ctx.send(u),
        Outcome::Defer(u) => lock(&ctx.deferred).push(FpCandidate { upsert: u }),
        Outcome::Move(m, e) => ctx.send_out(Out::Move(m, e)),
        Outcome::Ffprobe(t) => {
            ctx.pending.add(1);
            let c = ctx.clone();
            let queued = ctx.inner.ffprobe.submit(Box::new(move || {
                if !c.job.is_cancelled() {
                    ffprobe_task(&c, *t);
                }
                c.pending.done();
            }));
            if !queued {
                ctx.pending.done();
            }
        }
        Outcome::Skip => {}
    }
}

fn ffprobe_task(c: &Arc<RootCtx>, t: FfprobeTask) {
    let start = Instant::now();
    let p = &c.job.progress;
    Progress::add(&p.opened, 1);
    let cancelled = || c.job.is_cancelled();
    let res = c.inner.ffprobe_run(&t.abs, &cancelled);
    if c.job.is_cancelled() {
        // Killed by a shutdown or cancel: not a fact about the file.
        return;
    }
    let probe = match res {
        Ok(r) => {
            Progress::add(&p.ffprobe_fallbacks, 1);
            ProbeUpdate::Probed(Box::new(r))
        }
        Err(e) if t.external && t.work.is_new() => {
            Progress::add(&p.external_not_media, 1);
            tracing::info!(path = %t.abs.display(), error = %e, "not cataloged: reached through a link leaving the library and not media");
            lock(&c.inner.rejected).insert((
                c.root_id,
                t.facts.rel_path.clone(),
                t.facts.size,
                t.facts.mtime_ns,
            ));
            return;
        }
        Err(e) => {
            Progress::add(&p.probe_failures, 1);
            tracing::debug!(path = %t.abs.display(), error = %e, "ffprobe fallback failed");
            ProbeUpdate::Failed(e)
        }
    };
    let guess = parse(&t.facts.rel_path, &t.name, &t.siblings, c.kind);
    let u = FileUpsert {
        facts: t.facts,
        probe,
        guess: Some(guess),
    };
    if t.later {
        c.fingerprint_later(&u.facts, &t.abs);
    }
    if t.defer {
        lock(&c.deferred).push(FpCandidate { upsert: u });
    } else {
        c.send(u);
    }
    c.mark_busy(start);
}

fn process(ctx: &RootCtx, item: WorkItem) -> Outcome {
    let WorkItem {
        mut facts,
        abs,
        name,
        siblings,
        work,
        need_fp,
        external,
    } = item;
    let p = &ctx.job.progress;
    let cfg = &ctx.inner.cfg;
    // A new path holding the inode of a live file of another root (same
    // volume, e.g. `Movies/` → `Movies 4K/`) is that file, moved: re-point
    // it before reading anything (asset and watch state kept).
    if work.is_new() {
        let t = Instant::now();
        let found = cross_root_source(ctx, &facts);
        Progress::add(&p.cross_root_ns, t.elapsed().as_nanos() as u64);
        if let Some(src) = found {
            let guess = parse(&facts.rel_path, &name, &siblings, ctx.kind);
            return Outcome::Move(
                FileMove {
                    file_id: src.file_id,
                    to: facts,
                    guess: Some(guess),
                },
                src,
            );
        }
    }
    let rejected_key = (ctx.root_id, facts.rel_path.clone(), facts.size, facts.mtime_ns);
    if external && work.is_new() && lock(&ctx.inner.rejected).contains(&rejected_key) {
        Progress::add(&p.external_not_media, 1);
        return Outcome::Skip;
    }
    let probe_result = {
        let _io = ctx.device.io.acquire(ctx.job.prio());
        let src = match FileSource::open(&abs) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Gone since the walk listed it; the next scan settles it.
                return Outcome::Skip;
            }
            Err(e) if external && work.is_new() => {
                Progress::add(&p.external_not_media, 1);
                tracing::debug!(path = %abs.display(), error = %e, "not cataloged: unreadable target outside the library");
                lock(&ctx.inner.rejected).insert(rejected_key);
                return Outcome::Skip;
            }
            Err(e) => {
                Progress::add(&p.probe_failures, 1);
                let guess = parse(&facts.rel_path, &name, &siblings, ctx.kind);
                return Outcome::Done(FileUpsert {
                    facts,
                    probe: ProbeUpdate::Failed(format!("open: {e}")),
                    guess: Some(guess),
                });
            }
        };
        Progress::add(&p.opened, 1);
        // 16 KiB pages on local disks (the fingerprint's 64 KiB head and
        // tail then cost ~144 KiB instead of ~192 KiB with 64 KiB pages, and
        // the probe's first read is still a cache hit); 64 KiB pages where
        // round trips are expensive (flux-probe's read-size advice).
        let page = match ctx.device.class {
            crate::DeviceClass::Fast => 16 * 1024,
            crate::DeviceClass::Slow => 64 * 1024,
        };
        let mut cache = PageCache::with_page_size(src, page);
        // Up front only when the fingerprint decides identity in this scan:
        // a new file the size of a fingerprinted file of this root may be
        // that file moved (copy + delete). Everything else is fingerprinted
        // after it is committed (the tail read is a cold scan's main cost).
        let inline = cfg.fingerprint
            && need_fp
            && (!cfg.fingerprint_later || (work == Work::New && ctx.fp_sizes.contains(&facts.size)));
        if inline {
            let t = Instant::now();
            match fingerprint(&mut cache) {
                Ok(fp) => {
                    facts.fp128 = Some(fp.fp128);
                    facts.oshash = fp.oshash;
                }
                Err(e) => tracing::debug!(path = %abs.display(), error = %e, "fingerprint failed"),
            }
            Progress::add(&p.fingerprint_ns, t.elapsed().as_nanos() as u64);
        }
        let opts = ProbeOptions {
            frame_sei: FrameSeiMode::IfNeeded,
            ffprobe_fallback: false,
            read_size: 0,
        };
        let t = Instant::now();
        let hint = ContainerHint::from_path(&abs);
        let r = flux_probe::probe_cached(&mut cache, hint, &opts);
        // No prober knows it: is it media at all? (A TypeScript `.ts` is
        // text.) The head is in the page cache already.
        let not_media = match &r {
            Err(ProbeError::UnknownFormat { .. }) => cache
                .read(0, flux_probe::NOT_MEDIA_SNIFF_LEN)
                .ok()
                .and_then(|head| flux_probe::not_media(&head, hint)),
            _ => None,
        };
        Progress::add(&p.probe_ns, t.elapsed().as_nanos() as u64);
        Progress::add(&p.read_calls, u64::from(cache.reads));
        Progress::add(&p.read_bytes, cache.bytes);
        (r, not_media)
    };
    let (probe_result, not_media) = probe_result;
    if let Some(why) = not_media {
        if external && work.is_new() {
            Progress::add(&p.external_not_media, 1);
            tracing::info!(path = %abs.display(), reason = why, "not cataloged: reached through a link leaving the library and not media");
            lock(&ctx.inner.rejected).insert(rejected_key);
            return Outcome::Skip;
        }
        // Cataloged as not media: never an item, never ffprobed, and not
        // opened again until it changes.
        Progress::add(&p.not_media, 1);
        tracing::debug!(path = %abs.display(), reason = why, "not media");
        let guess = parse(&facts.rel_path, &name, &siblings, ctx.kind);
        return Outcome::Done(FileUpsert {
            facts,
            probe: ProbeUpdate::NotMedia(format!("not media: {why}")),
            guess: Some(guess),
        });
    }
    let defer = work == Work::New
        && facts
            .fp128
            .is_some_and(|fp| ctx.fp_index.contains(&(fp, facts.size)));
    let later = cfg.fingerprint && need_fp && facts.fp128.is_none();
    let probe = match probe_result {
        Ok(r) => ProbeUpdate::Probed(Box::new(r)),
        Err(e) if cfg.ffprobe_fallback && e.wants_ffprobe() => {
            return Outcome::Ffprobe(Box::new(FfprobeTask {
                facts,
                siblings,
                name,
                abs,
                work,
                defer,
                later,
                external,
            }));
        }
        Err(e) if external && work.is_new() => {
            Progress::add(&p.external_not_media, 1);
            tracing::info!(path = %abs.display(), error = %e, "not cataloged: reached through a link leaving the library and not media");
            lock(&ctx.inner.rejected).insert(rejected_key);
            return Outcome::Skip;
        }
        Err(e) => {
            Progress::add(&p.probe_failures, 1);
            ProbeUpdate::Failed(e.to_string())
        }
    };
    let t = Instant::now();
    let guess = parse(&facts.rel_path, &name, &siblings, ctx.kind);
    Progress::add(&p.parse_ns, t.elapsed().as_nanos() as u64);
    let u = FileUpsert {
        facts,
        probe,
        guess: Some(guess),
    };
    if later {
        ctx.fingerprint_later(&u.facts, &abs);
    }
    if defer {
        Outcome::Defer(u)
    } else {
        Outcome::Done(u)
    }
}

/// Cross-root moves on one volume: a new file whose `(dev, ino)` belongs to
/// a file of another root that is no longer at its path (e.g. moved from
/// `Movies/` to `Movies 4K/`), live or already tombstoned by that root's
/// own scan. Same inode, size and birth time: the same file, so it is
/// re-pointed (file id, asset and watch state kept) without reading it or
/// needing its fingerprint. Returns that file's row as the guard for the
/// move (the catalog applies it only while the row is still there).
/// Looking up by inode (indexed, one row) keeps this O(1) per new file even
/// in libraries full of identical files. Moves across volumes (new inode)
/// re-attach through the catalog's tombstone match by fingerprint instead.
pub(crate) fn cross_root_source(ctx: &RootCtx, f: &FileFacts) -> Option<ExpectedRow> {
    if ctx.other_roots.is_empty() {
        return None;
    }
    let (Some(dev), Some(ino)) = (f.dev, f.ino) else {
        return None;
    };
    let Ok(Some((root_id, rel, row))) = ctx
        .inner
        .catalog
        .read_blocking(move |c| scan_support::find_by_dev_ino(c, dev, ino))
    else {
        return None;
    };
    // An inode reused by another file has another size or birth time.
    let btime_ok = match (row.btime_ns, f.btime_ns) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    };
    if root_id == ctx.root_id || row.size != f.size || !btime_ok {
        return None;
    }
    let root_path = ctx.other_roots.get(&root_id)?;
    let old = Path::new(root_path).join(&rel);
    let gone = matches!(ctx.inner.root_stat.stat(&old, false), Err(e) if e.code() == ErrorCode::ENOENT);
    // Only when its own root answers (an unreachable root proves nothing).
    if gone && ctx.inner.stat_root(Path::new(root_path)).is_ok() {
        Some(ExpectedRow {
            file_id: row.id,
            rel_path: rel,
            size: row.size,
            mtime_ns: row.mtime_ns,
        })
    } else {
        None
    }
}

/// flux-names with the directory's other file names as siblings.
pub(crate) fn parse(rel: &str, name: &str, siblings: &[String], kind: LibraryKind) -> flux_core::NameGuess {
    let sibs: Vec<&str> = siblings
        .iter()
        .map(String::as_str)
        .filter(|s| *s != name)
        .collect();
    flux_names::parse(Path::new(rel), &sibs, kind)
}

type FfJob = Box<dyn FnOnce() + Send + 'static>;

/// `ffprobe_workers` threads behind a bounded queue: at most that many
/// ffprobe processes run at once, and a full queue applies back-pressure to
/// the workers that hand files over.
pub(crate) struct FfprobePool {
    tx: Mutex<Option<Sender<FfJob>>>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl FfprobePool {
    pub(crate) fn new(n: usize) -> FfprobePool {
        let (tx, rx) = bounded::<FfJob>(64);
        let threads = (0..n.max(1))
            .filter_map(|i| {
                let rx = rx.clone();
                std::thread::Builder::new()
                    .name(format!("flux-scan-ffprobe-{i}"))
                    .spawn(move || {
                        for job in rx.iter() {
                            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                        }
                    })
                    .ok()
            })
            .collect();
        FfprobePool {
            tx: Mutex::new(Some(tx)),
            threads: Mutex::new(threads),
        }
    }

    pub(crate) fn submit(&self, job: FfJob) -> bool {
        let tx = lock(&self.tx).clone();
        tx.is_some_and(|tx| tx.send(job).is_ok())
    }

    pub(crate) fn stop(&self) {
        lock(&self.tx).take();
        for t in std::mem::take(&mut *lock(&self.threads)) {
            let _ = t.join();
        }
    }
}

impl Inner {
    /// The ffprobe fallback with the configured binary and timeout; killed
    /// as soon as `cancelled()` (the scan's cancel flag) turns true.
    pub(crate) fn ffprobe_run(
        &self,
        path: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<flux_core::ProbeResult, String> {
        crate::ffprobe::run(&self.cfg.ffprobe, path, self.cfg.ffprobe_timeout, cancelled)
    }
}

/// Count of upserts in a batch that carry a fresh probe.
pub(crate) fn probed_count(us: &[FileUpsert]) -> u64 {
    us.iter()
        .filter(|u| !matches!(u.probe, ProbeUpdate::Keep))
        .count() as u64
}
