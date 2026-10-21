//! Setting up libraries: a folder another library already has is refused
//! however it is spelled (API and `--library` flags).

mod common;

use std::time::Instant;

use axum::http::StatusCode;
use common::*;
use flux_core::LibraryKind;
use flux_server::config::LibrarySpec;
use flux_server::{api::router, build_state, Config};
use serde_json::json;

/// A second library on the first one's folder, or inside or around it, is a
/// 400 that names the other library; spelled through `..`, a symlink or (on
/// case-insensitive volumes) another letter case too.
#[tokio::test]
async fn a_folder_of_another_library_is_a_bad_request() {
    let dir = scratch("roots-other-lib");
    let media = dir.path().join("media");
    let movies = media.join("Movies");
    let tv = media.join("TV");
    std::fs::create_dir_all(&movies).unwrap();
    std::fs::create_dir_all(&tv).unwrap();
    std::fs::create_dir_all(dir.path().join("other")).unwrap();
    let (st, _) = build_state(Config::for_tests(dir.path().join("data")), Instant::now(), None)
        .await
        .unwrap();
    let app = router(st);
    let create =
        |name: &str, root: &std::path::Path| json!({"name": name, "kind": "movies", "roots": [root]});
    let r = post(&app, "/api/v1/libraries", create("Movies", &movies)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());

    let mut same = vec![movies.clone(), dir.path().join("other/../media/Movies")];
    #[cfg(unix)]
    {
        let alias = dir.path().join("media-link");
        std::os::unix::fs::symlink(&media, &alias).unwrap();
        same.push(alias.join("Movies"));
    }
    let upper = dir.path().join("MEDIA").join("MOVIES");
    if upper.is_dir() {
        same.push(upper);
    }
    for root in same {
        let r = post(&app, "/api/v1/libraries", create("Films", &root)).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{root:?}: {}", r.text());
        let text = r.text();
        assert!(text.contains("is already in the library \"Movies\""), "{text}");
        assert!(text.contains("only one library"), "{text}");
    }
    let r = post(&app, "/api/v1/libraries", create("Around", &media)).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.text());
    assert!(r.text().contains("contains"), "{}", r.text());
    let r = post(&app, "/api/v1/libraries", create("Inside", &movies.join("4K"))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.text());
    assert!(r.text().contains("is inside"), "{}", r.text());
    // A neighbour is fine.
    let r = post(&app, "/api/v1/libraries", create("TV", &tv)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
}

/// `--library` flags (and config entries) go through the same check at
/// startup: a second name for the same folder stops the start with the
/// reason instead of cataloging every file twice.
#[tokio::test]
async fn library_flags_for_the_same_folder_stop_the_start() {
    let dir = scratch("roots-flags");
    let movies = dir.path().join("media").join("Movies");
    std::fs::create_dir_all(&movies).unwrap();
    std::fs::create_dir_all(dir.path().join("other")).unwrap();
    let spec = |name: &str, path: std::path::PathBuf| LibrarySpec {
        name: name.into(),
        kind: LibraryKind::Movies,
        path: path.to_string_lossy().into_owned(),
    };
    let mut cfg = Config::for_tests(dir.path().join("data"));
    cfg.libraries = vec![
        spec("Movies", movies.clone()),
        spec("Films", dir.path().join("other/../media/Movies")),
    ];
    let err = match build_state(cfg, Instant::now(), None).await {
        Ok(_) => panic!("started with the same folder in two libraries"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("library \"Films\""), "{err}");
    assert!(err.contains("is already in the library \"Movies\""), "{err}");
    assert!(err.contains("--library"), "{err}");

    // Restarting with the first flag alone works (the library exists by name).
    let mut cfg = Config::for_tests(dir.path().join("data"));
    cfg.libraries = vec![spec("Movies", movies)];
    let (st, _) = build_state(cfg, Instant::now(), None).await.unwrap();
    let libs = st.catalog.read(flux_catalog::read::list_libraries).await.unwrap();
    assert_eq!(libs.len(), 1);
}
