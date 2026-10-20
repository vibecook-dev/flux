//! AI-assisted identification that never sees TMDB content.
//!
//! # Why the types look like this
//!
//! TMDB's API Terms §1.C prohibit using "the TMDB APIs or TMDB Content in
//! connection with, including for training, a machine learning (ML) or
//! artificial intelligence (AI) based Application" (research/05 §2.2,
//! research/01 §9.4). flux's answer is *AI-isolated mode*: the model only ever
//! sees evidence the user owns, and only proposes a [`Hypothesis`]; the
//! deterministic [`crate::matcher`] queries providers and decides.
//!
//! That rule is enforced by construction:
//!
//! - [`Hypothesizer::hypothesize`] takes a [`LocalEvidence`], whose fields are
//!   private and which has **no constructor that accepts a string**. It can
//!   only be built from a file [`Path`], sibling paths, a
//!   [`flux_core::ProbeResult`] (the file's own headers), a
//!   [`flux_core::NameGuess`] (parsed from the path) and [`OcrText`] (which
//!   can only come from running an [`OcrEngine`] on a [`crate::art::Frame`]
//!   decoded from the user's file).
//! - TMDB text arrives as [`crate::tmdb::TmdbText`], which converts into none
//!   of those. This module does not import [`crate::tmdb`] at all.
//!
//! ```compile_fail,E0308
//! # use flux_meta::ai::LocalEvidence;
//! # fn f(m: &flux_meta::tmdb::MovieResult) -> LocalEvidence {
//! // TMDB text is not a path, and there is no string constructor.
//! LocalEvidence::from_path(&m.overview)
//! # }
//! ```
//!
//! ```compile_fail,E0451
//! # use flux_meta::ai::LocalEvidence;
//! // Fields are private: no struct-literal back door.
//! let ev = LocalEvidence { path: "x".into(), siblings: vec![], probe: None, parse: None, ocr: vec![] };
//! ```
//!
//! What the type system cannot stop is deliberate laundering
//! (`Path::new(tmdb_text.as_str())`); `TmdbText::as_str` is the single,
//! documented, greppable exit for TMDB text.

mod openai;

use std::path::Path;

use flux_core::{NameGuess, ProbeResult};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::art::Frame;
use crate::lang::to_iso639_1;
use crate::matcher::{MatchQuery, MediaKind};

pub use openai::{MaxTokensField, OpenAiCompatible, OpenAiCompatibleConfig};

/// Bump when the prompt or schema changes (part of the job idempotency key).
pub const PROMPT_VERSION: u32 = 1;

/// Errors from AI backends.
#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("HTTP error talking to the model server: {0}")]
    Http(#[from] reqwest::Error),
    #[error("model server returned HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("the model refused: {0}")]
    Refusal(String),
    #[error("the model's output did not match the schema: {0}")]
    Schema(String),
    #[error("OCR failed: {0}")]
    Ocr(String),
    #[error("image encoding failed: {0}")]
    Image(String),
}

/// Probe facts worth showing a model: everything here comes from the file's own headers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProbeFacts {
    pub duration_min: Option<f64>,
    pub container: Option<String>,
    pub video: Option<String>,
    pub audio_languages: Vec<String>,
    pub subtitle_languages: Vec<String>,
    /// Container title tag (`title=`), often the release name.
    pub container_title: Option<String>,
    /// Track names (`"Director's Commentary"`, `"Japanese 5.1"`).
    pub track_titles: Vec<String>,
    pub chapter_titles: Vec<String>,
}

/// Text read from a frame of the user's own video by an [`OcrEngine`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OcrText {
    text: String,
    at_s: f64,
}

impl OcrText {
    /// Run `engine` on a frame decoded from the user's file. This is the only
    /// way to make an `OcrText`.
    pub fn recognize(engine: &dyn OcrEngine, frame: &Frame) -> Result<Vec<OcrText>, AiError> {
        Ok(engine
            .recognize(frame)?
            .into_iter()
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .map(|text| OcrText {
                text,
                at_s: frame.at_s(),
            })
            .collect())
    }

