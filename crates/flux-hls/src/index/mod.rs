//! The playback index: everything needed to cut a file into segments without
//! scanning it — keyframe positions, codec configuration, timing facts.
//!
//! Built from container indexes only (Matroska SeekHead → Info/Tracks/Cues,
//! MP4 `moov` sample tables) plus one small read of the first cluster for
//! facts that live in the bitstream (first timestamps, HDR SEI, AC-3 headers).
//! It is `serde`-serializable so `flux-server` can cache it per asset.

mod binary;
pub use binary::INDEX_FORMAT_BINARY;
pub mod external;
pub mod mkv;
pub mod mp4;

use std::path::{Path, PathBuf};

use flux_core::{
    Codec, ColorInfo, Container, HdrFormat, HdrInfo, ProbeResult, Rational, Track, TrackKind, PROBE_VERSION,
};
use serde::{Deserialize, Serialize};

pub use crate::codec::h264::BaselineTools;
use crate::error::{HlsError, Result};
use crate::source::{Source, SourceId};

/// Bump when the index format or its derivation changes.
///
/// Branches that bump it side by side all change it to the same number, and
/// git merges that line without a conflict. Each branch takes a number of its
/// own instead, with its own line below: a build made between the merges
/// caches indexes under the first one's number, which the second change must
/// not accept as current. A number can be skipped (one taken by a branch that
/// lands later); `version_history_names_each_version_once` checks that the
/// list starts at 3, rises line by line and ends at `INDEX_VERSION`.
///
/// 3: H.264 Constrained Baseline is named as such (was "Baseline").
/// 4: [`IndexTrack::extra`] (rotation, pixel aspect, video end), measured
///    frame rates and audio bitrates for Matroska, `MkvLayout::cues_pos`
///    (phase-d/hls-correct).
/// 5: also MPEG-TS/M2TS with H.264/HEVC indexed in-process (`Layout::Ts`)
///    instead of by an ffprobe scan (phase-d/hls-perf). Both branches bumped
///    to 4 independently; 5 makes every cached index rebuild once.
/// 6: MP4 Dolby Vision entries (`dvhe`, `dvh1`, …) index as HEVC/H.264/AV1
///    with their DV record (were "Other"); AC-3/E-AC-3 channels come from
///    `dac3`/`dec3` (were the entry's nominal 2); VP8/VP9/JPEG and unknown
///    video entries keep their size.
/// 7: [`TrackExtra::interlaced`]: the container's interlace flag (Matroska
///    FlagInterlaced, MP4 `fiel`), which decides over the SPS.
/// 8: laced E-AC-3 in Matroska: dec3 and the bitrate describe one access
///    unit, not the whole first block (8× too high with 8-frame lacing).
///    play-audio had bumped to 6 alongside play-dv; 8 makes every index
///    cached by any of these builds rebuild once.
/// 9: an MP4 video without a constant sample duration is rated by the
///    durations most of its frames have (`mp4::dominant_rate`), not their
///    average: frames held for seconds made 23.976 fps 22.75, and a
///    transcode converts such a video at this rate.
/// 10: MP4/MOV `ctts` offsets are signed in version-0 boxes too (were 2^32
///     ticks late when negative), `stts` deltas of nearly 2^32 are clipped,
///     and a `moov` with non-UTF-8 text, padded boxes or junk indexes (was
///     an error).
/// 11: [`TrackExtra::baseline_tools`] (H.264 Baseline FMO/ASO/redundant
///    pictures), and VP9 video: profile, bit depth and level from the first
///    frame, with a `vpcC` record ([`IndexTrack::synth_config`]).
pub const INDEX_VERSION: u16 = 11;

/// Matroska timestamps are rounded (mkvmerge) or truncated (ffmpeg) to the
/// TimestampScale, 1 ms in practice: a frame within this distance of its
/// track's frame grid is on the grid (see [`IndexTrack::snap_ticks`]).
pub const GRID_TOLERANCE_NS: i64 = 1_000_000;

/// A video sync point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keyframe {
    /// Presentation time in nanoseconds (source timeline).
    pub pts_ns: i64,
    /// Matroska: raw block timestamp in TimestampScale units, for exact
    /// matching while parsing clusters. MP4: the sample's composition time in
    /// media ticks.
    pub raw_ts: i64,
    /// Matroska: absolute offset of the Cluster holding the keyframe.
    /// MP4: index of the sync sample in the video track.
    pub pos: u64,
    /// Matroska `CueRelativePosition` (block offset inside the cluster body).
    pub rel: Option<u32>,
}

/// Dolby Vision configuration record carried through to the sample entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoviRecord {
    /// `dvcC`, `dvvC` or `dvwC`.
    pub fourcc: [u8; 4],
    pub data: Vec<u8>,
}

