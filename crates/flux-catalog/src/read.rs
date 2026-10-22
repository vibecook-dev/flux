//! Readers: a small pool of `query_only` connections and the queries the
//! API serves, returning `flux_core::api` DTOs.
//!
//! WAL lets these run concurrently with the writer; research/02 §7.1
//! measured point-query p99 ≤ 0.15–0.9 ms during bulk inserts. Query
//! functions take a plain `&Connection` so they compose and test easily; use
//! [`ReadPool::run`] (async, `spawn_blocking`) or [`ReadPool::with`] (sync)
//! to borrow one.

use std::collections::HashMap;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use flux_core::api::{
    ArtRef, AssetInfo, FolderEntry, FolderPage, FolderRef, Home, HomeRow, Id, ItemDetail, ItemFilter,
    ItemKind, ItemSummary, Library, Page, Progress, RootStatus, ScanState, ScanStatus,
};
pub use flux_core::api::{FolderSort, ItemSort};
use flux_core::{Container, HdrFormat, ProbeResult};
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OpenFlags, OptionalExtension};

use crate::error::{CatalogError, Result};
use crate::group::sort_title;
use crate::model::{
    enum_from_text, item_kind_str, parse_art_source, parse_item_kind, parse_library_kind, parse_role,
    AssetLocation, CachedPlayIndex, UserRow,
};

// ---- pool --------------------------------------------------------------------

struct PoolInner {
    path: PathBuf,
    max: usize,
    cache_kib: i64,
    mmap_bytes: i64,
    state: Mutex<PoolState>,
    available: Condvar,
}

struct PoolState {
    idle: Vec<Connection>,
    open: usize,
}

/// A bounded pool of read-only connections. Cheap to clone.
#[derive(Clone)]
pub struct ReadPool {
    inner: Arc<PoolInner>,
}

/// A connection borrowed from the pool; returned on drop.
pub struct PooledConn {
    conn: Option<Connection>,
    pool: Arc<PoolInner>,
}

impl Deref for PooledConn {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        // Only `None` inside `drop`.
        self.conn.as_ref().expect("pooled connection present until drop")
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            let mut st = lock(&self.pool.state);
            st.idle.push(c);
            drop(st);
            self.pool.available.notify_one();
        }
    }
}

fn lock(m: &Mutex<PoolState>) -> MutexGuard<'_, PoolState> {
    // A panic while holding the lock cannot leave PoolState inconsistent.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ReadPool {
    /// A pool of up to `size` connections, opened lazily.
    pub fn new(path: impl Into<PathBuf>, size: usize, cache_kib: i64, mmap_bytes: i64) -> ReadPool {
        ReadPool {
            inner: Arc::new(PoolInner {
                path: path.into(),
                max: size.max(1),
                cache_kib,
                mmap_bytes,
                state: Mutex::new(PoolState {
                    idle: Vec::new(),
                    open: 0,
                }),
                available: Condvar::new(),
            }),
        }
    }

    fn open_conn(&self) -> Result<Connection> {
        let conn = Connection::open_with_flags(
            &self.inner.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI,
        )?;
        crate::schema::configure_reader(&conn, self.inner.cache_kib, self.inner.mmap_bytes)?;
        Ok(conn)
    }

    /// Borrow a connection, blocking while all are in use.
    pub fn get(&self) -> Result<PooledConn> {
        let mut st = lock(&self.inner.state);
        loop {
            if let Some(c) = st.idle.pop() {
                return Ok(PooledConn {
                    conn: Some(c),
                    pool: self.inner.clone(),
                });
            }
            if st.open < self.inner.max {
                st.open += 1;
                drop(st);
                return match self.open_conn() {
                    Ok(c) => Ok(PooledConn {
                        conn: Some(c),
                        pool: self.inner.clone(),
                    }),
                    Err(e) => {
                        lock(&self.inner.state).open -= 1;
                        self.inner.available.notify_one();
                        Err(e)
                    }
                };
            }
            st = self.inner.available.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Run a read on the calling thread.
    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let c = self.get()?;
        f(&c)
    }

    /// Run a read on tokio's blocking pool (never blocks a runtime worker).
    pub async fn run<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let pool = self.clone();
        tokio::task::spawn_blocking(move || pool.with(f))
            .await
            .map_err(|e| CatalogError::ReadTask(e.to_string()))?
    }
}

// ---- libraries -----------------------------------------------------------------

/// A scan status with nothing going on.
pub fn idle_status() -> ScanStatus {
    ScanStatus {
        state: ScanState::Idle,
        files_seen: 0,
        files_total: None,
        files_probed: 0,
        items_added: 0,
        items_updated: 0,
        items_removed: 0,
        started_at: None,
        finished_at: None,
        last_error: None,
        dirs_unavailable: 0,
    }
}

