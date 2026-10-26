//! Organising files through the operations (docs/AGENT-NATIVE.md): a plan
//! changes nothing, applying it moves real files (never replacing one) and
//! the catalog follows them, keeping item ids; undo puts everything back.
//! Also structure fixes and merges over HTTP.

mod common;

use std::path::Path;
use std::time::Instant;

use axum::http::{Method, StatusCode};
use common::*;
use flux_catalog::scan_support;
use flux_core::agent::{ChangesetState, Decided, FindOutput, FixOutput, PlanOutput, StepKind};
use flux_core::api::{Id, Identity};
use flux_core::LibraryKind;
use flux_server::{api::router, build_state, Config};
use serde_json::{json, Value};

struct Lib {
    _dir: tempfile::TempDir,
    st: Shared,
    app: axum::Router,
    root: std::path::PathBuf,
}

/// A library of `kind` over a folder holding `files` (relative path,
/// contents), seeded as a scan would.
async fn library(kind: LibraryKind, files: &[(&str, &str)]) -> Lib {
    let dir = scratch("organize");
    let root = dir.path().join("media");
    for (rel, body) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
    }
    let (st, _) = build_state(Config::for_tests(dir.path().join("data")), Instant::now(), None)
        .await
        .unwrap();
    let cat = st.catalog.clone();
    let r = root.clone();
    tokio::task::block_in_place(move || {
        seed(&cat, "Lib", kind, &r, |_| true);
    });
    let app = router(st.clone());
    Lib {
        _dir: dir,
        st,
        app,
        root,
    }
}

async fn op_as(app: &axum::Router, who: Identity, name: &str, input: Value) -> (StatusCode, String) {
    let r = call(
        app,
        Method::POST,
        &format!("/api/v1/ops/{name}"),
        Some(who),
        &[("x-flux-agent", "claude-code")],
        Some(input),
    )
    .await;
    (r.status, r.text())
}

async fn run(app: &axum::Router, name: &str, input: Value) -> Value {
    let (status, text) = op_as(app, owner(), name, input).await;
    assert_eq!(status, StatusCode::OK, "{name}: {text}");
    serde_json::from_str(&text).unwrap()
}

