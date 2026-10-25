//! Changesets: every edit a person or an agent makes, journaled so it can
//! be reviewed and undone (docs/AGENT-NATIVE.md, "Changesets").
//!
//! - `changeset` is one edit (a summary, who made it, its state) and
//!   `change` its rows: one per field (`title`, `ids.tmdb`, …) or shown
//!   image (`art.poster`), each with the value before and after.
//! - **Apply** writes the values and records each field's source
//!   ([`crate::fields`]): the person or agent, the changeset, the links.
//! - **Propose** writes only the journal. Applying the proposal later skips
//!   a field that no longer holds what it held when proposed.
//! - **Undo** restores a field only while it still holds what the
//!   changeset set; a field changed since is kept, and reported.

use std::collections::BTreeSet;

use flux_core::agent::{ChangeState, ChangesetState, ChangesetSummary, Link};
use flux_core::api::Id;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::error::{CatalogError, Result};
use crate::fields::{self, FieldRow};

/// Most changes one changeset holds.
pub const MAX_CHANGES: usize = 1000;

/// Who an edit is made by: a login, and the agent acting for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    pub login: String,
    pub agent: Option<String>,
}

impl Actor {
    fn source(&self) -> &'static str {
        if self.agent.is_some() {
            fields::AGENT
        } else {
            fields::USER
        }
    }
}

/// One item's edit, checked by the caller ([`fields::check`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ItemEdit {
    pub item: Id,
    pub set: fields::Values,
    pub clear: Vec<String>,
    pub sources: Vec<Link>,
}

/// A field an edit would change.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub field: String,
    pub before: Option<Value>,
    /// What it will hold: for a cleared title, the file's title.
    pub after: Option<Value>,
    /// Who set the value before (`None`: the file).
    pub before_source: Option<FieldRow>,
    /// The field is handed back to flux (no source recorded).
    pub clear: bool,
}

/// What an edit would do to one item.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedItem {
    pub item: Id,
    /// The item's title before the edit.
    pub title: String,
    /// The provider that gave that title, if one did (`tmdb`), and then
    /// the title the file gives, for a reader who may not see the
    /// provider's.
    pub title_provider: Option<String>,
    pub file_title: Option<String>,
    pub kind: String,
    pub changes: Vec<Planned>,
    pub unchanged: Vec<String>,
    /// Fields left alone, and why.
    pub refused: Vec<(String, String)>,
    pub sources: Vec<Link>,
}

/// Work out what `edits` would change, without changing anything (a dry
/// run reads it from a reader; [`commit_edit`] runs it again in the
/// writer). An agent's edit of a field a person set is refused unless
/// `force`.
pub fn plan_edit(c: &Connection, actor: &Actor, edits: &[ItemEdit], force: bool) -> Result<Vec<PlannedItem>> {
    let mut out = Vec::with_capacity(edits.len());
    let mut items = BTreeSet::new();
    for e in edits {
        if !items.insert(e.item) {
            return Err(CatalogError::Invalid(format!(
                "item {} appears twice: give each item once, with all its fields",
                e.item
            )));
        }
        let cur =
            fields::read(c, e.item)?.ok_or_else(|| CatalogError::NotFound(format!("item {}", e.item)))?;
        let title_provider = fields::source_of(c, e.item, "title")?
            .filter(|s| s.source == fields::PROVIDER)
            .and_then(|s| s.provider);
        let file_title = match title_provider {
            Some(_) => fields::file_title(c, e.item)?,
            None => None,
        };
        let mut p = PlannedItem {
            item: e.item,
            title: cur
                .values
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            title_provider,
            file_title,
            kind: cur.kind.clone(),
            changes: Vec::new(),
            unchanged: Vec::new(),
            refused: Vec::new(),
            sources: e.sources.clone(),
        };
        let wanted = e
            .set
            .iter()
            .map(|(f, v)| (f.clone(), Some(v.clone())))
            .chain(e.clear.iter().map(|f| (f.clone(), None)));
        let mut seen = BTreeSet::new();
        for (field, after) in wanted {
            let clear = after.is_none();
            if !seen.insert(field.clone()) {
                p.refused.push((field, "set and cleared in one edit".into()));
                continue;
            }
            let before = cur.values.get(&field).cloned();
            let src = fields::source_of(c, e.item, &field)?;
            let after = match after {
                Some(v) => Some(v),
                None if field == "title" => match fields::file_title(c, e.item)? {
                    Some(t) => Some(Value::String(t)),
                    None => {
                        p.refused
                            .push((field, "no title from the file to go back to".into()));
                        continue;
                    }
                },
                None => None,
            };
            // Clearing a value that equals flux's own still hands the field back.
            if before == after && !(clear && src.as_ref().is_some_and(FieldRow::locks)) {
                p.unchanged.push(field);
                continue;
            }
            if actor.agent.is_some() && !force {
                if let Some(s) = src.as_ref().filter(|s| s.source == fields::USER) {
                    let who = s.actor.as_deref().unwrap_or("a person");
                    p.refused.push((
                        field,
                        format!("{who} set this by hand; replace it only if they asked you to (force)"),
                    ));
                    continue;
                }
            }
            p.changes.push(Planned {
                field,
                before,
                after,
                before_source: src,
                clear,
            });
        }
        out.push(p);
    }
    Ok(out)
}