/// Per-track facts the segmenter and the init-segment writer need.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexTrack {
    /// 0-based order in the file (matches `flux_core::Track::index`).
    pub index: u32,
    /// Matroska TrackNumber / MP4 track_ID.
    pub id: u64,
    pub kind: TrackKind,
    pub codec: Codec,
    pub codec_tag: String,
    /// Configuration record as found: Matroska CodecPrivate (avcC/hvcC/av1C
    /// record, AudioSpecificConfig, OpusHead, `fLaC`+metadata) or the MP4
    /// sample entry's equivalent.
    pub codec_private: Vec<u8>,
    /// Sample-entry configuration synthesized from the first frame
    /// (`dac3`/`dec3` body) or copied from an MP4 sample entry.
    pub synth_config: Option<Vec<u8>>,
    /// Output media timescale (exact rational frame rate for video, sample
    /// rate for audio).
    pub timescale: u32,
    /// Ticks per frame at `timescale`, when constant.
    pub frame_duration: Option<u32>,
    pub frame_rate: Option<Rational>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub bit_depth: Option<u8>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u8>,
    pub language: Option<String>,
    pub name: Option<String>,
    pub default: bool,
    pub forced: bool,
    pub hearing_impaired: bool,
    pub color: Option<ColorInfo>,
    pub hdr: Option<HdrInfo>,
    pub dovi: Option<DoviRecord>,
    /// Max frames a picture can be delayed by reordering (SPS
    /// `max_num_reorder_frames` / `sps_max_num_reorder_pics`).
    pub reorder_depth: u32,
    /// PTS of the first frame in the file, ns (origin of the frame grid).
    pub first_pts_ns: Option<i64>,
    /// RFC 6381 codec string for HLS `CODECS`.
    pub codec_string: Option<String>,
    pub profile: Option<String>,
    pub level: Option<u16>,
    /// Keyframes repeat VPS/SPS/PPS in-band.
    pub inband_parameter_sets: bool,
    /// Matroska header-stripping bytes to re-prepend to every frame.
    pub header_strip: Option<Vec<u8>>,
    pub default_duration_ns: Option<u64>,
    /// Matroska CodecDelay (Opus pre-skip), ns.
    pub codec_delay_ns: u64,
    /// Video: keyframes after the first are open-GOP random-access points
    /// (HEVC CRA/BLA, H.264 non-IDR I pictures), not IDR pictures. Sampled
    /// from two keyframes at index time.
    #[serde(default)]
    pub open_gop: bool,
    /// Average bits per second, when known.
    pub bitrate: Option<u64>,
    /// Display and timing facts added after the first index format
    /// (additive: older serialized indexes read as the defaults).
    #[serde(default, skip_serializing_if = "TrackExtra::is_empty")]
    pub extra: TrackExtra,
}

/// Per-track facts beyond the original index (all optional/zero by default).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackExtra {
    /// Video: the MP4/MOV `tkhd` transformation matrix (a, b, u, c, d, v,
    /// x, y, w as stored), when it is not the identity. Copied into the
    /// fMP4 init so players rotate the picture as the source asks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matrix: Option<[i32; 9]>,
    /// Video: clockwise display rotation in degrees (0, 90, 180 or 270)
    /// derived from `matrix`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rotation: u16,
    /// Video: sample (pixel) aspect ratio h:v when it is not square
    /// (Matroska DisplayWidth/DisplayHeight, MP4 `pasp`, else the SPS VUI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sar: Option<(u32, u32)>,
    /// Video: presentation end of the track (last frame's PTS + its
    /// duration), ns on the source timeline. Ends the last segment, so a
    /// subtitle or audio track that outlasts the video doesn't stretch it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ns: Option<i64>,
    /// Video: what the container says about interlacing (Matroska
    /// FlagInterlaced 1/2, MP4 `fiel` with 2/1 fields), when it says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interlaced: Option<bool>,
    /// H.264 Baseline video: the tools outside Constrained Baseline it uses
    /// (`classify_baseline_tools`); none for other video.
    #[serde(default, skip_serializing_if = "is_no_tools")]
    pub baseline_tools: BaselineTools,
}

fn is_no_tools(t: &BaselineTools) -> bool {
    !t.any()
}

fn is_zero(v: &u16) -> bool {
    *v == 0
}

impl TrackExtra {
    /// True when nothing is set (the serialized form omits it).
    pub fn is_empty(&self) -> bool {
        *self == TrackExtra::default()
    }
}

/// Clockwise rotation in degrees of a `tkhd` matrix (a, b, u, c, d, v, x,
/// y, w), when it is a pure 90° multiple: QuickTime's (a b; c d) is
/// (cos θ, sin θ; −sin θ, cos θ) for a picture turned θ clockwise.
pub(crate) fn matrix_rotation(m: &[i32; 9]) -> u16 {
    let (a, b, c, d) = (m[0], m[1], m[3], m[4]);
    match (a.signum(), b.signum(), c.signum(), d.signum()) {
        (0, 1, -1, 0) => 90,
        (-1, 0, 0, -1) => 180,
        (0, -1, 1, 0) => 270,
        _ => 0,
    }
}

impl IndexTrack {
    /// Nanoseconds → ticks of this track's timescale (rounded).
    pub fn ns_to_ticks(&self, ns: i64) -> i64 {
        ns_to_ticks(ns, self.timescale)
    }

