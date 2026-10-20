//! Playback through the router: POST /play planned from the playback index,
//! HLS resources, direct play with Range, sessions, restarts and moves.
//! Unix only: the link tests make symlinks.
#![cfg(unix)]

mod common;

use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;

use axum::http::{Method, StatusCode};
use common::*;
use flux_catalog::{read, FileMove, ScanBatch};
use flux_core::api::{Home, PlanPreview, PlayResponse, SessionDiagnostics};
use flux_core::{AudioOp, Codec, DeliveryFormat, LibraryKind};
use flux_server::{api::router, build_state, Config};
use serde_json::json;

fn play_req(item: i64, caps: serde_json::Value) -> serde_json::Value {
    json!({"item_id": item, "asset_id": null, "caps": caps, "audio_track": null, "subtitle_track": null, "start_ms": null})
}

fn playlist_uris(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

fn default_audio_uri(master: &str) -> Option<String> {
    master
        .lines()
        .filter(|l| l.starts_with("#EXT-X-MEDIA:TYPE=AUDIO") && l.contains("DEFAULT=YES"))
        .find_map(|l| l.split("URI=\"").nth(1)?.split('"').next().map(String::from))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn play_truehd_mkv_over_hls_with_sessions() {
    if !have_media() {
        eprintln!("skipping: corpus not mounted");
        return;
    }
    let dir = scratch("play-hls");
    let (st, _) = build_state(Config::for_tests(dir.path()), Instant::now(), None)
        .await
        .unwrap();
    let cat = st.catalog.clone();
    tokio::task::block_in_place(|| {
        seed(&cat, "Features", LibraryKind::Movies, &media("features"), |n| {
            n == "h264_truehd71.mkv"
        })
    });
    let (item, asset) = item_of(&cat, "h264_truehd71.mkv");
    let app = router(st.clone());

    let r = post(&app, "/api/v1/play", play_req(item, chrome_caps())).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let play: PlayResponse = r.json();
    assert_eq!(play.asset_id, asset);
    assert_eq!(play.plan.delivery, DeliveryFormat::HlsFmp4);
    assert!(play.url.starts_with(&format!("/media/{asset}-")) && play.url.ends_with("/master.m3u8"));
    assert!(
        play.explain.iter().any(|e| e.contains("TrueHD")),
        "{:?}",
        play.explain
    );
    // TrueHD can't go in fMP4: the renditions are conversions.
    assert!(!play.audio.is_empty() && play.audio.iter().all(|a| a.codec != Codec::TrueHd));
    assert!(play.audio[0].default);
    let base = play.url.trim_end_matches("master.m3u8").to_string();
    assert!(play.prefetch.contains(&format!("{base}v/init.mp4")));
    assert!(play.prefetch.contains(&format!("{base}v/0.m4s")));
    assert!(
        play.prefetch.iter().any(|u| u.contains("/a/")),
        "{:?}",
        play.prefetch
    );
    assert_eq!(play.duration_ms.map(|d| d / 1000), Some(30));

    // The same request maps to the same stream (deterministic plan id).
    let again: PlayResponse = post(&app, "/api/v1/play", play_req(item, chrome_caps()))
        .await
        .json();
    assert_eq!(again.url, play.url);
    assert_ne!(again.session_id, play.session_id);

    // master → variant → init → segment 0, then the default audio rendition.
    let master = get(&app, &play.url).await;
    assert_eq!(master.status, StatusCode::OK);
    assert_eq!(
        master.header("content-type").as_deref(),
        Some("application/vnd.apple.mpegurl")
    );
    assert_eq!(
        master.header("cache-control").as_deref(),
        Some("public, max-age=10")
    );
    let mtext = master.text();
    assert!(mtext.starts_with("#EXTM3U"));
    let variant_rel = playlist_uris(&mtext).into_iter().next().expect("variant");
    let variant = get(&app, &format!("{base}{variant_rel}")).await;
    assert_eq!(variant.status, StatusCode::OK);
    let segs = playlist_uris(&variant.text());
    assert!(segs.len() >= 3, "{}", variant.text());
    let vdir = format!("{base}{}", variant_rel.trim_end_matches("index.m3u8"));
    let init = get(&app, &format!("{vdir}init.mp4")).await;
    assert_eq!(init.status, StatusCode::OK);
    assert_eq!(&init.body[4..8], b"ftyp");
    assert_eq!(
        init.header("cache-control").as_deref(),
        Some("public, max-age=31536000, immutable")
    );
    let seg0 = get(&app, &format!("{vdir}{}", segs[0])).await;
    assert_eq!(seg0.status, StatusCode::OK);
    assert_eq!(seg0.header("content-length"), Some(seg0.body.len().to_string()));
    assert_eq!(&seg0.body[4..8], b"styp");
    assert!(seg0.header("x-flux-ms").is_some());
    let etag = seg0.header("etag").expect("etag");
    let cached = call(
        &app,
        Method::GET,
        &format!("{vdir}{}", segs[0]),
        Some(owner()),
        &[("if-none-match", &etag)],
        None,
    )
    .await;
    assert_eq!(cached.status, StatusCode::NOT_MODIFIED);
    let audio = default_audio_uri(&mtext).expect("audio rendition");
    let adir = format!("{base}{}", audio.trim_end_matches("index.m3u8"));
    let ainit = get(&app, &format!("{adir}init.mp4")).await;
    assert_eq!(ainit.status, StatusCode::OK, "{}", ainit.text());
    let aseg = get(&app, &format!("{adir}1.m4s")).await;
    assert_eq!(aseg.status, StatusCode::OK, "{}", aseg.text());
    assert!(aseg.body.len() > 1000);
    // Out-of-range and malformed resources.
    assert_eq!(
        get(&app, &format!("{vdir}99999.m4s")).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&app, &format!("{base}x/y.z")).await.status,
        StatusCode::NOT_FOUND
    );

    // Dry run.
    let preview: PlanPreview = post(&app, "/api/v1/plan", play_req(item, chrome_caps()))
        .await
        .json();
    assert_eq!(
        (preview.asset_id, preview.label.as_str()),
        (asset, "Audio conversion")
    );
    assert!(preview
        .plan
        .audio
        .iter()
        .all(|a| matches!(a.op, AudioOp::Transcode { .. })));

    // Progress → resume → continue watching; diagnostics.
    let report = json!({"position_ms": 12_000, "paused": false, "ttff_ms": 180, "last_seek_ms": null, "stalls": 0, "dropped_frames": 0});
    let r = post(
        &app,
        &format!("/api/v1/sessions/{}/progress", play.session_id),
        report,
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    let home: Home = get(&app, "/api/v1/home").await.json();
    assert!(home.continue_watching.iter().any(|i| i.id == item));
    let resumed: PlayResponse = post(&app, "/api/v1/play", play_req(item, chrome_caps()))
        .await
        .json();
    assert_eq!(resumed.start_ms, 12_000);
    // A rewatch: finishing (≥ 90 %) marks the item watched and clears the
    // resume point; stopping part-way through a second viewing is listed in
    // Continue watching, so Play must resume there too (it restarted at 0).
    for position in [29_000, 9_000] {
        let report = json!({"position_ms": position, "paused": false, "ttff_ms": 180, "last_seek_ms": null, "stalls": 0, "dropped_frames": 0});
        let r = post(
            &app,
            &format!("/api/v1/sessions/{}/progress", play.session_id),
            report,
        )
        .await;
        assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    }
    let home: Home = get(&app, "/api/v1/home").await.json();
    let entry = home
        .continue_watching
        .iter()
        .find(|i| i.id == item)
        .expect("rewatch listed");
    assert!(entry
        .progress
        .as_ref()
        .is_some_and(|p| p.watched && p.position_ms == 9_000));
    let rewatch: PlayResponse = post(&app, "/api/v1/play", play_req(item, chrome_caps()))
        .await
        .json();
    assert_eq!(rewatch.start_ms, 9_000);
    let diag: SessionDiagnostics = get(&app, &format!("/api/v1/sessions/{}/diagnostics", play.session_id))
        .await
        .json();
    assert!(diag.segments_served >= 1 && diag.bytes_served > 0, "{diag:?}");
    assert!(diag.segment_ms_p50.is_some());
    assert_eq!(diag.path, "localhost");
    assert_eq!(diag.client.and_then(|c| c.ttff_ms), Some(180));

    // The index was cached in the catalog for next time.
    let stored = eventually(10, || {
        cat.read_blocking(move |c| read::asset_play_index(c, asset))
            .ok()
            .flatten()
    })
    .await;
    let stored = stored.expect("cached index");
    assert_eq!(
        (stored.format.as_str(), stored.version),
        (
            flux_hls::index::INDEX_FORMAT_BINARY,
            flux_hls::index::INDEX_VERSION
        )
    );
    // flux-hls's binary encoding, decodable as such.
    flux_hls::PlaybackIndex::from_bytes(&stored.bytes).expect("binary index");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_play_with_ranges() {
    if !have_media() {
        eprintln!("skipping: corpus not mounted");
        return;
    }
    let dir = scratch("play-direct");
    let (st, _) = build_state(Config::for_tests(dir.path()), Instant::now(), None)
        .await
        .unwrap();
    let cat = st.catalog.clone();
    tokio::task::block_in_place(|| {
        seed(&cat, "Features", LibraryKind::Movies, &media("features"), |n| {
            n == "h264_aac_faststart.mp4"
        })
    });
    let (item, asset) = item_of(&cat, "h264_aac_faststart.mp4");
    let app = router(st.clone());
    let play: PlayResponse = post(&app, "/api/v1/play", play_req(item, chrome_caps()))
        .await
        .json();
    assert_eq!(play.plan.delivery, DeliveryFormat::ProgressiveFile);
    assert_eq!(play.url, format!("/media/{asset}/file"));
    assert!(play.prefetch.is_empty() && play.audio.is_empty());

    let path = media("features/h264_aac_faststart.mp4");
    let bytes = std::fs::read(&path).unwrap();
    let len = bytes.len();
    let ranged = |range: &'static str| {
        let app = app.clone();
        let url = play.url.clone();
        async move { call(&app, Method::GET, &url, Some(owner()), &[("range", range)], None).await }
    };
    let r = ranged("bytes=0-99").await;
    assert_eq!(r.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(r.header("content-range"), Some(format!("bytes 0-99/{len}")));
    assert_eq!(&r.body[..], &bytes[..100]);
    assert_eq!(r.header("accept-ranges").as_deref(), Some("bytes"));
    assert_eq!(r.header("content-type").as_deref(), Some("video/mp4"));
    let r = ranged("bytes=-10").await;
    assert_eq!(&r.body[..], &bytes[len - 10..]);
    // A range spanning several read chunks.
    let r = ranged("bytes=1000000-3500000").await;
    assert_eq!(&r.body[..], &bytes[1_000_000..=3_500_000]);
    let r = ranged("bytes=999999999-").await;
    assert_eq!(r.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(r.header("content-range"), Some(format!("bytes */{len}")));

    // HEAD: full length without the body; validators.
    let head = call(&app, Method::HEAD, &play.url, Some(owner()), &[], None).await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.header("content-length"), Some(len.to_string()));
    let etag = head.header("etag").expect("etag");
    let lm = head.header("last-modified").expect("last-modified");
    let stale = call(
        &app,
        Method::HEAD,
        &play.url,
        Some(owner()),
        &[("range", "bytes=0-9"), ("if-range", "\"other\"")],
        None,
    )
    .await;
    assert_eq!(stale.status, StatusCode::OK);
    for validator in [etag.as_str(), lm.as_str()] {
        let fresh = call(
            &app,
            Method::GET,
            &play.url,
            Some(owner()),
            &[("range", "bytes=0-9"), ("if-range", validator)],
            None,
        )
        .await;
        assert_eq!(fresh.status, StatusCode::PARTIAL_CONTENT, "{validator}");
    }
    let nm = call(
        &app,
        Method::GET,
        &play.url,
        Some(owner()),
        &[("if-none-match", &etag)],
        None,
    )
    .await;
    assert_eq!(nm.status, StatusCode::NOT_MODIFIED);

    let diag: SessionDiagnostics = get(&app, &format!("/api/v1/sessions/{}/diagnostics", play.session_id))
        .await
        .json();
    assert!(diag.bytes_served >= 100 + 10 + 2_500_001, "{diag:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streams_survive_restarts_and_file_moves() {
    if !have_media() {
        eprintln!("skipping: corpus not mounted");
        return;
    }
    let dir = scratch("play-restart");
    // A scratch library with an APFS clone of a corpus file (media/ is read-only).
    let lib = dir.path().join("lib");
    let movie_dir = lib.join("Sintel Test (2020)");
    std::fs::create_dir_all(&movie_dir).unwrap();
    let file = movie_dir.join("Sintel Test (2020).mkv");
    flux_test_media::clone_file(&media("features/h264_multi_audio_subs.mkv"), &file).unwrap();
    let data = dir.path().join("data");

    let (st1, _) = build_state(Config::for_tests(&data), Instant::now(), None)
        .await
        .unwrap();
    let cat = st1.catalog.clone();
    tokio::task::block_in_place(|| seed(&cat, "Scratch", LibraryKind::Movies, &lib, |_| true));
    let (item, asset) = item_of(&cat, "Sintel Test (2020).mkv");
    let app1 = router(st1.clone());
    let play: PlayResponse = post(&app1, "/api/v1/play", play_req(item, safari_caps()))
        .await
        .json();
    assert_eq!(play.plan.delivery, DeliveryFormat::HlsFmp4);
    let base = play.url.trim_end_matches("master.m3u8").to_string();
    let seg_a = get(&app1, &format!("{base}v/1.m4s")).await;
    assert_eq!(seg_a.status, StatusCode::OK);
    assert!(!play.subtitles.is_empty(), "the clip has text subtitles");
    let vtt = get(&app1, &play.subtitles[0].url).await;
    assert_eq!(vtt.status, StatusCode::OK);
    assert!(vtt.text().starts_with("WEBVTT"));
    eventually(10, || {
        cat.read_blocking(move |c| read::asset_play_index(c, asset))
            .ok()
            .flatten()
    })
    .await;
    st1.catalog.writer().flush().await.unwrap();
    drop(app1);
    let st1_catalog = st1.catalog.clone();
    drop(st1);
    st1_catalog.writer().shutdown().await.unwrap();
    drop(st1_catalog);
    drop(cat);

    // "Restart": a new server over the same data dir knows the stream and the session.
    let (st2, _) = build_state(Config::for_tests(&data), Instant::now(), None)
        .await
        .unwrap();
    let app2 = router(st2.clone());
    let seg_b = get(&app2, &format!("{base}v/1.m4s")).await;
    assert_eq!(seg_b.status, StatusCode::OK, "{}", seg_b.text());
    assert_eq!(seg_a.body, seg_b.body);
    let report = json!({"position_ms": 5_000, "paused": true, "ttff_ms": null, "last_seek_ms": null, "stalls": null, "dropped_frames": null});
    let r = post(
        &app2,
        &format!("/api/v1/sessions/{}/progress", play.session_id),
        report,
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());

    // Rename on disk + FileMove in the catalog: the cached index is reused
    // with its path rewritten, and the plan id doesn't change.
    let loc = st2
        .catalog
        .read(move |c| read::asset_location(c, asset))
        .await
        .unwrap()
        .unwrap();
    let moved = movie_dir.join("renamed.mkv");
    std::fs::rename(&file, &moved).unwrap();
    let meta = std::fs::metadata(&moved).unwrap();
    st2.catalog
        .writer()
        .apply(ScanBatch {
            root_id: loc.root_id,
            moves: vec![FileMove {
                file_id: loc.file_id,
                to: facts("Sintel Test (2020)/renamed.mkv", &meta),
                guess: None,
            }],
            ..Default::default()
        })
        .await
        .unwrap();
    let index_before = st2
        .catalog
        .read(move |c| read::asset_play_index(c, asset))
        .await
        .unwrap();
    let play2: PlayResponse = post(&app2, "/api/v1/play", play_req(item, safari_caps()))
        .await
        .json();
    assert_eq!(play2.url, play.url);
    let seg_c = get(&app2, &format!("{base}v/1.m4s")).await;
    assert_eq!((seg_c.status, &seg_c.body), (StatusCode::OK, &seg_a.body));
    let index_after = st2
        .catalog
        .read(move |c| read::asset_play_index(c, asset))
        .await
        .unwrap();
    assert_eq!(
        index_before, index_after,
        "the cached index was reused, not rebuilt"
    );
    assert_eq!(
        index_after.map(|i| i.path),
        Some(moved.to_string_lossy().into_owned()),
        "served with the file's current path"
    );

    // An unreachable root: 409, never a crash or a delete.
    st2.catalog
        .writer()
        .set_root_availability(loc.root_id, false)
        .await
        .unwrap();
    let r = post(&app2, "/api/v1/play", play_req(item, safari_caps())).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.text());
    let file_url = format!("/media/{asset}/file");
    assert_eq!(get(&app2, &file_url).await.status, StatusCode::CONFLICT);
    assert!(Path::new(&moved).exists());
}

/// The plan id inside a session id (`{item}.{plan}.{nonce}`).
fn plan_of(session_id: &str) -> String {
    session_id.split('.').nth(1).expect("plan id").to_string()
}

/// Review SEC-4: the reviewer's play_flood.py replayed POST /play with
/// `max_bitrate` 100000, 100001, … and every request wrote a new
/// `<data>/plans/<id>.json`. Caps are now rounded to standard rungs before
/// planning, so these requests share one plan (and one file).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn varied_caps_do_not_mint_a_plan_per_request() {
    if !have_media() {
        eprintln!("skipping: corpus not mounted");
        return;
    }
    let dir = scratch("plan-flood");
    let mut cfg = Config::for_tests(dir.path());
    // As in the verifier's run: no ffmpeg, so the transcodes these low
    // bitrates ask for are never started.
    cfg.ffmpeg = "/nonexistent/ffmpeg".into();
    cfg.ffprobe = "/nonexistent/ffprobe".into();
    let (st, _) = build_state(cfg, Instant::now(), None).await.unwrap();
    let cat = st.catalog.clone();
    tokio::task::block_in_place(|| {
        seed(&cat, "Features", LibraryKind::Movies, &media("features"), |n| {
            n == "h264_aac_faststart.mp4"
        })
    });
    let (item, _) = item_of(&cat, "h264_aac_faststart.mp4");
    let app = router(st.clone());
    let plan_files = || {
        std::fs::read_dir(dir.path().join("plans"))
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .count()
    };
    let mut plans = HashSet::new();
    for (base, step) in [(100_000u64, 1u64), (3_000_000, 7_919)] {
        for i in 0..30 {
            let mut caps = chrome_caps();
            caps["max_bitrate"] = json!(base + i * step);
            let r = post(&app, "/api/v1/play", play_req(item, caps)).await;
            assert_eq!(r.status, StatusCode::OK, "{}", r.text());
            plans.insert(plan_of(&r.json::<PlayResponse>().session_id));
        }
    }
    assert!(plans.len() <= 2, "60 requests, {} plans: {plans:?}", plans.len());
    assert_eq!(plan_files(), plans.len());
    assert_eq!(st.play.plan_files(), plans.len());
}

/// Review SEC-5: forged session ids on `GET …/diagnostics` (any nonce with
/// a real plan id) each added a session that was never evicted. Reading
/// diagnostics now creates no state.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forged_session_ids_create_no_sessions() {
    if !have_media() {
        eprintln!("skipping: corpus not mounted");
        return;
    }
    let dir = scratch("session-flood");
    let (st, _) = build_state(Config::for_tests(dir.path()), Instant::now(), None)
        .await
        .unwrap();
    let cat = st.catalog.clone();
    tokio::task::block_in_place(|| {
        seed(&cat, "Features", LibraryKind::Movies, &media("features"), |n| {
            n == "h264_aac_faststart.mp4"
        })
    });
    let (item, _) = item_of(&cat, "h264_aac_faststart.mp4");
    let app = router(st.clone());
    let play: PlayResponse = post(&app, "/api/v1/play", play_req(item, chrome_caps()))
        .await
        .json();
    let plan = plan_of(&play.session_id);
    let before = st.play.sessions_open();
    assert_eq!(before, 1);
    for i in 0..500u32 {
        let r = get(&app, &format!("/api/v1/sessions/{item}.{plan}.{i:x}/diagnostics")).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    }
    assert_eq!(
        st.play.sessions_open(),
        before,
        "diagnostics must not create sessions"
    );
    // A forgotten session's progress report still rebuilds it (restarts).
    let report = json!({"position_ms": 1_000, "paused": false, "ttff_ms": null, "last_seek_ms": null, "stalls": null, "dropped_frames": null});
    let r = post(
        &app,
        &format!("/api/v1/sessions/{item}.{plan}.beef/progress"),
        report,
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(st.play.sessions_open(), before + 1);
}

/// Review SEC-2: a file under a library root that is a symlink to a file
/// outside the library (here re-pointed after the scan, which the verifier
/// showed is served at once) must not be served to anyone. Symlinks that
/// stay inside the library, and a library whose root path is itself a
/// symlink, keep working.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_outside_the_library_are_not_served() {
    let dir = scratch("contain");
    let lib = dir.path().join("lib");
    let outside = dir.path().join("outside");
    for d in [
        "Leak (2021)",
        "Ok (2020)",
        "Alias (2019)",
        "Moved (2018)",
        "Ext (2017)",
    ] {
        std::fs::create_dir_all(lib.join(d)).unwrap();
    }
    std::fs::create_dir_all(outside.join("Moved (2018)")).unwrap();
    std::fs::create_dir_all(outside.join("Ext (2017)")).unwrap();
    let secret = outside.join("id_ed25519");
    std::fs::write(
        &secret,
        "-----BEGIN OPENSSH PRIVATE KEY-----\nstand-in, not a key\n",
    )
    .unwrap();
    std::fs::write(outside.join("Moved (2018)/Moved (2018).mkv"), "outside bytes").unwrap();
    let leak = lib.join("Leak (2021)/Leak (2021).mkv");
    let ok = lib.join("Ok (2020)/Ok (2020).mkv");
    let alias = lib.join("Alias (2019)/Alias (2019).mkv");
    let moved = lib.join("Moved (2018)/Moved (2018).mkv");
    for f in [&leak, &ok, &alias, &moved] {
        std::fs::write(f, vec![0x1a_u8; 4096]).unwrap();
    }
    // A file link that leaves the library at scan time (another disk, a
    // debrid mount): flux-scan catalogs it with its target's identity, but
    // only when it probes as media (review SEC-2), so this case needs the
    // corpus. A non-media external file would be `probe_failed` and is not
    // served (see `a_repointed_external_link_to_a_non_media_file_is_not_served`).
    let ext_target = outside.join("Ext (2017)/Ext (2017).mp4");
    let ext = lib.join("Ext (2017)/Ext (2017).mp4");
    let ext_len = if have_media() {
        std::fs::copy(media("features/h264_aac_faststart.mp4"), &ext_target).unwrap()
    } else {
        0
    };
    if have_media() {
        std::os::unix::fs::symlink(&ext_target, &ext).unwrap();
    }
    // The library's root is a symlink to the real folder.
    let root = dir.path().join("lib-link");
    std::os::unix::fs::symlink(&lib, &root).unwrap();

    let (st, _) = build_state(Config::for_tests(dir.path().join("data")), Instant::now(), None)
        .await
        .unwrap();
    let cat = st.catalog.clone();
    tokio::task::block_in_place(|| seed(&cat, "Movies", LibraryKind::Movies, &root, |_| true));
    let app = router(st.clone());
    let file_url = |name: &str| format!("/media/{}/file", item_of(&cat, name).1);

    // A regular file inside the library is served (through the root symlink).
    let r = get(&app, &file_url("Ok (2020).mkv")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_eq!(r.body.len(), 4096);
    // So is the file the scan recorded behind a link that leaves the library.
    if have_media() {
        let r = get(&app, &file_url("Ext (2017).mp4")).await;
        assert_eq!(r.status, StatusCode::OK, "external link: {}", r.text());
        assert_eq!(r.body.len() as u64, ext_len);
    }

    // After the scan: one file becomes a link to a secret outside, one a
    // link to another file of the library, and one folder becomes a link to
    // a folder outside.
    std::fs::remove_file(&leak).unwrap();
    std::os::unix::fs::symlink(&secret, &leak).unwrap();
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&ok, &alias).unwrap();
    std::fs::remove_dir_all(lib.join("Moved (2018)")).unwrap();
    std::os::unix::fs::symlink(outside.join("Moved (2018)"), lib.join("Moved (2018)")).unwrap();
    // And the external link is re-pointed at the secret.
    if have_media() {
        std::fs::remove_file(&ext).unwrap();
        std::os::unix::fs::symlink(&secret, &ext).unwrap();
    }

    let mut repointed = vec!["Leak (2021).mkv", "Moved (2018).mkv"];
    if have_media() {
        repointed.push("Ext (2017).mp4");
    }
    for name in repointed {
        let r = get(&app, &file_url(name)).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{name}: {}", r.text());
        assert!(!r.text().contains("OPENSSH") && !r.text().contains("outside bytes"));
        // Not as an anonymous LAN caller, not with a Range either.
        let r = call(
            &app,
            Method::GET,
            &file_url(name),
            None,
            &[("range", "bytes=0-9")],
            None,
        )
        .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{name}");
        // Nor through playback.
        let (item, _) = item_of(&cat, name);
        let r = post(&app, "/api/v1/play", play_req(item, chrome_caps())).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{name}: {}", r.text());
    }
    let r = get(&app, &file_url("Alias (2019).mkv")).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "a link inside the library: {}",
        r.text()
    );
}

