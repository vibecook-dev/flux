//! Evaluation harness: scores a parser against the synthetic library ground
//! truth (`tools/corpus/gen-library.py` manifests) and the hard cases.
//!
//! Used by the `names-eval` binary and by the integration tests.
//!
//! Manifest paths start with `Movies/` (a movie library) or `TV/` (a shows
//! library); the parser sees the path relative to that root, the library
//! kind, and the names of the other files in the same directory.
//!
//! Scoring notes:
//! - Titles compare as lowercase alphanumerics ([`norm_key`]), because the
//!   generator writes `:` as ` -` and drops punctuation in dotted scene names.
//! - Episodes: `season+episodes` must match exactly. For anime files named
//!   with absolute numbers (`[Group] Show - 035`, no season folder) the
//!   path holds no season, so the correct answer is `absolute_episode`;
//!   mapping absolute → SxxEyy needs provider data (research/05 §3.4) and is
//!   reported separately as `anime_season_mapped`.
//! - Episode `year` is scored only when the year appears in the path (the
//!   generator records the show's year even for `TV/Show/…` folders).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::hard_cases::{CaseResult, HardCase, CASES};
use crate::{is_ignored, is_video_file, norm_key, GuessKind, LibraryKind, NameGuess};

/// One line of a `*.manifest.jsonl` ground-truth file.
#[derive(Debug, Clone, Deserialize)]
pub struct ManifestRow {
    pub path: String,
    /// movie | episode | sample | extra | nfo | subtitle | artwork | junk
    pub kind: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub show: Option<String>,
    #[serde(default)]
    pub parent_title: Option<String>,
    #[serde(default)]
    pub year: Option<u16>,
    #[serde(default)]
    pub season: Option<u16>,
    #[serde(default)]
    pub episodes: Option<Vec<u16>>,
    #[serde(default)]
    pub absolute: Option<u32>,
    #[serde(default)]
    pub edition: Option<String>,
    #[serde(default)]
    pub part: Option<u8>,
    #[serde(default)]
    pub tmdb_id: Option<u64>,
    #[serde(default)]
    pub resolution: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
}

/// Errors loading a manifest.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("reading manifest: {0}")]
    Io(#[from] std::io::Error),
    #[error("manifest line {line}: {err}")]
    Json { line: usize, err: serde_json::Error },
}

/// Loads a JSON-lines manifest.
pub fn load_manifest(path: &Path) -> Result<Vec<ManifestRow>, ManifestError> {
    let text = std::fs::read_to_string(path)?;
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| serde_json::from_str(l).map_err(|err| ManifestError::Json { line: i + 1, err }))
        .collect()
}

/// What the parser is called with for one manifest row.
#[derive(Debug, Clone)]
pub struct Input {
    /// Index into the manifest rows.
    pub row: usize,
    /// Path relative to the library root (`Movies/` or `TV/` stripped).
    pub rel: PathBuf,
    pub kind: LibraryKind,
    /// Other file names in the same directory.
    pub siblings: Vec<String>,
}

impl Input {
    /// Siblings as `&str`s, the shape [`crate::parse`] takes.
    pub fn sibling_refs(&self) -> Vec<&str> {
        self.siblings.iter().map(String::as_str).collect()
    }
}

/// Media kinds the parser is scored on.
const MEDIA_KINDS: &[&str] = &["movie", "episode", "sample", "extra"];

/// Builds parser inputs for every media row (movies, episodes, samples,
/// extras), with siblings taken from the manifest's directory listing.
pub fn inputs(rows: &[ManifestRow]) -> Vec<Input> {
    let mut by_dir: HashMap<&str, Vec<&str>> = HashMap::new();
    for r in rows {
        let (dir, name) = r.path.rsplit_once('/').unwrap_or(("", r.path.as_str()));
        by_dir.entry(dir).or_default().push(name);
    }
    rows.iter()
        .enumerate()
        .filter(|(_, r)| MEDIA_KINDS.contains(&r.kind.as_str()))
        .filter_map(|(i, r)| {
            let (root, rest) = r.path.split_once('/')?;
            let kind = match root {
                "Movies" => LibraryKind::Movies,
                "TV" => LibraryKind::Shows,
                _ => LibraryKind::Mixed,
            };
            let (dir, name) = r.path.rsplit_once('/').unwrap_or(("", r.path.as_str()));
            let siblings = by_dir
                .get(dir)
                .map(|v| {
                    v.iter()
                        .filter(|s| **s != name)
                        .map(|s| (*s).to_string())
                        .collect()
                })
                .unwrap_or_default();
            Some(Input {
                row: i,
                rel: PathBuf::from(rest),
                kind,
                siblings,
            })
        })
        .collect()
}

