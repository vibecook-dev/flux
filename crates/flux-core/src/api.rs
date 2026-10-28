//! JSON shapes of flux's HTTP API (v1), shared by `flux-server` and the web
//! client (`web/src/api/types.ts` mirrors these by hand; keep them in sync).
//!
//! Routes (all JSON unless noted; ids are integers):
//!
//! | Method | Path | Body → Response |
//! |---|---|---|
//! | GET  | `/api/v1/server` | → [`ServerInfo`] |
//! | GET  | `/api/v1/libraries` | → `Vec<`[`Library`]`>` |
//! | POST | `/api/v1/libraries` | [`CreateLibrary`] → [`Library`] |
//! | POST | `/api/v1/libraries/{id}/scan` | → [`ScanStatus`] |
//! | GET  | `/api/v1/libraries/{id}/items?cursor=&limit=&sort=&filter=` | [`ItemsParams`] → [`Page`]`<`[`ItemSummary`]`>` |
//! | GET  | `/api/v1/libraries/{id}/folders?folder=&cursor=&limit=&sort=` | [`FolderParams`] → [`FolderPage`] (a videos library by folder: a folder's subfolders and the videos directly in it; 400 for other kinds) |
//! | GET  | `/api/v1/home` | → [`Home`] (continue watching, recently added, per-library rows) |
//! | GET  | `/api/v1/items/{id}` | → [`ItemDetail`] |
//! | GET  | `/api/v1/items/{id}/children` | → `Vec<`[`ItemSummary`]`>` (seasons of a show, episodes of a season) |
//! | POST | `/api/v1/items/{id}/watched` | [`MarkWatched`] → 204 (a show or season marks every episode) |
//! | GET  | `/api/v1/search?q=` | → `Vec<`[`ItemSummary`]`>` |
//! | POST | `/api/v1/play` | [`PlayRequest`] → [`PlayResponse`] |
//! | POST | `/api/v1/plan` | [`PlayRequest`] → [`PlanPreview`] (dry run: no session, no URLs) |
//! | POST | `/api/v1/sessions/{id}/progress` | [`ProgressReport`] → 204 (≥ 90 % of the duration marks the item watched) |
//! | GET  | `/api/v1/sessions/{id}/diagnostics` | → [`SessionDiagnostics`] |
//! | GET  | `/api/v1/tailnet` | → [`TailnetInfo`] (remote-access onboarding) |
//! | POST | `/api/v1/tailnet/connect` | [`TailnetConnect`] → [`TailnetInfo`] (Owner/Admin only) |
//! | GET  | `/api/v1/events` | Server-Sent Events, each `data:` is an [`Event`] |
//! | GET  | `/api/v1/art/{art_id}?w=` | image bytes (JPEG/WebP) |
//! | GET  | `/media/{asset_id}/file` | original file, HTTP Range |
//! | GET  | `/media/{plan_id}/master.m3u8` | HLS multivariant playlist |
//! | GET  | `/media/{plan_id}/v/index.m3u8`, `/v/init.mp4`, `/v/{n}.m4s` | video rendition |
//! | GET  | `/media/{plan_id}/a/{rendition}/index.m3u8`, `/init.mp4`, `/{n}.m4s` | audio renditions (below) |
//! | GET  | `/media/{plan_id}/s/{track}.m3u8` → `/s/{track}.vtt` | WebVTT subtitles: a one-segment playlist (HLS) and the file itself (`<track>` sidecar, [`SubtitleRef::url`]); `{track}` is a text track of the file or a text subtitle file beside it ([`AssetInfo::subtitle_files`]) |
//!
//! Audio rendition keys (`{rendition}`, as `flux_hls::AudioKey` parses them):
//! `{track}` is the source track copied as-is; `{track}-aac2` (stereo AAC) and
//! `{track}-eac36` (E-AC-3 5.1) are transcodes of it. One source track can be
//! offered several ways in the same plan, so the key is not just the track index.
//! Players find them in the multivariant playlist; [`PlayResponse::audio`]
//! lists them for a player's own audio menu. Clients never build them.
//!
//! Conventions: every `Option` field is always serialized (`null`, never
//! omitted; the web mirror declares `T | null` keys). Times are Unix ms.
//! Errors (4xx/5xx) carry a plain-text message meant for the user
//! (`text/plain`, `X-Content-Type-Options: nosniff`), never JSON. Anonymous
//! callers (the LAN listener, unknown tailnet peers) share one guest user,
//! so progress and "continue watching" are shared between them.
//!
//! Cross-site protection (every listener; docs/prototype/server.md):
//! - a request whose `Host` is not one of the server's names gets **421**
//!   (DNS rebinding);
//! - an unsafe call (`POST` …) to `/api/` needs the header
//!   [`REQUEST_HEADER`]` = 1`, and a present `Origin` must name one of the
//!   server's hosts; otherwise **403**. `EventSource`, media, HLS and art
//!   `GET`s need nothing;
//! - **403** also means "your role may not do this" (Owner/Admin routes).

