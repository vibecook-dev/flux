//! Scanning a videos library (one item per file), and files with a video
//! extension that are not media at all.

mod support;

use std::path::Path;
use std::time::Duration;

use flux_catalog::read::{self, FolderQuery};
use flux_catalog::scan_support::PROBE_RETRY_NEVER;
use flux_catalog::{FileState, ItemsQuery};
use flux_core::api::Id;
use flux_core::LibraryKind;
use flux_scan::{Priority, ScanMode};
use support::*;

/// Binary bytes no prober reads: an unreadable video file, which is still
/// listed (by its name).
const UNREADABLE: &[u8] = &[0x1a; 4096];

fn put(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

fn top_titles(cat: &flux_catalog::Catalog, lib: Id) -> Vec<String> {
    let mut t: Vec<String> = cat
        .read_blocking(move |c| {
            read::items_page(
                c,
                &ItemsQuery {
                    library_id: Some(lib),
                    limit: 500,
                    ..Default::default()
                },
            )
        })
        .unwrap()
        .items
        .into_iter()
        .map(|i| i.title)
        .collect();
    t.sort();
    t
}

/// A JavaScript project inside a library: its TypeScript sources share
/// MPEG-TS's extensions. `node_modules` is never walked, and the sources
/// elsewhere are cataloged as not media: no items, no ffprobe runs, and no
/// retries on later scans.
#[test]
fn code_files_with_video_extensions_are_not_media() {
    let scratch = Scratch::new("not-media");
    let root = scratch.path("Clips");
    put(&root, "site/src/app.ts", b"export const a: number = 1;\n");
    put(&root, "site/src/types.d.mts", b"declare module 'x';\n");
    put(&root, "site/node_modules/pkg/index.d.ts", b"export {};\n");
    put(&root, "site/node_modules/@scope/pkg/index.ts", b"export {};\n");
    put(&root, "home/clip.mp4", UNREADABLE);
    let cat = open_catalog(&scratch.dir);
    let (lib, root_id) = add_library(&cat, "Clips", LibraryKind::Mixed, &root);
    let s = scanner(&cat);
    let r = scan(&s, lib);
    assert_eq!(
        (r.media_files, r.not_media, r.ffprobe_fallbacks),
        (3, 2, 0),
        "{r:?}"
    );
    let snap = snapshot(&cat, root_id);
    let mut rows: Vec<&str> = snap.keys().map(String::as_str).collect();
    rows.sort();
    assert_eq!(rows, ["home/clip.mp4", "site/src/app.ts", "site/src/types.d.mts"]);
    for code in ["site/src/app.ts", "site/src/types.d.mts"] {
        let row = &snap[code];
        assert_eq!(
            (row.state, row.probe_retry_at),
            (FileState::ProbeFailed, Some(PROBE_RETRY_NEVER))
        );
        assert_eq!(row.asset_id, None);
    }
    // The unreadable video is still listed by its name.
    assert_eq!(top_titles(&cat, lib), ["Clip"]);
    // A catch-up scan opens nothing again.
    let r = s
        .scan_library(lib, ScanMode::Quick, Priority::Scheduled)
        .wait()
        .unwrap();
    assert_eq!(
        (r.probe_retries, r.media_files_opened, r.not_media),
        (0, 0, 0),
        "{r:?}"
    );
    // An edit makes it a file to look at again (still text).
    put(
        &root,
        "site/src/app.ts",
        b"export const a: number = 2; // changed\n",
    );
    let r = s
        .scan_library(lib, ScanMode::Quick, Priority::Scheduled)
        .wait()
        .unwrap();
    assert_eq!((r.changed, r.not_media), (1, 1), "{r:?}");
    assert_eq!(top_titles(&cat, lib), ["Clip"]);
    s.shutdown(Duration::from_secs(5));
}

/// A videos library: every file is its own item, titled after the file,
/// whatever its name looks like; only a multi-part stack is one item. The
/// folders come back through the folder view.
#[test]
fn a_videos_library_is_one_item_per_file_by_folder() {
    let scratch = Scratch::new("videos");
    let root = scratch.path("Home");
    for rel in [
        "2019/Trip/IMG_0001.mp4",
        "2019/Trip/IMG_0002.mp4",
        "2019/Others/IMG_0001.mp4",
        "2019/Trip/VID_20190501_123456.mp4",
        "Wedding/Wedding part1.mp4",
        "Wedding/Wedding part2.mp4",
        "Show/Show S01E01.mkv",
        "Show/Show S01E02.mkv",
        "Film (2010)/Film (2010).mkv",
        "Film (2010)/Featurettes/Making Of.mkv",
        "Film (2010)/Sample/sample.mkv",
        "001.mp4",
        "002.mp4",
    ] {
        put(&root, rel, UNREADABLE);
    }
    let cat = open_catalog(&scratch.dir);
    let (lib, root_id) = add_library(&cat, "Home", LibraryKind::Videos, &root);
    let s = scanner(&cat);
    let r = scan(&s, lib);
    assert_eq!(r.media_files, 13, "{r:?}");
    assert_eq!(
        top_titles(&cat, lib),
        [
            "001",
            "002",
            "Film (2010)",
            "IMG 0001",
            "IMG 0001",
            "IMG 0002",
            "Making Of",
            "Show S01E01",
            "Show S01E02",
            "VID 20190501 123456",
            "Wedding",
            "sample",
        ]
    );
    // No match jobs: the library is never matched.
    let matches: i64 = cat
        .read_blocking(
            |c| Ok(c.query_row("SELECT count(*) FROM job WHERE kind = 'match'", [], |r| r.get(0))?),
        )
        .unwrap();
    assert_eq!(matches, 0);
    let q = |folder: String| FolderQuery {
        library_id: lib,
        folder,
        ..Default::default()
    };
    let top = cat
        .read_blocking(|c| read::folder_page(c, &q(String::new())))
        .unwrap()
        .unwrap();
    let folders: Vec<(&str, u64)> = top
        .folders
        .iter()
        .map(|f| (f.name.as_str(), f.item_count))
        .collect();
    assert_eq!(
        folders,
        [("2019", 4), ("Film (2010)", 3), ("Show", 2), ("Wedding", 1)]
    );
    assert_eq!(top.total, 2);
    let trip = cat
        .read_blocking(|c| read::folder_page(c, &q(format!("{root_id}/2019/Trip"))))
        .unwrap()
        .unwrap();
    assert_eq!(trip.total, 3);
    assert_eq!(
        trip.parents.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        ["Home", "2019"]
    );
    s.shutdown(Duration::from_secs(5));
}

/// In a mixed library a weakly numbered episode (`Clip E01`) needs another
/// file of its numbered series beside it. Copied in alone it is a video of
/// its own; when the next one arrives, the first is parsed again and both
/// are episodes of one show.
#[test]
fn a_series_arriving_later_regroups_the_first_file() {
    let scratch = Scratch::new("series-later");
    let root = scratch.path("Mixed");
    put(&root, "Trip/Clip E01.mp4", UNREADABLE);
    let cat = open_catalog(&scratch.dir);
    let (lib, root_id) = add_library(&cat, "Mixed", LibraryKind::Mixed, &root);
    let s = scanner(&cat);
    let quick = || {
        s.scan_library(lib, ScanMode::Quick, Priority::Scheduled)
            .wait()
            .expect("scan")
    };
    let kinds = || -> Vec<(String, String)> {
        cat.read_blocking(move |c| {
            let mut stmt = c.prepare(
                "SELECT i.kind, i.title FROM item i WHERE i.library_id = ?1 AND i.parent_id IS NULL
                   AND i.state <> 'missing' ORDER BY i.title",
            )?;
            let rows = stmt.query_map([lib], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .unwrap()
    };
    quick();
    assert_eq!(kinds(), [("movie".to_string(), "Clip E01".to_string())]);

    put(&root, "Trip/Clip E02.mp4", UNREADABLE);
    let r = quick();
    assert_eq!(r.added, 1, "{r:?}");
    assert_eq!(kinds(), [("show".to_string(), "Clip".to_string())]);
    let episodes: i64 = cat
        .read_blocking(move |c| {
            Ok(c.query_row(
                "SELECT count(*) FROM item WHERE library_id = ?1 AND kind = 'episode' AND state <> 'missing'",
                [lib],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(episodes, 2);
    assert_eq!(snapshot(&cat, root_id).len(), 2);
    s.shutdown(Duration::from_secs(5));
}

/// A phone's folder in a mixed library: every `VID_…` file shares the
/// series key of a new one, so when it arrives the scanner parses them all
/// again. None of them reads any differently, so none is written: adding a
/// file to a folder costs no write per file already in it. A numbered
/// series beside them still takes its new episode.
#[test]
fn a_new_clip_rewrites_none_of_its_folder() {
    let scratch = Scratch::new("phone-folder");
    let root = scratch.path("Mixed");
    // Unreadable, and each file its own content.
    let bytes = |i: u8| [UNREADABLE, &[i]].concat();
    for i in 0..40 {
        put(&root, &format!("Phone/VID_20190501_1200{i:02}.mp4"), &bytes(i));
    }
    put(&root, "Phone/Clip E01.mp4", &bytes(41));
    put(&root, "Phone/Clip E02.mp4", &bytes(42));
    let cat = open_catalog(&scratch.dir);
    let (lib, _) = add_library(&cat, "Mixed", LibraryKind::Mixed, &root);
    let s = scanner(&cat);
    let quick = || {
        s.scan_library(lib, ScanMode::Quick, Priority::Scheduled)
            .wait()
            .expect("scan")
    };
    let r = quick();
    assert_eq!(r.added, 42, "{r:?}");
    assert!(s.wait_fingerprints(Duration::from_secs(60)));
    let last_write = || -> i64 {
        cat.read_blocking(|c| Ok(c.query_row("SELECT max(updated_at) FROM file", [], |r| r.get(0))?))
            .unwrap()
    };
    let before = last_write();
    std::thread::sleep(Duration::from_millis(5));

    put(&root, "Phone/VID_20190601_120000.mp4", &bytes(43));
    put(&root, "Phone/Clip E03.mp4", &bytes(44));
    let r = quick();
    assert_eq!((r.added, r.changed), (2, 0), "{r:?}");
    let rewritten: i64 = cat
        .read_blocking(move |c| {
            Ok(
                c.query_row("SELECT count(*) FROM file WHERE updated_at > ?1", [before], |r| {
                    r.get(0)
                })?,
            )
        })
        .unwrap();
    assert_eq!(rewritten, 2, "only the new files are written");
    let episodes: i64 = cat
        .read_blocking(move |c| {
            Ok(c.query_row(
                "SELECT count(*) FROM item WHERE library_id = ?1 AND kind = 'episode' AND state <> 'missing'",
                [lib],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(episodes, 3);
    s.shutdown(Duration::from_secs(5));
}
