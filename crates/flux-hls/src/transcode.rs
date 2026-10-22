//! Stateless per-segment audio transcode (TrueHD/DTS/AC-3/E-AC-3/FLAC/MP3/AAC
//! → AAC or E-AC-3) via a supervised ffmpeg child process.
//!
//! Design (research/03 §5, recommendation 8): each output segment is a pure
//! function of (asset, track, recipe, n), so a seek is one GET with no
//! long-lived process. Seams between independently encoded segments are made
//! inaudible by making every encode sample-aligned to one global frame grid:
//!
//! 1. The output frame grid starts at the source track's first sample (a0)
//!    and steps by the encoder frame size F (AAC 1024, E-AC-3 1536). Segment k
//!    owns grid frames [g_k, g_{k+1}) where g_k is the first frame starting at
//!    or after the video segment boundary T_k.
//! 2. flux demuxes the source frames itself and pipes the *elementary stream*
//!    to ffmpeg (no container timestamps, so ffmpeg decodes sample-exactly from
//!    the first frame fed). The first frame's global sample position is
//!    recovered by averaging `pts × rate − samples_before` over every fed
//!    frame, which cancels Matroska's 1 ms timestamp rounding (critical for
//!    TrueHD, whose 0.83 ms access units are finer than the rounding).
//! 3. `atrim` drops exactly enough samples that the encoder's input starts at
//!    grid position E + d (d = encoder delay, measured: aac 1024, eac3 256),
//!    so output frame j covers [E + jF, E + (j+1)F) — the same grid in every
//!    encode. Three frames of pre-roll warm the encoder and the source
//!    decoder (TrueHD starts at a major sync); two frames of post-roll give
//!    the last kept frame real look-ahead.
//! 4. Frames outside [g_k, g_{k+1}) are discarded; the kept frames are
//!    packaged as an fMP4 fragment with tfdt = a0 + g_k·F + D.
//!
//! Results are cached by (asset, track, recipe, n); the next segments are
//! prefetched in the background (bounded, cancellable). A segment's bytes do
//! not depend on how it was requested: prefetches and on-demand requests
//! encode the same parts with the same deterministic encoder.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use flux_core::Codec;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{OnceCell, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::asset::{Asset, SourceFrame};
use crate::cache::ByteCache;
use crate::codec::{audio, wrap};
use crate::error::{HlsError, Result};
use crate::fmp4::{self, AudioConfig, EntrySpec, Fragment, InitSpec, Sample, SampleData};
use crate::index::{ns_to_ticks, pack_language, ticks_to_ns, Layout};
use crate::SegmentBody;

/// What a transcoded audio rendition is encoded to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AudioTarget {
    /// `Codec::Aac` or `Codec::Eac3`.
    pub codec: Codec,
    pub channels: u8,
    pub bitrate: u32,
}

impl AudioTarget {
    fn frame(&self) -> i64 {
        if self.codec == Codec::Eac3 {
            1536
        } else {
            1024
        }
    }
    /// Encoder delay of ffmpeg's encoders, measured with an impulse:
    /// native `aac` 1024, `eac3` 256 samples.
    fn delay(&self) -> i64 {
        if self.codec == Codec::Eac3 {
            256
        } else {
            1024
        }
    }
}

/// Output sample rate: E-AC-3 allows 32/44.1/48 kHz; AAC renditions are kept
/// at ≤ 48 kHz for compatibility. Only integer down-sampling ratios are used.
fn output_rate(src: u32) -> u32 {
    match src {
        0 => 48_000,
        r if r <= 48_000 => r,
        r if r % 48_000 == 0 => 48_000,
        r if r % 44_100 == 0 => 44_100,
        r => r,
    }
}

/// Samples per source frame at the source rate, from the frame header.
fn frame_samples(codec: Codec, data: &[u8], rate: u32, fallback: Option<u32>) -> Option<u32> {
    match codec {
        Codec::TrueHd => Some(40 * (rate / 44_100).max(1).next_power_of_two()),
        Codec::Ac3 => Some(1536),
        Codec::Eac3 => audio::Ac3Frame::parse(data).map(|f| f.samples),
        Codec::Dts => audio::dts_frame_samples(data),
        Codec::Flac => audio::flac_frame_samples(data),
        Codec::Mp3 | Codec::Mp2 => audio::mpeg_audio_frame_samples(data),
        _ => None,
    }
    .or(fallback)
}

/// True when the transcoder can read this track. Indexed sources are piped
/// to ffmpeg: as elementary streams, raw PCM, or (Opus, Vorbis) a minimal
/// Matroska stream; index-less sources (AVI, TS without H.264/HEVC) are read
/// by ffmpeg directly, so any codec works, and so is TS audio the remux
/// doesn't frame itself (DTS, TrueHD, LPCM, LATM …).
pub fn can_transcode(asset: &Asset, track: u32) -> bool {
    let Some(t) = asset.index.track(track) else {
        return false;
    };
    matches!(asset.index.layout, Layout::External(_) | Layout::Ts(_)) || input_kind(t).is_some()
}

