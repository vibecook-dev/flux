//! flux-meta benchmarks. Every run writes machine-readable JSON to
//! `$FLUX_BENCH_RESULTS/flux-meta-<name>.json` (printed instead when unset).
//! TMDB exports live in `$FLUX_TMDB_EXPORTS` (default
//! `<temp dir>/flux-tmdb-exports`), media comes from `$FLUX_TEST_MEDIA`.
//!
//! ```text
//! cargo run --release -p flux-meta --example meta_bench -- export   [--download]
//! cargo run --release -p flux-meta --example meta_bench -- posters  [--out DIR] [--only SUBSTR] [--hwaccel]
//! ```
//!
//! `export`  : download (optional) + build the movie/TV export indexes (time,
//!             memory), evaluate REAL_MOVIES top-1 (parser-free), matcher throughput.
//! `posters` : frame posters + backdrops for every file in media/features and
//!             media/playback-research (p50/p95 time per title), plus a gallery.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use flux_meta::eval::{evaluate_movies, load_truth};
use flux_meta::export_index::{download_latest, BuildOptions, ExportIndex, ExportKind};
use flux_meta::matcher::{Candidate, MatchQuery, Matcher, MediaKind};
use serde_json::json;

/// Where TMDB's daily ID exports are (and `--download` puts them).
fn exports_dir() -> PathBuf {
    std::env::var_os("FLUX_TMDB_EXPORTS")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("flux-tmdb-exports"))
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("export") => export_bench(args.iter().any(|a| a == "--download")),
        Some("posters") => posters::run(&args[1..]),
        _ => bail!("usage: meta_bench export [--download] | posters [--out DIR] [--only SUBSTR] [--hwaccel]"),
    }
}

/// Load average and process memory, for context in every result file.
pub(crate) fn machine() -> serde_json::Value {
    let uptime = std::process::Command::new("uptime")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
    json!({ "uptime": uptime, "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0) })
}

fn rss_bytes() -> u64 {
    let pid = std::process::id().to_string();
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok())
        .map_or(0, |kib| kib * 1024)
}

/// Not Unix: no getrusage.
#[cfg(not(unix))]
fn peak_rss_bytes() -> u64 {
    0
}

#[cfg(unix)]
fn peak_rss_bytes() -> u64 {
    // SAFETY: getrusage only writes into the zeroed struct we pass.
    #[allow(unsafe_code)]
    let ru = unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
        ru
    };
    let v = ru.ru_maxrss as u64;
    if cfg!(target_os = "macos") {
        v
    } else {
        v * 1024
    }
}

pub(crate) fn write_result(name: &str, value: &serde_json::Value) -> Result<Option<PathBuf>> {
    let text = serde_json::to_string_pretty(value)?;
    let path = flux_test_media::write_result(&format!("flux-meta-{name}.json"), &text);
    if path.is_none() {
        println!("{text}");
    }
    Ok(path)
}

fn latest_local(kind: ExportKind) -> Option<PathBuf> {
    let prefix = match kind {
        ExportKind::Movie => "movie_ids_",
        ExportKind::Tv => "tv_series_ids_",
    };
    let mut files: Vec<(String, PathBuf)> = std::fs::read_dir(exports_dir())
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            // movie_ids_MM_DD_YYYY.json.gz → sort key YYYYMMDD
            let rest = name.strip_prefix(prefix)?.strip_suffix(".json.gz")?.to_owned();
            let p: Vec<&str> = rest.split('_').collect();
            (p.len() == 3).then(|| (format!("{}{}{}", p[2], p[0], p[1]), e.path()))
        })
        .collect();
    files.sort();
    files.pop().map(|(_, p)| p)
}

