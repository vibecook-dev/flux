//! probe → index → HLS segment → transcode on clips made with ffmpeg at
//! test time (`flux_test_media::clip`), so CI exercises the real paths on
//! every platform. The video transcodes are forced through libx264 (the
//! Linux software path; `EngineConfig::video_encoder`), and checked with
//! ffprobe: every source frame is there, and every piece starts on an IDR.
//! Each test skips, with the reason, when ffmpeg, ffprobe or an encoder a
//! clip needs is missing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use flux_core::{AudioOp, Codec, PlaybackPlan, SubtitleFile, SubtitleOp, VideoOp, SUBTITLE_FILE_TRACK_BASE};
use flux_hls::index::Layout;
use flux_hls::playlist::{HlsResource, Rendition};
use flux_hls::{presets, Asset, AudioKey, EngineConfig, HlsEngine, PlanPrefs, VideoEncoder, WebVttBody};
use flux_test_media::{clip, ffprobe_video_frames, Clip};
use futures::StreamExt;
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

/// (frames decoded, packets) of the video of `path`, by ffprobe.
fn decoded_frames(path: &Path) -> (u64, u64) {
    let out = std::process::Command::new(flux_test_media::ffprobe().expect("ffprobe"))
        .args([
            "-v",
            "error",
            "-count_frames",
            "-count_packets",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=nb_read_frames,nb_read_packets",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("ffprobe");
    let text = String::from_utf8_lossy(&out.stdout);
    let n: Vec<u64> = text
        .trim()
        .split(',')
        .map(|x| x.trim().parse().unwrap_or(0))
        .collect();
    (n.first().copied().unwrap_or(0), n.get(1).copied().unwrap_or(0))
}

/// MP4s cut like a stream edited without re-encoding: frames held for
/// seconds, and frames after the cuts that the decoder drops.
/// [`Clip::HeldFramesMp4`] is 23.976 fps on a 90 kHz clock, so it has no
/// constant sample duration; [`Clip::HeldLateMp4`] is 25 fps with one
/// duration for nine tenths of its samples and its cut in the rest. Pieces
/// cut at the index's frame counts once started on P frames after the cuts
/// (a piece played alone showed nothing), and lasted a whole keyframe
/// span. Converted to their nominal rate instead, a forced transcode is cut
/// in pieces of 2 s across the held frames, each starting with an IDR and
/// decoding alone, contiguous, with the frames the playlist promises.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_frames_mp4s_transcode_in_short_pieces_that_each_start_on_an_idr() {
    if flux_test_media::ffprobe().is_none() {
        return;
    }
    let Some(engine) = x264_engine() else { return };
    let dir = flux_test_media::scratch("hls-synthetic-held-frames");
    // (clip, rate, frame duration at 90 kHz, frames a piece)
    let cases = [
        (Clip::HeldFramesMp4, (24000, 1001), None, 48),
        (Clip::HeldLateMp4, (25, 1), Some(3600), 50),
    ];
    for (c, (num, den), fd, per) in cases {
        let Some(p) = clip(c) else { continue };
        let name = c.file_name();
        let asset = engine.prepare(&p).await.unwrap();
        let mut client = presets::chrome_desktop();
        client.max_bitrate = Some(100_000);
        let plan = engine.plan(&asset, &client, &hls_only());
        assert!(
            matches!(plan.video, VideoOp::Transcode { .. }),
            "{name}: {:?}",
            plan.explain()
        );
        let (file, segs) = write_rendition(&engine, &asset, &plan, Rendition::Video, &dir, name).await;
        let init = engine
            .serve(&asset, &plan, HlsResource::Init(Rendition::Video))
            .await
            .unwrap()
            .body
            .into_bytes()
            .await
            .unwrap();
        let mut next_dts = None;
        let (mut lens, mut idrs) = (Vec::new(), Vec::new());
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
            // Alone, as a player starts or seeks there: every frame decodes.
            let alone = dir.join(format!("{name}.piece{n}.mp4"));
            std::fs::write(&alone, [init.as_ref(), s.as_slice()].concat()).unwrap();
            let (decoded, packets) = decoded_frames(&alone);
            assert_eq!(packets, samples.len() as u64, "{name}: piece {n}");
            assert_eq!(
                decoded, packets,
                "{name}: piece {n} decodes {decoded} of {packets} frames alone"
            );
            let _ = std::fs::remove_file(&alone);
            lens.push(samples.len());
            idrs.push(samples.iter().filter(|x| x.2).count());
        }
        // One IDR a piece, and 2 s of frames (a keyframe span made a piece
        // of 177).
        assert!(idrs.iter().all(|&n| n == 1), "{name}: IDRs per piece {idrs:?}");
        assert!(
            lens[..lens.len() - 1].iter().all(|&n| n == per),
            "{name}: {lens:?}"
        );
        // The playlist: pieces of 2 s, the last one the rest.
        let step = per as f64 * den as f64 / num as f64;
        let playlist = engine.media_playlist(&asset, &plan, &Rendition::Video).unwrap();
        let durs: Vec<f64> = playlist
            .lines()
            .filter_map(|l| l.strip_prefix("#EXTINF:"))
            .map(|d| d.trim_end_matches(',').parse().unwrap())
            .collect();
        assert_eq!(durs.len(), segs.len(), "{name}");
        assert!(
            durs[..durs.len() - 1].iter().all(|d| (d - step).abs() < 1e-3),
            "{name}: {durs:?}"
        );
        assert!(playlist.contains("#EXT-X-TARGETDURATION:2\n"), "{playlist}");
        // Every frame of the grid, the held and dropped frames filled.
        let frames: usize = lens.iter().sum();
        let expect = (durs.iter().sum::<f64>() * num as f64 / den as f64).round() as usize;
        assert!(
            expect as f64 > durs.iter().sum::<f64>() * 23.0,
            "{name}: {expect}"
        );
        assert_eq!(frames, expect, "{name}");
        let all = ffprobe_video_frames(&file).expect("ffprobe");
        assert_eq!(all.len(), frames, "{name}: frames out");
        assert_eq!(
            all.iter().filter(|f| f.1).count(),
            segs.len(),
            "{name}: keyframes"
        );
        // The rate most frames have (the average over the held frames was
        // lower).
        let v = asset.index.video_track().unwrap();
        assert_eq!(v.frame_duration, fd, "{name}");
        assert_eq!(v.frame_rate, Some(flux_core::Rational { num, den }), "{name}");
    }
}

