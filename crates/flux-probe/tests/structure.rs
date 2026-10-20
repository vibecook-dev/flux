//! Structural edge cases, made by byte-patching real files in memory:
//! Matroska without a SeekHead (linear walk), unknown-size Clusters, an
//! hvcC whose HDR SEIs are hidden (forces the keyframe pass), plus the
//! generated fixtures (fragmented MP4, H.264 PQ with SEI-only mastering,
//! HLG, QuickTime, LPCM M2TS, ffprobe fallback, lying extensions).

mod common;

use std::path::Path;

use common::{ebml_size, features, find, fixtures, probe_mf, vint_len, MemFile};
use flux_core::{Codec, Container, ContentLightLevel, HdrFormat, TrackKind};
use flux_probe::{
    probe_file, probe_file_with, probe_source, ContainerHint, FrameSeiMode, ProbeError, ProbeOptions,
};

const SEEK_HEAD: [u8; 4] = [0x11, 0x4d, 0x9b, 0x74];
const CLUSTER: [u8; 4] = [0x1f, 0x43, 0xb6, 0x75];

fn load(dir: &Path, name: &str) -> Option<MemFile> {
    let p = dir.join(name);
    let f = MemFile::load(&p, 2 << 20, 512 << 10);
    if f.is_none() {
        eprintln!("skipping: {} not present", p.display());
    }
    f
}

fn opts(mode: FrameSeiMode) -> ProbeOptions {
    ProbeOptions {
        frame_sei: mode,
        ..ProbeOptions::default()
    }
}

/// Replace the element at `at` (4-byte ID) with a Void element of the same
/// total length, so it disappears from the file's structure.
fn void_out(head: &mut [u8], at: usize) {
    let sl = vint_len(head[at + 4]);
    let mut size = 0u64;
    for (i, b) in head[at + 4..at + 4 + sl].iter().enumerate() {
        size = (size << 8) | u64::from(if i == 0 { b & (0xff >> sl) } else { *b });
    }
    // Void ID is 1 byte, so its size field grows by the 3 bytes saved.
    head[at] = 0xec;
    let enc = ebml_size(size, sl + 3);
    head[at + 1..at + 1 + enc.len()].copy_from_slice(&enc);
}

#[test]
fn mkv_without_seekhead_uses_linear_walk() {
    let Some(orig) = load(&features(), "h264_multi_audio_subs.mkv") else {
        return;
    };
    let mut f = orig.clone();
    let at = find(&f.head[..4096], &SEEK_HEAD).expect("SeekHead");
    void_out(&mut f.head, at);
    assert!(find(&f.head[..4096], &SEEK_HEAD).is_none());
    let a = probe_mf(&orig).unwrap();
    let b = probe_mf(&f).unwrap();
    assert!(
        b.warnings.iter().any(|w| w.contains("no SeekHead")),
        "{:?}",
        b.warnings
    );
    assert_eq!(a.tracks, b.tracks);
    assert_eq!(a.chapters, b.chapters);
    assert_eq!(a.duration_ms, b.duration_ms);
    assert!(a.has_index);
    // Cues sit after the clusters; without a SeekHead we cannot know about them.
    assert!(!b.has_index);
    assert_eq!(b.reads, 1);
}

#[test]
fn unknown_size_segment_and_cluster() {
    let Some(mut f) = load(&fixtures(), "h264_pq_sei.mkv") else {
        return;
    };
    let seg = find(&f.head[..256], &[0x18, 0x53, 0x80, 0x67]).expect("Segment");
    let sl = vint_len(f.head[seg + 4]);
    let ones = ebml_size((1u64 << (7 * sl)) - 1, sl);
    f.head[seg + 4..seg + 4 + sl].copy_from_slice(&ones);
    let cl = find(&f.head, &CLUSTER).expect("Cluster");
    let cl_sl = vint_len(f.head[cl + 4]);
    let ones = ebml_size((1u64 << (7 * cl_sl)) - 1, cl_sl);
    f.head[cl + 4..cl + 4 + cl_sl].copy_from_slice(&ones);

    let r = probe_source(f, ContainerHint::NONE, &opts(FrameSeiMode::IfNeeded)).unwrap();
    let h = r.main_video().unwrap().hdr.as_ref().unwrap();
    assert_eq!(h.format, HdrFormat::Hdr10);
    assert_eq!(
        h.cll,
        Some(ContentLightLevel {
            max_cll: 1000,
            max_fall: 400
        })
    );
    assert!(h.mastering.is_some());
}

