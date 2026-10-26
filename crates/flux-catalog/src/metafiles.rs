//! Metadata files beside the videos: a `.flux.json` (flux's own sidecar,
//! [`flux_core::agent::Sidecar`]) or a Kodi `.nfo`. The scan records them
//! like subtitle files (`meta_file`, by directory), with what flux read
//! from them; which videos one describes is decided from the names:
//!
//! - `<video name>.flux.json` and `<video name>.nfo` describe that video
//!   (a film, an episode, an extra);
//! - `flux.json` and `movie.nfo` in a film's own folder describe the film;
//! - `flux.json` and `tvshow.nfo` in a show's folder describe the show.

use flux_core::api::Id;
use rusqlite::{params, Connection};

use crate::error::Result;

/// `meta_file.format` of a `.flux.json`.
pub const FLUX: &str = "flux";
/// `meta_file.format` of a Kodi `.nfo`.
pub const NFO: &str = "nfo";

/// The format of a metadata file the scan records, by its name.
pub fn format_of(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".flux.json") || lower == "flux.json" {
        Some(FLUX)
    } else if lower.ends_with(".nfo") && lower.len() > 4 {
        Some(NFO)
    } else {
        None
    }
}

fn dir_of(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(d, _)| d)
}

fn name_of(rel: &str) -> &str {
    rel.rsplit_once('/').map_or(rel, |(_, n)| n)
}

fn stem_of(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

/// An episode file's show folder as it is on disk (root-relative): its
/// folder without trailing season folders; `None` at the library folder.
/// ([`crate::group::show_folder`] is the same, lowercased, for grouping.)
pub fn show_dir(rel_path: &str) -> Option<String> {
    let (dir, _) = rel_path.rsplit_once('/')?;
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    while parts.last().is_some_and(|p| crate::group::is_season_dir(p)) {
        parts.pop();
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// What a metadata file describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// The video named like it.
    Video,
    /// The film whose folder it's in.
    Folder,
    /// The show whose folder it's in.
    Show,
}

/// What a metadata file named `name` in a folder describes, for a video
/// whose name (without extension) is `stem` in that folder.
fn scope_beside(name: &str, stem: &str) -> Option<Scope> {
    let lower = name.to_ascii_lowercase();
    let stem = stem.to_ascii_lowercase();
    if lower == format!("{stem}.flux.json") || lower == format!("{stem}.nfo") {
        Some(Scope::Video)
    } else if lower == "flux.json" || lower == "movie.nfo" {
        Some(Scope::Folder)
    } else {
        None
    }
}

/// The metadata files in `dir` of a root, `(rel_path, format)`.
fn in_dir(c: &Connection, root_id: Id, dir: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = c.prepare_cached(
        "SELECT rel_path, format FROM meta_file WHERE root_id = ?1 AND dir = ?2 ORDER BY rel_path",
    )?;
    let rows = stmt.query_map(params![root_id, dir], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The metadata files that describe the video at `rel_path` of a root, or
/// the film or show it belongs to (`show_folder`: an episode's show folder,
/// root-relative), with what each describes: the video's own first.
pub fn for_video(
    c: &Connection,
    root_id: Id,
    rel_path: &str,
    show_folder: Option<&str>,
) -> Result<Vec<(String, String, Scope)>> {
    let dir = dir_of(rel_path);
    let stem = stem_of(name_of(rel_path));
    let mut out = Vec::new();
    for (rel, format) in in_dir(c, root_id, dir)? {
        match scope_beside(name_of(&rel), stem) {
            Some(Scope::Video) => out.push((rel, format, Scope::Video)),
            // A film's folder file, only in a folder of its own (not the
            // library folder itself) and not for an episode.
            Some(Scope::Folder) if !dir.is_empty() && show_folder.is_none() => {
                out.push((rel, format, Scope::Folder))
            }
            _ => {}
        }
    }
    if let Some(show) = show_folder.filter(|s| !s.is_empty()) {
        for (rel, format) in in_dir(c, root_id, show)? {
            let lower = name_of(&rel).to_ascii_lowercase();
            if lower == "tvshow.nfo" || lower == "flux.json" {
                out.push((rel, format, Scope::Show));
            }
        }
    }
    out.sort_by_key(|(_, f, s)| (*s, f.as_str() != FLUX));
    Ok(out)
}

/// The metadata files beside the file `file_id` (root-relative paths), as
/// `item.get` lists them.
pub fn beside(c: &Connection, file_id: Id) -> Result<Vec<String>> {
    let row: Option<(Id, String)> = c
        .prepare_cached("SELECT root_id, rel_path FROM file WHERE id = ?1")?
        .query_row([file_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .ok();
    let Some((root_id, rel)) = row else {
        return Ok(Vec::new());
    };
    let show = show_dir(&rel);
    let episode: bool = c
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM file f JOIN asset a ON a.id = f.asset_id JOIN item i ON i.id = a.item_id
                             WHERE f.id = ?1 AND i.kind = 'episode')",
        )?
        .query_row([file_id], |r| r.get(0))?;
    let show = if episode { show } else { None };
    Ok(for_video(c, root_id, &rel, show.as_deref())?
        .into_iter()
        .map(|(rel, _, _)| rel)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_scopes_come_from_names() {
        assert_eq!(format_of("Film.flux.json"), Some(FLUX));
        assert_eq!(format_of("flux.json"), Some(FLUX));
        assert_eq!(format_of("Film.NFO"), Some(NFO));
        assert_eq!(format_of(".nfo"), None);
        assert_eq!(format_of("Film.json"), None);
        assert_eq!(scope_beside("Film (2010).nfo", "Film (2010)"), Some(Scope::Video));
        assert_eq!(
            scope_beside("film (2010).FLUX.json", "Film (2010)"),
            Some(Scope::Video)
        );
        assert_eq!(scope_beside("movie.nfo", "Film (2010)"), Some(Scope::Folder));
        assert_eq!(scope_beside("Other.nfo", "Film (2010)"), None);
        assert_eq!(
            show_dir("Show (2023)/Season 01/Show S01E01.mkv").as_deref(),
            Some("Show (2023)")
        );
        assert_eq!(show_dir("Show.S01E01.mkv"), None);
    }
}
