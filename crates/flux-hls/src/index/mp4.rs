//! MP4/MOV playback index from the `moov` sample tables (parsed with
//! `mp4-atom` 0.16).
//!
//! `moov` already holds a complete sample table (better than Matroska Cues):
//! one or two reads, wherever `moov` lives (front or end of file).

use flux_core::{Codec, ColorInfo, Container, HdrInfo, Rational, TrackKind};
use mp4_atom::{Codec as SampleEntry, Colr, Decode, Encode, Moov, StszSamples, Trak};
use serde::{Deserialize, Serialize};

use super::{
    classify_hdr, mkv::avc_profile_name, mkv::parse_dovi, ticks_to_ns, DoviRecord, IndexMethod, IndexStats,
    IndexTrack, Keyframe, Layout, PlaybackIndex, INDEX_VERSION,
};
use crate::codec::{self, audio, h264, hevc, NalFlavor};
use crate::error::{HlsError, Result};
use crate::source::Source;

/// Compact sample tables of one track, as stored in `stbl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mp4Tables {
    pub timescale: u32,
    /// (sample_count, sample_delta)
    pub stts: Vec<(u32, u32)>,
    /// (sample_count, composition offset)
    pub ctts: Vec<(u32, i64)>,
    /// Per-sample sizes; empty when `constant_size` is set.
    pub sizes: Vec<u32>,
    pub constant_size: Option<u32>,
    pub sample_count: u32,
    /// (first_chunk (1-based), samples_per_chunk)
    pub stsc: Vec<(u32, u32)>,
    pub chunk_offsets: Vec<u64>,
    /// 1-based sync sample numbers; `None` = every sample is a sync sample.
    pub sync: Option<Vec<u32>>,
    /// Edit-list shift: presentation = composition − `edit_shift` (media ticks).
    pub edit_shift: i64,
}

/// Expanded per-sample view of [`Mp4Tables`] (built in memory, not cached).
#[derive(Debug, Clone, Default)]
pub struct Mp4Samples {
    pub dts: Vec<i64>,
    pub cts: Vec<i64>,
    pub size: Vec<u32>,
    pub offset: Vec<u64>,
    pub sync: Vec<bool>,
}

impl Mp4Samples {
    pub fn len(&self) -> usize {
        self.dts.len()
    }
    pub fn is_empty(&self) -> bool {
        self.dts.is_empty()
    }
    /// Presentation time (media ticks, edit list applied).
    pub fn pts(&self, i: usize, t: &Mp4Tables) -> i64 {
        self.dts[i] + self.cts[i] - t.edit_shift
    }
}

/// Most samples a track may have when its tables are expanded per sample
/// (≈ 39 h at 60 fps, ≈ 50 h of 48 kHz AAC; PCM is expanded per chunk
/// instead). Building a segment costs ~140 bytes of memory per sample in
/// it, so this also bounds what one segment request can allocate.
pub const MAX_SAMPLES: u32 = 8 << 20;

/// Fewest bytes of the file per sample of a track expanded per sample. Real
/// tracks are far above it (a 60 fps video at 8 kb/s would still pass); a
/// crafted `stsz` of millions of 1-byte samples is refused (reverify of
/// ROB-2: a 1.6 MB file cost ~250 MB per request at one byte per sample).
pub const MIN_SAMPLE_BYTES: u64 = 16;

/// Largest `moof` box accepted (real ones are KBs).
pub const MAX_MOOF: u64 = 64 << 20;

/// Decode time, composition offset and duration of every sample, in order,
/// walking the run-length tables without expanding them.
pub struct SampleTiming<'a> {
    t: &'a Mp4Tables,
    i: u32,
    dts: i64,
    stts: (usize, u32),
    ctts: (usize, u32),
}

impl Iterator for SampleTiming<'_> {
    /// (dts, cts offset, duration) in media ticks.
    type Item = (i64, i64, u32);

    fn next(&mut self) -> Option<Self::Item> {
        if self.i >= self.t.sample_count {
            return None;
        }
        while self.t.stts.get(self.stts.0).is_some_and(|e| self.stts.1 >= e.0) {
            self.stts = (self.stts.0 + 1, 0);
        }
        while self.t.ctts.get(self.ctts.0).is_some_and(|e| self.ctts.1 >= e.0) {
            self.ctts = (self.ctts.0 + 1, 0);
        }
        let delta = self.t.stts.get(self.stts.0).map_or(0, |e| e.1);
        let cts = self.t.ctts.get(self.ctts.0).map_or(0, |e| e.1);
        let out = (self.dts, cts, delta);
        self.dts += delta as i64;
        self.stts.1 += 1;
        self.ctts.1 += 1;
        self.i += 1;
        Some(out)
    }
}

impl Mp4Tables {
    /// Per-sample timing without allocating ([`SampleTiming`]).
    pub fn timing(&self) -> SampleTiming<'_> {
        SampleTiming {
            t: self,
            i: 0,
            dts: 0,
            stts: (0, 0),
            ctts: (0, 0),
        }
    }

    /// True when sample `i` (0-based) is a sync sample, given a cursor into
    /// the sorted `stss` list that the caller advances with `i`.
    fn is_sync(&self, i: u32, cursor: &mut usize) -> bool {
        match &self.sync {
            None => true,
            Some(list) => {
                while list.get(*cursor).is_some_and(|&k| k < i + 1) {
                    *cursor += 1;
                }
                list.get(*cursor) == Some(&(i + 1))
            }
        }
    }

    /// Size of sample `i`.
    pub fn sample_size(&self, i: u32) -> Option<u32> {
        match self.constant_size {
            Some(c) => (i < self.sample_count).then_some(c),
            None => self.sizes.get(i as usize).copied(),
        }
    }

    /// (file offset, size) of sample `i`, by walking the chunk table.
    pub fn sample_location(&self, i: u32) -> Option<(u64, u32)> {
        let mut first = 0u64; // first sample of the chunk
        for (ci, &chunk_off) in self.chunk_offsets.iter().enumerate() {
            let chunk_no = ci as u32 + 1;
            let per = self
                .stsc
                .iter()
                .rev()
                .find(|e| e.0 <= chunk_no)
                .map(|e| e.1 as u64)
                .unwrap_or(0);
            if (i as u64) < first + per {
                let mut off = chunk_off;
                for k in first..i as u64 {
                    off += self.sample_size(k as u32)? as u64;
                }
                return Some((off, self.sample_size(i)?));
            }
            first += per;
        }
        None
    }

    /// Bytes of samples `from..to` (no expansion).
    pub fn bytes_of(&self, from: u32, to: u32) -> u64 {
        let to = to.min(self.sample_count);
        if from >= to {
            return 0;
        }
        match self.constant_size {
            Some(c) => c as u64 * (to - from) as u64,
            None => self
                .sizes
                .get(from as usize..to as usize)
                .map_or(0, |x| x.iter().map(|&v| v as u64).sum()),
        }
    }

    /// Rejects tables a real file can't have before anything is allocated
    /// per sample (ROB-2): every sample is at least a byte of the file (PCM,
    /// expanded per chunk) or [`MIN_SAMPLE_BYTES`] on average (everything
    /// else), and tables expanded per sample are capped at [`MAX_SAMPLES`].
    /// (The sizes may sum past the end of a file that is still being copied:
    /// its `moov` describes the whole movie, and the part already there
    /// plays.)
    pub(crate) fn check(&self, file_len: u64, per_chunk: bool) -> Result<()> {
        let min_bytes = if per_chunk { 1 } else { MIN_SAMPLE_BYTES };
        if self.sample_count as u64 * min_bytes > file_len {
            return Err(HlsError::mp4(format!(
                "implausible sample table: {} samples in a {file_len}-byte file",
                self.sample_count
            )));
        }
        if !per_chunk && self.sample_count > MAX_SAMPLES {
            return Err(HlsError::mp4(format!(
                "implausible sample count {} (max {MAX_SAMPLES})",
                self.sample_count
            )));
        }
        Ok(())
    }

    /// One entry per chunk instead of per sample (for PCM, whose samples are
    /// single sample frames of a few bytes: 2.9 M per minute at 48 kHz).
    /// Each entry's decode time is its first sample's; `size` is the chunk's.
    pub fn expand_chunks(&self) -> Mp4Samples {
        let n = self.sample_count as u64;
        let mut s = Mp4Samples::default();
        // Decode time of sample k via the stts runs, walked incrementally.
        let mut run = 0usize;
        let mut run_left = self.stts.first().map(|e| e.0 as u64).unwrap_or(0);
        let mut t = 0i64;
        let mut sample = 0u64;
        for (ci, &off) in self.chunk_offsets.iter().enumerate() {
            let chunk_no = ci as u32 + 1;
            let per = self
                .stsc
                .iter()
                .rev()
                .find(|e| e.0 <= chunk_no)
                .map(|e| e.1 as u64)
                .unwrap_or(0)
                .min(n.saturating_sub(sample));
            if per == 0 {
                continue;
            }
            let bytes: u64 = match self.constant_size {
                Some(c) => c as u64 * per,
                None => self
                    .sizes
                    .get(sample as usize..(sample + per) as usize)
                    .map(|x| x.iter().map(|&v| v as u64).sum())
                    .unwrap_or(0),
            };
            s.dts.push(t);
            s.cts.push(0);
            s.size.push(bytes.min(u32::MAX as u64) as u32);
            s.offset.push(off);
            s.sync.push(true);
            // Advance t by `per` samples.
            let mut left = per;
            while left > 0 && run < self.stts.len() {
                let step = left.min(run_left);
                t += step as i64 * self.stts[run].1 as i64;
                left -= step;
                run_left -= step;
                if run_left == 0 {
                    run += 1;
                    run_left = self.stts.get(run).map(|e| e.0 as u64).unwrap_or(0);
                }
            }
            sample += per;
        }
        s
    }

    /// Expands run-length tables into per-sample arrays.
    pub fn expand(&self) -> Mp4Samples {
        let n = self.sample_count as usize;
        let mut s = Mp4Samples {
            dts: Vec::with_capacity(n),
            cts: Vec::with_capacity(n),
            size: Vec::with_capacity(n),
            offset: Vec::with_capacity(n),
            sync: Vec::with_capacity(n),
        };
        let mut t = 0i64;
        for &(count, delta) in &self.stts {
            let take = (count as usize).min(n - s.dts.len());
            for _ in 0..take {
                s.dts.push(t);
                t += delta as i64;
            }
            if s.dts.len() == n {
                break;
            }
        }
        while s.dts.len() < n {
            s.dts.push(t);
        }
        for &(count, off) in &self.ctts {
            // A run can claim u32::MAX samples: take only what is left.
            let take = (count as usize).min(n - s.cts.len());
            s.cts.extend(std::iter::repeat_n(off, take));
            if s.cts.len() == n {
                break;
            }
        }
        s.cts.resize(n, 0);
        match self.constant_size {
            Some(c) => s.size.resize(n, c),
            None => {
                s.size.extend(self.sizes.iter().take(n));
                s.size.resize(n, 0);
            }
        }
        // Chunk offsets → sample offsets.
        let mut sample = 0usize;
        for (ci, &chunk_off) in self.chunk_offsets.iter().enumerate() {
            let chunk_no = ci as u32 + 1;
            let per_chunk = self
                .stsc
                .iter()
                .rev()
                .find(|e| e.0 <= chunk_no)
                .map(|e| e.1)
                .unwrap_or(0);
            let mut off = chunk_off;
            for _ in 0..per_chunk {
                if sample >= n {
                    break;
                }
                s.offset.push(off);
                off += s.size[sample] as u64;
                sample += 1;
            }
        }
        s.offset.resize(n, 0);
        match &self.sync {
            None => s.sync.resize(n, true),
            Some(list) => {
                s.sync.resize(n, false);
                for &k in list {
                    if let Some(x) = s.sync.get_mut((k as usize).wrapping_sub(1)) {
                        *x = true;
                    }
                }
            }
        }
        s
    }
}

