//! Integration tests against the test corpus (`$FLUX_TEST_MEDIA`). Each
//! test skips (passes with a note) when its media is missing.

use std::path::PathBuf;

use flux_core::{AudioOp, Codec, DeliveryFormat, HdrFormat, TrackKind, VideoOp};
use flux_hls::index::{IndexMethod, Layout};
use flux_hls::playlist::{AudioKey, HlsResource, Rendition};
use flux_hls::{presets, Asset, EngineConfig, HlsEngine, PlanPrefs, PlaybackIndex};
use mp4_atom::{Any, Decode};

const SIX_S: i64 = 6_000_000_000;

/// A feature clip (`$FLUX_TEST_MEDIA/features/<name>`).
fn media(name: &str) -> Option<PathBuf> {
    flux_test_media::media(&format!("features/{name}"))
}

/// ffmpeg is installed (`FLUX_FFMPEG`, `PATH`).
fn have_ffmpeg() -> bool {
    EngineConfig::default().ffmpeg.is_file()
}

fn have_ffprobe() -> bool {
    EngineConfig::default().ffprobe.is_file()
}

/// Per-sample (duration, size, cts).
type Samples = Vec<(u32, u32, i32)>;

/// (tfdt, samples) of a media segment.
fn parse_segment(seg: &[u8]) -> (u64, Samples) {
    let mut cur = std::io::Cursor::new(seg);
    loop {
        match Any::decode(&mut cur).expect("box") {
            Any::Moof(m) => {
                let traf = &m.traf[0];
                let tfdt = traf.tfdt.as_ref().expect("tfdt").base_media_decode_time;
                let trun = &traf.trun[0];
                let samples = trun
                    .entries
                    .iter()
                    .map(|e| (e.duration.unwrap_or(0), e.size.unwrap_or(0), e.cts.unwrap_or(0)))
                    .collect();
                return (tfdt, samples);
            }
            _ => continue,
        }
    }
}

#[test]
fn mkv_index_from_cues() {
    let Some(p) = media("h264_multi_audio_subs.mkv") else {
        return;
    };
    let idx = PlaybackIndex::build(&p).unwrap();
    assert_eq!(idx.method, IndexMethod::Cues);
    assert!(idx.stats.reads <= 16, "index took {} reads", idx.stats.reads);
    assert_eq!(idx.tracks.len(), 5);
    let v = idx.video_track().unwrap();
    assert_eq!(
        (v.codec, v.timescale, v.frame_duration, v.reorder_depth),
        (Codec::H264, 24000, Some(1001), 2)
    );
    assert_eq!(v.codec_string.as_deref(), Some("avc1.640028"));
    assert_eq!(idx.keyframes.len(), 30);
    // D = reorder depth (2) × frame duration (1001/24000 s).
    assert_eq!(idx.presentation_offset_ns, 83_416_667);
    // AAC: ffmpeg writes CodecDelay = 1024 samples; the grid origin is −21.333 ms.
    let aac = idx.track(1).unwrap();
    assert_eq!(aac.codec, Codec::Aac);
    assert_eq!(aac.first_pts_ns, Some(-21_333_333));
    // AC-3: dac3 synthesized from the first sync frame.
    let ac3 = idx.track(2).unwrap();
    assert_eq!(ac3.synth_config.as_ref().map(|c| c.len()), Some(3));
    assert_eq!(ac3.channels, Some(6));
    // Round trip through the serialized cache forms.
    let back = PlaybackIndex::from_json(&idx.to_json().unwrap()).unwrap();
    assert_eq!(back, idx);
    assert_eq!(PlaybackIndex::from_bytes(&idx.to_bytes().unwrap()).unwrap(), idx);
}

