//! Index for containers flux-hls can't cut itself (ASF/WMV, FLV, RealMedia,
//! MPEG-PS, AVI, MPEG-TS without H.264/HEVC video), from one short
//! `ffprobe` run: the streams, the first few hundred packets (for the real
//! frame rate and GOP length), and, for FLV and ASF, where the packets end
//! (`flux_probe::tail`). No packet scan of the whole file: playback starts
//! at once (a full scan took ~6 s/GB from a USB disk, 21 s for 3.75 GB).
//!
//! Only the ffmpeg-backed renditions (video and audio transcode) are made
//! from such an index; the planner never copies these containers because
//! `to_probe_result()` reports `has_index = false`. Segments lie on a fixed
//! time grid ([`crate::segment::SegmentPlan`]), and each video run seeks
//! ffmpeg by time and converts to a constant frame rate, so every piece has
//! a known frame count whatever the source's timestamps
//! ([`crate::vtranscode`]).

use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flux_core::{Rational, TrackKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{standard_rate, IndexMethod, IndexStats, IndexTrack, Layout, PlaybackIndex, INDEX_VERSION};
use crate::error::{HlsError, Result};
use crate::source::SourceId;

/// Where the stream timeline starts (ffmpeg's `-ss` is relative to it) and
/// how far before a piece a run starts decoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalLayout {
    pub start_time_ns: i64,
    /// A video run seeks ffmpeg this far before its first piece and drops
    /// what comes before the piece: half a second, or 1.5 GOPs for MPEG
    /// program and transport streams, whose seeks land on any packet.
    #[serde(default)]
    pub seek_margin_ns: i64,
    /// An MPEG-PS file of joined recordings: its parts, each with a clock
    /// of its own (`flux_probe::tail::ps_parts`), one after the other on
    /// the asset's timeline. Empty for any other file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<ExternalPart>,
}

/// A part of an MPEG-PS file whose clock restarts ([`ExternalLayout::parts`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalPart {
    /// Where the part starts in the file.
    pub offset: u64,
    /// Where it starts on the asset's timeline, ns.
    pub start_ns: i64,
}

impl ExternalLayout {
    /// Which part `t_ns` (asset timeline) lies in (0 without parts).
    pub fn part_of(&self, t_ns: i64) -> usize {
        self.parts.iter().rposition(|p| p.start_ns <= t_ns).unwrap_or(0)
    }

    /// What an ffmpeg run that reads from `t_ns` (asset timeline) on opens,
    /// and where that input's clock starts on the timeline: the file, or
    /// for joined parts only the part `t_ns` lies in (ffmpeg's `subfile`
    /// protocol), since a seek by timestamp can't tell the parts apart.
    /// Segments start anew at each part (`crate::segment`), and a video
    /// run stays in one part, so no run reads past its part's end.
    pub fn input_at(&self, path: &Path, t_ns: i64) -> (String, i64) {
        match self.parts.iter().rposition(|p| p.start_ns <= t_ns) {
            Some(i) if self.parts.len() > 1 => {
                let p = self.parts[i];
                let end = self.parts.get(i + 1).map_or(0, |n| n.offset);
                (
                    format!("subfile,,start,{},end,{end},,:{}", p.offset, path.display()),
                    p.start_ns,
                )
            }
            _ => (path.display().to_string(), 0),
        }
    }
}

/// Packets read at the start of the file: enough for the frame rate and a
/// few GOPs (12 s of 25 fps video with its audio), and ~0.1–0.4 s of
/// ffprobe.
const HEAD_PACKETS: &str = "%+#600";

