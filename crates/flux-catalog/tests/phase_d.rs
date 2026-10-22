//! Phase D regressions (review findings INT-1, INT-4, INT-6, INT-7, INT-8)
//! and the catalog side of the scan/footprint work: deferred fingerprints,
//! the re-attach tie-break, subtree snapshots, cached library counts,
//! newest-first art jobs, bulk mode.

mod common;

use std::time::Duration;

use common::*;
use flux_catalog::{
    read, scan_support, ArtKind, CatalogError, ClaimJobs, ExpectedRow, FileFingerprint, FileMove, FileState,
    FileUpsert, JobResult, NewArt, ProbeUpdate, RootOverlap, ScanBatch, JOB_ART, JOB_MATCH,
};
use flux_core::api::{ArtSource, CreateLibrary, Id, Identity, Role};
use flux_core::LibraryKind;

fn user(e: &Env) -> Id {
    e.cat
        .writer()
        .ensure_user(Identity {
            login_name: "a@example.com".into(),
            display_name: "A".into(),
            profile_pic_url: None,
            node_name: None,
            role: Role::Viewer,
            tags: Vec::new(),
        })
        .wait()
        .unwrap()
}

fn item_of_asset(e: &Env, asset: Id) -> Id {
    e.cat
        .read_blocking(move |c| {
            Ok(c.query_row("SELECT item_id FROM asset WHERE id = ?1", [asset], |r| r.get(0))?)
        })
        .unwrap()
}

fn item_exists(e: &Env, id: Id) -> bool {
    e.cat
        .read_blocking(move |c| {
            Ok(c.query_row("SELECT count(*) FROM item WHERE id = ?1", [id], |r| {
                r.get::<_, i64>(0)
            })?)
        })
        .unwrap()
        > 0
}

fn pinned(e: &Env, item: Id) -> Vec<Id> {
    e.cat
        .read_blocking(move |c| {
            let mut st = c.prepare("SELECT id FROM art WHERE item_id = ?1 AND pinned = 1 ORDER BY id")?;
            let rows = st.query_map([item], |r| r.get(0))?;
            Ok(rows.collect::<Result<Vec<Id>, _>>()?)
        })
        .unwrap()
}

fn art(path: &str) -> NewArt {
    NewArt {
        kind: ArtKind::Poster,
        source: ArtSource::Local,
        path: Some(path.into()),
        url: None,
        width: None,
        height: None,
        blurhash: None,
        attribution: None,
        generator: None,
        gen_version: None,
        gen_title: None,
    }
}

fn progress(e: &Env, u: Id, item: Id) -> Option<flux_core::api::Progress> {
    e.cat.read_blocking(move |c| read::progress(c, u, item)).unwrap()
}

fn row(e: &Env, path: &str) -> Option<flux_catalog::FileRow> {
    let root = e.root;
    let p = path.to_string();
    e.cat
        .read_blocking(move |c| Ok(scan_support::load_root_snapshot(c, root)?.remove(&p)))
        .unwrap()
}

// ---- INT-1: a rename that changes the parsed title keeps the work's state --

/// A typo fixed in a movie's name (same file: a FileMove whose guess has
/// another title). The resume point, the watched flag and the user's pinned
/// poster follow the file to its new work; the emptied old work goes.
#[test]
fn int1_a_title_changing_move_carries_progress_and_the_pin() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![upsert(
        "Teh Matrix (1999).mkv",
        movie("Teh Matrix", Some(1999)),
        Some(seed("h264_mkv")),
    )]);
    let old = e.find_top("Teh Matrix").id;
    let asset = e.detail(old).assets[0].id;
    let u = user(&e);
    let w = e.cat.writer();
    w.set_progress(u, old, Some(asset), 600_000, 6_000_000)
        .wait()
        .unwrap();
    let chosen = w.add_art(old, art("/art/users-choice.jpg")).wait().unwrap();
    assert!(w.pin_art(chosen).wait().unwrap());
    let file_id = e.file_id("Teh Matrix (1999).mkv");
    let mut to = facts("The Matrix (1999).mkv");
    to.fp128 = None;
    let out = e.batch(ScanBatch {
        root_id: e.root,
        moves: vec![FileMove {
            file_id,
            to,
            guess: Some(movie("The Matrix", Some(1999))),
        }],
        ..Default::default()
    });
    assert_eq!(out.files_moved, 1);
    let new = item_of_asset(&e, asset);
    assert_ne!(new, old, "a new work for the new title");
    let p = progress(&e, u, new).expect("resume point carried to the renamed movie");
    assert_eq!(p.position_ms, 600_000);
    assert_eq!(pinned(&e, new), vec![chosen], "the user's pin follows");
    assert!(!item_exists(&e, old), "the emptied old work is merged away");
    assert!(out.items_removed.contains(&old), "{out:?}");
    let cw: Vec<String> = e
        .cat
        .read_blocking(move |c| read::continue_watching(c, u, 20))
        .unwrap()
        .into_iter()
        .map(|s| s.title)
        .collect();
    assert_eq!(cw, vec!["The Matrix".to_string()]);
}

