//! Small text helpers: Unicode normalisation, comparison keys and title casing.

use std::borrow::Cow;

use unicode_normalization::{is_nfc_quick, IsNormalized, UnicodeNormalization};

/// Returns `s` in Unicode NFC.
///
/// Why: HFS+ stored names in (a variant of) NFD and files copied from it keep
/// that form on APFS, so "Amélie" can arrive as `Ame\u{301}lie`. Titles must be
/// NFC before display, comparison or matching. The quick check makes the common
/// already-NFC case allocation-free.
pub(crate) fn nfc(s: &str) -> Cow<'_, str> {
    match is_nfc_quick(s.chars()) {
        IsNormalized::Yes => Cow::Borrowed(s),
        _ => Cow::Owned(s.nfc().collect()),
    }
}

/// Comparison key for titles: lowercase, alphanumerics only.
///
/// This makes `Léon: The Professional`, `Léon - The Professional` and
/// `Léon.The.Professional` compare equal, which is how release names encode
/// the same title (':' is illegal on Windows, dots replace spaces in scene
/// names, apostrophes are dropped).
pub fn norm_key(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Words kept lowercase by [`title_case_if_lower`] unless they start the title.
const SMALL_WORDS: &[&str] = &[
    "a", "an", "and", "as", "at", "but", "by", "de", "for", "from", "in", "into", "nor", "of", "on", "or",
    "the", "to", "vs", "with",
];

/// Title-cases a title that is entirely lowercase (`the.matrix.1999.mkv` →
/// "The Matrix"); returns `None` when the title has any uppercase letter (the
/// user's casing wins) or no cased letters at all (CJK, digits).
pub(crate) fn title_case_if_lower(s: &str) -> Option<String> {
    let mut has_cased = false;
    for c in s.chars() {
        if c.is_uppercase() {
            return None;
        }
        has_cased |= c.is_lowercase();
    }
    if !has_cased {
        return None;
    }
    let mut out = String::with_capacity(s.len());
    let mut prev_numeric = false;
    for (i, word) in s.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        // Capitalise small words at the start and after a number-only word
        // ("2001 A Space Odyssey"), otherwise keep them lowercase.
        let keep_lower = i > 0 && !prev_numeric && SMALL_WORDS.contains(&word);
        if keep_lower {
            out.push_str(word);
        } else {
            let mut chars = word.chars();
            if let Some(first) = chars.next() {
                out.extend(first.to_uppercase());
                out.push_str(chars.as_str());
            }
        }
        prev_numeric = !word.is_empty() && word.chars().all(|c| c.is_ascii_digit());
    }
    Some(out)
}

/// `true` for CJK script characters: Han ideographs, kana, Hangul, bopomofo
/// and their extensions. Release names glue these to Latin words and digits
/// (`示例4k`, `S01E01中字`), and they are never ASCII-cased.
pub(crate) fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x1100..=0x11FF       // Hangul jamo
            | 0x2E80..=0x2FDF // CJK and Kangxi radicals
            | 0x3005..=0x3007 // 々 〆 〇
            | 0x3021..=0x3029 // Suzhou numerals
            | 0x3040..=0x30FF // Hiragana, Katakana
            | 0x3100..=0x312F // Bopomofo
            | 0x3130..=0x318F // Hangul compatibility jamo
            | 0x31F0..=0x31FF // Katakana extensions
            | 0x3400..=0x4DBF // CJK extension A
            | 0x4E00..=0x9FFF // CJK unified ideographs
            | 0xAC00..=0xD7AF // Hangul syllables
            | 0xF900..=0xFAFF // CJK compatibility ideographs
            | 0xFF66..=0xFF9F // halfwidth Katakana
            | 0x20000..=0x323AF // CJK extensions B–H
    )
}

/// `true` when `s` contains a CJK character ([`is_cjk`]).
pub(crate) fn has_cjk(s: &str) -> bool {
    !s.is_ascii() && s.chars().any(is_cjk)
}

/// Collapses runs of whitespace into single spaces and trims the ends.
pub(crate) fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for w in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out
}

/// Trims separator punctuation that never ends a real title (`-`, `_`, `,`,
/// whitespace) from both ends. Dots are handled by the tokenizer.
pub(crate) fn trim_seps(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || matches!(c, '-' | '_' | ',' | '–' | '—'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nfc_composes_decomposed_accents() {
        assert_eq!(nfc("Ame\u{301}lie"), "Amélie");
        assert!(matches!(nfc("Amélie"), Cow::Borrowed(_)));
    }

    #[test]
    fn norm_key_ignores_punctuation_and_case() {
        assert_eq!(
            norm_key("Léon: The Professional"),
            norm_key("Léon - The Professional")
        );
        assert_eq!(norm_key("WALL·E"), "walle");
        assert_eq!(norm_key("千と千尋の神隠し"), "千と千尋の神隠し");
    }

    #[test]
    fn cjk_characters() {
        assert!(is_cjk('剧') && is_cjk('の') && is_cjk('カ') && is_cjk('한') && is_cjk('〇'));
        assert!(!is_cjk('a') && !is_cjk('1') && !is_cjk('é') && !is_cjk('：') && !is_cjk('【'));
        assert!(has_cjk("Some.Show.示例") && !has_cjk("Amélie"));
    }

    #[test]
    fn title_case_only_for_lowercase() {
        assert_eq!(title_case_if_lower("the matrix").as_deref(), Some("The Matrix"));
        assert_eq!(
            title_case_if_lower("2001 a space odyssey").as_deref(),
            Some("2001 A Space Odyssey")
        );
        assert_eq!(
            title_case_if_lower("lord of the rings").as_deref(),
            Some("Lord of the Rings")
        );
        assert_eq!(title_case_if_lower("léon").as_deref(), Some("Léon"));
        assert_eq!(title_case_if_lower("The Matrix"), None);
        assert_eq!(title_case_if_lower("1917"), None);
    }
}
