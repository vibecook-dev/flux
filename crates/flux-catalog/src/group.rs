//! Grouping rules: from a file's [`NameGuess`] (and path) decide which items
//! it belongs to.
//!
//! - **Movies** are one item per work: an explicit provider id
//!   (`[tmdbid-603]`) wins; otherwise the normalised `(title, year)`.
//!   Versions (2160p/1080p) and editions ("Director's Cut") become several
//!   assets of the same movie (edition and part stored on the asset), and
//!   multi-part files (cd1/cd2) are ordered by `part`.
//! - **Episodes** go show → season → episode. A multi-episode file
//!   (S01E01-E02) is one asset linked to several episode items.
//! - **Extras** attach to the movie/show that owns their folder
//!   (`Movie (2010)/Featurettes/x.mkv`), or to the item named by the guess.
//!   A folder named for extras by one of its words (`Film/Bonus Clips)/`,
//!   [`extras_folder_by_word`]) holds extras of the one film beside it.
//! - **Show folders**: a show owns the folders its episodes are in; a
//!   subfolder that isn't a season (`Show/alt versions/`) is the same show,
//!   so its files become more versions of the same episodes. A subfolder
//!   takes the year of the folder around it, never the other way round:
//!   `Show/Show (1978)/` is another series. Two undated same-titled shows
//!   in nested folders (`Show/` and `Show/US/`) are one show, as nothing
//!   tells them apart from alternate versions.
//! - **Samples** are ignored: the file row is kept (so it is never
//!   re-probed) but it maps to no item.
//! - **Unknown** files become an "unmatched" movie titled after the cleaned
//!   filename, so nothing the user added is invisible.
//!
//! Year handling: a guess without a year joins the *only* same-titled item;
//! a guess with a year adopts a same-titled item that has none. This keeps
//! `Show/Season 01/…` and `Show (2008)/…` together without merging remakes
//! (two candidates → ambiguous → new item).
//!
//! The pure part ([`place`], [`normalize_title`], …) has no I/O; the
//! `resolve_*` functions run inside the writer's transaction.

use std::collections::HashMap;

use flux_core::api::Id;
use flux_core::{ExtraKind, GuessKind, NameGuess, ProviderIds};
use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;
use crate::jobs::JOB_MATCH;
use crate::model::enum_to_text;

// ---- normalisation ----------------------------------------------------------

/// Grouping key for a title: lowercase, Latin diacritics folded, `&` → "and",
/// apostrophes dropped, every other non-alphanumeric run collapsed to one
/// space. `"Mad Max: Fury Road"`, `"Mad.Max.-.Fury.Road"` → `"mad max fury road"`.
pub fn normalize_title(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut gap = false;
    for ch in s.chars() {
        if matches!(ch, '\'' | '’' | '`' | 'ʼ') {
            continue;
        }
        if ch == '&' {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str("and");
            gap = true;
            continue;
        }
        if ch.is_alphanumeric() {
            if gap && !out.is_empty() {
                out.push(' ');
            }
            gap = false;
            for lc in ch.to_lowercase() {
                push_folded(&mut out, lc);
            }
        } else {
            gap = true;
        }
    }
    out
}

/// Fold common Latin diacritics so "Amélie" groups with "Amelie". (FTS5 does
/// its own folding for search; this is only for identity keys.)
fn push_folded(out: &mut String, c: char) {
    let s: &str = match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ą' | 'ă' => "a",
        'ç' | 'ć' | 'č' => "c",
        'ď' | 'đ' | 'ð' => "d",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ę' | 'ě' => "e",
        'ì' | 'í' | 'î' | 'ï' | 'ī' | 'ı' => "i",
        'ł' | 'ľ' => "l",
        'ñ' | 'ń' | 'ň' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => "o",
        'ř' => "r",
        'ś' | 'š' | 'ş' => "s",
        'ť' | 'ţ' => "t",
        'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' => "u",
        'ý' | 'ÿ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        'æ' => "ae",
        'œ' => "oe",
        'ß' => "ss",
        'þ' => "th",
        _ => {
            out.push(c);
            return;
        }
    };
    out.push_str(s);
}

/// Sort key: the normalised title without a leading article, with digit runs
/// zero-padded so "Rocky 2" sorts before "Rocky 10".
pub fn sort_title(title: &str) -> String {
    let n = normalize_title(title);
    let body = ["the ", "a ", "an "]
        .iter()
        .find_map(|a| n.strip_prefix(a).filter(|rest| !rest.is_empty()))
        .unwrap_or(&n);
    let mut out = String::with_capacity(body.len() + 8);
    let mut digits = String::new();
    let flush = |out: &mut String, digits: &mut String| {
        if !digits.is_empty() {
            for _ in digits.len()..6 {
                out.push('0');
            }
            out.push_str(digits);
            digits.clear();
        }
    };
    for c in body.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
        } else {
            flush(&mut out, &mut digits);
            out.push(c);
        }
    }
    flush(&mut out, &mut digits);
    out
}