/// What committing an edit or a decision changed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Committed {
    pub changeset: Option<ChangesetSummary>,
    /// Items whose values changed, with their library.
    pub items: Vec<(Id, Id)>,
    /// Art rows deleted, with their stored path (for the caller to delete
    /// the file it owns).
    pub art_removed: Vec<(Id, Option<String>)>,
    /// Changes left as they were, and why (item, field, reason).
    pub not_done: Vec<(Id, String, String)>,
    pub done: u32,
}

/// Make `edits` a changeset: applied now, or (`propose`) only proposed.
/// `jobs`: attempts for the match and art jobs it queues (0: none).
#[allow(clippy::too_many_arguments)]
pub fn commit_edit(
    c: &Connection,
    actor: &Actor,
    summary: &str,
    edits: &[ItemEdit],
    force: bool,
    propose: bool,
    now: i64,
    jobs: u32,
) -> Result<(Committed, Vec<PlannedItem>)> {
    let planned = plan_edit(c, actor, edits, force)?;
    let n: usize = planned.iter().map(|p| p.changes.len()).sum();
    if n == 0 {
        return Ok((Committed::default(), planned));
    }
    if n > MAX_CHANGES {
        return Err(CatalogError::Invalid(format!(
            "one changeset holds at most {MAX_CHANGES} changes; split the edit"
        )));
    }
    let state = if propose { "proposed" } else { "applied" };
    c.prepare_cached(
        "INSERT INTO changeset(actor, agent, summary, state, created_at, decided_at, decided_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?
    .execute(params![
        actor.login,
        actor.agent,
        summary,
        state,
        now,
        (!propose).then_some(now),
        (!propose).then_some(&actor.login),
    ])?;
    let cs = c.last_insert_rowid();
    let mut seq = 0i64;
    let mut out = Committed::default();
    for p in &planned {
        let sources = json_or_null(&p.sources)?;
        for ch in &p.changes {
            seq += 1;
            c.prepare_cached(
                "INSERT INTO change(changeset_id, seq, item_id, field, op, before, after, before_source, sources, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?
            .execute(params![
                cs,
                seq,
                p.item,
                ch.field,
                if ch.clear { "clear" } else { "set" },
                ch.before.as_ref().map(Value::to_string),
                ch.after.as_ref().map(Value::to_string),
                ch.before_source.as_ref().map(serde_json::to_string).transpose()?,
                sources,
                if propose { "pending" } else { "applied" },
            ])?;
            if !propose {
                let row = (!ch.clear).then(|| row_for(actor, cs, &p.sources, now));
                set_field(c, p.item, &ch.field, ch.after.as_ref(), row.as_ref(), now)?;
                out.done += 1;
            }
        }
        if !propose && !p.changes.is_empty() {
            after_edit(
                c,
                p.item,
                p.changes.iter().map(|c| (c.field.as_str(), c.clear)),
                now,
                jobs,
            )?;
            out.items.push((p.item, library_of(c, p.item)?));
        }
    }
    out.changeset = summary_of(c, cs)?;
    Ok((out, planned))
}

/// The source row an applied change records.
fn row_for(actor: &Actor, cs: Id, sources: &[Link], now: i64) -> FieldRow {
    FieldRow {
        source: actor.source().to_string(),
        provider: None,
        agent: actor.agent.clone(),
        actor: Some(actor.login.clone()),
        changeset: Some(cs),
        sources: sources.to_vec(),
        at: now,
    }
}

/// Write one field and its source: `row`, or for a cleared field none (it
/// is flux's again; a cleared title holds the file's).
fn set_field(
    c: &Connection,
    item: Id,
    field: &str,
    value: Option<&Value>,
    row: Option<&FieldRow>,
    now: i64,
) -> Result<()> {
    fields::write(c, item, field, value, now)?;
    fields::put_source(c, item, field, row)
}

/// After an item's fields changed (`changed`: each field, and whether it
/// was handed back): a film or show retitled gets its art again (flux's
/// poster carries the title and year), and one with new ids, a new title
/// or year, or a field handed back is matched again.
fn after_edit<'a>(
    c: &Connection,
    item: Id,
    changed: impl Iterator<Item = (&'a str, bool)>,
    now: i64,
    jobs: u32,
) -> Result<()> {
    let changed: Vec<(&str, bool)> = changed.collect();
    let top: Option<(String, Option<Id>, String)> = c
        .prepare_cached("SELECT i.kind, i.parent_id, l.kind FROM item i JOIN library l ON l.id = i.library_id WHERE i.id = ?1")?
        .query_row([item], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    let Some((kind, parent, library_kind)) = top else {
        return Ok(());
    };
    if jobs == 0 || parent.is_some() || !matches!(kind.as_str(), "movie" | "show") {
        return Ok(());
    }
    if changed.iter().any(|(f, _)| matches!(*f, "title" | "year")) {
        crate::jobs::rearm_art(c, item, now, jobs)?;
    }
    let rematch = changed
        .iter()
        .any(|(f, cleared)| *cleared || f.starts_with("ids.") || *f == "title" || *f == "year");
    if rematch && library_kind != "videos" {
        requeue_match(c, item, now, jobs)?;
    }
    Ok(())
}

/// Queue a match for `item` now (interactive priority), keyed by its title,
/// year and ids so an edit that changes them matches again. A `locked`
/// item is never matched.
fn requeue_match(c: &Connection, item: Id, now: i64, jobs: u32) -> Result<()> {
    type Row = (
        String,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        String,
    );
    let row: Option<Row> = c
        .prepare_cached(
            "SELECT group_key, year, tmdb_id, tvdb_id, imdb_id, match_state FROM item WHERE id = ?1",
        )?
        .query_row([item], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })
        .optional()?;
    let Some((group_key, year, tmdb, tvdb, imdb, state)) = row else {
        return Ok(());
    };
    if state == "locked" {
        return Ok(());
    }
    let title: String = c
        .prepare_cached("SELECT title FROM item WHERE id = ?1")?
        .query_row([item], |r| r.get(0))?;
    let ids = flux_core::ProviderIds {
        tmdb: tmdb.map(|v| v as u64),
        tvdb: tvdb.map(|v| v as u64),
        imdb,
    };
    // The title is part of the key: a new title from an edit is a new match.
    let key = format!(
        "{}|{}",
        crate::jobs::match_key(item, &group_key, year.and_then(|y| u16::try_from(y).ok()), &ids),
        crate::group::sort_title(&title)
    );
    let spec = crate::jobs::JobSpec::new(crate::JOB_MATCH, key)
        .item(item)
        .priority(crate::jobs::priority::INTERACTIVE);
    crate::jobs::enqueue(c, &spec, now, jobs)?;
    Ok(())
}

fn library_of(c: &Connection, item: Id) -> Result<Id> {
    Ok(c.prepare_cached("SELECT library_id FROM item WHERE id = ?1")?
        .query_row([item], |r| r.get(0))?)
}

fn json_or_null<T: serde::Serialize>(v: &[T]) -> Result<Option<String>> {
    Ok((!v.is_empty()).then(|| serde_json::to_string(v)).transpose()?)
}

// ---- art ---------------------------------------------------------------------------

/// The image `item` pins for `kind`, if any.
fn pinned(c: &Connection, item: Id, kind: &str) -> Result<Option<Id>> {
    Ok(c.prepare_cached(
        "SELECT id FROM art WHERE item_id = ?1 AND kind = ?2 AND pinned = 1 ORDER BY id DESC LIMIT 1",
    )?
    .query_row(params![item, kind], |r| r.get(0))
    .optional()?)
}

fn pin(c: &Connection, item: Id, kind: &str, art: Option<Id>) -> Result<()> {
    c.prepare_cached(
        "UPDATE art SET pinned = (id IS ?3) WHERE item_id = ?1 AND kind = ?2 AND pinned IS NOT (id IS ?3)",
    )?
    .execute(params![item, kind, art])?;
    Ok(())
}

/// Journal an image change of `item` (`art.<kind>`): `added` is a new
/// image (already inserted), `show` the image to pin (`None`: unpin, back
/// to flux's choice). Returns the changeset.
#[allow(clippy::too_many_arguments)]
pub fn commit_art(
    c: &Connection,
    actor: &Actor,
    summary: &str,
    item: Id,
    kind: &str,
    added: Option<Id>,
    show: Option<Id>,
    sources: &[Link],
    now: i64,
) -> Result<Committed> {
    let before = pinned(c, item, kind)?;
    if added.is_none() && before == show {
        return Ok(Committed::default());
    }
    c.prepare_cached(
        "INSERT INTO changeset(actor, agent, summary, state, created_at, decided_at, decided_by)
         VALUES (?1, ?2, ?3, 'applied', ?4, ?4, ?1)",
    )?
    .execute(params![actor.login, actor.agent, summary, now])?;
    let cs = c.last_insert_rowid();
    let after = match added {
        Some(a) => json!({ "pinned": show, "added": a }),
        None => json!({ "pinned": show }),
    };
    c.prepare_cached(
        "INSERT INTO change(changeset_id, seq, item_id, field, before, after, sources, state)
         VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6, 'applied')",
    )?
    .execute(params![
        cs,
        item,
        format!("art.{kind}"),
        json!({ "pinned": before }).to_string(),
        after.to_string(),
        json_or_null(sources)?,
    ])?;
    pin(c, item, kind, show)?;
    let mut out = Committed {
        done: 1,
        items: vec![(item, library_of(c, item)?)],
        ..Committed::default()
    };
    out.changeset = summary_of(c, cs)?;
    Ok(out)
}

// ---- decisions ---------------------------------------------------------------------

#[derive(Debug)]
struct ChangeRow {
    seq: i64,
    item: Id,
    field: String,
    clear: bool,
    before: Option<Value>,
    after: Option<Value>,
    before_source: Option<FieldRow>,
    sources: Vec<Link>,
    state: String,
}

fn change_rows(c: &Connection, cs: Id) -> Result<Vec<ChangeRow>> {
    let mut stmt = c.prepare_cached(
        "SELECT seq, item_id, field, before, after, before_source, sources, state, op FROM change
          WHERE changeset_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt.query_map([cs], |r| {
        let parse = |s: Option<String>| s.and_then(|s| serde_json::from_str::<Value>(&s).ok());
        Ok(ChangeRow {
            seq: r.get(0)?,
            item: r.get(1)?,
            field: r.get(2)?,
            before: parse(r.get(3)?),
            after: parse(r.get(4)?),
            before_source: r
                .get::<_, Option<String>>(5)?
                .and_then(|s| serde_json::from_str(&s).ok()),
            sources: r
                .get::<_, Option<String>>(6)?
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default(),
            state: r.get(7)?,
            clear: r.get::<_, String>(8)? == "clear",
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn changeset_head(c: &Connection, cs: Id) -> Result<(String, Option<String>, String)> {
    c.prepare_cached("SELECT actor, agent, state FROM changeset WHERE id = ?1")?
        .query_row([cs], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?
        .ok_or_else(|| CatalogError::NotFound(format!("changeset {cs}")))
}

fn set_change_state(c: &Connection, cs: Id, seq: i64, state: &str) -> Result<()> {
    c.prepare_cached("UPDATE change SET state = ?3 WHERE changeset_id = ?1 AND seq = ?2")?
        .execute(params![cs, seq, state])?;
    Ok(())
}

/// The value a field holds now (`None`: the item is gone, or the field
/// empty).
fn current(c: &Connection, item: Id, field: &str) -> Result<Option<Option<Value>>> {
    Ok(fields::read(c, item)?.map(|cur| cur.values.get(field).cloned()))
}

/// Apply a proposal: each change whose field still holds what it held when
/// proposed. `by` decides it.
pub fn apply_proposal(c: &Connection, cs: Id, by: &str, now: i64, jobs: u32) -> Result<Committed> {
    let (login, agent, state) = changeset_head(c, cs)?;
    if state != "proposed" {
        return Err(CatalogError::Invalid(format!(
            "changeset {cs} is {state}, not proposed"
        )));
    }
    let actor = Actor { login, agent };
    let mut out = Committed::default();
    let mut touched: Vec<(Id, Vec<(String, bool)>)> = Vec::new();
    for ch in change_rows(c, cs)? {
        if ch.state != "pending" {
            continue;
        }
        let now_value = current(c, ch.item, &ch.field)?;
        let Some(now_value) = now_value else {
            set_change_state(c, cs, ch.seq, "skipped")?;
            out.not_done.push((ch.item, ch.field, "the item is gone".into()));
            continue;
        };
        if now_value != ch.before {
            set_change_state(c, cs, ch.seq, "skipped")?;
            out.not_done
                .push((ch.item, ch.field, "it changed since it was proposed".into()));
            continue;
        }
        // What it holds now is what was proposed against: remember its
        // source for an undo.
        let src = fields::source_of(c, ch.item, &ch.field)?;
        c.prepare_cached("UPDATE change SET before_source = ?3 WHERE changeset_id = ?1 AND seq = ?2")?
            .execute(params![
                cs,
                ch.seq,
                src.as_ref().map(serde_json::to_string).transpose()?
            ])?;
        let row = (!ch.clear).then(|| row_for(&actor, cs, &ch.sources, now));
        set_field(c, ch.item, &ch.field, ch.after.as_ref(), row.as_ref(), now)?;
        set_change_state(c, cs, ch.seq, "applied")?;
        out.done += 1;
        match touched.iter_mut().find(|(i, _)| *i == ch.item) {
            Some((_, f)) => f.push((ch.field, ch.clear)),
            None => touched.push((ch.item, vec![(ch.field, ch.clear)])),
        }
    }
    for (item, f) in &touched {
        after_edit(c, *item, f.iter().map(|(f, cl)| (f.as_str(), *cl)), now, jobs)?;
        out.items.push((*item, library_of(c, *item)?));
    }
    c.prepare_cached(
        "UPDATE changeset SET state = 'applied', decided_at = ?2, decided_by = ?3 WHERE id = ?1",
    )?
    .execute(params![cs, now, by])?;
    out.changeset = summary_of(c, cs)?;
    Ok(out)
}

/// Turn a proposal down.
pub fn reject_proposal(c: &Connection, cs: Id, by: &str, now: i64) -> Result<Committed> {
    let (_, _, state) = changeset_head(c, cs)?;
    if state != "proposed" {
        return Err(CatalogError::Invalid(format!(
            "changeset {cs} is {state}, not proposed"
        )));
    }
    c.prepare_cached(
        "UPDATE changeset SET state = 'rejected', decided_at = ?2, decided_by = ?3 WHERE id = ?1",
    )?
    .execute(params![cs, now, by])?;
    Ok(Committed {
        changeset: summary_of(c, cs)?,
        ..Committed::default()
    })
}

/// Undo an applied changeset, newest change first: each field that still
/// holds what it set gets its value (and source) back.
pub fn revert(c: &Connection, cs: Id, by: &str, now: i64, jobs: u32) -> Result<Committed> {
    let (_, _, state) = changeset_head(c, cs)?;
    if state != "applied" {
        return Err(CatalogError::Invalid(format!(
            "changeset {cs} is {state}: only an applied one can be undone"
        )));
    }
    let mut out = Committed::default();
    let mut touched: Vec<(Id, Vec<(String, bool)>)> = Vec::new();
    let mut rows = change_rows(c, cs)?;
    rows.reverse();
    for ch in rows {
        if ch.state != "applied" {
            continue;
        }
        if let Some(kind) = ch.field.strip_prefix("art.") {
            match revert_art(c, &ch, kind)? {
                Ok(removed) => {
                    out.art_removed.extend(removed);
                    set_change_state(c, cs, ch.seq, "reverted")?;
                    out.done += 1;
                    if !out.items.iter().any(|(i, _)| *i == ch.item) {
                        if let Ok(lib) = library_of(c, ch.item) {
                            out.items.push((ch.item, lib));
                        }
                    }
                }
                Err(why) => {
                    set_change_state(c, cs, ch.seq, "kept")?;
                    out.not_done.push((ch.item, ch.field, why));
                }
            }
            continue;
        }
        let Some(now_value) = current(c, ch.item, &ch.field)? else {
            set_change_state(c, cs, ch.seq, "kept")?;
            out.not_done.push((ch.item, ch.field, "the item is gone".into()));
            continue;
        };
        if now_value != ch.after {
            set_change_state(c, cs, ch.seq, "kept")?;
            out.not_done
                .push((ch.item, ch.field, "it changed again since".into()));
            continue;
        }
        let before = match (&ch.before, ch.field.as_str()) {
            (None, "title") => {
                // A title is never empty; this can only be a corrupt row.
                set_change_state(c, cs, ch.seq, "kept")?;
                out.not_done
                    .push((ch.item, ch.field, "no title to go back to".into()));
                continue;
            }
            (b, _) => b.clone(),
        };
        fields::write(c, ch.item, &ch.field, before.as_ref(), now)?;
        fields::put_source(c, ch.item, &ch.field, ch.before_source.as_ref())?;
        set_change_state(c, cs, ch.seq, "reverted")?;
        out.done += 1;
        // Undoing hands a field back too (its value before may be flux's).
        match touched.iter_mut().find(|(i, _)| *i == ch.item) {
            Some((_, f)) => f.push((ch.field, true)),
            None => touched.push((ch.item, vec![(ch.field, true)])),
        }
    }
    for (item, f) in &touched {
        after_edit(c, *item, f.iter().map(|(f, cl)| (f.as_str(), *cl)), now, jobs)?;
        if !out.items.iter().any(|(i, _)| i == item) {
            out.items.push((*item, library_of(c, *item)?));
        }
    }
    c.prepare_cached(
        "UPDATE changeset SET state = 'reverted', reverted_at = ?2, reverted_by = ?3 WHERE id = ?1",
    )?
    .execute(params![cs, now, by])?;
    out.changeset = summary_of(c, cs)?;
    Ok(out)
}

/// The art rows an undo deleted (with their paths), or why it did nothing.
type ArtUndo = std::result::Result<Vec<(Id, Option<String>)>, String>;

/// Undo an image change: the pin goes back if it is still this change's;
/// an image the change added goes unless something pins it now. Returns
/// the deleted rows, or why nothing was done.
fn revert_art(c: &Connection, ch: &ChangeRow, kind: &str) -> Result<ArtUndo> {
    let id_of = |v: &Option<Value>, k: &str| v.as_ref().and_then(|v| v.get(k)).and_then(Value::as_i64);
    let (was, set, added) = (
        id_of(&ch.before, "pinned"),
        id_of(&ch.after, "pinned"),
        id_of(&ch.after, "added"),
    );
    if pinned(c, ch.item, kind)? != set {
        return Ok(Err("another image was chosen since".into()));
    }
    let was = match was {
        Some(w)
            if c.prepare_cached("SELECT 1 FROM art WHERE id = ?1")?
                .query_row([w], |_| Ok(()))
                .optional()?
                .is_some() =>
        {
            Some(w)
        }
        _ => None,
    };
    pin(c, ch.item, kind, was)?;
    let mut removed = Vec::new();
    if let Some(a) = added.filter(|a| Some(*a) != was) {
        let path: Option<Option<String>> = c
            .prepare_cached("DELETE FROM art WHERE id = ?1 AND pinned = 0 RETURNING path")?
            .query_row([a], |r| r.get(0))
            .optional()?;
        if let Some(path) = path {
            let shared: bool = match path.as_deref() {
                Some(p) => c
                    .prepare_cached("SELECT EXISTS (SELECT 1 FROM art WHERE path = ?1)")?
                    .query_row([p], |r| r.get(0))?,
                None => false,
            };
            removed.push((a, if shared { None } else { path }));
        }
    }
    Ok(Ok(removed))
}

// ---- reading -----------------------------------------------------------------------

fn parse_state(s: &str) -> ChangesetState {
    match s {
        "proposed" => ChangesetState::Proposed,
        "reverted" => ChangesetState::Reverted,
        "rejected" => ChangesetState::Rejected,
        _ => ChangesetState::Applied,
    }
}

fn state_str(s: ChangesetState) -> &'static str {
    match s {
        ChangesetState::Proposed => "proposed",
        ChangesetState::Applied => "applied",
        ChangesetState::Reverted => "reverted",
        ChangesetState::Rejected => "rejected",
    }
}

fn parse_change_state(s: &str) -> ChangeState {
    match s {
        "pending" => ChangeState::Pending,
        "skipped" => ChangeState::Skipped,
        "reverted" => ChangeState::Reverted,
        "kept" => ChangeState::Kept,
        _ => ChangeState::Applied,
    }
}

const SUMMARY_SQL: &str =
    "SELECT cs.id, cs.state, cs.summary, cs.agent, cs.actor, cs.created_at, cs.decided_at,
        cs.decided_by, cs.reverted_at, cs.reverted_by,
        (SELECT count(DISTINCT item_id) FROM change WHERE changeset_id = cs.id),
        (SELECT count(*) FROM change WHERE changeset_id = cs.id)
   FROM changeset cs";

fn summary_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ChangesetSummary> {
    Ok(ChangesetSummary {
        id: r.get(0)?,
        state: parse_state(&r.get::<_, String>(1)?),
        summary: r.get(2)?,
        agent: r.get(3)?,
        by: r.get(4)?,
        created_at: r.get(5)?,
        decided_at: r.get(6)?,
        decided_by: r.get(7)?,
        reverted_at: r.get(8)?,
        reverted_by: r.get(9)?,
        item_count: r.get::<_, i64>(10)? as u32,
        change_count: r.get::<_, i64>(11)? as u32,
    })
}

/// One changeset, briefly.
pub fn summary_of(c: &Connection, cs: Id) -> Result<Option<ChangesetSummary>> {
    Ok(c.prepare_cached(&format!("{SUMMARY_SQL} WHERE cs.id = ?1"))?
        .query_row([cs], summary_row)
        .optional()?)
}

/// Changesets newest first: in `state`, changing `item`, older than
/// `before`; at most `limit`.
pub fn list(
    c: &Connection,
    state: Option<ChangesetState>,
    item: Option<Id>,
    before: Option<Id>,
    limit: u32,
) -> Result<Vec<ChangesetSummary>> {
    let mut stmt = c.prepare_cached(&format!(
        "{SUMMARY_SQL}
          WHERE (?1 IS NULL OR cs.state = ?1)
            AND (?2 IS NULL OR EXISTS (SELECT 1 FROM change x WHERE x.changeset_id = cs.id AND x.item_id = ?2))
            AND (?3 IS NULL OR cs.id < ?3)
          ORDER BY cs.id DESC LIMIT ?4"
    ))?;
    let rows = stmt.query_map(
        params![state.map(state_str), item, before, limit.clamp(1, 200)],
        summary_row,
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// How many proposals wait for a decision.
pub fn proposed_count(c: &Connection) -> Result<u64> {
    Ok(
        c.prepare_cached("SELECT count(*) FROM changeset WHERE state = 'proposed'")?
            .query_row([], |r| r.get::<_, i64>(0))? as u64,
    )
}

/// One change as stored, with what a reader needs to show it.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangeView {
    pub item: Id,
    /// The item's title now (`None`: the item is gone).
    pub title: Option<String>,
    /// The provider of the item's title now (`tmdb`), if a provider's.
    pub title_provider: Option<String>,
    pub field: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
    /// The provider the value before came from, if a provider's.
    pub before_provider: Option<String>,
    pub sources: Vec<Link>,
    pub state: ChangeState,
}

/// One changeset with its changes.
pub fn get(c: &Connection, cs: Id) -> Result<Option<(ChangesetSummary, Vec<ChangeView>)>> {
    let Some(summary) = summary_of(c, cs)? else {
        return Ok(None);
    };
    let mut out = Vec::new();
    for ch in change_rows(c, cs)? {
        let head: Option<String> = c
            .prepare_cached("SELECT title FROM item WHERE id = ?1")?
            .query_row([ch.item], |r| r.get(0))
            .optional()?;
        let title_provider = match head {
            Some(_) => fields::source_of(c, ch.item, "title")?
                .filter(|s| s.source == fields::PROVIDER)
                .and_then(|s| s.provider),
            None => None,
        };
        out.push(ChangeView {
            item: ch.item,
            title: head,
            title_provider,
            before_provider: ch
                .before_source
                .as_ref()
                .filter(|s| s.source == fields::PROVIDER)
                .and_then(|s| s.provider.clone()),
            field: ch.field,
            before: ch.before,
            after: ch.after,
            sources: ch.sources,
            state: parse_change_state(&ch.state),
        });
    }
    Ok(Some((summary, out)))
}
