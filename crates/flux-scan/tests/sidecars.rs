//! Metadata files beside the videos (`.nfo`, `.flux.json`) through a real
//! scan: read once, kept current, nothing written when nothing changed, and
//! what they say applied to the items they describe. Runs on a synthetic
//! clip (`flux_test_media::clip`); skips where ffmpeg can't make one.

mod support;

use std::path::Path;
use std::time::Duration;

use flux_catalog::{fields, metafiles};
use flux_core::api::Id;
use flux_core::LibraryKind;
use flux_test_media::{clip, Clip};
use serde_json::{json, Value};
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

/// The item of the video at `rel`, and one of its fields.
fn field(cat: &flux_catalog::Catalog, rel: &'static str, f: &'static str) -> (Id, Option<Value>) {
    cat.read_blocking(move |c| {
        let item: Id = c.query_row(
            "SELECT a.item_id FROM asset a JOIN file f ON f.id = a.file_id WHERE f.rel_path = ?1",
            [rel],
            |r| r.get(0),
        )?;
        Ok((item, fields::read(c, item)?.unwrap().values.get(f).cloned()))
    })
    .unwrap()
}

const VIDEO: &str = "Heat (1995)/Heat (1995).mp4";
const NFO: &str = "Heat (1995)/Heat (1995).nfo";

#[test]
fn metadata_files_are_read_kept_current_and_applied() {
    let Some(src) = clip(Clip::H264AacMp4) else {
        return;
    };
    let scratch = Scratch::new("metadata-files");
    let root = scratch.path("Films");
    put_video(&root, VIDEO, &src);
    put(
        &root,
        NFO,
        b"<movie><title>Heat</title><director>Michael Mann</director></movie>",
    );
    let cat = open_catalog(&scratch.dir);
    let s = scanner(&cat);
    let (lib, root_id) = add_library(&cat, "Films", LibraryKind::Movies, &root);
    scan(&s, lib);
    let (item, directors) = field(&cat, VIDEO, "directors");
    assert_eq!(directors, Some(json!(["Michael Mann"])));
    let snap = cat
        .read_blocking(move |c| metafiles::load_snapshot(c, root_id, &[String::new()]))
        .unwrap();
    assert_eq!(snap.keys().collect::<Vec<_>>(), [NFO]);

    // Unchanged: a rescan writes nothing.
    assert!(s.wait_fingerprints(Duration::from_secs(120)));
    let before = writer_commands(&cat);
    scan(&s, lib);
    assert_eq!(writer_commands(&cat), before, "an unchanged rescan wrote");

    // Changed: read again.
    std::thread::sleep(Duration::from_millis(20));
    put(
        &root,
        NFO,
        b"<movie><title>Heat</title><director>Michael Mann</director><tagline>A Los Angeles crime saga</tagline></movie>",
    );
    scan(&s, lib);
    assert_eq!(
        field(&cat, VIDEO, "tagline").1,
        Some(json!("A Los Angeles crime saga"))
    );

    // A flux sidecar named like the video: it wins over the NFO.
    put(
        &root,
        "Heat (1995)/Heat (1995).flux.json",
        br#"{"flux": "flux.sidecar/1", "fields": {"tagline": "A crime saga", "studios": ["Forward Pass"]}}"#,
    );
    scan(&s, lib);
    assert_eq!(field(&cat, VIDEO, "tagline").1, Some(json!("A crime saga")));
    assert_eq!(field(&cat, VIDEO, "studios").1, Some(json!(["Forward Pass"])));
    assert_eq!(field(&cat, VIDEO, "title").0, item, "the same film");

    // Gone: what they gave is flux's again.
    std::fs::remove_file(root.join(NFO)).unwrap();
    std::fs::remove_file(root.join("Heat (1995)/Heat (1995).flux.json")).unwrap();
    scan(&s, lib);
    let src_of = cat
        .read_blocking(move |c| fields::source_of(c, item, "directors"))
        .unwrap();
    assert!(src_of.is_none(), "{src_of:?}");
}
