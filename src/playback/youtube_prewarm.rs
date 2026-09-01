//! Cancellable, resource-bounded pre-resolution for `YouTube` audio.
//!
//! The normal mpv `ytdl_hook` path should remain the authoritative fallback:
//! signed media URLs can expire, depend on the current network address, or
//! require extractor-specific state. This module exists only to move a
//! selected video's `yt-dlp` work off the Enter-to-audio critical path.
//!
//! [`YouTubePrewarmResolver::resolve`] is synchronous and must run on a worker
//! thread. A request and its completion carry the same monotonically increasing
//! generation, allowing a controller to discard results for an older
//! selection. [`YouTubePrewarmCancellation`] lets that controller terminate a
//! superseded child process promptly.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use rustix::io::Errno;
#[cfg(unix)]
use rustix::process::{Pid, Signal, kill_process_group};
use serde::Deserialize;
use thiserror::Error;
use url::Url;

use super::{PlaybackHttpHeaders, ytdlp::ADDITIONAL_JS_RUNTIME};

const DEFAULT_AUDIO_FORMAT: &str = "bestaudio[acodec^=opus]/bestaudio";
const PRINT_TEMPLATE: &str = "%(.{url,http_headers,acodec,vcodec,protocol})j";
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const OUTPUT_CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_CONFIGURED_TIMEOUT: Duration = Duration::from_mins(5);
const MAX_CONFIGURED_JSON_BYTES: usize = 1024 * 1024;
const MAX_CONFIGURED_STDERR_BYTES: usize = 256 * 1024;
const MAX_SOURCE_URL_BYTES: usize = 8 * 1024;
const MAX_AUDIO_FORMAT_BYTES: usize = 512;
const MAX_HTTP_HEADERS: usize = 32;
const MAX_HTTP_HEADER_BYTES: usize = 16 * 1024;
const MAX_CODEC_BYTES: usize = 128;
const MAX_PROTOCOL_BYTES: usize = 128;

/// Closed `YouTube` player-client policies permitted by the bounded resolver.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum YouTubePlayerClientPolicy {
    /// Let yt-dlp choose its normal anonymous clients.
    #[default]
    Default,
    /// Prefer the cookie-free embedded client, then try yt-dlp's defaults.
    EmbeddedThenDefault,
}

impl YouTubePlayerClientPolicy {
    const fn extractor_argument(self) -> Option<&'static str> {
        match self {
            Self::Default => None,
            Self::EmbeddedThenDefault => Some("youtube:player_client=web_embedded,default"),
        }
    }
}

/// Process and memory limits for speculative `YouTube` audio resolution.
#[derive(Clone, Eq, PartialEq)]
pub struct YouTubePrewarmConfig {
    /// Executable path or name.
    pub executable: PathBuf,
    /// Whole-operation timeout, including extractor network work.
    pub timeout: Duration,
    /// Maximum retained bytes from machine-readable standard output.
    pub max_json_bytes: usize,
    /// Maximum retained bytes from diagnostic standard error.
    ///
    /// The pipe is drained after this limit, but additional bytes are
    /// discarded so a noisy helper cannot grow Youta's memory without bound.
    pub max_stderr_bytes: usize,
    /// Whether explicitly installed `yt-dlp` plugins may execute.
    ///
    /// User and system `yt-dlp` configuration files remain disabled either
    /// way. Enabling plugins is an execution-trust decision and should be an
    /// explicit user preference when this resolver is wired into the app.
    pub allow_plugins: bool,
    /// Allowlisted player-client sequence used for anonymous extraction.
    pub player_client_policy: YouTubePlayerClientPolicy,
    /// `yt-dlp` format expression used to select one audio-only stream.
    pub audio_format: String,
}

impl Default for YouTubePrewarmConfig {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("yt-dlp"),
            timeout: Duration::from_secs(15),
            max_json_bytes: 128 * 1024,
            max_stderr_bytes: 8 * 1024,
            allow_plugins: false,
            player_client_policy: YouTubePlayerClientPolicy::Default,
            audio_format: DEFAULT_AUDIO_FORMAT.to_owned(),
        }
    }
}

impl fmt::Debug for YouTubePrewarmConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("YouTubePrewarmConfig")
            .field("executable", &"<configured>")
            .field("timeout", &self.timeout)
            .field("max_json_bytes", &self.max_json_bytes)
            .field("max_stderr_bytes", &self.max_stderr_bytes)
            .field("allow_plugins", &self.allow_plugins)
            .field("player_client_policy", &self.player_client_policy)
            .field("audio_format", &"<configured>")
            .finish()
    }
}

/// One selection-generation request for a canonical `YouTube` watch URL.
#[derive(Clone, Eq, PartialEq)]
pub struct YouTubePrewarmRequest {
    generation: u64,
    source_url: Url,
}

impl YouTubePrewarmRequest {
    /// Creates a request tagged with the controller's current selection
    /// generation.
    #[must_use]
    pub const fn new(generation: u64, source_url: Url) -> Self {
        Self {
            generation,
            source_url,
        }
    }

    /// Returns the selection generation copied into the completion.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the stable watch-page URL given to `yt-dlp`.
    #[must_use]
    pub const fn source_url(&self) -> &Url {
        &self.source_url
    }
}

impl fmt::Debug for YouTubePrewarmRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("YouTubePrewarmRequest")
            .field("generation", &self.generation)
            .field("source_url", &"<redacted-youtube-url>")
            .finish()
    }
}

/// Cloneable cancellation signal for one or more superseded prewarm requests.
#[derive(Clone, Default)]
pub struct YouTubePrewarmCancellation {
    cancelled: Arc<AtomicBool>,
}

