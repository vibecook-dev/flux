//! flux-scan end to end on the real corpus (library-tiny and scratch clones
//! of it). Each test skips when the corpus SSD is not mounted.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use flux_catalog::synth::{read_manifest, ManifestEntry};
use flux_catalog::{scan_support, FileState, RootState};
use flux_core::api::Event;
use flux_core::LibraryKind;
use flux_scan::{Priority, ScanConfig, ScanError, ScanMode, ScannerOptions, StartupCatchUp};
use mille_core::{ReadHook, ReadKind, ReadProgress};
use serde_json::json;
use support::*;

/// library-tiny as a user would set it up: Movies/ as a movie library and
/// TV/ as a shows library. The catalog must match the ground truth exactly,
/// and an unchanged rescan must open nothing and write nothing.
#[test]
fn library_tiny_end_to_end_and_zero_touch_rescan() {
    let Some(media) = corpus() else { return };
    let root = media.join("library-tiny");
    let truth = read_manifest(&media.join("library-tiny.manifest.jsonl")).unwrap();
    let scratch = Scratch::new("tiny-e2e");
    let cat = open_catalog(&scratch.dir);
    let s = scanner(&cat);
    for (folder, kind) in [("Movies", LibraryKind::Movies), ("TV", LibraryKind::Shows)] {
        let (lib, root_id) = add_library(&cat, folder, kind, &root.join(folder));
        let mut events = s.subscribe();
        let first = scan(&s, lib);
        // Progress while walking (total unknown), after the walk (known),
        // and items as they commit.
        let (mut walking, mut known, mut items) = (false, false, 0usize);
        while let Ok(e) = events.try_recv() {
            match e {
                Event::ScanProgress { status, .. } => match status.files_total {
                    None => walking = true,
                    Some(_) => known = true,
                },
                Event::ItemsChanged { added, .. } => items += added.len(),
                _ => {}
            }
        }
        assert!(walking && known, "{folder}: progress events");
        assert_eq!(
            items as u64, first.items_added,
            "{folder}: ItemsChanged covers every new item"
        );
        let st = s.status(lib);
        assert_eq!(st.files_total, Some(first.media_files));
        assert_eq!(st.files_seen, first.media_files);
        assert_eq!(first.probe_failures, 0, "{folder}: {first:?}");
        assert!(first.unread_dirs.is_empty() && first.unavailable_roots.is_empty());
        assert_eq!(
            first.media_files_opened, first.media_files,
            "{folder}: every media file opened once"
        );
        let entries: Vec<&ManifestEntry> = truth
            .iter()
            .filter(|e| e.path.starts_with(&format!("{folder}/")))
            .collect();
        let gt = truth::compare(&cat, lib, root_id, folder, &entries).unwrap();
        assert_eq!(
            gt["files_correct"], gt["media_files_in_manifest"],
            "{folder}: misplaced {}",
            gt["wrong_by_class"]
        );
        for (k, n) in gt["expected_items"].as_object().unwrap() {
            let got = gt["catalog_items"].get(k).cloned().unwrap_or(json!(0));
            assert_eq!(&got, n, "{folder}: {k} items");
        }
        // Every media file got a fingerprint (from the background pass,
        // after the files were committed).
        assert!(
            s.wait_fingerprints(Duration::from_secs(120)),
            "{folder}: fingerprint pass finished"
        );
        assert_eq!(first.fingerprints_deferred, first.media_files, "{folder}");
        let snap = snapshot(&cat, root_id);
        assert!(
            snap.values().all(|r| r.fp128.is_some()),
            "{folder}: fp128 for every file"
        );

        let before = writer_commands(&cat);
        let again = scan(&s, lib);
        assert_eq!(again.media_files_opened, 0, "{folder}: rescan opened files");
        assert_eq!(again.writer_commands, 0, "{folder}: rescan writer commands");
        assert_eq!(
            writer_commands(&cat),
            before,
            "{folder}: catalog writer saw commands"
        );
        assert_eq!(again.unchanged, first.media_files);
        assert_eq!(again.added + again.changed + again.moved + again.missing, 0);
    }
    s.shutdown(Duration::from_secs(5));
}

/// A scratch library of a few movie folders from library-tiny.
struct Small {
    scratch: Scratch,
    root: PathBuf,
    cat: Arc<flux_catalog::Catalog>,
    lib: i64,
    root_id: i64,
}