    /// Snaps a source timestamp to this track's frame grid and returns ticks.
    ///
    /// Matroska stores millisecond timestamps, so 23.976 fps frames land at
    /// 0, 42, 83, 125… ms. Snapping to the exact grid (origin = first frame,
    /// step = frame duration) restores 0, 1001, 2002… at timescale 24000 and
    /// makes segment boundaries line up exactly with no rounding jitter.
    ///
    /// Only timestamps within [`GRID_TOLERANCE_NS`] of a grid point are
    /// snapped. A variable-frame-rate stretch (59.94 fps credits in a
    /// 23.976 fps encode whose DefaultDuration says 23.976) keeps its real
    /// times: snapping those collided frames on one grid point (PB-1). Every
    /// timescale here is ≥ 1000 ticks/s, so whole-millisecond times stay
    /// distinct and in order.
    pub fn snap_ticks(&self, pts_ns: i64) -> i64 {
        self.grid_point(pts_ns)
            .unwrap_or_else(|| self.ns_to_ticks(pts_ns))
    }

    /// The frame-grid point (ticks) within [`GRID_TOLERANCE_NS`] of
    /// `pts_ns`, if the track has a grid and the time is on it.
    pub fn grid_point(&self, pts_ns: i64) -> Option<i64> {
        let (Some(fd), Some(origin_ns)) = (self.frame_duration, self.first_pts_ns) else {
            return None;
        };
        if fd == 0 {
            return None;
        }
        let t = self.ns_to_ticks(pts_ns);
        let o = self.ns_to_ticks(origin_ns);
        let k = ((t - o) as f64 / fd as f64).round() as i64;
        let g = o + k * fd as i64;
        // Compare in ns against the exact grid time (the tick rounding of
        // `t` alone could add half a tick).
        let off = (ticks_to_ns(g, self.timescale) - pts_ns).abs();
        (off <= GRID_TOLERANCE_NS).then_some(g)
    }

    /// Whether the video is interlaced: the container's flag when it has
    /// one, else the codec's ([`IndexTrack::coded_interlaced`]). This is
    /// flux-probe's rule, so the plan from the catalog's probe and the plan
    /// from this index agree. The container decides because an SPS that
    /// allows field coding doesn't mean the frames use it: x264's
    /// `fake-interlaced` (25p/30p Blu-ray encodes) writes progressive
    /// frames under such an SPS, and ffmpeg flags those files progressive.
    pub fn interlaced(&self) -> Option<bool> {
        self.extra.interlaced.or_else(|| self.coded_interlaced())
    }

    /// Whether the codec allows the video to be coded interlaced, from its
    /// parameter sets: an H.264 SPS without `frame_mbs_only_flag` (field
    /// pictures or MBAFF frames, e.g. a 576i DVD or 1080i broadcast
    /// encode). `None` when the record doesn't tell (other codecs).
    pub fn coded_interlaced(&self) -> Option<bool> {
        use crate::codec::h264;
        match self.codec {
            Codec::H264 => h264::AvcConfig::parse(&self.codec_private)
                .and_then(|c| c.sps.first().and_then(|s| h264::parse_sps(s)))
                .map(|s| !s.frame_mbs_only),
            _ => None,
        }
    }
}

/// How the keyframe list was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexMethod {
    /// Matroska Cues (no cluster scan).
    Cues,
    /// Matroska without Cues: cluster/block headers scanned sequentially.
    ClusterScan,
    /// MP4 `moov` sample tables.
    Moov,
    /// Fragmented MP4: sample tables built from every `moof`/`traf`/`trun`.
    Fragments,
    /// No container index (TS/M2TS/AVI): an ffprobe packet scan.
    FfprobeScan,
    /// MPEG-TS/M2TS: one in-process pass over the packets (`crate::ts`).
    TsScan,
}

/// I/O and time spent building the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct IndexStats {
    pub reads: u64,
    pub bytes_read: u64,
    pub micros: u64,
}

/// Container-specific layout needed to locate samples.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Layout {
    Matroska(mkv::MkvLayout),
    Mp4(mp4::Mp4Layout),
    /// Containers only ffmpeg can read (transcode paths only).
    External(external::ExternalLayout),
    /// MPEG-TS/M2TS remuxed in-process.
    Ts(crate::ts::TsLayout),
}

/// The serializable playback index of one file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaybackIndex {
    pub version: u16,
    pub source: SourceId,
    pub container: Container,
    pub duration_ns: i64,
    pub tracks: Vec<IndexTrack>,
    /// Position in `tracks` of the main video track.
    pub video: Option<usize>,
    /// Video sync points sorted by PTS (unique).
    pub keyframes: Vec<Keyframe>,
    /// Constant added to every track's presentation time so that all decode
    /// times are ≥ 0 and video composition offsets are ≥ 0 (research/03 §4.4).
    pub presentation_offset_ns: i64,
    pub method: IndexMethod,
    pub layout: Layout,
    pub stats: IndexStats,
    pub warnings: Vec<String>,
}

