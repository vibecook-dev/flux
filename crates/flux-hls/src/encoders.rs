//! Which H.264 encoder a video transcode uses, and the ffmpeg arguments
//! around it (docs/ENCODERS.md).
//!
//! FFmpeg policy: flux only ever runs `ffmpeg`/`ffprobe` as separate
//! processes and never links libav*. What that ffmpeg can do is detected at
//! run time, once per binary, and cached ([`FfmpegCaps::detect`]):
//!
//! 1. `ffmpeg -hide_banner -encoders`, `-decoders`, `-filters` and
//!    `-hwaccels`, parsed into name sets;
//! 2. each candidate encoder in preference order, kept only if this ffmpeg
//!    has it and its probe passes:
//!    - `h264_videotoolbox` (macOS only),
//!    - `h264_nvenc`, `h264_qsv`: a short test encode,
//!    - `h264_vaapi`: the render node (`/dev/dri/renderD128`, or
//!      `FLUX_VAAPI_DEVICE`) exists, then a test encode,
//!    - `libx264`, the software fallback (also test-encoded).
//!
//!    Every test encode runs the encoder with the same options a transcode
//!    uses, so an ffmpeg too old for one of them rejects the encoder here
//!    instead of failing every transcode.
//! 3. the first usable one wins, unless `FLUX_VIDEO_ENCODER` (or
//!    `EngineConfig::video_encoder`) names one.
//!
//! [`pipeline`] then builds, per encoder, the decode set-up, the scale /
//! tone-map / rotate filter chain and the rate control:
//!
//! | encoder | decode | scale | HDR → SDR |
//! |---|---|---|---|
//! | VideoToolbox | `videotoolbox` (H.264/HEVC) | `scale_vt` | `scale_vt` colour conversion |
//! | NVENC | `cuda` (8-bit H.264/HEVC) | `scale_cuda` | software |
//! | QSV | `qsv` (H.264/HEVC) | `scale_qsv` | software |
//! | VAAPI | `vaapi` (H.264/HEVC) | `scale_vaapi` | `tonemap_vaapi`, else download → software → upload |
//! | libx264 | software | `scale` | `tonemapx` (jellyfin-ffmpeg), else `zscale` + `tonemap` |
//!
//! "Software" tone mapping is the libx264 chain on decoded frames; without
//! `tonemapx` or `zscale` there is none (the picture keeps its HDR signal
//! and looks washed out), which `flux-server --check` reports.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};

use flux_core::Codec;
use serde::{Deserialize, Serialize};

/// The variable that forces an encoder (`auto` or empty: detect).
pub const ENCODER_VAR: &str = "FLUX_VIDEO_ENCODER";
/// The variable naming the VAAPI render node.
pub const VAAPI_DEVICE_VAR: &str = "FLUX_VAAPI_DEVICE";
/// The VAAPI render node used when `FLUX_VAAPI_DEVICE` is not set.
pub const DEFAULT_VAAPI_DEVICE: &str = "/dev/dri/renderD128";

/// An H.264 encoder flux can drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoEncoder {
    /// Apple VideoToolbox (`h264_videotoolbox`), macOS.
    VideoToolbox,
    /// NVIDIA NVENC (`h264_nvenc`).
    Nvenc,
    /// Intel Quick Sync Video (`h264_qsv`).
    Qsv,
    /// VA-API (`h264_vaapi`): Intel and AMD GPUs on Linux.
    Vaapi,
    /// x264 in software (`libx264`).
    Libx264,
}

impl VideoEncoder {
    /// Preference order of automatic selection.
    pub const ORDER: [VideoEncoder; 5] = [
        VideoEncoder::VideoToolbox,
        VideoEncoder::Nvenc,
        VideoEncoder::Qsv,
        VideoEncoder::Vaapi,
        VideoEncoder::Libx264,
    ];

    /// ffmpeg's encoder name.
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            VideoEncoder::VideoToolbox => "h264_videotoolbox",
            VideoEncoder::Nvenc => "h264_nvenc",
            VideoEncoder::Qsv => "h264_qsv",
            VideoEncoder::Vaapi => "h264_vaapi",
            VideoEncoder::Libx264 => "libx264",
        }
    }

    /// A GPU or media engine encodes (not the CPU).
    pub fn is_hardware(self) -> bool {
        self != VideoEncoder::Libx264
    }

    /// Parses a `FLUX_VIDEO_ENCODER` value: a short name (`videotoolbox`,
    /// `nvenc`, `qsv`, `vaapi`, `libx264`/`x264`/`software`) or ffmpeg's
    /// encoder name. `auto` and the empty string mean "detect" (`None`).
    pub fn parse_choice(s: &str) -> Result<Option<VideoEncoder>, String> {
        let v = s.trim().to_ascii_lowercase();
        Ok(Some(match v.as_str() {
            "" | "auto" => return Ok(None),
            "videotoolbox" | "vt" | "h264_videotoolbox" => VideoEncoder::VideoToolbox,
            "nvenc" | "nvidia" | "cuda" | "h264_nvenc" => VideoEncoder::Nvenc,
            "qsv" | "quicksync" | "h264_qsv" => VideoEncoder::Qsv,
            "vaapi" | "h264_vaapi" => VideoEncoder::Vaapi,
            "libx264" | "x264" | "software" | "sw" | "cpu" => VideoEncoder::Libx264,
            _ => {
                return Err(format!(
                    "unknown video encoder {s:?} (auto, videotoolbox, nvenc, qsv, vaapi or libx264)"
                ))
            }
        }))
    }

    /// The encoder `FLUX_VIDEO_ENCODER` forces, if any (an unknown value is
    /// logged and ignored).
    pub fn from_env() -> Option<VideoEncoder> {
        let v = std::env::var(ENCODER_VAR).ok()?;
        match VideoEncoder::parse_choice(&v) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("{ENCODER_VAR}: {e}; detecting instead");
                None
            }
        }
    }
}

