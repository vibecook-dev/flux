//! The agent contract, `flux.agent/1`: what flux's operations take and
//! return. The same operations are served over HTTP (`POST
//! /api/v1/ops/{name}`), by the `flux` command line and over MCP
//! (docs/AGENT-NATIVE.md). Their JSON Schemas are generated from these types
//! (feature `schema`), so the doc comments here are what an agent reads:
//! they say what a field means and how to use it.
//!
//! Conventions (as in [`crate::api`]): ids are integers, times are Unix
//! milliseconds, every `Option` is serialized (`null`, never omitted).
//! Inputs may leave out any field that has a default.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::{ArtSource, Id, ItemKind, MetadataSource, Role, ScanStatus};
use crate::names::{LibraryKind, NameGuess};

/// The contract these types make up. A breaking change would be
/// `flux.agent/2`, served alongside.
pub const CONTRACT: &str = "flux.agent/1";

/// The header naming the agent behind an HTTP call (`claude-code`, …). MCP
/// clients name themselves when they connect; the `flux` CLI sends this.
pub const AGENT_HEADER: &str = "x-flux-agent";

// ---- errors -------------------------------------------------------------

/// What went wrong, for a program to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ErrorCode {
    /// The input is wrong: see the message, fix it and try again.
    Invalid,
    /// No such item, image, changeset, library or operation.
    NotFound,
    /// Your role may not do this.
    Forbidden,
    /// It conflicts with how things are now (for example a field a person
    /// set, or a changeset already undone).
    Conflict,
    /// The server can't do it right now (a scan is not possible, a fetch
    /// failed); trying later may work.
    Unavailable,
    /// The input or a fetched file is too large.
    TooLarge,
    /// A bug or an I/O error on the server.
    Internal,
}

/// An operation's error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OpError {
    pub code: ErrorCode,
    /// What happened and what to do, in a sentence or two.
    pub message: String,
}

/// The body of a failed `POST /api/v1/ops/{name}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ErrorBody {
    pub error: OpError,
}

// ---- the manifest -------------------------------------------------------

/// What an operation changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Effect {
    /// Nothing.
    Read,
    /// The library's data, through a changeset you can undo (or a proposal).
    Write,
    /// Server state outside the journal: lenses, scans.
    Act,
}

/// One operation, as `GET /api/v1/agent` lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OpInfo {
    /// `items.find`, … (over MCP the dots are underscores: `items_find`).
    pub name: String,
    pub title: String,
    pub description: String,
    pub effect: Effect,
    /// Who may call it, in words.
    pub who: String,
    /// Calling it twice with the same input changes nothing more.
    pub idempotent: bool,
    /// It may replace or remove what's there (always undoable).
    pub destructive: bool,
    /// The input's JSON Schema.
    pub input: Value,
    /// The output's JSON Schema.
    pub output: Value,
}

/// A skill flux ships for agents (`skills/<name>/SKILL.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    /// Where to fetch its `SKILL.md`.
    pub url: String,
}

/// `GET /api/v1/agent`: everything an agent needs to start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Manifest {
    /// [`CONTRACT`].
    pub contract: String,
    pub server: ServerAbout,
    /// Run one with `POST /api/v1/ops/{name}` (JSON in, JSON out; send the
    /// header `X-Flux-Request: 1`, and `X-Flux-Agent: <your name>`).
    pub ops: Vec<OpInfo>,
    /// The MCP endpoint (Streamable HTTP): `/mcp`.
    pub mcp: String,
    pub skills: Vec<SkillInfo>,
    /// Schemas by name, at `/api/v1/schemas/{name}`.
    pub schemas: Vec<String>,
    pub policy: Policy,
}

// ---- status -------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ServerAbout {
    pub name: String,
    pub version: String,
}

/// What your role lets you do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct May {
    /// Change things directly: `items.edit` applies, `art.add`, `art.pin`,
    /// `changes.undo` (Owner or Admin).
    pub apply: bool,
    /// Propose changes for an Owner or Admin to review (`items.edit` with
    /// mode `propose`; Viewers, Owners and Admins).
    pub propose: bool,
    /// Scan libraries, decide proposals, keep lenses for everyone (Owner or
    /// Admin).
    pub admin: bool,
}

/// Who you are to this server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct You {
    /// Your login: a Tailscale login, `local` on the server's own machine
    /// off the tailnet, or `anonymous`.
    pub login: String,
    pub name: String,
    /// `null` when anonymous.
    pub role: Option<Role>,
    /// The agent flux takes you for (named by MCP, the `X-Flux-Agent`
    /// header or the CLI), or `null` for a person.
    pub agent: Option<String>,
    pub may: May,
}

/// What flux won't show agents, and the house rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Policy {
    /// Providers whose content flux never shows an agent, because their
    /// terms forbid using it with AI (`tmdb`). Fields from them are listed
    /// under `withheld` instead of their values: fill them from the open
    /// web, in your own words, with sources.
    pub withheld: Vec<String>,
    /// The rules, one sentence each.
    pub rules: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RootAbout {
    pub path: String,
    /// `false` while the folder doesn't answer (a NAS asleep, a drive
    /// unplugged): its items are kept, shown as offline.
    pub available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LibraryAbout {
    pub id: Id,
    pub name: String,
    /// `movies`, `shows`, `mixed`, or `videos` (home videos and clips: one
    /// item per file, never matched).
    pub kind: LibraryKind,
    /// Top-level items: films, shows, videos.
    pub items: u64,
    pub roots: Vec<RootAbout>,
    pub scanning: bool,
}

/// `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Status {
    /// [`CONTRACT`].
    pub contract: String,
    pub server: ServerAbout,
    /// What flux matches titles against by itself: `none` (titles come
    /// from file names, art from video frames: everything else is up to
    /// you), `tmdb`, or `exports` (offline suggestions only).
    pub metadata_source: MetadataSource,
    pub you: You,
    pub policy: Policy,
    pub libraries: Vec<LibraryAbout>,
}

