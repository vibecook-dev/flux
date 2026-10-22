//! Configuration: command-line flags > environment variables > TOML file >
//! defaults. `flux-server --help` lists every flag with its variable.
//!
//! TOML keys are the long flag names with `_` (`data_dir`, `port`, `lan`,
//! `tailnet`, `web_dir`, …) plus `[[library]]` tables:
//!
//! ```toml
//! data_dir = "/srv/flux"
//! port = 8096
//! tailnet = "off"            # off | embedded | system
//! allow_host = ["media.example.com"]
//! watch = true
//! [[library]]
//! name = "Movies"
//! kind = "movies"            # movies | shows | mixed
//! path = "/srv/media/Movies"
//! video_encoder = "auto"     # auto | videotoolbox | nvenc | qsv | vaapi | libx264
//! max_video_transcodes = 0   # concurrent video transcodes; 0 = by encoder
//! ```
//!
//! Windows paths go in 'single quotes' (TOML literal strings) or use forward
//! slashes: in "double quotes" a backslash starts an escape, so
//! `"D:\new\tv"` would hold a line feed and a tab. Paths from the file with
//! control characters are refused for that reason.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
use clap::builder::BoolishValueParser;
use clap::{ArgAction, Parser, ValueEnum};
use flux_core::api::Role;
use flux_core::LibraryKind;
use flux_hls::encoders::VideoEncoder;
use serde::Deserialize;

/// A library to create on first run: `NAME:KIND:PATH`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LibrarySpec {
    pub name: String,
    #[serde(deserialize_with = "de_kind")]
    pub kind: LibraryKind,
    pub path: String,
}

fn parse_kind(s: &str) -> Result<LibraryKind, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "movies" | "movie" | "films" => Ok(LibraryKind::Movies),
        "shows" | "show" | "tv" | "series" => Ok(LibraryKind::Shows),
        "mixed" => Ok(LibraryKind::Mixed),
        other => Err(format!("unknown library kind {other:?} (movies, shows or mixed)")),
    }
}

fn de_kind<'de, D: serde::Deserializer<'de>>(d: D) -> Result<LibraryKind, D::Error> {
    let s = String::deserialize(d)?;
    parse_kind(&s).map_err(serde::de::Error::custom)
}

impl std::str::FromStr for LibrarySpec {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let mut parts = s.splitn(3, ':');
        let (Some(name), Some(kind), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(format!("expected NAME:KIND:PATH, got {s:?}"));
        };
        if name.trim().is_empty() || path.trim().is_empty() {
            return Err(format!("expected NAME:KIND:PATH, got {s:?}"));
        }
        Ok(LibrarySpec {
            name: name.trim().to_string(),
            kind: parse_kind(kind)?,
            path: path.trim().to_string(),
        })
    }
}

/// How flux reaches the tailnet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TailnetChoice {
    /// No tailnet listener (default).
    #[default]
    Off,
    /// flux's own node through Truffle ("Connect with Tailscale").
    Embedded,
    /// This machine's tailscaled.
    System,
}

impl TailnetChoice {
    pub fn as_str(self) -> &'static str {
        match self {
            TailnetChoice::Off => "off",
            TailnetChoice::Embedded => "embedded",
            TailnetChoice::System => "system",
        }
    }

    pub fn from_api(m: flux_core::api::TailnetMode) -> Self {
        match m {
            flux_core::api::TailnetMode::Off => TailnetChoice::Off,
            flux_core::api::TailnetMode::Embedded => TailnetChoice::Embedded,
            flux_core::api::TailnetMode::System => TailnetChoice::System,
        }
    }

    pub fn to_api(self) -> flux_core::api::TailnetMode {
        match self {
            TailnetChoice::Off => flux_core::api::TailnetMode::Off,
            TailnetChoice::Embedded => flux_core::api::TailnetMode::Embedded,
            TailnetChoice::System => flux_core::api::TailnetMode::System,
        }
    }
}

/// The role of callers on this machine's loopback interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalRole {
    /// The server's owner (default: a personal machine).
    #[default]
    Owner,
    Admin,
    Viewer,
    Kid,
    Guest,
    /// No identity, like a LAN caller (shared or multi-user machines).
    None,
}

impl LocalRole {
    pub fn to_role(self) -> Option<Role> {
        match self {
            LocalRole::Owner => Some(Role::Owner),
            LocalRole::Admin => Some(Role::Admin),
            LocalRole::Viewer => Some(Role::Viewer),
            LocalRole::Kid => Some(Role::Kid),
            LocalRole::Guest => Some(Role::Guest),
            LocalRole::None => None,
        }
    }
}