/// A hit/total counter.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Counter {
    pub ok: u64,
    pub n: u64,
}

impl Counter {
    fn add(&mut self, ok: bool) {
        self.n += 1;
        self.ok += u64::from(ok);
    }
    /// Accuracy in percent (100 when empty).
    pub fn pct(&self) -> f64 {
        if self.n == 0 {
            100.0
        } else {
            self.ok as f64 * 100.0 / self.n as f64
        }
    }
}

/// One failed field, kept as an example of its failure class.
#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    pub path: String,
    pub expected: String,
    pub got: String,
    pub confidence: f32,
    pub evidence: Vec<String>,
}

/// Result of scoring one parser over one manifest.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Evaluation {
    /// kind → field → counter.
    pub per_kind: BTreeMap<String, BTreeMap<String, Counter>>,
    /// "kind/style" → `all` counter (style = which generator layout).
    pub per_style: BTreeMap<String, Counter>,
    /// "kind/style/field" → number of failures.
    pub failure_counts: BTreeMap<String, u64>,
    /// "kind/style/field" → up to N examples.
    pub failures: BTreeMap<String, Vec<Failure>>,
    /// Non-media files (nfo, subtitles, artwork, junk) a scanner must skip:
    /// `is_ignored(path) || !is_video_file(name)`.
    pub non_media_skipped: Counter,
    /// Guesses where the fallback parser ran.
    pub fallback_used: u64,
    /// Guesses with confidence < 0.5 (would be queued for AI matching), and
    /// how many of those were actually wrong.
    pub low_confidence: Counter,
    /// Wrong guesses that still had confidence ≥ 0.5 (silent errors).
    pub confident_but_wrong: u64,
    pub media_files: u64,
}

/// Scores `parse` over `rows`, keeping up to `max_examples` per failure class.
pub fn evaluate(
    rows: &[ManifestRow],
    mut parse: impl FnMut(&Input) -> NameGuess,
    max_examples: usize,
) -> Evaluation {
    let mut ev = Evaluation::default();
    for r in rows.iter().filter(|r| !MEDIA_KINDS.contains(&r.kind.as_str())) {
        let name = r.path.rsplit('/').next().unwrap_or("");
        ev.non_media_skipped
            .add(is_ignored(Path::new(&r.path)) || !is_video_file(name));
    }
    for input in inputs(rows) {
        let row = &rows[input.row];
        let g = parse(&input);
        ev.media_files += 1;
        if g.evidence.iter().any(|e| e.starts_with("fallback")) {
            ev.fallback_used += 1;
        }
        let style = style_of(row);
        let checks = checks(row, &g);
        let all = checks.iter().filter(|c| c.gating).all(|c| c.ok);
        let kind_map = ev.per_kind.entry(row.kind.clone()).or_default();
        for c in &checks {
            kind_map.entry(c.field.to_string()).or_default().add(c.ok);
        }
        kind_map.entry("all".into()).or_default().add(all);
        ev.per_style
            .entry(format!("{}/{style}", row.kind))
            .or_default()
            .add(all);
        if g.confidence < 0.5 {
            ev.low_confidence.add(!all);
        } else if !all {
            ev.confident_but_wrong += 1;
        }
        for c in checks.iter().filter(|c| !c.ok) {
            let class = format!("{}/{style}/{}", row.kind, c.field);
            *ev.failure_counts.entry(class.clone()).or_default() += 1;
            let ex = ev.failures.entry(class).or_default();
            if ex.len() < max_examples {
                ex.push(Failure {
                    path: row.path.clone(),
                    expected: c.expected.clone(),
                    got: c.got.clone(),
                    confidence: g.confidence,
                    evidence: g.evidence.clone(),
                });
            }
        }
    }
    ev
}

struct Check {
    field: &'static str,
    ok: bool,
    /// Counts toward the row's `all` verdict.
    gating: bool,
    expected: String,
    got: String,
}

