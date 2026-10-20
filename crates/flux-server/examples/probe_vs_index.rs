//! Integration check: does the catalog's stored probe (flux-probe, written at
//! scan time) agree with the playback index (flux-hls, built at play time)?
//!
//! flux-server needs them to agree in three places:
//! - `PlayRequest.audio_track` / `subtitle_track` are `Track::index` values the
//!   client read from `AssetInfo.probe` (flux-probe); flux-hls interprets them
//!   against its own index.
//! - A dry-run plan (`POST /api/v1/plan`, the item page's "expected on this
//!   device") should come from the stored probe without loading the index.
//! - Labels and badges in the catalog come from the probe; the player's
//!   diagnostics come from the index.
//!
//! For every file it compares tracks field by field, then plans for each
//! client preset three ways: `HlsEngine::plan` (index), `finalize_plan` of a
//! plan made from the flux-probe result (what `POST /play` can do from the
//! catalog), and `flux_hls::plan_from_probe` (the dry run `POST /plan` makes
//! without the index).
//!
//! ```sh
//! cargo run --release -p flux-server --example probe_vs_index [-- FILE...]
//! ```
//!
//! Without FILEs it checks the corpus (`$FLUX_TEST_MEDIA/features` and
//! `playback-research`). Writes
//! `$FLUX_BENCH_RESULTS/flux-integrate-probe-vs-index.json` when that is set.

use std::error::Error;
use std::path::PathBuf;

use flux_core::{PlaybackPlan, ProbeResult, Track};
use flux_hls::{presets, EngineConfig, HlsEngine, PlanPrefs};
use serde_json::{json, Value};

/// The corpus directories checked when no files are given.
fn dirs() -> [PathBuf; 2] {
    [
        flux_test_media::media_dir("features"),
        flux_test_media::media_dir("playback-research"),
    ]
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut files: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    if files.is_empty() {
        files = media_in(&dirs());
    }
    let out = check(&files)?;
    let text = serde_json::to_string_pretty(&out)?;
    println!("{text}");
    flux_test_media::write_result("flux-integrate-probe-vs-index.json", &text);
    Ok(())
}

/// Runs with `cargo test -p flux-server` (the example is `test = true`): on
/// the feature clips (and the clips made with ffmpeg, so it runs anywhere),
/// the catalog's probe and the playback index must list the same tracks,
/// and both a plan made from the catalog probe (then finalized by the
/// engine) and the probe-only dry run must equal the engine's own plan for
/// every client preset.
#[test]
fn feature_clips_plan_the_same_from_catalog_probe_and_index() {
    use flux_test_media::Clip;
    let mut files = media_in(&dirs()[..1]);
    files.extend(
        [
            Clip::H264AacMp4,
            Clip::HevcAc3Mkv,
            Clip::H264AacTs,
            Clip::HevcAacTs,
            Clip::Mpeg4Mp2Avi,
            Clip::Hdr10HevcMkv,
        ]
        .into_iter()
        .filter_map(flux_test_media::clip),
    );
    if files.is_empty() {
        return;
    }
    let out = check(&files).expect("check runs");
    for f in out["per_file"].as_array().expect("per_file") {
        assert!(
            f.get("probe_error").is_none() && f.get("index_error").is_none(),
            "{f}"
        );
        assert_eq!(f["same_track_list"], true, "{f}");
    }
    assert_eq!(out["plans_differ_index_vs_catalog_finalized"], 0, "{out}");
    assert_eq!(out["plans_differ_index_vs_dry_run"], 0, "{out}");
}

/// Media files directly inside `dirs`, sorted.
fn media_in(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else {
            eprintln!("skipping: {} not present", dir.display());
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
            if matches!(ext, "mkv" | "mp4" | "webm" | "ts" | "m2ts" | "avi" | "mov") {
                files.push(p);
            }
        }
    }
    files.sort();
    files
}

