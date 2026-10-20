//! Phase D regression tests (hls-correct): one or more per finding fixed in
//! flux-hls, ported from the review's repros. Inputs: the corpus
//! (`$FLUX_TEST_MEDIA`) plus small clips made with ffmpeg on first use
//! (`flux_test_media::clip`, the recipes of the review's
//! `make_scratch.sh`). Each test skips, with a note, when an input is
//! missing. Outputs go to `$TMPDIR/flux-tests/phase_d-<test>-…/`.
//! Transcodes use the detected encoder; `FLUX_VIDEO_ENCODER=libx264`
//! runs them through the software path.
//!
//! ```text
//! nice -n 19 cargo test -p flux-hls --test phase_d -j 3 -- --test-threads 2
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use flux_core::{Reason, VideoOp};
use flux_hls::playlist::{HlsResource, Rendition};
use flux_hls::{presets, EngineConfig, HlsEngine, PlanPrefs, PlaybackIndex};
use mp4_atom::{Any, Decode};

/// A corpus file (`media/<rel below $FLUX_TEST_MEDIA>`).
fn input(rel: &str) -> Option<PathBuf> {
    flux_test_media::media(rel.strip_prefix("media/").unwrap_or(rel))
}

/// A Phase D clip, made with ffmpeg on first use.
fn scratch(name: &str) -> Option<PathBuf> {
    flux_test_media::clip_named(name)
}

/// A fresh output directory for one test.
fn out_dir(test: &str) -> PathBuf {
    flux_test_media::scratch(&format!("phase_d-{test}"))
}

fn ffprobe() -> PathBuf {
    flux_test_media::ffprobe().unwrap_or_else(|| PathBuf::from("ffprobe"))
}

/// One sample of a `trun`: (duration, cts, sync).
type Entry = (u32, i32, bool);

/// (tfdt, samples) of the first traf of a media segment.
fn trun(seg: &[u8]) -> (u64, Vec<Entry>) {
    let mut cur = std::io::Cursor::new(seg);
    loop {
        match Any::decode(&mut cur).expect("box") {
            Any::Moof(m) => {
                let traf = &m.traf[0];
                let tfdt = traf.tfdt.as_ref().expect("tfdt").base_media_decode_time;
                let s = traf.trun[0]
                    .entries
                    .iter()
                    .map(|e| {
                        let flags = e.flags.unwrap_or(0);
                        (
                            e.duration.unwrap_or(0),
                            e.cts.unwrap_or(0),
                            flags & 0x0001_0000 == 0,
                        )
                    })
                    .collect();
                return (tfdt, s);
            }
            _ => continue,
        }
    }
}

/// Presentation times of every sample across segments, and whether decode
/// times increase strictly (within and across segments).
fn timeline(segs: &[Vec<u8>]) -> (Vec<i64>, bool) {
    let mut pts = Vec::new();
    let mut last_dts: Option<i64> = None;
    let mut increasing = true;
    for seg in segs {
        let (tfdt, s) = trun(seg);
        let mut dts = tfdt as i64;
        for (d, c, _) in s {
            if last_dts.is_some_and(|l| dts <= l) {
                increasing = false;
            }
            last_dts = Some(dts);
            pts.push(dts + c as i64);
            dts += d as i64;
        }
    }
    (pts, increasing)
}

fn duplicates(pts: &[i64]) -> usize {
    let mut seen: HashMap<i64, u32> = HashMap::new();
    for p in pts {
        *seen.entry(*p).or_default() += 1;
    }
    seen.values().map(|&c| (c - 1) as usize).sum()
}

/// Video packets of a file, by ffprobe.
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
    String::from_utf8_lossy(&o.stdout)
        .trim()
        .trim_end_matches(',')
        .parse()
        .unwrap_or(0)
}

/// Writes init + every video segment of a plan to `out/video.mp4`; returns
/// the path and the segments.
async fn write_video(
    engine: &HlsEngine,
    asset: &std::sync::Arc<flux_hls::Asset>,
    plan: &flux_core::PlaybackPlan,
    out: &Path,
) -> (PathBuf, Vec<Vec<u8>>) {
    let init = engine
        .serve(asset, plan, HlsResource::Init(Rendition::Video))
        .await
        .unwrap()
        .body
        .into_bytes()
        .await
        .unwrap();
    let mut all = init.to_vec();
    let mut segs = Vec::new();
    for n in 0..engine.segment_count(asset, plan, &Rendition::Video) as u32 {
        let b = engine
            .media_segment(asset, plan, &Rendition::Video, n)
            .await
            .unwrap()
            .into_bytes()
            .await
            .unwrap()
            .to_vec();
        all.extend_from_slice(&b);
        segs.push(b);
    }
    let p = out.join("video.mp4");
    std::fs::write(&p, &all).unwrap();
    (p, segs)
}

fn hls_only() -> PlanPrefs {
    PlanPrefs {
        allow_direct_play: false,
        ..Default::default()
    }
}

