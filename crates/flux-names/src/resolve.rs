//! Path-level reasoning: combines the filename with its folders, the library
//! kind and the sibling names into one [`NameGuess`].
//!
//! Canonical layouts recognised here (Plex, Jellyfin, Sonarr/Radarr):
//!
//! ```text
//! Movies:  Title (Year) [ids]/Title (Year) [- Version] [{edition-X}] [ids].ext
//!          Title (Year)/Title (Year) - cd1.ext, … - 2160p.ext
//!          Title (Year)/{Featurettes,Trailers,…}/Name.ext, Title (Year)-trailer.ext
//!          Release.Name.Year.Tags-GRP/{Release…ext, Sample/…-sample.ext}
//!          Title (Year)/{BDMV/STREAM,VIDEO_TS}/…, Title (Year)/CD1/…
//! Shows:   Show (Year)/Season NN/Show - SxxEyy[-Ezz] - Title.ext
//!          Show/{Specials,Season 00}/…, Show/S01/…, Show/Show.S01E02.ext
//!          Show/[Group] Show - 123 [CRC].ext (anime absolute numbering)
//!          Show/Show.YYYY.MM.DD.ext (date-based)
//! ```
//!
//! Folder titles beat filename titles for shows (the show folder is the
//! user's curation; Jellyfin's rule too). For movies the filename wins when it
//! carries a year, otherwise the movie folder does.

use std::path::{Component, Path};
use std::sync::LazyLock;

use flux_core::{ExtraKind, GuessKind, LibraryKind, NameGuess};
use regex::Regex;

use crate::folders::{self, FolderRole, GenericHint};
use crate::ids;
use crate::stem::{self, Marker, Stem, StemOpts};
use crate::text::{nfc, norm_key, trim_seps};
use crate::vocab;

/// Fast-path confidence below which the fallback parser is consulted.
pub(crate) const FALLBACK_THRESHOLD: f32 = 0.5;

/// What later stages (fallback, post-rules) need to know about the path.
#[derive(Debug, Clone)]
pub(crate) struct Context {
    /// The relative path, `/`-separated, NFC.
    pub path: String,
    /// Parent directory of the file (`/`-separated, may be empty); the
    /// fallback joins sibling names onto it.
    #[cfg_attr(not(feature = "hunch"), allow(dead_code))]
    pub parent: String,
    /// Filename stem with ids, edition tags and extra suffixes removed.
    pub stem: String,
    /// Season number from a season folder (`Season 02`, `Specials` = 0).
    pub season_folder: Option<u16>,
    pub lib: LibraryKind,
}

/// The fast-path result plus its context.
pub(crate) struct Resolved {
    pub guess: NameGuess,
    pub ctx: Context,
}

#[derive(Debug)]
struct Dir {
    /// Name with ids and edition tags removed.
    name: String,
    role: FolderRole,
    edition: Option<String>,
}

/// Special folders found between the file and the library root.
#[derive(Debug, Default)]
struct Layout {
    season: Option<(usize, u16)>,
    specials: Option<usize>,
    extras: Option<(usize, ExtraKind)>,
    sample: bool,
    disc: bool,
    part: Option<u8>,
    /// Nearest plain (title-bearing) folder.
    owner: Option<usize>,
    /// Top-most plain folder.
    top: Option<usize>,
    hint: Option<GenericHint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Suffix {
    Sample,
    Extra(ExtraKind),
}

/// Jellyfin/Plex extra suffixes (`-trailer`, `-featurette`, …) and scene
/// sample suffixes (`-sample`, `.sample`).
static SUFFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:[-._ ](trailer|sample)|-(featurette|behindthescenes|deleted|deletedscene|interview|scene|short|clip|extra|other))\d{0,2}$",
    )
    .expect("valid suffix regex")
});

/// Multi-part stacking (`Movie cd1`, `Movie - part2`), as in Jellyfin's
/// `VideoFileStackingRules`.
static STACK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(.*?)[\s._-]*(?:cd|dvd|part|pt|dis[ck])[\s._-]*(\d{1,2})$").expect("valid stack regex")
});

/// Bare episode numbers inside a season folder: `03.mkv`, `03 - Title.mkv`.
static BARE_EP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{1,3})(?:[\s._-]+(.+))?$").expect("valid bare episode regex"));