/// Command line. Every flag can also come from the environment variable
/// named in `--help`, or from the TOML file.
#[derive(Debug, Default, Parser)]
#[command(
    name = "flux-server",
    version,
    about = "flux media server: browse, play and stream your movie and TV library",
    after_help = "Examples:\n  flux-server --library Movies:movies:/srv/media/Movies --library TV:shows:/srv/media/TV\n  flux-server --config /etc/flux.toml\n  flux-server --check\n\nTOML keys are the long flags with '_' plus [[library]] tables {name, kind, path}.\nOn Windows, write paths in 'single quotes' or with forward slashes."
)]
pub struct Cli {
    /// TOML config file.
    #[arg(long, env = "FLUX_CONFIG", value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Where flux keeps its catalog, caches and artwork [default:
    /// ~/Library/Application Support/flux on macOS, %LOCALAPPDATA%\flux on
    /// Windows, $XDG_DATA_HOME/flux or ~/.local/share/flux elsewhere].
    #[arg(long, env = "FLUX_DATA_DIR", value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
    /// A library to create on first run (repeatable; ';'-separated in the
    /// variable). KIND is movies, shows or mixed.
    #[arg(
        long = "library",
        env = "FLUX_LIBRARIES",
        value_name = "NAME:KIND:PATH",
        value_delimiter = ';'
    )]
    pub libraries: Vec<LibrarySpec>,
    /// Local port [default: 8096].
    #[arg(long, env = "FLUX_PORT")]
    pub port: Option<u16>,
    /// Local listener address [default: 127.0.0.1]. Callers on it are the
    /// Owner; use --lan for other machines.
    #[arg(long, env = "FLUX_BIND", value_name = "IP")]
    pub bind: Option<IpAddr>,
    /// Also listen on the LAN, e.g. 0.0.0.0:8097. LAN callers are anonymous.
    #[arg(long, env = "FLUX_LAN", value_name = "ADDR:PORT")]
    pub lan: Option<SocketAddr>,
    /// Remote access over Tailscale [default: off].
    #[arg(long, env = "FLUX_TAILNET", value_enum)]
    pub tailnet: Option<TailnetChoice>,
    /// The built web client (web/dist) [default: ./web/dist or next to the binary].
    #[arg(long, env = "FLUX_WEB_DIR", value_name = "DIR")]
    pub web_dir: Option<PathBuf>,
    /// Server name shown to clients and used for the tailnet device [default: host name].
    #[arg(long, env = "FLUX_NAME")]
    pub name: Option<String>,
    /// Don't scan libraries (at startup or on request).
    #[arg(long, env = "FLUX_NO_SCAN", action = ArgAction::SetTrue, value_parser = BoolishValueParser::new())]
    pub no_scan: bool,
    /// Don't run background jobs (matching, frame posters).
    #[arg(long, env = "FLUX_NO_JOBS", action = ArgAction::SetTrue, value_parser = BoolishValueParser::new())]
    pub no_jobs: bool,
    /// Don't watch library folders for changes: scan at startup and when
    /// asked ("Scan now") only.
    #[arg(long, env = "FLUX_NO_WATCH", action = ArgAction::SetTrue, value_parser = BoolishValueParser::new())]
    pub no_watch: bool,
    /// Also answer to these host names or addresses (repeatable;
    /// comma-separated in the variable), e.g. a reverse proxy's name.
    /// localhost, this machine's name and addresses and the tailnet's name
    /// are always allowed; requests naming any other host get 421.
    #[arg(
        long = "allow-host",
        env = "FLUX_ALLOW_HOSTS",
        value_name = "HOST",
        value_delimiter = ','
    )]
    pub allow_hosts: Vec<String>,
    /// Role of callers on this machine (loopback) [default: owner]. Use
    /// `none` on a machine other people can log in to.
    #[arg(long, env = "FLUX_LOCALHOST_ROLE", value_enum)]
    pub localhost_role: Option<LocalRole>,
    /// The tailnet login that owns this server [default: the login that
    /// owns its tailnet node, learned when the tailnet starts].
    #[arg(long, env = "FLUX_OWNER_LOGIN", value_name = "LOGIN")]
    pub owner_login: Option<String>,
    /// ffmpeg binary [default: from PATH or Homebrew].
    #[arg(long, env = "FLUX_FFMPEG", value_name = "PATH")]
    pub ffmpeg: Option<PathBuf>,
    /// ffprobe binary [default: next to ffmpeg].
    #[arg(long, env = "FLUX_FFPROBE", value_name = "PATH")]
    pub ffprobe: Option<PathBuf>,
    /// H.264 encoder of video transcodes: auto (detect: VideoToolbox, NVENC,
    /// QSV, VAAPI, then libx264), videotoolbox, nvenc, qsv, vaapi or libx264
    /// [default: auto].
    #[arg(long, env = "FLUX_VIDEO_ENCODER", value_name = "ENCODER", value_parser = parse_encoder)]
    pub video_encoder: Option<EncoderChoice>,
    /// VAAPI render node [default: /dev/dri/renderD128].
    #[arg(long, env = "FLUX_VAAPI_DEVICE", value_name = "PATH")]
    pub vaapi_device: Option<PathBuf>,
    /// Video transcodes encoding at once (ffmpeg runs, pieces encoded ahead
    /// included); 0 picks by encoder: 6 on hardware, 3 for libx264
    /// [default: 0].
    #[arg(long, env = "FLUX_MAX_VIDEO_TRANSCODES", value_name = "N")]
    pub max_video_transcodes: Option<usize>,
    /// Print what ffmpeg and ffprobe can do for flux (paths, versions, the
    /// encoder per codec, hardware acceleration, tone mapping, what is
    /// missing) and exit: non-zero when ffmpeg or ffprobe is missing.
    #[arg(long, alias = "doctor")]
    pub check: bool,
    /// Directory with TMDB daily ID exports (`movie_ids_*.json.gz`) for
    /// offline matching [default: <data_dir>/tmdb-exports].
    #[arg(long, env = "FLUX_TMDB_EXPORTS", value_name = "DIR")]
    pub tmdb_exports: Option<PathBuf>,
    /// Log filter (tracing `EnvFilter` syntax) [default: info, or RUST_LOG].
    #[arg(long, env = "FLUX_LOG")]
    pub log: Option<String>,
}

