//! Probe throughput: flux-probe (in-process, 1 and N threads) vs spawning
//! ffprobe with Jellyfin's arguments on a sample.
//!
//! ```text
//! probe-bench <dir> [--threads N] [--ffprobe-sample 200] [--read-size BYTES] [--out results.json] [--no-ffprobe]
//! ```
//!
//! Writes `$FLUX_BENCH_RESULTS/flux-probe-bench-<dir>.json` by default
//! (printed only when neither that nor `--out` is given). The first flux pass runs with whatever the OS page cache holds
//! ("first"); the second is warm. ffprobe runs after flux, so it is warm too.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use flux_probe::{
    ffprobe, is_media_path, probe_file_with, probe_source, ContainerHint, ProbeOptions, RangeRead,
};
use serde_json::{json, Value};

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

#[derive(Default, Clone, Copy)]
struct Sample {
    us: u64,
    bytes: u64,
    reads: u32,
    ok: bool,
}

/// Run `f` over `files` on `threads` threads; returns per-file samples and wall time.
fn run<F>(files: &[PathBuf], threads: usize, f: F) -> (Vec<Sample>, Duration)
where
    F: Fn(&Path) -> Sample + Sync,
{
    let next = AtomicUsize::new(0);
    let out = Mutex::new(Vec::with_capacity(files.len()));
    let start = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads.max(1) {
            s.spawn(|| {
                let mut local = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(p) = files.get(i) else { break };
                    local.push(f(p));
                }
                if let Ok(mut o) = out.lock() {
                    o.extend(local);
                }
            });
        }
    });
    let wall = start.elapsed();
    (out.into_inner().unwrap_or_default(), wall)
}

fn stats(samples: &[Sample], wall: Duration) -> Value {
    let mut us: Vec<u64> = samples.iter().map(|s| s.us).collect();
    us.sort_unstable();
    let n = us.len().max(1);
    let pct = |p: usize| {
        us.get((us.len() * p / 100).min(us.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0)
    };
    let ok = samples.iter().filter(|s| s.ok).count();
    let okn = ok.max(1) as f64;
    let bytes: u64 = samples.iter().filter(|s| s.ok).map(|s| s.bytes).sum();
    let reads: u64 = samples.iter().filter(|s| s.ok).map(|s| u64::from(s.reads)).sum();
    let mut reads_sorted: Vec<u32> = samples.iter().filter(|s| s.ok).map(|s| s.reads).collect();
    reads_sorted.sort_unstable();
    json!({
        "files": samples.len(),
        "errors": samples.len() - ok,
        "wall_s": wall.as_secs_f64(),
        "files_per_s": samples.len() as f64 / wall.as_secs_f64().max(1e-9),
        "us_p50": pct(50),
        "us_p90": pct(90),
        "us_p99": pct(99),
        "us_max": us.last().copied().unwrap_or(0),
        "us_mean": us.iter().sum::<u64>() as f64 / n as f64,
        "bytes_per_file_mean": bytes as f64 / okn,
        "reads_per_file_mean": reads as f64 / okn,
        "reads_per_file_p50": reads_sorted.get(reads_sorted.len() / 2).copied().unwrap_or(0),
        "reads_per_file_max": reads_sorted.last().copied().unwrap_or(0),
    })
}

fn flux_one(p: &Path, opts: &ProbeOptions) -> Sample {
    let t = Instant::now();
    let r = probe_file_with(p, opts);
    let us = t.elapsed().as_micros() as u64;
    match r {
        Ok(r) => Sample {
            us,
            bytes: r.bytes_read,
            reads: r.reads,
            ok: true,
        },
        Err(_) => Sample {
            us,
            ok: false,
            ..Sample::default()
        },
    }
}

fn ffprobe_one(p: &Path) -> Sample {
    let t = Instant::now();
    let ok = Command::new(ffprobe::ffprobe_bin())
        .args(ffprobe::JELLYFIN_ARGS)
        .arg(p)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    Sample {
        us: t.elapsed().as_micros() as u64,
        ok,
        ..Sample::default()
    }
}

/// Head (2 MiB) + tail (1 MiB) of a file in memory, zeros between: lets us
/// time the parser with no I/O at all.
struct Sparse {
    size: u64,
    head: Vec<u8>,
    tail_off: u64,
    tail: Vec<u8>,
}

impl Sparse {
    fn load(p: &Path) -> Option<Self> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(p).ok()?;
        let size = f.metadata().ok()?.len();
        let mut head = Vec::new();
        f.by_ref().take(2 << 20).read_to_end(&mut head).ok()?;
        let tail_off = size.saturating_sub(1 << 20).max(head.len() as u64);
        f.seek(SeekFrom::Start(tail_off)).ok()?;
        let mut tail = Vec::new();
        f.read_to_end(&mut tail).ok()?;
        Some(Sparse {
            size,
            head,
            tail_off,
            tail,
        })
    }
}

