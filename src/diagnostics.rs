//! Privacy-conscious diagnostic reports for recoverable errors and panics.
//!
//! Reports intentionally contain a small, fixed set of debugging facts. They
//! never enumerate the process environment or serialize Youta's configuration.
//! Free-form fields and platform-file reads are bounded, while the dependency
//! list and every captured backtrace frame are retained.

use std::backtrace::Backtrace;
use std::error::Error;
use std::fmt::{self, Write as _};
use std::fs::File;
use std::io::Read as _;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

const EMBEDDED_CARGO_LOCK: &str = include_str!("../Cargo.lock");
const MAX_ERROR_BYTES: usize = 64 * 1024;
const MAX_ERROR_SOURCES: usize = 32;
const MAX_OS_RELEASE_BYTES: usize = 64 * 1024;
const MAX_FIELD_BYTES: usize = 4 * 1024;
const MAX_HELPERS: usize = 64;
const MAX_HELPER_OUTPUT_BYTES: usize = 16 * 1024;
const MAX_HELPER_VERSION_BYTES: usize = 512;
const HELPER_PROBE_TIMEOUT: Duration = Duration::from_millis(1_500);
const HELPER_OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(500);

/// The diagnostic report format emitted by this Youta release.
pub const DIAGNOSTIC_FORMAT_VERSION: u32 = 2;

/// An external media helper whose version command is known to Youta.
///
/// Each variant maps to fixed arguments. Callers may select the executable
/// path, but cannot inject diagnostic command arguments.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalHelperKind {
    /// The `mpv` media player, queried with `--version`.
    Mpv,
    /// The `yt-dlp` media extractor, queried with `--version`.
    YtDlp,
    /// The `ffmpeg` media converter, queried with `-version`.
    Ffmpeg,
    /// The `ffprobe` media inspector, queried with `-version`.
    Ffprobe,
}

impl ExternalHelperKind {
    const fn name(self) -> &'static str {
        match self {
            Self::Mpv => "mpv",
            Self::YtDlp => "yt-dlp",
            Self::Ffmpeg => "ffmpeg",
            Self::Ffprobe => "ffprobe",
        }
    }

    const fn version_arguments(self) -> &'static [&'static str] {
        match self {
            Self::Mpv | Self::YtDlp => &["--version"],
            Self::Ffmpeg | Self::Ffprobe => &["-version"],
        }
    }
}

/// Outcome of an explicitly requested external-helper version probe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExternalHelperProbeStatus {
    /// No process was launched; this is the default for [`ExternalHelper::new`].
    NotProbed,
    /// The helper returned a bounded, redacted version description.
    Available {
        /// One-line version output captured from standard output and error.
        version: String,
    },
    /// No executable was configured, or the configured executable was absent.
    Unavailable,
    /// The version command exceeded Youta's fixed probe deadline.
    TimedOut,
    /// The version command could not run or did not complete successfully.
    Failed {
        /// A bounded and redacted one-line failure description.
        detail: String,
    },
}

/// A configured external executable that may be relevant to an error.
///
/// Only the executable is accepted: command arguments can contain media URLs,
/// tokens, or other private values and must not be passed here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalHelper {
    /// Human-readable helper name, such as `mpv` or `yt-dlp`.
    pub name: String,
    /// Configured executable name or path, without command arguments.
    pub executable: Option<PathBuf>,
    /// Result of an explicit version probe, or [`ExternalHelperProbeStatus::NotProbed`].
    pub probe_status: ExternalHelperProbeStatus,
}

impl ExternalHelper {
    /// Creates an unprobed diagnostic description for a configured executable.
    ///
    /// This constructor never launches a process. Use [`Self::probe`] lazily
    /// while preparing an error report when runtime helper versions are useful.
    #[must_use]
    pub fn new(name: impl Into<String>, executable: Option<impl Into<PathBuf>>) -> Self {
        Self {
            name: name.into(),
            executable: executable.map(Into::into),
            probe_status: ExternalHelperProbeStatus::NotProbed,
        }
    }

    /// Creates a diagnostic description for a helper that is not configured.
    #[must_use]
    pub fn unavailable(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            executable: None,
            probe_status: ExternalHelperProbeStatus::Unavailable,
        }
    }

    /// Lazily probes one known helper using fixed version arguments.
    ///
    /// The executable is invoked directly without a shell. Standard output and
    /// standard error are drained concurrently into one strict byte budget.
    /// The process has a fixed deadline and is killed and reaped on timeout.
    /// Probe output is redacted and reduced to one bounded line.
    ///
    /// Passing `None` performs no process launch and returns an unavailable
    /// helper. Diagnostic report capture itself never calls this function.
    #[must_use]
    pub fn probe(kind: ExternalHelperKind, executable: Option<PathBuf>) -> Self {
        probe_helper_with_timeout(kind, executable, HELPER_PROBE_TIMEOUT)
    }

    /// Lazily probes one configured known helper using fixed version arguments.
    ///
    /// This is a convenience wrapper around [`Self::probe`] for a configured
    /// executable.
    #[must_use]
    pub fn probe_configured(kind: ExternalHelperKind, executable: impl Into<PathBuf>) -> Self {
        Self::probe(kind, Some(executable.into()))
    }

    /// Probes several known helpers concurrently while preserving input order.
    ///
    /// At most 64 entries are accepted, matching the diagnostic-report bound.
    /// Each executable is invoked directly with its fixed version argument,
    /// and every probe retains the same independent timeout and output limits
    /// as [`Self::probe`]. A panic inside one probe is converted into a failed
    /// status instead of discarding results from the other helpers.
    #[must_use]
    pub fn probe_many(
        helpers: impl IntoIterator<Item = (ExternalHelperKind, Option<PathBuf>)>,
    ) -> Vec<Self> {
        probe_helpers_with_timeout(helpers, HELPER_PROBE_TIMEOUT)
    }
}

/// A redacted external-helper entry included in a diagnostic report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalHelperInfo {
    /// Redacted human-readable helper name.
    pub name: String,
    /// Redacted executable name or path, or `None` when it is not configured.
    pub executable: Option<String>,
    /// Redacted outcome of an optional, explicit version probe.
    pub probe_status: ExternalHelperProbeStatus,
}