/// Input of operations that take nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Nothing {}

// ---- where values come from ---------------------------------------------

/// Where a value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Source {
    /// The file: its name, its folder or its tags (what the scan found).
    File,
    /// A metadata provider's match (`provider` names it).
    Provider,
    /// A person (the web app, or the CLI with no agent named).
    User,
    /// An agent (`agent` names it).
    Agent,
    /// A metadata file beside the video: a `.flux.json` (`provider`:
    /// `flux`) or an `.nfo` (`provider`: `nfo`).
    Sidecar,
}

/// A web page or file that supports a value, or a link to keep. In an
/// input, a plain URL string is read as a link without a title.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Link {
    /// An `http` or `https` URL.
    pub url: String,
    /// What it is ("Wikipedia", "Official site", "Festival programme").
    #[serde(default)]
    pub title: Option<String>,
}

impl<'de> Deserialize<'de> for Link {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Url(String),
            Full {
                url: String,
                #[serde(default)]
                title: Option<String>,
            },
        }
        Ok(match Raw::deserialize(d)? {
            Raw::Url(url) => Link { url, title: None },
            Raw::Full { url, title } => Link { url, title },
        })
    }
}

/// Who set a field, when, and from what.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FieldSource {
    pub source: Source,
    /// The provider, for `provider` (`tmdb`).
    pub provider: Option<String>,
    /// The agent, for `agent` (`claude-code`).
    pub agent: Option<String>,
    /// The person who set it, or for whom the agent did (a login).
    pub by: Option<String>,
    /// The changeset that set it.
    pub changeset: Option<Id>,
    pub at: Option<i64>,
    /// What supports the value.
    pub sources: Vec<Link>,
}

// ---- fields ---------------------------------------------------------------

/// One credited performer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CastMember {
    pub name: String,
    /// The part they play, or what they do ("Narrator", "Herself").
    #[serde(default)]
    pub role: Option<String>,
}

/// The facts of an item an agent (or you) can set. In `items.edit` a field
/// left out (or `null`) is left as it is; to empty a field, or hand it back
/// to what flux finds by itself, name it in `clear`. In `item.get` every
/// field is listed with its value now (`null` when empty or withheld).
///
/// Field names (for `clear`, `sources` and `withheld`) are these keys, and
/// `ids.<provider>` for one id (`ids.tmdb`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Fields {
    /// The title to show: the film's or show's best-known title in the
    /// library's language, the season's name, the episode's title.
    pub title: Option<String>,
    /// The title in its original language and script (`千と千尋の神隠し`),
    /// when it differs. Search finds both.
    pub original_title: Option<String>,
    /// The year of first release (a show's first season).
    pub year: Option<u16>,
    /// What it's about, two to five sentences, in your own words: never
    /// copied from a provider (and never from TMDB).
    pub overview: Option<String>,
    /// A one-line hook, if it has a well-known one.
    pub tagline: Option<String>,
    /// Genres, in English, capitalised: `["Documentary", "Music"]`.
    pub genres: Option<Vec<String>>,
    /// Your own labels, for collections and search: `["Studio Ghibli",
    /// "Concert", "Festival short"]`.
    pub tags: Option<Vec<String>>,
    /// First release: `YYYY-MM-DD`, `YYYY-MM` or `YYYY`.
    pub release_date: Option<String>,
    /// Episodes: when it first aired, `YYYY-MM-DD`.
    pub air_date: Option<String>,
    /// Spoken languages, as ISO 639-1 codes where one exists (`ja`, `en`,
    /// `zh-Hant`).
    pub languages: Option<Vec<String>>,
    /// Countries of production, as ISO 3166-1 alpha-2 codes (`JP`, `FR`).
    pub countries: Option<Vec<String>>,
    pub directors: Option<Vec<String>>,
    pub writers: Option<Vec<String>>,
    /// Top-billed cast, in billing order (at most 100).
    pub cast: Option<Vec<CastMember>>,
    /// Studios, labels or (for shows) networks.
    pub studios: Option<Vec<String>>,
    /// A rating such as `PG-13`, `12`, `TV-MA`, with no country prefix.
    pub content_rating: Option<String>,
    /// Pages worth keeping with it: official site, encyclopaedia entries,
    /// an archive page, the festival listing.
    pub links: Option<Vec<Link>>,
    /// Ids in other databases, by provider: `tmdb` (number), `imdb`
    /// (`tt…`), `tvdb` (number), `wikidata` (`Q…`), or any other
    /// (`anidb`, `musicbrainz`, `letterboxd`, …). In `items.edit` the ids
    /// given are added or replaced; the others stay. With a metadata
    /// provider configured, ids you set make flux match by id instead of
    /// by title.
    pub ids: Option<BTreeMap<String, String>>,
}

impl Fields {
    /// The field names, in the order of the struct (ids are `ids.<x>`).
    pub const NAMES: [&'static str; 17] = [
        "title",
        "original_title",
        "year",
        "overview",
        "tagline",
        "genres",
        "tags",
        "release_date",
        "air_date",
        "languages",
        "countries",
        "directors",
        "writers",
        "cast",
        "studios",
        "content_rating",
        "links",
    ];
}

// ---- items ------------------------------------------------------------------

/// How flux identified an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum MatchState {
    /// Not even the name could be read.
    Unmatched,
    /// Named from the file only (or offline suggestions): nobody has
    /// confirmed what it is.
    Parsed,
    /// A metadata provider confirmed it.
    Matched,
    /// Settled by hand (flux matches it no more).
    Locked,
}

