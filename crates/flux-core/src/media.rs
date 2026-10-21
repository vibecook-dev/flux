//! Media facts produced by `flux-probe` and consumed by the catalog, the
//! playback planner and the remux engine.
//!
//! Numeric colour fields use ITU-T H.273 code points (the values found in
//! H.264/HEVC VUI, MP4 `colr` nclx and Matroska `Colour`), so nothing is lost
//! in translation between containers.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Bump when the probe's output changes in a way that should trigger re-probing.
///
/// v2: [`Track::sample_aspect_ratio`].
pub const PROBE_VERSION: u16 = 2;

/// File extensions (lower case, no dot) of video files a scanner hands to
/// flux-probe and flux-names. This is the one list: `flux_probe::MEDIA_EXTENSIONS`
/// and `flux_names::is_video_ext` are thin wrappers over it.
///
/// It is the union of what the in-process probe reads (Matroska, MP4/MOV,
/// MPEG-TS/M2TS, AVI and their aliases) and what is worth sending to the
/// ffprobe fallback (`mpg`/`vob`/`wmv`/`flv`/`ogv`/`3gp`/`rmvb`: the probe
/// sniffs magic bytes and answers `UnknownFormat` for those).
///
/// Deliberately **not** listed:
/// - `iso` (and other disc images): a filesystem, not a stream. Playing one
///   needs DVD/Blu-ray navigation (titles, VOB/M2TS inside), which flux
///   doesn't do; treating it as a file would only produce probe failures.
/// - `strm`: a text file holding a URL (Kodi/Jellyfin convention), not media.
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mk3d", "webm", "mp4", "m4v", "mov", "qt", "avi", "divx", "ts", "m2ts", "mts", "m2t", "tp", "trp",
    "mpg", "mpeg", "vob", "wmv", "asf", "flv", "ogv", "3gp", "rmvb",
];

/// Extensions of files that travel with a video and inform the scan without
/// being media themselves: subtitles (`srt`, `ass`, `ssa`, `vtt`, VobSub
/// `sub`+`idx`, PGS `sup`), Kodi/Jellyfin `nfo` and local artwork. flux-names
/// uses them as sibling evidence; walks should not drop them.
pub const SIDECAR_EXTENSIONS: &[&str] = &[
    "srt", "ass", "ssa", "vtt", "sub", "idx", "sup", "nfo", "jpg", "jpeg", "png", "webp",
];

/// `true` when `ext` (without the dot, any case) is in [`VIDEO_EXTENSIONS`].
pub fn is_video_extension(ext: &str) -> bool {
    VIDEO_EXTENSIONS.iter().any(|e| e.eq_ignore_ascii_case(ext))
}

/// `true` when `ext` (without the dot, any case) is in [`SIDECAR_EXTENSIONS`].
pub fn is_sidecar_extension(ext: &str) -> bool {
    SIDECAR_EXTENSIONS.iter().any(|e| e.eq_ignore_ascii_case(ext))
}

/// The subtitle format of a sidecar file by its extension (without the dot,
/// any case): `srt`, `ass`, `ssa`, `vtt`, PGS `sup` and VobSub `idx` (whose
/// pictures are in the `.sub` next to it). `None` for anything else, a bare
/// `.sub` included (VobSub data without its index, or MicroDVD text).
pub fn subtitle_file_codec(ext: &str) -> Option<Codec> {
    Some(match ext.to_ascii_lowercase().as_str() {
        "srt" => Codec::Subrip,
        "ass" => Codec::Ass,
        "ssa" => Codec::Ssa,
        "vtt" => Codec::WebVtt,
        "sup" => Codec::Pgs,
        "idx" => Codec::VobSub,
        _ => return None,
    })
}

/// Container formats flux can identify. Always sniffed from magic bytes, never
/// trusted from the file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Container {
    Matroska,
    WebM,
    Mp4,
    Mov,
    MpegTs,
    /// Blu-ray BDAV MPEG-TS (192-byte packets).
    M2ts,
    Avi,
    Unknown,
}

