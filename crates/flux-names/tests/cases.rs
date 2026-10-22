//! Behavioural tests: the 36 research hard cases plus flux's own regression
//! cases for every layout the parser claims to support.

use std::path::Path;

use flux_names::hard_cases::CASES;
use flux_names::{parse, parse_with, ExtraKind, Fallback, GuessKind, LibraryKind, NameGuess, ParseOptions};

fn p(path: &str, kind: LibraryKind) -> NameGuess {
    parse(Path::new(path), &[], kind)
}

fn movies(path: &str) -> NameGuess {
    p(path, LibraryKind::Movies)
}

fn shows(path: &str) -> NameGuess {
    p(path, LibraryKind::Shows)
}

#[track_caller]
fn assert_movie(g: &NameGuess, title: &str, year: Option<u16>) {
    assert_eq!(g.kind, GuessKind::Movie, "{g:#?}");
    assert_eq!(g.title.as_deref(), Some(title), "{g:#?}");
    assert_eq!(g.year, year, "{g:#?}");
}

#[track_caller]
fn assert_episode(g: &NameGuess, title: &str, season: Option<u16>, episodes: &[u16]) {
    assert_eq!(g.kind, GuessKind::Episode, "{g:#?}");
    assert_eq!(g.title.as_deref(), Some(title), "{g:#?}");
    assert_eq!(g.season, season, "{g:#?}");
    assert_eq!(g.episodes, episodes, "{g:#?}");
}

