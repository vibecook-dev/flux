//! Catalog-local types: file states, scanner diff rows, write commands'
//! inputs and outputs, and the string mappings for enums stored as TEXT.

use std::time::{SystemTime, UNIX_EPOCH};

use flux_core::api::{ArtSource, Id, ItemKind, Role};
use flux_core::{Container, HdrFormat, LibraryKind, NameGuess, ProbeResult};
use serde::{Deserialize, Serialize};

use crate::error::{CatalogError, Result};

/// Current time as Unix milliseconds (the catalog's timestamp unit).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Store a `u64` (inode, device, oshash) in a signed SQLite integer by bit
/// pattern. Round-trips exactly through [`u64_from_sql`].
pub fn u64_to_sql(v: u64) -> i64 {
    v as i64
}

/// Inverse of [`u64_to_sql`].
pub fn u64_from_sql(v: i64) -> u64 {
    v as u64
}

/// Clamp a `u64` quantity (size, duration) into SQLite's signed range.
pub(crate) fn clamp_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// `fp128` is stored as 16 little-endian bytes (same layout as research/02's harness).
pub fn fp128_to_blob(v: u128) -> [u8; 16] {
    v.to_le_bytes()
}

/// Inverse of [`fp128_to_blob`]; `None` if the blob is not 16 bytes.
pub fn fp128_from_blob(b: &[u8]) -> Option<u128> {
    <[u8; 16]>::try_from(b).ok().map(u128::from_le_bytes)
}

/// Lifecycle of a location (research/02 §8.2 side states).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileState {
    /// Seen by the last successful listing.
    Present,
    /// A successful listing of its directory did not contain it; kept as a
    /// tombstone until `tombstone_until` so it can re-attach.
    Missing,
    /// Its root is offline (NAS asleep, drive unplugged). Never deleted.
    Unavailable,
    /// Present, but the probe failed; still browsable from its name.
    ProbeFailed,
}

impl FileState {
    pub fn as_str(self) -> &'static str {
        match self {
            FileState::Present => "present",
            FileState::Missing => "missing",
            FileState::Unavailable => "unavailable",
            FileState::ProbeFailed => "probe_failed",
        }
    }
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "present" => FileState::Present,
            "missing" => FileState::Missing,
            "unavailable" => FileState::Unavailable,
            "probe_failed" => FileState::ProbeFailed,
            other => return Err(CatalogError::Corrupt(format!("file.state = {other:?}"))),
        })
    }
}

/// Whether a root's storage is reachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootState {
    Available,
    Unavailable,
}

impl RootState {
    pub fn as_str(self) -> &'static str {
        match self {
            RootState::Available => "available",
            RootState::Unavailable => "unavailable",
        }
    }
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "available" => Ok(RootState::Available),
            "unavailable" => Ok(RootState::Unavailable),
            other => Err(CatalogError::Corrupt(format!("root.state = {other:?}"))),
        }
    }
}

/// One row of [`crate::scan_support::load_root_snapshot`]: what the scanner
/// diffs a fresh listing against. Deliberately small (no probe/guess JSON).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    pub id: Id,
    pub dev: Option<u64>,
    pub ino: Option<u64>,
    pub size: u64,
    pub mtime_ns: i64,
    pub btime_ns: Option<i64>,
    pub fp128: Option<u128>,
    pub state: FileState,
    /// `None` = never probed (or probe failed before producing a result).
    pub probe_version: Option<u16>,
    pub parse_version: Option<u16>,
    pub asset_id: Option<Id>,
    /// For a `ProbeFailed` file: when the scanner should probe it again
    /// (exponential backoff per failed attempt, 1 min doubling to 1 day;
    /// `None` = now). A file whose stat changes is re-probed regardless.
    pub probe_retry_at: Option<i64>,
}

impl FileRow {
    /// The cheap "nothing changed" test from research/02 §0: same inode and
    /// same size and mtime. A file that matches and is `Present` needs no
    /// fingerprint, no probe and no write.
    pub fn same_stat(&self, f: &FileFacts) -> bool {
        self.size == f.size
            && self.mtime_ns == f.mtime_ns
            && (self.ino.is_none() || f.ino.is_none() || (self.ino == f.ino && self.dev == f.dev))
    }
}