fn small(name: &str, folders: &[&str]) -> Option<Small> {
    let media = corpus()?;
    let scratch = Scratch::new(name);
    let root = scratch.path("Movies");
    std::fs::create_dir_all(&root).unwrap();
    for f in folders {
        clone_tree(&media.join("library-tiny/Movies").join(f), &root.join(f));
    }
    let cat = open_catalog(&scratch.dir);
    let (lib, root_id) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    Some(Small {
        scratch,
        root,
        cat,
        lib,
        root_id,
    })
}

const MATRIX: &str = "The Matrix (1999)";
const ALIEN: &str = "Alien (1979) [tmdbid-170655]";
const F1917: &str = "1917 (2019) [tmdbid-908673]";

fn video_in(dir: &Path) -> PathBuf {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| flux_core::is_video_extension(p.extension().and_then(|e| e.to_str()).unwrap_or("")))
        .expect("a video file")
}

#[test]
fn renames_and_moves_keep_the_file_and_open_nothing() {
    let Some(t) = small("moves", &[MATRIX, ALIEN, F1917]) else {
        return;
    };
    let s = scanner(&t.cat);
    let first = scan(&s, t.lib);
    assert_eq!(first.added, first.media_files);
    // The background pass fingerprints them (the copy below is found by it).
    assert!(s.wait_fingerprints(Duration::from_secs(60)));
    let snap = snapshot(&t.cat, t.root_id);

    // 1. Rename in place (same inode).
    let src = video_in(&t.root.join(MATRIX));
    let rel_old = format!("{MATRIX}/{}", src.file_name().unwrap().to_string_lossy());
    let renamed = t.root.join(MATRIX).join("The Matrix (1999) - renamed.avi");
    std::fs::rename(&src, &renamed).unwrap();
    let r = scan(&s, t.lib);
    assert_eq!((r.moved, r.added, r.missing), (1, 0, 0), "{r:?}");
    assert_eq!(r.media_files_opened, 0, "an inode rename opens nothing");
    let after = snapshot(&t.cat, t.root_id);
    let moved = &after[&format!("{MATRIX}/The Matrix (1999) - renamed.avi")];
    assert_eq!(moved.id, snap[&rel_old].id, "same file row");
    assert_eq!(moved.asset_id, snap[&rel_old].asset_id, "same asset");
    assert!(!after.contains_key(&rel_old));

    // 2. Folder rename (every file inside moves).
    std::fs::rename(t.root.join(ALIEN), t.root.join("Alien (1979)")).unwrap();
    let r = scan(&s, t.lib);
    assert_eq!((r.moved, r.added, r.missing), (1, 0, 0), "{r:?}");
    assert_eq!(r.media_files_opened, 0);

    // 3. Copy + delete (a new inode, as across volumes): found by fp128.
    let src = video_in(&t.root.join(F1917));
    let name = src.file_name().unwrap().to_string_lossy().into_owned();
    let old_id = snapshot(&t.cat, t.root_id)[&format!("{F1917}/{name}")].id;
    let dst_dir = t.root.join("1917 (2019)");
    std::fs::create_dir_all(&dst_dir).unwrap();
    let dst = dst_dir.join(&name);
    std::fs::write(&dst, std::fs::read(&src).unwrap()).unwrap(); // plain copy: new inode, new btime
    std::fs::remove_file(&src).unwrap();
    let r = scan(&s, t.lib);
    assert_eq!((r.moved, r.added, r.missing), (1, 0, 0), "{r:?}");
    assert_eq!(r.media_files_opened, 1, "only the new inode is fingerprinted");
    assert_eq!(
        snapshot(&t.cat, t.root_id)[&format!("1917 (2019)/{name}")].id,
        old_id
    );

    // Nothing left to do.
    let r = scan(&s, t.lib);
    assert_eq!((r.writer_commands, r.media_files_opened), (0, 0), "{r:?}");
    drop(t.scratch);
}

