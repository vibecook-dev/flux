//! Structure fixes: what a file is when its name says otherwise
//! (docs/AGENT-NATIVE.md, "Structure fixes").
//!
//! flux places every file by the guess its name parser made ([`crate::group`]).
//! A fix (`file_fix`, [`FileFix`]) corrects that guess, and the scan places
//! the file by its guess with the fix applied ([`effective`]): a fix holds
//! through rescans, re-parses and renames, as it belongs to the file row,
//! which follows the file. A fix says
//!
//! - `of`: the work the file belongs to (a merge: two films that are one);
//! - `title`, `year`: what to group it by (a split: one item, two films);
//! - `season`, `episodes`: an episode's place;
//! - `kind`: a film, an episode, an extra, or nothing (`ignore`); and
//!   `extra`, `edition`, `part`.
//!
//! Changing fixes is a changeset ([`commit`]): a `file.fix` change per
//! file, with its fix before and after. The files are placed again at once
//! ([`crate::scan_support::refix`]); a work left with no files is carried
//! into the work its files went to, as when a rename moves them, and goes.
//! So that undoing the change brings that work back as it was, the change
//! keeps what people and agents had set on the file's items ([`Keep`]):
//! their fields, everyone's watched state, their collections and the images
//! chosen for them.

use std::collections::BTreeSet;

use flux_core::agent::{FileFix, FixKind, Link};
use flux_core::api::Id;
use flux_core::{GuessKind, NameGuess};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::changes::{self, Actor, Committed};
use crate::error::{CatalogError, Result};
use crate::fields::{self, FieldRow};

/// Most files one changeset fixes.
pub const MAX_FIXES: usize = 200;

/// `change.field` of a fix.
pub const FIELD: &str = "file.fix";

/// Who set a file's fix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixMeta {
    /// [`fields::USER`], [`fields::AGENT`] or [`fields::SIDECAR`].
    pub source: String,
    pub agent: Option<String>,
    pub actor: Option<String>,
    pub changeset: Option<Id>,
    pub at: i64,
}

/// A file's fix and who set it. A row that can't be read is no fix.
pub fn get(c: &Connection, file: Id) -> Result<Option<(FileFix, FixMeta)>> {
    type Row = (String, String, Option<String>, Option<String>, Option<Id>, i64);
    let row: Option<Row> = c
        .prepare_cached(
            "SELECT fix, source, agent, actor, changeset_id, at FROM file_fix WHERE file_id = ?1",
        )?
        .query_row([file], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })
        .optional()?;
    Ok(row.and_then(|(fix, source, agent, actor, changeset, at)| {
        let fix = match serde_json::from_str::<FileFix>(&fix) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(file, error = %e, "unreadable file fix ignored");
                return None;
            }
        };
        Some((
            fix,
            FixMeta {
                source,
                agent,
                actor,
                changeset,
                at,
            },
        ))
    }))
}

/// A file's fix, if it has one.
pub fn fix_of(c: &Connection, file: Id) -> Result<Option<FileFix>> {
    Ok(get(c, file)?.map(|(f, _)| f))
}

/// Set a file's fix, or with `None` (or an empty fix) clear it.
pub(crate) fn put(c: &Connection, file: Id, fix: Option<&FileFix>, meta: &FixMeta) -> Result<()> {
    match fix.filter(|f| !f.is_empty()) {
        Some(f) => {
            c.prepare_cached(
                "INSERT INTO file_fix(file_id, fix, of_item, source, agent, actor, changeset_id, at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(file_id) DO UPDATE SET fix = excluded.fix, of_item = excluded.of_item,
                    source = excluded.source, agent = excluded.agent, actor = excluded.actor,
                    changeset_id = excluded.changeset_id, at = excluded.at",
            )?
            .execute(params![
                file,
                serde_json::to_string(f)?,
                f.of,
                meta.source,
                meta.agent,
                meta.actor,
                meta.changeset,
                meta.at
            ])?;
        }
        None => {
            c.prepare_cached("DELETE FROM file_fix WHERE file_id = ?1")?
                .execute([file])?;
        }
    }
    Ok(())
}

/// `guess` corrected by `fix` (no I/O): what the file is placed by.
pub fn apply(guess: &NameGuess, fix: &FileFix) -> NameGuess {
    let mut g = guess.clone();
    match fix.kind {
        Some(FixKind::Movie) => {
            g.kind = GuessKind::Movie;
            g.season = None;
            g.episodes.clear();
            g.absolute_episode = None;
            g.air_date = None;
            g.extra = None;
        }
        Some(FixKind::Episode) => {
            g.kind = GuessKind::Episode;
            g.extra = None;
        }
        Some(FixKind::Extra) => g.kind = GuessKind::Extra,
        Some(FixKind::Ignore) => g.kind = GuessKind::Sample,
        None => {}
    }
    if let Some(t) = fix.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        g.title = Some(t.to_string());
    }
    if let Some(y) = fix.year {
        g.year = Some(y);
    }
    if let Some(s) = fix.season {
        g.season = Some(s);
    }
    if let Some(e) = fix.episodes.as_ref().filter(|e| !e.is_empty()) {
        g.episodes = e.clone();
        g.absolute_episode = None;
        g.air_date = None;
    }
    if let Some(x) = fix.extra {
        g.extra = Some(x);
        if fix.kind.is_none() {
            g.kind = GuessKind::Extra;
        }
    }
    if let Some(ed) = &fix.edition {
        g.edition = Some(ed.trim().to_string()).filter(|e| !e.is_empty());
    }
    if let Some(p) = fix.part {
        g.part = Some(p);
    }
    // Someone said what it is: not a hard name for AI-assisted matching.
    g.confidence = g.confidence.max(0.9);
    g.evidence.push("fixed by hand (files.fix)".into());
    g
}