/// The TOML file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    data_dir: Option<PathBuf>,
    #[serde(default, rename = "library")]
    libraries: Vec<LibrarySpec>,
    port: Option<u16>,
    bind: Option<IpAddr>,
    lan: Option<SocketAddr>,
    tailnet: Option<TailnetChoice>,
    web_dir: Option<PathBuf>,
    name: Option<String>,
    scan: Option<bool>,
    jobs: Option<bool>,
    watch: Option<bool>,
    allow_host: Option<Vec<String>>,
    localhost_role: Option<LocalRole>,
    owner_login: Option<String>,
    ffmpeg: Option<PathBuf>,
    ffprobe: Option<PathBuf>,
    #[serde(default, deserialize_with = "de_encoder")]
    video_encoder: Option<EncoderChoice>,
    vaapi_device: Option<PathBuf>,
    max_video_transcodes: Option<usize>,
    tmdb_exports: Option<PathBuf>,
    log: Option<String>,
}

impl FileConfig {
    /// Refuses paths with control characters. In a TOML string in "double
    /// quotes" a backslash starts an escape, so a Windows path such as
    /// `"D:\\media\new\tv"` parses into a line feed and a tab, silently.
    fn check_paths(&self) -> anyhow::Result<()> {
        let named = [
            ("data_dir", &self.data_dir),
            ("web_dir", &self.web_dir),
            ("ffmpeg", &self.ffmpeg),
            ("ffprobe", &self.ffprobe),
            ("vaapi_device", &self.vaapi_device),
            ("tmdb_exports", &self.tmdb_exports),
        ];
        let mut paths: Vec<(String, std::borrow::Cow<'_, str>)> = named
            .into_iter()
            .filter_map(|(key, p)| Some((key.to_string(), p.as_deref()?.to_string_lossy())))
            .collect();
        for l in &self.libraries {
            paths.push((format!("library {:?}: path", l.name), l.path.as_str().into()));
        }
        for (key, value) in paths {
            if value.chars().any(char::is_control) {
                bail!(
                    "{key} \"{}\" contains control characters. In \"double quotes\" a backslash \
                     starts an escape (\\n, \\t, \\f, \\b, \\r); write Windows paths in 'single \
                     quotes' or with forward slashes: path = 'D:\\Media\\TV' or \"D:/Media/TV\"",
                    show_controls(&value)
                );
            }
        }
        Ok(())
    }
}

/// `s` with each control character shown as `<TAB>`, `<LF>`, … or `<U+XXXX>`.
fn show_controls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\t' => out.push_str("<TAB>"),
            '\n' => out.push_str("<LF>"),
            '\r' => out.push_str("<CR>"),
            '\u{8}' => out.push_str("<BS>"),
            '\u{c}' => out.push_str("<FF>"),
            c if c.is_control() => out.push_str(&format!("<U+{:04X}>", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// `--video-encoder`: `auto` or one encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderChoice(pub Option<VideoEncoder>);

fn parse_encoder(s: &str) -> Result<EncoderChoice, String> {
    VideoEncoder::parse_choice(s).map(EncoderChoice)
}

fn de_encoder<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<EncoderChoice>, D::Error> {
    let s = String::deserialize(d)?;
    parse_encoder(&s).map(Some).map_err(serde::de::Error::custom)
}

/// Resolved configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    /// A folder that an earlier flux used as its default data dir, still
    /// holding a catalog, while this run uses the default (Windows only).
    /// Logged at startup and shown by `--check`.
    pub older_data_dir: Option<PathBuf>,
    pub libraries: Vec<LibrarySpec>,
    /// Local listener (`127.0.0.1:8096`; port 0 = any, for tests).
    pub local_addr: SocketAddr,
    pub lan_addr: Option<SocketAddr>,
    pub tailnet: TailnetChoice,
    /// The tailnet mode came from the command line or the environment; a
    /// mode chosen in the UI (persisted in the data dir) does not override it.
    pub tailnet_explicit: bool,
    pub web_dir: Option<PathBuf>,
    pub name: String,
    pub scan: bool,
    pub jobs: bool,
    /// Watch library folders for changes (`--no-watch` turns it off).
    pub watch: bool,
    /// Extra host names/addresses the server answers to (`--allow-host`).
    pub allow_hosts: Vec<String>,
    /// Role of loopback callers (`None`: anonymous, like the LAN).
    pub localhost_role: Option<Role>,
    /// The owner's tailnet login, pinned (`None`: learned from the tailnet).
    pub owner_login: Option<String>,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    /// The H.264 encoder of video transcodes (`None`: detect).
    pub video_encoder: Option<VideoEncoder>,
    /// The VAAPI render node.
    pub vaapi_device: PathBuf,
    /// Concurrent video transcode runs (0: by encoder).
    pub max_video_transcodes: usize,
    /// Detect the video encoder in the background at startup (else on the
    /// first transcode).
    pub detect_encoder_at_start: bool,
    pub tmdb_exports: PathBuf,
    pub log: String,
    /// Delay before background jobs start (lets startup scans go first).
    pub jobs_delay: Duration,
    /// Probe this machine's tailscaled once (read-only `tailscale status`)
    /// to fill `TailnetInfo::system_tailscale`. Not when `--tailnet off` (or
    /// `FLUX_TAILNET=off`) is explicit: then flux runs no `tailscale`
    /// command at all, unless the UI switches remote access on.
    pub probe_system_tailscale: bool,
    /// Playback idle this long (no media request): the playback caches are
    /// dropped and their memory returned ([`crate::play::trim_when_idle`]).
    pub cache_idle: Duration,
}

impl Config {
    /// Merge flags/env (already in `cli`) with the TOML file and defaults.
    pub fn resolve(cli: Cli) -> anyhow::Result<Config> {
        let file: FileConfig = match &cli.config {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("reading config {}", path.display()))?;
                let file: FileConfig =
                    toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
                file.check_paths()
                    .with_context(|| format!("config {}", path.display()))?;
                file
            }
            None => FileConfig::default(),
        };
        let (data_dir, older_data_dir) = match cli.data_dir.or(file.data_dir) {
            Some(d) => (expand_home(d), None),
            None => {
                let d = default_data_dir()?;
                let older = older_data_dir(&d, older_default_data_dirs());
                (d, older)
            }
        };
        // Logged at startup and shown by --check as the folder flux uses,
        // whatever the working directory was.
        let data_dir = std::path::absolute(&data_dir).unwrap_or(data_dir);
        let mut libraries = file.libraries;
        for l in cli.libraries {
            if !libraries.iter().any(|x| x.name == l.name) {
                libraries.push(l);
            }
        }
        for l in &mut libraries {
            l.path = expand_home(PathBuf::from(&l.path)).to_string_lossy().into_owned();
            if !Path::new(&l.path).is_absolute() {
                bail!("library {:?}: path {:?} must be absolute", l.name, l.path);
            }
        }
        let port = cli.port.or(file.port).unwrap_or(flux_net::DEFAULT_PORT);
        let bind = cli.bind.or(file.bind).unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let tailnet_explicit = cli.tailnet.is_some();
        let tailnet = cli.tailnet.or(file.tailnet).unwrap_or_default();
        let ffmpeg = cli.ffmpeg.or(file.ffmpeg).unwrap_or_else(|| find_tool("ffmpeg"));
        let ffprobe = cli.ffprobe.or(file.ffprobe).unwrap_or_else(|| {
            let sibling = ffmpeg.with_file_name(format!("ffprobe{}", std::env::consts::EXE_SUFFIX));
            if sibling.is_file() {
                sibling
            } else {
                find_tool("ffprobe")
            }
        });
        let video_encoder = cli.video_encoder.or(file.video_encoder).and_then(|c| c.0);
        let vaapi_device = cli
            .vaapi_device
            .or(file.vaapi_device)
            .unwrap_or_else(|| PathBuf::from(flux_hls::encoders::DEFAULT_VAAPI_DEVICE));
        let web_dir = cli
            .web_dir
            .or(file.web_dir)
            .map(expand_home)
            .or_else(default_web_dir);
        let tmdb_exports = cli
            .tmdb_exports
            .or(file.tmdb_exports)
            .map(expand_home)
            .unwrap_or_else(|| data_dir.join("tmdb-exports"));
        let mut allow_hosts = file.allow_host.unwrap_or_default();
        allow_hosts.extend(cli.allow_hosts);
        allow_hosts.retain(|h| !h.trim().is_empty());
        let owner_login = cli
            .owner_login
            .or(file.owner_login)
            .filter(|l| !l.trim().is_empty());
        Ok(Config {
            libraries,
            local_addr: SocketAddr::new(bind, port),
            lan_addr: cli.lan.or(file.lan),
            tailnet,
            tailnet_explicit,
            web_dir,
            name: cli.name.or(file.name).unwrap_or_else(host_name),
            scan: !cli.no_scan && file.scan.unwrap_or(true),
            jobs: !cli.no_jobs && file.jobs.unwrap_or(true),
            watch: !cli.no_watch && file.watch.unwrap_or(true),
            allow_hosts,
            localhost_role: cli
                .localhost_role
                .or(file.localhost_role)
                .unwrap_or_default()
                .to_role(),
            owner_login,
            ffmpeg,
            ffprobe,
            video_encoder,
            vaapi_device,
            max_video_transcodes: cli
                .max_video_transcodes
                .or(file.max_video_transcodes)
                .unwrap_or(0),
            detect_encoder_at_start: true,
            tmdb_exports,
            log: cli
                .log
                .or(file.log)
                .or_else(|| std::env::var("RUST_LOG").ok())
                .unwrap_or_else(|| "info".into()),
            jobs_delay: Duration::from_secs(10),
            probe_system_tailscale: !(tailnet_explicit && tailnet == TailnetChoice::Off),
            cache_idle: crate::play::CACHE_IDLE,
            data_dir,
            older_data_dir,
        })
    }