/// Keyframes whose first picture is sampled for [`IndexTrack::open_gop`]:
/// the second and the middle one (the first is an IDR in any stream).
pub(crate) fn open_gop_samples(keyframes: usize) -> Vec<usize> {
    let mut v: Vec<usize> = [1, keyframes / 2]
        .into_iter()
        .filter(|&i| i >= 1 && i < keyframes)
        .collect();
    v.dedup();
    v
}

/// True when any sampled keyframe is an open-GOP random-access point.
pub(crate) fn classify_open_gop(t: &IndexTrack, firsts: &[Vec<u8>]) -> bool {
    use crate::codec::{h264, hevc, NalFlavor};
    let (flavor, ls) = match t.codec {
        Codec::H264 => (
            NalFlavor::H264,
            h264::AvcConfig::parse(&t.codec_private).map(|c| c.length_size),
        ),
        Codec::Hevc => (
            NalFlavor::Hevc,
            hevc::HevcConfig::parse(&t.codec_private).map(|c| c.length_size),
        ),
        _ => return false,
    };
    let ls = ls.unwrap_or(4) as usize;
    firsts
        .iter()
        .any(|f| crate::codec::idr_random_access(f, ls, flavor) == Some(false))
}

/// The Baseline-only tools an H.264 track with `profile_idc` 66 and no
/// `constraint_set1_flag` uses: slice groups and redundant pictures from
/// the PPSs of its record and of the sampled `pictures`, arbitrary slice
/// order from the slices of those pictures (the keyframes sampled for
/// [`classify_open_gop`]: free, they are read anyway). Other tracks,
/// including Constrained Baseline ones, use none.
pub(crate) fn classify_baseline_tools(t: &IndexTrack, pictures: &[Vec<u8>]) -> BaselineTools {
    let mut tools = BaselineTools::default();
    if t.codec != Codec::H264 {
        return tools;
    }
    let Some(c) = crate::codec::h264::AvcConfig::parse(&t.codec_private) else {
        return tools;
    };
    if c.profile_idc != 66 || c.constraint_flags & 0x40 != 0 {
        return tools;
    }
    for pps in &c.pps {
        tools.add_pps(pps);
    }
    for p in pictures {
        tools.add_access_unit(p, c.length_size as usize);
    }
    tools
}

/// Sniffed container family.
fn sniff(head: &[u8]) -> Option<Container> {
    if head.len() >= 4 && head[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        return Some(Container::Matroska);
    }
    if head.len() >= 8
        && matches!(
            &head[4..8],
            b"ftyp" | b"moov" | b"mdat" | b"free" | b"wide" | b"skip"
        )
    {
        return Some(Container::Mp4);
    }
    None
}

impl PlaybackIndex {
    /// Builds the index of the file at `path` (blocking I/O).
    pub fn build(path: impl AsRef<Path>) -> Result<Self> {
        let src = Source::open(path)?;
        Self::build_from(&src)
    }

    /// Like [`PlaybackIndex::build_from`], but falls back to an ffprobe scan
    /// for containers without an index (TS, M2TS, AVI). The scan is bounded
    /// by the default [`external::ScanLimits`].
    pub fn build_with_fallback(src: &Source, ffprobe: Option<&Path>) -> Result<Self> {
        Self::build_with_fallback_limits(src, ffprobe, &external::ScanLimits::default())
    }

    /// [`PlaybackIndex::build_with_fallback`] with explicit limits on the
    /// ffprobe scan (ROB-1).
    pub fn build_with_fallback_limits(
        src: &Source,
        ffprobe: Option<&Path>,
        limits: &external::ScanLimits,
    ) -> Result<Self> {
        match (Self::build_from(src), ffprobe) {
            (Err(HlsError::Unsupported(why)), Some(fp)) => {
                let mut idx = external::build_with_limits(&src.id().path, fp, src.id().clone(), limits)?;
                // The planner reads this: no in-process remux for this file.
                if why.starts_with(crate::ts::NO_REMUX) {
                    idx.warnings.push(why);
                }
                Ok(idx)
            }
            (r, _) => r,
        }
    }

    /// True when segments can be cut in-process (Matroska/MP4/MPEG-TS with keyframes).
    pub fn remuxable(&self) -> bool {
        matches!(self.layout, Layout::Matroska(_) | Layout::Mp4(_) | Layout::Ts(_))
            && !self.keyframes.is_empty()
    }

    /// Builds the index from an open source (blocking I/O).
    pub fn build_from(src: &Source) -> Result<Self> {
        let t0 = std::time::Instant::now();
        let (r0, b0) = src.io_stats();
        let head = src.read_at(0, 12)?;
        let mut idx = match sniff(&head) {
            Some(Container::Matroska) => mkv::build(src)?,
            Some(_) => mp4::build(src)?,
            // MPEG-TS/M2TS with H.264/HEVC; anything else is `Unsupported`.
            None => crate::ts::build(src)?,
        };
        let (r1, b1) = src.io_stats();
        idx.stats = IndexStats {
            reads: r1 - r0,
            bytes_read: b1 - b0,
            micros: t0.elapsed().as_micros() as u64,
        };
        Ok(idx)
    }