/// Something an item lacks: what `library.gaps` counts and `items.find`
/// filters by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Gap {
    /// Not identified: no provider match, and neither you nor an agent set
    /// its title or ids. Films and shows.
    Identity,
    /// No year. Films and shows.
    Year,
    /// No description. Films and shows.
    Overview,
    /// No poster but flux's own (a frame of the video, or AI art), or none.
    /// Films and shows.
    Poster,
    /// No backdrop but flux's own, or none. Films and shows.
    Backdrop,
    /// Episodes still titled "Episode N": shows that have some, and those
    /// episodes.
    EpisodeTitles,
    /// No file on a drive that answers.
    Offline,
}

impl Gap {
    pub const ALL: [Gap; 7] = [
        Gap::Identity,
        Gap::Year,
        Gap::Overview,
        Gap::Poster,
        Gap::Backdrop,
        Gap::EpisodeTitles,
        Gap::Offline,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Gap::Identity => "identity",
            Gap::Year => "year",
            Gap::Overview => "overview",
            Gap::Poster => "poster",
            Gap::Backdrop => "backdrop",
            Gap::EpisodeTitles => "episode_titles",
            Gap::Offline => "offline",
        }
    }
}

/// An item, briefly: what lists return.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentItem {
    pub id: Id,
    pub kind: ItemKind,
    pub library: Id,
    /// A season's show, an episode's season, an extra's film or show.
    pub parent: Option<Id>,
    /// The title as you may see it: one from a withheld provider is
    /// replaced by the title the file gives.
    pub title: String,
    pub year: Option<u16>,
    /// `S01E03` for episodes, the season's name for seasons.
    pub subtitle: Option<String>,
    pub match_state: MatchState,
    /// What it lacks; empty when nothing is missing.
    pub gaps: Vec<Gap>,
    /// Where the poster it shows comes from; `null`: it has none.
    pub poster: Option<ArtSource>,
    /// `false` when none of its files is on a drive that answers.
    pub available: bool,
    /// The first file's path (films, episodes, videos), or the show's
    /// first folder.
    pub path: Option<String>,
}

/// One track of a file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TrackEvidence {
    /// `video`, `audio` or `subtitle`.
    pub kind: String,
    /// "H.264", "AAC", "SubRip"…
    pub codec: String,
    /// The language the file declares (ISO 639-2, `jpn`), if any.
    pub language: Option<String>,
    /// The track's own name, if the file gives one ("Director's
    /// commentary").
    pub title: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f32>,
    pub channels: Option<u8>,
    /// HDR format, for video ("HDR10", "Dolby Vision").
    pub hdr: Option<String>,
    pub default: bool,
    pub forced: bool,
}

/// One file of an item and what flux read from it: the evidence to identify
/// it by.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FileEvidence {
    /// The file, for `files.fix` and `files.plan`.
    pub id: Id,
    /// Relative to its library folder (`root`).
    pub path: String,
    /// The library folder it's in.
    pub root: String,
    pub size: u64,
    /// Last modified (Unix ms): for home videos, often when it was filmed.
    pub modified: Option<i64>,
    pub available: bool,
    /// "Matroska", "MP4"…
    pub container: Option<String>,
    pub duration_ms: Option<u64>,
    /// The title stored inside the file, if any.
    pub container_title: Option<String>,
    /// Edition flux read from the name ("Director's Cut").
    pub edition: Option<String>,
    pub tracks: Vec<TrackEvidence>,
    /// Chapter names, when the file names its chapters (at most 50).
    pub chapters: Vec<String>,
    /// Subtitle files beside it.
    pub subtitle_files: Vec<String>,
    /// Metadata files beside it that flux read: a `.flux.json` or an `.nfo`.
    pub sidecars: Vec<String>,
    /// What flux read from the file's name and folders.
    pub parsed: Option<NameGuess>,
    /// What the file was fixed to be (`files.fix`), which flux places it by
    /// instead of its name; `null`: its name decides.
    pub fix: Option<FileFix>,
}

/// Image kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ArtKind {
    /// 2:3, with the title.
    Poster,
    /// 16:9, without text.
    Backdrop,
    /// An episode's still, 16:9.
    Thumb,
    /// The title as a transparent logo.
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

/// One image of an item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentArt {
    pub id: Id,
    pub kind: ArtKind,
    /// `frame` and `generated` are flux's own (a frame of the video; AI
    /// art); `custom` was chosen by you or an agent; `provider` and `local`
    /// came from a provider or a file beside the video.
    pub source: ArtSource,
    /// The image the item shows for its kind.
    pub shown: bool,
    /// Chosen by hand: it shows until unpinned.
    pub pinned: bool,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Who it's credited to.
    pub credit: Option<String>,
    /// The page it came from.
    pub found_at: Option<String>,
    /// The AI model that made it (for `generated`).
    pub generator: Option<String>,
    /// Where to fetch it (or use `art.view`); `null` when withheld.
    pub url: Option<String>,
    /// The provider it's withheld for (its terms forbid use with AI).
    pub withheld: Option<String>,
}

