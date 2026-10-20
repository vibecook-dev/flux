//! Re-placing episode assets after the fact: anime absolute numbering mapped
//! to seasons by flux-meta (TMDB episode groups), or a user fixing numbers.
//!
//! The scanner files `[SubsPlease] Show - 027.mkv` as season 1, episode 27
//! (the parser knows no seasons). Once flux-meta knows that 27 is S02E01,
//! [`crate::Writer::rekey_episodes`]:
//!
//! 1. links the asset to the target episode items (created if needed),
//! 2. carries every user's state from the old episode items to the new ones
//!    (watched wins, the latest position wins, play counts don't add up),
//! 3. deletes old episode items (and seasons) left with nothing, and
//! 4. records `absolute → (season, episodes)` in the show's `episode_map`,
//!    so files that arrive later with that absolute number are placed right
//!    away by the scanner's apply.
//!
//! The result lists added/updated/removed items for `Event::ItemsChanged`.

use std::collections::{BTreeSet, HashMap};

use flux_core::api::{Event, Id};
use flux_core::NameGuess;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::error::{CatalogError, Result};
use crate::group::{self, EpisodeKey, Resolver};
use crate::scan_support::{recompute, show_of_asset, Touched};

/// Where an episode asset belongs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeRekey {
    pub asset_id: Id,
    pub season: u16,
    /// One or more episode numbers (a double episode links to both).
    pub episodes: Vec<u16>,
    /// The absolute number the file was parsed with. When set, the mapping
    /// is remembered for the show and applied to future files.
    pub absolute: Option<u32>,
}

/// What a re-key changed. [`RekeyOutcome::event`] gives the SSE event.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RekeyOutcome {
    /// Library of the re-keyed assets (`None` if nothing was re-keyed).
    pub library_id: Option<Id>,
    /// Assets whose episode links changed.
    pub assets_moved: u64,
    /// `user_state` rows written on target episodes.
    pub user_states_moved: u64,
    /// `episode_map` rows written.
    pub mappings: u64,
    pub items_added: Vec<Id>,
    pub items_updated: Vec<Id>,
    pub items_removed: Vec<Id>,
}

impl RekeyOutcome {
    /// `Event::ItemsChanged` for this change, if anything changed.
    pub fn event(&self) -> Option<Event> {
        let library_id = self.library_id?;
        if self.items_added.is_empty() && self.items_updated.is_empty() && self.items_removed.is_empty() {
            return None;
        }
        Some(Event::ItemsChanged {
            library_id,
            added: self.items_added.clone(),
            updated: self.items_updated.clone(),
            removed: self.items_removed.clone(),
        })
    }
}

/// An episode file of a show that was numbered by absolute episode only
/// (from its stored name guess), with where it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbsoluteEpisode {
    pub asset_id: Id,
    pub rel_path: String,
    pub absolute: u32,
    pub season: Option<u16>,
    pub episodes: Vec<u16>,
}

