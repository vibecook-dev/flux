//! Matroska / WebM header path (EBML RFC 8794, Matroska RFC 9559).
//!
//! Reads: EBML header → Segment → SeekHead → Info, Tracks, Chapters,
//! Attachments (headers only, never `FileData`), Tags (only when already in
//! the cache). **Cues and Clusters are never read**; `has_index` comes from
//! the SeekHead (or a Cues element met during the top-level walk). Files
//! without a SeekHead fall back to a bounded linear walk of top-level headers.

use std::collections::HashMap;

use flux_core::{
    Attachment, Chapter, Codec, ColorInfo, Container, ContentLightLevel, MasteringDisplay, Rational, Track,
    TrackKind,
};

use crate::bytes::string;
use crate::codec::{
    self, apply_audio, apply_video, audio, av1, dovi, h264, hevc, new_track, riff, AudioCfg, VideoCfg,
};
use crate::prober::{empty_result, FrameLocator, Parsed};
use crate::store::{malformed, Ctx, PErr, PResult};

#[allow(dead_code)]
mod id {
    pub const EBML: u32 = 0x1A45_DFA3;
    pub const DOC_TYPE: u32 = 0x4282;
    pub const SEGMENT: u32 = 0x1853_8067;
    pub const SEEK_HEAD: u32 = 0x114D_9B74;
    pub const SEEK: u32 = 0x4DBB;
    pub const SEEK_ID: u32 = 0x53AB;
    pub const SEEK_POSITION: u32 = 0x53AC;
    pub const INFO: u32 = 0x1549_A966;
    pub const TIMESTAMP_SCALE: u32 = 0x2AD7B1;
    pub const DURATION: u32 = 0x4489;
    pub const TITLE: u32 = 0x7BA9;
    pub const TRACKS: u32 = 0x1654_AE6B;
    pub const TRACK_ENTRY: u32 = 0xAE;
    pub const TRACK_NUMBER: u32 = 0xD7;
    pub const TRACK_UID: u32 = 0x73C5;
    pub const TRACK_TYPE: u32 = 0x83;
    pub const FLAG_ENABLED: u32 = 0xB9;
    pub const FLAG_DEFAULT: u32 = 0x88;
    pub const FLAG_FORCED: u32 = 0x55AA;
    pub const FLAG_HEARING_IMPAIRED: u32 = 0x55AB;
    pub const DEFAULT_DURATION: u32 = 0x23E383;
    pub const NAME: u32 = 0x536E;
    pub const LANGUAGE: u32 = 0x22B59C;
    pub const LANGUAGE_BCP47: u32 = 0x22B59D;
    pub const CODEC_ID: u32 = 0x86;
    pub const CODEC_PRIVATE: u32 = 0x63A2;
    pub const BLOCK_ADDITION_MAPPING: u32 = 0x41E4;
    pub const BLOCK_ADD_ID_TYPE: u32 = 0x41E7;
    pub const BLOCK_ADD_ID_EXTRA_DATA: u32 = 0x41ED;
    pub const VIDEO: u32 = 0xE0;
    pub const FLAG_INTERLACED: u32 = 0x9A;
    pub const PIXEL_WIDTH: u32 = 0xB0;
    pub const PIXEL_HEIGHT: u32 = 0xBA;
    pub const PIXEL_CROP_BOTTOM: u32 = 0x54AA;
    pub const PIXEL_CROP_TOP: u32 = 0x54BB;
    pub const PIXEL_CROP_LEFT: u32 = 0x54CC;
    pub const PIXEL_CROP_RIGHT: u32 = 0x54DD;
    pub const DISPLAY_WIDTH: u32 = 0x54B0;
    pub const DISPLAY_HEIGHT: u32 = 0x54BA;
    pub const DISPLAY_UNIT: u32 = 0x54B2;
    pub const STEREO_MODE: u32 = 0x53B8;
    pub const COLOUR: u32 = 0x55B0;
    pub const MATRIX_COEFFICIENTS: u32 = 0x55B1;
    pub const BITS_PER_CHANNEL: u32 = 0x55B2;
    pub const RANGE: u32 = 0x55B9;
    pub const TRANSFER_CHARACTERISTICS: u32 = 0x55BA;
    pub const PRIMARIES: u32 = 0x55BB;
    pub const MAX_CLL: u32 = 0x55BC;
    pub const MAX_FALL: u32 = 0x55BD;
    pub const MASTERING_METADATA: u32 = 0x55D0;
    pub const AUDIO: u32 = 0xE1;
    pub const SAMPLING_FREQUENCY: u32 = 0xB5;
    pub const OUTPUT_SAMPLING_FREQUENCY: u32 = 0x78B5;
    pub const CHANNELS: u32 = 0x9F;
    pub const BIT_DEPTH: u32 = 0x6264;
    pub const CONTENT_ENCODINGS: u32 = 0x6D80;
    pub const CONTENT_ENCODING: u32 = 0x6240;
    pub const CONTENT_COMPRESSION: u32 = 0x5034;
    pub const CONTENT_COMP_ALGO: u32 = 0x4254;
    pub const CONTENT_ENCRYPTION: u32 = 0x5035;
    pub const CHAPTERS: u32 = 0x1043_A770;
    pub const EDITION_ENTRY: u32 = 0x45B9;
    pub const EDITION_FLAG_DEFAULT: u32 = 0x45DB;
    pub const EDITION_FLAG_ORDERED: u32 = 0x45DD;
    pub const CHAPTER_ATOM: u32 = 0xB6;
    pub const CHAPTER_TIME_START: u32 = 0x91;
    pub const CHAPTER_TIME_END: u32 = 0x92;
    pub const CHAPTER_FLAG_HIDDEN: u32 = 0x98;
    pub const CHAPTER_FLAG_ENABLED: u32 = 0x4598;
    pub const CHAPTER_DISPLAY: u32 = 0x80;
    pub const CHAP_STRING: u32 = 0x85;
    pub const ATTACHMENTS: u32 = 0x1941_A469;
    pub const ATTACHED_FILE: u32 = 0x61A7;
    pub const FILE_NAME: u32 = 0x466E;
    pub const FILE_MEDIA_TYPE: u32 = 0x4660;
    pub const FILE_DATA: u32 = 0x465C;
    pub const TAGS: u32 = 0x1254_C367;
    pub const TAG: u32 = 0x7373;
    pub const TARGETS: u32 = 0x63C0;
    pub const TARGET_TYPE_VALUE: u32 = 0x68CA;
    pub const TAG_TRACK_UID: u32 = 0x63C5;
    pub const SIMPLE_TAG: u32 = 0x67C8;
    pub const TAG_NAME: u32 = 0x45A3;
    pub const TAG_STRING: u32 = 0x4487;
    pub const CUES: u32 = 0x1C53_BB6B;
    pub const CLUSTER: u32 = 0x1F43_B675;
    pub const TIMESTAMP: u32 = 0xE7;
    pub const SIMPLE_BLOCK: u32 = 0xA3;
    pub const BLOCK_GROUP: u32 = 0xA0;
    pub const BLOCK: u32 = 0xA1;
    pub const VOID: u32 = 0xEC;
    pub const CRC32: u32 = 0xBF;
}