/// ffmpeg reads this track from the file itself (no frames piped).
fn reads_file(asset: &Asset, t: &crate::index::IndexTrack) -> bool {
    match asset.index.layout {
        Layout::External(_) => true,
        Layout::Ts(_) => !crate::ts::frames_in_process(t),
        _ => false,
    }
}

/// How source frames of a track are fed to ffmpeg's stdin.
#[derive(Debug, Clone)]
enum InputKind {
    /// Self-delimiting elementary stream (`-f truehd`, …).
    Elementary(&'static str),
    /// Raw PCM: ffmpeg format name (`s16le`, `f32be`, …) and bytes per
    /// sample frame (all channels).
    Pcm { format: String, frame_bytes: u32 },
    /// Packets in a Matroska stream (`A_OPUS`, `A_VORBIS`).
    Matroska { codec_id: &'static str },
}

/// The elementary-stream input format ffmpeg should read from stdin.
fn input_format(codec: Codec) -> Option<&'static str> {
    Some(match codec {
        Codec::TrueHd => "truehd",
        Codec::Dts => "dts",
        Codec::Ac3 => "ac3",
        Codec::Eac3 => "eac3",
        Codec::Flac => "flac",
        Codec::Mp3 | Codec::Mp2 => "mp3",
        Codec::Aac => "aac",
        _ => return None,
    })
}

/// ffmpeg's raw PCM format for a track: Matroska `A_PCM/INT/LIT|BIG`,
/// `A_PCM/FLOAT/IEEE` + BitDepth, or the MP4 index's `"sowt:s16le"` tag.
fn pcm_format(t: &crate::index::IndexTrack) -> Option<String> {
    let (kind, bits, big) = if let Some((_, f)) = t.codec_tag.split_once(':') {
        // "s16le", "f32be", …
        let kind = f.chars().next()?;
        let big = f.ends_with("be");
        let bits: u8 = f.get(1..f.len() - 2)?.parse().ok()?;
        (kind, bits, big)
    } else {
        let bits = t.bit_depth?;
        let tag = t.codec_tag.to_ascii_uppercase();
        let kind = if tag.contains("FLOAT") { 'f' } else { 's' };
        (kind, bits, tag.ends_with("/BIG"))
    };
    Some(match (kind, bits) {
        // Matroska and QuickTime 8-bit PCM is unsigned.
        ('s', 8) => "u8".into(),
        ('s', 16 | 24 | 32) | ('f', 32 | 64) => {
            format!("{kind}{bits}{}", if big { "be" } else { "le" })
        }
        _ => return None,
    })
}

fn input_kind(t: &crate::index::IndexTrack) -> Option<InputKind> {
    if let Some(f) = input_format(t.codec) {
        return Some(InputKind::Elementary(f));
    }
    match t.codec {
        Codec::Pcm => {
            let format = pcm_format(t)?;
            let bits: u32 = format
                .trim_start_matches(['s', 'u', 'f'])
                .trim_end_matches(['l', 'b', 'e'])
                .parse()
                .ok()?;
            let frame_bytes = bits / 8 * t.channels.unwrap_or(2).max(1) as u32;
            Some(InputKind::Pcm { format, frame_bytes })
        }
        Codec::Opus if audio::OpusHead::parse(&t.codec_private).is_some() => {
            Some(InputKind::Matroska { codec_id: "A_OPUS" })
        }
        Codec::Vorbis if wrap::VorbisBlocks::from_codec_private(&t.codec_private).is_some() => {
            Some(InputKind::Matroska { codec_id: "A_VORBIS" })
        }
        _ => None,
    }
}

/// ADTS header for one raw AAC frame (ISO/IEC 14496-3 §1.A.2.2).
fn adts_header(c: &audio::AacConfig, payload_len: usize) -> [u8; 7] {
    let fi = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ]
    .iter()
    .position(|&r| r == c.sample_rate)
    .unwrap_or(3) as u8;
    let profile = c.object_type.clamp(1, 4) - 1;
    let len = payload_len + 7;
    [
        0xFF,
        0xF1,
        (profile << 6) | (fi << 2) | (c.channel_config >> 2),
        ((c.channel_config & 3) << 6) | ((len >> 11) as u8 & 0x03),
        (len >> 3) as u8,
        (((len & 7) as u8) << 5) | 0x1F,
        0xFC,
    ]
}

