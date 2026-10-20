//! Builds the playback index of an MPEG-TS/M2TS file in one sequential pass.
//!
//! 1. **Head** (256 KB, then 1 MB, as flux-probe reads it): PAT → PMT → the
//!    track list, classified and ordered exactly as flux-probe does, so the
//!    catalog's track numbers address the same tracks here.
//! 2. **Scan** (8 MB sequential reads to the end): for the video PID, every
//!    PES header (PTS/DTS) and the start of every access unit up to its first
//!    slice (H.264 IDR or I slice with a recovery point / random access
//!    indicator, HEVC IRAP); for audio PIDs, PES timestamps. Continuity
//!    counters are checked, timestamps unwrapped (33 bits) and jumps
//!    recorded as discontinuities.
//! 3. **Timeline**: discontinuities are removed (each later epoch is shifted
//!    to continue where the previous one ended), the origin is ffmpeg's
//!    `start_time` (the earliest first timestamp of the audio/video streams),
//!    and the frame rate is exact when the stream is constant-rate.
//!
//! Anything the remux can't reproduce faithfully returns `Unsupported`, and
//! the caller falls back to the ffprobe index (ffmpeg transcode paths).

use std::collections::HashMap;

use flux_core::{Codec, ColorInfo, Container, HdrInfo, TrackKind};

use super::audio::{self as taudio};
use super::packet::{self, Cc, Continuity, Framing, Packet};
use super::pes::{self, unwrap_near};
use super::psi::{self, Es, Section};
use super::video::{self, AuInfo, Flavor, ParamCtx};
use super::{unsupported, Discontinuity, TsLayout};
use crate::codec::{self, h264, hevc};
use crate::error::{HlsError, Result};
use crate::index::{
    average_rate, classify_hdr, merge_color, rational_rate, ticks_to_ns, IndexMethod, IndexStats, IndexTrack,
    Keyframe, Layout, PlaybackIndex, INDEX_VERSION,
};
use crate::source::Source;

const HEAD: u64 = 256 << 10;
const HEAD_MAX: u64 = 1 << 20;
const AUDIO_KEEP: usize = 16 << 10;
const SCAN_CHUNK: usize = 8 << 20;
/// Video PES whose pictures are counted: the remux needs one per PES.
const CHECK_PES: usize = 120;
/// Bytes of an access unit inspected for its first slice.
const INSPECT_MAX: usize = 64 << 10;
/// Timestamp steps (90 kHz) that are discontinuities, not gaps: back by more
/// than 0.5 s, or forward by more than 10 s (ffmpeg's `dts_delta_threshold`).
const JUMP_BACK: i64 = 45_000;
const JUMP_FWD: i64 = 900_000;

// ---------------------------------------------------------------- head

#[derive(Debug, Default)]
struct HeadStream {
    es: Es,
    started: bool,
    first_pts: Option<u64>,
    data: Vec<u8>,
}

#[derive(Debug, Default)]
struct Head {
    pmt_pids: Vec<u16>,
    pmts_done: Vec<u16>,
    hdmv: bool,
    streams: Vec<HeadStream>,
}

impl Head {
    fn complete(&self) -> bool {
        !self.pmt_pids.is_empty()
            && self.pmts_done.len() >= self.pmt_pids.len()
            && self.streams.iter().all(|s| s.started || !psi::could_be_av(&s.es))
    }
}

/// The first bytes of the file as flux-probe reads them.
fn scan_head(buf: &[u8], f: Framing, first: usize) -> Head {
    let mut head = Head::default();
    let mut pat = Section::default();
    let mut pmts: HashMap<u16, Section> = HashMap::new();
    let mut pat_done = false;
    packet::walk(&buf[first..], first as u64, f, |pk| {
        if pk.pid == 0 {
            if !pat_done {
                if let Some(s) = pat.push(pk) {
                    head.pmt_pids = psi::parse_pat(&s);
                    pat_done = true;
                }
            }
            return true;
        }
        if head.pmt_pids.contains(&pk.pid) {
            if !head.pmts_done.contains(&pk.pid) {
                if let Some(s) = pmts.entry(pk.pid).or_default().push(pk) {
                    if let Some((_, hdmv, _, streams)) = psi::parse_pmt(&s) {
                        head.hdmv |= hdmv;
                        for es in streams {
                            if !head.streams.iter().any(|h| h.es.pid == es.pid) {
                                head.streams.push(HeadStream {
                                    es,
                                    ..Default::default()
                                });
                            }
                        }
                    }
                    head.pmts_done.push(pk.pid);
                }
            }
            return true;
        }
        let Some(hs) = head.streams.iter_mut().find(|h| h.es.pid == pk.pid) else {
            return true;
        };
        let video = matches!(hs.es.stream_type, 0x01 | 0x02 | 0x10 | 0x1B | 0x24 | 0xEA);
        let keep = if video { 512 << 10 } else { AUDIO_KEEP };
        if pk.pusi {
            if let Some(h) = pes::parse_header(pk.payload) {
                if hs.first_pts.is_none() {
                    hs.first_pts = h.pts;
                }
                hs.started = true;
                if hs.data.len() < keep {
                    hs.data
                        .extend_from_slice(pk.payload.get(h.header_len..).unwrap_or(&[]));
                }
            }
        } else if hs.started && hs.data.len() < keep {
            hs.data.extend_from_slice(pk.payload);
        }
        true
    });
    head
}

