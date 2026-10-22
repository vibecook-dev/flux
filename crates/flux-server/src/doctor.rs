//! `flux-server --check`: what this machine's ffmpeg and ffprobe can do for
//! flux (paths and versions, the H.264 encoder per codec, hardware
//! acceleration, tone mapping, what is missing), also served to owners and
//! admins as `GET /api/v1/admin/media-tools`. flux runs ffmpeg/ffprobe as
//! separate processes and never links libav*; see docs/ENCODERS.md.
//! `--check` also prints the data dir and library folders the configuration
//! resolves to ([`config_text`]).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use flux_hls::encoders::{DetectOptions, FfmpegCaps, VideoEncoder};
use serde::Serialize;

use crate::config::Config;

/// One external tool.
#[derive(Debug, Clone, Serialize)]
pub struct ToolInfo {
    pub path: PathBuf,
    /// First line of `-version`.
    pub version: Option<String>,
    /// Why it could not run.
    pub error: Option<String>,
}

impl ToolInfo {
    pub fn probe(path: &Path) -> ToolInfo {
        match flux_hls::ffmpeg_tools::version(path) {
            Ok(v) => ToolInfo {
                path: path.to_path_buf(),
                version: Some(v),
                error: None,
            },
            Err(e) => ToolInfo {
                path: path.to_path_buf(),
                version: None,
                error: Some(e),
            },
        }
    }

    pub fn ok(&self) -> bool {
        self.version.is_some()
    }
}

/// The encoder flux uses for one output codec.
#[derive(Debug, Clone, Serialize)]
pub struct CodecChoice {
    /// Output codec (`h264`; flux transcodes video to H.264 only).
    pub codec: &'static str,
    pub encoder: Option<VideoEncoder>,
    pub hardware: bool,
    /// How HDR is turned into SDR on this path, if it can be.
    pub tonemap: Option<String>,
}

/// What `--check` prints and the admin API returns.
#[derive(Debug, Clone, Serialize)]
pub struct ToolReport {
    /// `macos aarch64`, `linux x86_64`, …
    pub platform: String,
    pub ffmpeg: ToolInfo,
    pub ffprobe: ToolInfo,
    pub video: Vec<CodecChoice>,
    /// Audio transcodes (TrueHD, DTS, … → AAC) need ffmpeg's `aac` encoder.
    pub aac_encoder: bool,
    /// ffmpeg's hardware acceleration methods (`-hwaccels`).
    pub hwaccels: Vec<String>,
    /// The VAAPI render node, when present.
    pub vaapi_device: Option<PathBuf>,
    /// Every candidate encoder and what its probe found.
    pub candidates: Vec<flux_hls::encoders::Candidate>,
    /// The encoder is forced by configuration (`--video-encoder`).
    pub forced: Option<VideoEncoder>,
    /// Gaps, from fatal (no ffprobe) to cosmetic (no hardware encoder).
    pub missing: Vec<String>,
    /// ffmpeg and ffprobe both run (`--check` exits non-zero otherwise).
    pub ok: bool,
}

impl ToolReport {
    /// A report from detected capabilities and ffprobe.
    pub fn from_caps(caps: &FfmpegCaps, ffprobe: &Path) -> ToolReport {
        let ffmpeg = ToolInfo {
            path: caps.ffmpeg.clone(),
            version: caps.version.clone(),
            error: caps.error.clone(),
        };
        let ffprobe = ToolInfo::probe(ffprobe);
        let mut missing = Vec::new();
        if let Some(e) = &ffprobe.error {
            missing.push(format!(
                "ffprobe: {e}: AVI and some MPEG-TS files cannot be indexed, and the scan's \
                 fallback probe is off (install ffmpeg, or pass --ffprobe)"
            ));
        }
        if let Some(e) = &ffmpeg.error {
            missing.push(format!(
                "ffmpeg: {e}: no audio or video transcodes and no frame posters \
                 (install ffmpeg, or pass --ffmpeg)"
            ));
        } else {
            missing.extend(caps.missing());
        }
        let chosen = caps.chosen();
        ToolReport {
            platform: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
            ok: ffmpeg.ok() && ffprobe.ok(),
            ffmpeg,
            ffprobe,
            video: vec![CodecChoice {
                codec: "h264",
                encoder: chosen,
                hardware: chosen.is_some_and(|e| e.is_hardware()),
                tonemap: chosen.and_then(|e| caps.tonemap_for(e)),
            }],
            aac_encoder: caps.has_encoder("aac"),
            hwaccels: caps.hwaccels.iter().cloned().collect(),
            vaapi_device: caps.vaapi_device.clone(),
            candidates: caps.candidates.clone(),
            forced: caps.forced,
            missing,
        }
    }

