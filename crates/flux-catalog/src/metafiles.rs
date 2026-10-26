//! Metadata files beside the videos: a `.flux.json` (flux's own sidecar,
//! [`flux_core::agent::Sidecar`]) or a Kodi `.nfo`. The scan records them
//! like subtitle files (`meta_file`, by directory), with what flux read
//! from them; which videos one describes is decided from the names:
//!
//! - `<video name>.flux.json` and `<video name>.nfo` describe that video
//!   (a film, an episode, an extra);
//! - `flux.json` and `movie.nfo` in a film's own folder describe the film;
//! - `flux.json` and `tvshow.nfo` in a show's folder describe the show.

use std::collections::{BTreeSet, HashMap};

use flux_core::agent::{FileFix, Link, Sidecar, SIDECAR};
use flux_core::api::Id;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::Result;
use crate::fields::{self, FieldRow, Values};

/// `meta_file.format` of a `.flux.json`.
pub const FLUX: &str = "flux";
/// `meta_file.format` of a Kodi `.nfo`.
pub const NFO: &str = "nfo";

/// The format of a metadata file the scan records, by its name.
pub fn format_of(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".flux.json") || lower == "flux.json" {
        Some(FLUX)
    } else if lower.ends_with(".nfo") && lower.len() > 4 {
        Some(NFO)
    } else {
        None
    }
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