    /// A configuration for tests: everything under `data_dir`, an ephemeral
    /// port on 127.0.0.1, no scanning, no jobs, no tailnet, no web UI.
    pub fn for_tests(data_dir: impl Into<PathBuf>) -> Config {
        let data_dir = data_dir.into();
        Config {
            libraries: Vec::new(),
            local_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            lan_addr: None,
            tailnet: TailnetChoice::Off,
            tailnet_explicit: true,
            web_dir: None,
            name: "flux-test".into(),
            scan: false,
            jobs: false,
            watch: false,
            allow_hosts: Vec::new(),
            localhost_role: Some(Role::Owner),
            owner_login: None,
            ffmpeg: find_tool("ffmpeg"),
            ffprobe: find_tool("ffprobe"),
            video_encoder: VideoEncoder::from_env(),
            vaapi_device: flux_hls::encoders::vaapi_device_from_env(),
            max_video_transcodes: 0,
            detect_encoder_at_start: false,
            tmdb_exports: data_dir.join("tmdb-exports"),
            log: "warn".into(),
            jobs_delay: Duration::ZERO,
            probe_system_tailscale: false,
            cache_idle: crate::play::CACHE_IDLE,
            data_dir,
            older_data_dir: None,
        }
    }

    pub fn catalog_path(&self) -> PathBuf {
        self.data_dir.join("catalog.db")
    }
}