    /// Probes everything (blocking: a few ffmpeg runs and test encodes).
    pub fn detect(
        ffmpeg: &Path,
        ffprobe: &Path,
        forced: Option<VideoEncoder>,
        vaapi_device: &Path,
    ) -> ToolReport {
        let caps = FfmpegCaps::detect(
            ffmpeg,
            &DetectOptions {
                forced,
                vaapi_device: vaapi_device.to_path_buf(),
                probe_all: true,
                ..Default::default()
            },
        );
        ToolReport::from_caps(&caps, ffprobe)
    }

    /// The human-readable form `--check` prints.
    pub fn text(&self) -> String {
        self.text_with("")
    }

    /// [`ToolReport::text`] with `extra` lines after the first.
    pub fn text_with(&self, extra: &str) -> String {
        let mut s = String::new();
        let tool = |s: &mut String, name: &str, t: &ToolInfo| {
            let _ = match (&t.version, &t.error) {
                (Some(v), _) => writeln!(s, "{name:<9}{}  ({v})", t.path.display()),
                (None, Some(e)) => writeln!(s, "{name:<9}{}  MISSING: {e}", t.path.display()),
                (None, None) => writeln!(s, "{name:<9}{}", t.path.display()),
            };
        };
        let _ = writeln!(
            s,
            "flux-server {} on {}",
            env!("CARGO_PKG_VERSION"),
            self.platform
        );
        s.push_str(extra);
        tool(&mut s, "ffmpeg", &self.ffmpeg);
        tool(&mut s, "ffprobe", &self.ffprobe);
        if self.ffmpeg.ok() {
            for v in &self.video {
                let _ = match v.encoder {
                    Some(e) => writeln!(
                        s,
                        "video    {} → {e} ({}{})",
                        v.codec,
                        if v.hardware { "hardware" } else { "software" },
                        if self.forced.is_some() { ", forced" } else { "" }
                    ),
                    None => writeln!(s, "video    {} → none usable", v.codec),
                };
                let _ = writeln!(
                    s,
                    "tonemap  HDR → SDR: {}",
                    v.tonemap.as_deref().unwrap_or("not available")
                );
            }
            let _ = writeln!(
                s,
                "audio    aac encoder: {}",
                if self.aac_encoder { "yes" } else { "MISSING" }
            );
            let _ = writeln!(
                s,
                "hwaccel  {}",
                if self.hwaccels.is_empty() {
                    "none".to_string()
                } else {
                    self.hwaccels.join(", ")
                }
            );
            if let Some(d) = &self.vaapi_device {
                let _ = writeln!(s, "vaapi    {}", d.display());
            }
            let _ = writeln!(s, "encoders, in the order flux tries them:");
            for c in &self.candidates {
                let mark = match c.usable {
                    Some(true) => "ok  ",
                    Some(false) => "no  ",
                    None => "--  ",
                };
                let _ = writeln!(s, "  {mark}{:<18} {}", c.encoder.ffmpeg_name(), c.note);
            }
        }
        if self.missing.is_empty() {
            let _ = writeln!(s, "nothing missing");
        } else {
            let _ = writeln!(s, "missing:");
            for m in &self.missing {
                let _ = writeln!(s, "  - {m}");
            }
        }
        s
    }
}

/// The paths a configuration resolves to and whether they exist, one per
/// line in `--check`'s layout. A path the TOML file mangled, or a library
/// folder that isn't there, shows up here before the server runs.
pub fn config_text(cfg: &Config) -> String {
    let mut s = String::new();
    let dir_state = |p: &Path, missing: &'static str| {
        if p.is_dir() {
            "found"
        } else if p.exists() {
            "NOT A FOLDER"
        } else {
            missing
        }
    };
    let _ = writeln!(
        s,
        "data     {}  ({})",
        cfg.data_dir.display(),
        dir_state(&cfg.data_dir, "created at start")
    );
    if let Some(older) = &cfg.older_data_dir {
        let _ = writeln!(
            s,
            "older    {}  (a catalog from an earlier flux: --data-dir to use it)",
            older.display()
        );
    }
    for l in &cfg.libraries {
        let path = Path::new(&l.path);
        let _ = writeln!(
            s,
            "library  {} ({}): {}  ({})",
            l.name,
            l.kind.as_str(),
            path.display(),
            dir_state(path, "NOT FOUND")
        );
    }
    let _ = match &cfg.web_dir {
        Some(d) if d.join("index.html").is_file() => writeln!(s, "web      {}  (found)", d.display()),
        Some(d) => writeln!(s, "web      {}  (NO index.html)", d.display()),
        None => writeln!(s, "web      none: API only (--web-dir)"),
    };
    s
}

