//! Containers only ffmpeg reads (ASF/WMV, FLV, MPEG-PS, RealMedia, AVI):
//! indexed without a packet scan, segmented on a time grid, and played
//! through time-based ffmpeg runs at a constant frame rate. On clips made
//! with ffmpeg at test time (`flux_test_media::clip`), transcoded by
//! libx264; each test skips, with the reason, when a tool or encoder is
//! missing.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use flux_core::{AudioOp, Codec, Container, PlaybackPlan, Rational, Reason, TrackKind, VideoOp};
use flux_hls::index::{IndexMethod, Layout};
use flux_hls::playlist::{HlsResource, Rendition};
use flux_hls::{presets, Asset, AudioKey, EngineConfig, HlsEngine, HlsError, PlanPrefs, VideoEncoder};
use flux_test_media::{clip, ffprobe_video_frames, Clip};
use mp4_atom::{Any, Decode};

fn hls_only() -> PlanPrefs {
    PlanPrefs {
        allow_direct_play: false,
        ..Default::default()
    }
}

/// An engine whose video transcodes use libx264, or `None` when this
/// ffmpeg has none.
fn x264_engine() -> Option<HlsEngine> {
    if flux_test_media::ffprobe().is_none() || !flux_test_media::has_encoder("libx264") {
        return None;
    }
    Some(HlsEngine::new(EngineConfig {
        video_encoder: Some(VideoEncoder::Libx264),
        video_lookahead: 0,
        audio_lookahead: 0,
        ..Default::default()
    }))
}

