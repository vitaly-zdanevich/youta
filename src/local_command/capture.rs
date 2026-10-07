//! Bounded, concurrent pipe draining for command output in desktop dialogs.

use std::io::{self, Read};
use std::process::Stdio;

use super::{CommandOutput, ShellCommandPlan};

const STREAM_LIMIT: usize = 64 * 1024;
const TRUNCATION_NOTICE: &str = "\n[Output truncated]\n";

/// A retained prefix plus evidence that the remaining stream was discarded.
struct CapturedStream {
    bytes: Vec<u8>,
    truncated: bool,
}

impl CapturedStream {
    /// Bounds the visible UTF-8 output even when invalid bytes expand lossily.
    fn text(self) -> String {
        let decoded = String::from_utf8_lossy(&self.bytes);
        let mut end = decoded.len().min(STREAM_LIMIT);
        while !decoded.is_char_boundary(end) {
            end -= 1;
        }
        let mut text = decoded[..end].to_owned();
        if self.truncated || end < decoded.len() {
            text.push_str(TRUNCATION_NOTICE);
        }
        text
    }
}

/// Drains a pipe to EOF while retaining only its bounded leading bytes.
fn read_capped(mut reader: impl Read) -> io::Result<CapturedStream> {
    let mut captured = CapturedStream {
        bytes: Vec::new(),
        truncated: false,
    };
    let mut chunk = [0_u8; 8_192];
    loop {
        let length = match reader.read(&mut chunk) {
            Ok(0) => return Ok(captured),
            Ok(length) => length,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let retained = length.min(STREAM_LIMIT.saturating_sub(captured.bytes.len()));
        captured.bytes.extend_from_slice(&chunk[..retained]);
        captured.truncated |= retained < length;
    }
}

impl ShellCommandPlan {
    /// Runs a noninteractive desktop command and captures a bounded result.
    ///
    /// The caller runs this on a worker. Standard input is closed; stdout and
    /// stderr drain concurrently so either may exceed its 64 KiB retained prefix
    /// without blocking the child. Process output is returned only to its dialog.
    /// A nonzero exit is an ordinary result with `success = false`.
    ///
    /// # Errors
    ///
    /// Reports shell setup, spawning, pipe, or wait failures without echoing the
    /// user's command template or captured output in the error message.
    pub fn capture_output(&self) -> Result<CommandOutput, String> {
        let mut child = self
            .noninteractive_command()?
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                format!("cannot run Bash (install bash and ensure it is on PATH): {error}")
            })?;
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("cannot capture the command output pipes".to_owned());
        };
        let stderr_reader = match std::thread::Builder::new()
            .name("youta-command-stderr".to_owned())
            .spawn(move || read_capped(stderr))
        {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("cannot start the command output reader: {error}"));
            }
        };
        let stdout = read_capped(stdout);
        if stdout.is_err() {
            let _ = child.kill();
        }
        let status = child.wait();
        let stderr = stderr_reader
            .join()
            .map_err(|_| "the command output reader stopped unexpectedly".to_owned())?
            .map_err(|error| format!("cannot read command standard error: {error}"))?;
        let mut output = stdout
            .map_err(|error| format!("cannot read command standard output: {error}"))?
            .text();
        let stderr = stderr.text();
        if !stderr.is_empty() {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str("[stderr]\n");
            output.push_str(&stderr);
        }
        let status = status.map_err(|error| format!("cannot wait for the command: {error}"))?;
        Ok(CommandOutput {
            output,
            success: status.success(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Excess bytes are drained after the retained prefix reaches its memory bound.
    #[test]
    fn stream_capture_drains_but_does_not_retain_excess_bytes() {
        let source = vec![b'x'; STREAM_LIMIT * 3];
        let mut reader = source.as_slice();
        let captured = read_capped(&mut reader).unwrap();
        assert!(reader.is_empty());
        assert_eq!(captured.bytes.len(), STREAM_LIMIT);
        assert!(captured.truncated);
        assert!(captured.text().ends_with("[Output truncated]\n"));
    }

    /// Invalid UTF-8 is readable without expanding the retained text beyond its cap.
    #[test]
    fn lossy_output_keeps_a_valid_bounded_character_prefix() {
        let captured = CapturedStream {
            bytes: vec![0xff; STREAM_LIMIT],
            truncated: false,
        };
        let text = captured.text();
        assert!(text.starts_with('\u{fffd}'));
        assert!(text.len() <= STREAM_LIMIT + TRUNCATION_NOTICE.len());
        assert!(text.ends_with(TRUNCATION_NOTICE));
    }

    /// Test commands use the same safe argument transport as actual configured buttons.
    #[cfg(unix)]
    fn plan(template: &str, directory: &std::path::Path) -> super::super::ShellCommandPlan {
        super::super::ShellCommandPlan {
            template: template.to_owned(),
            argument: "/tmp/a file;$(not-shell-source)".into(),
            downloaded_path: None,
            directory: directory.to_owned(),
        }
    }

    /// The desktop worker preserves stdout, stderr, and failure status privately.
    #[cfg(unix)]
    #[test]
    fn commands_capture_both_streams_and_nonzero_exit_status() {
        let directory = tempfile::tempdir().unwrap();
        let output = plan(
            "printf '%s\\n' %; printf 'fixture error\\n' >&2; exit 7",
            directory.path(),
        )
        .capture_output()
        .unwrap();
        assert!(!output.success);
        assert!(output.output.contains("/tmp/a file;$(not-shell-source)"));
        assert!(output.output.contains("fixture error"));
        assert!(!format!("{output:?}").contains("fixture error"));
    }

    /// Draining stdout and stderr concurrently avoids a full-pipe deadlock.
    #[cfg(unix)]
    #[test]
    fn large_output_is_drained_on_both_pipes_without_unbounded_storage() {
        let directory = tempfile::tempdir().unwrap();
        let output = plan(
            "for ((i=0; i<10000; i++)); do printf 0123456789; printf abcdefghij >&2; done",
            directory.path(),
        )
        .capture_output()
        .unwrap();
        assert!(output.success);
        assert_eq!(output.output.matches(TRUNCATION_NOTICE).count(), 2);
        assert!(output.output.len() <= 2 * (STREAM_LIMIT + TRUNCATION_NOTICE.len()) + 16);
    }

    /// Desktop commands receive EOF instead of inheriting an invisible terminal.
    #[cfg(unix)]
    #[test]
    fn command_input_is_closed_and_spawn_errors_omit_private_templates() {
        let directory = tempfile::tempdir().unwrap();
        let output = plan(
            "if read -r value; then exit 4; else printf no-input; fi",
            directory.path(),
        )
        .capture_output()
        .unwrap();
        assert!(output.success);
        assert_eq!(output.output, "no-input");
        let error = plan(
            "printf secret-command-token",
            &directory.path().join("missing"),
        )
        .capture_output()
        .unwrap_err();
        assert!(!error.contains("secret-command-token"));
    }
}