use serde::{Deserialize, Serialize};

/// The header every unsafe `/api/` call must carry, with the value `1`: a
/// page on another site can't send it without a CORS preflight, which flux
/// never grants.
pub const REQUEST_HEADER: &str = "x-flux-request";

use crate::media::{Codec, Container, HdrFormat, ProbeResult};
use crate::names::LibraryKind;
use crate::plan::{ClientCaps, PlaybackPlan, SubtitleFile};

pub type Id = i64;

/// The tailnet identity of the caller, from Tailscale WhoIs. `None` on the
/// LAN listener or when the caller is anonymous.
///
/// What `flux-net` attaches per listener:
/// - **localhost:** a synthetic identity (login = the node owner, or `"local"`)
///   with role Owner by default; `RolePolicy::localhost_role = None` makes
///   localhost anonymous (multi-user hosts).
/// - **LAN:** `None` (no pairing yet; the server decides what anonymous may do).
/// - **tailnet:** WhoIs, with the role from the `vibecook.dev/cap/flux` grant,
///   local overrides, node ownership, then the tagged/default role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub login_name: String,
    pub display_name: String,
    pub profile_pic_url: Option<String>,
    pub node_name: Option<String>,
    /// flux role derived from tailnet app capabilities or local settings.
    pub role: Role,
    /// Tailscale ACL tags of the calling device (`tag:tv`, …), from WhoIs.
    /// Non-empty means a tag-owned device (shared TV, server) with no human
    /// owner. Empty for localhost and for user-owned devices.
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Role {
    Owner,
    Admin,
    Viewer,
    Kid,
    Guest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    /// Who the server thinks you are (from Tailscale WhoIs), if known.
    pub you: Option<Identity>,
    /// How you reached the server: "localhost", "lan", "tailnet-direct",
    /// "tailnet-relay", or "tailnet" (tailnet, path unknown: embedded mode can
    /// only classify peers that are themselves Truffle nodes). The strings are
    /// `flux_net::NetPath::as_str()`.
    pub path: String,
    /// MagicDNS name when the server is on a tailnet, e.g. "flux.tail1234.ts.net".
    pub tailnet_name: Option<String>,
    /// Where titles and artwork come from.
    pub metadata: MetadataInfo,
    /// The server's HTTPS address on the tailnet, e.g.
    /// `https://flux.tail1234.ts.net:8096`, while HTTPS is on and its
    /// certificate is loaded ([`TailnetInfo::https`]). A browser that opened
    /// flux over plain HTTP on the tailnet can move there: browsers offer
    /// some features (WebGPU) only to secure pages.
    #[serde(default)]
    pub https_url: Option<String>,
    /// The web client this server serves: its build id, from the
    /// `<meta name="flux-build">` the build stamps into its `index.html`. An
    /// open page compares it with its own to know flux was updated under it
    /// and offer a reload. `None` without a built client (or one too old to
    /// carry the id).
    #[serde(default)]
    pub web_build: Option<String>,
}

/// [`ServerInfo::metadata`]: what the server can match titles against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataInfo {
    pub source: MetadataSource,
}

