//! MP4 clips rewritten into shapes found in a large personal library
//! (`flux_test_media::moov`): signed composition offsets in a version-0
//! `ctts`, and text in a legacy code page plus junk in `moov`. Each must
//! index, plan and remux like the clean clip it was made from, and decode
//! without errors. Each test skips, with the reason, without ffmpeg or
//! ffprobe.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use flux_core::{PlaybackPlan, VideoOp};
use flux_hls::playlist::{HlsResource, Rendition};
use flux_hls::{presets, Asset, AudioKey, EngineConfig, HlsEngine, PlanPrefs};
use flux_test_media::moov::{with_moov, Bx};
use flux_test_media::{clip, ffprobe_video_frames, Clip};

fn hls_only() -> PlanPrefs {
    PlanPrefs {
        allow_direct_play: false,
        ..Default::default()
    }
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b[..4].try_into().unwrap())
}

/// Rewrites a video `trak` the way QuickTime encoders write one: the same
/// presentation, but composition offsets lowered by `s` (some go negative)
/// in a version-0 `ctts`, and edit media times lowered by `s` to match.
/// Returns `s`.
fn negative_offsets_in_a_version_0_ctts(trak: &mut Bx) -> i64 {
    let elst = trak.find_mut(b"elst").expect("an edit list");
    let v1 = elst.body[0] == 1;
    let (entry, at) = if v1 { (20, 8) } else { (12, 4) };
    let media_time = |b: &[u8], i: usize| -> i64 {
        let p = 8 + i * entry + at;
        if v1 {
            i64::from_be_bytes(b[p..p + 8].try_into().unwrap())
        } else {
            be32(&b[p..]) as i32 as i64
        }
    };
    let edits = be32(&elst.body[4..]) as usize;
    let times: Vec<i64> = (0..edits).map(|i| media_time(&elst.body, i)).collect();
    let ctts = trak.find_mut(b"ctts").expect("B-frames: a ctts");
    let n = be32(&ctts.body[4..]) as usize;
    let offsets: Vec<i64> = (0..n)
        .map(|i| be32(&ctts.body[12 + 8 * i..]) as i32 as i64)
        .collect();
    let s = times
        .iter()
        .copied()
        .filter(|&t| t >= 0)
        .fold(offsets[0], i64::min);
    assert!(s > 0 && offsets.iter().any(|&o| o < s), "{s} {offsets:?}");
    ctts.body[0] = 0;
    for (i, o) in offsets.iter().enumerate() {
        let p = 12 + 8 * i;
        ctts.body[p..p + 4].copy_from_slice(&((o - s) as i32 as u32).to_be_bytes());
    }
    let elst = trak.find_mut(b"elst").unwrap();
    for (i, t) in times.iter().enumerate().filter(|(_, t)| **t >= 0) {
        let p = 8 + i * entry + at;
        if v1 {
            elst.body[p..p + 8].copy_from_slice(&(t - s).to_be_bytes());
        } else {
            elst.body[p..p + 4].copy_from_slice(&((t - s) as i32).to_be_bytes());
        }
    }
    s
}

/// Init and every media segment of a rendition, also written to `path`.
async fn rendition(
    engine: &HlsEngine,
    asset: &Arc<Asset>,
    plan: &PlaybackPlan,
    r: Rendition,
    path: &Path,
) -> Vec<Vec<u8>> {
    let init = engine
        .serve(asset, plan, HlsResource::Init(r))
        .await
        .unwrap()
        .body
        .into_bytes()
        .await
        .unwrap()
        .to_vec();
    let mut parts = vec![init];
    for n in 0..engine.segment_count(asset, plan, &r) as u32 {
        let b = engine
            .media_segment(asset, plan, &r, n)
            .await
            .unwrap_or_else(|e| panic!("{}: segment {n}: {e}", path.display()))
            .into_bytes()
            .await
            .unwrap();
        parts.push(b.to_vec());
    }
    std::fs::write(path, parts.concat()).unwrap();
    parts
}

