//! The stem grammar: parses one name (a filename without its extension, or a
//! folder name) into title, year, episode marker, edition, part and tags.
//!
//! The approach is "anchor first": find the earliest *anchor* (an episode
//! marker such as `S01E02`, `1x03`, `Season 2 Episode 3`, a date, an anime
//! ` - 12`, or the first technical tag such as `1080p`/`x264`/`[`), then pick
//! the year inside the zone before it. Everything before the year (or the
//! anchor) is the title. This is how release names are built
//! (`Title.Year.Tags-GROUP`, `Show.SxxEyy.Episode.Title.Tags`), and it handles
//! the classic failure classes by construction:
//!
//! - **The year is the last plausible year before the anchor**, never the
//!   first token: `1917.2019` → "1917" (2019), `2001.A.Space.Odyssey.1968` →
//!   "2001 A Space Odyssey" (1968), `2012.mkv` → "2012" (no year).
//! - **A bracketed year wins** (`Blade Runner 2049 (2017)`), and numbers above
//!   [`MAX_YEAR`] are never years, so `2049` stays in the title. There is no
//!   rule that turns a bare 4-digit number into `SxxEyy` (hunch's `S20E49`).
//! - **Dates are anchors**, so `Show.2024.03.15` yields an air date, not a
//!   year plus episode 15.
//! - **Multi-part markers only count after the year** (or as `cd1`/`disc1`),
//!   so "Dune Part Two" and "The Godfather Part II" keep their titles.
//! - **Weak tags** (`Web`, `Opus`, `Extended`, languages) only end the title
//!   when another tag follows, so "Charlotte's Web 1973" keeps "Web".
//! - **Bare numbers are episodes only with evidence**: a fansub `[Group]`
//!   (`[Group] Title 14 [..]`) or an episode context (shows library, season
//!   folder) — never in a movie library ("Toy Story 3", "Apollo 13").
//!
//! Chinese release names get the same treatment: `第01集`/`第十二话` are
//! episode markers and `第2季` a season marker, CJK subtitle and fansub tags
//! (`中英字幕`, `某某字幕组`) end the title, names made only of bracket groups
//! (`[中文名][Group][English Name][1080P]`) are read group by group, and a
//! bilingual title (`中文名.English.Name`) keeps its Latin half as the title and
//! its CJK half as the original title.

use std::sync::LazyLock;

use regex::Regex;

use crate::text::{collapse_ws, has_cjk, is_cjk, title_case_if_lower, trim_seps};
use crate::tokens::{tokenize, Tok, TokKind};
use crate::vocab::{self, TagKind};

/// Lowest plausible release year (the first films are from the 1890s).
pub(crate) const MIN_YEAR: u16 = 1890;
/// Highest plausible release year. A constant rather than "now + 1" keeps the
/// parser deterministic (results are cached by `PARSER_VERSION`); bump both
/// together. Numbers above it stay in the title ("Blade Runner 2049").
pub(crate) const MAX_YEAR: u16 = 2035;

/// Which episode marker anchored the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Marker {
    /// `S01E02`, `S01E01-E02`, `S01 E02`.
    SxxEyy,
    /// `1x02`.
    NxNN,
    /// `Season 1 Episode 3`, `Temporada 1 … Cap.102`.
    Words,
    /// `E12`, `Ep 12`, `Episode 12`, `1 of 6` (no season).
    EpisodeOnly,
    /// `Show.102` in an episode context: season 1, episode 2.
    Packed,
    /// `2024.03.15`, `2024-03-15`.
    Date,
    /// Anime absolute numbering: `Title - 123`, `[Group] Title 123`.
    Absolute,
    /// `S01` / `Season 1` alone (season packs, folders).
    SeasonOnly,
    /// `第12集`, `第十二话`, `第3回` (Chinese/Japanese episode numbering).
    CjkEpisode,
}

impl Marker {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Marker::SxxEyy => "SxxEyy",
            Marker::NxNN => "NxNN",
            Marker::Words => "season/episode words",
            Marker::EpisodeOnly => "episode number",
            Marker::Packed => "packed season+episode number",
            Marker::Date => "air date",
            Marker::Absolute => "anime absolute number",
            Marker::SeasonOnly => "season marker",
            Marker::CjkEpisode => "第N集 episode marker",
        }
    }
}

/// Parse options.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct StemOpts {
    /// Episode context (shows library, season folder, season-pack folder):
    /// allows `Title - 05` and trailing bare numbers as episodes.
    pub tv: bool,
    /// Siblings stack with this file (`part1`/`part2`), so a trailing
    /// `partN` is a multi-part marker even without a year.
    pub stacked: bool,
    /// Parsing a folder name: never read a trailing number as an episode
    /// (`Burning Archive 2` is a show title).
    pub folder: bool,
    /// Season from a season folder, to validate packed numbers (`0106`).
    pub season_hint: Option<u16>,
    /// A movie library: a trailing number is never an episode
    /// ("Movies/Fahrenheit.451.HDTV.mkv").
    pub movie_lib: bool,
    /// `第N期` names a season even with no episode after it: a show or
    /// season-pack folder (`示例动画 第二期/`), or a bracket name whose
    /// episode number has a group of its own. Otherwise `第N期` alone is an
    /// episode (variety shows: `示例综艺 第12期`).
    pub period_season: bool,
}

/// The result of parsing one name.
#[derive(Debug, Clone, Default)]
pub(crate) struct Stem {
    pub title: Option<String>,
    pub year: Option<u16>,
    /// The year came from `(YYYY)`/`[YYYY]`: the canonical Plex/Jellyfin form.
    pub year_bracketed: bool,
    pub season: Option<u16>,
    pub episodes: Vec<u16>,
    pub absolute: Option<u32>,
    pub air_date: Option<String>,
    pub episode_title: Option<String>,
    pub marker: Option<Marker>,
    pub edition: Option<String>,
    /// The edition is a free-form ` - label` (Jellyfin multi-version naming).
    pub version_label: bool,
    pub part: Option<u8>,
    pub resolution: Option<String>,
    pub source: Option<String>,
    pub video_codec: Option<String>,
    pub release_group: Option<String>,
    /// Number of recognised technical tags.
    pub tags: u16,
    /// A plausible year kept in the title because a later year won
    /// ("2001 A Space Odyssey" 1968). Ambiguous names get lower confidence.
    pub year_like_in_title: Option<u16>,
    /// An episode marker was ignored because a year follows it
    /// ("Star Wars Episode 4 1977").
    pub ignored_episode_marker: bool,
    /// The title ends with a bare number that may be an episode or a sequel
    /// number ("FooBar 7", "Toy Story 3"); lowers confidence.
    pub trailing_number: bool,
    /// Text removed from the title as an alternative title ("(The Prestige)").
    pub alt_title: Option<String>,
    /// The CJK half of a bilingual title (`示例剧.Some.Show` → "示例剧"; the
    /// title is then "Some Show").
    pub original_title: Option<String>,
    /// The whole title before a bilingual split ("示例剧 Some Show").
    pub full_title: Option<String>,
    /// The name was made only of bracket groups and read group by group.
    pub bracket_layout: bool,
}

impl Stem {
    /// The name carries episode information (not just a season).
    pub(crate) fn has_episode_info(&self) -> bool {
        !self.episodes.is_empty() || self.absolute.is_some() || self.air_date.is_some()
    }
}

/// An anchor found in the token stream: tokens `start..end`.
#[derive(Debug, Clone)]
struct Anchor {
    start: usize,
    end: usize,
    marker: Marker,
    season: Option<u16>,
    episodes: Vec<u16>,
    absolute: Option<u32>,
    air_date: Option<String>,
    /// Tokens inside the anchor that belong to neither marker part
    /// ("(2016) 720p" in "Stagione 6 (2016) 720p Episodio 13").
    inner: Option<(usize, usize)>,
    /// Spelled-out form ("Episode 4"), which a following year can veto.
    spelled: bool,
}

impl Anchor {
    fn new(start: usize, end: usize, marker: Marker) -> Self {
        Anchor {
            start,
            end,
            marker,
            season: None,
            episodes: Vec::new(),
            absolute: None,
            air_date: None,
            inner: None,
            spelled: false,
        }
    }
}

// Within one word (dots and spaces are already separators).
static SXE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^s(\d{1,4})[-x]?e(\d{1,4})((?:-?e\d{1,4}|-\d{1,4})*)(?:v\d)?$").expect("valid SxE regex")
});
static E_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^e(\d{1,4})((?:-?e\d{1,4}|-\d{1,4})*)$").expect("valid E regex"));
static EP_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^ep?(\d{1,4})$").expect("valid Ep regex"));
/// `S01`, and `S07D1` (season 7, disc 1).
static S_ONLY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^s(\d{1,3})(?:d\d{1,2})?$").expect("valid S regex"));
static NXN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(\d{1,2}|(?:19|20)\d{2})x(\d{1,3})((?:x\d{1,3}|-\d{1,2}x\d{1,3}|-\d{1,3})*)$")
        .expect("valid NxN regex")
});
static DATE_WORD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{4})-(\d{2})-(\d{2})$").expect("valid date regex"));
/// `12`, `12v2`, `02-03` (absolute numbers and ranges).
static NUM_RANGE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(\d{1,4})(?:-(\d{1,4}))?(v\d)?$").expect("valid number regex"));
/// `1of4`.
static OF_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(\d{1,3})of(\d{1,3})$").expect("valid of regex"));
/// `Temporada1`, `Season2`.
static SEASON_GLUED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:season|saison|staffel|temporada|stagione|seizoen|sezon)(\d{1,3})$")
        .expect("valid regex")
});
/// `第12集`, `第十二话`, `第3季` as a whole word (the tokenizer cuts them out).
static CJK_MARKER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^第([0-9０-９]{1,4}|[零〇一二两兩三四五六七八九十百千]{1,7})([集话話回期季])$")
        .expect("valid CJK marker regex")
});
/// A web address inside a bracket group (`[某某影视分享example.co]`): `www.`
/// anywhere, or a common top-level domain ending the group (so a title such
/// as "Lost.in.Space" is not an address).
static SITE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)www\.[a-z0-9-]+\.[a-z]{2,}|(?:^|[^a-z0-9])[a-z0-9-]+\.(?:com|net|org|co|cc|tv|me|top|xyz|cn|info|io|la|vip|app|club|site|pw|to)(?:\.[a-z]{2})?$",
    )
    .expect("valid site regex")
});

/// Season words in the languages release names use.
const SEASON_WORDS: &[&str] = &[
    "season",
    "series",
    "saison",
    "staffel",
    "temporada",
    "temp",
    "stagione",
    "seizoen",
    "sezon",
    "kausi",
    "säsong",
    "sæson",
];
/// Episode words; `cap`/`capitulo` (Spanish) may carry a packed `102`.
const EPISODE_WORDS: &[&str] = &[
    "episode",
    "ep",
    "e",
    "episodio",
    "épisode",
    "episod",
    "folge",
    "afl",
    "aflevering",
    "cap",
    "capitulo",
    "capítulo",
];

/// Parses one name. Never fails: unknown names come back with just a title.
pub(crate) fn parse_stem(raw: &str, opts: StemOpts) -> Stem {
    let raw = raw.trim();
    if let Some(st) = bracket_name(raw, opts) {
        return st;
    }
    let (group, s) = leading_group(raw);
    parse_core(s, opts, group)
}

