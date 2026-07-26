//! Bounded `YouTube Music` search through the external `yt-dlp` executable.
//!
//! `YouTube Music` does not require a `Google API` key for this adapter. `yt-dlp`
//! opens the public `YouTube Music` search page and recursively resolves its
//! browse containers. A JSON-safe print template emits only video-stage leaf
//! entries, so callers receive playable identifiers rather than albums.
//!
//! Search is deliberately synchronous, matching Youta's provider interfaces.
//! Call it from the provider worker rather than the terminal event loop. The
//! child process is supervised with a whole-operation timeout while stdout and
//! stderr are drained concurrently into fixed-size buffers.

use std::collections::BTreeSet;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;
use url::Url;

const SEARCH_URL: &str = "https://music.youtube.com/search";
const MAX_QUERY_BYTES: usize = 512;
const MAX_RESULTS: usize = 100;
const SCAN_MULTIPLIER: usize = 4;
const MAX_SCAN_ENTRIES: usize = MAX_RESULTS * SCAN_MULTIPLIER;
const MAX_TITLE_BYTES: usize = 1_024;
const MAX_ARTIST_BYTES: usize = 512;
const MAX_STDERR_BYTES: usize = 8 * 1_024;
const OUTPUT_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_CONFIGURED_TIMEOUT: Duration = Duration::from_mins(5);
const MAX_CONFIGURED_JSON_BYTES: usize = 16 * 1024 * 1024;
const PRINT_PREFIX: &str = "youta-music";
const PRINT_TEMPLATE: &str = concat!(
    "youta-music\t%(id)j\t%(title)j\t%(channel)j\t%(uploader)j\t",
    "%(duration)j\t%(thumbnail)j\t%(extractor)j\t%(extractor_key)j\t%(live_status)j"
);

/// Process and resource settings for `YouTube Music` search.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct YouTubeMusicSearchConfig {
    /// Executable path or name.
    pub executable: PathBuf,
    /// Whole-operation timeout, including process startup and extraction.
    pub timeout: Duration,
    /// Maximum JSON bytes retained from `yt-dlp` stdout.
    pub max_json_bytes: usize,
    /// Whether user-installed `yt-dlp` plugins may be loaded.
    pub allow_plugins: bool,
}

impl Default for YouTubeMusicSearchConfig {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("yt-dlp"),
            timeout: Duration::from_secs(20),
            max_json_bytes: 4 * 1024 * 1024,
            allow_plugins: false,
        }
    }
}

/// One playable track-level result from `YouTube Music`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct YouTubeMusicTrack {
    /// Eleven-character `YouTube` video identifier.
    pub video_id: String,
    /// Track or video title returned by `YouTube Music`.
    pub title: String,
    /// Artist or channel when extracted metadata exposes it.
    pub artist: Option<String>,
    /// Duration rounded to whole seconds when known.
    pub duration_seconds: Option<u64>,
    /// Canonical `YouTube Music` watch URL suitable for `yt-dlp` playback.
    pub webpage_url: Url,
    /// Search thumbnail URL, with a deterministic `YouTube` fallback.
    pub thumbnail_url: Url,
}

/// Failure while validating, running, or parsing `YouTube Music` search.
#[derive(Debug, Error)]
pub enum YouTubeMusicSearchError {
    /// Search input or adapter configuration is outside its documented bounds.
    #[error("invalid YouTube Music search request: {0}")]
    InvalidRequest(String),
    /// The configured executable does not exist.
    #[error("yt-dlp is unavailable at `{0}`")]
    ExecutableUnavailable(String),
    /// The supervised process exceeded the configured whole-operation timeout.
    #[error("YouTube Music search timed out after {0:?}")]
    TimedOut(Duration),
    /// The process returned an unsuccessful status.
    #[error("yt-dlp YouTube Music search exited with {status}{detail}")]
    ProcessExited {
        /// Operating-system exit status.
        status: ExitStatus,
        /// Bounded, single-line diagnostic suffix.
        detail: String,
    },
    /// Machine-readable stdout exceeded its configured memory bound.
    #[error("yt-dlp YouTube Music JSON exceeded the {limit}-byte limit")]
    OutputTooLarge {
        /// Configured JSON byte limit.
        limit: usize,
    },
    /// Machine-readable output did not match the expected tagged-field shape.
    #[error("invalid yt-dlp YouTube Music output: {0}")]
    InvalidOutput(String),
    /// A child-process or pipe operation failed.
    #[error("YouTube Music process I/O failed: {0}")]
    Io(#[from] io::Error),
}

/// Lightweight process adapter for key-free `YouTube Music` search.
#[derive(Clone, Debug)]
pub struct YouTubeMusicSearch {
    config: YouTubeMusicSearchConfig,
}

impl YouTubeMusicSearch {
    /// Creates an adapter with explicit process and resource settings.
    #[must_use]
    pub const fn new(config: YouTubeMusicSearchConfig) -> Self {
        Self { config }
    }

