//! Explicit provider ids embedded in names: `[tmdbid-603]`, `{tmdb-603}`,
//! `[imdbid-tt0133093]`, `{imdb-tt0133093}`, `[tvdbid-81189]`, `{tvdb-81189}`.
//!
//! These are the Jellyfin (`[tmdbid-…]`), Plex/*arr (`{tmdb-…}`) and Kodi
//! conventions; `=` instead of `-` is accepted too (Jellyfin allows both).
//! An id short-circuits matching (research/02 §6.3 step 1).

use std::sync::LazyLock;

use flux_core::ProviderIds;
use regex::Regex;

static ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[\[{(]\s*(tmdb|tmdbid|imdb|imdbid|tvdb|tvdbid)\s*[-=:]\s*(tt\d{5,10}|\d{1,10})\s*[\]})]")
        .expect("valid id regex")
});

/// `{edition-Director's Cut}` (Plex edition tag; Radarr writes it too).
static EDITION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\{\s*edition\s*-\s*([^}]+?)\s*\}").expect("valid edition regex"));

/// Extracts provider ids from `name` into `ids` (existing values win, so
/// callers pass the filename before its folders) and returns `name` with the
/// id tags removed. Also reports which tags fired, for evidence.
pub(crate) fn extract(name: &str, ids: &mut ProviderIds, evidence: &mut Vec<String>) -> String {
    if !name.contains(['[', '{', '(']) {
        return name.to_string();
    }
    for c in ID_RE.captures_iter(name) {
        let (Some(key), Some(val)) = (c.get(1), c.get(2)) else {
            continue;
        };
        let key = key.as_str().to_ascii_lowercase();
        let val = val.as_str();
        let set = match key.as_str() {
            "tmdb" | "tmdbid" => set_num(&mut ids.tmdb, val),
            "tvdb" | "tvdbid" => set_num(&mut ids.tvdb, val),
            _ => {
                // IMDb ids are always `tt` + digits; a bare number is not one.
                if ids.imdb.is_none() && val.len() > 2 && val[..2].eq_ignore_ascii_case("tt") {
                    ids.imdb = Some(format!("tt{}", &val[2..]));
                    true
                } else {
                    false
                }
            }
        };
        if set {
            evidence.push(format!("explicit id {}", &c[0]));
        }
    }
    ID_RE.replace_all(name, " ").into_owned()
}

fn set_num(slot: &mut Option<u64>, val: &str) -> bool {
    if slot.is_some() {
        return false;
    }
    match val.parse::<u64>() {
        Ok(v) if v > 0 => {
            *slot = Some(v);
            true
        }
        _ => false,
    }
}

/// Extracts a `{edition-X}` tag. Returns the edition and `name` without it.
pub(crate) fn extract_edition(name: &str) -> (Option<String>, String) {
    if !name.contains('{') {
        return (None, name.to_string());
    }
    match EDITION_RE.captures(name) {
        Some(c) => {
            let ed = c
                .get(1)
                .map(|m| m.as_str().trim().to_string())
                .filter(|s| !s.is_empty());
            (ed, EDITION_RE.replace_all(name, " ").into_owned())
        }
        None => (None, name.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_id_styles() {
        let mut ids = ProviderIds::default();
        let mut ev = Vec::new();
        let rest = extract(
            "The Matrix (1999) [tmdbid-603] {imdb-tt0133093}",
            &mut ids,
            &mut ev,
        );
        assert_eq!(ids.tmdb, Some(603));
        assert_eq!(ids.imdb.as_deref(), Some("tt0133093"));
        assert_eq!(rest.trim(), "The Matrix (1999)");
        assert_eq!(ev.len(), 2);

        let mut ids = ProviderIds::default();
        extract("Show {tvdb-81189} [tmdbid=1396]", &mut ids, &mut ev);
        assert_eq!(ids.tvdb, Some(81189));
        assert_eq!(ids.tmdb, Some(1396));
    }

    #[test]
    fn first_id_wins() {
        let mut ids = ProviderIds {
            tmdb: Some(1),
            ..Default::default()
        };
        let mut ev = Vec::new();
        extract("x [tmdbid-2]", &mut ids, &mut ev);
        assert_eq!(ids.tmdb, Some(1));
    }

    #[test]
    fn edition_tag() {
        let (ed, rest) = extract_edition("Alien (1979) {edition-Director's Cut}");
        assert_eq!(ed.as_deref(), Some("Director's Cut"));
        assert_eq!(rest.trim(), "Alien (1979)");
    }
}
