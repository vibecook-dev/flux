//! One root, one admission: walk → diff → work → batched apply → reconcile.
//!
//! ```text
//! walk_scan (job thread) ──dir groups──► classify vs snapshot
//!     unchanged ─► counted, nothing else
//!     stat-only (revived tombstone, renumbered device) ─► upsert Keep (no I/O)
//!     new / changed / failed probe due a retry ─► WorkerPool (probe + names) ─► batcher ─► Writer::apply
//!     same inode at a new path / new inode at a known path ─► held to the end
//!     directory links leaving the roots ─► walked after the main walk, sorted
//! after the walk: vanished = known, not seen, and proven absent by a
//! successful listing (never under `unread`, never behind a link that does
//! not resolve, never below an unmounted volume)
//!     held inode matches + fingerprint matches of vanished files ─► FileMove
//!     the rest of vanished ─► missing (tombstones)
//!     both guarded by the snapshot row (path + stat) the decision came from
//! committed files without a fingerprint ─► the background fingerprint pass
//! ```
//!
//! **Symlinks** (deterministic, whatever order listings answer in):
//! - A link (file or folder) whose canonical target is inside a root of the
//!   library is an alias: never followed, so the real path is cataloged
//!   once. Rows the catalog had under an alias are tombstoned.
//! - A folder link whose target is outside every root is walked (when
//!   `ScanConfig::follow_external_links`), after the main walk, in sorted
//!   order, at most once per canonical target, and never when the target
//!   contains a root. A file link to outside is cataloged under the link's
//!   path. A *new* file reached either way is added only if it probes as
//!   media (SEC-2: a link to a key or token is not served as a movie).
//! - A link that does not resolve (target offline) makes what the catalog
//!   has behind it unread, not missing.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError};
use flux_catalog::{
    scan_support, ExpectedRow, FileFacts, FileMove, FileRow, FileState, FileUpsert, ProbeUpdate, RootInfo,
    ScanBatch,
};
use flux_core::api::{Event, Id};
use flux_core::file_id::FileId;
use flux_core::{PARSER_VERSION, PROBE_VERSION};
use mille_core::{
    walk_scan, DirCtx, EntryKind, ErrorCode, FileFilter, FxError, IgnoreMatcher, ScanEntry, ScanOptions,
    Visit,
};

use crate::engine::Inner;
use crate::job::{lock, prio_u8, ErrRepr, JobShared, Progress};
use crate::paths::{
    decode_name, has_hidden_component, is_under, is_video_name, parent_rel, rel_join, rel_of,
};
use crate::process::{parse, probed_count, Out, RootCtx, Work, WorkItem};
use crate::resume::ResumeData;
use crate::{Priority, ScanMode};

/// What a job scans of one root.
pub(crate) enum Scope {
    Full(ScanMode),
    /// Absolute directories inside the root.
    Dirs(Vec<PathBuf>),
}

pub(crate) struct RootResult {
    /// The whole root was walked and every directory answered.
    pub complete: bool,
    pub resume: Option<ResumeData>,
}

fn ms(t: Instant) -> u64 {
    t.elapsed().as_nanos() as u64
}

