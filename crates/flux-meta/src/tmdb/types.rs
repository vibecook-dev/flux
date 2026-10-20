//! Typed TMDB v3 response shapes (only the fields flux uses; unknown fields
//! are ignored so TMDB can add fields without breaking us).
//!
//! Human-readable text is wrapped in [`TmdbText`] so it cannot silently flow
//! into the AI path (see the type's docs).

use serde::{Deserialize, Deserializer, Serialize};

use crate::util::year_of;

/// Text content that came from TMDB: titles, names, overviews, taglines.
///
/// TMDB API Terms §1.C forbid using "the TMDB APIs or TMDB Content in
/// connection with, including for training, a machine learning (ML) or
/// artificial intelligence (AI) based Application" (research/05 §2.2). flux
/// therefore keeps TMDB text in this wrapper, which deliberately implements
/// **no** `Display`, `Deref<Target = str>`, `AsRef<str>`, `Borrow<str>` or
/// `Into<String>`, so it cannot be passed where the AI path expects text by
/// accident. [`crate::ai::LocalEvidence`] has no constructor that accepts a
/// string at all. Reading the text is explicit via [`TmdbText::as_str`], which
/// is for showing it to people and for deterministic (non-AI) matching only.
#[derive(Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TmdbText(#[serde(deserialize_with = "null_as_empty")] String);

impl TmdbText {
    /// The text, for display to users and for deterministic matching **only**.
    /// Never hand it to an LLM/VLM/image model (TMDB API ToS §1.C).
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// True when TMDB sent an empty string or `null`.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for TmdbText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TmdbText({:?})", self.0)
    }
}

#[cfg(test)]
impl From<&str> for TmdbText {
    fn from(s: &str) -> Self {
        TmdbText(s.to_owned())
    }
}

fn null_as_empty<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

/// TMDB sends unknown dates as `""`; treat that as `None`.
fn empty_as_none<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(Option::<String>::deserialize(d)?.filter(|s| !s.is_empty()))
}

/// `null` → `0.0` for numeric fields TMDB occasionally nulls.
fn null_as_zero<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    Ok(Option::<f64>::deserialize(d)?.unwrap_or(0.0))
}

/// A page of results (`/search/*`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Paged<T> {
    pub page: u32,
    pub results: Vec<T>,
    #[serde(default)]
    pub total_pages: u32,
    #[serde(default)]
    pub total_results: u32,
}

/// One `/search/movie` (or `/find`) movie result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MovieResult {
    pub id: u64,
    #[serde(default)]
    pub title: TmdbText,
    #[serde(default)]
    pub original_title: TmdbText,
    #[serde(default)]
    pub original_language: Option<String>,
    #[serde(default, deserialize_with = "empty_as_none")]
    pub release_date: Option<String>,
    #[serde(default, deserialize_with = "null_as_zero")]
    pub popularity: f64,
    #[serde(default)]
    pub vote_count: u32,
    #[serde(default)]
    pub overview: TmdbText,
    #[serde(default)]
    pub poster_path: Option<String>,
    #[serde(default)]
    pub backdrop_path: Option<String>,
    #[serde(default)]
    pub adult: bool,
    #[serde(default)]
    pub video: bool,
    #[serde(default)]
    pub genre_ids: Vec<u32>,
}

impl MovieResult {
    /// Release year from `release_date`.
    pub fn year(&self) -> Option<u16> {
        year_of(self.release_date.as_deref())
    }
}

/// One `/search/tv` (or `/find`) series result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TvResult {
    pub id: u64,
    #[serde(default)]
    pub name: TmdbText,
    #[serde(default)]
    pub original_name: TmdbText,
    #[serde(default)]
    pub original_language: Option<String>,
    #[serde(default, deserialize_with = "empty_as_none")]
    pub first_air_date: Option<String>,
    #[serde(default, deserialize_with = "null_as_zero")]
    pub popularity: f64,
    #[serde(default)]
    pub vote_count: u32,
    #[serde(default)]
    pub overview: TmdbText,
    #[serde(default)]
    pub poster_path: Option<String>,
    #[serde(default)]
    pub backdrop_path: Option<String>,
    #[serde(default)]
    pub origin_country: Vec<String>,
    #[serde(default)]
    pub genre_ids: Vec<u32>,
}