#[test]
fn mp4_index_front_and_back_moov_agree() {
    let (Some(a), Some(b)) = (media("h264_aac_faststart.mp4"), media("h264_aac_moov_end.mp4")) else {
        return;
    };
    let ia = PlaybackIndex::build(&a).unwrap();
    let ib = PlaybackIndex::build(&b).unwrap();
    assert_eq!(ia.method, IndexMethod::Moov);
    assert_eq!(ia.keyframes.len(), ib.keyframes.len());
    assert_eq!(
        ia.keyframes.iter().map(|k| k.pts_ns).collect::<Vec<_>>(),
        ib.keyframes.iter().map(|k| k.pts_ns).collect::<Vec<_>>()
    );
    let sa = flux_hls::SegmentPlan::build(&ia, SIX_S).unwrap();
    assert_eq!(sa.len(), 10);
    // Compact binary form round-trips exactly and is much smaller than JSON.
    let bin = ia.to_bytes().unwrap();
    assert_eq!(PlaybackIndex::from_bytes(&bin).unwrap(), ia);
    assert!(bin.len() * 2 < ia.to_json().unwrap().len(), "{} bytes", bin.len());
}

/// Fragmented MP4 (moof/trun) and QuickTime AC-3 without dac3 are indexed
/// instead of failing (integration.md §4.2 #3).
#[test]
fn fragmented_mp4_and_quicktime_ac3_are_indexed() {
    let (Some(frag), Some(qt)) = (
        flux_test_media::media("probe-fixtures/fragmented.mp4"),
        flux_test_media::media("probe-fixtures/quicktime_ac3.mov"),
    ) else {
        return;
    };
    let f = PlaybackIndex::build(&frag).unwrap();
    assert_eq!(f.method, IndexMethod::Fragments);
    assert!(f.remuxable() && !f.keyframes.is_empty());
    let Layout::Mp4(l) = &f.layout else {
        panic!("layout")
    };
    assert!(l.tracks.iter().all(|t| t.sample_count > 0));
    assert_eq!(PlaybackIndex::from_bytes(&f.to_bytes().unwrap()).unwrap(), f);
    let q = PlaybackIndex::build(&qt).unwrap();
    let ac3 = q
        .tracks
        .iter()
        .find(|t| t.codec == Codec::Ac3)
        .expect("AC-3 track");
    assert_eq!(ac3.synth_config.as_ref().map(|c| c.len()), Some(3));
    assert_eq!(ac3.channels, Some(6));
}

#[test]
fn hdr10_from_vui_and_sei() {
    let Some(p) = media("hevc_main10_hdr10_eac3.mkv") else {
        return;
    };
    let asset = Asset::open(&p, SIX_S).unwrap();
    let v = asset.index.video_track().unwrap();
    let c = v.color.unwrap();
    assert_eq!((c.primaries, c.transfer, c.matrix), (Some(9), Some(16), Some(9)));
    let hdr = v.hdr.as_ref().unwrap();
    assert_eq!(hdr.format, HdrFormat::Hdr10);
    assert_eq!(hdr.cll.map(|c| (c.max_cll, c.max_fall)), Some((1000, 400)));
    assert_eq!(hdr.mastering.unwrap().max_luminance, 10_000_000);
    let init = asset.video_init().unwrap();
    for b in [b"hvc1", b"hvcC", b"colr", b"mdcv", b"clli"] {
        assert!(
            init.windows(4).any(|w| w == b),
            "init lacks {}",
            String::from_utf8_lossy(b)
        );
    }
    let engine = HlsEngine::new(EngineConfig::default());
    let plan = engine.plan(&asset, &presets::apple_tv_4k(), &PlanPrefs::default());
    let master = flux_hls::playlist::master_playlist(&asset, &plan);
    assert!(master.contains("VIDEO-RANGE=PQ"), "{master}");
    assert!(master.contains("hvc1.2.4.L150.90"));
    assert!(master.contains("FRAME-RATE=23.976"));
}

#[test]
fn video_segments_are_continuous_with_nonnegative_cts() {
    let Some(p) = media("h264_long_gop.mkv") else {
        return;
    };
    let asset = Asset::open(&p, SIX_S).unwrap();
    let mut next_dts = None;
    let mut frames = 0;
    // Request out of order: segments are pure functions of n.
    let order: Vec<usize> = (0..asset.segments.len()).rev().collect();
    let mut parsed: Vec<(usize, u64, Samples)> = order
        .iter()
        .map(|&n| {
            let (tfdt, s) = parse_segment(&asset.video_segment(n).unwrap().read_all().unwrap());
            (n, tfdt, s)
        })
        .collect();
    parsed.sort_by_key(|x| x.0);
    for (n, tfdt, samples) in parsed {
        if let Some(expected) = next_dts {
            assert_eq!(tfdt, expected, "segment {n} tfdt");
        }
        assert!(samples.iter().all(|s| s.2 >= 0), "negative CTS in segment {n}");
        next_dts = Some(tfdt + samples.iter().map(|s| s.0 as u64).sum::<u64>());
        frames += samples.len();
    }
    // 60 s at 24000/1001 fps.
    assert_eq!(frames, 1439);
}

