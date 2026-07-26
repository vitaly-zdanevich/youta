//! Safe process adapter for the external yt-dlp executable.
//!
//! Arguments are passed directly to the executable. Youta never constructs a
//! shell command from a media URL or title.

use std::collections::BTreeMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::time::Duration;

use serde::Deserialize;
use url::Url;

use super::{PlaybackError, Result};

const MAX_METADATA_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_AUDIO_FORMAT: &str = "bestaudio[acodec^=opus]/bestaudio";
const DOWNLOAD_OPUS_FORMAT: &str = "bestaudio[acodec^=opus]";

/// Configuration for invoking yt-dlp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct YtDlpConfig {
    /// Executable path or name.
    pub executable: PathBuf,
    /// yt-dlp format expression used for audio playback.
    pub audio_format: String,
    /// Whether user-installed yt-dlp plugins may be loaded.
    pub allow_plugins: bool,
}

impl Default for YtDlpConfig {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("yt-dlp"),
            audio_format: DEFAULT_AUDIO_FORMAT.to_owned(),
            allow_plugins: false,
        }
    }
}

/// Selected audio stream metadata returned by yt-dlp.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedMedia {
    /// Short-lived media URL.
    pub media_url: Url,
    /// HTTP headers that must accompany requests to the media URL.
    pub http_headers: BTreeMap<String, String>,
    /// Extracted title.
    pub title: String,
    /// Duration in seconds when known.
    pub duration_seconds: Option<f64>,
    /// Original webpage URL.
    pub webpage_url: Option<Url>,
    /// Thumbnail URL when exposed by the extractor.
    pub thumbnail_url: Option<Url>,
    /// Extractor-specific media identifier.
    pub id: String,
    /// Selected format identifier.
    pub format_id: Option<String>,
    /// Selected audio codec.
    pub audio_codec: Option<String>,
    /// yt-dlp extractor that handled the URL.
    pub extractor: Option<String>,
}

/// Flat entry returned for a channel, playlist, album, or other collection URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionEntry {
    /// Extractor-specific entry identifier.
    pub id: String,
    /// Entry title when the flat extractor exposes it.
    pub title: String,
    /// Canonical or extractor-provided webpage URL.
    pub webpage_url: Option<Url>,
    /// Duration in whole seconds when known.
    pub duration_seconds: Option<u64>,
}

/// Bounded flat collection used for generic yt-dlp URL subscriptions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractedCollection {
    /// Collection identifier.
    pub id: String,
    /// Collection title.
    pub title: String,
    /// Extractor that handled the URL.
    pub extractor: Option<String>,
    /// Flat entries in provider order.
    pub entries: Vec<CollectionEntry>,
}

/// Download behavior selected by the user.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DownloadFormat {
    /// Prefer an Opus source and remux it without lossy re-encoding.
    #[default]
    OpusWithoutTranscoding,
    /// Keep the provider's selected best-audio container and codec.
    OriginalBestAudio,
    /// Explicitly permit a lossy Opus transcode when a compatible stream is not
    /// available.
    TranscodeToOpus,
}

/// One machine-readable event emitted by a supervised `yt-dlp` download.
#[derive(Clone, Debug, PartialEq)]
pub enum DownloadEvent {
    /// Updated byte counts, speed, and estimated time remaining.
    Progress {
        /// Bytes written for the current media file.
        downloaded_bytes: u64,
        /// Exact or estimated total byte count.
        total_bytes: Option<u64>,
        /// Current transfer speed in bytes per second.
        bytes_per_second: Option<f64>,
        /// Estimated seconds until the current media file finishes.
        eta_seconds: Option<u64>,
    },
    /// Final path printed after every post-processing move.
    CompletedFile(PathBuf),
}

/// Request used to start a yt-dlp download.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadRequest {
    /// Source page URL.
    pub source_url: Url,
    /// Youta-owned destination directory.
    pub destination: PathBuf,
    /// Audio output behavior.
    pub format: DownloadFormat,
    /// Download the provider thumbnail alongside the audio.
    pub write_thumbnail: bool,
}

