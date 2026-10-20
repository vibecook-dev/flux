//! TMDB client tests against a local mock server serving hand-written fixtures
//! modelled on TMDB's documented response shapes. No network, no key.

use std::time::{Duration, Instant};

use flux_meta::matcher::{Candidate, Decision, MatchQuery, Matcher, MediaKind};
use flux_meta::tmdb::{
    attribution, DiskCache, ExternalSource, MovieSearch, TmdbAuth, TmdbClient, TmdbConfig, TmdbError,
};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/tmdb/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn json(name: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(fixture(name), "application/json")
}

fn config(server: &MockServer, auth: TmdbAuth) -> TmdbConfig {
    let mut cfg = TmdbConfig::new(auth);
    cfg.base_url = format!("{}/3", server.uri());
    cfg.backoff_base = Duration::from_millis(10);
    cfg
}

fn bearer(server: &MockServer) -> TmdbClient {
    TmdbClient::new(config(server, TmdbAuth::ReadToken("test-token".into()))).unwrap()
}

#[tokio::test]
async fn search_movie_with_bearer_token_and_match() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/3/search/movie"))
        .and(query_param("query", "The Matrix"))
        .and(query_param("year", "1999"))
        .and(query_param("language", "en-US"))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(json("search_movie_matrix.json"))
        .expect(1)
        .mount(&server)
        .await;
    let client = bearer(&server);
    let page = client
        .search_movie(&MovieSearch {
            query: "The Matrix".into(),
            year: Some(1999),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.total_results, 3);
    assert_eq!(page.results[0].id, 603);
    assert_eq!(page.results[0].title.as_str(), "The Matrix");
    assert_eq!(page.results[0].year(), Some(1999));
    // TMDB sends unknown dates as "".
    assert_eq!(page.results[1].release_date, None);
    assert_eq!(page.results[1].year(), None);

    // Search results feed the deterministic matcher directly.
    let cands: Vec<Candidate> = page.results.iter().map(Candidate::from_tmdb_movie).collect();
    let q = MatchQuery {
        kind: Some(MediaKind::Movie),
        titles: vec!["The Matrix".into()],
        year: Some(1999),
        ..Default::default()
    };
    let r = Matcher::default().match_candidates(&q, &cands);
    assert_eq!(r.ids.tmdb, Some(603));
    // No runtime yet (search results have none): review, not accept.
    assert_eq!(r.decision, Decision::Review);
}

#[tokio::test]
async fn api_key_goes_in_the_query_string() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/3/search/tv"))
        .and(query_param("api_key", "v3key"))
        .and(query_param("first_air_date_year", "2008"))
        .respond_with(json("search_tv_breaking_bad.json"))
        .expect(1)
        .mount(&server)
        .await;
    let client = TmdbClient::new(config(&server, TmdbAuth::ApiKey("v3key".into()))).unwrap();
    let page = client.search_tv("Breaking Bad", Some(2008), 1).await.unwrap();
    assert_eq!(page.results[0].id, 1396);
    assert_eq!(page.results[0].year(), Some(2008));
}

#[tokio::test]
async fn movie_details_append_to_response_and_runtime_accept() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/3/movie/603"))
        .and(query_param(
            "append_to_response",
            "images,external_ids,release_dates",
        ))
        .respond_with(json("movie_603.json"))
        .mount(&server)
        .await;
    let d = bearer(&server).movie_details(603).await.unwrap();
    assert_eq!(d.runtime_minutes(), Some(136));
    assert_eq!(d.imdb_id.as_deref(), Some("tt0133093"));
    assert_eq!(
        d.external_ids.as_ref().and_then(|e| e.wikidata_id.as_deref()),
        Some("Q83495")
    );
    assert_eq!(d.images.as_ref().map(|i| i.posters.len()), Some(1));
    assert_eq!(
        d.images.as_ref().map(|i| i.backdrops[0].iso_639_1.is_none()),
        Some(true)
    );
    let us = d
        .release_dates
        .as_ref()
        .unwrap()
        .results
        .iter()
        .find(|r| r.iso_3166_1 == "US")
        .unwrap();
    assert_eq!(us.release_dates[0].certification, "R");
    assert_eq!(d.tagline.as_str(), "Welcome to the Real World.");

    // With details (runtime) and a probed duration, the matcher auto-accepts.
    let q = MatchQuery {
        kind: Some(MediaKind::Movie),
        titles: vec!["The Matrix".into()],
        year: Some(1999),
        duration_min: Some(136.3),
        audio_languages: vec!["en".into()],
        ..Default::default()
    };
    let r = Matcher::default().match_candidates(&q, &[Candidate::from_tmdb_movie_details(&d)]);
    assert_eq!(
        (r.decision, r.ids.tmdb, r.ids.imdb.as_deref()),
        (Decision::Accept, Some(603), Some("tt0133093"))
    );
}