#[test]
fn copied_audio_partitions_frames_exactly() {
    let Some(p) = media("h264_multi_audio_subs.mkv") else {
        return;
    };
    let asset = Asset::open(&p, SIX_S).unwrap();
    for (track, expected) in [(1u32, 2814usize), (2, 1875)] {
        let mut total = 0;
        let mut next = None;
        for n in 0..asset.segments.len() {
            let (tfdt, s) = parse_segment(&asset.audio_copy_segment(track, n).unwrap().read_all().unwrap());
            if let Some(e) = next {
                assert_eq!(tfdt, e, "track {track} segment {n}: gap/overlap");
            }
            next = Some(tfdt + s.iter().map(|x| x.0 as u64).sum::<u64>());
            total += s.len();
        }
        assert_eq!(total, expected, "track {track} frame count");
    }
}

#[test]
fn mp4_segments_match_sample_tables() {
    let Some(p) = media("hevc_main10_hdr10_hvc1.mp4") else {
        return;
    };
    let asset = Asset::open(&p, SIX_S).unwrap();
    let Layout::Mp4(l) = &asset.index.layout else {
        panic!("layout")
    };
    let vt = &l.tracks[asset.index.video.unwrap()];
    let mut frames = 0;
    for n in 0..asset.segments.len() {
        let (_, s) = parse_segment(&asset.video_segment(n).unwrap().read_all().unwrap());
        assert!(s.iter().all(|x| x.2 >= 0));
        frames += s.len();
    }
    assert_eq!(frames as u32, vt.sample_count);
}

#[test]
fn cueless_mkv_falls_back_to_cluster_scan() {
    let Some(src) = media("h264_flac.mkv") else { return };
    let out = std::env::temp_dir().join("flux-hls-nocues.mkv");
    let ok = std::process::Command::new(EngineConfig::default().ffmpeg)
        .args(["-v", "error", "-y", "-i"])
        .arg(&src)
        .args(["-t", "12", "-c", "copy", "-live", "1"])
        .arg(&out)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("skipping: ffmpeg unavailable");
        return;
    }
    let asset = Asset::open(&out, SIX_S).unwrap();
    assert_eq!(asset.index.method, IndexMethod::ClusterScan);
    assert!(asset.index.keyframes.len() >= 2);
    let mut frames = 0;
    for n in 0..asset.segments.len() {
        frames += parse_segment(&asset.video_segment(n).unwrap().read_all().unwrap())
            .1
            .len();
    }
    assert!(frames >= 280, "only {frames} frames");
    let _ = std::fs::remove_file(out);
}

#[test]
fn planner_on_real_indexes() {
    let (Some(thd), Some(mp4)) = (media("h264_truehd71.mkv"), media("h264_aac_faststart.mp4")) else {
        return;
    };
    let engine = HlsEngine::new(EngineConfig::default());
    let a = Asset::open(&thd, SIX_S).unwrap();
    let chrome = engine.plan(&a, &presets::chrome_desktop(), &PlanPrefs::default());
    assert_eq!(chrome.delivery, DeliveryFormat::HlsFmp4);
    assert!(matches!(chrome.video, VideoOp::Copy { .. }));
    assert!(matches!(
        chrome.audio[0].op,
        AudioOp::Transcode {
            codec: Codec::Aac,
            channels: 2,
            ..
        }
    ));
    let atv = engine.plan(&a, &presets::apple_tv_4k(), &PlanPrefs::default());
    assert!(matches!(
        atv.audio[0].op,
        AudioOp::Transcode {
            codec: Codec::Eac3,
            channels: 6,
            ..
        }
    ));
    let m = Asset::open(&mp4, SIX_S).unwrap();
    let direct = engine.plan(&m, &presets::chrome_desktop(), &PlanPrefs::default());
    assert!(direct.is_direct());
}