/// A resume just after a keyframe that lies inside a converted video's
/// piece: `warm` starts the encoder run at that piece, the one the player
/// asks for first (`v/{vn}.m4s`), so serving it takes no run of its own.
/// `warm` used to start at the first piece starting in the keyframe's
/// segment, the piece after, and the player's piece then took a second
/// cold run from the keyframe before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warm_starts_at_the_piece_a_resume_asks_for() {
    if !flux_test_media::has_encoder("libx264") {
        return;
    }
    let Some(p) = clip(Clip::HeldFramesMp4) else {
        return;
    };
    // One keyframe span (5 s) a remux segment: their keyframes then lie
    // inside the 2 s pieces of the 23.976 fps grid (spans of two, 240
    // frames, end on a piece's 48).
    let engine = HlsEngine::new(EngineConfig {
        video_encoder: Some(VideoEncoder::Libx264),
        video_lookahead: 0,
        audio_lookahead: 0,
        target_segment: std::time::Duration::from_secs(4),
        ..Default::default()
    });
    let asset = engine.prepare(&p).await.unwrap();
    let mut client = presets::chrome_desktop();
    client.max_bitrate = Some(100_000);
    let plan = engine.plan(&asset, &client, &hls_only());
    assert!(
        matches!(plan.video, VideoOp::Transcode { .. }),
        "{:?}",
        plan.explain()
    );
    let segs = &asset.segments.segments;
    let piece_ns = engine.config().video_segment_target.as_nanos() as i64;
    let pieces = flux_hls::vtranscode::pieces(&asset, piece_ns);
    // A keyframe inside a piece, and a time between it and the end of
    // both the piece and the keyframe's segment.
    let (seg, vn) = (1..segs.len())
        .find_map(|s| {
            let k = segs[s].start_ticks;
            let p = pieces.iter().position(|p| p.start_ticks < k && k < p.end_ticks)?;
            Some((s, p))
        })
        .expect("a piece across a keyframe");
    let mid = (segs[seg].start_ticks + pieces[vn].end_ticks.min(segs[seg].end_ticks)) / 2;
    let t = (mid - segs[0].start_ticks) as f64 / asset.segments.timescale as f64;
    assert_eq!(asset.segments.segment_at(t), seg);
    assert_eq!(
        engine.segment_at(&asset, &plan, &Rendition::Video, t) as usize,
        vn
    );
    assert!(flux_hls::vtranscode::first_piece_of(&pieces, seg) > vn);
    engine.warm(&asset, &plan, t);
    let t0 = std::time::Instant::now();
    while engine.video_transcode_stats().segments == 0 {
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(120),
            "warm produced nothing"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let runs = engine.video_transcode_stats().runs;
    assert_eq!(runs, 1);
    engine
        .media_segment(&asset, &plan, &Rendition::Video, vn as u32)
        .await
        .unwrap()
        .into_bytes()
        .await
        .unwrap();
    assert_eq!(
        engine.video_transcode_stats().runs,
        runs,
        "piece {vn} took a run of its own"
    );
}

