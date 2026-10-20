//! `flux-scan-bench`: flux-scan on the Jellyfin baseline corpus, with
//! tools/bench/jellyfin_scan_bench.py's methodology.
//!
//! Two libraries as in the Jellyfin run: "Movies" (`<corpus>/Movies`,
//! movies) and "TV" (`<corpus>/TV`, shows). Per corpus (`library-tiny`,
//! `library`):
//!
//! 1. **original** (the read-only corpus): a cold scan into a fresh catalog
//!    with the page cache as found, the same again (warm), the same with the
//!    fingerprint computed up front (`fingerprint_later = false`, the phase-C
//!    behaviour) and with fingerprinting off (what fp128's tail read costs),
//!    then an unchanged rescan. Cold scans report when the scan finished and
//!    when the background fingerprint pass did, the process's RSS and the
//!    WAL size once idle, and `GET /libraries`-style listing cost (uncached
//!    vs the writer-invalidated cache).
//! 2. **fresh clone** (a new APFS clone of the corpus under `--work`, a
//!    plain copy off macOS; a clone shares the original's blocks on disk but
//!    not its page-cache pages, which macOS keeps per file, so this is
//!    close to an OS-cold run; the SSD's own cache may still be warm): cold
//!    scan, unchanged rescan, rescan after
//!    adding 50 movies (as Jellyfin's touch rescan), rescan after touching
//!    50 existing files, watcher latency (an episode copied in → visible),
//!    and startup catch-up: Quick scan vs Mille resume snapshot.
//!
//! Every scan phase runs in its own child process, so peak RSS and CPU
//! seconds (getrusage) are per phase. Ground-truth placement is checked
//! after each cold scan. The corpus is `$FLUX_TEST_MEDIA/<corpus>`; results go
//! to `$FLUX_BENCH_RESULTS/flux-scan-<corpus>.json` (printed only when the
//! variable is unset).
//!
//! ```sh
//! nice -n 19 flux-scan-bench [tiny|full|both] [--workers N] [--keep] [--out-prefix P] [--work DIR]
//! ```
//!
//! `--out-prefix` names the result files (`<P>-<corpus>.json`, default
//! `flux-scan`); `--work` is where catalogs and the clone go (default
//! `<temp dir>/flux-scan-bench`).

#[path = "../../tests/support/truth.rs"]
mod truth;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use flux_catalog::synth::{read_manifest, ManifestEntry};
use flux_catalog::{scan_support, Catalog, CatalogConfig};
use flux_core::api::{CreateLibrary, Event, Id};
use flux_core::LibraryKind;
use flux_scan::{Priority, ScanConfig, ScanMode, ScanSummary, Scanner, ScannerOptions, StartupCatchUp};
use serde_json::{json, Value};
use truth::BoxResult;

/// A directory from an environment variable, if set.
fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Copies `src` to `dst`: an APFS clone (`cp -c`/`-cR`) on macOS, `cp -a`
/// on other Unix systems.
fn clone_path(src: &Path, dst: &Path, tree: bool) -> BoxResult<()> {
    let flag = match (cfg!(target_os = "macos"), tree) {
        (true, true) => "-cR",
        (true, false) => "-c",
        (false, _) => "-a",
    };
    if Command::new("cp").arg(flag).arg(src).arg(dst).status()?.success() {
        Ok(())
    } else {
        Err(format!("cp {flag} {} {} failed", src.display(), dst.display()).into())
    }
}

const LIBS: [(&str, LibraryKind); 2] = [("Movies", LibraryKind::Movies), ("TV", LibraryKind::Shows)];