/// Fansub convention: a leading `[Group]` (or `【Group】`). A sharing site's
/// prefix (`[某某影视分享example.co]`) is dropped without becoming the group.
/// Returns the group and the rest of the name.
fn leading_group(s: &str) -> (Option<String>, &str) {
    let (rest, close) = if let Some(r) = s.strip_prefix('[') {
        (r, ']')
    } else if let Some(r) = s.strip_prefix('【') {
        (r, '】')
    } else {
        return (None, s);
    };
    let Some(end) = rest.find(close) else {
        return (None, s);
    };
    let inner = rest[..end].trim();
    let after = rest[end + close.len_utf8()..].trim_start_matches([' ', '.', '_']);
    if inner.is_empty() || after.is_empty() || year_of(inner).is_some() || vocab::classify(inner).is_some() {
        return (None, s);
    }
    if is_site(inner) {
        return (None, after);
    }
    (Some(inner.to_string()), after)
}

/// A sharing site's tag: a web address, or the words for "sharing",
/// "forum" or "download" (`[某某影视分享]`, `[某某论坛]`).
fn is_site(inner: &str) -> bool {
    SITE_RE.is_match(inner)
        || ["分享", "论坛", "論壇", "下载", "下載"]
            .iter()
            .any(|w| inner.contains(w))
}

/// Parses a name whose fansub group (if any) is already removed.
fn parse_core(s: &str, opts: StemOpts, group: Option<String>) -> Stem {
    let mut out = Stem {
        release_group: group,
        ..Stem::default()
    };
    let mut toks = split_hyphenated(s, tokenize(s));
    let lead = leading_junk(s, &toks);
    toks.drain(..lead);
    let n = toks.len();
    if n == 0 {
        return out;
    }

    let actx = AnchorCtx {
        tv: opts.tv,
        group: out.release_group.is_some(),
        movie_lib: opts.movie_lib,
        folder: opts.folder,
        period_season: opts.period_season,
    };
    let mut anchor = (0..n).find_map(|i| anchor_at(s, &toks, i, actx));
    // "Star Wars Episode 4 1977": a spelled-out episode number followed by a
    // year is part of a movie title, unless we know this is TV.
    if let Some(a) = &anchor {
        if a.spelled
            && !opts.tv
            && toks[a.end..]
                .iter()
                .any(|t| t.is_word() && year_of(t.text(s)).is_some())
        {
            anchor = None;
            out.ignored_episode_marker = true;
        }
    }
    let anchor_start = anchor.as_ref().map_or(n, |a| a.start);
    let zone_end = anchor_start.min(find_tag_boundary(s, &toks, 1));

    // Year and the end of the title.
    let (title_end, rest_start, tail_start) = match choose_year(s, &toks, zone_end, anchor_start) {
        Some(y) => {
            out.year = Some(y.year);
            out.year_bracketed = y.bracketed;
            let tail = if y.bracketed {
                anchor_start.min(find_tag_boundary(s, &toks, y.end))
            } else {
                zone_end
            };
            (y.idx, y.end, tail.max(y.end))
        }
        None => (zone_end, zone_end, zone_end),
    };

    // Title zone [t_lo, t_hi).
    let (t_lo, mut t_hi) = trim_range(&toks, 0, title_end);
    if out.year.is_none() && anchor.is_none() {
        t_hi = trailing_part(s, &toks, t_lo, t_hi, opts, &mut out);
        if !opts.folder {
            if let Some(a) = trailing_number(s, &toks, t_lo, t_hi, opts, out.release_group.is_some()) {
                t_hi = trim_range(&toks, t_lo, a.start).1;
                anchor = Some(a);
            }
        }
    }
    if out.year.is_none() && anchor.is_none() && !opts.folder && t_hi > t_lo {
        // "FooBar 7", "Toy Story 3": an episode or a sequel number. Keep it in
        // the title but flag the ambiguity.
        out.trailing_number =
            word_at(s, &toks, t_hi - 1).is_some_and(|w| w.len() <= 3 && number(w).is_some());
    }
    // "La petite bande (Michel Deville - 1983)": the year closed the zone
    // inside a parenthesis; the title ends before it.
    if let Some(open) = (t_lo..t_hi).find(|&i| matches!(toks[i].kind, TokKind::Open(_))) {
        if !toks[open..t_hi]
            .iter()
            .any(|t| matches!(t.kind, TokKind::Close(_)))
            && open > t_lo
        {
            t_hi = trim_range(&toks, t_lo, open).1;
        }
    }
    t_hi = strip_trailing_edition(s, &toks, t_lo, t_hi, &mut out);
    t_hi = strip_alt_title(s, &toks, t_lo, t_hi, &mut out);
    for t in toks[t_lo..t_hi].iter().filter(|t| t.is_word()) {
        if let Some(y) = year_of(t.text(s)) {
            if out.year.is_some() {
                out.year_like_in_title = Some(y);
            }
        }
    }
    // "[示例片]" alone in the title zone: the bracket is the title.
    let (t_lo, t_hi) = unwrap_bracket(&toks, t_lo, t_hi);
    let title = match split_bilingual(s, &toks, t_lo, t_hi) {
        Some(((a, b), (c, d))) => {
            out.original_title = Some(assemble(s, &toks[c..d])).filter(|o| !o.is_empty());
            out.full_title = Some(assemble(s, &toks[t_lo..t_hi]));
            invert_article(assemble(s, &toks[a..b]))
        }
        None => invert_article(assemble(s, &toks[t_lo..t_hi])),
    };
    out.title = (!title.is_empty()).then_some(title);

    // Between the year and the tags: edition, part or a version label.
    if rest_start < tail_start {
        let after_dash = toks[rest_start].kind == TokKind::Dash;
        let (lo, hi) = trim_range(&toks, rest_start, tail_start);
        let left = &toks[lo..hi];
        let words: Vec<&str> = left.iter().filter(|t| t.is_word()).map(|t| t.text(s)).collect();
        if !words.is_empty() {
            if let Some((p, _)) = vocab::part_at(&words, true).filter(|(_, u)| *u == words.len()) {
                out.part = Some(p);
            } else if let Some((ed, _)) = vocab::edition_at(&words).filter(|(_, u)| *u == words.len()) {
                out.edition = Some(ed.to_string());
            } else if after_dash || out.year_bracketed {
                // Jellyfin multi-version naming: "Title (Year) - Label.ext".
                let label = assemble(s, left);
                if !label.is_empty() {
                    out.edition = Some(label);
                    out.version_label = true;
                }
            }
        }
    }

    // Episode marker, a year next to it, and the episode title.
    let mut ep_title_range = (n, n);
    let mut year_range = None;
    if let Some(a) = &anchor {
        out.marker = Some(a.marker);
        out.season = a.season;
        out.episodes = a.episodes.clone();
        out.absolute = a.absolute;
        out.air_date = a.air_date.clone();
        let mut st = a.end;
        while st < n && toks[st].kind == TokKind::Dash {
            st += 1;
        }
        if out.year.is_none() && a.marker != Marker::Date {
            // "Show.E02.2010", "Breaking.Bad.S01E01.2008.BluRay", "Stagione 6 (2016) … Episodio 13".
            if let Some((y, lo, hi)) = year_near_anchor(s, &toks, a, st) {
                out.year = Some(y);
                year_range = Some((lo, hi));
                if lo >= st {
                    st = hi;
                }
            }
        }
        let en = find_tag_boundary(s, &toks, st.max(1));
        if st < en {
            let (lo, hi) = trim_range(&toks, st, en);
            let et = assemble(s, &toks[lo..hi]);
            if !et.is_empty() {
                out.episode_title = Some(et);
            }
            ep_title_range = (st, en);
        }
    }

    // Technical tail.
    let skip = [
        anchor.as_ref().map(|a| (a.start, a.end)),
        year_range,
        Some(ep_title_range),
    ];
    scan_tail(s, &toks, tail_start, &skip, &mut out);
    if out.release_group.is_none() {
        out.release_group = group_from_last_word(s, &toks, tail_start.min(anchor_start));
    }
    out
}

/// The year of `w` when it is a plausible 4-digit year.
pub(crate) fn year_of(w: &str) -> Option<u16> {
    if w.len() != 4 || !w.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    w.parse::<u16>()
        .ok()
        .filter(|y| (MIN_YEAR..=MAX_YEAR).contains(y))
}

fn number(w: &str) -> Option<u32> {
    if w.is_empty() || w.len() > 4 || !w.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    w.parse().ok()
}

fn word_at<'a>(s: &'a str, toks: &[Tok], i: usize) -> Option<&'a str> {
    toks.get(i).filter(|t| t.is_word()).map(|t| t.text(s))
}

/// Splits hyphen-joined release names (`Perfect Child-2007-TRUEFRENCH-TVRip`)
/// into words when a later part is a year or an inner part is a tag. Plain
/// hyphenated words (`Spider-Man`, `9-1-1`), tag compounds (`WEB-DL`) and the
/// trailing `tag-GROUP` are left alone.
fn split_hyphenated(s: &str, toks: Vec<Tok>) -> Vec<Tok> {
    if !toks.iter().any(|t| t.is_word() && t.text(s).contains('-')) {
        return toks;
    }
    let mut out = Vec::with_capacity(toks.len() + 4);
    for t in toks {
        let w = t.text(s);
        let mut spans: Vec<(usize, usize)> = Vec::new();
        let mut st = 0;
        for (i, _) in w.match_indices('-') {
            spans.push((st, i));
            st = i + 1;
        }
        spans.push((st, w.len()));
        let is_marker =
            |p: &str| SXE_RE.is_match(p) || S_ONLY_RE.is_match(p) || E_RE.is_match(p) || NXN_RE.is_match(p);
        // Split "Child-2007-TRUEFRENCH-TVRip" (year, inner tag) and
        // "Parks_and_Recreation-s03-e01" (a marker after a title word), but
        // not "S01E01-E03" (a marker continuation, which may be a range).
        let split = t.is_word()
            && !w.starts_with('-')
            && spans.len() >= 2
            && !vocab::is_hyphenated_tag(w)
            && !is_marker(&w[spans[0].0..spans[0].1])
            && spans.iter().enumerate().skip(1).any(|(k, (a, b))| {
                let p = &w[*a..*b];
                year_of(p).is_some()
                    || is_marker(p)
                    || (k + 1 < spans.len() && vocab::classify(p).is_some_and(|t| t.strong))
            });
        if split {
            for (a, b) in spans.into_iter().filter(|(a, b)| b > a) {
                out.push(Tok {
                    kind: TokKind::Word,
                    start: t.start + a,
                    end: t.start + b,
                });
            }
        } else {
            out.push(t);
        }
    }
    out
}