/// A package and exact version parsed from Youta's embedded `Cargo.lock`.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct LockedPackage {
    /// Cargo package name.
    pub name: String,
    /// Exact resolved Cargo package version.
    pub version: String,
}

/// Best-effort operating-system identity collected without launching commands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperatingSystemInfo {
    /// Display name, such as `Debian GNU/Linux 13` or the Rust target OS.
    pub name: String,
    /// Distribution or operating-system version when the platform exposes one.
    pub version: Option<String>,
}

/// A self-contained, redacted error report suitable for a TUI popup or log.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticReport {
    /// Youta package version captured at compile time.
    pub youta_version: &'static str,
    /// Redacted recoverable-error or panic description.
    pub error: String,
    /// Rust target operating-system identifier.
    pub target_os: &'static str,
    /// Rust target architecture identifier.
    pub target_arch: &'static str,
    /// Rust target family identifier.
    pub target_family: &'static str,
    /// Best-effort operating-system name and version.
    pub operating_system: OperatingSystemInfo,
    /// Enabled Youta Cargo feature names.
    pub enabled_features: Vec<&'static str>,
    /// Configured helper executables, with private home paths redacted.
    pub external_helpers: Vec<ExternalHelperInfo>,
    /// Every package and exact version from the embedded `Cargo.lock`.
    pub locked_packages: Vec<LockedPackage>,
    /// A forced backtrace captured at the report creation site.
    pub backtrace: String,
    /// Explicit notes describing any bounded or omitted diagnostic data.
    pub truncation_notes: Vec<String>,
}

impl DiagnosticReport {
    /// Captures a report for a recoverable [`Error`], including its source chain.
    ///
    /// The error chain is redacted and bounded. The forced backtrace and locked
    /// package list are never truncated.
    #[must_use]
    pub fn capture_error(
        error: &(dyn Error + 'static),
        helpers: impl IntoIterator<Item = ExternalHelper>,
    ) -> Self {
        let (message, error_was_truncated) = format_error_chain(error);
        Self::capture_inner(message, error_was_truncated, helpers)
    }

    /// Captures a report for a displayable recoverable-error message.
    ///
    /// Prefer [`Self::capture_error`] when an error source chain is available.
    /// The forced backtrace and locked package list are never truncated.
    #[must_use]
    pub fn capture_message(
        message: impl fmt::Display,
        helpers: impl IntoIterator<Item = ExternalHelper>,
    ) -> Self {
        let redacted = redact_diagnostic_text(&message.to_string());
        let (message, error_was_truncated) = truncate_utf8(&redacted, MAX_ERROR_BYTES);
        Self::capture_inner(message, error_was_truncated, helpers)
    }

    /// Replaces helper details while retaining the original error backtrace.
    ///
    /// This supports panic handling: Youta can capture the backtrace at the
    /// panic site, restore terminal state, then perform explicit helper probes
    /// outside the panic hook. Helper fields are redacted and bounded exactly
    /// as they are during initial report capture.
    #[must_use]
    pub fn with_external_helpers(
        mut self,
        helpers: impl IntoIterator<Item = ExternalHelper>,
    ) -> Self {
        let helper_limit_note =
            format!("External-helper diagnostics were limited to {MAX_HELPERS} entries.");
        self.truncation_notes
            .retain(|note| note != &helper_limit_note);
        let (external_helpers, helpers_were_truncated) = sanitize_helpers(helpers);
        self.external_helpers = external_helpers;
        if helpers_were_truncated {
            self.truncation_notes.push(helper_limit_note);
        }
        self
    }

    fn capture_inner(
        error: String,
        error_was_truncated: bool,
        helpers: impl IntoIterator<Item = ExternalHelper>,
    ) -> Self {
        let (operating_system, os_release_was_truncated) = operating_system_info();
        let (external_helpers, helpers_were_truncated) = sanitize_helpers(helpers);
        let backtrace = redact_diagnostic_text(&Backtrace::force_capture().to_string());
        let mut truncation_notes = Vec::new();

        if error_was_truncated {
            truncation_notes.push(format!(
                "Error text was truncated at {MAX_ERROR_BYTES} bytes."
            ));
        }
        if os_release_was_truncated {
            truncation_notes.push(format!(
                "The OS release file was read only up to {MAX_OS_RELEASE_BYTES} bytes."
            ));
        }
        if helpers_were_truncated {
            truncation_notes.push(format!(
                "External-helper diagnostics were limited to {MAX_HELPERS} entries."
            ));
        }

        Self {
            youta_version: env!("CARGO_PKG_VERSION"),
            error,
            target_os: std::env::consts::OS,
            target_arch: std::env::consts::ARCH,
            target_family: std::env::consts::FAMILY,
            operating_system,
            enabled_features: enabled_compile_features(),
            external_helpers,
            locked_packages: locked_packages(),
            backtrace,
            truncation_notes,
        }
    }

    /// Renders the complete report as plain text for terminal display or copy.
    ///
    /// Arbitrary error/platform/helper fields are bounded before rendering.
    /// The full captured backtrace and embedded-lockfile package list remain in
    /// the output even when that makes the report larger than the field budget.
    #[must_use]
    pub fn render(&self) -> String {
        let mut output = String::new();
        let _ = writeln!(output, "Youta diagnostic report");
        let _ = writeln!(output, "Report format: {DIAGNOSTIC_FORMAT_VERSION}");
        let _ = writeln!(output, "Youta version: {}", self.youta_version);
        let _ = writeln!(
            output,
            "Target: {} / {} / {}",
            self.target_os, self.target_arch, self.target_family
        );
        let _ = write!(output, "Operating system: {}", self.operating_system.name);
        if let Some(version) = &self.operating_system.version {
            let _ = write!(output, " ({version})");
        }
        output.push('\n');

        output.push_str("Enabled compile features:");
        if self.enabled_features.is_empty() {
            output.push_str(" none\n");
        } else {
            output.push(' ');
            output.push_str(&self.enabled_features.join(", "));
            output.push('\n');
        }

        output.push_str("External helpers:\n");
        if self.external_helpers.is_empty() {
            output.push_str("- none supplied\n");
        } else {
            for helper in &self.external_helpers {
                let executable = helper.executable.as_deref().unwrap_or("not configured");
                let status = match &helper.probe_status {
                    ExternalHelperProbeStatus::NotProbed => "version not probed".to_owned(),
                    ExternalHelperProbeStatus::Available { version } => {
                        format!("version: {version}")
                    }
                    ExternalHelperProbeStatus::Unavailable => "unavailable".to_owned(),
                    ExternalHelperProbeStatus::TimedOut => "version probe timed out".to_owned(),
                    ExternalHelperProbeStatus::Failed { detail } => {
                        format!("version probe failed: {detail}")
                    }
                };
                let _ = writeln!(output, "- {}: {executable} ({status})", helper.name);
            }
        }

        output.push_str("Error:\n");
        output.push_str(&self.error);
        output.push('\n');

        let _ = writeln!(
            output,
            "Cargo.lock packages ({}):",
            self.locked_packages.len()
        );
        for package in &self.locked_packages {
            let _ = writeln!(output, "- {} {}", package.name, package.version);
        }

        output.push_str("Forced backtrace:\n");
        output.push_str(&self.backtrace);
        if !self.backtrace.ends_with('\n') {
            output.push('\n');
        }

        output.push_str("Privacy and truncation policy:\n");
        output.push_str(
            "- Environment variables, configuration contents, command arguments, \
             tokens, and user-specific home paths are not collected.\n",
        );
        output.push_str(
            "- Free-form fields use fixed byte limits; the package list and \
             backtrace frames are retained in full.\n",
        );
        if self.truncation_notes.is_empty() {
            output.push_str("- No bounded input reached its limit.\n");
        } else {
            for note in &self.truncation_notes {
                let _ = writeln!(output, "- {note}");
            }
        }
        output
    }
}

/// Formats the payload and source location supplied to a panic hook.
///
/// Non-string payloads are deliberately not debug-formatted because arbitrary
/// panic payload objects may contain private application data.
#[must_use]
pub fn format_panic(info: &PanicHookInfo<'_>) -> String {
    let payload = if let Some(message) = info.payload().downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = info.payload().downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload (contents omitted)".to_owned()
    };
    let location = info.location().map_or_else(
        || "unknown location".to_owned(),
        |location| {
            format!(
                "{}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            )
        },
    );
    let redacted = redact_diagnostic_text(&format!("panic at {location}: {payload}"));
    truncate_utf8(&redacted, MAX_ERROR_BYTES).0
}

/// Returns all enabled Youta Cargo feature names in stable alphabetical order.
#[must_use]
pub fn enabled_compile_features() -> Vec<&'static str> {
    let mut enabled = Vec::new();
    macro_rules! record_features {
        ($($feature:literal),+ $(,)?) => {
            $(
                if cfg!(feature = $feature) {
                    enabled.push($feature);
                }
            )+
        };
    }

