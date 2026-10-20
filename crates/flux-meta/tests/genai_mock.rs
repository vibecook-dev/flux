//! GenAI poster pipeline against mock image APIs (no keys, no network).

use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use flux_core::api::ArtSource;
use flux_meta::ai::{OpenAiCompatible, OpenAiCompatibleConfig, SceneDescriber};
use flux_meta::art::genai::{
    png_bytes, GenAiPolicy, GenAiPosterPipeline, HttpImageApi, ImageGenRequest, OpenAiImages, PosterGenerator,
};
use flux_meta::art::{read_xmp, ArtError};
use flux_meta::jobs::{Budget, BudgetLimits};
use image::RgbImage;
use serde_json::{json, Value};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// A fake "generated" 1024×1536 image: a dusky gradient with a bright disc.
fn fake_art() -> Vec<u8> {
    let img = RgbImage::from_fn(1024, 1536, |x, y| {
        let d = ((x as f32 - 512.0).powi(2) + (y as f32 - 520.0).powi(2)).sqrt();
        if d < 180.0 {
            image::Rgb([250, 200, 120])
        } else {
            image::Rgb([(30 + y / 20) as u8, (20 + x / 30) as u8, 70])
        }
    });
    png_bytes(&img).unwrap()
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn openai_images(server: &MockServer) -> OpenAiImages {
    let mut g = OpenAiImages::new("sk-img").unwrap();
    g.base_url = format!("{}/v1", server.uri());
    g
}

#[tokio::test]
async fn openai_images_pipeline_typesets_labels_and_charges_budget() {
    let server = MockServer::start().await;
    let title = "Grandma's 80th Birthday";
    // The prompt must be text-free and must not contain the title.
    let clean_prompt = move |req: &Request| {
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        let p = body["prompt"].as_str().unwrap_or("");
        body["model"] == "gpt-image-1-mini"
            && body["size"] == "1024x1536"
            && p.contains("no text")
            && !p.to_lowercase().contains("grandma")
    };
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .and(header("authorization", "Bearer sk-img"))
        .and(clean_prompt)
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "created": 1, "data": [{ "b64_json": b64(&fake_art()) }] })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let budget = Budget::new(BudgetLimits {
        usd_per_day: 0.10,
        tokens_per_day: 1_000_000,
    });
    let pipeline = GenAiPosterPipeline::new(
        Arc::new(openai_images(&server)),
        None,
        Arc::clone(&budget),
        GenAiPolicy {
            enabled: true,
            ..Default::default()
        },
    );
    let out = pipeline
        .run(Path::new("/nonexistent/home-video.mov"), title, Some(2014))
        .await
        .unwrap();
    assert_eq!(out.poster.source, ArtSource::Generated);
    assert_eq!(out.poster.width * 3, out.poster.height * 2, "2:3 poster");
    let xmp = read_xmp(&out.poster.bytes).unwrap();
    assert!(xmp.contains("digitalsourcetype/trainedAlgorithmicMedia"), "{xmp}");
    assert!(xmp.contains("flux:model=\"openai/gpt-image-1-mini\""));
    assert!(xmp.contains(&out.prompt_hash));
    assert!((budget.spent_today().usd - 0.015).abs() < 1e-9);
    // Decodes, and the typeset title made the lower third brighter than the scrim alone.
    let img = image::load_from_memory(&out.poster.bytes).unwrap().to_rgb8();
    let bright_bottom = (img.height() * 3 / 4..img.height())
        .flat_map(|y| (0..img.width()).map(move |x| (x, y)))
        .filter(|&(x, y)| img.get_pixel(x, y).0[0] > 200)
        .count();
    assert!(bright_bottom > 1000, "title pixels: {bright_bottom}");
}

#[tokio::test]
async fn budget_exhausted_or_disabled_means_no_api_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let tiny = Budget::new(BudgetLimits {
        usd_per_day: 0.01,
        tokens_per_day: 1000,
    });
    let p = GenAiPosterPipeline::new(
        Arc::new(openai_images(&server)),
        None,
        tiny,
        GenAiPolicy {
            enabled: true,
            ..Default::default()
        },
    );
    assert!(matches!(
        p.run(Path::new("x.mkv"), "X", None).await,
        Err(ArtError::Budget(_))
    ));
    let off = GenAiPosterPipeline::new(
        Arc::new(openai_images(&server)),
        None,
        Budget::new(BudgetLimits::default()),
        GenAiPolicy::default(),
    );
    assert!(matches!(
        off.run(Path::new("x.mkv"), "X", None).await,
        Err(ArtError::Disabled)
    ));
}

