//! Where the packets of an FLV or ASF file really end, read from its last
//! bytes, and how long an MPEG-PS file of joined recordings really is: the
//! duration those containers state (or ffprobe estimates) describes the file
//! as it was meant to be, not as it is.
//!
//! - **FLV.** `onMetaData` carries the duration the encoder intended; a
//!   download that stopped early keeps it (a 3 MB file claiming 300 s held
//!   42 s of tags, so seeks past 42 s failed). Every tag carries its own
//!   millisecond timestamp and is followed by its size, so the last whole
//!   tag is found by walking back from the end.
//! - **ASF.** The File Properties play duration likewise. When the Data
//!   Object runs past the end of the file (a cut download), the payloads of
//!   the last whole packets carry the latest presentation times.
//!
//! - **MPEG-PS.** Files joined byte-wise start their clock over at each
//!   join; ffprobe then reports the last part's clock as the duration.
//!   [`ps_parts`] finds the joins by sampling the clock through the file.
//!
//! [`fix_duration`] folds the result into an ffprobe [`ProbeResult`]: the
//! catalog's duration and the playlist's length then cover what can be
//! decoded. Two or three small reads per FLV or ASF file, ~32 per MPEG-PS.

use std::path::Path;

use flux_core::{Container, ProbeResult};

use crate::driver::{FileSource, RangeRead};

/// Bytes read from the end of a file to find its last FLV tag or ASF packets.
const WINDOW: u64 = 512 << 10;

/// Where a file's packets end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketEnd {
    /// Presentation time of the last packet found, ms, on ffmpeg's timeline
    /// for the file (FLV: tag time; ASF: presentation time minus preroll).
    pub last_ms: u64,
    /// Time of the first packet on the same timeline (FLV), so
    /// `last_ms - first_ms` is the span the packets cover.
    pub first_ms: u64,
    /// ASF: the Data Object is cut short (its stated size runs past the end
    /// of the file).
    pub cut: bool,
}

impl PacketEnd {
    /// The span the packets cover, ms.
    pub fn span_ms(&self) -> u64 {
        self.last_ms.saturating_sub(self.first_ms)
    }
}

fn read<R: RangeRead>(src: &mut R, off: u64, len: u64) -> Option<Vec<u8>> {
    let size = src.size();
    if off >= size {
        return None;
    }
    let len = len.min(size - off) as usize;
    let mut buf = vec![0u8; len];
    let mut got = 0;
    while got < len {
        match src.read_at(off + got as u64, &mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(_) => return None,
        }
    }
    buf.truncate(got);
    Some(buf)
}

fn be24(b: &[u8]) -> u32 {
    (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2])
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// FLV tag at `p` in `b`: (type, data size, timestamp ms), when its header
/// is plausible (audio, video or script data from stream 0).
fn flv_tag(b: &[u8], p: usize) -> Option<(u8, u32, u32)> {
    let h = b.get(p..p + 11)?;
    let kind = h[0] & 0x1F;
    if !matches!(kind, 8 | 9 | 18) || h[0] & 0xC0 != 0 || h[8..11] != [0, 0, 0] {
        return None;
    }
    let size = be24(&h[1..4]);
    let ts = be24(&h[4..7]) | (u32::from(h[7]) << 24);
    Some((kind, size, ts))
}

/// The last whole audio or video tag of `tail` (the file's last bytes): the
/// latest position `q` whose preceding PreviousTagSize points back at a tag
/// header of exactly that size. A tag cut off by the end of the file has no
/// size after it, so it is skipped, as is trailing garbage.
fn flv_last_tag(tail: &[u8]) -> Option<u32> {
    let mut q = tail.len();
    while q >= 4 + 11 {
        let prev = be32(&tail[q - 4..q]) as usize;
        if prev >= 11 && prev <= q - 4 {
            let p = q - 4 - prev;
            if let Some((kind, size, ts)) = flv_tag(tail, p) {
                if size as usize + 11 == prev {
                    if kind != 18 {
                        return Some(ts);
                    }
                    // Script data at the end: the tag before it.
                    q = p;
                    continue;
                }
            }
        }
        q -= 1;
    }
    None
}

