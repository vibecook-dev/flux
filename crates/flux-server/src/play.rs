//! Playback: `POST /api/v1/play`, `POST /api/v1/plan`, sessions, the plan
//! registry and the prepared-asset cache.
//!
//! Plans come from the **playback index**, not the probe stored at scan time
//! (docs/prototype/integration.md §4.2 #2/#4): the catalog-cached index
//! (flux-hls's binary encoding in the catalog's `asset_index` BLOB, its path
//! rewritten to the file's current location) or a fresh one, stored back.
//! `POST /plan` (a dry run) plans from the stored probe with
//! `flux_hls::plan_from_probe` unless the asset is already prepared. Plan
//! ids are a hash of (asset, file identity, recipe), so the same play
//! request maps to the same URLs, and a plan survives a restart: plans are
//! also written to `<data>/plans/<id>.json` and reloaded on a miss.
//!
//! Everything a caller can create is bounded (review SEC-4, SEC-5):
//! - the caller's numeric caps are rounded down to standard rungs before
//!   planning ([`quantize_caps`]), so one file has a small set of plans;
//! - at most [`PLAN_FILE_CAP`] plan files are kept, the least recently used
//!   going first, and a used plan's file mtime is refreshed (hourly) so the
//!   startup prune removes plans unused for two weeks, not plans made two
//!   weeks ago;
//! - at most [`SESSION_CAP`] sessions are kept (oldest first, O(1) per
//!   insert), and `GET …/diagnostics` of an unknown session id creates no
//!   state.
//!
//! Files are served only from inside their library, or when they are
//! exactly the file the scan recorded behind a link that leaves it
//! ([`location`], [`open_contained`]; review SEC-2).

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::Json;
use flux_catalog::{read, scan_support, AssetLocation, PlayIndex};
use flux_core::api::{
    AssetInfo, Id, PlanPreview, PlayRequest, PlayResponse, ProgressReport, SessionDiagnostics, SubtitleRef,
};
use flux_core::file_id::FileId;
use flux_core::{ClientCaps, DeliveryFormat, HdrFormat, PlaybackPlan, SubtitleOp};
use flux_hls::index::{INDEX_FORMAT_BINARY, INDEX_VERSION};
use flux_hls::{Asset, AudioKey, HlsError, PlanPrefs, PlaybackIndex, Rendition};
use serde::{Deserialize, Serialize};

use crate::error::{ApiError, ApiResult};
use crate::lru::Lru;
use crate::state::{AppState, Caller};

/// Plans kept in memory (each is ~1 KB; misses reload from disk).
const PLAN_CAP: usize = 512;
/// Prepared assets kept in memory, at most (and at most
/// `EngineConfig::asset_cache_bytes` of them: an index can be MBs for long MP4s).
const ASSET_CAP: usize = 64;
/// How often a cached asset's file is re-checked (size + mtime).
const RECHECK_MS: i64 = 10_000;
/// Sessions idle this long are forgotten.
const SESSION_IDLE_MS: i64 = 12 * 3600 * 1000;
/// Sessions kept at most; past it the oldest go first (a forgotten session
/// is rebuilt from its id on its next progress report).
pub const SESSION_CAP: usize = 1024;
/// Plan files not used for this long are pruned at startup.
const PLAN_FILE_MAX_AGE_S: u64 = 14 * 86_400;
/// Plan files kept at most (each takes one 4 KiB block: 16 MiB); the least
/// recently used go first.
pub const PLAN_FILE_CAP: usize = 4096;
/// A plan file's mtime records its last use, refreshed at most this often.
const PLAN_TOUCH_MS: i64 = 3600 * 1000;

/// File identity that a plan (and its cached segments) is valid for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceKey {
    pub size: u64,
    pub mtime_ns: i64,
}

impl SourceKey {
    pub fn of(asset: &Asset) -> SourceKey {
        SourceKey {
            size: asset.index.source.size,
            mtime_ns: asset.index.source.mtime_ns,
        }
    }

    /// As flux-hls's `Source` computes it.
    pub fn stat(path: &Path) -> std::io::Result<SourceKey> {
        let meta = std::fs::metadata(path)?;
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        Ok(SourceKey {
            size: meta.len(),
            mtime_ns,
        })
    }
}

/// `path` still names the file (inode, Windows file id) `asset`'s source
/// reads. A symlink re-pointed at another file with the same size and mtime
/// doesn't: the asset is then reloaded, which re-checks that the file may be
/// served (review SEC-2).
fn same_inode(path: &Path, asset: &Asset) -> bool {
    FileId::of_path(path).is_ok_and(|id| id.ino == asset.source().ino())
}

/// Server-side counters for one stream (a plan, or a file served directly).
#[derive(Default)]
pub struct StreamStats {
    segments: AtomicU64,
    bytes: AtomicU64,
    recent_ms: Mutex<VecDeque<f32>>,
}

impl StreamStats {
    /// One segment (or file response) produced in `ms`, `bytes` long.
    pub fn record(&self, ms: f32, bytes: u64) {
        self.segments.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        let mut r = self.recent_ms.lock().unwrap_or_else(|p| p.into_inner());
        if r.len() >= 256 {
            r.pop_front();
        }
        r.push_back(ms);
    }