/// `item.get`: one item as an agent sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentItemDetail {
    #[serde(flatten)]
    pub item: AgentItem,
    /// Every field you can set, with its value now (`null`: empty, or
    /// withheld).
    pub fields: Fields,
    /// Where each field that has a value came from. A field that isn't
    /// listed came from the file (or the scan).
    pub sources: BTreeMap<String, FieldSource>,
    /// Fields left out because their provider's terms forbid use with AI,
    /// and that provider: `{"overview": "tmdb"}`. They have a value you
    /// don't see; replace them only if you have better.
    pub withheld: BTreeMap<String, String>,
    /// How sure flux's matcher was (0–1), when it tried.
    pub match_confidence: Option<f32>,
    /// Seasons and episodes: the numbers.
    pub season_number: Option<u16>,
    pub episode_numbers: Vec<u16>,
    /// The item's own files (films, episodes, videos, extras; a show's and
    /// a season's files are on their episodes).
    pub files: Vec<FileEvidence>,
    /// A show's folders, relative to their library folder.
    pub folders: Vec<String>,
    pub art: Vec<AgentArt>,
    /// A show's seasons, a season's episodes, a film's extras.
    pub children: Vec<AgentItem>,
    /// The latest changesets that changed it, newest first.
    pub changes: Vec<ChangesetSummary>,
}

/// `item.get` input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ItemRef {
    pub item: Id,
}

/// `items.find` input. Every filter is optional; they combine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FindInput {
    pub library: Option<Id>,
    /// Items below this one: a show's seasons and episodes, a season's
    /// episodes, a film's extras.
    pub parent: Option<Id>,
    /// Without `parent`, the default is top-level items (films, shows,
    /// videos); `episode` finds episodes across shows.
    pub kind: Option<ItemKind>,
    pub gap: Option<Gap>,
    pub match_state: Option<MatchState>,
    /// Text in the title (as you may see it) or in a file's path, any case.
    pub q: Option<String>,
    /// 1–200, default 50.
    pub limit: Option<u32>,
    /// `next` from the previous page.
    pub cursor: Option<String>,
}

/// `items.find` output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FindOutput {
    pub items: Vec<AgentItem>,
    /// Pass as `cursor` for the next page; `null` at the end.
    pub next: Option<String>,
    /// Items matching, on every page.
    pub total: u64,
}

/// `library.gaps` input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GapsInput {
    /// One library; `null`: every library.
    pub library: Option<Id>,
}

/// Items lacking each thing. `null`: the gap doesn't apply to the library's
/// kind (a videos library is never matched).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GapCounts {
    pub identity: Option<u64>,
    pub year: Option<u64>,
    pub overview: Option<u64>,
    pub poster: Option<u64>,
    pub backdrop: Option<u64>,
    /// Shows with placeholder episode titles (and, in `episodes`, the
    /// episodes).
    pub episode_titles: Option<u64>,
    pub offline: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LibraryGaps {
    pub id: Id,
    pub name: String,
    pub kind: LibraryKind,
    /// Top-level items.
    pub items: u64,
    pub gaps: GapCounts,
    /// Episodes titled "Episode N".
    pub placeholder_episodes: Option<u64>,
}

/// `library.gaps` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GapsOutput {
    pub libraries: Vec<LibraryGaps>,
}

// ---- edits --------------------------------------------------------------------

/// Apply now, or leave for an Owner or Admin to review.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum EditMode {
    /// Change it now (Owner or Admin; anyone else's edit is proposed).
    #[default]
    Apply,
    /// Make a proposal: nothing changes until it's applied in the app or
    /// with `changes.apply`.
    Propose,
}

/// One item's edit.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ItemEdit {
    pub item: Id,
    /// The values to set.
    #[serde(default)]
    pub set: Fields,
    /// Fields to empty, or to hand back to flux (`ids.tmdb` for one id).
    #[serde(default)]
    pub clear: Vec<String>,
    /// The pages that support these values (a value should have at least
    /// one, unless it comes from the file itself).
    #[serde(default)]
    pub sources: Vec<Link>,
}

/// `items.edit` input: one changeset over up to 100 items.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EditInput {
    /// What these changes are and why, in a sentence or two, for the
    /// person reviewing them ("Identified 3 Ghibli films from their
    /// Japanese file names").
    pub summary: String,
    pub edits: Vec<ItemEdit>,
    #[serde(default)]
    pub mode: EditMode,
    /// Also replace values a person set. Only when they asked you to.
    #[serde(default)]
    pub force: bool,
    /// Report what would change, and change nothing.
    #[serde(default)]
    pub dry_run: bool,
}

/// A field's change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FieldChange {
    pub field: String,
    /// The value before (`null`: empty, or withheld).
    pub before: Option<Value>,
    pub after: Option<Value>,
    /// The provider the value before is withheld for.
    pub withheld: Option<String>,
}

/// A field an edit didn't change, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Refused {
    pub field: String,
    pub reason: String,
}

/// What an edit did to one item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EditResult {
    pub item: Id,
    pub title: String,
    pub changed: Vec<FieldChange>,
    /// Fields that already held the value.
    pub unchanged: Vec<String>,
    pub refused: Vec<Refused>,
}

/// `items.edit` output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EditOutput {
    /// The changeset (applied or proposed); `null` for a dry run or when
    /// nothing would change.
    pub changeset: Option<ChangesetSummary>,
    pub items: Vec<EditResult>,
}

// ---- changesets -------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ChangesetState {
    /// Waiting for an Owner or Admin.
    Proposed,
    Applied,
    /// Undone.
    Reverted,
    /// A proposal turned down.
    Rejected,
}

/// What became of one change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ChangeState {
    /// Proposed, not applied.
    Pending,
    Applied,
    /// Not applied: the field had changed since it was proposed.
    Skipped,
    Reverted,
    /// Not undone: the field had changed again since.
    Kept,
}

