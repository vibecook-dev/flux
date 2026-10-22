//! Fallback parser: `hunch` 2.0 (MIT), a Rust port of Python's guessit.
//!
//! Only consulted when the fast path is unsure (confidence below
//! [`crate::resolve::FALLBACK_THRESHOLD`]) because it costs ~230–370 µs per
//! name versus single-digit µs for the fast path (research/02 §6.2). Its
//! output goes through the post-rules before it can override anything.

use std::sync::LazyLock;

use flux_core::{ExtraKind, GuessKind, LibraryKind, NameGuess};
use hunch::{Confidence, MediaType, Property};

use crate::resolve::{
    episode_numbers, explicit_episode, numbered_series, split_ext, Context, FALLBACK_THRESHOLD,
};
use crate::rules::{self, Origin};
use crate::stem::title_looks_like_tags;
use crate::stem::{MAX_YEAR, MIN_YEAR};
use crate::text::norm_key;

/// Building a pipeline compiles hunch's rule set; do it once.
static PIPELINE: LazyLock<hunch::Pipeline> = LazyLock::new(hunch::Pipeline::new);

/// hunch runs its first pass on every sibling, so cost grows linearly with
/// them; a handful is enough for its cross-file title invariance.
const MAX_SIBLINGS: usize = 6;

/// A fallback result plus what the merge needs to judge it.
pub(crate) struct HunchGuess {
    pub guess: NameGuess,
    /// hunch's `alternative_title` (it splits "Title - Subtitle" in two).
    alt_title: Option<String>,
    /// hunch read part of the name as a second container
    /// ("…The.Finest.Ass.Frolicking.mp4" → `ass` subtitles): its title is
    /// truncated and must not replace ours.
    artifact: bool,
}

/// Runs hunch on the path (with folder context and a few siblings) and maps
/// the result to a [`NameGuess`]. No post-rules are applied here.
pub(crate) fn guess(ctx: &Context, siblings: &[&str]) -> HunchGuess {
    let own = ctx.path.rsplit('/').next().unwrap_or("");
    let sibs: Vec<String> = siblings
        .iter()
        .filter(|s| **s != own && split_ext(s).1.is_some_and(crate::is_video_ext))
        .take(MAX_SIBLINGS)
        .map(|s| {
            if ctx.parent.is_empty() {
                (*s).to_string()
            } else {
                format!("{}/{s}", ctx.parent)
            }
        })
        .collect();
    let r = PIPELINE.run_with_context(&ctx.path, &sibs);

    let mut g = NameGuess::unknown();
    g.title = r
        .title()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    g.year = r
        .year()
        .and_then(|y| u16::try_from(y).ok())
        .filter(|y| (MIN_YEAR..=MAX_YEAR).contains(y));
    g.season = r.season().and_then(|s| u16::try_from(s).ok());
    g.episodes = r
        .all(Property::Episode)
        .iter()
        .filter_map(|e| e.parse().ok())
        .collect();
    g.absolute_episode = r.first(Property::AbsoluteEpisode).and_then(|e| e.parse().ok());
    g.air_date = r.date().map(str::to_string);
    g.episode_title = r.episode_title().map(str::to_string);
    g.edition = r.edition().map(str::to_string);
    g.part = r
        .part()
        .or_else(|| r.first(Property::Cd).and_then(|c| c.parse().ok()))
        .or_else(|| r.disc())
        .and_then(|p| u8::try_from(p).ok());
    g.resolution = r.screen_size().map(str::to_string);
    g.source = r.source().map(str::to_string);
    g.video_codec = r.video_codec().map(str::to_string);
    g.release_group = r.release_group().map(str::to_string);

    let has_episode = !g.episodes.is_empty() || g.absolute_episode.is_some() || g.air_date.is_some();
    // In a movie library a bare number ("Toy.Story.3") is not an episode
    // unless the name has a season or an air date.
    let episode_ok = ctx.lib != LibraryKind::Movies || g.season.is_some() || g.air_date.is_some();
    g.kind = match r.media_type() {
        Some(MediaType::Extra) => GuessKind::Extra,
        _ if has_episode && episode_ok => GuessKind::Episode,
        _ if g.title.is_some() && ctx.lib != LibraryKind::Shows => GuessKind::Movie,
        _ => GuessKind::Unknown,
    };
    if g.kind == GuessKind::Extra {
        g.extra = Some(ExtraKind::Other);
    }
    let (conf, label) = match r.confidence() {
        Confidence::High => (0.6, "high"),
        Confidence::Medium => (0.45, "medium"),
        _ => (0.3, "low"),
    };
    g.confidence = if g.title.is_some() { conf } else { 0.1 };
    g.evidence
        .push(format!("fallback: hunch (guessit port), {label} confidence"));
    HunchGuess {
        guess: g,
        alt_title: r.first(Property::AlternativeTitle).map(str::to_string),
        artifact: r.all(Property::Container).len() > 1,
    }
}