/// One ffmpeg encode covering grid frames [keep_from, keep_to).
struct Job {
    stdin: Vec<u8>,
    args: Vec<String>,
    /// Grid index of output frame 0.
    first_index: i64,
    keep_from: i64,
    keep_to: Option<i64>,
}

/// One part of a segment encode: grid frames [from, to) of `track`.
struct PartRequest {
    asset: Arc<Asset>,
    track: u32,
    target: AudioTarget,
    grid: Arc<SegmentGrid>,
    from: i64,
    to: Option<i64>,
    prefetch: bool,
}

/// Grid facts of one output segment.
struct SegmentGrid {
    /// Output sample rate.
    rate: u32,
    /// Grid origin (output ticks) = the source track's first sample.
    a0: i64,
    /// Encoder frame size.
    frame: i64,
    /// Grid frames [g_k, g_end) of the segment (`None` = to the end).
    g_k: i64,
    g_end: Option<i64>,
}

fn segment_grid(asset: &Asset, track: u32, target: AudioTarget, n: usize) -> Result<SegmentGrid> {
    let a = asset
        .index
        .track(track)
        .ok_or_else(|| HlsError::NotFound(format!("track {track}")))?;
    let rate = output_rate(a.sample_rate.unwrap_or(a.timescale));
    let frame = target.frame();
    let span = *asset
        .segments
        .segments
        .get(n)
        .ok_or_else(|| HlsError::NotFound(format!("segment {n}")))?;
    let next = asset.segments.segments.get(n + 1).copied();
    let a0 = ns_to_ticks(a.first_pts_ns.unwrap_or(0), rate);
    let grid = |t_ns: i64| -> i64 {
        let t = ns_to_ticks(t_ns, rate) - a0;
        if t <= 0 {
            0
        } else {
            (t + frame - 1) / frame
        }
    };
    let g_k = if n == 0 { 0 } else { grid(span.start_ns) };
    Ok(SegmentGrid {
        rate,
        a0,
        frame,
        g_k,
        g_end: next.map(|s| grid(s.start_ns)),
    })
}

/// Source pre-roll before the first encoded frame: TrueHD can only start
/// decoding at a major sync (≤ 128 access units ≈ 107 ms apart); other
/// codecs need one frame of MDCT overlap.
fn source_preroll_ns(codec: Codec) -> i64 {
    match codec {
        // Opus needs ~80 ms to converge from a cold decoder; Vorbis's first
        // packet decodes to nothing.
        Codec::TrueHd | Codec::Opus | Codec::Vorbis => 120_000_000,
        _ => 40_000_000,
    }
}

/// Aggregate transcode statistics.
#[derive(Debug, Clone, Copy, Default)]
pub struct TranscodeStats {
    pub segments: u64,
    pub cache_hits: u64,
    pub prefetched: u64,
    /// Wall time per segment (plan + ffmpeg + package), summed.
    pub total_ms: f64,
    pub max_ms: f64,
    /// Reading/demuxing the source window, summed.
    pub plan_ms: f64,
    /// ffmpeg child (spawn → exit), summed.
    pub ffmpeg_ms: f64,
}

type SegKey = (String, u32, AudioTarget, u32);

/// Supervised ffmpeg audio transcoder with a segment cache.
pub struct AudioTranscoder {
    ffmpeg: PathBuf,
    permits: Arc<Semaphore>,
    prefetch_permits: Arc<Semaphore>,
    cache: Mutex<ByteCache<SegKey>>,
    inflight: Mutex<HashMap<SegKey, Arc<OnceCell<Bytes>>>>,
    stats: Mutex<TranscodeStats>,
    /// Segments to prefetch after each request (0 = off).
    pub lookahead: u32,
    /// ffmpeg processes per on-demand segment (parts encoded in parallel).
    pub parallel_parts: u32,
    timeout: Duration,
    cancel: CancellationToken,
}

impl AudioTranscoder {
    /// `max_jobs` bounds concurrent ffmpeg processes for requests; prefetch
    /// uses at most one extra process.
    pub fn new(ffmpeg: PathBuf, max_jobs: usize, cache_bytes: usize, cancel: CancellationToken) -> Self {
        AudioTranscoder {
            ffmpeg,
            permits: Arc::new(Semaphore::new(max_jobs.max(1))),
            prefetch_permits: Arc::new(Semaphore::new(1)),
            cache: Mutex::new(ByteCache::new(cache_bytes)),
            inflight: Mutex::new(HashMap::new()),
            stats: Mutex::new(TranscodeStats::default()),
            lookahead: 2,
            parallel_parts: 2,
            timeout: Duration::from_secs(60),
            cancel,
        }
    }

    /// Statistics so far.
    pub fn stats(&self) -> TranscodeStats {
        self.stats.lock().map(|s| *s).unwrap_or_default()
    }

