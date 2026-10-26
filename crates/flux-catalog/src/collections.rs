//! Collections kept on the server (docs/AGENT-NATIVE.md, "Collections"):
//! titles picked by hand (`collection_item`, in order, each with a note)
//! and/or a rule ([`CollectionRule`]) that takes the titles matching it as
//! the library changes. Per person (`owner` = user id) or for everyone
//! (`owner` 0). The app shows them as spaces beside the libraries
//! ([`crate::read::items_page`] with a collection), agents read them with
//! [`info`] and [`members`].
//!
//! A rule never counts a value a reader may not see: an agent's view of a
//! rule on genres leaves out genres a provider withheld from it gave, so
//! which titles are in a collection can't tell it what TMDB says.

use flux_core::agent::{CollectionInfo, CollectionPick, CollectionRule, Link};
use flux_core::api::{Id, ItemKind};
use rusqlite::types::Value as Sql;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};

use crate::error::{CatalogError, Result};

/// `collection.owner` of a collection for everyone.
pub const EVERYONE: Id = 0;
/// Most collections one owner keeps.
pub const MAX_PER_OWNER: usize = 100;
/// Most titles one collection picks.
pub const MAX_PICKED: usize = 2000;
/// Most titles a collection lists (picked and by rule).
pub const MAX_LISTED: usize = 5000;

/// A collection as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: Id,
    pub owner: Id,
    pub name: String,
    pub description: Option<String>,
    pub rule: Option<CollectionRule>,
    pub added_by: String,
    pub agent: Option<String>,
    pub sources: Vec<Link>,
    pub created_at: i64,
    pub updated_at: i64,
}

const ROW_COLS: &str = "id, owner, name, description, rule, added_by, agent, sources, created_at, updated_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    let rule: Option<String> = r.get(4)?;
    let sources: Option<String> = r.get(7)?;
    Ok(Row {
        id: r.get(0)?,
        owner: r.get(1)?,
        name: r.get(2)?,
        description: r.get(3)?,
        rule: rule.and_then(|j| serde_json::from_str(&j).ok()),
        added_by: r.get(5)?,
        agent: r.get(6)?,
        sources: sources
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default(),
        created_at: r.get(8)?,
        updated_at: r.get(9)?,
    })
}

/// One collection.
pub fn get(c: &Connection, id: Id) -> Result<Option<Row>> {
    Ok(
        c.prepare_cached(&format!("SELECT {ROW_COLS} FROM collection WHERE id = ?1"))?
            .query_row([id], row)
            .optional()?,
    )
}

/// Whether `user` (`None`: anonymous) may see collection `r`: everyone's,
/// or their own.
pub fn visible(r: &Row, user: Option<Id>) -> bool {
    r.owner == EVERYONE || Some(r.owner) == user
}

fn lower_list(v: &[String]) -> String {
    serde_json::to_string(
        &v.iter()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".into())
}

/// A value of `field` counts for a reader that may not see `hide` unless a
/// hidden provider (or metadata file) gave it.
fn counts(field: &str, hidden: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM item_field hf WHERE hf.item_id = i.id AND hf.field = '{field}'
                      AND hf.source IN ('provider', 'sidecar') AND hf.provider IN (SELECT value FROM json_each({hidden})))"
    )
}