/// Leading tokens that are not part of a title: strong tags and dashes
/// (`h265 - HEVC Riddick …`) and list numbering (`(1)The Girl …`).
fn leading_junk(s: &str, toks: &[Tok]) -> usize {
    let mut i = 0;
    while i + 1 < toks.len() {
        let t = toks[i];
        let lang_dash = t.is_word()
            && vocab::classify(t.text(s)).is_some_and(|g| g.kind == TagKind::Language)
            && toks.get(i + 1).is_some_and(|n| n.kind == TokKind::Dash);
        if t.kind == TokKind::Dash
            || lang_dash
            || (t.is_word() && vocab::classify(t.text(s)).is_some_and(|g| g.strong))
        {
            i += 1;
        } else if t.kind == TokKind::Open(b'(')
            && word_at(s, toks, i + 1).is_some_and(|w| w.len() <= 2 && number(w).is_some())
            && toks.get(i + 2).is_some_and(|t| t.kind == TokKind::Close(b')'))
            && i + 3 < toks.len()
        {
            i += 3;
        } else if let Some(close) = tag_bracket(s, toks, i) {
            // "[Group] [BD][1080p] Title 08": tag-only brackets before the title.
            i = close + 1;
        } else {
            break;
        }
    }
    i
}

/// What the anchor search may assume.
#[derive(Debug, Clone, Copy)]
struct AnchorCtx {
    /// Episode context (shows library, season folder).
    tv: bool,
    /// The name starts with a fansub `[Group]`.
    group: bool,
    /// A movie library.
    movie_lib: bool,
    /// Parsing a folder name (no bare episode numbers).
    folder: bool,
    /// See [`StemOpts::period_season`].
    period_season: bool,
}

/// When token `i` opens a `[…]` group whose words are all tags, the index of
/// its closing bracket.
fn tag_bracket(s: &str, toks: &[Tok], i: usize) -> Option<usize> {
    if toks.get(i)?.kind != TokKind::Open(b'[') {
        return None;
    }
    let close = (i + 1..toks.len()).find(|&j| toks[j].kind == TokKind::Close(b']'))?;
    let inner = &toks[i + 1..close];
    let all_tags = !inner.is_empty()
        && inner
            .iter()
            .all(|t| t.is_word() && vocab::classify(t.text(s)).is_some());
    (all_tags && close + 1 < toks.len()).then_some(close)
}

/// Finds an episode/date/absolute marker starting at token `i`.
fn anchor_at(s: &str, toks: &[Tok], i: usize, actx: AnchorCtx) -> Option<Anchor> {
    if toks[i].kind == TokKind::Dash {
        return absolute_at(s, toks, i, actx);
    }
    let w = word_at(s, toks, i)?;
    // A CJK marker anchors only as cjk_anchor decides (in a movie library
    // `第三期` is a title word, not an episode for the rules below).
    let cjk = cjk_anchor(s, toks, i, w, actx);
    if cjk.is_some() || cjk_marker_unit(w).is_some() {
        return cjk;
    }
    // Fansub "[Group] Monster 34 - At the End of Darkness": a bare number
    // followed by a dash and a word (not "Title 2 - 09", where 09 is the episode).
    let dash_then_word = toks.get(i + 1).is_some_and(|t| t.kind == TokKind::Dash)
        && word_at(s, toks, i + 2).is_some_and(|n| !NUM_RANGE_RE.is_match(n));
    if actx.group && i > 0 && dash_then_word {
        if let Some(c) = NUM_RANGE_RE.captures(w).filter(|c| year_of(&c[1]).is_none()) {
            let mut a = Anchor::new(i, i + 1, Marker::Absolute);
            fill_absolute(&mut a, &c)?;
            return Some(a);
        }
    }
    if let Some(c) = SXE_RE.captures(w) {
        let mut a = Anchor::new(i, i + 1, Marker::SxxEyy);
        a.season = Some(c[1].parse().ok()?);
        a.episodes = episode_list(c[2].parse().ok()?, c.get(3).map_or("", |m| m.as_str()));
        extend_multi(s, toks, &mut a);
        return Some(a);
    }
    if let Some(c) = S_ONLY_RE.captures(w) {
        let season: u16 = c[1].parse().ok()?;
        // "S01 E02" / "S01.E02"
        if let Some(c2) = word_at(s, toks, i + 1).and_then(|w2| E_RE.captures(w2)) {
            let mut a = Anchor::new(i, i + 2, Marker::SxxEyy);
            a.season = Some(season);
            a.episodes = episode_list(c2[1].parse().ok()?, c2.get(2).map_or("", |m| m.as_str()));
            extend_multi(s, toks, &mut a);
            return Some(a);
        }
        return (i > 0).then(|| season_then_episode(s, toks, i, i + 1, season));
    }
    if let Some(c) = NXN_RE.captures(w) {
        // "1920x1080" is not caught (4 digits); "2x4" needs a 2-digit episode.
        if c[2].len() >= 2 || i > 0 {
            let mut a = Anchor::new(i, i + 1, Marker::NxNN);
            a.season = Some(c[1].parse().ok()?);
            a.episodes = vec![c[2].parse().ok()?];
            let cont = c.get(3).map_or("", |m| m.as_str());
            // "16x03-05": a range within the season.
            if let Some(last) = cont.strip_prefix('-').and_then(|l| l.parse::<u16>().ok()) {
                let first = a.episodes[0];
                if last > first + 1 && last - first <= 30 {
                    a.episodes = (first..=last).collect();
                    extend_multi(s, toks, &mut a);
                    return Some(a);
                }
            }
            for piece in cont.split(['x', 'X', '-']).filter(|p| !p.is_empty()) {
                // "-1x04": the season repeats before the next episode; skip it.
                let is_season = cont.contains(&format!("-{piece}x")) || cont.contains(&format!("-{piece}X"));
                if let (false, Ok(e)) = (is_season, piece.parse::<u16>()) {
                    a.episodes.push(e);
                }
            }
            a.episodes.dedup();
            extend_multi(s, toks, &mut a);
            return Some(a);
        }
    }
    if let Some((season, end)) = season_words(s, toks, i) {
        return (i > 0).then(|| season_then_episode(s, toks, i, end, season));
    }
    // "E13 - The Wedding of River Song.mkv" inside a season folder starts
    // with its marker; elsewhere a marker needs a title before it.
    if i > 0 || actx.tv {
        if let Some(p) = episode_part(s, toks, i) {
            let marker = if p.season.is_some() {
                Marker::Packed
            } else {
                Marker::EpisodeOnly
            };
            let mut a = Anchor::new(i, p.end, marker);
            a.season = p.season;
            a.episodes = p.episodes;
            a.spelled = p.spelled;
            return Some(a);
        }
    }
    date_at(s, toks, i)
}

/// `第12集` / `第十二话` (an episode), `第2季` (a season, maybe followed by its
/// episode: `第2季第3集`, `第2季 E03`, `第二季 05`), and the spaced `第 12 集`.
/// Unlike Latin markers these count at the start of a name too.
///
/// `第N期` is a season when an episode follows it (`示例动画 第二期 05`,
/// anime's "second cour"), and an episode otherwise (`示例综艺 第12期`, a
/// variety show's issue). In a movie library only an episode unit
/// (集/话/話) anchors: 回 ("round"), 期 ("phase") and 季 alone stay title
/// words, as a bare number would.
fn cjk_anchor(s: &str, toks: &[Tok], i: usize, w: &str, actx: AnchorCtx) -> Option<Anchor> {
    let (unit, n, end) = match cjk_marker_unit(w) {
        Some((unit, n)) => (unit, n, i + 1),
        None if w == "第" => {
            let n = word_at(s, toks, i + 1).and_then(cjk_number)?;
            let unit = match word_at(s, toks, i + 2)? {
                u @ ("集" | "话" | "話" | "回" | "期" | "季") => u.chars().next()?,
                _ => return None,
            };
            (unit, u16::try_from(n).ok()?, i + 3)
        }
        None => return None,
    };
    let movie = actx.movie_lib && !actx.tv;
    let episode = || {
        let mut a = Anchor::new(i, end, Marker::CjkEpisode);
        a.episodes = vec![n];
        a
    };
    match unit {
        '集' | '话' | '話' => Some(episode()),
        '回' => (!movie).then(episode),
        _ => {
            // 季 or 期: a season when something numbers the episode after it.
            if let Some(a) = season_episode(s, toks, i, end, n, !movie && !actx.folder) {
                return Some(a);
            }
            if movie {
                None
            } else if unit == '季' || actx.period_season {
                let mut a = Anchor::new(i, end, Marker::SeasonOnly);
                a.season = Some(n);
                Some(a)
            } else {
                Some(episode())
            }
        }
    }
}

/// After a season marker ending at `season_end`: its episode, as an episode
/// part a few tokens on (`第2季第3集`, `第2季 E03`) or, with `bare`, a 1–3
/// digit number right after it (`第二季 05`, `第2季 - 05`, `第二期 01-02`).
fn season_episode(
    s: &str,
    toks: &[Tok],
    start: usize,
    season_end: usize,
    season: u16,
    bare: bool,
) -> Option<Anchor> {
    let a = season_then_episode(s, toks, start, season_end, season);
    if a.marker != Marker::SeasonOnly {
        return Some(a);
    }
    if !bare {
        return None;
    }
    let mut j = season_end;
    while j < toks.len() && toks[j].kind == TokKind::Dash {
        j += 1;
    }
    let c = word_at(s, toks, j).and_then(|w| NUM_RANGE_RE.captures(w))?;
    if c[1].len() > 3 {
        return None;
    }
    let mut a = Anchor::new(start, j + 1, Marker::Words);
    fill_absolute(&mut a, &c)?;
    a.absolute = None;
    if a.episodes.is_empty() {
        a.episodes = vec![c[1].parse().ok()?];
    }
    a.season = Some(season);
    Some(a)
}

/// `第12集` → `(false, 12)`, `第二季` → `(true, 2)`.
pub(crate) fn cjk_marker(w: &str) -> Option<(bool, u16)> {
    cjk_marker_unit(w).map(|(unit, n)| (unit == '季', n))
}

/// `第12集` → `('集', 12)`, `第二期` → `('期', 2)`.
pub(crate) fn cjk_marker_unit(w: &str) -> Option<(char, u16)> {
    let c = CJK_MARKER_RE.captures(w)?;
    let n = u16::try_from(cjk_number(&c[1])?).ok()?;
    Some((c[2].chars().next()?, n))
}

/// Arabic (also full-width) or Chinese numerals: `12`, `１２`, `十二`,
/// `二十三`, `一百零五`.
pub(crate) fn cjk_number(w: &str) -> Option<u32> {
    if w.is_empty() || w.chars().count() > 7 {
        return None;
    }
    if w.chars()
        .all(|c| c.is_ascii_digit() || ('０'..='９').contains(&c))
    {
        let digits: String = w
            .chars()
            .map(|c| {
                if c.is_ascii_digit() {
                    c
                } else {
                    char::from_u32(c as u32 - '０' as u32 + '0' as u32).unwrap_or('0')
                }
            })
            .collect();
        return digits.parse().ok();
    }
    let (mut total, mut cur) = (0u32, 0u32);
    for c in w.chars() {
        let digit = match c {
            '零' | '〇' => 0,
            '一' => 1,
            '二' | '两' | '兩' => 2,
            '三' => 3,
            '四' => 4,
            '五' => 5,
            '六' => 6,
            '七' => 7,
            '八' => 8,
            '九' => 9,
            '十' | '百' | '千' => {
                let unit = match c {
                    '十' => 10,
                    '百' => 100,
                    _ => 1000,
                };
                total += cur.max(1) * unit;
                cur = 0;
                continue;
            }
            _ => return None,
        };
        cur = digit;
    }
    Some(total + cur)
}

