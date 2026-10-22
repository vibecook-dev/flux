//! A file flux-probe finds malformed goes to the ffprobe fallback, as an
//! unknown container does, and RealMedia `.rm` and Flash `.f4v` files are
//! walked. Skips, with the reason, without ffmpeg or ffprobe.

mod support;

use flux_catalog::FileState;
use flux_core::LibraryKind;
use flux_test_media::moov::with_moov;
use flux_test_media::{clip, Clip};
use support::*;

#[test]
fn malformed_mp4s_go_to_ffprobe_and_rm_f4v_files_are_media() {
    let Some(src) = clip(Clip::H264AacMp4) else { return };
    let have_ffprobe = std::process::Command::new(flux_probe::ffprobe::ffprobe_bin())
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !have_ffprobe {
        eprintln!("skipping: no ffprobe");
        return;
    }
    let scratch = Scratch::new("mp4-fallback");
    let root = scratch.path("Movies");
    let bytes = std::fs::read(&src).unwrap();
    // A `tkhd` padded past what flux-probe reads: malformed to it, while
    // ffprobe skips the rest of a box.
    let padded = with_moov(&bytes, |m| {
        let tkhd = m.traks_mut(b"vide").remove(0).find_mut(b"tkhd").unwrap();
        tkhd.body.extend(vec![0; 8192]);
    });
    for (rel, data) in [
        ("Padded Header (2001)/Padded Header (2001).mp4", padded),
        ("Flash Video (2002)/Flash Video (2002).f4v", bytes),
        // Not real RealMedia: ffprobe fails it, but it is walked and kept.
        (
            "Real Media (2003)/Real Media (2003).rm",
            b"not a stream".repeat(100),
        ),
    ] {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }
    let cat = open_catalog(&scratch.dir);
    let (lib, root_id) = add_library(&cat, "Movies", LibraryKind::Movies, &root);
    let r = scan(&scanner(&cat), lib);
    assert_eq!(r.media_files, 3, "{r:?}");
    // The malformed MP4 was a probe failure; ffprobe reads it now.
    assert_eq!((r.ffprobe_fallbacks, r.probe_failures), (1, 1), "{r:?}");
    let snap = snapshot(&cat, root_id);
    let state = |ext: &str| {
        snap.iter()
            .find(|(k, _)| k.ends_with(ext))
            .map(|(_, r)| r.state)
            .unwrap_or_else(|| panic!("no .{ext} file in {:?}", snap.keys()))
    };
    assert_eq!(state(".mp4"), FileState::Present);
    assert_eq!(state(".f4v"), FileState::Present);
    assert_eq!(state(".rm"), FileState::ProbeFailed);
}