/// Compares probe and index for every file and plans each preset three ways.
fn check(files: &[PathBuf]) -> Result<Value, Box<dyn Error>> {
    let presets: Vec<(&str, flux_core::ClientCaps)> =
        ["chrome", "safari", "firefox", "appletv", "androidtv", "legacy"]
            .iter()
            .filter_map(|n| presets::by_name(n).map(|c| (*n, c)))
            .collect();

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let engine = HlsEngine::new(EngineConfig::default());
    let prefs = PlanPrefs::default();
    let mut report = Vec::new();
    let (mut track_diffs, mut plan_diffs, mut dry_run_diffs) = (0usize, 0usize, 0usize);
    for f in files {
        let name = f
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let probe = match flux_probe::probe_file(f) {
            Ok(p) => p,
            Err(e) => {
                report.push(json!({"file": name, "probe_error": e.to_string()}));
                continue;
            }
        };
        let asset = match rt.block_on(engine.prepare(f)) {
            Ok(a) => a,
            Err(e) => {
                report.push(json!({"file": name, "index_error": e.to_string()}));
                continue;
            }
        };
        let index = engine.probe(&asset);
        let diffs = compare_probe(&probe, &index);
        track_diffs += diffs.len();

        let mut plans = serde_json::Map::new();
        for (preset, caps) in &presets {
            let by_index = engine.plan(&asset, caps, &prefs);
            let raw = flux_hls::plan(&probe, caps, &prefs);
            let from_catalog = engine.finalize_plan(&asset, raw);
            let dry_run = flux_hls::plan_from_probe(&probe, caps, &prefs);
            let a = summarize(&by_index);
            let b = summarize(&from_catalog);
            let c = summarize(&dry_run);
            if a != b {
                plan_diffs += 1;
            }
            if a != c {
                dry_run_diffs += 1;
            }
            if a != b || a != c {
                plans.insert(
                    (*preset).to_string(),
                    json!({"index": a, "catalog_probe_finalized": b, "catalog_probe_dry_run": c}),
                );
            }
        }
        let same_track_list = probe
            .tracks
            .iter()
            .map(|t| (t.kind, t.index))
            .eq(index.tracks.iter().map(|t| (t.kind, t.index)));
        report.push(json!({
            "file": name,
            "tracks": probe.tracks.len(),
            "same_track_list": same_track_list,
            "track_diffs": diffs,
            "plan_diffs": plans,
        }));
    }
    Ok(json!({
        "component": "integrate",
        "name": "probe-vs-index",
        "files": files.len(),
        "track_field_diffs": track_diffs,
        "plans_differ_index_vs_catalog_finalized": plan_diffs,
        "plans_differ_index_vs_dry_run": dry_run_diffs,
        "per_file": report,
    }))
}

/// Field-by-field differences that matter to the server (track identity and
/// the facts the planner reads). Duration may differ by up to a frame.
fn compare_probe(p: &ProbeResult, q: &ProbeResult) -> Vec<String> {
    let mut d = Vec::new();
    if p.container != q.container {
        d.push(format!("container {:?} vs {:?}", p.container, q.container));
    }
    match (p.duration_ms, q.duration_ms) {
        (Some(a), Some(b)) if a.abs_diff(b) > 100 => d.push(format!("duration {a} vs {b} ms")),
        (a, b) if a.is_some() != b.is_some() => d.push(format!("duration {a:?} vs {b:?}")),
        _ => {}
    }
    let key = |t: &Track| (t.kind, t.index);
    let pk: Vec<_> = p.tracks.iter().map(key).collect();
    let qk: Vec<_> = q.tracks.iter().map(key).collect();
    if pk != qk {
        d.push(format!("track (kind, index) lists differ: {pk:?} vs {qk:?}"));
    }
    for t in &p.tracks {
        let Some(u) = q.tracks.iter().find(|u| u.index == t.index) else {
            continue;
        };
        let mut f = |field: &str, a: String, b: String| {
            if a != b {
                d.push(format!("track {} {field}: {a} vs {b}", t.index));
            }
        };
        f("id", t.id.to_string(), u.id.to_string());
        f("codec", format!("{:?}", t.codec), format!("{:?}", u.codec));
        f(
            "channels",
            format!("{:?}", t.channels),
            format!("{:?}", u.channels),
        );
        f(
            "sample_rate",
            format!("{:?}", t.sample_rate),
            format!("{:?}", u.sample_rate),
        );
        f("width", format!("{:?}", t.width), format!("{:?}", u.width));
        f("height", format!("{:?}", t.height), format!("{:?}", u.height));
        f(
            "bit_depth",
            format!("{:?}", t.bit_depth),
            format!("{:?}", u.bit_depth),
        );
        f("profile", format!("{:?}", t.profile), format!("{:?}", u.profile));
        f("level", format!("{:?}", t.level), format!("{:?}", u.level));
        f(
            "language",
            format!("{:?}", t.language),
            format!("{:?}", u.language),
        );
        f("default", t.default.to_string(), u.default.to_string());
        f("forced", t.forced.to_string(), u.forced.to_string());
        f(
            "hdr",
            format!("{:?}", t.hdr.as_ref().map(|h| h.format)),
            format!("{:?}", u.hdr.as_ref().map(|h| h.format)),
        );
        f(
            "frame_rate",
            format!(
                "{:?}",
                t.frame_rate.map(|r| (r.as_f64() * 1000.0).round() / 1000.0)
            ),
            format!(
                "{:?}",
                u.frame_rate.map(|r| (r.as_f64() * 1000.0).round() / 1000.0)
            ),
        );
    }
    d
}

/// The parts of a plan a user sees: label, delivery, video op, audio
/// renditions, reasons.
fn summarize(p: &PlaybackPlan) -> Value {
    json!({
        "label": p.label(),
        "delivery": p.delivery,
        "video": match &p.video {
            flux_core::VideoOp::Copy { fixes } => format!("copy {fixes:?}"),
            flux_core::VideoOp::Transcode { codec, width, height, tonemap, .. } => {
                format!("transcode {codec:?} {width}x{height} tonemap={tonemap}")
            }
        },
        "audio": p.audio.iter().map(|a| format!("{}:{:?}", a.track, a.op)).collect::<Vec<_>>(),
        "reasons": p.reasons.iter().map(|r| r.explain()).collect::<Vec<_>>(),
    })
}
