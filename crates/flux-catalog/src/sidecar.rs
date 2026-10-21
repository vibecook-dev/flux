//! Subtitle files beside the videos (sidecars): what the scan records, and
//! which of them belong to an asset.
//!
//! The scan keeps `subtitle_file` current like `file`: every `.srt`, `.ass`,
//! `.ssa`, `.vtt`, `.sup` and `.idx` it walks past, by directory, and rows
//! go when a listing proves the file gone ([`sync`]). Which videos a file
//! belongs to is decided when an asset is read ([`asset_subtitle_files`]),
//! from the names alone, so adding a subtitle file (or a video) anywhere
//! needs no rescan of the other:
//!
//! - **Same name.** `Film.mkv` takes `Film.srt`, `Film.en.srt`,
//!   `Film.chs.forced.ass`, in its folder or a subtitle folder in it
//!   (`Subs/`, `Subtitles/`, `字幕/`, any case).
//! - **Same episode.** An episode (`S01E02`, `1x02`, `第2集`) takes the files
//!   of that episode in its folder, a subtitle folder in it, its parent
//!   folder (a season folder's show folder; never the library root), or a
//!   subtitle folder there. Release tags may differ (`Show.S01E02.1080p.WEB`
//!   takes `Show.S01E02.1080p.<group>.sup`); a different show name before
//!   the episode number doesn't match.
//! - **One video in the folder.** A video that is not an episode, alone in
//!   its folder (not the root), takes every subtitle file there and in its
//!   subtitle folders, whatever their names.
//! - **Per-video folders.** `Subs/<video name>/2_English.srt` (one folder of
//!   subtitles per video) belongs to that video.
//!
//! Language, title and flags come from what the file name adds to the
//! video's (`.en`, `.chs`, `.cht`, `繁`, `中英`, `.forced`, `.sdh`…).

use std::collections::HashMap;

use flux_core::api::Id;
use flux_core::{SubtitleFile, SUBTITLE_FILE_TRACK_BASE};
use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;

/// A subtitle file as the scan saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleFileRow {
    /// Root-relative, `/`-separated.
    pub rel_path: String,
    pub size: u64,
    pub mtime_ns: i64,
}

/// `true` for a file name the scan records as a subtitle file.
pub fn is_subtitle_file_name(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && flux_core::subtitle_file_codec(ext).is_some())
}

fn dir_of(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(d, _)| d)
}

fn name_of(rel: &str) -> &str {
    rel.rsplit_once('/').map_or(rel, |(_, n)| n)
}

fn stem_of(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

/// The subtitle files of a root at or below each of `prefixes` (`""` = the
/// whole root) as `rel_path → (size, mtime_ns)`: what a scan of those
/// directories diffs against.
pub fn load_snapshot(
    conn: &Connection,
    root_id: Id,
    prefixes: &[String],
) -> Result<HashMap<String, (u64, i64)>> {
    let mut map = HashMap::new();
    let mut add = |rows: &mut rusqlite::Rows<'_>| -> Result<()> {
        while let Some(r) = rows.next()? {
            map.insert(
                r.get::<_, String>(0)?,
                (r.get::<_, i64>(1)?.max(0) as u64, r.get::<_, i64>(2)?),
            );
        }
        Ok(())
    };
    if prefixes.iter().any(|p| p.trim_matches('/').is_empty()) {
        let mut stmt =
            conn.prepare_cached("SELECT rel_path, size, mtime_ns FROM subtitle_file WHERE root_id = ?1")?;
        add(&mut stmt.query([root_id])?)?;
    } else {
        // `prefix/` ≤ rel_path < `prefix0` ('0' follows '/'), as for files.
        let mut stmt = conn.prepare_cached(
            "SELECT rel_path, size, mtime_ns FROM subtitle_file
              WHERE root_id = ?1 AND rel_path >= ?2 || '/' AND rel_path < ?2 || '0'",
        )?;
        for p in prefixes {
            add(&mut stmt.query(params![root_id, p.trim_matches('/')])?)?;
        }
    }
    Ok(map)
}