/// A display title from a filename: the stem with `.`/`_` as spaces and
/// `[…]`/`{…}` groups removed. `"Some_Video.[x264].mkv"` → `"Some Video"`.
pub fn clean_filename(rel_path: &str) -> String {
    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    let stem = match name.rsplit_once('.') {
        Some((s, ext))
            if !s.is_empty() && ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            s
        }
        _ => name,
    };
    let mut out = String::with_capacity(stem.len());
    let mut depth = 0u32;
    for c in stem.chars() {
        match c {
            '[' | '{' => depth += 1,
            ']' | '}' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            '.' | '_' => out.push(' '),
            _ => out.push(c),
        }
    }
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_matches(|c: char| c == '-' || c.is_whitespace());
    if trimmed.is_empty() {
        stem.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Folder names that hold extras (Plex/Jellyfin conventions).
const EXTRA_DIRS: &[&str] = &[
    "extras",
    "featurettes",
    "behind the scenes",
    "deleted scenes",
    "interviews",
    "scenes",
    "shorts",
    "trailers",
    "other",
    "others",
    "bonus",
];

/// Is this directory name a season folder (`Season 1`, `S01`, `Specials`,
/// `Series 2`, `Staffel 3`, `Disc 1`), or a season pack
/// (`Show.S01.1080p.BluRay-GRP`: an `S<n>` token not followed by `E<n>`)?
/// Such folders sit *inside* a show folder, so [`show_folder`] skips them.
pub fn is_season_dir(name: &str) -> bool {
    const SEASON_WORDS: &[&str] = &[
        "season",
        "series",
        "staffel",
        "saison",
        "temporada",
        "stagione",
        "seizoen",
        "sezon",
        "disc",
        "disk",
        "dvd",
        "cd",
        "volume",
        "vol",
        "s",
    ];
    let lower = name.trim().to_lowercase();
    if matches!(lower.as_str(), "specials" | "special") {
        return true;
    }
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let numeric = |w: &str| !w.is_empty() && w.len() <= 4 && w.chars().all(|c| c.is_ascii_digit());
    match words.as_slice() {
        // "01", "2024" (date-based shows keep a folder per year).
        [only] if numeric(only) => return true,
        // "Season 1", "Season.01", "Disc 2", "Season 1 (2005)".
        [first, second, ..] if SEASON_WORDS.contains(first) && numeric(second) => return true,
        // "Season01", "S01", "Disc1" (optionally followed by one tag word).
        [first] | [first, _]
            if SEASON_WORDS
                .iter()
                .any(|w| first.strip_prefix(w).is_some_and(numeric)) =>
        {
            return true
        }
        _ => {}
    }
    // A season pack: an `s<digits>` word (never `s01e02`) anywhere in the name.
    words
        .iter()
        .any(|w| (2..=4).contains(&w.len()) && w.starts_with('s') && numeric(&w[1..]))
}

/// The show folder of an episode file: its directory with trailing season
/// folders ([`is_season_dir`]) removed, root-relative and lowercased.
/// `None` when the file sits at the root (no show folder to go by).
///
/// `Doctor Who/S01/x.mp4` → `doctor who`; `Doctor Who (1963)/Season 1/x.mkv`
/// → `doctor who (1963)`; `Show.S01E01.mkv` → `None`.
pub fn show_folder(rel_path: &str) -> Option<String> {
    let (dir, _) = rel_path.rsplit_once('/')?;
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    while parts.last().is_some_and(|p| is_season_dir(p)) {
        parts.pop();
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/").to_lowercase())
    }
}

/// The directory of the item that owns an extra: the extra's folder, or its
/// parent when the folder is an extras folder. `None` at the root.
pub fn extras_parent_dir(rel_path: &str) -> Option<String> {
    let (dir, _) = rel_path.rsplit_once('/')?;
    let (parent, last) = match dir.rsplit_once('/') {
        Some((p, l)) => (Some(p), l),
        None => (None, dir),
    };
    if EXTRA_DIRS.contains(&last.to_lowercase().as_str()) {
        parent.map(str::to_string)
    } else {
        Some(dir.to_string())
    }
}

/// Words that mark a folder as a film's extras when they are among the
/// folder name's words, though the name isn't exactly an extras folder
/// (`Bonus Clips)`, `Bonus Trailers`). Plural forms only: "Extra",
/// "Short" or "Trailer" alone are likelier to be part of a title.
const EXTRA_FOLDER_WORDS: &[(&[&str], ExtraKind)] = &[
    (&["behind", "the", "scenes"], ExtraKind::BehindTheScenes),
    (&["deleted", "scenes"], ExtraKind::DeletedScene),
    (&["featurettes"], ExtraKind::Featurette),
    (&["trailers"], ExtraKind::Trailer),
    (&["interviews"], ExtraKind::Interview),
    (&["shorts"], ExtraKind::Short),
    (&["clips"], ExtraKind::Other),
    (&["extras"], ExtraKind::Other),
];

/// A file in a folder named for extras by one of its words, below another
/// folder: `Film/1080p/Bonus Clips)/Song.mkv` → `(Other, "Film/1080p")`.
/// Returns the extra kind and the folder that should hold the film.
///
/// Conservative, because a miss only leaves the file as a film of its own:
/// never at the root (there is no film folder above), never for a folder
/// with a year in its name (`Shorts (2009)/` is a film's own folder).
/// [`crate::Writer::apply`] also requires that the folder above holds the
/// files of exactly one film directly; otherwise the file stays a film.
pub fn extras_folder_by_word(rel_path: &str) -> Option<(ExtraKind, &str)> {
    let (dir, _) = rel_path.rsplit_once('/')?;
    let (parent, name) = dir.rsplit_once('/')?;
    if parent.is_empty() {
        return None;
    }
    let lower = name.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let is_year = |w: &&str| {
        w.len() == 4 && w.chars().all(|c| c.is_ascii_digit()) && (w.starts_with("19") || w.starts_with("20"))
    };
    if words.iter().any(is_year) {
        return None;
    }
    EXTRA_FOLDER_WORDS
        .iter()
        .find(|(seq, _)| words.windows(seq.len()).any(|w| w == *seq))
        .map(|&(_, kind)| (kind, parent))
}

// ---- placement (pure) -------------------------------------------------------

/// A top-level work (movie or show) as identified by the parser.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkTarget {
    pub title: String,
    /// [`normalize_title`] of `title`, or `u:<path>` for unmatched files.
    pub group_key: String,
    pub year: Option<u16>,
    pub ids: ProviderIds,
    pub confidence: f32,
    pub evidence: Vec<String>,
    /// No usable title: named after the file, queued for AI-assisted matching.
    pub unmatched: bool,
    /// Shows only: the file's [`show_folder`]. A show owns the folders its
    /// episodes live in; another folder means another show unless provider
    /// ids agree, or one folder is inside the other (`Show/` and
    /// `Show/alt versions/` are one show).
    pub folder: Option<String>,
}