impl YouTubePrewarmCancellation {
    /// Creates an unset cancellation signal.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation.
    ///
    /// A running resolver checks this signal at least once per process-poll
    /// interval, kills the exact child it spawned, and waits for that child to
    /// be reaped before returning.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Returns whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl fmt::Debug for YouTubePrewarmCancellation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("YouTubePrewarmCancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

/// One freshly resolved, short-lived `YouTube` audio stream.
#[derive(Clone, Eq, PartialEq)]
pub struct PrewarmedYouTubeAudio {
    media_url: Url,
    http_headers: PlaybackHttpHeaders,
    expires_at_unix: Option<u64>,
    audio_codec: String,
    protocol: Option<String>,
}

impl PrewarmedYouTubeAudio {
    /// Returns the short-lived media URL.
    ///
    /// Callers must keep this URL in memory, avoid logs and diagnostics, and
    /// fall back to a fresh canonical watch-page load after any open failure.
    #[must_use]
    pub const fn media_url(&self) -> &Url {
        &self.media_url
    }

    /// Returns the sensitive request headers required by the media URL.
    #[must_use]
    pub const fn http_headers(&self) -> &PlaybackHttpHeaders {
        &self.http_headers
    }

    /// Returns the signed URL's advertised Unix expiry when it has one.
    ///
    /// This value is only an upper bound. A network, proxy, cookie, client, or
    /// token change can invalidate the URL earlier.
    #[must_use]
    pub const fn expires_at_unix(&self) -> Option<u64> {
        self.expires_at_unix
    }

    /// Returns the selected audio codec reported by `yt-dlp`.
    #[must_use]
    pub fn audio_codec(&self) -> &str {
        &self.audio_codec
    }

    /// Returns the extractor protocol label when reported.
    #[must_use]
    pub fn protocol(&self) -> Option<&str> {
        self.protocol.as_deref()
    }

    /// Consumes the value into the two fields required by
    /// [`super::PlaybackInput`].
    #[must_use]
    pub fn into_playback_parts(self) -> (Url, PlaybackHttpHeaders) {
        (self.media_url, self.http_headers)
    }

    /// Constructs a resolved stream for controller state-machine tests.
    #[cfg(all(test, feature = "controller"))]
    pub(crate) fn for_test(media_url: Url, expires_at_unix: Option<u64>) -> Self {
        Self {
            media_url,
            http_headers: PlaybackHttpHeaders::default(),
            expires_at_unix,
            audio_codec: "opus".to_owned(),
            protocol: Some("https".to_owned()),
        }
    }
}

impl fmt::Debug for PrewarmedYouTubeAudio {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrewarmedYouTubeAudio")
            .field("media_url", &"<redacted-signed-media-url>")
            .field("http_header_count", &self.http_headers.iter().len())
            .field("expires_at_unix", &self.expires_at_unix)
            .field("audio_codec", &self.audio_codec)
            .field("protocol", &self.protocol)
            .finish()
    }
}

/// Completion for one generation-tagged prewarm request.
///
/// Failures retain the request generation too, so a controller can discard a
/// stale failure without replacing the status of a newer selection.
pub struct YouTubePrewarmResult {
    generation: u64,
    outcome: Result<PrewarmedYouTubeAudio, YouTubePrewarmError>,
}

impl YouTubePrewarmResult {
    /// Constructs a completion for controller generation tests.
    #[cfg(all(test, feature = "controller"))]
    pub(crate) const fn for_test(
        generation: u64,
        outcome: Result<PrewarmedYouTubeAudio, YouTubePrewarmError>,
    ) -> Self {
        Self {
            generation,
            outcome,
        }
    }

    /// Returns the request generation that produced this completion.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns whether this completion belongs to the controller's latest
    /// selection generation.
    #[must_use]
    pub const fn is_latest(&self, latest_generation: u64) -> bool {
        self.generation == latest_generation
    }

    /// Borrows the resolution outcome.
    ///
    /// # Errors
    ///
    /// Returns the stored validation, process, cancellation, timeout, or
    /// parsing error when the request did not produce a usable audio stream.
    pub fn outcome(&self) -> Result<&PrewarmedYouTubeAudio, &YouTubePrewarmError> {
        self.outcome.as_ref()
    }

    /// Consumes the completion and returns its resolution outcome.
    ///
    /// # Errors
    ///
    /// Returns the stored validation, process, cancellation, timeout, or
    /// parsing error when the request did not produce a usable audio stream.
    pub fn into_outcome(self) -> Result<PrewarmedYouTubeAudio, YouTubePrewarmError> {
        self.outcome
    }
}

impl fmt::Debug for YouTubePrewarmResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = match &self.outcome {
            Ok(_) => "resolved",
            Err(YouTubePrewarmError::Cancelled) => "cancelled",
            Err(_) => "failed",
        };
        formatter
            .debug_struct("YouTubePrewarmResult")
            .field("generation", &self.generation)
            .field("outcome", &state)
            .finish()
    }
}