/// Writes a scan's subtitle files of a root: `upserts` new or changed,
/// `removed` (relative paths) proven gone. Returns the rows changed.
pub fn sync(conn: &Connection, root_id: Id, upserts: &[SubtitleFileRow], removed: &[String]) -> Result<u64> {
    let mut n = 0;
    let mut up = conn.prepare_cached(
        "INSERT INTO subtitle_file(root_id, rel_path, dir, size, mtime_ns) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(root_id, rel_path) DO UPDATE SET size = excluded.size, mtime_ns = excluded.mtime_ns",
    )?;
    for f in upserts {
        n += up.execute(params![
            root_id,
            f.rel_path,
            dir_of(&f.rel_path),
            crate::model::clamp_i64(f.size),
            f.mtime_ns
        ])? as u64;
    }
    let mut del = conn.prepare_cached("DELETE FROM subtitle_file WHERE root_id = ?1 AND rel_path = ?2")?;
    for rel in removed {
        n += del.execute(params![root_id, rel])? as u64;
    }
    Ok(n)
}

// ---- names ------------------------------------------------------------------

/// Folder names that hold subtitles (compared without case).
const SUBTITLE_FOLDERS: &[&str] = &["subs", "sub", "subtitles", "subtitle", "字幕"];

fn is_separator(c: char) -> bool {
    matches!(
        c,
        '.' | '_'
            | ' '
            | '-'
            | '['
            | ']'
            | '('
            | ')'
            | '+'
            | '&'
            | ','
            | '【'
            | '】'
            | '「'
            | '」'
            | '《'
            | '》'
    )
}

/// Season (if stated) and episode of a file stem, and where the episode
/// token starts: `S01E02`, `s1e2`, `1x02`, `第2集` / `第02话`.
fn episode_of(stem: &str) -> Option<(Option<u32>, u32, usize)> {
    let b = stem.as_bytes();
    let digits_at = |i: usize, max: usize| -> (usize, Option<u32>) {
        let n = b[i..]
            .iter()
            .take(max + 1)
            .take_while(|c| c.is_ascii_digit())
            .count();
        if n == 0 || n > max {
            return (n, None);
        }
        (n, stem[i..i + n].parse().ok())
    };
    let boundary_before = |i: usize| i == 0 || !b[i - 1].is_ascii_alphanumeric();
    let boundary_after = |i: usize| i >= b.len() || !b[i].is_ascii_digit();
    for i in 0..b.len() {
        // S01E02
        if (b[i] == b's' || b[i] == b'S') && boundary_before(i) {
            let (ns, season) = digits_at(i + 1, 2);
            let e = i + 1 + ns;
            if let Some(season) = season {
                if e < b.len() && (b[e] == b'e' || b[e] == b'E') {
                    let (ne, ep) = digits_at(e + 1, 3);
                    if let Some(ep) = ep.filter(|_| boundary_after(e + 1 + ne)) {
                        return Some((Some(season), ep, i));
                    }
                }
            }
        }
        // 1x02
        if b[i].is_ascii_digit() && boundary_before(i) {
            let (ns, season) = digits_at(i, 2);
            let x = i + ns;
            if let Some(season) = season {
                if x < b.len() && (b[x] == b'x' || b[x] == b'X') {
                    let (ne, ep) = digits_at(x + 1, 3);
                    if let Some(ep) = ep.filter(|_| ne >= 2 && boundary_after(x + 1 + ne)) {
                        return Some((Some(season), ep, i));
                    }
                }
            }
        }
    }
    // 第2集, 第02话
    let at = stem.find('第')?;
    let rest = &stem[at + '第'.len_utf8()..];
    let n: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let after = rest[n.len()..].chars().next()?;
    if !n.is_empty() && matches!(after, '集' | '话' | '話') {
        return Some((None, n.parse().ok()?, at));
    }
    None
}

/// The Latin letters and digits of a show name, lower case: what two names
/// of an episode must share (`示例剧.Some.Show` and `Some Show`
/// agree; a name in CJK only says nothing either way).
fn latin_name(s: &str) -> String {
    s.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// `s` without `prefix`, compared without case.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let mut it = s.char_indices();
    for p in prefix.chars() {
        let (_, c) = it.next()?;
        if !c.to_lowercase().eq(p.to_lowercase()) {
            return None;
        }
    }
    Some(it.next().map_or("", |(i, _)| &s[i..]))
}

