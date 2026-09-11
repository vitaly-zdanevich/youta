//! Safe process adapter for the external yt-dlp executable.
//!
//! Arguments are passed directly to the executable. Youta never constructs a
//! shell command from a media URL or title.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use url::Url;

use super::youtube_prewarm::{YouTubePrewarmCancellation, run_bounded_json_command};
use super::{PlaybackError, Result};

const MAX_METADATA_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_AUDIO_FORMAT: &str = "bestaudio[acodec^=opus]/bestaudio";
const DOWNLOAD_OPUS_FORMAT: &str = "bestaudio[acodec^=opus]";

/// Lightweight JavaScript runtime enabled in addition to yt-dlp's default Deno.
///
/// QuickJS-ng is available on 32-bit Gentoo x86, where the Deno binary is not.
/// Enabling it here preserves Deno's higher priority on platforms that have it.
pub(crate) const ADDITIONAL_JS_RUNTIME: &str = "quickjs";

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
    /// Provider artwork retained without resolving the media stream.
    pub thumbnail_url: Option<Url>,
    /// Provider publication time in Unix seconds; date-only values use UTC midnight.
    /// Missing dates can be enriched without resolving or downloading the audio.
    pub published_at: Option<i64>,
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
    /// Collection artwork, preferring a square channel avatar when available.
    pub thumbnail_url: Option<Url>,
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