    pub fn add_bytes(&self, bytes: u64) {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn segments(&self) -> u64 {
        self.segments.load(Ordering::Relaxed)
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// (p50, p95) of recent production times, ms.
    pub fn percentiles(&self) -> (Option<f32>, Option<f32>) {
        let mut v: Vec<f32> = self
            .recent_ms
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .copied()
            .collect();
        if v.is_empty() {
            return (None, None);
        }
        v.sort_by(|a, b| a.total_cmp(b));
        let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
        (Some(p(0.5)), Some(p(0.95)))
    }
}

/// A registered plan: what `/media/{plan_id}/…` serves.
pub struct PlanEntry {
    pub id: String,
    pub asset_id: Id,
    pub source: SourceKey,
    pub duration_ms: Option<u64>,
    pub plan: PlaybackPlan,
    pub stats: StreamStats,
}

impl PlanEntry {
    pub fn is_direct(&self) -> bool {
        self.plan.delivery == DeliveryFormat::ProgressiveFile
    }
}

/// On-disk form of a plan (`<data>/plans/<id>.json`).
#[derive(Serialize, Deserialize)]
struct PlanFile {
    asset_id: Id,
    source: SourceKey,
    duration_ms: Option<u64>,
    plan: PlaybackPlan,
}

struct CachedAsset {
    asset: Arc<Asset>,
    checked_ms: AtomicI64,
}

/// One playback session (`PlayResponse::session_id`).
pub struct Session {
    pub id: String,
    pub item_id: Id,
    pub asset_id: Id,
    pub plan_id: String,
    pub user_id: Id,
    pub path: String,
    pub duration_ms: Option<u64>,
    touched_ms: AtomicI64,
    client: Mutex<Option<ProgressReport>>,
}

/// Open sessions in creation order: bounded, O(1) per insert.
#[derive(Default)]
struct Sessions {
    map: HashMap<String, Arc<Session>>,
    order: VecDeque<String>,
}

impl Sessions {
    /// Inserts `s` (or returns the session already there under its id),
    /// then forgets the oldest sessions while there are more than
    /// [`SESSION_CAP`] or the oldest has been idle for 12 h.
    fn insert(&mut self, s: Arc<Session>, now: i64) -> Arc<Session> {
        if let Some(existing) = self.map.get(&s.id) {
            return existing.clone();
        }
        self.order.push_back(s.id.clone());
        self.map.insert(s.id.clone(), s.clone());
        while let Some(oldest) = self.order.front() {
            let idle = self
                .map
                .get(oldest)
                .is_none_or(|o| now - o.touched_ms.load(Ordering::Relaxed) >= SESSION_IDLE_MS);
            if self.map.len() <= SESSION_CAP && !idle {
                break;
            }
            if let Some(id) = self.order.pop_front() {
                self.map.remove(&id);
            }
        }
        s
    }
}

/// Last use of every plan file in `<data>/plans`.
struct PlanFiles {
    dir: PathBuf,
    cap: usize,
    used: HashMap<String, PlanUse>,
}

#[derive(Debug, Clone, Copy)]
struct PlanUse {
    /// Unix ms of the last use.
    used_ms: i64,
    /// Unix ms the file's mtime was last set to.
    touched_ms: i64,
}

impl PlanFiles {
    fn new(dir: PathBuf, cap: usize) -> PlanFiles {
        PlanFiles {
            dir,
            cap,
            used: HashMap::new(),
        }
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Plan `id`'s file exists, last used at `used_ms`, mtime `touched_ms`.
    /// Returns the files to delete to stay within the cap (least recently
    /// used first).
    fn record(&mut self, id: &str, used_ms: i64, touched_ms: i64) -> Vec<PathBuf> {
        let u = self
            .used
            .entry(id.to_string())
            .or_insert(PlanUse { used_ms, touched_ms });
        u.used_ms = u.used_ms.max(used_ms);
        let mut gone = Vec::new();
        while self.used.len() > self.cap {
            let Some(oldest) = self
                .used
                .iter()
                .min_by_key(|(_, u)| u.used_ms)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            self.used.remove(&oldest);
            gone.push(self.path(&oldest));
        }
        gone
    }

    /// Plan `id` is used now; returns its file when its mtime is due for a
    /// refresh. Unknown ids (no file) are ignored.
    fn use_now(&mut self, id: &str, now: i64) -> Option<PathBuf> {
        let u = self.used.get_mut(id)?;
        u.used_ms = now;
        if now - u.touched_ms < PLAN_TOUCH_MS {
            return None;
        }
        u.touched_ms = now;
        Some(self.path(id))
    }
}

fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64)
}

fn touch(path: &Path) {
    if let Ok(f) = File::options().write(true).open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
}

fn remove_files(paths: Vec<PathBuf>) {
    for p in paths {
        let _ = std::fs::remove_file(p);
    }
}

/// Playback state shared by the play and media routes.
pub struct PlayState {
    plan_dir: PathBuf,
    plans: Mutex<Lru<String, Arc<PlanEntry>>>,
    plan_files: Mutex<PlanFiles>,
    assets: Mutex<Lru<Id, Arc<CachedAsset>>>,
    loading: Mutex<HashMap<Id, Arc<tokio::sync::Mutex<()>>>>,
    sessions: Mutex<Sessions>,
    /// Bytes served by `/media/{asset}/file`, per asset.
    direct: Mutex<HashMap<Id, Arc<StreamStats>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl PlayState {
    /// `asset_bytes`: memory budget of prepared assets (the engine's
    /// `asset_cache_bytes`; the server, not the engine, caches them, because
    /// it layers the catalog's index cache under them).
    pub fn new(data_dir: &Path, asset_bytes: usize) -> PlayState {
        let plan_dir = data_dir.join("plans");
        let _ = std::fs::create_dir_all(&plan_dir);
        PlayState {
            plan_files: Mutex::new(PlanFiles::new(plan_dir.clone(), PLAN_FILE_CAP)),
            plan_dir,
            plans: Mutex::new(Lru::new(PLAN_CAP)),
            assets: Mutex::new(Lru::with_budget(ASSET_CAP, asset_bytes)),
            loading: Mutex::new(HashMap::new()),
            sessions: Mutex::new(Sessions::default()),
            direct: Mutex::new(HashMap::new()),
        }
    }

    /// Deletes plan files nobody used for two weeks (a used plan's mtime is
    /// refreshed) and leftovers of interrupted writes, then registers the
    /// rest and applies [`PLAN_FILE_CAP`] (blocking; run at startup).
    pub fn prune_plan_files(&self) {
        let Ok(rd) = std::fs::read_dir(&self.plan_dir) else {
            return;
        };
        let mut found = Vec::new();
        for e in rd.flatten() {
            let path = e.path();
            let name = e.file_name();
            let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
                if path.extension().is_some_and(|x| x == "tmp") {
                    let _ = std::fs::remove_file(&path);
                }
                continue;
            };
            let Ok(meta) = e.metadata() else { continue };
            let old = meta
                .modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age.as_secs() > PLAN_FILE_MAX_AGE_S);
            if old {
                let _ = std::fs::remove_file(&path);
            } else {
                found.push((id.to_string(), mtime_ms(&meta)));
            }
        }
        let gone: Vec<PathBuf> = {
            let mut files = lock(&self.plan_files);
            found
                .into_iter()
                .flat_map(|(id, ms)| files.record(&id, ms, ms))
                .collect()
        };
        remove_files(gone);
    }

    /// Plan files on disk that this process knows of.
    pub fn plan_files(&self) -> usize {
        lock(&self.plan_files).used.len()
    }

    /// A plan was used: remember when, and refresh its file's mtime hourly.
    fn plan_used(&self, id: &str) {
        let due = lock(&self.plan_files).use_now(id, flux_catalog::now_ms());
        if let Some(path) = due {
            touch(&path);
        }
    }

    /// The prepared asset for a catalog location: memory, else the
    /// catalog's cached index (path rewritten), else a fresh index (stored).
    pub async fn load_asset(&self, st: &AppState, loc: &AssetLocation) -> Result<Arc<Asset>, HlsError> {
        if let Some(a) = self.cached_asset(loc.asset_id, Some(&loc.path)) {
            return Ok(a);
        }
        let gate = lock(&self.loading).entry(loc.asset_id).or_default().clone();
        let _guard = gate.lock().await;
        if let Some(a) = self.cached_asset(loc.asset_id, Some(&loc.path)) {
            return Ok(a);
        }
        let built = build_asset(st, loc).await;
        lock(&self.loading).remove(&loc.asset_id);
        let asset = built?;
        let weight = asset.approx_bytes();
        lock(&self.assets).insert_weighted(
            loc.asset_id,
            Arc::new(CachedAsset {
                asset: asset.clone(),
                checked_ms: AtomicI64::new(flux_catalog::now_ms()),
            }),
            weight,
        );
        Ok(asset)
    }

