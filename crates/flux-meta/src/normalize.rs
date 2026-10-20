//! Title normalisation and similarity, shared by the offline export index and
//! the matcher so that both sides of every comparison are normalised the same way.
//!
//! Normalisation is deliberately conservative: it folds what is *the same
//! title written differently* (case, diacritics, punctuation, `&`/`and`,
//! roman-numeral sequels) and nothing else. Non-Latin scripts are kept as they
//! are, so `기생충` still matches `기생충` exactly; romanising CJK is the job of
//! TMDB `/search` (which matches translated titles), not of this module.

use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

/// English articles that may be dropped or moved (`Matrix, The`). Other
/// languages' articles are not stripped: "Die Hard" is not "Hard".
const ARTICLES: [&str; 3] = ["the", "a", "an"];

/// Normalise a title for comparison.
///
/// - Moves a trailing English article to the front (`"Matrix, The"` → `"the matrix"`).
/// - Lowercases, NFKD-decomposes and drops combining marks (`Amélie` → `amelie`).
/// - Maps letters that do not decompose (`ß`→`ss`, `æ`→`ae`, `ø`→`o`, …).
/// - `&` → `and`; apostrophes vanish (`Ocean's` → `oceans`); other punctuation
///   becomes a word break (`WALL·E` → `wall e`, `Léon: The Professional` →
///   `leon the professional`).
/// - Standalone roman numerals II–IX become digits (`Rocky II` → `rocky 2`).
pub fn normalize_title(raw: &str) -> String {
    let fronted = front_trailing_article(raw.trim());
    let mut out = String::with_capacity(fronted.len() + 4);
    let mut pending_space = false;
    let push_char = |out: &mut String, pending_space: &mut bool, c: char| {
        if *pending_space && !out.is_empty() {
            out.push(' ');
        }
        *pending_space = false;
        out.push(c);
    };
    for c in fronted.chars() {
        // Characters that need a word break or a multi-letter expansion
        // before compatibility decomposition (which would glue `Alien³` into `alien3`).
        let expanded: Option<&str> = match c {
            '&' => Some(" and "),
            '¹' => Some(" 1"),
            '²' => Some(" 2"),
            '³' => Some(" 3"),
            'ß' => Some("ss"),
            'æ' | 'Æ' => Some("ae"),
            'œ' | 'Œ' => Some("oe"),
            'ø' | 'Ø' => Some("o"),
            'ł' | 'Ł' => Some("l"),
            'đ' | 'Đ' | 'ð' | 'Ð' => Some("d"),
            'þ' | 'Þ' => Some("th"),
            'ı' => Some("i"),
            _ => None,
        };
        if let Some(s) = expanded {
            for e in s.chars() {
                if e == ' ' {
                    pending_space = true;
                } else {
                    push_char(&mut out, &mut pending_space, e);
                }
            }
            continue;
        }
        if matches!(c, '\'' | '’' | 'ʼ' | '`' | '´') {
            // "Ocean's" → "oceans" (apostrophes join), but French/Italian
            // elision splits: "d'Amélie" → "d amelie", "l'Odyssée" → "l odyssee".
            if is_elision(&out) {
                pending_space = true;
            }
            continue;
        }
        if folds(c) {
            for d in std::iter::once(c).nfkd() {
                if is_combining_mark(d) {
                    continue;
                }
                if d.is_alphanumeric() {
                    for l in d.to_lowercase() {
                        push_char(&mut out, &mut pending_space, l);
                    }
                } else {
                    pending_space = true;
                }
            }
        } else if c.is_alphanumeric() || is_combining_mark(c) {
            // Other scripts are kept verbatim (only lowercased): NFKD would
            // split Hangul syllables into jamo and strip Japanese dakuten
            // (ゴジラ → コシラ), which is not "the same title".
            for l in c.to_lowercase() {
                push_char(&mut out, &mut pending_space, l);
            }
        } else {
            pending_space = true;
        }
    }
    romanize_sequels(out)
}