fn tree(root: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let rel = p.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/");
            if p.is_dir() {
                out.push(format!("{rel}/"));
                walk(base, &p, out);
            } else {
                out.push(rel);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

impl Lib {
    async fn find(&self, title: &str) -> Id {
        let found: FindOutput =
            serde_json::from_value(run(&self.app, "items.find", json!({ "q": title })).await).unwrap();
        found
            .items
            .iter()
            .find(|i| i.title == title)
            .unwrap_or_else(|| panic!("no {title:?} in {found:?}"))
            .id
    }

    fn path_of(&self, file: Id) -> String {
        self.st
            .catalog
            .read_blocking(move |c| {
                Ok(c.query_row("SELECT rel_path FROM file WHERE id = ?1", [file], |r| r.get(0))?)
            })
            .unwrap()
    }

    fn file_at(&self, rel: &'static str) -> Id {
        self.st
            .catalog
            .read_blocking(move |c| {
                Ok(c.query_row("SELECT id FROM file WHERE rel_path = ?1", [rel], |r| r.get(0))?)
            })
            .unwrap()
    }

    fn item_of_file(&self, file: Id) -> Id {
        self.st
            .catalog
            .read_blocking(move |c| {
                Ok(c.query_row(
                    "SELECT a.item_id FROM file f JOIN asset a ON a.id = f.asset_id WHERE f.id = ?1",
                    [file],
                    |r| r.get(0),
                )?)
            })
            .unwrap()
    }

    /// A rescan would see nothing to do: every catalog path is on the disk
    /// with the inode the catalog has.
    #[cfg(unix)]
    fn catalog_matches_disk(&self) {
        use std::os::unix::fs::MetadataExt;
        let root_id = self
            .st
            .catalog
            .read_blocking(|c| Ok(scan_support::list_roots(c)?[0].id))
            .unwrap();
        let snap = self
            .st
            .catalog
            .read_blocking(move |c| scan_support::load_root_snapshot(c, root_id))
            .unwrap();
        for (rel, row) in snap {
            let m =
                std::fs::metadata(self.root.join(&rel)).unwrap_or_else(|_| panic!("{rel} isn't on the disk"));
            assert_eq!(row.ino, Some(m.ino()), "{rel}");
        }
    }

    #[cfg(not(unix))]
    fn catalog_matches_disk(&self) {}
}

#[tokio::test(flavor = "multi_thread")]
async fn a_loose_film_gets_its_own_folder_and_undo_puts_it_back() {
    let lib = library(
        LibraryKind::Movies,
        &[
            ("星际穿越4k.mp4", "video"),
            ("星际穿越4k.zh.srt", "1\n00:00:01,000 --> 00:00:02,000\n你好\n"),
            ("Other Film (1999).mkv", "other"),
        ],
    )
    .await;
    // The name reads as 星际穿越 (4k is a resolution).
    let id = lib.find("星际穿越").await;
    let file = lib.file_at("星际穿越4k.mp4");
    run(
        &lib.app,
        "items.edit",
        json!({"summary": "Identified", "edits": [{"item": id, "set": {"title": "Interstellar", "year": 2014},
               "sources": ["https://en.wikipedia.org/wiki/Interstellar_(film)"]}]}),
    )
    .await;
    let before = tree(&lib.root);

    let plan: PlanOutput = serde_json::from_value(
        run(
            &lib.app,
            "files.plan",
            json!({"summary": "Name Interstellar's files the standard way", "items": [id]}),
        )
        .await,
    )
    .unwrap();
    let cs = plan.changeset.as_ref().expect("a plan").id;
    assert_eq!(plan.changeset.as_ref().unwrap().state, ChangesetState::Proposed);
    let kinds: Vec<StepKind> = plan.steps.iter().map(|s| s.step).collect();
    assert_eq!(
        kinds,
        [StepKind::NewFolder, StepKind::MoveVideo, StepKind::MoveCompanion],
        "{plan:?}"
    );
    assert_eq!(
        plan.steps[1].to.as_deref(),
        Some("Interstellar (2014)/Interstellar (2014).mp4")
    );
    assert_eq!(
        plan.steps[2].to.as_deref(),
        Some("Interstellar (2014)/Interstellar (2014).zh.srt")
    );
    assert_eq!(tree(&lib.root), before, "a plan moves nothing");

    let done: Decided =
        serde_json::from_value(run(&lib.app, "changes.apply", json!({"changeset": cs})).await).unwrap();
    assert_eq!(done.done, 3, "{done:?}");
    assert_eq!(
        tree(&lib.root),
        [
            "Interstellar (2014)/",
            "Interstellar (2014)/Interstellar (2014).mp4",
            "Interstellar (2014)/Interstellar (2014).zh.srt",
            "Other Film (1999).mkv",
        ]
    );
    assert_eq!(lib.path_of(file), "Interstellar (2014)/Interstellar (2014).mp4");
    assert_eq!(lib.item_of_file(file), id, "the film keeps its id");
    lib.catalog_matches_disk();
    // Its guess follows its new name.
    let guess: String = lib
        .st
        .catalog
        .read_blocking(move |c| {
            Ok(
                c.query_row("SELECT guess FROM file_meta WHERE file_id = ?1", [file], |r| {
                    r.get(0)
                })?,
            )
        })
        .unwrap();
    assert!(guess.contains("\"Interstellar\""), "{guess}");

    let undone: Decided =
        serde_json::from_value(run(&lib.app, "changes.undo", json!({"changeset": cs})).await).unwrap();
    assert_eq!(undone.done, 3, "{undone:?}");
    assert_eq!(tree(&lib.root), before);
    assert_eq!(lib.path_of(file), "星际穿越4k.mp4");
    assert_eq!(lib.item_of_file(file), id);
    lib.catalog_matches_disk();
    let d: Value = run(&lib.app, "item.get", json!({"item": id})).await;
    assert_eq!(d["title"], "Interstellar", "what the agent set stays");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_films_own_folder_is_renamed_with_everything_in_it() {
    let lib = library(
        LibraryKind::Movies,
        &[
            ("the thing 1982 remaster/the.thing.1982.mkv", "film"),
            ("the thing 1982 remaster/poster.jpg", "jpg"),
            ("the thing 1982 remaster/the.thing.1982.nfo", "<movie/>"),
        ],
    )
    .await;
    let found: FindOutput = serde_json::from_value(run(&lib.app, "items.find", json!({})).await).unwrap();
    assert_eq!(found.items.len(), 1, "{found:?}");
    let id = found.items[0].id;
    run(
        &lib.app,
        "items.edit",
        json!({"summary": "t", "edits": [{"item": id, "set": {"title": "The Thing", "year": 1982}}]}),
    )
    .await;
    let plan: PlanOutput =
        serde_json::from_value(run(&lib.app, "files.plan", json!({"summary": "tidy", "items": [id]})).await)
            .unwrap();
    let cs = plan.changeset.as_ref().unwrap().id;
    let kinds: Vec<StepKind> = plan.steps.iter().map(|s| s.step).collect();
    assert_eq!(
        kinds,
        [
            StepKind::RenameFolder,
            StepKind::MoveVideo,
            StepKind::MoveCompanion
        ],
        "{plan:?}"
    );
    run(&lib.app, "changes.apply", json!({"changeset": cs})).await;
    assert_eq!(
        tree(&lib.root),
        [
            "The Thing (1982)/",
            "The Thing (1982)/The Thing (1982).mkv",
            "The Thing (1982)/The Thing (1982).nfo",
            "The Thing (1982)/poster.jpg",
        ]
    );
    lib.catalog_matches_disk();
    run(&lib.app, "changes.undo", json!({"changeset": cs})).await;
    assert_eq!(
        tree(&lib.root),
        [
            "the thing 1982 remaster/",
            "the thing 1982 remaster/poster.jpg",
            "the thing 1982 remaster/the.thing.1982.mkv",
            "the thing 1982 remaster/the.thing.1982.nfo",
        ]
    );
    lib.catalog_matches_disk();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plan_never_replaces_a_file_that_appeared_since() {
    let lib = library(LibraryKind::Movies, &[("Heat.1995.mkv", "mine")]).await;
    let id = lib.find("Heat").await;
    let plan: PlanOutput =
        serde_json::from_value(run(&lib.app, "files.plan", json!({"summary": "tidy", "items": [id]})).await)
            .unwrap();
    let cs = plan.changeset.unwrap().id;
    // Someone puts a file where the plan would.
    std::fs::create_dir_all(lib.root.join("Heat (1995)")).unwrap();
    std::fs::write(lib.root.join("Heat (1995)/Heat (1995).mkv"), "theirs").unwrap();
    let done: Decided =
        serde_json::from_value(run(&lib.app, "changes.apply", json!({"changeset": cs})).await).unwrap();
    assert_eq!(done.done, 0, "{done:?}");
    assert!(
        done.not_done.iter().any(|n| n.reason.contains("already there")),
        "{done:?}"
    );
    assert_eq!(
        std::fs::read_to_string(lib.root.join("Heat (1995)/Heat (1995).mkv")).unwrap(),
        "theirs"
    );
    assert_eq!(
        std::fs::read_to_string(lib.root.join("Heat.1995.mkv")).unwrap(),
        "mine"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_show_gets_its_title_and_season_folders() {
    let lib = library(
        LibraryKind::Shows,
        &[
            ("拾荒者统治/拾荒者统治 S01E01.mkv", "1"),
            ("拾荒者统治/拾荒者统治 S01E02.mkv", "2"),
            ("拾荒者统治/拾荒者统治 S01E02.ass", "sub"),
        ],
    )
    .await;
    let found: FindOutput = serde_json::from_value(run(&lib.app, "items.find", json!({})).await).unwrap();
    assert_eq!(found.items.len(), 1, "{found:?}");
    let show = found.items[0].id;
    let e1 = lib.file_at("拾荒者统治/拾荒者统治 S01E01.mkv");
    let episode = lib.item_of_file(e1);
    run(
        &lib.app,
        "items.edit",
        json!({"summary": "t", "edits": [
            {"item": show, "set": {"title": "Scavengers Reign", "year": 2023}},
            {"item": episode, "set": {"title": "The Signal"}}
        ]}),
    )
    .await;
    let plan: PlanOutput = serde_json::from_value(
        run(
            &lib.app,
            "files.plan",
            json!({"summary": "tidy", "items": [show]}),
        )
        .await,
    )
    .unwrap();
    let cs = plan.changeset.as_ref().unwrap().id;
    run(&lib.app, "changes.apply", json!({"changeset": cs})).await;
    assert_eq!(
        tree(&lib.root),
        [
            "Scavengers Reign (2023)/",
            "Scavengers Reign (2023)/Season 01/",
            "Scavengers Reign (2023)/Season 01/Scavengers Reign (2023) - S01E01 - The Signal.mkv",
            "Scavengers Reign (2023)/Season 01/Scavengers Reign (2023) - S01E02.ass",
            "Scavengers Reign (2023)/Season 01/Scavengers Reign (2023) - S01E02.mkv",
        ],
        "{plan:?}"
    );
    assert_eq!(lib.item_of_file(e1), episode, "the episode keeps its id");
    let found: FindOutput = serde_json::from_value(run(&lib.app, "items.find", json!({})).await).unwrap();
    assert_eq!(found.items.len(), 1);
    assert_eq!(found.items[0].id, show, "the show keeps its id");
    lib.catalog_matches_disk();
    run(&lib.app, "changes.undo", json!({"changeset": cs})).await;
    assert_eq!(
        tree(&lib.root),
        [
            "拾荒者统治/",
            "拾荒者统治/拾荒者统治 S01E01.mkv",
            "拾荒者统治/拾荒者统治 S01E02.ass",
            "拾荒者统治/拾荒者统治 S01E02.mkv",
        ]
    );
    lib.catalog_matches_disk();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_moves_a_file_where_it_says_and_bad_paths_are_refused() {
    let lib = library(LibraryKind::Movies, &[("downloads/x264-grp.mkv", "f")]).await;
    let file = lib.file_at("downloads/x264-grp.mkv");
    let plan: PlanOutput = serde_json::from_value(
        run(
            &lib.app,
            "files.plan",
            json!({"summary": "move", "moves": [
                {"file": file, "to": "Kids/Spirited Away (2001)/Spirited Away (2001).mkv"},
            ]}),
        )
        .await,
    )
    .unwrap();
    let kinds: Vec<StepKind> = plan.steps.iter().map(|s| s.step).collect();
    assert_eq!(
        kinds,
        [
            StepKind::NewFolder,
            StepKind::NewFolder,
            StepKind::MoveVideo,
            StepKind::RemoveFolder
        ],
        "{plan:?}"
    );
    run(
        &lib.app,
        "changes.apply",
        json!({"changeset": plan.changeset.unwrap().id}),
    )
    .await;
    assert_eq!(
        tree(&lib.root),
        [
            "Kids/",
            "Kids/Spirited Away (2001)/",
            "Kids/Spirited Away (2001)/Spirited Away (2001).mkv",
        ]
    );
    for (to, why) in [
        ("../x.mkv", "no empty"),
        ("C:/x.mkv", "relative"),
        ("Mission: Impossible.mkv", "Mission - Impossible"),
        ("x.avi", "extension"),
    ] {
        let plan: PlanOutput = serde_json::from_value(
            run(
                &lib.app,
                "files.plan",
                json!({"summary": "x", "moves": [{"file": file, "to": to}]}),
            )
            .await,
        )
        .unwrap();
        assert!(plan.changeset.is_none());
        assert!(plan.skipped[0].reason.contains(why), "{to}: {plan:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_viewer_may_plan_but_not_apply() {
    let lib = library(LibraryKind::Movies, &[("Heat.1995.mkv", "f")]).await;
    let id = lib.find("Heat").await;
    let viewer = Identity {
        role: flux_core::api::Role::Viewer,
        login_name: "v@example.com".into(),
        ..owner()
    };
    let (status, text) = op_as(
        &lib.app,
        viewer.clone(),
        "files.plan",
        json!({"summary": "x", "items": [id]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let plan: PlanOutput = serde_json::from_str(&text).unwrap();
    let cs = plan.changeset.unwrap().id;
    let (status, _) = op_as(&lib.app, viewer, "changes.apply", json!({"changeset": cs})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(lib.root.join("Heat.1995.mkv").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn files_fix_and_items_merge_over_http() {
    let lib = library(
        LibraryKind::Movies,
        &[("Interstellar.2014.mkv", "a"), ("星际穿越4k.mp4", "b")],
    )
    .await;
    let keep = lib.find("Interstellar").await;
    let other = lib.find("星际穿越").await;
    let dry: FixOutput = serde_json::from_value(
        run(
            &lib.app,
            "items.merge",
            json!({"summary": "One film", "into": keep, "from": [other], "dry_run": true}),
        )
        .await,
    )
    .unwrap();
    assert!(dry.changeset.is_none());
    assert_eq!(dry.files[0].after.as_ref().unwrap().item, Some(keep));
    let out: FixOutput = serde_json::from_value(
        run(
            &lib.app,
            "items.merge",
            json!({"summary": "One film", "into": keep, "from": [other]}),
        )
        .await,
    )
    .unwrap();
    let cs = out.changeset.unwrap().id;
    let found: FindOutput = serde_json::from_value(run(&lib.app, "items.find", json!({})).await).unwrap();
    assert_eq!(found.items.len(), 1);
    let d: Value = run(&lib.app, "item.get", json!({"item": keep})).await;
    assert_eq!(d["files"].as_array().unwrap().len(), 2);
    let fixed = d["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "星际穿越4k.mp4")
        .unwrap();
    assert_eq!(fixed["fix"]["of"], keep);
    run(&lib.app, "changes.undo", json!({"changeset": cs})).await;
    let found: FindOutput = serde_json::from_value(run(&lib.app, "items.find", json!({})).await).unwrap();
    assert_eq!(found.items.len(), 2);

    // A fix by hand: an ignored file.
    let file = lib.file_at("星际穿越4k.mp4");
    let out: FixOutput = serde_json::from_value(
        run(
            &lib.app,
            "files.fix",
            json!({"summary": "junk", "fixes": [{"file": file, "fix": {"kind": "ignore"}}]}),
        )
        .await,
    )
    .unwrap();
    assert!(out.files[0].after.as_ref().unwrap().item.is_none());
    let (status, text) = op_as(
        &lib.app,
        owner(),
        "files.fix",
        json!({"summary": "bad", "fixes": [{"file": file, "fix": {"year": 1200}}]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_merged_film_keeps_its_title_through_a_plan_and_its_undo() {
    let lib = library(
        LibraryKind::Movies,
        &[("Interstellar.2014.1080p.mkv", "a"), ("星际穿越4k.mp4", "b")],
    )
    .await;
    let keep = lib.find("Interstellar").await;
    let other = lib.find("星际穿越").await;
    run(
        &lib.app,
        "items.merge",
        json!({"summary": "One film", "into": keep, "from": [other]}),
    )
    .await;
    let plan: PlanOutput = serde_json::from_value(
        run(
            &lib.app,
            "files.plan",
            json!({"summary": "tidy", "items": [keep]}),
        )
        .await,
    )
    .unwrap();
    let cs = plan.changeset.unwrap().id;
    run(&lib.app, "changes.apply", json!({"changeset": cs})).await;
    run(&lib.app, "changes.undo", json!({"changeset": cs})).await;
    let d: Value = run(&lib.app, "item.get", json!({"item": keep})).await;
    assert_eq!(d["title"], "Interstellar", "the file merged in doesn't rename it");
    assert_eq!(d["files"].as_array().unwrap().len(), 2);
    lib.catalog_matches_disk();
}
