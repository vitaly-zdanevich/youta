//! Bounded playback of a YouTube broadcast from its original beginning.
//!
//! The extractor's live fragment generator cannot be passed through mpv's
//! JSON hook. This adapter keeps yt-dlp running and serves its original audio
//! bytes through a private loopback endpoint instead.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::task::Waker;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;
use tempfile::TempDir;
use url::Url;

use super::youtube_prewarm::{YouTubePrewarmCancellation, run_bounded_json_command};
use super::{PlaybackError, Result};
use crate::worker_wake::WorkerNotifier;

const MAX_START_AGE: Duration = Duration::from_secs(120 * 60 * 60);
const CHUNK_BYTES: usize = 32 * 1024;
const QUEUED_CHUNKS: usize = 8;
const MAX_HEADERS: usize = 8192;
const CANCEL_CHECK: Duration = Duration::from_millis(50);
const SOCKET_WAIT: Duration = Duration::from_millis(250);
const AUDIO_FORMAT: &str =
    "bestaudio[protocol=http_dash_segments_generator]/bestaudio[protocol=http_dash_segments]";
const METADATA_TEMPLATE: &str = "{\"id\":%(id)j,\"live_status\":%(live_status)j,\"release_timestamp\":%(release_timestamp)j,\"protocol\":%(protocol)j,\"acodec\":%(acodec)j,\"vcodec\":%(vcodec)j,\"ext\":%(ext)j,\"is_from_start\":%(is_from_start|false)j,\"first_fragment_url\":%(fragments.0.url|null)j}";

/// Limits and executable used by one explicitly requested from-start load.
#[derive(Clone, Debug)]
pub struct LiveFromStartConfig {
    /// Fixed yt-dlp executable, invoked directly without a shell.
    pub executable: PathBuf,
    /// Shared deadline for metadata validation and receiving initial audio.
    pub startup_timeout: Duration,
    /// Whether explicitly installed extractor plugins may execute.
    pub allow_plugins: bool,
}

impl Default for LiveFromStartConfig {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("yt-dlp"),
            startup_timeout: Duration::from_secs(40),
            allow_plugins: false,
        }
    }
}

/// Shared cancellation and sanitized connection/download failure state.
#[derive(Default)]
struct TransportState {
    stopped: AtomicBool,
    active_socket: Mutex<Option<TcpStream>>,
    failure: Mutex<Option<String>>,
    process_done: Mutex<bool>,
    process_complete: Condvar,
    notifier: WorkerNotifier,
}

impl TransportState {
    /// Keeps the first failure without retaining extractor text or media URLs.
    fn fail(&self, message: &str) {
        if !self.stopped.load(Ordering::Acquire) {
            let publish = {
                let mut failure = self.failure.lock().expect("live transport failure lock");
                let first = failure.is_none();
                failure.get_or_insert_with(|| message.to_owned());
                first
            };
            if publish {
                self.notifier.wake();
            }
        }
    }

    /// Keeps HTTP EOF behind the helper's authoritative completion/failure state.
    fn wait_for_process(&self) {
        let mut done = self
            .process_done
            .lock()
            .expect("live process completion lock");
        while !*done && !self.stopped.load(Ordering::Acquire) {
            done = self
                .process_complete
                .wait(done)
                .expect("live process completion lock");
        }
    }
}

/// A bounded binary event from the extractor's stdout reader.
enum BodyEvent {
    Bytes(Vec<u8>),
    End,
    Failed,
}

/// Owns one validated broadcast-start download and its private HTTP endpoint.
///
/// Prepare this guard on a worker. Keeping it alive retains the child and
/// source bytes; dropping or stopping it terminates the supervised process
/// tree, wakes blocked I/O, joins its workers, and removes fragment artifacts.
pub struct LiveFromStartStream {
    url: Url,
    address: SocketAddr,
    child: Option<Arc<Mutex<Child>>>,
    directory: Option<TempDir>,
    state: Arc<TransportState>,
    pending: Option<Receiver<BodyEvent>>,
    threads: Vec<JoinHandle<()>>,
    failure_taken: bool,
}

