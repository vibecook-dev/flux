//! Startup catch-up through Mille's crash-resume snapshot (`events_since`).
//!
//! After a complete full scan of a library, the scanner writes one Mille
//! `ResumeSnapshot` per root next to the catalog database
//! (`<db>.scan-resume-root-<id>.bin`), built from the entries that walk
//! already delivered (no second walk). On the next start, `watch()` loads
//! it, asks Mille's `events_since` what changed, and rescans only the
//! parent directories of those paths.
//!
//! Note what `events_since` is: a full re-walk of the root diffed against
//! the snapshot on `(path, mtime_ms, size)` — not an FSEvents event-id
//! replay (research/07 §2.8, §6.3). See docs/prototype/scan.md for how it
//! compares with simply running a Quick scan.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use flux_catalog::scan_support;
use flux_core::api::Id;
use mille_core::{
    DirCtx, Durability, EntryKind, HistoryToken, Replay, ReplayOptions, ResumeEvent, RootStat, ScanEntry,
    WalkedEntry,
};

use crate::engine::Inner;
use crate::paths;

/// What a complete full walk of one root saw, for the snapshot.
pub(crate) struct ResumeData {
    pub root_id: Id,
    pub root: PathBuf,
    pub root_mtime_ms: i64,
    pub entries: Vec<WalkedEntry>,
    /// Captured before the walk (macOS; `None` where there is no journal).
    pub history: Option<HistoryToken>,
}

/// More directories than this to rescan and a Quick scan is cheaper (Mille
/// Phase D: 1,000 depth-1 rescans lost to one full walk of 21k entries).
const MAX_CATCH_UP_DIRS: usize = 256;

/// A startup catch-up: the directories to rescan, and the history tokens to
/// save once that rescan is applied.
pub(crate) struct CatchUp {
    pub dirs: Vec<PathBuf>,
    pub next: Vec<(PathBuf, HistoryToken)>,
}

/// Where a root's history token lives.
pub(crate) fn history_path(inner: &Inner, root_id: Id) -> PathBuf {
    let mut p = snapshot_path(inner, root_id).into_os_string();
    p.push(".history");
    PathBuf::from(p)
}

/// Save tokens (best effort: a missing token means a Quick scan next time).
pub(crate) fn save_tokens(tokens: &[(PathBuf, HistoryToken)]) {
    for (path, token) in tokens {
        if let Err(e) = token.write(path) {
            tracing::warn!(path = %path.display(), error = %e, "history token not written");
        }
    }
}

fn mtime_ms(ns: i64) -> i64 {
    // `walk_bounded` truncates `SystemTime` to whole milliseconds.
    if ns >= 0 {
        ns / 1_000_000
    } else {
        -((-ns + 999_999) / 1_000_000)
    }
}

/// The `WalkedEntry` Mille's own walker would have produced for `e`.
pub(crate) fn walked_entry(dir: &DirCtx<'_>, e: &ScanEntry<'_>, name: &str) -> WalkedEntry {
    // `events_since` walks without following links: a link is a Symlink
    // entry there (size 0).
    let kind = if e.is_symlink { EntryKind::Symlink } else { e.kind };
    WalkedEntry {
        path: dir.join(e.name),
        parent_path: Some(dir.path.to_path_buf()),
        name: name.to_string(),
        kind,
        size: if kind == EntryKind::File { e.size } else { 0 },
        mtime_ms: mtime_ms(e.mtime_ns),
        ctime_ms: mtime_ms(e.mtime_ns),
        is_symlink: e.is_symlink,
        symlink_target_is_dir: e.is_symlink.then(|| e.is_dir()),
        is_readonly: false,
        is_hidden: false,
        depth: (dir.depth + 1).min(u16::MAX as usize) as u16,
    }
}

/// Where a root's snapshot lives.
pub(crate) fn snapshot_path(inner: &Inner, root_id: Id) -> PathBuf {
    let db = inner.catalog.path();
    let name = db
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "catalog.db".into());
    db.with_file_name(format!("{name}.scan-resume-root-{root_id}.bin"))
}

/// Write the snapshot for one root (best effort: a failure only means the
/// next start runs a Quick scan).
pub(crate) fn save(inner: &Inner, data: &ResumeData) {
    let t = std::time::Instant::now();
    let path = snapshot_path(inner, data.root_id);
    let stats = [RootStat {
        path: data.root.clone(),
        mtime_ms: data.root_mtime_ms,
    }];
    // The snapshot only saves work: unsynced (a stale or unreadable one
    // means a Quick scan), and no EntryStore.
    let res = mille_core::write_snapshot_from_with(&data.entries, &stats, &path, Durability::Unsynced);
    match res {
        Ok(()) => tracing::debug!(
            root = %data.root.display(),
            entries = data.entries.len(),
            ms = t.elapsed().as_millis() as u64,
            "wrote resume snapshot"
        ),
        Err(e) => tracing::warn!(root = %data.root.display(), error = %e, "resume snapshot not written"),
    }
    if let Some(token) = &data.history {
        save_tokens(&[(history_path(inner, data.root_id), token.clone())]);
    }
}