    /// Searches `YouTube Music` and returns at most `max_results` playable tracks.
    ///
    /// `YouTube Music` frequently returns album and artist browse containers
    /// before individual tracks. `yt-dlp` resolves those containers, while a
    /// video-stage print template emits only playable leaf entries. The child
    /// stops after `max_results` videos and also has independent time and output
    /// byte bounds.
    ///
    /// This method blocks its calling thread while the supervised child runs.
    /// Youta's provider worker should call it so the TUI remains responsive.
    ///
    /// # Errors
    ///
    /// Returns [`YouTubeMusicSearchError`] for invalid input or limits, process
    /// startup/timeout/failure, oversized output, or malformed JSON.
    pub fn search(
        &self,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<YouTubeMusicTrack>, YouTubeMusicSearchError> {
        let query = validate_search_request(query, max_results, &self.config)?;
        let scan_entries = max_results
            .saturating_mul(SCAN_MULTIPLIER)
            .min(MAX_SCAN_ENTRIES);
        let mut command = build_search_command(&self.config, query, max_results, scan_entries)?;
        let output = run_bounded_command(
            &mut command,
            self.config.timeout,
            self.config.max_json_bytes,
            MAX_STDERR_BYTES,
        )
        .map_err(|error| match error {
            RunCommandError::ExecutableUnavailable => {
                YouTubeMusicSearchError::ExecutableUnavailable(
                    self.config.executable.display().to_string(),
                )
            }
            RunCommandError::TimedOut => YouTubeMusicSearchError::TimedOut(self.config.timeout),
            RunCommandError::Io(error) => YouTubeMusicSearchError::Io(error),
        })?;

        if output.stdout.truncated {
            return Err(YouTubeMusicSearchError::OutputTooLarge {
                limit: self.config.max_json_bytes,
            });
        }
        let tracks = parse_ytdlp_music_search(&output.stdout.bytes, max_results)?;
        if !output.status.success() && tracks.len() < max_results {
            return Err(YouTubeMusicSearchError::ProcessExited {
                status: output.status,
                detail: sanitized_process_detail(&output.stderr.bytes),
            });
        }
        Ok(tracks)
    }
}

impl Default for YouTubeMusicSearch {
    fn default() -> Self {
        Self::new(YouTubeMusicSearchConfig::default())
    }
}

/// Parses bounded line-oriented JSON fields printed by the adapter's `yt-dlp`
/// template.
///
/// Non-flat extraction recursively opens `YoutubeTab` album and artist browse
/// containers. The default `video` print stage emits only resolved leaf videos,
/// and this parser accepts only the `Youtube` extractor with valid video IDs.
/// Unknown diagnostic or future container lines are ignored.
///
/// # Errors
///
/// Returns [`YouTubeMusicSearchError::InvalidOutput`] when a tagged result line
/// contains malformed JSON fields, or when `max_results` is unsupported.
pub fn parse_ytdlp_music_search(
    output: &[u8],
    max_results: usize,
) -> Result<Vec<YouTubeMusicTrack>, YouTubeMusicSearchError> {
    if !(1..=MAX_RESULTS).contains(&max_results) {
        return Err(YouTubeMusicSearchError::InvalidRequest(format!(
            "result limit must be between 1 and {MAX_RESULTS}"
        )));
    }
    let mut seen_ids = BTreeSet::new();
    let mut tracks = Vec::with_capacity(max_results);
    for line in output.split(|byte| *byte == b'\n') {
        if tracks.len() == max_results {
            break;
        }
        let Some(entry) = parse_printed_entry(line)? else {
            continue;
        };
        let Some(track) = track_from_printed_entry(entry) else {
            continue;
        };
        if seen_ids.insert(track.video_id.clone()) {
            tracks.push(track);
        }
    }
    Ok(tracks)
}

struct PrintedEntry {
    id: Option<String>,
    title: Option<String>,
    channel: Option<String>,
    uploader: Option<String>,
    duration: Option<f64>,
    thumbnail: Option<String>,
    extractor: Option<String>,
    extractor_key: Option<String>,
    live_status: Option<String>,
}

fn parse_printed_entry(line: &[u8]) -> Result<Option<PrintedEntry>, YouTubeMusicSearchError> {
    let line = std::str::from_utf8(line)
        .map_err(|error| YouTubeMusicSearchError::InvalidOutput(error.to_string()))?;
    let mut fields = line.trim_end_matches('\r').split('\t');
    if fields.next() != Some(PRINT_PREFIX) {
        return Ok(None);
    }
    let fields = fields.collect::<Vec<_>>();
    if fields.len() != 9 {
        return Err(YouTubeMusicSearchError::InvalidOutput(format!(
            "track line had {} fields instead of 9",
            fields.len()
        )));
    }
    Ok(Some(PrintedEntry {
        id: json_optional_string(fields[0])?,
        title: json_optional_string(fields[1])?,
        channel: json_optional_string(fields[2])?,
        uploader: json_optional_string(fields[3])?,
        duration: json_optional_number(fields[4])?,
        thumbnail: json_optional_string(fields[5])?,
        extractor: json_optional_string(fields[6])?,
        extractor_key: json_optional_string(fields[7])?,
        live_status: json_optional_string(fields[8])?,
    }))
}

fn track_from_printed_entry(entry: PrintedEntry) -> Option<YouTubeMusicTrack> {
    let extractor = entry
        .extractor_key
        .as_deref()
        .or(entry.extractor.as_deref())?;
    if !extractor.eq_ignore_ascii_case("youtube")
        || entry
            .live_status
            .as_deref()
            .is_some_and(|status| status.eq_ignore_ascii_case("is_upcoming"))
    {
        return None;
    }
    let video_id = entry.id?.trim().to_owned();
    if !is_youtube_video_id(&video_id) {
        return None;
    }
    let title = bounded_display_text(entry.title.as_deref()?, MAX_TITLE_BYTES)?;
    let artist = entry
        .channel
        .as_deref()
        .or(entry.uploader.as_deref())
        .and_then(|value| bounded_display_text(value, MAX_ARTIST_BYTES));
    let duration_seconds = entry.duration.and_then(rounded_nonnegative_seconds);
    let webpage_url = youtube_music_watch_url(&video_id)?;
    let thumbnail_url = entry
        .thumbnail
        .as_deref()
        .and_then(secure_remote_url)
        .or_else(|| youtube_thumbnail_url(&video_id))?;
    Some(YouTubeMusicTrack {
        video_id,
        title,
        artist,
        duration_seconds,
        webpage_url,
        thumbnail_url,
    })
}

fn validate_search_request<'a>(
    query: &'a str,
    max_results: usize,
    config: &YouTubeMusicSearchConfig,
) -> Result<&'a str, YouTubeMusicSearchError> {
    let query = query.trim();
    if query.is_empty() {
        return Err(YouTubeMusicSearchError::InvalidRequest(
            "query cannot be empty".to_owned(),
        ));
    }
    if query.len() > MAX_QUERY_BYTES {
        return Err(YouTubeMusicSearchError::InvalidRequest(format!(
            "query cannot exceed {MAX_QUERY_BYTES} bytes"
        )));
    }
    if !(1..=MAX_RESULTS).contains(&max_results) {
        return Err(YouTubeMusicSearchError::InvalidRequest(format!(
            "result limit must be between 1 and {MAX_RESULTS}"
        )));
    }
    if config.timeout.is_zero() || config.timeout > MAX_CONFIGURED_TIMEOUT {
        return Err(YouTubeMusicSearchError::InvalidRequest(
            "process timeout must be greater than zero and at most five minutes".to_owned(),
        ));
    }
    if config.max_json_bytes == 0 || config.max_json_bytes > MAX_CONFIGURED_JSON_BYTES {
        return Err(YouTubeMusicSearchError::InvalidRequest(format!(
            "JSON byte limit must be between 1 and {MAX_CONFIGURED_JSON_BYTES}"
        )));
    }
    Ok(query)
}