/// An episode file's show folder as it is on disk (root-relative): its
/// folder without trailing season folders; `None` at the library folder.
/// ([`crate::group::show_folder`] is the same, lowercased, for grouping.)
pub fn show_dir(rel_path: &str) -> Option<String> {
    let (dir, _) = rel_path.rsplit_once('/')?;
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    while parts.last().is_some_and(|p| crate::group::is_season_dir(p)) {
        parts.pop();
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// What a metadata file describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// The video named like it.
    Video,
    /// The film whose folder it's in.
    Folder,
    /// The show whose folder it's in.
    Show,
}

/// What a metadata file named `name` in a folder describes, for a video
/// whose name (without extension) is `stem` in that folder.
fn scope_beside(name: &str, stem: &str) -> Option<Scope> {
    let lower = name.to_ascii_lowercase();
    let stem = stem.to_ascii_lowercase();
    if lower == format!("{stem}.flux.json") || lower == format!("{stem}.nfo") {
        Some(Scope::Video)
    } else if lower == "flux.json" || lower == "movie.nfo" {
        Some(Scope::Folder)
    } else {
        None
    }
}

/// The metadata files in `dir` of a root, `(rel_path, format)`.
fn in_dir(c: &Connection, root_id: Id, dir: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = c.prepare_cached(
        "SELECT rel_path, format FROM meta_file WHERE root_id = ?1 AND dir = ?2 ORDER BY rel_path",
    )?;
    let rows = stmt.query_map(params![root_id, dir], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The metadata files that describe the video at `rel_path` of a root, or
/// the film or show it belongs to (`show_folder`: an episode's show folder,
/// root-relative), with what each describes: the video's own first.
pub fn for_video(
    c: &Connection,
    root_id: Id,
    rel_path: &str,
    show_folder: Option<&str>,
) -> Result<Vec<(String, String, Scope)>> {
    let dir = dir_of(rel_path);
    let stem = stem_of(name_of(rel_path));
    let mut out = Vec::new();
    for (rel, format) in in_dir(c, root_id, dir)? {
        match scope_beside(name_of(&rel), stem) {
            Some(Scope::Video) => out.push((rel, format, Scope::Video)),
            // A film's folder file, only in a folder of its own (not the
            // library folder itself) and not for an episode.
            Some(Scope::Folder) if !dir.is_empty() && show_folder.is_none() => {
                out.push((rel, format, Scope::Folder))
            }
            _ => {}
        }
    }
    if let Some(show) = show_folder.filter(|s| !s.is_empty()) {
        for (rel, format) in in_dir(c, root_id, show)? {
            let lower = name_of(&rel).to_ascii_lowercase();
            if lower == "tvshow.nfo" || lower == "flux.json" {
                out.push((rel, format, Scope::Show));
            }
        }
    }
    out.sort_by_key(|(_, f, s)| (*s, f.as_str() != FLUX));
    Ok(out)
}

/// The metadata files beside the file `file_id` (root-relative paths), as
/// `item.get` lists them.
pub fn beside(c: &Connection, file_id: Id) -> Result<Vec<String>> {
    let row: Option<(Id, String)> = c
        .prepare_cached("SELECT root_id, rel_path FROM file WHERE id = ?1")?
        .query_row([file_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .ok();
    let Some((root_id, rel)) = row else {
        return Ok(Vec::new());
    };
    let show = show_dir(&rel);
    let episode: bool = c
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM file f JOIN asset a ON a.id = f.asset_id JOIN item i ON i.id = a.item_id
                             WHERE f.id = ?1 AND i.kind = 'episode')",
        )?
        .query_row([file_id], |r| r.get(0))?;
    let show = if episode { show } else { None };
    Ok(for_video(c, root_id, &rel, show.as_deref())?
        .into_iter()
        .map(|(rel, _, _)| rel)
        .collect())
}

// ---- what a metadata file says -------------------------------------------------------------

/// Most bytes of a metadata file flux reads.
pub const MAX_BYTES: u64 = 1 << 20;

/// What flux read from a metadata file (`meta_file.content`): facts as
/// field values (checked against an item's kind when applied), and for a
/// `.flux.json` what the video is and a show's episodes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Content {
    /// An NFO's kind: `movie`, `show` or `episode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Values::is_empty")]
    pub fields: Values,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<FileFix>,
    /// `(season, episode, fields)`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub episodes: Vec<(u16, u16, Values)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<Link>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

/// Read a metadata file of `format` (its bytes): what it says, or why it
/// can't be read.
pub fn parse(format: &str, bytes: &[u8]) -> std::result::Result<Content, String> {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_start_matches('\u{feff}');
    match format {
        FLUX => parse_flux(text),
        NFO => parse_nfo(text),
        other => Err(format!("{other:?} isn't a metadata file flux reads")),
    }
}

fn parse_flux(text: &str) -> std::result::Result<Content, String> {
    let s: Sidecar = serde_json::from_str(text).map_err(|e| format!("not a flux sidecar: {e}"))?;
    if s.flux != SIDECAR {
        return Err(format!("\"flux\" must be {SIDECAR:?}, not {:?}", s.flux));
    }
    let mut c = Content {
        fields: fields::from_fields(&s.fields)?,
        fix: s.fix.filter(|f| !f.is_empty()),
        sources: s.sources,
        by: s
            .by
            .map(|b| b.trim().chars().take(80).collect())
            .filter(|b: &String| !b.is_empty()),
        ..Content::default()
    };
    for e in s.episodes.into_iter().take(2000) {
        c.episodes
            .push((e.season, e.episode, fields::from_fields(&e.fields)?));
    }
    Ok(c)
}

/// An XML element, as much of one as an NFO needs.
#[derive(Debug, Default, Clone, PartialEq)]
struct El {
    name: String,
    attrs: Vec<(String, String)>,
    text: String,
    kids: Vec<El>,
}

impl El {
    fn attr(&self, k: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    }

    fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a El> + 'a {
        self.kids
            .iter()
            .filter(move |k| k.name.eq_ignore_ascii_case(name))
    }

    fn first(&self, name: &str) -> Option<&El> {
        self.kids.iter().find(|k| k.name.eq_ignore_ascii_case(name))
    }

    /// The text of the first `name` child, tidied; `None` if empty.
    fn text_of(&self, name: &str) -> Option<String> {
        self.first(name).map(|e| tidy(&e.text)).filter(|t| !t.is_empty())
    }
}

fn tidy(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `&amp;`, `&#38;`, `&#x26;` and the other XML entities.
fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|e| *e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..end];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            e if e.starts_with("#x") || e.starts_with("#X") => {
                u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32)
            }
            e if e.starts_with('#') => e[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The top-level elements of an XML document, forgivingly: declarations,
/// comments and doctypes are skipped, CDATA is text, a close tag that
/// doesn't match closes up to its match.
fn xml(s: &str) -> std::result::Result<Vec<El>, String> {
    let mut stack: Vec<El> = vec![El::default()];
    let mut rest = s;
    let mut count = 0usize;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            if let Some(top) = stack.last_mut() {
                top.text.push_str(&unescape(rest));
            }
            break;
        };
        if lt > 0 {
            if let Some(top) = stack.last_mut() {
                top.text.push_str(&unescape(&rest[..lt]));
            }
        }
        rest = &rest[lt..];
        if let Some(r) = rest.strip_prefix("<!--") {
            rest = r.find("-->").map_or("", |e| &r[e + 3..]);
            continue;
        }
        if let Some(r) = rest.strip_prefix("<![CDATA[") {
            let end = r.find("]]>").unwrap_or(r.len());
            if let Some(top) = stack.last_mut() {
                top.text.push_str(&r[..end]);
            }
            rest = r.get(end + 3..).unwrap_or("");
            continue;
        }
        if rest.starts_with("<?") || rest.starts_with("<!") {
            rest = rest.find('>').map_or("", |e| &rest[e + 1..]);
            continue;
        }
        let Some(gt) = rest.find('>') else { break };
        let tag = &rest[1..gt];
        rest = &rest[gt + 1..];
        if let Some(name) = tag.strip_prefix('/') {
            let name = name.trim();
            if let Some(pos) = stack.iter().rposition(|e| e.name.eq_ignore_ascii_case(name)) {
                while stack.len() > pos.max(1) {
                    let done = stack.pop().unwrap_or_default();
                    if let Some(parent) = stack.last_mut() {
                        parent.kids.push(done);
                    }
                }
            }
            continue;
        }
        let selfclose = tag.ends_with('/');
        let tag = tag.trim_end_matches('/').trim();
        let (name, attrs_src) = tag.split_once(char::is_whitespace).unwrap_or((tag, ""));
        let mut el = El {
            name: name.to_string(),
            ..El::default()
        };
        let mut a = attrs_src.trim();
        while let Some(eq) = a.find('=') {
            let key = a[..eq].trim().to_string();
            let v = a[eq + 1..].trim_start();
            let (val, after) = match v.chars().next() {
                Some(q @ ('"' | '\'')) => match v[1..].find(q) {
                    Some(e) => (&v[1..e + 1], &v[e + 2..]),
                    None => (&v[1..], ""),
                },
                _ => match v.find(char::is_whitespace) {
                    Some(e) => (&v[..e], &v[e..]),
                    None => (v, ""),
                },
            };
            el.attrs.push((key, unescape(val)));
            a = after.trim();
        }
        count += 1;
        if count > 20_000 || stack.len() > 32 {
            return Err("too large or too deep to be an NFO".into());
        }
        if selfclose {
            if let Some(top) = stack.last_mut() {
                top.kids.push(el);
            }
        } else {
            stack.push(el);
        }
    }
    while stack.len() > 1 {
        let done = stack.pop().unwrap_or_default();
        if let Some(parent) = stack.last_mut() {
            parent.kids.push(done);
        }
    }
    Ok(stack.pop().map(|d| d.kids).unwrap_or_default())
}

