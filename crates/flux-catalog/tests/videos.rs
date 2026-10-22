//! Videos libraries (`LibraryKind::Videos`): one item per file, browsed by
//! folder, never matched; and files that are not media at all.

mod common;

use common::*;
use flux_catalog::read::{self, FolderQuery, FolderSort};
use flux_catalog::scan_support::{self, PROBE_RETRY_NEVER};
use flux_catalog::{FileState, FileUpsert, ProbeUpdate, ScanBatch};
use flux_core::api::{CreateLibrary, FolderPage, Id, Identity, Role};
use flux_core::{LibraryKind, NameGuess};

/// A videos-library guess: what flux-names gives (title, maybe a part).
fn video(title: &str, part: Option<u8>) -> NameGuess {
    let mut g = movie(title, None);
    g.part = part;
    g.confidence = 1.0;
    g
}

fn jobs_by_kind(e: &Env) -> Vec<(String, i64)> {
    e.cat
        .read_blocking(|c| {
            let mut stmt = c.prepare("SELECT kind, count(*) FROM job GROUP BY kind ORDER BY kind")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .unwrap()
}

#[test]
fn every_file_of_a_videos_library_is_an_item_of_its_own() {
    let e = env(LibraryKind::Videos);
    let out = e.apply(vec![
        upsert("Trip/Clip.mp4", video("Clip", None), Some(seed("h264_mkv"))),
        // Same title in another folder, a second copy: still its own video.
        upsert("Party/Clip.mp4", video("Clip", None), None),
        // Whatever a guess says (ids, a year, an extra, a sample), a videos
        // library's file is a video of its own.
        upsert("Party/Clip (2019).mp4", movie_tmdb("Clip", Some(2019), 603), None),
        upsert(
            "Party/Others/Bloopers.mp4",
            extra(Some("Party"), None, flux_core::ExtraKind::Other),
            None,
        ),
        upsert("Party/Sample/s.mp4", sample(), None),
        // Only a multi-part stack is one video.
        upsert("Wedding/Wedding part1.mp4", video("Wedding", Some(1)), None),
        upsert("Wedding/Wedding part2.mp4", video("Wedding", Some(2)), None),
    ]);
    assert_eq!((out.files_added, out.files_ignored, out.files_pending), (7, 0, 0));
    let top = e.top();
    assert_eq!(top.len(), 6, "{top:#?}");
    let mut titles: Vec<&str> = top.iter().map(|i| i.title.as_str()).collect();
    titles.sort();
    assert_eq!(titles, ["Clip", "Clip", "Clip", "Party", "Wedding", "s"]);
    assert!(top
        .iter()
        .all(|i| i.kind == flux_core::api::ItemKind::Movie && i.year.is_none()));
    let wedding = e.find_top("Wedding");
    let d = e.detail(wedding.id);
    let parts: Vec<_> = d
        .assets
        .iter()
        .map(|a| a.path.rsplit('/').next().unwrap().to_string())
        .collect();
    assert_eq!(parts, ["Wedding part1.mp4", "Wedding part2.mp4"]);
    assert!(d.external_ids.is_empty());
    // Never matched: frame art only, no match jobs.
    assert_eq!(jobs_by_kind(&e), [("art".to_string(), 6)]);
    // A rescan places nothing anew.
    let again = e.apply(vec![upsert("Trip/Clip.mp4", video("Clip", None), None)]);
    assert!(again.items_added.is_empty(), "{again:?}");
}

#[test]
fn a_renamed_video_keeps_its_watch_state() {
    let e = env(LibraryKind::Videos);
    e.apply(vec![upsert("Trip/IMG 1234.mp4", video("IMG 1234", None), None)]);
    let before = e.find_top("IMG 1234");
    let user = e
        .cat
        .writer()
        .ensure_user(Identity {
            login_name: "a@x".into(),
            display_name: "a".into(),
            profile_pic_url: None,
            node_name: None,
            role: Role::Viewer,
            tags: Vec::new(),
        })
        .wait()
        .unwrap();
    e.cat.writer().set_watched(user, before.id, true).wait().unwrap();
    let file_id = e.file_id("Trip/IMG 1234.mp4");
    let mut to = facts("Trip/Beach.mp4");
    to.fp128 = None;
    let out = e.batch(ScanBatch {
        root_id: e.root,
        moves: vec![flux_catalog::FileMove {
            file_id,
            to,
            guess: Some(video("Beach", None)),
        }],
        ..Default::default()
    });
    assert_eq!(out.files_moved, 1);
    let top = e.top();
    assert_eq!(top.len(), 1, "{top:#?}");
    let after = e.find_top("Beach");
    let watched = e
        .cat
        .read_blocking(move |c| read::item_summary(c, after.id, Some(user)))
        .unwrap()
        .unwrap()
        .progress
        .is_some_and(|p| p.watched);
    assert!(watched, "the watch state moved with the file");
}

fn page(e: &Env, folder: &str, sort: FolderSort, cursor: Option<String>, limit: u32) -> Option<FolderPage> {
    let q = FolderQuery {
        library_id: e.lib.id,
        folder: folder.to_string(),
        sort,
        cursor,
        limit,
        user_id: None,
    };
    e.cat.read_blocking(move |c| read::folder_page(c, &q)).unwrap()
}

fn titles(p: &FolderPage) -> Vec<&str> {
    p.items.iter().map(|i| i.title.as_str()).collect()
}

/// `path` with modification time `t` (seconds).
fn at(path: &str, title: &str, t: i64) -> FileUpsert {
    let mut u = upsert(path, video(title, None), None);
    u.facts.mtime_ns = t * 1_000_000_000;
    u
}

#[test]
fn folders_browse_a_videos_library() {
    let e = env(LibraryKind::Videos);
    e.apply(vec![
        at("a.mp4", "a", 50),
        at("Trip/b.mp4", "b", 30),
        at("Trip/c.mp4", "c", 10),
        at("Trip/Day 2/d.mp4", "d", 20),
        at("Trip/Day 10/e.mp4", "e", 40),
        at("Party/f.mp4", "f", 60),
    ]);
    let r = e.root;
    // The top of a one-folder library is that folder.
    let top = page(&e, "", FolderSort::Title, None, 100).unwrap();
    assert_eq!((top.at.folder.as_str(), top.at.name.as_str()), ("", "Lib"));
    assert!(top.parents.is_empty());
    let folders: Vec<(&str, u64)> = top
        .folders
        .iter()
        .map(|f| (f.name.as_str(), f.item_count))
        .collect();
    assert_eq!(folders, [("Party", 1), ("Trip", 4)]);
    assert_eq!(top.folders[1].folder, format!("{r}/Trip"));
    assert_eq!((titles(&top), top.total), (vec!["a"], 1));

    let trip = page(&e, &format!("{r}/Trip"), FolderSort::Title, None, 100).unwrap();
    assert_eq!(trip.at.name, "Trip");
    assert_eq!(
        trip.parents.iter().map(|p| p.folder.as_str()).collect::<Vec<_>>(),
        [""]
    );
    // Numbers in natural order.
    let names: Vec<&str> = trip.folders.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["Day 2", "Day 10"]);
    assert_eq!(titles(&trip), ["b", "c"]);
    let by_date = page(&e, &format!("{r}/Trip"), FolderSort::Date, None, 100).unwrap();
    assert_eq!(titles(&by_date), ["c", "b"]);
    let newest = page(&e, &format!("{r}/Trip"), FolderSort::DateDesc, None, 100).unwrap();
    assert_eq!(titles(&newest), ["b", "c"]);
    let desc = page(&e, &format!("{r}/Trip"), FolderSort::TitleDesc, None, 100).unwrap();
    assert_eq!(titles(&desc), ["c", "b"]);

    // Paging: one at a time, the subfolders on every page.
    for sort in [FolderSort::Title, FolderSort::DateDesc] {
        let first = page(&e, &format!("{r}/Trip"), sort, None, 1).unwrap();
        assert_eq!((first.items.len(), first.total, first.folders.len()), (1, 2, 2));
        let second = page(&e, &format!("{r}/Trip"), sort, first.next.clone(), 1).unwrap();
        assert_eq!(second.items.len(), 1);
        assert_ne!(first.items[0].id, second.items[0].id);
        assert_eq!(second.next, None);
        // A cursor of another order is refused.
        let other = if sort == FolderSort::Title {
            FolderSort::Date
        } else {
            FolderSort::Title
        };
        let q = FolderQuery {
            library_id: e.lib.id,
            folder: format!("{r}/Trip"),
            sort: other,
            cursor: first.next.clone(),
            ..Default::default()
        };
        assert!(e.cat.read_blocking(move |c| read::folder_page(c, &q)).is_err());
    }

    let day2 = page(&e, &format!("{r}/Trip/Day 2"), FolderSort::Title, None, 100).unwrap();
    assert_eq!(
        day2.parents.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        ["Lib", "Trip"]
    );
    assert_eq!((day2.at.name.as_str(), titles(&day2)), ("Day 2", vec!["d"]));
    assert!(day2.folders.is_empty());

    // Folders that hold nothing, or aren't this library's.
    assert!(page(&e, &format!("{r}/Nope"), FolderSort::Title, None, 100).is_none());
    assert!(page(&e, &format!("{}/Trip", r + 99), FolderSort::Title, None, 100).is_none());
    let q = FolderQuery {
        library_id: e.lib.id,
        folder: "Trip".into(),
        ..Default::default()
    };
    assert!(e.cat.read_blocking(move |c| read::folder_page(c, &q)).is_err());
    let q = FolderQuery {
        library_id: e.lib.id + 99,
        ..Default::default()
    };
    assert!(e
        .cat
        .read_blocking(move |c| read::folder_page(c, &q))
        .unwrap()
        .is_none());

    // A file that goes missing leaves its folder.
    let gone = e.file_id("Party/f.mp4");
    e.batch(ScanBatch {
        root_id: e.root,
        missing: vec![gone],
        ..Default::default()
    });
    let top = page(&e, "", FolderSort::Title, None, 100).unwrap();
    assert_eq!(
        top.folders.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
        ["Trip"]
    );
}

#[test]
fn a_library_with_several_folders_lists_them_at_the_top() {
    let dir = tempfile::tempdir().unwrap();
    let cat =
        flux_catalog::Catalog::open(dir.path().join("c.db"), flux_catalog::CatalogConfig::default()).unwrap();
    let lib = cat
        .writer()
        .create_library(CreateLibrary {
            name: "Home".into(),
            kind: LibraryKind::Videos,
            roots: vec![
                "/media/camera/".into(),
                "/media/phone".into(),
                "/media/empty".into(),
            ],
        })
        .wait()
        .unwrap();
    let roots: Vec<Id> = cat
        .read_blocking(|c| Ok(scan_support::list_roots(c)?.into_iter().map(|r| r.id).collect()))
        .unwrap();
    for (root, path) in [(roots[0], "2019/a.mp4"), (roots[0], "b.mp4"), (roots[1], "c.mp4")] {
        cat.writer()
            .apply(ScanBatch {
                root_id: root,
                upserts: vec![upsert(path, video(path, None), None)],
                ..Default::default()
            })
            .wait()
            .unwrap();
    }
    let q = |folder: String| FolderQuery {
        library_id: lib.id,
        folder,
        ..Default::default()
    };
    let top = cat
        .read_blocking(|c| read::folder_page(c, &q(String::new())))
        .unwrap()
        .unwrap();
    let folders: Vec<(&str, &str, u64)> = top
        .folders
        .iter()
        .map(|f| (f.folder.as_str(), f.name.as_str(), f.item_count))
        .collect();
    let (r0, r1) = (roots[0].to_string(), roots[1].to_string());
    assert_eq!(folders, [(r0.as_str(), "camera", 2), (r1.as_str(), "phone", 1)]);
    assert!(top.items.is_empty());
    let camera = cat
        .read_blocking(|c| read::folder_page(c, &q(r0.clone())))
        .unwrap()
        .unwrap();
    assert_eq!((camera.at.name.as_str(), camera.parents.len()), ("camera", 1));
    assert_eq!(camera.items.len(), 1);
    assert_eq!(camera.folders[0].folder, format!("{r0}/2019"));
    let year = cat
        .read_blocking(|c| read::folder_page(c, &q(format!("{r0}/2019"))))
        .unwrap()
        .unwrap();
    assert_eq!(
        year.parents.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        ["Home", "camera"]
    );
    drop(cat);
}

#[test]
fn files_that_are_not_media_never_become_items() {
    let e = env(LibraryKind::Mixed);
    let text = |path: &str| FileUpsert {
        facts: facts(path),
        probe: ProbeUpdate::NotMedia("not media: a text file, not a video".into()),
        guess: Some(movie("index d", None)),
    };
    let out = e.apply(vec![text("site/lib/index.d.ts")]);
    assert_eq!((out.files_added, out.files_ignored), (1, 1));
    assert!(out.items_added.is_empty());
    assert!(e.top().is_empty());
    let root = e.root;
    let rows = e
        .cat
        .read_blocking(move |c| scan_support::load_root_snapshot(c, root))
        .unwrap();
    let r = &rows["site/lib/index.d.ts"];
    assert_eq!(
        (r.state, r.probe_retry_at),
        (FileState::ProbeFailed, Some(PROBE_RETRY_NEVER))
    );
    // A re-parse (no probe: a stack's new part, a parser upgrade) leaves it
    // no item too.
    let out = e.apply(vec![upsert("site/lib/index.d.ts", movie("index d", None), None)]);
    assert!(out.items_added.is_empty(), "{out:?}");
    assert!(e.top().is_empty());
    // Renamed (the same file at a new path), still no item.
    let id = e.file_id("site/lib/index.d.ts");
    let mut to = facts("site/lib/types.d.ts");
    to.fp128 = None;
    let out = e.batch(ScanBatch {
        root_id: e.root,
        moves: vec![flux_catalog::FileMove {
            file_id: id,
            to,
            guess: Some(movie("types d", None)),
        }],
        ..Default::default()
    });
    assert_eq!((out.files_moved, out.items_added.len()), (1, 0), "{out:?}");
    assert!(e.top().is_empty());

    // One an older flux cataloged (a failed probe, placed as an item) loses
    // its item when it turns out not to be media.
    let old = FileUpsert {
        probe: ProbeUpdate::Failed("ffprobe: Invalid data found when processing input".into()),
        ..upsert("site/app.ts", movie("app", None), None)
    };
    let stat = old.facts.clone();
    e.apply(vec![old]);
    let item = e.find_top("app");
    let out = e.apply(vec![FileUpsert {
        facts: stat,
        probe: ProbeUpdate::NotMedia("not media: a text file, not a video".into()),
        guess: Some(movie("app", None)),
    }]);
    assert_eq!(out.items_removed, vec![item.id]);
    assert!(e.top().is_empty());
    // Neither counts as a file of the library.
    let lib = e.lib.id;
    let l = e
        .cat
        .read_blocking(move |c| read::library(c, lib))
        .unwrap()
        .unwrap();
    assert_eq!(l.scan.files_seen, 0);
}

/// The scanner re-parses every file of a folder that shares a new file's
/// stack or series key, and a camera's files all share one: a re-parse
/// whose guess is the stored one writes nothing, so adding a file to a big
/// folder costs no write per file already in it. A new guess, or a new
/// stat, is still written.
#[test]
fn a_reparse_that_changes_nothing_writes_nothing() {
    let e = env(LibraryKind::Mixed);
    let clip = upsert(
        "Camera/VID_20190501_120000.mp4",
        movie("VID 20190501 120000", None),
        None,
    );
    let out = e.apply(vec![clip.clone()]);
    assert_eq!(out.files_added, 1);
    let id = e.file_id("Camera/VID_20190501_120000.mp4");
    let stamp = || -> i64 {
        e.cat
            .read_blocking(move |c| {
                Ok(c.query_row("SELECT updated_at FROM file WHERE id = ?1", [id], |r| r.get(0))?)
            })
            .unwrap()
    };
    let before = stamp();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let out = e.apply(vec![clip.clone()]);
    assert_eq!(
        (out.files_changed, out.items_added.len(), out.items_updated.len()),
        (0, 0, 0),
        "{out:?}"
    );
    assert_eq!(stamp(), before);
    assert_eq!(e.top().len(), 1);

    // Another guess is written, and the file placed by it.
    let mut renamed = clip.clone();
    renamed.guess = Some(movie("Beach", None));
    let out = e.apply(vec![renamed]);
    assert_eq!(out.files_changed, 1, "{out:?}");
    assert!(stamp() > before);
    e.find_top("Beach");
    // So is a new stat with the guess it has.
    let mut touched = clip.clone();
    touched.guess = Some(movie("Beach", None));
    touched.facts.mtime_ns += 1_000_000_000;
    let out = e.apply(vec![touched]);
    assert_eq!(out.files_changed, 1, "{out:?}");
}
