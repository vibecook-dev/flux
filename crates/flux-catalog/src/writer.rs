//! The single writer: one std thread owns the read-write connection and
//! drains a bounded command channel (research/02 §7.2).
//!
//! - **Batching.** Commands are grouped into one transaction until it holds
//!   ≥ [`WriterConfig::max_rows`] changed rows or has been open
//!   [`WriterConfig::max_latency`], or the queue is empty (group commit: an
//!   idle writer commits immediately, a busy one amortises commits).
//! - **Isolation per command.** Each command runs in its own `SAVEPOINT`, so a
//!   failing command rolls back alone and the rest of the batch commits.
//! - **Replies after commit.** A command's result is delivered only once its
//!   transaction committed, so a caller that then reads through the pool sees
//!   its own write.
//! - **Maintenance when idle:** a passive WAL checkpoint, and `PRAGMA
//!   optimize` at most every [`WriterConfig::optimize_every`].
//!
//! Every public method returns a [`Request`]: `.await` it from async code,
//! `.wait()` from threads, or `.submit()` to pipeline several commands and
//! collect the [`Reply`]s later.

use std::future::{Future, IntoFuture};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use flux_core::api::{CreateLibrary, Id, Identity, Library, Progress};
use rusqlite::{params, Connection, OptionalExtension};

use crate::changes::{Actor, Committed, PlannedItem};
use crate::error::{CatalogError, Result, RootOverlap};
use crate::jobs::{Backoff, ClaimJobs, Enqueued, Finished, JobLease, JobResult, JobSpec};
use crate::model::{
    art_source_str, clamp_i64, library_kind_str, now_ms, role_str, ApplyOutcome, ArtKind, AvailabilityChange,
    FileFingerprint, ItemMetadata, NewArt, PlayIndex, PurgeOutcome, ScanBatch, ScanRecord,
};
use crate::read::FileCounts;
use crate::rekey::{EpisodeRekey, RekeyOutcome};
use crate::scan_support;

/// Writer tuning. Defaults follow research/02 §7.2.
#[derive(Debug, Clone)]
pub struct WriterConfig {
    /// Commit once a transaction has changed this many rows.
    pub max_rows: u64,
    /// [`Writer::apply`] splits larger scan batches into jobs of at most this
    /// many upserts (≈8 changed rows per new file → ≈1000 rows per job), so
    /// one command never holds the write lock for long and progress commits
    /// stay frequent whatever batch size the scanner picks.
    pub max_files_per_job: usize,
    /// Commit once a transaction has been open this long.
    pub max_latency: Duration,
    /// Bounded command queue length (backpressure for the scanner).
    pub queue: usize,
    /// How long `missing` files are kept before [`Writer::purge_tombstones`] deletes them.
    pub tombstone_grace: Duration,
    /// Idle time after writes before a passive WAL checkpoint.
    pub idle_maintenance: Duration,
    /// Minimum interval between `PRAGMA optimize` runs.
    pub optimize_every: Duration,
    /// Enqueue match/art jobs for new and changed items during
    /// [`Writer::apply`] (off for a server that runs no metadata executor).
    pub enqueue_jobs: bool,
    /// Default attempts per job before it is marked failed.
    pub job_max_attempts: u32,
    /// Backoff after the first failed attempt; doubles per attempt.
    pub job_backoff_base: Duration,
    /// Backoff cap.
    pub job_backoff_max: Duration,
    /// The writer's page cache while a scan is writing ([`Writer::begin_bulk`]),
    /// KiB. Outside scans it is [`crate::CatalogConfig::writer_cache_kib`].
    pub bulk_cache_kib: i64,
}

impl Default for WriterConfig {
    fn default() -> Self {
        WriterConfig {
            bulk_cache_kib: 64 * 1024,
            max_rows: 1000,
            max_files_per_job: 128,
            max_latency: Duration::from_millis(50),
            queue: 256,
            tombstone_grace: Duration::from_secs(30 * 24 * 3600),
            idle_maintenance: Duration::from_secs(1),
            optimize_every: Duration::from_secs(600),
            enqueue_jobs: true,
            job_max_attempts: 5,
            job_backoff_base: Duration::from_secs(30),
            job_backoff_max: Duration::from_secs(6 * 3600),
        }
    }
}

/// Counters the writer maintains (cheap to read from any thread).
#[derive(Debug, Default)]
pub struct WriterStats {
    pub transactions: AtomicU64,
    pub commands: AtomicU64,
    pub rows: AtomicU64,
    pub failed_commands: AtomicU64,
    pub checkpoints: AtomicU64,
    pub optimizes: AtomicU64,
}

/// Called after the batch's commit with `Some(error)` if the commit failed.
type Finish = Box<dyn FnOnce(Option<&str>) + Send>;
type JobFn = Box<dyn FnOnce(&Connection) -> Finish + Send>;

enum Msg {
    Job(JobFn),
    Flush(flume::Sender<Result<()>>),
    Shutdown(flume::Sender<Result<()>>),
    /// Enter (`true`) or leave a bulk write (a scan); handled between
    /// transactions.
    Bulk(bool, flume::Sender<Result<()>>),
}

/// Cloneable handle to the writer thread.
#[derive(Clone)]
pub struct Writer {
    tx: flume::Sender<Msg>,
    stats: Arc<WriterStats>,
    tombstone_grace_ms: i64,
    max_files_per_job: usize,
    backoff: Backoff,
    /// `max_attempts` for jobs enqueued by apply; 0 = don't enqueue.
    apply_jobs: u32,
    /// Invalidated after every commit that may change `file` rows.
    counts: Arc<FileCounts>,
}

/// A command not yet sent. `.await` / `.wait()` sends it and waits for the
/// committed result; `.submit()` only enqueues it.
#[must_use = "a Request does nothing until awaited, waited or submitted"]
pub struct Request<T> {
    tx: flume::Sender<Msg>,
    /// Messages to enqueue, in order: usually one job; none for an empty
    /// batch (answered without the writer); several for a split scan batch.
    msgs: Vec<Msg>,
    rx: flume::Receiver<Result<T>>,
}

/// The pending result of a submitted command.
#[must_use = "dropping a Reply discards the command's result (the command still runs)"]
pub struct Reply<T> {
    rx: flume::Receiver<Result<T>>,
}

impl<T> Reply<T> {
    /// Block until the command's transaction committed (or failed).
    pub fn wait(self) -> Result<T> {
        self.rx.recv().map_err(|_| CatalogError::WriterGone)?
    }
    /// Async version of [`Reply::wait`].
    pub async fn wait_async(self) -> Result<T> {
        self.rx.recv_async().await.map_err(|_| CatalogError::WriterGone)?
    }
    /// The result if the writer has sent it, without waiting;
    /// [`CatalogError::WriterGone`] if not. Final only once the writer
    /// thread has exited.
    pub(crate) fn take_sent(self) -> Result<T> {
        self.rx.try_recv().map_err(|_| CatalogError::WriterGone)?
    }
}

impl<T: Send + 'static> Request<T> {
    /// Enqueue (blocking while the queue is full) and return the reply handle.
    pub fn submit(self) -> Result<Reply<T>> {
        for msg in self.msgs {
            self.tx.send(msg).map_err(|_| CatalogError::WriterGone)?;
        }
        Ok(Reply { rx: self.rx })
    }
    /// Enqueue without blocking a runtime thread.
    pub async fn submit_async(self) -> Result<Reply<T>> {
        for msg in self.msgs {
            self.tx
                .send_async(msg)
                .await
                .map_err(|_| CatalogError::WriterGone)?;
        }
        Ok(Reply { rx: self.rx })
    }
    /// Enqueue and block until committed.
    pub fn wait(self) -> Result<T> {
        self.submit()?.wait()
    }
}

impl<T: Send + 'static> IntoFuture for Request<T> {
    type Output = Result<T>;
    type IntoFuture = Pin<Box<dyn Future<Output = Result<T>> + Send>>;
    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move { self.submit_async().await?.wait_async().await })
    }
}

/// Run `f` inside `SAVEPOINT job`, rolling back just this job on error or panic.
fn in_savepoint<T>(conn: &Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    conn.execute_batch("SAVEPOINT job")?;
    let res = match catch_unwind(AssertUnwindSafe(|| f(conn))) {
        Ok(r) => r,
        Err(p) => {
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            Err(CatalogError::JobPanicked(msg))
        }
    };
    match res {
        Ok(v) => {
            conn.execute_batch("RELEASE job")?;
            Ok(v)
        }
        Err(e) => {
            if let Err(rb) = conn.execute_batch("ROLLBACK TO job; RELEASE job") {
                tracing::error!(error = %rb, "rollback to savepoint failed");
            }
            Err(e)
        }
    }
}