/// (tfdt, [(duration, sync)]) of the first traf of a media segment.
fn trun(seg: &[u8]) -> (u64, Vec<(u32, bool)>) {
    let mut cur = std::io::Cursor::new(seg);
    loop {
        if let Any::Moof(m) = Any::decode(&mut cur).expect("box") {
            let traf = &m.traf[0];
            let tfdt = traf.tfdt.as_ref().expect("tfdt").base_media_decode_time;
            let s = traf.trun[0]
                .entries
                .iter()
                .map(|e| (e.duration.unwrap_or(0), e.flags.unwrap_or(0) & 0x0001_0000 == 0))
                .collect();
            return (tfdt, s);
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

async fn bytes(engine: &HlsEngine, asset: &Arc<Asset>, plan: &PlaybackPlan, res: HlsResource) -> Vec<u8> {
    engine
        .serve(asset, plan, res)
        .await
        .unwrap_or_else(|e| panic!("{res:?}: {e}"))
        .body
        .into_bytes()
        .await
        .unwrap()
        .to_vec()
}

/// Init + segments `ns` of a rendition, written to `<dir>/<name>`.
async fn write_segments(
    engine: &HlsEngine,
    asset: &Arc<Asset>,
    plan: &PlaybackPlan,
    r: Rendition,
    ns: impl IntoIterator<Item = u32>,
    dir: &Path,
    name: &str,
) -> (PathBuf, Vec<Vec<u8>>) {
    let mut all = bytes(engine, asset, plan, HlsResource::Init(r)).await;
    let mut segs = Vec::new();
    for n in ns {
        let b = bytes(engine, asset, plan, HlsResource::Segment(r, n)).await;
        all.extend_from_slice(&b);
        segs.push(b);
    }
    let p = dir.join(name);
    std::fs::write(&p, &all).unwrap();
    (p, segs)
}

/// The EXTINF durations of a media playlist.
fn extinfs(m3u8: &str) -> Vec<f64> {
    m3u8.lines()
        .filter_map(|l| l.strip_prefix("#EXTINF:"))
        .map(|l| l.trim_end_matches(',').parse().unwrap())
        .collect()
}

/// Every index-less container: no packet scan (no keyframe list), the
/// container and codecs named, the video transcoded in pieces that each
/// start with an IDR, hold their exact frame count and follow each other
/// without a gap, and every audio segment encoded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_less_containers_play_without_a_packet_scan() {
    let Some(engine) = x264_engine() else { return };
    let dir = flux_test_media::scratch("hls-old-formats");
    let cases = [
        (
            Clip::Wmv2WmaAsf,
            Container::Asf,
            Codec::Wmv2,
            Codec::Wma,
            (320, 240),
        ),
        (
            Clip::Flv1AacFlv,
            Container::Flv,
            Codec::Flv1,
            Codec::Aac,
            (320, 240),
        ),
        (
            Clip::Mpeg1Mp2Ps,
            Container::MpegPs,
            Codec::Mpeg1Video,
            Codec::Mp2,
            (320, 240),
        ),
        (
            Clip::Rv20Ac3Rm,
            Container::RealMedia,
            Codec::RealVideo,
            Codec::Ac3,
            (352, 288),
        ),
        (
            Clip::Mpeg4Mp2Avi,
            Container::Avi,
            Codec::Mpeg4Part2,
            Codec::Mp2,
            (320, 240),
        ),
    ];
    for (c, container, vcodec, acodec, (w, h)) in cases {
        let Some(p) = clip(c) else { continue };
        let name = c.file_name();
        let asset = engine.prepare(&p).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        let idx = &asset.index;
        assert_eq!(idx.method, IndexMethod::HeaderProbe, "{name}");
        assert!(idx.keyframes.is_empty(), "{name}: no packet scan");
        assert_eq!(idx.container, container, "{name}");
        let v = idx.video_track().unwrap();
        assert_eq!((v.codec, v.width, v.height), (vcodec, Some(w), Some(h)), "{name}");
        assert_eq!(v.frame_rate, Some(Rational { num: 25, den: 1 }), "{name}");
        let a = idx.tracks.iter().find(|t| t.kind == TrackKind::Audio).unwrap();
        assert_eq!(a.codec, acodec, "{name}");
        assert!(
            (idx.duration_secs() - 10.0).abs() < 0.3,
            "{name}: {}",
            idx.duration_secs()
        );

        let plan = engine.plan(&asset, &presets::chrome_desktop(), &hls_only());
        assert!(matches!(plan.video, VideoOp::Transcode { .. }), "{name}");
        assert!(
            plan.reasons.contains(&Reason::NoIndex { container }),
            "{name}: {:?}",
            plan.reasons
        );
        let explain = plan.explain().join(" / ");
        assert!(
            !explain.contains("Other") && !explain.contains("seek index"),
            "{name}: {explain}"
        );

        // Video: pieces of 2 s (50 frames) on the grid, each an IDR, in a row.
        let playlist =
            String::from_utf8(bytes(&engine, &asset, &plan, HlsResource::Playlist(Rendition::Video)).await)
                .unwrap();
        let durs = extinfs(&playlist);
        let pieces = engine.segment_count(&asset, &plan, &Rendition::Video) as u32;
        assert_eq!(durs.len() as u32, pieces, "{name}");
        let (file, segs) = write_segments(
            &engine,
            &asset,
            &plan,
            Rendition::Video,
            0..pieces,
            &dir,
            &format!("{name}.mp4"),
        )
        .await;
        let ts = v.timescale as u64;
        let mut next = 0u64;
        let mut frames = 0usize;
        for (n, s) in segs.iter().enumerate() {
            let (tfdt, samples) = trun(s);
            assert!(
                samples[0].1,
                "{name}: piece {n} does not start with a sync sample"
            );
            assert_eq!(
                first_vcl(s),
                Some(5),
                "{name}: piece {n} does not start with an IDR"
            );
            assert_eq!(tfdt, next, "{name}: piece {n} is not contiguous");
            let dur: u64 = samples.iter().map(|x| u64::from(x.0)).sum();
            assert_eq!(
                samples.len() as f64,
                (durs[n] * 25.0).round(),
                "{name}: piece {n} frames"
            );
            assert!(
                ((dur as f64 / ts as f64) - durs[n]).abs() < 1e-3,
                "{name}: piece {n} duration"
            );
            next = tfdt + dur;
            frames += samples.len();
        }
        let out = ffprobe_video_frames(&file).expect("ffprobe");
        assert_eq!(out.len(), frames, "{name}: frames decoded");
        assert!(frames >= 250, "{name}: {frames} frames");

        // Audio: every segment of every rendition has its frames.
        for ap in &plan.audio {
            assert!(matches!(ap.op, AudioOp::Transcode { .. }), "{name}");
            let r = Rendition::Audio(AudioKey::from_plan(ap));
            let n = engine.segment_count(&asset, &plan, &r) as u32;
            let (_, segs) =
                write_segments(&engine, &asset, &plan, r, 0..n, &dir, &format!("{name}.a.mp4")).await;
            let total: u64 = segs.iter().map(|s| trun(s).1.len() as u64 * 1024).sum();
            let rate = a.sample_rate.unwrap_or(48_000).min(48_000) as f64;
            assert!(
                (total as f64 / rate - idx.duration_secs()).abs() < 0.2,
                "{name}: audio {total}"
            );
        }
    }
}

/// Decodes `file`'s video and compares it, frame by frame, with the
/// source's from `from_s` on (`fps=25` grid, the source's own timeline):
/// the smallest PSNR of the first `n` frames.
fn min_psnr(file: &Path, src: &Path, from_s: f64, n: usize, size: (u32, u32)) -> f64 {
    let ff = flux_test_media::ffmpeg().unwrap();
    let graph = format!(
        "[1:v]fps=25:start_time=0,trim=start={from_s},setpts=PTS-STARTPTS,scale={}:{},format=yuv420p[r];\
         [0:v]setpts=PTS-STARTPTS,format=yuv420p[o];[o][r]psnr=stats_file=-",
        size.0, size.1
    );
    let out = Command::new(ff)
        .args(["-hide_banner", "-nostats", "-i"])
        .arg(file)
        .arg("-i")
        .arg(src)
        .args([
            "-filter_complex",
            &graph,
            "-frames:v",
            &n.to_string(),
            "-f",
            "null",
            "-",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let v: Vec<f64> = text
        .split_whitespace()
        .filter_map(|w| w.strip_prefix("psnr_avg:"))
        .map(|x| if x == "inf" { 99.0 } else { x.parse().unwrap() })
        .take(n)
        .collect();
    assert_eq!(v.len(), n, "psnr: {}", String::from_utf8_lossy(&out.stderr));
    v.into_iter().fold(f64::INFINITY, f64::min)
}

/// A piece requested cold (a seek) shows the source's pictures at its
/// time, frame for frame: MPEG-PS seeks land on any packet (the run starts
/// 1.5 GOPs early, or the first frames decode grey), and this FLV's video
/// starts 23 ms after its audio (the run starts half a second early, or the
/// frame on screen at the piece's start is dropped).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_piece_requested_cold_is_the_source_at_its_time() {
    if flux_test_media::ffmpeg().is_none() {
        return;
    }
    let dir = flux_test_media::scratch("hls-old-formats-seek");
    for c in [
        Clip::Mpeg1Mp2Ps,
        Clip::Flv1AacFlv,
        Clip::Wmv2WmaAsf,
        Clip::Mpeg4Mp2Avi,
    ] {
        let Some(engine) = x264_engine() else { return };
        let Some(p) = clip(c) else { continue };
        let name = c.file_name();
        let asset = engine.prepare(&p).await.unwrap();
        let plan = engine.plan(&asset, &presets::chrome_desktop(), &hls_only());
        // Piece 3 starts at 6 s: asked for first, it is a run's first piece
        // (the init segment then comes from that run).
        let seg = bytes(&engine, &asset, &plan, HlsResource::Segment(Rendition::Video, 3)).await;
        assert_eq!(first_vcl(&seg), Some(5), "{name}");
        let mut all = bytes(&engine, &asset, &plan, HlsResource::Init(Rendition::Video)).await;
        all.extend_from_slice(&seg);
        let file = dir.join(format!("{name}.p3.mp4"));
        std::fs::write(&file, all).unwrap();
        let v = asset.index.video_track().unwrap();
        let size = (v.width.unwrap(), v.height.unwrap());
        let psnr = min_psnr(&file, &p, 6.0, 50, size);
        assert!(
            psnr > 28.0,
            "{name}: piece 3 is not the source at 6 s (min PSNR {psnr:.1} dB)"
        );
    }
}

/// A seek past a file's last keyframe can decode nothing at all (MPEG-PS
/// seeks land on any packet; a RealMedia file whose index names a stream it
/// lacks did so for every target after its last keyframe): the run then
/// seeks further back, instead of answering 500 ("encoder stopped after 0
/// of 1 pieces"). Here the last piece (28 s) lies past the clip's last
/// I-frame (15 s) by more than any seek margin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_seek_that_decodes_nothing_seeks_further_back() {
    let Some(engine) = x264_engine() else { return };
    let Some(p) = clip(Clip::SparseKeysPs) else { return };
    let asset = engine.prepare(&p).await.unwrap();
    let plan = engine.plan(&asset, &presets::chrome_desktop(), &hls_only());
    let playlist =
        String::from_utf8(bytes(&engine, &asset, &plan, HlsResource::Playlist(Rendition::Video)).await)
            .unwrap();
    let durs = extinfs(&playlist);
    let last = durs.len() - 1;
    let start: f64 = durs[..last].iter().sum();
    assert!(start >= 25.0, "the last piece starts at {start} s");
    // Asked for first (a seek to the end), so it is its run's first piece.
    let seg = bytes(
        &engine,
        &asset,
        &plan,
        HlsResource::Segment(Rendition::Video, last as u32),
    )
    .await;
    assert_eq!(first_vcl(&seg), Some(5));
    let (_, samples) = trun(&seg);
    assert_eq!(samples.len() as f64, (durs[last] * 25.0).round());
    let mut all = bytes(&engine, &asset, &plan, HlsResource::Init(Rendition::Video)).await;
    all.extend_from_slice(&seg);
    let dir = flux_test_media::scratch("hls-old-formats-seek-back");
    let file = dir.join("sparse_keys.last.mp4");
    std::fs::write(&file, all).unwrap();
    let psnr = min_psnr(&file, &p, start, 25, (320, 240));
    assert!(
        psnr > 28.0,
        "the last piece is not the source at {start} s (min PSNR {psnr:.1} dB)"
    );
}

/// Two recordings joined byte for byte, the clock starting over at 10 s:
/// ffprobe calls the file 8 s long (the last clock) and its seeks can't
/// tell the parts apart. The playlist covers both parts, and a piece of
/// either, asked for cold, shows that part's pictures at its time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn joined_program_streams_play_both_parts() {
    let Some(engine) = x264_engine() else { return };
    let (Some(p), Some(first), Some(second)) = (
        clip(Clip::JoinedPs),
        clip(Clip::Mpeg1Mp2Ps),
        clip(Clip::Mpeg1Mp2PsB),
    ) else {
        return;
    };
    let asset = engine.prepare(&p).await.unwrap();
    let secs = asset.index.duration_secs();
    assert!((17.6..18.4).contains(&secs), "{secs} s");
    let Layout::External(l) = &asset.index.layout else {
        panic!("{:?}", asset.index.layout)
    };
    assert_eq!(l.parts.len(), 2, "{:?}", l.parts);
    assert!(
        (l.parts[1].start_ns as f64 / 1e9 - 10.0).abs() < 0.1,
        "{:?}",
        l.parts
    );
    let plan = engine.plan(&asset, &presets::chrome_desktop(), &hls_only());
    let dir = flux_test_media::scratch("hls-old-formats-joined");
    // Piece 6 (12–14 s) is the second recording at 2 s, piece 2 (4–6 s) the
    // first at 4 s; each the first piece of its own run.
    for (n, src, at) in [(6u32, &second, 2.0), (2, &first, 4.0)] {
        let seg = bytes(&engine, &asset, &plan, HlsResource::Segment(Rendition::Video, n)).await;
        assert_eq!(first_vcl(&seg), Some(5), "piece {n}");
        let mut all = bytes(&engine, &asset, &plan, HlsResource::Init(Rendition::Video)).await;
        all.extend_from_slice(&seg);
        let file = dir.join(format!("joined.p{n}.mp4"));
        std::fs::write(&file, all).unwrap();
        let psnr = min_psnr(&file, src, at, 50, (320, 240));
        assert!(
            psnr > 28.0,
            "piece {n} is not its part at {at} s (min PSNR {psnr:.1} dB)"
        );
    }
    for a in &plan.audio {
        let r = Rendition::Audio(AudioKey::from_plan(a));
        let n = engine.segment_count(&asset, &plan, &r) as u32;
        let (_, segs) = write_segments(&engine, &asset, &plan, r, 0..n, &dir, "joined.a.mp4").await;
        let total: u64 = segs.iter().map(|s| trun(s).1.len() as u64 * 1024).sum();
        assert!((total as f64 / 48_000.0 - secs).abs() < 0.2, "audio {total}");
    }
    // The catalog's probe agrees.
    let probed = flux_probe::ffprobe::probe(&p).unwrap();
    let ms = probed.duration_ms.unwrap();
    assert!((17_600..18_400).contains(&ms), "{ms} ms");
}

/// A download that stopped at 40 %: the FLV's header still says 10 s, but
/// the playlist ends where the tags do, so every segment (the last one
/// too) is produced. Likewise for an FLV written without `onMetaData`,
/// whose header says nothing (it was refused: "the file's duration is
/// unknown").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cut_flv_ends_where_its_tags_do() {
    let Some(engine) = x264_engine() else { return };
    for c in [Clip::CutFlv, Clip::CutBareFlv] {
        let Some(p) = clip(c) else { continue };
        let name = c.file_name();
        let asset = engine.prepare(&p).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        let secs = asset.index.duration_secs();
        assert!((3.4..4.4).contains(&secs), "{name}: {secs} s");
        if c == Clip::CutFlv {
            assert!(
                asset.index.warnings.iter().any(|w| w.contains("FLV header says")),
                "{:?}",
                asset.index.warnings
            );
        }
        let plan = engine.plan(&asset, &presets::chrome_desktop(), &hls_only());
        let dir = flux_test_media::scratch("hls-old-formats-cut");
        let n = engine.segment_count(&asset, &plan, &Rendition::Video) as u32;
        let (file, _) = write_segments(
            &engine,
            &asset,
            &plan,
            Rendition::Video,
            0..n,
            &dir,
            &format!("{name}.mp4"),
        )
        .await;
        let frames = ffprobe_video_frames(&file).expect("ffprobe").len() as f64;
        assert!(frames >= secs * 25.0 - 1.0, "{name}: {frames} frames");
        for a in &plan.audio {
            let r = Rendition::Audio(AudioKey::from_plan(a));
            let n = engine.segment_count(&asset, &plan, &r) as u32;
            write_segments(&engine, &asset, &plan, r, 0..n, &dir, &format!("{name}.a.mp4")).await;
        }
        // The catalog's probe agrees (flux-probe's ffprobe fallback).
        let probed = flux_probe::ffprobe::probe(&p).unwrap();
        let ms = probed.duration_ms.unwrap();
        assert!((3400..4400).contains(&ms), "{name}: {ms} ms");
    }
}