/// Per-track sample tables, parallel to [`PlaybackIndex::tracks`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mp4Layout {
    pub tracks: Vec<Mp4Tables>,
}

/// Body of an encoded box (drops the 8-byte header, or 12 for full boxes).
fn box_body<T: Encode>(atom: &T, full: bool) -> Option<Vec<u8>> {
    let mut v = Vec::new();
    atom.encode(&mut v).ok()?;
    let skip = if full { 12 } else { 8 };
    (v.len() >= skip).then(|| v[skip..].to_vec())
}

fn colr_info(c: &Option<Colr>) -> Option<ColorInfo> {
    match c {
        Some(Colr::Nclx {
            colour_primaries,
            transfer_characteristics,
            matrix_coefficients,
            full_range_flag,
        }) => Some(ColorInfo {
            primaries: Some(*colour_primaries as u8).filter(|&v| v != 2),
            transfer: Some(*transfer_characteristics as u8).filter(|&v| v != 2),
            matrix: Some(*matrix_coefficients as u8).filter(|&v| v != 2),
            full_range: Some(*full_range_flag),
        }),
        _ => None,
    }
}

fn top_level_moov(src: &Source) -> Result<(u64, u64)> {
    let mut pos = 0u64;
    let len = src.len();
    while pos + 8 <= len {
        let h = src.read_at(pos, 16)?;
        if h.len() < 8 {
            break;
        }
        let mut size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64;
        let kind = &h[4..8];
        let mut header = 8;
        if size == 1 && h.len() >= 16 {
            size = u64::from_be_bytes([h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]]);
            header = 16;
        } else if size == 0 {
            size = len - pos;
        }
        if size < header {
            return Err(HlsError::mp4(format!("bad box size at {pos}")));
        }
        if kind == b"moov" {
            return Ok((pos, size));
        }
        pos += size;
    }
    Err(HlsError::mp4("no moov box"))
}