/// A show folder renamed to another title re-places every episode under a
/// new show: every per-episode watched flag follows, and the old show (now
/// without files) merges into the new one.
#[test]
fn int1_a_show_retitle_carries_every_watched_episode() {
    let e = env(LibraryKind::Shows);
    let names = ["S01E01", "S01E02", "S01E03"];
    e.apply(
        names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                upsert(
                    &format!("Teh Office/Season 1/Teh Office - {n}.mkv"),
                    episode("Teh Office", None, 1, &[i as u16 + 1]),
                    None,
                )
            })
            .collect(),
    );
    let show = e.find_top("Teh Office").id;
    let season = e.children(show)[0].id;
    let eps: Vec<Id> = e.children(season).into_iter().map(|s| s.id).collect();
    let u = user(&e);
    for &ep in &eps[..2] {
        e.cat.writer().set_watched(u, ep, true).wait().unwrap();
    }
    let chosen = e.cat.writer().add_art(show, art("/art/show.jpg")).wait().unwrap();
    assert!(e.cat.writer().pin_art(chosen).wait().unwrap());
    let moves = names
        .iter()
        .enumerate()
        .map(|(i, n)| FileMove {
            file_id: e.file_id(&format!("Teh Office/Season 1/Teh Office - {n}.mkv")),
            to: facts(&format!("The Office/Season 1/The Office - {n}.mkv")),
            guess: Some(episode("The Office", None, 1, &[i as u16 + 1])),
        })
        .collect();
    e.batch(ScanBatch {
        root_id: e.root,
        moves,
        ..Default::default()
    });
    let new_show = e.find_top("The Office").id;
    let new_season = e.children(new_show)[0].id;
    let watched = e
        .children(new_season)
        .iter()
        .filter(|s| progress(&e, u, s.id).is_some_and(|p| p.watched))
        .count();
    assert_eq!(watched, 2, "2 watched episodes stay watched");
    assert!(!item_exists(&e, show), "the old show merged into the new one");
    assert_eq!(pinned(&e, new_show), vec![chosen], "the show's pin follows");
    assert_eq!(e.top().len(), 1);
}

/// The file moves onto a work that already exists (and the user has state
/// on both): the states merge — watched wins, the newer position wins.
#[test]
fn int1_a_move_onto_an_existing_work_merges_both_states() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![
        upsert(
            "Heat (1995).mkv",
            movie("Heat", Some(1995)),
            Some(seed("h264_mkv")),
        ),
        upsert(
            "Heet (1995) 4K.mkv",
            movie("Heet", Some(1995)),
            Some(seed("hevc_mkv")),
        ),
    ]);
    let heat = e.find_top("Heat").id;
    let typo = e.find_top("Heet").id;
    let u = user(&e);
    e.cat.writer().set_watched(u, heat, true).wait().unwrap();
    std::thread::sleep(Duration::from_millis(5));
    let asset = e.detail(typo).assets[0].id;
    e.cat
        .writer()
        .set_progress(u, typo, Some(asset), 500_000, 6_000_000)
        .wait()
        .unwrap();
    let file_id = e.file_id("Heet (1995) 4K.mkv");
    e.batch(ScanBatch {
        root_id: e.root,
        moves: vec![FileMove {
            file_id,
            to: facts("Heat (1995) 4K.mkv"),
            guess: Some(movie("Heat", Some(1995))),
        }],
        ..Default::default()
    });
    assert_eq!(item_of_asset(&e, asset), heat);
    let p = progress(&e, u, heat).unwrap();
    assert!(p.watched, "watched wins");
    assert_eq!(p.position_ms, 500_000, "the newer position wins");
    assert!(!item_exists(&e, typo));
    assert_eq!(e.detail(heat).assets.len(), 2);
}

/// A file moved to another library is re-attached there (by fingerprint);
/// its asset lands on a work of that library, and the watch state is
/// copied there (the old library's work stays, hidden).
#[test]
fn int1_a_move_to_another_library_copies_the_watch_state() {
    let e = env(LibraryKind::Movies);
    let kids = e
        .cat
        .writer()
        .create_library(CreateLibrary {
            name: "Kids".into(),
            kind: LibraryKind::Movies,
            roots: vec!["/media/kids/".into()],
        })
        .wait()
        .unwrap();
    let kids_root: Id = e
        .cat
        .read_blocking(move |c| {
            Ok(scan_support::list_roots(c)?
                .into_iter()
                .find(|r| r.library_id == kids.id)
                .unwrap()
                .id)
        })
        .unwrap();
    let up = upsert(
        "Up (2009)/Up (2009).mkv",
        movie("Up", Some(2009)),
        Some(seed("h264_mkv")),
    );
    let f = up.facts.clone();
    e.apply(vec![up]);
    let old = e.find_top("Up").id;
    let asset = e.detail(old).assets[0].id;
    let u = user(&e);
    e.cat.writer().set_watched(u, old, true).wait().unwrap();
    let fid = e.file_id("Up (2009)/Up (2009).mkv");
    e.batch(ScanBatch {
        root_id: e.root,
        missing: vec![fid],
        ..Default::default()
    });
    let out = e.batch(ScanBatch {
        root_id: kids_root,
        upserts: vec![FileUpsert {
            facts: f,
            probe: ProbeUpdate::Keep,
            guess: Some(movie("Up", Some(2009))),
        }],
        ..Default::default()
    });
    assert_eq!(out.files_reattached, 1);
    let new = item_of_asset(&e, asset);
    assert_ne!(new, old);
    assert!(progress(&e, u, new).is_some_and(|p| p.watched));
}