pub(crate) fn scan_root(
    inner: &Arc<Inner>,
    job: &Arc<JobShared>,
    root: &RootInfo,
    want_resume: bool,
    follow_up: bool,
) -> Result<RootResult, ErrRepr> {
    let incomplete = RootResult {
        complete: false,
        resume: None,
    };
    let root_abs = PathBuf::from(&root.path);
    let p = &job.progress;

    // 1. Is the root there at all? (bounded: a sleeping NAS gets its
    //    first-response deadline, a hung one an error, never a hang)
    let st = match inner.stat_root(&root_abs) {
        Ok(s) => s,
        Err(why) => {
            if !job.is_sealed() {
                job.seal();
            }
            inner.root_unreachable(job, root, &why);
            return Ok(incomplete);
        }
    };
    let device = inner
        .devices
        .get(st.dev, &root_abs, &inner.cfg, inner.opts.read_hook.as_ref());

    // 2. Per-device admission, by priority.
    let Some(_slot) = device.admission.admit(job.id, &job.prio, || job.is_cancelled()) else {
        return Err(ErrRepr::Cancelled);
    };
    let scope = inner.scope_for(&job.seal(), root, follow_up);
    if let Scope::Dirs(d) = &scope {
        if d.is_empty() {
            return Ok(incomplete);
        }
    }
    let root_started = flux_catalog::now_ms();
    let cmds0 = p.writer_commands.load(std::sync::atomic::Ordering::Relaxed);
    p.started_at
        .compare_exchange(
            0,
            flux_catalog::now_ms(),
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
        )
        .ok();
    inner.progress_event(job);

    // 3. The catalog's view of this root (of the requested directories,
    //    for a watcher batch: one index range per directory).
    let t = Instant::now();
    let rid = root.id;
    let snapshot = match &scope {
        Scope::Full(_) => inner
            .catalog
            .read_blocking(move |c| scan_support::load_root_snapshot(c, rid))?,
        Scope::Dirs(dirs) => {
            let prefixes: Vec<String> = dirs.iter().filter_map(|d| rel_of(&root_abs, d)).collect();
            inner
                .catalog
                .read_blocking(move |c| scan_support::load_subtrees_snapshot(c, rid, &prefixes))?
        }
    };
    let idx = Indexes::build(&snapshot, matches!(scope, Scope::Dirs(_)));
    Progress::add(&p.diff_ns, ms(t));

    // 4. Batcher thread and the context workers share.
    let (tx, rx) = bounded::<Out>(inner.cfg.batch_rows.max(1) * 2);
    let batcher = {
        let (inner, job) = (inner.clone(), job.clone());
        std::thread::Builder::new()
            .name(format!("flux-scan-batch-{}", job.id))
            .spawn(move || batcher(&inner, &job, rid, rx))
            .map_err(|e| ErrRepr::Other(format!("spawn batcher: {e}")))?
    };
    let roots = inner.catalog.read_blocking(scan_support::list_roots)?;
    let other_roots = roots
        .iter()
        .filter(|r| r.id != root.id)
        .map(|r| (r.id, r.path.clone()))
        .collect();
    let lib_roots: Vec<PathBuf> = roots
        .iter()
        .filter(|r| r.library_id == root.library_id && r.id != root.id)
        .map(|r| PathBuf::from(&r.path))
        .collect();
    let ctx = Arc::new(RootCtx {
        inner: inner.clone(),
        job: job.clone(),
        root_id: root.id,
        kind: root.library_kind,
        device: device.clone(),
        fp_index: idx.fp.clone(),
        fp_sizes: idx.fp.iter().map(|&(_, s)| s).collect(),
        other_roots,
        tx: Mutex::new(Some(tx)),
        deferred: Mutex::new(Vec::new()),
        later: Mutex::new(Vec::new()),
        pending: Default::default(),
        busy: Mutex::new(None),
    });

    // 5. Walk. A history token taken before it vouches for everything the
    // walk sees; it is saved with the resume snapshot once the scan is
    // complete (resume.rs).
    let history = (want_resume && matches!(scope, Scope::Full(_)))
        .then(|| mille_core::HistoryToken::capture(&root_abs).ok())
        .flatten();
    let mode = match &scope {
        Scope::Full(m) => *m,
        Scope::Dirs(_) => ScanMode::Quick,
    };
    let mut w = Walk {
        ctx: &ctx,
        root_abs: &root_abs,
        snapshot: &snapshot,
        idx: &idx,
        mode,
        full: matches!(scope, Scope::Full(_)),
        now: flux_catalog::now_ms(),
        retry_all: mode == ScanMode::Deep || job.prio() >= prio_u8(Priority::Interactive),
        cur: None,
        seen: HashSet::new(),
        replaced: HashMap::new(),
        inode: Vec::new(),
        listed: HashSet::new(),
        seen_dirs: HashSet::new(),
        unread: HashSet::new(),
        bases: Vec::new(),
        entries: 0,
        media_walked: 0,
        resume: (want_resume && matches!(scope, Scope::Full(_))).then(Vec::new),
        link_dirs: Vec::new(),
        cap: (inner.cfg.workers * 8).max(64),
        canon_root: st.canonical.clone(),
        other_lib_roots: lib_roots,
        roots_canon: None,
        ext_links: Vec::new(),
        walked_targets: Vec::new(),
        external: Vec::new(),
        dangling: HashSet::new(),
        link_probe: HashMap::new(),
        with_entries: HashSet::new(),
        dir_dev: HashMap::new(),
        renumbered: HashMap::new(),
    };
    let t = Instant::now();
    let walked = match &scope {
        Scope::Full(_) => w.walk_full(),
        Scope::Dirs(dirs) => w.walk_dirs(dirs),
    }
    .and_then(|()| w.walk_links());
    Progress::add(&p.walk_ns, ms(t));

    let finish = |ctx: &Arc<RootCtx>| {
        lock(&ctx.tx).take();
    };
    match walked {
        Ok(()) => {}
        Err(WalkErr::Cancelled) => {
            finish(&ctx);
            let _ = batcher.join();
            return Err(ErrRepr::Cancelled);
        }
        Err(WalkErr::Root(why)) => {
            // The root answered the stat but not the listing.
            ctx.pending.wait_zero();
            finish(&ctx);
            let _ = batcher.join();
            inner.root_unreachable(job, root, &why);
            return Ok(incomplete);
        }
    }
    // Across a multi-root library this is the running total of walked roots
    // (u64::MAX: none walked yet). A compare-exchange loop, not
    // `fetch_update` (deprecated by Rust 1.99; `try_update` is newer than
    // the MSRV).
    {
        use std::sync::atomic::Ordering::Relaxed;
        let mut t = p.files_total.load(Relaxed);
        loop {
            let next = if t == u64::MAX {
                w.media_walked
            } else {
                t + w.media_walked
            };
            match p.files_total.compare_exchange_weak(t, next, Relaxed, Relaxed) {
                Ok(_) => break,
                Err(now) => t = now,
            }
        }
    }
    inner.progress_event(job);

    // An empty listing of a root the catalog has files for is far more
    // likely an unmounted volume (the bare mount point) than a deleted
    // library: keep everything and call the root unavailable.
    let live = snapshot
        .values()
        .filter(|r| r.state != FileState::Missing)
        .count();
    if w.full && w.entries == 0 && live > 0 {
        ctx.pending.wait_zero();
        finish(&ctx);
        let _ = batcher.join();
        inner.root_unreachable(
            job,
            root,
            &format!(
                "{} is empty but the catalog has {live} files there (unmounted?)",
                root.path
            ),
        );
        return Ok(incomplete);
    }

    // 6. First pass done: reconcile what the walk could not decide alone.
    ctx.pending.wait_zero();
    if job.is_cancelled() {
        finish(&ctx);
        let _ = batcher.join();
        return Err(ErrRepr::Cancelled);
    }
    let t = Instant::now();
    let rec = w.reconcile();
    Progress::add(&p.diff_ns, ms(t));
    if !w.unread.is_empty() {
        let mut u = lock(&p.unread);
        let mut list: Vec<&String> = w.unread.iter().collect();
        list.sort();
        for d in list {
            u.push(root_abs.join(d).to_string_lossy().into_owned());
        }
        p.dirs_unavailable
            .fetch_add(w.unread.len() as u32, std::sync::atomic::Ordering::Relaxed);
    }
    if !rec.moves.is_empty() || !rec.missing.is_empty() {
        let batch = ScanBatch {
            root_id: root.id,
            moves: rec.moves,
            missing: rec.missing,
            expect: rec.expect,
            ..Default::default()
        };
        apply(inner, job, batch, 0);
    }
    for u in rec.upserts {
        ctx.send(u);
    }
    for item in rec.rework {
        ctx.submit(item);
    }
    ctx.pending.wait_zero();
    finish(&ctx);
    let _ = batcher.join();
    if let Some((s, e)) = *lock(&ctx.busy) {
        Progress::add(&p.process_ns, (e - s).as_nanos() as u64);
    }
    // Committed without a fingerprint: the background pass does them.
    let later = std::mem::take(&mut *lock(&ctx.later));
    if !later.is_empty() && !job.is_cancelled() {
        inner
            .fingerprints
            .enqueue(inner, root.library_id, root.id, device.clone(), later);
    }

    // 7. Bookkeeping: availability, scan record. The record is a write, so
    //    it is kept only when this scan wrote anyway or clears an error: an
    //    unchanged rescan stays at zero writer commands. When the root last
    //    answered is kept in memory instead (`Scanner::root_last_seen`).
    inner.root_reachable(job, root);
    inner.root_seen(root);
    crate::watch::ensure_watched(inner, root.library_id, &root_abs, &st.canonical);
    let wrote = p.writer_commands.load(std::sync::atomic::Ordering::Relaxed) > cmds0;
    if wrote || root.last_scan_error.is_some() {
        Progress::add(&p.writer_commands, 1);
        let rec = flux_catalog::ScanRecord {
            started_at: Some(root_started),
            finished_at: Some(flux_catalog::now_ms()),
            error: None,
        };
        let _ = inner.catalog.writer().record_scan(root.id, rec).wait();
    }
    let complete = w.full && w.unread.is_empty();
    let resume = w.resume.take().map(|entries| ResumeData {
        root_id: root.id,
        root: root_abs.clone(),
        root_mtime_ms: st.mtime_ms,
        entries,
        history,
    });
    Ok(RootResult { complete, resume })
}

