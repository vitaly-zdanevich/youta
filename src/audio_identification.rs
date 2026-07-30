//! Explicit local-audio identification through Chromaprint and `AcoustID`.
//!
//! [`LocalAudioIdentifier`] runs the installed `fpcalc` executable without a
//! shell, submits its bounded JSON fingerprint to `AcoustID`, and returns
//! ranked [`MusicBrainzCandidate`] values. Identification is deliberately
//! explicit: this module does not scan directories or fingerprint files in the
//! background. [`AudioIdentificationCancellation`] lets a caller stop an
//! in-flight `fpcalc` process when its selected file changes or the application
//! shuts down.
//!
//! `AcoustID` documents the lookup parameters and its three-requests-per-second
//! service limit in the [web-service documentation][acoustid]. Chromaprint
//! documents the `fpcalc` utility on its [project page][chromaprint].
//!
//! [acoustid]: https://acoustid.org/webservice
//! [chromaprint]: https://acoustid.org/chromaprint

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fmt,
    io::{self, Read},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use thiserror::Error;
use url::Url;

/// Official `AcoustID` fingerprint-lookup endpoint.
pub const ACOUSTID_LOOKUP_ENDPOINT: &str = "https://api.acoustid.org/v2/lookup";

/// Package-manager commands that install the official `fpcalc` helper.
pub const FPCALC_INSTALL_GUIDANCE: &str = "\
Install the Chromaprint tools package:
  Gentoo: USE=tools emerge media-libs/chromaprint
  Debian/Ubuntu: apt install libchromaprint-tools
  Fedora: dnf install chromaprint-tools
  macOS (Homebrew): brew install chromaprint";

/// Maximum accepted encoded Chromaprint fingerprint size.
pub const MAX_FINGERPRINT_BYTES: usize = 1_048_576;

/// Maximum accepted `AcoustID` response size.
pub const MAX_ACOUSTID_RESPONSE_BYTES: usize = 1_048_576;

/// Maximum number of unique `MusicBrainz` recording candidates returned.
pub const MAX_RECORDING_CANDIDATES: usize = 256;

/// Maximum number of unique `MusicBrainz` recording URLs returned per lookup.
///
/// This compatibility alias has the same value as
/// [`MAX_RECORDING_CANDIDATES`].
pub const MAX_RECORDING_URLS: usize = MAX_RECORDING_CANDIDATES;

/// Conservative minimum spacing between `AcoustID` lookup starts.
///
/// A 334-millisecond interval keeps the serialized identifier below the
/// service's documented limit of three requests per second.
pub const MIN_ACOUSTID_LOOKUP_INTERVAL: Duration = Duration::from_millis(334);

const DEFAULT_FP_CALC_TIMEOUT: Duration = Duration::from_mins(2);
const MAX_FP_CALC_TIMEOUT: Duration = Duration::from_mins(10);
const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_HTTP_TIMEOUT: Duration = Duration::from_mins(1);
const DEFAULT_STDOUT_LIMIT: usize = 2_097_152;
const DEFAULT_STDERR_LIMIT: usize = 65_536;
const MAX_STDOUT_LIMIT: usize = 8_388_608;
const MAX_STDERR_LIMIT: usize = 1_048_576;
const MAX_CLIENT_KEY_BYTES: usize = 128;
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(25);
const PROCESS_CAPTURE_FINISH_TIMEOUT: Duration = Duration::from_secs(2);

/// Cloneable cooperative-cancellation signal for one identification request.
///
/// Clones observe the same state. Cancellation is permanent, lock-free, and
/// safe to request from another thread.
#[derive(Clone, Default)]
pub struct AudioIdentificationCancellation {
    cancelled: Arc<AtomicBool>,
}

impl AudioIdentificationCancellation {
    /// Creates a cancellation signal in its active state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation for this signal and all of its clones.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Returns whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl fmt::Debug for AudioIdentificationCancellation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AudioIdentificationCancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

trait MonotonicLookupClock: Send {
    fn now(&self) -> Instant;
    fn sleep(&mut self, duration: Duration, cancellation: &AudioIdentificationCancellation);
}

#[derive(Clone, Copy, Debug, Default)]
struct SystemMonotonicLookupClock;

impl MonotonicLookupClock for SystemMonotonicLookupClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&mut self, duration: Duration, cancellation: &AudioIdentificationCancellation) {
        let mut remaining = duration;
        while !remaining.is_zero() && !cancellation.is_cancelled() {
            let slice = remaining.min(PROCESS_POLL_INTERVAL);
            thread::sleep(slice);
            remaining = remaining.saturating_sub(slice);
        }
    }
}

struct AcoustIdLookupLimiter {
    last_lookup_started_at: Option<Instant>,
    clock: Box<dyn MonotonicLookupClock>,
}

impl AcoustIdLookupLimiter {
    fn new(clock: Box<dyn MonotonicLookupClock>) -> Self {
        Self {
            last_lookup_started_at: None,
            clock,
        }
    }

    fn wait_for_slot(
        &mut self,
        cancellation: &AudioIdentificationCancellation,
    ) -> Result<(), AudioIdentificationError> {
        loop {
            if cancellation.is_cancelled() {
                return Err(AudioIdentificationError::Cancelled);
            }
            let Some(last_started) = self.last_lookup_started_at else {
                break;
            };
            let earliest_start = last_started
                .checked_add(MIN_ACOUSTID_LOOKUP_INTERVAL)
                .ok_or(AudioIdentificationError::InvalidHttpConfig)?;
            let now = self.clock.now();
            if now >= earliest_start {
                break;
            }
            self.clock
                .sleep(earliest_start.duration_since(now), cancellation);
        }
        if cancellation.is_cancelled() {
            return Err(AudioIdentificationError::Cancelled);
        }
        self.last_lookup_started_at = Some(self.clock.now());
        Ok(())
    }
}

impl Default for AcoustIdLookupLimiter {
    fn default() -> Self {
        Self::new(Box::new(SystemMonotonicLookupClock))
    }
}

impl fmt::Debug for AcoustIdLookupLimiter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AcoustIdLookupLimiter")
            .field(
                "has_previous_lookup",
                &self.last_lookup_started_at.is_some(),
            )
            .finish_non_exhaustive()
    }
}

/// One ranked `MusicBrainz` recording linked by an `AcoustID` result.
#[derive(Clone, Debug, PartialEq)]
pub struct MusicBrainzCandidate {
    recording_id: String,
    url: Url,
    acoustid_result_id: String,
    score: f64,
}

impl MusicBrainzCandidate {
    /// Creates a validated candidate from recording and `AcoustID` UUIDs.
    ///
    /// UUIDs are canonicalized to lowercase. Returns `None` when either ID is
    /// malformed or `score` is not finite and within the inclusive zero-to-one
    /// range.
    #[must_use]
    pub fn new(recording_id: &str, acoustid_result_id: &str, score: f64) -> Option<Self> {
        let recording_id = canonical_uuid(recording_id)?;
        let acoustid_result_id = canonical_uuid(acoustid_result_id)?;
        if !score.is_finite() || !(0.0..=1.0).contains(&score) {
            return None;
        }
        let url = Url::parse(&format!("https://musicbrainz.org/recording/{recording_id}")).ok()?;
        Some(Self {
            recording_id,
            url,
            acoustid_result_id,
            score,
        })
    }

    /// Returns the canonical lowercase `MusicBrainz` recording UUID.
    #[must_use]
    pub fn recording_id(&self) -> &str {
        &self.recording_id
    }

    /// Returns the canonical `MusicBrainz` recording URL.
    #[must_use]
    pub const fn url(&self) -> &Url {
        &self.url
    }

    /// Returns the canonical lowercase UUID of the supporting `AcoustID`
    /// result.
    #[must_use]
    pub fn acoustid_result_id(&self) -> &str {
        &self.acoustid_result_id
    }

    /// Returns the finite `AcoustID` confidence score in the inclusive range
    /// from zero to one.
    #[must_use]
    pub const fn score(&self) -> f64 {
        self.score
    }
}

/// A validated whole-file duration and encoded Chromaprint fingerprint.
pub struct AudioFingerprint {
    duration_seconds: u32,
    encoded: String,
}

