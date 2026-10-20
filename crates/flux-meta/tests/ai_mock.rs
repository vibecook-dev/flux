//! `OpenAiCompatible` against a local mock of the chat-completions API.

use std::path::Path;

use flux_core::{GuessKind, NameGuess};
use flux_meta::ai::{
    AiError, HypothesisKind, Hypothesizer, LocalEvidence, Locality, OpenAiCompatible, OpenAiCompatibleConfig,
};
use flux_meta::matcher::{Candidate, CandidateSource, MatchQuery, Matcher, MediaKind};
use serde_json::{json, Value};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn chat_response(content: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": "qwen3.5:4b",
        "choices": [{ "index": 0, "message": { "role": "assistant", "content": content }, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 612, "completion_tokens": 88, "total_tokens": 700 }
    }))
}

const PARASITE: &str = r#"{"kind":"movie","titles":[{"title":"Parasite","language":"en","original":false},{"title":"기생충","language":"ko","original":true}],"year":2019,"season":null,"episodes":[],"absolute_episode":null,"edition":null,"confidence":0.86,"rationale":"KOREAN tag and 2019 in the release name"}"#;

fn ollama(server: &MockServer) -> OpenAiCompatible {
    let mut cfg = OpenAiCompatibleConfig::ollama("qwen3.5:4b");
    cfg.base_url = format!("{}/v1", server.uri());
    OpenAiCompatible::new(cfg).unwrap()
}

fn evidence() -> LocalEvidence {
    let mut guess = NameGuess::unknown();
    guess.kind = GuessKind::Movie;
    guess.title = Some("Parasite".into());
    guess.year = Some(2019);
    guess.confidence = 0.4;
    LocalEvidence::from_path(Path::new(
        "Movies/Parasite.2019.KOREAN.1080p.BluRay.x264-NOGRP.mkv",
    ))
    .with_siblings([Path::new("Parasite.2019.KOREAN.1080p.BluRay.x264-NOGRP.nfo")])
    .with_parse(&guess)
}

#[tokio::test]
async fn structured_request_and_validated_hypothesis() {
    let server = MockServer::start().await;
    // The request must ask for strict JSON-schema output at temperature 0…
    let strict = json!({
        "model": "qwen3.5:4b",
        "temperature": 0.0,
        "response_format": { "type": "json_schema", "json_schema": { "name": "flux_media_hypothesis", "strict": true } }
    });
    // …and carry only local evidence in the user message.
    let only_local = |req: &Request| {
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        let user = body["messages"][1]["content"].as_str().unwrap_or("");
        let ev: Value = serde_json::from_str(user).unwrap_or(Value::Null);
        body["messages"][0]["role"] == "system"
            && ev["path"] == "Movies/Parasite.2019.KOREAN.1080p.BluRay.x264-NOGRP.mkv"
            && ev["parse"]["title"] == "Parasite"
            && ev.as_object().is_some_and(|o| {
                o.keys()
                    .all(|k| ["path", "siblings", "probe", "parse", "ocr"].contains(&k.as_str()))
            })
            && body["response_format"]["json_schema"]["schema"]["required"]
                .as_array()
                .is_some_and(|r| r.len() == 9)
    };
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(strict))
        .and(only_local)
        .respond_with(chat_response(PARASITE))
        .expect(1)
        .mount(&server)
        .await;
    let ai = ollama(&server);
    assert_eq!(ai.descriptor().locality, Locality::Local);
    let out = ai.hypothesize(&evidence()).await.unwrap();
    assert_eq!(out.hypothesis.kind, HypothesisKind::Movie);
    assert_eq!(out.hypothesis.titles[1].title, "기생충");
    assert!(out.hypothesis.titles[1].original);
    assert_eq!(out.hypothesis.year, Some(2019));
    assert_eq!((out.usage.input_tokens, out.usage.output_tokens), (612, 88));
    assert_eq!(out.usd, 0.0, "local models cost nothing");

    // The hypothesis only adds query titles; the deterministic matcher decides.
    // Offline export candidates know only original titles (기생충), which the
    // parser's "Parasite" alone could not reach.
    let base = MatchQuery {
        kind: Some(MediaKind::Movie),
        titles: vec!["Parasite".into()],
        year: Some(2019),
        ..Default::default()
    };
    let q = out.hypothesis.refine(base);
    let mut target = Candidate::from_export(&flux_meta::export_index::ExportCandidate {
        id: 496243,
        kind: flux_meta::export_index::ExportKind::Movie,
        title: "기생충".into(),
        popularity: 39.9,
        similarity: 1.0,
        hit: flux_meta::export_index::HitKind::Exact,
    });
    target.source = CandidateSource::TmdbExport;
    let decoy = Candidate {
        ids: flux_core::ProviderIds {
            tmdb: Some(48311),
            ..Default::default()
        },
        title: "parasite".into(),
        popularity: Some(3.1),
        ..target.clone()
    };
    let r = Matcher::default().match_candidates(&q, &[decoy, target]);
    assert_eq!(r.ids.tmdb, Some(496243), "{r:#?}");
}

