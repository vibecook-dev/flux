//! ffprobe/ffmpeg plumbing: media facts and keyframe extraction.
//!
//! Extraction decodes **one keyframe per seek** and nothing else:
//! `-noaccurate_seek -skip_frame nokey -ss T -i FILE -frames:v 1`. The demuxer
//! seeks to the keyframe at or before `T`, the decoder discards every
//! non-keyframe, and accurate seeking is off so ffmpeg does not decode forward
//! to `T`. Some decoders output nothing at all with `-skip_frame nokey`
//! (ffmpeg's libdav1d on an AV1 file whose only keyframe lies before `T`), so
//! an empty result is retried without it: with `-noaccurate_seek` the first
//! frame out is still the one the demuxer landed on.
//!
//! Hardware decoding is **off by default**: measured on the M1 Max across all
//! 24 test files, software decode was faster for every file (p50 0.64 s vs
//! 1.49 s per poster, 4K HEVC included), because creating a VideoToolbox
//! session per process costs more than decoding one keyframe. When it is on
//! and fails (AV1 on an M1), the frame is retried in software.
//!
//! HDR (PQ/HLG) sources are read as 16-bit RGB and tone-mapped to SDR in Rust
//! (see [`super::tonemap`]); the Homebrew ffmpeg on the dev box has neither
//! `zscale` nor `libplacebo`, and a server cannot assume them.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use flux_core::{HdrFormat, ProbeResult};
use image::RgbImage;
use serde::{Deserialize, Serialize};

use super::tonemap;
use super::ArtError;

/// External tools and how to run them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    /// Use hardware decoding (`-hwaccel videotoolbox` on macOS, `-hwaccel auto`
    /// elsewhere). Default off: slower for single keyframes (see module docs).
    /// A hardware failure is retried in software.
    pub hwaccel: bool,
    /// Per-invocation timeout.
    pub timeout: Duration,
}

impl Default for Tools {
    fn default() -> Self {
        Tools {
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: PathBuf::from("ffprobe"),
            hwaccel: false,
            timeout: Duration::from_secs(60),
        }
    }
}

/// Transfer characteristic of the main video track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transfer {
    Sdr,
    /// SMPTE ST 2084 (HDR10, HDR10+, most Dolby Vision base layers).
    Pq,
    /// ARIB STD-B67.
    Hlg,
}

/// What frame extraction needs to know about a file.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MediaFacts {
    pub duration_s: f64,
    /// Display size (sample aspect ratio and rotation applied).
    pub width: u32,
    pub height: u32,
    pub transfer: Transfer,
    /// Content peak luminance in nits (MaxCLL or mastering max), for tone mapping.
    pub peak_nits: Option<f32>,
}

impl MediaFacts {
    /// From a `flux-probe` result (no subprocess). `None` without a duration or video size.
    pub fn from_probe(p: &ProbeResult) -> Option<Self> {
        let v = p.main_video()?;
        let transfer = match v.hdr.as_ref().map(|h| h.format) {
            Some(HdrFormat::Hdr10 | HdrFormat::Hdr10Plus | HdrFormat::DolbyVision) => Transfer::Pq,
            Some(HdrFormat::Hlg) => Transfer::Hlg,
            _ => match v.color.and_then(|c| c.transfer) {
                Some(16) => Transfer::Pq,
                Some(18) => Transfer::Hlg,
                _ => Transfer::Sdr,
            },
        };
        let peak_nits = v.hdr.as_ref().and_then(|h| {
            h.cll
                .map(|c| f32::from(c.max_cll))
                .filter(|&n| n > 0.0)
                .or_else(|| h.mastering.map(|m| m.max_luminance as f32 / 10_000.0))
        });
        Some(MediaFacts {
            duration_s: p.duration_ms? as f64 / 1000.0,
            width: v.width?,
            height: v.height?,
            transfer,
            peak_nits,
        })
    }