#[tokio::test]
async fn tv_details_season_and_episode_existence() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/3/tv/1396"))
        .respond_with(json("tv_1396.json"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/3/tv/1396/season/1"))
        .respond_with(json("tv_1396_season_1.json"))
        .mount(&server)
        .await;
    let client = bearer(&server);
    let tv = client.tv_details(1396).await.unwrap();
    assert_eq!(tv.seasons.len(), 6);
    assert_eq!(tv.external_ids.as_ref().and_then(|e| e.tvdb_id), Some(81189));
    let s1 = client.tv_season(1396, 1).await.unwrap();
    assert_eq!(s1.episodes.len(), 3);
    assert_eq!(s1.episodes[0].name.as_str(), "Pilot");
    assert_eq!(s1.episodes[2].episode_type.as_deref(), Some("finale"));

    let cand = Candidate::from_tmdb_tv_details(&tv);
    let m = Matcher::default();
    let mut q = MatchQuery {
        kind: Some(MediaKind::Tv),
        titles: vec!["Breaking Bad".into()],
        year: Some(2008),
        season: Some(5),
        episodes: vec![16],
        duration_min: Some(46.0),
        audio_languages: vec!["en".into()],
        ..Default::default()
    };
    let r = m.match_candidates(&q, std::slice::from_ref(&cand));
    assert_eq!(r.decision, Decision::Accept, "{r:#?}");
    // S05E17 does not exist: no longer an auto-accept.
    q.episodes = vec![17];
    let r = m.match_candidates(&q, &[cand]);
    assert_ne!(r.decision, Decision::Accept, "{r:#?}");
}

#[tokio::test]
async fn find_by_imdb_id_and_configuration() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/3/find/tt0133093"))
        .and(query_param("external_source", "imdb_id"))
        .respond_with(json("find_tt0133093.json"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/3/configuration"))
        .respond_with(json("configuration.json"))
        .mount(&server)
        .await;
    let client = bearer(&server);
    let found = client.find("tt0133093", ExternalSource::Imdb).await.unwrap();
    assert_eq!(found.movie_results[0].id, 603);
    assert!(found.tv_results.is_empty());

    // An {imdb-tt…} path id resolves through /find and then short-circuits the matcher.
    let mut cand = Candidate::from_tmdb_movie(&found.movie_results[0]);
    cand.ids.imdb = Some("tt0133093".into());
    let mut q = MatchQuery {
        titles: vec!["matrix".into()],
        ..Default::default()
    };
    q.ids.imdb = Some("tt0133093".into());
    let r = Matcher::default().match_candidates(&q, &[cand]);
    assert_eq!((r.decision, r.ids.tmdb), (Decision::Accept, Some(603)));

    let conf = client.configuration().await.unwrap();
    assert_eq!(
        conf.images.url("w500", "/p96dm7sCMn4VYAStA6siNz30G1r.jpg"),
        "https://image.tmdb.org/t/p/w500/p96dm7sCMn4VYAStA6siNz30G1r.jpg"
    );
}

#[tokio::test]
async fn unauthorized_and_not_found_are_typed_errors() {
    let server = MockServer::start().await;
    Mock::given(path("/3/movie/1"))
        .respond_with(ResponseTemplate::new(401).set_body_raw(fixture("error_401.json"), "application/json"))
        .mount(&server)
        .await;
    Mock::given(path("/3/movie/2"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let client = bearer(&server);
    match client.movie_details(1).await {
        Err(TmdbError::Unauthorized(msg)) => assert!(msg.contains("Invalid API key"), "{msg}"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        client.movie_details(2).await,
        Err(TmdbError::NotFound(_))
    ));
}

#[tokio::test]
async fn retries_429_honouring_retry_after() {
    let server = MockServer::start().await;
    Mock::given(path("/3/movie/603"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "1")
                .set_body_raw(fixture("error_429.json"), "application/json"),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(path("/3/movie/603"))
        .respond_with(json("movie_603.json"))
        .with_priority(2)
        .mount(&server)
        .await;
    let client = bearer(&server);
    let t = Instant::now();
    let d = client.movie_details(603).await.unwrap();
    assert_eq!(d.id, 603);
    assert!(
        t.elapsed() >= Duration::from_millis(950),
        "Retry-After ignored: {:?}",
        t.elapsed()
    );
    let s = client.stats();
    assert_eq!((s.requests, s.retries), (2, 1));
}

#[tokio::test]
async fn gives_up_after_max_retries_and_retries_5xx() {
    let server = MockServer::start().await;
    Mock::given(path("/3/movie/1"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;
    Mock::given(path("/3/movie/603"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(path("/3/movie/603"))
        .respond_with(json("movie_603.json"))
        .with_priority(2)
        .mount(&server)
        .await;
    let mut cfg = config(&server, TmdbAuth::ReadToken("t".into()));
    cfg.max_retries = 2;
    let client = TmdbClient::new(cfg).unwrap();
    assert!(matches!(
        client.movie_details(1).await,
        Err(TmdbError::RateLimited(2))
    ));
    assert_eq!(client.movie_details(603).await.unwrap().id, 603);
}

#[tokio::test]
async fn disk_cache_serves_repeats_and_never_stores_credentials() {
    let server = MockServer::start().await;
    Mock::given(path("/3/movie/603"))
        .respond_with(json("movie_603.json"))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(&server, TmdbAuth::ApiKey("secret-v3-key".into()));
    cfg.cache = Some(DiskCache::new(dir.path(), Duration::from_secs(7 * 86_400)));
    let client = TmdbClient::new(cfg).unwrap();
    for _ in 0..3 {
        assert_eq!(
            client.movie_details(603).await.unwrap().runtime_minutes(),
            Some(136)
        );
    }
    let s = client.stats();
    assert_eq!((s.requests, s.cache_hits), (1, 2));
    // The cached files must not contain the key.
    for shard in std::fs::read_dir(dir.path()).unwrap().flatten() {
        for f in std::fs::read_dir(shard.path()).unwrap().flatten() {
            let body = std::fs::read_to_string(f.path()).unwrap();
            assert!(!body.contains("secret-v3-key"));
        }
    }
}

#[tokio::test]
async fn client_side_rate_limit_is_enforced() {
    let server = MockServer::start().await;
    Mock::given(path("/3/configuration"))
        .respond_with(json("configuration.json"))
        .mount(&server)
        .await;
    let mut cfg = config(&server, TmdbAuth::ReadToken("t".into()));
    cfg.requests_per_second = 10.0;
    cfg.burst = 1;
    let client = TmdbClient::new(cfg).unwrap();
    let t = Instant::now();
    for _ in 0..6 {
        client.configuration().await.unwrap();
    }
    // 1 burst + 5 more at 10/s ≥ 0.5 s.
    assert!(t.elapsed() >= Duration::from_millis(480), "{:?}", t.elapsed());
}

#[test]
fn attribution_and_terms() {
    assert!(attribution::NOTICE.contains("not endorsed, certified, or otherwise approved by TMDB"));
    const { assert!(!attribution::TMDB_TERMS.ai_allowed) };
    assert!(attribution::TMDB_TERMS.max_cache_age <= Duration::from_secs(183 * 86_400));
    // Secrets never appear in Debug output.
    let dbg = format!("{:?}", TmdbAuth::ApiKey("supersecret".into()));
    assert!(!dbg.contains("supersecret"));
}