    /// The recognised text.
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// An OCR backend (Apple Vision bridge, `ocrs`, a VLM, …).
pub trait OcrEngine: Send + Sync {
    /// Lines of text visible in the frame.
    fn recognize(&self, frame: &Frame) -> Result<Vec<String>, AiError>;
}

/// Everything a model may see about one file. See the module docs for why
/// this cannot hold TMDB content.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LocalEvidence {
    path: String,
    siblings: Vec<String>,
    probe: Option<ProbeFacts>,
    parse: Option<ParseFacts>,
    ocr: Vec<OcrText>,
}

/// The name parser's reading of the path (flux-names output, itself derived from the path).
#[derive(Debug, Clone, PartialEq, Serialize)]
struct ParseFacts {
    kind: String,
    title: Option<String>,
    year: Option<u16>,
    season: Option<u16>,
    episodes: Vec<u16>,
    absolute_episode: Option<u32>,
    edition: Option<String>,
    confidence: f32,
}

/// Keep at most this many siblings: enough context, bounded prompt size.
const MAX_SIBLINGS: usize = 24;

impl LocalEvidence {
    /// Start from the media file's path (library-relative paths are best: they
    /// keep the user's directory names out of cloud prompts).
    pub fn from_path(path: &Path) -> Self {
        LocalEvidence {
            path: path.to_string_lossy().into_owned(),
            siblings: Vec::new(),
            probe: None,
            parse: None,
            ocr: Vec::new(),
        }
    }

    /// Names of other files in the same folder (file names only, at most 24).
    pub fn with_siblings<'a>(mut self, siblings: impl IntoIterator<Item = &'a Path>) -> Self {
        self.siblings = siblings
            .into_iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .take(MAX_SIBLINGS)
            .collect();
        self
    }

    /// Facts from the file's own headers.
    pub fn with_probe(mut self, p: &ProbeResult) -> Self {
        let langs = |it: &mut dyn Iterator<Item = &flux_core::Track>| -> Vec<String> {
            let mut v: Vec<String> = it
                .filter_map(|t| t.language.as_deref().and_then(to_iso639_1))
                .collect();
            v.dedup();
            v
        };
        let video = p.main_video().map(|v| {
            let hdr = v
                .hdr
                .as_ref()
                .map(|h| format!(" {:?}", h.format))
                .unwrap_or_default();
            format!(
                "{:?} {}x{}{hdr}",
                v.codec,
                v.width.unwrap_or(0),
                v.height.unwrap_or(0)
            )
        });
        self.probe = Some(ProbeFacts {
            duration_min: p
                .duration_ms
                .map(|ms| (ms as f64 / 60_000.0 * 10.0).round() / 10.0),
            container: Some(format!("{:?}", p.container)),
            video,
            audio_languages: langs(&mut p.audio()),
            subtitle_languages: langs(&mut p.subtitles()),
            container_title: p.title.clone(),
            track_titles: p.tracks.iter().filter_map(|t| t.title.clone()).take(12).collect(),
            chapter_titles: p
                .chapters
                .iter()
                .filter_map(|c| c.title.clone())
                .take(12)
                .collect(),
        });
        self
    }

    /// The name parser's guess (derived from the path).
    pub fn with_parse(mut self, g: &NameGuess) -> Self {
        self.parse = Some(ParseFacts {
            kind: format!("{:?}", g.kind).to_lowercase(),
            title: g.title.clone(),
            year: g.year,
            season: g.season,
            episodes: g.episodes.clone(),
            absolute_episode: g.absolute_episode,
            edition: g.edition.clone(),
            confidence: g.confidence,
        });
        self
    }

    /// Text recognised in the file's own frames.
    pub fn with_ocr(mut self, ocr: impl IntoIterator<Item = OcrText>) -> Self {
        self.ocr.extend(ocr);
        self.ocr.truncate(20);
        self
    }

    /// The JSON the model receives.
    pub fn to_prompt_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

