//! Catalog benchmarks (re-runnable; results JSON per scenario).
//!
//! ```text
//! cargo run --release -p flux-catalog --bin catalog-bench -- [--quick] [--runs 3] [--rows 100000]
//!     [--batch 500] [--out DIR] [--db-dir DIR] [--manifest FILE] [--library-dir DIR]
//! ```
//!
//! Defaults: results to `$FLUX_BENCH_RESULTS` (printed only when unset),
//! databases under the temp dir, the library and its manifest from
//! `$FLUX_TEST_MEDIA` (`library/`, `library.manifest.jsonl`; the library
//! scenarios skip without them).
//!
//! Scenarios (each writes `flux-catalog-<name>.json`):
//! - `apply-library`: apply the 16.7k-file messy library (media entries; names
//!   from the manifest ground truth, probes from the seed files, stat from the
//!   real files) through the full grouping path → files/s, total ms, grouping
//!   accuracy vs ground truth, DB size.
//! - `search-library`: 500 prefix searches over the library's real titles.
//! - `rescan-unchanged`: snapshot + in-memory diff of the same library → ms,
//!   files changed (0), writer transactions/commands issued (0).
//! - `insert-100k`: 100k synthetic files (80 % episodes, 20 % movies) → files/s.
//! - `snapshot-100k`: `load_root_snapshot` for 100k rows → ms.
//! - `reader-latency`: point queries (file by id; item_detail; search) from the
//!   reader pool while the writer applies another 100k-file diff → p50/p99/max.
//! - `dbsize`: bytes per 10k files for both corpora.
//!
//! `--phase c` runs the phase-C scenarios instead (each writes
//! `flux-catalog-c-<name>.json`; databases go to `--db-dir`, default
//! `<temp dir>/flux-catalog-bench/db`):
//! - `c-apply-library`: the library apply with and without job enqueueing.
//! - `c-browse-library` / `c-browse-100k`: 1k users' state rows, then every
//!   sort × filter (`items_page`, first page of 120 and the next of 500 via
//!   its cursor) plus continue-watching, home and show summaries: p50/p99
//!   idle and while the writer applies batches into another library.
//! - `c-apply-100k`: 100k synthetic files (4K/HDR on half the movies).
//! - `c-jobs`: enqueue / claim / finish throughput and claim latency.
//! - `c-dbsize`: database sizes, per table where SQLite's `dbstat` exists.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use flux_catalog::synth::{self, ManifestEntry};
use flux_catalog::{
    read, scan_support, Catalog, CatalogConfig, FileFacts, FileUpsert, ProbeUpdate, Reply, ScanBatch,
};
use flux_core::api::{CreateLibrary, Id};
use flux_core::{GuessKind, LibraryKind, NameGuess, ProbeResult};
use serde_json::{json, Value};

type R<T> = Result<T, Box<dyn Error>>;

struct Args {
    /// `None`: results are printed, not written.
    out: Option<PathBuf>,
    db_dir: PathBuf,
    manifest: PathBuf,
    library_dir: PathBuf,
    rows: usize,
    batch: usize,
    runs: usize,
    autocheckpoint: u32,
    phase: Option<String>,
    users: usize,
    rows_per_user: usize,
    samples: usize,
}

/// `$FLUX_TEST_MEDIA/<rel>`, or a path that never exists when it is unset
/// (the scenarios that need it then skip, naming the variable).
fn corpus(rel: &str) -> PathBuf {
    match std::env::var_os("FLUX_TEST_MEDIA").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d).join(rel),
        None => PathBuf::from("<FLUX_TEST_MEDIA unset>").join(rel),
    }
}

fn parse_args() -> R<Args> {
    let mut a = Args {
        out: std::env::var_os("FLUX_BENCH_RESULTS")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from),
        db_dir: std::env::temp_dir().join("flux-catalog-bench"),
        manifest: corpus("library.manifest.jsonl"),
        library_dir: corpus("library"),
        rows: 100_000,
        batch: 500,
        runs: 3,
        autocheckpoint: CatalogConfig::default().wal_autocheckpoint_pages,
        phase: None,
        users: 1000,
        rows_per_user: 200,
        samples: 30,
    };
    let mut db_dir_set = false;
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut v = || it.next().ok_or_else(|| format!("{k} needs a value"));
        match k.as_str() {
            "--out" => a.out = Some(v()?.into()),
            "--db-dir" => {
                a.db_dir = v()?.into();
                db_dir_set = true;
            }
            "--phase" => a.phase = Some(v()?),
            "--users" => a.users = v()?.parse()?,
            "--rows-per-user" => a.rows_per_user = v()?.parse()?,
            "--samples" => a.samples = v()?.parse()?,
            "--manifest" => a.manifest = v()?.into(),
            "--library-dir" => a.library_dir = v()?.into(),
            "--rows" => a.rows = v()?.parse()?,
            "--batch" => a.batch = v()?.parse()?,
            "--runs" => a.runs = v()?.parse()?,
            "--autocheckpoint" => a.autocheckpoint = v()?.parse()?,
            "--quick" => {
                a.rows = 10_000;
                a.runs = 1;
            }
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    if a.phase.is_some() && !db_dir_set {
        a.db_dir = std::env::temp_dir().join("flux-catalog-bench").join("db");
    }
    Ok(a)
}

fn min(v: &[f64]) -> f64 {
    v.iter().copied().fold(f64::INFINITY, f64::min)
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    if v.is_empty() {
        0.0
    } else {
        v[v.len() / 2]
    }
}

fn pct(sorted: &[Duration], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i].as_secs_f64() * 1e6
}

fn uptime() -> String {
    std::process::Command::new("uptime")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn env_info() -> Value {
    json!({
        "when_unix_ms": flux_catalog::now_ms(),
        "uptime": uptime(),
        "sqlite_version": rusqlite_version(),
        "host": format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
    })
}

fn rusqlite_version() -> String {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(sqlite_version_uncached).clone()
}

fn sqlite_version_uncached() -> String {
    // The catalog's own connection reports the bundled library version.
    let dir = std::env::temp_dir().join(format!("flux-catalog-ver-{}", std::process::id()));
    let ver = (|| -> R<String> {
        let c = Catalog::open(dir.join("v.db"), CatalogConfig::default())?;
        let v =
            c.read_blocking(|c| Ok(c.query_row("SELECT sqlite_version()", [], |r| r.get::<_, String>(0))?))?;
        c.close()?;
        Ok(v)
    })()
    .unwrap_or_default();
    let _ = std::fs::remove_dir_all(dir);
    ver
}

/// Writes `flux-catalog-<name>.json` to `out`, or prints it when there is
/// no results directory.
fn write_result(out: &Option<PathBuf>, name: &str, v: &Value) -> R<()> {
    let Some(out) = out else {
        println!("flux-catalog-{name}: {}", serde_json::to_string(v)?);
        return Ok(());
    };
    std::fs::create_dir_all(out)?;
    let p = out.join(format!("flux-catalog-{name}.json"));
    std::fs::write(&p, serde_json::to_vec_pretty(v)?)?;
    println!("wrote {}", p.display());
    Ok(())
}

fn fresh_db(dir: &Path, name: &str) -> R<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let p = dir.join(format!("{name}.db"));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", p.display()));
    }
    Ok(p)
}