impl AudioFingerprint {
    /// Creates a validated encoded fingerprint.
    ///
    /// # Errors
    ///
    /// Returns an error when the duration is zero, or when the fingerprint is
    /// empty, too large, or contains whitespace/control bytes.
    pub fn new(
        duration_seconds: u32,
        encoded: impl Into<String>,
    ) -> Result<Self, AudioIdentificationError> {
        let encoded = encoded.into();
        if duration_seconds == 0 {
            return Err(AudioIdentificationError::InvalidDuration);
        }
        if encoded.is_empty()
            || encoded.len() > MAX_FINGERPRINT_BYTES
            || !encoded.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(AudioIdentificationError::InvalidFingerprint);
        }
        Ok(Self {
            duration_seconds,
            encoded,
        })
    }

    /// Returns the whole-file duration rounded to seconds.
    #[must_use]
    pub const fn duration_seconds(&self) -> u32 {
        self.duration_seconds
    }

    /// Returns the encoded Chromaprint fingerprint.
    #[must_use]
    pub fn encoded(&self) -> &str {
        &self.encoded
    }
}

impl fmt::Debug for AudioFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AudioFingerprint")
            .field("duration_seconds", &self.duration_seconds)
            .field("encoded_bytes", &self.encoded.len())
            .finish()
    }
}

/// Validated resource limits and executable name for `fpcalc`.
#[derive(Clone)]
pub struct FpcalcConfig {
    executable: OsString,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
}

impl FpcalcConfig {
    /// Creates a configuration with bounded default timeout and output sizes.
    ///
    /// `executable` can be a command name resolved through `PATH` or an
    /// explicit executable path.
    ///
    /// # Errors
    ///
    /// Returns an error when the executable is empty.
    pub fn new(executable: impl Into<OsString>) -> Result<Self, AudioIdentificationError> {
        let executable = executable.into();
        if executable.is_empty() {
            return Err(AudioIdentificationError::InvalidFpcalcConfig);
        }
        Ok(Self {
            executable,
            timeout: DEFAULT_FP_CALC_TIMEOUT,
            stdout_limit: DEFAULT_STDOUT_LIMIT,
            stderr_limit: DEFAULT_STDERR_LIMIT,
        })
    }

    /// Sets the process deadline.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero timeout or a timeout above ten minutes.
    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self, AudioIdentificationError> {
        if timeout.is_zero() || timeout > MAX_FP_CALC_TIMEOUT {
            return Err(AudioIdentificationError::InvalidFpcalcConfig);
        }
        self.timeout = timeout;
        Ok(self)
    }

    /// Sets bounded capture sizes for standard output and standard error.
    ///
    /// The process pipes continue to be drained after these limits are
    /// exceeded, so a noisy helper cannot deadlock while Youta stores only a
    /// bounded prefix.
    ///
    /// # Errors
    ///
    /// Returns an error for zero limits or limits above the hard safety caps.
    pub fn with_output_limits(
        mut self,
        stdout_limit: usize,
        stderr_limit: usize,
    ) -> Result<Self, AudioIdentificationError> {
        if stdout_limit == 0
            || stderr_limit == 0
            || stdout_limit > MAX_STDOUT_LIMIT
            || stderr_limit > MAX_STDERR_LIMIT
        {
            return Err(AudioIdentificationError::InvalidFpcalcConfig);
        }
        self.stdout_limit = stdout_limit;
        self.stderr_limit = stderr_limit;
        Ok(self)
    }
}

impl Default for FpcalcConfig {
    fn default() -> Self {
        Self::new("fpcalc").expect("the built-in fpcalc executable name is valid")
    }
}

impl fmt::Debug for FpcalcConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FpcalcConfig")
            .field("timeout", &self.timeout)
            .field("stdout_limit", &self.stdout_limit)
            .field("stderr_limit", &self.stderr_limit)
            .finish_non_exhaustive()
    }
}

/// One shell-free `fpcalc` process invocation.
#[derive(Clone)]
pub struct FpcalcInvocation {
    executable: OsString,
    arguments: Vec<OsString>,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
}

impl FpcalcInvocation {
    /// Returns the executable name or path.
    #[must_use]
    pub fn executable(&self) -> &OsStr {
        &self.executable
    }

    /// Returns arguments exactly as passed to the executable.
    #[must_use]
    pub fn arguments(&self) -> &[OsString] {
        &self.arguments
    }

    /// Returns the process deadline.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Returns the standard-output capture limit.
    #[must_use]
    pub const fn stdout_limit(&self) -> usize {
        self.stdout_limit
    }

    /// Returns the standard-error capture limit.
    #[must_use]
    pub const fn stderr_limit(&self) -> usize {
        self.stderr_limit
    }
}

impl fmt::Debug for FpcalcInvocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FpcalcInvocation")
            .field("argument_count", &self.arguments.len())
            .field("timeout", &self.timeout)
            .field("stdout_limit", &self.stdout_limit)
            .field("stderr_limit", &self.stderr_limit)
            .finish_non_exhaustive()
    }
}

/// Bounded output from an injected `fpcalc` process executor.
pub struct FpcalcProcessOutput {
    success: bool,
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stdout_exceeded_limit: bool,
    stderr_exceeded_limit: bool,
}

impl FpcalcProcessOutput {
    /// Creates a successful mock-process result.
    #[must_use]
    pub fn success(stdout: impl Into<Vec<u8>>) -> Self {
        Self {
            success: true,
            exit_code: Some(0),
            stdout: stdout.into(),
            stdout_exceeded_limit: false,
            stderr_exceeded_limit: false,
        }
    }

    /// Creates a failed mock-process result.
    #[must_use]
    pub fn failure(exit_code: Option<i32>) -> Self {
        Self {
            success: false,
            exit_code,
            stdout: Vec::new(),
            stdout_exceeded_limit: false,
            stderr_exceeded_limit: false,
        }
    }

    /// Marks standard output as having exceeded its configured capture limit.
    #[must_use]
    pub const fn with_stdout_limit_exceeded(mut self) -> Self {
        self.stdout_exceeded_limit = true;
        self
    }

    /// Marks standard error as having exceeded its configured capture limit.
    #[must_use]
    pub const fn with_stderr_limit_exceeded(mut self) -> Self {
        self.stderr_exceeded_limit = true;
        self
    }
}

impl fmt::Debug for FpcalcProcessOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FpcalcProcessOutput")
            .field("success", &self.success)
            .field("exit_code", &self.exit_code)
            .field("stdout_bytes", &self.stdout.len())
            .field("stdout_exceeded_limit", &self.stdout_exceeded_limit)
            .field("stderr_exceeded_limit", &self.stderr_exceeded_limit)
            .finish()
    }
}

/// Sanitized failure from an `fpcalc` process executor.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("{message}")]
pub struct FpcalcProcessError {
    kind: FpcalcProcessErrorKind,
    message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FpcalcProcessErrorKind {
    Failure,
    Cancelled,
}

impl FpcalcProcessError {
    /// Creates a path-free process failure for an injected executor.
    ///
    /// Callers must not include the selected file path or command arguments.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            kind: FpcalcProcessErrorKind::Failure,
            message: message.into(),
        }
    }

    /// Creates a cooperative-cancellation result for an injected executor.
    #[must_use]
    pub fn cancelled() -> Self {
        Self {
            kind: FpcalcProcessErrorKind::Cancelled,
            message: "fpcalc process was cancelled".to_owned(),
        }
    }

    fn is_cancelled(&self) -> bool {
        self.kind == FpcalcProcessErrorKind::Cancelled
    }
}

/// Injectable process boundary used by [`FpcalcFingerprintRunner`].
pub trait FpcalcProcess: Send {
    /// Executes one already-structured invocation.
    ///
    /// # Errors
    ///
    /// Returns a path-free diagnostic if the process cannot be started,
    /// polled, terminated, waited for, or read.
    fn execute(
        &mut self,
        invocation: &FpcalcInvocation,
        cancellation: &AudioIdentificationCancellation,
    ) -> Result<FpcalcProcessOutput, FpcalcProcessError>;
}

/// Shell-free operating-system process executor for `fpcalc`.
///
/// Cancellation terminates the configured executable directly. Configure the
/// actual `fpcalc` executable rather than a wrapper that leaves descendant
/// processes running.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemFpcalcProcess;