/// Lookups built once from the snapshot.
struct Indexes<'a> {
    /// Live rows by inode (move detection without opening anything).
    by_ino: HashMap<u64, Vec<&'a str>>,
    /// `(fp128, size)` of live rows.
    fp: HashSet<(u128, u64)>,
    /// Directories that hold at least one live file (directory scans walk
    /// into directories the catalog does not know).
    known_dirs: HashSet<&'a str>,
}

impl<'a> Indexes<'a> {
    fn build(snap: &'a HashMap<String, FileRow>, with_dirs: bool) -> Self {
        let mut by_ino: HashMap<u64, Vec<&'a str>> = HashMap::new();
        let mut fp = HashSet::new();
        let mut known_dirs = HashSet::new();
        for (rel, row) in snap {
            if row.state == FileState::Missing {
                continue;
            }
            if let Some(i) = row.ino {
                by_ino.entry(i).or_default().push(rel.as_str());
            }
            if let Some(f) = row.fp128 {
                fp.insert((f, row.size));
            }
            if with_dirs {
                let mut d = parent_rel(rel);
                while !d.is_empty() && known_dirs.insert(d) {
                    d = parent_rel(d);
                }
            }
        }
        Indexes {
            by_ino,
            fp,
            known_dirs,
        }
    }
}

/// How a walked media file relates to the catalog.
enum Class<'a> {
    /// Nothing to do (`true`: the row has no fingerprint yet).
    Unchanged(bool),
    /// Stat-only refresh, no I/O: a tombstoned file is back unchanged, or
    /// a remount renumbered the device (same inode, size, mtime).
    Keep,
    /// Deep scan: parsed by an older flux-names; re-parse, no I/O.
    Reparse,
    /// Open it (`true`: the catalog has no fingerprint of this content).
    Work(Work, bool),
    /// A known inode at a new path: a move, unless the old path still has it.
    Inode(&'a FileRow, &'a str),
    /// The path now holds a different inode.
    Replaced,
}

fn classify<'a>(
    snap: &'a HashMap<String, FileRow>,
    by_ino: &HashMap<u64, Vec<&'a str>>,
    f: &FileFacts,
    mode: ScanMode,
    now: i64,
    retry_all: bool,
) -> Class<'a> {
    if let Some(row) = snap.get(&f.rel_path) {
        let same_ino = match (row.ino, f.ino) {
            (Some(a), Some(b)) => Some(a == b),
            _ => None,
        };
        if same_ino == Some(false) && row.state != FileState::Missing {
            return Class::Replaced;
        }
        if row.size != f.size || row.mtime_ns != f.mtime_ns || same_ino == Some(false) {
            return Class::Work(Work::Changed, true);
        }
        let need_fp = row.fp128.is_none();
        if mode == ScanMode::Deep {
            // Bring the file up to this build's probe and parser versions.
            if row.state == FileState::ProbeFailed || row.probe_version.is_none_or(|v| v < PROBE_VERSION) {
                return Class::Work(Work::Reprobe, need_fp);
            }
            if row.parse_version.is_none_or(|v| v < PARSER_VERSION) {
                return Class::Reparse;
            }
        }
        // Stat-only refresh: a tombstone is back, a remount renumbered the
        // device, or the catalog has no identity for a file the walk now
        // has one for (a file behind a link out of the library, cataloged
        // on Windows before the scan recorded identities there).
        let gained_identity = row.ino.is_none() && f.ino.is_some();
        if row.state == FileState::Missing || (same_ino == Some(true) && row.dev != f.dev) || gained_identity
        {
            return Class::Keep;
        }
        // A failed probe (unreadable at the time, ffprobe missing or timed
        // out, a NAS hiccup) is retried once its backoff passed, or at once
        // when a user asked.
        if row.state == FileState::ProbeFailed && (retry_all || row.probe_retry_at.is_none_or(|t| t <= now)) {
            return Class::Work(Work::Retry, need_fp);
        }
        return Class::Unchanged(need_fp);
    }
    if let Some(cands) = f.ino.and_then(|i| by_ino.get(&i)) {
        for rel in cands {
            let Some(row) = snap.get(*rel) else { continue };
            let btime_ok = match (row.btime_ns, f.btime_ns) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            };
            // `st_dev` is not stable across remounts; a matching birth time
            // vouches for the inode instead.
            let dev_ok = row.dev == f.dev || (row.btime_ns.is_some() && row.btime_ns == f.btime_ns);
            if btime_ok && dev_ok && row.size == f.size {
                return Class::Inode(row, rel);
            }
        }
    }
    Class::Work(Work::New, true)
}

/// A walked media file of one directory: name, stat, reached through a
/// link that leaves the library's roots.
type Media = (String, FileFacts, bool);

/// Files of one directory, as the walk delivers them (together).
struct Group {
    id: u64,
    abs: PathBuf,
    rel: Option<String>,
    names: Vec<String>,
    media: Vec<Media>,
}

enum WalkErr {
    Cancelled,
    Root(String),
}

/// The snapshot row a held move was decided from.
struct OldRow {
    id: Id,
    rel: String,
    size: u64,
    mtime_ns: i64,
}

struct Walk<'a> {
    ctx: &'a Arc<RootCtx>,
    root_abs: &'a Path,
    snapshot: &'a HashMap<String, FileRow>,
    idx: &'a Indexes<'a>,
    mode: ScanMode,
    full: bool,
    now: i64,
    /// Retry every failed probe now (Interactive request, Deep scan).
    retry_all: bool,
    cur: Option<Group>,
    /// Media paths seen by this walk.
    seen: HashSet<String>,
    /// Paths that now hold another inode (decided after the walk).
    replaced: HashMap<String, WorkItem>,
    /// New paths holding a known inode.
    inode: Vec<(WorkItem, OldRow)>,
    /// Directories whose listing completed (relative; `""` = root).
    listed: HashSet<String>,
    /// Every directory entry delivered (and directory links decided later).
    seen_dirs: HashSet<String>,
    unread: HashSet<String>,
    /// Scope roots (relative): `""` for a full scan, else each directory.
    bases: Vec<String>,
    entries: u64,
    media_walked: u64,
    resume: Option<Vec<mille_core::WalkedEntry>>,
    /// Followed directory links (their subtrees stay out of the resume
    /// snapshot, whose own walk does not follow links).
    link_dirs: Vec<PathBuf>,
    cap: usize,
    /// This root, canonical.
    canon_root: PathBuf,
    /// The library's other roots as stored, and all of them canonical
    /// (computed on the first link).
    other_lib_roots: Vec<PathBuf>,
    roots_canon: Option<Vec<PathBuf>>,
    /// Directory links leaving the roots, walked after the main walk:
    /// `(rel, absolute link path, canonical target)`.
    ext_links: Vec<(String, PathBuf, PathBuf)>,
    /// Canonical targets already walked through a link.
    walked_targets: Vec<PathBuf>,
    /// Relative paths of the directory links walked (content outside the
    /// roots).
    external: Vec<String>,
    /// Links (files or folders) whose target does not resolve.
    dangling: HashSet<String>,
    /// Memo: is this relative path a link that does not resolve?
    link_probe: HashMap<String, bool>,
    /// Directories whose listing delivered at least one entry.
    with_entries: HashSet<String>,
    /// A directory's `st_dev` as its parent's listing reported it.
    dir_dev: HashMap<String, u64>,
    /// Devices renumbered since the snapshot (old → new), seen on files
    /// with the same inode at the same path.
    renumbered: HashMap<u64, u64>,
}