/// `Season 2`, `Temporada 1`, `Season 3of5`, `Temporada1`.
fn season_words(s: &str, toks: &[Tok], i: usize) -> Option<(u16, usize)> {
    let w = word_at(s, toks, i)?;
    if let Some(c) = SEASON_GLUED_RE.captures(w) {
        return Some((c[1].parse().ok()?, i + 1));
    }
    if !SEASON_WORDS.contains(&w.to_lowercase().as_str()) {
        return None;
    }
    let next = word_at(s, toks, i + 1)?;
    let num = OF_RE
        .captures(next)
        .map_or(next, |c| c.get(1).map_or(next, |m| m.as_str()));
    // "The.Series.2019.1080p": a year after "Series" is not a season.
    if year_of(num).is_some() {
        return None;
    }
    let season = u16::try_from(number(num)?).ok()?;
    Some((season, i + 2))
}

/// An episode part: `E05`, `Ep 5`, `Episode 5`, `Episodio 13`, `1of4`,
/// `1 of 6`, `Cap.102` (season 1, episode 2), or a full `S01E05`.
struct EpPart {
    season: Option<u16>,
    episodes: Vec<u16>,
    end: usize,
    spelled: bool,
}

fn episode_part(s: &str, toks: &[Tok], j: usize) -> Option<EpPart> {
    let w = word_at(s, toks, j)?;
    if let Some((false, n)) = cjk_marker(w) {
        return Some(EpPart {
            season: None,
            episodes: vec![n],
            end: j + 1,
            spelled: false,
        });
    }
    if let Some(c) = SXE_RE.captures(w) {
        let episodes = episode_list(c[2].parse().ok()?, c.get(3).map_or("", |m| m.as_str()));
        return Some(EpPart {
            season: c[1].parse().ok(),
            episodes,
            end: j + 1,
            spelled: false,
        });
    }
    if let Some(c) = E_RE.captures(w).or_else(|| EP_RE.captures(w)) {
        let episodes = episode_list(c[1].parse().ok()?, c.get(2).map_or("", |m| m.as_str()));
        return Some(EpPart {
            season: None,
            episodes,
            end: j + 1,
            spelled: false,
        });
    }
    if let Some(c) = OF_RE.captures(w) {
        return Some(EpPart {
            season: None,
            episodes: vec![c[1].parse().ok()?],
            end: j + 1,
            spelled: false,
        });
    }
    let lw = w.to_lowercase();
    if let Some(ep) = word_at(s, toks, j + 1).and_then(number) {
        if EPISODE_WORDS.contains(&lw.as_str()) {
            let digits = word_at(s, toks, j + 1).map_or(0, str::len);
            if lw.starts_with("cap") && digits >= 3 {
                // Spanish releases: "Cap.102" = season 1, episode 2; "Cap.1503" = 15×03.
                let (season, e) = (ep / 100, ep % 100);
                let mut episodes = vec![u16::try_from(e).ok()?];
                // "Cap.112_114": a range within the season.
                if let Some(last) = word_at(s, toks, j + 2)
                    .and_then(number)
                    .filter(|l| l / 100 == season)
                {
                    episodes = ((e as u16)..=(u16::try_from(last % 100).ok()?)).collect();
                }
                let end = if episodes.len() > 1 { j + 3 } else { j + 2 };
                return Some(EpPart {
                    season: u16::try_from(season).ok(),
                    episodes,
                    end,
                    spelled: false,
                });
            }
            return Some(EpPart {
                season: None,
                episodes: vec![u16::try_from(ep).ok()?],
                end: j + 2,
                spelled: true,
            });
        }
    }
    // "1 of 6"
    if let (Some(ep), Some(of), Some(_)) = (
        number(w).filter(|_| w.len() <= 3),
        word_at(s, toks, j + 1),
        word_at(s, toks, j + 2).and_then(number),
    ) {
        if of.eq_ignore_ascii_case("of") {
            return Some(EpPart {
                season: None,
                episodes: vec![u16::try_from(ep).ok()?],
                end: j + 3,
                spelled: true,
            });
        }
    }
    None
}

/// After a season marker, looks a few tokens ahead for its episode part
/// ("Season.2.1of4", "Temporada 4 [HDTV][Cap.408]", "Stagione 6 (2016) 720p
/// Episodio 13"); otherwise it is a season-only marker (a season pack).
fn season_then_episode(s: &str, toks: &[Tok], start: usize, season_end: usize, season: u16) -> Anchor {
    let limit = (season_end + 8).min(toks.len());
    for j in season_end..limit {
        if let Some(p) = episode_part(s, toks, j) {
            let mut a = Anchor::new(start, p.end, Marker::Words);
            a.season = p.season.or(Some(season));
            a.episodes = p.episodes;
            a.inner = (j > season_end).then_some((season_end, j));
            return a;
        }
    }
    let mut a = Anchor::new(start, season_end, Marker::SeasonOnly);
    a.season = Some(season);
    a
}

/// Absorbs further markers of the same season after an episode marker:
/// `S01E00 & S01E01`, `S01E02 - S01E03 - S01E04`, `1x02 - 1x03`, `S01.E02.E03`.
fn extend_multi(s: &str, toks: &[Tok], a: &mut Anchor) {
    let mut j = a.end;
    loop {
        let mut k = j;
        while k < toks.len()
            && (toks[k].kind == TokKind::Dash
                || word_at(s, toks, k).is_some_and(|w| matches!(w, "&" | "+" | "," | "and" | "&amp;")))
        {
            k += 1;
        }
        let Some(w) = word_at(s, toks, k) else { break };
        let more: Option<Vec<u16>> = if let Some(c) = SXE_RE.captures(w) {
            (c[1].parse::<u16>().ok() == a.season)
                .then(|| {
                    c[2].parse()
                        .ok()
                        .map(|f| episode_list(f, c.get(3).map_or("", |m| m.as_str())))
                })
                .flatten()
        } else if let Some(c) = NXN_RE.captures(w) {
            (c[1].parse::<u16>().ok() == a.season)
                .then(|| c[2].parse().ok().map(|e| vec![e]))
                .flatten()
        } else if let Some(c) = E_RE.captures(w) {
            c[1].parse()
                .ok()
                .map(|f| episode_list(f, c.get(2).map_or("", |m| m.as_str())))
        } else {
            None
        };
        match more {
            Some(eps) => {
                for e in eps {
                    if !a.episodes.contains(&e) {
                        a.episodes.push(e);
                    }
                }
                j = k + 1;
            }
            None => break,
        }
    }
    a.end = j;
}

/// `Title - 12`, `[Group] Title - 1100 [1080p]`, `Bleach - 02-03`: an absolute
/// episode number (or range) after a stand-alone dash, followed by the end, a
/// bracket, a dash, tags or punctuation.
fn absolute_at(s: &str, toks: &[Tok], i: usize, actx: AnchorCtx) -> Option<Anchor> {
    if i == 0 {
        return None;
    }
    let w = word_at(s, toks, i + 1)?;
    let c = NUM_RANGE_RE.captures(w)?;
    if year_of(&c[1]).is_some() {
        return None;
    }
    // Outside an episode context, "Show Name - 722 [HD]" still reads as an
    // episode when the number has 2–3 digits; never in a movie library, and
    // never for 4 digits ("Blade Runner - 2049").
    let allow = actx.tv || actx.group || (!actx.movie_lib && (2..=3).contains(&c[1].len()));
    if !allow {
        return None;
    }
    let follows_ok = match toks.get(i + 2) {
        None => true,
        Some(t) => match t.kind {
            TokKind::Open(_) | TokKind::Dash => true,
            TokKind::Word => {
                let w = t.text(s);
                vocab::classify(w).is_some() || !w.chars().any(char::is_alphanumeric)
            }
            TokKind::Close(_) => false,
        },
    };
    if !follows_ok {
        return None;
    }
    let mut a = Anchor::new(i, i + 2, Marker::Absolute);
    fill_absolute(&mut a, &c)?;
    Some(a)
}

fn fill_absolute(a: &mut Anchor, c: &regex::Captures<'_>) -> Option<()> {
    let first: u32 = c[1].parse().ok()?;
    a.absolute = Some(first);
    if let Some(last) = c.get(2).and_then(|m| m.as_str().parse::<u32>().ok()) {
        if last > first && last - first <= 30 {
            a.episodes = (first..=last).filter_map(|e| u16::try_from(e).ok()).collect();
        }
    }
    Some(())
}

/// A bare number ending the title zone, read as an episode only with
/// evidence: after a fansub `[Group]` it is an absolute number
/// (`[Group] Title 14 [DVD]`); in an episode context `102`/`0106` is a packed
/// season+episode and `7`/`07v4` an episode.
fn trailing_number(
    s: &str,
    toks: &[Tok],
    lo: usize,
    hi: usize,
    opts: StemOpts,
    group: bool,
) -> Option<Anchor> {
    // A TV-broadcast source right after the number is episode evidence too
    // ("the.mentalist.501.hdtv-lol", "FooBar.7.PDTV").
    let broadcast = word_at(s, toks, hi).is_some_and(|w| {
        let base = w.split('-').next().unwrap_or(w).to_ascii_lowercase();
        matches!(base.as_str(), "hdtv" | "pdtv" | "sdtv" | "dsr" | "hdtvrip")
    });
    if hi < lo + 2 || opts.movie_lib || !(group || opts.tv || broadcast) {
        return None;
    }
    let k = hi - 1;
    let w = word_at(s, toks, k)?;
    // "示例2": a number glued to a CJK title is a sequel number.
    if toks[k].start == toks[k - 1].end && has_cjk(toks[k - 1].text(s)) {
        return None;
    }
    let c = NUM_RANGE_RE.captures(w)?;
    // "Show.Name.Part.3", "Kill Bill Vol 2": a numbered part, not an episode.
    let prev = (lo..k)
        .rev()
        .find_map(|i| word_at(s, toks, i))
        .unwrap_or_default()
        .to_lowercase();
    if matches!(
        prev.as_str(),
        "part" | "pt" | "vol" | "volume" | "chapter" | "cd" | "disc" | "disk"
    ) {
        return None;
    }
    if year_of(&c[1]).is_some() || !toks[lo..k].iter().any(|t| t.is_word()) {
        return None;
    }
    if group {
        let mut a = Anchor::new(k, k + 1, Marker::Absolute);
        fill_absolute(&mut a, &c)?;
        return Some(a);
    }
    let digits = &c[1];
    if c.get(2).is_some() {
        return None;
    }
    if (3..=4).contains(&digits.len()) && c.get(3).is_none() {
        let n: u16 = digits.parse().ok()?;
        let (season, ep) = (n / 100, n % 100);
        if ep == 0 || opts.season_hint.is_some_and(|h| h != season) {
            return None;
        }
        let mut a = Anchor::new(k, k + 1, Marker::Packed);
        a.season = Some(season);
        a.episodes = vec![ep];
        return Some(a);
    }
    if digits.len() <= 2 {
        let mut a = Anchor::new(k, k + 1, Marker::EpisodeOnly);
        a.episodes = vec![digits.parse().ok()?];
        return Some(a);
    }
    None
}

