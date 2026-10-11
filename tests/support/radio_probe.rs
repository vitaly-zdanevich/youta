//! Bounded ffprobe execution for opted-in public radio checks.

use std::io::{self, Read};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// Caps each diagnostic pipe independently, including unsuccessful probes.
const OUTPUT_LIMIT: usize = 16 * 1024;
/// A trickling remote stream must not extend the probe indefinitely.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs ffprobe with bounded capture and a deadline beyond its per-I/O timeout.
pub(super) fn run(command: Command) -> Result<Output, String> {
    run_with_limits(command, PROBE_TIMEOUT, OUTPUT_LIMIT)
}

/// Reaps the direct ffprobe process on success, timeout, or any early error.
struct ProbeChild(Child);

impl Drop for ProbeChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Captures a pipe concurrently so stdout cannot deadlock behind stderr.
fn capture(reader: impl Read + Send + 'static, limit: usize) -> Receiver<io::Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = reader
            .take(u64::try_from(limit.saturating_add(1)).unwrap_or(u64::MAX))
            .read_to_end(&mut bytes)
            .and_then(|_| {
                if bytes.len() > limit {
                    Err(io::Error::other(format!("output exceeds {limit} bytes")))
                } else {
                    Ok(bytes)
                }
            });
        let _ = sender.send(result);
    });
    receiver
}

/// Distinguishes pending pipe data from failed or oversized capture.
fn collect(
    receiver: &Receiver<io::Result<Vec<u8>>>,
    output: &mut Option<Vec<u8>>,
    label: &str,
) -> Result<(), String> {
    if output.is_none() {
        match receiver.try_recv() {
            Ok(result) => {
                *output = Some(result.map_err(|error| format!("ffprobe {label}: {error}"))?);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                return Err(format!("ffprobe {label} capture ended without a result"));
            }
        }
    }
    Ok(())
}

/// Injects limits for offline subprocess tests without altering live-check bounds.
fn run_with_limits(
    mut command: Command,
    timeout: Duration,
    limit: usize,
) -> Result<Output, String> {
    let started = Instant::now();
    let mut child = ProbeChild(
        youta::child_process::quiet(&mut command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("start ffprobe: {error}"))?,
    );
    let stdout = capture(child.0.stdout.take().expect("piped stdout"), limit);
    let stderr = capture(child.0.stderr.take().expect("piped stderr"), limit);
    let mut output = None;
    let mut diagnostics = None;
    loop {
        collect(&stdout, &mut output, "stdout")?;
        collect(&stderr, &mut diagnostics, "stderr")?;
        if let Some(status) = child
            .0
            .try_wait()
            .map_err(|error| format!("wait for ffprobe: {error}"))?
            && output.is_some()
            && diagnostics.is_some()
        {
            return Ok(Output {
                status,
                stdout: output.expect("completed stdout"),
                stderr: diagnostics.expect("completed stderr"),
            });
        }
        if started.elapsed() >= timeout {
            return Err(format!("ffprobe exceeded its {timeout:?} deadline"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reuses this test binary as a portable offline subprocess fixture.
    fn fixture(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "radio_probe::tests::probe_child_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("YOUTA_RADIO_PROBE_FIXTURE", mode);
        command
    }

    /// Invoked only by the bounded subprocess tests; never contacts a service.
    #[test]
    #[ignore = "offline subprocess fixture invoked by its parent test"]
    fn probe_child_fixture() {
        use std::io::Write;

        match std::env::var("YOUTA_RADIO_PROBE_FIXTURE").as_deref() {
            Ok("success") => {
                println!("codec:flac");
                eprintln!("fixture diagnostic");
            }
            Ok("failure") => std::process::exit(7),
            Ok("timeout") => std::thread::sleep(Duration::from_secs(30)),
            Ok("stdout") => {
                std::io::stdout().write_all(&[b'x'; 4096]).unwrap();
                std::thread::sleep(Duration::from_secs(30));
            }
            Ok("stderr") => {
                std::io::stderr().write_all(&[b'x'; 4096]).unwrap();
                std::thread::sleep(Duration::from_secs(30));
            }
            other => panic!("unknown radio probe fixture: {other:?}"),
        }
    }

    /// Both pipes are preserved and nonzero exits stay visible to the caller.
    #[test]
    fn probe_preserves_output_and_failure_status() {
        let output = run_with_limits(fixture("success"), Duration::from_secs(5), 1024)
            .expect("bounded successful fixture");
        assert!(output.status.success());
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .contains("codec:flac")
        );
        assert_eq!(output.stderr, b"fixture diagnostic\n");
        let output = run_with_limits(fixture("failure"), Duration::from_secs(5), 1024)
            .expect("capture failed fixture");
        assert_eq!(output.status.code(), Some(7));
    }

    /// A silent helper cannot outlive the deadline even without an I/O timeout.
    #[test]
    fn probe_kills_and_reaps_timed_out_helper() {
        let started = Instant::now();
        let error = run_with_limits(fixture("timeout"), Duration::from_millis(100), 1024)
            .expect_err("silent probe must time out");
        assert!(error.contains("deadline"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// Output bounds apply to either pipe without waiting for the helper to exit.
    #[test]
    fn probe_kills_helper_after_either_output_limit() {
        for pipe in ["stdout", "stderr"] {
            let error = run_with_limits(fixture(pipe), Duration::from_secs(5), 1024)
                .expect_err("oversized output must fail");
            assert!(
                error.contains(pipe) && error.contains("exceeds 1024"),
                "{error}"
            );
        }
    }
}
