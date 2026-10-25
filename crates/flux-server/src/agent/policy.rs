//! What agents are never shown, and the house rules every agent is told.
//!
//! TMDB's API terms forbid using TMDB's content with AI (research/05,
//! §1.C; `flux_meta::tmdb::attribution::TMDB_TERMS`), and flux's rule since
//! the start is that AI never sees it (docs/ARCHITECTURE.md, principle 6).
//! So a caller that names an agent (MCP always does; HTTP with
//! `X-Flux-Agent`; the CLI when it runs under one) gets TMDB's text and
//! images withheld, and is told which fields those are. People get
//! everything.

use flux_core::agent::Policy;

/// Providers whose terms forbid use with AI, by the name `item_field`
/// records them under.
pub fn ai_forbidden() -> Vec<String> {
    let mut v = Vec::new();
    if !flux_meta::tmdb::attribution::TMDB_TERMS.ai_allowed {
        v.push("tmdb".to_string());
    }
    v
}

/// What `agent` (a caller naming one) may not see: [`ai_forbidden`]; a
/// person sees everything.
pub fn hidden(agent: Option<&str>) -> Vec<String> {
    if agent.is_some() {
        ai_forbidden()
    } else {
        Vec::new()
    }
}

/// The rules, one sentence each: the manifest's, `status`'s and the MCP
/// server's instructions.
pub const RULES: [&str; 7] = [
    "Every value you set needs the pages that support it (sources); write descriptions in your own words.",
    "Use what the files say (path, container title, duration, languages, chapters) before guessing.",
    "Never replace what a person set by hand unless they asked you to (force).",
    "TMDB's text and images are withheld from agents (its terms forbid use with AI); fill gaps from the open web instead.",
    "Credit images you add (credit, found_at); make AI images only when asked, and say which model made them.",
    "Prefer a few precise changes with a clear summary; every changeset can be reviewed and undone in the app.",
    "When you aren't sure, propose (mode: propose) instead of applying.",
];

pub fn policy() -> Policy {
    Policy {
        withheld: ai_forbidden(),
        rules: RULES.iter().map(|r| r.to_string()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_never_see_tmdb_and_people_see_all() {
        assert_eq!(hidden(Some("claude-code")), ["tmdb"]);
        assert!(hidden(None).is_empty());
        assert_eq!(policy().withheld, ["tmdb"]);
    }
}