/// A year right after an episode marker (`Show.E02.2010`,
/// `Breaking.Bad.S01E01.2008.BluRay`) or inside a spread-out marker
/// (`Stagione 6 (2016) 720p Episodio 13`). Returns the year and its tokens.
fn year_near_anchor(s: &str, toks: &[Tok], a: &Anchor, after: usize) -> Option<(u16, usize, usize)> {
    if let Some((lo, hi)) = a.inner {
        for i in lo..hi {
            if let Some(y) = bracket_year(s, toks, i) {
                return Some((y, i, i + 3));
            }
            if let Some(y) = word_at(s, toks, i).and_then(year_of) {
                return Some((y, i, i + 1));
            }
        }
    }
    if let Some(y) = bracket_year(s, toks, after) {
        return Some((y, after, after + 3));
    }
    let y = word_at(s, toks, after).and_then(year_of)?;
    let next_ok = match toks.get(after + 1) {
        None => true,
        Some(t) => !t.is_word() || vocab::classify(t.text(s)).is_some() || is_h_codec(s, toks, after + 1),
    };
    next_ok.then_some((y, after, after + 1))
}

/// A trailing multi-part marker without a year: `cd1`/`disc2` always,
/// `part2` only when siblings stack (otherwise "Harry Potter … Part 1"
/// breaks). Returns the new end of the title zone.
fn trailing_part(s: &str, toks: &[Tok], lo: usize, hi: usize, opts: StemOpts, out: &mut Stem) -> usize {
    let idx: Vec<usize> = (lo..hi).filter(|&i| toks[i].is_word()).collect();
    for take in [1usize, 2] {
        if idx.len() > take {
            let first_i = idx[idx.len() - take];
            let words: Vec<&str> = idx[idx.len() - take..].iter().map(|&i| toks[i].text(s)).collect();
            if let Some((p, used)) = vocab::part_at(&words, false) {
                let first = words[0].to_ascii_lowercase();
                let disc_like = ["cd", "dvd", "disc", "disk"].iter().any(|p| first.starts_with(p));
                if used == take && (disc_like || opts.stacked) {
                    out.part = Some(p);
                    return trim_range(toks, lo, first_i).1;
                }
            }
        }
    }
    hi
}

/// Edition words at the end of the title zone (`Alien DC (1979)`,
/// `Aliens.SE.1986`, `… THEATRICAL EDITION (2001)`, `Brazil Criterion
/// Edition`) move to the edition. The title must keep a non-article word, so
/// "The Final Cut" stays a title.
fn strip_trailing_edition(s: &str, toks: &[Tok], lo: usize, mut hi: usize, out: &mut Stem) -> usize {
    loop {
        let idx: Vec<usize> = (lo..hi).filter(|&i| toks[i].is_word()).collect();
        if idx.len() < 2 {
            return hi;
        }
        let words: Vec<&str> = idx.iter().map(|&i| toks[i].text(s)).collect();
        let last = words[words.len() - 1];
        let found = if let Some((ed, 2)) = vocab::edition_at(&words[words.len() - 2..]) {
            Some((ed, 2))
        } else if let Some((ed, 1)) = vocab::edition_at(&[last]) {
            Some((ed, 1))
        } else {
            // Uppercase release abbreviations only ("SE", not "Se").
            match last {
                "DC" => Some(("Director's Cut", 1)),
                "SE" => Some(("Special Edition", 1)),
                "CE" => Some(("Collector's Edition", 1)),
                "UE" => Some(("Ultimate Edition", 1)),
                "OM" => Some(("Open Matte", 1)),
                _ => None,
            }
        };
        let Some((ed, used)) = found else { return hi };
        let keep = &words[..words.len() - used];
        let meaningful = keep
            .iter()
            .any(|w| !matches!(w.to_lowercase().as_str(), "the" | "a" | "an"));
        if !meaningful {
            return hi;
        }
        out.edition.get_or_insert_with(|| ed.to_string());
        hi = trim_range(toks, lo, idx[idx.len() - used]).1;
    }
}

/// A trailing parenthesised group that is not a year or a country code is an
/// alternative title (`Le Prestige (The Prestige)`, `Bunker Palace Hôtel
/// (Enki Bilal)`); "The Office (US)" keeps its country.
fn strip_alt_title(s: &str, toks: &[Tok], lo: usize, hi: usize, out: &mut Stem) -> usize {
    if hi <= lo + 2 || toks[hi - 1].kind != TokKind::Close(b')') {
        return hi;
    }
    let Some(open) = (lo..hi - 1).rev().find(|&i| toks[i].kind == TokKind::Open(b'(')) else {
        return hi;
    };
    if !toks[lo..open].iter().any(|t| t.is_word()) {
        return hi;
    }
    let inner = &s[toks[open].end..toks[hi - 1].start];
    let is_country = matches!(
        inner.trim(),
        "US" | "UK" | "AU" | "NZ" | "CA" | "IE" | "FR" | "DE" | "JP" | "KR"
    );
    if is_country || inner.trim().is_empty() {
        return hi;
    }
    out.alt_title = Some(collapse_ws(inner));
    trim_range(toks, lo, open).1
}

/// "Simpsons, The" → "The Simpsons" (library sort-order naming).
fn invert_article(title: String) -> String {
    for art in ["The", "A", "An"] {
        for sep in [", ", ","] {
            let suffix = format!("{sep}{art}");
            let cut = title.len().saturating_sub(suffix.len());
            if cut > 0 && title.is_char_boundary(cut) && title[cut..].eq_ignore_ascii_case(&suffix) {
                return format!("{art} {}", title[..cut].trim_end());
            }
        }
    }
    title
}

/// `2024-03-15` (one word) or `2024.03.15` / `2024 03 15` (three words).
fn date_at(s: &str, toks: &[Tok], i: usize) -> Option<Anchor> {
    let w = word_at(s, toks, i)?;
    let (y, m, d, end) = if let Some(c) = DATE_WORD_RE.captures(w) {
        (c[1].to_string(), c[2].to_string(), c[3].to_string(), i + 1)
    } else {
        year_of(w)?;
        let (m, d) = (word_at(s, toks, i + 1)?, word_at(s, toks, i + 2)?);
        // Single-character separators only: "2024.03.15", not "2024 - 03 (15)".
        let adjacent = toks[i + 1].start == toks[i].end + 1 && toks[i + 2].start == toks[i + 1].end + 1;
        if m.len() != 2 || d.len() != 2 || !adjacent {
            return None;
        }
        (w.to_string(), m.to_string(), d.to_string(), i + 3)
    };
    let (yy, mm, dd) = (
        y.parse::<u16>().ok()?,
        m.parse::<u8>().ok()?,
        d.parse::<u8>().ok()?,
    );
    if !(MIN_YEAR..=MAX_YEAR).contains(&yy) || !(1..=12).contains(&mm) || !(1..=31).contains(&dd) {
        return None;
    }
    let mut a = Anchor::new(i, end, Marker::Date);
    a.air_date = Some(format!("{yy:04}-{mm:02}-{dd:02}"));
    Some(a)
}

/// Expands the continuation of an episode marker. `E01E02` and `E01-E02-E03`
/// are lists; `E01-E03`/`E01-03` with a gap is a range (Sonarr's "Range" and
/// "Prefixed Range" multi-episode styles).
fn episode_list(first: u16, cont: &str) -> Vec<u16> {
    let mut eps = vec![first];
    let mut dashed = Vec::new();
    let mut cur = String::new();
    let mut dash = false;
    let mut flush = |cur: &mut String, dash: &mut bool, eps: &mut Vec<u16>| {
        if let Ok(n) = cur.parse::<u16>() {
            eps.push(n);
            dashed.push(*dash);
        }
        cur.clear();
        *dash = false;
    };
    for c in cont.chars() {
        match c {
            '-' => {
                flush(&mut cur, &mut dash, &mut eps);
                dash = true;
            }
            'e' | 'E' => {
                let d = dash;
                flush(&mut cur, &mut dash, &mut eps);
                dash = d;
            }
            d if d.is_ascii_digit() => cur.push(d),
            _ => {}
        }
    }
    flush(&mut cur, &mut dash, &mut eps);
    if eps.len() == 2 && dashed.first() == Some(&true) && eps[1] > eps[0] + 1 && eps[1] - eps[0] <= 30 {
        return (eps[0]..=eps[1]).collect();
    }
    eps.dedup();
    eps
}

/// Index of the first token that starts the technical zone, at or after
/// `from` (never 0: a title needs at least one token). Returns `toks.len()`
/// when there is none.
pub(crate) fn find_tag_boundary(s: &str, toks: &[Tok], from: usize) -> usize {
    let n = toks.len();
    let mut i = from.max(1);
    while i < n {
        let t = toks[i];
        match t.kind {
            TokKind::Open(b'[') if bracket_year(s, toks, i).is_none() => return i,
            TokKind::Open(_) => {
                if let Some(w) = word_at(s, toks, i + 1) {
                    if vocab::classify(w).is_some() {
                        return i;
                    }
                }
            }
            TokKind::Word => {
                if is_h_codec(s, toks, i) {
                    return i;
                }
                match vocab::classify(t.text(s)) {
                    Some(tag) if tag.strong => return i,
                    Some(_) if next_word_is_tag(s, toks, i) => return i,
                    _ => {}
                }
            }
            _ => {}
        }
        i += 1;
    }
    n
}

fn next_word_is_tag(s: &str, toks: &[Tok], i: usize) -> bool {
    match toks[i + 1..].iter().position(|t| t.is_word()) {
        Some(off) => {
            let j = i + 1 + off;
            vocab::classify(toks[j].text(s)).is_some() || is_h_codec(s, toks, j)
        }
        None => false,
    }
}

/// `H.264` / `H 265` tokenized as `H`, `264`.
fn is_h_codec(s: &str, toks: &[Tok], i: usize) -> bool {
    word_at(s, toks, i).is_some_and(|w| w.eq_ignore_ascii_case("h"))
        && word_at(s, toks, i + 1).is_some_and(|w| w.starts_with("264") || w.starts_with("265"))
}

/// `(1999)` or `[1999]` starting at token `i`.
fn bracket_year(s: &str, toks: &[Tok], i: usize) -> Option<u16> {
    let open = matches!(toks.get(i)?.kind, TokKind::Open(b'(') | TokKind::Open(b'['));
    let close = matches!(toks.get(i + 2)?.kind, TokKind::Close(b')') | TokKind::Close(b']'));
    if !(open && close) {
        return None;
    }
    year_of(word_at(s, toks, i + 1)?)
}

struct YearPick {
    idx: usize,
    end: usize,
    year: u16,
    bracketed: bool,
}

/// Picks the release year: the first bracketed year after the title (the
/// canonical `Title (Year)` form wins even over tags before it, e.g.
/// "Step Up 3D (2010)"), else the last plain year inside the title zone.
fn choose_year(s: &str, toks: &[Tok], zone_end: usize, anchor_start: usize) -> Option<YearPick> {
    for i in 1..anchor_start.min(toks.len()) {
        if let Some(year) = bracket_year(s, toks, i) {
            if toks[..i].iter().any(|t| t.is_word()) {
                return Some(YearPick {
                    idx: i,
                    end: i + 3,
                    year,
                    bracketed: true,
                });
            }
        }
    }
    let mut pick = None;
    // "示例1900": a number glued to a CJK word is part of the title, like
    // the sequel number in "示例2".
    let glued_cjk = |i: usize| {
        let glued = |a: &Tok, b: &Tok| a.end == b.start && a.is_word() && b.is_word();
        (glued(&toks[i - 1], &toks[i]) && has_cjk(toks[i - 1].text(s)))
            || toks
                .get(i + 1)
                .is_some_and(|n| glued(&toks[i], n) && has_cjk(n.text(s)))
    };
    for (i, t) in toks.iter().enumerate().take(zone_end).skip(1) {
        if let Some(year) = year_of(t.text(s)).filter(|_| t.is_word() && !glued_cjk(i)) {
            pick = Some(YearPick {
                idx: i,
                end: i + 1,
                year,
                bracketed: false,
            });
        }
    }
    pick
}