/// What `sub` adds to `video`: after their common start, cut back to a
/// separator (`Show.S01E02.1080p.WEB` / `Show.S01E02.1080p.chs` → `chs`).
fn added_part<'a>(sub: &'a str, video: &str) -> &'a str {
    let mut end = 0;
    for ((i, a), b) in sub.char_indices().zip(video.chars()) {
        if !a.to_lowercase().eq(b.to_lowercase()) {
            break;
        }
        end = i + a.len_utf8();
    }
    let boundary = end == 0
        || end == sub.len()
        || sub[..end].ends_with(is_separator)
        || sub[end..].starts_with(is_separator);
    if !boundary {
        // The shared part ends inside a word: back to its last separator.
        end = sub[..end].rfind(is_separator).map_or(0, |i| i + 1);
    }
    sub[end..].trim_start_matches(is_separator)
}

/// Language, title and flags read from what a subtitle file name adds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NameTags {
    pub language: Option<String>,
    pub title: Option<String>,
    pub forced: bool,
    pub hearing_impaired: bool,
}

fn language_code(token: &str) -> Option<&'static str> {
    Some(match token.to_lowercase().as_str() {
        "en" | "eng" | "english" | "英" | "英文" | "英语" | "英語" => "eng",
        "zh" | "chi" | "zho" | "chn" | "chinese" | "中" | "中文" | "汉语" | "漢語" => "chi",
        "chs" | "sc" | "gb" | "zhs" | "zh-cn" | "zh_cn" | "zhcn" | "zh-hans" | "hans" | "简" | "简体"
        | "简中" | "简体中文" | "簡體" => "zh-Hans",
        "cht" | "tc" | "big5" | "zht" | "zh-tw" | "zh_tw" | "zhtw" | "zh-hk" | "zh-hant" | "hant" | "繁"
        | "繁体" | "繁體" | "繁中" | "繁体中文" | "繁體中文" => "zh-Hant",
        "ja" | "jp" | "jpn" | "japanese" | "日" | "日文" | "日语" | "日語" => "jpn",
        "ko" | "kor" | "korean" | "韩" | "韓" | "韩文" | "韩语" | "韓語" => "kor",
        "fr" | "fre" | "fra" | "french" => "fre",
        "de" | "ger" | "deu" | "german" => "ger",
        "es" | "spa" | "spanish" => "spa",
        "it" | "ita" | "italian" => "ita",
        "pt" | "por" | "portuguese" | "pt-br" | "ptbr" => "por",
        "ru" | "rus" | "russian" => "rus",
        "nl" | "dut" | "nld" | "dutch" => "dut",
        "sv" | "swe" | "swedish" => "swe",
        "no" | "nor" | "norwegian" => "nor",
        "da" | "dan" | "danish" => "dan",
        "fi" | "fin" | "finnish" => "fin",
        "pl" | "pol" | "polish" => "pol",
        "tr" | "tur" | "turkish" => "tur",
        "ar" | "ara" | "arabic" => "ara",
        "he" | "heb" | "hebrew" => "heb",
        "th" | "tha" | "thai" => "tha",
        "vi" | "vie" | "vietnamese" => "vie",
        "cs" | "cze" | "ces" | "czech" => "cze",
        "hu" | "hun" | "hungarian" => "hun",
        "el" | "gre" | "ell" | "greek" => "gre",
        "id" | "ind" | "indonesian" => "ind",
        "ms" | "may" | "msa" | "malay" => "may",
        _ => return None,
    })
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF)
}

/// The language a run of CJK text names (`中英特效字幕`, `简体中文`, `繁中`).
fn cjk_language(token: &str) -> Option<&'static str> {
    if !token.chars().any(is_cjk) {
        return None;
    }
    let has = |s: &str| token.contains(s);
    if has("简") || has("簡") {
        Some("zh-Hans")
    } else if has("繁") {
        Some("zh-Hant")
    } else if has("中") || has("汉") || has("漢") || has("双语") || has("雙語") {
        Some("chi")
    } else if has("英") {
        Some("eng")
    } else if has("日") {
        Some("jpn")
    } else if has("韩") || has("韓") {
        Some("kor")
    } else {
        None
    }
}

