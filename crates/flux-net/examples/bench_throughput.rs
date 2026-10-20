//! Throughput and seek-latency benchmark of flux-net's serving path.
//!
//! Serves one synthetic object (default 1 GiB, generated in memory so disk
//! I/O is out of the picture) with HTTP Range support, then fetches it over:
//! - `loopback`: the local listener on 127.0.0.1;
//! - `system_tailnet`: system mode, this machine's own Tailscale address
//!   (skipped when tailscaled isn't running). This is a SAME-HOST path. On
//!   macOS it still loops through the Tailscale network extension (utun,
//!   MTU 1280) — measured: the extension burns CPU per MB — so it includes
//!   Tailscale's userspace packet path, but no real network and no
//!   peer-to-peer WireGuard hop.
//!
//! Per path: full-object Range GET throughput (median of `--runs`), process
//! CPU per MB (client and server share the process), Tailscale daemon CPU
//! per MB where its pid is visible, time to first byte, "seek" latency —
//! 1 MiB Range GETs at random offsets on one keep-alive connection — and the
//! latency of a small GET on a NEW connection (the first one pays the
//! uncached WhoIs; the rest hit the cache).
//!
//! ```text
//! cargo run --release -p flux-net --example bench_throughput -- \
//!   [--bytes 1073741824] [--runs 3] [--seeks 50] [--out FILE]
//! ```
//!
//! `--out` defaults to `$FLUX_BENCH_RESULTS/flux-net-throughput.json`; with
//! neither, the results are printed only.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use flux_net::{FluxNet, FluxNetConfig, SystemConfig, TailnetConfig, TailnetStatus};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const CHUNK: usize = 256 * 1024;

#[derive(Clone)]
struct Blob {
    len: u64,
    pattern: Bytes,
}

/// `bytes=a-b` / `bytes=a-` → inclusive range within `len`.
fn parse_range(headers: &HeaderMap, len: u64) -> Option<(u64, u64)> {
    let spec = headers
        .get(header::RANGE)?
        .to_str()
        .ok()?
        .strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?;
    let start: u64 = a.parse().ok()?;
    let end: u64 = if b.is_empty() {
        len - 1
    } else {
        b.parse::<u64>().ok()?.min(len - 1)
    };
    (start <= end).then_some((start, end))
}

async fn serve_blob(blob: Blob, headers: HeaderMap) -> Response {
    let (start, end, status) = match parse_range(&headers, blob.len) {
        Some((s, e)) => (s, e, StatusCode::PARTIAL_CONTENT),
        None => (0, blob.len - 1, StatusCode::OK),
    };
    let total = end - start + 1;
    let pattern = blob.pattern.clone();
    // Zero-copy chunks: slices of one shared pattern buffer.
    let body = futures::stream::unfold(0u64, move |sent| {
        let pattern = pattern.clone();
        async move {
            if sent >= total {
                return None;
            }
            let n = (total - sent).min(CHUNK as u64) as usize;
            Some((Ok::<_, Infallible>(pattern.slice(..n)), sent + n as u64))
        }
    });
    let mut resp = Response::builder()
        .status(status)
        .header(header::CONTENT_LENGTH, total)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_TYPE, "application/octet-stream");
    if status == StatusCode::PARTIAL_CONTENT {
        resp = resp.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{}", blob.len));
    }
    resp.body(Body::from_stream(body)).unwrap_or_default()
}

/// Process CPU (user+sys) so far (zero where there is no getrusage).
#[cfg(not(unix))]
fn process_cpu() -> Duration {
    Duration::ZERO
}

/// Process CPU (user+sys) so far.
#[cfg(unix)]
fn process_cpu() -> Duration {
    // SAFETY: getrusage writes into the zeroed struct we pass; RUSAGE_SELF is valid.
    #[allow(unsafe_code)]
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };
    let tv =
        |t: libc::timeval| Duration::from_secs(t.tv_sec as u64) + Duration::from_micros(t.tv_usec as u64);
    tv(usage.ru_utime) + tv(usage.ru_stime)
}

/// The Tailscale daemon process (tailscaled or the macOS network extension).
fn daemon_pid() -> Option<u32> {
    for pattern in [
        "io.tailscale.ipn.macsys.network-extension",
        "IPNExtension",
        "tailscaled",
    ] {
        let out = std::process::Command::new("pgrep")
            .args(["-f", pattern])
            .output()
            .ok()?;
        if let Some(pid) = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .and_then(|l| l.trim().parse().ok())
        {
            return Some(pid);
        }
    }
    None
}