    /// A cached asset whose file is unchanged (re-checked every 10 s). With
    /// `path`, the cached asset must be for that path.
    pub fn cached_asset(&self, asset_id: Id, path: Option<&str>) -> Option<Arc<Asset>> {
        let e = lock(&self.assets).get(&asset_id).cloned()?;
        let src = &e.asset.index.source;
        if path.is_some_and(|p| Path::new(p) != src.path) {
            return None;
        }
        let now = flux_catalog::now_ms();
        if now - e.checked_ms.load(Ordering::Relaxed) > RECHECK_MS {
            let same = SourceKey::stat(&src.path).is_ok_and(|k| k == SourceKey::of(&e.asset))
                && same_inode(&src.path, &e.asset);
            if !same {
                lock(&self.assets).remove(&asset_id);
                return None;
            }
            e.checked_ms.store(now, Ordering::Relaxed);
        }
        Some(e.asset.clone())
    }

    /// The prepared asset a plan was made for; 410 if the file changed since.
    pub async fn asset_for_plan(&self, st: &AppState, entry: &PlanEntry) -> ApiResult<Arc<Asset>> {
        let asset = match self.cached_asset(entry.asset_id, None) {
            Some(a) => a,
            None => {
                let loc = location(st, entry.asset_id).await?;
                self.load_asset(st, &loc).await?
            }
        };
        if SourceKey::of(&asset) != entry.source {
            return Err(ApiError::gone(
                "The file changed since playback started; start it again",
            ));
        }
        Ok(asset)
    }

    /// Registers a plan (memory + disk). An identical recipe has the same
    /// id, so it reuses the plan and its file.
    fn register(&self, entry: PlanEntry) -> Arc<PlanEntry> {
        let known = lock(&self.plans).get(&entry.id).cloned();
        if let Some(existing) = known {
            self.plan_used(&existing.id);
            return existing;
        }
        let now = flux_catalog::now_ms();
        let file = self.plan_dir.join(format!("{}.json", entry.id));
        match std::fs::metadata(&file) {
            Ok(meta) => {
                // Written before a restart.
                let gone = lock(&self.plan_files).record(&entry.id, now, mtime_ms(&meta));
                remove_files(gone);
                self.plan_used(&entry.id);
            }
            Err(_) => {
                let body = PlanFile {
                    asset_id: entry.asset_id,
                    source: entry.source,
                    duration_ms: entry.duration_ms,
                    plan: entry.plan.clone(),
                };
                if let Ok(json) = serde_json::to_vec(&body) {
                    let tmp = file.with_extension("tmp");
                    if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, &file).is_ok() {
                        let gone = lock(&self.plan_files).record(&entry.id, now, now);
                        remove_files(gone);
                    }
                }
            }
        }
        let entry = Arc::new(entry);
        lock(&self.plans).insert(entry.id.clone(), entry.clone());
        entry
    }

    /// A plan by id: memory, else its file (after a restart).
    pub async fn plan_entry(&self, id: &str) -> ApiResult<Arc<PlanEntry>> {
        let hit = lock(&self.plans).get(&id.to_string()).cloned();
        if let Some(e) = hit {
            self.plan_used(id);
            return Ok(e);
        }
        let valid =
            !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if !valid {
            return Err(ApiError::not_found("No such stream"));
        }
        let file = self.plan_dir.join(format!("{id}.json"));
        let bytes = tokio::fs::read(&file)
            .await
            .map_err(|_| ApiError::not_found("No such stream (it expired); start playback again"))?;
        let f: PlanFile =
            serde_json::from_slice(&bytes).map_err(|e| ApiError::internal(format!("plan file {id}: {e}")))?;
        if let Ok(meta) = tokio::fs::metadata(&file).await {
            let gone = lock(&self.plan_files).record(id, flux_catalog::now_ms(), mtime_ms(&meta));
            remove_files(gone);
            self.plan_used(id);
        }
        let entry = Arc::new(PlanEntry {
            id: id.to_string(),
            asset_id: f.asset_id,
            source: f.source,
            duration_ms: f.duration_ms,
            plan: f.plan,
            stats: StreamStats::default(),
        });
        lock(&self.plans).insert(entry.id.clone(), entry.clone());
        Ok(entry)
    }

    /// Counters for `/media/{asset}/file`.
    pub fn direct_stats(&self, asset_id: Id) -> Arc<StreamStats> {
        lock(&self.direct).entry(asset_id).or_default().clone()
    }

    fn new_session(&self, s: Session) -> Arc<Session> {
        lock(&self.sessions).insert(Arc::new(s), flux_catalog::now_ms())
    }

    /// Sessions held in memory.
    pub fn sessions_open(&self) -> usize {
        lock(&self.sessions).map.len()
    }

    /// A session by id. Session ids are `{item}.{plan_id}.{nonce}`, so one
    /// the server forgot (restart) is rebuilt for the caller; it is kept
    /// only when `remember` (a progress report), so reading diagnostics of
    /// made-up ids creates no state.
    pub async fn session(
        &self,
        st: &AppState,
        id: &str,
        caller: &Caller,
        remember: bool,
    ) -> ApiResult<Arc<Session>> {
        let known = lock(&self.sessions).map.get(id).cloned();
        if let Some(s) = known {
            s.touched_ms.store(flux_catalog::now_ms(), Ordering::Relaxed);
            return Ok(s);
        }
        let unknown = || ApiError::not_found("No such playback session");
        let mut parts = id.split('.');
        let (Some(item), Some(plan_id), Some(_nonce), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(unknown());
        };
        let item_id: Id = item.parse().map_err(|_| unknown())?;
        let entry = self.plan_entry(plan_id).await.map_err(|_| unknown())?;
        let user_id = st.user_id(caller).await?;
        let s = Session {
            id: id.to_string(),
            item_id,
            asset_id: entry.asset_id,
            plan_id: entry.id.clone(),
            user_id,
            path: caller.conn.path.as_str().to_string(),
            duration_ms: entry.duration_ms,
            touched_ms: AtomicI64::new(flux_catalog::now_ms()),
            client: Mutex::new(None),
        };
        Ok(if remember {
            self.new_session(s)
        } else {
            Arc::new(s)
        })
    }
}

