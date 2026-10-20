//! OpenAI-compatible chat-completions backend with JSON-schema structured output.
//!
//! One implementation covers OpenAI, Ollama (`/v1`), LM Studio, vLLM and
//! llama-server: they all accept `POST {base}/chat/completions` with
//! `response_format: {type: "json_schema", json_schema: {name, strict, schema}}`.
//! flux validates the result itself regardless ([`Hypothesis::validated`]),
//! because not every server enforces the schema.

use std::io::Cursor;
use std::time::Duration;

use base64::Engine as _;
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::json;

use super::{
    hypothesis_schema, scene_schema, AiError, Hypothesis, HypothesisOutput, Hypothesizer, LocalEvidence,
    Locality, ModelDescriptor, SceneDescriber, SceneDescription, Usage, SYSTEM_PROMPT,
};
use crate::art::Frame;

/// Which field caps output length: OpenAI's reasoning models only accept
/// `max_completion_tokens`; most local servers only know `max_tokens`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxTokensField {
    MaxTokens,
    MaxCompletionTokens,
}

/// Backend configuration. Use a preset and override fields as needed.
#[derive(Clone)]
pub struct OpenAiCompatibleConfig {
    /// Base URL including the version segment, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    /// `None` = server default (OpenAI's reasoning models reject anything else).
    pub temperature: Option<f32>,
    pub max_output_tokens: u32,
    pub max_tokens_field: MaxTokensField,
    pub timeout: Duration,
    pub locality: Locality,
    /// USD per million (input, output) tokens.
    pub price_per_m: (f64, f64),
    pub provider: String,
}

impl std::fmt::Debug for OpenAiCompatibleConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatibleConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("model", &self.model)
            .field("locality", &self.locality)
            .finish_non_exhaustive()
    }
}

impl OpenAiCompatibleConfig {
    /// OpenAI cloud (BYO key). Prices default to gpt-5-nano list prices
    /// ($0.05 / $0.40 per M tokens, research/05 §3.6); override for other models.
    pub fn openai(model: impl Into<String>, api_key: impl Into<String>) -> Self {
        OpenAiCompatibleConfig {
            base_url: "https://api.openai.com/v1".into(),
            api_key: Some(api_key.into()),
            model: model.into(),
            temperature: None,
            max_output_tokens: 400,
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            timeout: Duration::from_secs(20),
            locality: Locality::Cloud,
            price_per_m: (0.05, 0.40),
            provider: "openai".into(),
        }
    }

    /// Local Ollama (`ollama serve`, OpenAI-compatible API on port 11434).
    pub fn ollama(model: impl Into<String>) -> Self {
        OpenAiCompatibleConfig {
            base_url: "http://localhost:11434/v1".into(),
            api_key: None,
            model: model.into(),
            temperature: Some(0.0),
            max_output_tokens: 400,
            max_tokens_field: MaxTokensField::MaxTokens,
            timeout: Duration::from_secs(60),
            locality: Locality::Local,
            price_per_m: (0.0, 0.0),
            provider: "ollama".into(),
        }
    }

    /// Local LM Studio server (port 1234).
    pub fn lm_studio(model: impl Into<String>) -> Self {
        OpenAiCompatibleConfig {
            base_url: "http://localhost:1234/v1".into(),
            provider: "lmstudio".into(),
            ..Self::ollama(model)
        }
    }

    /// A vLLM / llama-server endpoint anywhere (e.g. a GPU box on the tailnet).
    pub fn vllm(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        OpenAiCompatibleConfig {
            base_url: base_url.into(),
            locality: Locality::Remote,
            provider: "vllm".into(),
            ..Self::ollama(model)
        }
    }

    /// From `FLUX_AI_BASE_URL` + `FLUX_AI_MODEL` (+ `FLUX_AI_API_KEY`), or
    /// `OPENAI_API_KEY` (+ optional `FLUX_AI_MODEL`, default `gpt-5-nano`).
    pub fn from_env() -> Option<Self> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        if let (Some(base), Some(model)) = (get("FLUX_AI_BASE_URL"), get("FLUX_AI_MODEL")) {
            let mut c = Self::vllm(base, model);
            c.api_key = get("FLUX_AI_API_KEY");
            return Some(c);
        }
        get("OPENAI_API_KEY")
            .map(|k| Self::openai(get("FLUX_AI_MODEL").unwrap_or_else(|| "gpt-5-nano".into()), k))
    }
}

/// The backend.
#[derive(Debug, Clone)]
pub struct OpenAiCompatible {
    cfg: OpenAiCompatibleConfig,
    http: reqwest::Client,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<RawUsage>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    refusal: Option<String>,
}

#[derive(Deserialize)]
struct RawUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

/// Models sometimes wrap JSON in a Markdown fence despite the schema; accept that.
fn strip_fences(s: &str) -> &str {
    let t = s.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    t.strip_suffix("```").unwrap_or(t).trim()
}

impl OpenAiCompatible {
    /// Build the client (no I/O).
    pub fn new(cfg: OpenAiCompatibleConfig) -> Result<Self, AiError> {
        let http = reqwest::Client::builder().timeout(cfg.timeout).build()?;
        Ok(OpenAiCompatible { cfg, http })
    }

    /// The configuration.
    pub fn config(&self) -> &OpenAiCompatibleConfig {
        &self.cfg
    }