    record_features!(
        "alsa",
        "apple-podcasts",
        "archive-org",
        "archive-rar",
        "archive-zip",
        "backend-mpv",
        "backend-native",
        "bandcamp",
        "bbc-radio",
        "bilibili",
        "bundled-sqlite",
        "dearrow",
        "discord",
        "evernote",
        "funkwhale",
        "generic-ytdlp",
        "google-drive",
        "gpm",
        "gpodder",
        "invidious",
        "jack",
        "jamendo",
        "keyring",
        "lastfm",
        "librivox",
        "litres",
        "local",
        "network",
        "odysee",
        "peertube",
        "pipewire",
        "podcast-index",
        "pulseaudio",
        "radio",
        "rss",
        "rumble",
        "rutube",
        "soundcloud",
        "soundstream",
        "sponsorblock",
        "ssh",
        "telegram",
        "thumbnails",
        "torrent",
        "tracker-music",
        "tui",
        "vimeo",
        "vk",
        "waveform",
        "webdav",
        "wikidata",
        "wikimedia",
        "yandex-disk",
        "yandex-music",
        "youtube-official",
        "yt-dlp",
    );
    enabled
}

/// Parses and returns every package name and exact version embedded in
/// Youta's compile-time `Cargo.lock`.
#[must_use]
pub fn locked_packages() -> Vec<LockedPackage> {
    parse_cargo_lock(EMBEDDED_CARGO_LOCK)
}

/// Redacts common secret assignments, URL credentials/query strings, and
/// user-specific home-directory paths from free-form diagnostic text.
///
/// This is defense in depth, not permission to pass configuration dumps or
/// command arguments to diagnostics. Callers should supply the narrowest error
/// description that is useful for debugging.
#[must_use]
pub fn redact_diagnostic_text(input: &str) -> String {
    let assignments_redacted = redact_sensitive_assignments(input);
    let urls_redacted = redact_urls(&assignments_redacted);
    redact_home_paths(&urls_redacted)
}

#[derive(Debug)]
struct ReaderCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

enum CommandCompletion {
    Exited(ExitStatus),
    TimedOut,
    Failed(String),
}

fn probe_helper_with_timeout(
    kind: ExternalHelperKind,
    executable: Option<PathBuf>,
    timeout: Duration,
) -> ExternalHelper {
    let Some(executable) = executable else {
        return ExternalHelper::unavailable(kind.name());
    };
    let probe_status = run_helper_probe(kind, &executable, timeout);
    ExternalHelper {
        name: kind.name().to_owned(),
        executable: Some(executable),
        probe_status,
    }
}

fn probe_helpers_with_timeout(
    helpers: impl IntoIterator<Item = (ExternalHelperKind, Option<PathBuf>)>,
    timeout: Duration,
) -> Vec<ExternalHelper> {
    let helpers = helpers.into_iter().take(MAX_HELPERS).collect::<Vec<_>>();
    thread::scope(|scope| {
        let probes = helpers
            .into_iter()
            .map(|(kind, executable)| {
                let panic_executable = executable.clone();
                let handle =
                    scope.spawn(move || probe_helper_with_timeout(kind, executable, timeout));
                (kind, panic_executable, handle)
            })
            .collect::<Vec<_>>();

        probes
            .into_iter()
            .map(|(kind, executable, handle)| {
                handle.join().unwrap_or_else(|_| ExternalHelper {
                    name: kind.name().to_owned(),
                    executable,
                    probe_status: ExternalHelperProbeStatus::Failed {
                        detail: "helper version probe panicked".to_owned(),
                    },
                })
            })
            .collect()
    })
}