/// What the walker + fingerprinter know about a file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileFacts {
    /// Path relative to the root, `/`-separated, no leading slash.
    pub rel_path: String,
    pub dev: Option<u64>,
    pub ino: Option<u64>,
    pub btime_ns: Option<i64>,
    pub size: u64,
    pub mtime_ns: i64,
    /// `xxh3_128(size ‖ head 64 KiB ‖ tail 64 KiB)` (research/02 §4.2).
    pub fp128: Option<u128>,
    pub oshash: Option<u64>,
}

/// What to do with a file's stored probe.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum ProbeUpdate {
    /// Keep the stored probe (stat-only change, or a revived file).
    #[default]
    Keep,
    /// A fresh probe result.
    Probed(Box<ProbeResult>),
    /// The probe failed; the file stays browsable by its name.
    Failed(String),
    /// The file is not media at all (a text file with a video extension,
    /// such as a TypeScript `.ts` source), and why. Its row is kept
    /// `probe_failed` with placement `ignored`, so it maps to no item and
    /// no later scan looks at it again until it changes
    /// ([`crate::scan_support::PROBE_RETRY_NEVER`]).
    NotMedia(String),
}

/// An added or changed file (keyed by `(root, rel_path)`).
#[derive(Debug, Clone, PartialEq)]
pub struct FileUpsert {
    pub facts: FileFacts,
    pub probe: ProbeUpdate,
    /// `None` keeps the stored guess; a new file without one is treated as
    /// [`NameGuess::unknown`] (an "unmatched" item named after the file).
    pub guess: Option<NameGuess>,
}

/// The catalog row a scanner decision was made from (its snapshot): a
/// `missing` id or a `moves` entry is applied only while the row is still
/// at this path with this stat. If another job (a concurrent watcher
/// batch) moved or changed the row since the snapshot was taken, the stale
/// decision is skipped instead of tombstoning or moving a live file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedRow {
    pub file_id: Id,
    pub rel_path: String,
    pub size: u64,
    pub mtime_ns: i64,
}

/// A content fingerprint computed after the file was cataloged (see
/// [`crate::Writer::attach_fingerprints`]). The row is found by
/// `(root, rel_path)` and updated only while its `size` and `mtime_ns` are
/// still the ones the fingerprint was computed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFingerprint {
    pub rel_path: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub fp128: u128,
    pub oshash: Option<u64>,
}

/// A rename/move within the root, detected by the scanner via `(dev, ino)`.
/// The file keeps its id, asset, item and therefore its watch state.
#[derive(Debug, Clone, PartialEq)]
pub struct FileMove {
    pub file_id: Id,
    /// New location and stat. `fp128`/`oshash` of `None` keep the stored ones.
    pub to: FileFacts,
    /// Guess for the new path; `None` keeps the stored one.
    pub guess: Option<NameGuess>,
}

/// A directory listing's mtime, for the O(dirs) poll fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirRow {
    pub rel_path: String,
    pub mtime_ns: i64,
}

/// One batch of scanner results for a root, applied atomically by the writer
/// (one savepoint), together with the change-source cursor it covers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScanBatch {
    pub root_id: Id,
    pub upserts: Vec<FileUpsert>,
    pub moves: Vec<FileMove>,
    /// File ids a successful listing confirmed gone (→ tombstone).
    pub missing: Vec<Id>,
    pub dirs: Vec<DirRow>,
    pub removed_dirs: Vec<String>,
    /// `Some(cursor)` persists the root's resume cursor in the same transaction.
    pub cursor: Option<Vec<u8>>,
    /// Guards for `missing` and `moves`: the snapshot rows they were decided
    /// from. An id listed here is tombstoned or moved only while its row
    /// still matches (path, size, mtime); ids without a guard apply as
    /// before. Writes nothing on its own.
    pub expect: Vec<ExpectedRow>,
}

