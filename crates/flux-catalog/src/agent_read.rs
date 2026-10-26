//! What agents read (docs/AGENT-NATIVE.md): how much each library lacks
//! ([`gaps`]), items by what they lack ([`find`]), and one item with the
//! evidence its files give ([`item`]).
//!
//! `hide` names the providers whose content the reader must not see
//! (their terms forbid use with AI): a field they set is left out and
//! listed as withheld, a title of theirs is replaced by the file's, and
//! their images keep no address. Pass none for a person.

use std::collections::BTreeMap;

use flux_core::agent::{
    AgentArt, AgentItem, AgentItemDetail, ArtKind, FileEvidence, Gap, GapCounts, LibraryGaps, MatchState,
    TrackEvidence,
};
use flux_core::api::{ArtSource, Id, ItemKind};
use flux_core::{LibraryKind, NameGuess, ProbeResult, TrackKind};
use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{CatalogError, Result};
use crate::fields;
use crate::model::{parse_art_source, parse_item_kind, parse_library_kind};

/// A placeholder episode title ("Episode 3"), in SQL (`GLOB`).
const PLACEHOLDER: &str = "'Episode [0-9]*'";

/// The SQL test that item `i` has `gap`. `i` is a row of `item`.
fn gap_sql(gap: Gap) -> String {
    let real_art = |kind: &str| {
        format!(
            "NOT EXISTS (SELECT 1 FROM art a WHERE a.item_id = i.id AND a.kind = '{kind}'
                AND (a.source IN ('provider', 'local', 'custom') OR a.pinned = 1))"
        )
    };
    let works = "i.kind IN ('movie', 'show') AND i.parent_id IS NULL AND l.kind <> 'videos'";
    match gap {
        Gap::Identity => format!(
            "{works} AND i.match_state IN ('unmatched', 'parsed')
             AND NOT EXISTS (SELECT 1 FROM item_field f WHERE f.item_id = i.id AND f.source IN ('user', 'agent', 'sidecar')
                             AND (f.field = 'title' OR f.field LIKE 'ids.%'))"
        ),
        Gap::Year => format!("{works} AND i.year IS NULL"),
        Gap::Overview => format!("{works} AND (i.overview IS NULL OR i.overview = '')"),
        Gap::Poster => format!("{works} AND {}", real_art("poster")),
        Gap::Backdrop => format!("{works} AND {}", real_art("backdrop")),
        Gap::EpisodeTitles => format!(
            "((i.kind = 'episode' AND i.title GLOB {PLACEHOLDER})
              OR (i.kind = 'show' AND EXISTS (
                    SELECT 1 FROM item e WHERE e.kind = 'episode' AND e.state <> 'missing' AND e.title GLOB {PLACEHOLDER}
                      AND (e.parent_id = i.id OR e.parent_id IN (SELECT s.id FROM item s WHERE s.parent_id = i.id)))))
             AND l.kind <> 'videos'"
        ),
        Gap::Offline => "i.state = 'unavailable'".to_string(),
    }
}

/// Whether a gap applies to a library of `kind` (a videos library is never
/// matched, so only `offline` does).
fn applies(gap: Gap, kind: LibraryKind) -> bool {
    kind != LibraryKind::Videos || gap == Gap::Offline
}

