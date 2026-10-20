//! Real-media tests over the feature corpus (`$FLUX_TEST_MEDIA`). Skipped
//! (with a message) when the corpus is not configured.

use std::path::PathBuf;

use flux_core::{Codec, Container, ContentLightLevel, HdrFormat, MasteringDisplay, ProbeResult, TrackKind};
use flux_probe::{
    probe_file, probe_file_with, probe_frame_sei, ContainerHint, FrameSeiMode, FrameSeiProber, ProbeOptions,
    Prober, SeiStep, Step,
};

fn feature(name: &str) -> Option<PathBuf> {
    flux_test_media::media(&format!("features/{name}"))
}

fn probe(name: &str) -> Option<ProbeResult> {
    feature(name).map(|p| probe_file(&p).unwrap_or_else(|e| panic!("{name}: {e}")))
}

const HDR10_MASTERING: MasteringDisplay = MasteringDisplay {
    primaries: [[13250, 34500], [7500, 3000], [34000, 16000]],
    white_point: [15635, 16450],
    max_luminance: 10_000_000,
    min_luminance: 1,
};
const HDR10_CLL: ContentLightLevel = ContentLightLevel {
    max_cll: 1000,
    max_fall: 400,
};

/// Every clip agrees with the ffprobe facts recorded in manifest.json.
#[test]
fn manifest_agreement() {
    let Some(m) = feature("manifest.json") else { return };
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(m).unwrap()).unwrap();
    for entry in v.as_array().unwrap() {
        let file = entry["file"].as_str().unwrap();
        let ff = &entry["ffprobe"];
        let r = probe(file).unwrap();
        let dur = (ff["format"]["duration"].as_str().unwrap().parse::<f64>().unwrap() * 1000.0) as u64;
        let ours = r.duration_ms.unwrap_or_else(|| panic!("{file}: no duration"));
        assert!(
            ours.abs_diff(dur) <= 100.max(dur / 200),
            "{file}: duration {ours} vs {dur}"
        );
        let streams = ff["streams"].as_array().unwrap();
        assert_eq!(r.tracks.len(), streams.len(), "{file}: track count");
        for (t, s) in r.tracks.iter().zip(streams) {
            let codec = flux_probe::ffprobe::codec_of(s["codec_name"].as_str().unwrap());
            assert_eq!(t.codec, codec, "{file}: track {} codec", t.index);
            if let Some(w) = s["width"].as_u64() {
                assert_eq!(t.width, Some(w as u32), "{file}: width");
                assert_eq!(t.height, s["height"].as_u64().map(|h| h as u32), "{file}: height");
            }
            if let Some(c) = s["channels"].as_u64() {
                assert_eq!(t.channels, Some(c as u8), "{file}: channels");
            }
            if let Some(p) = s["pix_fmt"].as_str() {
                let depth = if p.contains("10") { 10 } else { 8 };
                assert_eq!(t.bit_depth, Some(depth), "{file}: bit depth");
            }
        }
    }
}

#[test]
fn mkv_multi_audio_subs_chapters() {
    let Some(r) = probe("h264_multi_audio_subs.mkv") else {
        return;
    };
    assert_eq!(r.container, Container::Matroska);
    assert_eq!(r.title.as_deref(), Some("flux feature corpus"));
    assert!(r.has_index, "SeekHead points to Cues");
    assert!(r.reads <= 2, "reads = {}", r.reads);
    let kinds: Vec<_> = r.tracks.iter().map(|t| (t.kind, t.codec)).collect();
    assert_eq!(
        kinds,
        vec![
            (TrackKind::Video, Codec::H264),
            (TrackKind::Audio, Codec::Aac),
            (TrackKind::Audio, Codec::Ac3),
            (TrackKind::Subtitle, Codec::Subrip),
            (TrackKind::Subtitle, Codec::Ass),
        ]
    );
    let v = &r.tracks[0];
    assert_eq!((v.profile.as_deref(), v.level), (Some("High"), Some(40)));
    assert_eq!(v.frame_rate.map(|f| (f.num, f.den)), Some((24000, 1001)));
    assert_eq!(r.tracks[1].language.as_deref(), Some("eng"));
    assert_eq!(r.tracks[1].title.as_deref(), Some("Stereo"));
    assert_eq!(r.tracks[2].language.as_deref(), Some("jpn"));
    assert_eq!(
        (r.tracks[2].channels, r.tracks[2].sample_rate),
        (Some(6), Some(48000))
    );
    assert_eq!(r.tracks[4].title.as_deref(), Some("Signs & Songs"));
    assert!(r.tracks[4].forced && !r.tracks[3].forced);
    let ch: Vec<_> = r
        .chapters
        .iter()
        .map(|c| (c.start_ms, c.end_ms, c.title.as_deref()))
        .collect();
    assert_eq!(
        ch,
        vec![
            (0, Some(20000), Some("Opening")),
            (20000, Some(40000), Some("Middle")),
            (40000, Some(60000), Some("End"))
        ]
    );
}