fn build_search_command(
    config: &YouTubeMusicSearchConfig,
    query: &str,
    max_results: usize,
    scan_entries: usize,
) -> Result<Command, YouTubeMusicSearchError> {
    let mut search_url = Url::parse(SEARCH_URL)
        .map_err(|error| YouTubeMusicSearchError::InvalidOutput(error.to_string()))?;
    search_url.query_pairs_mut().append_pair("q", query);

    let socket_timeout = config.timeout.as_secs().max(1);
    let mut command = Command::new(&config.executable);
    command.arg("--ignore-config");
    if !config.allow_plugins {
        command.arg("--no-plugin-dirs");
    }
    command
        .arg("--no-flat-playlist")
        .arg("--skip-download")
        .arg("--no-warnings")
        .arg("--output-na-placeholder")
        .arg("null")
        .arg("--print")
        .arg(PRINT_TEMPLATE)
        .arg("--playlist-end")
        .arg(scan_entries.to_string())
        .arg("--max-downloads")
        .arg(max_results.to_string())
        .arg("--socket-timeout")
        .arg(socket_timeout.to_string())
        .arg("--retries")
        .arg("1")
        .arg("--extractor-retries")
        .arg("1")
        .arg("--")
        .arg(search_url.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command)
}

struct BoundedCommandOutput {
    status: ExitStatus,
    stdout: BoundedCapture,
    stderr: BoundedCapture,
}