/// Generic stems that carry no title (disc rips, camera-less dumps).
static JUNK_STEM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:movie|video|film|main|feature|title|index|disc|dvd|title[\s_-]*t?\d+|vts[\s_-]*\d+[\s_-]*\d+|\d+)$")
        .expect("valid junk regex")
});

/// Splits a filename into stem and extension. An extension is 1–5 ASCII
/// alphanumerics with at least one letter that is not a release tag, so
/// `Show.Season.1` and `Name.BDMux.720p` keep their last word.
pub(crate) fn split_ext(file: &str) -> (&str, Option<&str>) {
    match file.rfind('.') {
        Some(pos) if pos > 0 => {
            let ext = &file[pos + 1..];
            let plausible = (1..=5).contains(&ext.len())
                && ext.bytes().all(|c| c.is_ascii_alphanumeric())
                && ext.bytes().any(|c| c.is_ascii_alphabetic());
            if plausible && (crate::is_video_ext(ext) || vocab::classify(ext).is_none()) {
                (&file[..pos], Some(ext))
            } else {
                (file, None)
            }
        }
        _ => (file, None),
    }
}

/// Runs the fast path over a whole relative path.
pub(crate) fn resolve(rel_path: &Path, siblings: &[&str], lib: LibraryKind) -> Resolved {
    // Backslash-separated paths (from Windows clients or NAS exports) split
    // like native ones; drive letters ("C:") are dropped.
    let comps: Vec<String> = rel_path
        .components()
        .filter_map(|c| match c {
            Component::Normal(os) => Some(nfc(&os.to_string_lossy()).into_owned()),
            _ => None,
        })
        .flat_map(|c| c.split('\\').map(str::to_string).collect::<Vec<_>>())
        .filter(|c| !c.is_empty() && !(c.len() == 2 && c.ends_with(':')))
        .collect();
    let mut g = NameGuess::unknown();
    let Some((file, dirs)) = comps.split_last() else {
        g.evidence.push("empty path".into());
        let ctx = Context {
            path: String::new(),
            parent: String::new(),
            stem: String::new(),
            season_folder: None,
            lib,
        };
        return Resolved { guess: g, ctx };
    };
    let (stem_raw, _ext) = split_ext(file);
    let mut ev = Vec::new();

    // 1. Explicit ids: the filename first, then folders from the nearest up.
    let stem_noid = ids::extract(stem_raw, &mut g.ids, &mut ev);
    let (edition_tag, stem_noid) = ids::extract_edition(&stem_noid);
    let mut dir_list: Vec<Dir> = Vec::with_capacity(dirs.len());
    for d in dirs.iter().rev() {
        let name = ids::extract(d, &mut g.ids, &mut ev);
        let (edition, name) = ids::extract_edition(&name);
        let name = name.trim().to_string();
        let role = folders::classify(&name);
        dir_list.push(Dir { name, role, edition });
    }
    dir_list.reverse();
    let l = layout(&dir_list);

    // 2. Library prior, refined by generic folders in a mixed library and by
    //    a season-pack parent folder ("Show.S01.1080p-GRP/…").
    let tv_prior =
        lib == LibraryKind::Shows || (lib == LibraryKind::Mixed && l.hint == Some(GenericHint::Shows));
    let parent = dir_list.last().filter(|d| d.role == FolderRole::Plain);
    let parent_parse = parent.map(|d| stem::parse_stem(&d.name, folder_opts(tv_prior)));
    let pack_season = parent_parse
        .as_ref()
        .filter(|p| p.marker == Some(Marker::SeasonOnly))
        .and_then(|p| p.season);
    let season_hint = l.season.map(|(_, n)| n).or(pack_season);
    let tv_ctx = tv_prior || season_hint.is_some();

    // 3. Extra/sample suffixes, then the stem grammar.
    let (suffix, stem_core) = split_suffix(stem_noid.trim());
    let stacked = stack_evidence(stem_core, siblings);
    let opts = StemOpts {
        tv: tv_ctx,
        stacked,
        folder: false,
        season_hint,
        movie_lib: lib == LibraryKind::Movies,
    };
    let mut sp = stem::parse_stem(stem_core, opts);

    // An obfuscated or bare filename inside a release folder
    // ("We.Bare.Bears.S01E14…-TVSmash/mxNMuJWe….mkv"): parse the folder.
    let file_blank = sp.marker.is_none() && sp.year.is_none() && sp.tags == 0;
    if let Some(pp) = parent_parse
        .as_ref()
        .filter(|p| file_blank && p.has_episode_info())
    {
        ev.push(format!(
            "filename has no structure; parsed release folder '{}'",
            parent.map_or("", |d| &d.name)
        ));
        sp = pp.clone();
    }

    let ctx = Context {
        path: comps.join("/"),
        parent: dirs.join("/"),
        stem: stem_core.to_string(),
        season_folder: l.season.map(|(_, n)| n),
        lib,
    };

    let specials_as_season =
        l.specials.is_some() && (tv_ctx || sp.has_episode_info() || lib != LibraryKind::Movies);
    let season_folder = l
        .season
        .map(|(_, n)| n)
        .or(if specials_as_season { Some(0) } else { None });
    let extra_kind = l.extras.map(|(_, k)| k).or(match suffix {
        Some(Suffix::Extra(k)) => Some(k),
        _ => None,
    });
    let extra_kind = extra_kind.or(if l.specials.is_some() && !specials_as_season {
        Some(ExtraKind::Other)
    } else {
        None
    });
    let is_sample = l.sample || suffix == Some(Suffix::Sample);

    // Show folder, for episodes and show extras.
    let (show_dir, show_strong) = show_folder(&dir_list, &l, lib);
    let show_parse = show_dir.map(|i| stem::parse_stem(&dir_list[i].name, folder_opts(true)));

    // Bare episode numbers in a season folder ("03 - Title.mkv", "Show 03.mkv").
    if !sp.has_episode_info() && (season_folder.is_some() || tv_prior) && !is_sample && extra_kind.is_none() {
        bare_episode(stem_core, &mut sp, show_parse.as_ref(), &mut ev);
    }

    if is_sample {
        sample_guess(&mut g, &sp, &dir_list, &l, &mut ev);
    } else if let Some(kind) = extra_kind {
        extra_guess(
            &mut g,
            kind,
            &sp,
            &dir_list,
            &l,
            suffix,
            show_parse.as_ref(),
            season_folder,
            &mut ev,
        );
    } else if sp.has_episode_info() || sp.marker == Some(Marker::SeasonOnly) {
        let show = ShowFolder {
            parse: show_parse.as_ref(),
            name: show_dir.map(|i| dir_list[i].name.as_str()),
            strong: show_strong,
        };
        episode_guess(&mut g, &sp, show, season_folder, &l, &mut ev);
    } else if lib == LibraryKind::Shows || (tv_prior && l.season.is_some()) {
        // A shows library file with no episode numbers: keep what we know.
        g.kind = GuessKind::Unknown;
        g.title = show_parse
            .as_ref()
            .and_then(|p| p.title.clone())
            .or_else(|| sp.title.clone());
        g.year = show_parse.as_ref().and_then(|p| p.year).or(sp.year);
        g.season = season_folder;
        g.confidence = 0.2;
        ev.push("shows library: no episode number found".into());
    } else {
        let owner = l.owner.map(|i| {
            (
                &dir_list[i],
                stem::parse_stem(&dir_list[i].name, folder_opts(false)),
            )
        });
        movie_guess(&mut g, &sp, owner, &l, edition_tag.clone(), &mut ev);
    }

    if g.kind != GuessKind::Extra && g.kind != GuessKind::Sample {
        if let Some(ed) = edition_tag {
            ev.push(format!("edition tag {{edition-{ed}}}"));
            g.edition = Some(ed);
        }
    }
    copy_tags(&mut g, &sp);
    if !g.ids.is_empty() {
        g.confidence = g.confidence.max(0.95);
    }
    g.evidence.append(&mut ev);
    Resolved { guess: g, ctx }
}

