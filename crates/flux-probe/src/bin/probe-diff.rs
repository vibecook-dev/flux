//! Differential test: flux-probe vs ffprobe on every media file under a dir.
//!
//! ```text
//! probe-diff <dir> [--out results.json] [--frame-hdr auto|always|never] [--quiet]
//! ```
//!
//! Compares container, duration (±0.5 % or ±100 ms), track count/kinds,
//! codecs, width/height, sample aspect ratio, bit depth, channels, sample
//! rate, language, default/forced flags and HDR format. Exits 1 on any
//! mismatch.
//!
//! Sample aspect ratio: a SAR that isn't stated plays as square pixels, so
//! "not stated" on either side (flux `None`, ffprobe `N/A`/absent) compares
//! equal to 1:1; every other value must match exactly (both are reduced).
//!
//! Files with a video extension that flux-probe doesn't parse (`.mpg`, `.flv`,
//! … → `UnknownFormat`) are what the scanner sends to the ffprobe fallback:
//! counted as `fallback`, not as mismatches.
//! ffprobe's stream-level output misses frame-level HDR facts (e.g. PQ
//! signalled only in an HEVC MKV's SPS), so for HDR-ish files the oracle also
//! reads the first frame (`--frame-hdr auto`).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use flux_core::{Codec, Container, ProbeResult, Rational, Track, TrackKind};
use flux_probe::{ffprobe, is_media_path, probe_file, ProbeError};
use serde_json::json;

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            walk(&p, out);
        } else if is_media_path(&p) {
            out.push(p);
        }
    }
}

fn class(c: Container) -> &'static str {
    match c {
        Container::Matroska | Container::WebM => "matroska",
        Container::Mp4 | Container::Mov => "mp4",
        Container::MpegTs | Container::M2ts => "mpegts",
        Container::Avi => "avi",
        Container::Asf => "asf",
        Container::Flv => "flv",
        Container::RealMedia => "rm",
        Container::MpegPs => "mpeg",
        Container::Unknown => "unknown",
    }
}

fn codec_class(c: Codec) -> Codec {
    match c {
        Codec::Ssa => Codec::Ass,
        c => c,
    }
}

fn lang(l: &Option<String>) -> Option<String> {
    l.as_deref()
        .map(str::to_ascii_lowercase)
        .filter(|l| l != "und" && !l.is_empty())
}

/// Unstated = square.
fn sar(r: Option<Rational>) -> (u32, u32) {
    r.map_or((1, 1), |r| (r.num, r.den))
}

fn media_tracks(r: &ProbeResult) -> Vec<&Track> {
    r.tracks
        .iter()
        .filter(|t| matches!(t.kind, TrackKind::Video | TrackKind::Audio | TrackKind::Subtitle))
        .collect()
}

#[derive(Default)]
struct Diff {
    items: Vec<(String, String, String)>,
}

impl Diff {
    fn check<T: std::fmt::Debug + PartialEq>(&mut self, field: impl Into<String>, ours: T, theirs: T) {
        if ours != theirs {
            self.items
                .push((field.into(), format!("{ours:?}"), format!("{theirs:?}")));
        }
    }
}