/// Release-name words that say nothing about the subtitles.
fn is_noise(token: &str) -> bool {
    let t = token.to_ascii_lowercase();
    let b = t.as_bytes();
    let resolution = b.len() >= 4
        && (b[b.len() - 1] == b'p' || b[b.len() - 1] == b'i')
        && b[..b.len() - 1].iter().all(u8::is_ascii_digit);
    // Also a lone letter (`h.264` split at its dot).
    b.iter().all(u8::is_ascii_digit)
        || (b.len() == 1 && b[0].is_ascii_alphabetic())
        || resolution
        || episode_of(token).is_some_and(|(_, _, at)| at == 0)
        || matches!(
            t.as_str(),
            "web"
                | "webrip"
                | "webdl"
                | "dl"
                | "bluray"
                | "bdrip"
                | "brrip"
                | "hdtv"
                | "dvdrip"
                | "x264"
                | "x265"
                | "h264"
                | "h265"
                | "hevc"
                | "avc"
                | "aac"
                | "ac3"
                | "dts"
                | "10bit"
                | "hdr"
                | "remux"
                | "proper"
                | "repack"
                | "default"
                | "subs"
                | "sub"
                | "subtitle"
                | "subtitles"
        )
}

/// Reads [`NameTags`] from what a subtitle file name adds to its video's
/// (`added`, see the module docs).
pub fn name_tags(added: &str) -> NameTags {
    let mut tags = NameTags::default();
    // Tags with a hyphen in them (`zh-CN`, `pt-BR`) before splitting at it.
    let mut text = added.to_string();
    for tag in ["zh-cn", "zh-hans", "zh-tw", "zh-hk", "zh-hant", "pt-br"] {
        let b = text.as_bytes();
        let at = (0..b.len().saturating_sub(tag.len() - 1)).find(|&i| {
            b[i..i + tag.len()].eq_ignore_ascii_case(tag.as_bytes())
                && (i == 0 || is_separator(b[i - 1] as char))
                && b.get(i + tag.len()).is_none_or(|&c| is_separator(c as char))
        });
        if let Some(i) = at {
            tags.language = language_code(tag).map(str::to_string);
            text.replace_range(i..i + tag.len(), ".");
            break;
        }
    }
    let mut title: Vec<&str> = Vec::new();
    for token in text.split(is_separator).filter(|t| !t.is_empty()) {
        let lower = token.to_lowercase();
        if let Some(l) = language_code(token) {
            tags.language.get_or_insert_with(|| l.to_string());
            continue;
        }
        match lower.as_str() {
            "forced" | "force" | "强制" | "強制" => {
                tags.forced = true;
                continue;
            }
            "sdh" | "cc" | "hi" | "hoh" => {
                tags.hearing_impaired = true;
                continue;
            }
            _ => {}
        }
        if let Some(l) = cjk_language(token) {
            tags.language.get_or_insert_with(|| l.to_string());
            // `中英特效字幕` says more than its language: keep it as the title.
            title.push(token);
            continue;
        }
        if !is_noise(token) {
            title.push(token);
        }
    }
    if !title.is_empty() {
        tags.title = Some(title.join(" "));
    }
    tags
}

/// How a subtitle file's folder relates to its video's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Place {
    /// The video's folder.
    Same,
    /// A subtitle folder in the video's folder.
    SubsFolder,
    /// The video's own folder inside a subtitle folder
    /// (`Subs/<video name>/`), of its folder or (episodes) the parent's.
    PerVideo,
    /// The video's parent folder (episodes).
    Parent,
    /// A subtitle folder in the parent folder (episodes).
    SiblingSubs,
}

/// A video an asset plays, as the matcher sees it.
#[derive(Debug, Clone)]
pub struct VideoName<'a> {
    /// Its folder, root-relative (`""` = the root).
    pub dir: &'a str,
    /// Its file name without the extension.
    pub stem: &'a str,
    /// The only video directly in its folder (and not at the root).
    pub alone: bool,
}