fn file_sizes(db: &Path) -> (u64, u64) {
    let main = std::fs::metadata(db).map(|m| m.len()).unwrap_or(0);
    let wal = std::fs::metadata(format!("{}-wal", db.display()))
        .map(|m| m.len())
        .unwrap_or(0);
    (main, wal)
}

fn open(db: &Path, kind: LibraryKind, root: &str, autocheckpoint: u32) -> R<(Catalog, Id, Id)> {
    let cfg = CatalogConfig {
        wal_autocheckpoint_pages: autocheckpoint,
        ..CatalogConfig::default()
    };
    let cat = Catalog::open(db, cfg)?;
    let lib = cat
        .writer()
        .create_library(CreateLibrary {
            name: "Bench".into(),
            kind,
            roots: vec![root.into()],
        })
        .wait()?;
    let root_id = cat.read_blocking(|c| Ok(scan_support::list_roots(c)?[0].id))?;
    Ok((cat, lib.id, root_id))
}

/// Stream batches like a scanner: submit everything (bounded queue applies
/// backpressure), then collect the replies.
fn apply_all(
    cat: &Catalog,
    root: Id,
    upserts: Vec<FileUpsert>,
    batch: usize,
) -> R<(f64, flux_catalog::ApplyOutcome)> {
    let t = Instant::now();
    let mut replies: Vec<Reply<flux_catalog::ApplyOutcome>> = Vec::new();
    let mut it = upserts.into_iter().peekable();
    while it.peek().is_some() {
        let chunk: Vec<FileUpsert> = it.by_ref().take(batch).collect();
        replies.push(
            cat.writer()
                .apply(ScanBatch {
                    root_id: root,
                    upserts: chunk,
                    ..Default::default()
                })
                .submit()?,
        );
    }
    let mut total = flux_catalog::ApplyOutcome::default();
    for r in replies {
        total.merge(r.wait()?);
    }
    Ok((t.elapsed().as_secs_f64() * 1000.0, total))
}

// ---- corpus: the 16.7k-file library -------------------------------------------------

fn library_upserts(
    entries: &[ManifestEntry],
    seeds: &HashMap<String, ProbeResult>,
    lib_dir: &Path,
) -> Vec<FileUpsert> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.is_media())
        .map(|(i, e)| {
            let facts = std::fs::metadata(lib_dir.join(&e.path))
                .map(|md| synth::facts_from_metadata(&e.path, &md))
                .unwrap_or_else(|_| synthetic_facts(&e.path, i as u64));
            FileUpsert {
                facts,
                probe: synth::probe_for_entry(e, seeds)
                    .map(|p| ProbeUpdate::Probed(Box::new(p)))
                    .unwrap_or_default(),
                guess: synth::guess_from_manifest(e),
            }
        })
        .collect()
}

fn expected_counts(entries: &[ManifestEntry]) -> Value {
    let norm = flux_catalog::group::normalize_title;
    let (mut movies, mut shows, mut seasons, mut eps) =
        (HashSet::new(), HashSet::new(), HashSet::new(), HashSet::new());
    let mut extras = 0;
    for e in entries {
        match e.kind.as_str() {
            "movie" => {
                movies.insert(match e.tmdb_id {
                    Some(id) => format!("tmdb:{id}"),
                    None => format!("{}|{:?}", norm(e.title.as_deref().unwrap_or("")), e.year),
                });
            }
            "episode" => {
                let show = format!("{}|{:?}", norm(e.show.as_deref().unwrap_or("")), e.year);
                for ep in &e.episodes {
                    eps.insert(format!("{show}|{:?}|{ep}", e.season));
                }
                seasons.insert(format!("{show}|{:?}", e.season));
                shows.insert(show);
            }
            "extra" => extras += 1,
            _ => {}
        }
    }
    json!({"movie": movies.len(), "show": shows.len(), "season": seasons.len(), "episode": eps.len(), "extra": extras})
}

fn item_counts(cat: &Catalog) -> R<Value> {
    Ok(cat.read_blocking(|c| {
        let mut m = serde_json::Map::new();
        let mut stmt = c.prepare("SELECT kind, count(*) FROM item GROUP BY kind")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (k, n) = row?;
            m.insert(k, json!(n));
        }
        Ok(Value::Object(m))
    })?)
}

