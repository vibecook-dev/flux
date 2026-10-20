//! Print the ProbeResult of each path as JSON: `cargo run --example dump -- FILE…`
//! Set `SEI=always` to force the keyframe SEI pass.
use flux_probe::{probe_file_with, FrameSeiMode, ProbeOptions};

fn main() {
    let mut opts = ProbeOptions::default();
    if std::env::var("SEI").as_deref() == Ok("always") {
        opts.frame_sei = FrameSeiMode::Always;
    }
    for p in std::env::args().skip(1) {
        match probe_file_with(std::path::Path::new(&p), &opts) {
            Ok(r) => println!("{p}\n{}", serde_json::to_string_pretty(&r).unwrap_or_default()),
            Err(e) => println!("{p}\nERROR: {e}"),
        }
    }
}