/// CPU time of `pid` via `ps` ([[dd-]hh:]mm:ss.cc), readable for root
/// processes without privileges.
fn pid_cpu(pid: u32) -> Option<Duration> {
    let out = std::process::Command::new("ps")
        .args(["-o", "time=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (days, rest) = match text.split_once('-') {
        Some((d, r)) => (d.parse::<f64>().ok()?, r.to_string()),
        None => (0.0, text),
    };
    let secs = rest.split(':').try_fold(0.0f64, |acc, part| {
        part.parse::<f64>().ok().map(|v| acc * 60.0 + v)
    })?;
    Some(Duration::from_secs_f64(days * 86400.0 + secs))
}

/// Read one response head; returns Content-Length.
async fn read_head(stream: &mut TcpStream, buf: &mut Vec<u8>) -> anyhow::Result<(u64, Vec<u8>)> {
    buf.clear();
    let mut byte = [0u8; 1024];
    loop {
        let n = stream.read(&mut byte).await?;
        anyhow::ensure!(n > 0, "connection closed before the response head");
        buf.extend_from_slice(&byte[..n]);
        if let Some(split) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..split]).to_ascii_lowercase();
            anyhow::ensure!(head.starts_with("http/1.1 20"), "unexpected response: {head}");
            let len = head
                .lines()
                .find_map(|l| {
                    l.strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse().ok())
                })
                .ok_or_else(|| anyhow::anyhow!("no content-length"))?;
            return Ok((len, buf[split + 4..].to_vec()));
        }
    }
}

/// GET `range` on `stream` (keep-alive); returns (time to first body byte, total).
async fn range_get(
    stream: &mut TcpStream,
    start: u64,
    end: u64,
    sink: &mut [u8],
) -> anyhow::Result<(Duration, Duration, u64)> {
    let t0 = Instant::now();
    let req = format!("GET /blob HTTP/1.1\r\nHost: flux\r\nRange: bytes={start}-{end}\r\n\r\n");
    stream.write_all(req.as_bytes()).await?;
    let mut head = Vec::with_capacity(1024);
    let (len, already) = read_head(stream, &mut head).await?;
    let mut got = already.len() as u64;
    let mut ttfb = (got > 0).then(|| t0.elapsed());
    while got < len {
        let n = stream.read(sink).await?;
        anyhow::ensure!(n > 0, "short body: {got} of {len}");
        ttfb.get_or_insert_with(|| t0.elapsed());
        got += n as u64;
    }
    Ok((ttfb.unwrap_or_default(), t0.elapsed(), got))
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    if v.is_empty() {
        0.0
    } else {
        v[v.len() / 2]
    }
}

fn percentile(mut v: Vec<f64>, p: f64) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    if v.is_empty() {
        0.0
    } else {
        v[((v.len() - 1) as f64 * p).round() as usize]
    }
}

