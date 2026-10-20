//! Integration smoke run of the scan pipeline that flux-scan will own, on the
//! real corpus:
//!
//! ```text
//! Mille walk_scan ─► diff vs catalog snapshot (same_stat) ─► worker threads:
//!   flux-names parse (path + siblings + library kind) + flux-probe probe_file
//!   ─► flux-catalog Writer::apply (batches of 500)
//! then: per-file comparison with the manifest ground truth, and an unchanged
//! rescan that must produce an empty diff and zero writer commands.
//! ```
//!
//! This is glue to check that the components fit together, not the
//! orchestrator: no fingerprints (fp128/oshash), no move/missing handling, no
//! per-device I/O admission, no watcher. Two libraries are created the way a
//! user would: `Movies/` as a movie library and `TV/` as a shows library.
//!
//! ```sh
//! cargo run --release -p flux-scan --example pipeline_smoke -- [tiny|full] [--threads N]
//! ```
//!
//! Reads the corpus from `$FLUX_TEST_MEDIA` (skips without it). Writes
//! `$FLUX_BENCH_RESULTS/flux-<prefix>-pipeline-<lib>.json` (`<prefix>` =
//! `$FLUX_SMOKE_PREFIX`, default `integrate`; printed only when the
//! variable is unset). Catalogs go to `$FLUX_SMOKE_WORK` (default
//! `<temp dir>/flux-integrate`); the `cargo test` run uses a per-process
//! database there and removes it.

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use flux_catalog::group::normalize_title;
use flux_catalog::synth::{read_manifest, ManifestEntry};
use flux_catalog::{
    scan_support, ApplyOutcome, Catalog, CatalogConfig, FileFacts, FileRow, FileUpsert, ProbeUpdate,
    ScanBatch,
};
use flux_core::api::CreateLibrary;
use flux_core::LibraryKind;
use mille_core::{walk_scan, FileFilter, ReadBoundary, ReadBoundaryOptions, ScanOptions, Visit};
use serde_json::{json, Value};

/// Where catalogs are written: `$FLUX_SMOKE_WORK`, else
/// `<temp dir>/flux-integrate`.
fn work_dir() -> PathBuf {
    std::env::var_os("FLUX_SMOKE_WORK")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("flux-integrate"))
}

/// Video plus the sidecars flux-names can use as sibling evidence: the one
/// list in flux-core. The probe sniffs magic bytes, so names are only a
/// pre-filter.
fn walk_extensions() -> impl Iterator<Item = &'static str> {
    flux_core::VIDEO_EXTENSIONS
        .iter()
        .chain(flux_core::SIDECAR_EXTENSIONS)
        .copied()
}

type BoxResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// One file the walk delivered.
struct Seen {
    dir: PathBuf,
    name: String,
    facts: FileFacts,
}

fn main() -> BoxResult<()> {
    let mut lib = "tiny".to_string();
    let mut threads = 8usize;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--threads" => threads = args.next().ok_or("--threads N")?.parse()?,
            "tiny" | "full" => lib = a,
            other => return Err(format!("unknown argument {other:?}").into()),
        }
    }
    let Some(out) = run(&lib, threads, &format!("pipeline-{lib}"))? else {
        return Ok(());
    };
    let prefix = std::env::var("FLUX_SMOKE_PREFIX").unwrap_or_else(|_| "integrate".into());
    let text = serde_json::to_string_pretty(&out)?;
    println!("{text}");
    flux_test_media::write_result(&format!("flux-{prefix}-pipeline-{lib}.json"), &text);
    Ok(())
}