impl FpcalcProcess for SystemFpcalcProcess {
    fn execute(
        &mut self,
        invocation: &FpcalcInvocation,
        cancellation: &AudioIdentificationCancellation,
    ) -> Result<FpcalcProcessOutput, FpcalcProcessError> {
        if cancellation.is_cancelled() {
            return Err(FpcalcProcessError::cancelled());
        }
        let mut child = Command::new(&invocation.executable)
            .args(&invocation.arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| map_process_start_error(&error))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| FpcalcProcessError::new("fpcalc standard output was unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| FpcalcProcessError::new("fpcalc standard error was unavailable"))?;
        let stdout_limit = invocation.stdout_limit;
        let stderr_limit = invocation.stderr_limit;
        let stdout_capture = spawn_capture(stdout, stdout_limit);
        let stderr_capture = spawn_capture(stderr, stderr_limit);

        let deadline = Instant::now()
            .checked_add(invocation.timeout)
            .ok_or_else(|| FpcalcProcessError::new("fpcalc deadline was invalid"))?;
        let status = loop {
            if cancellation.is_cancelled() {
                terminate_direct_child(&mut child)?;
                return Err(FpcalcProcessError::cancelled());
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    terminate_direct_child(&mut child)?;
                    return Err(FpcalcProcessError::new("fpcalc process timed out"));
                }
                Ok(None) => thread::sleep(PROCESS_POLL_INTERVAL),
                Err(_) => {
                    terminate_direct_child(&mut child)?;
                    return Err(FpcalcProcessError::new("failed to poll the fpcalc process"));
                }
            }
        };

        let capture_deadline = Instant::now()
            .checked_add(PROCESS_CAPTURE_FINISH_TIMEOUT)
            .ok_or_else(|| FpcalcProcessError::new("fpcalc output deadline was invalid"))?;
        let stdout = receive_capture(&stdout_capture, capture_deadline)?;
        let stderr = receive_capture(&stderr_capture, capture_deadline)?;
        Ok(FpcalcProcessOutput {
            success: status.success(),
            exit_code: status.code(),
            stdout: stdout.bytes,
            stdout_exceeded_limit: stdout.exceeded_limit,
            stderr_exceeded_limit: stderr.exceeded_limit,
        })
    }
}

/// Fingerprint boundary used by [`LocalAudioIdentifier`].
pub trait FingerprintRunner: Send {
    /// Produces one validated fingerprint for a local regular file.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid paths, process failures, or malformed
    /// fingerprint output.
    fn fingerprint(
        &mut self,
        audio_path: &Path,
    ) -> Result<AudioFingerprint, AudioIdentificationError> {
        self.fingerprint_with_cancellation(audio_path, &AudioIdentificationCancellation::default())
    }

    /// Produces one validated fingerprint while observing cancellation.
    ///
    /// Implementations should return [`AudioIdentificationError::Cancelled`]
    /// promptly after `cancellation` is requested.
    ///
    /// # Errors
    ///
    /// Returns an error for cancellation, invalid paths, process failures, or
    /// malformed fingerprint output.
    fn fingerprint_with_cancellation(
        &mut self,
        audio_path: &Path,
        cancellation: &AudioIdentificationCancellation,
    ) -> Result<AudioFingerprint, AudioIdentificationError>;
}

/// `fpcalc`-backed [`FingerprintRunner`] with an injectable process boundary.
pub struct FpcalcFingerprintRunner<P = SystemFpcalcProcess> {
    config: FpcalcConfig,
    process: P,
}

impl<P> FpcalcFingerprintRunner<P> {
    /// Creates a runner from validated configuration and a process executor.
    #[must_use]
    pub const fn new(config: FpcalcConfig, process: P) -> Self {
        Self { config, process }
    }

    /// Consumes the runner and returns the process executor.
    #[must_use]
    pub fn into_process(self) -> P {
        self.process
    }
}

impl Default for FpcalcFingerprintRunner<SystemFpcalcProcess> {
    fn default() -> Self {
        Self::new(FpcalcConfig::default(), SystemFpcalcProcess)
    }
}

impl<P: FpcalcProcess> FingerprintRunner for FpcalcFingerprintRunner<P> {
    fn fingerprint_with_cancellation(
        &mut self,
        audio_path: &Path,
        cancellation: &AudioIdentificationCancellation,
    ) -> Result<AudioFingerprint, AudioIdentificationError> {
        if cancellation.is_cancelled() {
            return Err(AudioIdentificationError::Cancelled);
        }
        let metadata = std::fs::metadata(audio_path)
            .map_err(|_| AudioIdentificationError::InvalidAudioPath)?;
        if !metadata.is_file() {
            return Err(AudioIdentificationError::InvalidAudioPath);
        }
        let canonical_path = audio_path
            .canonicalize()
            .map_err(|_| AudioIdentificationError::InvalidAudioPath)?;
        let invocation = FpcalcInvocation {
            executable: self.config.executable.clone(),
            arguments: vec![
                OsString::from("-json"),
                OsString::from("--"),
                canonical_path.into_os_string(),
            ],
            timeout: self.config.timeout,
            stdout_limit: self.config.stdout_limit,
            stderr_limit: self.config.stderr_limit,
        };
        let output = self
            .process
            .execute(&invocation, cancellation)
            .map_err(|error| {
                if error.is_cancelled() {
                    AudioIdentificationError::Cancelled
                } else {
                    AudioIdentificationError::FpcalcProcess(error)
                }
            })?;
        if cancellation.is_cancelled() {
            return Err(AudioIdentificationError::Cancelled);
        }
        if output.stdout_exceeded_limit || output.stdout.len() > invocation.stdout_limit {
            return Err(AudioIdentificationError::FpcalcOutputTooLarge {
                stream: "standard output",
                limit: invocation.stdout_limit,
            });
        }
        if output.stderr_exceeded_limit {
            return Err(AudioIdentificationError::FpcalcOutputTooLarge {
                stream: "standard error",
                limit: invocation.stderr_limit,
            });
        }
        if !output.success {
            return Err(AudioIdentificationError::FpcalcFailed {
                exit_code: output.exit_code,
            });
        }
        parse_fpcalc_output(&output.stdout)
    }
}

/// `AcoustID` lookup request passed to an injected HTTP transport.
pub struct AcoustIdLookupRequest<'a> {
    client_key: &'a str,
    fingerprint: &'a AudioFingerprint,
}

impl AcoustIdLookupRequest<'_> {
    /// Returns the configured application client key.
    #[must_use]
    pub fn client_key(&self) -> &str {
        self.client_key
    }

    /// Returns the whole-file duration in seconds.
    #[must_use]
    pub const fn duration_seconds(&self) -> u32 {
        self.fingerprint.duration_seconds()
    }

    /// Returns the encoded Chromaprint fingerprint.
    #[must_use]
    pub fn fingerprint(&self) -> &str {
        self.fingerprint.encoded()
    }

    /// Returns the requested `AcoustID` metadata field.
    #[must_use]
    pub const fn metadata(&self) -> &'static str {
        "recordingids"
    }
}

impl fmt::Debug for AcoustIdLookupRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AcoustIdLookupRequest")
            .field("duration_seconds", &self.duration_seconds())
            .field("fingerprint_bytes", &self.fingerprint().len())
            .field("metadata", &self.metadata())
            .finish_non_exhaustive()
    }
}

/// Bounded JSON body returned by an injected `AcoustID` transport.
pub struct AcoustIdTransportResponse {
    body: Vec<u8>,
}

impl AcoustIdTransportResponse {
    /// Creates a response from JSON bytes.
    #[must_use]
    pub fn new(body: impl Into<Vec<u8>>) -> Self {
        Self { body: body.into() }
    }
}

impl fmt::Debug for AcoustIdTransportResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AcoustIdTransportResponse")
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// Sanitized `AcoustID` HTTP failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("{message}")]
pub struct AcoustIdTransportError {
    message: String,
}

impl AcoustIdTransportError {
    /// Creates a credential-free transport failure for an injected client.
    ///
    /// Callers must not include the API key, fingerprint, or request URL.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Injectable HTTP boundary used by [`LocalAudioIdentifier`].
pub trait AcoustIdTransport: Send {
    /// Performs one `AcoustID` fingerprint lookup.
    ///
    /// # Errors
    ///
    /// Returns a credential-free transport diagnostic on request failure.
    fn lookup(
        &mut self,
        request: &AcoustIdLookupRequest<'_>,
    ) -> Result<AcoustIdTransportResponse, AcoustIdTransportError>;
}

/// Bounded synchronous HTTPS transport for the official `AcoustID` service.
#[derive(Clone)]
pub struct UreqAcoustIdTransport {
    agent: ureq::Agent,
    max_response_bytes: usize,
}

impl UreqAcoustIdTransport {
    /// Creates a transport with a global timeout and fixed one-mebibyte body
    /// limit.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero timeout or a timeout above one minute.
    pub fn new(timeout: Duration) -> Result<Self, AudioIdentificationError> {
        if timeout.is_zero() || timeout > MAX_HTTP_TIMEOUT {
            return Err(AudioIdentificationError::InvalidHttpConfig);
        }
        Ok(Self {
            agent: acoustid_agent(timeout),
            max_response_bytes: MAX_ACOUSTID_RESPONSE_BYTES,
        })
    }
}

impl Default for UreqAcoustIdTransport {
    fn default() -> Self {
        Self::new(DEFAULT_HTTP_TIMEOUT).expect("the built-in AcoustID timeout is valid")
    }
}

impl fmt::Debug for UreqAcoustIdTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UreqAcoustIdTransport")
            .field("max_response_bytes", &self.max_response_bytes)
            .finish_non_exhaustive()
    }
}