/// First and last audio/video tag times of an FLV file.
pub fn flv_end<R: RangeRead>(src: &mut R) -> Option<PacketEnd> {
    let size = src.size();
    let head = read(src, 0, 64 << 10)?;
    if head.get(..3) != Some(b"FLV") || head.len() < 13 {
        return None;
    }
    let mut p = be32(&head[5..9]) as usize + 4;
    let mut first = None;
    // The first audio/video tag (after onMetaData), within the first 64 KiB.
    while let Some((kind, len, ts)) = flv_tag(&head, p) {
        if kind != 18 {
            first = Some(ts);
            break;
        }
        p += 11 + len as usize + 4;
    }
    let from = size.saturating_sub(WINDOW);
    let tail = read(src, from, size - from)?;
    let last = flv_last_tag(&tail)?;
    Some(PacketEnd {
        last_ms: u64::from(last),
        first_ms: u64::from(first.unwrap_or(0).min(last)),
        cut: false,
    })
}

const ASF_HEADER: [u8; 16] = [
    0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C,
];
const ASF_DATA: [u8; 16] = [
    0x36, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C,
];
const ASF_FILE_PROPERTIES: [u8; 16] = [
    0xA1, 0xDC, 0xAB, 0x8C, 0x47, 0xA9, 0xCF, 0x11, 0x8E, 0xE4, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65,
];

fn le16(b: &[u8]) -> u64 {
    u64::from(u16::from_le_bytes([b[0], b[1]]))
}

fn le32(b: &[u8]) -> u64 {
    u64::from(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap_or([0; 8]))
}

/// Reads a field of the size an ASF length type gives (0, 1, 2 or 4 bytes).
fn asf_field(b: &[u8], p: &mut usize, kind: u8) -> Option<u64> {
    let n = [0usize, 1, 2, 4][usize::from(kind & 3)];
    let f = b.get(*p..*p + n)?;
    *p += n;
    Some(match n {
        0 => 0,
        1 => u64::from(f[0]),
        2 => le16(f),
        _ => le32(f),
    })
}

/// The latest payload presentation time (ms, preroll included) in one ASF
/// data packet (ASF spec §5.2), `None` if it doesn't parse.
fn asf_packet_time(b: &[u8]) -> Option<u64> {
    let mut p = 0usize;
    let mut flags = *b.first()?;
    if flags & 0x80 != 0 {
        // Error correction data, then the length type flags.
        p += 1 + usize::from(flags & 0x0F);
        flags = *b.get(p)?;
    }
    p += 1;
    let props = *b.get(p)?;
    p += 1;
    let packet_len = asf_field(b, &mut p, flags >> 5)?;
    asf_field(b, &mut p, flags >> 1)?; // sequence
    let padding = asf_field(b, &mut p, flags >> 3)?;
    let send = le32(b.get(p..p + 4)?);
    p += 6; // send time, duration
    let packet_len = if packet_len == 0 {
        b.len() as u64
    } else {
        packet_len
    };
    let end = (packet_len.saturating_sub(padding) as usize).min(b.len());
    let multiple = flags & 1 != 0;
    let (count, len_kind) = if multiple {
        let f = *b.get(p)?;
        p += 1;
        (usize::from(f & 0x3F), f >> 6)
    } else {
        (1, 0)
    };
    let mut latest = None;
    for _ in 0..count {
        if p >= end {
            break;
        }
        p += 1; // stream number
        asf_field(b, &mut p, props >> 4)?; // media object number
        let offset = asf_field(b, &mut p, props >> 2)?;
        let rep_len = asf_field(b, &mut p, props)? as usize;
        let time = if rep_len == 1 {
            // Compressed payload: the offset field is the presentation
            // time, and the one byte of replicated data its delta.
            p += 1;
            Some(offset)
        } else {
            let rep = b.get(p..p + rep_len)?;
            p += rep_len;
            (rep_len >= 8).then(|| le32(&rep[4..8]))
        };
        latest = latest.max(time);
        let len = if multiple {
            asf_field(b, &mut p, len_kind)? as usize
        } else {
            end.saturating_sub(p)
        };
        p += len;
    }
    // A packet whose payloads carry no time (rare): its send time.
    latest.or(Some(send))
}