/// How many present files of a library an older flux cataloged: parsed by
/// an older flux-names (`parse_version < PARSER_VERSION`) or probed by an
/// older flux-probe. After an upgrade the server brings them up to date
/// with a Deep scan; a Quick scan leaves unchanged files alone. Missing,
/// unreachable and failed files don't count (a Deep scan can't reach the
/// first two, and failed probes have their own retries), so this drops to
/// 0 once that scan ran.
pub fn outdated_files(c: &Connection, library_id: Id) -> Result<u64> {
    let n: i64 = c
        .prepare_cached(
            "SELECT COUNT(*) FROM file f JOIN root r ON r.id = f.root_id
              WHERE r.library_id = ?1 AND f.state = 'present'
                AND (f.parse_version < ?2 OR f.probe_version < ?3)",
        )?
        .query_row(
            params![library_id, flux_core::PARSER_VERSION, flux_core::PROBE_VERSION],
            |r| r.get(0),
        )?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// All libraries with their roots (and whether each is reachable), item
/// counts and stored scan status. Live scan progress is the server's (in
/// memory, from flux-scan); it overlays it on this, including
/// `files_total` while walking and `dirs_unavailable`.
///
/// This counts every file of each library; [`list_libraries_cached`] (or
/// [`crate::Catalog::list_libraries`]) serves the counts from a cache.
pub fn list_libraries(c: &Connection) -> Result<Vec<Library>> {
    list_libraries_with(c, None)
}

/// [`list_libraries`] with each library's file counts served from `counts`
/// until the writer commits a change to `file` rows. For `GET /libraries`,
/// the most-called endpoint, whose uncached cost grows with the library.
pub fn list_libraries_cached(c: &Connection, counts: &FileCounts) -> Result<Vec<Library>> {
    list_libraries_with(c, Some(counts))
}

fn list_libraries_with(c: &Connection, counts: Option<&FileCounts>) -> Result<Vec<Library>> {
    let libs: Vec<(Id, String, String)> = {
        let mut stmt = c.prepare_cached("SELECT id, name, kind FROM library ORDER BY name, id")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    libs.into_iter()
        .map(|(id, name, kind)| library_row(c, id, name, &kind, counts))
        .collect()
}

/// One library.
pub fn library(c: &Connection, id: Id) -> Result<Option<Library>> {
    library_with(c, id, None)
}

/// [`library`] with cached file counts (see [`list_libraries_cached`]).
pub fn library_cached(c: &Connection, id: Id, counts: &FileCounts) -> Result<Option<Library>> {
    library_with(c, id, Some(counts))
}

fn library_with(c: &Connection, id: Id, counts: Option<&FileCounts>) -> Result<Option<Library>> {
    let row: Option<(String, String)> = c
        .prepare_cached("SELECT name, kind FROM library WHERE id = ?1")?
        .query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    row.map(|(name, kind)| library_row(c, id, name, &kind, counts))
        .transpose()
}

/// Per-library counts for the library listing (files, probed files,
/// visible top-level items), cached until the writer commits anything that
/// may change `file` or `item` rows (it calls [`FileCounts::invalidate`]
/// after each such commit). Counting is linear in the library (≈4–6 ms per
/// call at 15k files), and `GET /libraries` is the endpoint clients call
/// most; at rest the cache answers.
#[derive(Debug, Default)]
pub struct FileCounts {
    generation: std::sync::atomic::AtomicU64,
    /// library → (generation, files, probed files, top-level items)
    map: Mutex<HashMap<Id, (u64, i64, i64, i64)>>,
}

impl FileCounts {
    /// Forget every cached count (the writer calls this after a commit).
    pub fn invalidate(&self) {
        self.generation.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    fn get(&self, c: &Connection, library_id: Id) -> Result<(i64, i64, i64)> {
        // Read the generation before counting: a commit racing the count
        // bumps it afterwards, so a stale count is never kept as current.
        let generation = self.generation.load(std::sync::atomic::Ordering::Acquire);
        {
            let map = self.map.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(&(g, seen, probed, items)) = map.get(&library_id) {
                if g == generation {
                    return Ok((seen, probed, items));
                }
            }
        }
        let (seen, probed, items) = count_library(c, library_id)?;
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(library_id, (generation, seen, probed, items));
        Ok((seen, probed, items))
    }
}

/// `(files, probed files, visible top-level items)` of a library. A file
/// that is not media (`probe_failed` and placed nowhere) doesn't count.
fn count_library(c: &Connection, library_id: Id) -> Result<(i64, i64, i64)> {
    let (seen, probed): (i64, i64) = c
        .prepare_cached(
            "SELECT count(*), count(f.probe_version) FROM file f JOIN root r ON r.id = f.root_id
             WHERE r.library_id = ?1 AND f.state <> 'missing'
               AND NOT (f.state = 'probe_failed' AND f.placement = 'ignored')",
        )?
        .query_row([library_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let items: i64 = c
        .prepare_cached(
            "SELECT count(*) FROM item WHERE library_id = ?1 AND parent_id IS NULL AND state <> 'missing'",
        )?
        .query_row([library_id], |r| r.get(0))?;
    Ok((seen, probed, items))
}

fn library_row(
    c: &Connection,
    id: Id,
    name: String,
    kind: &str,
    counts: Option<&FileCounts>,
) -> Result<Library> {
    let mut status = idle_status();
    let mut roots = Vec::new();
    {
        let mut stmt = c.prepare_cached(
            "SELECT path, state, last_scan_started_at, last_scan_finished_at, last_scan_error, state_changed_at
             FROM root WHERE library_id = ?1 ORDER BY id",
        )?;
        let mut rows = stmt.query([id])?;
        while let Some(r) = rows.next()? {
            let available = r.get::<_, String>(1)? != "unavailable";
            let finished: Option<i64> = r.get(3)?;
            let changed: Option<i64> = r.get(5)?;
            roots.push(RootStatus {
                path: r.get(0)?,
                available,
                // Confirmed reachable at the end of a scan, or when it last
                // came back (`state_changed_at` of an available root). For an
                // unavailable root that stamp is when it went away instead.
                last_seen: if available {
                    finished.max(changed)
                } else {
                    finished
                },
            });
            if !available {
                status.state = ScanState::Unavailable;
            }
            status.started_at = status.started_at.max(r.get(2)?);
            status.finished_at = status.finished_at.max(r.get(3)?);
            if let Some(e) = r.get::<_, Option<String>>(4)? {
                status.last_error = Some(e);
                if status.state == ScanState::Idle {
                    status.state = ScanState::Error;
                }
            }
        }
    }
    let (seen, probed, item_count) = match counts {
        Some(cache) => cache.get(c, id)?,
        None => count_library(c, id)?,
    };
    status.files_seen = seen.max(0) as u64;
    // At rest, the total is what the catalog tracks.
    status.files_total = Some(status.files_seen);
    status.files_probed = probed.max(0) as u64;
    Ok(Library {
        id,
        name,
        kind: parse_library_kind(kind)?,
        roots,
        item_count: item_count.max(0) as u64,
        scan: status,
    })
}

/// Parse a stored JSON column, treating unreadable values as empty: one bad
/// row must not break a page (it is logged).
fn lenient<T: serde::de::DeserializeOwned + Default>(json: Option<String>) -> T {
    match json.as_deref().map(serde_json::from_str) {
        Some(Ok(v)) => v,
        Some(Err(e)) => {
            tracing::warn!(error = %e, "unreadable JSON column ignored");
            T::default()
        }
        None => T::default(),
    }
}

// ---- summaries -------------------------------------------------------------------

const SUMMARY_COLS: &str = "i.id, i.kind, i.title, i.year, i.season_number, i.episode_number, i.air_date,
    i.parent_id, i.badges, i.added_at,
    CASE WHEN i.kind IN ('show','season') THEN
        (SELECT count(*) FROM item c WHERE c.parent_id = i.id AND c.kind <> 'extra' AND c.state <> 'missing')
    END, i.state, i.episode_count";
/// Number of columns in [`SUMMARY_COLS`]; extra columns of a query start here.
const SUMMARY_N: usize = 13;

struct SummaryRow {
    id: Id,
    kind: String,
    title: String,
    year: Option<u16>,
    season: Option<u16>,
    episode: Option<u16>,
    air_date: Option<String>,
    parent_id: Option<Id>,
    badges: Option<String>,
    added_at: i64,
    child_count: Option<i64>,
    /// Derived `item.state`: `available`, `unavailable` or `missing`.
    state: String,
    /// Seasons and shows: visible episodes below (writer-maintained).
    episode_count: Option<i64>,
}

fn summary_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SummaryRow> {
    Ok(SummaryRow {
        id: r.get(0)?,
        kind: r.get(1)?,
        title: r.get(2)?,
        year: r.get(3)?,
        season: r.get(4)?,
        episode: r.get(5)?,
        air_date: r.get(6)?,
        parent_id: r.get(7)?,
        badges: r.get(8)?,
        added_at: r.get(9)?,
        child_count: r.get(10)?,
        state: r.get(11)?,
        episode_count: r.get(12)?,
    })
}

fn episode_code(season: Option<u16>, episode: Option<u16>, air_date: Option<&str>) -> Option<String> {
    match (season, episode, air_date) {
        (Some(s), Some(e), _) => Some(format!("S{s:02}E{e:02}")),
        (_, _, Some(d)) => Some(d.to_string()),
        _ => None,
    }
}

fn parent_of(c: &Connection, id: Id) -> Result<Option<Id>> {
    Ok(c.prepare_cached("SELECT parent_id FROM item WHERE id = ?1")?
        .query_row([id], |r| r.get(0))
        .optional()?
        .flatten())
}

/// `EXISTS`-able subquery over the visible episodes `e` of item `i` (a show:
/// through its seasons; a season: directly), followed by `cond` on `e`.
/// Written as two index walks (`item_children`) rather than an `OR`, which
/// SQLite would turn into a scan of every item.
fn episodes_of_i(kind_is_show: bool, cond: &str) -> String {
    if kind_is_show {
        format!(
            "SELECT 1 FROM item s JOIN item e ON e.parent_id = s.id
              WHERE s.parent_id = i.id AND s.kind = 'season' AND e.kind = 'episode' AND e.state <> 'missing'
                AND {cond}"
        )
    } else {
        format!("SELECT 1 FROM item e WHERE e.parent_id = i.id AND e.kind = 'episode' AND e.state <> 'missing' AND {cond}")
    }
}

/// The order that picks an item's image of a kind: the user's pin, then
/// provider > local > frame > generated, then the newest.
const ART_ORDER: &str = "pinned DESC,
    CASE source WHEN 'provider' THEN 0 WHEN 'local' THEN 1 WHEN 'frame' THEN 2 ELSE 3 END, id DESC";

fn art_ref(c: &Connection, item_id: Id, kind: &str) -> Result<Option<ArtRef>> {
    let mut stmt = c.prepare_cached(&format!(
        "SELECT id, width, height, blurhash, source, attribution, generator FROM art WHERE item_id = ?1 AND kind = ?2
         ORDER BY {ART_ORDER} LIMIT 1"
    ))?;
    let mut rows = stmt.query(params![item_id, kind])?;
    let Some(r) = rows.next()? else { return Ok(None) };
    let id: Id = r.get(0)?;
    Ok(Some(ArtRef {
        url: format!("/api/v1/art/{id}"),
        width: r.get(1)?,
        height: r.get(2)?,
        blurhash: r.get(3)?,
        source: parse_art_source(&r.get::<_, String>(4)?)?,
        attribution: r.get(5)?,
        generator: r.get(6)?,
    }))
}

/// A user's progress on an item.
pub fn progress(c: &Connection, user_id: Id, item_id: Id) -> Result<Option<Progress>> {
    Ok(c
        .prepare_cached(
            "SELECT position_ms, duration_ms, watched, updated_at FROM user_state WHERE user_id = ?1 AND item_id = ?2",
        )?
        .query_row(params![user_id, item_id], |r| {
            Ok(Progress {
                position_ms: r.get::<_, i64>(0)?.max(0) as u64,
                duration_ms: r.get::<_, i64>(1)?.max(0) as u64,
                watched: r.get(2)?,
                updated_at: r.get(3)?,
            })
        })
        .optional()?)
}

/// Above this many episodes under a page's shows/seasons, unwatched counts
/// come from one pass over the user's finished episodes instead of a state
/// lookup per episode.
const PROBE_LIMIT: i64 = 3_000;

/// One bound parameter for any number of ids: a JSON array read with
/// `json_each` (`IN (SELECT value FROM json_each(?))`).
fn id_list<'a>(ids: impl IntoIterator<Item = &'a Id>) -> String {
    let mut s = String::from("[");
    for (i, id) in ids.into_iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&id.to_string());
    }
    s.push(']');
    s
}

/// `(kind, title, parent_id)` of the given items into `out`.
fn load_heads(
    c: &Connection,
    ids: &std::collections::BTreeSet<Id>,
    out: &mut HashMap<Id, (String, String, Option<Id>)>,
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut stmt = c.prepare_cached(
        "SELECT id, kind, title, parent_id FROM item WHERE id IN (SELECT value FROM json_each(?1))",
    )?;
    let mut rows = stmt.query([id_list(ids)])?;
    while let Some(r) = rows.next()? {
        out.insert(r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?));
    }
    Ok(())
}

/// The summaries of a list of items, the same in every list: own title, the
/// show (or parent work) in `parent_title`, "S01E03" / season name in
/// `subtitle`. Built with a handful of set queries for the whole list
/// (ancestors, art, progress, unwatched counts) instead of several point
/// queries per item.
fn summaries(c: &Connection, rows: Vec<SummaryRow>, user: Option<Id>) -> Result<Vec<ItemSummary>> {
    summaries_with(c, rows, user, PROBE_LIMIT)
}

fn summaries_with(
    c: &Connection,
    rows: Vec<SummaryRow>,
    user: Option<Id>,
    probe_limit: i64,
) -> Result<Vec<ItemSummary>> {
    use std::collections::BTreeSet;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    // Parents and grandparents (season → show) for titles and art fallback.
    let mut heads: HashMap<Id, (String, String, Option<Id>)> = HashMap::new();
    let parents: BTreeSet<Id> = rows.iter().filter_map(|r| r.parent_id).collect();
    load_heads(c, &parents, &mut heads)?;
    let grand: BTreeSet<Id> = parents
        .iter()
        .filter_map(|p| heads.get(p).and_then(|h| h.2))
        .filter(|g| !heads.contains_key(g))
        .collect();
    load_heads(c, &grand, &mut heads)?;

    // Best image per (item, kind) for the rows and their ancestors.
    let mut art: HashMap<(Id, String), ArtRef> = HashMap::new();
    {
        let ids: BTreeSet<Id> = rows
            .iter()
            .map(|r| r.id)
            .chain(parents.iter().copied())
            .chain(grand.iter().copied())
            .collect();
        let mut stmt = c.prepare_cached(&format!(
            "SELECT item_id, kind, id, width, height, blurhash, source, attribution, generator FROM art
              WHERE item_id IN (SELECT value FROM json_each(?1)) AND kind IN ('poster','thumb','backdrop')
              ORDER BY item_id, kind, {ART_ORDER}"
        ))?;
        let mut q = stmt.query([id_list(&ids)])?;
        while let Some(r) = q.next()? {
            let key: (Id, String) = (r.get(0)?, r.get(1)?);
            if art.contains_key(&key) {
                continue;
            }
            let id: Id = r.get(2)?;
            art.insert(
                key,
                ArtRef {
                    url: format!("/api/v1/art/{id}"),
                    width: r.get(3)?,
                    height: r.get(4)?,
                    blurhash: r.get(5)?,
                    source: parse_art_source(&r.get::<_, String>(6)?)?,
                    attribution: r.get(7)?,
                    generator: r.get(8)?,
                },
            );
        }
    }
    // Own art of `kinds` (in order), else the nearest ancestor's of the last kind.
    let pick = |id: Id, parent: Option<Id>, kinds: &[&str]| -> Option<ArtRef> {
        for k in kinds {
            if let Some(a) = art.get(&(id, k.to_string())) {
                return Some(a.clone());
            }
        }
        let fallback = kinds.last()?.to_string();
        let mut cur = parent;
        for _ in 0..2 {
            let p = cur?;
            if let Some(a) = art.get(&(p, fallback.clone())) {
                return Some(a.clone());
            }
            cur = heads.get(&p).and_then(|h| h.2);
        }
        None
    };

    // The user's progress on every row.
    let mut progress_of: HashMap<Id, Progress> = HashMap::new();
    if let Some(u) = user {
        let mut stmt = c.prepare_cached(
            "SELECT item_id, position_ms, duration_ms, watched, updated_at FROM user_state
              WHERE user_id = ?1 AND item_id IN (SELECT value FROM json_each(?2))",
        )?;
        let mut q = stmt.query(params![u, id_list(rows.iter().map(|r| &r.id))])?;
        while let Some(r) = q.next()? {
            progress_of.insert(
                r.get(0)?,
                Progress {
                    position_ms: r.get::<_, i64>(1)?.max(0) as u64,
                    duration_ms: r.get::<_, i64>(2)?.max(0) as u64,
                    watched: r.get(3)?,
                    updated_at: r.get(4)?,
                },
            );
        }
    }

    // Unwatched episodes per show and season (for this user): the
    // writer-maintained `episode_count` minus what the user finished.
    // Few episodes on the page: probe each episode's state; many (a page
    // of long shows): one pass over the user's own finished episodes.
    let mut unwatched: HashMap<Id, i64> = HashMap::new();
    let containers: Vec<&SummaryRow> = rows
        .iter()
        .filter(|r| r.kind == "show" || r.kind == "season")
        .collect();
    let counted = containers.iter().all(|r| r.episode_count.is_some());
    let page_episodes: i64 = containers.iter().filter_map(|r| r.episode_count).sum();
    if containers.is_empty() {
    } else if counted && user.is_none() {
        for r in &containers {
            unwatched.insert(r.id, r.episode_count.unwrap_or(0));
        }
    } else if counted && page_episodes > probe_limit {
        let mut finished: HashMap<Id, i64> = HashMap::new();
        let mut stmt = c.prepare_cached(
            "SELECT s.parent_id, e.parent_id, count(*)
               FROM user_state us CROSS JOIN item e CROSS JOIN item s
              WHERE us.user_id = ?1 AND us.watched = 1 AND e.id = us.item_id AND e.kind = 'episode'
                AND e.state <> 'missing' AND s.id = e.parent_id AND s.kind = 'season'
              GROUP BY s.parent_id, e.parent_id",
        )?;
        let mut q = stmt.query([user])?;
        while let Some(r) = q.next()? {
            let (show, season, n): (Option<Id>, Id, i64) = (r.get(0)?, r.get(1)?, r.get(2)?);
            *finished.entry(season).or_default() += n;
            if let Some(show) = show {
                *finished.entry(show).or_default() += n;
            }
        }
        for r in &containers {
            let total = r.episode_count.unwrap_or(0);
            unwatched.insert(r.id, (total - finished.get(&r.id).copied().unwrap_or(0)).max(0));
        }
    } else {
        let not_watched = "(?2 IS NULL OR NOT EXISTS
            (SELECT 1 FROM user_state us WHERE us.user_id = ?2 AND us.item_id = e.id AND us.watched = 1))";
        for (kind, sql) in [
            (
                "show",
                format!(
                    "SELECT s.parent_id, count(*) FROM item s JOIN item e ON e.parent_id = s.id
                      WHERE s.parent_id IN (SELECT value FROM json_each(?1)) AND s.kind = 'season'
                        AND e.kind = 'episode' AND e.state <> 'missing' AND {not_watched}
                      GROUP BY s.parent_id"
                ),
            ),
            (
                "season",
                format!(
                    "SELECT e.parent_id, count(*) FROM item e
                      WHERE e.parent_id IN (SELECT value FROM json_each(?1))
                        AND e.kind = 'episode' AND e.state <> 'missing' AND {not_watched}
                      GROUP BY e.parent_id"
                ),
            ),
        ] {
            let ids: Vec<&Id> = containers
                .iter()
                .filter(|r| r.kind == kind)
                .map(|r| &r.id)
                .collect();
            if ids.is_empty() {
                continue;
            }
            let mut stmt = c.prepare_cached(&sql)?;
            let mut q = stmt.query(params![id_list(ids), user])?;
            while let Some(r) = q.next()? {
                unwatched.insert(r.get(0)?, r.get(1)?);
            }
        }
    }

    let title_of = |id: Option<Id>| id.and_then(|p| heads.get(&p)).map(|h| h.1.clone());
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let kind = parse_item_kind(&row.kind)?;
        let (subtitle, parent_title) = match kind {
            ItemKind::Episode => {
                // The parent is a season (title of its show) or the show.
                let show = row.parent_id.and_then(|p| match heads.get(&p) {
                    Some((k, _, gp)) if k == "season" => *gp,
                    Some(_) => Some(p),
                    None => None,
                });
                (
                    episode_code(row.season, row.episode, row.air_date.as_deref()),
                    title_of(show),
                )
            }
            ItemKind::Season => (Some(row.title.clone()), title_of(row.parent_id)),
            ItemKind::Extra => (None, title_of(row.parent_id)),
            ItemKind::Movie | ItemKind::Show => (None, None),
        };
        let unwatched_count = match kind {
            ItemKind::Show | ItemKind::Season => Some(
                unwatched
                    .get(&row.id)
                    .copied()
                    .unwrap_or(0)
                    .clamp(0, i64::from(u32::MAX)) as u32,
            ),
            _ => None,
        };
        let poster_kinds: &[&str] = if kind == ItemKind::Episode {
            &["thumb", "poster"]
        } else {
            &["poster"]
        };
        let badges: Vec<String> = match row.badges {
            Some(b) => lenient(Some(b)),
            None => Vec::new(),
        };
        out.push(ItemSummary {
            id: row.id,
            kind,
            title: row.title,
            parent_title,
            year: row.year,
            subtitle,
            poster: pick(row.id, row.parent_id, poster_kinds),
            backdrop: pick(row.id, row.parent_id, &["backdrop"]),
            progress: progress_of.remove(&row.id),
            badges,
            child_count: row.child_count.map(|n| n.clamp(0, u32::MAX as i64) as u32),
            unwatched_count,
            available: row.state == "available",
            added_at: row.added_at,
        });
    }
    Ok(out)
}