fn run_helper_probe(
    kind: ExternalHelperKind,
    executable: &Path,
    timeout: Duration,
) -> ExternalHelperProbeStatus {
    let mut command = Command::new(executable);
    command
        .args(kind.version_arguments())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ExternalHelperProbeStatus::Unavailable;
        }
        Err(error) => {
            return ExternalHelperProbeStatus::Failed {
                detail: bounded_helper_line(&format!("could not start helper: {error}")).0,
            };
        }
    };

    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return ExternalHelperProbeStatus::Failed {
            detail: "could not capture helper output".to_owned(),
        };
    };

    let remaining_budget = Arc::new(AtomicUsize::new(MAX_HELPER_OUTPUT_BYTES));
    let stdout_receiver = spawn_bounded_reader(stdout, Arc::clone(&remaining_budget));
    let stderr_receiver = spawn_bounded_reader(stderr, remaining_budget);
    let completion = wait_for_helper(&mut child, timeout);

    let drain_deadline = Instant::now() + HELPER_OUTPUT_DRAIN_GRACE;
    let stdout_capture = receive_reader_capture(&stdout_receiver, drain_deadline);
    let stderr_capture = receive_reader_capture(&stderr_receiver, drain_deadline);

    match completion {
        CommandCompletion::TimedOut => ExternalHelperProbeStatus::TimedOut,
        CommandCompletion::Failed(detail) => ExternalHelperProbeStatus::Failed {
            detail: bounded_helper_line(&detail).0,
        },
        CommandCompletion::Exited(status) => {
            let (stdout, stderr) = match (stdout_capture, stderr_capture) {
                (Ok(stdout), Ok(stderr)) => (stdout, stderr),
                (Err(error), _) | (_, Err(error)) => {
                    return ExternalHelperProbeStatus::Failed {
                        detail: bounded_helper_line(&error).0,
                    };
                }
            };
            let output = helper_output_line(&stdout, &stderr);
            if status.success() {
                if output.is_empty() {
                    ExternalHelperProbeStatus::Failed {
                        detail: "helper returned no version text".to_owned(),
                    }
                } else {
                    ExternalHelperProbeStatus::Available { version: output }
                }
            } else {
                let detail = if output.is_empty() {
                    format!("helper exited with {status}")
                } else {
                    format!("helper exited with {status}: {output}")
                };
                ExternalHelperProbeStatus::Failed {
                    detail: bounded_helper_line(&detail).0,
                }
            }
        }
    }
}

fn spawn_bounded_reader<R>(
    mut reader: R,
    remaining_budget: Arc<AtomicUsize>,
) -> mpsc::Receiver<std::io::Result<ReaderCapture>>
where
    R: std::io::Read + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4 * 1024];
        let mut truncated = false;
        let result = loop {
            match reader.read(&mut buffer) {
                Ok(0) => break Ok(ReaderCapture { bytes, truncated }),
                Ok(read) => {
                    let previous = remaining_budget
                        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                            Some(remaining.saturating_sub(read))
                        })
                        .unwrap_or(0);
                    let retained = previous.min(read);
                    bytes.extend_from_slice(&buffer[..retained]);
                    truncated |= retained < read;
                }
                Err(error) => break Err(error),
            }
        };
        let _ = sender.send(result);
    });
    receiver
}

fn wait_for_helper(child: &mut std::process::Child, timeout: Duration) -> CommandCompletion {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return CommandCompletion::Exited(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return CommandCompletion::TimedOut;
            }
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                thread::sleep(remaining.min(Duration::from_millis(5)));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return CommandCompletion::Failed(format!("could not wait for helper: {error}"));
            }
        }
    }
}

fn receive_reader_capture(
    receiver: &mpsc::Receiver<std::io::Result<ReaderCapture>>,
    deadline: Instant,
) -> Result<ReaderCapture, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match receiver.recv_timeout(remaining) {
        Ok(Ok(capture)) => Ok(capture),
        Ok(Err(error)) => Err(format!("could not read helper output: {error}")),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            Err("helper output did not close after the process exited".to_owned())
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err("helper output reader stopped unexpectedly".to_owned())
        }
    }
}

fn helper_output_line(stdout: &ReaderCapture, stderr: &ReaderCapture) -> String {
    let mut raw = String::from_utf8_lossy(&stdout.bytes).into_owned();
    if !raw.is_empty() && !stderr.bytes.is_empty() {
        raw.push(' ');
    }
    raw.push_str(&String::from_utf8_lossy(&stderr.bytes));
    if stdout.truncated || stderr.truncated {
        raw.push_str(" [output truncated]");
    }
    bounded_helper_line(&raw).0
}

fn bounded_helper_line(input: &str) -> (String, bool) {
    let redacted = redact_diagnostic_text(input);
    let normalized = redacted.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_one_line(&normalized, MAX_HELPER_VERSION_BYTES)
}

fn truncate_one_line(input: &str, limit: usize) -> (String, bool) {
    const SUFFIX: &str = " [truncated]";

    if input.len() <= limit {
        return (input.to_owned(), false);
    }
    let content_limit = limit.saturating_sub(SUFFIX.len());
    let mut end = content_limit;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    let mut output = input[..end].to_owned();
    output.push_str(SUFFIX);
    (output, true)
}