/// What a file with a fix is placed by: its guess with the fix applied,
/// the work the fix pins it to, and the kind the fix gives it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Effective {
    pub guess: NameGuess,
    pub of: Option<Id>,
    pub kind: Option<FixKind>,
}

/// `file`'s guess with its fix; `None` when it has none.
pub(crate) fn effective(c: &Connection, file: Id, guess: &NameGuess) -> Result<Option<Effective>> {
    Ok(fix_of(c, file)?.map(|f| Effective {
        guess: apply(guess, &f),
        of: f.of,
        kind: f.kind,
    }))
}

/// `file`'s guess with its fix applied (the guess itself without one).
pub(crate) fn fixed(c: &Connection, file: Id, guess: NameGuess) -> Result<NameGuess> {
    Ok(match fix_of(c, file)? {
        Some(f) => apply(&guess, &f),
        None => guess,
    })
}

/// Fixes that pin files to `from` pin them to `into` instead: `from` was
/// merged into it.
pub(crate) fn remap(c: &Connection, from: Id, into: Id) -> Result<()> {
    if from == into {
        return Ok(());
    }
    let rows: Vec<(Id, String)> = {
        let mut stmt = c.prepare_cached("SELECT file_id, fix FROM file_fix WHERE of_item = ?1")?;
        let rows = stmt.query_map([from], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (file, json) in rows {
        let Ok(mut fix) = serde_json::from_str::<FileFix>(&json) else {
            continue;
        };
        fix.of = Some(into);
        c.prepare_cached("UPDATE file_fix SET fix = ?2, of_item = ?3 WHERE file_id = ?1")?
            .execute(params![file, serde_json::to_string(&fix)?, into])?;
    }
    Ok(())
}

// ---- what an undo brings back ------------------------------------------------------

/// One user's state of an item, as kept for an undo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StateRow {
    pub user: Id,
    pub asset: Option<Id>,
    pub position_ms: i64,
    pub duration_ms: i64,
    pub watched: bool,
    pub play_count: i64,
    pub last_played_at: Option<i64>,
    pub updated_at: i64,
}

/// What people and agents had set on an item that a fix may empty (it is
/// then carried into another and deleted): an undo that gives the item's
/// files back gives it back too.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Keep {
    pub item: Id,
    /// The values of the fields a person, an agent or a metadata file set,
    /// and their sources.
    #[serde(default, skip_serializing_if = "fields::Values::is_empty")]
    pub fields: fields::Values,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub sources: std::collections::BTreeMap<String, FieldRow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<StateRow>,
    /// `(collection, position, note)`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collections: Vec<(Id, i64, Option<String>)>,
    /// Images a person or an agent chose for it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub art: Vec<Id>,
}

impl Keep {
    fn is_empty(&self) -> bool {
        self.fields.is_empty() && self.states.is_empty() && self.collections.is_empty() && self.art.is_empty()
    }
}