/// A country name an NFO writes, as the ISO code flux keeps.
fn country_code(name: &str) -> Option<&'static str> {
    let n = name.trim().to_ascii_lowercase();
    if n.len() == 2 && n.chars().all(|c| c.is_ascii_alphabetic()) {
        return COUNTRIES
            .iter()
            .find(|(_, c)| c.eq_ignore_ascii_case(&n))
            .map(|(_, c)| *c);
    }
    COUNTRIES.iter().find(|(full, _)| *full == n).map(|(_, c)| *c)
}

const COUNTRIES: &[(&str, &str)] = &[
    ("united states of america", "US"),
    ("united states", "US"),
    ("usa", "US"),
    ("us", "US"),
    ("united kingdom", "GB"),
    ("uk", "GB"),
    ("great britain", "GB"),
    ("gb", "GB"),
    ("france", "FR"),
    ("germany", "DE"),
    ("west germany", "DE"),
    ("japan", "JP"),
    ("china", "CN"),
    ("hong kong", "HK"),
    ("taiwan", "TW"),
    ("south korea", "KR"),
    ("korea", "KR"),
    ("italy", "IT"),
    ("spain", "ES"),
    ("canada", "CA"),
    ("australia", "AU"),
    ("india", "IN"),
    ("russia", "RU"),
    ("soviet union", "SU"),
    ("sweden", "SE"),
    ("denmark", "DK"),
    ("norway", "NO"),
    ("mexico", "MX"),
    ("brazil", "BR"),
    ("ireland", "IE"),
    ("new zealand", "NZ"),
    ("belgium", "BE"),
    ("netherlands", "NL"),
    ("switzerland", "CH"),
    ("austria", "AT"),
    ("poland", "PL"),
    ("argentina", "AR"),
    ("thailand", "TH"),
    ("iran", "IR"),
    ("israel", "IL"),
    ("turkey", "TR"),
    ("greece", "GR"),
    ("finland", "FI"),
    ("czech republic", "CZ"),
    ("czechia", "CZ"),
    ("hungary", "HU"),
    ("portugal", "PT"),
    ("south africa", "ZA"),
    ("singapore", "SG"),
    ("indonesia", "ID"),
    ("philippines", "PH"),
    ("vietnam", "VN"),
    ("chile", "CL"),
    ("colombia", "CO"),
    ("iceland", "IS"),
    ("romania", "RO"),
    ("egypt", "EG"),
    ("nigeria", "NG"),
    ("malaysia", "MY"),
    ("ukraine", "UA"),
    ("serbia", "RS"),
];