/// A same-volume move from another root (same inode) arrives as a guarded
/// FileMove from that root: file id, asset and state kept.
#[test]
fn a_guarded_move_from_another_root_keeps_the_file() {
    let e = env(LibraryKind::Movies);
    let r2 = e
        .cat
        .writer()
        .add_root(e.lib.id, "/media/lib2".into(), None)
        .wait()
        .unwrap();
    let up = upsert(
        "Heat (1995).mkv",
        movie("Heat", Some(1995)),
        Some(seed("h264_mkv")),
    );
    let mut f = up.facts.clone();
    e.apply(vec![up]);
    let row = row(&e, "Heat (1995).mkv").unwrap();
    // Unguarded: refused.
    let out = e.batch(ScanBatch {
        root_id: r2,
        moves: vec![FileMove {
            file_id: row.id,
            to: f.clone(),
            guess: Some(movie("Heat", Some(1995))),
        }],
        ..Default::default()
    });
    assert_eq!((out.files_moved, out.files_reattached), (0, 0));
    f.rel_path = "Movies/Heat (1995).mkv".into();
    let out = e.batch(ScanBatch {
        root_id: r2,
        moves: vec![FileMove {
            file_id: row.id,
            to: f,
            guess: Some(movie("Heat", Some(1995))),
        }],
        expect: vec![ExpectedRow {
            file_id: row.id,
            rel_path: "Heat (1995).mkv".into(),
            size: row.size,
            mtime_ns: row.mtime_ns,
        }],
        ..Default::default()
    });
    assert_eq!(out.files_reattached, 1);
    let moved = e
        .cat
        .read_blocking(move |c| scan_support::load_root_snapshot(c, r2))
        .unwrap();
    assert_eq!(moved["Movies/Heat (1995).mkv"].id, row.id);
    assert_eq!(moved["Movies/Heat (1995).mkv"].asset_id, row.asset_id);
}

// ---- INT-4: tombstones and moves decided from a stale snapshot ------------

/// A scan decided from an old snapshot that file `id` vanished from
/// `Old/X/`; meanwhile a watcher batch moved the row to `New/`. With the
/// snapshot row as guard, the stale tombstone is not applied.
#[test]
fn int4_a_stale_missing_does_not_tombstone_a_moved_row() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![upsert(
        "Old/X/Heat (1995).mkv",
        movie("Heat", Some(1995)),
        None,
    )]);
    let before = row(&e, "Old/X/Heat (1995).mkv").unwrap();
    e.batch(ScanBatch {
        root_id: e.root,
        moves: vec![FileMove {
            file_id: before.id,
            to: facts("New/Heat (1995).mkv"),
            guess: Some(movie("Heat", Some(1995))),
        }],
        ..Default::default()
    });
    let stale = ExpectedRow {
        file_id: before.id,
        rel_path: "Old/X/Heat (1995).mkv".into(),
        size: before.size,
        mtime_ns: before.mtime_ns,
    };
    let out = e.batch(ScanBatch {
        root_id: e.root,
        missing: vec![before.id],
        expect: vec![stale.clone()],
        ..Default::default()
    });
    assert_eq!(out.files_missing, 0, "the moved, live row is not tombstoned");
    assert_eq!(row(&e, "New/Heat (1995).mkv").unwrap().state, FileState::Present);
    // A stale move is skipped the same way.
    let out = e.batch(ScanBatch {
        root_id: e.root,
        moves: vec![FileMove {
            file_id: before.id,
            to: facts("Elsewhere/Heat (1995).mkv"),
            guess: None,
        }],
        expect: vec![stale],
        ..Default::default()
    });
    assert_eq!(out.files_moved, 0);
    assert!(row(&e, "New/Heat (1995).mkv").is_some());
    // A guard that still matches applies.
    let now = row(&e, "New/Heat (1995).mkv").unwrap();
    let out = e.batch(ScanBatch {
        root_id: e.root,
        missing: vec![now.id],
        expect: vec![ExpectedRow {
            file_id: now.id,
            rel_path: "New/Heat (1995).mkv".into(),
            size: now.size,
            mtime_ns: now.mtime_ns,
        }],
        ..Default::default()
    });
    assert_eq!(out.files_missing, 1);
}

// ---- INT-6: a failed re-probe keeps the facts and is retried --------------