/// Narrows `lo..hi` by dropping dashes, unmatched brackets and opening
/// brackets from both ends.
fn trim_range(toks: &[Tok], mut lo: usize, mut hi: usize) -> (usize, usize) {
    loop {
        let before = (lo, hi);
        if hi > lo {
            let drop = match toks[hi - 1].kind {
                TokKind::Dash | TokKind::Open(_) => true,
                TokKind::Close(_) => !toks[lo..hi].iter().any(|t| matches!(t.kind, TokKind::Open(_))),
                TokKind::Word => false,
            };
            if drop {
                hi -= 1;
            }
        }
        if hi > lo {
            let drop = match toks[lo].kind {
                TokKind::Dash | TokKind::Close(_) => true,
                TokKind::Open(_) => !toks[lo..hi].iter().any(|t| matches!(t.kind, TokKind::Close(_))),
                TokKind::Word => false,
            };
            if drop {
                lo += 1;
            }
        }
        if (lo, hi) == before {
            return (lo, hi);
        }
    }
}

/// Turns title tokens back into display text.
///
/// Space-separated names keep the user's text verbatim (punctuation such as
/// "Mr. Robot", "Your Name.", "Léon - The Professional" survives), except
/// that dots joining words in a dotted part become spaces ("XY-01 示例2.Some.Film"
/// → "XY-01 示例2 Some Film"; see [`undot_mixed`]). Dotted scene names are
/// re-joined with spaces (tokens split only by a script change stay glued:
/// "示例4k"), and runs of single letters become acronyms ("S.W.A.T.",
/// "E.T."). All-lowercase titles are title-cased.
fn assemble(s: &str, toks: &[Tok]) -> String {
    let (Some(first), Some(last)) = (toks.first(), toks.last()) else {
        return String::new();
    };
    let raw = &s[first.start..last.end];
    let dotted = !raw.contains(' ') && raw.contains(['.', '_']);
    let text = if !dotted {
        collapse_ws(&undot_mixed(&raw.replace('_', " ")))
    } else {
        let mut out = String::with_capacity(raw.len());
        let mut prev_end = None;
        let mut i = 0;
        while i < toks.len() {
            let t = toks[i];
            let glued = prev_end == Some(t.start);
            prev_end = Some(t.end);
            match t.kind {
                TokKind::Word => {
                    // Single letters split by dots ("S.W.A.T"), never letters
                    // glued to other script ("X示例").
                    let mut run = 0;
                    while i + run < toks.len()
                        && toks[i + run].is_word()
                        && is_letter(toks[i + run].text(s))
                        && toks.get(i + run + 1).is_none_or(|n| n.start > toks[i + run].end)
                        && ((run == 0 && !glued) || (run > 0 && toks[i + run].start > toks[i + run - 1].end))
                    {
                        run += 1;
                    }
                    if !out.is_empty() && !glued && !out.ends_with('(') && !out.ends_with('[') {
                        out.push(' ');
                    }
                    if run >= 2 {
                        for t in &toks[i..i + run] {
                            out.extend(t.text(s).chars().take(1).flat_map(char::to_uppercase));
                            out.push('.');
                        }
                        prev_end = Some(toks[i + run - 1].end);
                        i += run;
                        continue;
                    }
                    out.push_str(t.text(s).trim_end_matches('.'));
                }
                TokKind::Open(c) => {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push(c as char);
                }
                TokKind::Close(c) => out.push(c as char),
                TokKind::Dash => out.push_str(" -"),
            }
            i += 1;
        }
        out
    };
    let text = trim_seps(&text).to_string();
    title_case_if_lower(&text).unwrap_or(text)
}

/// In a name that uses spaces, a part that still joins words with dots the
/// way scene names do ("示例2.Some.Film", "Some.Film.Name") gets spaces
/// instead: a part with CJK in it, or with two joining dots or more. Kept:
/// a single dot after a short word ("Vol.1", "Mr.Smith", "No.7"),
/// abbreviations ("S.W.A.T.", "E.T"), decimals ("Tron 2.0") and a dot that
/// ends a word ("Mr. Robot").
fn undot_mixed(text: &str) -> String {
    if !text.contains('.') {
        return text.to_string();
    }
    let undot = |chunk: &str| -> String {
        let letters = chunk.trim_end_matches('.').split('.').collect::<Vec<_>>();
        let abbreviation =
            letters.len() >= 2 && letters.iter().all(|p| p.chars().count() == 1 && is_letter(p));
        if abbreviation {
            return chunk.to_string();
        }
        let cs: Vec<char> = chunk.chars().collect();
        let joins: Vec<usize> = (1..cs.len().saturating_sub(1))
            .filter(|&k| {
                let (a, b) = (cs[k - 1], cs[k + 1]);
                cs[k] == '.'
                    && a.is_alphanumeric()
                    && b.is_alphanumeric()
                    && !(a.is_ascii_digit() && b.is_ascii_digit())
            })
            .collect();
        if !(has_cjk(chunk) || joins.len() >= 2) {
            return chunk.to_string();
        }
        cs.iter()
            .enumerate()
            .map(|(k, &c)| if joins.contains(&k) { ' ' } else { c })
            .collect()
    };
    text.split(' ').map(undot).collect::<Vec<_>>().join(" ")
}

fn is_letter(w: &str) -> bool {
    let mut chars = w.trim_end_matches('.').chars();
    matches!((chars.next(), chars.next()), (Some(c), None) if c.is_alphabetic() && !is_cjk(c))
}

/// When `lo..hi` is exactly one `[…]`/`(…)` group, its inside.
fn unwrap_bracket(toks: &[Tok], lo: usize, hi: usize) -> (usize, usize) {
    let wrapped = hi >= lo + 3
        && matches!(toks[lo].kind, TokKind::Open(_))
        && matches!(toks[hi - 1].kind, TokKind::Close(_))
        && !toks[lo + 1..hi - 1]
            .iter()
            .any(|t| matches!(t.kind, TokKind::Open(_) | TokKind::Close(_)));
    if wrapped {
        trim_range(toks, lo + 1, hi - 1)
    } else {
        (lo, hi)
    }
}

/// A title in two scripts as release groups write it, `示例剧.Some.Show`:
/// returns the token ranges of the Latin half (the title, which providers
/// know) and the CJK half (the original title).
///
/// Only that layout splits: a dotted name, CJK first, then a Latin half of
/// two words or more that are not all generic. Otherwise the Latin part is
/// as likely a subtitle of the CJK title ("示例动画 THE MOVIE", "示例 Final
/// Season", "示例动画 III") or the CJK part an arc name ("SOMEANIME 千年篇"),
/// and splitting would name, and group, different works after it. Numbers
/// ride along with their neighbour ("示例.Some.Film.3000"), and a word mixing
/// both scripts ("示ing例"), brackets, or a third part ("XY-01 示例2 Some
/// Film") leave the title whole too.
fn split_bilingual(s: &str, toks: &[Tok], lo: usize, hi: usize) -> Option<((usize, usize), (usize, usize))> {
    if hi <= lo {
        return None;
    }
    let raw = &s[toks[lo].start..toks[hi - 1].end];
    if !has_cjk(raw) || raw.contains(' ') {
        return None;
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Script {
        Cjk,
        Latin,
        Neutral,
    }
    // (script, first token, end token) per part.
    let mut parts: Vec<(Script, usize, usize)> = Vec::new();
    let mut i = lo;
    while i < hi {
        match toks[i].kind {
            TokKind::Open(_) | TokKind::Close(_) => return None,
            TokKind::Dash => {
                i += 1;
                continue;
            }
            TokKind::Word => {}
        }
        // A chunk: words glued together (split only by a script change).
        let mut j = i + 1;
        while j < hi && toks[j].is_word() && toks[j].start == toks[j - 1].end {
            j += 1;
        }
        let text = &s[toks[i].start..toks[j - 1].end];
        let script = match (has_cjk(text), text.bytes().any(|b| b.is_ascii_alphabetic())) {
            (true, true) => return None,
            (true, false) => Script::Cjk,
            (false, true) => Script::Latin,
            (false, false) => Script::Neutral,
        };
        match parts.last_mut() {
            Some(last) if last.0 == script || script == Script::Neutral => last.2 = j,
            Some(last) if last.0 == Script::Neutral => *last = (script, last.1, j),
            _ => parts.push((script, i, j)),
        }
        i = j;
    }
    let [cjk, latin] = parts.as_slice() else {
        return None;
    };
    if (cjk.0, latin.0) != (Script::Cjk, Script::Latin) {
        return None;
    }
    let words: Vec<&str> = (latin.1..latin.2)
        .filter(|&k| toks[k].is_word())
        .map(|k| toks[k].text(s))
        .collect();
    let real_word = words
        .iter()
        .any(|w| w.bytes().filter(u8::is_ascii_alphabetic).count() >= 2);
    (words.len() >= 2 && real_word && !all_generic(&words)).then_some(((latin.1, latin.2), (cjk.1, cjk.2)))
}

/// Words that name what a work is rather than which one: a Latin "title"
/// made only of them ("THE MOVIE", "Final Season", "III", "Part 2") is a
/// subtitle of the other-script title, never a title of its own.
fn all_generic(words: &[&str]) -> bool {
    const GENERIC: &str = "the a an of movie film final season series part chapter special specials \
        ova oad ona tv edition cut version complete collection arc saga vol volume episode ep sp new \
        end fin gekijouban theatrical i ii iii iv v vi vii viii ix x";
    words.iter().all(|w| {
        let w = w
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_ascii_lowercase();
        w.is_empty()
            || w.bytes().all(|b| b.is_ascii_digit())
            || GENERIC.split_ascii_whitespace().any(|g| g == w)
    })
}

/// A title that is itself a bracket group or nothing but release tags
/// ("[Some Studio]", "中英字幕"): the parser picked the wrong part of the name.
pub(crate) fn title_looks_like_tags(title: &str) -> bool {
    let t = title.trim();
    if (t.starts_with('[') && t.ends_with(']')) || (t.starts_with('【') && t.ends_with('】')) {
        return true;
    }
    let mut words = t.split_whitespace().peekable();
    words.peek().is_some() && words.all(|w| vocab::classify(w).is_some_and(|g| g.strong))
}

/// What one bracket group of an all-bracket name holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    /// A title (or a fansub group named without a telltale word).
    Title,
    /// A fansub group: `…Sub(s)`, `…Raws`, `…Studio`, `…Team`, `某某字幕组`.
    Fansub,
    /// Release tags (`BDRip_960P`, `1080P`, `简体内嵌`).
    Tags,
    Year,
    /// An episode number or marker (`01`, `E01`, `S01E01`, `第01集`).
    Episode,
    /// A sharing site's address or name: dropped.
    Site,
}