/// Number of extractor entries one supervised download may consume.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DownloadScope {
    /// Download only the selected media item.
    #[default]
    SingleItem,
    /// Download every public entry exposed by a channel or playlist URL.
    Collection,
    /// Record all current collection entries without downloading media.
    CollectionArchiveOnly,
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
        /// One-based position of the current media inside a collection.
        collection_index: Option<u64>,
        /// Total collection entries reported by the extractor.
        collection_count: Option<u64>,
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
    /// Whether yt-dlp may traverse a collection URL.
    pub scope: DownloadScope,
    /// One-based first collection entry, or every entry when absent.
    pub playlist_start: Option<u64>,
    /// Reject YouTube collection entries whose canonical source is a Shorts URL.
    pub skip_shorts: bool,
    /// Download the provider thumbnail alongside the audio.
    pub write_thumbnail: bool,
    /// Explicit archive used instead of the shared manual collection archive.
    pub archive_path: Option<PathBuf>,
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
        let output = crate::child_process::quiet(&mut Command::new(&self.config.executable))
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

    /// Enumerates all upload types in one newest-first channel catalogue.
    ///
    /// The uploads playlist supplies the global order, while the channel page
    /// supplies its title and avatar. When `classify_shorts` is enabled, channel
    /// tabs are fully enumerated to restore Shorts URLs omitted by the uploads
    /// playlist. Otherwise only one entry per channel tab is requested for its
    /// metadata. Neither request resolves or downloads media streams.
    ///
    /// # Errors
    ///
    /// Returns a [`PlaybackError`] for an invalid channel ID or zero limit,
    /// and propagates collection extraction errors from either request.
    pub fn youtube_channel_collection(
        &self,
        channel_id: &str,
        max_entries: u16,
        classify_shorts: bool,
    ) -> Result<ExtractedCollection> {
        youtube_channel_collection_with(channel_id, max_entries, classify_shorts, |url, limit| {
            self.collection(url, limit)
        })
    }

    /// Enumerates a channel with bounded helpers that are killed and reaped on
    /// cancellation. Each catalogue request has a two-minute deadline.
    ///
    /// # Errors
    ///
    /// Returns the usual catalogue errors, or a sanitized cancellation, timeout,
    /// output-size, or helper failure without including extractor diagnostics.
    pub fn youtube_channel_collection_cancellable(
        &self,
        channel_id: &str,
        max_entries: u16,
        classify_shorts: bool,
        cancellation: &YouTubePrewarmCancellation,
    ) -> Result<ExtractedCollection> {
        youtube_channel_collection_with(channel_id, max_entries, classify_shorts, |url, limit| {
            let mut command = self.base_command();
            command
                .args([
                    "--flat-playlist",
                    "--dump-single-json",
                    "--skip-download",
                    "--socket-timeout",
                    "15",
                    "--retries",
                    "1",
                    "--extractor-retries",
                    "1",
                    "--playlist-end",
                ])
                .arg(limit.to_string())
                .arg("--")
                .arg(url.as_str());
            let output = run_bounded_json_command(
                &mut command,
                Duration::from_secs(120),
                MAX_METADATA_BYTES,
                cancellation,
            )
            .map_err(|error| {
                PlaybackError::Protocol(format!("YouTube channel metadata: {error}"))
            })?;
            let collection: ExtractedCollectionJson = serde_json::from_slice(&output)?;
            collection.try_into()
        })
    }

    /// Populates missing episode publication dates without downloading media.
    ///
    /// Only the caller's retained entries are inspected. Exact cached dates are
    /// reused; each uncached video uses one metadata-only yt-dlp helper with a
    /// 30-second deadline. At most four helpers run together, and cancellation
    /// kills and reaps their process groups. Date-only metadata uses UTC midnight.
    /// Cache failures are nonfatal: a successfully fetched date remains usable.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid video ID, missing or mismatched date
    /// metadata, cancellation, timeout, or unsuccessful helper. No later batch
    /// starts after a failure, and the caller's cancellation token is not changed.
    pub fn populate_youtube_publication_dates(
        &self,
        entries: &mut [CollectionEntry],
        cache_dir: &Path,
        cancellation: &YouTubePrewarmCancellation,
    ) -> Result<()> {
        self.populate_youtube_publication_dates_with_lookup(
            entries,
            cache_dir,
            cancellation,
            |id| self.youtube_publication_date(id, cancellation),
        )
    }

    /// Reuses existing dates, then requests only uncached IDs in batches of 50.
    ///
    /// A caller can supply the existing official API client, or an empty map
    /// when it is not configured. A failed batch disables further batch calls
    /// for this feed. Missing results use small anonymous metadata requests when
    /// networking is built, falling back to the bounded yt-dlp helper. Three
    /// consecutive anonymous failures disable that shortcut for this feed.
    /// Neither path resolves media. Exact dates remain required for every item.
    ///
    /// # Errors
    ///
    /// Returns an error for cancellation, invalid IDs or batch metadata, or when
    /// both per-video lookup paths fail. Cache write failures remain nonfatal.
    pub fn populate_youtube_publication_dates_with_batch(
        &self,
        entries: &mut [CollectionEntry],
        cache_dir: &Path,
        cancellation: &YouTubePrewarmCancellation,
        lookup_batch: impl FnMut(&[String]) -> Result<HashMap<String, i64>>,
    ) -> Result<()> {
        #[cfg(feature = "network")]
        let fast_client = super::youtube_dates::YouTubeDateClient::new();
        #[cfg(feature = "network")]
        let failures = std::sync::atomic::AtomicUsize::new(0);
        self.populate_youtube_publication_dates_with_batch_lookup(
            entries,
            cache_dir,
            cancellation,
            lookup_batch,
            |id| {
                #[cfg(feature = "network")]
                {
                    use std::sync::atomic::Ordering;
                    if failures.load(Ordering::Relaxed) < 3 {
                        match fast_client.publication_date(id) {
                            Ok(date) => {
                                failures.store(0, Ordering::Relaxed);
                                return Ok(date);
                            }
                            Err(_) => {
                                failures.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
                ensure_publication_lookup_active(cancellation)?;
                self.youtube_publication_date(id, cancellation)
            },
        )
    }

    /// Keeps cache and batch policy independently testable without live services.
    fn populate_youtube_publication_dates_with_batch_lookup(
        &self,
        entries: &mut [CollectionEntry],
        cache_dir: &Path,
        cancellation: &YouTubePrewarmCancellation,
        mut lookup_batch: impl FnMut(&[String]) -> Result<HashMap<String, i64>>,
        lookup_one: impl Fn(&str) -> Result<i64> + Sync,
    ) -> Result<()> {
        ensure_publication_lookup_active(cancellation)?;
        let mut missing = Vec::new();
        let mut seen = HashSet::new();
        for entry in entries.iter_mut() {
            ensure_publication_lookup_active(cancellation)?;
            validate_publication_video_id(&entry.id)?;
            if let Some(date) = entry
                .published_at
                .filter(|date| valid_publication_timestamp(*date))
            {
                // Persist reused API metadata too, so a restart needs no new lookup.
                if load_publication_date(cache_dir, &entry.id) != Some(date) {
                    let _ = store_publication_date(cache_dir, &entry.id, date);
                }
            } else {
                entry.published_at = load_publication_date(cache_dir, &entry.id);
            }
            if entry.published_at.is_none() && seen.insert(entry.id.clone()) {
                missing.push(entry.id.clone());
            }
        }
        let mut dates = HashMap::new();
        for ids in missing.chunks(50) {
            ensure_publication_lookup_active(cancellation)?;
            let result = lookup_batch(ids);
            ensure_publication_lookup_active(cancellation)?;
            let Ok(batch) = result else {
                break;
            };
            if batch
                .iter()
                .any(|(id, date)| !ids.contains(id) || !valid_publication_timestamp(*date))
            {
                return Err(PlaybackError::Protocol(
                    "YouTube date batch returned invalid publication metadata".to_owned(),
                ));
            }
            for (id, date) in batch {
                let _ = store_publication_date(cache_dir, &id, date);
                dates.insert(id, date);
            }
        }
        for entry in entries.iter_mut() {
            if entry.published_at.is_none() {
                entry.published_at = dates.get(&entry.id).copied();
            }
        }
        self.populate_youtube_publication_dates_with_lookup(
            entries,
            cache_dir,
            cancellation,
            lookup_one,
        )
    }

    /// Runs bounded per-video fallback work while retaining exact cache results.
    fn populate_youtube_publication_dates_with_lookup(
        &self,
        entries: &mut [CollectionEntry],
        cache_dir: &Path,
        cancellation: &YouTubePrewarmCancellation,
        lookup: impl Fn(&str) -> Result<i64> + Sync,
    ) -> Result<()> {
        ensure_publication_lookup_active(cancellation)?;
        let mut missing = entries
            .iter_mut()
            .filter(|entry| !entry.published_at.is_some_and(valid_publication_timestamp))
            .collect::<Vec<_>>();
        for batch in missing.chunks_mut(4) {
            let lookup = &lookup;
            std::thread::scope(|scope| -> Result<()> {
                let workers = batch
                    .iter_mut()
                    .map(|entry| {
                        scope.spawn(move || -> Result<()> {
                            ensure_publication_lookup_active(cancellation)?;
                            if entry.published_at.is_some_and(valid_publication_timestamp) {
                                return Ok(());
                            }
                            validate_publication_video_id(&entry.id)?;
                            let published_at = match load_publication_date(cache_dir, &entry.id) {
                                Some(date) => date,
                                None => {
                                    let date = lookup(&entry.id)?;
                                    if !valid_publication_timestamp(date) {
                                        return Err(PlaybackError::Protocol(
                                            "YouTube returned an invalid publication date"
                                                .to_owned(),
                                        ));
                                    }
                                    let _ = store_publication_date(cache_dir, &entry.id, date);
                                    date
                                }
                            };
                            ensure_publication_lookup_active(cancellation)?;
                            entry.published_at = Some(published_at);
                            Ok(())
                        })
                    })
                    .collect::<Vec<_>>();
                for worker in workers {
                    worker.join().map_err(|_| {
                        PlaybackError::Protocol("YouTube episode date worker failed".to_owned())
                    })??;
                }
                Ok(())
            })?;
        }
        ensure_publication_lookup_active(cancellation)
    }

    /// Last-resort exact date extraction using the existing supervised helper.
    fn youtube_publication_date(
        &self,
        video_id: &str,
        cancellation: &YouTubePrewarmCancellation,
    ) -> Result<i64> {
        let mut command = build_publication_date_command(&self.config, video_id);
        let output =
            run_bounded_json_command(&mut command, Duration::from_secs(30), 4096, cancellation)
                .map_err(|error| {
                    PlaybackError::Protocol(format!(
                        "YouTube episode date lookup failed for {video_id}: {error}"
                    ))
                })?;
        parse_youtube_publication_date(&output, video_id)
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

/// Loads channel metadata and the unified uploads playlist through one adapter.
fn youtube_channel_collection_with(
    channel_id: &str,
    max_entries: u16,
    classify_shorts: bool,
    mut extract: impl FnMut(&Url, u16) -> Result<ExtractedCollection>,
) -> Result<ExtractedCollection> {
    let suffix = channel_id
        .strip_prefix("UC")
        .filter(|suffix| !suffix.is_empty());
    if max_entries == 0
        || suffix.is_none()
        || channel_id.len() > 128
        || !channel_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(PlaybackError::InvalidValue(
            "YouTube channel catalogue requires a valid UC channel ID and a positive limit"
                .to_owned(),
        ));
    }
    let channel_url = Url::parse(&format!("https://www.youtube.com/channel/{channel_id}"))
        .map_err(|error| {
            PlaybackError::InvalidValue(format!("invalid YouTube channel URL: {error}"))
        })?;
    let mut uploads_url = Url::parse("https://www.youtube.com/playlist").map_err(|error| {
        PlaybackError::InvalidValue(format!("invalid YouTube uploads URL: {error}"))
    })?;
    uploads_url
        .query_pairs_mut()
        .append_pair("list", &format!("UU{}", suffix.unwrap_or_default()));
    let channel_limit = if classify_shorts { max_entries } else { 1 };
    let channel = extract(&channel_url, channel_limit)?;
    let uploads = extract(&uploads_url, max_entries)?;
    Ok(merge_youtube_channel_collections(channel, uploads))
}

/// Keeps global upload order and restores channel artwork and canonical Shorts URLs.
///
/// Shorts are marked rather than removed so a selected Short remains available
/// as an inclusive podcast boundary. Repeated uploads retain their first,
/// newest occurrence before the podcast preparation reverses the catalogue.
fn merge_youtube_channel_collections(
    channel: ExtractedCollection,
    mut uploads: ExtractedCollection,
) -> ExtractedCollection {
    let shorts = channel
        .entries
        .into_iter()
        .filter_map(|entry| {
            let url = entry.webpage_url?;
            let is_short = url
                .domain()
                .is_some_and(|domain| domain == "youtube.com" || domain.ends_with(".youtube.com"))
                && url.path_segments().and_then(|mut segments| segments.next()) == Some("shorts");
            is_short.then_some((entry.id, url))
        })
        .collect::<HashMap<_, _>>();
    let mut seen = HashSet::new();
    uploads.entries.retain_mut(|entry| {
        if !seen.insert(entry.id.clone()) {
            return false;
        }
        if let Some(short_url) = shorts.get(&entry.id) {
            entry.webpage_url = Some(short_url.clone());
        }
        true
    });
    ExtractedCollection {
        id: channel.id,
        title: if channel.title.trim().is_empty() {
            uploads.title
        } else {
            channel.title
        },
        extractor: channel.extractor.or(uploads.extractor),
        thumbnail_url: channel.thumbnail_url.or(uploads.thumbnail_url),
        entries: uploads.entries,
    }
}

#[derive(Debug, Deserialize)]
struct ExtractedCollectionJson {
    id: String,
    /// Flat extractors can omit a title or explicitly report JSON null.
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    extractor: Option<String>,
    // yt-dlp emits both extractor fields for channel and playlist documents.
    #[serde(default)]
    extractor_key: Option<String>,
    #[serde(default)]
    thumbnail: Option<String>,
    #[serde(default)]
    thumbnails: Vec<ExtractedThumbnailJson>,
    #[serde(default)]
    entries: Vec<ExtractedCollectionEntryJson>,
}

#[derive(Debug, Deserialize)]
struct ExtractedCollectionEntryJson {
    id: String,
    /// Missing and null titles share the existing empty-title display fallback.
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    webpage_url: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    thumbnail: Option<String>,
    #[serde(default)]
    thumbnails: Vec<ExtractedThumbnailJson>,
    #[serde(flatten)]
    publication: PublicationDateJson,
    // Channel roots contain Videos, Live, and Shorts playlist wrappers.
    #[serde(default)]
    entries: Vec<ExtractedCollectionEntryJson>,
}

#[derive(Debug, Deserialize)]
struct ExtractedThumbnailJson {
    url: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
}

/// Actual extractor timestamps, deliberately excluding relative playlist dates.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PublicationDateJson {
    release_timestamp: Option<i64>,
    timestamp: Option<i64>,
    upload_date: Option<String>,
}

impl PublicationDateJson {
    /// Prefers a premiere/stream release time, then upload time, then its date.
    fn published_at(&self) -> Option<i64> {
        self.release_timestamp
            .filter(|value| valid_publication_timestamp(*value))
            .or_else(|| {
                self.timestamp
                    .filter(|value| valid_publication_timestamp(*value))
            })
            .or_else(|| {
                let raw = self.upload_date.as_deref()?;
                if raw.len() != 8 || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
                    return None;
                }
                let date = NaiveDate::parse_from_str(raw, "%Y%m%d").ok()?;
                let timestamp = date.and_hms_opt(0, 0, 0)?.and_utc().timestamp();
                valid_publication_timestamp(timestamp).then_some(timestamp)
            })
    }
}

/// Limits RSS dates to representable four-digit calendar years after the epoch.
fn valid_publication_timestamp(timestamp: i64) -> bool {
    (0..=253_402_300_799).contains(&timestamp)
        && DateTime::<Utc>::from_timestamp(timestamp, 0).is_some()
}

/// Accepts only canonical video IDs before they become either URLs or filenames.
fn validate_publication_video_id(video_id: &str) -> Result<()> {
    if video_id.len() == 11
        && video_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Ok(())
    } else {
        Err(PlaybackError::InvalidValue(
            "YouTube episode date requires a valid video ID".to_owned(),
        ))
    }
}

/// Stops lookup before touching cache files or starting another helper batch.
fn ensure_publication_lookup_active(cancellation: &YouTubePrewarmCancellation) -> Result<()> {
    if cancellation.is_cancelled() {
        Err(PlaybackError::Protocol(
            "YouTube episode date lookup was cancelled".to_owned(),
        ))
    } else {
        Ok(())
    }
}

/// Fetches the original webpage's microformat date while skipping media work.
fn build_publication_date_command(config: &YtDlpConfig, video_id: &str) -> Command {
    let mut command = build_base_command(config);
    command
        .args([
            "--no-warnings",
            "--no-playlist",
            "--skip-download",
            "--ignore-no-formats-error",
            "--no-check-formats",
            "--socket-timeout",
            "10",
            "--retries",
            "0",
            "--extractor-retries",
            "0",
            "--extractor-args",
            "youtube:player_client=web;player_skip=configs,js;skip=hls,dash;webpage_skip=",
            "--print",
            "%(.{id,timestamp,release_timestamp,upload_date})j",
            "--",
        ])
        .arg(format!("https://www.youtube.com/watch?v={video_id}"));
    command
}

/// Rejects mismatched video metadata instead of assigning another episode's date.
fn parse_youtube_publication_date(bytes: &[u8], video_id: &str) -> Result<i64> {
    #[derive(Deserialize)]
    struct Metadata {
        id: String,
        #[serde(flatten)]
        publication: PublicationDateJson,
    }
    let metadata: Metadata = serde_json::from_slice(bytes).map_err(|_| {
        PlaybackError::Protocol("YouTube episode date lookup returned invalid JSON".to_owned())
    })?;
    if metadata.id != video_id {
        return Err(PlaybackError::Protocol(
            "YouTube episode date lookup returned another video's metadata".to_owned(),
        ));
    }
    metadata.publication.published_at().ok_or_else(|| {
        PlaybackError::Protocol(format!(
            "YouTube did not provide a publication date for episode {video_id}"
        ))
    })
}

/// Versioned cache stores only public immutable identity and publication metadata.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CachedPublicationDate {
    version: u8,
    id: String,
    published_at: i64,
}