fn main() -> BoxResult<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--phase") {
        return child(&args[1..]);
    }
    let mut which = "both".to_string();
    let mut workers: Option<usize> = None;
    let mut keep = false;
    let mut prefix = "flux-scan".to_string();
    let mut work = std::env::temp_dir().join("flux-scan-bench");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "tiny" | "full" | "both" => which = a.clone(),
            "--workers" => workers = Some(it.next().ok_or("--workers N")?.parse()?),
            "--keep" => keep = true,
            "--out-prefix" => prefix = it.next().ok_or("--out-prefix P")?.clone(),
            "--work" => work = PathBuf::from(it.next().ok_or("--work DIR")?),
            other => return Err(format!("unknown argument {other:?}").into()),
        }
    }
    let corpora: Vec<&str> = match which.as_str() {
        "tiny" => vec!["library-tiny"],
        "full" => vec!["library"],
        _ => vec!["library-tiny", "library"],
    };
    let Some(media) = env_dir("FLUX_TEST_MEDIA") else {
        eprintln!("skipping: FLUX_TEST_MEDIA is not set (the corpus to scan)");
        return Ok(());
    };
    let results = env_dir("FLUX_BENCH_RESULTS");
    if let Some(r) = &results {
        std::fs::create_dir_all(r)?;
    } else {
        eprintln!("FLUX_BENCH_RESULTS is not set: results are printed, not written");
    }
    let stamp = Command::new("date")
        .arg("+%Y%m%d-%H%M%S")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    for c in corpora {
        let out = run_corpus(&media, results.as_deref(), c, workers, keep, &work)?;
        let text = serde_json::to_string_pretty(&out)?;
        let Some(results) = &results else {
            println!("{text}");
            continue;
        };
        // The latest run, plus every run kept (the machine is shared:
        // compare runs, not single numbers).
        for path in [
            results.join(format!("{prefix}-{c}.json")),
            results.join(format!("{prefix}-{c}-{stamp}.json")),
        ] {
            std::fs::write(&path, &text)?;
            eprintln!("wrote {}", path.display());
        }
    }
    Ok(())
}

// ---- parent: orchestration ------------------------------------------------------

fn run_corpus(
    media: &Path,
    results: Option<&Path>,
    corpus: &str,
    workers: Option<usize>,
    keep: bool,
    work_dir: &Path,
) -> BoxResult<Value> {
    let src = media.join(corpus);
    if !src.exists() {
        return Err(format!("{} not present", src.display()).into());
    }
    let manifest = media.join(format!("{corpus}.manifest.jsonl"));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let work = work_dir.join(format!("{corpus}-{stamp}"));
    std::fs::create_dir_all(&work)?;
    let mut phases = BTreeMap::new();
    let w = workers.map(|n| n.to_string()).unwrap_or_default();
    let phase = |name: &str, root: &Path, db: &Path| -> BoxResult<Value> {
        eprintln!("[{corpus}] {name} …");
        let out = Command::new(std::env::current_exe()?)
            .args(["--phase", name])
            .arg(root)
            .arg(db)
            .arg(&manifest)
            .arg(&w)
            .output()?;
        if !out.status.success() {
            return Err(format!("phase {name} failed: {}", String::from_utf8_lossy(&out.stderr)).into());
        }
        let v: Value = serde_json::from_slice(&out.stdout)?;
        eprintln!("[{corpus}] {name}: {} ms", v["wall_ms"]);
        Ok(v)
    };

    // 1. The original corpus (read-only use): a cold scan as the page cache
    //    happens to be, the same again (now warm), and an unchanged rescan.
    let db = work.join("original.db");
    phases.insert("original_cold_scan_first", phase("cold", &src, &db)?);
    phases.insert("original_cold_scan_warm", phase("cold", &src, &db)?);
    // The fingerprint up front, before each file is committed (phase C).
    phases.insert(
        "original_cold_scan_inline_fingerprint",
        phase("cold-inline", &src, &db)?,
    );
    // The same without fp128 (ScanConfig::fingerprint = false): what the
    // fingerprint's tail read costs.
    phases.insert(
        "original_cold_scan_no_fingerprint",
        phase("cold-nofp", &src, &db)?,
    );
    phases.insert("original_unchanged_rescan", phase("rescan", &src, &db)?);

    // 2. A fresh APFS clone.
    let clone = work.join("clone");
    let t = Instant::now();
    clone_path(&src, &clone, true)?;
    let clone_ms = t.elapsed().as_millis() as u64;
    let db = work.join("clone.db");
    phases.insert("clone_cold_scan", phase("cold", &clone, &db)?);
    phases.insert("clone_unchanged_rescan", phase("rescan", &clone, &db)?);
    let added = add_movies(&clone, 50)?;
    phases.insert("clone_add50_rescan", phase("rescan", &clone, &db)?);
    let touched = touch_files(&clone, 50)?;
    phases.insert("clone_touch50_rescan", phase("rescan", &clone, &db)?);
    phases.insert("clone_watch_latency", phase("watch", &clone, &db)?);
    phases.insert("clone_startup_catchup", phase("startup", &clone, &db)?);

    let jellyfin = results.map_or(Value::Null, |r| jellyfin_baseline(r, corpus));
    if !keep {
        let _ = std::fs::remove_dir_all(&work);
    }
    Ok(json!({
        "component": "scan",
        "corpus": src,
        "machine": format!(
            "{} {}; machines shared with other work give indicative numbers, re-run on a quiet one",
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
        "page_cache": "no purge without sudo. original_cold_scan_first runs with the cache as found, original_cold_scan_warm right after it. The clone runs use a fresh APFS clone: same blocks on disk, but macOS caches pages per file, so its first scan reads from the device (the SSD's own cache may hold the shared blocks).",
        "workers": workers.unwrap_or_else(|| ScanConfig::default().workers),
        "clone_ms": clone_ms,
        "mutations": {"added_movies": added, "touched_files": touched},
        "phases": phases,
        "jellyfin_baseline": jellyfin,
    }))
}

/// Jellyfin's touch rescan: 50 new movie folders, each a copy of the first
/// `.mkv` under Movies/ (here an APFS clone: no data copied).
fn add_movies(root: &Path, n: usize) -> BoxResult<usize> {
    let seed = find_first(&root.join("Movies"), "mkv").ok_or("no .mkv seed")?;
    for i in 0..n {
        let dir = root
            .join("Movies")
            .join(format!("Bench Added Movie {i:03} (2026)"));
        std::fs::create_dir_all(&dir)?;
        let dst = dir.join(format!("Bench Added Movie {i:03} (2026).mkv"));
        clone_path(&seed, &dst, false)?;
    }
    Ok(n)
}

/// Bump the mtime of `n` media files spread over TV/ (content unchanged).
fn touch_files(root: &Path, n: usize) -> BoxResult<usize> {
    let mut files = Vec::new();
    collect_videos(&root.join("TV"), &mut files);
    files.sort();
    let step = (files.len() / n.max(1)).max(1);
    let picked: Vec<&PathBuf> = files.iter().step_by(step).take(n).collect();
    for f in &picked {
        if !Command::new("touch").arg(f).status()?.success() {
            return Err("touch failed".into());
        }
    }
    Ok(picked.len())
}

fn collect_videos(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name.starts_with('@') {
            continue;
        }
        if p.is_dir() {
            collect_videos(&p, out);
        } else if flux_names::is_video_file(&name) && !name.to_ascii_lowercase().contains("sample") {
            out.push(p);
        }
    }
}

