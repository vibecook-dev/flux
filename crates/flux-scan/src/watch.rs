//! Change watching: Mille's `Watcher` (FSEvents / inotify /
//! ReadDirectoryChangesW through notify), its O(n) `coalesce_events`, and a
//! debounce thread that turns a burst of events into one `scan_dirs` of the
//! affected parent directories at `Priority::Watcher`.
//!
//! - The notify callback never blocks: it hands batches over a bounded
//!   channel and, if that is full, flags an overflow, which (like Mille's
//!   `Coarse`) becomes a Quick scan of the library.
//! - Events are mapped from the canonical paths FSEvents reports back to
//!   the catalog's root paths. Hidden paths and NAS/OS litter are dropped.
//! - Coalescing is stateful (`CoalesceOptions::stateful`): a create+delete of
//!   one path inside a window is reported, not cancelled; FSEvents merges
//!   flags per path, so that can be a cataloged file being deleted.
//! - On `watch()`, a catch-up runs once: Mille resume snapshot +
//!   `events_since` where configured and present, else a Quick scan.
//! - Unavailable roots are tried again on a timer
//!   (`ScannerOptions::recheck_unavailable`). A watch doesn't notice its
//!   root being renamed: the handle follows the folder, so the next event
//!   finds the root's path gone and marks it unavailable, and renaming it
//!   back produces no event. On Windows the root's *parent* can't be renamed
//!   at all while it is watched (the open watch handle forbids it).

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, RecvTimeoutError, Sender};
use flux_catalog::{scan_support, RootState};
use flux_core::api::Id;
use mille_core::{coalesce_events_with, CoalesceOptions, FsChangeEvent, RawEvent, Watcher, WatcherOptions};

use crate::engine::Inner;
use crate::job::{lock, JobKind};
use crate::{paths, Priority, ScanError, ScanMode, StartupCatchUp};

enum Msg {
    Events(Vec<FsChangeEvent>),
    Overflow,
}