impl AcoustIdTransport for UreqAcoustIdTransport {
    fn lookup(
        &mut self,
        request: &AcoustIdLookupRequest<'_>,
    ) -> Result<AcoustIdTransportResponse, AcoustIdTransportError> {
        let duration = request.duration_seconds().to_string();
        let mut response = self
            .agent
            .post(ACOUSTID_LOOKUP_ENDPOINT)
            .header("Accept", "application/json")
            .send_form([
                ("format", "json"),
                ("client", request.client_key()),
                ("duration", duration.as_str()),
                ("fingerprint", request.fingerprint()),
                ("meta", request.metadata()),
            ])
            .map_err(|error| map_acoustid_transport_error(&error))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(AcoustIdTransportError::new(format!(
                "AcoustID lookup returned HTTP status {status}"
            )));
        }
        if response
            .body()
            .content_length()
            .is_some_and(|length| length > self.max_response_bytes as u64)
        {
            return Err(AcoustIdTransportError::new(
                "AcoustID response exceeded its byte limit",
            ));
        }
        let body = response
            .body_mut()
            .with_config()
            .limit(self.max_response_bytes.saturating_add(1) as u64)
            .read_to_vec()
            .map_err(|error| map_acoustid_transport_error(&error))?;
        if body.len() > self.max_response_bytes {
            return Err(AcoustIdTransportError::new(
                "AcoustID response exceeded its byte limit",
            ));
        }
        Ok(AcoustIdTransportResponse::new(body))
    }
}

/// Object-safe boundary for explicitly identifying one local audio file.
///
/// Application workers can own this trait behind a `Box` and inject mock
/// implementations without depending on an HTTP transport or `fpcalc`.
pub trait AudioIdentifier: Send {
    /// Identifies one local audio file while observing cooperative
    /// cancellation.
    ///
    /// # Errors
    ///
    /// Returns an error when cancellation, fingerprinting, transport, response
    /// bounds, or response parsing fails.
    fn identify(
        &mut self,
        audio_path: &Path,
        cancellation: &AudioIdentificationCancellation,
    ) -> Result<Vec<MusicBrainzCandidate>, AudioIdentificationError>;
}

/// Explicit local-file identifier with injectable process and HTTP boundaries.
pub struct LocalAudioIdentifier<F, T> {
    fingerprint_runner: F,
    transport: T,
    client_key: String,
    lookup_limiter: AcoustIdLookupLimiter,
}

impl<F, T> LocalAudioIdentifier<F, T> {
    /// Creates an identifier using a configured `AcoustID` application key.
    ///
    /// # Errors
    ///
    /// Returns an error when the key is empty, over 128 bytes, or contains
    /// whitespace/control bytes.
    pub fn new(
        fingerprint_runner: F,
        transport: T,
        client_key: impl Into<String>,
    ) -> Result<Self, AudioIdentificationError> {
        let client_key = client_key.into();
        if client_key.is_empty()
            || client_key.len() > MAX_CLIENT_KEY_BYTES
            || !client_key.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(AudioIdentificationError::InvalidClientKey);
        }
        Ok(Self {
            fingerprint_runner,
            transport,
            client_key,
            lookup_limiter: AcoustIdLookupLimiter::default(),
        })
    }

    /// Consumes the identifier and returns its injected boundaries.
    #[must_use]
    pub fn into_parts(self) -> (F, T) {
        (self.fingerprint_runner, self.transport)
    }
}

impl LocalAudioIdentifier<FpcalcFingerprintRunner<SystemFpcalcProcess>, UreqAcoustIdTransport> {
    /// Creates an identifier using `fpcalc` from `PATH` and the official
    /// `AcoustID` HTTPS endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured application key is invalid.
    pub fn with_defaults(client_key: impl Into<String>) -> Result<Self, AudioIdentificationError> {
        Self::new(
            FpcalcFingerprintRunner::default(),
            UreqAcoustIdTransport::default(),
            client_key,
        )
    }
}

impl<F: FingerprintRunner, T: AcoustIdTransport> LocalAudioIdentifier<F, T> {
    /// Identifies one explicitly selected local audio file.
    ///
    /// Candidates are unique by recording UUID, ranked by descending
    /// `AcoustID` confidence score, and use canonical lowercase UUIDs and URLs.
    ///
    /// An empty vector means `AcoustID` returned no valid linked `MusicBrainz`
    /// recording.
    ///
    /// # Errors
    ///
    /// Returns an error when fingerprinting, transport, response bounds, or
    /// response parsing fails.
    pub fn identify(
        &mut self,
        audio_path: &Path,
    ) -> Result<Vec<MusicBrainzCandidate>, AudioIdentificationError> {
        self.identify_with_cancellation(audio_path, &AudioIdentificationCancellation::default())
    }

    /// Identifies one explicitly selected local audio file while observing
    /// cooperative cancellation.
    ///
    /// Cancellation interrupts `fpcalc` promptly. A synchronous HTTP lookup
    /// cannot be interrupted, but cancellation is checked immediately before
    /// and after that bounded request.
    ///
    /// # Errors
    ///
    /// Returns an error when cancellation, fingerprinting, transport, response
    /// bounds, or response parsing fails.
    pub fn identify_with_cancellation(
        &mut self,
        audio_path: &Path,
        cancellation: &AudioIdentificationCancellation,
    ) -> Result<Vec<MusicBrainzCandidate>, AudioIdentificationError> {
        let fingerprint = self
            .fingerprint_runner
            .fingerprint_with_cancellation(audio_path, cancellation)?;
        if cancellation.is_cancelled() {
            return Err(AudioIdentificationError::Cancelled);
        }
        let request = AcoustIdLookupRequest {
            client_key: &self.client_key,
            fingerprint: &fingerprint,
        };
        self.lookup_limiter.wait_for_slot(cancellation)?;
        let response = self.transport.lookup(&request)?;
        if cancellation.is_cancelled() {
            return Err(AudioIdentificationError::Cancelled);
        }
        parse_acoustid_response(&response.body)
    }
}

impl<F: FingerprintRunner, T: AcoustIdTransport> AudioIdentifier for LocalAudioIdentifier<F, T> {
    fn identify(
        &mut self,
        audio_path: &Path,
        cancellation: &AudioIdentificationCancellation,
    ) -> Result<Vec<MusicBrainzCandidate>, AudioIdentificationError> {
        self.identify_with_cancellation(audio_path, cancellation)
    }
}

impl<F, T> fmt::Debug for LocalAudioIdentifier<F, T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalAudioIdentifier")
            .finish_non_exhaustive()
    }
}