/// A changeset, briefly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChangesetSummary {
    pub id: Id,
    pub state: ChangesetState,
    pub summary: String,
    /// The agent that made it; `null` for a person.
    pub agent: Option<String>,
    /// The person it was made by, or for (a login).
    pub by: String,
    pub created_at: i64,
    /// Applied or rejected.
    pub decided_at: Option<i64>,
    /// Who applied or rejected a proposal.
    pub decided_by: Option<String>,
    pub reverted_at: Option<i64>,
    pub reverted_by: Option<String>,
    /// How many items it changes.
    pub item_count: u32,
    /// How many changes it holds.
    pub change_count: u32,
}

/// One change of a changeset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Change {
    pub item: Id,
    /// The item's title now, as you may see it (`null` once it's gone).
    pub title: Option<String>,
    /// A field name; `art.<kind>` for the image an item shows; `file.fix`
    /// for what a file is (`before` and `after` are `{file, path, fix}`);
    /// `file.path` for a file moved and `folder.path` for a folder renamed
    /// (`{file, path}` or `{folder, path}`).
    pub field: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
    /// The provider the value before is withheld for.
    pub withheld: Option<String>,
    pub sources: Vec<Link>,
    pub state: ChangeState,
}

/// `changes.get` output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Changeset {
    #[serde(flatten)]
    pub summary: ChangesetSummary,
    pub changes: Vec<Change>,
}

/// `changes.list` input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChangesListInput {
    pub state: Option<ChangesetState>,
    /// Only changesets that change this item.
    pub item: Option<Id>,
    /// 1–200, default 50.
    pub limit: Option<u32>,
    /// `next` from the previous page: changesets older than this one.
    pub before: Option<Id>,
}

/// `changes.list` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChangesListOutput {
    pub changesets: Vec<ChangesetSummary>,
    /// Pass as `before` for older ones; `null` at the end.
    pub next: Option<Id>,
    /// Proposals waiting for a decision (all of them, not just this page).
    pub proposed: u64,
}

/// Input naming one changeset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChangesetRef {
    pub changeset: Id,
}

/// A change that couldn't be applied or undone, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NotDone {
    pub item: Id,
    pub field: String,
    pub reason: String,
}

/// `changes.undo`, `changes.apply` and `changes.reject` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Decided {
    pub changeset: ChangesetSummary,
    /// Changes undone, or applied.
    pub done: u32,
    /// Changes left as they were, and why.
    pub not_done: Vec<NotDone>,
}

// ---- art ------------------------------------------------------------------------

/// How an image came to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ArtOrigin {
    /// Someone else made it (a studio's poster, a photograph): give
    /// `credit` and `found_at`.
    Found,
    /// You made it without AI (a crop, a collage of the video's frames).
    Made,
    /// An AI model made it: give `generator`. It is labelled AI-generated
    /// wherever it shows. Make one only when the person asked for it.
    Generated,
}

/// `art.add` input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ArtAddInput {
    pub item: Id,
    pub kind: ArtKind,
    /// The image's address (`http` or `https`; flux fetches it). Give this
    /// or `data_base64`.
    #[serde(default)]
    pub url: Option<String>,
    /// The image itself (JPEG, PNG or WebP), base64.
    #[serde(default)]
    pub data_base64: Option<String>,
    pub origin: ArtOrigin,
    /// Who to credit: the studio, the distributor, the photographer, the
    /// site. Needed for `found`.
    #[serde(default)]
    pub credit: Option<String>,
    /// The page you found it on.
    #[serde(default)]
    pub found_at: Option<String>,
    /// The model, for `generated` ("gpt-image-2").
    #[serde(default)]
    pub generator: Option<String>,
    /// Show it from now on (default `true`).
    #[serde(default = "yes")]
    pub pin: bool,
    /// For the changeset; default "Added a poster".
    #[serde(default)]
    pub summary: Option<String>,
}

fn yes() -> bool {
    true
}

/// `art.add` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ArtAddOutput {
    pub art: AgentArt,
    pub changeset: ChangesetSummary,
}

/// `art.view` input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ArtViewInput {
    pub art: Id,
    /// Scale down to this width (default 480; 0 = full size).
    #[serde(default)]
    pub width: Option<u32>,
}

/// `art.view` output: the image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ArtImage {
    pub art: Id,
    /// `image/jpeg`, `image/png` or `image/webp`.
    pub mime: String,
    pub data_base64: String,
}

/// `art.pin` input: show `art`, or (with `item` and `kind`, no `art`) go
/// back to flux's own choice.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ArtPinInput {
    pub art: Option<Id>,
    pub item: Option<Id>,
    pub kind: Option<ArtKind>,
    pub summary: Option<String>,
}

/// `art.pin` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ArtPinOutput {
    /// `null` when nothing changed.
    pub changeset: Option<ChangesetSummary>,
    /// The image the item shows now.
    pub shown: Option<AgentArt>,
}

// ---- what a file is ---------------------------------------------------------------

/// What a file is, when flux's reading of its name is wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum FixKind {
    /// A film, or one version of a film.
    Movie,
    /// An episode of a show.
    Episode,
    /// Bonus material of a film or show: a trailer, a featurette…
    Extra,
    /// Not part of the library (a sample, a stray clip): flux keeps the file
    /// but shows it nowhere.
    Ignore,
}

