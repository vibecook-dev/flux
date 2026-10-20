//! `names-eval`: accuracy and throughput of flux-names.
//!
//! Scores the parser against the synthetic library ground truth
//! (`library.manifest.jsonl`, `library-tiny.manifest.jsonl`) and the 36 hard
//! cases from research/02 §6.2, scores it out of distribution on guessit's
//! own test suite (the fixtures shipped in the `hunch` crate; even-indexed
//! cases are the dev half, odd-indexed the held-out half), compares it with
//! `hunch` alone, and measures single-thread throughput. Prints Markdown tables and writes JSON to
//! `<out>/flux-names-*.json`.
//!
//! ```text
//! cargo run --release -p flux-names --bin names-eval -- \
//!     [--media DIR] [--out DIR] [--guessit DIR] [--examples N] [--reps N]
//!     [--no-bench] [--no-baseline] [--failures]
//! ```

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use flux_names::eval::{self, Evaluation, HardReport, Input};
use flux_names::hard_cases::CASES;
use flux_names::{parse_with, Fallback, LibraryKind, NameGuess, ParseOptions, PARSER_VERSION};
use serde_json::json;

struct Args {
    media: PathBuf,
    out: PathBuf,
    examples: usize,
    reps: usize,
    bench: bool,
    baseline: bool,
    show_failures: bool,
    guessit: Option<PathBuf>,
}

/// guessit's fixtures inside the locally cached `hunch` crate.
fn find_guessit_fixtures() -> Option<PathBuf> {
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))?;
    let src = home.join("registry/src");
    std::fs::read_dir(src).ok()?.flatten().find_map(|d| {
        let p = d.path().join("hunch-2.0.2/tests/fixtures");
        p.join("movies.yml").exists().then_some(p)
    })
}

