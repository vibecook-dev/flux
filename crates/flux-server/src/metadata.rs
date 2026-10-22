//! Where titles and artwork come from: TMDB online (the server's key), the
//! offline TMDB ID exports, or neither (file names and video frames only).
//! `GET /api/v1/server` reports it ([`flux_core::api::MetadataInfo`]), the
//! startup log says so when there is none, and the job loop matches with it.

use std::path::{Path, PathBuf};

use flux_core::api::{MetadataInfo, MetadataSource};
use flux_meta::tmdb::TmdbAuth;

/// The source matching would use now: a TMDB key in the environment wins,
/// then TMDB ID exports in `exports_dir`.
pub fn source(exports_dir: &Path) -> MetadataSource {
    if TmdbAuth::from_env().is_some() {
        MetadataSource::Tmdb
    } else if has_exports(exports_dir) {
        MetadataSource::Exports
    } else {
        MetadataSource::None
    }
}

/// [`source`] as `ServerInfo::metadata`.
pub fn info(exports_dir: &Path) -> MetadataInfo {
    MetadataInfo {
        source: source(exports_dir),
    }
}

/// The newest movie and TV export files in `dir` (`movie_ids_*.json.gz`,
/// `tv_series_ids_*.json.gz`), if any.
pub fn export_files(dir: &Path) -> (Option<PathBuf>, Option<PathBuf>) {
    let latest = |prefix: &str| -> Option<PathBuf> {
        std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.starts_with(prefix) && n.ends_with(".json.gz")
            })
            .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
            .map(|e| e.path())
    };
    (latest("movie_ids_"), latest("tv_series_ids_"))
}

fn has_exports(dir: &Path) -> bool {
    let (movie, tv) = export_files(dir);
    movie.is_some() || tv.is_some()
}

/// Logs once at startup, at info, when nothing can match titles, so that
/// "the titles are just the file names" has an explanation in the log.
pub fn log_startup(exports_dir: &Path) {
    if source(exports_dir) == MetadataSource::None {
        tracing::info!(
            exports = %exports_dir.display(),
            "no TMDB key or offline exports: titles come from file names \
             (set TMDB_READ_TOKEN or TMDB_API_KEY, or put TMDB's ID exports in the exports folder, then restart)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_are_found_by_name() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(export_files(dir.path()), (None, None));
        assert_eq!(export_files(&dir.path().join("missing")), (None, None));
        std::fs::write(dir.path().join("notes.txt"), b"x").unwrap();
        std::fs::write(dir.path().join("movie_ids_10_18_2026.json"), b"x").unwrap();
        assert!(!has_exports(dir.path()), "only .json.gz exports count");
        std::fs::write(dir.path().join("tv_series_ids_10_18_2026.json.gz"), b"x").unwrap();
        let (movie, tv) = export_files(dir.path());
        assert!(movie.is_none());
        assert!(tv.is_some_and(|p| p.ends_with("tv_series_ids_10_18_2026.json.gz")));
        assert!(has_exports(dir.path()));
        // A TMDB key, if this environment has one, wins over the exports.
        let want = if TmdbAuth::from_env().is_some() {
            MetadataSource::Tmdb
        } else {
            MetadataSource::Exports
        };
        assert_eq!(source(dir.path()), want);
    }
}
