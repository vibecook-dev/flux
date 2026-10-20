//! A thin, typed TMDB v3 client.
//!
//! - **Auth:** `TMDB_READ_TOKEN` (v4 "API Read Access Token", sent as
//!   `Authorization: Bearer`, preferred so keys stay out of URLs and logs) or
//!   `TMDB_API_KEY` (v3 key, sent as the `api_key` query parameter).
//! - **Rate limit:** client-side token bucket, default 20 rps (TMDB's ceiling
//!   is ~40 rps).
//! - **Retries:** 429 honours `Retry-After`; 5xx/timeouts back off
//!   exponentially with jitter.
//! - **Cache:** optional [`DiskCache`] whose maximum age is clamped to 180 days
//!   (TMDB ToS §1.C: never cache longer than 6 months).
//! - **Attribution:** [`attribution`] holds the notice and links the UI must show.
//!
//! The client is deterministic plumbing: nothing it returns may be passed to
//! the AI path (see [`TmdbText`] and [`crate::ai`]).

mod cache;
mod ratelimit;
mod types;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reqwest::StatusCode;
use serde::de::DeserializeOwned;

pub use cache::{DiskCache, PurgeStats, TMDB_MAX_CACHE_AGE};
pub use ratelimit::RateLimiter;
pub use types::*;

/// TMDB attribution requirements (API ToS §3) and terms profile.
pub mod attribution {
    use std::time::Duration;

    /// Provider display name.
    pub const PROVIDER_NAME: &str = "TMDB";
    /// Link target for the attribution logo.
    pub const PROVIDER_URL: &str = "https://www.themoviedb.org/";
    /// The notice ToS §3 requires, verbatim apart from the application name.
    pub const NOTICE: &str =
        "flux uses TMDB and the TMDB APIs but is not endorsed, certified, or otherwise approved by TMDB.";
    /// Official logo files and usage rules. The logo must be less prominent than flux's own.
    pub const LOGO_GUIDELINES_URL: &str = "https://www.themoviedb.org/about/logos-attribution";
    /// The API terms these constants implement.
    pub const TERMS_URL: &str = "https://www.themoviedb.org/api-terms-of-use";

    /// The API reference for a TMDB image (`ImageConfiguration::url`), credited
    /// to TMDB (`ArtRef::attribution`). Store `PROVIDER_NAME` as the catalog
    /// art row's attribution so lists return it too.
    pub fn art_ref(
        url: impl Into<String>,
        width: Option<u32>,
        height: Option<u32>,
    ) -> flux_core::api::ArtRef {
        flux_core::api::ArtRef {
            url: url.into(),
            width,
            height,
            blurhash: None,
            source: flux_core::api::ArtSource::Provider,
            attribution: Some(PROVIDER_NAME.to_string()),
            generator: None,
        }
    }

    /// What a provider's terms allow; the AI job layer checks `ai_allowed`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct TermsProfile {
        pub provider: &'static str,
        /// Hard cap on how long any of its content may be cached.
        pub max_cache_age: Duration,
        /// May its content be used "in connection with" an AI application?
        pub ai_allowed: bool,
        /// Free use only for non-commercial applications.
        pub non_commercial_only: bool,
        pub attribution_notice: &'static str,
    }

    /// TMDB: 6-month cache cap (enforced as 180 days), no AI use (§1.C),
    /// non-commercial without a written agreement (§2).
    pub const TMDB_TERMS: TermsProfile = TermsProfile {
        provider: PROVIDER_NAME,
        max_cache_age: Duration::from_secs(180 * 86_400),
        ai_allowed: false,
        non_commercial_only: true,
        attribution_notice: NOTICE,
    };
}

/// `append_to_response` for movie details (one HTTP request instead of four).
pub const MOVIE_APPEND: &str = "images,external_ids,release_dates";
/// `append_to_response` for TV details.
pub const TV_APPEND: &str = "external_ids,images";
/// Default API base.
pub const DEFAULT_BASE_URL: &str = "https://api.themoviedb.org/3";