// ---------------------------------------------------------------- clocks

/// Unwraps one PID's timestamps and records its jumps.
#[derive(Debug, Default)]
struct Clock {
    last: Option<i64>,
    /// Last forward step (the frame spacing).
    step: i64,
    jumps: Vec<Jump>,
}

#[derive(Debug, Clone, Copy)]
struct Jump {
    /// File offset of the first PES after the jump.
    pos: u64,
    /// Where the timestamps would have continued.
    expected: i64,
    /// Where they went.
    to: i64,
}

impl Clock {
    fn next(&mut self, raw: u64, global: i64, pos: u64) -> i64 {
        let u = unwrap_near(raw, self.last.unwrap_or(global));
        if let Some(l) = self.last {
            let d = u - l;
            if !(-JUMP_BACK..=JUMP_FWD).contains(&d) {
                self.jumps.push(Jump {
                    pos,
                    expected: l + self.step.max(0),
                    to: u,
                });
            } else if d > 0 {
                self.step = d;
            }
        }
        self.last = Some(u);
        u
    }
}

// ---------------------------------------------------------------- video

#[derive(Debug, Clone, Copy)]
struct VFrame {
    /// Unwrapped 90 kHz PTS (before discontinuity correction).
    pts: i64,
    pos: u64,
    key: bool,
    idr: bool,
    /// Video epoch (discontinuities before it).
    epoch: usize,
}

struct VPes {
    pos: u64,
    random_access: bool,
    raw: Vec<u8>,
    header: Option<pes::PesHeader>,
    info: Option<AuInfo>,
    give_up: bool,
    count_pictures: bool,
}

impl VPes {
    fn wants_bytes(&self) -> bool {
        self.header.is_none() || (self.info.is_none() && !self.give_up) || self.count_pictures
    }
}

struct VState {
    pid: u16,
    flavor: Flavor,
    cc: Continuity,
    lost: u32,
    scrambled: u64,
    clock: Clock,
    ctx: ParamCtx,
    cur: Option<VPes>,
    frames: Vec<VFrame>,
    pes_seen: usize,
    no_pts: usize,
    field: bool,
    multi_picture: usize,
    first_key: Option<AuInfo>,
    params: HashMap<(u8, u32), Vec<u8>>,
    params_changed: bool,
    payload_bytes: u64,
}

impl VState {
    fn new(pid: u16, flavor: Flavor) -> Self {
        VState {
            pid,
            flavor,
            cc: Continuity::default(),
            lost: 0,
            scrambled: 0,
            clock: Clock::default(),
            ctx: ParamCtx::default(),
            cur: None,
            frames: Vec::new(),
            pes_seen: 0,
            no_pts: 0,
            field: false,
            multi_picture: 0,
            first_key: None,
            params: HashMap::new(),
            params_changed: false,
            payload_bytes: 0,
        }
    }

    fn packet(&mut self, pk: &Packet<'_>, global: &mut Option<i64>) {
        match self.cc.check(pk) {
            Cc::Duplicate => return,
            Cc::Lost => self.lost += 1,
            Cc::Ok => {}
        }
        if pk.scrambled {
            self.scrambled += 1;
        }
        if !pk.has_payload {
            return;
        }
        self.payload_bytes += pk.payload.len() as u64;
        if pk.pusi {
            self.finish(global);
            self.cur = Some(VPes {
                pos: pk.pos,
                random_access: pk.random_access,
                raw: pk.payload.to_vec(),
                header: None,
                info: None,
                give_up: false,
                count_pictures: self.pes_seen < CHECK_PES,
            });
            self.pes_seen += 1;
            self.advance();
        } else if let Some(c) = self.cur.as_mut() {
            if c.wants_bytes() {
                c.raw.extend_from_slice(pk.payload);
                self.advance();
            }
        }
    }

    fn advance(&mut self) {
        let Some(c) = self.cur.as_mut() else { return };
        if c.header.is_none() {
            match pes::parse_header(&c.raw) {
                Some(h) => {
                    c.raw.drain(..h.header_len.min(c.raw.len()));
                    c.header = Some(h);
                }
                None if c.raw.len() > 300 || (c.raw.len() >= 3 && c.raw[..3] != [0, 0, 1]) => {
                    self.cur = None;
                    return;
                }
                None => return,
            }
        }
        if c.info.is_none() && !c.give_up {
            c.info = video::inspect(&c.raw, self.flavor, &mut self.ctx);
            if c.info.is_none() && c.raw.len() > INSPECT_MAX {
                c.give_up = true;
            }
        }
        if !c.count_pictures && (c.info.is_some() || c.give_up) {
            c.raw = Vec::new();
        }
    }

