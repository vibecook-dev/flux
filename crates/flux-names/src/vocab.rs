//! Release-name vocabulary: which words are technical tags, and their
//! canonical spellings.
//!
//! Tags are *strong* when they essentially never occur in titles (`x264`,
//! `1080p`, `WEB-DL`) and *weak* when they do (`Web` in "Charlotte's Web",
//! `Opus` in "Mr. Holland's Opus", `Extended`, languages). A weak tag only
//! starts the technical zone of a name when another tag follows it; see
//! `stem::find_tag_boundary`.

/// The family a tag belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TagKind {
    Resolution,
    Source,
    VideoCodec,
    Audio,
    Hdr,
    Edition,
    Language,
    Streaming,
    Misc,
}

/// A recognised technical tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tag {
    pub kind: TagKind,
    pub strong: bool,
}

const fn strong(kind: TagKind) -> Option<Tag> {
    Some(Tag { kind, strong: true })
}
const fn weak(kind: TagKind) -> Option<Tag> {
    Some(Tag { kind, strong: false })
}

/// Classifies one word (case-insensitive). Hyphenated compounds of tags
/// (`WEBDL-2160p`, `Bluray-1080p`, `x264-GROUP`) count as strong tags when
/// any part is a strong tag.
pub(crate) fn classify(word: &str) -> Option<Tag> {
    let w = word.trim_matches(|c: char| !(c.is_alphanumeric() || c == '+'));
    if w.is_empty() || w.len() > 32 {
        return None;
    }
    let lw = w.to_ascii_lowercase();
    if let Some(t) = classify_lower(&lw) {
        return Some(t);
    }
    if lw.contains('-') {
        let mut found = None;
        for part in lw.split('-') {
            if let Some(t) = classify_lower(part) {
                if t.strong {
                    found.get_or_insert(t.kind);
                }
            }
        }
        return found.map(|kind| Tag { kind, strong: true });
    }
    None
}

/// `true` when the whole word is itself a tag spelled with a hyphen
/// (`WEB-DL`, `DTS-HD`, `Blu-ray`), so its last part is not a release group.
pub(crate) fn is_hyphenated_tag(word: &str) -> bool {
    word.contains('-') && classify_lower(&word.to_ascii_lowercase()).is_some()
}