#[test]
fn int6_a_failed_reprobe_keeps_facts_and_backs_off() {
    let e = env(LibraryKind::Movies);
    let up = upsert(
        "Heat (1995).mkv",
        movie("Heat", Some(1995)),
        Some(seed("hevc_mkv")),
    );
    let mut f = up.facts.clone();
    e.apply(vec![up]);
    let heat = e.find_top("Heat").id;
    let label = e.detail(heat).assets[0].label.clone();
    assert!(!label.is_empty());
    let fail = |e: &Env, f: &flux_catalog::FileFacts| {
        e.batch(ScanBatch {
            root_id: e.root,
            upserts: vec![FileUpsert {
                facts: f.clone(),
                probe: ProbeUpdate::Failed("open: Permission denied".into()),
                guess: Some(movie("Heat", Some(1995))),
            }],
            ..Default::default()
        })
    };
    // The file changed and can't be read right now.
    f.mtime_ns += 1;
    let t0 = flux_catalog::now_ms();
    fail(&e, &f);
    let r = row(&e, "Heat (1995).mkv").unwrap();
    assert_eq!(r.state, FileState::ProbeFailed);
    let first = r.probe_retry_at.expect("a retry is scheduled");
    assert!(
        first >= t0 + 59_000 && first <= flux_catalog::now_ms() + 61_000,
        "{first} vs {t0}"
    );
    assert_eq!(e.detail(heat).assets[0].label, label, "playback facts kept");
    // Fails again: the backoff doubles.
    fail(&e, &f);
    let second = row(&e, "Heat (1995).mkv").unwrap().probe_retry_at.unwrap();
    assert!(second >= first + 59_000, "{second} vs {first}");
    // Readable again: probed, the retry state is cleared.
    e.batch(ScanBatch {
        root_id: e.root,
        upserts: vec![FileUpsert {
            facts: f,
            probe: ProbeUpdate::Probed(Box::new(seed("h264_mkv"))),
            guess: Some(movie("Heat", Some(1995))),
        }],
        ..Default::default()
    });
    let r = row(&e, "Heat (1995).mkv").unwrap();
    assert_eq!((r.state, r.probe_retry_at), (FileState::Present, None));
    assert_ne!(e.detail(heat).assets[0].label, label, "the new probe's facts");
}

// ---- INT-7: overlapping roots --------------------------------------------

#[test]
fn int7_overlapping_roots_in_one_library_are_refused() {
    let e = env(LibraryKind::Movies);
    let w = e.cat.writer();
    let lib = |roots: &[&str]| {
        w.create_library(CreateLibrary {
            name: "X".into(),
            kind: LibraryKind::Movies,
            roots: roots.iter().map(|s| s.to_string()).collect(),
        })
        .wait()
    };
    assert!(matches!(
        lib(&["/m", "/m/Sub"]),
        Err(CatalogError::OverlappingRoots { .. })
    ));
    assert!(matches!(
        lib(&["/m/Sub/", "/m/Sub"]),
        Err(CatalogError::OverlappingRoots { .. })
    ));
    // Whole components: Movies2 is not inside Movies.
    let ok = lib(&["/m/Movies", "/m/Movies2"]).unwrap();
    assert!(matches!(
        w.add_root(ok.id, "/m/Movies/4K".into(), None).wait(),
        Err(CatalogError::OverlappingRoots { .. })
    ));
    assert!(matches!(
        w.add_root(ok.id, "/m".into(), None).wait(),
        Err(CatalogError::OverlappingRoots { .. })
    ));
    assert!(w.add_root(ok.id, "/m/Other".into(), None).wait().is_ok());
}

/// A folder another library already has, or one inside or around it, is
/// refused too: every file below would be listed in both libraries (found
/// on real media: the same folder added twice, once in another letter case).
#[test]
fn roots_of_other_libraries_are_refused() {
    let e = env(LibraryKind::Movies); // "Lib" at /media/lib/
    let w = e.cat.writer();
    let lib = |name: &str, roots: &[&str]| {
        w.create_library(CreateLibrary {
            name: name.into(),
            kind: LibraryKind::Shows,
            roots: roots.iter().map(|s| s.to_string()).collect(),
        })
        .wait()
    };
    for (root, how) in [
        ("/media/lib", RootOverlap::Same),
        ("/media/lib/", RootOverlap::Same),
        ("/media/lib/TV", RootOverlap::Inside),
        ("/media", RootOverlap::Contains),
    ] {
        match lib("TV", &[root]) {
            Err(CatalogError::RootInOtherLibrary {
                other,
                library,
                how: got,
                ..
            }) => {
                assert_eq!(
                    (other.as_str(), library.as_str(), got),
                    ("/media/lib", "Lib", how),
                    "{root}"
                );
            }
            other => panic!("{root}: {other:?}"),
        }
    }
    let msg = lib("TV", &["/media/lib/TV"]).unwrap_err().to_string();
    assert_eq!(
        msg,
        "The folder \"/media/lib/TV\" is inside \"/media/lib\", which is already in the library \"Lib\". \
         A folder can be in only one library, or its files would be listed twice."
    );
    // Neighbours are fine, and adding a root checks the other libraries too.
    let tv = lib("TV", &["/media/library-tv"]).unwrap();
    assert!(matches!(
        w.add_root(tv.id, "/media/lib/More".into(), None).wait(),
        Err(CatalogError::RootInOtherLibrary { .. })
    ));
    assert!(matches!(
        w.add_root(tv.id, "/media/library-tv/4K".into(), None).wait(),
        Err(CatalogError::OverlappingRoots { .. })
    ));
    assert!(w.add_root(tv.id, "/media/tv2".into(), None).wait().is_ok());
    // Letter case: the same folder on Windows and macOS, whose volumes
    // ignore it; two folders elsewhere.
    let upper = lib("Upper", &["/MEDIA/LIB"]);
    if cfg!(any(windows, target_os = "macos")) {
        assert!(
            matches!(
                upper,
                Err(CatalogError::RootInOtherLibrary {
                    how: RootOverlap::Same,
                    ..
                })
            ),
            "{upper:?}"
        );
    } else {
        assert!(upper.is_ok(), "{upper:?}");
    }
}