/// The web client lists H.264 Main and High (no Baseline entry). A
/// Constrained Baseline file (constraint_set1) must still be copied for it, as
/// the probe's plan says; the index used to call it "Baseline", so `/play`
/// video-transcoded long_2h_360p.mkv in Chrome while `/plan` said direct play.
#[test]
fn constrained_baseline_is_copied_for_main_high_clients() {
    let Some(p) = media("long_2h_360p.mkv") else {
        return;
    };
    let a = Asset::open(&p, SIX_S).unwrap();
    let v = &a.index.tracks[a.index.video.unwrap()];
    assert_eq!(v.profile.as_deref(), Some("Constrained Baseline"));
    let mut caps = presets::chrome_desktop();
    for c in caps.video.iter_mut().filter(|c| c.codec == Codec::H264) {
        c.profiles = vec!["high".into(), "main".into()];
    }
    let engine = HlsEngine::new(EngineConfig::default());
    let plan = engine.plan(&a, &caps, &PlanPrefs::default());
    assert!(matches!(plan.video, VideoOp::Copy { .. }), "{:?}", plan.reasons);
}

#[test]
fn audio_refs_name_the_playlist_renditions() {
    let Some(p) = media("h264_multi_audio_subs.mkv") else {
        return;
    };
    let engine = HlsEngine::new(EngineConfig::default());
    let asset = Asset::open(&p, SIX_S).unwrap();
    let plan = engine.plan(&asset, &presets::safari_macos(), &PlanPrefs::default());
    let master = flux_hls::playlist::master_playlist(&asset, &plan);
    let refs = flux_hls::playlist::audio_refs(&asset, &plan);
    assert_eq!(refs.len(), plan.audio.len());
    assert_eq!(refs.iter().filter(|r| r.default).count(), 1);
    assert!(refs[0].default);
    for r in &refs {
        let key = AudioKey::parse(&r.rendition).expect("a valid rendition key");
        assert_eq!(key.track, r.track);
        assert!(
            master.contains(&format!("NAME=\"{}\"", r.label)),
            "{} not in\n{master}",
            r.label
        );
        assert!(master.contains(&format!("URI=\"a/{}/index.m3u8\"", r.rendition)));
    }
    let direct = engine.plan(
        &Asset::open(media("h264_aac_faststart.mp4").unwrap(), SIX_S).unwrap(),
        &presets::chrome_desktop(),
        &PlanPrefs::default(),
    );
    assert!(direct.is_direct());
    assert!(flux_hls::playlist::audio_refs(&asset, &direct).is_empty());
}

#[tokio::test]
async fn transcoded_audio_segment_is_grid_aligned() {
    let Some(p) = media("h264_dts51.mkv") else { return };
    if !have_ffmpeg() {
        return;
    }
    let engine = HlsEngine::new(EngineConfig {
        audio_lookahead: 0,
        ..Default::default()
    });
    let asset = engine.prepare(&p).await.unwrap();
    let plan = engine.plan(
        &asset,
        &presets::chrome_desktop(),
        &PlanPrefs {
            allow_direct_play: false,
            ..Default::default()
        },
    );
    let key = AudioKey::from_plan(&plan.audio[0]);
    let r = Rendition::Audio(key);
    let mut prev_end = None;
    for n in [1u32, 2] {
        let seg = engine
            .media_segment(&asset, &plan, &r, n)
            .await
            .unwrap()
            .into_bytes()
            .await
            .unwrap();
        let (tfdt, s) = parse_segment(&seg);
        assert!(s.iter().all(|x| x.0 == 1024));
        if let Some(e) = prev_end {
            assert_eq!(tfdt, e, "segments {n}-1/{n} are not contiguous");
        }
        prev_end = Some(tfdt + s.len() as u64 * 1024);
    }
    let init = engine.init_segment(&asset, &plan, &r).await.unwrap();
    assert!(init.windows(4).any(|w| w == b"esds"));
}