// ------------------------------------------------------------------ PB-1

/// PB-1: a Matroska track whose DefaultDuration (23.976) is longer than
/// some real frame intervals (a 59.94 stretch) keeps every frame's own
/// presentation time in the remux: 288/288 frames, no collisions, DTS
/// strictly increasing (was: 39 collisions, 58 non-increasing DTS, 231
/// frames decoded).
#[tokio::test]
async fn vfr_matroska_frames_keep_unique_timestamps() {
    let Some(p) = scratch("vfr_section.mkv") else {
        return;
    };
    let engine = HlsEngine::new(EngineConfig::default());
    let asset = engine.prepare(&p).await.unwrap();
    let plan = engine.plan(&asset, &presets::chrome_desktop(), &hls_only());
    assert!(matches!(plan.video, VideoOp::Copy { .. }));
    let out = out_dir("pb1_remux");
    let (file, segs) = write_video(&engine, &asset, &plan, &out).await;
    let (pts, increasing) = timeline(&segs);
    assert!(
        segs.iter().all(|s| trun(s).1.iter().all(|e| e.1 >= 0)),
        "negative CTS"
    );
    eprintln!("{} frames, {} colliding", pts.len(), duplicates(&pts));
    assert_eq!(duplicates(&pts), 0, "frames with identical PTS");
    assert!(increasing, "decode times must increase");
    assert_eq!(pts.len() as u64, video_packets(&p));
    assert_eq!(video_packets(&file), video_packets(&p));
}

/// PB-1 + PB-2 on the transcode path, and PB-NEW-2's case: the VFR HEVC
/// clip transcoded for an H.264-only client keeps every frame (was 230/288)
/// with distinct, increasing times, and every piece starts on an IDR.
#[tokio::test]
async fn vfr_transcode_keeps_every_frame() {
    let Some(p) = scratch("vfr_hevc_short.mkv") else {
        return;
    };
    let engine = HlsEngine::new(EngineConfig {
        video_lookahead: 0,
        ..Default::default()
    });
    let asset = engine.prepare(&p).await.unwrap();
    let plan = engine.plan(&asset, &presets::legacy_h264(), &hls_only());
    assert!(matches!(plan.video, VideoOp::Transcode { .. }));
    let out = out_dir("pb1_transcode");
    let (file, segs) = write_video(&engine, &asset, &plan, &out).await;
    let (pts, increasing) = timeline(&segs);
    assert_eq!(duplicates(&pts), 0);
    assert!(increasing);
    for (n, s) in segs.iter().enumerate() {
        assert!(trun(s).1[0].2, "piece {n} does not start with a sync sample");
    }
    let src = video_packets(&p);
    assert_eq!(pts.len() as u64, src, "frames out vs source");
    assert_eq!(video_packets(&file), src);
}

// ------------------------------------------------------------------ PB-2

/// PB-2: an MKV without DefaultDuration gets its frame rate from its
/// timestamps (30 fps, not an assumed 24), and its transcode has exactly the
/// source's 360 frames (was cut by `-frames:v round(span × 24)`).
#[tokio::test]
async fn transcode_without_default_duration_counts_real_frames() {
    let Some(p) = scratch("hevc30_nodd.mkv") else {
        return;
    };
    let idx = PlaybackIndex::build(&p).unwrap();
    let v = idx.video_track().unwrap();
    assert_eq!(v.default_duration_ns, None);
    assert_eq!(v.frame_duration, None);
    let r = v.frame_rate.expect("measured rate");
    assert_eq!((r.num, r.den), (30, 1), "{r:?}");
    let engine = HlsEngine::new(EngineConfig {
        video_lookahead: 0,
        ..Default::default()
    });
    let asset = engine.prepare(&p).await.unwrap();
    let plan = engine.plan(&asset, &presets::legacy_h264(), &hls_only());
    let out = out_dir("pb2");
    let (file, segs) = write_video(&engine, &asset, &plan, &out).await;
    let (pts, increasing) = timeline(&segs);
    assert!(increasing && duplicates(&pts) == 0);
    assert_eq!(video_packets(&file), video_packets(&p));
}

/// PB-2's copy path: the same clip remuxed (Chrome decodes HEVC) keeps
/// every composition offset ≥ 0. D used to leave out the reorder depth
/// when there was no DefaultDuration, so B-frames got negative offsets and
/// players shifted the video 46 ms against the audio.
#[tokio::test]
async fn copy_without_default_duration_keeps_offsets_non_negative() {
    let Some(p) = scratch("hevc30_nodd.mkv") else {
        return;
    };
    let engine = HlsEngine::new(EngineConfig::default());
    let asset = engine.prepare(&p).await.unwrap();
    let plan = engine.plan(&asset, &presets::chrome_desktop(), &hls_only());
    assert!(matches!(plan.video, VideoOp::Copy { .. }));
    let v = asset.index.video_track().unwrap();
    // reorder depth × 1/30 s (x265 ultrafast: 3 B-frames)
    assert!(asset.index.presentation_offset_ns >= v.reorder_depth as i64 * 33_333_333);
    for n in 0..asset.segments.len() {
        let seg = asset.video_segment(n).unwrap().read_all().unwrap();
        let (_, s) = trun(&seg);
        assert!(s.iter().all(|e| e.1 >= 0), "segment {n}: {s:?}");
    }
}