/// For an ASF file whose Data Object runs past the end of the file, where
/// the whole packets end; `None` for a complete file (its header holds) or
/// one that doesn't parse.
pub fn asf_end<R: RangeRead>(src: &mut R) -> Option<PacketEnd> {
    let size = src.size();
    let h = read(src, 0, 30)?;
    if h.get(..16)? != ASF_HEADER {
        return None;
    }
    let header_size = le64(&h[16..24]);
    let header = read(src, 0, header_size.min(1 << 20))?;
    let fp = header
        .windows(16)
        .position(|w| w == ASF_FILE_PROPERTIES)
        .and_then(|i| header.get(i..i + 104))?;
    let preroll = le64(&fp[80..88]);
    let (min_packet, max_packet) = (le32(&fp[92..96]), le32(&fp[96..100]));
    if min_packet != max_packet || min_packet == 0 {
        return None;
    }
    let data = read(src, header_size, 50)?;
    if data.get(..16)? != ASF_DATA {
        return None;
    }
    let data_size = le64(&data[16..24]);
    let first_packet = header_size + 50;
    let cut = data_size != 0 && header_size.saturating_add(data_size) > size;
    if !cut {
        return None;
    }
    let whole = size.saturating_sub(first_packet) / min_packet;
    // The last whole packets (64 at most): the latest time among them.
    let from = whole.saturating_sub(64);
    let bytes = read(src, first_packet + from * min_packet, (whole - from) * min_packet)?;
    let latest = bytes
        .chunks_exact(min_packet as usize)
        .filter_map(asf_packet_time)
        .max()?;
    Some(PacketEnd {
        last_ms: latest.saturating_sub(preroll),
        first_ms: 0,
        cut: true,
    })
}

/// A stretch of an MPEG program stream whose clock runs on: files joined
/// byte-wise (concatenated VOBs, `copy /b`, a capture that restarted)
/// start their timestamps over at each join. ffmpeg then reports the last
/// part's clock as the file's duration (2,576 s for a 2 GB file of two
/// recordings, 3,336 + 2,575 s), and its seeks, a binary search on the
/// clock, can't tell the parts apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PsPart {
    /// Where the part starts in the file (a pack header).
    pub offset: u64,
    /// Its earliest and latest timestamps, 90 kHz (the latest unwrapped
    /// past a 33-bit wrap).
    pub first_pts: u64,
    pub last_pts: u64,
}

impl PsPart {
    /// The part's span, ms.
    pub fn span_ms(&self) -> u64 {
        self.last_pts.saturating_sub(self.first_pts) / 90
    }
}

/// Bytes read at each place sampled in an MPEG-PS file.
const PS_WINDOW: u64 = 128 << 10;
/// Places sampled for a clock that goes back.
const PS_SAMPLES: u64 = 32;
const WRAP_33: u64 = 1 << 33;

/// A PES timestamp field (PTS or DTS, 5 bytes with marker bits).
fn ts33(x: &[u8]) -> Option<u64> {
    if x.len() < 5 || x[0] & 1 == 0 || x[2] & 1 == 0 || x[4] & 1 == 0 {
        return None;
    }
    Some(
        (u64::from((x[0] >> 1) & 7) << 30)
            | (u64::from(x[1]) << 22)
            | (u64::from(x[2] >> 1) << 15)
            | (u64::from(x[3]) << 7)
            | u64::from(x[4] >> 1),
    )
}