#[tokio::test]
async fn webvtt_from_srt_and_ass() {
    let Some(p) = media("h264_multi_audio_subs.mkv") else {
        return;
    };
    let engine = HlsEngine::new(EngineConfig::default());
    let asset = engine.prepare(&p).await.unwrap();
    let srt = engine.webvtt(&asset, 3).await.unwrap();
    assert!(srt.contains("00:00:01.000 --> 00:00:04.000\nHello from flux."));
    assert!(srt.contains("<i>italics</i>"));
    let ass = engine.webvtt(&asset, 4).await.unwrap();
    assert!(ass.contains("A typeset sign"));
    assert!(!ass.contains("\\pos"));
    // Served through the plan routes.
    let plan = engine.plan(
        &asset,
        &presets::chrome_desktop(),
        &PlanPrefs {
            allow_direct_play: false,
            ..Default::default()
        },
    );
    let res = engine.serve(&asset, &plan, HlsResource::WebVtt(3)).await.unwrap();
    assert_eq!(res.content_type, "text/vtt; charset=utf-8");
    let subs: Vec<u32> = asset
        .index
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Subtitle)
        .map(|t| t.index)
        .collect();
    assert_eq!(subs, vec![3, 4]);
}

#[tokio::test]
async fn ts_without_index_uses_ffprobe_and_transcodes() {
    let Some(p) = media("mpeg2_ac3.ts") else { return };
    if !have_ffprobe() {
        return;
    }
    let engine = HlsEngine::new(EngineConfig {
        audio_lookahead: 0,
        video_lookahead: 0,
        ..Default::default()
    });
    let asset = engine.prepare(&p).await.unwrap();
    assert_eq!(asset.index.method, IndexMethod::HeaderProbe);
    assert!(!asset.index.to_probe_result().has_index);
    // No packet scan: segments on a time grid, no keyframe list.
    assert!(asset.index.keyframes.is_empty());
    let plan = engine.plan(&asset, &presets::chrome_desktop(), &PlanPrefs::default());
    assert!(matches!(plan.video, VideoOp::Transcode { .. }));
    // AC-3 can't be copied out of TS in-process: it becomes stereo AAC.
    assert!(plan.audio.iter().all(|a| matches!(
        a.op,
        AudioOp::Transcode {
            codec: Codec::Aac,
            ..
        }
    )));
    let init = engine
        .init_segment(&asset, &plan, &Rendition::Video)
        .await
        .unwrap();
    assert!(init.windows(4).any(|w| w == b"avcC"));
    // Remux segment 1 (6.006 s of 29.97 fps video) is cut into pieces of
    // about 2 s; together they hold its 180 frames.
    let pieces = flux_hls::vtranscode::pieces(&asset, 2_000_000_000);
    let of_seg1: Vec<u32> = (0..pieces.len() as u32)
        .filter(|&i| pieces[i as usize].seg == 1)
        .collect();
    assert_eq!(of_seg1.len(), 3);
    let mut frames = 0;
    for n in of_seg1 {
        let seg = engine
            .media_segment(&asset, &plan, &Rendition::Video, n)
            .await
            .unwrap()
            .into_bytes()
            .await
            .unwrap();
        let (_, s) = parse_segment(&seg);
        frames += s.len();
    }
    assert_eq!(frames, 180);
}

#[tokio::test]
async fn av1_on_safari_transcodes_video_exactly() {
    let Some(p) = media("av1_opus.mkv") else { return };
    if !have_ffmpeg() {
        return;
    }
    let engine = HlsEngine::new(EngineConfig {
        audio_lookahead: 0,
        video_lookahead: 0,
        ..Default::default()
    });
    let asset = engine.prepare(&p).await.unwrap();
    let plan = engine.plan(&asset, &presets::safari_macos(), &PlanPrefs::default());
    assert!(matches!(
        plan.video,
        VideoOp::Transcode {
            codec: Codec::H264,
            ..
        }
    ));
    let master = engine.master_playlist(&asset, &plan).unwrap();
    assert!(master.contains("avc1.640029"), "{master}");
    let mut next = None;
    let mut frames = 0;
    // Transcoded video is cut into ~2 s pieces (more than the remux segments).
    let count = engine.segment_count(&asset, &plan, &Rendition::Video);
    assert!(count > asset.segments.len(), "{count} pieces");
    for n in 0..count as u32 {
        let seg = engine
            .media_segment(&asset, &plan, &Rendition::Video, n)
            .await
            .unwrap()
            .into_bytes()
            .await
            .unwrap();
        let (tfdt, s) = parse_segment(&seg);
        if let Some(e) = next {
            assert_eq!(tfdt, e, "segment {n} not contiguous");
        }
        next = Some(tfdt + s.iter().map(|x| x.0 as u64).sum::<u64>());
        frames += s.len();
    }
    assert_eq!(frames, 719);
}

