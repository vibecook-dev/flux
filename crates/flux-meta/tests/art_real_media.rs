//! Frame posters and thumbnails on the real test media (`$FLUX_TEST_MEDIA`;
//! skipped when it is not configured).

use std::path::PathBuf;

use flux_core::api::ArtSource;
use flux_meta::art::{episode_thumbnail, frame_posters, read_xmp, FramePosterOptions, MediaFacts, Transfer};

/// A feature clip of the corpus.
fn media(name: &str) -> Option<PathBuf> {
    flux_test_media::media(&format!("features/{name}"))
}

#[tokio::test]
async fn poster_and_backdrop_from_a_1080p_mkv() {
    let Some(p) = media("h264_multi_audio_subs.mkv") else {
        return;
    };
    let fp = frame_posters(
        &p,
        "Mad Max: Fury Road",
        Some(2015),
        None,
        &FramePosterOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        (fp.facts.width, fp.facts.height, fp.facts.transfer),
        (1920, 1080, Transfer::Sdr)
    );
    // 2:3 poster at the source's natural crop size (1080 tall → 720×1080), 16:9 backdrop.
    assert_eq!((fp.poster.width, fp.poster.height), (720, 1080));
    assert_eq!((fp.backdrop.width, fp.backdrop.height), (1920, 1080));
    assert_eq!(fp.poster.source, ArtSource::Frame);
    let xmp = read_xmp(&fp.poster.bytes).unwrap();
    assert!(xmp.contains("compositeSynthetic") && xmp.contains("flux:artSource=\"frame\""));
    assert!(!fp.poster.blurhash.is_empty() && !fp.backdrop.blurhash.is_empty());
    // The chosen frame is inside the 10–80% window and every candidate was scored.
    let d = fp.facts.duration_s;
    assert!(fp.chosen_at_s >= 0.10 * d && fp.chosen_at_s <= 0.80 * d);
    assert_eq!(fp.candidates.len(), 12);
    let img = image::load_from_memory(&fp.poster.bytes).unwrap();
    assert_eq!(img.width(), 720);
}

#[tokio::test]
async fn hdr10_is_detected_and_tone_mapped() {
    let Some(p) = media("hevc_main10_hdr10_eac3.mkv") else {
        return;
    };
    let fp = frame_posters(&p, "HDR", None, None, &FramePosterOptions::default())
        .await
        .unwrap();
    assert_eq!(
        fp.facts.transfer,
        Transfer::Pq,
        "HDR10 signalled only in the bitstream must still be found"
    );
    // Tone-mapped output uses the range: not washed out (PQ shown as SDR is flat and grey).
    let img = image::load_from_memory(&fp.backdrop.bytes).unwrap().to_luma8();
    let mut v: Vec<u8> = img.pixels().map(|p| p.0[0]).collect();
    v.sort_unstable();
    let (p5, p95) = (v[v.len() / 20], v[v.len() * 19 / 20]);
    assert!(p95 - p5 > 120, "contrast after tone mapping: p5 {p5} p95 {p95}");
}

#[tokio::test]
async fn av1_falls_back_to_software_decoding() {
    let Some(p) = media("av1_opus.webm") else { return };
    let fp = frame_posters(&p, "AV1 Opus", Some(2026), None, &FramePosterOptions::default())
        .await
        .unwrap();
    assert_eq!(fp.poster.width * 3, fp.poster.height * 2);
}

#[tokio::test]
async fn episode_thumbnail_is_16_9_without_text() {
    let Some(p) = media("show_e02_with_intro.mkv") else {
        return;
    };
    let (thumb, scores) = episode_thumbnail(&p, None, &FramePosterOptions::default())
        .await
        .unwrap();
    assert_eq!((thumb.width, thumb.height), (640, 360));
    assert_eq!(thumb.source, ArtSource::Frame);
    assert!(read_xmp(&thumb.bytes).unwrap().contains("digitalCapture"));
    assert!(!scores.is_empty());
}

#[tokio::test]
async fn low_res_and_long_files() {
    let Some(p) = media("long_2h_360p.mkv") else {
        return;
    };
    let facts = MediaFacts::ffprobe(&Default::default(), &p).await.unwrap();
    assert!(facts.duration_s > 7000.0);
    let fp = frame_posters(&p, "Two Hours", None, Some(facts), &FramePosterOptions::default())
        .await
        .unwrap();
    // Small sources are upscaled to the minimum poster width (600×900), never smaller.
    assert_eq!((fp.poster.width, fp.poster.height), (600, 900));
    assert!(fp.chosen_at_s > 0.1 * facts.duration_s);
}

/// The library's AV1 clips (a single keyframe at 0, then 6 s of inter frames):
/// ffmpeg's libdav1d outputs nothing with `-skip_frame nokey` there, which made
/// every AV1 title in the e2e library fail with "no decodable frame found".
#[tokio::test]
async fn av1_with_one_keyframe_still_gets_a_poster() {
    let Some(p) = flux_test_media::media("seeds/seed_av1_mkv.mkv") else {
        return;
    };
    let fp = frame_posters(
        &p,
        "Burning Frontier",
        Some(2016),
        None,
        &FramePosterOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(fp.poster.width * 3, fp.poster.height * 2);
    assert!(!fp.candidates.is_empty());
    let (thumb, _) = episode_thumbnail(&p, None, &FramePosterOptions::default())
        .await
        .unwrap();
    assert_eq!(thumb.source, ArtSource::Frame);
}