/// Errors ffmpeg prints decoding `path` (none for a clean stream; a remux
/// with colliding decode times gets "non monotonically increasing dts").
fn decode_errors(path: &Path) -> Vec<String> {
    let ff = flux_test_media::ffmpeg().expect("ffmpeg");
    let out = Command::new(ff)
        .args(["-hide_banner", "-v", "error", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .map(str::to_string)
        .collect()
}

/// The first presentation time in `path`, in seconds (ffprobe).
fn start_time(path: &Path) -> f64 {
    let out = Command::new(flux_test_media::ffprobe().expect("ffprobe"))
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=start_time",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

/// EXTINF durations of a media playlist.
fn extinf(playlist: &str) -> Vec<String> {
    playlist
        .lines()
        .filter(|l| l.starts_with("#EXTINF:"))
        .map(str::to_string)
        .collect()
}

fn tools() -> bool {
    flux_test_media::ffmpeg().is_some() && flux_test_media::ffprobe().is_some()
}

/// Negative composition offsets in a version-0 `ctts` were read as
/// unsigned: 2^32 ticks late, so a remux had a last segment about
/// 2^32/fps seconds long and colliding decode times ("non monotonically
/// increasing dts" in every segment pair; 324 files of a real library).
/// Signed, the clip plays exactly like the clean one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn negative_offsets_in_a_version_0_ctts_play_like_the_clean_clip() {
    if !tools() {
        return;
    }
    let Some(src) = clip(Clip::H264AacMp4) else { return };
    let dir = flux_test_media::scratch("mp4-negative-ctts");
    let bytes = std::fs::read(&src).unwrap();
    let neg = with_moov(&bytes, |m| {
        negative_offsets_in_a_version_0_ctts(m.traks_mut(b"vide").remove(0));
    });
    let neg_path = dir.join("negative-ctts.mp4");
    std::fs::write(&neg_path, &neg).unwrap();
    // ffmpeg reads both alike (it reads ctts offsets as signed).
    let (a, b) = (
        ffprobe_video_frames(&src).unwrap(),
        ffprobe_video_frames(&neg_path).unwrap(),
    );
    assert_eq!(a.len(), 250);
    assert_eq!(a, b, "the rewrite changes no presentation time");

    let engine = HlsEngine::new(EngineConfig::default());
    let clean = engine.prepare(&src).await.unwrap();
    let asset = engine.prepare(&neg_path).await.unwrap();
    assert_eq!(asset.index.keyframes, clean.index.keyframes);
    assert_eq!(asset.index.duration_ns, clean.index.duration_ns);
    let (v, cv) = (
        asset.index.video_track().unwrap(),
        clean.index.video_track().unwrap(),
    );
    assert_eq!(
        v.extra.end_ns, cv.extra.end_ns,
        "the video ends with its last frame"
    );
    assert!(v.extra.end_ns.unwrap() <= asset.index.duration_ns + 100_000_000);

    // A remux (the path Chrome took): the same segments, every frame at its
    // time, decode times increasing, audio where it was.
    let caps = presets::chrome_desktop();
    let (plan, cplan) = (
        engine.plan(&asset, &caps, &hls_only()),
        engine.plan(&clean, &caps, &hls_only()),
    );
    assert!(matches!(plan.video, VideoOp::Copy { .. }), "{:?}", plan.explain());
    assert_eq!(
        extinf(&engine.media_playlist(&asset, &plan, &Rendition::Video).unwrap()),
        extinf(&engine.media_playlist(&clean, &cplan, &Rendition::Video).unwrap())
    );
    let (out, cout) = (dir.join("neg-video.mp4"), dir.join("clean-video.mp4"));
    rendition(&engine, &asset, &plan, Rendition::Video, &out).await;
    rendition(&engine, &clean, &cplan, Rendition::Video, &cout).await;
    assert_eq!(decode_errors(&out), Vec::<String>::new());
    let (f, cf) = (
        ffprobe_video_frames(&out).unwrap(),
        ffprobe_video_frames(&cout).unwrap(),
    );
    assert_eq!(f.len(), 250);
    let rel = |x: &[(f64, bool)]| {
        x.iter()
            .map(|(t, k)| ((t - x[0].0) * 1e6).round() as i64 * 2 + *k as i64)
            .collect::<Vec<_>>()
    };
    assert_eq!(rel(&f), rel(&cf), "frames at the same times");
    let a = Rendition::Audio(AudioKey::from_plan(&plan.audio[0]));
    let (aout, caout) = (dir.join("neg-audio.mp4"), dir.join("clean-audio.mp4"));
    rendition(&engine, &asset, &plan, a, &aout).await;
    rendition(&engine, &clean, &cplan, a, &caout).await;
    let av = start_time(&out) - start_time(&aout);
    let cav = start_time(&cout) - start_time(&caout);
    assert!((av - cav).abs() < 0.001, "A/V offset {av} vs {cav}");
}

/// A last `stts` delta of nearly 2^32 (a negative correction written
/// unsigned) made the last remuxed segment ~2^32 ticks long: playlists of
/// ~172 000 s for 24 files of a real library. Clipped to a tick, as ffmpeg
/// does, the clip plays like the clean one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_last_stts_delta_of_nearly_2_32_plays_like_the_clean_clip() {
    if !tools() {
        return;
    }
    let Some(src) = clip(Clip::H264AacMp4) else { return };
    let dir = flux_test_media::scratch("mp4-huge-stts");
    let bytes = std::fs::read(&src).unwrap();
    let file = with_moov(&bytes, |m| {
        let v = m.traks_mut(b"vide").remove(0);
        let ts = be32(&v.find_mut(b"mdhd").unwrap().body[12..]);
        let stts = v.find_mut(b"stts").unwrap();
        let n = be32(&stts.body[4..]) as usize;
        let last = 8 + 8 * (n - 1);
        let count = be32(&stts.body[last..]);
        assert!(count > 1, "{n} entries");
        // The last sample's delta becomes −2 s, written unsigned.
        stts.body[last..last + 4].copy_from_slice(&(count - 1).to_be_bytes());
        stts.body.extend_from_slice(&1u32.to_be_bytes());
        stts.body.extend_from_slice(&(-2 * ts as i64).to_be_bytes()[4..]);
        stts.body[4..8].copy_from_slice(&(n as u32 + 1).to_be_bytes());
    });
    let path = dir.join("huge-stts.mp4");
    std::fs::write(&path, &file).unwrap();

    let engine = HlsEngine::new(EngineConfig::default());
    let clean = engine.prepare(&src).await.unwrap();
    let asset = engine.prepare(&path).await.unwrap();
    let frame = 50_000_000; // 25 fps
    let end = |a: &Asset| a.index.video_track().unwrap().extra.end_ns.unwrap();
    assert!(
        (end(&asset) - end(&clean)).abs() <= frame,
        "{} vs {}",
        end(&asset),
        end(&clean)
    );
    assert!(
        asset.index.warnings.iter().any(|w| w.contains("clipped")),
        "{:?}",
        asset.index.warnings
    );
    let caps = presets::chrome_desktop();
    let (plan, cplan) = (
        engine.plan(&asset, &caps, &hls_only()),
        engine.plan(&clean, &caps, &hls_only()),
    );
    let total = |p: String| -> f64 {
        p.lines()
            .filter_map(|l| l.strip_prefix("#EXTINF:"))
            .map(|l| l.split(',').next().unwrap().parse::<f64>().unwrap())
            .sum()
    };
    let t = total(engine.media_playlist(&asset, &plan, &Rendition::Video).unwrap());
    let ct = total(engine.media_playlist(&clean, &cplan, &Rendition::Video).unwrap());
    assert!((t - ct).abs() <= 0.05, "{t} vs {ct}");
    let out = dir.join("video.mp4");
    rendition(&engine, &asset, &plan, Rendition::Video, &out).await;
    assert_eq!(decode_errors(&out), Vec::<String>::new());
    assert_eq!(ffprobe_video_frames(&out).unwrap().len(), 250);
}

/// `udta` with iTunes items whose text is in a legacy code page.
fn udta_with_ilst(text: &[u8]) -> Bx {
    let data = Bx::leaf(b"data", [&[0u8, 0, 0, 1, 0, 0, 0, 0][..], text].concat()).to_bytes();
    let ilst = Bx::leaf(
        b"ilst",
        [
            Bx::leaf(b"\xa9nam", data.clone()).to_bytes(),
            Bx::leaf(b"cprt", data).to_bytes(),
        ]
        .concat(),
    );
    let hdlr = Bx::leaf(b"hdlr", [&[0u8; 8][..], b"mdir", b"appl", &[0; 9]].concat());
    let meta = Bx::leaf(
        b"meta",
        [&[0u8; 4][..], &hdlr.to_bytes(), &ilst.to_bytes()].concat(),
    );
    Bx::container(b"udta", vec![meta])
}

/// Handler names and iTunes text in a legacy code page, a binary `alis`
/// data reference, sample tables, an `avcC` and an `esds` with bytes past
/// their entries, a track with an empty `stbl`, and junk after the last box
/// of `moov` and of a `trak` each failed the index (501 "flux can't stream
/// this file to this device yet"), and the junk failed the probe too ("box
/// overruns its parent"). The file now probes, indexes and remuxes byte for
/// byte like the clean clip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_text_and_junk_in_moov_remux_like_the_clean_clip() {
    if !tools() {
        return;
    }
    let Some(src) = clip(Clip::H264AacMp4) else { return };
    let dir = flux_test_media::scratch("mp4-legacy-moov");
    // "视频" in GBK: not UTF-8.
    const GBK: &[u8] = b"\xca\xd3\xc6\xb5";
    let bytes = std::fs::read(&src).unwrap();
    let bad = with_moov(&bytes, |m| {
        m.visit(&mut |_, b| match &b.kind {
            b"hdlr" => {
                b.body.truncate(24);
                b.body.extend_from_slice(GBK);
                b.body.push(0);
            }
            b"dref" => {
                let alis = Bx::leaf(b"alis", [&[0u8, 0, 0, 0][..], GBK].concat()).to_bytes();
                b.body = [&[0u8, 0, 0, 0, 0, 0, 0, 1][..], &alis].concat();
            }
            b"stts" | b"stsc" | b"stsz" | b"stco" | b"stss" | b"ctts" => b.body.extend_from_slice(&[0; 4]),
            b"stsd" if &b.body[12..16] == b"avc1" => b.edit_entry_child(b"avcC", |c| [c, &[0, 0]].concat()),
            // Bytes past the ES_Descriptor: read as a descriptor that
            // overruns the box.
            b"stsd" => b.edit_entry_child(b"esds", |c| [c, &[1, 2, 3]].concat()),
            _ => {}
        });
        // A track whose stbl holds nothing (mp4-atom requires an stsd).
        let mut empty = m.traks_mut(b"soun")[0].clone();
        empty.visit(&mut |_, b| {
            if &b.kind == b"tkhd" {
                b.body[12..16].copy_from_slice(&9u32.to_be_bytes());
            }
            if &b.kind == b"stbl" {
                b.kids = Some(Vec::new());
            }
        });
        // As found: a 32-bit word read as a size far past its parent.
        let mut junk = vec![0x3c, 0xc7, 0x09, 0xf3, 0x03, 0x80, 0, 0];
        junk.extend((0..4096u32).map(|i| (i * 31 % 251) as u8));
        for t in m.traks_mut(b"vide") {
            t.kids.as_mut().unwrap().push(Bx::raw(junk.clone()));
        }
        let kids = m.kids.as_mut().unwrap();
        kids.retain(|k| &k.kind != b"udta");
        kids.push(empty);
        kids.push(udta_with_ilst(GBK));
        kids.push(Bx::raw(junk));
    });
    let path = dir.join("legacy-moov.mp4");
    std::fs::write(&path, &bad).unwrap();

    let probe = flux_probe::probe_file(&path).unwrap();
    let cprobe = flux_probe::probe_file(&src).unwrap();
    // flux-probe lists the empty track (as ffprobe does), with no codec.
    assert_eq!(probe.tracks[..2], cprobe.tracks[..]);
    assert_eq!(probe.duration_ms, cprobe.duration_ms);
    assert!(
        probe.warnings.iter().any(|w| w.contains("overran")),
        "{:?}",
        probe.warnings
    );

    let engine = HlsEngine::new(EngineConfig::default());
    let clean = engine.prepare(&src).await.unwrap();
    let asset = engine.prepare(&path).await.unwrap();
    assert_eq!(asset.index.tracks, clean.index.tracks);
    assert_eq!(asset.index.keyframes, clean.index.keyframes);
    for w in [
        "not UTF-8",
        "bytes past their entries",
        "esds",
        "left out",
        "overran",
    ] {
        assert!(
            asset.index.warnings.iter().any(|x| x.contains(w)),
            "{w}: {:?}",
            asset.index.warnings
        );
    }
    let caps = presets::chrome_desktop();
    let (plan, cplan) = (
        engine.plan(&asset, &caps, &hls_only()),
        engine.plan(&clean, &caps, &hls_only()),
    );
    assert_eq!(plan, cplan);
    let mut renditions = vec![Rendition::Video];
    renditions.extend(
        plan.audio
            .iter()
            .map(|a| Rendition::Audio(AudioKey::from_plan(a))),
    );
    for (i, r) in renditions.into_iter().enumerate() {
        let out = rendition(&engine, &asset, &plan, r, &dir.join(format!("bad-{i}.mp4"))).await;
        let cout = rendition(&engine, &clean, &cplan, r, &dir.join(format!("clean-{i}.mp4"))).await;
        assert!(out == cout, "rendition {i}: the same bytes as the clean clip's");
    }
    assert_eq!(decode_errors(&dir.join("bad-0.mp4")), Vec::<String>::new());
}

