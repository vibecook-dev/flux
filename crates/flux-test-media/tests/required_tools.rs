//! With `FLUX_TEST_REQUIRE_FFMPEG` set (CI sets it), ffmpeg, ffprobe, every
//! encoder the synthetic clips need and every clip itself must be there.
//!
//! The tests that use them skip themselves otherwise, which is right on a
//! developer's machine but would let a CI runner with a missing or reduced
//! ffmpeg pass without running a single transcode. Unset, this test does
//! nothing.

use flux_test_media::{clip, ffmpeg, ffprobe, has_encoder, Clip};

fn required() -> bool {
    std::env::var_os("FLUX_TEST_REQUIRE_FFMPEG").is_some_and(|v| !v.is_empty() && v != "0")
}

#[test]
fn media_tools_and_synthetic_clips_are_there_when_required() {
    if !required() {
        return;
    }
    assert!(
        ffmpeg().is_some(),
        "FLUX_TEST_REQUIRE_FFMPEG is set but there is no ffmpeg"
    );
    assert!(
        ffprobe().is_some(),
        "FLUX_TEST_REQUIRE_FFMPEG is set but there is no ffprobe"
    );

    let mut encoders: Vec<&str> = Clip::ALL
        .iter()
        .flat_map(|c| c.encoders().iter().copied())
        .collect();
    encoders.sort_unstable();
    encoders.dedup();
    let missing: Vec<&str> = encoders.into_iter().filter(|e| !has_encoder(e)).collect();
    assert!(
        missing.is_empty(),
        "this ffmpeg has no {missing:?} encoder: the synthetic clips need them"
    );

    let failed: Vec<&str> = Clip::ALL
        .into_iter()
        .filter(|&c| clip(c).is_none())
        .map(Clip::file_name)
        .collect();
    assert!(
        failed.is_empty(),
        "synthetic clips that could not be made: {failed:?}"
    );
}
