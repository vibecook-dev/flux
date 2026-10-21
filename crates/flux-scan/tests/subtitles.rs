//! Subtitle files beside the videos: the scan records them, keeps them
//! current on full and directory rescans (what the watcher runs), writes
//! nothing when nothing changed, and the catalog matches them to assets by
//! name. Runs on a synthetic clip (`flux_test_media::clip`), so CI runs it
//! everywhere ffmpeg with libx264 is installed; skips otherwise.

mod support;

use std::path::Path;
use std::time::Duration;

use flux_catalog::sidecar;
use flux_core::api::Id;
use flux_core::{Codec, LibraryKind, SubtitleFile, SUBTITLE_FILE_TRACK_BASE};
use flux_scan::Priority;
use flux_test_media::{clip, Clip};
use support::*;

fn put(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

fn put_video(root: &Path, rel: &str, src: &Path) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    flux_test_media::clone_file(src, &p).unwrap();
}

/// The subtitle files the catalog has for a root, by relative path.
fn rows(cat: &flux_catalog::Catalog, root_id: Id) -> Vec<String> {
    let mut v: Vec<String> = cat
        .read_blocking(move |c| sidecar::load_snapshot(c, root_id, &[String::new()]))
        .unwrap()
        .into_keys()
        .collect();
    v.sort();
    v
}

/// The subtitle files matched to the asset of the video at `rel`.
fn files_of(cat: &flux_catalog::Catalog, rel: &'static str) -> Vec<SubtitleFile> {
    cat.read_blocking(move |c| {
        let asset: Id = c.query_row(
            "SELECT a.id FROM asset a JOIN file f ON f.id = a.file_id WHERE f.rel_path = ?1",
            [rel],
            |r| r.get(0),
        )?;
        sidecar::asset_subtitle_files(c, asset)
    })
    .unwrap()
}

fn names(files: &[SubtitleFile]) -> Vec<String> {
    files
        .iter()
        .map(|f| {
            Path::new(&f.path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

const MP4: &str = "示例剧/Some.Show.S01E01.1080p.中英字幕.某字幕组.mp4";
const RAW: &str = "示例剧/另一版本/Some.Show.S01E01.1080p.WEB.h264-GRP.mp4";
const SUP: &str = "示例剧/另一版本/Some.Show.S01E01.1080p.中英字幕.某字幕组.sup";
const SRT: &str = "示例剧/另一版本/Some.Show.S01E01.1080p.WEB.h264-GRP.en.srt";
const ASS: &str = "示例剧/另一版本/Subs/Some.Show.S01E01.chs.ass";

#[test]
fn subtitle_files_are_cataloged_kept_current_and_matched_by_name() {
    let Some(src) = clip(Clip::H264AacMp4) else {
        return;
    };
    let scratch = Scratch::new("subtitle-files");
    let root = scratch.path("Shows");
    // A show folder with an MP4 per episode, and a subfolder of other
    // versions whose `.sup` carries the MP4's name.
    put_video(&root, MP4, &src);
    put_video(&root, RAW, &src);
    put(&root, SUP, b"PG not really pgs");
    put(&root, SRT, b"1\n00:00:01,000 --> 00:00:02,000\nHello\n");
    put(&root, "示例剧/notes.txt", b"not a subtitle");
    let cat = open_catalog(&scratch.dir);
    let s = scanner(&cat);
    let (lib, root_id) = add_library(&cat, "Shows", LibraryKind::Shows, &root);
    scan(&s, lib);
    assert_eq!(rows(&cat, root_id), vec![SRT.to_string(), SUP.to_string()]);

    // The version beside them takes both; the MP4 above takes neither.
    let raw = files_of(&cat, RAW);
    assert_eq!(
        names(&raw),
        vec![
            "Some.Show.S01E01.1080p.WEB.h264-GRP.en.srt",
            "Some.Show.S01E01.1080p.中英字幕.某字幕组.sup"
        ]
    );
    let srt = &raw[0];
    assert_eq!((srt.codec, srt.language.as_deref()), (Codec::Subrip, Some("eng")));
    let sup = &raw[1];
    assert_eq!(sup.codec, Codec::Pgs);
    assert_eq!(sup.language.as_deref(), Some("chi"));
    assert_eq!(sup.title.as_deref(), Some("中英字幕 某字幕组"));
    assert!(raw.iter().all(|f| f.track >= SUBTITLE_FILE_TRACK_BASE));
    assert!(files_of(&cat, MP4).is_empty());

    // Unchanged: a rescan writes nothing.
    assert!(s.wait_fingerprints(Duration::from_secs(120)));
    let before = writer_commands(&cat);
    scan(&s, lib);
    assert_eq!(writer_commands(&cat), before, "an unchanged rescan wrote");

    // One removed, one added in a Subs folder: a directory rescan (what
    // the watcher runs) keeps the list current, and track numbers of the
    // files that stayed don't change.
    std::fs::remove_file(root.join(SRT)).unwrap();
    put(&root, ASS, b"[Events]\n");
    s.scan_dirs(lib, vec![root.join("示例剧/另一版本")], Priority::Interactive)
        .wait()
        .unwrap();
    assert_eq!(rows(&cat, root_id), vec![SUP.to_string(), ASS.to_string()]);
    let raw2 = files_of(&cat, RAW);
    assert_eq!(
        names(&raw2),
        vec![
            "Some.Show.S01E01.1080p.中英字幕.某字幕组.sup",
            "Some.Show.S01E01.chs.ass"
        ]
    );
    assert_eq!(raw2[0].track, sup.track);
    assert_eq!(raw2[1].language.as_deref(), Some("zh-Hans"));

    // The whole folder gone: its subtitle files go with it.
    std::fs::remove_dir_all(root.join("示例剧/另一版本")).unwrap();
    scan(&s, lib);
    assert!(rows(&cat, root_id).is_empty());
}