// ---- INT-8: merge keeps the source's pin when the target has none ----------

#[test]
fn int8_merge_keeps_the_users_pin_when_the_target_has_none() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![
        upsert("The Wire (2002)/a.mkv", movie("The Wire", Some(2002)), None),
        upsert("Wire (2002)/b.mkv", movie("Wire", Some(2002)), None),
    ]);
    let into = e.find_top("The Wire").id;
    let from = e.find_top("Wire").id;
    let w = e.cat.writer();
    w.add_art(into, art("/art/into.jpg")).wait().unwrap();
    let chosen = w.add_art(from, art("/art/users-choice.jpg")).wait().unwrap();
    assert!(w.pin_art(chosen).wait().unwrap());
    w.merge_items(from, into).wait().unwrap();
    assert_eq!(pinned(&e, into), vec![chosen]);
}

#[test]
fn merge_keeps_the_target_pin_when_both_have_one() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![
        upsert("The Wire (2002)/a.mkv", movie("The Wire", Some(2002)), None),
        upsert("Wire (2002)/b.mkv", movie("Wire", Some(2002)), None),
    ]);
    let into = e.find_top("The Wire").id;
    let from = e.find_top("Wire").id;
    let w = e.cat.writer();
    let a = w.add_art(into, art("/art/into.jpg")).wait().unwrap();
    let b = w.add_art(from, art("/art/from.jpg")).wait().unwrap();
    assert!(w.pin_art(a).wait().unwrap());
    assert!(w.pin_art(b).wait().unwrap());
    w.merge_items(from, into).wait().unwrap();
    assert_eq!(pinned(&e, into), vec![a]);
}

// ---- deferred fingerprints -------------------------------------------------

fn same_content(path: &str, like: &flux_catalog::FileFacts) -> flux_catalog::FileFacts {
    let mut f = facts(path);
    f.size = like.size;
    f.fp128 = None;
    f.oshash = None;
    f
}

/// A file cataloged without its fingerprint (flux-scan computes it after
/// the commit) whose content matches a tombstone: attaching the fingerprint
/// merges the fresh row into the tombstone, which keeps its id, asset and
/// watch state at the new path.
#[test]
fn attach_fingerprint_merges_a_fresh_row_into_its_tombstone() {
    let e = env(LibraryKind::Movies);
    let up = upsert(
        "Old/Heat (1995).mkv",
        movie("Heat", Some(1995)),
        Some(seed("h264_mkv")),
    );
    let old_facts = up.facts.clone();
    e.apply(vec![up]);
    let heat = e.find_top("Heat").id;
    let u = user(&e);
    e.cat.writer().set_watched(u, heat, true).wait().unwrap();
    let tomb = row(&e, "Old/Heat (1995).mkv").unwrap();
    e.batch(ScanBatch {
        root_id: e.root,
        missing: vec![tomb.id],
        ..Default::default()
    });
    // A copy elsewhere (new inode), committed before its fingerprint.
    let fresh = same_content("New/Heat (1995).mkv", &old_facts);
    e.apply(vec![FileUpsert {
        facts: fresh.clone(),
        probe: ProbeUpdate::Probed(Box::new(seed("h264_mkv"))),
        guess: Some(movie("Heat", Some(1995))),
    }]);
    let fresh_row = row(&e, "New/Heat (1995).mkv").unwrap();
    assert_ne!(fresh_row.id, tomb.id);
    assert_eq!(fresh_row.fp128, None);
    let w = e.cat.writer();
    // A stale stat (the file changed since it was fingerprinted): nothing.
    let out = w
        .attach_fingerprints(
            e.root,
            vec![FileFingerprint {
                rel_path: "New/Heat (1995).mkv".into(),
                size: fresh.size,
                mtime_ns: fresh.mtime_ns + 1,
                fp128: old_facts.fp128.unwrap(),
                oshash: None,
            }],
        )
        .wait()
        .unwrap();
    assert_eq!(out.files_reattached, 0);
    let out = w
        .attach_fingerprints(
            e.root,
            vec![FileFingerprint {
                rel_path: "New/Heat (1995).mkv".into(),
                size: fresh.size,
                mtime_ns: fresh.mtime_ns,
                fp128: old_facts.fp128.unwrap(),
                oshash: old_facts.oshash,
            }],
        )
        .wait()
        .unwrap();
    assert_eq!(out.files_reattached, 1, "{out:?}");
    let now = row(&e, "New/Heat (1995).mkv").unwrap();
    assert_eq!(now.id, tomb.id, "the tombstone took the new path");
    assert_eq!(now.asset_id, tomb.asset_id);
    assert_eq!((now.state, now.fp128), (FileState::Present, old_facts.fp128));
    assert!(row(&e, "Old/Heat (1995).mkv").is_none());
    let top = e.top();
    assert_eq!(top.len(), 1, "one work: {top:?}");
    assert!(progress(&e, u, top[0].id).is_some_and(|p| p.watched));
    assert_eq!(e.detail(top[0].id).assets.len(), 1);
}

