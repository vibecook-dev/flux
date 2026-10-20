//! Ground-truth placement checks against the corpus manifest, shared by the
//! integration tests and `flux-scan-bench` (included there with `#[path]`).
//! Taken from `examples/pipeline_smoke.rs`.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::error::Error;

use flux_catalog::group::normalize_title;
use flux_catalog::synth::ManifestEntry;
use flux_catalog::{scan_support, Catalog};
use serde_json::{json, Value};

pub type BoxResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// One linked item of an asset, with its parent and grandparent.
#[derive(Debug, Clone)]
struct Linked {
    kind: String,
    title: String,
    year: Option<u16>,
    episode: Option<u16>,
    parent_title: Option<String>,
    parent_season: Option<u16>,
    grandparent_title: Option<String>,
}

/// Compares where every manifest file landed with the ground truth.
pub fn compare(
    cat: &Catalog,
    library_id: i64,
    root_id: i64,
    folder: &str,
    truth: &[&ManifestEntry],
) -> BoxResult<Value> {
    let snapshot = cat.read_blocking(move |c| scan_support::load_root_snapshot(c, root_id))?;
    let links: HashMap<i64, Vec<Linked>> = cat.read_blocking(|c| {
        let mut st = c.prepare(
            "SELECT ai.asset_id, i.kind, i.title, i.year, i.episode_number,
                    p.title, p.season_number, g.title
             FROM asset_item ai JOIN item i ON i.id = ai.item_id
             LEFT JOIN item p ON p.id = i.parent_id
             LEFT JOIN item g ON g.id = p.parent_id",
        )?;
        let mut map: HashMap<i64, Vec<Linked>> = HashMap::new();
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                Linked {
                    kind: r.get(1)?,
                    title: r.get(2)?,
                    year: r.get(3)?,
                    episode: r.get(4)?,
                    parent_title: r.get(5)?,
                    parent_season: r.get(6)?,
                    grandparent_title: r.get(7)?,
                },
            ))
        })?;
        for row in rows {
            let (asset, l) = row?;
            map.entry(asset).or_default().push(l);
        }
        Ok(map)
    })?;
    let counts: BTreeMap<String, i64> = cat.read_blocking(move |c| {
        let mut st = c.prepare(
            "SELECT kind || CASE WHEN match_state = 'unmatched' THEN ':unmatched' ELSE '' END, count(*)
             FROM item WHERE library_id = ?1 GROUP BY 1",
        )?;
        let rows = st.query_map([library_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut m = BTreeMap::new();
        for row in rows {
            let (k, n) = row?;
            m.insert(k, n);
        }
        Ok(m)
    })?;

    let norm = |s: Option<&str>| normalize_title(s.unwrap_or(""));
    let mut ok: BTreeMap<&str, u64> = BTreeMap::new();
    let mut wrong: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut fail = |class: String, e: &ManifestEntry, got: String| {
        wrong
            .entry(class)
            .or_default()
            .push(format!("{} → {got}", e.path));
    };
    for e in truth.iter().filter(|e| e.is_media()) {
        let rel = e.path.strip_prefix(&format!("{folder}/")).unwrap_or(&e.path);
        let Some(row) = snapshot.get(rel) else {
            fail("not walked".into(), e, "absent from catalog".into());
            continue;
        };
        let linked = row
            .asset_id
            .and_then(|a| links.get(&a))
            .cloned()
            .unwrap_or_default();
        let summary = || {
            linked
                .iter()
                .map(|l| {
                    format!(
                        "{} {:?} {:?} e{:?} p={:?} s{:?} g={:?}",
                        l.kind,
                        l.title,
                        l.year,
                        l.episode,
                        l.parent_title,
                        l.parent_season,
                        l.grandparent_title
                    )
                })
                .collect::<Vec<_>>()
                .join("; ")
        };
        match e.kind.as_str() {
            "sample" => {
                if linked.is_empty() {
                    *ok.entry("sample").or_default() += 1;
                } else {
                    fail("sample became an item".into(), e, summary());
                }
            }
            "movie" => match linked.as_slice() {
                [l] if l.kind == "movie"
                    && norm(Some(&l.title)) == norm(e.title.as_deref())
                    && l.year == e.year =>
                {
                    *ok.entry("movie").or_default() += 1;
                }
                [l] if l.kind == "movie" && norm(Some(&l.title)) == norm(e.title.as_deref()) => {
                    fail("movie: wrong year".into(), e, summary())
                }
                [l] if l.kind == "movie" => fail("movie: wrong title".into(), e, summary()),
                _ => fail("movie: wrong kind".into(), e, summary()),
            },
            "episode" => {
                let all_eps = !linked.is_empty() && linked.iter().all(|l| l.kind == "episode");
                let show_ok = linked
                    .iter()
                    .all(|l| norm(l.grandparent_title.as_deref()) == norm(e.show.as_deref()));
                let mut got: Vec<u16> = linked.iter().filter_map(|l| l.episode).collect();
                got.sort_unstable();
                let mut want = e.episodes.clone();
                want.sort_unstable();
                let season_ok = linked.iter().all(|l| l.parent_season == e.season);
                if !all_eps {
                    let class = if e.absolute.is_some() {
                        "episode (anime absolute): wrong kind"
                    } else {
                        "episode: wrong kind"
                    };
                    fail(class.into(), e, summary());
                } else if !show_ok {
                    fail("episode: wrong show".into(), e, summary());
                } else if !season_ok || got != want {
                    let class = if e.absolute.is_some() {
                        "episode (anime absolute): season/episode differ (by design, needs flux-meta)"
                    } else {
                        "episode: wrong season/episode"
                    };
                    fail(class.into(), e, summary());
                } else {
                    *ok.entry("episode").or_default() += 1;
                }
            }
            "extra" => match linked.as_slice() {
                [l] if l.kind == "extra"
                    && norm(l.parent_title.as_deref()) == norm(e.parent_title.as_deref()) =>
                {
                    *ok.entry("extra").or_default() += 1;
                }
                [l] if l.kind == "extra" => fail("extra: wrong parent".into(), e, summary()),
                [] => fail("extra: not placed (pending?)".into(), e, summary()),
                _ => fail("extra: wrong kind".into(), e, summary()),
            },
            _ => {}
        }
    }
    let media = truth.iter().filter(|e| e.is_media()).count();
    let correct: u64 = ok.values().sum();
    Ok(json!({
        "media_files_in_manifest": media,
        "files_correct": correct,
        "files_correct_pct": 100.0 * correct as f64 / media.max(1) as f64,
        "correct_by_kind": ok,
        "wrong_by_class": wrong.iter().map(|(k, v)| (k.clone(), json!({"count": v.len(), "examples": v.iter().take(5).collect::<Vec<_>>()}))).collect::<serde_json::Map<_, _>>(),
        "expected_items": expected_items(truth),
        "catalog_items": counts,
    }))
}

/// Distinct items the ground truth implies (same keys as flux-catalog's corpus test).
pub fn expected_items(truth: &[&ManifestEntry]) -> Value {
    use std::collections::HashSet;
    let mut movies = HashSet::new();
    let mut shows = HashSet::new();
    let mut seasons = HashSet::new();
    let mut episodes = HashSet::new();
    let mut extras = 0;
    for e in truth {
        match e.kind.as_str() {
            "movie" => {
                movies.insert(match e.tmdb_id {
                    Some(id) => format!("tmdb:{id}"),
                    None => format!(
                        "{}|{:?}",
                        normalize_title(e.title.as_deref().unwrap_or("")),
                        e.year
                    ),
                });
            }
            "episode" => {
                let show = format!(
                    "{}|{:?}",
                    normalize_title(e.show.as_deref().unwrap_or("")),
                    e.year
                );
                for ep in &e.episodes {
                    episodes.insert(format!("{show}|{:?}|{ep}", e.season));
                }
                seasons.insert(format!("{show}|{:?}", e.season));
                shows.insert(show);
            }
            "extra" => extras += 1,
            _ => {}
        }
    }
    json!({"movie": movies.len(), "show": shows.len(), "season": seasons.len(), "episode": episodes.len(), "extra": extras})
}