impl std::fmt::Display for VideoEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.ffmpeg_name())
    }
}

impl FromStr for VideoEncoder {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        VideoEncoder::parse_choice(s)?.ok_or_else(|| "expected an encoder, not auto".to_string())
    }
}

/// The VAAPI render node: `FLUX_VAAPI_DEVICE`, else `/dev/dri/renderD128`.
pub fn vaapi_device_from_env() -> PathBuf {
    std::env::var_os(VAAPI_DEVICE_VAR)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_VAAPI_DEVICE))
}

/// How [`FfmpegCaps::detect`] probes.
#[derive(Debug, Clone)]
pub struct DetectOptions {
    /// Use this encoder whatever the probes say (`FLUX_VIDEO_ENCODER`).
    pub forced: Option<VideoEncoder>,
    /// The VAAPI render node.
    pub vaapi_device: PathBuf,
    /// Run a short test encode per candidate (off: trust the encoder list).
    pub test_encodes: bool,
    /// Probe every candidate, not just up to the first usable one
    /// (`flux-server --check`).
    pub probe_all: bool,
    /// Limit per ffmpeg run.
    pub timeout: Duration,
}

impl Default for DetectOptions {
    fn default() -> Self {
        DetectOptions {
            forced: None,
            vaapi_device: PathBuf::from(DEFAULT_VAAPI_DEVICE),
            test_encodes: true,
            probe_all: false,
            timeout: Duration::from_secs(15),
        }
    }
}

/// What the probe of one candidate found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub encoder: VideoEncoder,
    /// `None`: not probed (an earlier candidate won).
    pub usable: Option<bool>,
    /// Why (not) usable.
    pub note: String,
}

/// Software HDR → SDR tone mapping available to the filter chains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SoftwareTonemap {
    /// jellyfin-ffmpeg's `tonemapx` (one filter, SIMD).
    Tonemapx,
    /// `zscale` (zimg) to linear light, `tonemap`, back to BT.709.
    ZscaleTonemap,
}

/// What an ffmpeg binary can do (cached by the transcoder).
#[derive(Debug, Clone, Default, Serialize)]
pub struct FfmpegCaps {
    pub ffmpeg: PathBuf,
    /// First line of `ffmpeg -version`.
    pub version: Option<String>,
    /// Why ffmpeg could not be run.
    pub error: Option<String>,
    #[serde(skip)]
    pub encoders: BTreeSet<String>,
    #[serde(skip)]
    pub decoders: BTreeSet<String>,
    #[serde(skip)]
    pub filters: BTreeSet<String>,
    /// `ffmpeg -hwaccels`.
    pub hwaccels: BTreeSet<String>,
    /// The VAAPI render node, when it exists.
    pub vaapi_device: Option<PathBuf>,
    /// The encoder forced by configuration.
    pub forced: Option<VideoEncoder>,
    /// Every candidate, in preference order.
    pub candidates: Vec<Candidate>,
    /// The encoder transcodes use.
    pub chosen: Option<VideoEncoder>,
}

/// Runs ffmpeg with `args`, returning stdout, or why it failed.
fn run_capture(ffmpeg: &Path, args: &[String], timeout: Duration) -> Result<Vec<u8>, String> {
    let mut child = Command::new(ffmpeg)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", ffmpeg.display()))?;
    // Drain both pipes on threads so a chatty child never blocks on a full pipe.
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let out_t = std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(o) = out.as_mut() {
            let _ = o.read_to_end(&mut b);
        }
        b
    });
    let err_t = std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(e) = err.as_mut() {
            let _ = e.read_to_end(&mut b);
        }
        b
    });
    let t0 = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if t0.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {} s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(format!("wait: {e}")),
        }
    };
    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();
    if status.success() {
        Ok(stdout)
    } else {
        let msg = String::from_utf8_lossy(&stderr);
        let first = msg.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
        Err(format!("{status}: {first}"))
    }
}

/// Names in a `-encoders`/`-decoders` listing (` V....D libx264  …`).
pub fn parse_codec_list(out: &str) -> BTreeSet<String> {
    out.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let flags = w.next()?;
            let name = w.next()?;
            let is_row =
                flags.len() == 6 && flags.chars().all(|c| c == '.' || c.is_ascii_uppercase()) && name != "=";
            is_row.then(|| name.to_string())
        })
        .collect()
}

/// Names in a `-filters` listing (` .. scale  V->V  …`, flags of 2–3 chars).
pub fn parse_filter_list(out: &str) -> BTreeSet<String> {
    out.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let flags = w.next()?;
            let name = w.next()?;
            let io = w.next()?;
            let is_row = (2..=3).contains(&flags.len())
                && flags.chars().all(|c| c == '.' || c.is_ascii_uppercase())
                && io.contains("->");
            is_row.then(|| name.to_string())
        })
        .collect()
}

