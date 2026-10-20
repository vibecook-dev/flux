//! server-bench: playback latency through the real `flux-server` binary,
//! measured like `tools/bench/jellyfin_playback_bench.py` (and flux-hls's
//! `hls-bench`), plus startup time and idle RSS.
//!
//! ```text
//! cargo build --release -p flux-server --bin flux-server --example server_bench
//! target/release/examples/server_bench [--runs 5] [--preset chrome] [--files a.mkv,b.mkv]
//!     [--server-bin PATH] [--big-catalog DB | --no-big-catalog] [--out DIR] [--no-nice]
//! ```
//!
//! 1. Seeds a catalog with the feature clips under test the way flux-scan
//!    will (flux-names + flux-probe → `Writer::apply`), then per run copies it
//!    (APFS clone) into a fresh data dir: no cached playback index, cold
//!    engine, like `hls-bench`'s fresh `hls-serve`. The OS page cache is not
//!    dropped (needs root).
//! 2. Per run: spawn `nice -n 19 flux-server --no-scan --no-jobs`, time
//!    spawn → first `GET /api/v1/server` 200 ("startup"), read the server's
//!    own `ready_ms`, sample idle RSS after 2 s. After the run, a restart on
//!    the same data dir is timed too (the first open of a freshly copied
//!    database costs extra: WAL/shm creation and copy-on-write).
//! 3. Per file: `POST /api/v1/play` (≈ Jellyfin's PlaybackInfo; it also warms
//!    segment 0), then TTFS = master → variant → init → segment 0, then the
//!    default demuxed audio rendition's playlist → init → segment 0
//!    (`ttfs_av_s`, sequential), then cold seeks to the segments covering
//!    600/3600/5100/30 s (video + audio).
//! 4. `--big-catalog`: startup, idle RSS and browse latencies on a copy of a
//!    large catalog (default: the 14.7k-file pipeline_smoke database in
//!    `$FLUX_SMOKE_WORK`, else `<temp dir>/flux-integrate`, if present).
//!
//! The feature clips come from `$FLUX_TEST_MEDIA/features` (it skips without
//! them). Writes `$FLUX_BENCH_RESULTS/flux-server-playback-<ts>.json`
//! (printed only when unset) and compares with the newest
//! `jellyfin-playback-*.json` there.

#[path = "../tests/common/mod.rs"]
#[allow(unused_imports)]
mod common;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use flux_catalog::{Catalog, CatalogConfig};
use flux_core::LibraryKind;
use serde_json::{json, Value};

const FILES: [&str; 5] = [
    "h264_multi_audio_subs.mkv",
    "h264_truehd71.mkv",
    "hevc_main10_hdr10_eac3.mkv",
    "hevc_4k_60mbps.mkv",
    "long_2h_360p.mkv",
];
const SEEKS_S: [f64; 4] = [600.0, 3600.0, 5100.0, 30.0];
/// Catalogs and data dirs of the runs.
fn work_dir() -> PathBuf {
    std::env::temp_dir().join("flux-server-bench")
}

/// The newest Jellyfin playback baseline in the results directory.
fn jellyfin_baseline(results: &Path) -> Option<PathBuf> {
    std::fs::read_dir(results)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("jellyfin-playback-") && n.ends_with(".json"))
        })
        .max()
}

/// Minimal HTTP/1.1 keep-alive client (every flux response has a Content-Length).
struct Http {
    addr: String,
    conn: Option<BufReader<TcpStream>>,
}

struct Resp {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
    secs: f64,
}

impl Http {
    fn new(addr: &str) -> Self {
        Http {
            addr: addr.into(),
            conn: None,
        }
    }

    fn get(&mut self, path: &str) -> Result<Resp> {
        self.request("GET", path, None)
    }

    fn post(&mut self, path: &str, body: &Value) -> Result<Resp> {
        self.request("POST", path, Some(serde_json::to_vec(body)?))
    }

