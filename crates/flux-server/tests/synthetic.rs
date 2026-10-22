//! The server end to end on a clip made with ffmpeg at test time
//! (`flux_test_media::clip`), so it runs on any machine with ffmpeg (CI
//! included): a seeded library, `POST /play` for an H.264-only client (an
//! HEVC source, so the video is transcoded, by libx264 here), the HLS
//! resources through the router, and the media-tools report.

mod common;

use std::time::Instant;

use axum::http::{Method, StatusCode};
use common::*;
use flux_core::api::{ItemDetail, PlanPreview, PlayResponse};
use flux_core::{Codec, LibraryKind, VideoOp};
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

/// Subtitle files beside a video, end to end: listed with the asset (with
/// what their names say), offered by `/play` as WebVTT renditions next to
/// the file (direct play here), served as WebVTT with their CJK text; and a
/// PGS file chosen as the subtitle makes the plan a transcode that draws
/// it into the video.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subtitle_files_play_end_to_end() {
    let Some(src) = flux_test_media::clip(Clip::H264AacMp4) else {
        return;
    };
    let dir = scratch("play-subtitle-files");
    let lib = dir.path().join("lib");
    let folder = lib.join("Synthetic Test (2020)");
    flux_test_media::clone_file(&src, &folder.join("Synthetic Test (2020).mp4")).unwrap();
    let files = [
        (
            "Synthetic Test (2020).en.srt",
            "1\n00:00:01,000 --> 00:00:02,000\nHello\n".as_bytes(),
        ),
        (
            "Subs/Synthetic Test (2020).chs.ass",
            "[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
             Dialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,示例字幕\n"
                .as_bytes(),
        ),
        ("Synthetic Test (2020).sup", b"PG".as_slice()),
    ];
    for (name, body) in files {
        let p = folder.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    let (st, _) = build_state(Config::for_tests(dir.path().join("data")), Instant::now(), None)
        .await
        .unwrap();
    let cat = st.catalog.clone();
    let lib_id = tokio::task::block_in_place(|| seed(&cat, "Movies", LibraryKind::Movies, &lib, |_| true));
    // `seed` doesn't run the scanner: record the subtitle files as a scan does.
    let root_id = cat
        .read_blocking(move |c| {
            Ok(flux_catalog::scan_support::list_roots(c)?
                .into_iter()
                .find(|r| r.library_id == lib_id)
                .map(|r| r.id))
        })
        .unwrap()
        .unwrap();
    let rows = files
        .iter()
        .map(|(name, body)| flux_catalog::SubtitleFileRow {
            rel_path: format!("Synthetic Test (2020)/{name}"),
            size: body.len() as u64,
            mtime_ns: 1,
        })
        .collect();
    cat.writer()
        .sync_subtitle_files(root_id, rows, vec![])
        .await
        .unwrap();
    let (item, asset) = item_of(&cat, "Synthetic Test (2020).mp4");
    let app = router(st.clone());

    let detail: ItemDetail = get(&app, &format!("/api/v1/items/{item}")).await.json();
    let listed = &detail.assets[0].subtitle_files;
    let langs: Vec<(Codec, Option<&str>)> = listed.iter().map(|f| (f.codec, f.language.as_deref())).collect();
    assert_eq!(
        langs,
        vec![
            (Codec::Subrip, Some("eng")),
            (Codec::Pgs, None),
            (Codec::Ass, Some("zh-Hans"))
        ],
        "{listed:?}"
    );

    let r = post(
        &app,
        "/api/v1/play",
        json!({"item_id": item, "asset_id": asset, "caps": chrome_caps(), "audio_track": null, "subtitle_track": null, "start_ms": 0}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let play: PlayResponse = r.json();
    assert!(play.plan.is_direct(), "{:?}", play.explain);
    assert_eq!(play.subtitles.len(), 2, "{:?}", play.subtitles);
    let mut texts = String::new();
    for s in &play.subtitles {
        let v = get(&app, &s.url).await;
        assert_eq!(v.status, StatusCode::OK, "{}: {}", s.url, v.text());
        assert_eq!(
            v.header("content-type").as_deref(),
            Some("text/vtt; charset=utf-8")
        );
        assert!(v.header("content-length").is_some(), "a file is sent whole");
        texts.push_str(&v.text());
    }
    assert!(texts.contains("00:00:01.000 --> 00:00:02.000\nHello"), "{texts}");
    assert!(
        texts.contains("00:00:03.000 --> 00:00:04.000\n示例字幕"),
        "{texts}"
    );

    let pgs = listed.iter().find(|f| f.codec == Codec::Pgs).unwrap();
    let r = post(
        &app,
        "/api/v1/plan",
        json!({"item_id": item, "asset_id": asset, "caps": chrome_caps(), "audio_track": null, "subtitle_track": pgs.track, "start_ms": null}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let preview: PlanPreview = r.json();
    assert_eq!(preview.label, "Transcode");
    assert!(
        matches!(preview.plan.video, VideoOp::Transcode { burn_subtitle: Some(t), .. } if t == pgs.track),
        "{:?}",
        preview.plan.video
    );
}