/// Reads a small ordinary cache file; stale, malformed and wrong-ID files miss.
fn load_publication_date(cache_dir: &Path, video_id: &str) -> Option<i64> {
    validate_publication_video_id(video_id).ok()?;
    let path = cache_dir.join(format!("{video_id}.json"));
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() > 1024 {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(1025)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 1024 {
        return None;
    }
    let cached: CachedPublicationDate = serde_json::from_slice(&bytes).ok()?;
    (cached.version == 1
        && cached.id == video_id
        && valid_publication_timestamp(cached.published_at))
    .then_some(cached.published_at)
}

/// Publishes a private atomic cache entry; callers may ignore disposable-cache I/O.
fn store_publication_date(
    cache_dir: &Path,
    video_id: &str,
    published_at: i64,
) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);
    if validate_publication_video_id(video_id).is_err()
        || !valid_publication_timestamp(published_at)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid publication date cache entry",
        ));
    }
    crate::private_files::create_private_directory(cache_dir)?;
    let cached = CachedPublicationDate {
        version: 1,
        id: video_id.to_owned(),
        published_at,
    };
    let bytes = serde_json::to_vec(&cached).map_err(std::io::Error::other)?;
    let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
    let temporary = cache_dir.join(format!(".{video_id}.{}.{sequence}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = crate::private_files::open_privately(&mut options).open(&temporary)?;
    let result = (|| {
        file.write_all(&bytes)?;
        file.flush()?;
        drop(file);
        std::fs::rename(&temporary, cache_dir.join(format!("{video_id}.json")))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

impl TryFrom<ExtractedCollectionJson> for ExtractedCollection {
    type Error = PlaybackError;

    fn try_from(value: ExtractedCollectionJson) -> Result<Self> {
        let thumbnail_url = preferred_collection_thumbnail(value.thumbnail, value.thumbnails);
        let mut entries = Vec::new();
        let mut pending = VecDeque::from(value.entries);
        while let Some(entry) = pending.pop_front() {
            if !entry.entries.is_empty() {
                // Prepending in reverse retains yt-dlp's depth-first provider order.
                for child in entry.entries.into_iter().rev() {
                    pending.push_front(child);
                }
            } else {
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
                let thumbnail_url = entry
                    .thumbnails
                    .into_iter()
                    .rev()
                    .map(|thumbnail| thumbnail.url)
                    .chain(entry.thumbnail)
                    .find_map(|raw| {
                        Url::parse(&raw)
                            .ok()
                            .filter(|url| matches!(url.scheme(), "http" | "https"))
                    });
                entries.push(CollectionEntry {
                    id: entry.id,
                    title: entry.title.unwrap_or_default(),
                    webpage_url,
                    duration_seconds,
                    thumbnail_url,
                    published_at: entry.publication.published_at(),
                });
            }
        }

        Ok(Self {
            id: value.id,
            title: value.title.unwrap_or_default(),
            extractor: value.extractor_key.or(value.extractor),
            thumbnail_url,
            entries,
        })
    }
}

/// Selects a collection cover without mistaking a wide channel banner for an
/// avatar when yt-dlp exposes both kinds in one thumbnail list.
fn preferred_collection_thumbnail(
    thumbnail: Option<String>,
    thumbnails: Vec<ExtractedThumbnailJson>,
) -> Option<Url> {
    thumbnails
        .into_iter()
        .filter_map(|candidate| {
            let url = Url::parse(&candidate.url)
                .ok()
                .filter(|url| matches!(url.scheme(), "http" | "https"))?;
            let named_avatar = candidate
                .id
                .as_deref()
                .is_some_and(|id| id.to_ascii_lowercase().contains("avatar"));
            let square = candidate
                .width
                .zip(candidate.height)
                .is_some_and(|(width, height)| width == height);
            let known_area = candidate
                .width
                .zip(candidate.height)
                .map_or(0, |(width, height)| u64::from(width) * u64::from(height));
            Some(((named_avatar || square, square, known_area), url))
        })
        .max_by_key(|(preference, _)| *preference)
        .map(|(_, url)| url)
        .or_else(|| {
            thumbnail.and_then(|raw| {
                Url::parse(&raw)
                    .ok()
                    .filter(|url| matches!(url.scheme(), "http" | "https"))
            })
        })
}

fn build_base_command(config: &YtDlpConfig) -> Command {
    let mut command = Command::new(&config.executable);
    crate::child_process::quiet(&mut command);
    command.arg("--ignore-config");
    if !config.allow_plugins {
        command.arg("--no-plugin-dirs");
    }
    command.arg("--js-runtimes").arg(ADDITIONAL_JS_RUNTIME);
    command
}

fn build_download_command(config: &YtDlpConfig, request: &DownloadRequest) -> Command {
    let mut command = build_base_command(config);
    command
		.arg("--no-overwrites")
        .arg(if request.scope == DownloadScope::CollectionArchiveOnly {
            "--simulate"
        } else {
            "--no-simulate"
        })
        .arg("--newline")
        .arg("--progress")
        .arg("--progress-template")
		.arg(
			"download:youta-progress|%(progress.downloaded_bytes)s|%(progress.total_bytes)s|%(progress.total_bytes_estimate)s|%(progress.speed)s|%(progress.eta)s|%(info.playlist_index)s|%(info.playlist_count)s",
		)
        .arg("--print")
        .arg("after_move:youta-file|%(filepath)s")
        .arg("--paths")
        .arg(&request.destination)
        .arg("--output")
		.arg(match request.scope {
			DownloadScope::SingleItem => "%(title).180B [%(id)s].%(ext)s",
			DownloadScope::Collection | DownloadScope::CollectionArchiveOnly => {
				"%(channel).100B [%(channel_id)s]/%(title).180B [%(id)s].%(ext)s"
			}
		});

    match request.scope {
        DownloadScope::SingleItem => {
            command.arg("--no-playlist");
        }
        DownloadScope::Collection | DownloadScope::CollectionArchiveOnly => {
            let default_archive = request.destination.join(".youta-download-archive");
            command
                .arg("--yes-playlist")
                .arg("--download-archive")
                .arg(request.archive_path.as_ref().unwrap_or(&default_archive));
            if request.scope == DownloadScope::CollectionArchiveOnly {
                command.arg("--flat-playlist").arg("--force-write-archive");
                return command;
            }
            if let Some(playlist_start) = request.playlist_start {
                command
                    .arg("--playlist-start")
                    .arg(playlist_start.to_string());
            }
            if request.skip_shorts {
                command
                    .arg("--match-filters")
                    .arg("original_url!*=/shorts/");
            }
        }
    }

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
    if !matches!(fields.len(), 5 | 7) {
        return None;
    }
    let downloaded_bytes = parse_optional_u64(fields[0])?;
    let total_bytes = parse_optional_u64(fields[1]).or_else(|| parse_optional_u64(fields[2]));
    let bytes_per_second = parse_optional_f64(fields[3]);
    let eta_seconds = parse_optional_u64(fields[4]);
    let collection_index = fields.get(5).and_then(|value| parse_optional_u64(value));
    let collection_count = fields.get(6).and_then(|value| parse_optional_u64(value));
    Some(DownloadEvent::Progress {
        downloaded_bytes,
        total_bytes,
        bytes_per_second,
        eta_seconds,
        collection_index,
        collection_count,
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
    fn publication_date_batches_replace_per_episode_helpers_and_reuse_cache() {
        let directory = tempfile::tempdir().expect("date cache");
        let client = YtDlp::new(YtDlpConfig {
            executable: directory.path().join("must-not-start-yt-dlp"),
            ..YtDlpConfig::default()
        });
        let mut entries = (0..121)
            .map(|index| CollectionEntry {
                id: format!("{index:011}"),
                title: "Episode".to_owned(),
                webpage_url: None,
                duration_seconds: None,
                thumbnail_url: None,
                published_at: None,
            })
            .collect::<Vec<_>>();
        entries[0].published_at = Some(1_709_164_800);
        store_publication_date(directory.path(), &entries[1].id, 1_709_164_801)
            .expect("cached date");
        let mut batches = Vec::new();
        client
            .populate_youtube_publication_dates_with_batch(
                &mut entries,
                directory.path(),
                &YouTubePrewarmCancellation::new(),
                |ids| {
                    batches.push(ids.to_vec());
                    Ok(ids.iter().map(|id| (id.clone(), 1_709_164_802)).collect())
                },
            )
            .expect("batched dates without helpers");
        assert_eq!(
            batches.iter().map(Vec::len).collect::<Vec<_>>(),
            [50, 50, 19]
        );
        assert_eq!(entries[0].published_at, Some(1_709_164_800));
        assert_eq!(entries[1].published_at, Some(1_709_164_801));
        assert!(
            entries[2..]
                .iter()
                .all(|entry| entry.published_at == Some(1_709_164_802))
        );
        for entry in &mut entries {
            entry.published_at = None;
        }
        client
            .populate_youtube_publication_dates_with_batch(
                &mut entries,
                directory.path(),
                &YouTubePrewarmCancellation::new(),
                |_| panic!("cached feed must not make API requests"),
            )
            .expect("repeat feed uses only cache");
    }

    #[test]
    fn publication_date_batch_fallback_and_failure_policy() {
        use std::sync::Mutex;
        let directory = tempfile::tempdir().expect("cache");
        let client = YtDlp::new(YtDlpConfig::default());
        let mut entries = (0..151)
            .map(|index| CollectionEntry {
                id: format!("{index:011}"),
                title: "Episode".to_owned(),
                webpage_url: None,
                duration_seconds: None,
                thumbnail_url: None,
                published_at: None,
            })
            .collect::<Vec<_>>();
        let mut requests = 0;
        let fallbacks = Mutex::new(Vec::new());
        client
            .populate_youtube_publication_dates_with_batch_lookup(
                &mut entries,
                directory.path(),
                &YouTubePrewarmCancellation::new(),
                |ids| {
                    requests += 1;
                    if requests == 1 {
                        Ok(ids[1..]
                            .iter()
                            .map(|id| (id.clone(), 1_709_164_800))
                            .collect())
                    } else {
                        Err(PlaybackError::Protocol("API quota fixture".to_owned()))
                    }
                },
                |id| {
                    fallbacks.lock().unwrap().push(id.to_owned());
                    Ok(1_709_164_801)
                },
            )
            .expect("all episodes dated");
        assert_eq!(requests, 2, "stop API calls after one failure");
        let mut actual = fallbacks.into_inner().unwrap();
        actual.sort();
        let expected = entries
            .iter()
            .enumerate()
            .filter(|(i, _)| *i == 0 || *i >= 50)
            .map(|(_, entry)| entry.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "never refetch successful batch results");
        assert!(entries.iter().all(|entry| entry.published_at.is_some()));
    }

    #[test]
    fn publication_date_sparse_fallbacks_keep_four_requests_in_flight() {
        use std::sync::{Condvar, Mutex};

        let directory = tempfile::tempdir().expect("date cache");
        let client = YtDlp::new(YtDlpConfig::default());
        let mut entries = (0..13)
            .map(|index| CollectionEntry {
                id: format!("{index:011}"),
                title: "Episode".to_owned(),
                webpage_url: None,
                duration_seconds: None,
                thumbnail_url: None,
                published_at: (index % 4 != 0).then_some(1_709_164_800),
            })
            .collect::<Vec<_>>();
        // Arrival count, active callbacks, and peak concurrent callbacks. A
        // timed condition variable makes the old serial behavior fail rather
        // than hanging forever while waiting for the remaining three workers.
        let counts = Mutex::new((0_usize, 0_usize, 0_usize));
        let arrivals = Condvar::new();
        client
            .populate_youtube_publication_dates_with_batch_lookup(
                &mut entries,
                directory.path(),
                &YouTubePrewarmCancellation::new(),
                |_| Ok(HashMap::new()),
                |id| {
                    assert_eq!(id.parse::<usize>().expect("fixture ID") % 4, 0);
                    let mut state = counts.lock().expect("callback counts");
                    state.0 += 1;
                    state.1 += 1;
                    state.2 = state.2.max(state.1);
                    arrivals.notify_all();
                    let (mut state, timeout) = arrivals
                        .wait_timeout_while(state, Duration::from_secs(5), |state| state.0 < 4)
                        .expect("bounded callback wait");
                    state.1 -= 1;
                    if timeout.timed_out() && state.0 < 4 {
                        return Err(PlaybackError::Protocol(
                            "sparse date requests did not run concurrently".to_owned(),
                        ));
                    }
                    Ok(1_709_164_801)
                },
            )
            .expect("four sparse misses should be looked up together without HTTP");

        let (started, active, peak) = counts.into_inner().expect("callback counts");
        assert_eq!(started, 4, "already dated entries must not be requested");
        assert_eq!(active, 0);
        assert_eq!(peak, 4, "sparse cache misses must use all four workers");
        for (index, entry) in entries.iter().enumerate() {
            assert_eq!(
                entry.published_at,
                Some(if index % 4 == 0 {
                    1_709_164_801
                } else {
                    1_709_164_800
                }),
            );
        }
    }

    #[test]
    fn publication_date_batches_require_valid_dates_and_honor_cancellation() {
        let client = YtDlp::new(YtDlpConfig::default());
        let entry = CollectionEntry {
            id: "jNQXAC9IVRw".to_owned(),
            title: "Episode".to_owned(),
            webpage_url: None,
            duration_seconds: None,
            thumbnail_url: None,
            published_at: None,
        };
        for scenario in 0..5 {
            let directory = tempfile::tempdir().expect("cache");
            let cancellation = YouTubePrewarmCancellation::new();
            let mut entries = [entry.clone()];
            let result = client.populate_youtube_publication_dates_with_batch_lookup(
                &mut entries,
                directory.path(),
                &cancellation,
                |_| match scenario {
                    0 => Ok(HashMap::from([("wrongvideo1".to_owned(), 1_709_164_800)])),
                    1 => Ok(HashMap::from([(entry.id.clone(), -1)])),
                    2 => {
                        cancellation.cancel();
                        Ok(HashMap::from([(entry.id.clone(), 1_709_164_800)]))
                    }
                    _ => Ok(HashMap::new()),
                },
                |_| match scenario {
                    3 => Err(PlaybackError::Protocol("no date fixture".to_owned())),
                    4 => Ok(-1),
                    _ => panic!("invalid or cancelled batch must not fall back"),
                },
            );
            assert!(result.is_err(), "scenario {scenario}");
            assert_eq!(entries[0].published_at, None);
            assert_eq!(load_publication_date(directory.path(), &entry.id), None);
            cancellation.cancel();
            assert!(
                client
                    .populate_youtube_publication_dates_with_batch_lookup(
                        &mut entries,
                        directory.path(),
                        &cancellation,
                        |_| panic!("no batch after cancellation"),
                        |_| panic!("no fallback after cancellation"),
                    )
                    .is_err()
            );
        }
    }

    #[test]
    fn publication_dates_survive_flat_collection_conversion() {
        let raw: ExtractedCollectionJson = serde_json::from_str(r#"{"id":"playlist","entries":[{"id":"video1","timestamp":1709164800},{"id":"video2","upload_date":"20240229"},{"id":"video3","upload_date":"20230229"}]}"#).expect("flat metadata");
        let collection = ExtractedCollection::try_from(raw).expect("collection");
        assert_eq!(
            collection
                .entries
                .iter()
                .map(|entry| entry.published_at)
                .collect::<Vec<_>>(),
            [Some(1_709_164_800), Some(1_709_164_800), None]
        );
    }

    #[cfg(unix)]
    #[test]
    fn publication_date_lookup_refetches_bad_cache_and_keeps_private_result() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("date fixture");
        let cache_dir = directory.path().join("cache");
        std::fs::create_dir(&cache_dir).expect("cache directory");
        std::fs::write(cache_dir.join("jNQXAC9IVRw.json"), "broken cache")
            .expect("broken cache fixture");
        let executable = directory.path().join("metadata-helper");
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' '{\"id\":\"jNQXAC9IVRw\",\"timestamp\":1114313512}'\n",
        )
        .expect("helper fixture");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
            .expect("executable fixture");
        let client = YtDlp::new(YtDlpConfig {
            executable: executable.clone(),
            ..YtDlpConfig::default()
        });
        let mut entries = [CollectionEntry {
            id: "jNQXAC9IVRw".to_owned(),
            title: "Test".to_owned(),
            webpage_url: None,
            duration_seconds: None,
            thumbnail_url: None,
            published_at: None,
        }];
        let cancellation = YouTubePrewarmCancellation::new();
        client
            .populate_youtube_publication_dates(&mut entries, &cache_dir, &cancellation)
            .expect("fresh date");
        assert_eq!(entries[0].published_at, Some(1_114_313_512));
        assert_eq!(
            load_publication_date(&cache_dir, "jNQXAC9IVRw"),
            Some(1_114_313_512)
        );
        assert_eq!(
            std::fs::metadata(cache_dir.join("jNQXAC9IVRw.json"))
                .expect("cache file")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&cache_dir)
                .expect("cache directory")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        std::fs::remove_file(executable).expect("remove fixture helper");
        entries[0].published_at = None;
        client
            .populate_youtube_publication_dates(&mut entries, &cache_dir, &cancellation)
            .expect("cached date without helper");
        assert!(!cancellation.is_cancelled());
    }

    #[test]
    fn publication_dates_keep_real_metadata_and_validate_calendar_dates() {
        let date = |json: &str| {
            serde_json::from_str::<PublicationDateJson>(json)
                .expect("date JSON")
                .published_at()
        };
        assert_eq!(
            date(r#"{"release_timestamp":1709164801,"timestamp":1709164800}"#),
            Some(1_709_164_801)
        );
        assert_eq!(
            date(r#"{"timestamp":1709164800,"upload_date":"20240228"}"#),
            Some(1_709_164_800)
        );
        assert_eq!(date(r#"{"upload_date":"20240229"}"#), Some(1_709_164_800));
        for json in [
            r#"{}"#,
            r#"{"upload_date":"20230229"}"#,
            r#"{"upload_date":"20241301"}"#,
            r#"{"upload_date":"2024022"}"#,
            r#"{"timestamp":-1}"#,
            r#"{"timestamp":9223372036854775807}"#,
        ] {
            assert_eq!(date(json), None, "{json}");
        }
    }

    #[test]
    fn publication_date_output_requires_matching_id_and_actual_date() {
        assert_eq!(
            parse_youtube_publication_date(
                br#"{"id":"jNQXAC9IVRw","upload_date":"20050424"}"#,
                "jNQXAC9IVRw"
            )
            .expect("matching metadata"),
            1_114_300_800,
        );
        assert!(
            parse_youtube_publication_date(
                br#"{"id":"wrong","timestamp":1114313512}"#,
                "jNQXAC9IVRw"
            )
            .is_err()
        );
        assert!(parse_youtube_publication_date(br#"{"id":"jNQXAC9IVRw"}"#, "jNQXAC9IVRw").is_err());
    }

    #[test]
    fn publication_date_command_skips_media_and_uses_bounded_metadata_fields() {
        let command = build_publication_date_command(&YtDlpConfig::default(), "jNQXAC9IVRw");
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        for required in [
            "--ignore-config",
            "--no-plugin-dirs",
            "--skip-download",
            "--ignore-no-formats-error",
            "--no-check-formats",
            "--no-playlist",
        ] {
            assert!(args.iter().any(|arg| arg == required), "{required}");
        }
        assert!(args.windows(2).any(|pair| pair
            == [
                "--extractor-args",
                "youtube:player_client=web;player_skip=configs,js;skip=hls,dash;webpage_skip="
            ]));
        assert!(args.windows(2).any(|pair| pair
            == [
                "--print",
                "%(.{id,timestamp,release_timestamp,upload_date})j"
            ]));
        assert_eq!(
            args.last().map(String::as_str),
            Some("https://www.youtube.com/watch?v=jNQXAC9IVRw")
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("approximate_date") || arg.contains("cookies"))
        );
    }

    #[test]
    fn publication_date_cache_hit_avoids_unavailable_helper() {
        let directory = tempfile::tempdir().expect("date cache");
        store_publication_date(directory.path(), "jNQXAC9IVRw", 1_114_313_512)
            .expect("cached date");
        let client = YtDlp::new(YtDlpConfig {
            executable: directory.path().join("missing-yt-dlp"),
            ..YtDlpConfig::default()
        });
        let mut entries = [CollectionEntry {
            id: "jNQXAC9IVRw".to_owned(),
            title: "Test".to_owned(),
            webpage_url: None,
            duration_seconds: None,
            thumbnail_url: None,
            published_at: None,
        }];
        client
            .populate_youtube_publication_dates(
                &mut entries,
                directory.path(),
                &YouTubePrewarmCancellation::new(),
            )
            .expect("cached lookup");
        assert_eq!(entries[0].published_at, Some(1_114_313_512));
    }

    #[test]
    fn publication_date_cache_rejects_wrong_id_and_invalid_dates() {
        let directory = tempfile::tempdir().expect("date cache");
        for contents in [
            r#"{"version":1,"id":"another-id","published_at":1114313512}"#,
            r#"{"version":9,"id":"jNQXAC9IVRw","published_at":1114313512}"#,
            r#"{"version":1,"id":"jNQXAC9IVRw","published_at":-1}"#,
            "broken JSON",
        ] {
            std::fs::write(directory.path().join("jNQXAC9IVRw.json"), contents)
                .expect("bad cache fixture");
            assert_eq!(load_publication_date(directory.path(), "jNQXAC9IVRw"), None);
        }
    }

    #[test]
    fn publication_date_lookup_honors_cancellation_before_cache_or_spawn() {
        let directory = tempfile::tempdir().expect("date cache");
        let cancellation = YouTubePrewarmCancellation::new();
        cancellation.cancel();
        let client = YtDlp::new(YtDlpConfig {
            executable: directory.path().join("missing-yt-dlp"),
            ..YtDlpConfig::default()
        });
        assert!(
            client
                .populate_youtube_publication_dates(&mut [], directory.path(), &cancellation)
                .expect_err("cancelled")
                .to_string()
                .contains("cancelled")
        );
    }

    #[test]
    fn base_command_disables_config_and_plugins_by_default() {
        let config = YtDlpConfig::default();
        let command = build_base_command(&config);
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            arguments,
            [
                "--ignore-config",
                "--no-plugin-dirs",
                "--js-runtimes",
                "quickjs"
            ]
        );
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

        assert_eq!(arguments, ["--ignore-config", "--js-runtimes", "quickjs"]);
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
            scope: DownloadScope::SingleItem,
            playlist_start: None,
            skip_shorts: false,
            write_thumbnail: true,
            archive_path: None,
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
            !arguments
                .iter()
                .any(|argument| argument == "--playlist-start")
        );
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
                collection_index: None,
                collection_count: None,
            })
        );
        assert_eq!(
            parse_download_event("youta-progress|1024|NA|5000|NA|NA"),
            Some(DownloadEvent::Progress {
                downloaded_bytes: 1024,
                total_bytes: Some(5000),
                bytes_per_second: None,
                eta_seconds: None,
                collection_index: None,
                collection_count: None,
            })
        );
        assert_eq!(
            parse_download_event("youta-progress|512|1024|NA|64|8|3|41"),
            Some(DownloadEvent::Progress {
                downloaded_bytes: 512,
                total_bytes: Some(1024),
                bytes_per_second: Some(64.0),
                eta_seconds: Some(8),
                collection_index: Some(3),
                collection_count: Some(41),
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
    fn collection_download_enables_playlist_traversal_and_a_restart_safe_archive() {
        let config = YtDlpConfig::default();
        let request = DownloadRequest {
            source_url: Url::parse("https://www.youtube.com/channel/UCfixture")
                .expect("channel URL"),
            destination: PathBuf::from("/tmp/youta-fixture-downloads"),
            format: DownloadFormat::OpusWithoutTranscoding,
            scope: DownloadScope::Collection,
            playlist_start: Some(17),
            skip_shorts: true,
            write_thumbnail: true,
            archive_path: None,
        };
        let command = build_download_command(&config, &request);
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(
            arguments
                .iter()
                .any(|argument| argument == "--yes-playlist")
        );
        assert!(!arguments.iter().any(|argument| argument == "--no-playlist"));
        // Joining uses native separators even when the fixture root has '/'.
        // Keep validating the entire archive path on Unix and Windows.
        assert!(arguments.windows(2).any(|pair| {
            pair[0] == "--download-archive"
                && Path::new(&pair[1]) == request.destination.join(".youta-download-archive")
        }));
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair[0] == "--playlist-start" && pair[1] == "17")
        );
        assert!(arguments.windows(2).any(|pair| {
            pair[0] == "--output" && pair[1].starts_with("%(channel).100B [%(channel_id)s]/")
        }));
        assert!(
            arguments.windows(2).any(|pair| {
                pair[0] == "--match-filters" && pair[1] == "original_url!*=/shorts/"
            })
        );

        let include_shorts_request = DownloadRequest {
            skip_shorts: false,
            ..request
        };
        let include_shorts_arguments = build_download_command(&config, &include_shorts_request)
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            !include_shorts_arguments
                .iter()
                .any(|argument| argument == "--match-filters")
        );
    }

    #[test]
    fn automatic_collection_check_skips_archived_items_without_stopping_at_them() {
        let config = YtDlpConfig::default();
        let request = DownloadRequest {
            source_url: Url::parse("https://www.youtube.com/channel/UCfixture")
                .expect("channel URL"),
            destination: PathBuf::from("/tmp/youta-fixture-downloads"),
            format: DownloadFormat::OpusWithoutTranscoding,
            scope: DownloadScope::Collection,
            playlist_start: None,
            skip_shorts: false,
            write_thumbnail: true,
            archive_path: Some(PathBuf::from(
                "/tmp/youta-fixture-downloads/.youta-auto-download/UCfixture.archive",
            )),
        };
        let arguments = build_download_command(&config, &request)
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(arguments.windows(2).any(|pair| {
            pair[0] == "--download-archive"
                && pair[1].ends_with(".youta-auto-download/UCfixture.archive")
        }));
        assert!(
            !arguments
                .iter()
                .any(|argument| argument == "--break-on-existing"),
            "archived newer uploads must not hide an older failed download on retry"
        );
    }

    #[test]
    fn automatic_collection_baseline_records_all_existing_items_without_media() {
        let config = YtDlpConfig::default();
        let request = DownloadRequest {
            source_url: Url::parse("https://www.youtube.com/channel/UCfixture")
                .expect("channel URL"),
            destination: PathBuf::from("/tmp/youta-fixture-downloads"),
            format: DownloadFormat::OpusWithoutTranscoding,
            scope: DownloadScope::CollectionArchiveOnly,
            playlist_start: None,
            skip_shorts: false,
            write_thumbnail: false,
            archive_path: Some(PathBuf::from("/tmp/UCfixture.archive")),
        };
        let arguments = build_download_command(&config, &request)
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(arguments.iter().any(|argument| argument == "--simulate"));
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "--force-write-archive")
        );
        assert!(
            !arguments
                .iter()
                .any(|argument| argument == "--playlist-end"),
            "baseline must also remember older uploads when the newest disappears"
        );
        assert!(!arguments.iter().any(|argument| argument == "--remux-video"));
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
			"thumbnails": [
				{"url": "https://images.example/banner.jpg", "width": 1280, "height": 240},
				{"url": "https://images.example/avatar.jpg", "width": 900, "height": 900}
			],
			"entries": [
				{
					"id": "entry-1",
					"title": "First",
					"webpage_url": "https://media.example/watch/entry-1",
					"duration": 90,
					"thumbnails": [{"url": "https://images.example/one.jpg"}]
				}
			]
		}"#;
        let extracted: ExtractedCollectionJson = serde_json::from_str(fixture).expect("fixture");
        let collection = ExtractedCollection::try_from(extracted).expect("collection");

        assert_eq!(collection.title, "Mock channel");
        assert_eq!(collection.entries.len(), 1);
        assert_eq!(
            collection.thumbnail_url.as_ref().map(Url::as_str),
            Some("https://images.example/avatar.jpg")
        );
        assert_eq!(collection.entries[0].duration_seconds, Some(90));
        assert_eq!(
            collection.entries[0]
                .thumbnail_url
                .as_ref()
                .map(Url::as_str),
            Some("https://images.example/one.jpg")
        );
    }

    #[test]
    fn parses_collection_with_null_titles_without_losing_episode_metadata() {
        let fixture = r#"{
            "id": "UCfixture", "title": null,
            "thumbnail": "https://images.example/avatar.jpg",
            "entries": [{
                "id": "videos", "title": null,
                "entries": [
                    {
                        "id": "aaaaaaaaaaa", "title": null,
                        "url": "https://www.youtube.com/watch?v=aaaaaaaaaaa",
                        "duration": 90,
                        "thumbnails": [{"url": "https://images.example/episode.jpg"}],
                        "timestamp": 1709164800
                    },
                    {"id": "bbbbbbbbbbb", "timestamp": null, "upload_date": null},
                    {"id": "ccccccccccc", "title": "", "upload_date": "20240229"},
                    {"id": "ddddddddddd", "title": "Named episode", "release_timestamp": 1709164801}
                ]
            }]
        }"#;
        let extracted: ExtractedCollectionJson =
            serde_json::from_str(fixture).expect("nullable flat-playlist titles");
        let collection = ExtractedCollection::try_from(extracted).expect("collection");

        assert_eq!(collection.id, "UCfixture");
        assert!(collection.title.is_empty());
        assert_eq!(
            collection.thumbnail_url.as_ref().map(Url::as_str),
            Some("https://images.example/avatar.jpg")
        );
        assert_eq!(
            collection
                .entries
                .iter()
                .map(|entry| (entry.id.as_str(), entry.title.as_str(), entry.published_at))
                .collect::<Vec<_>>(),
            [
                ("aaaaaaaaaaa", "", Some(1_709_164_800)),
                ("bbbbbbbbbbb", "", None),
                ("ccccccccccc", "", Some(1_709_164_800)),
                ("ddddddddddd", "Named episode", Some(1_709_164_801)),
            ]
        );
        let first = &collection.entries[0];
        assert_eq!(first.duration_seconds, Some(90));
        assert_eq!(
            first.webpage_url.as_ref().map(Url::as_str),
            Some("https://www.youtube.com/watch?v=aaaaaaaaaaa")
        );
        assert_eq!(
            first.thumbnail_url.as_ref().map(Url::as_str),
            Some("https://images.example/episode.jpg")
        );
    }

    #[test]
    fn collection_nullable_titles_do_not_relax_other_metadata_types() {
        for fixture in [
            r#"{"id":null}"#,
            r#"{"title":"Missing collection ID"}"#,
            r#"{"id":"channel","entries":[{"id":null}]}"#,
            r#"{"id":"channel","entries":[{"title":"Missing episode ID"}]}"#,
            r#"{"id":"channel","entries":[{"id":null,"entries":[{"id":"aaaaaaaaaaa"}]}]}"#,
            r#"{"id":"channel","entries":[{"entries":[{"id":"aaaaaaaaaaa"}]}]}"#,
            r#"{"id":"channel","title":1}"#,
            r#"{"id":"channel","title":{}}"#,
            r#"{"id":"channel","entries":[{"id":"aaaaaaaaaaa","title":false}]}"#,
            r#"{"id":"channel","entries":[{"id":"aaaaaaaaaaa","title":[]}]}"#,
            r#"{"id":"channel","entries":[{"id":"videos","title":{},"entries":[{"id":"aaaaaaaaaaa"}]}]}"#,
            r#"{"id":"channel","entries":[{"id":"aaaaaaaaaaa","timestamp":"1709164800"}]}"#,
            r#"{"id":"channel","entries":[{"id":"aaaaaaaaaaa","upload_date":20240229}]}"#,
            r#"{"id":"channel","thumbnails":[{"url":null}]}"#,
        ] {
            assert!(
                serde_json::from_str::<ExtractedCollectionJson>(fixture).is_err(),
                "invalid metadata must remain rejected: {fixture}"
            );
        }
    }

    #[test]
    fn parses_collection_with_extractor_and_extractor_key() {
        let fixture = r#"{
			"id": "collection-1",
			"title": "Mock channel",
			"extractor": "youtube:tab",
			"extractor_key": "YoutubeTab",
			"entries": []
		}"#;

        let extracted: ExtractedCollectionJson =
            serde_json::from_str(fixture).expect("yt-dlp collection metadata");
        let collection = ExtractedCollection::try_from(extracted).expect("collection");

        assert_eq!(collection.extractor.as_deref(), Some("YoutubeTab"));
    }

    #[test]
    fn youtube_channel_catalogue_keeps_upload_order_and_channel_short_metadata() {
        let channel_json = r#"{
            "id": "UCfixture", "title": "Fixture channel",
            "thumbnail": "https://images.example/avatar.jpg",
            "entries": [
                {"id": "videos", "entries": [
                    {"id": "new-video", "title": "New video"},
                    {"id": "old-video", "title": "Old video"}
                ]},
                {"id": "shorts", "entries": [
                    {"id": "new-short", "url": "https://www.youtube.com/shorts/new-short"},
                    {"id": "selected-short", "url": "https://www.youtube.com/shorts/selected-short"},
                    {"id": "old-short", "url": "https://www.youtube.com/shorts/old-short"}
                ]}
            ]
        }"#;
        let uploads_json = r#"{
            "id": "UUfixture", "title": "Uploads from Fixture channel",
            "thumbnail": "https://images.example/first-video.jpg",
            "entries": [
                {"id": "new-short", "title": "New Short", "url": "https://www.youtube.com/watch?v=new-short"},
                {"id": "new-video", "title": "New video", "url": "https://www.youtube.com/watch?v=new-video"},
                {"id": "selected-short", "title": "Selected Short", "url": "https://www.youtube.com/watch?v=selected-short"},
                {"id": "old-video", "title": "Old video", "url": "https://www.youtube.com/watch?v=old-video"},
                {"id": "old-short", "title": "Old Short", "url": "https://www.youtube.com/watch?v=old-short"},
                {"id": "new-video", "title": "Duplicate video"}
            ]
        }"#;
        let channel: ExtractedCollectionJson =
            serde_json::from_str(channel_json).expect("channel fixture");
        let uploads: ExtractedCollectionJson =
            serde_json::from_str(uploads_json).expect("uploads fixture");
        let collection = merge_youtube_channel_collections(
            channel.try_into().expect("channel"),
            uploads.try_into().expect("uploads"),
        );

        assert_eq!(collection.title, "Fixture channel");
        assert_eq!(
            collection.thumbnail_url.as_ref().map(Url::as_str),
            Some("https://images.example/avatar.jpg")
        );
        assert_eq!(
            collection
                .entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            [
                "new-short",
                "new-video",
                "selected-short",
                "old-video",
                "old-short"
            ]
        );
        assert_eq!(
            collection.entries[0].webpage_url.as_ref().map(Url::as_str),
            Some("https://www.youtube.com/shorts/new-short")
        );
        assert_eq!(
            collection.entries[1].webpage_url.as_ref().map(Url::as_str),
            Some("https://www.youtube.com/watch?v=new-video")
        );
        assert_eq!(collection.entries[1].title, "New video");
        assert_eq!(
            collection.entries[2].webpage_url.as_ref().map(Url::as_str),
            Some("https://www.youtube.com/shorts/selected-short")
        );
    }

    #[test]
    fn youtube_channel_catalogue_limits_metadata_work_when_shorts_are_not_filtered() {
        for (classify_shorts, channel_limit) in [(false, 1), (true, 2000)] {
            let mut calls = Vec::new();
            youtube_channel_collection_with("UCfixture", 2000, classify_shorts, |url, limit| {
                calls.push((url.to_string(), limit));
                Ok(ExtractedCollection {
                    id: "fixture".to_owned(),
                    title: "Fixture".to_owned(),
                    extractor: None,
                    thumbnail_url: None,
                    entries: Vec::new(),
                })
            })
            .expect("channel catalogue");
            assert_eq!(
                calls,
                [
                    (
                        "https://www.youtube.com/channel/UCfixture".to_owned(),
                        channel_limit
                    ),
                    (
                        "https://www.youtube.com/playlist?list=UUfixture".to_owned(),
                        2000
                    ),
                ]
            );
        }
    }

    #[test]
    fn youtube_channel_catalogue_rejects_invalid_ids_and_zero_limits_before_extraction() {
        for (channel_id, limit) in [
            ("", 20),
            ("UC", 20),
            ("@fixture", 20),
            ("UCfixture/shorts", 20),
            ("UCfixture?list=x", 20),
            ("UCfixture", 0),
        ] {
            let result = youtube_channel_collection_with(channel_id, limit, false, |_, _| {
                panic!("invalid catalogue request must not execute yt-dlp")
            });
            assert!(
                matches!(result, Err(PlaybackError::InvalidValue(_))),
                "{channel_id:?}"
            );
        }
    }

    #[test]
    fn flattens_nested_youtube_channel_tabs() {
        let fixture = r#"{
			"id": "UCfixture",
			"title": "Fixture channel",
			"extractor": "youtube:tab",
			"extractor_key": "YoutubeTab",
			"entries": [
				{
					"id": "UCfixture",
					"title": "Fixture channel - Videos",
					"entries": [
						{"id": "dQw4w9WgXcQ", "title": "First video"},
						{"id": "M7lc1UVf-VE", "title": "Selected video"}
					]
				},
				{
					"id": "UCfixture",
					"title": "Fixture channel - Shorts",
					"entries": [
						{"id": "aqz-KE-bpKQ", "title": "One short"}
					]
				}
			]
		}"#;

        let extracted: ExtractedCollectionJson =
            serde_json::from_str(fixture).expect("nested yt-dlp channel metadata");
        let collection = ExtractedCollection::try_from(extracted).expect("collection");

        assert_eq!(
            collection
                .entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["dQw4w9WgXcQ", "M7lc1UVf-VE", "aqz-KE-bpKQ"]
        );
    }
}