/// The metadata source background matching uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum MetadataSource {
    /// None: titles and years come from file and folder names, artwork from
    /// frames of the video.
    None,
    /// TMDB online, with the server's key (`TMDB_READ_TOKEN` or
    /// `TMDB_API_KEY`).
    Tmdb,
    /// TMDB's daily ID exports, offline: they suggest candidates but accept
    /// none on their own (no year or runtime to confirm a match).
    Exports,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateLibrary {
    pub name: String,
    pub kind: LibraryKind,
    pub roots: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Library {
    pub id: Id,
    pub name: String,
    pub kind: LibraryKind,
    pub roots: Vec<RootStatus>,
    pub item_count: u64,
    pub scan: ScanStatus,
}

/// One folder of a library and whether flux can reach it. An unreachable
/// root keeps its items (they show as unavailable), never deletes them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootStatus {
    /// As the user added it, e.g. `/srv/media/Movies`.
    pub path: String,
    /// `false` while the root doesn't answer (NAS asleep, drive unplugged).
    pub available: bool,
    /// When flux last confirmed the root reachable: the later of the last
    /// finished scan and the moment it last came back online. `None` = never
    /// confirmed (not scanned yet). For an unavailable root it is a lower
    /// bound of the last sighting ("not responding since …" is at least this).
    pub last_seen: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScanStatus {
    pub state: ScanState,
    pub files_seen: u64,
    /// Files the current scan will look at, known once the directory walk is
    /// done (`None` while walking). With `files_seen` it gives a progress
    /// fraction. For a library at rest it equals `files_seen`.
    pub files_total: Option<u64>,
    pub files_probed: u64,
    pub items_added: u64,
    pub items_updated: u64,
    pub items_removed: u64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub last_error: Option<String>,
    /// Directories that didn't answer before their deadline in the last scan
    /// (Mille `ScanReport::unread`). Nothing under them was marked missing;
    /// the UI says "N folders didn't respond".
    pub dirs_unavailable: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ScanState {
    Idle,
    Scanning,
    Watching,
    /// A root is unreachable (NAS asleep, drive unplugged). Items are kept.
    Unavailable,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Opaque cursor for the next page; `None` at the end.
    pub next: Option<String>,
    pub total: Option<u64>,
}

/// Query parameters of `GET /api/v1/libraries/{id}/items`. Every field is
/// optional in the query string.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ItemsParams {
    /// Opaque cursor from the previous page's [`Page::next`].
    pub cursor: Option<String>,
    /// Page size; the server clamps it (flux-catalog: 1..=500, default 100).
    pub limit: Option<u32>,
    /// `None` = [`ItemSort::Title`].
    pub sort: Option<ItemSort>,
    /// `None` = everything.
    pub filter: Option<ItemFilter>,
}

/// Library list filters (`?filter=`). Wire values are the web client's chips:
/// `unwatched`, `in_progress`, `4k`, `hdr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ItemFilter {
    /// Movies not watched; shows with at least one unwatched episode.
    #[serde(rename = "unwatched")]
    Unwatched,
    /// Started but not finished (a show: any episode in progress).
    #[serde(rename = "in_progress")]
    InProgress,
    /// Has a 2160p (or larger) version: the `4K` badge.
    #[serde(rename = "4k")]
    Uhd,
    /// Has an HDR version (HDR10, HDR10+, HLG or Dolby Vision badges).
    #[serde(rename = "hdr")]
    Hdr,
}

/// Sort orders for library item lists. On the wire a leading `-` reverses the
/// order: `title`, `-title`, `year`, `-year`, `added`, `-added` (the values
/// the web client sends).
///
/// flux-catalog implements all six with keyset cursors (ties break on sort
/// title, then id; unknown years sort last), and `flux_catalog::ItemSort` is
/// this type, so the server passes it straight through.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ItemSort {
    /// By sort title (articles ignored, numbers in natural order), A→Z.
    #[default]
    #[serde(rename = "title")]
    Title,
    #[serde(rename = "-title")]
    TitleDesc,
    /// Oldest release year first.
    #[serde(rename = "year")]
    Year,
    #[serde(rename = "-year")]
    YearDesc,
    /// Oldest addition first.
    #[serde(rename = "added")]
    Added,
    /// Newest addition first ("recently added").
    #[serde(rename = "-added")]
    AddedDesc,
}

