//! The startup scan after an upgrade: the server starts the watcher
//! without its catch-up, then a Deep scan of the library. Started while
//! that scan ran, the catch-up joined it as a second pass whose counts
//! added to the summary (every file, and every hidden file's log line,
//! twice). Runs on a synthetic clip (`flux_test_media::clip`); skips
//! without ffmpeg.

mod support;

use std::path::Path;
use std::time::Duration;

use flux_core::api::ScanState;
use flux_core::LibraryKind;
use flux_scan::{Priority, ScanMode, ScannerOptions, StartupCatchUp};
use flux_test_media::{clip, Clip};
use support::*;

fn put_video(root: &Path, rel: &str, src: &Path) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    flux_test_media::clone_file(src, &p).unwrap();
}

#[test]
fn a_deep_scan_after_watching_without_catch_up_counts_each_file_once() {
    let Some(src) = clip(Clip::H264AacMp4) else {
        return;
    };
    let scratch = Scratch::new("deep-after-watch");
    let root = scratch.path("Movies");
    put_video(&root, "Copper Sky (2019)/Copper Sky (2019).mp4", &src);
    put_video(&root, "Tide Line (2020)/Tide Line (2020).mp4", &src);
    put_video(&root, "Tide Line (2020)/.Tide Line (2020) alt.mp4", &src);
    let other = scratch.path("Other");
    put_video(&other, "Copper Sky (2019)/Copper Sky (2019).mp4", &src);
    let cat = open_catalog(&scratch.dir);
    let (lib, _) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let (control, _) = add_library(&cat, "Other", LibraryKind::Movies, &other);
    let s = scanner_with(
        &cat,
        config(),
        ScannerOptions {
            startup: StartupCatchUp::QuickScan,
            ..ScannerOptions::default()
        },
    );
    s.watch_without_catch_up(lib).unwrap();
    s.watch(control).unwrap();
    // The control's catch-up scans it; nothing scans the library.
    eventually(Duration::from_secs(30), || s.status(control).started_at)
        .expect("the control library's catch-up scan");
    std::thread::sleep(Duration::from_millis(300));
    let idle = s.status(lib);
    assert_eq!(
        (idle.state, idle.started_at),
        (ScanState::Watching, None),
        "{idle:?}"
    );

    let r = s
        .scan_library(lib, ScanMode::Deep, Priority::Scheduled)
        .wait()
        .unwrap();
    assert_eq!(
        (r.files_seen, r.media_files, r.hidden_skipped, r.follow_up_passes),
        (2, 2, 1, 0),
        "{r:?}"
    );
    s.shutdown(Duration::from_secs(5));
}
