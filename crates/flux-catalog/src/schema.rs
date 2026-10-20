//! Schema, migrations (`PRAGMA user_version`) and connection pragmas.
//!
//! The domain model follows `initial-discussion.md` §7 and research/02 §4.2:
//!
//! | table | concept |
//! |---|---|
//! | `library`, `root` | what the user added; a root is one folder (location source) |
//! | `dir` | per-directory mtimes for the O(dirs) poll fallback (research/02 §3.5) |
//! | `file` | a *Location* `(root, rel_path)` plus the *FileObject* `(dev, ino, btime)` and *Content* `fp128` identities, state and versions (narrow: read on every rescan) |
//! | `file_meta` | the file's probe (`flux_core::ProbeResult`) and name guess (`NameGuess`) as JSON, 1:1 with `file` |
//! | `asset` | one playable encoding (4K, 1080p, an edition, a part); playback facts are denormalised here at scan time (research/03 §11) |
//! | `asset_item` | asset → item membership; a multi-episode file links to several episode items (the `asset_episode` join, generalised) |
//! | `item` | a *Work*: movie / show / season / episode / extra |
//! | `art`, `user`, `user_state`, `job` | artwork, tailnet users, per-user progress, background work queue |
//! | `item_fts` | FTS5 index over item titles, kept in sync by the writer in the same transaction (`fts.rs`) |
//!
//! Timestamps are Unix **milliseconds** (`i64`) everywhere. `dev`/`ino` are
//! stored as the bit pattern of the `u64` in an `i64` column (SQLite integers
//! are signed); see [`crate::model::u64_to_sql`].

use rusqlite::Connection;

use crate::error::{CatalogError, Result};

/// Schema version this build writes. Bump by appending to `MIGRATIONS`.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// Ordered migrations; index `i` upgrades `user_version` `i` → `i + 1`.
/// Never edit a released migration: append a new one.
const MIGRATIONS: &[&str] = &[V1, V2, V3, V4];

const V1: &str = r#"
CREATE TABLE library(
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL,
    kind        TEXT NOT NULL CHECK(kind IN ('movies','shows','mixed')),
    created_at  INTEGER NOT NULL
);

CREATE TABLE root(
    id                     INTEGER PRIMARY KEY,
    library_id             INTEGER NOT NULL REFERENCES library(id) ON DELETE CASCADE,
    path                   TEXT NOT NULL,
    volume_uuid            TEXT,
    -- change-source resume point (FSEvents event id / USN / walk frontier),
    -- committed in the same transaction as the files it covers.
    cursor                 BLOB,
    state                  TEXT NOT NULL DEFAULT 'available' CHECK(state IN ('available','unavailable')),
    state_changed_at       INTEGER,
    last_scan_started_at   INTEGER,
    last_scan_finished_at  INTEGER,
    last_scan_error        TEXT,
    created_at             INTEGER NOT NULL,
    UNIQUE(library_id, path)
);

CREATE TABLE dir(
    root_id   INTEGER NOT NULL REFERENCES root(id) ON DELETE CASCADE,
    rel_path  TEXT NOT NULL,
    mtime_ns  INTEGER NOT NULL,
    PRIMARY KEY(root_id, rel_path)
) WITHOUT ROWID;