/// Names in a `-hwaccels` listing (one per line after the header).
pub fn parse_hwaccel_list(out: &str) -> BTreeSet<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.ends_with(':') && !l.contains(' '))
        .map(str::to_string)
        .collect()
}

impl FfmpegCaps {
    /// Probes `ffmpeg` (blocking: run it on a blocking thread).
    pub fn detect(ffmpeg: &Path, opts: &DetectOptions) -> FfmpegCaps {
        let mut caps = FfmpegCaps {
            ffmpeg: ffmpeg.to_path_buf(),
            forced: opts.forced,
            ..Default::default()
        };
        let q = |args: &[&str]| {
            let mut a = vec!["-hide_banner".to_string()];
            a.extend(args.iter().map(|s| s.to_string()));
            run_capture(ffmpeg, &a, opts.timeout).map(|o| String::from_utf8_lossy(&o).into_owned())
        };
        match q(&["-version"]) {
            Ok(v) => caps.version = v.lines().next().map(|l| l.trim().to_string()),
            Err(e) => {
                caps.error = Some(e);
                caps.chosen = opts.forced;
                return caps;
            }
        }
        caps.encoders = q(&["-encoders"])
            .map(|o| parse_codec_list(&o))
            .unwrap_or_default();
        caps.decoders = q(&["-decoders"])
            .map(|o| parse_codec_list(&o))
            .unwrap_or_default();
        caps.filters = q(&["-filters"])
            .map(|o| parse_filter_list(&o))
            .unwrap_or_default();
        caps.hwaccels = q(&["-hwaccels"])
            .map(|o| parse_hwaccel_list(&o))
            .unwrap_or_default();
        caps.vaapi_device = Some(opts.vaapi_device.clone()).filter(|d| cfg!(unix) && d.exists());
        let probe_until_found = !opts.probe_all && opts.forced.is_none();
        for enc in VideoEncoder::ORDER {
            if probe_until_found && caps.chosen.is_some() {
                caps.candidates.push(Candidate {
                    encoder: enc,
                    usable: None,
                    note: "not probed (an earlier encoder is used)".into(),
                });
                continue;
            }
            if opts.forced.is_some() && !opts.probe_all && opts.forced != Some(enc) {
                caps.candidates.push(Candidate {
                    encoder: enc,
                    usable: None,
                    note: "not probed (an encoder is forced)".into(),
                });
                continue;
            }
            let (usable, note) = caps.probe(enc, opts);
            if usable && caps.chosen.is_none() && opts.forced.is_none() {
                caps.chosen = Some(enc);
            }
            caps.candidates.push(Candidate {
                encoder: enc,
                usable: Some(usable),
                note,
            });
        }
        if opts.forced.is_some() {
            caps.chosen = opts.forced;
        }
        caps
    }

    /// Whether `enc` works here, and why (not).
    fn probe(&self, enc: VideoEncoder, opts: &DetectOptions) -> (bool, String) {
        if !self.encoders.contains(enc.ffmpeg_name()) {
            return (false, "not built into this ffmpeg".into());
        }
        if enc == VideoEncoder::VideoToolbox && !cfg!(target_os = "macos") {
            return (false, "macOS only".into());
        }
        if enc == VideoEncoder::Vaapi && self.vaapi_device.is_none() {
            return (
                false,
                format!("no render node at {}", opts.vaapi_device.display()),
            );
        }
        if !opts.test_encodes {
            return (true, "listed (not test-encoded)".into());
        }
        match run_capture(&self.ffmpeg, &self.test_encode_args(enc, opts), opts.timeout) {
            Ok(_) => (true, "test encode passed".into()),
            Err(e) => (false, format!("test encode failed: {e}")),
        }
    }

    /// A ten-frame 320×240 encode with the options transcodes use.
    pub fn test_encode_args(&self, enc: VideoEncoder, opts: &DetectOptions) -> Vec<String> {
        let mut a: Vec<String> = ["-hide_banner", "-nostdin", "-loglevel", "error"]
            .map(String::from)
            .to_vec();
        let upload = enc == VideoEncoder::Vaapi;
        if upload {
            a.extend(vaapi_device_args(&opts.vaapi_device, false));
        }
        a.extend(
            [
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x240:rate=25",
                "-frames:v",
                "10",
                "-vf",
            ]
            .map(String::from),
        );
        a.push(if upload {
            "format=nv12,hwupload".into()
        } else {
            format!("format={}", sw_format(enc))
        });
        a.extend(encoder_args(enc, 41, 1_000_000));
        a.extend(["-f", "null", "-"].map(String::from));
        a
    }

    /// The encoder transcodes use.
    pub fn chosen(&self) -> Option<VideoEncoder> {
        self.chosen
    }

    pub fn has_filter(&self, f: &str) -> bool {
        self.filters.contains(f)
    }

    pub fn has_encoder(&self, e: &str) -> bool {
        self.encoders.contains(e)
    }

    /// Software tone mapping available (`tonemapx` first).
    pub fn software_tonemap(&self) -> Option<SoftwareTonemap> {
        if self.has_filter("tonemapx") {
            Some(SoftwareTonemap::Tonemapx)
        } else if self.has_filter("zscale") && self.has_filter("tonemap") {
            Some(SoftwareTonemap::ZscaleTonemap)
        } else {
            None
        }
    }