fn group_kind(g: &str) -> Group {
    let g = g.trim();
    if year_of(g).is_some() {
        return Group::Year;
    }
    if NUM_RANGE_RE.is_match(g)
        || SXE_RE.is_match(g)
        || E_RE.is_match(g)
        || EP_RE.is_match(g)
        || cjk_marker(g).is_some()
    {
        return Group::Episode;
    }
    if is_site(g) {
        return Group::Site;
    }
    let cjk_fansub = [
        "字幕组",
        "字幕組",
        "字幕社",
        "压制组",
        "壓制組",
        "汉化组",
        "漢化組",
    ]
    .iter()
    .any(|x| g.contains(x));
    if cjk_fansub || vocab::is_cjk_group_name(g) {
        return Group::Fansub;
    }
    let toks = tokenize(g);
    let words: Vec<&str> = toks.iter().filter(|t| t.is_word()).map(|t| t.text(g)).collect();
    let tags: Vec<_> = words.iter().map(|w| vocab::classify(w)).collect();
    let strong = tags.iter().filter(|t| t.is_some_and(|t| t.strong)).count();
    // "[GB]", "[BIG5]", "[CHS&JPN]", "[GB_MP4]": fansub notes, as a whole group.
    let bracket_tag = |w: &str| {
        w.split(|c: char| !c.is_alphanumeric())
            .filter(|p| !p.is_empty())
            .all(|p| vocab::classify(p).is_some() || vocab::is_bracket_tag(p))
    };
    if words.is_empty()
        || tags.iter().all(Option::is_some)
        || strong * 2 >= words.len()
        || words.iter().all(|w| bracket_tag(w))
    {
        return Group::Tags;
    }
    let last = words.last().map(|w| w.to_ascii_lowercase()).unwrap_or_default();
    let fansub = ["sub", "subs", "raws", "fansub", "fansubs"]
        .iter()
        .any(|x| last.ends_with(x))
        || matches!(last.as_str(), "studio" | "studios" | "team" | "group" | "raw");
    if fansub {
        Group::Fansub
    } else {
        Group::Title
    }
}

/// Fills the episode fields from an episode bracket group: `[第01集]`,
/// `[S01E01]`, `[E01]`, or a bare `[01]` (anime absolute numbering).
fn episode_group(out: &mut Stem, g: &str) {
    if let Some((season, n)) = cjk_marker(g) {
        if season {
            out.season = Some(n);
            out.marker = Some(Marker::SeasonOnly);
        } else {
            out.episodes = vec![n];
            out.marker = Some(Marker::CjkEpisode);
        }
    } else if let Some(c) = SXE_RE.captures(g) {
        if let (Ok(season), Ok(first)) = (c[1].parse(), c[2].parse()) {
            out.season = Some(season);
            out.episodes = episode_list(first, c.get(3).map_or("", |m| m.as_str()));
            out.marker = Some(Marker::SxxEyy);
        }
    } else if let Some(c) = E_RE.captures(g).or_else(|| EP_RE.captures(g)) {
        if let Ok(first) = c[1].parse() {
            out.episodes = episode_list(first, c.get(2).map_or("", |m| m.as_str()));
            out.marker = Some(Marker::EpisodeOnly);
        }
    } else if let Some(c) = NUM_RANGE_RE.captures(g) {
        let mut a = Anchor::new(0, 0, Marker::Absolute);
        if fill_absolute(&mut a, &c).is_some() {
            out.absolute = a.absolute;
            out.episodes = a.episodes;
            out.marker = Some(Marker::Absolute);
        }
    }
}

/// A name made only of bracket groups (`[A][B][C]`, separators between them
/// allowed): its groups' insides.
fn bracket_groups(s: &str) -> Option<Vec<&str>> {
    let mut rest = s.trim();
    let mut out = Vec::new();
    while !rest.is_empty() {
        let (inner_rest, close) = match rest.strip_prefix('[') {
            Some(r) => (r, ']'),
            None => (rest.strip_prefix('【')?, '】'),
        };
        let end = inner_rest.find(close)?;
        out.push(inner_rest[..end].trim());
        rest = inner_rest[end + close.len_utf8()..].trim_start_matches([' ', '.', '_', '-']);
    }
    (out.len() >= 2).then_some(out)
}

/// Reads a name made only of bracket groups, the usual form of Chinese
/// fansub releases: `[Group][Title][01][1080P]`, `[中文名][Group][English
/// Name][BDRip]`. Each group is classified ([`group_kind`]).
///
/// - The fansub group is the group that says so ("Sub", "字幕组", …), else,
///   by convention, the first group when another title group follows
///   (`[某组][示例动画][Some Anime]`, `[Group][Some Anime][01]`). The
///   exception is `[中文名][Group][English Name]`: a CJK title first, then
///   two Latin groups; the middle one is the group.
/// - The title is the Latin title group right after a CJK one (only the
///   fansub group or a site may sit between them, and no episode, year or
///   tag group), with the CJK one as the original title (bilingual), else
///   the first title group. A Latin group of generic words ("Movie",
///   "Final Season") is a subtitle, never the title.
///
/// The title, year and tag groups are then parsed as one ordinary name
/// (`Title (Year) [tags]`, as a folder name, so a number in the title stays
/// a title word) and the first episode group supplies the episode
/// (`[第二期][05]`: a 期/季 group is the season when another group numbers
/// the episode).
fn bracket_name(s: &str, opts: StemOpts) -> Option<Stem> {
    let groups = bracket_groups(s)?;
    let kinds: Vec<Group> = groups.iter().map(|g| group_kind(g)).collect();
    let titles: Vec<usize> = (0..groups.len()).filter(|&i| kinds[i] == Group::Title).collect();
    if titles.is_empty() {
        return None;
    }
    let cjk = |i: usize| has_cjk(groups[i]);
    let latin = |i: usize| {
        let words: Vec<&str> = groups[i]
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        !cjk(i)
            && words
                .iter()
                .any(|w| w.bytes().filter(u8::is_ascii_alphabetic).count() >= 2)
            && !all_generic(&words)
    };
    let mut fansub = kinds.iter().position(|k| *k == Group::Fansub);
    if fansub.is_none() && titles.len() >= 2 && titles[0] == 0 {
        let cjk_group_latin = cjk(0) && titles.len() >= 3 && !cjk(titles[1]) && latin(titles[2]);
        fansub = Some(if cjk_group_latin { titles[1] } else { 0 });
    }
    let titles: Vec<usize> = titles.into_iter().filter(|&i| Some(i) != fansub).collect();
    let structural = |i: usize| matches!(kinds[i], Group::Episode | Group::Year | Group::Tags);
    let pair = titles.iter().copied().find(|&c| cjk(c)).and_then(|c| {
        let l = (c + 1..groups.len())
            .take_while(|&i| !structural(i))
            .find(|&i| kinds[i] == Group::Title && Some(i) != fansub)?;
        latin(l).then_some((c, l))
    });
    let (title_i, original_i) = match pair {
        Some((c, l)) => (l, Some(c)),
        None => (*titles.first()?, None),
    };
    let eps: Vec<usize> = (0..groups.len())
        .filter(|&i| kinds[i] == Group::Episode)
        .collect();
    let season_group = |i: usize| matches!(cjk_marker_unit(groups[i]), Some(('季' | '期', _)));
    let episode_i = eps.iter().copied().find(|&i| !season_group(i));
    let mut text = groups[title_i].to_string();
    for (i, k) in kinds.iter().enumerate() {
        match k {
            Group::Year => text.push_str(&format!(" ({})", groups[i])),
            Group::Tags => text.push_str(&format!(" [{}]", groups[i])),
            _ => {}
        }
    }
    let folder = StemOpts {
        folder: true,
        period_season: opts.period_season || episode_i.is_some(),
        ..opts
    };
    let mut out = parse_core(&text, folder, None);
    if let Some(i) = fansub {
        out.release_group = Some(groups[i].to_string());
    }
    if let Some(i) = episode_i.or(eps.first().copied()) {
        episode_group(&mut out, groups[i]);
    }
    if episode_i.is_some() {
        if let Some(n) = eps
            .iter()
            .filter(|&&i| season_group(i))
            .find_map(|&i| cjk_marker_unit(groups[i]).map(|(_, n)| n))
        {
            out.season.get_or_insert(n);
        }
    }
    if out.season.is_some() && out.marker == Some(Marker::Absolute) {
        // "[示例动画 第二季][05]": a number after a season marker counts
        // within that season.
        if let Some(abs) = out.absolute.take() {
            if out.episodes.is_empty() {
                out.episodes = u16::try_from(abs).ok().into_iter().collect();
            }
        }
        out.marker = Some(Marker::Words);
    }
    if let Some(i) = original_i {
        let original = parse_core(groups[i], folder, None).title;
        if let (Some(o), Some(t)) = (&original, &out.title) {
            out.full_title = Some(format!("{o} {t}"));
        }
        out.original_title = original.or(out.original_title);
    }
    out.bracket_layout = true;
    Some(out)
}

/// Reads resolution, source, codec, edition and part from the technical
/// tail, skipping the `skip` ranges (anchor, episode title, year).
fn scan_tail(s: &str, toks: &[Tok], from: usize, skip: &[Option<(usize, usize)>], out: &mut Stem) {
    let n = toks.len();
    let mut i = from;
    while i < n {
        if let Some((_, b)) = skip.iter().flatten().find(|(a, b)| (*a..*b).contains(&i)) {
            i = *b;
            continue;
        }
        let t = toks[i];
        if !t.is_word() {
            i += 1;
            continue;
        }
        if is_h_codec(s, toks, i) {
            out.tags += 1;
            out.video_codec.get_or_insert_with(|| {
                if word_at(s, toks, i + 1).is_some_and(|w| w.starts_with("265")) {
                    "HEVC"
                } else {
                    "H.264"
                }
                .to_string()
            });
            i += 2;
            continue;
        }
        let words: Vec<&str> = toks[i..]
            .iter()
            .filter(|t| t.is_word())
            .take(2)
            .map(|t| t.text(s))
            .collect();
        if let Some((ed, used)) = vocab::edition_at(&words) {
            out.edition.get_or_insert_with(|| ed.to_string());
            out.tags += 1;
            i = advance_words(toks, i, used);
            continue;
        }
        if let Some((p, used)) = vocab::part_at(&words, false) {
            out.part.get_or_insert(p);
            i = advance_words(toks, i, used);
            continue;
        }
        let w = t.text(s);
        if vocab::classify(w).is_some() {
            out.tags += 1;
            // "1080P.某某字幕组", "x264-某某影视": a CJK fansub or site name.
            let last = w.rsplit('-').next().unwrap_or(w);
            if out.release_group.is_none() && vocab::is_cjk_group_name(last) {
                out.release_group = Some(last.to_string());
            }
            // "WEBDL-2160p" and "x264-GROUP" carry several tags; "WEB-DL" is one.
            let parts: Vec<&str> = if vocab::is_hyphenated_tag(w) {
                vec![w]
            } else {
                w.split('-').filter(|p| !p.is_empty()).collect()
            };
            for p in parts {
                match vocab::classify(p).map(|t| t.kind) {
                    Some(TagKind::Resolution) if out.resolution.is_none() => {
                        out.resolution = vocab::canonical_resolution(p);
                    }
                    Some(TagKind::Source) => {
                        // Prefer the real source over "Remux" ("UHD BluRay REMUX").
                        if out.source.is_none() || out.source.as_deref() == Some("Remux") {
                            if let Some(src) = vocab::canonical_source(p) {
                                out.source = Some(src.to_string());
                            }
                        }
                    }
                    Some(TagKind::VideoCodec) if out.video_codec.is_none() => {
                        out.video_codec = vocab::canonical_codec(p).map(str::to_string);
                    }
                    _ => {}
                }
            }
        }
        i += 1;
    }
}

