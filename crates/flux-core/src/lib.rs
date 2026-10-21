//! Shared domain types for flux.
//!
//! This crate is the contract between flux's engine crates. It holds plain
//! data (no async, one dependency, plus the Windows API bindings on Windows)
//! so every crate can depend on it without pulling anything else in. The
//! one exception to "no I/O" is [`file_id`], the file identity every crate
//! that opens media files must agree on.
//!
//! - [`media`]: what a probe learns about a file (container, tracks, HDR, …).
//! - [`names`]: what a filename/folder parser guesses (title, year, SxxEyy, …).
//! - [`plan`]: client capabilities and the explainable playback plan.
//! - [`api`]: JSON shapes of the HTTP API shared by the server and the web client.
//! - [`file_id`]: which file a path or open handle names (Unix inodes, Windows file ids).

pub mod api;
pub mod file_id;
pub mod media;
pub mod names;
pub mod plan;

pub use media::*;
pub use names::*;
pub use plan::*;