#[derive(Debug, Eq, PartialEq)]
struct BoundedCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

#[derive(Debug)]
enum RunCommandError {
    ExecutableUnavailable,
    TimedOut,
    Io(io::Error),
}

fn run_bounded_command(
    command: &mut Command,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<BoundedCommandOutput, RunCommandError> {
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            RunCommandError::ExecutableUnavailable
        } else {
            RunCommandError::Io(error)
        }
    })?;
    let Some(stdout) = child.stdout.take() else {
        terminate_child(&mut child);
        return Err(RunCommandError::Io(io::Error::other(
            "yt-dlp stdout pipe was not created",
        )));
    };
    let Some(stderr) = child.stderr.take() else {
        terminate_child(&mut child);
        return Err(RunCommandError::Io(io::Error::other(
            "yt-dlp stderr pipe was not created",
        )));
    };
    let stdout_receiver = capture_bounded(stdout, stdout_limit);
    let stderr_receiver = capture_bounded(stderr, stderr_limit);
    let status = wait_for_child(&mut child, timeout)?;
    let stdout = receive_capture(&stdout_receiver)?;
    let stderr = receive_capture(&stderr_receiver)?;
    Ok(BoundedCommandOutput {
        status,
        stdout,
        stderr,
    })
}

fn wait_for_child(child: &mut Child, timeout: Duration) -> Result<ExitStatus, RunCommandError> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| RunCommandError::Io(io::Error::other("yt-dlp timeout overflowed")))?;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                terminate_child(child);
                return Err(RunCommandError::TimedOut);
            }
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                thread::sleep(remaining.min(PROCESS_POLL_INTERVAL));
            }
            Err(error) => {
                terminate_child(child);
                return Err(RunCommandError::Io(error));
            }
        }
    }
}

