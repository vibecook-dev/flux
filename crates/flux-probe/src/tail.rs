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
//! decoded. Two or three small reads per FLV or ASF file, ~45 per MPEG-PS.

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

/// The largest ASF packet size taken at the header's word. Encoders write
/// packets of a few KiB (tens at most); ffmpeg accepts anything below
/// 512 MiB, so a corrupt or hostile header could otherwise make each read
/// below as large as the file.
const ASF_MAX_PACKET: u64 = 1 << 20;

/// For an ASF file whose Data Object runs past the end of the file, where
/// the whole packets end; `None` for a complete file (its header holds) or
/// one that doesn't parse. Reads the header and at most 512 KiB (or one
/// packet) of the end.
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
    if min_packet != max_packet || min_packet == 0 || min_packet > ASF_MAX_PACKET {
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
    // The last whole packets, as many as the window holds (at least one,
    // 64 at most): the latest time among them.
    let from = whole.saturating_sub((WINDOW / min_packet).clamp(1, 64));
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
    /// Where ffmpeg starts its clock, 90 kHz: the earliest of each
    /// stream's first timestamp (`ps_start`), so `first_pts` of a part read
    /// alone is its start time.
    pub first_pts: u64,
    /// Its latest timestamp of any stream, 90 kHz (unwrapped past a 33-bit
    /// wrap).
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
/// Bytes read in one go where a part starts (the file's start, a join): a
/// part shorter than this shows its clock going back inside them.
const PS_HEAD: u64 = 1 << 20;
/// Places sampled for a clock that goes back.
const PS_SAMPLES: u64 = 32;
/// Parts looked for at most (each costs ~25 reads).
const PS_MAX_PARTS: usize = 256;
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

/// A PES packet with a timestamp: (offset in the file, PTS, stream id).
type PsTs = (u64, u64, u8);

/// The substream of a private stream 1 packet (MPEG-2 PES header): DVD
/// audio (0x80–0xAF) or subpictures (0x20–0x3F).
fn private_substream(p: &[u8]) -> Option<u8> {
    (*p.get(6)? & 0xC0 == 0x80)
        .then(|| p.get(9 + usize::from(*p.get(8)?)).copied())
        .flatten()
}

/// The PES packets with a timestamp in `b`, a window of the file at `at`:
/// video, MPEG audio and private stream 1 (AC-3, DTS, LPCM on DVDs; not
/// its subpictures, whose times run ahead of the rest). Packets are walked
/// by their lengths from the window's first pack header, as a demuxer
/// does, so a start code inside a payload is not taken for a packet;
/// where the walk loses step it resumes at the next pack header.
fn ps_timestamps(b: &[u8], at: u64) -> Vec<PsTs> {
    let pack_from = |from: usize| {
        b.get(from..)?
            .windows(4)
            .position(|w| w == [0, 0, 1, 0xBA])
            .map(|k| from + k)
    };
    let mut out = Vec::new();
    let mut i = pack_from(0);
    while let Some(p) = i.filter(|&p| p + 6 <= b.len()) {
        let len = match (&b[p..p + 3], b[p + 3]) {
            ([0, 0, 1], 0xBA) => match b[p + 4] {
                x if x & 0xC0 == 0x40 => b.get(p + 13).map(|s| 14 + usize::from(s & 7)),
                x if x & 0xF0 == 0x20 => Some(12),
                _ => None,
            },
            ([0, 0, 1], 0xB9) => Some(4),
            ([0, 0, 1], sid) if sid >= 0xBB => {
                Some(6 + usize::from(u16::from_be_bytes([b[p + 4], b[p + 5]])))
            }
            _ => None,
        };
        let Some(len) = len.filter(|&l| l >= 4) else {
            i = pack_from(p + 1);
            continue;
        };
        let sid = b[p + 3];
        let wanted = match sid {
            0xBD => !private_substream(&b[p..]).is_some_and(|s| (0x20..=0x3F).contains(&s)),
            s => is_video(s) || (0xC0..=0xDF).contains(&s),
        };
        if let Some(pts) = wanted.then(|| pes_pts(&b[p..])).flatten() {
            out.push((at + p as u64, pts, sid));
        }
        i = Some(p + len);
    }
    out
}

/// The timestamps of one window that a neighbour of the same stream backs
/// up (within 10 s of it, either way). A lone packet, such as the first of
/// a recording cut off at the end of the file, or a corrupt timestamp,
/// tells nothing about the clock.
fn backed(ts: Vec<PsTs>) -> Vec<PsTs> {
    let near = |a: u64, b: u64| {
        let d = a.abs_diff(b) % WRAP_33;
        d.min(WRAP_33 - d) <= 10 * 90_000
    };
    let mut prev = [None::<usize>; 256];
    let mut ok = vec![false; ts.len()];
    for (k, t) in ts.iter().enumerate() {
        let s = usize::from(t.2);
        if let Some(j) = prev[s].filter(|&j| near(ts[j].1, t.1)) {
            (ok[j], ok[k]) = (true, true);
        }
        prev[s] = Some(k);
    }
    ts.into_iter()
        .zip(ok)
        .filter_map(|(t, ok)| ok.then_some(t))
        .collect()
}

fn is_video(sid: u8) -> bool {
    (0xE0..=0xEF).contains(&sid)
}

/// Where ffmpeg starts the clock of packets that begin a stretch (in file
/// order): the earliest of each stream's *first* timestamp, as it takes a
/// stream's start time from its first packet that has one. (Not the
/// smallest timestamp: B-frames after the first I-frame come before it.)
fn ps_start<'a>(ts: impl IntoIterator<Item = &'a PsTs>) -> Option<u64> {
    let mut seen = [false; 256];
    let mut start = None::<u64>;
    for &(_, p, sid) in ts {
        if !std::mem::replace(&mut seen[usize::from(sid)], true) {
            start = Some(start.map_or(p, |s| s.min(p)));
        }
    }
    start
}