    fn request(&mut self, method: &str, path: &str, body: Option<Vec<u8>>) -> Result<Resp> {
        for attempt in 0..2 {
            let t0 = Instant::now();
            match self.try_request(method, path, body.as_deref()) {
                Ok((status, headers, body)) => {
                    return Ok(Resp {
                        status,
                        headers,
                        body,
                        secs: t0.elapsed().as_secs_f64(),
                    })
                }
                Err(e) if attempt == 0 => {
                    self.conn = None;
                    let _ = e;
                }
                Err(e) => return Err(e),
            }
        }
        bail!("unreachable")
    }

    fn try_request(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<(u16, HashMap<String, String>, Vec<u8>)> {
        if self.conn.is_none() {
            let s = TcpStream::connect(&self.addr)?;
            s.set_nodelay(true)?;
            self.conn = Some(BufReader::with_capacity(1 << 20, s));
        }
        let conn = self.conn.as_mut().context("conn")?;
        // X-Flux-Request: what the web client sends on every API call
        // (unsafe /api/ methods need it).
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nX-Flux-Request: 1\r\nConnection: keep-alive\r\n",
            self.addr
        );
        if let Some(b) = body {
            head.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                b.len()
            ));
        }
        head.push_str("\r\n");
        let s = conn.get_mut();
        s.write_all(head.as_bytes())?;
        if let Some(b) = body {
            s.write_all(b)?;
        }
        let mut line = String::new();
        conn.read_line(&mut line)?;
        let status: u16 = line.split_whitespace().nth(1).context("status line")?.parse()?;
        let mut headers = HashMap::new();
        loop {
            line.clear();
            conn.read_line(&mut line)?;
            let l = line.trim_end();
            if l.is_empty() {
                break;
            }
            if let Some((k, v)) = l.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        let len: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; len];
        conn.read_exact(&mut body)?;
        Ok((status, headers, body))
    }
}

fn lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

fn extinf(text: &str) -> Vec<f64> {
    text.lines()
        .filter_map(|l| l.strip_prefix("#EXTINF:"))
        .filter_map(|v| v.split(',').next()?.parse().ok())
        .collect()
}

fn join(base: &str, rel: &str) -> String {
    match base.rfind('/') {
        Some(i) => format!("{}/{}", &base[..i], rel),
        None => rel.into(),
    }
}

fn default_audio(master: &str) -> Option<String> {
    master
        .lines()
        .filter(|l| l.starts_with("#EXT-X-MEDIA:TYPE=AUDIO") && l.contains("DEFAULT=YES"))
        .find_map(|l| l.split("URI=\"").nth(1)?.split('"').next().map(String::from))
}

fn server_ms(r: &Resp) -> Option<f64> {
    r.headers.get("x-flux-ms").and_then(|v| v.parse().ok())
}

fn loadavg() -> String {
    Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn rss_mb(pid: u32) -> Option<f64> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .ok()
        .map(|kb| kb / 1024.0)
}

fn free_port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

fn pct(v: &[f64], q: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    Some(s[((s.len() - 1) as f64 * q).round() as usize])
}

fn median_of(runs: &[Value], f: impl Fn(&Value) -> Option<f64>) -> Value {
    json!(pct(&runs.iter().filter_map(f).collect::<Vec<_>>(), 0.5))
}

struct Server {
    child: Child,
    log: PathBuf,
}

impl Server {
    /// SIGTERM and wait; returns the shutdown time.
    fn stop(mut self) -> Duration {
        let t = Instant::now();
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return t.elapsed();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        t.elapsed()
    }

    /// The server's own "flux is ready … ready_ms" log value.
    fn ready_ms(&self) -> Option<f64> {
        let log = std::fs::read_to_string(&self.log).ok()?;
        let line = log.lines().find(|l| l.contains("flux is ready"))?;
        let v = line.split("ready_ms=").nth(1)?;
        v.trim_start_matches('"')
            .split(|c: char| !c.is_ascii_digit() && c != '.')
            .next()?
            .parse()
            .ok()
    }
}