impl std::fmt::Debug for LiveFromStartStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiveFromStartStream")
            .field("stopped", &self.state.stopped.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl LiveFromStartStream {
    /// Validates the original broadcast start and waits for its first audio bytes.
    ///
    /// The URL must be a credential-free canonical HTTPS YouTube watch URL.
    /// Metadata and audio startup share one bounded deadline and cancellation
    /// token. Neither a rolling cache nor the live edge is used as a fallback.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error for unavailable beginnings, cancellation,
    /// timeout, malformed metadata, missing helpers, or failed initial audio.
    pub fn prepare(
        config: &LiveFromStartConfig,
        source: &Url,
        cancellation: &YouTubePrewarmCancellation,
    ) -> Result<Self> {
        let video_id = validate_request(config, source)?;
        // Retain only stable public identity in the helper argument vector.
        let source = Url::parse(&format!("https://www.youtube.com/watch?v={video_id}"))
            .expect("validated ASCII video ID forms a canonical watch URL");
        let deadline = Instant::now() + config.startup_timeout;
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        let directory = tempfile::Builder::new()
            .prefix("youta-live-start-")
            .rand_bytes(24)
            .tempdir()?;
        crate::private_files::set_private_directory_permissions(directory.path())?;
        let mut metadata_command = base_command(config)?;
        metadata_command
            .current_dir(directory.path())
            .args(["--skip-download", "--print", METADATA_TEMPLATE, "--"])
            .arg(source.as_str());
        let metadata_bytes = run_bounded_json_command(
            &mut metadata_command,
            remaining(deadline)?,
            8192,
            cancellation,
        )
        .map_err(|error| {
            PlaybackError::Protocol(format!(
                "Could not resolve the broadcast's beginning: {error}"
            ))
        })?;
        let metadata = validate_metadata(&metadata_bytes, &video_id)?;
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        remaining(deadline)?;
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let address = listener.local_addr()?;
        let capability = capability_token()?;
        let route = format!("/{capability}");
        let url = Url::parse(&format!("http://{address}{route}"))
            .expect("loopback address and ASCII capability form a URL");
        let mut command = base_command(config)?;
        command
            .current_dir(directory.path())
            .args(["--no-progress", "--output", "-", "--"])
            .arg(source.as_str())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::child_process::supervised(&mut command);
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                PlaybackError::ExecutableUnavailable(config.executable.display().to_string())
            } else {
                PlaybackError::Io(error)
            }
        })?;
        let mut stdout = child.stdout.take().expect("piped extractor stdout");
        let mut stderr = child.stderr.take().expect("piped extractor stderr");
        let (sender, receiver) = mpsc::sync_channel(QUEUED_CHUNKS);
        let state = Arc::new(TransportState::default());
        let child = Arc::new(Mutex::new(child));
        let mut guard = Self {
            url,
            address,
            child: Some(Arc::clone(&child)),
            directory: Some(directory),
            state: Arc::clone(&state),
            pending: Some(receiver),
            threads: Vec::new(),
            failure_taken: false,
        };
        let body_state = Arc::clone(&state);
        guard.threads.push(
            thread::Builder::new()
                .name("youta-live-audio".to_owned())
                .spawn(move || {
                    let mut bytes = [0_u8; CHUNK_BYTES];
                    loop {
                        let event = match stdout.read(&mut bytes) {
                            Ok(0) => {
                                body_state.wait_for_process();
                                BodyEvent::End
                            }
                            Ok(count) => BodyEvent::Bytes(bytes[..count].to_vec()),
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                            Err(_) => {
                                body_state.fail("Live-from-start audio read failed");
                                BodyEvent::Failed
                            }
                        };
                        let finished = !matches!(&event, BodyEvent::Bytes(_));
                        if sender.send(event).is_err() || finished {
                            break;
                        }
                    }
                })?,
        );
        let errors = Arc::clone(&state);
        guard.threads.push(
            thread::Builder::new()
                .name("youta-live-errors".to_owned())
                .spawn(move || {
                    let mut bytes = [0_u8; 8192];
                    let mut tail = Vec::new();
                    loop {
                        let count = match stderr.read(&mut bytes) {
                            Ok(0) => break,
                            Ok(count) => count,
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                            Err(_) => {
                                errors.fail("Live-from-start audio diagnostics failed");
                                break;
                            }
                        };
                        tail.extend_from_slice(&bytes[..count]);
                        let text = String::from_utf8_lossy(&tail);
                        if text.contains("Starting download from the last 120 hours") {
                            errors.fail("The beginning of this broadcast is unavailable");
                        } else if text.contains("ERROR:") {
                            errors.fail("Live-from-start audio download failed");
                        }
                        let retain = tail.len().saturating_sub(128);
                        tail.drain(..retain);
                    }
                    // Normal stderr EOF is the process-completion event. Poll only this
                    // short final race, never throughout a running broadcast.
                    while !errors.stopped.load(Ordering::Acquire) {
                        let status = child.lock().expect("live process lock").try_wait();
                        match status {
                            Ok(None) => thread::park_timeout(CANCEL_CHECK),
                            result => {
                                if !matches!(result, Ok(Some(status)) if status.success()) {
                                    errors.fail("Live-from-start audio download failed");
                                }
                                *errors
                                    .process_done
                                    .lock()
                                    .expect("live process completion lock") = true;
                                errors.process_complete.notify_all();
                                errors.notifier.wake();
                                break;
                            }
                        }
                    }
                })?,
        );
        let first = loop {
            if cancellation.is_cancelled() {
                return Err(cancelled());
            }
            if let Some(error) = guard.error() {
                return Err(PlaybackError::Protocol(error));
            }
            let wait = remaining(deadline)?.min(CANCEL_CHECK);
            match guard
                .pending
                .as_ref()
                .expect("startup receiver")
                .recv_timeout(wait)
            {
                Ok(BodyEvent::Bytes(bytes)) => break bytes,
                Err(RecvTimeoutError::Timeout) => {}
                _ => {
                    return Err(PlaybackError::Protocol(
                        "The broadcast's initial audio is unavailable".to_owned(),
                    ));
                }
            }
        };
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        if let Some(error) = guard.error() {
            return Err(PlaybackError::Protocol(error));
        }
        let receiver = guard.pending.take().expect("prepared audio receiver");
        guard.threads.push(
            thread::Builder::new()
                .name("youta-live-http".to_owned())
                .spawn(move || {
                    serve(
                        listener,
                        &route,
                        metadata.content_type(),
                        first,
                        receiver,
                        &state,
                    );
                })?,
        );
        Ok(guard)
    }

    /// Returns the capability-protected URL to load with mpv's extractor disabled.
    #[must_use]
    pub const fn url(&self) -> &Url {
        &self.url
    }

    /// Returns a sanitized asynchronous downloader or connection failure.
    ///
    /// Poll this while the guard is retained; a nonzero extractor exit is not
    /// successful playback completion and must not trigger a live-edge retry.
    pub fn error(&self) -> Option<String> {
        self.state
            .failure
            .lock()
            .expect("live transport failure lock")
            .clone()
    }

    /// Consumes an asynchronous failure once, before normal EOF/autoplay handling.
    pub fn take_error(&mut self) -> Option<String> {
        if self.failure_taken {
            return None;
        }
        let error = self.error();
        if error.is_some() {
            self.failure_taken = true;
        }
        error
    }

    /// Registers frontend notification and replays failures queued before attach.
    pub fn set_worker_waker(&self, waker: Option<Waker>) {
        self.state.notifier.set_waker(waker);
    }

    /// Stops the entire helper tree and wakes every blocked transport operation.
    ///
    /// Repeated calls are harmless. This releases the listener, active client,
    /// bounded audio queue, worker threads, and private fragment directory.
    pub fn stop(&mut self) {
        if self.state.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(socket) = self
            .state
            .active_socket
            .lock()
            .expect("live transport socket lock")
            .take()
        {
            let _ = socket.shutdown(Shutdown::Both);
        }
        self.pending.take();
        *self
            .state
            .process_done
            .lock()
            .expect("live process completion lock") = true;
        self.state.process_complete.notify_all();
        let _ = TcpStream::connect_timeout(&self.address, SOCKET_WAIT);
        if let Some(child) = self.child.take() {
            crate::child_process::terminate_tree(&mut child.lock().expect("live process lock"));
        }
        for worker in self.threads.drain(..) {
            let _ = worker.join();
        }
        self.directory.take();
    }
}