/// Child process for a running download.
///
/// Dropping this value terminates the exact yt-dlp child started by Youta.
pub struct DownloadProcess {
    child: Child,
    stdout: Option<BufReader<ChildStdout>>,
    stderr: Option<BufReader<ChildStderr>>,
}

impl DownloadProcess {
    /// Returns the progress stream. yt-dlp writes one machine-readable event per
    /// line because Youta starts it with `--newline` and `--progress-template`.
    pub fn take_progress_reader(&mut self) -> Option<BufReader<ChildStdout>> {
        self.stdout.take()
    }

    /// Returns the diagnostic stream.
    ///
    /// Callers should drain this concurrently with the progress stream so a
    /// chatty extractor cannot fill its operating-system pipe buffer.
    pub fn take_error_reader(&mut self) -> Option<BufReader<ChildStderr>> {
        self.stderr.take()
    }

    /// Checks whether the download has finished.
    ///
    /// # Errors
    ///
    /// Returns [`PlaybackError::Io`] when the operating system cannot query
    /// the supervised child process.
    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }

    /// Cancels the exact process created for this download.
    ///
    /// # Errors
    ///
    /// Returns [`PlaybackError::Io`] when the child status cannot be queried,
    /// the process cannot be terminated, or the terminated child cannot be
    /// reaped.
    pub fn cancel(&mut self) -> Result<()> {
        if self.child.try_wait()?.is_none() {
            let kill_result = self.child.kill();
            let wait_result = self.child.wait();
            kill_result?;
            wait_result?;
        }
        Ok(())
    }
}

impl Drop for DownloadProcess {
    fn drop(&mut self) {
        let _ = self.cancel();
    }
}

/// Lightweight yt-dlp process client.
#[derive(Clone, Debug)]
pub struct YtDlp {
    config: YtDlpConfig,
}

impl YtDlp {
    /// Creates a client with explicit process settings.
    #[must_use]
    pub fn new(config: YtDlpConfig) -> Self {
        Self { config }
    }

