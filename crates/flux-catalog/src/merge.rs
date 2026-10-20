//! Merging two top-level works that turned out to be one: the show-folder
//! rule keeps `The Wire/` and `The Wire (2002)/` apart until provider ids
//! agree; once flux-meta matches both to the same TMDB id, the server calls
//! [`crate::Writer::merge_items`] (see [`works_sharing_ids`] to find them).
//!
//! Children merge by identity (`kind`, `group_key`): season 1 with season 1,
//! S01E03 with S01E03, an extra with the same-named extra; the rest moves
//! over. Assets, art, folder claims, absolute-episode maps and every user's
//! state follow (watched wins, the latest position wins).

use std::collections::BTreeSet;

use flux_core::api::Id;
use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{CatalogError, Result};
use crate::rekey::{merge_user_state, RekeyOutcome};
use crate::scan_support::{recompute, Touched};

/// Top-level works of the same kind and library as `item_id` that carry one
/// of its provider ids (tmdb, tvdb or imdb): candidates for a merge.
pub fn works_sharing_ids(c: &Connection, item_id: Id) -> Result<Vec<Id>> {
    let mut stmt = c.prepare_cached(
        "SELECT o.id FROM item i JOIN item o
            ON o.library_id = i.library_id AND o.kind = i.kind AND o.parent_id IS NULL AND o.id <> i.id
         WHERE i.id = ?1 AND i.parent_id IS NULL AND o.state <> 'missing'
           AND ((i.tmdb_id IS NOT NULL AND o.tmdb_id = i.tmdb_id)
             OR (i.tvdb_id IS NOT NULL AND o.tvdb_id = i.tvdb_id)
             OR (i.imdb_id IS NOT NULL AND o.imdb_id = i.imdb_id))
         ORDER BY o.id",
    )?;
    let rows = stmt.query_map([item_id], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

struct Merge<'c> {
    c: &'c Connection,
    now: i64,
    t: Touched,
    removed: BTreeSet<Id>,
    states: u64,
}

impl Merge<'_> {
    /// Move everything of `from` onto `into` (same kind), then delete `from`.
    fn item(&mut self, from: Id, into: Id) -> Result<()> {
        let c = self.c;
        // Children: merge same identities, move the rest.
        let kids: Vec<(Id, String, String)> = {
            let mut stmt = c.prepare_cached("SELECT id, kind, group_key FROM item WHERE parent_id = ?1")?;
            let rows = stmt.query_map([from], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for (kid, kind, key) in kids {
            let twin: Option<Id> = c
                .prepare_cached(
                    "SELECT id FROM item WHERE parent_id = ?1 AND kind = ?2 AND group_key = ?3 ORDER BY id LIMIT 1",
                )?
                .query_row(params![into, kind, key], |r| r.get(0))
                .optional()?;
            match twin {
                Some(t) => self.item(kid, t)?,
                None => {
                    c.prepare_cached("UPDATE item SET parent_id = ?2, updated_at = ?3 WHERE id = ?1")?
                        .execute(params![kid, into, self.now])?;
                    self.touch_subtree(kid)?;
                }
            }
        }
        // The item's own files and links.
        let linked: Vec<Id> = {
            let mut stmt = c.prepare_cached("SELECT asset_id FROM asset_item WHERE item_id = ?1")?;
            let rows = stmt.query_map([from], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for a in &linked {
            c.prepare_cached("INSERT OR IGNORE INTO asset_item(item_id, asset_id) VALUES (?1, ?2)")?
                .execute(params![into, a])?;
        }
        c.prepare_cached("UPDATE asset SET item_id = ?2, updated_at = ?3 WHERE item_id = ?1")?
            .execute(params![from, into, self.now])?;
        if !linked.is_empty() {
            self.t.leaves.insert(into);
            self.t.linked.insert(into);
        }
        self.states += merge_user_state(c, from, into)?;
        // Images, folders, absolute maps. A pin survives unless the target
        // has its own pin of that kind (the target's wins; at most one pin
        // per item and kind, as `pin_art` keeps it).
        c.prepare_cached(
            "UPDATE art SET item_id = ?2,
                    pinned = CASE WHEN pinned = 1 AND EXISTS
                                  (SELECT 1 FROM art t WHERE t.item_id = ?2 AND t.kind = art.kind AND t.pinned = 1)
                             THEN 0 ELSE pinned END
              WHERE item_id = ?1",
        )?
        .execute(params![from, into])?;
        c.prepare_cached("UPDATE OR IGNORE item_folder SET item_id = ?2 WHERE item_id = ?1")?
            .execute(params![from, into])?;
        c.prepare_cached("UPDATE OR IGNORE episode_map SET show_id = ?2 WHERE show_id = ?1")?
            .execute(params![from, into])?;
        // Fill ids the target lacks (never overwrite).
        c.prepare_cached(
            "UPDATE item SET year = COALESCE(year, (SELECT year FROM item WHERE id = ?1)),
                tmdb_id = COALESCE(tmdb_id, (SELECT tmdb_id FROM item WHERE id = ?1)),
                tvdb_id = COALESCE(tvdb_id, (SELECT tvdb_id FROM item WHERE id = ?1)),
                imdb_id = COALESCE(imdb_id, (SELECT imdb_id FROM item WHERE id = ?1))
             WHERE id = ?2",
        )?
        .execute(params![from, into])?;
        if let Some(doc) = crate::fts::doc(c, from)? {
            crate::fts::remove(c, from, &doc)?;
        }
        // Cascades the leftovers (its jobs, folder claims that collided, …).
        c.prepare_cached("DELETE FROM item WHERE id = ?1")?
            .execute([from])?;
        self.removed.insert(from);
        self.t.leaves.remove(&from);
        self.t.linked.remove(&from);
        self.t.parents.insert(into);
        Ok(())
    }

    /// A moved subtree: its leaves need a recompute under their new parent.
    fn touch_subtree(&mut self, id: Id) -> Result<()> {
        let mut stmt = self.c.prepare_cached(
            "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION ALL SELECT i.id FROM item i JOIN sub ON i.parent_id = sub.id)
             SELECT sub.id FROM sub JOIN item i ON i.id = sub.id WHERE i.kind IN ('movie','episode','extra')",
        )?;
        for leaf in stmt.query_map([id], |r| r.get::<_, Id>(0))? {
            let leaf = leaf?;
            self.t.leaves.insert(leaf);
            self.t.linked.insert(leaf);
        }
        Ok(())
    }
}

/// What [`carry_emptied`] changed, for the caller's recompute.
#[derive(Default)]
pub(crate) struct Carried {
    pub touched: Touched,
    /// Items deleted (merged into another).
    pub deleted: BTreeSet<Id>,
}

/// `from` lost its last file to `to`: a rename or re-parse changed the
/// file's parsed identity (a typo fixed in a folder name, a Sonarr/Radarr
/// rename to another title, a show folder renamed, a parser upgrade), or a
/// file moved to another library. Watch state, resume points, art pins,
/// provider ids and a locked match belong to the work, not to the path, so
/// they follow the file (scan apply, writer thread):
///
/// - Same library and kind (movie → movie, episode → episode, extra →
///   extra): `from` merges into `to` as in [`merge_items`] (children by
///   identity, every user's state merged with what `to` has, art with the
///   target's pin winning, ids `to` lacks filled in) and is deleted. A
///   `locked` match (a user decision) is carried when `to` has none; an
///   automatic match is not (the rename may be fixing it), `to`'s own match
///   job decides. When `from`'s top-level work (an episode's show, an
///   extra's movie) has no file left either, it follows into `to`'s.
/// - Another library or kind: every user's state is copied onto `to`;
///   `from` stays (hidden), as it would before.
pub(crate) fn carry_emptied(c: &Connection, from: Id, to: Id, now: i64) -> Result<Carried> {
    let mut out = Carried::default();
    if from == to {
        return Ok(out);
    }
    let (Some((lib_f, kind_f, _)), Some((lib_t, kind_t, _))) = (head(c, from)?, head(c, to)?) else {
        return Ok(out);
    };
    if lib_f != lib_t || kind_f != kind_t {
        merge_user_state(c, from, to)?;
        return Ok(out);
    }
    let top_f = top_of(c, from)?;
    let top_t = top_of(c, to)?;
    let parent_f = head(c, from)?.and_then(|h| h.2);
    let mut m = Merge {
        c,
        now,
        t: Touched::default(),
        removed: BTreeSet::new(),
        states: 0,
    };
    carry_locked_match(c, from, to, now)?;
    m.item(from, to)?;
    if let Some(p) = parent_f {
        m.t.parents.insert(p);
    }
    if top_f != from && top_f != top_t && !m.removed.contains(&top_f) && subtree_is_empty(c, top_f)? {
        if let (Some((_, kf, _)), Some((_, kt, _))) = (head(c, top_f)?, head(c, top_t)?) {
            if kf == kt {
                carry_locked_match(c, top_f, top_t, now)?;
                m.item(top_f, top_t)?;
                if kt == "movie" {
                    m.t.leaves.insert(top_t);
                    m.t.parents.remove(&top_t);
                }
            }
        }
    }
    out.touched = m.t;
    out.deleted = m.removed;
    Ok(out)
}

/// `(library_id, kind, parent_id)` of an item.
fn head(c: &Connection, id: Id) -> Result<Option<(Id, String, Option<Id>)>> {
    Ok(
        c.prepare_cached("SELECT library_id, kind, parent_id FROM item WHERE id = ?1")?
            .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?,
    )
}

/// The top-level work an item belongs to (itself if top-level).
fn top_of(c: &Connection, id: Id) -> Result<Id> {
    let mut cur = id;
    for _ in 0..4 {
        match head(c, cur)?.and_then(|h| h.2) {
            Some(p) => cur = p,
            None => break,
        }
    }
    Ok(cur)
}

/// No file is linked anywhere under `id` (itself, seasons, episodes, extras).
fn subtree_is_empty(c: &Connection, id: Id) -> Result<bool> {
    Ok(c
        .prepare_cached(
            "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION ALL SELECT i.id FROM item i JOIN sub ON i.parent_id = sub.id)
             SELECT 1 FROM sub JOIN asset_item ai ON ai.item_id = sub.id LIMIT 1",
        )?
        .query_row([id], |_| Ok(()))
        .optional()?
        .is_none())
}

/// A user-locked match on `from` becomes `to`'s when `to` has none: the
/// title, ids and provider metadata the user settled on.
fn carry_locked_match(c: &Connection, from: Id, to: Id, now: i64) -> Result<()> {
    let old = crate::fts::doc(c, to)?;
    let n = c
        .prepare_cached(
            "UPDATE item SET title = f.title, sort_title = f.sort_title, original_title = f.original_title,
                    year = f.year, overview = f.overview, genres = f.genres, tmdb_id = f.tmdb_id,
                    tvdb_id = f.tvdb_id, imdb_id = f.imdb_id, external_ids = f.external_ids,
                    match_confidence = f.match_confidence, match_evidence = f.match_evidence,
                    match_state = 'locked', updated_at = ?3
               FROM (SELECT * FROM item WHERE id = ?1) AS f
              WHERE item.id = ?2 AND f.match_state = 'locked' AND item.match_state NOT IN ('matched', 'locked')",
        )?
        .execute(params![from, to, now])?;
    if n > 0 {
        if let Some(old) = old {
            crate::fts::update(c, to, &old)?;
        }
    }
    Ok(())
}

/// Merge top-level work `from` into `into` (writer thread).
pub(crate) fn merge_items(c: &Connection, from: Id, into: Id, now: i64) -> Result<RekeyOutcome> {
    let head = |id: Id| -> Result<(Id, String, Option<Id>)> {
        c.prepare_cached("SELECT library_id, kind, parent_id FROM item WHERE id = ?1")?
            .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?
            .ok_or_else(|| CatalogError::NotFound(format!("item {id}")))
    };
    let (lib_a, kind_a, parent_a) = head(from)?;
    let (lib_b, kind_b, parent_b) = head(into)?;
    if from == into || lib_a != lib_b || kind_a != kind_b || parent_a.is_some() || parent_b.is_some() {
        return Err(CatalogError::Invalid(format!(
            "can only merge two top-level works of one kind in one library ({from} {kind_a} → {into} {kind_b})"
        )));
    }
    let mut m = Merge {
        c,
        now,
        t: Touched::default(),
        removed: BTreeSet::new(),
        states: 0,
    };
    m.item(from, into)?;
    // `into` as a container (show) is recomputed from its seasons; as a
    // leaf (movie) from its assets.
    if kind_b == "movie" {
        m.t.leaves.insert(into);
        m.t.parents.remove(&into);
    }
    let (mut updated, hidden) = recompute(c, now, &m.t, &Default::default())?;
    if !updated.contains(&into) {
        updated.push(into);
    }
    Ok(RekeyOutcome {
        library_id: Some(lib_b),
        assets_moved: 0,
        user_states_moved: m.states,
        mappings: 0,
        items_added: Vec::new(),
        items_updated: updated,
        items_removed: m.removed.into_iter().chain(hidden).collect(),
    })
}