impl Drop for LiveFromStartStream {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Compact selected-format fields; signed fragment URLs remain in RAM only.
#[derive(Deserialize)]
struct StartMetadata {
    id: String,
    live_status: String,
    release_timestamp: i64,
    protocol: String,
    acodec: String,
    vcodec: String,
    ext: String,
    is_from_start: Value,
    first_fragment_url: Option<String>,
}

impl StartMetadata {
    /// Names the untouched container, avoiding a transcoding or remuxing step.
    fn content_type(&self) -> &'static str {
        match self.ext.as_str() {
            "m4a" | "mp4" => "audio/mp4",
            "webm" => "audio/webm",
            _ => "application/octet-stream",
        }
    }
}

/// Rejects unsafe sources and settings before starting a helper.
fn validate_request(config: &LiveFromStartConfig, source: &Url) -> Result<String> {
    if config.executable.as_os_str().is_empty()
        || config.startup_timeout.is_zero()
        || config.startup_timeout > Duration::from_secs(300)
    {
        return Err(PlaybackError::InvalidValue(
            "Invalid live-from-start executable or timeout".to_owned(),
        ));
    }
    let id = source
        .query_pairs()
        .find(|(key, _)| key == "v")
        .map(|(_, value)| value.into_owned());
    if source.scheme() != "https"
        || !matches!(
            source.host_str(),
            Some("www.youtube.com" | "youtube.com" | "m.youtube.com")
        )
        || source.path() != "/watch"
        || !source.username().is_empty()
        || source.password().is_some()
        || source.port_or_known_default() != Some(443)
        || source.as_str().len() > 4096
        || id.as_ref().is_none_or(|id| {
            id.len() != 11
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
    {
        return Err(PlaybackError::InvalidValue(
            "Live-from-start playback requires a canonical HTTPS YouTube watch URL".to_owned(),
        ));
    }
    Ok(id.expect("validated YouTube video ID"))
}

/// Refuses the extractor's truncated history behavior and non-start formats.
fn validate_metadata(bytes: &[u8], video_id: &str) -> Result<StartMetadata> {
    let unavailable =
        || PlaybackError::Protocol("The beginning of this broadcast is unavailable".to_owned());
    let metadata: StartMetadata = serde_json::from_slice(bytes).map_err(|_| unavailable())?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| unavailable())?
        .as_secs();
    let start = u64::try_from(metadata.release_timestamp)
        .ok()
        .filter(|start| *start > 0);
    if metadata.id != video_id
        || start.is_none_or(|start| start > now || now - start >= MAX_START_AGE.as_secs())
        || metadata.vcodec != "none"
        || metadata.acodec.is_empty()
        || metadata.acodec == "none"
    {
        return Err(unavailable());
    }
    let starts_at_zero = match metadata.live_status.as_str() {
        "is_live" => {
            metadata.protocol == "http_dash_segments_generator"
                && metadata.is_from_start.as_bool() == Some(true)
        }
        "post_live" => {
            metadata.protocol == "http_dash_segments"
                && metadata
                    .first_fragment_url
                    .as_deref()
                    .and_then(|url| Url::parse(url).ok())
                    .is_some_and(|url| {
                        url.query_pairs()
                            .any(|(key, value)| key == "sq" && value == "0")
                    })
        }
        _ => false,
    };
    if !starts_at_zero {
        return Err(unavailable());
    }
    Ok(metadata)
}

/// Builds a noninteractive, strict from-start invocation without a shell.
fn base_command(config: &LiveFromStartConfig) -> Result<Command> {
    let executable =
        if !config.executable.is_absolute() && config.executable.components().count() > 1 {
            std::env::current_dir()?.join(&config.executable)
        } else {
            config.executable.clone()
        };
    let mut command = Command::new(executable);
    crate::child_process::quiet(&mut command);
    command.args([
        "--ignore-config",
        "--no-cache-dir",
        "--no-playlist",
        "--live-from-start",
        "--abort-on-unavailable-fragments",
        "--format",
        AUDIO_FORMAT,
        "--socket-timeout",
        "10",
        "--retries",
        "1",
        "--fragment-retries",
        "1",
        "--extractor-retries",
        "1",
        "--js-runtimes",
        super::ytdlp::ADDITIONAL_JS_RUNTIME,
    ]);
    if !config.allow_plugins {
        command.arg("--no-plugin-dirs");
    }
    Ok(command)
}

/// Computes the remaining shared startup budget.
fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or_else(|| {
            PlaybackError::Protocol("Live-from-start audio startup timed out".to_owned())
        })
}

