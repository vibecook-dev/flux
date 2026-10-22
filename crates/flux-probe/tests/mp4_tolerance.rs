//! MP4 shapes from the wild (made from a synthetic clip with
//! `flux_test_media::moov`) that flux-probe reads as ffprobe does: junk
//! after the last whole box of a container is ignored, and a file it still
//! can't read is left to ffprobe. Each test skips, with the reason, without
//! ffmpeg (or ffprobe where it needs one).

use flux_core::{Codec, Container};
use flux_probe::{probe_file, probe_file_with, ProbeError, ProbeOptions};
use flux_test_media::moov::{with_moov, Bx};
use flux_test_media::{clip, Clip};

/// As found in a 1.4 GB file: a 32-bit word read as a box size far past
/// its parent, then ~440 KB of sample-table-like data.
fn junk() -> Bx {
    let mut j = vec![0x3c, 0xc7, 0x09, 0xf3, 0x03, 0x80, 0, 0];
    j.extend((0..4096u32).map(|i| (i * 31 % 251) as u8));
    Bx::raw(j)
}

fn have_ffprobe() -> bool {
    let ok = std::process::Command::new(flux_probe::ffprobe::ffprobe_bin())
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("skipping: no ffprobe");
    }
    ok
}

/// A `moov` (and a `trak`) whose last child overruns it failed the whole
/// file ("box … overruns its parent"), though its tracks were intact and
/// ffprobe read it. The junk is ignored with a warning; with nothing but
/// junk where the tracks were, the file is malformed (and goes to ffprobe).
#[test]
fn junk_after_the_last_box_of_moov_is_ignored() {
    let Some(src) = clip(Clip::H264AacMp4) else { return };
    let dir = flux_test_media::scratch("probe-moov-junk");
    let clean = probe_file(&src).unwrap();
    let bytes = std::fs::read(&src).unwrap();
    let path = dir.join("junk.mp4");
    let file = with_moov(&bytes, |m| {
        for t in m.traks_mut(b"vide") {
            t.kids.as_mut().unwrap().push(junk());
        }
        m.kids.as_mut().unwrap().push(junk());
    });
    std::fs::write(&path, file).unwrap();
    let r = probe_file(&path).unwrap();
    assert_eq!(r.tracks, clean.tracks);
    assert_eq!(
        (r.duration_ms, r.title.clone()),
        (clean.duration_ms, clean.title.clone())
    );
    assert!(
        r.warnings.iter().any(|w| w.contains("2 boxes overran")),
        "{:?}",
        r.warnings
    );

    let path = dir.join("no-tracks.mp4");
    let file = with_moov(&bytes, |m| {
        let kids = m.kids.as_mut().unwrap();
        kids.retain(|k| &k.kind == b"mvhd");
        kids.push(junk());
    });
    std::fs::write(&path, file).unwrap();
    let e = probe_file(&path).unwrap_err();
    assert!(
        matches!(e, ProbeError::Malformed { .. }) && e.wants_ffprobe(),
        "{e}"
    );
}

/// A file flux-probe finds malformed goes to ffprobe when the fallback is
/// on, as an unknown container does (only `UnknownFormat` did). Here a
/// `tkhd` padded past what flux-probe reads; ffmpeg skips a box's rest.
#[test]
fn malformed_files_go_to_ffprobe() {
    let Some(src) = clip(Clip::H264AacMp4) else { return };
    if !have_ffprobe() {
        return;
    }
    let dir = flux_test_media::scratch("probe-malformed-ffprobe");
    let bytes = std::fs::read(&src).unwrap();
    let path = dir.join("padded-tkhd.mp4");
    let file = with_moov(&bytes, |m| {
        let tkhd = m.traks_mut(b"vide").remove(0).find_mut(b"tkhd").unwrap();
        tkhd.body.extend(vec![0; 8192]);
    });
    std::fs::write(&path, file).unwrap();
    let e = probe_file(&path).unwrap_err();
    assert!(
        matches!(e, ProbeError::Malformed { .. }) && e.wants_ffprobe(),
        "{e}"
    );
    let opts = ProbeOptions {
        ffprobe_fallback: true,
        ..Default::default()
    };
    let r = probe_file_with(&path, &opts).unwrap();
    assert_eq!(r.main_video().map(|v| v.codec), Some(Codec::H264), "{r:?}");
    // Not every failure goes there: an empty file is no media at all.
    assert!(!ProbeError::Empty.wants_ffprobe());
}

/// Flash's `.f4v` is an MP4: it is media, read in-process.
#[test]
fn f4v_is_an_mp4() {
    let Some(src) = clip(Clip::H264AacMp4) else { return };
    let path = flux_test_media::scratch("probe-f4v").join("clip.f4v");
    std::fs::copy(&src, &path).unwrap();
    assert!(flux_probe::is_media_path(&path));
    let r = probe_file(&path).unwrap();
    assert_eq!(r.container, Container::Mp4);
    assert_eq!(r.tracks, probe_file(&src).unwrap().tracks);
}
