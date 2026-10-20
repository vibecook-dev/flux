//! Post-rules: validators for known failure classes (research/02 §6.2–6.3,
//! research/05 §3.3–3.4). They run on every result, fast path or fallback,
//! and each records evidence when it fires.
//!
//! The fast path avoids these errors by construction; the rules exist mainly
//! to correct the fallback parser (hunch), whose measured failures were:
//! `Blade Runner 2049 (2017)` → S20E49, `Dune.Part.Two` → title "Dune" +
//! part 2, `The.Daily.Show.2024.03.15` → year 2024 + episode 15, and
//! `The.Office.US` → "The Office" (US as country).

use std::sync::LazyLock;

use flux_core::{GuessKind, LibraryKind, NameGuess};
use regex::Regex;

use crate::resolve::Context;
use crate::stem::{MAX_YEAR, MIN_YEAR};
use crate::text::norm_key;

/// Explicit episode markers anywhere in a stem.
static EXPLICIT_EP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[^a-z0-9])s\d{1,3}[\s._-]?e\d{1,4}|(?:^|[^0-9])\d{1,2}x\d{2,3}(?:$|[^0-9])")
        .expect("valid explicit-episode regex")
});

static DATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[\s._\-\[(])((?:19|20)\d{2})[\s._-](\d{2})[\s._-](\d{2})(?:$|[\s._\-\])])")
        .expect("valid date regex")
});

static PART_WORDS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^|[\s._-])(part|pt|vol|volume|chapter)[\s._-]+(\d{1,2}|i{1,3}|iv|v|vi{1,3}|ix|x|one|two|three|four|five|six|seven|eight|nine|ten)(?:$|[\s._\-(\[])",
    )
    .expect("valid part-words regex")
});

static COUNTRY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[\s._(\[-])(US|UK|AU|NZ|CA)[\s._)\]-]+(?:(?:19|20)\d{2}[\s._)\]-]+)?(?i:s\d{1,3}e\d|\d{1,2}x\d)")
        .expect("valid country regex")
});

/// Which parser produced a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    /// The fast path: it avoids hunch's failure classes by construction and
    /// makes deliberate calls (a packed `117` in a TV context *is* S1E17), so
    /// only the general sanity rules run.
    Fast,
    /// The fallback parser: every rule runs.
    Fallback,
}

/// Applies the post-rules to `g`.
pub(crate) fn apply(g: &mut NameGuess, ctx: &Context, origin: Origin) {
    year_sanity(g);
    date_is_air_date(g, ctx);
    if origin == Origin::Fallback {
        title_number_is_not_an_episode(g, ctx);
        part_words_belong_to_title(g, ctx);
        country_belongs_to_title(g, ctx);
    }
    season_cap(g, ctx);
    if g.episodes.len() > 64 {
        g.episodes.truncate(64);
    }
    g.confidence = g.confidence.clamp(0.0, 1.0);
}

fn year_sanity(g: &mut NameGuess) {
    if let Some(y) = g.year {
        if !(MIN_YEAR..=MAX_YEAR).contains(&y) {
            g.evidence
                .push(format!("post-rule: dropped implausible year {y}"));
            g.year = None;
        }
    }
}

/// `Show.2024.03.15` is an air date, never "year 2024, episode 15".
fn date_is_air_date(g: &mut NameGuess, ctx: &Context) {
    if g.air_date.is_some() || EXPLICIT_EP_RE.is_match(&ctx.stem) {
        return;
    }
    let Some(c) = DATE_RE.captures(&ctx.stem) else {
        return;
    };
    let (Ok(y), Ok(m), Ok(d)) = (c[1].parse::<u16>(), c[2].parse::<u16>(), c[3].parse::<u16>()) else {
        return;
    };
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return;
    }
    g.air_date = Some(format!("{y:04}-{m:02}-{d:02}"));
    if g.year == Some(y) {
        g.year = None;
    }
    if g.episodes.iter().all(|e| *e == d || *e == m) {
        g.episodes.clear();
    }
    if g.season == Some(m) || g.season == Some(y) {
        g.season = None;
    }
    g.kind = GuessKind::Episode;
    g.evidence.push(format!(
        "post-rule: date {} is an air date, not year/episode",
        c[0].trim()
    ));
}

/// `Blade Runner 2049 (2017)` must not become S20E49: when no explicit
/// marker exists and the season+episode digits appear as one number in the
/// name, the number belongs to the title.
fn title_number_is_not_an_episode(g: &mut NameGuess, ctx: &Context) {
    let (Some(season), &[ep]) = (g.season, g.episodes.as_slice()) else {
        return;
    };
    if EXPLICIT_EP_RE.is_match(&ctx.stem) || ctx.season_folder.is_some() {
        return;
    }
    let combined = format!("{season}{ep:02}");
    let found = ctx
        .stem
        .split(|c: char| !c.is_ascii_digit())
        .any(|tok| tok == combined);
    if !found {
        return;
    }
    if let Some(t) = &g.title {
        if !norm_key(t).contains(&combined) {
            g.title = Some(format!("{t} {combined}"));
        }
    }
    g.season = None;
    g.episodes.clear();
    if g.kind == GuessKind::Episode && g.absolute_episode.is_none() && g.air_date.is_none() {
        g.kind = if ctx.lib == LibraryKind::Shows {
            GuessKind::Unknown
        } else {
            GuessKind::Movie
        };
    }
    g.evidence.push(format!(
        "post-rule: '{combined}' is part of the title, not S{season}E{ep}"
    ));
}