/// Errors from the TMDB client.
#[derive(Debug, thiserror::Error)]
pub enum TmdbError {
    #[error("no TMDB credentials: set TMDB_READ_TOKEN (v4 read token) or TMDB_API_KEY (v3 key)")]
    NoCredentials,
    #[error("TMDB rejected the credentials ({0})")]
    Unauthorized(String),
    #[error("TMDB resource not found: {0}")]
    NotFound(String),
    #[error("TMDB rate limit still exceeded after {0} retries")]
    RateLimited(u32),
    #[error("TMDB returned HTTP {status} for {path}: {message}")]
    Status {
        status: u16,
        path: String,
        message: String,
    },
    #[error("HTTP error talking to TMDB: {0}")]
    Http(#[from] reqwest::Error),
    #[error("could not decode TMDB response for {path}: {source}")]
    Decode { path: String, source: serde_json::Error },
}

/// How to authenticate.
#[derive(Clone)]
pub enum TmdbAuth {
    /// v3 API key (`api_key=` query parameter).
    ApiKey(String),
    /// v4 API Read Access Token (`Authorization: Bearer`), works for all v3 endpoints.
    ReadToken(String),
}

impl std::fmt::Debug for TmdbAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print secrets.
        match self {
            TmdbAuth::ApiKey(_) => f.write_str("ApiKey(<redacted>)"),
            TmdbAuth::ReadToken(_) => f.write_str("ReadToken(<redacted>)"),
        }
    }
}

impl TmdbAuth {
    /// `TMDB_READ_TOKEN` if set (preferred), else `TMDB_API_KEY`.
    pub fn from_env() -> Option<Self> {
        let get = |k: &str| {
            std::env::var(k)
                .ok()
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        get("TMDB_READ_TOKEN")
            .map(TmdbAuth::ReadToken)
            .or_else(|| get("TMDB_API_KEY").map(TmdbAuth::ApiKey))
    }
}

/// Client configuration. Start from [`TmdbConfig::new`] and override fields.
#[derive(Debug, Clone)]
pub struct TmdbConfig {
    pub auth: TmdbAuth,
    /// API base, default [`DEFAULT_BASE_URL`] (tests point this at a mock server).
    pub base_url: String,
    /// `language=` for localized fields, e.g. `"en-US"`. `None` = TMDB default.
    pub language: Option<String>,
    /// Sustained requests per second (default 20).
    pub requests_per_second: f64,
    /// Burst size (default 20).
    pub burst: u32,
    /// Retries after 429/5xx/timeouts (default 4).
    pub max_retries: u32,
    /// Base delay for exponential backoff when no `Retry-After` is given (default 500 ms).
    pub backoff_base: Duration,
    /// Per-request timeout (default 15 s).
    pub timeout: Duration,
    /// Optional response cache (max age clamped to 180 days).
    pub cache: Option<DiskCache>,
    pub user_agent: String,
}

impl TmdbConfig {
    /// Defaults: 20 rps, 4 retries, 15 s timeout, `en-US`, no cache.
    pub fn new(auth: TmdbAuth) -> Self {
        TmdbConfig {
            auth,
            base_url: DEFAULT_BASE_URL.to_owned(),
            language: Some("en-US".to_owned()),
            requests_per_second: 20.0,
            burst: 20,
            max_retries: 4,
            backoff_base: Duration::from_millis(500),
            timeout: Duration::from_secs(15),
            cache: None,
            user_agent: format!(
                "flux/{} (+https://github.com/vibecook-dev/flux)",
                env!("CARGO_PKG_VERSION")
            ),
        }
    }

    /// [`TmdbConfig::new`] with credentials from the environment.
    pub fn from_env() -> Result<Self, TmdbError> {
        TmdbAuth::from_env()
            .map(TmdbConfig::new)
            .ok_or(TmdbError::NoCredentials)
    }
}

/// Request counters, for diagnostics and benchmarks.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ClientStats {
    /// HTTP requests actually sent (including retries).
    pub requests: u64,
    pub cache_hits: u64,
    pub retries: u64,
}

#[derive(Debug, Default)]
struct Counters {
    requests: AtomicU64,
    cache_hits: AtomicU64,
    retries: AtomicU64,
}

