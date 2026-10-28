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
//! | `subtitle_file` | subtitle files (`.srt`, `.ass`, `.sup`…) the scan saw, per directory; matched to videos when read (`sidecar.rs`) |
//! | `item_field`, `changeset`, `change`, `lens` | where each value came from, the journal of edits, lenses kept on the server (`fields.rs`, `changes.rs`) |
//! | `file_fix` | what a file is when its name says otherwise (`fixes.rs`) |
//! | `meta_file` | `.flux.json` and `.nfo` files beside the videos and what flux read from them (`metafiles.rs`) |
//! | `collection`, `collection_item` | collections kept on the server (`collections.rs`) |
//!
//! Timestamps are Unix **milliseconds** (`i64`) everywhere. `dev`/`ino` are
//! stored as the bit pattern of the `u64` in an `i64` column (SQLite integers
//! are signed); see [`crate::model::u64_to_sql`].

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::error::{CatalogError, Result};

/// Schema version this build writes. Bump by appending to `MIGRATIONS`.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// Ordered migrations; index `i` upgrades `user_version` `i` → `i + 1`.
/// Never edit a released migration: append a new one.
const MIGRATIONS: &[&str] = &[V1, V2, V3, V4, V5, V6, V7, V8, V9];

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

/// v5: subtitle files beside the videos (sidecars).
const V5: &str = r#"
-- Every subtitle file the scan sees (srt, ass, ssa, vtt, sup, idx), by
-- directory. Which videos a file belongs to (same name, same episode in the
-- folder or a Subs folder…) and its language are decided when an asset is
-- read (sidecar.rs), so a rescan of one directory keeps every video's list
-- current. Rows go when the scan proves the file gone.
CREATE TABLE subtitle_file(
    id        INTEGER PRIMARY KEY,
    root_id   INTEGER NOT NULL REFERENCES root(id) ON DELETE CASCADE,
    rel_path  TEXT NOT NULL,
    -- the directory part of rel_path ('' = the root), matched without case
    dir       TEXT NOT NULL COLLATE NOCASE,
    size      INTEGER NOT NULL,
    mtime_ns  INTEGER NOT NULL,
    UNIQUE(root_id, rel_path)
);
CREATE INDEX subtitle_file_dir ON subtitle_file(root_id, dir);
"#;