fn args() -> Result<Args, Box<dyn Error>> {
    let mut a = Args {
        // The corpus (`$FLUX_TEST_MEDIA`; the manifest scores skip without
        // it) and the results directory (`$FLUX_BENCH_RESULTS`, else a
        // directory in the temp dir, printed).
        media: std::env::var_os("FLUX_TEST_MEDIA")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("<FLUX_TEST_MEDIA unset>")),
        out: std::env::var_os("FLUX_BENCH_RESULTS")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let d = std::env::temp_dir().join("flux-names-eval");
                eprintln!("FLUX_BENCH_RESULTS is not set: results go to {}", d.display());
                d
            }),
        examples: 5,
        reps: 5,
        bench: true,
        baseline: true,
        show_failures: false,
        guessit: find_guessit_fixtures(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--media" => a.media = it.next().ok_or("--media needs a value")?.into(),
            "--out" => a.out = it.next().ok_or("--out needs a value")?.into(),
            "--examples" => a.examples = it.next().ok_or("--examples needs a value")?.parse()?,
            "--reps" => a.reps = it.next().ok_or("--reps needs a value")?.parse()?,
            "--no-bench" => a.bench = false,
            "--no-baseline" => a.baseline = false,
            "--failures" => a.show_failures = true,
            "--guessit" => a.guessit = Some(it.next().ok_or("--guessit needs a value")?.into()),
            "-h" | "--help" => {
                println!(
                    "names-eval [--media DIR] [--out DIR] [--guessit DIR] [--examples N] [--reps N] [--no-bench] [--no-baseline] [--failures]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    Ok(a)
}

fn opts(fallback: Fallback) -> ParseOptions {
    ParseOptions { fallback }
}

fn run(i: &Input, fallback: Fallback) -> NameGuess {
    parse_with(&i.rel, &i.sibling_refs(), i.kind, opts(fallback))
}

#[cfg(feature = "hunch")]
fn run_hunch(i: &Input) -> NameGuess {
    flux_names::parse_fallback_only(&i.rel, &i.sibling_refs(), i.kind, false)
}

fn load_avg() -> String {
    std::process::Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().trim_matches(['{', '}']).trim().to_string())
        .unwrap_or_default()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn summary(e: &Evaluation) -> serde_json::Value {
    let get = |k: &str, f: &str| {
        e.per_kind
            .get(k)
            .and_then(|m| m.get(f))
            .map_or(100.0, |c| c.pct())
    };
    json!({
        "movie_title_year": get("movie", "title+year"),
        "movie_all": get("movie", "all"),
        "episode_season_episodes": get("episode", "season+episodes"),
        "episode_title": get("episode", "title"),
        "episode_all": get("episode", "all"),
        "sample_kind": get("sample", "kind"),
        "extra_all": get("extra", "all"),
        "fallback_used": e.fallback_used,
        "media_files": e.media_files,
    })
}

fn main() -> Result<(), Box<dyn Error>> {
    let a = args()?;
    std::fs::create_dir_all(&a.out)?;
    let load_start = load_avg();
    println!("# flux-names evaluation (parser v{PARSER_VERSION})\n\nload average at start: {load_start}\n");

    for (name, file) in [
        ("library-tiny", "library-tiny.manifest.jsonl"),
        ("library", "library.manifest.jsonl"),
    ] {
        let path = a.media.join(file);
        let rows = match eval::load_manifest(&path) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping {name}: {e}");
                continue;
            }
        };
        let t = Instant::now();
        let auto = eval::evaluate(&rows, |i| run(i, Fallback::Auto), a.examples);
        let auto_s = t.elapsed().as_secs_f64();
        let fast = eval::evaluate(&rows, |i| run(i, Fallback::Never), a.examples);
        print!(
            "{}",
            eval::render_markdown(
                &format!("{name}: flux-names (default: fast path + fallback when unsure)"),
                &auto
            )
        );
        println!("fast path only: {}\n", summary(&fast));
        #[allow(unused_mut)]
        let mut modes = json!({ "auto": auto, "fast_only": fast });
        #[cfg(feature = "hunch")]
        if a.baseline {
            let t = Instant::now();
            let h = eval::evaluate(&rows, run_hunch, a.examples);
            println!(
                "hunch alone (with siblings, no post-rules), {:.1}s: {}\n",
                t.elapsed().as_secs_f64(),
                summary(&h)
            );
            modes["hunch_only"] = serde_json::to_value(&h)?;
        }
        if a.show_failures {
            for (class, ex) in &auto.failures {
                println!("## {class}");
                for f in ex {
                    println!(
                        "- `{}` expected {} got {} (conf {:.2}) {:?}",
                        f.path, f.expected, f.got, f.confidence, f.evidence
                    );
                }
            }
        }
        let out = json!({
            "component": "flux-names",
            "name": format!("accuracy-{name}"),
            "manifest": path,
            "parser_version": PARSER_VERSION,
            "created_unix": now(),
            "load_avg": load_avg(),
            "eval_seconds_auto": auto_s,
            "summary": { "auto": summary(&auto), "fast_only": summary(&fast) },
            "modes": modes,
        });
        let p = a.out.join(format!("flux-names-accuracy-{name}.json"));
        std::fs::write(&p, serde_json::to_string_pretty(&out)?)?;
        println!("wrote {}\n", p.display());
    }

    // Hard cases.
    let hard_input = |c: &flux_names::hard_cases::HardCase| Path::new(c.input).to_path_buf();
    let flux_auto =
        eval::evaluate_hard(|c| parse_with(&hard_input(c), &[], LibraryKind::Mixed, opts(Fallback::Auto)));
    let flux_fast =
        eval::evaluate_hard(|c| parse_with(&hard_input(c), &[], LibraryKind::Mixed, opts(Fallback::Never)));
    print_hard("flux-names (default)", &flux_auto);
    #[allow(unused_mut)]
    let mut hard = json!({ "flux_auto": flux_auto, "flux_fast_only": flux_fast });
    #[cfg(feature = "hunch")]
    {
        let raw = eval::evaluate_hard(|c| {
            flux_names::parse_fallback_only(&hard_input(c), &[], LibraryKind::Mixed, false)
        });
        let ruled = eval::evaluate_hard(|c| {
            flux_names::parse_fallback_only(&hard_input(c), &[], LibraryKind::Mixed, true)
        });
        println!(
            "hunch alone: all {}/{} (title {} year {} season {} episodes {}); hunch + flux post-rules: all {}/{} (title {})\n",
            raw.all, raw.n, raw.title, raw.year, raw.season, raw.episodes, ruled.all, ruled.n, ruled.title
        );
        hard["hunch_only"] = serde_json::to_value(&raw)?;
        hard["hunch_plus_post_rules"] = serde_json::to_value(&ruled)?;
    }
    let p = a.out.join("flux-names-hard-cases.json");
    std::fs::write(
        &p,
        serde_json::to_string_pretty(&json!({
            "component": "flux-names", "name": "hard-cases", "parser_version": PARSER_VERSION,
            "created_unix": now(), "load_avg": load_avg(), "results": hard,
        }))?,
    )?;
    println!("wrote {}\n", p.display());

    if let Some(dir) = &a.guessit {
        guessit(&a, dir)?;
    } else {
        eprintln!("guessit fixtures not found (pass --guessit DIR); skipping");
    }

    let full = a.media.join("library.manifest.jsonl");
    if a.bench && !full.exists() {
        eprintln!("skipping the throughput bench: {} not present", full.display());
    } else if a.bench {
        bench(&a)?;
    }
    Ok(())
}

