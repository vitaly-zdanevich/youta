//! Starting and stopping helper processes the same way on every platform.
//!
//! Youta shells out to a handful of fixed executables — mpv, yt-dlp, FFmpeg,
//! FFprobe, fpcalc, git — always with an argument vector and never through a
//! shell. Two things about that differ per platform, and both of them are the
//! kind of difference that is invisible until someone runs the program on the
//! other operating system.
//!
//! # A console window is a user-visible bug
//!
//! On Windows, starting a console program from a graphical process opens a
//! console window for it. Youta's helpers are invisible by design: mpv renders
//! nothing, yt-dlp answers a question, FFprobe reads a duration. Without
//! `CREATE_NO_WINDOW` the desktop window would flash a black rectangle on every
//! search, every seek preview, and every track change. [`quiet`] is a no-op on
//! Unix, where the problem does not exist, and is applied at every spawn site
//! so a new one cannot silently reintroduce the flash.
//!
//! # Killing a helper is not killing what it started
//!
//! yt-dlp starts FFmpeg. Killing yt-dlp leaves FFmpeg holding the download, the
//! socket, and the disk. Unix solves this by putting the helper in its own
//! process group at spawn and signalling the group, which is what
//! [`terminate_tree`] does there.
//!
//! Windows' equivalent is a Job Object with `KILL_ON_JOB_CLOSE`, and reaching
//! it means raw Win32 calls, which this crate forbids. The shipped alternative
//! is `taskkill /T`, which walks the live parent-child chain and ends the whole
//! tree — the same fixed-executable, argument-vector discipline as every other
//! subprocess here. It has one hole a Job Object would not: a grandchild whose
//! parent has already exited is no longer reachable through the chain. The
//! direct child is killed regardless, so nothing is worse than it was; what is
//! bought is the common case, where the resolver is still alive and holding its
//! own children.

#[cfg(any(
    feature = "ascii-visualizer",
    feature = "audio-quality",
    feature = "yt-dlp"
))]
use std::process::Child;
use std::process::Command;

/// Starts a helper without giving it a console window of its own.
///
/// Returns the same command so it can be applied inline where the command is
/// built.
pub fn quiet(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        /// `CREATE_NO_WINDOW`: run the console helper with no console.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        command.creation_flags(CREATE_NO_WINDOW)
    }
    #[cfg(not(windows))]
    {
        command
    }
}

/// Starts a supervised helper: no console of its own, and its own process
/// group, so [`terminate_tree`] can end everything it goes on to start.
///
/// Only bounded or explicitly on-demand helpers are supervised this way. `mpv`
/// is not: it is meant to receive the terminal's own interrupt.
#[cfg(any(
    feature = "ascii-visualizer",
    feature = "audio-quality",
    feature = "yt-dlp"
))]
pub fn supervised(command: &mut Command) -> &mut Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        command.process_group(0);
    }
    quiet(command)
}

/// Ends everything a helper started, leaving the helper itself alone.
///
/// Used where the caller already has its own rules for killing and reporting
/// the direct child and only needs the descendants dealt with.
#[cfg(any(
    feature = "ascii-visualizer",
    feature = "audio-quality",
    feature = "yt-dlp"
))]
pub fn kill_descendants(pid: u32) {
    #[cfg(windows)]
    {
        // Best effort: it fails when the tree is already gone, which is
        // exactly when there is nothing to do.
        let _ = Command::new(windows_kill::executable(
            std::env::var("SystemRoot").ok().as_deref(),
        ))
        .args(windows_kill::arguments(pid))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    }
    #[cfg(not(windows))]
    {
        // The process group carries the descendants, and the caller signals it.
        let _ = pid;
    }
}