/// Upper bounds on element bodies we read whole. Anything larger is either a
/// hostile file or something we don't need.
const MAX_SMALL_MASTER: u64 = 1 << 20;
const MAX_TRACKS: u64 = 16 << 20;
/// Linear-walk bound (top-level elements) when a SeekHead is missing.
const MAX_WALK: usize = 256;

/// A parsed element header.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ElemHdr {
    pub id: u32,
    /// Offset of the element's data (just after its header).
    pub data: u64,
    /// `None` = unknown size (live streams; only Segment and Cluster allow it).
    pub size: Option<u64>,
}

impl ElemHdr {
    pub(crate) fn end(&self) -> Option<u64> {
        self.size.and_then(|s| self.data.checked_add(s))
    }
}

/// Element ID: 1–4 bytes, marker bits kept (that's how IDs are written).
fn vint_id(b: &[u8]) -> Option<(u32, usize)> {
    let first = *b.first()?;
    let len = first.leading_zeros() as usize + 1;
    if len > 4 {
        return None;
    }
    let bytes = b.get(..len)?;
    Some((bytes.iter().fold(0u32, |a, &x| (a << 8) | u32::from(x)), len))
}

/// Data size: 1–8 bytes, marker removed; all value bits set = unknown.
fn vint_size(b: &[u8]) -> Option<(Option<u64>, usize)> {
    let first = *b.first()?;
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    let bytes = b.get(..len)?;
    // len is 1..=8; shift in u16 so len = 8 cannot overflow.
    let mut v = u64::from(first) & (0xffu64 >> len);
    for &x in &bytes[1..] {
        v = (v << 8) | u64::from(x);
    }
    let all_ones = (1u64 << (7 * len)) - 1;
    Some((if v == all_ones { None } else { Some(v) }, len))
}

fn parse_hdr(b: &[u8], off: u64) -> Option<ElemHdr> {
    let (id, il) = vint_id(b)?;
    let (size, sl) = vint_size(b.get(il..)?)?;
    Some(ElemHdr {
        id,
        data: off + (il + sl) as u64,
        size,
    })
}

/// Read the element header at `off` (must lie before `limit`).
pub(crate) fn read_header(ctx: &Ctx<'_>, off: u64, limit: u64) -> PResult<ElemHdr> {
    let n = limit.saturating_sub(off).min(12);
    if n < 2 {
        return malformed(
            ctx.container,
            format!("element header at {off} runs past its parent"),
        );
    }
    let b = ctx.bytes(off, n)?;
    match parse_hdr(b, off) {
        Some(h) => Ok(h),
        None => malformed(ctx.container, format!("bad element header at {off}")),
    }
}

/// Children of an in-memory master element body. Stops (and reports
/// `false`) at the first malformed or overrunning child.
pub(crate) fn children(body: &[u8]) -> (Vec<(u32, &[u8])>, bool) {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < body.len() {
        let Some(h) = body.get(pos..).and_then(|b| parse_hdr(b, pos as u64)) else {
            return (out, false);
        };
        let Some(size) = h.size else { return (out, false) };
        let start = h.data as usize;
        let Some(end) = usize::try_from(size).ok().and_then(|s| start.checked_add(s)) else {
            return (out, false);
        };
        let Some(data) = body.get(start..end) else {
            return (out, false);
        };
        out.push((h.id, data));
        pos = end;
    }
    (out, true)
}

pub(crate) fn uint(b: &[u8]) -> u64 {
    b.iter().take(8).fold(0u64, |a, &x| (a << 8) | u64::from(x))
}

fn float(b: &[u8]) -> Option<f64> {
    match b.len() {
        0 => Some(0.0),
        4 => Some(f64::from(f32::from_be_bytes(b.try_into().ok()?))),
        8 => Some(f64::from_be_bytes(b.try_into().ok()?)),
        _ => None,
    }
}

/// Positions of the top-level elements we care about.
#[derive(Debug, Default)]
struct TopLevel {
    seek_heads: Vec<u64>,
    info: Option<u64>,
    tracks: Option<u64>,
    chapters: Option<u64>,
    attachments: Option<u64>,
    tags: Option<u64>,
    has_cues: bool,
    first_cluster: Option<u64>,
    /// Where the linear walk stopped.
    walk_end: u64,
}

impl TopLevel {
    fn record(&mut self, id: u32, off: u64) {
        match id {
            id::SEEK_HEAD => {
                if !self.seek_heads.contains(&off) {
                    self.seek_heads.push(off)
                }
            }
            id::INFO => {
                self.info.get_or_insert(off);
            }
            id::TRACKS => {
                self.tracks.get_or_insert(off);
            }
            id::CHAPTERS => {
                self.chapters.get_or_insert(off);
            }
            id::ATTACHMENTS => {
                self.attachments.get_or_insert(off);
            }
            id::TAGS => {
                self.tags.get_or_insert(off);
            }
            id::CUES => self.has_cues = true,
            id::CLUSTER => {
                self.first_cluster.get_or_insert(off);
            }
            _ => {}
        }
    }
}

/// Walk top-level element headers from `start`. In free mode only cached
/// bytes are used (no reads); in paid mode reads are allowed, bounded by
/// `MAX_WALK`. Stops at the first Cluster (free mode, or once Info and
/// Tracks are known), at an unknown-size element, or at the segment end.
fn walk(ctx: &Ctx<'_>, top: &mut TopLevel, start: u64, seg_end: u64, paid: bool) -> PResult<()> {
    let mut pos = start;
    let mut n = 0;
    while pos < seg_end && n < MAX_WALK {
        let hlen = (seg_end - pos).min(12);
        if !paid && ctx.store().get(pos, hlen).is_none() {
            break;
        }
        let h = match read_header(ctx, pos, seg_end) {
            Ok(h) => h,
            // Junk after the last element (common in truncated files): stop the walk.
            Err(PErr::Fail(_)) => break,
            Err(e) => return Err(e),
        };
        top.record(h.id, pos);
        if h.id == id::CLUSTER && (!paid || (top.info.is_some() && top.tracks.is_some())) {
            break;
        }
        match h.end() {
            Some(e) if e > pos => pos = e,
            _ => break,
        }
        n += 1;
    }
    top.walk_end = top.walk_end.max(pos);
    Ok(())
}