/// Moves past `used` word tokens starting at `i`.
fn advance_words(toks: &[Tok], i: usize, used: usize) -> usize {
    let mut seen = 0;
    let mut j = i;
    while j < toks.len() && seen < used {
        if toks[j].is_word() {
            seen += 1;
        }
        j += 1;
    }
    j
}

/// Scene release group: `x264-GROUP`, `H.265-FLUX`, `[h265]-FLUX`.
fn group_from_last_word(s: &str, toks: &[Tok], tail_start: usize) -> Option<String> {
    let idx = toks.iter().rposition(|t| t.is_word())?;
    if idx < tail_start.max(1) {
        return None;
    }
    let w = toks[idx].text(s);
    let valid =
        |g: &str| !g.is_empty() && g.len() <= 30 && g.chars().all(|c| c.is_alphanumeric() || c == '_');
    if let Some(g) = w.strip_prefix('-') {
        return valid(g).then(|| g.to_string());
    }
    if vocab::is_hyphenated_tag(w) {
        return None;
    }
    let (left, right) = w.rsplit_once('-')?;
    let left_is_tag = vocab::classify(left).is_some() || left.bytes().all(|b| b.is_ascii_digit());
    let right_ok = vocab::classify(right).is_none() || vocab::is_cjk_group_name(right);
    (left_is_tag && valid(right) && right_ok).then(|| right.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Stem {
        parse_stem(s, StemOpts::default())
    }

    #[test]
    fn scene_movie() {
        let r = p("The.Matrix.1999.1080p.BluRay.x264-GROUP");
        assert_eq!(r.title.as_deref(), Some("The Matrix"));
        assert_eq!(r.year, Some(1999));
        assert_eq!(r.resolution.as_deref(), Some("1080p"));
        assert_eq!(r.source.as_deref(), Some("BluRay"));
        assert_eq!(r.video_codec.as_deref(), Some("H.264"));
        assert_eq!(r.release_group.as_deref(), Some("GROUP"));
    }

    #[test]
    fn year_like_titles() {
        let r = p("1917.2019.2160p.WEB-DL.DDP5.1.Atmos.HDR.HEVC-EVO");
        assert_eq!((r.title.as_deref(), r.year), (Some("1917"), Some(2019)));
        let r = p("2001.A.Space.Odyssey.1968.REMASTERED.1080p");
        assert_eq!(
            (r.title.as_deref(), r.year),
            (Some("2001 A Space Odyssey"), Some(1968))
        );
        assert_eq!(r.edition.as_deref(), Some("Remastered"));
        assert_eq!(r.year_like_in_title, Some(2001));
        let r = p("Blade Runner 2049 (2017) 2160p UHD BluRay REMUX HDR HEVC Atmos-FGT");
        assert_eq!(
            (r.title.as_deref(), r.year),
            (Some("Blade Runner 2049"), Some(2017))
        );
        assert!(r.episodes.is_empty() && r.season.is_none());
        assert_eq!(r.release_group.as_deref(), Some("FGT"));
        let r = p("2012");
        assert_eq!((r.title.as_deref(), r.year), (Some("2012"), None));
        let r = p("Blade.Runner.2049");
        assert_eq!((r.title.as_deref(), r.year), (Some("Blade Runner 2049"), None));
    }

    #[test]
    fn episodes() {
        let r = p("Game.of.Thrones.S08E01-E02.2160p");
        assert_eq!((r.season, r.episodes.clone()), (Some(8), vec![1, 2]));
        let r = p("The.Office.US.S02E01E02.720p.WEB-DL");
        assert_eq!(r.title.as_deref(), Some("The Office US"));
        assert_eq!(r.episodes, vec![1, 2]);
        let r = p("Show - S01E01-E03 - Title");
        assert_eq!(r.episodes, vec![1, 2, 3]);
        let r = p("Friends - 1x01 - The One Where Monica Gets a Roommate");
        assert_eq!(
            (r.title.as_deref(), r.season, r.episodes.clone()),
            (Some("Friends"), Some(1), vec![1])
        );
        assert_eq!(
            r.episode_title.as_deref(),
            Some("The One Where Monica Gets a Roommate")
        );
        let r = p("Show.Name.Season.1.Episode.3");
        assert_eq!(
            (r.title.as_deref(), r.season, r.episodes.clone()),
            (Some("Show Name"), Some(1), vec![3])
        );
        let r = p("S.W.A.T.2017.S05E01.720p.HDTV.x264");
        assert_eq!(
            (r.title.as_deref(), r.year, r.season),
            (Some("S.W.A.T."), Some(2017), Some(5))
        );
        let r = p("Breaking.Bad.S05E14.Ozymandias.1080p.BluRay.x264-ROVERS");
        assert_eq!(r.episode_title.as_deref(), Some("Ozymandias"));
    }

    #[test]
    fn dates_and_anime() {
        let r = p("The.Daily.Show.2024.03.15.Guest.Name.720p.WEB.h264");
        assert_eq!(r.title.as_deref(), Some("The Daily Show"));
        assert_eq!(r.air_date.as_deref(), Some("2024-03-15"));
        assert_eq!((r.year, r.episodes.len()), (None, 0));
        let r = p("[SubsPlease] Sousou no Frieren - 12 (1080p) [ABCDEF12]");
        assert_eq!(
            (r.title.as_deref(), r.absolute),
            (Some("Sousou no Frieren"), Some(12))
        );
        assert_eq!(r.release_group.as_deref(), Some("SubsPlease"));
        let r = p("[Erai-raws] One Piece - 1100 [1080p][Multiple Subtitle]");
        assert_eq!((r.title.as_deref(), r.absolute), (Some("One Piece"), Some(1100)));
    }

    #[test]
    fn canonical_forms() {
        let r = p("Alien (1979) - Director's Cut [Bluray-1080p]");
        assert_eq!((r.title.as_deref(), r.year), (Some("Alien"), Some(1979)));
        assert_eq!(r.edition.as_deref(), Some("Director's Cut"));
        assert_eq!(r.resolution.as_deref(), Some("1080p"));
        let r = p("Burning Orchard (1972) - 1080p");
        assert_eq!(
            (r.title.as_deref(), r.edition.as_deref()),
            (Some("Burning Orchard"), None)
        );
        let r = p("Electric Signal 2 (1952) - cd2");
        assert_eq!((r.title.as_deref(), r.part), (Some("Electric Signal 2"), Some(2)));
        let r = p("Léon - The Professional (1994)");
        assert_eq!(r.title.as_deref(), Some("Léon - The Professional"));
        let r = p("Your Name. (2016)");
        assert_eq!(r.title.as_deref(), Some("Your Name."));
    }

    #[test]
    fn part_words_stay_in_title() {
        let r = p("Dune.Part.Two.2024.2160p.WEB-DL");
        assert_eq!((r.title.as_deref(), r.part), (Some("Dune Part Two"), None));
        let r = p("Kill.Bill.Vol.1.2003.1080p.BluRay.x264.cd1");
        assert_eq!((r.title.as_deref(), r.part), (Some("Kill Bill Vol 1"), Some(1)));
        let r = p("Harry Potter and the Deathly Hallows Part 1 (2010)");
        assert_eq!(
            r.title.as_deref(),
            Some("Harry Potter and the Deathly Hallows Part 1")
        );
        let r = p("Some Movie cd2");
        assert_eq!((r.title.as_deref(), r.part), (Some("Some Movie"), Some(2)));
    }

    #[test]
    fn weak_tags_keep_titles() {
        let r = p("Charlottes.Web.1973.1080p");
        assert_eq!(r.title.as_deref(), Some("Charlottes Web"));
        let r = p("Mr.Hollands.Opus.1995");
        assert_eq!(r.title.as_deref(), Some("Mr Hollands Opus"));
        let r = p("Movie.Name.FRENCH.1080p");
        assert_eq!(r.title.as_deref(), Some("Movie Name"));
    }

    #[test]
    fn chinese_numerals() {
        for (w, n) in [
            ("12", 12),
            ("１２", 12),
            ("十", 10),
            ("十二", 12),
            ("二十", 20),
            ("二十三", 23),
            ("一百零五", 105),
            ("两", 2),
            ("〇七", 7),
        ] {
            assert_eq!(cjk_number(w), Some(n), "{w}");
        }
        assert_eq!(cjk_number("十a"), None);
        assert_eq!(cjk_marker("第十二集"), Some((false, 12)));
        assert_eq!(cjk_marker("第2季"), Some((true, 2)));
        assert_eq!(cjk_marker("第一滴血"), None);
    }

    #[test]
    fn cjk_titles() {
        let r = p("示例剧_第03集");
        assert_eq!(
            (r.title.as_deref(), r.episodes.clone()),
            (Some("示例剧"), vec![3])
        );
        assert_eq!(r.marker, Some(Marker::CjkEpisode));
        let r = p("示例剧.Some.Show.S01E01.中英字幕.BDrip.1080p-某某影视");
        assert_eq!(r.title.as_deref(), Some("Some Show"));
        assert_eq!(r.original_title.as_deref(), Some("示例剧"));
        assert_eq!(r.full_title.as_deref(), Some("示例剧 Some Show"));
        assert_eq!(
            (r.episode_title, r.release_group.as_deref()),
            (None, Some("某某影视"))
        );
        let r = p("示例片4k");
        assert_eq!(
            (r.title.as_deref(), r.resolution.as_deref()),
            (Some("示例片"), Some("2160p"))
        );
        let r = p("[示例片.副题][Some.Studio][Some.Film][BDRip_960P]");
        assert!(r.bracket_layout);
        assert_eq!(r.title.as_deref(), Some("Some Film"));
        assert_eq!(r.original_title.as_deref(), Some("示例片 副题"));
    }

    #[test]
    fn mixed_dots_become_spaces_per_part() {
        assert_eq!(undot_mixed("XY-01 示例2.Some.Film.2"), "XY-01 示例2 Some Film 2");
        assert_eq!(
            undot_mixed("Agents of S.H.I.E.L.D. Now"),
            "Agents of S.H.I.E.L.D. Now"
        );
        assert_eq!(undot_mixed("Tron 2.0 Legacy"), "Tron 2.0 Legacy");
        assert_eq!(undot_mixed("Mr. Robot"), "Mr. Robot");
        assert!(title_looks_like_tags("[Some Studio]"));
        assert!(title_looks_like_tags("中英字幕 1080p"));
        assert!(!title_looks_like_tags("Some Film"));
    }

    #[test]
    fn lowercase_titles_are_cased() {
        let r = p("the.lord.of.the.rings.2001");
        assert_eq!(r.title.as_deref(), Some("The Lord of the Rings"));
    }
}