    fn finish(&mut self, global: &mut Option<i64>) {
        let Some(c) = self.cur.take() else { return };
        let Some(h) = c.header else { return };
        let Some(pts) = h.pts else {
            if !self.frames.is_empty() {
                self.no_pts += 1;
            }
            return;
        };
        let g = *global.get_or_insert(pts as i64);
        let clock = self.clock.next(h.dts.unwrap_or(pts), g, c.pos);
        let pts_u = unwrap_near(pts, clock);
        let info = c.info.unwrap_or_default();
        if info.field == Some(true) {
            self.field = true;
        }
        if c.count_pictures && video::picture_starts(&c.raw, self.flavor) != 1 {
            self.multi_picture += 1;
        }
        let key = match self.flavor {
            Flavor::H264 => info.idr || (info.intra && (info.recovery_point || c.random_access)),
            Flavor::Hevc => info.irap,
        };
        if key {
            for p in &info.params {
                if let Some(k) = video::parameter_set_key(p, self.flavor) {
                    match self.params.get(&k) {
                        Some(old) if old != p => self.params_changed = true,
                        Some(_) => {}
                        None => {
                            self.params.insert(k, p.clone());
                        }
                    }
                }
            }
            if self.first_key.is_none() {
                self.first_key = Some(info.clone());
            }
        }
        self.frames.push(VFrame {
            pts: pts_u,
            pos: c.pos,
            key,
            idr: info.idr,
            epoch: self.clock.jumps.len(),
        });
    }
}

// ---------------------------------------------------------------- audio

#[derive(Default)]
struct AState {
    pid: u16,
    cc: Continuity,
    lost: u32,
    clock: Clock,
    /// (unwrapped PTS, position) of the first PES with a timestamp.
    first: Option<(i64, u64)>,
    /// PES start bytes until the header is complete.
    hdr: Vec<u8>,
    pes_pos: u64,
    /// The first payload bytes (configuration records).
    data: Vec<u8>,
}

impl AState {
    fn packet(&mut self, pk: &Packet<'_>, global: &mut Option<i64>) {
        match self.cc.check(pk) {
            Cc::Duplicate => return,
            Cc::Lost => self.lost += 1,
            Cc::Ok => {}
        }
        if !pk.has_payload {
            return;
        }
        if pk.pusi {
            self.hdr.clear();
            self.hdr.extend_from_slice(pk.payload);
            self.pes_pos = pk.pos;
            self.header(global);
        } else if !self.hdr.is_empty() {
            // A header split over packets (a large adaptation field).
            self.hdr.extend_from_slice(pk.payload);
            self.header(global);
        } else if self.first.is_some() && self.data.len() < AUDIO_KEEP {
            self.data.extend_from_slice(pk.payload);
        }
    }

    fn header(&mut self, global: &mut Option<i64>) {
        match pes::parse_header(&self.hdr) {
            Some(h) => {
                if let Some(pts) = h.pts {
                    let g = *global.get_or_insert(pts as i64);
                    let u = self.clock.next(pts, g, self.pes_pos);
                    self.first.get_or_insert((u, self.pes_pos));
                }
                if self.first.is_some() && self.data.len() < AUDIO_KEEP {
                    self.data
                        .extend_from_slice(self.hdr.get(h.header_len..).unwrap_or(&[]));
                }
                self.hdr.clear();
            }
            None if self.hdr.len() > 300 || (self.hdr.len() >= 3 && self.hdr[..3] != [0, 0, 1]) => {
                self.hdr.clear();
            }
            None => {}
        }
    }
}

// ---------------------------------------------------------------- build

/// Track facts for one PMT stream, as flux-probe would list it.
struct Listed {
    es: Es,
    kind: TrackKind,
    codec: Codec,
    facts: Option<taudio::AudioFacts>,
}

fn list_tracks(head: &Head) -> Vec<Listed> {
    let mut out = Vec::new();
    for hs in &head.streams {
        let (mut kind, mut codec) = psi::classify(&hs.es, head.hdmv);
        let mut facts = None;
        match kind {
            TrackKind::Audio => {
                facts = taudio::facts(codec, &hs.data);
                if let Some(c) = facts.as_ref().and_then(|f| f.codec) {
                    codec = c;
                }
            }
            TrackKind::Data if hs.es.stream_type == 0x06 => {
                if let Some(f) = taudio::facts(Codec::Other, &hs.data) {
                    kind = TrackKind::Audio;
                    codec = f.codec.unwrap_or(Codec::Other);
                    facts = Some(f);
                }
            }
            _ => {}
        }
        if kind == TrackKind::Data && hs.first_pts.is_none() && hs.data.is_empty() {
            continue;
        }
        out.push(Listed {
            es: hs.es.clone(),
            kind,
            codec,
            facts,
        });
    }
    out
}