// ---------------------------------------------------------------- subtitles

/// A subtitle file beside a video, as the catalog would offer it.
fn subtitle_file(path: &Path, codec: Codec, n: u32) -> SubtitleFile {
    SubtitleFile {
        track: SUBTITLE_FILE_TRACK_BASE + n,
        path: path.display().to_string(),
        codec,
        language: None,
        title: None,
        forced: false,
        hearing_impaired: false,
        size: std::fs::metadata(path).unwrap().len(),
        mtime_ns: 0,
    }
}

/// The body of a WebVTT response.
async fn vtt_of(body: WebVttBody) -> String {
    match body {
        WebVttBody::Whole(doc) => doc.to_string(),
        WebVttBody::Streaming(s) => {
            let parts: Vec<_> = s.collect().await;
            let mut doc = Vec::new();
            for p in parts {
                doc.extend_from_slice(&p.unwrap());
            }
            String::from_utf8(doc).unwrap()
        }
    }
}

/// MP4 `mov_text` (tx3g) subtitles are served as WebVTT: they were offered
/// before, but every fetch failed after a 200 ("subtitles: Matroska only").
/// The response starts only once the first cue is read.
#[tokio::test]
async fn mp4_text_subtitles_are_served_as_webvtt() {
    let Some(ff) = flux_test_media::ffmpeg() else {
        return;
    };
    let Some(p) = clip(Clip::H264AacMp4) else { return };
    let dir = flux_test_media::scratch("hls-synthetic-movtext");
    let srt = dir.join("subs.srt");
    std::fs::write(
        &srt,
        "1\n00:00:01,000 --> 00:00:02,500\nFish & chips > fries\n\n2\n00:00:04,000 --> 00:00:05,000\n示例字幕\n",
    )
    .unwrap();
    let out = dir.join("movtext.mp4");
    let st = std::process::Command::new(&ff)
        .args(["-v", "error", "-y", "-i"])
        .arg(&p)
        .arg("-i")
        .arg(&srt)
        .args([
            "-map", "0", "-map", "1", "-c", "copy", "-c:s", "mov_text", "-f", "mp4",
        ])
        .arg(&out)
        .status()
        .unwrap();
    assert!(st.success(), "ffmpeg made no mov_text MP4");
    let engine = HlsEngine::new(EngineConfig::default());
    let asset = engine.prepare(&out).await.unwrap();
    let t = asset
        .index
        .tracks
        .iter()
        .find(|t| t.codec == Codec::MovText)
        .expect("a tx3g track")
        .index;
    assert!(asset.text_subtitle_readable(t));
    let plan = engine.plan(&asset, &presets::chrome_desktop(), &hls_only());
    assert!(plan
        .subtitles
        .iter()
        .any(|s| s.track == t && s.op == SubtitleOp::WebVtt));
    // Streamed the first time: the first chunk is the header and a cue.
    let first = match engine.plan_webvtt_body(&asset, &plan, t).await.unwrap() {
        WebVttBody::Streaming(mut s) => String::from_utf8(s.next().await.unwrap().unwrap().to_vec()).unwrap(),
        WebVttBody::Whole(_) => panic!("nothing was extracted yet"),
    };
    assert!(
        first.starts_with("WEBVTT\n") && first.contains(" --> "),
        "{first}"
    );
    let vtt = engine.webvtt(&asset, t).await.unwrap();
    // Plain text: `&` and `>` are escaped (ffmpeg's SRT reader would take
    // `<…>` for a tag, so none here).
    assert!(
        vtt.contains("00:00:01.000 --> 00:00:02.500\nFish &amp; chips &gt; fries\n"),
        "{vtt}"
    );
    assert!(vtt.contains("00:00:04.000 --> 00:00:05.000\n示例字幕\n"), "{vtt}");
    // Through the HLS route too.
    let r = engine.serve(&asset, &plan, HlsResource::WebVtt(t)).await.unwrap();
    let served = String::from_utf8(r.body.into_bytes().await.unwrap().to_vec()).unwrap();
    assert_eq!(served, *vtt);
}

