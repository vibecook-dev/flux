//! Opt-in generative posters (research/05 §5): text-free key art from a model,
//! flux's own typography on top, provenance embedded, under a budget.
//!
//! ```text
//! frames (user's own file) ──► optional VLM description (SceneDescriber, local evidence only)
//!   ──► deterministic prompt (template × palette × mood; never a title, studio or person;
//!       "no text, no letters, no logos")
//!   ──► PosterGenerator (OpenAI Images / generic fal-/replicate-style HTTP API)
//!   ──► cover-crop to 2:3 ──► flux typography ──► JPEG + XMP DigitalSourceType=trainedAlgorithmicMedia
//!   ──► ArtSource::Generated (the UI always badges it "AI-generated")
//! ```
//!
//! Guardrails:
//! - Off unless the library opted in ([`GenAiPolicy::enabled`]).
//! - Never sends TMDB text or TMDB images to a model (TMDB ToS §1.C): the
//!   only inputs are the user's frames and the deterministic template.
//! - The title is never put in the prompt (no imitation of real posters).
//! - Budget: a [`crate::jobs::Budget`] reservation per poster ($/day cap) and a
//!   semaphore for max concurrent generations.

use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use flux_core::api::ArtSource;
use futures::future::BoxFuture;
use image::RgbImage;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Semaphore;

use super::encode::{encode_jpeg, ArtImage, DigitalSourceType, Provenance};
use super::{compose, crop, harvest, ArtError, FramePosterOptions, Typesetter};
use crate::ai::{SceneDescriber, SceneDescription};
use crate::jobs::{Budget, Cost};
use crate::util::hash_hex;

/// Bump when the prompt template changes (part of the job idempotency key).
pub const PROMPT_TEMPLATE_VERSION: u32 = 1;

/// A text-to-image request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageGenRequest {
    pub prompt: String,
    pub negative_prompt: Option<String>,
    pub width: u32,
    pub height: u32,
    pub n: u32,
    pub seed: Option<u64>,
}

/// One generated image (PNG/JPEG/WebP bytes as returned).
#[derive(Debug, Clone, PartialEq)]
pub struct GeneratedImage {
    pub bytes: Vec<u8>,
    pub revised_prompt: Option<String>,
}

/// Generator identity, price and weights/terms note (for the policy gate and the XMP).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeneratorDescriptor {
    pub provider: String,
    pub model: String,
    /// List price per image at the requested size/quality (USD).
    pub usd_per_image: f64,
    /// Licence of the weights or the API terms governing outputs.
    pub terms: String,
}

/// A text-to-image backend.
pub trait PosterGenerator: Send + Sync {
    fn descriptor(&self) -> GeneratorDescriptor;
    fn generate<'a>(
        &'a self,
        req: &'a ImageGenRequest,
    ) -> BoxFuture<'a, Result<Vec<GeneratedImage>, ArtError>>;
}

/// OpenAI Images API (`POST /v1/images/generations`, gpt-image models; BYO key).
#[derive(Debug, Clone)]
pub struct OpenAiImages {
    pub base_url: String,
    api_key: String,
    pub model: String,
    /// `low` | `medium` | `high`.
    pub quality: String,
    /// List price per portrait image at `quality` (research/05 §5.2:
    /// gpt-image-1-mini medium $0.015, gpt-image-2 medium $0.041).
    pub usd_per_image: f64,
    http: reqwest::Client,
}

impl OpenAiImages {
    /// `gpt-image-1-mini`, medium quality ($0.015 per 1024×1536 image).
    pub fn new(api_key: impl Into<String>) -> Result<Self, ArtError> {
        Ok(OpenAiImages {
            base_url: "https://api.openai.com/v1".into(),
            api_key: api_key.into(),
            model: "gpt-image-1-mini".into(),
            quality: "medium".into(),
            usd_per_image: 0.015,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(180))
                .build()?,
        })
    }
}

#[derive(Deserialize)]
struct OpenAiImagesResponse {
    #[serde(default)]
    data: Vec<OpenAiImageDatum>,
}

#[derive(Deserialize)]
struct OpenAiImageDatum {
    #[serde(default)]
    b64_json: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    revised_prompt: Option<String>,
}