CREATE TABLE file(
    id               INTEGER PRIMARY KEY,
    root_id          INTEGER NOT NULL REFERENCES root(id) ON DELETE CASCADE,
    rel_path         TEXT NOT NULL,
    dev              INTEGER,
    ino              INTEGER,
    btime_ns         INTEGER,
    size             INTEGER NOT NULL,
    mtime_ns         INTEGER NOT NULL,
    fp128            BLOB,
    oshash           INTEGER,
    state            TEXT NOT NULL DEFAULT 'present'
                     CHECK(state IN ('present','missing','unavailable','probe_failed')),
    -- set when state = 'missing': the row (and its asset) is purged after this.
    tombstone_until  INTEGER,
    probe_version    INTEGER,
    probe_error      TEXT,
    parse_version    INTEGER,
    -- where the grouping rules put this file: an item ('item'), nowhere
    -- because it is a sample ('ignored'), or not yet ('pending': an extra
    -- whose parent has not been scanned yet).
    placement        TEXT NOT NULL DEFAULT 'pending' CHECK(placement IN ('pending','item','ignored')),
    -- denormalised pointer to asset.file_id (no FK to avoid a cycle).
    asset_id         INTEGER,
    first_seen_at    INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    UNIQUE(root_id, rel_path)
);
CREATE INDEX file_dev_ino ON file(dev, ino);
CREATE INDEX file_fp128 ON file(fp128) WHERE fp128 IS NOT NULL;
CREATE INDEX file_tombstone ON file(tombstone_until) WHERE tombstone_until IS NOT NULL;
CREATE INDEX file_pending ON file(root_id) WHERE placement = 'pending';

-- The bulky, cold per-file JSON lives in a 1:1 side table so `file` rows stay
-- ≈150 bytes: the scanner's snapshot (read on every rescan) scans `file`
-- only. With the JSON inline (≈1 KB rows, 4 rows per page) a 100k-row
-- snapshot read ~10x more pages.
CREATE TABLE file_meta(
    file_id  INTEGER PRIMARY KEY REFERENCES file(id) ON DELETE CASCADE,
    probe    TEXT,      -- JSON flux_core::ProbeResult, null members dropped
    guess    TEXT       -- JSON flux_core::NameGuess, null members dropped
);

CREATE TABLE item(
    id                INTEGER PRIMARY KEY,
    library_id        INTEGER NOT NULL REFERENCES library(id) ON DELETE CASCADE,
    kind              TEXT NOT NULL CHECK(kind IN ('movie','show','season','episode','extra')),
    parent_id         INTEGER REFERENCES item(id) ON DELETE CASCADE,
    -- grouping identity among siblings: normalised title (top level),
    -- 's<n>' (season), 'e<n>' / 'd<date>' (episode), 'x:<title>' (extra),
    -- 'u:<path>' (unmatched).
    group_key         TEXT NOT NULL,
    title             TEXT NOT NULL,
    sort_title        TEXT NOT NULL,
    original_title    TEXT,
    year              INTEGER,
    season_number     INTEGER,
    episode_number    INTEGER,
    air_date          TEXT,
    extra_kind        TEXT,
    overview          TEXT,
    genres            TEXT,     -- JSON array of strings
    tmdb_id           INTEGER,
    tvdb_id           INTEGER,
    imdb_id           TEXT,
    external_ids      TEXT,     -- JSON object of other provider ids
    match_state       TEXT NOT NULL DEFAULT 'parsed'
                      CHECK(match_state IN ('unmatched','parsed','matched','locked')),
    match_confidence  REAL,
    match_evidence    TEXT,     -- JSON array of strings
    -- derived by the writer: 'available' (a playable file exists),
    -- 'unavailable' (only offline files), 'missing' (only tombstoned files).
    state             TEXT NOT NULL DEFAULT 'available' CHECK(state IN ('available','unavailable','missing')),
    badges            TEXT,     -- JSON array, merged from the item's assets
    added_at          INTEGER NOT NULL,
    -- newest descendant's added_at (a show is "recently added" when an episode is).
    content_added_at  INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL
);
CREATE INDEX item_top_lookup ON item(library_id, kind, group_key) WHERE parent_id IS NULL;
CREATE INDEX item_top_title ON item(library_id, sort_title, id) WHERE parent_id IS NULL;
CREATE INDEX item_top_added ON item(content_added_at, id) WHERE parent_id IS NULL;
CREATE INDEX item_children ON item(parent_id, kind, group_key);
CREATE INDEX item_tmdb ON item(library_id, kind, tmdb_id) WHERE tmdb_id IS NOT NULL;
CREATE INDEX item_tvdb ON item(library_id, kind, tvdb_id) WHERE tvdb_id IS NOT NULL;
CREATE INDEX item_imdb ON item(library_id, kind, imdb_id) WHERE imdb_id IS NOT NULL;