/// How many items of each library (or of `library`) lack each thing.
pub fn gaps(c: &Connection, library: Option<Id>) -> Result<Vec<LibraryGaps>> {
    let libs: Vec<(Id, String, String)> = {
        let mut stmt =
            c.prepare_cached("SELECT id, name, kind FROM library WHERE ?1 IS NULL OR id = ?1 ORDER BY id")?;
        let rows = stmt.query_map([library], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    if let (Some(id), true) = (library, libs.is_empty()) {
        return Err(CatalogError::NotFound(format!("library {id}")));
    }
    let mut out = Vec::with_capacity(libs.len());
    for (id, name, kind) in libs {
        let kind = parse_library_kind(&kind)?;
        let count = |sql: &str| -> Result<u64> {
            Ok(c.prepare_cached(&format!(
                "SELECT count(*) FROM item i JOIN library l ON l.id = i.library_id
                  WHERE i.library_id = ?1 AND i.state <> 'missing' AND ({sql})"
            ))?
            .query_row([id], |r| r.get::<_, i64>(0))? as u64)
        };
        let items = count("i.parent_id IS NULL")?;
        let mut g = GapCounts::default();
        let top = |gap: Gap| -> Result<Option<u64>> {
            if !applies(gap, kind) {
                return Ok(None);
            }
            let sql = gap_sql(gap);
            // Offline and episode titles count the library's top level.
            count(&format!("i.parent_id IS NULL AND ({sql})")).map(Some)
        };
        g.identity = top(Gap::Identity)?;
        g.year = top(Gap::Year)?;
        g.overview = top(Gap::Overview)?;
        g.poster = top(Gap::Poster)?;
        g.backdrop = top(Gap::Backdrop)?;
        g.episode_titles = top(Gap::EpisodeTitles)?;
        g.offline = top(Gap::Offline)?;
        let placeholder_episodes = if applies(Gap::EpisodeTitles, kind) {
            Some(count(&format!(
                "i.kind = 'episode' AND i.title GLOB {PLACEHOLDER}"
            ))?)
        } else {
            None
        };
        out.push(LibraryGaps {
            id,
            name,
            kind,
            items,
            gaps: g,
            placeholder_episodes,
        });
    }
    Ok(out)
}

/// What [`find`] looks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FindQuery {
    pub library: Option<Id>,
    pub parent: Option<Id>,
    pub kind: Option<ItemKind>,
    pub gap: Option<Gap>,
    pub match_state: Option<MatchState>,
    pub q: Option<String>,
    pub limit: u32,
    /// Items after this id.
    pub after: Option<Id>,
}

fn kind_str(k: ItemKind) -> &'static str {
    match k {
        ItemKind::Movie => "movie",
        ItemKind::Show => "show",
        ItemKind::Season => "season",
        ItemKind::Episode => "episode",
        ItemKind::Extra => "extra",
    }
}

fn match_state_str(m: MatchState) -> &'static str {
    match m {
        MatchState::Unmatched => "unmatched",
        MatchState::Parsed => "parsed",
        MatchState::Matched => "matched",
        MatchState::Locked => "locked",
    }
}

fn parse_match_state(s: &str) -> MatchState {
    match s {
        "unmatched" => MatchState::Unmatched,
        "matched" => MatchState::Matched,
        "locked" => MatchState::Locked,
        _ => MatchState::Parsed,
    }
}

/// `%text%` for `LIKE … ESCAPE '\'`.
fn like(text: &str) -> String {
    let mut s = String::with_capacity(text.len() + 2);
    s.push('%');
    for ch in text.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            s.push('\\');
        }
        s.push(ch);
    }
    s.push('%');
    s
}