/// Query parameters of `GET /api/v1/libraries/{id}/folders`, the folder
/// view of a videos library ([`LibraryKind::Videos`]), where every file is
/// a video of its own. A library of another kind answers 400: there a
/// show's episodes and a film's extras are no top-level items, so its
/// folders would list few of their files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FolderParams {
    /// The folder: a [`FolderRef::folder`] from an earlier answer. `None`
    /// (or empty) = the library's top: its folder when it has one, one
    /// subfolder per folder when it has several.
    pub folder: Option<String>,
    /// Opaque cursor from the previous page's [`FolderPage::next`].
    pub cursor: Option<String>,
    /// Page size of [`FolderPage::items`]; the server clamps it (1..=500,
    /// default 100).
    pub limit: Option<u32>,
    /// `None` = [`FolderSort::Title`].
    pub sort: Option<FolderSort>,
}

/// Orders of a folder's videos. On the wire a leading `-` reverses:
/// `title`, `-title`, `date`, `-date`. Subfolders are always by name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FolderSort {
    /// By name, numbers in natural order (`clip 2` before `clip 10`).
    #[default]
    #[serde(rename = "title")]
    Title,
    #[serde(rename = "-title")]
    TitleDesc,
    /// Oldest first, by the file's modification time (when a home video
    /// was recorded or saved, for most files).
    #[serde(rename = "date")]
    Date,
    /// Newest first.
    #[serde(rename = "-date")]
    DateDesc,
}

/// A folder of a library: what to call it and how to open it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderRef {
    /// Opaque; pass it back as [`FolderParams::folder`]. `""` is the top.
    pub folder: String,
    /// The folder's own name (for one of a library's folders, the last
    /// part of its path; for the top, the library's name).
    pub name: String,
}

/// A subfolder in a [`FolderPage`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderEntry {
    /// Opaque; pass it back as [`FolderParams::folder`].
    pub folder: String,
    pub name: String,
    /// Videos in it and in its subfolders.
    pub item_count: u64,
}

/// `GET /api/v1/libraries/{id}/folders`: one folder of a library.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FolderPage {
    /// This folder (`folder` `""` and the library's name at the top).
    pub at: FolderRef,
    /// The folders above this one, top first, for breadcrumbs: empty at the
    /// top, else the top and every folder down to this one's parent.
    pub parents: Vec<FolderRef>,
    /// Subfolders that hold videos, by name. Every page repeats them.
    pub folders: Vec<FolderEntry>,
    /// A page of the videos directly in this folder (top-level items with
    /// a file here), in [`FolderParams::sort`] order.
    pub items: Vec<ItemSummary>,
    /// Cursor for the next page of `items`; `None` at the end.
    pub next: Option<String>,
    /// Videos directly in this folder, on every page.
    pub total: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ItemKind {
    Movie,
    Show,
    Season,
    Episode,
    Extra,
}

/// A reference to an image. `url` is ready to use; append `?w=` to resize.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtRef {
    pub url: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Tiny placeholder rendered while the image loads.
    pub blurhash: Option<String>,
    /// Where the image came from; the UI must label `frame` and `generated`.
    pub source: ArtSource,
    /// Who to credit, for provider art: `"TMDB"`, `"fanart.tv"`. `None` for
    /// local, frame and generated art.
    pub attribution: Option<String>,
    /// The model that made a [`ArtSource::Generated`] image (e.g.
    /// `"gpt-image-1"`), shown next to the "AI-generated" label. `None` otherwise.
    pub generator: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ArtSource {
    /// From a metadata provider (TMDB, fanart.tv…), with attribution.
    Provider,
    /// A poster/thumb file next to the media.
    Local,
    /// A frame picked from the video plus typeset title (no AI).
    Frame,
    /// AI-generated; always labelled as such in the UI.
    Generated,
    /// Chosen by you or your agent (`art.add`): found elsewhere and credited
    /// ([`ArtRef::attribution`]), or made without AI. Shown over every
    /// other image of its kind.
    Custom,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    pub position_ms: u64,
    pub duration_ms: u64,
    pub watched: bool,
    pub updated_at: i64,
}