fn find_first(dir: &Path, ext: &str) -> Option<PathBuf> {
    let mut v = Vec::new();
    collect_videos(dir, &mut v);
    v.sort();
    v.into_iter()
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some(ext))
}

fn jellyfin_baseline(results: &Path, corpus: &str) -> Value {
    let Ok(rd) = std::fs::read_dir(results) else {
        return Value::Null;
    };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().unwrap_or_default().to_string_lossy();
            let rest = n.strip_prefix(&format!("jellyfin-scan-{corpus}-")).unwrap_or("");
            // `library-` must not pick up `library-tiny-…`.
            rest.starts_with(|c: char| c.is_ascii_digit())
        })
        .collect();
    files.sort();
    let Some(f) = files.last() else { return Value::Null };
    let Ok(v) = std::fs::read(f).map(|b| serde_json::from_slice::<Value>(&b).unwrap_or(Value::Null)) else {
        return Value::Null;
    };
    let run = |k: &str| {
        let r = &v["runs"][k];
        json!({"wall_s": r["wall_s"], "peak_rss_mb": r["peak_rss_mb"]})
    };
    json!({
        "file": f,
        "cold_scan": run("cold_scan"),
        "unchanged_rescan": run("unchanged_rescan"),
        "touch_rescan": run("touch_rescan"),
        "db_bytes": v["db_bytes"],
        "idle_rss_mb": v["idle_rss_mb"],
    })
}

// ---- child: one measured phase ---------------------------------------------------

struct Env {
    root: PathBuf,
    db: PathBuf,
    manifest: PathBuf,
    workers: Option<usize>,
    no_fingerprint: bool,
    inline_fingerprint: bool,
}

