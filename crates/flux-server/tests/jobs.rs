//! Background jobs: a title without art gets a frame poster and backdrop
//! (flux-meta), stored as art rows and served resized by /api/v1/art.

mod common;

use std::time::{Duration, Instant};

use axum::http::{Method, StatusCode};
use common::*;
use flux_core::api::{ArtSource, Event, ItemDetail};
use flux_core::LibraryKind;
use flux_server::{api::router, build_state, jobs, Config};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn frame_posters_become_art_rows_and_resize() {
    if !have_media() {
        eprintln!("skipping: corpus not mounted");
        return;
    }
    let dir = scratch("jobs");
    let mut cfg = Config::for_tests(dir.path());
    cfg.jobs = true;
    let (st, _) = build_state(cfg, Instant::now(), None).await.unwrap();
    let cat = st.catalog.clone();
    tokio::task::block_in_place(|| {
        seed(&cat, "Features", LibraryKind::Movies, &media("features"), |n| {
            n == "intro_shared.mkv"
        })
    });
    let (item, _) = item_of(&cat, "intro_shared.mkv");
    let mut events = st.events.subscribe();
    let job = jobs::spawn(st.clone());
    let app = router(st.clone());

    let updated = tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            match events.recv().await {
                Ok(Event::ItemsChanged { updated, .. }) if updated.contains(&item) => break,
                Ok(_) => {}
                Err(e) => panic!("event bus: {e}"),
            }
        }
    })
    .await;
    assert!(updated.is_ok(), "no frame art within 180 s");
    let detail: ItemDetail = get(&app, &format!("/api/v1/items/{item}")).await.json();
    let poster = detail.summary.poster.expect("poster");
    let backdrop = detail.summary.backdrop.expect("backdrop");
    assert_eq!(
        (poster.source, backdrop.source),
        (ArtSource::Frame, ArtSource::Frame)
    );
    assert!(poster.blurhash.is_some() && poster.width.is_some());
    // The ffmpeg wrapper runs decodes at low priority.
    assert!(dir.path().join("bin/ffmpeg-low").is_file());

    let r = get(&app, &format!("{}?w=300", poster.url)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_eq!(r.header("content-type").as_deref(), Some("image/jpeg"));
    assert_eq!(
        r.header("cache-control").as_deref(),
        Some("public, max-age=31536000, immutable")
    );
    let img = image::load_from_memory(&r.body).unwrap();
    assert_eq!(img.width(), 320, "rounded up to the 320 bucket");
    let etag = r.header("etag").unwrap();
    let again = call(
        &app,
        Method::GET,
        &format!("{}?w=300", poster.url),
        Some(owner()),
        &[("if-none-match", &etag)],
        None,
    )
    .await;
    assert_eq!(again.status, StatusCode::NOT_MODIFIED);
    let full = get(&app, &poster.url).await;
    assert_eq!(
        image::load_from_memory(&full.body).unwrap().width(),
        poster.width.unwrap()
    );
    assert_eq!(
        get(&app, "/api/v1/art/999999").await.status,
        StatusCode::NOT_FOUND
    );

    st.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), job).await;
}