#[test]
fn hevc_hdr10_mkv_from_hvcc_and_keyframe() {
    let Some(path) = feature("hevc_main10_hdr10_eac3.mkv") else {
        return;
    };
    let r = probe_file(&path).unwrap();
    let v = r.main_video().unwrap();
    assert_eq!(
        (v.codec, v.profile.as_deref(), v.bit_depth),
        (Codec::Hevc, Some("Main 10"), Some(10))
    );
    let c = v.color.unwrap();
    assert_eq!((c.primaries, c.transfer, c.matrix), (Some(9), Some(16), Some(9)));
    let h = v.hdr.as_ref().unwrap();
    assert_eq!(h.format, HdrFormat::Hdr10);
    // x265 also put the HDR SEIs in hvcC, so the header pass alone has them.
    assert_eq!(h.mastering, Some(HDR10_MASTERING));
    assert_eq!(h.cll, Some(HDR10_CLL));
    assert!(!r.needs_frame_sei);
    assert_eq!(r.reads, 1);

    // The keyframe pass on its own (sans-IO) must recover the same values
    // from the first block's SEI NAL units.
    let data = std::fs::read(&path).unwrap();
    let f = run_frame_pass(&data);
    assert_eq!(f.mastering, Some(HDR10_MASTERING));
    assert_eq!(f.cll, Some(HDR10_CLL));
    assert!(!f.hdr10plus && !f.dovi_rpu);
    assert!(f.reads <= 2, "frame pass reads = {}", f.reads);
}

#[test]
fn hevc_hdr10_mp4_from_hvcc_and_first_sample() {
    let Some(path) = feature("hevc_main10_hdr10_hvc1.mp4") else {
        return;
    };
    let r = probe_file(&path).unwrap();
    assert_eq!(r.faststart, Some(true));
    let v = r.main_video().unwrap();
    assert_eq!(v.codec_tag, "hvc1");
    let h = v.hdr.as_ref().unwrap();
    assert_eq!(
        (h.format, h.mastering, h.cll),
        (HdrFormat::Hdr10, Some(HDR10_MASTERING), Some(HDR10_CLL))
    );
    let data = std::fs::read(&path).unwrap();
    let f = run_frame_pass(&data);
    assert_eq!((f.mastering, f.cll), (Some(HDR10_MASTERING), Some(HDR10_CLL)));
}

/// Strip the container-derived metadata and check that `probe_frame_sei`
/// (the path-based second pass) restores it from the keyframe.
#[test]
fn probe_frame_sei_restores_metadata() {
    for name in ["hevc_main10_hdr10_eac3.mkv", "hevc_main10_hdr10_hvc1.mp4"] {
        let Some(path) = feature(name) else { return };
        let mut r = probe_file_with(
            &path,
            &ProbeOptions {
                frame_sei: FrameSeiMode::Never,
                ..Default::default()
            },
        )
        .unwrap();
        let v = r.tracks.iter_mut().find(|t| t.kind == TrackKind::Video).unwrap();
        let h = v.hdr.as_mut().unwrap();
        h.mastering = None;
        h.cll = None;
        r.needs_frame_sei = true;
        probe_frame_sei(&path, &mut r).unwrap();
        let h = r.main_video().unwrap().hdr.as_ref().unwrap();
        assert_eq!(
            (h.format, h.mastering, h.cll),
            (HdrFormat::Hdr10, Some(HDR10_MASTERING), Some(HDR10_CLL)),
            "{name}"
        );
        assert!(!r.needs_frame_sei);
    }
}

fn run_frame_pass(data: &[u8]) -> flux_probe::FrameSei {
    let mut p = Prober::new(data.len() as u64, ContainerHint::NONE);
    let mut step = p.start().unwrap();
    while let Step::Need { offset, len } = step {
        step = p.feed(offset, slice(data, offset, len)).unwrap();
    }
    let mut sp: FrameSeiProber = p.into_frame_sei().expect("locator");
    let mut s = sp.start().unwrap();
    loop {
        match s {
            SeiStep::Need { offset, len } => s = sp.feed(offset, slice(data, offset, len)).unwrap(),
            SeiStep::Done(f) => return f,
        }
    }
}

fn slice(d: &[u8], off: u64, len: u32) -> &[u8] {
    let s = (off as usize).min(d.len());
    &d[s..(s + len as usize).min(d.len())]
}

#[test]
fn mp4_faststart_vs_moov_at_end() {
    let (Some(a), Some(b)) = (probe("h264_aac_faststart.mp4"), probe("h264_aac_moov_end.mp4")) else {
        return;
    };
    assert_eq!(a.faststart, Some(true));
    assert_eq!(b.faststart, Some(false));
    assert_eq!(a.reads, 1);
    assert_eq!(b.reads, 2, "head + tail");
    assert!(a.has_index && b.has_index);
    for r in [&a, &b] {
        assert_eq!(r.tracks.len(), 2);
        assert_eq!(r.tracks[1].profile.as_deref(), Some("LC"));
        assert_eq!(r.tracks[0].language.as_deref(), Some("und"));
        assert!(r.tracks[0].default);
    }
}