#[test]
fn research_hard_cases_all_pass() {
    let mut failures = Vec::new();
    for c in CASES {
        let g = p(c.input, LibraryKind::Mixed);
        if !c.score(&g).all() {
            failures.push(format!(
                "{} → {:?} {:?} S{:?} E{:?} abs {:?}",
                c.input, g.title, g.year, g.season, g.episodes, g.absolute_episode
            ));
        }
    }
    // Target ≥ 33/36 (research/02 §6.2: hunch 33/36 titles); we hold 36/36.
    assert!(
        failures.is_empty(),
        "hard-case failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn hard_cases_pass_without_fallback() {
    for c in CASES {
        let g = parse_with(
            Path::new(c.input),
            &[],
            LibraryKind::Mixed,
            ParseOptions {
                fallback: Fallback::Never,
            },
        );
        assert!(c.score(&g).all(), "{}: {g:#?}", c.input);
    }
}

#[test]
fn explicit_ids() {
    let g = movies("The Matrix (1999) [tmdbid-603]/The Matrix (1999) [tmdbid-603].mkv");
    assert_movie(&g, "The Matrix", Some(1999));
    assert_eq!(g.ids.tmdb, Some(603));
    assert!(g.confidence >= 0.95);
    let g = movies("The Matrix (1999) {imdb-tt0133093}/movie.mkv");
    assert_eq!(g.ids.imdb.as_deref(), Some("tt0133093"));
    assert_eq!(g.title.as_deref(), Some("The Matrix"));
    let g = shows("Breaking Bad (2008) {tvdb-81189}/Season 01/Breaking Bad - S01E01 - Pilot.mkv");
    assert_eq!(g.ids.tvdb, Some(81189));
    assert_episode(&g, "Breaking Bad", Some(1), &[1]);
    assert_eq!(g.episode_title.as_deref(), Some("Pilot"));
}

#[test]
fn canonical_movie_layouts() {
    let g = movies("Alien (1979)/Alien (1979) {edition-Director's Cut}.mkv");
    assert_movie(&g, "Alien", Some(1979));
    assert_eq!(g.edition.as_deref(), Some("Director's Cut"));
    let g = movies("Dune (2021)/Dune (2021) - 2160p.mkv");
    assert_movie(&g, "Dune", Some(2021));
    assert_eq!(
        (g.resolution.as_deref(), g.edition.as_deref()),
        (Some("2160p"), None)
    );
    let g = movies("Kill Bill (2003)/Kill Bill (2003) - cd2.avi");
    assert_eq!(g.part, Some(2));
    let g = movies("Blade Runner (1982)/Blade Runner (1982) - Final Cut.mkv");
    assert_eq!(g.edition.as_deref(), Some("Final Cut"));
    // A file without a year takes the movie folder's title and year.
    let g = movies("Heat (1995)/heat.1080p.mkv");
    assert_movie(&g, "Heat", Some(1995));
    // Disc structures.
    let g = movies("Inception (2010)/BDMV/STREAM/00001.m2ts");
    assert_movie(&g, "Inception", Some(2010));
    let g = movies("Inception (2010)/VIDEO_TS/VTS_01_1.VOB");
    assert_movie(&g, "Inception", Some(2010));
    // Multi-part folders.
    let g = movies("Heat (1995)/CD2/heat.avi");
    assert_eq!((g.title.as_deref(), g.part), (Some("Heat"), Some(2)));
}

#[test]
fn year_like_titles() {
    assert_movie(&movies("1917 (2019)/1917 (2019).mkv"), "1917", Some(2019));
    assert_movie(&movies("1917.mkv"), "1917", None);
    assert_movie(&movies("2012.2009.1080p.BluRay.x264.mkv"), "2012", Some(2009));
    assert_movie(
        &movies("blade.runner.2049.2017.mkv"),
        "Blade Runner 2049",
        Some(2017),
    );
    assert_movie(
        &movies("2001.a.space.odyssey.1968.ts"),
        "2001 A Space Odyssey",
        Some(1968),
    );
    assert_movie(
        &movies("2001 - A Space Odyssey 1968.mkv"),
        "2001 - A Space Odyssey",
        Some(1968),
    );
    assert_movie(&movies("Se7en (1995)/Se7en (1995).mkv"), "Se7en", Some(1995));
    assert_movie(&movies("WALL·E (2008)/WALL·E (2008).mkv"), "WALL·E", Some(2008));
    let g = shows("9-1-1/Season 7/9-1-1 - S07E01 - Pilot.mkv");
    assert_episode(&g, "9-1-1", Some(7), &[1]);
}

#[test]
fn part_words_stay_in_titles() {
    assert_movie(
        &movies("Dune.Part.Two.2024.2160p.WEB-DL.mkv"),
        "Dune Part Two",
        Some(2024),
    );
    assert_movie(
        &movies("Dune - Part Two (2024)/Dune - Part Two (2024).mkv"),
        "Dune - Part Two",
        Some(2024),
    );
    assert_movie(
        &movies("The.Godfather.Part.II.1974.1080p.mkv"),
        "The Godfather Part II",
        Some(1974),
    );
    let g = movies("Harry Potter and the Deathly Hallows Part 1 (2010).mkv");
    assert_movie(&g, "Harry Potter and the Deathly Hallows Part 1", Some(2010));
    assert_eq!(g.part, None);
    // Stacked parts without a year need sibling evidence.
    let sibs = ["Some Movie part2.avi"];
    let g = parse(Path::new("Some Movie part1.avi"), &sibs, LibraryKind::Movies);
    assert_eq!((g.title.as_deref(), g.part), (Some("Some Movie"), Some(1)));
    // Without siblings the fast path keeps "part1" in the (low-confidence)
    // title; the fallback may read it as a part.
    let g = parse_with(
        Path::new("Some Movie part1.avi"),
        &[],
        LibraryKind::Movies,
        ParseOptions {
            fallback: Fallback::Never,
        },
    );
    assert_eq!(g.part, None);
    assert!(g.confidence < 0.5);
}

#[test]
fn unicode_titles() {
    assert_movie(
        &movies("千と千尋の神隠し (2001)/千と千尋の神隠し (2001).mkv"),
        "千と千尋の神隠し",
        Some(2001),
    );
    assert_movie(&movies("기생충 (2019)/기생충 (2019).mp4"), "기생충", Some(2019));
    assert_movie(&movies("Amélie (2001)/Amélie (2001).mkv"), "Amélie", Some(2001));
    // NFD on disk (HFS+ heritage) comes back as NFC.
    assert_movie(
        &movies("Ame\u{301}lie (2001)/Ame\u{301}lie (2001).mkv"),
        "Amélie",
        Some(2001),
    );
    assert_movie(
        &movies("Léon - The Professional (1994)/Léon - The Professional (1994).mkv"),
        "Léon - The Professional",
        Some(1994),
    );
    assert_movie(
        &movies("Le.Fabuleux.Destin.dAmélie.Poulain.2001.1080p.BluRay.x264-GRP.mkv"),
        "Le Fabuleux Destin dAmélie Poulain",
        Some(2001),
    );
    assert_movie(
        &movies("Your Name. (2016)/Your Name. (2016).mkv"),
        "Your Name.",
        Some(2016),
    );
    assert_movie(
        &movies("E.T.the.Extra-Terrestrial.1982.1080p.mkv"),
        "E.T. the Extra-Terrestrial",
        Some(1982),
    );
    assert_movie(
        &movies("Ocean's Eleven (2001)/Ocean's Eleven (2001).mkv"),
        "Ocean's Eleven",
        Some(2001),
    );
}

#[test]
fn canonical_show_layouts() {
    let g = shows("Severance (2022)/Season 02/Severance (2022) - S02E10 - Cold Harbor.mkv");
    assert_episode(&g, "Severance", Some(2), &[10]);
    assert_eq!(
        (g.year, g.episode_title.as_deref()),
        (Some(2022), Some("Cold Harbor"))
    );
    assert!(g.confidence >= 0.9);
    assert_episode(
        &shows("The Expanse/Season 3/The Expanse - S03E01-E02 - Fight or Flight.mkv"),
        "The Expanse",
        Some(3),
        &[1, 2],
    );
    assert_episode(
        &shows("Show/S01/Show - S01E01-E03.mkv"),
        "Show",
        Some(1),
        &[1, 2, 3],
    );
    assert_episode(
        &shows("Sherlock/Specials/Sherlock - S00E01 - The Abominable Bride.mkv"),
        "Sherlock",
        Some(0),
        &[1],
    );
    assert_episode(
        &shows("Sherlock/Season 00/Sherlock - S00E02.mkv"),
        "Sherlock",
        Some(0),
        &[2],
    );
    assert_episode(
        &shows("Friends/Season 1/Friends 1x03.avi"),
        "Friends",
        Some(1),
        &[3],
    );
    assert_episode(&shows("Fargo (2014)/Fargo.S01E02.mkv"), "Fargo", Some(1), &[2]);
    // Folder title wins over a scene filename's title.
    assert_episode(
        &shows("The Office (US)/Season 2/The.Office.S02E01.720p.mkv"),
        "The Office (US)",
        Some(2),
        &[1],
    );
    // Bare episode numbers inside a season folder.
    assert_episode(
        &shows("Doctor Who/Season 06/E13 - The Wedding of River Song.mkv"),
        "Doctor Who",
        Some(6),
        &[13],
    );
    assert_episode(
        &shows("Doctor Who/Season 06/03 - Title.mkv"),
        "Doctor Who",
        Some(6),
        &[3],
    );
    assert_episode(
        &shows("Neverwhere/Neverwhere.05.Down.Street.avi"),
        "Neverwhere",
        None,
        &[5],
    );
    // Packed season+episode numbers in a season folder.
    assert_episode(
        &shows("Show Name/Season 1/show.name.106.720p.mkv"),
        "Show Name",
        Some(1),
        &[6],
    );
}

#[test]
fn dates_and_anime() {
    let g = shows("The Daily Show/The.Daily.Show.2024.03.15.Guest.Name.720p.WEB.h264.mkv");
    assert_eq!(g.kind, GuessKind::Episode);
    assert_eq!(
        (g.title.as_deref(), g.air_date.as_deref(), g.year),
        (Some("The Daily Show"), Some("2024-03-15"), None)
    );
    let g = shows("Jeopardy/Jeopardy 2024-03-15.mkv");
    assert_eq!(g.air_date.as_deref(), Some("2024-03-15"));
    let g = shows("Frieren/[SubsPlease] Sousou no Frieren - 12 (1080p) [ABCDEF12].mkv");
    assert_eq!(
        (g.kind, g.title.as_deref(), g.absolute_episode, g.season),
        (GuessKind::Episode, Some("Frieren"), Some(12), None)
    );
    let g = p(
        "[Erai-raws] One Piece - 1100 [1080p][Multiple Subtitle].mkv",
        LibraryKind::Mixed,
    );
    assert_eq!(
        (g.title.as_deref(), g.absolute_episode),
        (Some("One Piece"), Some(1100))
    );
    assert_eq!(g.release_group.as_deref(), Some("Erai-raws"));
    // Inside a season folder the anime number is read as season-relative too.
    let g = shows("Show/Season 2/[Group] Show - 05 [1080p].mkv");
    assert_eq!(
        (g.season, g.episodes.as_slice(), g.absolute_episode),
        (Some(2), &[5][..], Some(5))
    );
    // Fansub names without a dash.
    let g = p(
        "[Evil-Saizen]_Laughing_Salesman_14_[DVD][1C98686A].mkv",
        LibraryKind::Mixed,
    );
    assert_eq!(
        (g.title.as_deref(), g.absolute_episode),
        (Some("Laughing Salesman"), Some(14))
    );
}

#[test]
fn extras_and_samples() {
    let g = movies("Knives Out (2019)/Featurettes/Making Of.mkv");
    assert_eq!((g.kind, g.extra), (GuessKind::Extra, Some(ExtraKind::Featurette)));
    assert_eq!(
        (g.title.as_deref(), g.year, g.episode_title.as_deref()),
        (Some("Knives Out"), Some(2019), Some("Making Of"))
    );
    for (folder, kind) in [
        ("Behind The Scenes", ExtraKind::BehindTheScenes),
        ("Deleted Scenes", ExtraKind::DeletedScene),
        ("Interviews", ExtraKind::Interview),
        ("Trailers", ExtraKind::Trailer),
        ("Shorts", ExtraKind::Short),
        ("Scenes", ExtraKind::Scene),
        ("Extras", ExtraKind::Other),
    ] {
        let g = movies(&format!("Alien (1979)/{folder}/Something.mkv"));
        assert_eq!(
            (g.kind, g.extra, g.title.as_deref()),
            (GuessKind::Extra, Some(kind), Some("Alien")),
            "{folder}"
        );
    }
    let g = movies("Alien (1979)/Alien (1979)-trailer.mkv");
    assert_eq!(
        (g.kind, g.extra, g.title.as_deref()),
        (GuessKind::Extra, Some(ExtraKind::Trailer), Some("Alien"))
    );
    let g = movies("Alien (1979)/Alien (1979)-featurette.mkv");
    assert_eq!(g.extra, Some(ExtraKind::Featurette));
    let g = shows("Chernobyl/Extras/Inside the Episode.mkv");
    assert_eq!(
        (g.kind, g.title.as_deref()),
        (GuessKind::Extra, Some("Chernobyl"))
    );
    let g = movies(
        "The.Matrix.1999.1080p.BluRay.x264-GRP/Sample/the.matrix.1999.1080p.bluray.x264-grp-sample.mkv",
    );
    assert_eq!(
        (g.kind, g.title.as_deref()),
        (GuessKind::Sample, Some("The Matrix"))
    );
    let g = movies("Movie.2019.1080p.WEB-DL-GRP/movie.2019.1080p-sample.mkv");
    assert_eq!(g.kind, GuessKind::Sample);
    // "The Interview" is a movie, not an extra.
    assert_movie(
        &movies("The Interview (2014)/The Interview (2014).mkv"),
        "The Interview",
        Some(2014),
    );
    // Specials in a movie folder are bonus material.
    let g = movies("Alien (1979)/Specials/Alien Anthology.mkv");
    assert_eq!(g.kind, GuessKind::Extra);
}

#[test]
fn library_kind_is_a_prior() {
    // A bare trailing number is a sequel number in a movie library…
    assert_movie(&movies("Toy.Story.3.720p.BluRay.x264.mkv"), "Toy Story 3", None);
    assert_movie(&movies("Fahrenheit.451.HDTV.x264.mkv"), "Fahrenheit 451", None);
    // …and an episode in a shows library.
    assert_episode(&shows("FooBar/FooBar.07.720p.mkv"), "FooBar", None, &[7]);
    // An explicit marker is an episode everywhere.
    assert_episode(&movies("Show.Name.1x03.mkv"), "Show Name", Some(1), &[3]);
}

#[test]
fn release_folders_and_obfuscation() {
    let g = p(
        "We.Bare.Bears.S01E14.Brother.Up.1080p.WEB-DL.AAC2.0.H.264-TVSmash/mxNMuJWeO7PUWCMEwqKSsS6D8Vs9S6V3PHD.mkv",
        LibraryKind::Mixed,
    );
    assert_episode(&g, "We Bare Bears", Some(1), &[14]);
    let g = movies("Alice in Wonderland DVDRip.XviD-DiAMOND/dmd-aw.avi");
    assert_eq!(g.title.as_deref(), Some("Alice in Wonderland"));
    // Season packs: the folder's season validates packed numbers.
    let g = p(
        "Show.Name.S01.720p.HDTV.DD5.1.x264-Group/show.name.0106.720p-group.mkv",
        LibraryKind::Mixed,
    );
    assert_episode(&g, "Show Name", Some(1), &[6]);
    // A season pack file has a season but no episode.
    let g = p(
        "Game.of.Thrones.S03.1080p.BluRay.DTS-HD.MA.5.1.AVC.REMUX-FraMeSToR.mkv",
        LibraryKind::Mixed,
    );
    assert_eq!(
        (g.kind, g.season, g.episodes.len()),
        (GuessKind::Episode, Some(3), 0)
    );
    assert!(g.confidence < 0.5);
}

#[test]
fn more_episode_markers() {
    let m = |s: &str| p(s, LibraryKind::Mixed);
    assert_episode(
        &m("Show Name - S01E02 - S01E03 - S01E04 - Ep Name.mkv"),
        "Show Name",
        Some(1),
        &[2, 3, 4],
    );
    assert_episode(
        &m("Phineas and Ferb S01E00 & S01E01 & S01E02.mkv"),
        "Phineas and Ferb",
        Some(1),
        &[0, 1, 2],
    );
    assert_episode(
        &m("Show Name 16x03-05 313-315.mkv"),
        "Show Name",
        Some(16),
        &[3, 4, 5],
    );
    assert_episode(&m("The Office - S06xE01.avi"), "The Office", Some(6), &[1]);
    assert_episode(
        &m("Parks_and_Recreation-s03-e01.mkv"),
        "Parks and Recreation",
        Some(3),
        &[1],
    );
    assert_episode(
        &m("Something.Season.2.1of4.Ep.Title.HDTV.mkv"),
        "Something",
        Some(2),
        &[1],
    );
    assert_episode(
        &m("Show.Name.-.Temporada.15.720p.HDTV.x264[Cap.1503]SPANISH.AUDIO-NEWPCT.mkv"),
        "Show Name",
        Some(15),
        &[3],
    );
    let g = m("Mastercook Italia - Stagione 6 (2016) 720p Episodio 13 spyro.mkv");
    assert_episode(&g, "Mastercook Italia", Some(6), &[13]);
    assert_eq!(g.year, Some(2016));
    let g = m("Breaking.Bad.S01E01.2008.BluRay.VC1.1080P.5.1.WMV-NOVO.mkv");
    assert_eq!(g.year, Some(2008));
    let g = m("FlexGet.US.S2013E14.Title.Here.720p.HDTV.AAC5.1.x264-NOGRP.mkv");
    assert_eq!(
        (g.season, g.episodes.as_slice(), g.year),
        (Some(2013), &[14][..], Some(2013))
    );
    let g = m("the.mentalist.501.hdtv-lol.mp4");
    assert_episode(&g, "The Mentalist", Some(5), &[1]);
    // A year-like season after "Series" is not a season.
    assert_movie(&m("The.Series.2019.1080p.BluRay.mkv"), "The Series", Some(2019));
}

#[test]
fn editions_tags_and_alt_titles() {
    let g = movies("Alien DC (1979) [1080p].mkv");
    assert_eq!(
        (g.title.as_deref(), g.edition.as_deref()),
        (Some("Alien"), Some("Director's Cut"))
    );
    let g =
        movies("The.Lord.of.the.Rings.The.Fellowship.of.the.Ring.2001.EXTENDED.1080p.BluRay.x264-GRP.mkv");
    assert_eq!(g.edition.as_deref(), Some("Extended"));
    assert_eq!(g.release_group.as_deref(), Some("GRP"));
    assert_movie(&movies("The Final Cut (2004).mkv"), "The Final Cut", Some(2004));
    let g = movies("Le Prestige (The Prestige) (2006).mkv");
    assert_movie(&g, "Le Prestige", Some(2006));
    assert!(g.evidence.iter().any(|e| e.contains("The Prestige")));
    assert_movie(
        &movies("Charlottes.Web.1973.1080p.BluRay.mkv"),
        "Charlottes Web",
        Some(1973),
    );
    assert_movie(&movies("Step Up 3D (2010).mkv"), "Step Up 3D", Some(2010));
    assert_movie(
        &movies("Perfect Child-2007-TRUEFRENCH-TVRip.Xvid-h@mster.avi"),
        "Perfect Child",
        Some(2007),
    );
    let g = p(
        "Simpsons,.The.12x08.A.Bas.Le.Sergent.Skinner.FR.avi",
        LibraryKind::Mixed,
    );
    assert_eq!(g.title.as_deref(), Some("The Simpsons"));
    let g = movies("Blade Runner 2049 (2017) 2160p UHD BluRay REMUX HDR HEVC Atmos-FGT.mkv");
    assert_eq!(
        (
            g.resolution.as_deref(),
            g.source.as_deref(),
            g.video_codec.as_deref()
        ),
        (Some("2160p"), Some("BluRay"), Some("HEVC"))
    );
}

#[test]
fn windows_paths_and_ignores() {
    let g = p(
        r"D:\TV\Sitcoms\That '70s Show\Season 07\That '70s Show - S07E22 - 2000 Light Years from Home.mkv",
        LibraryKind::Mixed,
    );
    assert_episode(&g, "That '70s Show", Some(7), &[22]);
    assert!(flux_names::is_ignored(Path::new("TV/.DS_Store")));
    assert!(flux_names::is_ignored(Path::new("Movies/@eaDir/x.mkv")));
}

#[test]
fn evidence_and_confidence_explain_decisions() {
    let g = movies("Blade Runner 2049 (2017)/Blade Runner 2049 (2017).mkv");
    assert!(
        g.evidence.iter().any(|e| e.contains("Title (Year)")),
        "{:?}",
        g.evidence
    );
    assert!(
        g.evidence.iter().any(|e| e.contains("folder agrees")),
        "{:?}",
        g.evidence
    );
    let g = movies("Some Random Name.mkv");
    assert!(
        g.confidence < 0.5,
        "a bare title should be queued for AI matching: {g:#?}"
    );
    let g = shows("Show/notes.mkv");
    assert_eq!(g.kind, GuessKind::Unknown);
}

// ---- CJK release names (all names below are made up) ----------------------

fn with_sibs(path: &str, sibs: &[&str], kind: LibraryKind) -> NameGuess {
    parse(Path::new(path), sibs, kind)
}

#[test]
fn chinese_episode_markers() {
    // `<title>_第01集`: the marker is the episode, never part of a title.
    let g = shows("示例剧_第01集.mp4");
    assert_episode(&g, "示例剧", None, &[1]);
    assert_eq!(g.episode_title, None, "{g:#?}");
    assert!(g.confidence >= 0.5, "{g:#?}");
    assert_episode(&shows("示例剧/示例剧_第12集.mp4"), "示例剧", None, &[12]);
    assert_episode(&shows("示例剧/示例剧 第十二集.mkv"), "示例剧", None, &[12]);
    assert_episode(&shows("示例剧/示例剧.第二十三话.mkv"), "示例剧", None, &[23]);
    assert_episode(&shows("示例剧/示例剧 第一百零五回.mkv"), "示例剧", None, &[105]);
    assert_episode(&shows("示例剧/示例剧第０７集.mkv"), "示例剧", None, &[7]);
    assert_episode(&shows("示例剧/示例剧 第 8 集.mkv"), "示例剧", None, &[8]);
    let g = shows("示例剧/示例剧_第01集_开端.mp4");
    assert_eq!(g.episode_title.as_deref(), Some("开端"), "{g:#?}");
    // Seasons: `第N季` in the name or as the season folder.
    assert_episode(&shows("示例剧/示例剧.第2季.第03集.mkv"), "示例剧", Some(2), &[3]);
    assert_episode(&shows("示例剧/示例剧第2季第3话.mkv"), "示例剧", Some(2), &[3]);
    assert_episode(&shows("示例剧/第二季/示例剧_第03集.mp4"), "示例剧", Some(2), &[3]);
    // Also outside a shows library.
    assert_episode(&p("示例剧_第05集.mp4", LibraryKind::Mixed), "示例剧", None, &[5]);
    // Titles that start with 第 but carry no marker stay titles.
    assert_movie(&movies("第一滴血.mkv"), "第一滴血", None);
    assert_movie(&movies("第九区 (2009).mkv"), "第九区", Some(2009));
}

#[test]
fn chinese_release_tags_are_not_titles() {
    let g = shows("Some Show/S01/示例剧.Some.Show.S01E01.中英字幕.BDrip.1080p-某某影视.mp4");
    assert_episode(&g, "Some Show", Some(1), &[1]);
    assert_eq!(g.episode_title, None, "{g:#?}");
    assert_eq!(g.release_group.as_deref(), Some("某某影视"));
    let g = shows("Some Show/Some.Show.S01E02.1080p.中英特效字幕.某某字幕组.mp4");
    assert_episode(&g, "Some Show", Some(1), &[2]);
    assert_eq!(g.episode_title, None, "{g:#?}");
    assert_eq!(g.release_group.as_deref(), Some("某某字幕组"));
    let g = shows("Some Show/Some.Show.S01E03.The.Pilot.官方中文字幕.1080p.mkv");
    assert_eq!(g.episode_title.as_deref(), Some("The Pilot"), "{g:#?}");
    assert_movie(&movies("示例片.2019.国语中字.mkv"), "示例片", Some(2019));
    assert_movie(&movies("示例片.粤语.mkv"), "示例片", None);
    assert_movie(&movies("示例片 简繁双语.mkv"), "示例片", None);
    // A stem that is only such a word keeps it as the title.
    assert_movie(&movies("粤语.mkv"), "粤语", None);
    // A sharing site's prefix, an edition and a glued quality+subtitle tag.
    let g = movies("[某某影视分享example.co]示例片.1048.2007.导演剪辑版.BD720P.高清中英双字.mkv");
    assert_movie(&g, "示例片 1048", Some(2007));
    assert_eq!(g.edition.as_deref(), Some("Director's Cut"));
    assert_eq!(g.release_group, None, "a site is not a release group");
    assert_eq!(g.resolution.as_deref(), Some("720p"));
}

#[test]
fn cjk_and_latin_split_where_the_script_changes() {
    let g = movies("示例片4k.mp4");
    assert_movie(&g, "示例片", None);
    assert_eq!(g.resolution.as_deref(), Some("2160p"));
    assert_movie(&movies("示例片1080p.x265.mkv"), "示例片", None);
    // A number glued to a CJK title is a sequel number, not an episode the
    // fast path reads (even in an episode context).
    let g = parse_with(
        Path::new("示例片2.mkv"),
        &[],
        LibraryKind::Shows,
        ParseOptions {
            fallback: Fallback::Never,
        },
    );
    assert_eq!(g.title.as_deref(), Some("示例片2"), "{g:#?}");
    assert!(g.episodes.is_empty(), "{g:#?}");
    // Mixed scripts inside one word stay one title.
    assert_movie(&movies("示ing例.1048.2007.mkv"), "示ing例 1048", Some(2007));
}

#[test]
fn bracket_group_names() {
    // [中文名][Group][English Name][tags]: the Latin title, the CJK original,
    // the group from the middle — not "[Some Studio]".
    // A number in the title group stays in the title.
    let g = movies("[示例片.副题][Some.Studio][Some.Film.3000][BDRip_720P][X264_AAC_ENG_US].mkv");
    assert_movie(&g, "Some Film 3000", None);
    assert_eq!(g.original_title.as_deref(), Some("示例片 副题"));
    assert_eq!(g.release_group.as_deref(), Some("Some.Studio"));
    assert_eq!(g.resolution.as_deref(), Some("720p"));
    // [Group][中文名][English][01][tags] (anime fansubs).
    let g = shows("Some Anime/[某某字幕组][示例动画][Some Anime][01][1080P][简体内嵌].mp4");
    assert_eq!(
        (g.kind, g.title.as_deref(), g.absolute_episode),
        (GuessKind::Episode, Some("Some Anime"), Some(1)),
        "{g:#?}"
    );
    assert_eq!(g.release_group.as_deref(), Some("某某字幕组"));
    assert_eq!(g.original_title.as_deref(), Some("示例动画"));
    // [Group][Title][EP][tags]: the first group is the fansub group.
    let g = shows("Some Title/[SomeSub][Some Title][02][1080p].mkv");
    assert_eq!(
        (g.title.as_deref(), g.absolute_episode),
        (Some("Some Title"), Some(2))
    );
    assert_eq!(g.release_group.as_deref(), Some("SomeSub"));
    let g = movies("[Moonlit Tea][Some Film][1080p].mkv");
    assert_movie(&g, "Some Film", None);
    assert_eq!(g.release_group.as_deref(), Some("Moonlit Tea"));
    // A bracketed title that merely looks like a web address is still a title.
    let g = movies("[Lost.in.Somewhere][1080p].mkv");
    assert_movie(&g, "Lost in Somewhere", None);
    let g = shows("示例剧/[某组][示例剧][第03集][1080P].mp4");
    assert_episode(&g, "示例剧", None, &[3]);
    let g = movies("[示例片][2019][1080P].mkv");
    assert_movie(&g, "示例片", Some(2019));
    // A title that is only a bracket group or tags is flagged for the fallback.
    let g = movies("中英字幕.1080p.mkv");
    assert!(
        g.evidence.iter().any(|e| e.contains("looks like")) || g.title.as_deref() != Some("1080p"),
        "{g:#?}"
    );
}

#[test]
fn mixed_space_and_dot_names() {
    let g = movies("XY-01 示例片2.Some.Film.2.2024.4K.WEBrip.x265.10bit.DD5.1-某组.mkv");
    assert_movie(&g, "XY-01 示例片2 Some Film 2", Some(2024));
    assert_eq!(g.original_title, None, "three parts: no bilingual split");
    assert_movie(
        &movies("Agents of S.H.I.E.L.D. Origins (2020).mkv"),
        "Agents of S.H.I.E.L.D. Origins",
        Some(2020),
    );
    assert_movie(
        &movies("Tron 2.0 Legacy (2010).mkv"),
        "Tron 2.0 Legacy",
        Some(2010),
    );
    assert_movie(
        &movies("Mr. Robot Returns (2015).mkv"),
        "Mr. Robot Returns",
        Some(2015),
    );
    // One dot after a short word is an abbreviation, not a word joint.
    for (name, title) in [
        ("Some Film Vol.2 (2004).mkv", "Some Film Vol.2"),
        ("Mr.Someone's Holiday.mkv", "Mr.Someone's Holiday"),
        ("No.7 Some Film.mkv", "No.7 Some Film"),
        ("St.Someone's Fire (1985).mkv", "St.Someone's Fire"),
    ] {
        assert_eq!(movies(name).title.as_deref(), Some(title), "{name}");
    }
    let g = shows("Some Show/Some Show S01E01 Mr.Someone Returns.mkv");
    assert_eq!(g.episode_title.as_deref(), Some("Mr.Someone Returns"), "{g:#?}");
    // Two joining dots or more are a scene-style part.
    assert_movie(
        &movies("XY-02 Some.Film.Name (2020).mkv"),
        "XY-02 Some Film Name",
        Some(2020),
    );
}

#[test]
fn letter_suffixed_parts_stack() {
    let sibs = ["示例片B.mp4", "notes.txt"];
    let g = with_sibs("示例片-MP4/示例片A.mp4", &sibs, LibraryKind::Movies);
    assert_movie(&g, "示例片", None);
    assert_eq!(g.part, Some(1), "{g:#?}");
    let g = with_sibs("示例片-MP4/示例片B.mp4", &["示例片A.mp4"], LibraryKind::Movies);
    assert_eq!((g.title.as_deref(), g.part), (Some("示例片"), Some(2)), "{g:#?}");
    let g = with_sibs("Some Movie-b.avi", &["Some Movie-a.avi"], LibraryKind::Movies);
    assert_eq!(
        (g.title.as_deref(), g.part),
        (Some("Some Movie"), Some(2)),
        "{g:#?}"
    );
    let g = with_sibs(
        "2046示例A.mp4",
        &["2046示例B.mp4", "2046示例C.mp4"],
        LibraryKind::Movies,
    );
    assert_eq!(g.part, Some(1), "{g:#?}");
    // Guards: no sibling, no "A", a word before the letter, numbered sequels.
    let g = with_sibs("示例片A.mp4", &[], LibraryKind::Movies);
    assert_eq!((g.title.as_deref(), g.part), (Some("示例片A"), None));
    let g = with_sibs("示例片B.mp4", &["示例片C.mp4"], LibraryKind::Movies);
    assert_eq!(g.part, None, "{g:#?}");
    let g = parse_with(
        Path::new("Plan A.mkv"),
        &["Plan B.mkv"],
        LibraryKind::Movies,
        ParseOptions {
            fallback: Fallback::Never,
        },
    );
    assert_eq!((g.title.as_deref(), g.part), (Some("Plan A"), None));
    let g = with_sibs("Panda.mkv", &["Pandb.mkv"], LibraryKind::Movies);
    assert_eq!(g.part, None);
    let g = parse_with(
        Path::new("示例片1.mp4"),
        &["示例片2.mp4"],
        LibraryKind::Movies,
        ParseOptions {
            fallback: Fallback::Never,
        },
    );
    assert_eq!((g.title.as_deref(), g.part), (Some("示例片1"), None));
}

#[test]
fn bilingual_names_keep_both_halves() {
    let g = shows("Some Show/S01/示例剧.Some.Show.S01E01.1080p.mp4");
    assert_episode(&g, "Some Show", Some(1), &[1]);
    assert_eq!(g.original_title.as_deref(), Some("示例剧"));
    // Without a show folder the Latin half is the show; the CJK half is kept.
    let g = p("示例剧.Some.Show.S01E01.1080p.mp4", LibraryKind::Mixed);
    assert_episode(&g, "Some Show", Some(1), &[1]);
    assert_eq!(g.original_title.as_deref(), Some("示例剧"));
    let g = movies("示例片.Some.Film.2022.BD1080P.X264.AAC/Some.Film.2022.BD1080P.X264.AAC.mp4");
    assert_movie(&g, "Some Film", Some(2022));
    assert_eq!(g.original_title.as_deref(), Some("示例片"));
    let g = movies("示例片.Some.Film.2019.1080p.mkv");
    assert_movie(&g, "Some Film", Some(2019));
    assert_eq!(g.original_title.as_deref(), Some("示例片"));
    // Latin first or spaced names are not split: the Latin part may be a
    // subtitle, the CJK part an arc name.
    let g = movies("Some Film 示例片 (2019).mkv");
    assert_movie(&g, "Some Film 示例片", Some(2019));
    assert_eq!(g.original_title, None);
    // Korean and Japanese too.
    let g = movies("예시영화.Some.Film.2019.mkv");
    assert_eq!(
        (g.title.as_deref(), g.original_title.as_deref()),
        (Some("Some Film"), Some("예시영화"))
    );
    // Not bilingual: one script, a single letter, mixed words.
    assert_eq!(movies("示例片 (2019).mkv").original_title, None);
    assert_movie(&movies("X示例.2010.mkv"), "X示例", Some(2010));
    assert_eq!(movies("示ing例.2007.mkv").original_title, None);
}

#[test]
fn non_episode_file_beside_episodes_is_an_extra() {
    let sibs = ["Some.Show.S01E01.1080p.mp4", "Some.Show.S01E02.1080p.mp4"];
    let g = with_sibs(
        "Some.Show/示例.Short.1080P.某某字幕组.mp4",
        &sibs,
        LibraryKind::Shows,
    );
    assert_eq!(
        (g.kind, g.extra),
        (GuessKind::Extra, Some(ExtraKind::Other)),
        "{g:#?}"
    );
    assert_eq!(g.title.as_deref(), Some("Some Show"));
    assert_eq!(g.episode_title.as_deref(), Some("示例 Short"));
    // Without episode siblings it stays unknown.
    let g = with_sibs("Some.Show/示例.Short.1080P.mp4", &[], LibraryKind::Shows);
    assert_eq!(g.kind, GuessKind::Unknown);
}

#[test]
fn fansub_tag_groups_are_not_titles() {
    // [GB], [BIG5], [MP4], [TV], [END]: notes in a group of their own.
    for tag in [
        "GB",
        "BIG5",
        "CHT][720P][MP4",
        "TV",
        "END",
        "GB_MP4",
        "CHS&JPN",
        "V2",
    ] {
        let name = format!("[某某字幕组][示例动画][02][{tag}][1080P].mp4");
        let g = shows(&name);
        assert_eq!(
            (g.kind, g.title.as_deref(), g.absolute_episode),
            (GuessKind::Episode, Some("示例动画"), Some(2)),
            "{name}: {g:#?}"
        );
        assert_eq!(g.original_title, None, "{name}");
        assert_eq!(g.release_group.as_deref(), Some("某某字幕组"), "{name}");
    }
    // Two films with the same tags stay two titles.
    let a = movies("[某某字幕组][示例片][GB][1080P].mp4");
    let b = movies("[某某字幕组][另一片][GB][1080P].mp4");
    assert_movie(&a, "示例片", None);
    assert_movie(&b, "另一片", None);
    // An unknown word after the episode is never the title.
    let g = shows("[某某字幕组][示例动画][03][XYZW][1080P].mp4");
    assert_eq!(g.title.as_deref(), Some("示例动画"), "{g:#?}");
    // A generic Latin group is a subtitle, not the title.
    let g = movies("[某某字幕组][示例动画][Movie][1080P].mp4");
    assert_movie(&g, "示例动画", None);
}

#[test]
fn cjk_fansub_group_without_suffix() {
    // No group says 字幕组: the first group is the fansub group, by convention.
    let g = shows("[某某奶茶屋][Some Anime][01][1080p].mp4");
    assert_eq!(g.title.as_deref(), Some("Some Anime"), "{g:#?}");
    assert_eq!(g.release_group.as_deref(), Some("某某奶茶屋"));
    assert_eq!(g.original_title, None);
    let g = shows("[某某学园][示例动画][Some Anime][01][720P].mp4");
    assert_eq!(g.title.as_deref(), Some("Some Anime"), "{g:#?}");
    assert_eq!(g.release_group.as_deref(), Some("某某学园"));
    assert_eq!(g.original_title.as_deref(), Some("示例动画"));
    // [中文名][Group][English Name]: the Latin group in the middle is the group.
    let g = movies("[示例片][Moonlit Tea][Some Film][1080p].mkv");
    assert_movie(&g, "Some Film", None);
    assert_eq!(g.release_group.as_deref(), Some("Moonlit Tea"));
    assert_eq!(g.original_title.as_deref(), Some("示例片"));
}

#[test]
fn cjk_season_markers_before_an_episode_number() {
    // 第二季 + a bare number: season 2, that episode.
    for name in [
        "示例动画/示例动画 第二季 05.mkv",
        "示例动画/示例动画.第二季.05.mkv",
        "示例动画/示例动画 第2季 - 05.mkv",
        "示例动画 第二季 05.mkv",
    ] {
        assert_episode(&shows(name), "示例动画", Some(2), &[5]);
    }
    let a = shows("示例动画 第二季 01.mp4");
    let b = shows("示例动画 第二季 02.mp4");
    assert_eq!((a.episodes.clone(), b.episodes.clone()), (vec![1], vec![2]));
    // 第二期 (an anime's second cour) before an episode number is a season.
    for name in [
        "示例动画 第二期/示例动画 第二期 05.mkv",
        "示例动画 第二期 第05话.mkv",
        "示例动画第二期 - 05.mkv",
    ] {
        assert_episode(&shows(name), "示例动画", Some(2), &[5]);
    }
    let g = shows("[某某字幕组][示例动画][第二期][05][1080P].mp4");
    assert_eq!((g.season, g.episodes.clone()), (Some(2), vec![5]), "{g:#?}");
    let g = shows("[某某字幕组][示例动画 第二季][05][1080P].mp4");
    assert_eq!((g.season, g.episodes.clone()), (Some(2), vec![5]), "{g:#?}");
    // Alone, 第N期 is a variety show's issue: an episode.
    assert_episode(&shows("示例综艺/示例综艺 第1期.mp4"), "示例综艺", None, &[1]);
    assert_episode(&shows("示例综艺 第12期.mp4"), "示例综艺", None, &[12]);
    let g = shows("[某某字幕组][示例综艺 第12期][1080P].mp4");
    assert_eq!(g.episodes, vec![12], "{g:#?}");
    // A show folder keeps its season marker; no episode is read from it.
    let g = shows("示例动画 第二季 01-12/示例动画 第二季 03.mkv");
    assert_episode(&g, "示例动画", Some(2), &[3]);
}

#[test]
fn cjk_markers_inside_words_and_in_movie_libraries() {
    // A marker that starts a longer word is part of the title.
    assert_movie(&movies("示例第一回合 (2019).mkv"), "示例第一回合", Some(2019));
    assert_movie(&movies("示例第三集团.2010.mkv"), "示例第三集团", Some(2010));
    // In a movie library 回, 期 and 季 alone are title words.
    assert_movie(&movies("示例 第三期 (2020).mkv"), "示例 第三期", Some(2020));
    assert_movie(&movies("示例第一季.mkv"), "示例第一季", None);
    assert_movie(&movies("示例 第一回 (2019).mkv"), "示例 第一回", Some(2019));
    assert_movie(&movies("示例第二季 05.mkv"), "示例第二季 05", None);
    // An episode unit still marks an episode there, like S01E01 does.
    assert_eq!(movies("示例剧 第2季 第3集.mkv").episodes, vec![3]);
    // A glued episode title after an episode unit is cut off.
    let g = shows("示例剧/示例剧第36集大结局.mp4");
    assert_episode(&g, "示例剧", None, &[36]);
    assert_eq!(g.episode_title.as_deref(), Some("大结局"));
}

#[test]
fn number_glued_to_a_cjk_title_is_not_a_year() {
    let g = movies("示例1900.mkv");
    assert_movie(&g, "示例1900", None);
    let g = movies("示例1900.2019.1080p.mkv");
    assert_movie(&g, "示例1900", Some(2019));
    assert_movie(&movies("示例片.2019.mkv"), "示例片", Some(2019));
}

#[test]
fn cjk_titles_with_latin_subtitles_stay_whole() {
    for (name, title) in [
        ("示例动画 III (2019).mkv", "示例动画 III"),
        ("示例动画 THE MOVIE (2019).mp4", "示例动画 THE MOVIE"),
        ("示例の刃 Some Train (2020).mkv", "示例の刃 Some Train"),
        ("示例动画.The.Movie.2019.1080p.mkv", "示例动画 The Movie"),
        ("示例动画.III.2019.mkv", "示例动画 III"),
    ] {
        let g = movies(name);
        assert_movie(&g, title, Some(if name.contains("2020") { 2020 } else { 2019 }));
        assert_eq!(g.original_title, None, "{name}");
    }
    // Two works with the same subtitle are two titles.
    let a = movies("示例动画 THE MOVIE (2019).mp4");
    let b = movies("其他动画 THE MOVIE (2019).mp4");
    assert_ne!(a.title, b.title);
    let g = shows("示例动画 Final Season/示例动画 Final Season S01E01.mkv");
    assert_eq!(g.title.as_deref(), Some("示例动画 Final Season"), "{g:#?}");
    let g = shows("SOMEANIME 千年篇/SOMEANIME 千年篇 S01E01.mkv");
    assert_eq!(g.title.as_deref(), Some("SOMEANIME 千年篇"), "{g:#?}");
    assert_eq!(g.original_title, None);
    // The release-group layout still splits.
    let g = movies("示例片.Some.Film.2019.1080p.mkv");
    assert_eq!(
        (g.title.as_deref(), g.original_title.as_deref()),
        (Some("Some Film"), Some("示例片"))
    );
}

fn videos(path: &str, siblings: &[&str]) -> NameGuess {
    parse(Path::new(path), siblings, LibraryKind::Videos)
}

fn mixed(path: &str, siblings: &[&str]) -> NameGuess {
    parse(Path::new(path), siblings, LibraryKind::Mixed)
}

#[test]
fn a_videos_library_keeps_every_file_its_own_video() {
    let g = videos("Holiday 2019/IMG_1234.MOV", &["IMG_1235.MOV"]);
    assert_eq!(
        (g.kind, g.title.as_deref(), g.year, g.part),
        (GuessKind::Movie, Some("IMG 1234"), None, None),
        "{g:#?}"
    );
    // No extras, samples, shows, years or ids: the name is the title.
    let g = videos("Trip/Featurettes/Making Of.mp4", &[]);
    assert_eq!(
        (g.kind, g.title.as_deref(), g.extra),
        (GuessKind::Movie, Some("Making Of"), None)
    );
    assert_eq!(videos("Party/Sample/sample.mp4", &[]).kind, GuessKind::Movie);
    assert_eq!(videos("Trip/Others/clip.mp4", &[]).kind, GuessKind::Movie);
    let g = videos("Show.S01E02.Pilot.mkv", &[]);
    assert_eq!(
        (g.kind, g.title.as_deref(), g.season, g.episodes.len()),
        (GuessKind::Movie, Some("Show S01E02 Pilot"), None, 0)
    );
    let g = videos("Alien (1979)/Alien (1979) [tmdbid-348].mkv", &[]);
    assert_eq!(
        (g.title.as_deref(), g.year, g.ids.tmdb),
        (Some("Alien (1979) [tmdbid-348]"), None, None)
    );
    // Numbered files are not named after their folder.
    assert_eq!(
        videos("Camera/001.mp4", &["002.mp4"]).title.as_deref(),
        Some("001")
    );
    assert_eq!(
        videos("VID_20190501_123456.mp4", &[]).title.as_deref(),
        Some("VID 20190501 123456")
    );
    assert_eq!(
        videos("My.Trip.v1.5.mkv", &[]).title.as_deref(),
        Some("My Trip v1.5")
    );
    // Only a true multi-part stack makes parts of one video.
    let g = videos("Wedding/Wedding part1.mp4", &["Wedding part2.mp4"]);
    assert_eq!((g.title.as_deref(), g.part), (Some("Wedding"), Some(1)), "{g:#?}");
    let g = videos("Wedding/Wedding part1.mp4", &["Wedding part2.srt"]);
    assert_eq!(
        (g.title.as_deref(), g.part),
        (Some("Wedding part1"), None),
        "{g:#?}"
    );
    let g = videos("示例A.mp4", &["示例B.mp4"]);
    assert_eq!((g.title.as_deref(), g.part), (Some("示例"), Some(1)), "{g:#?}");
    let g = videos("part1.mp4", &["part2.mp4"]);
    assert_eq!((g.title.as_deref(), g.part), (Some("part1"), None), "{g:#?}");
}

#[test]
fn others_folders_hold_extras_only_outside_mixed_libraries() {
    let g = mixed("2019/others/Beach1234.mp4", &[]);
    assert_eq!(
        (g.kind, g.title.as_deref()),
        (GuessKind::Movie, Some("Beach1234")),
        "{g:#?}"
    );
    let g = mixed("Some Film (2010)/Other/Making Of.mkv", &[]);
    assert_eq!(
        (g.kind, g.title.as_deref()),
        (GuessKind::Movie, Some("Making Of")),
        "{g:#?}"
    );
    // Plex's and Jellyfin's convention still holds in movie and show libraries.
    let g = movies("Some Film (2010)/Other/Making Of.mkv");
    assert_eq!(
        (g.kind, g.extra),
        (GuessKind::Extra, Some(ExtraKind::Other)),
        "{g:#?}"
    );
    let g = shows("Some Show/Others/Bloopers.mkv");
    assert_eq!(g.kind, GuessKind::Extra, "{g:#?}");
    // Other extras folders keep their meaning in a mixed library.
    assert_eq!(
        mixed("Some Film (2010)/Featurettes/Making Of.mkv", &[]).kind,
        GuessKind::Extra
    );
}

#[test]
fn a_dated_recording_in_a_mixed_library_is_no_daily_show() {
    for path in [
        "Phone/VID_2019-05-01_12-34-56.mp4",
        "Screen Recording 2019-05-01 at 12.34.56.mov",
        "Trip/2019-05-01.mp4",
        "Trip/2019.05.01 Beach.mp4",
        "Trip/[2019-05-01] Beach.mp4",
    ] {
        let g = mixed(path, &[]);
        assert_eq!(
            (g.kind, g.season, g.air_date.as_deref()),
            (GuessKind::Movie, None, None),
            "{path}: {g:#?}"
        );
    }
    // A camera's compact timestamp: the fallback reads it as a date.
    let g = mixed(
        "Camera/VID_20190501_123456789.mp4",
        &["VID_20190502_101010123.mp4", "VID_20190503_090909000.mp4"],
    );
    assert_eq!(
        (g.kind, g.air_date.as_deref()),
        (GuessKind::Movie, None),
        "{g:#?}"
    );
    assert_eq!(g.title.as_deref(), Some("VID 20190501 123456789"), "{g:#?}");
    // A TV folder, a season folder or a show's own dated name still make episodes.
    let g = mixed("TV/Some Show/2019-05-01.mp4", &[]);
    assert_eq!(
        (g.kind, g.title.as_deref(), g.air_date.as_deref()),
        (GuessKind::Episode, Some("Some Show"), Some("2019-05-01")),
        "{g:#?}"
    );
    // A release's dated name is a daily show's episode, alone or not…
    let daily = "The.Daily.Show.2024.03.15.Guest.Name.720p.WEB.h264.mkv";
    let g = mixed(daily, &["Beach.mp4"]);
    assert_eq!(
        (g.kind, g.title.as_deref(), g.air_date.as_deref()),
        (GuessKind::Episode, Some("The Daily Show"), Some("2024-03-15")),
        "{g:#?}"
    );
    // …but a title and a date are all a home video has, too: without
    // anything of a release about it, a dated name needs others of its
    // series beside it, so a lone one keeps no year as a season.
    let g = mixed("Trip/Beach_Day_2019-05-01.mp4", &["Beach_Day_2019-05-01.srt"]);
    assert_eq!(
        (g.kind, g.title.as_deref(), g.air_date.as_deref()),
        (GuessKind::Movie, Some("Beach Day 2019-05-01"), None),
        "{g:#?}"
    );
    let g = mixed("Trip/Beach_Day_2019-05-01.mp4", &["Beach_Day_2019-05-04.mp4"]);
    assert_eq!(
        (g.kind, g.air_date.as_deref()),
        (GuessKind::Episode, Some("2019-05-01")),
        "{g:#?}"
    );
    let g = shows("Some Show/Some Show 2019-05-01 12-34-56.mkv");
    assert_eq!(g.kind, GuessKind::Episode, "{g:#?}");
}

#[test]
fn a_weakly_numbered_episode_in_a_mixed_library_needs_its_series() {
    // `E05` and a number after a dash are episode markers the fast path
    // reads anywhere; in a mixed library without a TV folder a lone file
    // with one and nothing of a release about it is a video titled after
    // its name.
    for (path, sib) in [
        ("Holiday/Clip E05.mp4", "Clip E06.mp4"),
        ("Holiday/Beach Walk - 07.mp4", "Beach Walk - 08.mp4"),
    ] {
        let alone = mixed(path, &["Beach.mp4"]);
        assert_eq!(
            (alone.kind, alone.season, alone.episodes.len()),
            (GuessKind::Movie, None, 0),
            "{path}: {alone:#?}"
        );
        let stem = path.rsplit('/').next().unwrap().rsplit_once('.').unwrap().0;
        assert_eq!(alone.title.as_deref(), Some(stem), "{alone:#?}");
        let series = mixed(path, &[sib]);
        assert_eq!(series.kind, GuessKind::Episode, "{path}: {series:#?}");
        // In a shows library, or a mixed library's TV folder, alone is fine.
        assert_eq!(shows(&format!("Some Show/{path}")).kind, GuessKind::Episode);
        assert_eq!(
            mixed(&format!("TV/Some Show/{path}"), &[]).kind,
            GuessKind::Episode
        );
    }
    // Explicit markers, and release names, need nothing else.
    assert_eq!(mixed("示例剧 第05集.mp4", &[]).kind, GuessKind::Episode);
    assert_eq!(mixed("Some Show 1x05.mkv", &[]).kind, GuessKind::Episode);
    assert_eq!(
        mixed("[Group] Some Show - 07 [1080p].mkv", &[]).kind,
        GuessKind::Episode
    );
}

#[test]
fn a_single_clip_in_a_mixed_library_is_no_show_of_one_episode() {
    // The fallback reads a trailing number as an episode; alone in its
    // folder, the file stays a video of its own.
    let g = mixed("Holiday/Holiday Clip 03.mp4", &["notes.txt", "Beach.mp4"]);
    assert_eq!(g.kind, GuessKind::Movie, "{g:#?}");
    assert_eq!(g.title.as_deref(), Some("Holiday Clip 03"), "{g:#?}");
    // A numbered series of files in one folder is evidence of a show.
    let g = mixed(
        "Holiday/Holiday Clip 03.mp4",
        &["Holiday Clip 01.mp4", "Holiday Clip 02.mp4"],
    );
    assert_eq!(
        (g.kind, g.episodes.as_slice()),
        (GuessKind::Episode, &[3][..]),
        "{g:#?}"
    );
    // A copy of the same clip, or a clip numbered after another name, is not.
    for sibs in [&["Holiday Clip 03.mov"][..], &["Beach Clip 04.mp4"][..]] {
        let g = mixed("Holiday/Holiday Clip 03.mp4", sibs);
        assert_eq!(g.kind, GuessKind::Movie, "{sibs:?}: {g:#?}");
    }
    // So are an explicit marker and a TV folder.
    assert_eq!(mixed("Holiday Clip S01E03.mp4", &[]).kind, GuessKind::Episode);
    let g = mixed("TV/Holiday Clip/Holiday Clip 03.mp4", &[]);
    assert_eq!(g.kind, GuessKind::Episode, "{g:#?}");
}