impl ScanBatch {
    /// A batch with nothing to write. The writer is never contacted for it,
    /// so an unchanged rescan writes zero WAL frames.
    pub fn is_empty(&self) -> bool {
        self.upserts.is_empty()
            && self.moves.is_empty()
            && self.missing.is_empty()
            && self.dirs.is_empty()
            && self.removed_dirs.is_empty()
            && self.cursor.is_none()
    }

    /// Number of file-level changes (upserts + moves + missing).
    pub fn file_count(&self) -> usize {
        self.upserts.len() + self.moves.len() + self.missing.len()
    }
}

/// What applying a [`ScanBatch`] did. Feeds `Event::ItemsChanged` and scan progress.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ApplyOutcome {
    pub files_added: u64,
    pub files_changed: u64,
    pub files_moved: u64,
    pub files_missing: u64,
    /// New paths that re-attached to a tombstoned file by `fp128` (copied or
    /// moved across roots/volumes): the old asset, item and watch state survive.
    pub files_reattached: u64,
    /// Samples and other files that intentionally map to no item.
    pub files_ignored: u64,
    /// Extras whose parent item is not known yet (retried on later batches).
    pub files_pending: u64,
    pub items_added: Vec<Id>,
    pub items_updated: Vec<Id>,
    /// Items that became hidden because all their files are now missing.
    pub items_removed: Vec<Id>,
}

impl ApplyOutcome {
    /// Accumulate another outcome (e.g. from consecutive batches).
    pub fn merge(&mut self, o: ApplyOutcome) {
        self.files_added += o.files_added;
        self.files_changed += o.files_changed;
        self.files_moved += o.files_moved;
        self.files_missing += o.files_missing;
        self.files_reattached += o.files_reattached;
        self.files_ignored += o.files_ignored;
        self.files_pending += o.files_pending;
        self.items_added.extend(o.items_added);
        self.items_updated.extend(o.items_updated);
        self.items_removed.extend(o.items_removed);
    }
}

/// A root as the scanner needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootInfo {
    pub id: Id,
    pub library_id: Id,
    pub library_kind: LibraryKind,
    pub path: String,
    pub volume_uuid: Option<String>,
    pub cursor: Option<Vec<u8>>,
    pub state: RootState,
    pub last_scan_started_at: Option<i64>,
    pub last_scan_finished_at: Option<i64>,
    pub last_scan_error: Option<String>,
}

/// Result of [`crate::writer::Writer::set_root_availability`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AvailabilityChange {
    pub root_id: Id,
    /// Whether the stored state actually changed.
    pub changed: bool,
    pub files_affected: u64,
    pub items_affected: u64,
}

/// Result of [`crate::writer::Writer::purge_tombstones`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PurgeOutcome {
    pub files_deleted: u64,
    pub items_deleted: u64,
}

/// Scan bookkeeping for a root. Persisting it is a write, so a server that
/// wants "zero writes on an unchanged rescan" should record only changes or
/// errors and keep live progress in memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanRecord {
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
}

/// Provider/matcher metadata for an item (written by flux-meta). `None`
/// fields are left unchanged.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ItemMetadata {
    pub title: Option<String>,
    pub original_title: Option<String>,
    pub year: Option<u16>,
    pub overview: Option<String>,
    pub genres: Option<Vec<String>>,
    pub tmdb_id: Option<u64>,
    pub tvdb_id: Option<u64>,
    pub imdb_id: Option<String>,
    /// Other provider ids, e.g. `("anidb", "1234")`; replaces the stored set.
    pub other_ids: Option<Vec<(String, String)>>,
    pub match_confidence: Option<f32>,
    pub match_evidence: Option<Vec<String>>,
    /// `true` marks the item `matched` (a provider confirmed it).
    pub matched: bool,
}

/// Image kinds stored in `art`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtKind {
    Poster,
    Backdrop,
    Thumb,
    Logo,
}

impl ArtKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ArtKind::Poster => "poster",
            ArtKind::Backdrop => "backdrop",
            ArtKind::Thumb => "thumb",
            ArtKind::Logo => "logo",
        }
    }
}

