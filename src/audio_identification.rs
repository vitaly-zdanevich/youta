//! Explicit local-audio identification through Chromaprint and `AcoustID`.
//!
//! [`LocalAudioIdentifier`] runs the installed `fpcalc` executable without a
//! shell, submits its bounded JSON fingerprint to `AcoustID`, and returns
//! canonical `MusicBrainz` recording URLs. Identification is deliberately
//! explicit: this module does not scan directories or fingerprint files in the
//! background.
//!
//! `AcoustID` documents the lookup parameters and its three-requests-per-second
//! service limit in the [web-service documentation][acoustid]. Chromaprint
//! documents the `fpcalc` utility on its [project page][chromaprint].
//!
//! [acoustid]: https://acoustid.org/webservice
//! [chromaprint]: https://acoustid.org/chromaprint

use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    fmt,
    io::{self, Read},
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use thiserror::Error;
use url::Url;

/// Official `AcoustID` fingerprint-lookup endpoint.
pub const ACOUSTID_LOOKUP_ENDPOINT: &str = "https://api.acoustid.org/v2/lookup";

/// Maximum accepted encoded Chromaprint fingerprint size.
pub const MAX_FINGERPRINT_BYTES: usize = 1_048_576;

/// Maximum accepted `AcoustID` response size.
pub const MAX_ACOUSTID_RESPONSE_BYTES: usize = 1_048_576;

/// Maximum number of unique `MusicBrainz` recording URLs returned per lookup.
pub const MAX_RECORDING_URLS: usize = 256;

const DEFAULT_FP_CALC_TIMEOUT: Duration = Duration::from_mins(2);
const MAX_FP_CALC_TIMEOUT: Duration = Duration::from_mins(10);
const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_HTTP_TIMEOUT: Duration = Duration::from_mins(1);
const DEFAULT_STDOUT_LIMIT: usize = 2_097_152;
const DEFAULT_STDERR_LIMIT: usize = 65_536;
const MAX_STDOUT_LIMIT: usize = 8_388_608;
const MAX_STDERR_LIMIT: usize = 1_048_576;
const MAX_CLIENT_KEY_BYTES: usize = 128;
const MAX_REMOTE_ERROR_CHARS: usize = 256;
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(25);

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
    message: String,
}

impl FpcalcProcessError {
    /// Creates a path-free process failure for an injected executor.
    ///
    /// Callers must not include the selected file path or command arguments.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
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
    ) -> Result<FpcalcProcessOutput, FpcalcProcessError>;
}

/// Shell-free operating-system process executor for `fpcalc`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemFpcalcProcess;

