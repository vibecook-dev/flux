//! Live TMDB smoke test. Ignored by default; run with credentials:
//!
//! ```text
//! TMDB_READ_TOKEN=… cargo test -p flux-meta --test tmdb_live -- --ignored
//! ```

use flux_meta::matcher::{Candidate, Decision, MatchQuery, Matcher, MediaKind};
use flux_meta::tmdb::{MovieSearch, TmdbClient, TmdbError};

#[tokio::test]
#[ignore = "needs TMDB_READ_TOKEN or TMDB_API_KEY and network"]
async fn live_search_details_and_match() {
    let client = match TmdbClient::from_env() {
        Ok(c) => c,
        Err(TmdbError::NoCredentials) => {
            eprintln!("skipping: no TMDB credentials in the environment");
            return;
        }
        Err(e) => panic!("{e}"),
    };
    let page = client
        .search_movie(&MovieSearch {
            query: "The Matrix".into(),
            year: Some(1999),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(page.results.iter().any(|r| r.id == 603));
    let details = client.movie_details(603).await.unwrap();
    assert_eq!(details.imdb_id.as_deref(), Some("tt0133093"));
    let q = MatchQuery {
        kind: Some(MediaKind::Movie),
        titles: vec!["The Matrix".into()],
        year: Some(1999),
        duration_min: details.runtime_minutes().map(f64::from),
        ..Default::default()
    };
    let r = Matcher::default().match_candidates(&q, &[Candidate::from_tmdb_movie_details(&details)]);
    assert_eq!((r.decision, r.ids.tmdb), (Decision::Accept, Some(603)));
}