    /// Ask ffprobe (one invocation: stream, format, and the first frame's
    /// transfer, because HDR10 signalled only in SEI/VUI is invisible at stream level).
    pub async fn ffprobe(tools: &Tools, path: &Path) -> Result<Self, ArtError> {
        let args: Vec<OsString> = [
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-read_intervals",
            "%+#1",
            "-show_entries",
            "stream=width,height,sample_aspect_ratio,color_transfer:stream_side_data=rotation:format=duration:frame=color_transfer",
            "-of",
            "json",
        ]
        .iter()
        .map(OsString::from)
        .chain([path.as_os_str().to_owned()])
        .collect();
        let out = run(&tools.ffprobe, &args, tools.timeout, path).await?;
        parse_ffprobe(&out, path)
    }
}

#[derive(Deserialize)]
struct ProbeJson {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    #[serde(default)]
    frames: Vec<ProbeFrame>,
    format: Option<ProbeFormat>,
}

#[derive(Deserialize)]
struct ProbeStream {
    width: Option<u32>,
    height: Option<u32>,
    sample_aspect_ratio: Option<String>,
    color_transfer: Option<String>,
    #[serde(default)]
    side_data_list: Vec<SideData>,
}

#[derive(Deserialize)]
struct SideData {
    rotation: Option<f64>,
}

#[derive(Deserialize)]
struct ProbeFrame {
    color_transfer: Option<String>,
}

#[derive(Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
}

fn parse_ffprobe(out: &[u8], path: &Path) -> Result<MediaFacts, ArtError> {
    let bad = |reason: &str| ArtError::Probe {
        path: path.to_owned(),
        reason: reason.to_owned(),
    };
    let j: ProbeJson = serde_json::from_slice(out).map_err(|e| bad(&e.to_string()))?;
    let s = j.streams.first().ok_or_else(|| bad("no video stream"))?;
    let (w, h) = (
        s.width.ok_or_else(|| bad("no width"))?,
        s.height.ok_or_else(|| bad("no height"))?,
    );
    let duration_s = j
        .format
        .and_then(|f| f.duration)
        .and_then(|d| d.parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0)
        .ok_or_else(|| bad("no duration"))?;
    // Display width = coded width × SAR (anamorphic DVDs are 720×480 with SAR 32:27).
    let sar = s
        .sample_aspect_ratio
        .as_deref()
        .and_then(|r| r.split_once(':'))
        .and_then(|(n, d)| Some((n.parse::<f64>().ok()?, d.parse::<f64>().ok()?)))
        .filter(|(n, d)| *n > 0.0 && *d > 0.0)
        .map_or(1.0, |(n, d)| n / d);
    let mut dw = ((f64::from(w) * sar).round() as u32).max(2);
    let mut dh = h;
    // Phone videos: ffmpeg auto-rotates, so ±90° swaps the output size.
    if s.side_data_list
        .iter()
        .filter_map(|d| d.rotation)
        .any(|r| (r.abs() - 90.0).abs() < 1.0)
    {
        std::mem::swap(&mut dw, &mut dh);
    }
    let trc = s
        .color_transfer
        .as_deref()
        .filter(|t| *t != "unknown")
        .or_else(|| j.frames.first().and_then(|f| f.color_transfer.as_deref()));
    let transfer = match trc {
        Some("smpte2084") => Transfer::Pq,
        Some("arib-std-b67") => Transfer::Hlg,
        _ => Transfer::Sdr,
    };
    Ok(MediaFacts {
        duration_s,
        width: dw,
        height: dh,
        transfer,
        peak_nits: None,
    })
}

/// A decoded frame from the user's own media file (8-bit sRGB, tone-mapped if HDR).
///
/// Frames can only be produced by extracting them from a local file, which is
/// why [`crate::ai`] accepts them as local evidence for VLM description.
#[derive(Debug, Clone)]
pub struct Frame {
    pub(crate) img: RgbImage,
    pub(crate) at_s: f64,
    pub(crate) source: PathBuf,
    /// Decoded with hardware acceleration (false after a software fallback).
    pub(crate) hw: bool,
    /// Decoded with `-skip_frame nokey` (false after the empty-output fallback).
    pub(crate) keyframes_only: bool,
}

