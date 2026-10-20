//! The background fingerprint pass ([`crate::ScanConfig::fingerprint_later`]).
//!
//! fp128 reads 64 KiB at both ends of a file. The probe needs only the
//! head, so the tail read is extra I/O that nothing else shares, and on a
//! cold scan it is most of the cost (docs/prototype/scan.md: 2–9×). New and
//! changed files are therefore committed without a fingerprint (browsable at
//! once) and fingerprinted here afterwards, at the lowest priority
//! (`Priority::Deep` on the shared pool and the device's I/O semaphore), and
//! only while no scan runs or waits on the device: a cold scan of the next
//! library or a watcher batch never competes with it for the disk. Results go to the catalog in
//! batches through `Writer::attach_fingerprints`, which also merges a file
//! into a tombstone with the same content (a move or copy across volumes
//! found late), keeping the tombstone's identity and watch state.
//!
//! A file is fingerprinted only if its stat is still the walked one after
//! the read, and the catalog attaches it only while the row still has that
//! stat, so a file that changes meanwhile is simply done again by the next
//! scan. Rows that lack a fingerprint for any reason (a shutdown before
//! this pass ran) are queued again by the next scan that sees them.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Sender};
use flux_catalog::FileFingerprint;
use flux_core::api::{Event, Id};
use flux_probe::FileSource;

use crate::device::Device;
use crate::engine::Inner;
use crate::fingerprint::fingerprint_direct;
use crate::job::lock;
use crate::pool::Pending;

/// A file to fingerprint after it was committed.
pub(crate) struct LaterFp {
    pub rel: String,
    pub abs: PathBuf,
    pub size: u64,
    pub mtime_ns: i64,
}

struct Batch {
    library_id: Id,
    root_id: Id,
    device: Arc<Device>,
    files: Vec<LaterFp>,
}

/// Lowest scheduling priority (`Priority::Deep`).
const PRIO: u8 = 0;

#[derive(Default)]
pub(crate) struct Fingerprinter {
    tx: Mutex<Option<Sender<Batch>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    /// Batches queued or running; `wait_idle` waits for zero.
    outstanding: Arc<(Mutex<usize>, Condvar)>,
    /// `(root, rel_path)` queued or in flight (a file is queued once).
    queued: Arc<Mutex<HashSet<(Id, String)>>>,
    stop: Arc<AtomicBool>,
}

impl Fingerprinter {
    /// Queue a root's files (those not queued already).
    pub(crate) fn enqueue(
        &self,
        inner: &Arc<Inner>,
        library_id: Id,
        root_id: Id,
        device: Arc<Device>,
        files: Vec<LaterFp>,
    ) {
        if self.stop.load(Ordering::Relaxed) {
            return;
        }
        let files: Vec<LaterFp> = {
            let mut q = lock(&self.queued);
            files
                .into_iter()
                .filter(|f| q.insert((root_id, f.rel.clone())))
                .collect()
        };
        if files.is_empty() {
            return;
        }
        let mut tx = lock(&self.tx);
        if tx.is_none() {
            let (s, r) = unbounded::<Batch>();
            let weak = Arc::downgrade(inner);
            let (outstanding, queued, stop) =
                (self.outstanding.clone(), self.queued.clone(), self.stop.clone());
            match std::thread::Builder::new()
                .name("flux-scan-fingerprint".into())
                .spawn(move || {
                    for b in r.iter() {
                        let big = b.files.len() >= 256;
                        run_batch(&weak, b, &queued, &stop);
                        if big {
                            // A cold scan's worth of keys left a table of
                            // ~1 MB behind; drop it once nothing is queued.
                            let mut q = lock(&queued);
                            if q.is_empty() {
                                *q = HashSet::new();
                            }
                            drop(q);
                            // Its queues and page buffers are freed by now.
                            flux_catalog::release_free_memory();
                        }
                        let (n, cv) = &*outstanding;
                        let mut n = lock(n);
                        *n = n.saturating_sub(1);
                        cv.notify_all();
                    }
                }) {
                Ok(h) => {
                    *lock(&self.thread) = Some(h);
                    *tx = Some(s);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "fingerprint thread did not start");
                    return;
                }
            }
        }
        *lock(&self.outstanding.0) += 1;
        let batch = Batch {
            library_id,
            root_id,
            device,
            files,
        };
        if let Some(s) = tx.as_ref() {
            if s.send(batch).is_err() {
                let (n, cv) = &*self.outstanding;
                *lock(n) -= 1;
                cv.notify_all();
            }
        }
    }

    /// Wait until nothing is queued or running, at most `timeout`.
    pub(crate) fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let (n, cv) = &*self.outstanding;
        let mut g = lock(n);
        while *g > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            g = cv
                .wait_timeout(g, left.min(Duration::from_millis(50)))
                .map(|r| r.0)
                .unwrap_or_else(|e| e.into_inner().0);
        }
        true
    }

    /// Stop between files and wait for the thread until `deadline`.
    pub(crate) fn stop(&self, deadline: Instant) {
        self.stop.store(true, Ordering::Relaxed);
        lock(&self.tx).take();
        let h = lock(&self.thread).take();
        if let Some(h) = h {
            while !h.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if h.is_finished() {
                let _ = h.join();
            }
        }
    }
}