/// Decoded 16-bit stereo samples of an audio rendition's init + segment.
fn decode_s16(init: &[u8], seg: &[u8], dir: &Path, name: &str) -> Vec<i16> {
    let p = dir.join(name);
    let mut all = init.to_vec();
    all.extend_from_slice(seg);
    std::fs::write(&p, all).unwrap();
    let out = Command::new(flux_test_media::ffmpeg().unwrap())
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(&p)
        .args(["-f", "s16le", "-ac", "1", "-"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    out.stdout
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect()
}

fn rms(s: &[i16]) -> f64 {
    (s.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / s.len().max(1) as f64).sqrt()
}

/// Audio segments where the source has no audio packets are silence, in
/// place: segment 1 (6–12 s) lies inside a gap from 5 s to 13 s (it
/// answered 500, "encoder produced no frames"), and segment 2 (12–18 s) is
/// silent until 13 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_the_source_lacks_is_silence() {
    let Some(engine) = x264_engine() else { return };
    let Some(p) = clip(Clip::GapAudioFlv) else { return };
    let asset = engine.prepare(&p).await.unwrap();
    let plan = engine.plan(&asset, &presets::chrome_desktop(), &hls_only());
    let r = Rendition::Audio(AudioKey::from_plan(&plan.audio[0]));
    let dir = flux_test_media::scratch("hls-old-formats-gap");
    let init = bytes(&engine, &asset, &plan, HlsResource::Init(r)).await;
    let rate = 44_100.0;
    for n in 0..engine.segment_count(&asset, &plan, &r) as u32 {
        let seg = bytes(&engine, &asset, &plan, HlsResource::Segment(r, n)).await;
        let (tfdt, frames) = trun(&seg);
        let span = asset.segments.duration_secs(n as usize);
        assert!(
            (frames.len() as f64 * 1024.0 / rate - span).abs() < 0.05,
            "segment {n}: {} frames for {span} s",
            frames.len()
        );
        let pcm = decode_s16(&init, &seg, &dir, &format!("gap{n}.mp4"));
        let t0 = tfdt as f64 / rate;
        let at = |s: f64| ((s - t0) * rate) as usize;
        match n {
            1 => assert!(rms(&pcm) < 50.0, "segment 1 is not silent: {}", rms(&pcm)),
            2 => {
                assert!(rms(&pcm[at(12.1)..at(12.9)]) < 50.0, "before 13 s");
                assert!(rms(&pcm[at(13.2)..at(17.8)]) > 1000.0, "after 13 s");
            }
            _ => {}
        }
    }
}

/// ASF's millisecond clock is not a frame rate: ffprobe says 1000/1 for
/// this 29.97 fps WMV, which made the H.264 level 5.2 (NVENC refused it:
/// no video). The transcode runs at 29.97, and the playlist says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_millisecond_clock_is_not_a_frame_rate() {
    let Some(engine) = x264_engine() else { return };
    let Some(p) = clip(Clip::MsClockWmv1Asf) else {
        return;
    };
    let asset = engine.prepare(&p).await.unwrap();
    let v = asset.index.video_track().unwrap();
    assert_eq!(v.codec, Codec::Wmv1);
    assert_eq!(
        v.frame_rate,
        Some(Rational {
            num: 30000,
            den: 1001
        })
    );
    let plan = engine.plan(&asset, &presets::chrome_desktop(), &hls_only());
    let master = engine.master_playlist(&asset, &plan).unwrap();
    assert!(master.contains("FRAME-RATE=29.970"), "{master}");
    assert!(master.contains("avc1.640029"), "{master}");
    let dir = flux_test_media::scratch("hls-old-formats-msclock");
    let playlist =
        String::from_utf8(bytes(&engine, &asset, &plan, HlsResource::Playlist(Rendition::Video)).await)
            .unwrap();
    let durs = extinfs(&playlist);
    let n = durs.len() as u32;
    let (_, segs) = write_segments(
        &engine,
        &asset,
        &plan,
        Rendition::Video,
        0..n,
        &dir,
        "msclock.mp4",
    )
    .await;
    // 2.002 s pieces of 60 frames in the first 6.006 s segment; each piece
    // exactly its playlist duration at 29.97 fps.
    for (k, s) in segs.iter().enumerate() {
        let frames = trun(s).1.len();
        assert_eq!(frames as f64, (durs[k] * 30_000.0 / 1001.0).round(), "piece {k}");
        if k < 3 {
            assert_eq!(frames, 60, "piece {k}");
        }
    }
}