    /// How `enc` tone-maps HDR to SDR here (a short description), or `None`.
    pub fn tonemap_for(&self, enc: VideoEncoder) -> Option<String> {
        let sw = self.software_tonemap().map(|t| match t {
            SoftwareTonemap::Tonemapx => "tonemapx (software)",
            SoftwareTonemap::ZscaleTonemap => "zscale + tonemap (software)",
        });
        let hw = match enc {
            VideoEncoder::VideoToolbox if self.has_filter("scale_vt") => Some("scale_vt (hardware)"),
            VideoEncoder::Vaapi if self.has_filter("tonemap_vaapi") => Some("tonemap_vaapi (hardware)"),
            _ => None,
        };
        match (hw, sw) {
            (Some(h), Some(s)) => Some(format!("{h}; {s} for software-decoded sources")),
            (Some(h), None) => Some(h.to_string()),
            (None, Some(s)) => Some(s.to_string()),
            (None, None) => None,
        }
    }

    /// Things a complete set-up has that this one lacks (for `--check`).
    pub fn missing(&self) -> Vec<String> {
        let mut m = Vec::new();
        if let Some(e) = &self.error {
            m.push(format!("ffmpeg: {e}"));
            return m;
        }
        if self.chosen.is_none() {
            m.push("no usable H.264 encoder: video transcodes fail (install an ffmpeg with libx264)".into());
        }
        if !self.has_encoder("aac") {
            m.push("no aac encoder: audio transcodes fail".into());
        }
        if let Some(enc) = self.chosen {
            if self.tonemap_for(enc).is_none() {
                m.push(
                    "no HDR → SDR tone mapping (needs zscale or jellyfin-ffmpeg's tonemapx): \
                     HDR sources look washed out on SDR screens"
                        .into(),
                );
            }
            if !enc.is_hardware() {
                m.push("no hardware H.264 encoder: video transcodes use the CPU (libx264)".into());
            }
        }
        m
    }
}

/// `-init_hw_device` set-up for a VAAPI render node (and its decoder).
fn vaapi_device_args(dev: &Path, decode: bool) -> Vec<String> {
    let mut a = vec![
        "-init_hw_device".to_string(),
        format!("vaapi=va:{}", dev.display()),
    ];
    if decode {
        a.extend(
            [
                "-hwaccel",
                "vaapi",
                "-hwaccel_device",
                "va",
                "-hwaccel_output_format",
                "vaapi",
            ]
            .map(String::from),
        );
    }
    a.extend(["-filter_hw_device", "va"].map(String::from));
    a
}

/// The pixel format an encoder takes from a software chain.
fn sw_format(enc: VideoEncoder) -> &'static str {
    match enc {
        VideoEncoder::Libx264 => "yuv420p",
        _ => "nv12",
    }
}

/// Encoder and rate-control arguments: H.264 High at `level` (e.g. 41),
/// averaging `bitrate` with peaks to 1.5×, no B-frames (each piece's CTS
/// is the constant offset D), no automatic keyframes (`-g` huge, no scene
/// cuts): the only IDRs are the ones `-force_key_frames` asks for.
pub fn encoder_args(enc: VideoEncoder, level: u8, bitrate: u64) -> Vec<String> {
    let br = bitrate.max(1);
    let dotted = format!("{}.{}", level / 10, level % 10);
    let rate = |a: &mut Vec<String>| {
        a.extend([
            "-b:v".to_string(),
            br.to_string(),
            "-maxrate".into(),
            (br * 3 / 2).to_string(),
            "-bufsize".into(),
            (br * 2).to_string(),
        ])
    };
    let mut a = vec!["-c:v".to_string(), enc.ffmpeg_name().to_string()];
    match enc {
        VideoEncoder::VideoToolbox => {
            a.extend(["-profile:v".to_string(), "high".into(), "-level:v".into(), dotted]);
            rate(&mut a);
        }
        VideoEncoder::Nvenc => {
            a.extend(
                [
                    "-preset",
                    "p4",
                    "-profile:v",
                    "high",
                    "-level:v",
                    dotted.as_str(),
                    "-rc",
                    "vbr",
                    "-no-scenecut",
                    "1",
                    "-forced-idr",
                    "1",
                ]
                .map(String::from),
            );
            rate(&mut a);
        }
        VideoEncoder::Qsv => {
            a.extend(
                [
                    "-preset",
                    "veryfast",
                    "-profile:v",
                    "high",
                    // The generic integer level (h264_qsv has no named levels).
                    "-level:v",
                    &level.to_string(),
                    "-forced_idr",
                    "1",
                ]
                .map(String::from),
            );
            rate(&mut a);
        }
        VideoEncoder::Vaapi => {
            a.extend(
                [
                    "-profile:v",
                    "high",
                    "-level:v",
                    dotted.as_str(),
                    "-rc_mode",
                    "VBR",
                ]
                .map(String::from),
            );
            rate(&mut a);
        }
        VideoEncoder::Libx264 => {
            // Capped CRF: quality-driven like a good VBR encode, with the
            // same peak and buffer as the hardware encoders' rate control.
            a.extend(
                [
                    "-preset",
                    "veryfast",
                    "-profile:v",
                    "high",
                    "-level:v",
                    dotted.as_str(),
                    "-crf",
                    "23",
                    "-maxrate",
                    &(br * 3 / 2).to_string(),
                    "-bufsize",
                    &(br * 2).to_string(),
                    "-sc_threshold",
                    "0",
                    "-forced-idr",
                    "1",
                ]
                .map(String::from),
            );
        }
    }
    a.extend(["-bf", "0", "-g", "1000000"].map(String::from));
    a
}

