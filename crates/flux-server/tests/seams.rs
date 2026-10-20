//! Seams the Phase D merge wired between the fix branches
//! (docs/prototype/fixes.md): the server's ffprobe reaches the scan
//! fallback (ROB-3), overlapping roots are a 400 (INT-7), an item page
//! without art moves its art job up, video transcodes carry weak validators
//! (PB-11), and subtitle sidecars stream (PB-7).

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use axum::http::{Method, StatusCode};
use common::*;
use flux_core::api::PlayResponse;
use flux_core::{LibraryKind, VideoOp};
use flux_server::{api::router, build_state, Config};
use serde_json::json;

fn play_req(item: i64, caps: serde_json::Value) -> serde_json::Value {
    json!({"item_id": item, "asset_id": null, "caps": caps, "audio_track": null, "subtitle_track": null, "start_ms": null})
}

/// INT-7 through the API: a library whose roots nest is refused with a
/// message, not a 500; either root alone is fine.
#[tokio::test]
async fn overlapping_roots_are_a_bad_request() {
    let dir = scratch("seam-overlap");
    let outer = dir.path().join("media");
    let inner = outer.join("Movies");
    std::fs::create_dir_all(&inner).unwrap();
    let (st, _) = build_state(Config::for_tests(dir.path().join("data")), Instant::now(), None)
        .await
        .unwrap();
    let app = router(st);
    let r = post(
        &app,
        "/api/v1/libraries",
        json!({"name": "Both", "kind": "movies", "roots": [outer, inner]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.text());
    assert!(r.text().contains("inside"), "{}", r.text());
    // The same folders spelled differently (found by the re-verification):
    // through a symlink, with `..`, and (case-insensitive volumes) in
    // another letter case. Each would catalog every file below twice.
    std::fs::create_dir_all(dir.path().join("other")).unwrap();
    let mut spellings = vec![dir.path().join("other/../media/Movies")];
    #[cfg(unix)]
    {
        let alias = dir.path().join("media-link");
        std::os::unix::fs::symlink(&outer, &alias).unwrap();
        spellings.push(alias.join("Movies"));
    }
    let upper = dir.path().join("MEDIA/Movies");
    if upper.is_dir() {
        spellings.push(upper);
    }
    for second in spellings {
        let r = post(
            &app,
            "/api/v1/libraries",
            json!({"name": "Both", "kind": "movies", "roots": [outer, second]}),
        )
        .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{second:?}: {}", r.text());
        assert!(r.text().contains("inside"), "{}", r.text());
    }
    let r = post(
        &app,
        "/api/v1/libraries",
        json!({"name": "Movies", "kind": "movies", "roots": [inner]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
}

/// Opening an item that has no art yet runs its work's art job next
/// (`Writer::bump_art`), instead of after every work queued before it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opening_an_item_without_art_moves_its_art_job_up() {
    let dir = scratch("seam-art");
    let films = dir.path().join("films");
    for t in ["Alpha (2001)", "Beta (2002)"] {
        std::fs::create_dir_all(films.join(t)).unwrap();
        std::fs::write(films.join(t).join(format!("{t}.mkv")), vec![0x1a_u8; 4096]).unwrap();
    }
    let (st, _) = build_state(Config::for_tests(dir.path().join("data")), Instant::now(), None)
        .await
        .unwrap();
    let cat = st.catalog.clone();
    tokio::task::block_in_place(|| seed(&cat, "Films", LibraryKind::Movies, &films, |_| true));
    let (item, _) = item_of(&cat, "Alpha (2001).mkv");
    let priority = || {
        tokio::task::block_in_place(|| {
            cat.read_blocking(move |c| {
                Ok(c.query_row(
                    "SELECT priority FROM job WHERE kind = 'art' AND item_id = ?1 AND state = 'queued'",
                    [item],
                    |r| r.get::<_, i64>(0),
                )?)
            })
            .unwrap()
        })
    };
    let interactive = flux_catalog::jobs::priority::INTERACTIVE;
    assert_ne!(priority(), interactive);
    let app = router(st.clone());
    let r = get(&app, &format!("/api/v1/items/{item}")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert!(r.json::<serde_json::Value>()["poster"].is_null());
    eventually(5, || (priority() == interactive).then_some(()))
        .await
        .expect("the art job was moved up");
}

/// ROB-3 through the server: the library scan's ffprobe fallback runs the
/// ffprobe the server resolved (`--ffprobe`), not whatever is on `PATH`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_scan_fallback_runs_the_servers_ffprobe() {
    let flv = media("probe-fixtures/flv_h264.flv");
    if !flv.is_file() {
        eprintln!("skipping: corpus not mounted");
        return;
    }
    let dir = scratch("seam-ffprobe");
    let films = dir.path().join("films");
    let folder = films.join("Flash Movie (2004)");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::copy(&flv, folder.join("Flash Movie (2004).flv")).unwrap();
    let fake = dir.path().join("fake-ffprobe");
    std::fs::write(
        &fake,
        "#!/bin/sh\necho '{\"streams\":[{\"index\":0,\"codec_type\":\"video\",\"codec_name\":\"flv1\",\"width\":320,\"height\":240}],\"format\":{\"format_name\":\"flv\",\"duration\":\"12.5\"}}'\n",
    )
    .unwrap();
    std::process::Command::new("chmod")
        .arg("+x")
        .arg(&fake)
        .status()
        .unwrap();
    let mut cfg = Config::for_tests(dir.path().join("data"));
    cfg.scan = true;
    cfg.ffprobe = fake;
    let (st, _) = build_state(cfg, Instant::now(), None).await.unwrap();
    let app = router(st.clone());
    let r = post(
        &app,
        "/api/v1/libraries",
        json!({"name": "Films", "kind": "movies", "roots": [films]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let cat = st.catalog.clone();
    let ms = eventually(30, || {
        tokio::task::block_in_place(|| {
            cat.read_blocking(|c| {
                Ok(c.query_row("SELECT duration_ms FROM asset", [], |r| {
                    r.get::<_, Option<i64>>(0)
                })
                .ok()
                .flatten())
            })
            .ok()
            .flatten()
        })
    })
    .await;
    let scanner = st.scanner.clone();
    tokio::task::spawn_blocking(move || scanner.shutdown(Duration::from_secs(2)))
        .await
        .unwrap();
    assert_eq!(ms, Some(12_500), "the fake ffprobe's answer");
}

/// PB-11: a video transcode's init and segments are equivalent whoever asks
/// but not byte-identical across encoder runs, so they carry a weak ETag
/// (a copied rendition keeps a strong one). No encoder runs here: the
/// validator is decided before any work.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn video_transcodes_get_weak_etags() {
    if !have_media() {
        eprintln!("skipping: corpus not mounted");
        return;
    }
    let dir = scratch("seam-etag");
    let mut cfg = Config::for_tests(dir.path());
    cfg.ffmpeg = PathBuf::from("/nonexistent/ffmpeg");
    let (st, _) = build_state(cfg, Instant::now(), None).await.unwrap();
    let cat = st.catalog.clone();
    tokio::task::block_in_place(|| {
        seed(&cat, "Features", LibraryKind::Movies, &media("features"), |n| {
            n == "h264_aac_faststart.mp4"
        })
    });
    let (item, _) = item_of(&cat, "h264_aac_faststart.mp4");
    let app = router(st.clone());
    let mut caps = chrome_caps();
    caps["max_bitrate"] = json!(1_000_000);
    let r = post(&app, "/api/v1/play", play_req(item, caps)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let play: PlayResponse = r.json();
    assert!(
        matches!(play.plan.video, VideoOp::Transcode { .. }),
        "{:?}",
        play.plan.video
    );
    let base = play.url.trim_end_matches("master.m3u8").to_string();
    let plan_id = base
        .trim_start_matches("/media/")
        .trim_end_matches('/')
        .to_string();
    let weak = format!("W/\"{plan_id}/v/init.mp4\"");
    for tag in [weak.clone(), weak.trim_start_matches("W/").to_string()] {
        let r = call(
            &app,
            Method::GET,
            &format!("{base}v/init.mp4"),
            Some(owner()),
            &[("if-none-match", &tag)],
            None,
        )
        .await;
        assert_eq!(r.status, StatusCode::NOT_MODIFIED, "{tag}: {}", r.text());
        assert_eq!(r.header("etag").as_deref(), Some(weak.as_str()));
    }
    let audio = &play.audio[0];
    let r = get(&app, &format!("{base}a/{}/init.mp4", audio.rendition)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_eq!(
        r.header("etag"),
        Some(format!("\"{plan_id}/a/{}/init.mp4\"", audio.rendition)),
        "a copied rendition keeps a strong validator"
    );
}

/// PB-7 through the server: the first request for a subtitle sidecar is
/// streamed (no length: cues go out as the track is read), the next one is
/// the cached document with its length, and both are the same bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subtitle_sidecars_stream_then_come_with_a_length() {
    if !have_media() {
        eprintln!("skipping: corpus not mounted");
        return;
    }
    let dir = scratch("seam-vtt");
    let (st, _) = build_state(Config::for_tests(dir.path()), Instant::now(), None)
        .await
        .unwrap();
    let cat = st.catalog.clone();
    tokio::task::block_in_place(|| {
        seed(&cat, "Features", LibraryKind::Movies, &media("features"), |n| {
            n == "h264_multi_audio_subs.mkv"
        })
    });
    let (item, _) = item_of(&cat, "h264_multi_audio_subs.mkv");
    let app = router(st.clone());
    let play: PlayResponse = post(&app, "/api/v1/play", play_req(item, chrome_caps()))
        .await
        .json();
    let sub = play.subtitles.first().expect("a subtitle track");
    let first = get(&app, &sub.url).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.text());
    assert_eq!(
        first.header("content-type").as_deref(),
        Some("text/vtt; charset=utf-8")
    );
    assert_eq!(first.header("content-length"), None, "streamed");
    assert!(first.text().starts_with("WEBVTT"));
    let second = get(&app, &sub.url).await;
    assert_eq!(second.status, StatusCode::OK);
    assert_eq!(
        second.header("content-length"),
        Some(second.body.len().to_string())
    );
    assert_eq!(second.body, first.body);
}
