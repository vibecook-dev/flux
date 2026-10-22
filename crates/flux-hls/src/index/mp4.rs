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

/// Box types [`normalize_moov`] descends into: the plain containers on the
/// way to the sample tables (their bodies are child boxes and nothing else).
const MOOV_CONTAINERS: [&[u8; 4]; 7] = [b"trak", b"mdia", b"minf", b"stbl", b"edts", b"dinf", b"mvex"];

/// Visual sample entries [`stsd`] fixes the boxes of (Dolby Vision's entries
/// extend `avc1`/`avc3`/`hvc1`/`hev1`/`av01`, and are only renamed later).
const VISUAL_ENTRIES: [&[u8; 4]; 13] = [
    b"avc1", b"avc3", b"dva1", b"dvav", b"hvc1", b"hev1", b"dvh1", b"dvhe", b"av01", b"dav1", b"vp08",
    b"vp09", b"jpeg",
];

/// Longest text (an `hdlr` name, a data reference) [`text_box`] copies:
/// mp4-atom reads each into a `String`, and the index reads none of them.
const MAX_TEXT: usize = 256;

/// What [`normalize_moov`] changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MoovRepairs {
    /// `udta` and `meta` boxes left out.
    pub metadata: usize,
    /// Text fields (`hdlr` names, data references) that were not UTF-8.
    pub text: usize,
    /// Bytes left out after the last whole child of a container: a child
    /// that overruns its parent (or a header that can't be one) and all
    /// that follows it.
    pub junk: u64,
    /// Sample tables, header boxes (`tkhd`, `mdhd`, …), `avcC` records and
    /// fixed-size sample entry boxes (`pasp`, …) with bytes past their
    /// entries or fields.
    pub trimmed: usize,
    /// `esds` boxes rebuilt: descriptors that overran their parent, bytes
    /// past them, or optional fields mp4-atom doesn't read.
    pub esds: usize,
}

impl MoovRepairs {
    /// Index warnings for anything beyond dropping metadata (which every
    /// file with a title has, so it is not worth a warning).
    pub fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        let s = |n: usize| if n == 1 { "" } else { "s" };
        if self.text > 0 {
            w.push(format!(
                "moov: {} text field{} not UTF-8 (replaced)",
                self.text,
                s(self.text)
            ));
        }
        if self.junk > 0 {
            w.push(format!(
                "moov: {} bytes after the last whole box of a container ignored (a box overran its parent)",
                self.junk
            ));
        }
        if self.esds > 0 {
            w.push(format!(
                "moov: {} esds box{} rebuilt from {} AudioSpecificConfig",
                self.esds,
                if self.esds == 1 { "" } else { "es" },
                if self.esds == 1 { "its" } else { "their" }
            ));
        }
        if self.trimmed > 0 {
            w.push(format!(
                "moov: {} box{} with bytes past {} entries or fields (ignored)",
                self.trimmed,
                if self.trimmed == 1 { "" } else { "es" },
                if self.trimmed == 1 { "its" } else { "their" }
            ));
        }
        w
    }
}

/// A copy of the `moov` box `moov` (header included) that mp4-atom decodes
/// whatever text and junk the file carries, as ffmpeg reads it:
/// - `udta` and `meta` boxes are left out. They hold titles, iTunes `ilst`
///   items and chapters as free text in any code page (non-UTF-8 text failed
///   the whole `moov`); the index needs none of it (flux-probe reads titles).
/// - Track `hdlr` names and data references that are not UTF-8 have their
///   other bytes replaced by `?` (QuickTime writes Pascal strings in the
///   system code page, and `alis` records in binary), and any longer than
///   [`MAX_TEXT`] bytes are cut there: nothing reads them, and a copy may
///   not cost more than the box.
/// - A child box that overruns its parent, or a remainder too short to be
///   a box, ends the container: it and anything after it are left out
///   (ffmpeg clamps such a box to its parent and skips it as unknown).
/// - Sample tables (`stts`, `stsz`, …), header boxes (`mvhd`, `tkhd`,
///   `mdhd`, `vmhd`, `smhd`, …), `avcC` records and the fixed-size boxes of
///   sample entries (`pasp`, `btrt`, `fiel`, an `nclx` `colr`) end after
///   their last entry or field: some muxers pad them, and mp4-atom fails a
///   box with bytes it did not read (ffmpeg skips them). An `esds` whose
///   descriptors overrun (or trail bytes) is rebuilt from its decoder
///   configuration.
///
/// Containers and the boxes fixed are rewritten with 32-bit sizes; every
/// other box is copied as is, so the copy is no larger than `moov` but for
/// a rebuilt `esds`'s few bytes. Sample tables hold absolute file offsets,
/// so nothing else moves.
pub(crate) fn normalize_moov(moov: &[u8], repairs: &mut MoovRepairs) -> Vec<u8> {
    let mut out = Vec::with_capacity(moov.len());
    match box_extent(moov) {
        Some((_, header)) => {
            let body = &moov[header..];
            container(*b"moov", body, 0, repairs, &mut out);
        }
        None => out.extend_from_slice(moov),
    }
    out
}

/// (size, header length) of the box at the start of `b`, the size clamped
/// to nothing past `b`. `None` when fewer than 8 bytes remain, or the size is
/// below the header's or runs past `b`.
fn box_extent(b: &[u8]) -> Option<(usize, usize)> {
    let size = u32::from_be_bytes(b.get(..4)?.try_into().ok()?) as u64;
    b.get(4..8)?;
    let (size, header) = match size {
        0 => (b.len() as u64, 8),
        1 => (u64::from_be_bytes(b.get(8..16)?.try_into().ok()?), 16),
        s => (s, 8),
    };
    (size >= header as u64 && size <= b.len() as u64).then_some((size as usize, header))
}

/// Writes `kind` with `body` (a 32-bit header).
fn put_box(kind: &[u8], body: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
}

/// Starts a box whose size [`end_box`] fills in; returns where it starts.
fn begin_box(kind: &[u8], out: &mut Vec<u8>) -> usize {
    let start = out.len();
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(kind);
    start
}

fn end_box(start: usize, out: &mut [u8]) {
    let size = (out.len() - start) as u32;
    out[start..start + 4].copy_from_slice(&size.to_be_bytes());
}

/// Writes container `kind` with the normalized children of `body`.
fn container(kind: [u8; 4], body: &[u8], depth: u32, r: &mut MoovRepairs, out: &mut Vec<u8>) {
    let start = begin_box(&kind, out);
    let mut p = 0usize;
    while p < body.len() {
        let rest = &body[p..];
        let Some((size, header)) = box_extent(rest) else {
            // QuickTime may end a container with a 32-bit zero terminator.
            if rest.iter().any(|&b| b != 0) {
                r.junk += rest.len() as u64;
            }
            break;
        };
        let child = &rest[..size];
        let ck: [u8; 4] = child[4..8].try_into().expect("8-byte header");
        let cb = &child[header..];
        match &ck {
            b"udta" | b"meta" => r.metadata += 1,
            k if depth < 8 && MOOV_CONTAINERS.contains(&k) => container(ck, cb, depth + 1, r, out),
            // hdlr: version/flags, pre_defined, handler_type, 12 reserved
            // bytes, name. Only a track's are read (one elsewhere is copied).
            b"hdlr" if matches!(&kind, b"mdia" | b"minf") => text_box(&ck, cb, 24, r, out),
            b"dref" => dref(cb, r, out),
            b"stsd" => stsd(cb, r, out),
            k => match body_len(k, cb).filter(|&n| n < cb.len()) {
                Some(n) => {
                    r.trimmed += 1;
                    put_box(k, &cb[..n], out);
                }
                None => out.extend_from_slice(child),
            },
        }
        p += size;
    }
    end_box(start, out);
}

/// Bytes the body `b` of a `kind` box (version and flags included) needs:
/// a sample table's for the entries it declares, a header box's for the
/// fields of its version. `None` for other boxes and unknown versions.
fn body_len(kind: &[u8; 4], b: &[u8]) -> Option<usize> {
    let be32 = |at: usize| {
        b.get(at..at + 4)
            .map(|x| u32::from_be_bytes([x[0], x[1], x[2], x[3]]) as u64)
    };
    let version = |v0: u64, v1: u64| match b.first()? {
        0 => Some(v0),
        1 => Some(v1),
        _ => None,
    };
    let need = match kind {
        // Header boxes of fixed fields (ISO/IEC 14496-12), with 32- or
        // 64-bit times by version.
        b"mvhd" => version(100, 112)?,
        b"tkhd" => version(84, 96)?,
        b"mdhd" => version(24, 36)?,
        b"vmhd" => 12,
        b"smhd" => 8,
        b"hmhd" => 20,
        b"nmhd" | b"sthd" => 4,
        _ => {
            let n = be32(4)?;
            match kind {
                b"stts" | b"ctts" | b"co64" => 8 + 8 * n,
                b"stss" | b"stco" => 8 + 4 * n,
                b"stsc" => 8 + 12 * n,
                // stsz: sample_size, then sample_count and (if sizes vary)
                // the sizes.
                b"stsz" => 12 + if n == 0 { 4 * be32(8)? } else { 0 },
                b"elst" => 8 + n * if b[0] == 1 { 20 } else { 12 },
                _ => return None,
            }
        }
    };
    usize::try_from(need).ok()
}

/// Bytes the body `b` of a fixed-size box `kind` of a sample entry needs.
fn entry_box_len(kind: &[u8], b: &[u8]) -> Option<usize> {
    Some(match kind {
        b"pasp" => 8,
        b"btrt" => 12,
        b"fiel" => 2,
        // colour_type, then primaries, transfer and matrix (and nclx's
        // full-range byte); an ICC profile takes the rest of the box.
        b"colr" => match b.get(..4)? {
            b"nclx" => 11,
            b"nclc" => 10,
            _ => return None,
        },
        _ => return None,
    })
}

