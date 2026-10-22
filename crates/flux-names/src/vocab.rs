//! Release-name vocabulary: which words are technical tags, and their
//! canonical spellings.
//!
//! Tags are *strong* when they essentially never occur in titles (`x264`,
//! `1080p`, `WEB-DL`) and *weak* when they do (`Web` in "Charlotte's Web",
//! `Opus` in "Mr. Holland's Opus", `Extended`, languages). A weak tag only
//! starts the technical zone of a name when another tag follows it; see
//! `stem::find_tag_boundary`.
//!
//! Chinese release names add their own tags: subtitle notes (`中英字幕`,
//! `简繁双语`, `官方中文字幕`), audio (`国语`, `粤语`), quality (`高清`,
//! `蓝光`), editions (`导演剪辑版`) and fansub/site names ending in
//! `字幕组`/`影视`. A CJK word is a tag only when it is *entirely* made of
//! such morphemes ([`classify_cjk`]), so titles that merely contain one
//! ("我的国语老师") are left alone.

use crate::text::has_cjk;

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
    if w.is_empty() || w.len() > 40 {
        return None;
    }
    if has_cjk(w) {
        if let Some(t) = classify_cjk(w) {
            return Some(t);
        }
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
    if is_resolution(w) || glued_resolution(w).is_some() {
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

/// Subtitle notes: `中英字幕` = 中英 + 字幕, `官方中文字幕`, `简繁双语`, `内封`.
const CJK_SUBS: &[&str] = &[
    "中英",
    "英中",
    "中日",
    "日中",
    "中韩",
    "中韓",
    "中英日",
    "简繁",
    "繁简",
    "簡繁",
    "繁簡",
    "简英",
    "繁英",
    "简日",
    "繁日",
    "简中",
    "繁中",
    "简体",
    "簡體",
    "繁体",
    "繁體",
    "中文",
    "中英文",
    "双语",
    "雙語",
    "双字",
    "雙字",
    "中字",
    "字幕",
    "双字幕",
    "雙字幕",
    "特效",
    "官方",
    "内封",
    "內封",
    "内嵌",
    "內嵌",
    "外挂",
    "外掛",
    "硬字幕",
    "软字幕",
    "軟字幕",
    "熟肉",
];
/// Audio: `国语`, `粤语`, `国粤双语` (with 双语 from [`CJK_SUBS`]), `双音轨`.
const CJK_AUDIO: &[&str] = &[
    "国语",
    "國語",
    "粤语",
    "粵語",
    "国粤",
    "國粵",
    "国配",
    "國配",
    "台配",
    "粤配",
    "粵配",
    "英语",
    "英語",
    "日语",
    "日語",
    "韩语",
    "韓語",
    "原声",
    "原聲",
    "音轨",
    "音軌",
    "双音轨",
    "雙音軌",
    "多音轨",
    "多音軌",
    "国英",
    "國英",
];
/// Quality and source words.
const CJK_QUALITY: &[&str] = &[
    "高清",
    "超清",
    "标清",
    "標清",
    "蓝光",
    "藍光",
    "原盘",
    "原盤",
    "无损",
    "無損",
    "杜比视界",
    "杜比視界",
    "全景声",
    "全景聲",
];
/// Fansub groups and sharing sites: `某某字幕组`, `某某影视`.
const CJK_GROUP_SUFFIXES: &[&str] = &[
    "字幕组",
    "字幕組",
    "字幕社",
    "压制组",
    "壓制組",
    "汉化组",
    "漢化組",
    "影视",
    "影視",
];

/// Classifies a word that contains CJK characters: a strong tag when the
/// whole word is a CJK edition, a fansub/site name (`…字幕组`, `…影视`), or
/// a run of subtitle/audio/quality morphemes (`高清中英双字`). Never a tag
/// for words with other characters in them.
pub(crate) fn classify_cjk(w: &str) -> Option<Tag> {
    use TagKind::*;
    if cjk_edition(w).is_some() {
        return strong(Edition);
    }
    if is_cjk_group_name(w) {
        return strong(Misc);
    }
    // Segment the word into known morphemes (dynamic programming over byte
    // offsets): `kinds[i]` is set when `w[..i]` is a run of morphemes, and
    // holds its family. Subtitle notes outrank audio, audio outranks quality.
    let rank = |k: TagKind| match k {
        Language => 2,
        Audio => 1,
        _ => 0,
    };
    let mut kinds: Vec<Option<TagKind>> = vec![None; w.len() + 1];
    for i in (0..w.len()).filter(|&i| w.is_char_boundary(i)) {
        let here = if i == 0 { Some(Source) } else { kinds[i] };
        let Some(here) = here else { continue };
        let rest = &w[i..];
        for (list, kind) in [(CJK_SUBS, Language), (CJK_AUDIO, Audio), (CJK_QUALITY, Source)] {
            for m in list.iter().filter(|m| rest.starts_with(**m)) {
                let k = if rank(here) > rank(kind) { here } else { kind };
                let j = i + m.len();
                if kinds[j].is_none_or(|old| rank(k) > rank(old)) {
                    kinds[j] = Some(k);
                }
            }
        }
    }
    kinds[w.len()].and_then(strong)
}

/// A fansub group or sharing site written in CJK (`某某字幕组`, `某某影视`):
/// a tag inside a name, and a release group at its end.
pub(crate) fn is_cjk_group_name(w: &str) -> bool {
    let chars = w.chars().count();
    has_cjk(w)
        && CJK_GROUP_SUFFIXES
            .iter()
            .any(|sfx| w.ends_with(sfx) && chars > sfx.chars().count() && chars <= 12)
}

/// Notes that fansub releases put in a bracket group of their own:
/// subtitle script (`[GB]`, `[BIG5]`, `[CHS]`, `[JPTC]`), container
/// (`[MP4]`), broadcast and status (`[TV]`, `[END]`, `[V2]`). They are tags
/// only as a whole bracket group (`[GB_MP4]`, `[CHS&JPN]` too), never in
/// running text, where they can be title words ("The End", "TV").
pub(crate) fn is_bracket_tag(word: &str) -> bool {
    let lw = word.to_ascii_lowercase();
    let known = matches!(
        lw.as_str(),
        "gb" | "gbk"
            | "big5"
            | "chs"
            | "cht"
            | "sc"
            | "tc"
            | "jp"
            | "jpn"
            | "jpsc"
            | "jptc"
            | "cn"
            | "chn"
            | "en"
            | "eng"
            | "mp4"
            | "mkv"
            | "avi"
            | "rmvb"
            | "tv"
            | "ova"
            | "oad"
            | "ona"
            | "sp"
            | "end"
            | "fin"
            | "raw"
            | "rev"
    );
    // "v2": a fixed re-release.
    known || (lw.len() == 2 && lw.starts_with('v') && lw.as_bytes()[1].is_ascii_digit())
}

/// Canonical edition for a CJK edition word (`导演剪辑版` → "Director's Cut").
pub(crate) fn cjk_edition(w: &str) -> Option<&'static str> {
    Some(match w {
        "导演剪辑版" | "導演剪輯版" | "导演剪辑" | "導演剪輯" | "导演版" | "導演版" => {
            "Director's Cut"
        }
        "加长版" | "加長版" | "加长剪辑版" | "加長剪輯版" => "Extended",
        "未删减版" | "未刪減版" | "无删减版" | "無刪減版" | "未删减" | "未刪減" | "无删减" | "無刪減" => {
            "Uncut"
        }
        "特别版" | "特別版" => "Special Edition",
        "重制版" | "重製版" | "修复版" | "修復版" => "Remastered",
        "典藏版" | "收藏版" => "Collector's Edition",
        "院线版" | "院線版" | "公映版" => "Theatrical",
        _ => return None,
    })
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

/// A resolution glued to its source, common in Chinese release names:
/// `BD1080P`, `HD720P`, `WEB2160P` → the resolution part.
fn glued_resolution(w: &str) -> Option<&str> {
    ["uhd", "bd", "hd", "web", "dvd"]
        .iter()
        .filter_map(|p| w.strip_prefix(p))
        .find(|rest| rest.len() >= 4 && is_resolution(rest))
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
    let glued = glued_resolution(&w).map(str::to_string);
    let w = glued.unwrap_or(w);
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
        "蓝光" | "藍光" | "原盘" | "原盤" => "BluRay",
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
    if let Some(e) = cjk_edition(&a) {
        return Some((e, 1));
    }
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
    fn cjk_tags() {
        for w in [
            "中英字幕",
            "中英双字",
            "高清中英双字",
            "简繁",
            "简繁双语",
            "双语",
            "官方中文字幕",
            "中英特效字幕",
            "中字",
            "国语中字",
            "内封简繁",
            "某某字幕组",
            "某某影视",
        ] {
            assert!(classify(w).is_some_and(|t| t.strong), "{w}");
        }
        assert_eq!(classify("国语").map(|t| t.kind), Some(TagKind::Audio));
        assert_eq!(classify("粤语").map(|t| t.kind), Some(TagKind::Audio));
        assert_eq!(classify("高清").map(|t| t.kind), Some(TagKind::Source));
        assert_eq!(classify("国语中字").map(|t| t.kind), Some(TagKind::Language));
        assert_eq!(classify("导演剪辑版").map(|t| t.kind), Some(TagKind::Edition));
        // Titles that merely contain a tag morpheme are not tags.
        for w in ["示例剧", "我的国语老师", "字幕组", "中英文学史", "高清人生"] {
            assert!(classify(w).is_none(), "{w}");
        }
        // Mixed words still classify their Latin parts.
        assert!(classify("1080p-某某影视").unwrap().strong);
        assert_eq!(edition_at(&["导演剪辑版", "BD720P"]), Some(("Director's Cut", 1)));
        assert_eq!(canonical_source("蓝光"), Some("BluRay"));
        assert!(classify("BD1080P").unwrap().strong);
        assert_eq!(canonical_resolution("BD720P").as_deref(), Some("720p"));
        assert_eq!(canonical_resolution("WEB2160P").as_deref(), Some("2160p"));
        assert!(classify("HDTV").is_some_and(|t| t.kind == TagKind::Source));
    }

    #[test]
    fn bracket_tags_are_not_running_text_tags() {
        for w in ["GB", "BIG5", "CHT", "JPSC", "MP4", "TV", "END", "v2"] {
            assert!(is_bracket_tag(w), "{w}");
        }
        for w in ["Some", "Movie", "v", "v22", "Film"] {
            assert!(!is_bracket_tag(w), "{w}");
        }
        // "The End", "TV" in a title: not tags in running text.
        assert!(classify("End").is_none() && classify("TV").is_none());
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
