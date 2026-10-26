//! The agent contract's JSON Schemas, checked in under `schemas/agent/v1/`
//! (docs/AGENT-NATIVE.md, "Contracts"), must be what the code generates:
//! a contract change shows in review as a schema diff. To update them:
//!
//! ```sh
//! UPDATE_SCHEMAS=1 cargo test -p flux-server --test schemas
//! ```

use std::collections::BTreeSet;
use std::path::PathBuf;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/agent/v1")
}

#[test]
fn checked_in_schemas_match_the_code() {
    let update = std::env::var_os("UPDATE_SCHEMAS").is_some();
    let dir = dir();
    if update {
        std::fs::create_dir_all(&dir).unwrap();
    }
    let mut expected = BTreeSet::new();
    let mut stale = Vec::new();
    for name in flux_server::agent::schema_names() {
        let file = format!("{name}.json");
        let text = serde_json::to_string_pretty(&flux_server::agent::schema(&name).unwrap()).unwrap() + "\n";
        let path = dir.join(&file);
        expected.insert(file.clone());
        if update {
            std::fs::write(&path, &text).unwrap();
        } else if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
            stale.push(file);
        }
    }
    // No schema the code no longer has.
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.ends_with(".json") && !expected.contains(&n) {
            if update {
                std::fs::remove_file(e.path()).unwrap();
            } else {
                stale.push(format!("{n} (no longer generated)"));
            }
        }
    }
    assert!(
        stale.is_empty(),
        "schemas/agent/v1 is out of date: {stale:?}. Run UPDATE_SCHEMAS=1 cargo test -p flux-server --test schemas"
    );
}