/// Scans `tiny` or `full` into a fresh catalog `{work_dir()}/{db_name}.db`.
/// `None` when the corpus is not mounted.
fn run(lib: &str, threads: usize, db_name: &str) -> BoxResult<Option<Value>> {
    let (root, manifest) = match lib {
        "full" => ("library", "library.manifest.jsonl"),
        _ => ("library-tiny", "library-tiny.manifest.jsonl"),
    };
    let Some(root) = flux_test_media::media(root) else {
        return Ok(None);
    };
    let root = root.to_string_lossy().into_owned();
    let truth = read_manifest(&flux_test_media::media_dir(manifest))?;

    let work = work_dir();
    std::fs::create_dir_all(&work)?;
    let db = work.join(format!("{db_name}.db"));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
    }
    let cat = Catalog::open(&db, CatalogConfig::default())?;
    let load_before = loadavg();
    let total_start = Instant::now();

    let mut per_library = Vec::new();
    for (folder, kind) in [("Movies", LibraryKind::Movies), ("TV", LibraryKind::Shows)] {
        let lib_root = PathBuf::from(&root).join(folder);
        let library = cat
            .writer()
            .create_library(CreateLibrary {
                name: folder.into(),
                kind,
                roots: vec![lib_root.to_string_lossy().into_owned()],
            })
            .wait()?;
        let root_id = cat.read_blocking(move |c| {
            Ok(scan_support::list_roots(c)?
                .into_iter()
                .find(|r| r.library_id == library.id)
                .map(|r| r.id))
        })?;
        let root_id = root_id.ok_or("library has no root")?;
        let entries: Vec<&ManifestEntry> = truth
            .iter()
            .filter(|e| e.path.starts_with(&format!("{folder}/")))
            .collect();
        let report = scan_library(
            &cat, library.id, root_id, &lib_root, kind, threads, folder, &entries,
        )?;
        per_library.push(report);
    }
    let total_ms = ms(total_start);

    Ok(Some(json!({
        "component": "integrate",
        "name": format!("pipeline-{lib}"),
        "library": root,
        "threads": threads,
        "loadavg_before": load_before,
        "loadavg_after": loadavg(),
        "total_ms": total_ms,
        "libraries": per_library,
        "note": "Warm page cache (corpus files are APFS clones read by earlier runs). No fingerprinting. Shared machine: re-run on a quiet one."
    })))
}