fn format_error_chain(error: &(dyn Error + 'static)) -> (String, bool) {
    let mut output = String::new();
    let mut current = Some(error);
    let mut depth = 0;
    let mut sources_were_truncated = false;

    while let Some(source) = current {
        if depth == MAX_ERROR_SOURCES {
            sources_were_truncated = true;
            break;
        }
        if depth > 0 {
            output.push_str("\ncaused by: ");
        }
        output.push_str(&source.to_string());
        current = source.source();
        depth += 1;
    }

    if sources_were_truncated {
        output.push_str("\n[additional error sources omitted]");
    }
    let redacted = redact_diagnostic_text(&output);
    let (output, bytes_were_truncated) = truncate_utf8(&redacted, MAX_ERROR_BYTES);
    (output, sources_were_truncated || bytes_were_truncated)
}

fn sanitize_helpers(
    helpers: impl IntoIterator<Item = ExternalHelper>,
) -> (Vec<ExternalHelperInfo>, bool) {
    let mut sanitized = Vec::new();
    let mut truncated = false;

    for (index, helper) in helpers.into_iter().enumerate() {
        if index == MAX_HELPERS {
            truncated = true;
            break;
        }
        let name = truncate_utf8(&redact_diagnostic_text(&helper.name), MAX_FIELD_BYTES).0;
        let executable = helper
            .executable
            .as_deref()
            .map(redact_helper_executable)
            .map(|value| truncate_utf8(&value, MAX_FIELD_BYTES).0);
        let probe_status = sanitize_probe_status(helper.probe_status);
        sanitized.push(ExternalHelperInfo {
            name,
            executable,
            probe_status,
        });
    }
    (sanitized, truncated)
}

fn sanitize_probe_status(status: ExternalHelperProbeStatus) -> ExternalHelperProbeStatus {
    match status {
        ExternalHelperProbeStatus::Available { version } => ExternalHelperProbeStatus::Available {
            version: bounded_helper_line(&version).0,
        },
        ExternalHelperProbeStatus::Failed { detail } => ExternalHelperProbeStatus::Failed {
            detail: bounded_helper_line(&detail).0,
        },
        unchanged => unchanged,
    }
}

fn redact_helper_executable(path: &Path) -> String {
    let raw = path.to_string_lossy();
    if contains_home_path(&raw) {
        let file_name = raw
            .rsplit(['/', '\\'])
            .next()
            .filter(|value| !value.is_empty())
            .unwrap_or("<executable>");
        return format!("<redacted-home>/{file_name}");
    }
    redact_diagnostic_text(&raw)
}

fn operating_system_info() -> (OperatingSystemInfo, bool) {
    #[cfg(target_os = "linux")]
    {
        for path in [
            Path::new("/etc/os-release"),
            Path::new("/usr/lib/os-release"),
        ] {
            if let Ok((contents, truncated)) = read_bounded(path, MAX_OS_RELEASE_BYTES)
                && let Some(info) = parse_os_release(&contents)
            {
                return (info, truncated);
            }
        }
    }

    (
        OperatingSystemInfo {
            name: std::env::consts::OS.to_owned(),
            version: None,
        },
        false,
    )
}

fn read_bounded(path: &Path, limit: usize) -> std::io::Result<(String, bool)> {
    let mut bytes = Vec::with_capacity(limit.min(8 * 1024));
    File::open(path)?
        .take(u64::try_from(limit).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut bytes)?;
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    Ok((String::from_utf8_lossy(&bytes).into_owned(), truncated))
}

fn parse_os_release(input: &str) -> Option<OperatingSystemInfo> {
    let mut pretty_name = None;
    let mut name = None;
    let mut version = None;
    let mut version_id = None;

    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = parse_os_release_value(value.trim());
        match key.trim() {
            "PRETTY_NAME" => pretty_name = Some(value),
            "NAME" => name = Some(value),
            "VERSION" => version = Some(value),
            "VERSION_ID" => version_id = Some(value),
            _ => {}
        }
    }

    let display_name = pretty_name.or(name)?;
    let (display_name, _) = truncate_utf8(&redact_diagnostic_text(&display_name), MAX_FIELD_BYTES);
    let version = version
        .or(version_id)
        .map(|value| truncate_utf8(&redact_diagnostic_text(&value), MAX_FIELD_BYTES).0);
    Some(OperatingSystemInfo {
        name: display_name,
        version,
    })
}

fn parse_os_release_value(value: &str) -> String {
    let unquoted = if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        &value[1..value.len() - 1]
    } else {
        value
    };

    let mut parsed = String::with_capacity(unquoted.len());
    let mut characters = unquoted.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            if let Some(escaped) = characters.next() {
                parsed.push(escaped);
            }
        } else {
            parsed.push(character);
        }
    }
    parsed
}

fn parse_cargo_lock(input: &str) -> Vec<LockedPackage> {
    let mut packages = Vec::new();
    let mut inside_package = false;
    let mut name = None;
    let mut version = None;

    let flush = |packages: &mut Vec<LockedPackage>,
                 name: &mut Option<String>,
                 version: &mut Option<String>| {
        if let (Some(name), Some(version)) = (name.take(), version.take()) {
            packages.push(LockedPackage { name, version });
        }
    };

    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed == "[[package]]" {
            if inside_package {
                flush(&mut packages, &mut name, &mut version);
            }
            inside_package = true;
            continue;
        }
        if trimmed.starts_with("[[") {
            if inside_package {
                flush(&mut packages, &mut name, &mut version);
                inside_package = false;
            }
            continue;
        }
        if !inside_package {
            continue;
        }
        if let Some(value) = parse_toml_string_assignment(trimmed, "name") {
            name = Some(value);
        } else if let Some(value) = parse_toml_string_assignment(trimmed, "version") {
            version = Some(value);
        }
    }
    if inside_package {
        flush(&mut packages, &mut name, &mut version);
    }

    packages.sort_unstable();
    packages.dedup();
    packages
}

fn parse_toml_string_assignment(line: &str, expected_key: &str) -> Option<String> {
    let (key, value) = line.split_once('=')?;
    if key.trim() != expected_key {
        return None;
    }
    let value = value.trim();
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    Some(inner.replace("\\\"", "\"").replace("\\\\", "\\"))
}

fn truncate_utf8(input: &str, limit: usize) -> (String, bool) {
    if input.len() <= limit {
        return (input.to_owned(), false);
    }
    let mut end = limit;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    (
        format!("{}\n[truncated after {limit} bytes]", &input[..end]),
        true,
    )
}