// ------------------------------------------------------------------ PB-NEW-1

/// PB-NEW-1: a Matroska file whose timestamps start at 1 s transcodes from
/// its first frame: 144/144 frames (was 120/144, video 1 s ahead).
#[tokio::test]
async fn transcode_honours_a_late_start_time() {
    let Some(p) = scratch("start1_hevc.mkv") else {
        return;
    };
    let engine = HlsEngine::new(EngineConfig {
        video_lookahead: 0,
        ..Default::default()
    });
    let asset = engine.prepare(&p).await.unwrap();
    assert!(asset.index.keyframes[0].pts_ns >= 1_000_000_000);
    let plan = engine.plan(&asset, &presets::legacy_h264(), &hls_only());
    let out = out_dir("pb_new1");
    let (file, segs) = write_video(&engine, &asset, &plan, &out).await;
    // Piece 0 starts at the source's first frame time (+ D).
    let (tfdt, s) = trun(&segs[0]);
    let first = tfdt as i64 + s[0].1 as i64;
    let v = asset.index.video_track().unwrap();
    let d = v.ns_to_ticks(asset.index.presentation_offset_ns);
    assert_eq!(first, v.snap_ticks(asset.index.keyframes[0].pts_ns) + d);
    assert_eq!(video_packets(&file), video_packets(&p));
}

// ------------------------------------------------------------------ PB-5

/// PB-5: requests the player abandons (a seek away) cancel their encoder
/// runs: no ffmpeg keeps running for them, and the permit is free for the
/// piece the player wants next.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoned_seeks_cancel_their_runs() {
    let Some(p) = input("media/features/av1_opus.mkv") else {
        return;
    };
    let pid = std::process::id();
    let ffmpeg_children = || {
        let o = Command::new("pgrep")
            .args(["-P", &pid.to_string(), "ffmpeg"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).lines().count()
    };
    let engine = HlsEngine::new(EngineConfig {
        video_lookahead: 0,
        audio_lookahead: 0,
        ..Default::default()
    });
    let asset = engine.prepare(&p).await.unwrap();
    let plan = engine.plan(&asset, &presets::safari_macos(), &PlanPrefs::default());
    assert!(matches!(plan.video, VideoOp::Transcode { .. }));
    // Init (+ piece 0's run) as a player starting at 0 would; let it finish.
    let _ = engine
        .serve(&asset, &plan, HlsResource::Init(Rendition::Video))
        .await
        .unwrap();
    for _ in 0..100 {
        if ffmpeg_children() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Scrubbing: three seeks, each request aborted after 50 ms.
    for n in [3u32, 6, 9] {
        let (e, a, pl) = (engine.clone(), asset.clone(), plan.clone());
        let h = tokio::spawn(async move { e.media_segment(&a, &pl, &Rendition::Video, n).await.map(|_| ()) });
        tokio::time::sleep(Duration::from_millis(50)).await;
        h.abort();
    }
    let t = Instant::now();
    let mut left = ffmpeg_children();
    while left > 0 && t.elapsed() < Duration::from_secs(2) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        left = ffmpeg_children();
    }
    eprintln!(
        "ffmpeg children {:.0} ms after the aborts: {left}",
        t.elapsed().as_secs_f64() * 1e3
    );
    assert_eq!(left, 0, "encoder runs kept going for abandoned requests");
    // The piece the player wants now is produced, with no queue behind.
    let t = Instant::now();
    let b = engine
        .media_segment(&asset, &plan, &Rendition::Video, 13)
        .await
        .unwrap();
    assert!(!b.is_empty());
    eprintln!(
        "piece 13 after the scrub: {:.0} ms",
        t.elapsed().as_secs_f64() * 1e3
    );
}

// ------------------------------------------------------------------ PB-7

