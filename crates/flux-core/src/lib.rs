//! Shared domain types for flux.
//!
//! This crate is the contract between flux's engine crates. It holds plain
//! data only (no I/O, no async, one dependency) so every crate can depend on
//! it without pulling anything else in. The one exception is [`tool`], which
//! only builds a `std::process::Command`.
//!
//! - [`media`]: what a probe learns about a file (container, tracks, HDR, …).
//! - [`names`]: what a filename/folder parser guesses (title, year, SxxEyy, …).
//! - [`plan`]: client capabilities and the explainable playback plan.
//! - [`api`]: JSON shapes of the HTTP API shared by the server and the web client.
//! - [`tool`]: how every crate starts ffmpeg, ffprobe and other helpers.

pub mod api;
pub mod media;
pub mod names;
pub mod plan;
pub mod tool;

pub use media::*;
pub use names::*;
pub use plan::*;