/// The frame rate transcodes are converted to: the stream's own rate when
/// its first packets agree with it, else what they measure (ASF's
/// `r_frame_rate` 1000/1 is its millisecond clock; three WMV 7 files of 720p
/// with it measured 29.97 fps), else ffprobe's average, else 25. At most 60:
/// the H.264 level and every encoder are made for that.
fn output_rate(r_frame_rate: Option<Rational>, avg: Option<Rational>, measured_ns: Option<f64>) -> Rational {
    let sane = |r: &Rational| (1.0..=flux_probe::ffprobe::MAX_PLAUSIBLE_FPS).contains(&r.as_f64());
    let (r, avg) = (r_frame_rate.filter(sane), avg.filter(sane));
    let measured = measured_ns.and_then(|ns| cfr(1e9 / ns));
    let rate = match (r, measured) {
        // The stream's exact rational, when the packets agree (5 %).
        (Some(r), Some(m)) if (r.as_f64() / m.as_f64() - 1.0).abs() <= 0.05 => cfr(r.as_f64()).unwrap_or(r),
        (_, Some(m)) => m,
        (Some(r), None) => cfr(r.as_f64()).unwrap_or(r),
        (None, None) => avg
            .and_then(|a| cfr(a.as_f64()))
            .unwrap_or(Rational { num: 25, den: 1 }),
    };
    if rate.as_f64() > 60.5 {
        Rational { num: 60, den: 1 }
    } else {
        rate
    }
}

/// A frame rate a transcode can keep on a frame grid: a standard rate
/// within 1.5 % (23.976, 25, 29.97 …), else the closest whole, NTSC
/// (×1000/1001) or half rate within 0.5 % (15, 14.985, 12.5), else the
/// nearest whole rate.
fn cfr(fps: f64) -> Option<Rational> {
    if !(fps.is_finite() && fps >= 0.5) {
        return None;
    }
    if let Some(r) = standard_rate(1e9 / fps, 0.015).filter(|r| r.den != 1000) {
        return Some(r);
    }
    let n = fps.round().max(1.0) as u32;
    let half = (fps * 2.0).round().max(1.0) as u32;
    let ntsc = (fps * 1.001).round().max(1.0) as u32 * 1000;
    let err = |r: &Rational| (r.as_f64() / fps - 1.0).abs();
    [
        Rational { num: n, den: 1 },
        Rational { num: ntsc, den: 1001 },
        reduce(Rational { num: half, den: 2 }),
    ]
    .into_iter()
    .filter(|r| err(r) <= 0.005)
    .min_by(|a, b| err(a).total_cmp(&err(b)))
    .or(Some(Rational { num: n, den: 1 }))
}

fn reduce(r: Rational) -> Rational {
    fn gcd(a: u32, b: u32) -> u32 {
        if b == 0 {
            a
        } else {
            gcd(b, a % b)
        }
    }
    let g = gcd(r.num, r.den).max(1);
    Rational {
        num: r.num / g,
        den: r.den / g,
    }
}

/// (timescale, ticks per frame) for a constant frame rate: 30000/1001 →
/// (30000, 1001); whole and half rates get a timescale ≥ 10 000 for
/// sub-millisecond audio alignment (25/1 → (10000, 400)).
pub(crate) fn frame_timebase(r: Rational) -> (u32, u32) {
    let (num, den) = (r.num.max(1), r.den.max(1));
    if den >= 1000 {
        return (num, den);
    }
    let mult = 10_000u32.div_ceil(num).max(1);
    (num * mult, den * mult)
}

/// The nominal frame interval of the first video packets, ns: the mean of
/// the intervals near their median, so millisecond timestamps (33/34 ms at
/// 29.97 fps) average out and dropped frames don't count. Also the longest
/// distance between keyframes (the GOP), when two were seen.
fn measure(times: &mut [f64], keys: &[f64]) -> (Option<f64>, Option<f64>) {
    times.sort_by(f64::total_cmp);
    let mut d: Vec<f64> = times
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|&x| x > 1e-4)
        .collect();
    let gop = keys
        .windows(2)
        .map(|w| w[1] - w[0])
        .fold(None, |m: Option<f64>, x| Some(m.map_or(x, |m| m.max(x))));
    if d.len() < 8 {
        return (None, gop);
    }
    d.sort_by(f64::total_cmp);
    let med = d[d.len() / 2];
    let near: Vec<f64> = d
        .iter()
        .copied()
        .filter(|&x| x >= med * 0.6 && x <= med * 1.4)
        .collect();
    let mean = near.iter().sum::<f64>() / near.len().max(1) as f64;
    (Some(mean * 1e9), gop)
}