fn redact_sensitive_assignments(input: &str) -> String {
    const MARKERS: &[&str] = &[
        "authorization:",
        "authorization=",
        "access_token:",
        "access_token=",
        "api_key:",
        "api_key=",
        "apikey:",
        "apikey=",
        "cookie:",
        "cookie=",
        "password:",
        "password=",
        "secret:",
        "secret=",
        "token:",
        "token=",
    ];

    let mut output = input.to_owned();
    for marker in MARKERS {
        let mut search_from = 0;
        loop {
            let lowercase = output.to_ascii_lowercase();
            let Some(relative_start) = lowercase[search_from..].find(marker) else {
                break;
            };
            let start = search_from + relative_start;
            let value_start = start + marker.len();
            let suffix = &output[value_start..];
            let leading_space_bytes = suffix.len() - suffix.trim_start().len();
            let content_start = value_start + leading_space_bytes;
            let content = &output[content_start..];
            let value_len = if marker.starts_with("authorization") {
                content.find('\n').unwrap_or(content.len())
            } else {
                content
                    .find(|character: char| {
                        character.is_whitespace()
                            || matches!(character, '&' | ',' | ';' | ')' | ']' | '}')
                    })
                    .unwrap_or(content.len())
            };
            output.replace_range(value_start..content_start + value_len, " <redacted>");
            search_from = value_start + " <redacted>".len();
        }
    }
    output
}

fn redact_urls(input: &str) -> String {
    input
        .split_inclusive(char::is_whitespace)
        .map(redact_url_token)
        .collect()
}

fn redact_url_token(token: &str) -> String {
    let Some(scheme_index) = token.find("://") else {
        return token.to_owned();
    };
    let authority_start = scheme_index + 3;
    let authority_end = token[authority_start..]
        .find(['/', '?', '#'])
        .map_or(token.len(), |index| authority_start + index);
    let mut redacted = token.to_owned();

    if let Some(user_info_end) = redacted[authority_start..authority_end].rfind('@') {
        let user_info_end = authority_start + user_info_end;
        redacted.replace_range(authority_start..user_info_end, "<redacted>");
    }

    let private_suffix = redacted
        .char_indices()
        .skip_while(|(index, _)| *index < authority_start)
        .find(|(_, character)| matches!(character, '?' | '#'))
        .map(|(index, _)| index);
    if let Some(private_suffix) = private_suffix {
        let trailing_whitespace = redacted
            .chars()
            .rev()
            .take_while(|character| character.is_whitespace())
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        redacted.truncate(private_suffix);
        redacted.push_str("?<redacted>");
        redacted.push_str(&trailing_whitespace);
    }
    redacted
}

fn redact_home_paths(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut index = 0;

    while index < input.len() {
        let rest = &input[index..];
        let home_prefix_len = home_prefix_len(rest);
        if let Some(prefix_len) = home_prefix_len {
            let path_end = rest[prefix_len..]
                .find(|character: char| {
                    character.is_whitespace()
                        || matches!(character, '"' | '\'' | ')' | ']' | '}' | ',' | ';')
                })
                .map_or(rest.len(), |offset| prefix_len + offset);
            output.push_str("<redacted-home>");
            index += path_end;
        } else {
            let character = rest.chars().next().expect("index remains on a character");
            output.push(character);
            index += character.len_utf8();
        }
    }
    output
}

fn contains_home_path(input: &str) -> bool {
    let lowercase = input.to_ascii_lowercase();
    lowercase.contains("/home/")
        || lowercase.contains("/users/")
        || lowercase.starts_with("/root/")
        || lowercase.contains(":\\users\\")
        || lowercase.starts_with("~/")
        || lowercase.starts_with("~\\")
}