#[test]
fn deleted_file_becomes_missing() {
    let Some(t) = small("missing", &[MATRIX, ALIEN]) else {
        return;
    };
    let s = scanner(&t.cat);
    scan(&s, t.lib);
    let victim = video_in(&t.root.join(MATRIX));
    let rel = format!("{MATRIX}/{}", victim.file_name().unwrap().to_string_lossy());
    std::fs::remove_file(&victim).unwrap();
    let r = scan(&s, t.lib);
    assert_eq!((r.missing, r.moved, r.added), (1, 0, 0), "{r:?}");
    assert!(r.items_removed >= 1, "its movie is hidden: {r:?}");
    assert_eq!(snapshot(&t.cat, t.root_id)[&rel].state, FileState::Missing);
    let r = scan(&s, t.lib);
    assert_eq!(
        (r.writer_commands, r.media_files_opened),
        (0, 0),
        "a tombstone stays put: {r:?}"
    );
}

#[test]
fn unreachable_root_is_unavailable_never_deleted_then_restored() {
    let Some(t) = small("unavail", &[MATRIX, ALIEN]) else {
        return;
    };
    let s = scanner(&t.cat);
    let mut events = s.subscribe();
    let first = scan(&s, t.lib);
    let n = first.media_files;

    let away = t.scratch.path("Movies.away");
    std::fs::rename(&t.root, &away).unwrap();
    let r = scan(&s, t.lib);
    assert_eq!(r.unavailable_roots, vec![t.root.to_string_lossy().into_owned()]);
    assert_eq!(r.missing, 0);
    let rid = t.root_id;
    let info = t
        .cat
        .read_blocking(move |c| scan_support::root_info(c, rid))
        .unwrap()
        .unwrap();
    assert_eq!(info.state, RootState::Unavailable);
    let snap = snapshot(&t.cat, t.root_id);
    assert_eq!(snap.len() as u64, n, "nothing deleted");
    assert!(snap.values().all(|r| r.state == FileState::Unavailable));
    assert_eq!(s.status(t.lib).state, flux_core::api::ScanState::Unavailable);

    std::fs::rename(&away, &t.root).unwrap();
    let r = scan(&s, t.lib);
    assert!(r.unavailable_roots.is_empty());
    assert_eq!((r.missing, r.added, r.media_files_opened), (0, 0, 0), "{r:?}");
    let info = t
        .cat
        .read_blocking(move |c| scan_support::root_info(c, rid))
        .unwrap()
        .unwrap();
    assert_eq!(info.state, RootState::Available);
    assert!(snapshot(&t.cat, t.root_id)
        .values()
        .all(|r| r.state == FileState::Present));

    let mut avail = Vec::new();
    while let Ok(e) = events.try_recv() {
        if let Event::SourceAvailability { available, .. } = e {
            avail.push(available);
        }
    }
    assert_eq!(avail, vec![false, true]);
}

#[test]
fn empty_mount_point_is_unavailable_not_a_mass_delete() {
    let Some(t) = small("emptymount", &[MATRIX]) else {
        return;
    };
    let s = scanner(&t.cat);
    scan(&s, t.lib);
    // The volume "unmounts" and leaves an empty directory behind.
    std::fs::rename(&t.root, t.scratch.path("elsewhere")).unwrap();
    std::fs::create_dir_all(&t.root).unwrap();
    let r = scan(&s, t.lib);
    assert_eq!(r.missing, 0, "{r:?}");
    assert_eq!(r.unavailable_roots.len(), 1);
}

#[cfg(unix)]
#[test]
fn unreadable_directory_keeps_its_files() {
    let Some(t) = small("unread", &[MATRIX, ALIEN]) else {
        return;
    };
    let s = scanner(&t.cat);
    scan(&s, t.lib);
    let locked = t.root.join(ALIEN);
    let chmod = |mode: &str| {
        assert!(std::process::Command::new("chmod")
            .arg(mode)
            .arg(&locked)
            .status()
            .unwrap()
            .success());
    };
    chmod("000");
    let r = scan(&s, t.lib);
    chmod("755");
    assert_eq!(r.missing, 0, "{r:?}");
    assert_eq!(r.unread_dirs, vec![locked.to_string_lossy().into_owned()]);
    assert_eq!(s.status(t.lib).dirs_unavailable, 1);
    assert!(snapshot(&t.cat, t.root_id)
        .iter()
        .filter(|(k, _)| k.starts_with(ALIEN))
        .all(|(_, r)| r.state == FileState::Present));
    let r = scan(&s, t.lib);
    assert_eq!((r.writer_commands, r.media_files_opened), (0, 0), "{r:?}");
}

/// Parks listings of one directory past the stall deadline.
struct SlowDir {
    dir: PathBuf,
    delay: Duration,
}