/// The presentation offset the stream needs (90 kHz): the largest
/// (sorted PTS − PTS) over frames in decode order, within each GOP, which
/// is what DTS = sorted PTS needs for CTS ≥ 0.
fn reorder_offset(frames: impl Iterator<Item = (i64, bool)>) -> i64 {
    fn gop_max(gop: &[i64]) -> i64 {
        let mut s = gop.to_vec();
        s.sort_unstable();
        s.iter().zip(gop).map(|(a, b)| a - b).max().unwrap_or(0)
    }
    let mut d = 0i64;
    let mut gop: Vec<i64> = Vec::new();
    for (pts, key) in frames {
        if key && !gop.is_empty() {
            d = d.max(gop_max(&gop));
            gop.clear();
        }
        gop.push(pts);
    }
    d.max(gop_max(&gop))
}

/// A standard frame rate (24000/1001, 25, 30000/1001 …) within 0.05 % of a
/// measured spacing (90 kHz ticks): (rate, timescale, ticks per frame,
/// frame ns). The measured mean carries the 90 kHz rounding of the
/// timestamps, so it is matched to the nearest standard rate rather than
/// used as is.
fn exact_rate(step: f64) -> Option<(flux_core::Rational, u32, u32, u64)> {
    let r = average_rate(step * 1e9 / 90_000.0)?;
    if r.den == 1000 || r.num == 0 {
        return None; // not a standard rate (milli-fps fallback)
    }
    let ns = (1e9 * r.den as f64 / r.num as f64).round() as u64;
    let (r, ts, fd) = rational_rate(ns)?;
    Some((r, ts, fd, ns))
}

/// Timestamp jitter a constant-rate stream may have (90 kHz ticks): 1 ms,
/// as in TS files remuxed from Matroska, whose timestamps are rounded to
/// the millisecond (41/42 ms steps at 23.976 fps). Snapping such frames to
/// the exact grid moves each by under 1 ms and can't make two collide.
const JITTER: f64 = 91.0;

/// Constant frame spacing (90 kHz ticks, fractional) of sorted timestamps,
/// or `None` for variable frame rate (e.g. a 59.94 fps section in a 23.976
/// fps stream: snapping it to one grid would collide frames, PB-1).
fn constant_spacing(sorted: &[i64]) -> Option<f64> {
    let all: Vec<i64> = sorted
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|&d| d > 0)
        .collect();
    if all.is_empty() {
        return None;
    }
    let mut by_size = all.clone();
    by_size.sort_unstable();
    let median = by_size[by_size.len() / 2] as f64;
    let close: Vec<i64> = all
        .iter()
        .copied()
        .filter(|&d| (d as f64 - median).abs() <= JITTER)
        .collect();
    // Steps that are not one frame: whole frames (dropped frames, gaps), or
    // a few odd ones where recordings were joined; never a second rate.
    let odd = all
        .iter()
        .filter(|&&d| {
            let k = (d as f64 / median).round();
            k < 1.0 || (d as f64 - k * median).abs() > 2.0 * JITTER
        })
        .count();
    if (close.len() as f64) < 0.95 * all.len() as f64 || odd > 4 + all.len() / 1000 {
        return None;
    }
    Some(close.iter().sum::<i64>() as f64 / close.len() as f64)
}