fn folder_opts(tv: bool) -> StemOpts {
    StemOpts {
        tv,
        stacked: false,
        folder: true,
        season_hint: None,
        movie_lib: false,
    }
}

fn layout(dirs: &[Dir]) -> Layout {
    let mut l = Layout::default();
    for (i, d) in dirs.iter().enumerate().rev() {
        match d.role {
            FolderRole::Season(n) if l.season.is_none() => l.season = Some((i, n)),
            FolderRole::Specials if l.specials.is_none() && l.season.is_none() => l.specials = Some(i),
            FolderRole::Extras(k) if l.extras.is_none() && l.owner.is_none() => l.extras = Some((i, k)),
            FolderRole::Sample if l.owner.is_none() => l.sample = true,
            FolderRole::Disc if l.owner.is_none() => l.disc = true,
            FolderRole::Part(n) if l.part.is_none() && l.owner.is_none() => l.part = Some(n),
            FolderRole::Generic(GenericHint::None) => {}
            // Walking upward, so the top-most generic hint wins.
            FolderRole::Generic(h) => l.hint = Some(h),
            FolderRole::Plain => {
                if l.owner.is_none() {
                    l.owner = Some(i);
                }
                l.top = Some(i);
            }
            _ => {}
        }
    }
    l
}

/// Nearest plain folder strictly above `idx`.
fn plain_above(dirs: &[Dir], idx: usize) -> Option<usize> {
    (0..idx).rev().find(|&i| dirs[i].role == FolderRole::Plain)
}