fn to_summary(c: &Connection, row: SummaryRow, user: Option<Id>) -> Result<ItemSummary> {
    summaries(c, vec![row], user)?
        .pop()
        .ok_or_else(|| CatalogError::Corrupt("summary vanished".into()))
}

/// One item's summary.
pub fn item_summary(c: &Connection, id: Id, user: Option<Id>) -> Result<Option<ItemSummary>> {
    let row = c
        .prepare_cached(&format!("SELECT {SUMMARY_COLS} FROM item i WHERE i.id = ?1"))?
        .query_row([id], summary_row)
        .optional()?;
    row.map(|r| to_summary(c, r, user)).transpose()
}

// ---- browse ------------------------------------------------------------------------

/// Parameters of [`items_page`].
#[derive(Debug, Clone, PartialEq)]
pub struct ItemsQuery {
    pub library_id: Option<Id>,
    /// Empty = movies and shows (the library's top level).
    pub kinds: Vec<ItemKind>,
    pub sort: ItemSort,
    /// Opaque cursor from a previous page's `next`.
    pub cursor: Option<String>,
    /// Page size, clamped to 1..=500.
    pub limit: u32,
    /// Fills `progress` and `unwatched_count` for this user, and is the
    /// user that `Unwatched` / `InProgress` filters refer to.
    pub user_id: Option<Id>,
    /// `None` = everything. Shows match through their episodes.
    pub filter: Option<ItemFilter>,
}

impl Default for ItemsQuery {
    fn default() -> Self {
        ItemsQuery {
            library_id: None,
            kinds: Vec::new(),
            sort: ItemSort::Title,
            cursor: None,
            limit: 100,
            user_id: None,
            filter: None,
        }
    }
}

/// SQL condition (on alias `i`) for a list filter, with its arguments.
///
/// - `4k` / `hdr` read the item's own badges: the writer merges episode
///   badges up into seasons and shows, so every level answers alone.
/// - `unwatched`: a leaf the user hasn't finished; a show or season with an
///   episode the user hasn't finished. Without a user, everything.
/// - `in_progress`: the set of the user's started-not-finished items and
///   their season and show, computed once per query (not per row).
fn filter_sql(f: ItemFilter, user: Option<Id>) -> (String, Vec<Value>) {
    let badge = |names: &[&str]| {
        let any = names
            .iter()
            .map(|n| format!("i.badges LIKE '%\"{n}\"%'"))
            .collect::<Vec<_>>()
            .join(" OR ");
        (format!("({any})"), Vec::new())
    };
    match f {
        ItemFilter::Uhd => badge(&["4K", "8K"]),
        ItemFilter::Hdr => badge(&["HDR10", "HDR10+", "HLG", "DV"]),
        ItemFilter::Unwatched => {
            let Some(u) = user else {
                return ("1".into(), Vec::new());
            };
            let not_watched = |x: &str| {
                format!("NOT EXISTS (SELECT 1 FROM user_state us WHERE us.user_id = ? AND us.item_id = {x}.id AND us.watched = 1)")
            };
            (
                format!(
                    "(CASE i.kind WHEN 'show' THEN EXISTS ({})
                                  WHEN 'season' THEN EXISTS ({})
                                  ELSE {} END)",
                    episodes_of_i(true, &not_watched("e")),
                    episodes_of_i(false, &not_watched("e")),
                    not_watched("i"),
                ),
                vec![u.into(), u.into(), u.into()],
            )
        }
        ItemFilter::InProgress => {
            let Some(u) = user else {
                return ("0".into(), Vec::new());
            };
            (
                "i.id IN (
                    SELECT us.item_id FROM user_state us
                     WHERE us.user_id = ? AND us.watched = 0 AND us.position_ms > 0
                    UNION SELECT e.parent_id FROM user_state us JOIN item e ON e.id = us.item_id
                     WHERE us.user_id = ? AND us.watched = 0 AND us.position_ms > 0 AND e.kind = 'episode'
                    UNION SELECT s.parent_id FROM user_state us JOIN item e ON e.id = us.item_id
                      JOIN item s ON s.id = e.parent_id
                     WHERE us.user_id = ? AND us.watched = 0 AND us.position_ms > 0 AND e.kind = 'episode')"
                    .into(),
                vec![u.into(), u.into(), u.into()],
            )
        }
    }
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<String> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect();
    String::from_utf8(bytes?).ok()
}

/// The position of the last row of a page, in the sort's own key. Ties on
/// the primary key always break by sort title, then id, so every order is
/// total and a cursor is exact.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Cursor {
    Title(String, Id),
    /// `COALESCE(year, 99999)` (ascending) or `COALESCE(year, -1)`
    /// (descending): unknown years sort last either way.
    Year(i64, String, Id),
    Added(i64, Id),
}

/// One letter per sort, so a cursor from another order is rejected.
fn cursor_tag(sort: ItemSort) -> char {
    match sort {
        ItemSort::Title => 't',
        ItemSort::TitleDesc => 'u',
        ItemSort::Year => 'y',
        ItemSort::YearDesc => 'z',
        ItemSort::Added => 'b',
        ItemSort::AddedDesc => 'a',
    }
}

fn year_key(sort: ItemSort) -> &'static str {
    match sort {
        ItemSort::YearDesc => "COALESCE(i.year, -1)",
        _ => "COALESCE(i.year, 99999)",
    }
}

fn encode_cursor(sort: ItemSort, c: &Cursor) -> String {
    let tag = cursor_tag(sort);
    match c {
        Cursor::Title(t, id) => format!("{tag}{}.{id}", hex(t)),
        Cursor::Year(y, t, id) => format!("{tag}{y}~{}.{id}", hex(t)),
        Cursor::Added(at, id) => format!("{tag}{at}.{id}"),
    }
}

fn decode_cursor(s: &str, sort: ItemSort) -> Result<Cursor> {
    let bad = || CatalogError::Invalid(format!("bad cursor {s:?}"));
    let (head, id) = s.rsplit_once('.').ok_or_else(bad)?;
    let id: Id = id.parse().map_err(|_| bad())?;
    let mut chars = head.chars();
    if chars.next() != Some(cursor_tag(sort)) {
        return Err(bad());
    }
    let body = chars.as_str();
    match sort {
        ItemSort::Title | ItemSort::TitleDesc => Ok(Cursor::Title(unhex(body).ok_or_else(bad)?, id)),
        ItemSort::Year | ItemSort::YearDesc => {
            let (y, t) = body.split_once('~').ok_or_else(bad)?;
            Ok(Cursor::Year(
                y.parse().map_err(|_| bad())?,
                unhex(t).ok_or_else(bad)?,
                id,
            ))
        }
        ItemSort::Added | ItemSort::AddedDesc => Ok(Cursor::Added(body.parse().map_err(|_| bad())?, id)),
    }
}

/// `ORDER BY` of a sort. The top-level indexes (`item_top_title`,
/// `item_top_year[_desc]`, `item_top_lib_added`) deliver exactly these.
fn order_sql(sort: ItemSort) -> String {
    match sort {
        ItemSort::Title => "i.sort_title, i.id".into(),
        ItemSort::TitleDesc => "i.sort_title DESC, i.id DESC".into(),
        ItemSort::Year => format!("{}, i.sort_title, i.id", year_key(sort)),
        ItemSort::YearDesc => format!("{} DESC, i.sort_title, i.id", year_key(sort)),
        ItemSort::Added => "i.content_added_at, i.id".into(),
        ItemSort::AddedDesc => "i.content_added_at DESC, i.id DESC".into(),
    }
}