/// `/search/movie` parameters.
#[derive(Debug, Clone, Default)]
pub struct MovieSearch {
    pub query: String,
    /// Nudges relevance only; TMDB does not filter by it (flux re-ranks client-side).
    pub year: Option<u16>,
    /// Filters by primary release year.
    pub primary_release_year: Option<u16>,
    /// 1-based; `0` is treated as 1.
    pub page: u32,
    pub include_adult: bool,
}

/// The TMDB client. Cheap to clone (shared connection pool, limiter and counters).
#[derive(Debug, Clone)]
pub struct TmdbClient {
    http: reqwest::Client,
    cfg: Arc<TmdbConfig>,
    limiter: Arc<RateLimiter>,
    counters: Arc<Counters>,
}

impl TmdbClient {
    /// Build a client (no network I/O happens here).
    pub fn new(cfg: TmdbConfig) -> Result<Self, TmdbError> {
        let http = reqwest::Client::builder()
            .user_agent(cfg.user_agent.clone())
            .timeout(cfg.timeout)
            .build()?;
        let limiter = Arc::new(RateLimiter::new(cfg.requests_per_second, cfg.burst));
        Ok(TmdbClient {
            http,
            cfg: Arc::new(cfg),
            limiter,
            counters: Arc::default(),
        })
    }

    /// Client from environment credentials (see [`TmdbAuth::from_env`]).
    pub fn from_env() -> Result<Self, TmdbError> {
        TmdbClient::new(TmdbConfig::from_env()?)
    }

    /// Request counters so far.
    pub fn stats(&self) -> ClientStats {
        ClientStats {
            requests: self.counters.requests.load(Ordering::Relaxed),
            cache_hits: self.counters.cache_hits.load(Ordering::Relaxed),
            retries: self.counters.retries.load(Ordering::Relaxed),
        }
    }

    /// `GET /search/movie`.
    pub async fn search_movie(&self, q: &MovieSearch) -> Result<Paged<MovieResult>, TmdbError> {
        let mut params = vec![
            ("query", q.query.clone()),
            ("page", q.page.max(1).to_string()),
            ("include_adult", q.include_adult.to_string()),
        ];
        if let Some(y) = q.year {
            params.push(("year", y.to_string()));
        }
        if let Some(y) = q.primary_release_year {
            params.push(("primary_release_year", y.to_string()));
        }
        self.get_json("/search/movie", &params).await
    }

    /// `GET /search/tv`.
    pub async fn search_tv(
        &self,
        query: &str,
        first_air_date_year: Option<u16>,
        page: u32,
    ) -> Result<Paged<TvResult>, TmdbError> {
        let mut params = vec![("query", query.to_owned()), ("page", page.max(1).to_string())];
        if let Some(y) = first_air_date_year {
            params.push(("first_air_date_year", y.to_string()));
        }
        self.get_json("/search/tv", &params).await
    }

    /// `GET /movie/{id}?append_to_response=images,external_ids,release_dates`.
    /// `include_image_language=en,null` keeps English and textless art.
    pub async fn movie_details(&self, id: u64) -> Result<MovieDetails, TmdbError> {
        let params = [
            ("append_to_response", MOVIE_APPEND.to_owned()),
            ("include_image_language", "en,null".to_owned()),
        ];
        self.get_json(&format!("/movie/{id}"), &params).await
    }

    /// `GET /tv/{id}?append_to_response=external_ids,images`.
    pub async fn tv_details(&self, id: u64) -> Result<TvDetails, TmdbError> {
        let params = [
            ("append_to_response", TV_APPEND.to_owned()),
            ("include_image_language", "en,null".to_owned()),
        ];
        self.get_json(&format!("/tv/{id}"), &params).await
    }

    /// `GET /tv/{id}/season/{n}`.
    pub async fn tv_season(&self, tv_id: u64, season: u16) -> Result<SeasonDetails, TmdbError> {
        self.get_json(&format!("/tv/{tv_id}/season/{season}"), &[]).await
    }

    /// `GET /find/{external_id}?external_source=…` (IMDb `tt…`, TVDB, Wikidata ids).
    pub async fn find(&self, external_id: &str, source: ExternalSource) -> Result<FindResults, TmdbError> {
        let params = [("external_source", source.as_param().to_owned())];
        self.get_json(&format!("/find/{external_id}"), &params).await
    }