/// Out-of-distribution accuracy on guessit's test suite.
fn guessit(a: &Args, dir: &Path) -> Result<(), Box<dyn Error>> {
    use flux_names::eval::{evaluate_guessit, GuessitCase, GuessitReport, Split};
    let mut results = serde_json::Map::new();
    println!(
        "### guessit test suite (out of distribution; fixtures: {})\n",
        dir.display()
    );
    println!(
        "| file | split | parser | cases | title | year | season | episodes | type | all | fallback ran |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|");
    let pct = |c: &eval::Counter| format!("{:.1}% ({}/{})", c.pct(), c.ok, c.n);
    for file in ["movies", "episodes", "various"] {
        let cases = eval::load_guessit(&dir.join(format!("{file}.yml")))?;
        let prior = |c: &GuessitCase| match c.kind.as_deref() {
            Some("movie") => LibraryKind::Movies,
            Some("episode") => LibraryKind::Shows,
            _ => LibraryKind::Mixed,
        };
        for split in [Split::Dev, Split::HeldOut] {
            #[cfg_attr(not(feature = "hunch"), allow(unused_mut))]
            let mut rows: Vec<(&str, GuessitReport)> = vec![
                (
                    "flux (mixed library)",
                    evaluate_guessit(&cases, split, true, |c| {
                        parse_with(Path::new(&c.input), &[], LibraryKind::Mixed, opts(Fallback::Auto))
                    }),
                ),
                (
                    "flux (library kind = type)",
                    evaluate_guessit(&cases, split, false, |c| {
                        parse_with(Path::new(&c.input), &[], prior(c), opts(Fallback::Auto))
                    }),
                ),
                (
                    "flux fast path only (mixed)",
                    evaluate_guessit(&cases, split, true, |c| {
                        parse_with(
                            Path::new(&c.input),
                            &[],
                            LibraryKind::Mixed,
                            opts(Fallback::Never),
                        )
                    }),
                ),
            ];
            #[cfg(feature = "hunch")]
            if a.baseline {
                rows.push((
                    "hunch alone",
                    evaluate_guessit(&cases, split, true, |c| {
                        flux_names::parse_fallback_only(Path::new(&c.input), &[], LibraryKind::Mixed, false)
                    }),
                ));
            }
            for (name, r) in &rows {
                println!(
                    "| {file} | {split:?} | {name} | {} | {} | {} | {} | {} | {} | {} | {} |",
                    r.cases,
                    pct(&r.title),
                    pct(&r.year),
                    pct(&r.season),
                    pct(&r.episodes),
                    if r.kind.n > 0 { pct(&r.kind) } else { "—".into() },
                    pct(&r.all),
                    r.fallback_used
                );
                results.insert(format!("{file}/{split:?}/{name}"), serde_json::to_value(r)?);
            }
            if a.show_failures && split == Split::Dev {
                for f in &rows[0].1.failures {
                    println!(
                        "  - [{}] `{}` {:?}: expected {} got {}",
                        f.file, f.input, f.fields, f.expected, f.got
                    );
                }
            }
        }
    }
    let p = a.out.join("flux-names-guessit.json");
    std::fs::write(
        &p,
        serde_json::to_string_pretty(&json!({
            "component": "flux-names", "name": "guessit-fixtures", "parser_version": PARSER_VERSION,
            "fixtures": dir, "created_unix": now(), "load_avg": load_avg(),
            "note": "even-indexed cases = dev half (inspected), odd-indexed = held-out half",
            "results": results,
        }))?,
    )?;
    println!("\nwrote {}\n", p.display());
    Ok(())
}

