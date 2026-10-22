//! Watcher and startup catch-up on scratch clones of library-tiny.

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use flux_catalog::FileState;
use flux_core::LibraryKind;
use flux_scan::{ScanError, ScannerOptions, StartupCatchUp};
use support::*;

const SHOW: &str = "Breaking Bad (2008)";

/// One show of library-tiny's TV folder, cloned.
fn show_clone(name: &str) -> Option<(Scratch, PathBuf)> {
    let media = corpus()?;
    let scratch = Scratch::new(name);
    let root = scratch.path("TV");
    std::fs::create_dir_all(&root).unwrap();
    clone_tree(&media.join("library-tiny/TV").join(SHOW), &root.join(SHOW));
    Some((scratch, root))
}

fn first_episode(dir: &Path) -> PathBuf {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| flux_core::is_video_extension(p.extension().and_then(|e| e.to_str()).unwrap_or("")))
        .collect();
    v.sort();
    v.remove(0)
}

fn state_of(cat: &flux_catalog::Catalog, root_id: i64, rel: &str) -> Option<FileState> {
    snapshot(cat, root_id).get(rel).map(|r| r.state)
}

#[test]
fn watcher_picks_up_added_and_deleted_files() {
    let Some((scratch, root)) = show_clone("watch") else {
        return;
    };
    let cat = open_catalog(&scratch.dir);
    let (lib, root_id) = add_library(&cat, "TV", LibraryKind::Shows, &root);
    let s = scanner(&cat);
    scan(&s, lib);
    s.watch(lib).unwrap();
    s.watch(lib).unwrap(); // idempotent
    assert_eq!(s.status(lib).state, flux_core::api::ScanState::Watching);
    std::thread::sleep(Duration::from_millis(300)); // FSEvents stream up

    let season = root.join(SHOW).join("S02");
    let seed = first_episode(&season);
    let ext = seed.extension().unwrap().to_string_lossy().into_owned();
    let new_rel = format!("{SHOW}/S02/Breaking Bad 2x99.{ext}");
    clone_tree(&seed, &root.join(&new_rel));
    let (_, latency) = eventually(Duration::from_secs(20), || {
        (state_of(&cat, root_id, &new_rel) == Some(FileState::Present)).then_some(())
    })
    .expect("new episode visible in the catalog");
    eprintln!("watcher add latency: {latency:?}");

    std::fs::remove_file(root.join(&new_rel)).unwrap();
    eventually(Duration::from_secs(20), || {
        (state_of(&cat, root_id, &new_rel) == Some(FileState::Missing)).then_some(())
    })
    .expect("deleted episode tombstoned");

    s.unwatch(lib);
    assert!(matches!(s.watch(424242), Err(ScanError::UnknownLibrary(424242))));
    s.shutdown(Duration::from_secs(5));
}

#[test]
fn restart_catches_up_from_the_mille_resume_snapshot() {
    let Some((scratch, root)) = show_clone("resume") else {
        return;
    };
    let cat = open_catalog(&scratch.dir);
    let (lib, root_id) = add_library(&cat, "TV", LibraryKind::Shows, &root);
    let opts = || ScannerOptions {
        startup: StartupCatchUp::MilleResume,
        ..ScannerOptions::default()
    };
    let total = {
        let s = scanner_with(&cat, config(), opts());
        let r = scan(&s, lib);
        s.shutdown(Duration::from_secs(5));
        r.media_files
    };
    let snap_file = PathBuf::from(format!("{}.scan-resume-root-{root_id}.bin", cat.path().display()));
    assert!(
        snap_file.exists(),
        "resume snapshot written after a complete scan"
    );

    // While flux is down: one episode added, one deleted.
    let season = root.join(SHOW).join("Season 01");
    let victim = first_episode(&season);
    let victim_rel = format!(
        "{SHOW}/Season 01/{}",
        victim.file_name().unwrap().to_string_lossy()
    );
    std::fs::remove_file(&victim).unwrap();
    let ext = victim.extension().unwrap().to_string_lossy().into_owned();
    let added_rel = format!("{SHOW}/S02/Breaking Bad 2x98.{ext}");
    clone_tree(
        &first_episode(&root.join(SHOW).join("S02")),
        &root.join(&added_rel),
    );

    let s = scanner_with(&cat, config(), opts());
    s.watch(lib).unwrap();
    eventually(Duration::from_secs(30), || {
        (state_of(&cat, root_id, &added_rel) == Some(FileState::Present)
            && state_of(&cat, root_id, &victim_rel) == Some(FileState::Missing))
        .then_some(())
    })
    .expect("catch-up applied both changes");
    // Only the two changed directories were rescanned, not the library.
    let st = eventually(Duration::from_secs(10), || {
        let st = s.status(lib);
        (st.state != flux_core::api::ScanState::Scanning).then_some(st)
    })
    .unwrap()
    .0;
    let walked = st.files_total.unwrap_or(0);
    assert!(walked < total, "catch-up walked {walked} of {total} files");
    s.shutdown(Duration::from_secs(5));
}