/// gpt-image sizes: portrait is 1024×1536.
fn openai_size(w: u32, h: u32) -> &'static str {
    if h > w {
        "1024x1536"
    } else if w > h {
        "1536x1024"
    } else {
        "1024x1024"
    }
}

impl PosterGenerator for OpenAiImages {
    fn descriptor(&self) -> GeneratorDescriptor {
        GeneratorDescriptor {
            provider: "openai".into(),
            model: self.model.clone(),
            usd_per_image: self.usd_per_image,
            terms: "OpenAI API terms; outputs carry C2PA metadata".into(),
        }
    }

    fn generate<'a>(
        &'a self,
        req: &'a ImageGenRequest,
    ) -> BoxFuture<'a, Result<Vec<GeneratedImage>, ArtError>> {
        Box::pin(async move {
            let body = json!({
                "model": self.model,
                "prompt": req.prompt,
                "n": req.n.max(1),
                "size": openai_size(req.width, req.height),
                "quality": self.quality,
                "output_format": "png",
            });
            let url = format!("{}/images/generations", self.base_url.trim_end_matches('/'));
            let resp = self
                .http
                .post(url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await?;
            let status = resp.status();
            if !status.is_success() {
                let t = resp.text().await.unwrap_or_default();
                return Err(ArtError::Generator(format!(
                    "HTTP {status}: {}",
                    t.chars().take(300).collect::<String>()
                )));
            }
            let r: OpenAiImagesResponse = resp.json().await?;
            let mut out = Vec::new();
            for d in r.data {
                let bytes = match (d.b64_json, d.url) {
                    (Some(b), _) => decode_b64(&b)?,
                    (None, Some(u)) => fetch_image(&self.http, &u).await?,
                    _ => continue,
                };
                out.push(GeneratedImage {
                    bytes,
                    revised_prompt: d.revised_prompt,
                });
            }
            Ok(out)
        })
    }
}

/// A generic JSON image API in the style of fal.ai / Replicate / BFL:
/// `POST url` with `{prompt, negative_prompt?, width, height, num_images, seed?}`
/// (plus any fixed `extra` fields); the response may carry images as
/// `images[].url`, `images[].b64_json`, `data[].b64_json`/`url`, or `output`
/// (a URL string or list). URLs may be `data:` URIs.
#[derive(Debug, Clone)]
pub struct HttpImageApi {
    pub url: String,
    /// e.g. `("Authorization", "Key …")` for fal, `("Authorization", "Bearer …")` for Replicate.
    auth: Option<(String, String)>,
    pub model: String,
    pub usd_per_image: f64,
    pub terms: String,
    /// Extra fixed request fields (e.g. `{"num_inference_steps": 4}`).
    pub extra: serde_json::Map<String, serde_json::Value>,
    http: reqwest::Client,
}

impl HttpImageApi {
    /// An endpoint with an optional auth header.
    pub fn new(
        url: impl Into<String>,
        auth: Option<(String, String)>,
        model: impl Into<String>,
        usd_per_image: f64,
    ) -> Result<Self, ArtError> {
        Ok(HttpImageApi {
            url: url.into(),
            auth,
            model: model.into(),
            usd_per_image,
            terms: "provider API terms".into(),
            extra: serde_json::Map::new(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(180))
                .build()?,
        })
    }
}

fn decode_b64(s: &str) -> Result<Vec<u8>, ArtError> {
    let payload = s.split_once("base64,").map_or(s, |(_, b)| b);
    base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .map_err(|e| ArtError::Decode(e.to_string()))
}

async fn fetch_image(http: &reqwest::Client, url: &str) -> Result<Vec<u8>, ArtError> {
    if url.starts_with("data:") {
        return decode_b64(url);
    }
    let resp = http.get(url).send().await?;
    if !resp.status().is_success() {
        return Err(ArtError::Generator(format!(
            "image download HTTP {}",
            resp.status()
        )));
    }
    Ok(resp.bytes().await?.to_vec())
}

impl PosterGenerator for HttpImageApi {
    fn descriptor(&self) -> GeneratorDescriptor {
        GeneratorDescriptor {
            provider: "http".into(),
            model: self.model.clone(),
            usd_per_image: self.usd_per_image,
            terms: self.terms.clone(),
        }
    }

