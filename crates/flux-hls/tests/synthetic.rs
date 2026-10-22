//! probe → index → HLS segment → transcode on clips made with ffmpeg at
//! test time (`flux_test_media::clip`), so CI exercises the real paths on
//! every platform. The video transcodes are forced through libx264 (the
//! Linux software path; `EngineConfig::video_encoder`), and checked with
//! ffprobe: every source frame is there, and every piece starts on an IDR.
//! Each test skips, with the reason, when ffmpeg, ffprobe or an encoder a
//! clip needs is missing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use flux_core::{AudioOp, Codec, PlaybackPlan, VideoOp};
use flux_hls::playlist::{HlsResource, Rendition};
use flux_hls::{presets, Asset, AudioKey, EngineConfig, HlsEngine, PlanPrefs, VideoEncoder};
use flux_test_media::{clip, ffprobe_video_frames, Clip};
use mp4_atom::{Any, Decode};

fn hls_only() -> PlanPrefs {
    PlanPrefs {
        allow_direct_play: false,
        ..Default::default()
    }
}

/// An engine whose video transcodes use libx264, or `None` (with the
/// reason) when this ffmpeg has none.
fn x264_engine() -> Option<HlsEngine> {
    if !flux_test_media::has_encoder("libx264") {
        return None;
    }
    Some(HlsEngine::new(EngineConfig {
        video_encoder: Some(VideoEncoder::Libx264),
        video_lookahead: 0,
        audio_lookahead: 0,
        ..Default::default()
    }))
}

/// (tfdt, [(duration, cts, sync)]) of the first traf of a media segment.
fn trun(seg: &[u8]) -> (u64, Vec<(u32, i32, bool)>) {
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

/// Init + every segment of a rendition, written to `<dir>/<name>`.
async fn write_rendition(
    engine: &HlsEngine,
    asset: &Arc<Asset>,
    plan: &PlaybackPlan,
    r: Rendition,
    dir: &Path,
    name: &str,
) -> (PathBuf, Vec<Vec<u8>>) {
    let init = engine
        .serve(asset, plan, HlsResource::Init(r))
        .await
        .unwrap()
        .body
        .into_bytes()
        .await
        .unwrap();
    let mut all = init.to_vec();
    let mut segs = Vec::new();
    for n in 0..engine.segment_count(asset, plan, &r) as u32 {
        let b = engine
            .media_segment(asset, plan, &r, n)
            .await
            .unwrap_or_else(|e| panic!("{name} segment {n}: {e}"))
            .into_bytes()
            .await
            .unwrap()
            .to_vec();
        all.extend_from_slice(&b);
        segs.push(b);
    }
    let p = dir.join(name);
    std::fs::write(&p, &all).unwrap();
    (p, segs)
}

/// Remux (or, for the AVI, transcode) every clip for Chrome: the video
/// rendition has every source frame, as ffprobe counts them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clips_play_as_hls() {
    if flux_test_media::ffprobe().is_none() {
        return;
    }
    let Some(engine) = x264_engine() else { return };
    let dir = flux_test_media::scratch("hls-synthetic-remux");
    for c in [
        Clip::H264AacMp4,
        Clip::HevcAc3Mkv,
        Clip::H264AacTs,
        Clip::HevcAacTs,
        Clip::Mpeg4Mp2Avi,
    ] {
        let Some(p) = clip(c) else { continue };
        let name = c.file_name();
        let asset = engine.prepare(&p).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        let plan = engine.plan(&asset, &presets::chrome_desktop(), &hls_only());
        let master = engine.master_playlist(&asset, &plan).unwrap();
        assert!(master.starts_with("#EXTM3U"), "{name}: {master}");
        // MPEG-4 Part 2 is transcoded; everything else is remuxed.
        assert_eq!(
            matches!(plan.video, VideoOp::Transcode { .. }),
            c == Clip::Mpeg4Mp2Avi,
            "{name}: {:?}",
            plan.explain()
        );
        let (file, segs) = write_rendition(
            &engine,
            &asset,
            &plan,
            Rendition::Video,
            &dir,
            &format!("{name}.mp4"),
        )
        .await;
        assert!(!segs.is_empty(), "{name}");
        let frames = ffprobe_video_frames(&file).expect("ffprobe");
        assert_eq!(frames.len() as u32, c.frames().unwrap(), "{name}: frames out");
        assert!(frames[0].1, "{name}: starts on a keyframe");
        // Every audio rendition decodes (AC-3 and MP2 become AAC for Chrome).
        for a in &plan.audio {
            let r = Rendition::Audio(flux_hls::AudioKey::from_plan(a));
            let (file, segs) = write_rendition(
                &engine,
                &asset,
                &plan,
                r,
                &dir,
                &format!("{name}.a{}.mp4", a.track),
            )
            .await;
            assert!(
                !segs.is_empty() && std::fs::metadata(&file).unwrap().len() > 1000,
                "{name}"
            );
        }
    }
}

