//! Organising files (docs/AGENT-NATIVE.md, "Organising files"): what naming
//! an item's files needs ([`work_files`]), a plan as a proposed changeset
//! ([`propose`]), and what moving files changes in the catalog ([`record`]).
//!
//! flux-server works out a plan and moves the files; the catalog keeps the
//! journal. Each step is a change row, run in order and undone in reverse:
//!
//! | field | before | after |
//! |---|---|---|
//! | `folder.new` | `null` | `{root, path}` |
//! | `folder.path` | `{root, path}` | `{root, path}` |
//! | `file.path` | `{root, path, what, file?, size?, mtime_ns?, guess?, key?}` | `{root, path, what, file?, guess?}` |
//! | `folder.gone` | `{root, path}` | `null` |
//!
//! `what` is `video` (a catalog file, with the size and mtime it had when
//! planned, its guess and its work's group key) or `companion` (a file
//! beside it). When a move is recorded the catalog follows the files: paths
//! (files, folders, subtitle and metadata files, a show's folders), each
//! moved video's guess (parsed from its new name), and its work's group key,
//! so the work keeps its id ([`record`]).

use std::collections::{BTreeMap, BTreeSet};

use flux_core::api::Id;
use flux_core::NameGuess;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::changes::{self, Actor, Committed};
use crate::error::{CatalogError, Result};

pub const NEW_FOLDER: &str = "folder.new";
pub const RENAME_FOLDER: &str = "folder.path";
pub const MOVE_FILE: &str = "file.path";
pub const REMOVE_FOLDER: &str = "folder.gone";

/// Whether a change row is a step of a file plan.
pub fn is_step(field: &str) -> bool {
    matches!(field, NEW_FOLDER | RENAME_FOLDER | MOVE_FILE | REMOVE_FOLDER)
}

// ---- what naming needs ------------------------------------------------------------------

/// A library folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub id: Id,
    pub path: String,
    pub available: bool,
}

/// What a video file is, for naming it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// A version of a film.
    Version {
        edition: Option<String>,
        part: Option<u8>,
        /// The video's height (2160, 1080…), to tell versions apart.
        height: Option<u32>,
    },
    /// One or more episodes.
    Episode {
        season: u16,
        episodes: Vec<u16>,
        /// The episodes' titles, `None` where it's still "Episode N".
        titles: Vec<Option<String>>,
        /// Providers whose titles these are.
        title_providers: Vec<Option<String>>,
        air_date: Option<String>,
    },
    /// An extra of the film or show.
    Extra { kind: Option<String>, title: String },
}

/// A video file of a work.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoFile {
    pub file: Id,
    /// The item it's the file of (a film, an episode, an extra).
    pub item: Id,
    pub root: Id,
    pub rel_path: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub present: bool,
    pub role: Role,
    pub guess: Option<NameGuess>,
}

/// A film or show and its files, as naming needs them.
#[derive(Debug, Clone, PartialEq)]
pub struct Work {
    pub id: Id,
    /// `movie` or `show`.
    pub kind: String,
    pub library: Id,
    pub library_kind: String,
    pub group_key: String,
    pub title: String,
    /// The provider of the title, if a provider's (`tmdb`).
    pub title_provider: Option<String>,
    pub year: Option<u16>,
    pub tmdb: Option<i64>,
    pub imdb: Option<String>,
    pub tvdb: Option<i64>,
    pub files: Vec<VideoFile>,
}

fn title_provider(c: &Connection, item: Id) -> Result<Option<String>> {
    Ok(crate::fields::source_of(c, item, "title")?
        .filter(|s| s.source == crate::fields::PROVIDER || s.source == crate::fields::SIDECAR)
        .and_then(|s| s.provider))
}