/// Builds the prepared asset: the catalog's cached index (flux-hls binary,
/// valid while the file's size and mtime are unchanged, path rewritten to
/// the file's current location), else a fresh index that is stored back.
async fn build_asset(st: &AppState, loc: &AssetLocation) -> Result<Arc<Asset>, HlsError> {
    let asset_id = loc.asset_id;
    let path = PathBuf::from(&loc.path);
    let cached = st
        .catalog
        .read(move |c| read::asset_play_index(c, asset_id))
        .await
        .ok()
        .flatten();
    if let Some(cached) = cached {
        let usable = cached.format == INDEX_FORMAT_BINARY && cached.version == INDEX_VERSION;
        let decoded = if usable {
            tokio::task::spawn_blocking(move || {
                // The catalog keeps assets across renames; size + mtime still
                // guard the content (integration.md §4.3).
                PlaybackIndex::from_bytes(&cached.bytes).map(|i| i.with_path(cached.path))
            })
            .await
            .map_err(|e| HlsError::Io(std::io::Error::other(e)))?
        } else {
            Err(HlsError::NotFound(format!(
                "cached index is {} v{}",
                cached.format, cached.version
            )))
        };
        let stale = match decoded {
            Ok(idx) => match st.engine.prepare_with_index(idx).await {
                Ok(a) => return Ok(a),
                Err(e) => e,
            },
            Err(e) => e,
        };
        tracing::debug!(asset_id, error = %stale, "cached playback index unusable; rebuilding");
        // Enqueued now, so it is ordered before the put below.
        let _ = st
            .catalog
            .writer()
            .clear_asset_index(asset_id)
            .submit_async()
            .await;
    }
    let ffprobe = st.cfg.ffprobe.clone();
    // A stalled or endless ffprobe scan is killed (review ROB-1).
    let limits = st.engine.config().index_scan_limits;
    let (idx, bytes) = tokio::task::spawn_blocking(move || {
        let src = flux_hls::source::Source::open(&path)?;
        let idx = PlaybackIndex::build_with_fallback_limits(&src, Some(&ffprobe), &limits)?;
        let bytes = idx.to_bytes();
        Ok::<_, HlsError>((idx, bytes))
    })
    .await
    .map_err(|e| HlsError::Io(std::io::Error::other(e)))??;
    match bytes {
        Ok(bytes) => {
            let put = PlayIndex {
                format: INDEX_FORMAT_BINARY.to_string(),
                version: idx.version,
                source_size: idx.source.size,
                source_mtime_ns: idx.source.mtime_ns,
                bytes,
            };
            // Enqueued, not awaited: playback doesn't wait for the commit.
            if let Err(e) = st
                .catalog
                .writer()
                .put_asset_index(asset_id, put)
                .submit_async()
                .await
            {
                tracing::debug!(asset_id, error = %e, "could not cache the playback index");
            }
        }
        Err(e) => tracing::debug!(asset_id, error = %e, "could not encode the playback index"),
    }
    st.engine.prepare_with_index(idx).await
}

/// The asset's location and the root paths of its library; 404 unknown,
/// 409 unreachable. The path is not checked yet: see [`location`].
pub async fn locate(st: &AppState, asset_id: Id) -> ApiResult<(AssetLocation, Vec<String>)> {
    let found = st
        .catalog
        .read(move |c| {
            let Some(loc) = read::asset_location(c, asset_id)? else {
                return Ok(None);
            };
            let roots = scan_support::list_roots(c)?;
            let library = roots.iter().find(|r| r.id == loc.root_id).map(|r| r.library_id);
            let paths = roots
                .into_iter()
                .filter(|r| Some(r.library_id) == library)
                .map(|r| r.path)
                .collect();
            Ok(Some((loc, paths)))
        })
        .await?;
    let (loc, roots) = found.ok_or_else(|| ApiError::not_found(format!("No file {asset_id}")))?;
    if !loc.available {
        return Err(unavailable());
    }
    Ok((loc, roots))
}

/// The asset's location, if its file may be served (see [`allowed`]). A
/// symlink under a root that points elsewhere, or is re-pointed after the
/// scan, would otherwise serve any file the server can read to any caller
/// (review SEC-2). 404 unknown or refused, 409 unreachable.
pub async fn location(st: &AppState, asset_id: Id) -> ApiResult<AssetLocation> {
    let (loc, roots) = locate(st, asset_id).await?;
    let path = PathBuf::from(&loc.path);
    let rec = Recorded::of(&loc);
    tokio::task::spawn_blocking(move || allowed(&path, &roots, &rec))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))??;
    Ok(loc)
}

/// The identity the scan recorded for an asset's file (`AssetLocation`):
/// for a link, its target's, since the scan stats through links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recorded {
    /// `st_dev`, or the Windows volume serial number.
    pub dev: Option<u64>,
    /// The inode, or the Windows file id (recorded by the scan for files
    /// behind a link that leaves the library; see `flux_core::file_id`).
    pub ino: Option<u64>,
    pub size: u64,
    pub mtime_ns: i64,
    /// The scan last probed this file as media (catalog state `present`,
    /// not `probe_failed`). A file reached through a link that leaves the
    /// library is served only when this holds: re-pointing the link at a
    /// non-media file (an ssh key, a document) re-probes as a failure, and
    /// although the scan then records that file's identity, it is not
    /// served (review SEC-2; found by the re-verification).
    pub media: bool,
}

impl Recorded {
    pub fn of(loc: &AssetLocation) -> Recorded {
        Recorded {
            dev: loc.dev,
            ino: loc.ino,
            size: loc.size,
            mtime_ns: loc.mtime_ns,
            media: loc.probe_error.is_none(),
        }
    }

    /// The file `meta` and `id` describe is the very file the scan
    /// recorded: same inode (file id), size and modification time. On Unix
    /// the device number is left out on purpose: a remounted NAS gets a new
    /// one, while an inode, size and nanosecond mtime that all match can't
    /// name a different file by accident, and a link's author can't give a
    /// file they want to read the inode of the media file the scan probed.
    /// On Windows the volume serial number is stable across remounts and is
    /// compared too: FAT and exFAT file ids are only positions in a
    /// directory, so they repeat across volumes. A scan that recorded no
    /// identity (`None`) matches nothing.
    pub fn matches(&self, meta: &std::fs::Metadata, id: &FileId) -> bool {
        let dev_ok = !cfg!(windows) || self.dev == Some(id.dev);
        dev_ok
            && self.ino == Some(id.ino)
            && self.size == meta.len()
            && self.mtime_ns == flux_core::file_id::mtime_ns(meta)
    }
}

fn cant_open(e: std::io::Error) -> ApiError {
    ApiError::conflict(format!("The file can't be opened right now: {e}"))
}

fn outside(path: &Path, real: &Path) -> ApiError {
    tracing::warn!(
        path = %path.display(),
        target = %real.display(),
        "refusing to serve a file outside its library that isn't the one the scan recorded (a re-pointed symlink?)"
    );
    ApiError::not_found("No such file")
}

/// `real` (a resolved path) is under one of `roots` (resolved too).
fn inside(real: &Path, roots: &[String]) -> bool {
    roots
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .any(|root| real.starts_with(&root))
}