/// With no matching tombstone, attaching only stores the fingerprint.
#[test]
fn attach_fingerprint_without_a_tombstone_only_stores_it() {
    let e = env(LibraryKind::Movies);
    let mut f = facts("Heat (1995).mkv");
    let fp = f.fp128.take().unwrap();
    f.oshash = None;
    e.apply(vec![FileUpsert {
        facts: f.clone(),
        probe: ProbeUpdate::Keep,
        guess: Some(movie("Heat", Some(1995))),
    }]);
    let out = e
        .cat
        .writer()
        .attach_fingerprints(
            e.root,
            vec![FileFingerprint {
                rel_path: f.rel_path.clone(),
                size: f.size,
                mtime_ns: f.mtime_ns,
                fp128: fp,
                oshash: Some(7),
            }],
        )
        .wait()
        .unwrap();
    assert_eq!((out.files_reattached, out.items_added.len()), (0, 0));
    assert_eq!(row(&e, "Heat (1995).mkv").unwrap().fp128, Some(fp));
}

/// The fresh file was parsed as another title (moved and renamed): the
/// tombstone's asset follows it, and the old work's state is carried.
#[test]
fn attach_fingerprint_carries_the_old_work_into_the_renamed_one() {
    let e = env(LibraryKind::Movies);
    let up = upsert(
        "Teh Matrix (1999).mkv",
        movie("Teh Matrix", Some(1999)),
        Some(seed("h264_mkv")),
    );
    let old_facts = up.facts.clone();
    e.apply(vec![up]);
    let old = e.find_top("Teh Matrix").id;
    let u = user(&e);
    e.cat.writer().set_watched(u, old, true).wait().unwrap();
    let tomb = row(&e, "Teh Matrix (1999).mkv").unwrap();
    e.batch(ScanBatch {
        root_id: e.root,
        missing: vec![tomb.id],
        ..Default::default()
    });
    let fresh = same_content("Disk2/The Matrix (1999).mkv", &old_facts);
    e.apply(vec![FileUpsert {
        facts: fresh.clone(),
        probe: ProbeUpdate::Probed(Box::new(seed("h264_mkv"))),
        guess: Some(movie("The Matrix", Some(1999))),
    }]);
    let out = e
        .cat
        .writer()
        .attach_fingerprints(
            e.root,
            vec![FileFingerprint {
                rel_path: fresh.rel_path.clone(),
                size: fresh.size,
                mtime_ns: fresh.mtime_ns,
                fp128: old_facts.fp128.unwrap(),
                oshash: None,
            }],
        )
        .wait()
        .unwrap();
    assert_eq!(out.files_reattached, 1);
    let new = e.find_top("The Matrix").id;
    assert!(progress(&e, u, new).is_some_and(|p| p.watched));
    assert!(!item_exists(&e, old));
    assert_eq!(row(&e, "Disk2/The Matrix (1999).mkv").unwrap().id, tomb.id);
}

/// Several tombstones with the same content: the one with the same file
/// name wins, then one in a same-named folder, then the most recent.
#[test]
fn reattach_prefers_the_same_name_then_the_same_folder() {
    let e = env(LibraryKind::Movies);
    let fp = 0xfeed_u128;
    let mk = |path: &str, title: &str| {
        let mut f = facts(path);
        f.size = 4_242_424;
        f.fp128 = Some(fp);
        FileUpsert {
            facts: f,
            probe: ProbeUpdate::Keep,
            guess: Some(movie(title, Some(2000))),
        }
    };
    e.apply(vec![
        mk("A/One.mkv", "One"),
        mk("B/Two.mkv", "Two"),
        mk("C/Three.mkv", "Three"),
    ]);
    let ids: Vec<Id> = ["A/One.mkv", "B/Two.mkv", "C/Three.mkv"]
        .iter()
        .map(|p| e.file_id(p))
        .collect();
    e.batch(ScanBatch {
        root_id: e.root,
        missing: ids.clone(),
        ..Default::default()
    });
    // Same name, another folder: "B/Two.mkv"'s identity.
    let out = e.apply(vec![mk("Z/Two.mkv", "Two")]);
    assert_eq!(out.files_reattached, 1);
    assert_eq!(e.file_id("Z/Two.mkv"), ids[1]);
    // Same folder name, another file name: "A/One.mkv"'s.
    let out = e.apply(vec![mk("Q/A/Other.mkv", "Other")]);
    assert_eq!(out.files_reattached, 1);
    assert_eq!(e.file_id("Q/A/Other.mkv"), ids[0]);
}