/// Spawns the server and waits for the API; returns it with the startup time (ms).
fn spawn(bin: &Path, data: &Path, port: u16, nice: bool) -> Result<(Server, f64)> {
    let log = data.join("server.log");
    let mut cmd = if nice {
        let mut c = Command::new("nice");
        c.args(["-n", "19"]).arg(bin);
        c
    } else {
        Command::new(bin)
    };
    cmd.arg("--data-dir")
        .arg(data)
        .args([
            "--port",
            &port.to_string(),
            "--no-scan",
            "--no-jobs",
            "--tailnet",
            "off",
            "--log",
            "info",
        ])
        .env("NO_COLOR", "1")
        .env_remove("RUST_LOG")
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log)?);
    let t0 = Instant::now();
    let child = cmd.spawn().with_context(|| format!("spawn {}", bin.display()))?;
    let srv = Server { child, log };
    let mut http = Http::new(&format!("127.0.0.1:{port}"));
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(r) = http.get("/api/v1/server") {
            if r.status == 200 {
                return Ok((srv, t0.elapsed().as_secs_f64() * 1000.0));
            }
        }
        if Instant::now() > deadline {
            bail!("flux-server did not answer on port {port}");
        }
        http.conn = None;
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn clone_db(from: &Path, to_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(to_dir)?;
    for suffix in ["", "-wal", "-shm"] {
        let src = PathBuf::from(format!("{}{suffix}", from.display()));
        if src.exists() {
            let dst = to_dir.join(format!("catalog.db{suffix}"));
            flux_test_media::clone_file(&src, &dst)?;
        }
    }
    Ok(())
}

fn label(plan: &Value) -> String {
    serde_json::from_value::<flux_core::PlaybackPlan>(plan.clone())
        .map(|p| p.label().to_string())
        .unwrap_or_default()
}

fn bench_file(http: &mut Http, item: i64, caps: &Value) -> Result<Value> {
    let play = http.post(
        "/api/v1/play",
        &json!({"item_id": item, "asset_id": null, "caps": caps, "audio_track": null, "subtitle_track": null, "start_ms": 0}),
    )?;
    if play.status != 200 {
        bail!("play: {} {}", play.status, String::from_utf8_lossy(&play.body));
    }
    let p: Value = serde_json::from_slice(&play.body)?;
    let master_url = p["url"].as_str().context("url")?.to_string();
    let mut entry = json!({
        "playbackinfo_s": play.secs,
        "label": label(&p["plan"]),
        "explain": p["explain"],
    });
    let t0 = Instant::now();
    let master = http.get(&master_url)?;
    let mtext = String::from_utf8_lossy(&master.body).to_string();
    let variant_url = join(&master_url, lines(&mtext).first().context("no variant")?);
    let variant = http.get(&variant_url)?;
    let vtext = String::from_utf8_lossy(&variant.body).to_string();
    let segs = lines(&vtext);
    let durs = extinf(&vtext);
    let init = http.get(&join(&variant_url, "init.mp4"))?;
    let seg0 = http.get(&join(&variant_url, &segs[0]))?;
    let ttfs = t0.elapsed().as_secs_f64();
    if init.status != 200 || seg0.status != 200 {
        bail!("init {} seg0 {}", init.status, seg0.status);
    }
    let audio_url = default_audio(&mtext).map(|u| join(&master_url, &u));
    let mut audio_segs = Vec::new();
    let mut ttfs_audio = 0.0;
    if let Some(au) = &audio_url {
        let t1 = Instant::now();
        let ap = http.get(au)?;
        audio_segs = lines(&String::from_utf8_lossy(&ap.body));
        http.get(&join(au, "init.mp4"))?;
        let a0 = http.get(&join(au, &audio_segs[0]))?;
        if a0.status != 200 {
            bail!("audio seg0 {}", a0.status);
        }
        ttfs_audio = t1.elapsed().as_secs_f64();
    }
    entry["ttfs_s"] = json!(ttfs);
    entry["ttfs_av_s"] = json!(ttfs + ttfs_audio);
    entry["play_to_seg0_s"] = json!(play.secs + ttfs);
    entry["seg0_bytes"] = json!(seg0.body.len());
    entry["seg0_server_ms"] = json!(server_ms(&seg0));
    entry["segments"] = json!(segs.len());
    let mut starts = Vec::new();
    let mut cum = 0.0;
    for d in &durs {
        starts.push(cum);
        cum += d;
    }
    let mut seeks = Vec::new();
    for &target in &SEEKS_S {
        if target >= cum {
            continue;
        }
        let idx = starts.iter().rposition(|&s| s <= target).unwrap_or(0);
        let v = http.get(&join(&variant_url, &segs[idx]))?;
        let mut s = json!({"target_s": target, "segment": idx, "latency_s": v.secs, "bytes": v.body.len(), "server_ms": server_ms(&v)});
        if let Some(au) = &audio_url {
            let a = http.get(&join(au, &audio_segs[idx.min(audio_segs.len() - 1)]))?;
            s["audio_latency_s"] = json!(a.secs);
            s["audio_server_ms"] = json!(server_ms(&a));
        }
        seeks.push(s);
    }
    entry["seeks"] = json!(seeks);
    Ok(entry)
}