/// Characters whose compatibility decomposition is a *spelling* variant:
/// Latin, Greek, Cyrillic, standalone combining marks, number forms (Ⅱ) and
/// full-width ASCII.
fn folds(c: char) -> bool {
    matches!(c as u32, 0x00A0..=0x052F | 0x1E00..=0x1FFF | 0x2150..=0x218F | 0xFF01..=0xFF5E)
}

/// True when `out` ends in a single-letter (or `qu`) word: an elided article
/// or preposition such as `d'`, `l'`, `j'`, `qu'`.
fn is_elision(out: &str) -> bool {
    let last = out.rsplit(' ').next().unwrap_or("");
    matches!(last, "d" | "l" | "j" | "n" | "s" | "c" | "m" | "t" | "qu")
}

/// `"Matrix, The"` → `"The Matrix"` (Plex/Kodi sort-title convention).
fn front_trailing_article(s: &str) -> std::borrow::Cow<'_, str> {
    if let Some((head, tail)) = s.rsplit_once(',') {
        let t = tail.trim();
        if ARTICLES.iter().any(|a| t.eq_ignore_ascii_case(a)) && !head.trim().is_empty() {
            return std::borrow::Cow::Owned(format!("{t} {}", head.trim()));
        }
    }
    std::borrow::Cow::Borrowed(s)
}

/// Replace standalone roman numerals II..IX with digits. `I`, `V` and `X` are
/// left alone: they are too often words or titles ("I, Robot", "X").
fn romanize_sequels(s: String) -> String {
    const MAP: [(&str, &str); 7] = [
        ("ii", "2"),
        ("iii", "3"),
        ("iv", "4"),
        ("vi", "6"),
        ("vii", "7"),
        ("viii", "8"),
        ("ix", "9"),
    ];
    if !s.split(' ').any(|t| MAP.iter().any(|(r, _)| *r == t)) {
        return s;
    }
    s.split(' ')
        .map(|t| MAP.iter().find(|(r, _)| *r == t).map_or(t, |(_, d)| d))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Drop one leading English article from an already-normalised title.
pub fn strip_article(norm: &str) -> &str {
    for a in ARTICLES {
        if let Some(rest) = norm.strip_prefix(a).and_then(|r| r.strip_prefix(' ')) {
            if !rest.is_empty() {
                return rest;
            }
        }
    }
    norm
}

/// Similarity of two **normalised** titles in `0.0..=1.0`.
///
/// - `1.0` exact; `0.97` equal once a leading article is ignored.
/// - Otherwise the mean of Jaro–Winkler (good for typos and truncations) and
///   the Sørensen–Dice coefficient over word tokens (good for word order and
///   extra words), capped at `0.95` so that nothing fuzzy ties an exact match.
/// - Different sequel numbers (`toy story 2` vs `toy story 3`) cost 25%:
///   sequels are the classic near-duplicate.
pub fn title_similarity(a: &str, b: &str) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    let (sa, sb) = (strip_article(a), strip_article(b));
    if sa == sb {
        return 0.97;
    }
    let jw = strsim::jaro_winkler(sa, sb) as f32;
    let dice = token_dice(sa, sb);
    let mut sim = 0.5 * jw + 0.5 * dice;
    if numbers_differ(sa, sb) {
        sim *= 0.75;
    }
    sim.min(0.95)
}

/// "Soft" Sørensen–Dice coefficient over whitespace tokens: a token counts as
/// shared with weight = its best Jaro–Winkler against the other title's tokens
/// when that is ≥ 0.88 (so `interstelar` still shares a word with
/// `interstellar`, but `2` never matches `3`).
fn token_dice(a: &str, b: &str) -> f32 {
    let ta: Vec<&str> = a.split(' ').collect();
    let tb: Vec<&str> = b.split(' ').collect();
    let common: f32 = ta
        .iter()
        .map(|x| {
            if tb.contains(x) {
                return 1.0;
            }
            let best = tb
                .iter()
                .map(|y| strsim::jaro_winkler(x, y) as f32)
                .fold(0.0, f32::max);
            // Short tokens (numbers, initials) must match exactly.
            if best >= 0.88 && x.chars().count() > 3 {
                best
            } else {
                0.0
            }
        })
        .sum();
    (2.0 * common) / (ta.len() + tb.len()) as f32
}