    /// Segment cache (entries, bytes, evictions).
    pub(crate) fn cache_usage(&self) -> (usize, usize, u64) {
        self.cache.lock().map(|c| c.usage()).unwrap_or_default()
    }

    /// Init segment of a transcoded rendition (no ffmpeg needed: the sample
    /// entry is fully determined by the recipe).
    pub fn init_segment(asset: &Asset, track: u32, target: AudioTarget) -> Result<Vec<u8>> {
        let a = asset
            .index
            .track(track)
            .ok_or_else(|| HlsError::NotFound(format!("track {track}")))?;
        let rate = output_rate(a.sample_rate.unwrap_or(a.timescale));
        let channels = target.channels.max(1);
        let entry = match target.codec {
            Codec::Eac3 => {
                // dec3 for one independent substream; acmod 7 + LFE for 5.1.
                let (acmod, lfe) = match channels {
                    1 => (1, false),
                    2 => (2, false),
                    6 => (7, true),
                    5 => (7, false),
                    _ => (7, true),
                };
                let fscod = match rate {
                    44_100 => 1,
                    32_000 => 2,
                    _ => 0,
                };
                let f = audio::Ac3Frame {
                    eac3: true,
                    fscod,
                    bsid: 16,
                    bsmod: 0,
                    acmod,
                    lfeon: lfe,
                    bit_rate_code: 0,
                    frame_size: (target.bitrate as u64 * 1536 / rate as u64 / 8) as usize,
                    sample_rate: rate,
                    samples: 1536,
                    strmtyp: 0,
                    chanmap: None,
                };
                EntrySpec::Audio {
                    fourcc: *b"ec-3",
                    channels: channels as u16,
                    sample_rate: rate,
                    config: AudioConfig::Dec3(audio::dec3(&[f]).unwrap_or_default()),
                }
            }
            _ => EntrySpec::Audio {
                fourcc: *b"mp4a",
                channels: channels as u16,
                sample_rate: rate,
                config: AudioConfig::Esds {
                    object_type: 0x40,
                    dsi: audio::aac_lc_asc(rate, channels),
                    avg_bitrate: target.bitrate,
                },
            },
        };
        let spec = InitSpec {
            track_id: Asset::track_id(a),
            timescale: rate,
            language: pack_language(a.language.as_deref()),
            entry,
        };
        Ok(fmp4::init_segment(&[spec]))
    }

    /// Index-less containers (TS/AVI), and TS audio the remux doesn't frame:
    /// ffmpeg demuxes the file itself and its accurate `-ss` trims to the
    /// exact sample (TS timestamps are 90 kHz), so the encoder input again
    /// starts at grid position E + d. The TS index's timeline origin is
    /// ffmpeg's `start_time`, which `-ss` is relative to.
    fn plan_direct(
        asset: &Asset,
        track: u32,
        target: AudioTarget,
        g: &SegmentGrid,
        from: i64,
        to: Option<i64>,
    ) -> Job {
        const PREROLL_FRAMES: i64 = 3;
        let fsz = g.frame;
        let enc_start = (from - PREROLL_FRAMES).max(0);
        let start = g.a0 + enc_start * fsz + target.delay();
        let secs = |ticks: i64| format!("{:.6}", ticks.max(0) as f64 / g.rate as f64);
        // Seek the demuxer a few seconds early and cut exactly on the output
        // side (decode and discard, sample-accurate): AVI's index can land
        // *after* an input `-ss` target (measured: 1.5 s of audio missing
        // from a segment of an H.264+AC-3 AVI), which accurate seek can't
        // repair; TS seeks are fine either way.
        const SEEK_MARGIN_S: i64 = 3;
        let pre = (start - SEEK_MARGIN_S * g.rate as i64).max(0);
        let mut args: Vec<String> = [
            "-hide_banner",
            "-nostdin",
            "-nostats",
            "-loglevel",
            "error",
            "-threads",
            "1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if pre > 0 {
            args.extend(["-ss".to_string(), secs(pre)]);
        }
        args.extend(["-i".to_string(), asset.index.source.path.display().to_string()]);
        match (&asset.index.layout, asset.index.track(track)) {
            // By PID: ffmpeg's stream numbers count streams the index skips.
            // A filtergraph input takes the *first* stream of the PID: for
            // Blu-ray TrueHD ffmpeg adds a second one (the AC-3 core) on the
            // same PID, which `-map 0:i:<pid>` would map too.
            (Layout::Ts(_), Some(t)) => args.extend([
                "-filter_complex".to_string(),
                format!("[0:i:{}]anull[a]", t.id),
                "-map".to_string(),
                "[a]".to_string(),
            ]),
            _ => args.extend(["-map".to_string(), format!("0:{track}")]),
        }
        args.extend(["-ss".to_string(), secs(start - pre)]);
        if let Some(e) = to {
            args.extend(["-t".to_string(), secs(g.a0 + (e + 2) * fsz - start)]);
        }
        let (codec_name, muxer) = if target.codec == Codec::Eac3 {
            ("eac3", "eac3")
        } else {
            ("aac", "adts")
        };
        args.extend([
            "-ar".to_string(),
            g.rate.to_string(),
            "-ac".into(),
            target.channels.to_string(),
            "-c:a".into(),
            codec_name.into(),
            "-b:a".into(),
            target.bitrate.to_string(),
            "-f".into(),
            muxer.into(),
            "pipe:1".into(),
        ]);
        Job {
            stdin: Vec::new(),
            args,
            first_index: enc_start,
            keep_from: from,
            keep_to: to,
        }
    }