impl Walk<'_> {
    fn options(&self, ignore: Option<Arc<IgnoreMatcher>>) -> ScanOptions {
        ScanOptions {
            parallelism: self.ctx.device.concurrency,
            file_filter: Some(FileFilter::extensions(crate::paths::walk_extensions())),
            cancel: Some(self.ctx.job.cancel.clone()),
            ignore,
            // Dot-names are delivered so that `visit` can count and log a
            // hidden media file (Windows doesn't hide `.Name.mp4`, so a user
            // there can't tell why it is missing). Nothing hidden is
            // cataloged or walked, as with the media profile's own filter;
            // OS litter (`.DS_Store`, AppleDouble `._*`) is still dropped by
            // Mille's default ignores and never reaches `visit`.
            include_hidden: true,
            ..ScanOptions::media()
        }
    }

    fn walk_full(&mut self) -> Result<(), WalkErr> {
        let opts = self.options(None);
        let device = self.ctx.device.clone();
        let root = self.root_abs;
        self.bases.push(String::new());
        self.listed.insert(String::new());
        let res = walk_scan(root, &opts, &device.boundary, |d, e| self.visit(d, e));
        self.flush();
        match res {
            Ok(report) => {
                for u in report.unread {
                    if let Some(rel) = rel_of(root, &u.path) {
                        tracing::warn!(dir = %u.path.display(), code = ?u.failure.code(), "directory did not answer; keeping what is known under it");
                        self.unread.insert(rel);
                    }
                }
                Ok(())
            }
            Err(FxError::Cancelled) => Err(WalkErr::Cancelled),
            Err(e) => Err(WalkErr::Root(e.to_string())),
        }
    }

    fn walk_dirs(&mut self, dirs: &[PathBuf]) -> Result<(), WalkErr> {
        let device = self.ctx.device.clone();
        let root = self.root_abs;
        for d in dirs {
            if self.ctx.job.is_cancelled() {
                return Err(WalkErr::Cancelled);
            }
            let Some(d_rel) = rel_of(root, d) else { continue };
            if has_hidden_component(&d_rel) || flux_names::is_ignored(Path::new(&d_rel)) {
                continue;
            }
            let matcher = ancestor_ignores(root, &d_rel);
            if matcher.as_ref().is_some_and(|m| m.is_ignored(d, true)) {
                continue;
            }
            let opts = self.options(matcher);
            self.bases.push(d_rel.clone());
            let res = walk_scan(d, &opts, &device.boundary, |dc, e| self.visit(dc, e));
            self.flush();
            match res {
                Ok(report) => {
                    self.listed.insert(d_rel);
                    for u in report.unread {
                        if let Some(rel) = rel_of(root, &u.path) {
                            self.unread.insert(rel);
                        }
                    }
                }
                Err(FxError::Cancelled) => return Err(WalkErr::Cancelled),
                // The directory is gone: an empty listing of it.
                Err(e) if matches!(e.code(), ErrorCode::ENOENT | ErrorCode::ENOTDIR) => {
                    self.listed.insert(d_rel);
                }
                Err(e) => {
                    tracing::warn!(dir = %d.display(), error = %e, "directory did not answer; keeping what is known under it");
                    self.unread.insert(d_rel);
                }
            }
        }
        Ok(())
    }

    /// Walk the directory links that leave the library's roots, after the
    /// walk that found them: sorted by path, each canonical target once, a
    /// target below one already walked skipped. Links found inside them are
    /// handled the same way in the next round. The order, and so the
    /// result, never depends on which listing answered first.
    fn walk_links(&mut self) -> Result<(), WalkErr> {
        let ctx = self.ctx;
        let device = ctx.device.clone();
        let p = &ctx.job.progress;
        loop {
            let mut round = std::mem::take(&mut self.ext_links);
            if round.is_empty() {
                return Ok(());
            }
            round.sort_by(|a, b| a.0.cmp(&b.0));
            for (rel, abs, target) in round {
                if ctx.job.is_cancelled() {
                    return Err(WalkErr::Cancelled);
                }
                if self.walked_targets.iter().any(|t| target.starts_with(t)) {
                    // Already cataloged through another link: what this
                    // link had is proven gone (not walked).
                    Progress::add(&p.links_skipped, 1);
                    self.seen_dirs.remove(&rel);
                    continue;
                }
                self.walked_targets.push(target);
                Progress::add(&p.links_external, 1);
                self.external.push(rel.clone());
                self.link_dirs.push(abs.clone());
                let opts = self.options(ancestor_ignores(self.root_abs, &rel));
                let res = walk_scan(&abs, &opts, &device.boundary, |d, e| self.visit(d, e));
                self.flush();
                match res {
                    Ok(report) => {
                        self.listed.insert(rel);
                        for u in report.unread {
                            if let Some(r) = rel_of(self.root_abs, &u.path) {
                                self.unread.insert(r);
                            }
                        }
                    }
                    Err(FxError::Cancelled) => return Err(WalkErr::Cancelled),
                    Err(e) => {
                        // An offline disk behind the link: unread, never missing.
                        tracing::warn!(link = %abs.display(), error = %e, "linked folder did not answer; keeping what is known under it");
                        self.unread.insert(rel);
                    }
                }
            }
        }
    }

    /// Canonical paths of the library's roots (this one first).
    fn roots_canon(&mut self) -> &[PathBuf] {
        if self.roots_canon.is_none() {
            let mut v = vec![self.canon_root.clone()];
            for r in &self.other_lib_roots {
                let c = self
                    .ctx
                    .inner
                    .stat_root(r)
                    .map(|s| s.canonical)
                    .unwrap_or_else(|_| r.clone());
                v.push(c);
            }
            self.roots_canon = Some(v);
        }
        self.roots_canon.as_deref().unwrap_or_default()
    }

    /// The canonical target of a link that resolves (bounded stat).
    fn link_target(&self, abs: &Path) -> Option<PathBuf> {
        let out = self.ctx.inner.root_stat.stat(abs, true).ok()?;
        let st = out.as_stat()?;
        st.target.as_ref()?;
        st.canonical.clone()
    }

    fn inside_roots(&mut self, target: &Path) -> bool {
        self.roots_canon().iter().any(|r| target.starts_with(r))
    }

    /// A directory link: never descended by Mille (`SkipChildren`); an
    /// external one is walked after the main walk (see [`Walk::walk_links`]).
    fn dir_link(&mut self, dir: &DirCtx<'_>, e: &ScanEntry<'_>, rel: String) -> Visit {
        let ctx = self.ctx;
        let p = &ctx.job.progress;
        // A directory scan doesn't re-walk a known linked folder, as it
        // doesn't re-walk any known subdirectory.
        if !self.full && self.idx.known_dirs.contains(rel.as_str()) {
            self.seen_dirs.insert(rel);
            return Visit::SkipChildren;
        }
        if !ctx.inner.cfg.follow_external_links {
            Progress::add(&p.links_skipped, 1);
            return Visit::SkipChildren;
        }
        let abs = dir.join(e.name);
        let Some(target) = self.link_target(&abs) else {
            self.dangling.insert(rel);
            return Visit::SkipChildren;
        };
        let contains_root = self.roots_canon().iter().any(|r| r.starts_with(&target));
        if self.inside_roots(&target) || contains_root {
            // An alias of something in the library (or a loop back to it):
            // the real path is cataloged; rows under the alias are gone.
            Progress::add(&p.links_skipped, 1);
            return Visit::SkipChildren;
        }
        self.seen_dirs.insert(rel.clone());
        self.ext_links.push((rel, abs, target));
        Visit::SkipChildren
    }

    /// A file link: cataloged when its target is outside every root (as
    /// external content), skipped as an alias when inside one.
    fn file_link_followed(&mut self, dir: &DirCtx<'_>, e: &ScanEntry<'_>, rel: &str) -> bool {
        let ctx = self.ctx;
        let p = &ctx.job.progress;
        if !ctx.inner.cfg.follow_external_links {
            Progress::add(&p.links_skipped, 1);
            return false;
        }
        // Cataloged before with this very stat: its target hasn't changed
        // (a retargeted link has another inode, size or mtime), so the
        // decision stands without a canonicalize per link per scan.
        if let Some(row) = self.snapshot.get(rel) {
            let known = e.dev != 0 || e.ino != 0;
            if row.state != FileState::Missing
                && row.size == e.size
                && row.mtime_ns == e.mtime_ns
                && (!known || row.ino == Some(e.ino))
            {
                Progress::add(&p.links_external, 1);
                return true;
            }
        }
        let abs = dir.join(e.name);
        match self.link_target(&abs) {
            None => {
                self.dangling.insert(rel.to_string());
                false
            }
            Some(t) if self.inside_roots(&t) => {
                Progress::add(&p.links_skipped, 1);
                false
            }
            Some(_) => {
                Progress::add(&p.links_external, 1);
                true
            }
        }
    }

    fn under_external(&self, rel: &str) -> bool {
        self.external.iter().any(|x| is_under(rel, x))
    }

    /// A name starting with `.`: never cataloged, never walked into. A
    /// media file is counted (`ScanSummary::hidden_skipped`) and logged, so
    /// a user can find out why it isn't in the library.
    fn hidden(&mut self, dir: &DirCtx<'_>, e: &ScanEntry<'_>) -> Visit {
        if e.is_dir() {
            return Visit::SkipChildren;
        }
        if e.is_file() && decode_name(e.name).is_some_and(is_video_name) {
            Progress::add(&self.ctx.job.progress.hidden_media, 1);
            tracing::info!(
                path = %dir.join(e.name).display(),
                "skipping a hidden media file: its name starts with '.' (rename it to add it to the library)"
            );
        }
        Visit::Continue
    }

    fn visit(&mut self, dir: &DirCtx<'_>, e: &ScanEntry<'_>) -> Visit {
        // Before anything else, so a hidden entry changes nothing the walk
        // decides (an empty listing, the resume snapshot, links).
        if e.name.as_encoded_bytes().first() == Some(&b'.') {
            return self.hidden(dir, e);
        }
        if self.cur.as_ref().is_none_or(|g| g.id != dir.id) {
            self.flush();
            let rel = rel_of(self.root_abs, dir.path);
            if let Some(r) = &rel {
                self.with_entries.insert(r.clone());
            }
            self.cur = Some(Group {
                id: dir.id,
                abs: dir.path.to_path_buf(),
                rel,
                names: Vec::new(),
                media: Vec::new(),
            });
        }
        self.entries += 1;
        if self.cur.as_ref().is_none_or(|g| g.rel.is_none()) {
            return Visit::SkipChildren;
        }
        let Some(name) = decode_name(e.name) else {
            Progress::add(&self.ctx.job.progress.non_utf8, 1);
            tracing::warn!(dir = %dir.path.display(), name = ?e.name, "skipping a name that is not valid UTF-8");
            return Visit::SkipChildren;
        };
        let Some(rel) = self
            .cur
            .as_ref()
            .and_then(|g| g.rel.as_deref())
            .map(|d| rel_join(d, name))
        else {
            return Visit::SkipChildren;
        };
        if let Some(entries) = self.resume.as_mut() {
            let under_link = self.link_dirs.iter().any(|l| dir.path.starts_with(l));
            if !under_link {
                entries.push(crate::resume::walked_entry(dir, e, name));
            }
        }
        let mut external = self.under_external(&rel);
        if e.is_symlink {
            match e.kind {
                EntryKind::Directory => return self.dir_link(dir, e, rel),
                EntryKind::File => {
                    if !self.file_link_followed(dir, e, &rel) {
                        return Visit::Continue;
                    }
                    external = true;
                }
                _ => {
                    // Delivered but not resolved: its target is offline.
                    self.dangling.insert(rel);
                    return Visit::Continue;
                }
            }
        }
        if e.is_dir() {
            let descend = self.full || !self.idx.known_dirs.contains(rel.as_str());
            self.dir_dev.insert(rel.clone(), e.dev);
            self.seen_dirs.insert(rel.clone());
            return if descend {
                self.listed.insert(rel);
                Visit::Continue
            } else {
                Visit::SkipChildren
            };
        }
        if e.is_file() {
            Progress::add(&self.ctx.job.progress.files_seen, 1);
            let Some(group) = self.cur.as_mut() else {
                return Visit::Continue;
            };
            group.names.push(name.to_string());
            if is_video_name(name) && !flux_names::is_ignored(Path::new(&rel)) {
                // Mille reports dev = ino = 0 where the platform has no inode
                // (Windows); the catalog's "unknown" is None. A file behind a
                // link that leaves the library is served only if it is the
                // file the scan recorded (SEC-2), so there the file itself
                // is asked (volume serial and file id; one open per such
                // file per scan).
                let (dev, ino) = if e.dev != 0 || e.ino != 0 {
                    (Some(e.dev), Some(e.ino))
                } else if external {
                    match FileId::of_path(&dir.join(e.name)) {
                        Ok(id) => (Some(id.dev), Some(id.ino)),
                        Err(err) => {
                            tracing::debug!(path = %dir.join(e.name).display(), error = %err, "no file identity");
                            (None, None)
                        }
                    }
                } else {
                    (None, None)
                };
                group.media.push((
                    name.to_string(),
                    FileFacts {
                        rel_path: rel,
                        dev,
                        ino,
                        btime_ns: e.btime_ns,
                        size: e.size,
                        mtime_ns: e.mtime_ns,
                        fp128: None,
                        oshash: None,
                    },
                    external,
                ));
            }
        }
        Visit::Continue
    }

    /// Classify one directory's media files and dispatch them.
    fn flush(&mut self) {
        let Some(g) = self.cur.take() else { return };
        if g.media.is_empty() {
            return;
        }
        let p = &self.ctx.job.progress;
        let fingerprints = self.ctx.inner.cfg.fingerprint;
        let siblings = Arc::new(g.names);
        for (name, facts, external) in g.media {
            if !self.seen.insert(facts.rel_path.clone()) {
                continue; // overlapping directory requests
            }
            self.media_walked += 1;
            Progress::add(&p.media_files, 1);
            let abs = g.abs.join(&name);
            if let Some(row) = self.snapshot.get(&facts.rel_path) {
                if let (Some(a), Some(b)) = (row.dev, facts.dev) {
                    if a != b && row.ino == facts.ino {
                        self.renumbered.insert(a, b);
                    }
                }
            }
            let item = |work, need_fp| WorkItem {
                facts: facts.clone(),
                abs: abs.clone(),
                name: name.clone(),
                siblings: siblings.clone(),
                work,
                need_fp,
                external,
            };
            match classify(
                self.snapshot,
                &self.idx.by_ino,
                &facts,
                self.mode,
                self.now,
                self.retry_all,
            ) {
                Class::Unchanged(need_fp) => {
                    Progress::add(&p.unchanged, 1);
                    Progress::add(&p.files_done, 1);
                    if need_fp && fingerprints {
                        self.ctx.fingerprint_later(&facts, &abs);
                    }
                }
                Class::Keep => self.ctx.send(FileUpsert {
                    facts,
                    probe: ProbeUpdate::Keep,
                    guess: None,
                }),
                Class::Reparse => {
                    let guess = parse(&facts.rel_path, &name, &siblings, self.ctx.kind);
                    self.ctx.send(FileUpsert {
                        facts,
                        probe: ProbeUpdate::Keep,
                        guess: Some(guess),
                    });
                }
                Class::Work(work, need_fp) => {
                    if work == Work::Retry {
                        Progress::add(&p.probe_retries, 1);
                    }
                    let ctx = self.ctx;
                    ctx.pending.wait_below(self.cap, || ctx.job.is_cancelled());
                    ctx.submit(item(work, need_fp));
                }
                Class::Inode(row, old) => {
                    let held = item(Work::NewFinal, false);
                    self.inode.push((
                        held,
                        OldRow {
                            id: row.id,
                            rel: old.to_string(),
                            size: row.size,
                            mtime_ns: row.mtime_ns,
                        },
                    ));
                }
                Class::Replaced => {
                    self.replaced
                        .insert(facts.rel_path.clone(), item(Work::Changed, true));
                }
            }
        }
    }

    /// Whether a successful listing proves `rel` (not seen) is gone: its
    /// nearest listed ancestor directory answered, and either is its parent
    /// or no longer contains the next directory down. Under an unread
    /// directory, or behind a link that does not resolve, nothing is
    /// proven. Returns that ancestor and the first directory below it on
    /// the way to `rel` (`None` when it is the parent).
    fn gone_at(&self, rel: &str) -> Option<(String, Option<String>)> {
        if self.dangling.contains(rel) {
            return None;
        }
        let mut child: Option<&str> = None;
        let mut p = parent_rel(rel);
        loop {
            if self.unread.contains(p) || self.dangling.contains(p) {
                return None;
            }
            if self.listed.contains(p) {
                return match child {
                    Some(c) if self.seen_dirs.contains(c) => None,
                    _ => Some((p.to_string(), child.map(str::to_string))),
                };
            }
            if p.is_empty() {
                return None;
            }
            child = Some(p);
            p = parent_rel(p);
        }
    }

    /// `rel` is a link whose target does not resolve. A folder link with no
    /// video extension is dropped by the walk's filter when it doesn't
    /// resolve, so its absence from a listing proves nothing: look.
    fn dangling_link(&mut self, rel: &str) -> bool {
        if let Some(&d) = self.link_probe.get(rel) {
            return d;
        }
        let abs = self.root_abs.join(rel);
        let d = match self.ctx.inner.root_stat.stat(&abs, false) {
            Ok(out) => out
                .as_stat()
                .is_some_and(|st| st.metadata.file_type().is_symlink() && st.target.is_none()),
            Err(_) => false,
        };
        self.link_probe.insert(rel.to_string(), d);
        d
    }

    /// The listed directory `anc` delivered nothing although `row` lived
    /// below it on another device: a volume mounted there is not mounted
    /// now (the bare mount point lists empty). The root has its own guard.
    fn unmounted(&mut self, anc: &str, row: &FileRow) -> bool {
        if anc.is_empty() || self.with_entries.contains(anc) {
            return false;
        }
        let Some(row_dev) = row.dev else { return false };
        let dir_dev = match self.dir_dev.get(anc) {
            Some(&d) => d,
            None => {
                let Some(d) = dev_of_path(self, &self.root_abs.join(anc)) else {
                    return false;
                };
                self.dir_dev.insert(anc.to_string(), d);
                d
            }
        };
        unmounted_rule(dir_dev, row_dev, &self.renumbered)
    }

    fn reconcile(&mut self) -> Reconciled {
        let ctx = self.ctx;
        let p = &ctx.job.progress;
        // Known, live, in scope, not seen, proven absent.
        let snapshot = self.snapshot;
        let mut cands: Vec<(&str, &FileRow, String, Option<String>)> = Vec::new();
        for (rel, row) in snapshot {
            if row.state == FileState::Missing || self.seen.contains(rel) {
                continue;
            }
            if !self.bases.iter().any(|b| is_under(rel, b)) {
                continue;
            }
            if let Some((anc, child)) = self.gone_at(rel) {
                cands.push((rel.as_str(), row, anc, child));
            }
        }
        let mut vanished: HashMap<&str, &FileRow> = HashMap::new();
        for (rel, row, anc, child) in cands {
            if let Some(c) = child {
                if self.dangling_link(&c) {
                    self.unread.insert(c);
                    continue;
                }
            }
            if self.unmounted(&anc, row) {
                tracing::warn!(dir = %self.root_abs.join(&anc).display(), "empty, but the catalog's files there were on another device (unmounted?); keeping them");
                self.unread.insert(anc);
                continue;
            }
            vanished.insert(rel, row);
        }
        // Links whose target is offline, with cataloged files behind them.
        let dangling: Vec<String> = self
            .dangling
            .iter()
            .filter(|d| {
                snapshot
                    .iter()
                    .any(|(k, r)| r.state != FileState::Missing && is_under(k, d))
            })
            .cloned()
            .collect();
        self.unread.extend(dangling);

        let mut out = Reconciled::default();
        let guard = |id: Id, rel: &str, row_size: u64, row_mtime: i64| ExpectedRow {
            file_id: id,
            rel_path: rel.to_string(),
            size: row_size,
            mtime_ns: row_mtime,
        };
        let mut moved_from: HashSet<String> = HashSet::new();
        // Same inode at a new path.
        for (item, old) in std::mem::take(&mut self.inode) {
            let gone = !moved_from.contains(&old.rel)
                && (vanished.remove(old.rel.as_str()).is_some() || self.replaced.contains_key(&old.rel));
            if gone {
                let guess = parse(&item.facts.rel_path, &item.name, &item.siblings, ctx.kind);
                out.moves.push(FileMove {
                    file_id: old.id,
                    to: item.facts.clone(),
                    guess: Some(guess),
                });
                out.expect.push(guard(old.id, &old.rel, old.size, old.mtime_ns));
                // Edited in place (same size, new mtime) and renamed: keep
                // the identity, and re-probe the content (INT-V1).
                if old.mtime_ns != item.facts.mtime_ns {
                    out.rework.push(WorkItem {
                        work: Work::Changed,
                        need_fp: true,
                        ..item
                    });
                }
                moved_from.insert(old.rel);
            } else {
                // A hard link (the old path still has the inode), or the old
                // path is under an unread directory: a new file.
                out.rework.push(WorkItem {
                    need_fp: true,
                    ..item
                });
            }
        }
        // Paths now holding another inode: a new file if their old content
        // moved away above, else the file was replaced in place.
        for (rel, mut item) in std::mem::take(&mut self.replaced) {
            item.work = if moved_from.contains(&rel) {
                Work::NewFinal
            } else {
                Work::Changed
            };
            out.rework.push(item);
        }
        // Fingerprint matches of vanished files: moves across directories
        // or devices (copy + delete), where the inode changed.
        let mut by_fp: HashMap<(u128, u64), Vec<&str>> = HashMap::new();
        for (rel, row) in &vanished {
            if let Some(fp) = row.fp128 {
                by_fp.entry((fp, row.size)).or_default().push(rel);
            }
        }
        for cand in std::mem::take(&mut *lock(&ctx.deferred)) {
            let f = &cand.upsert.facts;
            let hit = f
                .fp128
                .and_then(|fp| by_fp.get_mut(&(fp, f.size)))
                .and_then(|v| pick_fp_source(v, &f.rel_path));
            match hit.and_then(|rel| vanished.remove(rel).map(|row| (rel, row))) {
                Some((rel, row)) => {
                    out.expect.push(guard(row.id, rel, row.size, row.mtime_ns));
                    out.moves.push(FileMove {
                        file_id: row.id,
                        to: cand.upsert.facts,
                        guess: cand.upsert.guess,
                    })
                }
                None => out.upserts.push(cand.upsert),
            }
        }
        let mut missing: Vec<(&str, &FileRow)> = vanished.into_iter().collect();
        missing.sort_by_key(|(_, r)| r.id);
        for (rel, row) in missing {
            out.missing.push(row.id);
            out.expect.push(guard(row.id, rel, row.size, row.mtime_ns));
        }
        Progress::add(&p.files_done, out.moves.len() as u64);
        out
    }
}