/// PB-7: turning a subtitle on reads the track's blocks, not the file: the
/// cue points lead to them (ffmpeg indexes subtitle blocks), and the first
/// cue is handed over after a few KB. Measured with the source's I/O
/// counters (was: the whole 49 MB file before any cue).
#[tokio::test]
async fn subtitle_extraction_reads_little() {
    let Some(p) = input("media/features/h264_multi_audio_subs.mkv") else {
        return;
    };
    let engine = HlsEngine::new(EngineConfig::default());
    let asset = engine.prepare(&p).await.unwrap();
    let size = std::fs::metadata(&p).unwrap().len();
    let src = asset.source().clone();
    let before = src.io_stats().1;
    let mut first_at = None;
    let mut cues = 0;
    let how = asset
        .for_each_subtitle_block(3, &mut |_| {
            if first_at.is_none() {
                first_at = Some(src.io_stats().1 - before);
            }
            cues += 1;
            true
        })
        .unwrap();
    let total = src.io_stats().1 - before;
    eprintln!(
        "{how:?}: {cues} cues; first after {} bytes; {total} of {size} bytes read ({:.2} %)",
        first_at.unwrap_or(0),
        total as f64 * 100.0 / size as f64
    );
    assert!(cues > 0);
    assert!(total < size / 100, "read {total} of {size}");
    assert!(first_at.unwrap() < 1 << 20, "first cue after {first_at:?} bytes");
    // The WebVTT is what it was (same cues, same order).
    let vtt = engine.webvtt(&asset, 3).await.unwrap();
    assert!(vtt.contains("00:00:01.000 --> 00:00:04.000"), "{vtt}");
}

/// PB-7 without Cues for the subtitle track: the cluster walk reads block
/// headers only (other tracks' payloads skipped) and finds the same cues.
#[tokio::test]
async fn subtitle_walk_without_cues_skips_payloads() {
    let Some(p) = input("media/features/h264_multi_audio_subs.mkv") else {
        return;
    };
    let asset = flux_hls::Asset::open(&p, 6_000_000_000).unwrap();
    let with_cues = asset.subtitle_blocks(3).unwrap();
    // The same file with its Cues position forgotten: the walk path.
    let mut idx = asset.index.clone();
    let flux_hls::index::Layout::Matroska(l) = &mut idx.layout else {
        panic!("mkv")
    };
    l.cues_pos = None;
    let walked = flux_hls::Asset::with_index(idx, 6_000_000_000).unwrap();
    let before = walked.source().io_stats().1;
    let blocks = walked.subtitle_blocks(3).unwrap();
    let read = walked.source().io_stats().1 - before;
    let size = std::fs::metadata(&p).unwrap().len();
    eprintln!("walk: {} cues, {read} of {size} bytes", blocks.len());
    assert_eq!(blocks, with_cues);
    assert!(read < size / 4, "read {read} of {size}");
}

// ------------------------------------------------------------------ PB-9

fn moov_of(init: &[u8]) -> mp4_atom::Moov {
    let mut cur = std::io::Cursor::new(init);
    loop {
        if let Any::Moov(m) = Any::decode(&mut cur).expect("box") {
            return m;
        }
    }
}

/// PB-9: a portrait phone clip (90° tkhd matrix) keeps its matrix in the
/// remux init (was identity: sideways), and its transcode is turned upright
/// (portrait output size, no matrix needed).
#[tokio::test]
async fn display_rotation_survives_copy_and_transcode() {
    let (Some(h264), Some(hevc)) = (scratch("rot90_h264.mov"), scratch("rot90_hevc.mov")) else {
        return;
    };
    let engine = HlsEngine::new(EngineConfig {
        video_lookahead: 0,
        ..Default::default()
    });
    let asset = engine.prepare(&h264).await.unwrap();
    let v = asset.index.video_track().unwrap();
    // `ffmpeg -display_rotation 90` turns the picture 90° counter-clockwise
    // (ffprobe "rotation: 90"; an iPhone portrait clip says -90): 270° clockwise.
    assert_eq!(v.extra.rotation, 270, "{:?}", v.extra);
    let plan = engine.plan(&asset, &presets::chrome_desktop(), &hls_only());
    assert!(matches!(plan.video, VideoOp::Copy { .. }));
    let init = engine
        .serve(&asset, &plan, HlsResource::Init(Rendition::Video))
        .await
        .unwrap()
        .body
        .into_bytes()
        .await
        .unwrap();
    let moov = moov_of(&init);
    let m = &moov.trak[0].tkhd.matrix;
    assert_eq!(
        Some([m.a, m.b, m.u, m.c, m.d, m.v, m.x, m.y, m.w]),
        v.extra.matrix
    );

    let asset = engine.prepare(&hevc).await.unwrap();
    let plan = engine.plan(&asset, &presets::legacy_h264(), &hls_only());
    let VideoOp::Transcode { width, height, .. } = plan.video else {
        panic!("transcode")
    };
    let out = out_dir("pb9");
    let (file, _) = write_video(&engine, &asset, &plan, &out).await;
    let o = Command::new(ffprobe())
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height:stream_side_data=rotation",
            "-of",
            "json",
        ])
        .arg(&file)
        .output()
        .unwrap();
    let j: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let s = &j["streams"][0];
    eprintln!("planned {width}x{height}, output {s}");
    assert_eq!(
        (s["width"].as_u64(), s["height"].as_u64()),
        (Some(height as u64), Some(width as u64))
    );
    assert!(s["width"].as_u64() < s["height"].as_u64(), "portrait");
    assert!(
        s.get("side_data_list").is_none(),
        "upright pixels need no rotation"
    );
    let master = engine.master_playlist(&asset, &plan).unwrap();
    assert!(
        master.contains(&format!("RESOLUTION={height}x{width}")),
        "{master}"
    );
}