    /// The same index for the file's current path: a cached index carries
    /// the path it was built at, which a rename makes stale. Size and mtime
    /// still guard the content.
    pub fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.source.path = path.into();
        self
    }

    /// Serializes to JSON (for the per-asset cache).
    pub fn to_json(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    /// Deserializes from JSON; rejects other index versions.
    pub fn from_json(b: &[u8]) -> Result<Self> {
        let idx: PlaybackIndex = serde_json::from_slice(b)?;
        if idx.version != INDEX_VERSION {
            return Err(HlsError::NotFound(format!(
                "index version {} != {INDEX_VERSION}",
                idx.version
            )));
        }
        Ok(idx)
    }

    /// The main video track.
    pub fn video_track(&self) -> Option<&IndexTrack> {
        self.video.and_then(|i| self.tracks.get(i))
    }

    /// A track by its file order index (`flux_core::Track::index`).
    pub fn track(&self, index: u32) -> Option<&IndexTrack> {
        self.tracks.iter().find(|t| t.index == index)
    }

    /// Duration in seconds.
    pub fn duration_secs(&self) -> f64 {
        self.duration_ns as f64 / 1e9
    }

    /// Presents the index as a [`ProbeResult`], so the planner can run on an
    /// index alone (dev tools, tests). `flux-probe` produces the real one.
    pub fn to_probe_result(&self) -> ProbeResult {
        let tracks = self
            .tracks
            .iter()
            .map(|t| Track {
                index: t.index,
                id: t.id,
                kind: t.kind,
                codec: t.codec,
                codec_tag: t.codec_tag.clone(),
                profile: t.profile.clone(),
                level: t.level,
                bit_depth: t.bit_depth,
                width: t.width,
                height: t.height,
                frame_rate: t.frame_rate,
                interlaced: t.interlaced(),
                color: t.color,
                hdr: t
                    .hdr
                    .clone()
                    .or_else(|| (t.kind == TrackKind::Video).then(sdr_hdr_info)),
                channels: t.channels,
                sample_rate: t.sample_rate,
                language: t.language.clone(),
                title: t.name.clone(),
                default: t.default,
                forced: t.forced,
                hearing_impaired: t.hearing_impaired,
                bitrate: t.bitrate,
                sample_aspect_ratio: t.extra.sar.map(|(num, den)| Rational { num, den }),
            })
            .collect();
        let duration_ms = (self.duration_ns > 0).then_some((self.duration_ns / 1_000_000) as u64);
        ProbeResult {
            container: self.container,
            duration_ms,
            bitrate: duration_ms
                .filter(|&d| d > 0)
                .map(|d| self.source.size * 8 * 1000 / d),
            title: None,
            tracks,
            chapters: vec![],
            attachments: vec![],
            has_index: self.remuxable(),
            faststart: None,
            needs_frame_sei: false,
            warnings: self.warnings.clone(),
            probe_version: PROBE_VERSION,
            bytes_read: self.stats.bytes_read,
            reads: self.stats.reads as u32,
        }
    }
}

fn sdr_hdr_info() -> HdrInfo {
    HdrInfo {
        format: HdrFormat::Sdr,
        mastering: None,
        cll: None,
        dovi: None,
    }
}

/// Nanoseconds → ticks at `timescale`, rounded to nearest.
pub(crate) fn ns_to_ticks(ns: i64, timescale: u32) -> i64 {
    let v = ns as i128 * timescale as i128;
    let half = if v >= 0 { 500_000_000 } else { -500_000_000 };
    ((v + half) / 1_000_000_000) as i64
}

/// Ticks at `timescale` → nanoseconds, rounded to nearest.
pub(crate) fn ticks_to_ns(ticks: i64, timescale: u32) -> i64 {
    if timescale == 0 {
        return 0;
    }
    let v = ticks as i128 * 1_000_000_000;
    let ts = timescale as i128;
    let half = if v >= 0 { ts / 2 } else { -ts / 2 };
    ((v + half) / ts) as i64
}

/// Exact frame rate from a frame duration in ns (Matroska DefaultDuration):
/// 41_708_333 → 24000/1001, 40_000_000 → 25/1. Returns (timescale, ticks per
/// frame) suitable for an MP4 media timescale.
pub(crate) fn rational_rate(frame_ns: u64) -> Option<(Rational, u32, u32)> {
    if frame_ns == 0 {
        return None;
    }
    // Integer rates first, so 40 ms is 25/1 rather than 25025/1001.
    for den in [1u64, 1001] {
        let num = ((1_000_000_000u64 * den) as f64 / frame_ns as f64).round() as u64;
        if num == 0 {
            continue;
        }
        // Matroska stores the duration rounded to the ns (or coarser).
        let exact = 1_000_000_000f64 * den as f64 / num as f64;
        if (exact - frame_ns as f64).abs() <= 1.0 + frame_ns as f64 * 1e-7 {
            let rate = Rational {
                num: num as u32,
                den: den as u32,
            };
            // Timescale ≥ 10_000 keeps sub-ms precision for audio alignment.
            let mult = if den == 1 { 10_000u64.div_ceil(num) } else { 1 };
            let timescale = (num * mult) as u32;
            let ticks = (den * mult) as u32;
            return Some((rate, timescale, ticks));
        }
    }
    None
}

