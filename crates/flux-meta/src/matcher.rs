//! Deterministic, explainable matching of a parsed file against provider candidates.
//!
//! The scorer follows research/05 §3.2:
//!
//! ```text
//! movie = 0.40·title + 0.20·year + 0.25·runtime + 0.10·language + 0.05·popularity
//! tv    = 0.40·title + 0.15·year + 0.25·episode-exists + 0.10·language + 0.05·runtime + 0.05·popularity
//! accept if score ≥ 0.85 and margin to the runner-up ≥ 0.10
//! review if score ≥ 0.60;  otherwise no match (→ AI hypothesis lane)
//! ```
//!
//! A signal that cannot be computed (no year in the export, no probed
//! duration, `und` audio) counts as a neutral `0.5`, so missing evidence can
//! never push a candidate over the auto-accept line on its own. Jellyfin's
//! scorer uses only title and year (TmdbUtils.cs:188-255); runtime and audio
//! language are the evidence that separates remakes (*Mulan* 1998 vs 2020) and
//! translated titles (*Parasite* with Korean audio).
//!
//! Explicit ids in the path (`[tmdbid-603]`, `{imdb-tt0133093}`) short-circuit
//! everything. The model never decides here: AI hypotheses only add query
//! titles ([`MatchQuery::extra_titles`]); acceptance is always this scorer's.

use flux_core::{GuessKind, NameGuess, ProbeResult, ProviderIds};
use serde::{Deserialize, Serialize};

use crate::export_index::{ExportCandidate, ExportKind};
use crate::lang::to_iso639_1;
use crate::normalize::{normalize_title, title_similarity};
use crate::tmdb::{MovieDetails, MovieResult, TvDetails, TvResult};

/// Movie or series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Movie,
    Tv,
}

/// Where a candidate came from (for evidence strings and auditing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateSource {
    /// TMDB daily ID export (offline; no year, original title only).
    TmdbExport,
    /// `/search/movie` or `/search/tv`.
    TmdbSearch,
    /// `/movie/{id}` or `/tv/{id}` (has runtime and seasons).
    TmdbDetails,
    /// `/find/{external_id}`.
    TmdbFind,
    /// Any other provider (Wikidata, TVmaze, a plugin…).
    Other,
}

/// A season's size, for episode-existence checks and absolute numbering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeasonInfo {
    pub season_number: u16,
    pub episode_count: u32,
}

/// A provider candidate in provider-neutral form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub source: CandidateSource,
    pub kind: MediaKind,
    /// Provider ids; `tmdb` is the movie id or the **series** id.
    pub ids: ProviderIds,
    pub title: String,
    pub original_title: Option<String>,
    pub alt_titles: Vec<String>,
    pub year: Option<u16>,
    /// Movie runtime, or typical episode runtime for series (minutes).
    pub runtime_min: Option<u32>,
    /// ISO 639-1.
    pub original_language: Option<String>,
    pub popularity: Option<f32>,
    /// Series only: seasons with episode counts (from `/tv/{id}`).
    pub seasons: Vec<SeasonInfo>,
}

impl Candidate {
    fn base(source: CandidateSource, kind: MediaKind, tmdb: u64, title: &str) -> Self {
        Candidate {
            source,
            kind,
            ids: ProviderIds {
                tmdb: Some(tmdb),
                imdb: None,
                tvdb: None,
            },
            title: title.to_owned(),
            original_title: None,
            alt_titles: Vec::new(),
            year: None,
            runtime_min: None,
            original_language: None,
            popularity: None,
            seasons: Vec::new(),
        }
    }

    /// From the offline export prefilter (title is already normalised; no year).
    pub fn from_export(c: &ExportCandidate) -> Self {
        let kind = match c.kind {
            ExportKind::Movie => MediaKind::Movie,
            ExportKind::Tv => MediaKind::Tv,
        };
        let mut out = Candidate::base(CandidateSource::TmdbExport, kind, u64::from(c.id), &c.title);
        out.popularity = Some(c.popularity);
        out
    }