/// The show folder and whether it is *strong* evidence: above a
/// season/specials/extras folder, or the first folder under a shows library
/// root (Jellyfin's layout rule). Otherwise the nearest plain folder, which
/// is only a weak hint (`/av/unsorted/Show.2015.07.22.mkv`).
fn show_folder(dirs: &[Dir], l: &Layout, lib: LibraryKind) -> (Option<usize>, bool) {
    let special = l
        .season
        .map(|(i, _)| i)
        .or(l.specials)
        .or(l.extras.map(|(i, _)| i));
    if let Some(i) = special.and_then(|i| plain_above(dirs, i)) {
        return (Some(i), true);
    }
    if lib == LibraryKind::Shows && l.top.is_some() {
        return (l.top, true);
    }
    (l.owner, false)
}

fn split_suffix(stem: &str) -> (Option<Suffix>, &str) {
    let lower = stem.to_ascii_lowercase();
    if matches!(lower.as_str(), "sample") || lower.starts_with("sample-") || lower.starts_with("sample.") {
        return (Some(Suffix::Sample), stem);
    }
    if lower == "trailer" {
        return (Some(Suffix::Extra(ExtraKind::Trailer)), stem);
    }
    let Some(c) = SUFFIX_RE.captures(stem) else {
        return (None, stem);
    };
    let Some(whole) = c.get(0) else {
        return (None, stem);
    };
    let word = c
        .get(1)
        .or_else(|| c.get(2))
        .map(|m| m.as_str().to_ascii_lowercase())
        .unwrap_or_default();
    let kind = match word.as_str() {
        "sample" => Suffix::Sample,
        "trailer" => Suffix::Extra(ExtraKind::Trailer),
        "featurette" => Suffix::Extra(ExtraKind::Featurette),
        "behindthescenes" => Suffix::Extra(ExtraKind::BehindTheScenes),
        "deleted" | "deletedscene" => Suffix::Extra(ExtraKind::DeletedScene),
        "interview" => Suffix::Extra(ExtraKind::Interview),
        "scene" => Suffix::Extra(ExtraKind::Scene),
        "short" => Suffix::Extra(ExtraKind::Short),
        _ => Suffix::Extra(ExtraKind::Other),
    };
    (Some(kind), &stem[..whole.start()])
}

/// `true` when a sibling differs from this stem only in its part number.
fn stack_evidence(stem: &str, siblings: &[&str]) -> bool {
    let Some(c) = STACK_RE.captures(stem) else {
        return false;
    };
    let prefix = c[1].to_lowercase();
    let num = &c[2];
    siblings.iter().any(|sib| {
        let (s, _) = split_ext(sib);
        STACK_RE
            .captures(s)
            .is_some_and(|c2| c2[1].to_lowercase() == prefix && &c2[2] != num)
    })
}