/// The software encoder end to end: HEVC (and an open-GOP HEVC TS, and an
/// AVI the index can only predict) transcoded to H.264 by libx264 for an
/// H.264-only client. Every piece starts with an IDR (in the bytes and
/// where ffprobe puts its keyframes), pieces are contiguous, and the
/// output has every source frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libx264_transcode_pieces_start_on_idrs_and_keep_every_frame() {
    if flux_test_media::ffprobe().is_none() {
        return;
    }
    let Some(engine) = x264_engine() else { return };
    let caps = engine.video_caps().await;
    assert_eq!(caps.chosen(), Some(VideoEncoder::Libx264), "{caps:?}");
    let dir = flux_test_media::scratch("hls-synthetic-x264");
    let mut ran = 0;
    for c in [
        Clip::HevcAc3Mkv,
        Clip::HevcAacTs,
        Clip::Mpeg4Mp2Avi,
        Clip::Hdr10HevcMkv,
    ] {
        let Some(p) = clip(c) else { continue };
        let name = c.file_name();
        let asset = engine.prepare(&p).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        let plan = engine.plan(&asset, &presets::legacy_h264(), &hls_only());
        let VideoOp::Transcode { tonemap, .. } = plan.video else {
            panic!("{name}: {:?}", plan.explain())
        };
        assert_eq!(tonemap, c == Clip::Hdr10HevcMkv, "{name}");
        let pieces = engine.segment_count(&asset, &plan, &Rendition::Video);
        assert!(pieces >= 3, "{name}: {pieces} pieces");
        let (file, segs) = write_rendition(
            &engine,
            &asset,
            &plan,
            Rendition::Video,
            &dir,
            &format!("{name}.x264.mp4"),
        )
        .await;
        let v = asset.index.video_track().unwrap();
        let ts = v.timescale as f64;
        let mut starts = Vec::new();
        let mut next_dts = None;
        for (n, s) in segs.iter().enumerate() {
            let (tfdt, samples) = trun(s);
            assert!(
                samples[0].2,
                "{name}: piece {n} does not start with a sync sample"
            );
            assert_eq!(
                first_vcl(s),
                Some(5),
                "{name}: piece {n} does not start with an IDR"
            );
            if let Some(d) = next_dts {
                assert_eq!(tfdt, d, "{name}: piece {n} is not contiguous");
            }
            next_dts = Some(tfdt + samples.iter().map(|x| x.0 as u64).sum::<u64>());
            starts.push((tfdt as f64 + samples[0].1 as f64) / ts);
        }
        let frames = ffprobe_video_frames(&file).expect("ffprobe");
        assert_eq!(frames.len() as u32, c.frames().unwrap(), "{name}: frames out");
        let fps = v.frame_rate.map(|r| r.as_f64()).unwrap_or(25.0);
        // ffprobe's clock may be shifted by an edit list: align the first frame.
        let shift = frames[0].0 - starts[0];
        for (n, t) in starts.iter().map(|t| t + shift).enumerate() {
            let at = frames
                .iter()
                .find(|(pts, _)| (pts - t).abs() < 0.5 / fps)
                .unwrap_or_else(|| panic!("{name}: no frame at piece {n}'s start {t:.3} s"));
            assert!(
                at.1,
                "{name}: piece {n} at {t:.3} s is not a keyframe for ffprobe"
            );
        }
        let keys = frames.iter().filter(|f| f.1).count();
        assert_eq!(keys, pieces, "{name}: one IDR per piece, none elsewhere");
        ran += 1;
    }
    eprintln!("{ran} clips transcoded by libx264");
}

/// A piece's bytes do not depend on how it was reached with libx264: two
/// fresh engines asked for the same piece cold (a seek) produce identical
/// bytes (same run, same deterministic encoder).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libx264_pieces_are_deterministic() {
    let Some(p) = clip(Clip::HevcAc3Mkv) else { return };
    let mut got = Vec::new();
    for _ in 0..2 {
        let Some(engine) = x264_engine() else { return };
        let asset = engine.prepare(&p).await.unwrap();
        let plan = engine.plan(&asset, &presets::legacy_h264(), &hls_only());
        let b = engine
            .media_segment(&asset, &plan, &Rendition::Video, 2)
            .await
            .unwrap()
            .into_bytes()
            .await
            .unwrap();
        got.push(b);
    }
    assert_eq!(got[0], got[1], "piece 2 differs between two cold runs");
}