fn classify_lower(w: &str) -> Option<Tag> {
    use TagKind::*;
    if is_resolution(w) {
        return strong(Resolution);
    }
    if is_audio(w) {
        return match w {
            "opus" | "mp3" | "mp2" | "pcm" | "dd" => weak(Audio),
            _ => strong(Audio),
        };
    }
    match w {
        "bluray" | "blu-ray" | "bdrip" | "brrip" | "bdremux" | "remux" | "web-dl" | "webdl" | "webrip"
        | "web-rip" | "hdtv" | "pdtv" | "sdtv" | "hdtvrip" | "dvdrip" | "dvd-rip" | "hdrip" | "dvdscr"
        | "hddvd" | "hd-dvd" | "uhdbd" | "bd25" | "bd50" | "bd66" | "bd100" | "dvd5" | "dvd9" | "dvdr"
        | "telesync" | "hdcam" | "camrip" | "tvrip" | "vhsrip" | "satrip" | "dsrip" | "webhd" | "uhdrip"
        | "bdmv" | "bdmux" | "bdripmux" | "bluraymux" | "webmux" | "webdlmux" | "dvdmux" | "hdmux"
        | "webdlrip" | "web-dlrip" | "dvdrip-mux" => strong(Source),
        "web" | "dvd" | "cam" | "bd" | "uhd" | "vhs" | "dsr" => weak(Source),
        "x264" | "x265" | "h264" | "h265" | "hevc" | "avc" | "xvid" | "divx" | "av1" | "vp9" | "vc1"
        | "vc-1" | "mpeg2" | "mpeg-2" | "10bit" | "10-bit" | "8bit" | "8-bit" | "12bit" | "hi10p"
        | "hi10" => strong(VideoCodec),
        "264" | "265" => weak(VideoCodec),
        "hdr" | "hdr10" | "hdr10+" | "hdr10plus" | "dovi" | "hlg" | "dolbyvision" => strong(Hdr),
        "dv" | "sdr" => weak(Hdr),
        "amzn" | "dsnp" | "hmax" | "atvp" | "pcok" | "pmtp" | "crav" | "dsny" | "hulu" => strong(Streaming),
        "nf" | "max" | "stan" | "itunes" => weak(Streaming),
        "repack" | "rerip" | "nfofix" | "readnfo" | "hsbs" | "h-sbs" | "hou" | "h-ou" | "multisubs"
        | "dual-audio" | "dualaudio" | "hardsub" | "hardsubs" | "softsubs" | "x264-hi10p" | "convert"
        | "dirfix" | "ac3d" | "ac3ld" => strong(Misc),
        "proper" | "internal" | "limited" | "complete" | "dubbed" | "subbed" | "real" | "hc" | "ws"
        | "sbs" | "3d" | "festival" | "retail" | "dc" | "hq" | "ntsc" | "pal" | "secam" | "md" | "ld" => {
            weak(Misc)
        }
        "extended" | "unrated" | "uncut" | "remastered" | "imax" | "theatrical" | "criterion" | "redux"
        | "despecialized" => weak(Edition),
        "french" | "truefrench" | "vff" | "vfq" | "vostfr" | "german" | "korean" | "japanese" | "spanish"
        | "italian" | "russian" | "hindi" | "multi" | "dual" | "english" | "eng" | "ita" | "fre" | "fra"
        | "ger" | "deu" | "spa" | "esp" | "jpn" | "kor" | "chi" | "chs" | "cht" | "swedish" | "nordic"
        | "danish" | "dutch" | "polish" | "portuguese" | "latino" | "castellano" | "dublado"
        | "legendado" | "subs" | "vost" | "swesub" | "nlsub" | "nlsubs" | "legendas" | "dl" | "fr" | "vf"
        | "vo" => weak(Language),
        _ => None,
    }
}

/// `480p`, `1080i`, `2160p`, `4k`, `1920x1080`.
fn is_resolution(w: &str) -> bool {
    let b = w.as_bytes();
    if matches!(w, "4k" | "8k" | "2k") {
        return true;
    }
    if b.len() >= 4 && b.len() <= 5 && matches!(b[b.len() - 1], b'p' | b'i') {
        return b[..b.len() - 1].iter().all(u8::is_ascii_digit);
    }
    if let Some((x, y)) = w.split_once('x') {
        return (3..=4).contains(&x.len())
            && (3..=4).contains(&y.len())
            && x.bytes().all(|c| c.is_ascii_digit())
            && y.bytes().all(|c| c.is_ascii_digit());
    }
    false
}

/// Audio codec words, optionally with glued channel digits (`DDP5`, `AAC2`,
/// `FLAC2`; the `.1`/`.0` after the dot is a separate token).
fn is_audio(w: &str) -> bool {
    const AUDIO: &[&str] = &[
        "aac", "ac3", "eac3", "e-ac-3", "ac-3", "dd", "dd+", "ddp", "dts", "dts-hd", "dtshd", "dts-x",
        "dtsx", "dts-es", "dts-ma", "truehd", "atmos", "flac", "opus", "lpcm", "pcm", "mp3", "mp2", "aac-lc",
        "he-aac",
    ];
    if AUDIO.contains(&w) {
        return true;
    }
    let base = w.trim_end_matches(|c: char| c.is_ascii_digit());
    // "ac3"/"eac3" end in a digit themselves; "dd5", "ddp5", "aac2" strip to a known base.
    base.len() < w.len()
        && w.len() - base.len() <= 2
        && !base.is_empty()
        && AUDIO.contains(&base)
        && base != "ac"
}