impl Writer {
    /// Spawn the writer thread over an already-migrated connection whose
    /// page cache is `cache_kib` (restored after each bulk write).
    pub(crate) fn spawn(
        conn: Connection,
        cfg: WriterConfig,
        cache_kib: i64,
        counts: Arc<FileCounts>,
    ) -> Result<(Writer, JoinHandle<()>)> {
        let (tx, rx) = flume::bounded(cfg.queue.max(1));
        let stats = Arc::new(WriterStats::default());
        let grace = i64::try_from(cfg.tombstone_grace.as_millis()).unwrap_or(i64::MAX);
        let max_files_per_job = cfg.max_files_per_job.max(1);
        let enqueue_jobs = cfg.enqueue_jobs;
        let backoff = Backoff {
            base_ms: i64::try_from(cfg.job_backoff_base.as_millis()).unwrap_or(i64::MAX),
            max_ms: i64::try_from(cfg.job_backoff_max.as_millis()).unwrap_or(i64::MAX),
            max_attempts: cfg.job_max_attempts.max(1),
        };
        let thread_stats = stats.clone();
        let handle = std::thread::Builder::new()
            .name("flux-catalog-writer".into())
            .spawn(move || writer_main(conn, rx, cfg, cache_kib, thread_stats))?;
        Ok((
            Writer {
                tx,
                stats,
                tombstone_grace_ms: grace,
                max_files_per_job,
                backoff,
                apply_jobs: if enqueue_jobs { backoff.max_attempts } else { 0 },
                counts,
            },
            handle,
        ))
    }

    /// Writer counters.
    pub fn stats(&self) -> &WriterStats {
        &self.stats
    }

    /// Messages waiting in the queue.
    #[cfg(test)]
    pub(crate) fn queued(&self) -> usize {
        self.tx.len()
    }