/// A text subtitle file beside the video (ASS, UTF-8 with a byte-order
/// mark) is offered and served as WebVTT with its CJK text; a picture
/// track that can't be converted is refused before any response starts.
#[tokio::test]
async fn subtitle_files_are_served_as_webvtt() {
    let Some(p) = clip(Clip::H264AacMp4) else { return };
    let dir = flux_test_media::scratch("hls-synthetic-sidecar");
    let ass = dir.join("Clip.chs.ass");
    std::fs::write(
        &ass,
        "\u{feff}[Script Info]\nScriptType: v4.00+\n\n[Events]\n\
         Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
         Dialogue: 0,0:00:01.50,0:00:03.00,Default,,0,0,0,,{\\b1}示例字幕{\\b0}\\N第二行\n",
    )
    .unwrap();
    let file = subtitle_file(&ass, Codec::Ass, 7);
    let engine = HlsEngine::new(EngineConfig::default());
    let asset = engine.prepare(&p).await.unwrap();
    let prefs = PlanPrefs {
        subtitle_files: vec![file.clone()],
        ..hls_only()
    };
    let plan = engine.plan(&asset, &presets::chrome_desktop(), &prefs);
    let entry = plan
        .subtitles
        .iter()
        .find(|s| s.track == file.track)
        .expect("offered");
    assert_eq!(entry.op, SubtitleOp::WebVtt);
    let master = engine.master_playlist(&asset, &plan).unwrap();
    assert!(
        master.contains(&format!("URI=\"s/{}.m3u8\"", file.track)),
        "{master}"
    );
    let vtt = vtt_of(engine.plan_webvtt_body(&asset, &plan, file.track).await.unwrap()).await;
    assert!(
        vtt.contains("00:00:01.500 --> 00:00:03.000\n示例字幕\n第二行\n"),
        "{vtt}"
    );
    // Direct play serves it too (a `<track>` of the page).
    let direct = engine.plan(
        &asset,
        &presets::chrome_desktop(),
        &PlanPrefs {
            subtitle_files: vec![file.clone()],
            ..Default::default()
        },
    );
    assert!(direct.is_direct());
    let again = vtt_of(
        engine
            .plan_webvtt_body(&asset, &direct, file.track)
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(again, vtt);
    // A file that changed since the plan was made is refused, not served stale.
    std::fs::write(&ass, "[Events]\n").unwrap();
    let fresh = HlsEngine::new(EngineConfig::default());
    let a2 = fresh.prepare(&p).await.unwrap();
    assert!(fresh.plan_webvtt_body(&a2, &plan, file.track).await.is_err());
    // Not a text track: an error before anything is sent.
    assert!(fresh.webvtt_body(&a2, 0).await.is_err());
}

/// One PGS display set in `.sup` form: a white box at `rect` on a `canvas`
/// from `from` seconds, cleared at `to`.
fn pgs_box(canvas: (u16, u16), rect: (u16, u16, u16, u16), from: f64, to: f64) -> Vec<u8> {
    fn seg(out: &mut Vec<u8>, pts: f64, ty: u8, body: &[u8]) {
        out.extend_from_slice(b"PG");
        out.extend_from_slice(&((pts * 90_000.0) as u32).to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.push(ty);
        out.extend_from_slice(&(body.len() as u16).to_be_bytes());
        out.extend_from_slice(body);
    }
    let be = |v: u16| v.to_be_bytes();
    let (x, y, w, h) = rect;
    let mut out = Vec::new();
    // Presentation composition: epoch start, one object in window 0.
    let mut pcs = [be(canvas.0), be(canvas.1)].concat();
    pcs.extend_from_slice(&[0x10, 0, 1, 0x80, 0, 0, 1, 0, 0, 0, 0]);
    pcs.extend_from_slice(&[be(x), be(y)].concat());
    let wds = [&[1u8, 0][..], &be(x), &be(y), &be(w), &be(h)].concat();
    // Palette 0: entry 1 is opaque white (Y, Cr, Cb, alpha).
    let pds = [0u8, 0, 1, 235, 128, 128, 255];
    // Object: every line one run of colour 1, then end of line.
    let mut rle = Vec::new();
    for _ in 0..h {
        rle.extend_from_slice(&[0, 0xC0 | (w >> 8) as u8, (w & 0xFF) as u8, 1, 0, 0]);
    }
    let len = (rle.len() + 4) as u32;
    let mut ods = vec![0, 0, 0, 0xC0];
    ods.extend_from_slice(&len.to_be_bytes()[1..]);
    ods.extend_from_slice(&[be(w), be(h)].concat());
    ods.extend_from_slice(&rle);
    seg(&mut out, from, 0x16, &pcs);
    seg(&mut out, from, 0x17, &wds);
    seg(&mut out, from, 0x14, &pds);
    seg(&mut out, from, 0x15, &ods);
    seg(&mut out, from, 0x80, &[]);
    // Cleared: a composition with no objects.
    let mut clear = [be(canvas.0), be(canvas.1)].concat();
    clear.extend_from_slice(&[0x10, 0, 2, 0x00, 0, 0, 0]);
    seg(&mut out, to, 0x16, &clear);
    seg(&mut out, to, 0x17, &wds);
    seg(&mut out, to, 0x80, &[]);
    out
}

/// The luma of the frame shown at `t` seconds.
fn gray_frame(ff: &Path, file: &Path, t: f64) -> Vec<u8> {
    let out = std::process::Command::new(ff)
        .args(["-v", "error", "-ss", &format!("{t:.3}"), "-i"])
        .arg(file)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "gray", "-"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

/// Mean absolute difference of two 640-wide gray frames inside `(x0, y0, x1, y1)`.
fn diff_in(a: &[u8], b: &[u8], (x0, y0, x1, y1): (usize, usize, usize, usize)) -> f64 {
    let mut sum = 0u64;
    for y in y0..y1 {
        for x in x0..x1 {
            sum += (a[y * 640 + x] as i64 - b[y * 640 + x] as i64).unsigned_abs();
        }
    }
    sum as f64 / ((x1 - x0) * (y1 - y0)) as f64
}

/// A PGS subtitle file is drawn into the transcode exactly while it is on
/// screen: compared with the same transcode without it, the box differs
/// from 1 s to 3 s and nowhere else. Each 2 s piece is its own encoder run,
/// so the run of the piece starting at 2 s has to draw a subtitle that
/// appeared before its first frame (and before its seek point).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pgs_file_is_burned_in_where_and_when_it_shows() {
    let Some(ff) = flux_test_media::ffmpeg() else {
        return;
    };
    if !flux_test_media::has_encoder("libx264") {
        return;
    }
    let Some(p) = clip(Clip::H264AacMp4) else { return };
    let dir = flux_test_media::scratch("hls-synthetic-burn");
    let sup = dir.join("Clip.sup");
    std::fs::write(&sup, pgs_box((640, 360), (200, 260, 240, 60), 1.0, 3.0)).unwrap();
    let file = subtitle_file(&sup, Codec::Pgs, 1);
    let engine = HlsEngine::new(EngineConfig {
        video_encoder: Some(VideoEncoder::Libx264),
        video_lookahead: 0,
        audio_lookahead: 0,
        video_run_segments: 1,
        ..Default::default()
    });
    let asset = engine.prepare(&p).await.unwrap();
    let prefs = PlanPrefs {
        subtitle_track: Some(file.track),
        subtitle_files: vec![file.clone()],
        ..hls_only()
    };
    let burned = engine.plan(&asset, &presets::chrome_desktop(), &prefs);
    let VideoOp::Transcode { burn_subtitle, .. } = &burned.video else {
        panic!("{:?}", burned.explain())
    };
    assert_eq!(*burn_subtitle, Some(file.track));
    let mut plain = burned.clone();
    if let VideoOp::Transcode { burn_subtitle, .. } = &mut plain.video {
        *burn_subtitle = None;
    }
    plain.subtitles.clear();
    let (a, _) = write_rendition(&engine, &asset, &burned, Rendition::Video, &dir, "burned.mp4").await;
    let (b, _) = write_rendition(&engine, &asset, &plain, Rendition::Video, &dir, "plain.mp4").await;
    assert_box_burned_in_from_1_to_3(&ff, &a, &b);
}

/// `burned`, a rendition with the box of [`pgs_box`] at (200, 260, 240, 60)
/// shown from 1 s to 3 s drawn in, against `plain`, the same transcode
/// without it: they differ inside the box exactly while it is shown, and
/// nowhere else.
fn assert_box_burned_in_from_1_to_3(ff: &Path, burned: &Path, plain: &Path) {
    let what = burned.file_name().unwrap().to_string_lossy();
    let (a, b) = (burned, plain);
    let inside = (208, 268, 432, 312);
    let above = (0, 0, 640, 200);
    for (t, shown) in [(0.5, false), (1.5, true), (2.5, true), (3.5, false), (6.5, false)] {
        let (fa, fb) = (gray_frame(ff, a, t), gray_frame(ff, b, t));
        assert_eq!(fa.len(), 640 * 360, "{what}, t = {t}");
        let (d_in, d_out) = (diff_in(&fa, &fb, inside), diff_in(&fa, &fb, above));
        assert!(
            d_out < 8.0,
            "{what}, t = {t}: the pictures differ away from the subtitle ({d_out:.1})"
        );
        if shown {
            assert!(d_in > 40.0, "{what}, t = {t}: no subtitle drawn ({d_in:.1})");
            let white = fa[290 * 640 + 320];
            assert!(white > 200, "{what}, t = {t}: the box is not white ({white})");
        } else {
            assert!(
                d_in < 8.0,
                "{what}, t = {t}: a subtitle drawn when none is shown ({d_in:.1})"
            );
        }
    }
}

/// A PGS track of an M2TS whose timestamps start at 21.4 s (a Blu-ray rip)
/// is drawn in at its own time. ffmpeg counts a TS from its start time,
/// where the source timeline has 0, and lines up a track that starts later
/// (here at 22.4 s, with its first subtitle) from its first packet when
/// seeked before it: the track used to land 21.4 s late, or not at all.
/// With TrueHD audio (when this ffmpeg can make it) ffmpeg also counts the
/// AC-3 core of the TrueHD PID as a stream of its own, so the track is
/// picked by PID.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pgs_track_of_an_m2ts_is_burned_in_on_time() {
    let Some(ff) = flux_test_media::ffmpeg() else {
        return;
    };
    if !flux_test_media::has_encoder("libx264") {
        return;
    }
    let Some(p) = clip(Clip::H264AacMp4) else { return };
    let dir = flux_test_media::scratch("hls-synthetic-burn-m2ts");
    let sup = dir.join("box.sup");
    std::fs::write(&sup, pgs_box((640, 360), (200, 260, 240, 60), 1.0, 3.0)).unwrap();
    let mut audio: Vec<(&str, &[&str])> = vec![("aac", &["-c:a", "copy"])];
    if flux_test_media::ffmpeg_encoders().iter().any(|e| e == "truehd") {
        audio.push(("truehd", &["-c:a", "truehd", "-strict", "-2"]));
    }
    for (name, args) in audio {
        let m2ts = dir.join(format!("Clip-{name}.m2ts"));
        // `-seek_timestamp 1`: both inputs keep their times (the box at 1 s).
        let st = std::process::Command::new(&ff)
            .args(["-v", "error", "-y", "-seek_timestamp", "1", "-i"])
            .arg(&p)
            .args(["-seek_timestamp", "1", "-i"])
            .arg(&sup)
            .args(["-map", "0:v", "-map", "0:a", "-map", "1", "-c:v", "copy"])
            .args(args)
            .args([
                "-c:s",
                "copy",
                "-output_ts_offset",
                "20",
                "-mpegts_m2ts_mode",
                "1",
                "-f",
                "mpegts",
            ])
            .arg(&m2ts)
            .status()
            .unwrap();
        assert!(st.success(), "ffmpeg made no {name} M2TS");
        let engine = HlsEngine::new(EngineConfig {
            video_encoder: Some(VideoEncoder::Libx264),
            video_lookahead: 0,
            audio_lookahead: 0,
            video_run_segments: 1,
            ..Default::default()
        });
        let asset = engine.prepare(&m2ts).await.unwrap();
        let Layout::Ts(ts) = &asset.index.layout else {
            panic!("{name}: not indexed as MPEG-TS: {:?}", asset.index.warnings)
        };
        assert!(ts.origin > 20 * 90_000, "{name}: starts at {} ticks", ts.origin);
        let track = asset
            .index
            .tracks
            .iter()
            .find(|t| t.codec == Codec::Pgs)
            .expect("a PGS track")
            .index;
        let prefs = PlanPrefs {
            subtitle_track: Some(track),
            ..hls_only()
        };
        let burned = engine.plan(&asset, &presets::chrome_desktop(), &prefs);
        let VideoOp::Transcode { burn_subtitle, .. } = &burned.video else {
            panic!("{name}: {:?}", burned.explain())
        };
        assert_eq!(*burn_subtitle, Some(track));
        let mut plain = burned.clone();
        if let VideoOp::Transcode { burn_subtitle, .. } = &mut plain.video {
            *burn_subtitle = None;
        }
        plain.subtitles.clear();
        let a_name = format!("burned-{name}.mp4");
        let b_name = format!("plain-{name}.mp4");
        let (a, _) = write_rendition(&engine, &asset, &burned, Rendition::Video, &dir, &a_name).await;
        let (b, _) = write_rendition(&engine, &asset, &plain, Rendition::Video, &dir, &b_name).await;
        assert_box_burned_in_from_1_to_3(&ff, &a, &b);
    }
}

/// Interlacing is the container's flag, else the SPS's, for the catalog's
/// probe and the playback index alike, so `POST /plan` (probe) and `/play`
/// (index) agree for every client. x264 `fake-interlaced` (an SPS that
/// allows field coding, progressive frames, flagged progressive) is
/// remuxed for browsers. Interlaced H.264, as MBAFF fields or as fields
/// woven into progressive-coded frames, is transcoded for them and
/// deinterlaced: the marked frames, or every frame when the decoder marks
/// none. The transcode keeps every frame and starts each piece on an IDR.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interlaced_clips_plan_alike_from_probe_and_index_and_deinterlace() {
    use flux_hls::encoders::Deinterlace;
    if flux_test_media::ffprobe().is_none() {
        return;
    }
    let Some(engine) = x264_engine() else { return };
    let dir = flux_test_media::scratch("hls-synthetic-interlace");
    let prefs = PlanPrefs::default();
    for (c, interlaced, mode) in [
        (Clip::FakeInterlacedMkv, false, Deinterlace::Off),
        (Clip::InterlacedMkv, true, Deinterlace::Marked),
        (Clip::WovenFieldsMkv, true, Deinterlace::All),
    ] {
        let Some(p) = clip(c) else { continue };
        let name = c.file_name();
        let probe = flux_probe::probe_file(&p).unwrap_or_else(|e| panic!("{name}: {e}"));
        let asset = engine.prepare(&p).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        let v = asset.index.video_track().unwrap();
        assert_eq!(v.coded_interlaced(), Some(c != Clip::WovenFieldsMkv), "{name}");
        assert_eq!(v.interlaced(), Some(interlaced), "{name}");
        assert_eq!(probe.main_video().unwrap().interlaced, Some(interlaced), "{name}");
        assert_eq!(flux_hls::vtranscode::deinterlace(v), mode, "{name}");
        for client in ["chrome", "safari", "firefox", "appletv", "androidtv", "legacy"] {
            let caps = presets::by_name(client).unwrap();
            let mut a = engine.plan(&asset, &caps, &prefs);
            let mut b = flux_hls::plan_from_probe(&probe, &caps, &prefs);
            a.est_cpu_ms_per_min = 0;
            b.est_cpu_ms_per_min = 0;
            assert_eq!(a, b, "{name} / {client}: index plan vs probe plan");
        }
        let plan = engine.plan(&asset, &presets::chrome_desktop(), &prefs);
        assert_eq!(
            matches!(plan.video, VideoOp::Transcode { .. }),
            interlaced,
            "{name}: {:?}",
            plan.explain()
        );
        if !interlaced {
            continue;
        }
        let (file, segs) = write_rendition(
            &engine,
            &asset,
            &plan,
            Rendition::Video,
            &dir,
            &format!("{name}.chrome.mp4"),
        )
        .await;
        for (n, s) in segs.iter().enumerate() {
            assert_eq!(
                first_vcl(s),
                Some(5),
                "{name}: piece {n} does not start with an IDR"
            );
        }
        let frames = ffprobe_video_frames(&file).expect("ffprobe");
        assert_eq!(frames.len() as u32, c.frames().unwrap(), "{name}: frames out");
    }
}