// ------------------------------------------------------------------ anamorphic

/// Anamorphic video (720x576 shown as 1024x576) keeps its shape: the index
/// knows the 64:45 pixel aspect (Matroska DisplayWidth or MP4 `pasp`), and
/// the init carries `pasp` and a 1024-wide `tkhd`.
#[tokio::test]
async fn anamorphic_video_keeps_its_shape() {
    for name in ["anamorphic.mkv", "anamorphic.mp4"] {
        let Some(p) = scratch(name) else { continue };
        let engine = HlsEngine::new(EngineConfig::default());
        let asset = engine.prepare(&p).await.unwrap();
        let v = asset.index.video_track().unwrap();
        assert_eq!(v.extra.sar, Some((64, 45)), "{name}");
        let probe = asset.index.to_probe_result();
        assert_eq!(
            probe
                .main_video()
                .and_then(|t| t.sample_aspect_ratio)
                .map(|r| (r.num, r.den)),
            Some((64, 45))
        );
        let init = asset.video_init().unwrap();
        let moov = moov_of(&init);
        assert_eq!(moov.trak[0].tkhd.width.integer(), 1024, "{name}");
        let pasp = match &moov.trak[0].mdia.minf.stbl.stsd.codecs[0] {
            mp4_atom::Codec::Avc1(e) => e.pasp.as_ref().map(|p| (p.h_spacing, p.v_spacing)),
            other => panic!("{other:?}"),
        };
        assert_eq!(pasp, Some((64, 45)), "{name}");
    }
}

// ------------------------------------------------------------------ PB-10

/// PB-10: BANDWIDTH counts what the variant sends. For a Matroska copy with
/// a TrueHD 7.1 track (never sent; AAC 192k is), AVERAGE-BANDWIDTH minus
/// the AAC rendition is within 25 % of the video bytes actually served
/// (it counted the TrueHD bytes too).
#[tokio::test]
async fn bandwidth_counts_only_sent_bytes() {
    let Some(p) = input("media/features/h264_truehd71.mkv") else {
        return;
    };
    let engine = HlsEngine::new(EngineConfig::default());
    let asset = engine.prepare(&p).await.unwrap();
    let plan = engine.plan(&asset, &presets::chrome_desktop(), &hls_only());
    let master = engine.master_playlist(&asset, &plan).unwrap();
    let line = master
        .lines()
        .find(|l| l.starts_with("#EXT-X-STREAM-INF"))
        .unwrap();
    let attr = |k: &str| -> u64 {
        line.split(',')
            .find_map(|kv| kv.strip_prefix(&format!("{k}=")))
            .and_then(|v| v.parse().ok())
            .unwrap()
    };
    let audio_bps = 192_000u64;
    let video_avg = attr("AVERAGE-BANDWIDTH") - audio_bps;
    let mut bytes = 0u64;
    for n in 0..asset.segments.len() as u32 {
        bytes += engine
            .media_segment(&asset, &plan, &Rendition::Video, n)
            .await
            .unwrap()
            .len();
    }
    let secs: f64 = (0..asset.segments.len())
        .map(|n| asset.segments.duration_secs(n))
        .sum();
    let real = (bytes as f64 * 8.0 / secs) as u64;
    eprintln!("signalled video {video_avg} b/s, served {real} b/s");
    assert!(
        (video_avg as f64) < real as f64 * 1.25 && (video_avg as f64) > real as f64 * 0.75,
        "signalled {video_avg} vs served {real}"
    );
}

// ------------------------------------------------------------------ PB-12

/// PB-12: a 1080p60 transcode is level 4.2, and CODECS says what the
/// encoder's SPS says (was avc1.640029 with -level 4.1 forced).
#[tokio::test]
async fn transcode_level_matches_size_and_rate() {
    let Some(p) = scratch("hevc1080p60.mkv") else {
        return;
    };
    let engine = HlsEngine::new(EngineConfig {
        video_lookahead: 0,
        ..Default::default()
    });
    let asset = engine.prepare(&p).await.unwrap();
    let plan = engine.plan(&asset, &presets::legacy_h264(), &hls_only());
    let master = engine.master_playlist(&asset, &plan).unwrap();
    assert!(master.contains("CODECS=\"avc1.64002a"), "{master}");
    let init = engine
        .serve(&asset, &plan, HlsResource::Init(Rendition::Video))
        .await
        .unwrap()
        .body
        .into_bytes()
        .await
        .unwrap();
    let moov = moov_of(&init);
    let mp4_atom::Codec::Avc1(avc1) = &moov.trak[0].mdia.minf.stbl.stsd.codecs[0] else {
        panic!("avc1")
    };
    assert_eq!(avc1.avcc.avc_level_indication, 0x2a);
}