/// The `st_dev` of a directory (through a link), bounded.
fn dev_of_path(w: &Walk<'_>, abs: &Path) -> Option<u64> {
    let out = w.ctx.inner.root_stat.stat(abs, false).ok()?;
    let st = out.as_stat()?;
    let meta = st.target.as_ref().unwrap_or(&st.metadata);
    dev_of(meta)
}

#[cfg(unix)]
fn dev_of(m: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(m.dev())
}

#[cfg(not(unix))]
fn dev_of(_m: &std::fs::Metadata) -> Option<u64> {
    None
}

/// An empty directory whose cataloged files were on device `row_dev`, now
/// itself on `dir_dev`: unmounted (keep the files) unless the device was
/// merely renumbered (a remount seen on other files of this walk), in
/// which case the files really are gone. On one device it is a deletion.
fn unmounted_rule(dir_dev: u64, row_dev: u64, renumbered: &HashMap<u64, u64>) -> bool {
    dir_dev != row_dev && renumbered.get(&row_dev) != Some(&dir_dev)
}

/// Among vanished files with the same content, the likeliest source of a
/// move to `new_rel`: same file name, else same folder name, else any.
fn pick_fp_source<'a>(cands: &mut Vec<&'a str>, new_rel: &str) -> Option<&'a str> {
    let base = |r: &str| r.rsplit('/').next().unwrap_or("").to_string();
    let folder = |r: &str| parent_rel(r).rsplit('/').next().unwrap_or("").to_string();
    let i = cands
        .iter()
        .position(|c| base(c) == base(new_rel))
        .or_else(|| cands.iter().position(|c| folder(c) == folder(new_rel)))
        .or(if cands.is_empty() {
            None
        } else {
            Some(cands.len() - 1)
        })?;
    Some(cands.swap_remove(i))
}

