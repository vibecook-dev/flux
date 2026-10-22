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
//!   forces an IDR at every piece boundary. Its fragmented-MP4 output is
//!   parsed as it streams in, and each piece is published as soon as its
//!   frames are complete, so a request waits only for its own piece.
//! - Frame counts are exact per piece (`-frames:v`, one IDR per piece, no
//!   B-frames, CTS = presentation offset D): pieces are independent, any one
//!   can be produced cold, and they line up with every audio rendition
//!   (tfdt = piece start on the asset's timeline).
//! - The init segment needs the encoder's `avcC`: it waits for the run that
//!   covers the first piece requested (or `warm`ed). Each piece also carries
//!   SPS/PPS in-band in its IDR sample, so a run whose encoder session
//!   picks different parameter sets stays decodable.
//! - Results are cached by (asset, recipe, piece) in a byte-budgeted LRU; the
//!   pieces after a request are prefetched by another run (bounded,
//!   cancellable); every ffmpeg child is `kill_on_drop`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use flux_core::{ColorInfo, VideoOp};
use mp4_atom::{Decode, Encode};
use tokio::io::AsyncReadExt;
use tokio::sync::{watch, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::asset::Asset;
use crate::cache::ByteCache;
use crate::encoders::{DetectOptions, FfmpegCaps, PipelineInput, VideoEncoder};
use crate::error::{HlsError, Result};
use crate::fmp4::{self, EntrySpec, Fragment, InitSpec, Sample, SampleData};
use crate::index::{pack_language, Layout};
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
}