// ---- subtree snapshots -----------------------------------------------------

#[test]
fn subtree_snapshot_is_the_directory_and_below() {
    let e = env(LibraryKind::Movies);
    e.apply(
        ["A/x.mkv", "A/B/y.mkv", "AB/z.mkv", "A0.mkv", "a.mkv"]
            .iter()
            .map(|p| upsert(p, movie(p, None), None))
            .collect(),
    );
    let root = e.root;
    let keys = |prefixes: Vec<String>| -> Vec<String> {
        let mut k: Vec<String> = e
            .cat
            .read_blocking(move |c| scan_support::load_subtrees_snapshot(c, root, &prefixes))
            .unwrap()
            .into_keys()
            .collect();
        k.sort();
        k
    };
    assert_eq!(keys(vec!["A".into()]), ["A/B/y.mkv", "A/x.mkv"]);
    assert_eq!(keys(vec!["A/B".into(), "AB/".into()]), ["A/B/y.mkv", "AB/z.mkv"]);
    assert_eq!(keys(vec!["A".into(), "A/B".into()]), ["A/B/y.mkv", "A/x.mkv"]);
    assert_eq!(keys(vec!["".into()]).len(), 5);
    let one = e
        .cat
        .read_blocking(move |c| scan_support::load_subtree_snapshot(c, root, "AB"))
        .unwrap();
    assert_eq!(one.len(), 1);
}

// ---- cached library counts -------------------------------------------------

#[test]
fn cached_library_counts_follow_every_file_change() {
    let e = env(LibraryKind::Movies);
    let counts = e.cat.file_counts().clone();
    let both = |e: &Env| {
        let c2 = counts.clone();
        let cached = e
            .cat
            .read_blocking(move |c| read::list_libraries_cached(c, &c2))
            .unwrap();
        let plain = e.cat.read_blocking(read::list_libraries).unwrap();
        assert_eq!(cached, plain);
        (cached[0].scan.files_seen, cached[0].scan.files_probed)
    };
    assert_eq!(both(&e), (0, 0));
    e.apply(vec![
        upsert(
            "Heat (1995).mkv",
            movie("Heat", Some(1995)),
            Some(seed("h264_mkv")),
        ),
        upsert("Alien (1979).mkv", movie("Alien", Some(1979)), None),
    ]);
    assert_eq!(both(&e), (2, 1));
    // Reads served from the cache stay right after a user-state write…
    let heat = e.find_top("Heat").id;
    let u = user(&e);
    e.cat.writer().set_watched(u, heat, true).wait().unwrap();
    assert_eq!(both(&e), (2, 1));
    // …and change with the files.
    let id = e.file_id("Alien (1979).mkv");
    e.batch(ScanBatch {
        root_id: e.root,
        missing: vec![id],
        ..Default::default()
    });
    assert_eq!(both(&e), (1, 1));
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let libs = rt.block_on(e.cat.list_libraries()).unwrap();
    assert_eq!(libs[0].scan.files_seen, 1);
    let one = rt.block_on(e.cat.library(e.lib.id)).unwrap().unwrap();
    assert_eq!(one.scan.files_seen, 1);
}

// ---- art jobs --------------------------------------------------------------

fn claim(e: &Env, kind: &str, n: u32) -> Vec<flux_catalog::JobLease> {
    e.cat
        .writer()
        .claim_jobs(ClaimJobs::new("t", &[kind], n, Duration::from_secs(60)))
        .wait()
        .unwrap()
}

#[test]
fn art_jobs_run_newest_first_and_a_bump_moves_one_ahead() {
    let e = env(LibraryKind::Movies);
    for (p, t) in [("A.mkv", "Alpha"), ("B.mkv", "Beta"), ("C.mkv", "Gamma")] {
        e.apply(vec![upsert(p, movie(t, Some(2000)), None)]);
    }
    let w = e.cat.writer();
    for j in claim(&e, JOB_MATCH, 10) {
        w.finish_job(j.id, j.attempt, JobResult::Done).wait().unwrap();
    }
    let ids: Vec<Id> = ["Alpha", "Beta", "Gamma"]
        .iter()
        .map(|t| e.find_top(t).id)
        .collect();
    // The oldest work's art is asked for by the UI.
    assert!(w.bump_art(ids[0]).wait().unwrap());
    let order: Vec<Option<Id>> = claim(&e, JOB_ART, 10).into_iter().map(|j| j.item_id).collect();
    assert_eq!(order, vec![Some(ids[0]), Some(ids[2]), Some(ids[1])]);
    // Nothing queued any more.
    assert!(!w.bump_art(ids[1]).wait().unwrap());
}