/// Read a whole element body after checking its ID and size bound.
fn body_of<'a>(ctx: &Ctx<'a>, off: u64, want: u32, seg_end: u64, max: u64) -> PResult<&'a [u8]> {
    let h = read_header(ctx, off, seg_end)?;
    if h.id != want {
        return malformed(
            ctx.container,
            format!("expected element {want:#x} at {off}, found {:#x}", h.id),
        );
    }
    let Some(size) = h.size else {
        return malformed(ctx.container, format!("element {want:#x} has unknown size"));
    };
    if size > max {
        return malformed(
            ctx.container,
            format!("element {want:#x} is {size} bytes (limit {max})"),
        );
    }
    ctx.bytes(h.data, size)
}

struct Segment {
    data: u64,
    end: u64,
}

fn segment(ctx: &mut Ctx<'_>) -> PResult<Segment> {
    let size = ctx.file_size();
    let ebml = read_header(ctx, 0, size)?;
    if ebml.id != id::EBML {
        return malformed(ctx.container, "missing EBML header");
    }
    let ebml_size = ebml.size.filter(|s| *s <= 4096);
    let Some(ebml_size) = ebml_size else {
        return malformed(ctx.container, "bad EBML header size");
    };
    let body = ctx.bytes(ebml.data, ebml_size)?;
    let (kids, _) = children(body);
    if let Some((_, dt)) = kids.iter().find(|(i, _)| *i == id::DOC_TYPE) {
        ctx.container = if string(dt) == "webm" {
            Container::WebM
        } else {
            Container::Matroska
        };
    }
    let mut pos = ebml.data + ebml_size;
    for _ in 0..16 {
        if pos >= size {
            break;
        }
        let h = read_header(ctx, pos, size)?;
        if h.id == id::SEGMENT {
            let end = match h.end() {
                Some(e) if e <= size => e,
                Some(_) => {
                    ctx.warn("segment extends past end of file (truncated)");
                    size
                }
                None => size,
            };
            return Ok(Segment { data: h.data, end });
        }
        match h.end() {
            Some(e) => pos = e,
            None => break,
        }
    }
    malformed(ctx.container, "no Segment element")
}

/// Parse a SeekHead body into `top`; returns nested SeekHead positions.
fn parse_seek_head(body: &[u8], seg: &Segment, top: &mut TopLevel) -> Vec<u64> {
    let mut nested = Vec::new();
    let (seeks, _) = children(body);
    for (sid, sbody) in seeks {
        if sid != id::SEEK {
            continue;
        }
        let (kids, _) = children(sbody);
        let target = kids
            .iter()
            .find(|(i, _)| *i == id::SEEK_ID)
            .map(|(_, b)| uint(b) as u32);
        let pos = kids
            .iter()
            .find(|(i, _)| *i == id::SEEK_POSITION)
            .map(|(_, b)| uint(b));
        if let (Some(t), Some(p)) = (target, pos) {
            let abs = seg.data.saturating_add(p);
            if abs >= seg.end {
                continue;
            }
            if t == id::SEEK_HEAD {
                nested.push(abs);
            } else {
                top.record(t, abs);
            }
        }
    }
    nested
}

/// Per-track parse state we need after building the `Track`.
struct TrackExtra {
    number: u64,
    uid: Option<u64>,
    cfg: VideoCfg,
    known_codec_cfg: bool,
    header_stripping: bool,
}

pub(crate) fn parse(ctx: &mut Ctx<'_>) -> PResult<Parsed> {
    ctx.container = Container::Matroska;
    let seg = segment(ctx)?;
    let mut top = TopLevel::default();
    walk(ctx, &mut top, seg.data, seg.end, false)?;

    // SeekHead(s): the index of everything else.
    let mut pending: Vec<u64> = top.seek_heads.clone();
    let mut seen = Vec::new();
    while let Some(off) = pending.pop() {
        if seen.contains(&off) || seen.len() >= 4 {
            continue;
        }
        seen.push(off);
        match body_of(ctx, off, id::SEEK_HEAD, seg.end, MAX_SMALL_MASTER) {
            Ok(body) => pending.extend(parse_seek_head(body, &seg, &mut top)),
            Err(PErr::Fail(e)) => ctx.warn(format!("SeekHead: {e}")),
            Err(e) => return Err(e),
        }
    }

    if top.seek_heads.is_empty() {
        ctx.warn("no SeekHead: linear walk of top-level elements (Cues after the clusters are not seen)");
    }
    if top.info.is_none() || top.tracks.is_none() {
        let from = top.walk_end.max(seg.data);
        walk(ctx, &mut top, from, seg.end, true)?;
    }

    let mut r = empty_result(ctx.container);
    r.has_index = top.has_cues;

    // Info (required).
    let Some(info_off) = top.info else {
        return malformed(ctx.container, "no Info element");
    };
    let info = body_of(ctx, info_off, id::INFO, seg.end, MAX_SMALL_MASTER)?;
    let mut scale = 1_000_000u64;
    let mut duration = None;
    for (cid, b) in children(info).0 {
        match cid {
            id::TIMESTAMP_SCALE => scale = uint(b).max(1),
            id::DURATION => duration = float(b),
            id::TITLE => r.title = Some(string(b)).filter(|s| !s.is_empty()),
            _ => {}
        }
    }
    if let Some(d) = duration.filter(|d| d.is_finite() && *d > 0.0) {
        r.duration_ms = Some((d * scale as f64 / 1e6).round() as u64);
    }

    // Tracks (required).
    let Some(tracks_off) = top.tracks else {
        return malformed(ctx.container, "no Tracks element");
    };
    let tracks = body_of(ctx, tracks_off, id::TRACKS, seg.end, MAX_TRACKS)?;
    let (entries, ok) = children(tracks);
    if !ok {
        ctx.warn("Tracks element is truncated or malformed");
    }
    let mut extras = Vec::new();
    for (eid, body) in entries {
        if eid != id::TRACK_ENTRY {
            continue;
        }
        let index = r.tracks.len() as u32;
        if let Some((t, x)) = parse_track_entry(ctx, body, index) {
            r.tracks.push(t);
            extras.push(x);
        }
    }

    // Chapters (optional, usually before the clusters).
    if let Some(off) = top.chapters {
        let res = body_of(ctx, off, id::CHAPTERS, seg.end, MAX_SMALL_MASTER * 4);
        if let Some(body) = ctx.optional("chapters", res)? {
            r.chapters = parse_chapters(ctx, body, r.duration_ms);
        }
    }

    // Attachments (optional): names, types and sizes only.
    if let Some(off) = top.attachments {
        r.attachments = parse_attachments(ctx, off, seg.end)?;
    }

    // Tags: only when they are already in the cache (they often sit at the end
    // of the file; the title is normally in Info anyway).
    if let Some(off) = top.tags {
        if let Ok(body) = body_of(ctx, off, id::TAGS, seg.end, MAX_SMALL_MASTER * 4) {
            let (title, bps) = parse_tags(body);
            if r.title.is_none() {
                r.title = title;
            }
            for (t, x) in r.tracks.iter_mut().zip(&extras) {
                if let Some(b) = x.uid.and_then(|u| bps.get(&u)) {
                    t.bitrate = t.bitrate.or(Some(*b));
                }
            }
        }
    }

    // HDR classification and the keyframe locator for the main video track.
    for (t, x) in r.tracks.iter_mut().zip(&extras) {
        crate::hdr::classify(t, &x.cfg, x.known_codec_cfg);
    }
    let locator = r
        .tracks
        .iter()
        .zip(&extras)
        .find(|(t, _)| t.kind == TrackKind::Video)
        .filter(|(_, x)| !x.header_stripping)
        .map(|(t, x)| FrameLocator::Mkv {
            scan_from: top.first_cluster.unwrap_or(top.walk_end.max(seg.data)),
            segment_end: seg.end,
            track_number: x.number,
            codec: t.codec,
            nal_len_size: x.cfg.nal_len_size.unwrap_or(4),
        });
    Ok(Parsed { result: r, locator })
}