#[derive(Default)]
struct Reconciled {
    moves: Vec<FileMove>,
    missing: Vec<Id>,
    /// Snapshot rows the moves and tombstones were decided from.
    expect: Vec<ExpectedRow>,
    /// Ready upserts (fingerprint candidates that were copies).
    upserts: Vec<FileUpsert>,
    /// Files still to process.
    rework: Vec<WorkItem>,
}

/// `.plexignore` / `.ignore` rules of the directories between the root and
/// `dir` (exclusive): a directory scan starts below them, so walk_scan would
/// not see them. `dir`'s own files are read by its listing.
fn ancestor_ignores(root: &Path, dir_rel: &str) -> Option<Arc<IgnoreMatcher>> {
    let mut m = IgnoreMatcher::new();
    let mut found = false;
    let mut at = root.to_path_buf();
    for part in dir_rel.split('/').filter(|s| !s.is_empty()) {
        for name in [".plexignore", ".ignore"] {
            let f = at.join(name);
            if let Ok(text) = std::fs::read_to_string(&f) {
                if m.add_from_contents(&f, &text).is_ok() {
                    found = true;
                }
            }
        }
        at.push(part);
    }
    found.then(|| Arc::new(m))
}

/// Apply one batch, account for it, publish events. `files` = how many
/// files it decides (for progress).
pub(crate) fn apply(inner: &Arc<Inner>, job: &Arc<JobShared>, batch: ScanBatch, files: u64) {
    let p = &job.progress;
    // The first write of a library scan gives the catalog writer its bulk
    // cache; the job ends bulk mode when it finishes (engine.rs), which
    // also truncates the WAL. A watcher batch writes a few rows: no switch.
    if job.is_library() && !job.bulk.swap(true, std::sync::atomic::Ordering::Relaxed) {
        if let Err(e) = inner.catalog.writer().begin_bulk().wait() {
            tracing::debug!(error = %e, "begin_bulk failed");
        }
    }
    let probed = probed_count(&batch.upserts);
    Progress::add(&p.writer_commands, 1);
    let t = Instant::now();
    let res = inner.catalog.writer().apply(batch).wait();
    Progress::add(&p.apply_ns, ms(t));
    Progress::add(&p.files_done, files);
    Progress::add(&p.files_probed, probed);
    match res {
        Ok(o) => {
            Progress::add(&p.added, o.files_added);
            Progress::add(&p.changed, o.files_changed);
            Progress::add(&p.moved, o.files_moved);
            Progress::add(&p.missing, o.files_missing);
            Progress::add(&p.reattached, o.files_reattached);
            Progress::add(&p.items_added, o.items_added.len() as u64);
            Progress::add(&p.items_updated, o.items_updated.len() as u64);
            Progress::add(&p.items_removed, o.items_removed.len() as u64);
            if !(o.items_added.is_empty() && o.items_updated.is_empty() && o.items_removed.is_empty()) {
                inner.emit(Event::ItemsChanged {
                    library_id: job.library_id,
                    added: o.items_added,
                    updated: o.items_updated,
                    removed: o.items_removed,
                });
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "catalog apply failed");
            *lock(&p.last_error) = Some(format!("catalog: {e}"));
        }
    }
    inner.progress_event(job);
}

