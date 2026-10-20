//! Dev-half disagreement analysis on guessit's fixtures: cases where exactly
//! one of flux-names (fast path) and hunch gets title/year/season/episodes
//! right. `cargo run --release -p flux-names --example compare -- FIXTURE_DIR`

use std::path::Path;

use flux_names::eval::{evaluate_guessit, load_guessit, GuessitCase, Split};
use flux_names::{parse_fallback_only, parse_with, Fallback, LibraryKind, ParseOptions};

fn main() {
    let dir = std::env::args().nth(1).expect("fixture dir");
    for file in ["movies", "episodes", "various"] {
        let cases = load_guessit(&Path::new(&dir).join(format!("{file}.yml"))).expect("fixtures");
        let fast = |c: &GuessitCase| {
            parse_with(
                Path::new(&c.input),
                &[],
                LibraryKind::Mixed,
                ParseOptions {
                    fallback: Fallback::Never,
                },
            )
        };
        let hunch =
            |c: &GuessitCase| parse_fallback_only(Path::new(&c.input), &[], LibraryKind::Mixed, false);
        let f = evaluate_guessit(&cases, Split::Dev, false, fast);
        let h = evaluate_guessit(&cases, Split::Dev, false, hunch);
        let fset: std::collections::HashSet<&str> = f.failures.iter().map(|x| x.input.as_str()).collect();
        let hset: std::collections::HashSet<&str> = h.failures.iter().map(|x| x.input.as_str()).collect();
        println!("== {file}: flux wrong & hunch right");
        for x in f.failures.iter().filter(|x| !hset.contains(x.input.as_str())) {
            let g = fast(&cases.iter().find(|c| c.input == x.input).cloned().expect("case"));
            println!(
                "  `{}`\n     exp {}\n     got {} conf={:.2}",
                x.input, x.expected, x.got, g.confidence
            );
        }
        println!("== {file}: hunch wrong & flux right");
        for x in h.failures.iter().filter(|x| !fset.contains(x.input.as_str())) {
            println!("  `{}`\n     exp {}\n     hunch {}", x.input, x.expected, x.got);
        }
    }
}