/// `plan_from_probe` (catalog probe only, no index) must equal
/// `HlsEngine::plan` (index) for every corpus file × client preset: the
/// 144 cases of flux-server's probe_vs_index check (a raw probe plan
/// differed in 23 of them).
#[test]
fn plan_from_probe_equals_engine_plan_on_corpus() {
    let dirs = [
        flux_test_media::media_dir("features"),
        flux_test_media::media_dir("playback-research"),
    ];
    let mut files: Vec<PathBuf> = Vec::new();
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(&d) else {
            eprintln!("skipping: {} not present", d.display());
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
            if matches!(ext, "mkv" | "mp4" | "webm" | "ts" | "m2ts" | "avi" | "mov") {
                files.push(p);
            }
        }
    }
    files.sort();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let engine = HlsEngine::new(EngineConfig::default());
    let prefs = PlanPrefs::default();
    let (mut cases, mut differ) = (0, Vec::new());
    for f in &files {
        let probe = flux_probe::probe_file(f).unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        let asset = rt.block_on(engine.prepare(f)).unwrap();
        for name in ["chrome", "safari", "firefox", "appletv", "androidtv", "legacy"] {
            let caps = presets::by_name(name).unwrap();
            let mut a = engine.plan(&asset, &caps, &prefs);
            let mut b = flux_hls::plan_from_probe(&probe, &caps, &prefs);
            // The CPU estimate scales with each side's bitrate estimate.
            a.est_cpu_ms_per_min = 0;
            b.est_cpu_ms_per_min = 0;
            cases += 1;
            if a != b {
                differ.push(format!("{} / {name}:\n  index {a:?}\n  probe {b:?}", f.display()));
            }
        }
    }
    eprintln!("{cases} cases, {} differ", differ.len());
    assert!(cases >= 144, "only {cases} cases");
    assert!(differ.is_empty(), "{}", differ.join("\n"));
}

/// I-frame playlist of a copied video: one single-sample keyframe segment
/// per source keyframe, on the copied rendition's timeline.
#[tokio::test]
async fn iframe_playlist_lists_every_keyframe() {
    let Some(p) = media("h264_multi_audio_subs.mkv") else {
        return;
    };
    let engine = HlsEngine::new(EngineConfig::default());
    let asset = engine.prepare(&p).await.unwrap();
    let plan = engine.plan(&asset, &presets::apple_tv_4k(), &PlanPrefs::default());
    assert!(matches!(plan.video, VideoOp::Copy { .. }));
    let master = engine.master_playlist(&asset, &plan).unwrap();
    assert!(master.contains("#EXT-X-I-FRAME-STREAM-INF:"), "{master}");
    assert!(master.contains("URI=\"v/iframes.m3u8\""));
    let pl = engine
        .serve(&asset, &plan, HlsResource::IFramePlaylist)
        .await
        .unwrap();
    let text = String::from_utf8(pl.body.into_bytes().await.unwrap().to_vec()).unwrap();
    assert!(text.contains("#EXT-X-I-FRAMES-ONLY"));
    assert_eq!(text.matches(".m4s").count(), asset.index.keyframes.len());
    let v = asset.index.video_track().unwrap();
    for n in [0u32, 1, 29] {
        let seg = engine
            .serve(&asset, &plan, HlsResource::IFrame(n))
            .await
            .unwrap()
            .body
            .into_bytes()
            .await
            .unwrap();
        let (tfdt, s) = parse_segment(&seg);
        assert_eq!(s.len(), 1);
        let kf = asset.index.keyframes[n as usize];
        let d = v.ns_to_ticks(asset.index.presentation_offset_ns);
        assert_eq!(tfdt as i64, v.snap_ticks(kf.pts_ns) + d, "keyframe {n}");
    }
    // Not offered for a video transcode.
    let legacy = engine.plan(&asset, &presets::legacy_h264(), &PlanPrefs::default());
    if matches!(legacy.video, VideoOp::Transcode { .. }) {
        assert!(engine
            .serve(&asset, &legacy, HlsResource::IFramePlaylist)
            .await
            .is_err());
    }
}