// ------------------------------------------------------------------ EXTINF

/// The last segment ends with the video (mkv_fonts.mkv: 2 s of video, a
/// subtitle to 4 s): the playlist's total equals the video, not the
/// container duration.
#[tokio::test]
async fn last_extinf_is_the_video_end() {
    let Some(p) = input("media/probe-fixtures/mkv_fonts.mkv") else {
        return;
    };
    let asset = flux_hls::Asset::open(&p, 6_000_000_000).unwrap();
    let total: f64 = (0..asset.segments.len())
        .map(|n| asset.segments.duration_secs(n))
        .sum();
    let v = asset.index.video_track().unwrap();
    let end = v.extra.end_ns.expect("video end") as f64 / 1e9;
    let first = asset.index.keyframes[0].pts_ns as f64 / 1e9;
    eprintln!(
        "container {:.3} s, video end {end:.3} s, playlist {total:.3} s",
        asset.index.duration_secs()
    );
    assert!((total - (end - first)).abs() < 0.002, "{total} vs {end}");
    assert!(
        asset.index.duration_secs() - total > 1.0,
        "the container outlasts the video here"
    );
    // MP4 knows its end exactly too.
    if let Some(mp4) = input("media/features/h264_aac_faststart.mp4") {
        let a = flux_hls::Asset::open(&mp4, 6_000_000_000).unwrap();
        let v = a.index.video_track().unwrap();
        let frames = 1439.0;
        let fd = v.frame_rate.map(|r| r.den as f64 / r.num as f64).unwrap();
        let total: f64 = (0..a.segments.len()).map(|n| a.segments.duration_secs(n)).sum();
        assert!((total - frames * fd).abs() < 0.002, "{total}");
    }
}

// ------------------------------------------------------------------ AVI

/// The AVI plan no longer explains itself with "repackages the streams (no
/// quality loss)" next to "transcodes it".
#[tokio::test]
async fn avi_plan_reasons_agree() {
    let Some(p) = input("media/probe-fixtures/h264_ac3.avi") else {
        return;
    };
    let engine = HlsEngine::new(EngineConfig::default());
    let asset = engine.prepare(&p).await.unwrap();
    for caps in [
        presets::chrome_desktop(),
        presets::safari_macos(),
        presets::legacy_h264(),
    ] {
        let plan = engine.plan(&asset, &caps, &PlanPrefs::default());
        assert!(matches!(plan.video, VideoOp::Transcode { .. }));
        assert!(
            !plan
                .reasons
                .iter()
                .any(|r| matches!(r, Reason::ContainerUnsupported { .. })),
            "{}: {:?}",
            caps.client,
            plan.explain()
        );
    }
}

// ------------------------------------------------------------------ ROB-1

/// ROB-1: a hung ffprobe can't hold a play request or a shutdown: the scan
/// is killed after its stall limit, with everything it spawned (the fake
/// ffprobe is a shell whose `sleep` child holds the pipes), and
/// `kill_running` ends a scan at once.
#[test]
fn hung_ffprobe_index_scan_is_killed() {
    use flux_hls::index::external::{build_with_limits, kill_running, ScanLimits};
    let Some(ts) = input("media/seeds/seed_h264_ts.ts") else {
        return;
    };
    let dir = out_dir("rob1");
    let fake = dir.join("ffprobe");
    // No `exec`: the sleep is a grandchild that only a group kill reaches.
    std::fs::write(&fake, "#!/bin/sh\nsleep 4242\n").unwrap();
    Command::new("chmod").arg("+x").arg(&fake).status().unwrap();
    let id = flux_hls::source::Source::open(&ts).unwrap().id().clone();
    let sleepers = || {
        let o = Command::new("pgrep").args(["-f", "sleep 4242"]).output().unwrap();
        String::from_utf8_lossy(&o.stdout).lines().count()
    };
    let limits = ScanLimits {
        stall: Duration::from_millis(800),
        ..Default::default()
    };
    let t = Instant::now();
    let err = build_with_limits(&ts, &fake, id.clone(), &limits).unwrap_err();
    let took = t.elapsed();
    eprintln!("{err} after {took:?}");
    assert!(took < Duration::from_secs(5), "{took:?}");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(sleepers(), 0, "the fake ffprobe's child outlived the scan");

    // Shutdown: kill_running ends a scan with generous limits right away.
    let (fake2, ts2) = (fake.clone(), ts.clone());
    let h = std::thread::spawn(move || {
        let t = Instant::now();
        let r = build_with_limits(&ts2, &fake2, id, &ScanLimits::default());
        (r.map(|_| ()), t.elapsed())
    });
    std::thread::sleep(Duration::from_millis(500));
    kill_running();
    let (r, took) = h.join().unwrap();
    eprintln!("after kill_running: {r:?} in {took:?}");
    assert!(r.is_err());
    assert!(took < Duration::from_secs(3), "{took:?}");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(sleepers(), 0);
}