fn run_batch(
    weak: &Weak<Inner>,
    b: Batch,
    queued: &Arc<Mutex<HashSet<(Id, String)>>>,
    stop: &Arc<AtomicBool>,
) {
    let Some(inner) = weak.upgrade() else {
        return;
    };
    let rows = inner.cfg.batch_rows.max(1);
    let cap = (inner.cfg.workers * 2).max(8);
    let done: Arc<Mutex<Vec<FileFingerprint>>> = Arc::new(Mutex::new(Vec::new()));
    let pending = Arc::new(Pending::default());
    // A cold scan's worth of attaches writes like a scan: the larger cache
    // while it runs, the WAL truncated after.
    let bulk = b.files.len() >= 256;
    if bulk {
        let _ = inner.catalog.writer().begin_bulk().wait();
    }
    let attach = |fps: Vec<FileFingerprint>| {
        if fps.is_empty() {
            return;
        }
        match inner.catalog.writer().attach_fingerprints(b.root_id, fps).wait() {
            Ok(o) => {
                if !(o.items_added.is_empty() && o.items_updated.is_empty() && o.items_removed.is_empty()) {
                    inner.emit(Event::ItemsChanged {
                        library_id: b.library_id,
                        added: o.items_added,
                        updated: o.items_updated,
                        removed: o.items_removed,
                    });
                }
            }
            Err(e) => tracing::warn!(error = %e, "attaching fingerprints failed"),
        }
    };
    for f in b.files {
        if stop.load(Ordering::Relaxed) {
            lock(queued).remove(&(b.root_id, f.rel));
            continue;
        }
        // Background work: the device belongs to any scan that runs or
        // waits on it (a cold scan of the next library, a watcher batch).
        while b.device.admission.busy() && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(20));
        }
        pending.wait_below(cap, || stop.load(Ordering::Relaxed));
        pending.add(1);
        let (device, done2, pending2, queued2, stop2) = (
            b.device.clone(),
            done.clone(),
            pending.clone(),
            queued.clone(),
            stop.clone(),
        );
        let root_id = b.root_id;
        let ok = inner.pool.submit(PRIO, move || {
            if !stop2.load(Ordering::Relaxed) {
                if let Some(fp) = fingerprint_file(&device, &f) {
                    lock(&done2).push(fp);
                }
            }
            lock(&queued2).remove(&(root_id, f.rel));
            pending2.done();
        });
        if !ok {
            pending.done();
        }
        let ready = {
            let mut d = lock(&done);
            (d.len() >= rows).then(|| std::mem::take(&mut *d))
        };
        if let Some(fps) = ready {
            attach(fps);
        }
    }
    pending.wait_below(0, || stop.load(Ordering::Relaxed));
    let rest = std::mem::take(&mut *lock(&done));
    attach(rest);
    if bulk {
        let _ = inner.catalog.writer().end_bulk().wait();
    }
}

/// fp128 + oshash of one file, if it still has the walked size and mtime
/// after the read (else it changed: the next scan does it again).
fn fingerprint_file(device: &Device, f: &LaterFp) -> Option<FileFingerprint> {
    let _io = device.io.acquire(PRIO);
    let mut src = FileSource::open(&f.abs).ok()?;
    // No page cache: nothing else reads the file in this pass.
    let fp = fingerprint_direct(&mut src).ok()?;
    let meta = std::fs::metadata(&f.abs).ok()?;
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)?;
    if meta.len() != f.size || mtime_ns != f.mtime_ns {
        return None;
    }
    Some(FileFingerprint {
        rel_path: f.rel.clone(),
        size: f.size,
        mtime_ns: f.mtime_ns,
        fp128: fp.fp128,
        oshash: fp.oshash,
    })
}