fn big_catalog(bin: &Path, db: &Path, nice: bool) -> Result<Value> {
    let data = work_dir().join("big");
    let _ = std::fs::remove_dir_all(&data);
    clone_db(db, &data)?;
    // The first open migrates an older schema in place; time a second start.
    let port = free_port()?;
    let (srv, first_ms) = spawn(bin, &data, port, nice)?;
    srv.stop();
    let port = free_port()?;
    let (srv, startup_ms) = spawn(bin, &data, port, nice)?;
    std::thread::sleep(Duration::from_secs(2));
    let idle = rss_mb(srv.child.id());
    let mut http = Http::new(&format!("127.0.0.1:{port}"));
    let libs: Value = serde_json::from_slice(&http.get("/api/v1/libraries")?.body)?;
    let mut timings: HashMap<&str, Vec<f64>> = HashMap::new();
    for _ in 0..20 {
        timings
            .entry("libraries")
            .or_default()
            .push(http.get("/api/v1/libraries")?.secs * 1e3);
        timings
            .entry("home")
            .or_default()
            .push(http.get("/api/v1/home")?.secs * 1e3);
        if let Some(id) = libs[0]["id"].as_i64() {
            timings
                .entry("items_page_100")
                .or_default()
                .push(http.get(&format!("/api/v1/libraries/{id}/items?limit=100"))?.secs * 1e3);
        }
        timings
            .entry("search")
            .or_default()
            .push(http.get("/api/v1/search?q=blade")?.secs * 1e3);
    }
    let busy = rss_mb(srv.child.id());
    let ready_ms = srv.ready_ms();
    let shutdown = srv.stop();
    let items: u64 = libs
        .as_array()
        .map(|a| a.iter().filter_map(|l| l["item_count"].as_u64()).sum())
        .unwrap_or(0);
    let map = |q: f64| -> serde_json::Map<String, Value> {
        timings
            .iter()
            .map(|(k, v)| (k.to_string(), json!(pct(v, q))))
            .collect()
    };
    Ok(json!({
        "catalog": db.display().to_string(),
        "libraries": libs.as_array().map(Vec::len),
        "top_level_items": items,
        "first_start_with_migration_ms": first_ms,
        "startup_to_api_ms": startup_ms,
        "server_ready_ms": ready_ms,
        "idle_rss_mb": idle,
        "rss_after_browse_mb": busy,
        "shutdown_ms": shutdown.as_secs_f64() * 1e3,
        "browse_ms_p50": map(0.5),
        "browse_ms_p95": map(0.95),
    }))
}