/// The show's mapping for an absolute number, if flux-meta provided one.
pub(crate) fn mapped_episode(c: &Connection, show_id: Id, absolute: u32) -> Result<Option<(u16, Vec<u16>)>> {
    let row: Option<(u16, String)> = c
        .prepare_cached("SELECT season, episodes FROM episode_map WHERE show_id = ?1 AND absolute = ?2")?
        .query_row(params![show_id, absolute], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    Ok(row.and_then(|(s, eps)| {
        let eps: Vec<u16> = serde_json::from_str(&eps).ok()?;
        (!eps.is_empty()).then_some((s, eps))
    }))
}

/// Every episode asset of `show_id` whose file was parsed with an absolute
/// number and no season/episode (what flux-meta needs to map), in absolute order.
pub fn absolute_episodes(c: &Connection, show_id: Id) -> Result<Vec<AbsoluteEpisode>> {
    let mut stmt = c.prepare_cached(
        "SELECT DISTINCT a.id, f.rel_path, m.guess FROM item s JOIN item e ON e.parent_id = s.id
           JOIN asset_item ai ON ai.item_id = e.id JOIN asset a ON a.id = ai.asset_id
           JOIN file f ON f.id = a.file_id JOIN file_meta m ON m.file_id = f.id
          WHERE s.parent_id = ?1 AND s.kind = 'season' AND e.kind = 'episode' AND f.state <> 'missing'",
    )?;
    let rows: Vec<(Id, String, Option<String>)> = stmt
        .query_map([show_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for (asset_id, rel_path, guess) in rows {
        let Some(g) = guess.and_then(|g| serde_json::from_str::<NameGuess>(&g).ok()) else {
            continue;
        };
        let Some(abs) = g.absolute_episode.filter(|_| g.episodes.is_empty()) else {
            continue;
        };
        let (season, episodes) = current_place(c, asset_id)?;
        out.push(AbsoluteEpisode {
            asset_id,
            rel_path,
            absolute: abs,
            season,
            episodes,
        });
    }
    out.sort_by_key(|e| (e.absolute, e.asset_id));
    Ok(out)
}

fn current_place(c: &Connection, asset_id: Id) -> Result<(Option<u16>, Vec<u16>)> {
    let mut stmt = c.prepare_cached(
        "SELECT e.season_number, e.episode_number FROM asset_item ai JOIN item e ON e.id = ai.item_id
         WHERE ai.asset_id = ?1 AND e.kind = 'episode' ORDER BY e.episode_number",
    )?;
    let rows: Vec<(Option<u16>, Option<u16>)> = stmt
        .query_map([asset_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let season = rows.first().and_then(|r| r.0);
    Ok((season, rows.iter().filter_map(|r| r.1).collect()))
}

/// Copy `from`'s per-user state onto `to`, merging with what `to` has:
/// watched wins, the more recent position wins, play counts take the max
/// (the same viewing must not count twice). Returns rows written.
pub(crate) fn merge_user_state(c: &Connection, from: Id, to: Id) -> Result<u64> {
    if from == to {
        return Ok(0);
    }
    Ok(c
        .prepare_cached(
            "INSERT INTO user_state(user_id, item_id, asset_id, position_ms, duration_ms, watched, play_count,
                                    last_played_at, updated_at)
             SELECT user_id, ?2, asset_id, position_ms, duration_ms, watched, play_count, last_played_at, updated_at
               FROM user_state WHERE item_id = ?1
             ON CONFLICT(user_id, item_id) DO UPDATE SET
                watched = MAX(watched, excluded.watched),
                play_count = MAX(play_count, excluded.play_count),
                asset_id = CASE WHEN excluded.updated_at > updated_at THEN excluded.asset_id ELSE asset_id END,
                position_ms = CASE WHEN excluded.updated_at > updated_at THEN excluded.position_ms ELSE position_ms END,
                duration_ms = CASE WHEN excluded.updated_at > updated_at THEN excluded.duration_ms ELSE duration_ms END,
                last_played_at = MAX(COALESCE(last_played_at, 0), COALESCE(excluded.last_played_at, 0)),
                updated_at = MAX(updated_at, excluded.updated_at)",
        )?
        .execute(params![from, to])? as u64)
}

/// Delete an item that lost its last asset (and has no children), and its
/// season if that is now empty. Returns the deleted ids and the nearest
/// surviving ancestor (whose state and badges need a recompute).
fn delete_if_empty(c: &Connection, id: Id) -> Result<(Vec<Id>, Option<Id>)> {
    let mut deleted = Vec::new();
    let mut cur = Some(id);
    while let Some(i) = cur {
        let row: Option<(Option<Id>, String)> = c
            .prepare_cached(
                "SELECT parent_id, kind FROM item it WHERE id = ?1
                   AND NOT EXISTS (SELECT 1 FROM asset_item WHERE item_id = it.id)
                   AND NOT EXISTS (SELECT 1 FROM asset WHERE item_id = it.id)
                   AND NOT EXISTS (SELECT 1 FROM item ch WHERE ch.parent_id = it.id)",
            )?
            .query_row([i], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let Some((parent, kind)) = row else { break };
        if !matches!(kind.as_str(), "episode" | "season") {
            break; // never delete a show here
        }
        if let Some(doc) = crate::fts::doc(c, i)? {
            crate::fts::remove(c, i, &doc)?;
        }
        c.prepare_cached("DELETE FROM item WHERE id = ?1")?.execute([i])?;
        deleted.push(i);
        cur = parent;
    }
    // `cur` is the first ancestor that was kept; `id` itself (still in use)
    // is recomputed as a leaf by the caller.
    let kept = if deleted.is_empty() { None } else { cur };
    Ok((deleted, kept))
}

/// Re-place episode assets (writer thread; one savepoint for the list).
pub(crate) fn rekey_episodes(
    c: &Connection,
    list: &[EpisodeRekey],
    now: i64,
    job_max_attempts: u32,
) -> Result<RekeyOutcome> {
    let mut out = RekeyOutcome::default();
    let mut resolvers: HashMap<Id, Resolver> = HashMap::new();
    let mut touched = Touched::default();
    let mut emptied: BTreeSet<Id> = BTreeSet::new();
    let mut created_meta = HashMap::new();
    for rk in list {
        let mut episodes = rk.episodes.clone();
        episodes.sort_unstable();
        episodes.dedup();
        if episodes.is_empty() {
            return Err(CatalogError::Invalid(format!(
                "asset {}: no episode numbers",
                rk.asset_id
            )));
        }
        let library_id: Option<Id> = c
            .prepare_cached(
                "SELECT i.library_id FROM asset a JOIN item i ON i.id = a.item_id WHERE a.id = ?1",
            )?
            .query_row([rk.asset_id], |r| r.get(0))
            .optional()?;
        let library_id =
            library_id.ok_or_else(|| CatalogError::NotFound(format!("asset {}", rk.asset_id)))?;
        let show_id = show_of_asset(c, rk.asset_id)?
            .ok_or_else(|| CatalogError::Invalid(format!("asset {} is not an episode", rk.asset_id)))?;
        out.library_id = Some(library_id);
        let r = resolvers.entry(library_id).or_insert_with(|| {
            let mut r = Resolver::new(library_id, now);
            r.job_max_attempts = job_max_attempts;
            r
        });
        if let Some(abs) = rk.absolute {
            c.prepare_cached(
                "INSERT INTO episode_map(show_id, absolute, season, episodes) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(show_id, absolute) DO UPDATE SET season = excluded.season, episodes = excluded.episodes
                 WHERE season IS NOT excluded.season OR episodes IS NOT excluded.episodes",
            )?
            .execute(params![show_id, abs, rk.season, serde_json::to_string(&episodes)?])?;
            out.mappings += 1;
        }
        let season_id = group::resolve_season(c, r, show_id, rk.season)?;
        let mut targets = Vec::with_capacity(episodes.len());
        for &e in &episodes {
            targets.push(group::resolve_episode(
                c,
                r,
                show_id,
                season_id,
                rk.season,
                &EpisodeKey::Number(e),
                None,
            )?);
        }
        let old: Vec<Id> = {
            let mut stmt =
                c.prepare_cached("SELECT item_id FROM asset_item WHERE asset_id = ?1 ORDER BY item_id")?;
            let rows = stmt.query_map([rk.asset_id], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut new = targets.clone();
        new.sort_unstable();
        if old == new {
            continue;
        }
        out.assets_moved += 1;
        for &o in old.iter().filter(|o| !new.contains(o)) {
            c.prepare_cached("DELETE FROM asset_item WHERE item_id = ?1 AND asset_id = ?2")?
                .execute(params![o, rk.asset_id])?;
            // Everyone's progress on the old number follows the file.
            for &t in &targets {
                out.user_states_moved += merge_user_state(c, o, t)?;
            }
            touched.leaves.insert(o);
            touched.linked.insert(o);
            emptied.insert(o);
        }
        for &t in new.iter().filter(|t| !old.contains(t)) {
            c.prepare_cached("INSERT INTO asset_item(item_id, asset_id) VALUES (?1, ?2)")?
                .execute(params![t, rk.asset_id])?;
        }
        c.prepare_cached("UPDATE asset SET item_id = ?2, updated_at = ?3 WHERE id = ?1")?
            .execute(params![rk.asset_id, targets[0], now])?;
        for &t in &targets {
            touched.leaves.insert(t);
            touched.linked.insert(t);
        }
    }
    // Old episodes with nothing left go away (their state was carried over).
    let mut removed: BTreeSet<Id> = BTreeSet::new();
    for o in emptied {
        let (gone, kept) = delete_if_empty(c, o)?;
        removed.extend(gone);
        touched.parents.extend(kept);
    }
    touched.parents.retain(|id| !removed.contains(id));
    touched.leaves.retain(|id| !removed.contains(id));
    touched.linked.retain(|id| !removed.contains(id));
    let mut added = Vec::new();
    for r in resolvers.into_values() {
        added.extend(r.created.iter().copied());
        created_meta.extend(r.created_meta);
    }
    let (updated, hidden) = recompute(c, now, &touched, &created_meta)?;
    out.items_added = added.into_iter().filter(|id| !removed.contains(id)).collect();
    out.items_updated = updated;
    out.items_removed = removed.into_iter().chain(hidden).collect();
    Ok(out)
}