/// VP6 whose keyframes say 0 macroblock rows (ffmpeg: "Invalid size
/// 272x0"): no frame decodes, so the file is refused when it is opened,
/// with the reason, instead of failing every segment with a raw ffmpeg
/// error.
#[test]
fn undecodable_video_is_refused_when_opened() {
    let Some(ffprobe) = flux_test_media::ffprobe() else {
        return;
    };
    let mut b = b"FLV\x01\x01\x00\x00\x00\x09\x00\x00\x00\x00".to_vec();
    for k in 0..50u32 {
        // VP6F keyframe tag: frame type 1, codec 4; adjustment byte; intra
        // frame, version 8; 0 rows × 17 columns of macroblocks.
        let mut data = vec![0x14, 0x00, 0x00, 0x40, 0x00, 0x11, 0x00, 0x11];
        data.extend_from_slice(&[0x5A; 200]);
        let size = data.len() as u32;
        b.push(9);
        b.extend_from_slice(&size.to_be_bytes()[1..]);
        b.extend_from_slice(&(k * 40).to_be_bytes()[1..]);
        b.extend_from_slice(&[0, 0, 0, 0]);
        b.extend_from_slice(&data);
        b.extend_from_slice(&(size + 11).to_be_bytes());
    }
    let dir = flux_test_media::scratch("hls-old-formats-vp6");
    let path = dir.join("vp6_no_rows.flv");
    std::fs::write(&path, &b).unwrap();
    match Asset::open_with(&path, 6_000_000_000, Some(ffprobe.as_path())) {
        Err(HlsError::Unsupported(m)) => assert!(m.contains("can't be decoded"), "{m}"),
        other => panic!("{other:?}"),
    }
}
