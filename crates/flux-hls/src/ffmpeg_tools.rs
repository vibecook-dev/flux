//! Locating the ffmpeg and ffprobe binaries (flux runs them as separate
//! processes; it never links libav*).

use std::path::{Path, PathBuf};

/// Variables naming the binaries.
pub const FFMPEG_VAR: &str = "FLUX_FFMPEG";
pub const FFPROBE_VAR: &str = "FLUX_FFPROBE";

/// `name` with the platform's executable suffix.
fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// `name` on `PATH`, else in jellyfin-ffmpeg's `/usr/lib/jellyfin-ffmpeg`
/// (the Docker image's), `/opt/homebrew/bin`, `/usr/local/bin` or `/usr/bin`.
pub fn find(name: &str) -> Option<PathBuf> {
    let file = exe(name);
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let p = dir.join(&file);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    [
        "/usr/lib/jellyfin-ffmpeg",
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
    ]
    .iter()
    .map(|d| Path::new(d).join(&file))
    .find(|p| p.is_file())
}

/// [`find`], else the bare name (so the error names the missing tool).
pub fn find_or_name(name: &str) -> PathBuf {
    find(name).unwrap_or_else(|| PathBuf::from(exe(name)))
}

/// ffmpeg: `$FLUX_FFMPEG`, else [`find_or_name`].
pub fn default_ffmpeg() -> PathBuf {
    std::env::var_os(FFMPEG_VAR)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| find_or_name("ffmpeg"))
}

/// ffprobe: `$FLUX_FFPROBE`, else the one next to `ffmpeg`, else
/// [`find_or_name`].
pub fn default_ffprobe(ffmpeg: &Path) -> PathBuf {
    if let Some(p) = std::env::var_os(FFPROBE_VAR).filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    let sibling = ffmpeg.with_file_name(exe("ffprobe"));
    if ffmpeg.parent().is_some_and(|p| !p.as_os_str().is_empty()) && sibling.is_file() {
        return sibling;
    }
    find_or_name("ffprobe")
}

/// The first line of `<tool> -version`, or why it could not run.
pub fn version(tool: &Path) -> Result<String, String> {
    let out = std::process::Command::new(tool)
        .args(["-hide_banner", "-version"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {}: {e}", tool.display()))?;
    if !out.status.success() {
        return Err(format!("{} -version: {}", tool.display(), out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string())
}