/// Length of an `avcC` record (ISO/IEC 14496-15 §5.3.3.1): the SPS and PPS
/// arrays, and the High-profile extension when it is whole. `None` when
/// the arrays don't fit.
fn avcc_len(b: &[u8]) -> Option<usize> {
    fn units(b: &[u8], mut p: usize, n: usize) -> Option<usize> {
        for _ in 0..n {
            let len = u16::from_be_bytes([*b.get(p)?, *b.get(p + 1)?]) as usize;
            p += 2 + len;
        }
        (p <= b.len()).then_some(p)
    }
    let p = units(b, 6, (*b.get(5)? & 0x1F) as usize)?;
    let p = units(b, p + 1, *b.get(p)? as usize)?;
    // What follows is the extension (mp4-atom reads it whenever bytes
    // remain): keep it when whole, drop it when it can't be one.
    match b.get(p + 3) {
        Some(&n) => Some(units(b, p + 4, n as usize).unwrap_or(p)),
        None => Some(p),
    }
}

/// Bytes of fields before the child boxes of a sample entry `e` (header
/// `h` bytes long) whose children [`stsd`] fixes, if it is one.
fn entry_fields(e: &[u8], h: usize) -> Option<usize> {
    let kind = &e[4..8];
    if VISUAL_ENTRIES.iter().any(|k| &k[..] == kind) {
        // VisualSampleEntry.
        return Some(78);
    }
    // AudioSampleEntry, and QuickTime's sound description versions 1 and 2.
    (kind == b"mp4a").then(|| {
        28 + match e.get(h + 8..h + 10) {
            Some([0, 1]) => 16,
            Some([0, 2]) => 36,
            _ => 0,
        }
    })
}

/// Copies an `stsd` with the `avcC` records and fixed-size boxes (`pasp`,
/// …) of its visual and `mp4a` entries ending after their last field, and
/// the `esds` of its `mp4a` entries rebuilt when mp4-atom can't read them.
fn stsd(b: &[u8], r: &mut MoovRepairs, out: &mut Vec<u8>) {
    let start = begin_box(b"stsd", out);
    out.extend_from_slice(&b[..b.len().min(8)]);
    let mut p = 8;
    while p < b.len() {
        let Some((size, h)) = box_extent(&b[p..]) else {
            out.extend_from_slice(&b[p..]);
            break;
        };
        let e = &b[p..p + size];
        match entry_fields(e, h).filter(|&f| e.len() >= h + f) {
            Some(f) => {
                let entry = begin_box(&e[4..8], out);
                out.extend_from_slice(&e[h..h + f]);
                let mut q = h + f;
                while let Some((cs, ch)) = box_extent(&e[q..]) {
                    let c = &e[q..q + cs];
                    let body = &c[ch..];
                    match &c[4..8] {
                        b"avcC" => match avcc_len(body).filter(|&n| n < body.len()) {
                            Some(n) => {
                                r.trimmed += 1;
                                put_box(b"avcC", &body[..n], out);
                            }
                            None => out.extend_from_slice(c),
                        },
                        b"esds" => match repair_esds(body) {
                            Some(fixed) => {
                                r.esds += 1;
                                put_box(b"esds", &fixed, out);
                            }
                            None => out.extend_from_slice(c),
                        },
                        k => match entry_box_len(k, body).filter(|&n| n < body.len()) {
                            Some(n) => {
                                r.trimmed += 1;
                                put_box(k, &body[..n], out);
                            }
                            None => out.extend_from_slice(c),
                        },
                    }
                    q += cs;
                }
                out.extend_from_slice(&e[q..]);
                end_box(entry, out);
            }
            None => out.extend_from_slice(e),
        }
        p += size;
    }
    end_box(start, out);
}

/// The MPEG-4 descriptor at `p` of `b` (ISO/IEC 14496-1 §8.3.3): its tag,
/// body start and body end, the end clamped to `b`. `None` when its header
/// runs past `b`.
fn descriptor(b: &[u8], p: usize) -> Option<(u8, usize, usize)> {
    let tag = *b.get(p)?;
    let (mut len, mut q) = (0usize, p + 1);
    for _ in 0..4 {
        let x = *b.get(q)?;
        q += 1;
        len = (len << 7) | (x & 0x7F) as usize;
        if x & 0x80 == 0 {
            break;
        }
    }
    Some((tag, q, (q + len).min(b.len())))
}

/// The first descriptor tagged `tag` among those in `b[from..]`, read as far
/// as their headers hold.
fn find_descriptor(b: &[u8], mut from: usize, tag: u8) -> Option<(usize, usize)> {
    while from < b.len() {
        let (t, s, e) = descriptor(b, from)?;
        if t == tag {
            return Some((s, e));
        }
        from = e;
    }
    None
}

fn put_descriptor(tag: u8, body: &[u8], out: &mut Vec<u8>) {
    out.push(tag);
    let n = body.len();
    if n < 0x80 {
        out.push(n as u8);
    } else {
        out.extend_from_slice(&[
            0x80 | ((n >> 21) as u8 & 0x7F),
            0x80 | ((n >> 14) as u8 & 0x7F),
            0x80 | ((n >> 7) as u8 & 0x7F),
            n as u8 & 0x7F,
        ]);
    }
    out.extend_from_slice(body);
}

/// An `esds` body (version and flags, then an ES_Descriptor) that mp4-atom
/// can't read, rebuilt as one it can: the ES_ID, the
/// DecoderConfigDescriptor's fields and AudioSpecificConfig, and a
/// predefined SLConfigDescriptor. Descriptors are read leniently (lengths
/// clamped to their parent, as ffmpeg reads them): muxers write lengths that
/// overrun (a DecoderConfigDescriptor swallowing half the SLConfig's
/// header), trailing bytes, or optional ES fields mp4-atom doesn't read.
/// `None` when the box reads as is, or has no DecoderConfigDescriptor.
fn repair_esds(b: &[u8]) -> Option<Vec<u8>> {
    let mut boxed = Vec::new();
    put_box(b"esds", b, &mut boxed);
    if mp4_atom::Esds::decode(&mut boxed.as_slice()).is_ok() {
        return None;
    }
    let (s, e) = find_descriptor(b, 4, 3).filter(|&(s, e)| e >= s + 3)?;
    let es = &b[..e];
    let flags = es[s + 2];
    let mut p = s + 3;
    if flags & 0x80 != 0 {
        p += 2; // dependsOn_ES_ID
    }
    if flags & 0x40 != 0 {
        p += 1 + *es.get(p)? as usize; // URL
    }
    if flags & 0x20 != 0 {
        p += 2; // OCR_ES_Id
    }
    let (ds, de) = find_descriptor(es, p, 4)?;
    let mut dc = es.get(ds..ds + 13)?.to_vec();
    if let Some((xs, xe)) = find_descriptor(&es[..de], ds + 13, 5) {
        put_descriptor(5, &es[xs..xe], &mut dc);
    }
    let mut body = vec![es[s], es[s + 1], 0];
    put_descriptor(4, &dc, &mut body);
    put_descriptor(6, &[2], &mut body);
    let mut out = b[..4].to_vec();
    put_descriptor(3, &body, &mut out);
    Some(out)
}

/// Writes `kind` with `body`, whose NUL-terminated string starts `fields`
/// bytes in. A string that is not UTF-8 or is longer than [`MAX_TEXT`]
/// bytes is written as its first [`MAX_TEXT`] bytes, each non-ASCII one as
/// `?`: no larger than it was (a lossy decode tripled a legacy code page's
/// bytes, and a box full of them cost ~8× the `moov` in copies).
fn text_box(kind: &[u8], body: &[u8], fields: usize, r: &mut MoovRepairs, out: &mut Vec<u8>) {
    let s = body.get(fields..).unwrap_or_default();
    let s = &s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())];
    let utf8 = std::str::from_utf8(s).is_ok();
    if utf8 && s.len() <= MAX_TEXT {
        put_box(kind, body, out);
        return;
    }
    if !utf8 {
        r.text += 1;
    }
    let start = begin_box(kind, out);
    out.extend_from_slice(&body[..fields]);
    out.extend(
        s.iter()
            .take(MAX_TEXT)
            .map(|&c| if c.is_ascii() { c } else { b'?' }),
    );
    out.push(0);
    end_box(start, out);
}

/// Copies a `dref` (version/flags, entry_count, entries), each entry's text
/// made short ASCII when it is not UTF-8 or long ([`text_box`]): mp4-atom
/// reads every entry as a `url ` location, also a QuickTime `alis`, whose
/// record is binary when it points elsewhere.
fn dref(b: &[u8], r: &mut MoovRepairs, out: &mut Vec<u8>) {
    let start = begin_box(b"dref", out);
    out.extend_from_slice(&b[..b.len().min(8)]);
    let mut p = 8;
    while p < b.len() {
        let Some((size, h)) = box_extent(&b[p..]) else {
            r.junk += (b.len() - p) as u64;
            break;
        };
        let e = &b[p..p + size];
        text_box(&e[4..8], &e[h..], 4, r, out);
        p += size;
    }
    end_box(start, out);
}

/// Placeholder sample-entry type for an entry mp4-atom can't decode (an
/// `esds` whose descriptors overrun it, …); its own type is kept in
/// [`EntryExtras::entry_fourcc`].
const UNDECODABLE_ENTRY: &[u8; 4] = b"fxun";

/// Video sample entries mp4-atom decodes (Dolby Vision's are renamed to
/// these by then).
const DECODED_VIDEO_ENTRIES: [&[u8; 4]; 7] = [b"avc1", b"avc3", b"hvc1", b"hev1", b"av01", b"vp08", b"vp09"];

