//! Lenses kept on the server (`lens.add`): one person's (`owner` = their
//! user id) or everyone's (`owner` 0). The spec is stored as the server
//! checked it ([`flux_core::agent::LensSpec`]); the browser sanitises it
//! again before it runs (docs/LENSES.md).

use flux_core::agent::{LensSpec, StoredLens};
use flux_core::api::Id;
use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{CatalogError, Result};

/// Most lenses one person (or everyone) keeps on the server.
pub const MAX_LENSES: u64 = 24;

/// `owner` of the lenses kept for everyone.
pub const EVERYONE: Id = 0;

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(StoredLens, String)> {
    Ok((
        StoredLens {
            id: r.get(0)?,
            everyone: r.get::<_, Id>(1)? == EVERYONE,
            added_by: r.get(2)?,
            agent: r.get(3)?,
            added_at: r.get(4)?,
            updated_at: r.get(5)?,
            spec: LensSpec {
                name: String::new(),
                blurb: String::new(),
                input: Default::default(),
                step: 160.0,
                window: 40,
                animated: false,
                css: String::new(),
                scene: String::new(),
                item: String::new(),
                layout: String::new(),
            },
        },
        r.get(6)?,
    ))
}

fn finish((mut l, spec): (StoredLens, String)) -> Option<StoredLens> {
    l.spec = serde_json::from_str(&spec).ok()?;
    Some(l)
}

/// `user`'s lenses, then everyone's (oldest first in each).
pub fn list(c: &Connection, user: Id) -> Result<Vec<StoredLens>> {
    let mut stmt = c.prepare_cached(
        "SELECT id, owner, added_by, agent, added_at, updated_at, spec FROM lens
          WHERE owner = ?1 OR owner = 0 ORDER BY owner = 0, added_at, id",
    )?;
    let rows = stmt.query_map([user], row)?;
    Ok(rows
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter_map(finish)
        .collect())
}

/// Keep `spec` as `id` for `owner` (replacing one with that id). Fails when
/// `owner` already keeps [`MAX_LENSES`] others.
pub fn put(
    c: &Connection,
    owner: Id,
    id: &str,
    spec: &LensSpec,
    added_by: &str,
    agent: Option<&str>,
    now: i64,
) -> Result<StoredLens> {
    let exists = c
        .prepare_cached("SELECT 1 FROM lens WHERE owner = ?1 AND id = ?2")?
        .query_row(params![owner, id], |_| Ok(()))
        .optional()?
        .is_some();
    if !exists {
        let n: i64 = c
            .prepare_cached("SELECT count(*) FROM lens WHERE owner = ?1")?
            .query_row([owner], |r| r.get(0))?;
        if n as u64 >= MAX_LENSES {
            return Err(CatalogError::Invalid(format!(
                "{} already keep {MAX_LENSES} lenses: remove one first",
                if owner == EVERYONE {
                    "Everyone's lenses"
                } else {
                    "You"
                }
            )));
        }
    }
    let json = serde_json::to_string(spec)?;
    c.prepare_cached(
        "INSERT INTO lens(owner, id, spec, added_by, agent, added_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
         ON CONFLICT(owner, id) DO UPDATE SET spec = excluded.spec, added_by = excluded.added_by,
            agent = excluded.agent, updated_at = excluded.updated_at",
    )?
    .execute(params![owner, id, json, added_by, agent, now])?;
    c.prepare_cached("SELECT id, owner, added_by, agent, added_at, updated_at, spec FROM lens WHERE owner = ?1 AND id = ?2")?
        .query_row(params![owner, id], row)
        .map(finish)?
        .ok_or_else(|| CatalogError::Corrupt(format!("lens {id}")))
}

/// Forget `owner`'s lens `id`. Returns whether there was one.
pub fn remove(c: &Connection, owner: Id, id: &str) -> Result<bool> {
    Ok(c.prepare_cached("DELETE FROM lens WHERE owner = ?1 AND id = ?2")?
        .execute(params![owner, id])?
        > 0)
}
