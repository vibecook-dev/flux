//! flux playback engine: an explainable planner plus a stateless,
//! index-driven remux of Matroska/MP4 and MPEG-TS/M2TS into HLS fMP4 (CMAF),
//! with supervised ffmpeg audio and video transcodes for what a client cannot
//! play (the video encoder is detected at run time: [`encoders`]).
//!
//! The big idea (research/03 §4): a playable rendition is a virtual file
//! system computed from an index. Every segment is a pure function of
//! `(asset, recipe, rendition, n)`, so a seek is just a GET — no process to
//! restart, no session state, no "wait for segment n+1".
//!
//! ```text
//! PlaybackIndex (Cues / moov)  ──► SegmentPlan (keyframes ≥ 6 s)
//!        │                                │
//!        ▼                                ▼
//!   init segment (ftyp+moov)     media segment n = read [kf_n, kf_n+1)
//!                                  → moof (DTS from sorted PTS, CTS ≥ 0)
//!                                  + mdat as (file offset, len) ranges
//! ```
//!
//! Entry points:
//! - [`HlsEngine`]: async API for `flux-server` (prepare, playlists, segments).
//! - [`planner::plan`] and [`planner::presets`]: `ProbeResult` + `ClientCaps` → `PlaybackPlan`.
//! - [`PlaybackIndex`]: serializable per-asset index (cache it in the catalog).
//! - [`Asset`]: synchronous core (index + segment plan + source).

pub mod asset;
mod bits;
pub mod body;
mod cache;
pub mod codec;
mod ebml;
pub mod encoders;
pub mod engine;
pub mod error;
pub mod ffmpeg_tools;
pub mod fmp4;
pub mod index;
pub mod planner;
pub mod playlist;
pub mod segment;
pub mod source;
pub mod subtitle;
#[cfg(test)]
mod test_mkv;
pub mod transcode;
pub mod ts;
pub mod vtranscode;

pub use asset::Asset;
pub use body::{Part, SegmentBody};
pub use encoders::{FfmpegCaps, VideoEncoder};
pub use engine::{CacheUsage, EngineConfig, HlsEngine, HlsResponse, WebVttBody};
pub use error::{HlsError, Result};
pub use index::PlaybackIndex;
pub use planner::{plan, plan_from_probe, presets, PlanPrefs};
pub use playlist::{AudioKey, AudioRecipe, HlsResource, Rendition};
pub use segment::SegmentPlan;
