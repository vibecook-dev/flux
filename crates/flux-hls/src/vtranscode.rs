//! Video transcode via ffmpeg (research/03 §6.2–6.3), in short pieces
//! encoded by warm encoder runs.
//!
//! - **Encoder.** H.264 from the encoder [`crate::encoders`] detects once per
//!   ffmpeg binary: VideoToolbox on macOS, else NVENC, QSV or VAAPI, else
//!   libx264 (`FLUX_VIDEO_ENCODER` forces one). Each has its own decode,
//!   scale and tone-map chain; the piece design below is the same for all.
//! - **Pieces.** The transcoded rendition has its own segmentation: every
//!   remux segment (source keyframe span) is split into pieces of about
//!   [`VideoTranscoder::piece_ns`] (2 s by default) on the frame grid. A short
//!   first piece is what the player waits for (time to first frame and after
//!   a seek), not a whole 6 s segment. Pieces are addressed like segments:
//!   `v/{n}.m4s` is piece n of a transcode plan.
//! - **Runs.** One ffmpeg process encodes a run of consecutive pieces
//!   (`run_pieces`, 4 by default), so the encoder session set-up
//!   (~0.3 s for VideoToolbox) is paid once per run instead of once per
//!   segment. The run
//!   seeks to the source keyframe of its first piece (`-ss T_k
//!   -noaccurate_seek`; an MPEG-TS source is instead fed from that keyframe
//!   through stdin, [`crate::ts::video_input`], since ffmpeg's TS seek lands
//!   on any PES), drops the frames before the piece (`trim`), and
//!   forces an IDR at every piece boundary. A container only ffmpeg reads
//!   has no keyframe list: its run seeks by time a margin before the piece
//!   and converts to the index's constant frame rate (`fps`, `tpad`), so
//!   the frame counts are exact by construction (`external_window`).
//!   Its fragmented-MP4 output is parsed as it streams in, and each piece
//!   is published as soon as its frames are complete, so a request waits
//!   only for its own piece.
//! - Frame counts are exact per piece (`-frames:v`, one IDR per piece, no
//!   B-frames, CTS = presentation offset D): pieces are independent, any one
//!   can be produced cold, and they line up with every audio rendition
//!   (tfdt = piece start on the asset's timeline).
//! - An MP4 video without a constant frame duration, or with frames held
//!   longer, is converted to its nominal rate ([`converted_rate`]), unless
//!   some stretch of it is faster: its pieces lie on that rate's grid and
//!   `fps` makes their frame counts exact, whatever frames the source holds
//!   for seconds or the decoder drops.
//! - The init segment needs the encoder's `avcC`: it waits for the run that
//!   covers the first piece requested (or `warm`ed). Each piece also carries
//!   SPS/PPS in-band in its IDR sample, so a run whose encoder session
//!   picks different parameter sets stays decodable.
//! - Results are cached by (asset, recipe, piece) in a byte-budgeted LRU; the
//!   pieces after a request are prefetched by another run (cancellable);
//!   every ffmpeg child is `kill_on_drop`.
//! - **Concurrency.** Request and prefetch runs share one pool of encoder
//!   slots ([`default_max_runs`] per encoder kind, or configured). A
//!   prefetch run only takes a spare slot, and gives it up to a request
//!   that finds every slot busy, so first pieces never wait behind prefetch.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use flux_core::{Codec, ColorInfo, Rational, VideoOp};
use mp4_atom::{Decode, Encode};
use tokio::io::AsyncReadExt;
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::asset::Asset;
use crate::burn::{self, BurnIn, BurnKey, BurnProbe};
use crate::cache::ByteCache;
use crate::encoders::{Deinterlace, DetectOptions, FfmpegCaps, PipelineInput, VideoEncoder};
use crate::error::{HlsError, Result};
use crate::fmp4::{self, EntrySpec, Fragment, InitSpec, Sample, SampleData};
use crate::index::{pack_language, Layout};
use crate::segment::SegmentPlan;
use crate::SegmentBody;

/// What a video transcode produces (H.264 High).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VideoTarget {
    pub width: u32,
    pub height: u32,
    pub bitrate: u64,
    /// HDR → SDR (BT.709) conversion (`scale_vt`, `tonemap_vaapi` or the
    /// software chain, see [`crate::encoders`]).
    pub tonemap: bool,
    /// A subtitle drawn into the picture ([`VideoTranscoder::burn_key`]).
    pub burn: Option<BurnKey>,
}

impl VideoTarget {
    /// The target of a `VideoOp::Transcode`, without a burned-in subtitle
    /// (the engine adds that from the plan).
    pub fn from_op(op: &VideoOp) -> Option<Self> {
        match op {
            VideoOp::Transcode {
                width,
                height,
                bitrate,
                tonemap,
                ..
            } => Some(VideoTarget {
                width: *width,
                height: *height,
                bitrate: *bitrate,
                tonemap: *tonemap,
                burn: None,
            }),
            VideoOp::Copy { .. } => None,
        }
    }

    /// HLS `CODECS` string: High profile at the level the output needs
    /// for its size and `fps` (see [`h264_level`]); the encoder is told the
    /// same level, so it matches the SPS (PB-12).
    pub fn codec_string(&self, fps: f64) -> String {
        format!("avc1.6400{:02x}", h264_level(self.width, self.height, fps))
    }
}

/// The H.264 `level_idc` an output of `width`×`height` at `fps` needs
/// (ITU-T H.264 Table A-1: frame size and macroblock rate), at least 4.1:
/// 1080p30 → 41, 1080p50/60 → 42, 2160p30 → 51.
pub fn h264_level(width: u32, height: u32, fps: f64) -> u8 {
    let mbs = width.div_ceil(16) as u64 * height.div_ceil(16) as u64;
    let rate = mbs as f64 * fps.max(1.0);
    const LEVELS: [(u8, u64, f64); 5] = [
        (41, 8_192, 245_760.0),
        (42, 8_704, 522_240.0),
        (50, 22_080, 589_824.0),
        (51, 36_864, 983_040.0),
        (52, 36_864, 2_073_600.0),
    ];
    LEVELS
        .iter()
        .find(|(_, fs, mbps)| mbs <= *fs && rate <= *mbps)
        .map_or(52, |l| l.0)
}

/// Frames per second of the asset's video for level/limit decisions: the
/// exact rate, else the measured average, else 30.
pub fn source_fps(asset: &Asset) -> f64 {
    asset
        .index
        .video_track()
        .and_then(|v| v.frame_rate)
        .map(|r| r.as_f64())
        .filter(|f| f.is_finite() && *f > 0.0)
        .unwrap_or(30.0)
}

/// The constant frame rate a transcode converts `asset`'s video to, if
/// any: an MP4 track without a constant sample duration (variable rate,
/// or a coarse clock's alternating deltas), or with samples held for more
/// than one and a half frames, is converted to its nominal rate, the one
/// most of its frames have
/// ([`IndexTrack::frame_rate`](crate::index::IndexTrack::frame_rate)),
/// unless a stretch of its frames comes faster (`converts`).
///
/// The frames of such a file can't be counted from its index. A stream cut
/// without re-encoding holds the frame before each cut for seconds, and
/// the decoder drops the frames after the cut that lost their references,
/// without an error (66, 137 and 19 frames in one episode). Pieces cut at
/// the index's counts then missed the IDRs forced at their starts, and a
/// piece played alone showed nothing. On the rate's grid, `fps` repeats
/// the frame on screen, so each piece's count holds by construction, and
/// pieces stay ~2 s long across the held frames instead of a whole
/// keyframe span of up to 52 s.
pub fn converted_rate(asset: &Asset) -> Option<Rational> {
    let Layout::Mp4(l) = &asset.index.layout else {
        return None;
    };
    let v = asset.index.video_track()?;
    let rate = v
        .frame_rate
        .filter(|r| r.num > 0 && r.den > 0 && (1.0..=240.0).contains(&r.as_f64()))?;
    let t = asset.index.video.and_then(|i| l.tracks.get(i))?;
    converts(&t.stts, t.timescale, v.frame_duration, rate).then_some(rate)
}

/// Samples in a row that, all shorter than a frame of the converted rate,
/// are frames shot at a higher rate rather than a glitch: half a second at
/// 24 fps. A muxer that cuts a stream may give a sample or two around a
/// cut a tick's duration to keep its decode times rising.
const FASTER_RUN: u64 = 12;

/// Whether a video with these samples (`stts` on `timescale`, the
/// index's constant duration `frame_duration`) is converted to `rate`
/// ([`converted_rate`]): it has no constant duration, or some sample lasts
/// more than one and a half of it, and no stretch of [`FASTER_RUN`]
/// samples is shorter than a frame of `rate` (beyond the clock's slack,
/// [`frame_slack`](crate::index::mp4::frame_slack)).
///
/// Converting only ever repeats frames then. A phone video that drops from
/// 30 to 15 fps in low light, mostly at 15, was converted at 15 fps and
/// lost every other frame of its 30 fps part; it keeps the real timestamps
/// instead (PB-2). So does a mix of rates without a majority, rated by its
/// average, when its faster frames come in stretches.
fn converts(stts: &[(u32, u32)], timescale: u32, frame_duration: Option<u32>, rate: Rational) -> bool {
    if timescale == 0 || rate.num == 0 || rate.den == 0 {
        return false;
    }
    // One duration for nine tenths of the samples, and nothing held
    // longer: the index's frame grid. A cut in the rest (or a last frame
    // held) is the same stream as one without a constant duration.
    if let Some(fd) = frame_duration {
        if !stts.iter().any(|&(_, d)| u64::from(d) * 2 > u64::from(fd) * 3) {
            return false;
        }
    }
    let frame = f64::from(timescale) * f64::from(rate.den) / f64::from(rate.num);
    let faster = frame - crate::index::mp4::frame_slack(frame, timescale);
    let mut run = 0u64;
    for &(count, d) in stts {
        if f64::from(d) < faster {
            run += u64::from(count);
            if run >= FASTER_RUN {
                return false;
            }
        } else {
            run = 0;
        }
    }
    true
}

/// Where a run of an index-less asset (`Layout::External`) starts ffmpeg:
/// `input` what it opens (the file, or the part of it the run starts in,
/// [`ExternalLayout::input_at`](crate::index::external::ExternalLayout::input_at)),
/// `seek_us` its `-ss` (µs from that input's start, as ffmpeg counts it),
/// and where the run's first piece starts on the input's own clock (µs):
/// the run keeps the input's timestamps (`-copyts`) and subtracts that.
/// Without `-copyts` the timestamps after `-ss` depend on the ffmpeg
/// version for a file whose start time isn't 0: 6.1 left the video's start
/// time in (an MPEG-PS piece showed the source 0.53 s early), 8.1 takes it
/// out. Audio agrees across versions, and keeps its `-ss` arithmetic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Window {
    pub input: String,
    pub seek_us: i64,
    pub first_us: i64,
}

/// The [`Window`] of a run whose first piece is `first`, or `None` for an
/// indexed asset: the seek margin before the piece
/// ([`crate::index::external::ExternalLayout::seek_margin_ns`]) plus
/// `back_us` (a retry, see [`SEEK_BACK_US`]), never before the start of
/// the input.
pub(crate) fn external_window(asset: &Asset, first: &Piece, ts: f64, back_us: i64) -> Option<Window> {
    let Layout::External(l) = &asset.index.layout else {
        return None;
    };
    let t0_ns = (first.start_ticks as f64 * 1e9 / ts).round() as i64;
    let (input, base_ns, clock_ns) = l.input_at(&asset.index.source.path, t0_ns);
    let t0 = (t0_ns - base_ns) / 1000;
    let seek = (t0 - l.seek_margin_ns.max(0) / 1000 - back_us.max(0)).max(0);
    Some(Window {
        input,
        seek_us: seek,
        first_us: clock_ns / 1000 + t0,
    })
}

/// How much further back a time-based run seeks on each retry, µs. A seek
/// past a file's last keyframe can decode nothing: MPEG-PS seeks land on
/// any packet, and in a RealMedia file whose index names a stream it
/// doesn't have, every target after the last keyframe (the last 4.2 s)
/// decoded no frame. A run that produced no frame at all is retried from
/// 15 s, then 60 s earlier. The frame grid (`fps=…:start_time=0`) drops
/// what comes before the piece, so the pieces are the same from any seek
/// point.
pub(crate) const SEEK_BACK_US: [i64; 3] = [0, 15_000_000, 60_000_000];

