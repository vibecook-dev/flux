//! hls-bench: playback latency benchmark against `hls-serve` over loopback
//! HTTP, measured like `tools/bench/jellyfin_playback_bench.py`.
//!
//! ```text
//! hls-bench [--serve-bin PATH] [--media DIR] [--runs 5] [--port 18181] [--preset chrome]
//!           [--out DIR] [--files a.mkv,b.mkv] [--seeks 600,3600,5100,30] [--no-sweep]
//! ```
//!
//! Each run starts a fresh `hls-serve` (cold engine: no index, no caches;
//! the OS page cache is *not* dropped — that needs root) and, per file:
//! 1. `prepare` (the analogue of Jellyfin's PlaybackInfo / flux `POST /play`).
//! 2. Time-to-first-segment: master → variant → init → segment 0 (video), as
//!    Jellyfin's harness measures; plus the demuxed audio rendition's
//!    playlist → init → segment 0 (`ttfs_av_s`).
//! 3. Cold seeks: the segment covering 600/3600/5100/30 s (video + audio).
//!
//! After the runs, a sweep fetches every segment once to get server time
//! p50/p99 per segment and CPU ms per MB served.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

const DEFAULT_FILES: [&str; 5] = [
    "h264_multi_audio_subs.mkv",
    "h264_truehd71.mkv",
    "hevc_main10_hdr10_eac3.mkv",
    "hevc_4k_60mbps.mkv",
    "long_2h_360p.mkv",
];
const SEEK_TARGETS_S: [f64; 4] = [600.0, 3600.0, 5100.0, 30.0];

/// Seek targets in seconds (`--seeks`; default: Jellyfin harness's).
static SEEKS: std::sync::OnceLock<Vec<f64>> = std::sync::OnceLock::new();