/// Collects upserts (and cross-root moves) into transactions of
/// ≤ `batch_rows` files or ≤ `batch_interval`, whichever comes first.
fn batcher(inner: &Arc<Inner>, job: &Arc<JobShared>, root_id: Id, rx: Receiver<Out>) {
    let rows = inner.cfg.batch_rows.max(1);
    let interval = inner.cfg.batch_interval;
    let mut batch = ScanBatch {
        root_id,
        ..Default::default()
    };
    let add = |b: &mut ScanBatch, o: Out| match o {
        Out::Upsert(u) => b.upserts.push(u),
        Out::Move(m, e) => {
            b.moves.push(m);
            b.expect.push(e);
        }
    };
    while let Ok(first) = rx.recv() {
        add(&mut batch, first);
        let deadline = Instant::now() + interval;
        let mut closed = false;
        while batch.upserts.len() + batch.moves.len() < rows {
            match rx.recv_deadline(deadline) {
                Ok(o) => add(&mut batch, o),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    closed = true;
                    break;
                }
            }
        }
        let n = (batch.upserts.len() + batch.moves.len()) as u64;
        let full = std::mem::replace(
            &mut batch,
            ScanBatch {
                root_id,
                ..Default::default()
            },
        );
        apply(inner, job, full, n);
        if closed {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_mount_point_on_another_device_is_unmounted_not_emptied() {
        let none = HashMap::new();
        // disk2 (dev 7) mounted at a directory of the root's volume (dev 1):
        // unmounted, the bare mount point is on dev 1.
        assert!(unmounted_rule(1, 7, &none));
        // A directory emptied on its own volume: a real deletion.
        assert!(!unmounted_rule(1, 1, &none));
        // The whole volume was remounted with a new number (7 → 9), seen on
        // other files of the walk: an emptied directory there is a deletion.
        let renum: HashMap<u64, u64> = [(7, 9)].into_iter().collect();
        assert!(!unmounted_rule(9, 7, &renum));
        assert!(unmounted_rule(1, 7, &renum));
    }

    #[test]
    fn a_row_without_an_identity_takes_the_walks_without_a_reprobe() {
        let row = FileRow {
            id: 1,
            dev: None,
            ino: None,
            size: 10,
            mtime_ns: 5,
            btime_ns: None,
            fp128: Some(1),
            state: FileState::Present,
            probe_version: Some(PROBE_VERSION),
            parse_version: Some(PARSER_VERSION),
            asset_id: Some(1),
            probe_retry_at: None,
        };
        let snap: HashMap<String, FileRow> = [("Ext/a.mkv".to_string(), row)].into_iter().collect();
        let facts = |dev, ino| FileFacts {
            rel_path: "Ext/a.mkv".into(),
            dev,
            ino,
            btime_ns: None,
            size: 10,
            mtime_ns: 5,
            fp128: None,
            oshash: None,
        };
        let c = |f: &FileFacts| classify(&snap, &HashMap::new(), f, ScanMode::Quick, 0, false);
        assert!(matches!(c(&facts(Some(7), Some(9))), Class::Keep));
        assert!(matches!(c(&facts(None, None)), Class::Unchanged(false)));
    }

    #[test]
    fn fp_move_source_prefers_name_then_folder() {
        let mut c = vec!["A/x.mkv", "B/y.mkv", "C/z.mkv"];
        assert_eq!(pick_fp_source(&mut c, "D/y.mkv"), Some("B/y.mkv"));
        let mut c = vec!["A/x.mkv", "Q/C/z.mkv"];
        assert_eq!(pick_fp_source(&mut c, "C/w.mkv"), Some("Q/C/z.mkv"));
    }
}