    /// From a `/search/movie` or `/find` result.
    pub fn from_tmdb_movie(r: &MovieResult) -> Self {
        let mut c = Candidate::base(
            CandidateSource::TmdbSearch,
            MediaKind::Movie,
            r.id,
            r.title.as_str(),
        );
        c.original_title = Some(r.original_title.as_str().to_owned()).filter(|t| !t.is_empty());
        c.year = r.year();
        c.original_language = r.original_language.clone();
        c.popularity = Some(r.popularity as f32);
        c
    }

    /// From `/movie/{id}` (adds runtime, IMDb id and alternative titles if appended).
    pub fn from_tmdb_movie_details(d: &MovieDetails) -> Self {
        let mut c = Candidate::base(
            CandidateSource::TmdbDetails,
            MediaKind::Movie,
            d.id,
            d.title.as_str(),
        );
        c.original_title = Some(d.original_title.as_str().to_owned()).filter(|t| !t.is_empty());
        c.year = d.year();
        c.runtime_min = d.runtime_minutes();
        c.original_language = d.original_language.clone();
        c.popularity = Some(d.popularity as f32);
        c.ids.imdb = d
            .imdb_id
            .clone()
            .or_else(|| d.external_ids.as_ref().and_then(|e| e.imdb_id.clone()));
        if let Some(alt) = &d.alternative_titles {
            c.alt_titles = alt.titles.iter().map(|t| t.title.as_str().to_owned()).collect();
        }
        c
    }

    /// From a `/search/tv` or `/find` result.
    pub fn from_tmdb_tv(r: &TvResult) -> Self {
        let mut c = Candidate::base(CandidateSource::TmdbSearch, MediaKind::Tv, r.id, r.name.as_str());
        c.original_title = Some(r.original_name.as_str().to_owned()).filter(|t| !t.is_empty());
        c.year = r.year();
        c.original_language = r.original_language.clone();
        c.popularity = Some(r.popularity as f32);
        c
    }

    /// From `/tv/{id}` (adds seasons, typical runtime and external ids).
    pub fn from_tmdb_tv_details(d: &TvDetails) -> Self {
        let mut c = Candidate::base(CandidateSource::TmdbDetails, MediaKind::Tv, d.id, d.name.as_str());
        c.original_title = Some(d.original_name.as_str().to_owned()).filter(|t| !t.is_empty());
        c.year = d.year();
        c.original_language = d.original_language.clone();
        c.popularity = Some(d.popularity as f32);
        c.runtime_min = d.episode_run_time.first().copied().filter(|&m| m > 0);
        c.seasons = d
            .seasons
            .iter()
            .map(|s| SeasonInfo {
                season_number: s.season_number,
                episode_count: s.episode_count,
            })
            .collect();
        if let Some(ext) = &d.external_ids {
            c.ids.imdb = ext.imdb_id.clone();
            c.ids.tvdb = ext.tvdb_id;
        }
        c
    }
}

/// What we know about the file, independent of any provider.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MatchQuery {
    /// `None` = unknown (score both kinds).
    pub kind: Option<MediaKind>,
    /// Title from the name parser (first) and any extra titles (AI hypotheses,
    /// folder names). Similarity is the best over all of them.
    pub titles: Vec<String>,
    pub year: Option<u16>,
    pub season: Option<u16>,
    pub episodes: Vec<u16>,
    pub absolute_episode: Option<u32>,
    /// Multi-part movie file (`cd1`): its duration is not the movie's runtime.
    pub part: Option<u8>,
    pub edition: Option<String>,
    /// Probed duration in minutes.
    pub duration_min: Option<f64>,
    /// ISO 639-1 audio languages (undetermined tracks excluded).
    pub audio_languages: Vec<String>,
    /// Explicit ids from the path.
    pub ids: ProviderIds,
}