impl Frame {
    /// The pixels.
    pub fn image(&self) -> &RgbImage {
        &self.img
    }
    /// The requested seek time (the keyframe is at or before it).
    pub fn at_s(&self) -> f64 {
        self.at_s
    }
    /// The media file it came from.
    pub fn source(&self) -> &Path {
        &self.source
    }
}

/// Output size for a given width, keeping the display aspect ratio (even dimensions).
pub(crate) fn scaled_size(facts: &MediaFacts, width: u32) -> (u32, u32) {
    let w = width.min(facts.width).max(2) & !1;
    let h = ((f64::from(w) * f64::from(facts.height) / f64::from(facts.width)).round() as u32).max(2) & !1;
    (w, h)
}

/// Extract the keyframe at or before `t` seconds, scaled to `width` (capped at
/// the source width). `Ok(None)` when the file has no frame there.
pub(crate) async fn extract_keyframe(
    tools: &Tools,
    path: &Path,
    facts: &MediaFacts,
    t: f64,
    width: u32,
) -> Result<Option<Frame>, ArtError> {
    if let Some(f) = extract_frame(tools, path, facts, t, width, true).await? {
        return Ok(Some(f));
    }
    // Nothing decoded with `-skip_frame nokey` (see the module docs): decode normally.
    extract_frame(tools, path, facts, t, width, false).await
}

/// [`extract_keyframe`] with the `-skip_frame nokey` choice made by the caller
/// (`keyframes_only = false` once a file is known to need it).
pub(crate) async fn extract_frame(
    tools: &Tools,
    path: &Path,
    facts: &MediaFacts,
    t: f64,
    width: u32,
    keyframes_only: bool,
) -> Result<Option<Frame>, ArtError> {
    let (w, h) = scaled_size(facts, width);
    let hdr = facts.transfer != Transfer::Sdr;
    // HDR: keep 16 bits and the BT.2020 matrix so the PQ/HLG signal survives to Rust.
    let vf = if hdr {
        format!("scale={w}:{h}:flags=bicubic:in_color_matrix=bt2020:out_range=full,format=rgb48le")
    } else {
        format!("scale={w}:{h}:flags=bicubic,format=rgb24")
    };
    let out = match run(
        &tools.ffmpeg,
        &keyframe_args(tools.hwaccel, keyframes_only, path, t, &vf),
        tools.timeout,
        path,
    )
    .await
    {
        Ok(out) => out,
        // ffmpeg does not fall back to software when hardware setup fails
        // (e.g. AV1 on an M1: "Your platform doesn't support hardware accelerated AV1 decoding").
        Err(ArtError::Tool { stderr, .. }) if tools.hwaccel => {
            tracing::debug!(path = %path.display(), %stderr, "hardware decode failed; retrying in software");
            let out = run(
                &tools.ffmpeg,
                &keyframe_args(false, keyframes_only, path, t, &vf),
                tools.timeout,
                path,
            )
            .await?;
            return decode_raw(out, w, h, hdr, facts, t, path, false, keyframes_only);
        }
        Err(e) => return Err(e),
    };
    decode_raw(out, w, h, hdr, facts, t, path, tools.hwaccel, keyframes_only)
}

fn keyframe_args(hwaccel: bool, keyframes_only: bool, path: &Path, t: f64, vf: &str) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["-hide_banner", "-loglevel", "error", "-nostdin"]
        .map(OsString::from)
        .to_vec();
    if hwaccel {
        args.push("-hwaccel".into());
        args.push(
            if cfg!(target_os = "macos") {
                "videotoolbox"
            } else {
                "auto"
            }
            .into(),
        );
    }
    // -noaccurate_seek + -skip_frame nokey: decode exactly one keyframe (at or before t).
    args.push("-noaccurate_seek".into());
    if keyframes_only {
        args.push("-skip_frame".into());
        args.push("nokey".into());
    }
    args.push("-ss".into());
    args.push(format!("{t:.3}").into());
    args.push("-i".into());
    args.push(path.as_os_str().to_owned());
    for a in ["-map", "0:v:0", "-frames:v", "1", "-an", "-sn", "-vf"] {
        args.push(a.into());
    }
    args.push(vf.into());
    for a in ["-f", "rawvideo", "pipe:1"] {
        args.push(a.into());
    }
    args
}