/// How far before a piece a run starts ffmpeg (its `-ss`), so the frame on
/// screen at the piece's start is decoded (the frame grid then picks it):
/// half a second, or two frames. MPEG program and transport streams seek
/// by timestamp to any packet, not to a keyframe, so there the run starts
/// one and a half GOPs early (1–10 s): from a picture that is not a
/// keyframe the decoder shows grey until the next I-frame.
fn seek_margin_ns(format_name: &str, frame_ns: f64, gop_s: Option<f64>) -> i64 {
    let base = (500_000_000f64).max(2.0 * frame_ns);
    let mpeg = matches!(format_name, "mpeg" | "mpegts" | "mpegvideo" | "vob");
    let ns = if mpeg {
        gop_s.map_or(3e9, |g| (g * 1.5e9 + 2.0 * frame_ns).clamp(1e9, 10e9))
    } else {
        base
    };
    ns.round() as i64
}

fn secs(v: &Value) -> Option<f64> {
    v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_f64())
}

/// Bounds on the ffprobe runs that build an index (ROB-1). A play request
/// waits for them, so a hung ffprobe (a stalled NAS read, a pathological
/// stream) must not hold it, or the server's shutdown, forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanLimits {
    /// Kill a run that has written nothing for this long. The packet scan
    /// streams a line per packet, so only a stall trips this.
    pub stall: Duration,
    /// Kill a run older than `base + per_gb × file size in GB`, whatever
    /// its progress (the packet scan reads the whole file: ~1 s/GB from an
    /// SSD, slower from a NAS).
    pub base: Duration,
    pub per_gb: Duration,
}

impl Default for ScanLimits {
    fn default() -> Self {
        ScanLimits {
            stall: Duration::from_secs(20),
            base: Duration::from_secs(30),
            per_gb: Duration::from_secs(30),
        }
    }
}

impl ScanLimits {
    fn deadline(&self, file_len: u64) -> Duration {
        self.base + self.per_gb.mul_f64(file_len as f64 / 1e9)
    }
}

/// Process groups of the index scans running now, so a shutdown can kill
/// them ([`kill_running`]).
static RUNNING: Mutex<Vec<i32>> = Mutex::new(Vec::new());

/// Kills every ffprobe index scan running in this process (and anything it
/// spawned): call on shutdown, so no blocking thread waits on one and no
/// child outlives the server. The scans fail with [`HlsError::Cancelled`].
pub fn kill_running() {
    let groups: Vec<i32> = RUNNING.lock().map(|g| g.clone()).unwrap_or_default();
    for g in groups {
        kill_group(g);
    }
}