#[test]
fn restart_catches_up_from_fsevents_history_alone() {
    let Some((scratch, root)) = show_clone("history") else {
        return;
    };
    let cat = open_catalog(&scratch.dir);
    let (lib, root_id) = add_library(&cat, "TV", LibraryKind::Shows, &root);
    let opts = || ScannerOptions {
        startup: StartupCatchUp::MilleResume,
        ..ScannerOptions::default()
    };
    let total = {
        let s = scanner_with(&cat, config(), opts());
        let r = scan(&s, lib);
        s.shutdown(Duration::from_secs(5));
        r.media_files
    };
    let snap_file = PathBuf::from(format!("{}.scan-resume-root-{root_id}.bin", cat.path().display()));
    assert!(
        snap_file.exists(),
        "resume snapshot written after a complete scan"
    );
    let token_file = PathBuf::from(format!("{}.history", snap_file.display()));
    assert!(token_file.exists(), "history token written after a complete scan");
    // Only the FSEvents history can answer now.
    std::fs::remove_file(&snap_file).unwrap();

    // While flux is down: one episode added, one deleted.
    let season = root.join(SHOW).join("Season 01");
    let victim = first_episode(&season);
    let victim_rel = format!(
        "{SHOW}/Season 01/{}",
        victim.file_name().unwrap().to_string_lossy()
    );
    std::fs::remove_file(&victim).unwrap();
    let ext = victim.extension().unwrap().to_string_lossy().into_owned();
    let added_rel = format!("{SHOW}/S02/Breaking Bad 2x98.{ext}");
    clone_tree(
        &first_episode(&root.join(SHOW).join("S02")),
        &root.join(&added_rel),
    );

    let s = scanner_with(&cat, config(), opts());
    s.watch(lib).unwrap();
    eventually(Duration::from_secs(30), || {
        (state_of(&cat, root_id, &added_rel) == Some(FileState::Present)
            && state_of(&cat, root_id, &victim_rel) == Some(FileState::Missing))
        .then_some(())
    })
    .expect("catch-up applied both changes");
    // Only the two changed directories were rescanned, not the library.
    let st = eventually(Duration::from_secs(10), || {
        let st = s.status(lib);
        (st.state != flux_core::api::ScanState::Scanning).then_some(st)
    })
    .unwrap()
    .0;
    let walked = st.files_total.unwrap_or(0);
    assert!(walked < total, "catch-up walked {walked} of {total} files");
    eprintln!("history catch-up walked {walked} of {total} media files");
    // The next token was saved after the catch-up.
    let saved = mille_core::HistoryToken::read(&token_file).unwrap();
    eprintln!("token event id now {}", saved.event_id);
    s.shutdown(Duration::from_secs(5));
}