    /// Plans one encode covering grid frames [from, to) (blocking: reads
    /// the source frames).
    fn plan(
        asset: &Asset,
        track: u32,
        target: AudioTarget,
        g: &SegmentGrid,
        from: i64,
        to: Option<i64>,
    ) -> Result<Job> {
        let a = asset
            .index
            .track(track)
            .ok_or_else(|| HlsError::NotFound(format!("track {track}")))?;
        if reads_file(asset, a) {
            return Ok(Self::plan_direct(asset, track, target, g, from, to));
        }
        let kind = input_kind(a)
            .ok_or_else(|| HlsError::Unsupported(format!("cannot transcode {:?} audio", a.codec)))?;
        let src_rate = if a.codec == Codec::Opus {
            48_000 // Opus always decodes at 48 kHz
        } else {
            a.sample_rate.unwrap_or(a.timescale).max(1)
        };
        let rate = g.rate;
        if src_rate % rate != 0 {
            return Err(HlsError::Unsupported(format!(
                "resampling {src_rate} → {rate} Hz is not sample-exact"
            )));
        }
        let ratio = (src_rate / rate) as i64;
        let fsz = g.frame;
        // Three frames of encoder pre-roll; two of post-roll for look-ahead.
        const PREROLL_FRAMES: i64 = 3;
        let mut enc_start = (from - PREROLL_FRAMES).max(0);
        let t0 = ticks_to_ns(g.a0 + enc_start * fsz, rate) - source_preroll_ns(a.codec);
        let t1 = to
            .map(|e| ticks_to_ns(g.a0 + (e + 2) * fsz, rate) + 20_000_000)
            .unwrap_or(i64::MAX);
        let mut frames: Vec<SourceFrame> = asset.audio_frames(track, t0, t1)?;
        if a.codec == Codec::TrueHd {
            // A TrueHD decoder can only start at a major sync.
            let first = frames
                .iter()
                .position(|f| audio::truehd_is_major_sync(&f.data))
                .unwrap_or(0);
            frames.drain(..first);
        }
        // Decoded samples per fed frame. A Vorbis decoder outputs nothing for
        // the first packet it sees, so its output starts at the second.
        let mut vorbis = (a.codec == Codec::Vorbis)
            .then(|| wrap::VorbisBlocks::from_codec_private(&a.codec_private))
            .flatten();
        let first_silent = vorbis.is_some();
        if frames.len() < 1 + first_silent as usize {
            return Err(HlsError::NotFound(format!(
                "no source audio for grid frames {from}.."
            )));
        }
        let durs: Vec<i64> = frames
            .iter()
            .map(|f| match (&kind, &mut vorbis) {
                (_, Some(v)) => v.packet_samples(&f.data) as i64,
                (InputKind::Pcm { frame_bytes, .. }, _) => {
                    (f.data.len() as u32 / (*frame_bytes).max(1)) as i64
                }
                (InputKind::Matroska { .. }, _) => audio::opus_packet_samples(&f.data).unwrap_or(960) as i64,
                (InputKind::Elementary(_), _) => {
                    frame_samples(a.codec, &f.data, src_rate, a.frame_duration).unwrap_or(1024) as i64
                }
            })
            .collect();
        // Global sample position (source rate) of the first decoded sample,
        // averaged over every fed frame to cancel timestamp rounding.
        let mut cum = 0i64;
        let mut acc = 0f64;
        let mut n = 0usize;
        for (i, f) in frames.iter().enumerate() {
            if !(first_silent && i == 0) {
                acc += f.pts_ns as f64 * src_rate as f64 / 1e9 - cum as f64;
                n += 1;
                cum += durs[i];
            }
        }
        let s_in = ((acc / n.max(1) as f64).round() as i64).div_euclid(ratio);
        // Encoder input must start at E + d with E on the grid and E + d ≥ s_in.
        let d = target.delay();
        let mut e = g.a0 + enc_start * fsz;
        if e + d < s_in {
            enc_start = (s_in - d - g.a0 + fsz - 1).div_euclid(fsz);
            e = g.a0 + enc_start * fsz;
        }
        let trim_src = (e + d - s_in) * ratio;

        let mut stdin = Vec::with_capacity(frames.iter().map(|f| f.data.len() + 7).sum::<usize>() + 64);
        let input: Vec<String> = match &kind {
            InputKind::Elementary(fmt) => {
                if a.codec == Codec::Flac {
                    // The flac demuxer needs the `fLaC` marker + STREAMINFO first.
                    stdin.extend_from_slice(&a.codec_private);
                }
                let aac_cfg = (a.codec == Codec::Aac)
                    .then(|| audio::AacConfig::parse(&a.codec_private))
                    .flatten();
                for f in &frames {
                    if let Some(c) = &aac_cfg {
                        stdin.extend_from_slice(&adts_header(c, f.data.len()));
                    }
                    stdin.extend_from_slice(&f.data);
                }
                vec!["-f".into(), fmt.to_string()]
            }
            InputKind::Pcm { format, .. } => {
                for f in &frames {
                    stdin.extend_from_slice(&f.data);
                }
                vec![
                    "-f".into(),
                    format.clone(),
                    "-ar".into(),
                    src_rate.to_string(),
                    "-ac".into(),
                    a.channels.unwrap_or(2).max(1).to_string(),
                ]
            }
            InputKind::Matroska { codec_id } => {
                let private = if a.codec == Codec::Opus {
                    wrap::opus_head_without_preskip(&a.codec_private)
                } else {
                    a.codec_private.clone()
                };
                let t0 = frames[0].pts_ns;
                let packets: Vec<(i64, &[u8])> = frames
                    .iter()
                    .map(|f| ((f.pts_ns - t0).div_euclid(1_000_000), f.data.as_ref()))
                    .collect();
                stdin = wrap::matroska_stream(
                    &wrap::MkvAudioTrack {
                        codec_id,
                        codec_private: &private,
                        sample_rate: src_rate,
                        channels: a.channels.unwrap_or(2),
                        bit_depth: None,
                    },
                    &packets,
                );
                vec!["-f".into(), "matroska".into()]
            }
        };
        // Samples are renumbered from the first decoded one before cutting,
        // so input timestamps (container or synthesized) never matter.
        let mut filters = vec![
            "asetpts=N/SR/TB".to_string(),
            format!("atrim=start_sample={trim_src}"),
            "asetpts=N/SR/TB".to_string(),
        ];
        if ratio > 1 {
            filters.push(format!("aresample={rate}"));
        }
        let (codec_name, muxer) = if target.codec == Codec::Eac3 {
            ("eac3", "eac3")
        } else {
            ("aac", "adts")
        };
        let mut args: Vec<String> = ["-hide_banner", "-nostats", "-loglevel", "error", "-threads", "1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        args.extend(input);
        let args: Vec<String> = args
            .into_iter()
            .chain(
                [
                    "-i",
                    "pipe:0",
                    "-af",
                    &filters.join(","),
                    "-ac",
                    &target.channels.to_string(),
                    "-c:a",
                    codec_name,
                    "-b:a",
                    &target.bitrate.to_string(),
                    "-f",
                    muxer,
                    "pipe:1",
                ]
                .iter()
                .map(|s| s.to_string()),
            )
            .collect();
        Ok(Job {
            stdin,
            args,
            first_index: enc_start,
            keep_from: from,
            keep_to: to,
        })
    }

    async fn run(&self, job: &Job) -> Result<Vec<u8>> {
        let mut child = tokio::process::Command::from(flux_core::tool::command(&self.ffmpeg))
            .args(&job.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| HlsError::Ffmpeg(format!("spawn {}: {e}", self.ffmpeg.display())))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| HlsError::Ffmpeg("no stdin".into()))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| HlsError::Ffmpeg("no stdout".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| HlsError::Ffmpeg("no stderr".into()))?;
        let input = job.stdin.clone();
        // Feed stdin concurrently with draining stdout (a full pipe would deadlock).
        let writer = async move {
            let r = stdin.write_all(&input).await;
            drop(stdin);
            r
        };
        let reader = async {
            let mut out = Vec::with_capacity(256 << 10);
            stdout.read_to_end(&mut out).await.map(|_| out)
        };
        let err_reader = async {
            let mut e = Vec::new();
            let _ = stderr.read_to_end(&mut e).await;
            e
        };
        let work = async {
            let (w, out, err) = tokio::join!(writer, reader, err_reader);
            let status = child.wait().await;
            (w, out, err, status)
        };
        let (w, out, err, status) = tokio::select! {
            r = tokio::time::timeout(self.timeout, work) => r.map_err(|_| HlsError::Ffmpeg("timed out".into()))?,
            _ = self.cancel.cancelled() => return Err(HlsError::Cancelled),
        };
        let status = status?;
        if !status.success() {
            return Err(HlsError::Ffmpeg(format!(
                "exit {status}: {}",
                String::from_utf8_lossy(&err).trim()
            )));
        }
        // A broken pipe is fine if ffmpeg exited cleanly (it may stop reading early).
        if let Err(e) = w {
            if e.kind() != std::io::ErrorKind::BrokenPipe {
                return Err(e.into());
            }
        }
        Ok(out?)
    }