fn child(args: &[String]) -> BoxResult<()> {
    let [name, root, db, manifest, workers, ..] = args else {
        return Err("--phase NAME ROOT DB MANIFEST WORKERS".into());
    };
    let mut env = Env {
        root: root.into(),
        db: db.into(),
        manifest: manifest.into(),
        workers: workers.parse().ok(),
        no_fingerprint: false,
        inline_fingerprint: false,
    };
    let load_before = loadavg();
    let t = Instant::now();
    let mut v = match name.as_str() {
        "cold" => phase_scan(&env, true)?,
        "cold-nofp" => {
            env.no_fingerprint = true;
            phase_scan(&env, true)?
        }
        "cold-inline" => {
            env.inline_fingerprint = true;
            phase_scan(&env, true)?
        }
        "rescan" => phase_scan(&env, false)?,
        "watch" => phase_watch(&env)?,
        "startup" => phase_startup(&env)?,
        other => return Err(format!("unknown phase {other}").into()),
    };
    let obj = v.as_object_mut().ok_or("phase output")?;
    obj.insert("wall_ms".into(), json!(t.elapsed().as_millis() as u64));
    obj.insert("loadavg_before".into(), json!(load_before));
    obj.insert("loadavg_after".into(), json!(loadavg()));
    obj.insert("rusage".into(), rusage());
    println!("{}", serde_json::to_string(&v)?);
    Ok(())
}

fn config(env: &Env) -> ScanConfig {
    let mut c = ScanConfig::default();
    if let Some(w) = env.workers {
        c.workers = w;
    }
    if env.no_fingerprint || std::env::var_os("FLUX_SCAN_BENCH_NO_FP").is_some() {
        c.fingerprint = false;
    }
    if env.inline_fingerprint {
        c.fingerprint_later = false;
    }
    c
}

type Libs = Vec<(String, Id, Id)>;

/// Open (or create, with both libraries) the catalog.
fn open(env: &Env, fresh: bool) -> BoxResult<(Arc<Catalog>, Libs)> {
    if fresh {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", env.db.display()));
        }
    }
    let cat = Arc::new(Catalog::open(&env.db, CatalogConfig::default())?);
    if fresh {
        for (folder, kind) in LIBS {
            cat.writer()
                .create_library(CreateLibrary {
                    name: folder.into(),
                    kind,
                    roots: vec![env.root.join(folder).to_string_lossy().into_owned()],
                })
                .wait()?;
        }
    }
    let roots = cat.read_blocking(scan_support::list_roots)?;
    let libs = cat.read_blocking(flux_catalog::read::list_libraries)?;
    let mut out = Vec::new();
    for (folder, _) in LIBS {
        let lib = libs.iter().find(|l| l.name == folder).ok_or("library")?;
        let root = roots.iter().find(|r| r.library_id == lib.id).ok_or("root")?;
        out.push((folder.to_string(), lib.id, root.id));
    }
    Ok((cat, out))
}

fn summary_json(s: &ScanSummary) -> Value {
    serde_json::to_value(s).unwrap_or(Value::Null)
}

fn close(cat: Arc<Catalog>) -> BoxResult<()> {
    let cat = Arc::try_unwrap(cat).map_err(|_| "catalog still shared")?;
    cat.close()?;
    Ok(())
}