impl TvResult {
    /// First-air year.
    pub fn year(&self) -> Option<u16> {
        year_of(self.first_air_date.as_deref())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Genre {
    pub id: u32,
    pub name: TmdbText,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpokenLanguage {
    pub iso_639_1: String,
    #[serde(default)]
    pub english_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionRef {
    pub id: u64,
    pub name: TmdbText,
    #[serde(default)]
    pub poster_path: Option<String>,
    #[serde(default)]
    pub backdrop_path: Option<String>,
}

/// An image entry from `append_to_response=images`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageInfo {
    pub file_path: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub aspect_ratio: f64,
    /// `None` = textless (the best kind for backdrops).
    #[serde(default)]
    pub iso_639_1: Option<String>,
    #[serde(default)]
    pub vote_average: f64,
    #[serde(default)]
    pub vote_count: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Images {
    #[serde(default)]
    pub backdrops: Vec<ImageInfo>,
    #[serde(default)]
    pub posters: Vec<ImageInfo>,
    #[serde(default)]
    pub logos: Vec<ImageInfo>,
}

/// `external_ids` (movie, tv, episode). Only the ids flux stores.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExternalIds {
    #[serde(default, deserialize_with = "empty_as_none")]
    pub imdb_id: Option<String>,
    #[serde(default)]
    pub tvdb_id: Option<u64>,
    #[serde(default, deserialize_with = "empty_as_none")]
    pub wikidata_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseDate {
    #[serde(default)]
    pub certification: String,
    #[serde(default)]
    pub release_date: String,
    /// 1 Premiere, 2 Theatrical (limited), 3 Theatrical, 4 Digital, 5 Physical, 6 TV.
    #[serde(rename = "type", default)]
    pub release_type: u8,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CountryReleases {
    pub iso_3166_1: String,
    pub release_dates: Vec<ReleaseDate>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReleaseDates {
    #[serde(default)]
    pub results: Vec<CountryReleases>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlternativeTitle {
    #[serde(default)]
    pub iso_3166_1: String,
    pub title: TmdbText,
    #[serde(rename = "type", default)]
    pub kind: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AlternativeTitles {
    /// Movies use `titles`; TV uses `results`.
    #[serde(default, alias = "results")]
    pub titles: Vec<AlternativeTitle>,
}

/// `/movie/{id}` with `append_to_response=images,external_ids,release_dates`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MovieDetails {
    pub id: u64,
    #[serde(default, deserialize_with = "empty_as_none")]
    pub imdb_id: Option<String>,
    #[serde(default)]
    pub title: TmdbText,
    #[serde(default)]
    pub original_title: TmdbText,
    #[serde(default)]
    pub original_language: Option<String>,
    #[serde(default, deserialize_with = "empty_as_none")]
    pub release_date: Option<String>,
    /// Minutes. `0`/`null` when unknown.
    #[serde(default)]
    pub runtime: Option<u32>,
    #[serde(default, deserialize_with = "null_as_zero")]
    pub popularity: f64,
    #[serde(default)]
    pub overview: TmdbText,
    #[serde(default)]
    pub tagline: TmdbText,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub genres: Vec<Genre>,
    #[serde(default)]
    pub poster_path: Option<String>,
    #[serde(default)]
    pub backdrop_path: Option<String>,
    #[serde(default)]
    pub belongs_to_collection: Option<CollectionRef>,
    #[serde(default)]
    pub spoken_languages: Vec<SpokenLanguage>,
    #[serde(default)]
    pub images: Option<Images>,
    #[serde(default)]
    pub external_ids: Option<ExternalIds>,
    #[serde(default)]
    pub release_dates: Option<ReleaseDates>,
    #[serde(default)]
    pub alternative_titles: Option<AlternativeTitles>,
}

impl MovieDetails {
    pub fn year(&self) -> Option<u16> {
        year_of(self.release_date.as_deref())
    }
    /// Runtime in minutes, `None` when TMDB reports 0/unknown.
    pub fn runtime_minutes(&self) -> Option<u32> {
        self.runtime.filter(|&m| m > 0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeasonSummary {
    #[serde(default)]
    pub id: u64,
    pub season_number: u16,
    #[serde(default)]
    pub episode_count: u32,
    #[serde(default)]
    pub name: TmdbText,
    #[serde(default, deserialize_with = "empty_as_none")]
    pub air_date: Option<String>,
    #[serde(default)]
    pub poster_path: Option<String>,
}

/// `/tv/{id}` with `append_to_response=external_ids,images`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TvDetails {
    pub id: u64,
    #[serde(default)]
    pub name: TmdbText,
    #[serde(default)]
    pub original_name: TmdbText,
    #[serde(default)]
    pub original_language: Option<String>,
    #[serde(default, deserialize_with = "empty_as_none")]
    pub first_air_date: Option<String>,
    #[serde(default, deserialize_with = "empty_as_none")]
    pub last_air_date: Option<String>,
    /// Typical episode runtimes in minutes (often empty on newer shows).
    #[serde(default)]
    pub episode_run_time: Vec<u32>,
    #[serde(default)]
    pub number_of_seasons: Option<u32>,
    #[serde(default)]
    pub number_of_episodes: Option<u32>,
    #[serde(default)]
    pub seasons: Vec<SeasonSummary>,
    #[serde(default, deserialize_with = "null_as_zero")]
    pub popularity: f64,
    #[serde(default)]
    pub overview: TmdbText,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(rename = "type", default)]
    pub show_type: Option<String>,
    #[serde(default)]
    pub genres: Vec<Genre>,
    #[serde(default)]
    pub origin_country: Vec<String>,
    #[serde(default)]
    pub poster_path: Option<String>,
    #[serde(default)]
    pub backdrop_path: Option<String>,
    #[serde(default)]
    pub external_ids: Option<ExternalIds>,
    #[serde(default)]
    pub images: Option<Images>,
}

impl TvDetails {
    pub fn year(&self) -> Option<u16> {
        year_of(self.first_air_date.as_deref())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    #[serde(default)]
    pub id: u64,
    pub episode_number: u16,
    pub season_number: u16,
    #[serde(default)]
    pub name: TmdbText,
    #[serde(default, deserialize_with = "empty_as_none")]
    pub air_date: Option<String>,
    #[serde(default)]
    pub runtime: Option<u32>,
    #[serde(default)]
    pub overview: TmdbText,
    #[serde(default)]
    pub still_path: Option<String>,
    /// "standard", "finale", "mid_season" (newer field).
    #[serde(default)]
    pub episode_type: Option<String>,
}

/// `/tv/{id}/season/{n}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeasonDetails {
    #[serde(default)]
    pub id: u64,
    pub season_number: u16,
    #[serde(default)]
    pub name: TmdbText,
    #[serde(default, deserialize_with = "empty_as_none")]
    pub air_date: Option<String>,
    #[serde(default)]
    pub overview: TmdbText,
    #[serde(default)]
    pub poster_path: Option<String>,
    #[serde(default)]
    pub episodes: Vec<Episode>,
}

/// A `/find` episode hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindEpisode {
    pub id: u64,
    pub show_id: u64,
    pub season_number: u16,
    pub episode_number: u16,
    #[serde(default)]
    pub name: TmdbText,
}

/// `/find/{external_id}?external_source=…`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FindResults {
    #[serde(default)]
    pub movie_results: Vec<MovieResult>,
    #[serde(default)]
    pub tv_results: Vec<TvResult>,
    #[serde(default)]
    pub tv_episode_results: Vec<FindEpisode>,
}

/// External id namespaces accepted by `/find`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalSource {
    Imdb,
    Tvdb,
    Wikidata,
}

impl ExternalSource {
    /// The `external_source` query value.
    pub fn as_param(self) -> &'static str {
        match self {
            ExternalSource::Imdb => "imdb_id",
            ExternalSource::Tvdb => "tvdb_id",
            ExternalSource::Wikidata => "wikidata_id",
        }
    }
}

/// `/configuration` → image base URLs and sizes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Configuration {
    pub images: ImageConfiguration,
    #[serde(default)]
    pub change_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageConfiguration {
    #[serde(default)]
    pub base_url: String,
    pub secure_base_url: String,
    #[serde(default)]
    pub backdrop_sizes: Vec<String>,
    #[serde(default)]
    pub logo_sizes: Vec<String>,
    #[serde(default)]
    pub poster_sizes: Vec<String>,
    #[serde(default)]
    pub profile_sizes: Vec<String>,
    #[serde(default)]
    pub still_sizes: Vec<String>,
}

impl ImageConfiguration {
    /// Full HTTPS URL of an image, e.g. `url("w500", "/abc.jpg")`.
    pub fn url(&self, size: &str, file_path: &str) -> String {
        format!("{}{}{}", self.secure_base_url, size, file_path)
    }
}

/// TMDB's error body: `{"status_code": 7, "status_message": "Invalid API key…", "success": false}`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ErrorBody {
    #[serde(default)]
    pub status_code: Option<i64>,
    #[serde(default)]
    pub status_message: Option<String>,
}