/// A clock as the file is read: each stream's latest timestamp, unwrapped
/// past 33-bit wraps. Streams are followed one by one: audio is multiplexed
/// up to about a second behind its video, and either may be sparse.
#[derive(Clone)]
struct Clock {
    top: [Option<u64>; 256],
    epoch: u64,
}

impl Clock {
    fn new() -> Self {
        Clock {
            top: [None; 256],
            epoch: 0,
        }
    }

    /// `p` of stream `sid` on this clock: unwrapped, with the wraps so far
    /// (`Some`), or `None` when it is more than a second behind that
    /// stream's latest (another clock).
    fn place(&self, sid: u8, p: u64) -> Option<(u64, u64)> {
        let (mut u, mut epoch) = (p + self.epoch * WRAP_33, self.epoch);
        let Some(top) = self.top[usize::from(sid)] else {
            return Some((u, epoch));
        };
        if u + WRAP_33 / 2 < top {
            // The clock wrapped.
            (u, epoch) = (u + WRAP_33, epoch + 1);
        } else if u > top + WRAP_33 / 2 && u >= WRAP_33 {
            // From before a wrap another stream has crossed.
            u -= WRAP_33;
        }
        (u + 90_000 >= top).then_some((u, epoch))
    }

    /// Moves the clock on to `p` of `sid` ([`Clock::place`]'s answer).
    fn take(&mut self, sid: u8, (u, epoch): (u64, u64)) {
        self.epoch = self.epoch.max(epoch);
        let t = &mut self.top[usize::from(sid)];
        *t = Some(t.map_or(u, |t| t.max(u)));
    }

    /// Places `t` and moves the clock on to it; `false` when it is behind.
    fn feed(&mut self, t: &PsTs) -> bool {
        match self.place(t.2, t.1) {
            Some(x) => {
                self.take(t.2, x);
                true
            }
            None => false,
        }
    }

    /// The latest timestamp of any stream.
    fn latest(&self) -> Option<u64> {
        self.top.iter().flatten().max().copied()
    }
}

