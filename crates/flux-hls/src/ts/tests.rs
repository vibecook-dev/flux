//! MPEG-TS remux against the read-only corpus (`$FLUX_TEST_MEDIA`; skipped
//! when it is not set), plus synthetic files derived from it in `$TMPDIR`
//! (a 33-bit timestamp wrap and timestamp discontinuities) and a clip made
//! with ffmpeg (`flux_test_media::clip`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use flux_core::{AudioOp, Codec, Container, TrackKind, VideoOp};
use mp4_atom::{Any, Decode};

use super::packet::{self, Framing};
use super::pes::{self, WRAP};
use crate::index::{IndexMethod, Layout, PlaybackIndex};
use crate::playlist::{AudioKey, AudioRecipe, Rendition};
use crate::{presets, Asset, EngineConfig, HlsEngine, PlanPrefs};

fn media(rel: &str) -> Option<PathBuf> {
    flux_test_media::media(rel)
}

fn ffmpeg() -> bool {
    Path::new(&EngineConfig::default().ffmpeg).is_file()
}

/// (tfdt, [(duration, size, cts, sync)]) of a media segment.
fn parse_segment(seg: &[u8]) -> (u64, Vec<(u32, u32, i32, bool)>) {
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
                            e.size.unwrap_or(0),
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

async fn prepared(path: &Path) -> (HlsEngine, Arc<Asset>) {
    let engine = HlsEngine::new(EngineConfig {
        audio_lookahead: 0,
        video_lookahead: 0,
        ..Default::default()
    });
    let idx = PlaybackIndex::build(path).unwrap();
    let asset = engine.prepare_with_index(idx).await.unwrap();
    (engine, asset)
}

/// Every video segment: frames, continuity of decode times, CTS ≥ 0, a
/// sync sample first. Returns the frame count.
async fn check_video(engine: &HlsEngine, asset: &Arc<Asset>, plan: &flux_core::PlaybackPlan) -> usize {
    let mut next = None;
    let mut frames = 0;
    for n in 0..asset.segments.len() as u32 {
        let seg = engine
            .media_segment(asset, plan, &Rendition::Video, n)
            .await
            .unwrap()
            .into_bytes()
            .await
            .unwrap();
        let (tfdt, s) = parse_segment(&seg);
        if let Some(e) = next {
            assert_eq!(tfdt, e, "video segment {n} not contiguous");
        }
        assert!(s[0].3, "segment {n} starts without a sync sample");
        assert!(s.iter().all(|x| x.2 >= 0), "negative CTS in segment {n}");
        next = Some(tfdt + s.iter().map(|x| x.0 as u64).sum::<u64>());
        frames += s.len();
    }
    frames
}

/// Copied audio: (frames, the largest gap/overlap between segments in ticks).
async fn check_audio(
    engine: &HlsEngine,
    asset: &Arc<Asset>,
    plan: &flux_core::PlaybackPlan,
    k: AudioKey,
) -> (usize, i64) {
    let mut next: Option<u64> = None;
    let (mut frames, mut worst) = (0, 0i64);
    for n in 0..asset.segments.len() as u32 {
        let seg = engine
            .media_segment(asset, plan, &Rendition::Audio(k), n)
            .await
            .unwrap()
            .into_bytes()
            .await
            .unwrap();
        let (tfdt, s) = parse_segment(&seg);
        if let Some(e) = next {
            worst = worst.max((tfdt as i64 - e as i64).abs());
        }
        next = Some(tfdt + s.iter().map(|x| x.0 as u64).sum::<u64>());
        frames += s.len();
    }
    (frames, worst)
}

#[tokio::test]
async fn h264_aac_ts_is_indexed_and_remuxed() {
    let Some(p) = media("seeds/seed_h264_ts.ts") else {
        return;
    };
    let (engine, asset) = prepared(&p).await;
    let idx = &asset.index;
    assert_eq!(
        (idx.method, idx.container),
        (IndexMethod::TsScan, Container::MpegTs)
    );
    assert!(matches!(idx.layout, Layout::Ts(_)));
    let v = idx.video_track().unwrap();
    assert_eq!((v.codec, v.width, v.height), (Codec::H264, Some(1280), Some(720)));
    assert_eq!(v.codec_string.as_deref(), Some("avc1.64001f"));
    assert_eq!(v.frame_rate.map(|r| (r.num, r.den)), Some((24, 1)));
    assert_eq!(v.reorder_depth, 2);
    // ffprobe: video start 1.483333, audio (format) start 1.462.
    assert_eq!(v.first_pts_ns, Some(21_333_333));
    let a = idx.track(1).unwrap();
    assert_eq!(
        (a.codec, a.sample_rate, a.channels, a.first_pts_ns),
        (Codec::Aac, Some(48_000), Some(1), Some(0))
    );
    assert_eq!(a.codec_private, vec![0x11, 0x88]); // AAC-LC, 48 kHz, mono
    assert_eq!(idx.keyframes.len(), 3);
    assert_eq!(idx.presentation_offset_ns, 83_333_333);
    assert!(
        (idx.duration_ns - 6_021_333_333).abs() < 1_000_000,
        "{}",
        idx.duration_ns
    );
    // The serialized forms keep the TS layout.
    assert_eq!(&PlaybackIndex::from_json(&idx.to_json().unwrap()).unwrap(), idx);
    assert_eq!(&PlaybackIndex::from_bytes(&idx.to_bytes().unwrap()).unwrap(), idx);

    // Chrome: video and AAC copied (was: NoIndex → video transcode).
    let plan = engine.plan(&asset, &presets::chrome_desktop(), &PlanPrefs::default());
    assert!(matches!(plan.video, VideoOp::Copy { .. }), "{plan:?}");
    assert_eq!(plan.audio[0].op, AudioOp::Copy);
    assert!(!plan
        .reasons
        .iter()
        .any(|r| matches!(r, flux_core::Reason::NoIndex { .. })));
    assert_eq!(check_video(&engine, &asset, &plan).await, 144);
    let k = AudioKey::from_plan(&plan.audio[0]);
    let all = asset.audio_frames(1, i64::MIN, i64::MAX).unwrap();
    let (frames, worst) = check_audio(&engine, &asset, &plan, k).await;
    assert_eq!(frames, all.len(), "every AAC frame exactly once");
    assert!(worst <= 1, "audio segments {worst} ticks apart");
    // Init segments carry the records built from the stream.
    let init = engine
        .init_segment(&asset, &plan, &Rendition::Video)
        .await
        .unwrap();
    assert!(init.windows(4).any(|w| w == b"avcC"));
    // I-frames: keyframe 1 alone.
    let kf = crate::ts::keyframe_segment(&asset, ts_layout(&asset), 1)
        .unwrap()
        .read_all()
        .unwrap();
    let (_, s) = parse_segment(&kf);
    assert_eq!(s.len(), 1);
}

fn ts_layout(asset: &Asset) -> &super::TsLayout {
    match &asset.index.layout {
        Layout::Ts(l) => l,
        _ => panic!("not a TS index"),
    }
}

#[tokio::test]
async fn m2ts_hevc_mp2_eac3_and_lpcm_plans() {
    let Some(p) = media("probe-fixtures/hevc_mp2_eac3.ts") else {
        return;
    };
    let (engine, asset) = prepared(&p).await;
    let v = asset.index.video_track().unwrap();
    assert_eq!(v.codec_string.as_deref(), Some("hvc1.1.6.L93.90"));
    let kinds: Vec<(Codec, TrackKind)> = asset.index.tracks.iter().map(|t| (t.codec, t.kind)).collect();
    assert_eq!(
        kinds,
        vec![
            (Codec::Hevc, TrackKind::Video),
            (Codec::Mp2, TrackKind::Audio),
            (Codec::Eac3, TrackKind::Audio)
        ]
    );
    // MP2 frames are demuxed here (1152 samples each, increasing times).
    let mp2 = asset.audio_frames(1, i64::MIN, i64::MAX).unwrap();
    assert!(mp2.len() > 100);
    assert!(mp2.windows(2).all(|w| w[1].pts_ns - w[0].pts_ns == 24_000_000));
    let tv = engine.plan(&asset, &presets::apple_tv_4k(), &PlanPrefs::default());
    assert!(matches!(tv.video, VideoOp::Copy { .. }));
    // E-AC-3 copies to the Apple TV (dec3 from the first access unit).
    assert!(
        tv.audio.iter().any(|a| a.track == 2 && a.op == AudioOp::Copy),
        "{tv:?}"
    );
    assert!(asset.audio_copy_init(2).unwrap().windows(4).any(|w| w == b"dec3"));
    // MP2 can't be copied: AAC.
    assert!(tv.audio.iter().all(|a| a.track != 1 || a.op != AudioOp::Copy));

    let Some(p) = media("probe-fixtures/h264_lpcm.m2ts") else {
        return;
    };
    let (engine, asset) = prepared(&p).await;
    assert_eq!(asset.index.container, Container::M2ts);
    let plan = engine.plan(&asset, &presets::chrome_desktop(), &PlanPrefs::default());
    assert!(matches!(plan.video, VideoOp::Copy { .. }));
    // Blu-ray LPCM is read by ffmpeg (by PID) and transcoded.
    let k = AudioKey::from_plan(&plan.audio[0]);
    assert!(matches!(k.recipe, AudioRecipe::Aac { .. }), "{plan:?}");
    if ffmpeg() {
        let seg = engine
            .media_segment(&asset, &plan, &Rendition::Audio(k), 0)
            .await
            .unwrap()
            .into_bytes()
            .await
            .unwrap();
        let (_, s) = parse_segment(&seg);
        assert!(s.len() > 100 && s.iter().all(|x| x.0 == 1024));
    }
}

/// `plan_from_probe` (the catalog's flux-probe result) equals the engine's
/// plan for every TS file of the corpus and every preset.
#[test]
fn plan_from_probe_agrees_on_ts() {
    let files = [
        "seeds/seed_h264_ts.ts",
        "probe-fixtures/hevc_mp2_eac3.ts",
        "probe-fixtures/h264_lpcm.m2ts",
        "features/h264_aac.m2ts",
    ];
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let engine = HlsEngine::new(EngineConfig::default());
    let prefs = PlanPrefs::default();
    for f in files {
        let Some(p) = media(f) else { return };
        let probe = flux_probe::probe_file(&p).unwrap();
        let asset = rt.block_on(engine.prepare(&p)).unwrap();
        // Same tracks, same numbering.
        let ids: Vec<(u32, u64, Codec)> = probe.tracks.iter().map(|t| (t.index, t.id, t.codec)).collect();
        let ours: Vec<(u32, u64, Codec)> = asset
            .index
            .tracks
            .iter()
            .map(|t| (t.index, t.id, t.codec))
            .collect();
        assert_eq!(ids, ours, "{f}");
        for name in ["chrome", "safari", "firefox", "appletv", "androidtv", "legacy"] {
            let caps = presets::by_name(name).unwrap();
            let mut a = engine.plan(&asset, &caps, &prefs);
            let mut b = crate::plan_from_probe(&probe, &caps, &prefs);
            a.est_cpu_ms_per_min = 0;
            b.est_cpu_ms_per_min = 0;
            assert_eq!(a, b, "{f} / {name}");
        }
    }
}

/// A TS the remux rejects (MPEG-2 video) still plays through ffprobe and
/// ffmpeg, and the planner knows not to plan a copy from it.
#[tokio::test]
async fn rejected_ts_falls_back_to_the_ffmpeg_paths() {
    let Some(p) = media("features/mpeg2_ac3.ts") else {
        return;
    };
    let cfg = EngineConfig::default();
    if !cfg.ffprobe.exists() {
        return;
    }
    let src = crate::source::Source::open(&p).unwrap();
    let idx = PlaybackIndex::build_with_fallback(&src, Some(&cfg.ffprobe)).unwrap();
    assert_eq!(idx.method, IndexMethod::HeaderProbe);
    assert!(
        idx.warnings.iter().any(|w| w.starts_with(super::NO_REMUX)),
        "{:?}",
        idx.warnings
    );
    assert!(!super::probe_remuxable(&idx.to_probe_result()));
}

// ---------------------------------------------------------------- synthetic

/// Adds `delta` (mod 2³³) to every PTS/DTS of the PES packets in `ts`.
fn shift_timestamps(ts: &mut [u8], f: Framing, delta: i64) {
    let mut starts = Vec::new();
    packet::walk(ts, 0, f, |pk| {
        if pk.pusi && pk.payload.starts_with(&[0, 0, 1]) {
            starts.push((
                pk.pos as usize + f.lead + (188 - pk.payload.len()),
                pk.payload.len(),
            ));
        }
        true
    });
    for (at, _) in starts {
        let h = &mut ts[at..];
        let Some(ph) = pes::parse_header(h) else { continue };
        let fix = |b: &mut [u8], t: u64| {
            let v = ((t as i64 + delta).rem_euclid(WRAP)) as u64;
            b[0] = (b[0] & 0xF1) | (((v >> 30) as u8 & 7) << 1);
            b[1] = (v >> 22) as u8;
            b[2] = (((v >> 15) as u8) << 1) | 1;
            b[3] = (v >> 7) as u8;
            b[4] = ((v as u8) << 1) | 1;
        };
        if let Some(t) = ph.pts {
            fix(&mut h[9..14], t);
        }
        if let Some(t) = ph.dts {
            fix(&mut h[14..19], t);
        }
    }
}

fn scratch(name: &str, data: &[u8]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("flux-ts-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, data).unwrap();
    p
}

/// Video and audio stay on one continuous timeline across a 33-bit
/// timestamp wrap and across discontinuities (two recordings joined: the
/// clock restarts, then jumps 1000 s ahead).
#[tokio::test]
async fn pts_wrap_and_discontinuities_keep_one_timeline() {
    let Some(p) = media("seeds/seed_h264_ts.ts") else {
        return;
    };
    let seed = std::fs::read(&p).unwrap();
    let f = Framing::TS;
    // ~4.5 s of the 6 s clip before the wrap.
    let mut wrap = seed.clone();
    shift_timestamps(&mut wrap, f, WRAP - 6 * 90_000);
    // seed + seed (restarts) + seed shifted by +1000 s.
    let mut later = seed.clone();
    shift_timestamps(&mut later, f, 1000 * 90_000);
    let joined = [seed.clone(), seed.clone(), later].concat();
    for (name, bytes, copies, discontinuities) in
        [("wrap.ts", wrap, 1usize, 0usize), ("joined.ts", joined, 3, 2)]
    {
        let path = scratch(name, &bytes);
        let (engine, asset) = prepared(&path).await;
        let l = ts_layout(&asset);
        assert_eq!(l.discontinuities.len(), discontinuities, "{name}");
        let kfs = &asset.index.keyframes;
        assert_eq!(kfs.len(), 3 * copies, "{name}");
        assert!(kfs
            .windows(2)
            .all(|w| w[1].pts_ns > w[0].pts_ns && w[1].pts_ns - w[0].pts_ns < 3_000_000_000));
        // Each copy continues where the previous ended (6.0 s of video).
        let dur = asset.index.duration_ns as f64 / 1e9;
        assert!(
            (dur - 6.02 * copies as f64).abs() < 0.1 * copies as f64,
            "{name}: {dur} s"
        );
        let plan = engine.plan(&asset, &presets::chrome_desktop(), &PlanPrefs::default());
        assert_eq!(check_video(&engine, &asset, &plan).await, 144 * copies, "{name}");
        let k = AudioKey::from_plan(&plan.audio[0]);
        let (frames, _) = check_audio(&engine, &asset, &plan, k).await;
        let all = asset.audio_frames(1, i64::MIN, i64::MAX).unwrap();
        assert_eq!(frames, all.len(), "{name}");
        // Joined recordings overlap by the audio that leads the video (one
        // frame here); nothing goes further back.
        assert!(
            all.windows(2).all(|w| w[1].pts_ns > w[0].pts_ns - 50_000_000),
            "{name}: audio goes back"
        );
        let _ = std::fs::remove_file(&path);
    }
}

/// Frames ffmpeg decodes from `video_input` (keyframe `from` to the end),
/// and its error output.
fn decode_from_keyframe(asset: &Asset, from: usize) -> (u64, String) {
    use std::io::Write;
    let mut bytes = Vec::new();
    super::video_input(asset, from, None, |b| {
        bytes.extend_from_slice(b);
        Ok(())
    })
    .unwrap();
    let mut child = std::process::Command::new(EngineConfig::default().ffprobe)
        .args(["-v", "error", "-count_frames", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"])
        .args(["-f", "mpegts", "-i", "pipe:0"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&bytes);
    });
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    // One line per program and per stream section.
    let n = String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.trim().parse().ok())
        .unwrap_or(0);
    (n, String::from_utf8_lossy(&out.stderr).into_owned())
}

/// The seek-free transcode input decodes every frame from a keyframe on,
/// where ffmpeg's own `-ss` into the TS lost HEVC frames.
#[tokio::test]
async fn video_input_decodes_cleanly_from_any_keyframe() {
    if !EngineConfig::default().ffprobe.is_file() {
        return;
    }
    let mut files: Vec<PathBuf> = media("features/h264_aac.m2ts").into_iter().collect();
    files.extend(flux_test_media::clip(flux_test_media::Clip::H264AacTs));
    for p in files {
        let (_, asset) = prepared(&p).await;
        let kfs = asset.index.keyframes.len();
        let from = kfs - 2;
        // Pictures from that keyframe on: video PES presented at or after
        // it (an open GOP's leading pictures need the GOP before, and a
        // decoder starting at the CRA skips them, as it should).
        let pid = asset.index.video_track().unwrap().id as u16;
        let l = ts_layout(&asset);
        let kf = asset.index.keyframes[from];
        let mut pes = 0u64;
        packet::scan(asset.source(), kf.pos, l.data_end, l.framing(), 1 << 20, |pk| {
            if pk.pid == pid && pk.pusi {
                let pts = pes::parse_header(pk.payload).and_then(|h| h.pts);
                if pts.is_some_and(|p| l.corrected(pid, pk.pos, p, kf.raw_ts) >= kf.raw_ts) {
                    pes += 1;
                }
            }
            true
        })
        .unwrap();
        let (frames, err) = decode_from_keyframe(&asset, from);
        assert_eq!(frames, pes, "{}: {err}", p.display());
        assert!(err.trim().is_empty(), "{}: {err}", p.display());
    }
}