/// Stream `map` of a file decoded by ffmpeg to mono 48 kHz samples.
fn decode_pcm(path: &Path, map: &str) -> Vec<i16> {
    let ff = flux_test_media::ffmpeg().expect("ffmpeg");
    let out = std::process::Command::new(ff)
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-map", map, "-ac", "1", "-ar", "48000", "-f", "s16le", "-"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run ffmpeg");
    assert!(
        out.status.success(),
        "decoding {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect()
}

/// Where each tone burst starts: the first sample at a quarter of the peak
/// after at least 100 ms below it.
fn burst_onsets(pcm: &[i16]) -> Vec<usize> {
    let threshold = (pcm.iter().map(|x| x.unsigned_abs()).max().unwrap_or(0) / 4).max(1);
    let mut onsets = Vec::new();
    let mut last_loud: Option<usize> = None;
    for (i, x) in pcm.iter().enumerate() {
        if x.unsigned_abs() >= threshold {
            if last_loud.is_none_or(|l| i - l > 4800) {
                onsets.push(i);
            }
            last_loud = Some(i);
        }
    }
    onsets
}

fn rms(x: &[i16]) -> f64 {
    let n = x.len().max(1) as f64;
    (x.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / n).sqrt()
}

/// Where `got` is silent but the reference is loud: tone replaced by
/// silence, which onsets can't see when it falls inside a burst. Each is
/// decoded PCM with its timeline origin (48 kHz ticks). 1 ms windows of the
/// reference from tick `after` on count when the reference is loud from
/// 3 ms before to 3 ms after them (so burst edges and small lags don't) and
/// `got` is below a tenth of it there. Returns the stretches as inclusive
/// ms ranges on the timeline.
fn silenced(reference: (&[i16], i64), got: (&[i16], i64), after: i64) -> Vec<(i64, i64)> {
    const W: usize = 48;
    let env: Vec<f64> = reference.0.chunks(W).map(rms).collect();
    let loud = env.iter().copied().fold(0.0, f64::max) / 4.0;
    let mut runs: Vec<(i64, i64)> = Vec::new();
    for k in 3..env.len().saturating_sub(3) {
        let at = reference.1 + (k * W) as i64;
        if at < after || env[k - 3..=k + 3].iter().any(|&e| e < loud) {
            continue;
        }
        let level = usize::try_from(at - got.1)
            .ok()
            .and_then(|j| got.0.get(j..j + W))
            .map_or(0.0, rms);
        if level < env[k] / 10.0 {
            let ms = at / 48;
            match runs.last_mut() {
                Some(r) if r.1 + 1 == ms => r.1 = ms,
                _ => runs.push((ms, ms)),
            }
        }
    }
    runs
}