fn bench_library(a: &Args, results: &mut Value) -> R<()> {
    if !a.manifest.exists() {
        println!("skip apply-library: {} missing", a.manifest.display());
        return Ok(());
    }
    let entries = synth::read_manifest(&a.manifest)?;
    let seeds = synth::seed_probes();
    let upserts = library_upserts(&entries, &seeds, &a.library_dir);
    let n = upserts.len();
    println!(
        "apply-library: {n} media files ({} manifest lines)",
        entries.len()
    );
    let mut ms_runs = Vec::new();
    let mut last: Option<(Catalog, Id, PathBuf, flux_catalog::ApplyOutcome)> = None;
    for run in 0..a.runs {
        if let Some((c, _, _, _)) = last.take() {
            c.close()?;
        }
        let db = fresh_db(&a.db_dir, "library")?;
        let (cat, _lib, root) = open(
            &db,
            LibraryKind::Mixed,
            &a.library_dir.display().to_string(),
            a.autocheckpoint,
        )?;
        let (ms, out) = apply_all(&cat, root, upserts.clone(), a.batch)?;
        println!("  run {run}: {ms:.0} ms, {:.0} files/s", n as f64 / ms * 1000.0);
        ms_runs.push(ms);
        last = Some((cat, root, db, out));
    }
    let Some((cat, root, db, out)) = last else {
        return Ok(());
    };
    let txns = cat.writer().stats().transactions.load(Ordering::Relaxed);
    let rows = cat.writer().stats().rows.load(Ordering::Relaxed);
    let counts = item_counts(&cat)?;
    let expected = expected_counts(&entries);
    let counts_for_results = counts.clone();
    let med = median(&mut ms_runs.clone());
    results["apply-library"] = json!({
        "files": n,
        "batch_files": a.batch,
        "wal_autocheckpoint_pages": a.autocheckpoint,
        "runs_ms": ms_runs.clone(),
        "median_ms": med,
        "files_per_s": n as f64 / med * 1000.0,
        "best_files_per_s": n as f64 / min(&ms_runs) * 1000.0,
        "sql_rows_changed": rows,
        "transactions": txns,
        "outcome": {
            "files_added": out.files_added, "files_ignored": out.files_ignored, "files_pending": out.files_pending,
            "items_added": out.items_added.len(),
        },
        "items_by_kind": counts_for_results,
        "expected_from_ground_truth": expected,
        "env": env_info(),
    });
    write_result(&a.out, "apply-library", &results["apply-library"])?;

    // Search latency on real titles: 3-6 character prefixes of random manifest titles.
    let titles: Vec<String> = entries
        .iter()
        .filter_map(|e| e.title.clone().or_else(|| e.show.clone()))
        .collect();
    let mut lat: Vec<Duration> = Vec::new();
    let mut hits_total = 0usize;
    let mut k: u64 = 0x9e37_79b9_7f4a_7c15;
    for _ in 0..500 {
        k = k
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let t = &titles[(k >> 33) as usize % titles.len().max(1)];
        let n = 3 + ((k >> 20) % 4) as usize;
        let q: String = t.chars().take(n).collect();
        let start = Instant::now();
        let hits = cat.read_blocking(|c| read::search(c, &q, 20, None))?;
        lat.push(start.elapsed());
        hits_total += hits.len();
    }
    lat.sort();
    results["search-library"] = json!({
        "queries": lat.len(),
        "query_shape": "3-6 char prefix of a random real title",
        "mean_hits": hits_total as f64 / lat.len().max(1) as f64,
        "p50_us": pct(&lat, 0.50),
        "p99_us": pct(&lat, 0.99),
        "max_us": pct(&lat, 1.0),
        "items": counts,
        "env": env_info(),
    });
    write_result(&a.out, "search-library", &results["search-library"])?;

    // Unchanged rescan: snapshot + in-memory diff; nothing is sent, nothing written.
    cat.writer().flush().wait()?;
    let stats = cat.writer().stats();
    let (tx_before, cmd_before) = (
        stats.transactions.load(Ordering::Relaxed),
        stats.commands.load(Ordering::Relaxed),
    );
    let fresh = library_upserts(&entries, &seeds, &a.library_dir);
    let mut snap_ms = Vec::new();
    let mut diff_ms = Vec::new();
    let mut changed = 0usize;
    for _ in 0..a.runs.max(3) {
        let t = Instant::now();
        let snap = cat.read_blocking(|c| scan_support::load_root_snapshot(c, root))?;
        snap_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        let t = Instant::now();
        changed = fresh
            .iter()
            .filter(|u| {
                snap.get(&u.facts.rel_path)
                    .is_none_or(|row| !row.same_stat(&u.facts))
            })
            .count();
        let batch = ScanBatch {
            root_id: root,
            ..Default::default()
        };
        cat.writer().apply(batch).wait()?;
        diff_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let stats = cat.writer().stats();
    let (tx_after, cmd_after) = (
        stats.transactions.load(Ordering::Relaxed),
        stats.commands.load(Ordering::Relaxed),
    );
    results["rescan-unchanged"] = json!({
        "files": n,
        "snapshot_ms_min": min(&snap_ms),
        "snapshot_ms_median": median(&mut snap_ms),
        "diff_ms_min": min(&diff_ms),
        "diff_ms_median": median(&mut diff_ms),
        "files_changed": changed,
        "writer_transactions": tx_after - tx_before,
        "writer_commands": cmd_after - cmd_before,
        "note": "walk/stat time excluded (flux-scan's job); this is the catalog's share of a zero-change rescan",
        "env": env_info(),
    });
    write_result(&a.out, "rescan-unchanged", &results["rescan-unchanged"])?;
    cat.close()?;
    let (main, wal) = file_sizes(&db);
    results["dbsize"]["library"] = json!({
        "files": n, "db_bytes": main, "wal_bytes": wal,
        "bytes_per_10k_files": (main as f64) / (n as f64) * 10_000.0,
    });
    Ok(())
}

// ---- synthetic 100k --------------------------------------------------------------------

fn synthetic_facts(path: &str, i: u64) -> FileFacts {
    FileFacts {
        rel_path: path.to_string(),
        dev: Some(16_777_234),
        ino: Some(10_000_000 + i),
        btime_ns: Some(1_600_000_000_000_000_000 + i as i64),
        size: 1_000_000_000 + i * 7919,
        mtime_ns: 1_700_000_000_000_000_000 + i as i64,
        fp128: Some(u128::from(i).wrapping_mul(0x9e37_79b9_7f4a_7c15_f39c_c060_5ced_c834) ^ 0xabcdef),
        oshash: Some(i.wrapping_mul(0x9e37_79b9_7f4a_7c15)),
    }
}

/// `n` files: every 5th a movie (pairs of versions), the rest episodes of
/// 80-episode shows (4 seasons × 20). `tag` keeps runs distinct.
fn synthetic_upserts(
    n: usize,
    tag: &str,
    seeds: &HashMap<String, ProbeResult>,
    ino_base: u64,
) -> Vec<FileUpsert> {
    let seed_names: Vec<&String> = {
        let mut v: Vec<&String> = seeds.keys().collect();
        v.sort();
        v
    };
    let mut ep_idx = 0usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let mut g = NameGuess::unknown();
        g.confidence = 0.95;
        let path = if i % 5 == 0 {
            let m = i / 5;
            let film = m / 2;
            let year = 1950 + (film % 70) as u16;
            g.kind = GuessKind::Movie;
            g.title = Some(format!("{tag} Film {film}"));
            g.year = Some(year);
            g.resolution = Some(if m % 2 == 0 { "2160p" } else { "1080p" }.into());
            format!(
                "Movies/{tag} Film {film} ({year})/{tag} Film {film} ({year}) - {}.mkv",
                g.resolution.as_deref().unwrap_or("")
            )
        } else {
            let show = ep_idx / 80;
            let season = (ep_idx % 80) / 20 + 1;
            let ep = ep_idx % 20 + 1;
            ep_idx += 1;
            g.kind = GuessKind::Episode;
            g.title = Some(format!("{tag} Show {show}"));
            g.year = Some(2000 + (show % 25) as u16);
            g.season = Some(season as u16);
            g.episodes = vec![ep as u16];
            format!("TV/{tag} Show {show}/Season {season:02}/{tag} Show {show} - S{season:02}E{ep:02} - Episode Title {i}.mkv")
        };
        let probe = seeds.get(seed_names[i % seed_names.len()].as_str()).cloned();
        out.push(FileUpsert {
            facts: synthetic_facts(&path, ino_base + i as u64),
            probe: probe
                .map(|p| ProbeUpdate::Probed(Box::new(p)))
                .unwrap_or_default(),
            guess: Some(g),
        });
    }
    out
}