fn terminate_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
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
        let result = loop {
            match reader.read(&mut buffer) {
                Ok(0) => break Ok(BoundedCapture { bytes, truncated }),
                Ok(read) => {
                    let remaining = limit.saturating_sub(bytes.len());
                    let retained = read.min(remaining);
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

fn receive_capture(
    receiver: &mpsc::Receiver<io::Result<BoundedCapture>>,
) -> Result<BoundedCapture, RunCommandError> {
    match receiver.recv_timeout(OUTPUT_CLOSE_TIMEOUT) {
        Ok(Ok(capture)) => Ok(capture),
        Ok(Err(error)) => Err(RunCommandError::Io(error)),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(RunCommandError::Io(io::Error::new(
            io::ErrorKind::TimedOut,
            "yt-dlp output pipe did not close",
        ))),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(RunCommandError::Io(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "yt-dlp output reader stopped unexpectedly",
        ))),
    }
}

fn is_youtube_video_id(value: &str) -> bool {
    value.len() == 11
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn youtube_music_watch_url(video_id: &str) -> Option<Url> {
    let mut url = Url::parse("https://music.youtube.com/watch").ok()?;
    url.query_pairs_mut().append_pair("v", video_id);
    Some(url)
}

fn youtube_thumbnail_url(video_id: &str) -> Option<Url> {
    Url::parse(&format!("https://i.ytimg.com/vi/{video_id}/hqdefault.jpg")).ok()
}

fn secure_remote_url(candidate: &str) -> Option<Url> {
    Url::parse(candidate).ok().filter(|url| {
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
    })
}

fn json_optional_string(value: &str) -> Result<Option<String>, YouTubeMusicSearchError> {
    let value: serde_json::Value = serde_json::from_str(value)
        .map_err(|error| YouTubeMusicSearchError::InvalidOutput(error.to_string()))?;
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(value) if matches!(value.as_str(), "NA" | "null") => Ok(None),
        serde_json::Value::String(value) => Ok(Some(value)),
        _ => Err(YouTubeMusicSearchError::InvalidOutput(
            "yt-dlp printed a non-string metadata field".to_owned(),
        )),
    }
}

fn json_optional_number(value: &str) -> Result<Option<f64>, YouTubeMusicSearchError> {
    let value: serde_json::Value = serde_json::from_str(value)
        .map_err(|error| YouTubeMusicSearchError::InvalidOutput(error.to_string()))?;
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(value) => Ok(value.as_f64()),
        serde_json::Value::String(value) if matches!(value.as_str(), "NA" | "null") => Ok(None),
        _ => Err(YouTubeMusicSearchError::InvalidOutput(
            "yt-dlp printed a non-numeric duration".to_owned(),
        )),
    }
}

fn bounded_display_text(value: &str, max_bytes: usize) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let mut output = String::with_capacity(value.len().min(max_bytes));
    for character in value.chars().filter(|character| !character.is_control()) {
        if output.len().saturating_add(character.len_utf8()) > max_bytes {
            break;
        }
        output.push(character);
    }
    (!output.is_empty()).then_some(output)
}

fn rounded_nonnegative_seconds(value: f64) -> Option<u64> {
    let duration = Duration::try_from_secs_f64(value).ok()?;
    Some(
        duration
            .as_secs()
            .saturating_add(u64::from(duration.subsec_nanos() >= 500_000_000)),
    )
}

