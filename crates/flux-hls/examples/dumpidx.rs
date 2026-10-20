//! Prints a file's playback index (debug aid).
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let idx = flux_hls::PlaybackIndex::build(&a[1]).expect("index");
    for t in &idx.tracks {
        println!("#{} {:?} {:?} tag={} ts={} fd={:?} rate={:?} first={:?} reorder={} codec={:?} color={:?} hdr={:?} prof={:?} lvl={:?} bd={:?} open_gop={}", t.index, t.kind, t.codec, t.codec_tag, t.timescale, t.frame_duration, t.frame_rate, t.first_pts_ns, t.reorder_depth, t.codec_string, t.color, t.hdr, t.profile, t.level, t.bit_depth, t.open_gop);
    }
    if let flux_hls::index::Layout::Mp4(l) = &idx.layout {
        for t in &l.tracks {
            println!(
                "stts {:?} ctts[..4] {:?} edit_shift {}",
                &t.stts[..t.stts.len().min(4)],
                &t.ctts[..t.ctts.len().min(4)],
                t.edit_shift
            );
        }
    }
    println!(
        "D={} kf={} dur={}",
        idx.presentation_offset_ns,
        idx.keyframes.len(),
        idx.duration_ns
    );
}