/// A correction to what a file's name says. flux places the file by it, on
/// every scan, until it's cleared; give only what's wrong, and the rest
/// still comes from the name.
///
/// - Two items that are one film: fix the second one's files with `of`, the
///   first (or use `items.merge`).
/// - One item that is two films: give each film's files their own `title`
///   and `year`.
/// - An episode with the wrong number: `season` and `episodes`.
/// - A trailer flux took for a film: `kind: extra`, `extra: trailer`, `of`
///   the film.
///
/// To say what a film or episode *is* (its title, year, ids), edit the item
/// with `items.edit`: a fix says which item a *file* belongs to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FileFix {
    pub kind: Option<FixKind>,
    /// The film or show it belongs to: a top-level item of its library. A
    /// film's other version, a show's episode, an extra's film or show.
    pub of: Option<Id>,
    /// The title to group it under, as if its name said it: a film's
    /// title, or an episode's show.
    pub title: Option<String>,
    pub year: Option<u16>,
    /// An episode's season (0 for specials).
    pub season: Option<u16>,
    /// An episode's number, or its numbers for a file that holds several
    /// (`[1, 2]`).
    pub episodes: Option<Vec<u16>>,
    /// An extra's kind.
    pub extra: Option<crate::names::ExtraKind>,
    /// A film version's edition ("Director's Cut").
    pub edition: Option<String>,
    /// A film's part, for one split over files (2 for `cd2`).
    pub part: Option<u8>,
}

impl FileFix {
    /// Nothing to correct.
    pub fn is_empty(&self) -> bool {
        *self == FileFix::default()
    }
}

/// One file's fix, in `files.fix`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FileFixEdit {
    /// The file (`files[].id` in `item.get`).
    pub file: Id,
    /// What it is. `null` (or `{}`) clears its fix: its name decides again.
    #[serde(default)]
    pub fix: Option<FileFix>,
    /// The pages that support it.
    #[serde(default)]
    pub sources: Vec<Link>,
}

/// `files.fix` input: one changeset over up to 200 files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FixInput {
    /// What these fixes are and why, for the person reviewing them.
    pub summary: String,
    pub fixes: Vec<FileFixEdit>,
    #[serde(default)]
    pub mode: EditMode,
    /// Report where each file would go, and change nothing.
    #[serde(default)]
    pub dry_run: bool,
}

/// `items.merge` input: films (or shows) that are one work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MergeInput {
    /// What's being merged and why, for the person reviewing it.
    pub summary: String,
    /// The film or show to keep.
    pub into: Id,
    /// The films or shows that are the same work. Their files are fixed to
    /// belong to `into` (episodes keep their numbers); each goes once it's
    /// empty, and what people and agents set on it carries over.
    pub from: Vec<Id>,
    #[serde(default)]
    pub mode: EditMode,
    #[serde(default)]
    pub dry_run: bool,
}

/// The item a file belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FilePlace {
    /// A film, episode, extra or video; `null`: none (ignored, or waiting
    /// for its film to be scanned).
    pub item: Option<Id>,
    /// The item's title as you may see it; for an episode, with its show
    /// and number (`Show · S01E03`).
    pub title: Option<String>,
}

/// What a fix did to one file (or, in a dry run, would do).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FixResult {
    pub file: Id,
    pub path: String,
    pub before: FilePlace,
    /// Where it is now; `null` for a proposal (known once it's applied).
    pub after: Option<FilePlace>,
    /// Its fix now (or as proposed).
    pub fix: Option<FileFix>,
    /// Why nothing changed for this file.
    pub unchanged: Option<String>,
}

/// `files.fix` and `items.merge` output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FixOutput {
    /// The changeset (applied or proposed); `null` for a dry run or when
    /// nothing would change.
    pub changeset: Option<ChangesetSummary>,
    pub files: Vec<FixResult>,
}

// ---- collections ------------------------------------------------------------------------

/// The titles a rule takes into a collection: those that match every
/// condition given (in a list, any of its values; names in any case). As
/// the library changes, so does the collection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CollectionRule {
    /// In these libraries (default: every film and show library).
    pub libraries: Vec<Id>,
    /// `movie`, `show` (default: both).
    pub kinds: Vec<ItemKind>,
    pub genres: Vec<String>,
    pub tags: Vec<String>,
    /// Directed, written by or starring any of these.
    pub people: Vec<String>,
    pub studios: Vec<String>,
    /// ISO 3166-1 codes (`JP`).
    pub countries: Vec<String>,
    /// ISO 639-1 codes (`ja`).
    pub languages: Vec<String>,
    pub year_from: Option<u16>,
    pub year_to: Option<u16>,
    /// Text in the title or the original title.
    pub text: Option<String>,
}

impl CollectionRule {
    /// No condition at all (it would take every title).
    pub fn is_empty(&self) -> bool {
        *self == CollectionRule::default()
    }
}

/// A title picked for a collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CollectionPick {
    /// A film or show.
    pub item: Id,
    /// Why it's here, in a sentence (shown with it).
    #[serde(default)]
    pub note: Option<String>,
}

/// `collection.put` input: make a collection, or change one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CollectionPutInput {
    /// The collection to change; leave out to make one (one of yours with
    /// the same name is changed instead).
    #[serde(default)]
    pub id: Option<Id>,
    /// A few words: "Ghibli", "Rainy Sunday", "Kids' films" (at most 60
    /// characters).
    pub name: String,
    /// What it is, in a sentence or two (at most 1,000).
    #[serde(default)]
    pub description: Option<String>,
    /// The titles picked, in order (replaces those picked before).
    #[serde(default)]
    pub items: Option<Vec<CollectionPick>>,
    /// Titles to pick too, after the others.
    #[serde(default)]
    pub add: Vec<CollectionPick>,
    /// Titles to take out.
    #[serde(default)]
    pub remove: Vec<Id>,
    /// Titles matching it are in the collection too (`null` keeps the rule
    /// it has; an empty rule `{}` drops it).
    #[serde(default)]
    pub rule: Option<CollectionRule>,
    /// Kept for everyone on this server (Owner or Admin), not just you.
    #[serde(default)]
    pub everyone: bool,
    /// Pages that say why these belong together.
    #[serde(default)]
    pub sources: Vec<Link>,
}