#[tokio::test]
async fn cloud_preset_sends_bearer_and_prices_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-test"))
        .and(body_partial_json(json!({ "max_completion_tokens": 400 })))
        .respond_with(chat_response(&format!("```json\n{PARASITE}\n```")))
        .mount(&server)
        .await;
    let mut cfg = OpenAiCompatibleConfig::openai("gpt-5-nano", "sk-test");
    cfg.base_url = format!("{}/v1", server.uri());
    let ai = OpenAiCompatible::new(cfg).unwrap();
    let out = ai.hypothesize(&evidence()).await.unwrap();
    // 612 in × $0.05/M + 88 out × $0.40/M
    assert!(
        (out.usd - (612.0 * 0.05 + 88.0 * 0.40) / 1e6).abs() < 1e-12,
        "{}",
        out.usd
    );
    assert_eq!(ai.descriptor().locality, Locality::Cloud);
}

#[tokio::test]
async fn refusals_bad_json_and_http_errors_are_typed() {
    let server = MockServer::start().await;
    let ai = ollama(&server);
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{ "message": { "role": "assistant", "content": null, "refusal": "I can't help with that." } }]
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    assert!(matches!(
        ai.hypothesize(&evidence()).await,
        Err(AiError::Refusal(_))
    ));
    server.reset().await;
    Mock::given(method("POST"))
        .respond_with(chat_response("The movie is Parasite."))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    assert!(matches!(
        ai.hypothesize(&evidence()).await,
        Err(AiError::Schema(_))
    ));
    server.reset().await;
    // Valid JSON, but a movie with no titles is rejected by validation.
    Mock::given(method("POST"))
        .respond_with(chat_response(r#"{"kind":"movie","titles":[],"year":null,"season":null,"episodes":[],"absolute_episode":null,"edition":null,"confidence":0.2,"rationale":"?"}"#))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    assert!(matches!(
        ai.hypothesize(&evidence()).await,
        Err(AiError::Schema(_))
    ));
    server.reset().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).set_body_string("model loading"))
        .mount(&server)
        .await;
    match ai.hypothesize(&evidence()).await {
        Err(AiError::Status { status, body }) => assert_eq!((status, body.as_str()), (503, "model loading")),
        other => panic!("{other:?}"),
    }
}

/// Real local smoke test: needs `ollama serve` and a pulled model, e.g.
/// `FLUX_OLLAMA_MODEL=qwen3.5:4b cargo test -p flux-meta --test ai_mock -- --ignored`.
#[tokio::test]
#[ignore = "needs a running Ollama with a pulled model"]
async fn ollama_smoke() {
    let model = std::env::var("FLUX_OLLAMA_MODEL").unwrap_or_else(|_| "qwen3.5:4b".into());
    let ai = OpenAiCompatible::new(OpenAiCompatibleConfig::ollama(model)).unwrap();
    let ev = LocalEvidence::from_path(Path::new(
        "Movies/Blade.Runner.2049.2017.2160p.UHD.BluRay.x265-TERMiNAL.mkv",
    ));
    let out = ai.hypothesize(&ev).await.unwrap();
    eprintln!("{:#?}", out);
    assert!(out.hypothesis.titles.iter().any(|t| t.title.contains("2049")));
}