fn bare_episode(stem_core: &str, sp: &mut Stem, show: Option<&Stem>, ev: &mut Vec<String>) {
    if let Some(c) = BARE_EP_RE.captures(stem_core.trim()) {
        if let Ok(ep) = c[1].parse::<u16>() {
            sp.episodes = vec![ep];
            sp.marker = Some(Marker::EpisodeOnly);
            sp.title = None;
            sp.year = None;
            sp.episode_title = c
                .get(2)
                .map(|m| trim_seps(m.as_str()).to_string())
                .filter(|s| !s.is_empty());
            ev.push(format!("bare episode number {ep} in a season folder"));
            return;
        }
    }
    // "Show Name 03.mkv" / "Neverwhere.05.Down.Street.avi": the show
    // folder's title, then a 1–3 digit number, then maybe an episode title.
    let (Some(title), Some(show_title)) = (sp.title.clone(), show.and_then(|s| s.title.as_deref())) else {
        return;
    };
    let words: Vec<&str> = title.split(' ').collect();
    let show_key = norm_key(show_title);
    for k in 1..words.len() {
        let w = words[k];
        if norm_key(&words[..k].join(" ")) != show_key {
            continue;
        }
        if (1..=3).contains(&w.len()) && w.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(ep) = w.parse::<u16>() {
                sp.episodes = vec![ep];
                sp.marker = Some(Marker::EpisodeOnly);
                sp.title = Some(words[..k].join(" "));
                let rest = words[k + 1..].join(" ");
                sp.episode_title = (!rest.is_empty()).then_some(rest);
                ev.push(format!("episode number {ep} after the show title"));
            }
        }
        return;
    }
}

fn sample_guess(g: &mut NameGuess, sp: &Stem, dirs: &[Dir], l: &Layout, ev: &mut Vec<String>) {
    g.kind = GuessKind::Sample;
    ev.push(
        if l.sample {
            "sample: Sample/ folder"
        } else {
            "sample: -sample suffix"
        }
        .to_string(),
    );
    let owner = l
        .owner
        .map(|i| stem::parse_stem(&dirs[i].name, folder_opts(false)));
    let src = owner.as_ref().filter(|o| o.title.is_some()).unwrap_or(sp);
    g.title = src.title.clone();
    g.year = src.year;
    g.confidence = 0.95;
}

#[allow(clippy::too_many_arguments)]
fn extra_guess(
    g: &mut NameGuess,
    kind: ExtraKind,
    sp: &Stem,
    dirs: &[Dir],
    l: &Layout,
    suffix: Option<Suffix>,
    show: Option<&Stem>,
    season_folder: Option<u16>,
    ev: &mut Vec<String>,
) {
    g.kind = GuessKind::Extra;
    g.extra = Some(kind);
    let from_folder = l.extras.is_some() || (l.specials.is_some() && suffix.is_none());
    if from_folder {
        let folder_idx = l.extras.map(|(i, _)| i).or(l.specials);
        ev.push(format!(
            "extra: {} folder",
            folder_idx.map_or("extras", |i| dirs[i].name.as_str())
        ));
        // The extra's own name ("Making Of"), the owner from the folder above.
        g.episode_title = sp.title.clone();
        let owner = folder_idx
            .and_then(|i| plain_above(dirs, i))
            .map(|i| stem::parse_stem(&dirs[i].name, folder_opts(false)));
        let owner = owner.filter(|o| o.title.is_some()).or_else(|| show.cloned());
        match owner {
            Some(o) => {
                g.title = o.title;
                g.year = o.year;
                g.confidence = 0.85;
            }
            None => g.confidence = 0.5,
        }
    } else {
        ev.push(format!("extra: -{kind:?} suffix").to_lowercase());
        let owner = l
            .owner
            .map(|i| stem::parse_stem(&dirs[i].name, folder_opts(false)));
        let src = if sp.title.is_some() {
            Some(sp)
        } else {
            owner.as_ref()
        };
        g.title = src.and_then(|s| s.title.clone());
        g.year = src.and_then(|s| s.year).or(owner.as_ref().and_then(|o| o.year));
        g.confidence = if g.title.is_some() { 0.85 } else { 0.5 };
    }
    g.season = season_folder;
}

/// The show folder chosen by [`show_folder`], parsed.
struct ShowFolder<'a> {
    parse: Option<&'a Stem>,
    name: Option<&'a str>,
    strong: bool,
}

