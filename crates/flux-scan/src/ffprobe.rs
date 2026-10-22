//! The ffprobe fallback for containers flux-probe does not read
//! (`ProbeError::UnknownFormat`: FLV, MPEG-PS/VOB, ASF/WMV, OGM, RMVB…) and
//! files it finds malformed, which ffmpeg's demuxers often still read.
//!
//! flux-probe's own `ffprobe::probe` has no timeout. Here every run is
//! bounded: at most `ffprobe_workers` processes at once (the pool in
//! `process.rs`), and a process still running after `ffprobe_timeout`, or
//! whose scan is cancelled (shutdown), is killed and reaped. The binary is
//! `ScanConfig::ffprobe` (the server passes the one it resolved).

use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use flux_core::ProbeResult;

/// Most of ffprobe's output (each stream) a run keeps. A real file's JSON is
/// a few KiB, tens with many chapters; a hostile one's is not (an MP4 whose
/// `hdlr` name is 96 MiB of a legacy code page, which flux-probe calls
/// malformed: buffering and parsing its JSON peaked at ~1.1 GiB).
const MAX_OUTPUT: u64 = 16 << 20;

/// Reads `r` to its end, keeping its first `max` bytes; true when it held
/// more. The rest is drained, so the child never blocks on a full pipe.
fn read_capped(r: &mut dyn Read, max: u64) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let _ = (&mut *r).take(max).read_to_end(&mut buf);
    let more = std::io::copy(r, &mut std::io::sink()).unwrap_or(0) > 0;
    (buf, more)
}

/// One ffprobe run with a deadline. Stdout/stderr are drained on helper
/// threads so a chatty child can't block on a full pipe. `cancelled` is
/// polled with the child: when it turns true (the scan was cancelled, the
/// scanner is shutting down) the child is killed at once.
pub(crate) fn run(
    bin: &Path,
    path: &Path,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
) -> Result<ProbeResult, String> {
    let mut child = flux_core::tool::command(bin)
        .args(flux_probe::ffprobe::DEFAULT_ARGS)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ffprobe ({}): spawn: {e}", bin.display()))?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || match pipe {
            Some(mut p) => read_capped(&mut p, MAX_OUTPUT),
            None => (Vec::new(), false),
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() >= timeout || cancelled() => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout.join();
                let _ = stderr.join();
                return Err(if cancelled() {
                    "ffprobe: cancelled (killed)".to_string()
                } else {
                    format!("ffprobe: timed out after {} ms (killed)", timeout.as_millis())
                });
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("ffprobe: wait: {e}"));
            }
        }
    };
    let (out, more) = stdout.join().unwrap_or_default();
    let (err, _) = stderr.join().unwrap_or_default();
    if !status.success() {
        return Err(format!(
            "ffprobe: exit {status}: {}",
            String::from_utf8_lossy(&err).trim()
        ));
    }
    if more {
        return Err(format!(
            "ffprobe: output over {} MiB (not read)",
            MAX_OUTPUT >> 20
        ));
    }
    let v: serde_json::Value = serde_json::from_slice(&out).map_err(|e| format!("ffprobe: bad JSON: {e}"))?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let mut r = flux_probe::ffprobe::from_json(&v, ext);
    if r.tracks.is_empty() && r.duration_ms.is_none() {
        return Err("ffprobe: no streams".into());
    }
    r.warnings.push("probed by ffprobe".into());
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Output is kept up to the cap, and the rest drained.
    #[test]
    fn output_is_read_up_to_a_cap() {
        let mut long = std::io::Cursor::new(vec![7u8; 100]);
        let (b, more) = read_capped(&mut long, 64);
        assert_eq!((b.len(), more, long.position()), (64, true, 100));
        let (b, more) = read_capped(&mut std::io::Cursor::new(vec![7u8; 64]), 64);
        assert_eq!((b.len(), more), (64, false));
    }

    #[test]
    fn missing_binary_and_garbage_fail_cleanly() {
        let dir = std::env::temp_dir().join(format!("flux-scan-ffprobe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let junk = dir.join("junk.flv");
        std::fs::write(&junk, b"not a media file at all").unwrap();
        // Garbage: ffprobe exits non-zero (or the binary is absent): an Err, never a panic.
        let bin = std::path::PathBuf::from(flux_probe::ffprobe::ffprobe_bin());
        assert!(run(&bin, &junk, Duration::from_secs(20), &|| false).is_err());
        // A binary that isn't there names itself in the error.
        let err = run(
            Path::new("/nonexistent/ffprobe"),
            &junk,
            Duration::from_secs(20),
            &|| false,
        )
        .unwrap_err();
        assert!(err.contains("/nonexistent/ffprobe"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ROB-4: a hung ffprobe is killed as soon as its scan is cancelled,
    /// not when its (20 s) timeout runs out.
    #[cfg(unix)]
    #[test]
    fn a_cancelled_run_kills_the_child_at_once() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("flux-scan-ffprobe-hang-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("ffprobe");
        std::fs::write(&fake, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let junk = dir.join("junk.flv");
        std::fs::write(&junk, b"x").unwrap();
        let t = Instant::now();
        let flag = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(200));
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            });
            let err = run(&fake, &junk, Duration::from_secs(20), &|| {
                flag.load(std::sync::atomic::Ordering::SeqCst)
            })
            .unwrap_err();
            assert!(err.contains("cancelled"), "{err}");
        });
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
