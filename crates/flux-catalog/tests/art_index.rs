//! Art selection (provider > local > frame > generated unless pinned) and
//! the playback-index cache (opaque bytes, invalidated by size/mtime, served
//! with the asset's current path).

mod common;

use common::*;
use flux_catalog::{read, ArtKind, FileMove, NewArt, PlayIndex, ScanBatch, INDEX_FORMAT_JSON};
use flux_core::api::ArtSource;
use flux_core::LibraryKind;

fn art(source: ArtSource, url: &str) -> NewArt {
    NewArt {
        kind: ArtKind::Poster,
        source,
        path: (source != ArtSource::Provider).then(|| format!("/cache/{url}.jpg")),
        url: (source == ArtSource::Provider).then(|| format!("https://image.tmdb.org/t/p/{url}.jpg")),
        width: Some(600),
        height: Some(900),
        blurhash: None,
        attribution: (source == ArtSource::Provider).then(|| "TMDB".to_string()),
        generator: (source == ArtSource::Generated).then(|| "openai/gpt-image-1".to_string()),
        gen_version: None,
        gen_title: None,
    }
}

#[test]
fn best_art_prefers_provider_then_local_frame_generated_unless_pinned() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![upsert("Heat (1995).mkv", movie("Heat", Some(1995)), None)]);
    let heat = e.find_top("Heat").id;
    let w = e.cat.writer();
    let best = || {
        e.cat
            .read_blocking(move |c| read::best_art(c, heat, ArtKind::Poster))
            .unwrap()
            .map(|a| (a.source, a.attribution, a.generator))
    };
    assert_eq!(best(), None);
    let gen = w.add_art(heat, art(ArtSource::Generated, "g")).wait().unwrap();
    assert_eq!(best().unwrap().0, ArtSource::Generated);
    assert_eq!(best().unwrap().2.as_deref(), Some("openai/gpt-image-1"));
    w.add_art(heat, art(ArtSource::Frame, "f")).wait().unwrap();
    assert_eq!(best().unwrap().0, ArtSource::Frame);
    w.add_art(heat, art(ArtSource::Local, "l")).wait().unwrap();
    assert_eq!(best().unwrap().0, ArtSource::Local);
    let prov = w.add_art(heat, art(ArtSource::Provider, "p")).wait().unwrap();
    assert_eq!(best().unwrap(), (ArtSource::Provider, Some("TMDB".into()), None));
    // Adding a lesser image later doesn't displace the provider poster.
    w.add_art(heat, art(ArtSource::Frame, "f2")).wait().unwrap();
    assert_eq!(best().unwrap().0, ArtSource::Provider);
    // Re-adding the same image is idempotent.
    assert_eq!(
        w.add_art(heat, art(ArtSource::Provider, "p")).wait().unwrap(),
        prov
    );
    let all = e.cat.read_blocking(move |c| read::item_art(c, heat)).unwrap();
    assert_eq!(all.len(), 5);
    assert_eq!(all[0].id, prov, "best first");
    // The user pins the generated one: it wins until unpinned.
    assert!(w.pin_art(gen).wait().unwrap());
    assert_eq!(best().unwrap().0, ArtSource::Generated);
    let summary = e.find_top("Heat");
    assert_eq!(summary.poster.unwrap().url, format!("/api/v1/art/{gen}"));
    assert!(
        e.cat
            .read_blocking(move |c| read::art(c, gen))
            .unwrap()
            .unwrap()
            .pinned
    );
    // Pinning another moves the pin.
    let local = all.iter().find(|a| a.source == ArtSource::Local).unwrap().id;
    w.pin_art(local).wait().unwrap();
    assert_eq!(best().unwrap().0, ArtSource::Local);
    assert_eq!(w.unpin_art(heat, ArtKind::Poster).wait().unwrap(), 1);
    assert_eq!(best().unwrap().0, ArtSource::Provider);
    assert!(!w.pin_art(999_999).wait().unwrap());
    assert!(w.remove_art(prov).wait().unwrap());
    assert_eq!(best().unwrap().0, ArtSource::Local);
}