/// Builds the playback index of an MP4/MOV file.
pub(crate) fn build(src: &Source) -> Result<PlaybackIndex> {
    let (moov_pos, moov_size) = top_level_moov(src)?;
    if moov_size > 512 << 20 {
        return Err(HlsError::mp4("moov larger than 512 MiB"));
    }
    let mut buf = src.read_at(moov_pos, moov_size as usize)?;
    let mut warnings = Vec::new();
    // QuickTime writes AC-3/E-AC-3 sample entries without dac3/dec3, which
    // mp4-atom requires; rename them so they decode as unknown entries and
    // synthesize the configuration from the first sync frame (as for MKV).
    let patched = neutralize_unconfigured_dolby(&mut buf);
    if patched > 0 {
        warnings.push(format!(
            "{patched} AC-3/E-AC-3 sample entr{} without dac3/dec3: configuration synthesized from the first frame",
            if patched == 1 { "y" } else { "ies" }
        ));
    }
    // Dolby Vision entries (`dvhe`, `dvh1`, …) decode as the standard entry
    // they extend; their DV record is read here (mp4-atom skips it).
    let extras = take_dolby_vision_entries(&mut buf);
    let moov = Moov::decode(&mut buf.as_slice()).map_err(|e| HlsError::mp4(format!("moov: {e}")))?;
    // Fragmented MP4 (empty `moov` + `moof`/`mdat` pairs): sample tables come
    // from the fragments' `traf`/`trun` boxes.
    let fragmented = moov.mvex.is_some()
        && moov
            .trak
            .iter()
            .any(|t| stbl_sample_count(&t.mdia.minf.stbl) == 0);
    let mut frag_tables = if fragmented {
        let trex: Vec<(u32, (u32, u32, u32))> = moov
            .mvex
            .as_ref()
            .map(|m| {
                m.trex
                    .iter()
                    .map(|t| {
                        (
                            t.track_id,
                            (
                                t.default_sample_duration,
                                t.default_sample_size,
                                t.default_sample_flags,
                            ),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let ids: Vec<(u32, u32)> = moov
            .trak
            .iter()
            .map(|t| (t.tkhd.track_id, t.mdia.mdhd.timescale.max(1)))
            .collect();
        let f = scan_fragments(src, moov_pos + moov_size, &ids, &trex)?;
        warnings.push(format!(
            "fragmented MP4: sample tables built from {} moof boxes",
            f.fragments
        ));
        Some(f.tables)
    } else {
        None
    };
    let head = src.read_at(0, 12)?;
    let container = if head.get(8..12) == Some(b"qt  ") {
        Container::Mov
    } else {
        Container::Mp4
    };
    let movie_ts = moov.mvhd.timescale.max(1);

    let mut tracks = Vec::new();
    let mut tables = Vec::new();
    for (i, trak) in moov.trak.iter().enumerate() {
        let frag = frag_tables.as_mut().and_then(|m| m.remove(&trak.tkhd.track_id));
        let x = extras.get(i).cloned().unwrap_or_default();
        match track_from_trak(i as u32, trak, movie_ts, src, &mut warnings, frag, x) {
            Ok((t, tab)) => {
                tracks.push(t);
                tables.push(tab);
            }
            Err(e) => warnings.push(format!("track {}: {e}", trak.tkhd.track_id)),
        }
    }
    let video = tracks.iter().position(|t| t.kind == TrackKind::Video);
    let mut keyframes = Vec::new();
    if let Some(vi) = video {
        // One pass over the run-length tables (no per-sample arrays: the
        // asset expands them once, ROB-2).
        let tab = &tables[vi];
        let mut cursor = 0usize;
        let mut end: Option<i64> = None;
        for (i, (dts, cts, dur)) in tab.timing().enumerate() {
            let pts = dts + cts - tab.edit_shift;
            end = Some(end.map_or(pts + dur as i64, |e: i64| e.max(pts + dur as i64)));
            if tab.is_sync(i as u32, &mut cursor) {
                keyframes.push(Keyframe {
                    pts_ns: ticks_to_ns(pts, tab.timescale),
                    raw_ts: pts,
                    pos: i as u64,
                    rel: None,
                });
            }
        }
        keyframes.sort_by_key(|k| k.raw_ts);
        keyframes.dedup_by_key(|k| k.raw_ts);
        if let Some(k0) = keyframes.first() {
            tracks[vi].first_pts_ns = Some(k0.pts_ns);
        }
        // The video's own end (last frame + its duration) ends the last
        // segment, not the movie duration.
        tracks[vi].extra.end_ns = end.map(|e| ticks_to_ns(e, tab.timescale));
        let mut firsts = Vec::new();
        for i in super::open_gop_samples(keyframes.len()) {
            if let Some((off, size)) = tab.sample_location(keyframes[i].pos as u32) {
                firsts.push(src.read_at(off, (size as usize).min(64 << 10))?);
            }
        }
        tracks[vi].open_gop = super::classify_open_gop(&tracks[vi], &firsts);
    }

    // D = the largest edit-list shift beyond the smallest composition offset,
    // so both video CTS and audio decode times stay ≥ 0 after shifting.
    let mut d = 0i64;
    for (t, tab) in tracks.iter().zip(&tables) {
        if !matches!(t.kind, TrackKind::Video | TrackKind::Audio) {
            continue;
        }
        let min_cts = tab.ctts.iter().map(|c| c.1).min().unwrap_or(0).min(0);
        d = d.max(ticks_to_ns(tab.edit_shift - min_cts, tab.timescale));
    }

    let duration_ns = if moov.mvhd.duration > 0 && !fragmented {
        ticks_to_ns(moov.mvhd.duration as i64, movie_ts)
    } else {
        tracks
            .iter()
            .zip(&tables)
            .map(|(_, tab)| {
                ticks_to_ns(
                    tab.stts.iter().map(|e| e.0 as i64 * e.1 as i64).sum(),
                    tab.timescale,
                )
            })
            .max()
            .unwrap_or(0)
    };

    Ok(PlaybackIndex {
        version: INDEX_VERSION,
        source: src.id().clone(),
        container,
        duration_ns,
        tracks,
        video,
        keyframes,
        presentation_offset_ns: d,
        method: if fragmented {
            IndexMethod::Fragments
        } else {
            IndexMethod::Moov
        },
        layout: Layout::Mp4(super::mp4::Mp4Layout { tracks: tables }),
        stats: IndexStats::default(),
        warnings,
    })
}

fn track_from_trak(
    index: u32,
    trak: &Trak,
    movie_ts: u32,
    src: &Source,
    warnings: &mut Vec<String>,
    frag: Option<Mp4Tables>,
    extras: EntryExtras,
) -> Result<(IndexTrack, Mp4Tables)> {
    let mdia = &trak.mdia;
    let stbl = &mdia.minf.stbl;
    let timescale = mdia.mdhd.timescale.max(1);
    let handler: [u8; 4] = mdia.hdlr.handler.into();
    let kind = match &handler {
        b"vide" => TrackKind::Video,
        b"soun" => TrackKind::Audio,
        b"subt" | b"text" | b"sbtl" => TrackKind::Subtitle,
        _ => TrackKind::Data,
    };
    let entry = stbl
        .stsd
        .codecs
        .first()
        .ok_or_else(|| HlsError::mp4("empty stsd"))?;

    let mut t = IndexTrack {
        index,
        id: trak.tkhd.track_id as u64,
        kind,
        codec: Codec::Other,
        codec_tag: String::new(),
        codec_private: Vec::new(),
        synth_config: None,
        timescale,
        frame_duration: None,
        frame_rate: None,
        width: None,
        height: None,
        bit_depth: None,
        sample_rate: None,
        channels: None,
        language: Some(mdia.mdhd.language.clone()).filter(|l| !l.is_empty() && l != "und"),
        name: None,
        default: trak.tkhd.enabled,
        forced: false,
        hearing_impaired: false,
        color: None,
        hdr: None,
        dovi: None,
        reorder_depth: 0,
        first_pts_ns: None,
        codec_string: None,
        profile: None,
        level: None,
        inband_parameter_sets: false,
        header_strip: None,
        default_duration_ns: None,
        codec_delay_ns: 0,
        open_gop: false,
        bitrate: None,
        extra: Default::default(),
    };
    let mut length_size = 4usize;
    let mut flavor = None;
    // `pasp` (h_spacing, v_spacing) of a visual sample entry.
    let mut pasp: Option<(u32, u32)> = None;
    let pasp_of = |p: &Option<mp4_atom::Pasp>| p.as_ref().map(|p| (p.h_spacing, p.v_spacing));
    match entry {
        SampleEntry::Avc1(e) => {
            pasp = pasp_of(&e.pasp);
            t.codec = Codec::H264;
            t.codec_tag = "avc1".into();
            t.codec_private = box_body(&e.avcc, false).unwrap_or_default();
            t.width = Some(e.visual.width as u32);
            t.height = Some(e.visual.height as u32);
            t.color = colr_info(&e.colr);
        }
        SampleEntry::Avc3(e) => {
            pasp = pasp_of(&e.pasp);
            t.codec = Codec::H264;
            t.codec_tag = "avc3".into();
            t.codec_private = box_body(&e.avcc, false).unwrap_or_default();
            t.width = Some(e.visual.width as u32);
            t.height = Some(e.visual.height as u32);
            t.color = colr_info(&e.colr);
            t.inband_parameter_sets = true;
        }
        SampleEntry::Hvc1(e) => {
            pasp = pasp_of(&e.pasp);
            t.codec = Codec::Hevc;
            t.codec_tag = "hvc1".into();
            t.codec_private = box_body(&e.hvcc, false).unwrap_or_default();
            t.width = Some(e.visual.width as u32);
            t.height = Some(e.visual.height as u32);
            t.color = colr_info(&e.colr);
        }
        SampleEntry::Hev1(e) => {
            pasp = pasp_of(&e.pasp);
            t.codec = Codec::Hevc;
            t.codec_tag = "hev1".into();
            t.codec_private = box_body(&e.hvcc, false).unwrap_or_default();
            t.width = Some(e.visual.width as u32);
            t.height = Some(e.visual.height as u32);
            t.color = colr_info(&e.colr);
            t.inband_parameter_sets = true;
        }
        SampleEntry::Av01(e) => {
            pasp = pasp_of(&e.pasp);
            t.codec = Codec::Av1;
            t.codec_tag = "av01".into();
            t.codec_private = box_body(&e.av1c, false).unwrap_or_default();
            t.width = Some(e.visual.width as u32);
            t.height = Some(e.visual.height as u32);
            t.color = colr_info(&e.colr);
        }
        // Not packageable as fMP4 (the planner transcodes them for HLS), but
        // a client may play the file as is, and a transcode needs the size.
        SampleEntry::Vp09(e) => {
            pasp = pasp_of(&e.pasp);
            t.codec = Codec::Vp9;
            t.codec_tag = "vp09".into();
            t.width = Some(e.visual.width as u32);
            t.height = Some(e.visual.height as u32);
            t.bit_depth = Some(e.vpcc.bit_depth);
            t.color = colr_info(&e.colr);
        }
        SampleEntry::Vp08(e) => {
            pasp = pasp_of(&e.pasp);
            t.codec = Codec::Vp8;
            t.codec_tag = "vp08".into();
            t.width = Some(e.visual.width as u32);
            t.height = Some(e.visual.height as u32);
        }
        SampleEntry::Jpeg(e) => {
            pasp = pasp_of(&e.pasp);
            t.codec = Codec::Mjpeg;
            t.codec_tag = "jpeg".into();
            t.width = Some(e.visual.width as u32);
            t.height = Some(e.visual.height as u32);
        }
        SampleEntry::Mp4a(e) => {
            let dc = &e.esds.es_desc.dec_config;
            t.codec_tag = "mp4a".into();
            t.channels = Some(e.audio.channel_count as u8);
            t.sample_rate = Some(e.audio.sample_rate.integer() as u32);
            if dc.object_type_indication == 0x6B || dc.object_type_indication == 0x69 {
                t.codec = Codec::Mp3;
                t.codec_string = Some("mp4a.40.34".into());
            } else {
                t.codec = Codec::Aac;
                t.codec_private = dc
                    .dec_specific
                    .as_ref()
                    .map(|d| d.raw.clone())
                    .unwrap_or_default();
                if t.codec_private.is_empty() {
                    t.codec_private = audio::aac_lc_asc(timescale, t.channels.unwrap_or(2));
                }
            }
        }
        SampleEntry::Opus(e) => {
            t.codec = Codec::Opus;
            t.codec_tag = "Opus".into();
            t.channels = Some(e.dops.output_channel_count);
            let mut head = b"OpusHead".to_vec();
            head.push(1);
            head.push(e.dops.output_channel_count);
            head.extend_from_slice(&e.dops.pre_skip.to_le_bytes());
            head.extend_from_slice(&e.dops.input_sample_rate.to_le_bytes());
            head.extend_from_slice(&e.dops.output_gain.to_le_bytes());
            head.push(0);
            t.codec_private = head;
        }
        SampleEntry::Flac(e) => {
            t.codec = Codec::Flac;
            t.codec_tag = "fLaC".into();
            t.synth_config = box_body(&e.dfla, true);
            t.channels = Some(e.audio.channel_count as u8);
        }
        // The entry's channel_count is 2 by convention (ETSI TS 102 366
        // Annex F); dac3/dec3 hold the real layout.
        SampleEntry::Ac3(e) => {
            t.codec = Codec::Ac3;
            t.codec_tag = "ac-3".into();
            t.synth_config = box_body(&e.dac3, false);
            t.channels = Some(audio::ac3_channels(e.dac3.acmod, e.dac3.lfeon));
        }
        SampleEntry::Eac3(e) => {
            t.codec = Codec::Eac3;
            t.codec_tag = "ec-3".into();
            t.synth_config = box_body(&e.dec3, false);
            t.channels = Some(match e.dec3.substreams.first() {
                Some(s) => {
                    let deps = if s.num_dep_sub > 0 {
                        s.chan_loc.unwrap_or(0)
                    } else {
                        0
                    };
                    audio::ac3_channels(s.acmod, s.lfeon) + audio::eac3_chan_loc_channels(deps)
                }
                None => e.audio.channel_count as u8,
            });
        }
        SampleEntry::Mp3(e) => {
            t.codec = Codec::Mp3;
            t.codec_tag = ".mp3".into();
            t.channels = Some(e.audio.channel_count as u8);
        }
        SampleEntry::Tx3g(_) => {
            t.codec = Codec::MovText;
            t.codec_tag = "tx3g".into();
        }
        SampleEntry::Wvtt(_) => {
            t.codec = Codec::WebVtt;
            t.codec_tag = "wvtt".into();
        }
        SampleEntry::Sowt(e) => pcm_track(&mut t, "sowt", &e.audio, e.format()),
        SampleEntry::Twos(e) => pcm_track(&mut t, "twos", &e.audio, e.format()),
        SampleEntry::Lpcm(e) => pcm_track(&mut t, "lpcm", &e.audio, e.format()),
        SampleEntry::In24(e) => pcm_track(&mut t, "in24", &e.audio, e.format()),
        SampleEntry::In32(e) => pcm_track(&mut t, "in32", &e.audio, e.format()),
        SampleEntry::Fl32(e) => pcm_track(&mut t, "fl32", &e.audio, e.format()),
        SampleEntry::Fl64(e) => pcm_track(&mut t, "fl64", &e.audio, e.format()),
        SampleEntry::S16l(e) => pcm_track(&mut t, "s16l", &e.audio, e.format()),
        SampleEntry::Ipcm(e) => pcm_track(&mut t, "ipcm", &e.audio, e.format()),
        SampleEntry::Fpcm(e) => pcm_track(&mut t, "fpcm", &e.audio, e.format()),
        SampleEntry::Unknown(fourcc, _) if fourcc.as_ref() == UNCONFIGURED_AC3 => {
            t.codec = Codec::Ac3;
            t.codec_tag = "ac-3".into();
        }
        SampleEntry::Unknown(fourcc, _) if fourcc.as_ref() == UNCONFIGURED_EAC3 => {
            t.codec = Codec::Eac3;
            t.codec_tag = "ec-3".into();
        }
        SampleEntry::Unknown(fourcc, body) => {
            t.codec_tag = fourcc.to_string();
            if kind == TrackKind::Video {
                // VisualSampleEntry: 24 bytes of reserved fields, then width
                // and height. A transcode needs them to keep the shape.
                let be16 = |at: usize| {
                    body.get(at..at + 2)
                        .map(|b| u16::from_be_bytes([b[0], b[1]]) as u32)
                };
                t.width = be16(24).filter(|&w| w > 0);
                t.height = be16(26).filter(|&h| h > 0);
            }
        }
        _ => t.codec_tag = "other".into(),
    }
    // A Dolby Vision entry keeps its own name (`dvhe`, `dvh1`…) and record.
    if let Some(dv) = extras.dovi_fourcc {
        t.codec_tag = String::from_utf8_lossy(&dv).into_owned();
    }
    if kind == TrackKind::Video {
        t.dovi = extras.dovi;
    }

    // Tables
    let stts: Vec<(u32, u32)> = stbl
        .stts
        .entries
        .iter()
        .map(|e| (e.sample_count, e.sample_delta))
        .collect();
    let ctts: Vec<(u32, i64)> = stbl
        .ctts
        .as_ref()
        .map(|c| {
            c.entries
                .iter()
                .map(|e| (e.sample_count, e.sample_offset))
                .collect()
        })
        .unwrap_or_default();
    let (sizes, constant_size, sample_count) = match &stbl.stsz.samples {
        StszSamples::Identical { count, size } => (Vec::new(), Some(*size), *count),
        StszSamples::Different { sizes } => (sizes.clone(), None, sizes.len() as u32),
    };
    let chunk_offsets: Vec<u64> = match (&stbl.stco, &stbl.co64) {
        (_, Some(c)) => c.entries.clone(),
        (Some(c), None) => c.entries.iter().map(|&o| o as u64).collect(),
        _ => return Err(HlsError::mp4("no stco/co64")),
    };
    let stsc = stbl
        .stsc
        .entries
        .iter()
        .map(|e| (e.first_chunk, e.samples_per_chunk))
        .collect();
    let sync = stbl.stss.as_ref().map(|s| s.entries.clone());
    let mut edit_shift = 0i64;
    if let Some(elst) = trak.edts.as_ref().and_then(|e| e.elst.as_ref()) {
        // Empty edits (media_time = −1) delay presentation; the first real
        // edit's media_time is where presentation starts.
        let mut dwell = 0i64;
        for e in &elst.entries {
            match e.media_time {
                None => dwell += e.segment_duration as i64,
                Some(mt) => {
                    edit_shift = mt as i64 - (dwell as i128 * timescale as i128 / movie_ts as i128) as i64;
                    break;
                }
            }
        }
    }
    let tables = match frag {
        // Fragment tables carry −(first decode time) in `edit_shift`.
        Some(mut f) => {
            f.edit_shift += edit_shift;
            f
        }
        None => Mp4Tables {
            timescale,
            stts,
            ctts,
            sizes,
            constant_size,
            sample_count,
            stsc,
            chunk_offsets,
            sync,
            edit_shift,
        },
    };
    // `moov` holds no samples in a fragmented file.
    let sample_count = tables.sample_count;
    tables.check(src.len(), t.codec == Codec::Pcm)?;

    // A `hev1`/`dvhe` entry may hold no parameter sets at all (they are only
    // in-band): complete its record from the first sync sample, so the SPS
    // facts below are known and a remux to `hvc1`/`dvh1`, which strips the
    // in-band sets, still decodes.
    if t.codec == Codec::Hevc && t.inband_parameter_sets {
        let empty = hevc::HevcConfig::parse(&t.codec_private).filter(|c| c.nalus(33).next().is_none());
        if let (Some(c), Some((off, size))) = (empty, first_sync_sample(&tables)) {
            if size < (16 << 20) {
                let bytes = src.read_at(off, size as usize)?;
                if let Some(rec) = complete_hvcc(&t.codec_private, &bytes, c.length_size as usize) {
                    t.codec_private = rec;
                    warnings.push(format!(
                        "track {}: hvcC without parameter sets: taken from the first keyframe",
                        t.id
                    ));
                }
            }
        }
    }

    // Codec facts
    let mut vui_sar = None;
    match t.codec {
        Codec::H264 => {
            if let Some(c) = h264::AvcConfig::parse(&t.codec_private) {
                t.codec_string = Some(c.codec_string());
                t.level = Some(c.level_idc as u16);
                t.profile = Some(avc_profile_name(c.profile_idc, c.constraint_flags).to_string());
                length_size = c.length_size as usize;
                flavor = Some(NalFlavor::H264);
                if let Some(sps) = c.sps.first().and_then(|s| h264::parse_sps(s)) {
                    t.bit_depth = Some(sps.bit_depth);
                    vui_sar = sps.sar;
                    t.reorder_depth = sps.max_num_reorder_frames.unwrap_or(2);
                    t.color = super::merge_color(t.color, sps.color.map(vui_to_color));
                }
            }
        }
        Codec::Hevc => {
            if let Some(c) = hevc::HevcConfig::parse(&t.codec_private) {
                // The source's entry; the HLS `CODECS` entry is named after
                // the entry flux writes (`playlist::video_codec_string`).
                let fourcc = if matches!(t.codec_tag.as_str(), "hev1" | "dvhe") {
                    "hev1"
                } else {
                    "hvc1"
                };
                t.codec_string = Some(c.codec_string(fourcc));
                t.level = Some(c.level_idc as u16);
                t.bit_depth = Some(c.bit_depth_luma);
                t.profile = Some(if c.profile_idc == 2 {
                    "Main 10".into()
                } else {
                    "Main".into()
                });
                length_size = c.length_size as usize;
                flavor = Some(NalFlavor::Hevc);
                if let Some(sps) = c.nalus(33).next().and_then(hevc::parse_sps) {
                    vui_sar = sps.sar;
                    t.reorder_depth = sps.max_num_reorder_pics;
                    t.color = super::merge_color(t.color, sps.color.map(vui_to_color));
                }
            }
        }
        Codec::Av1 => {
            t.codec_string = codec::av1_codec_string(&t.codec_private);
            t.bit_depth = codec::av1_bit_depth(&t.codec_private);
        }
        Codec::Aac => {
            if let Some(c) = audio::AacConfig::parse(&t.codec_private) {
                t.codec_string = Some(c.codec_string());
                t.frame_duration = Some(c.frame_samples);
                t.sample_rate = Some(c.sample_rate);
                t.profile = Some(if c.object_type == 2 {
                    "LC".into()
                } else {
                    format!("AOT {}", c.object_type)
                });
            }
        }
        Codec::Ac3 | Codec::Eac3 if t.synth_config.is_none() => {
            // QuickTime entry without dac3/dec3: synthesize from the first frame.
            t.codec_string = Some(if t.codec == Codec::Ac3 { "ac-3" } else { "ec-3" }.into());
            if let Some((off, len)) = first_sample(&tables) {
                let b = src.read_at(off, (len as usize).min(1 << 16))?;
                let frames: Vec<audio::Ac3Frame> = audio::split_ac3(&b).into_iter().map(|x| x.0).collect();
                match frames.first() {
                    Some(f0) => {
                        t.channels = Some(f0.channels());
                        t.sample_rate = Some(f0.sample_rate);
                        t.frame_duration = Some(f0.samples);
                        t.synth_config = if t.codec == Codec::Ac3 {
                            Some(audio::dac3(f0).to_vec())
                        } else {
                            audio::dec3(&frames)
                        };
                    }
                    None => warnings.push(format!("track {}: no AC-3 sync frame in the first sample", t.id)),
                }
            }
        }
        Codec::Ac3 => t.codec_string = Some("ac-3".into()),
        Codec::Eac3 => t.codec_string = Some("ec-3".into()),
        Codec::Opus => {
            t.codec_string = Some("Opus".into());
            t.sample_rate = Some(48_000);
        }
        Codec::Flac => t.codec_string = Some("fLaC".into()),
        _ => {}
    }
    if kind == TrackKind::Video {
        // Display: the tkhd matrix (portrait phone video is coded landscape
        // with a 90° matrix) and the pixel aspect (`pasp`, else the VUI).
        let m = &trak.tkhd.matrix;
        let matrix = [m.a, m.b, m.u, m.c, m.d, m.v, m.x, m.y, m.w];
        if matrix != IDENTITY_MATRIX {
            t.extra.matrix = Some(matrix);
            t.extra.rotation = super::matrix_rotation(&matrix);
        }
        match pasp {
            Some((h, v)) if h > 0 && v > 0 => t.extra.sar = super::mkv::reduce_ratio(h as u64, v as u64),
            _ => t.extra.sar = vui_sar,
        }
        if let Some(&(count, delta)) = tables.stts.first() {
            if delta > 0 && (tables.stts.len() == 1 || count as usize * 10 > sample_count as usize * 9) {
                t.frame_duration = Some(delta);
                let g = gcd(timescale, delta);
                t.frame_rate = Some(Rational {
                    num: timescale / g,
                    den: delta / g,
                });
            }
        }
        if t.frame_rate.is_none() && sample_count > 0 {
            // Coarse timescales alternate deltas (656/672 at 16 kHz for
            // 23.976 fps): derive the rate from the average instead.
            let total: i64 = tables.stts.iter().map(|e| e.0 as i64 * e.1 as i64).sum();
            t.frame_rate = super::average_rate(ticks_to_ns(total, timescale) as f64 / sample_count as f64);
        }
        t.default_duration_ns = t
            .frame_duration
            .map(|fd| ticks_to_ns(fd as i64, timescale) as u64);
        // In-band parameter sets / HDR SEI: look at the first sync sample.
        if let (Some(fl), true) = (flavor, sample_count > 0) {
            if let Some((off, size)) = first_sync_sample(&tables) {
                if size < (16 << 20) {
                    let bytes = src.read_at(off, size as usize)?;
                    if !t.inband_parameter_sets {
                        t.inband_parameter_sets = codec::has_inband_parameter_sets(&bytes, length_size, fl);
                    }
                    let sei = codec::sample_hdr_sei(&bytes, length_size, fl);
                    if sei.mastering.is_some() || sei.cll.is_some() {
                        t.hdr = Some(HdrInfo {
                            format: classify_hdr(t.color.as_ref(), false),
                            mastering: sei.mastering,
                            cll: sei.cll,
                            dovi: None,
                        });
                    }
                }
            }
        }
        let format = classify_hdr(t.color.as_ref(), t.dovi.is_some());
        let mut hdr = t.hdr.clone().unwrap_or(HdrInfo {
            format,
            mastering: None,
            cll: None,
            dovi: None,
        });
        hdr.format = format;
        hdr.dovi = t.dovi.as_ref().and_then(|d| parse_dovi(&d.data));
        t.hdr = Some(hdr);
    } else if kind == TrackKind::Audio {
        t.sample_rate = t.sample_rate.or(Some(timescale));
        if t.frame_duration.is_none() {
            if let Some(&(_, delta)) = tables.stts.first() {
                t.frame_duration = (tables.stts.len() <= 2).then_some(delta);
            }
        }
        let total: u64 = match tables.constant_size {
            Some(c) => c as u64 * sample_count as u64,
            None => tables.sizes.iter().map(|&s| s as u64).sum(),
        };
        let dur: i64 = tables.stts.iter().map(|e| e.0 as i64 * e.1 as i64).sum();
        if dur > 0 {
            t.bitrate = Some(total * 8 * timescale as u64 / dur as u64);
        }
        // The first sample decodes at 0; the edit list shifts its presentation.
        t.first_pts_ns = Some(ticks_to_ns(-tables.edit_shift, timescale));
    }
    if t.codec == Codec::Other && kind != TrackKind::Data {
        warnings.push(format!(
            "track {}: unsupported sample entry {}",
            t.id, t.codec_tag
        ));
    }
    Ok((t, tables))
}

/// The identity `tkhd` matrix (a, b, u, c, d, v, x, y, w).
const IDENTITY_MATRIX: [i32; 9] = [0x1_0000, 0, 0, 0, 0x1_0000, 0, 0, 0, 0x4000_0000];

/// Placeholder sample-entry types for AC-3/E-AC-3 entries without their
/// configuration box (see [`neutralize_unconfigured_dolby`]).
const UNCONFIGURED_AC3: &[u8; 4] = b"fxa3";
const UNCONFIGURED_EAC3: &[u8; 4] = b"fxe3";

/// Children of a box body as (type, start of the box, box end).
fn child_boxes(b: &[u8], from: usize) -> Vec<([u8; 4], usize, usize)> {
    let mut out = Vec::new();
    let mut p = from;
    while p + 8 <= b.len() {
        let mut size = u32::from_be_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]]) as usize;
        let kind = [b[p + 4], b[p + 5], b[p + 6], b[p + 7]];
        if size == 1 && p + 16 <= b.len() {
            size = u64::from_be_bytes([
                b[p + 8],
                b[p + 9],
                b[p + 10],
                b[p + 11],
                b[p + 12],
                b[p + 13],
                b[p + 14],
                b[p + 15],
            ]) as usize;
        } else if size == 0 {
            size = b.len() - p;
        }
        if size < 8 || p + size > b.len() {
            break;
        }
        out.push((kind, p, p + size));
        p += size;
    }
    out
}

/// The `stsd` box of each `trak` of a `moov` buffer, in order, as (box
/// start, box end); `None` for a `trak` without one.
fn stsd_boxes(moov: &[u8]) -> Vec<Option<(usize, usize)>> {
    child_boxes(moov, 8)
        .into_iter()
        .filter(|c| &c.0 == b"trak")
        .map(|(_, s, e)| {
            let mut level = vec![(s, e)];
            for want in [b"mdia", b"minf", b"stbl", b"stsd"] {
                level = level
                    .iter()
                    .flat_map(|&(s, e)| child_boxes(&moov[..e], s + 8))
                    .filter(|c| &c.0 == want)
                    .map(|c| (c.1, c.2))
                    .collect();
            }
            level.first().copied()
        })
        .collect()
}

/// Dolby Vision sample entries and the standard entry each one extends
/// (Dolby Vision Streams Within the ISO Base Media File Format, §3): the
/// same `avcC`/`hvcC`/`av1C` box plus a `dvcC`/`dvvC`/`dvwC` record.
/// `dvhe`/`dvav` (like `hev1`/`avc3`) may carry parameter sets in-band.
const DOVI_ENTRIES: [([u8; 4], [u8; 4]); 5] = [
    (*b"dvh1", *b"hvc1"),
    (*b"dvhe", *b"hev1"),
    (*b"dva1", *b"avc1"),
    (*b"dvav", *b"avc3"),
    (*b"dav1", *b"av01"),
];

/// Visual sample entries that may carry a Dolby Vision record.
const DOVI_CAPABLE: [&[u8; 4]; 5] = [b"hvc1", b"hev1", b"avc1", b"avc3", b"av01"];

/// What a track's first sample entry says beyond what mp4-atom decodes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct EntryExtras {
    /// The entry's own type when it is a Dolby Vision entry (`dvhe`, …).
    dovi_fourcc: Option<[u8; 4]>,
    /// Its Dolby Vision configuration record, if any (also found in plain
    /// `hvc1` entries of profile 8 files).
    dovi: Option<DoviRecord>,
}

/// Reads the Dolby Vision record of each track's first sample entry and
/// renames Dolby Vision entries to the standard entry they extend, in place,
/// so mp4-atom decodes them (it has no `dvh1`/`dvhe`/…: they came out as
/// unknown entries, so the video was "Other" and transcoded without tone
/// mapping). Returns one [`EntryExtras`] per `trak`, in order.
fn take_dolby_vision_entries(moov: &mut [u8]) -> Vec<EntryExtras> {
    stsd_boxes(moov)
        .into_iter()
        .map(|stsd| {
            let mut x = EntryExtras::default();
            let Some((s, e)) = stsd else { return x };
            // stsd: header, version/flags, entry_count, then the entries.
            let Some((kind, es, ee)) = child_boxes(&moov[..e], s + 16).first().copied() else {
                return x;
            };
            let standard = DOVI_ENTRIES.iter().find(|(dv, _)| *dv == kind).map(|e| e.1);
            if standard.is_none() && !DOVI_CAPABLE.contains(&&kind) {
                return x;
            }
            // VisualSampleEntry: 8-byte box header and 78 bytes of fields,
            // then its boxes.
            x.dovi = child_boxes(&moov[..ee], es + 86)
                .into_iter()
                .find(|c| matches!(&c.0, b"dvcC" | b"dvvC" | b"dvwC"))
                .map(|(fourcc, s, e)| DoviRecord {
                    fourcc,
                    data: moov[s + 8..e].to_vec(),
                });
            if let Some(std) = standard {
                moov[es + 4..es + 8].copy_from_slice(&std);
                x.dovi_fourcc = Some(kind);
            }
            x
        })
        .collect()
}

/// Renames `ac-3`/`ec-3` sample entries that lack `dac3`/`dec3` (QuickTime
/// files) to placeholder types, in place; returns how many were renamed.
fn neutralize_unconfigured_dolby(moov: &mut [u8]) -> usize {
    let mut renamed = 0;
    let stsds: Vec<(usize, usize)> = stsd_boxes(moov).into_iter().flatten().collect();
    for (s, e) in stsds {
        // stsd: header, version/flags, entry_count, then the entries.
        for (k, es, ee) in child_boxes(&moov[..e], s + 16) {
            let (config, placeholder) = match &k {
                b"ac-3" => (b"dac3", UNCONFIGURED_AC3),
                b"ec-3" => (b"dec3", UNCONFIGURED_EAC3),
                _ => continue,
            };
            // AudioSampleEntry: 8-byte box header, 28 bytes of fields, plus
            // 16 (QuickTime v1) or 36 (v2) more; then child boxes. QuickTime
            // nests dac3 in a `wave` box, which mp4-atom does not look into.
            let version = moov
                .get(es + 16..es + 18)
                .map(|v| u16::from_be_bytes([v[0], v[1]]))
                .unwrap_or(0);
            let fields = 36
                + match version {
                    1 => 16,
                    2 => 36,
                    _ => 0,
                };
            let direct = child_boxes(&moov[..ee], es + fields)
                .iter()
                .any(|c| &c.0 == config);
            if !direct {
                moov[es + 4..es + 8].copy_from_slice(placeholder);
                renamed += 1;
            }
        }
    }
    renamed
}

/// Number of samples in a `stbl` (0 in a fragmented file's `moov`).
fn stbl_sample_count(stbl: &mp4_atom::Stbl) -> u32 {
    match &stbl.stsz.samples {
        StszSamples::Identical { count, .. } => *count,
        StszSamples::Different { sizes } => sizes.len() as u32,
    }
}

/// (file offset, size) of a track's first sync sample.
fn first_sync_sample(t: &Mp4Tables) -> Option<(u64, u32)> {
    let i = match &t.sync {
        None => 0,
        Some(list) => list.first()?.saturating_sub(1),
    };
    t.sample_location(i)
}

/// An `hvcC` record with no SPS, completed with the VPS/SPS/PPS found
/// in-band in `sample` (marked complete: the remux strips in-band sets).
/// Other arrays (SEI) are kept. `None` when the sample has no SPS and PPS.
fn complete_hvcc(rec: &[u8], sample: &[u8], length_size: usize) -> Option<Vec<u8>> {
    let c = hevc::HevcConfig::parse(rec)?;
    let mut sets: [(u8, Vec<&[u8]>); 3] = [(32, Vec::new()), (33, Vec::new()), (34, Vec::new())];
    for (_, nal) in crate::bits::length_prefixed_nals(sample, length_size) {
        let Some(&h) = nal.first() else { continue };
        if let Some(s) = sets.iter_mut().find(|s| s.0 == ((h >> 1) & 0x3F)) {
            if !s.1.contains(&nal) {
                s.1.push(nal);
            }
        }
    }
    if sets[1].1.is_empty() || sets[2].1.is_empty() {
        return None;
    }
    let mut arrays: Vec<(u8, bool, Vec<&[u8]>)> = sets
        .iter()
        .filter(|s| !s.1.is_empty())
        .map(|s| (s.0, true, s.1.clone()))
        .collect();
    for a in c.arrays.iter().filter(|a| !(32..=34).contains(&a.nal_type)) {
        arrays.push((
            a.nal_type,
            a.completeness,
            a.nalus.iter().map(|n| n.as_slice()).collect(),
        ));
    }
    // The 22 header bytes, then numOfArrays and the arrays (ISO/IEC 14496-15 §8.3.3.1).
    let mut out = rec.get(..22)?.to_vec();
    out.push(arrays.len() as u8);
    for (ty, complete, nalus) in arrays {
        out.push(ty | if complete { 0x80 } else { 0 });
        out.extend_from_slice(&(nalus.len() as u16).to_be_bytes());
        for n in nalus {
            out.extend_from_slice(&(n.len() as u16).to_be_bytes());
            out.extend_from_slice(n);
        }
    }
    Some(out)
}

/// (file offset, size) of a track's first sample, without expanding tables.
fn first_sample(t: &Mp4Tables) -> Option<(u64, u32)> {
    let off = *t.chunk_offsets.first()?;
    let size = t.constant_size.or_else(|| t.sizes.first().copied())?;
    Some((off, size))
}

/// A PCM sample entry: the fourcc says the layout (`sowt` = little-endian,
/// `twos` = big-endian 16-bit, `in24`, `fl32`, …), which the transcoder needs.
fn pcm_track(t: &mut IndexTrack, tag: &str, a: &mp4_atom::Audio, f: Option<mp4_atom::PcmFormat>) {
    t.codec = Codec::Pcm;
    t.codec_tag = match f {
        Some(f) => format!(
            "{tag}:{}{}{}",
            if f.float { "f" } else { "s" },
            f.sample_size,
            if f.big_endian { "be" } else { "le" }
        ),
        None => tag.to_string(),
    };
    t.bit_depth = f.map(|f| f.sample_size as u8).or(Some(a.sample_size as u8));
    t.channels = Some(a.channel_count as u8);
}

/// Sample tables of a fragmented MP4, per track ID.
struct Fragments {
    tables: std::collections::HashMap<u32, Mp4Tables>,
    fragments: usize,
}

/// Per-track samples gathered from `traf` boxes.
#[derive(Default)]
struct FragTrack {
    timescale: u32,
    first_dts: Option<u64>,
    next_dts: u64,
    durations: Vec<u32>,
    cts: Vec<i64>,
    sizes: Vec<u32>,
    /// (offset of the run's first sample, samples in the run)
    runs: Vec<(u64, u32)>,
    sync: Vec<u32>,
}

impl FragTrack {
    fn into_tables(self) -> Mp4Tables {
        let n = self.sizes.len() as u32;
        let mut stts: Vec<(u32, u32)> = Vec::new();
        for &d in &self.durations {
            match stts.last_mut() {
                Some(e) if e.1 == d => e.0 += 1,
                _ => stts.push((1, d)),
            }
        }
        let mut ctts: Vec<(u32, i64)> = Vec::new();
        if self.cts.iter().any(|&c| c != 0) {
            for &c in &self.cts {
                match ctts.last_mut() {
                    Some(e) if e.1 == c => e.0 += 1,
                    _ => ctts.push((1, c)),
                }
            }
        }
        let mut stsc: Vec<(u32, u32)> = Vec::new();
        for (i, &(_, count)) in self.runs.iter().enumerate() {
            if stsc.last().is_none_or(|e| e.1 != count) {
                stsc.push((i as u32 + 1, count));
            }
        }
        let constant = self
            .sizes
            .first()
            .copied()
            .filter(|&c| self.sizes.iter().all(|&s| s == c));
        Mp4Tables {
            timescale: self.timescale,
            stts,
            ctts,
            sizes: if constant.is_some() {
                Vec::new()
            } else {
                self.sizes
            },
            constant_size: constant,
            sample_count: n,
            stsc,
            chunk_offsets: self.runs.iter().map(|r| r.0).collect(),
            sync: (self.sync.len() as u32 != n).then_some(self.sync),
            // Expanded decode times start at 0; presentation shifts by the
            // first fragment's decode time.
            edit_shift: -(self.first_dts.unwrap_or(0) as i64),
        }
    }
}

/// Walks the top-level boxes after `moov` and decodes every `moof`.
fn scan_fragments(
    src: &Source,
    from: u64,
    tracks: &[(u32, u32)],
    trex: &[(u32, (u32, u32, u32))],
) -> Result<Fragments> {
    use mp4_atom::Moof;
    let mut per: std::collections::HashMap<u32, FragTrack> = tracks
        .iter()
        .map(|&(id, ts)| {
            (
                id,
                FragTrack {
                    timescale: ts,
                    ..Default::default()
                },
            )
        })
        .collect();
    let len = src.len();
    let mut pos = from;
    let mut fragments = 0usize;
    while pos + 8 <= len {
        // One read covers the box header and, usually, the whole moof.
        let head = src.read_at(pos, 64 << 10)?;
        if head.len() < 8 {
            break;
        }
        let mut size = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as u64;
        let kind = [head[4], head[5], head[6], head[7]];
        let mut hdr = 8;
        let to_eof = size == 0;
        if size == 1 && head.len() >= 16 {
            size = u64::from_be_bytes([
                head[8], head[9], head[10], head[11], head[12], head[13], head[14], head[15],
            ]);
            hdr = 16;
        } else if to_eof {
            size = len - pos;
        }
        if size < hdr {
            return Err(HlsError::mp4(format!("bad box size at {pos}")));
        }
        if &kind == b"moof" && (to_eof || size > MAX_MOOF) {
            // A corrupt size (0 = "to the end of the file", or huge) would
            // read the rest of the file into memory (ROB-V2).
            return Err(HlsError::mp4(format!("implausible moof size {size} at {pos}")));
        }
        if &kind == b"moof" {
            let bytes = if size as usize <= head.len() {
                head[..size as usize].to_vec()
            } else {
                src.read_at(pos, size as usize)?
            };
            let moof = Moof::decode(&mut bytes.as_slice())
                .map_err(|e| HlsError::mp4(format!("moof at {pos}: {e}")))?;
            fragments += 1;
            for traf in &moof.traf {
                let id = traf.tfhd.track_id;
                let Some(t) = per.get_mut(&id) else { continue };
                let defaults = trex.iter().find(|x| x.0 == id).map(|x| x.1).unwrap_or((0, 0, 0));
                let base = traf.tfhd.base_data_offset.unwrap_or(pos);
                if let Some(tfdt) = &traf.tfdt {
                    let b = tfdt.base_media_decode_time;
                    match t.first_dts {
                        None => t.first_dts = Some(b),
                        Some(_) if b != t.next_dts => {
                            // A gap/overlap between fragments: stretch (or
                            // shrink) the previous sample so decode times stay exact.
                            if let Some(d) = t.durations.last_mut() {
                                *d = (*d as i64 + b as i64 - t.next_dts as i64).max(0) as u32;
                            }
                        }
                        Some(_) => {}
                    }
                    t.next_dts = b;
                } else if t.first_dts.is_none() {
                    t.first_dts = Some(0);
                }
                let mut data = base;
                for trun in &traf.trun {
                    if let Some(off) = trun.data_offset {
                        data = (base as i64 + off as i64) as u64;
                    }
                    if trun.entries.is_empty() {
                        continue;
                    }
                    t.runs.push((data, trun.entries.len() as u32));
                    if t.sizes.len() + trun.entries.len() > MAX_SAMPLES as usize {
                        return Err(HlsError::mp4(format!(
                            "implausible fragmented track: more than {MAX_SAMPLES} samples"
                        )));
                    }
                    for e in &trun.entries {
                        let dur = e
                            .duration
                            .or(traf.tfhd.default_sample_duration)
                            .unwrap_or(defaults.0);
                        let size = e.size.or(traf.tfhd.default_sample_size).unwrap_or(defaults.1);
                        let flags = e.flags.or(traf.tfhd.default_sample_flags).unwrap_or(defaults.2);
                        t.durations.push(dur);
                        t.cts.push(e.cts.unwrap_or(0) as i64);
                        t.sizes.push(size);
                        if flags & 0x0001_0000 == 0 {
                            t.sync.push(t.sizes.len() as u32);
                        }
                        t.next_dts += dur as u64;
                        data += size as u64;
                    }
                }
            }
        }
        pos += size;
    }
    Ok(Fragments {
        tables: per.into_iter().map(|(id, t)| (id, t.into_tables())).collect(),
        fragments,
    })
}

fn vui_to_color(c: codec::VuiColor) -> ColorInfo {
    ColorInfo {
        primaries: Some(c.primaries).filter(|&v| v != 2),
        transfer: Some(c.transfer).filter(|&v| v != 2),
        matrix: Some(c.matrix).filter(|&v| v != 2),
        full_range: Some(c.full_range),
    }
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 {
        a.max(1)
    } else {
        gcd(b, a % b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fmp4::{self, AudioConfig, EntrySpec, Fragment, InitSpec, Sample, SampleData};

    /// `hvcC` of HEVC Main 10, level 5.0, 4-byte NAL lengths, no arrays.
    const HVCC_MAIN10: [u8; 23] = [
        1, 0x02, 0x20, 0, 0, 0, 0xB0, 0, 0, 0, 0, 0, 150, 0xF0, 0, 0xFC, 0xFD, 0xFA, 0xFA, 0, 0, 0x0F, 0,
    ];
    /// `dvcC` of Dolby Vision profile 5, level 6, RPU + base layer, no
    /// compatible base layer (bl_signal_compatibility_id 0).
    const DVCC_P5: [u8; 24] = {
        let mut d = [0u8; 24];
        d[0] = 1; // version 1.0
        d[2] = 5 << 1; // profile 5, level's top bit 0
        d[3] = (6 << 3) | 0b101; // level 6, rpu, no el, bl
        d
    };
    /// `dac3`: 48 kHz, bsid 8, acmod 7 (3/2), LFE on, 448 kb/s.
    const DAC3_51: [u8; 3] = [0x10, 0x3D, 0xC0];

    /// A fragmented MP4 (init segment + one media segment) whose video
    /// entry is `video` with an optional `dvcC`, plus one audio entry.
    fn synthetic_mp4(video: &[u8; 4], dovi: Option<&[u8]>, audio: EntrySpec) -> std::path::PathBuf {
        let init = fmp4::init_segment(&[
            InitSpec {
                track_id: 1,
                timescale: 24_000,
                language: 0x55C4,
                entry: EntrySpec::Video {
                    fourcc: *video,
                    config_box: *b"hvcC",
                    config: HVCC_MAIN10.to_vec(),
                    width: 3832,
                    height: 1592,
                    color: None,
                    mastering: None,
                    cll: None,
                    dovi: dovi.map(|d| DoviRecord {
                        fourcc: *b"dvcC",
                        data: d.to_vec(),
                    }),
                    pasp: None,
                    matrix: None,
                },
            },
            InitSpec {
                track_id: 2,
                timescale: 48_000,
                language: 0x55C4,
                entry: audio,
            },
        ]);
        // Length-prefixed NAL units: an IDR, then trailing pictures. With a
        // `hev1`/`dvhe` entry the parameter sets are only in-band (the
        // `hvcC` has none), before the IDR.
        let nal = |t: u8, len: usize| {
            let mut v = vec![0, 0, 0, len as u8, t << 1, 1];
            v.extend(std::iter::repeat_n(0x55u8, len - 2));
            v
        };
        let inband = matches!(video, b"hev1" | b"dvhe");
        let video_samples = (0..4)
            .map(|i| {
                let mut data = Vec::new();
                if i == 0 && inband {
                    for ps in [32, 33, 34] {
                        data.extend(nal(ps, 8));
                    }
                }
                data.extend(nal(if i == 0 { 19 } else { 1 }, 20));
                Sample {
                    duration: 1001,
                    cts: 0,
                    sync: i == 0,
                    data: SampleData::Bytes(data.into()),
                }
            })
            .collect();
        let audio_samples = (0..4)
            .map(|_| Sample {
                duration: 1536,
                cts: 0,
                sync: true,
                data: SampleData::Bytes(vec![0x0B, 0x77, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0].into()),
            })
            .collect();
        let seg = fmp4::media_segment(
            1,
            &[
                Fragment {
                    track_id: 1,
                    base_dts: 0,
                    samples: video_samples,
                    video: true,
                },
                Fragment {
                    track_id: 2,
                    base_dts: 0,
                    samples: audio_samples,
                    video: false,
                },
            ],
        );
        let mut file = init;
        file.extend(seg.read_in_memory().expect("in-memory segment"));
        let path = flux_test_media::scratch("mp4-entries").join("clip.mp4");
        std::fs::write(&path, file).unwrap();
        path
    }

    fn ac3_51() -> EntrySpec {
        EntrySpec::Audio {
            fourcc: *b"ac-3",
            // As written by muxers: the real layout is in dac3.
            channels: 2,
            sample_rate: 48_000,
            config: AudioConfig::Dac3(DAC3_51.to_vec()),
        }
    }

    /// Dolby Vision profile 5 in a `dvhe` entry (the bug: "Other" video,
    /// transcoded without tone mapping at a stretched 1920×1080) and AC-3
    /// 5.1 whose entry says 2 channels (the bug: "AC-3 Stereo").
    #[test]
    fn dolby_vision_entries_index_as_hevc_with_their_record() {
        for (fourcc, inband) in [(b"dvhe", true), (b"dvh1", false)] {
            let path = synthetic_mp4(fourcc, Some(&DVCC_P5[..]), ac3_51());
            let idx = crate::index::PlaybackIndex::build(&path).unwrap();
            let v = &idx.tracks[0];
            let tag = std::str::from_utf8(fourcc).unwrap();
            assert_eq!((v.codec, v.codec_tag.as_str()), (Codec::Hevc, tag));
            assert_eq!(
                (v.width, v.height, v.bit_depth),
                (Some(3832), Some(1592), Some(10))
            );
            assert_eq!(v.inband_parameter_sets, inband, "{tag}");
            let hdr = v.hdr.as_ref().unwrap();
            assert_eq!(hdr.format, flux_core::HdrFormat::DolbyVision);
            let d = hdr.dovi.unwrap();
            assert_eq!((d.profile, d.level, d.bl_signal_compat_id), (5, 6, 0));
            assert!(d.rpu_present && d.bl_present && !d.el_present);
            assert_eq!(v.dovi.as_ref().unwrap().data, DVCC_P5);
            assert_eq!(idx.tracks[1].channels, Some(6));
            assert!(
                !idx.warnings.iter().any(|w| w.contains("unsupported")),
                "{:?}",
                idx.warnings
            );

            // The remux writes `dvh1` with the record, and CODECS says so.
            // A `dvhe` record without parameter sets is completed from the
            // first keyframe (the remux strips the in-band ones).
            let asset = crate::asset::Asset::open(&path, 6_000_000_000).unwrap();
            let EntrySpec::Video {
                fourcc, dovi, config, ..
            } = asset.video_entry().unwrap()
            else {
                panic!("not video");
            };
            assert_eq!((&fourcc, dovi.is_some()), (b"dvh1", true));
            let hvcc = hevc::HevcConfig::parse(&config).unwrap();
            let sets: Vec<(u8, bool, usize)> = hvcc
                .arrays
                .iter()
                .map(|a| (a.nal_type, a.completeness, a.nalus.len()))
                .collect();
            if inband {
                assert_eq!(sets, [(32, true, 1), (33, true, 1), (34, true, 1)]);
                assert_eq!(&config[..22], &HVCC_MAIN10[..22]);
                assert!(idx
                    .warnings
                    .iter()
                    .any(|w| w.contains("hvcC without parameter sets")));
            } else {
                assert!(sets.is_empty());
            }
            assert_eq!(
                crate::playlist::video_codec_string(&asset).as_deref(),
                Some("dvh1.05.06")
            );
            // Safari (Dolby Vision 5): `dvhe` is remuxed (no transcode), as PQ.
            let probe = asset.index.to_probe_result();
            let plan = crate::planner::plan(&probe, &crate::presets::safari_macos(), &Default::default());
            assert!(matches!(plan.video, flux_core::VideoOp::Copy { .. }), "{plan:?}");
            assert_eq!(plan.is_direct(), !inband, "{tag}");
            if inband {
                let m = crate::playlist::master_playlist(&asset, &plan);
                assert!(m.contains("CODECS=\"dvh1.05.06,ac-3\""), "{m}");
                assert!(
                    m.contains("VIDEO-RANGE=PQ") && !m.contains("VIDEO-RANGE=SDR"),
                    "{m}"
                );
                assert!(m.contains("NAME=\"Unknown (AC-3 5.1)\""), "{m}");
            }
        }
    }

    /// A `hev1` entry remuxes to `hvc1` (parameter sets stripped), and the
    /// `CODECS` entry names the entry written, not the source's.
    #[test]
    fn hev1_codec_string_follows_the_written_entry() {
        let path = synthetic_mp4(b"hev1", None, ac3_51());
        let mut asset = crate::asset::Asset::open(&path, 6_000_000_000).unwrap();
        let v = asset.index.video_track().unwrap();
        assert_eq!(v.codec_string.as_deref(), Some("hev1.2.4.L150.B0"));
        assert!(v.dovi.is_none());
        assert_eq!(v.hdr.as_ref().unwrap().format, flux_core::HdrFormat::Sdr);
        assert_eq!(
            crate::playlist::video_codec_string(&asset).as_deref(),
            Some("hvc1.2.4.L150.B0")
        );
        asset.strip_parameter_sets = false;
        assert_eq!(
            crate::playlist::video_codec_string(&asset).as_deref(),
            Some("hev1.2.4.L150.B0")
        );
    }

    /// E-AC-3 channels come from `dec3` (5.1 here), not the entry's 2.
    #[test]
    fn eac3_channels_come_from_dec3() {
        let f = audio::Ac3Frame {
            eac3: true,
            fscod: 0,
            bsid: 16,
            bsmod: 0,
            acmod: 7,
            lfeon: true,
            bit_rate_code: 0,
            frame_size: 1792,
            sample_rate: 48_000,
            samples: 1536,
            strmtyp: 0,
            chanmap: None,
        };
        let entry = EntrySpec::Audio {
            fourcc: *b"ec-3",
            channels: 2,
            sample_rate: 48_000,
            config: AudioConfig::Dec3(audio::dec3(&[f]).unwrap()),
        };
        let idx = crate::index::PlaybackIndex::build(synthetic_mp4(b"hvc1", None, entry)).unwrap();
        assert_eq!(
            (idx.tracks[1].codec, idx.tracks[1].channels),
            (Codec::Eac3, Some(6))
        );
        assert_eq!(audio::eac3_chan_loc_channels(0b10), 2); // Lrs/Rrs: 7.1
        assert_eq!(audio::eac3_chan_loc_channels(0x1FF), 14);
    }

    fn aac_stereo() -> EntrySpec {
        EntrySpec::Audio {
            fourcc: *b"mp4a",
            channels: 2,
            sample_rate: 48_000,
            config: AudioConfig::Esds {
                object_type: 0x40,
                dsi: vec![0x11, 0x90],
                avg_bitrate: 128_000,
            },
        }
    }

    /// `dvcC` of profile 8.1: an HDR10 base layer (bl_signal_compatibility_id 1).
    const DVCC_P81: [u8; 24] = {
        let mut d = DVCC_P5;
        d[2] = 8 << 1;
        d[4] = 1 << 4;
        d
    };

    /// Profile 8.1 in a `dvh1`/`dvhe` entry, for a browser without Dolby
    /// Vision: not the file (players without platform DV may not open these
    /// entries) but a remux to `hvc1` with the record, as HDR10.
    #[test]
    fn dolby_vision_profile_8_entries_remux_to_hvc1_without_dv() {
        for fourcc in [b"dvh1", b"dvhe"] {
            let tag = std::str::from_utf8(fourcc).unwrap();
            let path = synthetic_mp4(fourcc, Some(&DVCC_P81[..]), aac_stereo());
            let asset = crate::asset::Asset::open(&path, 6_000_000_000).unwrap();
            let v = asset.index.video_track().unwrap();
            assert_eq!((v.codec, v.codec_tag.as_str()), (Codec::Hevc, tag));
            let d = v.hdr.as_ref().and_then(|h| h.dovi).unwrap();
            assert_eq!((d.profile, d.bl_signal_compat_id), (8, 1));
            let probe = asset.index.to_probe_result();
            let plan = crate::planner::plan(&probe, &crate::presets::chrome_desktop(), &Default::default());
            assert_eq!(
                plan.delivery,
                flux_core::DeliveryFormat::HlsFmp4,
                "{tag}: {plan:?}"
            );
            assert!(matches!(plan.video, flux_core::VideoOp::Copy { .. }), "{plan:?}");
            let EntrySpec::Video {
                fourcc: written,
                dovi,
                ..
            } = asset.video_entry().unwrap()
            else {
                panic!("not video");
            };
            assert_eq!((&written, dovi.is_some()), (b"hvc1", true), "{tag}");
            let m = crate::playlist::master_playlist(&asset, &plan);
            assert!(m.contains("CODECS=\"hvc1.2.4.L150.B0,"), "{m}");
            assert!(m.contains("VIDEO-RANGE=PQ"), "{m}");
        }
    }

    #[test]
    fn expand_tables() {
        let t = Mp4Tables {
            timescale: 1000,
            stts: vec![(4, 40)],
            ctts: vec![(1, 80), (1, 120), (2, 40)],
            sizes: vec![10, 20, 30, 40],
            constant_size: None,
            sample_count: 4,
            stsc: vec![(1, 3), (2, 1)],
            chunk_offsets: vec![1000, 5000],
            sync: Some(vec![1]),
            edit_shift: 80,
        };
        let s = t.expand();
        assert_eq!(s.dts, vec![0, 40, 80, 120]);
        assert_eq!(s.offset, vec![1000, 1010, 1030, 5000]);
        assert_eq!(s.sync, vec![true, false, false, false]);
        assert_eq!(s.pts(0, &t), 0);
        assert_eq!(s.pts(1, &t), 80);
        // The non-allocating views agree with the expansion.
        let timing: Vec<(i64, i64, u32)> = t.timing().collect();
        assert_eq!(timing.iter().map(|x| x.0).collect::<Vec<_>>(), s.dts);
        assert_eq!(timing.iter().map(|x| x.1).collect::<Vec<_>>(), s.cts);
        for i in 0..4 {
            assert_eq!(
                t.sample_location(i),
                Some((s.offset[i as usize], s.size[i as usize]))
            );
        }
        assert_eq!(t.sample_location(4), None);
        assert_eq!(t.bytes_of(1, 3), 50);
        let mut cursor = 0;
        let sync: Vec<bool> = (0..4).map(|i| t.is_sync(i, &mut cursor)).collect();
        assert_eq!(sync, s.sync);
    }

    /// ROB-2: a 1.6 MB file whose `stsz` claims 60 M identical samples (or a
    /// `ctts` run of u32::MAX) must fail cleanly or stay bounded, not
    /// allocate ~29 bytes per claimed sample.
    #[test]
    fn sample_table_bombs_are_rejected() {
        let bomb = Mp4Tables {
            timescale: 24_000,
            stts: vec![(60_000_000, 1001)],
            ctts: vec![],
            sizes: vec![],
            constant_size: Some(100),
            sample_count: 60_000_000,
            stsc: vec![(1, 60_000_000)],
            chunk_offsets: vec![48],
            sync: None,
            edit_shift: 0,
        };
        let err = bomb.check(1_600_000, false).unwrap_err().to_string();
        assert!(err.contains("implausible"), "{err}");
        // One byte per sample, as many as the file has bytes (reverify): a
        // segment of them cost ~140 bytes of memory per sample per request.
        let one_byte = Mp4Tables {
            constant_size: Some(1),
            sample_count: 1_638_936,
            ..bomb.clone()
        };
        assert!(one_byte.check(1_638_936, false).is_err());
        // A real track: 2 h at 60 fps in a 300 MB file.
        let real = Mp4Tables {
            constant_size: None,
            sample_count: 432_000,
            ..bomb.clone()
        };
        assert!(real.check(300 << 20, false).is_ok());
        // Within the file's size but above the per-track cap.
        let tiny = Mp4Tables {
            constant_size: Some(1),
            sample_count: MAX_SAMPLES + 1,
            ..bomb.clone()
        };
        assert!(tiny.check(u64::MAX, false).is_err());
        // PCM is expanded per chunk: only the size bound applies.
        assert!(tiny.check(u64::MAX, true).is_ok());
        // A ctts run of u32::MAX entries costs what the samples need, not 4 G iterations.
        let ctts = Mp4Tables {
            stts: vec![(4, 1001)],
            ctts: vec![(u32::MAX, 2002), (u32::MAX, 0)],
            constant_size: Some(10),
            sample_count: 4,
            stsc: vec![(1, 4)],
            ..bomb
        };
        let t0 = std::time::Instant::now();
        let s = ctts.expand();
        assert_eq!(s.cts, vec![2002; 4]);
        assert!(t0.elapsed() < std::time::Duration::from_millis(100));
        assert_eq!(ctts.timing().count(), 4);
    }
}