/// Failure while validating, executing, or parsing a speculative resolution.
#[derive(Debug, Error)]
pub enum YouTubePrewarmError {
    /// A request or resource setting is outside the module's documented bounds.
    #[error("invalid YouTube prewarm request: {0}")]
    InvalidRequest(&'static str),
    /// The configured executable does not exist.
    #[error("yt-dlp is unavailable")]
    ExecutableUnavailable,
    /// A newer selection cancelled this request.
    #[error("YouTube prewarm was cancelled")]
    Cancelled,
    /// The supervised child exceeded the whole-operation timeout.
    #[error("YouTube prewarm timed out after {0:?}")]
    TimedOut(Duration),
    /// Machine-readable output exceeded the configured byte limit.
    #[error("yt-dlp prewarm JSON exceeded the {limit}-byte limit")]
    OutputTooLarge {
        /// Configured retained-byte limit.
        limit: usize,
    },
    /// `yt-dlp` returned an unsuccessful status.
    ///
    /// Raw standard error is deliberately not retained in the public error:
    /// extractor diagnostics can contain signed URLs, cookies, or tokens.
    #[error("yt-dlp YouTube prewarm exited with {status}")]
    ProcessExited {
        /// Operating-system exit status.
        status: ExitStatus,
        /// Whether discarded standard error exceeded its retained-byte bound.
        stderr_truncated: bool,
    },
    /// `yt-dlp` did not return exactly one valid direct audio object.
    #[error("invalid yt-dlp YouTube prewarm output: {0}")]
    InvalidOutput(&'static str),
    /// A child-process or pipe operation failed.
    ///
    /// Only the operation and error kind are retained, avoiding executable
    /// paths or remote URLs in `Debug` and user-visible diagnostics.
    #[error("yt-dlp prewarm process I/O failed while {operation}: {kind:?}")]
    ProcessIo {
        /// Bounded description of the failed operation.
        operation: &'static str,
        /// Operating-system error category without path-bearing context.
        kind: io::ErrorKind,
    },
}

/// Synchronous worker-side adapter for speculative `YouTube` resolution.
#[derive(Clone)]
pub struct YouTubePrewarmResolver {
    config: YouTubePrewarmConfig,
}

impl YouTubePrewarmResolver {
    /// Creates a resolver with explicit process and resource policy.
    #[must_use]
    pub const fn new(config: YouTubePrewarmConfig) -> Self {
        Self { config }
    }

    /// Resolves one selected `YouTube` video into a direct audio URL.
    ///
    /// The executable is invoked without a shell. User and system `yt-dlp`
    /// configuration files are ignored, and plugins are disabled unless the
    /// caller explicitly opted into them. Standard output and standard error
    /// are drained concurrently into fixed-size buffers. Cancellation and
    /// timeout both kill and reap the child before this method returns.
    ///
    /// This call blocks. Run it on a worker thread, publish the returned
    /// [`YouTubePrewarmResult`], and apply it only when
    /// [`YouTubePrewarmResult::is_latest`] succeeds.
    #[must_use]
    pub fn resolve(
        &self,
        request: YouTubePrewarmRequest,
        cancellation: &YouTubePrewarmCancellation,
    ) -> YouTubePrewarmResult {
        let YouTubePrewarmRequest {
            generation,
            source_url,
        } = request;
        let outcome = self.resolve_inner(&source_url, cancellation);
        YouTubePrewarmResult {
            generation,
            outcome,
        }
    }

    fn resolve_inner(
        &self,
        source_url: &Url,
        cancellation: &YouTubePrewarmCancellation,
    ) -> Result<PrewarmedYouTubeAudio, YouTubePrewarmError> {
        validate_config(&self.config)?;
        validate_source_url(source_url)?;
        if cancellation.is_cancelled() {
            return Err(YouTubePrewarmError::Cancelled);
        }

        let mut command = build_command(&self.config, source_url);
        let output = run_bounded_command(
            &mut command,
            self.config.timeout,
            self.config.max_json_bytes,
            self.config.max_stderr_bytes,
            cancellation,
        )?;
        if cancellation.is_cancelled() {
            return Err(YouTubePrewarmError::Cancelled);
        }
        if output.stdout.truncated {
            return Err(YouTubePrewarmError::OutputTooLarge {
                limit: self.config.max_json_bytes,
            });
        }
        if !output.status.success() {
            return Err(YouTubePrewarmError::ProcessExited {
                status: output.status,
                stderr_truncated: output.stderr.truncated,
            });
        }
        parse_resolved_audio(&output.stdout.bytes)
    }
}

impl Default for YouTubePrewarmResolver {
    fn default() -> Self {
        Self::new(YouTubePrewarmConfig::default())
    }
}

impl fmt::Debug for YouTubePrewarmResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("YouTubePrewarmResolver")
            .field("config", &self.config)
            .finish()
    }
}

fn validate_config(config: &YouTubePrewarmConfig) -> Result<(), YouTubePrewarmError> {
    if config.executable.as_os_str().is_empty() {
        return Err(YouTubePrewarmError::InvalidRequest(
            "the yt-dlp executable cannot be empty",
        ));
    }
    if config.timeout.is_zero() || config.timeout > MAX_CONFIGURED_TIMEOUT {
        return Err(YouTubePrewarmError::InvalidRequest(
            "the timeout must be between 1 nanosecond and 5 minutes",
        ));
    }
    if !(1..=MAX_CONFIGURED_JSON_BYTES).contains(&config.max_json_bytes) {
        return Err(YouTubePrewarmError::InvalidRequest(
            "the JSON limit must be between 1 byte and 1 MiB",
        ));
    }
    if !(1..=MAX_CONFIGURED_STDERR_BYTES).contains(&config.max_stderr_bytes) {
        return Err(YouTubePrewarmError::InvalidRequest(
            "the stderr limit must be between 1 byte and 256 KiB",
        ));
    }
    if config.audio_format.is_empty()
        || config.audio_format.len() > MAX_AUDIO_FORMAT_BYTES
        || config
            .audio_format
            .bytes()
            .any(|byte| byte.is_ascii_control())
    {
        return Err(YouTubePrewarmError::InvalidRequest(
            "the audio format expression is empty, oversized, or contains a control byte",
        ));
    }
    Ok(())
}

