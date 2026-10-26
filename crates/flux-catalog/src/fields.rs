//! The facts of an item that people and agents set
//! ([`flux_core::agent::Fields`]): where each is stored, where each value
//! came from, and the locks that keep flux's own processes off what someone
//! chose (docs/AGENT-NATIVE.md, "Where values come from").
//!
//! - **Storage.** Most fields are `item` columns (`title`, `year`, …). Ids
//!   are `tmdb_id`, `tvdb_id`, `imdb_id` and the `external_ids` object; the
//!   rest (`tagline`, `cast`, `links`, …) live in the `details` object.
//! - **Sources.** `item_field` has a row for each field a provider, a person
//!   or an agent set. A field without one came from the file (or the scan).
//! - **Locks.** A `user`, `agent` or `sidecar` row locks its field: the
//!   matcher ([`crate::Writer::update_item_metadata`]) and the scanner's
//!   re-parse leave it alone, and a merge carries it to the item that stays
//!   (`carry`). A person's value outranks an agent's, which outranks a
//!   metadata file's ([`rank`]).

use std::collections::{BTreeMap, BTreeSet};

use flux_core::agent::{FieldSource, Fields, Link, Source};
use flux_core::api::Id;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{CatalogError, Result};

/// `item_field.source` of a provider match.
pub const PROVIDER: &str = "provider";
/// `item_field.source` of a person.
pub const USER: &str = "user";
/// `item_field.source` of an agent.
pub const AGENT: &str = "agent";
/// `item_field.source` of a metadata file beside the video (`provider`:
/// `flux` for a `.flux.json`, `nfo` for an `.nfo`).
pub const SIDECAR: &str = "sidecar";

/// How much a source's value counts: a person's (3) over an agent's (2)
/// over a metadata file's (1) over a provider's (0).
pub fn rank(source: &str) -> u8 {
    match source {
        USER => 3,
        AGENT => 2,
        SIDECAR => 1,
        _ => 0,
    }
}

/// The keys of `item.details`.
pub const DETAILS: [&str; 11] = [
    "tagline",
    "tags",
    "release_date",
    "languages",
    "countries",
    "directors",
    "writers",
    "cast",
    "studios",
    "content_rating",
    "links",
];

/// Most entries a list field keeps.
pub const MAX_LIST: usize = 100;
/// Most characters of a description.
pub const MAX_TEXT: usize = 16_000;
/// Most characters of a title, name or other short text.
pub const MAX_SHORT: usize = 300;
/// Most links of an item.
pub const MAX_LINKS: usize = 50;