/// Items matching `q`, by id, and how many match in all.
pub fn find(c: &Connection, q: &FindQuery, hide: &[String]) -> Result<(Vec<AgentItem>, Option<Id>, u64)> {
    use rusqlite::types::Value as Sql;
    fn arg(args: &mut Vec<Sql>, v: Sql) -> String {
        args.push(v);
        format!("?{}", args.len())
    }
    let mut wh: Vec<String> = vec!["i.state <> 'missing'".into()];
    let mut args: Vec<Sql> = Vec::new();
    if let Some(l) = q.library {
        wh.push(format!("i.library_id = {}", arg(&mut args, l.into())));
    }
    if let Some(p) = q.parent {
        wh.push(format!(
            "i.id IN (WITH RECURSIVE sub(id) AS (SELECT id FROM item WHERE parent_id = {p}
                      UNION ALL SELECT x.id FROM item x JOIN sub ON x.parent_id = sub.id) SELECT id FROM sub)",
            p = arg(&mut args, p.into())
        ));
    }
    match q.kind {
        Some(k) => wh.push(format!(
            "i.kind = {}",
            arg(&mut args, kind_str(k).to_string().into())
        )),
        None if q.parent.is_none() => wh.push("i.parent_id IS NULL".into()),
        None => {}
    }
    if let Some(g) = q.gap {
        wh.push(format!("({})", gap_sql(g)));
    }
    if let Some(m) = q.match_state {
        wh.push(format!(
            "i.match_state = {}",
            arg(&mut args, match_state_str(m).to_string().into())
        ));
    }
    if let Some(text) = q.q.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        let pat = arg(&mut args, like(text).into());
        let hidden = arg(&mut args, serde_json::to_string(hide)?.into());
        wh.push(format!(
            "((i.title LIKE {pat} ESCAPE '\\' AND NOT EXISTS (SELECT 1 FROM item_field f WHERE f.item_id = i.id
                  AND f.field = 'title' AND f.source = 'provider' AND f.provider IN (SELECT value FROM json_each({hidden}))))
              OR (i.original_title LIKE {pat} ESCAPE '\\' AND NOT EXISTS (SELECT 1 FROM item_field f WHERE f.item_id = i.id
                  AND f.field = 'original_title' AND f.source = 'provider' AND f.provider IN (SELECT value FROM json_each({hidden}))))
              OR EXISTS (SELECT 1 FROM asset_item ai JOIN asset a ON a.id = ai.asset_id JOIN file fl ON fl.id = a.file_id
                          WHERE ai.item_id = i.id AND fl.rel_path LIKE {pat} ESCAPE '\\')
              OR EXISTS (SELECT 1 FROM item_folder d WHERE d.item_id = i.id AND d.folder LIKE {pat} ESCAPE '\\'))"
        ));
    }
    let filter = wh.join(" AND ");
    let total: i64 = c
        .prepare(&format!(
            "SELECT count(*) FROM item i JOIN library l ON l.id = i.library_id WHERE {filter}"
        ))?
        .query_row(rusqlite::params_from_iter(args.iter()), |r| r.get(0))?;
    let limit_n = q.limit.clamp(1, 200);
    let after = arg(&mut args, q.after.unwrap_or(0).into());
    let limit = arg(&mut args, i64::from(limit_n).into());
    let ids: Vec<Id> = {
        let mut stmt = c.prepare(&format!(
            "SELECT i.id FROM item i JOIN library l ON l.id = i.library_id
              WHERE {filter} AND i.id > {after} ORDER BY i.id LIMIT {limit}"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let next = if ids.len() as u32 == limit_n {
        ids.last().copied()
    } else {
        None
    };
    let mut items = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(it) = agent_item(c, id, hide)? {
            items.push(it);
        }
    }
    Ok((items, next, total.max(0) as u64))
}

/// The gaps item `id` has.
fn item_gaps(c: &Connection, id: Id) -> Result<Vec<Gap>> {
    let mut out = Vec::new();
    for g in Gap::ALL {
        let has: bool = c
            .prepare_cached(&format!(
                "SELECT EXISTS (SELECT 1 FROM item i JOIN library l ON l.id = i.library_id WHERE i.id = ?1 AND ({}))",
                gap_sql(g)
            ))?
            .query_row([id], |r| r.get(0))?;
        if has {
            out.push(g);
        }
    }
    Ok(out)
}

/// The provider an image is credited to, as `hide` names providers.
fn art_provider(source: ArtSource, attribution: Option<&str>) -> Option<String> {
    if source != ArtSource::Provider {
        return None;
    }
    attribution.map(|a| a.trim().to_ascii_lowercase().replace([' ', '.'], ""))
}