fn sanitized_process_detail(stderr: &[u8]) -> String {
    let line = String::from_utf8_lossy(stderr)
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .map(|word| {
            if word.contains("http://") || word.contains("https://") {
                "<redacted-url>"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if line.is_empty() {
        String::new()
    } else {
        format!(": {line}")
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::io::Cursor;

    use super::*;

    fn fixture() -> &'static [u8] {
        concat!(
            // A broad flat search may contain only these browse containers.
            // Non-flat extraction resolves them before the following video
            // print-stage lines are emitted.
            r#"{"_type":"url","ie_key":"YoutubeTab","id":"MPREb_album","url":"https://music.youtube.com/browse/MPREb_album"}"#,
            "\n",
            "youta-music\t\"Tb0MC0jFv6M\"\t\"Teardrop (feat. Elizabeth Fraser)\"",
            "\t\"Massive Attack\"\tnull\t331.5\t\"https://img.example/large.jpg\"",
            "\t\"youtube\"\t\"Youtube\"\tnull\n",
            "youta-music\t\"3h-JYx76QNM\"\t\"Teardrop\"\tnull\t\"Massive Attack\"",
            "\tnull\tnull\t\"youtube\"\t\"Youtube\"\tnull\n",
            "youta-music\t\"Tb0MC0jFv6M\"\t\"Duplicate\"\tnull\tnull",
            "\tnull\tnull\t\"youtube\"\t\"Youtube\"\tnull\n",
            "youta-music\t\"short\"\t\"Invalid identifier\"\tnull\tnull",
            "\tnull\tnull\t\"youtube\"\t\"Youtube\"\tnull\n",
        )
        .as_bytes()
    }

    #[test]
    fn recursive_output_after_browse_containers_returns_unique_playable_tracks() {
        let tracks = parse_ytdlp_music_search(fixture(), 10).expect("parse fixture");

        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].video_id, "Tb0MC0jFv6M");
        assert_eq!(tracks[0].artist.as_deref(), Some("Massive Attack"));
        assert_eq!(tracks[0].duration_seconds, Some(332));
        assert_eq!(
            tracks[0].webpage_url.as_str(),
            "https://music.youtube.com/watch?v=Tb0MC0jFv6M"
        );
        assert_eq!(
            tracks[0].thumbnail_url.as_str(),
            "https://img.example/large.jpg"
        );
        assert_eq!(
            tracks[1].thumbnail_url.as_str(),
            "https://i.ytimg.com/vi/3h-JYx76QNM/hqdefault.jpg"
        );
    }

    #[test]
    fn parser_honors_result_limit_after_filtering_containers() {
        let tracks = parse_ytdlp_music_search(fixture(), 1).expect("parse one result");
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].video_id, "Tb0MC0jFv6M");
    }

    #[test]
    fn command_is_headless_bounded_and_uses_music_search_without_an_api_key() {
        let config = YouTubeMusicSearchConfig::default();
        let command =
            build_search_command(&config, "Björk & strings", 3, 12).expect("search command");
        let arguments = command
            .get_args()
            .map(std::ffi::OsStr::to_os_string)
            .collect::<Vec<OsString>>();
        let visible = arguments
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>();

        assert_eq!(command.get_program(), "yt-dlp");
        assert!(visible.iter().any(|argument| argument == "--ignore-config"));
        assert!(
            visible
                .iter()
                .any(|argument| argument == "--no-plugin-dirs")
        );
        assert!(
            visible
                .iter()
                .any(|argument| argument == "--no-flat-playlist")
        );
        assert!(
            visible
                .windows(2)
                .any(|pair| { pair[0] == "--print" && pair[1].starts_with("youta-music\t%(id)j") })
        );
        assert!(
            visible
                .windows(2)
                .any(|pair| pair[0] == "--playlist-end" && pair[1] == "12")
        );
        assert!(
            visible
                .windows(2)
                .any(|pair| pair[0] == "--max-downloads" && pair[1] == "3")
        );
        assert_eq!(
            visible
                .iter()
                .filter(|argument| argument.as_ref() == "--")
                .count(),
            1
        );
        assert_eq!(visible[visible.len() - 2], "--");
        assert!(
            visible
                .last()
                .is_some_and(|argument| argument.starts_with(SEARCH_URL))
        );
        assert!(visible.iter().any(|argument| {
            argument.starts_with("https://music.youtube.com/search?")
                && argument.contains("q=Bj%C3%B6rk+%26+strings")
        }));
        assert!(
            !visible
                .iter()
                .any(|argument| matches!(argument.as_ref(), "--api-key" | "--youtube-api-key"))
        );
    }

    #[test]
    fn bounded_capture_drains_but_retains_only_the_configured_prefix() {
        let receiver = capture_bounded(Cursor::new(vec![7_u8; 33]), 8);
        let capture = receive_capture(&receiver).expect("bounded capture");

        assert_eq!(capture.bytes, vec![7_u8; 8]);
        assert!(capture.truncated);
    }

    #[test]
    fn request_validation_rejects_empty_queries_and_unbounded_limits() {
        let config = YouTubeMusicSearchConfig::default();
        assert!(validate_search_request(" ", 1, &config).is_err());
        assert!(validate_search_request("music", 0, &config).is_err());
        assert!(validate_search_request("music", MAX_RESULTS + 1, &config).is_err());
        assert!(validate_search_request(&"q".repeat(MAX_QUERY_BYTES + 1), 1, &config).is_err());
        assert!(
            validate_search_request(
                "music",
                1,
                &YouTubeMusicSearchConfig {
                    timeout: MAX_CONFIGURED_TIMEOUT + Duration::from_secs(1),
                    ..config.clone()
                }
            )
            .is_err()
        );
        assert!(
            validate_search_request(
                "music",
                1,
                &YouTubeMusicSearchConfig {
                    max_json_bytes: MAX_CONFIGURED_JSON_BYTES + 1,
                    ..config
                }
            )
            .is_err()
        );
    }

    #[test]
    fn diagnostics_redact_urls_and_remain_single_line() {
        let detail =
            sanitized_process_detail(b"ERROR: failed https://music.youtube.com/search?q=private\n");
        assert_eq!(detail, ": ERROR: failed <redacted-url>");
    }
}
