//! Matroska/WebM playback index and block reader.
//!
//! Index build (research/03 §4.2): EBML header → Segment → level-1 walk up to
//! the first Cluster (SeekHead, Info, Tracks are normally there) → Cues via
//! the SeekHead. One extra read of the first cluster(s) yields facts that only
//! exist in the bitstream: first timestamps, HDR SEI, AC-3 headers.
//!
//! Without Cues the keyframe list is rebuilt by walking every cluster with
//! large sequential reads (`IndexMethod::ClusterScan`, ≈ file size / disk
//! throughput).

use flux_core::{
    Codec, ColorInfo, Container, DoviConfig, HdrFormat, HdrInfo, MasteringDisplay, Rational, TrackKind,
};
use serde::{Deserialize, Serialize};

use super::{
    classify_hdr, rational_rate, DoviRecord, IndexMethod, IndexStats, IndexTrack, Keyframe, Layout,
    PlaybackIndex, INDEX_VERSION,
};
use crate::codec::{self, audio, h264, hevc, NalFlavor};
use crate::ebml::{self, children};
use crate::error::{HlsError, Result};
use crate::source::{Source, Windows};

/// Where things are inside the Matroska Segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MkvLayout {
    /// Nanoseconds per raw timestamp unit (Info/TimestampScale).
    pub timestamp_scale: u64,
    /// Absolute offset of the Segment's data (SeekPosition/CueClusterPosition origin).
    pub segment_data_start: u64,
    /// Absolute offset of the first Cluster.
    pub first_cluster: u64,
    /// End of cluster data (Cues position when Cues follow the clusters, else segment end).
    pub data_end: u64,
    /// Absolute offset of the Cues element, when there is one (subtitle
    /// tracks are read through their cue points).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cues_pos: Option<u64>,
}

impl MkvLayout {
    /// Raw block timestamp → nanoseconds.
    pub fn ts_ns(&self, raw: i64) -> i64 {
        raw.saturating_mul(self.timestamp_scale as i64)
    }
}

// ------------------------------------------------------------------ block walker

/// One (possibly laced) Matroska block.
#[derive(Debug, Clone)]
pub(crate) struct Block {
    pub track: u64,
    /// Raw timestamp (cluster timestamp + relative), TimestampScale units.
    pub ts: i64,
    pub key: bool,
    /// BlockGroup/BlockDuration, raw units.
    pub duration: Option<u64>,
    /// Absolute offset of the Cluster element holding this block.
    pub cluster_pos: u64,
    /// Frames: (absolute payload offset, length). More than one when laced.
    pub frames: Vec<(u64, u32)>,
}

/// Walks clusters and blocks sequentially from a position, reading through a
/// [`Windows`] so headers cost no extra syscalls.
pub(crate) struct BlockWalker<'s, 'w> {
    win: &'w mut Windows<'s>,
    pos: u64,
    end: u64,
    cluster_ts: i64,
    cluster_pos: u64,
    /// `Some(end)` inside a known-size cluster, `Some(u64::MAX)` inside an
    /// unknown-size cluster, `None` at Segment level.
    cluster_end: Option<u64>,
    /// Only return blocks of this track; others are skipped after reading
    /// their track number (a few bytes), never their lace headers.
    only: Option<u64>,
}

const MAX_LACE_HEADER: usize = 8192;

impl<'s, 'w> BlockWalker<'s, 'w> {
    pub(crate) fn new(win: &'w mut Windows<'s>, pos: u64, end: u64) -> Self {
        BlockWalker {
            win,
            pos,
            end,
            cluster_ts: 0,
            cluster_pos: pos,
            cluster_end: None,
            only: None,
        }
    }

    /// Returns only blocks of `track` (see [`BlockWalker::only`]).
    pub(crate) fn only_track(mut self, track: u64) -> Self {
        self.only = Some(track);
        self
    }

    /// Current read position.
    pub(crate) fn pos(&self) -> u64 {
        self.pos
    }

    /// Enters the Cluster element at the current position: reads its header
    /// and its Timestamp (the first child). Returns the cluster body's
    /// offset, which Matroska `CueRelativePosition`s are relative to, or
    /// `None` when there is no Cluster here.
    pub(crate) fn enter_cluster(&mut self) -> Result<Option<u64>> {
        let Some((id, hl, size)) = self.header(self.pos)? else {
            return Ok(None);
        };
        if id != ebml::CLUSTER {
            return Ok(None);
        }
        let body = self.pos + hl as u64;
        self.cluster_pos = self.pos;
        self.cluster_ts = 0;
        self.cluster_end = Some(size.map(|s| body + s).unwrap_or(u64::MAX));
        self.pos = body;
        // The Timestamp comes first, after an optional CRC-32 (ffmpeg writes
        // one) or Void.
        let mut p = body;
        for _ in 0..4 {
            let Some((cid, chl, Some(csz))) = self.header(p)? else {
                break;
            };
            match cid {
                ebml::CLUSTER_TIMESTAMP => {
                    let b = self.win.get(p + chl as u64, csz as usize)?;
                    self.cluster_ts = ebml::uint(b) as i64;
                    break;
                }
                ebml::CRC32 | ebml::VOID => p += chl as u64 + csz,
                _ => break,
            }
        }
        Ok(Some(body))
    }

    /// Moves to `pos` inside the current cluster (after [`Self::enter_cluster`]).
    pub(crate) fn seek_in_cluster(&mut self, pos: u64) {
        self.pos = pos;
    }

    /// True while the walker is inside the cluster at `cluster`.
    pub(crate) fn in_cluster(&self, cluster: u64) -> bool {
        self.cluster_end.is_some() && self.cluster_pos == cluster
    }

    /// Changes the size of subsequent window reads.
    pub(crate) fn set_chunk(&mut self, chunk: usize) {
        self.win.set_chunk(chunk);
    }

    /// Reads a frame payload through the window.
    pub(crate) fn payload(&mut self, offset: u64, len: u32) -> Result<Vec<u8>> {
        Ok(self.win.get(offset, len as usize)?.to_vec())
    }

    fn header(&mut self, pos: u64) -> Result<Option<(u32, usize, Option<u64>)>> {
        let h = self.win.get(pos, 12)?;
        if h.is_empty() {
            return Ok(None);
        }
        let mut p = 0;
        let Some(id) = ebml::read_id(h, &mut p) else {
            return Err(HlsError::mkv(format!("bad element ID at {pos}")));
        };
        let Some(size) = ebml::read_size(h, &mut p) else {
            return Err(HlsError::mkv(format!("bad element size at {pos}")));
        };
        Ok(Some((id, p, size)))
    }

    /// Returns the next block, or `None` at the end of the range / file.
    pub(crate) fn next_block(&mut self) -> Result<Option<Block>> {
        loop {
            if self.pos >= self.end {
                return Ok(None);
            }
            if let Some(ce) = self.cluster_end {
                if self.pos >= ce {
                    self.cluster_end = None;
                }
            }
            let Some((id, hl, size)) = self.header(self.pos)? else {
                return Ok(None);
            };
            let body = self.pos + hl as u64;

            if self.cluster_end.is_none() || (self.cluster_end == Some(u64::MAX) && ebml::is_top_level(id)) {
                // Segment level.
                self.cluster_end = None;
                if id == ebml::CLUSTER {
                    self.cluster_pos = self.pos;
                    self.cluster_ts = 0;
                    self.cluster_end = Some(size.map(|s| body + s).unwrap_or(u64::MAX));
                    self.pos = body;
                    continue;
                }
                match size {
                    Some(s) => self.pos = body + s,
                    None => return Ok(None), // unknown-size non-cluster: give up
                }
                continue;
            }

            // Inside a cluster.
            let Some(size) = size else {
                return Err(HlsError::mkv(format!(
                    "unknown-size child {id:#x} at {}",
                    self.pos
                )));
            };
            let next = body + size;
            match id {
                ebml::CLUSTER_TIMESTAMP => {
                    let b = self.win.get(body, size as usize)?;
                    self.cluster_ts = ebml::uint(b) as i64;
                }
                ebml::SIMPLE_BLOCK => {
                    let block = self.parse_block(body, size, None, None)?;
                    self.pos = next;
                    if let Some(b) = block {
                        return Ok(Some(b));
                    }
                    continue;
                }
                ebml::BLOCK_GROUP => {
                    let mut p = body;
                    let mut block_at: Option<(u64, u64)> = None;
                    let mut has_ref = false;
                    let mut duration = None;
                    while p < next {
                        let Some((cid, chl, Some(csz))) = self.header(p)? else {
                            break;
                        };
                        let cbody = p + chl as u64;
                        match cid {
                            ebml::BLOCK => block_at = Some((cbody, csz)),
                            ebml::REFERENCE_BLOCK => has_ref = true,
                            ebml::BLOCK_DURATION => {
                                duration = Some(ebml::uint(self.win.get(cbody, csz as usize)?))
                            }
                            _ => {}
                        }
                        p = cbody + csz;
                    }
                    self.pos = next;
                    if let Some((b, s)) = block_at {
                        if let Some(block) = self.parse_block(b, s, Some(!has_ref), duration)? {
                            return Ok(Some(block));
                        }
                    }
                    continue;
                }
                _ => {}
            }
            self.pos = next;
        }
    }