/// True when the two titles carry different standalone numbers.
fn numbers_differ(a: &str, b: &str) -> bool {
    let nums = |s: &str| -> Vec<String> {
        s.split(' ')
            .filter(|t| t.chars().all(|c| c.is_ascii_digit()))
            .map(str::to_owned)
            .collect()
    };
    let (na, nb) = (nums(a), nums(b));
    (!na.is_empty() || !nb.is_empty()) && na != nb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_diacritics_and_punctuation() {
        assert_eq!(normalize_title("Amélie"), "amelie");
        assert_eq!(normalize_title("Léon: The Professional"), "leon the professional");
        assert_eq!(normalize_title("WALL·E"), "wall e");
        assert_eq!(normalize_title("WALL-E"), "wall e");
        assert_eq!(normalize_title("Ocean's Eleven"), "oceans eleven");
        assert_eq!(normalize_title("Fast & Furious"), "fast and furious");
        assert_eq!(normalize_title("Alien³"), "alien 3");
        assert_eq!(normalize_title("Tár"), "tar");
        assert_eq!(normalize_title("Shōgun"), "shogun");
        assert_eq!(normalize_title("Das Boot"), "das boot");
        assert_eq!(normalize_title("Straße"), "strasse");
        assert_eq!(normalize_title("  Se7en  "), "se7en");
        assert_eq!(normalize_title("Your Name."), "your name");
        assert_eq!(
            normalize_title("Le Fabuleux Destin d'Amélie Poulain"),
            "le fabuleux destin d amelie poulain"
        );
        assert_eq!(normalize_title("Ｔｏｋｙｏ"), "tokyo");
        assert_eq!(normalize_title("Rocky Ⅱ"), "rocky 2");
    }

    #[test]
    fn keeps_non_latin_scripts() {
        assert_eq!(normalize_title("기생충"), "기생충");
        assert_eq!(normalize_title("千と千尋の神隠し"), "千と千尋の神隠し");
        assert_eq!(normalize_title("ゴジラ-1.0"), "ゴジラ 1 0");
        assert_eq!(normalize_title("Солярис"), "солярис");
    }

    #[test]
    fn articles_and_sequels() {
        assert_eq!(normalize_title("Matrix, The"), "the matrix");
        assert_eq!(normalize_title("Rocky II"), "rocky 2");
        assert_eq!(
            normalize_title("Star Wars: Episode IV - A New Hope"),
            "star wars episode 4 a new hope"
        );
        assert_eq!(normalize_title("I, Robot"), "i robot");
        assert_eq!(strip_article("the matrix"), "matrix");
        assert_eq!(strip_article("the"), "the");
        assert_eq!(strip_article("theatre"), "theatre");
    }

    #[test]
    fn similarity_ordering() {
        let n = normalize_title;
        assert_eq!(title_similarity(&n("The Matrix"), &n("the matrix")), 1.0);
        assert_eq!(title_similarity(&n("Matrix"), &n("The Matrix")), 0.97);
        let sequel = title_similarity(&n("Toy Story 2"), &n("Toy Story 3"));
        let typo = title_similarity(&n("Interstelar"), &n("Interstellar"));
        assert!(typo > 0.6, "typo {typo}");
        assert!(sequel < typo, "sequel {sequel} typo {typo}");
        assert!(title_similarity(&n("Heat"), &n("Inception")) < 0.5);
        assert!(title_similarity("", "x") == 0.0);
    }
}