/// Reports cancellation without exposing an extractor diagnostic.
fn cancelled() -> PlaybackError {
    PlaybackError::Protocol("Live-from-start playback was cancelled".to_owned())
}

/// Draws a private capability independently of observable temporary filenames.
fn capability_token() -> Result<String> {
    use std::fmt::Write as _;
    let mut random = [0_u8; 32];
    getrandom::fill(&mut random)
        .map_err(|_| io::Error::other("Secure live audio capability randomness is unavailable"))?;
    let mut token = String::with_capacity(64);
    for byte in random {
        write!(&mut token, "{byte:02x}").expect("formatting into a String cannot fail");
    }
    Ok(token)
}

/// Serves one audio reader; HEAD and invalid requests never consume its bytes.
fn serve(
    listener: TcpListener,
    route: &str,
    content_type: &str,
    first: Vec<u8>,
    receiver: Receiver<BodyEvent>,
    state: &TransportState,
) {
    for incoming in listener.incoming() {
        if state.stopped.load(Ordering::Acquire) {
            break;
        }
        let Ok(mut socket) = incoming else {
            state.fail("Live-from-start audio listener failed");
            break;
        };
        let clone = socket.try_clone().ok();
        *state
            .active_socket
            .lock()
            .expect("live transport socket lock") = clone;
        let _ = socket.set_write_timeout(Some(SOCKET_WAIT));
        if state.stopped.load(Ordering::Acquire) {
            break;
        }
        let headers = read_headers(&mut socket, state);
        let (status, head) = match headers.as_deref() {
            Some(headers) => request_status(headers, route),
            None => ("400 Bad Request", true),
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nCache-Control: no-store\r\nAccept-Ranges: none\r\nConnection: close\r\n{}\r\n",
            if head { "Content-Length: 0\r\n" } else { "" }
        );
        let response_written = write_bytes(&mut socket, response.as_bytes(), state);
        if response_written.is_err() && !head {
            state.fail("Live-from-start audio connection failed");
            break;
        }
        if response_written.is_ok() && !head {
            if write_bytes(&mut socket, &first, state).is_ok() {
                while !state.stopped.load(Ordering::Acquire) {
                    match receiver.recv() {
                        Ok(BodyEvent::Bytes(bytes)) => {
                            if write_bytes(&mut socket, &bytes, state).is_err() {
                                state.fail("Live-from-start audio connection failed");
                                break;
                            }
                        }
                        Ok(BodyEvent::Failed) => {
                            state.fail("Live-from-start audio read failed");
                            break;
                        }
                        _ => break,
                    }
                }
            } else {
                state.fail("Live-from-start audio connection failed");
            }
            let _ = socket.shutdown(Shutdown::Both);
            break;
        }
        let _ = socket.shutdown(Shutdown::Both);
        state
            .active_socket
            .lock()
            .expect("live transport socket lock")
            .take();
    }
}