/// Runs with `cargo test -p flux-scan` (the example is `test = true`): the
/// whole pipeline on library-tiny must reproduce the ground truth and an
/// unchanged rescan must find nothing to do.
#[test]
fn library_tiny_end_to_end() {
    // Per process, so concurrent test runs don't share a database.
    let db_name = format!("pipeline-tiny-test-{}", std::process::id());
    let result = run("tiny", 4, &db_name);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(work_dir().join(format!("{db_name}.db{suffix}")));
    }
    let Some(out) = result.expect("pipeline runs") else {
        return;
    };
    for lib in out["libraries"].as_array().expect("libraries") {
        let name = &lib["library"];
        let gt = &lib["ground_truth"];
        assert_eq!(lib["probe_failures"], json!({}), "{name}: probe failures");
        assert_eq!(
            gt["files_correct"], gt["media_files_in_manifest"],
            "{name}: misplaced files {}",
            gt["wrong_by_class"]
        );
        for (kind, n) in gt["expected_items"].as_object().expect("expected") {
            let got = gt["catalog_items"].get(kind).cloned().unwrap_or(json!(0));
            assert_eq!(&got, n, "{name}: {kind} items");
        }
        assert_eq!(lib["rescan"]["files_changed"], 0, "{name}: rescan diff");
        assert_eq!(lib["rescan"]["writer_commands"], 0, "{name}: rescan writes");
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_library(
    cat: &Catalog,
    library_id: i64,
    root_id: i64,
    root: &Path,
    kind: LibraryKind,
    threads: usize,
    folder: &str,
    truth: &[&ManifestEntry],
) -> BoxResult<Value> {
    // 1. Walk (directories only; no media file is opened).
    let t = Instant::now();
    let (seen, unread) = walk(root, threads)?;
    let walk_ms = ms(t);

    // 2. Diff against the catalog snapshot.
    let t = Instant::now();
    let snapshot = cat.read_blocking(move |c| scan_support::load_root_snapshot(c, root_id))?;
    let mut siblings: HashMap<&Path, Vec<&str>> = HashMap::new();
    for s in &seen {
        siblings.entry(s.dir.as_path()).or_default().push(&s.name);
    }
    let todo: Vec<&Seen> = seen
        .iter()
        .filter(|s| is_media(s))
        .filter(|s| changed(&snapshot, &s.facts))
        .collect();
    let diff_ms = ms(t);

    // 3. Parse + probe on worker threads.
    let t = Instant::now();
    let next = AtomicUsize::new(0);
    let names_ns = AtomicU64::new(0);
    let probe_ns = AtomicU64::new(0);
    let reads = AtomicU64::new(0);
    let bytes = AtomicU64::new(0);
    let failures: Mutex<BTreeMap<String, Vec<String>>> = Mutex::new(BTreeMap::new());
    let done: Mutex<Vec<FileUpsert>> = Mutex::new(Vec::with_capacity(todo.len()));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut local = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(s) = todo.get(i) else { break };
                    let sibs: Vec<&str> = siblings
                        .get(s.dir.as_path())
                        .map(|v| v.iter().copied().filter(|n| *n != s.name).collect())
                        .unwrap_or_default();
                    let t = Instant::now();
                    let guess = flux_names::parse(Path::new(&s.facts.rel_path), &sibs, kind);
                    names_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    let t = Instant::now();
                    let probe = match flux_probe::probe_file(&s.dir.join(&s.name)) {
                        Ok(p) => {
                            reads.fetch_add(u64::from(p.reads), Ordering::Relaxed);
                            bytes.fetch_add(p.bytes_read, Ordering::Relaxed);
                            ProbeUpdate::Probed(Box::new(p))
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            let key = msg.split(':').next().unwrap_or("").to_string();
                            if let Ok(mut f) = failures.lock() {
                                f.entry(key).or_default().push(s.facts.rel_path.clone());
                            }
                            ProbeUpdate::Failed(msg)
                        }
                    };
                    probe_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    local.push(FileUpsert {
                        facts: s.facts.clone(),
                        probe,
                        guess: Some(guess),
                    });
                }
                if let Ok(mut d) = done.lock() {
                    d.append(&mut local);
                }
            });
        }
    });
    let upserts = done.into_inner().map_err(|_| "worker panicked")?;
    let work_ms = ms(t);

    // 4. Apply in batches.
    let t = Instant::now();
    let mut outcome = ApplyOutcome::default();
    let mut batch = Vec::with_capacity(500);
    for u in upserts {
        batch.push(u);
        if batch.len() == 500 {
            outcome.merge(apply(cat, root_id, std::mem::take(&mut batch))?);
        }
    }
    if !batch.is_empty() {
        outcome.merge(apply(cat, root_id, batch)?);
    }
    let apply_ms = ms(t);

    // 5. Ground truth.
    let verdict = compare(cat, library_id, root_id, folder, truth)?;

    // 6. Unchanged rescan: walk again, diff in memory, write nothing.
    let commands_before = cat.writer().stats().commands.load(Ordering::Relaxed);
    let t = Instant::now();
    let (seen2, _) = walk(root, threads)?;
    let snapshot = cat.read_blocking(move |c| scan_support::load_root_snapshot(c, root_id))?;
    let changed_on_rescan = seen2
        .iter()
        .filter(|s| is_media(s))
        .filter(|s| changed(&snapshot, &s.facts))
        .count();
    let rescan_ms = ms(t);
    let commands_after = cat.writer().stats().commands.load(Ordering::Relaxed);

    let probed = todo.len() as f64;
    let failures = failures.into_inner().map_err(|_| "worker panicked")?;
    Ok(json!({
        "library": folder,
        "kind": kind,
        "files_walked": seen.len(),
        "dirs_unread": unread,
        "media_files": todo.len(),
        "walk_ms": walk_ms,
        "diff_ms": diff_ms,
        "parse_probe_wall_ms": work_ms,
        "names_cpu_ms": names_ns.into_inner() as f64 / 1e6,
        "probe_wall_sum_ms": probe_ns.into_inner() as f64 / 1e6,
        "probe_mean_reads": reads.into_inner() as f64 / probed.max(1.0),
        "probe_mean_kib": bytes.into_inner() as f64 / 1024.0 / probed.max(1.0),
        "probe_failures": failures.iter().map(|(k, v)| (k.clone(), json!({"count": v.len(), "examples": v.iter().take(3).collect::<Vec<_>>()}))).collect::<serde_json::Map<_, _>>(),
        "apply_ms": apply_ms,
        "apply": {
            "files_added": outcome.files_added,
            "files_ignored": outcome.files_ignored,
            "files_pending": outcome.files_pending,
            "items_added": outcome.items_added.len(),
        },
        "first_scan_ms": walk_ms + diff_ms + work_ms + apply_ms,
        "ground_truth": verdict,
        "rescan": {
            "ms": rescan_ms,
            "files_changed": changed_on_rescan,
            "writer_commands": commands_after - commands_before,
        },
    }))
}

fn walk(root: &Path, threads: usize) -> BoxResult<(Vec<Seen>, usize)> {
    let reads = ReadBoundary::new(ReadBoundaryOptions {
        concurrency: threads,
        ..ReadBoundaryOptions::media()
    });
    let options = ScanOptions {
        parallelism: threads,
        file_filter: Some(FileFilter::extensions(walk_extensions())),
        ..ScanOptions::media()
    };
    let mut seen = Vec::new();
    let report = walk_scan(root, &options, &reads, |dir, e| {
        if e.is_file() {
            let abs = dir.join(e.name);
            let rel = abs
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            // Mille reports dev = ino = 0 where the platform has no inode
            // (Windows); the catalog's "unknown" is None.
            let known = e.dev != 0 || e.ino != 0;
            seen.push(Seen {
                dir: dir.path.to_path_buf(),
                name: e.name.to_string_lossy().into_owned(),
                facts: FileFacts {
                    rel_path: rel,
                    dev: known.then_some(e.dev),
                    ino: known.then_some(e.ino),
                    btime_ns: e.btime_ns,
                    size: e.size,
                    mtime_ns: e.mtime_ns,
                    fp128: None,
                    oshash: None,
                },
            });
        }
        Visit::Continue
    })?;
    Ok((seen, report.unread.len()))
}