fn validate_source_url(url: &Url) -> Result<(), YouTubePrewarmError> {
    let host = url.host_str().unwrap_or_default();
    let youtube_host = host == "youtu.be"
        || host == "youtube.com"
        || host.ends_with(".youtube.com")
        || host == "youtube-nocookie.com"
        || host.ends_with(".youtube-nocookie.com");
    if url.as_str().len() > MAX_SOURCE_URL_BYTES
        || url.scheme() != "https"
        || !youtube_host
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(YouTubePrewarmError::InvalidRequest(
            "the source must be a bounded credential-free HTTPS YouTube URL",
        ));
    }
    Ok(())
}

fn build_command(config: &YouTubePrewarmConfig, source_url: &Url) -> Command {
    let socket_timeout = config.timeout.as_secs().max(1);
    let mut command = Command::new(&config.executable);
    crate::child_process::supervised(&mut command);
    command.arg("--ignore-config");
    if !config.allow_plugins {
        command.arg("--no-plugin-dirs");
    }
    command
        .arg("--js-runtimes")
        .arg(ADDITIONAL_JS_RUNTIME)
        .arg("--no-warnings")
        .arg("--no-playlist")
        .arg("--skip-download")
        .arg("--socket-timeout")
        .arg(socket_timeout.to_string())
        .arg("--retries")
        .arg("1")
        .arg("--extractor-retries")
        .arg("1");
    if let Some(argument) = config.player_client_policy.extractor_argument() {
        command.arg("--extractor-args").arg(argument);
    }
    command
        .arg("--format")
        .arg(&config.audio_format)
        .arg("--print")
        .arg(PRINT_TEMPLATE)
        .arg("--")
        .arg(source_url.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

struct BoundedCommandOutput {
    status: ExitStatus,
    stdout: BoundedCapture,
    stderr: BoundedCapture,
}

struct BoundedCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

enum WaitOutcome {
    Exited(ExitStatus),
    Cancelled,
    TimedOut,
}

fn run_bounded_command(
    command: &mut Command,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
    cancellation: &YouTubePrewarmCancellation,
) -> Result<BoundedCommandOutput, YouTubePrewarmError> {
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            YouTubePrewarmError::ExecutableUnavailable
        } else {
            process_io_error("starting yt-dlp", &error)
        }
    })?;
    let Some(stdout) = child.stdout.take() else {
        let _ = terminate_and_reap(&mut child);
        return Err(YouTubePrewarmError::ProcessIo {
            operation: "opening the stdout pipe",
            kind: io::ErrorKind::BrokenPipe,
        });
    };
    let Some(stderr) = child.stderr.take() else {
        let _ = terminate_and_reap(&mut child);
        return Err(YouTubePrewarmError::ProcessIo {
            operation: "opening the stderr pipe",
            kind: io::ErrorKind::BrokenPipe,
        });
    };
    let stdout_receiver = capture_bounded(stdout, stdout_limit);
    let stderr_receiver = capture_bounded(stderr, stderr_limit);
    let wait_outcome = wait_for_child(&mut child, timeout, cancellation)?;
    let stdout = receive_capture(&stdout_receiver, "reading yt-dlp stdout");
    let stderr = receive_capture(&stderr_receiver, "reading yt-dlp stderr");

    match wait_outcome {
        WaitOutcome::Cancelled => Err(YouTubePrewarmError::Cancelled),
        WaitOutcome::TimedOut => Err(YouTubePrewarmError::TimedOut(timeout)),
        WaitOutcome::Exited(status) => Ok(BoundedCommandOutput {
            status,
            stdout: stdout?,
            stderr: stderr?,
        }),
    }
}

fn wait_for_child(
    child: &mut Child,
    timeout: Duration,
    cancellation: &YouTubePrewarmCancellation,
) -> Result<WaitOutcome, YouTubePrewarmError> {
    let deadline =
        Instant::now()
            .checked_add(timeout)
            .ok_or(YouTubePrewarmError::InvalidRequest(
                "the process deadline overflowed",
            ))?;
    loop {
        if cancellation.is_cancelled() {
            terminate_and_reap(child)?;
            return Ok(WaitOutcome::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(WaitOutcome::Exited(status)),
            Ok(None) if Instant::now() >= deadline => {
                terminate_and_reap(child)?;
                return Ok(WaitOutcome::TimedOut);
            }
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                thread::sleep(remaining.min(PROCESS_POLL_INTERVAL));
            }
            Err(error) => {
                let poll_error = process_io_error("polling yt-dlp", &error);
                let _ = terminate_and_reap(child);
                return Err(poll_error);
            }
        }
    }
}

fn terminate_and_reap(child: &mut Child) -> Result<(), YouTubePrewarmError> {
    let kill_error = terminate_process_tree(child);
    let wait_result = child.wait();
    if let Err(error) = wait_result {
        return Err(process_io_error("reaping yt-dlp", &error));
    }
    if let Some(error) = kill_error {
        return Err(process_io_error("terminating yt-dlp", &error));
    }
    Ok(())
}

/// Terminates the supervised helper and, on Unix, every descendant that kept
/// its dedicated process group.
fn terminate_process_tree(child: &mut Child) -> Option<io::Error> {
    #[cfg(unix)]
    {
        match kill_process_group(Pid::from_child(child), Signal::KILL) {
            Ok(()) => None,
            Err(Errno::SRCH) => child
                .kill()
                .err()
                .filter(|error| error.kind() != io::ErrorKind::InvalidInput),
            Err(error) => {
                // Killing the exact child remains a bounded fallback when a
                // platform refuses the group signal. Preserve the group error
                // because descendants could still own inherited pipes.
                let _ = child.kill();
                Some(io::Error::from(error))
            }
        }
    }
    #[cfg(not(unix))]
    {
        // No process group to signal, so the descendants are ended by name
        // before the direct child, while the chain to them still exists.
        crate::child_process::kill_descendants(child.id());
        child
            .kill()
            .err()
            .filter(|error| error.kind() != io::ErrorKind::InvalidInput)
    }
}