/// One episode slot within a season.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EpisodeKey {
    Number(u16),
    /// Date-based shows (`YYYY-MM-DD`).
    Date(String),
}

impl EpisodeKey {
    fn group_key(&self) -> String {
        match self {
            EpisodeKey::Number(n) => format!("e{n}"),
            EpisodeKey::Date(d) => format!("d{d}"),
        }
    }
}

/// Where a file goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Placement {
    /// Not a library item (samples).
    Ignore {
        reason: &'static str,
    },
    Movie {
        work: WorkTarget,
        edition: Option<String>,
        part: Option<u8>,
    },
    Episodes {
        show: WorkTarget,
        season: u16,
        episodes: Vec<EpisodeKey>,
        episode_title: Option<String>,
        /// Set when the number came from anime-style absolute numbering
        /// (season 1 is a placeholder): the show's `episode_map` may place
        /// it properly (see [`crate::Writer::rekey_episodes`]).
        absolute: Option<u32>,
    },
    Extra(ExtraTarget),
}

/// An extra and the hints for finding its parent.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtraTarget {
    pub title: String,
    pub kind: ExtraKind,
    /// Folder of the owning item (see [`extras_parent_dir`]).
    pub parent_dir: Option<String>,
    /// The guess's title/year, if the parser attributed it to a parent.
    pub parent_title: Option<String>,
    pub parent_year: Option<u16>,
}

fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

fn unmatched(guess: &NameGuess, rel_path: &str) -> Placement {
    let stem = rel_path.rsplit_once('.').map(|(s, _)| s).unwrap_or(rel_path);
    Placement::Movie {
        work: WorkTarget {
            title: clean_filename(rel_path),
            group_key: format!("u:{}", normalize_title(stem)),
            year: guess.year,
            ids: ProviderIds::default(),
            confidence: guess.confidence.min(0.3),
            evidence: guess.evidence.clone(),
            unmatched: true,
            folder: None,
        },
        edition: guess.edition.clone(),
        part: guess.part,
    }
}

/// Decide where a file belongs from its guess and path (no I/O).
pub fn place(guess: &NameGuess, rel_path: &str) -> Placement {
    match guess.kind {
        GuessKind::Sample => Placement::Ignore { reason: "sample" },
        GuessKind::Movie => {
            let title = non_empty(&guess.title);
            if title.is_none() && guess.ids.is_empty() {
                return unmatched(guess, rel_path);
            }
            let title = title
                .map(str::to_string)
                .unwrap_or_else(|| clean_filename(rel_path));
            Placement::Movie {
                work: WorkTarget {
                    group_key: normalize_title(&title),
                    title,
                    year: guess.year,
                    ids: guess.ids.clone(),
                    confidence: guess.confidence,
                    evidence: guess.evidence.clone(),
                    unmatched: false,
                    folder: None,
                },
                edition: guess.edition.clone(),
                part: guess.part,
            }
        }
        GuessKind::Episode => {
            let Some(show) = non_empty(&guess.title) else {
                return unmatched(guess, rel_path);
            };
            let mut season = guess.season;
            let mut episodes: Vec<EpisodeKey> =
                guess.episodes.iter().map(|&e| EpisodeKey::Number(e)).collect();
            if episodes.is_empty() {
                if let Some(abs) = guess.absolute_episode.and_then(|a| u16::try_from(a).ok()) {
                    // Anime-style absolute numbering without a season: season 1.
                    episodes.push(EpisodeKey::Number(abs));
                } else if let Some(date) = non_empty(&guess.air_date) {
                    // Date-based shows: season = the year (Plex convention).
                    season = season.or_else(|| date.get(..4).and_then(|y| y.parse().ok()));
                    episodes.push(EpisodeKey::Date(date.to_string()));
                } else {
                    return unmatched(guess, rel_path);
                }
            }
            episodes.dedup();
            Placement::Episodes {
                show: WorkTarget {
                    title: show.to_string(),
                    group_key: normalize_title(show),
                    year: guess.year,
                    ids: guess.ids.clone(),
                    confidence: guess.confidence,
                    evidence: guess.evidence.clone(),
                    unmatched: false,
                    folder: show_folder(rel_path),
                },
                season: season.unwrap_or(1),
                episodes,
                episode_title: non_empty(&guess.episode_title).map(str::to_string),
                absolute: if guess.episodes.is_empty() {
                    guess.absolute_episode
                } else {
                    None
                },
            }
        }
        GuessKind::Extra => Placement::Extra(ExtraTarget {
            title: non_empty(&guess.episode_title)
                .map(str::to_string)
                .unwrap_or_else(|| clean_filename(rel_path)),
            kind: guess.extra.unwrap_or(ExtraKind::Other),
            parent_dir: extras_parent_dir(rel_path),
            parent_title: non_empty(&guess.title).map(str::to_string),
            parent_year: guess.year,
        }),
        GuessKind::Unknown => unmatched(guess, rel_path),
    }
}

// ---- resolution (inside the writer transaction) -----------------------------

/// Per-job state shared by the resolve functions: the clock, a cache of
/// resolved ids (episodes of one show hit the same show/season rows hundreds
/// of times per batch) and the items created so far.
pub(crate) struct Resolver {
    pub now: i64,
    pub library_id: Id,
    cache: HashMap<String, Id>,
    bumped: std::collections::HashSet<Id>,
    /// `(show, folder)` claims already made by this job.
    claimed: std::collections::HashSet<(Id, String)>,
    /// Items created by this job, in creation order.
    pub created: Vec<Id>,
    /// `(kind, parent)` of every item created by this job. A parent created
    /// in this job cannot have stored children yet, so child lookups skip
    /// the SELECT; recompute skips re-reading kind/parent.
    pub created_meta: HashMap<Id, (&'static str, Option<Id>)>,
    /// `max_attempts` for jobs enqueued by this job; 0 = enqueue none
    /// ([`crate::WriterConfig::enqueue_jobs`] off).
    pub job_max_attempts: u32,
}

impl Resolver {
    pub fn new(library_id: Id, now: i64) -> Self {
        Resolver {
            now,
            library_id,
            cache: HashMap::new(),
            bumped: Default::default(),
            claimed: Default::default(),
            created: Vec::new(),
            created_meta: HashMap::new(),
            job_max_attempts: 5,
        }
    }