#[test]
fn bumping_an_episode_bumps_its_shows_art_and_the_match_it_waits_for() {
    let e = env(LibraryKind::Shows);
    e.apply(vec![upsert(
        "Lost/Season 1/Lost - S01E01.mkv",
        episode("Lost", None, 1, &[1]),
        None,
    )]);
    e.apply(vec![upsert(
        "Dark/Season 1/Dark - S01E01.mkv",
        episode("Dark", None, 1, &[1]),
        None,
    )]);
    let lost = e.find_top("Lost").id;
    let season = e.children(lost)[0].id;
    let ep = e.children(season)[0].id;
    let w = e.cat.writer();
    assert!(w.bump_art(ep).wait().unwrap());
    // Lost's match runs first (interactive), then its art.
    let m = claim(&e, JOB_MATCH, 1);
    assert_eq!(m[0].item_id, Some(lost));
    w.finish_job(m[0].id, m[0].attempt, JobResult::Done)
        .wait()
        .unwrap();
    let a = claim(&e, JOB_ART, 1);
    assert_eq!(a[0].item_id, Some(lost));
}

// ---- bulk mode -------------------------------------------------------------

fn cache_kib(e: &Env) -> i64 {
    -e.cat
        .writer()
        .exec(|c| Ok(c.query_row("PRAGMA cache_size", [], |r| r.get::<_, i64>(0))?))
        .wait()
        .unwrap()
}

#[test]
fn bulk_mode_raises_the_writer_cache_then_restores_it_and_truncates_the_wal() {
    let e = env(LibraryKind::Movies);
    let w = e.cat.writer();
    assert_eq!(cache_kib(&e), 16 * 1024, "small cache at rest");
    w.begin_bulk().wait().unwrap();
    w.begin_bulk().wait().unwrap();
    assert_eq!(cache_kib(&e), 64 * 1024, "scan-sized cache while a scan writes");
    e.apply(
        (0..200)
            .map(|i| {
                upsert(
                    &format!("f{i}.mkv"),
                    movie(&format!("Film {i}"), Some(2000)),
                    None,
                )
            })
            .collect(),
    );
    let wal = format!("{}-wal", e.cat.path().display());
    assert!(std::fs::metadata(&wal).unwrap().len() > 0);
    w.end_bulk().wait().unwrap();
    assert_eq!(cache_kib(&e), 64 * 1024, "nested: still in bulk");
    w.end_bulk().wait().unwrap();
    assert_eq!(cache_kib(&e), 16 * 1024, "back to the idle cache");
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0, "WAL truncated");
    // An extra end is harmless.
    w.end_bulk().wait().unwrap();
    assert_eq!(cache_kib(&e), 16 * 1024);
}

// ---- SEC-2 (catalog side): what the scan recorded, for the server ----------

/// `asset_location` carries the identity the scan recorded (so a server can
/// `fstat` what it opened and refuse a retargeted link or a swapped file),
/// the root, and the last probe error.
#[test]
fn asset_location_carries_the_recorded_identity_and_probe_error() {
    let e = env(LibraryKind::Movies);
    let up = upsert(
        "Heat (1995).mkv",
        movie("Heat", Some(1995)),
        Some(seed("h264_mkv")),
    );
    let mut f = up.facts.clone();
    e.apply(vec![up]);
    let asset = e.detail(e.find_top("Heat").id).assets[0].id;
    let loc = e
        .cat
        .read_blocking(move |c| read::asset_location(c, asset))
        .unwrap()
        .unwrap();
    assert_eq!((loc.dev, loc.ino, loc.mtime_ns), (f.dev, f.ino, f.mtime_ns));
    assert_eq!(loc.root_path, "/media/lib");
    assert!(loc.path.starts_with(&loc.root_path));
    assert_eq!(loc.probe_error, None);
    f.mtime_ns += 1;
    e.batch(ScanBatch {
        root_id: e.root,
        upserts: vec![FileUpsert {
            facts: f,
            probe: ProbeUpdate::Failed("open: Permission denied".into()),
            guess: None,
        }],
        ..Default::default()
    });
    let loc = e
        .cat
        .read_blocking(move |c| read::asset_location(c, asset))
        .unwrap()
        .unwrap();
    assert_eq!(loc.probe_error.as_deref(), Some("open: Permission denied"));
    assert!(loc.available, "a probe failure stays playable by name");
}

/// The cached listing's item count follows merges too.
#[test]
fn cached_library_counts_follow_a_merge() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![
        upsert("The Wire (2002)/a.mkv", movie("The Wire", Some(2002)), None),
        upsert("Wire (2002)/b.mkv", movie("Wire", Some(2002)), None),
    ]);
    let counts = e.cat.file_counts().clone();
    let items = |e: &Env| {
        let c2 = counts.clone();
        e.cat
            .read_blocking(move |c| read::list_libraries_cached(c, &c2))
            .unwrap()[0]
            .item_count
    };
    assert_eq!(items(&e), 2);
    let (into, from) = (e.find_top("The Wire").id, e.find_top("Wire").id);
    e.cat.writer().merge_items(from, into).wait().unwrap();
    assert_eq!(items(&e), 1);
}