/// Seasons above 50 are almost always misread numbers unless a season
/// folder says so, or an explicit marker uses a year as the season
/// (`Show.S2013E14`, Sonarr's year-numbered seasons).
fn season_cap(g: &mut NameGuess, ctx: &Context) {
    let Some(season) = g.season else { return };
    if season <= 50 || ctx.season_folder == Some(season) {
        return;
    }
    let lower = ctx.stem.to_ascii_lowercase();
    if (MIN_YEAR..=MAX_YEAR).contains(&season)
        && (lower.contains(&format!("s{season}e")) || lower.contains(&format!("{season}x")))
    {
        if g.year.is_none() {
            g.year = Some(season);
        }
        g.evidence
            .push(format!("post-rule: year-numbered season {season}"));
        return;
    }
    g.evidence.push(format!(
        "post-rule: rejected season {season} (> 50, no season folder)"
    ));
    g.season = None;
    g.episodes.clear();
    if g.kind == GuessKind::Episode && g.absolute_episode.is_none() && g.air_date.is_none() {
        g.kind = GuessKind::Unknown;
        g.confidence = g.confidence.min(0.3);
    }
}

/// "Dune Part Two", "Kill Bill Vol 1": part words before the year are title.
fn part_words_belong_to_title(g: &mut NameGuess, ctx: &Context) {
    let (Some(_), Some(title)) = (g.part, g.title.clone()) else {
        return;
    };
    let Some(c) = PART_WORDS_RE.captures(&ctx.stem) else {
        return;
    };
    let Some(m) = c.get(0) else { return };
    // Only when the part words come right after the title and before the year.
    if let Some(y) = g.year {
        match ctx.stem.find(&y.to_string()) {
            Some(pos) if pos > m.start() => {}
            _ => return,
        }
    }
    if norm_key(&ctx.stem[..m.start()]) != norm_key(&title) {
        return;
    }
    let extra = format!("{} {}", &c[1], &c[2]);
    g.title = Some(format!("{title} {extra}"));
    g.part = None;
    g.evidence.push(format!(
        "post-rule: '{extra}' belongs to the title, not a multi-part marker"
    ));
}

/// "The Office US": a country code right before the episode marker is part
/// of the show name (it distinguishes the US and UK shows).
fn country_belongs_to_title(g: &mut NameGuess, ctx: &Context) {
    if g.kind != GuessKind::Episode {
        return;
    }
    let Some(title) = g.title.clone() else { return };
    let Some(c) = COUNTRY_RE.captures(&ctx.stem) else {
        return;
    };
    let (Some(code), Some(m)) = (c.get(1), c.get(0)) else {
        return;
    };
    let key = norm_key(&title);
    if key.ends_with(&code.as_str().to_lowercase()) || norm_key(&ctx.stem[..m.start()]) != key {
        return;
    }
    g.title = Some(format!("{title} {}", code.as_str()));
    g.evidence.push(format!(
        "post-rule: country code '{}' kept in the show name",
        code.as_str()
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(stem: &str) -> Context {
        Context {
            path: stem.into(),
            parent: String::new(),
            stem: stem.into(),
            season_folder: None,
            lib: LibraryKind::Mixed,
        }
    }

    fn guess(title: &str) -> NameGuess {
        let mut g = NameGuess::unknown();
        g.title = Some(title.into());
        g
    }

    #[test]
    fn blade_runner_2049() {
        let mut g = guess("Blade Runner");
        g.kind = GuessKind::Episode;
        g.year = Some(2017);
        g.season = Some(20);
        g.episodes = vec![49];
        apply(
            &mut g,
            &ctx("Blade Runner 2049 (2017) 2160p UHD BluRay"),
            Origin::Fallback,
        );
        assert_eq!(g.title.as_deref(), Some("Blade Runner 2049"));
        assert_eq!((g.season, g.episodes.len(), g.kind), (None, 0, GuessKind::Movie));
    }

    #[test]
    fn daily_show_date() {
        let mut g = guess("The Daily Show");
        g.kind = GuessKind::Episode;
        g.year = Some(2024);
        g.episodes = vec![15];
        apply(
            &mut g,
            &ctx("The.Daily.Show.2024.03.15.Guest.Name.720p.WEB.h264"),
            Origin::Fallback,
        );
        assert_eq!(g.air_date.as_deref(), Some("2024-03-15"));
        assert_eq!((g.year, g.episodes.len()), (None, 0));
    }

    #[test]
    fn dune_part_two() {
        let mut g = guess("Dune");
        g.kind = GuessKind::Movie;
        g.year = Some(2024);
        g.part = Some(2);
        apply(&mut g, &ctx("Dune.Part.Two.2024.2160p.WEB-DL"), Origin::Fallback);
        assert_eq!((g.title.as_deref(), g.part), (Some("Dune Part Two"), None));
    }

    #[test]
    fn office_us() {
        let mut g = guess("The Office");
        g.kind = GuessKind::Episode;
        g.season = Some(2);
        g.episodes = vec![1, 2];
        apply(
            &mut g,
            &ctx("The.Office.US.S02E01E02.720p.WEB-DL"),
            Origin::Fallback,
        );
        assert_eq!(g.title.as_deref(), Some("The Office US"));
    }

    #[test]
    fn season_above_fifty() {
        let mut g = guess("Show");
        g.kind = GuessKind::Episode;
        g.season = Some(64);
        g.episodes = vec![1];
        apply(&mut g, &ctx("Show.S64E01"), Origin::Fallback);
        assert_eq!((g.season, g.kind), (None, GuessKind::Unknown));
    }
}