CREATE TABLE asset(
    id                  INTEGER PRIMARY KEY,
    item_id             INTEGER NOT NULL REFERENCES item(id) ON DELETE CASCADE,
    file_id             INTEGER NOT NULL UNIQUE REFERENCES file(id) ON DELETE CASCADE,
    container           TEXT,
    duration_ms         INTEGER,
    video_codec         TEXT,
    width               INTEGER,
    height              INTEGER,
    hdr                 TEXT,
    audio_summary       TEXT,
    edition             TEXT,
    part                INTEGER,
    label               TEXT NOT NULL DEFAULT '',
    badges              TEXT,   -- JSON array
    -- playback index cache (MKV Cues / MP4 sample tables), written by flux-hls.
    play_index          BLOB,
    play_index_version  INTEGER,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL
);
CREATE INDEX asset_item_id ON asset(item_id);

-- Every asset links to its primary item here too; a multi-episode file
-- (S01E01-E02) links to each of its episode items.
CREATE TABLE asset_item(
    item_id   INTEGER NOT NULL REFERENCES item(id) ON DELETE CASCADE,
    asset_id  INTEGER NOT NULL REFERENCES asset(id) ON DELETE CASCADE,
    PRIMARY KEY(item_id, asset_id)
) WITHOUT ROWID;
CREATE INDEX asset_item_asset ON asset_item(asset_id);

