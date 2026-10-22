//! Client capabilities and the explainable playback plan.
//!
//! The planner's decision order (research report 03 §12.2):
//! 1. Direct file, if the container and every selected track are supported.
//! 2. HLS CMAF with video and audio copied (in-process, stateless remux).
//! 3. … plus bitstream fixes (Dolby Vision 7→8.1, in-band parameter sets).
//! 4. … plus audio transcode (stateless, per segment).
//! 5. Video transcode (hardware first, keyframe-aligned segments).
//!
//! Every step that is not "direct" records a [`Reason`], so the UI can say
//! *why* ("Converting TrueHD 7.1 → AAC 5.1 because Chrome cannot decode TrueHD").
//!
//! **HDR.** Decoding HDR is not displaying it (research/03 §8). A client lists
//! in [`VideoCap::hdr`] the formats it *presents correctly*: on an HDR display,
//! or by tone-mapping to its SDR display itself (Apple platforms, Chrome and
//! Safari on macOS). The planner copies HDR video only in those formats and
//! tone-maps on the server otherwise. [`ClientCaps::display_hdr`] says whether
//! the display itself is HDR; the planner does not need it to decide copy vs
//! tone-map (that is already in `VideoCap::hdr`), but the server uses it to
//! pick between versions (an SDR version on an SDR display beats a client
//! tone-map) and for diagnostics.

use serde::{Deserialize, Serialize};