    /// Drop cached resolutions that point at deleted items (a work merged
    /// away mid-job must not be resolved to again).
    pub(crate) fn forget(&mut self, deleted: &std::collections::BTreeSet<Id>) {
        self.cache.retain(|_, id| !deleted.contains(id));
    }
}

struct NewItem<'a> {
    kind: &'static str,
    parent_id: Option<Id>,
    group_key: &'a str,
    title: &'a str,
    year: Option<u16>,
    season_number: Option<u16>,
    episode_number: Option<u16>,
    air_date: Option<&'a str>,
    extra_kind: Option<String>,
    ids: Option<&'a ProviderIds>,
    match_state: &'a str,
    confidence: Option<f32>,
    evidence: Option<&'a [String]>,
}

fn insert_item(conn: &Connection, r: &mut Resolver, n: NewItem<'_>) -> Result<Id> {
    let evidence = match n.evidence {
        Some(e) if !e.is_empty() => Some(serde_json::to_string(e)?),
        _ => None,
    };
    let ids = n.ids.cloned().unwrap_or_default();
    conn.prepare_cached(
        "INSERT INTO item(library_id, kind, parent_id, group_key, title, sort_title, year,
                          season_number, episode_number, air_date, extra_kind, tmdb_id, tvdb_id, imdb_id,
                          match_state, match_confidence, match_evidence, added_at, content_added_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?18, ?18)",
    )?
    .execute(params![
        r.library_id,
        n.kind,
        n.parent_id,
        n.group_key,
        n.title,
        sort_title(n.title),
        n.year,
        n.season_number,
        n.episode_number,
        n.air_date,
        n.extra_kind,
        ids.tmdb.map(crate::model::clamp_i64),
        ids.tvdb.map(crate::model::clamp_i64),
        ids.imdb,
        n.match_state,
        n.confidence.map(f64::from),
        evidence,
        r.now,
    ])?;
    let id = conn.last_insert_rowid();
    r.created.push(id);
    r.created_meta.insert(id, (n.kind, n.parent_id));
    if crate::fts::indexable(n.kind, n.title) {
        let doc = crate::fts::Doc {
            kind: n.kind.to_string(),
            title: n.title.to_string(),
            original_title: None,
            overview: None,
        };
        crate::fts::add(conn, id, &doc)?;
    }
    // flux-meta picks these up: match, then art, for every new top-level work.
    if n.parent_id.is_none() && r.job_max_attempts > 0 {
        let key = crate::jobs::match_key(id, n.group_key, n.year, &ids);
        crate::jobs::enqueue_for_new_work(
            conn,
            id,
            &key,
            n.match_state == "unmatched",
            r.now,
            r.job_max_attempts,
        )?;
    }
    Ok(id)
}