/// Limits local request headers and applies an absolute slow-client deadline.
fn read_headers(socket: &mut TcpStream, state: &TransportState) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    while bytes.len() < MAX_HEADERS
        && Instant::now() < deadline
        && !state.stopped.load(Ordering::Acquire)
    {
        let _ = socket.set_read_timeout(Some(SOCKET_WAIT));
        match socket.read(&mut buffer) {
            Ok(0) => return None,
            Ok(count) => {
                bytes.extend_from_slice(&buffer[..count]);
                if bytes.len() > MAX_HEADERS {
                    return None;
                }
                if bytes.ends_with(b"\r\n\r\n") {
                    return String::from_utf8(bytes).ok();
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return None,
        }
    }
    None
}

/// Requires the private route and forbids a nonzero seek into a streaming body.
fn request_status(headers: &str, route: &str) -> (&'static str, bool) {
    let mut lines = headers.split("\r\n");
    let parts: Vec<_> = lines
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    if parts.len() != 3 || parts[1] != route {
        return ("404 Not Found", true);
    }
    if !matches!(parts[0], "GET" | "HEAD") {
        return ("405 Method Not Allowed", true);
    }
    if lines.any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("range") && value.trim() != "bytes=0-"
        })
    }) {
        return ("416 Range Not Satisfiable", true);
    }
    ("200 OK", parts[0] == "HEAD")
}

