//! Seams wired by the Phase D merge (docs/prototype/fixes.md), where the
//! two flux-hls branches (correctness, performance) meet the engine:
//!
//! - the engine kills ffprobe index scans when it shuts down (ROB-1);
//! - WebVTT streams cue by cue, then comes from the cache (PB-7);
//! - an MPEG-TS video transcode reads the source from the run's keyframe
//!   (the in-process TS index × the frame-exact transcoder).
//!
//! Each skips when its media is missing: the corpus (`$FLUX_TEST_MEDIA`)
//! or a clip made with ffmpeg (`flux_test_media::clip`). Its own test
//! binary, because `HlsEngine::shutdown` ends every ffprobe scan of the
//! process.

use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(unix)]
use std::time::{Duration, Instant};

use flux_core::{TrackKind, VideoOp};
use flux_hls::index::Layout;
use flux_hls::playlist::{HlsResource, Rendition};
use flux_hls::{presets, EngineConfig, HlsEngine, PlanPrefs, WebVttBody};
use futures::StreamExt;

use flux_test_media::Clip;

/// A corpus file (`media/<rel below $FLUX_TEST_MEDIA>`).
fn input(rel: &str) -> Option<PathBuf> {
    flux_test_media::media(rel.strip_prefix("media/").unwrap_or(rel))
}

/// The corpus AVI (it lacks a frame at 0.04 s), else the synthetic one.
fn avi() -> Option<PathBuf> {
    input("media/features/mpeg4_mp3.avi").or_else(|| flux_test_media::clip(Clip::Mpeg4Mp2Avi))
}

/// A fresh output directory for one test.
fn out_dir(test: &str) -> PathBuf {
    flux_test_media::scratch(&format!("merge_seams-{test}"))
}

fn ffprobe() -> PathBuf {
    flux_test_media::ffprobe().unwrap_or_else(|| PathBuf::from("ffprobe"))
}

fn video_packets(path: &Path) -> u64 {
    let o = Command::new(ffprobe())
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_packets",
            "-show_entries",
            "stream=nb_read_packets",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("ffprobe");
    // MPEG-TS lists the stream under its program too: the first line.
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(|l| l.trim().trim_end_matches(','))
        .find(|l| !l.is_empty())
        .and_then(|l| l.parse().ok())
        .unwrap_or(0)
}

/// ROB-1, engine side: `HlsEngine::shutdown` kills an ffprobe index scan
/// that hangs (here a fake ffprobe whose `sleep` child holds the pipes), so
/// `prepare` fails at once and no blocking thread keeps the runtime alive.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_shutdown_kills_a_hung_index_scan() {
    let Some(avi) = avi() else {
        return;
    };
    let dir = out_dir("shutdown");
    let fake = dir.join("ffprobe");
    // No `exec`: the sleep is a grandchild that only a group kill reaches.
    std::fs::write(&fake, "#!/bin/sh\nsleep 4243\n").unwrap();
    Command::new("chmod").arg("+x").arg(&fake).status().unwrap();
    let sleepers = || {
        let o = Command::new("pgrep").args(["-f", "sleep 4243"]).output().unwrap();
        String::from_utf8_lossy(&o.stdout).lines().count()
    };
    let engine = HlsEngine::new(EngineConfig {
        ffprobe: fake,
        ..Default::default()
    });
    let e2 = engine.clone();
    let task = tokio::spawn(async move { e2.prepare(&avi).await.map(|_| ()) });
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(!task.is_finished(), "the AVI is indexed by the (hanging) ffprobe");
    let t = Instant::now();
    engine.shutdown();
    let r = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("prepare returned after shutdown")
        .unwrap();
    eprintln!("prepare after shutdown: {r:?} in {:?}", t.elapsed());
    assert!(r.is_err());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(sleepers(), 0, "the fake ffprobe's child outlived the shutdown");
}

/// PB-7, end to end in the engine: the first request for a subtitle track
/// streams it (the header with the first cue, then cue by cue as blocks
/// are read), and the
/// completed document is cached, byte for byte what the whole-document path
/// (sorted cues) makes.
#[tokio::test]
async fn webvtt_streams_cue_by_cue_then_comes_from_the_cache() {
    let Some(p) = input("media/features/h264_multi_audio_subs.mkv") else {
        return;
    };
    let engine = HlsEngine::new(EngineConfig::default());
    let asset = engine.prepare(&p).await.unwrap();
    let track = asset
        .index
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Subtitle && t.codec.is_text_subtitle())
        .expect("a text subtitle track")
        .index;
    let streamed = match engine.webvtt_body(&asset, track).await.unwrap() {
        WebVttBody::Streaming(s) => {
            let parts: Vec<_> = s.collect().await;
            assert!(parts.len() > 2, "cues arrive as they are read: {}", parts.len());
            let mut doc = Vec::new();
            for p in parts {
                doc.extend_from_slice(&p.unwrap());
            }
            String::from_utf8(doc).unwrap()
        }
        WebVttBody::Whole(_) => panic!("nothing was extracted yet"),
    };
    assert!(streamed.starts_with("WEBVTT\n"));
    assert!(streamed.contains(" --> "));
    match engine.webvtt_body(&asset, track).await.unwrap() {
        WebVttBody::Whole(doc) => assert_eq!(&*doc, streamed),
        WebVttBody::Streaming(_) => panic!("the completed extraction is cached"),
    }
    let fresh = HlsEngine::new(EngineConfig::default());
    let asset2 = fresh.prepare(&p).await.unwrap();
    assert_eq!(&*fresh.webvtt(&asset2, track).await.unwrap(), streamed);
}