/// The work of `item` (itself for a film or show) and the video files of
/// `item` (all of a work's: versions, episodes, extras). `None` if there is
/// no such item.
pub fn work_files(c: &Connection, item: Id) -> Result<Option<Work>> {
    let mut top = item;
    for _ in 0..4 {
        let parent: Option<Id> = c
            .prepare_cached("SELECT parent_id FROM item WHERE id = ?1")?
            .query_row([top], |r| r.get(0))
            .optional()?
            .flatten();
        match parent {
            Some(p) => top = p,
            None => break,
        }
    }
    type Head = (
        String,
        Id,
        String,
        String,
        String,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<i64>,
    );
    let head: Option<Head> = c
        .prepare_cached(
            "SELECT i.kind, i.library_id, l.kind, i.group_key, i.title, i.year, i.tmdb_id, i.imdb_id, i.tvdb_id
               FROM item i JOIN library l ON l.id = i.library_id WHERE i.id = ?1",
        )?
        .query_row([top], |r| {
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
            ))
        })
        .optional()?;
    let Some((kind, library, library_kind, group_key, title, year, tmdb, imdb, tvdb)) = head else {
        return Ok(None);
    };
    let mut w = Work {
        id: top,
        kind,
        library,
        library_kind,
        group_key,
        title_provider: title_provider(c, top)?,
        title,
        year: year.and_then(|y| u16::try_from(y).ok()),
        tmdb,
        imdb,
        tvdb,
        files: Vec::new(),
    };
    // Every leaf at or below `item`, and its files.
    let mut stmt = c.prepare_cached(
        "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION ALL SELECT i.id FROM item i JOIN sub ON i.parent_id = sub.id)
         SELECT DISTINCT a.file_id FROM sub
           JOIN item i ON i.id = sub.id AND i.kind IN ('movie', 'episode', 'extra')
           JOIN asset_item ai ON ai.item_id = i.id JOIN asset a ON a.id = ai.asset_id
         ORDER BY 1",
    )?;
    let files: Vec<Id> = stmt
        .query_map([item], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for file in files {
        if let Some(v) = video_file(c, file)? {
            w.files.push(v);
        }
    }
    Ok(Some(w))
}

fn video_file(c: &Connection, file: Id) -> Result<Option<VideoFile>> {
    type Row = (
        Id,
        Id,
        String,
        i64,
        i64,
        String,
        Option<String>,
        Option<u8>,
        Option<i64>,
        Option<String>,
    );
    let row: Option<Row> = c
        .prepare_cached(
            "SELECT a.item_id, f.root_id, f.rel_path, f.size, f.mtime_ns, f.state, a.edition, a.part, a.height, m.guess
               FROM file f JOIN asset a ON a.id = f.asset_id LEFT JOIN file_meta m ON m.file_id = f.id
              WHERE f.id = ?1",
        )?
        .query_row([file], |r| {
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
            ))
        })
        .optional()?;
    let Some((item, root, rel_path, size, mtime_ns, state, edition, part, height, guess)) = row else {
        return Ok(None);
    };
    let (kind, extra_kind, item_title): (String, Option<String>, String) = c
        .prepare_cached("SELECT kind, extra_kind, title FROM item WHERE id = ?1")?
        .query_row([item], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    let role = match kind.as_str() {
        "episode" => {
            // The file's episodes, in order (a double episode is one file).
            let mut stmt = c.prepare_cached(
                "SELECT ep.id, se.season_number, ep.episode_number, ep.title, ep.air_date FROM asset_item ai
                   JOIN item ep ON ep.id = ai.item_id AND ep.kind = 'episode'
                   JOIN item se ON se.id = ep.parent_id
                  WHERE ai.asset_id = (SELECT asset_id FROM file WHERE id = ?1)
                  ORDER BY se.season_number, ep.episode_number, ep.air_date",
            )?;
            type Ep = (Id, Option<i64>, Option<i64>, String, Option<String>);
            let eps: Vec<Ep> = stmt
                .query_map([file], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })?
                .collect::<rusqlite::Result<_>>()?;
            let season = eps
                .first()
                .and_then(|e| e.1)
                .and_then(|s| u16::try_from(s).ok())
                .unwrap_or(1);
            let mut titles = Vec::new();
            let mut providers = Vec::new();
            let mut numbers = Vec::new();
            for (id, _, n, t, _) in &eps {
                if let Some(n) = n.and_then(|n| u16::try_from(n).ok()) {
                    numbers.push(n);
                    let placeholder = t == &format!("Episode {n}");
                    titles.push((!placeholder && !t.trim().is_empty()).then(|| t.clone()));
                } else {
                    titles.push(Some(t.clone()).filter(|t| !t.trim().is_empty()));
                }
                providers.push(title_provider(c, *id)?);
            }
            Role::Episode {
                season,
                episodes: numbers,
                titles,
                title_providers: providers,
                air_date: eps.first().and_then(|e| e.4.clone()),
            }
        }
        "extra" => Role::Extra {
            kind: extra_kind,
            title: item_title,
        },
        _ => Role::Version {
            edition,
            part,
            height: height.and_then(|h| u32::try_from(h).ok()),
        },
    };
    Ok(Some(VideoFile {
        file,
        item,
        root,
        rel_path,
        size: size.max(0) as u64,
        mtime_ns,
        // On a drive that answers (a file flux couldn't probe is there too).
        present: state == "present" || state == "probe_failed",
        role,
        guess: guess.and_then(|g| serde_json::from_str(&g).ok()),
    }))
}