impl RangeRead for &Sparse {
    fn size(&self) -> u64 {
        self.size
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let end = (offset + buf.len() as u64).min(self.size);
        let n = end.saturating_sub(offset) as usize;
        let out = &mut buf[..n];
        out.fill(0);
        // Copy the overlaps with the head and tail runs.
        let (o, e) = (offset as usize, end as usize);
        let h_end = self.head.len().min(e);
        if o < h_end {
            out[..h_end - o].copy_from_slice(&self.head[o..h_end]);
        }
        let t0 = (self.tail_off as usize).max(o);
        if t0 < e {
            let src = &self.tail[t0 - self.tail_off as usize..e - self.tail_off as usize];
            out[t0 - o..e - o].copy_from_slice(src);
        }
        Ok(n)
    }
}

/// Median-of-5 in-memory probe time per file (pure parse CPU, no syscalls).
fn parse_only(files: &[PathBuf], opts: &ProbeOptions) -> Value {
    let mut per_file = Vec::new();
    for p in files {
        let Some(sp) = Sparse::load(p) else { continue };
        let hint = ContainerHint::from_path(p);
        let mut t: Vec<u64> = (0..5)
            .map(|_| {
                let s = Instant::now();
                let _ = probe_source(&sp, hint, opts);
                s.elapsed().as_nanos() as u64
            })
            .collect();
        t.sort_unstable();
        per_file.push(t[2]);
    }
    per_file.sort_unstable();
    let pct = |q: usize| per_file.get(per_file.len() * q / 100).copied().unwrap_or(0) as f64 / 1000.0;
    json!({ "files": per_file.len(), "us_p50": pct(50), "us_p90": pct(90), "us_p99": pct(99),
            "us_max": per_file.last().copied().unwrap_or(0) as f64 / 1000.0 })
}

