//! The 36 hard names from the research harness
//! (`scan-bench/src/bin/names.rs`, research/02 §6.2), with the same expected
//! values, so flux-names is scored exactly like hunch 2.0.2 (33/36 titles)
//! and torrent-name-parser 0.12.1 (29/36) were.
//!
//! Inputs are parsed as given (some include `Movies/`/`TV/` folders) with
//! [`LibraryKind::Mixed`](crate::LibraryKind::Mixed) and no siblings.

use crate::{norm_key, NameGuess};

/// One hard case: input path and expected title/year/season/episodes.
#[derive(Debug, Clone, Copy)]
pub struct HardCase {
    pub input: &'static str,
    pub title: &'static str,
    pub year: Option<u16>,
    pub season: Option<u16>,
    /// Expected episode numbers. For anime absolute numbering the harness
    /// expects the absolute number here (season `None`).
    pub episodes: &'static [u16],
}

const fn c(
    input: &'static str,
    title: &'static str,
    year: Option<u16>,
    season: Option<u16>,
    episodes: &'static [u16],
) -> HardCase {
    HardCase {
        input,
        title,
        year,
        season,
        episodes,
    }
}

/// The 36 cases.
pub const CASES: &[HardCase] = &[
    c("The.Matrix.1999.1080p.BluRay.x264-GROUP.mkv", "The Matrix", Some(1999), None, &[]),
    c("Blade Runner 2049 (2017) 2160p UHD BluRay REMUX HDR HEVC Atmos-FGT.mkv", "Blade Runner 2049", Some(2017), None, &[]),
    c("2001.A.Space.Odyssey.1968.REMASTERED.1080p.BluRay.x265.10bit-RARBG.mkv", "2001 A Space Odyssey", Some(1968), None, &[]),
    c("1917.2019.2160p.WEB-DL.DDP5.1.Atmos.HDR.HEVC-EVO.mkv", "1917", Some(2019), None, &[]),
    c("Movies/Alien (1979)/Alien (1979) - Director's Cut [Bluray-1080p].mkv", "Alien", Some(1979), None, &[]),
    c("Amélie.2001.FRENCH.1080p.BluRay.DTS.x264-HDMaNiAcS.mkv", "Amélie", Some(2001), None, &[]),
    c("The.Lord.of.the.Rings.The.Fellowship.of.the.Ring.2001.EXTENDED.1080p.BluRay.x264.mkv", "The Lord of the Rings The Fellowship of the Ring", Some(2001), None, &[]),
    c("Se7en.1995.1080p.BluRay.x264.mkv", "Se7en", Some(1995), None, &[]),
    c("WALL-E.2008.720p.BluRay.x264.mkv", "WALL-E", Some(2008), None, &[]),
    c("Spider-Man.Into.the.Spider-Verse.2018.1080p.WEBRip.x264.mkv", "Spider-Man Into the Spider-Verse", Some(2018), None, &[]),
    c("Parasite (2019) [tmdbid-496243].mkv", "Parasite", Some(2019), None, &[]),
    c("Avatar.The.Way.of.Water.2022.3D.HSBS.1080p.BluRay.x264.mkv", "Avatar The Way of Water", Some(2022), None, &[]),
    c("Dune.Part.Two.2024.2160p.WEB-DL.DV.HDR10Plus.DDP5.1.Atmos.H.265-FLUX.mkv", "Dune Part Two", Some(2024), None, &[]),
    c("Kill.Bill.Vol.1.2003.1080p.BluRay.x264.cd1.avi", "Kill Bill Vol 1", Some(2003), None, &[]),
    c("Breaking.Bad.S05E14.Ozymandias.1080p.BluRay.x264-ROVERS.mkv", "Breaking Bad", None, Some(5), &[14]),
    c("The.Office.US.S02E01E02.720p.WEB-DL.mkv", "The Office US", None, Some(2), &[1, 2]),
    c("Doctor.Who.2005.S01E01.Rose.720p.BluRay.mkv", "Doctor Who", Some(2005), Some(1), &[1]),
    c("Friends - 1x01 - The One Where Monica Gets a Roommate.avi", "Friends", None, Some(1), &[1]),
    c("Game.of.Thrones.S08E01-E02.2160p.UHD.BluRay.mkv", "Game of Thrones", None, Some(8), &[1, 2]),
    c("TV/Severance (2022)/Season 02/Severance (2022) - S02E10 - Cold Harbor [WEBDL-2160p][DV HDR10][EAC3 Atmos 5.1][h265]-FLUX.mkv", "Severance", Some(2022), Some(2), &[10]),
    c("TV/The Expanse/Season 3/The Expanse - S03E01-E02 - Fight or Flight.mkv", "The Expanse", None, Some(3), &[1, 2]),
    c("TV/Sherlock/Specials/Sherlock - S00E01 - The Abominable Bride.mkv", "Sherlock", None, Some(0), &[1]),
    c("The.Daily.Show.2024.03.15.Guest.Name.720p.WEB.h264.mkv", "The Daily Show", None, None, &[]),
    c("[SubsPlease] Sousou no Frieren - 12 (1080p) [ABCDEF12].mkv", "Sousou no Frieren", None, None, &[12]),
    c("[Erai-raws] One Piece - 1100 [1080p][Multiple Subtitle].mkv", "One Piece", None, None, &[1100]),
    c("Shingeki no Kyojin S04E28 [1080p].mkv", "Shingeki no Kyojin", None, Some(4), &[28]),
    c("Planet.Earth.II.S01E01.Islands.2160p.UHD.BluRay.x265.mkv", "Planet Earth II", None, Some(1), &[1]),
    c("Westworld.S01E10.The.Bicameral.Mind.1080p.mkv", "Westworld", None, Some(1), &[10]),
    c("Show.Name.Season.1.Episode.3.mkv", "Show Name", None, Some(1), &[3]),
    c("Mr.Robot.S04E13.eps4.0_whoami.1080p.mkv", "Mr Robot", None, Some(4), &[13]),
    c("9-1-1.S07E01.1080p.WEB.h264.mkv", "9-1-1", None, Some(7), &[1]),
    c("S.W.A.T.2017.S05E01.720p.HDTV.x264.mkv", "S.W.A.T.", Some(2017), Some(5), &[1]),
    c("Its.Always.Sunny.in.Philadelphia.S16E08.1080p.mkv", "Its Always Sunny in Philadelphia", None, Some(16), &[8]),
    c("The.100.S03E05.720p.mkv", "The 100", None, Some(3), &[5]),
    c("Fargo.S05E01.2160p.WEB.H265.mkv", "Fargo", None, Some(5), &[1]),
    c("Star.Trek.Strange.New.Worlds.S02E09.Subspace.Rhapsody.1080p.mkv", "Star Trek Strange New Worlds", None, Some(2), &[9]),
];

/// Per-field result of one case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaseResult {
    pub title: bool,
    pub year: bool,
    pub season: bool,
    pub episodes: bool,
}

impl CaseResult {
    /// All four fields correct.
    pub fn all(&self) -> bool {
        self.title && self.year && self.season && self.episodes
    }
}

impl HardCase {
    /// Scores a guess the way the research harness did (titles compared as
    /// lowercase alphanumerics; anime absolute numbers count as episodes).
    pub fn score(&self, g: &NameGuess) -> CaseResult {
        let eps: Vec<u16> = if g.episodes.is_empty() {
            g.absolute_episode
                .and_then(|a| u16::try_from(a).ok())
                .into_iter()
                .collect()
        } else {
            g.episodes.clone()
        };
        CaseResult {
            title: g.title.as_deref().map(norm_key) == Some(norm_key(self.title)),
            year: g.year == self.year,
            season: g.season == self.season,
            episodes: eps == self.episodes,
        }
    }
}