/// The codec configuration box of a sample entry `kind`, for those with one.
fn entry_config(kind: &[u8; 4]) -> Option<&'static [u8; 4]> {
    Some(match kind {
        b"avc1" | b"avc3" => b"avcC",
        b"hvc1" | b"hev1" => b"hvcC",
        b"av01" => b"av1C",
        b"vp08" | b"vp09" => b"vpcC",
        b"mp4a" => b"esds",
        b"ac-3" => b"dac3",
        b"ec-3" => b"dec3",
        b"Opus" => b"dOps",
        b"fLaC" => b"dfLa",
        _ => return None,
    })
}

/// The `trak`s of a normalized `moov` that mp4-atom decodes, with their
/// position among all `trak`s (the stream index ffmpeg gives them) and
/// their [`EntryExtras`] ([`decode_trak`] mends what it can). A `trak` that
/// still fails is left out only when no client could play it: one whose
/// handler is not video or sound (timecode, chapters, subtitles…) or that
/// holds no samples (an empty `stbl`, as found). A video or sound track
/// with samples fails the index with its cause instead, so the file's
/// probe decides how it plays (a client may play it as is): left out, a
/// movie played without its sound.
fn decode_moov(
    buf: &mut [u8],
    extras: Vec<EntryExtras>,
    warnings: &mut Vec<String>,
) -> Result<(Moov, Vec<(u32, EntryExtras)>)> {
    let mut moov = Moov::default();
    let mut mvhd = None;
    let mut kept = Vec::new();
    let mut extras = extras.into_iter();
    let mut ordinal = 0u32;
    let fail = |e: mp4_atom::Error| HlsError::mp4(format!("moov: {e}"));
    let boxes = child_boxes(buf, 8);
    // A fragmented file's samples are in its fragments.
    let fragmented = boxes.iter().any(|c| &c.0 == b"mvex");
    for (kind, s, e) in boxes {
        match &kind {
            b"mvhd" => mvhd = Some(mp4_atom::Mvhd::decode(&mut &buf[s..e]).map_err(fail)?),
            b"mvex" => moov.mvex = Some(mp4_atom::Mvex::decode(&mut &buf[s..e]).map_err(fail)?),
            b"trak" => {
                let n = ordinal;
                ordinal += 1;
                let mut x = extras.next().unwrap_or_default();
                match decode_trak(buf, s, e, n, &mut x, warnings) {
                    Ok(t) => {
                        moov.trak.push(t);
                        kept.push((n, x));
                    }
                    Err(err) => match trak_handler(buf, s, e) {
                        Some(h) if matches!(&h, b"vide" | b"soun") => {
                            let h = String::from_utf8_lossy(&h);
                            if trak_has_samples(buf, s, e, fragmented) {
                                return Err(HlsError::mp4(format!("moov: trak {n} ({h}): {err}")));
                            }
                            warnings.push(format!("trak {n} ({h}): {err}: left out (no samples)"));
                        }
                        h => warnings.push(format!(
                            "trak {n} ({}): {err}: left out (not video or sound)",
                            h.map_or_else(
                                || "no handler".into(),
                                |h| String::from_utf8_lossy(&h).into_owned()
                            )
                        )),
                    },
                }
            }
            _ => {}
        }
    }
    moov.mvhd = mvhd.ok_or_else(|| HlsError::mp4("moov: missing box: mvhd"))?;
    Ok((moov, kept))
}

/// Decodes the `trak` at `s..e` of a normalized `moov`, mending its first
/// sample entry in place when that is what mp4-atom rejects:
/// 1. optional boxes of the entry it rejects (a `pasp`, `btrt` or `colr`
///    [`normalize_moov`] could not fix…) are renamed `free` and ignored, as
///    ffmpeg ignores them: the codec is still copied or played as is;
/// 2. a Dolby Vision entry renamed to the standard entry it extends decodes
///    as the unknown entry it was (a transcode), as before Dolby Vision
///    entries were recognised;
/// 3. an entry whose codec configuration mp4-atom rejects or misses decodes
///    as an unknown entry: video is then transcoded (ffmpeg reads what
///    mp4-atom rejects), AC-3/E-AC-3 is configured from its first frame,
///    other audio is transcoded. A video entry whose configuration reads
///    is not (a transcode for a stream a client may play as is): its error
///    stands.
///
/// Returns the first error when the `trak` still fails.
fn decode_trak(
    buf: &mut [u8],
    s: usize,
    e: usize,
    n: u32,
    x: &mut EntryExtras,
    warnings: &mut Vec<String>,
) -> std::result::Result<Trak, mp4_atom::Error> {
    let first = match Trak::decode(&mut &buf[s..e]) {
        Ok(t) => return Ok(t),
        Err(err) => err,
    };
    let Some((own, es, ee)) =
        trak_stsd(buf, s, e).and_then(|(ss, se)| child_boxes(&buf[..se], ss + 16).first().copied())
    else {
        return Err(first);
    };
    let name = String::from_utf8_lossy(&own).into_owned();
    let config = entry_config(&own);
    let children = entry_children(buf, es, ee, trak_handler(buf, s, e));
    let mut ignored = Vec::new();
    for &(k, cs, ce) in &children {
        if Some(&k) != config && mp4_atom::Any::decode(&mut &buf[cs..ce]).is_err() {
            buf[cs + 4..cs + 8].copy_from_slice(b"free");
            ignored.push(String::from_utf8_lossy(&k).into_owned());
        }
    }
    if !ignored.is_empty() {
        if let Ok(t) = Trak::decode(&mut &buf[s..e]) {
            warnings.push(format!(
                "trak {n}: {} of sample entry {name} not decodable ({first}): ignored",
                ignored.join(", ")
            ));
            return Ok(t);
        }
    }
    if let (Some(at), Some(dv)) = (x.renamed_at.take(), x.dovi_fourcc.take()) {
        buf[at..at + 4].copy_from_slice(&dv);
        return match Trak::decode(&mut &buf[s..e]) {
            Ok(t) => {
                warnings.push(format!(
                    "Dolby Vision sample entry not decodable as its standard entry ({first}): indexed as unknown"
                ));
                Ok(t)
            }
            Err(_) => Err(first),
        };
    }
    let config_reads = config.is_some_and(|c| {
        children
            .iter()
            .any(|&(k, cs, ce)| &k == c && mp4_atom::Any::decode(&mut &buf[cs..ce]).is_ok())
    });
    if config_reads && DECODED_VIDEO_ENTRIES.contains(&&own) {
        return Err(first);
    }
    // AC-3 with a `dac3` mp4-atom rejects: configured from the first frame,
    // as without one.
    let placeholder = match &own {
        b"ac-3" => UNCONFIGURED_AC3,
        b"ec-3" => UNCONFIGURED_EAC3,
        _ => UNDECODABLE_ENTRY,
    };
    buf[es + 4..es + 8].copy_from_slice(placeholder);
    match Trak::decode(&mut &buf[s..e]) {
        Ok(t) => {
            warnings.push(format!(
                "trak {n}: sample entry {name} not decodable ({first}): indexed as unknown"
            ));
            x.entry_fourcc = Some(own);
            Ok(t)
        }
        Err(_) => {
            buf[es + 4..es + 8].copy_from_slice(&own);
            Err(first)
        }
    }
}

/// The child boxes of the sample entry at `es..ee` of a `moov` buffer, in a
/// `trak` whose handler is `handler`: after a VisualSampleEntry's 78 bytes of
/// fields, or an AudioSampleEntry's 28 (plus 16 or 36 in QuickTime's sound
/// descriptions 1 and 2). None for other entries.
fn entry_children(
    moov: &[u8],
    es: usize,
    ee: usize,
    handler: Option<[u8; 4]>,
) -> Vec<([u8; 4], usize, usize)> {
    let kind = moov.get(es + 4..es + 8).unwrap_or_default();
    let visual = VISUAL_ENTRIES.iter().any(|k| &k[..] == kind) || handler == Some(*b"vide");
    let fields = if visual {
        78
    } else if handler == Some(*b"soun") {
        28 + match moov.get(es + 16..es + 18) {
            Some([0, 1]) => 16,
            Some([0, 2]) => 36,
            _ => 0,
        }
    } else {
        return Vec::new();
    };
    child_boxes(&moov[..ee], es + 8 + fields)
}

/// The big-endian `u32` `at` bytes into the box at `s..e` of `b`.
fn box_u32(b: &[u8], (s, e): (usize, usize), at: usize) -> Option<u32> {
    (s + at + 4 <= e).then(|| u32::from_be_bytes([b[s + at], b[s + at + 1], b[s + at + 2], b[s + at + 3]]))
}

/// The handler type of the `trak` at `s..e` of a `moov` buffer (`hdlr`:
/// header, version/flags, pre_defined, then the type).
fn trak_handler(moov: &[u8], s: usize, e: usize) -> Option<[u8; 4]> {
    let hdlr = trak_box(moov, s, e, &[b"mdia", b"hdlr"])?;
    box_u32(moov, hdlr, 16).map(u32::to_be_bytes)
}

/// Whether the `trak` at `s..e` of a `moov` buffer holds samples: its
/// `stsz`/`stz2` counts some (header, version/flags, then a field before
/// the count), or, in a fragmented file, whose fragments hold the samples,
/// its `stsd` describes some.
fn trak_has_samples(moov: &[u8], s: usize, e: usize, fragmented: bool) -> bool {
    let field = |leaf: &[u8; 4], at: usize| {
        trak_box(moov, s, e, &[b"mdia", b"minf", b"stbl", leaf])
            .and_then(|b| box_u32(moov, b, at))
            .unwrap_or(0)
    };
    field(b"stsz", 16) > 0 || field(b"stz2", 16) > 0 || (fragmented && field(b"stsd", 12) > 0)
}

