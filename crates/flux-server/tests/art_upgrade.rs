//! Art across upgrades: images keep serving when the data dir moves (they
//! are stored relative to it; an older catalog's absolute paths are read
//! from the new data dir), and frame art an older flux made is made again
//! by the art job, which then deletes the old rows and files.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use common::*;
use flux_catalog::{read, ArtKind, Catalog, CatalogConfig, NewArt};
use flux_core::api::{ArtSource, Id};
use flux_core::LibraryKind;
use flux_server::{api::router, build_state, jobs, Config};
use flux_test_media::Clip;

fn jpeg(seed: u8) -> Vec<u8> {
    let img = image::RgbImage::from_fn(60, 90, |x, y| image::Rgb([seed, x as u8, y as u8]));
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80)
        .encode_image(&img)
        .unwrap();
    out
}

fn frame_art(kind: ArtKind, path: String) -> NewArt {
    NewArt {
        kind,
        source: ArtSource::Frame,
        path: Some(path),
        url: None,
        width: Some(60),
        height: Some(90),
        blurhash: None,
        attribution: None,
        generator: None,
        gen_version: None,
        gen_title: None,
    }
}

/// The data dir's contents moved elsewhere (as the startup message about
/// an older default data dir suggests): every image still serves, the
/// relative rows and an older catalog's absolute ones alike, and from the
/// new folder (the old one is gone).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn art_still_serves_after_the_data_dir_moves() {
    let dir = scratch("art-moved");
    let old = dir.path().join("old-data");
    let new = dir.path().join("new-data");
    let (poster, backdrop) = (jpeg(1), jpeg(2));
    std::fs::create_dir_all(old.join("art")).unwrap();
    std::fs::write(old.join("art").join("1-poster-0000000a.jpg"), &poster).unwrap();
    std::fs::write(old.join("art").join("1-backdrop-0000000b.jpg"), &backdrop).unwrap();
    let legacy = old
        .join("art")
        .join("1-backdrop-0000000b.jpg")
        .to_string_lossy()
        .into_owned();
    let (relative_id, legacy_id): (Id, Id) = tokio::task::block_in_place(|| {
        let cat = Catalog::open(old.join("catalog.db"), CatalogConfig::default()).unwrap();
        let w = cat.writer();
        w.exec(|c| {
            c.execute_batch(
                "INSERT INTO library(id, name, kind, created_at) VALUES (1, 'Films', 'movies', 0);
                 INSERT INTO item(id, library_id, kind, group_key, title, sort_title, added_at, content_added_at, updated_at)
                 VALUES (1, 1, 'movie', 'copper sky', 'Copper Sky', 'copper sky', 0, 0, 0);",
            )?;
            Ok(())
        })
        .wait()
        .unwrap();
        let a = w
            .add_art(1, frame_art(ArtKind::Poster, "art/1-poster-0000000a.jpg".into()))
            .wait()
            .unwrap();
        // How catalogs up to v5 stored it: the old data dir's absolute path.
        let b = w
            .add_art(1, frame_art(ArtKind::Backdrop, legacy.clone()))
            .wait()
            .unwrap();
        cat.close().unwrap();
        (a, b)
    });
    std::fs::rename(&old, &new).unwrap();
    assert!(!Path::new(&legacy).exists());

    let (st, _) = build_state(Config::for_tests(&new), Instant::now(), None)
        .await
        .unwrap();
    let app = router(st.clone());
    for (id, bytes) in [(relative_id, &poster), (legacy_id, &backdrop)] {
        let r = get(&app, &format!("/api/v1/art/{id}")).await;
        assert_eq!(r.status, StatusCode::OK, "art {id}: {}", r.text());
        assert_eq!(&r.body[..], &bytes[..], "art {id}");
        // Resized copies too (they are cached by id).
        let r = get(&app, &format!("/api/v1/art/{id}?w=40")).await;
        assert_eq!(r.status, StatusCode::OK, "art {id} resized: {}", r.text());
    }
}

/// An older flux's frame poster and backdrop (no art version): the art
/// job makes new ones (versioned, the poster with its caption, stored
/// relative to the data dir), then deletes the old rows and their files.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_frame_art_is_made_again_then_deleted() {
    let Some(src) = flux_test_media::clip(Clip::H264AacMp4) else {
        return;
    };
    let dir = scratch("art-stale");
    let lib = dir.path().join("lib");
    flux_test_media::clone_file(&src, &lib.join("Copper Sky (2019)").join("Copper Sky (2019).mp4")).unwrap();
    let data = dir.path().join("data");
    let mut cfg = Config::for_tests(&data);
    cfg.jobs = true;
    let (st, _) = build_state(cfg, Instant::now(), None).await.unwrap();
    let cat = st.catalog.clone();
    tokio::task::block_in_place(|| seed(&cat, "Movies", LibraryKind::Movies, &lib, |_| true));
    let (item, _) = item_of(&cat, "Copper Sky (2019).mp4");
    let old_names = [
        format!("{item}-poster-0000000a.jpg"),
        format!("{item}-backdrop-0000000b.jpg"),
    ];
    let mut old_ids = Vec::new();
    for (name, kind) in old_names.iter().zip([ArtKind::Poster, ArtKind::Backdrop]) {
        std::fs::write(data.join("art").join(name), jpeg(3)).unwrap();
        let id = cat
            .writer()
            .add_art(item, frame_art(kind, format!("art/{name}")))
            .await
            .unwrap();
        old_ids.push(id);
    }

    let job = jobs::spawn(st.clone());
    let art = tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            let rows = cat.read(move |c| read::item_art(c, item)).await.unwrap();
            if rows.len() == 2 && rows.iter().all(|r| r.gen_version.is_some()) {
                break rows;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("new frame art within 180 s, the old rows gone");
    for r in &art {
        assert!(!old_ids.contains(&r.id));
        assert_eq!(r.gen_version, Some(flux_meta::art::ART_VERSION));
        let path = r.path.as_deref().unwrap();
        assert!(path.starts_with("art/"), "{path}");
        assert!(data.join(path).is_file(), "{path}");
    }
    let poster = art.iter().find(|r| r.kind == "poster").unwrap();
    assert_eq!(poster.gen_title.as_deref(), Some("Copper Sky (2019)"));
    let backdrop = art.iter().find(|r| r.kind == "backdrop").unwrap();
    assert_eq!(backdrop.gen_title, None, "backdrops carry no title");
    for name in &old_names {
        assert!(!data.join("art").join(name).exists(), "{name} deleted");
    }
    st.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), job).await;
}