/// AC-3 and E-AC-3 in Matroska with their frames laced 8 to a block
/// (256 ms per block, as many releases are muxed), transcoded to AAC for
/// Chrome. Each segment (and each of its parallel parts) once lost the
/// frames of the laced block that began before its window: frames went
/// missing at the seams (21–256 ms holes) and the rest of the segment slid
/// earlier. Every segment must hold all its grid frames and end exactly
/// where the next begins, every tone burst must sit where it is in the
/// copied original (a constant offset, across seams and part joins), and no
/// tone may be lost: the transcoder pads a source that starts late with
/// silence, which keeps the frame count and the onsets right, so frames
/// lost at the start of a part show only as silence where the copy is loud.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn laced_ac3_in_matroska_transcodes_without_gaps() {
    if flux_test_media::ffmpeg().is_none() {
        return;
    }
    let Some(p) = clip(Clip::LacedAc3Eac3Mkv) else {
        return;
    };
    // 2 s segments (one per GOP): 4 seams, and each segment is encoded as
    // two parallel parts, so 5 part joins as well.
    let engine = HlsEngine::new(EngineConfig {
        target_segment: std::time::Duration::from_secs(2),
        video_lookahead: 0,
        audio_lookahead: 0,
        ..Default::default()
    });
    let asset = engine.prepare(&p).await.unwrap();
    let plan = engine.plan(&asset, &presets::chrome_desktop(), &hls_only());
    assert_eq!(plan.audio.len(), 2, "{:?}", plan.explain());
    // The reference: the same tracks copied for Safari, frame for frame.
    let safari = engine.plan(&asset, &presets::safari_macos(), &hls_only());
    let dir = flux_test_media::scratch("hls-synthetic-laced-ac3");
    // A rendition decoded, with its timeline origin: the first tfdt (48 kHz
    // ticks; the segments are contiguous).
    let decode = |file: &Path, segs: &[Vec<u8>]| (decode_pcm(file, "0:a:0"), trun(&segs[0]).0 as i64);
    // Burst onsets on a rendition's timeline.
    let onsets = |(pcm, t0): &(Vec<i16>, i64)| -> Vec<i64> {
        burst_onsets(pcm).into_iter().map(|i| t0 + i as i64).collect()
    };
    for a in &plan.audio {
        let t = asset.index.track(a.track).expect("track");
        let name = format!("{:?} track {}", t.codec, a.track);
        assert!(
            matches!(
                a.op,
                AudioOp::Transcode {
                    codec: Codec::Aac,
                    ..
                }
            ),
            "{name}: {:?}",
            a.op
        );
        assert_eq!(t.sample_rate, Some(48_000), "{name}");
        let (file, segs) = write_rendition(
            &engine,
            &asset,
            &plan,
            Rendition::Audio(AudioKey::from_plan(a)),
            &dir,
            &format!("laced.a{}.mp4", a.track),
        )
        .await;
        assert!(segs.len() >= 5, "{name}: {} segments", segs.len());
        let mut next = None;
        for (n, s) in segs.iter().enumerate() {
            let (tfdt, samples) = trun(s);
            assert!(samples.iter().all(|x| x.0 == 1024), "{name}: segment {n}");
            if let Some(end) = next {
                assert_eq!(
                    tfdt,
                    end,
                    "{name}: segment {n} starts {:.1} ms after segment {} ends",
                    (tfdt as f64 - end as f64) / 48.0,
                    n - 1
                );
            }
            next = Some(tfdt + 1024 * samples.len() as u64);
        }
        // Each AAC burst against the nearest burst of the copied original.
        // The first burst is skipped: an encode loses its encoder delay at
        // the very start of the stream.
        let copy = safari
            .audio
            .iter()
            .find(|c| c.track == a.track && matches!(c.op, AudioOp::Copy))
            .unwrap_or_else(|| panic!("{name}: not copied for Safari: {:?}", safari.audio));
        let (copy_file, copy_segs) = write_rendition(
            &engine,
            &asset,
            &safari,
            Rendition::Audio(AudioKey::from_plan(copy)),
            &dir,
            &format!("laced.a{}.copy.mp4", a.track),
        )
        .await;
        let want = decode(&copy_file, &copy_segs);
        let got = decode(&file, &segs);
        let reference = onsets(&want);
        assert!(reference.len() >= 38, "{name}: {} bursts", reference.len());
        let lags: Vec<i64> = onsets(&got)
            .into_iter()
            .filter(|&at| at > reference[0] + 4800)
            .map(|at| {
                let near = reference.iter().min_by_key(|&&r| (r - at).abs()).unwrap();
                at - near
            })
            .collect();
        assert!(
            lags.len() + 2 >= reference.len(),
            "{name}: {} bursts out of {}",
            lags.len(),
            reference.len()
        );
        // An AAC frame is 1024 ticks (21.3 ms); onsets agree within 2 ms.
        assert!(
            lags.iter().all(|l| l.abs() <= 96),
            "{name}: burst offsets from the copy (ticks) {lags:?}"
        );
        // Part joins fall inside bursts here (a part is ~47 AAC frames, just
        // over 1 s; bursts come every 0.25 s), so a burst cut short at a join
        // keeps its onset: compare the level too, past the first burst.
        let lost = silenced((&want.0, want.1), (&got.0, got.1), reference[0] + 4800);
        assert!(
            lost.is_empty(),
            "{name}: silent where the copy is loud (ms on the timeline) {lost:?}"
        );
    }
}

/// The index describes one access unit of laced (E-)AC-3, not the first
/// block's eight: the dec3 data rate and the bitrate (which feeds the HLS
/// BANDWIDTH and the planner's bitrate checks) are what the track was
/// encoded at.
#[test]
fn laced_eac3_reports_the_bitrate_it_was_encoded_at() {
    let Some(p) = clip(Clip::LacedAc3Eac3Mkv) else {
        return;
    };
    let idx = flux_hls::PlaybackIndex::build(&p).unwrap();
    let mut seen = 0;
    for t in idx
        .tracks
        .iter()
        .filter(|t| matches!(t.codec, Codec::Ac3 | Codec::Eac3))
    {
        assert_eq!(t.bitrate, Some(192_000), "{:?}", t.codec);
        assert_eq!(t.frame_duration, Some(1536), "{:?}", t.codec);
        if t.codec == Codec::Eac3 {
            let dec3 = t.synth_config.as_deref().expect("dec3");
            assert_eq!(
                u16::from_be_bytes([dec3[0], dec3[1]]) >> 3,
                192,
                "dec3 data_rate (kb/s)"
            );
        }
        seen += 1;
    }
    assert_eq!(seen, 2);
}