/// A library folder by id.
pub fn root(c: &Connection, id: Id) -> Result<Option<Root>> {
    Ok(
        c.prepare_cached("SELECT id, path, state FROM root WHERE id = ?1")?
            .query_row([id], |r| {
                Ok(Root {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    available: r.get::<_, String>(2)? == "available",
                })
            })
            .optional()?,
    )
}

/// The top-level works (other than `work`) with a video file in `dir` of a
/// root or below it: a folder is a work's own when this is empty.
pub fn other_works_under(c: &Connection, root: Id, dir: &str, work: Id) -> Result<Vec<Id>> {
    let (lo, hi) = (format!("{dir}/"), format!("{dir}0"));
    let mut stmt = c.prepare_cached(
        "SELECT DISTINCT a.item_id FROM file f JOIN asset a ON a.id = f.asset_id
          WHERE f.root_id = ?1 AND f.rel_path >= ?2 AND f.rel_path < ?3 AND f.state <> 'missing'",
    )?;
    let items: Vec<Id> = stmt
        .query_map(params![root, lo, hi], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut tops = BTreeSet::new();
    for i in items {
        let mut top = i;
        for _ in 0..4 {
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
        if top != work {
            tops.insert(top);
        }
    }
    Ok(tops.into_iter().collect())
}

/// Whether the catalog has a video at `rel_path` of a root.
pub fn has_file(c: &Connection, root: Id, rel_path: &str) -> Result<bool> {
    Ok(
        c.prepare_cached("SELECT EXISTS (SELECT 1 FROM file WHERE root_id = ?1 AND rel_path = ?2)")?
            .query_row(params![root, rel_path], |r| r.get(0))?,
    )
}

// ---- the plan --------------------------------------------------------------------------

/// One step of a plan, as the server works it out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    /// [`NEW_FOLDER`], [`RENAME_FOLDER`], [`MOVE_FILE`] or [`REMOVE_FOLDER`].
    pub field: String,
    /// The item it's for (the work for folders).
    pub item: Id,
    pub root: Id,
    pub from: Option<String>,
    pub to: Option<String>,
    /// A video's file id (with the size and mtime it has now); `None` for
    /// a companion or a folder.
    pub file: Option<Id>,
    pub size: Option<u64>,
    pub mtime_ns: Option<i64>,
}

impl Step {
    fn what(&self) -> &'static str {
        if self.file.is_some() {
            "video"
        } else {
            "companion"
        }
    }

    fn before(&self) -> Option<Value> {
        let from = self.from.as_ref()?;
        Some(match self.field.as_str() {
            MOVE_FILE => json!({
                "root": self.root, "path": from, "what": self.what(),
                "file": self.file, "size": self.size, "mtime_ns": self.mtime_ns,
            }),
            _ => json!({ "root": self.root, "path": from }),
        })
    }

    fn after(&self) -> Option<Value> {
        let to = self.to.as_ref()?;
        Some(match self.field.as_str() {
            MOVE_FILE => json!({ "root": self.root, "path": to, "what": self.what(), "file": self.file }),
            _ => json!({ "root": self.root, "path": to }),
        })
    }
}