fn check<T: PartialEq + std::fmt::Debug>(field: &'static str, expected: T, got: T, gating: bool) -> Check {
    Check {
        field,
        ok: expected == got,
        gating,
        expected: format!("{expected:?}"),
        got: format!("{got:?}"),
    }
}

fn title_check(field: &'static str, expected: Option<&str>, got: Option<&str>) -> Check {
    Check {
        field,
        ok: expected.map(norm_key) == got.map(norm_key),
        gating: true,
        expected: format!("{expected:?}"),
        got: format!("{got:?}"),
    }
}

fn checks(row: &ManifestRow, g: &NameGuess) -> Vec<Check> {
    let mut v = Vec::new();
    match row.kind.as_str() {
        "movie" => {
            v.push(check("kind", GuessKind::Movie, g.kind, true));
            let t = title_check("title", row.title.as_deref(), g.title.as_deref());
            let y = check("year", row.year, g.year, true);
            v.push(Check {
                field: "title+year",
                ok: t.ok && y.ok,
                gating: false,
                expected: format!("{} ({})", t.expected, y.expected),
                got: format!("{} ({})", t.got, y.got),
            });
            v.push(t);
            v.push(y);
            v.push(check(
                "edition",
                row.edition.as_deref().map(norm_key),
                g.edition.as_deref().map(norm_key),
                true,
            ));
            v.push(check("part", row.part, g.part, true));
            v.push(check("tmdb_id", row.tmdb_id, g.ids.tmdb, true));
            if let Some(res) = &row.resolution {
                v.push(check(
                    "resolution",
                    Some(res.as_str()),
                    g.resolution.as_deref(),
                    false,
                ));
            }
            if let Some(grp) = &row.group {
                v.push(check(
                    "release_group",
                    Some(grp.as_str()),
                    g.release_group.as_deref(),
                    false,
                ));
            }
        }
        "episode" => {
            v.push(check("kind", GuessKind::Episode, g.kind, true));
            v.push(title_check("title", row.show.as_deref(), g.title.as_deref()));
            let eps = row.episodes.clone().unwrap_or_default();
            match row.absolute {
                // No season folder or SxxEyy in the path: the absolute number
                // is all a filename can carry.
                Some(abs) => {
                    v.push(check("season+episodes", Some(abs), g.absolute_episode, true));
                    v.push(check(
                        "anime_season_mapped",
                        (row.season, eps),
                        (g.season, g.episodes.clone()),
                        false,
                    ));
                }
                None => {
                    v.push(check(
                        "season+episodes",
                        (row.season, eps),
                        (g.season, g.episodes.clone()),
                        true,
                    ));
                }
            }
            if let Some(y) = row.year.filter(|y| row.path.contains(&y.to_string())) {
                v.push(check("year_in_path", Some(y), g.year, true));
            }
        }
        "sample" => v.push(check("kind", GuessKind::Sample, g.kind, true)),
        "extra" => {
            v.push(check("kind", GuessKind::Extra, g.kind, true));
            v.push(title_check(
                "title",
                row.parent_title.as_deref(),
                g.title.as_deref(),
            ));
            v.push(check("year", row.year, g.year, true));
        }
        _ => {}
    }
    v
}

/// Which generator layout produced a row (for the per-style breakdown).
pub fn style_of(row: &ManifestRow) -> &'static str {
    let p = row.path.as_str();
    let file = p.rsplit('/').next().unwrap_or(p);
    let depth = p.matches('/').count();
    match row.kind.as_str() {
        "movie" => {
            if p.contains("[tmdbid-") {
                "tmdbid"
            } else if p.contains("{edition-") {
                "edition"
            } else if file.contains(" - cd") {
                "multipart"
            } else if [" - 2160p.", " - 1080p.", " - 720p."]
                .iter()
                .any(|s| file.contains(s))
            {
                "clean_versions"
            } else if depth == 1 {
                if file.contains(' ') {
                    "flat"
                } else {
                    "lowercase_flat"
                }
            } else if file.contains(" (") {
                "clean"
            } else {
                "scene"
            }
        }
        "episode" => {
            if row.absolute.is_some() {
                "anime"
            } else if p.contains("/Specials/") {
                "specials"
            } else if file.contains(" - S") {
                "clean"
            } else if is_x_style(file) {
                "x_style"
            } else if depth == 2 {
                "flat_season"
            } else {
                "scene"
            }
        }
        "extra" => "folder_extra",
        "sample" => "scene_sample",
        _ => "other",
    }
}

