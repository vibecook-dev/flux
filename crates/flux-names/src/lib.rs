//! Movie/TV filename and folder parser for flux.
//!
//! [`parse`] turns a library-relative path (plus the names of the files next
//! to it and the library kind) into a [`NameGuess`]: kind, title, year,
//! season/episodes, air date, edition, part, extras, provider ids, a
//! confidence and the evidence for every decision.
//!
//! It is layered (research/02 §6.3):
//!
//! 1. **Explicit ids** anywhere in the path: `[tmdbid-603]`, `{tmdb-603}`,
//!    `[imdbid-tt0133093]`, `{imdb-tt…}`, `[tvdbid-…]`, `{tvdb-…}`.
//! 2. **Fast path** (µs): an anchored tokenizer grammar for release names
//!    (`stem`) plus folder-layout rules for Plex/Jellyfin/*arr libraries
//!    (`Title (Year)/…`, `Show/Season NN/…`, specials, extras folders and
//!    suffixes, samples, multi-part, multi-version, anime absolute numbering,
//!    date-based shows, disc structures).
//! 3. **Fallback**: `hunch` (guessit port) with sibling context, only when the
//!    fast path's confidence is below 0.5 (feature `hunch`, on by default).
//! 4. **Post-rules** for known failure classes (year-like titles, dates read
//!    as episodes, seasons > 50, "Part Two" in titles, "The Office US").
//!
//! Confidence below ~0.5 means "queue for AI-assisted matching".
//!
//! ```
//! use std::path::Path;
//! use flux_names::{parse, GuessKind, LibraryKind};
//!
//! let g = parse(Path::new("Blade Runner 2049 (2017)/Blade Runner 2049 (2017).mkv"), &[], LibraryKind::Movies);
//! assert_eq!(g.kind, GuessKind::Movie);
//! assert_eq!(g.title.as_deref(), Some("Blade Runner 2049"));
//! assert_eq!(g.year, Some(2017));
//!
//! let g = parse(Path::new("Severance (2022)/Season 02/Severance (2022) - S02E10 - Cold Harbor.mkv"), &[], LibraryKind::Shows);
//! assert_eq!((g.title.as_deref(), g.season, g.episodes.as_slice()), (Some("Severance"), Some(2), &[10][..]));
//! ```

use std::path::Path;

#[cfg(feature = "hunch")]
mod fallback;
mod folders;
mod ids;
mod resolve;
mod rules;
mod stem;
mod text;
mod tokens;
mod vocab;

pub mod eval;
pub mod hard_cases;

pub use flux_core::names::{ExtraKind, GuessKind, LibraryKind, NameGuess, ProviderIds, PARSER_VERSION};
pub use text::norm_key;

/// When to consult the fallback parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fallback {
    /// Fast path and post-rules only.
    Never,
    /// Only when the fast path is unsure (confidence < 0.5). The default.
    #[default]
    Auto,
    /// Always run it and merge (for comparisons and benchmarks).
    Always,
}

/// Options for [`parse_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ParseOptions {
    pub fallback: Fallback,
}

/// `true` when this build includes the `hunch` fallback.
pub const FALLBACK_AVAILABLE: bool = cfg!(feature = "hunch");

/// Parses a library-relative path.
///
/// - `rel_path`: path relative to the library root, e.g.
///   `Severance (2022)/Season 02/Severance (2022) - S02E10.mkv`.
/// - `siblings`: file names (not paths) in the same directory; used for
///   multi-part stacking evidence and the fallback's title invariance. May be
///   empty.
/// - `kind`: the library kind, a strong prior (`1917.mkv` in a movie library
///   is a title; `Show 1x03.mkv` is an episode anywhere).
///
/// Never panics and never fails: unparseable names come back as
/// [`GuessKind::Unknown`] with low confidence and evidence saying why.
pub fn parse(rel_path: &Path, siblings: &[&str], kind: LibraryKind) -> NameGuess {
    parse_with(rel_path, siblings, kind, ParseOptions::default())
}

/// [`parse`] with explicit options.
pub fn parse_with(rel_path: &Path, siblings: &[&str], kind: LibraryKind, opts: ParseOptions) -> NameGuess {
    let resolve::Resolved { mut guess, ctx } = resolve::resolve(rel_path, siblings, kind);
    let needs = guess.confidence < resolve::FALLBACK_THRESHOLD || guess.kind == GuessKind::Unknown;
    let run = match opts.fallback {
        Fallback::Never => false,
        Fallback::Auto => needs,
        Fallback::Always => true,
    };
    if run && !ctx.path.is_empty() {
        #[cfg(feature = "hunch")]
        {
            let h = fallback::guess(&ctx, siblings);
            fallback::merge(&mut guess, h, &ctx);
        }
        #[cfg(not(feature = "hunch"))]
        guess
            .evidence
            .push("fallback unavailable (built without the `hunch` feature)".into());
    }
    rules::apply(&mut guess, &ctx, rules::Origin::Fast);
    guess
}