fn print_hard(name: &str, r: &HardReport) {
    println!("### hard cases: {name}: all four fields {}/{}\n", r.all, r.n);
    println!("| input | title | year | season | episodes | ok (t/y/s/e) |\n|---|---|---|---|---|---|");
    for c in &r.cases {
        let eps = if c.episodes.is_empty() {
            c.absolute.map(|a| format!("abs {a}")).unwrap_or_default()
        } else {
            format!("{:?}", c.episodes)
        };
        let ok: String = c.ok.iter().map(|b| if *b { '✓' } else { '✗' }).collect();
        println!(
            "| `{}` | {} | {} | {} | {} | {ok} |",
            c.input,
            c.title.as_deref().unwrap_or("—"),
            c.year.map_or("—".into(), |y| y.to_string()),
            c.season.map_or("—".into(), |s| s.to_string()),
            eps
        );
    }
    println!(
        "\ntitle {}/{} · year {}/{} · season {}/{} · episodes {}/{}\n",
        r.title, r.n, r.year, r.n, r.season, r.n, r.episodes, r.n
    );
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

/// Single-thread throughput over every media file of the full library.
fn bench(a: &Args) -> Result<(), Box<dyn Error>> {
    let rows = eval::load_manifest(&a.media.join("library.manifest.jsonl"))?;
    let inputs = eval::inputs(&rows);
    let sibs: Vec<Vec<&str>> = inputs.iter().map(Input::sibling_refs).collect();
    let n = inputs.len();
    let load_before = load_avg();

    let time_mode = |fallback: Fallback, reps: usize| -> f64 {
        let mut rates = Vec::with_capacity(reps);
        for _ in 0..reps.max(1) {
            let t = Instant::now();
            for (i, s) in inputs.iter().zip(&sibs) {
                std::hint::black_box(parse_with(&i.rel, s, i.kind, opts(fallback)));
            }
            rates.push(n as f64 / t.elapsed().as_secs_f64());
        }
        median(&mut rates)
    };
    // Warm up lazy regexes and the hunch pipeline.
    for (i, s) in inputs.iter().zip(&sibs).take(50) {
        std::hint::black_box(parse_with(&i.rel, s, i.kind, opts(Fallback::Always)));
    }
    let fast = time_mode(Fallback::Never, a.reps);
    let auto = time_mode(Fallback::Auto, a.reps);
    let always = time_mode(Fallback::Always, 1);
    #[cfg(feature = "hunch")]
    let hunch_only = {
        let t = Instant::now();
        for (i, s) in inputs.iter().zip(&sibs) {
            std::hint::black_box(flux_names::parse_fallback_only(&i.rel, s, i.kind, false));
        }
        Some(n as f64 / t.elapsed().as_secs_f64())
    };
    #[cfg(not(feature = "hunch"))]
    let hunch_only: Option<f64> = None;

    // The 36 hard cases (filenames as given, no siblings), as in research/02 §6.2.
    let hard: Vec<PathBuf> = CASES.iter().map(|c| PathBuf::from(c.input)).collect();
    let hard_rate = |fallback: Fallback, reps: usize| -> f64 {
        let t = Instant::now();
        for _ in 0..reps {
            for p in &hard {
                std::hint::black_box(parse_with(p, &[], LibraryKind::Mixed, opts(fallback)));
            }
        }
        (reps * hard.len()) as f64 / t.elapsed().as_secs_f64()
    };
    let hard_fast = hard_rate(Fallback::Never, 2000);
    let hard_always = hard_rate(Fallback::Always, 20);

    let us = |rate: f64| 1e6 / rate;
    println!(
        "### throughput (single thread, {n} media paths of the full library, median of {} reps)\n",
        a.reps
    );
    println!("| mode | names/s | µs/name |\n|---|---|---|");
    println!("| fast path only | {fast:.0} | {:.2} |", us(fast));
    println!("| default (fallback when unsure) | {auto:.0} | {:.2} |", us(auto));
    println!(
        "| fallback always (fast + hunch + merge) | {always:.0} | {:.1} |",
        us(always)
    );
    if let Some(h) = hunch_only {
        println!("| hunch alone (with siblings) | {h:.0} | {:.1} |", us(h));
    }
    println!(
        "| hard cases, fast path only | {hard_fast:.0} | {:.2} |",
        us(hard_fast)
    );
    println!(
        "| hard cases, fallback always | {hard_always:.0} | {:.1} |\n",
        us(hard_always)
    );
    let load_after = load_avg();
    println!("load average: before {load_before}, after {load_after}\n");

    let out = json!({
        "component": "flux-names",
        "name": "throughput",
        "parser_version": PARSER_VERSION,
        "created_unix": now(),
        "threads": 1,
        "paths": n,
        "reps": a.reps,
        "load_avg_before": load_before,
        "load_avg_after": load_after,
        "names_per_s": {
            "fast_only": fast,
            "default_auto": auto,
            "fallback_always": always,
            "hunch_only": hunch_only,
            "hard_cases_fast_only": hard_fast,
            "hard_cases_fallback_always": hard_always,
        },
        "us_per_name": {
            "fast_only": us(fast),
            "default_auto": us(auto),
            "fallback_always": us(always),
            "hunch_only": hunch_only.map(us),
        },
        "command": "cargo run --release -p flux-names --bin names-eval",
    });
    let p = a.out.join("flux-names-throughput.json");
    std::fs::write(&p, serde_json::to_string_pretty(&out)?)?;
    println!("wrote {}", p.display());
    Ok(())
}