/// SEC-2, found by the re-verification: an HLS source closes when idle
/// (PB-4) and reopens by path. A link that leaves the library, re-pointed
/// after the play at another file with the same size and mtime, must not be
/// read by the reopened source: the reopen and the server's re-check both
/// compare the inode too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reopened_hls_source_does_not_follow_a_repointed_link() {
    if !have_media() {
        eprintln!("skipping: corpus not mounted");
        return;
    }
    let dir = scratch("reopen");
    let lib = dir.path().join("lib");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(lib.join("Ext (2016)")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let target = outside.join("Ext (2016).mp4");
    flux_test_media::clone_file(&media("features/h264_aac_faststart.mp4"), &target).unwrap();
    let link = lib.join("Ext (2016)/Ext (2016).mp4");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    // Another file with the target's size and modification time.
    let other = outside.join("other.bin");
    let meta = std::fs::metadata(&target).unwrap();
    std::fs::write(&other, vec![0x5a_u8; meta.len() as usize]).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&other)
        .unwrap()
        .set_modified(meta.modified().unwrap())
        .unwrap();

    let (st, _) = build_state(Config::for_tests(dir.path().join("data")), Instant::now(), None)
        .await
        .unwrap();
    let cat = st.catalog.clone();
    tokio::task::block_in_place(|| seed(&cat, "Movies", LibraryKind::Movies, &lib, |_| true));
    let app = router(st.clone());
    let (item, asset) = item_of(&cat, "Ext (2016).mp4");

    // HLS only, so the MP4 is repackaged: segments copy sample ranges.
    let mut caps = chrome_caps();
    caps["delivery"] = json!(["hls_fmp4"]);
    caps["progressive_containers"] = json!([]);
    let r = post(&app, "/api/v1/play", play_req(item, caps)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let play: PlayResponse = r.json();
    assert_eq!(play.plan.delivery, DeliveryFormat::HlsFmp4);
    let base = play.url.trim_end_matches("master.m3u8").to_string();
    let seg = get(&app, &format!("{base}v/0.m4s")).await;
    assert_eq!(seg.status, StatusCode::OK, "{}", seg.text());

    // The source goes idle (closed), then the link is re-pointed.
    let cached = st.play.cached_asset(asset, None).expect("cached asset");
    cached.source().close();
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&other, &link).unwrap();

    let seg = get(&app, &format!("{base}v/1.m4s")).await;
    assert_ne!(
        seg.status,
        StatusCode::OK,
        "a segment read from the re-pointed link"
    );
    assert!(
        !seg.body.windows(64).any(|w| w.iter().all(|&b| b == 0x5a)),
        "the other file's bytes were served"
    );
    let r = get(&app, &format!("/media/{asset}/file")).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.text());

    // Pointed back at the file the scan recorded, playback resumes.
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let seg = get(&app, &format!("{base}v/2.m4s")).await;
    assert_eq!(seg.status, StatusCode::OK, "{}", seg.text());
}