// ------------------------------------------------------------------ ROB-2

/// (offset, size, header) of the video `stbl` children of an MP4 in memory.
fn video_stbl(buf: &[u8]) -> HashMap<String, (usize, usize, usize)> {
    fn boxes(b: &[u8], start: usize, end: usize) -> Vec<(usize, usize, [u8; 4], usize)> {
        let mut out = Vec::new();
        let mut p = start;
        while p + 8 <= end {
            let mut size = u32::from_be_bytes(b[p..p + 4].try_into().unwrap()) as usize;
            let kind: [u8; 4] = b[p + 4..p + 8].try_into().unwrap();
            let mut hdr = 8;
            if size == 1 {
                size = u64::from_be_bytes(b[p + 8..p + 16].try_into().unwrap()) as usize;
                hdr = 16;
            } else if size == 0 {
                size = end - p;
            }
            if size < hdr {
                break;
            }
            out.push((p, size, kind, hdr));
            p += size;
        }
        out
    }
    for (p, size, kind, hdr) in boxes(buf, 0, buf.len()) {
        if &kind != b"moov" {
            continue;
        }
        for (tp, tsize, tkind, thdr) in boxes(buf, p + hdr, p + size) {
            if &tkind != b"trak" {
                continue;
            }
            let mdia = boxes(buf, tp + thdr, tp + tsize)
                .into_iter()
                .find(|b| &b.2 == b"mdia")
                .unwrap();
            let kids = boxes(buf, mdia.0 + mdia.3, mdia.0 + mdia.1);
            let hdlr = kids.iter().find(|b| &b.2 == b"hdlr").unwrap();
            if &buf[hdlr.0 + hdlr.3 + 8..hdlr.0 + hdlr.3 + 12] != b"vide" {
                continue;
            }
            let minf = kids.iter().find(|b| &b.2 == b"minf").unwrap();
            let stbl = boxes(buf, minf.0 + minf.3, minf.0 + minf.1)
                .into_iter()
                .find(|b| &b.2 == b"stbl")
                .unwrap();
            return boxes(buf, stbl.0 + stbl.3, stbl.0 + stbl.1)
                .into_iter()
                .map(|(bp, bs, k, bh)| (String::from_utf8_lossy(&k).to_string(), (bp, bs, bh)))
                .collect();
        }
    }
    panic!("no video trak")
}