fn load_avg() -> String {
    Command::new("uptime")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let Some(dir) = args.first().filter(|a| !a.starts_with("--")).cloned() else {
        eprintln!(
            "usage: probe-bench <dir> [--threads N] [--ffprobe-sample 200] [--out f.json] [--no-ffprobe]"
        );
        std::process::exit(2);
    };
    let threads: usize = opt("--threads").and_then(|t| t.parse().ok()).unwrap_or(8);
    let sample_n: usize = opt("--ffprobe-sample")
        .and_then(|t| t.parse().ok())
        .unwrap_or(200);
    let no_ffprobe = args.iter().any(|a| a == "--no-ffprobe");
    let read_size: u64 = opt("--read-size").and_then(|t| t.parse().ok()).unwrap_or(0);
    let opts = ProbeOptions {
        read_size,
        ..ProbeOptions::default()
    };
    let flux = |p: &Path| flux_one(p, &opts);
    let base = Path::new(&dir)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("dir")
        .to_string();
    let out_path = opt("--out").or_else(|| {
        std::env::var("FLUX_BENCH_RESULTS")
            .ok()
            .filter(|d| !d.is_empty())
            .map(|d| format!("{d}/flux-probe-bench-{base}.json"))
    });

    let mut files = Vec::new();
    walk(Path::new(&dir), &mut files);
    eprintln!("{} media files under {dir}", files.len());
    let load_before = load_avg();

    let (s, w) = run(&files, 1, flux);
    let flux_first = stats(&s, w);
    eprintln!("flux 1 thread (first pass): {}", flux_first);
    let (s, w) = run(&files, 1, flux);
    let flux_warm = stats(&s, w);
    eprintln!("flux 1 thread (warm):       {}", flux_warm);
    let (s, w) = run(&files, threads, flux);
    let flux_multi = stats(&s, w);
    eprintln!("flux {threads} threads (warm):     {}", flux_multi);

    // Pure parse cost on a sample (no I/O).
    let pstep = (files.len() / sample_n.max(1)).max(1);
    let psample: Vec<PathBuf> = files
        .iter()
        .step_by(pstep)
        .take(sample_n.max(1))
        .cloned()
        .collect();
    let flux_parse = parse_only(&psample, &opts);
    eprintln!("flux parse-only (in memory): {flux_parse}");

    let mut ffprobe_v = Value::Null;
    if !no_ffprobe && sample_n > 0 && !files.is_empty() {
        let step = (files.len() / sample_n).max(1);
        let sample: Vec<PathBuf> = files.iter().step_by(step).take(sample_n).cloned().collect();
        let (s1, w1) = run(&sample, 1, ffprobe_one);
        let ff1 = stats(&s1, w1);
        eprintln!("ffprobe 1 thread:  {ff1}");
        let (sn, wn) = run(&sample, threads, ffprobe_one);
        let ffn = stats(&sn, wn);
        eprintln!("ffprobe {threads} threads: {ffn}");
        // flux on exactly the same sample, for a like-for-like ratio.
        let (fs, fw) = run(&sample, 1, flux);
        let flux_same = stats(&fs, fw);
        let ratio = |a: &Value, b: &Value, k: &str| {
            let (x, y) = (a[k].as_f64().unwrap_or(0.0), b[k].as_f64().unwrap_or(0.0));
            if y > 0.0 {
                x / y
            } else {
                0.0
            }
        };
        ffprobe_v = json!({
            "args": ffprobe::JELLYFIN_ARGS,
            "binary": ffprobe::ffprobe_bin(),
            "sample_files": sample.len(),
            "single": ff1,
            "multi": ffn,
            "flux_single_same_sample": flux_same,
            "speedup_p50_single": ratio(&ff1, &flux_same, "us_p50"),
            "speedup_throughput_single": ratio(&flux_same, &ff1, "files_per_s"),
            "speedup_throughput_multi": ratio(&flux_multi, &ffn, "files_per_s"),
        });
    }
    let result = json!({
        "tool": "flux-probe probe-bench",
        "dir": dir,
        "files": files.len(),
        "threads": threads,
        "read_size": if read_size == 0 { flux_probe::PAGE } else { read_size },
        "started_load": load_before,
        "finished_load": load_avg(),
        "note": "first = first flux pass with the OS cache as found; warm = immediately repeated",
        "flux": { "single_first": flux_first, "single_warm": flux_warm, "multi_warm": flux_multi, "parse_only_in_memory": flux_parse },
        "ffprobe_jellyfin_args": ffprobe_v,
    });
    match &out_path {
        Some(out_path) => match std::fs::write(
            out_path,
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        ) {
            Ok(()) => eprintln!("wrote {out_path}"),
            Err(e) => eprintln!("cannot write {out_path}: {e}"),
        },
        None => eprintln!("results not written (pass --out or set FLUX_BENCH_RESULTS)"),
    }
    println!("{}", serde_json::to_string_pretty(&result).unwrap_or_default());
}