/// SEC-2, found by the re-verification: re-pointing an external link at a
/// non-media file and letting the scan re-record it (the scanner follows the
/// link and stores the new target's identity, state probe_failed) must not
/// make that file servable. The serve-time check requires a file resolved
/// outside the roots to be confirmed media.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repointed_external_link_to_a_non_media_file_is_not_served() {
    if !have_media() {
        eprintln!("skipping: corpus not mounted");
        return;
    }
    let dir = scratch("repoint-rescan");
    let lib = dir.path().join("lib");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(lib.join("Ext (2015)")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let target = outside.join("Ext (2015).mp4");
    flux_test_media::clone_file(&media("features/h264_aac_faststart.mp4"), &target).unwrap();
    let link = lib.join("Ext (2015)/Ext (2015).mp4");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    // The attacker's file, which the scan can't read as media.
    let secret = outside.join("owner-secret");
    std::fs::write(&secret, b"OWNER PRIVATE KEY the attacker cannot otherwise read\n").unwrap();

    let (st, _) = build_state(Config::for_tests(dir.path().join("data")), Instant::now(), None)
        .await
        .unwrap();
    let cat = st.catalog.clone();
    tokio::task::block_in_place(|| seed(&cat, "Movies", LibraryKind::Movies, &lib, |_| true));
    let app = router(st.clone());
    let (_item, asset) = item_of(&cat, "Ext (2015).mp4");

    // The legitimate external media file is served.
    let r = get(&app, &format!("/media/{asset}/file")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());

    // Re-point at the secret and re-record it as the scan does: same path,
    // the secret's identity, a failed probe (it isn't media).
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    let meta = std::fs::metadata(&secret).unwrap();
    let batch = ScanBatch {
        root_id: root_of(&cat, asset),
        upserts: vec![flux_catalog::FileUpsert {
            facts: facts("Ext (2015)/Ext (2015).mp4", &meta),
            probe: flux_catalog::ProbeUpdate::Failed("not media".into()),
            guess: None,
        }],
        ..Default::default()
    };
    cat.writer().apply(batch).wait().unwrap();

    // The secret must not be served, by its bytes or a 200.
    let r = get(&app, &format!("/media/{asset}/file")).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "served the secret: {}", r.text());
    assert!(!r.text().contains("OWNER PRIVATE KEY"), "the secret leaked");

    // Pointed back at the recorded media file and re-probed present (as the
    // scan's seed does), it serves again.
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    tokio::task::block_in_place(|| {
        reseed_file(&cat, root_of(&cat, asset), "Ext (2015)/Ext (2015).mp4", &target)
    });
    let r = get(&app, &format!("/media/{asset}/file")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
}