/// v6: art that survives a moved data dir and knows what made it.
///
/// - `path` of flux's own images (frame and generated art) is relative to
///   the data dir (`art/<file>`), so moving the data dir keeps every image.
///   Rows of older catalogs held the absolute path of the data dir they
///   were made in; those whose file is in an `art` folder are rewritten
///   here (the server also reads a legacy absolute path, preferring the
///   same file name in its own `art` folder). Local art (next to the media)
///   keeps its absolute path.
/// - `gen_version`: the art version (`flux_meta::art::ART_VERSION`) that
///   made a frame or generated image (NULL: made before versioning). An
///   older one is made again (fonts, tone mapping…), see
///   [`crate::read::stale_art`].
/// - `gen_title`: the caption typeset into a poster ("Title (Year)"); a
///   poster whose item is titled otherwise now is made again.
/// - Ids are never reused (`AUTOINCREMENT`): `/api/v1/art/{id}` responses
///   are cached as immutable, by browsers and in `art-cache/`, so an id
///   must not name another image after its row is deleted.
const V6: &str = r#"
CREATE TABLE art_v6(
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id      INTEGER NOT NULL REFERENCES item(id) ON DELETE CASCADE,
    kind         TEXT NOT NULL CHECK(kind IN ('poster','backdrop','thumb','logo')),
    source       TEXT NOT NULL CHECK(source IN ('provider','local','frame','generated')),
    path         TEXT,
    url          TEXT,
    width        INTEGER,
    height       INTEGER,
    blurhash     TEXT,
    generated    INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL,
    attribution  TEXT,
    generator    TEXT,
    pinned       INTEGER NOT NULL DEFAULT 0,
    gen_version  INTEGER,
    gen_title    TEXT
);
-- `p` is the path with `/` separators, `dir` its folder part up to the
-- last `/` (rtrim strips every character that isn't a `/` from the end).
INSERT INTO art_v6(id, item_id, kind, source, path, url, width, height, blurhash, generated, created_at,
                   attribution, generator, pinned)
SELECT id, item_id, kind, source,
       CASE WHEN source IN ('frame', 'generated') AND dir LIKE '%/art/' AND length(p) > length(dir)
            THEN 'art/' || substr(p, length(dir) + 1)
            ELSE path END,
       url, width, height, blurhash, generated, created_at, attribution, generator, pinned
  FROM (SELECT *, rtrim(p, replace(p, '/', '')) AS dir
          FROM (SELECT *, replace(path, '\', '/') AS p FROM art));
DROP TABLE art;
ALTER TABLE art_v6 RENAME TO art;
CREATE INDEX art_item ON art(item_id, kind);
"#;

/// v7: the `videos` library kind (`LibraryKind::Videos`). SQLite can't
/// change a CHECK constraint, so `library` is rebuilt the way SQLite's
/// documentation describes (new table, copy, drop, rename). [`migrate`]
/// runs migrations with foreign keys off, so dropping the old table
/// deletes nothing that references it; `root` and `item` keep referring to
/// `library` by name, which is the new table once it is renamed.
const V7: &str = r#"
CREATE TABLE library_v7(
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL,
    kind        TEXT NOT NULL CHECK(kind IN ('movies','shows','mixed','videos')),
    created_at  INTEGER NOT NULL
);
INSERT INTO library_v7(id, name, kind, created_at) SELECT id, name, kind, created_at FROM library;
DROP TABLE library;
ALTER TABLE library_v7 RENAME TO library;
"#;

/// v8: agents (docs/AGENT-NATIVE.md).
///
/// - `item_field`: where a field's value came from (a provider match, a
///   person, an agent, with its sources); a person's or agent's row locks
///   the field against the matcher and the scanner (`fields.rs`).
/// - `changeset` and `change`: every edit, journaled so it can be reviewed
///   and undone (`changes.rs`). `change.item_id` has no foreign key: the
///   history outlives an item.
/// - `item.details`: what agents dig out beyond the columns (directors,
///   cast, languages, links…), a JSON object.
/// - `art`: rebuilt to allow the `custom` source (an image a person or an
///   agent chose), with `found_at`, the page it came from. AUTOINCREMENT's
///   high-water mark carries over, so an id is still never reused.
/// - `lens`: lenses kept on the server, per person (`owner` = user id) or
///   for everyone (`owner` 0).
/// - What earlier online TMDB matches filled in (a description only ever
///   came from one) is recorded as the provider's, so agents are never
///   shown it.
const V8: &str = r#"
ALTER TABLE item ADD COLUMN details TEXT;

CREATE TABLE item_field(
    item_id       INTEGER NOT NULL REFERENCES item(id) ON DELETE CASCADE,
    field         TEXT NOT NULL,
    source        TEXT NOT NULL CHECK(source IN ('provider','user','agent')),
    provider      TEXT,
    agent         TEXT,
    actor         TEXT,
    changeset_id  INTEGER,
    sources       TEXT,
    at            INTEGER NOT NULL,
    PRIMARY KEY(item_id, field)
) WITHOUT ROWID;

CREATE TABLE changeset(
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    actor        TEXT NOT NULL,
    agent        TEXT,
    summary      TEXT NOT NULL,
    state        TEXT NOT NULL CHECK(state IN ('proposed','applied','reverted','rejected')),
    created_at   INTEGER NOT NULL,
    decided_at   INTEGER,
    decided_by   TEXT,
    reverted_at  INTEGER,
    reverted_by  TEXT
);
CREATE INDEX changeset_proposed ON changeset(id) WHERE state = 'proposed';

CREATE TABLE change(
    changeset_id   INTEGER NOT NULL REFERENCES changeset(id) ON DELETE CASCADE,
    seq            INTEGER NOT NULL,
    item_id        INTEGER NOT NULL,
    field          TEXT NOT NULL,
    op             TEXT NOT NULL DEFAULT 'set' CHECK(op IN ('set','clear')),
    before         TEXT,
    after          TEXT,
    before_source  TEXT,
    sources        TEXT,
    state          TEXT NOT NULL CHECK(state IN ('pending','applied','skipped','reverted','kept')),
    PRIMARY KEY(changeset_id, seq)
) WITHOUT ROWID;
CREATE INDEX change_item ON change(item_id, changeset_id);

CREATE TABLE lens(
    owner       INTEGER NOT NULL,
    id          TEXT NOT NULL,
    spec        TEXT NOT NULL,
    added_by    TEXT NOT NULL,
    agent       TEXT,
    added_at    INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY(owner, id)
) WITHOUT ROWID;

CREATE TABLE art_v8(
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id      INTEGER NOT NULL REFERENCES item(id) ON DELETE CASCADE,
    kind         TEXT NOT NULL CHECK(kind IN ('poster','backdrop','thumb','logo')),
    source       TEXT NOT NULL CHECK(source IN ('provider','local','frame','generated','custom')),
    path         TEXT,
    url          TEXT,
    width        INTEGER,
    height       INTEGER,
    blurhash     TEXT,
    generated    INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL,
    attribution  TEXT,
    generator    TEXT,
    pinned       INTEGER NOT NULL DEFAULT 0,
    gen_version  INTEGER,
    gen_title    TEXT,
    found_at     TEXT
);
INSERT INTO art_v8(id, item_id, kind, source, path, url, width, height, blurhash, generated, created_at,
                   attribution, generator, pinned, gen_version, gen_title)
SELECT id, item_id, kind, source, path, url, width, height, blurhash, generated, created_at,
       attribution, generator, pinned, gen_version, gen_title FROM art;
DELETE FROM sqlite_sequence WHERE name = 'art_v8';
INSERT INTO sqlite_sequence(name, seq)
SELECT 'art_v8', max(COALESCE((SELECT max(seq) FROM sqlite_sequence WHERE name = 'art'), 0),
                     COALESCE((SELECT max(id) FROM art_v8), 0));
DROP TABLE art;
ALTER TABLE art_v8 RENAME TO art;
CREATE INDEX art_item ON art(item_id, kind);

INSERT OR IGNORE INTO item_field(item_id, field, source, provider, at)
SELECT id, 'title', 'provider', 'tmdb', updated_at FROM item
 WHERE match_state = 'matched' AND tmdb_id IS NOT NULL AND overview IS NOT NULL;
INSERT OR IGNORE INTO item_field(item_id, field, source, provider, at)
SELECT id, 'overview', 'provider', 'tmdb', updated_at FROM item
 WHERE match_state = 'matched' AND tmdb_id IS NOT NULL AND overview IS NOT NULL;
INSERT OR IGNORE INTO item_field(item_id, field, source, provider, at)
SELECT id, 'genres', 'provider', 'tmdb', updated_at FROM item
 WHERE match_state = 'matched' AND tmdb_id IS NOT NULL AND overview IS NOT NULL AND genres IS NOT NULL;
"#;

/// v9: agents organise the library (docs/AGENT-NATIVE.md, Phase 2).
///
/// - `file_fix`: what a file is when its name says otherwise (`fixes.rs`):
///   the scan places the file by its guess with the fix applied. `of_item`
///   (the work a fix pins the file to, no foreign key) follows merges.
/// - `meta_file`: metadata files beside the videos, `.flux.json` and
///   `.nfo`, with what flux read from them (`metafiles.rs`).
/// - `collection`, `collection_item`: collections kept on the server, per
///   person (`owner` = user id) or for everyone (`owner` 0), picked by hand
///   and/or by a rule (`collections.rs`).
/// - `item_field`: rebuilt to allow the `sidecar` source (a value read from
///   a metadata file; `provider` says which kind).
const V9: &str = r#"
CREATE TABLE file_fix(
    file_id       INTEGER PRIMARY KEY REFERENCES file(id) ON DELETE CASCADE,
    fix           TEXT NOT NULL,
    of_item       INTEGER,
    source        TEXT NOT NULL CHECK(source IN ('user','agent','sidecar')),
    agent         TEXT,
    actor         TEXT,
    changeset_id  INTEGER,
    at            INTEGER NOT NULL
);
CREATE INDEX file_fix_of ON file_fix(of_item) WHERE of_item IS NOT NULL;

CREATE TABLE meta_file(
    root_id    INTEGER NOT NULL REFERENCES root(id) ON DELETE CASCADE,
    rel_path   TEXT NOT NULL,
    dir        TEXT NOT NULL,
    format     TEXT NOT NULL CHECK(format IN ('flux','nfo')),
    size       INTEGER NOT NULL,
    mtime_ns   INTEGER NOT NULL,
    content    TEXT,
    error      TEXT,
    PRIMARY KEY(root_id, rel_path)
) WITHOUT ROWID;
CREATE INDEX meta_file_dir ON meta_file(root_id, dir);

CREATE TABLE collection(
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    owner        INTEGER NOT NULL,
    name         TEXT NOT NULL,
    description  TEXT,
    rule         TEXT,
    sort         TEXT NOT NULL DEFAULT 'picked',
    added_by     TEXT NOT NULL,
    agent        TEXT,
    sources      TEXT,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);
CREATE UNIQUE INDEX collection_name ON collection(owner, name COLLATE NOCASE);

CREATE TABLE collection_item(
    collection_id  INTEGER NOT NULL REFERENCES collection(id) ON DELETE CASCADE,
    item_id        INTEGER NOT NULL REFERENCES item(id) ON DELETE CASCADE,
    position       INTEGER NOT NULL,
    note           TEXT,
    PRIMARY KEY(collection_id, item_id)
) WITHOUT ROWID;
CREATE INDEX collection_item_item ON collection_item(item_id);

CREATE TABLE item_field_v9(
    item_id       INTEGER NOT NULL REFERENCES item(id) ON DELETE CASCADE,
    field         TEXT NOT NULL,
    source        TEXT NOT NULL CHECK(source IN ('provider','user','agent','sidecar')),
    provider      TEXT,
    agent         TEXT,
    actor         TEXT,
    changeset_id  INTEGER,
    sources       TEXT,
    at            INTEGER NOT NULL,
    PRIMARY KEY(item_id, field)
) WITHOUT ROWID;
INSERT INTO item_field_v9(item_id, field, source, provider, agent, actor, changeset_id, sources, at)
SELECT item_id, field, source, provider, agent, actor, changeset_id, sources, at FROM item_field;
DROP TABLE item_field;
ALTER TABLE item_field_v9 RENAME TO item_field;
"#;

/// Bring the database up to [`SCHEMA_VERSION`], one transaction per step.
///
/// Foreign keys are off while migrations run (a migration may rebuild a
/// table that others reference, and dropping it must not cascade), and
/// checked afterwards. Idempotent: a database already at the current
/// version is left untouched (no writes).
///
/// A catalog an older flux made is copied first (`backup_before_migrating`):
/// migrating is the one step of an upgrade that the older flux can't undo.
pub fn migrate(conn: &mut Connection) -> Result<()> {
    let found: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if found > SCHEMA_VERSION {
        return Err(CatalogError::SchemaTooNew {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    if found == SCHEMA_VERSION {
        return Ok(());
    }
    if found > 0 {
        backup_before_migrating(conn, found)?;
    }
    // `foreign_keys` can only change outside a transaction.
    let fk: bool = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
    if fk {
        conn.execute_batch("PRAGMA foreign_keys = OFF")?;
    }
    let res = run_migrations(conn, found);
    if fk {
        conn.execute_batch("PRAGMA foreign_keys = ON")?;
    }
    res?;
    let broken: i64 = conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r.get(0))?;
    if broken > 0 {
        tracing::warn!(rows = broken, "catalog rows refer to rows that don't exist");
    }
    Ok(())
}

/// Backups of catalogs made before a migration that are kept: the newest two.
pub const KEPT_BACKUPS: usize = 2;

/// The copy of a catalog at schema `version`, kept before migrating it:
/// `catalog.db.v8.bak` beside `catalog.db`. The flux that made it can open it
/// again (`flux-server update --rollback` puts it back).
pub fn backup_path(db: &Path, version: i64) -> PathBuf {
    let mut name = db
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "catalog.db".into());
    name.push(format!(".v{version}.bak"));
    db.with_file_name(name)
}

/// The backups `backup_before_migrating` made beside `db`, oldest schema
/// first: `(version, path)`.
pub fn backups(db: &Path) -> Vec<(i64, PathBuf)> {
    let Some(stem) = db.file_name().and_then(|n| n.to_str()).map(|n| format!("{n}.v")) else {
        return Vec::new();
    };
    let dir = db
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut found: Vec<(i64, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let version = name.strip_prefix(&stem)?.strip_suffix(".bak")?.parse().ok()?;
            Some((version, e.path()))
        })
        .collect();
    found.sort();
    found
}

/// Copies the catalog, as it is at schema `version`, beside itself
/// ([`backup_path`]) with `VACUUM INTO`: one consistent file, WAL included.
/// Keeps the newest [`KEPT_BACKUPS`].
fn backup_before_migrating(conn: &Connection, version: i64) -> Result<()> {
    let Some(db) = conn.path().filter(|p| !p.is_empty()).map(PathBuf::from) else {
        return Ok(()); // in memory: nothing to keep
    };
    let to = backup_path(&db, version);
    // VACUUM INTO refuses to write over a file.
    let _ = std::fs::remove_file(&to);
    conn.execute("VACUUM INTO ?1", [to.to_string_lossy()])?;
    tracing::info!(from = version, to = SCHEMA_VERSION, backup = %to.display(), "catalog copied before migrating");
    let all = backups(&db);
    for (_, old) in all.iter().take(all.len().saturating_sub(KEPT_BACKUPS)) {
        let _ = std::fs::remove_file(old);
    }
    Ok(())
}

fn run_migrations(conn: &mut Connection, found: i64) -> Result<()> {
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
            "subtitle_file",
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
    fn an_older_catalog_is_copied_before_it_is_migrated() {
        let (d, mut c) = open_tmp();
        let db = d.path().join("t.db");
        for sql in &MIGRATIONS[..5] {
            c.execute_batch(sql).unwrap();
        }
        c.execute_batch(
            "PRAGMA user_version = 5;
             INSERT INTO library(id, name, kind, created_at) VALUES (1, 'Films', 'movies', 0);",
        )
        .unwrap();
        migrate(&mut c).unwrap();
        // The copy is the catalog as the v5 flux left it: that flux opens it again.
        let copy = backup_path(&db, 5);
        assert_eq!(copy, d.path().join("t.db.v5.bak"));
        let old = Connection::open(&copy).unwrap();
        let v: i64 = old.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        let name: String = old
            .query_row("SELECT name FROM library", [], |r| r.get(0))
            .unwrap();
        assert_eq!((v, name.as_str()), (5, "Films"));
        assert_eq!(backups(&db), vec![(5, copy)]);
        // Migrating again (already current) copies nothing.
        migrate(&mut c).unwrap();
        assert_eq!(backups(&db).len(), 1);
    }

    #[test]
    fn a_new_catalog_needs_no_copy_and_only_the_newest_two_are_kept() {
        let (d, mut c) = open_tmp();
        let db = d.path().join("t.db");
        migrate(&mut c).unwrap();
        assert!(backups(&db).is_empty(), "a fresh catalog has nothing to keep");
        drop(c);
        // Three older copies, and files that aren't copies of this catalog.
        for v in [2, 3, 4] {
            std::fs::write(backup_path(&db, v), b"").unwrap();
        }
        std::fs::write(d.path().join("t.db.vx.bak"), b"").unwrap();
        std::fs::write(d.path().join("other.db.v1.bak"), b"").unwrap();
        // Then a v5 catalog migrates.
        std::fs::remove_file(&db).unwrap();
        let mut c = Connection::open(&db).unwrap();
        for sql in &MIGRATIONS[..5] {
            c.execute_batch(sql).unwrap();
        }
        c.execute_batch("PRAGMA user_version = 5").unwrap();
        migrate(&mut c).unwrap();
        let kept: Vec<i64> = backups(&db).into_iter().map(|(v, _)| v).collect();
        assert_eq!(kept, vec![4, 5]);
        assert!(d.path().join("t.db.vx.bak").exists());
        assert!(d.path().join("other.db.v1.bak").exists());
    }

    /// Catalogs up to v5 stored flux's own images by the absolute path of
    /// the data dir they were made in: moving the data dir lost them all.
    #[test]
    fn v6_makes_flux_art_paths_relative_and_never_reuses_art_ids() {
        let (_d, mut c) = open_tmp();
        for sql in &MIGRATIONS[..5] {
            c.execute_batch(sql).unwrap();
        }
        c.execute_batch("PRAGMA user_version = 5").unwrap();
        c.execute_batch(
            r#"INSERT INTO library(id, name, kind, created_at) VALUES (1, 'M', 'movies', 0);
               INSERT INTO item(id, library_id, kind, group_key, title, sort_title, added_at, content_added_at, updated_at)
               VALUES (1, 1, 'movie', 'heat', 'Heat', 'heat', 0, 0, 0);
               INSERT INTO art(id, item_id, kind, source, path, url, created_at, pinned) VALUES
                 (1, 1, 'poster',   'frame',     'D:\old\flux data\art\1-poster-0a1b2c3d.jpg', NULL, 0, 1),
                 (2, 1, 'backdrop', 'frame',     '/srv/old/art/1-backdrop-00ff00ff.jpg', NULL, 0, 0),
                 (3, 1, 'poster',   'local',     '/media/Heat (1995)/art/poster.jpg', NULL, 0, 0),
                 (4, 1, 'poster',   'provider',  NULL, 'https://image.example/p.jpg', 0, 0),
                 (5, 1, 'thumb',    'frame',     '\\?\D:\data\ART\2-thumb-12345678.jpg', NULL, 0, 0),
                 (6, 1, 'backdrop', 'frame',     'D:\elsewhere\1-backdrop-0000beef.jpg', NULL, 0, 0),
                 (7, 1, 'poster',   'generated', '/home/用户/flux/art/1-poster-deadbeef.jpg', NULL, 0, 0);"#,
        )
        .unwrap();
        migrate(&mut c).unwrap();
        type Row = (i64, Option<String>, i64, Option<i64>, Option<String>);
        let rows: Vec<Row> = c
            .prepare("SELECT id, path, pinned, gen_version, gen_title FROM art ORDER BY id")
            .unwrap()
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let paths: Vec<Option<&str>> = rows.iter().map(|r| r.1.as_deref()).collect();
        assert_eq!(
            paths,
            [
                Some("art/1-poster-0a1b2c3d.jpg"),
                Some("art/1-backdrop-00ff00ff.jpg"),
                Some("/media/Heat (1995)/art/poster.jpg"), // local art stays where it is
                None,
                Some("art/2-thumb-12345678.jpg"),
                Some(r"D:\elsewhere\1-backdrop-0000beef.jpg"), // not in an art folder
                Some("art/1-poster-deadbeef.jpg"),
            ]
        );
        assert_eq!(rows[0].2, 1, "pins are kept");
        assert!(
            rows.iter().all(|r| r.3.is_none() && r.4.is_none()),
            "made before versioning"
        );
        // The newest image's id is never handed out again.
        c.execute_batch(
            "DELETE FROM art WHERE id = 7;
             INSERT INTO art(item_id, kind, source, created_at) VALUES (1, 'poster', 'provider', 0);",
        )
        .unwrap();
        let max: i64 = c.query_row("SELECT max(id) FROM art", [], |r| r.get(0)).unwrap();
        assert_eq!(max, 8);
        let idx: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'art_item'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(idx, 1);
    }

    /// v7 rebuilds `library` to allow the `videos` kind. Roots and items
    /// refer to it with ON DELETE CASCADE (and art to items): the rebuild
    /// must keep them all, and deleting a library must still cascade
    /// afterwards. Catalogs arrive at v5 (before v6 rebuilt `art`) and at v6.
    #[test]
    fn the_library_rebuild_keeps_what_refers_to_it() {
        for from in [5, 6] {
            library_rebuild_from(from);
        }
    }

    fn library_rebuild_from(from: usize) {
        let (_d, mut c) = open_tmp();
        configure_writer(&c, 8192, 0, 1000).unwrap();
        // A catalog at `from`, written with foreign keys on as the writer does.
        let fk = c
            .query_row("PRAGMA foreign_keys", [], |r| r.get::<_, bool>(0))
            .unwrap();
        assert!(fk);
        for (i, sql) in MIGRATIONS.iter().take(from).enumerate() {
            c.execute_batch(sql).unwrap();
            c.execute_batch(&format!("PRAGMA user_version = {}", i + 1))
                .unwrap();
        }
        c.execute_batch(
            "INSERT INTO library(id, name, kind, created_at) VALUES (1, 'M', 'movies', 0), (2, 'S', 'shows', 0);
             INSERT INTO root(id, library_id, path, created_at) VALUES (1, 1, '/m', 0), (2, 2, '/s', 0);
             INSERT INTO item(id, library_id, kind, group_key, title, sort_title, added_at, content_added_at, updated_at)
             VALUES (1, 1, 'movie', 'heat', 'Heat', 'heat', 0, 0, 0), (2, 2, 'show', 'x', 'X', 'x', 0, 0, 0);
             INSERT INTO art(id, item_id, kind, source, path, created_at) VALUES
               (1, 1, 'poster', 'frame', 'art/1-poster-0a1b2c3d.jpg', 0),
               (2, 2, 'poster', 'frame', 'art/2-poster-00ff00ff.jpg', 0);",
        )
        .unwrap();
        assert!(
            c.execute(
                "INSERT INTO library(name, kind, created_at) VALUES ('V', 'videos', 0)",
                []
            )
            .is_err(),
            "v{from}"
        );
        migrate(&mut c).unwrap();
        let count = |sql: &str| c.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(count("PRAGMA user_version"), SCHEMA_VERSION, "v{from}");
        assert_eq!(
            (
                count("SELECT count(*) FROM library"),
                count("SELECT count(*) FROM root"),
                count("SELECT count(*) FROM item"),
                count("SELECT count(*) FROM art")
            ),
            (2, 2, 2, 2),
            "v{from}"
        );
        assert_eq!(
            count("SELECT count(*) FROM pragma_foreign_key_check"),
            0,
            "v{from}"
        );
        assert!(c
            .query_row("PRAGMA foreign_keys", [], |r| r.get::<_, bool>(0))
            .unwrap());
        c.execute(
            "INSERT INTO library(name, kind, created_at) VALUES ('V', 'videos', 0)",
            [],
        )
        .unwrap();
        assert!(c
            .execute(
                "INSERT INTO library(name, kind, created_at) VALUES ('X', 'nope', 0)",
                []
            )
            .is_err());
        // v6's art ids are still never reused.
        c.execute_batch(
            "DELETE FROM art WHERE id = 2;
             INSERT INTO art(item_id, kind, source, created_at) VALUES (2, 'poster', 'provider', 0);",
        )
        .unwrap();
        assert_eq!(count("SELECT max(id) FROM art"), 3, "v{from}");
        // The cascade still reaches roots, items and their art.
        c.execute("DELETE FROM library WHERE id = 1", []).unwrap();
        assert_eq!(
            (
                count("SELECT count(*) FROM root"),
                count("SELECT count(*) FROM item"),
                count("SELECT count(*) FROM art")
            ),
            (1, 1, 1),
            "v{from}"
        );
    }

    /// v8 rebuilds `art` to allow chosen (`custom`) images: rows, pins and
    /// AUTOINCREMENT's high-water mark carry over (an id once used is never
    /// used again), and what an online TMDB match filled in is recorded as
    /// TMDB's.
    #[test]
    fn v8_keeps_art_ids_and_records_tmdb_fields() {
        let (_d, mut c) = open_tmp();
        configure_writer(&c, 8192, 0, 1000).unwrap();
        for (i, sql) in MIGRATIONS.iter().take(7).enumerate() {
            c.execute_batch(sql).unwrap();
            c.execute_batch(&format!("PRAGMA user_version = {}", i + 1))
                .unwrap();
        }
        c.execute_batch(
            "INSERT INTO library(id, name, kind, created_at) VALUES (1, 'M', 'movies', 0);
             INSERT INTO item(id, library_id, kind, group_key, title, sort_title, added_at, content_added_at, updated_at,
                              match_state, tmdb_id, overview, genres)
             VALUES (1, 1, 'movie', 'a', 'Parasite', 'parasite', 0, 0, 5, 'matched', 496243, 'TMDB text', '[\"Thriller\"]'),
                    (2, 1, 'movie', 'b', 'B', 'b', 0, 0, 5, 'matched', 7, NULL, NULL),
                    (3, 1, 'movie', 'c', 'C', 'c', 0, 0, 5, 'parsed', NULL, NULL, NULL);
             INSERT INTO art(id, item_id, kind, source, path, created_at, pinned) VALUES
               (1, 1, 'poster', 'frame', 'art/1-poster-0a1b2c3d.jpg', 0, 1),
               (2, 1, 'poster', 'frame', 'art/1-poster-0a1b2c3e.jpg', 0, 0),
               (3, 2, 'poster', 'frame', 'art/2-poster-0a1b2c3f.jpg', 0, 0);
             DELETE FROM art WHERE id = 3;",
        )
        .unwrap();
        migrate(&mut c).unwrap();
        let count = |sql: &str| c.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(count("PRAGMA user_version"), SCHEMA_VERSION);
        assert_eq!(count("SELECT count(*) FROM art"), 2);
        assert_eq!(count("SELECT pinned FROM art WHERE id = 1"), 1);
        c.execute(
            "INSERT INTO art(item_id, kind, source, path, created_at) VALUES (1, 'poster', 'custom', 'art/1-customposter-00000001.jpg', 0)",
            [],
        )
        .unwrap();
        assert_eq!(
            count("SELECT max(id) FROM art"),
            4,
            "id 3 was used once: never again"
        );
        assert_eq!(count("SELECT count(*) FROM pragma_foreign_key_check"), 0);
        let fields: Vec<String> = c
            .prepare("SELECT item_id || ':' || field || ':' || provider FROM item_field ORDER BY 1")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(fields, ["1:genres:tmdb", "1:overview:tmdb", "1:title:tmdb"]);
        // Deleting an item still takes its art and its sources with it.
        c.execute("DELETE FROM item WHERE id = 1", []).unwrap();
        assert_eq!(count("SELECT count(*) FROM art"), 0);
        assert_eq!(count("SELECT count(*) FROM item_field"), 0);
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