/// ROB-2: a 1.6 MB MP4 whose `stsz` claims 20 M identical samples fails
/// to index cleanly (it allocated ~580 MB per play); one whose `ctts` runs
/// claim u32::MAX samples indexes in milliseconds (it spun ~5 s).
#[test]
fn mp4_sample_table_bombs() {
    let Some(seed) = input("media/seeds/seed_h264_mp4.mp4") else {
        return;
    };
    let dir = out_dir("rob2");
    let orig = std::fs::read(&seed).unwrap();

    // stsz → sample_size = 1000, sample_count = 20 M (+ a free box).
    let mut buf = orig.clone();
    let (p, s, h) = video_stbl(&buf)["stsz"];
    let mut new = Vec::new();
    new.extend_from_slice(&20u32.to_be_bytes());
    new.extend_from_slice(b"stsz");
    new.extend_from_slice(&[0, 0, 0, 0]);
    new.extend_from_slice(&1000u32.to_be_bytes());
    new.extend_from_slice(&20_000_000u32.to_be_bytes());
    new.extend_from_slice(&((s - 20) as u32).to_be_bytes());
    new.extend_from_slice(b"free");
    new.resize(s, 0);
    let _ = h;
    buf[p..p + s].copy_from_slice(&new);
    let bomb = dir.join("Stsz Bomb (2010).mp4");
    std::fs::write(&bomb, &buf).unwrap();
    let r = flux_hls::Asset::open(&bomb, 6_000_000_000);
    let e = r.expect_err("must not index").to_string();
    eprintln!("stsz bomb: {e}");

    // ctts → every entry's sample_count = 0xFFFFFFFF.
    let mut buf = orig;
    let st = video_stbl(&buf);
    if let Some(&(p, _, h)) = st.get("ctts") {
        let entries = u32::from_be_bytes(buf[p + h + 4..p + h + 8].try_into().unwrap()) as usize;
        for i in 0..entries {
            let o = p + h + 8 + 8 * i;
            buf[o..o + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        }
        let bomb = dir.join("Ctts Bomb (2010).mp4");
        std::fs::write(&bomb, &buf).unwrap();
        let t = Instant::now();
        let a = flux_hls::Asset::open(&bomb, 6_000_000_000);
        let took = t.elapsed();
        eprintln!(
            "ctts bomb: {:?} in {took:?}",
            a.as_ref().map(|a| a.segments.len())
        );
        assert!(took < Duration::from_millis(500), "{took:?}");
    }

    // Reverify: stsz → sample_size = 1, sample_count = the file's length.
    // Within "a sample is at least a byte", but its one segment cost ~140
    // bytes of memory per sample on every request (~250 MB here).
    let mut buf = std::fs::read(&seed).unwrap();
    let len = buf.len() as u32;
    let (p, s, _) = video_stbl(&buf)["stsz"];
    let mut new = Vec::new();
    new.extend_from_slice(&20u32.to_be_bytes());
    new.extend_from_slice(b"stsz");
    new.extend_from_slice(&[0, 0, 0, 0]);
    new.extend_from_slice(&1u32.to_be_bytes());
    new.extend_from_slice(&len.to_be_bytes());
    new.extend_from_slice(&((s - 20) as u32).to_be_bytes());
    new.extend_from_slice(b"free");
    new.resize(s, 0);
    buf[p..p + s].copy_from_slice(&new);
    let bomb = dir.join("Stsz One (2016).mp4");
    std::fs::write(&bomb, &buf).unwrap();
    let e = flux_hls::Asset::open(&bomb, 6_000_000_000)
        .expect_err("must not index")
        .to_string();
    eprintln!("1-byte stsz: {e}");
}

// ------------------------------------------------------------------ ROB-V2

/// ROB-V2: a fragmented MP4 whose first `moof` says size 0 ("to the end of
/// the file") is rejected instead of reading the rest of a 128 MiB file
/// into memory.
#[test]
fn moof_size_zero_is_rejected() {
    let Some(frag) = input("media/probe-fixtures/fragmented.mp4") else {
        return;
    };
    let dir = out_dir("rob_v2");
    let mut buf = std::fs::read(&frag).unwrap();
    // The first moof after the moov.
    let at = buf.windows(4).position(|w| w == b"moof").expect("moof") - 4;
    buf[at..at + 4].copy_from_slice(&0u32.to_be_bytes());
    let p = dir.join("Moof Zero (2014).mp4");
    std::fs::write(&p, &buf).unwrap();
    // Sparse: 128 MiB on paper, a few MB on disk.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&p)
        .unwrap()
        .set_len(128 << 20)
        .unwrap();
    let e = PlaybackIndex::build(&p)
        .expect_err("must be rejected")
        .to_string();
    eprintln!("{e}");
    assert!(e.contains("moof"), "{e}");
}

// ------------------------------------------------------------------ ROB-5

/// ROB-5: a faststart MP4 cut after 64 KiB (its `moov` still describes the
/// whole movie) fails when the segment is built, before any response, not
/// after a 200 with a Content-Length the body can't fill.
#[tokio::test]
async fn truncated_mp4_fails_before_the_response() {
    let Some(src) = input("media/probe-fixtures/mp4_named.mkv") else {
        return;
    };
    let dir = out_dir("rob5");
    let bytes = std::fs::read(&src).unwrap();
    let p = dir.join("trunc64k.mkv");
    std::fs::write(&p, &bytes[..64 << 10]).unwrap();
    let engine = HlsEngine::new(EngineConfig::default());
    let asset = engine.prepare(&p).await.unwrap();
    let plan = engine.plan(&asset, &presets::chrome_desktop(), &hls_only());
    let r = engine.media_segment(&asset, &plan, &Rendition::Video, 0).await;
    let e = r
        .expect_err("segment 0 lies past the end of the file")
        .to_string();
    eprintln!("{e}");
    assert!(e.contains("truncated") || e.contains("past the end"), "{e}");
}

/// ROB-5, found by the re-verification: a file that shrinks after its
/// asset was prepared (overwritten in place while its descriptor is open)
/// also fails before the response. The ranges were checked against the
/// length recorded at open, so the body failed mid-stream after a 200.
#[tokio::test]
async fn a_file_that_shrinks_after_prepare_fails_before_the_response() {
    let Some(src) = input("media/features/h264_aac_faststart.mp4") else {
        return;
    };
    let dir = out_dir("rob5-shrink");
    let p = dir.join("Shrinks (2003).mp4");
    let _ = std::fs::remove_file(&p);
    flux_test_media::clone_file(&src, &p).unwrap();
    let engine = HlsEngine::new(EngineConfig::default());
    let asset = engine.prepare(&p).await.unwrap();
    let plan = engine.plan(&asset, &presets::chrome_desktop(), &hls_only());
    let last = asset.segments.len() as u32 - 1;
    engine
        .media_segment(&asset, &plan, &Rendition::Video, 0)
        .await
        .expect("segment 0 before the overwrite")
        .into_bytes()
        .await
        .unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&p)
        .unwrap()
        .set_len(4 << 20)
        .unwrap();
    let e = engine
        .media_segment(&asset, &plan, &Rendition::Video, last)
        .await
        .expect_err("the last segment now lies past the end of the file")
        .to_string();
    eprintln!("{e}");
    assert!(e.contains("shrank"), "{e}");
}