fn episode_guess(
    g: &mut NameGuess,
    sp: &Stem,
    show_folder: ShowFolder<'_>,
    season_folder: Option<u16>,
    l: &Layout,
    ev: &mut Vec<String>,
) {
    let show = show_folder.parse;
    g.kind = GuessKind::Episode;
    let marker = sp.marker.unwrap_or(Marker::EpisodeOnly);
    ev.push(format!("episode marker: {}", marker.label()));
    let mut conf: f32 = match marker {
        Marker::SxxEyy | Marker::NxNN | Marker::Words => 0.85,
        Marker::Date => 0.8,
        Marker::Absolute | Marker::Packed => 0.75,
        Marker::EpisodeOnly => 0.65,
        // A season pack file: no episode number; needs a human or AI.
        Marker::SeasonOnly => 0.4,
    };

    // Show title: a strong show folder wins; a weak one (just the parent
    // folder in a mixed library) only fills in when the filename has none.
    let folder_title = show.and_then(|s| s.title.clone());
    let folder_name = show_folder
        .name
        .or(folder_title.as_deref())
        .unwrap_or_default()
        .to_string();
    match (&folder_title, &sp.title) {
        (Some(ft), file_title)
            if show_folder.strong
                || file_title.as_deref().is_none_or(|tt| {
                    // "Kaamelott/Kaamelott - Livre V - Ep 23": the folder is the
                    // cleaner show name.
                    let (f, t) = (norm_key(ft), norm_key(tt));
                    !f.is_empty() && t.starts_with(&f)
                }) =>
        {
            ev.push(format!("show from folder '{folder_name}'"));
            g.title = Some(ft.clone());
            conf += if show_folder.strong { 0.1 } else { 0.0 };
            if let Some(tt) = file_title {
                let (a, b) = (norm_key(ft), norm_key(tt));
                if !a.starts_with(&b) && !b.starts_with(&a) {
                    ev.push(format!("filename says '{tt}'"));
                    conf -= 0.05;
                }
            }
        }
        (folder, Some(tt)) => {
            ev.push("show from filename".into());
            if folder.as_deref().is_some_and(|ft| norm_key(ft) == norm_key(tt)) {
                ev.push(format!("folder '{folder_name}' agrees"));
                conf += 0.05;
            }
            g.title = Some(tt.clone());
        }
        (_, None) => {
            ev.push("no show title".into());
            conf = 0.3;
        }
    }
    g.year = show.and_then(|s| s.year).or(sp.year);

    // Season: the filename marker, then the season folder, then a season-pack folder.
    let pack_season = show.and_then(|s| s.season);
    g.season = sp.season.or(season_folder).or(pack_season);
    if let (Some(fs), Some(ff)) = (sp.season, season_folder) {
        if fs != ff {
            ev.push(format!("season folder says {ff}, filename says {fs}"));
            conf -= 0.1;
        }
    } else if sp.season.is_none() && season_folder.is_some() && l.season.is_some() {
        ev.push("season from folder".into());
    }

    g.episodes = sp.episodes.clone();
    g.air_date = sp.air_date.clone();
    if let Some(abs) = sp.absolute {
        g.absolute_episode = Some(abs);
        if season_folder.is_some() {
            // "[Group] Show - 05" inside "Season 2": probably season-relative.
            if let Ok(e) = u16::try_from(abs) {
                g.episodes = vec![e];
            }
            ev.push("anime number inside a season folder: treated as season-relative too".into());
        } else {
            g.season = None;
        }
    }
    if marker == Marker::SeasonOnly {
        ev.push("season pack: no episode number".into());
    }
    g.episode_title = sp.episode_title.clone();
    g.confidence = conf;
}