/// What a transcode run needs from the source and the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineInput {
    pub codec: Codec,
    /// Luma bit depth, when known.
    pub bit_depth: Option<u8>,
    /// Clockwise display rotation (0/90/180/270), turned into the pixels.
    pub rotation: u16,
    /// Scaled size before rotation.
    pub width: u32,
    pub height: u32,
    pub tonemap: bool,
    pub bitrate: u64,
    /// H.264 `level_idc` (41 = 4.1).
    pub level: u8,
}

/// The ffmpeg arguments of one transcode run, around the input and the
/// `trim`/`setpts` that start the filter chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoPipeline {
    pub encoder: VideoEncoder,
    /// Before `-i`: hardware device and decoder set-up.
    pub input_args: Vec<String>,
    /// The filter chain after `trim,setpts` (scale, tone map, rotate, format).
    pub filters: String,
    /// After the filters: encoder and rate control.
    pub output_args: Vec<String>,
    /// Frames are decoded by the hardware.
    pub hw_decode: bool,
}

/// The source codecs every hardware decoder here handles (others decode in
/// software: MPEG-2/MPEG-4 Part 2 have hwaccels too, but a failed hwaccel
/// with hardware output frames has no software fallback).
fn hw_codec(c: Codec) -> bool {
    matches!(c, Codec::H264 | Codec::Hevc)
}

/// The software scale / tone-map / rotate chain ending in `fmt`.
fn sw_chain(caps: &FfmpegCaps, inp: &PipelineInput, fmt: &str) -> String {
    let mut v = vec![format!("scale={}:{}", inp.width, inp.height)];
    if inp.tonemap {
        match caps.software_tonemap() {
            Some(SoftwareTonemap::Tonemapx) => v.push(
                "tonemapx=tonemap=bt2390:desat=0:peak=100:t=bt709:m=bt709:p=bt709:format=yuv420p".into(),
            ),
            Some(SoftwareTonemap::ZscaleTonemap) => v.push(
                "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,\
                 tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv"
                    .into(),
            ),
            None => {}
        }
    }
    match inp.rotation {
        90 => v.push("transpose=clock".into()),
        180 => v.push("hflip,vflip".into()),
        270 => v.push("transpose=cclock".into()),
        _ => {}
    }
    v.push(format!("format={fmt}"));
    v.join(",")
}

/// Decode, filters and encoder for one run with `enc` (see the module
/// docs' table). `vaapi_device` is the render node for VAAPI.
pub fn pipeline(
    caps: &FfmpegCaps,
    enc: VideoEncoder,
    inp: &PipelineInput,
    vaapi_device: &Path,
) -> VideoPipeline {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
    let (w, h) = (inp.width, inp.height);
    let eight_bit = inp.bit_depth.is_none_or(|d| d <= 8);
    let hwaccel = |name: &str| caps.hwaccels.contains(name);
    let output_args = encoder_args(enc, inp.level, inp.bitrate);
    let (input_args, filters, hw_decode) = match enc {
        VideoEncoder::VideoToolbox => {
            if hw_codec(inp.codec) && hwaccel("videotoolbox") && caps.has_filter("scale_vt") {
                let mut f = format!("scale_vt=w={w}:h={h}");
                if inp.tonemap {
                    f.push_str(":color_matrix=bt709:color_primaries=bt709:color_transfer=bt709");
                }
                match inp.rotation {
                    90 => f.push_str(",transpose_vt=dir=clock"),
                    180 => f.push_str(",transpose_vt=dir=reversal"),
                    270 => f.push_str(",transpose_vt=dir=cclock"),
                    _ => {}
                }
                (
                    s(&[
                        "-hwaccel",
                        "videotoolbox",
                        "-hwaccel_output_format",
                        "videotoolbox_vld",
                    ]),
                    f,
                    true,
                )
            } else {
                (Vec::new(), sw_chain(caps, inp, "nv12"), false)
            }
        }
        VideoEncoder::Nvenc => {
            let full_hw = hw_codec(inp.codec)
                && eight_bit
                && !inp.tonemap
                && inp.rotation == 0
                && hwaccel("cuda")
                && caps.has_filter("scale_cuda");
            if full_hw {
                (
                    s(&["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"]),
                    format!("scale_cuda=w={w}:h={h}"),
                    true,
                )
            } else {
                (Vec::new(), sw_chain(caps, inp, "nv12"), false)
            }
        }
        VideoEncoder::Qsv => {
            let decoder = match inp.codec {
                Codec::H264 => "h264_qsv",
                _ => "hevc_qsv",
            };
            let full_hw = hw_codec(inp.codec)
                && !inp.tonemap
                && inp.rotation == 0
                && hwaccel("qsv")
                && caps.decoders.contains(decoder)
                && caps.has_filter("scale_qsv");
            if full_hw {
                (
                    s(&[
                        "-hwaccel",
                        "qsv",
                        "-hwaccel_output_format",
                        "qsv",
                        "-c:v",
                        decoder,
                    ]),
                    format!("scale_qsv=w={w}:h={h}:format=nv12"),
                    true,
                )
            } else {
                (Vec::new(), sw_chain(caps, inp, "nv12"), false)
            }
        }
        VideoEncoder::Vaapi => {
            let decode = hw_codec(inp.codec) && hwaccel("vaapi");
            let rot_hw = match inp.rotation {
                0 => Some(""),
                _ if !caps.has_filter("transpose_vaapi") => None,
                90 => Some(",transpose_vaapi=dir=clock"),
                180 => Some(",transpose_vaapi=dir=reversal"),
                270 => Some(",transpose_vaapi=dir=cclock"),
                _ => Some(""),
            };
            let tonemap_hw = !inp.tonemap || caps.has_filter("tonemap_vaapi");
            if decode && caps.has_filter("scale_vaapi") && tonemap_hw && rot_hw.is_some() {
                let mut f = if inp.tonemap {
                    format!("scale_vaapi=w={w}:h={h},tonemap_vaapi=format=nv12:t=bt709:m=bt709:p=bt709")
                } else {
                    format!("scale_vaapi=w={w}:h={h}:format=nv12")
                };
                f.push_str(rot_hw.unwrap_or(""));
                (vaapi_device_args(vaapi_device, true), f, true)
            } else if decode && inp.bit_depth.is_some() {
                // Download → software → upload.
                let dl = if eight_bit { "nv12" } else { "p010le" };
                let f = format!("hwdownload,format={dl},{},hwupload", sw_chain(caps, inp, "nv12"));
                (vaapi_device_args(vaapi_device, true), f, true)
            } else {
                let f = format!("{},hwupload", sw_chain(caps, inp, "nv12"));
                (vaapi_device_args(vaapi_device, false), f, false)
            }
        }
        VideoEncoder::Libx264 => (Vec::new(), sw_chain(caps, inp, "yuv420p"), false),
    };
    VideoPipeline {
        encoder: enc,
        input_args,
        filters,
        output_args,
        hw_decode,
    }
}

