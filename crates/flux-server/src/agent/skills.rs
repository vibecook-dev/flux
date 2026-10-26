//! The skills flux ships for agents (`skills/` at the top of the repo),
//! compiled in: served at `/api/v1/agent/skills/{name}` and over MCP as
//! resources and prompts. `flux skills install` copies the same files.

use flux_core::agent::SkillInfo;

/// One skill: its `SKILL.md` and the files beside it.
pub struct Skill {
    pub name: &'static str,
    pub text: &'static str,
    pub files: &'static [(&'static str, &'static str)],
}

pub const SKILLS: [Skill; 5] = [
    Skill {
        name: "flux",
        text: include_str!("../../../../skills/flux/SKILL.md"),
        files: &[],
    },
    Skill {
        name: "flux-metadata",
        text: include_str!("../../../../skills/flux-metadata/SKILL.md"),
        files: &[],
    },
    Skill {
        name: "flux-artwork",
        text: include_str!("../../../../skills/flux-artwork/SKILL.md"),
        files: &[],
    },
    Skill {
        name: "flux-organize",
        text: include_str!("../../../../skills/flux-organize/SKILL.md"),
        files: &[],
    },
    Skill {
        name: "flux-lens",
        text: include_str!("../../../../skills/flux-lens/SKILL.md"),
        files: &[(
            "reference.md",
            include_str!("../../../../skills/flux-lens/reference.md"),
        )],
    },
];

impl Skill {
    /// The `description` of its front matter.
    pub fn description(&self) -> String {
        front_matter(self.text, "description").unwrap_or_default()
    }

    /// The text after the front matter.
    pub fn body(&self) -> &'static str {
        let t = self.text;
        match t
            .strip_prefix("---\n")
            .and_then(|rest| rest.find("\n---\n").map(|i| &rest[i + 5..]))
        {
            Some(body) => body.trim_start(),
            None => t,
        }
    }
}

/// A `key: value` line of the front matter.
fn front_matter(text: &str, key: &str) -> Option<String> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    rest[..end].lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == key).then(|| v.trim().trim_matches('"').to_string())
    })
}

pub fn get(name: &str) -> Option<&'static Skill> {
    SKILLS.iter().find(|s| s.name == name)
}

pub fn list() -> Vec<SkillInfo> {
    SKILLS
        .iter()
        .map(|s| SkillInfo {
            name: s.name.into(),
            description: s.description(),
            url: format!("/api/v1/agent/skills/{}", s.name),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_skill_has_a_name_and_description_that_fit_the_spec() {
        for s in &SKILLS {
            assert_eq!(front_matter(s.text, "name").as_deref(), Some(s.name));
            let d = s.description();
            assert!(!d.is_empty() && d.len() <= 1024, "{}: {}", s.name, d.len());
            assert!(s.name.len() <= 64 && s.name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'));
            assert!(s.body().starts_with('#'), "{}", s.name);
        }
        assert!(get("flux-lens").unwrap().files[0]
            .1
            .contains("layout(items, view)"));
    }
}