#[test]
fn restart_without_snapshot_runs_a_quick_scan() {
    let Some((scratch, root)) = show_clone("quickstart") else {
        return;
    };
    let cat = open_catalog(&scratch.dir);
    let (lib, root_id) = add_library(&cat, "TV", LibraryKind::Shows, &root);
    scan(&scanner(&cat), lib);
    let added_rel = format!("{SHOW}/S02/Breaking Bad 2x97.mp4");
    clone_tree(
        &first_episode(&root.join(SHOW).join("S02")),
        &root.join(&added_rel),
    );
    let s = scanner_with(
        &cat,
        config(),
        ScannerOptions {
            startup: StartupCatchUp::QuickScan,
            ..ScannerOptions::default()
        },
    );
    s.watch(lib).unwrap();
    eventually(Duration::from_secs(30), || {
        (state_of(&cat, root_id, &added_rel) == Some(FileState::Present)).then_some(())
    })
    .expect("quick scan at startup found the new file");
    s.shutdown(Duration::from_secs(5));
}

#[test]
fn a_root_that_returns_is_watched_again() {
    let Some((scratch, root)) = show_clone("rewatch") else {
        return;
    };
    let cat = open_catalog(&scratch.dir);
    let (lib, root_id) = add_library(&cat, "TV", LibraryKind::Shows, &root);
    let s = scanner(&cat);
    scan(&s, lib);
    // The drive is gone when watching starts…
    let away = scratch.path("TV.away");
    std::fs::rename(&root, &away).unwrap();
    s.watch(lib).unwrap();
    // …and comes back; the next scan sees it and starts watching it.
    std::fs::rename(&away, &root).unwrap();
    let r = scan(&s, lib);
    assert!(r.unavailable_roots.is_empty());
    std::thread::sleep(Duration::from_millis(300));
    let new_rel = format!("{SHOW}/S02/Breaking Bad 2x96.mp4");
    clone_tree(&first_episode(&root.join(SHOW).join("S02")), &root.join(&new_rel));
    eventually(Duration::from_secs(20), || {
        (state_of(&cat, root_id, &new_rel) == Some(FileState::Present)).then_some(())
    })
    .expect("events from the returned root reach the catalog");
    s.shutdown(Duration::from_secs(5));
}

/// A watched root renamed away and back comes back by itself. The scan that
/// ran while it was gone marked it unavailable, and nothing says it is
/// back: on Windows the watch follows the renamed folder and renaming it
/// back raises no event, so it used to stay unavailable until "Scan now".
#[test]
fn an_unavailable_root_that_answers_again_comes_back_by_itself() {
    use flux_catalog::{scan_support, RootState};
    let scratch = Scratch::new("recheck");
    let root = scratch.path("Movies");
    std::fs::create_dir_all(root.join("Film (2020)")).unwrap();
    std::fs::write(root.join("Film (2020)/Film (2020).mkv"), b"not really a movie").unwrap();
    let cat = open_catalog(&scratch.dir);
    let (lib, root_id) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let s = scanner_with(
        &cat,
        config(),
        ScannerOptions {
            startup: StartupCatchUp::Nothing,
            recheck_unavailable: Duration::from_millis(200),
            ..ScannerOptions::default()
        },
    );
    let state = || {
        cat.read_blocking(scan_support::list_roots)
            .unwrap()
            .into_iter()
            .find(|r| r.id == root_id)
            .unwrap()
            .state
    };
    scan(&s, lib);
    s.watch(lib).unwrap();
    assert_eq!(state(), RootState::Available);

    let away = scratch.path("Movies (renamed)");
    std::fs::rename(&root, &away).unwrap();
    let r = scan(&s, lib);
    assert_eq!(r.unavailable_roots.len(), 1, "{r:?}");
    assert_eq!(state(), RootState::Unavailable);
    // Still gone: the recheck leaves it alone.
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(state(), RootState::Unavailable);

    std::fs::rename(&away, &root).unwrap();
    let (_, after) = eventually(Duration::from_secs(10), || {
        (state() == RootState::Available).then_some(())
    })
    .expect("the root came back without a manual scan");
    eprintln!("root back after {after:?}");
    s.shutdown(Duration::from_secs(5));
}