/// The keyset condition "after `cur`" for a sort, with its arguments.
fn after_sql(sort: ItemSort, cur: Cursor) -> Result<(String, Vec<Value>)> {
    Ok(match (sort, cur) {
        (ItemSort::Title, Cursor::Title(t, id)) => {
            ("(i.sort_title, i.id) > (?, ?)".into(), vec![t.into(), id.into()])
        }
        (ItemSort::TitleDesc, Cursor::Title(t, id)) => {
            ("(i.sort_title, i.id) < (?, ?)".into(), vec![t.into(), id.into()])
        }
        (ItemSort::Year, Cursor::Year(y, t, id)) => (
            format!("({}, i.sort_title, i.id) > (?, ?, ?)", year_key(sort)),
            vec![y.into(), t.into(), id.into()],
        ),
        (ItemSort::YearDesc, Cursor::Year(y, t, id)) => {
            let k = year_key(sort);
            (
                format!("({k} < ? OR ({k} = ? AND (i.sort_title, i.id) > (?, ?)))"),
                vec![y.into(), y.into(), t.into(), id.into()],
            )
        }
        (ItemSort::Added, Cursor::Added(at, id)) => (
            "(i.content_added_at, i.id) > (?, ?)".into(),
            vec![at.into(), id.into()],
        ),
        (ItemSort::AddedDesc, Cursor::Added(at, id)) => (
            "(i.content_added_at, i.id) < (?, ?)".into(),
            vec![at.into(), id.into()],
        ),
        _ => return Err(CatalogError::Invalid("cursor does not match the sort".into())),
    })
}

/// A page of items with keyset (cursor) pagination: stable under concurrent
/// inserts, O(page) regardless of depth. Every sort of `api::ItemSort` is
/// supported; `total` counts the whole filtered set on every page.
pub fn items_page(c: &Connection, q: &ItemsQuery) -> Result<Page<ItemSummary>> {
    let kinds = if q.kinds.is_empty() {
        vec![ItemKind::Movie, ItemKind::Show]
    } else {
        q.kinds.clone()
    };
    let limit = q.limit.clamp(1, 500);
    let kind_list = kinds
        .iter()
        .map(|k| format!("'{}'", item_kind_str(*k)))
        .collect::<Vec<_>>()
        .join(",");
    let mut filter = String::new();
    let mut args: Vec<Value> = Vec::new();
    if let Some(lib) = q.library_id {
        // First, so the per-library sort indexes apply.
        filter.push_str("i.library_id = ? AND ");
        args.push(lib.into());
    }
    filter.push_str(&format!("i.state <> 'missing' AND i.kind IN ({kind_list})"));
    if kinds
        .iter()
        .all(|k| matches!(k, ItemKind::Movie | ItemKind::Show))
    {
        // Lets SQLite use the partial top-level indexes.
        filter.push_str(" AND i.parent_id IS NULL");
    }
    if let Some(f) = q.filter {
        let (cond, fargs) = filter_sql(f, q.user_id);
        filter.push_str(" AND ");
        filter.push_str(&cond);
        args.extend(fargs);
    }
    let total: i64 = c
        .prepare_cached(&format!("SELECT count(*) FROM item i WHERE {filter}"))?
        .query_row(params_from_iter(args.iter()), |r| r.get(0))?;
    let mut page_filter = filter;
    if let Some(cur) = &q.cursor {
        let (cond, cargs) = after_sql(q.sort, decode_cursor(cur, q.sort)?)?;
        page_filter.push_str(" AND ");
        page_filter.push_str(&cond);
        args.extend(cargs);
    }
    args.push(i64::from(limit + 1).into());
    let sql = format!(
        "SELECT {SUMMARY_COLS}, i.sort_title, i.content_added_at FROM item i WHERE {page_filter}
         ORDER BY {} LIMIT ?",
        order_sql(q.sort)
    );
    let mut rows: Vec<(SummaryRow, String, i64)> = {
        let mut stmt = c.prepare_cached(&sql)?;
        let it = stmt.query_map(params_from_iter(args.iter()), |r| {
            Ok((summary_row(r)?, r.get(SUMMARY_N)?, r.get(SUMMARY_N + 1)?))
        })?;
        it.collect::<rusqlite::Result<_>>()?
    };
    let next = if rows.len() > limit as usize {
        rows.truncate(limit as usize);
        rows.last().map(|(r, sort, added)| {
            let cur = match q.sort {
                ItemSort::Title | ItemSort::TitleDesc => Cursor::Title(sort.clone(), r.id),
                ItemSort::Year => Cursor::Year(r.year.map_or(99999, i64::from), sort.clone(), r.id),
                ItemSort::YearDesc => Cursor::Year(r.year.map_or(-1, i64::from), sort.clone(), r.id),
                ItemSort::Added | ItemSort::AddedDesc => Cursor::Added(*added, r.id),
            };
            encode_cursor(q.sort, &cur)
        })
    } else {
        None
    };
    let items = summaries(c, rows.into_iter().map(|(r, _, _)| r).collect(), q.user_id)?;
    Ok(Page {
        items,
        next,
        total: Some(total.max(0) as u64),
    })
}