/// An item as lists show it. The same item reads the same in every list
/// (home rows, library pages, children, search).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemSummary {
    pub id: Id,
    pub kind: ItemKind,
    /// The item's own name: the movie or show title, the season name
    /// ("Season 2", "Specials"), the episode title.
    pub title: String,
    /// The show's name for seasons and episodes; the parent work's title for
    /// extras. `None` for movies and shows. Saves a lookup per card.
    pub parent_title: Option<String>,
    pub year: Option<u16>,
    /// "S01E03" for episodes, season name for seasons.
    pub subtitle: Option<String>,
    pub poster: Option<ArtRef>,
    pub backdrop: Option<ArtRef>,
    pub progress: Option<Progress>,
    /// Short quality badges: "4K", "HDR10", "DV", "Atmos", "5.1".
    pub badges: Vec<String>,
    /// Number of children (seasons/episodes) for shows and seasons.
    pub child_count: Option<u32>,
    /// Shows and seasons: episodes the caller hasn't watched (all of them for
    /// an anonymous caller). `None` for other kinds.
    pub unwatched_count: Option<u32>,
    /// `false` when no file of the item (for shows and seasons: of any
    /// episode) is on a reachable root. The item stays listed; Play answers 409.
    pub available: bool,
    pub added_at: i64,
}

