//! Phase D regressions at scanner level: the integrity review's repros
//! (INT-1…7, INT-V1, ported from its worktree), SEC-2's scan side, ROB-3,
//! ROB-4 and ROB-V1, and the deferred fingerprint pass. Each test skips when
//! the corpus is not configured (`FLUX_TEST_MEDIA`); scratch trees are
//! APFS clones on macOS. Unix only (symlinks, permission bits).
#![cfg(unix)]

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flux_catalog::{Catalog, CatalogError, FileState};
use flux_core::api::{CreateLibrary, Id};
use flux_core::LibraryKind;
use flux_scan::{
    Priority, ScanConfig, ScanError, ScanMode, ScanSummary, Scanner, ScannerOptions, StartupCatchUp,
};
use mille_core::{ReadHook, ReadKind, ReadProgress};
use support::*;

const MATRIX: &str = "The Matrix (1999)";
const ALIEN: &str = "Alien (1979) [tmdbid-170655]";
const F1917: &str = "1917 (2019) [tmdbid-908673]";
const HEAT: &str = "Heat (1995)";

fn movie_dir(name: &str) -> PathBuf {
    flux_test_media::media_dir("library-tiny/Movies").join(name)
}

/// An APFS clone on macOS, a copy elsewhere.
fn cp_c(src: &Path, dst: &Path) {
    flux_test_media::clone_file(src, dst).unwrap();
}

fn sc(cat: &Arc<Catalog>, hook: Option<Arc<dyn ReadHook>>) -> Scanner {
    scanner_with(
        cat,
        config(),
        ScannerOptions {
            startup: StartupCatchUp::Nothing,
            read_hook: hook,
            ..ScannerOptions::default()
        },
    )
}

fn scan_at(s: &Scanner, lib: Id, p: Priority) -> ScanSummary {
    s.scan_library(lib, ScanMode::Quick, p).wait().unwrap()
}

fn sql_i64(cat: &Catalog, sql: &'static str, id: Id) -> i64 {
    cat.read_blocking(move |c| Ok(c.query_row(sql, [id], |r| r.get(0))?))
        .unwrap()
}