/// SIGKILL to a process group (each run is its own group leader).
#[cfg(unix)]
#[allow(unsafe_code)]
fn kill_group(pgid: i32) {
    if pgid > 0 {
        // SAFETY: killpg takes plain integers; a stale group id only makes
        // it fail with ESRCH.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
}

/// Not Unix (Windows): the process and its children, by id (`taskkill /T`).
#[cfg(not(unix))]
fn kill_group(pid: i32) {
    if pid > 0 {
        let _ = flux_core::tool::command("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Runs ffprobe with `args` on `path` and returns its stdout. The child is
/// its own process group; stdout and stderr are drained on helper threads
/// (the packet scan of a film is MBs, more than a pipe holds); the group is
/// killed when it stalls, overruns its deadline or is cancelled.
pub(crate) fn run(
    ffprobe: &Path,
    args: &[&str],
    path: &Path,
    limits: &ScanLimits,
    file_len: u64,
) -> Result<String> {
    let mut cmd = flux_core::tool::command(ffprobe);
    cmd.args(args)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| HlsError::Ffmpeg(format!("spawn {}: {e}", ffprobe.display())))?;
    let pgid = child.id() as i32;
    if let Ok(mut g) = RUNNING.lock() {
        g.push(pgid);
    }
    let unregister = || {
        if let Ok(mut g) = RUNNING.lock() {
            g.retain(|&p| p != pgid);
        }
    };
    let progress = Arc::new(AtomicU64::new(0));
    let out_thread = {
        let progress = progress.clone();
        let pipe = child.stdout.take();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let mut chunk = [0u8; 64 << 10];
                while let Ok(n) = p.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    progress.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
            buf
        })
    };
    let err_thread = {
        let pipe = child.stderr.take();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let start = Instant::now();
    let deadline = limits.deadline(file_len);
    let (mut seen, mut last_change) = (0u64, start);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                kill_group(pgid);
                let _ = child.wait();
                unregister();
                return Err(HlsError::Ffmpeg(format!("ffprobe: wait: {e}")));
            }
        }
        let now = Instant::now();
        let p = progress.load(Ordering::Relaxed);
        if p != seen {
            (seen, last_change) = (p, now);
        }
        let why = if now - last_change >= limits.stall {
            Some(format!("no output for {:.1} s", limits.stall.as_secs_f64()))
        } else if now - start >= deadline {
            Some(format!("still running after {:.1} s", deadline.as_secs_f64()))
        } else {
            None
        };
        if let Some(why) = why {
            // The whole group: a wrapper's children hold the pipes too.
            kill_group(pgid);
            let _ = child.wait();
            unregister();
            // The reader threads end when the pipes close; not joined, in
            // case something outside the group still holds one open.
            return Err(HlsError::Ffmpeg(format!(
                "ffprobe index scan of {} killed: {why}",
                path.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    unregister();
    let out = out_thread.join().unwrap_or_default();
    let err = err_thread.join().unwrap_or_default();
    if !status.success() {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if status.signal() == Some(libc::SIGKILL) {
                return Err(HlsError::Cancelled);
            }
        }
        return Err(HlsError::Ffmpeg(format!(
            "ffprobe: {}",
            String::from_utf8_lossy(&err).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// The streams, the format and the first packets, in one ffprobe run.
const ENTRIES: &str = "format=format_name,start_time,duration\
    :stream=index,codec_type,codec_name,profile,width,height,has_b_frames,r_frame_rate,avg_frame_rate,\
    sample_rate,channels,start_time,duration,bit_rate,sample_aspect_ratio\
    :stream_tags=language,title:stream_disposition=default,forced\
    :packet=stream_index,pts_time,dts_time,flags";

fn rational_of(v: &Value) -> Option<Rational> {
    let (n, d) = v.as_str()?.split_once('/')?;
    let (num, den): (u32, u32) = (n.parse().ok()?, d.parse().ok()?);
    (num > 0 && den > 0).then_some(Rational { num, den })
}

/// The size of the first video frame ffmpeg decodes, for a stream whose
/// header gives none (VP6 in FLV with a 0×0 header). `Err` with ffmpeg's
/// reason when no frame decodes: such a file can't be played at all, and
/// is refused at once instead of failing every segment.
fn decoded_size(
    ffprobe: &Path,
    path: &Path,
    index: u32,
    limits: &ScanLimits,
    file_len: u64,
) -> Result<(u32, u32)> {
    let sel = format!("{index}");
    let out = run(
        ffprobe,
        &[
            "-v",
            "error",
            "-select_streams",
            &sel,
            "-read_intervals",
            "%+#30",
            "-show_entries",
            "frame=width,height",
            "-of",
            "csv=p=0",
        ],
        path,
        limits,
        file_len,
    );
    let size = out.as_ref().ok().and_then(|o| {
        o.lines().find_map(|l| {
            let (w, h) = l.trim().split_once(',')?;
            let (w, h): (u32, u32) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
            (w > 0 && h > 0).then_some((w, h))
        })
    });
    size.ok_or_else(|| {
        let why = match out {
            Err(e) => e.to_string(),
            Ok(_) => "no frame decodes".into(),
        };
        HlsError::Unsupported(format!("its video can't be decoded ({why})"))
    })
}

/// Builds an index by running ffprobe (stream facts and the first
/// packets) and reading where FLV/ASF packets end, each run bounded by
/// `limits` and killable with [`kill_running`].
pub fn build_with_limits(
    path: &Path,
    ffprobe: &Path,
    source: SourceId,
    limits: &ScanLimits,
) -> Result<PlaybackIndex> {
    let t0 = std::time::Instant::now();
    let file_len = source.size;
    let info: Value = serde_json::from_str(&run(
        ffprobe,
        &[
            "-v",
            "error",
            "-show_entries",
            ENTRIES,
            "-read_intervals",
            HEAD_PACKETS,
            "-of",
            "json",
        ],
        path,
        limits,
        file_len,
    )?)?;
    let format = &info["format"];
    let start = secs(&format["start_time"]).unwrap_or(0.0);
    let start_ns = (start * 1e9).round() as i64;
    let mut duration_ns = (secs(&format["duration"]).unwrap_or(0.0) * 1e9).round() as i64;
    let name = format["format_name"].as_str().unwrap_or("");
    let ext = source.path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let container = flux_probe::ffprobe::container_of(name, ext);
    let mut warnings =
        vec!["no container index flux reads: ffmpeg seeks by time; transcode paths only".to_string()];
    let mut ps_parts = Vec::new();
    // FLV/ASF: the header's duration may outlast the packets (a download
    // that stopped): the playlist must end where they do. MPEG-PS: joined
    // recordings (below, once the frame rate is known).
    if let Ok(mut src) = flux_probe::FileSource::open(path) {
        if container == flux_core::Container::MpegPs {
            ps_parts = flux_probe::tail::ps_parts(&mut src)
                .filter(|p| p.len() > 1)
                .unwrap_or_default();
        }
        if let Some(end) = flux_probe::tail::packet_end(&mut src, container) {
            let header_ms = (duration_ns > 0).then_some((duration_ns / 1_000_000) as u64);
            let (ms, w) = flux_probe::tail::corrected_ms(header_ms, &end, container);
            // Also when the header gives none (an FLV written without
            // `onMetaData`, cut so ffmpeg finds no last tag): the tags'.
            if let Some(ms) = ms.filter(|&ms| Some(ms) != header_ms) {
                duration_ns = ms as i64 * 1_000_000;
            }
            warnings.extend(w);
        }
    }
    if duration_ns <= 0 {
        return Err(HlsError::Unsupported("the file's duration is unknown".into()));
    }
    let streams = info["streams"].as_array().map(|v| v.as_slice()).unwrap_or(&[]);
    let packets = info["packets"].as_array().map(|v| v.as_slice()).unwrap_or(&[]);
    let mut tracks = Vec::new();
    let mut first_of_kind = std::collections::HashSet::new();
    for s in streams {
        let kind = match s["codec_type"].as_str() {
            Some("video") => TrackKind::Video,
            Some("audio") => TrackKind::Audio,
            Some("subtitle") => TrackKind::Subtitle,
            _ => TrackKind::Data,
        };
        let index = s["index"].as_u64().unwrap_or(0) as u32;
        let codec_name = s["codec_name"].as_str().unwrap_or("").to_string();
        let first_pts_ns = secs(&s["start_time"]).map(|t| ((t - start) * 1e9).round() as i64);
        let mut t = IndexTrack {
            index,
            id: index as u64,
            kind,
            codec: flux_probe::ffprobe::codec_of(&codec_name),
            codec_tag: codec_name,
            codec_private: Vec::new(),
            synth_config: None,
            timescale: 90_000,
            frame_duration: None,
            frame_rate: None,
            width: s["width"].as_u64().map(|v| v as u32).filter(|&v| v > 0),
            height: s["height"].as_u64().map(|v| v as u32).filter(|&v| v > 0),
            bit_depth: None,
            sample_rate: secs(&s["sample_rate"]).map(|v| v as u32),
            channels: s["channels"].as_u64().map(|v| v as u8),
            language: s["tags"]["language"]
                .as_str()
                .map(String::from)
                .filter(|l| l != "und"),
            name: s["tags"]["title"].as_str().map(String::from),
            default: first_of_kind.insert(kind as u8),
            forced: s["disposition"]["forced"].as_u64() == Some(1),
            hearing_impaired: false,
            color: None,
            hdr: None,
            dovi: None,
            reorder_depth: s["has_b_frames"].as_u64().unwrap_or(0) as u32,
            first_pts_ns,
            codec_string: None,
            profile: s["profile"].as_str().map(String::from),
            level: None,
            inband_parameter_sets: false,
            header_strip: None,
            default_duration_ns: None,
            codec_delay_ns: 0,
            open_gop: false,
            bitrate: secs(&s["bit_rate"]).map(|v| v as u64),
            extra: Default::default(),
        };
        if kind == TrackKind::Audio {
            t.timescale = t.sample_rate.unwrap_or(48_000);
        }
        if kind == TrackKind::Video {
            let sar = s["sample_aspect_ratio"].as_str().and_then(|x| {
                let (n, d) = x.split_once(':')?;
                let (n, d): (u32, u32) = (n.parse().ok()?, d.parse().ok()?);
                (n > 0 && d > 0 && n != d).then_some((n, d))
            });
            t.extra.sar = sar;
        }
        tracks.push(t);
    }
    let video = tracks.iter().position(|t| t.kind == TrackKind::Video);
    let mut seek_margin = 500_000_000;
    let mut frame_ns = 40_000_000f64;
    if let Some(vi) = video {
        let v = &mut tracks[vi];
        let s = streams
            .iter()
            .find(|s| s["index"].as_u64() == Some(v.index as u64));
        // The first video packets: their times (presentation, else decode:
        // AVI has only decode times) and keyframes.
        let (mut times, mut keys) = (Vec::new(), Vec::new());
        for p in packets
            .iter()
            .filter(|p| p["stream_index"].as_u64() == Some(v.index as u64))
        {
            let t = secs(&p["pts_time"]).or_else(|| secs(&p["dts_time"]));
            if let Some(t) = t {
                times.push(t);
                if p["flags"].as_str().is_some_and(|f| f.starts_with('K')) {
                    keys.push(t);
                }
            }
        }
        let (measured_ns, gop) = measure(&mut times, &keys);
        let rate = output_rate(
            s.and_then(|s| rational_of(&s["r_frame_rate"])),
            s.and_then(|s| rational_of(&s["avg_frame_rate"])),
            measured_ns,
        );
        let (timescale, ticks) = frame_timebase(rate);
        v.frame_rate = Some(rate);
        v.timescale = timescale;
        v.frame_duration = Some(ticks);
        v.default_duration_ns = Some((1e9 * rate.den as f64 / rate.num as f64).round() as u64);
        // The frame grid starts where the timeline does: a run's frame
        // grid (`fps=…:start_time=0`) is counted from there.
        v.first_pts_ns = Some(0);
        frame_ns = 1e9 * rate.den as f64 / rate.num as f64;
        seek_margin = seek_margin_ns(name, frame_ns, gop);
        // Where the video ends, when a little before the file does (its
        // audio runs longer): the last segment ends there, as for indexed
        // files, rather than repeating the last frame. (Not on a header's
        // word alone when it is far shorter.)
        let end = s.and_then(|s| {
            Some(((secs(&s["start_time"])? - start + secs(&s["duration"])?) * 1e9).round() as i64)
        });
        v.extra.end_ns = end.filter(|&e| e < duration_ns && e >= duration_ns / 10 * 9 && ps_parts.is_empty());
        if v.width.is_none() || v.height.is_none() {
            let (w, h) = decoded_size(ffprobe, path, v.index, limits, file_len)?;
            warnings.push(format!(
                "the header gives no video size; the first frame is {w}×{h}"
            ));
            (v.width, v.height) = (Some(w), Some(h));
        }
    }
    // Joined recordings: each part from where the one before it ends (on
    // the frame grid); the first one's clock starts at the file's start
    // time, the others' at their earliest timestamp (what ffmpeg reads a
    // part's start as).
    let mut parts = Vec::new();
    if !ps_parts.is_empty() {
        let on_grid = |ns: i64| (ns as f64 / frame_ns).round() as i64 * frame_ns.round() as i64;
        let mut at = 0i64;
        for (k, p) in ps_parts.iter().enumerate() {
            let origin = if k == 0 {
                (start * 90_000.0).round() as u64
            } else {
                p.first_pts
            };
            let start_ns = match &video {
                Some(vi) => {
                    let t = &tracks[*vi];
                    let fd = i64::from(t.frame_duration.unwrap_or(1).max(1));
                    let ticks = super::ns_to_ticks(at, t.timescale);
                    super::ticks_to_ns((ticks + fd / 2) / fd * fd, t.timescale)
                }
                None => on_grid(at),
            };
            parts.push(ExternalPart {
                offset: p.offset,
                start_ns,
            });
            let span_ns = p.last_pts.saturating_sub(origin) as i64 * 1_000_000_000 / 90_000;
            at = start_ns + span_ns + frame_ns.round() as i64;
        }
        warnings.push(format!(
            "MPEG-PS of {} joined parts (its clock starts over at each): {:.1} s in all, ffprobe says {:.1} s",
            parts.len(),
            at as f64 / 1e9,
            duration_ns as f64 / 1e9
        ));
        duration_ns = at;
    }
    Ok(PlaybackIndex {
        version: INDEX_VERSION,
        source,
        container,
        duration_ns,
        tracks,
        video,
        // No keyframe list: segments lie on a time grid and runs seek by
        // time (crate::segment, crate::vtranscode).
        keyframes: Vec::new(),
        // Only transcodes are produced (no B-frames in the output), and every
        // stream starts at or after the format start time.
        presentation_offset_ns: 0,
        method: IndexMethod::HeaderProbe,
        layout: Layout::External(ExternalLayout {
            start_time_ns: start_ns,
            seek_margin_ns: seek_margin,
            parts,
        }),
        stats: IndexStats {
            reads: 0,
            bytes_read: 0,
            micros: t0.elapsed().as_micros() as u64,
        },
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(num: u32, den: u32) -> Rational {
        Rational { num, den }
    }

    /// ASF's 1000/1 is a clock, not a rate: the packets say 29.97 (their
    /// 33/34 ms steps), and the transcode runs at that, not 1000 fps.
    #[test]
    fn the_output_rate_follows_the_packets() {
        // ms timestamps of 29.97 fps with dropped frames (a 67 ms gap).
        let mut t: Vec<f64> = (0..300)
            .map(|i| ((i as f64 * 1001.0 / 30.0).round()) / 1000.0)
            .collect();
        t.remove(200);
        t.remove(100);
        let (ns, _) = measure(&mut t, &[]);
        assert_eq!(output_rate(Some(r(1000, 1)), None, ns), r(30000, 1001));
        // The stream's exact rate when the packets agree.
        let mut t25: Vec<f64> = (0..300).map(|i| i as f64 * 0.04).collect();
        let (ns, _) = measure(&mut t25, &[]);
        assert_eq!(output_rate(Some(r(25, 1)), Some(r(25, 1)), ns), r(25, 1));
        // Soft telecine: the header says 29.97, the frames are 23.976.
        let mut film: Vec<f64> = (0..300).map(|i| i as f64 * 1001.0 / 24000.0).collect();
        let (ns, _) = measure(&mut film, &[]);
        assert_eq!(output_rate(Some(r(30000, 1001)), None, ns), r(24000, 1001));
        // No packets: the header, its average, else 25; never above 60.
        assert_eq!(output_rate(Some(r(15, 1)), None, None), r(15, 1));
        assert_eq!(output_rate(Some(r(90000, 1)), Some(r(30, 1)), None), r(30, 1));
        assert_eq!(output_rate(Some(r(90000, 1)), None, None), r(25, 1));
        assert_eq!(output_rate(Some(r(120, 1)), None, None), r(60, 1));
        // Odd rates keep a frame grid: 12.5, 14.985, 8.
        assert_eq!(cfr(12.5), Some(r(25, 2)));
        assert_eq!(cfr(15000.0 / 1001.0), Some(r(15000, 1001)));
        assert_eq!(cfr(7.8), Some(r(8, 1)));
        assert_eq!(frame_timebase(r(25, 1)), (10_000, 400));
        assert_eq!(frame_timebase(r(30000, 1001)), (30_000, 1001));
        assert_eq!(frame_timebase(r(25, 2)), (10_000, 800));
    }

    /// MPEG program streams seek to any packet: the run starts 1.5 GOPs
    /// early; others half a second (or two frames) early.
    #[test]
    fn mpeg_streams_start_a_gop_early() {
        assert_eq!(seek_margin_ns("asf", 40e6, Some(10.0)), 500_000_000);
        assert_eq!(seek_margin_ns("flv", 500e6, None), 1_000_000_000);
        assert_eq!(seek_margin_ns("mpeg", 40e6, Some(0.6)), 1_000_000_000);
        assert_eq!(seek_margin_ns("mpeg", 40e6, Some(2.0)), 3_080_000_000);
        assert_eq!(seek_margin_ns("mpegts", 40e6, None), 3_000_000_000);
        assert_eq!(seek_margin_ns("mpeg", 40e6, Some(30.0)), 10_000_000_000);
        // The longest keyframe distance of the first packets.
        let (_, gop) = measure(&mut [0.0, 0.5, 1.0], &[0.0, 0.6, 1.8, 2.4]);
        assert!(gop.is_some_and(|g| (g - 1.2).abs() < 1e-9), "{gop:?}");
    }
}