/// The in-process MPEG-TS index (phase-d/hls-perf) feeding the frame-exact
/// transcoder (phase-d/hls-correct): every run reads the source from its
/// keyframe through stdin instead of `-ss` into the TS file, which lands on
/// any PES and lost HEVC frames (a 4K HDR10 M2TS: 48 of 143 in its last
/// run; here an open-GOP HEVC TS made with ffmpeg). The transcode keeps
/// every source frame.
#[tokio::test]
async fn ts_hevc_transcode_keeps_every_frame() {
    let Some(p) = flux_test_media::clip(Clip::HevcAacTs) else {
        return;
    };
    if flux_test_media::ffprobe().is_none() {
        return;
    }
    let engine = HlsEngine::new(EngineConfig {
        video_lookahead: 0,
        ..Default::default()
    });
    let asset = engine.prepare(&p).await.unwrap();
    assert!(matches!(asset.index.layout, Layout::Ts(_)), "indexed in process");
    let prefs = PlanPrefs {
        allow_direct_play: false,
        ..Default::default()
    };
    let plan = engine.plan(&asset, &presets::legacy_h264(), &prefs);
    assert!(matches!(plan.video, VideoOp::Transcode { .. }));
    let mut all = engine
        .serve(&asset, &plan, HlsResource::Init(Rendition::Video))
        .await
        .unwrap()
        .body
        .into_bytes()
        .await
        .unwrap()
        .to_vec();
    for n in 0..engine.segment_count(&asset, &plan, &Rendition::Video) as u32 {
        let b = engine
            .media_segment(&asset, &plan, &Rendition::Video, n)
            .await
            .unwrap()
            .into_bytes()
            .await
            .unwrap();
        all.extend_from_slice(&b);
    }
    let out = out_dir("ts_transcode").join("video.mp4");
    std::fs::write(&out, &all).unwrap();
    let (got, want) = (video_packets(&out), video_packets(&p));
    eprintln!("{got} of {want} frames");
    assert_eq!(got, want);
}

/// First VCL NAL unit type of a media segment's first sample (5 = IDR).
fn first_vcl(seg: &[u8]) -> Option<u8> {
    let mut p = 0;
    while p + 8 <= seg.len() {
        let size = u32::from_be_bytes(seg[p..p + 4].try_into().ok()?) as usize;
        if &seg[p + 4..p + 8] == b"mdat" {
            let (mut o, end) = (p + 8, (p + size).min(seg.len()));
            while o + 5 <= end {
                let n = u32::from_be_bytes(seg[o..o + 4].try_into().ok()?) as usize;
                let t = seg[o + 4] & 0x1f;
                if t == 1 || t == 5 {
                    return Some(t);
                }
                o += 4 + n;
            }
            return None;
        }
        if size < 8 {
            return None;
        }
        p += size;
    }
    None
}

/// Found by the merge's smoke run (pre-existing; main flagged the samples
/// sync regardless): an AVI indexed by ffprobe whose source lacks a frame
/// (0.04 s) had the IDRs forced at its first run's piece boundaries one
/// sample before the pieces, so pieces 1–3 began on a P frame and a seek
/// into them decoded garbage until the next IDR. Pieces of predicted
/// length now end at the forced IDR nearby: every piece starts on an IDR,
/// and no frame is lost.
#[tokio::test]
async fn predicted_pieces_start_on_their_idr() {
    let Some(p) = avi() else {
        return;
    };
    let engine = HlsEngine::new(EngineConfig {
        video_lookahead: 0,
        ..Default::default()
    });
    let asset = engine.prepare(&p).await.unwrap();
    let plan = engine.plan(&asset, &presets::legacy_h264(), &PlanPrefs::default());
    assert!(matches!(plan.video, VideoOp::Transcode { .. }));
    let count = engine.segment_count(&asset, &plan, &Rendition::Video) as u32;
    let mut all = engine
        .serve(&asset, &plan, HlsResource::Init(Rendition::Video))
        .await
        .unwrap()
        .body
        .into_bytes()
        .await
        .unwrap()
        .to_vec();
    let mut starts = Vec::new();
    for n in 0..count {
        let b = engine
            .media_segment(&asset, &plan, &Rendition::Video, n)
            .await
            .unwrap()
            .into_bytes()
            .await
            .unwrap();
        starts.push(first_vcl(&b));
        all.extend_from_slice(&b);
    }
    eprintln!("first VCL NAL per piece: {starts:?}");
    assert!(starts.iter().all(|t| *t == Some(5)), "{starts:?}");
    let out = out_dir("predicted_idr").join("video.mp4");
    std::fs::write(&out, &all).unwrap();
    assert_eq!(video_packets(&out), video_packets(&p));
}