/// µs as ffmpeg seconds ("12.345678", "-0.500000").
fn micros(us: i64) -> String {
    let sign = if us < 0 { "-" } else { "" };
    let us = us.unsigned_abs();
    format!("{sign}{}.{:06}", us / 1_000_000, us % 1_000_000)
}

/// Clockwise display rotation of the asset's video (0/90/180/270).
fn rotation(asset: &Asset) -> u16 {
    asset.index.video_track().map_or(0, |v| v.extra.rotation)
}

/// Which frames a transcode of `v` deinterlaces (the encoder writes
/// progressive frames):
/// - interlaced H.264 ([`IndexTrack::interlaced`](crate::index::IndexTrack::interlaced):
///   the container's flag, else the SPS): the frames the decoder marks
///   interlaced, or every frame when the SPS codes only progressive frames
///   (the decoder marks none, so the container's flag is all there is);
/// - MPEG-2 and VC-1, the codecs of DVD and broadcast sources: the frames
///   the decoder marks interlaced.
///
/// HEVC and newer codecs code fields as separate pictures, if at all.
pub fn deinterlace(v: &crate::index::IndexTrack) -> Deinterlace {
    match v.codec {
        Codec::H264 => match (v.interlaced(), v.coded_interlaced()) {
            (Some(true), Some(false)) => Deinterlace::All,
            (Some(true), _) => Deinterlace::Marked,
            _ => Deinterlace::Off,
        },
        Codec::Mpeg2Video | Codec::Vc1 => Deinterlace::Marked,
        _ => Deinterlace::Off,
    }
}

/// Size of the encoded pictures: the target, turned upright for a source
/// with a 90°/270° display rotation (the transcode rotates the pixels, so
/// players need no matrix, PB-9).
pub fn output_size(asset: &Asset, target: &VideoTarget) -> (u32, u32) {
    match rotation(asset) {
        90 | 270 => (target.height, target.width),
        _ => (target.width, target.height),
    }
}

/// Aggregate statistics.
#[derive(Debug, Clone, Copy, Default)]
pub struct VideoTranscodeStats {
    /// Pieces produced.
    pub segments: u64,
    pub cache_hits: u64,
    /// Pieces produced by prefetch runs.
    pub prefetched: u64,
    /// Sum over pieces of the time from their run's start to the piece.
    pub total_ms: f64,
    pub max_ms: f64,
    /// ffmpeg runs started.
    pub runs: u64,
    /// Sum over runs of the time to the first piece (spawn + set-up + encode).
    pub first_piece_ms: f64,
}

/// One output segment of a transcoded video rendition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Piece {
    /// The remux segment (source keyframe span) the piece starts in: its
    /// run seeks to that keyframe. Only a converted video's pieces
    /// ([`converted_rate`]) cross into the next segment.
    pub seg: usize,
    /// Presentation start/end on the video timescale (frame grid).
    pub start_ticks: i64,
    pub end_ticks: i64,
    /// Exact frame count; `None` for the last piece of the file (to the end).
    pub frames: Option<u32>,
}

/// Splits every remux segment of `asset` into pieces of about `piece_ns`
/// on the frame grid. An MP4 video without one, or with frames held
/// longer, is converted to its nominal rate, on whose grid its pieces lie
/// (`grid_pieces`); other sources without a frame grid keep one piece per
/// segment.
/// `Piece::frames` is a prediction from the frame rate: the encoder run
/// counts and times its frames from the index's real timestamps
/// ([`run_frames`]), so VFR stretches stay exact.
pub fn pieces(asset: &Asset, piece_ns: i64) -> Vec<Piece> {
    if let Some(rate) = converted_rate(asset) {
        return grid_pieces(&asset.segments, rate, piece_ns);
    }
    let v = asset.index.video_track();
    let fd = v
        .and_then(|v| v.frame_duration)
        .map(|f| f as i64)
        .filter(|&f| f > 0);
    let ts = asset.segments.timescale.max(1) as i64;
    let piece_ticks = (piece_ns as i128 * ts as i128 / 1_000_000_000) as i64;
    let mut out = Vec::with_capacity(asset.segments.len() * 3);
    for (k, s) in asset.segments.segments.iter().enumerate() {
        let last = s.next_kf.is_none();
        let span = (s.end_ticks - s.start_ticks).max(1);
        let frames_total = match fd {
            Some(fd) => (span + fd / 2) / fd,
            None => 0,
        };
        let m = match fd {
            Some(_) if piece_ticks > 0 => {
                ((span + piece_ticks / 2) / piece_ticks).clamp(1, frames_total.max(1))
            }
            _ => 1,
        };
        let mut start = s.start_ticks;
        for j in 0..m {
            let (frames, end) = match fd {
                Some(fd) => {
                    let f = frames_total * (j + 1) / m - frames_total * j / m;
                    (Some(f as u32), start + f * fd)
                }
                None => (
                    Some(
                        ((span as f64 / ts as f64)
                            * v.and_then(|v| v.frame_rate).map(|r| r.as_f64()).unwrap_or(24.0))
                        .round()
                        .max(1.0) as u32,
                    ),
                    s.end_ticks,
                ),
            };
            let is_last = last && j + 1 == m;
            out.push(Piece {
                seg: k,
                start_ticks: start,
                end_ticks: if j + 1 == m { s.end_ticks } else { end },
                frames: if is_last { None } else { frames },
            });
            start = end;
        }
    }
    out
}

/// The pieces of a video converted to `rate` ([`converted_rate`]): one
/// grid of that rate from the first keyframe to the end of the video, cut
/// every `piece_ns` worth of frames (rounded), the last piece taking the
/// rest (half a piece to one and a half). The grid ignores the keyframes:
/// a piece's run seeks to the keyframe of the segment it starts in, and
/// `fps` puts every frame of the grid on the same source frame from any
/// run (the frame on screen at that time).
fn grid_pieces(plan: &SegmentPlan, rate: Rational, piece_ns: i64) -> Vec<Piece> {
    let segs = &plan.segments;
    let (Some(first), Some(last)) = (segs.first(), segs.last()) else {
        return Vec::new();
    };
    let ts = i128::from(plan.timescale.max(1));
    let (num, den) = (i128::from(rate.num.max(1)), i128::from(rate.den.max(1)));
    const NS: i128 = 1_000_000_000;
    // Grid frame k starts k·den/num s after the origin (rounded to a tick).
    let at = |k: i64| first.start_ticks + ((2 * i128::from(k) * den * ts + num) / (2 * num)) as i64;
    let span = i128::from((last.end_ticks - first.start_ticks).max(1));
    let total = ((2 * span * num + den * ts) / (2 * den * ts)).max(1) as i64;
    let per = ((2 * i128::from(piece_ns.max(1)) * num + den * NS) / (2 * den * NS)).max(1) as i64;
    let m = ((total + per / 2) / per).max(1);
    (0..m)
        .map(|j| {
            let a = j * per;
            let b = if j + 1 == m { total } else { a + per };
            let start = at(a);
            let end = if j + 1 == m { last.end_ticks } else { at(b) };
            Piece {
                seg: segs.partition_point(|s| s.start_ticks <= start).saturating_sub(1),
                start_ticks: start,
                end_ticks: end.max(start + 1),
                frames: Some((b - a) as u32),
            }
        })
        .collect()
}

/// The first piece that starts in remux segment `seg` (or later).
pub fn first_piece_of(pieces: &[Piece], seg: usize) -> usize {
    pieces.partition_point(|p| p.seg < seg)
}

/// The source frames a run encodes, from the index (PB-2): presentation
/// times on the video timescale, as the remux places them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunFrames {
    /// Every frame of the remux segments the run touches, plus the next
    /// one (whose open-GOP leading pictures present before its keyframe),
    /// sorted.
    pub all: Vec<i64>,
    /// The frames of each piece of the run: those presented in
    /// `[start_ticks, end_ticks)`.
    pub per_piece: Vec<Vec<i64>>,
}

impl RunFrames {
    /// The first frame presented at or after `t`.
    fn first_at_or_after(&self, t: i64) -> Option<i64> {
        let i = self.all.partition_point(|&x| x < t);
        self.all.get(i).copied()
    }

    /// The last frame presented before `t`.
    fn last_before(&self, t: i64) -> Option<i64> {
        let i = self.all.partition_point(|&x| x < t);
        i.checked_sub(1).map(|i| self.all[i])
    }
}

/// The real frames of `run` (consecutive pieces), or `None` when the index
/// can't list them (containers only ffmpeg reads), a piece would have
/// none, or the video is converted to a constant rate ([`converted_rate`]):
/// the predicted counts are used then.
pub fn run_frames(asset: &Asset, run: &[Piece]) -> Result<Option<RunFrames>> {
    let (Some(first), Some(last)) = (run.first(), run.last()) else {
        return Ok(None);
    };
    if converted_rate(asset).is_some() {
        return Ok(None);
    }
    let mut all = Vec::new();
    for seg in first.seg..=(last.seg + 1).min(asset.segments.len().saturating_sub(1)) {
        match asset.video_frame_ticks(seg)? {
            Some(t) => all.extend(t),
            None => return Ok(None),
        }
    }
    all.sort_unstable();
    all.dedup();
    let per_piece: Vec<Vec<i64>> = run
        .iter()
        .map(|p| {
            let a = all.partition_point(|&x| x < p.start_ticks);
            let b = all.partition_point(|&x| x < p.end_ticks);
            all[a..b].to_vec()
        })
        .collect();
    if per_piece.iter().any(|f| f.is_empty()) {
        return Ok(None);
    }
    Ok(Some(RunFrames { all, per_piece }))
}

/// Where the piece that starts at `pending[0]` ends in the encoder's
/// output `pending` (samples, sync flag): at the encoder's sync sample (the
/// IDR forced at the piece's boundary) nearest to `f`, the piece's frame
/// count (from the index, else predicted from the frame grid), when one lies
/// within [`CUT_SLACK`] frames; else at `f`. `None`: wait for more output.
///
/// `pending` holds whole fragments, and ffmpeg starts a fragment at every
/// keyframe (`frag_keyframe`), so until the encoder is done (`eof`) the
/// sample after the last one received is a sync sample too: a piece is cut
/// as soon as its fragment is complete, also when that fragment ends up to
/// [`CUT_SLACK`] frames short of `f` (the IDR came early). The run's last
/// piece takes what the encoder produced, up to `f`: a frame the source
/// lacks made `-frames:v` read one past the run's end, which the next run
/// starts with.
///
/// An AVI with one frame missing at 0.04 s put every forced IDR of the
/// run's later pieces one sample before their start, so a seek into them
/// began on a P frame.
fn piece_cut(pending: &[(Bytes, bool)], f: usize, last: bool, eof: bool) -> Option<usize> {
    let f = f.max(1);
    // Short of `f` and not done: the next fragment is still to come, unless
    // the IDR that starts it is the boundary's, early (the run's last piece
    // has no boundary IDR: it waits for all its frames).
    let short = pending.len() < f && (last || pending.len() + CUT_SLACK < f);
    if pending.is_empty() || (short && !eof) {
        return None;
    }
    if last {
        return Some(pending.len().min(f));
    }
    let next_fragment = (!eof).then_some(pending.len());
    let at_sync = (1..pending.len())
        .filter(|&i| pending[i].1)
        .chain(next_fragment)
        .filter(|&i| i.abs_diff(f) <= CUT_SLACK)
        .min_by_key(|&i| (i.abs_diff(f), i));
    match at_sync {
        Some(c) => Some(c),
        None => (pending.len() >= f).then_some(f),
    }
}

/// How far (in frames) a forced IDR may be from a predicted piece boundary
/// and still mark it.
const CUT_SLACK: usize = 3;

/// Concurrent encoder runs (sessions), requests and prefetch together, when
/// none is configured: a hardware encoder takes several sessions at once
/// (8 parallel 1080p sessions kept an RTX 4090's NVENC under 91% busy; a
/// GeForce driver allows 8 per machine, shared with other apps), while
/// libx264 runs share the CPU.
pub fn default_max_runs(encoder: Option<VideoEncoder>) -> usize {
    match encoder {
        Some(e) if e.is_hardware() => 6,
        _ => 3,
    }
}