/// One playable file of an item (an item can have several: 4K and 1080p, editions).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetInfo {
    pub id: Id,
    pub path: String,
    pub size: u64,
    pub container: Container,
    pub duration_ms: Option<u64>,
    pub edition: Option<String>,
    /// "4K HEVC HDR10 · TrueHD 7.1"
    pub label: String,
    pub hdr: Option<HdrFormat>,
    /// `false` when the source root is offline.
    pub available: bool,
    pub probe: Option<ProbeResult>,
    /// Subtitle files beside the video (`Film.en.srt`, an episode's `.sup`),
    /// offered like its subtitle tracks: pass a file's `track` as
    /// [`PlayRequest::subtitle_track`]. Text files play as WebVTT, picture
    /// files (PGS, VobSub) are drawn into the video.
    #[serde(default)]
    pub subtitle_files: Vec<SubtitleFile>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemDetail {
    #[serde(flatten)]
    pub summary: ItemSummary,
    pub overview: Option<String>,
    pub genres: Vec<String>,
    pub runtime_ms: Option<u64>,
    pub assets: Vec<AssetInfo>,
    /// For episodes/seasons: the parent show id.
    pub show_id: Option<Id>,
    pub season_number: Option<u16>,
    pub episode_numbers: Vec<u16>,
    /// Provider attribution / ids for the "matched as" panel.
    pub external_ids: Vec<(String, String)>,
    /// How confident the match is, and why (explainable matching).
    pub match_confidence: Option<f32>,
    pub match_evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Home {
    pub continue_watching: Vec<ItemSummary>,
    pub recently_added: Vec<ItemSummary>,
    pub rows: Vec<HomeRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HomeRow {
    pub title: String,
    pub library_id: Option<Id>,
    pub items: Vec<ItemSummary>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayRequest {
    /// Item to play; the server picks the best asset unless `asset_id` is set.
    pub item_id: Id,
    pub asset_id: Option<Id>,
    pub caps: ClientCaps,
    /// Preferred audio track index (source file order).
    pub audio_track: Option<u32>,
    /// A subtitle track of the file, or a file's [`SubtitleFile::track`]. A
    /// picture subtitle the client can't render is drawn into the video.
    pub subtitle_track: Option<u32>,
    /// Start position; `None` = resume point (or 0).
    pub start_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayResponse {
    pub session_id: String,
    pub asset_id: Id,
    pub plan: PlaybackPlan,
    /// What to hand the player: an HLS multivariant playlist or a progressive URL.
    pub url: String,
    /// Human sentences from `plan.reasons`, ready to show.
    pub explain: Vec<String>,
    /// Where to start (resume point), in ms.
    pub start_ms: u64,
    pub duration_ms: Option<u64>,
    /// URLs worth fetching right away (init + first segment) to cut TTFF.
    pub prefetch: Vec<String>,
    /// Audio renditions of the HLS stream, in playlist order, for the
    /// player's own audio menu. Empty for progressive (direct) play.
    pub audio: Vec<AudioRef>,
    /// Subtitle renditions available as WebVTT sidecars.
    pub subtitles: Vec<SubtitleRef>,
}

/// One audio rendition of a plan. A source track can appear several times
/// (copied, and transcoded to stereo AAC / E-AC-3 5.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioRef {
    /// The rendition key in media URLs (`a/{rendition}/index.m3u8`):
    /// `"{track}"` copied, `"{track}-aac2"`, `"{track}-eac36"` (see the module docs).
    pub rendition: String,
    /// Source track index.
    pub track: u32,
    /// "English (TrueHD 7.1)", "English (AAC Stereo)": the playlist's NAME.
    pub label: String,
    pub language: Option<String>,
    /// What the client receives (the transcode target, or the source codec).
    pub codec: Codec,
    pub channels: u8,
    /// The rendition the stream starts with.
    pub default: bool,
}

/// `POST /api/v1/plan`: what `POST /play` would do for these caps, without
/// opening a session (the item page's "Expected on this device").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanPreview {
    /// The asset the server picked (or `PlayRequest::asset_id`).
    pub asset_id: Id,
    pub plan: PlaybackPlan,
    /// [`PlaybackPlan::explain`].
    pub explain: Vec<String>,
    /// [`PlaybackPlan::label`]: "Direct play", "Direct stream", "Audio conversion", "Transcode".
    pub label: String,
}

impl PlanPreview {
    pub fn new(asset_id: Id, plan: PlaybackPlan) -> PlanPreview {
        PlanPreview {
            asset_id,
            explain: plan.explain(),
            label: plan.label().to_string(),
            plan,
        }
    }
}

/// `POST /api/v1/items/{id}/watched`. Watching also happens implicitly: a
/// session's progress report at ≥ 90 % of the duration marks the item watched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkWatched {
    /// `true` = watched (resume point cleared); `false` = unwatched.
    pub watched: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubtitleRef {
    pub track: u32,
    pub label: String,
    pub language: Option<String>,
    pub url: String,
    pub forced: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgressReport {
    pub position_ms: u64,
    pub paused: bool,
    /// Client-measured timings for diagnostics (all optional).
    pub ttff_ms: Option<u32>,
    pub last_seek_ms: Option<u32>,
    pub stalls: Option<u32>,
    pub dropped_frames: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionDiagnostics {
    pub session_id: String,
    pub plan: PlaybackPlan,
    pub explain: Vec<String>,
    pub path: String,
    pub segments_served: u64,
    pub bytes_served: u64,
    /// Server-side time to produce each of the last segments, ms.
    pub segment_ms_p50: Option<f32>,
    pub segment_ms_p95: Option<f32>,
    pub client: Option<ProgressReport>,
}

/// Server-Sent Events payloads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    ScanProgress {
        library_id: Id,
        status: ScanStatus,
    },
    ItemsChanged {
        library_id: Id,
        added: Vec<Id>,
        updated: Vec<Id>,
        removed: Vec<Id>,
    },
    SourceAvailability {
        library_id: Id,
        root: String,
        available: bool,
    },
    /// Remote-access state changed (login URL ready, running, failed…).
    Tailnet {
        info: TailnetInfo,
    },
    /// A changeset was made, applied, undone or turned down (an agent's or
    /// a person's edits; the app's Changes screen follows these).
    Changes {
        changeset: crate::agent::ChangesetSummary,
    },
    /// The lenses kept on the server changed (`lens.add`, `lens.remove`).
    Lenses,
    /// The collections kept on the server changed (`collection.put`,
    /// `collection.remove`).
    Collections,
}

/// How flux reaches the tailnet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TailnetMode {
    /// No tailnet listener.
    Off,
    /// flux's own node via Truffle ("Connect with Tailscale").
    Embedded,
    /// This machine's tailscaled.
    System,
}

/// Remote-access state (`flux_net::TailnetStatus`, flattened).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TailnetState {
    /// Not configured.
    Off,
    /// Starting the node / looking for tailscaled.
    Starting,
    /// Show [`TailnetInfo::auth_url`] (link + QR) so the owner can sign flux in.
    NeedsLogin,
    /// The tailnet requires device approval in the admin console.
    NeedsApproval,
    /// Serving on the tailnet at [`TailnetInfo::url`].
    Running,
    /// System mode: tailscaled is missing, stopped or logged out (flux keeps checking).
    Unavailable,
    /// Gave up; see [`TailnetInfo::last_error`].
    Error,
}