use crate::media::{Codec, Container, HdrFormat};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryFormat {
    /// The original file over HTTP Range.
    ProgressiveFile,
    /// HLS with fragmented MP4 / CMAF segments.
    HlsFmp4,
    /// HLS with MPEG-TS segments (legacy clients).
    HlsTs,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoCap {
    pub codec: Codec,
    /// Highest supported profile names, e.g. ["main", "main10"]; empty = any.
    pub profiles: Vec<String>,
    /// Highest level idc; `None` = any.
    pub max_level: Option<u16>,
    pub max_bit_depth: Option<u8>,
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
    /// HDR formats the client presents correctly as-is: on an HDR display or
    /// by tone-mapping itself. Anything else is tone-mapped by the server.
    pub hdr: Vec<HdrFormat>,
    /// Dolby Vision profiles accepted (5, 8, …).
    pub dovi_profiles: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioCap {
    pub codec: Codec,
    pub max_channels: u8,
}

/// What a client says it can play. Built by the client from runtime probes
/// (`MediaSource.isTypeSupported`, `mediaCapabilities.decodingInfo`,
/// AVFoundation/Media3 queries) and refined by the server's quirks table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientCaps {
    /// Free-form client identity for quirks lookup, e.g. "web/chrome-141/macos".
    pub client: String,
    pub delivery: Vec<DeliveryFormat>,
    /// Containers playable as a progressive file (e.g. Chrome: mp4, webm; Firefox 145+: also mkv).
    pub progressive_containers: Vec<Container>,
    pub video: Vec<VideoCap>,
    pub audio: Vec<AudioCap>,
    /// The display shows HDR right now (`matchMedia('(dynamic-range: high)')`,
    /// AVPlayer `eligibleForHDRPlayback`, Android `Display.isHdr`). Older
    /// clients omit it: `false`. See the module docs for how it differs from
    /// [`VideoCap::hdr`].
    #[serde(default)]
    pub display_hdr: bool,
    /// Subtitle formats the client can render itself.
    pub subtitles: Vec<Codec>,
    /// HLS audio must be a separate rendition (Roku CMAF requirement).
    pub needs_demuxed_audio: bool,
    /// Estimated path throughput in bits/s (LAN, tailnet direct, DERP…).
    pub max_bitrate: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum VideoOp {
    Copy {
        /// Bitstream fixes applied while copying.
        fixes: Vec<BitstreamFix>,
    },
    Transcode {
        codec: Codec,
        width: u32,
        height: u32,
        bitrate: u64,
        /// Tone-map HDR→SDR during the transcode.
        tonemap: bool,
        /// Subtitle track (index) burned into the picture.
        burn_subtitle: Option<u32>,
        hardware: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BitstreamFix {
    /// Dolby Vision profile 7 → 8.1 (drop the enhancement layer, rewrite RPU).
    DoviP7To81,
    /// Strip in-band VPS/SPS/PPS, leaving them to the `hvc1`/`avc1` record.
    ///
    /// Not set by the planner: whether a stream carries in-band parameter sets
    /// is only known from the playback index, so flux-hls decides it per asset
    /// (`EngineConfig::strip_parameter_sets`) and plans list no fix for it.
    /// flux-hls strips them from MPEG-TS remuxes only; Matroska/MP4 keyframes
    /// keep theirs under `avc1`/`hvc1`, since a stream may change them
    /// between keyframes.
    StripInbandParameterSets,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AudioOp {
    Copy,
    Transcode {
        codec: Codec,
        channels: u8,
        bitrate: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SubtitleOp {
    /// Converted to WebVTT and served as a sidecar rendition.
    WebVtt,
    /// Sent as-is for the client to render (ASS via JASSUB, PGS via libbitsub).
    ClientRender,
    /// Burned into the video (forces a video transcode).
    BurnIn,
}

/// Why the plan is not a plain direct play. Machine-readable for quirks and
/// metrics; [`Reason::explain`] gives the human sentence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Reason {
    ContainerUnsupported {
        container: Container,
    },
    VideoCodecUnsupported {
        codec: Codec,
    },
    VideoProfileUnsupported {
        codec: Codec,
        profile: String,
    },
    VideoLevelTooHigh {
        codec: Codec,
        level: u16,
        max: u16,
    },
    VideoBitDepthUnsupported {
        bit_depth: u8,
    },
    VideoResolutionTooHigh {
        width: u32,
        height: u32,
    },
    HdrUnsupported {
        format: HdrFormat,
    },
    DoviProfileUnsupported {
        profile: u8,
    },
    AudioCodecUnsupported {
        codec: Codec,
    },
    AudioChannelsTooMany {
        channels: u8,
        max: u8,
    },
    SubtitleNeedsBurnIn {
        codec: Codec,
    },
    BitrateTooHigh {
        bitrate: u64,
        max: u64,
    },
    NoIndex,
    /// The client could decode `codec`, but flux can't package it for HLS
    /// (fMP4/CMAF): TrueHD, DTS, Vorbis and PCM audio; video other than
    /// H.264/HEVC/AV1. So it is converted.
    CodecNotPackageable {
        codec: Codec,
    },
    ClientQuirk {
        note: String,
    },
}

impl Reason {
    /// The human sentence for this reason, with display names ("HEVC",
    /// "TrueHD", "MKV", "Dolby Vision"; see [`Codec::display_name`]).
    pub fn explain(&self) -> String {
        match self {
            Reason::ContainerUnsupported { container } => {
                format!("This device can't open {container} files directly, so flux repackages the streams (no quality loss)")
            }
            Reason::VideoCodecUnsupported { codec } => format!("This device can't decode {codec} video"),
            Reason::VideoProfileUnsupported { codec, profile } => {
                format!("This device can't decode {codec} {profile} video")
            }
            Reason::VideoLevelTooHigh { codec, level, max } => {
                format!("{codec} level {level} is above this device's limit ({max})")
            }
            Reason::VideoBitDepthUnsupported { bit_depth } => {
                format!("This device can't decode {bit_depth}-bit video")
            }
            Reason::VideoResolutionTooHigh { width, height } => {
                format!("{width}×{height} is above this device's maximum resolution")
            }
            Reason::HdrUnsupported { format } => {
                format!("This display can't show {format}, so flux tone-maps to SDR")
            }
            Reason::DoviProfileUnsupported { profile } => {
                format!("This device can't play Dolby Vision profile {profile}")
            }
            Reason::AudioCodecUnsupported { codec } => format!("This device can't decode {codec} audio"),
            Reason::AudioChannelsTooMany { channels, max } => {
                format!("{channels}-channel audio is more than this device supports ({max})")
            }
            Reason::SubtitleNeedsBurnIn { codec } => {
                format!("{codec} subtitles can't be shown by this device, so they're drawn into the picture")
            }
            Reason::BitrateTooHigh { bitrate, max } => format!(
                "The file's bitrate ({:.0} Mb/s) is above this connection's capacity ({:.0} Mb/s)",
                *bitrate as f64 / 1e6,
                *max as f64 / 1e6
            ),
            Reason::NoIndex => "The file has no seek index, so flux transcodes it".to_string(),
            Reason::CodecNotPackageable { codec } => {
                format!("{codec} can't be carried in an HLS stream, so flux converts it")
            }
            Reason::ClientQuirk { note } => note.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioPlan {
    /// Track index in the source file.
    pub track: u32,
    pub op: AudioOp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubtitlePlan {
    /// Track index in the source file, or a subtitle file's track number
    /// (≥ [`SUBTITLE_FILE_TRACK_BASE`], see [`SubtitleFile`]).
    pub track: u32,
    pub op: SubtitleOp,
    /// The subtitle file `track` names, when it is one (a sidecar beside the
    /// video rather than a track inside it). It is part of the plan, so a
    /// plan (and its id) pins the file's identity. (Plans written before
    /// subtitle files existed read as `None`.)
    #[serde(default)]
    pub file: Option<SubtitleFile>,
}

/// Track numbers from here up name subtitle **files** next to the video
/// ([`SubtitleFile`]), not tracks inside it: no container numbers its tracks
/// this high.
pub const SUBTITLE_FILE_TRACK_BASE: u32 = 1 << 20;

/// A subtitle file beside a video (a sidecar: `Film.en.srt`, `Film.chs.ass`,
/// an episode's `.sup`), offered like one of its tracks under `track`.
/// Language, title and flags are read from the file name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SubtitleFile {
    /// The number to ask for it by (`PlayRequest::subtitle_track`), at least
    /// [`SUBTITLE_FILE_TRACK_BASE`].
    pub track: u32,
    /// Where the file is on the server (like `AssetInfo::path`).
    pub path: String,
    pub codec: Codec,
    /// ISO 639-2 ("eng") or BCP 47 ("zh-Hans") from the name's tags.
    pub language: Option<String>,
    /// What the name says besides language and flags ("Commentary", a
    /// fansub group), if anything.
    pub title: Option<String>,
    pub forced: bool,
    pub hearing_impaired: bool,
    /// The file's size and modification time when cataloged.
    pub size: u64,
    pub mtime_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaybackPlan {
    pub delivery: DeliveryFormat,
    /// Video track index in the source file.
    pub video_track: Option<u32>,
    pub video: VideoOp,
    pub audio: Vec<AudioPlan>,
    pub subtitles: Vec<SubtitlePlan>,
    /// Audio served as separate HLS renditions rather than muxed with video.
    pub demuxed_audio: bool,
    pub reasons: Vec<Reason>,
    /// Rough server cost, for admission control and the diagnostics overlay.
    pub est_cpu_ms_per_min: u32,
}

impl PlaybackPlan {
    /// [`Reason::explain`] of every reason, in order (`PlayResponse::explain`).
    pub fn explain(&self) -> Vec<String> {
        self.reasons.iter().map(Reason::explain).collect()
    }

    pub fn is_direct(&self) -> bool {
        self.delivery == DeliveryFormat::ProgressiveFile
    }
    /// Short label for the UI: "Direct play", "Direct stream", "Audio conversion", "Transcode".
    pub fn label(&self) -> &'static str {
        match (
            &self.delivery,
            &self.video,
            self.audio.iter().any(|a| a.op != AudioOp::Copy),
        ) {
            (DeliveryFormat::ProgressiveFile, _, _) => "Direct play",
            (_, VideoOp::Transcode { .. }, _) => "Transcode",
            (_, VideoOp::Copy { .. }, true) => "Audio conversion",
            (_, VideoOp::Copy { .. }, false) => "Direct stream",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explain_uses_display_names() {
        let r = Reason::ContainerUnsupported {
            container: Container::Matroska,
        };
        assert!(r.explain().starts_with("This device can't open MKV files"));
        assert_eq!(
            Reason::AudioCodecUnsupported { codec: Codec::TrueHd }.explain(),
            "This device can't decode TrueHD audio"
        );
        assert_eq!(
            Reason::HdrUnsupported {
                format: HdrFormat::DolbyVision
            }
            .explain(),
            "This display can't show Dolby Vision, so flux tone-maps to SDR"
        );
        assert_eq!(
            Reason::VideoLevelTooHigh {
                codec: Codec::Hevc,
                level: 153,
                max: 150
            }
            .explain(),
            "HEVC level 153 is above this device's limit (150)"
        );
    }

    #[test]
    fn display_hdr_defaults_to_false() {
        let caps: ClientCaps = serde_json::from_str(
            r#"{"client":"x","delivery":[],"progressive_containers":[],"video":[],"audio":[],
                "subtitles":[],"needs_demuxed_audio":false,"max_bitrate":null}"#,
        )
        .unwrap();
        assert!(!caps.display_hdr);
    }
}