impl MatchQuery {
    /// Build from the name parser's guess and (optionally) the probe.
    pub fn from_guess(guess: &NameGuess, probe: Option<&ProbeResult>) -> Self {
        let kind = match guess.kind {
            GuessKind::Movie => Some(MediaKind::Movie),
            GuessKind::Episode => Some(MediaKind::Tv),
            _ => None,
        };
        let mut q = MatchQuery {
            kind,
            titles: guess.title.iter().cloned().collect(),
            year: guess.year,
            season: guess.season,
            episodes: guess.episodes.clone(),
            absolute_episode: guess.absolute_episode,
            part: guess.part,
            edition: guess.edition.clone(),
            ids: guess.ids.clone(),
            ..MatchQuery::default()
        };
        if let Some(p) = probe {
            q.duration_min = p.duration_ms.map(|ms| ms as f64 / 60_000.0);
            for t in p.audio() {
                if let Some(l) = t.language.as_deref().and_then(to_iso639_1) {
                    if !q.audio_languages.contains(&l) {
                        q.audio_languages.push(l);
                    }
                }
            }
        }
        q
    }

    /// Add more titles to try (deduplicated after normalisation).
    pub fn extra_titles<I: IntoIterator<Item = String>>(mut self, titles: I) -> Self {
        for t in titles {
            let n = normalize_title(&t);
            if !n.is_empty() && !self.titles.iter().any(|x| normalize_title(x) == n) {
                self.titles.push(t);
            }
        }
        self
    }
}

/// Signal weights (they need not sum to 1; the score is normalised).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Weights {
    pub title: f32,
    pub year: f32,
    pub runtime: f32,
    pub language: f32,
    pub popularity: f32,
    pub episode: f32,
}

/// Matcher thresholds and weights.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MatcherConfig {
    pub movie: Weights,
    pub tv: Weights,
    /// Auto-accept threshold (default 0.85).
    pub accept_score: f32,
    /// Required lead over the runner-up for auto-accept (default 0.10).
    pub accept_margin: f32,
    /// Below this, no match (default 0.60).
    pub review_score: f32,
    /// Value used for a signal that cannot be computed (default 0.5).
    pub neutral: f32,
    /// Candidates of the other kind are multiplied by this (default 0.6).
    pub kind_mismatch_factor: f32,
}

impl Default for MatcherConfig {
    fn default() -> Self {
        MatcherConfig {
            movie: Weights {
                title: 0.40,
                year: 0.20,
                runtime: 0.25,
                language: 0.10,
                popularity: 0.05,
                episode: 0.0,
            },
            tv: Weights {
                title: 0.40,
                year: 0.15,
                runtime: 0.05,
                language: 0.10,
                popularity: 0.05,
                episode: 0.25,
            },
            accept_score: 0.85,
            accept_margin: 0.10,
            review_score: 0.60,
            neutral: 0.5,
            kind_mismatch_factor: 0.6,
        }
    }
}

/// Per-signal scores in `0..=1`; `None` = not computable (scored as neutral).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Signals {
    pub title: f32,
    pub year: Option<f32>,
    pub runtime: Option<f32>,
    pub language: Option<f32>,
    pub popularity: Option<f32>,
    pub episode: Option<f32>,
}

/// A candidate with its score and the reasons for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoredCandidate {
    pub ids: ProviderIds,
    pub kind: MediaKind,
    pub source: CandidateSource,
    pub title: String,
    pub year: Option<u16>,
    pub score: f32,
    pub signals: Signals,
    /// `(season, episodes)` after absolute-number mapping, for series.
    pub episode: Option<(u16, Vec<u16>)>,
    pub evidence: Vec<String>,
}

/// The matcher's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Confident: use it.
    Accept,
    /// Plausible but not certain: show in the review queue.
    Review,
    /// Nothing plausible: queue for the AI hypothesis lane / manual search.
    NoMatch,
    /// The path names an IMDb/TVDB id none of the candidates carry: resolve it
    /// with TMDB `/find` first, then match again.
    LookupExternalId,
}