impl ReadHook for SlowDir {
    fn before_read(&self, kind: ReadKind, path: &Path, _p: &ReadProgress) {
        if kind == ReadKind::ListDir && path == self.dir {
            std::thread::sleep(self.delay);
        }
    }
}

#[test]
fn a_directory_past_its_deadline_is_unread_not_deleted() {
    let Some(t) = small("deadline", &[MATRIX, ALIEN]) else {
        return;
    };
    scan(&scanner(&t.cat), t.lib);
    let slow = t.root.join(MATRIX);
    let cfg = ScanConfig {
        first_response: Duration::from_millis(150),
        stall: Duration::from_millis(100),
        ..config()
    };
    let s = scanner_with(
        &t.cat,
        cfg,
        ScannerOptions {
            startup: StartupCatchUp::Nothing,
            read_hook: Some(Arc::new(SlowDir {
                dir: slow.clone(),
                delay: Duration::from_millis(600),
            })),
            ..ScannerOptions::default()
        },
    );
    let r = scan(&s, t.lib);
    assert_eq!(r.missing, 0, "{r:?}");
    assert_eq!(r.unread_dirs, vec![slow.to_string_lossy().into_owned()]);
}

#[test]
fn unknown_containers_go_to_ffprobe_with_a_timeout() {
    let Some(media) = corpus() else { return };
    let have_ffprobe = std::process::Command::new(flux_probe::ffprobe::ffprobe_bin())
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !have_ffprobe {
        eprintln!("skipping: no ffprobe");
        return;
    }
    let scratch = Scratch::new("ffprobe");
    let root = scratch.path("Movies");
    for (src, dst) in [
        ("flv_h264.flv", "Flash Movie (2004)/Flash Movie (2004).flv"),
        ("mpeg_ps.mpg", "Program Stream (1999)/Program Stream (1999).mpg"),
        ("vp9_vorbis.webm", "Web Movie (2015)/Web Movie (2015).webm"),
    ] {
        let d = root.join(dst);
        std::fs::create_dir_all(d.parent().unwrap()).unwrap();
        clone_tree(&media.join("probe-fixtures").join(src), &d);
    }
    let cat = open_catalog(&scratch.dir);
    let (lib, root_id) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let r = scan(&scanner(&cat), lib);
    assert_eq!((r.ffprobe_fallbacks, r.probe_failures), (2, 0), "{r:?}");
    let snap = snapshot(&cat, root_id);
    assert!(snap
        .values()
        .all(|r| r.state == FileState::Present && r.probe_version.is_some()));
    // A copy of a known file is held as a possible move, and still goes to
    // ffprobe: it ends up probed, not as a probe failure.
    let copy = root.join("Flash Copy (2005)/Flash Copy (2005).flv");
    std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
    clone_tree(&root.join("Flash Movie (2004)/Flash Movie (2004).flv"), &copy);
    let r = scan(&scanner(&cat), lib);
    assert_eq!(
        (r.added, r.moved, r.ffprobe_fallbacks, r.probe_failures),
        (1, 0, 1, 0),
        "{r:?}"
    );
    assert!(snapshot(&cat, root_id)
        .values()
        .all(|r| r.state == FileState::Present));

    // A run that can't finish in time is killed and recorded as a failure.
    let scratch2 = Scratch::new("ffprobe-timeout");
    let cat2 = open_catalog(&scratch2.dir);
    let (lib2, _) = add_library(&cat2, "Movies", LibraryKind::Movies, &root);
    let cfg = ScanConfig {
        ffprobe_timeout: Duration::from_millis(1),
        ..config()
    };
    let t = Instant::now();
    let r = scanner_with(&cat2, cfg, ScannerOptions::default())
        .scan_library(lib2, ScanMode::Quick, Priority::Interactive)
        .wait()
        .unwrap();
    assert_eq!((r.ffprobe_fallbacks, r.probe_failures), (0, 3), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(10));
}

/// Slows every listing a little, so a scan is still running when we act.
struct SlowAll(Duration);

impl ReadHook for SlowAll {
    fn before_read(&self, kind: ReadKind, _path: &Path, p: &ReadProgress) {
        if kind == ReadKind::ListDir {
            p.tick();
            std::thread::sleep(self.0);
            p.tick();
        }
    }
}