#[test]
fn av1_opus_matroska_and_webm() {
    let (Some(mkv), Some(webm)) = (probe("av1_opus.mkv"), probe("av1_opus.webm")) else {
        return;
    };
    assert_eq!(mkv.container, Container::Matroska);
    assert_eq!(webm.container, Container::WebM);
    for r in [&mkv, &webm] {
        let v = &r.tracks[0];
        assert_eq!(
            (v.codec, v.profile.as_deref(), v.bit_depth, v.level),
            (Codec::Av1, Some("Main"), Some(10), Some(8))
        );
        assert_eq!(v.hdr.as_ref().map(|h| h.format), Some(HdrFormat::Sdr));
        assert_eq!(
            (r.tracks[1].codec, r.tracks[1].sample_rate),
            (Codec::Opus, Some(48000))
        );
    }
}

#[test]
fn lossless_and_dts_audio() {
    let (Some(thd), Some(flac), Some(dts)) = (
        probe("h264_truehd71.mkv"),
        probe("h264_flac.mkv"),
        probe("h264_dts51.mkv"),
    ) else {
        return;
    };
    assert_eq!(
        (
            thd.tracks[1].codec,
            thd.tracks[1].channels,
            thd.tracks[1].bit_depth
        ),
        (Codec::TrueHd, Some(6), Some(24))
    );
    assert_eq!(
        (flac.tracks[1].codec, flac.tracks[1].bit_depth),
        (Codec::Flac, Some(16))
    );
    assert_eq!(
        (
            dts.tracks[1].codec,
            dts.tracks[1].channels,
            dts.tracks[1].bit_depth
        ),
        (Codec::Dts, Some(6), None)
    );
}

#[test]
fn mpeg_ts_and_m2ts() {
    let (Some(ts), Some(m2ts)) = (probe("mpeg2_ac3.ts"), probe("h264_aac.m2ts")) else {
        return;
    };
    assert_eq!(ts.container, Container::MpegTs);
    let v = &ts.tracks[0];
    assert_eq!(
        (v.codec, v.width, v.height, v.level),
        (Codec::Mpeg2Video, Some(1920), Some(1080), Some(4))
    );
    assert_eq!((ts.tracks[1].codec, ts.tracks[1].channels), (Codec::Ac3, Some(6)));
    assert!(ts.reads <= 3, "ts reads = {}", ts.reads);

    assert_eq!(m2ts.container, Container::M2ts);
    assert_eq!(m2ts.tracks.len(), 2);
    assert_eq!((m2ts.tracks[0].codec, m2ts.tracks[0].id), (Codec::H264, 0x1011));
    // Stream type 0x06 without a descriptor: identified by sniffing ADTS.
    assert_eq!(
        (m2ts.tracks[1].codec, m2ts.tracks[1].profile.as_deref()),
        (Codec::Aac, Some("LC"))
    );
    assert!(!m2ts.has_index);
}

#[test]
fn avi_mpeg4_mp3() {
    let Some(r) = probe("mpeg4_mp3.avi") else { return };
    assert_eq!(r.container, Container::Avi);
    assert_eq!(r.duration_ms, Some(30040));
    let v = &r.tracks[0];
    assert_eq!(
        (v.codec, v.codec_tag.as_str(), v.width, v.height),
        (Codec::Mpeg4Part2, "xvid", Some(720), Some(400))
    );
    assert_eq!(v.frame_rate.map(|f| (f.num, f.den)), Some((25, 1)));
    assert_eq!(
        (r.tracks[1].codec, r.tracks[1].sample_rate),
        (Codec::Mp3, Some(48000))
    );
    assert_eq!(r.reads, 1);
}

#[test]
fn two_hour_mkv_is_one_read() {
    let Some(r) = probe("long_2h_360p.mkv") else {
        return;
    };
    assert_eq!(r.duration_ms, Some(7_200_021));
    assert_eq!(r.reads, 1, "Cues/Clusters must not be read");
    assert_eq!(r.tracks[0].profile.as_deref(), Some("Constrained Baseline"));
}

/// The headline I/O goal: 1–4 reads per file across the whole corpus.
#[test]
fn read_budget_across_corpus() {
    let Ok(rd) = std::fs::read_dir(flux_test_media::media_dir("features")) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if !flux_probe::is_media_path(&p) {
            continue;
        }
        let r = probe_file(&p).unwrap();
        assert!((1..=4).contains(&r.reads), "{}: {} reads", p.display(), r.reads);
        assert!(r.bytes_read <= 2 << 20, "{}: {} bytes", p.display(), r.bytes_read);
    }
}