/// Hide the HDR SEIs inside hvcC (rename their NAL type to a reserved one):
/// the header pass must then ask for the keyframe pass, which finds them in
/// the first block, in at most one extra read.
#[test]
fn hvcc_without_sei_triggers_keyframe_pass() {
    let Some(mut f) = load(&features(), "hevc_main10_hdr10_eac3.mkv") else {
        return;
    };
    let first_cluster = find(&f.head, &CLUSTER).expect("Cluster");
    let mut patched = 0;
    let mut i = 0;
    while let Some(k) = find(&f.head[i..first_cluster], &[0x4e, 0x01]) {
        f.head[i + k] = 41 << 1; // reserved NAL type 41
        patched += 1;
        i += k + 2;
    }
    assert!(patched >= 2, "expected SEI NALs in hvcC");

    let header_only = probe_source(f.clone(), ContainerHint::NONE, &opts(FrameSeiMode::Never)).unwrap();
    assert!(header_only.needs_frame_sei);
    let h = header_only.main_video().unwrap().hdr.as_ref().unwrap();
    assert_eq!((h.format, h.mastering, h.cll), (HdrFormat::Hdr10, None, None));

    let full = probe_source(f, ContainerHint::NONE, &opts(FrameSeiMode::IfNeeded)).unwrap();
    assert!(!full.needs_frame_sei);
    let h = full.main_video().unwrap().hdr.as_ref().unwrap();
    assert_eq!(
        h.cll,
        Some(ContentLightLevel {
            max_cll: 1000,
            max_fall: 400
        })
    );
    assert_eq!(h.mastering.map(|m| m.white_point), Some([15635, 16450]));
    assert!(full.reads <= 2, "reads = {}", full.reads);
}

#[test]
fn h264_pq_mastering_only_in_sei() {
    for name in ["h264_pq_sei.mkv", "h264_pq_sei.mp4"] {
        let p = fixtures().join(name);
        if !p.exists() {
            return;
        }
        let header = probe_file_with(&p, &opts(FrameSeiMode::Never)).unwrap();
        assert!(header.needs_frame_sei, "{name}: avcC carries no SEI");
        let r = probe_file(&p).unwrap();
        assert!(!r.needs_frame_sei);
        let v = r.main_video().unwrap();
        assert_eq!(
            (v.codec, v.bit_depth, v.profile.as_deref()),
            (Codec::H264, Some(10), Some("High 10"))
        );
        let h = v.hdr.as_ref().unwrap();
        assert_eq!(h.format, HdrFormat::Hdr10, "{name}");
        assert_eq!(
            h.cll,
            Some(ContentLightLevel {
                max_cll: 1000,
                max_fall: 400
            }),
            "{name}"
        );
        assert_eq!(
            h.mastering.map(|m| m.primaries[2]),
            Some([34000, 16000]),
            "{name}"
        );
        assert!(r.reads <= 2, "{name}: {} reads", r.reads);
    }
}

#[test]
fn hlg_classified_without_mastering() {
    let p = fixtures().join("hevc_hlg.mkv");
    if !p.exists() {
        return;
    }
    let r = probe_file(&p).unwrap();
    let h = r.main_video().unwrap().hdr.as_ref().unwrap();
    assert_eq!((h.format, h.mastering), (HdrFormat::Hlg, None));
}

#[test]
fn fragmented_mp4_duration_from_last_moof() {
    let p = fixtures().join("fragmented.mp4");
    if !p.exists() {
        return;
    }
    let r = probe_file(&p).unwrap();
    assert_eq!(r.duration_ms, Some(4021));
    assert!(!r.has_index, "sample tables are empty in a fragmented file");
    assert!(r.reads <= 3);
}

#[test]
fn quicktime_mov_and_mp4_extras() {
    let dir = fixtures();
    let dir = dir.as_path();
    if !dir.exists() {
        return;
    }
    let mov = probe_file(&dir.join("quicktime_ac3.mov")).unwrap();
    assert_eq!(mov.container, Container::Mov);
    assert_eq!(mov.faststart, Some(false));
    assert_eq!(mov.title.as_deref(), Some("QuickTime Title"));
    assert_eq!(
        (mov.tracks[1].codec, mov.tracks[1].channels),
        (Codec::Ac3, Some(6))
    );

    let ch = probe_file(&dir.join("mp4_eac3_chapters.mp4")).unwrap();
    assert_eq!(ch.title.as_deref(), Some("Chaptered"));
    let titles: Vec<_> = ch
        .chapters
        .iter()
        .map(|c| c.title.clone().unwrap_or_default())
        .collect();
    assert_eq!(titles, ["One", "Two", "Three"]);
    assert_eq!(ch.chapters[2].end_ms, Some(3000));
    assert_eq!(
        (ch.tracks[1].codec, ch.tracks[1].channels),
        (Codec::Eac3, Some(6))
    );

    // Extensions lie: an MP4 named .mkv is still an MP4.
    let lie = probe_file(&dir.join("mp4_named.mkv")).unwrap();
    assert_eq!(lie.container, Container::Mp4);
}

