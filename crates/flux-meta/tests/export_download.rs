//! The export downloader walks back a day when today's file is not published
//! yet (S3 answers 403 before ~08:00 UTC), and never downloads twice.

use std::io::Write;

use flux_meta::export_index::{
    download_latest_from, export_file_name, BuildOptions, ExportError, ExportIndex, ExportKind,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TODAY: u64 = 20_745; // 2026-10-19

fn gz(body: &str) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(body.as_bytes()).unwrap();
    e.finish().unwrap()
}

#[tokio::test]
async fn falls_back_to_yesterday_and_caches() {
    let server = MockServer::start().await;
    let today = format!("/{}", export_file_name(ExportKind::Movie, TODAY));
    let yesterday = format!("/{}", export_file_name(ExportKind::Movie, TODAY - 1));
    assert_eq!(yesterday, "/movie_ids_10_18_2026.json.gz");
    Mock::given(method("GET"))
        .and(path(today.as_str()))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&server)
        .await;
    let body = gz("{\"adult\":false,\"id\":603,\"original_title\":\"The Matrix\",\"popularity\":48.1,\"video\":false}\n");
    Mock::given(method("GET"))
        .and(path(yesterday.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let http = reqwest::Client::new();
    let p = download_latest_from(&http, &server.uri(), dir.path(), ExportKind::Movie, 3, TODAY)
        .await
        .unwrap();
    assert!(p.ends_with("movie_ids_10_18_2026.json.gz"));
    // Second call: today's file is still missing (403, not mocked again → 404), yesterday's is on disk.
    server.reset().await;
    let p2 = download_latest_from(&http, &server.uri(), dir.path(), ExportKind::Movie, 3, TODAY)
        .await
        .unwrap();
    assert_eq!(p, p2);
    let idx = ExportIndex::build_from_gz(&p, ExportKind::Movie, BuildOptions::default()).unwrap();
    assert_eq!(idx.lookup_exact("The Matrix")[0].id, 603);
}

#[tokio::test]
async fn reports_when_nothing_is_published() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let err = download_latest_from(
        &reqwest::Client::new(),
        &server.uri(),
        dir.path(),
        ExportKind::Tv,
        1,
        TODAY,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            ExportError::NotPublished {
                kind: ExportKind::Tv,
                days: 2,
                status: 403
            }
        ),
        "{err}"
    );
}