/// A new artwork row.
#[derive(Debug, Clone, PartialEq)]
pub struct NewArt {
    pub kind: ArtKind,
    pub source: ArtSource,
    /// Local file (cached or next to the media).
    pub path: Option<String>,
    /// Remote URL (provider).
    pub url: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub blurhash: Option<String>,
    /// Who to credit (provider art), e.g. "TMDB" → `ArtRef::attribution`.
    pub attribution: Option<String>,
    /// Model id for generated art → `ArtRef::generator`.
    pub generator: Option<String>,
}

/// A stored user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRow {
    pub id: Id,
    pub login_name: String,
    pub display_name: String,
    pub role: Role,
}

/// A playback index to cache for an asset (flux-hls produces the bytes; the
/// catalog stores them opaquely).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayIndex {
    /// Encoding of `bytes`, e.g. [`INDEX_FORMAT_JSON`] or flux-hls's binary tag.
    pub format: String,
    /// The producer's index version (`flux_hls::INDEX_VERSION`).
    pub version: u16,
    /// File size the index was built from (`PlaybackIndex.source.size`).
    pub source_size: u64,
    /// File mtime the index was built from, ns (`PlaybackIndex.source.mtime_ns`).
    pub source_mtime_ns: i64,
    pub bytes: Vec<u8>,
}

/// `PlaybackIndex::to_json` bytes.
pub const INDEX_FORMAT_JSON: &str = "json";

/// A cached playback index that is still valid for the file, with the
/// file's **current** location: flux-hls's index carries the path it was
/// built at, which a rename makes stale, so the server rewrites
/// `index.source.path` with [`CachedPlayIndex::path`] before using it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedPlayIndex {
    pub asset_id: Id,
    pub format: String,
    pub version: u16,
    pub bytes: Vec<u8>,
    /// Absolute path now: root path + `/` + rel_path.
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    /// The file's root is reachable and the file isn't missing.
    pub available: bool,
}

/// Everything playback needs to open an asset's file.
#[derive(Debug, Clone, PartialEq)]
pub struct AssetLocation {
    pub asset_id: Id,
    pub item_id: Id,
    pub file_id: Id,
    pub root_id: Id,
    /// Absolute path: the root path and rel_path, joined with the root's
    /// separator (`\` for a Windows root, `/` otherwise).
    pub path: String,
    pub size: u64,
    pub available: bool,
    pub container: Option<Container>,
    pub duration_ms: Option<u64>,
    pub hdr: Option<HdrFormat>,
    pub probe: Option<ProbeResult>,
    /// The root the file was cataloged under (`path` starts with it).
    pub root_path: String,
    /// The identity the scan recorded: `(dev, ino)` where known, and the
    /// mtime. A server can `fstat` the file it opened and refuse to serve
    /// something else (a link retargeted after the scan, a swapped file).
    pub dev: Option<u64>,
    pub ino: Option<u64>,
    pub mtime_ns: i64,
    /// Why the last probe failed (`None` when it didn't); the file is
    /// retried by later scans.
    pub probe_error: Option<String>,
}

// ---- TEXT ↔ enum mappings -------------------------------------------------

pub(crate) fn library_kind_str(k: LibraryKind) -> &'static str {
    k.as_str()
}

pub(crate) fn parse_library_kind(s: &str) -> Result<LibraryKind> {
    LibraryKind::ALL
        .into_iter()
        .find(|k| k.as_str() == s)
        .ok_or_else(|| CatalogError::Corrupt(format!("library.kind = {s:?}")))
}

pub(crate) fn item_kind_str(k: ItemKind) -> &'static str {
    match k {
        ItemKind::Movie => "movie",
        ItemKind::Show => "show",
        ItemKind::Season => "season",
        ItemKind::Episode => "episode",
        ItemKind::Extra => "extra",
    }
}

pub(crate) fn parse_item_kind(s: &str) -> Result<ItemKind> {
    match s {
        "movie" => Ok(ItemKind::Movie),
        "show" => Ok(ItemKind::Show),
        "season" => Ok(ItemKind::Season),
        "episode" => Ok(ItemKind::Episode),
        "extra" => Ok(ItemKind::Extra),
        other => Err(CatalogError::Corrupt(format!("item.kind = {other:?}"))),
    }
}