impl Container {
    /// Short name for people: "MKV", "WebM", "MP4", "MOV", "MPEG-TS", "M2TS", "AVI".
    /// Used by [`crate::Reason::explain`]; the web client mirrors it.
    pub fn display_name(self) -> &'static str {
        match self {
            Container::Matroska => "MKV",
            Container::WebM => "WebM",
            Container::Mp4 => "MP4",
            Container::Mov => "MOV",
            Container::MpegTs => "MPEG-TS",
            Container::M2ts => "M2TS",
            Container::Avi => "AVI",
            Container::Unknown => "Unknown",
        }
    }
}

impl fmt::Display for Container {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
    Attachment,
    Data,
}

/// Codecs flux distinguishes. `Other` keeps the raw identifier in
/// [`Track::codec_tag`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    // video
    H264,
    Hevc,
    Av1,
    Vp9,
    Vp8,
    Mpeg2Video,
    Mpeg4Part2,
    Vc1,
    ProRes,
    Mjpeg,
    // audio
    Aac,
    Ac3,
    Eac3,
    TrueHd,
    Dts,
    Flac,
    Opus,
    Vorbis,
    Mp3,
    Mp2,
    Pcm,
    Alac,
    // subtitles
    Subrip,
    Ass,
    Ssa,
    WebVtt,
    MovText,
    Pgs,
    VobSub,
    DvbSub,
    Other,
}

impl Codec {
    pub fn kind(self) -> TrackKind {
        use Codec::*;
        match self {
            H264 | Hevc | Av1 | Vp9 | Vp8 | Mpeg2Video | Mpeg4Part2 | Vc1 | ProRes | Mjpeg => {
                TrackKind::Video
            }
            Aac | Ac3 | Eac3 | TrueHd | Dts | Flac | Opus | Vorbis | Mp3 | Mp2 | Pcm | Alac => {
                TrackKind::Audio
            }
            Subrip | Ass | Ssa | WebVtt | MovText | Pgs | VobSub | DvbSub => TrackKind::Subtitle,
            Other => TrackKind::Data,
        }
    }

    /// Text subtitle formats that can be converted to WebVTT without OCR.
    pub fn is_text_subtitle(self) -> bool {
        matches!(
            self,
            Codec::Subrip | Codec::Ass | Codec::Ssa | Codec::WebVtt | Codec::MovText
        )
    }

    /// Picture subtitle formats: shown by drawing them into the video
    /// (burn-in) unless the client renders them itself.
    pub fn is_image_subtitle(self) -> bool {
        matches!(self, Codec::Pgs | Codec::VobSub | Codec::DvbSub)
    }

    /// Short name for people, as players and spec sheets write it: "H.264",
    /// "HEVC", "TrueHD", "E-AC-3", "PGS"… Used by [`crate::Reason::explain`];
    /// the web client mirrors it (checked by its golden test).
    pub fn display_name(self) -> &'static str {
        use Codec::*;
        match self {
            H264 => "H.264",
            Hevc => "HEVC",
            Av1 => "AV1",
            Vp9 => "VP9",
            Vp8 => "VP8",
            Mpeg2Video => "MPEG-2",
            Mpeg4Part2 => "MPEG-4 Part 2",
            Vc1 => "VC-1",
            ProRes => "ProRes",
            Mjpeg => "MJPEG",
            Aac => "AAC",
            Ac3 => "AC-3",
            Eac3 => "E-AC-3",
            TrueHd => "TrueHD",
            Dts => "DTS",
            Flac => "FLAC",
            Opus => "Opus",
            Vorbis => "Vorbis",
            Mp3 => "MP3",
            Mp2 => "MP2",
            Pcm => "PCM",
            Alac => "ALAC",
            Subrip => "SRT",
            Ass => "ASS",
            Ssa => "SSA",
            WebVtt => "WebVTT",
            MovText => "MP4 text",
            Pgs => "PGS",
            VobSub => "VobSub",
            DvbSub => "DVB",
            Other => "Other",
        }
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// An exact rational number (frame rates, aspect ratios).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rational {
    pub num: u32,
    pub den: u32,
}