/// `GET /api/v1/tailnet`, `POST /api/v1/tailnet/connect`, [`Event::Tailnet`].
/// flux-net builds it (`TailnetStatus::to_api`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TailnetInfo {
    pub mode: TailnetMode,
    pub state: TailnetState,
    /// Interactive login URL while `needs_login`.
    pub auth_url: Option<String>,
    /// Admin-console page to approve the device while `needs_approval`.
    pub approve_url: Option<String>,
    /// The tailnet's DNS suffix, e.g. `tail1234.ts.net`.
    pub tailnet_name: Option<String>,
    /// This server's MagicDNS name, e.g. `flux.tail1234.ts.net`.
    pub dns_name: Option<String>,
    /// Where tailnet devices open flux, e.g. `http://flux.tail1234.ts.net:8096`.
    pub url: Option<String>,
    /// The login that owns the node (the flux Owner).
    pub owner_login: Option<String>,
    /// When the node key expires (Unix ms); `None` = unknown or expiry disabled.
    pub key_expiry: Option<i64>,
    /// A system tailscaled is available on this machine (offer "use this
    /// machine's Tailscale" as an alternative).
    pub system_tailscale: bool,
    /// Why the state is `unavailable` or `error`.
    pub last_error: Option<String>,
    /// HTTPS on the tailnet is switched on: tailnet devices can open flux at
    /// `https://` with the tailnet's MagicDNS certificate (needs HTTPS
    /// certificates enabled for the tailnet). [`url`](Self::url) is the
    /// `https://` address once the certificate is loaded. With this
    /// machine's Tailscale, plain HTTP keeps working on the same port.
    #[serde(default)]
    pub https: bool,
    /// Why HTTPS is on but not serving yet, e.g. the tailnet has no HTTPS
    /// certificates.
    #[serde(default)]
    pub https_error: Option<String>,
}