pub(crate) fn art_source_str(s: ArtSource) -> &'static str {
    match s {
        ArtSource::Provider => "provider",
        ArtSource::Local => "local",
        ArtSource::Frame => "frame",
        ArtSource::Generated => "generated",
    }
}

pub(crate) fn parse_art_source(s: &str) -> Result<ArtSource> {
    match s {
        "provider" => Ok(ArtSource::Provider),
        "local" => Ok(ArtSource::Local),
        "frame" => Ok(ArtSource::Frame),
        "generated" => Ok(ArtSource::Generated),
        other => Err(CatalogError::Corrupt(format!("art.source = {other:?}"))),
    }
}

pub(crate) fn role_str(r: Role) -> &'static str {
    match r {
        Role::Owner => "owner",
        Role::Admin => "admin",
        Role::Viewer => "viewer",
        Role::Kid => "kid",
        Role::Guest => "guest",
    }
}

pub(crate) fn parse_role(s: &str) -> Result<Role> {
    match s {
        "owner" => Ok(Role::Owner),
        "admin" => Ok(Role::Admin),
        "viewer" => Ok(Role::Viewer),
        "kid" => Ok(Role::Kid),
        "guest" => Ok(Role::Guest),
        other => Err(CatalogError::Corrupt(format!("user.role = {other:?}"))),
    }
}

/// Serialise to JSON without `null` object members. flux-core's probe and
/// guess types are mostly `Option`s; dropping the nulls halves the stored
/// size (≈905 → ≈450 bytes per probe), and serde reads a missing `Option`
/// field back as `None`, so the round trip is exact.
pub fn compact_json<T: Serialize>(v: &T) -> Result<String> {
    Ok(strip_null_members(&serde_json::to_string(v)?))
}

/// Remove `"key":null` members from serde_json's compact output in one pass
/// (no `Value` tree: this runs once per scanned file). Only ASCII structural
/// bytes are inspected, so UTF-8 inside strings is copied untouched. Nulls
/// inside arrays are kept (they are positional).
fn strip_null_members(json: &str) -> String {
    let b = json.as_bytes();
    let string_end = |mut j: usize| -> usize {
        // `j` is at an opening quote; return the index after the closing one.
        j += 1;
        while j < b.len() {
            match b[j] {
                b'\\' => j += 2,
                b'"' => return j + 1,
                _ => j += 1,
            }
        }
        b.len()
    };
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    // (is_object, has_emitted_a_member)
    let mut stack: Vec<(bool, bool)> = Vec::new();
    let mut expect_key = false;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'{' => {
                out.push(b'{');
                stack.push((true, false));
                expect_key = true;
                i += 1;
            }
            b'[' => {
                out.push(b'[');
                stack.push((false, false));
                expect_key = false;
                i += 1;
            }
            c @ (b'}' | b']') => {
                out.push(c);
                stack.pop();
                expect_key = false;
                i += 1;
            }
            b',' => {
                i += 1;
                if stack.last().is_some_and(|t| t.0) {
                    expect_key = true; // comma deferred until a member survives
                } else {
                    out.push(b',');
                }
            }
            b'"' if expect_key => {
                let colon = string_end(i);
                let value = colon + 1;
                expect_key = false;
                if b.get(value..value + 4) == Some(b"null") {
                    i = value + 4;
                    continue;
                }
                if let Some(top) = stack.last_mut() {
                    if top.1 {
                        out.push(b',');
                    }
                    top.1 = true;
                }
                out.extend_from_slice(&b[i..value.min(b.len())]);
                i = value;
            }
            b'"' => {
                let end = string_end(i);
                out.extend_from_slice(&b[i..end]);
                i = end;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| json.to_string())
}

/// Enums that flux-core serialises as snake_case strings (`Container`,
/// `HdrFormat`, `Codec`) are stored as that same string.
pub(crate) fn enum_to_text<T: Serialize>(v: &T) -> Option<String> {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => Some(s),
        _ => None,
    }
}