/// The folders whose subtitle files may belong to `v`, with their place
/// (lookups in `subtitle_file(root_id, dir)`).
fn candidate_dirs(v: &VideoName<'_>) -> Vec<(String, Place)> {
    let join = |a: &str, b: &str| {
        if a.is_empty() {
            b.to_string()
        } else {
            format!("{a}/{b}")
        }
    };
    let mut out = vec![(v.dir.to_string(), Place::Same)];
    for f in SUBTITLE_FOLDERS {
        out.push((join(v.dir, f), Place::SubsFolder));
        out.push((join(&join(v.dir, f), v.stem), Place::PerVideo));
    }
    // An episode's show folder, never the library root (whose files and
    // subtitle folders would be every show's).
    let parent = dir_of(v.dir);
    if episode_of(v.stem).is_some() && !parent.is_empty() {
        out.push((parent.to_string(), Place::Parent));
        for f in SUBTITLE_FOLDERS {
            out.push((join(parent, f), Place::SiblingSubs));
            out.push((join(&join(parent, f), v.stem), Place::PerVideo));
        }
    }
    out
}

/// Whether the subtitle file `sub_stem` in a folder at `place` belongs to
/// video `v`, and if so what its name adds (see the module docs).
fn belongs<'a>(v: &VideoName<'_>, place: Place, sub_stem: &'a str) -> Option<&'a str> {
    if place == Place::PerVideo {
        return Some(sub_stem);
    }
    // Same name, with tags after it.
    if let Some(rest) = strip_prefix_ci(sub_stem, v.stem) {
        if rest.is_empty() || rest.starts_with(is_separator) {
            return Some(rest.trim_start_matches(is_separator));
        }
    }
    // Same episode, under the same show name (or none).
    if let (Some((vs, ve, vat)), Some((ss, se, sat))) = (episode_of(v.stem), episode_of(sub_stem)) {
        let season_ok = vs == ss || vs.is_none() || ss.is_none();
        let (vp, sp) = (latin_name(&v.stem[..vat]), latin_name(&sub_stem[..sat]));
        let show_ok = vp.is_empty() || sp.is_empty() || vp == sp;
        if ve == se && season_ok && show_ok {
            return Some(added_part(sub_stem, v.stem));
        }
        return None;
    }
    // Alone in its folder: everything there is its.
    if v.alone && episode_of(v.stem).is_none() && matches!(place, Place::Same | Place::SubsFolder) {
        return Some(added_part(sub_stem, v.stem));
    }
    None
}