/// Where `path` really is (symlinks resolved), if that is inside one of
/// `roots` (blocking).
pub fn contained(path: &Path, roots: &[String]) -> ApiResult<PathBuf> {
    let real = std::fs::canonicalize(path).map_err(cant_open)?;
    if inside(&real, roots) {
        Ok(real)
    } else {
        Err(outside(path, &real))
    }
}

/// Where `path` really is, if it may be served (blocking): inside one of
/// `roots`, or, behind a link that leaves them, exactly the file the scan
/// recorded (`rec`). flux-scan follows links out of the roots (a debrid or
/// rclone mount, another disk) and catalogs a file reached that way only if
/// it probes as media (scan.md "Symlinks"); a link re-pointed after the
/// scan names another file and is refused.
pub fn allowed(path: &Path, roots: &[String], rec: &Recorded) -> ApiResult<PathBuf> {
    let real = std::fs::canonicalize(path).map_err(cant_open)?;
    if inside(&real, roots) {
        return Ok(real);
    }
    let meta = std::fs::metadata(&real).map_err(cant_open)?;
    let id = FileId::of_path(&real).map_err(cant_open)?;
    if rec.media && rec.matches(&meta, &id) {
        Ok(real)
    } else {
        Err(outside(path, &real))
    }
}

/// Opens `path` and checks that the file opened may be served (blocking):
/// it lies inside one of `roots`, or it is the file the scan recorded
/// (`rec`; see [`allowed`]). It opens first, then resolves the path and
/// compares the opened file's identity with the file the resolved path
/// names, so re-pointing a symlink (or swapping a directory) between the
/// check and the open can't slip another file through.
pub fn open_contained(path: &Path, roots: &[String], rec: &Recorded) -> ApiResult<(File, std::fs::Metadata)> {
    let file = File::open(path).map_err(cant_open)?;
    let meta = file.metadata().map_err(cant_open)?;
    let opened = FileId::of_file(&file).map_err(cant_open)?;
    let real = std::fs::canonicalize(path).map_err(cant_open)?;
    if !inside(&real, roots) {
        // Outside the library: only the very file the scan recorded, and
        // only while that file probes as media (`rec.media`), so a link
        // re-pointed at a non-media file the scan then re-records (with its
        // new identity) is still refused.
        return if rec.media && rec.matches(&meta, &opened) {
            Ok((file, meta))
        } else {
            Err(outside(path, &real))
        };
    }
    let named = FileId::of_path(&real).map_err(cant_open)?;
    if !named.same_file(&opened) {
        return Err(outside(path, &real));
    }
    Ok((file, meta))
}

fn unavailable() -> ApiError {
    ApiError::conflict("This file is on a drive or server that isn't reachable right now. It will play again once it's back.")
}

/// Picks the asset to play: the requested one, else the best available one
/// (an SDR version on an SDR display when the item has both).
async fn pick_asset(
    st: &AppState,
    item_id: Id,
    asset_id: Option<Id>,
    caps: &ClientCaps,
) -> ApiResult<AssetLocation> {
    let assets = st.catalog.read(move |c| read::item_assets(c, item_id)).await?;
    let chosen = match asset_id {
        Some(a) => {
            if !assets.iter().any(|x| x.id == a) {
                return Err(ApiError::bad_request(format!(
                    "File {a} is not a version of item {item_id}"
                )));
            }
            a
        }
        None => {
            if assets.is_empty() {
                let exists = st
                    .catalog
                    .read(move |c| read::item_summary(c, item_id, None))
                    .await?
                    .is_some();
                return Err(if exists {
                    ApiError::bad_request("This item has no file of its own to play (pick an episode)")
                } else {
                    ApiError::not_found(format!("No item {item_id}"))
                });
            }
            best_asset(&assets, caps).ok_or_else(unavailable)?
        }
    };
    location(st, chosen).await
}

fn is_hdr(a: &AssetInfo) -> bool {
    a.hdr.is_some_and(|h| h != HdrFormat::Sdr)
}

fn best_asset(assets: &[AssetInfo], caps: &ClientCaps) -> Option<Id> {
    // `item_assets` is ordered best first within each edition.
    let first = assets.iter().find(|a| a.available)?;
    if !caps.display_hdr && is_hdr(first) {
        if let Some(sdr) = assets
            .iter()
            .find(|a| a.available && a.edition == first.edition && !is_hdr(a))
        {
            return Some(sdr.id);
        }
    }
    Some(first.id)
}

/// The plan for a location from its playback index. A file the engine
/// can't index (no video track, a broken container, …) can still play
/// directly when its stored probe says the client takes it as is.
async fn plan_for(
    st: &AppState,
    loc: &AssetLocation,
    caps: &ClientCaps,
    prefs: &PlanPrefs,
) -> ApiResult<(Option<Arc<Asset>>, PlaybackPlan, SourceKey)> {
    match st.play.load_asset(st, loc).await {
        Ok(asset) => {
            let plan = st.engine.plan(&asset, caps, prefs);
            let key = SourceKey::of(&asset);
            Ok((Some(asset), plan, key))
        }
        Err(e) => {
            if let Some(probe) = &loc.probe {
                let plan = flux_hls::plan_from_probe(probe, caps, prefs);
                if plan.delivery == DeliveryFormat::ProgressiveFile {
                    tracing::debug!(asset = loc.asset_id, error = %e, "no playback index; direct play only");
                    let key = SourceKey::stat(Path::new(&loc.path)).map_err(|_| unavailable())?;
                    return Ok((None, plan, key));
                }
            }
            Err(match e {
                HlsError::Unsupported(m) | HlsError::Malformed { detail: m, .. } => {
                    ApiError::not_implemented(format!("flux can't stream this file to this device yet: {m}"))
                }
                other => other.into(),
            })
        }
    }
}

/// Bit rate rungs (bits/s) a caller's `max_bitrate` is rounded down to.
const BITRATE_RUNGS: &[u64] = &[
    250_000,
    500_000,
    750_000,
    1_000_000,
    1_500_000,
    2_000_000,
    3_000_000,
    4_000_000,
    5_000_000,
    6_000_000,
    8_000_000,
    10_000_000,
    12_000_000,
    15_000_000,
    20_000_000,
    25_000_000,
    30_000_000,
    40_000_000,
    50_000_000,
    60_000_000,
    80_000_000,
    100_000_000,
    120_000_000,
    150_000_000,
    200_000_000,
];
/// Widths a caller's `max_width` is rounded down to.
const WIDTH_RUNGS: &[u32] = &[
    160, 256, 320, 426, 480, 640, 720, 854, 960, 1024, 1280, 1366, 1600, 1920, 2048, 2560, 3200, 3840, 4096,
    5120, 7680, 8192,
];
/// Heights a caller's `max_height` is rounded down to.
const HEIGHT_RUNGS: &[u32] = &[
    90, 120, 144, 180, 240, 288, 360, 480, 540, 576, 720, 768, 900, 1080, 1200, 1440, 1600, 2160, 2880, 4320,
];