    fn generate<'a>(
        &'a self,
        req: &'a ImageGenRequest,
    ) -> BoxFuture<'a, Result<Vec<GeneratedImage>, ArtError>> {
        Box::pin(async move {
            let mut body = serde_json::Map::new();
            body.insert("prompt".into(), json!(req.prompt));
            if let Some(n) = &req.negative_prompt {
                body.insert("negative_prompt".into(), json!(n));
            }
            body.insert("width".into(), json!(req.width));
            body.insert("height".into(), json!(req.height));
            body.insert("num_images".into(), json!(req.n.max(1)));
            if let Some(s) = req.seed {
                body.insert("seed".into(), json!(s));
            }
            for (k, v) in &self.extra {
                body.insert(k.clone(), v.clone());
            }
            let mut r = self.http.post(&self.url).json(&body);
            if let Some((k, v)) = &self.auth {
                r = r.header(k.as_str(), v.as_str());
            }
            let resp = r.send().await?;
            let status = resp.status();
            if !status.is_success() {
                let t = resp.text().await.unwrap_or_default();
                return Err(ArtError::Generator(format!(
                    "HTTP {status}: {}",
                    t.chars().take(300).collect::<String>()
                )));
            }
            let v: serde_json::Value = resp.json().await?;
            let mut refs: Vec<String> = Vec::new();
            for key in ["images", "data"] {
                for item in v[key].as_array().into_iter().flatten() {
                    if let Some(s) = item.as_str() {
                        refs.push(s.to_owned());
                    } else if let Some(b) = item["b64_json"].as_str().or_else(|| item["b64"].as_str()) {
                        refs.push(format!("data:image/png;base64,{b}"));
                    } else if let Some(u) = item["url"].as_str() {
                        refs.push(u.to_owned());
                    }
                }
            }
            match &v["output"] {
                serde_json::Value::String(s) => refs.push(s.clone()),
                serde_json::Value::Array(a) => {
                    refs.extend(a.iter().filter_map(|x| x.as_str().map(str::to_owned)))
                }
                _ => {}
            }
            if refs.is_empty() {
                return Err(ArtError::Generator("response contained no images".into()));
            }
            let mut out = Vec::new();
            for r in refs {
                out.push(GeneratedImage {
                    bytes: fetch_image(&self.http, &r).await?,
                    revised_prompt: None,
                });
            }
            Ok(out)
        })
    }
}

/// Words that must never reach an image prompt (imitation / text / likeness risk).
const DENY: &[&str] = &[
    "text",
    "title",
    "logo",
    "letter",
    "letters",
    "word",
    "words",
    "typography",
    "caption",
    "watermark",
    "signature",
    "poster of",
    "movie poster",
    "in the style of",
    "celebrity",
    "actor",
    "actress",
    "brand",
];

fn clean_phrase(s: &str, max_chars: usize) -> String {
    let lower = s.to_lowercase();
    if DENY.iter().any(|d| lower.contains(d)) {
        return String::new();
    }
    s.chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | ',' | '\''))
        .take(max_chars)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Build the deterministic, text-free key-art prompt. The title is deliberately