    /// Run an arbitrary write closure on the writer thread, in its own
    /// savepoint, batched with other commands. The escape hatch for writes
    /// this crate has no typed command for.
    pub fn exec<T, F>(&self, f: F) -> Request<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        // It may touch anything: cached file counts are recomputed.
        self.exec_inner(f, true)
    }

    /// [`Writer::exec`] for commands that change neither `file` rows nor
    /// which items exist (user state, art, jobs, metadata): cached library
    /// counts stay valid.
    fn exec_quiet<T, F>(&self, f: F) -> Request<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        self.exec_inner(f, false)
    }

    fn exec_inner<T, F>(&self, f: F, files: bool) -> Request<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let (rtx, rrx) = flume::bounded(1);
        let counts = files.then(|| self.counts.clone());
        let job: JobFn = Box::new(move |conn: &Connection| -> Finish {
            let res = in_savepoint(conn, f);
            Box::new(move |commit_err: Option<&str>| {
                // After the commit: a reader that recounts now sees it.
                if let Some(c) = counts {
                    c.invalidate();
                }
                let out = match (res, commit_err) {
                    (Ok(v), None) => Ok(v),
                    (Ok(_), Some(e)) => Err(CatalogError::CommitFailed(e.to_string())),
                    (Err(e), _) => Err(e),
                };
                let _ = rtx.send(out);
            })
        });
        Request {
            tx: self.tx.clone(),
            msgs: vec![Msg::Job(job)],
            rx: rrx,
        }
    }

    /// A scan is about to write a lot: give the writer its larger page
    /// cache ([`WriterConfig::bulk_cache_kib`]). Nested calls are counted;
    /// pair each with [`Writer::end_bulk`]. Not a transaction and not a
    /// [`WriterStats::commands`] entry.
    pub fn begin_bulk(&self) -> Request<()> {
        self.bulk(true)
    }

    /// The last bulk writer is done: shrink the page cache back to
    /// [`crate::CatalogConfig::writer_cache_kib`], release the memory, and
    /// checkpoint with `TRUNCATE`, so an idle server neither holds a
    /// scan-sized cache nor a 64 MB WAL file. The checkpoint gives up at
    /// once if a reader holds an old snapshot (the idle passive checkpoint
    /// and `journal_size_limit` catch up later).
    pub fn end_bulk(&self) -> Request<()> {
        self.bulk(false)
    }

    fn bulk(&self, on: bool) -> Request<()> {
        let (rtx, rrx) = flume::bounded(1);
        Request {
            tx: self.tx.clone(),
            msgs: vec![Msg::Bulk(on, rtx)],
            rx: rrx,
        }
    }

    /// Commit whatever is pending and reply once it is durable in the WAL.
    pub fn flush(&self) -> Request<()> {
        let (rtx, rrx) = flume::bounded(1);
        Request {
            tx: self.tx.clone(),
            msgs: vec![Msg::Flush(rtx)],
            rx: rrx,
        }
    }

    /// Ask the thread to commit, checkpoint, optimize and exit. Other
    /// handles then get [`CatalogError::WriterGone`].
    pub fn shutdown(&self) -> Request<()> {
        let (rtx, rrx) = flume::bounded(1);
        Request {
            tx: self.tx.clone(),
            msgs: vec![Msg::Shutdown(rtx)],
            rx: rrx,
        }
    }

    // ---- libraries and roots ----------------------------------------------

    /// Create a library and its roots. Roots that overlap (the same folder,
    /// or one inside another) are refused with
    /// [`CatalogError::OverlappingRoots`].
    pub fn create_library(&self, req: CreateLibrary) -> Request<Library> {
        self.exec(move |c| {
            let paths: Vec<String> = req.roots.iter().map(|r| normalize_root_path(r)).collect();
            for (i, a) in paths.iter().enumerate() {
                if let Some(b) = paths[..i].iter().find(|b| roots_overlap(a, b)) {
                    return Err(CatalogError::OverlappingRoots {
                        root: a.clone(),
                        other: b.clone(),
                    });
                }
            }
            let existing = all_roots(c)?;
            for a in &paths {
                if let Some(e) = in_other_library(a, &existing, None) {
                    return Err(e);
                }
            }
            let now = now_ms();
            c.prepare_cached("INSERT INTO library(name, kind, created_at) VALUES (?1, ?2, ?3)")?
                .execute(params![req.name, library_kind_str(req.kind), now])?;
            let id = c.last_insert_rowid();
            let mut roots = Vec::new();
            for r in &req.roots {
                let path = normalize_root_path(r);
                c.prepare_cached("INSERT INTO root(library_id, path, created_at) VALUES (?1, ?2, ?3)")?
                    .execute(params![id, path, now])?;
                // Not scanned yet: assumed reachable, never confirmed.
                roots.push(flux_core::api::RootStatus {
                    path,
                    available: true,
                    last_seen: None,
                });
            }
            Ok(Library {
                id,
                name: req.name,
                kind: req.kind,
                roots,
                item_count: 0,
                scan: crate::read::idle_status(),
            })
        })
    }

    /// Add a root folder to a library; returns the root id. A folder that
    /// overlaps one of the library's roots is refused with
    /// [`CatalogError::OverlappingRoots`].
    pub fn add_root(&self, library_id: Id, path: String, volume_uuid: Option<String>) -> Request<Id> {
        self.exec(move |c| {
            let path = normalize_root_path(&path);
            let existing = all_roots(c)?;
            if let Some((other, ..)) = existing
                .iter()
                .find(|(o, lib, _)| *lib == library_id && roots_overlap(&path, o))
            {
                return Err(CatalogError::OverlappingRoots {
                    root: path,
                    other: other.clone(),
                });
            }
            if let Some(e) = in_other_library(&path, &existing, Some(library_id)) {
                return Err(e);
            }
            c.prepare_cached(
                "INSERT INTO root(library_id, path, volume_uuid, created_at) VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![library_id, path, volume_uuid, now_ms()])?;
            Ok(c.last_insert_rowid())
        })
    }

    /// Delete a library and everything in it (explicit user action only).
    /// The search index is rebuilt afterwards (cascaded deletes bypass it).
    pub fn delete_library(&self, library_id: Id) -> Request<bool> {
        self.exec(move |c| {
            let deleted = c.execute("DELETE FROM library WHERE id = ?1", [library_id])? > 0;
            if deleted {
                crate::fts::rebuild(c)?;
            }
            Ok(deleted)
        })
    }

    /// Rebuild the title search index from the items (repair tool).
    /// Returns the number of indexed items.
    pub fn rebuild_search_index(&self) -> Request<u64> {
        self.exec_quiet(crate::fts::rebuild)
    }

    /// Persist a root's change-source cursor (also possible inside a [`ScanBatch`]).
    pub fn save_cursor(&self, root_id: Id, cursor: Option<Vec<u8>>) -> Request<()> {
        self.exec_quiet(move |c| {
            c.prepare_cached("UPDATE root SET cursor = ?2 WHERE id = ?1 AND cursor IS NOT ?2")?
                .execute(params![root_id, cursor])?;
            Ok(())
        })
    }

    /// Record scan start/finish/error for a root (fields left `None` are kept).
    pub fn record_scan(&self, root_id: Id, rec: ScanRecord) -> Request<()> {
        self.exec_quiet(move |c| {
            c.prepare_cached(
                "UPDATE root SET last_scan_started_at = COALESCE(?2, last_scan_started_at),
                                 last_scan_finished_at = COALESCE(?3, last_scan_finished_at),
                                 last_scan_error = ?4
                 WHERE id = ?1",
            )?
            .execute(params![root_id, rec.started_at, rec.finished_at, rec.error])?;
            Ok(())
        })
    }

    /// Mark a root (un)reachable. Never deletes: see
    /// [`crate::scan_support`]. Items stay listed; their assets report
    /// `available = false`.
    pub fn set_root_availability(&self, root_id: Id, available: bool) -> Request<AvailabilityChange> {
        self.exec(move |c| scan_support::set_root_availability(c, root_id, available))
    }

    // ---- scanning -----------------------------------------------------------

    /// Apply a scanner batch atomically (files, dirs, cursor, and the
    /// assets/items the grouping rules derive from them).
    ///
    /// An empty batch returns immediately without touching the writer, so a
    /// zero-change rescan writes nothing.
    ///
    /// A batch with more than [`WriterConfig::max_files_per_job`] upserts is
    /// split into consecutive jobs (each atomic on its own): the first also
    /// carries the moves, missing files and dirs, the last carries the
    /// cursor. If any part fails, the reply is
    /// [`CatalogError::BatchFailed`] and the cursor is **not** saved, so the
    /// scanner simply rescans that range (upserts are idempotent).
    pub fn apply(&self, batch: ScanBatch) -> Request<ApplyOutcome> {
        if batch.is_empty() {
            let (rtx, rrx) = flume::bounded(1);
            let _ = rtx.send(Ok(ApplyOutcome::default()));
            // Nothing is sent to the writer: the reply is already filled.
            return Request {
                tx: self.tx.clone(),
                msgs: Vec::new(),
                rx: rrx,
            };
        }
        let grace = self.tombstone_grace_ms;
        let max_attempts = self.apply_jobs;
        if batch.upserts.len() <= self.max_files_per_job {
            return self.exec(move |c| scan_support::apply_batch(c, &batch, grace, max_attempts));
        }
        let ScanBatch {
            root_id,
            upserts,
            moves,
            missing,
            dirs,
            removed_dirs,
            cursor,
            expect,
        } = batch;
        let mut parts: Vec<ScanBatch> = Vec::new();
        let mut it = upserts.into_iter().peekable();
        while it.peek().is_some() {
            let chunk = it.by_ref().take(self.max_files_per_job).collect();
            parts.push(ScanBatch {
                root_id,
                upserts: chunk,
                ..Default::default()
            });
        }
        if let Some(first) = parts.first_mut() {
            first.moves = moves;
            first.missing = missing;
            first.dirs = dirs;
            first.removed_dirs = removed_dirs;
            first.expect = expect;
        }
        if let Some(last) = parts.last_mut() {
            last.cursor = cursor;
        }
        struct Split {
            out: ApplyOutcome,
            error: Option<String>,
        }
        fn lock(m: &std::sync::Mutex<Split>) -> std::sync::MutexGuard<'_, Split> {
            m.lock().unwrap_or_else(|e| e.into_inner())
        }
        let state = Arc::new(std::sync::Mutex::new(Split {
            out: ApplyOutcome::default(),
            error: None,
        }));
        let (rtx, rrx) = flume::bounded(1);
        let n = parts.len();
        let mut msgs = Vec::with_capacity(n);
        for (i, mut part) in parts.into_iter().enumerate() {
            let is_last = i + 1 == n;
            let state = state.clone();
            let rtx = rtx.clone();
            let counts = self.counts.clone();
            let job: JobFn = Box::new(move |conn: &Connection| -> Finish {
                if is_last && lock(&state).error.is_some() {
                    part.cursor = None; // never claim coverage of a failed range
                }
                match in_savepoint(conn, |c| scan_support::apply_batch(c, &part, grace, max_attempts)) {
                    Ok(o) => lock(&state).out.merge(o),
                    Err(e) => {
                        lock(&state).error.get_or_insert_with(|| e.to_string());
                    }
                }
                Box::new(move |commit_err: Option<&str>| {
                    counts.invalidate();
                    let mut s = lock(&state);
                    if let Some(e) = commit_err {
                        s.error.get_or_insert_with(|| format!("commit failed: {e}"));
                    }
                    if is_last {
                        let res = match s.error.take() {
                            Some(e) => Err(CatalogError::BatchFailed(e)),
                            None => Ok(std::mem::take(&mut s.out)),
                        };
                        let _ = rtx.send(res);
                    }
                })
            });
            msgs.push(Msg::Job(job));
        }
        Request {
            tx: self.tx.clone(),
            msgs,
            rx: rrx,
        }
    }

    /// Fingerprints flux-scan computed after the files were cataloged (it
    /// defers the tail read so new items are browsable sooner). Each row is
    /// found by `(root, rel_path)` and updated only while its size and mtime
    /// are the ones fingerprinted. A file whose content matches a tombstone
    /// is merged into it, as an upsert with the fingerprint would have
    /// re-attached: the tombstone's file id, asset and every user's state
    /// survive, at the new path (`files_reattached`). Among tombstones with
    /// the same content, the one with the same file name wins, then one in a
    /// same-named folder, then the most recent.
    pub fn attach_fingerprints(&self, root_id: Id, fps: Vec<FileFingerprint>) -> Request<ApplyOutcome> {
        if fps.is_empty() {
            let (rtx, rrx) = flume::bounded(1);
            let _ = rtx.send(Ok(ApplyOutcome::default()));
            return Request {
                tx: self.tx.clone(),
                msgs: Vec::new(),
                rx: rrx,
            };
        }
        let grace = self.tombstone_grace_ms;
        let max_attempts = self.apply_jobs;
        self.exec(move |c| scan_support::attach_fingerprints(c, root_id, &fps, grace, max_attempts))
    }

    /// A scan's subtitle files of a root ([`crate::sidecar::sync`]):
    /// `upserts` new or changed, `removed` (root-relative paths) proven
    /// gone. Returns the rows changed.
    pub fn sync_subtitle_files(
        &self,
        root_id: Id,
        upserts: Vec<crate::sidecar::SubtitleFileRow>,
        removed: Vec<String>,
    ) -> Request<u64> {
        self.exec_quiet(move |c| crate::sidecar::sync(c, root_id, &upserts, &removed))
    }

    /// A scan's metadata files of a root ([`crate::metafiles::sync`]):
    /// `upserts` new or changed (with what flux read from them), `removed`
    /// proven gone. What they say is applied to the items they describe.
    pub fn sync_meta_files(
        &self,
        root_id: Id,
        upserts: Vec<crate::metafiles::MetaFileRow>,
        removed: Vec<String>,
    ) -> Request<ApplyOutcome> {
        let jobs = self.apply_jobs;
        self.exec(move |c| crate::metafiles::sync(c, root_id, &upserts, &removed, now_ms(), jobs))
    }

    /// Delete tombstoned files older than their grace period, and items left
    /// with nothing playable. `now_ms` is injectable for tests.
    pub fn purge_tombstones(&self, now_ms: i64) -> Request<PurgeOutcome> {
        self.exec(move |c| scan_support::purge_tombstones(c, now_ms))
    }

    /// Cache an asset's playback index (flux-hls bytes, opaque here). It is
    /// served by [`crate::read::asset_play_index`] only while the file's
    /// size and mtime equal `index.source_size` / `source_mtime_ns` (the
    /// stat the index was built from), and dropped when a scan sees the file
    /// change.
    pub fn put_asset_index(&self, asset_id: Id, index: PlayIndex) -> Request<()> {
        self.exec_quiet(move |c| {
            let exists: Option<i64> = c
                .prepare_cached("SELECT 1 FROM asset WHERE id = ?1")?
                .query_row([asset_id], |r| r.get(0))
                .optional()?;
            if exists.is_none() {
                return Err(CatalogError::NotFound(format!("asset {asset_id}")));
            }
            c.prepare_cached(
                "INSERT INTO asset_index(asset_id, format, version, source_size, source_mtime_ns, bytes, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(asset_id) DO UPDATE SET format = excluded.format, version = excluded.version,
                    source_size = excluded.source_size, source_mtime_ns = excluded.source_mtime_ns,
                    bytes = excluded.bytes, created_at = excluded.created_at",
            )?
            .execute(params![
                asset_id,
                index.format,
                index.version,
                clamp_i64(index.source_size),
                index.source_mtime_ns,
                index.bytes,
                now_ms()
            ])?;
            Ok(())
        })
    }

    /// Forget an asset's cached playback index (e.g. it failed to decode).
    pub fn clear_asset_index(&self, asset_id: Id) -> Request<bool> {
        self.exec_quiet(move |c| {
            Ok(c.prepare_cached("DELETE FROM asset_index WHERE asset_id = ?1")?
                .execute([asset_id])?
                > 0)
        })
    }

    // ---- episode re-keying and merging ---------------------------------------

    /// Merge top-level work `from` into `into` (same kind and library):
    /// children merge by season/episode/extra identity, assets, art, folder
    /// claims and every user's state move over, `from` is deleted. For works
    /// whose provider ids agree ([`crate::merge::works_sharing_ids`]).
    pub fn merge_items(&self, from: Id, into: Id) -> Request<RekeyOutcome> {
        let jobs = self.apply_jobs;
        self.exec(move |c| crate::merge::merge_items(c, from, into, now_ms(), jobs))
    }

    /// Re-place episode assets (anime absolute numbering mapped by
    /// flux-meta, or a manual fix), carrying every user's state along; see
    /// [`crate::rekey`]. All entries apply atomically.
    pub fn rekey_episodes(&self, list: Vec<EpisodeRekey>) -> Request<RekeyOutcome> {
        let max_attempts = self.apply_jobs;
        self.exec(move |c| crate::rekey::rekey_episodes(c, &list, now_ms(), max_attempts))
    }

    // ---- metadata -----------------------------------------------------------

    /// Apply provider/matcher metadata to an item. Items the user `locked`
    /// are left alone (returns `false`), and so is each field a person or
    /// an agent set ([`crate::fields`]). A movie or show whose title or
    /// year changes gets its art job again: its frame poster carries them.
    pub fn update_item_metadata(&self, item_id: Id, m: ItemMetadata) -> Request<bool> {
        let jobs = self.apply_jobs;
        self.exec_quiet(move |c| {
            let mut m = m;
            type Head = (String, String, Option<i64>, Option<Id>);
            let head = |c: &Connection| -> Result<Option<Head>> {
                Ok(c.prepare_cached("SELECT match_state, title, year, parent_id FROM item WHERE id = ?1")?
                    .query_row([item_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                    .optional()?)
            };
            let before = head(c)?;
            match before.as_ref().map(|h| h.0.as_str()) {
                None => return Err(CatalogError::NotFound(format!("item {item_id}"))),
                Some("locked") => return Ok(false),
                _ => {}
            }
            keep_locked_fields(c, item_id, &mut m)?;
            let old_doc = crate::fts::doc(c, item_id)?;
            let genres = m.genres.as_ref().map(serde_json::to_string).transpose()?;
            let evidence = m.match_evidence.as_ref().map(serde_json::to_string).transpose()?;
            let others = m
                .other_ids
                .as_ref()
                .map(|v| serde_json::to_string(&v.iter().cloned().collect::<std::collections::BTreeMap<_, _>>()))
                .transpose()?;
            let sort = m.title.as_deref().map(crate::group::sort_title);
            c.prepare_cached(
                "UPDATE item SET
                    title = COALESCE(?2, title), sort_title = COALESCE(?3, sort_title),
                    original_title = COALESCE(?4, original_title), year = COALESCE(?5, year),
                    overview = COALESCE(?6, overview), genres = COALESCE(?7, genres),
                    tmdb_id = COALESCE(?8, tmdb_id), tvdb_id = COALESCE(?9, tvdb_id), imdb_id = COALESCE(?10, imdb_id),
                    external_ids = COALESCE(?11, external_ids),
                    match_confidence = COALESCE(?12, match_confidence), match_evidence = COALESCE(?13, match_evidence),
                    match_state = CASE WHEN ?14 THEN 'matched' ELSE match_state END,
                    updated_at = ?15
                 WHERE id = ?1",
            )?
            .execute(params![
                item_id,
                m.title,
                sort,
                m.original_title,
                m.year,
                m.overview,
                genres,
                m.tmdb_id.map(clamp_i64),
                m.tvdb_id.map(clamp_i64),
                m.imdb_id,
                others,
                m.match_confidence.map(f64::from),
                evidence,
                m.matched,
                now_ms(),
            ])?;
            if let Some(old) = old_doc {
                crate::fts::update(c, item_id, &old)?;
            }
            if let Some(p) = m.provider.as_deref() {
                let row = crate::fields::FieldRow {
                    source: crate::fields::PROVIDER.into(),
                    provider: Some(p.to_string()),
                    agent: None,
                    actor: None,
                    changeset: None,
                    sources: Vec::new(),
                    at: now_ms(),
                };
                let set = [
                    ("title", m.title.is_some()),
                    ("original_title", m.original_title.is_some()),
                    ("year", m.year.is_some()),
                    ("overview", m.overview.is_some()),
                    ("genres", m.genres.is_some()),
                    ("ids.tmdb", m.tmdb_id.is_some()),
                    ("ids.tvdb", m.tvdb_id.is_some()),
                    ("ids.imdb", m.imdb_id.is_some()),
                ];
                for (field, _) in set.iter().filter(|(_, set)| *set) {
                    crate::fields::put_source(c, item_id, field, Some(&row))?;
                }
            }
            if let (Some(b), Some(a)) = (before, head(c)?) {
                let renamed = (&b.1, b.2) != (&a.1, a.2);
                if renamed && a.3.is_none() && jobs > 0 {
                    crate::jobs::rearm_art(c, item_id, now_ms(), jobs)?;
                }
            }
            Ok(true)
        })
    }

    /// Add artwork to an item; returns the art id. Idempotent: the same
    /// image (item, kind, source, url or path) updates its row instead of
    /// adding one, so a re-run art job adds nothing. Adding never changes a
    /// user's pin; the best image is chosen at read time
    /// ([`crate::read::best_art`]).
    pub fn add_art(&self, item_id: Id, art: NewArt) -> Request<Id> {
        self.exec_quiet(move |c| {
            let existing: Option<Id> = c
                .prepare_cached(
                    "SELECT id FROM art WHERE item_id = ?1 AND kind = ?2 AND source = ?3
                       AND url IS ?4 AND path IS ?5 ORDER BY id LIMIT 1",
                )?
                .query_row(
                    params![item_id, art.kind.as_str(), art_source_str(art.source), art.url, art.path],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = existing {
                c.prepare_cached(
                    "UPDATE art SET width = ?2, height = ?3, blurhash = ?4, attribution = ?5, generator = ?6,
                                    gen_version = ?7, gen_title = ?8
                     WHERE id = ?1 AND (width IS NOT ?2 OR height IS NOT ?3 OR blurhash IS NOT ?4
                                        OR attribution IS NOT ?5 OR generator IS NOT ?6
                                        OR gen_version IS NOT ?7 OR gen_title IS NOT ?8)",
                )?
                .execute(params![
                    id,
                    art.width,
                    art.height,
                    art.blurhash,
                    art.attribution,
                    art.generator,
                    art.gen_version,
                    art.gen_title
                ])?;
                return Ok(id);
            }
            c.prepare_cached(
                "INSERT INTO art(item_id, kind, source, path, url, width, height, blurhash, generated, created_at,
                                 attribution, generator, gen_version, gen_title)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            )?
            .execute(params![
                item_id,
                art.kind.as_str(),
                art_source_str(art.source),
                art.path,
                art.url,
                art.width,
                art.height,
                art.blurhash,
                art.source == flux_core::api::ArtSource::Generated,
                now_ms(),
                art.attribution,
                art.generator,
                art.gen_version,
                art.gen_title,
            ])?;
            Ok(c.last_insert_rowid())
        })
    }

    /// Pin an image: it wins over every other image of its kind on its item
    /// until unpinned. Returns `false` if the art id doesn't exist.
    pub fn pin_art(&self, art_id: Id) -> Request<bool> {
        self.exec_quiet(move |c| {
            let row: Option<(Id, String)> = c
                .prepare_cached("SELECT item_id, kind FROM art WHERE id = ?1")?
                .query_row([art_id], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            let Some((item, kind)) = row else { return Ok(false) };
            c.prepare_cached("UPDATE art SET pinned = (id = ?3) WHERE item_id = ?1 AND kind = ?2 AND pinned IS NOT (id = ?3)")?
                .execute(params![item, kind, art_id])?;
            Ok(true)
        })
    }

    /// Drop the pin of an item's `kind` images (back to automatic choice).
    pub fn unpin_art(&self, item_id: Id, kind: ArtKind) -> Request<u64> {
        self.exec_quiet(move |c| {
            Ok(
                c.prepare_cached("UPDATE art SET pinned = 0 WHERE item_id = ?1 AND kind = ?2 AND pinned = 1")?
                    .execute(params![item_id, kind.as_str()])? as u64,
            )
        })
    }

    /// Delete an image row. Returns whether it existed.
    pub fn remove_art(&self, art_id: Id) -> Request<bool> {
        self.exec_quiet(move |c| {
            Ok(c.prepare_cached("DELETE FROM art WHERE id = ?1")?
                .execute([art_id])?
                > 0)
        })
    }

    /// Delete work `work`'s stale images that have a fresh replacement now
    /// ([`crate::read::replaced_stale_art`] for art `version`): what its art
    /// job leaves behind once it made new ones. The rows are found and
    /// deleted in one write, so an image the user pins meanwhile
    /// ([`Writer::pin_art`]) is kept; a pinned image, and provider or local
    /// art, is never deleted.
    ///
    /// Returns each deleted row's id with its stored path, for the caller
    /// to delete the file it owns. The path is `None` when the row had none,
    /// or when another row still refers to the same file (two legacy rows
    /// of different data dirs can share one `art/<file>` after v6).
    pub fn remove_replaced_art(&self, work: Id, version: u32) -> Request<Vec<(Id, Option<String>)>> {
        self.exec_quiet(move |c| {
            let mut gone = Vec::new();
            for s in crate::read::replaced_stale_art(c, version, work)? {
                let path: Option<Option<String>> = c
                    .prepare_cached(
                        "DELETE FROM art WHERE id = ?1 AND pinned = 0 AND source IN ('frame', 'generated')
                         RETURNING path",
                    )?
                    .query_row([s.art_id], |r| r.get(0))
                    .optional()?;
                if let Some(path) = path {
                    gone.push((s.art_id, path));
                }
            }
            for (_, path) in &mut gone {
                let shared: bool = match path.as_deref() {
                    Some(p) => c
                        .prepare_cached("SELECT EXISTS (SELECT 1 FROM art WHERE path = ?1)")?
                        .query_row([p], |r| r.get(0))?,
                    None => false,
                };
                if shared {
                    *path = None;
                }
            }
            Ok(gone)
        })
    }

    /// Queue an art job for every movie or show whose own images are stale
    /// for art `version` (see [`crate::read::stale_art`]); the jobs make them
    /// again. Returns how many works got a job.
    pub fn refresh_stale_art(&self, version: u32) -> Request<u64> {
        let max = self.backoff.max_attempts;
        self.exec_quiet(move |c| crate::jobs::refresh_stale_art(c, version, now_ms(), max))
    }

    /// Delete the items no file is linked to anywhere below them, and the
    /// containers they leave empty: what an upgrade by an earlier build
    /// left when a re-parse moved every file of a work elsewhere (hidden as
    /// `missing` since). Their images, jobs and user state go with them
    /// (the state was copied to where the files went). Returns the deleted
    /// ids.
    pub fn purge_empty_items(&self) -> Request<Vec<Id>> {
        self.exec(move |c| {
            let (deleted, survivors) = crate::merge::purge_empty_items(c)?;
            if !survivors.is_empty() {
                let t = scan_support::Touched {
                    parents: survivors,
                    ..Default::default()
                };
                scan_support::recompute(c, now_ms(), &t, &Default::default())?;
            }
            Ok(deleted.into_iter().collect())
        })
    }

    // ---- changesets and lenses (agents) -----------------------------------------

    /// Make `edits` a changeset: applied now, or (`propose`) proposed for a
    /// person to decide; see [`crate::changes`]. Also returns what it
    /// planned per item (changed, unchanged, refused fields).
    pub fn commit_edit(
        &self,
        actor: Actor,
        summary: String,
        edits: Vec<crate::changes::ItemEdit>,
        force: bool,
        propose: bool,
    ) -> Request<(Committed, Vec<PlannedItem>)> {
        let jobs = self.apply_jobs;
        self.exec_quiet(move |c| {
            crate::changes::commit_edit(c, &actor, &summary, &edits, force, propose, now_ms(), jobs)
        })
    }

    /// Apply a proposed changeset (`by` decides it). A structure change
    /// in it places files again, so library counts are recomputed.
    pub fn apply_proposal(&self, changeset: Id, by: String) -> Request<Committed> {
        let jobs = self.apply_jobs;
        self.exec(move |c| crate::changes::apply_proposal(c, changeset, &by, now_ms(), jobs))
    }

    /// Fix what files are (`files.fix`, `items.merge`) as one changeset:
    /// applied now, or (`propose`) proposed; a `dry_run` is worked out and
    /// rolled back. Returns what changed for each file, as a reader that
    /// may not see `hide` names the items. See [`crate::fixes`].
    #[allow(clippy::too_many_arguments)]
    pub fn commit_fixes(
        &self,
        actor: Actor,
        summary: String,
        edits: Vec<crate::fixes::FixEdit>,
        propose: bool,
        dry_run: bool,
        hide: Vec<String>,
    ) -> Request<(Committed, Vec<crate::fixes::FixOutcome>)> {
        let jobs = self.apply_jobs;
        self.exec(move |c| {
            crate::fixes::commit(
                c,
                &actor,
                &summary,
                &edits,
                propose,
                dry_run,
                &hide,
                now_ms(),
                jobs,
            )
        })
    }

    /// Turn a proposed changeset down.
    pub fn reject_proposal(&self, changeset: Id, by: String) -> Request<Committed> {
        self.exec_quiet(move |c| crate::changes::reject_proposal(c, changeset, &by, now_ms()))
    }

    /// Make or change a collection ([`crate::collections::put`]).
    pub fn put_collection(&self, p: crate::collections::Put) -> Request<crate::collections::Row> {
        self.exec_quiet(move |c| crate::collections::put(c, &p, now_ms()))
    }

    /// Forget collection `id` of `owner`.
    pub fn remove_collection(&self, owner: Id, id: Id) -> Request<bool> {
        self.exec_quiet(move |c| crate::collections::remove(c, owner, id))
    }

    /// Keep a file plan (`files.plan`) as a proposed changeset; see
    /// [`crate::organize`].
    pub fn propose_plan(
        &self,
        actor: Actor,
        summary: String,
        steps: Vec<crate::organize::Step>,
    ) -> Request<Committed> {
        self.exec_quiet(move |c| crate::organize::propose(c, &actor, &summary, &steps, now_ms()))
    }

    /// Record what moving a plan's files did (or, with `undo`, putting them
    /// back): the catalog follows the files, and moved videos are placed
    /// again by their new names. `by` decided it.
    pub fn record_plan(
        &self,
        changeset: Id,
        by: String,
        done: Vec<crate::organize::Done>,
        undo: bool,
    ) -> Request<Committed> {
        let jobs = self.apply_jobs;
        self.exec(move |c| crate::organize::record(c, changeset, &by, &done, undo, now_ms(), jobs))
    }

    /// Undo an applied changeset, where its fields still hold what it set.
    pub fn revert_changeset(&self, changeset: Id, by: String) -> Request<Committed> {
        let jobs = self.apply_jobs;
        self.exec(move |c| crate::changes::revert(c, changeset, &by, now_ms(), jobs))
    }

    /// Add an image a person or an agent chose (`custom`, or `generated`
    /// for AI art), found at `found_at`, and journal it as a changeset that
    /// also pins it when `pin`. Returns the new image's id.
    #[allow(clippy::too_many_arguments)]
    pub fn add_chosen_art(
        &self,
        actor: Actor,
        summary: String,
        item_id: Id,
        art: NewArt,
        found_at: Option<String>,
        pin: bool,
        sources: Vec<flux_core::agent::Link>,
    ) -> Request<(Id, Committed)> {
        self.exec_quiet(move |c| {
            let exists = c
                .prepare_cached("SELECT 1 FROM item WHERE id = ?1")?
                .query_row([item_id], |_| Ok(()))
                .optional()?
                .is_some();
            if !exists {
                return Err(CatalogError::NotFound(format!("item {item_id}")));
            }
            c.prepare_cached(
                "INSERT INTO art(item_id, kind, source, path, url, width, height, blurhash, generated, created_at,
                                 attribution, generator, found_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            )?
            .execute(params![
                item_id,
                art.kind.as_str(),
                art_source_str(art.source),
                art.path,
                art.url,
                art.width,
                art.height,
                art.blurhash,
                art.source == flux_core::api::ArtSource::Generated,
                now_ms(),
                art.attribution,
                art.generator,
                found_at,
            ])?;
            let id = c.last_insert_rowid();
            let kind = art.kind.as_str();
            let show = if pin {
                Some(id)
            } else {
                c.prepare_cached("SELECT id FROM art WHERE item_id = ?1 AND kind = ?2 AND pinned = 1")?
                    .query_row(params![item_id, kind], |r| r.get(0))
                    .optional()?
            };
            let out = crate::changes::commit_art(c, &actor, &summary, item_id, kind, Some(id), show, &sources, now_ms())?;
            Ok((id, out))
        })
    }

    /// Show image `art` for its item and kind (`None` with `item` and
    /// `kind`: back to flux's own choice), journaled as a changeset.
    pub fn choose_art(
        &self,
        actor: Actor,
        summary: String,
        item_id: Id,
        kind: ArtKind,
        art: Option<Id>,
    ) -> Request<Committed> {
        self.exec_quiet(move |c| {
            if let Some(a) = art {
                let owner: Option<(Id, String)> = c
                    .prepare_cached("SELECT item_id, kind FROM art WHERE id = ?1")?
                    .query_row([a], |r| Ok((r.get(0)?, r.get(1)?)))
                    .optional()?;
                match owner {
                    Some((i, k)) if i == item_id && k == kind.as_str() => {}
                    Some(_) => {
                        return Err(CatalogError::Invalid(format!(
                            "image {a} isn't a {} of item {item_id}",
                            kind.as_str()
                        )))
                    }
                    None => return Err(CatalogError::NotFound(format!("image {a}"))),
                }
            }
            crate::changes::commit_art(
                c,
                &actor,
                &summary,
                item_id,
                kind.as_str(),
                None,
                art,
                &[],
                now_ms(),
            )
        })
    }

    /// Keep a lens on the server for `owner` (a user id, or
    /// [`crate::lenses::EVERYONE`]).
    pub fn put_lens(
        &self,
        owner: Id,
        id: String,
        spec: flux_core::agent::LensSpec,
        added_by: String,
        agent: Option<String>,
    ) -> Request<flux_core::agent::StoredLens> {
        self.exec_quiet(move |c| {
            crate::lenses::put(c, owner, &id, &spec, &added_by, agent.as_deref(), now_ms())
        })
    }

    /// Forget a lens kept on the server.
    pub fn remove_lens(&self, owner: Id, id: String) -> Request<bool> {
        self.exec_quiet(move |c| crate::lenses::remove(c, owner, &id))
    }

    // ---- users and progress ---------------------------------------------------

    /// Find or create the user for a tailnet identity; updates the display
    /// name/role only when they changed. Returns the user id.
    pub fn ensure_user(&self, who: Identity) -> Request<Id> {
        self.exec_quiet(move |c| {
            c.prepare_cached(
                "INSERT INTO user(login_name, display_name, role, created_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(login_name) DO UPDATE SET display_name = excluded.display_name, role = excluded.role
                 WHERE display_name IS NOT excluded.display_name OR role IS NOT excluded.role",
            )?
            .execute(params![who.login_name, who.display_name, role_str(who.role), now_ms()])?;
            Ok(c.prepare_cached("SELECT id FROM user WHERE login_name = ?1")?
                .query_row([&who.login_name], |r| r.get(0))?)
        })
    }

    /// Record a playback position. Crossing 90 % of the duration marks the
    /// item watched, clears the resume point and counts a play (once per
    /// viewing); the other episodes of a multi-episode file are marked
    /// watched with it. A rewatch keeps `watched` and tracks the new position.
    pub fn set_progress(
        &self,
        user_id: Id,
        item_id: Id,
        asset_id: Option<Id>,
        position_ms: u64,
        duration_ms: u64,
    ) -> Request<Progress> {
        self.exec_quiet(move |c| put_progress(c, user_id, item_id, asset_id, position_ms, duration_ms))
    }

    /// Same as [`Writer::set_progress`] (the phase-B name).
    pub fn put_progress(
        &self,
        user_id: Id,
        item_id: Id,
        asset_id: Option<Id>,
        position_ms: u64,
        duration_ms: u64,
    ) -> Request<Progress> {
        self.exec_quiet(move |c| put_progress(c, user_id, item_id, asset_id, position_ms, duration_ms))
    }

    /// Mark an item (or every episode of a show/season) watched or unwatched.
    /// Returns the number of leaf items changed.
    pub fn set_watched(&self, user_id: Id, item_id: Id, watched: bool) -> Request<u64> {
        self.exec_quiet(move |c| set_watched(c, user_id, item_id, watched))
    }

    // ---- jobs -------------------------------------------------------------------

    /// Enqueue a job (idempotent by `(kind, key)`; see [`crate::jobs`]).
    pub fn enqueue_job(&self, spec: JobSpec) -> Request<Enqueued> {
        let max = self.backoff.max_attempts;
        self.exec_quiet(move |c| crate::jobs::enqueue(c, &spec, now_ms(), max))
    }

    /// Enqueue several jobs in one savepoint.
    pub fn enqueue_jobs(&self, specs: Vec<JobSpec>) -> Request<Vec<Enqueued>> {
        let max = self.backoff.max_attempts;
        self.exec_quiet(move |c| {
            let now = now_ms();
            specs
                .iter()
                .map(|s| crate::jobs::enqueue(c, s, now, max))
                .collect()
        })
    }

    /// Lease up to `limit` ready jobs (expired leases are settled first).
    /// The executor polls this; [`crate::jobs::next_job_due`] says when to
    /// poll next.
    pub fn claim_jobs(&self, req: ClaimJobs) -> Request<Vec<JobLease>> {
        let backoff = self.backoff;
        self.exec_quiet(move |c| crate::jobs::claim(c, &req, &backoff))
    }

    /// Finish a leased attempt: done, retry after backoff, or fail. A
    /// result for a lease that was lost (expired and re-claimed) is
    /// discarded ([`Finished::LeaseLost`]).
    pub fn finish_job(&self, job_id: Id, attempt: u32, result: JobResult) -> Request<Finished> {
        let backoff = self.backoff;
        self.exec_quiet(move |c| crate::jobs::finish(c, job_id, attempt, &result, now_ms(), &backoff))
    }

    /// Extend a held lease by `lease` from now. `false` if it was lost.
    pub fn extend_job_lease(&self, job_id: Id, attempt: u32, lease: Duration) -> Request<bool> {
        self.exec_quiet(move |c| crate::jobs::extend(c, job_id, attempt, lease, now_ms()))
    }

    /// Queue failed jobs (of `kind`, or all) again with fresh attempts.
    pub fn requeue_failed_jobs(&self, kind: Option<String>) -> Request<u64> {
        self.exec_quiet(move |c| crate::jobs::requeue_failed(c, kind.as_deref(), now_ms()))
    }

    /// The UI asked for art `item_id` doesn't have yet: run the art job of
    /// its work (and the match it waits for) next. `false` if no art job is
    /// queued for it. See [`crate::jobs`].
    pub fn bump_art(&self, item_id: Id) -> Request<bool> {
        self.exec_quiet(move |c| crate::jobs::bump_art(c, item_id, now_ms()))
    }
}

/// A root as the catalog stores it: without a trailing separator, except
/// where that would change what it names (`/`, a volume root such as `D:\`
/// or `\\?\Volume{…}\`). A Windows path (see [`is_windows_path`]) is
/// spelled with `\` throughout, so the asset paths built from it
/// ([`crate::read`]) read one way.
fn normalize_root_path(p: &str) -> String {
    if is_windows_path(p) {
        let s = p.replace('/', "\\");
        let t = s.trim_end_matches('\\');
        return if names_a_volume_not_its_root(t) {
            format!("{t}\\")
        } else {
            t.to_string()
        };
    }
    let t = p.trim_end_matches('/');
    if t.is_empty() {
        "/".into()
    } else {
        t.to_string()
    }
}

/// A Windows path: it starts with a drive letter (`D:`) or `\\` (a share,
/// `\\server\share`, or a verbatim `\\?\` path). Decided by spelling, not
/// by the platform, so a catalog reads the same everywhere; absolute paths
/// of other platforms (`/…`) never look like one.
pub(crate) fn is_windows_path(p: &str) -> bool {
    let b = p.as_bytes();
    (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':') || p.starts_with("\\\\")
}

/// A Windows path (trailing `\` removed) that, without that `\`, names
/// something other than a volume's root folder: `D:` (drive D's current
/// folder), or a single component in the device namespace, `\\?\D:`,
/// `\\?\Volume{…}`, `\\.\D:` (the volume device itself, which can't be
/// listed: "Incorrect function").
fn names_a_volume_not_its_root(t: &str) -> bool {
    if t.is_empty() || t.ends_with(':') {
        return true;
    }
    t.strip_prefix("\\\\?\\")
        .or_else(|| t.strip_prefix("\\\\.\\"))
        .is_some_and(|rest| !rest.is_empty() && !rest.contains('\\'))
}

/// The same folder, or one inside the other (see [`root_overlap`]).
pub(crate) fn roots_overlap(a: &str, b: &str) -> bool {
    root_overlap(a, b).is_some()
}

/// How root `a` relates to root `b`: the same folder, inside it or around
/// it; `None` when it is neither. Compared as written, by whole path
/// components (`/media/Movies2` is not inside `/media/Movies`; `.`,
/// duplicate and trailing separators are ignored), and without regard to
/// letter case on Windows and macOS, whose volumes usually ignore it.
/// flux-server compares the folders' real paths too (symlinks, `..`).
/// Drop from `m` what a person or an agent set on `item` (an id kept in
/// `external_ids` stays as it is).
fn keep_locked_fields(c: &Connection, item: Id, m: &mut ItemMetadata) -> Result<()> {
    let locked = crate::fields::locked(c, item)?;
    if locked.is_empty() {
        return Ok(());
    }
    let has = |f: &str| locked.contains(f);
    if has("title") {
        m.title = None;
    }
    if has("original_title") {
        m.original_title = None;
    }
    if has("year") {
        m.year = None;
    }
    if has("overview") {
        m.overview = None;
    }
    if has("genres") {
        m.genres = None;
    }
    if has("ids.tmdb") {
        m.tmdb_id = None;
    }
    if has("ids.tvdb") {
        m.tvdb_id = None;
    }
    if has("ids.imdb") {
        m.imdb_id = None;
    }
    if let Some(others) = m.other_ids.as_mut() {
        let current = crate::fields::read(c, item)?
            .map(|c| c.values)
            .unwrap_or_default();
        others.retain(|(k, _)| !has(&format!("ids.{k}")));
        for f in locked.iter().filter_map(|f| f.strip_prefix("ids.")) {
            if matches!(f, "tmdb" | "tvdb" | "imdb") {
                continue;
            }
            if let Some(v) = current
                .get(&format!("ids.{f}"))
                .and_then(serde_json::Value::as_str)
            {
                others.push((f.to_string(), v.to_string()));
            }
        }
    }
    Ok(())
}

pub fn root_overlap(a: &str, b: &str) -> Option<RootOverlap> {
    let fold = |s: &str| {
        if cfg!(any(windows, target_os = "macos")) {
            s.to_lowercase()
        } else {
            s.to_string()
        }
    };
    let (a, b) = (fold(a), fold(b));
    let (a, b) = (std::path::Path::new(&a), std::path::Path::new(&b));
    match (a.starts_with(b), b.starts_with(a)) {
        (true, true) => Some(RootOverlap::Same),
        (true, false) => Some(RootOverlap::Inside),
        (false, true) => Some(RootOverlap::Contains),
        (false, false) => None,
    }
}

/// Every root: `(path, library id, library name)`.
fn all_roots(c: &Connection) -> Result<Vec<(String, Id, String)>> {
    let mut stmt = c.prepare_cached(
        "SELECT r.path, r.library_id, l.name FROM root r JOIN library l ON l.id = r.library_id ORDER BY r.id",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// [`CatalogError::RootInOtherLibrary`] if `path` overlaps a root of a
/// library other than `own`.
fn in_other_library(path: &str, roots: &[(String, Id, String)], own: Option<Id>) -> Option<CatalogError> {
    roots
        .iter()
        .filter(|(_, lib, _)| Some(*lib) != own)
        .find_map(|(other, _, name)| {
            root_overlap(path, other).map(|how| CatalogError::RootInOtherLibrary {
                root: path.to_string(),
                other: other.clone(),
                library: name.clone(),
                how,
            })
        })
}

/// ≥ 90 % watched counts as finished (Plex/Jellyfin use 90–95 %).
pub(crate) fn is_watched(position_ms: u64, duration_ms: u64) -> bool {
    duration_ms > 0 && position_ms.saturating_mul(10) >= duration_ms.saturating_mul(9)
}

fn put_progress(
    c: &Connection,
    user_id: Id,
    item_id: Id,
    asset_id: Option<Id>,
    position_ms: u64,
    duration_ms: u64,
) -> Result<Progress> {
    let now = now_ms();
    // Fall back to the asset's (or item's first asset's) probed duration.
    let duration = if duration_ms > 0 {
        duration_ms
    } else {
        c.prepare_cached(
            "SELECT a.duration_ms FROM asset a WHERE (?2 IS NOT NULL AND a.id = ?2)
                OR (?2 IS NULL AND a.id IN (SELECT asset_id FROM asset_item WHERE item_id = ?1))
             ORDER BY a.id LIMIT 1",
        )?
        .query_row(params![item_id, asset_id], |r| r.get::<_, Option<i64>>(0))
        .optional()?
        .flatten()
        .map(|d| d.max(0) as u64)
        .unwrap_or(0)
    };
    let prev: Option<(i64, bool)> = c
        .prepare_cached("SELECT position_ms, watched FROM user_state WHERE user_id = ?1 AND item_id = ?2")?
        .query_row(params![user_id, item_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let finished = is_watched(position_ms, duration);
    // Count a play once per viewing: when crossing 90 % from a resumable
    // position (or from never-watched).
    let new_play = finished && prev.is_none_or(|(pos, watched)| pos > 0 || !watched);
    let stored_pos = if finished { 0 } else { position_ms };
    c.prepare_cached(
        "INSERT INTO user_state(user_id, item_id, asset_id, position_ms, duration_ms, watched, play_count, last_played_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
         ON CONFLICT(user_id, item_id) DO UPDATE SET
            asset_id = COALESCE(excluded.asset_id, asset_id), position_ms = excluded.position_ms,
            duration_ms = excluded.duration_ms, watched = MAX(watched, excluded.watched),
            play_count = play_count + excluded.play_count, last_played_at = excluded.last_played_at,
            updated_at = excluded.updated_at",
    )?
    .execute(params![
        user_id,
        item_id,
        asset_id,
        clamp_i64(stored_pos),
        clamp_i64(duration),
        finished,
        i64::from(new_play),
        now
    ])?;
    if finished {
        // A multi-episode file (S01E02-E03) finishes every episode it holds.
        c.prepare_cached(
            "INSERT INTO user_state(user_id, item_id, position_ms, watched, play_count, last_played_at, updated_at)
             SELECT DISTINCT ?1, ai2.item_id, 0, 1, 1, ?3, ?3
               FROM asset_item ai JOIN asset_item ai2 ON ai2.asset_id = ai.asset_id
               JOIN item e ON e.id = ai2.item_id
              WHERE ai.item_id = ?2 AND ai2.item_id <> ?2 AND e.kind = 'episode'
                AND (?4 IS NULL OR ai.asset_id = ?4)
             ON CONFLICT(user_id, item_id) DO UPDATE SET
                watched = 1, position_ms = 0, play_count = play_count + 1,
                last_played_at = excluded.last_played_at, updated_at = excluded.updated_at
             WHERE NOT watched",
        )?
        .execute(params![user_id, item_id, now, asset_id])?;
    }
    let watched: bool = c
        .prepare_cached("SELECT watched FROM user_state WHERE user_id = ?1 AND item_id = ?2")?
        .query_row(params![user_id, item_id], |r| r.get(0))?;
    Ok(Progress {
        position_ms: stored_pos,
        duration_ms: duration,
        watched,
        updated_at: now,
    })
}

fn set_watched(c: &Connection, user_id: Id, item_id: Id, watched: bool) -> Result<u64> {
    let now = now_ms();
    // The item itself if it is playable, else every episode below it.
    let leaves: Vec<Id> = {
        let mut stmt = c.prepare_cached(
            "WITH RECURSIVE sub(id, kind) AS (
                 SELECT id, kind FROM item WHERE id = ?1
                 UNION ALL SELECT i.id, i.kind FROM item i JOIN sub ON i.parent_id = sub.id
                 WHERE sub.kind IN ('show','season') AND i.kind <> 'extra')
             SELECT id FROM sub WHERE kind IN ('movie','episode','extra') AND id IN
                 (SELECT item_id FROM asset_item)",
        )?;
        let rows = stmt.query_map([item_id], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut changed = 0u64;
    for id in leaves {
        changed += c
            .prepare_cached(
                "INSERT INTO user_state(user_id, item_id, position_ms, watched, play_count, last_played_at, updated_at)
                 VALUES (?1, ?2, 0, ?3, ?4, CASE WHEN ?3 THEN ?5 END, ?5)
                 ON CONFLICT(user_id, item_id) DO UPDATE SET
                    position_ms = 0, watched = excluded.watched,
                    play_count = play_count + CASE WHEN excluded.watched AND NOT watched THEN 1 ELSE 0 END,
                    updated_at = excluded.updated_at
                 WHERE watched IS NOT excluded.watched OR position_ms <> 0",
            )?
            .execute(params![user_id, id, watched, i64::from(watched), now])? as u64;
    }
    Ok(changed)
}

fn writer_main(
    conn: Connection,
    rx: flume::Receiver<Msg>,
    cfg: WriterConfig,
    cache_kib: i64,
    stats: Arc<WriterStats>,
) {
    let mut dirty_rows: u64 = 0;
    // Nested bulk writers (scans) in progress.
    let mut bulk_depth: u32 = 0;
    let mut rows_since_optimize: u64 = 0;
    let mut last_optimize = Instant::now();
    loop {
        let first = if dirty_rows > 0 {
            match rx.recv_timeout(cfg.idle_maintenance) {
                Ok(m) => m,
                Err(flume::RecvTimeoutError::Timeout) => {
                    maintenance(
                        &conn,
                        &stats,
                        &cfg,
                        &mut rows_since_optimize,
                        &mut last_optimize,
                        false,
                    );
                    dirty_rows = 0;
                    continue;
                }
                Err(flume::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(m) => m,
                Err(_) => break,
            }
        };
        // A bulk switch needs no transaction (and must not open one).
        let first = match first {
            Msg::Bulk(on, reply) => {
                let _ = reply.send(bulk_mode(&conn, &cfg, cache_kib, &stats, &mut bulk_depth, on));
                continue;
            }
            m => m,
        };

        let began = match conn.execute_batch("BEGIN IMMEDIATE") {
            Ok(()) => true,
            Err(e) => {
                // Each job still runs atomically: its SAVEPOINT opens and
                // RELEASE commits a transaction of its own.
                tracing::error!(error = %e, "BEGIN failed; running jobs unbatched");
                false
            }
        };
        let start = Instant::now();
        let mut rows: u64 = 0;
        let mut finishes: Vec<Finish> = Vec::new();
        let mut flushes: Vec<flume::Sender<Result<()>>> = Vec::new();
        let mut shutdown: Option<flume::Sender<Result<()>>> = None;
        let mut bulks: Vec<(bool, flume::Sender<Result<()>>)> = Vec::new();
        let mut next = Some(first);
        while let Some(msg) = next.take() {
            match msg {
                Msg::Job(job) => {
                    let before = conn.total_changes();
                    finishes.push(job(&conn));
                    rows += conn.total_changes().saturating_sub(before);
                    stats.commands.fetch_add(1, Ordering::Relaxed);
                }
                Msg::Flush(reply) => {
                    flushes.push(reply);
                    break;
                }
                Msg::Shutdown(reply) => {
                    shutdown = Some(reply);
                    break;
                }
                Msg::Bulk(on, reply) => {
                    bulks.push((on, reply));
                    break;
                }
            }
            if rows >= cfg.max_rows || start.elapsed() >= cfg.max_latency {
                break;
            }
            // Group commit: keep draining while work is queued, commit as
            // soon as the queue is momentarily empty.
            next = rx.try_recv().ok();
        }
        let commit_err = if began {
            match conn.execute_batch("COMMIT") {
                Ok(()) => None,
                Err(e) => {
                    tracing::error!(error = %e, "catalog commit failed");
                    let _ = conn.execute_batch("ROLLBACK");
                    Some(e.to_string())
                }
            }
        } else {
            None
        };
        stats.transactions.fetch_add(1, Ordering::Relaxed);
        stats.rows.fetch_add(rows, Ordering::Relaxed);
        if commit_err.is_some() {
            stats
                .failed_commands
                .fetch_add(finishes.len() as u64, Ordering::Relaxed);
        }
        for f in finishes {
            f(commit_err.as_deref());
        }
        for f in flushes {
            let _ = f.send(Ok(()));
        }
        // After the commit: a checkpoint can't run inside a transaction.
        for (on, reply) in bulks {
            let _ = reply.send(bulk_mode(&conn, &cfg, cache_kib, &stats, &mut bulk_depth, on));
        }
        dirty_rows += rows;
        rows_since_optimize += rows;
        if let Some(reply) = shutdown {
            maintenance(
                &conn,
                &stats,
                &cfg,
                &mut rows_since_optimize,
                &mut last_optimize,
                true,
            );
            let _ = reply.send(Ok(()));
            return;
        }
    }
    maintenance(
        &conn,
        &stats,
        &cfg,
        &mut rows_since_optimize,
        &mut last_optimize,
        true,
    );
}

/// Enter or leave bulk mode (see [`Writer::begin_bulk`]). Runs between
/// transactions on the writer thread, which owns `depth`.
fn bulk_mode(
    conn: &Connection,
    cfg: &WriterConfig,
    cache_kib: i64,
    stats: &WriterStats,
    depth: &mut u32,
    on: bool,
) -> Result<()> {
    if on {
        *depth += 1;
        if *depth == 1 && cfg.bulk_cache_kib > cache_kib {
            conn.execute_batch(&format!("PRAGMA cache_size = -{}", cfg.bulk_cache_kib))?;
        }
        return Ok(());
    }
    if *depth == 0 {
        return Ok(());
    }
    *depth -= 1;
    if *depth > 0 {
        return Ok(());
    }
    conn.execute_batch(&format!(
        "PRAGMA cache_size = -{cache_kib}; PRAGMA shrink_memory;"
    ))?;
    // SQLite returns the pages to malloc, which keeps freed small blocks
    // resident: measured, an idle server stayed at its scan-time footprint.
    release_free_memory();
    // TRUNCATE waits for readers through the busy handler; a reader holding
    // an old snapshot must not stall the writer for the usual 5 s.
    conn.busy_timeout(Duration::from_millis(50))?;
    let res = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get::<_, i64>(0));
    conn.busy_timeout(Duration::from_millis(5000))?;
    match res {
        Ok(0) => {
            stats.checkpoints.fetch_add(1, Ordering::Relaxed);
        }
        Ok(_) => tracing::debug!("WAL truncation deferred: a reader is still on an old snapshot"),
        Err(e) => tracing::warn!(error = %e, "wal checkpoint (truncate) failed"),
    }
    Ok(())
}

/// Ask the allocator to return freed memory to the OS. macOS keeps freed
/// small blocks resident (measured: an idle server held its scan-time
/// footprint, ~85 MB, with ~12 MB live); this releases the free pages.
/// macOS: `malloc_zone_pressure_relief` over all zones; glibc:
/// `malloc_trim`; Windows: `HeapCompact` of the process heap (Rust's and
/// the C runtime's allocator there); a no-op elsewhere. Then the hook a binary registered for
/// its own global allocator ([`set_release_hook`]), which also returns the
/// calling thread's caches. The writer calls it when bulk mode ends; a
/// scanner can call it once its own scan-sized structures are gone.
pub fn release_free_memory() {
    system_release();
    release_thread_memory();
}

/// Only the registered allocator hook ([`set_release_hook`]): memory the
/// calling thread's allocator caches hold. For a long-lived worker thread
/// going idle after a burst of work; nothing happens without a hook.
pub fn release_thread_memory() {
    if let Some(hook) = RELEASE_HOOK.get() {
        hook();
    }
}

static RELEASE_HOOK: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// Lets the binary that picks a global allocator (flux-server: jemalloc)
/// add that allocator's release to [`release_free_memory`] and
/// [`release_thread_memory`], so the libraries stay allocator-agnostic.
/// The hook runs on the releasing thread. The first registration wins;
/// returns whether this one did.
pub fn set_release_hook(hook: fn()) -> bool {
    RELEASE_HOOK.set(hook).is_ok()
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn system_release() {
    extern "C" {
        fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
    }
    // SAFETY: a libSystem function; a null zone means every zone and a goal
    // of 0 means as much as possible. It only releases free pages.
    unsafe {
        malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[allow(unsafe_code)]
fn system_release() {
    extern "C" {
        fn malloc_trim(pad: usize) -> std::ffi::c_int;
    }
    // SAFETY: glibc's malloc_trim only returns free heap memory to the OS.
    unsafe {
        malloc_trim(0);
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn system_release() {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetProcessHeap() -> *mut std::ffi::c_void;
        fn HeapCompact(heap: *mut std::ffi::c_void, flags: u32) -> usize;
    }
    // SAFETY: kernel32 functions. The process heap lives as long as the
    // process; HeapCompact only coalesces free blocks and decommits free
    // pages (flags 0: serialized, like every other call on that heap).
    unsafe {
        HeapCompact(GetProcessHeap(), 0);
    }
}

/// See the macOS version: a no-op on this platform.
#[cfg(not(any(target_os = "macos", all(target_os = "linux", target_env = "gnu"), windows)))]
fn system_release() {}

fn maintenance(
    conn: &Connection,
    stats: &WriterStats,
    cfg: &WriterConfig,
    rows_since_optimize: &mut u64,
    last_optimize: &mut Instant,
    force_optimize: bool,
) {
    // PASSIVE never blocks readers; it copies what it can and returns.
    match conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(())) {
        Ok(()) => {
            stats.checkpoints.fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => tracing::warn!(error = %e, "wal checkpoint failed"),
    }
    if *rows_since_optimize > 0 && (force_optimize || last_optimize.elapsed() >= cfg.optimize_every) {
        match conn.execute_batch("PRAGMA optimize") {
            Ok(()) => {
                stats.optimizes.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => tracing::warn!(error = %e, "PRAGMA optimize failed"),
        }
        *rows_since_optimize = 0;
        *last_optimize = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writer() -> (tempfile::TempDir, Writer, JoinHandle<()>, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.db");
        let mut conn = Connection::open(&path).unwrap();
        crate::schema::configure_writer(&conn, 8192, 0, 1000).unwrap();
        crate::schema::migrate(&mut conn).unwrap();
        conn.execute_batch("CREATE TABLE t(x INTEGER NOT NULL)").unwrap();
        let (w, h) = Writer::spawn(conn, WriterConfig::default(), 8192, Default::default()).unwrap();
        (dir, w, h, path)
    }

    #[test]
    fn failing_job_rolls_back_alone() {
        let (_d, w, h, path) = writer();
        let a = w
            .exec(|c| Ok(c.execute("INSERT INTO t VALUES (1)", [])?))
            .submit()
            .unwrap();
        let b = w
            .exec(|c| {
                c.execute("INSERT INTO t VALUES (2)", [])?;
                Err::<(), _>(CatalogError::Invalid("boom".into()))
            })
            .submit()
            .unwrap();
        let p = w
            .exec(|_c| -> Result<()> { panic!("job panic") })
            .submit()
            .unwrap();
        let d = w
            .exec(|c| Ok(c.execute("INSERT INTO t VALUES (3)", [])?))
            .submit()
            .unwrap();
        assert_eq!(a.wait().unwrap(), 1);
        assert!(matches!(b.wait(), Err(CatalogError::Invalid(_))));
        assert!(matches!(p.wait(), Err(CatalogError::JobPanicked(_))));
        assert_eq!(d.wait().unwrap(), 1);
        let r = Connection::open(&path).unwrap();
        let xs: Vec<i64> = r
            .prepare("SELECT x FROM t ORDER BY x")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|x| x.unwrap())
            .collect();
        assert_eq!(xs, vec![1, 3]);
        w.shutdown().wait().unwrap();
        h.join().unwrap();
    }

    #[test]
    fn replies_arrive_after_commit_and_jobs_are_batched() {
        let (_d, w, h, path) = writer();
        let replies: Vec<_> = (0..500)
            .map(|i| {
                w.exec(move |c| Ok(c.execute("INSERT INTO t VALUES (?1)", [i])?))
                    .submit()
                    .unwrap()
            })
            .collect();
        let done: Vec<_> = replies.into_iter().map(|r| r.wait().unwrap()).collect();
        assert_eq!(done.last(), Some(&1));
        // Visible to an independent reader as soon as the reply arrived.
        let r = Connection::open(&path).unwrap();
        let n: i64 = r.query_row("SELECT count(*) FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 500);
        let tx = w.stats().transactions.load(Ordering::Relaxed);
        assert!(tx < 500, "500 queued jobs should share transactions, got {tx}");
        w.shutdown().wait().unwrap();
        h.join().unwrap();
        assert!(matches!(w.flush().wait(), Err(CatalogError::WriterGone)));
    }

    #[test]
    fn async_await_works() {
        let (_d, w, h, _p) = writer();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let v = rt.block_on(async { w.exec(|_c| Ok(41 + 1)).await }).unwrap();
        assert_eq!(v, 42);
        w.shutdown().wait().unwrap();
        h.join().unwrap();
    }

    #[test]
    fn watched_threshold_is_ninety_percent() {
        assert!(!is_watched(89_999, 100_000));
        assert!(is_watched(90_000, 100_000));
        assert!(!is_watched(5, 0));
    }
}
