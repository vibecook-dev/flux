//! Sans-IO media header probe for flux: Matroska/WebM, MP4/MOV, MPEG-TS/M2TS
//! and AVI, driven by range reads.
//!
//! The core is [`Prober`]: a state machine that says which byte range it
//! needs next (`Step::Need { offset, len }`) and is fed bytes until it returns
//! `Step::Done(ProbeResult)`. It never does I/O itself, so it runs unchanged
//! over local `pread` ([`probe_file`]), HTTP range requests, SMB or Truffle.
//!
//! Design rules (see `docs/ARCHITECTURE.md` §flux-probe and
//! `research/02-scan-engine.md` §5):
//! - Sniff magic bytes, never trust extensions ([`sniff`]).
//! - Read headers, not indexes: Matroska Cues/Clusters and MP4 sample tables
//!   are skipped; a typical file costs 1–3 reads.
//! - HDR static metadata comes from the container or codec configuration
//!   record; when missing, an optional second pass ([`FrameSeiProber`],
//!   [`probe_frame_sei`]) parses the first keyframe's SEI (parse, don't decode).
//! - Hostile input returns [`ProbeError`], never panics.
//! - [`ffprobe`] is the fallback for unknown containers and the
//!   differential-testing oracle.

mod avi;
mod bytes;
mod codec;
mod driver;
mod error;
pub mod ffprobe;
mod hdr;
mod mkv;
mod mp4;
mod prober;
mod sniff;
mod store;
mod ts;

pub use driver::{
    probe_cached, probe_file, probe_file_with, probe_frame_sei, probe_source, FileSource, FrameSeiMode,
    PageCache, ProbeOptions, RangeRead, PAGE,
};
pub use error::ProbeError;
pub use prober::{FrameSei, FrameSeiProber, Prober, SeiStep, Step, MAX_BYTES, MAX_READS, MIN_READ, TS_HEAD};
pub use sniff::{sniff, ContainerHint};

/// File extensions the scanner hands to the probe (lower case): the single
/// list in [`flux_core::VIDEO_EXTENSIONS`]. It includes containers only the
/// ffprobe fallback reads (`mpg`, `wmv`, `flv`, …); for those [`probe_file`]
/// answers [`ProbeError::UnknownFormat`] after sniffing.
pub const MEDIA_EXTENSIONS: &[&str] = flux_core::VIDEO_EXTENSIONS;

/// Whether a path has one of [`MEDIA_EXTENSIONS`] (any case).
pub fn is_media_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(flux_core::is_video_extension)
}