/// The subtitle files of one asset ([`AssetInfo::subtitle_files`]), best
/// placed first: track numbers are `SUBTITLE_FILE_TRACK_BASE + row id`, so
/// they stay the same while the file is cataloged.
///
/// [`AssetInfo::subtitle_files`]: flux_core::api::AssetInfo::subtitle_files
pub fn asset_subtitle_files(conn: &Connection, asset_id: Id) -> Result<Vec<SubtitleFile>> {
    let found: Option<(Id, String, String)> = conn
        .prepare_cached(
            "SELECT f.root_id, f.rel_path, r.path FROM asset a JOIN file f ON f.id = a.file_id
               JOIN root r ON r.id = f.root_id WHERE a.id = ?1",
        )?
        .query_row([asset_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    let Some((root_id, rel, root_path)) = found else {
        return Ok(Vec::new());
    };
    let dir = dir_of(&rel);
    let stem = stem_of(name_of(&rel));
    let alone = !dir.is_empty() && videos_in(conn, root_id, dir)? == 1;
    let video = VideoName { dir, stem, alone };
    let mut stmt = conn.prepare_cached(
        "SELECT id, rel_path, size, mtime_ns FROM subtitle_file WHERE root_id = ?1 AND dir = ?2",
    )?;
    let mut out: Vec<(Place, SubtitleFile)> = Vec::new();
    for (d, place) in candidate_dirs(&video) {
        let mut rows = stmt.query(params![root_id, d])?;
        while let Some(r) = rows.next()? {
            let id: Id = r.get(0)?;
            let rel_path: String = r.get(1)?;
            // NOCASE also matched `subs/` for `Subs/`: compare the place again.
            if !dir_of(&rel_path).eq_ignore_ascii_case(&d)
                || out.iter().any(|(_, f)| f.path.ends_with(&rel_path))
            {
                continue;
            }
            let name = name_of(&rel_path);
            let Some((sub_stem, ext)) = name.rsplit_once('.') else {
                continue;
            };
            let Some(codec) = flux_core::subtitle_file_codec(ext) else {
                continue;
            };
            let Some(track) = u32::try_from(id)
                .ok()
                .and_then(|i| i.checked_add(SUBTITLE_FILE_TRACK_BASE))
            else {
                continue;
            };
            let Some(added) = belongs(&video, place, sub_stem) else {
                continue;
            };
            let tags = name_tags(added);
            out.push((
                place,
                SubtitleFile {
                    track,
                    path: crate::read::path_join(&root_path, &rel_path),
                    codec,
                    language: tags.language,
                    title: tags.title,
                    forced: tags.forced,
                    hearing_impaired: tags.hearing_impaired,
                    size: r.get::<_, i64>(2)?.max(0) as u64,
                    mtime_ns: r.get(3)?,
                },
            ));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.path.cmp(&b.1.path)));
    Ok(out.into_iter().map(|(_, f)| f).collect())
}

/// Video files directly in folder `dir` of a root (not below it).
fn videos_in(conn: &Connection, root_id: Id, dir: &str) -> Result<usize> {
    let mut stmt = conn.prepare_cached(
        "SELECT rel_path FROM file WHERE root_id = ?1 AND state <> 'missing'
           AND rel_path >= ?2 || '/' AND rel_path < ?2 || '0'",
    )?;
    let mut rows = stmt.query(params![root_id, dir])?;
    let mut n = 0;
    while let Some(r) = rows.next()? {
        let rel: String = r.get(0)?;
        if !rel[dir.len() + 1..].contains('/') {
            n += 1;
            if n > 1 {
                break;
            }
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video<'a>(rel: &'a str, alone: bool) -> VideoName<'a> {
        VideoName {
            dir: dir_of(rel),
            stem: stem_of(name_of(rel)),
            alone,
        }
    }

    /// The matcher over plain paths: which of `subs` belong to `video_rel`.
    fn matched(video_rel: &str, alone: bool, subs: &[&str]) -> Vec<String> {
        let v = video(video_rel, alone);
        let dirs = candidate_dirs(&v);
        let mut out = Vec::new();
        for s in subs {
            let d = dir_of(s);
            let Some((_, place)) = dirs.iter().find(|(c, _)| c.eq_ignore_ascii_case(d)) else {
                continue;
            };
            if belongs(&v, *place, stem_of(name_of(s))).is_some() {
                out.push(s.to_string());
            }
        }
        out
    }

    #[test]
    fn episodes() {
        assert_eq!(episode_of("Show.S01E02.1080p"), Some((Some(1), 2, 5)));
        assert_eq!(episode_of("show s1e12 x264"), Some((Some(1), 12, 5)));
        assert_eq!(episode_of("Show 2x05"), Some((Some(2), 5, 5)));
        assert_eq!(episode_of("示例剧_第01集"), Some((None, 1, "示例剧_".len())));
        assert_eq!(episode_of("Film.2010.1080p.x264"), None);
        assert_eq!(episode_of("1920x1080"), None);
        assert_eq!(episode_of("Mass.Effect"), None);
    }

    #[test]
    fn same_name_with_tags() {
        let subs = [
            "Films/Film (2010)/Film (2010).srt",
            "Films/Film (2010)/Film (2010).en.forced.srt",
            "Films/Film (2010)/Subs/Film (2010).chs.ass",
            "Films/Film (2010)/Film (2010) Extras.srt",
            "Films/Film (2010)/Other.srt",
        ];
        // Two videos in the folder: only the same-name files (a word after
        // the name is a title: "Extras").
        assert_eq!(
            matched("Films/Film (2010)/Film (2010).mkv", false, &subs),
            subs[..4].to_vec()
        );
        // Alone in its folder: everything (the folder is the film's).
        assert_eq!(matched("Films/Film (2010)/Film (2010).mkv", true, &subs).len(), 5);
        // Never across folders that aren't subtitle folders.
        assert!(matched("Films/Other/Other.mkv", true, &subs).is_empty());
    }

    #[test]
    fn episode_files_in_a_versions_subfolder() {
        // A show folder with MP4s, and a subfolder of MKV versions whose
        // .sup files carry the MP4s' names: each .sup belongs to the MKV of
        // its episode beside it, not to the MP4s above.
        let subs = [
            "示例剧/另一版本/Some.Show.S01E01.1080p.中英字幕.某字幕组.sup",
            "示例剧/另一版本/Some.Show.S01E02.1080p.中英字幕.某字幕组.sup",
        ];
        let mkv = "示例剧/另一版本/Some.Show.S01E01.1080p.WEB.h264-GRP.mkv";
        assert_eq!(matched(mkv, false, &subs), vec![subs[0].to_string()]);
        let mp4 = "示例剧/Some.Show.S01E01.1080p.中英字幕.某字幕组.mp4";
        assert!(matched(mp4, false, &subs).is_empty());
        // What the name adds: language and title.
        let added = added_part(stem_of(name_of(subs[0])), stem_of(name_of(mkv)));
        assert_eq!(added, "中英字幕.某字幕组");
        let t = name_tags(added);
        assert_eq!(t.language.as_deref(), Some("chi"));
        assert_eq!(t.title.as_deref(), Some("中英字幕 某字幕组"));
    }

    #[test]
    fn episode_files_in_parent_and_sibling_folders() {
        let subs = [
            "Show/Show.S01E02.en.srt",
            "Show/Subs/S01E02.chs.ass",
            "Show/Subs/S01E03.chs.ass",
            "Show/Subtitles/Show.S01E02/2_English.srt",
            "Show/Season 1/Other.Show.S01E02.srt",
            "Show.S01E02.srt",
        ];
        let got = matched("Show/Season 1/Show.S01E02.1080p.mkv", false, &subs);
        assert_eq!(got, subs[..2].to_vec());
        // RARBG-style per-video folders.
        let got = matched("Show/Season 1/Show.S01E02.mkv", false, &subs);
        assert!(got.contains(&subs[3].to_string()), "{got:?}");
        // The library root is never another show's parent.
        let got = matched("Show2/Show2.S01E02.mkv", false, &subs);
        assert!(got.is_empty(), "{got:?}");
    }

    #[test]
    fn tags_from_names() {
        let t = name_tags("en.forced");
        assert_eq!(
            (t.language.as_deref(), t.forced, t.title.as_deref()),
            (Some("eng"), true, None)
        );
        assert_eq!(name_tags("chs").language.as_deref(), Some("zh-Hans"));
        assert_eq!(name_tags("cht").language.as_deref(), Some("zh-Hant"));
        assert_eq!(name_tags("繁").language.as_deref(), Some("zh-Hant"));
        assert_eq!(name_tags("简体中文").language.as_deref(), Some("zh-Hans"));
        assert_eq!(name_tags("zh-CN").language.as_deref(), Some("zh-Hans"));
        assert_eq!(name_tags("zh-CN").title, None);
        let t = name_tags("eng.SDH");
        assert!(t.hearing_impaired && t.language.as_deref() == Some("eng"));
        let t = name_tags("Commentary.en");
        assert_eq!(
            (t.language.as_deref(), t.title.as_deref()),
            (Some("eng"), Some("Commentary"))
        );
        // Release noise is not a title.
        assert_eq!(name_tags("2010.720p.WEB.x264").title, None);
        assert_eq!(name_tags("WEB-DL.h.264.chs").title, None);
        assert_eq!(name_tags("2_English").language.as_deref(), Some("eng"));
        assert_eq!(name_tags("").language, None);
    }

    #[test]
    fn subtitle_file_names() {
        assert!(is_subtitle_file_name("Film.en.srt"));
        assert!(is_subtitle_file_name("Ep.SUP"));
        assert!(!is_subtitle_file_name("Film.sub"));
        assert!(!is_subtitle_file_name(".srt"));
        assert!(!is_subtitle_file_name("Film.nfo"));
    }

    #[test]
    fn prefixes_ignore_case() {
        assert_eq!(strip_prefix_ci("FILM.en", "film"), Some(".en"));
        assert_eq!(strip_prefix_ci("film", "film"), Some(""));
        assert_eq!(strip_prefix_ci("fil", "film"), None);
        assert_eq!(added_part("Show.S01E01.chs", "Show.S01E01"), "chs");
        assert_eq!(added_part("Show.S01E01.chs", "Show.S01E01.1080p"), "chs");
        assert_eq!(
            added_part("Show.S01E01.WEBRip.chs", "Show.S01E01.WEB-DL"),
            "WEBRip.chs"
        );
        assert_eq!(added_part("S01E01.chs", "Show.S01E01.mkv"), "S01E01.chs");
    }
}