/// Result of matching one file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchResult {
    pub decision: Decision,
    /// Ids of the accepted (or best) candidate.
    pub ids: ProviderIds,
    pub confidence: f32,
    /// Lead over the runner-up (distinct ids).
    pub margin: f32,
    /// Top candidates, best first (at most 5).
    pub ranked: Vec<ScoredCandidate>,
    /// Human-readable reasons for the decision.
    pub evidence: Vec<String>,
}

impl MatchResult {
    /// The best candidate, if any.
    pub fn best(&self) -> Option<&ScoredCandidate> {
        self.ranked.first()
    }
}

/// Maps an anime-style absolute episode number to `(season, episode)` for a candidate.
pub trait AbsoluteMapper: Send + Sync {
    fn map(&self, candidate: &Candidate, absolute: u32) -> Option<(u16, u16)>;
}

/// Default mapping: walk regular seasons (1, 2, …) subtracting their episode
/// counts. Correct when the provider's seasons are the release's cours; TMDB
/// "Absolute" episode groups or AniDB/TVDB mappings can replace it.
#[derive(Debug, Clone, Copy, Default)]
pub struct CumulativeSeasons;

impl AbsoluteMapper for CumulativeSeasons {
    fn map(&self, candidate: &Candidate, absolute: u32) -> Option<(u16, u16)> {
        if absolute == 0 {
            return None;
        }
        let mut seasons: Vec<&SeasonInfo> =
            candidate.seasons.iter().filter(|s| s.season_number > 0).collect();
        seasons.sort_by_key(|s| s.season_number);
        let mut remaining = absolute;
        for s in seasons {
            if remaining <= s.episode_count {
                return Some((s.season_number, u16::try_from(remaining).ok()?));
            }
            remaining -= s.episode_count;
        }
        None
    }
}

/// The deterministic matcher.
pub struct Matcher {
    cfg: MatcherConfig,
    mapper: Box<dyn AbsoluteMapper>,
}

impl Default for Matcher {
    fn default() -> Self {
        Matcher::new(MatcherConfig::default())
    }
}

impl std::fmt::Debug for Matcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Matcher")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

impl Matcher {
    /// A matcher with the [`CumulativeSeasons`] absolute mapper.
    pub fn new(cfg: MatcherConfig) -> Self {
        Matcher {
            cfg,
            mapper: Box::new(CumulativeSeasons),
        }
    }

    /// Replace the absolute-episode mapping (anime).
    pub fn with_absolute_mapper(mut self, mapper: Box<dyn AbsoluteMapper>) -> Self {
        self.mapper = mapper;
        self
    }

    /// The configuration in use.
    pub fn config(&self) -> &MatcherConfig {
        &self.cfg
    }

    /// Score every candidate and decide.
    pub fn match_candidates(&self, q: &MatchQuery, candidates: &[Candidate]) -> MatchResult {
        // 1. Explicit ids short-circuit.
        if let Some(r) = self.explicit(q, candidates) {
            return r;
        }
        let norm_titles: Vec<String> = q
            .titles
            .iter()
            .map(|t| normalize_title(t))
            .filter(|t| !t.is_empty())
            .collect();
        let mut scored: Vec<ScoredCandidate> = candidates
            .iter()
            .map(|c| self.score_norm(q, &norm_titles, c))
            .collect();
        scored.sort_by(|a, b| b.score.total_cmp(&a.score));
        // Keep the best entry per (kind, tmdb id): search and details can both supply the same title.
        let mut ranked: Vec<ScoredCandidate> = Vec::new();
        for s in scored {
            if !ranked
                .iter()
                .any(|r| r.kind == s.kind && r.ids.tmdb == s.ids.tmdb && s.ids.tmdb.is_some())
            {
                ranked.push(s);
            }
        }
        let best = ranked.first().map_or(0.0, |s| s.score);
        let second = ranked.get(1).map_or(0.0, |s| s.score);
        let margin = best - second;
        let mut evidence = Vec::new();
        let decision = if ranked.is_empty() {
            evidence.push("no candidates".to_owned());
            Decision::NoMatch
        } else if best >= self.cfg.accept_score && margin >= self.cfg.accept_margin {
            evidence.push(format!(
                "accepted: score {best:.2} ≥ {:.2} and margin {margin:.2}",
                self.cfg.accept_score
            ));
            Decision::Accept
        } else if best >= self.cfg.review_score {
            evidence.push(if best >= self.cfg.accept_score {
                format!("review: score {best:.2} but runner-up within {margin:.2}")
            } else {
                format!(
                    "review: best score {best:.2} below auto-accept {:.2}",
                    self.cfg.accept_score
                )
            });
            Decision::Review
        } else {
            evidence.push(format!(
                "no match: best score {best:.2} < {:.2}",
                self.cfg.review_score
            ));
            Decision::NoMatch
        };
        ranked.truncate(5);
        MatchResult {
            decision,
            ids: ranked.first().map(|s| s.ids.clone()).unwrap_or_default(),
            confidence: best,
            margin,
            ranked,
            evidence,
        }
    }