#[tokio::test]
async fn generic_http_api_fal_and_replicate_shapes() {
    let server = MockServer::start().await;
    let art = fake_art();
    Mock::given(method("GET"))
        .and(path("/files/out.png"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(art.clone()))
        .mount(&server)
        .await;
    // fal style: images[].url
    Mock::given(method("POST"))
        .and(path("/fal-ai/flux/schnell"))
        .and(header("authorization", "Key fal-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "images": [{ "url": format!("{}/files/out.png", server.uri()), "width": 1024, "height": 1536 }], "seed": 7
        })))
        .mount(&server)
        .await;
    // replicate style: output = [data URI]
    Mock::given(method("POST"))
        .and(path("/v1/models/x/y/predictions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({ "status": "succeeded", "output": [format!("data:image/png;base64,{}", b64(&art))] }),
        ))
        .mount(&server)
        .await;
    let req = ImageGenRequest {
        prompt: "a lighthouse, no text".into(),
        negative_prompt: None,
        width: 1024,
        height: 1536,
        n: 1,
        seed: Some(7),
    };
    let fal = HttpImageApi::new(
        format!("{}/fal-ai/flux/schnell", server.uri()),
        Some(("Authorization".into(), "Key fal-test".into())),
        "flux-schnell",
        0.003,
    )
    .unwrap();
    let imgs = fal.generate(&req).await.unwrap();
    assert_eq!(imgs[0].bytes, art);
    let rep = HttpImageApi::new(
        format!("{}/v1/models/x/y/predictions", server.uri()),
        None,
        "z-image-turbo",
        0.0,
    )
    .unwrap();
    assert_eq!(rep.generate(&req).await.unwrap()[0].bytes, art);
    assert_eq!(rep.descriptor().model, "z-image-turbo");
}

/// With a VLM describer, the pipeline decodes frames from the user's own file
/// and the description (not the title) shapes the prompt.
#[tokio::test]
async fn vlm_description_from_local_frames_shapes_the_prompt() {
    // The corpus clip, else one made with ffmpeg.
    let Some(media) = flux_test_media::media("playback-research/bbb_1080_10s.mkv")
        .or_else(|| flux_test_media::clip(flux_test_media::Clip::H264AacMp4))
    else {
        return;
    };
    let media = media.as_path();
    let server = MockServer::start().await;
    let has_frames = |req: &Request| {
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        body["messages"][1]["content"].as_array().is_some_and(|parts| {
            parts.iter().any(|p| {
                p["image_url"]["url"]
                    .as_str()
                    .is_some_and(|u| u.starts_with("data:image/jpeg;base64,"))
            })
        })
    };
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(has_frames)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{ "message": { "role": "assistant", "content": json!({
                "setting": "a sunlit forest clearing with a giant tree", "era": "timeless", "mood": "whimsical",
                "palette": ["moss green", "amber", "sky blue"], "genre_guess": "animated comedy",
                "key_objects": ["an old tree", "a burrow", "mossy rocks"], "people_count": 0, "time_of_day": "morning"
            }).to_string() } }],
            "usage": { "prompt_tokens": 3000, "completion_tokens": 90 }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let described_prompt = |req: &Request| {
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        body["prompt"]
            .as_str()
            .is_some_and(|p| p.contains("sunlit forest clearing") && p.contains("moss green"))
    };
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .and(described_prompt)
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "data": [{ "b64_json": b64(&fake_art()) }] })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let mut vlm_cfg = OpenAiCompatibleConfig::ollama("qwen3-vl:4b");
    vlm_cfg.base_url = format!("{}/v1", server.uri());
    let vlm: Arc<dyn SceneDescriber> = Arc::new(OpenAiCompatible::new(vlm_cfg).unwrap());
    let pipeline = GenAiPosterPipeline::new(
        Arc::new(openai_images(&server)),
        Some(vlm),
        Budget::new(BudgetLimits::default()),
        GenAiPolicy {
            enabled: true,
            ..Default::default()
        },
    );
    let out = pipeline.run(media, "Big Buck Bunny", Some(2008)).await.unwrap();
    assert_eq!(
        out.description.as_ref().map(|d| d.mood.as_str()),
        Some("whimsical")
    );
    assert!(!out.prompt.contains("Bunny"));
}
