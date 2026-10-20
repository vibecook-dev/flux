//! Real test media: the 424-file `library-tiny` corpus (stat facts from the
//! real files, names from the manifest ground truth, probes from the seed
//! files) and the feature clips' ffprobe facts, under `$FLUX_TEST_MEDIA`.
//!
//! Skipped (with a message) when the corpus is not configured.

mod common;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use flux_catalog::synth::{self, ManifestEntry};
use flux_catalog::{
    read, scan_support, Catalog, CatalogConfig, FileUpsert, ItemsQuery, ProbeUpdate, ScanBatch,
};
use flux_core::api::{CreateLibrary, ItemKind};
use flux_core::LibraryKind;

fn tiny() -> PathBuf {
    flux_test_media::media_dir("library-tiny")
}

fn tiny_manifest() -> PathBuf {
    flux_test_media::media_dir("library-tiny.manifest.jsonl")
}

fn features_manifest() -> PathBuf {
    flux_test_media::media_dir("features/manifest.json")
}

fn available(p: &Path) -> bool {
    if p.exists() {
        true
    } else {
        eprintln!("skipping: {} not present", p.display());
        false
    }
}

/// Expected top-level items, seasons and episodes from the ground truth.
fn expected(entries: &[ManifestEntry]) -> (usize, usize, usize, usize) {
    let norm = flux_catalog::group::normalize_title;
    let mut movies = HashSet::new();
    let mut shows = HashSet::new();
    let mut seasons = HashSet::new();
    let mut episodes = HashSet::new();
    let mut extras = 0;
    for e in entries {
        match e.kind.as_str() {
            "movie" => match e.tmdb_id {
                Some(id) => {
                    movies.insert(format!("tmdb:{id}"));
                }
                None => {
                    movies.insert(format!("{}|{:?}", norm(e.title.as_deref().unwrap_or("")), e.year));
                }
            },
            "episode" => {
                let show = format!("{}|{:?}", norm(e.show.as_deref().unwrap_or("")), e.year);
                for ep in &e.episodes {
                    episodes.insert(format!("{show}|{:?}|{ep}", e.season));
                }
                seasons.insert(format!("{show}|{:?}", e.season));
                shows.insert(show);
            }
            "extra" => extras += 1,
            _ => {}
        }
    }
    (movies.len() + shows.len(), seasons.len(), episodes.len(), extras)
}

fn scan_tiny(entries: &[ManifestEntry], seeds: &HashMap<String, flux_core::ProbeResult>) -> Vec<FileUpsert> {
    entries
        .iter()
        .filter(|e| e.is_media())
        .map(|e| {
            let md = std::fs::metadata(tiny().join(&e.path)).unwrap();
            FileUpsert {
                facts: synth::facts_from_metadata(&e.path, &md),
                probe: synth::probe_for_entry(e, seeds)
                    .map(|p| ProbeUpdate::Probed(Box::new(p)))
                    .unwrap_or_default(),
                guess: synth::guess_from_manifest(e),
            }
        })
        .collect()
}