/// Builds the index (blocking). `Unsupported` when the file is not MPEG-TS
/// or holds something the remux doesn't reproduce faithfully.
pub(crate) fn build(src: &Source) -> Result<PlaybackIndex> {
    let size = src.len();
    let head_buf = src.read_at(0, HEAD_MAX.min(size) as usize)?;
    let Some((framing, first)) = packet::detect(&head_buf) else {
        return Err(HlsError::Unsupported(format!(
            "{}: not Matroska/WebM, MP4/MOV or MPEG-TS; needs the ffmpeg path",
            src.id().path.display()
        )));
    };
    let container = if framing.size == 192 {
        Container::M2ts
    } else {
        Container::MpegTs
    };
    let mut head = scan_head(
        &head_buf[..(HEAD.min(size) as usize).min(head_buf.len())],
        framing,
        first,
    );
    if !head.complete() && head_buf.len() as u64 > HEAD {
        head = scan_head(&head_buf, framing, first);
    }
    if head.streams.is_empty() {
        return Err(unsupported("no PAT/PMT in the first 1 MB"));
    }
    let listed = list_tracks(&head);
    let Some(vl) = listed.iter().find(|l| l.kind == TrackKind::Video) else {
        return Err(unsupported("no video stream"));
    };
    let flavor = match vl.codec {
        Codec::H264 => Flavor::H264,
        Codec::Hevc => Flavor::Hevc,
        c => return Err(unsupported(format!("{c:?} video is transcoded by ffmpeg"))),
    };
    let av_programs: Vec<u16> = listed
        .iter()
        .filter(|l| matches!(l.kind, TrackKind::Video | TrackKind::Audio))
        .map(|l| l.es.program)
        .fold(Vec::new(), |mut v, p| {
            if !v.contains(&p) {
                v.push(p);
            }
            v
        });
    if av_programs.len() > 1 {
        return Err(unsupported("several programs (a multiplex recording)"));
    }

    // ---- the full pass
    let mut slot = vec![0u16; 8192]; // 0 = ignore, 1 = video, 2.. = audio + 2
    let mut v = VState::new(vl.es.pid, flavor);
    slot[vl.es.pid as usize] = 1;
    let mut audio: Vec<AState> = Vec::new();
    for l in listed.iter().filter(|l| l.kind == TrackKind::Audio) {
        if slot[l.es.pid as usize] == 0 {
            slot[l.es.pid as usize] = 2 + audio.len() as u16;
            audio.push(AState {
                pid: l.es.pid,
                ..AState::default()
            });
        }
    }
    let mut global: Option<i64> = None;
    let mut tei = 0u64;
    packet::scan(src, first as u64, size, framing, SCAN_CHUNK, |pk| {
        let s = slot[pk.pid as usize];
        if s == 0 {
            return true;
        }
        if pk.tei {
            tei += 1;
        }
        if s == 1 {
            v.packet(pk, &mut global);
        } else {
            audio[s as usize - 2].packet(pk, &mut global);
        }
        true
    })?;
    v.finish(&mut global);

    // ---- what the remux can't reproduce
    if v.scrambled > 0 {
        return Err(unsupported("scrambled (encrypted) video"));
    }
    if v.field {
        return Err(unsupported("field-coded H.264 (PAFF) pictures"));
    }
    if v.multi_picture > 0 {
        return Err(unsupported("video PES packets with several pictures"));
    }
    if v.no_pts > 0 {
        return Err(unsupported(format!(
            "{} video PES packets without a PTS",
            v.no_pts
        )));
    }
    if v.params_changed {
        return Err(unsupported("parameter sets change mid-stream"));
    }
    let Some(first_kf) = v.frames.iter().position(|f| f.key) else {
        return Err(unsupported("no random access point in the video"));
    };
    let key_info = v.first_key.clone().unwrap_or_default();

    // ---- discontinuities: video jumps define the epochs
    let mut offsets = vec![0i64];
    for j in &v.clock.jumps {
        let o = offsets[offsets.len() - 1] + (j.expected - j.to);
        offsets.push(o);
    }
    let mut discontinuities: Vec<Discontinuity> = v
        .clock
        .jumps
        .iter()
        .zip(offsets.iter().skip(1))
        .map(|(j, &o)| Discontinuity {
            offset: o,
            starts: vec![(v.pid, j.pos)],
        })
        .collect();
    let mut warnings = Vec::new();
    // An audio jump belongs to the nearest video jump (by file position).
    let near = 64u64 << 20;
    for a in &audio {
        let mut used = vec![false; discontinuities.len()];
        for j in &a.clock.jumps {
            let best = v
                .clock
                .jumps
                .iter()
                .enumerate()
                .filter(|(i, vj)| !used[*i] && vj.pos.abs_diff(j.pos) <= near)
                .min_by_key(|(_, vj)| vj.pos.abs_diff(j.pos));
            match best {
                Some((i, _)) => {
                    used[i] = true;
                    discontinuities[i].starts.push((a.pid, j.pos));
                }
                None => warnings.push(format!(
                    "pid {:#x}: timestamp jump at byte {} with no video discontinuity near it",
                    a.pid, j.pos
                )),
            }
        }
    }
    let epoch_offset = |pid: u16, pos: u64| -> i64 {
        discontinuities
            .iter()
            .filter(|d| d.starts.iter().any(|&(p, at)| p == pid && at <= pos))
            .map(|d| d.offset)
            .next_back()
            .unwrap_or(0)
    };

    // ---- origin: ffmpeg's start_time (earliest first timestamp of A/V)
    let mut origin = v.frames.first().map(|f| f.pts).unwrap_or(0);
    for a in &audio {
        if let Some((u, pos)) = a.first {
            origin = origin.min(u + epoch_offset(a.pid, pos));
        }
    }
    let corrected = |f: &VFrame| f.pts + offsets[f.epoch];
    let frames = &v.frames[first_kf..];

    // ---- keyframes
    let mut keyframes: Vec<Keyframe> = frames
        .iter()
        .filter(|f| f.key)
        .map(|f| {
            let raw_ts = corrected(f);
            Keyframe {
                pts_ns: ticks_to_ns(raw_ts - origin, 90_000),
                raw_ts,
                pos: f.pos,
                rel: None,
            }
        })
        .collect();
    keyframes.sort_by_key(|k| k.pts_ns);
    keyframes.dedup_by_key(|k| k.pts_ns);
    if keyframes.windows(2).any(|w| w[1].pos <= w[0].pos) {
        return Err(unsupported("keyframe times and file order disagree"));
    }
    let open_gop = frames.iter().filter(|f| f.key).skip(1).any(|f| !f.idr);

    // ---- timing: frame rate, reorder depth, presentation offset
    let mut sorted: Vec<i64> = frames.iter().map(corrected).collect();
    sorted.sort_unstable();
    // Constant rate only if snapping every frame to the grid keeps every
    // presentation time distinct (a join of two recordings may sit off it).
    let spacing = constant_spacing(&sorted).filter(|&step| {
        let fd = exact_rate(step)
            .map(|(r, ..)| 90_000.0 * r.den as f64 / r.num as f64)
            .unwrap_or(step);
        let first = sorted[0];
        let snapped: Vec<i64> = sorted
            .iter()
            .map(|&p| ((p - first) as f64 / fd).round() as i64)
            .collect();
        snapped.windows(2).all(|w| w[1] > w[0])
    });
    let d90 = reorder_offset(frames.iter().map(|f| (corrected(f), f.key)));
    let last_pts = sorted.last().copied().unwrap_or(origin);
    let mean_step = spacing.unwrap_or_else(|| {
        if sorted.len() > 1 {
            (last_pts - sorted[0]) as f64 / (sorted.len() - 1) as f64
        } else {
            3754.0
        }
    });
    let video_end = last_pts + mean_step.round() as i64;
    let duration_ns = ticks_to_ns(video_end - origin, 90_000);

    // ---- the video track
    let sps: Vec<Vec<u8>> = key_info
        .params
        .iter()
        .filter(|n| flavor.nal_type(n) == if flavor == Flavor::H264 { 7 } else { 33 })
        .cloned()
        .collect();
    let pps: Vec<Vec<u8>> = key_info
        .params
        .iter()
        .filter(|n| flavor.nal_type(n) == if flavor == Flavor::H264 { 8 } else { 34 })
        .cloned()
        .collect();
    let vps: Vec<Vec<u8>> = key_info
        .params
        .iter()
        .filter(|n| flavor == Flavor::Hevc && flavor.nal_type(n) == 32)
        .cloned()
        .collect();
    let config = match flavor {
        Flavor::H264 => video::avcc(&sps, &pps),
        Flavor::Hevc => video::hvcc(&vps, &sps, &pps),
    }
    .ok_or_else(|| unsupported("no SPS/PPS before the first keyframe"))?;

    let (rate, timescale, frame_duration, frame_ns) = match spacing.map(exact_rate) {
        Some(Some((r, ts, fd, ns))) => (Some(r), ts, Some(fd), Some(ns)),
        // Constant but unusual: exact at 90 kHz when the step is whole.
        Some(None) if spacing.is_some_and(|s| (s - s.round()).abs() < 0.01) => {
            let step = spacing.unwrap_or(mean_step);
            let ns = step * 1e9 / 90_000.0;
            (
                average_rate(ns),
                90_000,
                Some(step.round() as u32),
                Some(ns.round() as u64),
            )
        }
        _ => (average_rate(mean_step * 1e9 / 90_000.0), 90_000, None, None),
    };

    let mut tracks: Vec<IndexTrack> = Vec::new();
    let mut video_index = None;
    for (i, l) in listed.iter().enumerate() {
        let mut t = IndexTrack {
            index: i as u32,
            id: l.es.pid as u64,
            kind: l.kind,
            codec: l.codec,
            codec_tag: format!("{:#04x}", l.es.stream_type),
            codec_private: Vec::new(),
            synth_config: None,
            timescale: 90_000,
            frame_duration: None,
            frame_rate: None,
            width: None,
            height: None,
            bit_depth: None,
            sample_rate: None,
            channels: None,
            language: l.es.language.clone().filter(|x| x != "und"),
            name: None,
            default: false,
            forced: false,
            hearing_impaired: l.es.hearing_impaired,
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
        if l.es.pid == v.pid && video_index.is_none() {
            video_index = Some(tracks.len());
            video_facts(&mut t, flavor, &config, &sps, &key_info);
            t.timescale = timescale;
            t.frame_duration = frame_duration;
            t.frame_rate = rate;
            t.default_duration_ns = frame_ns;
            t.first_pts_ns = keyframes.first().map(|k| k.pts_ns);
            t.inband_parameter_sets = true;
            t.open_gop = open_gop;
            // The video's own end: TS has no other duration (see above).
            t.extra.end_ns = Some(duration_ns);
            // Reorder depth: the SPS's promise or what the stream does.
            if frame_duration.is_some() {
                t.reorder_depth = t.reorder_depth.max((d90 as f64 / mean_step).round() as u32);
            }
            if duration_ns > 0 {
                t.bitrate = Some(v.payload_bytes * 8 * 1_000_000_000 / duration_ns as u64);
            }
        } else if l.kind == TrackKind::Audio {
            audio_track(&mut t, l, &audio, &|pid, pos| epoch_offset(pid, pos), origin);
        }
        tracks.push(t);
    }
    let video = video_index;

    // D = reorder depth × frame (whole frames for constant-rate video, as
    // for Matroska), else the measured offset; at least −earliest PTS.
    let mut d_ns = match video.and_then(|i| tracks.get(i)) {
        Some(vt) if vt.frame_duration.is_some() => ticks_to_ns(
            vt.reorder_depth as i64 * vt.frame_duration.unwrap_or(0) as i64,
            vt.timescale,
        ),
        _ => ticks_to_ns(d90, 90_000),
    };
    for t in tracks
        .iter()
        .filter(|t| matches!(t.kind, TrackKind::Video | TrackKind::Audio))
    {
        if let Some(p) = t.first_pts_ns {
            d_ns = d_ns.max(-p);
        }
    }

    let lost: u32 = v.lost + audio.iter().map(|a| a.lost).sum::<u32>();
    if lost > 0 {
        warnings.push(format!("{lost} continuity-counter errors (lost packets)"));
    }
    if tei > 0 {
        warnings.push(format!("{tei} packets flagged with transport errors"));
    }
    if !discontinuities.is_empty() {
        warnings.push(format!(
            "{} timestamp discontinuities, removed from the timeline",
            discontinuities.len()
        ));
    }
    let data_bytes = size.saturating_sub(first as u64).max(1);
    let layout = TsLayout {
        packet_size: framing.size as u16,
        first_packet: first as u64,
        data_end: size,
        origin,
        discontinuities,
        video_share_ppm: (v.payload_bytes as u128 * 1_000_000 / data_bytes as u128).min(1_000_000) as u32,
        byte_rate: if duration_ns > 0 {
            (data_bytes as u128 * 1_000_000_000 / duration_ns as u128) as u64
        } else {
            0
        },
    };
    Ok(PlaybackIndex {
        version: INDEX_VERSION,
        source: src.id().clone(),
        container,
        duration_ns,
        tracks,
        video,
        keyframes,
        presentation_offset_ns: d_ns,
        method: IndexMethod::TsScan,
        layout: Layout::Ts(layout),
        stats: IndexStats::default(),
        warnings,
    })
}

/// Video facts from the parameter sets and SEI of the first keyframe (the
/// same derivation as the Matroska index's).
fn video_facts(t: &mut IndexTrack, flavor: Flavor, config: &[u8], sps: &[Vec<u8>], key: &AuInfo) {
    t.codec_private = config.to_vec();
    let mut vui = None;
    match flavor {
        Flavor::H264 => {
            if let Some(c) = h264::AvcConfig::parse(config) {
                t.codec_string = Some(c.codec_string());
                t.level = Some(c.level_idc as u16);
                t.profile =
                    Some(crate::index::mkv::avc_profile_name(c.profile_idc, c.constraint_flags).to_string());
            }
            if let Some(s) = sps.first().and_then(|s| h264::parse_sps(s)) {
                t.bit_depth = Some(s.bit_depth);
                t.width = Some(s.width);
                t.height = Some(s.height);
                vui = s.color;
                t.extra.sar = s.sar;
                t.reorder_depth = s.max_num_reorder_frames.unwrap_or(0);
            }
        }
        Flavor::Hevc => {
            if let Some(c) = hevc::HevcConfig::parse(config) {
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
            }
            if let Some(s) = sps.first().and_then(|s| hevc::parse_sps(s)) {
                t.width = Some(s.width);
                t.height = Some(s.height);
                vui = s.color;
                t.extra.sar = s.sar;
                t.reorder_depth = s.max_num_reorder_pics;
            }
        }
    }
    t.color = merge_color(
        None,
        vui.map(|c| ColorInfo {
            primaries: Some(c.primaries).filter(|&v| v != 2),
            transfer: Some(c.transfer).filter(|&v| v != 2),
            matrix: Some(c.matrix).filter(|&v| v != 2),
            full_range: Some(c.full_range),
        }),
    );
    let mut sei = codec::SeiHdr::default();
    let header = if flavor == Flavor::H264 { 1 } else { 2 };
    for n in &key.sei {
        let s = codec::parse_hdr_sei(n, header);
        sei.mastering = sei.mastering.or(s.mastering);
        sei.cll = sei.cll.or(s.cll);
    }
    t.hdr = Some(HdrInfo {
        format: classify_hdr(t.color.as_ref(), false),
        mastering: sei.mastering,
        cll: sei.cll,
        dovi: None,
    });
}

/// Audio track facts: configuration records for the codecs the remux
/// copies, rates and channel counts for all.
fn audio_track(
    t: &mut IndexTrack,
    l: &Listed,
    audio: &[AState],
    epoch_offset: &dyn Fn(u16, u64) -> i64,
    origin: i64,
) {
    let a = audio.iter().find(|a| a.pid == l.es.pid);
    let facts = l.facts.clone().unwrap_or_default();
    t.channels = facts.channels;
    t.sample_rate = facts.sample_rate;
    t.bit_depth = facts.bit_depth;
    t.profile = facts.profile.clone();
    t.timescale = facts.sample_rate.unwrap_or(48_000);
    if let Some((u, pos)) = a.and_then(|a| a.first) {
        t.first_pts_ns = Some(ticks_to_ns(u + epoch_offset(l.es.pid, pos) - origin, 90_000));
    }
    let data = a.map(|a| a.data.as_slice()).unwrap_or(&[]);
    match taudio::Framing::of(t.codec, &t.codec_tag) {
        Some(taudio::Framing::Adts) => {
            if let Some(asc) = (0..data.len().min(4096)).find_map(|i| taudio::adts_asc(&data[i..])) {
                if let Some(c) = codec::audio::AacConfig::parse(&asc) {
                    t.timescale = c.sample_rate;
                    t.sample_rate = Some(c.sample_rate);
                    t.codec_string = Some(c.codec_string());
                }
                t.codec_private = asc;
            }
            t.frame_duration = Some(1024);
        }
        Some(taudio::Framing::Ac3) => {
            if let Some((cfg, samples, rate, bitrate)) = taudio::ac3_config(data) {
                t.synth_config = Some(cfg);
                t.frame_duration = Some(samples);
                t.timescale = rate;
                t.sample_rate = Some(rate);
                t.bitrate = Some(bitrate);
            }
            t.codec_string = Some(if t.codec == Codec::Ac3 { "ac-3" } else { "ec-3" }.into());
        }
        Some(taudio::Framing::Mpa) => {
            if let Some(h) = (0..data.len().min(4096)).find_map(|i| taudio::mpa(&data[i..])) {
                t.frame_duration = Some(h.samples);
                t.timescale = h.rate;
            }
            t.codec_string = Some(
                if t.codec == Codec::Mp3 {
                    "mp4a.40.34"
                } else {
                    "mp4a.40.33"
                }
                .into(),
            );
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_rate_from_90khz_timestamps() {
        // 23.976 fps: 3753.75 ticks per frame, rounded to the 90 kHz clock.
        let pts: Vec<i64> = (0..500)
            .map(|k| (k as f64 * 3753.75).round() as i64 + 126_000)
            .collect();
        let step = constant_spacing(&pts).unwrap();
        assert!((step - 3753.75).abs() < 0.01, "{step}");
        let (r, ts, fd, ns) = exact_rate(step).unwrap();
        assert_eq!((r.num, r.den, ts, fd, ns), (24000, 1001, 24000, 1001, 41_708_333));
        assert_eq!(exact_rate(3600.0).map(|x| (x.1, x.2)), Some((10_000, 400)));
        assert_eq!(exact_rate(3000.0 * 1.003), None);
        // A dropped frame is still constant-rate.
        let mut gap = pts.clone();
        gap.remove(100);
        assert!(constant_spacing(&gap).is_some());
        // Millisecond timestamps (a remux of Matroska): 41/42 ms steps.
        let ms: Vec<i64> = (0..2000)
            .map(|k| (k as f64 * 1001.0 / 24.0).round() as i64 * 90)
            .collect();
        let (r, ..) = exact_rate(constant_spacing(&ms).unwrap()).unwrap();
        assert_eq!((r.num, r.den), (24000, 1001));
        // A 59.94 fps section in a 23.976 stream is not (PB-1 for TS).
        let mut vfr = pts[..300].to_vec();
        let base = *vfr.last().unwrap();
        vfr.extend((1..200).map(|k| base + (k as f64 * 1501.5).round() as i64));
        assert_eq!(constant_spacing(&vfr), None);
    }

    #[test]
    fn presentation_offset_from_decode_order() {
        // I P B B per GOP (decode order), 1 tick per frame: P is shown
        // two frames after it is decoded.
        let gop = |base: i64| {
            vec![
                (base, true),
                (base + 3, false),
                (base + 1, false),
                (base + 2, false),
            ]
        };
        let frames: Vec<(i64, bool)> = gop(0).into_iter().chain(gop(4)).collect();
        assert_eq!(reorder_offset(frames.into_iter()), 1);
        assert_eq!(reorder_offset([(0, true), (1, false)].into_iter()), 0);
    }

    #[test]
    fn clock_unwraps_and_records_jumps() {
        let w = pes::WRAP;
        let mut c = Clock::default();
        let g = w - 7_200;
        assert_eq!(c.next((w - 7_200) as u64, g, 0), w - 7_200);
        assert_eq!(c.next((w - 3_600) as u64, g, 188), w - 3_600);
        // Across the 33-bit wrap: continuous.
        assert_eq!(c.next(3_600, g, 376), w + 3_600);
        assert!(c.jumps.is_empty());
        // Back by 30 s: a discontinuity, expected to continue one step later.
        let back = w + 3_600 - 30 * 90_000;
        assert_eq!(c.next((back % w) as u64, g, 564), back);
        assert_eq!(c.jumps.len(), 1);
        assert_eq!(
            (c.jumps[0].expected, c.jumps[0].to, c.jumps[0].pos),
            (w + 10_800, back, 564)
        );
    }
}