/// The SQL condition (on alias `i`) a rule takes titles by, and its
/// arguments, for a reader that may not see `hide`.
pub fn rule_sql(rule: &CollectionRule, hide: &[String]) -> (String, Vec<Sql>) {
    let mut wh: Vec<String> = Vec::new();
    let mut args: Vec<Sql> = Vec::new();
    let mut arg = |v: Sql| {
        args.push(v);
        format!("?{}", args.len() + 1000)
    };
    let hidden = arg(Sql::Text(
        serde_json::to_string(hide).unwrap_or_else(|_| "[]".into()),
    ));
    if rule.libraries.is_empty() {
        wh.push("i.library_id IN (SELECT id FROM library WHERE kind <> 'videos')".into());
    } else {
        let libs = arg(Sql::Text(
            serde_json::to_string(&rule.libraries).unwrap_or_default(),
        ));
        wh.push(format!("i.library_id IN (SELECT value FROM json_each({libs}))"));
    }
    let kinds: Vec<&str> = rule
        .kinds
        .iter()
        .filter_map(|k| match k {
            ItemKind::Movie => Some("movie"),
            ItemKind::Show => Some("show"),
            _ => None,
        })
        .collect();
    if kinds.is_empty() {
        wh.push("i.kind IN ('movie', 'show')".into());
    } else {
        wh.push(format!(
            "i.kind IN ({})",
            kinds
                .iter()
                .map(|k| format!("'{k}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !rule.genres.is_empty() {
        let g = arg(Sql::Text(lower_list(&rule.genres)));
        wh.push(format!(
            "(EXISTS (SELECT 1 FROM json_each(i.genres) x WHERE lower(x.value) IN (SELECT value FROM json_each({g})))
              AND {})",
            counts("genres", &hidden)
        ));
    }
    let detail_list =
        |key: &str, values: &[String], wh: &mut Vec<String>, arg: &mut dyn FnMut(Sql) -> String| {
            if values.is_empty() {
                return;
            }
            let v = arg(Sql::Text(lower_list(values)));
            wh.push(format!(
                "(EXISTS (SELECT 1 FROM json_each(json_extract(i.details, '$.{key}')) x
                       WHERE lower(x.value) IN (SELECT value FROM json_each({v})))
              AND {})",
                counts(key, &hidden)
            ));
        };
    detail_list("tags", &rule.tags, &mut wh, &mut arg);
    detail_list("studios", &rule.studios, &mut wh, &mut arg);
    detail_list("countries", &rule.countries, &mut wh, &mut arg);
    detail_list("languages", &rule.languages, &mut wh, &mut arg);
    if !rule.people.is_empty() {
        let p = arg(Sql::Text(lower_list(&rule.people)));
        let in_list = |key: &str| {
            format!(
                "(EXISTS (SELECT 1 FROM json_each(json_extract(i.details, '$.{key}')) x
                           WHERE lower(x.value) IN (SELECT value FROM json_each({p}))) AND {})",
                counts(key, &hidden)
            )
        };
        wh.push(format!(
            "({} OR {} OR (EXISTS (SELECT 1 FROM json_each(json_extract(i.details, '$.cast')) x
                     WHERE lower(json_extract(x.value, '$.name')) IN (SELECT value FROM json_each({p})))
                   AND {}))",
            in_list("directors"),
            in_list("writers"),
            counts("cast", &hidden)
        ));
    }
    if let Some(y) = rule.year_from {
        let a = arg(Sql::Integer(i64::from(y)));
        wh.push(format!("(i.year >= {a} AND {})", counts("year", &hidden)));
    }
    if let Some(y) = rule.year_to {
        let a = arg(Sql::Integer(i64::from(y)));
        wh.push(format!("(i.year <= {a} AND {})", counts("year", &hidden)));
    }
    if let Some(t) = rule.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        let pat = format!(
            "%{}%",
            t.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
        );
        let a = arg(Sql::Text(pat));
        wh.push(format!(
            "((i.title LIKE {a} ESCAPE '\\' AND {}) OR (i.original_title LIKE {a} ESCAPE '\\' AND {}))",
            counts("title", &hidden),
            counts("original_title", &hidden)
        ));
    }
    (wh.join(" AND "), args)
}

/// Renumber a condition's `?1001…` placeholders to follow `before`
/// arguments already bound (SQLite numbers them as written).
pub(crate) fn rebind(sql: &str, base: usize, n: usize) -> String {
    let mut out = sql.to_string();
    // Highest first, so `?1010` isn't read as `?101` + `0`.
    for i in (1..=n).rev() {
        out = out.replace(&format!("?{}", 1000 + i), &format!("?{}", base + i));
    }
    out
}

/// The condition (on alias `i`) for "in collection `r`": picked, or taken
/// by its rule; with its arguments (placeholders `?1001…`).
pub fn member_sql(r: &Row, hide: &[String]) -> (String, Vec<Sql>) {
    let picked = format!(
        "EXISTS (SELECT 1 FROM collection_item ci WHERE ci.collection_id = {} AND ci.item_id = i.id)",
        r.id
    );
    match r.rule.as_ref().filter(|x| !x.is_empty()) {
        None => (picked, Vec::new()),
        Some(rule) => {
            let (cond, args) = rule_sql(rule, hide);
            (
                format!("({picked} OR (i.parent_id IS NULL AND i.state <> 'missing' AND {cond}))"),
                args,
            )
        }
    }
}