    /// Splits encoder output into frames and keeps those in the job's range.
    fn keep_frames(job: &Job, out: Vec<u8>, eac3: bool) -> Vec<Bytes> {
        let out = Bytes::from(out);
        let frames: Vec<Bytes> = if eac3 {
            let mut p = 0usize;
            audio::split_ac3(&out)
                .into_iter()
                .map(|(_, raw)| {
                    p += raw.len();
                    out.slice(p - raw.len()..p)
                })
                .collect()
        } else {
            let (raw, _asc) = audio::split_adts(&out);
            raw.iter()
                .map(|r| {
                    let start = r.as_ptr() as usize - out.as_ptr() as usize;
                    out.slice(start..start + r.len())
                })
                .collect()
        };
        // Frame 0 starts with the encoder's d samples of padding; it falls in
        // the pre-roll except at the very start of the stream.
        frames
            .into_iter()
            .enumerate()
            .filter(|(j, _)| {
                let idx = job.first_index + *j as i64;
                idx >= job.keep_from && !job.keep_to.is_some_and(|e| idx >= e)
            })
            .map(|(_, f)| f)
            .collect()
    }

    /// Plans and runs one part (one ffmpeg child).
    async fn part(&self, req: PartRequest) -> Result<(Vec<Bytes>, f64, f64)> {
        let PartRequest {
            asset,
            track,
            target,
            grid,
            from,
            to,
            prefetch,
        } = req;
        let permits = if prefetch {
            self.prefetch_permits.clone()
        } else {
            self.permits.clone()
        };
        let _permit = permits.acquire_owned().await.map_err(|_| HlsError::Cancelled)?;
        let t0 = Instant::now();
        let job = tokio::task::spawn_blocking(move || Self::plan(&asset, track, target, &grid, from, to))
            .await
            .map_err(|e| HlsError::Ffmpeg(format!("plan task: {e}")))??;
        let plan_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let t1 = Instant::now();
        let out = self.run(&job).await?;
        let ffmpeg_ms = t1.elapsed().as_secs_f64() * 1000.0;
        Ok((
            Self::keep_frames(&job, out, target.codec == Codec::Eac3),
            plan_ms,
            ffmpeg_ms,
        ))
    }