/// The PTS of the PES packet starting at `p` (`00 00 01 sid len len …`):
/// MPEG-2 PES headers and MPEG-1 system stream packets.
fn pes_pts(p: &[u8]) -> Option<u64> {
    let mut q = 6;
    if p.get(q)? & 0xC0 == 0x80 {
        return (p.get(q + 1)? & 0x80 != 0)
            .then(|| ts33(p.get(q + 3..q + 8)?))
            .flatten();
    }
    while *p.get(q)? == 0xFF {
        q += 1;
        if q > 6 + 16 {
            return None;
        }
    }
    if p.get(q)? & 0xC0 == 0x40 {
        q += 2; // STD buffer size
    }
    (p.get(q)? & 0xE0 == 0x20)
        .then(|| ts33(p.get(q..q + 5)?))
        .flatten()
}

/// (offset, PTS, is video) of the PES packets with a timestamp in `b`, a
/// window of the file at `at`.
fn ps_timestamps(b: &[u8], at: u64) -> Vec<(u64, u64, bool)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 14 <= b.len() {
        if b[i + 2] > 1 {
            i += 3;
            continue;
        }
        if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 1 {
            let sid = b[i + 3];
            let video = (0xE0..=0xEF).contains(&sid);
            if video || (0xC0..=0xDF).contains(&sid) || sid == 0xBD {
                if let Some(pts) = pes_pts(&b[i..]) {
                    out.push((at + i as u64, pts, video));
                }
            }
            i += 4;
        } else {
            i += 1;
        }
    }
    out
}

/// The clock went back from `a` to `b` by more than a second (not a 33-bit
/// wrap, which goes back by almost 2^33).
fn went_back(a: u64, b: u64) -> bool {
    b + 90_000 < a && a - b < WRAP_33 / 2
}

/// The first video timestamp at or after `off`.
fn video_pts_at<R: RangeRead>(src: &mut R, off: u64) -> Option<(u64, u64)> {
    let b = read(src, off, PS_WINDOW)?;
    ps_timestamps(&b, off)
        .into_iter()
        .find(|t| t.2)
        .map(|t| (t.0, t.1))
}

/// Where between `a` and `b` (offset, video PTS; `b`'s clock is behind
/// `a`'s) the clock goes back: the pack header before the first packet of
/// the new clock, the last video PTS before it and the earliest PTS after
/// it (any stream: where ffmpeg starts that part's clock).
fn ps_join<R: RangeRead>(src: &mut R, a: (u64, u64), b: (u64, u64)) -> Option<(u64, u64, u64)> {
    let (mut lo, mut lo_pts, mut hi) = (a.0, a.1, b.0);
    while hi - lo > PS_WINDOW {
        let mid = lo + (hi - lo) / 2;
        match video_pts_at(src, mid) {
            Some((at, p)) if at < hi && !went_back(lo_pts, p) => (lo, lo_pts) = (at, lo_pts.max(p)),
            Some((at, _)) if at < hi && at > lo => hi = at,
            _ => hi = mid.max(lo + 1),
        }
    }
    let b = read(src, lo, hi - lo + 2 * PS_WINDOW)?;
    let ts = ps_timestamps(&b, lo);
    let mut last = lo_pts;
    let i = ts.iter().position(|&(_, p, video)| {
        let back = went_back(last, p);
        if video && !back {
            last = last.max(p);
        }
        back
    })?;
    let at = ts[i].0;
    let rel = (at - lo) as usize;
    let pack = b[..rel]
        .windows(4)
        .rposition(|w| w == [0, 0, 1, 0xBA])
        .map_or(at, |k| lo + k as u64);
    // The new clock's earliest time: the smallest PTS in the window after
    // the join (B-frames and audio start a little before the first one).
    let first = ts[i..]
        .iter()
        .take(64)
        .map(|t| t.1)
        .filter(|&p| !went_back(ts[i].1 + 90_000 * 5, p))
        .min()
        .unwrap_or(ts[i].1);
    Some((pack, last, first))
}

