//! probe → index → HLS segment → transcode on clips made with ffmpeg at
//! test time (`flux_test_media::clip`), so CI exercises the real paths on
//! every platform. The video transcodes are forced through libx264 (the
//! Linux software path; `EngineConfig::video_encoder`), and checked with
//! ffprobe: every source frame is there, and every piece starts on an IDR.
//! Each test skips, with the reason, when ffmpeg, ffprobe or an encoder a
//! clip needs is missing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use flux_core::{Codec, PlaybackPlan, SubtitleFile, SubtitleOp, VideoOp, SUBTITLE_FILE_TRACK_BASE};
use flux_hls::playlist::{HlsResource, Rendition};
use flux_hls::{presets, Asset, EngineConfig, HlsEngine, PlanPrefs, VideoEncoder, WebVttBody};
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
    let inside = (208, 268, 432, 312);
    let above = (0, 0, 640, 200);
    for (t, shown) in [(0.5, false), (1.5, true), (2.5, true), (3.5, false), (6.5, false)] {
        let (fa, fb) = (gray_frame(&ff, &a, t), gray_frame(&ff, &b, t));
        assert_eq!(fa.len(), 640 * 360, "t = {t}");
        let (d_in, d_out) = (diff_in(&fa, &fb, inside), diff_in(&fa, &fb, above));
        assert!(
            d_out < 8.0,
            "t = {t}: the pictures differ away from the subtitle ({d_out:.1})"
        );
        if shown {
            assert!(d_in > 40.0, "t = {t}: no subtitle drawn ({d_in:.1})");
            let white = fa[290 * 640 + 320];
            assert!(white > 200, "t = {t}: the box is not white ({white})");
        } else {
            assert!(
                d_in < 8.0,
                "t = {t}: a subtitle drawn when none is shown ({d_in:.1})"
            );
        }
    }
}