/// What to keep of `item`, if anything.
pub(crate) fn keep(c: &Connection, item: Id) -> Result<Option<Keep>> {
    let mut k = Keep {
        item,
        ..Keep::default()
    };
    let sources = fields::sources(c, item)?;
    if sources.values().any(FieldRow::locks) {
        if let Some(cur) = fields::read(c, item)? {
            for (field, row) in sources.into_iter().filter(|(_, r)| r.locks()) {
                if let Some(v) = cur.values.get(&field) {
                    k.fields.insert(field.clone(), v.clone());
                }
                k.sources.insert(field, row);
            }
        }
    }
    {
        let mut stmt = c.prepare_cached(
            "SELECT user_id, asset_id, position_ms, duration_ms, watched, play_count, last_played_at, updated_at
               FROM user_state WHERE item_id = ?1 ORDER BY user_id",
        )?;
        let rows = stmt.query_map([item], |r| {
            Ok(StateRow {
                user: r.get(0)?,
                asset: r.get(1)?,
                position_ms: r.get(2)?,
                duration_ms: r.get(3)?,
                watched: r.get::<_, i64>(4)? != 0,
                play_count: r.get(5)?,
                last_played_at: r.get(6)?,
                updated_at: r.get(7)?,
            })
        })?;
        k.states = rows.collect::<rusqlite::Result<_>>()?;
    }
    {
        let mut stmt = c.prepare_cached(
            "SELECT collection_id, position, note FROM collection_item WHERE item_id = ?1 ORDER BY collection_id",
        )?;
        let rows = stmt.query_map([item], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        k.collections = rows.collect::<rusqlite::Result<_>>()?;
    }
    {
        let mut stmt =
            c.prepare_cached("SELECT id FROM art WHERE item_id = ?1 AND source = 'custom' ORDER BY id")?;
        let rows = stmt.query_map([item], |r| r.get(0))?;
        k.art = rows.collect::<rusqlite::Result<_>>()?;
    }
    Ok((!k.is_empty()).then_some(k))
}

/// Give `onto` (where a kept item's files are now) what was kept, where
/// nothing that counts as much is set on it now.
pub(crate) fn restore(c: &Connection, k: &Keep, onto: Id, now: i64) -> Result<()> {
    for (field, row) in &k.sources {
        let take = match fields::source_of(c, onto, field)? {
            None => true,
            Some(mine) => fields::rank(&row.source) > fields::rank(&mine.source),
        };
        if !take {
            continue;
        }
        match k.fields.get(field) {
            Some(v) => fields::write(c, onto, field, Some(v), now)?,
            None if field == "title" => continue,
            None => fields::write(c, onto, field, None, now)?,
        }
        fields::put_source(c, onto, field, Some(row))?;
    }
    for s in &k.states {
        let user_exists: bool = c
            .prepare_cached("SELECT EXISTS (SELECT 1 FROM user WHERE id = ?1)")?
            .query_row([s.user], |r| r.get(0))?;
        if !user_exists {
            continue;
        }
        let asset = match s.asset {
            Some(a) => c
                .prepare_cached("SELECT id FROM asset WHERE id = ?1")?
                .query_row([a], |r| r.get::<_, Id>(0))
                .optional()?,
            None => None,
        };
        c.prepare_cached(
            "INSERT INTO user_state(user_id, item_id, asset_id, position_ms, duration_ms, watched, play_count,
                                    last_played_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(user_id, item_id) DO UPDATE SET
                watched = MAX(watched, excluded.watched),
                play_count = MAX(play_count, excluded.play_count),
                asset_id = CASE WHEN excluded.updated_at > updated_at THEN excluded.asset_id ELSE asset_id END,
                position_ms = CASE WHEN excluded.updated_at > updated_at THEN excluded.position_ms ELSE position_ms END,
                duration_ms = CASE WHEN excluded.updated_at > updated_at THEN excluded.duration_ms ELSE duration_ms END,
                last_played_at = MAX(COALESCE(last_played_at, 0), COALESCE(excluded.last_played_at, 0)),
                updated_at = MAX(updated_at, excluded.updated_at)",
        )?
        .execute(params![
            s.user,
            onto,
            asset,
            s.position_ms,
            s.duration_ms,
            s.watched,
            s.play_count,
            s.last_played_at,
            s.updated_at
        ])?;
    }
    for (coll, position, note) in &k.collections {
        c.prepare_cached(
            "INSERT OR IGNORE INTO collection_item(collection_id, item_id, position, note)
             SELECT ?1, ?2, ?3, ?4 WHERE EXISTS (SELECT 1 FROM collection WHERE id = ?1)",
        )?
        .execute(params![coll, onto, position, note])?;
    }
    for a in &k.art {
        c.prepare_cached("UPDATE art SET item_id = ?2 WHERE id = ?1 AND source = 'custom'")?
            .execute(params![a, onto])?;
    }
    Ok(())
}

// ---- reading -------------------------------------------------------------------------

/// The item a file belongs to, as a reader that may not see `hide` names
/// it: `(item, title)`; an episode's title has its show and number.
pub fn place_of(c: &Connection, file: Id, hide: &[String]) -> Result<(Option<Id>, Option<String>)> {
    let item: Option<Id> = c
        .prepare_cached("SELECT s.item_id FROM file f JOIN asset s ON s.id = f.asset_id WHERE f.id = ?1")?
        .query_row([file], |r| r.get(0))
        .optional()?;
    let Some(item) = item else {
        return Ok((None, None));
    };
    Ok((Some(item), item_label(c, item, hide)?))
}

/// An item's title as a reader that may not see `hide` sees it, with an
/// episode's show and number (`Show · S01E03`).
pub fn item_label(c: &Connection, item: Id, hide: &[String]) -> Result<Option<String>> {
    let Some(a) = crate::agent_read::agent_item(c, item, hide)? else {
        return Ok(None);
    };
    if a.kind == flux_core::api::ItemKind::Episode {
        let show: Option<Id> = c
            .prepare_cached(
                "SELECT se.parent_id FROM item ep JOIN item se ON se.id = ep.parent_id WHERE ep.id = ?1",
            )?
            .query_row([item], |r| r.get(0))
            .optional()?
            .flatten();
        if let Some(show) = show.and_then(|s| crate::agent_read::agent_item(c, s, hide).transpose()) {
            let show = show?;
            return Ok(Some(match &a.subtitle {
                Some(sub) => format!("{} · {sub} · {}", show.title, a.title),
                None => format!("{} · {}", show.title, a.title),
            }));
        }
    }
    Ok(Some(a.title))
}

// ---- changesets ------------------------------------------------------------------------

/// One file's fix, checked by the caller's schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixEdit {
    pub file: Id,
    /// `None` (or an empty fix): clear it.
    pub fix: Option<FileFix>,
    pub sources: Vec<Link>,
}