type VKey = (String, VideoTarget, u32);
/// A piece being produced: `None` until done.
type Slot = Option<std::result::Result<Bytes, Arc<str>>>;
/// A piece in flight: its result channel and its run.
type InFlight = (watch::Receiver<Slot>, Arc<RunCtl>);

/// An encoder run in flight: its cancellation and who waits on it (PB-5).
#[derive(Debug, Default)]
struct RunCtl {
    cancel: CancellationToken,
    /// Requests waiting on one of the run's pieces right now.
    waiting: AtomicUsize,
}

/// One request waiting on a run's piece. Dropped without its piece (the
/// player aborted the request: a seek away) while nobody else waits on the
/// run, it cancels the run: ffmpeg is killed and the encode permit freed
/// for the piece the player wants now.
struct Waiter {
    run: Arc<RunCtl>,
    served: bool,
}

impl Waiter {
    fn new(run: Arc<RunCtl>) -> Self {
        run.waiting.fetch_add(1, Ordering::SeqCst);
        Waiter { run, served: false }
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        let before = self.run.waiting.fetch_sub(1, Ordering::SeqCst);
        if !self.served && before == 1 {
            tracing::debug!("video run abandoned by its last waiter: cancelled");
            self.run.cancel.cancel();
        }
    }
}

/// Supervised ffmpeg video transcoder with a piece cache.
pub struct VideoTranscoder {
    ffmpeg: PathBuf,
    /// ffprobe, for when a burned-in track starts (`burn::probe`).
    pub ffprobe: PathBuf,
    /// How the encoder is chosen (forced encoder, VAAPI node, test encodes).
    pub detect: DetectOptions,
    /// What `ffmpeg` can do, detected on first use.
    caps: tokio::sync::OnceCell<Arc<FfmpegCaps>>,
    /// Encoder slots, shared by request and prefetch runs; filled once the
    /// encoder is known ([`Self::caps`]).
    permits: Arc<Semaphore>,
    /// Configured slot count (0: [`default_max_runs`]).
    max_jobs: usize,
    /// Slots in the pool once the encoder is known.
    max_runs: AtomicUsize,
    /// Prefetch runs in flight, the ones a request may take a slot from.
    prefetching: Mutex<Vec<Arc<RunCtl>>>,
    cache: Mutex<ByteCache<VKey>>,
    inflight: Mutex<HashMap<VKey, InFlight>>,
    avcc: Mutex<HashMap<(String, VideoTarget), Vec<u8>>>,
    /// Burned-in subtitles named by `VideoTarget::burn`, and what was read
    /// from them once ([`crate::burn::probe`]).
    burns: Mutex<HashMap<BurnKey, Arc<BurnIn>>>,
    burn_probes: Mutex<HashMap<BurnKey, BurnProbe>>,
    stats: Mutex<VideoTranscodeStats>,
    /// Pieces kept encoded ahead of the last request.
    pub lookahead: u32,
    /// Target piece duration in ns (see the module docs).
    pub piece_ns: i64,
    /// Pieces per encoder run.
    pub run_pieces: u32,
    timeout: Duration,
    cancel: CancellationToken,
}

/// Converts an `avcC` record's SPS/PPS to length-prefixed NAL units.
fn avcc_parameter_sets(avcc: &[u8]) -> Vec<u8> {
    let Some(c) = crate::codec::h264::AvcConfig::parse(avcc) else {
        return Vec::new();
    };
    let ls = c.length_size as usize;
    let mut out = Vec::new();
    for nal in c.sps.iter().chain(c.pps.iter()) {
        let n = nal.len() as u32;
        out.extend_from_slice(&n.to_be_bytes()[4 - ls..]);
        out.extend_from_slice(nal);
    }
    out
}

/// What [`FragReader`] found in the encoder output so far.
enum FragEvent {
    /// The `moov`: the encoder's `avcC`.
    Avcc(Vec<u8>),
    /// One `moof` + `mdat`: samples in decode order (bytes, sync).
    Samples(Vec<(Bytes, bool)>),
}

/// Incremental parser of ffmpeg's fragmented MP4 output on a pipe.
#[derive(Default)]
struct FragReader {
    buf: Vec<u8>,
    /// The H.264 track's ID. Only its samples are read: ffmpeg can add
    /// other tracks, e.g. a QuickTime text track holding the source's
    /// chapters, whose 24-byte samples once ended up in the video pieces.
    video_track: Option<u32>,
    trex_size: u32,
    trex_flags: u32,
}

/// (box size, header length, type) of the box at the start of `b`, if the
/// header is complete.
fn box_header(b: &[u8]) -> Option<(usize, usize, [u8; 4])> {
    if b.len() < 8 {
        return None;
    }
    let size = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize;
    let kind = [b[4], b[5], b[6], b[7]];
    if size == 1 {
        let s = u64::from_be_bytes(b.get(8..16)?.try_into().ok()?) as usize;
        return Some((s, 16, kind));
    }
    Some((size, 8, kind))
}

impl FragReader {
    fn push(&mut self, data: &[u8]) -> Result<Vec<FragEvent>> {
        self.buf.extend_from_slice(data);
        let mut events = Vec::new();
        let mut pos = 0usize;
        while let Some((size, _, kind)) = box_header(&self.buf[pos..]) {
            if size < 8 {
                return Err(HlsError::Ffmpeg("encoder output: bad box size".into()));
            }
            if self.buf.len() - pos < size {
                break;
            }
            match &kind {
                b"moov" => {
                    let moov = mp4_atom::Moov::decode(&mut &self.buf[pos..pos + size])
                        .map_err(|e| HlsError::Ffmpeg(format!("encoder moov: {e}")))?;
                    let video = moov
                        .trak
                        .iter()
                        .find_map(|t| match t.mdia.minf.stbl.stsd.codecs.first() {
                            Some(mp4_atom::Codec::Avc1(a)) => Some((t.tkhd.track_id, a)),
                            _ => None,
                        });
                    if let Some((id, a)) = video {
                        self.video_track = Some(id);
                        let mut v = Vec::new();
                        a.avcc
                            .encode(&mut v)
                            .map_err(|e| HlsError::Ffmpeg(format!("avcC: {e}")))?;
                        if let Some(b) = v.get(8..) {
                            events.push(FragEvent::Avcc(b.to_vec()));
                        }
                    }
                    let trex = moov.mvex.as_ref().and_then(|x| {
                        x.trex
                            .iter()
                            .find(|t| Some(t.track_id) == self.video_track)
                            .or(x.trex.first())
                    });
                    if let Some(t) = trex {
                        self.trex_size = t.default_sample_size;
                        self.trex_flags = t.default_sample_flags;
                    }
                    pos += size;
                }
                b"moof" => {
                    // Wait for the following mdat too.
                    let after = pos + size;
                    let Some((msize, _, mkind)) = box_header(&self.buf[after..]) else {
                        break;
                    };
                    if &mkind != b"mdat" {
                        return Err(HlsError::Ffmpeg("encoder output: moof without mdat".into()));
                    }
                    if self.buf.len() - after < msize {
                        break;
                    }
                    let chunk = Bytes::copy_from_slice(&self.buf[pos..after + msize]);
                    events.push(FragEvent::Samples(self.samples(&chunk, size)?));
                    pos = after + msize;
                }
                _ => pos += size,
            }
        }
        self.buf.drain(..pos);
        Ok(events)
    }

    /// Samples of one `moof`+`mdat` chunk (data offsets relative to the moof).
    fn samples(&self, chunk: &Bytes, moof_size: usize) -> Result<Vec<(Bytes, bool)>> {
        let moof = mp4_atom::Moof::decode(&mut &chunk[..moof_size])
            .map_err(|e| HlsError::Ffmpeg(format!("encoder moof: {e}")))?;
        let mut out = Vec::new();
        let video = |id: u32| self.video_track.is_none_or(|v| v == id);
        for traf in moof.traf.iter().filter(|t| video(t.tfhd.track_id)) {
            // ffmpeg writes default-base-is-moof (we ask for it).
            let base = 0usize;
            for trun in &traf.trun {
                let mut p = base + trun.data_offset.unwrap_or(0).max(0) as usize;
                for e in &trun.entries {
                    let size = e.size.or(traf.tfhd.default_sample_size).unwrap_or(self.trex_size) as usize;
                    let flags = e
                        .flags
                        .or(traf.tfhd.default_sample_flags)
                        .unwrap_or(self.trex_flags);
                    if p + size > chunk.len() {
                        return Err(HlsError::Ffmpeg("encoder output truncated".into()));
                    }
                    out.push((chunk.slice(p..p + size), flags & 0x0001_0000 == 0));
                    p += size;
                }
            }
        }
        Ok(out)
    }
}

impl VideoTranscoder {
    /// `max_jobs` bounds concurrent encoder runs, requests and prefetch
    /// together (0: [`default_max_runs`] for the detected encoder).
    pub fn new(ffmpeg: PathBuf, max_jobs: usize, cache_bytes: usize, cancel: CancellationToken) -> Self {
        VideoTranscoder {
            ffprobe: crate::ffmpeg_tools::default_ffprobe(&ffmpeg),
            ffmpeg,
            detect: DetectOptions::default(),
            caps: tokio::sync::OnceCell::new(),
            permits: Arc::new(Semaphore::new(0)),
            max_jobs,
            max_runs: AtomicUsize::new(0),
            prefetching: Mutex::new(Vec::new()),
            cache: Mutex::new(ByteCache::new(cache_bytes)),
            inflight: Mutex::new(HashMap::new()),
            avcc: Mutex::new(HashMap::new()),
            burns: Mutex::new(HashMap::new()),
            burn_probes: Mutex::new(HashMap::new()),
            stats: Mutex::new(VideoTranscodeStats::default()),
            lookahead: 3,
            piece_ns: 2_000_000_000,
            run_pieces: 4,
            timeout: Duration::from_secs(120),
            cancel,
        }
    }

    /// The key a [`VideoTarget`] that draws `b` into the picture carries.
    /// Registered for the process (a few hundred bytes per subtitle ever
    /// burned in).
    pub fn burn_key(&self, b: BurnIn) -> BurnKey {
        let k = b.key();
        if let Ok(mut m) = self.burns.lock() {
            m.entry(k).or_insert_with(|| Arc::new(b));
        }
        k
    }

    /// A target's burned-in subtitle and its probe (read once, blocking).
    async fn burn_of(
        &self,
        asset: &Arc<Asset>,
        target: &VideoTarget,
    ) -> Result<Option<(Arc<BurnIn>, BurnProbe)>> {
        let Some(k) = target.burn else {
            return Ok(None);
        };
        let b = self
            .burns
            .lock()
            .ok()
            .and_then(|m| m.get(&k).cloned())
            .ok_or_else(|| HlsError::NotFound("burned-in subtitle".into()))?;
        let known = self.burn_probes.lock().ok().and_then(|m| m.get(&k).copied());
        let probe = match known {
            Some(p) => p,
            None => {
                let (a, bb, ffprobe) = (asset.clone(), b.clone(), self.ffprobe.clone());
                let p = tokio::task::spawn_blocking(move || burn::probe(&a, &bb, &ffprobe))
                    .await
                    .map_err(|e| HlsError::Io(std::io::Error::other(e)))?;
                if let Ok(mut m) = self.burn_probes.lock() {
                    m.insert(k, p);
                }
                p
            }
        };
        Ok(Some((b, probe)))
    }

    /// What ffmpeg can do and the encoder transcodes use: detected on the
    /// first call (a few ffmpeg runs and test encodes, on a blocking
    /// thread), then cached.
    pub async fn caps(&self) -> Arc<FfmpegCaps> {
        self.caps
            .get_or_init(|| async {
                let ffmpeg = self.ffmpeg.clone();
                let opts = self.detect.clone();
                let caps = tokio::task::spawn_blocking(move || FfmpegCaps::detect(&ffmpeg, &opts))
                    .await
                    .unwrap_or_else(|e| FfmpegCaps {
                        ffmpeg: self.ffmpeg.clone(),
                        error: Some(format!("detection failed: {e}")),
                        ..Default::default()
                    });
                let runs = match self.max_jobs {
                    0 => default_max_runs(caps.chosen()),
                    n => n,
                };
                self.max_runs.store(runs, Ordering::SeqCst);
                self.permits.add_permits(runs);
                match caps.chosen() {
                    Some(e) => tracing::info!(
                        encoder = %e,
                        hardware = e.is_hardware(),
                        forced = caps.forced.is_some(),
                        tonemap = caps.tonemap_for(e).as_deref().unwrap_or("none"),
                        max_runs = runs,
                        "video transcode encoder"
                    ),
                    None => tracing::warn!(
                        ffmpeg = %self.ffmpeg.display(),
                        error = caps.error.as_deref().unwrap_or(""),
                        "no usable H.264 encoder: video transcodes will fail"
                    ),
                }
                Arc::new(caps)
            })
            .await
            .clone()
    }