/// A top-level item's parse identity changed (it adopted a year, or learned a
/// provider id): queue a fresh match unless a provider or the user already
/// decided. Supersedes a queued match job of the item.
fn requeue_match(conn: &Connection, r: &Resolver, id: Id) -> Result<()> {
    type Row = (
        String,
        Option<u16>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        String,
    );
    let row: Option<Row> = conn
        .prepare_cached(
            "SELECT group_key, year, tmdb_id, tvdb_id, imdb_id, match_state FROM item WHERE id = ?1",
        )?
        .query_row([id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .optional()?;
    let Some((group_key, year, tmdb, tvdb, imdb, state)) = row else {
        return Ok(());
    };
    if matches!(state.as_str(), "matched" | "locked") || r.job_max_attempts == 0 {
        return Ok(());
    }
    let ids = ProviderIds {
        tmdb: tmdb.map(|v| v as u64),
        tvdb: tvdb.map(|v| v as u64),
        imdb,
    };
    let spec = crate::jobs::JobSpec::new(JOB_MATCH, crate::jobs::match_key(id, &group_key, year, &ids))
        .item(id)
        .priority(if state == "unmatched" {
            crate::jobs::priority::UNMATCHED
        } else {
            crate::jobs::priority::MATCH
        });
    crate::jobs::enqueue(conn, &spec, r.now, r.job_max_attempts)?;
    Ok(())
}

/// Record that `show` has episodes in `folder` (no-op if already known).
fn claim_folder(conn: &Connection, r: &mut Resolver, show: Id, folder: &str) -> Result<()> {
    if r.claimed.insert((show, folder.to_string())) {
        conn.prepare_cached("INSERT OR IGNORE INTO item_folder(folder, item_id) VALUES (?1, ?2)")?
            .execute(params![folder, show])?;
    }
    Ok(())
}

/// Do two id sets contradict each other (both know a provider id and differ)?
fn ids_conflict(a: &ProviderIds, b: &ProviderIds) -> bool {
    matches!((a.tmdb, b.tmdb), (Some(x), Some(y)) if x != y)
        || matches!((a.tvdb, b.tvdb), (Some(x), Some(y)) if x != y)
        || matches!((&a.imdb, &b.imdb), (Some(x), Some(y)) if x != y)
}

/// Find or create the top-level movie/show item for a work.
///
/// `prefer` is the show the file belongs to now (a moved or re-parsed
/// episode file): when its folder has no same-titled owner yet, the file
/// stays with that show and the show claims the folder, so renaming a show
/// folder keeps the show (and everyone's watch state).
pub(crate) fn resolve_work(
    conn: &Connection,
    r: &mut Resolver,
    kind: &'static str,
    w: &WorkTarget,
    prefer: Option<Id>,
) -> Result<Id> {
    let cache_key = format!(
        "{kind}|{}|{:?}|{:?}|{:?}|{:?}|{:?}",
        w.group_key, w.year, w.ids.tmdb, w.ids.tvdb, w.ids.imdb, w.folder
    );
    if let Some(&id) = r.cache.get(&cache_key) {
        return Ok(id);
    }
    let found = find_work(conn, r, kind, w, prefer)?;
    let id = match found {
        Some(id) => id,
        None => insert_item(
            conn,
            r,
            NewItem {
                kind,
                parent_id: None,
                group_key: &w.group_key,
                title: &w.title,
                year: w.year,
                season_number: None,
                episode_number: None,
                air_date: None,
                extra_kind: None,
                ids: Some(&w.ids),
                match_state: if w.unmatched { "unmatched" } else { "parsed" },
                confidence: Some(w.confidence),
                evidence: Some(&w.evidence),
            },
        )?,
    };
    if kind == "show" {
        if let Some(folder) = &w.folder {
            claim_folder(conn, r, id, folder)?;
        }
    }
    r.cache.insert(cache_key, id);
    Ok(id)
}

/// A same-titled top-level candidate: `(id, year, ids)`.
type Candidate = (Id, Option<u16>, ProviderIds);

/// The year rules among `cands`: an exact year wins; a yearless guess joins
/// the only candidate; a dated guess adopts the only yearless candidate
/// (writing the year). Anything else is ambiguous → `None` (a new item).
fn pick_by_year(
    conn: &Connection,
    r: &Resolver,
    w: &WorkTarget,
    cands: &[&Candidate],
) -> Result<Option<Candidate>> {
    if let Some(c) = cands.iter().find(|(_, y, _)| *y == w.year) {
        return Ok(Some((*c).clone()));
    }
    if w.year.is_none() {
        return Ok(match cands {
            [only] => Some((*only).clone()),
            _ => None,
        });
    }
    let yearless: Vec<&&Candidate> = cands.iter().filter(|(_, y, _)| y.is_none()).collect();
    if let [only] = yearless.as_slice() {
        // Adopt the year we just learned.
        conn.prepare_cached("UPDATE item SET year = ?2, updated_at = ?3 WHERE id = ?1")?
            .execute(params![only.0, w.year, r.now])?;
        requeue_match(conn, r, only.0)?;
        return Ok(Some((**only).clone()));
    }
    Ok(None)
}

/// Shows (among `ids`) that own `folder`.
fn folder_owners(conn: &Connection, folder: &str) -> Result<std::collections::HashSet<Id>> {
    let mut stmt = conn.prepare_cached("SELECT item_id FROM item_folder WHERE folder = ?1")?;
    let rows = stmt.query_map([folder], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn owns_any_folder(conn: &Connection, show: Id) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT 1 FROM item_folder WHERE item_id = ?1 LIMIT 1")?
        .query_row([show], |_| Ok(()))
        .optional()?
        .is_some())
}

/// A same-titled show that owns a folder around a file's show folder
/// (`around`: `show` for `show/alt versions`) or inside it (`inside`).
struct Nested<'a> {
    cand: &'a Candidate,
    around: bool,
    inside: bool,
}

/// The candidates that own a folder around `folder` or inside it. Lengths
/// are in characters, as SQLite's `substr` counts them.
fn nested_owners<'a>(conn: &Connection, cands: &[&'a Candidate], folder: &str) -> Result<Vec<Nested<'a>>> {
    let mut stmt = conn.prepare_cached(
        "SELECT COALESCE(MAX(substr(?2, 1, length(folder) + 1) = folder || '/'), 0),
                COALESCE(MAX(substr(folder, 1, ?3 + 1) = ?2 || '/'), 0)
           FROM item_folder WHERE item_id = ?1",
    )?;
    let mut nested = Vec::new();
    for &c in cands {
        let (around, inside): (bool, bool) = stmt
            .query_row(params![c.0, folder, folder.chars().count() as i64], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
        if around || inside {
            nested.push(Nested {
                cand: c,
                around,
                inside,
            });
        }
    }
    Ok(nested)
}

