//! Scanner support: the snapshot the scanner diffs against, and the batched
//! apply of its diff (files → assets → items, in one savepoint).
//!
//! The scanner's loop (research/02 §7.2 "diff in memory rather than
//! UPSERT-all"):
//!
//! 1. [`load_root_snapshot`] (one indexed scan; ≈25–32 ms per 100k rows in
//!    the research, see the `catalog-bench` numbers for this schema).
//! 2. Walk the root, compare each entry with [`crate::FileRow::same_stat`];
//!    detect moves by `(dev, ino)`.
//! 3. Send only the differences as a [`ScanBatch`]. An unchanged rescan sends
//!    nothing, so the catalog performs zero writes.
//!
//! Reads here run on a reader connection; `apply_batch`,
//! `set_root_availability` and `purge_tombstones` run on the writer thread
//! (see [`crate::writer::Writer`]).

use std::collections::{BTreeSet, HashMap, HashSet};

use flux_core::api::Id;
use flux_core::{GuessKind, NameGuess, ProbeResult, PARSER_VERSION};
use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{CatalogError, Result};
use crate::facts::{asset_facts, merge_badges, AssetFacts};
use crate::group::{self, Placement, Resolver};
use crate::model::{
    clamp_i64, compact_json, enum_to_text, fp128_from_blob, fp128_to_blob, now_ms, parse_library_kind,
    u64_from_sql, u64_to_sql, ApplyOutcome, AvailabilityChange, ExpectedRow, FileFacts, FileFingerprint,
    FileMove, FileRow, FileState, FileUpsert, ProbeUpdate, PurgeOutcome, RootInfo, RootState, ScanBatch,
};

// ---- reads -------------------------------------------------------------------

fn file_row(r: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<FileRow> {
    // Borrow rather than allocate: this runs once per file on every rescan.
    let state = r.get_ref(offset + 7)?.as_str().unwrap_or("");
    Ok(FileRow {
        id: r.get(offset)?,
        dev: r.get::<_, Option<i64>>(offset + 1)?.map(u64_from_sql),
        ino: r.get::<_, Option<i64>>(offset + 2)?.map(u64_from_sql),
        size: r.get::<_, i64>(offset + 3)?.max(0) as u64,
        mtime_ns: r.get(offset + 4)?,
        btime_ns: r.get(offset + 5)?,
        fp128: r
            .get_ref(offset + 6)?
            .as_blob_or_null()?
            .and_then(fp128_from_blob),
        // Unknown strings cannot occur (CHECK constraint); map defensively.
        state: FileState::parse(state).unwrap_or(FileState::Missing),
        probe_version: r
            .get::<_, Option<i64>>(offset + 8)?
            .and_then(|v| u16::try_from(v).ok()),
        parse_version: r
            .get::<_, Option<i64>>(offset + 9)?
            .and_then(|v| u16::try_from(v).ok()),
        asset_id: r.get(offset + 10)?,
        probe_retry_at: r.get(offset + 11)?,
    })
}

const FILE_ROW_COLS: &str = "id, dev, ino, size, mtime_ns, btime_ns, fp128, state, probe_version, parse_version, asset_id, probe_retry_at";

/// Every file of a root keyed by `rel_path`, for the in-memory diff.
pub fn load_root_snapshot(conn: &Connection, root_id: Id) -> Result<HashMap<String, FileRow>> {
    let n: i64 = conn.query_row("SELECT count(*) FROM file WHERE root_id = ?1", [root_id], |r| {
        r.get(0)
    })?;
    let mut map = HashMap::with_capacity(n.max(0) as usize);
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT rel_path, {FILE_ROW_COLS} FROM file WHERE root_id = ?1"
    ))?;
    let mut rows = stmt.query([root_id])?;
    while let Some(r) = rows.next()? {
        map.insert(r.get::<_, String>(0)?, file_row(r, 1)?);
    }
    Ok(map)
}

/// The files of a root at or below `rel_prefix` (a directory, root-relative,
/// `""` = the whole root), keyed by `rel_path`: what a watcher batch that
/// rescans a few directories diffs against. One range scan of the
/// `(root_id, rel_path)` index instead of the whole root.
pub fn load_subtree_snapshot(
    conn: &Connection,
    root_id: Id,
    rel_prefix: &str,
) -> Result<HashMap<String, FileRow>> {
    let prefix = rel_prefix.trim_matches('/');
    if prefix.is_empty() {
        return load_root_snapshot(conn, root_id);
    }
    let mut map = HashMap::new();
    load_subtree_into(conn, root_id, prefix, &mut map)?;
    Ok(map)
}

/// [`load_subtree_snapshot`] for several directories into one map
/// (overlapping prefixes are fine).
pub fn load_subtrees_snapshot(
    conn: &Connection,
    root_id: Id,
    rel_prefixes: &[String],
) -> Result<HashMap<String, FileRow>> {
    if rel_prefixes.iter().any(|p| p.trim_matches('/').is_empty()) {
        return load_root_snapshot(conn, root_id);
    }
    let mut map = HashMap::new();
    for p in rel_prefixes {
        load_subtree_into(conn, root_id, p.trim_matches('/'), &mut map)?;
    }
    Ok(map)
}

fn load_subtree_into(
    conn: &Connection,
    root_id: Id,
    prefix: &str,
    map: &mut HashMap<String, FileRow>,
) -> Result<()> {
    // `prefix/` ≤ rel_path < `prefix0` ('0' follows '/'): every path below
    // the directory, and nothing that merely shares its name's start.
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT rel_path, {FILE_ROW_COLS} FROM file
          WHERE root_id = ?1 AND rel_path >= ?2 || '/' AND rel_path < ?2 || '0'"
    ))?;
    let mut rows = stmt.query(params![root_id, prefix])?;
    while let Some(r) = rows.next()? {
        map.insert(r.get::<_, String>(0)?, file_row(r, 1)?);
    }
    Ok(())
}

