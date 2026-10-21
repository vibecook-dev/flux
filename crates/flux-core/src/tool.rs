//! How flux starts its helper programs (ffmpeg, ffprobe, the `tailscale`
//! CLI). Every spawn site builds its command with [`command`] or
//! [`background_command`], so they all get the same treatment. This is std
//! only, so the crate keeps its single dependency.
//!
//! On Windows a helper:
//! - has no console (`CREATE_NO_WINDOW`). No console window flashes up when
//!   flux itself runs without one (as a service), and a Ctrl-C or Ctrl-Break
//!   sent to flux's console (typed, or sent by a service manager) reaches
//!   flux only. flux then stops its helpers itself, rather than each one
//!   dying mid-work on its own;
//! - leads its own process group (`CREATE_NEW_PROCESS_GROUP`), which also
//!   turns Ctrl-C off for it, for a helper that does end up on a console;
//! - runs below normal priority (`BELOW_NORMAL_PRIORITY_CLASS`) when it is
//!   [`Priority::Background`] work, under playback and the machine's other
//!   programs.
//!
//! flux-server also puts itself in a job object that ends every helper when
//! flux ends, however it ends.
//!
//! Elsewhere both are `Command::new`. Unix callers lower a background tool's
//! priority with `nice` (flux-server's jobs run tools through a wrapper
//! script), because changing a child's priority there needs code between
//! `fork` and `exec`.

use std::ffi::OsStr;
use std::process::Command;

/// How urgent a helper's work is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Priority {
    /// Work someone waits for (playback transcodes, probes, test encodes):
    /// flux's own priority.
    #[default]
    Normal,
    /// Background work (frame posters): below normal on Windows.
    Background,
}

/// A command that runs `program` as a flux helper at flux's own priority
/// (see the module docs). For tokio, wrap it:
/// `tokio::process::Command::from(command(…))`.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    with_priority(program, Priority::Normal)
}

/// [`command`] for background work: below normal priority on Windows.
pub fn background_command(program: impl AsRef<OsStr>) -> Command {
    with_priority(program, Priority::Background)
}

/// [`command`] at `priority`.
pub fn with_priority(program: impl AsRef<OsStr>, priority: Priority) -> Command {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(creation_flags(priority));
    }
    #[cfg(not(windows))]
    let _ = priority;
    cmd
}

/// The Win32 process creation flags [`with_priority`] sets on Windows
/// (values from the Windows SDK's `WinBase.h`).
pub fn creation_flags(priority: Priority) -> u32 {
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let flags = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
    match priority {
        Priority::Normal => flags,
        Priority::Background => flags | BELOW_NORMAL_PRIORITY_CLASS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_get_no_console_and_their_own_group() {
        assert_eq!(creation_flags(Priority::Normal), 0x0800_0200);
        assert_eq!(
            creation_flags(Priority::Background),
            0x0800_4200,
            "plus BELOW_NORMAL_PRIORITY_CLASS"
        );
        assert_eq!(Priority::default(), Priority::Normal);
    }

    #[test]
    fn helpers_run_and_report_their_exit_status() {
        // The creation flags are accepted, and stdio still works without a
        // console.
        #[cfg(windows)]
        let (program, args) = ("cmd.exe", ["/d", "/c", "echo flux& exit 3"]);
        #[cfg(not(windows))]
        let (program, args) = ("sh", ["-c", "echo flux; exit 3"]);
        for priority in [Priority::Normal, Priority::Background] {
            let out = with_priority(program, priority).args(args).output().unwrap();
            assert_eq!(out.status.code(), Some(3), "{priority:?}");
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "flux");
        }
    }
}