fn codec_of(codec_id: &str) -> Codec {
    match codec_id {
        "V_MPEG4/ISO/AVC" => Codec::H264,
        "V_MPEGH/ISO/HEVC" => Codec::Hevc,
        "V_AV1" => Codec::Av1,
        "V_VP9" => Codec::Vp9,
        "V_VP8" => Codec::Vp8,
        "V_MPEG2" => Codec::Mpeg2Video,
        "V_MPEG4/ISO/SP" | "V_MPEG4/ISO/ASP" | "V_MPEG4/ISO/AP" => Codec::Mpeg4Part2,
        "V_MS/VFW/FOURCC" => Codec::Other, // refined from BITMAPINFOHEADER
        "V_PRORES" => Codec::ProRes,
        "V_MJPEG" => Codec::Mjpeg,
        "A_AAC" => Codec::Aac,
        s if s.starts_with("A_AAC/") => Codec::Aac,
        "A_AC3" | "A_AC3/BSID9" | "A_AC3/BSID10" => Codec::Ac3,
        "A_EAC3" => Codec::Eac3,
        "A_TRUEHD" => Codec::TrueHd,
        s if s.starts_with("A_DTS") => Codec::Dts,
        "A_FLAC" => Codec::Flac,
        "A_OPUS" => Codec::Opus,
        "A_VORBIS" => Codec::Vorbis,
        "A_MPEG/L3" => Codec::Mp3,
        "A_MPEG/L2" => Codec::Mp2,
        s if s.starts_with("A_PCM/") => Codec::Pcm,
        "A_ALAC" => Codec::Alac,
        "A_MS/ACM" => Codec::Other, // refined from WAVEFORMATEX
        "S_TEXT/UTF8" | "S_TEXT/ASCII" => Codec::Subrip,
        "S_TEXT/ASS" | "S_ASS" => Codec::Ass,
        "S_TEXT/SSA" | "S_SSA" => Codec::Ssa,
        "S_TEXT/WEBVTT" | "D_WEBVTT/SUBTITLES" | "D_WEBVTT/CAPTIONS" => Codec::WebVtt,
        "S_HDMV/PGS" => Codec::Pgs,
        "S_VOBSUB" => Codec::VobSub,
        "S_DVBSUB" => Codec::DvbSub,
        _ => Codec::Other,
    }
}