CREATE TABLE art(
    id          INTEGER PRIMARY KEY,
    item_id     INTEGER NOT NULL REFERENCES item(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL CHECK(kind IN ('poster','backdrop','thumb','logo')),
    source      TEXT NOT NULL CHECK(source IN ('provider','local','frame','generated')),
    path        TEXT,
    url         TEXT,
    width       INTEGER,
    height      INTEGER,
    blurhash    TEXT,
    generated   INTEGER NOT NULL DEFAULT 0,
    selected    INTEGER NOT NULL DEFAULT 1,
    created_at  INTEGER NOT NULL
);
CREATE INDEX art_item ON art(item_id, kind);

CREATE TABLE user(
    id            INTEGER PRIMARY KEY,
    login_name    TEXT NOT NULL UNIQUE,
    display_name  TEXT NOT NULL,
    role          TEXT NOT NULL CHECK(role IN ('owner','admin','viewer','kid','guest')),
    created_at    INTEGER NOT NULL
);

CREATE TABLE user_state(
    user_id         INTEGER NOT NULL REFERENCES user(id) ON DELETE CASCADE,
    item_id         INTEGER NOT NULL REFERENCES item(id) ON DELETE CASCADE,
    -- the asset the position refers to: editions with different durations
    -- must not blindly share a resume point (initial-discussion.md §7).
    asset_id        INTEGER REFERENCES asset(id) ON DELETE SET NULL,
    position_ms     INTEGER NOT NULL DEFAULT 0,
    duration_ms     INTEGER NOT NULL DEFAULT 0,
    watched         INTEGER NOT NULL DEFAULT 0,
    play_count      INTEGER NOT NULL DEFAULT 0,
    last_played_at  INTEGER,
    updated_at      INTEGER NOT NULL,
    PRIMARY KEY(user_id, item_id)
) WITHOUT ROWID;
CREATE INDEX user_state_recent ON user_state(user_id, updated_at);

CREATE TABLE job(
    id          INTEGER PRIMARY KEY,
    kind        TEXT NOT NULL,
    item_id     INTEGER REFERENCES item(id) ON DELETE CASCADE,
    state       TEXT NOT NULL DEFAULT 'queued' CHECK(state IN ('queued','running','done','failed')),
    priority    INTEGER NOT NULL DEFAULT 2,
    attempts    INTEGER NOT NULL DEFAULT 0,
    not_before  INTEGER NOT NULL DEFAULT 0,
    payload     TEXT,
    last_error  TEXT,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE UNIQUE INDEX job_active ON job(kind, item_id) WHERE state IN ('queued','running');
CREATE INDEX job_ready ON job(kind, state, priority, not_before);

-- Search. External-content FTS5 over item, kept in sync explicitly by the
-- writer (see fts.rs: triggers made every item insert open a statement
-- journal). Only movies, shows and episodes with a real title are indexed.
CREATE VIRTUAL TABLE item_fts USING fts5(
    title, original_title, overview,
    content='item', content_rowid='id',
    tokenize='unicode61 remove_diacritics 2',
    prefix='2 3'
);
-- `rank` = bm25 weighted title ≫ original title ≫ overview, so
-- `ORDER BY rank` is computed inside FTS5 (see read::search).
INSERT INTO item_fts(item_fts, rank) VALUES ('rank', 'bm25(10.0, 5.0, 1.0)');
"#;

/// v2 (core v2): who to credit for an image (`ArtRef::attribution`, e.g.
/// "TMDB") and which model generated it (`ArtRef::generator`).
const V2: &str = r#"
ALTER TABLE art ADD COLUMN attribution TEXT;
ALTER TABLE art ADD COLUMN generator TEXT;
"#;

/// v3 (phase C): show folders, absolute-episode maps, sort indexes, the
/// playback-index cache table, pinned art, and the durable job queue.
const V3: &str = r#"
-- Which top-level folders (root-relative, lowercased) a show's episode files
-- live in. A different show folder is a different show unless provider ids
-- agree (group.rs); `Doctor Who/` and `Doctor Who (1963)/` stay apart.
CREATE TABLE item_folder(
    folder   TEXT NOT NULL,
    item_id  INTEGER NOT NULL REFERENCES item(id) ON DELETE CASCADE,
    PRIMARY KEY(folder, item_id)
) WITHOUT ROWID;
CREATE INDEX item_folder_item ON item_folder(item_id);

-- Absolute episode numbers mapped to season/episode for a show (anime),
-- written by Writer::rekey_episodes; consulted when new files are placed.
CREATE TABLE episode_map(
    show_id   INTEGER NOT NULL REFERENCES item(id) ON DELETE CASCADE,
    absolute  INTEGER NOT NULL,
    season    INTEGER NOT NULL,
    episodes  TEXT NOT NULL,    -- JSON array of episode numbers
    PRIMARY KEY(show_id, absolute)
) WITHOUT ROWID;

-- Seasons and shows: visible episodes below them, maintained by the
-- writer's recompute (lists read it instead of counting per row).
ALTER TABLE item ADD COLUMN episode_count INTEGER;

-- Keyset sorts per library (read::items_page). Unknown years sort last in
-- both directions, ties by title then id. The per-library added index
-- replaces the global one (home merges libraries).
DROP INDEX item_top_added;
CREATE INDEX item_top_lib_added ON item(library_id, content_added_at, id) WHERE parent_id IS NULL;
CREATE INDEX item_top_year ON item(library_id, COALESCE(year, 99999), sort_title, id) WHERE parent_id IS NULL;
CREATE INDEX item_top_year_desc ON item(library_id, COALESCE(year, -1) DESC, sort_title, id) WHERE parent_id IS NULL;

-- Playback index cache: opaque bytes from flux-hls plus the format tag and
-- version, and the file stat it was built from. Valid only while the file's
-- (size, mtime) still match; the writer drops stale rows when a scan sees the
-- file change. Kept out of `asset` so asset rows stay narrow.
CREATE TABLE asset_index(
    asset_id         INTEGER PRIMARY KEY REFERENCES asset(id) ON DELETE CASCADE,
    format           TEXT NOT NULL,
    version          INTEGER NOT NULL,
    source_size      INTEGER NOT NULL,
    source_mtime_ns  INTEGER NOT NULL,
    bytes            BLOB NOT NULL,
    created_at       INTEGER NOT NULL
);
ALTER TABLE asset DROP COLUMN play_index;
ALTER TABLE asset DROP COLUMN play_index_version;

-- Art: the best image is chosen by source (provider > local > frame >
-- generated) unless the user pinned one; adding art no longer selects it.
ALTER TABLE art DROP COLUMN selected;
ALTER TABLE art ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;

-- The one durable job queue (match, art, …): idempotency keys, priorities,
-- leases that expire, retries with exponential backoff (writer.rs, jobs.rs).
CREATE TABLE job_v3(
    id            INTEGER PRIMARY KEY,
    kind          TEXT NOT NULL,
    -- idempotency key, unique per kind: enqueueing an existing key is a no-op
    key           TEXT NOT NULL,
    item_id       INTEGER REFERENCES item(id) ON DELETE CASCADE,
    state         TEXT NOT NULL DEFAULT 'queued' CHECK(state IN ('queued','running','done','failed')),
    -- lower runs first
    priority      INTEGER NOT NULL DEFAULT 50,
    attempts      INTEGER NOT NULL DEFAULT 0,
    max_attempts  INTEGER NOT NULL DEFAULT 5,
    -- earliest start: creation time, or the backoff after a failed attempt
    not_before    INTEGER NOT NULL DEFAULT 0,
    lease_owner   TEXT,
    lease_until   INTEGER,
    -- run only after this job finished (no FK: a deleted job counts as finished)
    after_id      INTEGER,
    payload       TEXT,
    last_error    TEXT,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    finished_at   INTEGER,
    UNIQUE(kind, key)
);
INSERT INTO job_v3(id, kind, key, item_id, state, priority, attempts, not_before, payload, last_error, created_at, updated_at)
    SELECT id, kind, kind || ':' || COALESCE(item_id, id), item_id,
           CASE state WHEN 'running' THEN 'queued' ELSE state END,
           priority * 10, attempts, MAX(not_before, created_at), payload, last_error, created_at, updated_at
    FROM job;
DROP TABLE job;
ALTER TABLE job_v3 RENAME TO job;
CREATE INDEX job_ready ON job(kind, priority, not_before, id) WHERE state = 'queued';
CREATE INDEX job_leased ON job(lease_until) WHERE state = 'running';
CREATE INDEX job_item ON job(item_id, kind) WHERE item_id IS NOT NULL;
"#;

/// v4 (phase D): probe retries, tombstone re-attach lookups, newest-first
/// art jobs.
const V4: &str = r#"
-- A failed probe is retried by later scans with exponential backoff
-- (scan_support: 1 min doubling to 1 day); a success resets both.
ALTER TABLE file ADD COLUMN probe_attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE file ADD COLUMN probe_retry_at INTEGER;

-- Re-attach looks up tombstones by content. The plain fp128 index also
-- holds every live copy of that content (thousands in a library of cloned
-- files), so a lookup scanned them all; this one holds tombstones only.
CREATE INDEX file_fp_missing ON file(fp128, size) WHERE state = 'missing';

-- Art jobs are claimed newest first (the items a user just added, and the
-- top of "Recently added", get art before the back catalogue).
CREATE INDEX job_ready_art ON job(priority, id DESC) WHERE state = 'queued' AND kind = 'art';
"#;

/// Bring the database up to [`SCHEMA_VERSION`], one transaction per step.
///
/// Idempotent: a database already at the current version is left untouched
/// (no writes).
pub fn migrate(conn: &mut Connection) -> Result<()> {
    let found: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if found > SCHEMA_VERSION {
        return Err(CatalogError::SchemaTooNew {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(found as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        // `user_version` cannot be bound as a parameter; the value is ours.
        tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
        tx.commit()?;
        tracing::info!(version = i + 1, "catalog schema migrated");
    }
    Ok(())
}

/// Pragmas for the single read-write connection (research/02 §7.2).
///
/// `synchronous=NORMAL` in WAL mode is durable at checkpoint; a crash can lose
/// the last few commits but never corrupts, which is fine for a catalogue the
/// scanner can rebuild.
///
/// Commits do not fsync in this mode, but every automatic checkpoint fsyncs
/// both the WAL and the database. With SQLite's default (every 1000 pages)
/// a 100k-file initial scan spent most of its time in `fsync` on the USB SSD,
/// and more so as the database grew (each transaction dirties more scattered
/// index pages). `wal_autocheckpoint_pages` raises that interval; the writer
/// also checkpoints passively whenever it goes idle, and `journal_size_limit`
/// truncates the WAL file back afterwards.
pub(crate) fn configure_writer(
    conn: &Connection,
    cache_kib: i64,
    mmap_bytes: i64,
    wal_autocheckpoint_pages: u32,
) -> Result<()> {
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(CatalogError::Invalid(format!(
            "could not enable WAL (journal_mode = {mode})"
        )));
    }
    conn.execute_batch(&format!(
        "PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = MEMORY;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;
         PRAGMA cache_size = -{cache_kib};
         PRAGMA mmap_size = {mmap_bytes};
         PRAGMA wal_autocheckpoint = {wal_autocheckpoint_pages};
         PRAGMA journal_size_limit = 67108864;"
    ))?;
    conn.set_prepared_statement_cache_capacity(128);
    Ok(())
}

/// Pragmas for pooled read connections: `query_only` so a bug in a read path
/// can never write, a smaller page cache, and the same mmap window.
pub(crate) fn configure_reader(conn: &Connection, cache_kib: i64, mmap_bytes: i64) -> Result<()> {
    conn.execute_batch(&format!(
        "PRAGMA query_only = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA busy_timeout = 5000;
         PRAGMA cache_size = -{cache_kib};
         PRAGMA mmap_size = {mmap_bytes};"
    ))?;
    conn.set_prepared_statement_cache_capacity(64);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_tmp() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("t.db")).unwrap();
        (dir, conn)
    }

    #[test]
    fn migrates_fresh_db_to_latest() {
        let (_d, mut c) = open_tmp();
        configure_writer(&c, 8192, 0, 1000).unwrap();
        migrate(&mut c).unwrap();
        let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        for t in [
            "library",
            "root",
            "dir",
            "file",
            "file_meta",
            "asset",
            "asset_item",
            "item",
            "art",
            "user",
            "user_state",
            "job",
            "item_fts",
        ] {
            let n: i64 = c
                .query_row("SELECT count(*) FROM sqlite_master WHERE name = ?1", [t], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(n, 1, "table {t} missing");
        }
        let mode: String = c.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn migrate_is_idempotent_and_writes_nothing_when_current() {
        let (_d, mut c) = open_tmp();
        migrate(&mut c).unwrap();
        let before = c.total_changes();
        migrate(&mut c).unwrap();
        assert_eq!(c.total_changes(), before);
    }

    #[test]
    fn upgrades_a_v1_catalog_in_place() {
        let (_d, mut c) = open_tmp();
        c.execute_batch(V1).unwrap();
        c.execute_batch("PRAGMA user_version = 1").unwrap();
        c.execute_batch(
            "INSERT INTO library(id, name, kind, created_at) VALUES (1, 'M', 'movies', 0);
             INSERT INTO item(id, library_id, kind, group_key, title, sort_title, added_at, content_added_at, updated_at)
             VALUES (1, 1, 'movie', 'heat', 'Heat', 'heat', 0, 0, 0);
             INSERT INTO art(item_id, kind, source, created_at) VALUES (1, 'poster', 'provider', 0);",
        )
        .unwrap();
        migrate(&mut c).unwrap();
        let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let (n, attribution): (i64, Option<String>) = c
            .query_row("SELECT count(*), max(attribution) FROM art", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((n, attribution), (1, None), "rows kept, new columns empty");
    }

    #[test]
    fn refuses_newer_schema() {
        let (_d, mut c) = open_tmp();
        c.execute_batch("PRAGMA user_version = 999").unwrap();
        assert!(matches!(
            migrate(&mut c),
            Err(CatalogError::SchemaTooNew { found: 999, .. })
        ));
    }

    #[test]
    fn fts5_is_available_in_the_bundled_sqlite() {
        let (_d, mut c) = open_tmp();
        migrate(&mut c).unwrap();
        c.execute_batch("INSERT INTO item_fts(item_fts) VALUES('integrity-check');")
            .unwrap();
    }
}
