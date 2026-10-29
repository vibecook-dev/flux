//! flux.app: flux on a Mac, from the menu bar.
//!
//! The app runs flux-server (in `flux.app/Contents/Helpers`, with the
//! FFmpeg the app bundles) for the person logged in, as `flux-server
//! service install` would, and keeps it running: it starts it again when it
//! stops on its own, and starts itself again as the new version when flux
//! updates itself (`supervisor.rs`). Its menu bar icon opens flux in the
//! browser, says whether flux runs, and has Open at Login, the log, and the
//! command line tools (`mac.rs`). It opens flux in the browser the first time
//! flux is up, for Set up flux, and offers to move itself to Applications
//! when it runs from its disk image. Settings go in
//! `~/Library/Application Support/flux/flux.toml`, which it writes, all
//! comments, the first time it starts.
//!
//! Off macOS this binary only says so.

#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod bundle;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod probe;
#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod supervisor;

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "macos")]
    {
        mac::run();
        std::process::ExitCode::SUCCESS
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("flux.app runs on macOS only: run flux-server instead.");
        std::process::ExitCode::FAILURE
    }
}
