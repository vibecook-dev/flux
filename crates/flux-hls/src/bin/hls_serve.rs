//! hls-serve: a tiny dev server exposing the engine's HLS routes.
//!
//! ```text
//! hls-serve <file-or-dir> [--port 18080] [--preset chrome] [--index-cache DIR]
//! ```
//!
//! Routes (`{name}` = file name; `{preset}` optional, default `--preset`):
//! - `GET /media/{name}/[{preset}/]master.m3u8`, `v/index.m3u8`, `v/init.mp4`,
//!   `v/{n}.m4s`, `a/{key}/…`, `s/{track}.m3u8`, `s/{track}.vtt`
//! - `GET /media/{name}/[{preset}/]prepare` → JSON: index time, plan, reasons
//! - `GET /_stats` → JSON: segment timings, CPU (self + ffmpeg children), RSS
//!
//! Every response carries `x-flux-ms` (server time to produce the response,
//! excluding streaming the payload) and `Content-Length`.

// getrusage(2) has no std wrapper; this dev binary declares it directly.
#![allow(unsafe_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};

#[path = "shared/cli.rs"]
mod cli;
use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{header, HeaderValue, Response, StatusCode};
use axum::routing::get;
use axum::Router;
use flux_core::PlaybackPlan;
use flux_hls::{presets, Asset, EngineConfig, HlsEngine, HlsError, HlsResource, PlanPrefs};
use serde_json::json;

#[cfg(unix)]
#[repr(C)]
#[derive(Default)]
struct RUsage {
    utime_sec: i64,
    utime_usec: i32,
    _pad1: i32,
    stime_sec: i64,
    stime_usec: i32,
    _pad2: i32,
    maxrss: i64,
    rest: [i64; 13],
}

#[cfg(unix)]
extern "C" {
    fn getrusage(who: i32, usage: *mut RUsage) -> i32;
}

/// (user µs, system µs, max RSS bytes) for RUSAGE_SELF (0) or RUSAGE_CHILDREN (-1).
#[cfg(unix)]
fn rusage(who: i32) -> (i64, i64, i64) {
    let mut r = RUsage::default();
    // SAFETY: `r` is a properly sized, writable rusage buffer (macOS/arm64 layout).
    let rc = unsafe { getrusage(who, &mut r) };
    if rc != 0 {
        return (0, 0, 0);
    }
    (
        r.utime_sec * 1_000_000 + r.utime_usec as i64,
        r.stime_sec * 1_000_000 + r.stime_usec as i64,
        r.maxrss,
    )
}

/// Not Unix (Windows): there is no getrusage, so `/_stats` reports zero
/// CPU time and RSS. (Declaring it anyway breaks the MSVC link.)
#[cfg(not(unix))]
fn rusage(_who: i32) -> (i64, i64, i64) {
    (0, 0, 0)
}

type PlanKey = (String, String);
/// A prepared asset, its plan for one preset, and the prepare time (ms).
type PlanEntry = Arc<(Arc<Asset>, PlaybackPlan, f64)>;

struct AppState {
    engine: HlsEngine,
    files: HashMap<String, PathBuf>,
    default_preset: String,
    plans: Mutex<HashMap<PlanKey, PlanEntry>>,
}