fn parse_track_entry(ctx: &mut Ctx<'_>, body: &[u8], index: u32) -> Option<(Track, TrackExtra)> {
    let (kids, ok) = children(body);
    if !ok {
        ctx.warn(format!("track {index}: malformed TrackEntry"));
    }
    let mut number = 0u64;
    let mut uid = None;
    let mut ttype = 0u64;
    let mut codec_id = String::new();
    let mut private: &[u8] = &[];
    let mut name = None;
    let mut lang = None;
    let mut lang_bcp47 = None;
    let mut default = true; // FlagDefault defaults to 1 (RFC 9559 §5.1.4.1.5)
    let mut forced = false;
    let mut hearing_impaired = false;
    let mut default_duration = None;
    let mut video: &[u8] = &[];
    let mut audio_el: &[u8] = &[];
    let mut dovi_cfg = None;
    let mut header_stripping = false;
    for (cid, b) in kids {
        match cid {
            id::TRACK_NUMBER => number = uint(b),
            id::TRACK_UID => uid = Some(uint(b)),
            id::TRACK_TYPE => ttype = uint(b),
            id::CODEC_ID => codec_id = string(b),
            id::CODEC_PRIVATE => private = b,
            id::NAME => name = Some(string(b)).filter(|s| !s.is_empty()),
            id::LANGUAGE => lang = Some(string(b)),
            id::LANGUAGE_BCP47 => lang_bcp47 = Some(string(b)),
            id::FLAG_DEFAULT => default = uint(b) != 0,
            id::FLAG_FORCED => forced = uint(b) != 0,
            id::FLAG_HEARING_IMPAIRED => hearing_impaired = uint(b) != 0,
            id::DEFAULT_DURATION => default_duration = Some(uint(b)),
            id::VIDEO => video = b,
            id::AUDIO => audio_el = b,
            id::BLOCK_ADDITION_MAPPING => {
                let (m, _) = children(b);
                let ty = m
                    .iter()
                    .find(|(i, _)| *i == id::BLOCK_ADD_ID_TYPE)
                    .map(|(_, b)| uint(b));
                let extra = m
                    .iter()
                    .find(|(i, _)| *i == id::BLOCK_ADD_ID_EXTRA_DATA)
                    .map(|(_, b)| *b);
                // BlockAddIDType 'dvcC' (0x64766343) / 'dvvC' (0x64767643).
                if matches!(ty, Some(0x6476_6343) | Some(0x6476_7643)) {
                    dovi_cfg = extra.and_then(dovi::parse_config);
                }
            }
            id::CONTENT_ENCODINGS => {
                for (_, enc) in children(b)
                    .0
                    .into_iter()
                    .filter(|(i, _)| *i == id::CONTENT_ENCODING)
                {
                    for (eid, eb) in children(enc).0 {
                        if eid == id::CONTENT_ENCRYPTION {
                            ctx.warn(format!("track {number}: encrypted"));
                        } else if eid == id::CONTENT_COMPRESSION {
                            let algo = children(eb)
                                .0
                                .iter()
                                .find(|(i, _)| *i == id::CONTENT_COMP_ALGO)
                                .map_or(0, |(_, b)| uint(b));
                            if algo == 3 {
                                header_stripping = true;
                            } else {
                                ctx.warn(format!("track {number}: compressed blocks (algo {algo})"));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let mut codec = codec_of(&codec_id);
    let kind = match ttype {
        1 => TrackKind::Video,
        2 => TrackKind::Audio,
        0x11 => TrackKind::Subtitle,
        _ => {
            if codec != Codec::Other {
                codec.kind()
            } else {
                TrackKind::Data
            }
        }
    };
    let mut t = new_track(index, number, kind, codec, codec_id.clone());
    t.title = name;
    t.language = lang_bcp47
        .filter(|s| !s.is_empty())
        .or(lang)
        .or_else(|| Some("eng".to_string()));
    t.default = default;
    t.forced = forced;
    t.hearing_impaired = hearing_impaired;
    t.frame_rate = default_duration
        .filter(|_| kind == TrackKind::Video)
        .and_then(codec::rate_from_ns);

    let mut cfg = VideoCfg {
        dovi: dovi_cfg,
        ..VideoCfg::default()
    };
    let mut known = false;
    match kind {
        TrackKind::Video => {
            let (container_cfg, color) = parse_video_element(video);
            t.width = container_cfg.width;
            t.height = container_cfg.height;
            t.sample_aspect_ratio = container_cfg.sar;
            t.interlaced = container_cfg.interlaced;
            t.bit_depth = container_cfg.bit_depth;
            t.color = color;
            cfg.mastering = container_cfg.mastering;
            cfg.cll = container_cfg.cll;
            let codec_cfg = match codec {
                Codec::H264 => h264::parse_avcc(private),
                Codec::Hevc => hevc::parse_hvcc(private),
                Codec::Av1 => av1::parse_av1c(private),
                Codec::Vp9 => vp9_private(private),
                Codec::Other if codec_id == "V_MS/VFW/FOURCC" => {
                    if let Some((fourcc, w, h, _)) = riff::bitmap_info(private) {
                        codec = riff::video_fourcc(fourcc);
                        t.codec = codec;
                        t.codec_tag = crate::bytes::fourcc_str(fourcc);
                        t.width = t.width.or(Some(w));
                        t.height = t.height.or(Some(h));
                    }
                    None
                }
                _ => None,
            };
            match codec_cfg {
                Some(c) => {
                    known = true;
                    apply_video(&mut t, &c);
                    cfg.absorb(c);
                }
                None if matches!(codec, Codec::H264 | Codec::Hevc | Codec::Av1) => {
                    ctx.warn(format!("track {number}: no usable {codec_id} CodecPrivate"));
                }
                None => known = matches!(codec, Codec::Vp9 | Codec::Vp8) && t.color.is_some(),
            }
        }
        TrackKind::Audio => {
            let (kids, _) = children(audio_el);
            let mut sr = None;
            let mut out_sr = None;
            let mut ch = Some(1u8); // Channels defaults to 1
            for (cid, b) in kids {
                match cid {
                    id::SAMPLING_FREQUENCY => sr = float(b),
                    id::OUTPUT_SAMPLING_FREQUENCY => out_sr = float(b),
                    id::CHANNELS => ch = u8::try_from(uint(b)).ok(),
                    id::BIT_DEPTH => t.bit_depth = u8::try_from(uint(b)).ok().filter(|d| *d > 0),
                    _ => {}
                }
            }
            t.channels = ch.filter(|c| *c > 0);
            t.sample_rate = out_sr
                .or(sr)
                .filter(|f| f.is_finite() && *f > 0.0 && *f < 1e7)
                .map(|f| f.round() as u32);
            let acfg: Option<AudioCfg> = match codec {
                Codec::Aac => audio::parse_asc(private),
                Codec::Opus => audio::parse_opus_head(private),
                Codec::Flac => audio::parse_flac_private(private),
                Codec::Vorbis => audio::parse_vorbis_private(private),
                Codec::Other if codec_id == "A_MS/ACM" => riff::wave_format_ex(private).map(|(_, c)| c),
                _ => None,
            };
            if let Some(a) = acfg {
                // Opus always plays at 48 kHz whatever the container says.
                if a.codec == Some(Codec::Opus) {
                    t.sample_rate = Some(48000);
                }
                apply_audio(&mut t, &a);
            }
            codec::normalize_audio_depth(&mut t);
            if codec == Codec::Dts {
                t.profile = match codec_id.as_str() {
                    "A_DTS/LOSSLESS" => Some("DTS-HD MA".into()),
                    "A_DTS/EXPRESS" => Some("DTS Express".into()),
                    _ => None,
                };
            }
        }
        _ => {}
    }
    Some((
        t,
        TrackExtra {
            number,
            uid,
            cfg,
            known_codec_cfg: known,
            header_stripping,
        },
    ))
}

/// `Video` master: pixel size (minus crop), display size → sample aspect
/// ratio, interlacing, bit depth and `Colour` (incl. MasteringMetadata,
/// MaxCLL/MaxFALL).
fn parse_video_element(b: &[u8]) -> (VideoCfg, Option<ColorInfo>) {
    let mut cfg = VideoCfg::default();
    let mut color: Option<ColorInfo> = None;
    let (mut w, mut h) = (None, None);
    let (mut ct, mut cb, mut cl, mut cr) = (0u64, 0u64, 0u64, 0u64);
    let (mut dw, mut dh, mut unit, mut stereo) = (None, None, 0u64, 0u64);
    for (cid, v) in children(b).0 {
        match cid {
            id::PIXEL_WIDTH => w = Some(uint(v)),
            id::PIXEL_HEIGHT => h = Some(uint(v)),
            id::DISPLAY_WIDTH => dw = Some(uint(v)),
            id::DISPLAY_HEIGHT => dh = Some(uint(v)),
            id::DISPLAY_UNIT => unit = uint(v),
            id::STEREO_MODE => stereo = uint(v),
            id::PIXEL_CROP_TOP => ct = uint(v),
            id::PIXEL_CROP_BOTTOM => cb = uint(v),
            id::PIXEL_CROP_LEFT => cl = uint(v),
            id::PIXEL_CROP_RIGHT => cr = uint(v),
            id::FLAG_INTERLACED => {
                cfg.interlaced = match uint(v) {
                    1 => Some(true),
                    2 => Some(false),
                    _ => None,
                }
            }
            id::COLOUR => {
                let mut c = ColorInfo::default();
                let (mut max_cll, mut max_fall) = (None, None);
                for (k, x) in children(v).0 {
                    let byte = u8::try_from(uint(x)).ok();
                    match k {
                        id::MATRIX_COEFFICIENTS => c.matrix = byte,
                        id::TRANSFER_CHARACTERISTICS => c.transfer = byte,
                        id::PRIMARIES => c.primaries = byte,
                        id::RANGE => {
                            c.full_range = match uint(x) {
                                1 => Some(false),
                                2 => Some(true),
                                _ => None,
                            }
                        }
                        id::BITS_PER_CHANNEL => cfg.bit_depth = byte.filter(|d| *d > 0),
                        id::MAX_CLL => max_cll = u16::try_from(uint(x)).ok(),
                        id::MAX_FALL => max_fall = u16::try_from(uint(x)).ok(),
                        id::MASTERING_METADATA => cfg.mastering = mastering_metadata(x),
                        _ => {}
                    }
                }
                if let (Some(max_cll), Some(max_fall)) = (max_cll, max_fall) {
                    cfg.cll = Some(ContentLightLevel { max_cll, max_fall });
                }
                color = (c != ColorInfo::default()).then_some(c);
            }
            _ => {}
        }
    }
    cfg.width = w
        .map(|w| w.saturating_sub(cl + cr))
        .and_then(|w| u32::try_from(w).ok())
        .filter(|w| *w > 0);
    cfg.height = h
        .map(|h| h.saturating_sub(ct + cb))
        .and_then(|h| u32::try_from(h).ok())
        .filter(|h| *h > 0);
    if let (Some(cw), Some(ch)) = (cfg.width, cfg.height) {
        cfg.sar = display_sar(u64::from(cw), u64::from(ch), dw, dh, unit, stereo);
    }
    (cfg, color)
}

/// The sample aspect ratio a Matroska `Video` element states, as FFmpeg's
/// demuxer derives it (and so as ffprobe prints it). DisplayWidth/Height
/// default to the (cropped) pixel size, so a track without them states
/// square pixels, and this container value beats the codec's VUI. Any
/// DisplayUnit below 4 (pixels, cm, inches, aspect ratio) is a ratio; 4
/// (unknown) states nothing. Side-by-side / top-bottom 3D halves the eye's
/// display width / height.
fn display_sar(w: u64, h: u64, dw: Option<u64>, dh: Option<u64>, unit: u64, stereo: u64) -> Option<Rational> {
    if unit >= 4 {
        return None;
    }
    let (dw, dh) = (dw.unwrap_or(w), dh.unwrap_or(h));
    let (wmul, hmul) = match stereo {
        1 | 8 | 9 | 11 => (2, 1),
        2 | 3 | 6 | 7 => (1, 2),
        _ => (1, 1),
    };
    let num = h.checked_mul(dw)?.checked_mul(wmul)?;
    let den = w.checked_mul(dh)?.checked_mul(hmul)?;
    codec::sar(num, den)
}

/// Matroska stores chromaticities and luminance as floats; convert to the
/// SEI integer units of `MasteringDisplay`.
fn mastering_metadata(b: &[u8]) -> Option<MasteringDisplay> {
    let mut v = [None::<f64>; 10];
    for (k, x) in children(b).0 {
        if (0x55D1..=0x55DA).contains(&k) {
            v[(k - 0x55D1) as usize] = float(x);
        }
    }
    let chroma = |f: Option<f64>| f.map(|f| (f * 50_000.0).round().clamp(0.0, 65_535.0) as u16);
    let lum = |f: Option<f64>| f.map(|f| (f * 10_000.0).round().clamp(0.0, u32::MAX as f64) as u32);
    // Order in the element table: R x/y, G x/y, B x/y, WP x/y, Lmax, Lmin.
    Some(MasteringDisplay {
        primaries: [
            [chroma(v[2])?, chroma(v[3])?],
            [chroma(v[4])?, chroma(v[5])?],
            [chroma(v[0])?, chroma(v[1])?],
        ],
        white_point: [chroma(v[6])?, chroma(v[7])?],
        max_luminance: lum(v[8])?,
        min_luminance: lum(v[9]).unwrap_or(0),
    })
}

/// Matroska `V_VP9` CodecPrivate: optional feature list (ID, length, value):
/// 1 profile, 2 level, 3 bit depth, 4 chroma subsampling.
fn vp9_private(b: &[u8]) -> Option<VideoCfg> {
    let mut cfg = VideoCfg::default();
    let mut i = 0usize;
    while i + 2 <= b.len() {
        let (fid, len) = (b[i], usize::from(b[i + 1]));
        let v = *b.get(i + 2)?;
        match fid {
            1 => cfg.profile = Some(format!("Profile {v}")),
            2 => cfg.level = Some(u16::from(v)),
            3 => cfg.bit_depth = Some(v),
            _ => {}
        }
        i += 2 + len;
    }
    (cfg != VideoCfg::default()).then_some(cfg)
}

fn parse_chapters(ctx: &mut Ctx<'_>, body: &[u8], duration_ms: Option<u64>) -> Vec<Chapter> {
    let editions: Vec<&[u8]> = children(body)
        .0
        .into_iter()
        .filter(|(i, _)| *i == id::EDITION_ENTRY)
        .map(|(_, b)| b)
        .collect();
    let flag = |e: &[u8], fid: u32| children(e).0.iter().any(|(i, b)| *i == fid && uint(b) != 0);
    let Some(edition) = editions
        .iter()
        .find(|e| flag(e, id::EDITION_FLAG_DEFAULT))
        .or(editions.first())
    else {
        return Vec::new();
    };
    if flag(edition, id::EDITION_FLAG_ORDERED) {
        ctx.warn("ordered chapters (segment linking) are not resolved");
    }
    let mut out: Vec<Chapter> = Vec::new();
    for (aid, atom) in children(edition).0 {
        if aid != id::CHAPTER_ATOM {
            continue;
        }
        let (kids, _) = children(atom);
        let get = |fid: u32| kids.iter().find(|(i, _)| *i == fid).map(|(_, b)| *b);
        if get(id::CHAPTER_FLAG_HIDDEN).is_some_and(|b| uint(b) != 0)
            || get(id::CHAPTER_FLAG_ENABLED).is_some_and(|b| uint(b) == 0)
        {
            continue;
        }
        // Chapter times are unscaled nanoseconds.
        let start_ms = get(id::CHAPTER_TIME_START).map_or(0, |b| uint(b) / 1_000_000);
        let end_ms = get(id::CHAPTER_TIME_END).map(|b| uint(b) / 1_000_000);
        let title = get(id::CHAPTER_DISPLAY).and_then(|d| {
            children(d)
                .0
                .into_iter()
                .find(|(i, _)| *i == id::CHAP_STRING)
                .map(|(_, s)| string(s))
        });
        out.push(Chapter {
            start_ms,
            end_ms,
            title,
        });
    }
    out.sort_by_key(|c| c.start_ms);
    for i in 0..out.len() {
        if out[i].end_ms.is_none() {
            out[i].end_ms = out.get(i + 1).map(|n| n.start_ms).or(duration_ms);
        }
    }
    out
}

/// Enumerate AttachedFile headers without reading `FileData`.
fn parse_attachments(ctx: &mut Ctx<'_>, off: u64, seg_end: u64) -> PResult<Vec<Attachment>> {
    let mut out = Vec::new();
    let h = match ctx.optional("attachments", read_header(ctx, off, seg_end))? {
        Some(h) if h.id == id::ATTACHMENTS => h,
        _ => return Ok(out),
    };
    let end = h.end().unwrap_or(seg_end).min(seg_end);
    let mut pos = h.data;
    while pos < end && out.len() < 1024 {
        let r = attached_file(ctx, pos, end);
        match ctx.optional("attachments", r)? {
            Some((next, a)) => {
                if let Some(a) = a {
                    out.push(a);
                }
                pos = next;
            }
            None => break,
        }
    }
    Ok(out)
}

fn attached_file(ctx: &Ctx<'_>, pos: u64, end: u64) -> PResult<(u64, Option<Attachment>)> {
    let f = read_header(ctx, pos, end)?;
    let Some(fend) = f.end().filter(|e| *e <= end) else {
        return malformed(ctx.container, "AttachedFile overruns Attachments");
    };
    if f.id != id::ATTACHED_FILE {
        return Ok((fend, None));
    }
    let (mut name, mut mime, mut size) = (String::new(), String::new(), 0u64);
    let mut p = f.data;
    while p < fend {
        let c = read_header(ctx, p, fend)?;
        let Some(cend) = c.end().filter(|e| *e <= fend) else {
            break;
        };
        let csize = cend - c.data;
        match c.id {
            id::FILE_NAME if csize <= 4096 => name = string(ctx.bytes(c.data, csize)?),
            id::FILE_MEDIA_TYPE if csize <= 4096 => mime = string(ctx.bytes(c.data, csize)?),
            id::FILE_DATA => size = csize,
            _ => {}
        }
        p = cend;
    }
    Ok((fend, Some(Attachment { name, mime, size })))
}

/// Global TITLE and per-track BPS from Tags.
fn parse_tags(body: &[u8]) -> (Option<String>, HashMap<u64, u64>) {
    let mut title = None;
    let mut bps = HashMap::new();
    for (tid, tag) in children(body).0 {
        if tid != id::TAG {
            continue;
        }
        let (kids, _) = children(tag);
        let mut track_uid = None;
        let mut target_type = 50;
        for (k, b) in &kids {
            if *k == id::TARGETS {
                for (tk, tb) in children(b).0 {
                    match tk {
                        id::TAG_TRACK_UID => track_uid = Some(uint(tb)).filter(|u| *u != 0),
                        id::TARGET_TYPE_VALUE => target_type = uint(tb),
                        _ => {}
                    }
                }
            }
        }
        for (k, b) in &kids {
            if *k != id::SIMPLE_TAG {
                continue;
            }
            let (st, _) = children(b);
            let name = st
                .iter()
                .find(|(i, _)| *i == id::TAG_NAME)
                .map(|(_, b)| string(b));
            let value = st
                .iter()
                .find(|(i, _)| *i == id::TAG_STRING)
                .map(|(_, b)| string(b));
            match (name.as_deref(), value, track_uid) {
                (Some(n), Some(v), None) if n.eq_ignore_ascii_case("TITLE") && target_type >= 50 => {
                    title.get_or_insert(v);
                }
                (Some(n), Some(v), Some(uid)) if n.eq_ignore_ascii_case("BPS") => {
                    if let Ok(x) = v.trim().parse::<u64>() {
                        bps.insert(uid, x);
                    }
                }
                _ => {}
            }
        }
    }
    (title.filter(|t| !t.is_empty()), bps)
}

// ---------------------------------------------------------------------------
// Second pass: first video block of the first cluster (HDR SEI).

/// Find the first block of `track_number` in the first clusters and return
/// the byte range of its frame payload. Only element headers are read.
pub(crate) fn first_video_frame(
    ctx: &Ctx<'_>,
    scan_from: u64,
    seg_end: u64,
    track_number: u64,
) -> PResult<(u64, u64)> {
    // Find the first Cluster from `scan_from`.
    let mut pos = scan_from;
    let mut cluster = None;
    for _ in 0..MAX_WALK {
        if pos >= seg_end {
            break;
        }
        let h = read_header(ctx, pos, seg_end)?;
        if h.id == id::CLUSTER {
            cluster = Some(h);
            break;
        }
        match h.end() {
            Some(e) if e > pos => pos = e,
            _ => break,
        }
    }
    let Some(mut cl) = cluster else {
        return malformed(ctx.container, "no Cluster found");
    };
    // Walk up to 4 clusters' children looking for a block of our track.
    for _ in 0..4 {
        let cend = cl.end().unwrap_or(seg_end).min(seg_end);
        let mut p = cl.data;
        let mut blocks = 0;
        while p < cend && blocks < 512 {
            let h = read_header(ctx, p, cend)?;
            // Unknown-size clusters end where the next top-level element starts.
            if h.id == id::CLUSTER || h.id == id::CUES {
                break;
            }
            let Some(hend) = h.end().filter(|e| *e <= cend) else {
                return malformed(ctx.container, "block overruns cluster");
            };
            let block_at = match h.id {
                id::SIMPLE_BLOCK => Some((h.data, hend)),
                id::BLOCK_GROUP => {
                    // First child of a BlockGroup is normally the Block.
                    let b = read_header(ctx, h.data, hend)?;
                    (b.id == id::BLOCK).then(|| (b.data, b.end().unwrap_or(hend).min(hend)))
                }
                _ => None,
            };
            if let Some((bdata, bend)) = block_at {
                blocks += 1;
                let head = ctx.bytes(bdata, (bend - bdata).min(12))?;
                if let Some((tn, tl)) = vint_size(head) {
                    if tn == Some(track_number) {
                        let flags = *head.get(tl + 2).unwrap_or(&0);
                        if flags & 0x06 != 0 {
                            return Err(PErr::Fail(crate::ProbeError::Unsupported(
                                "laced video block".into(),
                            )));
                        }
                        let fstart = bdata + tl as u64 + 3;
                        return Ok((fstart, bend.max(fstart)));
                    }
                }
            }
            p = hend;
        }
        // Next cluster.
        if cend >= seg_end {
            break;
        }
        let h = read_header(ctx, cend, seg_end)?;
        if h.id != id::CLUSTER {
            break;
        }
        cl = h;
    }
    malformed(ctx.container, "no video block in the first clusters")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vints() {
        assert_eq!(vint_id(&[0x1a, 0x45, 0xdf, 0xa3]), Some((0x1A45_DFA3, 4)));
        assert_eq!(vint_size(&[0x81]), Some((Some(1), 1)));
        assert_eq!(vint_size(&[0x40, 0x02]), Some((Some(2), 2)));
        assert_eq!(
            vint_size(&[0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]),
            Some((None, 8))
        );
        assert_eq!(vint_size(&[0xff]), Some((None, 1)));
        assert_eq!(vint_size(&[0x00]), None);
        assert_eq!(vint_id(&[0x08]), None);
    }

    #[test]
    fn children_stop_on_overrun() {
        let (kids, ok) = children(&[0x83, 0x81, 0x01, 0x86, 0x85, b'A']);
        assert_eq!(kids.len(), 1);
        assert!(!ok);
    }

    /// EBML element with an 8-byte size field.
    fn el(eid: u32, body: &[u8]) -> Vec<u8> {
        let idb = eid.to_be_bytes();
        let skip = idb.iter().position(|b| *b != 0).unwrap_or(3);
        let mut v = idb[skip..].to_vec();
        v.push(0x01);
        v.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn display_size_states_the_pixel_aspect() {
        let sar = |dw, dh, unit, stereo| display_sar(720, 480, dw, dh, unit, stereo).map(|r| (r.num, r.den));
        // mkvmerge style (pixels, rounded), ffmpeg style (DAR, unit 3), absent.
        assert_eq!(sar(Some(853), Some(480), 0, 0), Some((853, 720)));
        assert_eq!(sar(Some(16), Some(9), 3, 0), Some((32, 27)));
        assert_eq!(sar(None, None, 0, 0), Some((1, 1)));
        assert_eq!(sar(Some(853), Some(480), 4, 0), None, "unit unknown");
        assert_eq!(sar(Some(1440), Some(480), 0, 1), Some((4, 1)), "side-by-side 3D");
        assert_eq!(sar(Some(0), Some(480), 0, 0), None);

        let video = [
            el(id::PIXEL_WIDTH, &[0x02, 0xd0]),
            el(id::PIXEL_HEIGHT, &[0x01, 0xe0]),
            el(id::DISPLAY_WIDTH, &[0x03, 0x55]),
            el(id::DISPLAY_HEIGHT, &[0x01, 0xe0]),
        ]
        .concat();
        let (cfg, _) = parse_video_element(&video);
        assert_eq!(cfg.sar, Some(Rational { num: 853, den: 720 }));
    }

    /// Dolby Vision profile 8.1 signalled through BlockAdditionMapping
    /// (`dvvC`), the way mkvmerge ≥ 63 writes it, plus PQ in `Colour`.
    #[test]
    fn dovi_block_addition_mapping() {
        let dovi = [1u8, 0, 0b0001_0000, 0b0011_0101, 0b0001_0000, 0, 0, 0];
        let colour = [
            el(id::TRANSFER_CHARACTERISTICS, &[16]),
            el(id::PRIMARIES, &[9]),
            el(id::MATRIX_COEFFICIENTS, &[9]),
        ]
        .concat();
        let video = [
            el(id::PIXEL_WIDTH, &[0x0f, 0x00]),
            el(id::PIXEL_HEIGHT, &[0x08, 0x70]),
            el(id::COLOUR, &colour),
        ]
        .concat();
        let mapping = [
            el(id::BLOCK_ADD_ID_TYPE, &0x6476_7643u32.to_be_bytes()),
            el(id::BLOCK_ADD_ID_EXTRA_DATA, &dovi),
        ]
        .concat();
        let entry = [
            el(id::TRACK_NUMBER, &[1]),
            el(id::TRACK_TYPE, &[1]),
            el(id::CODEC_ID, b"V_MPEGH/ISO/HEVC"),
            el(id::VIDEO, &video),
            el(id::BLOCK_ADDITION_MAPPING, &mapping),
        ]
        .concat();
        let store = crate::store::Store::new(0);
        let mut ctx = Ctx::new(&store, 0);
        let (mut t, x) = parse_track_entry(&mut ctx, &entry, 0).expect("track");
        crate::hdr::classify(&mut t, &x.cfg, x.known_codec_cfg);
        assert_eq!((t.width, t.height), (Some(3840), Some(2160)));
        let h = t.hdr.as_ref().expect("hdr");
        assert_eq!(h.format, flux_core::HdrFormat::DolbyVision);
        let d = h.dovi.expect("dovi");
        assert_eq!(
            (d.profile, d.level, d.bl_signal_compat_id, d.rpu_present),
            (8, 6, 1, true)
        );
        // No mastering metadata anywhere yet: the keyframe pass is wanted.
        assert!(crate::hdr::needs_frame_sei(&t));
    }
}