/// What kind of thing the model thinks the file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HypothesisKind {
    Movie,
    Episode,
    /// Home video / camera footage: never matched against a provider.
    Personal,
    /// Trailer, featurette, sample, bonus material.
    Extra,
    Unknown,
}

/// One title the model proposes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TitleGuess {
    pub title: String,
    /// ISO 639-1 language of this title, if known.
    pub language: Option<String>,
    /// The original-language (as-released) title.
    pub original: bool,
}

/// A structured hypothesis. Never accepted on its own: it feeds the matcher.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hypothesis {
    pub kind: HypothesisKind,
    /// Most likely first; includes original-language and English titles.
    pub titles: Vec<TitleGuess>,
    pub year: Option<u16>,
    pub season: Option<u16>,
    pub episodes: Vec<u16>,
    pub absolute_episode: Option<u32>,
    pub edition: Option<String>,
    /// The model's self-reported confidence. Poorly calibrated: used only to
    /// order the review queue, never to accept.
    pub confidence: f32,
    pub rationale: String,
}

impl Hypothesis {
    /// Validate and normalise a model's raw output (ranges, lengths, empties).
    pub fn validated(mut self) -> Result<Self, AiError> {
        self.titles.retain(|t| !t.title.trim().is_empty());
        for t in &mut self.titles {
            t.title = t.title.trim().chars().take(200).collect();
            t.language = t.language.as_deref().and_then(to_iso639_1);
        }
        self.titles.truncate(6);
        self.year = self.year.filter(|y| (1870..=2100).contains(y));
        self.season = self.season.filter(|s| *s <= 300);
        self.episodes.retain(|e| (1..=5000).contains(e));
        self.episodes.truncate(10);
        self.absolute_episode = self.absolute_episode.filter(|a| (1..=100_000).contains(a));
        self.edition = self
            .edition
            .map(|e| e.trim().chars().take(80).collect::<String>())
            .filter(|e| !e.is_empty());
        self.confidence = if self.confidence.is_finite() {
            self.confidence.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.rationale = self.rationale.trim().chars().take(400).collect();
        if matches!(self.kind, HypothesisKind::Movie | HypothesisKind::Episode) && self.titles.is_empty() {
            return Err(AiError::Schema(format!(
                "{:?} hypothesis without any title",
                self.kind
            )));
        }
        Ok(self)
    }

    /// Fold the hypothesis into a matcher query: its titles become extra query
    /// titles and its numbers fill gaps the parser left. The matcher still decides.
    pub fn refine(&self, mut q: MatchQuery) -> MatchQuery {
        match self.kind {
            HypothesisKind::Movie => q.kind = q.kind.or(Some(MediaKind::Movie)),
            HypothesisKind::Episode => q.kind = q.kind.or(Some(MediaKind::Tv)),
            _ => {}
        }
        q.year = q.year.or(self.year);
        q.season = q.season.or(self.season);
        if q.episodes.is_empty() {
            q.episodes = self.episodes.clone();
        }
        q.absolute_episode = q.absolute_episode.or(self.absolute_episode);
        q.edition = q.edition.or_else(|| self.edition.clone());
        q.extra_titles(self.titles.iter().map(|t| t.title.clone()))
    }
}

/// The JSON schema the model must follow (OpenAI strict-mode compatible: every
/// property required, nullable via `["type","null"]`, no unsupported keywords;
/// ranges are enforced by [`Hypothesis::validated`] instead).
pub fn hypothesis_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["kind", "titles", "year", "season", "episodes", "absolute_episode", "edition", "confidence", "rationale"],
        "properties": {
            "kind": { "type": "string", "enum": ["movie", "episode", "personal", "extra", "unknown"] },
            "titles": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["title", "language", "original"],
                    "properties": {
                        "title": { "type": "string" },
                        "language": { "type": ["string", "null"] },
                        "original": { "type": "boolean" }
                    }
                }
            },
            "year": { "type": ["integer", "null"] },
            "season": { "type": ["integer", "null"] },
            "episodes": { "type": "array", "items": { "type": "integer" } },
            "absolute_episode": { "type": ["integer", "null"] },
            "edition": { "type": ["string", "null"] },
            "confidence": { "type": "number" },
            "rationale": { "type": "string" }
        }
    })
}