impl VideoTarget {
    /// The target of a `VideoOp::Transcode`.
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

/// Clockwise display rotation of the asset's video (0/90/180/270).
fn rotation(asset: &Asset) -> u16 {
    asset.index.video_track().map_or(0, |v| v.extra.rotation)
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
    /// The remux segment (source keyframe span) the piece lies in.
    pub seg: usize,
    /// Presentation start/end on the video timescale (frame grid).
    pub start_ticks: i64,
    pub end_ticks: i64,
    /// Exact frame count; `None` for the last piece of the file (to the end).
    pub frames: Option<u32>,
}

/// Splits every remux segment of `asset` into pieces of about `piece_ns`
/// on the frame grid. Sources without a frame grid keep one piece per
/// segment. `Piece::frames` is a prediction from the frame rate: the
/// encoder run counts and times its frames from the index's real
/// timestamps ([`run_frames`]), so VFR stretches stay exact.
pub fn pieces(asset: &Asset, piece_ns: i64) -> Vec<Piece> {
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

/// The first piece of remux segment `seg`.
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
/// can't list them (containers only ffmpeg reads) or a piece would have
/// none (the predicted counts are used then).
pub fn run_frames(asset: &Asset, run: &[Piece]) -> Result<Option<RunFrames>> {
    let (Some(first), Some(last)) = (run.first(), run.last()) else {
        return Ok(None);
    };
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

/// Where a piece whose frame count is only predicted (`f`, from the frame
/// grid) ends in the encoder's output `pending` (samples, sync flag): at the
/// sync sample forced at its boundary when one lies within [`CUT_SLACK`]
/// frames of `f`, else at `f`. The run's last piece, once the encoder is
/// done, takes what it produced up to `f`: a frame the source lacks made
/// `-frames:v` read one past the run's end, which the next run starts with.
/// `None`: wait for more output.
///
/// An AVI with one frame missing at 0.04 s put every forced IDR of the
/// run's later pieces one sample before their start, so a seek into them
/// began on a P frame.
fn predicted_cut(pending: &[(Bytes, bool)], f: usize, last: bool, eof: bool) -> Option<usize> {
    if last {
        return (eof && !pending.is_empty()).then_some(pending.len().min(f.max(1)));
    }
    if pending.len() < f + CUT_SLACK + 1 && !eof {
        return None;
    }
    let lo = f.saturating_sub(CUT_SLACK).max(1);
    let hi = (f + CUT_SLACK).min(pending.len().saturating_sub(1));
    let cut = (lo..=hi)
        .filter(|&i| pending[i].1)
        .min_by_key(|&i| i.abs_diff(f))
        .unwrap_or(f);
    (cut > 0 && cut <= pending.len()).then_some(cut)
}

/// How far (in frames) a forced IDR may be from a predicted piece boundary
/// and still mark it.
const CUT_SLACK: usize = 3;

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
    /// How the encoder is chosen (forced encoder, VAAPI node, test encodes).
    pub detect: DetectOptions,
    /// What `ffmpeg` can do, detected on first use.
    caps: tokio::sync::OnceCell<Arc<FfmpegCaps>>,
    permits: Arc<Semaphore>,
    prefetch_permits: Arc<Semaphore>,
    cache: Mutex<ByteCache<VKey>>,
    inflight: Mutex<HashMap<VKey, InFlight>>,
    avcc: Mutex<HashMap<(String, VideoTarget), Vec<u8>>>,
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
                    if let Some(mp4_atom::Codec::Avc1(a)) = moov
                        .trak
                        .first()
                        .and_then(|t| t.mdia.minf.stbl.stsd.codecs.first())
                    {
                        let mut v = Vec::new();
                        a.avcc
                            .encode(&mut v)
                            .map_err(|e| HlsError::Ffmpeg(format!("avcC: {e}")))?;
                        if let Some(b) = v.get(8..) {
                            events.push(FragEvent::Avcc(b.to_vec()));
                        }
                    }
                    if let Some(t) = moov.mvex.as_ref().and_then(|x| x.trex.first()) {
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
        for traf in &moof.traf {
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
    /// `max_jobs` bounds concurrent encoder runs for requests; prefetch
    /// uses at most one more.
    pub fn new(ffmpeg: PathBuf, max_jobs: usize, cache_bytes: usize, cancel: CancellationToken) -> Self {
        VideoTranscoder {
            ffmpeg,
            detect: DetectOptions::default(),
            caps: tokio::sync::OnceCell::new(),
            permits: Arc::new(Semaphore::new(max_jobs.max(1))),
            prefetch_permits: Arc::new(Semaphore::new(1)),
            cache: Mutex::new(ByteCache::new(cache_bytes)),
            inflight: Mutex::new(HashMap::new()),
            avcc: Mutex::new(HashMap::new()),
            stats: Mutex::new(VideoTranscodeStats::default()),
            lookahead: 3,
            piece_ns: 2_000_000_000,
            run_pieces: 4,
            timeout: Duration::from_secs(120),
            cancel,
        }
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
                match caps.chosen() {
                    Some(e) => tracing::info!(
                        encoder = %e,
                        hardware = e.is_hardware(),
                        forced = caps.forced.is_some(),
                        tonemap = caps.tonemap_for(e).as_deref().unwrap_or("none"),
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
    pub(crate) fn args(
        asset: &Asset,
        target: &VideoTarget,
        run: &[Piece],
        frames: Option<&RunFrames>,
        caps: &FfmpegCaps,
        enc: VideoEncoder,
        vaapi_device: &std::path::Path,
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
        let pipe = crate::encoders::pipeline(
            caps,
            enc,
            &PipelineInput {
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
            },
            vaapi_device,
        );
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
        // first second). The ffprobe-scan index already counts from the
        // start time.
        if matches!(asset.index.layout, Layout::Matroska(_) | Layout::Mp4(_)) {
            a.extend(["-seek_timestamp".to_string(), "1".into()]);
        }
        if matches!(asset.index.layout, Layout::Ts(_)) {
            // MPEG-TS: ffmpeg's `-ss` lands on any PES by DTS, not on the
            // keyframe (HEVC then misses references and loses frames), so
            // the run reads the source from its keyframe on through stdin
            // ([`Self::ts_input`]); the input's time starts at that keyframe.
            a.extend(["-f", "mpegts", "-i", "pipe:0"].map(String::from));
        } else {
            // Start exactly on the source keyframe of the run's segment.
            a.extend([
                "-ss".to_string(),
                format!("{:.6}", span.start_ns as f64 / 1e9),
                "-noaccurate_seek".into(),
            ]);
            a.extend(["-i".to_string(), asset.index.source.path.display().to_string()]);
        }
        // passthrough: never duplicate/drop frames to fake a constant rate
        // (the default CFR vsync padded the last segment by one frame).
        a.extend(["-map", "0:v:0", "-an", "-sn", "-dn", "-fps_mode", "passthrough"].map(String::from));
        if run.iter().all(|p| p.frames.is_some()) {
            let n: u64 = match frames {
                Some(f) => f.per_piece.iter().map(|x| x.len() as u64).sum(),
                None => run.iter().map(|p| p.frames.unwrap_or(0) as u64).sum(),
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
        let vf = format!(
            "trim=start={:.6},setpts=PTS-STARTPTS,{}",
            trim.max(0.0),
            pipe.filters
        );
        a.extend(["-vf".to_string(), vf]);
        // An IDR at every piece boundary after the first: halfway between
        // the piece's first frame and the frame before it, on the output
        // clock (which `setpts` starts at the run's first kept frame).
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
    fn start_run(
        self: &Arc<Self>,
        key: &str,
        asset: &Arc<Asset>,
        target: VideoTarget,
        all: &Arc<Vec<Piece>>,
        from: usize,
        prefetch: bool,
    ) -> Option<InFlight> {
        let mut senders = Vec::new();
        let first_rx;
        let ctl = Arc::new(RunCtl {
            cancel: self.cancel.child_token(),
            waiting: AtomicUsize::new(0),
        });
        {
            let mut inflight = self.inflight.lock().ok()?;
            let cache = self.cache.lock().ok()?;
            let mut rx0 = None;
            for n in from..(from + self.run_pieces.max(1) as usize).min(all.len()) {
                let k: VKey = (key.to_string(), target, n as u32);
                if cache.contains(&k) || inflight.contains_key(&k) {
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
        prefetch: bool,
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
        let permits = if prefetch {
            self.prefetch_permits.clone()
        } else {
            self.permits.clone()
        };
        let caps = self.caps().await;
        let enc = caps.chosen().ok_or_else(|| {
            HlsError::Unsupported(format!(
                "no usable H.264 encoder in {} ({}); see `flux-server --check`",
                self.ffmpeg.display(),
                caps.error.as_deref().unwrap_or("none passed its test encode")
            ))
        })?;
        let _permit = permits.acquire_owned().await.map_err(|_| HlsError::Cancelled)?;
        let args = Self::args(
            asset,
            &target,
            &run,
            frames.as_ref(),
            &caps,
            enc,
            &self.detect.vaapi_device,
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
                    let want = match (piece.frames, real) {
                        (Some(_), Some(r)) => r.len(),
                        // Counts predicted from the frame grid (sources only
                        // ffprobe indexes, TS): a frame the source lacks
                        // shifts them, so cut at the IDR forced at the
                        // boundary when it is within a few frames.
                        (Some(f), None) => match predicted_cut(&pending, f as usize, last, n == 0) {
                            Some(w) => w,
                            None => break,
                        },
                        (None, _) if n == 0 => pending.len(),
                        (None, _) => break,
                    };
                    if pending.len() < want || want == 0 {
                        break;
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
                        if prefetch {
                            s.prefetched += 1;
                        }
                    }
                    tracing::debug!(piece = *pn, ms, prefetch, "video piece transcoded");
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
                None => self.start_run(key, asset, target, all, n, false),
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
    /// lookahead) that is neither cached nor in flight.
    fn prefetch_after(
        self: &Arc<Self>,
        key: &str,
        asset: &Arc<Asset>,
        target: VideoTarget,
        all: &Arc<Vec<Piece>>,
        n: usize,
    ) {
        if self.prefetch_permits.available_permits() == 0 {
            return;
        }
        for m in n + 1..=(n + self.lookahead as usize).min(all.len().saturating_sub(1)) {
            let k: VKey = (key.to_string(), target, m as u32);
            let known = self.cache.lock().map(|c| c.contains(&k)).unwrap_or(true)
                || self.inflight.lock().map(|i| i.contains_key(&k)).unwrap_or(true);
            if !known {
                let _ = self.start_run(key, asset, target, all, m, true);
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
                let _ = self.start_run(key, &asset, target, &all, n, false);
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
    fn predicted_pieces_end_at_the_forced_idr_nearby() {
        // The source lacks a frame: the IDR forced at the boundary is one
        // sample before the predicted count.
        let p = out(&[0, 51, 103], 160);
        assert_eq!(predicted_cut(&p, 52, false, false), Some(51));
        // On the grid: exactly the count.
        let p = out(&[0, 52], 160);
        assert_eq!(predicted_cut(&p, 52, false, false), Some(52));
        // No IDR near the boundary (an encoder that ignored the request):
        // the count, as before.
        let p = out(&[0, 40], 160);
        assert_eq!(predicted_cut(&p, 52, false, false), Some(52));
        // Not enough output yet to see past the boundary: wait.
        let p = out(&[0], 53);
        assert_eq!(predicted_cut(&p, 52, false, false), None);
        // The run's last piece waits for the encoder to finish, and leaves
        // out a frame read past the run's end.
        let p = out(&[0], 53);
        assert_eq!(predicted_cut(&p, 52, true, false), None);
        assert_eq!(predicted_cut(&p, 52, true, true), Some(52));
        let p = out(&[0], 50);
        assert_eq!(predicted_cut(&p, 52, true, true), Some(50));
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