/// [`pipeline`] with software decode and filters, still encoding with
/// `enc`: for a run that draws subtitles into the picture, whose overlay
/// needs the frames in memory ([`crate::burn`]). Returns it with `filters`
/// ending in the encoder's software pixel format, and the filter that then
/// hands the frames to the encoder, if it needs one (`hwupload`, VAAPI).
pub fn software_pipeline(
    caps: &FfmpegCaps,
    enc: VideoEncoder,
    inp: &PipelineInput,
    vaapi_device: &Path,
) -> (VideoPipeline, Option<&'static str>) {
    // Without hwaccels every encoder takes its software chain.
    let sw = FfmpegCaps {
        hwaccels: Default::default(),
        ..caps.clone()
    };
    let mut p = pipeline(&sw, enc, inp, vaapi_device);
    let upload = match p.filters.strip_suffix(",hwupload") {
        Some(f) => {
            p.filters = f.to_string();
            Some("hwupload")
        }
        None => None,
    };
    (p, upload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burn_in_runs_decode_in_software() {
        let c = caps(
            &["scale_cuda", "scale_vaapi", "scale_vt", "tonemapx"],
            &["cuda", "vaapi", "videotoolbox"],
            &[],
        );
        for enc in [
            VideoEncoder::Nvenc,
            VideoEncoder::VideoToolbox,
            VideoEncoder::Libx264,
        ] {
            let (p, upload) = software_pipeline(&c, enc, &input(Codec::H264), Path::new(DEV));
            assert!(!p.hw_decode && p.input_args.is_empty(), "{enc}: {p:?}");
            assert!(p.filters.starts_with("scale=1280:720"), "{enc}: {}", p.filters);
            assert_eq!(upload, None);
        }
        // VAAPI encodes from its own surfaces: upload after the overlay.
        let (p, upload) = software_pipeline(&c, VideoEncoder::Vaapi, &input(Codec::H264), Path::new(DEV));
        assert!(!p.hw_decode && !p.filters.contains("hwupload"), "{p:?}");
        assert!(p.filters.ends_with("format=nv12"), "{}", p.filters);
        assert_eq!(upload, Some("hwupload"));
    }

    fn caps(filters: &[&str], hwaccels: &[&str], decoders: &[&str]) -> FfmpegCaps {
        FfmpegCaps {
            filters: filters.iter().map(|s| s.to_string()).collect(),
            hwaccels: hwaccels.iter().map(|s| s.to_string()).collect(),
            decoders: decoders.iter().map(|s| s.to_string()).collect(),
            encoders: ["libx264", "aac"].iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn input(codec: Codec) -> PipelineInput {
        PipelineInput {
            codec,
            bit_depth: Some(8),
            rotation: 0,
            width: 1280,
            height: 720,
            tonemap: false,
            bitrate: 4_000_000,
            level: 41,
        }
    }

    const DEV: &str = "/dev/dri/renderD128";

    #[test]
    fn parses_ffmpeg_listings() {
        let enc = "Encoders:\n V..... = Video\n A..... = Audio\n ------\n V....D libx264              libx264 H.264\n V....D h264_videotoolbox    VideoToolbox H.264 Encoder (codec h264)\n A....D aac                  AAC (Advanced Audio Coding)\n";
        let e = parse_codec_list(enc);
        assert!(e.contains("libx264") && e.contains("h264_videotoolbox") && e.contains("aac"));
        assert!(!e.contains("=") && !e.contains("Video"));
        let filt = "Filters:\n  T.. = Timeline support\n  ... trim              V->V       Pick one continuous section.\n .S tonemap           V->V       Conversion to/from different dynamic ranges.\n .. scale_vt          V->V       Scale Videotoolbox frames\n";
        let f = parse_filter_list(filt);
        assert!(f.contains("trim") && f.contains("tonemap") && f.contains("scale_vt"));
        assert!(!f.contains("="));
        let hw = parse_hwaccel_list("Hardware acceleration methods:\nvideotoolbox\ncuda\n\n");
        assert_eq!(hw.into_iter().collect::<Vec<_>>(), vec!["cuda", "videotoolbox"]);
    }

    #[test]
    fn encoder_names_parse() {
        assert_eq!(VideoEncoder::parse_choice("auto"), Ok(None));
        assert_eq!(VideoEncoder::parse_choice(""), Ok(None));
        assert_eq!(
            VideoEncoder::parse_choice("libx264"),
            Ok(Some(VideoEncoder::Libx264))
        );
        assert_eq!(
            VideoEncoder::parse_choice("Software"),
            Ok(Some(VideoEncoder::Libx264))
        );
        assert_eq!(
            VideoEncoder::parse_choice("h264_vaapi"),
            Ok(Some(VideoEncoder::Vaapi))
        );
        assert_eq!(VideoEncoder::parse_choice("NVENC"), Ok(Some(VideoEncoder::Nvenc)));
        assert!(VideoEncoder::parse_choice("h265").is_err());
        for e in VideoEncoder::ORDER {
            assert_eq!(e.ffmpeg_name().parse::<VideoEncoder>(), Ok(e));
        }
    }

    #[test]
    fn videotoolbox_keeps_its_chain() {
        let c = caps(&["scale_vt", "transpose_vt"], &["videotoolbox"], &[]);
        let mut i = input(Codec::Hevc);
        i.tonemap = true;
        i.rotation = 90;
        let p = pipeline(&c, VideoEncoder::VideoToolbox, &i, Path::new(DEV));
        assert_eq!(
            p.input_args,
            [
                "-hwaccel",
                "videotoolbox",
                "-hwaccel_output_format",
                "videotoolbox_vld"
            ]
        );
        assert_eq!(
            p.filters,
            "scale_vt=w=1280:h=720:color_matrix=bt709:color_primaries=bt709:color_transfer=bt709,\
             transpose_vt=dir=clock"
        );
        // MPEG-4 Part 2 decodes in software.
        let p = pipeline(
            &c,
            VideoEncoder::VideoToolbox,
            &input(Codec::Mpeg4Part2),
            Path::new(DEV),
        );
        assert!(p.input_args.is_empty() && !p.hw_decode);
        assert_eq!(p.filters, "scale=1280:720,format=nv12");
        assert_eq!(
            p.output_args,
            [
                "-c:v",
                "h264_videotoolbox",
                "-profile:v",
                "high",
                "-level:v",
                "4.1",
                "-b:v",
                "4000000",
                "-maxrate",
                "6000000",
                "-bufsize",
                "8000000",
                "-bf",
                "0",
                "-g",
                "1000000"
            ]
        );
    }

    #[test]
    fn software_chain_tonemaps_with_what_there_is() {
        let mut i = input(Codec::Hevc);
        i.tonemap = true;
        i.bit_depth = Some(10);
        i.rotation = 270;
        let z = caps(&["zscale", "tonemap"], &[], &[]);
        let p = pipeline(&z, VideoEncoder::Libx264, &i, Path::new(DEV));
        assert_eq!(
            p.filters,
            "scale=1280:720,zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,\
             tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,transpose=cclock,format=yuv420p"
        );
        let x = caps(&["zscale", "tonemap", "tonemapx"], &[], &[]);
        let p = pipeline(&x, VideoEncoder::Libx264, &i, Path::new(DEV));
        assert!(p.filters.contains(",tonemapx=tonemap=bt2390:"), "{}", p.filters);
        // Nothing to tone-map with: scale only (reported by --check).
        let none = caps(&["tonemap"], &[], &[]);
        let p = pipeline(&none, VideoEncoder::Libx264, &i, Path::new(DEV));
        assert_eq!(p.filters, "scale=1280:720,transpose=cclock,format=yuv420p");
        assert!(p.output_args.windows(2).any(|w| w == ["-preset", "veryfast"]));
        assert!(p.output_args.windows(2).any(|w| w == ["-crf", "23"]));
        assert!(p.output_args.windows(2).any(|w| w == ["-maxrate", "6000000"]));
        assert!(!p.output_args.iter().any(|a| a == "-b:v"));
    }

    #[test]
    fn nvenc_and_qsv_scale_on_the_gpu_when_they_can() {
        let c = caps(
            &["scale_cuda", "scale_qsv"],
            &["cuda", "qsv"],
            &["h264_qsv", "hevc_qsv"],
        );
        let p = pipeline(&c, VideoEncoder::Nvenc, &input(Codec::H264), Path::new(DEV));
        assert_eq!(
            p.input_args,
            ["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"]
        );
        assert_eq!(p.filters, "scale_cuda=w=1280:h=720");
        let p = pipeline(&c, VideoEncoder::Qsv, &input(Codec::Hevc), Path::new(DEV));
        assert_eq!(
            p.input_args,
            [
                "-hwaccel",
                "qsv",
                "-hwaccel_output_format",
                "qsv",
                "-c:v",
                "hevc_qsv"
            ]
        );
        assert_eq!(p.filters, "scale_qsv=w=1280:h=720:format=nv12");
        // Tone mapping (and 10-bit for NVENC) goes through software.
        let mut i = input(Codec::Hevc);
        i.tonemap = true;
        i.bit_depth = Some(10);
        for e in [VideoEncoder::Nvenc, VideoEncoder::Qsv] {
            let p = pipeline(&c, e, &i, Path::new(DEV));
            assert!(!p.hw_decode && p.input_args.is_empty(), "{e}");
            assert_eq!(p.filters, "scale=1280:720,format=nv12", "{e}");
        }
    }

    #[test]
    fn vaapi_uses_its_filters_or_downloads() {
        let full = caps(&["scale_vaapi", "tonemap_vaapi"], &["vaapi"], &[]);
        let mut i = input(Codec::Hevc);
        i.tonemap = true;
        i.bit_depth = Some(10);
        let p = pipeline(&full, VideoEncoder::Vaapi, &i, Path::new(DEV));
        assert_eq!(
            p.input_args,
            [
                "-init_hw_device",
                "vaapi=va:/dev/dri/renderD128",
                "-hwaccel",
                "vaapi",
                "-hwaccel_device",
                "va",
                "-hwaccel_output_format",
                "vaapi",
                "-filter_hw_device",
                "va"
            ]
        );
        assert_eq!(
            p.filters,
            "scale_vaapi=w=1280:h=720,tonemap_vaapi=format=nv12:t=bt709:m=bt709:p=bt709"
        );
        // No tonemap_vaapi: download, tone-map in software, upload.
        let partial = caps(&["scale_vaapi", "zscale", "tonemap"], &["vaapi"], &[]);
        let p = pipeline(&partial, VideoEncoder::Vaapi, &i, Path::new(DEV));
        assert!(p.hw_decode);
        assert!(
            p.filters
                .starts_with("hwdownload,format=p010le,scale=1280:720,zscale="),
            "{}",
            p.filters
        );
        assert!(p.filters.ends_with(",format=nv12,hwupload"), "{}", p.filters);
        // A codec the GPU doesn't decode: software decode, then upload.
        let p = pipeline(
            &full,
            VideoEncoder::Vaapi,
            &input(Codec::Mpeg2Video),
            Path::new(DEV),
        );
        assert!(!p.hw_decode);
        assert_eq!(
            p.input_args,
            [
                "-init_hw_device",
                "vaapi=va:/dev/dri/renderD128",
                "-filter_hw_device",
                "va"
            ]
        );
        assert_eq!(p.filters, "scale=1280:720,format=nv12,hwupload");
        assert!(p.output_args.windows(2).any(|w| w == ["-rc_mode", "VBR"]));
    }

    #[test]
    fn test_encodes_use_the_transcode_options() {
        let c = caps(&[], &[], &[]);
        let o = DetectOptions::default();
        let a = c.test_encode_args(VideoEncoder::Vaapi, &o);
        let s = a.join(" ");
        assert!(s.contains("-init_hw_device vaapi=va:/dev/dri/renderD128"), "{s}");
        assert!(s.contains("-vf format=nv12,hwupload -c:v h264_vaapi"), "{s}");
        let s = c.test_encode_args(VideoEncoder::Libx264, &o).join(" ");
        assert!(
            s.contains("-vf format=yuv420p -c:v libx264 -preset veryfast"),
            "{s}"
        );
        assert!(s.ends_with("-f null -"), "{s}");
    }

    /// The real ffmpeg on this machine (skips without one): the chosen
    /// encoder passed its test encode, and forcing libx264 is honoured.
    #[test]
    fn detects_this_machines_ffmpeg() {
        let Some(ff) = crate::ffmpeg_tools::find("ffmpeg") else {
            eprintln!("skipping: no ffmpeg");
            return;
        };
        let caps = FfmpegCaps::detect(&ff, &DetectOptions::default());
        assert!(caps.error.is_none(), "{caps:?}");
        if let Some(e) = caps.chosen() {
            let c = caps
                .candidates
                .iter()
                .find(|c| c.encoder == e)
                .expect("candidate");
            assert_eq!(c.usable, Some(true), "{caps:?}");
        }
        if caps.has_encoder("libx264") {
            let forced = FfmpegCaps::detect(
                &ff,
                &DetectOptions {
                    forced: Some(VideoEncoder::Libx264),
                    ..Default::default()
                },
            );
            assert_eq!(forced.chosen(), Some(VideoEncoder::Libx264));
            let c = forced
                .candidates
                .iter()
                .find(|c| c.encoder == VideoEncoder::Libx264);
            assert_eq!(c.and_then(|c| c.usable), Some(true), "{forced:?}");
        }
    }
}
