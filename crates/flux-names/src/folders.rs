//! Folder roles: season folders, extras folders, sample folders, disc
//! structures and generic container folders that never carry a title.

use std::sync::LazyLock;

use flux_core::ExtraKind;
use regex::Regex;

/// What a directory name tells us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FolderRole {
    /// `Season 02`, `Season 2`, `S02`, `Series 2`, `Staffel 2`, `Season 00`,
    /// `第2季`, `第二季`.
    Season(u16),
    /// `Specials`: season 0 for shows, bonus material for movies.
    Specials,
    /// Jellyfin/Plex extras folders (`Featurettes`, `Behind The Scenes`, …).
    Extras(ExtraKind),
    /// `Sample`/`Samples`.
    Sample,
    /// Disc structure internals (`BDMV`, `STREAM`, `VIDEO_TS`): the title is above.
    Disc,
    /// Multi-part folders (`CD1`, `Disc 2`).
    Part(u8),
    /// Library/container folders that never carry a title (`Movies`, `TV`, `Downloads`).
    Generic(GenericHint),
    /// Anything else: a candidate title folder.
    Plain,
}

/// The prior a generic folder name gives about its contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GenericHint {
    Movies,
    Shows,
    None,
}

static SEASON_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?ix)^(?:
            (?:season|series|saison|staffel|temporada|stagione|seizoen|sezon|säsong|sæson|kausi)[\s._-]*(\d{1,4})
            | s(\d{1,3})
        )(?:[\s._-]*(?:[\[(].*|-.*))?$",
    )
    .expect("valid season regex")
});

static PART_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:cd|dvd|disc|disk|part|pt)[\s._-]*(\d{1,2})$").expect("valid part regex")
});

/// Classifies one directory name.
pub(crate) fn classify(name: &str) -> FolderRole {
    let lower = name.trim().to_lowercase();
    match lower.as_str() {
        "specials" | "special" | "season specials" => return FolderRole::Specials,
        "sample" | "samples" => return FolderRole::Sample,
        "bdmv" | "stream" | "video_ts" | "audio_ts" | "hvdvd_ts" | "playlist" | "clipinf" | "backup"
        | "certificate" | "bdav" => return FolderRole::Disc,
        _ => {}
    }
    if let Some(kind) = extras_folder(&lower) {
        return FolderRole::Extras(kind);
    }
    if let Some(hint) = generic_folder(&lower) {
        return FolderRole::Generic(hint);
    }
    if let Some((true, n)) = crate::stem::cjk_marker(name.trim()) {
        return FolderRole::Season(n);
    }
    if let Some(c) = SEASON_RE.captures(name.trim()) {
        if let Some(n) = c
            .get(1)
            .or_else(|| c.get(2))
            .and_then(|m| m.as_str().parse::<u16>().ok())
        {
            return FolderRole::Season(n);
        }
    }
    if let Some(c) = PART_RE.captures(name.trim()) {
        if let Some(n) = c
            .get(1)
            .and_then(|m| m.as_str().parse::<u8>().ok())
            .filter(|n| *n > 0)
        {
            return FolderRole::Part(n);
        }
    }
    FolderRole::Plain
}

/// `Other` or `Others`: an extras folder by Plex's and Jellyfin's rules, and
/// a common name for a folder of miscellaneous files.
pub(crate) fn is_other_folder(name: &str) -> bool {
    matches!(name.trim().to_lowercase().as_str(), "other" | "others")
}

/// A title, then a year in brackets: `Some Film (2010)`, `示例片 [2010]`.
static TITLED_YEAR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[\p{L}\p{N}].*[(\[]\s*(?:19|20)\d{2}\s*[)\]]").expect("valid titled-year regex")
});

/// A folder (its name as on disk) named the way Plex and Jellyfin name a
/// film's or show's folder: a title and a year in brackets (`Some Film
/// (2010)`), or a provider id tag (`[tmdbid-603]`).
pub(crate) fn names_a_work(raw: &str) -> bool {
    crate::ids::has_id(raw) || TITLED_YEAR_RE.is_match(raw)
}