fn export_bench(download: bool) -> Result<()> {
    if download {
        let rt = tokio::runtime::Runtime::new()?;
        let http = reqwest::Client::new();
        for kind in [ExportKind::Movie, ExportKind::Tv] {
            let t = Instant::now();
            let p = rt.block_on(download_latest(&http, &exports_dir(), kind, 3))?;
            println!("{kind:?}: {} ({:.1} s)", p.display(), t.elapsed().as_secs_f64());
        }
    }
    let rss0 = rss_bytes();
    let mut builds = Vec::new();
    let mut movie_index = None;
    for kind in [ExportKind::Movie, ExportKind::Tv] {
        let path = latest_local(kind).with_context(|| {
            format!(
                "no {kind:?} export in {}; run with --download",
                exports_dir().display()
            )
        })?;
        let gz = std::fs::metadata(&path)?.len();
        let t = Instant::now();
        let idx = ExportIndex::build_from_gz(&path, kind, BuildOptions::default())?;
        let secs = t.elapsed().as_secs_f64();
        let s = idx.stats();
        println!(
            "{kind:?}: {} rows → {} titles, {} tokens in {secs:.2} s; index heap {:.1} MB",
            s.rows_read,
            s.distinct_titles,
            s.tokens,
            idx.heap_bytes() as f64 / 1e6
        );
        builds.push(json!({
            "kind": kind, "file": path, "gz_bytes": gz, "build_s": secs,
            "index_heap_bytes": idx.heap_bytes(), "stats": s,
        }));
        if kind == ExportKind::Movie {
            movie_index = Some(idx);
        } else {
            drop(idx);
        }
    }
    let rss_after = rss_bytes();
    let movies = movie_index.context("movie index")?;

    // Lookup latency.
    let probes = [
        "The Matrix",
        "Blade Runner 2049",
        "Amelie",
        "Star Wars Episode IV A New Hope",
        "Godzilla Minus One",
    ];
    let t = Instant::now();
    let mut n = 0;
    for _ in 0..200 {
        for p in probes {
            n += movies.candidates(p, 10).len();
        }
    }
    let lookup_us = t.elapsed().as_secs_f64() * 1e6 / (200.0 * probes.len() as f64);
    println!("candidates(): {lookup_us:.0} µs/lookup ({n} hits)");

    // REAL_MOVIES evaluation (parser-free).
    let truth_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/real_movies_truth.json");
    let rows = load_truth(&std::fs::read_to_string(&truth_path)?)?;
    let matcher = Matcher::default();
    let t = Instant::now();
    let report = evaluate_movies(&movies, &matcher, &rows, 10);
    let eval_s = t.elapsed().as_secs_f64();
    println!(
        "REAL_MOVIES parser-free: top-1 {}/{} = {:.1}%, recall@10 {:.1}%  ({:?}); auto-accepted {} (wrong {})",
        report.top1,
        report.evaluated,
        100.0 * report.top1_accuracy,
        100.0 * report.recall_at_k,
        report.failures_by_class,
        report.auto_accepted,
        report.wrong_auto_accepted
    );
    for c in report.cases.iter().filter(|c| !c.correct) {
        println!(
            "  ✗ {:40} {} truth {:>7} ({:?}) got {:?} {:?} rank {:?} [{:?}]",
            c.title,
            c.year,
            c.truth,
            c.truth_original_title,
            c.predicted,
            c.predicted_title,
            c.rank_of_truth,
            c.failure
        );
    }

    // Matcher throughput on realistic candidate lists (10 export candidates each).
    let lists: Vec<(MatchQuery, Vec<Candidate>)> = rows
        .iter()
        .map(|r| {
            let cands = movies
                .candidates(&r.title, 10)
                .iter()
                .map(Candidate::from_export)
                .collect();
            let q = MatchQuery {
                kind: Some(MediaKind::Movie),
                titles: vec![r.title.clone()],
                year: Some(r.year),
                ..Default::default()
            };
            (q, cands)
        })
        .collect();
    let iters = 50;
    let t = Instant::now();
    let mut acc = 0usize;
    for _ in 0..iters {
        for (q, c) in &lists {
            acc += matcher.match_candidates(q, c).ranked.len();
        }
    }
    let total = (iters * lists.len()) as f64;
    let match_us = t.elapsed().as_secs_f64() * 1e6 / total;
    let avg_cands = lists.iter().map(|(_, c)| c.len()).sum::<usize>() as f64 / lists.len() as f64;
    println!(
        "matcher: {match_us:.1} µs/match ({:.0} matches/s, {avg_cands:.1} candidates each, {acc})",
        1e6 / match_us
    );

    let peak = peak_rss_bytes();
    write_result(
        "export-index",
        &json!({
            "machine": machine(), "builds": builds,
            "rss_before_bytes": rss0, "rss_after_builds_bytes": rss_after, "peak_rss_bytes": peak,
            "lookup_us_per_query": lookup_us,
            "notes": "index heap is exact (capacities); RSS includes allocator slack and the TV index built then dropped",
        }),
    )?;
    write_result(
        "matcher",
        &json!({
            "machine": machine(), "us_per_match": match_us, "matches_per_s": 1e6 / match_us,
            "avg_candidates": avg_cands, "queries": lists.len(), "iterations": iters,
        }),
    )?;
    write_result(
        "match-eval",
        &json!({ "machine": machine(), "eval_s": eval_s, "truth": truth_path, "report": report }),
    )?;
    Ok(())
}