    fn parse_block(
        &mut self,
        body: u64,
        size: u64,
        key: Option<bool>,
        duration: Option<u64>,
    ) -> Result<Option<Block>> {
        let cluster_ts = self.cluster_ts;
        let cluster_pos = self.cluster_pos;
        if let Some(want) = self.only {
            // The track number (a vint ≤ 8 bytes) is all a skip needs.
            let head = self.win.get(body, (size as usize).min(8))?;
            let mut p = 0usize;
            if ebml::read_size(head, &mut p).flatten() != Some(want) {
                return Ok(None);
            }
        }
        let hdr = self.win.get(body, (size as usize).min(MAX_LACE_HEADER))?;
        let mut p = 0usize;
        let Some(Some(track)) = ebml::read_size(hdr, &mut p) else {
            return Ok(None);
        };
        if p + 3 > hdr.len() {
            return Ok(None);
        }
        let rel = i16::from_be_bytes([hdr[p], hdr[p + 1]]) as i64;
        let flags = hdr[p + 2];
        p += 3;
        let key = key.unwrap_or(flags & 0x80 != 0);
        let lacing = (flags >> 1) & 0x3;
        let ts = cluster_ts + rel;
        let mut frames = Vec::with_capacity(1);
        if lacing == 0 {
            frames.push((body + p as u64, (size as usize - p) as u32));
        } else {
            let Some(&count) = hdr.get(p) else { return Ok(None) };
            let n = count as usize + 1;
            p += 1;
            let mut sizes = Vec::with_capacity(n);
            match lacing {
                1 => {
                    // Xiph lacing: each size as a run of 255s plus a final byte.
                    for _ in 0..n - 1 {
                        let mut s = 0usize;
                        loop {
                            let Some(&b) = hdr.get(p) else {
                                return Err(HlsError::mkv("truncated Xiph lace"));
                            };
                            p += 1;
                            s += b as usize;
                            if b != 255 {
                                break;
                            }
                        }
                        sizes.push(s);
                    }
                }
                3 => {
                    // EBML lacing: first size as a vint, then signed deltas.
                    let Some(Some(first)) = ebml::read_size(hdr, &mut p) else {
                        return Err(HlsError::mkv("bad EBML lace"));
                    };
                    let mut prev = first as i64;
                    sizes.push(first as usize);
                    for _ in 1..n - 1 {
                        let Some(delta) = ebml::read_signed_vint(hdr, &mut p) else {
                            return Err(HlsError::mkv("bad EBML lace delta"));
                        };
                        prev += delta;
                        sizes.push(prev.max(0) as usize);
                    }
                }
                _ => {}
            }
            let total = (size as usize).saturating_sub(p);
            if lacing == 2 {
                sizes = vec![total / n; n - 1];
            }
            let used: usize = sizes.iter().sum();
            if used > total {
                return Err(HlsError::mkv("lace sizes exceed block"));
            }
            sizes.push(total - used);
            let mut off = body + p as u64;
            for s in sizes {
                frames.push((off, s as u32));
                off += s as u64;
            }
        }
        Ok(Some(Block {
            track,
            ts,
            key,
            duration,
            cluster_pos,
            frames,
        }))
    }
}

// ------------------------------------------------------------------ index build

#[derive(Default)]
struct Entry {
    number: u64,
    kind: u64,
    codec_id: String,
    codec_private: Vec<u8>,
    default_duration: Option<u64>,
    name: Option<String>,
    language: Option<String>,
    language_bcp47: Option<String>,
    default: bool,
    forced: bool,
    hearing_impaired: bool,
    enabled: bool,
    codec_delay: u64,
    width: Option<u32>,
    height: Option<u32>,
    /// DisplayWidth, DisplayHeight, DisplayUnit (the display aspect when
    /// they differ from the pixel size: anamorphic video; unit 4 = unknown).
    display: (Option<u64>, Option<u64>, u64),
    /// FlagInterlaced: 1 interlaced, 2 progressive, else unknown.
    interlaced: Option<bool>,
    sampling: Option<f64>,
    channels: Option<u8>,
    bit_depth: Option<u8>,
    color: ColorInfo,
    has_color: bool,
    mastering: Option<MasteringDisplay>,
    cll: (Option<u16>, Option<u16>),
    dovi: Option<DoviRecord>,
    header_strip: Option<Vec<u8>>,
    compressed: bool,
}