/// Jellyfin's and Plex's extras folder names (`NamingOptions.VideoExtraRules`,
/// Plex "Local Media Assets – Movies").
fn extras_folder(lower: &str) -> Option<ExtraKind> {
    Some(match lower {
        "trailers" | "trailer" => ExtraKind::Trailer,
        "featurettes" | "featurette" => ExtraKind::Featurette,
        "behind the scenes" | "behindthescenes" | "behind-the-scenes" | "making of" => {
            ExtraKind::BehindTheScenes
        }
        "deleted scenes" | "deletedscenes" | "deleted" | "deleted-scenes" => ExtraKind::DeletedScene,
        "interviews" | "interview" => ExtraKind::Interview,
        "shorts" | "short" => ExtraKind::Short,
        "scenes" | "scene" => ExtraKind::Scene,
        "extras"
        | "extra"
        | "other"
        | "others"
        | "clips"
        | "clip"
        | "bonus"
        | "bonus features"
        | "special features"
        | "featurettes & extras"
        | "theme-music"
        | "backdrops" => ExtraKind::Other,
        _ => return None,
    })
}

fn generic_folder(lower: &str) -> Option<GenericHint> {
    Some(match lower {
        "movies" | "movie" | "films" | "film" | "4k movies" | "kids movies" | "documentaries" => {
            GenericHint::Movies
        }
        "tv" | "tv shows" | "tvshows" | "tv-shows" | "shows" | "series" | "tv series" | "anime"
        | "kids tv" | "cartoons" => GenericHint::Shows,
        "media" | "video" | "videos" | "downloads" | "download" | "complete" | "completed" | "incoming"
        | "torrents" | "library" | "new" | "unsorted" | "misc" | "4k" | "uhd" | "hd" | "1080p" | "720p"
        | "home videos" | "volumes" => GenericHint::None,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn season_folders() {
        assert_eq!(classify("Season 02"), FolderRole::Season(2));
        assert_eq!(classify("Season 1"), FolderRole::Season(1));
        assert_eq!(classify("S03"), FolderRole::Season(3));
        assert_eq!(classify("season.04"), FolderRole::Season(4));
        assert_eq!(classify("Staffel 5"), FolderRole::Season(5));
        assert_eq!(classify("Season 00"), FolderRole::Season(0));
        assert_eq!(classify("Season 2 (2019)"), FolderRole::Season(2));
        assert_eq!(classify("Specials"), FolderRole::Specials);
        assert_eq!(classify("第二季"), FolderRole::Season(2));
        assert_eq!(classify("第3季"), FolderRole::Season(3));
        assert_eq!(classify("第3集"), FolderRole::Plain);
        assert_eq!(classify("Seasons of Love"), FolderRole::Plain);
        assert_eq!(classify("Severance (2022)"), FolderRole::Plain);
    }

    #[test]
    fn extras_and_others() {
        assert_eq!(classify("Featurettes"), FolderRole::Extras(ExtraKind::Featurette));
        assert_eq!(
            classify("Behind The Scenes"),
            FolderRole::Extras(ExtraKind::BehindTheScenes)
        );
        assert_eq!(classify("Sample"), FolderRole::Sample);
        assert_eq!(classify("VIDEO_TS"), FolderRole::Disc);
        assert_eq!(classify("CD2"), FolderRole::Part(2));
        assert_eq!(classify("Movies"), FolderRole::Generic(GenericHint::Movies));
        assert_eq!(classify("TV"), FolderRole::Generic(GenericHint::Shows));
        assert_eq!(classify("Others"), FolderRole::Extras(ExtraKind::Other));
        assert!(is_other_folder("others") && is_other_folder(" Other "));
        assert!(!is_other_folder("Extras") && !is_other_folder("Otherwise"));
        for work in [
            "Some Film (2010)",
            "示例片 [2010]",
            "Some Show [tvdbid-81189]",
            "Film {tmdb-603}",
        ] {
            assert!(names_a_work(work), "{work}");
        }
        for plain in [
            "2019",
            "(2019)",
            "Trip 2019",
            "Holiday",
            "Clips (old)",
            "Film (1080p)",
        ] {
            assert!(!names_a_work(plain), "{plain}");
        }
    }
}