/// A `ctts` offset as ffmpeg reads it: signed 32-bit, whatever the box's
/// version. mp4-atom reads version-0 offsets as unsigned (ISO/IEC 14496-12
/// before 2008), but QuickTime always defined them as signed, and encoders
/// write negative ones in version-0 boxes; read as unsigned, one is 2^32
/// ticks late (a remuxed MOV's last segment ran ~2^32 ticks long). Only such
/// a negative offset can reach 2^31.
fn composition_offset(raw: i64) -> i64 {
    raw as u32 as i32 as i64
}

/// `stts` deltas above this are not durations but negative corrections
/// written as unsigned (ffmpeg's default `max_stts_delta`).
const MAX_STTS_DELTA: u32 = u32::MAX - 48_000 * 10;

/// Clips `stts` deltas above [`MAX_STTS_DELTA`] to one tick, as ffmpeg does:
/// some muxers end a track with a negative delta (2^32 − 32 000 at 25 kHz:
/// −1.28 s) that, read as unsigned, made the last remuxed segment last
/// ~2^32 ticks (24 files of a real library: playlists of ~172 000 s). The
/// correction it stood for is taken out of the following deltas, never
/// below one tick (ffmpeg's `mov_read_stts`). Returns the entries clipped.
fn clip_stts_deltas(stts: &mut [(u32, u32)]) -> usize {
    let (mut corrected, mut current) = (0i64, 0i64);
    let mut clipped = 0;
    for e in stts.iter_mut() {
        let count = e.0 as i64;
        if e.1 > MAX_STTS_DELTA {
            let magnitude = e.1 as i32 as i64;
            e.1 = 1;
            corrected += if magnitude < 0 { magnitude } else { 1 } * count;
            clipped += 1;
        } else {
            corrected += e.1 as i64 * count;
        }
        current += e.1 as i64 * count;
        // A zero delta has nothing to give (ffmpeg's unsigned `duration - 1`
        // wraps there; here it is left alone).
        if current > corrected && e.1 > 0 {
            let drift = (current - corrected) / count.max(1);
            let correction = if e.1 as i64 > drift { drift } else { e.1 as i64 - 1 };
            current -= correction * count;
            e.1 -= correction as u32;
        }
    }
    clipped
}

/// Zeroes composition offsets larger than the track itself (its decode
/// span, at most 2^28 ticks, at least a second): no reordering is that
/// deep, and one would stretch the last segment or shift every presentation
/// time (ffmpeg drops a `ctts` with an offset beyond 2^28: "CTTS invalid").
/// Returns how many entries were zeroed.
fn drop_implausible_offsets(ctts: &mut [(u32, i64)], stts: &[(u32, u32)], timescale: u32) -> usize {
    let span: i64 = stts.iter().map(|e| e.0 as i64 * e.1 as i64).sum();
    let limit = span.min(1 << 28).max(timescale as i64);
    let mut n = 0;
    for e in ctts.iter_mut().filter(|e| e.1.abs() > limit) {
        e.1 = 0;
        n += 1;
    }
    n
}