fn home_prefix_len(input: &str) -> Option<usize> {
    let lowercase = input.to_ascii_lowercase();
    if lowercase.starts_with("/home/") {
        Some("/home/".len())
    } else if lowercase.starts_with("/users/") {
        Some("/users/".len())
    } else if lowercase.starts_with("/root/") {
        Some("/root/".len())
    } else if lowercase.len() >= 9
        && lowercase.as_bytes()[0].is_ascii_alphabetic()
        && &lowercase.as_bytes()[1..9] == b":\\users\\"
    {
        Some(9)
    } else if lowercase.starts_with("~/") || lowercase.starts_with("~\\") {
        Some(2)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Write as _};

    #[cfg(unix)]
    const MOCK_HELPER_TIMEOUT: Duration = Duration::from_secs(5);

    #[cfg(unix)]
    struct MockExecutable {
        directory: PathBuf,
        path: PathBuf,
    }

    #[cfg(unix)]
    impl MockExecutable {
        fn new(body: &str) -> Self {
            use std::os::unix::fs::PermissionsExt as _;

            static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
            const READY_ARGUMENT: &str = "__youta_mock_helper_ready__";
            const READY_ATTEMPTS: usize = 50;

            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir()
                .join(format!("youta-helper-probe-{}-{id}", std::process::id()));
            std::fs::create_dir(&directory).expect("create mock helper directory");
            let path = directory.join("mock helper; no shell");
            let writing_path = directory.join("mock helper.writing");
            {
                let mut file =
                    std::fs::File::create(&writing_path).expect("create mock helper staging file");
                file.write_all(
                    format!(
                        "#!/bin/sh\n\
                         if [ \"${{1-}}\" = '{READY_ARGUMENT}' ]; then exit 0; fi\n\
                         {body}\n"
                    )
                    .as_bytes(),
                )
                .expect("write mock helper");
                file.sync_all().expect("sync mock helper");
            }
            let mut permissions = std::fs::metadata(&writing_path)
                .expect("read mock helper metadata")
                .permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(&writing_path, permissions)
                .expect("make staged mock helper executable");
            std::fs::rename(&writing_path, &path).expect("publish closed mock helper atomically");

            // Instrumented parallel tests can briefly observe ETXTBSY after an
            // executable script is published on some filesystems. Prove that
            // the fixture is executable before testing production probe logic.
            let mut last_error = None;
            for _ in 0..READY_ATTEMPTS {
                match std::process::Command::new(&path)
                    .arg(READY_ARGUMENT)
                    .status()
                {
                    Ok(status) if status.success() => {
                        last_error = None;
                        break;
                    }
                    Ok(status) => {
                        panic!("mock helper readiness probe exited with {status}");
                    }
                    Err(error) => {
                        last_error = Some(error);
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }
            if let Some(error) = last_error {
                panic!("mock helper stayed busy after publication: {error}");
            }
            Self { directory, path }
        }
    }

    #[cfg(unix)]
    impl Drop for MockExecutable {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn parses_lock_packages_and_keeps_parallel_versions() {
        let lock = r#"
version = 4

[[package]]
name = "alpha"
version = "1.0.0"
dependencies = [
 "ignored",
]

[[package]]
name = "alpha"
version = "2.0.0"

[[package]]
name = "escaped-name"
version = "3.0.0+meta"
"#;

        assert_eq!(
            parse_cargo_lock(lock),
            vec![
                LockedPackage {
                    name: "alpha".to_owned(),
                    version: "1.0.0".to_owned(),
                },
                LockedPackage {
                    name: "alpha".to_owned(),
                    version: "2.0.0".to_owned(),
                },
                LockedPackage {
                    name: "escaped-name".to_owned(),
                    version: "3.0.0+meta".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn embedded_lock_contains_youtas_direct_dependencies() {
        let packages = locked_packages();
        assert!(packages.iter().any(|package| package.name == "anyhow"));
        assert!(packages.iter().any(|package| package.name == "rusqlite"));
        assert!(packages.iter().any(|package| package.name == "youta"));
        assert!(packages.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[test]
    fn parses_linux_os_release_without_executing_commands() {
        let info = parse_os_release(
            r#"
NAME=Example
VERSION_ID="42"
PRETTY_NAME="Example Linux \"Quokka\""
VERSION="42 (Stable)"
"#,
        )
        .expect("fixture has a name");

        assert_eq!(info.name, "Example Linux \"Quokka\"");
        assert_eq!(info.version.as_deref(), Some("42 (Stable)"));
    }

    #[test]
    fn redacts_secrets_urls_and_home_paths() {
        let input = concat!(
            "token=hunter2 password: swordfish\n",
            "Authorization: Bearer abc.def\n",
            "request https://alice:password@example.test/watch?v=private#position\n",
            "at /home/alice/projects/youta/src/main.rs:10\n",
            "at C:\\Users\\Alice\\projects\\youta.exe"
        );
        let redacted = redact_diagnostic_text(input);

        assert!(!redacted.contains("hunter2"));
        assert!(!redacted.contains("swordfish"));
        assert!(!redacted.contains("abc.def"));
        assert!(!redacted.contains("alice:password"));
        assert!(!redacted.contains("private"));
        assert!(!redacted.contains("/home/alice"));
        assert!(!redacted.contains("\\Users\\Alice"));
        assert!(redacted.contains("<redacted>"));
        assert!(redacted.contains("<redacted-home>"));
    }

    #[test]
    fn helper_paths_preserve_system_paths_but_hide_home_paths() {
        let (helpers, truncated) = sanitize_helpers([
            ExternalHelper::new("mpv", Some("/usr/bin/mpv")),
            ExternalHelper::new("yt-dlp", Some("/home/alice/.local/bin/yt-dlp")),
            ExternalHelper::unavailable("ffmpeg"),
        ]);

        assert!(!truncated);
        assert_eq!(helpers[0].executable.as_deref(), Some("/usr/bin/mpv"));
        assert_eq!(
            helpers[1].executable.as_deref(),
            Some("<redacted-home>/yt-dlp")
        );
        assert_eq!(helpers[2].executable, None);
        assert_eq!(
            helpers[0].probe_status,
            ExternalHelperProbeStatus::NotProbed
        );
        assert_eq!(
            helpers[2].probe_status,
            ExternalHelperProbeStatus::Unavailable
        );
    }

    #[cfg(unix)]
    #[test]
    fn helper_probe_uses_direct_execution_and_fixed_arguments() {
        let executable = MockExecutable::new("printf 'mock mpv 1.2 args=%s\\n' \"$*\"");

        let helper = probe_helper_with_timeout(
            ExternalHelperKind::Mpv,
            Some(executable.path.clone()),
            MOCK_HELPER_TIMEOUT,
        );

        assert_eq!(helper.name, "mpv");
        assert_eq!(
            helper.executable.as_deref(),
            Some(executable.path.as_path())
        );
        assert_eq!(
            helper.probe_status,
            ExternalHelperProbeStatus::Available {
                version: "mock mpv 1.2 args=--version".to_owned(),
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn helper_probes_run_concurrently_and_preserve_input_order() {
        let yt_dlp = MockExecutable::new("printf 'mock yt-dlp 2026.07.25\\n'");
        let ffmpeg = MockExecutable::new("printf 'mock ffmpeg 9.0\\n' >&2");

        let helpers = probe_helpers_with_timeout(
            [
                (ExternalHelperKind::YtDlp, Some(yt_dlp.path.clone())),
                (ExternalHelperKind::Mpv, None),
                (ExternalHelperKind::Ffmpeg, Some(ffmpeg.path.clone())),
            ],
            MOCK_HELPER_TIMEOUT,
        );

        assert_eq!(
            helpers
                .iter()
                .map(|helper| helper.name.as_str())
                .collect::<Vec<_>>(),
            ["yt-dlp", "mpv", "ffmpeg"]
        );
        assert_eq!(
            helpers[0].probe_status,
            ExternalHelperProbeStatus::Available {
                version: "mock yt-dlp 2026.07.25".to_owned(),
            }
        );
        assert_eq!(
            helpers[1].probe_status,
            ExternalHelperProbeStatus::Unavailable
        );
        assert_eq!(
            helpers[2].probe_status,
            ExternalHelperProbeStatus::Available {
                version: "mock ffmpeg 9.0".to_owned(),
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn helper_probe_accepts_version_text_on_standard_error() {
        let executable = MockExecutable::new("printf 'ffmpeg mock 7.0\\n' >&2");

        let helper = probe_helper_with_timeout(
            ExternalHelperKind::Ffmpeg,
            Some(executable.path.clone()),
            MOCK_HELPER_TIMEOUT,
        );

        assert_eq!(
            helper.probe_status,
            ExternalHelperProbeStatus::Available {
                version: "ffmpeg mock 7.0".to_owned(),
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn helper_probe_concurrently_drains_oversized_output_into_strict_bounds() {
        let executable = MockExecutable::new(
            "printf '%020000d\\n' 0\n\
             printf '%020000d\\n' 0 >&2",
        );

        let helper = probe_helper_with_timeout(
            ExternalHelperKind::YtDlp,
            Some(executable.path.clone()),
            Duration::from_secs(5),
        );

        let ExternalHelperProbeStatus::Available { version } = helper.probe_status else {
            panic!(
                "oversized output should still produce a version, got {:?}",
                helper.probe_status
            );
        };
        assert!(version.len() <= MAX_HELPER_VERSION_BYTES);
        assert!(version.contains("[truncated]"));
        assert!(!version.contains('\n'));
    }

    #[cfg(unix)]
    #[test]
    fn helper_probe_has_a_hard_timeout_and_reaps_the_child() {
        let executable = MockExecutable::new("while :; do :; done");
        let started = Instant::now();

        let helper = probe_helper_with_timeout(
            ExternalHelperKind::Ffprobe,
            Some(executable.path.clone()),
            Duration::from_millis(25),
        );

        assert_eq!(helper.probe_status, ExternalHelperProbeStatus::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn helper_probe_redacts_secrets_before_reporting_version_text() {
        let executable = MockExecutable::new(
            "printf '%s\\n' \
             'token=hunter2 https://alice:password@example.test/watch?v=private \
             /home/alice/bin/tool'",
        );

        let helper = probe_helper_with_timeout(
            ExternalHelperKind::YtDlp,
            Some(executable.path.clone()),
            MOCK_HELPER_TIMEOUT,
        );

        let ExternalHelperProbeStatus::Available { version } = helper.probe_status else {
            panic!(
                "mock helper should be available, got {:?}",
                helper.probe_status
            );
        };
        assert!(!version.contains("hunter2"));
        assert!(!version.contains("alice:password"));
        assert!(!version.contains("private"));
        assert!(!version.contains("/home/alice"));
        assert!(version.contains("<redacted>"));
        assert!(version.contains("<redacted-home>"));
    }

    #[cfg(unix)]
    #[test]
    fn helper_probe_reports_bounded_nonzero_exit_details() {
        let executable =
            MockExecutable::new("printf 'password=swordfish failed to start\\n' >&2\nexit 7");

        let helper = probe_helper_with_timeout(
            ExternalHelperKind::Ffmpeg,
            Some(executable.path.clone()),
            MOCK_HELPER_TIMEOUT,
        );

        let ExternalHelperProbeStatus::Failed { detail } = helper.probe_status else {
            panic!("nonzero helper exit should fail");
        };
        assert!(detail.contains('7'));
        assert!(!detail.contains("swordfish"));
        assert!(detail.len() <= MAX_HELPER_VERSION_BYTES);
        assert!(!detail.contains('\n'));
    }

    #[test]
    fn helper_probe_reports_missing_executables_as_unavailable() {
        let helper = probe_helper_with_timeout(
            ExternalHelperKind::Mpv,
            Some(PathBuf::from(
                "/definitely/not/a/youta-test-helper-executable",
            )),
            Duration::from_millis(10),
        );

        assert_eq!(helper.probe_status, ExternalHelperProbeStatus::Unavailable);
    }

    #[test]
    fn helper_probe_is_explicit_and_report_rendering_preserves_statuses() {
        let unprobed = ExternalHelper::new("mpv", Some("mpv"));
        let unavailable = ExternalHelper::probe(ExternalHelperKind::Ffmpeg, None);
        let report = DiagnosticReport::capture_message("example", [unprobed.clone(), unavailable]);
        let rendered = report.render();

        assert_eq!(unprobed.probe_status, ExternalHelperProbeStatus::NotProbed);
        assert!(rendered.contains("mpv: mpv (version not probed)"));
        assert!(rendered.contains("ffmpeg: not configured (unavailable)"));
    }

    #[test]
    fn report_can_add_bounded_helpers_without_losing_the_original_backtrace() {
        let report = DiagnosticReport::capture_message(
            "panic-site message",
            [ExternalHelper::new("old", Some("old"))],
        );
        let original_backtrace = report.backtrace.clone();
        let helpers = (0..=MAX_HELPERS)
            .map(|index| ExternalHelper::unavailable(format!("replacement-helper-{index}")));

        let report = report.with_external_helpers(helpers);

        assert_eq!(report.backtrace, original_backtrace);
        assert_eq!(report.external_helpers.len(), MAX_HELPERS);
        assert_eq!(report.external_helpers[0].name, "replacement-helper-0");
        assert!(
            report
                .truncation_notes
                .iter()
                .any(|note| note.contains("limited to 64 entries"))
        );
    }

    #[test]
    fn utf8_truncation_ends_at_a_character_boundary() {
        let (truncated, did_truncate) = truncate_utf8("aé日", 4);

        assert!(did_truncate);
        assert!(truncated.starts_with("aé"));
        assert!(!truncated.starts_with("aé日"));
    }

    #[test]
    fn report_contains_debug_facts_and_full_lock_list() {
        let error = io::Error::other("failed token=secret at /home/alice/media");
        let report = DiagnosticReport::capture_error(
            &error,
            [ExternalHelper::new("mpv", Some("/usr/bin/mpv"))],
        );
        let rendered = report.render();

        assert_eq!(report.locked_packages, locked_packages());
        assert!(!report.backtrace.is_empty());
        assert!(rendered.contains(env!("CARGO_PKG_VERSION")));
        assert!(rendered.contains(std::env::consts::OS));
        assert!(rendered.contains("Cargo.lock packages"));
        assert!(rendered.contains("Forced backtrace"));
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("/home/alice"));
    }

    #[test]
    fn report_records_error_truncation() {
        let report = DiagnosticReport::capture_message("x".repeat(MAX_ERROR_BYTES + 1), Vec::new());

        assert!(
            report
                .truncation_notes
                .iter()
                .any(|note| note.contains("Error text"))
        );
        assert!(report.error.contains("[truncated after"));
    }

    #[test]
    fn enabled_features_are_sorted_and_unique() {
        let features = enabled_compile_features();

        assert!(features.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