impl FpcalcProcess for SystemFpcalcProcess {
    fn execute(
        &mut self,
        invocation: &FpcalcInvocation,
    ) -> Result<FpcalcProcessOutput, FpcalcProcessError> {
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
        let stdout_reader = thread::spawn(move || drain_bounded(stdout, stdout_limit));
        let stderr_reader = thread::spawn(move || drain_bounded(stderr, stderr_limit));

        let deadline = Instant::now()
            .checked_add(invocation.timeout)
            .ok_or_else(|| FpcalcProcessError::new("fpcalc deadline was invalid"))?;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    join_capture(stdout_reader)?;
                    join_capture(stderr_reader)?;
                    return Err(FpcalcProcessError::new("fpcalc process timed out"));
                }
                Ok(None) => thread::sleep(PROCESS_POLL_INTERVAL),
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    join_capture(stdout_reader)?;
                    join_capture(stderr_reader)?;
                    return Err(FpcalcProcessError::new("failed to poll the fpcalc process"));
                }
            }
        };

        let stdout = join_capture(stdout_reader)?;
        let stderr = join_capture(stderr_reader)?;
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
    fn fingerprint(
        &mut self,
        audio_path: &Path,
    ) -> Result<AudioFingerprint, AudioIdentificationError> {
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
        let output = self.process.execute(&invocation)?;
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

/// Explicit local-file identifier with injectable process and HTTP boundaries.
pub struct LocalAudioIdentifier<F, T> {
    fingerprint_runner: F,
    transport: T,
    client_key: String,
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
    /// URLs are unique, retain `AcoustID` result order, use lowercase UUIDs, and
    /// always have the form
    /// `https://musicbrainz.org/recording/00000000-0000-0000-0000-000000000000`.
    ///
    /// An empty vector means `AcoustID` returned no valid linked `MusicBrainz`
    /// recording.
    ///
    /// # Errors
    ///
    /// Returns an error when fingerprinting, transport, response bounds, or
    /// response parsing fails.
    pub fn identify(&mut self, audio_path: &Path) -> Result<Vec<Url>, AudioIdentificationError> {
        let fingerprint = self.fingerprint_runner.fingerprint(audio_path)?;
        let request = AcoustIdLookupRequest {
            client_key: &self.client_key,
            fingerprint: &fingerprint,
        };
        let response = self.transport.lookup(&request)?;
        parse_acoustid_response(&response.body)
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
    #[error("AcoustID rejected the lookup (code {code:?}): {message}")]
    AcoustIdRejected {
        /// `AcoustID` numeric error code, when supplied.
        code: Option<i64>,
        /// Bounded, control-free service message.
        message: String,
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

fn join_capture(
    handle: thread::JoinHandle<Result<CapturedOutput, io::Error>>,
) -> Result<CapturedOutput, FpcalcProcessError> {
    handle
        .join()
        .map_err(|_| FpcalcProcessError::new("fpcalc output reader panicked"))?
        .map_err(|_| FpcalcProcessError::new("failed to read fpcalc process output"))
}

fn map_process_start_error(error: &io::Error) -> FpcalcProcessError {
    let message = match error.kind() {
        io::ErrorKind::NotFound => "fpcalc executable was not found",
        io::ErrorKind::PermissionDenied => "fpcalc executable is not permitted",
        _ => "failed to start fpcalc",
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
    #[serde(default)]
    recordings: Vec<MusicBrainzRecording>,
}

#[derive(Deserialize)]
struct MusicBrainzRecording {
    id: String,
}

#[derive(Deserialize)]
struct AcoustIdApiError {
    code: Option<i64>,
    message: Option<String>,
}

fn parse_acoustid_response(bytes: &[u8]) -> Result<Vec<Url>, AudioIdentificationError> {
    if bytes.len() > MAX_ACOUSTID_RESPONSE_BYTES {
        return Err(AudioIdentificationError::AcoustIdResponseTooLarge {
            limit: MAX_ACOUSTID_RESPONSE_BYTES,
        });
    }
    let response: AcoustIdResponse = serde_json::from_slice(bytes)
        .map_err(|_| AudioIdentificationError::InvalidAcoustIdResponse)?;
    if response.status != "ok" {
        let code = response.error.as_ref().and_then(|error| error.code);
        let message = response.error.and_then(|error| error.message).map_or_else(
            || "service returned an unspecified error".to_owned(),
            |message| sanitize_remote_message(&message),
        );
        return Err(AudioIdentificationError::AcoustIdRejected { code, message });
    }

    let mut seen = HashSet::new();
    let mut urls = Vec::new();
    for recording in response
        .results
        .into_iter()
        .flat_map(|result| result.recordings)
    {
        let Some(id) = canonical_uuid(&recording.id) else {
            continue;
        };
        if seen.insert(id.clone()) {
            let url = Url::parse(&format!("https://musicbrainz.org/recording/{id}"))
                .expect("a validated UUID always forms a canonical MusicBrainz URL");
            urls.push(url);
            if urls.len() == MAX_RECORDING_URLS {
                break;
            }
        }
    }
    Ok(urls)
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

fn sanitize_remote_message(message: &str) -> String {
    let sanitized: String = message
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_REMOTE_ERROR_CHARS)
        .collect();
    if sanitized.is_empty() {
        "service returned an unspecified error".to_owned()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        ffi::OsString,
        fs,
        path::{Path, PathBuf},
    };

    use tempfile::TempDir;

    use super::*;

    const RECORDING_ONE: &str = "38035858-f990-4fbb-b3b2-f2f8b958eeba";
    const RECORDING_TWO_UPPER: &str = "CD2E7C47-16F5-46C6-A37C-A1EB7BF599FF";

    #[derive(Default)]
    struct MockProcess {
        outputs: VecDeque<Result<FpcalcProcessOutput, FpcalcProcessError>>,
        invocations: Vec<FpcalcInvocation>,
    }

    impl FpcalcProcess for MockProcess {
        fn execute(
            &mut self,
            invocation: &FpcalcInvocation,
        ) -> Result<FpcalcProcessOutput, FpcalcProcessError> {
            self.invocations.push(invocation.clone());
            self.outputs
                .pop_front()
                .expect("mock process output must be configured")
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
        fn fingerprint(
            &mut self,
            _audio_path: &Path,
        ) -> Result<AudioFingerprint, AudioIdentificationError> {
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
    fn identifier_posts_required_fields_and_returns_canonical_unique_urls() {
        let (_directory, path) = selected_file();
        let fingerprint = AudioFingerprint::new(641, "AQAB_mock").expect("valid mock fingerprint");
        let mut transport = MockTransport::default();
        transport
            .responses
            .push_back(Ok(AcoustIdTransportResponse::new(format!(
                r#"{{
                    "status":"ok",
                    "results":[
                        {{"recordings":[
                            {{"id":"{RECORDING_ONE}"}},
                            {{"id":"{RECORDING_TWO_UPPER}"}},
                            {{"id":"not-a-recording-id"}}
                        ]}},
                        {{"recordings":[{{"id":"{RECORDING_ONE}"}}]}}
                    ]
                }}"#
            ))));
        let mut identifier =
            LocalAudioIdentifier::new(StaticFingerprintRunner(fingerprint), transport, "test-key")
                .expect("valid identifier");

        let urls = identifier.identify(&path).expect("successful lookup");
        let (_, transport) = identifier.into_parts();

        assert_eq!(
            urls.iter().map(Url::as_str).collect::<Vec<_>>(),
            vec![
                "https://musicbrainz.org/recording/38035858-f990-4fbb-b3b2-f2f8b958eeba",
                "https://musicbrainz.org/recording/cd2e7c47-16f5-46c6-a37c-a1eb7bf599ff",
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
    fn identifier_accepts_a_successful_lookup_without_matches() {
        let (_directory, path) = selected_file();
        let fingerprint = AudioFingerprint::new(120, "AQAB_mock").expect("valid mock fingerprint");
        let mut transport = MockTransport::default();
        transport
            .responses
            .push_back(Ok(AcoustIdTransportResponse::new(
                br#"{"status":"ok","results":[]}"#,
            )));
        let mut identifier =
            LocalAudioIdentifier::new(StaticFingerprintRunner(fingerprint), transport, "test-key")
                .expect("valid identifier");

        assert!(
            identifier
                .identify(&path)
                .expect("valid empty lookup")
                .is_empty()
        );
    }

    #[test]
    fn identifier_reports_bounded_sanitized_api_errors() {
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
            AudioIdentificationError::AcoustIdRejected {
                code: Some(4),
                ref message
            } if message == "invalidAPI key"
        ));
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