/// `~/x` → `$HOME/x` (on Windows `%USERPROFILE%` when `HOME` is unset).
fn expand_home(p: PathBuf) -> PathBuf {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| cfg!(windows).then(|| std::env::var_os("USERPROFILE")).flatten());
    match (p.strip_prefix("~"), home) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => p,
    }
}

/// `~/Library/Application Support/flux` (macOS), `%LOCALAPPDATA%\flux`
/// (Windows), `$XDG_DATA_HOME/flux` or `~/.local/share/flux` (elsewhere).
pub fn default_data_dir() -> anyhow::Result<PathBuf> {
    #[cfg(windows)]
    {
        // The known folder, not HOME: OpenSSH and Git shells set HOME to the
        // profile folder, where a Unix-style `.local/share` doesn't belong.
        crate::win32::local_app_data()
            .map(|d| d.join("flux"))
            .context("finding %LOCALAPPDATA% for the data dir; pass --data-dir")
    }
    #[cfg(not(windows))]
    Ok(unix_data_dir(
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from),
        std::env::var_os("XDG_DATA_HOME")
            .filter(|x| !x.is_empty())
            .map(PathBuf::from),
        cfg!(target_os = "macos"),
    ))
}

/// The folders flux up to 0.1.0-alpha.2 used as its default data dir on
/// Windows, where the default is now `%LOCALAPPDATA%\flux`. Which one it
/// used depended on the environment it ran in (see [`older_windows_data_dirs`]).
/// None elsewhere: the default there hasn't moved.
fn older_default_data_dirs() -> Vec<PathBuf> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let var = |name| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    older_windows_data_dirs(var("HOME"), var("XDG_DATA_HOME"), std::env::current_dir().ok())
}