fn phase_scan(env: &Env, fresh: bool) -> BoxResult<Value> {
    let (cat, libs) = open(env, fresh)?;
    let rss_open = rusage()["max_rss_mb"].clone();
    let scanner = Scanner::with_options(
        cat.clone(),
        config(env),
        ScannerOptions {
            startup: StartupCatchUp::QuickScan,
            ..ScannerOptions::default()
        },
    );
    let stats = || {
        let s = cat.writer().stats();
        (
            s.commands.load(std::sync::atomic::Ordering::Relaxed),
            s.transactions.load(std::sync::atomic::Ordering::Relaxed),
        )
    };
    let (c0, t0) = stats();
    // Time to the first browsable item: the first commit that adds items.
    let mut rx = scanner.subscribe();
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let t = Instant::now();
    let first_item = {
        let done = done.clone();
        std::thread::spawn(move || loop {
            match rx.try_recv() {
                Ok(Event::ItemsChanged { added, .. }) if !added.is_empty() => {
                    return Some(t.elapsed().as_secs_f64() * 1e3)
                }
                Ok(_) => {}
                Err(_) if done.load(std::sync::atomic::Ordering::Relaxed) => return None,
                Err(_) => std::thread::sleep(Duration::from_micros(200)),
            }
        })
    };
    // Like Jellyfin's "scan all libraries": both requested at once (same
    // device, same priority: admitted one after the other).
    let handles: Vec<_> = libs
        .iter()
        .map(|(_, lib, _)| scanner.scan_library(*lib, ScanMode::Quick, Priority::Scheduled))
        .collect();
    let mut per = BTreeMap::new();
    for ((folder, _, _), h) in libs.iter().zip(handles) {
        per.insert(folder.clone(), summary_json(&h.wait()?));
    }
    let scan_ms = t.elapsed().as_millis() as u64;
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let first_item_ms = first_item.join().ok().flatten();
    // The background fingerprint pass (ScanConfig::fingerprint_later).
    let fp_idle = scanner.wait_fingerprints(Duration::from_secs(1800));
    let fingerprints_done_ms = t.elapsed().as_millis() as u64;
    let (c1, t1) = stats();
    let (rows, with_fp): (i64, i64) = cat.read_blocking(|c| {
        Ok(c.query_row(
            "SELECT count(*), count(fp128) FROM file WHERE state <> 'missing'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    })?;
    // Idle footprint: the writer's idle maintenance has run (1 s), caches
    // are back to their idle size, the WAL truncated.
    std::thread::sleep(Duration::from_secs(3));
    let rss_idle = rss_now_mb();
    let wal_bytes = std::fs::metadata(format!("{}-wal", env.db.display()))
        .map(|m| m.len())
        .unwrap_or(0);
    // GET /libraries: counting every file per call vs the cached count.
    let time_list = |cached: bool| -> BoxResult<Value> {
        let mut v = Vec::new();
        for _ in 0..100 {
            let t = Instant::now();
            if cached {
                let counts = cat.file_counts().clone();
                cat.read_blocking(|c| flux_catalog::read::list_libraries_cached(c, &counts))?;
            } else {
                cat.read_blocking(flux_catalog::read::list_libraries)?;
            }
            v.push(t.elapsed().as_secs_f64() * 1e3);
        }
        Ok(json!({"p50_ms": median(&mut v), "max_ms": v.last().copied().unwrap_or(0.0)}))
    };
    let list_libraries = json!({"uncached": time_list(false)?, "cached": time_list(true)?});
    // What a watcher batch loads: the whole root's rows vs one show folder's.
    let watcher_snapshot = {
        let tv_root = libs.iter().find(|l| l.0 == "TV").ok_or("TV")?.2;
        let show = cat.read_blocking(move |c| {
            Ok(c.query_row(
                "SELECT substr(rel_path, 1, instr(rel_path, '/') - 1) FROM file
                  WHERE root_id = ?1 AND instr(rel_path, '/') > 0 ORDER BY rel_path LIMIT 1",
                [tv_root],
                |r| r.get::<_, String>(0),
            )?)
        })?;
        let time = |subtree: bool| -> BoxResult<Value> {
            let mut v = Vec::new();
            let mut rows = 0;
            for _ in 0..20 {
                let show = show.clone();
                let t = Instant::now();
                rows = if subtree {
                    cat.read_blocking(move |c| scan_support::load_subtree_snapshot(c, tv_root, &show))?
                        .len()
                } else {
                    cat.read_blocking(move |c| scan_support::load_root_snapshot(c, tv_root))?
                        .len()
                };
                v.push(t.elapsed().as_secs_f64() * 1e3);
            }
            Ok(json!({"p50_ms": median(&mut v), "rows": rows}))
        };
        json!({"show": show, "root": time(false)?, "subtree": time(true)?})
    };
    let devices = serde_json::to_value(scanner.devices())?;
    let mut truth_out = Value::Null;
    if fresh {
        let truth = read_manifest(&env.manifest)?;
        let mut m = serde_json::Map::new();
        for (folder, lib, root_id) in &libs {
            let entries: Vec<&ManifestEntry> = truth
                .iter()
                .filter(|e| e.path.starts_with(&format!("{folder}/")))
                .collect();
            m.insert(
                folder.clone(),
                truth::compare(&cat, *lib, *root_id, folder, &entries)?,
            );
        }
        truth_out = Value::Object(m);
    }
    scanner.shutdown(Duration::from_secs(10));
    drop(scanner);
    close(cat)?;
    Ok(json!({
        "scan_ms": scan_ms,
        "fingerprints_done_ms": fingerprints_done_ms,
        "fingerprints_idle": fp_idle,
        "files": rows,
        "files_with_fp128": with_fp,
        "rss_idle_after_scan_mb": rss_idle,
        "wal_bytes_idle_after_scan": wal_bytes,
        "list_libraries": list_libraries,
        "watcher_snapshot": watcher_snapshot,
        "first_items_committed_ms": first_item_ms,
        "libraries": per,
        "writer": {"commands": c1 - c0, "transactions": t1 - t0},
        "devices": devices,
        "rss_after_catalog_open_mb": rss_open,
        "db_bytes_after_close": std::fs::metadata(&env.db).map(|m| m.len()).unwrap_or(0),
        "ground_truth": truth_out,
    }))
}

/// Copy episodes into the watched clone one at a time and time until the
/// catalog commits each (the `ItemsChanged` event), then the same for
/// deleting them.
fn phase_watch(env: &Env) -> BoxResult<Value> {
    let (cat, libs) = open(env, false)?;
    let scanner = Scanner::with_options(
        cat.clone(),
        config(env),
        ScannerOptions {
            startup: StartupCatchUp::Nothing,
            ..ScannerOptions::default()
        },
    );
    let tv = libs.iter().find(|l| l.0 == "TV").ok_or("TV")?.1;
    let mut events = scanner.subscribe();
    scanner.watch(tv)?;
    std::thread::sleep(Duration::from_millis(500));
    let mut eps = Vec::new();
    collect_videos(&env.root.join("TV"), &mut eps);
    eps.sort();
    let (mut add_ms, mut del_ms, mut created) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..10 {
        let seed = &eps[(i * 97) % eps.len()];
        let ext = seed.extension().and_then(|e| e.to_str()).unwrap_or("mkv");
        let dst = seed
            .parent()
            .ok_or("parent")?
            .join(format!("Bench Watch S77E{i:02}.{ext}"));
        while events.try_recv().is_ok() {}
        let t = Instant::now();
        clone_path(seed, &dst, false)?;
        add_ms.push(wait_change(&mut events, t, |e| {
            matches!(e, Event::ItemsChanged { added, updated, .. } if !added.is_empty() || !updated.is_empty())
        })?);
        created.push(dst);
        std::thread::sleep(Duration::from_millis(300));
    }
    for dst in &created {
        while events.try_recv().is_ok() {}
        let t = Instant::now();
        std::fs::remove_file(dst)?;
        del_ms.push(wait_change(&mut events, t, |e| {
            matches!(e, Event::ItemsChanged { removed, updated, .. } if !removed.is_empty() || !updated.is_empty())
        })?);
        std::thread::sleep(Duration::from_millis(300));
    }
    scanner.shutdown(Duration::from_secs(5));
    drop(scanner);
    close(cat)?;
    let stat = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.total_cmp(b));
        json!({
            "median_ms": v[v.len() / 2],
            "p90_ms": v[(v.len() * 9 / 10).min(v.len() - 1)],
            "max_ms": v[v.len() - 1],
            "all_ms": v,
        })
    };
    Ok(json!({
        "settle_ms": ScannerOptions::default().watch_settle.as_millis() as u64,
        "add": stat(&mut add_ms),
        "delete": stat(&mut del_ms),
        "note": "from the file operation returning to the catalog commit's ItemsChanged event: FSEvents latency + flux's settle window + scan_dirs + commit",
    }))
}

fn wait_change(
    rx: &mut tokio::sync::broadcast::Receiver<Event>,
    t: Instant,
    want: impl Fn(&Event) -> bool,
) -> BoxResult<f64> {
    loop {
        if t.elapsed() > Duration::from_secs(30) {
            return Err("no catalog change within 30 s".into());
        }
        match rx.try_recv() {
            Ok(e) if want(&e) => return Ok(t.elapsed().as_secs_f64() * 1e3),
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

fn add_episode(root: &Path, tag: &str, n: usize) -> BoxResult<()> {
    let mut eps = Vec::new();
    collect_videos(&root.join("TV"), &mut eps);
    eps.sort();
    let seed = &eps[(n * 131 + 7) % eps.len()];
    let ext = seed.extension().and_then(|e| e.to_str()).unwrap_or("mkv");
    let dst = seed
        .parent()
        .ok_or("parent")?
        .join(format!("Bench Startup {tag}E{n:02}.{ext}"));
    clone_path(seed, &dst, false)?;
    Ok(())
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v.get(v.len() / 2).copied().unwrap_or(0.0)
}

/// Startup catch-up after a restart: Mille's resume snapshot +
/// `events_since` (then `scan_dirs` of the parents) vs a plain Quick scan,
/// 5 interleaved trials each with one new episode, plus what keeping the
/// snapshot costs a full scan.
fn phase_startup(env: &Env) -> BoxResult<Value> {
    let (cat, libs) = open(env, false)?;
    let mk = |startup| {
        Scanner::with_options(
            cat.clone(),
            config(env),
            ScannerOptions {
                startup,
                ..ScannerOptions::default()
            },
        )
    };
    let resume = mk(StartupCatchUp::MilleResume);
    let quick = mk(StartupCatchUp::QuickScan);
    let tv = libs.iter().find(|l| l.0 == "TV").ok_or("TV")?.1;
    let full = |s: &Scanner| -> BoxResult<f64> {
        let t = Instant::now();
        s.scan_library(tv, ScanMode::Quick, Priority::Scheduled).wait()?;
        Ok(t.elapsed().as_secs_f64() * 1e3)
    };
    let (mut with_snap, mut without, mut diff, mut via_resume, mut via_quick, mut dirs) =
        (vec![], vec![], vec![], vec![], vec![], vec![]);
    for i in 0..5 {
        // Unchanged full scans: with (writes the snapshot) and without.
        with_snap.push(full(&resume)?);
        without.push(full(&quick)?);
        // (a) one new episode, caught up through the snapshot.
        add_episode(&env.root, "S88", i)?;
        let t = Instant::now();
        let d = resume.resume_dirs(tv).ok_or("no resume snapshot")?;
        diff.push(t.elapsed().as_secs_f64() * 1e3);
        dirs.push(d.len() as f64);
        resume.scan_dirs(tv, d, Priority::Watcher).wait()?;
        via_resume.push(t.elapsed().as_secs_f64() * 1e3);
        // (b) another new episode, caught up by a Quick scan.
        add_episode(&env.root, "S89", i)?;
        via_quick.push(full(&quick)?);
    }
    let snap_bytes: u64 = libs
        .iter()
        .map(|(_, _, root_id)| {
            let p = format!("{}.scan-resume-root-{root_id}.bin", env.db.display());
            std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0)
        })
        .sum();
    resume.shutdown(Duration::from_secs(5));
    quick.shutdown(Duration::from_secs(5));
    drop((resume, quick));
    close(cat)?;
    Ok(json!({
        "trials": 5,
        "library": "TV",
        "mille_resume_catchup_median_ms": median(&mut via_resume),
        "events_since_median_ms": median(&mut diff),
        "dirs_rescanned_median": median(&mut dirs),
        "quick_scan_catchup_median_ms": median(&mut via_quick),
        "unchanged_full_scan_writing_snapshot_median_ms": median(&mut with_snap),
        "unchanged_full_scan_median_ms": median(&mut without),
        "snapshot_bytes": snap_bytes,
        "all": {"resume": via_resume, "quick": via_quick, "events_since": diff},
    }))
}

// ---- measurement helpers ------------------------------------------------------------

/// This process's resident set size now (not the peak), MB.
fn rss_now_mb() -> Value {
    Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<f64>().ok())
        .map(|kib| json!((kib / 1024.0 * 10.0).round() / 10.0))
        .unwrap_or(Value::Null)
}

fn loadavg() -> String {
    Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Not Unix: no getrusage.
#[cfg(not(unix))]
fn rusage() -> Value {
    Value::Null
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn rusage() -> Value {
    // SAFETY: getrusage fills the zeroed struct we own.
    let mut r: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut r) } != 0 {
        return Value::Null;
    }
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    // ru_maxrss: bytes on macOS, KiB on Linux.
    let max_rss = if cfg!(target_os = "macos") {
        r.ru_maxrss as f64 / (1024.0 * 1024.0)
    } else {
        r.ru_maxrss as f64 / 1024.0
    };
    json!({
        "user_s": secs(r.ru_utime),
        "sys_s": secs(r.ru_stime),
        "cpu_s": secs(r.ru_utime) + secs(r.ru_stime),
        "max_rss_mb": (max_rss * 10.0).round() / 10.0,
    })
}