/// Directory mtimes of a root (for the O(dirs) poll fallback).
pub fn load_dirs(conn: &Connection, root_id: Id) -> Result<HashMap<String, i64>> {
    let mut stmt = conn.prepare_cached("SELECT rel_path, mtime_ns FROM dir WHERE root_id = ?1")?;
    let rows = stmt.query_map([root_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Files anywhere in the catalog with this content fingerprint (cross-root
/// move/copy detection). Returns `(root_id, rel_path, row)`.
pub fn find_by_fp128(conn: &Connection, fp128: u128, size: u64) -> Result<Vec<(Id, String, FileRow)>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT root_id, rel_path, {FILE_ROW_COLS} FROM file WHERE fp128 = ?1 AND size = ?2 ORDER BY id"
    ))?;
    let rows = stmt.query_map(params![fp128_to_blob(fp128).as_slice(), clamp_i64(size)], |r| {
        Ok((r.get(0)?, r.get(1)?, file_row(r, 2)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The file with this `(dev, ino)`, if any (rename detection across roots on
/// one volume). Returns `(root_id, rel_path, row)`.
pub fn find_by_dev_ino(conn: &Connection, dev: u64, ino: u64) -> Result<Option<(Id, String, FileRow)>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT root_id, rel_path, {FILE_ROW_COLS} FROM file WHERE dev = ?1 AND ino = ?2 ORDER BY id LIMIT 1"
    ))?;
    Ok(stmt
        .query_row(params![u64_to_sql(dev), u64_to_sql(ino)], |r| {
            Ok((r.get(0)?, r.get(1)?, file_row(r, 2)?))
        })
        .optional()?)
}

fn root_info_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(RootInfo, String, String)> {
    Ok((
        RootInfo {
            id: r.get(0)?,
            library_id: r.get(1)?,
            library_kind: flux_core::LibraryKind::Mixed,
            path: r.get(3)?,
            volume_uuid: r.get(4)?,
            cursor: r.get(5)?,
            state: RootState::Available,
            last_scan_started_at: r.get(7)?,
            last_scan_finished_at: r.get(8)?,
            last_scan_error: r.get(9)?,
        },
        r.get::<_, String>(2)?,
        r.get::<_, String>(6)?,
    ))
}

const ROOT_SELECT: &str = "SELECT r.id, r.library_id, l.kind, r.path, r.volume_uuid, r.cursor, r.state,
        r.last_scan_started_at, r.last_scan_finished_at, r.last_scan_error
   FROM root r JOIN library l ON l.id = r.library_id";

fn finish_root(t: (RootInfo, String, String)) -> Result<RootInfo> {
    let (mut info, kind, state) = t;
    info.library_kind = parse_library_kind(&kind)?;
    info.state = RootState::parse(&state)?;
    Ok(info)
}

/// All roots, for the scanner and the watcher.
pub fn list_roots(conn: &Connection) -> Result<Vec<RootInfo>> {
    let mut stmt = conn.prepare_cached(&format!("{ROOT_SELECT} ORDER BY r.id"))?;
    let rows = stmt
        .query_map([], root_info_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().map(finish_root).collect()
}

/// One root.
pub fn root_info(conn: &Connection, root_id: Id) -> Result<Option<RootInfo>> {
    let mut stmt = conn.prepare_cached(&format!("{ROOT_SELECT} WHERE r.id = ?1"))?;
    stmt.query_row([root_id], root_info_row)
        .optional()?
        .map(finish_root)
        .transpose()
}

// ---- writes (writer thread only) ---------------------------------------------

/// Where an asset's facts come from during placement.
#[derive(Clone, Copy)]
enum ProbeSrc<'a> {
    /// Use what is stored (only read back if a new asset must be created).
    Keep,
    /// A fresh probe (`None` = the probe failed).
    New(Option<&'a ProbeResult>),
}

/// Recompute bookkeeping shared by every write that changes file states or
/// asset links.
#[derive(Default)]
pub(crate) struct Touched {
    /// Leaf items (movie/episode/extra) whose files or links changed.
    pub leaves: BTreeSet<Id>,
    /// Items whose asset set changed (reported as updated even if their
    /// state/badges did not).
    pub linked: HashSet<Id>,
    /// Seasons or shows to recompute although no touched leaf is under them
    /// (a child was deleted).
    pub parents: BTreeSet<Id>,
}

impl Touched {
    fn file(&mut self, conn: &Connection, file_id: Id) -> Result<()> {
        let mut stmt = conn.prepare_cached(
            "SELECT ai.item_id FROM asset a JOIN asset_item ai ON ai.asset_id = a.id WHERE a.file_id = ?1",
        )?;
        for id in stmt.query_map([file_id], |r| r.get::<_, Id>(0))? {
            self.leaves.insert(id?);
        }
        Ok(())
    }
}

struct Apply<'c> {
    conn: &'c Connection,
    root_id: Id,
    grace_ms: i64,
    r: Resolver,
    t: Touched,
    out: ApplyOutcome,
    /// A movie/episode was placed: pending extras may now find a parent.
    placed_parent: bool,
    /// Extras that went pending in this batch (no point retrying them).
    pending_now: HashSet<Id>,
    /// Folders a film's file was placed directly in by this batch: files
    /// already placed as films in an extras-named subfolder of one may now
    /// be its extras (see [`group::extras_folder_by_word`]).
    movie_dirs: BTreeSet<String>,
    /// Snapshot guards for `missing` and `moves` (see [`ExpectedRow`]).
    expect: HashMap<Id, ExpectedRow>,
    /// Items this job deleted: works merged into the work their last file
    /// moved to (see [`crate::merge::carry_emptied`]).
    deleted: BTreeSet<Id>,
}

/// Backoff before a failed probe is retried: 1 min, doubling per failed
/// attempt (over the row's previous `probe_attempts`), capped at
/// [`PROBE_RETRY_MAX_MS`]; [`crate::FileRow::probe_retry_at`] holds the
/// result. The writer's SQL computes the same.
pub const PROBE_RETRY_BASE_MS: i64 = 60_000;
/// The longest probe-retry backoff: 1 day.
pub const PROBE_RETRY_MAX_MS: i64 = 86_400_000;

impl<'c> Apply<'c> {
    /// A job's state for `root_id`; also returns whether the root is
    /// stored as unavailable.
    fn new(conn: &'c Connection, root_id: Id, grace_ms: i64, job_max_attempts: u32) -> Result<(Self, bool)> {
        let now = now_ms();
        let (library_id, state): (Id, String) = conn
            .prepare_cached("SELECT library_id, state FROM root WHERE id = ?1")?
            .query_row([root_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?
            .ok_or_else(|| CatalogError::NotFound(format!("root {root_id}")))?;
        let mut resolver = Resolver::new(library_id, now);
        resolver.job_max_attempts = job_max_attempts;
        Ok((
            Apply {
                conn,
                root_id,
                grace_ms,
                r: resolver,
                t: Touched::default(),
                out: ApplyOutcome::default(),
                placed_parent: false,
                pending_now: HashSet::new(),
                movie_dirs: BTreeSet::new(),
                expect: HashMap::new(),
                deleted: BTreeSet::new(),
            },
            state == RootState::Unavailable.as_str(),
        ))
    }

    /// Recompute what the job touched and report it.
    fn finish(mut self) -> Result<ApplyOutcome> {
        let deleted = std::mem::take(&mut self.deleted);
        self.t.leaves.retain(|id| !deleted.contains(id));
        self.t.linked.retain(|id| !deleted.contains(id));
        self.t.parents.retain(|id| !deleted.contains(id));
        let (updated, mut removed) = recompute(self.conn, self.r.now, &self.t, &self.r.created_meta)?;
        self.out.items_added = self
            .r
            .created
            .iter()
            .copied()
            .filter(|id| !deleted.contains(id))
            .collect();
        self.out.items_updated = updated.into_iter().filter(|id| !deleted.contains(id)).collect();
        for id in deleted {
            if !removed.contains(&id) && !self.r.created.contains(&id) {
                removed.push(id);
            }
        }
        self.out.items_removed = removed;
        Ok(self.out)
    }
}

/// Apply one scanner batch. Runs inside the writer's savepoint; any error
/// rolls the whole batch back.
pub(crate) fn apply_batch(
    conn: &Connection,
    b: &ScanBatch,
    grace_ms: i64,
    job_max_attempts: u32,
) -> Result<ApplyOutcome> {
    let (mut ap, unavailable) = Apply::new(conn, b.root_id, grace_ms, job_max_attempts)?;
    let now = ap.r.now;
    ap.expect = b.expect.iter().map(|e| (e.file_id, e.clone())).collect();
    if unavailable {
        // A successful listing is proof the root is reachable again.
        set_availability_inner(conn, b.root_id, true, now, &mut ap.t)?;
    }

    for d in &b.dirs {
        conn.prepare_cached(
            "INSERT INTO dir(root_id, rel_path, mtime_ns) VALUES (?1, ?2, ?3)
             ON CONFLICT(root_id, rel_path) DO UPDATE SET mtime_ns = excluded.mtime_ns WHERE mtime_ns <> excluded.mtime_ns",
        )?
        .execute(params![b.root_id, d.rel_path, d.mtime_ns])?;
    }
    for d in &b.removed_dirs {
        conn.prepare_cached(
            "DELETE FROM dir WHERE root_id = ?1 AND (rel_path = ?2 OR (rel_path >= ?2 || '/' AND rel_path < ?2 || '0'))",
        )?
        .execute(params![b.root_id, d])?;
    }
    for &id in &b.missing {
        ap.mark_missing(id)?;
    }
    for m in &b.moves {
        ap.apply_move(m)?;
    }
    // Extras last, so a parent movie in the same batch already exists.
    let is_extra = |u: &&FileUpsert| {
        u.guess
            .as_ref()
            .is_some_and(|g| g.kind == GuessKind::Extra || word_extra_candidate(g, &u.facts.rel_path))
    };
    for u in b
        .upserts
        .iter()
        .filter(|u| !is_extra(u))
        .chain(b.upserts.iter().filter(is_extra))
    {
        ap.apply_upsert(u)?;
    }
    if ap.placed_parent {
        ap.retry_pending()?;
    }
    ap.regroup_word_extras()?;
    if let Some(cursor) = &b.cursor {
        conn.prepare_cached("UPDATE root SET cursor = ?2 WHERE id = ?1")?
            .execute(params![b.root_id, cursor])?;
    }
    ap.finish()
}

impl Apply<'_> {
    fn mark_missing(&mut self, file_id: Id) -> Result<()> {
        let until = self.r.now.saturating_add(self.grace_ms);
        let n = match self.expect.get(&file_id) {
            // Only while the row is still where (and what) the scanner's
            // snapshot said: a concurrent job may have moved it since.
            Some(e) => self
                .conn
                .prepare_cached(
                    "UPDATE file SET state = 'missing', tombstone_until = ?3, updated_at = ?4
                     WHERE id = ?1 AND root_id = ?2 AND state <> 'missing'
                       AND rel_path = ?5 AND size = ?6 AND mtime_ns = ?7",
                )?
                .execute(params![
                    file_id,
                    self.root_id,
                    until,
                    self.r.now,
                    e.rel_path,
                    clamp_i64(e.size),
                    e.mtime_ns
                ])?,
            None => self
                .conn
                .prepare_cached(
                    "UPDATE file SET state = 'missing', tombstone_until = ?3, updated_at = ?4
                     WHERE id = ?1 AND root_id = ?2 AND state <> 'missing'",
                )?
                .execute(params![file_id, self.root_id, until, self.r.now])?,
        };
        if n > 0 {
            self.out.files_missing += 1;
            self.t.file(self.conn, file_id)?;
        } else if self.expect.contains_key(&file_id) {
            tracing::debug!(
                file_id,
                "missing decided from a stale snapshot; the row moved or changed since"
            );
        }
        Ok(())
    }

    /// Delete a stale row (e.g. a file overwritten by a rename onto its path).
    fn evict(&mut self, file_id: Id) -> Result<()> {
        self.t.file(self.conn, file_id)?;
        // Cascades to asset and asset_item.
        self.conn
            .prepare_cached("DELETE FROM file WHERE id = ?1")?
            .execute([file_id])?;
        Ok(())
    }

    /// A move inside this root, or (with a guard in `expect`) from another
    /// root of the same volume: the same inode at a new path. The file keeps
    /// its id and asset; a move from another root counts as re-attached.
    fn apply_move(&mut self, m: &FileMove) -> Result<()> {
        type Row = (Option<Id>, String, i64, i64, Id);
        let row: Option<Row> = self
            .conn
            .prepare_cached("SELECT asset_id, rel_path, size, mtime_ns, root_id FROM file WHERE id = ?1")?
            .query_row(params![m.file_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .optional()?;
        let guard = self.expect.get(&m.file_id);
        let Some((asset_id, rel_path, size, mtime_ns, root_id)) = row else {
            tracing::warn!(file_id = m.file_id, to = %m.to.rel_path, "move of unknown file ignored");
            return Ok(());
        };
        // From another root (same inode: the same file), possibly already a
        // tombstone there: only with the scanner's guard.
        let cross_root = root_id != self.root_id;
        if cross_root && guard.is_none() {
            tracing::warn!(file_id = m.file_id, to = %m.to.rel_path, "unguarded move from another root ignored");
            return Ok(());
        }
        if let Some(e) = guard {
            if e.rel_path != rel_path || clamp_i64(e.size) != size || e.mtime_ns != mtime_ns {
                tracing::debug!(
                    file_id = m.file_id,
                    to = %m.to.rel_path,
                    "move decided from a stale snapshot skipped; the row moved or changed since"
                );
                return Ok(());
            }
        }
        let occupant: Option<Id> = self
            .conn
            .prepare_cached("SELECT id FROM file WHERE root_id = ?1 AND rel_path = ?2 AND id <> ?3")?
            .query_row(params![self.root_id, m.to.rel_path, m.file_id], |r| r.get(0))
            .optional()?;
        if let Some(other) = occupant {
            self.evict(other)?;
        }
        // Its items (in the old root's library too) lose the file: recompute.
        self.t.file(self.conn, m.file_id)?;
        self.update_file(m.file_id, &m.to, ProbeSrc::Keep, None, m.guess.as_ref())?;
        if cross_root {
            self.out.files_reattached += 1;
        } else {
            self.out.files_moved += 1;
        }
        self.t.file(self.conn, m.file_id)?;
        if let Some(g) = &m.guess {
            self.place(m.file_id, &m.to.rel_path, asset_id, g, ProbeSrc::Keep)?;
        }
        Ok(())
    }

    /// Rewrite a file row's location, stat, probe and guess. `probe_error`
    /// is `Some` only for a failed probe (`ProbeSrc::New(None)`).
    ///
    /// A failed probe keeps the last good probe JSON (and so the asset's
    /// playback facts) but clears `probe_version` and schedules a retry
    /// (`probe_attempts`, `probe_retry_at`); a successful one resets both.
    /// A stat change without a fresh fingerprint clears `fp128`/`oshash`,
    /// which described the old content (the scanner fingerprints it later).
    fn update_file(
        &self,
        id: Id,
        f: &FileFacts,
        probe: ProbeSrc<'_>,
        probe_error: Option<&str>,
        guess: Option<&NameGuess>,
    ) -> Result<()> {
        // 0 = keep the stored probe, 1 = a fresh probe, 2 = the probe failed.
        let (set_probe, probe_json, probe_version) = match probe {
            ProbeSrc::Keep => (0, None, None),
            ProbeSrc::New(Some(p)) => (1, Some(compact_json(p)?), Some(p.probe_version)),
            ProbeSrc::New(None) => (2, None, None),
        };
        let guess_json = guess.map(compact_json).transpose()?;
        self.conn
            .prepare_cached(
                "UPDATE file SET root_id = ?2, rel_path = ?3, dev = ?4, ino = ?5, btime_ns = ?6, size = ?7, mtime_ns = ?8,
                    fp128 = CASE WHEN ?9 IS NOT NULL THEN ?9
                                 WHEN size <> ?7 OR mtime_ns <> ?8 THEN NULL ELSE fp128 END,
                    oshash = CASE WHEN ?10 IS NOT NULL THEN ?10
                                  WHEN size <> ?7 OR mtime_ns <> ?8 THEN NULL ELSE oshash END,
                    probe_version = CASE ?11 WHEN 0 THEN probe_version ELSE ?12 END,
                    probe_error   = CASE ?11 WHEN 0 THEN probe_error ELSE ?13 END,
                    probe_attempts = CASE ?11 WHEN 0 THEN probe_attempts WHEN 1 THEN 0 ELSE probe_attempts + 1 END,
                    probe_retry_at = CASE ?11 WHEN 0 THEN probe_retry_at WHEN 1 THEN NULL
                                     ELSE ?16 + MIN(60000 * (1 << MIN(probe_attempts, 20)), 86400000) END,
                    parse_version = CASE WHEN ?14 THEN ?15 ELSE parse_version END,
                    state = CASE WHEN (CASE ?11 WHEN 0 THEN probe_error ELSE ?13 END) IS NULL
                                 THEN 'present' ELSE 'probe_failed' END,
                    tombstone_until = NULL, updated_at = ?16
                 WHERE id = ?1",
            )?
            .execute(params![
                id,
                self.root_id,
                f.rel_path,
                f.dev.map(u64_to_sql),
                f.ino.map(u64_to_sql),
                f.btime_ns,
                clamp_i64(f.size),
                f.mtime_ns,
                f.fp128.map(fp128_to_blob).as_ref().map(|b| b.as_slice()),
                f.oshash.map(u64_to_sql),
                set_probe,
                probe_version,
                probe_error,
                guess_json.is_some(),
                PARSER_VERSION,
                self.r.now,
            ])?;
        // A cached playback index of other content is useless now.
        self.conn
            .prepare_cached(
                "DELETE FROM asset_index WHERE asset_id = (SELECT asset_id FROM file WHERE id = ?1)
                   AND (source_size <> ?2 OR source_mtime_ns <> ?3)",
            )?
            .execute(params![id, clamp_i64(f.size), f.mtime_ns])?;
        if set_probe == 1 || guess_json.is_some() {
            // A failed probe (2) keeps the last good probe JSON.
            self.conn
                .prepare_cached(
                    "INSERT INTO file_meta(file_id, probe, guess) VALUES (?1, ?2, ?3)
                     ON CONFLICT(file_id) DO UPDATE SET
                        probe = CASE ?4 WHEN 1 THEN excluded.probe ELSE probe END,
                        guess = COALESCE(excluded.guess, guess)",
                )?
                .execute(params![id, probe_json, guess_json, set_probe])?;
        }
        Ok(())
    }

    fn insert_file(
        &self,
        f: &FileFacts,
        probe: ProbeSrc<'_>,
        probe_error: Option<&str>,
        guess: Option<&NameGuess>,
    ) -> Result<Id> {
        let (probe_json, probe_version) = match probe {
            ProbeSrc::New(Some(p)) => (Some(compact_json(p)?), Some(p.probe_version)),
            _ => (None, None),
        };
        let guess_json = guess.map(compact_json).transpose()?;
        let failed = probe_error.is_some();
        self.conn
            .prepare_cached(
                "INSERT INTO file(root_id, rel_path, dev, ino, btime_ns, size, mtime_ns, fp128, oshash, state,
                                  probe_version, probe_error, parse_version, first_seen_at, updated_at,
                                  probe_attempts, probe_retry_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14, ?15, ?16)",
            )?
            .execute(params![
                self.root_id,
                f.rel_path,
                f.dev.map(u64_to_sql),
                f.ino.map(u64_to_sql),
                f.btime_ns,
                clamp_i64(f.size),
                f.mtime_ns,
                f.fp128.map(fp128_to_blob).as_ref().map(|b| b.as_slice()),
                f.oshash.map(u64_to_sql),
                if failed { "probe_failed" } else { "present" },
                probe_version,
                probe_error,
                guess.map(|_| PARSER_VERSION),
                self.r.now,
                i64::from(failed),
                failed.then(|| self.r.now + PROBE_RETRY_BASE_MS),
            ])?;
        let id = self.conn.last_insert_rowid();
        if probe_json.is_some() || guess_json.is_some() {
            self.conn
                .prepare_cached("INSERT INTO file_meta(file_id, probe, guess) VALUES (?1, ?2, ?3)")?
                .execute(params![id, probe_json, guess_json])?;
        }
        Ok(id)
    }

    /// A tombstoned file with the same content: a copy/move across roots or
    /// volumes, or a restore. Re-attaching keeps its asset and watch state.
    ///
    /// Byte-identical files are common enough (a library of clones, the
    /// same trailer in many folders) that "any tombstone with this content"
    /// would swap identities between them: prefer the tombstone with the
    /// same file name, then one in a folder of the same name, then the most
    /// recent one (the rule flux-scan uses for moves inside a root).
    fn reattach_candidate(
        &self,
        fp: u128,
        size: u64,
        new_rel: &str,
        exclude: Option<Id>,
    ) -> Result<Option<(Id, Option<Id>, String)>> {
        type Cand = (Id, Option<Id>, String, String);
        let cands: Vec<Cand> = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT id, asset_id, placement, rel_path FROM file INDEXED BY file_fp_missing
                  WHERE fp128 = ?1 AND size = ?2 AND state = 'missing' AND id IS NOT ?3
                  ORDER BY tombstone_until DESC, id DESC LIMIT 256",
            )?;
            let rows = stmt.query_map(
                params![fp128_to_blob(fp).as_slice(), clamp_i64(size), exclude],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let base = |r: &str| r.rsplit('/').next().unwrap_or("").to_string();
        let folder = |r: &str| {
            r.rsplit_once('/')
                .map(|(p, _)| p.rsplit('/').next().unwrap_or("").to_string())
                .unwrap_or_default()
        };
        let (b, d) = (base(new_rel), folder(new_rel));
        let pick = cands
            .iter()
            .position(|c| base(&c.3) == b)
            .or_else(|| cands.iter().position(|c| !d.is_empty() && folder(&c.3) == d))
            .or(if cands.is_empty() { None } else { Some(0) });
        Ok(pick.map(|i| {
            let (id, asset, placement, _) = cands[i].clone();
            (id, asset, placement)
        }))
    }

    fn apply_upsert(&mut self, u: &FileUpsert) -> Result<()> {
        let f = &u.facts;
        let (probe, probe_error) = match &u.probe {
            ProbeUpdate::Keep => (ProbeSrc::Keep, None),
            ProbeUpdate::Probed(p) => (ProbeSrc::New(Some(p.as_ref())), None),
            ProbeUpdate::Failed(e) => (ProbeSrc::New(None), Some(e.as_str())),
        };
        let existing: Option<(Id, Option<Id>, String)> = self
            .conn
            .prepare_cached("SELECT id, asset_id, placement FROM file WHERE root_id = ?1 AND rel_path = ?2")?
            .query_row(params![self.root_id, f.rel_path], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?;
        let (file_id, asset_id, placement) = match existing {
            Some((id, asset, placement)) => {
                self.update_file(id, f, probe, probe_error, u.guess.as_ref())?;
                self.out.files_changed += 1;
                self.t.file(self.conn, id)?;
                (id, asset, placement)
            }
            None => match f
                .fp128
                .map(|fp| self.reattach_candidate(fp, f.size, &f.rel_path, None))
                .transpose()?
                .flatten()
            {
                Some((id, asset, placement)) => {
                    self.update_file(id, f, probe, probe_error, u.guess.as_ref())?;
                    self.out.files_reattached += 1;
                    self.t.file(self.conn, id)?;
                    (id, asset, placement)
                }
                None => {
                    let id = self.insert_file(f, probe, probe_error, u.guess.as_ref())?;
                    self.out.files_added += 1;
                    (id, None, "pending".to_string())
                }
            },
        };

        match (&u.guess, asset_id) {
            (Some(g), _) => self.place(file_id, &f.rel_path, asset_id, g, probe),
            // A new file the parser said nothing about: unmatched item.
            (None, None) if placement == "pending" => {
                let stored = self.load_guess(file_id)?;
                let g = stored.unwrap_or_else(NameGuess::unknown);
                self.place(file_id, &f.rel_path, None, &g, probe)
            }
            (None, Some(a)) => match probe {
                // Re-probed in place: refresh the asset's playback facts.
                // A failed re-probe keeps them (the file is retried later).
                ProbeSrc::New(Some(p)) => {
                    let guess = self.load_guess(file_id)?;
                    self.update_asset_facts(a, &asset_facts(Some(p), guess.as_ref()))?;
                    let mut stmt = self
                        .conn
                        .prepare_cached("SELECT item_id FROM asset_item WHERE asset_id = ?1")?;
                    for id in stmt.query_map([a], |r| r.get::<_, Id>(0))? {
                        let id = id?;
                        self.t.leaves.insert(id);
                        self.t.linked.insert(id);
                    }
                    Ok(())
                }
                ProbeSrc::New(None) | ProbeSrc::Keep => Ok(()),
            },
            (None, None) => Ok(()), // ignored file: stat refresh only
        }
    }

    fn load_guess(&self, file_id: Id) -> Result<Option<NameGuess>> {
        let json: Option<String> = self
            .conn
            .prepare_cached("SELECT guess FROM file_meta WHERE file_id = ?1")?
            .query_row([file_id], |r| r.get(0))
            .optional()?
            .flatten();
        Ok(json.and_then(|j| parse_stored(file_id, "guess", &j)))
    }

    fn load_probe(&self, file_id: Id) -> Result<Option<ProbeResult>> {
        let json: Option<String> = self
            .conn
            .prepare_cached("SELECT probe FROM file_meta WHERE file_id = ?1")?
            .query_row([file_id], |r| r.get(0))
            .optional()?
            .flatten();
        Ok(json.and_then(|j| parse_stored(file_id, "probe", &j)))
    }

    fn place(
        &mut self,
        file_id: Id,
        rel_path: &str,
        current: Option<Id>,
        guess: &NameGuess,
        probe: ProbeSrc<'_>,
    ) -> Result<()> {
        let conn = self.conn;
        match group::place(guess, rel_path) {
            Placement::Ignore { .. } => {
                if let Some(a) = current {
                    self.t.file(conn, file_id)?;
                    conn.prepare_cached("DELETE FROM asset WHERE id = ?1")?
                        .execute([a])?;
                }
                conn.prepare_cached("UPDATE file SET placement = 'ignored', asset_id = NULL WHERE id = ?1")?
                    .execute([file_id])?;
                self.out.files_ignored += 1;
            }
            Placement::Movie { work, edition, part } => {
                if let Some((film, x)) = self.word_extra(guess, rel_path)? {
                    let item = group::resolve_extra(conn, &mut self.r, film, &x)?;
                    self.link(file_id, current, &[item], None, None, guess, probe)?;
                    return Ok(());
                }
                let item = group::resolve_work(conn, &mut self.r, "movie", &work, None)?;
                self.placed_parent = true;
                if let Some((dir, _)) = rel_path.rsplit_once('/') {
                    self.movie_dirs.insert(dir.to_string());
                }
                self.link(file_id, current, &[item], edition.as_deref(), part, guess, probe)?;
            }
            Placement::Episodes {
                show,
                season,
                episodes,
                episode_title,
                absolute,
            } => {
                let prefer = match current {
                    Some(a) => show_of_asset(conn, a)?,
                    None => None,
                };
                let show_id = group::resolve_work(conn, &mut self.r, "show", &show, prefer)?;
                // Anime absolute numbering the show's map already places.
                let (season, episodes) = match absolute {
                    Some(abs) => match crate::rekey::mapped_episode(conn, show_id, abs)? {
                        Some((s, eps)) => (s, eps.into_iter().map(group::EpisodeKey::Number).collect()),
                        None => (season, episodes),
                    },
                    None => (season, episodes),
                };
                let season_id = group::resolve_season(conn, &mut self.r, show_id, season)?;
                let title = if episodes.len() == 1 {
                    episode_title.as_deref()
                } else {
                    None
                };
                let mut items = Vec::with_capacity(episodes.len());
                for ep in &episodes {
                    items.push(group::resolve_episode(
                        conn,
                        &mut self.r,
                        show_id,
                        season_id,
                        season,
                        ep,
                        title,
                    )?);
                }
                self.placed_parent = true;
                self.link(file_id, current, &items, None, None, guess, probe)?;
            }
            Placement::Extra(x) => match group::find_extra_parent(conn, self.r.library_id, self.root_id, &x)?
            {
                Some(parent) => {
                    let item = group::resolve_extra(conn, &mut self.r, parent, &x)?;
                    self.link(file_id, current, &[item], None, None, guess, probe)?;
                }
                None if current.is_none() => {
                    conn.prepare_cached("UPDATE file SET placement = 'pending' WHERE id = ?1")?
                        .execute([file_id])?;
                    self.out.files_pending += 1;
                    self.pending_now.insert(file_id);
                }
                // Keep an existing extra where it is rather than orphan it.
                None => {}
            },
        }
        Ok(())
    }

    fn update_asset_facts(&self, asset_id: Id, f: &AssetFacts) -> Result<()> {
        self.conn
            .prepare_cached(
                "UPDATE asset SET container = ?2, duration_ms = ?3, video_codec = ?4, width = ?5, height = ?6,
                                  hdr = ?7, audio_summary = ?8, label = ?9, badges = ?10, updated_at = ?11
                 WHERE id = ?1",
            )?
            .execute(params![
                asset_id,
                f.container.as_ref().and_then(enum_to_text),
                f.duration_ms.map(clamp_i64),
                f.video_codec.as_ref().and_then(enum_to_text),
                f.width,
                f.height,
                f.hdr.as_ref().and_then(enum_to_text),
                f.audio_summary,
                f.label,
                badges_json(&f.badges)?,
                self.r.now,
            ])?;
        Ok(())
    }

    /// Create or update the file's asset and link it to `items` (the first is
    /// the primary item).
    #[allow(clippy::too_many_arguments)]
    fn link(
        &mut self,
        file_id: Id,
        current: Option<Id>,
        items: &[Id],
        edition: Option<&str>,
        part: Option<u8>,
        guess: &NameGuess,
        probe: ProbeSrc<'_>,
    ) -> Result<()> {
        let Some(&primary) = items.first() else {
            return Ok(());
        };
        let conn = self.conn;
        let facts = match (probe, current) {
            (ProbeSrc::New(Some(p)), _) => Some(asset_facts(Some(p), Some(guess))),
            // A failed re-probe keeps the asset's facts (the stored probe is
            // kept too; the file is retried with backoff).
            (ProbeSrc::New(None), Some(_)) | (ProbeSrc::Keep, Some(_)) => None,
            (ProbeSrc::New(None), None) | (ProbeSrc::Keep, None) => {
                Some(asset_facts(self.load_probe(file_id)?.as_ref(), Some(guess)))
            }
        };
        let (asset_id, old) = match current {
            None => {
                let f = facts.clone().unwrap_or_default();
                conn.prepare_cached(
                    "INSERT INTO asset(item_id, file_id, container, duration_ms, video_codec, width, height, hdr,
                                       audio_summary, edition, part, label, badges, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14)",
                )?
                .execute(params![
                    primary,
                    file_id,
                    f.container.as_ref().and_then(enum_to_text),
                    f.duration_ms.map(clamp_i64),
                    f.video_codec.as_ref().and_then(enum_to_text),
                    f.width,
                    f.height,
                    f.hdr.as_ref().and_then(enum_to_text),
                    f.audio_summary,
                    edition,
                    part,
                    f.label,
                    badges_json(&f.badges)?,
                    self.r.now,
                ])?;
                let id = conn.last_insert_rowid();
                conn.prepare_cached("UPDATE file SET asset_id = ?2, placement = 'item' WHERE id = ?1")?
                    .execute(params![file_id, id])?;
                (id, Vec::new())
            }
            Some(a) => {
                if let Some(f) = &facts {
                    self.update_asset_facts(a, f)?;
                }
                conn.prepare_cached(
                    "UPDATE asset SET item_id = ?2, edition = ?3, part = ?4, updated_at = ?5
                     WHERE id = ?1 AND (item_id IS NOT ?2 OR edition IS NOT ?3 OR part IS NOT ?4)",
                )?
                .execute(params![a, primary, edition, part, self.r.now])?;
                let mut stmt = conn
                    .prepare_cached("SELECT item_id FROM asset_item WHERE asset_id = ?1 ORDER BY item_id")?;
                let old = stmt
                    .query_map([a], |r| r.get::<_, Id>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                (a, old)
            }
        };
        let mut new: Vec<Id> = items.to_vec();
        new.sort_unstable();
        new.dedup();
        let mut left: Vec<Id> = Vec::new();
        for id in old.iter().filter(|id| !new.contains(id)) {
            conn.prepare_cached("DELETE FROM asset_item WHERE item_id = ?1 AND asset_id = ?2")?
                .execute(params![id, asset_id])?;
            self.t.leaves.insert(*id);
            self.t.linked.insert(*id);
            left.push(*id);
        }
        for id in new.iter().filter(|id| !old.contains(id)) {
            conn.prepare_cached("INSERT INTO asset_item(item_id, asset_id) VALUES (?1, ?2)")?
                .execute(params![id, asset_id])?;
            self.t.linked.insert(*id);
        }
        for &id in &new {
            if facts.is_some() {
                self.t.linked.insert(id);
            }
            self.t.leaves.insert(id);
        }
        // A rename or re-parse took the last file of a work elsewhere: the
        // user's state, art pin and match belong to the work, not the path.
        for from in left {
            if item_is_empty(conn, from)? {
                let to = carry_target(conn, from, items)?;
                self.carry(from, to)?;
            }
        }
        Ok(())
    }

    /// Carry an emptied work into `to` (see [`crate::merge::carry_emptied`])
    /// and fold what that touched and deleted into this job.
    fn carry(&mut self, from: Id, to: Id) -> Result<()> {
        if self.deleted.contains(&from) || self.deleted.contains(&to) {
            return Ok(());
        }
        let c = crate::merge::carry_emptied(self.conn, from, to, self.r.now)?;
        self.t.leaves.extend(c.touched.leaves);
        self.t.linked.extend(c.touched.linked);
        self.t.parents.extend(c.touched.parents);
        if !c.deleted.is_empty() {
            self.r.forget(&c.deleted);
            self.deleted.extend(c.deleted);
        }
        Ok(())
    }

    /// A film-like file in an extras-named subfolder of a folder that holds
    /// exactly one film directly (`Film/1080p/Bonus Clips)/Song.mkv`
    /// beside `Film/1080p/Film.mkv`): that film and the extra to place it
    /// under. `None` keeps the file a film of its own.
    fn word_extra(&self, guess: &NameGuess, rel_path: &str) -> Result<Option<(Id, group::ExtraTarget)>> {
        if !word_extra_candidate(guess, rel_path) {
            return Ok(None);
        }
        let Some((kind, dir)) = group::extras_folder_by_word(rel_path) else {
            return Ok(None);
        };
        let Some(film) = group::sole_movie_in_dir(self.conn, self.root_id, dir)? else {
            return Ok(None);
        };
        Ok(Some((
            film,
            group::ExtraTarget {
                title: group::clean_filename(rel_path),
                kind,
                parent_dir: Some(dir.to_string()),
                parent_title: None,
                parent_year: None,
            },
        )))
    }

    /// Files an earlier batch placed as films because the film beside their
    /// extras-named folder wasn't cataloged yet (scan batches follow probe
    /// order, not folder order): for each folder this batch placed a film
    /// in, move them under that film now.
    fn regroup_word_extras(&mut self) -> Result<()> {
        for dir in std::mem::take(&mut self.movie_dirs) {
            if group::sole_movie_in_dir(self.conn, self.root_id, &dir)?.is_none() {
                continue;
            }
            let lo = format!("{dir}/");
            let hi = format!("{dir}0");
            // Paths first (index only); the few in an extras-named
            // subfolder are then checked against their item.
            let paths: Vec<(Id, String)> = {
                let mut stmt = self.conn.prepare_cached(
                    "SELECT id, rel_path FROM file WHERE root_id = ?1 AND rel_path >= ?2 AND rel_path < ?3",
                )?;
                let rows = stmt.query_map(params![self.root_id, lo, hi], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            let in_extras_folder =
                |rel: &str| group::extras_folder_by_word(rel).is_some_and(|(_, d)| d == dir.as_str());
            for (id, rel_path) in paths.into_iter().filter(|(_, rel)| in_extras_folder(rel)) {
                let row: Option<(Id, Option<String>)> = self
                    .conn
                    .prepare_cached(
                        "SELECT a.id, m.guess FROM file f
                           JOIN asset a ON a.file_id = f.id
                           JOIN item i ON i.id = a.item_id
                           LEFT JOIN file_meta m ON m.file_id = f.id
                          WHERE f.id = ?1 AND f.state <> 'missing'
                            AND i.kind = 'movie' AND i.parent_id IS NULL",
                    )?
                    .query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))
                    .optional()?;
                let Some((asset, Some(guess))) = row else { continue };
                let Some(guess) = parse_stored::<NameGuess>(id, "guess", &guess) else {
                    continue;
                };
                if word_extra_candidate(&guess, &rel_path) {
                    self.place(id, &rel_path, Some(asset), &guess, ProbeSrc::Keep)?;
                }
            }
        }
        Ok(())
    }

    /// Extras whose parent was not known when they were scanned.
    fn retry_pending(&mut self) -> Result<()> {
        let rows: Vec<(Id, String, Option<String>)> = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT f.id, f.rel_path, m.guess FROM file f LEFT JOIN file_meta m ON m.file_id = f.id
                 WHERE f.root_id = ?1 AND f.placement = 'pending' AND f.state <> 'missing'",
            )?;
            let rows = stmt.query_map([self.root_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for (id, rel_path, guess) in rows {
            if self.pending_now.contains(&id) {
                continue;
            }
            let Some(guess) = guess.and_then(|g| parse_stored::<NameGuess>(id, "guess", &g)) else {
                continue;
            };
            if let Placement::Extra(x) = group::place(&guess, &rel_path) {
                if let Some(parent) =
                    group::find_extra_parent(self.conn, self.r.library_id, self.root_id, &x)?
                {
                    let item = group::resolve_extra(self.conn, &mut self.r, parent, &x)?;
                    self.link(id, None, &[item], None, None, &guess, ProbeSrc::Keep)?;
                }
            }
        }
        Ok(())
    }
}

impl Apply<'_> {
    /// One deferred fingerprint (see [`attach_fingerprints`]).
    fn attach(&mut self, f: &FileFingerprint) -> Result<()> {
        let conn = self.conn;
        type Row = (Id, Option<Id>, String, Option<Vec<u8>>, Option<i64>);
        let row: Option<Row> = conn
            .prepare_cached(
                "SELECT id, asset_id, state, fp128, oshash FROM file
                  WHERE root_id = ?1 AND rel_path = ?2 AND size = ?3 AND mtime_ns = ?4",
            )?
            .query_row(
                params![self.root_id, f.rel_path, clamp_i64(f.size), f.mtime_ns],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        // Gone or changed since it was fingerprinted: a later scan does it again.
        let Some((id, asset, state, old_fp, old_os)) = row else {
            return Ok(());
        };
        if state == FileState::Missing.as_str() {
            return Ok(());
        }
        match self.reattach_candidate(f.fp128, f.size, &f.rel_path, Some(id))? {
            Some((tomb, tomb_asset, _)) => self.merge_into_tombstone(id, asset, tomb, tomb_asset, f),
            None => {
                let blob = fp128_to_blob(f.fp128);
                let os = f.oshash.map(u64_to_sql);
                if old_fp.as_deref() != Some(blob.as_slice()) || old_os != os {
                    conn.prepare_cached("UPDATE file SET fp128 = ?2, oshash = ?3 WHERE id = ?1")?
                        .execute(params![id, blob.as_slice(), os])?;
                }
                Ok(())
            }
        }
    }

    /// The fresh row `fresh` (cataloged before its fingerprint was known)
    /// holds the content of tombstone `tomb`: a move or copy across roots or
    /// volumes, found late. The tombstone takes the fresh row's place (its
    /// id, asset and so every user's state survive, as when the fingerprint
    /// is known up front) and the fresh row goes; a work the fresh file had
    /// created is carried into wherever the tombstone's asset lands.
    fn merge_into_tombstone(
        &mut self,
        fresh: Id,
        fresh_asset: Option<Id>,
        tomb: Id,
        tomb_asset: Option<Id>,
        f: &FileFingerprint,
    ) -> Result<()> {
        let conn = self.conn;
        type Fresh = (
            String,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<String>,
            Option<i64>,
            i64,
            Option<i64>,
        );
        let (rel_path, dev, ino, btime, probe_version, probe_error, parse_version, attempts, retry_at): Fresh =
            conn.prepare_cached(
                "SELECT rel_path, dev, ino, btime_ns, probe_version, probe_error, parse_version,
                        probe_attempts, probe_retry_at FROM file WHERE id = ?1",
            )?
            .query_row([fresh], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            })?;
        let (probe_json, guess_json): (Option<String>, Option<String>) = conn
            .prepare_cached("SELECT probe, guess FROM file_meta WHERE file_id = ?1")?
            .query_row([fresh], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?
            .unwrap_or_default();
        let fresh_items: Vec<Id> = match fresh_asset {
            Some(a) => {
                let mut stmt = conn.prepare_cached("SELECT item_id FROM asset_item WHERE asset_id = ?1")?;
                let rows = stmt.query_map([a], |r| r.get(0))?;
                rows.collect::<rusqlite::Result<_>>()?
            }
            None => Vec::new(),
        };
        self.t.file(conn, fresh)?;
        // Cascades to its asset, links, file_meta and index cache.
        conn.prepare_cached("DELETE FROM file WHERE id = ?1")?
            .execute([fresh])?;
        conn.prepare_cached(
            "UPDATE file SET root_id = ?2, rel_path = ?3, dev = ?4, ino = ?5, btime_ns = ?6, size = ?7, mtime_ns = ?8,
                fp128 = ?9, oshash = ?10, probe_version = COALESCE(?11, probe_version), probe_error = ?12,
                parse_version = COALESCE(?13, parse_version), probe_attempts = ?14, probe_retry_at = ?15,
                state = CASE WHEN ?12 IS NULL THEN 'present' ELSE 'probe_failed' END,
                tombstone_until = NULL, updated_at = ?16
             WHERE id = ?1",
        )?
        .execute(params![
            tomb,
            self.root_id,
            rel_path,
            dev,
            ino,
            btime,
            clamp_i64(f.size),
            f.mtime_ns,
            fp128_to_blob(f.fp128).as_slice(),
            f.oshash.map(u64_to_sql),
            probe_version,
            probe_error,
            parse_version,
            attempts,
            retry_at,
            self.r.now,
        ])?;
        conn.prepare_cached(
            "INSERT INTO file_meta(file_id, probe, guess) VALUES (?1, ?2, ?3)
             ON CONFLICT(file_id) DO UPDATE SET probe = COALESCE(excluded.probe, probe),
                                                guess = COALESCE(excluded.guess, guess)",
        )?
        .execute(params![tomb, probe_json, guess_json])?;
        conn.prepare_cached(
            "DELETE FROM asset_index WHERE asset_id = ?1 AND (source_size <> ?2 OR source_mtime_ns <> ?3)",
        )?
        .execute(params![tomb_asset, clamp_i64(f.size), f.mtime_ns])?;
        self.out.files_reattached += 1;
        self.t.file(conn, tomb)?;
        // Re-place the tombstone's asset by what the fresh row was parsed as.
        let probe: Option<ProbeResult> = probe_json.and_then(|j| parse_stored(tomb, "probe", &j));
        let guess = match guess_json.and_then(|j| parse_stored::<NameGuess>(tomb, "guess", &j)) {
            Some(g) => Some(g),
            None => self.load_guess(tomb)?,
        };
        let src = match &probe {
            Some(p) => ProbeSrc::New(Some(p)),
            None => ProbeSrc::Keep,
        };
        if let Some(g) = &guess {
            self.place(tomb, &rel_path, tomb_asset, g, src)?;
        }
        // The fresh file's work lost its only file: it carries into the
        // work the tombstone's asset now belongs to.
        let target: Option<Id> = conn
            .prepare_cached("SELECT a.item_id FROM file f JOIN asset a ON a.id = f.asset_id WHERE f.id = ?1")?
            .query_row([tomb], |r| r.get(0))
            .optional()?;
        if let Some(to) = target {
            for from in fresh_items {
                if from != to && !self.deleted.contains(&from) && item_is_empty(conn, from)? {
                    self.carry(from, to)?;
                }
            }
        }
        Ok(())
    }
}

/// Attach fingerprints computed after the files were cataloged (flux-scan
/// fingerprints new files in a background pass so they are browsable
/// sooner). A file whose content matches a tombstone is merged into it,
/// exactly as an upsert carrying the fingerprint would have re-attached:
/// the tombstone's file id, asset and watch state survive.
pub(crate) fn attach_fingerprints(
    conn: &Connection,
    root_id: Id,
    fps: &[FileFingerprint],
    grace_ms: i64,
    job_max_attempts: u32,
) -> Result<ApplyOutcome> {
    let (mut ap, _) = Apply::new(conn, root_id, grace_ms, job_max_attempts)?;
    for f in fps {
        ap.attach(f)?;
    }
    if ap.placed_parent {
        ap.retry_pending()?;
    }
    ap.finish()
}

/// A film (or unrecognised) guess without provider ids, in an extras-named
/// folder ([`group::extras_folder_by_word`]): it may be a film's extra.
fn word_extra_candidate(guess: &NameGuess, rel_path: &str) -> bool {
    matches!(guess.kind, GuessKind::Movie | GuessKind::Unknown)
        && guess.ids.is_empty()
        && group::extras_folder_by_word(rel_path).is_some()
}

/// No asset links to the item and no asset names it as primary.
pub(crate) fn item_is_empty(conn: &Connection, id: Id) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "SELECT 1 FROM item i WHERE i.id = ?1
               AND NOT EXISTS (SELECT 1 FROM asset_item WHERE item_id = i.id)
               AND NOT EXISTS (SELECT 1 FROM asset WHERE item_id = i.id)",
        )?
        .query_row([id], |_| Ok(()))
        .optional()?
        .is_some())
}

/// Which of a file's new items an emptied old item carries into: the
/// episode with the same number (a multi-episode file re-numbered or
/// re-titled), else the primary item.
fn carry_target(conn: &Connection, from: Id, items: &[Id]) -> Result<Id> {
    let primary = items[0];
    if items.len() == 1 {
        return Ok(primary);
    }
    let number = |id: Id| -> Result<Option<i64>> {
        Ok(conn
            .prepare_cached("SELECT episode_number FROM item WHERE id = ?1 AND kind = 'episode'")?
            .query_row([id], |r| r.get::<_, Option<i64>>(0))
            .optional()?
            .flatten())
    };
    if let Some(n) = number(from)? {
        for &i in items {
            if number(i)? == Some(n) {
                return Ok(i);
            }
        }
    }
    Ok(primary)
}

/// The show an asset's episodes belong to (episode → season → show).
pub(crate) fn show_of_asset(conn: &Connection, asset_id: Id) -> Result<Option<Id>> {
    Ok(conn
        .prepare_cached(
            "SELECT s.parent_id FROM asset_item ai JOIN item e ON e.id = ai.item_id JOIN item s ON s.id = e.parent_id
             WHERE ai.asset_id = ?1 AND e.kind = 'episode' AND s.kind = 'season' LIMIT 1",
        )?
        .query_row([asset_id], |r| r.get(0))
        .optional()?
        .flatten())
}

/// Parse a stored probe/guess. A row this build cannot read (corruption, or
/// written by a newer flux-core) is treated as absent rather than failing the
/// whole batch: the file is re-probed/re-parsed on its next change.
fn parse_stored<T: for<'de> serde::Deserialize<'de>>(file_id: Id, what: &str, json: &str) -> Option<T> {
    match serde_json::from_str(json) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(file_id, what, error = %e, "unreadable stored JSON ignored");
            None
        }
    }
}

fn badges_json(b: &[String]) -> Result<Option<String>> {
    Ok(if b.is_empty() {
        None
    } else {
        Some(serde_json::to_string(b)?)
    })
}

fn state_rank(s: &str) -> u8 {
    match s {
        "present" | "probe_failed" | "available" => 0,
        "unavailable" => 1,
        _ => 2,
    }
}

const RANK_STATE: [&str; 3] = ["available", "unavailable", "missing"];

/// Recompute derived `item.state` and `item.badges` for touched leaves, then
/// their seasons and shows. Writes only rows that actually change. Returns
/// `(updated, removed)` item ids (excluding `created`).
pub(crate) fn recompute(
    conn: &Connection,
    now: i64,
    t: &Touched,
    created: &HashMap<Id, (&'static str, Option<Id>)>,
) -> Result<(Vec<Id>, Vec<Id>)> {
    let mut updated: BTreeSet<Id> = BTreeSet::new();
    let mut removed: Vec<Id> = Vec::new();
    let mut seasons: BTreeSet<Id> = BTreeSet::new();
    for &p in &t.parents {
        // Only containers derive from children (a movie whose extra went
        // away must not become "missing").
        let kind: Option<String> = conn
            .prepare_cached("SELECT kind FROM item WHERE id = ?1")?
            .query_row([p], |r| r.get(0))
            .optional()?;
        if matches!(kind.as_deref(), Some("season" | "show")) {
            seasons.insert(p);
        }
    }

    for &id in &t.leaves {
        let head: Option<(String, Option<Id>)> = match created.get(&id) {
            Some(&(kind, parent)) => Some((kind.to_string(), parent)),
            None => conn
                .prepare_cached("SELECT kind, parent_id FROM item WHERE id = ?1")?
                .query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?,
        };
        let Some((kind, parent)) = head else { continue };
        let mut rank = 2u8;
        let mut lists: Vec<Vec<String>> = Vec::new();
        {
            let mut stmt = conn.prepare_cached(
                "SELECT f.state, a.badges FROM asset_item ai JOIN asset a ON a.id = ai.asset_id
                   JOIN file f ON f.id = a.file_id WHERE ai.item_id = ?1",
            )?;
            let mut rows = stmt.query([id])?;
            while let Some(r) = rows.next()? {
                let st: String = r.get(0)?;
                let rk = state_rank(&st);
                rank = rank.min(rk);
                if rk < 2 {
                    if let Some(b) = r.get::<_, Option<String>>(1)? {
                        if let Ok(v) = serde_json::from_str::<Vec<String>>(&b) {
                            lists.push(v);
                        }
                    }
                }
            }
        }
        let state = RANK_STATE[rank as usize];
        let badges = badges_json(&merge_badges(lists.iter().map(Vec::as_slice)))?;
        let n = conn
            .prepare_cached(
                "UPDATE item SET state = ?2, badges = ?3, updated_at = ?4
                 WHERE id = ?1 AND (state IS NOT ?2 OR badges IS NOT ?3)",
            )?
            .execute(params![id, state, badges, now])?;
        if !created.contains_key(&id) {
            if n > 0 && state == "missing" {
                removed.push(id);
            } else if n > 0 || t.linked.contains(&id) {
                updated.insert(id);
            }
        }
        if kind == "episode" {
            if let Some(p) = parent {
                seasons.insert(p);
            }
        }
    }

    // Seasons, then shows: a parent is as available as its best child.
    let mut shows: BTreeSet<Id> = BTreeSet::new();
    for level in 0..2 {
        let ids: Vec<Id> = if level == 0 {
            seasons.iter().copied().collect()
        } else {
            shows.iter().copied().collect()
        };
        for id in ids {
            // As available as the best child; badges merged from the
            // children that aren't hidden (a show is "4K" if an episode is).
            // `episode_count`: visible episodes below (directly, or the
            // seasons' counts), so lists never count episodes per row.
            let mut rank = 2u8;
            let mut lists: Vec<Vec<String>> = Vec::new();
            let mut episodes: i64 = 0;
            {
                let mut stmt = conn.prepare_cached(
                    "SELECT state, badges, kind, episode_count FROM item WHERE parent_id = ?1 AND kind <> 'extra'",
                )?;
                let mut rows = stmt.query([id])?;
                while let Some(r) = rows.next()? {
                    let rk = state_rank(&r.get::<_, String>(0)?);
                    rank = rank.min(rk);
                    match r.get_ref(2)?.as_str().unwrap_or("") {
                        "episode" if rk < 2 => episodes += 1,
                        "season" => episodes += r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                        _ => {}
                    }
                    if rk < 2 {
                        if let Some(b) = r.get::<_, Option<String>>(1)? {
                            if let Ok(v) = serde_json::from_str::<Vec<String>>(&b) {
                                lists.push(v);
                            }
                        }
                    }
                }
            }
            let state = RANK_STATE[rank as usize];
            let badges = badges_json(&merge_badges(lists.iter().map(Vec::as_slice)))?;
            let n = conn
                .prepare_cached(
                    "UPDATE item SET state = ?2, badges = ?3, updated_at = ?4,
                                     episode_count = ?5
                     WHERE id = ?1 AND (state IS NOT ?2 OR badges IS NOT ?3 OR episode_count IS NOT ?5)",
                )?
                .execute(params![id, state, badges, now, episodes])?;
            if n > 0 && !created.contains_key(&id) {
                if state == "missing" {
                    removed.push(id);
                } else {
                    updated.insert(id);
                }
            }
            if level == 0 {
                let show: Option<Option<Id>> = conn
                    .prepare_cached("SELECT parent_id FROM item WHERE id = ?1")?
                    .query_row([id], |r| r.get(0))
                    .optional()?;
                if let Some(Some(s)) = show {
                    shows.insert(s);
                }
            }
        }
    }
    Ok((updated.into_iter().collect(), removed))
}

fn set_availability_inner(
    conn: &Connection,
    root_id: Id,
    available: bool,
    now: i64,
    t: &mut Touched,
) -> Result<(bool, u64)> {
    let target = if available {
        RootState::Available
    } else {
        RootState::Unavailable
    };
    let changed = conn
        .prepare_cached("UPDATE root SET state = ?2, state_changed_at = ?3 WHERE id = ?1 AND state <> ?2")?
        .execute(params![root_id, target.as_str(), now])?
        > 0;
    let files = if available {
        conn.prepare_cached(
            "UPDATE file SET state = CASE WHEN probe_error IS NULL THEN 'present' ELSE 'probe_failed' END, updated_at = ?2
             WHERE root_id = ?1 AND state = 'unavailable'",
        )?
        .execute(params![root_id, now])?
    } else {
        // Missing files stay missing (their tombstone clock is paused by
        // purge_tombstones skipping unavailable roots).
        conn.prepare_cached(
            "UPDATE file SET state = 'unavailable', updated_at = ?2 WHERE root_id = ?1 AND state IN ('present','probe_failed')",
        )?
        .execute(params![root_id, now])?
    };
    if files > 0 {
        let mut stmt = conn.prepare_cached(
            "SELECT DISTINCT ai.item_id FROM file f JOIN asset a ON a.file_id = f.id
               JOIN asset_item ai ON ai.asset_id = a.id
              WHERE f.root_id = ?1 AND f.state <> 'missing'",
        )?;
        for id in stmt.query_map([root_id], |r| r.get::<_, Id>(0))? {
            t.leaves.insert(id?);
        }
    }
    Ok((changed, files as u64))
}

/// Mark a root reachable or not. Unreachable never deletes anything: files
/// become `unavailable`, items stay listed (with `available = false` assets).
pub(crate) fn set_root_availability(
    conn: &Connection,
    root_id: Id,
    available: bool,
) -> Result<AvailabilityChange> {
    let exists: Option<i64> = conn
        .prepare_cached("SELECT 1 FROM root WHERE id = ?1")?
        .query_row([root_id], |r| r.get(0))
        .optional()?;
    if exists.is_none() {
        return Err(CatalogError::NotFound(format!("root {root_id}")));
    }
    let now = now_ms();
    let mut t = Touched::default();
    let (changed, files) = set_availability_inner(conn, root_id, available, now, &mut t)?;
    let items = t.leaves.len() as u64;
    if !t.leaves.is_empty() {
        recompute(conn, now, &t, &HashMap::new())?;
    }
    Ok(AvailabilityChange {
        root_id,
        changed,
        files_affected: files,
        items_affected: items,
    })
}

/// Delete files whose tombstone expired (only on reachable roots), their
/// assets, and items left with no assets and no children.
pub(crate) fn purge_tombstones(conn: &Connection, now: i64) -> Result<PurgeOutcome> {
    let ids: Vec<Id> = {
        let mut stmt = conn.prepare_cached(
            "SELECT f.id FROM file f JOIN root r ON r.id = f.root_id
              WHERE f.state = 'missing' AND f.tombstone_until IS NOT NULL AND f.tombstone_until < ?1
                AND r.state = 'available'",
        )?;
        let rows = stmt.query_map([now], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut t = Touched::default();
    for &id in &ids {
        t.file(conn, id)?;
        conn.prepare_cached("DELETE FROM file WHERE id = ?1")?
            .execute([id])?;
    }
    let mut items_deleted = 0u64;
    // Leaves without assets and without children, then empty seasons, then
    // empty shows, then movies whose last extra just went.
    let mut candidates: BTreeSet<Id> = t.leaves.clone();
    let mut parents_seen: BTreeSet<Id> = BTreeSet::new();
    for _ in 0..4 {
        let mut parents = BTreeSet::new();
        for &id in &candidates {
            let row: Option<Option<Id>> = conn
                .prepare_cached(
                    "SELECT parent_id FROM item i WHERE id = ?1
                       AND NOT EXISTS (SELECT 1 FROM asset_item WHERE item_id = i.id)
                       AND NOT EXISTS (SELECT 1 FROM item c WHERE c.parent_id = i.id)",
                )?
                .query_row([id], |r| r.get(0))
                .optional()?;
            if let Some(parent) = row {
                if let Some(doc) = crate::fts::doc(conn, id)? {
                    crate::fts::remove(conn, id, &doc)?;
                }
                items_deleted += conn
                    .prepare_cached("DELETE FROM item WHERE id = ?1")?
                    .execute([id])? as u64;
                if let Some(p) = parent {
                    parents.insert(p);
                }
            }
        }
        if parents.is_empty() {
            break;
        }
        parents_seen.extend(parents.iter().copied());
        candidates = parents;
    }
    // Survivors (items that still have other assets) need their state/badges refreshed.
    t.leaves.retain(|id| {
        conn.query_row("SELECT 1 FROM item WHERE id = ?1", [id], |_| Ok(()))
            .optional()
            .ok()
            .flatten()
            .is_some()
    });
    // …and so do the seasons/shows that lost a child but survived.
    t.parents = parents_seen;
    recompute(conn, now, &t, &HashMap::new())?;
    Ok(PurgeOutcome {
        files_deleted: ids.len() as u64,
        items_deleted,
    })
}
