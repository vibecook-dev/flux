//! Metadata for flux: providers (TMDB), matching, AI hypotheses, and artwork.
//!
//! Everything here is designed to work **offline first** and to degrade
//! gracefully when no API key is configured:
//!
//! | Module | What it does | Needs network / keys? |
//! |---|---|---|
//! | [`tmdb`] | Thin typed TMDB v3 client: rate limiter, 429 backoff, ToS-bounded disk cache, attribution | `TMDB_READ_TOKEN` or `TMDB_API_KEY` |
//! | [`export_index`] | Downloads TMDB's daily ID exports and builds a compact in-memory title index (candidate prefilter) | Download only; no key |
//! | [`matcher`] | Deterministic, explainable scoring of candidates against a [`flux_core::NameGuess`] + [`flux_core::ProbeResult`] | No |
//! | [`ai`] | `Hypothesizer` trait over **local evidence only** + an OpenAI-compatible backend (OpenAI, Ollama, LM Studio, vLLM) | Optional (BYO) |
//! | [`art`] | Frame posters / backdrops / episode thumbnails (ffmpeg + deterministic typography), opt-in genAI posters | ffmpeg; genAI optional |
//! | [`jobs`] | Priority job queue with lanes, concurrency, $/day and tokens/day budgets, idempotency keys | No |
//!
//! Design rules (docs/ARCHITECTURE.md, research/05):
//! - **AI never sees TMDB content** (TMDB API ToS §1.C). The [`ai::LocalEvidence`]
//!   type cannot be built from TMDB types, and TMDB text is wrapped in
//!   [`tmdb::TmdbText`], which does not convert into anything the AI path accepts.
//! - **AI never decides.** Hypotheses go back through the deterministic
//!   [`matcher`]; only its score can auto-accept a match.
//! - **AI never blocks scan or playback.** AI and genAI work runs in the lowest
//!   [`jobs::Lane`], pausable and budgeted.

pub mod ai;
pub mod art;
pub mod eval;
pub mod export_index;
pub mod jobs;
pub mod lang;
pub mod matcher;
pub mod normalize;
pub mod tmdb;
pub(crate) mod util;
