//! flux-server: wires the flux crates into the HTTP API of `flux_core::api`.
//!
//! ```text
//! flux-net (localhost / LAN / tailnet, hyper half_close)
//!   └─ axum Router (api::router)
//!        /api/v1/…        browse (flux-catalog), libraries + scans (flux-scan),
//!                         play/plan/sessions (flux-hls), events (SSE), art (flux-meta output)
//!        /api/v1/ops/…    agent operations (agent; docs/AGENT-NATIVE.md), and /mcp
//!        /media/{plan}/…  HLS from flux-hls; /media/{asset}/file with Range
//!        everything else  web/dist (SPA, strict CSP)
//! background: startup scans, jobs (matching, frame posters), tailnet status
//! ```
//!
//! The binary is `src/main.rs`; [`server::start`] runs the same server in
//! tests and benchmarks.

pub mod agent;
pub mod api;
pub mod art;
pub mod browse;
pub mod config;
pub mod devices;
pub mod doctor;
pub mod error;
pub mod events;
pub mod guard;
pub mod jobs;
mod lru;
pub mod media;
pub mod metadata;
pub mod net;
pub mod play;
pub mod roots;
pub mod scan;
pub mod server;
pub mod service;
pub mod state;
pub mod update;
pub mod web;
#[cfg(windows)]
pub mod win32;

pub use config::{Cli, Config};
pub use server::{build_state, start, start_at, start_with, Server};