    /// Score one candidate (normalises the query's titles; for loops use
    /// [`Matcher::match_candidates`], which normalises once).
    pub fn score(&self, q: &MatchQuery, c: &Candidate) -> ScoredCandidate {
        let norm: Vec<String> = q
            .titles
            .iter()
            .map(|t| normalize_title(t))
            .filter(|t| !t.is_empty())
            .collect();
        self.score_norm(q, &norm, c)
    }

    fn explicit(&self, q: &MatchQuery, candidates: &[Candidate]) -> Option<MatchResult> {
        let hit = |c: &Candidate| {
            (q.ids.tmdb.is_some() && c.ids.tmdb == q.ids.tmdb && q.kind.is_none_or(|k| k == c.kind))
                || (q.ids.imdb.is_some() && c.ids.imdb == q.ids.imdb)
                || (q.ids.tvdb.is_some() && c.ids.tvdb == q.ids.tvdb)
        };
        if let Some(c) = candidates.iter().find(|c| hit(c)) {
            let mut s = self.score(q, c);
            s.score = 1.0;
            s.evidence
                .insert(0, "explicit provider id in the path".to_owned());
            return Some(MatchResult {
                decision: Decision::Accept,
                ids: c.ids.clone(),
                confidence: 1.0,
                margin: 1.0,
                ranked: vec![s],
                evidence: vec![format!("explicit id {}", fmt_ids(&q.ids))],
            });
        }
        if let Some(tmdb) = q.ids.tmdb {
            // A TMDB id needs no lookup: accept it; details are fetched by id.
            return Some(MatchResult {
                decision: Decision::Accept,
                ids: ProviderIds {
                    tmdb: Some(tmdb),
                    imdb: q.ids.imdb.clone(),
                    tvdb: q.ids.tvdb,
                },
                confidence: 1.0,
                margin: 1.0,
                ranked: Vec::new(),
                evidence: vec![format!("explicit tmdb id {tmdb} in the path")],
            });
        }
        if !q.ids.is_empty() {
            return Some(MatchResult {
                decision: Decision::LookupExternalId,
                ids: q.ids.clone(),
                confidence: 0.0,
                margin: 0.0,
                ranked: Vec::new(),
                evidence: vec![format!(
                    "explicit id {} not among candidates: resolve with /find",
                    fmt_ids(&q.ids)
                )],
            });
        }
        None
    }