fn error_response(e: &HlsError) -> Response<Body> {
    let status = match e {
        HlsError::NotFound(_) => StatusCode::NOT_FOUND,
        HlsError::Unsupported(_) => StatusCode::NOT_IMPLEMENTED,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let mut r = Response::new(Body::from(e.to_string()));
    *r.status_mut() = status;
    r
}

async fn plan_for(st: &AppState, name: &str, preset: &str) -> Result<PlanEntry, HlsError> {
    let key = (name.to_string(), preset.to_string());
    if let Some(p) = st.plans.lock().ok().and_then(|m| m.get(&key).cloned()) {
        return Ok(p);
    }
    let path = st
        .files
        .get(name)
        .ok_or_else(|| HlsError::NotFound(format!("no file {name}")))?;
    let caps = presets::by_name(preset).ok_or_else(|| HlsError::NotFound(format!("no preset {preset}")))?;
    let t0 = Instant::now();
    let asset = st.engine.prepare(path).await?;
    let prepare_ms = t0.elapsed().as_secs_f64() * 1000.0;
    // Dev server: always HLS (direct play is plain byte serving).
    let plan = st.engine.plan(
        &asset,
        &caps,
        &PlanPrefs {
            allow_direct_play: false,
            ..Default::default()
        },
    );
    // One-round-trip play: start segment 0 of every rendition right away.
    st.engine.warm(&asset, &plan, 0.0);
    let entry = Arc::new((asset, plan, prepare_ms));
    if let Ok(mut m) = st.plans.lock() {
        m.insert(key, entry.clone());
    }
    Ok(entry)
}

async fn media(
    State(st): State<Arc<AppState>>,
    UrlPath((name, rest)): UrlPath<(String, String)>,
) -> Response<Body> {
    let t0 = Instant::now();
    let (preset, path) = match rest.split_once('/') {
        Some((p, r)) if presets::by_name(p).is_some() => (p.to_string(), r.to_string()),
        _ => (st.default_preset.clone(), rest.clone()),
    };
    let entry = match plan_for(&st, &name, &preset).await {
        Ok(e) => e,
        Err(e) => return error_response(&e),
    };
    let (asset, plan, prepare_ms) = (&entry.0, &entry.1, entry.2);
    if path == "prepare" {
        let body = json!({
            "file": name, "preset": preset, "prepare_ms": prepare_ms,
            "index": { "method": format!("{:?}", asset.index.method), "micros": asset.index.stats.micros, "reads": asset.index.stats.reads, "bytes": asset.index.stats.bytes_read },
            "segments": asset.segments.len(), "duration_s": asset.index.duration_secs(),
            "segment_durations": (0..asset.segments.len()).map(|n| asset.segments.duration_secs(n)).collect::<Vec<_>>(),
            "presentation_offset_s": asset.index.presentation_offset_ns as f64 / 1e9,
            "frame_duration_s": asset.index.video_track().and_then(|v| v.default_duration_ns).map(|ns| ns as f64 / 1e9),
            "label": plan.label(), "plan": plan, "explain": plan.reasons.iter().map(|r| r.explain()).collect::<Vec<_>>(),
        });
        return json_response(body);
    }
    let Some(res) = HlsResource::parse(&path) else {
        return error_response(&HlsError::NotFound(path));
    };
    match st.engine.serve(asset, plan, res).await {
        Ok(r) => {
            let len = r.body.len();
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            let mut resp = Response::new(Body::from_stream(r.body.into_stream(1 << 20)));
            let h = resp.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static(r.content_type));
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static(r.cache_control));
            h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
            h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
            if let Ok(v) = HeaderValue::from_str(&format!("{ms:.3}")) {
                h.insert("x-flux-ms", v);
            }
            resp
        }
        Err(e) => error_response(&e),
    }
}

fn json_response(v: serde_json::Value) -> Response<Body> {
    let mut r = Response::new(Body::from(v.to_string()));
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r.headers_mut()
        .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    r
}

async fn stats(State(st): State<Arc<AppState>>) -> Response<Body> {
    let s = st.engine.stats();
    let (p50, p99) = s.percentiles().unwrap_or((0.0, 0.0));
    let t = st.engine.transcode_stats();
    let v = st.engine.video_transcode_stats();
    let c = st.engine.cache_usage();
    let (su, ss, rss) = rusage(0);
    let (cu, cs, _) = rusage(-1);
    json_response(json!({
        "segments": s.segments, "segment_bytes": s.segment_bytes, "segment_ms_p50": p50, "segment_ms_p99": p99,
        "transcode": { "segments": t.segments, "cache_hits": t.cache_hits, "prefetched": t.prefetched, "total_ms": t.total_ms, "max_ms": t.max_ms, "plan_ms": t.plan_ms, "ffmpeg_ms": t.ffmpeg_ms },
        "video_transcode": { "pieces": v.segments, "cache_hits": v.cache_hits, "prefetched": v.prefetched, "runs": v.runs, "first_piece_ms_total": v.first_piece_ms, "piece_ms_total": v.total_ms, "max_ms": v.max_ms },
        "caches": { "assets": c.assets, "subtitles": c.subtitles, "audio_segments": c.audio_segments, "video_segments": c.video_segments },
        "cpu_self_us": su + ss, "cpu_children_us": cu + cs, "max_rss_bytes": rss,
    }))
}

async fn index(State(st): State<Arc<AppState>>) -> Response<Body> {
    let mut names: Vec<&String> = st.files.keys().collect();
    names.sort();
    let mut html = String::from(
        "<!doctype html><meta charset=utf-8><title>hls-serve</title><h1>flux hls-serve</h1><ul>",
    );
    for n in names {
        html.push_str(&format!("<li>{n}: <a href=\"/media/{n}/chrome/master.m3u8\">chrome</a> · <a href=\"/media/{n}/safari/master.m3u8\">safari</a> · <a href=\"/media/{n}/appletv/master.m3u8\">appletv</a></li>"));
    }
    html.push_str("</ul>");
    let mut r = Response::new(Body::from(html));
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    r
}