/// Writes with bounded system waits; stop shuts the same socket to wake a pause.
fn write_bytes(socket: &mut TcpStream, mut bytes: &[u8], state: &TransportState) -> io::Result<()> {
    while !bytes.is_empty() && !state.stopped.load(Ordering::Acquire) {
        match socket.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(count) => bytes = &bytes[count..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use serde_json::{Value, json};
    use url::Url;

    use super::*;

    /// Probe argument that exits before any mock metadata or audio behavior runs.
    const MOCK_READY_ARGUMENT: &str = "__youta_live_fixture_ready__";

    /// Checks fixture readiness, retrying only a busy inode within the caller's budget.
    fn ensure_mock_helper_ready(
        mut probe: impl FnMut() -> io::Result<ExitStatus>,
        mut retry_busy: impl FnMut() -> bool,
    ) -> io::Result<()> {
        loop {
            match probe() {
                Ok(status) if status.success() => return Ok(()),
                Ok(status) => {
                    return Err(io::Error::other(format!(
                        "mock helper readiness exited with {status}"
                    )));
                }
                Err(error) if error.kind() == io::ErrorKind::ExecutableFileBusy && retry_busy() => {
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Publishes a closed fresh inode and proves readiness without running its body.
    fn install_mock_helper(executable: &std::path::Path, body: &str) {
        let mut staged = tempfile::Builder::new()
            .prefix(".mock-yt-dlp-")
            .tempfile_in(executable.parent().expect("mock helper directory"))
            .expect("stage mock extractor");
        staged
            .write_all(
                format!(
                    "#!/bin/sh\nif [ \"${{1-}}\" = '{MOCK_READY_ARGUMENT}' ]; then exit 0; fi\n{body}\n"
                )
                .as_bytes(),
            )
            .expect("write mock extractor");
        staged.as_file().sync_all().expect("sync mock extractor");
        staged
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o700))
            .expect("executable helper");
        // Close the writer before exposing the replacement pathname. Existing
        // writable handles keep only the old inode, not this executable.
        let staged = staged.into_temp_path();
        fs::rename(&staged, executable).expect("publish closed mock extractor");

        let started = Instant::now();
        ensure_mock_helper_ready(
            || {
                Command::new(executable)
                    .arg(MOCK_READY_ARGUMENT)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
            },
            || {
                // Parallel instrumentation may briefly retain another writable
                // handle. Wait only after that exact spawn error, not for any
                // production helper failure or fixture-body execution.
                let remaining = Duration::from_secs(1).saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return false;
                }
                thread::sleep(remaining.min(Duration::from_millis(1)));
                true
            },
        )
        .expect("mock extractor must become executable");
    }

    /// Owns a deterministic extractor fixture without contacting any service.
    struct MockHelper {
        _directory: tempfile::TempDir,
        config: LiveFromStartConfig,
    }

    impl MockHelper {
        /// Separates compact metadata output from the binary streaming invocation.
        fn new(metadata: Value, body: &str) -> Self {
            let directory = tempfile::tempdir().expect("mock helper directory");
            let executable = directory.path().join("mock-yt-dlp");
            let body = format!(
                "case \" $* \" in\n*' --skip-download '*) printf '%s\\n' '{}' ;;\n*) {} ;;\nesac",
                metadata, body
            );
            install_mock_helper(&executable, &body);
            Self {
                _directory: directory,
                config: LiveFromStartConfig {
                    executable,
                    startup_timeout: Duration::from_secs(2),
                    allow_plugins: false,
                },
            }
        }
    }

    /// Republishing a helper must not rewrite an inode still held for writing.
    #[cfg(target_os = "linux")]
    #[test]
    fn mock_helper_publication_replaces_a_busy_inode_without_rewriting_it() {
        let directory = tempfile::tempdir().expect("mock publication directory");
        let executable = directory.path().join("mock-yt-dlp");
        install_mock_helper(&executable, "printf 'original-fixture'");
        let original = fs::read(&executable).expect("original helper contents");
        let mut writer = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&executable)
            .expect("hold original executable open for writing");
        let busy = Command::new(&executable)
            .output()
            .expect_err("an open executable writer must prevent Linux execution");
        assert_eq!(busy.kind(), io::ErrorKind::ExecutableFileBusy);

        install_mock_helper(&executable, "printf 'replacement-fixture'");
        let output = Command::new(&executable)
            .output()
            .expect("replacement helper must not retain the busy inode");
        assert!(output.status.success(), "replacement helper: {output:?}");
        assert_eq!(output.stdout, b"replacement-fixture");
        let mut retained = Vec::new();
        writer
            .read_to_end(&mut retained)
            .expect("retained original inode");
        assert_eq!(
            retained, original,
            "publication must not modify the old inode"
        );
    }

    /// A readiness probe must not consume metadata or execute streaming behavior.
    #[test]
    fn mock_helper_readiness_skips_body_and_preserves_normal_invocation() {
        let directory = tempfile::tempdir().expect("mock readiness directory");
        let executable = directory.path().join("mock-yt-dlp");
        let marker = executable.with_extension("body-ran");
        install_mock_helper(
            &executable,
            "printf 'fixture body' > \"$0.body-ran\"; exit 7",
        );
        assert!(
            !marker.exists(),
            "publication readiness must not run the body"
        );
        let ready = Command::new(&executable)
            .arg(MOCK_READY_ARGUMENT)
            .output()
            .expect("readiness invocation");
        assert!(ready.status.success());
        assert!(ready.stdout.is_empty());
        assert!(ready.stderr.is_empty());
        assert!(!marker.exists(), "explicit readiness must not run the body");

        let output = Command::new(&executable)
            .arg("--ordinary-fixture-argument")
            .output()
            .expect("ordinary fixture invocation");
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(
            fs::read(marker).expect("fixture body marker"),
            b"fixture body"
        );
    }

    /// Only transient executable-busy errors consume the fixture retry budget.
    #[test]
    fn mock_helper_readiness_retries_executable_busy_until_success() {
        let mut probes = 0;
        let mut retries = 0;
        ensure_mock_helper_ready(
            || {
                probes += 1;
                assert!(
                    probes <= 3,
                    "readiness must stop after successful execution"
                );
                if probes <= 2 {
                    Err(io::ErrorKind::ExecutableFileBusy.into())
                } else {
                    Ok(ExitStatus::from_raw(0))
                }
            },
            || {
                retries += 1;
                true
            },
        )
        .expect("busy fixture becomes ready");
        assert_eq!(probes, 3);
        assert_eq!(retries, 2);
    }

    /// A finite budget cannot spin indefinitely on a permanently busy fixture.
    #[test]
    fn mock_helper_readiness_stops_when_busy_retry_budget_is_exhausted() {
        let mut probes = 0;
        let mut retries_left = 3;
        let error = ensure_mock_helper_ready(
            || {
                probes += 1;
                assert!(probes <= 4, "busy exhaustion must stop additional probes");
                Err(io::ErrorKind::ExecutableFileBusy.into())
            },
            || {
                if retries_left == 0 {
                    return false;
                }
                retries_left -= 1;
                true
            },
        )
        .expect_err("permanently busy fixture exhausts its budget");
        assert_eq!(error.kind(), io::ErrorKind::ExecutableFileBusy);
        assert_eq!(probes, 4);
        assert_eq!(retries_left, 0);
    }

    /// Missing or inaccessible fixtures fail immediately without invoking retries.
    #[test]
    fn mock_helper_readiness_does_not_retry_other_spawn_errors() {
        for kind in [io::ErrorKind::NotFound, io::ErrorKind::PermissionDenied] {
            let mut probes = 0;
            let error = ensure_mock_helper_ready(
                || {
                    probes += 1;
                    assert_eq!(probes, 1, "unrelated spawn errors must not be retried");
                    Err(kind.into())
                },
                || panic!("only executable-busy errors may use the retry budget"),
            )
            .expect_err("unrelated fixture spawn failure");
            assert_eq!(error.kind(), kind);
            assert_eq!(probes, 1);
        }
    }

    /// A running probe's nonzero exit is a fixture error, not a retryable spawn race.
    #[test]
    fn mock_helper_readiness_rejects_nonzero_exit_without_retrying() {
        let mut probes = 0;
        let error = ensure_mock_helper_ready(
            || {
                probes += 1;
                assert_eq!(probes, 1, "nonzero readiness exits must not be retried");
                Ok(ExitStatus::from_raw(7 << 8))
            },
            || panic!("nonzero readiness exits must not use the retry budget"),
        )
        .expect_err("failed readiness fixture");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("7"));
        assert_eq!(probes, 1);
    }

    /// Supplies a recent, fully available generator format for startup tests.
    fn metadata() -> Value {
        json!({
            "id": "dQw4w9WgXcQ",
            "live_status": "is_live",
            "release_timestamp": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() - 60,
            "protocol": "http_dash_segments_generator",
            "acodec": "mp4a.40.2",
            "vcodec": "none",
            "ext": "m4a",
            "is_from_start": true,
            "first_fragment_url": "NA",
        })
    }

    /// Uses the canonical URL required by production transport requests.
    fn source() -> Url {
        Url::parse("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap()
    }

    /// Reads one HTTP response while bounding a broken fixture's wait.
    fn request(url: &Url, method: &str, path: &str, extra: &str) -> Vec<u8> {
        let mut client = TcpStream::connect(("127.0.0.1", url.port().unwrap())).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write!(
            client,
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{extra}\r\n"
        )
        .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).unwrap();
        response
    }

    /// Missing lazy fragment URLs must retain a valid JSON null placeholder.
    #[test]
    fn ongoing_generator_metadata_allows_an_absent_concrete_fragment_url() {
        let mut data = metadata();
        data["first_fragment_url"] = Value::Null;
        assert!(validate_metadata(&serde_json::to_vec(&data).unwrap(), "dQw4w9WgXcQ").is_ok());
        assert!(METADATA_TEMPLATE.contains("%(fragments.0.url|null)j"));
    }

    /// Metadata alone must not replace the current player before audio arrives.
    #[test]
    fn launch_waits_for_initial_audio_and_serves_original_bytes() {
        let fixture = MockHelper::new(metadata(), "printf 'opening-audio-bytes'");
        let stream = LiveFromStartStream::prepare(
            &fixture.config,
            &source(),
            &YouTubePrewarmCancellation::default(),
        )
        .expect("ready stream");
        assert_eq!(stream.url().path().len(), 65);
        assert!(!stream.url().path().contains("youta-live-start-"));
        let response = request(
            stream.url(),
            "GET",
            stream.url().path(),
            "Range: bytes=0-\r\n",
        );
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(response.ends_with(b"opening-audio-bytes"));
        assert_eq!(stream.error(), None);
    }

    /// Unavailable beginnings must never fall back to the current live edge.
    #[test]
    fn missing_old_or_partial_start_metadata_is_rejected() {
        for invalid in ["missing", "old", "recent", "mismatch", "partial"] {
            let mut data = metadata();
            match invalid {
                "missing" => data["release_timestamp"] = Value::Null,
                "old" => data["release_timestamp"] = json!(1),
                "recent" => data["is_from_start"] = json!(false),
                "mismatch" => data["id"] = json!("aaaaaaaaaaa"),
                _ => {
                    data["live_status"] = json!("post_live");
                    data["protocol"] = json!("http_dash_segments");
                    data["first_fragment_url"] =
                        json!("https://media.example/fragment?sq=17&sig=private");
                }
            }
            let fixture = MockHelper::new(data, "printf 'wrong-start-audio'");
            let error = LiveFromStartStream::prepare(
                &fixture.config,
                &source(),
                &YouTubePrewarmCancellation::default(),
            )
            .err()
            .expect("unsupported beginning")
            .to_string();
            assert!(error.contains("beginning"), "{invalid}: {error}");
            assert!(!error.contains("sig=private"));
        }
    }

    /// Processing broadcasts retain their concrete sequence-zero audio route.
    #[test]
    fn post_live_concrete_sequence_zero_is_supported() {
        let mut data = metadata();
        data["live_status"] = json!("post_live");
        data["protocol"] = json!("http_dash_segments");
        data["is_from_start"] = Value::Null;
        data["first_fragment_url"] = json!("https://media.example/fragment?sq=0&sig=private");
        let fixture = MockHelper::new(data, "printf 'original-first-fragment'");
        let stream = LiveFromStartStream::prepare(
            &fixture.config,
            &source(),
            &YouTubePrewarmCancellation::default(),
        )
        .expect("post-live beginning");
        assert!(
            request(stream.url(), "GET", stream.url().path(), "")
                .ends_with(b"original-first-fragment")
        );
    }

    /// Neither a guessed capability nor a later byte offset may consume audio.
    #[test]
    fn capability_and_range_validation_preserve_the_first_audio_bytes() {
        let fixture = MockHelper::new(metadata(), "printf 'first-byte-preserved'");
        let stream = LiveFromStartStream::prepare(
            &fixture.config,
            &source(),
            &YouTubePrewarmCancellation::default(),
        )
        .unwrap();
        assert!(request(stream.url(), "GET", "/guessed", "").starts_with(b"HTTP/1.1 404"));
        assert!(
            request(stream.url(), "HEAD", stream.url().path(), "").starts_with(b"HTTP/1.1 200")
        );
        assert!(
            request(
                stream.url(),
                "GET",
                stream.url().path(),
                "Range: bytes=10-\r\n"
            )
            .starts_with(b"HTTP/1.1 416")
        );
        assert!(
            request(stream.url(), "GET", stream.url().path(), "")
                .ends_with(b"first-byte-preserved")
        );
    }

    /// A fragment failure before any bytes arrive is an actionable startup error.
    #[test]
    fn early_fragment_failure_is_sanitized_and_never_returns_a_stream() {
        let fixture = MockHelper::new(
            metadata(),
            "printf 'ERROR: https://media.example/?sig=private\\n' >&2; exit 7",
        );
        let error = LiveFromStartStream::prepare(
            &fixture.config,
            &source(),
            &YouTubePrewarmCancellation::default(),
        )
        .err()
        .expect("failed initial fragment")
        .to_string();
        assert!(!error.contains("sig=private"));
        assert!(error.contains("audio") || error.contains("beginning"));
    }

    /// Metadata and audio startup both have finite cancellation-aware deadlines.
    #[test]
    fn missing_first_audio_times_out_and_reaps_the_helper() {
        let mut fixture = MockHelper::new(metadata(), "sleep 60");
        fixture.config.startup_timeout = Duration::from_millis(150);
        let started = Instant::now();
        let error = LiveFromStartStream::prepare(
            &fixture.config,
            &source(),
            &YouTubePrewarmCancellation::default(),
        )
        .err()
        .expect("startup timeout")
        .to_string();
        assert!(error.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// A metadata helper cannot hold a worker past its requested startup deadline.
    #[test]
    fn unresponsive_metadata_times_out_without_launching_audio() {
        let mut fixture = MockHelper::new(metadata(), "printf 'unused-audio'");
        install_mock_helper(&fixture.config.executable, "sleep 60");
        fixture.config.startup_timeout = Duration::from_millis(150);
        let started = Instant::now();
        assert!(
            LiveFromStartStream::prepare(
                &fixture.config,
                &source(),
                &YouTubePrewarmCancellation::default(),
            )
            .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// Superseding a pending start interrupts a helper that has produced no audio.
    #[test]
    fn cancellation_interrupts_initial_audio_wait() {
        let fixture = MockHelper::new(metadata(), "sleep 60");
        let cancellation = YouTubePrewarmCancellation::default();
        let worker_cancellation = cancellation.clone();
        let config = fixture.config.clone();
        let worker = std::thread::spawn(move || {
            LiveFromStartStream::prepare(&config, &source(), &worker_cancellation)
        });
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        cancellation.cancel();
        assert!(worker.join().unwrap().is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// A full audio queue and an idle HTTP listener must both unblock on stop.
    #[test]
    fn stop_releases_a_full_audio_queue_and_helper_descendants() {
        let fixture = MockHelper::new(metadata(), "yes opening-audio-bytes");
        let mut stream = LiveFromStartStream::prepare(
            &fixture.config,
            &source(),
            &YouTubePrewarmCancellation::default(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        stream.stop();
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// Stopping a stream wakes its idle listener and blocking helper readers.
    #[test]
    fn stop_without_an_http_client_is_bounded_and_removes_private_artifacts() {
        let fixture = MockHelper::new(metadata(), "printf 'first-fragment'; sleep 60");
        let mut stream = LiveFromStartStream::prepare(
            &fixture.config,
            &source(),
            &YouTubePrewarmCancellation::default(),
        )
        .unwrap();
        let directory = stream.directory.as_ref().unwrap().path().to_owned();
        let started = Instant::now();
        stream.stop();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!directory.exists());
        stream.stop();
    }

    /// Downstream callers receive sanitized failures after successful startup too.
    #[test]
    fn failure_after_initial_audio_is_observable_without_signed_urls() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::task::Wake;

        struct Counter(AtomicUsize);
        impl Wake for Counter {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let fixture = MockHelper::new(
            metadata(),
            "printf 'first-fragment'; sleep 0.1; printf 'ERROR: https://media.example/?sig=private\\n' >&2; exit 7",
        );
        let mut stream = LiveFromStartStream::prepare(
            &fixture.config,
            &source(),
            &YouTubePrewarmCancellation::default(),
        )
        .unwrap();
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        stream.set_worker_waker(Some(Waker::from(Arc::clone(&counter))));
        let deadline = Instant::now() + Duration::from_secs(2);
        while stream.error().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let error = stream.take_error().expect("download failure");
        assert!(!error.contains("sig=private"));
        assert_eq!(stream.take_error(), None);
        assert!(
            counter.0.load(Ordering::SeqCst) >= 2,
            "attach and asynchronous failure must wake the frontend"
        );
    }
}