/// Ends a helper and, as far as the platform allows, everything it started.
///
/// Always reaps the child, so no zombie is left behind on Unix.
#[cfg(any(
    feature = "ascii-visualizer",
    feature = "audio-quality",
    feature = "yt-dlp"
))]
pub fn terminate_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        use rustix::process::{Pid, Signal, kill_process_group};

        if kill_process_group(Pid::from_child(child), Signal::KILL).is_err() {
            let _ = child.kill();
        }
    }
    #[cfg(windows)]
    {
        // The tree kill is best effort: it fails when the child is already
        // gone, which is exactly when there is nothing left to do.
        let _ = Command::new(windows_kill::executable(
            std::env::var("SystemRoot").ok().as_deref(),
        ))
        .args(windows_kill::arguments(child.id()))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
        // The direct child is ended regardless of whether the walk found it.
        let _ = child.kill();
    }
    let _ = child.wait();
}

/// How the tree is ended on Windows.
///
/// Compiled on Windows, where it is used, and under `cfg(test)` everywhere,
/// because the argument vector is the whole behaviour and should not be a thing
/// only one platform's build ever reads.
#[cfg(any(
    all(
        windows,
        any(
            feature = "ascii-visualizer",
            feature = "audio-quality",
            feature = "yt-dlp"
        )
    ),
    test
))]
mod windows_kill {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    /// Resolves the process terminator by absolute path rather than `PATH`.
    pub(super) fn executable(system_root: Option<&str>) -> PathBuf {
        let root = system_root
            .map(str::trim)
            .filter(|root| !root.is_empty())
            .unwrap_or(r"C:\Windows");
        Path::new(root).join("System32").join("taskkill.exe")
    }

    /// Ends the process and its descendants, without asking politely.
    ///
    /// `/T` is the tree; `/F` skips the close request a console helper has no
    /// window to receive.
    pub(super) fn arguments(pid: u32) -> Vec<OsString> {
        vec![
            OsString::from("/PID"),
            OsString::from(pid.to_string()),
            OsString::from("/T"),
            OsString::from("/F"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_terminator_is_named_absolutely_and_never_searched_for() {
        // Asserted by component rather than by rendered string: this test also
        // runs on Unix, where `Path::join` writes a different separator, and
        // the claim is about *which* file is named, not how it is spelled.
        use std::ffi::OsStr;
        use std::path::Path;

        let terminator = windows_kill::executable(Some(r"D:\Windows"));
        assert_eq!(terminator.file_name(), Some(OsStr::new("taskkill.exe")));
        assert_eq!(
            terminator.parent().and_then(Path::file_name),
            Some(OsStr::new("System32"))
        );
        assert!(terminator.starts_with(r"D:\Windows"));
        assert!(windows_kill::executable(None).starts_with(r"C:\Windows"));
    }

    #[test]
    fn the_whole_tree_is_ended_and_the_identifier_is_its_own_argument() {
        let arguments: Vec<String> = windows_kill::arguments(4321)
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();

        assert_eq!(arguments, vec!["/PID", "4321", "/T", "/F"]);
    }

    #[test]
    fn a_quiet_helper_still_runs_and_still_reports_what_it_printed() {
        // The flag only exists on Windows, so what this proves everywhere is
        // that applying it does not disturb an ordinary spawn.
        let mut command = Command::new(if cfg!(windows) { "cmd" } else { "echo" });
        if cfg!(windows) {
            command.args(["/C", "echo", "helper"]);
        } else {
            command.arg("helper");
        }
        let output = quiet(&mut command).output().expect("helper runs");

        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("helper"));
    }

    #[cfg(any(
        feature = "ascii-visualizer",
        feature = "audio-quality",
        feature = "yt-dlp"
    ))]
    #[test]
    fn a_terminated_helper_is_reaped_rather_than_left_behind() {
        let mut command = Command::new(if cfg!(windows) { "cmd" } else { "sleep" });
        if cfg!(windows) {
            command.args(["/C", "timeout", "/T", "30"]);
        } else {
            command.arg("30");
        }
        let mut child = quiet(&mut command).spawn().expect("helper starts");

        terminate_tree(&mut child);

        // `wait` already returned inside `terminate_tree`; a second one would
        // fail if the child had been left unreaped.
        assert!(child.try_wait().expect("reaped child").is_some());
    }
}