fn parse_track_entry(b: &[u8]) -> Entry {
    let mut e = Entry {
        default: true,
        enabled: true,
        language: Some("eng".into()),
        ..Default::default()
    };
    for c in children(b) {
        match c.id {
            ebml::TRACK_NUMBER => e.number = ebml::uint(c.body),
            ebml::TRACK_TYPE => e.kind = ebml::uint(c.body),
            ebml::CODEC_ID => e.codec_id = ebml::string(c.body),
            ebml::CODEC_PRIVATE => e.codec_private = c.body.to_vec(),
            ebml::DEFAULT_DURATION => e.default_duration = Some(ebml::uint(c.body)),
            ebml::NAME => e.name = Some(ebml::string(c.body)),
            ebml::LANGUAGE => e.language = Some(ebml::string(c.body)),
            ebml::LANGUAGE_BCP47 => e.language_bcp47 = Some(ebml::string(c.body)),
            ebml::FLAG_DEFAULT => e.default = ebml::uint(c.body) != 0,
            ebml::FLAG_FORCED => e.forced = ebml::uint(c.body) != 0,
            ebml::FLAG_HEARING_IMPAIRED => e.hearing_impaired = ebml::uint(c.body) != 0,
            ebml::FLAG_ENABLED => e.enabled = ebml::uint(c.body) != 0,
            ebml::CODEC_DELAY => e.codec_delay = ebml::uint(c.body),
            ebml::VIDEO => parse_video(&mut e, c.body),
            ebml::AUDIO => {
                for a in children(c.body) {
                    match a.id {
                        ebml::SAMPLING_FREQUENCY => e.sampling = Some(ebml::float(a.body)),
                        ebml::CHANNELS => e.channels = Some(ebml::uint(a.body) as u8),
                        ebml::BIT_DEPTH => e.bit_depth = Some(ebml::uint(a.body) as u8),
                        _ => {}
                    }
                }
            }
            ebml::BLOCK_ADDITION_MAPPING => {
                let mut ty = 0u64;
                let mut extra = None;
                for m in children(c.body) {
                    match m.id {
                        ebml::BLOCK_ADD_ID_TYPE => ty = ebml::uint(m.body),
                        ebml::BLOCK_ADD_ID_EXTRA_DATA => extra = Some(m.body.to_vec()),
                        _ => {}
                    }
                }
                let fourcc = (ty as u32).to_be_bytes();
                if let (Some(data), b"dvcC" | b"dvvC" | b"dvwC") = (extra, &fourcc) {
                    e.dovi = Some(DoviRecord { fourcc, data });
                }
            }
            ebml::CONTENT_ENCODINGS => {
                for enc in children(c.body).filter(|x| x.id == ebml::CONTENT_ENCODING) {
                    let mut ty = 0;
                    for f in children(enc.body) {
                        if f.id == ebml::CONTENT_ENCODING_TYPE {
                            ty = ebml::uint(f.body);
                        }
                    }
                    for comp in children(enc.body).filter(|x| x.id == ebml::CONTENT_COMPRESSION) {
                        let mut algo = 0;
                        let mut settings = Vec::new();
                        for f in children(comp.body) {
                            match f.id {
                                ebml::CONTENT_COMP_ALGO => algo = ebml::uint(f.body),
                                ebml::CONTENT_COMP_SETTINGS => settings = f.body.to_vec(),
                                _ => {}
                            }
                        }
                        if ty == 0 && algo == 3 {
                            e.header_strip = Some(settings);
                        } else if ty == 0 {
                            e.compressed = true;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    e
}

fn parse_video(e: &mut Entry, b: &[u8]) {
    for v in children(b) {
        match v.id {
            ebml::PIXEL_WIDTH => e.width = Some(ebml::uint(v.body) as u32),
            ebml::PIXEL_HEIGHT => e.height = Some(ebml::uint(v.body) as u32),
            ebml::DISPLAY_WIDTH => e.display.0 = Some(ebml::uint(v.body)),
            ebml::DISPLAY_HEIGHT => e.display.1 = Some(ebml::uint(v.body)),
            ebml::DISPLAY_UNIT => e.display.2 = ebml::uint(v.body),
            ebml::FLAG_INTERLACED => {
                e.interlaced = match ebml::uint(v.body) {
                    1 => Some(true),
                    2 => Some(false),
                    _ => None,
                }
            }
            ebml::COLOUR => {
                for c in children(v.body) {
                    let u = ebml::uint(c.body);
                    match c.id {
                        // 2 = "unspecified" in H.273; keep it out of colr.
                        ebml::MATRIX_COEFFICIENTS if u != 2 => {
                            e.color.matrix = Some(u as u8);
                            e.has_color = true;
                        }
                        ebml::TRANSFER_CHARACTERISTICS if u != 2 => {
                            e.color.transfer = Some(u as u8);
                            e.has_color = true;
                        }
                        ebml::PRIMARIES if u != 2 => {
                            e.color.primaries = Some(u as u8);
                            e.has_color = true;
                        }
                        ebml::RANGE if u == 1 || u == 2 => e.color.full_range = Some(u == 2),
                        ebml::MAX_CLL => e.cll.0 = Some(u as u16),
                        ebml::MAX_FALL => e.cll.1 = Some(u as u16),
                        ebml::MASTERING_METADATA => e.mastering = parse_mastering(c.body),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

fn parse_mastering(b: &[u8]) -> Option<MasteringDisplay> {
    let mut f = [None::<f64>; 10];
    for c in children(b) {
        let i = match c.id {
            ebml::PRIMARY_G_X => 0,
            ebml::PRIMARY_G_Y => 1,
            ebml::PRIMARY_B_X => 2,
            ebml::PRIMARY_B_Y => 3,
            ebml::PRIMARY_R_X => 4,
            ebml::PRIMARY_R_Y => 5,
            ebml::WHITE_POINT_X => 6,
            ebml::WHITE_POINT_Y => 7,
            ebml::LUMINANCE_MAX => 8,
            ebml::LUMINANCE_MIN => 9,
            _ => continue,
        };
        f[i] = Some(ebml::float(c.body));
    }
    // Matroska stores CIE 1931 xy and cd/m²; MP4/SEI units are 0.00002 and 0.0001.
    let xy = |v: Option<f64>| v.map(|x| (x * 50_000.0).round() as u16);
    Some(MasteringDisplay {
        primaries: [
            [xy(f[0])?, xy(f[1])?],
            [xy(f[2])?, xy(f[3])?],
            [xy(f[4])?, xy(f[5])?],
        ],
        white_point: [xy(f[6])?, xy(f[7])?],
        max_luminance: (f[8]? * 10_000.0).round() as u32,
        min_luminance: (f[9].unwrap_or(0.0) * 10_000.0).round() as u32,
    })
}

fn map_codec(id: &str) -> Codec {
    match id {
        "V_MPEG4/ISO/AVC" => Codec::H264,
        "V_MPEGH/ISO/HEVC" => Codec::Hevc,
        "V_AV1" => Codec::Av1,
        "V_VP9" => Codec::Vp9,
        "V_VP8" => Codec::Vp8,
        "V_MPEG2" | "V_MPEG1" => Codec::Mpeg2Video,
        "V_PRORES" => Codec::ProRes,
        "V_MJPEG" => Codec::Mjpeg,
        "A_AC3" => Codec::Ac3,
        "A_EAC3" => Codec::Eac3,
        "A_TRUEHD" | "A_MLP" => Codec::TrueHd,
        "A_FLAC" => Codec::Flac,
        "A_OPUS" => Codec::Opus,
        "A_VORBIS" => Codec::Vorbis,
        "A_MPEG/L3" => Codec::Mp3,
        "A_MPEG/L2" | "A_MPEG/L1" => Codec::Mp2,
        "A_ALAC" => Codec::Alac,
        "S_TEXT/UTF8" | "S_TEXT/ASCII" => Codec::Subrip,
        "S_TEXT/ASS" => Codec::Ass,
        "S_TEXT/SSA" => Codec::Ssa,
        "S_TEXT/WEBVTT" => Codec::WebVtt,
        "S_HDMV/PGS" => Codec::Pgs,
        "S_VOBSUB" => Codec::VobSub,
        "S_DVBSUB" => Codec::DvbSub,
        s if s.starts_with("V_MPEG4/ISO/") => Codec::Mpeg4Part2,
        s if s.starts_with("A_AAC") => Codec::Aac,
        s if s.starts_with("A_DTS") => Codec::Dts,
        s if s.starts_with("A_PCM") => Codec::Pcm,
        _ => Codec::Other,
    }
}

/// Facts gathered from the first cluster(s).
#[derive(Default)]
struct FirstFrames {
    /// Per track number: (min raw ts seen, first frame bytes).
    seen: Vec<(u64, i64, Vec<u8>)>,
    /// Video block timestamps in decode order.
    video_ts: Vec<i64>,
    /// Per audio track: (raw block ts, frames before this block), first blocks.
    audio_ts: Vec<(u64, Vec<(i64, u32)>)>,
    /// Per audio track, over the blocks walked: (track, first ts, last ts,
    /// bytes of the blocks before the last), for a bitrate estimate where
    /// the codec header gives none.
    audio_bytes: Vec<AudioSample>,
}

/// Bytes and time span of an audio track's first blocks.
#[derive(Debug, Clone, Copy, Default)]
struct AudioSample {
    track: u64,
    first_ts: Option<i64>,
    last_ts: i64,
    /// Bytes of every block before the last one seen.
    bytes_before_last: u64,
    last_bytes: u64,
}

/// Video frames needed to measure the reorder depth when the SPS lacks it.
const REORDER_SAMPLES: usize = 24;
/// Audio frames used to check the grid origin.
const AUDIO_SAMPLES: usize = 8;

/// True when the video's reorder depth cannot be read from its parameter
/// sets (H.264 SPS without VUI bitstream restriction, other codecs).
fn needs_reorder_measurement(e: &Entry) -> bool {
    match map_codec(&e.codec_id) {
        Codec::H264 => h264::AvcConfig::parse(&e.codec_private)
            .and_then(|c| c.sps.first().and_then(|s| h264::parse_sps(s)))
            .is_none_or(|s| s.max_num_reorder_frames.is_none()),
        Codec::Hevc | Codec::Av1 | Codec::Vp9 => false,
        _ => true,
    }
}

fn read_first_frames(
    src: &Source,
    layout: &MkvLayout,
    entries: &[Entry],
    video_number: Option<u64>,
) -> Result<FirstFrames> {
    let video_samples = match video_number.and_then(|vn| entries.iter().find(|e| e.number == vn)) {
        // Without a DefaultDuration the frame rate is measured too.
        Some(e) if needs_reorder_measurement(e) || e.default_duration.is_none() => REORDER_SAMPLES,
        Some(_) => 1,
        None => 0,
    };
    // Headers only, through small non-retained windows (a 4K frame is
    // ~0.5 MB; audio may first appear ~1 s into the interleave). The few
    // payloads needed are read explicitly below.
    const FIRST_READ: usize = 128 << 10;
    const MAX_READ: u64 = 24 << 20;
    let mut win = Windows::streaming(src, FIRST_READ);
    let mut walker = BlockWalker::new(&mut win, layout.first_cluster, layout.data_end);
    let mut out = FirstFrames::default();
    let audio: Vec<u64> = entries.iter().filter(|e| e.kind == 2).map(|e| e.number).collect();
    let wanted: Vec<u64> = entries
        .iter()
        .filter(|e| matches!(e.kind, 1 | 2))
        .map(|e| e.number)
        .collect();
    out.audio_ts = audio.iter().map(|&n| (n, Vec::new())).collect();
    out.audio_bytes = audio
        .iter()
        .map(|&n| AudioSample {
            track: n,
            ..Default::default()
        })
        .collect();
    let mut frame_counts = vec![0u32; audio.len()];
    while let Some(b) = walker.next_block()? {
        if Some(b.track) == video_number && out.video_ts.len() < video_samples {
            out.video_ts.push(b.ts);
        }
        if let Some(i) = audio.iter().position(|&n| n == b.track) {
            if out.audio_ts[i].1.len() < AUDIO_SAMPLES {
                out.audio_ts[i].1.push((b.ts, frame_counts[i]));
            }
            frame_counts[i] += b.frames.len() as u32;
            let a = &mut out.audio_bytes[i];
            a.first_ts.get_or_insert(b.ts);
            a.bytes_before_last += a.last_bytes;
            a.last_bytes = b.frames.iter().map(|f| f.1 as u64).sum::<u64>();
            a.last_ts = b.ts;
        }
        match out.seen.iter_mut().find(|s| s.0 == b.track) {
            Some(s) => s.1 = s.1.min(b.ts),
            None => {
                // The whole block for audio (E-AC-3 may hold dependent
                // substreams after the independent frame); first frame for video.
                let (off, len) = b.frames[0];
                let len = if Some(b.track) == video_number {
                    len as u64
                } else {
                    b.frames.iter().map(|f| f.1 as u64).sum::<u64>().min(1 << 16)
                };
                let bytes = src.read_at(off, len as usize)?;
                out.seen.push((b.track, b.ts, bytes));
            }
        }
        let all_seen = wanted.iter().all(|n| out.seen.iter().any(|s| s.0 == *n));
        let video_full = out.video_ts.len() >= video_samples;
        let audio_full = out.audio_ts.iter().all(|a| a.1.len() >= AUDIO_SAMPLES);
        if all_seen && video_full && audio_full {
            break;
        }
        if walker.pos() > layout.first_cluster + MAX_READ {
            break;
        }
    }
    Ok(out)
}

/// Estimates the exact frame-grid origin (ns) of a constant-frame-size audio
/// track. Matroska stores timestamps in TimestampScale units (usually 1 ms),
/// rounded or — as ffmpeg's muxer does — truncated. The first frame of a
/// stream normally starts on an exact unit (often 0), so its timestamp is the
/// origin; the average of `ts − n × frame` over the first frames is only used
/// when the first frame is inconsistent with the rest (e.g. a damaged start).
fn audio_grid_origin(
    ts: &[(i64, u32)],
    layout: &MkvLayout,
    delay_ns: i64,
    timescale: u32,
    frame: u32,
) -> Option<i64> {
    let &(t0, n0) = ts.first()?;
    if frame == 0 || timescale == 0 {
        return None;
    }
    let to_ticks = |t: i64, n: u32| {
        (layout.ts_ns(t) - delay_ns) as f64 * timescale as f64 / 1e9 - n as f64 * frame as f64
    };
    let first = to_ticks(t0, n0);
    let mean = ts.iter().map(|&(t, n)| to_ticks(t, n)).sum::<f64>() / ts.len() as f64;
    let unit_ticks = layout.timestamp_scale as f64 * timescale as f64 / 1e9;
    let origin = if (mean - first).abs() <= unit_ticks {
        first
    } else {
        mean
    };
    Some(super::ticks_to_ns(origin.round() as i64, timescale))
}

/// Builds the playback index of a Matroska/WebM file.
pub(crate) fn build(src: &Source) -> Result<PlaybackIndex> {
    let file_len = src.len();
    let mut win = Windows::new(src, 64 << 10);
    let mut warnings = Vec::new();

    // EBML header
    let h = win.get(0, 64)?.to_vec();
    let mut p = 0;
    if ebml::read_id(&h, &mut p) != Some(ebml::EBML) {
        return Err(HlsError::mkv("missing EBML header"));
    }
    let Some(Some(ebml_size)) = ebml::read_size(&h, &mut p) else {
        return Err(HlsError::mkv("bad EBML header size"));
    };
    let ebml_body = win.get(p as u64, ebml_size as usize)?.to_vec();
    let doc_type = children(&ebml_body)
        .find(|c| c.id == ebml::DOC_TYPE)
        .map(|c| ebml::string(c.body))
        .unwrap_or_default();
    let container = if doc_type == "webm" {
        Container::WebM
    } else {
        Container::Matroska
    };
    let seg_pos = p as u64 + ebml_size;
    let sh = win.get(seg_pos, 12)?.to_vec();
    let mut q = 0;
    if ebml::read_id(&sh, &mut q) != Some(ebml::SEGMENT) {
        return Err(HlsError::mkv("missing Segment"));
    }
    let seg_size = ebml::read_size(&sh, &mut q).flatten();
    let seg_start = seg_pos + q as u64;
    let seg_end = seg_size.map(|s| seg_start + s).unwrap_or(file_len).min(file_len);

    // Level-1 walk up to the first cluster.
    let mut seek: Vec<(u32, u64)> = Vec::new();
    let mut info: Option<Vec<u8>> = None;
    let mut tracks: Option<Vec<u8>> = None;
    let mut cues_pos: Option<u64> = None;
    let mut first_cluster: Option<u64> = None;
    let mut pos = seg_start;
    let mut steps = 0;
    while pos < seg_end && steps < 64 {
        steps += 1;
        let hb = win.get(pos, 12)?.to_vec();
        let mut r = 0;
        let (Some(id), Some(size)) = (ebml::read_id(&hb, &mut r), ebml::read_size(&hb, &mut r)) else {
            break;
        };
        let body = pos + r as u64;
        match id {
            ebml::CLUSTER => {
                first_cluster = Some(pos);
                break;
            }
            ebml::SEEK_HEAD | ebml::INFO | ebml::TRACKS => {
                let Some(size) = size else { break };
                let b = win.get(body, size as usize)?.to_vec();
                match id {
                    ebml::SEEK_HEAD => {
                        for s in children(&b).filter(|c| c.id == ebml::SEEK) {
                            let mut sid = None;
                            let mut spos = None;
                            for f in children(s.body) {
                                match f.id {
                                    ebml::SEEK_ID => sid = Some(ebml::uint(f.body) as u32),
                                    ebml::SEEK_POSITION => spos = Some(ebml::uint(f.body)),
                                    _ => {}
                                }
                            }
                            if let (Some(i), Some(sp)) = (sid, spos) {
                                seek.push((i, seg_start + sp));
                            }
                        }
                    }
                    ebml::INFO => info = Some(b),
                    _ => tracks = Some(b),
                }
            }
            ebml::CUES => cues_pos = Some(pos),
            _ => {}
        }
        let Some(size) = size else { break };
        pos = body + size;
    }
    let seek_find = |id: u32| seek.iter().find(|s| s.0 == id).map(|s| s.1);
    let read_element = |at: u64, expect: u32| -> Result<Option<Vec<u8>>> {
        let hb = src.read_at(at, 12)?;
        let mut r = 0;
        if ebml::read_id(&hb, &mut r) != Some(expect) {
            return Ok(None);
        }
        let Some(Some(size)) = ebml::read_size(&hb, &mut r) else {
            return Ok(None);
        };
        Ok(Some(src.read_at(at + r as u64, size as usize)?))
    };
    if info.is_none() {
        if let Some(at) = seek_find(ebml::INFO) {
            info = read_element(at, ebml::INFO)?;
        }
    }
    if tracks.is_none() {
        if let Some(at) = seek_find(ebml::TRACKS) {
            tracks = read_element(at, ebml::TRACKS)?;
        }
    }
    let Some(info) = info else {
        return Err(HlsError::mkv("no Info element"));
    };
    let Some(tracks_body) = tracks else {
        return Err(HlsError::mkv("no Tracks element"));
    };
    let first_cluster = match first_cluster.or_else(|| seek_find(ebml::CLUSTER)) {
        Some(c) => c,
        None => return Err(HlsError::mkv("no Cluster found")),
    };

    let mut timestamp_scale = 1_000_000u64;
    let mut duration_raw = 0f64;
    for c in children(&info) {
        match c.id {
            ebml::TIMESTAMP_SCALE => timestamp_scale = ebml::uint(c.body).max(1),
            ebml::DURATION => duration_raw = ebml::float(c.body),
            _ => {}
        }
    }

    // Cues: via SeekHead, else walk level-1 headers after the first cluster.
    let cues_pos = cues_pos.or_else(|| seek_find(ebml::CUES));
    let cues_body = match cues_pos {
        Some(at) => read_element(at, ebml::CUES)?,
        None => None,
    };
    let data_end = cues_pos.filter(|&c| c > first_cluster).unwrap_or(seg_end);
    let layout = MkvLayout {
        timestamp_scale,
        segment_data_start: seg_start,
        first_cluster,
        data_end,
        cues_pos: cues_body.as_ref().and(cues_pos),
    };

    let entries: Vec<Entry> = children(&tracks_body)
        .filter(|c| c.id == ebml::TRACK_ENTRY)
        .map(|c| parse_track_entry(c.body))
        .collect();
    let video_number = entries
        .iter()
        .find(|e| e.kind == 1 && e.enabled)
        .map(|e| e.number);

    let first = read_first_frames(src, &layout, &entries, video_number)?;

    // Keyframes
    let mut keyframes = Vec::new();
    let mut method = IndexMethod::Cues;
    let mut scanned_end: Option<i64> = None;
    if let (Some(cb), Some(vn)) = (&cues_body, video_number) {
        for cp in children(cb).filter(|c| c.id == ebml::CUE_POINT) {
            let mut time = None;
            for f in children(cp.body) {
                match f.id {
                    ebml::CUE_TIME => time = Some(ebml::uint(f.body) as i64),
                    ebml::CUE_TRACK_POSITIONS => {
                        let (mut track, mut cpos, mut rel) = (0, None, None);
                        for g in children(f.body) {
                            match g.id {
                                ebml::CUE_TRACK => track = ebml::uint(g.body),
                                ebml::CUE_CLUSTER_POSITION => cpos = Some(ebml::uint(g.body)),
                                ebml::CUE_RELATIVE_POSITION => rel = Some(ebml::uint(g.body) as u32),
                                _ => {}
                            }
                        }
                        if let (true, Some(t), Some(cp)) = (track == vn, time, cpos) {
                            keyframes.push(Keyframe {
                                pts_ns: layout.ts_ns(t),
                                raw_ts: t,
                                pos: seg_start + cp,
                                rel,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    if keyframes.is_empty() {
        if let Some(vn) = video_number {
            method = IndexMethod::ClusterScan;
            warnings.push("no usable Cues: keyframes rebuilt by scanning all clusters".to_string());
            let (k, end) = scan_keyframes(src, &layout, vn)?;
            keyframes = k;
            scanned_end = Some(end);
        }
    }
    keyframes.sort_by_key(|k| k.raw_ts);
    keyframes.dedup_by_key(|k| k.raw_ts);

    // Tracks
    let mut out_tracks = Vec::with_capacity(entries.len());
    for (i, e) in entries.iter().enumerate() {
        let first_frame = first.seen.iter().find(|s| s.0 == e.number);
        let t = make_track(
            i as u32,
            e,
            first_frame.map(|s| (layout.ts_ns(s.1), s.2.as_slice())),
            &first,
            &layout,
            &mut warnings,
        );
        out_tracks.push(t);
    }
    let video = video_number.and_then(|vn| entries.iter().position(|e| e.number == vn));
    if let (Some(vi), Some(k0)) = (video, keyframes.first()) {
        // The grid origin of the video track is its first keyframe.
        out_tracks[vi].first_pts_ns = Some(k0.pts_ns);
    }
    if let (Some(vi), Some(vn), Some(last)) = (video, video_number, keyframes.last()) {
        // Where the video really ends (the last segment's EXTINF): a walk of
        // the last GOP's block headers.
        let from = if keyframes.len() == 1 {
            layout.first_cluster
        } else {
            last.pos
        };
        match video_end(src, &layout, vn, from, &out_tracks[vi]) {
            Ok(end) => out_tracks[vi].extra.end_ns = end,
            Err(e) => warnings.push(format!("video end not found: {e}")),
        }
    }

    let mut duration_ns = (duration_raw * timestamp_scale as f64).round() as i64;
    if duration_ns <= 0 {
        // No Duration element: the scan's last block (+ one video frame).
        let frame = video.and_then(|v| out_tracks[v].default_duration_ns).unwrap_or(0) as i64;
        if let Some(end) = scanned_end {
            duration_ns = layout.ts_ns(end) + frame;
        }
    }
    let presentation_offset_ns = presentation_offset(&out_tracks, video);
    if let (Some(vi), Some(vn)) = (video, video_number) {
        let mut firsts = Vec::new();
        for i in super::open_gop_samples(keyframes.len()) {
            if let Some(f) = keyframe_payload(src, &layout, &keyframes[i], vn)? {
                firsts.push(f);
            }
        }
        out_tracks[vi].open_gop = super::classify_open_gop(&out_tracks[vi], &firsts);
    }

    Ok(PlaybackIndex {
        version: INDEX_VERSION,
        source: src.id().clone(),
        container,
        duration_ns,
        tracks: out_tracks,
        video,
        keyframes,
        presentation_offset_ns,
        method,
        layout: Layout::Matroska(layout),
        stats: IndexStats::default(),
        warnings,
    })
}

/// The first bytes (≤ 64 KiB) of the video frame a keyframe entry points at.
fn keyframe_payload(src: &Source, layout: &MkvLayout, kf: &Keyframe, video: u64) -> Result<Option<Vec<u8>>> {
    match keyframe_range(src, layout, kf, video, kf.pos)? {
        Some((off, len)) => Ok(Some(src.read_at(off, (len as usize).min(64 << 10))?)),
        None => Ok(None),
    }
}

/// File range (offset, length) of the video frame a keyframe entry points
/// at, walking block headers from `from` (its cluster, or the first cluster).
pub(crate) fn keyframe_range(
    src: &Source,
    layout: &MkvLayout,
    kf: &Keyframe,
    video: u64,
    from: u64,
) -> Result<Option<(u64, u32)>> {
    let mut win = Windows::streaming(src, 64 << 10);
    let mut walker = BlockWalker::new(&mut win, from, layout.data_end);
    while let Some(b) = walker.next_block()? {
        if b.track == video && b.ts == kf.raw_ts {
            return Ok(b.frames.first().copied());
        }
        if walker.pos() > from + (16 << 20) {
            break;
        }
    }
    Ok(None)
}

/// D = max(reorder depth × frame duration, −(earliest PTS of any track), 0).
pub(crate) fn presentation_offset(tracks: &[IndexTrack], video: Option<usize>) -> i64 {
    let mut d = 0i64;
    if let Some(v) = video.and_then(|i| tracks.get(i)) {
        if let Some(fd) = v.frame_duration {
            d = super::ticks_to_ns(v.reorder_depth as i64 * fd as i64, v.timescale);
        } else if let Some(r) = v.frame_rate.filter(|r| r.num > 0) {
            // No DefaultDuration: the measured rate, plus the timestamp
            // rounding per frame (33/34 ms intervals at 30 fps), since these
            // times are not snapped. Without this term the B-frames of such
            // a track got negative composition offsets, which players
            // resolve by shifting the video (46 ms against the audio in a
            // 30 fps HEVC clip).
            let frame_ns = 1e9 * r.den as f64 / r.num as f64 + super::GRID_TOLERANCE_NS as f64;
            d = (v.reorder_depth as f64 * frame_ns).ceil() as i64;
        }
    }
    for t in tracks
        .iter()
        .filter(|t| matches!(t.kind, TrackKind::Video | TrackKind::Audio))
    {
        if let Some(p) = t.min_pts_ns() {
            d = d.max(-p);
        }
    }
    d
}

impl IndexTrack {
    /// Earliest presentation time observed at index time.
    fn min_pts_ns(&self) -> Option<i64> {
        self.first_pts_ns
    }
}

fn make_track(
    index: u32,
    e: &Entry,
    first: Option<(i64, &[u8])>,
    ff: &FirstFrames,
    layout: &MkvLayout,
    warnings: &mut Vec<String>,
) -> IndexTrack {
    let codec = map_codec(&e.codec_id);
    let kind = match e.kind {
        1 => TrackKind::Video,
        2 => TrackKind::Audio,
        17 => TrackKind::Subtitle,
        _ => TrackKind::Data,
    };
    if e.compressed {
        warnings.push(format!(
            "track {}: zlib/bzip/lzo-compressed blocks are not supported",
            e.number
        ));
    }
    let language = e
        .language_bcp47
        .clone()
        .or_else(|| e.language.clone())
        .filter(|l| l != "und");
    let mut t = IndexTrack {
        index,
        id: e.number,
        kind,
        codec,
        codec_tag: e.codec_id.clone(),
        codec_private: e.codec_private.clone(),
        synth_config: None,
        timescale: 1000,
        frame_duration: None,
        frame_rate: None,
        width: e.width,
        height: e.height,
        bit_depth: e.bit_depth,
        sample_rate: e.sampling.map(|s| s.round() as u32),
        channels: e.channels,
        language,
        name: e.name.clone(),
        default: e.default,
        forced: e.forced,
        hearing_impaired: e.hearing_impaired,
        color: e.has_color.then_some(e.color),
        hdr: None,
        dovi: e.dovi.clone(),
        reorder_depth: 0,
        // Matroska CodecDelay (ffmpeg writes it for AAC/Opus/E-AC-3 priming)
        // is subtracted from every block timestamp, as ffmpeg's demuxer does.
        first_pts_ns: first.map(|f| f.0 - e.codec_delay as i64),
        codec_string: None,
        profile: None,
        level: None,
        inband_parameter_sets: false,
        header_strip: e.header_strip.clone(),
        default_duration_ns: e.default_duration,
        codec_delay_ns: e.codec_delay,
        open_gop: false,
        bitrate: None,
        extra: Default::default(),
    };
    if kind == TrackKind::Video {
        t.extra.sar = display_sar(e);
        t.extra.interlaced = e.interlaced;
    }
    let first_bytes: Option<Vec<u8>> = first.map(|f| {
        let mut v = e.header_strip.clone().unwrap_or_default();
        v.extend_from_slice(f.1);
        v
    });
    match kind {
        TrackKind::Video => {
            // DisplayWidth/DisplayHeight decide the shape when present
            // (even when square), as in ffmpeg's demuxer; else the VUI.
            let container_sar = e.display.0.is_some() && e.display.1.is_some() && e.display.2 != 4;
            video_facts(&mut t, first_bytes.as_deref(), ff, layout, container_sar)
        }
        TrackKind::Audio => {
            audio_facts(&mut t, first_bytes.as_deref(), warnings);
            if let (Some(fd), Some((_, ts))) =
                (t.frame_duration, ff.audio_ts.iter().find(|a| a.0 == e.number))
            {
                if let Some(origin) = audio_grid_origin(ts, layout, t.codec_delay_ns as i64, t.timescale, fd)
                {
                    t.first_pts_ns = Some(origin);
                }
            }
            if t.bitrate.is_none() {
                t.bitrate = sampled_bitrate(&t, ff.audio_bytes.iter().find(|a| a.track == e.number), layout);
            }
        }
        _ => {}
    }
    t
}

/// Sample aspect ratio from DisplayWidth/DisplayHeight when they give a
/// different shape than the pixel size (anamorphic DVD/broadcast video):
/// SAR = (DW × H) : (DH × W), reduced.
fn display_sar(e: &Entry) -> Option<(u32, u32)> {
    let (Some(dw), Some(dh), unit) = e.display else {
        return None;
    };
    let (w, h) = (e.width? as u64, e.height? as u64);
    if unit == 4 || dw == 0 || dh == 0 || w == 0 || h == 0 {
        return None;
    }
    reduce_ratio(dw * h, dh * w)
}

/// `num:den` in lowest terms, `None` for 1:1 or a value that doesn't fit.
pub(crate) fn reduce_ratio(num: u64, den: u64) -> Option<(u32, u32)> {
    fn gcd(a: u64, b: u64) -> u64 {
        if b == 0 {
            a.max(1)
        } else {
            gcd(b, a % b)
        }
    }
    let g = gcd(num, den);
    let (n, d) = (num / g, den / g);
    if n == d || n == 0 || d == 0 {
        return None;
    }
    Some((u32::try_from(n).ok()?, u32::try_from(d).ok()?))
}

/// Average bitrate of an audio track from the frames seen in the
/// first-frames walk, where the codec header gives none (AAC, TrueHD, DTS,
/// Opus…), or exactly for PCM. Used to keep other tracks' bytes out of the
/// video rendition's BANDWIDTH (PB-10) and as the copied rendition's rate.
fn sampled_bitrate(t: &IndexTrack, seen: Option<&AudioSample>, layout: &MkvLayout) -> Option<u64> {
    if t.codec == Codec::Pcm {
        let bits = t.bit_depth.unwrap_or(16) as u64;
        return Some(t.sample_rate? as u64 * t.channels.unwrap_or(2) as u64 * bits);
    }
    // Bytes of the blocks before the last over the time to the last (block
    // timestamps; a block may hold several frames, e.g. TrueHD).
    let a = seen?;
    let span_ns = layout.ts_ns(a.last_ts - a.first_ts?);
    if span_ns <= 0 || a.bytes_before_last == 0 {
        return None;
    }
    Some((a.bytes_before_last as u128 * 8 * 1_000_000_000 / span_ns as u128) as u64)
}

fn video_facts(
    t: &mut IndexTrack,
    first: Option<&[u8]>,
    ff: &FirstFrames,
    layout: &MkvLayout,
    container_sar: bool,
) {
    // Exact timescale from DefaultDuration (research/03 §4.4 "Timescales").
    match t.default_duration_ns.and_then(rational_rate) {
        Some((rate, ts, fd)) => {
            t.frame_rate = Some(rate);
            t.timescale = ts;
            t.frame_duration = Some(fd);
        }
        None => {
            // VFR or unknown: a 90 kHz clock represents ms timestamps exactly.
            let per_sec = 1_000_000_000 / layout.timestamp_scale.clamp(1, 1_000_000_000);
            t.timescale = if 90_000 % per_sec == 0 || per_sec > u32::MAX as u64 {
                90_000
            } else {
                per_sec as u32
            };
            if let Some(dd) = t.default_duration_ns.filter(|&d| d > 0) {
                t.frame_rate = Some(Rational {
                    num: 1_000_000,
                    den: (dd / 1000) as u32,
                });
            } else {
                // No DefaultDuration: the rate the first frames really have
                // (PB-2; the frame grid stays off, timestamps stay real).
                t.frame_rate = measured_rate(&ff.video_ts, layout);
            }
        }
    }
    let mut sps_color = None;
    let mut sps_sar = None;
    let mut measured_reorder = None;
    match t.codec {
        Codec::H264 => {
            if let Some(c) = h264::AvcConfig::parse(&t.codec_private) {
                t.codec_string = Some(c.codec_string());
                t.level = Some(c.level_idc as u16);
                t.profile = Some(avc_profile_name(c.profile_idc, c.constraint_flags).to_string());
                if let Some(sps) = c.sps.first().and_then(|s| h264::parse_sps(s)) {
                    t.bit_depth = Some(sps.bit_depth);
                    sps_color = sps.color;
                    sps_sar = sps.sar;
                    measured_reorder = sps.max_num_reorder_frames;
                }
                if let Some(f) = first {
                    t.inband_parameter_sets =
                        codec::has_inband_parameter_sets(f, c.length_size as usize, NalFlavor::H264);
                }
            }
        }
        Codec::Hevc => {
            if let Some(c) = hevc::HevcConfig::parse(&t.codec_private) {
                t.codec_string = Some(c.codec_string("hvc1"));
                t.level = Some(c.level_idc as u16);
                t.bit_depth = Some(c.bit_depth_luma);
                t.profile = Some(
                    match c.profile_idc {
                        1 => "Main",
                        2 => "Main 10",
                        3 => "Main Still Picture",
                        4 => "Rext",
                        _ => "HEVC",
                    }
                    .to_string(),
                );
                if let Some(sps) = c.nalus(33).next().and_then(hevc::parse_sps) {
                    sps_color = sps.color;
                    sps_sar = sps.sar;
                    measured_reorder = Some(sps.max_num_reorder_pics);
                }
                if let Some(f) = first {
                    t.inband_parameter_sets =
                        codec::has_inband_parameter_sets(f, c.length_size as usize, NalFlavor::Hevc);
                }
                // Declarative SEI in hvcC, then the first keyframe's SEI.
                let mut sei = codec::SeiHdr::default();
                for nal in c.nalus(39) {
                    let s = codec::parse_hdr_sei(nal, 2);
                    sei.mastering = sei.mastering.or(s.mastering);
                    sei.cll = sei.cll.or(s.cll);
                }
                if let Some(f) = first {
                    let s = codec::sample_hdr_sei(f, c.length_size as usize, NalFlavor::Hevc);
                    sei.mastering = sei.mastering.or(s.mastering);
                    sei.cll = sei.cll.or(s.cll);
                }
                apply_sei(t, sei);
            }
        }
        Codec::Av1 => {
            t.codec_string = codec::av1_codec_string(&t.codec_private);
            t.bit_depth = codec::av1_bit_depth(&t.codec_private);
            t.profile = Some("Main".into());
            measured_reorder = Some(0);
        }
        Codec::Vp9 => {
            t.codec_string = Some("vp09.00.40.08".into());
            measured_reorder = Some(0);
        }
        _ => {}
    }
    let vui = sps_color.map(|c| ColorInfo {
        primaries: Some(c.primaries).filter(|&v| v != 2),
        transfer: Some(c.transfer).filter(|&v| v != 2),
        matrix: Some(c.matrix).filter(|&v| v != 2),
        full_range: Some(c.full_range),
    });
    t.color = super::merge_color(t.color, vui);
    if !container_sar {
        t.extra.sar = sps_sar;
    }
    // Reorder depth: from the SPS; otherwise measured on the first frames
    // (max displacement between decode order and sorted presentation order).
    t.reorder_depth = match measured_reorder {
        Some(r) => r,
        None => {
            let mut sorted = ff.video_ts.clone();
            sorted.sort_unstable();
            let fd_raw = t
                .default_duration_ns
                .map(|d| d as f64 / layout.timestamp_scale as f64)
                .unwrap_or(1.0)
                .max(1.0);
            ff.video_ts
                .iter()
                .zip(&sorted)
                .map(|(pts, s)| (((s - pts) as f64) / fd_raw).ceil().max(0.0) as u32)
                .max()
                .unwrap_or(0)
                + 1 // one frame of margin: the measurement only sees the first GOP
        }
    };
    let hdr_format = classify_hdr(t.color.as_ref(), t.dovi.is_some());
    let mut hdr = t.hdr.clone().unwrap_or(HdrInfo {
        format: hdr_format,
        mastering: None,
        cll: None,
        dovi: None,
    });
    hdr.format = hdr_format;
    if let Some(d) = &t.dovi {
        hdr.dovi = parse_dovi(&d.data);
    }
    t.hdr = Some(hdr);
}

/// Frame rate of a video track without DefaultDuration, measured on the
/// first frames' timestamps (raw units, decode order): the longest run of
/// regular intervals in presentation order (a decode-order sample can miss
/// frames at its end), its span over its frame count. Timestamp rounding
/// (one unit over the span) bounds the error, so a standard rate within
/// that is taken exactly; otherwise milli-fps.
fn measured_rate(ts: &[i64], layout: &MkvLayout) -> Option<Rational> {
    let mut t: Vec<i64> = ts.to_vec();
    t.sort_unstable();
    t.dedup();
    if t.len() < 3 {
        return None;
    }
    let d: Vec<i64> = t.windows(2).map(|w| w[1] - w[0]).collect();
    let mut sorted = d.clone();
    sorted.sort_unstable();
    let med = sorted[sorted.len() / 2] as f64;
    let regular = |x: i64| (x as f64 - med).abs() <= med * 0.4 + 1.0;
    let (mut best, mut start) = ((0usize, 0usize), 0usize);
    for i in 0..=d.len() {
        if i == d.len() || !regular(d[i]) {
            if i - start > best.1 - best.0 {
                best = (start, i);
            }
            start = i + 1;
        }
    }
    let n = best.1 - best.0;
    if n < 2 {
        return None;
    }
    let span = (t[best.1] - t[best.0]) as f64 * layout.timestamp_scale as f64;
    let frame_ns = span / n as f64;
    let tol = layout.timestamp_scale as f64 / span.max(1.0);
    super::standard_rate(frame_ns, tol.max(5e-4))
}

fn apply_sei(t: &mut IndexTrack, sei: codec::SeiHdr) {
    if sei.mastering.is_none() && sei.cll.is_none() {
        return;
    }
    let mut hdr = t.hdr.clone().unwrap_or(HdrInfo {
        format: HdrFormat::Sdr,
        mastering: None,
        cll: None,
        dovi: None,
    });
    hdr.mastering = hdr.mastering.or(sei.mastering);
    hdr.cll = hdr.cll.or(sei.cll);
    t.hdr = Some(hdr);
}

/// `dvcC`/`dvvC` record (Dolby Vision streams in ISOBMFF §3.2).
pub(crate) fn parse_dovi(d: &[u8]) -> Option<DoviConfig> {
    if d.len() < 5 {
        return None;
    }
    Some(DoviConfig {
        profile: d[2] >> 1,
        level: ((d[2] & 1) << 5) | (d[3] >> 3),
        rpu_present: d[3] & 0x04 != 0,
        el_present: d[3] & 0x02 != 0,
        bl_present: d[3] & 0x01 != 0,
        bl_signal_compat_id: d[4] >> 4,
    })
}

/// H.264 profile name from `profile_idc` and the avcC `profile_compatibility`
/// byte, as flux-probe (and ffprobe) name it: constraint_set1_flag (0x40)
/// makes Baseline "Constrained Baseline", which every decoder of Main or High
/// plays (the planner's `profile_satisfies`). Without it, a Constrained
/// Baseline file planned from the index was video-transcoded for clients
/// listing Main/High while the probe's plan said direct play (found by the
/// e2e test on long_2h_360p.mkv in Chrome).
pub(crate) fn avc_profile_name(p: u8, constraint_flags: u8) -> &'static str {
    match p {
        66 if constraint_flags & 0x40 != 0 => "Constrained Baseline",
        66 => "Baseline",
        77 => "Main",
        88 => "Extended",
        100 => "High",
        110 => "High 10",
        122 => "High 4:2:2",
        244 => "High 4:4:4",
        _ => "H.264",
    }
}

fn audio_facts(t: &mut IndexTrack, first: Option<&[u8]>, warnings: &mut Vec<String>) {
    let sr = t.sample_rate.unwrap_or(48_000);
    t.timescale = sr;
    match t.codec {
        Codec::Aac => {
            if let Some(c) = audio::AacConfig::parse(&t.codec_private) {
                t.timescale = c.sample_rate;
                t.sample_rate = Some(t.sample_rate.unwrap_or(c.sample_rate));
                t.frame_duration = Some(c.frame_samples);
                t.codec_string = Some(c.codec_string());
                t.profile = Some(match c.object_type {
                    2 => "LC".into(),
                    5 => "HE-AAC".into(),
                    29 => "HE-AACv2".into(),
                    o => format!("AOT {o}"),
                });
            } else {
                // Some old MKVs encode the profile in the CodecID and carry no ASC.
                t.frame_duration = Some(1024);
                t.codec_private = audio::aac_lc_asc(sr, t.channels.unwrap_or(2));
                t.codec_string = Some("mp4a.40.2".into());
            }
        }
        Codec::Ac3 | Codec::Eac3 => {
            let frames: Vec<audio::Ac3Frame> = first
                .map(|f| audio::split_ac3(f).into_iter().map(|x| x.0).collect())
                .unwrap_or_default();
            match frames.first() {
                Some(f0) => {
                    t.frame_duration = Some(f0.samples);
                    t.timescale = f0.sample_rate;
                    t.sample_rate = Some(f0.sample_rate);
                    if t.codec == Codec::Ac3 {
                        t.synth_config = Some(audio::dac3(f0).to_vec());
                        t.codec_string = Some("ac-3".into());
                        t.bitrate = Some(f0.frame_size as u64 * 8 * f0.sample_rate as u64 / 1536);
                    } else {
                        t.synth_config = audio::dec3(&frames);
                        t.codec_string = Some("ec-3".into());
                        let bytes: usize = frames.iter().map(|f| f.frame_size).sum();
                        t.bitrate = Some(bytes as u64 * 8 * f0.sample_rate as u64 / f0.samples.max(1) as u64);
                    }
                }
                None => warnings.push(format!(
                    "track {}: no AC-3/E-AC-3 sync frame found for dac3/dec3",
                    t.id
                )),
            }
        }
        Codec::Opus => {
            t.timescale = 48_000;
            t.codec_string = Some("Opus".into());
            t.frame_duration = first.and_then(audio::opus_packet_samples);
            if let Some(h) = audio::OpusHead::parse(&t.codec_private) {
                t.channels = Some(h.channels);
            }
        }
        Codec::Flac => {
            t.codec_string = Some("fLaC".into());
            if let Some(dfla) = audio::dfla_from_codec_private(&t.codec_private) {
                if let Some((rate, ch, bps)) = audio::flac_streaminfo(&dfla) {
                    t.timescale = rate;
                    t.sample_rate = Some(rate);
                    t.channels = Some(ch);
                    t.bit_depth = Some(bps);
                }
                // Fixed-blocksize streams have min == max block size.
                let min_b = u16::from_be_bytes([dfla[4], dfla[5]]);
                let max_b = u16::from_be_bytes([dfla[6], dfla[7]]);
                if min_b == max_b && min_b > 0 {
                    t.frame_duration = Some(min_b as u32);
                }
                t.synth_config = Some(dfla);
            }
        }
        Codec::Mp3 | Codec::Mp2 => {
            t.codec_string = Some(if t.codec == Codec::Mp3 {
                "mp4a.40.34".into()
            } else {
                "mp4a.40.33".into()
            });
            t.frame_duration = first.and_then(audio::mpeg_audio_frame_samples);
        }
        Codec::Dts => {
            t.frame_duration = first.and_then(audio::dts_frame_samples);
            t.profile = Some("DTS".into());
        }
        Codec::TrueHd => {
            t.frame_duration = Some(sr / 1200);
        }
        _ => {}
    }
}

/// Rebuilds the video keyframe list by walking every cluster (no Cues).
/// Also returns the largest block end time seen (raw units), which stands in
/// for a missing `Duration` (live captures written without one).
fn scan_keyframes(src: &Source, layout: &MkvLayout, video: u64) -> Result<(Vec<Keyframe>, i64)> {
    // Large sequential reads: ≈ file size / disk throughput, no seeks.
    let mut win = Windows::streaming(src, 8 << 20);
    let mut walker = BlockWalker::new(&mut win, layout.first_cluster, layout.data_end);
    let mut out = Vec::new();
    let mut end = 0i64;
    while let Some(b) = walker.next_block()? {
        end = end.max(b.ts + b.duration.unwrap_or(0) as i64);
        if b.track == video && b.key {
            out.push(Keyframe {
                pts_ns: layout.ts_ns(b.ts),
                raw_ts: b.ts,
                pos: b.cluster_pos,
                rel: None,
            });
        }
    }
    Ok((out, end))
}

/// Presentation end (ns) of the video track: the latest block timestamp
/// from `from` (the last keyframe's cluster) to the end of the cluster
/// data, plus that frame's duration (BlockDuration, else DefaultDuration,
/// else the measured frame rate). A last GOP of up to 8 MiB is one read
/// (the index stays at ~10 reads); a longer one is walked block header by
/// block header through 16 KiB windows; `None` when it is implausibly long.
fn video_end(src: &Source, layout: &MkvLayout, video: u64, from: u64, t: &IndexTrack) -> Result<Option<i64>> {
    const ONE_READ: u64 = 8 << 20;
    const MAX_WALK: u64 = 256 << 20;
    let range = layout.data_end.saturating_sub(from);
    if range > MAX_WALK {
        return Ok(None);
    }
    // (+ slack: the last element header must not straddle the window.)
    let chunk = if range <= ONE_READ {
        range as usize + (64 << 10)
    } else {
        16 << 10
    };
    let mut win = Windows::streaming(src, chunk);
    let mut walker = BlockWalker::new(&mut win, from, layout.data_end).only_track(video);
    let mut last: Option<(i64, Option<u64>)> = None;
    while let Some(b) = walker.next_block()? {
        if last.is_none_or(|(ts, _)| b.ts > ts) {
            last = Some((b.ts, b.duration));
        }
    }
    let Some((ts, dur)) = last else {
        return Ok(None);
    };
    let frame_ns = dur
        .map(|d| layout.ts_ns(d as i64))
        .or(t.default_duration_ns.map(|d| d as i64))
        .or_else(|| {
            t.frame_rate
                .filter(|r| r.num > 0)
                .map(|r| (1e9 * r.den as f64 / r.num as f64).round() as i64)
        })
        .unwrap_or(0);
    Ok(Some(layout.ts_ns(ts) + frame_ns))
}

// ------------------------------------------------------------------ segment reads

/// Frames of one track read for a segment.
#[derive(Debug, Clone)]
pub(crate) struct MkvFrame {
    /// Raw block timestamp (TimestampScale units).
    pub ts: i64,
    /// Position within a laced block (0 for unlaced).
    pub lace: u16,
    pub offset: u64,
    pub len: u32,
    pub key: bool,
    /// Payload copied during the walk (`SegmentQuery::capture`), so windows
    /// need not be retained.
    pub data: Option<bytes::Bytes>,
}

/// What to collect while walking a byte range of clusters.
pub(crate) struct SegmentQuery<'a> {
    /// Collect video frames of this track number (decode order).
    pub video: Option<u64>,
    /// Raw timestamp of the keyframe the video starts at.
    pub video_start: i64,
    /// Raw timestamp of the keyframe that starts the next segment (exclusive).
    pub video_end: Option<i64>,
    /// Collect frames of these track numbers whose PTS is in `[t_start, t_end)`.
    pub others: &'a [u64],
    pub t_start: i64,
    pub t_end: i64,
    /// Where to start walking (a Cluster position).
    pub start_pos: u64,
    /// Where the expected data ends (for sizing the first read).
    pub end_pos: u64,
    /// Read chunk size: `None` = read the whole expected range at once (video);
    /// small for audio-only requests, so large video payloads are skipped.
    pub chunk: Option<usize>,
    /// Keep every window read so payloads can be sliced from memory. Off for
    /// large segments: only element headers are read (payloads are skipped
    /// and later streamed as file ranges), bounding memory per request.
    pub retain: bool,
    /// Copy the payloads of `others` frames while their window is current
    /// (small audio frames), so the walk can use non-retained windows.
    pub capture: bool,
}

/// Reads the frames selected by `q`.
///
/// Video: blocks in file (= decode) order from the keyframe with PTS T_k up to
/// (excluding) the keyframe with PTS T_{k+1}. Other tracks: frames whose PTS is
/// in [t_start, t_end), which may require reading a little past the next
/// keyframe because muxers interleave by decode time.
pub(crate) fn read_segment<'s>(
    src: &'s Source,
    layout: &MkvLayout,
    q: &SegmentQuery<'_>,
) -> Result<(Vec<MkvFrame>, Windows<'s>)> {
    // Stop once blocks are this far past the end (tracks that end early would
    // otherwise make us read to EOF).
    let slack = (10_000_000_000i64 / layout.timestamp_scale.max(1) as i64).max(1);
    let horizon = match (q.video_end, q.others.is_empty()) {
        (Some(v), true) => v,
        (Some(v), false) => v.max(q.t_end),
        (None, true) => i64::MAX,
        (None, false) => q.t_end,
    };
    let expected = (q.end_pos.saturating_sub(q.start_pos) + (256 << 10)) as usize;
    let chunk = q.chunk.unwrap_or(expected.min(64 << 20));
    let mut win = if q.retain {
        Windows::new(src, chunk)
    } else {
        Windows::streaming(src, chunk)
    };
    let mut frames = Vec::new();
    {
        let mut walker = BlockWalker::new(&mut win, q.start_pos, layout.data_end);
        let mut video_started = q.video.is_none();
        let mut video_done = q.video.is_none();
        let mut others_done: Vec<bool> = vec![false; q.others.len()];
        let mut first_block = true;
        while let Some(b) = walker.next_block()? {
            if first_block && q.chunk.is_none() {
                // The first window covered the expected range; any overshoot
                // (audio past the next keyframe) is read in small pieces.
                first_block = false;
                walker.set_chunk(1 << 20);
            }
            if Some(b.track) == q.video {
                if !video_started {
                    if b.key && b.ts == q.video_start {
                        video_started = true;
                    } else {
                        continue;
                    }
                }
                if !video_done {
                    if b.key && Some(b.ts) == q.video_end {
                        video_done = true;
                    } else {
                        for (i, &(off, len)) in b.frames.iter().enumerate() {
                            frames.push(MkvFrame {
                                ts: b.ts,
                                lace: i as u16,
                                offset: off,
                                len,
                                key: b.key,
                                data: None,
                            });
                        }
                    }
                }
            } else if let Some(i) = q.others.iter().position(|&n| n == b.track) {
                if b.ts >= q.t_end {
                    others_done[i] = true;
                } else if b.ts >= q.t_start {
                    for (li, &(off, len)) in b.frames.iter().enumerate() {
                        let data = if q.capture {
                            Some(bytes::Bytes::from(walker.payload(off, len)?))
                        } else {
                            None
                        };
                        frames.push(MkvFrame {
                            ts: b.ts,
                            lace: li as u16,
                            offset: off,
                            len,
                            key: b.key,
                            data,
                        });
                    }
                }
            }
            if video_done && others_done.iter().all(|&d| d) {
                break;
            }
            if horizon != i64::MAX && b.ts > horizon.saturating_add(slack) && walker.pos() > q.end_pos {
                break;
            }
        }
    }
    Ok((frames, win))
}

/// One subtitle cue as read from the file: (start ns, end ns, payload).
pub(crate) type SubtitleBlock = (i64, i64, bytes::Bytes);

/// Default cue length when a block carries no duration.
const SUBTITLE_DEFAULT_NS: i64 = 2_000_000_000;

/// Every block of one (subtitle) track, handed to `f` in time order as it is
/// read; `f` returns `false` to stop early (a client went away).
///
/// - With Cues for the track (mkvmerge and ffmpeg index subtitle blocks),
///   only those blocks are read: one small read per cue plus one per
///   cluster, so turning a subtitle on costs O(cues), not O(file).
/// - Otherwise every cluster is walked, but only block headers are read
///   (other tracks' payloads are skipped through 4 KiB windows: 15 % of a
///   49 MB clip instead of all of it).
///
/// Returns where the blocks came from.
pub(crate) fn read_subtitles_each(
    src: &Source,
    layout: &MkvLayout,
    track: u64,
    f: &mut dyn FnMut(SubtitleBlock) -> bool,
) -> Result<SubtitleSource> {
    let cues = subtitle_cue_points(src, layout, track)?;
    // No cue leading to its block: fall back to the walk.
    if !cues.is_empty() && read_cued_blocks(src, layout, track, &cues, f)? {
        return Ok(SubtitleSource::Cues);
    }
    let mut win = Windows::streaming(src, 4 << 10);
    let mut walker = BlockWalker::new(&mut win, layout.first_cluster, layout.data_end).only_track(track);
    while let Some(b) = walker.next_block()? {
        if !emit_block(&mut walker, layout, &b, None, f)? {
            break;
        }
    }
    Ok(SubtitleSource::ClusterWalk)
}

/// How a track's subtitle blocks were found ([`crate::Asset::for_each_subtitle_block`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleSource {
    /// Through the track's cue points.
    Cues,
    /// By walking every cluster's block headers.
    ClusterWalk,
}

/// Hands one block's frames to `f` as cues; `false` when `f` asked to stop.
fn emit_block(
    walker: &mut BlockWalker<'_, '_>,
    layout: &MkvLayout,
    b: &Block,
    cue_duration: Option<u64>,
    f: &mut dyn FnMut(SubtitleBlock) -> bool,
) -> Result<bool> {
    let start = layout.ts_ns(b.ts);
    let end = start
        + b.duration
            .or(cue_duration)
            .map(|d| layout.ts_ns(d as i64))
            .unwrap_or(SUBTITLE_DEFAULT_NS);
    for &(off, len) in &b.frames {
        if !f((start, end, bytes::Bytes::from(walker.payload(off, len)?))) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// A subtitle block located by the Cues: (raw time, cluster offset,
/// position relative to the cluster body, duration in raw units).
type CuedBlock = (i64, u64, Option<u64>, Option<u64>);

/// The cue points of `track` (empty without Cues or without entries for it).
fn subtitle_cue_points(src: &Source, layout: &MkvLayout, track: u64) -> Result<Vec<CuedBlock>> {
    let Some(at) = layout.cues_pos else {
        return Ok(Vec::new());
    };
    let hb = src.read_at(at, 12)?;
    let mut r = 0;
    if ebml::read_id(&hb, &mut r) != Some(ebml::CUES) {
        return Ok(Vec::new());
    }
    let Some(Some(size)) = ebml::read_size(&hb, &mut r) else {
        return Ok(Vec::new());
    };
    // Cues of a long film with every subtitle indexed are a few MB.
    if size > 256 << 20 {
        return Ok(Vec::new());
    }
    let body = src.read_at(at + r as u64, size as usize)?;
    let mut out = Vec::new();
    for cp in children(&body).filter(|c| c.id == ebml::CUE_POINT) {
        let mut time = None;
        for f in children(cp.body) {
            match f.id {
                ebml::CUE_TIME => time = Some(ebml::uint(f.body) as i64),
                ebml::CUE_TRACK_POSITIONS => {
                    let (mut t, mut cpos, mut rel, mut dur) = (0, None, None, None);
                    for g in children(f.body) {
                        match g.id {
                            ebml::CUE_TRACK => t = ebml::uint(g.body),
                            ebml::CUE_CLUSTER_POSITION => cpos = Some(ebml::uint(g.body)),
                            ebml::CUE_RELATIVE_POSITION => rel = Some(ebml::uint(g.body)),
                            ebml::CUE_DURATION => dur = Some(ebml::uint(g.body)),
                            _ => {}
                        }
                    }
                    if let (true, Some(tm), Some(cp)) = (t == track, time, cpos) {
                        out.push((tm, layout.segment_data_start + cp, rel, dur));
                    }
                }
                _ => {}
            }
        }
    }
    out.sort_by_key(|c| (c.0, c.1));
    out.dedup();
    Ok(out)
}

/// Reads the blocks the cue points point at, emitting each as it is found
/// (the first cue is out after a few reads). A cue that doesn't lead to a
/// block of the track is skipped. Returns `false`, having emitted nothing,
/// when no cue leads anywhere (the caller then walks the clusters).
fn read_cued_blocks(
    src: &Source,
    layout: &MkvLayout,
    track: u64,
    cues: &[CuedBlock],
    f: &mut dyn FnMut(SubtitleBlock) -> bool,
) -> Result<bool> {
    let mut win = Windows::streaming(src, 4 << 10);
    let mut walker = BlockWalker::new(&mut win, layout.first_cluster, layout.data_end).only_track(track);
    let mut cluster: Option<(u64, u64)> = None; // (cluster offset, body offset)
    let (mut emitted, mut missed) = (0usize, 0usize);
    for &(time, cpos, rel, dur) in cues {
        if cluster.is_none_or(|c| c.0 != cpos) {
            walker.seek_in_cluster(cpos);
            cluster = walker.enter_cluster()?.map(|body| (cpos, body));
        }
        let Some((_, body)) = cluster else {
            missed += 1;
            continue;
        };
        // CueRelativePosition points at the block; without it (or if it is
        // off), scan the cluster for the block with the cue's time.
        let mut hit = None;
        for start in [rel.map(|r| body + r), Some(body)].into_iter().flatten() {
            if !walker.in_cluster(cpos) {
                walker.seek_in_cluster(cpos);
                walker.enter_cluster()?;
            }
            walker.seek_in_cluster(start);
            while let Some(b) = walker.next_block()? {
                if b.cluster_pos != cpos || b.ts > time {
                    break;
                }
                if b.ts == time {
                    hit = Some(b);
                    break;
                }
            }
            if hit.is_some() {
                break;
            }
        }
        match hit {
            Some(b) => {
                emitted += 1;
                if !emit_block(&mut walker, layout, &b, dur, f)? {
                    return Ok(true);
                }
            }
            None => missed += 1,
        }
        // `next_block` may have left the cluster: re-enter on the next cue.
        cluster = cluster.filter(|_| walker.in_cluster(cpos));
    }
    if missed > 0 {
        tracing::debug!(track, missed, emitted, "subtitle cue points without their block");
    }
    Ok(emitted > 0 || missed == 0)
}

/// All blocks of one (subtitle) track as (start ns, end ns, payload); see
/// [`read_subtitles_each`] for how they are found.
pub(crate) fn read_subtitles(src: &Source, layout: &MkvLayout, track: u64) -> Result<Vec<SubtitleBlock>> {
    let mut out = Vec::new();
    read_subtitles_each(src, layout, track, &mut |c| {
        out.push(c);
        true
    })?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avc_profile_names_keep_the_constraint_flag() {
        assert_eq!(avc_profile_name(66, 0xC0), "Constrained Baseline");
        assert_eq!(avc_profile_name(66, 0xE0), "Constrained Baseline");
        assert_eq!(avc_profile_name(66, 0x80), "Baseline");
        assert_eq!(avc_profile_name(77, 0x40), "Main");
        assert_eq!(avc_profile_name(100, 0x00), "High");
    }

    #[test]
    fn mastering_units() {
        // ST 2086 P3-D65 1000 nits: Matroska floats → SEI units
        let mut b = Vec::new();
        let mut put = |id: u16, v: f64| {
            b.extend_from_slice(&id.to_be_bytes());
            b.push(0x88);
            b.extend_from_slice(&v.to_be_bytes());
        };
        put(0x55D1, 0.68);
        put(0x55D2, 0.32);
        put(0x55D3, 0.265);
        put(0x55D4, 0.69);
        put(0x55D5, 0.15);
        put(0x55D6, 0.06);
        put(0x55D7, 0.3127);
        put(0x55D8, 0.329);
        put(0x55D9, 1000.0);
        put(0x55DA, 0.0001);
        let m = parse_mastering(&b).unwrap();
        assert_eq!(m.primaries, [[13250, 34500], [7500, 3000], [34000, 16000]]);
        assert_eq!(m.white_point, [15635, 16450]);
        assert_eq!((m.max_luminance, m.min_luminance), (10_000_000, 1));
    }
}