impl Rational {
    pub fn as_f64(self) -> f64 {
        if self.den == 0 {
            0.0
        } else {
            self.num as f64 / self.den as f64
        }
    }
}

/// Colour description using ITU-T H.273 code points.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColorInfo {
    /// 1 = BT.709, 9 = BT.2020, …
    pub primaries: Option<u8>,
    /// 1 = BT.709, 16 = SMPTE ST 2084 (PQ), 18 = ARIB STD-B67 (HLG), …
    pub transfer: Option<u8>,
    /// 1 = BT.709, 9 = BT.2020 non-constant, …
    pub matrix: Option<u8>,
    pub full_range: Option<bool>,
}

/// SMPTE ST 2086 mastering display colour volume, in the units used by HEVC
/// SEI and MP4 `mdcv`: chromaticities in 0.00002, luminance in 0.0001 cd/m².
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MasteringDisplay {
    /// [G, B, R] as (x, y), the HEVC SEI order.
    pub primaries: [[u16; 2]; 3],
    pub white_point: [u16; 2],
    pub max_luminance: u32,
    pub min_luminance: u32,
}

/// CTA-861.3 content light level, in cd/m².
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentLightLevel {
    pub max_cll: u16,
    pub max_fall: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HdrFormat {
    Sdr,
    Hdr10,
    Hdr10Plus,
    Hlg,
    DolbyVision,
}

impl HdrFormat {
    /// "SDR", "HDR10", "HDR10+", "HLG", "Dolby Vision".
    pub fn display_name(self) -> &'static str {
        match self {
            HdrFormat::Sdr => "SDR",
            HdrFormat::Hdr10 => "HDR10",
            HdrFormat::Hdr10Plus => "HDR10+",
            HdrFormat::Hlg => "HLG",
            HdrFormat::DolbyVision => "Dolby Vision",
        }
    }
}

impl fmt::Display for HdrFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// Dolby Vision decoder configuration record (`dvcC`/`dvvC`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoviConfig {
    pub profile: u8,
    pub level: u8,
    pub rpu_present: bool,
    pub el_present: bool,
    pub bl_present: bool,
    /// Base-layer compatibility: 0 none, 1 HDR10, 2 SDR, 4 HLG, 6 BD-compatible HDR10.
    pub bl_signal_compat_id: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HdrInfo {
    pub format: HdrFormat,
    pub mastering: Option<MasteringDisplay>,
    pub cll: Option<ContentLightLevel>,
    pub dovi: Option<DoviConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// 0-based order of the track within the file.
    pub index: u32,
    /// Container-native id: Matroska TrackNumber, MP4 track_ID, TS PID, AVI stream number.
    pub id: u64,
    pub kind: TrackKind,
    pub codec: Codec,
    /// Raw codec identifier as found: `V_MPEGH/ISO/HEVC`, `hvc1`, `0x24`, `H264`…
    pub codec_tag: String,
    /// Human profile name, e.g. "Main 10", "High", "LC", "DTS-HD MA".
    pub profile: Option<String>,
    /// Codec level as the raw idc (e.g. 51 for H.264 level 5.1, 153 for HEVC 5.1).
    pub level: Option<u16>,
    pub bit_depth: Option<u8>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Video only: the shape of one pixel (width:height), e.g. 32:27 for
    /// anamorphic NTSC widescreen at 720×480 (display width = 720 × 32/27 ≈ 853).
    /// Reduced, never 0. `None` = not stated (play as square pixels);
    /// `Some(1:1)` = stated square. As ffprobe reports it: the container's
    /// value wins (Matroska DisplayWidth/DisplayHeight, MP4 `pasp`, AVI
    /// `vprp`), else the codec's (H.264/HEVC VUI, MPEG-2 sequence header).
    pub sample_aspect_ratio: Option<Rational>,
    pub frame_rate: Option<Rational>,
    pub interlaced: Option<bool>,
    pub color: Option<ColorInfo>,
    /// Video only. `None` when unknown; `Some(Sdr)` when known to be SDR.
    pub hdr: Option<HdrInfo>,
    pub channels: Option<u8>,
    pub sample_rate: Option<u32>,
    /// Language as found (ISO 639-2 like "eng", or BCP 47 like "en-US").
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    pub forced: bool,
    pub hearing_impaired: bool,
    /// Average bits per second, when the container states it.
    pub bitrate: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chapter {
    pub start_ms: u64,
    pub end_ms: Option<u64>,
    pub title: Option<String>,
}

/// Matroska attachment (fonts for ASS subtitles, cover art).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    pub size: u64,
}