/// The highest rung at or below `v` (the lowest rung below that).
fn round_down<T: Copy + Ord>(v: T, rungs: &[T]) -> T {
    rungs.iter().rev().find(|r| **r <= v).copied().unwrap_or(rungs[0])
}

/// The caller's capabilities with their free numbers (`max_bitrate`, the
/// video caps' `max_width`/`max_height`) rounded down to standard rungs. The
/// planner copies them into plans (a bitrate limit becomes the transcode
/// rate and a `Reason`), and plan ids hash the plan, so unrounded numbers
/// let one caller mint a new persisted plan per request (review SEC-4).
pub fn quantize_caps(caps: &ClientCaps) -> ClientCaps {
    let mut caps = caps.clone();
    caps.max_bitrate = caps.max_bitrate.map(|b| round_down(b, BITRATE_RUNGS));
    for v in &mut caps.video {
        v.max_width = v.max_width.map(|w| round_down(w, WIDTH_RUNGS));
        v.max_height = v.max_height.map(|h| round_down(h, HEIGHT_RUNGS));
    }
    caps
}

fn prefs_of(req: &PlayRequest) -> PlanPrefs {
    PlanPrefs {
        audio_track: req.audio_track,
        subtitle_track: req.subtitle_track,
        ..PlanPrefs::default()
    }
}

/// Deterministic plan id: `{asset}-{xxh3(index version, file identity, plan)}`.
pub fn plan_id(asset_id: Id, source: SourceKey, plan: &PlaybackPlan) -> String {
    let recipe = serde_json::to_vec(&(INDEX_VERSION, source, plan)).unwrap_or_default();
    format!("{asset_id}-{:016x}", xxhash_rust::xxh3::xxh3_64(&recipe))
}

fn nonce() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_i64(flux_catalog::now_ms());
    h.finish()
}

/// `POST /api/v1/play`.
pub async fn play(
    State(st): State<Arc<AppState>>,
    caller: Caller,
    body: Result<Json<PlayRequest>, JsonRejection>,
) -> ApiResult<Json<PlayResponse>> {
    let Json(req) = body?;
    let caps = quantize_caps(&req.caps);
    let user_id = st.user_id(&caller).await?;
    let loc = pick_asset(&st, req.item_id, req.asset_id, &caps).await?;
    let (asset, plan, source) = plan_for(&st, &loc, &caps, &prefs_of(&req)).await?;
    let id = plan_id(loc.asset_id, source, &plan);
    let duration_ms = loc.duration_ms.or_else(|| {
        asset
            .as_ref()
            .map(|a| (a.index.duration_ns.max(0) / 1_000_000) as u64)
    });
    let entry = st.play.register(PlanEntry {
        id: id.clone(),
        asset_id: loc.asset_id,
        source,
        duration_ms,
        plan: plan.clone(),
        stats: StreamStats::default(),
    });

    // The caller's resume point. A finished viewing stores position 0, so a
    // watched item with a position is a rewatch in progress: Continue
    // watching lists it, and it resumes like any other.
    let start_ms = match req.start_ms {
        Some(ms) => ms,
        None => {
            let item = req.item_id;
            st.catalog
                .read(move |c| read::progress(c, user_id, item))
                .await?
                .map_or(0, |p| p.position_ms)
        }
    };

    let base = format!("/media/{id}");
    let mut prefetch = Vec::new();
    let (url, audio, subtitles) = match (&asset, entry.is_direct()) {
        (Some(asset), false) => {
            let t = start_ms as f64 / 1000.0;
            let n = asset.segments.segment_at(t) as u32;
            // One-round-trip play: segment n of every rendition starts now.
            st.engine.warm(asset, &plan, n);
            // A transcoded video rendition is cut into ~2 s pieces, so its
            // segment number differs from the remux segment n.
            let vn = st.engine.segment_at(asset, &plan, &Rendition::Video, t);
            prefetch.push(format!("{base}/v/init.mp4"));
            prefetch.push(format!("{base}/v/{vn}.m4s"));
            if let Some(a) = plan.audio.first() {
                let k = AudioKey::from_plan(a).path();
                prefetch.push(format!("{base}/a/{k}/init.mp4"));
                prefetch.push(format!("{base}/a/{k}/{n}.m4s"));
                prefetch.push(format!("{base}/a/{k}/index.m3u8"));
            }
            prefetch.push(format!("{base}/v/index.m3u8"));
            (
                format!("{base}/master.m3u8"),
                flux_hls::playlist::audio_refs(asset, &plan),
                subtitle_refs(asset, &plan, &base),
            )
        }
        (asset, _) => (
            format!("/media/{}/file", loc.asset_id),
            Vec::new(),
            asset
                .as_ref()
                .map(|a| subtitle_refs(a, &plan, &base))
                .unwrap_or_default(),
        ),
    };

    let session = st.play.new_session(Session {
        id: format!("{}.{id}.{:08x}", req.item_id, nonce() as u32),
        item_id: req.item_id,
        asset_id: loc.asset_id,
        plan_id: id,
        user_id,
        path: caller.conn.path.as_str().to_string(),
        duration_ms,
        touched_ms: AtomicI64::new(flux_catalog::now_ms()),
        client: Mutex::new(None),
    });
    Ok(Json(PlayResponse {
        session_id: session.id.clone(),
        asset_id: loc.asset_id,
        explain: plan.explain(),
        plan,
        url,
        start_ms,
        duration_ms,
        prefetch,
        audio,
        subtitles,
    }))
}

fn subtitle_refs(asset: &Asset, plan: &PlaybackPlan, base: &str) -> Vec<SubtitleRef> {
    plan.subtitles
        .iter()
        .filter(|s| s.op == SubtitleOp::WebVtt)
        .filter_map(|s| {
            let t = asset.index.track(s.track)?;
            Some(SubtitleRef {
                track: s.track,
                label: t
                    .name
                    .clone()
                    .filter(|n| !n.is_empty())
                    .or_else(|| t.language.clone())
                    .unwrap_or_else(|| format!("Subtitles {}", s.track)),
                language: t.language.clone(),
                url: format!("{base}/s/{}.vtt", s.track),
                forced: t.forced,
            })
        })
        .collect()
}

