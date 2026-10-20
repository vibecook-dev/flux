//! In-process remux of MPEG-TS (188-byte packets) and Blu-ray BDAV M2TS
//! (192-byte packets) to fMP4, without ffmpeg.
//!
//! H.264/HEVC video and AAC (ADTS), AC-3 and E-AC-3 audio are copied; MPEG
//! audio is transcoded from frames demuxed here; any other audio (DTS,
//! TrueHD, LPCM, LATM AAC …) is transcoded by ffmpeg reading the file
//! (`ffmpeg -map 0:i:<pid>`). Before Phase D every TS/M2TS file was indexed
//! by an ffprobe packet scan and always video-transcoded.
//!
//! - `packet`: transport packets, sync recovery, continuity counters.
//! - `psi`: PAT/PMT and stream classification (flux-probe's, so track
//!   numbers agree with the catalog).
//! - `pes`: PES headers (PTS/DTS), reassembly, 33-bit unwrapping.
//! - `video`: Annex B NAL units, random-access detection, `avcC`/`hvcC`
//!   from in-band parameter sets, length-prefixed samples.
//! - `audio`: ADTS / AC-3 / MPEG audio framing with per-frame timestamps.
//! - `scan`: the index, one sequential read of the file.
//! - `remux`: video/audio segments and I-frames from the index.
//!
//! The index is built by a full read (TS has no index to read instead): a
//! 2 h file costs about one sequential pass at disk speed, against ffprobe's
//! packet scan of the same file plus a video transcode for every play.

mod audio;
mod packet;
mod pes;
mod psi;
mod remux;
mod scan;
#[cfg(test)]
mod tests;
mod video;

use flux_core::{Codec, Container, ProbeResult, Track};
use serde::{Deserialize, Serialize};

use crate::error::HlsError;
use crate::index::{IndexTrack, PlaybackIndex};
use crate::segment::SegmentPlan;

pub use remux::video_input;
pub(crate) use remux::{audio_fragment, keyframe_segment, source_frames, video_fragment};
pub(crate) use scan::build;

/// Prefix of the reason the TS remux rejected a file. An index built by the
/// ffprobe fallback after such a rejection carries it as a warning, so the
/// planner knows not to plan a copy (`probe_remuxable`).
pub const NO_REMUX: &str = "MPEG-TS remux: ";

pub(crate) fn unsupported(why: impl Into<String>) -> HlsError {
    HlsError::Unsupported(format!("{NO_REMUX}{}", why.into()))
}

/// Where things are in an MPEG-TS/M2TS file (serialized with the index).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TsLayout {
    /// 188 (MPEG-TS) or 192 (M2TS).
    pub packet_size: u16,
    /// Offset of the first packet (its M2TS prefix included).
    pub first_packet: u64,
    /// End of the packet data (the file size when indexed).
    pub data_end: u64,
    /// Timeline origin as a continuous 90 kHz time: ffmpeg's `start_time`,
    /// the earliest first timestamp of the audio and video streams, so the
    /// ffmpeg paths (`-ss`) and the remux share one timeline.
    pub origin: i64,
    /// Timestamp discontinuities, in file order; each later epoch is
    /// shifted to continue where the previous one ended.
    pub discontinuities: Vec<Discontinuity>,
    /// Share of the packet data that is video payload, in millionths
    /// (bandwidth estimates).
    pub video_share_ppm: u32,
    /// Average bytes per second of the file (audio read windows).
    pub byte_rate: u64,
}

/// One timestamp discontinuity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discontinuity {
    /// 90 kHz ticks added to unwrapped timestamps of the epoch it starts.
    pub offset: i64,
    /// Per PID, the file offset of its first PES in that epoch.
    pub starts: Vec<(u16, u64)>,
}

/// The planner's view of a probe: an MPEG-TS/M2TS file whose main video is
/// H.264 or HEVC is remuxed in-process (like Matroska without Cues, the
/// index is built at play time), unless an index built after the remux
/// rejected the file says so in its warnings.
pub fn probe_remuxable(probe: &ProbeResult) -> bool {
    matches!(probe.container, Container::MpegTs | Container::M2ts)
        && probe
            .main_video()
            .is_some_and(|v| matches!(v.codec, Codec::H264 | Codec::Hevc))
        && !probe.warnings.iter().any(|w| w.starts_with(NO_REMUX))
}

/// Audio the TS remux copies into fMP4: AAC in ADTS (not LATM, stream type
/// 0x11), AC-3 and E-AC-3.
pub fn copyable(track: &Track) -> bool {
    match track.codec {
        // A channel configuration in a PCE (no channel count) is transcoded.
        Codec::Aac => track.codec_tag != "0x11" && track.channels.is_some(),
        Codec::Ac3 | Codec::Eac3 => true,
        _ => false,
    }
}

/// True when the transcoder is fed frames demuxed here (AAC, AC-3, E-AC-3,
/// MPEG audio); other codecs are read by ffmpeg from the file.
pub(crate) fn frames_in_process(t: &IndexTrack) -> bool {
    audio::Framing::of(t.codec, &t.codec_tag).is_some()
}

/// Copying `t` is possible (checked by `Asset::audio_copy_entry`).
pub(crate) fn check_copy(t: &IndexTrack) -> crate::Result<()> {
    // LATM, no ADTS header found, or a program config element (channel
    // configuration 0) that ADTS doesn't carry into an AudioSpecificConfig.
    let asc = crate::codec::audio::AacConfig::parse(&t.codec_private);
    if t.codec == Codec::Aac && (t.codec_tag == "0x11" || asc.is_none_or(|c| c.channel_config == 0)) {
        return Err(HlsError::Unsupported(format!(
            "track {}: AAC in LATM, or without a usable ADTS header, is transcoded",
            t.index
        )));
    }
    Ok(())
}

/// Segment byte estimates (bandwidth): the video's share of each range.
pub(crate) fn fill_segment_sizes(plan: &mut SegmentPlan, idx: &PlaybackIndex, l: &TsLayout) {
    let kf = &idx.keyframes;
    for s in &mut plan.segments {
        let from = kf.get(s.kf).map(|k| k.pos).unwrap_or(l.first_packet);
        let to = s
            .next_kf
            .and_then(|k| kf.get(k))
            .map(|k| k.pos)
            .unwrap_or(l.data_end);
        s.approx_bytes = (to.saturating_sub(from) as u128 * l.video_share_ppm as u128 / 1_000_000) as u64;
    }
}
