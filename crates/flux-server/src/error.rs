//! HTTP errors.
//!
//! **Error bodies are plain text** (`text/plain; charset=utf-8`,
//! `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`), never JSON: one short sentence for a person,
//! which the web client shows as the message (`FluxApiError`). This holds
//! for the handlers' errors, the cross-site guard's `421`/`403`
//! (`crate::guard`), the event-stream cap's `503`, and flux-net's `503` for
//! a connection over its limits; only bare protocol answers (`405`, a
//! missing static file's `404`, `416`) have no body. Clients branch on the
//! status code, never on the text. Success bodies are JSON (or media).

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use flux_catalog::CatalogError;
use flux_hls::HlsError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        ApiError {
            status,
            message: message.into(),
        }
    }
    pub fn bad_request(m: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, m)
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, m)
    }
    pub fn forbidden(m: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, m)
    }
    pub fn conflict(m: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, m)
    }
    pub fn gone(m: impl Into<String>) -> Self {
        Self::new(StatusCode::GONE, m)
    }
    pub fn unavailable(m: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, m)
    }
    pub fn not_implemented(m: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_IMPLEMENTED, m)
    }
    pub fn internal(m: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, m)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.status.as_u16(), self.message)
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            tracing::warn!(status = self.status.as_u16(), "{}", self.message);
        }
        (
            self.status,
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
                // Some bodies echo request text (the guard's 421 names the host).
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            self.message,
        )
            .into_response()
    }
}

impl From<CatalogError> for ApiError {
    fn from(e: CatalogError) -> Self {
        match e {
            CatalogError::NotFound(m) => ApiError::not_found(format!("Not found: {m}")),
            CatalogError::Invalid(m) => ApiError::bad_request(m),
            // One folder inside another of the same library would catalog
            // every file below the inner one twice (review INT-7).
            CatalogError::OverlappingRoots { root, other } => ApiError::bad_request(format!(
                "The folder {root:?} is inside {other:?} (or contains it): a library can't have both. Pick one."
            )),
            // The same folder in two libraries would list every file twice.
            e @ CatalogError::RootInOtherLibrary { .. } => ApiError::bad_request(e.to_string()),
            CatalogError::WriterGone => ApiError::unavailable("The server is shutting down"),
            other => ApiError::internal(format!("Catalog error: {other}")),
        }
    }
}

impl From<HlsError> for ApiError {
    fn from(e: HlsError) -> Self {
        match e {
            HlsError::NotFound(m) => ApiError::not_found(m),
            HlsError::Unsupported(m) => ApiError::not_implemented(format!("Not supported yet: {m}")),
            HlsError::Cancelled => ApiError::unavailable("Cancelled"),
            HlsError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
                ApiError::conflict(format!("The file can't be opened right now: {io}"))
            }
            other => ApiError::internal(other.to_string()),
        }
    }
}

impl From<axum::extract::rejection::QueryRejection> for ApiError {
    fn from(e: axum::extract::rejection::QueryRejection) -> Self {
        ApiError::bad_request(e.body_text())
    }
}

impl From<axum::extract::rejection::JsonRejection> for ApiError {
    fn from(e: axum::extract::rejection::JsonRejection) -> Self {
        ApiError::new(e.status(), e.body_text())
    }
}

impl From<axum::extract::rejection::PathRejection> for ApiError {
    fn from(e: axum::extract::rejection::PathRejection) -> Self {
        ApiError::bad_request(e.body_text())
    }
}