/// Merges a fallback guess into the fast-path guess. The fast path keeps
/// anything it was confident about; the fallback fills gaps and replaces
/// weak results it can explain better. `siblings` are the other file names
/// in the folder.
pub(crate) fn merge(fast: &mut NameGuess, hg: HunchGuess, ctx: &Context, siblings: &[&str]) {
    let HunchGuess {
        guess: mut h,
        alt_title,
        artifact,
    } = hg;
    rules::apply(&mut h, ctx, Origin::Fallback);
    if matches!(fast.kind, GuessKind::Sample | GuessKind::Extra) {
        fast.evidence
            .push("fallback consulted; kept the extra/sample classification".into());
        return;
    }
    if !mixed_episode_ok(ctx, &h, siblings) {
        fast.evidence.push(
            "fallback read an episode with no show around it (mixed library); kept the fast path".into(),
        );
        return;
    }
    let fast_key = fast.title.as_deref().map(norm_key).filter(|k| !k.is_empty());
    let h_key = h.title.as_deref().map(norm_key).filter(|k| !k.is_empty());
    let weak = fast.confidence < FALLBACK_THRESHOLD || fast.kind == GuessKind::Unknown;
    // hunch split "Mission Impossible - Rogue Nation" into title and
    // alternative title; our full title matches providers better.
    let only_alt_split = match (&fast_key, &h_key, &alt_title) {
        (Some(a), Some(b), Some(alt)) => *a == format!("{b}{}", norm_key(alt)),
        _ => false,
    };
    // "[Some Studio]" or "中英字幕" as a title: the fast path read the wrong
    // part of the name; a real title from the fallback wins.
    let fast_tags = fast.title.as_deref().is_some_and(title_looks_like_tags);
    let h_tags = h.title.as_deref().is_none_or(title_looks_like_tags);
    if fast_tags && !h_tags && fast_key != h_key && !artifact && movie_lib_ok(ctx, &h) {
        take_all(fast, h, "fast-path title looks like tags; took the fallback's");
        return;
    }
    match (&fast_key, &h_key) {
        (None, Some(_)) => {
            take_all(fast, h, "fallback supplied the title");
        }
        (Some(a), Some(b)) if a == b => {
            // Two parsers agreeing on an ambiguous name is weak evidence:
            // a bare title stays below the AI-matching threshold.
            fast.evidence.push("fallback agrees on the title".into());
            fast.confidence = (fast.confidence + 0.05).min(0.9);
            fill_gaps(fast, &h);
        }
        // The fast path was unsure and hunch's title is contained in ours: it
        // stripped words we did not recognise (unknown tags, `-f17-` film
        // numbering, alternative titles).
        (Some(a), Some(b))
            if weak && a.contains(b.as_str()) && !only_alt_split && !artifact && movie_lib_ok(ctx, &h) =>
        {
            take_all(fast, h, "fallback found structure the fast path missed");
        }
        (_, Some(_)) => {
            let t = h.title.as_deref().unwrap_or_default();
            fast.evidence
                .push(format!("fallback disagrees ('{t}'); kept the fast path"));
            if weak && fast.kind == GuessKind::Unknown && h.kind != GuessKind::Unknown {
                take_all(fast, h, "fallback classified what the fast path could not");
            }
        }
        (_, None) => fast.evidence.push("fallback found no title".into()),
    }
}

/// In a mixed library with no TV or season folder around the file, an
/// episode only the fallback sees needs a stronger signal than its reading
/// of the name: an explicit marker (`S01E02`, `1x02`, `第2集`), or another
/// video file in the folder of one numbered series with it (`Clip 01`
/// beside `Clip 02`). A single clip is never a show of one episode, and a
/// date the fast path did not read (`VID_20190501_123456789`, a camera's
/// timestamp) is never an air date, so a year never becomes its season.
fn mixed_episode_ok(ctx: &Context, h: &NameGuess, siblings: &[&str]) -> bool {
    if ctx.lib != LibraryKind::Mixed || ctx.tv || h.kind != GuessKind::Episode {
        return true;
    }
    if h.air_date.is_some() {
        return false;
    }
    explicit_episode(&ctx.stem)
        || numbered_series(
            &ctx.stem,
            siblings,
            &episode_numbers(&h.episodes, h.absolute_episode, None),
            h.title.as_deref(),
        )
}

/// In a movie library, never let the fallback turn a movie into an episode
/// without a season or air date.
fn movie_lib_ok(ctx: &Context, h: &NameGuess) -> bool {
    ctx.lib != LibraryKind::Movies
        || h.kind != GuessKind::Episode
        || h.season.is_some()
        || h.air_date.is_some()
}

fn take_all(fast: &mut NameGuess, h: NameGuess, why: &str) {
    let ids = std::mem::take(&mut fast.ids);
    let mut evidence = std::mem::take(&mut fast.evidence);
    evidence.push(why.to_string());
    evidence.extend(h.evidence.iter().cloned());
    let edition = fast.edition.take();
    *fast = h;
    fast.ids = ids;
    fast.evidence = evidence;
    if fast.edition.is_none() {
        fast.edition = edition;
    }
    if !fast.ids.is_empty() {
        fast.confidence = fast.confidence.max(0.95);
    }
}

fn fill_gaps(fast: &mut NameGuess, h: &NameGuess) {
    if fast.year.is_none() && h.year.is_some() {
        fast.year = h.year;
        fast.evidence.push("year from fallback".into());
    }
    if fast.kind == GuessKind::Unknown && h.kind != GuessKind::Unknown {
        fast.kind = h.kind;
        fast.season = fast.season.or(h.season);
        if fast.episodes.is_empty() {
            fast.episodes = h.episodes.clone();
        }
        fast.absolute_episode = fast.absolute_episode.or(h.absolute_episode);
        fast.air_date = fast.air_date.clone().or_else(|| h.air_date.clone());
    }
    fast.edition = fast.edition.clone().or_else(|| h.edition.clone());
    fast.resolution = fast.resolution.clone().or_else(|| h.resolution.clone());
    fast.source = fast.source.clone().or_else(|| h.source.clone());
    fast.video_codec = fast.video_codec.clone().or_else(|| h.video_codec.clone());
    fast.release_group = fast.release_group.clone().or_else(|| h.release_group.clone());
}