/// The year rules between shows in nested folders (`Show/` and
/// `Show/alt versions/`): the same year, or none on either side, is the
/// same show. A subfolder takes the year of the folder around it, but a
/// folder never takes a subfolder's year, so a dated series kept inside an
/// undated same-titled folder (`Show/Show (1978)/`) stays another show.
/// The rules give the same result in either scan order. `None`: no show.
fn pick_nested(
    conn: &Connection,
    r: &Resolver,
    w: &WorkTarget,
    nested: &[Nested<'_>],
) -> Result<Option<Candidate>> {
    if let Some(n) = nested.iter().find(|n| n.cand.1 == w.year) {
        return Ok(Some(n.cand.clone()));
    }
    if w.year.is_none() {
        // An undated subfolder of a dated show's folder: that show.
        let around: Vec<&Nested<'_>> = nested.iter().filter(|n| n.around).collect();
        return Ok(match around.as_slice() {
            [only] => Some(only.cand.clone()),
            _ => None,
        });
    }
    // A dated folder around an undated show's subfolder: that show, which
    // takes the folder's year.
    let inside: Vec<&Nested<'_>> = nested.iter().filter(|n| n.inside && n.cand.1.is_none()).collect();
    if let [only] = inside.as_slice() {
        conn.prepare_cached("UPDATE item SET year = ?2, updated_at = ?3 WHERE id = ?1")?
            .execute(params![only.cand.0, w.year, r.now])?;
        requeue_match(conn, r, only.cand.0)?;
        return Ok(Some(only.cand.clone()));
    }
    Ok(None)
}

fn find_work(
    conn: &Connection,
    r: &Resolver,
    kind: &str,
    w: &WorkTarget,
    prefer: Option<Id>,
) -> Result<Option<Id>> {
    // 1. An explicit provider id is authoritative.
    let by_id: [(&str, Option<rusqlite::types::Value>); 3] = [
        ("tmdb_id", w.ids.tmdb.map(|v| crate::model::clamp_i64(v).into())),
        ("tvdb_id", w.ids.tvdb.map(|v| crate::model::clamp_i64(v).into())),
        ("imdb_id", w.ids.imdb.clone().map(Into::into)),
    ];
    for (col, val) in by_id {
        let Some(val) = val else { continue };
        let sql = format!(
            "SELECT id FROM item WHERE library_id = ?1 AND kind = ?2 AND {col} = ?3 AND parent_id IS NULL ORDER BY id LIMIT 1"
        );
        if let Some(id) = conn
            .prepare_cached(&sql)?
            .query_row(params![r.library_id, kind, val], |row| row.get(0))
            .optional()?
        {
            return Ok(Some(id));
        }
    }
    if w.unmatched {
        // Unmatched files only group with the same path stem.
        return conn
            .prepare_cached(
                "SELECT id FROM item WHERE library_id = ?1 AND kind = ?2 AND group_key = ?3 AND parent_id IS NULL ORDER BY id LIMIT 1",
            )?
            .query_row(params![r.library_id, kind, w.group_key], |row| row.get(0))
            .optional()
            .map_err(Into::into);
    }
    // 2. Same normalised title, compatible ids, then year rules.
    let mut stmt = conn.prepare_cached(
        "SELECT id, year, tmdb_id, tvdb_id, imdb_id FROM item
         WHERE library_id = ?1 AND kind = ?2 AND group_key = ?3 AND parent_id IS NULL ORDER BY id",
    )?;
    let candidates: Vec<(Id, Option<u16>, ProviderIds)> = stmt
        .query_map(params![r.library_id, kind, w.group_key], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                ProviderIds {
                    tmdb: row.get::<_, Option<i64>>(2)?.map(|v| v as u64),
                    tvdb: row.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                    imdb: row.get(4)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let compatible: Vec<&Candidate> = candidates
        .iter()
        .filter(|(_, _, ids)| !ids_conflict(ids, &w.ids))
        .collect();
    let picked: Option<Candidate> = match (kind, &w.folder) {
        // Shows: a different show folder is a different show (ids agreeing
        // was handled above), unless one folder holds the other. Within one
        // folder, the year rules apply.
        ("show", Some(folder)) => {
            let owners = folder_owners(conn, folder)?;
            let owned: Vec<&Candidate> = compatible
                .iter()
                .copied()
                .filter(|c| owners.contains(&c.0))
                .collect();
            if !owned.is_empty() {
                pick_by_year(conn, r, w, &owned)?
            } else if let Some(p) = prefer.and_then(|p| compatible.iter().find(|c| c.0 == p)) {
                // The file's current show follows it into the new folder
                // (folder renamed) unless the years contradict; a yearless
                // show adopts the new folder's year.
                pick_by_year(conn, r, w, &[*p])?
            } else if let Some(c) = pick_nested(conn, r, w, &nested_owners(conn, &compatible, folder)?)? {
                // A subfolder of a show's folder (`Show/alt versions/`), or
                // the folder around one: the same show, so its files become
                // more versions of the same episodes, in either scan order.
                Some(c)
            } else {
                // Shows that don't know their folders yet (files at the
                // root, or catalogs from before v3) still join by year rules.
                let mut free = Vec::new();
                for c in &compatible {
                    if !owns_any_folder(conn, c.0)? {
                        free.push(*c);
                    }
                }
                pick_by_year(conn, r, w, &free)?
            }
        }
        _ => pick_by_year(conn, r, w, &compatible)?,
    };
    let Some((id, _, have)) = picked else {
        return Ok(None);
    };
    // Fill in ids the stored item lacks (never overwrite).
    let fill_tmdb = have.tmdb.is_none() && w.ids.tmdb.is_some();
    let fill_tvdb = have.tvdb.is_none() && w.ids.tvdb.is_some();
    let fill_imdb = have.imdb.is_none() && w.ids.imdb.is_some();
    if fill_tmdb || fill_tvdb || fill_imdb {
        conn.prepare_cached(
            "UPDATE item SET tmdb_id = COALESCE(tmdb_id, ?2), tvdb_id = COALESCE(tvdb_id, ?3),
                             imdb_id = COALESCE(imdb_id, ?4), updated_at = ?5 WHERE id = ?1",
        )?
        .execute(params![
            id,
            w.ids.tmdb.map(crate::model::clamp_i64),
            w.ids.tvdb.map(crate::model::clamp_i64),
            w.ids.imdb,
            r.now
        ])?;
        requeue_match(conn, r, id)?;
    }
    Ok(Some(id))
}

/// Find or create the season item of a show.
pub(crate) fn resolve_season(conn: &Connection, r: &mut Resolver, show_id: Id, season: u16) -> Result<Id> {
    let key = format!("s{season}");
    let title = if season == 0 {
        "Specials".to_string()
    } else {
        format!("Season {season}")
    };
    resolve_child(conn, r, show_id, "season", &key, || NewChild {
        title,
        season_number: Some(season),
        ..Default::default()
    })
}

/// Find or create an episode item of a season. A real `title` replaces a
/// placeholder ("Episode 3") on an existing, not provider-matched episode.
pub(crate) fn resolve_episode(
    conn: &Connection,
    r: &mut Resolver,
    show_id: Id,
    season_id: Id,
    season: u16,
    ep: &EpisodeKey,
    title: Option<&str>,
) -> Result<Id> {
    let key = ep.group_key();
    let cache_key = format!("{season_id}|episode|{key}");
    let existed = r.cache.contains_key(&cache_key);
    let before = r.created.len();
    let id = resolve_child(conn, r, season_id, "episode", &key, || {
        let (number, date) = match ep {
            EpisodeKey::Number(n) => (Some(*n), None),
            EpisodeKey::Date(d) => (None, Some(d.clone())),
        };
        NewChild {
            title: title.map(str::to_string).unwrap_or_else(|| match ep {
                EpisodeKey::Number(n) => format!("Episode {n}"),
                EpisodeKey::Date(d) => d.clone(),
            }),
            season_number: Some(season),
            episode_number: number,
            air_date: date,
            ..Default::default()
        }
    })?;
    if r.created.len() > before {
        // A new episode makes its season and show "recently added".
        for parent in [season_id, show_id] {
            if r.bumped.insert(parent) {
                conn.prepare_cached(
                    "UPDATE item SET content_added_at = ?2 WHERE id = ?1 AND content_added_at < ?2",
                )?
                .execute(params![parent, r.now])?;
                if parent == show_id && !r.created_meta.contains_key(&show_id) && r.job_max_attempts > 0 {
                    // …and needs thumbs: re-arm the show's art job.
                    crate::jobs::rearm_art(conn, show_id, r.now, r.job_max_attempts)?;
                }
            }
        }
    } else if let (Some(t), false) = (title, existed) {
        // Replace a placeholder title (never indexed) with the parsed one.
        let n = conn
            .prepare_cached(
                "UPDATE item SET title = ?2, sort_title = ?3, updated_at = ?4
                 WHERE id = ?1 AND title GLOB 'Episode [0-9]*' AND match_state NOT IN ('matched','locked')",
            )?
            .execute(params![id, t, sort_title(t), r.now])?;
        if n > 0 {
            if let Some(doc) = crate::fts::doc(conn, id)? {
                crate::fts::add(conn, id, &doc)?;
            }
        }
    }
    Ok(id)
}

/// Find or create an extra under its parent movie/show.
pub(crate) fn resolve_extra(
    conn: &Connection,
    r: &mut Resolver,
    parent_id: Id,
    x: &ExtraTarget,
) -> Result<Id> {
    let key = format!("x:{}", normalize_title(&x.title));
    resolve_child(conn, r, parent_id, "extra", &key, || NewChild {
        title: x.title.clone(),
        extra_kind: enum_to_text(&x.kind),
        ..Default::default()
    })
}

#[derive(Default)]
struct NewChild {
    title: String,
    season_number: Option<u16>,
    episode_number: Option<u16>,
    air_date: Option<String>,
    extra_kind: Option<String>,
}

fn resolve_child(
    conn: &Connection,
    r: &mut Resolver,
    parent_id: Id,
    kind: &'static str,
    group_key: &str,
    make: impl FnOnce() -> NewChild,
) -> Result<Id> {
    let cache_key = format!("{parent_id}|{kind}|{group_key}");
    if let Some(&id) = r.cache.get(&cache_key) {
        return Ok(id);
    }
    let found: Option<Id> = if r.created_meta.contains_key(&parent_id) {
        None // a parent created by this job has no stored children yet
    } else {
        conn.prepare_cached(
            "SELECT id FROM item WHERE parent_id = ?1 AND kind = ?2 AND group_key = ?3 ORDER BY id LIMIT 1",
        )?
        .query_row(params![parent_id, kind, group_key], |row| row.get(0))
        .optional()?
    };
    let id = match found {
        Some(id) => id,
        None => {
            let c = make();
            insert_item(
                conn,
                r,
                NewItem {
                    kind,
                    parent_id: Some(parent_id),
                    group_key,
                    title: &c.title,
                    year: None,
                    season_number: c.season_number,
                    episode_number: c.episode_number,
                    air_date: c.air_date.as_deref(),
                    extra_kind: c.extra_kind,
                    ids: None,
                    match_state: "parsed",
                    confidence: None,
                    evidence: None,
                },
            )?
        }
    };
    r.cache.insert(cache_key, id);
    Ok(id)
}

/// Find the movie/show an extra belongs to: first structurally (a movie or
/// episode file under the extra's parent folder, preferring files directly in
/// it), then by the guess's title/year. `None` → keep the extra pending.
pub(crate) fn find_extra_parent(
    conn: &Connection,
    library_id: Id,
    root_id: Id,
    x: &ExtraTarget,
) -> Result<Option<Id>> {
    if let Some(dir) = x.parent_dir.as_deref().filter(|d| !d.is_empty()) {
        // Range scan on UNIQUE(root_id, rel_path): '0' is the byte after '/'.
        let lo = format!("{dir}/");
        let hi = format!("{dir}0");
        let hit: Option<(Id, String, Option<Id>)> = conn
            .prepare_cached(
                "SELECT i.id, i.kind, i.parent_id FROM file f
                   JOIN asset a ON a.file_id = f.id
                   JOIN item i ON i.id = a.item_id
                  WHERE f.root_id = ?1 AND f.rel_path >= ?2 AND f.rel_path < ?3
                    AND i.kind IN ('movie','episode') AND f.state <> 'missing'
                  ORDER BY instr(substr(f.rel_path, ?4), '/') = 0 DESC, f.rel_path
                  LIMIT 1",
            )?
            // `substr` counts characters, not bytes (non-ASCII folder names).
            .query_row(params![root_id, lo, hi, lo.chars().count() as i64 + 1], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .optional()?;
        if let Some((id, kind, season)) = hit {
            if kind == "movie" {
                return Ok(Some(id));
            }
            // episode → season → show
            if let Some(season) = season {
                let show: Option<Option<Id>> = conn
                    .prepare_cached("SELECT parent_id FROM item WHERE id = ?1")?
                    .query_row([season], |row| row.get(0))
                    .optional()?;
                if let Some(Some(show)) = show {
                    return Ok(Some(show));
                }
            }
        }
    }
    if let Some(title) = &x.parent_title {
        let found = conn
            .prepare_cached(
                "SELECT id FROM item WHERE library_id = ?1 AND parent_id IS NULL AND kind IN ('movie','show')
                   AND group_key = ?2 AND (?3 IS NULL OR year IS ?3 OR year IS NULL)
                 ORDER BY (year IS ?3) DESC, id LIMIT 1",
            )?
            .query_row(
                params![library_id, normalize_title(title), x.parent_year],
                |row| row.get(0),
            )
            .optional()?;
        return Ok(found);
    }
    Ok(None)
}

/// The one film whose files sit directly in `dir` (not in its subfolders);
/// `None` when there is none, or more than one (a flat folder of films
/// gives no single owner for extras).
pub(crate) fn sole_movie_in_dir(conn: &Connection, root_id: Id, dir: &str) -> Result<Option<Id>> {
    if dir.is_empty() {
        return Ok(None);
    }
    let lo = format!("{dir}/");
    let hi = format!("{dir}0");
    let ids: Vec<Id> = conn
        .prepare_cached(
            "SELECT DISTINCT i.id FROM file f
               JOIN asset a ON a.file_id = f.id
               JOIN item i ON i.id = a.item_id
              WHERE f.root_id = ?1 AND f.rel_path >= ?2 AND f.rel_path < ?3
                AND instr(substr(f.rel_path, ?4), '/') = 0
                AND f.state <> 'missing' AND i.kind = 'movie' AND i.parent_id IS NULL
              LIMIT 2",
        )?
        .query_map(params![root_id, lo, hi, lo.chars().count() as i64 + 1], |row| {
            row.get(0)
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(match ids.as_slice() {
        [only] => Some(*only),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guess(kind: GuessKind, title: Option<&str>) -> NameGuess {
        let mut g = NameGuess::unknown();
        g.kind = kind;
        g.title = title.map(str::to_string);
        g.confidence = 0.9;
        g
    }

    #[test]
    fn normalisation_groups_punctuation_case_and_diacritics() {
        assert_eq!(normalize_title("Mad Max: Fury Road"), "mad max fury road");
        assert_eq!(normalize_title("Mad.Max.-.Fury.Road"), "mad max fury road");
        assert_eq!(
            normalize_title("Léon: The Professional"),
            normalize_title("Leon - The Professional")
        );
        assert_eq!(normalize_title("Ocean's Eleven"), "oceans eleven");
        assert_eq!(normalize_title("Law & Order: SVU"), "law and order svu");
        assert_eq!(normalize_title("  "), "");
        assert_eq!(normalize_title("Æon Flux"), "aeon flux");
    }

    #[test]
    fn sort_titles_drop_articles_and_pad_numbers() {
        assert_eq!(sort_title("The Matrix"), "matrix");
        assert_eq!(sort_title("A Quiet Place"), "quiet place");
        assert_eq!(sort_title("The"), "the");
        assert!(sort_title("Rocky 2") < sort_title("Rocky 10"));
        assert_eq!(sort_title("2001: A Space Odyssey"), "002001 a space odyssey");
    }

    #[test]
    fn clean_filename_strips_noise() {
        assert_eq!(clean_filename("Movies/x/Some_Video.[x264].mkv"), "Some Video");
        assert_eq!(clean_filename("a/b/My.Home.Movie.2019.mp4"), "My Home Movie 2019");
        assert_eq!(clean_filename("[group] - .mkv"), "[group] - ");
    }

    #[test]
    fn extras_parent_dir_skips_extras_folders() {
        assert_eq!(
            extras_parent_dir("Movies/Mad Max (2015)/Featurettes/Making Of.mp4").as_deref(),
            Some("Movies/Mad Max (2015)")
        );
        assert_eq!(
            extras_parent_dir("Movies/Mad Max (2015)/Mad Max-trailer.mp4").as_deref(),
            Some("Movies/Mad Max (2015)")
        );
        assert_eq!(extras_parent_dir("Trailers/x.mp4"), None);
        assert_eq!(extras_parent_dir("x.mp4"), None);
    }

    #[test]
    fn samples_are_ignored_and_unknowns_are_unmatched_movies() {
        assert_eq!(
            place(&guess(GuessKind::Sample, Some("Alien")), "a/Sample/s.mkv"),
            Placement::Ignore { reason: "sample" }
        );
        match place(&NameGuess::unknown(), "Movies/weird_clip.[abc].mkv") {
            Placement::Movie { work, .. } => {
                assert!(work.unmatched);
                assert_eq!(work.title, "weird clip");
                assert!(work.group_key.starts_with("u:"));
            }
            other => panic!("{other:?}"),
        }
        // An episode guess without a show name is also unmatched.
        let mut g = guess(GuessKind::Episode, None);
        g.episodes = vec![1];
        assert!(matches!(
            place(&g, "x/E01.mkv"),
            Placement::Movie {
                work: WorkTarget { unmatched: true, .. },
                ..
            }
        ));
    }

    #[test]
    fn multi_episode_absolute_and_date_based_episodes() {
        let mut g = guess(GuessKind::Episode, Some("Better Call Saul"));
        g.season = Some(1);
        g.episodes = vec![9, 10];
        match place(&g, "TV/x.ts") {
            Placement::Episodes { season, episodes, .. } => {
                assert_eq!(season, 1);
                assert_eq!(episodes, vec![EpisodeKey::Number(9), EpisodeKey::Number(10)]);
            }
            other => panic!("{other:?}"),
        }
        let mut g = guess(GuessKind::Episode, Some("One Piece"));
        g.absolute_episode = Some(1001);
        assert!(
            matches!(place(&g, "x"), Placement::Episodes { season: 1, ref episodes, .. } if episodes == &[EpisodeKey::Number(1001)])
        );
        let mut g = guess(GuessKind::Episode, Some("The Daily Show"));
        g.air_date = Some("2024-03-05".into());
        assert!(
            matches!(place(&g, "x"), Placement::Episodes { season: 2024, ref episodes, .. } if episodes == &[EpisodeKey::Date("2024-03-05".into())])
        );
    }

    #[test]
    fn extras_carry_parent_hints() {
        let mut g = guess(GuessKind::Extra, Some("Mad Max: Fury Road"));
        g.year = Some(2015);
        g.extra = Some(ExtraKind::Featurette);
        match place(&g, "Movies/Mad Max - Fury Road (2015)/Featurettes/Making Of.mp4") {
            Placement::Extra(x) => {
                assert_eq!(x.title, "Making Of");
                assert_eq!(x.kind, ExtraKind::Featurette);
                assert_eq!(x.parent_dir.as_deref(), Some("Movies/Mad Max - Fury Road (2015)"));
                assert_eq!(x.parent_year, Some(2015));
            }
            other => panic!("{other:?}"),
        }
    }
}