#[allow(clippy::too_many_arguments)]
fn decode_raw(
    out: Vec<u8>,
    w: u32,
    h: u32,
    hdr: bool,
    facts: &MediaFacts,
    t: f64,
    path: &Path,
    hw: bool,
    keyframes_only: bool,
) -> Result<Option<Frame>, ArtError> {
    let bpp = if hdr { 6 } else { 3 };
    let need = (w * h) as usize * bpp;
    if out.len() < need {
        return Ok(None); // seek past the last keyframe, or an undecodable frame
    }
    let img = if hdr {
        let px: Vec<u16> = out[..need]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes(*b))
            .collect();
        tonemap::to_sdr(&px, w, h, facts.transfer, facts.peak_nits)
    } else {
        RgbImage::from_raw(w, h, out[..need].to_vec())
            .ok_or_else(|| ArtError::Encode("raw frame size".into()))?
    };
    Ok(Some(Frame {
        img,
        at_s: t,
        source: path.to_owned(),
        hw,
        keyframes_only,
    }))
}

async fn run(tool: &Path, args: &[OsString], timeout: Duration, path: &Path) -> Result<Vec<u8>, ArtError> {
    let child = tokio::process::Command::new(tool)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| ArtError::Spawn {
            tool: tool.display().to_string(),
            source,
        })?;
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| ArtError::Timeout {
            tool: tool.display().to_string(),
            path: path.to_owned(),
        })?
        .map_err(|source| ArtError::Spawn {
            tool: tool.display().to_string(),
            source,
        })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(ArtError::Tool {
            tool: tool.display().to_string(),
            path: path.to_owned(),
            stderr: stderr.lines().take(5).collect::<Vec<_>>().join(" | "),
        });
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ffprobe_json() {
        let j = br#"{"frames":[{"color_transfer":"smpte2084"}],"streams":[{"codec_name":"hevc","width":3840,"height":2160,"sample_aspect_ratio":"1:1"}],"format":{"duration":"30.005000"}}"#;
        let f = parse_ffprobe(j, Path::new("x")).unwrap();
        assert_eq!((f.width, f.height, f.transfer), (3840, 2160, Transfer::Pq));
        assert!((f.duration_s - 30.005).abs() < 1e-9);

        let anamorphic = br#"{"streams":[{"width":720,"height":480,"sample_aspect_ratio":"32:27"}],"format":{"duration":"10"}}"#;
        let f = parse_ffprobe(anamorphic, Path::new("x")).unwrap();
        assert_eq!((f.width, f.height, f.transfer), (853, 480, Transfer::Sdr));

        let rotated = br#"{"streams":[{"width":1920,"height":1080,"side_data_list":[{"rotation":-90}]}],"format":{"duration":"10"}}"#;
        let f = parse_ffprobe(rotated, Path::new("x")).unwrap();
        assert_eq!((f.width, f.height), (1080, 1920));
    }

    #[test]
    fn sizes_are_even_and_capped() {
        let f = MediaFacts {
            duration_s: 1.0,
            width: 640,
            height: 360,
            transfer: Transfer::Sdr,
            peak_nits: None,
        };
        assert_eq!(scaled_size(&f, 480), (480, 270));
        assert_eq!(scaled_size(&f, 4000), (640, 360));
        let f = MediaFacts {
            width: 853,
            height: 480,
            ..f
        };
        assert_eq!(scaled_size(&f, 853), (852, 478)); // 852×480/853 = 479.4 → even
    }
}