/// `POST /api/v1/plan`: what `/play` would do, without a session. Uses the
/// prepared asset when one is in memory (exact), else the stored probe
/// (`plan_from_probe`: no file I/O; it agrees with the index plan on the
/// whole corpus except the Firefox/macOS open-GOP quirk, which needs the
/// index), else prepares the asset.
pub async fn plan_preview(
    State(st): State<Arc<AppState>>,
    body: Result<Json<PlayRequest>, JsonRejection>,
) -> ApiResult<Json<PlanPreview>> {
    let Json(req) = body?;
    let caps = quantize_caps(&req.caps);
    let loc = pick_asset(&st, req.item_id, req.asset_id, &caps).await?;
    let prefs = prefs_of(&req);
    let plan = match (st.play.cached_asset(loc.asset_id, Some(&loc.path)), &loc.probe) {
        (Some(asset), _) => st.engine.plan(&asset, &caps, &prefs),
        (None, Some(probe)) => flux_hls::plan_from_probe(probe, &caps, &prefs),
        (None, None) => plan_for(&st, &loc, &caps, &prefs).await?.1,
    };
    Ok(Json(PlanPreview::new(loc.asset_id, plan)))
}

/// `POST /api/v1/sessions/{id}/progress`.
pub async fn progress(
    State(st): State<Arc<AppState>>,
    caller: Caller,
    UrlPath(id): UrlPath<String>,
    body: Result<Json<ProgressReport>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let Json(report) = body?;
    let s = st.play.session(&st, &id, &caller, true).await?;
    let position = report.position_ms;
    *lock(&s.client) = Some(report);
    // Per-user state: ≥ 90 % marks the item watched (and every episode of a
    // multi-episode file). Track choices aren't stored per user yet.
    if let Some(duration) = s.duration_ms.filter(|d| *d > 0) {
        st.catalog
            .writer()
            .set_progress(s.user_id, s.item_id, Some(s.asset_id), position, duration)
            .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/sessions/{id}/diagnostics`.
pub async fn diagnostics(
    State(st): State<Arc<AppState>>,
    caller: Caller,
    UrlPath(id): UrlPath<String>,
) -> ApiResult<Json<SessionDiagnostics>> {
    let s = st.play.session(&st, &id, &caller, false).await?;
    let entry = st.play.plan_entry(&s.plan_id).await?;
    let direct;
    let stats: &StreamStats = if entry.is_direct() {
        direct = st.play.direct_stats(entry.asset_id);
        &direct
    } else {
        &entry.stats
    };
    let (p50, p95) = stats.percentiles();
    let client = lock(&s.client).clone();
    Ok(Json(SessionDiagnostics {
        session_id: s.id.clone(),
        explain: entry.plan.explain(),
        plan: entry.plan.clone(),
        path: s.path.clone(),
        segments_served: stats.segments(),
        bytes_served: stats.bytes(),
        segment_ms_p50: p50,
        segment_ms_p95: p95,
        client,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_core::{AudioOp, AudioPlan, VideoOp};

    fn plan() -> PlaybackPlan {
        PlaybackPlan {
            delivery: DeliveryFormat::HlsFmp4,
            video_track: Some(0),
            video: VideoOp::Copy { fixes: vec![] },
            audio: vec![AudioPlan {
                track: 1,
                op: AudioOp::Copy,
            }],
            subtitles: vec![],
            demuxed_audio: true,
            reasons: vec![],
            est_cpu_ms_per_min: 0,
        }
    }

    #[test]
    fn plan_ids_are_deterministic_and_change_with_the_recipe_or_file() {
        let src = SourceKey {
            size: 10,
            mtime_ns: 5,
        };
        let a = plan_id(7, src, &plan());
        assert_eq!(a, plan_id(7, src, &plan()));
        assert!(a.starts_with("7-"));
        let mut other = plan();
        other.audio[0].op = AudioOp::Transcode {
            codec: flux_core::Codec::Aac,
            channels: 2,
            bitrate: 192_000,
        };
        assert_ne!(a, plan_id(7, src, &other));
        assert_ne!(
            a,
            plan_id(
                7,
                SourceKey {
                    size: 11,
                    mtime_ns: 5
                },
                &plan()
            )
        );
    }

    fn session(id: &str, touched_ms: i64) -> Session {
        Session {
            id: id.to_string(),
            item_id: 1,
            asset_id: 1,
            plan_id: "1-0".into(),
            user_id: 1,
            path: "lan".into(),
            duration_ms: None,
            touched_ms: AtomicI64::new(touched_ms),
            client: Mutex::new(None),
        }
    }

    /// Review SEC-5: the map was "capped" by dropping idle sessions only, so
    /// fresh ones grew it without bound (with an O(n) scan per insert).
    #[test]
    fn sessions_are_capped_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let play = PlayState::new(dir.path(), 1 << 20);
        let now = flux_catalog::now_ms();
        for i in 0..SESSION_CAP + 500 {
            play.new_session(session(&format!("1.p.{i}"), now));
        }
        assert_eq!(play.sessions_open(), SESSION_CAP);
        let sessions = lock(&play.sessions);
        assert!(!sessions.map.contains_key("1.p.0"), "the oldest went first");
        assert!(sessions.map.contains_key(&format!("1.p.{}", SESSION_CAP + 499)));
        assert_eq!(sessions.order.len(), sessions.map.len());
        drop(sessions);
        // Re-inserting a known id keeps the session that is there.
        let again = play.new_session(session(&format!("1.p.{}", SESSION_CAP + 499), 0));
        assert_eq!(again.touched_ms.load(Ordering::Relaxed), now);
        assert_eq!(play.sessions_open(), SESSION_CAP);
        // Idle sessions at the front go when the next one arrives.
        let mut s = Sessions::default();
        s.insert(Arc::new(session("old", now - SESSION_IDLE_MS - 1)), now);
        s.insert(Arc::new(session("new", now)), now);
        assert_eq!(s.map.len(), 1);
        assert!(s.map.contains_key("new"));
    }

    fn set_mtime(path: &Path, age_days: u64) {
        let t = SystemTime::now() - std::time::Duration::from_secs(age_days * 86_400);
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn mtime_age_s(path: &Path) -> u64 {
        std::fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap()
            .as_secs()
    }

    /// Review SEC-4: plan files had no bound, and their mtime was their
    /// creation time, so the startup prune also removed plans in use.
    #[test]
    fn plan_files_are_bounded_and_record_their_last_use() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = PlanFiles::new(dir.path().to_path_buf(), 3);
        for (i, id) in ["a", "b", "c"].iter().enumerate() {
            assert!(files.record(id, 1000 + i as i64, 1000).is_empty());
        }
        // "a" is used again, so "b" is now the least recently used.
        assert!(
            files.use_now("a", 2000).is_none(),
            "touched less than an hour ago"
        );
        let gone = files.record("d", 3000, 3000);
        assert_eq!(gone, [dir.path().join("b.json")]);
        assert_eq!(files.used.len(), 3);
        assert!(files.use_now("b", 4000).is_none(), "unknown plans are ignored");
        // The mtime is refreshed once an hour of use has passed.
        assert_eq!(
            files.use_now("c", 1000 + PLAN_TOUCH_MS),
            Some(dir.path().join("c.json"))
        );
        assert!(files.use_now("c", 1001 + PLAN_TOUCH_MS).is_none());
    }

    #[test]
    fn startup_prune_keeps_recently_used_plans() {
        let dir = tempfile::tempdir().unwrap();
        let play = PlayState::new(dir.path(), 1 << 20);
        let plans = dir.path().join("plans");
        for (id, age) in [("1-old", 20), ("1-used", 13), ("1-new", 0)] {
            let f = plans.join(format!("{id}.json"));
            std::fs::write(&f, b"{}").unwrap();
            set_mtime(&f, age);
        }
        std::fs::write(plans.join("1-x.tmp"), b"half").unwrap();
        play.prune_plan_files();
        let mut left: Vec<String> = std::fs::read_dir(&plans)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["1-new.json", "1-used.json"]);
        assert_eq!(play.plan_files(), 2);
        // Using the 13-day-old plan refreshes its mtime, so the next
        // startup's prune keeps it.
        assert!(mtime_age_s(&plans.join("1-used.json")) > 12 * 86_400);
        play.plan_used("1-used");
        assert!(mtime_age_s(&plans.join("1-used.json")) < 60);
    }

    #[test]
    fn caps_round_down_to_rungs() {
        let mut caps = flux_hls::presets::chrome_desktop();
        caps.max_bitrate = Some(100_000);
        caps.video[0].max_width = Some(1919);
        caps.video[0].max_height = Some(1080);
        let q = quantize_caps(&caps);
        assert_eq!(q.max_bitrate, Some(250_000), "below the lowest rung");
        assert_eq!(
            (q.video[0].max_width, q.video[0].max_height),
            (Some(1600), Some(1080))
        );
        for (b, want) in [
            (3_000_000, 3_000_000),
            (3_999_999, 3_000_000),
            (7_000_000, 6_000_000),
            (u64::MAX, 200_000_000),
        ] {
            caps.max_bitrate = Some(b);
            assert_eq!(quantize_caps(&caps).max_bitrate, Some(want), "{b}");
        }
        caps.max_bitrate = None;
        assert_eq!(quantize_caps(&caps).max_bitrate, None);
        // Every rung is a fixed point.
        for &r in BITRATE_RUNGS {
            assert_eq!(round_down(r, BITRATE_RUNGS), r);
        }
    }

    fn id_scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("flux-play-id-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// What the scan records for a file it reached through a link that
    /// leaves the library (`flux_scan` asks the file for its identity where
    /// the walk doesn't report one, as on Windows).
    fn recorded(path: &Path) -> Recorded {
        let meta = std::fs::metadata(path).unwrap();
        let id = FileId::of_path(path).unwrap();
        Recorded {
            dev: Some(id.dev),
            ino: Some(id.ino),
            size: meta.len(),
            mtime_ns: flux_core::file_id::mtime_ns(&meta),
            media: true,
        }
    }

    /// SEC-2 on every platform: outside its library's roots, a file is
    /// served only when it is the very file the scan recorded and probed as
    /// media. Windows used to refuse every such file (`matches` was always
    /// false there), so a film behind a junction was listed but never
    /// played.
    #[test]
    fn a_file_outside_the_roots_is_served_only_as_recorded() {
        let d = id_scratch("outside");
        let (lib, ext) = (d.join("lib"), d.join("ext"));
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::create_dir_all(&ext).unwrap();
        let film = ext.join("Film (2021).mkv");
        let other = ext.join("not-media");
        std::fs::write(&film, vec![1u8; 4096]).unwrap();
        std::fs::write(&other, vec![2u8; 4096]).unwrap();
        let mtime = std::fs::metadata(&film).unwrap().modified().unwrap();
        File::options()
            .write(true)
            .open(&other)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let roots = vec![lib.to_string_lossy().into_owned()];
        let rec = recorded(&film);

        assert!(allowed(&film, &roots, &rec).is_ok());
        let (_f, meta) = open_contained(&film, &roots, &rec).expect("the recorded file is served");
        assert_eq!(meta.len(), 4096);
        // Same size and mtime, another file (a link re-pointed at it).
        assert!(allowed(&other, &roots, &rec).is_err());
        assert!(open_contained(&other, &roots, &rec).is_err());
        // Not media, or no identity recorded: refused.
        let not_media = Recorded { media: false, ..rec };
        assert!(open_contained(&film, &roots, &not_media).is_err());
        let unknown = Recorded { ino: None, ..rec };
        assert!(allowed(&film, &roots, &unknown).is_err());
        if cfg!(windows) {
            // The volume serial is part of a Windows identity.
            let elsewhere = Recorded {
                dev: Some(rec.dev.unwrap() ^ 1),
                ..rec
            };
            assert!(allowed(&film, &roots, &elsewhere).is_err());
        }
        // Inside the roots, the record doesn't matter.
        let inside = lib.join("a.mkv");
        std::fs::write(&inside, b"x").unwrap();
        assert!(open_contained(&inside, &roots, &unknown).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The case from a Windows run: a folder junction inside the library
    /// leads to a folder outside it. The file behind it plays while the
    /// junction points where the scan saw it, and not once it is
    /// re-pointed at a look-alike.
    #[cfg(windows)]
    #[test]
    fn a_file_behind_a_junction_leaving_the_library_plays() {
        let d = id_scratch("junction");
        let (lib, ext, ext2) = (d.join("lib"), d.join("ext"), d.join("ext2"));
        for dir in [&lib, &ext.join("Film (2021)"), &ext2.join("Film (2021)")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let rel = Path::new("Film (2021)").join("Film (2021).mkv");
        std::fs::write(ext.join(&rel), vec![1u8; 4096]).unwrap();
        std::fs::write(ext2.join(&rel), vec![2u8; 4096]).unwrap();
        let mtime = std::fs::metadata(ext.join(&rel)).unwrap().modified().unwrap();
        File::options()
            .write(true)
            .open(ext2.join(&rel))
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let link = lib.join("Ext");
        let junction = |target: &Path| {
            let ok = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&link)
                .arg(target)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "mklink /J");
        };
        junction(&ext);
        let path = link.join(&rel);
        let roots = vec![lib.to_string_lossy().into_owned()];
        let rec = recorded(&path);
        assert!(allowed(&path, &roots, &rec).is_ok());
        assert!(open_contained(&path, &roots, &rec).is_ok());
        // Re-pointed: same names, size and mtime, another file.
        std::fs::remove_dir(&link).unwrap();
        junction(&ext2);
        assert!(allowed(&path, &roots, &rec).is_err());
        assert!(open_contained(&path, &roots, &rec).is_err());
        std::fs::remove_dir(&link).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn stats_percentiles() {
        let s = StreamStats::default();
        assert_eq!(s.percentiles(), (None, None));
        for ms in [1.0, 2.0, 3.0, 4.0, 100.0] {
            s.record(ms, 10);
        }
        assert_eq!(s.percentiles(), (Some(3.0), Some(100.0)));
        assert_eq!((s.segments(), s.bytes()), (5, 50));
    }
}
