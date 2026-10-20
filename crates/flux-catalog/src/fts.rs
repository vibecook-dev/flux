//! Explicit FTS5 sync for `item_fts` (external content = `item`).
//!
//! Triggers were measured to be the top cost of an item insert: any trigger
//! makes the INSERT a "multi-write" statement, so SQLite opens and truncates a
//! statement journal per row, and `INSERT INTO vtab SELECT …` materialises an
//! ephemeral table. Syncing from the writer is cheaper and lets one Rust
//! predicate ([`indexable`]) decide what is searchable. Every write path that
//! inserts, renames or deletes items goes through this module; [`rebuild`]
//! repairs the index if one ever does not.
//!
//! External-content rule: a `'delete'` must carry exactly the values that
//! were indexed, so callers pass the *old* row to [`remove`].

use flux_core::api::Id;
use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;

/// "Episode 3": a placeholder title, not worth indexing.
pub(crate) fn is_placeholder_episode_title(t: &str) -> bool {
    t.strip_prefix("Episode ")
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
}

/// Movies, shows and episodes with a real title are searchable; seasons,
/// extras and placeholder episodes are not (they would flood prefix results).
pub(crate) fn indexable(kind: &str, title: &str) -> bool {
    kind == "movie" || kind == "show" || (kind == "episode" && !is_placeholder_episode_title(title))
}

/// The indexed columns of an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Doc {
    pub kind: String,
    pub title: String,
    pub original_title: Option<String>,
    pub overview: Option<String>,
}

impl Doc {
    fn indexable(&self) -> bool {
        indexable(&self.kind, &self.title)
    }
}

/// Read an item's indexed columns.
pub(crate) fn doc(conn: &Connection, id: Id) -> Result<Option<Doc>> {
    Ok(conn
        .prepare_cached("SELECT kind, title, original_title, overview FROM item WHERE id = ?1")?
        .query_row([id], |r| {
            Ok(Doc {
                kind: r.get(0)?,
                title: r.get(1)?,
                original_title: r.get(2)?,
                overview: r.get(3)?,
            })
        })
        .optional()?)
}

/// Index a new or renamed item (no-op if not indexable).
pub(crate) fn add(conn: &Connection, id: Id, d: &Doc) -> Result<()> {
    if d.indexable() {
        conn.prepare_cached(
            "INSERT INTO item_fts(rowid, title, original_title, overview) VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![id, d.title, d.original_title, d.overview])?;
    }
    Ok(())
}

/// Remove an item's entry; `old` must be the values that were indexed.
pub(crate) fn remove(conn: &Connection, id: Id, old: &Doc) -> Result<()> {
    if old.indexable() {
        conn.prepare_cached(
            "INSERT INTO item_fts(item_fts, rowid, title, original_title, overview) VALUES ('delete', ?1, ?2, ?3, ?4)",
        )?
        .execute(params![id, old.title, old.original_title, old.overview])?;
    }
    Ok(())
}

/// Re-index an item whose indexed columns may have changed from `old`.
pub(crate) fn update(conn: &Connection, id: Id, old: &Doc) -> Result<()> {
    let Some(new) = doc(conn, id)? else {
        return remove(conn, id, old);
    };
    if &new != old {
        remove(conn, id, old)?;
        add(conn, id, &new)?;
    }
    Ok(())
}

/// Drop and rebuild the whole index from `item` (repair, or after a bulk
/// delete such as removing a library).
pub(crate) fn rebuild(conn: &Connection) -> Result<u64> {
    conn.execute("INSERT INTO item_fts(item_fts) VALUES ('delete-all')", [])?;
    let mut stmt = conn.prepare("SELECT id, kind, title, original_title, overview FROM item")?;
    let mut rows = stmt.query([])?;
    let mut n = 0u64;
    while let Some(r) = rows.next()? {
        let id: Id = r.get(0)?;
        let d = Doc {
            kind: r.get(1)?,
            title: r.get(2)?,
            original_title: r.get(3)?,
            overview: r.get(4)?,
        };
        if d.indexable() {
            add(conn, id, &d)?;
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_titles_are_not_indexable() {
        assert!(is_placeholder_episode_title("Episode 3"));
        assert!(!is_placeholder_episode_title("Episode"));
        assert!(!is_placeholder_episode_title("Episode IV"));
        assert!(indexable("episode", "Ozymandias"));
        assert!(!indexable("episode", "Episode 12"));
        assert!(!indexable("season", "Season 1"));
        assert!(!indexable("extra", "Making Of"));
        assert!(indexable("movie", "Episode 1"));
    }

    #[test]
    fn add_update_remove_rebuild_keep_the_index_consistent() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Connection::open(dir.path().join("f.db")).unwrap();
        crate::schema::migrate(&mut c).unwrap();
        c.execute_batch(
            "INSERT INTO library(id,name,kind,created_at) VALUES (1,'L','mixed',0);
             INSERT INTO item(id,library_id,kind,group_key,title,sort_title,added_at,content_added_at,updated_at)
               VALUES (1,1,'movie','amelie','Amélie','amelie',0,0,0),
                      (2,1,'episode','e3','Episode 3','episode 3',0,0,0),
                      (3,1,'season','s1','Season 1','season 1',0,0,0);",
        )
        .unwrap();
        for id in 1..=3 {
            let d = doc(&c, id).unwrap().unwrap();
            add(&c, id, &d).unwrap();
        }
        let hits = |c: &Connection, q: &str| -> i64 {
            c.query_row(
                "SELECT count(*) FROM item_fts WHERE item_fts MATCH ?1",
                [q],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(hits(&c, "amel*"), 1, "diacritics are folded");
        assert_eq!(hits(&c, "episode"), 0);
        assert_eq!(hits(&c, "season"), 0);
        let old = doc(&c, 2).unwrap().unwrap();
        c.execute("UPDATE item SET title = 'Ozymandias' WHERE id = 2", [])
            .unwrap();
        update(&c, 2, &old).unwrap();
        assert_eq!(hits(&c, "ozym*"), 1);
        let old = doc(&c, 2).unwrap().unwrap();
        remove(&c, 2, &old).unwrap();
        c.execute("DELETE FROM item WHERE id = 2", []).unwrap();
        assert_eq!(hits(&c, "ozym*"), 0);
        c.execute_batch("INSERT INTO item_fts(item_fts) VALUES('integrity-check');")
            .unwrap();
        assert_eq!(rebuild(&c).unwrap(), 1);
        assert_eq!(hits(&c, "amel*"), 1);
        c.execute_batch("INSERT INTO item_fts(item_fts) VALUES('integrity-check');")
            .unwrap();
    }
}
