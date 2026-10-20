//! Parser-free evaluation of the offline export index + matcher on the
//! REAL_MOVIES ground truth. Skips when no export is found: the newest
//! `movie_ids_*.json.gz` in `$FLUX_TMDB_EXPORTS` (TMDB's daily ID exports).
//! Numbers are recorded by `examples/meta_bench.rs export`; this test guards
//! against regressions.

use std::path::PathBuf;

use flux_meta::eval::{evaluate_movies, load_truth, FailureClass};
use flux_meta::export_index::{BuildOptions, ExportIndex, ExportKind};
use flux_meta::matcher::Matcher;

/// The newest movie export in `$FLUX_TMDB_EXPORTS`, or `None` (with why).
fn export() -> Option<PathBuf> {
    let Some(dir) = std::env::var_os("FLUX_TMDB_EXPORTS").filter(|v| !v.is_empty()) else {
        eprintln!("skipping: FLUX_TMDB_EXPORTS is not set");
        return None;
    };
    let newest = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("movie_ids_") && n.ends_with(".json.gz"))
        })
        .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    if newest.is_none() {
        eprintln!(
            "skipping: no movie_ids_*.json.gz in {}",
            PathBuf::from(dir).display()
        );
    }
    newest
}

#[test]
fn real_movies_top1_regression_floor() {
    let Some(export) = export() else {
        return;
    };
    let idx = ExportIndex::build_from_gz(&export, ExportKind::Movie, BuildOptions::default()).unwrap();
    assert!(idx.len() > 1_000_000);
    let truth = load_truth(include_str!("fixtures/real_movies_truth.json")).unwrap();
    let r = evaluate_movies(&idx, &Matcher::default(), &truth, 10);
    assert_eq!(r.evaluated + r.excluded, truth.len());
    // Measured 2026-10-19: 95/131 top-1 (72.5%), recall@10 77.9%.
    assert!(r.top1_accuracy >= 0.70, "top-1 {:.3}", r.top1_accuracy);
    assert!(r.recall_at_k >= 0.75, "recall {:.3}", r.recall_at_k);
    // Export candidates have no year or runtime, so nothing may auto-accept on them.
    assert_eq!(r.wrong_auto_accepted, 0);
    assert_eq!(r.auto_accepted, 0);
    // Translated titles are the dominant offline failure (the export has original titles only).
    let translated = r
        .failures_by_class
        .iter()
        .find(|(c, _)| *c == FailureClass::TranslatedTitle)
        .map_or(0, |(_, n)| *n);
    assert!(translated >= 20);
    // Spot checks: exact original titles and the popularity prior.
    let find = |t: &str, y: u16| r.cases.iter().find(|c| c.title == t && c.year == y).unwrap();
    assert!(find("기생충", 2019).correct);
    assert!(find("The Matrix", 1999).correct);
    assert!(
        !find("King Kong", 1933).correct,
        "remakes need a year (not in the export)"
    );
}