/// `$XDG_DATA_HOME\flux`, `%HOME%\.local\share\flux` (OpenSSH and Git
/// shells set `HOME`) and `flux-data` in the working directory: the old
/// Windows defaults, in the order they applied.
fn older_windows_data_dirs(
    home: Option<PathBuf>,
    xdg_data_home: Option<PathBuf>,
    cwd: Option<PathBuf>,
) -> Vec<PathBuf> {
    [
        xdg_data_home.map(|x| x.join("flux")),
        home.map(|h| h.join(".local").join("share").join("flux")),
        cwd.map(|c| c.join("flux-data")),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// The first of `older`, other than `data_dir`, that holds a catalog: after
/// the default moved, flux would otherwise start on an empty catalog while
/// the old one (libraries, watched and resume state) sat there unnoticed.
/// Found whether or not `data_dir` has a catalog of its own yet, so the
/// warning stays until the old folder is moved, deleted or named with
/// `--data-dir`.
fn older_data_dir(data_dir: &Path, older: Vec<PathBuf>) -> Option<PathBuf> {
    older
        .into_iter()
        .find(|d| d != data_dir && d.join("catalog.db").is_file())
}

/// [`default_data_dir`] off Windows, from `HOME` and `XDG_DATA_HOME`;
/// without either, `flux-data` in the working directory (which
/// [`Config::resolve`] makes absolute).
#[cfg_attr(windows, allow(dead_code))]
fn unix_data_dir(home: Option<PathBuf>, xdg_data_home: Option<PathBuf>, macos: bool) -> PathBuf {
    match (home, xdg_data_home) {
        (Some(h), _) if macos => h.join("Library/Application Support/flux"),
        (_, Some(x)) => x.join("flux"),
        (Some(h), None) => h.join(".local/share/flux"),
        (None, None) => PathBuf::from("flux-data"),
    }
}

/// `./web/dist`, then `<exe dir>/web`, then `<exe dir>/../share/flux/web`.
/// The exe dir is the real binary's: on macOS `current_exe` is the path the
/// binary was started by, e.g. a `/usr/local/bin/flux-server` symlink to
/// `/opt/flux/bin/flux-server` (docs/INSTALL.md).
fn default_web_dir() -> Option<PathBuf> {
    let mut candidates = vec![PathBuf::from("web/dist")];
    if let Some(dir) = std::env::current_exe()
        .ok()
        .map(|e| e.canonicalize().unwrap_or(e))
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        candidates.push(dir.join("web"));
        candidates.push(dir.join("../share/flux/web"));
    }
    candidates
        .into_iter()
        .find(|c| c.join("index.html").is_file())
        .map(|c| c.canonicalize().unwrap_or(c))
}

/// A tool from `PATH`, else the usual Homebrew/system locations, else the
/// bare name (with the platform's executable suffix).
pub fn find_tool(name: &str) -> PathBuf {
    flux_hls::ffmpeg_tools::find_or_name(name)
}

/// The machine's host name without `.local`.
pub fn host_name() -> String {
    let name = system_host_name();
    let name = name.trim_end_matches(".local").to_string();
    if name.is_empty() {
        "flux".into()
    } else {
        name
    }
}

#[cfg(unix)]
fn system_host_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `len` bytes into the buffer we own.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    } else {
        String::new()
    }
}

/// Not Unix (Windows): the computer name from the environment.
#[cfg(not(unix))]
fn system_host_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

/// Where the tailnet mode chosen in the UI is remembered.
pub fn tailnet_choice_path(data_dir: &Path) -> PathBuf {
    data_dir.join("tailnet-mode")
}

