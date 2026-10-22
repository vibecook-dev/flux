//! A new library's folders, checked against each other and against every
//! existing library by where they really are: symlinks and `..` resolved,
//! in the letter case the volume stores (Windows, macOS), without Windows'
//! `\\?\` prefix. The catalog checks them again as written
//! ([`flux_catalog::writer::root_overlap`]), which also catches a library
//! whose folder can't be resolved right now (an unplugged drive).
//!
//! Used by `POST /api/v1/libraries` and by `--library` flags and config
//! entries at startup.

use std::path::{Path, PathBuf};
use std::time::Duration;

use flux_catalog::writer::root_overlap;
use flux_catalog::{read, Catalog, CatalogError, RootOverlap};

/// How long resolving the folders may take (a dead network mount can hang
/// the file system call); after that they are compared as written.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

/// Refuses `roots` (a new library's folders) that are the same folder as,
/// or inside or around, another of them ([`CatalogError::OverlappingRoots`])
/// or a folder of an existing library ([`CatalogError::RootInOtherLibrary`]):
/// every file below would be cataloged twice.
pub async fn check_new_roots(catalog: &Catalog, roots: &[String]) -> Result<(), CatalogError> {
    let existing: Vec<(String, String)> = catalog
        .read(read::list_libraries)
        .await?
        .into_iter()
        .flat_map(|l| {
            let name = l.name;
            l.roots.into_iter().map(move |r| (r.path, name.clone()))
        })
        .collect();
    let mut paths = roots.to_vec();
    paths.extend(existing.iter().map(|(p, _)| p.clone()));
    let real = resolve_all(paths).await;
    let (new, old) = real.split_at(roots.len());
    for (i, a) in new.iter().enumerate() {
        for (j, b) in new.iter().enumerate().skip(i + 1) {
            let (inner, outer) = match root_overlap(a, b) {
                Some(RootOverlap::Same | RootOverlap::Inside) => (i, j),
                Some(RootOverlap::Contains) => (j, i),
                None => continue,
            };
            return Err(CatalogError::OverlappingRoots {
                root: roots[inner].clone(),
                other: roots[outer].clone(),
            });
        }
        for (b, (other, library)) in old.iter().zip(&existing) {
            if let Some(how) = root_overlap(a, b) {
                return Err(CatalogError::RootInOtherLibrary {
                    root: roots[i].clone(),
                    other: other.clone(),
                    library: library.clone(),
                    how,
                });
            }
        }
    }
    Ok(())
}

/// [`real_path`] of each, or every path as written if that takes too long.
async fn resolve_all(paths: Vec<String>) -> Vec<String> {
    let written = paths.clone();
    let work = tokio::task::spawn_blocking(move || paths.iter().map(|p| real_path(p)).collect::<Vec<_>>());
    match tokio::time::timeout(RESOLVE_TIMEOUT, work).await {
        Ok(Ok(real)) => real,
        _ => written,
    }
}

/// The folder as the file system names it. A folder that doesn't exist yet
/// is its nearest existing parent's real path plus the rest as written; one
/// whose volume is away stays as written.
fn real_path(p: &str) -> String {
    let path = Path::new(p);
    let mut rest = Vec::new();
    let mut at = Some(path);
    while let Some(dir) = at {
        if let Ok(real) = std::fs::canonicalize(dir) {
            let mut real = PathBuf::from(strip_verbatim(&real.to_string_lossy()));
            real.extend(rest.iter().rev());
            return real.to_string_lossy().into_owned();
        }
        match (dir.parent(), dir.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                at = Some(parent);
            }
            _ => break,
        }
    }
    p.to_string()
}

/// `\\?\D:\x` → `D:\x` and `\\?\UNC\server\share` → `\\server\share`
/// (Windows' canonical form), so that resolved and unresolved paths compare.
fn strip_verbatim(p: &str) -> String {
    if let Some(rest) = p.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = p.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        p.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_prefixes_are_dropped() {
        assert_eq!(strip_verbatim(r"\\?\D:\Media\Movies"), r"D:\Media\Movies");
        assert_eq!(strip_verbatim(r"\\?\UNC\nas\media\TV"), r"\\nas\media\TV");
        assert_eq!(strip_verbatim("/media/Movies"), "/media/Movies");
    }

    #[test]
    fn folders_that_dont_exist_yet_resolve_through_their_parent() {
        let dir = tempfile::tempdir().unwrap();
        let here = real_path(&dir.path().to_string_lossy());
        assert!(!here.starts_with(r"\\?\"), "{here}");
        assert!(Path::new(&here).is_dir(), "{here}");
        let later = dir.path().join("not-there").join("yet");
        assert_eq!(
            Path::new(&real_path(&later.to_string_lossy())),
            Path::new(&here).join("not-there").join("yet")
        );
    }
}