#[test]
fn library_tiny_groups_like_the_ground_truth_and_rescan_is_a_no_op() {
    if !available(&tiny_manifest()) {
        return;
    }
    let entries = synth::read_manifest(&tiny_manifest()).unwrap();
    let seeds = synth::seed_probes();
    let dir = tempfile::tempdir().unwrap();
    let cat = Catalog::open(dir.path().join("c.db"), CatalogConfig::default()).unwrap();
    let lib = cat
        .writer()
        .create_library(CreateLibrary {
            name: "Tiny".into(),
            kind: LibraryKind::Mixed,
            roots: vec![tiny().to_string_lossy().into_owned()],
        })
        .wait()
        .unwrap();
    let root = cat
        .read_blocking(|c| Ok(scan_support::list_roots(c)?[0].id))
        .unwrap();

    // Walk order is arbitrary: apply in batches of 50 in manifest order.
    let upserts = scan_tiny(&entries, &seeds);
    let media = upserts.len();
    let mut total = flux_catalog::ApplyOutcome::default();
    for chunk in upserts.chunks(50) {
        let out = cat
            .writer()
            .apply(ScanBatch {
                root_id: root,
                upserts: chunk.to_vec(),
                ..Default::default()
            })
            .wait()
            .unwrap();
        total.merge(out);
    }
    assert_eq!(total.files_added as usize, media);
    let samples = entries.iter().filter(|e| e.kind == "sample").count();
    assert_eq!(total.files_ignored as usize, samples);

    let (top_expected, seasons_expected, episodes_expected, extras_expected) = expected(&entries);
    let page = cat
        .read_blocking(|c| {
            read::items_page(
                c,
                &ItemsQuery {
                    library_id: Some(lib.id),
                    limit: 500,
                    ..Default::default()
                },
            )
        })
        .unwrap();
    assert_eq!(page.total, Some(top_expected as u64), "top-level items");
    let count = |kind: &str| -> usize {
        let k = kind.to_string();
        cat.read_blocking(move |c| {
            Ok(
                c.query_row("SELECT count(*) FROM item WHERE kind = ?1", [k], |r| {
                    r.get::<_, i64>(0)
                })?,
            )
        })
        .unwrap() as usize
    };
    assert_eq!(count("season"), seasons_expected);
    assert_eq!(count("episode"), episodes_expected);
    assert_eq!(count("extra"), extras_expected);

    // Every movie in the manifest resolves to exactly one item holding all its files.
    let mut by_item: HashMap<i64, BTreeSet<String>> = HashMap::new();
    let snapshot = cat
        .read_blocking(|c| scan_support::load_root_snapshot(c, root))
        .unwrap();
    for e in entries.iter().filter(|e| e.kind == "movie") {
        let asset = snapshot[&e.path].asset_id.unwrap();
        let item: i64 = cat
            .read_blocking(move |c| {
                Ok(c.query_row("SELECT item_id FROM asset WHERE id = ?1", [asset], |r| r.get(0))?)
            })
            .unwrap();
        by_item
            .entry(item)
            .or_default()
            .insert(format!("{:?}|{:?}", e.title, e.year));
    }
    for (item, keys) in &by_item {
        assert_eq!(keys.len(), 1, "item {item} merged different movies: {keys:?}");
    }

    // Unchanged rescan: stat the real files again and diff in memory → nothing to send.
    let again = scan_tiny(&entries, &seeds);
    let changed: Vec<_> = again
        .iter()
        .filter(|u| {
            snapshot
                .get(&u.facts.rel_path)
                .is_none_or(|row| !row.same_stat(&u.facts))
        })
        .collect();
    assert!(changed.is_empty(), "{} files look changed", changed.len());
    let before = cat
        .writer()
        .stats()
        .commands
        .load(std::sync::atomic::Ordering::Relaxed);
    let out = cat
        .writer()
        .apply(ScanBatch {
            root_id: root,
            ..Default::default()
        })
        .wait()
        .unwrap();
    assert_eq!(out, flux_catalog::ApplyOutcome::default());
    assert_eq!(
        cat.writer()
            .stats()
            .commands
            .load(std::sync::atomic::Ordering::Relaxed),
        before
    );

    // Shows have seasons with episodes; badges reflect the seeds' probes.
    let show = page.items.iter().find(|i| i.kind == ItemKind::Show).unwrap();
    assert!(show.child_count.unwrap() >= 1);
    let seasons = cat.read_blocking(|c| read::children(c, show.id, None)).unwrap();
    let eps = cat
        .read_blocking(|c| read::children(c, seasons[0].id, None))
        .unwrap();
    assert!(!eps.is_empty());
    let interstellar = page.items.iter().find(|i| i.title == "Interstellar").unwrap();
    assert_eq!(interstellar.badges.first().map(String::as_str), Some("4K"));
    // The explicitly-synced search index is consistent with the items.
    cat.writer()
        .exec(|c| Ok(c.execute_batch("INSERT INTO item_fts(item_fts) VALUES('integrity-check')")?))
        .wait()
        .unwrap();
    let indexed: i64 = cat
        .read_blocking(|c| Ok(c.query_row("SELECT count(*) FROM item_fts_docsize", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(
        indexed as u64,
        cat.writer().rebuild_search_index().wait().unwrap()
    );
    cat.close().unwrap();
}

#[test]
fn feature_clip_probes_become_labels_and_badges() {
    if !available(&features_manifest()) {
        return;
    }
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(features_manifest()).unwrap()).unwrap();
    let mut labels = HashMap::new();
    for clip in doc.as_array().unwrap() {
        let name = clip["file"].as_str().unwrap();
        let size = clip["bytes"].as_u64().unwrap();
        let p = synth::probe_from_ffprobe(&clip["ffprobe"], name, size);
        let f = flux_catalog::facts::asset_facts(Some(&p), None);
        labels.insert(name.to_string(), (f.label, f.badges, f.audio_summary));
    }
    let (label, badges, _) = &labels["hevc_main10_hdr10_hvc1.mp4"];
    assert_eq!(label, "4K HEVC HDR10 · AAC 1.0");
    assert_eq!(badges, &vec!["4K".to_string(), "HDR10".to_string()]);
    // HDR10 in this MKV lives only in SEI (research/02 §0.7), which ffprobe's
    // stream summary does not surface: flux-probe's SEI pass must supply it.
    let (label, _, _) = &labels["hevc_main10_hdr10_eac3.mkv"];
    assert_eq!(label, "4K HEVC · E-AC3 5.1");
    let (label, badges, _) = &labels["h264_truehd71.mkv"];
    assert_eq!(label, "1080p H.264 · TrueHD 5.1");
    assert!(badges.contains(&"5.1".to_string()));
    let (_, _, audio) = &labels["h264_multi_audio_subs.mkv"];
    assert_eq!(audio.as_deref(), Some("AAC 1.0, AC3 5.1"));
    assert_eq!(labels["h264_dts51.mkv"].0, "1080p H.264 · DTS 5.1");
    assert_eq!(labels["mpeg4_mp3.avi"].0, "SD MPEG-4 · MP3 1.0");
    assert_eq!(labels["av1_opus.webm"].0, "1080p AV1 · Opus 1.0");
    assert_eq!(labels["mpeg2_ac3.ts"].0, "1080p MPEG-2 · AC3 5.1");
}