/// The parts of an MPEG-PS file whose clock restarts (see [`PsPart`]), in
/// file order; one part for a file whose clock runs on. `None` if no
/// timestamps are found. About 32 reads of 128 KiB, plus ~10 per restart.
pub fn ps_parts<R: RangeRead>(src: &mut R) -> Option<Vec<PsPart>> {
    let size = src.size();
    let head = read(src, 0, PS_WINDOW)?;
    let first = ps_timestamps(&head, 0).iter().map(|t| t.1).min()?;
    let mut samples: Vec<(u64, u64)> = (0..PS_SAMPLES)
        .filter_map(|k| video_pts_at(src, size * k / PS_SAMPLES))
        .collect();
    samples.dedup_by_key(|s| s.0);
    let tail_from = size.saturating_sub(2 * PS_WINDOW);
    let tail = read(src, tail_from, size - tail_from)?;
    if let Some(end) = ps_timestamps(&tail, tail_from).iter().rfind(|t| t.2) {
        samples.push((end.0, end.1));
    }
    let mut parts = vec![PsPart {
        offset: 0,
        first_pts: first,
        last_pts: first,
    }];
    // Each part's clock, unwrapped past 33-bit wraps (`epoch`).
    let (mut prev, mut epoch) = (None::<(u64, u64)>, 0u64);
    for s in samples {
        match prev {
            Some(a) if went_back(a.1, s.1) => {
                let Some((at, last, start)) = ps_join(src, a, s) else {
                    continue;
                };
                if let Some(p) = parts.last_mut() {
                    p.last_pts = p.last_pts.max(last + epoch * WRAP_33);
                }
                parts.push(PsPart {
                    offset: at,
                    first_pts: start,
                    last_pts: start,
                });
                epoch = 0;
            }
            Some(a) if s.1 + WRAP_33 / 2 < a.1 => epoch += 1,
            _ => {}
        }
        if let Some(p) = parts.last_mut() {
            p.last_pts = p.last_pts.max(s.1 + epoch * WRAP_33);
        }
        prev = Some(s);
    }
    Some(parts)
}

/// Where the packets of a file of `container` end, for the containers whose
/// header duration may outlast them (FLV, ASF).
pub fn packet_end<R: RangeRead>(src: &mut R, container: Container) -> Option<PacketEnd> {
    match container {
        Container::Flv => flv_end(src),
        Container::Asf => asf_end(src),
        _ => None,
    }
}

/// The duration to use, ms, from the header's (`header_ms`, ffprobe's) and
/// where the packets end, with a warning when they disagree:
/// - FLV: the tags (exact millisecond times) whenever they disagree with
///   the header by more than a second, either way: a cut download ends
///   early, and `onMetaData` may also be missing or stale.
/// - ASF: only shortened, when the Data Object is cut and its last packet
///   comes earlier than the header says.
pub fn corrected_ms(
    header_ms: Option<u64>,
    end: &PacketEnd,
    container: Container,
) -> (Option<u64>, Option<String>) {
    let tol = |h: u64| (h / 100).max(1000);
    let span = end.span_ms();
    match (container, header_ms) {
        (Container::Flv, Some(h)) if h.abs_diff(span) > tol(h) && span > 0 => (
            Some(span),
            Some(format!(
                "FLV header says {:.1} s, its tags end at {:.1} s",
                h as f64 / 1e3,
                span as f64 / 1e3
            )),
        ),
        (Container::Flv, None) if span > 0 => (Some(span), None),
        (Container::Asf, Some(h)) if end.cut && span + tol(h) < h => (
            Some(span),
            Some(format!(
                "ASF data ends early: header says {:.1} s, its last packet is at {:.1} s",
                h as f64 / 1e3,
                span as f64 / 1e3
            )),
        ),
        (_, h) => (h, None),
    }
}