fn slow_scanner(cat: &Arc<flux_catalog::Catalog>, per_dir: Duration) -> flux_scan::Scanner {
    scanner_with(
        cat,
        ScanConfig {
            ssd_concurrency: 1,
            ..config()
        },
        ScannerOptions {
            startup: StartupCatchUp::Nothing,
            read_hook: Some(Arc::new(SlowAll(per_dir))),
            ..ScannerOptions::default()
        },
    )
}

#[test]
fn cancellation_stops_a_scan_and_a_later_scan_completes() {
    let Some(media) = corpus() else { return };
    let scratch = Scratch::new("cancel");
    let cat = open_catalog(&scratch.dir);
    let (lib, _) = add_library(&cat, "TV", LibraryKind::Shows, &media.join("library-tiny/TV"));
    let s = slow_scanner(&cat, Duration::from_millis(40));
    let h = s.scan_library(lib, ScanMode::Quick, Priority::Scheduled);
    std::thread::sleep(Duration::from_millis(200));
    let t = Instant::now();
    h.cancel();
    assert!(matches!(h.wait(), Err(ScanError::Cancelled)));
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "cancel took {:?}",
        t.elapsed()
    );
    assert!(h.is_finished());
    // What committed stays; a full scan afterwards completes the library.
    let r = scan(&scanner(&cat), lib);
    assert!(r.media_files > 0 && r.unread_dirs.is_empty());
    let r = scan(&scanner(&cat), lib);
    assert_eq!((r.writer_commands, r.media_files_opened), (0, 0));
}

#[test]
fn concurrent_requests_coalesce_and_priorities_order_admission() {
    let Some(t) = small("coalesce", &[MATRIX, ALIEN, F1917]) else {
        return;
    };
    let media = corpus().unwrap();
    // A second library on the same device, scanned slowly to hold it busy.
    let (tv, _) = add_library(&t.cat, "TV", LibraryKind::Shows, &media.join("library-tiny/TV"));
    let s = slow_scanner(&t.cat, Duration::from_millis(25));

    // Two identical library requests are one scan.
    let a = s.scan_library(tv, ScanMode::Quick, Priority::Watcher);
    let b = s.scan_library(tv, ScanMode::Quick, Priority::Scheduled);
    assert_eq!(a.id(), b.id());
    // Wait until it holds the device.
    eventually(Duration::from_secs(10), || s.status(tv).started_at).expect("TV scan starts");

    // Directory requests for the Movies library queue behind the running
    // Watcher-priority scan (Scheduled < Watcher) and merge while queued.
    let d1 = s.scan_dirs(t.lib, vec![t.root.join(MATRIX)], Priority::Scheduled);
    let d2 = s.scan_dirs(t.lib, vec![t.root.join(ALIEN)], Priority::Scheduled);
    assert_eq!(d1.id(), d2.id(), "queued directory requests merge");
    assert!(!d1.is_finished());
    // A Deep request can't join a running Quick scan: it queues a new one.
    let deep = s.scan_library(tv, ScanMode::Deep, Priority::Deep);
    assert_ne!(deep.id(), a.id());
    // An interactive request raises the merged job, which then overtakes.
    let d3 = s.scan_dirs(t.lib, vec![t.root.join(F1917)], Priority::Interactive);
    assert_eq!(d3.id(), d1.id());
    let dirs = d3.wait().unwrap();
    assert_eq!(dirs.media_files, 3, "{dirs:?}");
    assert!(
        !a.is_finished(),
        "the interactive job finished before the slow scan"
    );
    let full = a.wait().unwrap();
    assert!(full.media_files > 300);
    deep.cancel();
    let _ = deep.wait();
}