fn is_media(s: &Seen) -> bool {
    flux_names::is_video_file(&s.name) && !flux_names::is_ignored(Path::new(&s.facts.rel_path))
}

fn changed(snapshot: &HashMap<String, FileRow>, f: &FileFacts) -> bool {
    snapshot.get(&f.rel_path).is_none_or(|row| !row.same_stat(f))
}

fn apply(cat: &Catalog, root_id: i64, upserts: Vec<FileUpsert>) -> BoxResult<ApplyOutcome> {
    Ok(cat
        .writer()
        .apply(ScanBatch {
            root_id,
            upserts,
            ..Default::default()
        })
        .wait()?)
}

/// One linked item of an asset, with its parent and grandparent.
#[derive(Debug, Clone)]
struct Linked {
    kind: String,
    title: String,
    year: Option<u16>,
    episode: Option<u16>,
    parent_title: Option<String>,
    parent_season: Option<u16>,
    grandparent_title: Option<String>,
}

/// Compares where every manifest file landed with the ground truth.
fn compare(
    cat: &Catalog,
    library_id: i64,
    root_id: i64,
    folder: &str,
    truth: &[&ManifestEntry],
) -> BoxResult<Value> {
    let snapshot = cat.read_blocking(move |c| scan_support::load_root_snapshot(c, root_id))?;
    let links: HashMap<i64, Vec<Linked>> = cat.read_blocking(|c| {
        let mut st = c.prepare(
            "SELECT ai.asset_id, i.kind, i.title, i.year, i.episode_number,
                    p.title, p.season_number, g.title
             FROM asset_item ai JOIN item i ON i.id = ai.item_id
             LEFT JOIN item p ON p.id = i.parent_id
             LEFT JOIN item g ON g.id = p.parent_id",
        )?;
        let mut map: HashMap<i64, Vec<Linked>> = HashMap::new();
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                Linked {
                    kind: r.get(1)?,
                    title: r.get(2)?,
                    year: r.get(3)?,
                    episode: r.get(4)?,
                    parent_title: r.get(5)?,
                    parent_season: r.get(6)?,
                    grandparent_title: r.get(7)?,
                },
            ))
        })?;
        for row in rows {
            let (asset, l) = row?;
            map.entry(asset).or_default().push(l);
        }
        Ok(map)
    })?;
    let counts: BTreeMap<String, i64> = cat.read_blocking(move |c| {
        let mut st = c.prepare(
            "SELECT kind || CASE WHEN match_state = 'unmatched' THEN ':unmatched' ELSE '' END, count(*)
             FROM item WHERE library_id = ?1 GROUP BY 1",
        )?;
        let rows = st.query_map([library_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut m = BTreeMap::new();
        for row in rows {
            let (k, n) = row?;
            m.insert(k, n);
        }
        Ok(m)
    })?;

    let norm = |s: Option<&str>| normalize_title(s.unwrap_or(""));
    let mut ok: BTreeMap<&str, u64> = BTreeMap::new();
    let mut wrong: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut fail = |class: String, e: &ManifestEntry, got: String| {
        wrong
            .entry(class)
            .or_default()
            .push(format!("{} → {got}", e.path));
    };
    for e in truth.iter().filter(|e| e.is_media()) {
        let rel = e.path.strip_prefix(&format!("{folder}/")).unwrap_or(&e.path);
        let Some(row) = snapshot.get(rel) else {
            fail("not walked".into(), e, "absent from catalog".into());
            continue;
        };
        let linked = row
            .asset_id
            .and_then(|a| links.get(&a))
            .cloned()
            .unwrap_or_default();
        let summary = || {
            linked
                .iter()
                .map(|l| {
                    format!(
                        "{} {:?} {:?} e{:?} p={:?} s{:?} g={:?}",
                        l.kind,
                        l.title,
                        l.year,
                        l.episode,
                        l.parent_title,
                        l.parent_season,
                        l.grandparent_title
                    )
                })
                .collect::<Vec<_>>()
                .join("; ")
        };
        match e.kind.as_str() {
            "sample" => {
                if linked.is_empty() {
                    *ok.entry("sample").or_default() += 1;
                } else {
                    fail("sample became an item".into(), e, summary());
                }
            }
            "movie" => match linked.as_slice() {
                [l] if l.kind == "movie"
                    && norm(Some(&l.title)) == norm(e.title.as_deref())
                    && l.year == e.year =>
                {
                    *ok.entry("movie").or_default() += 1;
                }
                [l] if l.kind == "movie" && norm(Some(&l.title)) == norm(e.title.as_deref()) => {
                    fail("movie: wrong year".into(), e, summary())
                }
                [l] if l.kind == "movie" => fail("movie: wrong title".into(), e, summary()),
                _ => fail("movie: wrong kind".into(), e, summary()),
            },
            "episode" => {
                let all_eps = !linked.is_empty() && linked.iter().all(|l| l.kind == "episode");
                let show_ok = linked
                    .iter()
                    .all(|l| norm(l.grandparent_title.as_deref()) == norm(e.show.as_deref()));
                let mut got: Vec<u16> = linked.iter().filter_map(|l| l.episode).collect();
                got.sort_unstable();
                let mut want = e.episodes.clone();
                want.sort_unstable();
                let season_ok = linked.iter().all(|l| l.parent_season == e.season);
                if !all_eps {
                    let class = if e.absolute.is_some() {
                        "episode (anime absolute): wrong kind"
                    } else {
                        "episode: wrong kind"
                    };
                    fail(class.into(), e, summary());
                } else if !show_ok {
                    fail("episode: wrong show".into(), e, summary());
                } else if !season_ok || got != want {
                    let class = if e.absolute.is_some() {
                        "episode (anime absolute): season/episode differ (by design, needs flux-meta)"
                    } else {
                        "episode: wrong season/episode"
                    };
                    fail(class.into(), e, summary());
                } else {
                    *ok.entry("episode").or_default() += 1;
                }
            }
            "extra" => match linked.as_slice() {
                [l] if l.kind == "extra"
                    && norm(l.parent_title.as_deref()) == norm(e.parent_title.as_deref()) =>
                {
                    *ok.entry("extra").or_default() += 1;
                }
                [l] if l.kind == "extra" => fail("extra: wrong parent".into(), e, summary()),
                [] => fail("extra: not placed (pending?)".into(), e, summary()),
                _ => fail("extra: wrong kind".into(), e, summary()),
            },
            _ => {}
        }
    }
    let media = truth.iter().filter(|e| e.is_media()).count();
    let correct: u64 = ok.values().sum();
    Ok(json!({
        "media_files_in_manifest": media,
        "files_correct": correct,
        "files_correct_pct": 100.0 * correct as f64 / media.max(1) as f64,
        "correct_by_kind": ok,
        "wrong_by_class": wrong.iter().map(|(k, v)| (k.clone(), json!({"count": v.len(), "examples": v.iter().take(5).collect::<Vec<_>>()}))).collect::<serde_json::Map<_, _>>(),
        "expected_items": expected_items(truth),
        "catalog_items": counts,
    }))
}

/// Distinct items the ground truth implies (same keys as flux-catalog's corpus test).
fn expected_items(truth: &[&ManifestEntry]) -> Value {
    use std::collections::HashSet;
    let mut movies = HashSet::new();
    let mut shows = HashSet::new();
    let mut seasons = HashSet::new();
    let mut episodes = HashSet::new();
    let mut extras = 0;
    for e in truth {
        match e.kind.as_str() {
            "movie" => {
                movies.insert(match e.tmdb_id {
                    Some(id) => format!("tmdb:{id}"),
                    None => format!(
                        "{}|{:?}",
                        normalize_title(e.title.as_deref().unwrap_or("")),
                        e.year
                    ),
                });
            }
            "episode" => {
                let show = format!(
                    "{}|{:?}",
                    normalize_title(e.show.as_deref().unwrap_or("")),
                    e.year
                );
                for ep in &e.episodes {
                    episodes.insert(format!("{show}|{:?}|{ep}", e.season));
                }
                seasons.insert(format!("{show}|{:?}", e.season));
                shows.insert(show);
            }
            "extra" => extras += 1,
            _ => {}
        }
    }
    json!({"movie": movies.len(), "show": shows.len(), "season": seasons.len(), "episode": episodes.len(), "extra": extras})
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn loadavg() -> String {
    std::process::Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}