/// An MP4's `fiel` decides like Matroska's FlagInterlaced, for the probe and
/// the index alike: the H.264 clip (a progressive SPS) with `fiel` set to
/// 2 fields is interlaced (fields woven into its frames: every frame is
/// deinterlaced), with 1 field progressive, without `fiel` the SPS's.
#[tokio::test]
async fn mp4_fiel_decides_interlacing_for_probe_and_index() {
    use flux_hls::encoders::Deinterlace;
    use mp4_atom::Encode;
    let Some(src) = clip(Clip::AnamorphicMp4) else {
        return;
    };
    let bytes = std::fs::read(&src).unwrap();
    // The top-level boxes: `moov` is last (no faststart), so rewriting it
    // moves no sample data.
    let mut pos = 0usize;
    let mut moov = None;
    while pos + 8 <= bytes.len() {
        let size = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let size = match size {
            0 => bytes.len() - pos,
            1 => u64::from_be_bytes(bytes[pos + 8..pos + 16].try_into().unwrap()) as usize,
            s => s,
        };
        if &bytes[pos + 4..pos + 8] == b"moov" {
            moov = Some(pos);
        }
        pos += size.max(8);
    }
    let at = moov.expect("moov");
    let engine = HlsEngine::new(EngineConfig::default());
    let dir = flux_test_media::scratch("hls-synthetic-fiel");
    for (fiel, interlaced, mode) in [
        (Some(2u8), true, Deinterlace::All),
        (Some(1), false, Deinterlace::Off),
        (None, false, Deinterlace::Off),
    ] {
        let mut m = mp4_atom::Moov::decode(&mut &bytes[at..]).unwrap();
        let mut set = 0;
        for t in &mut m.trak {
            for c in &mut t.mdia.minf.stbl.stsd.codecs {
                if let mp4_atom::Codec::Avc1(a) = c {
                    a.fiel = fiel.map(|n| mp4_atom::Fiel {
                        field_count: n,
                        field_order: if n == 2 { 1 } else { 0 },
                    });
                    set += 1;
                }
            }
        }
        assert_eq!(set, 1, "one avc1 entry");
        let mut out = bytes[..at].to_vec();
        m.encode(&mut out).unwrap();
        let p = dir.join(format!("fiel-{fiel:?}.mp4"));
        std::fs::write(&p, &out).unwrap();
        let probe = flux_probe::probe_file(&p).unwrap();
        let asset = engine.prepare(&p).await.unwrap();
        let v = asset.index.video_track().unwrap();
        assert_eq!(v.coded_interlaced(), Some(false), "{fiel:?}");
        assert_eq!(v.extra.interlaced, fiel.map(|n| n == 2), "{fiel:?}");
        assert_eq!(v.interlaced(), Some(interlaced), "{fiel:?}");
        assert_eq!(
            probe.main_video().unwrap().interlaced,
            Some(interlaced),
            "{fiel:?}"
        );
        assert_eq!(flux_hls::vtranscode::deinterlace(v), mode, "{fiel:?}");
        let caps = presets::chrome_desktop();
        let mut a = engine.plan(&asset, &caps, &PlanPrefs::default());
        let mut b = flux_hls::plan_from_probe(&probe, &caps, &PlanPrefs::default());
        a.est_cpu_ms_per_min = 0;
        b.est_cpu_ms_per_min = 0;
        assert_eq!(a, b, "{fiel:?}");
        assert_eq!(
            matches!(a.video, VideoOp::Transcode { .. }),
            interlaced,
            "{fiel:?}"
        );
    }
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