#[test]
fn ts_variants() {
    let dir = fixtures();
    let dir = dir.as_path();
    if !dir.exists() {
        return;
    }
    let ts = probe_file(&dir.join("hevc_mp2_eac3.ts")).unwrap();
    let k: Vec<_> = ts.tracks.iter().map(|t| (t.codec, t.language.clone())).collect();
    assert_eq!(
        k,
        vec![
            (Codec::Hevc, None),
            (Codec::Mp2, Some("deu".into())),
            (Codec::Eac3, Some("spa".into()))
        ]
    );
    assert_eq!(ts.tracks[0].width, Some(1280));
    let m2ts = probe_file(&dir.join("h264_lpcm.m2ts")).unwrap();
    assert_eq!(m2ts.container, Container::M2ts);
    let a = &m2ts.tracks[1];
    assert_eq!(
        (a.codec, a.channels, a.sample_rate),
        (Codec::Pcm, Some(6), Some(48000))
    );
}

#[test]
fn avi_variants() {
    let dir = fixtures();
    let dir = dir.as_path();
    if !dir.exists() {
        return;
    }
    let a = probe_file(&dir.join("h264_ac3.avi")).unwrap();
    assert_eq!(
        (a.tracks[0].codec, a.tracks[1].codec, a.tracks[1].channels),
        (Codec::H264, Codec::Ac3, Some(6))
    );
    let m = probe_file(&dir.join("mjpeg_pcm.avi")).unwrap();
    assert_eq!(
        (m.tracks[0].codec, m.tracks[1].codec, m.tracks[1].bit_depth),
        (Codec::Mjpeg, Codec::Pcm, Some(16))
    );
}

#[test]
fn unknown_containers_fall_back_to_ffprobe() {
    let dir = fixtures();
    let dir = dir.as_path();
    if !dir.exists() {
        return;
    }
    for name in ["flv_h264.flv", "mpeg_ps.mpg"] {
        let p = dir.join(name);
        assert!(
            matches!(probe_file(&p), Err(ProbeError::UnknownFormat { .. })),
            "{name}"
        );
        let o = ProbeOptions {
            ffprobe_fallback: true,
            ..ProbeOptions::default()
        };
        match probe_file_with(&p, &o) {
            Ok(r) => {
                assert_eq!(r.container, Container::Unknown);
                assert!(r.tracks.iter().any(|t| t.kind == TrackKind::Video), "{name}");
                assert!(r.warnings.iter().any(|w| w.contains("ffprobe")));
            }
            Err(ProbeError::Ffprobe(e)) => eprintln!("ffprobe unavailable: {e}"),
            Err(e) => panic!("{name}: {e}"),
        }
    }
}

/// A 2-hour MP4 has a 4.4 MB `moov`, almost all sample tables: it must be
/// parsed piecewise (≈ one read per track), never read whole.
#[test]
fn two_hour_mp4_piecewise_moov() {
    for (name, faststart, max_reads) in [
        ("long_2h_faststart.mp4", true, 3),
        ("long_2h_moov_end.mp4", false, 4),
    ] {
        let p = fixtures().join(name);
        if !p.exists() {
            return;
        }
        let r = probe_file(&p).unwrap();
        assert_eq!(r.faststart, Some(faststart), "{name}");
        assert_eq!(r.duration_ms, Some(7_200_000), "{name}");
        assert_eq!(r.tracks.len(), 2, "{name}");
        assert!(r.has_index);
        assert!(r.reads <= max_reads, "{name}: {} reads", r.reads);
        assert!(r.bytes_read < 512 * 1024, "{name}: {} bytes", r.bytes_read);
    }
}

#[test]
fn mkv_font_attachments_without_reading_data() {
    let p = fixtures().join("mkv_fonts.mkv");
    if !p.exists() {
        return;
    }
    let r = probe_file(&p).unwrap();
    let names: Vec<_> = r
        .attachments
        .iter()
        .map(|a| (a.name.as_str(), a.mime.as_str()))
        .collect();
    assert_eq!(names, [("Arial.ttf", "font/ttf"), ("Georgia.ttf", "font/ttf")]);
    let sizes: Vec<u64> = ["Arial.ttf", "Georgia.ttf"]
        .iter()
        .map(|f| {
            std::fs::metadata(Path::new("/System/Library/Fonts/Supplemental").join(f)).map_or(0, |m| m.len())
        })
        .collect();
    assert_eq!(r.attachments.iter().map(|a| a.size).collect::<Vec<_>>(), sizes);
    assert!(
        r.bytes_read < 512 * 1024,
        "font data must not be read: {} bytes",
        r.bytes_read
    );
    assert_eq!(r.tracks[1].codec, Codec::Ass);
}