fn bench_100k(a: &Args, results: &mut Value) -> R<(Catalog, Id, PathBuf)> {
    let seeds = synth::seed_probes();
    let upserts = synthetic_upserts(a.rows, "Syn", &seeds, 0);
    println!("insert-100k: {} synthetic files", upserts.len());
    let mut ms_runs = Vec::new();
    let mut last = None;
    for run in 0..a.runs {
        if let Some((c, _, _)) = last.take() {
            let c: Catalog = c;
            c.close()?;
        }
        let db = fresh_db(&a.db_dir, "synthetic")?;
        let (cat, _lib, root) = open(&db, LibraryKind::Mixed, "/synthetic", a.autocheckpoint)?;
        let (ms, _out) = apply_all(&cat, root, upserts.clone(), a.batch)?;
        println!(
            "  run {run}: {ms:.0} ms, {:.0} files/s",
            a.rows as f64 / ms * 1000.0
        );
        ms_runs.push(ms);
        last = Some((cat, root, db));
    }
    let Some((cat, root, db)) = last else {
        return Err("no runs".into());
    };
    let med = median(&mut ms_runs.clone());
    let rows = cat.writer().stats().rows.load(Ordering::Relaxed);
    let txns = cat.writer().stats().transactions.load(Ordering::Relaxed);
    results["insert-100k"] = json!({
        "files": a.rows,
        "batch_files": a.batch,
        "wal_autocheckpoint_pages": a.autocheckpoint,
        "runs_ms": ms_runs.clone(),
        "median_ms": med,
        "files_per_s": a.rows as f64 / med * 1000.0,
        "best_files_per_s": a.rows as f64 / min(&ms_runs) * 1000.0,
        "sql_rows_changed_last_run": rows,
        "sql_rows_per_s": rows as f64 / med * 1000.0,
        "transactions_last_run": txns,
        "items_by_kind": item_counts(&cat)?,
        "env": env_info(),
    });
    write_result(&a.out, "insert-100k", &results["insert-100k"])?;

    // Snapshot load for 100k rows + in-memory diff against an unchanged enumeration.
    let mut load = Vec::new();
    let mut diff = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let snap = cat.read_blocking(|c| scan_support::load_root_snapshot(c, root))?;
        load.push(t.elapsed().as_secs_f64() * 1000.0);
        let t = Instant::now();
        let changed = upserts
            .iter()
            .filter(|u| snap.get(&u.facts.rel_path).is_none_or(|r| !r.same_stat(&u.facts)))
            .count();
        diff.push(t.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(changed, 0);
    }
    results["snapshot-100k"] = json!({
        "rows": a.rows,
        "load_ms_runs": load.clone(),
        "load_ms_min": min(&load),
        "load_ms_median": median(&mut load),
        "diff_ms_min": min(&diff),
        "diff_ms_median": median(&mut diff),
        "research_baseline_ms": "25-32 (4-column rows, research/02 §7.1)",
        "env": env_info(),
    });
    write_result(&a.out, "snapshot-100k", &results["snapshot-100k"])?;
    cat.writer().flush().wait()?;
    let _ = cat.read_blocking(|c| Ok(c.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))?));
    let (main, wal) = file_sizes(&db);
    results["dbsize"]["synthetic"] = json!({
        "files": a.rows, "db_bytes": main, "wal_bytes": wal,
        "bytes_per_10k_files": (main as f64) / (a.rows as f64) * 10_000.0,
    });
    Ok((cat, root, db))
}

// ---- readers under write load -------------------------------------------------------------

struct Sampler {
    name: &'static str,
    lat: Vec<Duration>,
}