/// Inverse of [`enum_to_text`]; unknown strings become `None`.
pub(crate) fn enum_from_text<T: for<'de> Deserialize<'de>>(s: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(s.to_owned())).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u64_bit_patterns_round_trip() {
        for v in [0u64, 1, i64::MAX as u64, u64::MAX, 0x8000_0000_0000_0001] {
            assert_eq!(u64_from_sql(u64_to_sql(v)), v);
        }
        let fp = 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210u128;
        assert_eq!(fp128_from_blob(&fp128_to_blob(fp)), Some(fp));
        assert_eq!(fp128_from_blob(&[1, 2, 3]), None);
    }

    /// Reference implementation: strip via a `Value` tree.
    fn strip_via_value(json: &str) -> String {
        fn strip(v: &mut serde_json::Value) {
            match v {
                serde_json::Value::Object(m) => {
                    m.retain(|_, x| !x.is_null());
                    m.values_mut().for_each(strip);
                }
                serde_json::Value::Array(a) => a.iter_mut().for_each(strip),
                _ => {}
            }
        }
        let mut v: serde_json::Value = serde_json::from_str(json).unwrap();
        strip(&mut v);
        serde_json::to_string(&v).unwrap()
    }

    #[test]
    fn null_stripper_matches_value_tree_reference() {
        let cases = [
            r#"{"a":null,"b":1,"c":null}"#,
            r#"{"a":null}"#,
            r#"{}"#,
            r#"[null,{"x":null,"y":[null,1]},"null"]"#,
            r#"{"k\"ey":null,"s":"a,\"b\":null,{}","n":{"m":null,"z":{"q":null}},"t":true}"#,
            r#"{"é":"ü","a":[{"b":null,"c":"é"}],"d":null}"#,
            r#"null"#,
            r#"{"nullable":"null","x":[]}"#,
        ];
        for c in cases {
            // serde_json::Map is sorted, so compare as parsed values.
            let ours: serde_json::Value = serde_json::from_str(&strip_null_members(c)).unwrap();
            let reference: serde_json::Value = serde_json::from_str(&strip_via_value(c)).unwrap();
            assert_eq!(ours, reference, "case {c}");
        }
    }

    #[test]
    fn compact_json_round_trips_probe_and_guess() {
        let probe = crate::synth::seed_probes().remove("hevc_mkv").unwrap();
        let full = serde_json::to_string(&probe).unwrap();
        let compact = compact_json(&probe).unwrap();
        assert!(
            compact.len() * 3 < full.len() * 2,
            "{} vs {}",
            compact.len(),
            full.len()
        );
        assert_eq!(serde_json::from_str::<ProbeResult>(&compact).unwrap(), probe);
        let mut g = NameGuess::unknown();
        g.title = Some("Heat".into());
        g.episodes = vec![1, 2];
        assert_eq!(
            serde_json::from_str::<NameGuess>(&compact_json(&g).unwrap()).unwrap(),
            g
        );
    }

    #[test]
    fn enum_text_matches_serde_names() {
        assert_eq!(enum_to_text(&Container::Matroska).as_deref(), Some("matroska"));
        assert_eq!(
            enum_to_text(&HdrFormat::DolbyVision).as_deref(),
            Some("dolby_vision")
        );
        assert_eq!(enum_from_text::<HdrFormat>("hdr10"), Some(HdrFormat::Hdr10));
        assert_eq!(enum_from_text::<HdrFormat>("nope"), None);
    }

    #[test]
    fn same_stat_ignores_unknown_inodes() {
        let row = FileRow {
            id: 1,
            dev: Some(1),
            ino: Some(42),
            size: 10,
            mtime_ns: 5,
            btime_ns: None,
            fp128: None,
            state: FileState::Present,
            probe_version: Some(1),
            parse_version: Some(1),
            asset_id: None,
            probe_retry_at: None,
        };
        let mut f = FileFacts {
            rel_path: "a.mkv".into(),
            dev: Some(1),
            ino: Some(42),
            size: 10,
            mtime_ns: 5,
            ..Default::default()
        };
        assert!(row.same_stat(&f));
        f.ino = Some(43);
        assert!(!row.same_stat(&f));
        f.ino = None;
        assert!(row.same_stat(&f));
        f.mtime_ns = 6;
        assert!(!row.same_stat(&f));
    }
}