/// Corrects an ffprobe result's duration with [`packet_end`] (a few reads
/// at the end of `path`); a warning says so when it changes.
pub fn fix_duration(path: &Path, r: &mut ProbeResult) {
    if !matches!(r.container, Container::Flv | Container::Asf | Container::MpegPs) {
        return;
    }
    let Ok(mut src) = FileSource::open(path) else {
        return;
    };
    if r.container == Container::MpegPs {
        // Joined recordings: the parts add up (ffprobe says the last one).
        if let Some(parts) = ps_parts(&mut src).filter(|p| p.len() > 1) {
            let ms: u64 = parts.iter().map(PsPart::span_ms).sum();
            r.warnings.push(format!(
                "MPEG-PS of {} joined parts (its clock starts over at each): {:.1} s in all, ffprobe says {:.1} s",
                parts.len(),
                ms as f64 / 1e3,
                r.duration_ms.unwrap_or(0) as f64 / 1e3
            ));
            r.duration_ms = Some(ms);
            if let Some(b) = r.bitrate.as_mut().filter(|_| ms > 0) {
                *b = src.size() * 8 * 1000 / ms;
            }
        }
        return;
    }
    let Some(end) = packet_end(&mut src, r.container) else {
        return;
    };
    let (ms, warning) = corrected_ms(r.duration_ms, &end, r.container);
    if let Some(w) = warning {
        r.warnings.push(w);
    }
    if ms != r.duration_ms {
        r.duration_ms = ms;
        if let (Some(ms), Some(_)) = (ms.filter(|&m| m > 0), r.bitrate) {
            let size = src.size();
            r.bitrate = Some(size * 8 * 1000 / ms);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An in-memory source.
    struct Mem(Vec<u8>);
    impl RangeRead for Mem {
        fn size(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&mut self, off: u64, buf: &mut [u8]) -> std::io::Result<usize> {
            let off = off as usize;
            let n = buf.len().min(self.0.len().saturating_sub(off));
            buf[..n].copy_from_slice(&self.0[off..off + n]);
            Ok(n)
        }
    }

    /// An FLV file: header, onMetaData, then one video tag every 40 ms and
    /// one audio tag every 26 ms up to `ms`.
    pub(crate) fn flv(ms: u32) -> Vec<u8> {
        let mut b = b"FLV\x01\x05\x00\x00\x00\x09\x00\x00\x00\x00".to_vec();
        let tag = |b: &mut Vec<u8>, kind: u8, ts: u32, data: &[u8]| {
            let size = data.len() as u32;
            b.push(kind);
            b.extend_from_slice(&size.to_be_bytes()[1..]);
            b.extend_from_slice(&(ts & 0xFF_FFFF).to_be_bytes()[1..]);
            b.push((ts >> 24) as u8);
            b.extend_from_slice(&[0, 0, 0]);
            b.extend_from_slice(data);
            b.extend_from_slice(&(size + 11).to_be_bytes());
        };
        tag(
            &mut b,
            18,
            0,
            &[
                2, 0, 10, b'o', b'n', b'M', b'e', b't', b'a', b'D', b'a', b't', b'a',
            ],
        );
        let (mut v, mut a) = (0u32, 0u32);
        while v <= ms || a <= ms {
            if v <= a {
                tag(&mut b, 9, v, &[0x24; 300]);
                v += 40;
            } else {
                tag(&mut b, 8, a, &[0x2F; 100]);
                a += 26;
            }
        }
        b
    }

    #[test]
    fn flv_ends_at_its_last_whole_tag() {
        let b = flv(60_000);
        let end = flv_end(&mut Mem(b.clone())).unwrap();
        assert_eq!((end.first_ms, end.last_ms), (0, 60_000));
        // Cut in the middle of a tag (a download that stopped): the last
        // whole tag before the cut.
        let cut = &b[..b.len() * 7 / 10 + 5];
        let end = flv_end(&mut Mem(cut.to_vec())).unwrap();
        assert!((41_900..=42_100).contains(&end.last_ms), "{end:?}");
        // Garbage after the last tag.
        let mut junk = b.clone();
        junk.extend_from_slice(&[0xAB; 777]);
        assert_eq!(flv_end(&mut Mem(junk)).unwrap().last_ms, 60_000);
        // Not an FLV.
        assert_eq!(flv_end(&mut Mem(vec![0; 4096])), None);
    }

    #[test]
    fn durations_follow_the_packets() {
        let flv_end = |last| PacketEnd {
            last_ms: last,
            first_ms: 0,
            cut: false,
        };
        // A cut FLV: header 300 s, tags to 42.35 s.
        let (ms, w) = corrected_ms(Some(300_000), &flv_end(42_350), Container::Flv);
        assert_eq!(ms, Some(42_350));
        assert!(w.unwrap().contains("300.0 s"));
        // Within a second: the header stands.
        assert_eq!(
            corrected_ms(Some(60_500), &flv_end(60_000), Container::Flv),
            (Some(60_500), None)
        );
        // No header duration.
        assert_eq!(
            corrected_ms(None, &flv_end(60_000), Container::Flv).0,
            Some(60_000)
        );
        // ASF: only a cut Data Object shortens it, never lengthens it.
        let asf = |last, cut| PacketEnd {
            last_ms: last,
            first_ms: 0,
            cut,
        };
        assert_eq!(
            corrected_ms(Some(600_000), &asf(400_000, true), Container::Asf).0,
            Some(400_000)
        );
        assert_eq!(
            corrected_ms(Some(600_000), &asf(400_000, false), Container::Asf).0,
            Some(600_000)
        );
        assert_eq!(
            corrected_ms(Some(600_000), &asf(700_000, true), Container::Asf).0,
            Some(600_000)
        );
        assert_eq!(
            corrected_ms(Some(600_000), &asf(599_500, true), Container::Asf).0,
            Some(600_000)
        );
    }

    /// An ASF data packet with multiple payloads (as WMV encoders write
    /// them): error correction, then payloads with 8 bytes of replicated
    /// data each (object size, presentation time).
    fn asf_packet(len: usize, times: &[u32]) -> Vec<u8> {
        let mut b = vec![0x82, 0x00, 0x00];
        // Length type flags: multiple payloads, padding length as a WORD;
        // property flags: replicated data length BYTE, offset DWORD, media
        // object number BYTE, stream number BYTE.
        b.extend_from_slice(&[0x11, 0x5D]);
        let pad_at = b.len();
        b.extend_from_slice(&[0, 0]); // padding length, set below
        b.extend_from_slice(&1000u32.to_le_bytes()); // send time
        b.extend_from_slice(&40u16.to_le_bytes()); // duration
        b.push(0x80 | times.len() as u8); // payload flags: WORD lengths
        for (i, t) in times.iter().enumerate() {
            b.push(0x82); // keyframe of stream 2
            b.push(i as u8); // media object number
            b.extend_from_slice(&0u32.to_le_bytes()); // offset into media object
            b.push(8); // replicated data length
            b.extend_from_slice(&100u32.to_le_bytes());
            b.extend_from_slice(&t.to_le_bytes());
            b.extend_from_slice(&100u16.to_le_bytes());
            b.extend_from_slice(&[0x55; 100]);
        }
        let pad = len - b.len();
        b[pad_at..pad_at + 2].copy_from_slice(&(pad as u16).to_le_bytes());
        b.resize(len, 0);
        b
    }

    fn guid_object(guid: &[u8; 16], body: &[u8]) -> Vec<u8> {
        let mut b = guid.to_vec();
        b.extend_from_slice(&((24 + body.len()) as u64).to_le_bytes());
        b.extend_from_slice(body);
        b
    }

    /// An ASF file of `packets` 1000-byte packets 400 ms apart, its Data
    /// Object stating `declared` packets.
    fn asf(packets: u32, declared: u32) -> Vec<u8> {
        let mut fp = vec![0u8; 104 - 24];
        // Offsets below are from the object start (24-byte header).
        let put = |fp: &mut Vec<u8>, at: usize, v: &[u8]| fp[at - 24..at - 24 + v.len()].copy_from_slice(v);
        put(&mut fp, 80, &3000u64.to_le_bytes()); // preroll, ms
        put(&mut fp, 92, &1000u32.to_le_bytes());
        put(&mut fp, 96, &1000u32.to_le_bytes());
        let fpo = guid_object(&ASF_FILE_PROPERTIES, &fp);
        let mut header_body = 1u32.to_le_bytes().to_vec();
        header_body.extend_from_slice(&[1, 2]);
        header_body.extend_from_slice(&fpo);
        let mut b = guid_object(&ASF_HEADER, &header_body);
        let mut data = ASF_DATA.to_vec();
        data.extend_from_slice(&(50 + u64::from(declared) * 1000).to_le_bytes());
        data.extend_from_slice(&[0; 16]);
        data.extend_from_slice(&u64::from(declared).to_le_bytes());
        data.extend_from_slice(&[1, 1]);
        b.extend_from_slice(&data);
        for k in 0..packets {
            let t = 3000 + k * 400;
            b.extend_from_slice(&asf_packet(1000, &[t, t + 40]));
        }
        b
    }

    /// An MPEG-2 PES packet of `sid` with a PTS, then `len` bytes.
    fn pes(sid: u8, pts: u64, len: usize) -> Vec<u8> {
        let mut b = vec![0, 0, 1, 0xBA];
        b.extend_from_slice(&[0x44, 0, 4, 0, 4, 1, 1, 0x89, 0xC3, 0xF8]); // pack header body
        b.extend_from_slice(&[0, 0, 1, sid]);
        b.extend_from_slice(&((len + 8) as u16).to_be_bytes());
        b.extend_from_slice(&[0x81, 0x80, 5]);
        b.push(0x21 | (((pts >> 30) & 7) as u8) << 1);
        b.push((pts >> 22) as u8);
        b.push((((pts >> 15) & 0x7F) as u8) << 1 | 1);
        b.push((pts >> 7) as u8);
        b.push(((pts & 0x7F) as u8) << 1 | 1);
        b.extend(std::iter::repeat_n(0x55u8, len));
        b
    }

    /// `secs` of 25 fps video (and audio) packets from `start` (90 kHz).
    fn ps(start: u64, secs: u64) -> Vec<u8> {
        let mut b = Vec::new();
        for f in 0..secs * 25 {
            let t = start + f * 3600;
            b.extend(pes(0xE0, t, 2000));
            b.extend(pes(0xC0, t + 900, 300));
        }
        b
    }

    #[test]
    fn joined_program_streams_add_up() {
        // One recording: one part, its span.
        let one = ps(90_000, 600);
        let parts = ps_parts(&mut Mem(one)).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].span_ms(), 599_960);
        // Two recordings joined: 400 s, then a clock from 0.5 s for 300 s.
        let mut two = ps(90_000, 400);
        let join = two.len() as u64;
        two.extend(ps(45_000, 300));
        let parts = ps_parts(&mut Mem(two)).unwrap();
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(parts[1].offset, join);
        assert_eq!(parts[0].span_ms(), 399_960);
        assert_eq!(parts[1].span_ms(), 299_960);
        // A 33-bit wrap is not a join.
        let wrapped = ps(WRAP_33 - 90_000 * 100, 300);
        let parts = ps_parts(&mut Mem(wrapped)).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].span_ms(), 299_960);
    }

    #[test]
    fn a_cut_asf_ends_at_its_last_whole_packet() {
        // 100 of 250 packets, plus half of the 101st.
        let mut b = asf(101, 250);
        b.truncate(b.len() - 500);
        let end = asf_end(&mut Mem(b)).unwrap();
        // Packet 99: 3000 + 99 × 400 + 40 ms, minus the 3000 ms preroll.
        assert_eq!(
            end,
            PacketEnd {
                last_ms: 39_640,
                first_ms: 0,
                cut: true
            }
        );
        // Complete: the header holds.
        assert_eq!(asf_end(&mut Mem(asf(250, 250))), None);
    }
}