/// Keep `steps` as a proposed changeset (a plan nothing has done yet).
pub fn propose(c: &Connection, actor: &Actor, summary: &str, steps: &[Step], now: i64) -> Result<Committed> {
    if steps.is_empty() {
        return Ok(Committed::default());
    }
    if steps.len() > changes::MAX_CHANGES {
        return Err(CatalogError::Invalid(format!(
            "one plan holds at most {} steps; plan fewer items at once",
            changes::MAX_CHANGES
        )));
    }
    c.prepare_cached(
        "INSERT INTO changeset(actor, agent, summary, state, created_at) VALUES (?1, ?2, ?3, 'proposed', ?4)",
    )?
    .execute(params![actor.login, actor.agent, summary, now])?;
    let cs = c.last_insert_rowid();
    for (i, s) in steps.iter().enumerate() {
        c.prepare_cached(
            "INSERT INTO change(changeset_id, seq, item_id, field, op, before, after, state)
             VALUES (?1, ?2, ?3, ?4, 'set', ?5, ?6, 'pending')",
        )?
        .execute(params![
            cs,
            i as i64 + 1,
            s.item,
            s.field,
            s.before().map(|v| v.to_string()),
            s.after().map(|v| v.to_string()),
        ])?;
    }
    Ok(Committed {
        changeset: changes::summary_of(c, cs)?,
        ..Committed::default()
    })
}

/// A step of a kept plan.
#[derive(Debug, Clone, PartialEq)]
pub struct StepRow {
    pub seq: i64,
    pub item: Id,
    pub field: String,
    pub root: Id,
    pub from: Option<String>,
    pub to: Option<String>,
    pub file: Option<Id>,
    pub size: Option<u64>,
    pub mtime_ns: Option<i64>,
    /// The guess and group key a video had before it moved (an undo puts
    /// them back).
    pub guess_before: Option<NameGuess>,
    pub key_before: Option<String>,
    pub state: String,
}

