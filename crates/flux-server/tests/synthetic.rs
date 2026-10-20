//! The server end to end on a clip made with ffmpeg at test time
//! (`flux_test_media::clip`), so it runs on any machine with ffmpeg (CI
//! included): a seeded library, `POST /play` for an H.264-only client (an
//! HEVC source, so the video is transcoded, by libx264 here), the HLS
//! resources through the router, and the media-tools report.

mod common;

use std::time::Instant;

use axum::http::{Method, StatusCode};
use common::*;
use flux_core::api::PlayResponse;
use flux_core::{LibraryKind, VideoOp};
use flux_hls::VideoEncoder;
use flux_server::{api::router, build_state, Config};
use flux_test_media::Clip;
use serde_json::json;

fn playlist_uris(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transcoded_play_of_a_generated_clip() {
    let Some(src) = flux_test_media::clip(Clip::HevcAc3Mkv) else {
        return;
    };
    if !flux_test_media::has_encoder("libx264") {
        return;
    }
    let dir = scratch("play-synthetic");
    let lib = dir.path().join("lib");
    let file = lib
        .join("Synthetic Test (2020)")
        .join("Synthetic Test (2020).mkv");
    flux_test_media::clone_file(&src, &file).unwrap();
    let mut cfg = Config::for_tests(dir.path().join("data"));
    cfg.video_encoder = Some(VideoEncoder::Libx264);
    let (st, _) = build_state(cfg, Instant::now(), None).await.unwrap();
    let cat = st.catalog.clone();
    tokio::task::block_in_place(|| seed(&cat, "Movies", LibraryKind::Movies, &lib, |_| true));
    let (item, asset) = item_of(&cat, "Synthetic Test (2020).mkv");
    let app = router(st.clone());

    let caps = serde_json::to_value(flux_hls::presets::legacy_h264()).unwrap();
    let r = post(
        &app,
        "/api/v1/play",
        json!({"item_id": item, "asset_id": null, "caps": caps, "audio_track": null, "subtitle_track": null, "start_ms": null}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let play: PlayResponse = r.json();
    assert_eq!(play.asset_id, asset);
    assert!(
        matches!(play.plan.video, VideoOp::Transcode { .. }),
        "{:?}",
        play.explain
    );
    let base = play.url.trim_end_matches("master.m3u8").to_string();
    let master = get(&app, &play.url).await;
    assert_eq!(master.status, StatusCode::OK);
    let variant_rel = playlist_uris(&master.text()).into_iter().next().expect("variant");
    let variant = get(&app, &format!("{base}{variant_rel}")).await;
    assert_eq!(variant.status, StatusCode::OK);
    let pieces = playlist_uris(&variant.text());
    assert!(pieces.len() >= 3, "{}", variant.text());
    let vdir = format!("{base}{}", variant_rel.trim_end_matches("index.m3u8"));
    let init = get(&app, &format!("{vdir}init.mp4")).await;
    assert_eq!(init.status, StatusCode::OK, "{}", init.text());
    assert_eq!(&init.body[4..8], b"ftyp");
    for p in [&pieces[0], &pieces[pieces.len() - 1]] {
        let seg = get(&app, &format!("{vdir}{p}")).await;
        assert_eq!(seg.status, StatusCode::OK, "{p}: {}", seg.text());
        assert_eq!(&seg.body[4..8], b"styp");
        // Transcoded video gets a weak validator (HlsEngine::is_deterministic).
        assert!(seg.header("etag").is_some_and(|e| e.starts_with("W/")), "{p}");
    }
    // AC-3 is not in the H.264-only client's list: an AAC rendition.
    let audio = play.audio.first().expect("an audio rendition");
    let a = get(&app, &format!("{base}a/{}/0.m4s", audio.rendition)).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.text());

    // The media-tools report: libx264, forced; owners and admins only.
    let r = get(&app, "/api/v1/admin/media-tools").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let report: serde_json::Value = r.json();
    assert_eq!(report["video"][0]["encoder"], "libx264", "{report}");
    assert_eq!(report["forced"], "libx264", "{report}");
    assert_eq!(report["ok"], true, "{report}");
    let anon = call(&app, Method::GET, "/api/v1/admin/media-tools", None, &[], None).await;
    assert_eq!(anon.status, StatusCode::FORBIDDEN);
}