/// A collection kept on the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CollectionInfo {
    pub id: Id,
    pub name: String,
    pub description: Option<String>,
    pub everyone: bool,
    pub rule: Option<CollectionRule>,
    /// Titles in it now.
    pub count: u64,
    /// The first few titles (ids), for a cover.
    pub cover: Vec<Id>,
    /// Who made it (a login).
    pub added_by: String,
    pub agent: Option<String>,
    pub sources: Vec<Link>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// `collections.list` output: everyone's, then yours.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Collections {
    pub collections: Vec<CollectionInfo>,
}

/// `collection.get` input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CollectionGetInput {
    pub collection: Id,
    /// 1–200, default 50.
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next` from the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// `collection.get` output: its titles, those picked first (in order),
/// then those its rule takes (by title).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CollectionItems {
    pub collection: CollectionInfo,
    pub items: Vec<AgentItem>,
    /// Why a picked title is there, by id.
    pub notes: BTreeMap<String, String>,
    pub next: Option<String>,
    pub total: u64,
}

/// Input naming one collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CollectionRef {
    pub collection: Id,
}

// ---- metadata files beside the videos -------------------------------------------------

/// The sidecar format, `flux.sidecar/1`.
pub const SIDECAR: &str = "flux.sidecar/1";

/// An episode's facts in a show's `flux.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SidecarEpisode {
    pub season: u16,
    pub episode: u16,
    #[serde(default)]
    pub fields: Fields,
}

/// A `.flux.json`: facts that travel with the files. Beside a video, named
/// like it (`Film (2010).flux.json`), it describes that video; as
/// `flux.json` in a film's or show's own folder, the film or show (a show's
/// can describe its episodes too). flux reads it on every scan. What it
/// says counts more than a provider's match, less than what a person or an
/// agent set in flux. An agent that can write files but can't reach flux
/// can still contribute this way.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Sidecar {
    /// [`SIDECAR`]: `flux.sidecar/1`.
    pub flux: String,
    /// The facts, as `items.edit` takes them.
    #[serde(default)]
    pub fields: Fields,
    /// What the video is, as `files.fix` takes it (a video's own sidecar
    /// only).
    #[serde(default)]
    pub fix: Option<FileFix>,
    /// A show's episodes (a show folder's `flux.json` only).
    #[serde(default)]
    pub episodes: Vec<SidecarEpisode>,
    /// The pages that support the facts.
    #[serde(default)]
    pub sources: Vec<Link>,
    /// Who wrote it: an agent's name or a person's.
    #[serde(default)]
    pub by: Option<String>,
}

// ---- organising files -----------------------------------------------------------------

/// How `files.plan` names files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum NamingScheme {
    /// What Plex, Jellyfin, Emby and Kodi all read: `Title (Year)/Title
    /// (Year).mkv` (versions ` - Edition` or ` - 1080p`, parts ` - part2`);
    /// `Show (Year)/Season 01/Show (Year) - S01E02 - Episode Title.mkv`;
    /// extras in `Trailers/`, `Featurettes/`, … beside the film.
    #[default]
    Standard,
}

/// A file to move where you say, in `files.plan`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FileMove {
    /// A video file (`files[].id` in `item.get`).
    pub file: Id,
    /// Its new path relative to its library folder, `/` between folders
    /// (`Interstellar (2014)/Interstellar (2014).mkv`).
    pub to: String,
}

/// `files.plan` input: name items' files by a scheme, or move files where
/// you say. Nothing moves: the plan is a proposal to review and apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PlanInput {
    /// What the plan does and why, for the person reviewing it.
    pub summary: String,
    /// Films, shows, seasons, episodes or extras whose files to name by
    /// `scheme` (a show: all its episodes and extras).
    #[serde(default)]
    pub items: Vec<Id>,
    /// Or: files to move where you say.
    #[serde(default)]
    pub moves: Vec<FileMove>,
    #[serde(default)]
    pub scheme: NamingScheme,
    /// Put the ids flux knows in folder names (`Interstellar (2014)
    /// [imdbid-tt0816692]`), which other media servers match by.
    #[serde(default)]
    pub ids_in_names: bool,
}

/// One step of a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum StepKind {
    /// Make a folder (`folder.new`).
    NewFolder,
    /// Rename a folder; everything in it comes along (`folder.path`).
    RenameFolder,
    /// Rename or move a video (`file.path`).
    MoveVideo,
    /// Rename or move a file beside a video: subtitles, a metadata file, an
    /// image named like it (`file.path`).
    MoveCompanion,
    /// Remove a folder the plan leaves empty (`folder.gone`).
    RemoveFolder,
}

/// One step of a plan: what moves where, inside one library folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PlanStep {
    pub step: StepKind,
    /// The video, for `move_video`.
    pub file: Option<Id>,
    /// The item it's for.
    pub item: Option<Id>,
    /// The library folder.
    pub root: String,
    /// Relative to the library folder; `null` for a new folder.
    pub from: Option<String>,
    /// Relative to the library folder; `null` for a folder removed.
    pub to: Option<String>,
}

/// A file a plan leaves where it is, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PlanSkip {
    pub file: Option<Id>,
    pub item: Option<Id>,
    pub path: Option<String>,
    pub reason: String,
}

/// `files.plan` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PlanOutput {
    /// The plan, a proposed changeset: nothing moves until it's applied
    /// (`changes.apply`, or in the app), and `changes.undo` puts everything
    /// back. `null` when there's nothing to do.
    pub changeset: Option<ChangesetSummary>,
    pub steps: Vec<PlanStep>,
    pub skipped: Vec<PlanSkip>,
}

// ---- lenses -------------------------------------------------------------------

/// Which scroll axis moves through the titles.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum LensInput {
    #[default]
    X,
    Y,
    /// Only the arrow keys.
    None,
}