/// The system prompt (versioned by [`PROMPT_VERSION`]).
pub const SYSTEM_PROMPT: &str = "You identify movies and TV episodes from evidence found on a user's own disk: \
the file path, sibling file names, the file's container metadata, and text read from the video's own frames. \
You have no database access and will not be shown any. Reply with JSON only, matching the schema.\n\
Rules:\n\
- titles: the most likely title first. Include the original-language title (original: true, with its ISO 639-1 \
language) and the common English title when you know them. Fix obvious parser mistakes (a year that is part of the \
title such as 'Blade Runner 2049', a dropped 'Part Two').\n\
- kind: movie | episode | personal (home video, phone or camera footage) | extra (trailer, featurette, sample) | unknown.\n\
- year: the release year if evident or confidently known, else null.\n\
- season/episodes for TV; absolute_episode for anime-style absolute numbering.\n\
- edition: e.g. \"Director's Cut\", \"Extended Edition\", or a fan-edit name.\n\
- confidence: your probability that the first title is correct. Never invent ids. If unsure, say so with a low confidence.\n\
- rationale: one short sentence naming the evidence you used.";

/// Token usage reported by the backend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Where a model runs (privacy and cost class).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Locality {
    /// Same machine (Ollama, LM Studio, llama-server).
    Local,
    /// Another device the user owns (e.g. over the tailnet).
    Remote,
    /// A third-party cloud API (BYO key).
    Cloud,
}

/// A model's identity and price, for budgets and job idempotency keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelDescriptor {
    pub provider: String,
    pub model: String,
    pub locality: Locality,
    /// USD per million input / output tokens (0 for local).
    pub usd_per_m_input: f64,
    pub usd_per_m_output: f64,
}

impl ModelDescriptor {
    /// Cost of a call with `usage` tokens.
    pub fn cost(&self, usage: Usage) -> f64 {
        (usage.input_tokens as f64 * self.usd_per_m_input
            + usage.output_tokens as f64 * self.usd_per_m_output)
            / 1e6
    }
}

/// A hypothesis plus accounting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HypothesisOutput {
    pub hypothesis: Hypothesis,
    pub usage: Usage,
    pub model: String,
    pub usd: f64,
}

/// Turns local evidence into a structured hypothesis.
pub trait Hypothesizer: Send + Sync {
    /// Which model this is.
    fn descriptor(&self) -> ModelDescriptor;
    /// Propose what the file is. Only [`LocalEvidence`] is accepted (TMDB ToS §1.C).
    fn hypothesize<'a>(
        &'a self,
        evidence: &'a LocalEvidence,
    ) -> BoxFuture<'a, Result<HypothesisOutput, AiError>>;
}

/// A VLM's description of the user's own frames, used to prompt genAI key art.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SceneDescription {
    pub setting: String,
    pub era: String,
    pub mood: String,
    pub palette: Vec<String>,
    pub genre_guess: String,
    pub key_objects: Vec<String>,
    pub people_count: u32,
    pub time_of_day: String,
}

/// JSON schema for [`SceneDescription`].
pub fn scene_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["setting", "era", "mood", "palette", "genre_guess", "key_objects", "people_count", "time_of_day"],
        "properties": {
            "setting": { "type": "string" },
            "era": { "type": "string" },
            "mood": { "type": "string" },
            "palette": { "type": "array", "items": { "type": "string" } },
            "genre_guess": { "type": "string" },
            "key_objects": { "type": "array", "items": { "type": "string" } },
            "people_count": { "type": "integer" },
            "time_of_day": { "type": "string" }
        }
    })
}