/// *not* an input: generated art must not imitate a real film's poster.
pub fn build_prompt(desc: Option<&SceneDescription>) -> (String, String) {
    let mut parts: Vec<String> = vec![
        "Cinematic key art illustration, vertical 2:3 composition".into(),
        "a single strong focal subject in the upper two-thirds".into(),
        "the lower third calm, dark and empty to leave room for a title".into(),
    ];
    if let Some(d) = desc {
        let setting = clean_phrase(&d.setting, 80);
        if !setting.is_empty() {
            parts.push(format!("set in {setting}"));
        }
        let era = clean_phrase(&d.era, 40);
        if !era.is_empty() {
            parts.push(format!("{era} era"));
        }
        let mood = clean_phrase(&d.mood, 40);
        if !mood.is_empty() {
            parts.push(format!("{mood} mood"));
        }
        let tod = clean_phrase(&d.time_of_day, 20);
        if !tod.is_empty() {
            parts.push(format!("{tod} light"));
        }
        let objs: Vec<String> = d
            .key_objects
            .iter()
            .map(|o| clean_phrase(o, 30))
            .filter(|o| !o.is_empty())
            .take(4)
            .collect();
        if !objs.is_empty() {
            parts.push(format!("featuring {}", objs.join(", ")));
        }
        let pal: Vec<String> = d
            .palette
            .iter()
            .map(|o| clean_phrase(o, 20))
            .filter(|o| !o.is_empty())
            .take(5)
            .collect();
        if !pal.is_empty() {
            parts.push(format!("colour palette of {}", pal.join(", ")));
        }
        if d.people_count > 0 {
            // Non-identifying by default (research/05 §5.7 likeness guardrail).
            parts.push("people only as distant silhouettes, faces not visible".into());
        }
    } else {
        parts.push("atmospheric, painterly, rich but restrained colour".into());
    }
    parts.push("no text, no letters, no logos, no watermark, no signature".into());
    let negative = "text, letters, words, typography, logo, watermark, signature, caption, frame, border, \
recognizable celebrity, real person's face"
        .to_owned();
    (parts.join(", "), negative)
}

/// Whether generation is allowed for an item, and how much.
#[derive(Debug, Clone)]
pub struct GenAiPolicy {
    /// Per-library opt-in (default off).
    pub enabled: bool,
    /// Variants per poster (we keep the first that decodes).
    pub variants: u32,
    pub max_concurrent: usize,
}

impl Default for GenAiPolicy {
    fn default() -> Self {
        GenAiPolicy {
            enabled: false,
            variants: 1,
            max_concurrent: 1,
        }
    }
}

/// A generated poster plus its accounting.
#[derive(Debug, Clone)]
pub struct GeneratedPoster {
    pub poster: ArtImage,
    pub prompt: String,
    pub prompt_hash: String,
    pub model: String,
    pub usd: f64,
    pub description: Option<SceneDescription>,
}

/// The opt-in genAI poster pipeline.
pub struct GenAiPosterPipeline {
    generator: Arc<dyn PosterGenerator>,
    describer: Option<Arc<dyn SceneDescriber>>,
    budget: Arc<Budget>,
    slots: Arc<Semaphore>,
    policy: GenAiPolicy,
    frame_opts: FramePosterOptions,
}

impl std::fmt::Debug for GenAiPosterPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenAiPosterPipeline")
            .field("generator", &self.generator.descriptor())
            .finish_non_exhaustive()
    }
}

impl GenAiPosterPipeline {
    /// A pipeline with a generator, an optional describer, a budget and a policy.
    pub fn new(
        generator: Arc<dyn PosterGenerator>,
        describer: Option<Arc<dyn SceneDescriber>>,
        budget: Arc<Budget>,
        policy: GenAiPolicy,
    ) -> Self {
        let slots = Arc::new(Semaphore::new(policy.max_concurrent.max(1)));
        GenAiPosterPipeline {
            generator,
            describer,
            budget,
            slots,
            policy,
            frame_opts: FramePosterOptions::default(),
        }
    }

    /// Override frame-harvest settings (tools, concurrency…).
    pub fn with_frame_options(mut self, opts: FramePosterOptions) -> Self {
        self.frame_opts = opts;
        self
    }