/// A lens: one way of seeing the library, written as data (docs/LENSES.md,
/// "The lens kit"). flux removes from it anything that could run or load
/// (scripts, links, images, `url()`, inline styles) and runs `layout` in a
/// locked-down worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LensSpec {
    /// One or two words (at most 28 characters).
    pub name: String,
    /// One plain sentence: what it looks like and how you move through it
    /// (at most 160).
    pub blurb: String,
    pub input: LensInput,
    /// Pixels of scroll or drag per title (30–600).
    pub step: f64,
    /// Titles drawn either side of the position (8–150).
    pub window: u32,
    /// Ask for a frame every tick (only if `layout` moves things by itself
    /// with `view.t`).
    pub animated: bool,
    /// The look. Every selector starts with `.lens` (at most 14,000
    /// characters).
    pub css: String,
    /// Static HTML behind the titles, decoration only (at most 4,000).
    pub scene: String,
    /// HTML for one title, with `{{title}}`, `{{year}}`, … and `<div
    /// class="art"></div>` where the poster goes (at most 4,000).
    pub item: String,
    /// Source of `function layout(items, view)` (at most 16,000).
    pub layout: String,
}

/// Input holding a lens as written (checked for you, with defaults filled
/// in).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LensCheckInput {
    #[cfg_attr(feature = "schema", schemars(with = "LensSpec"))]
    pub spec: Value,
}

/// `lens.check` output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LensCheck {
    /// The lens can be added.
    pub ok: bool,
    /// What must change.
    pub errors: Vec<String>,
    /// What flux will remove or change, and other advice.
    pub warnings: Vec<String>,
    /// The lens with defaults filled in, as flux would keep it.
    pub lens: Option<LensSpec>,
    /// The id it would be kept under.
    pub id: Option<String>,
}

/// `lens.add` input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LensAddInput {
    #[cfg_attr(feature = "schema", schemars(with = "LensSpec"))]
    pub spec: Value,
    /// Keep it for everyone on this server (Owner or Admin), not just you.
    #[serde(default)]
    pub everyone: bool,
}

/// A lens kept on the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct StoredLens {
    /// `s-<name>`: adding a lens with the same name replaces it.
    pub id: String,
    pub everyone: bool,
    /// Who added it (a login).
    pub added_by: String,
    pub agent: Option<String>,
    pub added_at: i64,
    pub updated_at: i64,
    pub spec: LensSpec,
}

/// `lens.add` output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LensAdded {
    pub lens: StoredLens,
    pub warnings: Vec<String>,
}

/// `lenses.list` output: yours, then everyone's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Lenses {
    pub lenses: Vec<StoredLens>,
}

/// `lens.remove` input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LensRemoveInput {
    pub id: String,
    /// The one kept for everyone (Owner or Admin).
    #[serde(default)]
    pub everyone: bool,
}

/// Output of an operation that removes something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Removed {
    /// `false`: there was nothing to remove.
    pub removed: bool,
}

// ---- scans ----------------------------------------------------------------------

/// `library.scan` input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScanInput {
    pub library: Id,
}

/// `library.scan` output: the scan's state as it starts. Follow it with
/// `status` (or the `/api/v1/events` stream).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScanOutput {
    pub library: Id,
    pub scan: ScanStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_read_with_defaults_and_refuse_unknown_fields() {
        let e: EditInput = serde_json::from_value(serde_json::json!({
            "summary": "Years",
            "edits": [{"item": 3, "set": {"year": 2001, "ids": {"wikidata": "Q155653"}}}]
        }))
        .unwrap();
        assert_eq!(e.mode, EditMode::Apply);
        assert!(!e.force && !e.dry_run);
        assert_eq!(e.edits[0].set.year, Some(2001));
        assert!(e.edits[0].clear.is_empty());
        // A misspelt field is an error, not a silent no-op.
        let bad = serde_json::from_value::<EditInput>(serde_json::json!({
            "summary": "x", "edits": [{"item": 3, "set": {"yaer": 2001}}]
        }));
        assert!(bad.is_err());
    }

    #[test]
    fn wire_names_are_snake_case() {
        assert_eq!(
            serde_json::to_value(Gap::EpisodeTitles).unwrap(),
            "episode_titles"
        );
        for g in Gap::ALL {
            assert_eq!(serde_json::to_value(g).unwrap(), g.as_str());
        }
        assert_eq!(serde_json::to_value(ErrorCode::NotFound).unwrap(), "not_found");
        assert_eq!(serde_json::to_value(LensInput::None).unwrap(), "none");
        assert_eq!(serde_json::to_value(ArtOrigin::Generated).unwrap(), "generated");
        let pin: ArtAddInput = serde_json::from_value(serde_json::json!({
            "item": 1, "kind": "poster", "url": "https://example.org/p.jpg", "origin": "found"
        }))
        .unwrap();
        assert!(pin.pin, "art.add pins by default");
    }

    #[test]
    fn a_link_is_a_url_or_an_object() {
        let l: Vec<Link> = serde_json::from_value(serde_json::json!([
            "https://a.example/x",
            {"url": "https://b.example/y", "title": "B"}
        ]))
        .unwrap();
        assert_eq!(
            l[0],
            Link {
                url: "https://a.example/x".into(),
                title: None
            }
        );
        assert_eq!(l[1].title.as_deref(), Some("B"));
    }

    #[test]
    fn field_names_match_the_struct() {
        let v = serde_json::to_value(Fields::default()).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        let mut names: Vec<&str> = Fields::NAMES.to_vec();
        names.push("ids");
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        names.sort_unstable();
        assert_eq!(sorted, names);
    }
}