/// The parents to rescan for one root from its FSEvents history since the
/// saved token, and the token to save after. `None`: no token, no journal,
/// or the journal cannot answer (the caller falls back).
fn from_history(inner: &Inner, root_id: Id, root: &Path) -> Option<(Vec<PathBuf>, HistoryToken)> {
    let token = HistoryToken::read(&history_path(inner, root_id)).ok()?;
    if token.root != root {
        return None;
    }
    let report = match mille_core::replay_since(&token, &ReplayOptions::default()) {
        Ok(r) => r,
        Err(e) => {
            tracing::info!(root = %root.display(), error = %e, "history replay failed");
            return None;
        }
    };
    let changes = match report.replay {
        Replay::Changes(c) if c.rescan_subtrees.is_empty() => c,
        Replay::Changes(_) => return None,
        Replay::Rescan(reason) => {
            tracing::info!(root = %root.display(), ?reason, "history cannot answer; Quick scan");
            return None;
        }
    };
    let mut dirs = BTreeSet::new();
    for e in &changes.changed {
        if relevant(root, &e.path, false) || e.path == root {
            push_parent(&mut dirs, root, &e.path);
        }
    }
    tracing::debug!(
        root = %root.display(),
        records = changes.records,
        dirs = dirs.len(),
        ms = report.elapsed.as_millis() as u64,
        "history replay"
    );
    Some((dirs.into_iter().collect(), report.next))
}

/// Directories to rescan after a restart, from every root's snapshot.
/// `None` when any root has no usable snapshot or its diff failed (an
/// unread directory, a vanished root): the caller runs a Quick scan.
pub(crate) fn catch_up(inner: &Arc<Inner>, library_id: Id) -> Option<CatchUp> {
    let roots: Vec<_> = inner
        .catalog
        .read_blocking(scan_support::list_roots)
        .ok()?
        .into_iter()
        .filter(|r| r.library_id == library_id)
        .collect();
    if roots.is_empty() {
        return None;
    }
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let mut next = Vec::new();
    for r in &roots {
        if let Some((d, token)) = from_history(inner, r.id, Path::new(&r.path)) {
            dirs.extend(d);
            next.push((history_path(inner, r.id), token));
            continue;
        }
        let snap = mille_core::read_snapshot(&snapshot_path(inner, r.id)).ok()?;
        let events = match mille_core::events_since(&snap) {
            Ok(ev) => ev,
            Err(e) => {
                tracing::info!(root = %r.path, error = %e, "resume diff failed; falling back to a Quick scan");
                return None;
            }
        };
        let root = Path::new(&r.path);
        for ev in events {
            match ev {
                ResumeEvent::RootDisappeared { .. } => return None,
                ResumeEvent::Appeared { path } => {
                    if relevant(root, &path, true) {
                        push_parent(&mut dirs, root, &path);
                    }
                }
                ResumeEvent::Disappeared { path } => {
                    if relevant(root, &path, false) {
                        push_parent(&mut dirs, root, &path);
                    }
                }
                ResumeEvent::Modified { path } => {
                    if relevant(root, &path, false) {
                        push_parent(&mut dirs, root, &path);
                        if path.is_dir() {
                            dirs.insert(path);
                        }
                    }
                }
            }
        }
    }
    if dirs.len() > MAX_CATCH_UP_DIRS {
        return None;
    }
    Some(CatchUp {
        dirs: dirs.into_iter().collect(),
        next,
    })
}

fn push_parent(dirs: &mut BTreeSet<PathBuf>, root: &Path, path: &Path) {
    match path.parent() {
        Some(p) if p.starts_with(root) => {
            dirs.insert(p.to_path_buf());
        }
        _ => {
            dirs.insert(root.to_path_buf());
        }
    }
}

/// `events_since` walks everything (hidden files, NAS litter, `.txt`…);
/// the snapshot only has what the media walk keeps. Drop what the media
/// walk would never deliver, so it doesn't trigger a rescan every start.
fn relevant(root: &Path, path: &Path, appeared: bool) -> bool {
    let Some(rel) = paths::rel_of(root, path) else {
        return false;
    };
    if paths::has_hidden_component(&rel) || flux_names::is_ignored(Path::new(&rel)) {
        return false;
    }
    if appeared && !path.is_dir() {
        let name = rel.rsplit('/').next().unwrap_or("");
        let ext = name.rsplit_once('.').map_or("", |(_, e)| e);
        return flux_core::is_video_extension(ext) || flux_core::is_sidecar_extension(ext);
    }
    true
}