    /// `GET /configuration` (image base URL and sizes; cache it for days).
    pub async fn configuration(&self) -> Result<Configuration, TmdbError> {
        self.get_json("/configuration", &[]).await
    }

    /// GET + decode with rate limiting, retries and the ToS-bounded cache.
    async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<T, TmdbError> {
        let mut params: Vec<(&str, String)> = params.to_vec();
        if let Some(lang) = &self.cfg.language {
            params.push(("language", lang.clone()));
        }
        params.sort();
        // The cache key never contains credentials.
        let key = {
            let qs: Vec<String> = params.iter().map(|(k, v)| format!("{k}={v}")).collect();
            format!("{path}?{}", qs.join("&"))
        };
        if let Some(cache) = &self.cfg.cache {
            if let Some(body) = cache.get(&key).await {
                if let Ok(v) = serde_json::from_slice::<T>(&body) {
                    self.counters.cache_hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(v);
                }
                // A cached body that no longer decodes (schema change) is refetched.
            }
        }
        let body = self.fetch(path, &params).await?;
        let value = serde_json::from_slice::<T>(&body).map_err(|source| TmdbError::Decode {
            path: path.to_owned(),
            source,
        })?;
        if let Some(cache) = &self.cfg.cache {
            if let Err(e) = cache.put(&key, &body).await {
                tracing::warn!(error = %e, "tmdb cache write failed");
            }
        }
        Ok(value)
    }

    async fn fetch(&self, path: &str, params: &[(&str, String)]) -> Result<bytes::Bytes, TmdbError> {
        let url = format!("{}{}", self.cfg.base_url.trim_end_matches('/'), path);
        let mut attempt = 0u32;
        loop {
            self.limiter.acquire().await;
            let mut req = self.http.get(&url).query(params);
            req = match &self.cfg.auth {
                TmdbAuth::ReadToken(t) => req.bearer_auth(t),
                TmdbAuth::ApiKey(k) => req.query(&[("api_key", k)]),
            };
            self.counters.requests.fetch_add(1, Ordering::Relaxed);
            let retry_delay = match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return Ok(resp.bytes().await?);
                    }
                    let retry_after = resp
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<f64>().ok())
                        .map(|s| Duration::from_secs_f64(s.clamp(0.0, 120.0)));
                    let message = resp
                        .bytes()
                        .await
                        .ok()
                        .and_then(|b| serde_json::from_slice::<ErrorBody>(&b).ok())
                        .map(|e| {
                            format!(
                                "{} (code {})",
                                e.status_message.unwrap_or_default(),
                                e.status_code.unwrap_or(0)
                            )
                        })
                        .unwrap_or_default();
                    match status {
                        StatusCode::UNAUTHORIZED => return Err(TmdbError::Unauthorized(message)),
                        StatusCode::NOT_FOUND => return Err(TmdbError::NotFound(path.to_owned())),
                        StatusCode::TOO_MANY_REQUESTS => {
                            if attempt >= self.cfg.max_retries {
                                return Err(TmdbError::RateLimited(attempt));
                            }
                            retry_after.unwrap_or_else(|| self.backoff(attempt))
                        }
                        s if s.is_server_error() && attempt < self.cfg.max_retries => self.backoff(attempt),
                        s => {
                            return Err(TmdbError::Status {
                                status: s.as_u16(),
                                path: path.to_owned(),
                                message,
                            })
                        }
                    }
                }
                Err(e) if (e.is_timeout() || e.is_connect()) && attempt < self.cfg.max_retries => {
                    self.backoff(attempt)
                }
                Err(e) => return Err(e.into()),
            };
            attempt += 1;
            self.counters.retries.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                path,
                attempt,
                delay_ms = retry_delay.as_millis() as u64,
                "tmdb retry"
            );
            tokio::time::sleep(retry_delay).await;
        }
    }

    /// Exponential backoff with ±25% jitter, capped at 30 s.
    fn backoff(&self, attempt: u32) -> Duration {
        let base = self.cfg.backoff_base.as_secs_f64() * 2f64.powi(attempt.min(10) as i32);
        let jitter = 0.75 + 0.5 * crate::util::jitter_unit();
        Duration::from_secs_f64((base * jitter).min(30.0))
    }
}
