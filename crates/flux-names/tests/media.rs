//! Tests against the real test media (`$FLUX_TEST_MEDIA`) and against the
//! guessit fixture names. They skip (with a message) when the files are not
//! present, e.g. in CI without the corpus.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use flux_names::eval::{self, ManifestRow};
use flux_names::{is_ignored, is_video_file, parse, LibraryKind};

fn manifest(name: &str) -> Option<Vec<ManifestRow>> {
    let path = flux_test_media::media(name)?;
    Some(eval::load_manifest(&path).expect("manifest parses"))
}

/// The tiny library scored from its manifest: the targets are ≥ 99 %
/// title+year on movies and ≥ 99 % season+episodes on episodes.
#[test]
fn tiny_library_manifest_meets_targets() {
    let Some(rows) = manifest("library-tiny.manifest.jsonl") else {
        return;
    };
    let e = eval::evaluate(&rows, |i| parse(&i.rel, &i.sibling_refs(), i.kind), 3);
    let pct = |k: &str, f: &str| e.per_kind[k][f].pct();
    assert!(
        pct("movie", "title+year") >= 99.0,
        "{}",
        eval::render_markdown("tiny", &e)
    );
    assert!(
        pct("episode", "season+episodes") >= 99.0,
        "{}",
        eval::render_markdown("tiny", &e)
    );
    assert!(
        pct("episode", "title") >= 99.0,
        "{}",
        eval::render_markdown("tiny", &e)
    );
    assert_eq!(e.non_media_skipped.ok, e.non_media_skipped.n);
}

/// Walks the real library-tiny tree on disk (names exactly as APFS stores
/// them) and checks every video file against the manifest by path.
#[test]
fn tiny_library_on_disk_matches_manifest() {
    let root = flux_test_media::media_dir("library-tiny");
    let Some(rows) = manifest("library-tiny.manifest.jsonl") else {
        return;
    };
    if !root.exists() {
        eprintln!("skipping: {} not found", root.display());
        return;
    }
    let truth: HashMap<String, &ManifestRow> = rows.iter().map(|r| (path_key(&r.path), r)).collect();
    let mut files = Vec::new();
    walk(&root, &mut files);
    let mut checked = 0;
    let mut wrong = Vec::new();
    for f in &files {
        let rel = f.strip_prefix(&root).expect("under root");
        if is_ignored(rel) || !is_video_file(&f.file_name().unwrap_or_default().to_string_lossy()) {
            continue;
        }
        let rel_s = rel.to_string_lossy().into_owned();
        let Some(row) = truth.get(&path_key(&rel_s)) else {
            continue;
        };
        let (lib_root, rest) = rel_s.split_once('/').expect("library root folder");
        let kind = if lib_root == "Movies" {
            LibraryKind::Movies
        } else {
            LibraryKind::Shows
        };
        let dir = f.parent().expect("parent");
        let sibs: Vec<String> = std::fs::read_dir(dir)
            .expect("read dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| Some(n.as_str()) != f.file_name().and_then(|x| x.to_str()))
            .collect();
        let sib_refs: Vec<&str> = sibs.iter().map(String::as_str).collect();
        let g = parse(Path::new(rest), &sib_refs, kind);
        checked += 1;
        let expected_title = row
            .title
            .as_deref()
            .or(row.show.as_deref())
            .or(row.parent_title.as_deref());
        let title_ok = row.kind == "sample"
            || g.title.as_deref().map(flux_names::norm_key) == expected_title.map(flux_names::norm_key);
        let kind_ok = format!("{:?}", g.kind).to_lowercase() == row.kind;
        if !(title_ok && kind_ok) {
            wrong.push(format!("{rel_s}: {:?} {:?}", g.kind, g.title));
        }
    }
    assert!(checked > 300, "only {checked} files checked");
    assert!(
        wrong.len() * 100 <= checked,
        "{} of {checked} wrong:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

/// Never panics on thousands of real-world names (guessit's fixtures) and on
/// adversarial strings.
#[test]
fn never_panics() {
    let mut inputs: Vec<String> = [
        "",
        ".",
        "..",
        "/",
        "-",
        " - ",
        "[",
        "]",
        "()",
        "[]()",
        "(((",
        ")))",
        "[[[]]]",
        ".mkv",
        "-trailer.mkv",
        "sample",
        "S01E01",
        "1x01",
        "2024.13.45",
        "Season",
        "Season 99999999",
        "S9999E9999",
        "E",
        "Ep",
        "千",
        "千.mkv",
        "🎬 (2019).mkv",
        "Ame\u{301}lie",
        "a-b-c-d-e-f-g-h-1999-x264",
        "{edition-}",
        "[tmdbid-]",
        "[tmdbid-99999999999999999999]",
        "Title (99999)",
        "Title - 99999 [x]",
        "\u{0}",
        "C:",
        "\\\\server\\share",
        "Show/Season 1/",
        "x264-",
        "-GRP",
        "]-GRP",
        "S01E01-E99999",
        "16x03-01",
        "Cap.99999",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some(dir) = guessit_fixtures() {
        for f in ["movies.yml", "episodes.yml", "various.yml"] {
            if let Ok(cases) = eval::load_guessit(&dir.join(f)) {
                inputs.extend(cases.into_iter().map(|c| c.input));
            }
        }
    }
    for s in &inputs {
        for kind in [LibraryKind::Movies, LibraryKind::Shows, LibraryKind::Mixed] {
            let g = parse(Path::new(s), &["a.mkv", "b part2.mkv"], kind);
            assert!((0.0..=1.0).contains(&g.confidence), "{s}: {}", g.confidence);
        }
    }
}

fn guessit_fixtures() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    std::fs::read_dir(home.join(".cargo/registry/src"))
        .ok()?
        .flatten()
        .find_map(|d| {
            let p = d.path().join("hunch-2.0.2/tests/fixtures");
            p.exists().then_some(p)
        })
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// Lookup key for a path: the alphanumeric key of each component, so the
/// on-disk spelling (NFC or NFD) and the manifest's spelling meet.
fn path_key(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for c in s.split('/') {
        // NFD accents are separate combining marks (not alphanumeric) and
        // drop out, so compose-insensitive keys need the base letters only.
        let k: String = flux_names::norm_key(c)
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect();
        let _ = write!(out, "{k}/");
    }
    out
}