/// ROB-V1 (Deep half): a Deep scan selects files by `probe_version` and
/// `parse_version`: an up-to-date catalog is left alone, an old probe is
/// re-probed, an old parse is re-parsed without opening the file.
#[test]
fn deep_scan_selects_outdated_versions_and_unknown_library_errors() {
    let Some(t) = small("deep", &[MATRIX, ALIEN, F1917]) else {
        return;
    };
    let s = scanner(&t.cat);
    let first = scan(&s, t.lib);
    assert!(s.wait_fingerprints(Duration::from_secs(60)));
    let deep = || {
        s.scan_library(t.lib, ScanMode::Deep, Priority::Deep)
            .wait()
            .unwrap()
    };
    let r = deep();
    assert_eq!(
        (r.media_files_opened, r.changed, r.writer_commands),
        (0, 0, 0),
        "{r:?}"
    );
    assert_eq!(r.unchanged, first.media_files);
    // One file probed by an older flux-probe, one parsed by an older
    // flux-names (as after an upgrade).
    let snap = snapshot(&t.cat, t.root_id);
    let mut ids: Vec<i64> = snap.values().map(|r| r.id).collect();
    ids.sort_unstable();
    let (old_probe, old_parse) = (ids[0], ids[1]);
    t.cat
        .writer()
        .exec(move |c| {
            c.execute("UPDATE file SET probe_version = 1 WHERE id = ?1", [old_probe])?;
            c.execute("UPDATE file SET parse_version = 0 WHERE id = ?1", [old_parse])?;
            Ok(())
        })
        .wait()
        .unwrap();
    let r = deep();
    assert_eq!((r.media_files_opened, r.changed), (1, 2), "{r:?}");
    let snap = snapshot(&t.cat, t.root_id);
    assert!(snap
        .values()
        .all(|r| r.probe_version == Some(flux_core::PROBE_VERSION)
            && r.parse_version == Some(flux_core::PARSER_VERSION)));
    let r = deep();
    assert_eq!((r.media_files_opened, r.writer_commands), (0, 0), "{r:?}");
    assert!(matches!(
        s.scan_library(9999, ScanMode::Quick, Priority::Interactive)
            .wait(),
        Err(ScanError::UnknownLibrary(9999))
    ));
}

#[test]
fn scan_dirs_walks_new_folders_and_tombstones_removed_ones() {
    let Some(t) = small("dirs", &[MATRIX, ALIEN]) else {
        return;
    };
    let s = scanner(&t.cat);
    scan(&s, t.lib);
    let media = corpus().unwrap();
    // A new movie folder (unknown to the catalog) appears; another goes.
    clone_tree(
        &media.join("library-tiny/Movies").join(F1917),
        &t.root.join(F1917),
    );
    std::fs::rename(t.root.join(MATRIX), t.scratch.path("matrix-gone")).unwrap();
    let r = s
        .scan_dirs(t.lib, vec![t.root.clone()], Priority::Interactive)
        .wait()
        .unwrap();
    assert_eq!((r.added, r.missing), (1, 1), "{r:?}");
    // Directories outside the library are ignored.
    let r = s
        .scan_dirs(t.lib, vec![PathBuf::from("/tmp")], Priority::Interactive)
        .wait()
        .unwrap();
    assert_eq!(r.media_files, 0);
}

#[test]
fn a_move_between_roots_of_one_volume_re_attaches() {
    let Some(media) = corpus() else { return };
    let scratch = Scratch::new("crossroot");
    let (a, b) = (scratch.path("Movies A"), scratch.path("Movies B"));
    clone_tree(&media.join("library-tiny/Movies").join(MATRIX), &a.join(MATRIX));
    clone_tree(&media.join("library-tiny/Movies").join(ALIEN), &a.join(ALIEN));
    std::fs::create_dir_all(&b).unwrap();
    let cat = open_catalog(&scratch.dir);
    let lib = cat
        .writer()
        .create_library(flux_core::api::CreateLibrary {
            name: "Movies".into(),
            kind: LibraryKind::Movies,
            roots: vec![a.to_string_lossy().into_owned(), b.to_string_lossy().into_owned()],
        })
        .wait()
        .unwrap();
    let roots = cat.read_blocking(scan_support::list_roots).unwrap();
    let id_of = |p: &Path| roots.iter().find(|r| Path::new(&r.path) == p).unwrap().id;
    let (ra, rb) = (id_of(&a), id_of(&b));
    let s = scanner(&t_cat(&cat));
    scan(&s, lib.id);
    let before = snapshot(&cat, ra);
    // Move one movie folder from root A to root B (same volume: same inode).
    std::fs::rename(a.join(MATRIX), b.join(MATRIX)).unwrap();
    let r = scan(&s, lib.id);
    assert_eq!((r.reattached, r.added), (1, 0), "{r:?}");
    let after_b = snapshot(&cat, rb);
    let (rel, row) = after_b.iter().next().expect("moved file in root B");
    let old = &before[rel];
    assert_eq!(row.asset_id, old.asset_id, "asset kept across roots");
    let r = scan(&s, lib.id);
    assert_eq!((r.writer_commands, r.media_files_opened), (0, 0), "{r:?}");
}