fn is_x_style(file: &str) -> bool {
    let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
    stem.rsplit_once(' ').is_some_and(|(_, code)| {
        code.split_once('x').is_some_and(|(s, e)| {
            !s.is_empty()
                && !e.is_empty()
                && s.bytes().all(|b| b.is_ascii_digit())
                && e.bytes().all(|b| b.is_ascii_digit())
        })
    })
}

/// Scores of the hard cases for one parser.
#[derive(Debug, Clone, Default, Serialize)]
pub struct HardReport {
    pub n: usize,
    pub all: usize,
    pub title: usize,
    pub year: usize,
    pub season: usize,
    pub episodes: usize,
    /// (input, got title, got year, got season, got episodes, per-field ok)
    pub cases: Vec<HardRow>,
}

/// One hard-case row of a [`HardReport`].
#[derive(Debug, Clone, Serialize)]
pub struct HardRow {
    pub input: String,
    pub title: Option<String>,
    pub year: Option<u16>,
    pub season: Option<u16>,
    pub episodes: Vec<u16>,
    pub absolute: Option<u32>,
    pub ok: [bool; 4],
    pub confidence: f32,
    pub evidence: Vec<String>,
}

/// Runs `parse` over the 36 hard cases.
pub fn evaluate_hard(mut parse: impl FnMut(&HardCase) -> NameGuess) -> HardReport {
    let mut r = HardReport {
        n: CASES.len(),
        ..Default::default()
    };
    for c in CASES {
        let g = parse(c);
        let s: CaseResult = c.score(&g);
        r.all += usize::from(s.all());
        r.title += usize::from(s.title);
        r.year += usize::from(s.year);
        r.season += usize::from(s.season);
        r.episodes += usize::from(s.episodes);
        r.cases.push(HardRow {
            input: c.input.to_string(),
            title: g.title.clone(),
            year: g.year,
            season: g.season,
            episodes: g.episodes.clone(),
            absolute: g.absolute_episode,
            ok: [s.title, s.year, s.season, s.episodes],
            confidence: g.confidence,
            evidence: g.evidence,
        });
    }
    r
}

/// Renders an [`Evaluation`] as Markdown tables.
pub fn render_markdown(name: &str, e: &Evaluation) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(s, "### {name}\n");
    let _ = writeln!(s, "| kind | field | accuracy | ok / n |\n|---|---|---|---|");
    for (kind, fields) in &e.per_kind {
        for (field, c) in fields {
            let _ = writeln!(s, "| {kind} | {field} | {:.2}% | {} / {} |", c.pct(), c.ok, c.n);
        }
    }
    let _ = writeln!(
        s,
        "| non-media | skipped by `is_ignored`/extension | {:.2}% | {} / {} |\n",
        e.non_media_skipped.pct(),
        e.non_media_skipped.ok,
        e.non_media_skipped.n
    );
    let _ = writeln!(s, "| kind/style | all fields | ok / n |\n|---|---|---|");
    for (k, c) in &e.per_style {
        let _ = writeln!(s, "| {k} | {:.2}% | {} / {} |", c.pct(), c.ok, c.n);
    }
    let _ = writeln!(
        s,
        "\nmedia files: {} · fallback used: {} · low confidence (<0.5): {} (of which wrong: {}) · wrong with confidence ≥0.5: {}\n",
        e.media_files, e.fallback_used, e.low_confidence.n, e.low_confidence.ok, e.confident_but_wrong
    );
    if !e.failure_counts.is_empty() {
        let _ = writeln!(
            s,
            "| failure class (kind/style/field) | count | example | expected | got |\n|---|---|---|---|---|"
        );
        for (class, n) in &e.failure_counts {
            if let Some(f) = e.failures.get(class).and_then(|v| v.first()) {
                let _ = writeln!(s, "| {class} | {n} | `{}` | {} | {} |", f.path, f.expected, f.got);
            }
        }
    }
    s
}

