//! What a filename/folder parser guesses about a media file.

use serde::{Deserialize, Serialize};

/// Bump when the parser's output changes in a way that should trigger re-parsing.
///
/// 4: in a mixed library without a TV or season folder, an episode without
/// an explicit marker needs a numbered series of files beside it, a dated
/// recording is never one, and `other`/`others` folders hold no extras;
/// [`LibraryKind::Videos`].
pub const PARSER_VERSION: u16 = 4;

/// What kind of library a root belongs to. It is a strong prior for parsing:
/// `Show 1x03.mkv` in a movie library is still probably an episode, but
/// `1917.mkv` in a movie library is a title, not an episode number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryKind {
    Movies,
    Shows,
    Mixed,
    /// Home videos, clips and anything else that is neither a film nor a
    /// series (Plex's "Other Videos"): every file is its own item, titled
    /// after the file and browsed by folder. Nothing is grouped (no shows,
    /// versions or extras; only a true multi-part stack is one item) and
    /// nothing is matched against a metadata source.
    Videos,
}

impl LibraryKind {
    /// Every kind, in the order the API lists them.
    pub const ALL: [LibraryKind; 4] = [
        LibraryKind::Movies,
        LibraryKind::Shows,
        LibraryKind::Mixed,
        LibraryKind::Videos,
    ];

    /// The wire and catalog name: `movies`, `shows`, `mixed`, `videos`.
    pub fn as_str(self) -> &'static str {
        match self {
            LibraryKind::Movies => "movies",
            LibraryKind::Shows => "shows",
            LibraryKind::Mixed => "mixed",
            LibraryKind::Videos => "videos",
        }
    }

    /// Whether a metadata source (TMDB) may match this library's items.
    pub fn matches_metadata(self) -> bool {
        self != LibraryKind::Videos
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuessKind {
    Movie,
    Episode,
    /// Bonus material attached to a movie or show (trailers, featurettes, …).
    Extra,
    /// A release "sample" clip; never shown as a library item.
    Sample,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtraKind {
    Trailer,
    Featurette,
    BehindTheScenes,
    DeletedScene,
    Interview,
    Short,
    Scene,
    Other,
}

/// External ids embedded in paths, e.g. `[tmdbid-603]`, `{imdb-tt0133093}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderIds {
    pub tmdb: Option<u64>,
    pub imdb: Option<String>,
    pub tvdb: Option<u64>,
}

impl ProviderIds {
    pub fn is_empty(&self) -> bool {
        self.tmdb.is_none() && self.imdb.is_none() && self.tvdb.is_none()
    }
}

/// A parser's reading of one media file's path.
///
/// Conventions flux-names follows and flux-catalog's grouping relies on:
/// - **Episodes:** `title`/`year` name the show; `season` + `episodes` place
///   the file (several episodes for `S01E01-E02`); `air_date` for date-based shows.
/// - **Anime absolute numbering** (`[Group] Show - 105`): `absolute_episode`
///   is set, `episodes` is empty and `season` is `None`. Inside a season
///   folder the number is also taken as season-relative: `season` comes from
///   the folder and `episodes = [n]`. Mapping absolute numbers to seasons
///   needs provider data (flux-meta); the catalog files them under season 1.
/// - **Season packs** (a file with only `S01`): `kind = Episode`, `season` set,
///   no `episodes`, confidence 0.4. Not placeable: the catalog keeps it as an
///   unmatched item for review.
/// - **Extras:** `title`/`year` name the *parent* work (movie or show),
///   `episode_title` is the extra's own name ("Making Of"), `extra` its kind.
///   The catalog prefers the folder structure and falls back to `title`.
/// - **Versions:** `edition` carries `{edition-…}` tags, edition words and
///   Jellyfin multi-version labels (`Title (2010) - Bob's Cut`); a label that
///   is a resolution (`Title (2010) - 2160p`) goes to `resolution` instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NameGuess {
    pub kind: GuessKind,
    /// Movie title or show name, cleaned for display/search.
    pub title: Option<String>,
    /// The other-script half of a bilingual name (`示例剧.Some.Show.S01E01…` →
    /// title "Some Show", original title "示例剧"). The catalog stores it as
    /// the item's `original_title`, so search finds both names.
    #[serde(default)]
    pub original_title: Option<String>,
    pub year: Option<u16>,
    pub season: Option<u16>,
    /// One entry normally; several for multi-episode files (S01E01-E02).
    pub episodes: Vec<u16>,
    /// Anime-style absolute episode number.
    pub absolute_episode: Option<u32>,
    /// Date-based shows, `YYYY-MM-DD`.
    pub air_date: Option<String>,
    pub episode_title: Option<String>,
    /// "Director's Cut", "Extended Edition", …
    pub edition: Option<String>,
    /// Multi-part movies (`cd1`, `part2`).
    pub part: Option<u8>,
    pub resolution: Option<String>,
    pub source: Option<String>,
    pub video_codec: Option<String>,
    pub release_group: Option<String>,
    pub extra: Option<ExtraKind>,
    pub ids: ProviderIds,
    /// 0.0–1.0. Below ~0.5 the item is queued for AI-assisted matching.
    pub confidence: f32,
    /// Which rules fired, for explainability ("fast path: Show/Season NN/…").
    pub evidence: Vec<String>,
}

impl NameGuess {
    pub fn unknown() -> Self {
        NameGuess {
            kind: GuessKind::Unknown,
            title: None,
            original_title: None,
            year: None,
            season: None,
            episodes: Vec::new(),
            absolute_episode: None,
            air_date: None,
            episode_title: None,
            edition: None,
            part: None,
            resolution: None,
            source: None,
            video_codec: None,
            release_group: None,
            extra: None,
            ids: ProviderIds::default(),
            confidence: 0.0,
            evidence: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_kinds_serialise_as_their_names() {
        for k in LibraryKind::ALL {
            let wire = serde_json::to_string(&k).unwrap();
            assert_eq!(wire, format!("\"{}\"", k.as_str()));
            assert_eq!(serde_json::from_str::<LibraryKind>(&wire).unwrap(), k);
        }
        assert_eq!(LibraryKind::Videos.as_str(), "videos");
        assert!(!LibraryKind::Videos.matches_metadata());
        assert!(LibraryKind::Mixed.matches_metadata());
    }
}