/// An NFO that is only an address (Kodi reads those too): the ids in it.
fn nfo_urls(text: &str) -> Values {
    let mut v = Values::new();
    let lower = text.to_ascii_lowercase();
    if let Some(i) = lower.find("imdb.com/title/tt") {
        let digits: String = lower[i + 15..]
            .chars()
            .skip(2)
            .take_while(char::is_ascii_digit)
            .collect();
        if digits.len() >= 7 {
            v.insert("ids.imdb".into(), Value::String(format!("tt{digits}")));
        }
    }
    for (host, kind) in [
        ("themoviedb.org/movie/", "tmdb"),
        ("themoviedb.org/tv/", "tmdb"),
        ("thetvdb.com/?tab=series&id=", "tvdb"),
    ] {
        if let Some(i) = lower.find(host) {
            let digits: String = lower[i + host.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if !digits.is_empty() {
                v.insert(format!("ids.{kind}"), Value::String(digits));
            }
        }
    }
    v
}

fn parse_nfo(text: &str) -> std::result::Result<Content, String> {
    if !text.trim_start().starts_with('<') {
        let ids = nfo_urls(text);
        if ids.is_empty() {
            return Err("not an NFO: no XML, and no address flux knows".into());
        }
        return Ok(Content {
            fields: ids,
            ..Content::default()
        });
    }
    let roots = xml(text)?;
    let Some(root) = roots.iter().find(|r| {
        ["movie", "tvshow", "episodedetails", "musicvideo"]
            .iter()
            .any(|n| r.name.eq_ignore_ascii_case(n))
    }) else {
        return Err("not a Kodi NFO (no <movie>, <tvshow> or <episodedetails>)".into());
    };
    let kind = match root.name.to_ascii_lowercase().as_str() {
        "tvshow" => "show",
        "episodedetails" => "episode",
        _ => "movie",
    };
    let mut f = Values::new();
    let mut put = |k: &str, v: Option<String>| {
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            f.insert(k.to_string(), Value::String(v));
        }
    };
    put("title", root.text_of("title"));
    put("original_title", root.text_of("originaltitle"));
    put(
        "overview",
        root.first("plot")
            .or_else(|| root.first("outline"))
            .map(|e| e.text.trim().to_string()),
    );
    put("tagline", root.text_of("tagline"));
    let mpaa = root.text_of("mpaa").map(|m| {
        let m = m.strip_prefix("Rated ").unwrap_or(&m).to_string();
        match m.split_once(':') {
            Some((country, r)) if country.len() <= 3 => r.trim().to_string(),
            _ => m,
        }
    });
    put("content_rating", mpaa.filter(|m| m.chars().count() <= 20));
    let date = |k: &str| {
        root.text_of(k)
            .filter(|d| d.len() >= 4 && d.chars().all(|c| c.is_ascii_digit() || c == '-'))
    };
    if kind == "episode" {
        put("air_date", date("aired").or_else(|| date("premiered")));
    } else {
        put("release_date", date("premiered").or_else(|| date("releasedate")));
    }
    let year = root
        .text_of("year")
        .and_then(|y| y.parse::<u16>().ok())
        .or_else(|| date("premiered").and_then(|d| d.get(..4)?.parse().ok()));
    if let Some(y) = year.filter(|y| (1870..=2100).contains(y)) {
        f.insert("year".into(), Value::from(y));
    }
    let list = |name: &str, split: bool| -> Vec<Value> {
        let mut out: Vec<String> = Vec::new();
        for e in root.all(name) {
            let t = tidy(&e.text);
            let parts: Vec<String> = if split {
                t.split(" / ").map(str::to_string).collect()
            } else {
                vec![t]
            };
            for p in parts {
                let p = p.trim().to_string();
                if !p.is_empty() && !out.contains(&p) {
                    out.push(p);
                }
            }
        }
        out.into_iter().map(Value::String).collect()
    };
    for (k, name, split) in [
        ("genres", "genre", true),
        ("tags", "tag", false),
        ("directors", "director", false),
        ("writers", "credits", false),
        ("studios", "studio", true),
    ] {
        let mut v = list(name, split);
        if k == "writers" && v.is_empty() {
            v = list("writer", false);
        }
        if !v.is_empty() {
            f.insert(k.into(), Value::Array(v));
        }
    }
    let countries: Vec<Value> = list("country", true)
        .into_iter()
        .filter_map(|c| c.as_str().and_then(country_code))
        .map(|c| Value::String(c.into()))
        .collect();
    if !countries.is_empty() {
        f.insert("countries".into(), Value::Array(countries));
    }
    let mut cast: Vec<(i64, Value)> = Vec::new();
    for (i, a) in root.all("actor").enumerate() {
        let Some(name) = a.text_of("name") else { continue };
        let order = a
            .text_of("order")
            .and_then(|o| o.parse().ok())
            .unwrap_or(i as i64 + 1000);
        cast.push((order, json!({ "name": name, "role": a.text_of("role") })));
    }
    cast.sort_by_key(|(o, _)| *o);
    if !cast.is_empty() {
        f.insert(
            "cast".into(),
            Value::Array(cast.into_iter().take(100).map(|(_, v)| v).collect()),
        );
    }
    for u in root.all("uniqueid") {
        let id = tidy(&u.text);
        let kind = u.attr("type").unwrap_or("").to_ascii_lowercase();
        if id.is_empty() || kind.is_empty() {
            continue;
        }
        let kind = if kind == "themoviedb" {
            "tmdb".to_string()
        } else {
            kind
        };
        f.insert(format!("ids.{kind}"), Value::String(id));
    }
    for (tag, kind) in [("imdbid", "imdb"), ("tmdbid", "tmdb"), ("tvdbid", "tvdb")] {
        if let Some(id) = root.text_of(tag) {
            f.entry(format!("ids.{kind}")).or_insert(Value::String(id));
        }
    }
    if let Some(id) = root.text_of("id") {
        if id.starts_with("tt") {
            f.entry("ids.imdb".to_string()).or_insert(Value::String(id));
        }
    }
    Ok(Content {
        kind: Some(kind.into()),
        fields: f,
        ..Content::default()
    })
}