mod posters {
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use anyhow::{Context, Result};
    use flux_meta::art::{episode_thumbnail, frame_posters, FramePosterOptions, MediaFacts};
    use serde_json::json;

    /// The corpus directories with titles to make posters for.
    fn media_dirs() -> [PathBuf; 2] {
        [
            flux_test_media::media_dir("features"),
            flux_test_media::media_dir("playback-research"),
        ]
    }
    const VIDEO_EXT: [&str; 7] = ["mkv", "mp4", "webm", "ts", "m2ts", "avi", "mov"];

    /// "h264_truehd71" → "H.264 TrueHD 7.1": readable titles for synthetic clips.
    pub fn humanize(stem: &str) -> String {
        let map = |t: &str| -> String {
            match t {
                "h264" => "H.264".into(),
                "hevc" => "HEVC".into(),
                "hevc10" => "HEVC 10-bit".into(),
                "av1" => "AV1".into(),
                "aac" => "AAC".into(),
                "ac3" => "AC-3".into(),
                "eac3" => "E-AC-3".into(),
                "dts51" => "DTS 5.1".into(),
                "truehd71" => "TrueHD 7.1".into(),
                "flac" => "FLAC".into(),
                "opus" => "Opus".into(),
                "mp3" => "MP3".into(),
                "mpeg2" => "MPEG-2".into(),
                "mpeg4" => "MPEG-4".into(),
                "hdr10" => "HDR10".into(),
                "main10" => "Main 10".into(),
                "hvc1" => "hvc1".into(),
                "4k" => "4K".into(),
                "60mbps" => "60 Mbps".into(),
                "2h" => "Two Hours".into(),
                "360p" => "360p".into(),
                "1080" => "1080p".into(),
                "10s" => "".into(),
                "10min" => "Ten Minutes".into(),
                "bbb" => "Big Buck Bunny".into(),
                "gop" => "GOP".into(),
                "opengop" => "Open GOP".into(),
                "moov" => "moov".into(),
                "30mb" | "actually" | "mp4" => "".into(),
                t if t.len() == 3 && t.starts_with('e') && t[1..].chars().all(|c| c.is_ascii_digit()) => {
                    t.to_uppercase()
                }
                t => {
                    let mut c = t.chars();
                    c.next()
                        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                        .unwrap_or_default()
                }
            }
        };
        stem.split('_')
            .map(|t| map(&t.to_lowercase()))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn media_files() -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = media_dirs()
            .iter()
            .flat_map(|d| {
                std::fs::read_dir(d)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path())
            })
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| VIDEO_EXT.contains(&e.to_lowercase().as_str()))
            })
            .collect();
        v.sort();
        v
    }

    fn pct(sorted: &[f64], p: f64) -> f64 {
        if sorted.is_empty() {
            return 0.0;
        }
        sorted[((sorted.len() - 1) as f64 * p).round() as usize]
    }

    pub fn run(args: &[String]) -> Result<()> {
        let out = args
            .windows(2)
            .find(|w| w[0] == "--out")
            .map(|w| PathBuf::from(&w[1]))
            .unwrap_or_else(|| std::env::temp_dir().join("flux-meta-posters"));
        let only = args.windows(2).find(|w| w[0] == "--only").map(|w| w[1].clone());
        std::fs::create_dir_all(&out)?;
        let rt = tokio::runtime::Runtime::new()?;
        let mut opts = FramePosterOptions::default();
        opts.tools.hwaccel = args.iter().any(|a| a == "--hwaccel");
        let result_name = if opts.tools.hwaccel {
            "frame-posters-hw"
        } else {
            "frame-posters"
        };
        let mut rows = Vec::new();
        let mut totals = Vec::new();
        for path in media_files() {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("x")
                .to_owned();
            // Full file name: av1_opus.mkv and av1_opus.webm must not overwrite each other.
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("x")
                .to_owned();
            if only.as_deref().is_some_and(|o| !stem.contains(o)) {
                continue;
            }
            let title = humanize(&stem);
            let t = Instant::now();
            let res = rt.block_on(frame_posters(&path, &title, Some(2026), None, &opts));
            let wall = t.elapsed().as_secs_f64() * 1e3;
            match res {
                Ok(fp) => {
                    let pp = out.join(format!("{name}.poster.jpg"));
                    let bp = out.join(format!("{name}.backdrop.jpg"));
                    std::fs::write(&pp, &fp.poster.bytes)?;
                    std::fs::write(&bp, &fp.backdrop.bytes)?;
                    let mut thumb = None;
                    if stem.starts_with("show_") {
                        let (img, _) = rt.block_on(episode_thumbnail(&path, Some(fp.facts), &opts))?;
                        let tp = out.join(format!("{name}.thumb.jpg"));
                        std::fs::write(&tp, &img.bytes)?;
                        thumb = Some(
                            json!({ "path": tp, "width": img.width, "height": img.height, "blurhash": img.blurhash }),
                        );
                    }
                    let rejected = fp.candidates.iter().filter(|c| c.reject.is_some()).count();
                    let dups = fp.candidates.iter().filter(|c| c.duplicate).count();
                    println!(
                        "{stem:38} {:>7.0} ms (harvest {:>5.0}, final {:>4.0}, compose {:>4.0})  {}x{} {:?}  t={:.1}s  cands {} rej {} dup {}",
                        wall,
                        fp.timings.harvest_ms,
                        fp.timings.final_decode_ms,
                        fp.timings.compose_ms,
                        fp.facts.width,
                        fp.facts.height,
                        fp.facts.transfer,
                        fp.chosen_at_s,
                        fp.candidates.len(),
                        rejected,
                        dups
                    );
                    totals.push(wall);
                    rows.push(json!({
                        "file": path, "title": title, "wall_ms": wall, "timings": fp.timings, "facts": fp.facts,
                        "chosen_at_s": fp.chosen_at_s, "candidates": fp.candidates,
                        "poster": { "path": pp, "width": fp.poster.width, "height": fp.poster.height, "bytes": fp.poster.bytes.len(), "blurhash": fp.poster.blurhash },
                        "backdrop": { "path": bp, "width": fp.backdrop.width, "height": fp.backdrop.height, "bytes": fp.backdrop.bytes.len(), "blurhash": fp.backdrop.blurhash },
                        "thumb": thumb,
                    }));
                }
                Err(e) => {
                    println!("{stem:38} FAILED: {e}");
                    rows.push(json!({ "file": path, "title": title, "error": e.to_string() }));
                }
            }
        }
        typography_sheet(&rt, &out, &opts)?;
        totals.sort_by(f64::total_cmp);
        let summary = json!({
            "titles": totals.len(), "p50_ms": pct(&totals, 0.5), "p95_ms": pct(&totals, 0.95),
            "max_ms": totals.last().copied().unwrap_or(0.0), "candidates_per_title": opts.candidates,
            "hwaccel": opts.tools.hwaccel, "concurrency": opts.concurrency,
        });
        println!("frame posters: {summary}");
        if only.is_none() {
            super::write_result(
                result_name,
                &json!({ "machine": super::machine(), "summary": summary, "rows": rows }),
            )?;
        }
        Ok(())
    }

    /// Typography stress test: one frame, many titles (long, CJK, Hangul, Telugu, diacritics, one letter).
    fn typography_sheet(rt: &tokio::runtime::Runtime, out: &Path, opts: &FramePosterOptions) -> Result<()> {
        let Some(src) = flux_test_media::media("playback-research/bbb_1080_10s.mkv") else {
            return Ok(());
        };
        let src = src.as_path();
        let dir = out.join("typography");
        std::fs::create_dir_all(&dir)?;
        let facts: MediaFacts = rt
            .block_on(MediaFacts::ffprobe(&opts.tools, src))
            .context("ffprobe")?;
        let titles: [(&str, Option<u16>); 10] = [
            ("M", Some(1931)),
            ("Heat", Some(1995)),
            ("Amélie", Some(2001)),
            ("Mad Max: Fury Road", Some(2015)),
            ("The Lord of the Rings: The Fellowship of the Ring", Some(2001)),
            ("Everything Everywhere All at Once", Some(2022)),
            ("千と千尋の神隠し", Some(2001)),
            ("기생충", Some(2019)),
            ("రౌద్రం రణం రుధిరం", Some(2022)),
            ("Grandma's 80th Birthday", None),
        ];
        for (i, (title, year)) in titles.iter().enumerate() {
            let fp = rt.block_on(frame_posters(src, title, *year, Some(facts), opts))?;
            std::fs::write(dir.join(format!("{i:02}.poster.jpg")), &fp.poster.bytes)?;
        }
        println!("typography sheet: {}", dir.display());
        Ok(())
    }
}