/// The steps of changeset `cs` (in order), its state and whether it is a
/// file plan at all.
pub fn steps(c: &Connection, cs: Id) -> Result<Option<(String, Vec<StepRow>)>> {
    let state: Option<String> = c
        .prepare_cached("SELECT state FROM changeset WHERE id = ?1")?
        .query_row([cs], |r| r.get(0))
        .optional()?;
    let Some(state) = state else { return Ok(None) };
    let mut stmt = c.prepare_cached(
        "SELECT seq, item_id, field, before, after, state FROM change WHERE changeset_id = ?1 ORDER BY seq",
    )?;
    type Row = (i64, Id, String, Option<String>, Option<String>, String);
    let rows: Vec<Row> = stmt
        .query_map([cs], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (seq, item, field, before, after, st) in rows {
        if !is_step(&field) {
            continue;
        }
        let parse = |s: Option<String>| s.and_then(|s| serde_json::from_str::<Value>(&s).ok());
        let (b, a) = (parse(before), parse(after));
        let get = |v: &Option<Value>, k: &str| v.as_ref().and_then(|v| v.get(k)).cloned();
        let root = get(&b, "root")
            .or_else(|| get(&a, "root"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        out.push(StepRow {
            seq,
            item,
            field,
            root,
            from: get(&b, "path").and_then(|v| v.as_str().map(str::to_string)),
            to: get(&a, "path").and_then(|v| v.as_str().map(str::to_string)),
            file: get(&b, "file")
                .or_else(|| get(&a, "file"))
                .and_then(|v| v.as_i64()),
            size: get(&b, "size").and_then(|v| v.as_u64()),
            mtime_ns: get(&b, "mtime_ns").and_then(|v| v.as_i64()),
            guess_before: get(&b, "guess").and_then(|v| serde_json::from_value(v).ok()),
            key_before: get(&b, "key").and_then(|v| v.as_str().map(str::to_string)),
            state: st,
        });
    }
    Ok(Some((state, out)))
}

/// Whether changeset `cs` is a file plan.
pub fn is_plan(c: &Connection, cs: Id) -> Result<bool> {
    Ok(c.prepare_cached(
        "SELECT EXISTS (SELECT 1 FROM change WHERE changeset_id = ?1
                         AND field IN ('folder.new', 'folder.path', 'file.path', 'folder.gone'))",
    )?
    .query_row([cs], |r| r.get(0))?)
}

// ---- what moving files changes ------------------------------------------------------------

/// What happened to one step when the server ran it (or undid it).
#[derive(Debug, Clone, PartialEq)]
pub struct Done {
    pub seq: i64,
    /// `None`: done. `Some(reason)`: left as it was.
    pub failed: Option<String>,
    /// A moved video's guess for its new name (the server parses it).
    pub guess: Option<NameGuess>,
}

/// `old` (a path or folder) moved to `new`: `path` under it, rebased.
fn rebase(path: &str, old: &str, new: &str) -> Option<String> {
    if path == old {
        Some(new.to_string())
    } else {
        path.strip_prefix(old)
            .and_then(|rest| rest.strip_prefix('/'))
            .map(|rest| format!("{new}/{rest}"))
    }
}

/// A folder renamed: everything the catalog keeps under it follows.
fn move_folder(c: &Connection, root: Id, old: &str, new: &str) -> Result<()> {
    let (lo, hi) = (format!("{old}/"), format!("{old}0"));
    let files: Vec<(Id, String)> = {
        let mut stmt = c.prepare_cached(
            "SELECT id, rel_path FROM file WHERE root_id = ?1 AND rel_path >= ?2 AND rel_path < ?3",
        )?;
        let rows = stmt.query_map(params![root, lo, hi], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (id, rel) in files {
        if let Some(p) = rebase(&rel, old, new) {
            c.prepare_cached("DELETE FROM file WHERE root_id = ?1 AND rel_path = ?2 AND id <> ?3")?
                .execute(params![root, p, id])?;
            c.prepare_cached("UPDATE file SET rel_path = ?2 WHERE id = ?1")?
                .execute(params![id, p])?;
        }
    }
    let dirs: Vec<String> = {
        let mut stmt = c.prepare_cached(
            "SELECT rel_path FROM dir WHERE root_id = ?1 AND (rel_path = ?2 OR (rel_path >= ?3 AND rel_path < ?4))",
        )?;
        let rows = stmt.query_map(params![root, old, lo, hi], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for d in dirs {
        if let Some(p) = rebase(&d, old, new) {
            c.prepare_cached("UPDATE OR REPLACE dir SET rel_path = ?3 WHERE root_id = ?1 AND rel_path = ?2")?
                .execute(params![root, d, p])?;
        }
    }
    for table in ["subtitle_file", "meta_file"] {
        let rows: Vec<String> = {
            let mut stmt = c.prepare_cached(&format!(
                "SELECT rel_path FROM {table} WHERE root_id = ?1 AND rel_path >= ?2 AND rel_path < ?3"
            ))?;
            let rows = stmt.query_map(params![root, lo, hi], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for rel in rows {
            if let Some(p) = rebase(&rel, old, new) {
                let dir = p.rsplit_once('/').map_or("", |(d, _)| d).to_string();
                c.prepare_cached(&format!(
                    "UPDATE OR REPLACE {table} SET rel_path = ?3, dir = ?4 WHERE root_id = ?1 AND rel_path = ?2"
                ))?
                .execute(params![root, rel, p, dir])?;
            }
        }
    }
    // A show's folders are kept lowercased (`group::show_folder`).
    let (old_l, new_l) = (old.to_lowercase(), new.to_lowercase());
    let claims: Vec<(String, Id)> = {
        let mut stmt = c.prepare_cached(
            "SELECT folder, item_id FROM item_folder WHERE folder = ?1 OR (folder >= ?2 AND folder < ?3)",
        )?;
        let rows = stmt.query_map(params![old_l, format!("{old_l}/"), format!("{old_l}0")], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (folder, item) in claims {
        if let Some(p) = rebase(&folder, &old_l, &new_l) {
            c.prepare_cached(
                "UPDATE OR IGNORE item_folder SET folder = ?3 WHERE folder = ?1 AND item_id = ?2",
            )?
            .execute(params![folder, item, p])?;
        }
    }
    Ok(())
}

/// A file beside a video moved: the subtitle or metadata file the catalog
/// keeps for it follows.
fn move_companion(c: &Connection, root: Id, old: &str, new: &str) -> Result<()> {
    let dir = new.rsplit_once('/').map_or("", |(d, _)| d);
    for table in ["subtitle_file", "meta_file"] {
        c.prepare_cached(&format!(
            "UPDATE OR REPLACE {table} SET rel_path = ?3, dir = ?4 WHERE root_id = ?1 AND rel_path = ?2"
        ))?
        .execute(params![root, old, new, dir])?;
    }
    Ok(())
}

/// The top-level work a file's video belongs to.
fn work_of_file(c: &Connection, file: Id) -> Result<Option<Id>> {
    let item: Option<Id> = c
        .prepare_cached("SELECT a.item_id FROM file f JOIN asset a ON a.id = f.asset_id WHERE f.id = ?1")?
        .query_row([file], |r| r.get(0))
        .optional()?;
    let Some(mut top) = item else { return Ok(None) };
    for _ in 0..4 {
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
    Ok(Some(top))
}

/// A work whose files now carry another name answers to the key that name
/// gives, unless another work of its kind already does: so placing the
/// files by their new names finds the same work, and it keeps its id.
fn rekey_work(c: &Connection, work: Id, key: &str) -> Result<()> {
    let taken: bool = c
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM item o JOIN item w ON w.id = ?1
                             WHERE o.library_id = w.library_id AND o.kind = w.kind AND o.parent_id IS NULL
                               AND o.group_key = ?2 AND o.id <> w.id)",
        )?
        .query_row(params![work, key], |r| r.get(0))?;
    if !taken {
        c.prepare_cached("UPDATE item SET group_key = ?2 WHERE id = ?1 AND parent_id IS NULL")?
            .execute(params![work, key])?;
    }
    Ok(())
}

/// The group key a guess gives a top-level work (`None` for an extra, a
/// sample, a video of a videos library).
fn key_of(
    guess: &NameGuess,
    rel_path: &str,
    library_kind: flux_core::LibraryKind,
    root: Id,
) -> Option<String> {
    use crate::group::{place_in, Placement};
    match place_in(library_kind, root, guess, rel_path) {
        Placement::Movie { work, .. } if !work.standalone => Some(work.group_key),
        Placement::Episodes { show, .. } => Some(show.group_key),
        _ => None,
    }
}

/// Record what the server did to plan `cs`: `done` (in the order run), and
/// with `undo`, what it put back. Paths, guesses and group keys follow,
/// moved videos are placed again, and each step and the changeset take
/// their new state. `by` decided it.
pub fn record(
    c: &Connection,
    cs: Id,
    by: &str,
    done: &[Done],
    undo: bool,
    now: i64,
    jobs: u32,
) -> Result<Committed> {
    let Some((state, rows)) = steps(c, cs)? else {
        return Err(CatalogError::NotFound(format!("changeset {cs}")));
    };
    let want = if undo { "applied" } else { "proposed" };
    if state != want {
        return Err(CatalogError::Invalid(format!(
            "changeset {cs} is {state}, not {want}"
        )));
    }
    let by_seq: BTreeMap<i64, &StepRow> = rows.iter().map(|r| (r.seq, r)).collect();
    let mut out = Committed::default();
    // Moved videos, with the guess each was placed by before (its fix applied).
    let mut moved: Vec<(Id, NameGuess)> = Vec::new();
    for d in done {
        let Some(row) = by_seq.get(&d.seq) else { continue };
        if let Some(why) = &d.failed {
            let st = if undo { "kept" } else { "skipped" };
            c.prepare_cached("UPDATE change SET state = ?3 WHERE changeset_id = ?1 AND seq = ?2")?
                .execute(params![cs, d.seq, st])?;
            out.not_done.push((row.item, row.field.clone(), why.clone()));
            continue;
        }
        // Where it was and where it is now.
        let (old, new) = if undo {
            (row.to.clone(), row.from.clone())
        } else {
            (row.from.clone(), row.to.clone())
        };
        match row.field.as_str() {
            RENAME_FOLDER => {
                if let (Some(o), Some(n)) = (&old, &new) {
                    move_folder(c, row.root, o, n)?;
                }
            }
            MOVE_FILE => {
                let (Some(o), Some(n)) = (&old, &new) else {
                    continue;
                };
                match row.file {
                    Some(file) => {
                        // The video keeps its id, asset and everyone's state.
                        let lib_kind: String = c
                            .prepare_cached(
                                "SELECT l.kind FROM root r JOIN library l ON l.id = r.library_id WHERE r.id = ?1",
                            )?
                            .query_row([row.root], |r| r.get(0))?;
                        let lib_kind = crate::model::parse_library_kind(&lib_kind)?;
                        let stored: Option<String> = c
                            .prepare_cached("SELECT guess FROM file_meta WHERE file_id = ?1")?
                            .query_row([file], |r| r.get(0))
                            .optional()?
                            .flatten();
                        let guess_now: Option<NameGuess> = stored.and_then(|g| serde_json::from_str(&g).ok());
                        let guess = if undo {
                            row.guess_before.clone().or(guess_now.clone())
                        } else {
                            d.guess.clone().or(guess_now.clone())
                        };
                        if !undo {
                            // Keep what an undo needs: the guess before, and
                            // the work's key.
                            let key: Option<String> = match work_of_file(c, file)? {
                                Some(w) => c
                                    .prepare_cached("SELECT group_key FROM item WHERE id = ?1")?
                                    .query_row([w], |r| r.get(0))
                                    .optional()?,
                                None => None,
                            };
                            c.prepare_cached(
                                "UPDATE change SET before = json_set(before, '$.guess', json(?3), '$.key', ?4)
                                  WHERE changeset_id = ?1 AND seq = ?2",
                            )?
                            .execute(params![
                                cs,
                                d.seq,
                                guess_now.as_ref().map(serde_json::to_string).transpose()?,
                                key
                            ])?;
                        }
                        // A row the catalog still keeps at the new path is stale:
                        // the file wasn't there (nothing is ever replaced).
                        c.prepare_cached(
                            "DELETE FROM file WHERE root_id = ?1 AND rel_path = ?2 AND id <> ?3",
                        )?
                        .execute(params![row.root, n, file])?;
                        // There now, whatever a scan that ran meanwhile thought.
                        c.prepare_cached(
                            "UPDATE file SET rel_path = ?2, updated_at = ?3, tombstone_until = NULL,
                                    state = CASE WHEN state = 'missing' THEN 'present' ELSE state END
                              WHERE id = ?1",
                        )?
                        .execute(params![file, n, now])?;
                        if let Some(g) = &guess {
                            c.prepare_cached(
                                "INSERT INTO file_meta(file_id, guess) VALUES (?1, ?2)
                                 ON CONFLICT(file_id) DO UPDATE SET guess = excluded.guess",
                            )?
                            .execute(params![file, serde_json::to_string(g)?])?;
                            let fixed = crate::fixes::fixed(c, file, g.clone())?;
                            let key = if undo {
                                row.key_before.clone()
                            } else {
                                key_of(&fixed, n, lib_kind, row.root)
                            };
                            if let (Some(k), Some(w)) = (key, work_of_file(c, file)?) {
                                rekey_work(c, w, &k)?;
                            }
                        }
                        let _ = o;
                        let was = guess_now.unwrap_or_else(NameGuess::unknown);
                        moved.push((file, crate::fixes::fixed(c, file, was)?));
                    }
                    None => move_companion(c, row.root, o, n)?,
                }
            }
            // Folders made or removed: the next scan records them.
            _ => {}
        }
        let st = if undo { "reverted" } else { "applied" };
        c.prepare_cached("UPDATE change SET state = ?3 WHERE changeset_id = ?1 AND seq = ?2")?
            .execute(params![cs, d.seq, st])?;
        out.done += 1;
        if let Ok(lib) = c
            .prepare_cached("SELECT library_id FROM item WHERE id = ?1")?
            .query_row([row.item], |r| r.get::<_, Id>(0))
        {
            if !out.items.contains(&(row.item, lib)) {
                out.items.push((row.item, lib));
            }
        }
    }
    if !moved.is_empty() {
        // Placed again by their new names (their fixes as they are).
        let placed = crate::scan_support::place_again(c, &moved, jobs)?;
        out.absorb(placed);
    }
    if undo {
        c.prepare_cached(
            "UPDATE changeset SET state = 'reverted', reverted_at = ?2, reverted_by = ?3 WHERE id = ?1",
        )?
        .execute(params![cs, now, by])?;
    } else {
        c.prepare_cached(
            "UPDATE changeset SET state = 'applied', decided_at = ?2, decided_by = ?3 WHERE id = ?1",
        )?
        .execute(params![cs, now, by])?;
    }
    out.changeset = changes::summary_of(c, cs)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_under_a_folder_follow_it() {
        assert_eq!(rebase("A/x.mkv", "A", "B").as_deref(), Some("B/x.mkv"));
        assert_eq!(rebase("A", "A", "B").as_deref(), Some("B"));
        assert_eq!(rebase("AB/x.mkv", "A", "B"), None);
        assert_eq!(
            rebase("A/sub/x.mkv", "A", "C/D").as_deref(),
            Some("C/D/sub/x.mkv")
        );
    }
}