/// The title a reader may see: the item's, unless a hidden provider gave
/// it (then the file's).
fn visible_title(c: &Connection, id: Id, title: String, hide: &[String]) -> Result<(String, Option<String>)> {
    if hide.is_empty() {
        return Ok((title, None));
    }
    match fields::source_of(c, id, "title")? {
        Some(s) if s.source == fields::PROVIDER && s.provider.as_ref().is_some_and(|p| hide.contains(p)) => {
            let own = fields::file_title(c, id)?.unwrap_or_else(|| format!("Item {id}"));
            Ok((own, s.provider))
        }
        _ => Ok((title, None)),
    }
}

/// One item, briefly, as a reader that may not see `hide` sees it.
pub fn agent_item(c: &Connection, id: Id, hide: &[String]) -> Result<Option<AgentItem>> {
    type Row = (
        String,
        Id,
        Option<Id>,
        String,
        Option<u16>,
        Option<u16>,
        Option<u16>,
        String,
        String,
    );
    let row: Option<Row> = c
        .prepare_cached(
            "SELECT kind, library_id, parent_id, title, year, season_number, episode_number, match_state, state
               FROM item WHERE id = ?1",
        )?
        .query_row([id], |r| {
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
    let Some((kind, library, parent, title, year, season, episode, match_state, state)) = row else {
        return Ok(None);
    };
    let kind = parse_item_kind(&kind)?;
    let (title, _) = visible_title(c, id, title, hide)?;
    let subtitle = match kind {
        ItemKind::Episode => {
            let s = match parent {
                Some(p) => c
                    .prepare_cached("SELECT season_number FROM item WHERE id = ?1")?
                    .query_row([p], |r| r.get::<_, Option<u16>>(0))
                    .optional()?
                    .flatten(),
                None => None,
            };
            match (s.or(season), episode) {
                (Some(s), Some(e)) => Some(format!("S{s:02}E{e:02}")),
                (None, Some(e)) => Some(format!("E{e:02}")),
                _ => None,
            }
        }
        ItemKind::Season => Some(title.clone()),
        _ => None,
    };
    let art_kind = if kind == ItemKind::Episode {
        "thumb"
    } else {
        "poster"
    };
    let poster: Option<String> = c
        .prepare_cached(&format!(
            "SELECT source FROM art WHERE item_id = ?1 AND kind = '{art_kind}' ORDER BY {} LIMIT 1",
            crate::read::ART_ORDER
        ))?
        .query_row([id], |r| r.get(0))
        .optional()?;
    Ok(Some(AgentItem {
        id,
        kind,
        library,
        parent,
        title,
        year,
        subtitle,
        match_state: parse_match_state(&match_state),
        gaps: item_gaps(c, id)?,
        poster: poster.as_deref().map(parse_art_source).transpose()?,
        available: state != "unavailable",
        path: first_path(c, id, kind)?,
    }))
}

/// The first file's path (a show: its first folder).
fn first_path(c: &Connection, id: Id, kind: ItemKind) -> Result<Option<String>> {
    let row: Option<(String, String)> = if kind == ItemKind::Show {
        c.prepare_cached(
            "SELECT r.path, d.folder FROM item_folder d JOIN item i ON i.id = d.item_id
               JOIN root r ON r.library_id = i.library_id
              WHERE d.item_id = ?1 ORDER BY d.folder, r.id LIMIT 1",
        )?
        .query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?
    } else {
        c.prepare_cached(
            "SELECT r.path, f.rel_path FROM asset_item ai JOIN asset a ON a.id = ai.asset_id
               JOIN file f ON f.id = a.file_id JOIN root r ON r.id = f.root_id
              WHERE ai.item_id = ?1 AND f.state <> 'missing' ORDER BY a.id LIMIT 1",
        )?
        .query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?
    };
    Ok(row.map(|(root, rel)| crate::read::path_join(&root, &rel)))
}

fn track(t: &flux_core::Track) -> Option<TrackEvidence> {
    let kind = match t.kind {
        TrackKind::Video => "video",
        TrackKind::Audio => "audio",
        TrackKind::Subtitle => "subtitle",
        _ => return None,
    };
    Some(TrackEvidence {
        kind: kind.into(),
        codec: t.codec.display_name().into(),
        language: t.language.clone(),
        title: t.title.clone(),
        width: t.width,
        height: t.height,
        fps: t
            .frame_rate
            .map(|r| (r.as_f64() * 1000.0).round() as f32 / 1000.0),
        channels: t.channels,
        hdr: t
            .hdr
            .as_ref()
            .map(|h| h.format.display_name().to_string())
            .filter(|h| h != "SDR"),
        default: t.default,
        forced: t.forced,
    })
}

/// One item as a reader that may not see `hide` sees it, with its files'
/// evidence, its images and its children.
pub fn item(c: &Connection, id: Id, hide: &[String]) -> Result<Option<AgentItemDetail>> {
    let Some(brief) = agent_item(c, id, hide)? else {
        return Ok(None);
    };
    let Some(cur) = fields::read(c, id)? else {
        return Ok(None);
    };
    let mut values = cur.values;
    let mut sources = BTreeMap::new();
    let mut withheld = BTreeMap::new();
    for (field, row) in fields::sources(c, id)? {
        // Ids name a work (and lead to open data): they stay visible.
        let hidden = row.source == fields::PROVIDER
            && row.provider.as_ref().is_some_and(|p| hide.contains(p))
            && !field.starts_with("ids.");
        if hidden {
            withheld.insert(field.clone(), row.provider.clone().unwrap_or_default());
            values.remove(&field);
            continue;
        }
        if values.contains_key(&field) {
            sources.insert(field, row.to_api());
        }
    }
    if withheld.contains_key("title") {
        // The file's title stands in, so the reader knows what this is.
        values.insert("title".into(), serde_json::Value::String(brief.title.clone()));
        sources.insert(
            "title".into(),
            flux_core::agent::FieldSource {
                source: flux_core::agent::Source::File,
                provider: None,
                agent: None,
                by: None,
                changeset: None,
                at: None,
                sources: Vec::new(),
            },
        );
    }
    let head: (Option<f64>, Option<u16>, Option<u16>) = c
        .prepare_cached("SELECT match_confidence, season_number, episode_number FROM item WHERE id = ?1")?
        .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    let (confidence, season_number, episode) = head;
    // Matcher evidence names the provider's candidates: a reader that may
    // not see them gets the confidence only (`match_evidence` isn't sent).
    let mut files = Vec::new();
    for a in crate::read::item_assets(c, id)? {
        let meta: Option<(Id, i64, Option<String>, String, String)> = c
            .prepare_cached(
                "SELECT f.id, f.mtime_ns, m.guess, r.path, f.rel_path FROM asset s JOIN file f ON f.id = s.file_id
                   JOIN root r ON r.id = f.root_id
                   LEFT JOIN file_meta m ON m.file_id = f.id WHERE s.id = ?1",
            )?
            .query_row([a.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .optional()?;
        let Some((file_id, mtime, guess, root, rel_path)) = meta else {
            continue;
        };
        let probe: Option<&ProbeResult> = a.probe.as_ref();
        files.push(FileEvidence {
            id: file_id,
            root,
            sidecars: crate::metafiles::beside(c, file_id)?,
            fix: crate::fixes::fix_of(c, file_id)?,
            path: rel_path,
            size: a.size,
            modified: (mtime > 0).then_some(mtime / 1_000_000),
            available: a.available,
            container: (a.container != flux_core::Container::Unknown)
                .then(|| a.container.display_name().to_string()),
            duration_ms: a.duration_ms,
            container_title: probe.and_then(|p| p.title.clone()),
            edition: a.edition.clone(),
            tracks: probe
                .map(|p| p.tracks.iter().filter_map(track).collect())
                .unwrap_or_default(),
            chapters: probe
                .map(|p| {
                    p.chapters
                        .iter()
                        .filter_map(|ch| ch.title.clone())
                        .take(50)
                        .collect()
                })
                .unwrap_or_default(),
            subtitle_files: a.subtitle_files.iter().map(|s| s.path.clone()).collect(),
            parsed: guess.and_then(|g| serde_json::from_str::<NameGuess>(&g).ok()),
        });
    }
    let folders: Vec<String> = {
        let mut stmt =
            c.prepare_cached("SELECT folder FROM item_folder WHERE item_id = ?1 ORDER BY folder")?;
        let rows = stmt.query_map([id], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut art = Vec::new();
    let shown: BTreeMap<String, Id> = {
        let mut m = BTreeMap::new();
        for k in ["poster", "backdrop", "thumb", "logo"] {
            let best: Option<Id> = c
                .prepare_cached(&format!(
                    "SELECT id FROM art WHERE item_id = ?1 AND kind = ?2 ORDER BY {} LIMIT 1",
                    crate::read::ART_ORDER
                ))?
                .query_row(params![id, k], |r| r.get(0))
                .optional()?;
            if let Some(b) = best {
                m.insert(k.to_string(), b);
            }
        }
        m
    };
    for r in crate::read::item_art(c, id)? {
        let kind = match r.kind.as_str() {
            "poster" => ArtKind::Poster,
            "backdrop" => ArtKind::Backdrop,
            "thumb" => ArtKind::Thumb,
            _ => ArtKind::Logo,
        };
        let provider = art_provider(r.source, r.attribution.as_deref()).filter(|p| hide.contains(p));
        art.push(AgentArt {
            id: r.id,
            kind,
            source: r.source,
            shown: shown.get(&r.kind) == Some(&r.id),
            pinned: r.pinned,
            width: r.width,
            height: r.height,
            credit: r.attribution.clone(),
            found_at: r.found_at.clone(),
            generator: r.generator.clone(),
            url: provider.is_none().then(|| format!("/api/v1/art/{}", r.id)),
            withheld: provider,
        });
    }
    let children: Vec<Id> = {
        let mut stmt = c.prepare_cached(
            "SELECT id FROM item WHERE parent_id = ?1 AND state <> 'missing'
              ORDER BY kind = 'extra', season_number, episode_number, sort_title, id LIMIT 500",
        )?;
        let rows = stmt.query_map([id], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut kids = Vec::with_capacity(children.len());
    for k in children {
        if let Some(it) = agent_item(c, k, hide)? {
            kids.push(it);
        }
    }
    let episode_numbers: Vec<u16> = if brief.kind == ItemKind::Episode {
        let mut stmt = c.prepare_cached(
            "SELECT DISTINCT e.episode_number FROM asset_item ai
               JOIN asset_item ai2 ON ai2.asset_id = ai.asset_id
               JOIN item e ON e.id = ai2.item_id
              WHERE ai.item_id = ?1 AND e.kind = 'episode' AND e.episode_number IS NOT NULL ORDER BY 1",
        )?;
        let rows = stmt.query_map([id], |r| r.get(0))?;
        let mut v: Vec<u16> = rows.collect::<rusqlite::Result<_>>()?;
        if v.is_empty() {
            v.extend(episode);
        }
        v
    } else {
        Vec::new()
    };
    Ok(Some(AgentItemDetail {
        fields: fields::to_fields(&values),
        sources,
        withheld,
        match_confidence: confidence.map(|c| c as f32),
        season_number,
        episode_numbers,
        files,
        folders,
        art,
        children: kids,
        changes: crate::changes::list(c, None, Some(id), None, 10)?,
        item: brief,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_escapes_wildcards() {
        assert_eq!(like("50%_off\\"), "%50\\%\\_off\\\\%");
    }

    #[test]
    fn provider_art_is_named_by_its_credit() {
        assert_eq!(
            art_provider(ArtSource::Provider, Some("TMDB")).as_deref(),
            Some("tmdb")
        );
        assert_eq!(
            art_provider(ArtSource::Provider, Some("fanart.tv")).as_deref(),
            Some("fanarttv")
        );
        assert_eq!(art_provider(ArtSource::Custom, Some("TMDB")), None);
    }
}