/// One case from guessit's YAML test suite (`movies.yml`, `episodes.yml`,
/// `various.yml`; LGPL-3.0 test vectors shipped in the `hunch` crate's
/// `tests/fixtures`). Only the properties flux cares about are kept; a field
/// is `None` when the case does not specify it (and is then not scored).
#[derive(Debug, Clone, Serialize)]
pub struct GuessitCase {
    /// Fixture file stem (`movies`, `episodes`, `various`).
    pub file: String,
    /// Position within the file (used for the dev/held-out split).
    pub index: usize,
    pub input: String,
    pub title: Option<String>,
    pub year: Option<u16>,
    pub season: Option<u16>,
    pub episodes: Option<Vec<u16>>,
    /// `movie` or `episode`.
    pub kind: Option<String>,
}

/// Loads a guessit fixture file. Mirrors guessit's own conventions: a
/// `__default__` group sets defaults, keys prefixed `+`/`-` and cases with
/// `options` are skipped, `-prop` removes an expectation.
pub fn load_guessit(path: &Path) -> Result<Vec<GuessitCase>, ManifestError> {
    let text = std::fs::read_to_string(path)?;
    let file = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    // (keys of one `? …` group, its `prop: value` pairs)
    type Group = (Vec<String>, Vec<(String, String)>);
    let mut groups: Vec<Group> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    let mut props: Vec<(String, String)> = Vec::new();
    let mut in_value = false;
    let mut list_key: Option<String> = None;
    let mut list_items: Vec<String> = Vec::new();
    fn flush(list_key: &mut Option<String>, items: &mut Vec<String>, props: &mut Vec<(String, String)>) {
        if let Some(k) = list_key.take() {
            props.push((k, format!("[{}]", items.join(", "))));
        }
        items.clear();
    }
    fn unquote(s: &str) -> String {
        let s = s.trim();
        let s = if s.starts_with('"') || s.starts_with('\'') {
            s
        } else {
            s.split(" #").next().unwrap_or(s).trim()
        };
        if s.len() >= 2
            && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
        {
            s[1..s.len() - 1].to_string()
        } else {
            s.to_string()
        }
    }
    let prop_line = |line: &str, props: &mut Vec<(String, String)>, list_key: &mut Option<String>| {
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim().to_string(), unquote(v));
            if v.is_empty() {
                *list_key = Some(k);
            } else if !k.is_empty() {
                props.push((k, v));
            }
        }
    };
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some(rest) = t.strip_prefix("? ") {
            if in_value {
                flush(&mut list_key, &mut list_items, &mut props);
                groups.push((std::mem::take(&mut keys), std::mem::take(&mut props)));
                in_value = false;
            }
            keys.push(unquote(rest));
        } else if let Some(rest) = t.strip_prefix(": ") {
            in_value = true;
            flush(&mut list_key, &mut list_items, &mut props);
            prop_line(rest, &mut props, &mut list_key);
        } else if in_value && line.starts_with([' ', '\t']) {
            if let Some(item) = t.strip_prefix("- ") {
                list_items.push(unquote(item));
            } else {
                flush(&mut list_key, &mut list_items, &mut props);
                prop_line(t, &mut props, &mut list_key);
            }
        }
    }
    flush(&mut list_key, &mut list_items, &mut props);
    if !keys.is_empty() {
        groups.push((keys, props));
    }

    let nums = |v: &str| -> Option<Vec<u16>> {
        v.trim_matches(['[', ']'])
            .split(',')
            .map(|x| x.trim().parse::<u16>().ok())
            .collect()
    };
    let mut defaults: Vec<(String, String)> = Vec::new();
    let mut out = Vec::new();
    for (keys, props) in groups {
        if keys.len() == 1 && keys[0] == "__default__" {
            defaults = props;
            continue;
        }
        let mut exp: BTreeMap<String, String> = defaults.iter().cloned().collect();
        for (k, v) in &props {
            match k.strip_prefix('-') {
                Some(stripped) => {
                    exp.remove(stripped);
                }
                None => {
                    exp.insert(k.clone(), v.clone());
                }
            }
        }
        if exp.contains_key("options") {
            continue;
        }
        for key in keys {
            if key.starts_with(['+', '-']) {
                continue;
            }
            let season = exp
                .get("season")
                .and_then(|v| nums(v))
                .and_then(|v| (v.len() == 1).then(|| v[0]));
            if exp.contains_key("season") && season.is_none() {
                continue; // multi-season packs: not representable in NameGuess
            }
            out.push(GuessitCase {
                file: file.clone(),
                index: out.len(),
                input: key,
                title: exp.get("title").cloned(),
                year: exp.get("year").and_then(|y| y.parse().ok()),
                season,
                episodes: exp.get("episode").and_then(|v| nums(v)),
                kind: exp.get("type").cloned(),
            });
        }
    }
    Ok(out)
}