/// Minimal HTTP/1.1 keep-alive client (responses always carry Content-Length).
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
            addr: addr.to_string(),
            conn: None,
        }
    }

    fn get(&mut self, path: &str) -> Result<Resp> {
        for attempt in 0..2 {
            let t0 = Instant::now();
            match self.try_get(path) {
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

    fn try_get(&mut self, path: &str) -> Result<(u16, HashMap<String, String>, Vec<u8>)> {
        if self.conn.is_none() {
            let s = TcpStream::connect(&self.addr)?;
            s.set_nodelay(true)?;
            self.conn = Some(BufReader::with_capacity(1 << 20, s));
        }
        let conn = self.conn.as_mut().context("conn")?;
        write!(
            conn.get_mut(),
            "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: keep-alive\r\n\r\n",
            self.addr
        )?;
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
        .filter_map(|v| v.trim_end_matches(',').split(',').next()?.parse().ok())
        .collect()
}

fn join(base: &str, rel: &str) -> String {
    match base.rfind('/') {
        Some(i) => format!("{}/{}", &base[..i], rel),
        None => rel.to_string(),
    }
}

/// The default audio rendition URI of the first variant's audio group.
fn default_audio(master: &str) -> Option<String> {
    let group = master
        .lines()
        .find(|l| l.starts_with("#EXT-X-STREAM-INF"))?
        .split("AUDIO=\"")
        .nth(1)?
        .split('"')
        .next()?
        .to_string();
    master
        .lines()
        .filter(|l| l.starts_with("#EXT-X-MEDIA:TYPE=AUDIO") && l.contains(&format!("GROUP-ID=\"{group}\"")))
        .find(|l| l.contains("DEFAULT=YES"))
        .and_then(|l| l.split("URI=\"").nth(1)?.split('"').next().map(String::from))
}

struct Server {
    child: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_server(bin: &Path, media: &Path, files: &[String], port: u16, preset: &str) -> Result<Server> {
    let mut c = Command::new(bin);
    for f in files {
        c.arg(media.join(f));
    }
    c.args(["--port", &port.to_string(), "--preset", preset])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = c.spawn().with_context(|| format!("spawn {}", bin.display()))?;
    let srv = Server { child };
    let addr = format!("127.0.0.1:{port}");
    for _ in 0..200 {
        if TcpStream::connect(&addr).is_ok() {
            return Ok(srv);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    bail!("hls-serve did not start on {addr}")
}

fn stats(http: &mut Http) -> Result<Value> {
    Ok(serde_json::from_slice(&http.get("/_stats")?.body)?)
}

fn bench_file(http: &mut Http, file: &str, preset: &str) -> Result<Value> {
    let base = format!("/media/{file}/{preset}");
    let prep = http.get(&format!("{base}/prepare"))?;
    if prep.status != 200 {
        return Ok(json!({ "error": String::from_utf8_lossy(&prep.body) }));
    }
    let info: Value = serde_json::from_slice(&prep.body)?;
    let mut entry = json!({
        "playbackinfo_s": prep.secs,
        "index_ms": info["index"]["micros"].as_f64().unwrap_or(0.0) / 1000.0,
        "index_method": info["index"]["method"],
        "decision": info["label"],
        "transcode_reasons": info["explain"],
    });
    // Time to first segment: master → variant → init → seg0 (as Jellyfin's harness).
    let t0 = Instant::now();
    let master_url = format!("{base}/master.m3u8");
    let master = http.get(&master_url)?;
    let master_text = String::from_utf8_lossy(&master.body).to_string();
    let variant_rel = lines(&master_text).into_iter().next().context("no variant")?;
    let variant_url = join(&master_url, &variant_rel);
    let variant = http.get(&variant_url)?;
    let vtext = String::from_utf8_lossy(&variant.body).to_string();
    let segs = lines(&vtext);
    let durs = extinf(&vtext);
    let init = http.get(&join(&variant_url, "init.mp4"))?;
    let seg0 = http.get(&join(&variant_url, &segs[0]))?;
    let ttfs = t0.elapsed().as_secs_f64();
    if init.status != 200 || seg0.status != 200 {
        bail!("{file}: init {} seg0 {}", init.status, seg0.status);
    }
    // Demuxed audio: playlist → init → seg0.
    let audio_url = default_audio(&master_text).map(|u| join(&master_url, &u));
    let mut audio_segs = Vec::new();
    let mut ttfs_audio = None;
    if let Some(au) = &audio_url {
        let t1 = Instant::now();
        let ap = http.get(au)?;
        audio_segs = lines(&String::from_utf8_lossy(&ap.body));
        http.get(&join(au, "init.mp4"))?;
        let a0 = http.get(&join(au, &audio_segs[0]))?;
        if a0.status != 200 {
            bail!("{file}: audio seg0 {}", a0.status);
        }
        ttfs_audio = Some(t1.elapsed().as_secs_f64());
    }
    entry["ttfs_s"] = json!(ttfs);
    entry["ttfs_av_s"] = json!(ttfs + ttfs_audio.unwrap_or(0.0));
    entry["seg0_bytes"] = json!(seg0.body.len());
    entry["seg0_server_ms"] = json!(seg0.headers.get("x-flux-ms").and_then(|v| v.parse::<f64>().ok()));
    entry["segments"] = json!(segs.len());
    entry["seg_dur_avg_s"] = json!(if durs.is_empty() {
        0.0
    } else {
        durs.iter().sum::<f64>() / durs.len() as f64
    });
    // Cold seeks: segment covering each target, never fetched before.
    let mut starts = Vec::with_capacity(durs.len());
    let mut cum = 0.0;
    for d in &durs {
        starts.push(cum);
        cum += d;
    }
    let mut seeks = Vec::new();
    for &target in SEEKS.get().map(|v| v.as_slice()).unwrap_or(&SEEK_TARGETS_S) {
        if target >= cum {
            continue;
        }
        let idx = starts.iter().rposition(|&s| s <= target).unwrap_or(0);
        let v = http.get(&join(&variant_url, &segs[idx]))?;
        let mut s = json!({ "target_s": target, "segment": idx, "latency_s": v.secs, "bytes": v.body.len(),
                            "server_ms": v.headers.get("x-flux-ms").and_then(|x| x.parse::<f64>().ok()) });
        if let Some(au) = &audio_url {
            let a = http.get(&join(au, &audio_segs[idx.min(audio_segs.len() - 1)]))?;
            s["audio_latency_s"] = json!(a.secs);
            s["audio_server_ms"] = json!(a.headers.get("x-flux-ms").and_then(|x| x.parse::<f64>().ok()));
        }
        seeks.push(s);
    }
    entry["seeks"] = json!(seeks);
    entry["server_rss_mb"] = json!(stats(http)?["max_rss_bytes"].as_f64().unwrap_or(0.0) / 1e6);
    Ok(entry)
}

/// Fetches every video + default-audio segment once; returns per-segment server ms and bytes.
fn sweep(http: &mut Http, file: &str, preset: &str) -> Result<(Vec<f64>, Vec<f64>, u64)> {
    let base = format!("/media/{file}/{preset}");
    let master_url = format!("{base}/master.m3u8");
    let master = String::from_utf8_lossy(&http.get(&master_url)?.body).to_string();
    let mut urls = Vec::new();
    let variant_url = join(&master_url, &lines(&master)[0]);
    for s in lines(&String::from_utf8_lossy(&http.get(&variant_url)?.body)) {
        urls.push((true, join(&variant_url, &s)));
    }
    if let Some(au) = default_audio(&master).map(|u| join(&master_url, &u)) {
        for s in lines(&String::from_utf8_lossy(&http.get(&au)?.body)) {
            urls.push((false, join(&au, &s)));
        }
    }
    let (mut video_ms, mut audio_ms, mut bytes) = (Vec::new(), Vec::new(), 0u64);
    for (is_video, u) in urls {
        let r = http.get(&u)?;
        bytes += r.body.len() as u64;
        let ms = r
            .headers
            .get("x-flux-ms")
            .and_then(|x| x.parse::<f64>().ok())
            .unwrap_or(0.0);
        if is_video {
            video_ms.push(ms);
        } else {
            audio_ms.push(ms);
        }
    }
    Ok((video_ms, audio_ms, bytes))
}

fn pct(v: &[f64], q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    s[((s.len() - 1) as f64 * q).round() as usize]
}

fn median_of(runs: &[Value], f: impl Fn(&Value) -> Option<f64>) -> Option<f64> {
    let v: Vec<f64> = runs.iter().filter_map(f).collect();
    (!v.is_empty()).then(|| pct(&v, 0.5))
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let exe_dir = std::env::current_exe()?
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    let mut serve_bin = exe_dir.join("hls-serve");
    // The corpus and the results directory come from the environment
    // (FLUX_TEST_MEDIA, FLUX_BENCH_RESULTS) unless given.
    let env_dir = |var: &str| std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from);
    let mut media = env_dir("FLUX_TEST_MEDIA").map(|d| d.join("features"));
    let mut runs = 5usize;
    let mut port = 18181u16;
    let mut preset = "chrome".to_string();
    let mut out = env_dir("FLUX_BENCH_RESULTS");
    let mut files: Vec<String> = DEFAULT_FILES.iter().map(|s| s.to_string()).collect();
    let mut do_sweep = true;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--serve-bin" => serve_bin = PathBuf::from(args.next().context("--serve-bin")?),
            "--media" => media = Some(PathBuf::from(args.next().context("--media")?)),
            "--runs" => runs = args.next().context("--runs")?.parse()?,
            "--port" => port = args.next().context("--port")?.parse()?,
            "--preset" => preset = args.next().context("--preset")?,
            "--out" => out = Some(PathBuf::from(args.next().context("--out")?)),
            "--files" => {
                files = args
                    .next()
                    .context("--files")?
                    .split(',')
                    .map(String::from)
                    .collect()
            }
            "--no-sweep" => do_sweep = false,
            "--seeks" => {
                let v: Vec<f64> = args
                    .next()
                    .context("--seeks")?
                    .split(',')
                    .map(|x| x.parse::<f64>())
                    .collect::<Result<_, _>>()?;
                let _ = SEEKS.set(v);
            }
            other => bail!("unknown argument {other}"),
        }
    }
    let Some(media) = media else {
        eprintln!("skipping: no media (pass --media DIR or set FLUX_TEST_MEDIA)");
        return Ok(());
    };
    let load = |label: &str| {
        let l = Command::new("sysctl")
            .args(["-n", "vm.loadavg"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        eprintln!("[{label}] load {l}");
        l
    };
    let load_start = load("start");
    let started = chrono_like_now();
    let mut per_file: HashMap<String, Vec<Value>> = HashMap::new();
    for run in 0..runs {
        let _srv = start_server(&serve_bin, &media, &files, port, &preset)?;
        let mut http = Http::new(&format!("127.0.0.1:{port}"));
        for f in &files {
            let e = bench_file(&mut http, f, &preset).with_context(|| f.clone())?;
            eprintln!(
                "run {run} {f}: ttfs {:.4}s av {:.4}s seeks {:?}",
                e["ttfs_s"].as_f64().unwrap_or(0.0),
                e["ttfs_av_s"].as_f64().unwrap_or(0.0),
                e["seeks"].as_array().map(|s| s
                    .iter()
                    .map(|x| format!("{:.4}", x["latency_s"].as_f64().unwrap_or(0.0)))
                    .collect::<Vec<_>>())
            );
            per_file.entry(f.clone()).or_default().push(e);
        }
    }
    let mut files_out = serde_json::Map::new();
    for f in &files {
        let rs = per_file.get(f).cloned().unwrap_or_default();
        let Some(first) = rs.first().cloned() else {
            continue;
        };
        let mut e = first.clone();
        e["ttfs_s"] = json!(median_of(&rs, |r| r["ttfs_s"].as_f64()));
        e["ttfs_av_s"] = json!(median_of(&rs, |r| r["ttfs_av_s"].as_f64()));
        e["playbackinfo_s"] = json!(median_of(&rs, |r| r["playbackinfo_s"].as_f64()));
        e["index_ms"] = json!(median_of(&rs, |r| r["index_ms"].as_f64()));
        if let Some(seeks) = first["seeks"].as_array() {
            let merged: Vec<Value> = seeks
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let mut m = s.clone();
                    m["latency_s"] = json!(median_of(&rs, |r| r["seeks"][i]["latency_s"].as_f64()));
                    m["audio_latency_s"] =
                        json!(median_of(&rs, |r| r["seeks"][i]["audio_latency_s"].as_f64()));
                    m["server_ms"] = json!(median_of(&rs, |r| r["seeks"][i]["server_ms"].as_f64()));
                    m["audio_server_ms"] =
                        json!(median_of(&rs, |r| r["seeks"][i]["audio_server_ms"].as_f64()));
                    m
                })
                .collect();
            e["seeks"] = json!(merged);
        }
        e["runs"] = json!(rs);
        files_out.insert(f.clone(), e);
    }
    let mut result = json!({ "tool": "flux", "started": started, "preset": preset, "runs": runs, "load_avg_start": load_start, "files": files_out,
        "note": "medians over runs; each run is a fresh hls-serve (cold index, cold caches); OS page cache not dropped" });
    if do_sweep {
        let _srv = start_server(&serve_bin, &media, &files, port, &preset)?;
        let mut http = Http::new(&format!("127.0.0.1:{port}"));
        let before = stats(&mut http)?;
        let mut all_video = Vec::new();
        let mut sweep_out = serde_json::Map::new();
        let mut total_bytes = 0u64;
        let mut all_audio = Vec::new();
        for f in &files {
            let (v, a, b) = sweep(&mut http, f, &preset)?;
            sweep_out.insert(f.clone(), json!({ "video_ms_p50": pct(&v, 0.5), "video_ms_p99": pct(&v, 0.99), "audio_ms_p50": pct(&a, 0.5), "audio_ms_p99": pct(&a, 0.99), "bytes": b, "segments": v.len() }));
            all_video.extend(v);
            all_audio.extend(a);
            total_bytes += b;
        }
        let after = stats(&mut http)?;
        let cpu_self_ms = (after["cpu_self_us"].as_f64().unwrap_or(0.0)
            - before["cpu_self_us"].as_f64().unwrap_or(0.0))
            / 1000.0;
        let cpu_child_ms = (after["cpu_children_us"].as_f64().unwrap_or(0.0)
            - before["cpu_children_us"].as_f64().unwrap_or(0.0))
            / 1000.0;
        let mb = total_bytes as f64 / 1e6;
        result["sweep"] = json!({
            "per_file": sweep_out, "video_segment_server_ms_p50": pct(&all_video, 0.5), "video_segment_server_ms_p99": pct(&all_video, 0.99),
            "audio_segment_server_ms_p50": pct(&all_audio, 0.5), "audio_segment_server_ms_p99": pct(&all_audio, 0.99),
            "bytes": total_bytes, "cpu_self_ms": cpu_self_ms, "cpu_ffmpeg_ms": cpu_child_ms,
            "cpu_self_ms_per_mb": if mb > 0.0 { cpu_self_ms / mb } else { 0.0 },
            "server_max_rss_mb": after["max_rss_bytes"].as_f64().unwrap_or(0.0) / 1e6,
            "transcode": after["transcode"],
        });
    }
    result["load_avg_end"] = json!(load("end"));
    match &out {
        Some(out) => {
            std::fs::create_dir_all(out)?;
            let path = out.join(format!(
                "flux-hls-playback-{}.json",
                started.replace([':', '-'], "").replace('T', "-")
            ));
            std::fs::write(&path, serde_json::to_vec_pretty(&result)?)?;
            eprintln!("wrote {}", path.display());
        }
        None => eprintln!("results not written (pass --out DIR or set FLUX_BENCH_RESULTS)"),
    }
    println!("{}", serde_json::to_string_pretty(&summary(&result))?);
    Ok(())
}

fn summary(r: &Value) -> Value {
    let mut m = serde_json::Map::new();
    if let Some(files) = r["files"].as_object() {
        for (f, e) in files {
            m.insert(f.clone(), json!({ "decision": e["decision"], "ttfs_s": e["ttfs_s"], "ttfs_av_s": e["ttfs_av_s"], "index_ms": e["index_ms"],
                "seeks_s": e["seeks"].as_array().map(|s| s.iter().map(|x| json!([x["target_s"], x["latency_s"], x["audio_latency_s"]])).collect::<Vec<_>>()) }));
        }
    }
    json!({ "files": m, "sweep": r.get("sweep").cloned().unwrap_or(Value::Null) })
}

/// UTC timestamp `YYYY-MM-DDTHH:MM:SS` without a date crate.
fn chrono_like_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}
