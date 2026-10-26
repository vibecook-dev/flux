//! flux's skills for agents, compiled in (the same files as `skills/` and
//! the server's `/api/v1/agent/skills`): `flux skills install` copies them
//! where an agent finds skills.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::client::Failure;

/// Each skill: its name and files (`SKILL.md` first).
const SKILLS: [(&str, &[(&str, &str)]); 5] = [
    (
        "flux",
        &[("SKILL.md", include_str!("../../../skills/flux/SKILL.md"))],
    ),
    (
        "flux-metadata",
        &[("SKILL.md", include_str!("../../../skills/flux-metadata/SKILL.md"))],
    ),
    (
        "flux-artwork",
        &[("SKILL.md", include_str!("../../../skills/flux-artwork/SKILL.md"))],
    ),
    (
        "flux-organize",
        &[("SKILL.md", include_str!("../../../skills/flux-organize/SKILL.md"))],
    ),
    (
        "flux-lens",
        &[
            ("SKILL.md", include_str!("../../../skills/flux-lens/SKILL.md")),
            (
                "reference.md",
                include_str!("../../../skills/flux-lens/reference.md"),
            ),
        ],
    ),
];

fn description(text: &str) -> String {
    text.strip_prefix("---\n")
        .and_then(|rest| rest.split("\n---\n").next())
        .and_then(|fm| fm.lines().find_map(|l| l.strip_prefix("description:")))
        .map(|d| d.trim().to_string())
        .unwrap_or_default()
}

pub fn list() -> Value {
    json!({
        "skills": SKILLS.iter().map(|(name, files)| json!({
            "name": name,
            "description": description(files[0].1),
        })).collect::<Vec<_>>(),
        "install": "flux skills install [--dir DIR]  (default: ~/.claude/skills)",
    })
}

pub fn show(name: &str) -> Option<&'static str> {
    SKILLS.iter().find(|(n, _)| *n == name).map(|(_, f)| f[0].1)
}

fn default_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".claude").join("skills"))
}

/// Write every skill under `dir` (`<dir>/<name>/SKILL.md`, …), replacing
/// older copies.
pub fn install(dir: Option<&Path>) -> Result<Value, Failure> {
    let dir = dir
        .map(Path::to_path_buf)
        .or_else(default_dir)
        .ok_or_else(|| Failure::usage("no home folder: pass --dir"))?;
    let mut written = Vec::new();
    for (name, files) in SKILLS {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).map_err(|e| Failure::error(format!("creating {}: {e}", d.display())))?;
        for (file, text) in files {
            let p = d.join(file);
            std::fs::write(&p, text).map_err(|e| Failure::error(format!("writing {}: {e}", p.display())))?;
            written.push(p.display().to_string());
        }
    }
    Ok(json!({ "installed": written, "dir": dir.display().to_string() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skills_install_with_their_files() {
        let dir = tempfile::tempdir().unwrap();
        let out = install(Some(dir.path())).unwrap();
        assert_eq!(out["installed"].as_array().unwrap().len(), 6);
        let lens = std::fs::read_to_string(dir.path().join("flux-lens/reference.md")).unwrap();
        assert!(lens.contains("layout(items, view)"));
        assert!(list()["skills"][1]["description"]
            .as_str()
            .unwrap()
            .contains("open web"));
        assert!(show("flux").unwrap().starts_with("---\nname: flux\n"));
    }
}