/// Failure identifying an explicitly selected local audio file.
#[derive(Debug, Error)]
pub enum AudioIdentificationError {
    /// The caller cancelled the in-flight identification request.
    #[error("audio identification was cancelled")]
    Cancelled,
    /// The configured `AcoustID` application client key is unusable.
    #[error("AcoustID client key is empty or invalid")]
    InvalidClientKey,
    /// The selected path does not resolve to a readable regular file.
    #[error("selected audio path is not a readable regular file")]
    InvalidAudioPath,
    /// The `fpcalc` executable, deadline, or output limits are invalid.
    #[error("fpcalc configuration is invalid")]
    InvalidFpcalcConfig,
    /// The `AcoustID` timeout is invalid.
    #[error("AcoustID HTTP configuration is invalid")]
    InvalidHttpConfig,
    /// The process executor could not complete the `fpcalc` invocation.
    #[error("fpcalc process error: {0}")]
    FpcalcProcess(#[from] FpcalcProcessError),
    /// The helper returned a non-success exit status.
    #[error("fpcalc failed with exit code {exit_code:?}")]
    FpcalcFailed {
        /// Portable numeric exit code, or `None` when terminated by a signal.
        exit_code: Option<i32>,
    },
    /// A helper output stream crossed its configured byte limit.
    #[error("fpcalc {stream} exceeded its {limit}-byte limit")]
    FpcalcOutputTooLarge {
        /// Name of the bounded process stream.
        stream: &'static str,
        /// Configured capture limit.
        limit: usize,
    },
    /// The helper JSON does not contain a usable duration and fingerprint.
    #[error("fpcalc returned invalid JSON fingerprint output")]
    InvalidFpcalcOutput,
    /// The reported whole-file duration cannot be sent to `AcoustID`.
    #[error("fpcalc returned an invalid whole-file duration")]
    InvalidDuration,
    /// The encoded fingerprint is empty, oversized, or malformed.
    #[error("fpcalc returned an invalid encoded fingerprint")]
    InvalidFingerprint,
    /// The HTTP boundary could not complete the lookup.
    #[error("AcoustID transport error: {0}")]
    AcoustIdTransport(#[from] AcoustIdTransportError),
    /// The injected HTTP response crossed the independent parser limit.
    #[error("AcoustID response exceeded its {limit}-byte limit")]
    AcoustIdResponseTooLarge {
        /// Parser-side response limit.
        limit: usize,
    },
    /// `AcoustID` returned malformed or unsupported JSON.
    #[error("AcoustID returned invalid lookup JSON")]
    InvalidAcoustIdResponse,
    /// `AcoustID` returned a structured API error.
    #[error("AcoustID rejected the lookup (code {code:?})")]
    AcoustIdRejected {
        /// `AcoustID` numeric error code, when supplied.
        code: Option<i64>,
    },
}

#[derive(Debug)]
struct CapturedOutput {
    bytes: Vec<u8>,
    exceeded_limit: bool,
}

fn drain_bounded(mut reader: impl Read, limit: usize) -> Result<CapturedOutput, io::Error> {
    let mut bytes = Vec::with_capacity(limit.min(8_192));
    let mut buffer = [0_u8; 8_192];
    let mut exceeded_limit = false;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        let retained = remaining.min(count);
        bytes.extend_from_slice(&buffer[..retained]);
        exceeded_limit |= retained < count;
    }
    Ok(CapturedOutput {
        bytes,
        exceeded_limit,
    })
}

fn spawn_capture(
    reader: impl Read + Send + 'static,
    limit: usize,
) -> Receiver<Result<CapturedOutput, io::Error>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = sender.send(drain_bounded(reader, limit));
    });
    receiver
}

fn receive_capture(
    receiver: &Receiver<Result<CapturedOutput, io::Error>>,
    deadline: Instant,
) -> Result<CapturedOutput, FpcalcProcessError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match receiver.recv_timeout(remaining) {
        Ok(Ok(capture)) => Ok(capture),
        Ok(Err(_)) => Err(FpcalcProcessError::new(
            "failed to read fpcalc process output",
        )),
        Err(RecvTimeoutError::Timeout) => Err(FpcalcProcessError::new(
            "fpcalc output did not close after the process exited",
        )),
        Err(RecvTimeoutError::Disconnected) => {
            Err(FpcalcProcessError::new("fpcalc output reader stopped"))
        }
    }
}

fn terminate_direct_child(child: &mut Child) -> Result<(), FpcalcProcessError> {
    if child.kill().is_err() {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) | Err(_) => {
                return Err(FpcalcProcessError::new(
                    "failed to terminate the fpcalc process",
                ));
            }
        }
    }
    child
        .wait()
        .map(|_| ())
        .map_err(|_| FpcalcProcessError::new("failed to wait for the fpcalc process"))
}

fn map_process_start_error(error: &io::Error) -> FpcalcProcessError {
    let message = match error.kind() {
        io::ErrorKind::NotFound => {
            format!("fpcalc executable was not found.\n{FPCALC_INSTALL_GUIDANCE}")
        }
        io::ErrorKind::PermissionDenied => "fpcalc executable is not permitted".to_owned(),
        _ => "failed to start fpcalc".to_owned(),
    };
    FpcalcProcessError::new(message)
}

#[derive(Deserialize)]
struct FpcalcJson {
    duration: f64,
    fingerprint: String,
}

fn parse_fpcalc_output(bytes: &[u8]) -> Result<AudioFingerprint, AudioIdentificationError> {
    let output: FpcalcJson =
        serde_json::from_slice(bytes).map_err(|_| AudioIdentificationError::InvalidFpcalcOutput)?;
    if !output.duration.is_finite()
        || output.duration < 0.5
        || output.duration > f64::from(u32::MAX)
    {
        return Err(AudioIdentificationError::InvalidDuration);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let duration_seconds = output.duration.round() as u32;
    AudioFingerprint::new(duration_seconds, output.fingerprint)
}

fn acoustid_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .max_redirects(0)
        .user_agent(concat!(
            "youta/",
            env!("CARGO_PKG_VERSION"),
            " (+",
            env!("CARGO_PKG_REPOSITORY"),
            ")"
        ))
        .build()
        .into()
}

fn map_acoustid_transport_error(error: &ureq::Error) -> AcoustIdTransportError {
    let message = match error {
        ureq::Error::StatusCode(status) => {
            return AcoustIdTransportError::new(format!(
                "AcoustID lookup returned HTTP status {status}"
            ));
        }
        ureq::Error::Timeout(_) => "AcoustID lookup timed out",
        ureq::Error::HostNotFound => "AcoustID host was not found",
        ureq::Error::ConnectionFailed => "AcoustID connection failed",
        ureq::Error::Tls(_) => "AcoustID TLS connection failed",
        ureq::Error::BodyExceedsLimit(_) => "AcoustID response exceeded its byte limit",
        _ => "AcoustID lookup failed",
    };
    AcoustIdTransportError::new(message)
}

#[derive(Deserialize)]
struct AcoustIdResponse {
    status: String,
    #[serde(default)]
    results: Vec<AcoustIdResult>,
    error: Option<AcoustIdApiError>,
}

#[derive(Deserialize)]
struct AcoustIdResult {
    id: Option<String>,
    score: Option<f64>,
    #[serde(default)]
    recordings: Vec<MusicBrainzRecording>,
}

#[derive(Deserialize)]
struct MusicBrainzRecording {
    id: Option<String>,
}

#[derive(Deserialize)]
struct AcoustIdApiError {
    code: Option<i64>,
}

fn parse_acoustid_response(
    bytes: &[u8],
) -> Result<Vec<MusicBrainzCandidate>, AudioIdentificationError> {
    if bytes.len() > MAX_ACOUSTID_RESPONSE_BYTES {
        return Err(AudioIdentificationError::AcoustIdResponseTooLarge {
            limit: MAX_ACOUSTID_RESPONSE_BYTES,
        });
    }
    let response: AcoustIdResponse = serde_json::from_slice(bytes)
        .map_err(|_| AudioIdentificationError::InvalidAcoustIdResponse)?;
    if response.status != "ok" {
        let code = response.error.as_ref().and_then(|error| error.code);
        return Err(AudioIdentificationError::AcoustIdRejected { code });
    }

    let mut candidates_by_recording = BTreeMap::<String, MusicBrainzCandidate>::new();
    for result in response.results {
        let Some(result_id) = result.id.as_deref().and_then(canonical_uuid) else {
            continue;
        };
        let Some(score) = result
            .score
            .filter(|score| score.is_finite() && (0.0..=1.0).contains(score))
        else {
            continue;
        };
        for recording in result.recordings {
            let Some(recording_id) = recording.id.as_deref() else {
                continue;
            };
            let Some(candidate) = MusicBrainzCandidate::new(recording_id, &result_id, score) else {
                continue;
            };
            match candidates_by_recording.entry(candidate.recording_id.clone()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(candidate);
                }
                std::collections::btree_map::Entry::Occupied(mut entry)
                    if candidate_is_better(&candidate, entry.get()) =>
                {
                    entry.insert(candidate);
                }
                std::collections::btree_map::Entry::Occupied(_) => {}
            }
        }
    }

    let mut candidates: Vec<_> = candidates_by_recording.into_values().collect();
    candidates.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.recording_id.cmp(&right.recording_id))
            .then_with(|| left.acoustid_result_id.cmp(&right.acoustid_result_id))
    });
    candidates.truncate(MAX_RECORDING_CANDIDATES);
    Ok(candidates)
}

