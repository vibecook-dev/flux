//! Index for containers without a usable seek index (MPEG-TS, M2TS, AVI),
//! built from an `ffprobe` packet scan (research/03 §4.2: "TS / M2TS / AVI:
//! no reliable index → full scan, FFmpeg path").
//!
//! Only the ffmpeg-backed renditions (video and audio transcode) can be
//! produced from such an index; the planner never copies these containers
//! because `to_probe_result()` reports `has_index = false`.

use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flux_core::{Codec, Container, TrackKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    rational_rate, IndexMethod, IndexStats, IndexTrack, Keyframe, Layout, PlaybackIndex, INDEX_VERSION,
};
use crate::error::{HlsError, Result};
use crate::source::SourceId;

/// Where the stream timeline starts (ffmpeg's `-ss` is relative to it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalLayout {
    pub start_time_ns: i64,
}

/// Codec of an ffprobe `codec_name`: flux-probe's table (the catalog's
/// probe uses the same one), plus names it leaves as `Other` that still map
/// to a family the planner knows.
fn map_codec(name: &str) -> Codec {
    match flux_probe::ffprobe::codec_of(name) {
        Codec::Other => match name {
            "mpeg1video" => Codec::Mpeg2Video,
            "msmpeg4v1" | "msmpeg4v2" | "msmpeg4v3" => Codec::Mpeg4Part2,
            _ => Codec::Other,
        },
        c => c,
    }
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

/// Builds an index by running ffprobe (stream facts + a video packet scan),
/// each run bounded by `limits` and killable with [`kill_running`].
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
            "format=format_name,start_time,duration:stream=index,codec_type,codec_name,profile,width,height,has_b_frames,r_frame_rate,sample_rate,channels,start_time,bit_rate:stream_tags=language,title:stream_disposition=default,forced",
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
    let duration_ns = (secs(&format["duration"]).unwrap_or(0.0) * 1e9).round() as i64;
    let name = format["format_name"].as_str().unwrap_or("");
    let container = if name.contains("mpegts") {
        if source
            .path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("m2ts"))
        {
            Container::M2ts
        } else {
            Container::MpegTs
        }
    } else if name.contains("avi") {
        Container::Avi
    } else {
        Container::Unknown
    };
    let mut tracks = Vec::new();
    let mut first_of_kind = std::collections::HashSet::new();
    for s in info["streams"].as_array().map(|v| v.as_slice()).unwrap_or(&[]) {
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
            codec: map_codec(&codec_name),
            codec_tag: codec_name,
            codec_private: Vec::new(),
            synth_config: None,
            timescale: 90_000,
            frame_duration: None,
            frame_rate: None,
            width: s["width"].as_u64().map(|v| v as u32),
            height: s["height"].as_u64().map(|v| v as u32),
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
        if kind == TrackKind::Video {
            let rate = s["r_frame_rate"].as_str().and_then(|r| {
                let (n, d) = r.split_once('/')?;
                Some((n.parse::<u64>().ok()?, d.parse::<u64>().ok()?))
            });
            if let Some((n, d)) = rate.filter(|(n, d)| *n > 0 && *d > 0) {
                let frame_ns = (1_000_000_000 * d / n).max(1);
                t.default_duration_ns = Some(frame_ns);
                if let Some((r, ts, fd)) = rational_rate(frame_ns) {
                    t.frame_rate = Some(r);
                    t.timescale = ts;
                    t.frame_duration = Some(fd);
                }
            }
        } else if kind == TrackKind::Audio {
            t.timescale = t.sample_rate.unwrap_or(48_000);
        }
        tracks.push(t);
    }
    let video = tracks.iter().position(|t| t.kind == TrackKind::Video);
    let mut keyframes = Vec::new();
    if let Some(vi) = video {
        let sel = format!("{}", tracks[vi].index);
        let csv = run(
            ffprobe,
            &[
                "-v",
                "error",
                "-select_streams",
                &sel,
                "-show_entries",
                "packet=pts_time,dts_time,flags",
                "-of",
                "csv=p=0",
            ],
            path,
            limits,
            file_len,
        )?;
        // AVI has no presentation times (`N/A`), only decode order: a
        // keyframe is presented `has_b_frames` frames after it is decoded
        // (how ffmpeg's decoder timestamps its output, which is what the
        // ffmpeg transcode's `-ss` sees).
        let delay_s =
            tracks[vi].reorder_depth as f64 * tracks[vi].default_duration_ns.unwrap_or(0) as f64 / 1e9;
        for line in csv.lines() {
            let mut f = line.split(',');
            let (pts, dts, flags) = (f.next(), f.next(), f.next());
            let t = pts
                .and_then(|p| p.parse::<f64>().ok())
                .or_else(|| dts.and_then(|d| d.parse::<f64>().ok()).map(|d| d + delay_s));
            let (Some(pts), Some(flags)) = (t, flags) else {
                continue;
            };
            if flags.starts_with('K') {
                let ns = ((pts - start) * 1e9).round() as i64;
                keyframes.push(Keyframe {
                    pts_ns: ns,
                    raw_ts: ns / 1000,
                    pos: 0,
                    rel: None,
                });
            }
        }
        keyframes.sort_by_key(|k| k.pts_ns);
        keyframes.dedup_by_key(|k| k.pts_ns);
        if let Some(k0) = keyframes.first() {
            tracks[vi].first_pts_ns = Some(k0.pts_ns);
        }
    }
    Ok(PlaybackIndex {
        version: INDEX_VERSION,
        source,
        container,
        duration_ns,
        tracks,
        video,
        keyframes,
        // Only transcodes are produced (no B-frames in the output), and every
        // stream starts at or after the format start time.
        presentation_offset_ns: 0,
        method: IndexMethod::FfprobeScan,
        layout: Layout::External(ExternalLayout {
            start_time_ns: start_ns,
        }),
        stats: IndexStats {
            reads: 0,
            bytes_read: 0,
            micros: t0.elapsed().as_micros() as u64,
        },
        warnings: vec!["no container index: built by an ffprobe packet scan; ffmpeg paths only".into()],
    })
}