/// What a fix did, or would do, to one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixOutcome {
    pub file: Id,
    pub path: String,
    pub before: (Option<Id>, Option<String>),
    /// `None` for a proposal.
    pub after: Option<(Option<Id>, Option<String>)>,
    pub fix: Option<FileFix>,
    pub unchanged: Option<String>,
}

/// A file as a fix needs it.
struct FileHead {
    path: String,
    library: Id,
    library_kind: String,
    asset: Option<Id>,
    guess: Option<NameGuess>,
}

fn head(c: &Connection, file: Id) -> Result<Option<FileHead>> {
    type Row = (String, Id, String, Option<Id>, Option<String>);
    let row: Option<Row> = c
        .prepare_cached(
            "SELECT f.rel_path, r.library_id, l.kind, f.asset_id, m.guess
               FROM file f JOIN root r ON r.id = f.root_id JOIN library l ON l.id = r.library_id
               LEFT JOIN file_meta m ON m.file_id = f.id
              WHERE f.id = ?1",
        )?
        .query_row([file], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .optional()?;
    Ok(row.map(|(path, library, library_kind, asset, guess)| FileHead {
        path,
        library,
        library_kind,
        asset,
        guess: guess.and_then(|g| serde_json::from_str(&g).ok()),
    }))
}

fn invalid(file: Id, why: impl std::fmt::Display) -> CatalogError {
    CatalogError::Invalid(format!("file {file}: {why}"))
}

/// Check a fix against its file and library; returns it tidied (`None`
/// when it says nothing).
fn check(c: &Connection, file: Id, h: &FileHead, fix: Option<&FileFix>) -> Result<Option<FileFix>> {
    let Some(f) = fix else { return Ok(None) };
    let mut f = f.clone();
    f.title = f
        .title
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|t| !t.is_empty());
    f.edition = f
        .edition
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|t| !t.is_empty());
    if f.is_empty() {
        return Ok(None);
    }
    if h.library_kind == "videos"
        && f != (FileFix {
            kind: Some(FixKind::Ignore),
            ..FileFix::default()
        })
    {
        return Err(invalid(
            file,
            "a videos library keeps every file as its own video: only kind ignore applies (edit the item to retitle it)",
        ));
    }
    if let Some(t) = &f.title {
        if t.chars().count() > fields::MAX_SHORT {
            return Err(invalid(
                file,
                format!("title is longer than {} characters", fields::MAX_SHORT),
            ));
        }
    }
    if let Some(e) = &f.edition {
        if e.chars().count() > 100 {
            return Err(invalid(file, "edition is longer than 100 characters"));
        }
    }
    if let Some(y) = f.year {
        if !(1870..=2100).contains(&y) {
            return Err(invalid(file, format!("year {y} is not between 1870 and 2100")));
        }
    }
    if f.season.is_some_and(|s| s > 999) {
        return Err(invalid(file, "season is more than 999"));
    }
    if let Some(eps) = &mut f.episodes {
        eps.sort_unstable();
        eps.dedup();
        if eps.is_empty() {
            f.episodes = None;
        } else if eps.len() > 20 || eps.iter().any(|&e| e > 9999) {
            return Err(invalid(file, "episodes: at most 20 numbers, each at most 9999"));
        }
    }
    if f.part.is_some_and(|p| !(1..=99).contains(&p)) {
        return Err(invalid(file, "part is between 1 and 99"));
    }
    // What the file will be, to check `of` against.
    let guess = apply(h.guess.as_ref().unwrap_or(&NameGuess::unknown()), &f);
    if f.kind == Some(FixKind::Episode) && guess.episodes.is_empty() && guess.absolute_episode.is_none() {
        return Err(invalid(
            file,
            "an episode needs its number: give season and episodes",
        ));
    }
    if let Some(of) = f.of {
        if guess.kind == GuessKind::Sample {
            return Err(invalid(file, "an ignored file belongs to nothing: leave out of"));
        }
        let target: Option<(String, Option<Id>, Id)> = c
            .prepare_cached("SELECT kind, parent_id, library_id FROM item WHERE id = ?1")?
            .query_row([of], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        let Some((kind, parent, library)) = target else {
            return Err(CatalogError::NotFound(format!("item {of}")));
        };
        if parent.is_some() || !matches!(kind.as_str(), "movie" | "show") {
            return Err(invalid(
                file,
                format!("of must be a film or a show; item {of} is a {kind}"),
            ));
        }
        if library != h.library {
            return Err(invalid(file, format!("item {of} is in another library")));
        }
        let fits = match guess.kind {
            GuessKind::Episode => kind == "show",
            GuessKind::Extra => true,
            _ => kind == "movie",
        };
        if !fits {
            let what = if guess.kind == GuessKind::Episode {
                "an episode"
            } else {
                "a film"
            };
            return Err(invalid(
                file,
                format!("it is {what}, and item {of} is a {kind} (set kind to say what the file is)"),
            ));
        }
    }
    Ok(Some(f))
}