fn candidate_is_better(candidate: &MusicBrainzCandidate, current: &MusicBrainzCandidate) -> bool {
    match candidate.score.total_cmp(&current.score) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Equal => candidate.acoustid_result_id < current.acoustid_result_id,
        std::cmp::Ordering::Less => false,
    }
}

fn canonical_uuid(value: &str) -> Option<String> {
    if value.len() != 36 {
        return None;
    }
    for (index, byte) in value.bytes().enumerate() {
        if matches!(index, 8 | 13 | 18 | 23) {
            if byte != b'-' {
                return None;
            }
        } else if !byte.is_ascii_hexdigit() {
            return None;
        }
    }
    Some(value.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        ffi::OsString,
        fs,
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };

    use tempfile::TempDir;

    use super::*;

    const RECORDING_ONE: &str = "38035858-f990-4fbb-b3b2-f2f8b958eeba";
    const RECORDING_TWO_UPPER: &str = "CD2E7C47-16F5-46C6-A37C-A1EB7BF599FF";
    const ACOUSTID_RESULT_ONE: &str = "11111111-1111-4111-8111-111111111111";
    const ACOUSTID_RESULT_TWO_UPPER: &str = "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA";

    #[derive(Default)]
    struct MockProcess {
        outputs: VecDeque<Result<FpcalcProcessOutput, FpcalcProcessError>>,
        invocations: Vec<FpcalcInvocation>,
    }

    impl FpcalcProcess for MockProcess {
        fn execute(
            &mut self,
            invocation: &FpcalcInvocation,
            _cancellation: &AudioIdentificationCancellation,
        ) -> Result<FpcalcProcessOutput, FpcalcProcessError> {
            self.invocations.push(invocation.clone());
            self.outputs
                .pop_front()
                .expect("mock process output must be configured")
        }
    }

    struct CancellationAwareProcess {
        started: Arc<AtomicBool>,
    }

    impl FpcalcProcess for CancellationAwareProcess {
        fn execute(
            &mut self,
            _invocation: &FpcalcInvocation,
            cancellation: &AudioIdentificationCancellation,
        ) -> Result<FpcalcProcessOutput, FpcalcProcessError> {
            self.started.store(true, Ordering::Release);
            let fallback_deadline = Instant::now() + Duration::from_secs(2);
            while !cancellation.is_cancelled() {
                if Instant::now() >= fallback_deadline {
                    return Err(FpcalcProcessError::new(
                        "mock process did not observe cancellation",
                    ));
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(FpcalcProcessError::cancelled())
        }
    }

    struct AdvancingLookupClock {
        now: Instant,
        waits: Arc<Mutex<Vec<Duration>>>,
        cancel_on_sleep: bool,
    }

    impl MonotonicLookupClock for AdvancingLookupClock {
        fn now(&self) -> Instant {
            self.now
        }

        fn sleep(&mut self, duration: Duration, cancellation: &AudioIdentificationCancellation) {
            self.waits.lock().expect("mock lookup waits").push(duration);
            if self.cancel_on_sleep {
                cancellation.cancel();
            } else {
                self.now = self
                    .now
                    .checked_add(duration)
                    .expect("bounded mock lookup time");
            }
        }
    }

    #[derive(Default)]
    struct MockTransport {
        responses: VecDeque<Result<AcoustIdTransportResponse, AcoustIdTransportError>>,
        requests: Vec<CapturedRequest>,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct CapturedRequest {
        client_key: String,
        duration_seconds: u32,
        fingerprint: String,
        metadata: &'static str,
    }

    impl AcoustIdTransport for MockTransport {
        fn lookup(
            &mut self,
            request: &AcoustIdLookupRequest<'_>,
        ) -> Result<AcoustIdTransportResponse, AcoustIdTransportError> {
            self.requests.push(CapturedRequest {
                client_key: request.client_key().to_owned(),
                duration_seconds: request.duration_seconds(),
                fingerprint: request.fingerprint().to_owned(),
                metadata: request.metadata(),
            });
            self.responses
                .pop_front()
                .expect("mock transport response must be configured")
        }
    }

    struct StaticFingerprintRunner(AudioFingerprint);

    impl FingerprintRunner for StaticFingerprintRunner {
        fn fingerprint_with_cancellation(
            &mut self,
            _audio_path: &Path,
            cancellation: &AudioIdentificationCancellation,
        ) -> Result<AudioFingerprint, AudioIdentificationError> {
            if cancellation.is_cancelled() {
                return Err(AudioIdentificationError::Cancelled);
            }
            AudioFingerprint::new(self.0.duration_seconds(), self.0.encoded())
        }
    }

    fn selected_file() -> (TempDir, PathBuf) {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("track;$(never-run).flac");
        fs::write(&path, b"mock audio").expect("mock audio file");
        (directory, path)
    }

    #[test]
    fn fpcalc_runner_uses_json_and_option_terminator_without_a_shell() {
        let (_directory, path) = selected_file();
        let mut process = MockProcess::default();
        process.outputs.push_back(Ok(FpcalcProcessOutput::success(
            br#"{"duration": 183.51, "fingerprint": "AQAB_test-123"}"#,
        )));
        let config = FpcalcConfig::new("custom fpcalc").expect("valid configuration");
        let mut runner = FpcalcFingerprintRunner::new(config, process);

        let fingerprint = runner.fingerprint(&path).expect("valid fingerprint");
        let process = runner.into_process();
        let invocation = &process.invocations[0];

        assert_eq!(fingerprint.duration_seconds(), 184);
        assert_eq!(fingerprint.encoded(), "AQAB_test-123");
        assert_eq!(invocation.executable(), OsStr::new("custom fpcalc"));
        assert_eq!(
            invocation.arguments(),
            &[
                OsString::from("-json"),
                OsString::from("--"),
                path.canonicalize()
                    .expect("canonical mock path")
                    .into_os_string(),
            ]
        );
    }

    #[test]
    fn fpcalc_runner_rejects_non_files_before_process_execution() {
        let directory = TempDir::new().expect("temporary directory");
        let mut runner =
            FpcalcFingerprintRunner::new(FpcalcConfig::default(), MockProcess::default());

        let error = runner
            .fingerprint(directory.path())
            .expect_err("directories are not audio files");
        let process = runner.into_process();

        assert!(matches!(error, AudioIdentificationError::InvalidAudioPath));
        assert!(process.invocations.is_empty());
    }

    #[test]
    fn fpcalc_runner_skips_process_for_pre_cancelled_request() {
        let (_directory, path) = selected_file();
        let cancellation = AudioIdentificationCancellation::new();
        cancellation.cancel();
        let mut runner =
            FpcalcFingerprintRunner::new(FpcalcConfig::default(), MockProcess::default());

        let error = runner
            .fingerprint_with_cancellation(&path, &cancellation)
            .expect_err("cancelled work must not start a process");
        let process = runner.into_process();

        assert!(matches!(error, AudioIdentificationError::Cancelled));
        assert!(process.invocations.is_empty());
    }

    #[test]
    fn fpcalc_runner_forwards_cooperative_cancellation_to_process() {
        let (_directory, path) = selected_file();
        let cancellation = AudioIdentificationCancellation::new();
        let worker_cancellation = cancellation.clone();
        let started = Arc::new(AtomicBool::new(false));
        let process = CancellationAwareProcess {
            started: Arc::clone(&started),
        };
        let mut runner = FpcalcFingerprintRunner::new(FpcalcConfig::default(), process);
        let worker = thread::spawn(move || {
            runner.fingerprint_with_cancellation(&path, &worker_cancellation)
        });
        let started_deadline = Instant::now() + Duration::from_secs(1);
        while !started.load(Ordering::Acquire) && Instant::now() < started_deadline {
            thread::sleep(Duration::from_millis(1));
        }

        assert!(started.load(Ordering::Acquire));
        cancellation.cancel();
        let error = worker
            .join()
            .expect("mock fingerprint worker must not panic")
            .expect_err("cooperative cancellation must stop fingerprinting");

        assert!(matches!(error, AudioIdentificationError::Cancelled));
    }

    #[cfg(unix)]
    #[test]
    fn system_fpcalc_cancellation_terminates_the_direct_executable_promptly() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TempDir::new().expect("temporary directory");
        let executable = directory.path().join("blocking-fpcalc");
        let marker = PathBuf::from(format!("{}.started", executable.display()));
        fs::write(
            &executable,
            b"#!/bin/sh\n: > \"$0.started\"\nwhile :; do :; done\n",
        )
        .expect("mock executable");
        let mut permissions = fs::metadata(&executable)
            .expect("mock executable metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&executable, permissions).expect("executable permissions");
        let invocation = FpcalcInvocation {
            executable: executable.into_os_string(),
            arguments: Vec::new(),
            timeout: Duration::from_secs(5),
            stdout_limit: 64,
            stderr_limit: 64,
        };
        let cancellation = AudioIdentificationCancellation::new();
        let worker_cancellation = cancellation.clone();
        let worker =
            thread::spawn(move || SystemFpcalcProcess.execute(&invocation, &worker_cancellation));
        let started_deadline = Instant::now() + Duration::from_secs(2);
        while !marker.exists() && Instant::now() < started_deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(marker.exists(), "mock executable did not start");

        let cancellation_started = Instant::now();
        cancellation.cancel();
        let error = worker
            .join()
            .expect("system process worker must not panic")
            .expect_err("cancellation must stop the direct executable");

        assert_eq!(error, FpcalcProcessError::cancelled());
        assert!(
            cancellation_started.elapsed() < Duration::from_secs(2),
            "direct-process cancellation exceeded its bounded test deadline"
        );
    }

    #[test]
    fn fpcalc_runner_reports_bounded_process_output() {
        let (_directory, path) = selected_file();
        let mut process = MockProcess::default();
        process.outputs.push_back(Ok(
            FpcalcProcessOutput::success(Vec::new()).with_stdout_limit_exceeded()
        ));
        let config = FpcalcConfig::default()
            .with_output_limits(32, 16)
            .expect("valid small test limits");
        let mut runner = FpcalcFingerprintRunner::new(config, process);

        let error = runner
            .fingerprint(&path)
            .expect_err("oversized process output must fail");

        assert!(matches!(
            error,
            AudioIdentificationError::FpcalcOutputTooLarge {
                stream: "standard output",
                limit: 32
            }
        ));
    }

    #[test]
    fn fpcalc_runner_independently_checks_mock_process_output_size() {
        let (_directory, path) = selected_file();
        let mut process = MockProcess::default();
        process
            .outputs
            .push_back(Ok(FpcalcProcessOutput::success(vec![b'x'; 33])));
        let config = FpcalcConfig::default()
            .with_output_limits(32, 16)
            .expect("valid small test limits");
        let mut runner = FpcalcFingerprintRunner::new(config, process);

        let error = runner
            .fingerprint(&path)
            .expect_err("an injected process cannot bypass output bounds");

        assert!(matches!(
            error,
            AudioIdentificationError::FpcalcOutputTooLarge {
                stream: "standard output",
                limit: 32
            }
        ));
    }

    #[test]
    fn bounded_reader_discards_bytes_beyond_limit() {
        let capture = drain_bounded(&b"0123456789"[..], 4).expect("in-memory read");

        assert_eq!(capture.bytes, b"0123");
        assert!(capture.exceeded_limit);
    }

    #[test]
    fn missing_fpcalc_error_lists_verified_package_commands_without_source_details() {
        let source = io::Error::new(
            io::ErrorKind::NotFound,
            "private executable path must not be shown",
        );

        let rendered = map_process_start_error(&source).to_string();

        assert!(rendered.starts_with("fpcalc executable was not found."));
        for command in [
            "USE=tools emerge media-libs/chromaprint",
            "apt install libchromaprint-tools",
            "dnf install chromaprint-tools",
            "brew install chromaprint",
        ] {
            assert!(
                rendered.contains(command),
                "missing installation command: {command}"
            );
        }
        assert!(!rendered.contains("private executable path"));
    }

    #[test]
    fn candidate_constructor_canonicalizes_ids_and_rejects_invalid_scores() {
        let candidate =
            MusicBrainzCandidate::new(RECORDING_TWO_UPPER, ACOUSTID_RESULT_TWO_UPPER, 0.8)
                .expect("valid candidate");

        assert_eq!(
            candidate.recording_id(),
            "cd2e7c47-16f5-46c6-a37c-a1eb7bf599ff"
        );
        assert_eq!(
            candidate.acoustid_result_id(),
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
        );
        assert_eq!(candidate.score(), 0.8);
        assert!(MusicBrainzCandidate::new(RECORDING_ONE, ACOUSTID_RESULT_ONE, f64::NAN).is_none());
        assert!(MusicBrainzCandidate::new(RECORDING_ONE, ACOUSTID_RESULT_ONE, 1.1).is_none());
    }

    #[test]
    fn candidates_are_deterministic_across_result_order_and_score_ties() {
        let response_one = format!(
            r#"{{
                "status":"ok",
                "results":[
                    {{
                      "id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                      "score":0.7,
                      "recordings":[{{"id":"{RECORDING_ONE}"}}]
                    }},
                    {{
                      "id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                      "score":0.7,
                      "recordings":[{{"id":"{RECORDING_ONE}"}}]
                    }},
                    {{
                      "id":"cccccccc-cccc-4ccc-8ccc-cccccccccccc",
                      "score":0.9,
                      "recordings":[{{"id":"{RECORDING_TWO_UPPER}"}}]
                    }}
                ]
            }}"#
        );
        let response_two = format!(
            r#"{{
                "status":"ok",
                "results":[
                    {{
                      "id":"cccccccc-cccc-4ccc-8ccc-cccccccccccc",
                      "score":0.9,
                      "recordings":[{{"id":"{RECORDING_TWO_UPPER}"}}]
                    }},
                    {{
                      "id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                      "score":0.7,
                      "recordings":[{{"id":"{RECORDING_ONE}"}}]
                    }},
                    {{
                      "id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                      "score":0.7,
                      "recordings":[{{"id":"{RECORDING_ONE}"}}]
                    }}
                ]
            }}"#
        );

        let candidates_one =
            parse_acoustid_response(response_one.as_bytes()).expect("first valid response");
        let candidates_two =
            parse_acoustid_response(response_two.as_bytes()).expect("second valid response");

        assert_eq!(candidates_one, candidates_two);
        assert_eq!(
            candidates_one
                .iter()
                .map(MusicBrainzCandidate::recording_id)
                .collect::<Vec<_>>(),
            vec!["cd2e7c47-16f5-46c6-a37c-a1eb7bf599ff", RECORDING_ONE,]
        );
        assert_eq!(
            candidates_one[1].acoustid_result_id(),
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
        );
    }

    #[test]
    fn identifier_posts_required_fields_and_returns_ranked_unique_candidates() {
        let (_directory, path) = selected_file();
        let fingerprint = AudioFingerprint::new(641, "AQAB_mock").expect("valid mock fingerprint");
        let mut transport = MockTransport::default();
        transport
            .responses
            .push_back(Ok(AcoustIdTransportResponse::new(format!(
                r#"{{
                    "status":"ok",
                    "results":[
                        {{
                          "id":"{ACOUSTID_RESULT_ONE}",
                          "score":0.7,
                          "recordings":[
                            {{"id":"{RECORDING_ONE}"}},
                            {{"id":"{RECORDING_TWO_UPPER}"}},
                            {{"id":"not-a-recording-id"}}
                          ]
                        }},
                        {{
                          "id":"{ACOUSTID_RESULT_TWO_UPPER}",
                          "score":0.95,
                          "recordings":[{{"id":"{RECORDING_ONE}"}}]
                        }}
                    ]
                }}"#
            ))));
        let mut identifier =
            LocalAudioIdentifier::new(StaticFingerprintRunner(fingerprint), transport, "test-key")
                .expect("valid identifier");

        let candidates = identifier.identify(&path).expect("successful lookup");
        let (_, transport) = identifier.into_parts();

        assert_eq!(
            candidates
                .iter()
                .map(|candidate| (
                    candidate.recording_id(),
                    candidate.url().as_str(),
                    candidate.acoustid_result_id(),
                    candidate.score(),
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    RECORDING_ONE,
                    "https://musicbrainz.org/recording/38035858-f990-4fbb-b3b2-f2f8b958eeba",
                    "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                    0.95,
                ),
                (
                    "cd2e7c47-16f5-46c6-a37c-a1eb7bf599ff",
                    "https://musicbrainz.org/recording/cd2e7c47-16f5-46c6-a37c-a1eb7bf599ff",
                    ACOUSTID_RESULT_ONE,
                    0.7,
                ),
            ]
        );
        assert_eq!(
            transport.requests,
            vec![CapturedRequest {
                client_key: "test-key".to_owned(),
                duration_seconds: 641,
                fingerprint: "AQAB_mock".to_owned(),
                metadata: "recordingids",
            }]
        );
    }

    #[test]
    fn identifier_spaces_lookup_starts_below_the_service_rate_limit() {
        let (_directory, path) = selected_file();
        let fingerprint = AudioFingerprint::new(641, "AQAB_mock").expect("valid mock fingerprint");
        let mut transport = MockTransport::default();
        for _ in 0..3 {
            transport
                .responses
                .push_back(Ok(AcoustIdTransportResponse::new(
                    br#"{"status":"ok","results":[]}"#,
                )));
        }
        let waits = Arc::new(Mutex::new(Vec::new()));
        let mut identifier =
            LocalAudioIdentifier::new(StaticFingerprintRunner(fingerprint), transport, "test-key")
                .expect("valid identifier");
        identifier.lookup_limiter = AcoustIdLookupLimiter::new(Box::new(AdvancingLookupClock {
            now: Instant::now(),
            waits: Arc::clone(&waits),
            cancel_on_sleep: false,
        }));

        for _ in 0..3 {
            identifier.identify(&path).expect("rate-limited lookup");
        }
        let (_, transport) = identifier.into_parts();

        assert_eq!(
            waits.lock().expect("mock lookup waits").as_slice(),
            [MIN_ACOUSTID_LOOKUP_INTERVAL, MIN_ACOUSTID_LOOKUP_INTERVAL,]
        );
        assert_eq!(transport.requests.len(), 3);
    }

    #[test]
    fn identifier_cancels_while_waiting_for_the_next_lookup_slot() {
        let (_directory, path) = selected_file();
        let fingerprint = AudioFingerprint::new(641, "AQAB_mock").expect("valid mock fingerprint");
        let mut transport = MockTransport::default();
        transport
            .responses
            .push_back(Ok(AcoustIdTransportResponse::new(
                br#"{"status":"ok","results":[]}"#,
            )));
        let waits = Arc::new(Mutex::new(Vec::new()));
        let mut identifier =
            LocalAudioIdentifier::new(StaticFingerprintRunner(fingerprint), transport, "test-key")
                .expect("valid identifier");
        identifier.lookup_limiter = AcoustIdLookupLimiter::new(Box::new(AdvancingLookupClock {
            now: Instant::now(),
            waits: Arc::clone(&waits),
            cancel_on_sleep: true,
        }));
        identifier.identify(&path).expect("initial lookup");
        let cancellation = AudioIdentificationCancellation::new();

        let error = identifier
            .identify_with_cancellation(&path, &cancellation)
            .expect_err("rate-limit waiting must observe cancellation");
        let (_, transport) = identifier.into_parts();

        assert!(matches!(error, AudioIdentificationError::Cancelled));
        assert!(cancellation.is_cancelled());
        assert_eq!(
            waits.lock().expect("mock lookup waits").as_slice(),
            [MIN_ACOUSTID_LOOKUP_INTERVAL]
        );
        assert_eq!(
            transport.requests.len(),
            1,
            "a cancelled wait must not start another HTTP lookup"
        );
    }

    #[test]
    fn identifier_accepts_a_successful_lookup_without_matches() {
        let (_directory, path) = selected_file();
        let fingerprint = AudioFingerprint::new(120, "AQAB_mock").expect("valid mock fingerprint");
        let mut transport = MockTransport::default();
        transport
            .responses
            .push_back(Ok(AcoustIdTransportResponse::new(
                br#"{"status":"ok","results":[]}"#,
            )));
        let identifier =
            LocalAudioIdentifier::new(StaticFingerprintRunner(fingerprint), transport, "test-key")
                .expect("valid identifier");
        let mut identifier: Box<dyn AudioIdentifier> = Box::new(identifier);

        assert!(
            identifier
                .identify(&path, &AudioIdentificationCancellation::default())
                .expect("valid empty lookup")
                .is_empty()
        );
    }

    #[test]
    fn identifier_reports_api_error_codes_without_remote_messages() {
        let (_directory, path) = selected_file();
        let fingerprint = AudioFingerprint::new(120, "AQAB_mock").expect("valid mock fingerprint");
        let mut transport = MockTransport::default();
        transport
            .responses
            .push_back(Ok(AcoustIdTransportResponse::new(
                br#"{
                    "status":"error",
                    "error":{"code":4,"message":"invalid\nAPI key"}
                }"#,
            )));
        let mut identifier =
            LocalAudioIdentifier::new(StaticFingerprintRunner(fingerprint), transport, "test-key")
                .expect("valid identifier");

        let error = identifier
            .identify(&path)
            .expect_err("API rejection must fail");

        assert!(matches!(
            error,
            AudioIdentificationError::AcoustIdRejected { code: Some(4) }
        ));
        assert!(!error.to_string().contains("invalid"));
    }

    #[test]
    fn remote_error_messages_cannot_expose_the_client_key_or_fingerprint() {
        const CLIENT_KEY: &str = "echoed-secret-client-key";
        const FINGERPRINT: &str = "AQAB_echoed_secret_fingerprint";
        let (_directory, path) = selected_file();
        let fingerprint = AudioFingerprint::new(120, FINGERPRINT).expect("valid mock fingerprint");
        let mut transport = MockTransport::default();
        transport
            .responses
            .push_back(Ok(AcoustIdTransportResponse::new(format!(
                r#"{{
                    "status":"error",
                    "error":{{
                        "code":4,
                        "message":"client={CLIENT_KEY}; fingerprint={FINGERPRINT}"
                    }}
                }}"#
            ))));
        let mut identifier =
            LocalAudioIdentifier::new(StaticFingerprintRunner(fingerprint), transport, CLIENT_KEY)
                .expect("valid identifier");

        let error = identifier
            .identify(&path)
            .expect_err("API rejection must fail");
        let rendered = error.to_string();

        assert!(!rendered.contains(CLIENT_KEY));
        assert!(!rendered.contains(FINGERPRINT));
        assert_eq!(rendered, "AcoustID rejected the lookup (code Some(4))");
    }

    #[test]
    fn identifier_independently_rejects_oversized_mock_http_body() {
        let (_directory, path) = selected_file();
        let fingerprint = AudioFingerprint::new(120, "AQAB_mock").expect("valid mock fingerprint");
        let mut transport = MockTransport::default();
        transport
            .responses
            .push_back(Ok(AcoustIdTransportResponse::new(vec![
                b'x';
                MAX_ACOUSTID_RESPONSE_BYTES
                    + 1
            ])));
        let mut identifier =
            LocalAudioIdentifier::new(StaticFingerprintRunner(fingerprint), transport, "test-key")
                .expect("valid identifier");

        let error = identifier
            .identify(&path)
            .expect_err("oversized mock response must fail");

        assert!(matches!(
            error,
            AudioIdentificationError::AcoustIdResponseTooLarge {
                limit: MAX_ACOUSTID_RESPONSE_BYTES
            }
        ));
    }

    #[test]
    fn secrets_and_fingerprints_are_redacted_from_debug_output() {
        let fingerprint =
            AudioFingerprint::new(120, "AQAB_secret_fingerprint").expect("valid fingerprint");
        let request = AcoustIdLookupRequest {
            client_key: "secret-client-key",
            fingerprint: &fingerprint,
        };
        let identifier = LocalAudioIdentifier::new(
            StaticFingerprintRunner(
                AudioFingerprint::new(120, "another-secret").expect("valid fingerprint"),
            ),
            MockTransport::default(),
            "secret-client-key",
        )
        .expect("valid identifier");

        let request_debug = format!("{request:?}");
        let identifier_debug = format!("{identifier:?}");

        assert!(!request_debug.contains("secret-client-key"));
        assert!(!request_debug.contains("AQAB_secret_fingerprint"));
        assert!(!identifier_debug.contains("secret-client-key"));
        assert!(!identifier_debug.contains("another-secret"));
    }

    #[test]
    fn client_key_and_fingerprint_validation_reject_whitespace() {
        assert!(matches!(
            LocalAudioIdentifier::new(
                StaticFingerprintRunner(
                    AudioFingerprint::new(120, "AQAB_valid").expect("valid fingerprint")
                ),
                MockTransport::default(),
                "bad key"
            ),
            Err(AudioIdentificationError::InvalidClientKey)
        ));
        assert!(matches!(
            AudioFingerprint::new(120, "bad fingerprint"),
            Err(AudioIdentificationError::InvalidFingerprint)
        ));
    }
}