/// ISO 639-2/T three-letter code packed for `mdhd` (ISO/IEC 14496-12 §8.4.2).
pub(crate) fn pack_language(lang: Option<&str>) -> u16 {
    let code = lang.map(iso639_2).unwrap_or("und");
    let b = code.as_bytes();
    if b.len() != 3 || !b.iter().all(|c| c.is_ascii_lowercase()) {
        return 0x55C4; // "und"
    }
    (((b[0] - 0x60) as u16) << 10) | (((b[1] - 0x60) as u16) << 5) | ((b[2] - 0x60) as u16)
}

/// Best-effort ISO 639-2 three-letter code from a Matroska/BCP 47 tag.
pub(crate) fn iso639_2(lang: &str) -> &str {
    let primary = lang.split(['-', '_']).next().unwrap_or(lang);
    match primary {
        "en" => "eng",
        "ja" => "jpn",
        "fr" => "fre",
        "de" => "ger",
        "es" => "spa",
        "it" => "ita",
        "zh" => "chi",
        "ko" => "kor",
        "ru" => "rus",
        "pt" => "por",
        "nl" => "dut",
        "sv" => "swe",
        other => other,
    }
}

/// BCP 47 language tag for HLS `LANGUAGE` (RFC 8216 §4.3.4.1) from a
/// Matroska/ISO 639-2 code.
pub fn bcp47(lang: &str) -> String {
    let l = lang.to_ascii_lowercase();
    let two = match l.as_str() {
        "eng" => "en",
        "jpn" => "ja",
        "fre" | "fra" => "fr",
        "ger" | "deu" => "de",
        "spa" => "es",
        "ita" => "it",
        "chi" | "zho" => "zh",
        "kor" => "ko",
        "rus" => "ru",
        "por" => "pt",
        "dut" | "nld" => "nl",
        "swe" => "sv",
        _ => return lang.to_string(),
    };
    two.to_string()
}

/// Merges colour descriptions field by field: container values win, the
/// bitstream (SPS VUI) fills what the container left unspecified. ffmpeg
/// often writes a Matroska `Colour` with only the matrix set while the HEVC
/// VUI says BT.2020/PQ.
pub(crate) fn merge_color(container: Option<ColorInfo>, vui: Option<ColorInfo>) -> Option<ColorInfo> {
    match (container, vui) {
        (Some(c), Some(v)) => Some(ColorInfo {
            primaries: c.primaries.or(v.primaries),
            transfer: c.transfer.or(v.transfer),
            matrix: c.matrix.or(v.matrix),
            full_range: c.full_range.or(v.full_range),
        }),
        (c, v) => c.or(v),
    }
}

/// Frame rate from an average frame duration: the exact rational when it is
/// a standard rate (24000/1001…), otherwise milli-fps.
pub(crate) fn average_rate(frame_ns: f64) -> Option<Rational> {
    // Accept a 0.05 % deviation: MP4 muxers with coarse timescales alternate
    // deltas (e.g. 656/672 at 16 kHz for 23.976 fps).
    standard_rate(frame_ns, 5e-4)
}

/// [`average_rate`] with a caller-chosen relative tolerance (a rate
/// measured from a few rounded timestamps is less precise).
pub(crate) fn standard_rate(frame_ns: f64, rel_tol: f64) -> Option<Rational> {
    if frame_ns <= 0.0 || !frame_ns.is_finite() {
        return None;
    }
    let mut best: Option<(f64, Rational)> = None;
    for (num, den) in [
        (24000u32, 1001u32),
        (24, 1),
        (25, 1),
        (30000, 1001),
        (30, 1),
        (48, 1),
        (50, 1),
        (60000, 1001),
        (60, 1),
    ] {
        let exact = 1e9 * den as f64 / num as f64;
        let err = (exact - frame_ns).abs() / exact;
        if err <= rel_tol && best.is_none_or(|b| err < b.0) {
            best = Some((err, Rational { num, den }));
        }
    }
    if let Some((_, r)) = best {
        return Some(r);
    }
    Some(Rational {
        num: (1e12 / frame_ns).round() as u32,
        den: 1000,
    })
}