    /// Returns the installed `yt-dlp` version string.
    ///
    /// # Errors
    ///
    /// Returns a [`PlaybackError`] when `yt-dlp` is unavailable, cannot be
    /// executed, or exits unsuccessfully.
    pub fn version(&self) -> Result<String> {
        let output = Command::new(&self.config.executable)
            .arg("--version")
            .output()
            .map_err(|error| self.map_spawn_error(error))?;
        if !output.status.success() {
            return Err(PlaybackError::ProcessExited(format!(
                " ({})",
                output.status
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// Resolves the selected best audio stream and associated metadata.
    ///
    /// # Errors
    ///
    /// Returns a [`PlaybackError`] for an unsafe source URL, a missing or
    /// unsuccessful `yt-dlp` process, oversized metadata, or malformed output.
    pub fn resolve(&self, source_url: &Url) -> Result<ResolvedMedia> {
        validate_remote_source(source_url)?;
        let mut command = self.base_command();
        command
            .arg("--dump-single-json")
            .arg("--skip-download")
            .arg("--no-playlist")
            .arg("--format")
            .arg(&self.config.audio_format)
            .arg("--")
            .arg(source_url.as_str());
        let output = command
            .output()
            .map_err(|error| self.map_spawn_error(error))?;
        if !output.status.success() {
            return Err(PlaybackError::ProcessExited(format!(
                " ({}): {}",
                output.status,
                sanitized_stderr(&output.stderr)
            )));
        }
        if output.stdout.len() > MAX_METADATA_BYTES {
            return Err(PlaybackError::Protocol(format!(
                "yt-dlp metadata exceeded {MAX_METADATA_BYTES} bytes"
            )));
        }
        let extracted: ExtractedMedia = serde_json::from_slice(&output.stdout)?;
        extracted.try_into()
    }

    /// Lists the built-in extractors reported by the installed yt-dlp.
    ///
    /// The result changes when the external executable is upgraded, so Youta
    /// does not compile one feature or provider type per extractor.
    ///
    /// # Errors
    ///
    /// Returns a [`PlaybackError`] when `yt-dlp` cannot run, exits
    /// unsuccessfully, or emits an oversized extractor list.
    pub fn extractors(&self) -> Result<Vec<String>> {
        let output = self
            .base_command()
            .arg("--list-extractors")
            .output()
            .map_err(|error| self.map_spawn_error(error))?;
        if !output.status.success() {
            return Err(PlaybackError::ProcessExited(format!(
                " ({})",
                output.status
            )));
        }
        if output.stdout.len() > 2 * 1024 * 1024 {
            return Err(PlaybackError::Protocol(
                "yt-dlp extractor list exceeded 2 MiB".to_owned(),
            ));
        }
        Ok(parse_extractor_list(&output.stdout))
    }

    /// Reads a bounded channel or playlist URL without resolving every media
    /// stream.
    ///
    /// This is the generic subscription path for yt-dlp-supported sites that do
    /// not have a richer native provider. The caller chooses a small limit to
    /// avoid expensive scans and excessive provider requests.
    ///
    /// # Errors
    ///
    /// Returns a [`PlaybackError`] for an unsafe URL or zero entry limit, a
    /// missing or unsuccessful `yt-dlp` process, oversized metadata, or
    /// malformed collection output.
    pub fn collection(&self, source_url: &Url, max_entries: u16) -> Result<ExtractedCollection> {
        validate_remote_source(source_url)?;
        if max_entries == 0 {
            return Err(PlaybackError::InvalidValue(
                "collection limit must be greater than zero".to_owned(),
            ));
        }
        let output = self
            .base_command()
            .arg("--flat-playlist")
            .arg("--dump-single-json")
            .arg("--skip-download")
            .arg("--playlist-end")
            .arg(max_entries.to_string())
            .arg("--")
            .arg(source_url.as_str())
            .output()
            .map_err(|error| self.map_spawn_error(error))?;
        if !output.status.success() {
            return Err(PlaybackError::ProcessExited(format!(
                " ({}): {}",
                output.status,
                sanitized_stderr(&output.stderr)
            )));
        }
        if output.stdout.len() > MAX_METADATA_BYTES {
            return Err(PlaybackError::Protocol(format!(
                "yt-dlp collection metadata exceeded {MAX_METADATA_BYTES} bytes"
            )));
        }
        let collection: ExtractedCollectionJson = serde_json::from_slice(&output.stdout)?;
        collection.try_into()
    }

    /// Starts an audio download and returns a supervised child process.
    ///
    /// # Errors
    ///
    /// Returns a [`PlaybackError`] when the source URL or destination is
    /// invalid, the destination cannot be created, or `yt-dlp` cannot start.
    pub fn download(&self, request: &DownloadRequest) -> Result<DownloadProcess> {
        validate_remote_source(&request.source_url)?;
        ensure_destination(&request.destination)?;

        let mut command = build_download_command(&self.config, request);
        command
            .arg("--")
            .arg(request.source_url.as_str())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .map_err(|error| self.map_spawn_error(error))?;
        let stdout = child.stdout.take().map(BufReader::new);
        let stderr = child.stderr.take().map(BufReader::new);
        Ok(DownloadProcess {
            child,
            stdout,
            stderr,
        })
    }

    fn base_command(&self) -> Command {
        build_base_command(&self.config)
    }

    fn map_spawn_error(&self, error: std::io::Error) -> PlaybackError {
        if error.kind() == std::io::ErrorKind::NotFound {
            PlaybackError::ExecutableUnavailable(self.config.executable.display().to_string())
        } else {
            PlaybackError::Io(error)
        }
    }
}

#[derive(Debug, Deserialize)]
struct ExtractedMedia {
    url: String,
    #[serde(default)]
    http_headers: BTreeMap<String, String>,
    title: String,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    webpage_url: Option<String>,
    #[serde(default)]
    thumbnail: Option<String>,
    id: String,
    #[serde(default)]
    format_id: Option<String>,
    #[serde(default)]
    acodec: Option<String>,
    #[serde(default, alias = "extractor_key")]
    extractor: Option<String>,
}

impl TryFrom<ExtractedMedia> for ResolvedMedia {
    type Error = PlaybackError;

    fn try_from(value: ExtractedMedia) -> Result<Self> {
        let media_url = Url::parse(&value.url)
            .map_err(|error| PlaybackError::Protocol(format!("invalid media URL: {error}")))?;
        validate_remote_source(&media_url)?;
        let webpage_url = value
            .webpage_url
            .as_deref()
            .map(Url::parse)
            .transpose()
            .map_err(|error| PlaybackError::Protocol(format!("invalid webpage URL: {error}")))?;
        let thumbnail_url = value
            .thumbnail
            .as_deref()
            .map(Url::parse)
            .transpose()
            .map_err(|error| PlaybackError::Protocol(format!("invalid thumbnail URL: {error}")))?;

        Ok(Self {
            media_url,
            http_headers: value.http_headers,
            title: value.title,
            duration_seconds: value
                .duration
                .filter(|duration| duration.is_finite() && *duration >= 0.0),
            webpage_url,
            thumbnail_url,
            id: value.id,
            format_id: value.format_id,
            audio_codec: value.acodec,
            extractor: value.extractor,
        })
    }
}

#[derive(Debug, Deserialize)]
struct ExtractedCollectionJson {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default, alias = "extractor_key")]
    extractor: Option<String>,
    #[serde(default)]
    entries: Vec<ExtractedCollectionEntryJson>,
}

#[derive(Debug, Deserialize)]
struct ExtractedCollectionEntryJson {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    webpage_url: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
}

impl TryFrom<ExtractedCollectionJson> for ExtractedCollection {
    type Error = PlaybackError;

    fn try_from(value: ExtractedCollectionJson) -> Result<Self> {
        let entries = value
            .entries
            .into_iter()
            .map(|entry| {
                let raw_url = entry.webpage_url.or(entry.url);
                let webpage_url = raw_url
                    .as_deref()
                    .filter(|raw| raw.starts_with("http://") || raw.starts_with("https://"))
                    .map(Url::parse)
                    .transpose()
                    .map_err(|error| {
                        PlaybackError::Protocol(format!("invalid collection entry URL: {error}"))
                    })?;
                let duration_seconds = entry.duration.and_then(rounded_nonnegative_seconds);
                Ok(CollectionEntry {
                    id: entry.id,
                    title: entry.title,
                    webpage_url,
                    duration_seconds,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            id: value.id,
            title: value.title,
            extractor: value.extractor,
            entries,
        })
    }
}

fn build_base_command(config: &YtDlpConfig) -> Command {
    let mut command = Command::new(&config.executable);
    command.arg("--ignore-config");
    if !config.allow_plugins {
        command.arg("--no-plugin-dirs");
    }
    command
}

fn build_download_command(config: &YtDlpConfig, request: &DownloadRequest) -> Command {
    let mut command = build_base_command(config);
    command
        .arg("--no-playlist")
        .arg("--no-overwrites")
        .arg("--no-simulate")
        .arg("--newline")
        .arg("--progress")
        .arg("--progress-template")
        .arg(
            "download:youta-progress|%(progress.downloaded_bytes)s|%(progress.total_bytes)s|%(progress.total_bytes_estimate)s|%(progress.speed)s|%(progress.eta)s",
        )
        .arg("--print")
        .arg("after_move:youta-file|%(filepath)s")
        .arg("--paths")
        .arg(&request.destination)
        .arg("--output")
        .arg("%(title).180B [%(id)s].%(ext)s");

    match request.format {
        DownloadFormat::OpusWithoutTranscoding => {
            command
                .arg("--format")
                .arg(DOWNLOAD_OPUS_FORMAT)
                .arg("--remux-video")
                .arg("opus");
        }
        DownloadFormat::OriginalBestAudio => {
            command.arg("--format").arg("bestaudio");
        }
        DownloadFormat::TranscodeToOpus => {
            command
                .arg("--format")
                .arg("bestaudio")
                .arg("--extract-audio")
                .arg("--audio-format")
                .arg("opus");
        }
    }
    if request.write_thumbnail {
        command.arg("--write-thumbnail");
    }
    command
}

fn validate_remote_source(url: &Url) -> Result<()> {
    match url.scheme() {
        "http" | "https"
            if url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none() =>
        {
            Ok(())
        }
        "http" | "https" => Err(PlaybackError::InvalidValue(
            "remote URL must have a host and must not contain credentials".to_owned(),
        )),
        scheme => Err(PlaybackError::InvalidValue(format!(
            "unsupported remote URL scheme `{scheme}`"
        ))),
    }
}

/// Parses one line produced by Youta's `yt-dlp` progress templates.
///
/// Ordinary extractor log lines return `None` and can be retained as bounded
/// diagnostics by the caller.
#[must_use]
pub fn parse_download_event(line: &str) -> Option<DownloadEvent> {
    if let Some(path) = line.trim_end().strip_prefix("youta-file|") {
        return (!path.is_empty()).then(|| DownloadEvent::CompletedFile(PathBuf::from(path)));
    }
    let fields = line
        .trim_end()
        .strip_prefix("youta-progress|")?
        .split('|')
        .collect::<Vec<_>>();
    if fields.len() != 5 {
        return None;
    }
    let downloaded_bytes = parse_optional_u64(fields[0])?;
    let total_bytes = parse_optional_u64(fields[1]).or_else(|| parse_optional_u64(fields[2]));
    let bytes_per_second = parse_optional_f64(fields[3]);
    let eta_seconds = parse_optional_u64(fields[4]);
    Some(DownloadEvent::Progress {
        downloaded_bytes,
        total_bytes,
        bytes_per_second,
        eta_seconds,
    })
}

fn parse_optional_u64(value: &str) -> Option<u64> {
    (!matches!(value, "" | "NA" | "None" | "null"))
        .then(|| value.parse().ok())
        .flatten()
}

fn parse_optional_f64(value: &str) -> Option<f64> {
    (!matches!(value, "" | "NA" | "None" | "null"))
        .then(|| value.parse::<f64>().ok())
        .flatten()
        .filter(|value| value.is_finite() && *value >= 0.0)
}

fn rounded_nonnegative_seconds(value: f64) -> Option<u64> {
    let duration = Duration::try_from_secs_f64(value).ok()?;
    Some(
        duration
            .as_secs()
            .saturating_add(u64::from(duration.subsec_nanos() >= 500_000_000)),
    )
}

fn ensure_destination(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        return Err(PlaybackError::InvalidValue(
            "download destination cannot be empty".to_owned(),
        ));
    }
    std::fs::create_dir_all(path)?;
    Ok(())
}

fn sanitized_stderr(stderr: &[u8]) -> String {
    const MAX: usize = 1024;
    let visible = &stderr[..stderr.len().min(MAX)];
    String::from_utf8_lossy(visible).replace(['\r', '\n'], " ")
}

fn parse_extractor_list(output: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(output)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_command_disables_config_and_plugins_by_default() {
        let config = YtDlpConfig::default();
        let command = build_base_command(&config);
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(arguments, ["--ignore-config", "--no-plugin-dirs"]);
    }

    #[test]
    fn explicit_plugin_opt_in_omits_plugin_block() {
        let config = YtDlpConfig {
            allow_plugins: true,
            ..YtDlpConfig::default()
        };
        let command = build_base_command(&config);
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(arguments, ["--ignore-config"]);
    }

    #[test]
    fn rejects_local_and_script_urls() {
        for value in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://user:secret@example.test/audio",
        ] {
            let url = Url::parse(value).expect("valid test URL");
            assert!(validate_remote_source(&url).is_err());
        }
    }

    #[test]
    fn download_command_is_bounded_headless_and_machine_readable() {
        let config = YtDlpConfig::default();
        let request = DownloadRequest {
            source_url: Url::parse("https://example.test/watch/fixture").expect("source URL"),
            destination: PathBuf::from("/tmp/youta-fixture-downloads"),
            format: DownloadFormat::OpusWithoutTranscoding,
            write_thumbnail: true,
        };
        let command = build_download_command(&config, &request);
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(arguments.windows(2).any(|pair| {
            pair[0] == "--progress-template" && pair[1].starts_with("download:youta-progress|")
        }));
        assert!(arguments.windows(2).any(|pair| {
            pair[0] == "--print" && pair[1] == "after_move:youta-file|%(filepath)s"
        }));
        assert!(
            arguments
                .windows(2)
                .any(|pair| { pair[0] == "--paths" && pair[1] == "/tmp/youta-fixture-downloads" })
        );
        assert!(arguments.iter().any(|argument| argument == "--no-playlist"));
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "--no-overwrites")
        );
        assert!(arguments.iter().any(|argument| argument == "--no-simulate"));
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "--write-thumbnail")
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| { pair[0] == "--format" && pair[1] == "bestaudio[acodec^=opus]" })
        );
        assert!(
            !arguments
                .iter()
                .any(|argument| argument == "--yes-playlist")
        );
    }

    #[test]
    fn download_events_parse_progress_fallbacks_and_paths() {
        assert_eq!(
            parse_download_event("youta-progress|1024|4096|5000|128.5|31"),
            Some(DownloadEvent::Progress {
                downloaded_bytes: 1024,
                total_bytes: Some(4096),
                bytes_per_second: Some(128.5),
                eta_seconds: Some(31),
            })
        );
        assert_eq!(
            parse_download_event("youta-progress|1024|NA|5000|NA|NA"),
            Some(DownloadEvent::Progress {
                downloaded_bytes: 1024,
                total_bytes: Some(5000),
                bytes_per_second: None,
                eta_seconds: None,
            })
        );
        assert_eq!(
            parse_download_event("youta-file|/tmp/name|with-pipe.opus"),
            Some(DownloadEvent::CompletedFile(PathBuf::from(
                "/tmp/name|with-pipe.opus"
            )))
        );
        assert_eq!(parse_download_event("[download] ordinary log"), None);
        assert_eq!(parse_download_event("youta-progress|bad|1|1|1|1"), None);
    }

    #[test]
    fn collection_duration_rounding_is_checked_and_half_up() {
        assert_eq!(rounded_nonnegative_seconds(0.49), Some(0));
        assert_eq!(rounded_nonnegative_seconds(0.5), Some(1));
        assert_eq!(rounded_nonnegative_seconds(90.4), Some(90));
        assert_eq!(rounded_nonnegative_seconds(-1.0), None);
        assert_eq!(rounded_nonnegative_seconds(f64::NAN), None);
        assert_eq!(rounded_nonnegative_seconds(f64::INFINITY), None);
    }

    #[test]
    fn parses_resolved_media_fixture() {
        let fixture = r#"{
			"url": "https://media.example/audio.webm",
			"http_headers": {"User-Agent": "fixture"},
			"title": "Mock track",
			"duration": 42.5,
			"webpage_url": "https://www.youtube.com/watch?v=abcdefghijk",
			"thumbnail": "https://img.example/thumb.jpg",
			"id": "abcdefghijk",
			"format_id": "251",
			"acodec": "opus",
			"extractor": "youtube"
		}"#;
        let extracted: ExtractedMedia = serde_json::from_str(fixture).expect("fixture");
        let resolved = ResolvedMedia::try_from(extracted).expect("resolved fixture");

        assert_eq!(resolved.title, "Mock track");
        assert_eq!(resolved.duration_seconds, Some(42.5));
        assert_eq!(resolved.audio_codec.as_deref(), Some("opus"));
        assert_eq!(resolved.extractor.as_deref(), Some("youtube"));
        assert_eq!(
            resolved.http_headers.get("User-Agent").map(String::as_str),
            Some("fixture")
        );
    }

    #[test]
    fn extractor_list_ignores_blank_lines() {
        let parsed = parse_extractor_list(b"youtube\nSoundcloud\n\nPeerTube\n");
        assert_eq!(parsed, ["youtube", "Soundcloud", "PeerTube"]);
    }

    #[test]
    fn parses_flat_collection_fixture() {
        let fixture = r#"{
			"id": "collection-1",
			"title": "Mock channel",
			"extractor_key": "PeerTubePlaylist",
			"entries": [
				{
					"id": "entry-1",
					"title": "First",
					"webpage_url": "https://media.example/watch/entry-1",
					"duration": 90
				}
			]
		}"#;
        let extracted: ExtractedCollectionJson = serde_json::from_str(fixture).expect("fixture");
        let collection = ExtractedCollection::try_from(extracted).expect("collection");

        assert_eq!(collection.title, "Mock channel");
        assert_eq!(collection.entries.len(), 1);
        assert_eq!(collection.entries[0].duration_seconds, Some(90));
    }
}