    async fn produce(
        &self,
        asset: Arc<Asset>,
        track: u32,
        target: AudioTarget,
        n: usize,
        prefetch: bool,
    ) -> Result<Bytes> {
        let t0 = Instant::now();
        let g = Arc::new(segment_grid(&asset, track, target, n)?);
        // Split long segments at a grid frame and encode the parts in
        // parallel: every encode is aligned to the same global grid, so the
        // join is as seamless as a segment boundary (decode dominates the
        // cost, e.g. TrueHD ≈ 20× real time per core under load).
        //
        // Prefetches use the same partition (their parts run one after the
        // other on the single prefetch permit): the encoder is deterministic
        // for a given input, so segment n has the same bytes whether it was
        // prefetched or requested on a seek, as a strong ETag promises
        // (PB-11). A 1-part prefetch encoded different AAC frames.
        let parts = self.parallel_parts.max(1) as i64;
        let mut bounds = vec![(g.g_k, g.g_end)];
        if let (Some(end), true) = (g.g_end, parts > 1) {
            let len = end - g.g_k;
            if len >= 4 * parts {
                bounds = (0..parts)
                    .map(|i| {
                        let a = g.g_k + len * i / parts;
                        let b = g.g_k + len * (i + 1) / parts;
                        (a, Some(b))
                    })
                    .collect();
            }
        }
        let results = futures::future::join_all(bounds.iter().map(|&(from, to)| {
            self.part(PartRequest {
                asset: asset.clone(),
                track,
                target,
                grid: g.clone(),
                from,
                to,
                prefetch,
            })
        }))
        .await;
        let a = asset
            .index
            .track(track)
            .ok_or_else(|| HlsError::NotFound(format!("track {track}")))?;
        let mut samples = Vec::new();
        let (mut plan_ms, mut ffmpeg_ms) = (0f64, 0f64);
        for r in results {
            let (frames, p, f) = r?;
            plan_ms = plan_ms.max(p);
            ffmpeg_ms = ffmpeg_ms.max(f);
            samples.extend(frames.into_iter().map(|f| Sample {
                duration: g.frame as u32,
                cts: 0,
                sync: true,
                data: SampleData::Bytes(f),
            }));
        }
        if samples.is_empty() {
            return Err(HlsError::Ffmpeg(
                "encoder produced no frames for this segment".into(),
            ));
        }
        let dd = ns_to_ticks(asset.index.presentation_offset_ns, g.rate);
        let frag = Fragment {
            track_id: Asset::track_id(a),
            base_dts: (g.a0 + g.g_k * g.frame + dd).max(0) as u64,
            samples,
            video: false,
        };
        let bytes = Bytes::from(fmp4::media_segment(n as u32 + 1, &[frag]).read_all()?);
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        if let Ok(mut s) = self.stats.lock() {
            s.segments += 1;
            s.total_ms += ms;
            s.max_ms = s.max_ms.max(ms);
            s.plan_ms += plan_ms;
            s.ffmpeg_ms += ffmpeg_ms;
            if prefetch {
                s.prefetched += 1;
            }
        }
        tracing::debug!(
            track,
            n,
            ms,
            plan_ms,
            ffmpeg_ms,
            prefetch,
            "audio segment transcoded"
        );
        Ok(bytes)
    }