fn collect_files(p: &Path) -> Result<HashMap<String, PathBuf>> {
    let mut out = HashMap::new();
    if p.is_dir() {
        for e in std::fs::read_dir(p)? {
            let path = e?.path();
            let ext = path
                .extension()
                .and_then(|x| x.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if matches!(
                ext.as_str(),
                "mkv" | "webm" | "mp4" | "m4v" | "mov" | "ts" | "m2ts" | "avi"
            ) {
                if let Some(n) = path.file_name().and_then(|n| n.to_str()) {
                    out.insert(n.to_string(), path.clone());
                }
            }
        }
    } else {
        let n = p.file_name().and_then(|n| n.to_str()).context("file name")?;
        out.insert(n.to_string(), p.to_path_buf());
    }
    Ok(out)
}

const USAGE: &str = "usage: hls-serve <file-or-dir>... [--port 18080] [--preset chrome] [--index-cache DIR]

Serves the files' HLS renditions on 127.0.0.1 (dev server for browsers,
AVPlayer and hls-bench): /media/{name}/[{preset}/]master.m3u8, /_stats.

  --port N           port on 127.0.0.1 (default 18080)
  --preset NAME      default client preset (default chrome)
  --index-cache DIR  cache playback indexes there
  -h, --help         this text";

/// Command-line options.
#[derive(Debug, PartialEq)]
struct Opts {
    port: u16,
    preset: String,
    cache: Option<PathBuf>,
    roots: Vec<PathBuf>,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<cli::Parsed<Opts>> {
    let mut args = args.into_iter();
    let mut o = Opts {
        port: 18080,
        preset: "chrome".to_string(),
        cache: None,
        roots: Vec::new(),
    };
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => return Ok(cli::Parsed::Help),
            "--port" => o.port = args.next().context("--port needs a number")?.parse()?,
            "--preset" => {
                o.preset = args.next().context("--preset needs a name")?;
                if presets::by_name(&o.preset).is_none() {
                    anyhow::bail!("unknown preset {}\n{USAGE}", o.preset);
                }
            }
            "--index-cache" => {
                o.cache = Some(PathBuf::from(
                    args.next().context("--index-cache needs a directory")?,
                ))
            }
            "--" => o.roots.extend(args.by_ref().map(PathBuf::from)),
            f if cli::is_flag(f) => return Err(cli::unknown_flag(f, USAGE)),
            r => o.roots.push(PathBuf::from(r)),
        }
    }
    if o.roots.is_empty() {
        anyhow::bail!("{USAGE}");
    }
    Ok(cli::Parsed::Run(o))
}

#[tokio::main]
async fn main() -> Result<()> {
    let o = match parse_args(std::env::args().skip(1))? {
        cli::Parsed::Help => {
            println!("{USAGE}");
            return Ok(());
        }
        cli::Parsed::Run(o) => o,
    };
    cli::check_inputs(&o.roots)?;
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let Opts {
        port,
        preset,
        cache,
        roots,
    } = o;
    let mut files = HashMap::new();
    for r in &roots {
        files.extend(collect_files(r)?);
    }
    let engine = HlsEngine::new(EngineConfig {
        index_cache_dir: cache,
        ..Default::default()
    });
    let state = Arc::new(AppState {
        engine,
        files,
        default_preset: preset,
        plans: Mutex::new(HashMap::new()),
    });
    let app = Router::new()
        .route("/", get(index))
        .route("/_stats", get(stats))
        .route("/media/{name}/{*rest}", get(media))
        .with_state(state.clone());
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("hls-serve: {} files on http://{addr}/", state.files.len());
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PB-NEW-3: `hls-serve --help` printed nothing useful and started a
    /// server with `--help` as a media root.
    #[test]
    fn help_and_unknown_flags() {
        let parse = |a: &[&str]| parse_args(a.iter().map(|s| s.to_string()));
        assert!(matches!(parse(&["--help"]).unwrap(), cli::Parsed::Help));
        assert!(parse(&["--prot", "1", "dir"]).is_err());
        assert!(parse(&["--preset", "nope", "dir"]).is_err());
        assert!(parse(&[]).is_err());
        let cli::Parsed::Run(o) = parse(&["dir", "--port", "18999", "--preset", "legacy"]).unwrap() else {
            panic!("run")
        };
        assert_eq!((o.port, o.preset.as_str()), (18999, "legacy"));
    }

    /// `/_stats`' CPU and RSS figures: real ones on Unix, zeros on Windows,
    /// where a getrusage declaration broke the link of this binary (and so
    /// `cargo test --workspace`).
    #[test]
    fn rusage_reports_this_process() {
        let (user, system, rss) = rusage(0);
        if cfg!(unix) {
            assert!(rss > 0 && user >= 0 && system >= 0, "{user} {system} {rss}");
        } else {
            assert_eq!((user, system, rss), (0, 0, 0));
        }
    }
}
