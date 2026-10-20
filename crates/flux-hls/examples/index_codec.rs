//! Size and decode time of the playback index as JSON vs the compact binary
//! form (`PlaybackIndex::to_bytes`), for catalog BLOB storage.
//!
//! ```sh
//! cargo run --release -p flux-hls --example index_codec [-- FILE...]
//! ```
//!
//! Without FILEs it measures the long corpus files under `$FLUX_TEST_MEDIA`
//! (skipping when it is not set). Writes
//! `$FLUX_BENCH_RESULTS/flux-hls-index-encoding.json` when that is set.

use std::path::PathBuf;
use std::time::Instant;

use flux_hls::PlaybackIndex;
use serde_json::json;

/// Corpus files (below `$FLUX_TEST_MEDIA`) measured when none are given.
const DEFAULT: &[&str] = &[
    "probe-fixtures/long_2h_faststart.mp4",
    "probe-fixtures/long_2h_moov_end.mp4",
    "features/long_2h_360p.mkv",
    "features/h264_aac_faststart.mp4",
    "features/hevc_4k_60mbps.mkv",
    "playback-research/synth_h264_10min.mkv",
];

/// Median wall time of `f` over `n` runs, in ms.
fn median_ms(n: usize, mut f: impl FnMut()) -> f64 {
    let mut v: Vec<f64> = (0..n)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    v.sort_by(|a, b| a.total_cmp(b));
    v[n / 2]
}

fn main() {
    let mut files: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    if files.is_empty() {
        if flux_test_media::media_root().is_none() {
            return;
        }
        files = DEFAULT
            .iter()
            .map(|rel| flux_test_media::media_dir(rel))
            .filter(|p| p.exists())
            .collect();
    }
    let mut rows = Vec::new();
    for f in &files {
        let idx = match PlaybackIndex::build(f) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("{}: {e}", f.display());
                continue;
            }
        };
        let js = idx.to_json().expect("json");
        let bin = idx.to_bytes().expect("binary");
        let back = PlaybackIndex::from_bytes(&bin).expect("decode");
        assert_eq!(back, idx, "{}: binary round trip differs", f.display());
        let json_ms = median_ms(9, || {
            PlaybackIndex::from_json(&js).expect("json decode");
        });
        let bin_ms = median_ms(9, || {
            PlaybackIndex::from_bytes(&bin).expect("binary decode");
        });
        let enc_ms = median_ms(9, || {
            idx.to_bytes().expect("binary");
        });
        let samples: u64 = match &idx.layout {
            flux_hls::index::Layout::Mp4(l) => l.tracks.iter().map(|t| t.sample_count as u64).sum(),
            _ => 0,
        };
        let name = f
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        println!(
            "{name:40} {:?} keyframes {:6} samples {:7}  json {:9} B {:7.2} ms  binary {:8} B ({:4.1}%) {:6.2} ms decode, {:6.2} ms encode",
            idx.method,
            idx.keyframes.len(),
            samples,
            js.len(),
            json_ms,
            bin.len(),
            bin.len() as f64 * 100.0 / js.len() as f64,
            bin_ms,
            enc_ms
        );
        rows.push(json!({
            "file": name, "method": format!("{:?}", idx.method), "keyframes": idx.keyframes.len(),
            "mp4_samples": samples, "json_bytes": js.len(), "json_decode_ms": json_ms,
            "binary_bytes": bin.len(), "binary_decode_ms": bin_ms, "binary_encode_ms": enc_ms,
            "ratio": bin.len() as f64 / js.len() as f64,
        }));
    }
    let load = std::process::Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let out = json!({
        "component": "flux-hls", "name": "index-encoding", "load_avg": load,
        "note": "shared machine (other projects running); median of 9 decodes, warm page cache",
        "rows": rows,
    });
    let text = serde_json::to_string_pretty(&out).expect("json");
    if flux_test_media::write_result("flux-hls-index-encoding.json", &text).is_none() {
        println!("{text}");
    }
}