/// Everything the scanner learns about a media file from its headers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeResult {
    pub container: Container,
    pub duration_ms: Option<u64>,
    /// Overall bits per second (file size / duration when not stated).
    pub bitrate: Option<u64>,
    /// Container-level title tag, if any.
    pub title: Option<String>,
    pub tracks: Vec<Track>,
    pub chapters: Vec<Chapter>,
    pub attachments: Vec<Attachment>,
    /// A seek index exists (Matroska Cues, MP4 `stss`), so playback can be
    /// segmented without scanning the file.
    pub has_index: bool,
    /// MP4 only: `moov` precedes `mdat`.
    pub faststart: Option<bool>,
    /// HDR static metadata is not in the container and must be read from the
    /// first keyframe's SEI (a second, targeted read).
    pub needs_frame_sei: bool,
    /// Non-fatal oddities worth surfacing in diagnostics.
    pub warnings: Vec<String>,
    pub probe_version: u16,
    /// I/O accounting: bytes actually read and number of read calls.
    pub bytes_read: u64,
    pub reads: u32,
}

impl ProbeResult {
    pub fn video(&self) -> impl Iterator<Item = &Track> {
        self.tracks.iter().filter(|t| t.kind == TrackKind::Video)
    }
    pub fn audio(&self) -> impl Iterator<Item = &Track> {
        self.tracks.iter().filter(|t| t.kind == TrackKind::Audio)
    }
    pub fn subtitles(&self) -> impl Iterator<Item = &Track> {
        self.tracks.iter().filter(|t| t.kind == TrackKind::Subtitle)
    }
    /// The first video track, which the planner treats as the main picture.
    pub fn main_video(&self) -> Option<&Track> {
        self.video().next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_video_extension_list() {
        for e in [
            "mkv", "MK3D", "m2t", "tp", "trp", "qt", "divx", "wmv", "vob", "rmvb", "3gp",
        ] {
            assert!(is_video_extension(e), "{e}");
        }
        for e in ["iso", "strm", "srt", "nfo", "txt", ""] {
            assert!(!is_video_extension(e), "{e}");
        }
        assert!(is_sidecar_extension("SRT") && is_sidecar_extension("idx"));
        assert_eq!(subtitle_file_codec("SUP"), Some(Codec::Pgs));
        assert_eq!(subtitle_file_codec("ass"), Some(Codec::Ass));
        assert_eq!(subtitle_file_codec("idx"), Some(Codec::VobSub));
        assert_eq!(subtitle_file_codec("sub"), None);
        assert_eq!(subtitle_file_codec("nfo"), None);
        // Every subtitle file the scan catalogs is a sidecar the walk delivers.
        for e in ["srt", "ass", "ssa", "vtt", "sup", "idx"] {
            assert!(is_sidecar_extension(e) && subtitle_file_codec(e).is_some(), "{e}");
        }
        assert!(VIDEO_EXTENSIONS.iter().all(|v| !SIDECAR_EXTENSIONS.contains(v)));
        assert!(VIDEO_EXTENSIONS.iter().all(|v| *v == v.to_ascii_lowercase()));
    }

    #[test]
    fn display_names() {
        assert_eq!(Codec::Eac3.to_string(), "E-AC-3");
        assert_eq!(Codec::H264.display_name(), "H.264");
        assert_eq!(Container::MpegTs.to_string(), "MPEG-TS");
        assert_eq!(HdrFormat::Hdr10Plus.to_string(), "HDR10+");
    }
}