/// The tailnet mode last chosen in the UI, if any.
pub fn persisted_tailnet(data_dir: &Path) -> Option<TailnetChoice> {
    let s = std::fs::read_to_string(tailnet_choice_path(data_dir)).ok()?;
    TailnetChoice::from_str(s.trim(), true).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_specs_parse() {
        let l: LibrarySpec = "Movies:movies:/srv/media/Movies".parse().unwrap();
        assert_eq!(l.kind, LibraryKind::Movies);
        assert_eq!(l.path, "/srv/media/Movies");
        let l: LibrarySpec = "TV:tv:/a:b/c".parse().unwrap();
        assert_eq!((l.kind, l.path.as_str()), (LibraryKind::Shows, "/a:b/c"));
        assert!("Movies:/x".parse::<LibrarySpec>().is_err());
        assert!("Movies:films2:/x".parse::<LibrarySpec>().is_err());
    }

    #[test]
    fn flags_beat_the_file_and_defaults_apply() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("flux.toml");
        // Absolute on every platform ("/m" has no drive letter on Windows);
        // a TOML literal string keeps Windows backslashes as they are.
        std::fs::write(
            &file,
            format!(
                r#"
port = 9000
tailnet = "system"
jobs = false
allow_host = ["nas.example.com"]
localhost_role = "none"
[[library]]
name = "Movies"
kind = "movies"
path = '{}'
"#,
                dir.path().join("m").display()
            ),
        )
        .unwrap();
        let tv = format!("TV:shows:{}", dir.path().join("tv").display());
        let cli = Cli::try_parse_from([
            "flux-server",
            "--config",
            file.to_str().unwrap(),
            "--port",
            "9100",
            "--library",
            tv.as_str(),
            "--data-dir",
            dir.path().to_str().unwrap(),
            "--allow-host",
            "media.example.com,10.0.0.5",
            "--no-watch",
        ])
        .unwrap();
        let cfg = Config::resolve(cli).unwrap();
        assert_eq!(
            cfg.allow_hosts,
            ["nas.example.com", "media.example.com", "10.0.0.5"]
        );
        assert!(!cfg.watch);
        assert_eq!(cfg.localhost_role, None, "from the file");
        assert_eq!(cfg.owner_login, None);
        assert_eq!(cfg.local_addr.port(), 9100);
        assert_eq!(cfg.local_addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(cfg.tailnet, TailnetChoice::System);
        assert!(!cfg.tailnet_explicit);
        assert!(!cfg.jobs && cfg.scan);
        let names: Vec<&str> = cfg.libraries.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["Movies", "TV"]);
        assert_eq!(cfg.tmdb_exports, dir.path().join("tmdb-exports"));
    }

    #[test]
    fn watching_and_localhost_owner_are_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli::try_parse_from(["flux-server", "--data-dir", dir.path().to_str().unwrap()]).unwrap();
        let cfg = Config::resolve(cli).unwrap();
        assert!(cfg.watch && cfg.allow_hosts.is_empty());
        assert_eq!(cfg.localhost_role, Some(Role::Owner));
        let cli = Cli::try_parse_from([
            "flux-server",
            "--data-dir",
            dir.path().to_str().unwrap(),
            "--localhost-role",
            "viewer",
            "--owner-login",
            "me@example.com",
        ])
        .unwrap();
        let cfg = Config::resolve(cli).unwrap();
        assert_eq!(cfg.localhost_role, Some(Role::Viewer));
        assert_eq!(cfg.owner_login.as_deref(), Some("me@example.com"));
    }

    #[test]
    fn unknown_toml_keys_are_rejected() {
        assert!(toml::from_str::<FileConfig>("prot = 1").is_err());
    }

    /// Windows used `%HOME%\.local/share/flux` (OpenSSH sets HOME) or, with
    /// no HOME, `flux-data` in whatever the working directory was.
    #[test]
    fn the_data_dir_is_per_user_and_absolute() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            unix_data_dir(home.clone(), None, true),
            Path::new("/home/u/Library/Application Support/flux")
        );
        assert_eq!(
            unix_data_dir(home.clone(), None, false),
            Path::new("/home/u/.local/share/flux")
        );
        assert_eq!(
            unix_data_dir(home, Some("/x".into()), false),
            Path::new("/x/flux")
        );
        assert_eq!(unix_data_dir(None, None, false), Path::new("flux-data"));
        #[cfg(windows)]
        {
            let local = crate::win32::local_app_data().unwrap();
            assert_eq!(default_data_dir().unwrap(), local.join("flux"));
        }
        let cfg =
            Config::resolve(Cli::try_parse_from(["flux-server", "--data-dir", "rel-data"]).unwrap()).unwrap();
        assert!(cfg.data_dir.is_absolute(), "{}", cfg.data_dir.display());
        assert!(cfg.data_dir.ends_with("rel-data"));
        assert_eq!(cfg.tmdb_exports, cfg.data_dir.join("tmdb-exports"));
    }

    /// The Windows default moved to `%LOCALAPPDATA%\flux`; a catalog in an
    /// old default folder (watched, resume) silently looked lost.
    #[test]
    fn an_older_windows_data_dir_with_a_catalog_is_found() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, xdg, cwd) = (
            tmp.path().join("home"),
            tmp.path().join("xdg"),
            tmp.path().join("cwd"),
        );
        let older = older_windows_data_dirs(Some(home.clone()), Some(xdg.clone()), Some(cwd.clone()));
        let old_home = home.join(".local").join("share").join("flux");
        assert_eq!(older, [xdg.join("flux"), old_home.clone(), cwd.join("flux-data")]);
        assert_eq!(older_windows_data_dirs(None, None, None), Vec::<PathBuf>::new());

        let new = tmp.path().join("local").join("flux");
        assert_eq!(older_data_dir(&new, older.clone()), None, "no catalog anywhere");
        std::fs::create_dir_all(&old_home).unwrap();
        assert_eq!(
            older_data_dir(&new, older.clone()),
            None,
            "a folder without a catalog"
        );
        std::fs::write(old_home.join("catalog.db"), b"").unwrap();
        assert_eq!(older_data_dir(&new, older.clone()), Some(old_home.clone()));
        // Still found once the new folder has a catalog of its own.
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(new.join("catalog.db"), b"").unwrap();
        assert_eq!(older_data_dir(&new, older.clone()), Some(old_home.clone()));
        // Not when it is the data dir in use.
        assert_eq!(older_data_dir(&old_home, older), None);

        // Only the default looks: an explicit --data-dir is what the user
        // chose. Off Windows the default never moved.
        let d = tmp.path().join("explicit");
        let cfg =
            Config::resolve(Cli::try_parse_from(["flux-server", "--data-dir", d.to_str().unwrap()]).unwrap())
                .unwrap();
        assert_eq!(cfg.older_data_dir, None);
        if !cfg!(windows) {
            assert!(older_default_data_dirs().is_empty());
        }
    }

    /// TOML "double quotes" turned Windows paths into control characters
    /// without a word (`\n`, `\t`, `\f`, …).
    #[test]
    fn toml_escapes_in_paths_are_refused_with_a_hint() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("flux.toml");
        let (f, d) = (file.to_str().unwrap(), dir.path().to_str().unwrap());
        let resolve = |toml: &str| {
            std::fs::write(&file, toml).unwrap();
            Config::resolve(Cli::try_parse_from(["flux-server", "--config", f, "--data-dir", d]).unwrap())
        };
        let err = resolve("[[library]]\nname = \"TV\"\nkind = \"shows\"\npath = \"D:\\\\media\\new\\tv\"\n")
            .unwrap_err();
        let err = format!("{err:#}");
        assert!(
            err.contains(r#"library "TV": path "D:\media<LF>ew<TAB>v" contains control characters"#),
            "{err}"
        );
        assert!(err.contains("'single quotes'") && err.contains(f), "{err}");
        // Refused even when the flag overrides it.
        let err = format!(
            "{:#}",
            resolve("data_dir = \"D:\\\\flux\\\\data\\new\"\n").unwrap_err()
        );
        assert!(err.contains(r#"data_dir "D:\flux\data<LF>ew" contains"#), "{err}");
        assert_eq!(
            show_controls("a\u{8}b\u{c}c\rd\u{1b}"),
            "a<BS>b<FF>c<CR>d<U+001B>"
        );
        // A literal string ('…') keeps backslashes as they are.
        let movies = dir.path().join("Movies");
        let cfg = resolve(&format!(
            "[[library]]\nname = \"Movies\"\nkind = \"movies\"\npath = '{}'\n",
            movies.display()
        ))
        .unwrap();
        assert_eq!(Path::new(&cfg.libraries[0].path), movies);
    }

    #[test]
    fn video_encoder_from_flag_or_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("flux.toml");
        std::fs::write(
            &file,
            "video_encoder = \"vaapi\"\nvaapi_device = \"/dev/dri/renderD129\"\n",
        )
        .unwrap();
        let d = dir.path().to_str().unwrap();
        let f = file.to_str().unwrap();
        let cfg =
            Config::resolve(Cli::try_parse_from(["flux-server", "--data-dir", d, "--config", f]).unwrap())
                .unwrap();
        // The variables beat the file (a run forcing an encoder sets them).
        if std::env::var_os("FLUX_VIDEO_ENCODER").is_none() {
            assert_eq!(cfg.video_encoder, Some(VideoEncoder::Vaapi));
        }
        if std::env::var_os("FLUX_VAAPI_DEVICE").is_none() {
            assert_eq!(cfg.vaapi_device, PathBuf::from("/dev/dri/renderD129"));
        }
        let cli = Cli::try_parse_from([
            "flux-server",
            "--data-dir",
            d,
            "--config",
            f,
            "--video-encoder",
            "libx264",
        ]);
        assert_eq!(
            Config::resolve(cli.unwrap()).unwrap().video_encoder,
            Some(VideoEncoder::Libx264)
        );
        let cli = Cli::try_parse_from([
            "flux-server",
            "--data-dir",
            d,
            "--config",
            f,
            "--video-encoder",
            "auto",
        ]);
        assert_eq!(Config::resolve(cli.unwrap()).unwrap().video_encoder, None);
        assert!(Cli::try_parse_from(["flux-server", "--video-encoder", "h265"]).is_err());
        assert!(toml::from_str::<FileConfig>("video_encoder = \"x265\"").is_err());
        assert!(Cli::try_parse_from(["flux-server", "--check"]).unwrap().check);
        assert!(Cli::try_parse_from(["flux-server", "--doctor"]).unwrap().check);
    }

    /// `--tailnet off` still ran `tailscale status` at every start.
    #[test]
    fn an_explicit_tailnet_off_runs_no_tailscale_probe() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_str().unwrap();
        let cfg = |args: &[&str]| {
            let argv = [&["flux-server", "--data-dir", d][..], args].concat();
            Config::resolve(Cli::try_parse_from(argv).unwrap()).unwrap()
        };
        if std::env::var_os("FLUX_TAILNET").is_none() {
            // Off by default, but the UI can offer this machine's Tailscale.
            assert!(cfg(&[]).probe_system_tailscale);
        }
        assert!(!cfg(&["--tailnet", "off"]).probe_system_tailscale);
        assert!(cfg(&["--tailnet", "system"]).probe_system_tailscale);
    }

    #[test]
    fn video_transcode_concurrency_from_flag_or_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("flux.toml");
        std::fs::write(&file, "max_video_transcodes = 4\n").unwrap();
        let d = dir.path().to_str().unwrap();
        let f = file.to_str().unwrap();
        let resolve = |args: &[&str]| {
            let mut a = vec!["flux-server", "--data-dir", d];
            a.extend_from_slice(args);
            Config::resolve(Cli::try_parse_from(a).unwrap()).unwrap()
        };
        if std::env::var_os("FLUX_MAX_VIDEO_TRANSCODES").is_none() {
            assert_eq!(resolve(&[]).max_video_transcodes, 0, "by encoder");
            assert_eq!(resolve(&["--config", f]).max_video_transcodes, 4);
        }
        assert_eq!(
            resolve(&["--config", f, "--max-video-transcodes", "8"]).max_video_transcodes,
            8
        );
        assert!(Cli::try_parse_from(["flux-server", "--max-video-transcodes", "many"]).is_err());
    }
}