/// Builds the playback index of an MP4/MOV file.
pub(crate) fn build(src: &Source) -> Result<PlaybackIndex> {
    let (moov_pos, moov_size) = top_level_moov(src)?;
    if moov_size > 512 << 20 {
        return Err(HlsError::mp4("moov larger than 512 MiB"));
    }
    if moov_pos.saturating_add(moov_size) > src.len() {
        return Err(HlsError::mp4("moov runs past the end of the file (truncated)"));
    }
    let raw = src.read_at(moov_pos, moov_size as usize)?;
    let mut repairs = MoovRepairs::default();
    let mut buf = normalize_moov(&raw, &mut repairs);
    drop(raw);
    let mut warnings = repairs.warnings();
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
    let (moov, extras) = decode_moov(&mut buf, extras, &mut warnings)?;
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
    for (trak, (n, x)) in moov.trak.iter().zip(extras) {
        let frag = frag_tables.as_mut().and_then(|m| m.remove(&trak.tkhd.track_id));
        match track_from_trak(n, trak, movie_ts, src, &mut warnings, frag, x) {
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
    // The video's end sets the last segment's: one far past the movie's
    // duration (an edit list can trim a little) means bad timing tables.
    if let Some(end) = video.and_then(|vi| tracks[vi].extra.end_ns) {
        if duration_ns > 0 && end > duration_ns.saturating_mul(2).saturating_add(60_000_000_000) {
            warnings.push(format!(
                "video ends at {:.3} s, far past the movie's {:.3} s",
                end as f64 / 1e9,
                duration_ns as f64 / 1e9
            ));
        }
    }

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
    // `fiel` (field count 1 = progressive, 2 = interlaced), as flux-probe reads it.
    let mut fiel: Option<bool> = None;
    let fiel_of = |f: &Option<mp4_atom::Fiel>| f.as_ref().map(|f| f.field_count == 2);
    match entry {
        SampleEntry::Avc1(e) => {
            pasp = pasp_of(&e.pasp);
            fiel = fiel_of(&e.fiel);
            t.codec = Codec::H264;
            t.codec_tag = "avc1".into();
            t.codec_private = box_body(&e.avcc, false).unwrap_or_default();
            t.width = Some(e.visual.width as u32);
            t.height = Some(e.visual.height as u32);
            t.color = colr_info(&e.colr);
        }
        SampleEntry::Avc3(e) => {
            pasp = pasp_of(&e.pasp);
            fiel = fiel_of(&e.fiel);
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
            fiel = fiel_of(&e.fiel);
            t.codec = Codec::Hevc;
            t.codec_tag = "hvc1".into();
            t.codec_private = box_body(&e.hvcc, false).unwrap_or_default();
            t.width = Some(e.visual.width as u32);
            t.height = Some(e.visual.height as u32);
            t.color = colr_info(&e.colr);
        }
        SampleEntry::Hev1(e) => {
            pasp = pasp_of(&e.pasp);
            fiel = fiel_of(&e.fiel);
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
    // A Dolby Vision entry keeps its own name (`dvhe`, `dvh1`…) and record,
    // and so does an entry read as unknown.
    if let Some(f) = extras.dovi_fourcc.or(extras.entry_fourcc) {
        t.codec_tag = String::from_utf8_lossy(&f).into_owned();
    }
    if kind == TrackKind::Video {
        t.dovi = extras.dovi;
    }

    // Tables
    let mut stts: Vec<(u32, u32)> = stbl
        .stts
        .entries
        .iter()
        .map(|e| (e.sample_count, e.sample_delta))
        .collect();
    let clipped = clip_stts_deltas(&mut stts);
    if clipped > 0 {
        warnings.push(format!(
            "track {}: {clipped} stts entr{} with a delta of nearly 2^32 ticks (a negative one) clipped to 1 tick",
            trak.tkhd.track_id,
            if clipped == 1 { "y" } else { "ies" }
        ));
    }
    let mut ctts: Vec<(u32, i64)> = stbl
        .ctts
        .as_ref()
        .map(|c| {
            c.entries
                .iter()
                .map(|e| (e.sample_count, composition_offset(e.sample_offset)))
                .collect()
        })
        .unwrap_or_default();
    let dropped = drop_implausible_offsets(&mut ctts, &stts, timescale);
    if dropped > 0 {
        warnings.push(format!(
            "track {}: {dropped} ctts entr{} with an offset longer than the track ignored",
            trak.tkhd.track_id,
            if dropped == 1 { "y" } else { "ies" }
        ));
    }
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
    // facts below are known and a remux to `hvc1`/`dvh1` has them in its
    // record, where those entries (and Apple's players) need them.
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
        t.extra.interlaced = fiel;
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
            // Coarse timescales alternate deltas (656/672 at 16 kHz,
            // 3753/3754 at 90 kHz for 23.976 fps): derive the rate from the
            // durations most frames have, else from the average.
            t.frame_rate = dominant_rate(&tables.stts, timescale).or_else(|| {
                let total: i64 = tables.stts.iter().map(|e| e.0 as i64 * e.1 as i64).sum();
                super::average_rate(ticks_to_ns(total, timescale) as f64 / sample_count as f64)
            });
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
            let large = u64::from_be_bytes([
                b[p + 8],
                b[p + 9],
                b[p + 10],
                b[p + 11],
                b[p + 12],
                b[p + 13],
                b[p + 14],
                b[p + 15],
            ]);
            size = usize::try_from(large).unwrap_or(usize::MAX);
        } else if size == 0 {
            size = b.len() - p;
        }
        // A 64-bit size near 2^64 must not wrap `p` back to an earlier box
        // (an endless loop that grows `out`).
        if size < 8 || p.checked_add(size).is_none_or(|end| end > b.len()) {
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
        .map(|(_, s, e)| trak_stsd(moov, s, e))
        .collect()
}

/// The `stsd` box (start, end) of the `trak` at `s..e` of a `moov` buffer.
fn trak_stsd(moov: &[u8], s: usize, e: usize) -> Option<(usize, usize)> {
    trak_box(moov, s, e, &[b"mdia", b"minf", b"stbl", b"stsd"])
}

/// The first box (start, end) at `path` below the `trak` at `s..e` of a
/// `moov` buffer (its containers have 32-bit headers: [`normalize_moov`]).
fn trak_box(moov: &[u8], s: usize, e: usize, path: &[&[u8; 4]]) -> Option<(usize, usize)> {
    let mut level = vec![(s, e)];
    for want in path {
        level = level
            .iter()
            .flat_map(|&(s, e)| child_boxes(&moov[..e], s + 8))
            .filter(|c| &c.0 == *want)
            .map(|c| (c.1, c.2))
            .collect();
    }
    level.first().copied()
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
    /// Where the entry's type was renamed to the standard entry's (the
    /// offset of its four bytes in the `moov` buffer).
    renamed_at: Option<usize>,
    /// The entry's own type when mp4-atom could not decode it and it was
    /// read as an unknown entry ([`UNDECODABLE_ENTRY`]).
    entry_fourcc: Option<[u8; 4]>,
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
                x.renamed_at = Some(es + 4);
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
/// in-band in `sample` (marked complete, as an `hvc1`/`dvh1` entry requires).
/// Other arrays (SEI) are kept. `None` when the sample has no SPS and PPS,
/// or sets an `hvcC` can't hold (a unit of 64 KiB or more).
///
/// A stream has at most 16 VPS, 16 SPS and 64 PPS ids (H.265 §7.4.3), so
/// each array keeps that many distinct units and skips the rest: a hostile
/// sample of millions of units costs one linear pass, and every count and
/// length written fits its 16-bit field.
fn complete_hvcc(rec: &[u8], sample: &[u8], length_size: usize) -> Option<Vec<u8>> {
    let c = hevc::HevcConfig::parse(rec)?;
    // (NAL type, most kept, the distinct units in order of appearance)
    let mut sets: [(u8, usize, Vec<&[u8]>); 3] =
        [(32, 16, Vec::new()), (33, 16, Vec::new()), (34, 64, Vec::new())];
    for (_, nal) in crate::bits::length_prefixed_nals(sample, length_size) {
        let Some(&h) = nal.first() else { continue };
        if let Some((_, max, units)) = sets.iter_mut().find(|s| s.0 == ((h >> 1) & 0x3F)) {
            if units.len() < *max && !units.contains(&nal) {
                if nal.len() > u16::MAX as usize {
                    return None;
                }
                units.push(nal);
            }
        }
    }
    if sets[1].2.is_empty() || sets[2].2.is_empty() {
        return None;
    }
    let mut arrays: Vec<(u8, bool, Vec<&[u8]>)> = sets
        .iter()
        .filter(|s| !s.2.is_empty())
        .map(|s| (s.0, true, s.2.clone()))
        .collect();
    // (Parsed from 16-bit fields, so these fit.)
    for a in c.arrays.iter().filter(|a| !(32..=34).contains(&a.nal_type)) {
        arrays.push((
            a.nal_type,
            a.completeness,
            a.nalus.iter().map(|n| n.as_slice()).collect(),
        ));
    }
    // The 22 header bytes, then numOfArrays and the arrays (ISO/IEC 14496-15 §8.3.3.1).
    let mut out = rec.get(..22)?.to_vec();
    out.push(u8::try_from(arrays.len()).ok()?);
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

/// How far a sample's duration may be from a frame of `frame_ticks` and
/// still be one such frame: 5 %, or what the clocks that timed it round
/// to, one tick or one millisecond. A remux keeps Matroska's millisecond
/// timestamps, so 59.94 fps alternates 16 and 17 ms: 256 and 272 ticks at
/// 16 kHz (6 % apart), 16 and 17 at 1 kHz, 10 and 11 at 600 Hz.
pub(crate) fn frame_slack(frame_ticks: f64, timescale: u32) -> f64 {
    (frame_ticks / 20.0)
        .max(f64::from(timescale) * super::GRID_TOLERANCE_NS as f64 / 1e9)
        .max(1.0)
}

/// The frame rate of the sample durations most of a video's frames have:
/// the mean of the `stts` deltas near the most common one
/// ([`frame_slack`]), when those hold a majority of the samples. A stream
/// cut without re-encoding holds the frame before each cut for seconds,
/// and six such frames of 2–20 s among 34 000 of 3753/3754 ticks at 90 kHz
/// pulled the plain average of a 23.976 fps video down to 22.75 fps.
fn dominant_rate(stts: &[(u32, u32)], timescale: u32) -> Option<Rational> {
    if timescale == 0 {
        return None;
    }
    let mut by_delta = std::collections::BTreeMap::<u32, u64>::new();
    for &(count, delta) in stts.iter().filter(|e| e.1 > 0) {
        *by_delta.entry(delta).or_default() += u64::from(count);
    }
    let total: u64 = stts.iter().map(|e| u64::from(e.0)).sum();
    // The most common delta; the shorter one on a tie.
    let (&mode, _) = by_delta
        .iter()
        .max_by_key(|&(&d, &c)| (c, std::cmp::Reverse(d)))?;
    let slack = frame_slack(f64::from(mode), timescale);
    let (n, sum) = by_delta
        .iter()
        .filter(|&(&d, _)| f64::from(d.abs_diff(mode)) <= slack)
        .fold((0u64, 0u128), |(n, s), (&d, &c)| {
            (n + c, s + u128::from(d) * u128::from(c))
        });
    if n * 2 <= total {
        return None;
    }
    super::average_rate(sum as f64 / n as f64 * 1e9 / f64::from(timescale))
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
            // first keyframe (the samples keep their in-band copies too).
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

    /// A `hev1` entry remuxes to `hvc1` (parameter sets in the record), and the
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

    /// A Dolby Vision entry without the `hvcC` its standard entry requires:
    /// once renamed, the whole `moov` failed to decode and every track was
    /// lost. It indexes as an unknown entry (a transcode) again, with its
    /// record, and the audio is kept.
    #[test]
    fn dolby_vision_entry_without_its_config_box_still_indexes() {
        let path = synthetic_mp4(b"dvhe", Some(&DVCC_P5[..]), ac3_51());
        let mut b = std::fs::read(&path).unwrap();
        let at = b.windows(4).position(|w| w == b"hvcC").unwrap();
        b[at..at + 4].copy_from_slice(b"hvcX");
        std::fs::write(&path, &b).unwrap();
        let idx = crate::index::PlaybackIndex::build(&path).unwrap();
        let v = &idx.tracks[0];
        assert_eq!((v.codec, v.codec_tag.as_str()), (Codec::Other, "dvhe"));
        assert_eq!((v.width, v.height), (Some(3832), Some(1592)));
        assert_eq!(v.hdr.as_ref().and_then(|h| h.dovi).map(|d| d.profile), Some(5));
        assert_eq!(
            (idx.tracks[1].codec, idx.tracks[1].channels),
            (Codec::Ac3, Some(6))
        );
        assert!(
            idx.warnings.iter().any(|w| w.contains("not decodable")),
            "{:?}",
            idx.warnings
        );
        let plan = crate::planner::plan(
            &idx.to_probe_result(),
            &crate::presets::chrome_desktop(),
            &Default::default(),
        );
        assert!(
            matches!(plan.video, flux_core::VideoOp::Transcode { tonemap: true, .. }),
            "{plan:?}"
        );
    }

    /// A first keyframe of 100 000 distinct VPS units (a hostile file): the
    /// completion kept them all, in quadratic time, and wrote counts that
    /// wrapped at 65 536 (an unparseable record). Each array now keeps at
    /// most as many units as a stream has ids.
    #[test]
    fn hvcc_completion_is_bounded() {
        let nal = |t: u8, i: u32| {
            let mut v = vec![0, 0, 0, 5, t << 1, 1];
            v.extend_from_slice(&i.to_be_bytes()[1..]);
            v
        };
        let mut sample = Vec::new();
        for i in 0..100_000 {
            sample.extend(nal(32, i));
        }
        for i in 0..100 {
            sample.extend(nal(34, i));
        }
        sample.extend(nal(33, 0));
        sample.extend(nal(34, 0)); // a repeat
        let rec = complete_hvcc(&HVCC_MAIN10, &sample, 4).unwrap();
        let c = hevc::HevcConfig::parse(&rec).unwrap();
        let counts = (c.nalus(32).count(), c.nalus(33).count(), c.nalus(34).count());
        assert_eq!(counts, (16, 1, 64));
        assert_eq!(c.nalus(32).next(), Some(&nal(32, 0)[4..]));
        // A unit an hvcC can't hold (over 65 535 bytes) keeps the record as is.
        let mut s2 = nal(33, 0);
        let mut big = vec![0, 1, 0, 2, 34 << 1, 1];
        big.resize(4 + 65_538, 0x55);
        s2.extend(big);
        assert_eq!(complete_hvcc(&HVCC_MAIN10, &s2, 4), None);
    }

    /// A 64-bit box size near 2^64 wrapped the walk back to an earlier box:
    /// an endless loop growing the list (overflow checks are off in release).
    #[test]
    fn child_boxes_stop_at_a_wrapping_large_size() {
        let mut b = vec![0, 0, 0, 16];
        b.extend_from_slice(b"free");
        b.extend_from_slice(&[0; 8]);
        b.extend_from_slice(&[0, 0, 0, 1]);
        b.extend_from_slice(b"wrap");
        b.extend_from_slice(&(u64::MAX - 15).to_be_bytes());
        b.extend_from_slice(&[0; 8]);
        assert_eq!(child_boxes(&b, 0), vec![(*b"free", 0, 16)]);
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

    /// A 23.976 fps video on a 90 kHz clock (deltas 3754, 3754, 3753, …)
    /// with stretches cut out without re-encoding: the frame before each
    /// cut lasts seconds. Its rate is the frames', not the average over the
    /// held frames; a mix with no majority keeps the average.
    #[test]
    fn the_rate_of_most_frames_ignores_held_frames() {
        let mut stts = Vec::new();
        for gop in 0..136 {
            for _ in 0..83 {
                stts.extend([(2, 3754), (1, 3753)]);
            }
            // Seven cuts: one frame held 9–15 s.
            if gop % 20 == 7 {
                stts.push((1, 90_000 * (2 + gop as u32 % 19)));
            }
        }
        let frames: u32 = stts.iter().map(|e| e.0).sum();
        let total: u64 = stts.iter().map(|e| e.0 as u64 * e.1 as u64).sum();
        let average = super::super::average_rate(total as f64 * 1e9 / 90_000.0 / frames as f64);
        assert!(average.is_some_and(|r| r.as_f64() < 23.5), "{average:?}");
        assert_eq!(
            dominant_rate(&stts, 90_000),
            Some(Rational {
                num: 24000,
                den: 1001
            })
        );
        // Alternating deltas of a coarse clock: their mean.
        assert_eq!(
            dominant_rate(&[(2, 667), (1, 668), (1, 667), (1, 668), (1, 667)], 16_000),
            Some(Rational {
                num: 24000,
                den: 1001
            })
        );
        // 40 % at 30 fps and 60 % at 60 fps: the 60 fps frames.
        assert_eq!(
            dominant_rate(&[(40, 3000), (60, 1500)], 90_000),
            Some(Rational { num: 60, den: 1 })
        );
        // No majority within 5 %: no dominant rate.
        assert_eq!(dominant_rate(&[(40, 3000), (30, 1500), (30, 4500)], 90_000), None);
        assert_eq!(dominant_rate(&[], 90_000), None);
    }

    /// The `stts` of samples at these decode times (the last one ends the
    /// track).
    fn stts_at(times: &[i64]) -> Vec<(u32, u32)> {
        let mut out: Vec<(u32, u32)> = Vec::new();
        for w in times.windows(2) {
            let d = (w[1] - w[0]) as u32;
            match out.last_mut() {
                Some(e) if e.1 == d => e.0 += 1,
                _ => out.push((1, d)),
            }
        }
        out
    }

    /// 59.94 fps on a coarse clock alternates deltas a whole tick (or a
    /// millisecond) apart, more than 5 % of a frame: 17/16 at 1 kHz,
    /// 272/256 at 16 kHz (a remux of Matroska's millisecond timestamps),
    /// 10/11 at 600 Hz. All of them are its frames, also with held frames
    /// among them; the most common delta alone made 58.824 or 60 fps, and a
    /// transcode converted to that rate dropped or repeated frames.
    #[test]
    fn coarse_clocks_rate_high_frame_rates_by_all_their_deltas() {
        let ntsc60 = Rational {
            num: 60000,
            den: 1001,
        };
        // Frame i at i × 1001/60 ms, rounded to the clock (720 frames).
        let ms = |i: i64| (i as f64 * 1001.0 / 60.0).round() as i64;
        for ts in [1000u32, 16_000, 600] {
            let at = |i: i64| match ts {
                16_000 => 16 * ms(i),
                600 => (i as f64 * 10.01).round() as i64,
                _ => ms(i),
            };
            let times: Vec<i64> = (0..=720).map(at).collect();
            let stts = stts_at(&times);
            assert!(stts.len() > 1, "{ts}: {stts:?}");
            assert_eq!(dominant_rate(&stts, ts), Some(ntsc60), "{ts} Hz: {stts:?}");
            // Two frames held 3 s and 5 s (cut without re-encoding).
            let mut held = times.clone();
            for (k, t) in held.iter_mut().enumerate() {
                *t += i64::from(ts) * (3 * i64::from(k > 200) + 5 * i64::from(k > 500));
            }
            assert_eq!(dominant_rate(&stts_at(&held), ts), Some(ntsc60), "{ts} Hz held");
        }
        // 60 fps at 1 kHz alternates 17/17/16: 60.
        let times: Vec<i64> = (0..=600)
            .map(|i| (i as f64 * 1000.0 / 60.0).round() as i64)
            .collect();
        assert_eq!(
            dominant_rate(&stts_at(&times), 1000),
            Some(Rational { num: 60, den: 1 })
        );
        // 30 and 60 fps stay apart on a fine clock.
        assert_eq!(
            dominant_rate(&[(60, 1500), (40, 3000)], 90_000),
            Some(Rational { num: 60, den: 1 })
        );
    }

    /// QuickTime defines `ctts` offsets as signed, and encoders write
    /// negative ones in version-0 boxes, which mp4-atom reads as unsigned (a
    /// remuxed MOV's last segment was ~2^32 ticks long, 324 files of a real
    /// library). They are signed whatever the version, and an offset longer
    /// than the track is ignored.
    #[test]
    fn composition_offsets_are_signed_in_version_0_boxes() {
        assert_eq!(composition_offset(0xFFFF_FFFF), -1);
        assert_eq!(composition_offset((1i64 << 32) - 2002), -2002);
        assert_eq!(composition_offset(2002), 2002);
        // Version 1 boxes are read as signed already.
        assert_eq!(composition_offset(-1001), -1001);
        // 100 frames of one tick at 25 fps.
        let stts = [(100u32, 1u32)];
        let mut ctts = vec![(1, 2), (2, -1), (1, 1 << 20), (1, -(1 << 20)), (95, 0)];
        assert_eq!(drop_implausible_offsets(&mut ctts, &stts, 25), 2);
        assert_eq!(ctts, vec![(1, 2), (2, -1), (1, 0), (1, 0), (95, 0)]);
        // A second is plausible however short the track.
        let mut short = vec![(2, 24_000)];
        assert_eq!(drop_implausible_offsets(&mut short, &[(2, 1001)], 24_000), 0);
    }

    /// A last `stts` delta of 2^32 − 32 000 (a −1.28 s correction at 25 kHz,
    /// written unsigned) made the last remuxed segment ~2^32 ticks long. It
    /// is clipped to a tick, and a correction mid-track is taken out of the
    /// deltas that follow, never below a tick, as ffmpeg does.
    #[test]
    fn huge_stts_deltas_are_clipped_like_ffmpeg() {
        let mut last = vec![(13_555, 1000), (1, u32::MAX - 31_999)];
        assert_eq!(clip_stts_deltas(&mut last), 1);
        assert_eq!(last, vec![(13_555, 1000), (1, 1)]);
        let mut mid = vec![(10, 1000), (1, u32::MAX - 2_999), (10, 1000)];
        assert_eq!(clip_stts_deltas(&mut mid), 1);
        assert_eq!(mid, vec![(10, 1000), (1, 1), (10, 700)]);
        // A zero delta after a correction stays zero (no underflow).
        let mut zero = vec![(10, 1000), (1, u32::MAX - 2_999), (1, 0), (10, 1000)];
        assert_eq!(clip_stts_deltas(&mut zero), 1);
        assert_eq!(zero, vec![(10, 1000), (1, 1), (1, 0), (10, 700)]);
        let mut fine = vec![(10, 1000), (1, 480_000)];
        assert_eq!(clip_stts_deltas(&mut fine), 0);
        assert_eq!(fine, vec![(10, 1000), (1, 480_000)]);
    }

    /// `udta` holding iTunes items in a legacy code page.
    fn udta_with_ilst(text: &[u8]) -> flux_test_media::moov::Bx {
        use flux_test_media::moov::Bx;
        let data = |t: &[u8]| Bx::leaf(b"data", [&[0u8, 0, 0, 1, 0, 0, 0, 0][..], t].concat());
        let item = |k: &[u8; 4]| Bx::leaf(k, data(text).to_bytes()).to_bytes();
        let ilst = Bx::leaf(b"ilst", [item(b"\xa9nam"), item(b"cprt")].concat());
        let hdlr = Bx::leaf(b"hdlr", [&[0u8; 8][..], b"mdir", b"appl", &[0; 9]].concat());
        let meta = Bx::leaf(
            b"meta",
            [&[0u8; 4][..], &hdlr.to_bytes(), &ilst.to_bytes()].concat(),
        );
        Bx::container(b"udta", vec![meta])
    }

    /// Text in a legacy code page anywhere in `moov` (handler names, iTunes
    /// items), a QuickTime `alis` self-reference and junk after the last
    /// box failed the whole `moov` ("invalid string", "Only UTF-8 text is
    /// supported in ilst data atoms": a 501 for 215 files of a real
    /// library). Each indexes like the clean file.
    #[test]
    fn legacy_text_and_junk_in_moov_index_like_the_clean_file() {
        use flux_test_media::moov::{find_moov, Bx};
        // "视频" in GBK: not UTF-8.
        const GBK: &[u8] = b"\xca\xd3\xc6\xb5";
        let clean_path = synthetic_mp4(b"hvc1", None, aac_stereo());
        let clean = crate::index::PlaybackIndex::build(&clean_path).unwrap();
        let bytes = std::fs::read(&clean_path).unwrap();
        let (at, size) = find_moov(&bytes).unwrap();
        type Edit = fn(&mut Bx);
        let cases: [(&str, Edit, &str); 8] = [
            (
                "hdlr names",
                |m| {
                    m.visit(&mut |_, b| {
                        if &b.kind == b"hdlr" {
                            b.body.truncate(24);
                            b.body.extend_from_slice(GBK);
                            b.body.push(0);
                        }
                    })
                },
                "not UTF-8",
            ),
            (
                "ilst text",
                |m| m.kids.as_mut().unwrap().push(udta_with_ilst(GBK)),
                "",
            ),
            (
                "track udta name",
                |m| {
                    for t in m.traks_mut(b"soun") {
                        let name = Bx::leaf(b"name", GBK.to_vec());
                        t.kids.as_mut().unwrap().push(Bx::container(b"udta", vec![name]));
                        t.kids.as_mut().unwrap().push(udta_with_ilst(GBK));
                    }
                },
                "",
            ),
            (
                "alis record",
                |m| {
                    // A reference to another file: a binary alias record.
                    let alis = Bx::leaf(b"alis", [&[0u8, 0, 0, 0][..], GBK].concat()).to_bytes();
                    m.visit(&mut |_, b| {
                        if &b.kind == b"dref" {
                            b.body = [&[0u8, 0, 0, 0, 0, 0, 0, 1][..], &alis].concat();
                        }
                    })
                },
                "not UTF-8",
            ),
            (
                "padded tables",
                |m| {
                    m.visit(&mut |_, b| {
                        if matches!(&b.kind, b"stts" | b"stsc" | b"stsz" | b"stco") {
                            b.body.extend_from_slice(&[0; 4]);
                        }
                    })
                },
                "bytes past their entries",
            ),
            (
                // As ffmpeg reads them: the fields, then bytes it skips.
                "padded headers",
                |m| {
                    m.visit(&mut |_, b| {
                        if matches!(&b.kind, b"mvhd" | b"tkhd" | b"mdhd" | b"vmhd" | b"smhd") {
                            b.body.extend_from_slice(&[0; 4]);
                        }
                    })
                },
                "bytes past",
            ),
            (
                "empty stbl",
                |m| {
                    // A third track whose stbl holds nothing (mp4-atom
                    // requires stsd): it is left out, the others are kept.
                    let mut t = m.traks_mut(b"soun")[0].clone();
                    t.visit(&mut |_, b| {
                        if &b.kind == b"tkhd" {
                            b.body[12..16].copy_from_slice(&3u32.to_be_bytes());
                        }
                        if &b.kind == b"stbl" {
                            b.kids = Some(Vec::new());
                        }
                    });
                    m.kids.as_mut().unwrap().push(t);
                },
                "left out",
            ),
            (
                "junk tail",
                |m| {
                    // As found: a 32-bit word read as a size far past `moov`.
                    let mut junk = vec![0x3c, 0xc7, 0x09, 0xf3, 0x03, 0x80, 0x00, 0x00];
                    junk.extend((0..440u32).map(|i| (i * 7) as u8));
                    m.kids.as_mut().unwrap().push(Bx::raw(junk));
                },
                "after the last whole box",
            ),
        ];
        for (what, edit, warning) in cases {
            let mut m = Bx::parse(&bytes[at..at + size]);
            edit(&mut m);
            let moov = m.to_bytes();
            assert!(
                Moov::decode(&mut moov.as_slice()).is_err(),
                "{what}: mp4-atom alone fails"
            );
            // The moov grows in place: fragments are addressed from their moof.
            let file = [&bytes[..at], &moov[..], &bytes[at + size..]].concat();
            let path = clean_path.with_file_name(format!("{}.mp4", what.replace(' ', "-")));
            std::fs::write(&path, &file).unwrap();
            let idx = crate::index::PlaybackIndex::build(&path).unwrap_or_else(|e| panic!("{what}: {e}"));
            assert_eq!(idx.tracks, clean.tracks, "{what}");
            assert_eq!(idx.keyframes, clean.keyframes, "{what}");
            assert_eq!(idx.duration_ns, clean.duration_ns, "{what}");
            assert!(
                warning.is_empty() || idx.warnings.iter().any(|w| w.contains(warning)),
                "{what}: {:?}",
                idx.warnings
            );
        }
        // A moov that runs past the end of the file still fails.
        let cut = &bytes[..at + size - 1];
        let path = clean_path.with_file_name("cut.mp4");
        std::fs::write(&path, cut).unwrap();
        let e = crate::index::PlaybackIndex::build(&path).unwrap_err().to_string();
        assert!(e.contains("truncated"), "{e}");
    }

    /// A sample entry mp4-atom rejects (here an `hvcC` cut short, and a
    /// `dac3` cut short) failed the whole `moov`. The track is indexed as an
    /// unknown entry (video: a transcode), and AC-3 as configured from its
    /// first frame.
    #[test]
    fn an_entry_mp4_atom_rejects_costs_only_its_track() {
        use flux_test_media::moov::{find_moov, Bx};
        let path = synthetic_mp4(b"hvc1", None, ac3_51());
        let bytes = std::fs::read(&path).unwrap();
        let (at, size) = find_moov(&bytes).unwrap();
        let mut m = Bx::parse(&bytes[at..at + size]);
        m.visit(&mut |_, b| {
            if &b.kind == b"stsd" {
                match &b.body[12..16] {
                    b"hvc1" => b.edit_entry_child(b"hvcC", |c| c[..5].to_vec()),
                    b"ac-3" => b.edit_entry_child(b"dac3", |c| c[..1].to_vec()),
                    _ => {}
                }
            }
        });
        let moov = m.to_bytes();
        assert!(Moov::decode(&mut moov.as_slice()).is_err());
        let path = path.with_file_name("rejected-entries.mp4");
        std::fs::write(&path, [&bytes[..at], &moov[..], &bytes[at + size..]].concat()).unwrap();
        let idx = crate::index::PlaybackIndex::build(&path).unwrap();
        let shape: Vec<_> = idx
            .tracks
            .iter()
            .map(|t| (t.codec, t.codec_tag.as_str(), t.width, t.height))
            .collect();
        assert_eq!(
            shape,
            [
                (Codec::Other, "hvc1", Some(3832), Some(1592)),
                (Codec::Ac3, "ac-3", None, None)
            ],
            "{:?}",
            idx.warnings
        );
        assert_eq!(
            idx.warnings
                .iter()
                .filter(|w| w.contains("not decodable"))
                .count(),
            2,
            "{:?}",
            idx.warnings
        );
        let plan = crate::planner::plan(
            &idx.to_probe_result(),
            &crate::presets::chrome_desktop(),
            &Default::default(),
        );
        assert!(
            matches!(plan.video, flux_core::VideoOp::Transcode { .. }),
            "{plan:?}"
        );
    }

    /// An `esds` mp4-atom can't read (a DecoderConfigDescriptor whose length
    /// swallows half the SLConfig's header, as found in a real file; bytes
    /// after the ES_Descriptor; a length past the box) is rebuilt from its
    /// decoder configuration; a clean one is left alone.
    #[test]
    fn esds_is_rebuilt_only_when_mp4_atom_cant_read_it() {
        let hex = |s: &str| -> Vec<u8> {
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
                .collect()
        };
        // ES_Descriptor { ES_ID 2, DecoderConfig { AAC, ASC 12 10 }, SLConfig },
        // with 4-byte lengths, as many muxers write them.
        let clean = hex(concat!(
            "00000000",
            "0380808022",
            "000200",
            "0480808014",
            "4015000000000496e80002ee00",
            "0580808002",
            "1210",
            "0680808001",
            "02"
        ));
        assert_eq!(repair_esds(&clean), None);
        // The DecoderConfigDescriptor claims 23 bytes: it ends inside the
        // SLConfig's length.
        let swallowing = hex(concat!(
            "00000000",
            "0380808022",
            "000200",
            "0480808017",
            "4015000000000496e80002ee00",
            "0580808002",
            "1210",
            "0680808001",
            "02"
        ));
        let trailing = [&clean[..], &[1, 2, 3]].concat();
        let mut over = clean.clone();
        over[8] += 50;
        for (what, b) in [
            ("swallowing", &swallowing),
            ("trailing", &trailing),
            ("over", &over),
        ] {
            let mut raw = Vec::new();
            put_box(b"esds", b, &mut raw);
            assert!(mp4_atom::Esds::decode(&mut raw.as_slice()).is_err(), "{what}");
            let fixed = repair_esds(b).unwrap_or_else(|| panic!("{what}"));
            let mut boxed = Vec::new();
            put_box(b"esds", &fixed, &mut boxed);
            let esds = mp4_atom::Esds::decode(&mut boxed.as_slice()).unwrap();
            let dc = &esds.es_desc.dec_config;
            assert_eq!(dc.object_type_indication, 0x40, "{what}");
            assert_eq!(
                dc.dec_specific.as_ref().map(|d| d.raw.clone()),
                Some(vec![0x12, 0x10]),
                "{what}"
            );
        }
    }

    /// An `avcC` ends after its parameter sets, or after its High-profile
    /// extension when that is whole: what follows made mp4-atom fail the
    /// whole `moov` ("under decode: avcC").
    #[test]
    fn avcc_records_end_after_their_last_field() {
        // Main profile, one 4-byte SPS, one 2-byte PPS.
        let base = [
            1u8, 77, 0, 30, 0xFF, 0xE1, 0, 4, 0x67, 77, 0, 30, 1, 0, 2, 0x68, 0xEE,
        ];
        assert_eq!(avcc_len(&base), Some(base.len()));
        let padded = [&base[..], &[0, 0]].concat();
        assert_eq!(avcc_len(&padded), Some(base.len()));
        // High-profile extension (4:2:0, 8 bit, no SPS extensions): kept;
        // trailing bytes after it are not.
        let ext = [&base[..], &[0xFD, 0xF8, 0xF8, 0]].concat();
        assert_eq!(avcc_len(&ext), Some(ext.len()));
        assert_eq!(avcc_len(&[&ext[..], &[9]].concat()), Some(ext.len()));
        // An extension that claims an SPS extension it doesn't have: dropped.
        assert_eq!(
            avcc_len(&[&base[..], &[0xFD, 0xF8, 0xF8, 1, 0, 9]].concat()),
            Some(base.len())
        );
        // Parameter sets past the end: not a record to trim.
        assert_eq!(avcc_len(&base[..12]), None);
    }

    /// `bytes` (a [`synthetic_mp4`] clip) with its `moov` edited in place
    /// (its fragments address samples from their `moof`), written beside
    /// `like` as `name`.
    fn edited(
        bytes: &[u8],
        like: &std::path::Path,
        name: &str,
        edit: impl FnOnce(&mut flux_test_media::moov::Bx),
    ) -> std::path::PathBuf {
        use flux_test_media::moov::{find_moov, Bx};
        let (at, size) = find_moov(bytes).unwrap();
        let mut m = Bx::parse(&bytes[at..at + size]);
        edit(&mut m);
        let path = like.with_file_name(name);
        std::fs::write(
            &path,
            [&bytes[..at], &m.to_bytes()[..], &bytes[at + size..]].concat(),
        )
        .unwrap();
        path
    }

    /// Sets a `trak`'s track_ID (a version-0 `tkhd`).
    fn set_track_id(t: &mut flux_test_media::moov::Bx, id: u32) {
        t.find_mut(b"tkhd").unwrap().body[12..16].copy_from_slice(&id.to_be_bytes());
    }

    /// A `trak` mp4-atom rejects is left out only when no client could play
    /// it: one with no samples (an empty `stbl`, as found) or whose handler
    /// is neither video nor sound, wherever it sits (the tracks after it
    /// keep their stream numbers). A video or sound track with samples fails
    /// the index with its cause: left out, HLS played the movie without its
    /// sound and said nothing (review F1). An audio `mdhd` with 4 bytes past
    /// its fields (which ffmpeg reads) is kept.
    #[test]
    fn only_tracks_no_client_could_play_are_left_out() {
        let clean_path = synthetic_mp4(b"hvc1", None, aac_stereo());
        let clean = crate::index::PlaybackIndex::build(&clean_path).unwrap();
        let bytes = std::fs::read(&clean_path).unwrap();
        let build = |name: &str, edit: fn(&mut flux_test_media::moov::Bx)| {
            crate::index::PlaybackIndex::build(edited(&bytes, &clean_path, name, edit))
        };

        let padded = build("padded-mdhd.mp4", |m| {
            let mdhd = m.traks_mut(b"soun").remove(0).find_mut(b"mdhd").unwrap();
            mdhd.body.extend_from_slice(&[0; 4]);
        })
        .unwrap();
        assert_eq!(padded.tracks, clean.tracks, "{:?}", padded.warnings);

        // Before the clip's tracks: a sound track with an empty stbl, and a
        // timecode track whose mdhd has a version mp4-atom doesn't know.
        let first = build("left-out-first.mp4", |m| {
            let soun = m.traks_mut(b"soun")[0].clone();
            let mut empty = soun.clone();
            set_track_id(&mut empty, 3);
            empty.find_mut(b"stbl").unwrap().kids = Some(Vec::new());
            let mut tmcd = soun;
            set_track_id(&mut tmcd, 4);
            tmcd.find_mut(b"hdlr").unwrap().body[8..12].copy_from_slice(b"tmcd");
            tmcd.find_mut(b"mdhd").unwrap().body[0] = 2;
            let kids = m.kids.as_mut().unwrap();
            let at = kids.iter().position(|k| &k.kind == b"trak").unwrap();
            kids.insert(at, tmcd);
            kids.insert(at, empty);
        })
        .unwrap();
        let shifted: Vec<_> = clean
            .tracks
            .iter()
            .cloned()
            .map(|mut t| {
                t.index += 2;
                t
            })
            .collect();
        assert_eq!(first.tracks, shifted, "{:?}", first.warnings);
        assert_eq!(first.keyframes, clean.keyframes);
        let left_out: Vec<_> = first.warnings.iter().filter(|w| w.contains("left out")).collect();
        assert_eq!(left_out.len(), 2, "{:?}", first.warnings);
        assert!(left_out[0].contains("trak 0 (soun)") && left_out[0].contains("no samples"));
        assert!(left_out[1].contains("trak 1 (tmcd)") && left_out[1].contains("not video or sound"));

        // The sound track with samples (in its fragments), then the video.
        let e = build("sound-rejected.mp4", |m| {
            m.traks_mut(b"soun")[0].find_mut(b"mdhd").unwrap().body[0] = 2;
        })
        .unwrap_err()
        .to_string();
        assert!(e.contains("trak 1 (soun): unknown version: 2"), "{e}");
        let e = build("video-rejected.mp4", |m| {
            m.traks_mut(b"vide")[0].find_mut(b"tkhd").unwrap().body[0] = 2;
        })
        .unwrap_err()
        .to_string();
        assert!(e.contains("trak 0 (vide): unknown version: 2"), "{e}");
    }

    /// Appends `child` (a whole box) to the only sample entry of `stsd`.
    fn push_entry_child(stsd: &mut flux_test_media::moov::Bx, child: &[u8]) {
        let b = &mut stsd.body;
        // stsd body: version/flags, entry_count, then the entry.
        let size = u32::from_be_bytes(b[8..12].try_into().unwrap()) as usize;
        assert_eq!(size, b.len() - 8, "one sample entry");
        b.extend_from_slice(child);
        let size = (size + child.len()) as u32;
        b[8..12].copy_from_slice(&size.to_be_bytes());
    }

    /// An optional box of a sample entry mp4-atom rejects made the track an
    /// unknown entry: Chrome transcoded a stream it plays as is (review F2;
    /// ffmpeg reads each). A `pasp` with 4 bytes past its fields ends after
    /// them (the pixel aspect holds); one that still fails (a `pasp` cut
    /// short, a `colr` of a type mp4-atom doesn't know) is ignored.
    #[test]
    fn a_rejected_optional_entry_box_keeps_the_codec() {
        use flux_test_media::moov::Bx;
        let clean_path = synthetic_mp4(b"hvc1", None, aac_stereo());
        let clean = crate::index::PlaybackIndex::build(&clean_path).unwrap();
        let bytes = std::fs::read(&clean_path).unwrap();
        let with_child = |name: &str, child: Vec<u8>| {
            let path = edited(&bytes, &clean_path, name, |m| {
                let stsd = m.traks_mut(b"vide").remove(0).find_mut(b"stsd").unwrap();
                push_entry_child(stsd, &child);
            });
            crate::index::PlaybackIndex::build(&path).unwrap_or_else(|e| panic!("{name}: {e}"))
        };
        let pasp = |extra: &[u8]| {
            let body = [&64u32.to_be_bytes()[..], &45u32.to_be_bytes(), extra].concat();
            Bx::leaf(b"pasp", body).to_bytes()
        };
        let anamorphic = with_child("pasp.mp4", pasp(&[]));
        assert_eq!(anamorphic.tracks[0].extra.sar, Some((64, 45)));
        let padded = with_child("padded-pasp.mp4", pasp(&[0; 4]));
        assert_eq!(padded.tracks, anamorphic.tracks, "{:?}", padded.warnings);
        assert!(
            padded.warnings.iter().any(|w| w.contains("bytes past")),
            "{:?}",
            padded.warnings
        );
        let caps = crate::presets::chrome_desktop();
        let plan = |i: &crate::index::PlaybackIndex| {
            crate::planner::plan(&i.to_probe_result(), &caps, &Default::default())
        };
        assert_eq!(plan(&padded), plan(&anamorphic));
        assert!(
            !matches!(plan(&padded).video, flux_core::VideoOp::Transcode { .. }),
            "{:?}",
            plan(&padded)
        );
        for (what, child) in [
            ("short-pasp", Bx::leaf(b"pasp", 1u32.to_be_bytes()).to_bytes()),
            (
                "unknown-colr",
                Bx::leaf(b"colr", *b"xxxx\0\x01\0\x01\0\x01").to_bytes(),
            ),
        ] {
            let idx = with_child(&format!("{what}.mp4"), child);
            assert_eq!(idx.tracks, clean.tracks, "{what}: {:?}", idx.warnings);
            assert!(
                idx.warnings
                    .iter()
                    .any(|w| w.contains("of sample entry hvc1 not decodable")),
                "{what}: {:?}",
                idx.warnings
            );
            assert_eq!(plan(&idx), plan(&clean), "{what}");
        }
    }

    /// Text that is not UTF-8 is written as at most [`MAX_TEXT`] ASCII
    /// bytes: a lossy decode tripled a legacy code page's bytes, and an
    /// `hdlr` full of them made one index build peak at ~8× the `moov`
    /// (review F3). An `hdlr` outside a track, which nothing reads, is
    /// copied as is.
    #[test]
    fn text_is_never_larger_than_it_was() {
        use flux_test_media::moov::{find_moov, Bx};
        let clean_path = synthetic_mp4(b"hvc1", None, aac_stereo());
        let clean = crate::index::PlaybackIndex::build(&clean_path).unwrap();
        let bytes = std::fs::read(&clean_path).unwrap();
        let (at, size) = find_moov(&bytes).unwrap();
        let name = vec![0xFFu8; 1 << 20];
        let hdlr = |h: &[u8]| Bx::leaf(b"hdlr", [&[0u8; 8][..], h, &[0; 12], &name, &[0]].concat());
        let mut m = Bx::parse(&bytes[at..at + size]);
        m.visit(&mut |_, b| {
            if &b.kind == b"hdlr" {
                *b = hdlr(&b.body[8..12]);
            }
        });
        m.kids.as_mut().unwrap().push(hdlr(b"mdir"));
        let raw = m.to_bytes();
        let mut r = MoovRepairs::default();
        let out = normalize_moov(&raw, &mut r);
        assert_eq!(r.text, 2);
        assert_eq!(out.len(), raw.len() - 2 * (name.len() - MAX_TEXT));
        let file = [&bytes[..at], &raw[..], &bytes[at + size..]].concat();
        let path = clean_path.with_file_name("long-names.mp4");
        std::fs::write(&path, file).unwrap();
        let idx = crate::index::PlaybackIndex::build(&path).unwrap();
        assert_eq!(idx.tracks, clean.tracks);
        // A long UTF-8 name is cut too; a short one is left alone.
        let mut out = Vec::new();
        let long = [&[0u8; 24][..], &[b'a'; 300], &[0]].concat();
        text_box(b"hdlr", &long, 24, &mut r, &mut out);
        assert_eq!(out.len(), 8 + 24 + MAX_TEXT + 1);
        out.clear();
        let short = [&[0u8; 24][..], "视频".as_bytes(), &[0]].concat();
        text_box(b"hdlr", &short, 24, &mut r, &mut out);
        assert_eq!(&out[8..], &short[..]);
        assert_eq!(r.text, 2);
    }

    /// The normalized `moov` of a clean file is the file's own (no text to
    /// fix, no junk): containers are rewritten with the same sizes.
    #[test]
    fn a_clean_moov_normalizes_to_itself() {
        use flux_test_media::moov::find_moov;
        let bytes = std::fs::read(synthetic_mp4(b"hvc1", None, aac_stereo())).unwrap();
        let (at, size) = find_moov(&bytes).unwrap();
        let mut r = MoovRepairs::default();
        assert_eq!(
            normalize_moov(&bytes[at..at + size], &mut r),
            bytes[at..at + size]
        );
        assert_eq!(r, MoovRepairs::default());
    }
}