fn compare(ours: &ProbeResult, theirs: &ProbeResult) -> Diff {
    let mut d = Diff::default();
    d.check("container", class(ours.container), class(theirs.container));
    match (ours.duration_ms, theirs.duration_ms) {
        (Some(a), Some(b)) => {
            let tol = 100.max(b / 200);
            if a.abs_diff(b) > tol {
                d.check("duration_ms", a, b);
            }
        }
        (a, b) => d.check("duration_ms", a, b),
    }
    let (mo, mt) = (media_tracks(ours), media_tracks(theirs));
    d.check(
        "track kinds",
        mo.iter().map(|t| t.kind).collect::<Vec<_>>(),
        mt.iter().map(|t| t.kind).collect(),
    );
    for (i, (a, b)) in mo.iter().zip(&mt).enumerate() {
        let f = |n: &str| format!("track {i} {n}");
        d.check(f("codec"), codec_class(a.codec), codec_class(b.codec));
        d.check(f("language"), lang(&a.language), lang(&b.language));
        d.check(f("default"), a.default, b.default);
        d.check(f("forced"), a.forced, b.forced);
        match a.kind {
            TrackKind::Video => {
                d.check(f("width"), a.width, b.width);
                d.check(f("height"), a.height, b.height);
                d.check(
                    f("sample_aspect_ratio"),
                    sar(a.sample_aspect_ratio),
                    sar(b.sample_aspect_ratio),
                );
                if let (Some(x), Some(y)) = (a.bit_depth, b.bit_depth) {
                    d.check(f("bit_depth"), x, y);
                }
                let fa = a.hdr.as_ref().map(|h| h.format);
                let fb = b.hdr.as_ref().map(|h| h.format);
                d.check(f("hdr"), fa, fb);
                if let (Some(ha), Some(hb)) = (&a.hdr, &b.hdr) {
                    if hb.mastering.is_some() {
                        d.check(f("mastering"), ha.mastering, hb.mastering);
                    }
                    if hb.cll.is_some() {
                        d.check(f("cll"), ha.cll, hb.cll);
                    }
                }
            }
            TrackKind::Audio => {
                d.check(f("channels"), a.channels, b.channels);
                d.check(f("sample_rate"), a.sample_rate, b.sample_rate);
            }
            _ => {}
        }
    }
    d
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(dir) = args
        .iter()
        .find(|a| !a.starts_with("--") && !a.ends_with(".json"))
        .cloned()
    else {
        eprintln!("usage: probe-diff <dir> [--out file.json] [--frame-hdr auto|always|never] [--quiet]");
        return ExitCode::from(2);
    };
    let opt = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let frame_hdr = opt("--frame-hdr").unwrap_or_else(|| "auto".into());
    let quiet = args.iter().any(|a| a == "--quiet");
    let base = Path::new(&dir)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("dir")
        .to_string();
    // `--out`, else `$FLUX_BENCH_RESULTS/flux-probe-diff-<dir>.json`, else none.
    let out_path = opt("--out").or_else(|| {
        std::env::var("FLUX_BENCH_RESULTS")
            .ok()
            .filter(|d| !d.is_empty())
            .map(|d| format!("{d}/flux-probe-diff-{base}.json"))
    });

    let mut files = Vec::new();
    walk(Path::new(&dir), &mut files);
    let mut report = Vec::new();
    let (mut ok, mut mismatched, mut both_rejected, mut fallback) = (0, 0, 0, 0);
    for p in &files {
        let name = p.strip_prefix(&dir).unwrap_or(p).display().to_string();
        let ours = probe_file(p);
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        let theirs = ffprobe::run_json(p, ffprobe::DEFAULT_ARGS).map(|v| ffprobe::from_json(&v, ext));
        let entry = match (ours, theirs) {
            (Ok(o), Ok(mut t)) => {
                let ours_hdr = o
                    .main_video()
                    .and_then(|v| v.hdr.as_ref())
                    .is_some_and(|h| h.format != flux_core::HdrFormat::Sdr);
                let theirs_unsure = t.main_video().is_some_and(|v| {
                    v.bit_depth.unwrap_or(8) > 8 && v.color.and_then(|c| c.transfer).is_none()
                });
                let want = match frame_hdr.as_str() {
                    "always" => t.main_video().is_some(),
                    "never" => false,
                    _ => ours_hdr || theirs_unsure,
                };
                if want {
                    if let Err(e) = ffprobe::add_first_frame_hdr(p, &mut t) {
                        eprintln!("{name}: ffprobe frame pass failed: {e}");
                    }
                }
                let d = compare(&o, &t);
                if d.items.is_empty() {
                    ok += 1;
                    if !quiet {
                        println!("ok        {name}  ({} reads, {} B)", o.reads, o.bytes_read);
                    }
                    json!({"file": name, "status": "ok", "reads": o.reads, "bytes": o.bytes_read})
                } else {
                    mismatched += 1;
                    println!("MISMATCH  {name}");
                    for (f, a, b) in &d.items {
                        println!("          {f}: flux={a} ffprobe={b}");
                    }
                    json!({"file": name, "status": "mismatch", "fields": d.items.iter().map(|(f, a, b)| json!({"field": f, "flux": a, "ffprobe": b})).collect::<Vec<_>>()})
                }
            }
            (Err(e), Err(fe)) => {
                both_rejected += 1;
                if !quiet {
                    println!("rejected  {name}  (flux: {e}; ffprobe: {fe})");
                }
                json!({"file": name, "status": "both_rejected", "flux": e.to_string(), "ffprobe": fe.to_string()})
            }
            (Err(e @ ProbeError::UnknownFormat { .. }), Ok(t)) => {
                fallback += 1;
                if !quiet {
                    println!("fallback  {name}  ({e}; ffprobe reads it as {:?})", t.container);
                }
                json!({"file": name, "status": "fallback", "flux": e.to_string()})
            }
            (Err(e), Ok(_)) => {
                mismatched += 1;
                println!("MISMATCH  {name}\n          flux error: {e}");
                json!({"file": name, "status": "mismatch", "flux_error": e.to_string()})
            }
            (Ok(_), Err(fe)) => {
                mismatched += 1;
                println!("MISMATCH  {name}\n          ffprobe error: {fe}");
                json!({"file": name, "status": "mismatch", "ffprobe_error": fe.to_string()})
            }
        };
        report.push(entry);
    }
    println!(
        "\n{} files: {ok} ok, {mismatched} mismatched, {both_rejected} rejected by both, {fallback} for the ffprobe fallback",
        files.len()
    );
    let summary = json!({
        "tool": "flux-probe probe-diff",
        "dir": dir,
        "files": files.len(),
        "ok": ok,
        "mismatched": mismatched,
        "both_rejected": both_rejected,
        "fallback": fallback,
        "results": report,
    });
    match &out_path {
        Some(out_path) => {
            if let Err(e) = std::fs::write(
                out_path,
                serde_json::to_string_pretty(&summary).unwrap_or_default(),
            ) {
                eprintln!("cannot write {out_path}: {e}");
            } else {
                println!("wrote {out_path}");
            }
        }
        None => eprintln!("results not written (pass --out or set FLUX_BENCH_RESULTS)"),
    }
    if mismatched > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