// ---- what the scan records ----------------------------------------------------------------

/// A metadata file as the scan saw it, with what flux read from it.
#[derive(Debug, Clone, PartialEq)]
pub struct MetaFileRow {
    /// Root-relative, `/`-separated.
    pub rel_path: String,
    pub size: u64,
    pub mtime_ns: i64,
    /// [`FLUX`] or [`NFO`].
    pub format: String,
    /// What it says (`None`: it couldn't be read; `error` says why).
    pub content: Option<Content>,
    pub error: Option<String>,
}

/// The metadata files the catalog has under `prefixes` of a root (all of
/// them for an empty prefix): path → (size, mtime).
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
            conn.prepare_cached("SELECT rel_path, size, mtime_ns FROM meta_file WHERE root_id = ?1")?;
        add(&mut stmt.query([root_id])?)?;
    } else {
        let mut stmt = conn.prepare_cached(
            "SELECT rel_path, size, mtime_ns FROM meta_file
              WHERE root_id = ?1 AND rel_path >= ?2 || '/' AND rel_path < ?2 || '0'",
        )?;
        for p in prefixes {
            add(&mut stmt.query(params![root_id, p.trim_matches('/')])?)?;
        }
    }
    Ok(map)
}

/// The video files (catalog ids) a metadata file at `rel` describes.
fn described(c: &Connection, root_id: Id, rel: &str) -> Result<Vec<Id>> {
    let dir = dir_of(rel);
    let name = name_of(rel).to_ascii_lowercase();
    let mut out = Vec::new();
    let in_dir = |d: &str| -> Result<Vec<(Id, String)>> {
        let (lo, hi) = (
            if d.is_empty() {
                String::new()
            } else {
                format!("{d}/")
            },
            if d.is_empty() {
                "\u{10ffff}".to_string()
            } else {
                format!("{d}0")
            },
        );
        let mut stmt = c.prepare_cached(
            "SELECT id, rel_path FROM file WHERE root_id = ?1 AND rel_path >= ?2 AND rel_path < ?3 AND asset_id IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![root_id, lo, hi], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    };
    if name == "tvshow.nfo" || name == "flux.json" || name == "movie.nfo" {
        // A folder's: every video below it (a film's own folder, a show's).
        if dir.is_empty() {
            return Ok(out);
        }
        for (id, _) in in_dir(dir)? {
            out.push(id);
        }
        return Ok(out);
    }
    let stem = name
        .strip_suffix(".flux.json")
        .or_else(|| name.strip_suffix(".nfo"))
        .unwrap_or(&name)
        .to_string();
    for (id, path) in in_dir(dir)? {
        if dir_of(&path) == dir && stem_of(name_of(&path)).to_ascii_lowercase() == stem {
            out.push(id);
        }
    }
    Ok(out)
}

/// Write a scan's metadata files of a root (`upserts` new or changed,
/// `removed` proven gone) and apply what they say to the items they
/// describe. Returns what changed in the library.
pub fn sync(
    c: &Connection,
    root_id: Id,
    upserts: &[MetaFileRow],
    removed: &[String],
    now: i64,
    jobs: u32,
) -> Result<crate::model::ApplyOutcome> {
    let mut files: BTreeSet<Id> = BTreeSet::new();
    for f in upserts {
        c.prepare_cached(
            "INSERT INTO meta_file(root_id, rel_path, dir, format, size, mtime_ns, content, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(root_id, rel_path) DO UPDATE SET format = excluded.format, size = excluded.size,
                mtime_ns = excluded.mtime_ns, content = excluded.content, error = excluded.error",
        )?
        .execute(params![
            root_id,
            f.rel_path,
            dir_of(&f.rel_path),
            f.format,
            crate::model::clamp_i64(f.size),
            f.mtime_ns,
            f.content.as_ref().map(serde_json::to_string).transpose()?,
            f.error,
        ])?;
        files.extend(described(c, root_id, &f.rel_path)?);
    }
    for rel in removed {
        files.extend(described(c, root_id, rel)?);
        c.prepare_cached("DELETE FROM meta_file WHERE root_id = ?1 AND rel_path = ?2")?
            .execute(params![root_id, rel])?;
    }
    apply_files(c, &files.into_iter().collect::<Vec<_>>(), now, jobs)
}

/// What the metadata files of `files` (videos) say, applied: their fixes
/// first (the files are placed again), then the facts, to the items they
/// are in.
pub(crate) fn apply_files(
    c: &Connection,
    files: &[Id],
    now: i64,
    jobs: u32,
) -> Result<crate::model::ApplyOutcome> {
    let mut out = crate::model::ApplyOutcome::default();
    // Fixes a `.flux.json` gives its video.
    let mut refix: Vec<(Id, Option<FileFix>)> = Vec::new();
    for &file in files {
        let wanted = video_fix(c, file)?;
        let current = crate::fixes::get(c, file)?;
        match (&current, &wanted) {
            // A fix a person or an agent set stays.
            (Some((_, meta)), _) if meta.source != fields::SIDECAR => {}
            (Some((now_fix, _)), Some(w)) if now_fix == w => {}
            (None, None) => {}
            (Some((_, _)), None) | (None, Some(_)) | (Some(_), Some(_)) => {
                let meta = crate::fixes::FixMeta {
                    source: fields::SIDECAR.into(),
                    agent: None,
                    actor: None,
                    changeset: None,
                    at: now,
                };
                crate::fixes::put(c, file, wanted.as_ref(), &meta)?;
                refix.push((file, current.map(|(f, _)| f)));
            }
        }
    }
    if !refix.is_empty() {
        for (_, o) in crate::scan_support::refix(c, &refix, jobs)? {
            out.merge(o);
        }
    }
    // The facts, on the items the files are in now (and their film or show).
    let mut items: BTreeSet<Id> = BTreeSet::new();
    for &file in files {
        let mut stmt = c.prepare_cached(
            "SELECT ai.item_id FROM file f JOIN asset_item ai ON ai.asset_id = f.asset_id WHERE f.id = ?1",
        )?;
        let leaves: Vec<Id> = stmt
            .query_map([file], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for leaf in leaves {
            items.insert(leaf);
            let mut top = leaf;
            for _ in 0..3 {
                match c
                    .prepare_cached("SELECT parent_id FROM item WHERE id = ?1")?
                    .query_row([top], |r| r.get::<_, Option<Id>>(0))
                    .optional()?
                    .flatten()
                {
                    Some(p) => top = p,
                    None => break,
                }
            }
            items.insert(top);
        }
    }
    for item in items {
        if apply_item(c, item, now, jobs)? && !out.items_updated.contains(&item) {
            out.items_updated.push(item);
        }
    }
    Ok(out)
}

/// The fix the video's own `.flux.json` gives it.
fn video_fix(c: &Connection, file: Id) -> Result<Option<FileFix>> {
    let Some((root, rel)): Option<(Id, String)> = c
        .prepare_cached("SELECT root_id, rel_path FROM file WHERE id = ?1")?
        .query_row([file], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?
    else {
        return Ok(None);
    };
    for (path, format, scope) in for_video(c, root, &rel, None)? {
        if scope == Scope::Video && format == FLUX {
            if let Some(content) = content_of(c, root, &path)? {
                return Ok(content.fix);
            }
        }
    }
    Ok(None)
}

fn content_of(c: &Connection, root: Id, rel: &str) -> Result<Option<Content>> {
    let json: Option<String> = c
        .prepare_cached("SELECT content FROM meta_file WHERE root_id = ?1 AND rel_path = ?2")?
        .query_row(params![root, rel], |r| r.get(0))
        .optional()?
        .flatten();
    Ok(json.and_then(|j| serde_json::from_str(&j).ok()))
}

/// Whether a root has any metadata files (most libraries have none: then
/// nothing is looked up for new items).
pub(crate) fn root_has_any(c: &Connection, root: Id) -> Result<bool> {
    Ok(
        c.prepare_cached("SELECT EXISTS (SELECT 1 FROM meta_file WHERE root_id = ?1)")?
            .query_row([root], |r| r.get(0))?,
    )
}

/// A fact a metadata file gives: `(field, value, kind of file, its sources,
/// who wrote it)`.
type Fact = (String, Value, String, Vec<Link>, Option<String>);

/// Facts for `item` from the metadata files that describe it, each with the
/// file's kind, its sources and who wrote it; the most specific first.
fn facts_for(c: &Connection, item: Id) -> Result<Vec<Fact>> {
    let head: Option<(String, Option<i64>, Option<i64>)> = c
        .prepare_cached(
            "SELECT i.kind, (SELECT season_number FROM item WHERE id = i.parent_id), i.episode_number
               FROM item i WHERE i.id = ?1",
        )?
        .query_row([item], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    let Some((kind, season, episode)) = head else {
        return Ok(Vec::new());
    };
    // The files to look beside: the item's own; a show's, its episodes'.
    let sql = if kind == "show" {
        "SELECT f.root_id, f.rel_path FROM item se JOIN item ep ON ep.parent_id = se.id
           JOIN asset_item ai ON ai.item_id = ep.id JOIN asset a ON a.id = ai.asset_id JOIN file f ON f.id = a.file_id
          WHERE se.parent_id = ?1 LIMIT 50"
    } else {
        "SELECT f.root_id, f.rel_path FROM asset_item ai JOIN asset a ON a.id = ai.asset_id
           JOIN file f ON f.id = a.file_id WHERE ai.item_id = ?1"
    };
    let files: Vec<(Id, String)> = {
        let mut stmt = c.prepare_cached(sql)?;
        let rows = stmt.query_map([item], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut metas: Vec<(Id, String, String, Scope)> = Vec::new();
    for (root, rel) in &files {
        let show = matches!(kind.as_str(), "show" | "episode")
            .then(|| show_dir(rel))
            .flatten();
        for (path, format, scope) in for_video(c, *root, rel, show.as_deref())? {
            let fits = match kind.as_str() {
                "show" => scope == Scope::Show,
                "episode" => scope == Scope::Video || scope == Scope::Show,
                _ => scope == Scope::Video || scope == Scope::Folder,
            };
            if fits && !metas.iter().any(|(r, p, _, _)| r == root && *p == path) {
                metas.push((*root, path, format, scope));
            }
        }
    }
    metas.sort_by_key(|(_, _, f, s)| (*s, f.as_str() != FLUX));
    let mut out: Vec<Fact> = Vec::new();
    for (root, path, format, scope) in metas {
        let Some(content) = content_of(c, root, &path)? else {
            continue;
        };
        // An NFO says what it describes: a film's, a show's or an episode's.
        if let Some(k) = &content.kind {
            let fits = match kind.as_str() {
                "show" => k == "show",
                "episode" => k == "episode",
                _ => k == "movie",
            };
            if !fits {
                continue;
            }
        }
        let values: Values = match (kind.as_str(), scope) {
            // A show's flux.json describes its episodes too.
            ("episode", Scope::Show) => content
                .episodes
                .iter()
                .find(|(s, e, _)| Some(i64::from(*s)) == season && Some(i64::from(*e)) == episode)
                .map(|(_, _, v)| v.clone())
                .unwrap_or_default(),
            _ => content.fields.clone(),
        };
        for (field, value) in values {
            if out.iter().any(|(f, ..)| *f == field) {
                continue;
            }
            out.push((
                field,
                value,
                format.clone(),
                content.sources.clone(),
                content.by.clone(),
            ));
        }
    }
    Ok(out)
}

/// Apply what `item`'s metadata files say: each fact where nothing a
/// person or an agent set counts more ([`fields::rank`]); a fact a file
/// gave earlier and no longer gives is handed back to flux (its value
/// stays). Returns whether anything changed.
pub(crate) fn apply_item(c: &Connection, item: Id, now: i64, jobs: u32) -> Result<bool> {
    let Some(cur) = fields::read(c, item)? else {
        return Ok(false);
    };
    let facts = facts_for(c, item)?;
    let mut changed = false;
    let mut given: BTreeSet<String> = BTreeSet::new();
    let mut retitled = false;
    for (field, value, format, sources, by) in facts {
        let Ok(value) = fields::check(&field, &cur.kind, &value) else {
            continue;
        };
        given.insert(field.clone());
        let src = fields::source_of(c, item, &field)?;
        if src
            .as_ref()
            .is_some_and(|s| fields::rank(&s.source) > fields::rank(fields::SIDECAR))
        {
            continue;
        }
        let same_value = cur.values.get(&field) == Some(&value);
        let same_source = src
            .as_ref()
            .is_some_and(|s| s.source == fields::SIDECAR && s.provider.as_deref() == Some(format.as_str()));
        if same_value && same_source {
            continue;
        }
        if !same_value {
            fields::write(c, item, &field, Some(&value), now)?;
            if field == "title" || field == "year" {
                retitled = true;
            }
        }
        fields::put_source(
            c,
            item,
            &field,
            Some(&FieldRow {
                source: fields::SIDECAR.into(),
                provider: Some(format.clone()),
                agent: by.clone(),
                actor: None,
                changeset: None,
                sources: sources.clone(),
                at: now,
            }),
        )?;
        changed = true;
    }
    // What files gave before and give no longer: flux's again.
    for (field, row) in fields::sources(c, item)? {
        if row.source == fields::SIDECAR && !given.contains(&field) {
            fields::put_source(c, item, &field, None)?;
            changed = true;
        }
    }
    if retitled && jobs > 0 {
        let top: Option<(Option<Id>, String)> = c
            .prepare_cached("SELECT parent_id, kind FROM item WHERE id = ?1")?
            .query_row([item], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        if let Some((None, k)) = top {
            if k == "movie" || k == "show" {
                crate::jobs::rearm_art(c, item, now, jobs)?;
            }
        }
    }
    Ok(changed)
}

/// Apply metadata files to items a scan just made (a video found after its
/// `.flux.json` or `.nfo` was).
pub(crate) fn apply_new_items(
    c: &Connection,
    root: Id,
    items: &[Id],
    now: i64,
    jobs: u32,
) -> Result<Vec<Id>> {
    if items.is_empty() || !root_has_any(c, root)? {
        return Ok(Vec::new());
    }
    let mut changed = Vec::new();
    for &i in items {
        if apply_item(c, i, now, jobs)? {
            changed.push(i);
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_scopes_come_from_names() {
        assert_eq!(format_of("Film.flux.json"), Some(FLUX));
        assert_eq!(format_of("flux.json"), Some(FLUX));
        assert_eq!(format_of("Film.NFO"), Some(NFO));
        assert_eq!(format_of(".nfo"), None);
        assert_eq!(format_of("Film.json"), None);
        assert_eq!(scope_beside("Film (2010).nfo", "Film (2010)"), Some(Scope::Video));
        assert_eq!(
            scope_beside("film (2010).FLUX.json", "Film (2010)"),
            Some(Scope::Video)
        );
        assert_eq!(scope_beside("movie.nfo", "Film (2010)"), Some(Scope::Folder));
        assert_eq!(scope_beside("Other.nfo", "Film (2010)"), None);
        assert_eq!(
            show_dir("Show (2023)/Season 01/Show S01E01.mkv").as_deref(),
            Some("Show (2023)")
        );
        assert_eq!(show_dir("Show.S01E01.mkv"), None);
    }

    #[test]
    fn a_kodi_movie_nfo_reads_as_facts() {
        let nfo = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes" ?>
<!-- created on 2024-01-01 - tinyMediaManager 4.3 -->
<movie>
  <title>Spirited Away</title>
  <originaltitle>千と千尋の神隠し</originaltitle>
  <year>2001</year>
  <plot><![CDATA[A girl & a <spirit> world.]]></plot>
  <tagline>Nothing that happens is ever forgotten.</tagline>
  <mpaa>US:PG</mpaa>
  <genre>Animation / Fantasy</genre>
  <genre>Family</genre>
  <country>Japan</country>
  <director>Hayao Miyazaki</director>
  <credits>Hayao Miyazaki</credits>
  <studio>Studio Ghibli</studio>
  <premiered>2001-07-20</premiered>
  <uniqueid type="imdb" default="true">tt0245429</uniqueid>
  <uniqueid type="tmdb">129</uniqueid>
  <actor><name>Miyu Irino</name><role>Haku</role><order>1</order></actor>
  <actor><name>Rumi Hiiragi</name><role>Chihiro &amp; Sen</role><order>0</order></actor>
  <thumb aspect="poster">https://image.example/p.jpg</thumb>
</movie>"#;
        let c = parse(NFO, nfo.as_bytes()).unwrap();
        assert_eq!(c.kind.as_deref(), Some("movie"));
        let f = &c.fields;
        assert_eq!(f["title"], "Spirited Away");
        assert_eq!(f["original_title"], "千と千尋の神隠し");
        assert_eq!(f["year"], 2001);
        assert_eq!(f["overview"], "A girl & a <spirit> world.");
        assert_eq!(f["content_rating"], "PG");
        assert_eq!(f["genres"], json!(["Animation", "Fantasy", "Family"]));
        assert_eq!(f["countries"], json!(["JP"]));
        assert_eq!(f["release_date"], "2001-07-20");
        assert_eq!(f["ids.imdb"], "tt0245429");
        assert_eq!(f["ids.tmdb"], "129");
        assert_eq!(f["cast"][0]["name"], "Rumi Hiiragi", "billing order");
        assert_eq!(f["cast"][0]["role"], "Chihiro & Sen");
    }

    #[test]
    fn other_nfos_and_flux_sidecars() {
        let ep = parse(NFO, b"<episodedetails><title>The Signal</title><aired>2023-10-19</aired><season>1</season><episode>1</episode></episodedetails>").unwrap();
        assert_eq!(ep.kind.as_deref(), Some("episode"));
        assert_eq!(ep.fields["air_date"], "2023-10-19");
        let show = parse(
            NFO,
            b"<tvshow><title>Scavengers Reign</title><year>2023</year></tvshow>",
        )
        .unwrap();
        assert_eq!(show.kind.as_deref(), Some("show"));
        let url = parse(NFO, b"https://www.imdb.com/title/tt0816692/\n").unwrap();
        assert_eq!(url.fields["ids.imdb"], "tt0816692");
        assert!(parse(NFO, b"<html><body>no</body></html>").is_err());
        assert!(parse(NFO, b"just words").is_err());

        let flux = br#"{"flux": "flux.sidecar/1", "fields": {"title": "Interstellar", "year": 2014},
            "fix": {"title": "Interstellar", "year": 2014}, "sources": ["https://en.wikipedia.org/wiki/Interstellar_(film)"],
            "by": "claude-code"}"#;
        let c = parse(FLUX, flux).unwrap();
        assert_eq!(c.fields["title"], "Interstellar");
        assert_eq!(c.fix.as_ref().unwrap().year, Some(2014));
        assert_eq!(c.by.as_deref(), Some("claude-code"));
        assert!(parse(FLUX, br#"{"flux": "flux.sidecar/9"}"#)
            .unwrap_err()
            .contains("flux.sidecar/1"));
        assert!(parse(FLUX, br#"{"flux": "flux.sidecar/1", "feilds": {}}"#).is_err());
    }
}