    fn score_norm(&self, q: &MatchQuery, norm_titles: &[String], c: &Candidate) -> ScoredCandidate {
        let w = match c.kind {
            MediaKind::Movie => self.cfg.movie,
            MediaKind::Tv => self.cfg.tv,
        };
        let mut ev = Vec::new();
        let mut sig = Signals::default();

        // Title: best over query titles × candidate titles.
        let cand_titles = std::iter::once(c.title.as_str())
            .chain(c.original_title.as_deref())
            .chain(c.alt_titles.iter().map(String::as_str));
        let mut best_title = (0.0f32, String::new());
        for ct in cand_titles {
            let cn = normalize_title(ct);
            for qn in norm_titles {
                let s = title_similarity(qn, &cn);
                if s > best_title.0 {
                    best_title = (s, format!("'{qn}' ~ '{cn}'"));
                }
            }
        }
        sig.title = best_title.0;
        ev.push(format!("title {} → {:.2}", best_title.1, sig.title));

        // Year: Δ0 → 1, Δ1 → 0.8, Δ2 → 0.3, else 0.
        if let (Some(qy), Some(cy)) = (q.year, c.year) {
            let d = qy.abs_diff(cy);
            let s = match d {
                0 => 1.0,
                1 => 0.8,
                2 => 0.3,
                _ => 0.0,
            };
            sig.year = Some(s);
            ev.push(format!("year {qy} vs {cy} (Δ{d}) → {s:.2}"));
        } else if q.year.is_some() {
            ev.push("year: candidate has none (neutral)".to_owned());
        }

        // Episode existence (series).
        let mut episode = None;
        if c.kind == MediaKind::Tv && !c.seasons.is_empty() {
            let (season, eps) = match (q.season, q.absolute_episode) {
                (Some(s), _) if !q.episodes.is_empty() => (Some(s), q.episodes.clone()),
                (_, Some(abs)) => match self.mapper.map(c, abs) {
                    Some((s, e)) => {
                        ev.push(format!("absolute {abs} → S{s:02}E{e:02}"));
                        (Some(s), vec![e])
                    }
                    None => {
                        ev.push(format!(
                            "absolute {abs} beyond the series' {} episodes",
                            total_eps(c)
                        ));
                        (None, Vec::new())
                    }
                },
                _ => (None, Vec::new()),
            };
            let s = match season {
                Some(sn) => match c.seasons.iter().find(|x| x.season_number == sn) {
                    Some(info) if eps.iter().all(|&e| u32::from(e) <= info.episode_count && e > 0) => {
                        ev.push(format!(
                            "S{sn:02}E{:02} exists ({} episodes)",
                            eps.first().copied().unwrap_or(0),
                            info.episode_count
                        ));
                        1.0
                    }
                    Some(info) => {
                        ev.push(format!("season {sn} has only {} episodes", info.episode_count));
                        0.15
                    }
                    None => {
                        ev.push(format!("season {sn} does not exist"));
                        0.0
                    }
                },
                None if q.absolute_episode.is_some() => 0.1,
                None => self.cfg.neutral,
            };
            sig.episode = Some(s);
            if let Some(sn) = season {
                episode = Some((sn, eps));
            }
        }

        // Runtime vs probed duration.
        if let (Some(dur), Some(rt)) = (q.duration_min, c.runtime_min) {
            if q.part.is_some() {
                ev.push("runtime: multi-part file, not comparable".to_owned());
            } else if dur >= 1.0 && rt > 0 {
                let expected = if c.kind == MediaKind::Tv {
                    f64::from(rt) * q.episodes.len().max(1) as f64
                } else {
                    f64::from(rt)
                };
                let delta = (dur - expected).abs();
                let rel = delta / expected;
                let edition = q.edition.is_some();
                let s = if delta <= 3.0 {
                    1.0
                } else if rel <= 0.08 {
                    0.7
                } else if rel <= 0.15 || (edition && rel <= 0.35) {
                    0.4
                } else {
                    0.0
                };
                sig.runtime = Some(s);
                ev.push(format!(
                    "runtime {expected:.0} min vs probed {dur:.1} min (Δ{delta:.1}) → {s:.2}"
                ));
            }
        }

        // Audio language vs original language.
        if let Some(ol) = c
            .original_language
            .as_deref()
            .filter(|_| !q.audio_languages.is_empty())
        {
            let s = if q.audio_languages.iter().any(|l| l == ol) {
                1.0
            } else {
                0.2
            };
            sig.language = Some(s);
            ev.push(format!(
                "audio {:?} vs original language {ol} → {s:.2}",
                q.audio_languages
            ));
        }

        // Popularity prior (log-scaled; TMDB popularity is roughly 0..500).
        if let Some(p) = c.popularity {
            let s = ((1.0 + p.max(0.0)).ln() / 201f32.ln()).clamp(0.0, 1.0);
            sig.popularity = Some(s);
        }

        let n = self.cfg.neutral;
        let parts = [
            (w.title, Some(sig.title)),
            (w.year, sig.year),
            (w.runtime, sig.runtime),
            (w.language, sig.language),
            (w.popularity, sig.popularity),
            (w.episode, sig.episode),
        ];
        let total_w: f32 = parts.iter().map(|(w, _)| *w).sum();
        let mut score =
            parts.iter().map(|(w, s)| w * s.unwrap_or(n)).sum::<f32>() / total_w.max(f32::EPSILON);
        if q.kind.is_some_and(|k| k != c.kind) {
            score *= self.cfg.kind_mismatch_factor;
            ev.push(format!(
                "kind mismatch ({:?} file, {:?} candidate)",
                q.kind, c.kind
            ));
        }
        ScoredCandidate {
            ids: c.ids.clone(),
            kind: c.kind,
            source: c.source,
            title: c.title.clone(),
            year: c.year,
            score,
            signals: sig,
            episode,
            evidence: ev,
        }
    }
}

