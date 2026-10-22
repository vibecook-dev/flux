//! The ffprobe fallback on containers flux-probe doesn't parse (ASF, FLV,
//! MPEG-PS, RealMedia): their container and codecs are named (they were
//! all `unknown`/`other`), and a cut FLV gets the duration of its tags. On
//! clips made with ffmpeg at test time; each case skips, with the reason,
//! when ffmpeg or an encoder it needs is missing.

use flux_core::{Codec, Container, Rational, TrackKind};
use flux_probe::{probe_file_with, ProbeOptions};
use flux_test_media::{clip, Clip};

fn probe(c: Clip) -> Option<flux_core::ProbeResult> {
    flux_test_media::ffprobe()?;
    let p = clip(c)?;
    let opts = ProbeOptions {
        ffprobe_fallback: true,
        ..Default::default()
    };
    Some(probe_file_with(&p, &opts).unwrap_or_else(|e| panic!("{}: {e}", c.file_name())))
}

#[test]
fn old_containers_and_codecs_are_named() {
    let cases = [
        (Clip::Wmv2WmaAsf, Container::Asf, Codec::Wmv2, Codec::Wma),
        (Clip::MsClockWmv1Asf, Container::Asf, Codec::Wmv1, Codec::Wma),
        (Clip::Flv1AacFlv, Container::Flv, Codec::Flv1, Codec::Aac),
        (Clip::Mpeg1Mp2Ps, Container::MpegPs, Codec::Mpeg1Video, Codec::Mp2),
        (
            Clip::Rv20Ac3Rm,
            Container::RealMedia,
            Codec::RealVideo,
            Codec::Ac3,
        ),
    ];
    for (c, container, video, audio) in cases {
        let Some(r) = probe(c) else { continue };
        let name = c.file_name();
        assert_eq!(r.container, container, "{name}");
        let v = r.main_video().unwrap();
        assert_eq!(v.codec, video, "{name}");
        let a = r.tracks.iter().find(|t| t.kind == TrackKind::Audio).unwrap();
        assert_eq!(a.codec, audio, "{name}");
        // A frame rate, never a millisecond clock (the WMV 7 clip's
        // r_frame_rate is 1000/1 with no average: none rather than 1000).
        assert!(
            v.frame_rate.is_none_or(|f| f.as_f64() <= 60.0),
            "{name}: {:?}",
            v.frame_rate
        );
        let ms = r.duration_ms.unwrap();
        assert!((9_800..10_300).contains(&ms), "{name}: {ms} ms");
    }
    if let Some(r) = probe(Clip::Wmv2WmaAsf) {
        assert_eq!(
            r.main_video().unwrap().frame_rate,
            Some(Rational { num: 25, den: 1 })
        );
    }
}

/// The first 40 % of a 10 s FLV: its header still says 10 s, its tags end
/// near 4 s, and the catalog gets the tags' duration.
#[test]
fn a_cut_flv_has_the_duration_of_its_tags() {
    let Some(r) = probe(Clip::CutFlv) else { return };
    let ms = r.duration_ms.unwrap();
    assert!((3_400..4_400).contains(&ms), "{ms} ms");
    assert!(
        r.warnings.iter().any(|w| w.contains("FLV header says")),
        "{:?}",
        r.warnings
    );
}