/// A watched library. Dropping it stops the watcher and its thread.
pub(crate) struct LibraryWatch {
    watcher: Option<Watcher>,
    map: Arc<Mutex<RootMap>>,
    stop: Arc<AtomicBool>,
    tx: Option<Sender<Msg>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for LibraryWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Stop events first, then the debounce thread.
        self.watcher.take();
        self.tx.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// `(canonical root as the OS reports events, root as the catalog stores it)`.
type RootMap = Vec<(PathBuf, PathBuf)>;

/// `with_catch_up`: also catch up with what changed while nothing watched
/// ([`crate::ScannerOptions::startup`]) once the watcher is up.
pub(crate) fn watch(inner: &Arc<Inner>, library_id: Id, with_catch_up: bool) -> Result<(), ScanError> {
    if inner.shutting_down.load(Ordering::Relaxed) {
        return Err(ScanError::ShuttingDown);
    }
    if lock(&inner.watches).contains_key(&library_id) {
        return Ok(());
    }
    let roots: Vec<_> = inner
        .catalog
        .read_blocking(scan_support::list_roots)?
        .into_iter()
        .filter(|r| r.library_id == library_id)
        .collect();
    if roots.is_empty() {
        let exists = inner
            .catalog
            .read_blocking(move |c| flux_catalog::read::library(c, library_id))?
            .is_some();
        if !exists {
            return Err(ScanError::UnknownLibrary(library_id));
        }
    }
    let mut map: RootMap = Vec::new();
    for r in &roots {
        let path = PathBuf::from(&r.path);
        let canonical = inner
            .stat_root(&path)
            .map(|s| s.canonical)
            .unwrap_or_else(|_| path.clone());
        map.push((canonical, path));
    }

    let (tx, rx) = bounded::<Msg>(4096);
    let overflow = Arc::new(AtomicBool::new(false));
    let cb_tx = tx.clone();
    let cb_overflow = overflow.clone();
    let watcher = Watcher::new_batch_with_options(
        move |batch: Vec<RawEvent>| {
            // Stateful: a path created and deleted in one window is reported
            // (Deleted, or Created if it is back), never dropped as
            // transient. FSEvents merges a path's flags, so that shape can
            // be a cataloged file being deleted.
            let events = coalesce_events_with(&batch, CoalesceOptions::stateful());
            if cb_tx.try_send(Msg::Events(events)).is_err() && !cb_overflow.swap(true, Ordering::Relaxed) {
                let _ = cb_tx.try_send(Msg::Overflow);
            }
        },
        WatcherOptions {
            recursive: true,
            // Zero = notify's raw stream (no debouncer).
            debounce_ms: (!inner.opts.watch_debounce.is_zero())
                .then(|| inner.opts.watch_debounce.as_millis().max(1) as u64),
        },
    )
    .map_err(|e| ScanError::Other(format!("watcher: {e}")))?;
    for (canonical, path) in &map {
        let opts = WatcherOptions {
            recursive: true,
            debounce_ms: None,
        };
        if let Err(e) = watcher.watch(canonical, opts) {
            tracing::warn!(root = %path.display(), error = %e, "root not watched");
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let weak = Arc::downgrade(inner);
    let settle = inner.opts.watch_settle;
    let map = Arc::new(Mutex::new(map));
    let thread = {
        let (stop, map, weak) = (stop.clone(), map.clone(), weak.clone());
        std::thread::Builder::new()
            .name(format!("flux-scan-watch-{library_id}"))
            .spawn(move || debounce_loop(weak, library_id, map, rx, overflow, stop, settle))
            .map_err(|e| ScanError::Other(format!("spawn watcher thread: {e}")))?
    };
    // Not joined on unwatch: a stat of a dead NAS may take its whole
    // first-response deadline, and shutdown must not wait for it. The thread
    // sees `stop` within a tick and holds the engine only while it checks.
    let every = inner.opts.recheck_unavailable;
    if !every.is_zero() {
        let stop = stop.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("flux-scan-recheck-{library_id}"))
            .spawn(move || recheck_loop(weak, library_id, stop, every));
        if let Err(e) = spawned {
            tracing::warn!(library = library_id, error = %e, "unavailable roots won't be retried by themselves");
        }
    }
    lock(&inner.watches).insert(
        library_id,
        LibraryWatch {
            watcher: Some(watcher),
            map,
            stop,
            tx: Some(tx),
            thread: Some(thread),
        },
    );

    // Catch up with what changed while nobody was watching.
    if !with_catch_up {
        return Ok(());
    }
    let inner2 = inner.clone();
    let mode = inner.opts.startup;
    let _ = std::thread::Builder::new()
        .name(format!("flux-scan-catchup-{library_id}"))
        .spawn(move || catch_up(&inner2, library_id, mode));
    Ok(())
}

fn catch_up(inner: &Arc<Inner>, library_id: Id, mode: StartupCatchUp) {
    match mode {
        StartupCatchUp::Nothing => {}
        StartupCatchUp::QuickScan => {
            inner.request(library_id, JobKind::Library(ScanMode::Quick), Priority::Scheduled);
        }
        StartupCatchUp::MilleResume => match crate::resume::catch_up(inner, library_id) {
            Some(c) => {
                if !c.dirs.is_empty() {
                    let job = inner.request(library_id, JobKind::Dirs(c.dirs), Priority::Watcher);
                    // Save the next tokens only once the catch-up is applied.
                    if job.done.wait().is_err() {
                        return;
                    }
                }
                crate::resume::save_tokens(&c.next);
            }
            None => {
                inner.request(library_id, JobKind::Library(ScanMode::Quick), Priority::Scheduled);
            }
        },
    }
}

/// After a successful scan of a watched library's root: make sure the root
/// is watched (it may have been unreachable when `watch()` ran: a NAS that
/// was asleep, a drive plugged in later) and that events map back to it.
pub(crate) fn ensure_watched(inner: &Inner, library_id: Id, root: &Path, canonical: &Path) {
    let watches = lock(&inner.watches);
    let Some(w) = watches.get(&library_id) else { return };
    let Some(watcher) = w.watcher.as_ref() else { return };
    if watcher.watched_roots().iter().any(|r| r == canonical) {
        return;
    }
    let opts = WatcherOptions {
        recursive: true,
        debounce_ms: None,
    };
    match watcher.watch(canonical, opts) {
        Ok(()) => {
            tracing::info!(root = %root.display(), "root is reachable again; watching it");
            let mut map = lock(&w.map);
            map.retain(|(_, r)| r != root);
            map.push((canonical.to_path_buf(), root.to_path_buf()));
        }
        Err(e) => tracing::debug!(root = %root.display(), error = %e, "root still not watchable"),
    }
}

/// Longest wait between two tries of a root that answers but whose scan
/// still finds it unavailable.
const RECHECK_MAX: Duration = Duration::from_secs(3600);

/// Every `every`, stat the library's unavailable roots; when one answers,
/// a Quick scan brings it back (the scan marks it available and watches it
/// again). A root whose scan still can't use it waits twice as long before
/// the next try, so an empty mount point isn't rescanned every interval.
fn recheck_loop(inner: Weak<Inner>, library_id: Id, stop: Arc<AtomicBool>, every: Duration) {
    // Root id → (next try, current wait).
    let mut backoff: HashMap<Id, (Instant, Duration)> = HashMap::new();
    let mut next = Instant::now() + every;
    loop {
        while Instant::now() < next {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(
                next.saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(100)),
            );
        }
        if stop.load(Ordering::Relaxed) {
            return;
        }
        next = Instant::now() + every;
        let Some(inner) = inner.upgrade() else { return };
        if inner.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        let Ok(roots) = inner.catalog.read_blocking(scan_support::list_roots) else {
            continue;
        };
        let down: Vec<_> = roots
            .into_iter()
            .filter(|r| r.library_id == library_id && r.state == RootState::Unavailable)
            .collect();
        backoff.retain(|id, _| down.iter().any(|r| r.id == *id));
        let now = Instant::now();
        for r in &down {
            let (due, wait) = backoff.get(&r.id).copied().unwrap_or((now, every));
            if now < due || stop.load(Ordering::Relaxed) {
                continue;
            }
            if inner.stat_root(Path::new(&r.path)).is_err() {
                continue;
            }
            tracing::info!(root = %r.path, "an unavailable root answers again; rescanning the library");
            inner.request(library_id, JobKind::Library(ScanMode::Quick), Priority::Watcher);
            let wait = (wait * 2).min(RECHECK_MAX);
            backoff.insert(r.id, (now + wait, wait));
            // One library scan covers every root.
            break;
        }
    }
}

pub(crate) fn unwatch(inner: &Inner, library_id: Id) {
    let w = lock(&inner.watches).remove(&library_id);
    drop(w);
}

pub(crate) fn unwatch_all(inner: &Inner) {
    let all: Vec<LibraryWatch> = lock(&inner.watches).drain().map(|(_, w)| w).collect();
    drop(all);
}

/// Collect events until the library has been quiet for `settle` (or one
/// second passed since the first event), then rescan the parents.
fn debounce_loop(
    inner: Weak<Inner>,
    library_id: Id,
    map: Arc<Mutex<RootMap>>,
    rx: crossbeam_channel::Receiver<Msg>,
    overflow: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    settle: Duration,
) {
    let max_delay = (settle * 4).max(Duration::from_secs(1));
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let mut full = false;
    let mut first: Option<Instant> = None;
    let mut last = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let wait = match first {
            None => Duration::from_millis(200),
            Some(f) => {
                let quiet = settle.saturating_sub(last.elapsed());
                let cap = max_delay.saturating_sub(f.elapsed());
                quiet.min(cap)
            }
        };
        match rx.recv_timeout(wait) {
            Ok(Msg::Events(evs)) => {
                let map = lock(&map);
                for ev in evs {
                    map_event(&map, ev, &mut dirs, &mut full);
                }
                first.get_or_insert_with(Instant::now);
                last = Instant::now();
            }
            Ok(Msg::Overflow) => {
                full = true;
                overflow.store(false, Ordering::Relaxed);
                first.get_or_insert_with(Instant::now);
                last = Instant::now();
            }
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let Some(f) = first else { continue };
        if last.elapsed() < settle && f.elapsed() < max_delay {
            continue;
        }
        let Some(inner) = inner.upgrade() else { return };
        if full {
            inner.request(library_id, JobKind::Library(ScanMode::Quick), Priority::Watcher);
        } else if !dirs.is_empty() {
            let list: Vec<PathBuf> = std::mem::take(&mut dirs).into_iter().collect();
            tracing::debug!(library = library_id, dirs = list.len(), "watcher batch");
            inner.request(library_id, JobKind::Dirs(list), Priority::Watcher);
        }
        dirs.clear();
        full = false;
        first = None;
    }
}

/// The catalog-side directory to rescan for one event.
fn map_event(map: &RootMap, ev: FsChangeEvent, dirs: &mut BTreeSet<PathBuf>, full: &mut bool) {
    let mut push = |p: &Path| {
        for (canonical, root) in map {
            let Some(rel) = paths::rel_of(canonical, p) else {
                continue;
            };
            if rel.is_empty() {
                dirs.insert(root.clone());
                return;
            }
            if paths::has_hidden_component(&rel) || flux_names::is_ignored(Path::new(&rel)) {
                return;
            }
            let parent = paths::parent_rel(&rel);
            dirs.insert(if parent.is_empty() {
                root.clone()
            } else {
                root.join(parent)
            });
            return;
        }
    };
    match ev {
        FsChangeEvent::Created { path }
        | FsChangeEvent::Modified { path }
        | FsChangeEvent::Deleted { path }
        | FsChangeEvent::Unknown { path } => push(&path),
        FsChangeEvent::Renamed { from, to } => {
            push(&from);
            push(&to);
        }
        FsChangeEvent::RenameDegraded { missing_half, .. } => push(&missing_half),
        FsChangeEvent::Coarse { .. } => *full = true,
        FsChangeEvent::Error { message } => tracing::debug!(message, "watcher error"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_map_to_parent_dirs_of_catalog_roots() {
        let map: RootMap = vec![(PathBuf::from("/private/var/m"), PathBuf::from("/var/m"))];
        // Dirs are the catalog root joined with the event's relative parent
        // (`Path::join`: `\` on Windows).
        let dir = |rel: &[&str]| rel.iter().fold(PathBuf::from("/var/m"), |p, c| p.join(c));
        let mut dirs = BTreeSet::new();
        let mut full = false;
        for ev in [
            FsChangeEvent::Created {
                path: "/private/var/m/Show/S01/e1.mkv".into(),
            },
            FsChangeEvent::Deleted {
                path: "/private/var/m/top.mkv".into(),
            },
            FsChangeEvent::Modified {
                path: "/private/var/m/Show/.DS_Store".into(),
            },
            FsChangeEvent::Created {
                path: "/private/var/m/@eaDir/x".into(),
            },
            FsChangeEvent::Renamed {
                from: "/private/var/m/A/a.mkv".into(),
                to: "/private/var/m/B/a.mkv".into(),
            },
            FsChangeEvent::Created {
                path: "/elsewhere/x.mkv".into(),
            },
        ] {
            map_event(&map, ev, &mut dirs, &mut full);
        }
        let got: Vec<_> = dirs.iter().cloned().collect();
        assert_eq!(got, [dir(&[]), dir(&["A"]), dir(&["B"]), dir(&["Show", "S01"])]);
        assert!(!full);
        map_event(
            &map,
            FsChangeEvent::Coarse {
                root: "/private/var/m".into(),
            },
            &mut dirs,
            &mut full,
        );
        assert!(full);
    }
}