/// Every file of a work: a film's versions and extras, a show's episodes
/// and extras.
pub fn files_of_work(c: &Connection, item: Id) -> Result<Vec<Id>> {
    let mut stmt = c.prepare_cached(
        "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION ALL SELECT i.id FROM item i JOIN sub ON i.parent_id = sub.id)
         SELECT a.file_id FROM sub JOIN asset_item ai ON ai.item_id = sub.id JOIN asset a ON a.id = ai.asset_id
         UNION SELECT a.file_id FROM sub JOIN asset a ON a.item_id = sub.id
         ORDER BY 1",
    )?;
    let rows = stmt.query_map([item], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The fixes that merge works `from` into `into` (`items.merge`): every
/// file of each is fixed to belong to `into`, keeping the rest of its fix.
pub fn merge_edits(c: &Connection, into: Id, from: &[Id], sources: &[Link]) -> Result<Vec<FixEdit>> {
    let head = |id: Id| -> Result<Option<(String, Option<Id>, Id)>> {
        Ok(
            c.prepare_cached("SELECT kind, parent_id, library_id FROM item WHERE id = ?1")?
                .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()?,
        )
    };
    let (kind, parent, library) =
        head(into)?.ok_or_else(|| CatalogError::NotFound(format!("item {into}")))?;
    if parent.is_some() || !matches!(kind.as_str(), "movie" | "show") {
        return Err(CatalogError::Invalid(format!(
            "into must be a film or a show; item {into} is a {kind}"
        )));
    }
    if from.is_empty() {
        return Err(CatalogError::Invalid("from names no item to merge".into()));
    }
    let mut seen = BTreeSet::new();
    let mut edits = Vec::new();
    for &f in from {
        if f == into || !seen.insert(f) {
            return Err(CatalogError::Invalid(format!(
                "item {f} is named twice (or is into itself)"
            )));
        }
        let (k, p, l) = head(f)?.ok_or_else(|| CatalogError::NotFound(format!("item {f}")))?;
        if p.is_some() || k != kind {
            return Err(CatalogError::Invalid(format!(
                "item {f} is a {k}: only a {kind} merges into {into}, a {kind}"
            )));
        }
        if l != library {
            return Err(CatalogError::Invalid(format!("item {f} is in another library")));
        }
        for file in files_of_work(c, f)? {
            let mut fix = fix_of(c, file)?.unwrap_or_default();
            fix.of = Some(into);
            edits.push(FixEdit {
                file,
                fix: Some(fix),
                sources: sources.to_vec(),
            });
        }
    }
    if edits.is_empty() {
        return Err(CatalogError::Invalid("those items have no files to merge".into()));
    }
    Ok(edits)
}

/// One planned fix.
struct Planned {
    edit: FixEdit,
    head: FileHead,
    before: Option<FileFix>,
    before_meta: Option<FixMeta>,
    after: Option<FileFix>,
    unchanged: Option<String>,
}

fn plan(c: &Connection, edits: &[FixEdit]) -> Result<Vec<Planned>> {
    if edits.len() > MAX_FIXES {
        return Err(CatalogError::Invalid(format!(
            "one changeset fixes at most {MAX_FIXES} files: split the work"
        )));
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(edits.len());
    for e in edits {
        if !seen.insert(e.file) {
            return Err(CatalogError::Invalid(format!(
                "file {} appears twice: give each file one fix",
                e.file
            )));
        }
        let h = head(c, e.file)?.ok_or_else(|| CatalogError::NotFound(format!("file {}", e.file)))?;
        let after = check(c, e.file, &h, e.fix.as_ref())?;
        let (before, before_meta) = match get(c, e.file)? {
            Some((f, m)) => (Some(f), Some(m)),
            None => (None, None),
        };
        let unchanged = (before == after).then(|| match after {
            Some(_) => "it already has this fix".to_string(),
            None => "it has no fix to clear".to_string(),
        });
        out.push(Planned {
            edit: e.clone(),
            head: h,
            before,
            before_meta,
            after,
            unchanged,
        });
    }
    Ok(out)
}

/// The top-level work of `item` (itself for a film or show).
fn top_of(c: &Connection, item: Id) -> Result<Id> {
    let mut cur = item;
    for _ in 0..4 {
        let parent: Option<Id> = c
            .prepare_cached("SELECT parent_id FROM item WHERE id = ?1")?
            .query_row([cur], |r| r.get(0))
            .optional()?
            .flatten();
        match parent {
            Some(p) => cur = p,
            None => break,
        }
    }
    Ok(cur)
}

/// The `before` of a fix change: the file, its fix (and who set it), the
/// item it's in, and what to bring back if that item (or its film or show)
/// goes.
fn before_json(
    c: &Connection,
    file: Id,
    path: &str,
    fix: Option<&FileFix>,
    meta: Option<&FixMeta>,
    item: Option<Id>,
) -> Result<Value> {
    let mut keeps = Vec::new();
    if let Some(i) = item {
        if let Some(k) = keep(c, i)? {
            keeps.push(k);
        }
        let top = top_of(c, i)?;
        if top != i {
            if let Some(k) = keep(c, top)? {
                keeps.push(k);
            }
        }
    }
    let mut v = json!({ "file": file, "path": path, "fix": fix, "item": item });
    if let Some(m) = meta {
        v["meta"] = serde_json::to_value(m)?;
    }
    if !keeps.is_empty() {
        v["keep"] = serde_json::to_value(keeps)?;
    }
    Ok(v)
}

/// Fix files as one changeset: applied now, or (`propose`) proposed; with
/// `dry_run`, worked out and rolled back. Returns what changed for each
/// file, as a reader that may not see `hide` names the items.
#[allow(clippy::too_many_arguments)]
pub fn commit(
    c: &Connection,
    actor: &Actor,
    summary: &str,
    edits: &[FixEdit],
    propose: bool,
    dry_run: bool,
    hide: &[String],
    now: i64,
    jobs: u32,
) -> Result<(Committed, Vec<FixOutcome>)> {
    let planned = plan(c, edits)?;
    let mut outcomes: Vec<FixOutcome> = Vec::with_capacity(planned.len());
    for p in &planned {
        outcomes.push(FixOutcome {
            file: p.edit.file,
            path: p.head.path.clone(),
            before: place_of(c, p.edit.file, hide)?,
            after: None,
            fix: p.after.clone(),
            unchanged: p.unchanged.clone(),
        });
    }
    let todo: Vec<&Planned> = planned.iter().filter(|p| p.unchanged.is_none()).collect();
    if todo.is_empty() {
        return Ok((Committed::default(), outcomes));
    }
    if dry_run {
        c.execute_batch("SAVEPOINT dry_fix")?;
        let res = (|| -> Result<()> {
            let meta = FixMeta {
                source: actor_source(actor).into(),
                agent: actor.agent.clone(),
                actor: Some(actor.login.clone()),
                changeset: None,
                at: now,
            };
            let mut files = Vec::with_capacity(todo.len());
            for p in &todo {
                put(c, p.edit.file, p.after.as_ref(), &meta)?;
                files.push((p.edit.file, p.before.clone()));
            }
            crate::scan_support::refix(c, &files, 0)?;
            for o in outcomes.iter_mut().filter(|o| o.unchanged.is_none()) {
                o.after = Some(place_of(c, o.file, hide)?);
            }
            Ok(())
        })();
        c.execute_batch("ROLLBACK TO dry_fix; RELEASE dry_fix")?;
        res?;
        return Ok((Committed::default(), outcomes));
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
    let mut rows = Vec::with_capacity(todo.len());
    for (i, p) in todo.iter().enumerate() {
        let item: Option<Id> = match p.head.asset {
            Some(a) => c
                .prepare_cached("SELECT item_id FROM asset WHERE id = ?1")?
                .query_row([a], |r| r.get(0))
                .optional()?,
            None => None,
        };
        let before = before_json(
            c,
            p.edit.file,
            &p.head.path,
            p.before.as_ref(),
            p.before_meta.as_ref(),
            item,
        )?;
        let after = json!({ "file": p.edit.file, "path": p.head.path, "fix": p.after });
        let sources = (!p.edit.sources.is_empty())
            .then(|| serde_json::to_string(&p.edit.sources))
            .transpose()?;
        c.prepare_cached(
            "INSERT INTO change(changeset_id, seq, item_id, field, op, before, after, sources, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?
        .execute(params![
            cs,
            i as i64 + 1,
            item.unwrap_or(0),
            FIELD,
            if p.after.is_some() { "set" } else { "clear" },
            before.to_string(),
            after.to_string(),
            sources,
            if propose { "pending" } else { "applied" },
        ])?;
        rows.push((i as i64 + 1, p.edit.file, p.before.clone(), p.after.clone()));
    }
    let mut out = Committed::default();
    if !propose {
        let meta = FixMeta {
            source: actor_source(actor).into(),
            agent: actor.agent.clone(),
            actor: Some(actor.login.clone()),
            changeset: Some(cs),
            at: now,
        };
        let mut files = Vec::with_capacity(rows.len());
        for (_, file, before, after) in &rows {
            put(c, *file, after.as_ref(), &meta)?;
            files.push((*file, before.clone()));
        }
        let placed = crate::scan_support::refix(c, &files, jobs)?;
        out.done = rows.len() as u32;
        record_landing(c, cs, &rows)?;
        out.absorb(placed);
        for o in outcomes.iter_mut().filter(|o| o.unchanged.is_none()) {
            o.after = Some(place_of(c, o.file, hide)?);
        }
    }
    out.changeset = changes::summary_of(c, cs)?;
    Ok((out, outcomes))
}

fn actor_source(actor: &Actor) -> &'static str {
    if actor.agent.is_some() {
        fields::AGENT
    } else {
        fields::USER
    }
}

/// Note in each applied change where its file went (`after.item`), for
/// the history.
fn record_landing(
    c: &Connection,
    cs: Id,
    rows: &[(i64, Id, Option<FileFix>, Option<FileFix>)],
) -> Result<()> {
    for (seq, file, _, _) in rows {
        let item: Option<Id> = c
            .prepare_cached("SELECT s.item_id FROM file f JOIN asset s ON s.id = f.asset_id WHERE f.id = ?1")?
            .query_row([file], |r| r.get(0))
            .optional()?;
        c.prepare_cached(
            "UPDATE change SET after = json_set(after, '$.item', ?3) WHERE changeset_id = ?1 AND seq = ?2",
        )?
        .execute(params![cs, seq, item])?;
    }
    Ok(())
}

/// A fix change as `changes` stores it.
#[derive(Debug, Clone)]
pub(crate) struct FixChange {
    pub seq: i64,
    pub before: Option<Value>,
    pub after: Option<Value>,
}

fn file_of(v: &Option<Value>) -> Option<Id> {
    v.as_ref().and_then(|v| v.get("file")).and_then(Value::as_i64)
}

fn fix_in(v: &Option<Value>) -> Option<FileFix> {
    v.as_ref()
        .and_then(|v| v.get("fix"))
        .filter(|f| !f.is_null())
        .and_then(|f| serde_json::from_value(f.clone()).ok())
}

/// Apply the pending fix changes of proposal `cs` (decided by its maker,
/// `actor`): each whose file still has the fix it had when proposed.
/// Returns the changes not done (seq, file, reason).
pub(crate) fn apply_rows(
    c: &Connection,
    cs: Id,
    actor: &Actor,
    rows: &[FixChange],
    now: i64,
    jobs: u32,
    out: &mut Committed,
) -> Result<Vec<(i64, Id, String)>> {
    let mut not_done = Vec::new();
    let mut done: Vec<(i64, Id, Option<FileFix>, Option<FileFix>)> = Vec::new();
    for r in rows {
        let Some(file) = file_of(&r.after) else {
            not_done.push((r.seq, 0, "the change names no file".into()));
            continue;
        };
        if head(c, file)?.is_none() {
            not_done.push((r.seq, file, "the file is gone".into()));
            continue;
        }
        let (before, after) = (fix_in(&r.before), fix_in(&r.after));
        if fix_of(c, file)? != before {
            not_done.push((r.seq, file, "its fix changed since it was proposed".into()));
            continue;
        }
        // Keep what its items hold now, for an undo.
        let item: Option<Id> = c
            .prepare_cached("SELECT s.item_id FROM file f JOIN asset s ON s.id = f.asset_id WHERE f.id = ?1")?
            .query_row([file], |r| r.get(0))
            .optional()?;
        let path = r
            .before
            .as_ref()
            .and_then(|v| v.get("path"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let meta = get(c, file)?.map(|(_, m)| m);
        let b = before_json(c, file, &path, before.as_ref(), meta.as_ref(), item)?;
        c.prepare_cached("UPDATE change SET before = ?3, item_id = ?4 WHERE changeset_id = ?1 AND seq = ?2")?
            .execute(params![cs, r.seq, b.to_string(), item.unwrap_or(0)])?;
        done.push((r.seq, file, before, after));
    }
    if !done.is_empty() {
        let meta = FixMeta {
            source: actor_source(actor).into(),
            agent: actor.agent.clone(),
            actor: Some(actor.login.clone()),
            changeset: Some(cs),
            at: now,
        };
        let mut files = Vec::with_capacity(done.len());
        for (_, file, before, after) in &done {
            put(c, *file, after.as_ref(), &meta)?;
            files.push((*file, before.clone()));
        }
        let placed = crate::scan_support::refix(c, &files, jobs)?;
        out.absorb(placed);
        record_landing(c, cs, &done)?;
        out.done += done.len() as u32;
    }
    for (seq, _, _, _) in &done {
        c.prepare_cached("UPDATE change SET state = 'applied' WHERE changeset_id = ?1 AND seq = ?2")?
            .execute(params![cs, seq])?;
    }
    Ok(not_done)
}

/// Undo the applied fix changes of `cs`: each file whose fix is still the
/// one set gets its fix before back, and is placed again; an item that
/// went is given back what it held ([`Keep`]). Returns the changes not
/// undone (seq, file, reason).
pub(crate) fn revert_rows(
    c: &Connection,
    cs: Id,
    rows: &[FixChange],
    now: i64,
    jobs: u32,
    out: &mut Committed,
) -> Result<Vec<(i64, Id, String)>> {
    let mut not_done = Vec::new();
    type Undo = (
        i64,
        Id,
        Option<FileFix>,
        Option<FileFix>,
        Option<FixMeta>,
        Vec<Keep>,
    );
    let mut undo: Vec<Undo> = Vec::new();
    for r in rows {
        let Some(file) = file_of(&r.before) else {
            not_done.push((r.seq, 0, "the change names no file".into()));
            continue;
        };
        if head(c, file)?.is_none() {
            not_done.push((r.seq, file, "the file is gone".into()));
            continue;
        }
        let (before, after) = (fix_in(&r.before), fix_in(&r.after));
        let now_fix = get(c, file)?;
        if now_fix.as_ref().map(|(f, _)| f) != after.as_ref() {
            not_done.push((r.seq, file, "its fix changed again since".into()));
            continue;
        }
        let keeps: Vec<Keep> = r
            .before
            .as_ref()
            .and_then(|v| v.get("keep"))
            .and_then(|k| serde_json::from_value(k.clone()).ok())
            .unwrap_or_default();
        let meta: Option<FixMeta> = r
            .before
            .as_ref()
            .and_then(|v| v.get("meta"))
            .and_then(|m| serde_json::from_value(m.clone()).ok());
        undo.push((r.seq, file, after, before, meta, keeps));
    }
    if undo.is_empty() {
        return Ok(not_done);
    }
    // The fix before goes back with who set it.
    let mut files = Vec::with_capacity(undo.len());
    for (_, file, after, before, meta, _) in &undo {
        let meta = meta.clone().unwrap_or(FixMeta {
            source: fields::USER.into(),
            agent: None,
            actor: None,
            changeset: None,
            at: now,
        });
        put(c, *file, before.as_ref(), &meta)?;
        files.push((*file, after.clone()));
    }
    let placed = crate::scan_support::refix(c, &files, jobs)?;
    // Items the files made again (a new item may reuse a gone one's id).
    let fresh: BTreeSet<Id> = placed
        .iter()
        .flat_map(|(_, o)| o.items_added.iter().copied())
        .collect();
    out.absorb(placed);
    // Items that went come back with what they held.
    for (_, file, _, _, _, keeps) in &undo {
        let landed: Option<Id> = c
            .prepare_cached("SELECT s.item_id FROM file f JOIN asset s ON s.id = f.asset_id WHERE f.id = ?1")?
            .query_row([file], |r| r.get(0))
            .optional()?;
        let Some(leaf) = landed else { continue };
        let top = top_of(c, leaf)?;
        for (i, k) in keeps.iter().enumerate() {
            let gone = fresh.contains(&k.item)
                || c.prepare_cached("SELECT NOT EXISTS (SELECT 1 FROM item WHERE id = ?1)")?
                    .query_row([k.item], |r| r.get::<_, bool>(0))?;
            // The first kept is the file's own item, the second its film or show.
            let onto = if i == 0 { leaf } else { top };
            if !gone || !(fresh.contains(&onto) || onto != k.item) {
                continue;
            }
            restore(c, k, onto, now)?;
            if let Ok(lib) = c
                .prepare_cached("SELECT library_id FROM item WHERE id = ?1")?
                .query_row([onto], |r| r.get::<_, Id>(0))
            {
                if !out.items.contains(&(onto, lib)) {
                    out.items.push((onto, lib));
                }
            }
        }
    }
    for (seq, _, _, _, _, _) in &undo {
        c.prepare_cached("UPDATE change SET state = 'reverted' WHERE changeset_id = ?1 AND seq = ?2")?
            .execute(params![cs, seq])?;
    }
    out.done += undo.len() as u32;
    Ok(not_done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_core::ExtraKind;

    fn guess(kind: GuessKind, title: &str) -> NameGuess {
        NameGuess {
            kind,
            title: Some(title.into()),
            confidence: 0.4,
            ..NameGuess::unknown()
        }
    }

    #[test]
    fn a_fix_corrects_only_what_it_names() {
        let mut g = guess(GuessKind::Episode, "Show");
        g.season = Some(1);
        g.episodes = vec![17];
        g.episode_title = Some("Pilot".into());
        let f = FileFix {
            season: Some(2),
            episodes: Some(vec![5]),
            ..FileFix::default()
        };
        let x = apply(&g, &f);
        assert_eq!((x.season, x.episodes.as_slice()), (Some(2), &[5][..]));
        assert_eq!(x.title.as_deref(), Some("Show"));
        assert_eq!(x.episode_title.as_deref(), Some("Pilot"));
        assert!(x.confidence >= 0.9);

        let film = apply(
            &g,
            &FileFix {
                kind: Some(FixKind::Movie),
                title: Some("  The Thing ".into()),
                year: Some(1982),
                ..FileFix::default()
            },
        );
        assert_eq!(film.kind, GuessKind::Movie);
        assert!(film.episodes.is_empty() && film.season.is_none());
        assert_eq!(
            (film.title.as_deref(), film.year),
            (Some("The Thing"), Some(1982))
        );

        let trailer = apply(
            &guess(GuessKind::Movie, "Interstellar Trailer"),
            &FileFix {
                extra: Some(ExtraKind::Trailer),
                ..FileFix::default()
            },
        );
        assert_eq!(
            (trailer.kind, trailer.extra),
            (GuessKind::Extra, Some(ExtraKind::Trailer))
        );

        let gone = apply(
            &g,
            &FileFix {
                kind: Some(FixKind::Ignore),
                ..FileFix::default()
            },
        );
        assert_eq!(gone.kind, GuessKind::Sample);
    }
}