    fn descriptor_inner(&self) -> ModelDescriptor {
        ModelDescriptor {
            provider: self.cfg.provider.clone(),
            model: self.cfg.model.clone(),
            locality: self.cfg.locality,
            usd_per_m_input: self.cfg.price_per_m.0,
            usd_per_m_output: self.cfg.price_per_m.1,
        }
    }

    /// One structured chat call: returns the JSON content, usage and model name.
    async fn structured(
        &self,
        name: &str,
        schema: serde_json::Value,
        system: &str,
        user: serde_json::Value,
    ) -> Result<(serde_json::Value, Usage, String), AiError> {
        let mut body = json!({
            "model": self.cfg.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
            "response_format": {
                "type": "json_schema",
                "json_schema": { "name": name, "strict": true, "schema": schema }
            },
            "stream": false,
        });
        let field = match self.cfg.max_tokens_field {
            MaxTokensField::MaxTokens => "max_tokens",
            MaxTokensField::MaxCompletionTokens => "max_completion_tokens",
        };
        body[field] = json!(self.cfg.max_output_tokens);
        if let Some(t) = self.cfg.temperature {
            body["temperature"] = json!(t);
        }
        let url = format!("{}/chat/completions", self.cfg.base_url.trim_end_matches('/'));
        let mut req = self.http.post(&url).json(&body);
        if let Some(k) = &self.cfg.api_key {
            req = req.bearer_auth(k);
        }
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(AiError::Status {
                status: status.as_u16(),
                body: body.chars().take(500).collect(),
            });
        }
        let r: ChatResponse = resp.json().await?;
        let usage = r
            .usage
            .map(|u| Usage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
            })
            .unwrap_or_default();
        let msg = r
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| AiError::Schema("no choices".into()))?
            .message;
        if let Some(refusal) = msg.refusal.filter(|s| !s.is_empty()) {
            return Err(AiError::Refusal(refusal));
        }
        let content = msg
            .content
            .ok_or_else(|| AiError::Schema("empty message".into()))?;
        let value: serde_json::Value = serde_json::from_str(strip_fences(&content))
            .map_err(|e| AiError::Schema(format!("not JSON: {e}")))?;
        Ok((value, usage, r.model.unwrap_or_else(|| self.cfg.model.clone())))
    }
}

impl Hypothesizer for OpenAiCompatible {
    fn descriptor(&self) -> ModelDescriptor {
        self.descriptor_inner()
    }

    fn hypothesize<'a>(
        &'a self,
        evidence: &'a LocalEvidence,
    ) -> BoxFuture<'a, Result<HypothesisOutput, AiError>> {
        Box::pin(async move {
            let user = json!(serde_json::to_string(&evidence.to_prompt_json()).unwrap_or_default());
            let (value, usage, model) = self
                .structured("flux_media_hypothesis", hypothesis_schema(), SYSTEM_PROMPT, user)
                .await?;
            let h: Hypothesis = serde_json::from_value(value).map_err(|e| AiError::Schema(e.to_string()))?;
            let hypothesis = h.validated()?;
            let usd = self.descriptor_inner().cost(usage);
            Ok(HypothesisOutput {
                hypothesis,
                usage,
                model,
                usd,
            })
        })
    }
}

/// System prompt for describing frames for key-art prompting.
const SCENE_PROMPT: &str = "Describe these frames from a user's own video for an artist who will paint \
text-free key art inspired by them. Reply with JSON only, matching the schema. Describe setting, era, mood, \
a palette of 3-5 colour names, a genre guess, up to 6 key objects, how many people are visible, and time of day. \
Do not identify people, do not guess names or titles, and do not mention any text visible in the frames.";

impl SceneDescriber for OpenAiCompatible {
    fn descriptor(&self) -> ModelDescriptor {
        self.descriptor_inner()
    }

    fn describe<'a>(
        &'a self,
        frames: &'a [Frame],
    ) -> BoxFuture<'a, Result<(SceneDescription, Usage), AiError>> {
        Box::pin(async move {
            let mut parts = vec![json!({ "type": "text", "text": "Frames:" })];
            for f in frames.iter().take(4) {
                let img = image::imageops::resize(
                    f.image(),
                    512,
                    512 * f.image().height() / f.image().width().max(1),
                    image::imageops::FilterType::Triangle,
                );
                let mut jpeg = Vec::new();
                image::DynamicImage::ImageRgb8(img)
                    .write_to(&mut Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
                    .map_err(|e| AiError::Image(e.to_string()))?;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&jpeg);
                parts.push(json!({ "type": "image_url", "image_url": { "url": format!("data:image/jpeg;base64,{b64}") } }));
            }
            let (value, usage, _) = self
                .structured(
                    "flux_scene_description",
                    scene_schema(),
                    SCENE_PROMPT,
                    json!(parts),
                )
                .await?;
            let d: SceneDescription =
                serde_json::from_value(value).map_err(|e| AiError::Schema(e.to_string()))?;
            Ok((d, usage))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fences_are_stripped() {
        assert_eq!(strip_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_fences(" {\"a\":1} "), "{\"a\":1}");
    }

    #[test]
    fn debug_redacts_key() {
        let c = OpenAiCompatibleConfig::openai("gpt-5-nano", "sk-secret");
        assert!(!format!("{c:?}").contains("sk-secret"));
    }
}
