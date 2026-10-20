//! The probe on clips made with ffmpeg at test time (`flux_test_media::clip`),
//! so every machine with ffmpeg (CI included) probes real files of each
//! container. Each case skips, with the reason, when ffmpeg or an encoder
//! it needs is missing.

use flux_core::{Codec, Container, HdrFormat, TrackKind};
use flux_probe::probe_file;
use flux_test_media::{clip, Clip};

/// What a clip should probe as: (clip, container, video codec, audio
/// codec, video size, seconds).
type Case = (Clip, Container, Codec, Option<Codec>, (u32, u32), f64);

const CASES: [Case; 6] = [
    (
        Clip::H264AacMp4,
        Container::Mp4,
        Codec::H264,
        Some(Codec::Aac),
        (640, 360),
        10.0,
    ),
    (
        Clip::HevcAc3Mkv,
        Container::Matroska,
        Codec::Hevc,
        Some(Codec::Ac3),
        (640, 360),
        10.0,
    ),
    (
        Clip::H264AacTs,
        Container::MpegTs,
        Codec::H264,
        Some(Codec::Aac),
        (640, 360),
        10.0,
    ),
    (
        Clip::HevcAacTs,
        Container::MpegTs,
        Codec::Hevc,
        Some(Codec::Aac),
        (640, 360),
        10.0,
    ),
    (
        Clip::Mpeg4Mp2Avi,
        Container::Avi,
        Codec::Mpeg4Part2,
        Some(Codec::Mp2),
        (320, 240),
        10.0,
    ),
    (
        Clip::Hdr10HevcMkv,
        Container::Matroska,
        Codec::Hevc,
        None,
        (640, 360),
        6.0,
    ),
];

#[test]
fn synthetic_clips_probe_as_made() {
    for (c, container, vcodec, acodec, (w, h), secs) in CASES {
        let Some(p) = clip(c) else { continue };
        let r = probe_file(&p).unwrap_or_else(|e| panic!("{}: {e}", c.file_name()));
        let name = c.file_name();
        assert_eq!(r.container, container, "{name}");
        let v = r.main_video().unwrap_or_else(|| panic!("{name}: no video"));
        assert_eq!(v.codec, vcodec, "{name}");
        assert_eq!((v.width, v.height), (Some(w), Some(h)), "{name}");
        let audio: Vec<_> = r.tracks.iter().filter(|t| t.kind == TrackKind::Audio).collect();
        match acodec {
            Some(a) => {
                assert_eq!(audio.len(), 1, "{name}");
                assert_eq!(audio[0].codec, a, "{name}");
                assert_eq!(audio[0].channels, Some(2), "{name}");
            }
            None => assert!(audio.is_empty(), "{name}"),
        }
        let d = r.duration_ms.unwrap_or_else(|| panic!("{name}: no duration")) as f64 / 1000.0;
        assert!((d - secs).abs() < 0.25, "{name}: {d} s");
        assert!((1..=4).contains(&r.reads), "{name}: {} reads", r.reads);
    }
}

#[test]
fn synthetic_hdr10_is_hdr10() {
    let Some(p) = clip(Clip::Hdr10HevcMkv) else { return };
    let r = probe_file(&p).unwrap();
    let v = r.main_video().unwrap();
    assert_eq!(v.bit_depth, Some(10));
    assert_eq!(v.hdr.as_ref().map(|h| h.format), Some(HdrFormat::Hdr10), "{v:?}");
}