/// The SQL plan of an [`items_page`] query (diagnostics and tests: every
/// sort of a library's top level must walk an index, not sort).
pub fn items_page_plan(c: &Connection, q: &ItemsQuery) -> Result<Vec<String>> {
    let mut filter = String::from(
        "i.library_id = ?1 AND i.state <> 'missing' AND i.kind IN ('movie','show') AND i.parent_id IS NULL",
    );
    if let Some(f) = q.filter {
        let (cond, _) = filter_sql(f, q.user_id);
        filter.push_str(" AND ");
        filter.push_str(&cond);
    }
    let sql = format!(
        "EXPLAIN QUERY PLAN SELECT i.id FROM item i WHERE {filter} ORDER BY {} LIMIT 101",
        order_sql(q.sort)
    );
    let mut stmt = c.prepare(&sql)?;
    let n = stmt.parameter_count();
    let args: Vec<Value> = (0..n).map(|_| Value::from(q.library_id.unwrap_or(1))).collect();
    let rows = stmt.query_map(params_from_iter(args.iter()), |r| r.get::<_, String>(3))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

// ---- folders -------------------------------------------------------------------------

/// Parameters of [`folder_page`].
#[derive(Debug, Clone, PartialEq)]
pub struct FolderQuery {
    pub library_id: Id,
    /// A [`FolderRef::folder`]: `""` is the library's top, `"{root id}"` one
    /// of its folders (roots), `"{root id}/{dir}"` a folder inside one.
    pub folder: String,
    pub sort: FolderSort,
    /// Opaque cursor from a previous page's `next`.
    pub cursor: Option<String>,
    /// Page size, clamped to 1..=500.
    pub limit: u32,
    /// Fills `progress` for this user.
    pub user_id: Option<Id>,
}

impl Default for FolderQuery {
    fn default() -> Self {
        FolderQuery {
            library_id: 0,
            folder: String::new(),
            sort: FolderSort::Title,
            cursor: None,
            limit: 100,
            user_id: None,
        }
    }
}

/// The last part of a root's path, its name in the folder view
/// (`D:\Home Videos` → "Home Videos", `/srv/media/` → "media").
fn root_name(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    match trimmed.rsplit(['/', '\\']).next() {
        Some(last) if !last.is_empty() => last.to_string(),
        _ => path.to_string(),
    }
}

/// Visible top-level items with a file in `(root, dir)`, each with its sort
/// title and the earliest modification time of its files there.
/// `dir = ""` is the root itself.
fn folder_items(c: &Connection, library: Id, root: Id, dir: &str) -> Result<Vec<(Id, String, i64)>> {
    const SELECT: &str = "SELECT i.id, i.sort_title, MIN(f.mtime_ns) FROM file f
          JOIN asset a ON a.file_id = f.id JOIN item i ON i.id = a.item_id";
    const VISIBLE: &str =
        "f.state <> 'missing' AND i.state <> 'missing' AND i.parent_id IS NULL AND i.library_id = ?2";
    let mut out = Vec::new();
    if dir.is_empty() {
        let mut stmt = c.prepare_cached(&format!(
            "{SELECT} WHERE f.root_id = ?1 AND instr(f.rel_path, '/') = 0 AND {VISIBLE} GROUP BY i.id"
        ))?;
        let rows = stmt.query_map(params![root, library], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        for r in rows {
            out.push(r?);
        }
    } else {
        // Range scan on UNIQUE(root_id, rel_path): '0' is the byte after '/';
        // `substr` counts characters.
        let (lo, hi) = (format!("{dir}/"), format!("{dir}0"));
        let mut stmt = c.prepare_cached(&format!(
            "{SELECT} WHERE f.root_id = ?1 AND f.rel_path >= ?3 AND f.rel_path < ?4
               AND instr(substr(f.rel_path, ?5), '/') = 0 AND {VISIBLE} GROUP BY i.id"
        ))?;
        let rows = stmt.query_map(
            params![root, library, lo, hi, lo.chars().count() as i64 + 1],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        for r in rows {
            out.push(r?);
        }
    }
    Ok(out)
}

/// The subfolders of `(root, dir)` that hold visible top-level items, with
/// the number of items in each (and below it).
fn subfolders(c: &Connection, library: Id, root: Id, dir: &str) -> Result<Vec<(String, u64)>> {
    let mut seen: HashMap<String, std::collections::HashSet<Id>> = HashMap::new();
    let mut add = |rest: &str, item: Id| {
        if let Some((name, _)) = rest.split_once('/') {
            seen.entry(name.to_string()).or_default().insert(item);
        }
    };
    if dir.is_empty() {
        let mut stmt = c.prepare_cached(
            "SELECT a.item_id, f.rel_path FROM file f JOIN asset a ON a.file_id = f.id JOIN item i ON i.id = a.item_id
              WHERE f.root_id = ?1 AND instr(f.rel_path, '/') > 0
                AND f.state <> 'missing' AND i.state <> 'missing' AND i.parent_id IS NULL AND i.library_id = ?2",
        )?;
        let mut rows = stmt.query(params![root, library])?;
        while let Some(r) = rows.next()? {
            add(&r.get::<_, String>(1)?, r.get(0)?);
        }
    } else {
        let (lo, hi) = (format!("{dir}/"), format!("{dir}0"));
        let mut stmt = c.prepare_cached(
            "SELECT a.item_id, substr(f.rel_path, ?5) FROM file f JOIN asset a ON a.file_id = f.id
               JOIN item i ON i.id = a.item_id
              WHERE f.root_id = ?1 AND f.rel_path >= ?3 AND f.rel_path < ?4 AND instr(substr(f.rel_path, ?5), '/') > 0
                AND f.state <> 'missing' AND i.state <> 'missing' AND i.parent_id IS NULL AND i.library_id = ?2",
        )?;
        let mut rows = stmt.query(params![root, library, lo, hi, lo.chars().count() as i64 + 1])?;
        while let Some(r) = rows.next()? {
            add(&r.get::<_, String>(1)?, r.get(0)?);
        }
    }
    let mut out: Vec<(String, u64)> = seen
        .into_iter()
        .map(|(name, items)| (name, items.len() as u64))
        .collect();
    out.sort_by(|a, b| {
        sort_title(&a.0)
            .cmp(&sort_title(&b.0))
            .then_with(|| a.0.cmp(&b.0))
    });
    Ok(out)
}

fn folder_cursor_tag(sort: FolderSort) -> char {
    match sort {
        FolderSort::Title => 't',
        FolderSort::TitleDesc => 'u',
        FolderSort::Date => 'd',
        FolderSort::DateDesc => 'e',
    }
}

/// One folder of a videos library (`GET /libraries/{id}/folders`): where
/// it is, its subfolders, and a page of the videos with a file directly in
/// it. Folders come from the files' paths, so a folder is listed while it
/// holds a visible video somewhere below it. `None` when the library or the
/// folder isn't there (the folder of another library, a folder that holds
/// nothing). [`CatalogError::Invalid`] for a library of another kind: only
/// in a videos library is every file a top-level item of its own (in a
/// shows library every file is an episode, and a movie library's extras
/// belong to their films), so elsewhere a folder page would leave most of
/// its files out.
pub fn folder_page(c: &Connection, q: &FolderQuery) -> Result<Option<FolderPage>> {
    let Some((name, kind)) = c
        .prepare_cached("SELECT name, kind FROM library WHERE id = ?1")?
        .query_row([q.library_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .optional()?
    else {
        return Ok(None);
    };
    if kind != flux_core::LibraryKind::Videos.as_str() {
        return Err(CatalogError::Invalid(format!(
            "a {kind} library has no folder view: folders are browsed in a videos library"
        )));
    }
    let roots: Vec<(Id, String)> = {
        let mut stmt = c.prepare_cached("SELECT id, path FROM root WHERE library_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map([q.library_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let top = FolderRef {
        folder: String::new(),
        name,
    };
    // Where we are: the top (several roots), or a root and a folder in it.
    let place: Option<(Id, String)> = match q.folder.trim_matches('/') {
        "" if roots.len() == 1 => Some((roots[0].0, String::new())),
        "" => None,
        f => {
            let (root, dir) = f.split_once('/').unwrap_or((f, ""));
            let root: Id = root
                .parse()
                .map_err(|_| CatalogError::Invalid(format!("bad folder {:?}", q.folder)))?;
            if !roots.iter().any(|(id, _)| *id == root) {
                return Ok(None);
            }
            Some((root, dir.to_string()))
        }
    };
    let token = |root: Id, dir: &str| {
        if dir.is_empty() {
            root.to_string()
        } else {
            format!("{root}/{dir}")
        }
    };
    let Some((root, dir)) = place else {
        // The top of a library with several roots: one folder per root.
        let mut folders = Vec::new();
        for (id, path) in &roots {
            let n: i64 = c
                .prepare_cached(
                    "SELECT count(DISTINCT i.id) FROM file f JOIN asset a ON a.file_id = f.id
                       JOIN item i ON i.id = a.item_id
                      WHERE f.root_id = ?1 AND f.state <> 'missing' AND i.state <> 'missing'
                        AND i.parent_id IS NULL AND i.library_id = ?2",
                )?
                .query_row(params![id, q.library_id], |r| r.get(0))?;
            if n > 0 {
                folders.push(FolderEntry {
                    folder: id.to_string(),
                    name: root_name(path),
                    item_count: n as u64,
                });
            }
        }
        return Ok(Some(FolderPage {
            at: top,
            parents: Vec::new(),
            folders,
            items: Vec::new(),
            next: None,
            total: 0,
        }));
    };
    let mut rows = folder_items(c, q.library_id, root, &dir)?;
    let folders: Vec<FolderEntry> = subfolders(c, q.library_id, root, &dir)?
        .into_iter()
        .map(|(n, count)| FolderEntry {
            folder: token(
                root,
                &if dir.is_empty() {
                    n.clone()
                } else {
                    format!("{dir}/{n}")
                },
            ),
            name: n,
            item_count: count,
        })
        .collect();
    if !dir.is_empty() && rows.is_empty() && folders.is_empty() {
        return Ok(None);
    }
    // The way down: the top, the root (when the library has several), then
    // every folder above this one.
    let single = roots.len() == 1;
    let mut parents = Vec::new();
    let mut at = top.clone();
    if !(single && dir.is_empty()) {
        parents.push(top);
        let root_path = roots
            .iter()
            .find(|(id, _)| *id == root)
            .map(|(_, p)| p.as_str())
            .unwrap_or("");
        let mut chain = vec![FolderRef {
            folder: token(root, ""),
            name: root_name(root_path),
        }];
        if !dir.is_empty() {
            let mut sofar = String::new();
            for part in dir.split('/') {
                if !sofar.is_empty() {
                    sofar.push('/');
                }
                sofar.push_str(part);
                chain.push(FolderRef {
                    folder: token(root, &sofar),
                    name: part.to_string(),
                });
            }
        }
        if single {
            // With one root, the top is that root: skip it.
            chain.remove(0);
        }
        at = chain.pop().unwrap_or(at);
        parents.extend(chain);
    }
    // Order, then the page after the cursor.
    let desc = matches!(q.sort, FolderSort::TitleDesc | FolderSort::DateDesc);
    match q.sort {
        FolderSort::Title | FolderSort::TitleDesc => rows.sort_by(|a, b| (&a.1, a.0).cmp(&(&b.1, b.0))),
        FolderSort::Date | FolderSort::DateDesc => {
            rows.sort_by(|a, b| (a.2, &a.1, a.0).cmp(&(b.2, &b.1, b.0)))
        }
    }
    if desc {
        rows.reverse();
    }
    // A row's position in the order: (mtime or 0, sort title, id).
    let key_of = |r: &(Id, String, i64)| match q.sort {
        FolderSort::Title | FolderSort::TitleDesc => (0, r.1.clone(), r.0),
        FolderSort::Date | FolderSort::DateDesc => (r.2, r.1.clone(), r.0),
    };
    // The page starts after the cursor's row (or where it was: a cursor
    // stays valid when its row goes away).
    let start = match &q.cursor {
        None => 0,
        Some(cur) => {
            let after = parse_folder_cursor(cur, q.sort)
                .ok_or_else(|| CatalogError::Invalid(format!("bad cursor {cur:?}")))?;
            rows.iter()
                .position(|r| {
                    if desc {
                        key_of(r) < after
                    } else {
                        key_of(r) > after
                    }
                })
                .unwrap_or(rows.len())
        }
    };
    let limit = q.limit.clamp(1, 500) as usize;
    let total = rows.len() as u64;
    let page: Vec<&(Id, String, i64)> = rows.iter().skip(start).take(limit).collect();
    let next = match page.last() {
        Some(last) if start + page.len() < rows.len() => {
            let (t, title, id) = key_of(last);
            let tag = folder_cursor_tag(q.sort);
            Some(match q.sort {
                FolderSort::Title | FolderSort::TitleDesc => format!("{tag}{}.{id}", hex(&title)),
                FolderSort::Date | FolderSort::DateDesc => format!("{tag}{t}~{}.{id}", hex(&title)),
            })
        }
        _ => None,
    };
    let ids: Vec<Id> = page.iter().map(|r| r.0).collect();
    let mut by_id: HashMap<Id, SummaryRow> = HashMap::new();
    {
        let mut stmt = c.prepare_cached(&format!(
            "SELECT {SUMMARY_COLS} FROM item i WHERE i.id IN (SELECT value FROM json_each(?1))"
        ))?;
        let it = stmt.query_map([id_list(&ids)], summary_row)?;
        for r in it {
            let r = r?;
            by_id.insert(r.id, r);
        }
    }
    let ordered: Vec<SummaryRow> = ids.iter().filter_map(|id| by_id.remove(id)).collect();
    Ok(Some(FolderPage {
        at,
        parents,
        folders,
        items: summaries(c, ordered, q.user_id)?,
        next,
        total,
    }))
}

/// `(mtime or 0, sort title, id)` of a folder cursor.
fn parse_folder_cursor(s: &str, sort: FolderSort) -> Option<(i64, String, Id)> {
    let (head, id) = s.rsplit_once('.')?;
    let id: Id = id.parse().ok()?;
    let body = head.strip_prefix(folder_cursor_tag(sort))?;
    match sort {
        FolderSort::Title | FolderSort::TitleDesc => Some((0, unhex(body)?, id)),
        FolderSort::Date | FolderSort::DateDesc => {
            let (t, title) = body.split_once('~')?;
            Some((t.parse().ok()?, unhex(title)?, id))
        }
    }
}

/// Seasons of a show (Specials last), episodes of a season, extras of a
/// movie. Hidden (missing) children are omitted.
pub fn children(c: &Connection, id: Id, user: Option<Id>) -> Result<Vec<ItemSummary>> {
    let rows: Vec<SummaryRow> = {
        let mut stmt = c.prepare_cached(&format!(
            "SELECT {SUMMARY_COLS} FROM item i WHERE i.parent_id = ?1 AND i.state <> 'missing'
             ORDER BY i.kind = 'extra', i.season_number = 0, i.season_number, i.episode_number, i.air_date,
                      i.sort_title, i.id"
        ))?;
        let it = stmt.query_map([id], summary_row)?;
        it.collect::<rusqlite::Result<_>>()?
    };
    summaries(c, rows, user)
}

// ---- detail --------------------------------------------------------------------------

/// A file's absolute path: its root and its `/`-separated relative path,
/// joined with the root's own separator (`\` for a Windows root, also
/// inside `rel`, so a path never mixes the two).
pub(crate) fn path_join(root: &str, rel: &str) -> String {
    if crate::writer::is_windows_path(root) {
        let root = root.trim_end_matches(['/', '\\']).replace('/', "\\");
        format!("{root}\\{}", rel.replace('/', "\\"))
    } else if root.ends_with('/') {
        format!("{root}{rel}")
    } else {
        format!("{root}/{rel}")
    }
}

/// Playable assets of an item, best first within each edition: editionless
/// first, then by edition, part, height. Tombstoned (missing) files are omitted.
pub fn item_assets(c: &Connection, item_id: Id) -> Result<Vec<AssetInfo>> {
    let mut stmt = c.prepare_cached(
        "SELECT a.id, r.path, f.rel_path, f.size, a.container, a.duration_ms, a.edition, a.label, a.hdr,
                f.state, r.state, m.probe
         FROM asset_item ai JOIN asset a ON a.id = ai.asset_id JOIN file f ON f.id = a.file_id
         JOIN root r ON r.id = f.root_id LEFT JOIN file_meta m ON m.file_id = f.id
         WHERE ai.item_id = ?1 AND f.state <> 'missing'
         ORDER BY a.edition IS NOT NULL, a.edition, a.part, a.height DESC, a.id",
    )?;
    let mut rows = stmt.query([item_id])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let container: Option<String> = r.get(4)?;
        let hdr: Option<String> = r.get(8)?;
        let fstate: String = r.get(9)?;
        let rstate: String = r.get(10)?;
        let probe: Option<String> = r.get(11)?;
        out.push(AssetInfo {
            id: r.get(0)?,
            path: path_join(&r.get::<_, String>(1)?, &r.get::<_, String>(2)?),
            size: r.get::<_, i64>(3)?.max(0) as u64,
            container: container
                .as_deref()
                .and_then(enum_from_text)
                .unwrap_or(Container::Unknown),
            duration_ms: r.get::<_, Option<i64>>(5)?.map(|d| d.max(0) as u64),
            edition: r.get(6)?,
            label: r.get(7)?,
            hdr: hdr.as_deref().and_then(enum_from_text::<HdrFormat>),
            available: fstate != "unavailable" && rstate == "available",
            probe: probe.and_then(|p| serde_json::from_str::<ProbeResult>(&p).ok()),
            subtitle_files: Vec::new(),
        });
    }
    drop(rows);
    for a in &mut out {
        a.subtitle_files = crate::sidecar::asset_subtitle_files(c, a.id)?;
    }
    Ok(out)
}

/// Everything the detail page needs: summary, overview, ids, match
/// evidence, and every asset with its probe.
pub fn item_detail(c: &Connection, id: Id, user: Option<Id>) -> Result<Option<ItemDetail>> {
    type Extra = (
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<f64>,
        Option<String>,
    );
    let row: Option<(SummaryRow, Extra)> = c
        .prepare_cached(&format!(
            "SELECT {SUMMARY_COLS}, i.overview, i.genres, i.tmdb_id, i.tvdb_id, i.imdb_id, i.external_ids,
                    i.match_confidence, i.match_evidence
             FROM item i WHERE i.id = ?1"
        ))?
        .query_row([id], |r| {
            Ok((
                summary_row(r)?,
                (
                    r.get(SUMMARY_N)?,
                    r.get(SUMMARY_N + 1)?,
                    r.get(SUMMARY_N + 2)?,
                    r.get(SUMMARY_N + 3)?,
                    r.get(SUMMARY_N + 4)?,
                    r.get(SUMMARY_N + 5)?,
                    r.get(SUMMARY_N + 6)?,
                    r.get(SUMMARY_N + 7)?,
                ),
            ))
        })
        .optional()?;
    let Some((row, (overview, genres, tmdb, tvdb, imdb, others, confidence, evidence))) = row else {
        return Ok(None);
    };
    let (parent, season_number, episode) = (row.parent_id, row.season, row.episode);
    let summary = to_summary(c, row, user)?;
    let show_id = match summary.kind {
        ItemKind::Season => parent,
        ItemKind::Episode => parent.map(|p| parent_of(c, p)).transpose()?.flatten(),
        _ => None,
    };
    let assets = item_assets(c, id)?;
    // A multi-episode file covers several episode numbers.
    let mut episode_numbers: Vec<u16> = if summary.kind == ItemKind::Episode {
        let mut stmt = c.prepare_cached(
            "SELECT DISTINCT e.episode_number FROM asset_item ai
               JOIN asset_item ai2 ON ai2.asset_id = ai.asset_id
               JOIN item e ON e.id = ai2.item_id
              WHERE ai.item_id = ?1 AND e.kind = 'episode' AND e.episode_number IS NOT NULL
              ORDER BY 1",
        )?;
        let it = stmt.query_map([id], |r| r.get(0))?;
        it.collect::<rusqlite::Result<_>>()?
    } else {
        Vec::new()
    };
    if episode_numbers.is_empty() {
        episode_numbers.extend(episode);
    }
    // Runtime: the parts of the first version add up; otherwise its duration.
    let runtime_ms = assets.first().and_then(|first| {
        let parts: Vec<&AssetInfo> = assets.iter().filter(|a| a.edition == first.edition).collect();
        let summed: u64 = parts.iter().filter_map(|a| a.duration_ms).sum();
        let multipart = parts.len() > 1 && is_multipart(c, id).unwrap_or(false);
        if multipart {
            Some(summed)
        } else {
            first.duration_ms
        }
    });
    let mut external_ids = Vec::new();
    if let Some(v) = tmdb {
        external_ids.push(("tmdb".to_string(), v.to_string()));
    }
    if let Some(v) = tvdb {
        external_ids.push(("tvdb".to_string(), v.to_string()));
    }
    if let Some(v) = imdb {
        external_ids.push(("imdb".to_string(), v));
    }
    if let Some(o) = others {
        let map: std::collections::BTreeMap<String, String> = lenient(Some(o));
        external_ids.extend(map);
    }
    Ok(Some(ItemDetail {
        summary,
        overview,
        genres: lenient(genres),
        runtime_ms,
        assets,
        show_id,
        season_number,
        episode_numbers,
        external_ids,
        match_confidence: confidence.map(|c| c as f32),
        match_evidence: lenient(evidence),
    }))
}

fn is_multipart(c: &Connection, item_id: Id) -> Result<bool> {
    Ok(c
        .prepare_cached(
            "SELECT count(*) FROM asset_item ai JOIN asset a ON a.id = ai.asset_id WHERE ai.item_id = ?1 AND a.part IS NOT NULL",
        )?
        .query_row([item_id], |r| r.get::<_, i64>(0))?
        > 1)
}

// ---- search ---------------------------------------------------------------------------

/// Turn user input into a safe FTS5 query: every alphanumeric run becomes a
/// quoted prefix term (`"matr"*`), all terms required. Returns `None` when
/// nothing searchable remains.
pub fn fts_query(input: &str) -> Option<String> {
    let terms: Vec<String> = input
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .take(16)
        .map(|t| format!("\"{}\"*", t.to_lowercase()))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

/// How many FTS5 hits (best bm25 first) the final ranking considers.
const SEARCH_CANDIDATES: u32 = 300;

/// Prefix search over titles. Two phases: FTS5 picks the best
/// `SEARCH_CANDIDATES` (300) by its weighted bm25 `rank` (title ≫ original title ≫
/// overview), computed inside FTS5; then those are re-ranked: exact title,
/// then title prefix, then movies/shows before episodes, then bm25. Joining
/// and sorting *every* match instead cost p99 > 100 ms for broad prefixes
/// over 200k items.
pub fn search(c: &Connection, q: &str, limit: u32, user: Option<Id>) -> Result<Vec<ItemSummary>> {
    let Some(fts) = fts_query(q) else {
        return Ok(Vec::new());
    };
    let prefix = sort_title(q);
    let rows: Vec<SummaryRow> = {
        let mut stmt = c.prepare_cached(&format!(
            "WITH hits AS (
                 SELECT rowid AS id, rank FROM item_fts WHERE item_fts MATCH ?1 ORDER BY rank LIMIT ?5
             )
             SELECT {SUMMARY_COLS} FROM hits JOIN item i ON i.id = hits.id
             WHERE i.state <> 'missing'
             ORDER BY lower(i.title) = lower(?2) DESC,
                      substr(i.sort_title, 1, length(?3)) = ?3 DESC,
                      i.kind <> 'movie' AND i.kind <> 'show',
                      hits.rank,
                      i.id
             LIMIT ?4"
        ))?;
        let it = stmt.query_map(
            params![fts, q.trim(), prefix, limit.clamp(1, 200), SEARCH_CANDIDATES],
            summary_row,
        )?;
        it.collect::<rusqlite::Result<_>>()?
    };
    summaries(c, rows, user)
}

// ---- home --------------------------------------------------------------------------------

/// How many of a user's most recent state rows "continue watching" looks
/// at (one row per show/movie survives, so this bounds the work per request).
const CONTINUE_SCAN: u32 = 500;

/// The sort key of an episode within its show: specials apart, then season,
/// then number, then air date (date-based shows).
const EPISODE_ORDER: &str = "printf('%05d|%05d|%s|%012d', s.season_number, COALESCE(e.episode_number, 0), COALESCE(e.air_date, ''), e.id)";

/// The next episode of `show` after the one with order key `after` that
/// `user` hasn't finished and that isn't hidden. Specials (season 0) only
/// follow specials.
fn next_episode(c: &Connection, user: Id, show: Id, after: &str, specials: bool) -> Result<Option<Id>> {
    Ok(c
        .prepare_cached(&format!(
            "SELECT e.id FROM item s JOIN item e ON e.parent_id = s.id
              WHERE s.parent_id = ?1 AND s.kind = 'season' AND e.kind = 'episode' AND e.state <> 'missing'
                AND (s.season_number = 0) = ?4 AND {EPISODE_ORDER} > ?2
                AND NOT EXISTS (SELECT 1 FROM user_state us WHERE us.user_id = ?3 AND us.item_id = e.id AND us.watched = 1)
              ORDER BY {EPISODE_ORDER} LIMIT 1"
        ))?
        .query_row(params![show, after, user, specials], |r| r.get(0))
        .optional()?)
}

/// "Continue watching" for a user, most recent activity first, one entry
/// per movie or show:
///
/// - a movie or episode with a resume point (including a rewatch);
/// - for a show whose latest activity was *finishing* an episode, the next
///   episode the user hasn't finished ("next up": after S1E3, S1E4; after a
///   season's last episode, the next season's first), if there is one.
///
/// Finished movies and finished shows drop out. Hidden (missing) items are
/// skipped; unavailable ones are listed (the card greys out).
pub fn continue_watching(c: &Connection, user: Id, limit: u32) -> Result<Vec<ItemSummary>> {
    struct Row {
        item: Id,
        position: i64,
        watched: bool,
        kind: String,
        state: String,
        show: Option<Id>,
        order: Option<String>,
        special: bool,
    }
    let rows: Vec<Row> = {
        let mut stmt = c.prepare_cached(&format!(
            "SELECT us.item_id, us.position_ms, us.watched, e.kind, e.state, s.parent_id,
                    CASE WHEN e.kind = 'episode' THEN {EPISODE_ORDER} END, COALESCE(s.season_number = 0, 0)
               FROM user_state us JOIN item e ON e.id = us.item_id
               LEFT JOIN item s ON s.id = e.parent_id AND e.kind = 'episode'
              WHERE us.user_id = ?1 AND (us.position_ms > 0 OR us.watched = 1)
              ORDER BY us.updated_at DESC, us.item_id DESC LIMIT ?2"
        ))?;
        let it = stmt.query_map(params![user, CONTINUE_SCAN], |r| {
            Ok(Row {
                item: r.get(0)?,
                position: r.get(1)?,
                watched: r.get(2)?,
                kind: r.get(3)?,
                state: r.get(4)?,
                show: r.get(5)?,
                order: r.get(6)?,
                special: r.get(7)?,
            })
        })?;
        it.collect::<rusqlite::Result<_>>()?
    };
    let mut seen = std::collections::HashSet::new();
    let mut picked: Vec<Id> = Vec::new();
    for row in rows {
        if picked.len() >= limit as usize {
            break;
        }
        let group = match (&row.show, row.kind.as_str()) {
            (Some(show), "episode") => *show,
            _ => row.item,
        };
        if !seen.insert(group) {
            continue; // an older entry of a show or movie already decided
        }
        if row.position > 0 {
            if row.state != "missing" {
                picked.push(row.item);
            }
        } else if row.watched && row.kind == "episode" {
            if let (Some(show), Some(order)) = (row.show, row.order.as_deref()) {
                if let Some(next) = next_episode(c, user, show, order, row.special)? {
                    picked.push(next);
                }
            }
        }
    }
    let mut out = Vec::with_capacity(picked.len());
    for id in picked {
        if let Some(s) = item_summary(c, id, Some(user))? {
            out.push(s);
        }
    }
    Ok(out)
}

/// Continue watching (for `user`, see [`continue_watching`]), recently
/// added, and a recently-added row per library.
pub fn home(c: &Connection, user: Option<Id>, limit: u32) -> Result<Home> {
    let limit = limit.clamp(1, 100);
    let continue_watching = match user {
        Some(u) => continue_watching(c, u, limit)?,
        None => Vec::new(),
    };
    // Per library through `item_top_lib_added`; the all-libraries row is
    // the merge of those (no global index to maintain on every insert).
    let libs: Vec<(Id, String)> = {
        let mut stmt = c.prepare_cached("SELECT id, name FROM library ORDER BY name, id")?;
        let it = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        it.collect::<rusqlite::Result<_>>()?
    };
    /// `(content_added_at, id)` of a library's newest works.
    type Newest = Vec<(i64, Id)>;
    let mut per_lib: Vec<(Id, String, Newest)> = Vec::new();
    let mut summary_of: HashMap<Id, ItemSummary> = HashMap::new();
    for (lib, name) in libs {
        let rows: Vec<(SummaryRow, i64)> = {
            let mut stmt = c.prepare_cached(&format!(
                "SELECT {SUMMARY_COLS}, i.content_added_at FROM item i
                 WHERE i.library_id = ?1 AND i.parent_id IS NULL AND i.state <> 'missing'
                   AND i.kind IN ('movie','show')
                 ORDER BY i.content_added_at DESC, i.id DESC LIMIT ?2"
            ))?;
            let it = stmt.query_map(params![lib, limit], |r| Ok((summary_row(r)?, r.get(SUMMARY_N)?)))?;
            it.collect::<rusqlite::Result<_>>()?
        };
        let keys: Vec<(i64, Id)> = rows.iter().map(|(r, at)| (*at, r.id)).collect();
        for s in summaries(c, rows.into_iter().map(|(r, _)| r).collect(), user)? {
            summary_of.insert(s.id, s);
        }
        per_lib.push((lib, name, keys));
    }
    let mut all: Vec<(i64, Id)> = per_lib.iter().flat_map(|(_, _, k)| k.iter().copied()).collect();
    all.sort_unstable_by(|a, b| b.cmp(a));
    all.truncate(limit as usize);
    let recently_added = all
        .iter()
        .filter_map(|(_, id)| summary_of.get(id).cloned())
        .collect();
    let rows = per_lib
        .into_iter()
        .filter(|(_, _, keys)| !keys.is_empty())
        .map(|(lib, name, keys)| HomeRow {
            title: format!("Recently added in {name}"),
            library_id: Some(lib),
            items: keys
                .iter()
                .filter_map(|(_, id)| summary_of.get(id).cloned())
                .collect(),
        })
        .collect();
    Ok(Home {
        continue_watching,
        recently_added,
        rows,
    })
}

// ---- users, playback, art -----------------------------------------------------------------

/// Look up a user by tailnet login name.
pub fn user_by_login(c: &Connection, login: &str) -> Result<Option<UserRow>> {
    let row: Option<(Id, String, String, String)> = c
        .prepare_cached("SELECT id, login_name, display_name, role FROM user WHERE login_name = ?1")?
        .query_row([login], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .optional()?;
    row.map(|(id, login_name, display_name, role)| {
        Ok(UserRow {
            id,
            login_name,
            display_name,
            role: parse_role(&role)?,
        })
    })
    .transpose()
}

/// Where an asset's file is and whether it can be opened now.
pub fn asset_location(c: &Connection, asset_id: Id) -> Result<Option<AssetLocation>> {
    let mut stmt = c.prepare_cached(
        "SELECT a.id, a.item_id, f.id, f.root_id, r.path, f.rel_path, f.size, f.state, r.state,
                a.container, a.duration_ms, a.hdr, m.probe, f.dev, f.ino, f.mtime_ns, f.probe_error
         FROM asset a JOIN file f ON f.id = a.file_id JOIN root r ON r.id = f.root_id
         LEFT JOIN file_meta m ON m.file_id = f.id WHERE a.id = ?1",
    )?;
    let mut rows = stmt.query([asset_id])?;
    let Some(r) = rows.next()? else { return Ok(None) };
    let fstate: String = r.get(7)?;
    let rstate: String = r.get(8)?;
    let container: Option<String> = r.get(9)?;
    let hdr: Option<String> = r.get(11)?;
    let probe: Option<String> = r.get(12)?;
    let root_path: String = r.get(4)?;
    Ok(Some(AssetLocation {
        asset_id: r.get(0)?,
        item_id: r.get(1)?,
        file_id: r.get(2)?,
        root_id: r.get(3)?,
        path: path_join(&root_path, &r.get::<_, String>(5)?),
        root_path,
        dev: r.get::<_, Option<i64>>(13)?.map(crate::model::u64_from_sql),
        ino: r.get::<_, Option<i64>>(14)?.map(crate::model::u64_from_sql),
        mtime_ns: r.get(15)?,
        probe_error: r.get(16)?,
        size: r.get::<_, i64>(6)?.max(0) as u64,
        available: matches!(fstate.as_str(), "present" | "probe_failed") && rstate == "available",
        container: container.as_deref().and_then(enum_from_text),
        duration_ms: r.get::<_, Option<i64>>(10)?.map(|d| d.max(0) as u64),
        hdr: hdr.as_deref().and_then(enum_from_text),
        probe: probe.and_then(|p| serde_json::from_str(&p).ok()),
    }))
}

/// The cached playback index of an asset, if one is stored **and** still
/// matches the file's size and mtime, with the file's current path (see
/// [`CachedPlayIndex`]). `None` means "build it" (then
/// [`crate::Writer::put_asset_index`]).
pub fn asset_play_index(c: &Connection, asset_id: Id) -> Result<Option<CachedPlayIndex>> {
    let mut stmt = c.prepare_cached(
        "SELECT x.format, x.version, x.bytes, r.path, f.rel_path, f.size, f.mtime_ns, f.state, r.state
           FROM asset_index x JOIN asset a ON a.id = x.asset_id JOIN file f ON f.id = a.file_id
           JOIN root r ON r.id = f.root_id
          WHERE x.asset_id = ?1 AND x.source_size = f.size AND x.source_mtime_ns = f.mtime_ns",
    )?;
    let mut rows = stmt.query([asset_id])?;
    let Some(r) = rows.next()? else { return Ok(None) };
    let fstate: String = r.get(7)?;
    let rstate: String = r.get(8)?;
    Ok(Some(CachedPlayIndex {
        asset_id,
        format: r.get(0)?,
        version: u16::try_from(r.get::<_, i64>(1)?).unwrap_or(0),
        bytes: r.get(2)?,
        path: path_join(&r.get::<_, String>(3)?, &r.get::<_, String>(4)?),
        size: r.get::<_, i64>(5)?.max(0) as u64,
        mtime_ns: r.get(6)?,
        available: matches!(fstate.as_str(), "present" | "probe_failed") && rstate == "available",
    }))
}

/// An artwork row, for serving `/api/v1/art/{id}` and the art picker.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtRow {
    pub id: Id,
    pub item_id: Id,
    pub kind: String,
    pub source: flux_core::api::ArtSource,
    pub path: Option<String>,
    pub url: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub blurhash: Option<String>,
    pub attribution: Option<String>,
    pub generator: Option<String>,
    /// The user chose this image.
    pub pinned: bool,
    /// The art version that made a frame or generated image (`None`:
    /// provider or local art, or made before versioning).
    pub gen_version: Option<u32>,
    /// The caption typeset into a frame or generated poster.
    pub gen_title: Option<String>,
}

const ART_COLS: &str = concat!(
    "id, item_id, kind, source, path, url, width, height, blurhash, attribution, generator, pinned, ",
    "gen_version, gen_title"
);

fn art_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(ArtRow, String)> {
    Ok((
        ArtRow {
            id: r.get(0)?,
            item_id: r.get(1)?,
            kind: r.get(2)?,
            source: flux_core::api::ArtSource::Provider,
            path: r.get(4)?,
            url: r.get(5)?,
            width: r.get(6)?,
            height: r.get(7)?,
            blurhash: r.get(8)?,
            attribution: r.get(9)?,
            generator: r.get(10)?,
            pinned: r.get(11)?,
            gen_version: r.get(12)?,
            gen_title: r.get(13)?,
        },
        r.get(3)?,
    ))
}

fn finish_art((mut row, source): (ArtRow, String)) -> Result<ArtRow> {
    row.source = parse_art_source(&source)?;
    Ok(row)
}

/// Look up an artwork row.
pub fn art(c: &Connection, art_id: Id) -> Result<Option<ArtRow>> {
    c.prepare_cached(&format!("SELECT {ART_COLS} FROM art WHERE id = ?1"))?
        .query_row([art_id], art_row)
        .optional()?
        .map(finish_art)
        .transpose()
}

/// Every image row's id and stored path: what the server's art and cache
/// folders may hold files for.
pub fn art_paths(c: &Connection) -> Result<Vec<(Id, Option<String>)>> {
    let mut stmt = c.prepare_cached("SELECT id, path FROM art")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Every image of an item, best first per kind (the order [`best_art`]
/// uses): for the art picker.
pub fn item_art(c: &Connection, item_id: Id) -> Result<Vec<ArtRow>> {
    let mut stmt = c.prepare_cached(&format!(
        "SELECT {ART_COLS} FROM art WHERE item_id = ?1 ORDER BY kind, {ART_ORDER}"
    ))?;
    let rows = stmt
        .query_map([item_id], art_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().map(finish_art).collect()
}

/// The image an item shows for `kind`: the pinned one, else provider >
/// local > frame > generated, newest first within a source.
pub fn best_art(c: &Connection, item_id: Id, kind: crate::ArtKind) -> Result<Option<ArtRef>> {
    art_ref(c, item_id, kind.as_str())
}

/// The library an item belongs to (`None`: no such item).
pub fn item_library(c: &Connection, item_id: Id) -> Result<Option<Id>> {
    Ok(c.prepare_cached("SELECT library_id FROM item WHERE id = ?1")?
        .query_row([item_id], |r| r.get(0))
        .optional()?)
}

/// The kind of the library an item belongs to.
pub fn item_library_kind(c: &Connection, item_id: Id) -> Result<Option<flux_core::LibraryKind>> {
    let kind: Option<String> = c
        .prepare_cached("SELECT l.kind FROM item i JOIN library l ON l.id = i.library_id WHERE i.id = ?1")?
        .query_row([item_id], |r| r.get(0))
        .optional()?;
    kind.as_deref().map(parse_library_kind).transpose()
}

/// The available episodes of a show (directly or through its seasons) in
/// viewing order, specials last, each with whether it has an image of
/// `kind`: the art executor's view of one show (its first episode supplies
/// the show's frame poster; episodes without a thumb get one).
pub fn show_episodes_art(c: &Connection, show_id: Id, kind: crate::ArtKind) -> Result<Vec<(Id, bool)>> {
    let mut stmt = c.prepare_cached(
        "SELECT e.id, EXISTS (SELECT 1 FROM art a WHERE a.item_id = e.id AND a.kind = ?2)
           FROM item e LEFT JOIN item s ON s.id = e.parent_id AND s.kind = 'season'
          WHERE e.kind = 'episode' AND e.state = 'available' AND (e.parent_id = ?1 OR s.parent_id = ?1)
          ORDER BY COALESCE(s.season_number, e.season_number, 0) = 0,
                   COALESCE(s.season_number, e.season_number), e.episode_number, e.id",
    )?;
    let rows = stmt.query_map(params![show_id, kind.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// [`show_episodes_art`], counting only images that aren't stale for art
/// `version` ([`stale_art`]): an episode whose only thumb is out of date
/// gets a new one.
pub fn show_episodes_fresh_art(
    c: &Connection,
    show_id: Id,
    kind: crate::ArtKind,
    version: u32,
) -> Result<Vec<(Id, bool)>> {
    let mut stmt = c.prepare_cached(&format!(
        "SELECT e.id, EXISTS (SELECT 1 FROM art a JOIN item i ON i.id = a.item_id
                               WHERE a.item_id = e.id AND a.kind = :kind AND NOT ({}))
           FROM item e LEFT JOIN item s ON s.id = e.parent_id AND s.kind = 'season'
          WHERE e.kind = 'episode' AND e.state = 'available' AND (e.parent_id = :show OR s.parent_id = :show)
          ORDER BY COALESCE(s.season_number, e.season_number, 0) = 0,
                   COALESCE(s.season_number, e.season_number), e.episode_number, e.id",
        stale_sql("a", "i")
    ))?;
    let rows = stmt.query_map(
        rusqlite::named_params! { ":show": show_id, ":kind": kind.as_str(), ":version": version },
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The caption flux typesets into a frame or generated poster of an item
/// titled `title` and dated `year`: "Title (2024)", or "Title". Stored as
/// the poster's `gen_title`; a poster whose caption isn't its item's any
/// more is stale ([`stale_art`]). The SQL that finds stale art builds the
/// same string.
pub fn art_caption(title: &str, year: Option<u16>) -> String {
    match year {
        Some(y) => format!("{title} ({y})"),
        None => title.to_string(),
    }
}

/// SQL: art row `a` (an image of item `i`) is one of flux's own images that
/// is out of date for art version `:version`. See [`stale_art`].
fn stale_sql(a: &str, i: &str) -> String {
    format!(
        "{a}.source IN ('frame', 'generated') AND {a}.pinned = 0
         AND ({a}.gen_version IS NULL OR {a}.gen_version < :version
              OR ({a}.kind = 'poster' AND {a}.gen_title IS NOT ({i}.title || COALESCE(' (' || {i}.year || ')', ''))))"
    )
}

/// Whether `item_id` has an image of `kind` that isn't stale for art
/// `version` ([`stale_art`]): provider or local art, a pinned image, or one
/// of flux's own made by this version for the item's title.
pub fn has_fresh_art(c: &Connection, item_id: Id, kind: crate::ArtKind, version: u32) -> Result<bool> {
    let sql = format!(
        "SELECT EXISTS (SELECT 1 FROM art a JOIN item i ON i.id = a.item_id
                         WHERE a.item_id = :item AND a.kind = :kind AND NOT ({}))",
        stale_sql("a", "i")
    );
    Ok(c.prepare_cached(&sql)?.query_row(
        rusqlite::named_params! { ":item": item_id, ":kind": kind.as_str(), ":version": version },
        |r| r.get(0),
    )?)
}

/// One of flux's own images that an art job should make again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleArt {
    pub art_id: Id,
    pub item_id: Id,
    /// The movie or show the item belongs to (the item itself when it is
    /// top-level): whose art job makes the image again.
    pub work_id: Id,
    pub kind: String,
    /// As stored: relative to the data dir (`art/<file>`), or a legacy
    /// absolute path.
    pub path: Option<String>,
}

/// flux's own images (frame posters, backdrops and thumbs; generated art)
/// that should be made again:
///
/// - made by an art version older than `version`, or before art was
///   versioned (the fonts without CJK glyphs, HDR frames without tone
///   mapping…);
/// - posters typeset with a caption ([`art_caption`]) other than their
///   item's title and year now: a re-parse, a regroup or a match renamed
///   the item, or a merge brought another work's poster along.
///
/// Provider and local art, and images the user pinned, are never stale.
/// `work`: only that movie or show and everything below it; `None`: the
/// whole catalog.
pub fn stale_art(c: &Connection, version: u32, work: Option<Id>) -> Result<Vec<StaleArt>> {
    stale_art_where(c, version, work, "")
}

/// The [`stale_art`] of a work whose item has a fresh image of the same
/// kind now ([`has_fresh_art`]): what an art job that made new images
/// deletes.
pub fn replaced_stale_art(c: &Connection, version: u32, work: Id) -> Result<Vec<StaleArt>> {
    let fresh = format!(
        "AND EXISTS (SELECT 1 FROM art f WHERE f.item_id = a.item_id AND f.kind = a.kind AND NOT ({}))",
        stale_sql("f", "i")
    );
    stale_art_where(c, version, Some(work), &fresh)
}

fn stale_art_where(c: &Connection, version: u32, work: Option<Id>, and: &str) -> Result<Vec<StaleArt>> {
    let stale = stale_sql("a", "i");
    let sql = match work {
        Some(_) => format!(
            "WITH RECURSIVE sub(id) AS (SELECT :work UNION ALL SELECT i.id FROM item i JOIN sub ON i.parent_id = sub.id)
             SELECT a.id, a.item_id, :work, a.kind, a.path
               FROM sub JOIN item i ON i.id = sub.id JOIN art a ON a.item_id = i.id
              WHERE {stale} {and} ORDER BY a.id"
        ),
        // Items are at most two levels below their work (episode → season → show).
        None => format!(
            "SELECT a.id, a.item_id, COALESCE(g.id, p.id, i.id), a.kind, a.path
               FROM art a JOIN item i ON i.id = a.item_id
               LEFT JOIN item p ON p.id = i.parent_id LEFT JOIN item g ON g.id = p.parent_id
              WHERE {stale} {and} ORDER BY a.id"
        ),
    };
    let mut stmt = c.prepare_cached(&sql)?;
    let map = |r: &rusqlite::Row<'_>| {
        Ok(StaleArt {
            art_id: r.get(0)?,
            item_id: r.get(1)?,
            work_id: r.get(2)?,
            kind: r.get(3)?,
            path: r.get(4)?,
        })
    };
    let rows = match work {
        Some(w) => stmt
            .query_map(rusqlite::named_params! { ":work": w, ":version": version }, map)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        None => stmt
            .query_map(rusqlite::named_params! { ":version": version }, map)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
    };
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_query_is_injection_safe() {
        assert_eq!(fts_query("The Matr").as_deref(), Some("\"the\"* \"matr\"*"));
        assert_eq!(fts_query("a\" OR x:*").as_deref(), Some("\"a\"* \"or\"* \"x\"*"));
        assert_eq!(fts_query("  -- "), None);
        assert_eq!(fts_query("Amélie").as_deref(), Some("\"amélie\"*"));
    }

    #[test]
    fn unwatched_counts_agree_between_probing_and_the_users_finished_rows() {
        use crate::{Catalog, CatalogConfig, FileFacts, FileUpsert, ProbeUpdate, ScanBatch};
        use flux_core::{GuessKind, LibraryKind, NameGuess};
        let dir = tempfile::tempdir().unwrap();
        let cat = Catalog::open(dir.path().join("c.db"), CatalogConfig::default()).unwrap();
        let w = cat.writer();
        let lib = w
            .create_library(flux_core::api::CreateLibrary {
                name: "L".into(),
                kind: LibraryKind::Shows,
                roots: vec!["/tv".into()],
            })
            .wait()
            .unwrap();
        let root = cat
            .read_blocking(|c| Ok(crate::scan_support::list_roots(c)?[0].id))
            .unwrap();
        let mut ups = Vec::new();
        for (i, show) in ["Alpha", "Beta", "Gamma"].iter().enumerate() {
            for season in 1..=2u16 {
                for ep in 1..=(3 + i as u16) {
                    let mut g = NameGuess::unknown();
                    g.kind = GuessKind::Episode;
                    g.title = Some(show.to_string());
                    g.season = Some(season);
                    g.episodes = vec![ep];
                    let path = format!("{show}/Season {season}/{show} S{season:02}E{ep:02}.mkv");
                    ups.push(FileUpsert {
                        facts: FileFacts {
                            rel_path: path,
                            size: 1,
                            mtime_ns: 1,
                            ..Default::default()
                        },
                        probe: ProbeUpdate::Keep,
                        guess: Some(g),
                    });
                }
            }
        }
        w.apply(ScanBatch {
            root_id: root,
            upserts: ups,
            ..Default::default()
        })
        .wait()
        .unwrap();
        let mut users = Vec::new();
        for login in ["a", "b"] {
            users.push(
                w.ensure_user(flux_core::api::Identity {
                    login_name: login.into(),
                    display_name: login.into(),
                    profile_pic_url: None,
                    node_name: None,
                    role: flux_core::api::Role::Viewer,
                    tags: Vec::new(),
                })
                .wait()
                .unwrap(),
            );
        }
        let episodes: Vec<Id> = cat
            .read_blocking(|c| {
                let mut st = c.prepare("SELECT id FROM item WHERE kind = 'episode' ORDER BY id")?;
                let rows = st.query_map([], |r| r.get(0))?;
                Ok(rows.collect::<rusqlite::Result<Vec<Id>>>()?)
            })
            .unwrap();
        // a finished every third episode, b every other; b has one in progress.
        for (n, &e) in episodes.iter().enumerate() {
            if n % 3 == 0 {
                w.set_watched(users[0], e, true).wait().unwrap();
            }
            if n % 2 == 0 {
                w.set_watched(users[1], e, true).wait().unwrap();
            }
        }
        w.set_progress(users[1], episodes[1], None, 10, 1_000)
            .wait()
            .unwrap();
        let both = |user: Option<Id>| {
            cat.read_blocking(move |c| {
                let load = || -> Result<Vec<SummaryRow>> {
                    let mut st = c.prepare(&format!(
                        "SELECT {SUMMARY_COLS} FROM item i WHERE i.kind IN ('show','season') ORDER BY i.id"
                    ))?;
                    let rows = st.query_map([], summary_row)?;
                    Ok(rows.collect::<rusqlite::Result<_>>()?)
                };
                let probe = summaries_with(c, load()?, user, i64::MAX)?;
                let pass = summaries_with(c, load()?, user, 0)?;
                Ok((probe, pass))
            })
            .unwrap()
        };
        for user in [None, Some(users[0]), Some(users[1])] {
            let (probe, pass) = both(user);
            assert_eq!(probe.len(), 9, "3 shows + 6 seasons");
            let counts = |v: &[ItemSummary]| v.iter().map(|s| (s.id, s.unwatched_count)).collect::<Vec<_>>();
            assert_eq!(counts(&probe), counts(&pass), "user {user:?}");
            assert!(probe.iter().all(|s| s.unwatched_count.is_some()));
        }
        // Anonymous: every episode (3+4+5 per season, two seasons).
        let (probe, _) = both(None);
        let shows: u32 = probe
            .iter()
            .filter(|s| s.kind == ItemKind::Show)
            .filter_map(|s| s.unwatched_count)
            .sum();
        assert_eq!(shows, 24);
        drop(lib);
    }

    #[test]
    fn cursors_round_trip_per_sort_and_reject_garbage() {
        let cases = [
            (ItemSort::Title, Cursor::Title("matrix 002001 é".into(), 42)),
            (ItemSort::TitleDesc, Cursor::Title("zz".into(), 1)),
            (ItemSort::Year, Cursor::Year(99999, "alien".into(), 3)),
            (ItemSort::YearDesc, Cursor::Year(-1, "".into(), 4)),
            (ItemSort::Added, Cursor::Added(1_700_000_000_000, 7)),
            (ItemSort::AddedDesc, Cursor::Added(1_700_000_000_001, 8)),
        ];
        for (sort, c) in cases {
            let s = encode_cursor(sort, &c);
            assert_eq!(decode_cursor(&s, sort).unwrap(), c, "{s}");
            // A cursor of one order is refused by every other.
            for other in [
                ItemSort::Title,
                ItemSort::TitleDesc,
                ItemSort::Year,
                ItemSort::YearDesc,
                ItemSort::Added,
                ItemSort::AddedDesc,
            ] {
                if other != sort {
                    assert!(decode_cursor(&s, other).is_err(), "{s} as {other:?}");
                }
            }
        }
        assert!(decode_cursor("zzz", ItemSort::Title).is_err());
        assert!(decode_cursor("tzz.1", ItemSort::Title).is_err());
        assert!(decode_cursor("y12.3", ItemSort::Year).is_err());
        assert!(decode_cursor("a12.x", ItemSort::AddedDesc).is_err());
    }
}