#[test]
fn play_index_is_served_while_the_file_is_unchanged_with_its_current_path() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![upsert(
        "Heat (1995)/Heat.mkv",
        movie("Heat", Some(1995)),
        Some(seed("h264_mkv")),
    )]);
    let heat = e.find_top("Heat").id;
    let asset = e.detail(heat).assets[0].id;
    let file = e.file_id("Heat (1995)/Heat.mkv");
    let (size, mtime) = {
        let root = e.root;
        e.cat
            .read_blocking(move |c| {
                let s = flux_catalog::scan_support::load_root_snapshot(c, root)?;
                let r = &s["Heat (1995)/Heat.mkv"];
                Ok((r.size, r.mtime_ns))
            })
            .unwrap()
    };
    let w = e.cat.writer();
    let get = || {
        e.cat
            .read_blocking(move |c| read::asset_play_index(c, asset))
            .unwrap()
    };
    assert!(get().is_none());
    let idx = |stamp: i64, bytes: &[u8]| PlayIndex {
        format: "fxi1".into(),
        version: 1,
        source_size: size,
        source_mtime_ns: stamp,
        bytes: bytes.to_vec(),
    };
    // Built from another stat (the file changed after the scan): not served.
    w.put_asset_index(asset, idx(mtime + 1, b"stale")).wait().unwrap();
    assert!(get().is_none());
    w.put_asset_index(asset, idx(mtime, b"\x00binary\xff"))
        .wait()
        .unwrap();
    let got = get().unwrap();
    assert_eq!((got.format.as_str(), got.version), ("fxi1", 1));
    assert_eq!(got.bytes, b"\x00binary\xff");
    assert_eq!(got.path, "/media/lib/Heat (1995)/Heat.mkv");
    assert!(got.available);
    // Renamed: same asset, same index, the new path.
    let mut to = facts("Heat (1995)/Heat (1995) - 1080p.mkv");
    to.size = size;
    to.mtime_ns = mtime;
    e.batch(ScanBatch {
        root_id: e.root,
        moves: vec![FileMove {
            file_id: file,
            to,
            guess: Some(movie("Heat", Some(1995))),
        }],
        ..Default::default()
    });
    let got = get().unwrap();
    assert_eq!(got.path, "/media/lib/Heat (1995)/Heat (1995) - 1080p.mkv");
    // The file is rewritten (new size/mtime): the cached row is dropped.
    let mut changed = upsert(
        "Heat (1995)/Heat (1995) - 1080p.mkv",
        movie("Heat", Some(1995)),
        Some(seed("h264_mkv")),
    );
    changed.facts.size = size + 10;
    e.apply(vec![changed]);
    assert!(get().is_none());
    let rows: i64 = e
        .cat
        .read_blocking(|c| Ok(c.query_row("SELECT count(*) FROM asset_index", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(rows, 0, "stale bytes are deleted, not just hidden");
    // Unknown asset; explicit clear.
    assert!(w.put_asset_index(999_999, idx(mtime, b"x")).wait().is_err());
    let size2 = size + 10;
    let mut fresh = idx(0, b"{}");
    fresh.format = INDEX_FORMAT_JSON.into();
    fresh.source_size = size2;
    fresh.source_mtime_ns = e
        .cat
        .read_blocking({
            let root = e.root;
            move |c| {
                Ok(flux_catalog::scan_support::load_root_snapshot(c, root)?
                    ["Heat (1995)/Heat (1995) - 1080p.mkv"]
                    .mtime_ns)
            }
        })
        .unwrap();
    w.put_asset_index(asset, fresh).wait().unwrap();
    assert_eq!(get().unwrap().format, "json");
    assert!(w.clear_asset_index(asset).wait().unwrap());
    assert!(get().is_none());
}

#[test]
fn show_episodes_art_lists_episodes_in_order_with_their_thumbs() {
    let e = env(LibraryKind::Shows);
    e.apply(vec![
        upsert(
            "Show/Season 2/Show - S02E01.mkv",
            episode("Show", None, 2, &[1]),
            None,
        ),
        upsert(
            "Show/Season 1/Show - S01E02.mkv",
            episode("Show", None, 1, &[2]),
            None,
        ),
        upsert(
            "Show/Season 1/Show - S01E01.mkv",
            episode("Show", None, 1, &[1]),
            None,
        ),
        upsert(
            "Show/Specials/Show - S00E01.mkv",
            episode("Show", None, 0, &[1]),
            None,
        ),
    ]);
    let show = e.find_top("Show").id;
    let lib = e.cat.read_blocking(move |c| read::item_library(c, show)).unwrap();
    assert!(lib.is_some());
    assert_eq!(
        e.cat.read_blocking(|c| read::item_library(c, 999_999)).unwrap(),
        None
    );
    let eps = || {
        e.cat
            .read_blocking(move |c| read::show_episodes_art(c, show, ArtKind::Thumb))
            .unwrap()
    };
    let list = eps();
    assert_eq!(list.len(), 4);
    assert!(list.iter().all(|(_, has)| !has));
    // Viewing order: S01E01, S01E02, S02E01, then specials.
    let numbers: Vec<(u16, u16)> = list
        .iter()
        .map(|(id, _)| {
            let d = e.detail(*id);
            (
                d.season_number.unwrap_or(0),
                d.episode_numbers.first().copied().unwrap_or(0),
            )
        })
        .collect();
    assert_eq!(numbers, [(1, 1), (1, 2), (2, 1), (0, 1)]);
    let mut thumb = art(ArtSource::Frame, "t");
    thumb.kind = ArtKind::Thumb;
    e.cat.writer().add_art(list[1].0, thumb).wait().unwrap();
    let after = eps();
    assert_eq!(after.iter().filter(|(_, has)| *has).count(), 1);
    assert!(after[1].1);
}