/// Where a field is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Slot {
    /// A TEXT column.
    Text(&'static str),
    /// The `year` column.
    Year,
    /// `genres`: a JSON array in a TEXT column.
    Genres,
    /// A key of `details`.
    Detail(&'static str),
    /// `tmdb_id` or `tvdb_id`.
    IdNumber(&'static str),
    /// `imdb_id`.
    Imdb,
    /// A key of `external_ids`.
    IdOther(String),
}

/// Where `field` is stored, or `None` if no such field exists.
pub(crate) fn slot(field: &str) -> Option<Slot> {
    Some(match field {
        "title" => Slot::Text("title"),
        "original_title" => Slot::Text("original_title"),
        "overview" => Slot::Text("overview"),
        "air_date" => Slot::Text("air_date"),
        "year" => Slot::Year,
        "genres" => Slot::Genres,
        f => {
            if let Some(d) = DETAILS.iter().find(|d| **d == f) {
                return Some(Slot::Detail(d));
            }
            let key = f.strip_prefix("ids.")?;
            if !id_key_ok(key) {
                return None;
            }
            match key {
                "tmdb" => Slot::IdNumber("tmdb_id"),
                "tvdb" => Slot::IdNumber("tvdb_id"),
                "imdb" => Slot::Imdb,
                other => Slot::IdOther(other.to_string()),
            }
        }
    })
}

/// Whether `field` names a field ([`flux_core::agent::Fields`] key or
/// `ids.<provider>`).
pub fn is_field(field: &str) -> bool {
    slot(field).is_some()
}

fn id_key_ok(k: &str) -> bool {
    (2..=32).contains(&k.len())
        && k.starts_with(|c: char| c.is_ascii_lowercase())
        && k.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

// ---- values ------------------------------------------------------------------

/// An item's settable facts as flat field names (`ids.tmdb`, …) → JSON
/// values; empty fields are absent.
pub type Values = BTreeMap<String, Value>;

/// What [`read`] returns.
#[derive(Debug, Clone, PartialEq)]
pub struct Current {
    pub kind: String,
    pub parent_id: Option<Id>,
    pub library_kind: String,
    pub match_state: String,
    pub values: Values,
}

/// An item's facts now, or `None` if there is no such item.
pub fn read(c: &Connection, item: Id) -> Result<Option<Current>> {
    type Row = (
        String,
        Option<Id>,
        String,
        String,
        String,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let row: Option<Row> = c
        .prepare_cached(
            "SELECT i.kind, i.parent_id, l.kind, i.match_state, i.title, i.original_title,
                    i.year, i.overview, i.genres, i.air_date, i.tmdb_id, i.tvdb_id, i.imdb_id,
                    i.external_ids, i.details
               FROM item i JOIN library l ON l.id = i.library_id WHERE i.id = ?1",
        )?
        .query_row([item], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
                r.get(12)?,
                r.get(13)?,
                r.get(14)?,
            ))
        })
        .optional()?;
    let Some((
        kind,
        parent_id,
        library_kind,
        match_state,
        title,
        original,
        year,
        overview,
        genres,
        air,
        tmdb,
        tvdb,
        imdb,
        others,
        details,
    )) = row
    else {
        return Ok(None);
    };
    let mut v = Values::new();
    let mut text = |k: &str, s: Option<String>| {
        if let Some(s) = s.filter(|s| !s.is_empty()) {
            v.insert(k.to_string(), Value::String(s));
        }
    };
    text("title", Some(title));
    text("original_title", original);
    text("overview", overview);
    text("air_date", air);
    text("ids.imdb", imdb);
    if let Some(y) = year {
        v.insert("year".into(), Value::from(y));
    }
    if let Some(g) = genres.and_then(|g| serde_json::from_str::<Value>(&g).ok()) {
        if g.as_array().is_some_and(|a| !a.is_empty()) {
            v.insert("genres".into(), g);
        }
    }
    for (k, n) in [("ids.tmdb", tmdb), ("ids.tvdb", tvdb)] {
        if let Some(n) = n {
            v.insert(k.into(), Value::String(n.to_string()));
        }
    }
    for (k, val) in object(others.as_deref()) {
        if let Some(s) = val.as_str().filter(|s| !s.is_empty()) {
            if id_key_ok(&k) {
                v.insert(format!("ids.{k}"), Value::String(s.to_string()));
            }
        }
    }
    for (k, val) in object(details.as_deref()) {
        if DETAILS.contains(&k.as_str()) && !val.is_null() {
            v.insert(k, val);
        }
    }
    Ok(Some(Current {
        kind,
        parent_id,
        library_kind,
        match_state,
        values: v,
    }))
}

fn object(s: Option<&str>) -> Map<String, Value> {
    s.and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

/// [`Values`] as [`Fields`] (ids nested again).
pub fn to_fields(values: &Values) -> Fields {
    let mut obj = Map::new();
    let mut ids = Map::new();
    for (k, v) in values {
        match k.strip_prefix("ids.") {
            Some(id) => {
                ids.insert(id.to_string(), v.clone());
            }
            None => {
                obj.insert(k.clone(), v.clone());
            }
        }
    }
    if !ids.is_empty() {
        obj.insert("ids".into(), Value::Object(ids));
    }
    // Values were checked on the way in; anything unreadable (a value an
    // older build wrote differently) is dropped rather than failing a read.
    let mut fields = Fields::default();
    for (k, v) in obj {
        let one = Map::from_iter([(k, v)]);
        if let Ok(f) = serde_json::from_value::<Fields>(Value::Object(one)) {
            merge_fields(&mut fields, f);
        }
    }
    fields
}

fn merge_fields(into: &mut Fields, f: Fields) {
    macro_rules! take {
        ($($n:ident),*) => { $( if f.$n.is_some() { into.$n = f.$n; } )* };
    }
    take!(
        title,
        original_title,
        year,
        overview,
        tagline,
        genres,
        tags,
        release_date,
        air_date,
        languages,
        countries,
        directors,
        writers,
        cast,
        studios,
        content_rating,
        links,
        ids
    );
}

/// [`Fields`] as flat [`Values`] (what `items.edit` sets). Fails on a
/// field whose value isn't valid, naming it.
pub fn from_fields(f: &Fields) -> std::result::Result<Values, String> {
    let v = serde_json::to_value(f).map_err(|e| e.to_string())?;
    let mut out = Values::new();
    let Value::Object(obj) = v else { return Ok(out) };
    for (k, v) in obj {
        if v.is_null() {
            continue;
        }
        if k == "ids" {
            if let Value::Object(ids) = v {
                for (id, val) in ids {
                    out.insert(format!("ids.{id}"), val);
                }
            }
        } else {
            out.insert(k, v);
        }
    }
    Ok(out)
}

// ---- checking -------------------------------------------------------------------

/// `value` for `field` of an item of `kind`, checked and tidied (trimmed,
/// de-duplicated, codes in their usual case), or why it can't be set.
pub fn check(field: &str, kind: &str, value: &Value) -> std::result::Result<Value, String> {
    let Some(slot) = slot(field) else {
        return Err(format!("{field:?} is not a field you can set"));
    };
    let s = |max: usize, multiline: bool| -> std::result::Result<Value, String> {
        let Some(t) = value.as_str() else {
            return Err(format!("{field} takes text"));
        };
        let t = tidy(t, multiline);
        if t.is_empty() {
            return Err(format!("{field} is empty: to empty a field, name it in `clear`"));
        }
        if t.chars().count() > max {
            return Err(format!("{field} is longer than {max} characters"));
        }
        Ok(Value::String(t))
    };
    match slot {
        Slot::Text("overview") => s(MAX_TEXT, true),
        Slot::Text("air_date") => {
            if kind != "episode" {
                return Err("air_date is for episodes (use release_date)".into());
            }
            let v = s(10, false)?;
            date(v.as_str().unwrap_or_default(), true)
                .map(Value::String)
                .ok_or_else(|| "air_date must be YYYY-MM-DD".into())
        }
        Slot::Text(_) => s(MAX_SHORT, false),
        Slot::Year => {
            let y = match value {
                Value::Number(n) => n.as_u64(),
                Value::String(t) => t.trim().parse().ok(),
                _ => None,
            };
            match y {
                Some(y) if (1870..=2100).contains(&y) => Ok(Value::from(y)),
                _ => Err("year must be a year between 1870 and 2100".into()),
            }
        }
        Slot::Genres => list(field, value, 60),
        Slot::Detail(d) => match d {
            "tagline" => s(MAX_SHORT, false),
            "content_rating" => s(20, false),
            "release_date" => {
                let v = s(10, false)?;
                date(v.as_str().unwrap_or_default(), false)
                    .map(Value::String)
                    .ok_or_else(|| "release_date must be YYYY-MM-DD, YYYY-MM or YYYY".into())
            }
            "languages" => codes(field, value, |c| {
                let mut parts = c.split('-');
                let lang = parts.next().unwrap_or_default();
                ((2..=3).contains(&lang.len()) && lang.bytes().all(|b| b.is_ascii_alphabetic()))
                    .then(|| {
                        let mut out = lang.to_ascii_lowercase();
                        for p in parts {
                            if p.is_empty() || p.len() > 8 || !p.bytes().all(|b| b.is_ascii_alphanumeric()) {
                                return None;
                            }
                            out.push('-');
                            out.push_str(p);
                        }
                        Some(out)
                    })
                    .flatten()
            }),
            "countries" => codes(field, value, |c| {
                (c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphabetic())).then(|| c.to_ascii_uppercase())
            }),
            "cast" => cast(value),
            "links" => links(field, value, MAX_LINKS).map(|l| serde_json::to_value(l).unwrap_or(Value::Null)),
            _ => list(field, value, 200),
        },
        Slot::IdNumber(_) => {
            let n = match value {
                Value::Number(n) => n.as_u64(),
                Value::String(t) => t.trim().parse().ok(),
                _ => None,
            };
            match n {
                Some(n) if n > 0 && n < i64::MAX as u64 => Ok(Value::String(n.to_string())),
                _ => Err(format!(
                    "{field} takes a number (the id in the provider's address)"
                )),
            }
        }
        Slot::Imdb => {
            let t = value.as_str().map(str::trim).unwrap_or_default();
            let ok = t.len() <= 12
                && t.strip_prefix("tt")
                    .is_some_and(|d| d.len() >= 5 && d.bytes().all(|b| b.is_ascii_digit()));
            if ok {
                Ok(Value::String(t.to_string()))
            } else {
                Err("ids.imdb takes an IMDb title id such as tt0245429".into())
            }
        }
        Slot::IdOther(k) => {
            let t = value
                .as_str()
                .map(str::trim)
                .map(String::from)
                .or_else(|| value.as_u64().map(|n| n.to_string()))
                .unwrap_or_default();
            if t.is_empty() || t.len() > 100 || t.chars().any(|c| c.is_control() || c.is_whitespace()) {
                return Err(format!(
                    "ids.{k} takes the id as it appears in that site's addresses"
                ));
            }
            if k == "wikidata"
                && !(t.starts_with('Q') && t.len() > 1 && t[1..].bytes().all(|b| b.is_ascii_digit()))
            {
                return Err("ids.wikidata takes an item id such as Q155653".into());
            }
            Ok(Value::String(t))
        }
    }
}

/// Collapse runs of whitespace (keeping paragraph breaks when `multiline`)
/// and drop control characters.
fn tidy(s: &str, multiline: bool) -> String {
    if !multiline {
        return s
            .split(|c: char| c.is_whitespace() || c.is_control())
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
    }
    let paras: Vec<String> = s
        .replace("\r\n", "\n")
        .split("\n\n")
        .map(|p| tidy(p, false))
        .filter(|p| !p.is_empty())
        .collect();
    paras.join("\n\n")
}

/// `YYYY-MM-DD` (or with `!day_only` also `YYYY-MM`, `YYYY`), checked.
fn date(s: &str, day_only: bool) -> Option<String> {
    let parts: Vec<&str> = s.split('-').collect();
    let num = |p: &str, len: usize| {
        (p.len() == len && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse::<u32>().ok())
            .flatten()
    };
    let y = num(parts.first()?, 4).filter(|y| (1870..=2100).contains(y))?;
    match parts.len() {
        1 if !day_only => Some(format!("{y:04}")),
        2 if !day_only => {
            let m = num(parts[1], 2).filter(|m| (1..=12).contains(m))?;
            Some(format!("{y:04}-{m:02}"))
        }
        3 => {
            let m = num(parts[1], 2).filter(|m| (1..=12).contains(m))?;
            let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
            let days = [
                31,
                if leap { 29 } else { 28 },
                31,
                30,
                31,
                30,
                31,
                31,
                30,
                31,
                30,
                31,
            ][m as usize - 1];
            let d = num(parts[2], 2).filter(|d| (1..=days).contains(d))?;
            Some(format!("{y:04}-{m:02}-{d:02}"))
        }
        _ => None,
    }
}

fn strings(field: &str, value: &Value, max_len: usize) -> std::result::Result<Vec<String>, String> {
    let items: Vec<&Value> = match value {
        Value::Array(a) => a.iter().collect(),
        // A single name is a list of one.
        Value::String(_) => vec![value],
        _ => return Err(format!("{field} takes a list of names")),
    };
    let mut out: Vec<String> = Vec::new();
    let mut seen = BTreeSet::new();
    for v in items {
        let Some(t) = v.as_str() else {
            return Err(format!("{field} takes a list of names"));
        };
        let t = tidy(t, false);
        if t.is_empty() {
            continue;
        }
        if t.chars().count() > max_len {
            return Err(format!("an entry of {field} is longer than {max_len} characters"));
        }
        if seen.insert(t.to_lowercase()) {
            out.push(t);
        }
    }
    if out.is_empty() {
        return Err(format!("{field} is empty: to empty a field, name it in `clear`"));
    }
    if out.len() > MAX_LIST {
        return Err(format!("{field} has more than {MAX_LIST} entries"));
    }
    Ok(out)
}

fn list(field: &str, value: &Value, max_len: usize) -> std::result::Result<Value, String> {
    strings(field, value, max_len).map(|l| Value::Array(l.into_iter().map(Value::String).collect()))
}

fn codes(
    field: &str,
    value: &Value,
    norm: impl Fn(&str) -> Option<String>,
) -> std::result::Result<Value, String> {
    let mut out = Vec::new();
    for c in strings(field, value, 20)? {
        match norm(&c) {
            Some(n) if !out.contains(&n) => out.push(n),
            Some(_) => {}
            None => {
                return Err(if field == "countries" {
                    format!("{c:?} is not a two-letter country code (ISO 3166-1, such as JP or FR)")
                } else {
                    format!("{c:?} is not a language code (ISO 639-1 where one exists, such as ja or en)")
                })
            }
        }
    }
    Ok(Value::Array(out.into_iter().map(Value::String).collect()))
}

fn cast(value: &Value) -> std::result::Result<Value, String> {
    let Value::Array(a) = value else {
        return Err("cast takes a list of {\"name\", \"role\"}".into());
    };
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for v in a {
        let (name, role) = match v {
            Value::String(n) => (n.as_str(), None),
            Value::Object(o) => (
                o.get("name").and_then(Value::as_str).unwrap_or_default(),
                o.get("role").and_then(Value::as_str),
            ),
            _ => return Err("cast takes a list of {\"name\", \"role\"}".into()),
        };
        let name = tidy(name, false);
        if name.is_empty() {
            return Err("a cast member has no name".into());
        }
        let role = role.map(|r| tidy(r, false)).filter(|r| !r.is_empty());
        if name.chars().count() > 200 || role.as_ref().is_some_and(|r| r.chars().count() > 200) {
            return Err("a cast member's name or role is longer than 200 characters".into());
        }
        if seen.insert((name.to_lowercase(), role.as_ref().map(|r| r.to_lowercase()))) {
            out.push(serde_json::json!({ "name": name, "role": role }));
        }
    }
    if out.is_empty() {
        return Err("cast is empty: to empty a field, name it in `clear`".into());
    }
    if out.len() > MAX_LIST {
        return Err(format!("cast has more than {MAX_LIST} entries"));
    }
    Ok(Value::Array(out))
}

/// Links (or sources) checked: `http(s)` URLs, de-duplicated.
pub fn links(field: &str, value: &Value, max: usize) -> std::result::Result<Vec<Link>, String> {
    let Value::Array(a) = value else {
        return Err(format!("{field} takes a list of {{\"url\", \"title\"}}"));
    };
    let mut out: Vec<Link> = Vec::new();
    for v in a {
        let (url, title) = match v {
            Value::String(u) => (u.as_str(), None),
            Value::Object(o) => (
                o.get("url").and_then(Value::as_str).unwrap_or_default(),
                o.get("title").and_then(Value::as_str),
            ),
            _ => return Err(format!("{field} takes a list of {{\"url\", \"title\"}}")),
        };
        let link = link(url, title).ok_or_else(|| format!("{url:?} is not an http or https address"))?;
        if !out.iter().any(|l| l.url == link.url) {
            out.push(link);
        }
    }
    if out.len() > max {
        return Err(format!("{field} has more than {max} links"));
    }
    Ok(out)
}

/// One checked link.
pub fn link(url: &str, title: Option<&str>) -> Option<Link> {
    let url = url.trim();
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() || url.len() > 2000 || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let title = title
        .map(|t| tidy(t, false))
        .filter(|t| !t.is_empty())
        .map(|t| t.chars().take(200).collect());
    Some(Link {
        url: url.to_string(),
        title,
    })
}

// ---- writing -------------------------------------------------------------------

/// Write `value` (`None`: empty it) into `field` of `item`, keeping the
/// sort title (always the title's) and the search index in step. `title` can't be emptied: the
/// caller gives the file's title instead ([`file_title`]).
pub(crate) fn write(c: &Connection, item: Id, field: &str, value: Option<&Value>, now: i64) -> Result<()> {
    let slot = slot(field).ok_or_else(|| CatalogError::Invalid(format!("no field {field:?}")))?;
    let indexed = matches!(slot, Slot::Text("title" | "original_title" | "overview"));
    let old = if indexed { crate::fts::doc(c, item)? } else { None };
    let text = value.and_then(Value::as_str);
    match &slot {
        Slot::Text("title") => {
            let Some(t) = text else {
                return Err(CatalogError::Invalid("a title can't be empty".into()));
            };
            c.prepare_cached("UPDATE item SET title = ?2, sort_title = ?3, updated_at = ?4 WHERE id = ?1")?
                .execute(params![item, t, crate::group::sort_title(t), now])?;
        }
        Slot::Text(col) => {
            // The column names are this module's own.
            c.prepare_cached(&format!(
                "UPDATE item SET {col} = ?2, updated_at = ?3 WHERE id = ?1"
            ))?
            .execute(params![item, text, now])?;
        }
        Slot::Year => {
            c.prepare_cached("UPDATE item SET year = ?2, updated_at = ?3 WHERE id = ?1")?
                .execute(params![item, value.and_then(Value::as_i64), now])?;
        }
        Slot::Genres => {
            let g = value.map(Value::to_string);
            c.prepare_cached("UPDATE item SET genres = ?2, updated_at = ?3 WHERE id = ?1")?
                .execute(params![item, g, now])?;
        }
        Slot::IdNumber(col) => {
            let n = text.and_then(|t| t.parse::<i64>().ok());
            c.prepare_cached(&format!(
                "UPDATE item SET {col} = ?2, updated_at = ?3 WHERE id = ?1"
            ))?
            .execute(params![item, n, now])?;
        }
        Slot::Imdb => {
            c.prepare_cached("UPDATE item SET imdb_id = ?2, updated_at = ?3 WHERE id = ?1")?
                .execute(params![item, text, now])?;
        }
        Slot::IdOther(key) => edit_object(c, item, "external_ids", key, value, now)?,
        Slot::Detail(key) => edit_object(c, item, "details", key, value, now)?,
    }
    if let Some(old) = old {
        crate::fts::update(c, item, &old)?;
    }
    Ok(())
}

/// Set or remove `key` of the JSON object in column `col`.
fn edit_object(
    c: &Connection,
    item: Id,
    col: &str,
    key: &str,
    value: Option<&Value>,
    now: i64,
) -> Result<()> {
    let s: Option<String> = c
        .prepare_cached(&format!("SELECT {col} FROM item WHERE id = ?1"))?
        .query_row([item], |r| r.get(0))?;
    let mut obj = object(s.as_deref());
    match value {
        Some(v) => {
            obj.insert(key.to_string(), v.clone());
        }
        None => {
            obj.remove(key);
        }
    }
    let out = (!obj.is_empty()).then(|| Value::Object(obj).to_string());
    c.prepare_cached(&format!(
        "UPDATE item SET {col} = ?2, updated_at = ?3 WHERE id = ?1"
    ))?
    .execute(params![item, out, now])?;
    Ok(())
}

/// The title flux gives `item` by itself: what its first file's name says
/// (a season: "Season N"), or `None` when no file says.
pub fn file_title(c: &Connection, item: Id) -> Result<Option<String>> {
    let head: Option<(String, Option<i64>)> = c
        .prepare_cached("SELECT kind, season_number FROM item WHERE id = ?1")?
        .query_row([item], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let Some((kind, season)) = head else {
        return Ok(None);
    };
    if kind == "season" {
        return Ok(Some(match season {
            Some(0) => "Specials".to_string(),
            Some(n) => format!("Season {n}"),
            None => "Season".to_string(),
        }));
    }
    let guess = first_guess(c, item, &kind)?;
    Ok(guess.and_then(|g| match kind.as_str() {
        "episode" => g
            .episode_title
            .or_else(|| g.episodes.first().map(|e| format!("Episode {e}"))),
        _ => g.title,
    }))
}

/// The parse of `item`'s first file (a show: of its first episode's file).
pub(crate) fn first_guess(c: &Connection, item: Id, kind: &str) -> Result<Option<flux_core::NameGuess>> {
    let sql = if kind == "show" {
        "SELECT m.guess FROM item s JOIN item e ON e.parent_id = s.id
           JOIN asset_item ai ON ai.item_id = e.id JOIN asset a ON a.id = ai.asset_id
           JOIN file_meta m ON m.file_id = a.file_id
          WHERE s.parent_id = ?1 AND m.guess IS NOT NULL ORDER BY a.file_id LIMIT 1"
    } else {
        "SELECT m.guess FROM asset_item ai JOIN asset a ON a.id = ai.asset_id
           JOIN file_meta m ON m.file_id = a.file_id
          WHERE ai.item_id = ?1 AND m.guess IS NOT NULL ORDER BY a.file_id LIMIT 1"
    };
    let s: Option<String> = c
        .prepare_cached(sql)?
        .query_row([item], |r| r.get(0))
        .optional()?;
    Ok(s.and_then(|s| serde_json::from_str(&s).ok()))
}

// ---- sources ---------------------------------------------------------------------

/// One `item_field` row: who or what set a field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldRow {
    /// [`PROVIDER`], [`USER`], [`AGENT`] or [`SIDECAR`].
    pub source: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    /// The person (a login).
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub changeset: Option<Id>,
    #[serde(default)]
    pub sources: Vec<Link>,
    pub at: i64,
}

impl FieldRow {
    /// The provider (or metadata file kind) a reader that may not see
    /// `hide` is kept from: a provider's value, or a value read from a
    /// metadata file of that kind (an `.nfo` is usually a provider's copy).
    pub fn withheld_by(&self, hide: &[String]) -> Option<String> {
        (self.source == PROVIDER || self.source == SIDECAR)
            .then(|| self.provider.clone())
            .flatten()
            .filter(|p| hide.contains(p))
    }

    /// A person, an agent or a metadata file set it: the matcher and the
    /// scanner's re-parse leave it alone.
    pub fn locks(&self) -> bool {
        rank(&self.source) > 0
    }

    pub fn to_api(&self) -> FieldSource {
        FieldSource {
            source: match self.source.as_str() {
                PROVIDER => Source::Provider,
                USER => Source::User,
                SIDECAR => Source::Sidecar,
                _ => Source::Agent,
            },
            provider: self.provider.clone(),
            agent: self.agent.clone(),
            by: self.actor.clone(),
            changeset: self.changeset,
            at: Some(self.at),
            sources: self.sources.clone(),
        }
    }
}

fn row_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<FieldRow> {
    let sources: Option<String> = r.get(5)?;
    Ok(FieldRow {
        source: r.get(0)?,
        provider: r.get(1)?,
        agent: r.get(2)?,
        actor: r.get(3)?,
        changeset: r.get(4)?,
        sources: sources
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        at: r.get(6)?,
    })
}

/// Who set `field` of `item`; `None`: the file (or the scan).
pub fn source_of(c: &Connection, item: Id, field: &str) -> Result<Option<FieldRow>> {
    Ok(c.prepare_cached(
        "SELECT source, provider, agent, actor, changeset_id, sources, at FROM item_field
          WHERE item_id = ?1 AND field = ?2",
    )?
    .query_row(params![item, field], row_of)
    .optional()?)
}

/// Every `item_field` row of `item`, by field.
pub fn sources(c: &Connection, item: Id) -> Result<BTreeMap<String, FieldRow>> {
    let mut stmt = c.prepare_cached(
        "SELECT source, provider, agent, actor, changeset_id, sources, at, field FROM item_field WHERE item_id = ?1",
    )?;
    let rows = stmt.query_map([item], |r| Ok((r.get::<_, String>(7)?, row_of(r)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Record (or, with `None`, forget) who set `field` of `item`.
pub(crate) fn put_source(c: &Connection, item: Id, field: &str, row: Option<&FieldRow>) -> Result<()> {
    match row {
        Some(r) => {
            let sources = (!r.sources.is_empty())
                .then(|| serde_json::to_string(&r.sources))
                .transpose()?;
            c.prepare_cached(
                "INSERT INTO item_field(item_id, field, source, provider, agent, actor, changeset_id, sources, at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(item_id, field) DO UPDATE SET source = excluded.source, provider = excluded.provider,
                    agent = excluded.agent, actor = excluded.actor, changeset_id = excluded.changeset_id,
                    sources = excluded.sources, at = excluded.at",
            )?
            .execute(params![item, field, r.source, r.provider, r.agent, r.actor, r.changeset, sources, r.at])?;
        }
        None => {
            c.prepare_cached("DELETE FROM item_field WHERE item_id = ?1 AND field = ?2")?
                .execute(params![item, field])?;
        }
    }
    Ok(())
}

/// Whether a person, an agent or a metadata file set `field` of `item`.
pub fn is_locked(c: &Connection, item: Id, field: &str) -> Result<bool> {
    Ok(source_of(c, item, field)?.is_some_and(|r| r.locks()))
}

/// The fields of `item` a person, an agent or a metadata file set.
pub fn locked(c: &Connection, item: Id) -> Result<BTreeSet<String>> {
    let mut stmt = c.prepare_cached(
        "SELECT field FROM item_field WHERE item_id = ?1 AND source IN ('user', 'agent', 'sidecar')",
    )?;
    let rows = stmt.query_map([item], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// What a person, an agent or a metadata file set on `from` goes to
/// `into`, where nothing that counts as much set it on `into` ([`rank`]):
/// the work merged into another, or a work whose files moved. Changes
/// recorded against `from` now name `into`, so they can still be undone.
pub(crate) fn carry(c: &Connection, from: Id, into: Id, now: i64) -> Result<()> {
    if from == into {
        return Ok(());
    }
    let theirs = sources(c, from)?;
    if theirs.values().any(FieldRow::locks) {
        let Some(cur) = read(c, from)? else { return Ok(()) };
        for (field, row) in theirs.into_iter().filter(|(_, r)| r.locks()) {
            let take = match source_of(c, into, &field)? {
                None => true,
                Some(mine) => rank(&row.source) > rank(&mine.source),
            };
            if !take {
                continue;
            }
            match cur.values.get(&field) {
                Some(v) => write(c, into, &field, Some(v), now)?,
                // Nothing to write for an emptied title; others empty too.
                None if field == "title" => continue,
                None => write(c, into, &field, None, now)?,
            }
            put_source(c, into, &field, Some(&row))?;
        }
    }
    c.prepare_cached("UPDATE change SET item_id = ?2 WHERE item_id = ?1")?
        .execute(params![from, into])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fields_have_slots_and_ids_take_any_provider() {
        for f in Fields::NAMES {
            assert!(is_field(f), "{f}");
        }
        assert!(is_field("ids.tmdb") && is_field("ids.wikidata") && is_field("ids.anidb"));
        assert!(!is_field("ids.") && !is_field("ids.Bad") && !is_field("poster") && !is_field("ids"));
    }

    #[test]
    fn check_tidies_and_explains() {
        assert_eq!(
            check("title", "movie", &json!("  Spirited\n Away ")).unwrap(),
            json!("Spirited Away")
        );
        assert!(check("title", "movie", &json!("   "))
            .unwrap_err()
            .contains("clear"));
        assert_eq!(check("year", "movie", &json!("2001")).unwrap(), json!(2001));
        assert!(check("year", "movie", &json!(1500)).is_err());
        assert_eq!(
            check("genres", "movie", &json!(["Animation", "animation", " Fantasy "])).unwrap(),
            json!(["Animation", "Fantasy"])
        );
        assert_eq!(
            check("directors", "movie", &json!("Hayao Miyazaki")).unwrap(),
            json!(["Hayao Miyazaki"])
        );
        assert_eq!(
            check("countries", "movie", &json!(["jp"])).unwrap(),
            json!(["JP"])
        );
        assert!(check("countries", "movie", &json!(["Japan"]))
            .unwrap_err()
            .contains("two-letter"));
        assert_eq!(
            check("languages", "movie", &json!(["JA", "zh-Hant"])).unwrap(),
            json!(["ja", "zh-Hant"])
        );
        assert_eq!(
            check("release_date", "movie", &json!("2001-07-20")).unwrap(),
            json!("2001-07-20")
        );
        assert_eq!(
            check("release_date", "movie", &json!("2001")).unwrap(),
            json!("2001")
        );
        assert!(check("release_date", "movie", &json!("2001-02-30")).is_err());
        assert!(check("air_date", "movie", &json!("2001-07-20"))
            .unwrap_err()
            .contains("episodes"));
        assert!(check("air_date", "episode", &json!("2001-07")).is_err());
        assert_eq!(check("ids.tmdb", "movie", &json!(129)).unwrap(), json!("129"));
        assert_eq!(
            check("ids.imdb", "movie", &json!("tt0245429")).unwrap(),
            json!("tt0245429")
        );
        assert!(check("ids.imdb", "movie", &json!("0245429")).is_err());
        assert!(check("ids.wikidata", "movie", &json!("155653")).is_err());
        assert_eq!(check("ids.anidb", "movie", &json!(112)).unwrap(), json!("112"));
        assert_eq!(
            check(
                "cast",
                "movie",
                &json!([{"name": "Rumi Hiiragi", "role": "Chihiro"}, "Miyu Irino"])
            )
            .unwrap(),
            json!([{"name": "Rumi Hiiragi", "role": "Chihiro"}, {"name": "Miyu Irino", "role": null}])
        );
        assert_eq!(
            check(
                "links",
                "movie",
                &json!([
                    "https://www.ghibli.jp/works/chihiro/",
                    "https://www.ghibli.jp/works/chihiro/"
                ])
            )
            .unwrap(),
            json!([{"url": "https://www.ghibli.jp/works/chihiro/", "title": null}])
        );
        assert!(check("links", "movie", &json!(["javascript:alert(1)"])).is_err());
        assert_eq!(
            check("overview", "movie", &json!("One.\r\n\r\nTwo   three.\n\n\n")).unwrap(),
            json!("One.\n\nTwo three.")
        );
        assert!(check("poster", "movie", &json!("x")).is_err());
    }

    #[test]
    fn fields_round_trip_through_values() {
        let f = Fields {
            title: Some("Spirited Away".into()),
            year: Some(2001),
            ids: Some([("tmdb".to_string(), "129".to_string())].into()),
            cast: Some(vec![flux_core::agent::CastMember {
                name: "Rumi Hiiragi".into(),
                role: Some("Chihiro".into()),
            }]),
            ..Fields::default()
        };
        let v = from_fields(&f).unwrap();
        assert_eq!(v.get("ids.tmdb"), Some(&json!("129")));
        assert_eq!(v.get("year"), Some(&json!(2001)));
        assert_eq!(to_fields(&v), f);
    }
}