/// Which half of the guessit cases to score. Even-indexed cases are the
/// *dev* half (inspected while improving the parser); odd-indexed cases are
/// *held out* (only aggregate scores are looked at).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Split {
    Dev,
    HeldOut,
    All,
}

impl Split {
    fn includes(self, index: usize) -> bool {
        match self {
            Split::Dev => index.is_multiple_of(2),
            Split::HeldOut => !index.is_multiple_of(2),
            Split::All => true,
        }
    }
}

/// Field accuracies on guessit cases (each field scored only where the case
/// specifies it).
#[derive(Debug, Clone, Default, Serialize)]
pub struct GuessitReport {
    pub cases: u64,
    pub title: Counter,
    pub year: Counter,
    pub season: Counter,
    pub episodes: Counter,
    pub kind: Counter,
    /// Every specified field among title/year/season/episodes correct.
    pub all: Counter,
    /// Cases where the fallback parser ran.
    pub fallback_used: u64,
    pub failures: Vec<GuessitFailure>,
}

/// A failed guessit case.
#[derive(Debug, Clone, Serialize)]
pub struct GuessitFailure {
    pub file: String,
    pub input: String,
    pub fields: Vec<String>,
    pub expected: String,
    pub got: String,
}

/// Scores `parse` on the chosen split of `cases`. `parse` receives the case
/// and returns a guess; `score_kind` controls whether `type` is scored (it
/// is meaningless when the library kind was derived from it).
pub fn evaluate_guessit(
    cases: &[GuessitCase],
    split: Split,
    score_kind: bool,
    mut parse: impl FnMut(&GuessitCase) -> NameGuess,
) -> GuessitReport {
    let mut r = GuessitReport::default();
    for c in cases.iter().filter(|c| split.includes(c.index)) {
        let g = parse(c);
        r.cases += 1;
        if g.evidence.iter().any(|e| e.starts_with("fallback")) {
            r.fallback_used += 1;
        }
        let eps: Vec<u16> = if g.episodes.is_empty() {
            g.absolute_episode
                .and_then(|a| u16::try_from(a).ok())
                .into_iter()
                .collect()
        } else {
            g.episodes.clone()
        };
        let mut bad = Vec::new();
        if let Some(t) = &c.title {
            let ok = g.title.as_deref().map(norm_key) == Some(norm_key(t));
            r.title.add(ok);
            if !ok {
                bad.push("title");
            }
        }
        if let Some(y) = c.year {
            let ok = g.year == Some(y);
            r.year.add(ok);
            if !ok {
                bad.push("year");
            }
        }
        if let Some(s) = c.season {
            let ok = g.season == Some(s);
            r.season.add(ok);
            if !ok {
                bad.push("season");
            }
        }
        if let Some(e) = &c.episodes {
            let ok = &eps == e;
            r.episodes.add(ok);
            if !ok {
                bad.push("episodes");
            }
        }
        if score_kind {
            if let Some(k) = &c.kind {
                let ok = matches!(
                    (k.as_str(), g.kind),
                    ("movie", GuessKind::Movie) | ("episode", GuessKind::Episode)
                );
                r.kind.add(ok);
            }
        }
        r.all.add(bad.is_empty());
        if !bad.is_empty() {
            r.failures.push(GuessitFailure {
                file: c.file.clone(),
                input: c.input.clone(),
                fields: bad.iter().map(|s| s.to_string()).collect(),
                expected: format!("{:?} | {:?} | S{:?} E{:?}", c.title, c.year, c.season, c.episodes),
                got: format!(
                    "{:?} | {:?} | S{:?} E{:?} | {:?}",
                    g.title, g.year, g.season, eps, g.kind
                ),
            });
        }
    }
    r
}