    async fn get_or_produce(
        self: &Arc<Self>,
        asset_key: &str,
        asset: Arc<Asset>,
        track: u32,
        target: AudioTarget,
        n: usize,
        prefetch: bool,
    ) -> Result<Bytes> {
        let key: SegKey = (asset_key.to_string(), track, target, n as u32);
        if let Some(b) = self.cache.lock().ok().and_then(|mut c| c.get(&key)) {
            if let Ok(mut s) = self.stats.lock() {
                s.cache_hits += 1;
            }
            return Ok(b);
        }
        let cell = {
            let mut m = self.inflight.lock().map_err(|_| HlsError::Cancelled)?;
            m.entry(key.clone())
                .or_insert_with(|| Arc::new(OnceCell::new()))
                .clone()
        };
        let res = cell
            .get_or_try_init(|| self.produce(asset, track, target, n, prefetch))
            .await
            .cloned();
        if let Ok(b) = &res {
            if let Ok(mut c) = self.cache.lock() {
                c.put(key.clone(), b.clone());
            }
        }
        if let Ok(mut m) = self.inflight.lock() {
            m.remove(&key);
        }
        res
    }

    /// Audio segment `n` of the transcoded rendition, from cache or a fresh
    /// encode; then prefetches the following segments in the background.
    pub async fn segment(
        self: &Arc<Self>,
        asset_key: &str,
        asset: Arc<Asset>,
        track: u32,
        target: AudioTarget,
        n: usize,
    ) -> Result<SegmentBody> {
        let bytes = self
            .get_or_produce(asset_key, asset.clone(), track, target, n, false)
            .await?;
        for k in 1..=self.lookahead as usize {
            let m = n + k;
            if m >= asset.segments.len() {
                break;
            }
            let key: SegKey = (asset_key.to_string(), track, target, m as u32);
            let known = self.cache.lock().map(|c| c.contains(&key)).unwrap_or(true)
                || self.inflight.lock().map(|i| i.contains_key(&key)).unwrap_or(true);
            if known || self.prefetch_permits.available_permits() == 0 {
                continue;
            }
            let this = self.clone();
            let asset = asset.clone();
            let ak = asset_key.to_string();
            let cancel = self.cancel.clone();
            tokio::spawn(async move {
                tokio::select! {
                    r = this.get_or_produce(&ak, asset, track, target, m, true) => {
                        if let Err(e) = r { tracing::debug!(%e, m, "audio prefetch failed"); }
                    }
                    _ = cancel.cancelled() => {}
                }
            });
        }
        Ok(SegmentBody::from_bytes(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adts_roundtrip() {
        let c = audio::AacConfig {
            object_type: 2,
            sample_rate: 48000,
            channel_config: 2,
            frame_samples: 1024,
        };
        let mut s = adts_header(&c, 3).to_vec();
        s.extend_from_slice(&[1, 2, 3]);
        let (frames, asc) = audio::split_adts(&s);
        assert_eq!(frames, vec![&[1u8, 2, 3][..]]);
        assert_eq!(asc, Some(vec![0x11, 0x90]));
    }

    #[test]
    fn rates() {
        assert_eq!(output_rate(48_000), 48_000);
        assert_eq!(output_rate(96_000), 48_000);
        assert_eq!(output_rate(88_200), 44_100);
        assert_eq!(frame_samples(Codec::TrueHd, &[], 48_000, None), Some(40));
        assert_eq!(frame_samples(Codec::TrueHd, &[], 96_000, None), Some(80));
    }
}