/// Describes frames from the user's own file (local evidence only, like [`Hypothesizer`]).
pub trait SceneDescriber: Send + Sync {
    fn descriptor(&self) -> ModelDescriptor;
    fn describe<'a>(
        &'a self,
        frames: &'a [Frame],
    ) -> BoxFuture<'a, Result<(SceneDescription, Usage), AiError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_serialises_only_local_facts() {
        let ev =
            LocalEvidence::from_path(Path::new("Movies/Parasite.2019.KOREAN.1080p.mkv")).with_siblings([
                Path::new("/x/Parasite.2019.KOREAN.1080p.srt"),
                Path::new("/x/poster.jpg"),
            ]);
        let j = ev.to_prompt_json();
        assert_eq!(j["path"], "Movies/Parasite.2019.KOREAN.1080p.mkv");
        assert_eq!(j["siblings"][0], "Parasite.2019.KOREAN.1080p.srt");
        assert!(j["probe"].is_null());
    }

    #[test]
    fn validation_clamps_and_rejects() {
        let h = Hypothesis {
            kind: HypothesisKind::Movie,
            titles: vec![
                TitleGuess {
                    title: "  Parasite ".into(),
                    language: Some("eng".into()),
                    original: false,
                },
                TitleGuess {
                    title: "기생충".into(),
                    language: Some("ko".into()),
                    original: true,
                },
                TitleGuess {
                    title: " ".into(),
                    language: None,
                    original: false,
                },
            ],
            year: Some(3000),
            season: None,
            episodes: vec![0, 3],
            absolute_episode: None,
            edition: Some("  ".into()),
            confidence: 1.7,
            rationale: "Korean audio".into(),
        }
        .validated()
        .unwrap();
        assert_eq!(h.titles.len(), 2);
        assert_eq!(h.titles[0].title, "Parasite");
        assert_eq!(h.titles[0].language.as_deref(), Some("en"));
        assert_eq!((h.year, h.confidence, h.edition.as_deref()), (None, 1.0, None));
        assert_eq!(h.episodes, vec![3]);

        let empty = Hypothesis {
            titles: vec![],
            ..h.clone()
        }
        .validated();
        assert!(matches!(empty, Err(AiError::Schema(_))));
        let personal = Hypothesis {
            kind: HypothesisKind::Personal,
            titles: vec![],
            ..h
        }
        .validated();
        assert!(personal.is_ok());
    }

    #[test]
    fn refine_adds_titles_without_overriding_the_parser() {
        let h = Hypothesis {
            kind: HypothesisKind::Movie,
            titles: vec![TitleGuess {
                title: "Blade Runner 2049".into(),
                language: Some("en".into()),
                original: true,
            }],
            year: Some(2017),
            season: None,
            episodes: vec![],
            absolute_episode: None,
            edition: None,
            confidence: 0.9,
            rationale: String::new(),
        };
        let q = MatchQuery {
            titles: vec!["Blade Runner".into()],
            year: Some(2017),
            ..Default::default()
        };
        let r = h.refine(q);
        assert_eq!(
            r.titles,
            vec!["Blade Runner".to_owned(), "Blade Runner 2049".to_owned()]
        );
        assert_eq!(r.kind, Some(MediaKind::Movie));
    }

    #[test]
    fn schema_is_strict_mode_shaped() {
        let s = hypothesis_schema();
        let props = s["properties"].as_object().unwrap();
        let required: Vec<&str> = s["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(props.len(), required.len(), "strict mode requires every property");
        assert_eq!(s["additionalProperties"], false);
        // The schema deserialises into the Rust type.
        let sample = serde_json::json!({
            "kind": "movie", "titles": [{"title": "Heat", "language": "en", "original": true}],
            "year": 1995, "season": null, "episodes": [], "absolute_episode": null,
            "edition": "Director's Cut", "confidence": 0.8, "rationale": "folder name"
        });
        let h: Hypothesis = serde_json::from_value(sample).unwrap();
        assert_eq!(h.year, Some(1995));
    }
}