fn bench_readers(a: &Args, results: &mut Value, cat: Catalog, root: Id) -> R<()> {
    let seeds = synth::seed_probes();
    let (max_file, max_item): (Id, Id) = cat.read_blocking(|c| {
        Ok((
            c.query_row("SELECT max(id) FROM file", [], |r| r.get(0))?,
            c.query_row("SELECT max(id) FROM item", [], |r| r.get(0))?,
        ))
    })?;
    let run_readers = |cat: &Catalog, stop: Arc<AtomicBool>| -> Vec<std::thread::JoinHandle<Sampler>> {
        let kinds: [&'static str; 3] = ["file_by_id", "item_detail", "search_prefix"];
        kinds
            .into_iter()
            .map(|name| {
                let pool = cat.readers().clone();
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut lat = Vec::new();
                    let mut k: u64 = 0x2545_f491_4f6c_dd1d;
                    let words = ["syn", "film 1", "show 2", "episode", "film 99", "show 7"];
                    while !stop.load(Ordering::Relaxed) {
                        k = k
                            .wrapping_mul(6364136223846793005)
                            .wrapping_add(1442695040888963407);
                        let t = Instant::now();
                        let r = pool.with(|c| match name {
                            "file_by_id" => {
                                let id = (k >> 33) as i64 % max_file + 1;
                                c.query_row("SELECT rel_path FROM file WHERE id = ?1", [id], |r| {
                                    r.get::<_, String>(0)
                                })
                                .map(|_| ())
                                .or_else(|e| {
                                    if e == rusqlite::Error::QueryReturnedNoRows {
                                        Ok(())
                                    } else {
                                        Err(e.into())
                                    }
                                })
                            }
                            "item_detail" => {
                                read::item_detail(c, (k >> 33) as i64 % max_item + 1, None).map(|_| ())
                            }
                            _ => {
                                read::search(c, words[(k >> 40) as usize % words.len()], 20, None).map(|_| ())
                            }
                        });
                        lat.push(t.elapsed());
                        if let Err(e) = r {
                            eprintln!("reader {name}: {e}");
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    lat.sort();
                    Sampler { name, lat }
                })
            })
            .collect()
    };
    let summarize = |samples: Vec<Sampler>| -> Value {
        let mut m = serde_json::Map::new();
        for s in samples {
            m.insert(
                s.name.into(),
                json!({
                    "samples": s.lat.len(),
                    "p50_us": pct(&s.lat, 0.50),
                    "p99_us": pct(&s.lat, 0.99),
                    "max_us": pct(&s.lat, 1.0),
                }),
            );
        }
        Value::Object(m)
    };

    // Idle baseline (2 s).
    let stop = Arc::new(AtomicBool::new(false));
    let hs = run_readers(&cat, stop.clone());
    std::thread::sleep(Duration::from_secs(2));
    stop.store(true, Ordering::Relaxed);
    let idle = summarize(hs.into_iter().filter_map(|h| h.join().ok()).collect());

    // Under load: the writer applies another `rows`-file diff.
    let more = synthetic_upserts(a.rows, "Load", &seeds, 50_000_000);
    let stop = Arc::new(AtomicBool::new(false));
    let hs = run_readers(&cat, stop.clone());
    let (write_ms, _) = apply_all(&cat, root, more, a.batch)?;
    stop.store(true, Ordering::Relaxed);
    let loaded = summarize(hs.into_iter().filter_map(|h| h.join().ok()).collect());
    let plan = cat.read_blocking(|c| {
        let mut stmt = c.prepare(
            "EXPLAIN QUERY PLAN SELECT i.id FROM item i WHERE i.state <> 'missing' AND i.kind IN ('movie','show')
             AND i.parent_id IS NULL AND i.library_id = 1 AND (i.sort_title, i.id) > ('m', 0) ORDER BY i.sort_title, i.id LIMIT 101",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(3))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })?;
    results["reader-latency"] = json!({
        "writer_files": a.rows,
        "writer_ms": write_ms,
        "writer_files_per_s": a.rows as f64 / write_ms * 1000.0,
        "idle": idle,
        "during_write": loaded,
        "items_page_query_plan": plan,
        "research_baseline": "file_by_id p50 20-36 µs, p99 0.11-0.92 ms during 200k inserts (research/02 §7.1)",
        "env": env_info(),
    });
    write_result(&a.out, "reader-latency", &results["reader-latency"])?;
    cat.close()?;
    Ok(())
}

fn main() -> R<()> {
    let a = parse_args()?;
    println!("load: {}", uptime());
    if a.phase.as_deref() == Some("c") {
        return phase_c::run(&a);
    }
    let mut results = json!({"dbsize": {}});
    bench_library(&a, &mut results)?;
    let (cat, root, _db) = bench_100k(&a, &mut results)?;
    bench_readers(&a, &mut results, cat, root)?;
    results["dbsize"]["env"] = env_info();
    write_result(&a.out, "dbsize", &results["dbsize"])?;
    println!("{}", serde_json::to_string_pretty(&results)?);
    Ok(())
}

// ---- phase C ----------------------------------------------------------------------------

mod phase_c {
    use super::*;
    use flux_catalog::jobs;
    use flux_catalog::{ClaimJobs, ItemSort, ItemsQuery, JobResult, JobSpec, WriterConfig};
    use flux_core::api::ItemFilter;

    const SORTS: [ItemSort; 6] = [
        ItemSort::Title,
        ItemSort::TitleDesc,
        ItemSort::Year,
        ItemSort::YearDesc,
        ItemSort::Added,
        ItemSort::AddedDesc,
    ];
    const FILTERS: [Option<ItemFilter>; 5] = [
        None,
        Some(ItemFilter::Unwatched),
        Some(ItemFilter::InProgress),
        Some(ItemFilter::Uhd),
        Some(ItemFilter::Hdr),
    ];
    const NOTE: &str = "Shared machine (M1 Max, another project's dev server and other agents running): \
                        latencies include CPU and SSD contention. Compare runs at similar load only.";

    fn wire<T: serde::Serialize>(v: &T) -> String {
        serde_json::to_value(v)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    }

    fn loadavg() -> String {
        std::process::Command::new("sysctl")
            .args(["-n", "vm.loadavg"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    }

    fn env() -> Value {
        let mut e = env_info();
        e["loadavg_1_5_15"] = json!(loadavg());
        e["note"] = json!(NOTE);
        e
    }

    fn summary(mut v: Vec<Duration>) -> Value {
        v.sort();
        json!({"n": v.len(), "p50_us": pct(&v, 0.50), "p99_us": pct(&v, 0.99), "max_us": pct(&v, 1.0)})
    }

    fn write(a: &Args, name: &str, v: &Value) -> R<()> {
        write_result(&a.out, &format!("c-{name}"), v)
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 17
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
    }

    fn open_cfg(db: &Path, enqueue_jobs: bool, autocheckpoint: u32) -> R<Catalog> {
        Ok(Catalog::open(
            db,
            CatalogConfig {
                wal_autocheckpoint_pages: autocheckpoint,
                writer: WriterConfig {
                    enqueue_jobs,
                    ..WriterConfig::default()
                },
                ..CatalogConfig::default()
            },
        )?)
    }

    fn library(cat: &Catalog, name: &str, root: &str) -> R<(Id, Id)> {
        let lib = cat
            .writer()
            .create_library(CreateLibrary {
                name: name.into(),
                kind: LibraryKind::Mixed,
                roots: vec![root.into()],
            })
            .wait()?;
        let root_id = cat.read_blocking(move |c| {
            Ok(scan_support::list_roots(c)?
                .into_iter()
                .find(|r| r.library_id == lib.id)
                .map(|r| r.id))
        })?;
        Ok((lib.id, root_id.ok_or("no root")?))
    }

    fn job_counts(cat: &Catalog) -> R<Value> {
        let rows = cat.read_blocking(jobs::job_counts)?;
        Ok(json!(rows
            .iter()
            .map(|r| (format!("{}:{}", r.kind, r.state), r.count))
            .collect::<std::collections::BTreeMap<_, _>>()))
    }

    /// 100k-style synthetic files with half the movies 4K HDR10.
    fn synthetic(n: usize, tag: &str, ino_base: u64) -> Vec<FileUpsert> {
        let seeds = synth::seed_probes();
        let mut v = synthetic_upserts(n, tag, &seeds, ino_base);
        for u in &mut v {
            let uhd = u.guess.as_ref().and_then(|g| g.resolution.as_deref()) == Some("2160p");
            if let (true, ProbeUpdate::Probed(p)) = (uhd, &mut u.probe) {
                for t in p
                    .tracks
                    .iter_mut()
                    .filter(|t| t.kind == flux_core::TrackKind::Video)
                {
                    t.width = Some(3840);
                    t.height = Some(2160);
                    t.color = Some(flux_core::ColorInfo {
                        primaries: Some(9),
                        transfer: Some(16),
                        matrix: Some(9),
                        full_range: Some(false),
                    });
                }
            }
        }
        v
    }

    /// `users` viewers, each with `per_user` state rows: runs of consecutive
    /// episodes watched (binge) with the next one in progress, and single
    /// movies watched or started. Direct SQL in the writer (setup, not timed
    /// as API calls).
    fn seed_users(cat: &Catalog, users: usize, per_user: usize, seed: u64) -> R<(Vec<Id>, Value)> {
        let t = Instant::now();
        let leaves: Vec<(Id, String)> = cat.read_blocking(|c| {
            let mut st = c.prepare(
                "SELECT id, kind FROM item WHERE kind IN ('movie','episode') AND state <> 'missing' ORDER BY id",
            )?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        let first_user: Id = cat
            .writer()
            .exec(move |c| {
                let now = flux_catalog::now_ms();
                let base: Id = c.query_row("SELECT COALESCE(max(id), 0) FROM user", [], |r| r.get(0))?;
                let mut st = c.prepare(
                    "INSERT INTO user(id, login_name, display_name, role, created_at) VALUES (?1, ?2, ?2, 'viewer', ?3)",
                )?;
                for i in 0..users {
                    let id = base + 1 + i as i64;
                    st.execute(rusqlite::params![id, format!("viewer{id}@example.com"), now])?;
                }
                Ok(base + 1)
            })
            .wait()?;
        let ids: Vec<Id> = (0..users as i64).map(|i| first_user + i).collect();
        let mut rng = Rng(seed);
        let mut rows: Vec<(Id, Id, i64, bool, i64)> = Vec::with_capacity(users * per_user);
        let now = flux_catalog::now_ms();
        for &u in &ids {
            let mut n = 0;
            while n < per_user && !leaves.is_empty() {
                let start = rng.below(leaves.len());
                if leaves[start].1 == "episode" {
                    let run = 3 + rng.below(12);
                    for k in 0..run.min(per_user - n) {
                        let Some((id, _)) = leaves.get(start + k) else {
                            break;
                        };
                        let last = k + 1 == run.min(per_user - n);
                        let at = now - rng.below(30 * 86_400_000) as i64;
                        rows.push((u, *id, if last { 600_000 } else { 0 }, !last, at));
                        n += 1;
                    }
                } else {
                    let started = rng.below(4) == 0;
                    let at = now - rng.below(30 * 86_400_000) as i64;
                    rows.push((
                        u,
                        leaves[start].0,
                        if started { 1_200_000 } else { 0 },
                        !started,
                        at,
                    ));
                    n += 1;
                }
            }
        }
        let total = rows.len();
        for chunk in rows.chunks(20_000) {
            let chunk = chunk.to_vec();
            cat.writer()
                .exec(move |c| {
                    let mut st = c.prepare(
                        "INSERT INTO user_state(user_id, item_id, position_ms, duration_ms, watched, play_count, last_played_at, updated_at)
                         VALUES (?1, ?2, ?3, 2400000, ?4, ?5, ?6, ?6)
                         ON CONFLICT(user_id, item_id) DO NOTHING",
                    )?;
                    for (u, i, pos, watched, at) in chunk {
                        st.execute(rusqlite::params![u, i, pos, watched, i64::from(watched), at])?;
                    }
                    Ok(())
                })
                .wait()?;
        }
        cat.writer().flush().wait()?;
        Ok((
            ids,
            json!({"users": users, "state_rows": total, "leaf_items": leaves.len(), "setup_ms": t.elapsed().as_secs_f64() * 1e3}),
        ))
    }

    /// Keeps the writer busy: synthetic batches into another library until
    /// `stop`. Returns `(files, ms)`.
    fn write_load(cat: &Catalog, root: Id, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<(usize, f64)> {
        let w = cat.writer().clone();
        std::thread::spawn(move || {
            let t = Instant::now();
            let mut files = 0usize;
            let mut round = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let ups = synthetic(2_000, &format!("Load{round}"), 90_000_000 + round * 10_000);
                let mut replies = Vec::new();
                for chunk in ups.chunks(500) {
                    match w
                        .apply(ScanBatch {
                            root_id: root,
                            upserts: chunk.to_vec(),
                            ..Default::default()
                        })
                        .submit()
                    {
                        Ok(r) => replies.push(r),
                        Err(_) => return (files, t.elapsed().as_secs_f64() * 1e3),
                    }
                }
                for r in replies {
                    if r.wait().is_ok() {
                        files += 500;
                    }
                }
                round += 1;
            }
            (files, t.elapsed().as_secs_f64() * 1e3)
        })
    }

    /// Every sort × filter: first page (120) and the next (500) per sample,
    /// each with a random user, on `threads` reader threads.
    fn browse(cat: &Catalog, lib: Id, users: &[Id], samples: usize, threads: usize) -> Value {
        let combos: Vec<(ItemSort, Option<ItemFilter>)> = SORTS
            .iter()
            .flat_map(|s| FILTERS.iter().map(move |f| (*s, *f)))
            .collect();
        let users = Arc::new(users.to_vec());
        let parts: Vec<Vec<(ItemSort, Option<ItemFilter>)>> = (0..threads)
            .map(|t| combos.iter().skip(t).step_by(threads).copied().collect())
            .collect();
        let handles: Vec<_> = parts
            .into_iter()
            .enumerate()
            .map(|(ti, part)| {
                let pool = cat.readers().clone();
                let users = users.clone();
                std::thread::spawn(move || {
                    let mut rng = Rng(0x5eed + ti as u64);
                    let mut out = Vec::new();
                    for (sort, filter) in part {
                        let (mut p1, mut p2) = (Vec::new(), Vec::new());
                        let (mut total, mut n1) = (0u64, 0usize);
                        let mut errors = 0;
                        for _ in 0..samples {
                            let user = users[rng.below(users.len())];
                            let mut q = ItemsQuery {
                                library_id: Some(lib),
                                sort,
                                filter,
                                user_id: Some(user),
                                limit: 120,
                                ..Default::default()
                            };
                            let t = Instant::now();
                            let page = pool.with(|c| read::items_page(c, &q));
                            p1.push(t.elapsed());
                            let Ok(page) = page else {
                                errors += 1;
                                continue;
                            };
                            total = page.total.unwrap_or(0);
                            n1 = page.items.len();
                            if let Some(next) = page.next {
                                q.cursor = Some(next);
                                q.limit = 500;
                                let t = Instant::now();
                                if pool.with(|c| read::items_page(c, &q)).is_err() {
                                    errors += 1;
                                }
                                p2.push(t.elapsed());
                            }
                        }
                        out.push(json!({
                            "sort": wire(&sort),
                            "filter": filter.map(|f| wire(&f)).unwrap_or_else(|| "none".into()),
                            "total_last": total,
                            "first_page_items": n1,
                            "page1_120": summary(p1),
                            "page2_500": summary(p2),
                            "errors": errors,
                        }));
                    }
                    out
                })
            })
            .collect();
        let mut all: Vec<Value> = handles
            .into_iter()
            .filter_map(|h| h.join().ok())
            .flatten()
            .collect();
        all.sort_by_key(|v| {
            (
                v["sort"].as_str().unwrap_or("").to_string(),
                v["filter"].as_str().unwrap_or("").to_string(),
            )
        });
        let worst = |key: &str, p: &str| {
            all.iter()
                .filter_map(|v| v[key][p].as_f64())
                .fold(0.0f64, f64::max)
        };
        json!({
            "combos": all,
            "worst_page1_p50_us": worst("page1_120", "p50_us"),
            "worst_page1_p99_us": worst("page1_120", "p99_us"),
            "worst_page2_p99_us": worst("page2_500", "p99_us"),
        })
    }

    /// Continue watching, home and show summaries (unwatched counts) for
    /// random users.
    fn other_reads(cat: &Catalog, users: &[Id], samples: usize) -> R<Value> {
        let shows: Vec<Id> = cat.read_blocking(|c| {
            let mut st =
                c.prepare("SELECT id FROM item WHERE kind = 'show' AND state <> 'missing' LIMIT 2000")?;
            let rows = st.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<Id>>>()?)
        })?;
        let mut rng = Rng(77);
        let (mut cw, mut home, mut show, mut seasons) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut cw_items = 0usize;
        for _ in 0..samples * 4 {
            let u = users[rng.below(users.len())];
            let t = Instant::now();
            cw_items += cat.read_blocking(|c| read::continue_watching(c, u, 20))?.len();
            cw.push(t.elapsed());
            let t = Instant::now();
            cat.read_blocking(|c| read::home(c, Some(u), 20))?;
            home.push(t.elapsed());
            if !shows.is_empty() {
                let s = shows[rng.below(shows.len())];
                let t = Instant::now();
                cat.read_blocking(|c| read::item_summary(c, s, Some(u)))?;
                show.push(t.elapsed());
                let t = Instant::now();
                cat.read_blocking(|c| read::children(c, s, Some(u)))?;
                seasons.push(t.elapsed());
            }
        }
        Ok(json!({
            "continue_watching_20": summary(cw),
            "continue_watching_mean_items": cw_items as f64 / (samples * 4) as f64,
            "home_20": summary(home),
            "show_summary_with_unwatched_count": summary(show),
            "show_children_seasons": summary(seasons),
        }))
    }

    fn plans(cat: &Catalog, lib: Id, user: Id) -> R<Value> {
        let mut m = serde_json::Map::new();
        for sort in SORTS {
            for filter in FILTERS {
                let q = ItemsQuery {
                    library_id: Some(lib),
                    sort,
                    filter,
                    user_id: Some(user),
                    ..Default::default()
                };
                let p = cat.read_blocking(move |c| read::items_page_plan(c, &q))?;
                m.insert(
                    format!(
                        "{}|{}",
                        wire(&sort),
                        filter.map(|f| wire(&f)).unwrap_or_else(|| "none".into())
                    ),
                    json!(p),
                );
            }
        }
        Ok(Value::Object(m))
    }

    /// Browse idle and under write load, plus the other reads.
    fn browse_suite(a: &Args, cat: &Catalog, lib: Id, users: &[Id]) -> R<Value> {
        let load_before = loadavg();
        let idle = browse(cat, lib, users, a.samples, 2);
        let idle_other = other_reads(cat, users, a.samples)?;
        let (_load_lib, load_root) = library(cat, "Load", "/load")?;
        let stop = Arc::new(AtomicBool::new(false));
        let h = write_load(cat, load_root, stop.clone());
        std::thread::sleep(Duration::from_millis(200));
        let tx0 = cat.writer().stats().transactions.load(Ordering::Relaxed);
        let busy = browse(cat, lib, users, a.samples, 2);
        let busy_other = other_reads(cat, users, a.samples)?;
        let tx1 = cat.writer().stats().transactions.load(Ordering::Relaxed);
        stop.store(true, Ordering::Relaxed);
        let (files, ms) = h.join().map_err(|_| "load thread panicked")?;
        Ok(json!({
            "idle": {"items_page": idle, "other": idle_other},
            "during_writes": {
                "items_page": busy,
                "other": busy_other,
                "writer": {"files_applied": files, "ms": ms, "files_per_s": files as f64 / ms * 1e3,
                           "transactions_during_reads": tx1 - tx0},
            },
            "plans": plans(cat, lib, users[0])?,
            "loadavg_before": load_before,
            "env": env(),
        }))
    }

    fn sizes(cat: &Catalog, db: &Path) -> R<Value> {
        // Checkpoint from a separate connection (not inside the writer's
        // transaction) so the main file holds everything.
        cat.writer().flush().wait()?;
        {
            let c = rusqlite::Connection::open(db)?;
            c.busy_timeout(Duration::from_secs(5))?;
            let _ = c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        }
        let (main, wal) = file_sizes(db);
        let tables: Value = cat
            .read_blocking(|c| {
                let r =
                    c.prepare("SELECT name, sum(pgsize) FROM dbstat GROUP BY name ORDER BY 2 DESC LIMIT 30");
                let Ok(mut st) = r else {
                    return Ok(json!("dbstat not compiled in"));
                };
                let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
                let m: serde_json::Map<String, Value> =
                    rows.filter_map(|r| r.ok()).map(|(k, v)| (k, json!(v))).collect();
                Ok(Value::Object(m))
            })
            .unwrap_or_else(|_| json!("unavailable"));
        let counts: Value = cat.read_blocking(|c| {
            let mut m = serde_json::Map::new();
            for t in [
                "file",
                "asset",
                "item",
                "user_state",
                "job",
                "art",
                "item_folder",
                "asset_index",
            ] {
                let n: i64 = c.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))?;
                m.insert(t.into(), json!(n));
            }
            Ok(Value::Object(m))
        })?;
        Ok(json!({"db_bytes": main, "wal_bytes": wal, "rows": counts, "bytes_by_table": tables}))
    }

    fn apply_runs(
        a: &Args,
        upserts: &[FileUpsert],
        name: &str,
        jobs: bool,
        root_path: &str,
    ) -> R<(Vec<f64>, Catalog, PathBuf, Id, Id)> {
        let mut ms_runs = Vec::new();
        let mut last = None;
        for run in 0..a.runs {
            if let Some((c, _, _, _)) = last.take() {
                let c: Catalog = c;
                c.close()?;
            }
            let db = fresh_db(&a.db_dir, name)?;
            let cat = open_cfg(&db, jobs, a.autocheckpoint)?;
            let (lib, root) = library(&cat, "Bench", root_path)?;
            let (ms, _) = apply_all(&cat, root, upserts.to_vec(), a.batch)?;
            println!(
                "  {name} jobs={jobs} run {run}: {ms:.0} ms ({:.0} files/s)",
                upserts.len() as f64 / ms * 1e3
            );
            ms_runs.push(ms);
            last = Some((cat, db, lib, root));
        }
        let (cat, db, lib, root) = last.ok_or("no runs")?;
        Ok((ms_runs, cat, db, lib, root))
    }

    fn apply_summary(n: usize, runs: &[f64]) -> Value {
        let med = median(&mut runs.to_vec());
        json!({"runs_ms": runs, "median_ms": med, "files_per_s": n as f64 / med * 1e3,
               "best_files_per_s": n as f64 / min(runs) * 1e3})
    }

    fn jobs_bench(a: &Args) -> R<Value> {
        let db = fresh_db(&a.db_dir, "jobs")?;
        let cat = open_cfg(&db, true, a.autocheckpoint)?;
        let w = cat.writer();
        let n = 20_000usize;
        let t = Instant::now();
        for chunk in (0..n).collect::<Vec<_>>().chunks(1_000) {
            let specs = chunk
                .iter()
                .map(|i| JobSpec::new("bench", format!("k{i}")).priority((*i % 3) as i64 * 10))
                .collect();
            w.enqueue_jobs(specs).wait()?;
        }
        let enqueue_ms = t.elapsed().as_secs_f64() * 1e3;
        // Re-enqueue everything: all duplicates.
        let t = Instant::now();
        for chunk in (0..n).collect::<Vec<_>>().chunks(1_000) {
            let specs = chunk
                .iter()
                .map(|i| JobSpec::new("bench", format!("k{i}")))
                .collect();
            w.enqueue_jobs(specs).wait()?;
        }
        let dedupe_ms = t.elapsed().as_secs_f64() * 1e3;
        let mut claim_lat = Vec::new();
        let mut finish_lat = Vec::new();
        let t = Instant::now();
        let mut done = 0usize;
        loop {
            let t1 = Instant::now();
            let leases = w
                .claim_jobs(ClaimJobs::new("bench", &["bench"], 32, Duration::from_secs(60)))
                .wait()?;
            claim_lat.push(t1.elapsed());
            if leases.is_empty() {
                break;
            }
            for l in leases {
                let t2 = Instant::now();
                w.finish_job(l.id, l.attempt, JobResult::Done).wait()?;
                finish_lat.push(t2.elapsed());
                done += 1;
            }
        }
        let drain_ms = t.elapsed().as_secs_f64() * 1e3;
        cat.close()?;
        Ok(json!({
            "jobs": n,
            "enqueue_ms": enqueue_ms, "enqueue_per_s": n as f64 / enqueue_ms * 1e3,
            "re_enqueue_duplicates_ms": dedupe_ms,
            "drain_claim32_finish_ms": drain_ms, "drained": done,
            "jobs_per_s_claim_finish_sequential": done as f64 / drain_ms * 1e3,
            "claim_32": summary(claim_lat),
            "finish_1": summary(finish_lat),
            "note": "one client thread, each command waits for its commit (worst case; a pipelined executor batches)",
            "env": env(),
        }))
    }

    pub fn run(a: &Args) -> R<()> {
        std::fs::create_dir_all(&a.db_dir)?;
        let mut dbsize = serde_json::Map::new();
        // ---- the 16.7k-file library ----
        if a.manifest.exists() {
            let entries = synth::read_manifest(&a.manifest)?;
            let seeds = synth::seed_probes();
            let upserts = library_upserts(&entries, &seeds, &a.library_dir);
            let n = upserts.len();
            println!("c-apply-library: {n} media files");
            let load = loadavg();
            let (off_runs, off_cat, _, _, _) = apply_runs(
                a,
                &upserts,
                "c-library-nojobs",
                false,
                &a.library_dir.display().to_string(),
            )?;
            off_cat.close()?;
            let (on_runs, cat, db, lib, _root) = apply_runs(
                a,
                &upserts,
                "c-library",
                true,
                &a.library_dir.display().to_string(),
            )?;
            let counts = item_counts(&cat)?;
            let after_apply = sizes(&cat, &db)?;
            dbsize.insert(
                "library_after_apply".into(),
                json!({
                    "files": n,
                    "bytes_per_10k_files": after_apply["db_bytes"].as_f64().unwrap_or(0.0) / n as f64 * 1e4,
                    "sizes": after_apply,
                }),
            );
            write(
                a,
                "apply-library",
                &json!({
                    "files": n, "batch_files": a.batch,
                    "with_jobs": apply_summary(n, &on_runs),
                    "without_jobs": apply_summary(n, &off_runs),
                    "items_by_kind": counts,
                    "expected_from_ground_truth": expected_counts(&entries),
                    "jobs": job_counts(&cat)?,
                    "loadavg_before": load,
                    "env": env(),
                }),
            )?;
            let (users, seeded) = seed_users(&cat, a.users, a.rows_per_user, 1)?;
            println!("c-browse-library: {seeded}");
            let mut b = browse_suite(a, &cat, lib, &users)?;
            b["users"] = seeded;
            write(a, "browse-library", &b)?;
            dbsize.insert(
                "library_after_browse".into(),
                json!({"files": n, "after": "apply + 1k users' state + the write-load library", "sizes": sizes(&cat, &db)?}),
            );
            cat.close()?;
        }
        // ---- 100k synthetic ----
        let ups = synthetic(a.rows, "Syn", 0);
        println!("c-apply-100k: {} files", ups.len());
        let load = loadavg();
        let (runs, cat, db, lib, _root) = apply_runs(a, &ups, "c-synthetic", true, "/synthetic")?;
        let size_after_apply = sizes(&cat, &db)?;
        dbsize.insert(
            "synthetic_after_apply".into(),
            json!({
                "files": ups.len(),
                "bytes_per_10k_files": size_after_apply["db_bytes"].as_f64().unwrap_or(0.0) / ups.len() as f64 * 1e4,
                "sizes": size_after_apply.clone(),
            }),
        );
        write(
            a,
            "apply-100k",
            &json!({
                "files": ups.len(), "batch_files": a.batch,
                "with_jobs": apply_summary(ups.len(), &runs),
                "items_by_kind": item_counts(&cat)?,
                "jobs": job_counts(&cat)?,
                "sizes": size_after_apply,
                "loadavg_before": load,
                "env": env(),
            }),
        )?;
        let (users, seeded) = seed_users(&cat, a.users, a.rows_per_user, 2)?;
        println!("c-browse-100k: {seeded}");
        let mut b = browse_suite(a, &cat, lib, &users)?;
        b["users"] = seeded;
        write(a, "browse-100k", &b)?;
        dbsize.insert(
            "synthetic_after_browse".into(),
            json!({"files": ups.len(), "after": "apply + 1k users' state + the write-load library", "sizes": sizes(&cat, &db)?}),
        );
        cat.close()?;
        // ---- jobs ----
        write(a, "jobs", &jobs_bench(a)?)?;
        dbsize.insert("env".into(), env());
        write(a, "dbsize", &Value::Object(dbsize))?;
        Ok(())
    }
}