/// Appends `child` (a whole box) to the only sample entry of `stsd`.
fn push_entry_child(stsd: &mut Bx, child: &[u8]) {
    let b = &mut stsd.body;
    // stsd body: version/flags, entry_count, then the entry.
    let size = be32(&b[8..]) as usize;
    assert_eq!(size, b.len() - 8, "one sample entry");
    b.extend_from_slice(child);
    b[8..12].copy_from_slice(&((size + child.len()) as u32).to_be_bytes());
}

/// Header boxes with bytes past their fields (`mvhd`, `tkhd`, `mdhd`,
/// `vmhd`, `smhd`) and a `pasp` with bytes past its own, all of which
/// ffmpeg reads. mp4-atom rejected each: the audio track was left out (HLS
/// played the movie silently, AC-3 too), and the `pasp` made the video an
/// unknown entry (a transcode where Chrome plays the file as is). Each box
/// now ends after its fields: the file plans and remuxes byte for byte
/// like the same clip without the padding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn padded_header_and_entry_boxes_play_like_the_clean_clip() {
    if !tools() {
        return;
    }
    let Some(src) = clip(Clip::H264AacMp4) else { return };
    let dir = flux_test_media::scratch("mp4-padded-boxes");
    let bytes = std::fs::read(&src).unwrap();
    // A square-pixel `pasp` in the video's entry, as many muxers write one
    // (padded where the clip has its own).
    let pasp = |m: &mut Bx, pad: usize| {
        let stsd = m.traks_mut(b"vide").remove(0).find_mut(b"stsd").unwrap();
        if stsd.body.windows(4).any(|w| w == b"pasp") {
            stsd.edit_entry_child(b"pasp", |c| [c, &vec![0u8; pad][..]].concat());
        } else {
            let body = [&1u32.to_be_bytes()[..], &1u32.to_be_bytes(), &vec![0u8; pad][..]].concat();
            push_entry_child(stsd, &Bx::leaf(b"pasp", body).to_bytes());
        }
    };
    let reference = with_moov(&bytes, |m| pasp(m, 0));
    let padded = with_moov(&bytes, |m| {
        pasp(m, 4);
        m.visit(&mut |_, b| {
            if matches!(&b.kind, b"mvhd" | b"tkhd" | b"mdhd" | b"vmhd" | b"smhd") {
                b.body.extend_from_slice(&[0; 4]);
            }
        });
    });
    let (ref_path, path) = (dir.join("reference.mp4"), dir.join("padded.mp4"));
    std::fs::write(&ref_path, &reference).unwrap();
    std::fs::write(&path, &padded).unwrap();
    assert_eq!(
        decode_errors(&path),
        Vec::<String>::new(),
        "ffmpeg reads the padded file"
    );

    let engine = HlsEngine::new(EngineConfig::default());
    let clean = engine.prepare(&ref_path).await.unwrap();
    let asset = engine.prepare(&path).await.unwrap();
    assert_eq!(asset.index.tracks, clean.index.tracks);
    assert_eq!(asset.index.tracks.len(), 2, "video and sound");
    assert!(
        asset.index.warnings.iter().any(|w| w.contains("bytes past")),
        "{:?}",
        asset.index.warnings
    );
    let caps = presets::chrome_desktop();
    assert_eq!(
        engine.plan(&asset, &caps, &Default::default()),
        engine.plan(&clean, &caps, &Default::default()),
        "Chrome plays it as it plays the clean clip"
    );
    let (plan, cplan) = (
        engine.plan(&asset, &caps, &hls_only()),
        engine.plan(&clean, &caps, &hls_only()),
    );
    assert_eq!(plan, cplan);
    assert!(matches!(plan.video, VideoOp::Copy { .. }), "{:?}", plan.explain());
    assert!(!plan.audio.is_empty(), "{:?}", plan.explain());
    let mut renditions = vec![Rendition::Video];
    renditions.extend(
        plan.audio
            .iter()
            .map(|a| Rendition::Audio(AudioKey::from_plan(a))),
    );
    for (i, r) in renditions.into_iter().enumerate() {
        let out = rendition(&engine, &asset, &plan, r, &dir.join(format!("padded-{i}.mp4"))).await;
        let cout = rendition(
            &engine,
            &clean,
            &cplan,
            r,
            &dir.join(format!("reference-{i}.mp4")),
        )
        .await;
        assert!(out == cout, "rendition {i}: the same bytes as the reference's");
    }
}