/// `POST /api/v1/tailnet/connect` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TailnetConnect {
    /// `embedded` = "Connect with Tailscale", `system` = use this machine's
    /// tailscaled, `off` = disconnect.
    pub mode: TailnetMode,
    /// Switch HTTPS on the tailnet on or off ([`TailnetInfo::https`]); left
    /// as it is when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub https: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_sort_wire_values_match_the_web_client() {
        // web/src/api/types.ts `LIBRARY_SORTS`.
        let all = [
            ItemSort::Title,
            ItemSort::TitleDesc,
            ItemSort::Year,
            ItemSort::YearDesc,
            ItemSort::Added,
            ItemSort::AddedDesc,
        ];
        let wire: Vec<String> = all.iter().map(|s| serde_json::to_string(s).unwrap()).collect();
        assert_eq!(
            wire,
            [
                "\"title\"",
                "\"-title\"",
                "\"year\"",
                "\"-year\"",
                "\"added\"",
                "\"-added\""
            ]
        );
        for s in all {
            let back: ItemSort = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
            assert_eq!(back, s);
        }
    }

    #[test]
    fn items_params_fields_are_optional() {
        let p: ItemsParams = serde_json::from_str(r#"{"sort":"-added","limit":50}"#).unwrap();
        assert_eq!(
            p,
            ItemsParams {
                sort: Some(ItemSort::AddedDesc),
                limit: Some(50),
                cursor: None,
                filter: None,
            }
        );
        assert_eq!(
            serde_json::from_str::<ItemsParams>("{}").unwrap(),
            ItemsParams::default()
        );
    }
    #[test]
    fn item_filter_wire_values_match_the_web_chips() {
        let all = [
            ItemFilter::Unwatched,
            ItemFilter::InProgress,
            ItemFilter::Uhd,
            ItemFilter::Hdr,
        ];
        let wire: Vec<String> = all.iter().map(|f| serde_json::to_string(f).unwrap()).collect();
        assert_eq!(wire, ["\"unwatched\"", "\"in_progress\"", "\"4k\"", "\"hdr\""]);
        let p: ItemsParams = serde_json::from_str(r#"{"filter":"4k"}"#).unwrap();
        assert_eq!(p.filter, Some(ItemFilter::Uhd));
    }

    #[test]
    fn folder_sorts_and_params_match_the_web_client() {
        // web/src/api/types.ts `FOLDER_SORTS`.
        let all = [
            FolderSort::Title,
            FolderSort::TitleDesc,
            FolderSort::Date,
            FolderSort::DateDesc,
        ];
        let wire: Vec<String> = all.iter().map(|s| serde_json::to_string(s).unwrap()).collect();
        assert_eq!(wire, ["\"title\"", "\"-title\"", "\"date\"", "\"-date\""]);
        let p: FolderParams = serde_json::from_str(r#"{"folder":"3/a/b","sort":"-date"}"#).unwrap();
        assert_eq!(
            p,
            FolderParams {
                folder: Some("3/a/b".into()),
                sort: Some(FolderSort::DateDesc),
                ..FolderParams::default()
            }
        );
        assert_eq!(
            serde_json::from_str::<FolderParams>("{}").unwrap(),
            FolderParams::default()
        );
        let lib: CreateLibrary =
            serde_json::from_str(r#"{"name":"Home","kind":"videos","roots":["/v"]}"#).unwrap();
        assert_eq!(lib.kind, LibraryKind::Videos);
    }

    #[test]
    fn tailnet_dtos_are_snake_case_and_tagged() {
        let info = TailnetInfo {
            mode: TailnetMode::Embedded,
            state: TailnetState::NeedsLogin,
            auth_url: Some("https://login.tailscale.com/a/x".into()),
            approve_url: None,
            tailnet_name: None,
            dns_name: None,
            url: None,
            owner_login: None,
            key_expiry: None,
            system_tailscale: false,
            last_error: None,
            https: false,
            https_error: None,
        };
        let v = serde_json::to_value(Event::Tailnet { info: info.clone() }).unwrap();
        assert_eq!(v["type"], "tailnet");
        assert_eq!(v["info"]["state"], "needs_login");
        assert_eq!(v["info"]["mode"], "embedded");
        // Option fields are present as null, never omitted.
        assert!(v["info"].as_object().unwrap().contains_key("dns_name"));
        let back: Event = serde_json::from_value(v).unwrap();
        assert_eq!(back, Event::Tailnet { info: info.clone() });
        let c: TailnetConnect = serde_json::from_str(r#"{"mode":"system"}"#).unwrap();
        assert_eq!((c.mode, c.https), (TailnetMode::System, None));
        let c: TailnetConnect = serde_json::from_str(r#"{"mode":"system","https":true}"#).unwrap();
        assert_eq!(c.https, Some(true));
        // `https` is left out when not changing it, so older servers accept the body.
        let wire = serde_json::to_value(TailnetConnect {
            mode: TailnetMode::Off,
            https: None,
        })
        .unwrap();
        assert!(!wire.as_object().unwrap().contains_key("https"));
        // A server from before HTTPS: the new fields default.
        let old: TailnetInfo = serde_json::from_value({
            let mut v = serde_json::to_value(&info).unwrap();
            let o = v.as_object_mut().unwrap();
            o.remove("https");
            o.remove("https_error");
            v
        })
        .unwrap();
        assert!(!old.https && old.https_error.is_none());
    }

    #[test]
    fn plan_preview_carries_explain_and_label() {
        use crate::plan::{AudioOp, AudioPlan, DeliveryFormat, Reason, VideoOp};
        use crate::Codec;
        let plan = PlaybackPlan {
            delivery: DeliveryFormat::HlsFmp4,
            video_track: Some(0),
            video: VideoOp::Copy { fixes: vec![] },
            audio: vec![AudioPlan {
                track: 1,
                op: AudioOp::Transcode {
                    codec: Codec::Aac,
                    channels: 2,
                    bitrate: 192_000,
                },
            }],
            subtitles: vec![],
            demuxed_audio: true,
            reasons: vec![Reason::CodecNotPackageable { codec: Codec::TrueHd }],
            est_cpu_ms_per_min: 300,
        };
        let p = PlanPreview::new(7, plan);
        assert_eq!(p.label, "Audio conversion");
        assert_eq!(
            p.explain,
            ["TrueHD can't be carried in an HLS stream, so flux converts it"]
        );
    }
}