/// The titles of collection `r` as a reader that may not see `hide` sees
/// it: picked ones (in order) first, then those its rule takes (by title).
/// At most [`MAX_LISTED`].
pub fn members(c: &Connection, r: &Row, hide: &[String]) -> Result<Vec<Id>> {
    let mut out: Vec<Id> = {
        let mut stmt = c.prepare_cached(
            "SELECT ci.item_id FROM collection_item ci JOIN item i ON i.id = ci.item_id
              WHERE ci.collection_id = ?1 AND i.state <> 'missing' ORDER BY ci.position, ci.item_id",
        )?;
        let rows = stmt.query_map([r.id], |x| x.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    if let Some(rule) = r.rule.as_ref().filter(|x| !x.is_empty()) {
        let (cond, args) = rule_sql(rule, hide);
        let cond = rebind(&cond, 0, args.len());
        let sql = format!(
            "SELECT i.id FROM item i WHERE i.parent_id IS NULL AND i.state <> 'missing' AND {cond}
              ORDER BY i.sort_title, i.id LIMIT {}",
            MAX_LISTED
        );
        let mut stmt = c.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(args.iter()), |x| x.get::<_, Id>(0))?;
        let picked: std::collections::HashSet<Id> = out.iter().copied().collect();
        for id in rows {
            let id = id?;
            if !picked.contains(&id) {
                out.push(id);
            }
        }
    }
    out.truncate(MAX_LISTED);
    Ok(out)
}

/// The notes of the titles picked for `r`, by item.
pub fn notes(c: &Connection, r: &Row) -> Result<Vec<(Id, String)>> {
    let mut stmt = c.prepare_cached(
        "SELECT item_id, note FROM collection_item WHERE collection_id = ?1 AND note IS NOT NULL ORDER BY position",
    )?;
    let rows = stmt.query_map([r.id], |x| Ok((x.get(0)?, x.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// What a reader that may not see `hide` sees of collection `r`.
pub fn info(c: &Connection, r: &Row, hide: &[String]) -> Result<CollectionInfo> {
    let members = members(c, r, hide)?;
    Ok(CollectionInfo {
        id: r.id,
        name: r.name.clone(),
        description: r.description.clone(),
        everyone: r.owner == EVERYONE,
        rule: r.rule.clone(),
        count: members.len() as u64,
        cover: members.iter().take(4).copied().collect(),
        added_by: r.added_by.clone(),
        agent: r.agent.clone(),
        sources: r.sources.clone(),
        created_at: r.created_at,
        updated_at: r.updated_at,
    })
}

/// The collections `user` (`None`: anonymous) may see: everyone's, then
/// theirs, by name.
pub fn list(c: &Connection, user: Option<Id>, hide: &[String]) -> Result<Vec<CollectionInfo>> {
    let rows: Vec<Row> = {
        let mut stmt = c.prepare_cached(&format!(
            "SELECT {ROW_COLS} FROM collection WHERE owner = 0 OR owner = ?1
              ORDER BY owner <> 0, name COLLATE NOCASE, id"
        ))?;
        let rows = stmt.query_map([user.unwrap_or(-1)], row)?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    rows.iter().map(|r| info(c, r, hide)).collect()
}

/// A collection to keep, checked by the caller (names tidied, sizes in
/// bounds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Put {
    pub id: Option<Id>,
    pub owner: Id,
    pub name: String,
    pub description: Option<String>,
    pub items: Option<Vec<CollectionPick>>,
    pub add: Vec<CollectionPick>,
    pub remove: Vec<Id>,
    /// `None`: keep the rule; `Some(empty)`: drop it.
    pub rule: Option<CollectionRule>,
    pub sources: Vec<Link>,
    pub by: String,
    pub agent: Option<String>,
}

/// Make or change a collection. A new one with the name of one of the
/// owner's replaces it (changes it).
pub fn put(c: &Connection, p: &Put, now: i64) -> Result<Row> {
    let existing: Option<Row> =
        match p.id {
            Some(id) => {
                let r = get(c, id)?.ok_or_else(|| CatalogError::NotFound(format!("collection {id}")))?;
                if r.owner != p.owner {
                    return Err(CatalogError::Invalid(format!(
                    "collection {id} isn't {}: change it as its owner (everyone's: as the Owner or an Admin)",
                    if p.owner == EVERYONE { "everyone's" } else { "yours" }
                )));
                }
                Some(r)
            }
            None => c
                .prepare_cached(&format!(
                    "SELECT {ROW_COLS} FROM collection WHERE owner = ?1 AND name = ?2 COLLATE NOCASE"
                ))?
                .query_row(params![p.owner, p.name], row)
                .optional()?,
        };
    // Every title picked is a film or show.
    let mut picks: Vec<&CollectionPick> = Vec::new();
    if let Some(items) = &p.items {
        picks.extend(items.iter());
    }
    picks.extend(p.add.iter());
    for pick in &picks {
        let ok: Option<(String, Option<Id>)> = c
            .prepare_cached("SELECT kind, parent_id FROM item WHERE id = ?1")?
            .query_row([pick.item], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        match ok {
            None => return Err(CatalogError::NotFound(format!("item {}", pick.item))),
            Some((k, None)) if k == "movie" || k == "show" => {}
            Some((k, _)) => {
                return Err(CatalogError::Invalid(format!(
                    "item {} is a {k}: a collection holds films and shows",
                    pick.item
                )))
            }
        }
    }
    let rule_json = |r: &Option<CollectionRule>| -> Result<Option<String>> {
        Ok(match r {
            Some(r) if !r.is_empty() => Some(serde_json::to_string(r)?),
            _ => None,
        })
    };
    let sources = (!p.sources.is_empty())
        .then(|| serde_json::to_string(&p.sources))
        .transpose()?;
    let id = match &existing {
        Some(r) => {
            let rule = match &p.rule {
                None => r.rule.as_ref().map(serde_json::to_string).transpose()?,
                some => rule_json(some)?,
            };
            c.prepare_cached(
                "UPDATE collection SET name = ?2, description = ?3, rule = ?4, sources = COALESCE(?5, sources),
                        agent = ?6, updated_at = ?7 WHERE id = ?1",
            )?
            .execute(params![r.id, p.name, p.description, rule, sources, p.agent, now])?;
            r.id
        }
        None => {
            let n: i64 = c
                .prepare_cached("SELECT count(*) FROM collection WHERE owner = ?1")?
                .query_row([p.owner], |r| r.get(0))?;
            if n as usize >= MAX_PER_OWNER {
                return Err(CatalogError::Invalid(format!(
                    "at most {MAX_PER_OWNER} collections each: remove one first"
                )));
            }
            c.prepare_cached(
                "INSERT INTO collection(owner, name, description, rule, added_by, agent, sources, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            )?
            .execute(params![
                p.owner,
                p.name,
                p.description,
                rule_json(&p.rule)?,
                p.by,
                p.agent,
                sources,
                now
            ])?;
            c.last_insert_rowid()
        }
    };
    if let Some(items) = &p.items {
        c.prepare_cached("DELETE FROM collection_item WHERE collection_id = ?1")?
            .execute([id])?;
        for (i, pick) in items.iter().enumerate() {
            c.prepare_cached(
                "INSERT INTO collection_item(collection_id, item_id, position, note) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(collection_id, item_id) DO UPDATE SET note = COALESCE(excluded.note, note)",
            )?
            .execute(params![id, pick.item, i as i64, pick.note])?;
        }
    }
    for item in &p.remove {
        c.prepare_cached("DELETE FROM collection_item WHERE collection_id = ?1 AND item_id = ?2")?
            .execute(params![id, item])?;
    }
    for pick in &p.add {
        let next: i64 = c
            .prepare_cached(
                "SELECT COALESCE(max(position) + 1, 0) FROM collection_item WHERE collection_id = ?1",
            )?
            .query_row([id], |r| r.get(0))?;
        c.prepare_cached(
            "INSERT INTO collection_item(collection_id, item_id, position, note) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(collection_id, item_id) DO UPDATE SET note = COALESCE(excluded.note, note)",
        )?
        .execute(params![id, pick.item, next, pick.note])?;
    }
    let n: i64 = c
        .prepare_cached("SELECT count(*) FROM collection_item WHERE collection_id = ?1")?
        .query_row([id], |r| r.get(0))?;
    if n as usize > MAX_PICKED {
        return Err(CatalogError::Invalid(format!(
            "a collection picks at most {MAX_PICKED} titles: use a rule for more"
        )));
    }
    get(c, id)?.ok_or_else(|| CatalogError::NotFound(format!("collection {id}")))
}

/// Forget collection `id` of `owner`. Returns whether it was there.
pub fn remove(c: &Connection, owner: Id, id: Id) -> Result<bool> {
    Ok(
        c.prepare_cached("DELETE FROM collection WHERE id = ?1 AND owner = ?2")?
            .execute(params![id, owner])?
            > 0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_renumbered_after_earlier_arguments() {
        assert_eq!(
            rebind("a = ?1001 AND b = ?1002 OR c = ?1010", 3, 10),
            "a = ?4 AND b = ?5 OR c = ?13"
        );
    }
}