/// The backed timestamps ([`backed`]) of the first window at or after
/// `from`, before `to`, that has any (up to 1 MiB on).
fn timestamps_from<R: RangeRead>(src: &mut R, from: u64, to: u64) -> Vec<PsTs> {
    let mut off = from;
    while off < to.min(from + PS_HEAD) {
        let Some(b) = read(src, off, PS_WINDOW.min(to - off)) else {
            break;
        };
        let ts: Vec<PsTs> = backed(ps_timestamps(&b, off))
            .into_iter()
            .filter(|t| t.0 < to)
            .collect();
        if !ts.is_empty() {
            return ts;
        }
        off += PS_WINDOW;
    }
    Vec::new()
}

/// Where between `lo` (the latest packet of `old`, the clock so far) and
/// `hi` (a packet behind it) a new clock starts: the pack header before its
/// first packet, the old clock's latest timestamp, and where ffmpeg starts
/// the new one ([`ps_start`]).
fn ps_join<R: RangeRead>(src: &mut R, old: &Clock, lo: u64, hi: u64) -> Option<(u64, u64, u64)> {
    let (mut old, mut lo, mut hi) = (old.clone(), lo, hi);
    while hi - lo > PS_WINDOW {
        let mid = lo + (hi - lo) / 2;
        let mut c = old.clone();
        let (mut last_old, mut first_new) = (None, None);
        for t in &timestamps_from(src, mid, hi) {
            if !c.feed(t) {
                first_new = Some(t.0);
                break;
            }
            last_old = Some(t.0);
        }
        if let Some(l) = last_old {
            (lo, old) = (l, c);
        }
        hi = match (first_new, last_old) {
            (Some(n), _) => n,
            (None, Some(_)) => hi,
            // Nothing between `mid` and `hi` tells the clocks apart.
            (None, None) => mid,
        };
    }
    let buf = read(src, lo, hi - lo + 2 * PS_WINDOW)?;
    let ts = backed(ps_timestamps(&buf, lo));
    let mut c = old;
    let i = ts.iter().position(|t| !c.feed(t))?;
    let at = ts[i].0;
    let rel = (at - lo) as usize;
    let pack = buf[..rel]
        .windows(4)
        .rposition(|w| w == [0, 0, 1, 0xBA])
        .map_or(at, |k| lo + k as u64);
    // The new clock's packets: those behind the old clock, and those of
    // streams it didn't have.
    let new = ts[i..]
        .iter()
        .filter(|t| c.top[usize::from(t.2)].is_none() || c.place(t.2, t.1).is_none());
    let start = ps_start(new).unwrap_or(ts[i].1);
    Some((pack, c.latest()?, start))
}