    /// Generate a poster for the media file at `path`.
    pub async fn run(
        &self,
        path: &Path,
        title: &str,
        year: Option<u16>,
    ) -> Result<GeneratedPoster, ArtError> {
        if !self.policy.enabled {
            return Err(ArtError::Disabled);
        }
        let desc_g = self.generator.descriptor();
        let estimate = Cost::usd(desc_g.usd_per_image * f64::from(self.policy.variants.max(1)) + 0.001);
        let reservation = self.budget.try_reserve(estimate)?;
        let _slot = self
            .slots
            .acquire()
            .await
            .map_err(|e| ArtError::Join(e.to_string()))?;

        // 1. Frames from the user's own file → optional VLM description.
        let description = match &self.describer {
            Some(d) => {
                let h = harvest(path, None, &self.frame_opts, (0.10, 0.80), 8).await?;
                let frames: Vec<_> = h.frames.into_iter().take(4).map(|(f, _, _)| f).collect();
                match d.describe(&frames).await {
                    Ok((desc, _usage)) => Some(desc),
                    Err(e) => {
                        tracing::warn!(error = %e, "scene description failed; using the generic template");
                        None
                    }
                }
            }
            None => None,
        };
        // 2. Deterministic prompt (no title).
        let (prompt, negative) = build_prompt(description.as_ref());
        let req = ImageGenRequest {
            prompt: prompt.clone(),
            negative_prompt: Some(negative),
            width: 1024,
            height: 1536,
            n: self.policy.variants.max(1),
            seed: None,
        };
        // 3. Generate; the cost is incurred once the call returns, success or not.
        let generated = self.generator.generate(&req).await;
        let usd = desc_g.usd_per_image * f64::from(req.n);
        reservation.commit(Cost::usd(usd));
        let generated = generated?;
        let img = generated
            .iter()
            .find_map(|g| image::load_from_memory(&g.bytes).ok())
            .ok_or_else(|| ArtError::Decode("no generated image could be decoded".into()))?
            .to_rgb8();
        // 4. Cover-crop to 2:3, typeset, embed provenance.
        let prompt_hash = hash_hex(prompt.as_bytes());
        let title_s = title.to_owned();
        let model = desc_g.model.clone();
        let p = Provenance {
            source: ArtSource::Generated,
            digital_source_type: DigitalSourceType::TrainedAlgorithmicMedia,
            model: Some(format!("{}/{}", desc_g.provider, desc_g.model)),
            prompt_hash: Some(prompt_hash.clone()),
            description: format!("AI-generated key art ({model}); title \"{title_s}\" typeset by flux"),
        };
        let quality = self.frame_opts.jpeg_quality;
        let poster = tokio::task::spawn_blocking(move || -> Result<ArtImage, ArtError> {
            let composed = compose_generated(&img, &title_s, year);
            encode_jpeg(&composed, quality, &p)
        })
        .await
        .map_err(|e| ArtError::Join(e.to_string()))??;
        Ok(GeneratedPoster {
            poster,
            prompt,
            prompt_hash,
            model,
            usd,
            description,
        })
    }
}

/// Cover-crop a generated image to 2:3 (centre-weighted by energy) and typeset it.
pub fn compose_generated(img: &RgbImage, title: &str, year: Option<u16>) -> RgbImage {
    let area = crop::Rect {
        x: 0,
        y: 0,
        w: img.width(),
        h: img.height(),
    };
    let c = crop::best_crop(img, area, 2.0 / 3.0);
    let w = (c.w.clamp(600, 1024)) & !1;
    compose::render_poster(img, c, w, w * 3 / 2, title, year, Typesetter::shared())
}

/// Encode an `RgbImage` as PNG (test helper for mock generators).
pub fn png_bytes(img: &RgbImage) -> Result<Vec<u8>, ArtError> {
    let mut out = Vec::new();
    image::DynamicImage::ImageRgb8(img.clone())
        .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|e| ArtError::Encode(e.to_string()))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_is_text_free_and_sanitised() {
        let d = SceneDescription {
            setting: "a rainy neon-lit harbour".into(),
            era: "near future".into(),
            mood: "melancholic".into(),
            palette: vec!["teal".into(), "amber".into(), "logo red".into()],
            genre_guess: "noir".into(),
            key_objects: vec!["a lighthouse".into(), "text on a sign".into()],
            people_count: 2,
            time_of_day: "night".into(),
        };
        let (p, neg) = build_prompt(Some(&d));
        assert!(p.contains("rainy neon-lit harbour") && p.contains("teal, amber"));
        assert!(!p.contains("logo red") && !p.contains("text on a sign"));
        assert!(p.contains("no text, no letters"));
        assert!(p.contains("silhouettes"));
        assert!(neg.contains("watermark"));
        let (generic, _) = build_prompt(None);
        assert!(generic.contains("no text"));
    }

    #[test]
    fn sizes() {
        assert_eq!(openai_size(1024, 1536), "1024x1536");
        assert_eq!(openai_size(1536, 1024), "1536x1024");
    }
}