fn capture_bounded(
    mut reader: impl Read + Send + 'static,
    limit: usize,
) -> mpsc::Receiver<io::Result<BoundedCapture>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
        let mut buffer = [0_u8; 8 * 1024];
        let mut truncated = false;
        let capture = loop {
            match reader.read(&mut buffer) {
                Ok(0) => break Ok(BoundedCapture { bytes, truncated }),
                Ok(read) => {
                    let retained = read.min(limit.saturating_sub(bytes.len()));
                    bytes.extend_from_slice(&buffer[..retained]);
                    truncated |= retained < read;
                }
                Err(error) => break Err(error),
            }
        };
        let _ = sender.send(capture);
    });
    receiver
}

fn receive_capture(
    receiver: &mpsc::Receiver<io::Result<BoundedCapture>>,
    operation: &'static str,
) -> Result<BoundedCapture, YouTubePrewarmError> {
    match receiver.recv_timeout(OUTPUT_CLOSE_TIMEOUT) {
        Ok(Ok(capture)) => Ok(capture),
        Ok(Err(error)) => Err(process_io_error(operation, &error)),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(YouTubePrewarmError::ProcessIo {
            operation,
            kind: io::ErrorKind::TimedOut,
        }),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(YouTubePrewarmError::ProcessIo {
            operation,
            kind: io::ErrorKind::BrokenPipe,
        }),
    }
}

fn process_io_error(operation: &'static str, error: &io::Error) -> YouTubePrewarmError {
    YouTubePrewarmError::ProcessIo {
        operation,
        kind: error.kind(),
    }
}

#[derive(Deserialize)]
struct ExtractedAudio {
    url: String,
    #[serde(default)]
    http_headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    acodec: Option<String>,
    #[serde(default)]
    vcodec: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
}

fn parse_resolved_audio(bytes: &[u8]) -> Result<PrewarmedYouTubeAudio, YouTubePrewarmError> {
    let extracted: ExtractedAudio = serde_json::from_slice(bytes).map_err(|_| {
        YouTubePrewarmError::InvalidOutput(
            "expected exactly one JSON object containing a selected audio URL",
        )
    })?;
    let media_url = Url::parse(&extracted.url)
        .map_err(|_| YouTubePrewarmError::InvalidOutput("the selected media URL is malformed"))?;
    if media_url.scheme() != "https"
        || media_url.host_str().is_none()
        || !media_url.username().is_empty()
        || media_url.password().is_some()
    {
        return Err(YouTubePrewarmError::InvalidOutput(
            "the selected media URL is not credential-free HTTPS",
        ));
    }

    let audio_codec = extracted.acodec.unwrap_or_default();
    if audio_codec.is_empty()
        || audio_codec == "none"
        || audio_codec.len() > MAX_CODEC_BYTES
        || audio_codec.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(YouTubePrewarmError::InvalidOutput(
            "the selected format does not contain a bounded audio codec",
        ));
    }
    if extracted
        .vcodec
        .as_deref()
        .is_some_and(|codec| !codec.is_empty() && codec != "none")
    {
        return Err(YouTubePrewarmError::InvalidOutput(
            "the selected format is not audio-only",
        ));
    }
    let protocol = extracted.protocol.filter(|protocol| !protocol.is_empty());
    if protocol.as_ref().is_some_and(|protocol| {
        protocol.len() > MAX_PROTOCOL_BYTES || protocol.bytes().any(|byte| byte.is_ascii_control())
    }) {
        return Err(YouTubePrewarmError::InvalidOutput(
            "the selected protocol is oversized or malformed",
        ));
    }

    let headers = extracted.http_headers.unwrap_or_default();
    validate_http_headers(&headers)?;
    let expires_at_unix = media_url
        .query_pairs()
        .find_map(|(name, value)| (name == "expire").then(|| value.parse::<u64>().ok()))
        .flatten();

    Ok(PrewarmedYouTubeAudio {
        media_url,
        http_headers: PlaybackHttpHeaders::new(headers),
        expires_at_unix,
        audio_codec,
        protocol,
    })
}

fn validate_http_headers(headers: &BTreeMap<String, String>) -> Result<(), YouTubePrewarmError> {
    if headers.len() > MAX_HTTP_HEADERS {
        return Err(YouTubePrewarmError::InvalidOutput(
            "the selected stream returned too many HTTP headers",
        ));
    }
    let mut total_bytes = 0_usize;
    for (name, value) in headers {
        if name.is_empty()
            || !name.bytes().all(is_http_token_byte)
            || value
                .bytes()
                .any(|byte| matches!(byte, b'\r' | b'\n' | b'\0'))
        {
            return Err(YouTubePrewarmError::InvalidOutput(
                "the selected stream returned a malformed HTTP header",
            ));
        }
        total_bytes = total_bytes
            .saturating_add(name.len())
            .saturating_add(value.len())
            .saturating_add(2);
        if total_bytes > MAX_HTTP_HEADER_BYTES {
            return Err(YouTubePrewarmError::InvalidOutput(
                "the selected stream HTTP headers exceed the memory limit",
            ));
        }
    }
    Ok(())
}

const fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    #[cfg(unix)]
    use std::sync::mpsc::RecvTimeoutError;
    #[cfg(unix)]
    use std::sync::{Mutex, MutexGuard};

    /// Serializes subprocess fixtures so parallel test load cannot consume
    /// their deliberately short timeout budgets.
    #[cfg(unix)]
    static PROCESS_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(unix)]
    fn process_test_guard() -> MutexGuard<'static, ()> {
        PROCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Waits until a subprocess fixture publishes one complete, nonzero PID.
    ///
    /// Shell redirection creates the marker before `printf` writes its value.
    /// Requiring the trailing newline prevents a loaded test runner from
    /// observing an empty or partially written marker as a ready fixture.
    #[cfg(unix)]
    fn wait_for_process_marker(marker: &std::path::Path, deadline: Instant) -> u32 {
        loop {
            match std::fs::read_to_string(marker) {
                Ok(contents) => {
                    if let Some(serialized_pid) = contents.strip_suffix('\n')
                        && let Ok(pid) = serialized_pid.parse::<u32>()
                        && pid != 0
                    {
                        return pid;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => panic!("read subprocess marker: {error}"),
            }
            assert!(
                Instant::now() < deadline,
                "mock child must publish its PID before cancellation"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Waits for Linux to publish a killed fixture process as gone or zombie.
    ///
    /// A successful process-group signal can precede the target's final
    /// `/proc` state transition, especially on a loaded CI host. The process
    /// can also disappear after `/proc/<pid>/stat` is opened, making its read
    /// fail with `ESRCH` instead of `ENOENT`. Both mean the descendant is gone.
    /// Keeping this wait bounded still detects a descendant that escaped
    /// termination.
    #[cfg(target_os = "linux")]
    fn assert_process_terminated(pid: &str) {
        let stat_path = PathBuf::from(format!("/proc/{pid}/stat"));
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match std::fs::read_to_string(&stat_path) {
                Ok(stat) => {
                    let state = stat
                        .rsplit_once(") ")
                        .and_then(|(_, fields)| fields.chars().next())
                        .expect("Linux process stat must contain a state");
                    if state == 'Z' {
                        return;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "a surviving descendant must be terminated; observed /proc state {state}"
                    );
                }
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        || error.raw_os_error() == Some(Errno::SRCH.raw_os_error()) =>
                {
                    return;
                }
                Err(error) => panic!("read descendant process state: {error}"),
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn watch_url() -> Url {
        Url::parse("https://www.youtube.com/watch?v=dQw4w9WgXcQ").expect("fixture URL")
    }

    fn resolved_json() -> &'static [u8] {
        br#"{
            "url": "https://media.example/audio.webm?expire=2000000000&sig=private-signature",
            "http_headers": {
                "Authorization": "Bearer private-token",
                "User-Agent": "Youta fixture"
            },
            "acodec": "opus",
            "vcodec": "none",
            "protocol": "https"
        }"#
    }

    #[test]
    fn parses_exactly_one_audio_object_and_preserves_expiry_and_headers() {
        let resolved = parse_resolved_audio(resolved_json()).expect("resolved audio");

        assert_eq!(resolved.audio_codec(), "opus");
        assert_eq!(resolved.protocol(), Some("https"));
        assert_eq!(resolved.expires_at_unix(), Some(2_000_000_000));
        assert_eq!(resolved.http_headers().iter().len(), 2);
        assert_eq!(
            resolved
                .http_headers()
                .iter()
                .find(|(name, _)| *name == "Authorization")
                .map(|(_, value)| value),
            Some("Bearer private-token")
        );
    }

    #[test]
    fn request_resolution_and_completion_debug_are_opaque() {
        let request = YouTubePrewarmRequest::new(17, watch_url());
        let resolved = parse_resolved_audio(resolved_json()).expect("resolved audio");
        let completion = YouTubePrewarmResult {
            generation: request.generation(),
            outcome: Ok(resolved),
        };
        let debug = format!("{request:?} {completion:?} {:?}", completion.outcome());

        assert!(completion.is_latest(17));
        assert!(!completion.is_latest(18));
        assert!(!debug.contains("dQw4w9WgXcQ"));
        assert!(!debug.contains("private-signature"));
        assert!(!debug.contains("private-token"));
        assert!(!debug.contains("media.example"));
    }

    #[test]
    fn parser_rejects_multiple_objects_video_formats_and_header_injection() {
        let mut multiple = resolved_json().to_vec();
        multiple.extend_from_slice(resolved_json());
        assert!(matches!(
            parse_resolved_audio(&multiple),
            Err(YouTubePrewarmError::InvalidOutput(_))
        ));

        let video = resolved_json().to_vec();
        let video = String::from_utf8(video)
            .expect("UTF-8 fixture")
            .replace(r#""vcodec": "none""#, r#""vcodec": "vp9""#);
        assert!(matches!(
            parse_resolved_audio(video.as_bytes()),
            Err(YouTubePrewarmError::InvalidOutput(
                "the selected format is not audio-only"
            ))
        ));

        let injected = String::from_utf8(resolved_json().to_vec())
            .expect("UTF-8 fixture")
            .replace("Youta fixture", "Youta\\r\\nInjected: yes");
        assert!(matches!(
            parse_resolved_audio(injected.as_bytes()),
            Err(YouTubePrewarmError::InvalidOutput(
                "the selected stream returned a malformed HTTP header"
            ))
        ));
    }

    #[test]
    fn request_validation_accepts_youtube_only_and_configuration_is_bounded() {
        assert!(validate_source_url(&watch_url()).is_ok());
        assert!(
            validate_source_url(
                &Url::parse("https://youtu.be/dQw4w9WgXcQ").expect("short fixture URL")
            )
            .is_ok()
        );
        assert!(
            validate_source_url(
                &Url::parse("https://example.com/watch?v=dQw4w9WgXcQ")
                    .expect("foreign fixture URL")
            )
            .is_err()
        );
        assert!(
            validate_source_url(
                &Url::parse("http://www.youtube.com/watch?v=dQw4w9WgXcQ")
                    .expect("clear-text fixture URL")
            )
            .is_err()
        );

        let mut config = YouTubePrewarmConfig::default();
        assert!(validate_config(&config).is_ok());
        config.max_json_bytes = 0;
        assert!(validate_config(&config).is_err());
        config.max_json_bytes = 1;
        config.timeout = Duration::ZERO;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn command_is_shell_free_and_disables_config_and_plugins_by_default() {
        let config = YouTubePrewarmConfig {
            executable: PathBuf::from("mock yt-dlp; still one executable"),
            ..YouTubePrewarmConfig::default()
        };
        let command = build_command(&config, &watch_url());
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            command.get_program().to_string_lossy(),
            "mock yt-dlp; still one executable"
        );
        assert!(arguments.contains(&"--ignore-config".to_owned()));
        assert!(arguments.contains(&"--no-plugin-dirs".to_owned()));
        assert!(!arguments.contains(&"--extractor-args".to_owned()));
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--js-runtimes", "quickjs"])
        );
        assert_eq!(
            &arguments[arguments.len() - 2..],
            ["--", "https://www.youtube.com/watch?v=dQw4w9WgXcQ"]
        );
        assert_eq!(
            arguments
                .windows(2)
                .find(|pair| pair[0] == "--print")
                .map(|pair| pair[1].as_str()),
            Some(PRINT_TEMPLATE)
        );
    }

    #[test]
    fn embedded_client_policy_is_explicit_and_keeps_config_disabled() {
        let config = YouTubePrewarmConfig {
            player_client_policy: YouTubePlayerClientPolicy::EmbeddedThenDefault,
            ..YouTubePrewarmConfig::default()
        };
        let command = build_command(&config, &watch_url());
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(arguments.contains(&"--ignore-config".to_owned()));
        assert!(arguments.contains(&"--no-plugin-dirs".to_owned()));
        assert!(arguments.windows(2).any(|pair| {
            pair == [
                "--extractor-args",
                "youtube:player_client=web_embedded,default",
            ]
        }));
    }

    #[test]
    fn explicit_plugin_opt_in_changes_only_plugin_discovery_policy() {
        let config = YouTubePrewarmConfig {
            allow_plugins: true,
            ..YouTubePrewarmConfig::default()
        };
        let command = build_command(&config, &watch_url());
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(arguments.contains(&"--ignore-config".to_owned()));
        assert!(!arguments.contains(&"--no-plugin-dirs".to_owned()));
    }

    #[cfg(unix)]
    struct MockExecutable {
        _directory: tempfile::TempDir,
        path: PathBuf,
    }

    #[cfg(unix)]
    impl MockExecutable {
        fn new(body: &str) -> Self {
            let directory = tempfile::tempdir().expect("temporary mock executable directory");
            let path = directory.path().join("mock yt-dlp; no shell");
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write mock executable");
            let mut permissions = std::fs::metadata(&path)
                .expect("mock executable metadata")
                .permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(&path, permissions).expect("make mock executable runnable");
            Self {
                _directory: directory,
                path,
            }
        }
    }

    #[cfg(unix)]
    fn resolver_for(
        executable: &MockExecutable,
        timeout: Duration,
        max_json_bytes: usize,
        max_stderr_bytes: usize,
    ) -> YouTubePrewarmResolver {
        YouTubePrewarmResolver::new(YouTubePrewarmConfig {
            executable: executable.path.clone(),
            timeout,
            max_json_bytes,
            max_stderr_bytes,
            ..YouTubePrewarmConfig::default()
        })
    }

    #[cfg(unix)]
    #[test]
    fn resolver_executes_mock_and_returns_generation_tagged_audio() {
        let _process_guard = process_test_guard();
        let executable = MockExecutable::new(
            r#"printf '%s\n' '{"url":"https://media.example/audio.webm?expire=2000000000","http_headers":{"User-Agent":"fixture"},"acodec":"opus","vcodec":"none","protocol":"https"}'"#,
        );
        let resolver = resolver_for(&executable, Duration::from_secs(2), 4 * 1024, 4 * 1024);
        let completion = resolver.resolve(
            YouTubePrewarmRequest::new(91, watch_url()),
            &YouTubePrewarmCancellation::new(),
        );

        assert_eq!(completion.generation(), 91);
        let audio = completion.outcome().expect("successful prewarm");
        assert_eq!(audio.audio_codec(), "opus");
        assert_eq!(audio.expires_at_unix(), Some(2_000_000_000));
    }

    #[cfg(unix)]
    #[test]
    fn stdout_and_stderr_are_drained_concurrently_and_retained_within_bounds() {
        let _process_guard = process_test_guard();
        let executable = MockExecutable::new(
            r#"i=0
while [ "$i" -lt 2000 ]; do
    printf 'private-stderr-value' >&2
    i=$((i + 1))
done
printf '%s\n' '{"url":"https://media.example/audio.webm","http_headers":{},"acodec":"opus","vcodec":"none","protocol":"https"}'"#,
        );
        let resolver = resolver_for(&executable, Duration::from_secs(5), 1024, 32);
        let completion = resolver.resolve(
            YouTubePrewarmRequest::new(1, watch_url()),
            &YouTubePrewarmCancellation::new(),
        );

        assert!(
            completion.outcome().is_ok(),
            "oversized stderr must be drained without blocking successful JSON"
        );

        let oversized_stdout = MockExecutable::new(
            r"printf '%s' 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'",
        );
        let resolver = resolver_for(&oversized_stdout, Duration::from_secs(5), 64, 64);
        let completion = resolver.resolve(
            YouTubePrewarmRequest::new(2, watch_url()),
            &YouTubePrewarmCancellation::new(),
        );
        let outcome = completion.into_outcome();
        assert!(
            matches!(
                &outcome,
                Err(YouTubePrewarmError::OutputTooLarge { limit: 64 })
            ),
            "unexpected oversized-stdout outcome: {outcome:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_process_reports_only_status_and_truncation_not_stderr_contents() {
        let _process_guard = process_test_guard();
        let executable = MockExecutable::new(
            r#"i=0
while [ "$i" -lt 200 ]; do
    printf 'https://secret.example/?token=private-token\n' >&2
    i=$((i + 1))
done
exit 7"#,
        );
        let resolver = resolver_for(&executable, Duration::from_secs(2), 1024, 32);
        let completion = resolver.resolve(
            YouTubePrewarmRequest::new(3, watch_url()),
            &YouTubePrewarmCancellation::new(),
        );
        let debug = format!("{completion:?} {:?}", completion.outcome());

        assert!(matches!(
            completion.into_outcome(),
            Err(YouTubePrewarmError::ProcessExited {
                stderr_truncated: true,
                ..
            })
        ));
        assert!(!debug.contains("secret.example"));
        assert!(!debug.contains("private-token"));
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_and_reaps_the_supervised_child() {
        let _process_guard = process_test_guard();
        let executable = MockExecutable::new("exec sleep 10");
        let resolver = resolver_for(&executable, Duration::from_millis(40), 1024, 1024);
        let started = Instant::now();
        let completion = resolver.resolve(
            YouTubePrewarmRequest::new(4, watch_url()),
            &YouTubePrewarmCancellation::new(),
        );

        assert!(matches!(
            completion.into_outcome(),
            Err(YouTubePrewarmError::TimedOut(duration))
                if duration == Duration::from_millis(40)
        ));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn timeout_terminates_descendants_in_the_dedicated_process_group() {
        let _process_guard = process_test_guard();
        let marker_directory = tempfile::tempdir().expect("temporary descendant marker");
        let marker = marker_directory.path().join("descendant.pid");
        let executable = MockExecutable::new(&format!(
            "sleep 10 &\nprintf '%s' \"$!\" > '{}'\nwait",
            marker.display()
        ));
        let resolver = resolver_for(&executable, Duration::from_millis(200), 1024, 1024);
        let completion = resolver.resolve(
            YouTubePrewarmRequest::new(40, watch_url()),
            &YouTubePrewarmCancellation::new(),
        );

        assert!(matches!(
            completion.into_outcome(),
            Err(YouTubePrewarmError::TimedOut(duration))
                if duration == Duration::from_millis(200)
        ));
        let descendant = std::fs::read_to_string(&marker)
            .expect("the mock helper should start its descendant before timeout");
        assert_process_terminated(descendant.trim());
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_kills_reaps_and_retains_the_stale_generation() {
        let _process_guard = process_test_guard();
        let marker_directory = tempfile::tempdir().expect("temporary cancellation marker");
        let marker = marker_directory.path().join("child.pid");
        let executable = MockExecutable::new(&format!(
            "printf '%s\\n' \"$$\" > '{}'\nexec sleep 10",
            marker.display()
        ));
        let resolver = resolver_for(&executable, Duration::from_secs(5), 1024, 1024);
        let cancellation = YouTubePrewarmCancellation::new();
        let worker_cancellation = cancellation.clone();
        let worker = thread::spawn(move || {
            resolver.resolve(
                YouTubePrewarmRequest::new(41, watch_url()),
                &worker_cancellation,
            )
        });

        let pid = wait_for_process_marker(&marker, Instant::now() + Duration::from_secs(3));
        cancellation.cancel();
        let completion = worker.join().expect("join cancellation worker");

        assert_eq!(completion.generation(), 41);
        assert!(!completion.is_latest(42));
        assert!(matches!(
            completion.into_outcome(),
            Err(YouTubePrewarmError::Cancelled)
        ));
        #[cfg(target_os = "linux")]
        assert!(
            !PathBuf::from(format!("/proc/{pid}")).exists(),
            "the cancelled child must be reaped"
        );
        #[cfg(not(target_os = "linux"))]
        let _ = pid;
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_before_start_does_not_execute_the_helper() {
        let _process_guard = process_test_guard();
        let marker_directory = tempfile::tempdir().expect("temporary pre-cancel marker");
        let marker = marker_directory.path().join("started");
        let executable = MockExecutable::new(&format!("printf 'started' > '{}'", marker.display()));
        let resolver = resolver_for(&executable, Duration::from_secs(2), 1024, 1024);
        let cancellation = YouTubePrewarmCancellation::new();
        cancellation.cancel();
        let completion =
            resolver.resolve(YouTubePrewarmRequest::new(5, watch_url()), &cancellation);

        assert!(matches!(
            completion.into_outcome(),
            Err(YouTubePrewarmError::Cancelled)
        ));
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn unavailable_executable_is_an_opaque_generation_tagged_failure() {
        let _process_guard = process_test_guard();
        let resolver = YouTubePrewarmResolver::new(YouTubePrewarmConfig {
            executable: PathBuf::from("/definitely/missing/youta-yt-dlp"),
            timeout: Duration::from_secs(1),
            ..YouTubePrewarmConfig::default()
        });
        let completion = resolver.resolve(
            YouTubePrewarmRequest::new(6, watch_url()),
            &YouTubePrewarmCancellation::new(),
        );

        assert_eq!(completion.generation(), 6);
        assert!(matches!(
            completion.into_outcome(),
            Err(YouTubePrewarmError::ExecutableUnavailable)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn capture_receiver_disconnect_is_reported_without_waiting_forever() {
        let (sender, receiver) = mpsc::channel::<io::Result<BoundedCapture>>();
        drop(sender);
        assert!(matches!(
            receive_capture(&receiver, "reading fixture"),
            Err(YouTubePrewarmError::ProcessIo {
                kind: io::ErrorKind::BrokenPipe,
                ..
            })
        ));
        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(1)),
            Err(RecvTimeoutError::Disconnected)
        ));
    }
}