#[cfg(test)]
mod tests {
    use flux_core::LibraryKind;

    use super::*;

    /// `--check` showed only the tools: a mangled data dir or library path
    /// (TOML escapes, a typo) went unnoticed until the server ran.
    #[test]
    fn check_lists_the_resolved_paths_and_whether_they_exist() {
        let dir = tempfile::tempdir().unwrap();
        let movies = dir.path().join("Movies");
        std::fs::create_dir(&movies).unwrap();
        let mut cfg = Config::for_tests(dir.path().join("data"));
        cfg.libraries = vec![
            crate::config::LibrarySpec {
                name: "Movies".into(),
                kind: LibraryKind::Movies,
                path: movies.to_string_lossy().into_owned(),
            },
            crate::config::LibrarySpec {
                name: "TV".into(),
                kind: LibraryKind::Shows,
                path: dir.path().join("TV").to_string_lossy().into_owned(),
            },
        ];
        let t = config_text(&cfg);
        let data = cfg.data_dir.display().to_string();
        assert!(t.contains(&format!("data     {data}  (created at start)")), "{t}");
        assert!(
            t.contains(&format!(
                "library  Movies (movies): {}  (found)",
                movies.display()
            )),
            "{t}"
        );
        assert!(
            t.contains("library  TV (shows): ") && t.contains("TV  (NOT FOUND)"),
            "{t}"
        );
        assert!(t.contains("web      none"), "{t}");
        assert!(!t.lines().any(|l| l.starts_with("older ")), "{t}");
        let older = dir.path().join("flux-data");
        cfg.older_data_dir = Some(older.clone());
        assert!(
            config_text(&cfg).contains(&format!(
                "older    {}  (a catalog from an earlier flux",
                older.display()
            )),
            "{t}"
        );
        let report = ToolReport::detect(
            Path::new("/nonexistent/ffmpeg"),
            Path::new("/nonexistent/ffprobe"),
            None,
            Path::new("/nonexistent/renderD128"),
        );
        let full = report.text_with(&t);
        let mut lines = full.lines();
        assert!(lines.next().unwrap().starts_with("flux-server "), "{full}");
        assert!(lines.next().unwrap().starts_with("data     "), "{full}");
    }

    #[test]
    fn missing_tools_fail_the_check() {
        let r = ToolReport::detect(
            Path::new("/nonexistent/ffmpeg"),
            Path::new("/nonexistent/ffprobe"),
            None,
            Path::new("/nonexistent/renderD128"),
        );
        assert!(!r.ok);
        assert!(
            r.missing.iter().any(|m| m.starts_with("ffprobe:")),
            "{:?}",
            r.missing
        );
        assert!(
            r.missing.iter().any(|m| m.starts_with("ffmpeg:")),
            "{:?}",
            r.missing
        );
        let t = r.text();
        assert!(t.contains("MISSING"), "{t}");
    }

    #[test]
    fn this_machine_reports_an_encoder_when_ffmpeg_is_there() {
        let ffmpeg = flux_hls::ffmpeg_tools::default_ffmpeg();
        if !ffmpeg.is_file() {
            eprintln!("skipping: no ffmpeg");
            return;
        }
        let ffprobe = flux_hls::ffmpeg_tools::default_ffprobe(&ffmpeg);
        let r = ToolReport::detect(&ffmpeg, &ffprobe, None, Path::new("/dev/dri/renderD128"));
        let t = r.text();
        eprintln!("{t}");
        assert!(r.ffmpeg.ok(), "{t}");
        assert_eq!(r.candidates.len(), VideoEncoder::ORDER.len());
        let json = serde_json::to_value(&r).unwrap();
        assert!(json["video"][0]["codec"] == "h264", "{json}");
    }
}