async fn bench_path(
    name: &str,
    addr: SocketAddr,
    bytes: u64,
    runs: usize,
    seeks: usize,
    daemon: Option<u32>,
) -> anyhow::Result<serde_json::Value> {
    // First, small GETs on fresh connections: identity classification per
    // connection (the very first pays the uncached WhoIs on tailnet paths).
    let mut new_conn_ms = Vec::new();
    for _ in 0..21 {
        let t0 = Instant::now();
        let mut stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        stream
            .write_all(b"GET /ping HTTP/1.1\r\nHost: flux\r\nConnection: close\r\n\r\n")
            .await?;
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).await?;
        anyhow::ensure!(reply.ends_with(b"pong"), "bad /ping reply");
        new_conn_ms.push(t0.elapsed().as_secs_f64() * 1e3);
    }
    let first_new_conn_ms = new_conn_ms.remove(0);

    let mut sink = vec![0u8; CHUNK];
    let mut mbps = Vec::new();
    let mut cpu_ms_per_mb = Vec::new();
    let mut daemon_ms_per_mb = Vec::new();
    let mut ttfb_ms = Vec::new();
    for run in 0..runs {
        let mut stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        let cpu0 = process_cpu();
        let d0 = daemon.and_then(pid_cpu);
        let (ttfb, total, got) = range_get(&mut stream, 0, bytes - 1, &mut sink).await?;
        let cpu = process_cpu() - cpu0;
        let mb = got as f64 / 1e6;
        let rate = got as f64 * 8.0 / total.as_secs_f64() / 1e6;
        mbps.push(rate);
        cpu_ms_per_mb.push(cpu.as_secs_f64() * 1e3 / mb);
        ttfb_ms.push(ttfb.as_secs_f64() * 1e3);
        if let (Some(d0), Some(d1)) = (d0, daemon.and_then(pid_cpu)) {
            daemon_ms_per_mb.push(d1.saturating_sub(d0).as_secs_f64() * 1e3 / mb);
        }
        eprintln!(
            "{name} run {run}: {got} B in {total:?} = {rate:.0} Mb/s, ttfb {ttfb:?}, process CPU {cpu:?}"
        );
    }

    // Seeks: 1 MiB at pseudo-random offsets on one keep-alive connection.
    let mut stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?;
    let mut seek_ms = Vec::new();
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..seeks {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let start = x % (bytes - (1 << 20));
        let (_, total, _) = range_get(&mut stream, start, start + (1 << 20) - 1, &mut sink).await?;
        seek_ms.push(total.as_secs_f64() * 1e3);
    }

    Ok(serde_json::json!({
        "addr": addr.to_string(),
        "new_conn_small_get_ms_first": first_new_conn_ms,
        "new_conn_small_get_ms_p50": percentile(new_conn_ms.clone(), 0.5),
        "new_conn_small_get_ms_p95": percentile(new_conn_ms, 0.95),
        "mbps": mbps,
        "median_mbps": median(mbps.clone()),
        "median_process_cpu_ms_per_mb": median(cpu_ms_per_mb),
        "median_daemon_cpu_ms_per_mb": if daemon_ms_per_mb.is_empty() { serde_json::Value::Null } else { median(daemon_ms_per_mb).into() },
        "median_ttfb_ms": median(ttfb_ms),
        "seek_1mib_ms_p50": percentile(seek_ms.clone(), 0.5),
        "seek_1mib_ms_p95": percentile(seek_ms, 0.95),
    }))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut bytes: u64 = 1 << 30;
    let mut runs = 3usize;
    let mut seeks = 50usize;
    let mut out = std::env::var_os("FLUX_BENCH_RESULTS")
        .filter(|v| !v.is_empty())
        .map(|d| std::path::PathBuf::from(d).join("flux-net-throughput.json"));
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| anyhow::anyhow!("{arg} needs a value"));
        match arg.as_str() {
            "--bytes" => bytes = value()?.parse()?,
            "--runs" => runs = value()?.parse()?,
            "--seeks" => seeks = value()?.parse()?,
            "--out" => out = Some(value()?.into()),
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    anyhow::ensure!(bytes > 2 << 20, "--bytes must exceed 2 MiB");

    let blob = Blob {
        len: bytes,
        pattern: Bytes::from((0..CHUNK).map(|i| (i % 251) as u8).collect::<Vec<_>>()),
    };
    let router = Router::new()
        .route(
            "/blob",
            get(move |headers: HeaderMap| serve_blob(blob.clone(), headers)),
        )
        .route("/ping", get(|| async { "pong" }));
    let config = FluxNetConfig {
        local_addr: "127.0.0.1:0".parse()?,
        tailnet: TailnetConfig::System(SystemConfig {
            port: 0,
            ..Default::default()
        }),
        ..Default::default()
    };
    let net = FluxNet::start(config, router).await?;

    // Wait (briefly) for system mode.
    let mut status = net.tailnet_status();
    let tailnet = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match status.borrow_and_update().clone() {
                TailnetStatus::Running(info) => return info.addrs.into_iter().find(|a| a.is_ipv4()),
                TailnetStatus::Failed { .. } | TailnetStatus::Unavailable { .. } => return None,
                _ => {}
            }
            if status.changed().await.is_err() {
                return None;
            }
        }
    })
    .await
    .ok()
    .flatten();

    let load = std::process::Command::new("uptime")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let mut paths = serde_json::Map::new();
    paths.insert(
        "loopback".into(),
        bench_path("loopback", net.local_addr(), bytes, runs, seeks, None).await?,
    );
    match tailnet {
        Some(addr) => {
            let daemon = daemon_pid();
            paths.insert(
                "system_tailnet".into(),
                bench_path("system_tailnet", addr, bytes, runs, seeks, daemon).await?,
            );
        }
        None => eprintln!("system tailnet unavailable; skipped"),
    }
    let result = serde_json::json!({
        "component": "flux-net",
        "name": "throughput",
        "date": std::process::Command::new("date").arg("-u").arg("+%Y-%m-%dT%H:%M:%SZ").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default(),
        "uptime": load,
        "bytes": bytes,
        "runs": runs,
        "seeks": seeks,
        "build": if cfg!(debug_assertions) { "debug" } else { "release" },
        "paths": paths,
        "notes": "Synthetic in-memory object; client and server in one process (process CPU covers both). \
                  system_tailnet is a same-host request to this machine's own Tailscale IP; on macOS it \
                  loops through the Tailscale network extension (daemon CPU) but crosses no network and \
                  no peer WireGuard hop. new_conn_small_get_ms_first is each path's first connection: on \
                  system_tailnet it includes the uncached WhoIs (CLI). Machine shared by parallel agents: \
                  numbers are noisy.",
    });
    println!("{}", serde_json::to_string_pretty(&result)?);
    match &out {
        Some(out) => {
            if let Some(dir) = out.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(out, serde_json::to_vec_pretty(&result)?)?;
            eprintln!("wrote {}", out.display());
        }
        None => eprintln!("results not written (pass --out or set FLUX_BENCH_RESULTS)"),
    }
    net.shutdown(Duration::from_secs(2)).await;
    Ok(())
}