    /// [`Self::caps`] if detection has run.
    pub fn caps_if_known(&self) -> Option<Arc<FfmpegCaps>> {
        self.caps.get().cloned()
    }

    /// Concurrent encoder runs, once the encoder is known.
    pub fn max_runs(&self) -> Option<usize> {
        Some(self.max_runs.load(Ordering::SeqCst)).filter(|&n| n > 0)
    }

    /// An encoder slot for a run a request waits on. With every slot busy,
    /// a prefetch run nobody waits on gives up its slot (it is cancelled;
    /// the pieces it finished stay cached), so a first piece or a seek
    /// never queues behind pieces encoded ahead.
    async fn request_permit(&self) -> Result<OwnedSemaphorePermit> {
        if let Ok(p) = self.permits.clone().try_acquire_owned() {
            return Ok(p);
        }
        let idle = self.prefetching.lock().ok().and_then(|runs| {
            runs.iter()
                .rev()
                .find(|r| r.waiting.load(Ordering::SeqCst) == 0 && !r.cancel.is_cancelled())
                .cloned()
        });
        if let Some(r) = idle {
            tracing::debug!("every encoder slot busy: a prefetch run gives its slot to a request");
            r.cancel.cancel();
        }
        self.permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| HlsError::Cancelled)
    }

    /// Statistics so far.
    pub fn stats(&self) -> VideoTranscodeStats {
        self.stats.lock().map(|s| *s).unwrap_or_default()
    }

    /// Segment cache (entries, bytes, evictions).
    pub(crate) fn cache_usage(&self) -> (usize, usize, u64) {
        self.cache.lock().map(|c| c.usage()).unwrap_or_default()
    }

    /// The pieces (output segments) of `asset`.
    pub fn pieces(&self, asset: &Asset) -> Vec<Piece> {
        pieces(asset, self.piece_ns)
    }

    /// ffmpeg arguments for a run covering `run` (consecutive pieces) with
    /// encoder `enc`. `frames` are the run's real source frames when the
    /// index lists them (PB-2): frame counts, the trim point and the forced
    /// IDR times then come from real timestamps, not from a frame rate.
    /// `burn` is the subtitle drawn into the picture and what was read from
    /// it ([`crate::burn`]). A time-based run (an index-less asset) seeks
    /// `seek_back_us` further back than its margin ([`SEEK_BACK_US`]).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn args(
        asset: &Asset,
        target: &VideoTarget,
        run: &[Piece],
        frames: Option<&RunFrames>,
        caps: &FfmpegCaps,
        enc: VideoEncoder,
        vaapi_device: &std::path::Path,
        burn: Option<(&BurnIn, &BurnProbe)>,
        seek_back_us: i64,
    ) -> Result<Vec<String>> {
        let v = asset
            .index
            .video_track()
            .ok_or_else(|| HlsError::Unsupported("no video track".into()))?;
        let first = run
            .first()
            .ok_or_else(|| HlsError::NotFound("empty run".into()))?;
        let span = *asset
            .segments
            .segments
            .get(first.seg)
            .ok_or_else(|| HlsError::NotFound(format!("segment {}", first.seg)))?;
        let ts = asset.segments.timescale.max(1) as f64;
        let fd_s = v
            .frame_duration
            .map(|f| f as f64 / ts)
            .or_else(|| v.frame_rate.map(|r| 1.0 / r.as_f64()))
            .unwrap_or(1.0 / 24.0);
        let (ow, oh) = output_size(asset, target);
        let input = PipelineInput {
            codec: v.codec,
            bit_depth: v.bit_depth,
            rotation: rotation(asset),
            width: target.width,
            height: target.height,
            tonemap: target.tonemap,
            bitrate: target.bitrate,
            level: h264_level(ow, oh, source_fps(asset)),
            dovi_reshape: v
                .hdr
                .as_ref()
                .and_then(|h| h.dovi)
                .is_some_and(|d| d.bl_signal_compat_id == 0),
            keep_aspect: v.width.is_none() || v.height.is_none(),
            deinterlace: deinterlace(v),
        };
        // Burning a subtitle in overlays frames in memory: software decode.
        let (pipe, upload) = match burn {
            Some(_) => crate::encoders::software_pipeline(caps, enc, &input, vaapi_device),
            None => (crate::encoders::pipeline(caps, enc, &input, vaapi_device), None),
        };
        let mut a: Vec<String> = ["-hide_banner", "-nostats", "-loglevel", "error"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // Hardware device and decoder set-up (before the input).
        a.extend(pipe.input_args.iter().cloned());
        // Rotation is applied explicitly in the filter chain, on every
        // decode path (a software decode would otherwise turn the frames
        // before `scale` stretched them back to the landscape target, PB-9).
        a.push("-noautorotate".to_string());
        // Matroska/MP4 keyframe times are absolute container times, and
        // ffmpeg adds the file's start_time to `-ss` unless told the seek
        // point is a timestamp (PB-NEW-1: a file starting at 1 s lost its
        // first second). The other indexes count from the start time.
        if matches!(asset.index.layout, Layout::Matroska(_) | Layout::Mp4(_)) {
            a.extend(["-seek_timestamp".to_string(), "1".into()]);
        }
        // Containers only ffmpeg reads have no keyframe list: the run seeks
        // by time, a margin before its first piece, and ffmpeg's accurate
        // seek decodes from the keyframe before that (`external_window`).
        let window = external_window(asset, first, ts, seek_back_us);
        match (&asset.index.layout, &window) {
            (Layout::Ts(_), _) => {
                // MPEG-TS: ffmpeg's `-ss` lands on any PES by DTS, not on
                // the keyframe (HEVC then misses references and loses
                // frames), so the run reads the source from its keyframe on
                // through stdin ([`Self::ts_input`]); the input's time
                // starts at that keyframe.
                a.extend(["-f", "mpegts", "-i", "pipe:0"].map(String::from));
            }
            (_, Some(w)) => {
                // The input's own timestamps (see `Window`).
                a.push("-copyts".to_string());
                if w.seek_us > 0 {
                    a.extend(["-ss".to_string(), micros(w.seek_us)]);
                }
                a.extend(["-i".to_string(), w.input.clone()]);
            }
            _ => {
                // Start exactly on the source keyframe of the run's segment.
                a.extend([
                    "-ss".to_string(),
                    format!("{:.6}", span.start_ns as f64 / 1e9),
                    "-noaccurate_seek".into(),
                ]);
                a.extend(["-i".to_string(), asset.index.source.path.display().to_string()]);
            }
        }
        // The run's first frame on the source timeline: where the subtitle
        // input is lined up (crate::burn).
        let first_ticks = frames
            .and_then(|f| f.per_piece.first()?.first().copied())
            .unwrap_or(first.start_ticks);
        if let Some((b, probe)) = burn {
            let clock = burn::start_clock_origin_s(&asset.index.layout);
            a.extend(burn::input_args(
                b,
                probe,
                first_ticks as f64 / ts,
                clock,
                window.is_some(),
            ));
        }
        let map = if burn.is_some() { "[out]" } else { "0:v:0" };
        // passthrough: never duplicate/drop frames to fake a constant rate
        // (the default CFR vsync padded the last segment by one frame).
        // Video only: no chapter text track (`-map_chapters`), no metadata
        // (a `udta` per run, a timecode track).
        a.extend(
            [
                "-map",
                map,
                "-an",
                "-sn",
                "-dn",
                "-map_chapters",
                "-1",
                "-map_metadata",
                "-1",
                "-fps_mode",
                "passthrough",
            ]
            .map(String::from),
        );
        // Frames per piece: from the index, else predicted from the frame
        // grid. A converted constant rate (`external_window`) makes the
        // prediction exact, also for the file's last piece.
        let count = |p: &Piece| {
            p.frames.map(u64::from).or_else(|| {
                window
                    .as_ref()
                    .and(v.frame_duration)
                    .map(|fd| ((p.end_ticks - p.start_ticks).max(1) as u64).div_ceil(u64::from(fd.max(1))))
            })
        };
        if run.iter().all(|p| count(p).is_some()) {
            let n: u64 = match frames {
                Some(f) => f.per_piece.iter().map(|x| x.len() as u64).sum(),
                None => run.iter().map(|p| count(p).unwrap_or(0)).sum(),
            };
            a.extend(["-frames:v".to_string(), n.max(1).to_string()]);
        }
        // Drop frames before the piece (decode-and-discard inside the GOP;
        // `trim` also drops open-GOP leading pictures that decode with
        // negative timestamps after `-ss`), then restart the clock at the
        // piece. The cut lies halfway between the last frame dropped and
        // the first kept: half-frame guards absorb the source's ms
        // timestamps, and real neighbours keep VFR sources exact.
        let off = (first.start_ticks - span.start_ticks) as f64 / ts;
        let trim = match frames {
            Some(f) if off > 0.0 => {
                let kept = f.per_piece[0][0];
                let before = f.last_before(first.start_ticks).unwrap_or(kept);
                ((before + kept) as f64 / 2.0 - span.start_ticks as f64) / ts
            }
            _ if off > 0.0 => off - fd_s / 2.0,
            _ => 0.0,
        };
        // Then scale, tone-map and rotate for the encoder (crate::encoders).
        // A time-based run instead puts the piece's start at 0 (in µs, so
        // AVI's 1/fps timebase doesn't round it to a frame; the input's own
        // clock, `-copyts`) and converts to the index's constant rate: `fps`
        // picks the frame on screen at each grid point (those before 0 only
        // for the first), `tpad` repeats the last one if the video ends
        // before the file does.
        // A converted video ([`converted_rate`]) instead starts its clock at
        // the piece (in µs, exact) and takes `fps`'s frames: the frame on
        // screen at each grid point (of those before 0, only the last one).
        // In the run that ends the file, `tpad` repeats the last frame where
        // the video ends early, so `-frames:v` ends the run; without it the
        // last piece takes the frames there are (`piece_cut`).
        let (pre, chain) = match (&window, v.frame_rate, converted_rate(asset)) {
            (Some(w), Some(r), _) => (
                format!("settb=AVTB,setpts=PTS-({})/TB", micros(w.first_us)),
                format!(
                    "{},fps={}/{}:start_time=0,tpad=stop=-1:stop_mode=clone",
                    pipe.filters, r.num, r.den
                ),
            ),
            (_, _, Some(r)) => {
                let us = |s: f64| (s * 1e6).round() as i64;
                let shift = us(first.start_ticks as f64 / ts) - us(span.start_ns as f64 / 1e9);
                let end = asset.segments.segments.last().map_or(i64::MAX, |s| s.end_ticks);
                let tail = run.last().is_some_and(|p| p.end_ticks >= end);
                // `tpad` takes frames in memory only before FFmpeg 6.1 (in
                // clone mode since), so it goes before an upload to the
                // encoder, and not after frames decoded to the device.
                let (filters, upload) = match pipe.filters.strip_suffix(",hwupload") {
                    Some(f) => (f, ",hwupload"),
                    None => (pipe.filters.as_str(), ""),
                };
                let pad = tail && (!pipe.hw_decode || !upload.is_empty());
                (
                    format!("settb=AVTB,setpts=PTS-{}", shift.max(0)),
                    format!(
                        "{filters},fps={}/{}:start_time=0{}{upload}",
                        r.num,
                        r.den,
                        if pad { ",tpad=stop=-1:stop_mode=clone" } else { "" }
                    ),
                )
            }
            _ => (
                format!("trim=start={:.6},setpts=PTS-STARTPTS", trim.max(0.0)),
                pipe.filters.clone(),
            ),
        };
        match burn {
            Some((b, probe)) => {
                let src = (v.width.unwrap_or(ow), v.height.unwrap_or(oh));
                let pix = if enc == VideoEncoder::Libx264 {
                    "yuv420p"
                } else {
                    "nv12"
                };
                let graph = burn::filter_graph(
                    &burn::stream_spec(asset, b),
                    &pre,
                    &chain,
                    pix,
                    upload,
                    probe.canvas.unwrap_or(src),
                    src,
                    (ow, oh),
                );
                a.extend(["-filter_complex".to_string(), graph]);
            }
            None => a.extend(["-vf".to_string(), format!("{pre},{chain}")]),
        }
        // An IDR at every piece boundary after the first: halfway between
        // the piece's first frame and the frame before it, on the output
        // clock (which `setpts` starts at the run's first kept frame, or a
        // converted video's at its first piece).
        let mut kf = Vec::new();
        for (j, p) in run.iter().enumerate().skip(1) {
            let t = match frames {
                Some(f) => {
                    let origin = f.per_piece[0][0];
                    let first_j = f.per_piece[j][0];
                    let before = f.last_before(p.start_ticks).unwrap_or(first_j);
                    ((before + first_j) as f64 / 2.0 - origin as f64) / ts
                }
                None => (p.start_ticks - first.start_ticks) as f64 / ts - fd_s / 2.0,
            };
            kf.push(format!("{:.6}", t.max(0.0)));
        }
        if !kf.is_empty() {
            a.extend(["-force_key_frames".to_string(), kf.join(",")]);
        }
        // H.264 High at the level the size × rate needs (PB-12, the one
        // CODECS says), no B-frames, IDRs only where forced.
        a.extend(pipe.output_args);
        a.extend([
            "-f".to_string(),
            "mp4".into(),
            "-movflags".into(),
            "+frag_keyframe+empty_moov+default_base_moof".into(),
            "pipe:1".into(),
        ]);
        Ok(a)
    }

    /// Feeds an MPEG-TS run's input to ffmpeg's stdin: the program tables,
    /// then the video packets from the keyframe of the run's first segment
    /// on ([`crate::ts::video_input`]), through the segment after its last
    /// one: an open GOP's leading pictures, which end the last segment in
    /// presentation order, come after the next keyframe in decode order. It
    /// stops when ffmpeg has its frames (`-frames:v`) and closes the pipe.
    fn ts_input(asset: &Arc<Asset>, run: &[Piece], mut stdin: tokio::process::ChildStdin) {
        let segs = &asset.segments.segments;
        let from = run.first().and_then(|p| segs.get(p.seg)).map(|s| s.kf);
        let to = run
            .last()
            .and_then(|p| segs.get(p.seg + 1))
            .and_then(|s| s.next_kf);
        let Some(from) = from else { return };
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        let asset = asset.clone();
        tokio::task::spawn_blocking(move || {
            let res = crate::ts::video_input(&asset, from, to, |b| {
                tx.blocking_send(b.to_vec())
                    .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            });
            if let Err(e) = res {
                tracing::debug!(error = %e, "TS transcode input ended early");
            }
        });
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            while let Some(b) = rx.recv().await {
                if stdin.write_all(&b).await.is_err() {
                    break; // ffmpeg has what it needs
                }
            }
            let _ = stdin.shutdown().await;
        });
    }

    /// Packages one piece's encoded samples on the asset's timeline.
    /// `ticks` are the piece's real source frame times (PB-2) and `next`
    /// the time of the frame after it: each sample then lasts until the
    /// next frame, as in the source. Without them (or if the encoder's
    /// count differs) the frame duration, or an even spread, is used.
    fn package(
        asset: &Asset,
        piece: &Piece,
        n: usize,
        avcc: &[u8],
        enc: &[(Bytes, bool)],
        ticks: Option<(&[i64], Option<i64>)>,
    ) -> Result<Bytes> {
        let v = asset
            .index
            .video_track()
            .ok_or_else(|| HlsError::Unsupported("no video track".into()))?;
        if enc.is_empty() {
            return Err(HlsError::Ffmpeg(format!(
                "encoder produced no frames for piece {n}"
            )));
        }
        let total = (piece.end_ticks - piece.start_ticks).max(1);
        let count = enc.len() as i64;
        let ticks = ticks.filter(|(t, _)| t.len() == enc.len());
        let ps = avcc_parameter_sets(avcc);
        let d = v
            .ns_to_ticks(asset.index.presentation_offset_ns)
            .clamp(0, i32::MAX as i64) as i32;
        if !enc[0].1 {
            // Pieces start with a forced IDR; a miscount shows up here.
            tracing::warn!(piece = n, "transcoded piece does not start with a sync sample");
        }
        let mut samples = Vec::with_capacity(enc.len());
        for (i, (data, sync)) in enc.iter().enumerate() {
            let dur = match (ticks, v.frame_duration) {
                // Real source timing: until the next source frame.
                (Some((t, next)), _) => match t.get(i + 1) {
                    Some(nx) => nx - t[i],
                    None => next.unwrap_or(piece.end_ticks) - t[i],
                },
                // CFR: the source frame duration; otherwise spread the span evenly.
                (None, Some(fd)) => fd as i64,
                (None, None) => total * (i as i64 + 1) / count - total * i as i64 / count,
            };
            let data = if i == 0 {
                // In-band SPS/PPS in the IDR: robust to parameter-set drift
                // between independent encoder runs.
                let mut b = Vec::with_capacity(ps.len() + data.len());
                b.extend_from_slice(&ps);
                b.extend_from_slice(data);
                Bytes::from(b)
            } else {
                data.clone()
            };
            samples.push(Sample {
                duration: dur.max(1) as u32,
                cts: d,
                sync: *sync,
                data: SampleData::Bytes(data),
            });
        }
        // Presentation = T + D like the remuxed rendition, with D carried as
        // a constant composition offset rather than in the decode time: hls.js
        // maps WebVTT X-TIMESTAMP-MAP against the first video decode time, so
        // a copy (DTS from 0) and a transcode must both start decoding at 0
        // for the same subtitle playlist to line up (browser check: −83 ms).
        let base = ticks.map_or(piece.start_ticks, |(t, _)| t[0]);
        let frag = Fragment {
            track_id: Asset::track_id(v),
            base_dts: base.max(0) as u64,
            samples,
            video: true,
        };
        Ok(Bytes::from(
            fmp4::media_segment(n as u32 + 1, &[frag]).read_all()?,
        ))
    }

    /// Registers the pieces of a run starting at `from` (up to `run_pieces`,
    /// stopping at the first piece already cached or in flight) and starts
    /// it. Returns the receiver for piece `from` and the run's control, or
    /// `None` if `from` itself is cached or in flight (the caller re-checks).
    /// A prefetch run comes with its encoder slot (`prefetch`); others wait
    /// for one ([`Self::request_permit`]).
    fn start_run(
        self: &Arc<Self>,
        key: &str,
        asset: &Arc<Asset>,
        target: VideoTarget,
        all: &Arc<Vec<Piece>>,
        from: usize,
        prefetch: Option<OwnedSemaphorePermit>,
    ) -> Option<InFlight> {
        let mut senders = Vec::new();
        let first_rx;
        let ctl = Arc::new(RunCtl {
            cancel: self.cancel.child_token(),
            waiting: AtomicUsize::new(0),
        });
        // A run reads one part of an MPEG-PS file of joined recordings
        // (`ExternalLayout::input_at`): it ends where the part does.
        let part = |n: usize| match &asset.index.layout {
            Layout::External(l) if !l.parts.is_empty() => {
                let ts = asset.segments.timescale.max(1) as i128;
                l.part_of((all[n].start_ticks as i128 * 1_000_000_000 / ts) as i64)
            }
            _ => 0,
        };
        {
            let mut inflight = self.inflight.lock().ok()?;
            let cache = self.cache.lock().ok()?;
            let mut rx0 = None;
            for n in from..(from + self.run_pieces.max(1) as usize).min(all.len()) {
                let k: VKey = (key.to_string(), target, n as u32);
                if cache.contains(&k) || inflight.contains_key(&k) || part(n) != part(from) {
                    break;
                }
                let (tx, rx) = watch::channel(None);
                if rx0.is_none() {
                    rx0 = Some(rx.clone());
                }
                inflight.insert(k, (rx, ctl.clone()));
                senders.push((n, tx));
            }
            first_rx = rx0;
        }
        if senders.is_empty() {
            return None;
        }
        let is_prefetch = prefetch.is_some();
        if is_prefetch {
            if let Ok(mut p) = self.prefetching.lock() {
                p.push(ctl.clone());
            }
        }
        let this = self.clone();
        let key = key.to_string();
        let asset = asset.clone();
        let all = all.clone();
        let run_ctl = ctl.clone();
        tokio::spawn(async move {
            let cancel = run_ctl.cancel.clone();
            let res = tokio::select! {
                r = this.run(&key, &asset, target, &all, &senders, prefetch) => r,
                _ = cancel.cancelled() => Err(HlsError::Cancelled),
            };
            if is_prefetch {
                if let Ok(mut p) = this.prefetching.lock() {
                    p.retain(|c| !Arc::ptr_eq(c, &run_ctl));
                }
            }
            // Anything not produced fails its waiters (they may retry).
            let msg: Arc<str> = match &res {
                Ok(()) => Arc::from("encoder run ended early"),
                Err(e) => Arc::from(e.to_string()),
            };
            if let Err(e) = &res {
                tracing::debug!(%e, "video run failed");
            }
            for (n, tx) in &senders {
                // Unregister first, so a waiter that retries starts a new run
                // instead of finding this one again.
                if let Ok(mut m) = this.inflight.lock() {
                    let k = (key.clone(), target, *n as u32);
                    if m.get(&k).is_some_and(|(_, c)| Arc::ptr_eq(c, &run_ctl)) {
                        m.remove(&k);
                    }
                }
                if tx.borrow().is_none() {
                    let _ = tx.send(Some(Err(msg.clone())));
                }
            }
        });
        first_rx.map(|rx| (rx, ctl))
    }

    /// One ffmpeg run producing `senders`' pieces in order.
    async fn run(
        &self,
        key: &str,
        asset: &Arc<Asset>,
        target: VideoTarget,
        all: &[Piece],
        senders: &[(usize, watch::Sender<Slot>)],
        prefetch: Option<OwnedSemaphorePermit>,
    ) -> Result<()> {
        let run: Vec<Piece> = senders.iter().map(|(n, _)| all[*n]).collect();
        // The run's real frames, from the index (block headers / sample
        // table; read before taking an encode permit).
        let frames = {
            let asset = asset.clone();
            let run = run.clone();
            tokio::task::spawn_blocking(move || run_frames(&asset, &run))
                .await
                .map_err(|e| HlsError::Io(std::io::Error::other(e)))??
        };
        let caps = self.caps().await;
        let enc = caps.chosen().ok_or_else(|| {
            HlsError::Unsupported(format!(
                "no usable H.264 encoder in {} ({}); see `flux-server --check`",
                self.ffmpeg.display(),
                caps.error.as_deref().unwrap_or("none passed its test encode")
            ))
        })?;
        let burn = self.burn_of(asset, &target).await?;
        let is_prefetch = prefetch.is_some();
        let _permit = match prefetch {
            Some(p) => p,
            None => self.request_permit().await?,
        };
        // Index-less assets seek by time (`external_window`).
        let time_based = matches!(asset.index.layout, Layout::External(_));
        let mut attempt = 0usize;
        let produced = loop {
            let args = Self::args(
                asset,
                &target,
                &run,
                frames.as_ref(),
                &caps,
                enc,
                &self.detect.vaapi_device,
                burn.as_ref().map(|(b, p)| (b.as_ref(), p)),
                SEEK_BACK_US[attempt],
            )?;
            tracing::debug!(first = run[0].seg, pieces = run.len(), %enc, ?args, "video run");
            let t0 = Instant::now();
            if let Ok(mut s) = self.stats.lock() {
                s.runs += 1;
            }
            let ts = matches!(asset.index.layout, Layout::Ts(_));
            let mut child = tokio::process::Command::from(flux_core::tool::command(&self.ffmpeg))
                .args(&args)
                .stdin(if ts { Stdio::piped() } else { Stdio::null() })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| HlsError::Ffmpeg(format!("spawn {}: {e}", self.ffmpeg.display())))?;
            if let (true, Some(stdin)) = (ts, child.stdin.take()) {
                Self::ts_input(asset, &run, stdin);
            }
            let mut stdout = child
                .stdout
                .take()
                .ok_or_else(|| HlsError::Ffmpeg("no stdout".into()))?;
            let mut stderr = child
                .stderr
                .take()
                .ok_or_else(|| HlsError::Ffmpeg("no stderr".into()))?;
            let err_task = tokio::spawn(async move {
                let mut e = Vec::new();
                let _ = stderr.read_to_end(&mut e).await;
                e
            });
            let work = async {
                let mut reader = FragReader::default();
                let mut avcc: Option<Vec<u8>> = None;
                let mut pending: Vec<(Bytes, bool)> = Vec::new();
                let mut next = 0usize; // index into `senders`
                let mut buf = vec![0u8; 1 << 20];
                loop {
                    let n = stdout.read(&mut buf).await?;
                    let events = if n == 0 {
                        Vec::new()
                    } else {
                        reader.push(&buf[..n])?
                    };
                    for ev in events {
                        match ev {
                            FragEvent::Avcc(a) => {
                                if let Ok(mut m) = self.avcc.lock() {
                                    m.entry((key.to_string(), target)).or_insert_with(|| a.clone());
                                }
                                avcc = Some(a);
                            }
                            FragEvent::Samples(s) => pending.extend(s),
                        }
                    }
                    // Publish every piece whose frames are complete.
                    while let Some((pn, tx)) = senders.get(next) {
                        let piece = all[*pn];
                        let real = frames.as_ref().map(|f| &f.per_piece[next]);
                        let last = next + 1 == senders.len();
                        // The frame count from the index (real timestamps), else
                        // predicted from the frame grid (sources only ffprobe
                        // indexes, TS). Either way the piece ends at the IDR the
                        // encoder forced at its boundary: a frame the source
                        // lacks, or one the decoder drops, shifts the count.
                        let count = match (piece.frames, real) {
                            (Some(_), Some(r)) => Some(r.len()),
                            (Some(f), None) => Some(f as usize),
                            (None, _) => None,
                        };
                        let want = match count {
                            Some(f) => match piece_cut(&pending, f, last, n == 0) {
                                Some(w) => w,
                                None => break,
                            },
                            None if n == 0 => pending.len(),
                            None => break,
                        };
                        if pending.len() < want || want == 0 {
                            break;
                        }
                        // (The file's last piece has no count: it takes the rest.)
                        if let (Some(_), Some(frames)) = (real, count.filter(|&c| c != want)) {
                            tracing::warn!(
                                piece = *pn,
                                frames,
                                cut = want,
                                "transcoded piece cut at the encoder's IDR, not at the index's frame count"
                            );
                        }
                        let a = avcc
                            .as_deref()
                            .ok_or_else(|| HlsError::Ffmpeg("encoder output has no avcC".into()))?;
                        let rest = pending.split_off(want.min(pending.len()));
                        let ticks = match (&frames, real) {
                            (Some(f), Some(r)) => Some((r.as_slice(), f.first_at_or_after(piece.end_ticks))),
                            _ => None,
                        };
                        let bytes = Self::package(asset, &piece, *pn, a, &pending, ticks)?;
                        pending = rest;
                        let ms = t0.elapsed().as_secs_f64() * 1000.0;
                        if let Ok(mut c) = self.cache.lock() {
                            c.put((key.to_string(), target, *pn as u32), bytes.clone());
                        }
                        if let Ok(mut s) = self.stats.lock() {
                            s.segments += 1;
                            s.total_ms += ms;
                            s.max_ms = s.max_ms.max(ms);
                            if next == 0 {
                                s.first_piece_ms += ms;
                            }
                            if is_prefetch {
                                s.prefetched += 1;
                            }
                        }
                        tracing::debug!(piece = *pn, ms, prefetch = is_prefetch, "video piece transcoded");
                        let _ = tx.send(Some(Ok(bytes)));
                        next += 1;
                    }
                    if n == 0 {
                        break;
                    }
                }
                Ok::<_, HlsError>(next)
            };
            let produced = match tokio::time::timeout(self.timeout, work).await {
                Ok(r) => r?,
                Err(_) => return Err(HlsError::Ffmpeg("video encode timed out".into())),
            };
            let status = child.wait().await?;
            let err = err_task.await.unwrap_or_default();
            if !status.success() {
                return Err(HlsError::Ffmpeg(format!(
                    "exit {status}: {}",
                    String::from_utf8_lossy(&err).trim()
                )));
            }
            // A time-based run whose seek found no frame: again, from earlier.
            if produced == 0 && time_based && attempt + 1 < SEEK_BACK_US.len() {
                attempt += 1;
                tracing::debug!(
                    first = run[0].seg,
                    back_s = SEEK_BACK_US[attempt] / 1_000_000,
                    "video run decoded no frame after its seek; seeking further back"
                );
                continue;
            }
            break produced;
        };
        if produced < senders.len() {
            return Err(HlsError::Ffmpeg(format!(
                "encoder stopped after {produced} of {} pieces",
                senders.len()
            )));
        }
        Ok(())
    }

    /// Piece `n`: from cache, from a run in flight, or a new run.
    async fn get(
        self: &Arc<Self>,
        key: &str,
        asset: &Arc<Asset>,
        target: VideoTarget,
        all: &Arc<Vec<Piece>>,
        n: usize,
    ) -> Result<Bytes> {
        if n >= all.len() {
            return Err(HlsError::NotFound(format!("piece {n}")));
        }
        let k: VKey = (key.to_string(), target, n as u32);
        for _ in 0..3 {
            if let Some(b) = self.cache.lock().ok().and_then(|mut c| c.get(&k)) {
                if let Ok(mut s) = self.stats.lock() {
                    s.cache_hits += 1;
                }
                return Ok(b);
            }
            let existing = self.inflight.lock().ok().and_then(|m| m.get(&k).cloned());
            let rx = match existing {
                Some(rx) => Some(rx),
                None => self.start_run(key, asset, target, all, n, None),
            };
            let Some((mut rx, ctl)) = rx else { continue };
            // While this request waits it keeps the run alive; dropped
            // unserved (the player went elsewhere), it may cancel it.
            let mut waiter = Waiter::new(ctl.clone());
            // `None`: the run went away without a result; try again.
            let slot: Slot = match rx.wait_for(|s| s.is_some()).await {
                Ok(v) => v.clone(),
                Err(_) => None,
            };
            waiter.served = true;
            drop(waiter);
            match slot {
                Some(Ok(b)) => return Ok(b),
                // A run another request abandoned: start a fresh one.
                Some(Err(_)) if ctl.cancel.is_cancelled() && !self.cancel.is_cancelled() => continue,
                Some(Err(e)) => return Err(HlsError::Ffmpeg(e.to_string())),
                None => continue,
            }
        }
        Err(HlsError::Ffmpeg(format!("piece {n} was not produced")))
    }

    /// Starts a prefetch run for the first piece after `n` (within the
    /// lookahead) that is neither cached nor in flight, in a spare encoder
    /// slot: one slot stays free for the next request.
    fn prefetch_after(
        self: &Arc<Self>,
        key: &str,
        asset: &Arc<Asset>,
        target: VideoTarget,
        all: &Arc<Vec<Piece>>,
        n: usize,
    ) {
        if self.permits.available_permits() < 2 {
            return;
        }
        for m in n + 1..=(n + self.lookahead as usize).min(all.len().saturating_sub(1)) {
            let k: VKey = (key.to_string(), target, m as u32);
            let known = self.cache.lock().map(|c| c.contains(&k)).unwrap_or(true)
                || self.inflight.lock().map(|i| i.contains_key(&k)).unwrap_or(true);
            if !known {
                if let Ok(slot) = self.permits.clone().try_acquire_owned() {
                    let _ = self.start_run(key, asset, target, all, m, Some(slot));
                }
                return;
            }
        }
    }

    /// Starts producing piece `n` (and the run after it) in the background.
    pub fn warm(self: &Arc<Self>, key: &str, asset: Arc<Asset>, target: VideoTarget, n: usize) {
        let all = Arc::new(self.pieces(&asset));
        if n < all.len() {
            let k: VKey = (key.to_string(), target, n as u32);
            let known = self.cache.lock().map(|c| c.contains(&k)).unwrap_or(true)
                || self.inflight.lock().map(|i| i.contains_key(&k)).unwrap_or(true);
            if !known {
                let _ = self.start_run(key, &asset, target, &all, n, None);
            }
        }
    }

    /// Init segment: needs the encoder's avcC, so it waits for the run in
    /// flight for this recipe (or starts one at piece 0).
    pub async fn init_segment(
        self: &Arc<Self>,
        key: &str,
        asset: Arc<Asset>,
        target: VideoTarget,
    ) -> Result<Vec<u8>> {
        let known = |this: &Self| {
            this.avcc
                .lock()
                .ok()
                .and_then(|m| m.get(&(key.to_string(), target)).cloned())
        };
        let avcc = match known(self) {
            Some(a) => a,
            None => {
                // The lowest piece in flight for this recipe, else piece 0.
                let all = Arc::new(self.pieces(&asset));
                let first = self.inflight.lock().ok().and_then(|m| {
                    m.keys()
                        .filter(|k| k.0 == key && k.1 == target)
                        .map(|k| k.2 as usize)
                        .min()
                });
                self.get(key, &asset, target, &all, first.unwrap_or(0)).await?;
                known(self).ok_or_else(|| HlsError::Ffmpeg("no avcC after the first piece".into()))?
            }
        };
        let v = asset
            .index
            .video_track()
            .ok_or_else(|| HlsError::Unsupported("no video track".into()))?;
        let color = if target.tonemap {
            Some(ColorInfo {
                primaries: Some(1),
                transfer: Some(1),
                matrix: Some(1),
                full_range: Some(false),
            })
        } else {
            v.color
        };
        // The pixels are turned upright (no matrix); uniform scaling keeps
        // the source's pixel aspect, which a quarter turn inverts.
        let (width, height) = output_size(&asset, &target);
        let pasp = match (v.extra.sar, rotation(&asset)) {
            (Some((h, w)), 90 | 270) => Some((w, h)),
            (sar, _) => sar,
        };
        let spec = InitSpec {
            track_id: Asset::track_id(v),
            timescale: v.timescale,
            language: pack_language(None),
            entry: EntrySpec::Video {
                fourcc: *b"avc1",
                config_box: *b"avcC",
                config: avcc,
                width: width as u16,
                height: height as u16,
                color,
                mastering: None,
                cll: None,
                dovi: None,
                pasp,
                matrix: None,
            },
        };
        Ok(fmp4::init_segment(&[spec]))
    }

    /// Piece `n` of the transcoded rendition; then keeps the next pieces
    /// encoding in the background.
    pub async fn segment(
        self: &Arc<Self>,
        key: &str,
        asset: Arc<Asset>,
        target: VideoTarget,
        n: usize,
    ) -> Result<SegmentBody> {
        let all = Arc::new(self.pieces(&asset));
        let bytes = self.get(key, &asset, target, &all, n).await?;
        self.prefetch_after(key, &asset, target, &all, n);
        Ok(SegmentBody::from_bytes(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(syncs: &[usize], len: usize) -> Vec<(Bytes, bool)> {
        (0..len).map(|i| (Bytes::new(), syncs.contains(&i))).collect()
    }

    #[test]
    fn pieces_end_at_the_forced_idr_nearby() {
        // `out(syncs, len)` is whole fragments: the sample after the last
        // one (index `len`) starts the next fragment, a sync sample.
        // The source lacks a frame: the IDR forced at the boundary is one
        // sample before the count.
        let p = out(&[0, 51, 103], 160);
        assert_eq!(piece_cut(&p, 52, false, false), Some(51));
        // On the grid: exactly the count.
        let p = out(&[0, 52], 160);
        assert_eq!(piece_cut(&p, 52, false, false), Some(52));
        // No IDR near the boundary (an encoder that ignored the request):
        // the count, as before.
        let p = out(&[0, 40], 160);
        assert_eq!(piece_cut(&p, 52, false, false), Some(52));
        // The piece's fragment is complete: its end is the next IDR, here
        // one frame after the count; no need to wait for the next fragment.
        let p = out(&[0], 52);
        assert_eq!(piece_cut(&p, 52, false, false), Some(52));
        let p = out(&[0], 53);
        assert_eq!(piece_cut(&p, 52, false, false), Some(53));
        // The piece's fragment is complete and ends early: the boundary IDR
        // came up to CUT_SLACK frames before the count. Cut now, without
        // waiting for the next piece's fragment.
        let p = out(&[0], 51);
        assert_eq!(piece_cut(&p, 52, false, false), Some(51));
        let p = out(&[0], 50);
        assert_eq!(piece_cut(&p, 52, false, false), Some(50));
        let p = out(&[0], 49);
        assert_eq!(piece_cut(&p, 52, false, false), Some(49));
        // The same piece once the next fragment is there too: the same cut.
        let p = out(&[0, 51], 103);
        assert_eq!(piece_cut(&p, 52, false, false), Some(51));
        // Further short than the slack: not all of the piece's frames yet.
        let p = out(&[0], 48);
        assert_eq!(piece_cut(&p, 52, false, false), None);
        // The encoder is done: the IDRs it made are all there is.
        let p = out(&[0, 50], 120);
        assert_eq!(piece_cut(&p, 52, false, true), Some(50));
        let p = out(&[0], 50);
        assert_eq!(piece_cut(&p, 52, false, true), None);
        // The run's last piece takes what the encoder produced up to its
        // count, and leaves out a frame read past the run's end.
        let p = out(&[0], 53);
        assert_eq!(piece_cut(&p, 52, true, false), Some(52));
        assert_eq!(piece_cut(&p, 52, true, true), Some(52));
        let p = out(&[0], 50);
        assert_eq!(piece_cut(&p, 52, true, false), None);
        assert_eq!(piece_cut(&p, 52, true, true), Some(50));
    }

    /// The chapter text track ffmpeg copies from the source (a 15-chapter
    /// film) is not video: its samples stay out of the pieces.
    #[test]
    fn frag_reader_reads_only_the_video_track() {
        use mp4_atom::{Mfhd, Moof, Tfhd, Traf, Trun, TrunEntry};
        let traf = |track_id: u32, sizes: &[u32], data_offset: i32| Traf {
            tfhd: Tfhd {
                track_id,
                default_sample_flags: Some(0x0101_0000),
                default_base_is_moof: true,
                ..Default::default()
            },
            trun: vec![Trun {
                data_offset: Some(data_offset),
                entries: sizes
                    .iter()
                    .enumerate()
                    .map(|(i, &size)| TrunEntry {
                        size: Some(size),
                        flags: (i == 0).then_some(0x0200_0000),
                        ..Default::default()
                    })
                    .collect(),
            }],
            ..Default::default()
        };
        let video: Vec<Vec<u8>> = vec![vec![0, 0, 0, 3, 0x65, 1, 2], vec![0, 0, 0, 2, 0x41, 3]];
        let chapters: Vec<Vec<u8>> = (1..=2)
            .map(|i| format!("\0\nChapter 0{i}").into_bytes())
            .collect();
        let sizes = |v: &[Vec<u8>]| v.iter().map(|x| x.len() as u32).collect::<Vec<_>>();
        let moof = |off_v: i32, off_c: i32| Moof {
            mfhd: Mfhd { sequence_number: 1 },
            traf: vec![traf(1, &sizes(&video), off_v), traf(2, &sizes(&chapters), off_c)],
        };
        let mut m = Vec::new();
        moof(0, 0).encode(&mut m).unwrap();
        let vlen: usize = video.iter().map(|x| x.len()).sum();
        let first = (m.len() + 8) as i32;
        m.clear();
        moof(first, first + vlen as i32).encode(&mut m).unwrap();
        let body: Vec<u8> = video.iter().chain(&chapters).flatten().copied().collect();
        m.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
        m.extend_from_slice(b"mdat");
        m.extend_from_slice(&body);
        let mut r = FragReader {
            video_track: Some(1),
            ..Default::default()
        };
        let ev = r.push(&m).unwrap();
        let [FragEvent::Samples(s)] = ev.as_slice() else {
            panic!("one fragment");
        };
        let got: Vec<(Vec<u8>, bool)> = s.iter().map(|(b, k)| (b.to_vec(), *k)).collect();
        assert_eq!(got, vec![(video[0].clone(), true), (video[1].clone(), false)]);
    }

    /// The encoder run maps the video alone (no chapter track, no
    /// metadata) and deinterlaces an interlaced H.264 source before scaling.
    /// Interlaced is the container's flag, else the SPS's, as flux-probe
    /// has it: an SPS that allows field coding under a container flagged
    /// progressive (x264 `fake-interlaced`) is progressive, and plans from
    /// the probe and from the index agree on it.
    #[test]
    fn runs_map_video_only_and_deinterlace_interlaced_sources() {
        use crate::encoders::Deinterlace::{All, Marked, Off};
        use crate::test_mkv::{TestMkv, PPS, SPS_INTERLACED, SPS_PROGRESSIVE};
        let target = VideoTarget {
            width: 64,
            height: 64,
            bitrate: 1_000_000,
            tonemap: false,
            burn: None,
        };
        let caps = FfmpegCaps {
            filters: ["bwdif", "yadif"].iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        // (SPS, FlagInterlaced, interlaced, deinterlacer)
        let cases = [
            (SPS_INTERLACED, None, true, Marked),
            (SPS_PROGRESSIVE, None, false, Off),
            // x264 fake-interlaced, as ffmpeg writes it.
            (SPS_INTERLACED, Some(2), false, Off),
            (SPS_INTERLACED, Some(1), true, Marked),
            // An interlaced source encoded as progressive frames.
            (SPS_PROGRESSIVE, Some(1), true, All),
            (SPS_PROGRESSIVE, Some(0), false, Off),
        ];
        for (n, (sps, flag, interlaced, mode)) in cases.into_iter().enumerate() {
            let (path, _) = TestMkv {
                sps,
                pps: PPS,
                inband: vec![vec![]; 3],
                flag_interlaced: flag,
            }
            .write(&format!("run-args-{n}.mkv"));
            let case = format!("case {n}: {flag:?}, frame_mbs_only {}", sps == SPS_PROGRESSIVE);
            let asset = Asset::open(&path, 1_000_000_000).unwrap();
            let v = asset.index.video_track().unwrap();
            assert_eq!(v.coded_interlaced(), Some(sps == SPS_INTERLACED), "{case}");
            assert_eq!(v.interlaced(), Some(interlaced), "{case}");
            assert_eq!(deinterlace(v), mode, "{case}");
            let index_probe = asset.index.to_probe_result();
            assert_eq!(
                index_probe.main_video().unwrap().interlaced,
                Some(interlaced),
                "{case}"
            );
            let probe = flux_probe::probe_file(&path).unwrap();
            assert_eq!(probe.main_video().unwrap().interlaced, Some(interlaced), "{case}");
            for client in [crate::presets::chrome_desktop(), crate::presets::safari_macos()] {
                let prefs = crate::PlanPrefs::default();
                let from_index = crate::planner::plan_with_index_facts(&index_probe, &client, &prefs, false);
                let from_probe = crate::planner::plan(&probe, &client, &prefs);
                assert_eq!(from_index.video, from_probe.video, "{case}, {}", client.client);
                assert_eq!(
                    from_index.reasons, from_probe.reasons,
                    "{case}, {}",
                    client.client
                );
                assert_eq!(
                    matches!(from_index.video, VideoOp::Transcode { .. }),
                    interlaced,
                    "{case}, {}: {:?}",
                    client.client,
                    from_index.reasons
                );
            }
            let all = pieces(&asset, 2_000_000_000);
            let run = &all[..2];
            let frames = run_frames(&asset, run).unwrap();
            let a = VideoTranscoder::args(
                &asset,
                &target,
                run,
                frames.as_ref(),
                &caps,
                VideoEncoder::Libx264,
                std::path::Path::new("/dev/dri/renderD128"),
                None,
                0,
            )
            .unwrap();
            assert!(a.windows(2).any(|w| w == ["-map_chapters", "-1"]), "{a:?}");
            assert!(a.windows(2).any(|w| w == ["-map_metadata", "-1"]), "{a:?}");
            let vf = &a[a.iter().position(|x| x == "-vf").unwrap() + 1];
            let deint = match mode {
                Off => None,
                Marked => Some("bwdif=mode=send_frame:parity=auto:deint=interlaced,scale=64:64"),
                All => Some("bwdif=mode=send_frame:parity=auto:deint=all,scale=64:64"),
            };
            match deint {
                Some(d) => assert!(vf.contains(d), "{case}: {vf}"),
                None => assert!(!vf.contains("bwdif") && !vf.contains("yadif"), "{case}: {vf}"),
            }
            let _ = std::fs::remove_file(&path);
        }
    }

    /// A segment plan on a 90 kHz clock with keyframes at these ticks,
    /// ending at `end`.
    fn plan_90k(keyframes: &[i64], end: i64) -> SegmentPlan {
        let ns = |t: i64| t * 1_000_000_000 / 90_000;
        SegmentPlan {
            target_ns: 6_000_000_000,
            timescale: 90_000,
            segments: keyframes
                .iter()
                .enumerate()
                .map(|(i, &k)| {
                    let e = keyframes.get(i + 1).copied().unwrap_or(end);
                    crate::segment::Span {
                        kf: i,
                        next_kf: (i + 1 < keyframes.len()).then_some(i + 1),
                        start_ns: ns(k),
                        end_ns: ns(e),
                        start_ticks: k,
                        end_ticks: e,
                        approx_bytes: 0,
                    }
                })
                .collect(),
        }
    }

    /// The keyframe spans of a 23.976 fps MP4 cut without re-encoding:
    /// 250 frames (10.4 s), and 20.9 s, 52.1 s and 31.3 s around frames held
    /// for seconds. Converted to 24000/1001, its pieces are 48 frames
    /// (2.002 s, 180 180 ticks) of one grid across all of them, contiguous,
    /// each in the segment whose keyframe its run seeks to; the last takes
    /// the rest. One piece per span made pieces of up to 52 s.
    #[test]
    fn converted_pieces_lie_on_one_grid_across_long_spans() {
        let kfs = [0, 938_438, 2_815_313, 3_753_750, 8_445_938, 11_261_250];
        let end = 12_199_688;
        let plan = plan_90k(&kfs, end);
        let rate = Rational {
            num: 24000,
            den: 1001,
        };
        let all = grid_pieces(&plan, rate, 2_000_000_000);
        // round(12 199 688 / 3753.75) = 3250 frames: 67 pieces of 48 and 34.
        assert_eq!(all.len(), 68);
        assert_eq!(all[0].start_ticks, 0);
        assert_eq!(all.last().unwrap().end_ticks, end);
        for (k, p) in all.iter().enumerate() {
            if k + 1 < all.len() {
                assert_eq!(p.frames, Some(48), "piece {k}");
                assert_eq!(p.start_ticks, k as i64 * 180_180, "piece {k}");
                assert_eq!(p.end_ticks, all[k + 1].start_ticks, "piece {k}");
            }
            let s = &plan.segments[p.seg];
            assert!(
                s.start_ticks <= p.start_ticks && p.start_ticks < s.end_ticks,
                "piece {k}: {p:?}"
            );
        }
        assert_eq!(all.last().unwrap().frames, Some(3250 - 67 * 48));
        let longest = all.iter().map(|p| p.end_ticks - p.start_ticks).max().unwrap();
        assert!(longest <= 180_180, "{longest}");
        // Every segment's keyframe opens a run of the pieces starting in it.
        for seg in 0..kfs.len() {
            let p = first_piece_of(&all, seg);
            assert!(all[p].seg >= seg && (p == 0 || all[p - 1].seg < seg));
        }
        // Half a piece left at the end goes into the last piece.
        let short = grid_pieces(&plan_90k(&[0], 48 * 3754 + 20 * 3754), rate, 2_000_000_000);
        assert_eq!(short.iter().map(|p| p.frames).collect::<Vec<_>>(), [Some(68)]);
    }

    /// A run over a converted MP4 ([`Clip::HeldFramesMp4`]: 23.976 fps on a
    /// 90 kHz clock with frames held for seconds) seeks to the keyframe of
    /// its first piece's segment, starts the clock at that piece, converts
    /// to 24000/1001 there, and forces an IDR half a frame before each
    /// piece. The index's frames are not used: the decoder drops some.
    #[test]
    fn converted_runs_put_the_rate_grid_on_their_first_piece() {
        use flux_test_media::Clip;
        let Some(path) = flux_test_media::clip(Clip::HeldFramesMp4) else {
            return;
        };
        let asset = Asset::open(&path, 6_000_000_000).unwrap();
        let v = asset.index.video_track().unwrap();
        assert_eq!(v.frame_duration, None);
        let rate = Rational {
            num: 24000,
            den: 1001,
        };
        assert_eq!(v.frame_rate, Some(rate));
        assert_eq!(converted_rate(&asset), Some(rate));
        let all = pieces(&asset, 2_000_000_000);
        assert!(all.len() >= 10, "{all:?}");
        let run = &all[3..7];
        assert!(run_frames(&asset, run).unwrap().is_none());
        let target = VideoTarget {
            width: 320,
            height: 180,
            bitrate: 500_000,
            tonemap: false,
            burn: None,
        };
        let a = VideoTranscoder::args(
            &asset,
            &target,
            run,
            None,
            &FfmpegCaps::default(),
            VideoEncoder::Libx264,
            std::path::Path::new("/dev/dri/renderD128"),
            None,
            0,
        )
        .unwrap();
        let arg = |k: &str| a[a.iter().position(|x| x == k).unwrap() + 1].clone();
        let span = asset.segments.segments[run[0].seg];
        assert_eq!(arg("-ss"), format!("{:.6}", span.start_ns as f64 / 1e9));
        assert_eq!(arg("-frames:v"), "192");
        // Piece 3 starts 3 × 2.002 s in.
        let shift = 6_006_000 - (span.start_ns as f64 / 1e3).round() as i64;
        let vf = arg("-vf");
        assert!(vf.starts_with(&format!("settb=AVTB,setpts=PTS-{shift},")), "{vf}");
        assert!(vf.ends_with(",fps=24000/1001:start_time=0"), "{vf}");
        assert!(!vf.contains("trim"), "{vf}");
        assert_eq!(arg("-force_key_frames"), "1.981146,3.983146,5.985146");
        // The run that ends the file pads with its last frame, should the
        // decoder drop frames at the end.
        let tail = &all[all.len() - 2..];
        let a = VideoTranscoder::args(
            &asset,
            &target,
            tail,
            None,
            &FfmpegCaps::default(),
            VideoEncoder::Libx264,
            std::path::Path::new("/dev/dri/renderD128"),
            None,
            0,
        )
        .unwrap();
        let vf = &a[a.iter().position(|x| x == "-vf").unwrap() + 1];
        assert!(
            vf.ends_with(",fps=24000/1001:start_time=0,tpad=stop=-1:stop_mode=clone"),
            "{vf}"
        );
        let n: u32 = tail.iter().map(|p| p.frames.unwrap()).sum();
        assert_eq!(
            a[a.iter().position(|x| x == "-frames:v").unwrap() + 1],
            n.to_string()
        );
    }

    /// The run that ends a converted video pads with `tpad` only frames in
    /// memory: before FFmpeg 6.1 `tpad` takes no hardware frames, and the
    /// run of the file's last pieces failed to start with every chain that
    /// decodes to the device (CUDA, QSV, VAAPI, VideoToolbox). Before an
    /// upload to the encoder (VAAPI after a software chain) the frames are
    /// still in memory, so `fps` and `tpad` go there. Without `tpad` the
    /// last piece takes the frames there are (`piece_cut`).
    #[test]
    fn converted_runs_pad_the_end_only_in_memory() {
        use flux_test_media::Clip;
        let Some(path) = flux_test_media::clip(Clip::HeldFramesMp4) else {
            return;
        };
        let asset = Asset::open(&path, 6_000_000_000).unwrap();
        let all = pieces(&asset, 2_000_000_000);
        let tail = &all[all.len() - 2..];
        let target = VideoTarget {
            width: 320,
            height: 180,
            bitrate: 500_000,
            tonemap: false,
            burn: None,
        };
        let set =
            |v: &[&str]| -> std::collections::BTreeSet<String> { v.iter().map(|s| s.to_string()).collect() };
        let caps = |hwaccels: &[&str], filters: &[&str], decoders: &[&str]| FfmpegCaps {
            hwaccels: set(hwaccels),
            filters: set(filters),
            decoders: set(decoders),
            ..Default::default()
        };
        let fps = ",fps=24000/1001:start_time=0";
        let pad = ",tpad=stop=-1:stop_mode=clone";
        let cases = [
            (VideoEncoder::Libx264, caps(&[], &[], &[]), format!("{fps}{pad}")),
            (VideoEncoder::Nvenc, caps(&[], &[], &[]), format!("{fps}{pad}")),
            (
                VideoEncoder::Nvenc,
                caps(&["cuda"], &["scale_cuda"], &[]),
                fps.to_string(),
            ),
            (
                VideoEncoder::Qsv,
                caps(&["qsv"], &["scale_qsv"], &["h264_qsv"]),
                fps.to_string(),
            ),
            (
                VideoEncoder::VideoToolbox,
                caps(&["videotoolbox"], &["scale_vt"], &[]),
                fps.to_string(),
            ),
            (
                VideoEncoder::Vaapi,
                caps(&["vaapi"], &["scale_vaapi"], &[]),
                fps.to_string(),
            ),
            (
                VideoEncoder::Vaapi,
                caps(&[], &[], &[]),
                format!("{fps}{pad},hwupload"),
            ),
        ];
        for (enc, caps, end) in cases {
            for run in [tail, &all[..2]] {
                let a = VideoTranscoder::args(
                    &asset,
                    &target,
                    run,
                    None,
                    &caps,
                    enc,
                    std::path::Path::new("/dev/dri/renderD128"),
                    None,
                    0,
                )
                .unwrap();
                let vf = &a[a.iter().position(|x| x == "-vf").unwrap() + 1];
                let end = if run == tail {
                    end.clone()
                } else {
                    end.replace(pad, "")
                };
                assert!(vf.ends_with(&end), "{enc:?}: {vf}");
                assert_eq!(vf.matches("fps=").count(), 1, "{enc:?}: {vf}");
                let n: u32 = run.iter().map(|p| p.frames.unwrap()).sum();
                assert_eq!(
                    a[a.iter().position(|x| x == "-frames:v").unwrap() + 1],
                    n.to_string()
                );
            }
        }
    }

    /// Which MP4 videos a transcode converts to their rate ([`converts`]):
    /// those without a constant duration or with frames held, as long as
    /// no stretch of frames comes faster than the rate, where `fps` would
    /// drop frames. A coarse clock's alternating deltas, and a sample or
    /// two of one tick at a cut, are not faster frames.
    #[test]
    fn videos_with_faster_frames_keep_their_timestamps() {
        let r = |num, den| Rational { num, den };
        let ntsc24 = r(24000, 1001);
        let ntsc60 = r(60000, 1001);
        // 23.976 fps at 90 kHz, with a frame held 2 s: converted, as are
        // its alternating deltas alone.
        let mut held: Vec<(u32, u32)> = Vec::new();
        for k in 0..200 {
            held.extend([(2, 3754), (1, 3753)]);
            if k == 100 {
                held.push((1, 180_000));
            }
        }
        assert!(converts(&held, 90_000, None, ntsc24));
        assert!(converts(&held[..20], 90_000, None, ntsc24));
        // A cut whose muxer gave two samples a tick to keep decode times
        // rising.
        let mut glitch = held.clone();
        glitch.insert(150, (2, 1));
        assert!(converts(&glitch, 90_000, None, ntsc24));
        // A constant duration: converted only with a frame held.
        assert!(!converts(&[(750, 3600)], 90_000, Some(3600), r(25, 1)));
        assert!(converts(
            &[(700, 3600), (1, 187_200), (49, 3600)],
            90_000,
            Some(3600),
            r(25, 1)
        ));
        // 10 s at 30 fps, then 30 s at 15 fps, rated 15 fps (most frames):
        // converting dropped every other frame of the 30 fps part.
        assert!(!converts(&[(300, 3000), (450, 6000)], 90_000, None, r(15, 1)));
        // Half a second at 60 fps among 30 fps frames, likewise.
        assert!(!converts(
            &[(300, 3000), (FASTER_RUN as u32, 1500), (300, 3000)],
            90_000,
            None,
            r(30, 1)
        ));
        // Mostly 30 fps with stretches at 15: converting at 30 only repeats
        // frames.
        assert!(converts(
            &[(450, 3000), (150, 6000), (300, 3000)],
            90_000,
            None,
            r(30, 1)
        ));
        // 59.94 fps on coarse clocks: 17/16 ticks at 1 kHz, 272/256 at
        // 16 kHz (Matroska's milliseconds), 10/11 at 600 Hz.
        let alt = |a: u32, b: u32| -> Vec<(u32, u32)> { (0..200).flat_map(|_| [(2, a), (1, b)]).collect() };
        assert!(converts(&alt(17, 16), 1000, None, ntsc60));
        assert!(converts(&alt(272, 256), 16_000, None, ntsc60));
        assert!(converts(
            &[(99, 10), (1, 11), (99, 10), (1, 11)],
            600,
            None,
            ntsc60
        ));
        // No usable rate or clock.
        assert!(!converts(&held, 0, None, ntsc24));
        assert!(!converts(&held, 90_000, None, r(0, 1)));
    }

    /// Indexes cached before version 9 rated an MP4 with held frames by its
    /// average sample duration (22.746 fps for a 23.976 fps episode), and a
    /// transcode converts at the index's rate: one frame in twenty dropped.
    /// Such an index no longer decodes, so the server builds it again.
    #[test]
    fn indexes_cached_with_the_average_rate_are_rebuilt() {
        use flux_test_media::Clip;
        let Some(path) = flux_test_media::clip(Clip::HeldFramesMp4) else {
            return;
        };
        let asset = Asset::open(&path, 6_000_000_000).unwrap();
        let mut bytes = asset.index.to_bytes().unwrap();
        let back = crate::index::PlaybackIndex::from_bytes(&bytes).unwrap();
        let rate = back.video_track().and_then(|v| v.frame_rate);
        assert_eq!(
            rate,
            Some(Rational {
                num: 24000,
                den: 1001
            })
        );
        // The header's version, as an index cached by version 8 has it.
        bytes[5..7].copy_from_slice(&8u16.to_le_bytes());
        assert!(crate::index::PlaybackIndex::from_bytes(&bytes).is_err());
    }

    #[test]
    fn avcc_to_inband() {
        let rec = [1, 0x64, 0x00, 0x29, 0xFF, 0xE1, 0, 2, 0x67, 0x64, 1, 0, 1, 0x68];
        assert_eq!(
            avcc_parameter_sets(&rec),
            vec![0, 0, 0, 2, 0x67, 0x64, 0, 0, 0, 1, 0x68]
        );
    }

    /// PB-12: the level follows size × frame rate (Table A-1).
    #[test]
    fn level_follows_size_and_rate() {
        assert_eq!(h264_level(1920, 1080, 24.0), 41);
        assert_eq!(h264_level(1920, 1080, 30.0), 41);
        assert_eq!(h264_level(1920, 1080, 50.0), 42);
        assert_eq!(h264_level(1920, 1080, 60.0), 42);
        assert_eq!(h264_level(1280, 720, 60.0), 41);
        assert_eq!(h264_level(3840, 2160, 30.0), 51);
        assert_eq!(h264_level(3840, 2160, 60.0), 52);
        let t = VideoTarget {
            width: 1920,
            height: 1080,
            bitrate: 8_000_000,
            tonemap: false,
            burn: None,
        };
        assert_eq!(t.codec_string(23.976), "avc1.640029");
        assert_eq!(t.codec_string(59.94), "avc1.64002a");
    }

    /// PB-5: a run is cancelled when its last waiter goes away unserved,
    /// not while another request still waits on it, and not by a request
    /// it served.
    #[test]
    fn abandoned_runs_are_cancelled() {
        let run = Arc::new(RunCtl::default());
        let a = Waiter::new(run.clone());
        let b = Waiter::new(run.clone());
        drop(a);
        assert!(!run.cancel.is_cancelled(), "another request still waits");
        drop(b);
        assert!(run.cancel.is_cancelled(), "nobody waits any more");

        let run = Arc::new(RunCtl::default());
        let mut a = Waiter::new(run.clone());
        a.served = true;
        drop(a);
        assert!(!run.cancel.is_cancelled(), "a served request keeps the run going");
    }

    #[test]
    fn frag_reader_waits_for_whole_boxes() {
        let mut r = FragReader::default();
        // A free box split across two reads produces nothing and is consumed.
        let mut b = 12u32.to_be_bytes().to_vec();
        b.extend_from_slice(b"free");
        b.extend_from_slice(&[0, 0, 0, 0]);
        assert!(r.push(&b[..6]).unwrap().is_empty());
        assert!(r.push(&b[6..]).unwrap().is_empty());
        assert!(r.buf.is_empty());
    }
}