fn movie_guess(
    g: &mut NameGuess,
    sp: &Stem,
    owner: Option<(&Dir, Stem)>,
    l: &Layout,
    edition_tag: Option<String>,
    ev: &mut Vec<String>,
) {
    g.kind = GuessKind::Movie;
    let file_junk = sp.title.as_deref().is_none_or(|t| JUNK_STEM_RE.is_match(t));
    let (owner_dir, folder) = match owner {
        Some((d, p)) if p.title.is_some() && !p.has_episode_info() => (Some(d), Some(p)),
        _ => (None, None),
    };
    // The folder wins for disc structures, junk filenames, a file without a
    // year in a "Title (Year)" folder, and a bare/obfuscated file inside a
    // tagged release folder ("Alice in Wonderland DVDRip.XviD-DiAMOND/dmd-aw.avi").
    let file_bare = sp.year.is_none() && sp.tags == 0;
    let use_folder = folder.as_ref().is_some_and(|f| {
        l.disc || file_junk || (sp.year.is_none() && f.year.is_some()) || (file_bare && f.tags > 0)
    });

    let mut conf: f32;
    if let (true, Some(f)) = (use_folder, folder.as_ref()) {
        ev.push(format!(
            "title from movie folder '{}'",
            owner_dir.map_or("", |d| d.name.as_str())
        ));
        if l.disc {
            ev.push("disc structure (BDMV/VIDEO_TS)".into());
        }
        g.title = f.title.clone();
        g.year = f.year;
        conf = match (f.year, f.year_bracketed) {
            (Some(_), true) => 0.9,
            (Some(_), false) => 0.85,
            (None, _) => 0.45,
        };
        g.edition = f.edition.clone();
        g.part = f.part;
        if f.year_like_in_title.is_some() {
            conf -= 0.05;
        }
    } else {
        g.title = sp.title.clone();
        g.year = sp.year;
        conf = match (sp.title.is_some(), sp.year, sp.year_bracketed) {
            (false, _, _) => 0.1,
            (true, Some(_), true) => 0.9,
            (true, Some(_), false) => 0.85,
            (true, None, _) if sp.tags > 0 => 0.45,
            (true, None, _) => 0.4,
        };
        if file_junk && sp.title.is_some() {
            conf = conf.min(0.3);
        }
        if sp.trailing_number {
            ev.push("title ends with a bare number (sequel or episode?)".into());
            conf = conf.min(0.45);
        }
        if let Some(y) = sp.year_like_in_title {
            ev.push(format!("kept year-like {y} in the title (a later year wins)"));
            conf -= 0.05;
        }
        match &folder {
            Some(f) if f.title.as_deref().map(norm_key) == g.title.as_deref().map(norm_key) => {
                ev.push("movie folder agrees".into());
                conf += 0.05;
                if g.year.is_none() {
                    g.year = f.year;
                }
            }
            Some(f) if f.year.is_some() && f.year == g.year => {
                ev.push(format!(
                    "movie folder title differs: '{}'",
                    f.title.as_deref().unwrap_or("")
                ));
            }
            _ => {}
        }
    }
    if g.title.is_some() {
        ev.push(match (g.year, use_folder) {
            (Some(_), _) if sp.year_bracketed || folder.as_ref().is_some_and(|f| f.year_bracketed) => {
                "fast path: Title (Year)".to_string()
            }
            (Some(_), _) => "fast path: Title.Year".to_string(),
            (None, _) if sp.tags > 0 => "fast path: title before tags, no year".to_string(),
            (None, _) => "fast path: title only".to_string(),
        });
    }
    if let Some(alt) = &sp.alt_title {
        ev.push(format!("alternative title '({alt})' dropped from the title"));
    }
    if sp.ignored_episode_marker {
        ev.push("episode-like words followed by a year: kept in the movie title".into());
    }
    if let Some(e) = sp.edition.clone() {
        if sp.version_label {
            ev.push(format!("version label '{e}'"));
        }
        g.edition = Some(e);
    }
    if g.edition.is_none() && edition_tag.is_none() {
        g.edition = owner_dir.and_then(|d| d.edition.clone());
    }
    g.part = sp.part.or(g.part).or(l.part);
    if let Some(p) = g.part {
        ev.push(format!("multi-part: part {p}"));
    }
    // Tags from the release folder when the file itself has none.
    if let Some(f) = folder {
        g.resolution = f.resolution;
        g.source = f.source;
        g.video_codec = f.video_codec;
        g.release_group = f.release_group;
    }
    g.confidence = conf.clamp(0.0, 1.0);
}

/// File tags override folder tags.
fn copy_tags(g: &mut NameGuess, sp: &Stem) {
    if sp.resolution.is_some() {
        g.resolution = sp.resolution.clone();
    }
    if sp.source.is_some() {
        g.source = sp.source.clone();
    }
    if sp.video_codec.is_some() {
        g.video_codec = sp.video_codec.clone();
    }
    if sp.release_group.is_some() {
        g.release_group = sp.release_group.clone();
    }
}