fn total_eps(c: &Candidate) -> u32 {
    c.seasons
        .iter()
        .filter(|s| s.season_number > 0)
        .map(|s| s.episode_count)
        .sum()
}

fn fmt_ids(ids: &ProviderIds) -> String {
    let mut parts = Vec::new();
    if let Some(t) = ids.tmdb {
        parts.push(format!("tmdb:{t}"));
    }
    if let Some(i) = &ids.imdb {
        parts.push(format!("imdb:{i}"));
    }
    if let Some(t) = ids.tvdb {
        parts.push(format!("tvdb:{t}"));
    }
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn movie(id: u64, title: &str, year: u16, runtime: Option<u32>, lang: &str, pop: f32) -> Candidate {
        let mut c = Candidate::base(CandidateSource::TmdbSearch, MediaKind::Movie, id, title);
        c.year = Some(year);
        c.runtime_min = runtime;
        c.original_language = Some(lang.to_owned());
        c.popularity = Some(pop);
        c
    }

    fn q(title: &str, year: Option<u16>, dur: Option<f64>, audio: &[&str]) -> MatchQuery {
        MatchQuery {
            kind: Some(MediaKind::Movie),
            titles: vec![title.to_owned()],
            year,
            duration_min: dur,
            audio_languages: audio.iter().map(|s| s.to_string()).collect(),
            ..MatchQuery::default()
        }
    }

    #[test]
    fn runtime_separates_remakes() {
        let m = Matcher::default();
        let cands = [
            movie(10674, "Mulan", 1998, Some(88), "en", 20.0),
            movie(337401, "Mulan", 2020, Some(115), "en", 19.7),
        ];
        // No year in the name: the probed duration decides.
        let r = m.match_candidates(&q("Mulan", None, Some(114.6), &["en"]), &cands);
        assert_eq!(r.ids.tmdb, Some(337401));
        // With the year as well, it is an auto-accept.
        let r = m.match_candidates(&q("Mulan", Some(2020), Some(114.6), &["en"]), &cands);
        assert_eq!(r.decision, Decision::Accept, "{r:#?}");
        assert_eq!(r.ids.tmdb, Some(337401));
    }

    #[test]
    fn missing_evidence_is_not_enough_to_accept() {
        let m = Matcher::default();
        let cands = [movie(603, "The Matrix", 1999, None, "en", 48.0)];
        let r = m.match_candidates(&q("The Matrix", Some(1999), None, &[]), &cands);
        assert_eq!(r.decision, Decision::Review, "{r:#?}");
        // Adding a matching runtime makes it an accept.
        let cands = [movie(603, "The Matrix", 1999, Some(136), "en", 48.0)];
        let r = m.match_candidates(&q("The Matrix", Some(1999), Some(136.3), &["en"]), &cands);
        assert_eq!(r.decision, Decision::Accept);
        assert!(r
            .best()
            .unwrap()
            .evidence
            .iter()
            .any(|e| e.starts_with("runtime")));
    }

    #[test]
    fn language_breaks_title_ties() {
        let m = Matcher::default();
        let mut parasite = movie(496243, "Parasite", 2019, Some(133), "ko", 40.0);
        parasite.original_title = Some("기생충".into());
        let other = movie(1322439, "Parasite", 2019, Some(95), "en", 2.0);
        let r = m.match_candidates(
            &q("Parasite", Some(2019), Some(132.5), &["ko"]),
            &[other, parasite],
        );
        assert_eq!(r.ids.tmdb, Some(496243));
        assert_eq!(r.decision, Decision::Accept);
    }

    #[test]
    fn explicit_ids_short_circuit() {
        let m = Matcher::default();
        let mut query = q("Whatever", None, None, &[]);
        query.ids.tmdb = Some(603);
        let r = m.match_candidates(&query, &[]);
        assert_eq!(
            (r.decision, r.ids.tmdb, r.confidence),
            (Decision::Accept, Some(603), 1.0)
        );

        let mut query = q("Whatever", None, None, &[]);
        query.ids.imdb = Some("tt0133093".into());
        assert_eq!(
            m.match_candidates(&query, &[]).decision,
            Decision::LookupExternalId
        );
        let mut c = movie(603, "The Matrix", 1999, None, "en", 48.0);
        c.ids.imdb = Some("tt0133093".into());
        let r = m.match_candidates(&query, &[c]);
        assert_eq!((r.decision, r.ids.tmdb), (Decision::Accept, Some(603)));
    }

    fn show(id: u64, name: &str, year: u16, seasons: &[u32]) -> Candidate {
        let mut c = Candidate::base(CandidateSource::TmdbDetails, MediaKind::Tv, id, name);
        c.year = Some(year);
        c.popularity = Some(30.0);
        c.seasons = std::iter::once(SeasonInfo {
            season_number: 0,
            episode_count: 3,
        })
        .chain(seasons.iter().enumerate().map(|(i, &n)| SeasonInfo {
            season_number: i as u16 + 1,
            episode_count: n,
        }))
        .collect();
        c
    }

    #[test]
    fn tv_episode_existence_and_absolute_mapping() {
        let m = Matcher::default();
        let us = show(2316, "The Office", 2005, &[6, 22, 25]);
        let uk = show(2996, "The Office", 2001, &[6, 6]);
        let tvq = MatchQuery {
            kind: Some(MediaKind::Tv),
            titles: vec!["The Office".into()],
            season: Some(2),
            episodes: vec![14],
            ..MatchQuery::default()
        };
        // Only the US version has an S02E14.
        let r = m.match_candidates(&tvq, &[uk.clone(), us.clone()]);
        assert_eq!(r.ids.tmdb, Some(2316), "{r:#?}");

        let anime = show(209867, "Frieren: Beyond Journey's End", 2023, &[28]);
        let aot = show(1429, "Attack on Titan", 2013, &[25, 12, 22, 30]);
        let absq = MatchQuery {
            kind: Some(MediaKind::Tv),
            titles: vec!["Attack on Titan".into()],
            absolute_episode: Some(27),
            ..MatchQuery::default()
        };
        let r = m.match_candidates(&absq, &[anime, aot]);
        let best = r.best().unwrap();
        assert_eq!(best.ids.tmdb, Some(1429));
        assert_eq!(best.episode, Some((2, vec![2])));
    }

    #[test]
    fn kind_mismatch_penalised() {
        let m = Matcher::default();
        let mut tv = show(1, "Heat", 1995, &[10]);
        tv.popularity = Some(48.0);
        let mv = movie(949, "Heat", 1995, None, "en", 28.0);
        let r = m.match_candidates(&q("Heat", Some(1995), None, &[]), &[tv, mv]);
        assert_eq!(r.ids.tmdb, Some(949));
    }
}