/// The parts of an MPEG-PS file whose clock restarts (see [`PsPart`]), in
/// file order; one part for a file whose clock runs on. `None` if no
/// timestamps are found.
///
/// The clock is read at 32 places through the file and at the end. A part
/// shorter than that step could hide between two places (the next one's
/// clock has already passed it), so where each part starts the first MiB
/// is read whole, then places half again as far each time, up to the next
/// regular place: about 45 reads of 128 KiB for a 2 GB file, plus ~25 per
/// restart.
pub fn ps_parts<R: RangeRead>(src: &mut R) -> Option<Vec<PsPart>> {
    let size = src.size();
    let regular: Vec<u64> = (1..PS_SAMPLES).map(|k| size * k / PS_SAMPLES).collect();
    let mut parts: Vec<PsPart> = Vec::new();
    let mut start = 0u64;
    'part: while parts.len() < PS_MAX_PARTS {
        let next = regular.iter().copied().find(|&o| o > start).unwrap_or(size);
        let mut windows = vec![(start, PS_HEAD)];
        let mut d = PS_HEAD;
        while start + d < next {
            windows.push((start + d, PS_WINDOW));
            d += d / 2;
        }
        windows.extend(regular.iter().filter(|&&o| o >= next).map(|&o| (o, PS_WINDOW)));
        windows.push((size.saturating_sub(2 * PS_WINDOW), 2 * PS_WINDOW));
        // This part's clock, and where its latest packet is.
        let (mut clock, mut latest_at) = (Clock::new(), start);
        let mut covered = start;
        for (off, len) in windows {
            let (off, end) = (off.max(covered), (off + len).min(size));
            if end <= off {
                continue;
            }
            covered = end;
            let Some(b) = read(src, off, end - off) else {
                continue;
            };
            let ts = backed(ps_timestamps(&b, off));
            if parts.is_empty() {
                // The file's start: its first clock, up to a restart in
                // this first MiB.
                let mut c = Clock::new();
                let upto = ts.iter().position(|t| !c.feed(t)).unwrap_or(ts.len());
                let first = ps_start(&ts[..upto])?;
                parts.push(PsPart {
                    offset: 0,
                    first_pts: first,
                    last_pts: first,
                });
            }
            for t in &ts {
                match clock.place(t.2, t.1) {
                    Some(x) => {
                        clock.take(t.2, x);
                        latest_at = t.0;
                        if let Some(cur) = parts.last_mut() {
                            cur.last_pts = cur.last_pts.max(x.0);
                        }
                    }
                    None => match ps_join(src, &clock, latest_at, t.0) {
                        Some((pack, last, first)) if pack > start => {
                            if let Some(cur) = parts.last_mut() {
                                cur.last_pts = cur.last_pts.max(last);
                            }
                            parts.push(PsPart {
                                offset: pack,
                                first_pts: first,
                                last_pts: first,
                            });
                            start = pack;
                            continue 'part;
                        }
                        // No new clock starts there after all: this stream
                        // runs on from here.
                        _ => clock.top[usize::from(t.2)] = Some(t.1 + clock.epoch * WRAP_33),
                    },
                }
            }
        }
        break;
    }
    (!parts.is_empty()).then_some(parts)
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
        asf_of(packets, declared, 1000)
    }

    /// [`asf`] with packets of `len` bytes.
    fn asf_of(packets: u32, declared: u32, len: u32) -> Vec<u8> {
        let mut b = asf_head(declared, len);
        for k in 0..packets {
            let t = 3000 + k * 400;
            b.extend_from_slice(&asf_packet(len as usize, &[t, t + 40]));
        }
        b
    }

    /// The Header Object and the Data Object's header of an ASF file of
    /// `declared` packets of `len` bytes.
    fn asf_head(declared: u32, len: u32) -> Vec<u8> {
        let mut fp = vec![0u8; 104 - 24];
        // Offsets below are from the object start (24-byte header).
        let put = |fp: &mut Vec<u8>, at: usize, v: &[u8]| fp[at - 24..at - 24 + v.len()].copy_from_slice(v);
        put(&mut fp, 80, &3000u64.to_le_bytes()); // preroll, ms
        put(&mut fp, 92, &len.to_le_bytes());
        put(&mut fp, 96, &len.to_le_bytes());
        let fpo = guid_object(&ASF_FILE_PROPERTIES, &fp);
        let mut header_body = 1u32.to_le_bytes().to_vec();
        header_body.extend_from_slice(&[1, 2]);
        header_body.extend_from_slice(&fpo);
        let mut b = guid_object(&ASF_HEADER, &header_body);
        let mut data = ASF_DATA.to_vec();
        data.extend_from_slice(&(50 + u64::from(declared) * u64::from(len)).to_le_bytes());
        data.extend_from_slice(&[0; 16]);
        data.extend_from_slice(&u64::from(declared).to_le_bytes());
        data.extend_from_slice(&[1, 1]);
        b.extend_from_slice(&data);
        b
    }

    /// A file of `size` bytes, `head` then zeros, that records its longest
    /// read (the buffer a caller allocated for it).
    struct Sparse {
        head: Vec<u8>,
        size: u64,
        longest: usize,
    }
    impl RangeRead for Sparse {
        fn size(&self) -> u64 {
            self.size
        }
        fn read_at(&mut self, off: u64, buf: &mut [u8]) -> std::io::Result<usize> {
            self.longest = self.longest.max(buf.len());
            let n = buf.len().min(self.size.saturating_sub(off) as usize);
            for (k, x) in buf[..n].iter_mut().enumerate() {
                *x = self.head.get(off as usize + k).copied().unwrap_or(0);
            }
            Ok(n)
        }
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

    /// An MPEG-2 PES packet of `sid` without a timestamp, then `len` bytes.
    fn pes_untimed(sid: u8, len: usize) -> Vec<u8> {
        let mut b = vec![0, 0, 1, sid];
        b.extend_from_slice(&((len + 3) as u16).to_be_bytes());
        b.extend_from_slice(&[0x81, 0x00, 0]);
        b.extend(std::iter::repeat_n(0x55u8, len));
        b
    }

    /// [`ps`] whose video has a timestamp only every 100th frame (4 s).
    fn ps_sparse(start: u64, secs: u64) -> Vec<u8> {
        let mut b = Vec::new();
        for f in 0..secs * 25 {
            let t = start + f * 3600;
            if f % 100 == 0 || f == secs * 25 - 1 {
                b.extend(pes(0xE0, t, 2000));
            } else {
                b.extend(pes_untimed(0xE0, 2000));
            }
            b.extend(pes(0xC0, t + 900, 300));
        }
        b
    }

    /// [`ps`] with its audio multiplexed `lag` frames behind its video: the
    /// last of the audio comes after the last video.
    fn ps_lagging(start: u64, secs: u64, lag: u64) -> Vec<u8> {
        let (n, mut b) = (secs * 25, Vec::new());
        for f in 0..n + lag {
            if f < n {
                b.extend(pes(0xE0, start + f * 3600, 2000));
            }
            if f >= lag {
                b.extend(pes(0xC0, start + (f - lag) * 3600 + 900, 300));
            }
        }
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
        assert_eq!(parts[0].span_ms(), 599_970);
        // Two recordings joined: 400 s, then a clock from 0.5 s for 300 s.
        let mut two = ps(90_000, 400);
        let join = two.len() as u64;
        two.extend(ps(45_000, 300));
        let parts = ps_parts(&mut Mem(two)).unwrap();
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(parts[1].offset, join);
        assert_eq!(parts[0].span_ms(), 399_970);
        assert_eq!(parts[1].span_ms(), 299_970);
        // A 33-bit wrap is not a join.
        let wrapped = ps(WRAP_33 - 90_000 * 100, 300);
        let parts = ps_parts(&mut Mem(wrapped)).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].span_ms(), 299_970);
    }

    /// The offsets and spans of a file's parts.
    fn layout(b: Vec<u8>) -> (Vec<u64>, Vec<u64>) {
        let parts = ps_parts(&mut Mem(b)).unwrap();
        (
            parts.iter().map(|p| p.offset).collect(),
            parts.iter().map(PsPart::span_ms).collect(),
        )
    }

    /// A part shorter than the step between the places sampled (1/32 of
    /// the file): the next place's clock has already passed it, so it was
    /// taken for the start of the next part (2 s + 300 s read as one 300 s
    /// part).
    #[test]
    fn a_part_shorter_than_the_sampling_step_is_found() {
        // 2 s, then 300 s: both inside the first MiB read.
        let mut b = ps(90_000, 2);
        let join = b.len() as u64;
        b.extend(ps(45_000, 300));
        assert_eq!(layout(b), (vec![0, join], vec![1_970, 299_970]));
        // 3 s between two long ones.
        let mut b = ps(90_000, 400);
        let j1 = b.len() as u64;
        b.extend(ps(45_000, 3));
        let j2 = b.len() as u64;
        b.extend(ps(45_000, 300));
        assert_eq!(layout(b), (vec![0, j1, j2], vec![399_970, 2_970, 299_970]));
        // 25 s (1.5 MB: past the first MiB, short of the 1.7 MB step), then
        // 900 s: a place between the two finds it.
        let mut b = ps(90_000, 25);
        let join = b.len() as u64;
        b.extend(ps(45_000, 900));
        assert!(join > PS_HEAD && join < b.len() as u64 / PS_SAMPLES);
        assert_eq!(layout(b), (vec![0, join], vec![24_970, 899_970]));
    }

    /// Video whose timestamps are sparse (one every 4 s, 240 KB apart, as
    /// in a real DVD capture with 201 in 66 MB): the clock is followed by
    /// every stream, so the audio's shows the join. A packet alone on a
    /// clock (the first of a recording cut off at the end) and corrupt
    /// timestamps (5,000 s ahead, or 0) tell nothing: no part of them.
    #[test]
    fn joins_are_found_by_any_stream_and_strays_are_not_joins() {
        let mut b = ps_sparse(90_000, 300);
        let join = b.len() as u64;
        b.extend(ps_sparse(45_000, 200));
        assert_eq!(layout(b), (vec![0, join], vec![299_970, 199_970]));
        let mut b = ps(90_000, 300);
        b.extend(pes(0xE0, 5_400, 2000));
        assert_eq!(layout(b), (vec![0], vec![299_970]));
        // Both inside the first MiB, which is read whole.
        let mut b = ps(90_000, 4);
        b.extend(pes(0xE0, 90_000 + 5_000 * 90_000, 2000));
        b.extend(ps(90_000 + 100 * 3_600, 12));
        b.extend(pes(0xE0, 0, 2000));
        b.extend(ps(90_000 + 400 * 3_600, 284));
        assert_eq!(layout(b), (vec![0], vec![299_970]));
    }

    /// Where a part's clock starts is where ffmpeg starts it when it reads
    /// the part alone: the earliest of each stream's first timestamp. Here
    /// the I-frame comes first at 0.58 s, its audio and B-frames from
    /// 0.5 s: 0.5 s, not 0.58 s. And audio multiplexed 1.2 s behind its
    /// video, the last of it after the first part's last video, still
    /// belongs to that part: the join is where the second part's packets
    /// start.
    #[test]
    fn a_part_starts_where_ffmpeg_starts_its_clock() {
        let mut b = ps_lagging(90_000, 400, 30);
        let join = b.len() as u64;
        b.extend(pes(0xE0, 52_200, 2000)); // the I-frame
        b.extend(pes(0xC0, 45_000, 300));
        b.extend(pes(0xE0, 45_000, 2000)); // B-frames
        b.extend(pes(0xE0, 48_600, 2000));
        b.extend(ps(55_800, 300));
        let parts = ps_parts(&mut Mem(b)).unwrap();
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(parts[1].offset, join);
        assert_eq!(parts[1].first_pts, 45_000);
        assert_eq!(parts[0].span_ms(), 399_970);
        assert_eq!(parts[1].span_ms(), (55_800 + 7_499 * 3_600 + 900 - 45_000) / 90);
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
        // 64 KiB packets: the last 8 whole ones (the 512 KiB window).
        let mut b = asf_of(16, 250, 65_536);
        b.truncate(b.len() - 30_000);
        let mut src = Sparse {
            size: b.len() as u64,
            head: b,
            longest: 0,
        };
        assert_eq!(asf_end(&mut src).map(|e| e.last_ms), Some(14 * 400 + 40));
        assert!(src.longest <= WINDOW as usize, "{}", src.longest);
    }

    /// A header that declares huge packets (corrupt or hostile: ffmpeg
    /// accepts up to 512 MiB) is not taken at its word: no read of up to 64
    /// such packets (here 19 of 16 MiB, 304 MiB in one buffer).
    #[test]
    fn an_asf_header_cannot_size_the_reads() {
        let mut src = Sparse {
            head: asf_head(1_000, 16 << 20),
            size: 20 * (16 << 20),
            longest: 0,
        };
        assert_eq!(asf_end(&mut src), None);
        assert!(src.longest <= 1 << 20, "{}", src.longest);
    }
}
