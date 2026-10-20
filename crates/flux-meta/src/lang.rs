//! Language code helpers: containers carry ISO 639-2 (`eng`, `kor`, `fre`) or
//! BCP 47 (`en-US`); TMDB's `original_language` is ISO 639-1 (`en`, `ko`, `fr`).

/// ISO 639-2 (terminology and bibliographic forms) → ISO 639-1 for the
/// languages that matter for film/TV matching.
const ISO639_2_TO_1: &[(&str, &str)] = &[
    ("eng", "en"),
    ("fra", "fr"),
    ("fre", "fr"),
    ("deu", "de"),
    ("ger", "de"),
    ("spa", "es"),
    ("ita", "it"),
    ("por", "pt"),
    ("rus", "ru"),
    ("jpn", "ja"),
    ("kor", "ko"),
    ("zho", "zh"),
    ("chi", "zh"),
    ("cmn", "zh"),
    ("yue", "zh"),
    ("hin", "hi"),
    ("tel", "te"),
    ("tam", "ta"),
    ("ara", "ar"),
    ("heb", "he"),
    ("tur", "tr"),
    ("pol", "pl"),
    ("nld", "nl"),
    ("dut", "nl"),
    ("swe", "sv"),
    ("nor", "no"),
    ("nob", "no"),
    ("dan", "da"),
    ("fin", "fi"),
    ("isl", "is"),
    ("ice", "is"),
    ("ces", "cs"),
    ("cze", "cs"),
    ("slk", "sk"),
    ("slo", "sk"),
    ("hun", "hu"),
    ("ron", "ro"),
    ("rum", "ro"),
    ("ell", "el"),
    ("gre", "el"),
    ("ukr", "uk"),
    ("tha", "th"),
    ("vie", "vi"),
    ("ind", "id"),
    ("msa", "ms"),
    ("may", "ms"),
    ("fas", "fa"),
    ("per", "fa"),
    ("cat", "ca"),
    ("eus", "eu"),
    ("baq", "eu"),
    ("glg", "gl"),
    ("srp", "sr"),
    ("hrv", "hr"),
    ("bul", "bg"),
    ("est", "et"),
    ("lav", "lv"),
    ("lit", "lt"),
    ("slv", "sl"),
    ("tgl", "tl"),
    ("fil", "tl"),
    ("ben", "bn"),
    ("mar", "mr"),
    ("mal", "ml"),
    ("kan", "kn"),
    ("urd", "ur"),
];

/// Normalise a container or provider language tag to ISO 639-1 (`"en"`).
/// Returns `None` for unknown, undetermined (`und`), `mul`, `zxx` and empty tags.
pub fn to_iso639_1(tag: &str) -> Option<String> {
    let primary = tag.split(['-', '_']).next()?.trim().to_ascii_lowercase();
    match primary.len() {
        2 if primary.chars().all(|c| c.is_ascii_lowercase()) => Some(primary),
        3 => ISO639_2_TO_1
            .iter()
            .find(|(three, _)| *three == primary)
            .map(|(_, two)| (*two).to_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_common_tags() {
        assert_eq!(to_iso639_1("eng").as_deref(), Some("en"));
        assert_eq!(to_iso639_1("kor").as_deref(), Some("ko"));
        assert_eq!(to_iso639_1("fre").as_deref(), Some("fr"));
        assert_eq!(to_iso639_1("en-US").as_deref(), Some("en"));
        assert_eq!(to_iso639_1("JA").as_deref(), Some("ja"));
        assert_eq!(to_iso639_1("und"), None);
        assert_eq!(to_iso639_1(""), None);
    }
}
