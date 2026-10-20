//! Prints the parse of each argument: `cargo run -p flux-names --example parse -- [--movies|--shows] PATH…`

use std::path::Path;

use flux_names::{parse, LibraryKind};

fn main() {
    let mut kind = LibraryKind::Mixed;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--movies" => kind = LibraryKind::Movies,
            "--shows" => kind = LibraryKind::Shows,
            "--mixed" => kind = LibraryKind::Mixed,
            path => {
                let g = parse(Path::new(path), &[], kind);
                println!("{path}\n  {:?} title={:?} year={:?} S={:?} E={:?} abs={:?} date={:?} ep_title={:?} edition={:?} part={:?} extra={:?}\n  res={:?} src={:?} codec={:?} group={:?} ids={:?}\n  confidence={:.2} evidence={:?}",
                    g.kind, g.title, g.year, g.season, g.episodes, g.absolute_episode, g.air_date, g.episode_title, g.edition, g.part, g.extra,
                    g.resolution, g.source, g.video_codec, g.release_group, g.ids, g.confidence, g.evidence);
            }
        }
    }
}