/// `(title, state, assets)` of every top-level work.
fn works(cat: &Catalog, lib: Id) -> Vec<(String, String, i64)> {
    cat.read_blocking(move |c| {
        let mut st = c.prepare(
            "SELECT i.title, i.state, (SELECT count(*) FROM asset_item ai WHERE ai.item_id = i.id)
               FROM item i WHERE i.library_id = ?1 AND i.parent_id IS NULL ORDER BY i.title",
        )?;
        let rows = st.query_map([lib], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    })
    .unwrap()
}

fn wait_for(what: &str, f: impl Fn() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(
            t.elapsed() < Duration::from_secs(30),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Parks the first listing of `dir` until released, ticking its progress so
/// the stall deadline never fires (a slow directory, not an unread one).
struct Gate {
    dir: PathBuf,
    armed: AtomicBool,
    parked: AtomicBool,
    release: AtomicBool,
}

impl Gate {
    fn new(dir: PathBuf) -> Arc<Gate> {
        Arc::new(Gate {
            dir,
            armed: AtomicBool::new(true),
            parked: AtomicBool::new(false),
            release: AtomicBool::new(false),
        })
    }
}

impl ReadHook for Gate {
    fn before_read(&self, kind: ReadKind, path: &Path, p: &ReadProgress) {
        if kind == ReadKind::ListDir && path == self.dir && self.armed.swap(false, Ordering::SeqCst) {
            self.parked.store(true, Ordering::SeqCst);
            while !self.release.load(Ordering::SeqCst) {
                p.tick();
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

/// Delays every listing of one (switchable) directory, ticking.
struct Delay {
    dir: Mutex<Option<PathBuf>>,
    ms: u64,
}

impl ReadHook for Delay {
    fn before_read(&self, kind: ReadKind, path: &Path, p: &ReadProgress) {
        let hit = kind == ReadKind::ListDir && self.dir.lock().unwrap().as_deref() == Some(path);
        if hit {
            let t = Instant::now();
            while t.elapsed() < Duration::from_millis(self.ms) {
                p.tick();
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

fn user(cat: &Catalog) -> Id {
    cat.writer()
        .ensure_user(flux_core::api::Identity {
            login_name: "alice@example.com".into(),
            display_name: "Alice".into(),
            profile_pic_url: None,
            node_name: None,
            role: flux_core::api::Role::Owner,
            tags: Vec::new(),
        })
        .wait()
        .unwrap()
}

/// INT-1: fixing a typo in a movie's folder and file name (a pure rename,
/// opening nothing) keeps the resume point and the pinned poster on the
/// renamed movie.
#[test]
fn int1_a_title_fixing_rename_keeps_watch_state_and_the_pin() {
    if corpus().is_none() {
        return;
    }
    let s = Scratch::new("int1-retitle");
    let root = s.path("Movies");
    let typo = root.join("The Matirx (1999)");
    cp_c(
        &movie_dir(MATRIX).join("The Matrix (1999).avi"),
        &typo.join("The Matirx (1999).avi"),
    );
    clone_tree(&movie_dir(ALIEN), &root.join(ALIEN));
    let cat = open_catalog(&s.dir);
    let (lib, _) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let scn = sc(&cat, None);
    assert_eq!(scan(&scn, lib).added, 2);
    let (item, asset): (Id, Id) = cat
        .read_blocking(|c| {
            Ok(c.query_row(
                "SELECT i.id, a.id FROM item i JOIN asset a ON a.item_id = i.id WHERE i.title LIKE 'The Matirx%'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .unwrap();
    let u = user(&cat);
    let w = cat.writer();
    w.set_progress(u, item, Some(asset), 3_000, 100_000)
        .wait()
        .unwrap();
    let art = w
        .add_art(
            item,
            flux_catalog::NewArt {
                kind: flux_catalog::ArtKind::Poster,
                source: flux_core::api::ArtSource::Local,
                path: Some("/tmp/poster-the-user-chose.jpg".into()),
                url: None,
                width: None,
                height: None,
                blurhash: None,
                attribution: None,
                generator: None,
            },
        )
        .wait()
        .unwrap();
    assert!(w.pin_art(art).wait().unwrap());

    let fixed = root.join(MATRIX);
    std::fs::rename(&typo, &fixed).unwrap();
    std::fs::rename(
        fixed.join("The Matirx (1999).avi"),
        fixed.join("The Matrix (1999).avi"),
    )
    .unwrap();
    let r = scan(&scn, lib);
    assert_eq!((r.moved, r.added, r.media_files_opened), (1, 0, 0), "{r:?}");
    let new_item = sql_i64(&cat, "SELECT item_id FROM asset WHERE id = ?1", asset);
    assert_ne!(new_item, item);
    let p = cat
        .read_blocking(move |c| flux_catalog::read::progress(c, u, new_item))
        .unwrap();
    assert_eq!(
        p.map(|p| p.position_ms),
        Some(3_000),
        "resume point follows the rename"
    );
    assert_eq!(
        sql_i64(
            &cat,
            "SELECT count(*) FROM art WHERE item_id = ?1 AND pinned = 1",
            new_item
        ),
        1,
        "the pin follows"
    );
    assert_eq!(sql_i64(&cat, "SELECT count(*) FROM item WHERE id = ?1", item), 0);
    let cw: Vec<String> = cat
        .read_blocking(move |c| flux_catalog::read::continue_watching(c, u, 20))
        .unwrap()
        .into_iter()
        .map(|s| s.title)
        .collect();
    assert_eq!(cw, vec!["The Matrix".to_string()]);
}

/// INT-2: a symlink farm whose target disk is offline (the link dangles):
/// unread, not deleted; back online, nothing to do.
#[test]
fn int2_a_dangling_file_link_is_unread_not_missing() {
    if corpus().is_none() {
        return;
    }
    let s = Scratch::new("int2-dangling");
    let disk2 = s.path("disk2");
    clone_tree(&movie_dir(HEAT), &disk2.join(HEAT));
    let root = s.path("Movies");
    clone_tree(&movie_dir(F1917), &root.join(F1917));
    std::fs::create_dir_all(root.join(HEAT)).unwrap();
    std::os::unix::fs::symlink(
        disk2.join(HEAT).join("Heat (1995).avi"),
        root.join(HEAT).join("Heat (1995).avi"),
    )
    .unwrap();
    let cat = open_catalog(&s.dir);
    let (lib, root_id) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let scn = sc(&cat, None);
    let first = scan(&scn, lib);
    assert_eq!((first.added, first.links_external), (2, 1), "{first:?}");
    std::fs::rename(&disk2, s.path("disk2.offline")).unwrap();
    let r = scan(&scn, lib);
    assert_eq!(
        r.missing, 0,
        "an unreachable link target is not a deletion: {r:?}"
    );
    assert!(
        r.unread_dirs.iter().any(|d| d.ends_with("Heat (1995).avi")),
        "{r:?}"
    );
    assert!(snapshot(&cat, root_id)
        .values()
        .all(|r| r.state != FileState::Missing));
    std::fs::rename(s.path("disk2.offline"), &disk2).unwrap();
    let r = scan(&scn, lib);
    assert_eq!((r.writer_commands, r.missing), (0, 0), "{r:?}");
}

/// INT-2: a folder link into an offline disk. The walk's extension filter
/// drops the unresolved link, so its absence is looked at before anything
/// below it is called gone.
#[test]
fn int2_a_dangling_folder_link_is_unread_not_missing() {
    if corpus().is_none() {
        return;
    }
    let s = Scratch::new("int2-dangling-dir");
    let disk2 = s.path("disk2");
    cp_c(
        &movie_dir(HEAT).join("Heat (1995).avi"),
        &disk2.join(HEAT).join("Heat (1995).avi"),
    );
    let root = s.path("Movies");
    cp_c(
        &movie_dir(MATRIX).join("The Matrix (1999).avi"),
        &root.join(MATRIX).join("The Matrix (1999).avi"),
    );
    std::os::unix::fs::symlink(disk2.join(HEAT), root.join(HEAT)).unwrap();
    let cat = open_catalog(&s.dir);
    let (lib, _) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let scn = sc(&cat, None);
    let first = scan(&scn, lib);
    assert_eq!((first.added, first.links_external), (2, 1), "{first:?}");
    std::fs::rename(&disk2, s.path("disk2.offline")).unwrap();
    let r = scan(&scn, lib);
    assert_eq!(r.missing, 0, "{r:?}");
    assert_eq!(
        r.unread_dirs,
        vec![root.join(HEAT).to_string_lossy().into_owned()]
    );
}

/// INT-2: a second volume mounted inside a root is unmounted; its bare
/// mount point lists empty. `RUN_MOUNT=1` (attaches a 20 MB disk image).
#[test]
fn int2_an_unmounted_volume_below_a_root_is_unread_not_missing() {
    if corpus().is_none() || std::env::var_os("RUN_MOUNT").is_none() {
        eprintln!("skipping: RUN_MOUNT not set");
        return;
    }
    let sh = |cmd: &str, args: &[&str]| {
        Command::new(cmd)
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    let s = Scratch::new("int2-submount");
    let root = s.path("Media");
    clone_tree(&movie_dir(MATRIX), &root.join(MATRIX));
    let disk2 = root.join("disk2");
    std::fs::create_dir_all(&disk2).unwrap();
    let img = s.path("disk2.dmg");
    let (img_s, mnt) = (img.to_str().unwrap(), disk2.to_str().unwrap());
    assert!(sh(
        "hdiutil",
        &[
            "create",
            "-quiet",
            "-size",
            "20m",
            "-fs",
            "HFS+",
            "-volname",
            "fluxdisk2",
            img_s
        ]
    ));
    if !sh(
        "hdiutil",
        &[
            "attach",
            "-quiet",
            "-nobrowse",
            "-noverify",
            "-mountpoint",
            mnt,
            img_s,
        ],
    ) {
        eprintln!("skipping: hdiutil attach refused here");
        return;
    }
    std::fs::write(disk2.join(".metadata_never_index"), b"").unwrap();
    std::fs::create_dir_all(disk2.join(HEAT)).unwrap();
    std::fs::copy(
        movie_dir(HEAT).join("Heat (1995).avi"),
        disk2.join(HEAT).join("Heat (1995).avi"),
    )
    .unwrap();
    let cat = open_catalog(&s.dir);
    let (lib, _) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let scn = sc(&cat, None);
    let first = scan(&scn, lib);
    let detached = sh("hdiutil", &["detach", "-quiet", mnt]);
    let r = scan(&scn, lib);
    let _ = sh("hdiutil", &["detach", "-quiet", "-force", mnt]);
    assert!(detached);
    assert_eq!(first.added, 2);
    assert_eq!(r.missing, 0, "the unmounted disk's files are kept: {r:?}");
    assert_eq!(r.unread_dirs, vec![mnt.to_string()]);
}

/// INT-2 without mount privileges: the catalog's rows for `disk2/` carry
/// another device than the folder (as files on a volume mounted there do),
/// then the folder lists empty (the bare mount point). Unread, not missing;
/// and once the files are back, nothing was lost. An emptied folder on the
/// root's own device is still a deletion.
#[test]
fn int2_an_empty_mount_point_with_files_from_another_device_is_unread() {
    if corpus().is_none() {
        return;
    }
    let s = Scratch::new("int2-mountpoint");
    let root = s.path("Media");
    clone_tree(&movie_dir(MATRIX), &root.join(MATRIX));
    let disk2 = root.join("disk2");
    cp_c(
        &movie_dir(HEAT).join("Heat (1995).avi"),
        &disk2.join(HEAT).join("Heat (1995).avi"),
    );
    cp_c(
        &movie_dir(F1917).join("1917 (2019) [tmdbid-908673].mkv"),
        &root.join("Gone").join("1917.mkv"),
    );
    let cat = open_catalog(&s.dir);
    let (lib, root_id) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let scn = sc(&cat, None);
    assert_eq!(scan(&scn, lib).added, 3);
    // disk2's files were cataloged from another device.
    cat.writer()
        .exec(|c| {
            c.execute(
                "UPDATE file SET dev = dev + 4242 WHERE rel_path LIKE 'disk2/%'",
                [],
            )?;
            Ok(())
        })
        .wait()
        .unwrap();
    // "Unmount": the mount point stays, empty. And on the root's own
    // device, a folder is emptied for real.
    std::fs::rename(&disk2, s.path("disk2.offline")).unwrap();
    std::fs::create_dir(&disk2).unwrap();
    std::fs::remove_file(root.join("Gone").join("1917.mkv")).unwrap();
    let r = scan(&scn, lib);
    assert_eq!(r.missing, 1, "only the real deletion: {r:?}");
    assert_eq!(r.unread_dirs, vec![disk2.to_string_lossy().into_owned()]);
    let snap = snapshot(&cat, root_id);
    assert_eq!(
        snap["disk2/Heat (1995)/Heat (1995).avi"].state,
        FileState::Present
    );
    assert_eq!(snap["Gone/1917.mkv"].state, FileState::Missing);
    // "Remount": back, nothing lost.
    std::fs::remove_dir(&disk2).unwrap();
    std::fs::rename(s.path("disk2.offline"), &disk2).unwrap();
    let r = scan(&scn, lib);
    assert_eq!((r.missing, r.added), (0, 0), "{r:?}");
}

/// INT-3: "Scan now" while a scan of the library is running (and has
/// already listed the directory that changed) joins it, and the handle
/// completes only after a pass that saw the change.
#[test]
fn int3_scan_now_during_a_running_scan_sees_changes_in_listed_dirs() {
    if corpus().is_none() {
        return;
    }
    let s = Scratch::new("int3-join");
    let root = s.path("Movies");
    clone_tree(&movie_dir(MATRIX), &root.join(MATRIX));
    clone_tree(&movie_dir(ALIEN), &root.join("Slow").join(ALIEN));
    let cat = open_catalog(&s.dir);
    let (lib, root_id) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    assert_eq!(scan(&sc(&cat, None), lib).added, 2);
    let gate = Gate::new(root.join("Slow"));
    let scn = sc(&cat, Some(gate.clone() as Arc<dyn ReadHook>));
    let running = scn.scan_library(lib, ScanMode::Quick, Priority::Scheduled);
    wait_for("the scan to reach Slow/", || gate.parked.load(Ordering::SeqCst));
    clone_tree(&movie_dir(F1917), &root.join(F1917));
    let now = scn.scan_library(lib, ScanMode::Quick, Priority::Interactive);
    assert_eq!(now.id(), running.id(), "joined the running scan");
    gate.release.store(true, Ordering::SeqCst);
    let r = now.wait().unwrap();
    assert!(r.follow_up_passes >= 1, "{r:?}");
    assert!(
        snapshot(&cat, root_id).keys().any(|k| k.starts_with("1917")),
        "'Scan now' finished without the new movie: {r:?}"
    );
    let again = scan(&scn, lib);
    assert_eq!(again.writer_commands, 0, "{again:?}");
}

/// INT-3: the watcher's overflow (a library Quick scan at Watcher priority)
/// joining a running scan also gets the follow-up pass.
#[test]
fn int3_a_watcher_overflow_joining_a_running_scan_gets_a_follow_up() {
    if corpus().is_none() {
        return;
    }
    let s = Scratch::new("int3-overflow");
    let root = s.path("Movies");
    clone_tree(&movie_dir(ALIEN), &root.join("Slow").join(ALIEN));
    let cat = open_catalog(&s.dir);
    let (lib, root_id) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let gate = Gate::new(root.join("Slow"));
    let scn = sc(&cat, Some(gate.clone() as Arc<dyn ReadHook>));
    let running = scn.scan_library(lib, ScanMode::Quick, Priority::Scheduled);
    wait_for("the scan to reach Slow/", || gate.parked.load(Ordering::SeqCst));
    clone_tree(&movie_dir(MATRIX), &root.join(MATRIX));
    let overflow = scn.scan_library(lib, ScanMode::Quick, Priority::Watcher);
    assert_eq!(overflow.id(), running.id());
    gate.release.store(true, Ordering::SeqCst);
    overflow.wait().unwrap();
    assert_eq!(snapshot(&cat, root_id).len(), 2);
}

/// INT-4: a running scan's snapshot goes stale while a concurrent watcher
/// batch moves a file; the scan's reconcile must not tombstone the moved
/// row.
#[test]
fn int4_a_stale_snapshot_does_not_tombstone_a_concurrently_moved_file() {
    if corpus().is_none() {
        return;
    }
    let s = Scratch::new("int4-stale");
    let root = s.path("Movies");
    clone_tree(&movie_dir(MATRIX), &root.join("Old").join(MATRIX));
    std::fs::create_dir_all(root.join("New")).unwrap();
    let cat = open_catalog(&s.dir);
    let (lib, root_id) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    assert_eq!(scan(&sc(&cat, None), lib).added, 1);
    let old_rel = format!("Old/{MATRIX}/The Matrix (1999).avi");
    let id = snapshot(&cat, root_id)[&old_rel].id;
    let gate = Gate::new(root.join("Old"));
    let scn = sc(&cat, Some(gate.clone() as Arc<dyn ReadHook>));
    let full = scn.scan_library(lib, ScanMode::Quick, Priority::Scheduled);
    wait_for("the scan to reach Old/", || gate.parked.load(Ordering::SeqCst));
    std::thread::sleep(Duration::from_millis(100)); // New/ listed (empty)
    std::fs::rename(
        root.join("Old").join(MATRIX).join("The Matrix (1999).avi"),
        root.join("New").join("The Matrix (1999).avi"),
    )
    .unwrap();
    let w = scn
        .scan_dirs(
            lib,
            vec![root.join("Old").join(MATRIX), root.join("New")],
            Priority::Watcher,
        )
        .wait()
        .unwrap();
    assert_eq!(w.moved, 1, "{w:?}");
    gate.release.store(true, Ordering::SeqCst);
    let f = full.wait().unwrap();
    let after = snapshot(&cat, root_id);
    let row = &after["New/The Matrix (1999).avi"];
    assert_eq!(row.id, id);
    assert_eq!(
        row.state,
        FileState::Present,
        "the stale scan tombstoned a live file: {f:?}"
    );
    assert_eq!(
        works(&cat, lib),
        vec![("The Matrix".into(), "available".into(), 1)]
    );
}

/// INT-5: an alias of a folder inside the root is never walked, whatever
/// order the listings answer in: one asset, and rescans write nothing.
#[test]
fn int5_an_in_root_folder_alias_is_never_walked() {
    if corpus().is_none() {
        return;
    }
    let s = Scratch::new("int5-alias");
    let root = s.path("Movies");
    clone_tree(&movie_dir(MATRIX), &root.join("Real").join(MATRIX));
    std::fs::create_dir_all(root.join("Alias")).unwrap();
    std::os::unix::fs::symlink(root.join("Real").join(MATRIX), root.join("Alias").join(MATRIX)).unwrap();
    let cat = open_catalog(&s.dir);
    let (lib, _) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let delay = Arc::new(Delay {
        dir: Mutex::new(None),
        ms: 200,
    });
    let scn = sc(&cat, Some(delay.clone() as Arc<dyn ReadHook>));
    let mut writes = 0;
    for (i, slow) in ["Alias", "Real", "Alias", "Real", "Alias"].iter().enumerate() {
        *delay.dir.lock().unwrap() = Some(root.join(slow));
        let r = scan(&scn, lib);
        assert_eq!(r.links_skipped, 1, "{r:?}");
        if i == 0 {
            assert_eq!(r.added, 1, "{r:?}");
        } else {
            writes += r.writer_commands;
        }
        assert_eq!(
            works(&cat, lib),
            vec![("The Matrix".into(), "available".into(), 1)]
        );
    }
    assert_eq!(writes, 0, "the tree never changed after scan 0");
}

/// INT-5: two links to the same folder outside the root: the first in path
/// order is walked, the other never, whatever the timing.
#[test]
fn int5_an_external_folder_is_walked_through_one_link_only() {
    if corpus().is_none() {
        return;
    }
    let s = Scratch::new("int5-external");
    let outside = s.path("outside");
    clone_tree(&movie_dir(HEAT), &outside.join(HEAT));
    let root = s.path("Movies");
    clone_tree(&movie_dir(MATRIX), &root.join(MATRIX));
    std::fs::create_dir_all(root.join("B")).unwrap();
    std::os::unix::fs::symlink(outside.join(HEAT), root.join("A link")).unwrap();
    std::os::unix::fs::symlink(outside.join(HEAT), root.join("B").join(HEAT)).unwrap();
    let cat = open_catalog(&s.dir);
    let (lib, root_id) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let delay = Arc::new(Delay {
        dir: Mutex::new(None),
        ms: 150,
    });
    let scn = sc(&cat, Some(delay.clone() as Arc<dyn ReadHook>));
    let mut writes = 0;
    for (i, slow) in ["", "B", "", "B"].iter().enumerate() {
        *delay.dir.lock().unwrap() = Some(root.join(slow));
        let r = scan(&scn, lib);
        assert_eq!((r.links_external, r.links_skipped), (1, 1), "{r:?}");
        if i > 0 {
            writes += r.writer_commands;
        }
    }
    assert_eq!(writes, 0);
    let mut keys: Vec<String> = snapshot(&cat, root_id).into_keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "A link/Heat (1995).avi".to_string(),
            format!("{MATRIX}/The Matrix (1999).avi")
        ]
    );
}

/// INT-6 / ROB-V1: a changed file unreadable at scan time is stored as a
/// probe failure, keeps its asset's facts, and is retried: not by a
/// Scheduled scan inside its backoff, at once by an Interactive one.
#[test]
fn int6_a_transient_open_failure_is_retried() {
    if corpus().is_none() {
        return;
    }
    let s = Scratch::new("int6-retry");
    let root = s.path("Movies");
    clone_tree(&movie_dir(F1917), &root.join(F1917));
    let cat = open_catalog(&s.dir);
    let (lib, root_id) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let scn = sc(&cat, None);
    assert_eq!(scan(&scn, lib).added, 1);
    let f = root.join(F1917).join("1917 (2019) [tmdbid-908673].mkv");
    let facts = |cat: &Catalog| -> (Option<i64>, String) {
        cat.read_blocking(|c| {
            Ok(c.query_row("SELECT duration_ms, label FROM asset", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?)
        })
        .unwrap()
    };
    let probed = facts(&cat);
    assert!(probed.0.is_some());
    assert!(Command::new("touch").arg(&f).status().unwrap().success());
    assert!(Command::new("chmod")
        .arg("000")
        .arg(&f)
        .status()
        .unwrap()
        .success());
    let r = scan(&scn, lib);
    assert_eq!((r.changed, r.probe_failures), (1, 1), "{r:?}");
    assert_eq!(facts(&cat), probed, "a failed re-probe keeps the playback facts");
    assert!(Command::new("chmod")
        .arg("644")
        .arg(&f)
        .status()
        .unwrap()
        .success());
    // Inside the backoff, a background scan leaves it alone…
    let r = scan_at(&scn, lib, Priority::Scheduled);
    assert_eq!(
        (r.probe_retries, r.media_files_opened, r.writer_commands),
        (0, 0, 0),
        "{r:?}"
    );
    // …a user's "Scan now" retries it.
    let r = scan_at(&scn, lib, Priority::Interactive);
    assert_eq!((r.probe_retries, r.probe_failures), (1, 0), "{r:?}");
    let row = snapshot(&cat, root_id).into_values().next().unwrap();
    assert_eq!((row.state, row.probe_retry_at), (FileState::Present, None));
    let r = scan(&scn, lib);
    assert_eq!((r.writer_commands, r.media_files_opened), (0, 0), "{r:?}");
}

/// INT-7: overlapping roots in one library are refused.
#[test]
fn int7_nested_roots_in_one_library_are_refused() {
    if corpus().is_none() {
        return;
    }
    let s = Scratch::new("int7-nested");
    let root = s.path("Movies");
    clone_tree(&movie_dir(MATRIX), &root.join("Sub").join(MATRIX));
    let cat = open_catalog(&s.dir);
    let r = cat
        .writer()
        .create_library(CreateLibrary {
            name: "Movies".into(),
            kind: LibraryKind::Movies,
            roots: vec![
                root.to_string_lossy().into_owned(),
                root.join("Sub").to_string_lossy().into_owned(),
            ],
        })
        .wait();
    assert!(matches!(r, Err(CatalogError::OverlappingRoots { .. })), "{r:?}");
    let (lib, _) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    assert_eq!(scan(&sc(&cat, None), lib).added, 1);
}

/// INT-V1: a file rewritten in place (same inode and size, new mtime) and
/// renamed before the next scan keeps its identity and is re-probed.
#[test]
fn intv1_an_edited_then_renamed_file_is_reprobed() {
    if corpus().is_none() {
        return;
    }
    let s = Scratch::new("intv1-edit");
    let root = s.path("Movies");
    let heat = root.join(HEAT);
    cp_c(
        &movie_dir(HEAT).join("Heat (1995).avi"),
        &heat.join("Heat (1995).avi"),
    );
    let cat = open_catalog(&s.dir);
    let (lib, root_id) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let scn = sc(&cat, None);
    assert_eq!(scan(&scn, lib).added, 1);
    let id = snapshot(&cat, root_id)["Heat (1995)/Heat (1995).avi"].id;
    let f = heat.join("Heat (1995).avi");
    let other = movie_dir(F1917).join("1917 (2019) [tmdbid-908673].mkv");
    std::thread::sleep(Duration::from_millis(10));
    assert!(Command::new("dd")
        .arg(format!("if={}", other.display()))
        .arg(format!("of={}", f.display()))
        .args(["conv=notrunc", "status=none"])
        .status()
        .unwrap()
        .success());
    let g = heat.join("Heat (1995).mkv");
    std::fs::rename(&f, &g).unwrap();
    let r = scan(&scn, lib);
    assert_eq!(r.moved, 1, "{r:?}");
    assert_eq!(r.media_files_opened, 1, "the edited content is re-probed: {r:?}");
    let row = &snapshot(&cat, root_id)["Heat (1995)/Heat (1995).mkv"];
    assert_eq!(row.id, id, "same file");
    let container: String = cat
        .read_blocking(|c| Ok(c.query_row("SELECT container FROM asset", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(container, "matroska");
    let r = scan(&scn, lib);
    assert_eq!((r.writer_commands, r.media_files_opened), (0, 0), "{r:?}");
}

/// SEC-2 (scan side): links that leave the library's roots. A link to a
/// non-media file (a key, a token) is never cataloged; a link to media is,
/// under the link's path; an alias of a file inside the root is skipped
/// (the real path is cataloged once). All counted.
#[test]
fn sec2_links_leaving_the_roots_follow_the_policy() {
    if corpus().is_none() {
        return;
    }
    let s = Scratch::new("sec2-links");
    let outside = s.path("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(
        outside.join("id_ed25519"),
        b"-----BEGIN OPENSSH PRIVATE KEY-----\nstand-in\n",
    )
    .unwrap();
    cp_c(
        &movie_dir(HEAT).join("Heat (1995).avi"),
        &outside.join("Heat.avi"),
    );
    let root = s.path("Movies");
    clone_tree(&movie_dir(MATRIX), &root.join(MATRIX));
    for (link, target) in [
        ("Leak (2021)/Leak (2021).mkv", outside.join("id_ed25519")),
        ("Heat (1995)/Heat (1995).avi", outside.join("Heat.avi")),
        (
            "The Matrix Again (1999).avi",
            root.join(MATRIX).join("The Matrix (1999).avi"),
        ),
    ] {
        let l = root.join(link);
        std::fs::create_dir_all(l.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(target, l).unwrap();
    }
    let cat = open_catalog(&s.dir);
    let (lib, root_id) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let cfg = ScanConfig {
        ffprobe: PathBuf::from("/nonexistent/ffprobe"),
        ..config()
    };
    let opts = || ScannerOptions {
        startup: StartupCatchUp::Nothing,
        ..ScannerOptions::default()
    };
    let scn = scanner_with(&cat, cfg.clone(), opts());
    let r = scan(&scn, lib);
    assert_eq!(
        (r.added, r.links_external, r.links_skipped, r.external_not_media),
        (2, 2, 1, 1),
        "{r:?}"
    );
    let mut keys: Vec<String> = snapshot(&cat, root_id).into_keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "Heat (1995)/Heat (1995).avi".to_string(),
            format!("{MATRIX}/The Matrix (1999).avi")
        ]
    );
    // Nothing to do, and the rejected file is not opened again.
    let r = scan(&scn, lib);
    assert_eq!(
        (
            r.writer_commands,
            r.media_files_opened,
            r.links_external,
            r.external_not_media
        ),
        (0, 0, 2, 1),
        "{r:?}"
    );
    // Following links that leave the roots can be turned off.
    let strict = scanner_with(
        &cat,
        ScanConfig {
            follow_external_links: false,
            ..cfg
        },
        opts(),
    );
    let r = scan(&strict, lib);
    assert_eq!((r.missing, r.links_external, r.links_skipped), (1, 0, 3), "{r:?}");
}

fn fake_ffprobe(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("fake-ffprobe");
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

const FAKE_JSON: &str = r#"echo '{"streams":[{"index":0,"codec_type":"video","codec_name":"flv1","width":320,"height":240}],"format":{"format_name":"flv","duration":"12.5"}}'"#;

fn flv_library(name: &str) -> Option<(Scratch, PathBuf)> {
    let media = corpus()?;
    let s = Scratch::new(name);
    let root = s.path("Movies");
    cp_c(
        &media.join("probe-fixtures/flv_h264.flv"),
        &root.join("Flash Movie (2004)/Flash Movie (2004).flv"),
    );
    Some((s, root))
}

/// ROB-3: the ffprobe fallback runs the configured binary, not whatever
/// `ffprobe` is on `PATH`.
#[test]
fn rob3_the_fallback_runs_the_configured_ffprobe() {
    let Some((s, root)) = flv_library("rob3-ffprobe") else {
        return;
    };
    let cat = open_catalog(&s.dir);
    let (lib, _) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let cfg = ScanConfig {
        ffprobe: fake_ffprobe(&s.dir, FAKE_JSON),
        ..config()
    };
    let scn = scanner_with(&cat, cfg, ScannerOptions::default());
    let r = scan(&scn, lib);
    assert_eq!((r.ffprobe_fallbacks, r.probe_failures), (1, 0), "{r:?}");
    let ms: i64 = cat
        .read_blocking(|c| Ok(c.query_row("SELECT duration_ms FROM asset", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(ms, 12_500, "the fake ffprobe's answer");
}

/// ROB-V1: files whose fallback failed (ffprobe missing) heal once it
/// works: retried by the next user-requested scan.
#[test]
fn robv1_a_failed_fallback_heals_when_ffprobe_works() {
    let Some((s, root)) = flv_library("robv1-heal") else {
        return;
    };
    let cat = open_catalog(&s.dir);
    let (lib, root_id) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let broken = ScanConfig {
        ffprobe: PathBuf::from("/nonexistent/ffprobe"),
        ..config()
    };
    let r = scan(&scanner_with(&cat, broken, ScannerOptions::default()), lib);
    assert_eq!(r.probe_failures, 1, "{r:?}");
    assert_eq!(
        snapshot(&cat, root_id).into_values().next().unwrap().state,
        FileState::ProbeFailed
    );
    let fixed = ScanConfig {
        ffprobe: fake_ffprobe(&s.dir, FAKE_JSON),
        ..config()
    };
    let scn = scanner_with(&cat, fixed, ScannerOptions::default());
    let r = scan(&scn, lib);
    assert_eq!((r.probe_retries, r.ffprobe_fallbacks), (1, 1), "{r:?}");
    assert_eq!(
        snapshot(&cat, root_id).into_values().next().unwrap().state,
        FileState::Present
    );
}

/// ROB-4: shutdown honours its grace: an ffprobe hung on a file is killed
/// with its scan, not after its 20 s timeout.
#[test]
fn rob4_shutdown_kills_a_hung_ffprobe_within_the_grace() {
    let Some((s, root)) = flv_library("rob4-hang") else {
        return;
    };
    let marker = s.path("ffprobe-started");
    let fake = fake_ffprobe(&s.dir, &format!("touch '{}'\nexec sleep 3191", marker.display()));
    let cat = open_catalog(&s.dir);
    let (lib, _) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let scn = scanner_with(
        &cat,
        ScanConfig {
            ffprobe: fake,
            ffprobe_timeout: Duration::from_secs(20),
            ..config()
        },
        ScannerOptions::default(),
    );
    let h = scn.scan_library(lib, ScanMode::Quick, Priority::Interactive);
    wait_for("ffprobe to start", || marker.exists());
    let t = Instant::now();
    scn.shutdown(Duration::from_secs(1));
    let took = t.elapsed();
    assert!(took < Duration::from_secs(4), "shutdown took {took:?}");
    assert!(matches!(
        h.wait(),
        Err(ScanError::ShuttingDown | ScanError::Cancelled)
    ));
    let left = Command::new("pgrep").args(["-f", "sleep 3191"]).output().unwrap();
    assert!(left.stdout.is_empty(), "ffprobe child left running");
}

/// Deferred fingerprints: a file copied to another root (new inode) and
/// deleted from the first is cataloged as new, then merged into its
/// tombstone when the background pass fingerprints it.
#[test]
fn a_late_fingerprint_merges_a_cross_volume_style_move_into_its_tombstone() {
    let Some(media) = corpus() else { return };
    let s = Scratch::new("fp-late");
    let (a, b) = (s.path("A"), s.path("B"));
    clone_tree(&media.join("library-tiny/Movies").join(MATRIX), &a.join(MATRIX));
    std::fs::create_dir_all(&b).unwrap();
    let cat = open_catalog(&s.dir);
    let lib = cat
        .writer()
        .create_library(CreateLibrary {
            name: "Movies".into(),
            kind: LibraryKind::Movies,
            roots: vec![a.to_string_lossy().into_owned(), b.to_string_lossy().into_owned()],
        })
        .wait()
        .unwrap();
    let scn = sc(&cat, None);
    scan(&scn, lib.id);
    assert!(scn.wait_fingerprints(Duration::from_secs(60)));
    let rel = format!("{MATRIX}/The Matrix (1999).avi");
    let roots = cat.read_blocking(flux_catalog::scan_support::list_roots).unwrap();
    let (ra, rb) = (roots[0].id, roots[1].id);
    let old = snapshot(&cat, ra)[&rel].clone();
    // A plain copy (new inode, as across volumes), then delete.
    std::fs::create_dir_all(b.join(MATRIX)).unwrap();
    std::fs::write(b.join(&rel), std::fs::read(a.join(&rel)).unwrap()).unwrap();
    std::fs::remove_file(a.join(&rel)).unwrap();
    let r = scan(&scn, lib.id);
    assert_eq!((r.added, r.missing, r.fingerprints_deferred), (1, 1, 1), "{r:?}");
    assert!(scn.wait_fingerprints(Duration::from_secs(60)));
    let now = &snapshot(&cat, rb)[&rel];
    assert_eq!(now.id, old.id, "the tombstone took the new path");
    assert_eq!(now.asset_id, old.asset_id);
    assert!(snapshot(&cat, ra).is_empty());
    assert_eq!(
        works(&cat, lib.id),
        vec![("The Matrix".into(), "available".into(), 1)]
    );
}

/// Files cataloged without a fingerprint (fingerprinting was off, or the
/// process stopped before the background pass) get one from the next scan,
/// which still opens and writes nothing itself.
#[test]
fn rows_without_a_fingerprint_are_fingerprinted_by_the_next_scan() {
    if corpus().is_none() {
        return;
    }
    let s = Scratch::new("fp-backfill");
    let root = s.path("Movies");
    clone_tree(&movie_dir(MATRIX), &root.join(MATRIX));
    clone_tree(&movie_dir(ALIEN), &root.join(ALIEN));
    let cat = open_catalog(&s.dir);
    let (lib, root_id) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let off = ScanConfig {
        fingerprint: false,
        ..config()
    };
    let r = scan(&scanner_with(&cat, off, ScannerOptions::default()), lib);
    assert_eq!(r.fingerprints_deferred, 0);
    assert!(snapshot(&cat, root_id).values().all(|r| r.fp128.is_none()));
    let scn = sc(&cat, None);
    let r = scan(&scn, lib);
    assert_eq!(
        (r.writer_commands, r.media_files_opened, r.fingerprints_deferred),
        (0, 0, 2),
        "{r:?}"
    );
    assert!(scn.wait_fingerprints(Duration::from_secs(60)));
    assert!(snapshot(&cat, root_id).values().all(|r| r.fp128.is_some()));
    let r = scan(&scn, lib);
    assert_eq!((r.writer_commands, r.fingerprints_deferred), (0, 0), "{r:?}");
}

/// RootStatus.last_seen: a scan that changes nothing writes nothing, so the
/// live time comes from the scanner.
#[test]
fn root_last_seen_moves_with_scans_that_write_nothing() {
    if corpus().is_none() {
        return;
    }
    let s = Scratch::new("last-seen");
    let root = s.path("Movies");
    clone_tree(&movie_dir(MATRIX), &root.join(MATRIX));
    let cat = open_catalog(&s.dir);
    let (lib, _) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let scn = sc(&cat, None);
    assert!(scn.root_last_seen(lib).is_empty());
    scan(&scn, lib);
    let t1 = scn.root_last_seen(lib)[0].1;
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(scan(&scn, lib).writer_commands, 0);
    let seen = scn.root_last_seen(lib);
    assert_eq!(seen[0].0, root.to_string_lossy());
    assert!(seen[0].1 > t1);
    let mut l = cat
        .read_blocking(move |c| flux_catalog::read::library(c, lib))
        .unwrap()
        .unwrap();
    scn.overlay_roots(&mut l);
    assert_eq!(l.roots[0].last_seen, Some(seen[0].1));
}

/// Footprint: after a scan that wrote, the catalog writer is back to its
/// idle cache and the WAL file is truncated.
#[test]
fn a_writing_scan_leaves_the_wal_truncated() {
    if corpus().is_none() {
        return;
    }
    let s = Scratch::new("wal");
    let root = s.path("Movies");
    for m in [MATRIX, ALIEN, F1917] {
        clone_tree(&movie_dir(m), &root.join(m));
    }
    let cat = open_catalog(&s.dir);
    let (lib, _) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let scn = scanner_with(
        &cat,
        ScanConfig {
            fingerprint: false,
            ..config()
        },
        ScannerOptions::default(),
    );
    assert_eq!(scan(&scn, lib).added, 3);
    let wal = format!("{}-wal", cat.path().display());
    assert_eq!(
        std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0),
        0,
        "WAL truncated after the scan"
    );
}