fn t_cat(c: &Arc<flux_catalog::Catalog>) -> Arc<flux_catalog::Catalog> {
    c.clone()
}

/// APFS refuses non-UTF-8 names (EILSEQ), so this runs where the filesystem
/// takes raw bytes; on macOS the name handling is unit-tested
/// (`paths::tests::non_utf8_names_are_not_decoded`).
#[cfg(target_os = "linux")]
#[test]
fn non_utf8_names_are_skipped_and_counted() {
    use std::os::unix::ffi::OsStrExt;
    let scratch = Scratch::new("nonutf8");
    let root = scratch.path("Movies");
    let dir = root.join("Heat (1995)");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Heat (1995).mkv"), b"not really a movie").unwrap();
    let bad = dir.join(std::ffi::OsStr::from_bytes(b"Heat \xff (1995).mkv"));
    if std::fs::write(&bad, b"x").is_err() {
        eprintln!("skipping: filesystem refuses non-UTF-8 names");
        return;
    }
    let cat = open_catalog(&scratch.dir);
    let (lib, root_id) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let r = scan(&scanner(&cat), lib);
    assert_eq!(r.non_utf8_skipped, 1, "{r:?}");
    assert_eq!(snapshot(&cat, root_id).len(), 1);
}

/// A two-part movie copied in one part at a time (synthetic files, no
/// corpus): the first part is parsed alone and does not stack; when the
/// second arrives, the first is re-parsed (no I/O) and both become parts of
/// one movie. A later rescan with nothing new re-parses nothing.
#[test]
fn a_part_arriving_later_restacks_the_earlier_one() {
    let scratch = Scratch::new("restack");
    let root = scratch.path("Movies");
    let dir = root.join("示例片-MP4");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("示例片A.mp4"), b"not really a movie, part one").unwrap();
    let cat = open_catalog(&scratch.dir);
    let (lib, root_id) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let s = scanner(&cat);
    // Scheduled, not Interactive: a failed probe is not retried at once, so
    // only the re-parse can change the first part.
    let quick = || {
        s.scan_library(lib, ScanMode::Quick, Priority::Scheduled)
            .wait()
            .expect("scan")
    };
    let guess_of = |name: &str| -> flux_core::NameGuess {
        let rel = format!("示例片-MP4/{name}");
        let json: String = cat
            .read_blocking(move |c| {
                Ok(c.query_row(
                    "SELECT m.guess FROM file f JOIN file_meta m ON m.file_id = f.id
                     WHERE f.root_id = ?1 AND f.rel_path = ?2",
                    (root_id, rel),
                    |r| r.get(0),
                )?)
            })
            .expect("guess");
        serde_json::from_str(&json).expect("guess json")
    };
    quick();
    let a = guess_of("示例片A.mp4");
    assert_eq!((a.title.as_deref(), a.part), (Some("示例片A"), None), "{a:#?}");

    std::fs::write(dir.join("示例片B.mp4"), b"not really a movie, part two").unwrap();
    let r = quick();
    assert_eq!(r.added, 1, "{r:?}");
    let (a, b) = (guess_of("示例片A.mp4"), guess_of("示例片B.mp4"));
    assert_eq!((a.title.as_deref(), a.part), (Some("示例片"), Some(1)), "{a:#?}");
    assert_eq!((b.title.as_deref(), b.part), (Some("示例片"), Some(2)), "{b:#?}");
    let titles: Vec<String> = cat
        .read_blocking(move |c| {
            let mut stmt = c.prepare(
                "SELECT DISTINCT i.title FROM file f
                 JOIN asset_item ai ON ai.asset_id = f.asset_id
                 JOIN item i ON i.id = ai.item_id
                 WHERE f.root_id = ?1 ORDER BY i.title",
            )?;
            let rows = stmt.query_map([root_id], |r| r.get(0))?;
            Ok(rows.collect::<Result<Vec<String>, _>>()?)
        })
        .expect("items");
    assert_eq!(titles, ["示例片"], "both parts belong to one movie");

    assert!(s.wait_fingerprints(Duration::from_secs(60)));
    let r = quick();
    assert_eq!((r.added, r.changed), (0, 0), "{r:?}");
}