fn main() -> Result<()> {
    let exe_dir = std::env::current_exe()?
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let mut bin = exe_dir.join("flux-server");
    let mut runs = 5usize;
    let mut preset = "chrome".to_string();
    let mut files: Vec<String> = FILES.iter().map(|s| s.to_string()).collect();
    let mut out = flux_test_media::bench_results();
    let mut big: Option<PathBuf> = Some(
        std::env::var_os("FLUX_SMOKE_WORK")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("flux-integrate"))
            .join("pipeline-full.db"),
    );
    let mut nice = true;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--server-bin" => bin = PathBuf::from(args.next().context("--server-bin")?),
            "--runs" => runs = args.next().context("--runs")?.parse()?,
            "--preset" => preset = args.next().context("--preset")?,
            "--files" => {
                files = args
                    .next()
                    .context("--files")?
                    .split(',')
                    .map(String::from)
                    .collect()
            }
            "--out" => out = Some(PathBuf::from(args.next().context("--out")?)),
            "--big-catalog" => big = Some(PathBuf::from(args.next().context("--big-catalog")?)),
            "--no-big-catalog" => big = None,
            "--no-nice" => nice = false,
            other => bail!("unknown argument {other}"),
        }
    }
    anyhow::ensure!(
        bin.is_file(),
        "no server binary at {} (--server-bin)",
        bin.display()
    );
    let caps = serde_json::to_value(flux_hls::presets::by_name(&preset).context("unknown preset")?)?;
    let load_start = loadavg();
    eprintln!("load {load_start}");

    if flux_test_media::media("features").is_none() {
        return Ok(());
    }
    // Seed once.
    let work = work_dir();
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(work.join("seed"))?;
    let seed_db = work.join("seed/catalog.db");
    let items: HashMap<String, i64> = {
        let cat = Catalog::open(&seed_db, CatalogConfig::default())?;
        let keep = files.clone();
        common::seed(
            &cat,
            "Features",
            LibraryKind::Movies,
            &common::media("features"),
            move |n| keep.iter().any(|f| f == n),
        );
        let map = files
            .iter()
            .map(|f| (f.clone(), common::item_of(&cat, f).0))
            .collect();
        cat.close()?;
        map
    };

    let started = Command::new("date")
        .args(["-u", "+%Y%m%d-%H%M%S"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let mut per_file: HashMap<String, Vec<Value>> = HashMap::new();
    let mut per_run = Vec::new();
    for run in 0..runs {
        let data = work.join(format!("run{run}"));
        clone_db(&seed_db, &data)?;
        let port = free_port()?;
        let (srv, startup_ms) = spawn(&bin, &data, port, nice)?;
        std::thread::sleep(Duration::from_secs(2));
        let idle = rss_mb(srv.child.id());
        let mut http = Http::new(&format!("127.0.0.1:{port}"));
        // What a client does on load; also opens the pooled catalog readers.
        http.get("/api/v1/libraries")?;
        http.get("/api/v1/home")?;
        for f in &files {
            let e = bench_file(&mut http, items[f], &caps).with_context(|| f.clone())?;
            eprintln!(
                "run {run} {f}: play {:.1} ms, ttfs {:.1} ms, +audio {:.1} ms, seeks {:?}",
                e["playbackinfo_s"].as_f64().unwrap_or(0.0) * 1e3,
                e["ttfs_s"].as_f64().unwrap_or(0.0) * 1e3,
                e["ttfs_av_s"].as_f64().unwrap_or(0.0) * 1e3,
                e["seeks"].as_array().map(|s| s
                    .iter()
                    .map(|x| format!("{:.1}", x["latency_s"].as_f64().unwrap_or(0.0) * 1e3))
                    .collect::<Vec<_>>())
            );
            per_file.entry(f.clone()).or_default().push(e);
        }
        let busy = rss_mb(srv.child.id());
        let ready_ms = srv.ready_ms();
        let shutdown = srv.stop();
        // A restart on the same data dir (the catalog file already opened once).
        let (again, restart_ms) = spawn(&bin, &data, free_port()?, nice)?;
        let restart_ready = again.ready_ms();
        again.stop();
        per_run.push(json!({
            "startup_to_api_ms": startup_ms, "server_ready_ms": ready_ms,
            "restart_to_api_ms": restart_ms, "restart_ready_ms": restart_ready,
            "idle_rss_mb": idle, "rss_after_playback_mb": busy, "shutdown_ms": shutdown.as_secs_f64() * 1e3,
        }));
        eprintln!(
            "run {run}: startup {startup_ms:.1} ms (server {ready_ms:?} ms), idle RSS {idle:?} MB, after {busy:?} MB"
        );
    }

    let jellyfin: Value = out
        .as_deref()
        .and_then(jellyfin_baseline)
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let mut files_out = serde_json::Map::new();
    for f in &files {
        let rs = per_file.get(f).cloned().unwrap_or_default();
        let Some(first) = rs.first().cloned() else {
            continue;
        };
        let mut e = first.clone();
        for k in [
            "playbackinfo_s",
            "ttfs_s",
            "ttfs_av_s",
            "play_to_seg0_s",
            "seg0_server_ms",
        ] {
            e[k] = median_of(&rs, |r| r[k].as_f64());
        }
        if let Some(seeks) = first["seeks"].as_array() {
            let merged: Vec<Value> = seeks
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let mut m = s.clone();
                    for k in ["latency_s", "audio_latency_s", "server_ms", "audio_server_ms"] {
                        m[k] = median_of(&rs, |r| r["seeks"][i][k].as_f64());
                    }
                    m
                })
                .collect();
            e["seeks"] = json!(merged);
        }
        let jf = &jellyfin["files"][f];
        e["jellyfin"] =
            json!({"playbackinfo_s": jf["playbackinfo_s"], "ttfs_s": jf["ttfs_s"], "seeks": jf["seeks"]});
        e["runs"] = json!(rs);
        files_out.insert(f.clone(), e);
    }
    let big_out = match big.filter(|p| p.exists()) {
        Some(db) => big_catalog(&bin, &db, nice).unwrap_or_else(|e| json!({"error": e.to_string()})),
        None => Value::Null,
    };
    let load_end = loadavg();
    let result = json!({
        "tool": "flux-server",
        "started_utc": started,
        "preset": preset,
        "runs": runs,
        "server_bin": bin.display().to_string(),
        "server_niced": nice,
        "load_avg_start": load_start,
        "load_avg_end": load_end,
        "startup": {
            "startup_to_api_ms_median": median_of(&per_run, |r| r["startup_to_api_ms"].as_f64()),
            "server_ready_ms_median": median_of(&per_run, |r| r["server_ready_ms"].as_f64()),
            "restart_to_api_ms_median": median_of(&per_run, |r| r["restart_to_api_ms"].as_f64()),
            "restart_ready_ms_median": median_of(&per_run, |r| r["restart_ready_ms"].as_f64()),
            "idle_rss_mb_median": median_of(&per_run, |r| r["idle_rss_mb"].as_f64()),
            "rss_after_playback_mb_median": median_of(&per_run, |r| r["rss_after_playback_mb"].as_f64()),
            "shutdown_ms_median": median_of(&per_run, |r| r["shutdown_ms"].as_f64()),
            "per_run": per_run,
        },
        "files": files_out,
        "big_catalog": big_out,
        "note": "Shared machine (another project's dev server and other agents running); load averages above. \
                 Each run is a fresh flux-server process (nice 19) on a fresh copy of a seeded catalog \
                 (no cached playback index); OS page cache not dropped. TTFS = master -> variant -> init -> seg0 \
                 after POST /play (which warms segment 0 of every rendition); ttfs_av_s adds the default audio \
                 rendition's playlist -> init -> seg0 sequentially; play_to_seg0_s = POST /play + TTFS. \
                 Loopback HTTP/1.1 keep-alive client.",
    });
    match &out {
        Some(out) => {
            std::fs::create_dir_all(out)?;
            let path = out.join(format!("flux-server-playback-{started}.json"));
            std::fs::write(&path, serde_json::to_vec_pretty(&result)?)?;
            eprintln!("wrote {}", path.display());
        }
        None => println!("{}", serde_json::to_string_pretty(&result)?),
    }
    let ms = |v: &Value| v.as_f64().map(|s| (s * 1e4).round() / 10.0);
    let summary: serde_json::Map<String, Value> = files_out
        .iter()
        .map(|(f, e)| {
            let seeks: Vec<Value> = e["seeks"]
                .as_array()
                .map(|s| {
                    s.iter()
                        .map(|x| json!([x["target_s"], ms(&x["latency_s"]), ms(&x["audio_latency_s"])]))
                        .collect()
                })
                .unwrap_or_default();
            (
                f.clone(),
                json!({"label": e["label"], "play_ms": ms(&e["playbackinfo_s"]), "ttfs_ms": ms(&e["ttfs_s"]),
                       "ttfs_av_ms": ms(&e["ttfs_av_s"]), "seeks_ms": seeks}),
            )
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"startup": result["startup"], "files": summary, "big_catalog": result["big_catalog"]})
        )?
    );
    Ok(())
}