/// Canonical resolution label (`4k` → `2160p`).
pub(crate) fn canonical_resolution(word: &str) -> Option<String> {
    let w = word.to_ascii_lowercase();
    if !is_resolution(&w) {
        return None;
    }
    Some(match w.as_str() {
        "4k" => "2160p".to_string(),
        "8k" => "4320p".to_string(),
        "2k" => "1080p".to_string(),
        _ => match w.split_once('x') {
            Some((_, h)) => format!("{h}p"),
            None => w,
        },
    })
}

/// Canonical source label.
pub(crate) fn canonical_source(word: &str) -> Option<&'static str> {
    Some(match word.to_ascii_lowercase().as_str() {
        "bluray" | "blu-ray" | "bdrip" | "brrip" | "bd25" | "bd50" | "bd66" | "bd100" | "bd" | "bdmv" => {
            "BluRay"
        }
        "bdremux" | "remux" => "Remux",
        "uhdbd" | "uhdrip" => "UHD BluRay",
        "web-dl" | "webdl" | "webhd" => "WEB-DL",
        "webrip" | "web-rip" => "WEBRip",
        "web" => "WEB",
        "hdtv" | "pdtv" | "sdtv" | "hdtvrip" | "tvrip" | "satrip" | "dsrip" => "HDTV",
        "dvdrip" | "dvd-rip" | "dvd" | "dvd5" | "dvd9" | "dvdr" | "dvdscr" => "DVD",
        "hdrip" => "HDRip",
        "hddvd" | "hd-dvd" => "HD DVD",
        "cam" | "camrip" | "hdcam" | "telesync" => "CAM",
        "vhsrip" => "VHS",
        _ => return None,
    })
}

/// Canonical video codec label.
pub(crate) fn canonical_codec(word: &str) -> Option<&'static str> {
    Some(match word.to_ascii_lowercase().as_str() {
        "x264" | "h264" | "avc" | "264" => "H.264",
        "x265" | "h265" | "hevc" | "265" => "HEVC",
        "xvid" => "XviD",
        "divx" => "DivX",
        "av1" => "AV1",
        "vp9" => "VP9",
        "vc1" | "vc-1" => "VC-1",
        "mpeg2" | "mpeg-2" => "MPEG-2",
        _ => return None,
    })
}

/// Recognises an edition phrase at the start of `words` (lowercase compare)
/// and returns its canonical name and how many words it used.
pub(crate) fn edition_at(words: &[&str]) -> Option<(&'static str, usize)> {
    let lw = |i: usize| -> String {
        words
            .get(i)
            .map(|w| {
                w.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'')
                    .to_lowercase()
            })
            .unwrap_or_default()
    };
    let (a, b) = (lw(0), lw(1));
    let two = match (a.as_str(), b.as_str()) {
        ("director" | "directors" | "director's" | "directors'", "cut" | "edition") => Some("Director's Cut"),
        ("final", "cut") => Some("Final Cut"),
        ("extended", "cut" | "edition" | "version") => Some("Extended Edition"),
        ("theatrical", "cut" | "edition" | "version") => Some("Theatrical Cut"),
        ("special", "edition") => Some("Special Edition"),
        ("collectors" | "collector's", "edition") | ("edition", "collector" | "collectors") => {
            Some("Collector's Edition")
        }
        ("edition", "speciale" | "special") => Some("Special Edition"),
        ("criterion", "edition" | "collection") => Some("Criterion"),
        ("ultimate", "edition" | "cut") => Some("Ultimate Edition"),
        ("anniversary", "edition") => Some("Anniversary Edition"),
        ("open", "matte") => Some("Open Matte"),
        ("unrated", "cut" | "edition") => Some("Unrated"),
        ("imax", "edition") => Some("IMAX"),
        _ => None,
    };
    if let Some(e) = two {
        return Some((e, 2));
    }
    let one = match a.as_str() {
        "extended" => "Extended",
        "unrated" => "Unrated",
        "uncut" => "Uncut",
        "remastered" => "Remastered",
        "imax" => "IMAX",
        "theatrical" => "Theatrical",
        "criterion" => "Criterion",
        "redux" => "Redux",
        "despecialized" => "Despecialized",
        _ => return None,
    };
    Some((one, 1))
}