/// HDR format from colour + metadata facts.
pub(crate) fn classify_hdr(color: Option<&ColorInfo>, dovi: bool) -> HdrFormat {
    if dovi {
        return HdrFormat::DolbyVision;
    }
    match color.and_then(|c| c.transfer) {
        Some(16) => HdrFormat::Hdr10,
        Some(18) => HdrFormat::Hlg,
        _ => HdrFormat::Sdr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// When two branches each bump `INDEX_VERSION` from the same number, git
    /// merges the constant cleanly and only the history lines conflict.
    /// Keeping both lines leaves two changes under one number, and indexes
    /// that a build with only the first change cached pass as current for
    /// the second. The history must start at 3, rise without repeats (a
    /// number taken by a branch that lands later may be missing) and end at
    /// the constant.
    #[test]
    fn version_history_names_each_version_once() {
        let src = include_str!("mod.rs");
        let start = src.find("/// Bump when the index format").expect("the history");
        let end = start
            + src[start..]
                .find("pub const INDEX_VERSION")
                .expect("the constant");
        let listed: Vec<u16> = src[start..end]
            .lines()
            .filter_map(|l| {
                let (n, _) = l.strip_prefix("/// ")?.split_once(": ")?;
                n.parse().ok()
            })
            .collect();
        assert_eq!(listed.first(), Some(&3), "the history starts at 3: {listed:?}");
        assert!(
            listed.windows(2).all(|w| w[0] < w[1]),
            "each INDEX_VERSION needs exactly one history line, in rising order; a branch \
             merged after another that bumped to the same number takes a number of its own: \
             {listed:?}"
        );
        assert_eq!(
            listed.last(),
            Some(&INDEX_VERSION),
            "the history ends at INDEX_VERSION: {listed:?}"
        );
    }

    #[test]
    fn rates() {
        let (r, ts, fd) = rational_rate(41_708_333).unwrap();
        assert_eq!((r.num, r.den, ts, fd), (24000, 1001, 24000, 1001));
        let (r, ts, fd) = rational_rate(40_000_000).unwrap();
        assert_eq!((r.num, r.den, ts, fd), (25, 1, 10000, 400));
        let (r, ..) = rational_rate(33_366_667).unwrap();
        assert_eq!((r.num, r.den), (30000, 1001));
        let (r, ..) = rational_rate(16_683_333).unwrap();
        assert_eq!((r.num, r.den), (60000, 1001));
        assert!(rational_rate(41_000_000).is_none());
    }

    #[test]
    fn tick_conversion() {
        assert_eq!(ns_to_ticks(41_708_333, 24000), 1001);
        assert_eq!(ns_to_ticks(-21_333_333, 48000), -1024);
        assert_eq!(ticks_to_ns(1001, 24000), 41_708_333);
    }

    /// A 23.976 fps track (DefaultDuration 41.708 ms) with a 59.94 fps
    /// stretch: on-grid ms timestamps snap exactly; the fast frames keep
    /// their own (distinct, ordered) times instead of colliding (PB-1).
    #[test]
    fn snapping_leaves_off_grid_frames_alone() {
        let (rate, ts, fd) = rational_rate(41_708_333).unwrap();
        let t = IndexTrack {
            frame_duration: Some(fd),
            frame_rate: Some(rate),
            timescale: ts,
            first_pts_ns: Some(0),
            ..test_track()
        };
        // ms-rounded grid times snap to k × 1001.
        for (k, ms) in [(0i64, 0i64), (1, 42), (2, 83), (3, 125), (144, 6006)] {
            assert_eq!(t.snap_ticks(ms * 1_000_000), k * 1001, "{ms} ms");
        }
        // ffmpeg truncates: 41 ms for frame 1 is still on the grid.
        assert_eq!(t.snap_ticks(41_000_000), 1001);
        // 59.94 fps frames after 6.006 s: 6.023, 6.039, 6.056, 6.073 s.
        let fast: Vec<i64> = [6023i64, 6039, 6056, 6073, 6089, 6106]
            .iter()
            .map(|ms| t.snap_ticks(ms * 1_000_000))
            .collect();
        let mut sorted = fast.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), fast.len(), "collisions: {fast:?}");
        assert!(fast.windows(2).all(|w| w[1] > w[0]), "{fast:?}");
        // 6.089 s lies 0.4 ms from grid point 146 (6.0894 s): snapped.
        assert_eq!(fast[4], 146 * 1001);
        // 6.039 s is 9 ms off the grid: kept (24 ticks per ms).
        assert_eq!(fast[1], 6039 * 24);
        // No grid (no DefaultDuration): plain conversion.
        let vfr = IndexTrack {
            frame_duration: None,
            ..t.clone()
        };
        assert_eq!(vfr.snap_ticks(6_039_000_000), 6039 * 24);
    }

    #[test]
    fn tkhd_rotation() {
        let one = 0x1_0000;
        let ident = [one, 0, 0, 0, one, 0, 0, 0, 0x4000_0000];
        assert_eq!(matrix_rotation(&ident), 0);
        // iPhone portrait: 90° clockwise (ffprobe "rotation: -90").
        assert_eq!(
            matrix_rotation(&[0, one, 0, -one, 0, 0, 1080 << 16, 0, 0x4000_0000]),
            90
        );
        assert_eq!(matrix_rotation(&[-one, 0, 0, 0, -one, 0, 0, 0, 0x4000_0000]), 180);
        assert_eq!(matrix_rotation(&[0, -one, 0, one, 0, 0, 0, 0, 0x4000_0000]), 270);
    }

    pub(crate) fn test_track() -> IndexTrack {
        IndexTrack {
            index: 0,
            id: 1,
            kind: TrackKind::Video,
            codec: Codec::H264,
            codec_tag: String::new(),
            codec_private: Vec::new(),
            synth_config: None,
            timescale: 1000,
            frame_duration: None,
            frame_rate: None,
            width: Some(1920),
            height: Some(1080),
            bit_depth: Some(8),
            sample_rate: None,
            channels: None,
            language: None,
            name: None,
            default: true,
            forced: false,
            hearing_impaired: false,
            color: None,
            hdr: None,
            dovi: None,
            reorder_depth: 0,
            first_pts_ns: Some(0),
            codec_string: None,
            profile: None,
            level: None,
            inband_parameter_sets: false,
            header_strip: None,
            default_duration_ns: None,
            codec_delay_ns: 0,
            open_gop: false,
            bitrate: None,
            extra: TrackExtra::default(),
        }
    }

    /// H.264 Baseline: slice groups from the record's PPS or one a keyframe
    /// repeats, arbitrary slice order from the sampled keyframes; the
    /// engine's plan transcodes those streams for a client that lists only
    /// Main and High, and plays the plain one as it is.
    #[tokio::test]
    async fn baseline_tools_from_the_record_and_the_sampled_keyframes() {
        use crate::codec::h264::tests::{baseline_pps, baseline_sps, slice};
        use crate::test_mkv::TestMkv;
        use flux_core::{Reason, VideoOp};
        let (plain, cb) = (baseline_sps(0x80), baseline_sps(0xC0));
        let (pps, fmo) = (baseline_pps(1, false), baseline_pps(2, false));
        let redundant = baseline_pps(1, true);
        let (plain, cb, pps, fmo, redundant) = (&plain[..], &cb[..], &pps[..], &fmo[..], &redundant[..]);
        let out_of_order = vec![slice(5, 4), slice(5, 0)];
        struct Case<'a> {
            name: &'a str,
            sps: &'a [u8],
            pps: &'a [u8],
            inband: Vec<Vec<&'a [u8]>>,
            idr: Vec<Vec<u8>>,
            want: BaselineTools,
        }
        fn case<'a>(
            name: &'a str,
            sps: &'a [u8],
            pps: &'a [u8],
            idr: Vec<Vec<u8>>,
            want: BaselineTools,
        ) -> Case<'a> {
            Case {
                name,
                sps,
                pps,
                inband: vec![vec![]; 3],
                idr,
                want,
            }
        }
        let none = BaselineTools::default();
        let fmo_used = BaselineTools {
            slice_groups: true,
            ..none
        };
        let cases = [
            case("plain", plain, pps, vec![], none),
            case("fmo", plain, fmo, vec![], fmo_used),
            Case {
                inband: vec![vec![plain, fmo]; 3],
                ..case("inband-fmo", plain, pps, vec![], fmo_used)
            },
            case(
                "aso",
                plain,
                pps,
                out_of_order.clone(),
                BaselineTools {
                    arbitrary_slice_order: true,
                    ..none
                },
            ),
            case(
                "redundant",
                plain,
                redundant,
                vec![],
                BaselineTools {
                    redundant_pictures: true,
                    ..none
                },
            ),
            // Constrained Baseline says it has none of them.
            case("constrained", cb, fmo, out_of_order, none),
        ];
        let mut caps = crate::presets::chrome_desktop();
        caps.video[0].profiles = vec!["high".into(), "main".into()];
        caps.progressive_containers.push(Container::Matroska);
        let engine = crate::HlsEngine::new(crate::EngineConfig::default());
        for Case {
            name,
            sps,
            pps,
            inband,
            idr,
            want,
        } in cases
        {
            let (path, _) = TestMkv {
                sps,
                pps,
                inband,
                flag_interlaced: None,
                idr,
            }
            .write(&format!("baseline-{name}.mkv"));
            let idx = PlaybackIndex::build(&path).unwrap();
            let v = idx.video_track().unwrap();
            assert_eq!(v.extra.baseline_tools, want, "{name}");
            let profile = if name == "constrained" {
                "Constrained Baseline"
            } else {
                "Baseline"
            };
            assert_eq!(v.profile.as_deref(), Some(profile), "{name}");
            // Serialized only when used (older indexes read as none).
            let json = String::from_utf8(idx.to_json().unwrap()).unwrap();
            assert_eq!(json.contains("baseline_tools"), want.any(), "{name}");
            let asset = crate::Asset::with_index(idx, 6_000_000_000).unwrap();
            let plan = engine.plan(&asset, &caps, &crate::PlanPrefs::default());
            if want.any() {
                assert!(
                    matches!(plan.video, VideoOp::Transcode { .. }),
                    "{name}: {:?}",
                    plan.video
                );
                assert!(
                    plan.reasons.iter().any(|r| matches!(r,
                        Reason::VideoProfileUnsupported { profile, .. } if profile.starts_with("Baseline ("))),
                    "{name}: {:?}",
                    plan.reasons
                );
            } else {
                assert!(plan.is_direct(), "{name}: {:?}", plan.reasons);
            }
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn language_packing() {
        assert_eq!(pack_language(Some("eng")), 0x15C7);
        assert_eq!(pack_language(Some("en")), 0x15C7);
        assert_eq!(pack_language(None), 0x55C4);
        assert_eq!(bcp47("jpn"), "ja");
    }
}
