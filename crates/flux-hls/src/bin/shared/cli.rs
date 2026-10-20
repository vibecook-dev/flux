//! Argument handling shared by the dev tools (`hls-validate`, `hls-serve`):
//! `--help`, unknown flags as errors, inputs that must exist, and results
//! files that are timestamped and never overwritten without `--force`.
//!
//! Included with `#[path]` from each binary (not a binary itself: files in
//! `src/bin/` subdirectories without a `main.rs` are not auto-discovered).

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

/// What a command line asked for.
pub enum Parsed<T> {
    /// Run with these options.
    Run(T),
    /// `-h`/`--help`: print the usage and exit successfully.
    Help,
}

/// Errors for a flag the tool doesn't know (a typo used to become an input
/// file named `--hepl`, then a results file listing it).
pub fn unknown_flag(flag: &str, usage: &str) -> anyhow::Error {
    anyhow::anyhow!("unknown option {flag}\n{usage}")
}

/// True for an argument that looks like an option.
pub fn is_flag(a: &str) -> bool {
    a.starts_with('-') && a.len() > 1
}

/// Every input must exist before anything is written.
pub fn check_inputs(paths: &[PathBuf]) -> Result<()> {
    let missing: Vec<String> = paths
        .iter()
        .filter(|p| !p.exists())
        .map(|p| p.display().to_string())
        .collect();
    if !missing.is_empty() {
        bail!("no such file: {}", missing.join(", "));
    }
    Ok(())
}

/// The bench results directory, `$FLUX_BENCH_RESULTS`, if set.
pub fn bench_results() -> Option<PathBuf> {
    std::env::var_os("FLUX_BENCH_RESULTS")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// UTC time as `YYYYMMDD-HHMMSS` (for results file names), without a date crate.
pub fn utc_stamp() -> String {
    stamp_of(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    )
}

/// [`utc_stamp`] for a Unix time in seconds.
pub fn stamp_of(secs: i64) -> String {
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
        "{y:04}{mo:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

/// Where results go: `explicit` if given, else `dir/<prefix>-<UTC stamp>.json`.
/// An existing file is an error unless `force` (a results file is evidence
/// other reports cite; one `--help` must not replace it).
pub fn results_path(dir: &Path, prefix: &str, explicit: Option<PathBuf>, force: bool) -> Result<PathBuf> {
    let path = explicit.unwrap_or_else(|| dir.join(format!("{prefix}-{}.json", utc_stamp())));
    if path.exists() && !force {
        bail!(
            "{} exists; pass --force to overwrite it, or --json for another path",
            path.display()
        );
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps() {
        assert_eq!(stamp_of(0), "19700101-000000");
        // 2026-10-20 04:31:18 UTC
        assert_eq!(stamp_of(1_792_470_678), "20261020-043118");
    }

    #[test]
    fn results_are_never_overwritten_silently() {
        let dir = std::env::temp_dir().join(format!("flux-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("r.json");
        std::fs::write(&p, b"{}").unwrap();
        assert!(results_path(&dir, "x", Some(p.clone()), false).is_err());
        assert_eq!(results_path(&dir, "x", Some(p.clone()), true).unwrap(), p);
        let auto = results_path(&dir, "flux-hls-validate", None, false).unwrap();
        let name = auto.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            name.starts_with("flux-hls-validate-2") && name.ends_with(".json"),
            "{name}"
        );
        assert!(check_inputs(&[dir.join("missing.mkv")]).is_err());
        assert!(check_inputs(std::slice::from_ref(&p)).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