/// Parses with the fallback parser alone (no fast path), optionally followed
/// by the post-rules. For baselines and debugging; use [`parse`] in flux.
#[cfg(feature = "hunch")]
pub fn parse_fallback_only(
    rel_path: &Path,
    siblings: &[&str],
    kind: LibraryKind,
    post_rules: bool,
) -> NameGuess {
    let resolve::Resolved { ctx, .. } = resolve::resolve(rel_path, siblings, kind);
    let mut g = fallback::guess(&ctx, siblings).guess;
    if post_rules {
        rules::apply(&mut g, &ctx, rules::Origin::Fallback);
    }
    g
}

/// `true` when `ext` (without the dot, any case) is a video extension: the
/// one list in [`flux_core::VIDEO_EXTENSIONS`] (shared with flux-probe).
/// Disc images (`iso`) and `.strm` URL files are not video files.
pub fn is_video_ext(ext: &str) -> bool {
    flux_core::is_video_extension(ext)
}

/// `true` when the file name has a video extension.
pub fn is_video_file(name: &str) -> bool {
    resolve::split_ext(name).1.is_some_and(is_video_ext)
}

/// The key under which a video file stacks with its siblings as one part of
/// a multi-part movie (`示例A.mp4` + `示例B.mp4`, `Movie part1` + `Movie
/// part2`), or `None`. A part only stacks once a sibling with the same key
/// exists, so when a new part arrives the scanner re-parses the parts it
/// already has under that key (the first part copied in was parsed alone).
pub fn stack_key(file_name: &str) -> Option<String> {
    let (stem, ext) = resolve::split_ext(file_name);
    if !ext.is_some_and(is_video_ext) {
        return None;
    }
    resolve::stack_key(stem)
}

/// `true` for paths a scanner should skip before parsing: hidden files and
/// folders, NAS metadata folders (`@eaDir`, `#recycle`, `.AppleDouble`),
/// OS litter (`Thumbs.db`, `.DS_Store`, `desktop.ini`) and partial downloads
/// (`.part`, `.partial~`, `.!qB`, `.crdownload`).
pub fn is_ignored(rel_path: &Path) -> bool {
    rel_path.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        let lower = s.to_ascii_lowercase();
        s.starts_with('.')
            || s.starts_with('@')
            || matches!(
                lower.as_str(),
                "#recycle"
                    | "$recycle.bin"
                    | "lost+found"
                    | "system volume information"
                    | "thumbs.db"
                    | "desktop.ini"
            )
            || lower.ends_with('~')
            || [".part", ".partial", ".!qb", ".crdownload", ".tmp"]
                .iter()
                .any(|x| lower.ends_with(x))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignored_paths() {
        assert!(is_ignored(Path::new("Movies/@eaDir/SYNOINDEX_MEDIA_INFO")));
        assert!(is_ignored(Path::new("Movies/.hidden.mkv")));
        assert!(is_ignored(Path::new("Movies/incomplete.download.mkv.partial~")));
        assert!(is_ignored(Path::new("TV/Thumbs.db")));
        assert!(!is_ignored(Path::new("Movies/Alien (1979)/Alien (1979).mkv")));
        assert!(is_video_file("Alien (1979).MKV"));
        assert!(!is_video_file("readme.txt"));
        // Probe-readable captures are video; disc images and URL stubs are not.
        assert!(is_video_file("Match.2024.m2t") && is_video_file("Recording.TP"));
        assert!(!is_video_file("Alien (1979).iso") && !is_video_file("Alien.strm"));
    }

    #[test]
    fn stack_keys() {
        assert_eq!(stack_key("示例片A.mp4"), stack_key("示例片B.mp4"));
        assert!(stack_key("示例片A.mp4").is_some());
        assert_eq!(
            stack_key("Some Movie part1.mkv"),
            stack_key("Some Movie part2.mkv")
        );
        assert_ne!(stack_key("示例片A.mp4"), stack_key("其他片B.mp4"));
        assert_eq!(stack_key("Plan A.mkv"), None);
        assert_eq!(stack_key("Some Movie (2019).mkv"), None);
        assert_eq!(stack_key("示例片A.srt"), None);
    }
}