/// Multi-part markers: `cd1`, `part2`, `pt.3`, `disc 1`, `dvd2`, `Part II`
/// (Roman/English numerals only when `allow_words`). Returns the part
/// number and how many words it used.
pub(crate) fn part_at(words: &[&str], allow_words: bool) -> Option<(u8, usize)> {
    let first = words.first()?.to_ascii_lowercase();
    const PREFIXES: &[&str] = &["cd", "dvd", "disc", "disk", "part", "pt"];
    for p in PREFIXES {
        if let Some(rest) = first.strip_prefix(p) {
            let rest = rest.trim_start_matches(['-', '.', '_']);
            if !rest.is_empty() {
                return small_number(rest, false).map(|n| (n, 1));
            }
            let next = words.get(1)?;
            return small_number(&next.to_ascii_lowercase(), allow_words).map(|n| (n, 2));
        }
    }
    None
}

/// 1–2 digit numbers, and with `allow_words` Roman (I–X) and English
/// (one–ten) numerals.
fn small_number(w: &str, allow_words: bool) -> Option<u8> {
    if !w.is_empty() && w.len() <= 2 && w.bytes().all(|b| b.is_ascii_digit()) {
        return w.parse().ok().filter(|n| *n > 0);
    }
    if !allow_words {
        return None;
    }
    const ROMAN: &[&str] = &["i", "ii", "iii", "iv", "v", "vi", "vii", "viii", "ix", "x"];
    const ENGLISH: &[&str] = &[
        "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    ];
    ROMAN
        .iter()
        .position(|r| *r == w)
        .or_else(|| ENGLISH.iter().position(|e| *e == w))
        .map(|i| i as u8 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strong_and_weak_tags() {
        assert!(classify("1080p").unwrap().strong);
        assert!(classify("x264-GROUP").unwrap().strong);
        assert!(classify("WEBDL-2160p").unwrap().strong);
        assert!(classify("DDP5").unwrap().strong);
        assert!(classify("AAC2").unwrap().strong);
        assert!(classify("EAC3").unwrap().strong);
        assert!(!classify("Web").unwrap().strong);
        assert!(!classify("Opus").unwrap().strong);
        assert!(classify("Spider-Man").is_none());
        assert!(classify("WALL-E").is_none());
        assert!(classify("9-1-1").is_none());
        assert!(classify("Matrix").is_none());
        assert!(classify("1999").is_none());
    }

    #[test]
    fn canonical_labels() {
        assert_eq!(canonical_resolution("4K").as_deref(), Some("2160p"));
        assert_eq!(canonical_resolution("1920x1080").as_deref(), Some("1080p"));
        assert_eq!(canonical_source("BDRip"), Some("BluRay"));
        assert_eq!(canonical_codec("x265"), Some("HEVC"));
    }

    #[test]
    fn editions_and_parts() {
        assert_eq!(edition_at(&["Director's", "Cut"]), Some(("Director's Cut", 2)));
        assert_eq!(edition_at(&["EXTENDED", "1080p"]), Some(("Extended", 1)));
        assert_eq!(part_at(&["cd1"], false), Some((1, 1)));
        assert_eq!(part_at(&["Part", "2"], false), Some((2, 2)));
        assert_eq!(part_at(&["Part", "Two"], false), None);
        assert_eq!(part_at(&["Part", "Two"], true), Some((2, 2)));
        assert_eq!(part_at(&["Partridge"], false), None);
    }
}
