//! Application controller connecting providers, persistence, playback, and TUI.
//!
//! Provider requests run on one blocking worker thread. Local directory
//! listings and selected-video `YouTube` prewarming use dedicated workers; the
//! prewarm queue is latest-only and bounded. Recursive scans, remote metadata,
//! and speculative extraction therefore cannot delay foreground navigation.
//! The terminal event loop never waits for these responses, while the process
//! avoids an asynchronous runtime and its additional idle bookkeeping.

use std::collections::{HashMap, HashSet, VecDeque};
#[cfg(feature = "yt-dlp")]
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
#[cfg(feature = "yt-dlp")]
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Datelike, Local, NaiveDate};
use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
#[cfg(feature = "yt-dlp")]
use crossbeam_channel::{TrySendError, bounded};
use unicode_segmentation::UnicodeSegmentation;

use crate::config::{
    Config, LOCAL_FOLDER_SIZES_ENV, SKIP_ADVERTISEMENT_CHAPTERS_ENV, SUBSCRIPTIONS_LAYOUT_ENV,
    SubscriptionsLayout, YOUTUBE_PREWARM_ENV, YouTubeBackend, YouTubeProviderSetting,
};
use crate::diagnostics::{DiagnosticReport, ExternalHelper, ExternalHelperKind};
use crate::domain::{
    Chapter, HistoryEntry, MediaId, MediaItem, MediaKind, MediaLicense, MediaStatistics,
    PanelFocus, PlaybackProgress, PlaybackQueue, QueueItem, Screen as StoredScreen, SessionState,
    SourceKind,
};
use crate::links::{
    LinkTarget, is_advertisement_chapter_title, normalize_description_chapter_lines,
    parse_description_chapters, parse_description_links, parse_description_video_links,
    parse_youtube_url,
};
#[cfg(feature = "wikidata")]
use crate::persistence::CachedWikidataLookup;
#[cfg(feature = "youtube-music")]
use crate::persistence::SavedYouTubeMusicSearch;
use crate::persistence::{
    CachedSubscriptionItems, MAX_SAVED_SUBSCRIPTION_ITEMS, MAX_SAVED_YOUTUBE_SEARCH_RESULTS,
    SavedYouTubeSearch, StateStore,
};
#[cfg(feature = "yt-dlp")]
use crate::playback::youtube_prewarm::{
    PrewarmedYouTubeAudio, YouTubePrewarmCancellation, YouTubePrewarmConfig, YouTubePrewarmError,
    YouTubePrewarmRequest, YouTubePrewarmResolver, YouTubePrewarmResult,
};
#[cfg(feature = "yt-dlp")]
use crate::playback::ytdlp::{
    DownloadFormat, DownloadProcess, DownloadRequest, YtDlp, YtDlpConfig, parse_download_event,
};
use crate::playback::{
    PlaybackBackend, PlaybackEnd, PlaybackEndReason, PlaybackError, PlaybackEvent, PlaybackInput,
    PlaybackStatus, PlayerCommand, Result as PlaybackResult,
};
#[cfg(feature = "youtube-music")]
use crate::providers::youtube_music::{
    YouTubeMusicSearch, YouTubeMusicSearchConfig, YouTubeMusicTrack,
};
use crate::providers::{
    ChannelDetails, ChannelExternalLinkKind, ChannelStatisticsMode, ChannelSubscriberCount,
    ChannelSummary, ChannelVideosRequest, Provider, SearchFeature, SearchItem, SearchPage,
    SearchRequest, SearchSort as ProviderSearchSort, SearchTarget, Thumbnail, VideoDetails,
    VideoOrientation, VideoSummary, invidious_youtube_provider, official_youtube_provider,
    validate_youtube_video_id,
};
use crate::report_actions::SystemReportActions;
#[cfg(test)]
use crate::subscriptions::SubscriptionNode;
use crate::subscriptions::{self, FlattenedSubscription, SubscriptionKind, SubscriptionTree};
use crate::tui::DetailLinkView;
#[cfg(feature = "yt-dlp")]
use crate::tui::DownloadView;
use crate::tui::{
    DetailTimecodeView, DetailVideoLinkView, DetailView, DetailsScroll, DetailsTextSelection,
    ErrorPopupScroll, ErrorPopupView, GOOGLE_CLOUD_CREDENTIALS_URL, INVIDIOUS_INSTANCES_URL,
    LocalFilePopupView, LocalSizeSort, MAX_DETAILS_SELECTION_BYTES, PreferencesPopupView,
    RightPanelMode, RowView, RssSubscriptionPopupView, Screen, SearchActivity, SearchKind,
    SubscriptionPane, SubscriptionRoute, UiAction, UiController, ViewModel,
    YOUTUBE_API_KEY_GUIDE_URL, YouTubeSearchSort, YouTubeSetupField, YouTubeSetupPopupView,
};
#[cfg(feature = "wikidata")]
use crate::tui::{DetailWikidataEntityView, DetailWikidataValueLinkView};

/// Truncates a clipboard payload without splitting a UTF-8 character.
fn truncate_utf8_bytes(value: &mut String, maximum_bytes: usize) {
    if value.len() <= maximum_bytes {
        return;
    }
    let mut boundary = maximum_bytes;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}

/// Clamps an editable basename cursor to a preceding grapheme boundary.
fn rename_cursor_boundary(value: &str, requested: usize) -> usize {
    let requested = requested.min(value.len());
    if requested == value.len() {
        return requested;
    }
    value
        .grapheme_indices(true)
        .map(|(index, _)| index)
        .take_while(|index| *index <= requested)
        .last()
        .unwrap_or_default()
}

/// Factory used to start a playback engine only when the user presses Play.
///
/// Delaying process creation keeps startup fast and avoids an idle decoder for
/// users who only browse subscriptions or metadata.
pub type PlaybackFactory =
    Box<dyn FnMut() -> PlaybackResult<Box<dyn PlaybackBackend>> + Send + 'static>;

trait YouTubeProviderBuilder: Send {
    fn official(&self, api_key: String) -> Result<Box<dyn Provider>, String>;
    fn invidious(&self, base_url: url::Url) -> Result<Box<dyn Provider>, String>;
}

struct SystemYouTubeProviderBuilder;

impl YouTubeProviderBuilder for SystemYouTubeProviderBuilder {
    fn official(&self, api_key: String) -> Result<Box<dyn Provider>, String> {
        official_youtube_provider(api_key).map_err(|error| error.to_string())
    }

    fn invidious(&self, base_url: url::Url) -> Result<Box<dyn Provider>, String> {
        invidious_youtube_provider(base_url).map_err(|error| error.to_string())
    }
}

trait DiagnosticActionHandler {
    fn gh_available(&self) -> bool;
    fn copy_report(&self, report: &str) -> Result<String, String>;
    fn fill_github_issue(&self, title: &str, report: &str) -> Result<(), String>;
    fn copy_and_open_github_issue(&self, title: &str, report: &str) -> Result<String, String>;
}

impl DiagnosticActionHandler for SystemReportActions {
    fn gh_available(&self) -> bool {
        SystemReportActions::gh_available(self)
    }

    fn copy_report(&self, report: &str) -> Result<String, String> {
        SystemReportActions::copy_report(self, report).map_err(|error| error.to_string())
    }

    fn fill_github_issue(&self, title: &str, report: &str) -> Result<(), String> {
        SystemReportActions::fill_github_issue(self, title, report)
            .map_err(|error| error.to_string())
    }

    fn copy_and_open_github_issue(&self, title: &str, report: &str) -> Result<String, String> {
        SystemReportActions::copy_and_open_github_issue(self, title, report)
            .map_err(|error| error.to_string())
    }
}

/// Search subsystem selected by a top-level screen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchRoute {
    /// The normal search screen queries only the configured YouTube provider.
    YouTube,
    /// The music tab queries `music.youtube.com` through the installed `yt-dlp`.
    YouTubeMusic,
    /// The dedicated tracker screen queries only module archives.
    TrackerArchives,
    /// The screen does not perform remote search.
    None,
}

/// A direct YouTube video reference accepted by the normal input box.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectVideoInput {
    /// Validated eleven-character video identifier.
    pub video_id: String,
    /// Optional initial seek position parsed from the link.
    pub start_seconds: Option<u64>,
}

/// A non-YouTube URL routed to a first-class or generic direct adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectSourceInput {
    /// Validated credential-free HTTP(S) URL.
    pub url: url::Url,
    /// Source selected from the URL host.
    pub source: SourceKind,
}

/// A local file or directory accepted by the unified input box.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectLocalInput {
    /// Canonical absolute path. Media is read in place and never moved.
    pub path: PathBuf,
    /// Whether the path is a directory that must be scanned on the worker.
    pub directory: bool,
}

/// Error returned while expanding or validating a local path.
#[derive(Debug, thiserror::Error)]
pub enum LocalInputError {
    /// `~` was requested but the platform home directory is unavailable.
    #[error("cannot expand `~` because the home directory is unavailable")]
    HomeUnavailable,
    /// `~name` expansion is intentionally unsupported because it can target a
    /// different user's files.
    #[error("only `~` and `~/…` home paths are supported")]
    UnsupportedTilde,
    /// A `file://` URL did not map to a path on this platform.
    #[error("the file URL cannot be converted to a local path")]
    InvalidFileUrl,
    /// The selected path could not be inspected.
    #[error("cannot access local path `{path}`: {source}")]
    Io {
        /// User-facing path involved in the failed operation.
        path: PathBuf,
        /// Operating-system error.
        #[source]
        source: std::io::Error,
    },
}

/// Expands and validates an explicit local path from the unified input box.
///
/// Accepted forms are absolute paths, `./…`, `../…`, `~`, `~/…`, `file://`
/// URLs, and existing relative paths containing a path separator. Plain search
/// words remain YouTube queries.
///
/// # Errors
///
/// Returns [`LocalInputError`] when an explicit path cannot be expanded,
/// canonicalized, or inspected.
pub fn parse_local_path_input(raw: &str) -> Result<Option<DirectLocalInput>, LocalInputError> {
    let home = directories::BaseDirs::new().map(|directories| directories.home_dir().to_owned());
    let current = std::env::current_dir().map_err(|source| LocalInputError::Io {
        path: PathBuf::from("."),
        source,
    })?;
    parse_local_path_input_from(raw, home.as_deref(), &current)
}

fn parse_local_path_input_from(
    raw: &str,
    home: Option<&Path>,
    current: &Path,
) -> Result<Option<DirectLocalInput>, LocalInputError> {
    let input = raw.trim();
    if input.is_empty() {
        return Ok(None);
    }
    let raw_path = if input.starts_with("file://") {
        let url = url::Url::parse(input).map_err(|_| LocalInputError::InvalidFileUrl)?;
        url.to_file_path()
            .map_err(|()| LocalInputError::InvalidFileUrl)?
    } else if input == "~" {
        home.ok_or(LocalInputError::HomeUnavailable)?.to_owned()
    } else if let Some(rest) = input.strip_prefix("~/") {
        home.ok_or(LocalInputError::HomeUnavailable)?.join(rest)
    } else if input.starts_with('~') {
        return Err(LocalInputError::UnsupportedTilde);
    } else {
        let path = PathBuf::from(input);
        let explicit = path.is_absolute()
            || input.starts_with("./")
            || input.starts_with("../")
            || ((input.contains('/') || input.contains('\\')) && current.join(&path).exists());
        if !explicit {
            return Ok(None);
        }
        if path.is_absolute() {
            path
        } else {
            current.join(path)
        }
    };
    let path = std::fs::canonicalize(&raw_path).map_err(|source| LocalInputError::Io {
        path: raw_path.clone(),
        source,
    })?;
    let metadata = std::fs::metadata(&path).map_err(|source| LocalInputError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(Some(DirectLocalInput {
        path,
        directory: metadata.is_dir(),
    }))
}

#[derive(Clone, Debug)]
struct ResolvedDirectMedia {
    /// Stable source identity used by history and resume state.
    source: SourceKind,
    /// Stable provider-specific media identifier.
    external_id: String,
    /// Display title returned by the first-class provider.
    title: String,
    /// Compact source-specific metadata shown in the result row.
    row_subtitle: String,
    /// Provider description plus relevant first-class metadata.
    description: String,
    /// Provider-reported license or terms label.
    license: String,
    /// Provider publication date or timestamp, when present.
    published: Option<String>,
    /// Artwork URL advertised by the provider, when present.
    artwork_url: Option<url::Url>,
    duration_seconds: Option<u64>,
    /// Explicit public media URL. Token-gated and inferred URLs stay absent.
    playback_url: Option<url::Url>,
    /// Canonical public page for browser navigation.
    webpage_url: Option<url::Url>,
    /// Resolution outcome shown both immediately and after an unavailable play.
    status_line: String,
}

#[derive(Clone, Debug)]
struct TrackerItem {
    source: String,
    title: String,
    subtitle: String,
    webpage_url: url::Url,
    #[cfg_attr(
        not(feature = "tracker-music"),
        allow(
            dead_code,
            reason = "remote preparation is unavailable without tracker-music"
        )
    )]
    download_url: Option<url::Url>,
    #[cfg_attr(
        not(feature = "tracker-music"),
        allow(
            dead_code,
            reason = "format hints are consumed only by tracker preparation"
        )
    )]
    expected_format: Option<String>,
    access: TrackerAccess,
    prepared_path: Option<PathBuf>,
    insecure_transport: bool,
}

#[cfg_attr(
    not(feature = "tracker-music"),
    allow(
        dead_code,
        reason = "tracker result access kinds are constructed by tracker providers"
    )
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrackerAccess {
    DirectModule,
    ArchiveNeedsInspection,
    MetadataOnly,
    Directory,
}

#[derive(Clone, Debug)]
struct LocalMediaItem {
    path: PathBuf,
    title: String,
    artist: Option<String>,
    album: Option<String>,
    genre: Option<String>,
    comment: Option<String>,
    metadata_url: Option<String>,
    acoustid_id: Option<String>,
    duration_seconds: Option<u64>,
    size_bytes: u64,
    codec: String,
    bitrate_kbps: Option<u32>,
    sample_rate_hz: Option<u32>,
    channels: Option<u8>,
    embedded_artwork: bool,
}

/// One process-local folder-size result valid only for its Local generation.
#[derive(Clone, Debug)]
struct CachedLocalFolderSize {
    /// Filesystem identity preventing a result from surviving path replacement.
    identity: crate::local_browser::LocalDirectoryIdentity,
    /// Complete recursive logical size.
    bytes: u64,
    /// Time of the last complete traversal; cached validation does not renew it.
    measured_at: Instant,
    /// Local generation in which a background identity check last succeeded.
    validated_generation: u64,
}

/// One bounded negative cache entry suppressing repeated failed traversals.
struct CachedLocalFolderSizeFailure {
    /// Directory identity from the listing that scheduled the failed work.
    identity: Option<crate::local_browser::LocalDirectoryIdentity>,
    /// Time after which the directory may be attempted again.
    failed_at: Instant,
}

/// Background result distinguishing a traversal from cheap cache validation.
struct LocalFolderSizeWorkerResult {
    /// Complete size and stable real-directory identity.
    measurement: crate::local_browser::LocalFolderSizeMeasurement,
    /// Whether the worker reused a cached traversal after checking identity.
    reused: bool,
}

/// Lazy artwork lookup selected for one Local Details target.
#[cfg(all(feature = "local", feature = "thumbnails"))]
#[derive(Clone, Copy)]
enum LocalArtworkKind {
    /// Extracts bounded embedded artwork from a playable media file.
    EmbeddedMedia,
    /// Discovers a conventional cover file without opening image contents.
    FolderCover,
}

/// Parses a bare YouTube ID or an official YouTube video URL.
///
/// Non-URL text returns `Ok(None)` and remains an ordinary search query.
/// YouTube-looking but malformed input returns an error so look-alike hosts and
/// broken identifiers are not sent to search accidentally.
///
/// # Errors
///
/// Returns a short validation message for malformed or non-video YouTube URLs.
pub fn parse_direct_youtube_input(raw: &str) -> Result<Option<DirectVideoInput>, &'static str> {
    let input = raw.trim();
    if input.is_empty() {
        return Ok(None);
    }
    if validate_youtube_video_id(input).is_ok() {
        return Ok(Some(DirectVideoInput {
            video_id: input.to_owned(),
            start_seconds: None,
        }));
    }

    let lower = input.to_ascii_lowercase();
    let looks_like_youtube = lower.contains("youtube.") || lower.contains("youtu.be");
    if !looks_like_youtube {
        return Ok(None);
    }
    let normalized = if lower.starts_with("youtube.")
        || lower.starts_with("www.youtube.")
        || lower.starts_with("music.youtube.")
        || lower.starts_with("m.youtube.")
        || lower.starts_with("youtu.be")
    {
        format!("https://{input}")
    } else {
        input.to_owned()
    };
    let url = url::Url::parse(&normalized).map_err(|_| "invalid YouTube URL")?;
    let Some(LinkTarget::YouTubeVideo {
        video_id,
        start_seconds,
    }) = parse_youtube_url(&url)
    else {
        return Err("input is not a valid YouTube video URL");
    };
    Ok(Some(DirectVideoInput {
        video_id,
        start_seconds,
    }))
}

/// Parses a direct non-YouTube source URL.
///
/// Known hosts receive a first-class source identity. Other credential-free
/// HTTP(S) links use the generic yt-dlp route, whose installed extractor list
/// determines whether playback is supported.
///
/// # Errors
///
/// Returns an error for credential-bearing, non-HTTP, or malformed URL-like
/// input. Ordinary free text returns `Ok(None)`.
pub fn parse_direct_source_input(raw: &str) -> Result<Option<DirectSourceInput>, &'static str> {
    let input = raw.trim();
    if input.is_empty() || input.chars().any(char::is_whitespace) {
        return Ok(None);
    }
    let lower = input.to_ascii_lowercase();
    let normalized = if lower.starts_with("http://") || lower.starts_with("https://") {
        input.to_owned()
    } else if looks_like_bare_host_path(input) {
        format!("https://{input}")
    } else {
        return Ok(None);
    };
    let url = url::Url::parse(&normalized).map_err(|_| "invalid source URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
    {
        return Err("source URL must be credential-free HTTP or HTTPS");
    }
    if parse_youtube_url(&url).is_some()
        || url
            .host_str()
            .is_some_and(|host| host.ends_with("youtube.com") || host.ends_with("youtu.be"))
    {
        return Err("input is not a valid YouTube video URL");
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let source = classify_known_host(&host, url.path());
    Ok(Some(DirectSourceInput { url, source }))
}

fn looks_like_bare_host_path(input: &str) -> bool {
    let authority = input.split('/').next().unwrap_or_default();
    authority.contains('.')
        && !authority.starts_with('.')
        && !authority.ends_with('.')
        && !authority.contains('@')
}

fn host_matches(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

fn classify_known_host(host: &str, path: &str) -> SourceKind {
    if is_supported_media_path(path) {
        SourceKind::RemoteFiles
    } else if host_matches(host, "podcasts.apple.com") {
        SourceKind::ApplePodcasts
    } else if host_matches(host, "soundcloud.com") {
        SourceKind::SoundCloud
    } else if host_matches(host, "jamendo.com") || host_matches(host, "jamen.do") {
        SourceKind::Jamendo
    } else if host_matches(host, "soundstream.media") {
        SourceKind::SoundStream
    } else if host_matches(host, "litres.ru") {
        SourceKind::LitRes
    } else if host_matches(host, "vimeo.com") {
        SourceKind::Vimeo
    } else if host_matches(host, "rutube.ru") {
        SourceKind::RuTube
    } else if host_matches(host, "bandcamp.com") {
        SourceKind::Bandcamp
    } else if host_matches(host, "odysee.com") {
        SourceKind::Odysee
    } else if host_matches(host, "rumble.com") {
        SourceKind::Rumble
    } else if host_matches(host, "bilibili.com") || host == "b23.tv" {
        SourceKind::Bilibili
    } else if host_matches(host, "vk.com") {
        SourceKind::Vk
    } else if host_matches(host, "archive.org") {
        SourceKind::ArchiveOrg
    } else if host_matches(host, "librivox.org") {
        SourceKind::LibriVox
    } else if host == "commons.wikimedia.org" {
        SourceKind::WikimediaCommons
    } else if host_matches(host, "music.yandex.ru") {
        SourceKind::YandexMusic
    } else if host_matches(host, "bbc.co.uk") || host_matches(host, "bbc.com") {
        SourceKind::BbcRadio
    } else if host_matches(host, "modarchive.org")
        || host_matches(host, "scene.org")
        || host_matches(host, "aminet.net")
        || host_matches(host, "modland.com")
        || host_matches(host, "dascene.net")
        || host_matches(host, "demozoo.org")
        || host_matches(host, "modules.pl")
        || host_matches(host, "mirsoft.info")
    {
        SourceKind::ModArchive
    } else {
        SourceKind::GenericYtDlp
    }
}

fn is_supported_media_path(path: &str) -> bool {
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "opus"
            | "m4a"
            | "aac"
            | "flac"
            | "wav"
            | "mp3"
            | "ogg"
            | "oga"
            | "webm"
            | "mkv"
            | "mp4"
            | "m4v"
            | "mov"
            | "avi"
            | "mod"
            | "xm"
            | "it"
            | "s3m"
            | "mptm"
            | "stm"
            | "mtm"
            | "669"
    )
}

/// Returns the deliberately narrow search route for a screen.
///
/// The normal search screen never fans a query out to every configured service.
/// This makes YouTube-only search the default and avoids surprise network use.
#[must_use]
pub const fn search_route(screen: Screen) -> SearchRoute {
    match screen {
        Screen::Search => SearchRoute::YouTube,
        Screen::YouTubeMusic => SearchRoute::YouTubeMusic,
        Screen::TrackerMusic => SearchRoute::TrackerArchives,
        Screen::Subscriptions
        | Screen::Local
        | Screen::Downloaded
        | Screen::History
        | Screen::Playlists
        | Screen::Statistics => SearchRoute::None,
    }
}

enum ProviderRequest {
    ReplaceYouTubeProvider {
        provider: Box<dyn Provider>,
    },
    Search {
        generation: u64,
        request: SearchRequest,
    },
    #[cfg(feature = "youtube-music")]
    YouTubeMusicSearch {
        generation: u64,
        query: String,
    },
    ChannelVideos {
        generation: u64,
        request: ChannelVideosRequest,
    },
    Details {
        generation: u64,
        video_id: String,
    },
    ChannelDetails {
        generation: u64,
        provider_generation: u64,
        channel_id: String,
    },
    ChannelSubscriberCounts {
        provider_generation: u64,
        channel_ids: Vec<String>,
    },
    ResolveApple {
        generation: u64,
        url: url::Url,
    },
    ResolveFirstClass {
        generation: u64,
        direct: DirectSourceInput,
    },
    TrackerSearch {
        generation: u64,
        query: String,
    },
    #[cfg(feature = "tracker-music")]
    PrepareTracker {
        generation: u64,
        item_key: String,
        request: crate::tracker_media::TrackerMediaRequest,
    },
    ScanLocal {
        generation: u64,
        root: PathBuf,
    },
    #[cfg(all(feature = "local", feature = "thumbnails"))]
    LocalArtwork {
        generation: u64,
        path: PathBuf,
        cache_directory: PathBuf,
        kind: LocalArtworkKind,
    },
    LocalFolderSize {
        generation: u64,
        path: PathBuf,
        cancellation: Arc<AtomicU64>,
        cached: Option<crate::local_browser::LocalFolderSizeMeasurement>,
    },
    #[cfg(feature = "wikidata")]
    Wikidata {
        generation: u64,
        kind: crate::providers::wikidata::WikidataExternalKind,
        external_id: String,
    },
    #[cfg(feature = "wikidata")]
    WikidataStatements {
        item_id: String,
    },
    Shutdown,
}

enum ProviderResponse {
    Search {
        generation: u64,
        request: SearchRequest,
        result: Result<SearchPage, String>,
    },
    #[cfg(feature = "youtube-music")]
    YouTubeMusicSearch {
        generation: u64,
        query: String,
        result: Result<Vec<YouTubeMusicTrack>, String>,
    },
    ChannelVideos {
        generation: u64,
        request: ChannelVideosRequest,
        result: Result<SearchPage, String>,
    },
    Details {
        generation: u64,
        result: Result<VideoDetails, String>,
    },
    ChannelDetails {
        generation: u64,
        provider_generation: u64,
        channel_id: String,
        result: Result<ChannelDetails, String>,
    },
    #[cfg(feature = "network")]
    ChannelPageMetadata {
        generation: u64,
        provider_generation: u64,
        channel_id: String,
        result: Result<crate::providers::youtube_channel_page::YouTubeChannelPageMetadata, String>,
    },
    ChannelSubscriberCounts {
        provider_generation: u64,
        requested_ids: Vec<String>,
        result: Result<Vec<ChannelSubscriberCount>, String>,
    },
    Apple {
        generation: u64,
        result: Result<ResolvedDirectMedia, String>,
    },
    FirstClass {
        generation: u64,
        source: SourceKind,
        result: Result<ResolvedDirectMedia, String>,
    },
    TrackerSource {
        generation: u64,
        source: String,
        result: Result<Vec<TrackerItem>, String>,
    },
    TrackerComplete {
        generation: u64,
    },
    #[cfg(feature = "tracker-music")]
    TrackerPrepared {
        generation: u64,
        item_key: String,
        result: Result<Vec<crate::tracker_media::PreparedTrackerModule>, String>,
    },
    LocalScan {
        generation: u64,
        root: PathBuf,
        result: Result<Vec<LocalMediaItem>, String>,
    },
    #[cfg(all(feature = "local", feature = "thumbnails"))]
    LocalArtwork {
        generation: u64,
        path: PathBuf,
        result: Result<Option<url::Url>, String>,
    },
    LocalFolderSize {
        generation: u64,
        path: PathBuf,
        result: Result<LocalFolderSizeWorkerResult, String>,
    },
    #[cfg(feature = "wikidata")]
    Wikidata {
        generation: u64,
        property_id: String,
        external_id: String,
        result: Result<Vec<crate::domain::WikidataLink>, String>,
    },
    #[cfg(feature = "wikidata")]
    WikidataStatements {
        item_id: String,
        result: Result<crate::providers::wikidata::WikidataEntityStatements, String>,
    },
}

/// One foreground directory-listing command for the isolated Local worker.
enum LocalBrowseRequest {
    /// Reads a bounded directory snapshot and optionally retains one child.
    Browse {
        generation: u64,
        directory: PathBuf,
        preferred_child: Option<PathBuf>,
    },
    /// Stops the isolated worker after queued listing work completes.
    Shutdown,
}

/// One directory-listing result returned by the isolated Local worker.
struct LocalBrowseResponse {
    /// Route generation that owns the result.
    generation: u64,
    /// Bounded listing or a user-facing filesystem error.
    result: Result<crate::local_browser::LocalDirectoryListing, String>,
}

#[cfg(feature = "yt-dlp")]
const DOWNLOAD_DIAGNOSTIC_BYTES: usize = 64 * 1024;
#[cfg(feature = "yt-dlp")]
const DOWNLOAD_LINE_BYTES: usize = 8 * 1024;
#[cfg(feature = "yt-dlp")]
const DOWNLOAD_COMPLETED_PATHS: usize = 4;

#[cfg(feature = "yt-dlp")]
#[derive(Clone, Debug)]
struct DownloadExit {
    success: bool,
    description: String,
}

#[cfg(feature = "yt-dlp")]
trait RunningDownload: Send {
    fn take_progress_reader(&mut self) -> Option<Box<dyn BufRead + Send>>;
    fn take_error_reader(&mut self) -> Option<Box<dyn BufRead + Send>>;
    fn try_wait(&mut self) -> Result<Option<DownloadExit>, String>;
    fn cancel(&mut self) -> Result<(), String>;
}

#[cfg(feature = "yt-dlp")]
impl RunningDownload for DownloadProcess {
    fn take_progress_reader(&mut self) -> Option<Box<dyn BufRead + Send>> {
        DownloadProcess::take_progress_reader(self)
            .map(|reader| Box::new(reader) as Box<dyn BufRead + Send>)
    }

    fn take_error_reader(&mut self) -> Option<Box<dyn BufRead + Send>> {
        DownloadProcess::take_error_reader(self)
            .map(|reader| Box::new(reader) as Box<dyn BufRead + Send>)
    }

    fn try_wait(&mut self) -> Result<Option<DownloadExit>, String> {
        DownloadProcess::try_wait(self)
            .map(|status| {
                status.map(|status| DownloadExit {
                    success: status.success(),
                    description: status.to_string(),
                })
            })
            .map_err(|error| error.to_string())
    }

    fn cancel(&mut self) -> Result<(), String> {
        DownloadProcess::cancel(self).map_err(|error| error.to_string())
    }
}

#[cfg(feature = "yt-dlp")]
trait DownloadLauncher: Send {
    fn start(&mut self, request: &DownloadRequest) -> Result<Box<dyn RunningDownload>, String>;
}

#[cfg(feature = "yt-dlp")]
struct YtDlpDownloadLauncher {
    client: YtDlp,
}

#[cfg(feature = "yt-dlp")]
impl DownloadLauncher for YtDlpDownloadLauncher {
    fn start(&mut self, request: &DownloadRequest) -> Result<Box<dyn RunningDownload>, String> {
        self.client
            .download(request)
            .map(|process| Box::new(process) as Box<dyn RunningDownload>)
            .map_err(|error| error.to_string())
    }
}

#[cfg(feature = "yt-dlp")]
#[derive(Clone, Copy, Debug, Default)]
struct DownloadProgress {
    downloaded_bytes: u64,
    total_bytes: Option<u64>,
    bytes_per_second: Option<f64>,
    eta_seconds: Option<u64>,
}

#[cfg(feature = "yt-dlp")]
#[derive(Debug, Default)]
struct DownloadOutputBuffer {
    progress: DownloadProgress,
    completed_paths: VecDeque<PathBuf>,
    diagnostic_lines: VecDeque<String>,
    diagnostic_bytes: usize,
    read_error: Option<String>,
}

#[cfg(feature = "yt-dlp")]
impl DownloadOutputBuffer {
    fn push_diagnostic(&mut self, stream: &str, line: &str, truncated: bool) {
        let suffix = if truncated {
            " …[line truncated]"
        } else {
            ""
        };
        let entry = format!("[{stream}] {}{suffix}", line.trim_end());
        self.diagnostic_bytes = self
            .diagnostic_bytes
            .saturating_add(entry.len().saturating_add(1));
        self.diagnostic_lines.push_back(entry);
        while self.diagnostic_bytes > DOWNLOAD_DIAGNOSTIC_BYTES {
            let Some(removed) = self.diagnostic_lines.pop_front() else {
                break;
            };
            self.diagnostic_bytes = self
                .diagnostic_bytes
                .saturating_sub(removed.len().saturating_add(1));
        }
    }

    fn apply_progress_line(&mut self, line: &str, truncated: bool) {
        if !truncated {
            match parse_download_event(line) {
                Some(crate::playback::ytdlp::DownloadEvent::Progress {
                    downloaded_bytes,
                    total_bytes,
                    bytes_per_second,
                    eta_seconds,
                }) => {
                    self.progress = DownloadProgress {
                        downloaded_bytes,
                        total_bytes,
                        bytes_per_second,
                        eta_seconds,
                    };
                    return;
                }
                Some(crate::playback::ytdlp::DownloadEvent::CompletedFile(path)) => {
                    if self.completed_paths.len() == DOWNLOAD_COMPLETED_PATHS {
                        self.completed_paths.pop_front();
                    }
                    self.completed_paths.push_back(path);
                    return;
                }
                None => {}
            }
        }
        self.push_diagnostic("stdout", line, truncated);
    }

    fn diagnostics(&self) -> String {
        self.diagnostic_lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(feature = "yt-dlp")]
struct ActiveDownload {
    title: String,
    destination: PathBuf,
    process: Box<dyn RunningDownload>,
    output: Arc<Mutex<DownloadOutputBuffer>>,
    reader_threads: Vec<JoinHandle<()>>,
}

#[cfg(feature = "yt-dlp")]
impl ActiveDownload {
    fn start(
        title: String,
        destination: PathBuf,
        mut process: Box<dyn RunningDownload>,
    ) -> Result<Self, String> {
        let progress_reader = process
            .take_progress_reader()
            .ok_or_else(|| "yt-dlp did not expose its progress stream".to_owned())?;
        let error_reader = process
            .take_error_reader()
            .ok_or_else(|| "yt-dlp did not expose its diagnostic stream".to_owned())?;
        let output = Arc::new(Mutex::new(DownloadOutputBuffer::default()));

        let progress_output = Arc::clone(&output);
        let progress_thread = thread::Builder::new()
            .name("youta-download-progress".to_owned())
            .spawn(move || drain_download_reader(progress_reader, &progress_output, true))
            .map_err(|error| format!("cannot start the download progress reader: {error}"))?;

        let error_output = Arc::clone(&output);
        let error_thread = match thread::Builder::new()
            .name("youta-download-diagnostics".to_owned())
            .spawn(move || drain_download_reader(error_reader, &error_output, false))
        {
            Ok(thread) => thread,
            Err(error) => {
                let _ = process.cancel();
                let _ = progress_thread.join();
                return Err(format!(
                    "cannot start the download diagnostic reader: {error}"
                ));
            }
        };

        Ok(Self {
            title,
            destination,
            process,
            output,
            reader_threads: vec![progress_thread, error_thread],
        })
    }

    fn join_readers(&mut self) {
        for thread in self.reader_threads.drain(..) {
            let _ = thread.join();
        }
    }

    fn cancel_and_join(&mut self) {
        let _ = self.process.cancel();
        self.join_readers();
    }
}

#[cfg(feature = "yt-dlp")]
fn drain_download_reader(
    mut reader: Box<dyn BufRead + Send>,
    output: &Arc<Mutex<DownloadOutputBuffer>>,
    parse_progress: bool,
) {
    let mut line = Vec::with_capacity(1024);
    let mut truncated = false;
    loop {
        let next = match reader.fill_buf() {
            Ok([]) => {
                if !line.is_empty() || truncated {
                    record_download_line(output, &line, truncated, parse_progress);
                }
                break;
            }
            Ok(bytes) => {
                let end = bytes
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(bytes.len(), |index| index.saturating_add(1));
                let has_newline = bytes.get(end.saturating_sub(1)) == Some(&b'\n');
                let remaining = DOWNLOAD_LINE_BYTES.saturating_sub(line.len());
                if end > remaining {
                    truncated = true;
                }
                let retained = end.min(remaining);
                let chunk = bytes[..retained].to_vec();
                (end, has_newline, chunk)
            }
            Err(error) => {
                let mut state = output
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.read_error = Some(format!("cannot read yt-dlp output: {error}"));
                break;
            }
        };
        reader.consume(next.0);
        line.extend_from_slice(&next.2);
        if next.1 {
            record_download_line(output, &line, truncated, parse_progress);
            line.clear();
            truncated = false;
        }
    }
}

#[cfg(feature = "yt-dlp")]
fn record_download_line(
    output: &Arc<Mutex<DownloadOutputBuffer>>,
    line: &[u8],
    truncated: bool,
    parse_progress: bool,
) {
    let line = String::from_utf8_lossy(line);
    let mut state = output
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if parse_progress {
        state.apply_progress_line(&line, truncated);
    } else {
        state.push_diagnostic("stderr", &line, truncated);
    }
}

/// Controller-side interpretation of authoritative backend lifecycle events.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PlaybackPhase {
    #[default]
    Idle,
    Loading,
    Loaded,
    Playing,
}

/// Bounded fallback stage for the current backend load attempt.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PlaybackLoadKind {
    #[default]
    Regular,
    YouTubeDirect,
    YouTubeCanonical,
    YouTubeChecked,
}

/// Stable position in a same-source list used for automatic continuation.
///
/// Keeping an index avoids confusing duplicate media identifiers, which are
/// possible when one tracker archive expands into several module files.
#[derive(Clone, Debug, Eq, PartialEq)]
enum AutoplayOrigin {
    YouTube {
        generation: u64,
        index: usize,
    },
    YouTubeMusic {
        generation: u64,
        index: usize,
    },
    Subscription {
        channel_id: String,
        index: usize,
    },
    LocalBrowser {
        directory: PathBuf,
        entries: Arc<[PathBuf]>,
        index: usize,
    },
    LocalScan {
        generation: u64,
        index: usize,
    },
    Tracker {
        generation: u64,
        index: usize,
    },
}

/// One bounded action selected after an autoplay-owned item reaches EOF.
enum AutoplayStep {
    Play {
        item: Box<QueueItem>,
        origin: AutoplayOrigin,
    },
    #[cfg(feature = "tracker-music")]
    PrepareTracker {
        index: usize,
        origin: AutoplayOrigin,
    },
    SourceChanged,
    Exhausted,
}

#[cfg(feature = "tracker-music")]
#[derive(Clone, Debug, Eq, PartialEq)]
enum TrackerPreparationOwner {
    Manual,
    Autoplay(AutoplayOrigin),
    CanceledAutoplay,
}

#[cfg(feature = "tracker-music")]
#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingTrackerPreparation {
    generation: u64,
    item_key: String,
    owner: TrackerPreparationOwner,
}

/// Maximum channel collections retained by the process-local LRU cache.
const MAX_CACHED_SUBSCRIPTION_CHANNELS: usize = 24;
/// Maximum playable summaries retained for one subscribed channel.
const MAX_CACHED_SUBSCRIPTION_VIDEOS_PER_CHANNEL: usize = 250;
/// Approximate heap budget shared by all process-local channel summaries.
const MAX_CACHED_SUBSCRIPTION_BYTES: usize = 8 * 1024 * 1024;
/// Description excerpt retained until the selected-video details request wins.
const MAX_CACHED_SUBSCRIPTION_DESCRIPTION_BYTES: usize = 4 * 1024;
/// Bound for a cached title or channel display name.
const MAX_CACHED_SUBSCRIPTION_LABEL_BYTES: usize = 2 * 1024;
/// Bound for a cached optional URL or publication-age string.
const MAX_CACHED_SUBSCRIPTION_FIELD_BYTES: usize = 2 * 1024;
/// Consecutive empty pages followed before requiring explicit continuation.
const MAX_AUTOMATIC_EMPTY_SUBSCRIPTION_PAGES: u32 = 3;
/// Stable channel selections wait briefly before optional metadata traffic.
const CHANNEL_DETAILS_DEBOUNCE: Duration = Duration::from_millis(500);
/// Maximum compact channel records retained by the process.
const MAX_CACHED_CHANNEL_DETAILS: usize = 64;
/// Provider channel metadata remains fresh for seven days, but stale artwork
/// is still rendered immediately while a background refresh runs.
const CHANNEL_DETAILS_CACHE_TTL_SECONDS: i64 = 7 * 24 * 60 * 60;
/// Maximum directory measurements scheduled from one Local listing.
const MAX_LAZY_LOCAL_FOLDER_SIZES: usize = 256;
/// Maximum completed directory sizes retained across Local navigation.
const MAX_CACHED_LOCAL_FOLDER_SIZES: usize = 512;
/// Maximum positive or negative embedded-artwork lookups retained in RAM.
#[cfg(all(feature = "local", feature = "thumbnails"))]
const MAX_CACHED_LOCAL_ARTWORK: usize = 256;
/// Maximum time a traversal can be reused when nested changes may be invisible.
const LOCAL_FOLDER_SIZE_CACHE_TTL: Duration = Duration::from_mins(1);
/// Retry delay for inaccessible or resource-limited directory trees.
const LOCAL_FOLDER_SIZE_FAILURE_TTL: Duration = Duration::from_mins(1);
/// Maximum listing cursors retained to rotate bounded measurement batches.
const MAX_LOCAL_FOLDER_SIZE_BATCH_CURSORS: usize = 64;

/// Bounded, non-persistent channel collection and its next sequential page.
#[derive(Clone, Debug, Default)]
struct CachedSubscriptionVideos {
    /// Playable videos accumulated in provider order.
    items: Vec<SearchItem>,
    /// Next sequential provider page, when one remains.
    next_page: Option<u32>,
    /// Consecutive empty remote pages since the last playable page.
    consecutive_empty_pages: u8,
}

impl CachedSubscriptionVideos {
    /// Returns a conservative approximation of owned heap bytes.
    fn estimated_heap_bytes(&self) -> usize {
        self.items
            .iter()
            .map(subscription_item_estimated_heap_bytes)
            .fold(0usize, usize::saturating_add)
    }
}

/// One channel record scheduled after the selected source settles.
#[derive(Clone, Debug)]
struct ScheduledChannelDetails {
    /// Selection generation that owns the eventual visible update.
    generation: u64,
    /// Exact provider channel identifier.
    channel_id: String,
    /// Earliest instant at which the background request may start.
    due_at: Instant,
}

/// One Wikidata channel lookup scheduled after source navigation settles.
#[cfg(feature = "wikidata")]
#[derive(Clone, Debug)]
struct ScheduledChannelWikidata {
    /// Lookup generation that owns the eventual visible update.
    generation: u64,
    /// Exact `YouTube` channel identifier queried through `P2397`.
    channel_id: String,
    /// Earliest instant at which the background request may start.
    due_at: Instant,
}

#[cfg(feature = "yt-dlp")]
const YOUTUBE_PREWARM_DEBOUNCE: Duration = Duration::from_millis(200);
#[cfg(feature = "yt-dlp")]
const YOUTUBE_PREWARM_TTL: Duration = Duration::from_secs(90);
#[cfg(feature = "yt-dlp")]
const YOUTUBE_PREWARM_EXPIRY_MARGIN_SECONDS: u64 = 30;
#[cfg(feature = "yt-dlp")]
const YOUTUBE_PREWARM_FAILURE_BACKOFF: Duration = Duration::from_secs(30);

/// Selected video waiting for the short debounce before extractor work.
#[cfg(feature = "yt-dlp")]
#[derive(Debug)]
struct ScheduledYouTubePrewarm {
    generation: u64,
    media_id: MediaId,
    source_url: url::Url,
    due_at: Instant,
}

/// One cancellable request owned by the dedicated resolver worker.
#[cfg(feature = "yt-dlp")]
#[derive(Debug)]
struct YouTubePrewarmJob {
    media_id: MediaId,
    request: YouTubePrewarmRequest,
    cancellation: YouTubePrewarmCancellation,
}

/// Bounded command accepted by the resolver worker.
#[cfg(feature = "yt-dlp")]
#[derive(Debug)]
enum YouTubePrewarmCommand {
    Resolve(YouTubePrewarmJob),
    Shutdown,
}

/// Worker completion paired with the exact selected media identity.
#[cfg(feature = "yt-dlp")]
#[derive(Debug)]
struct YouTubePrewarmCompletion {
    media_id: MediaId,
    completed_at: Instant,
    result: YouTubePrewarmResult,
}

/// Request currently running or waiting in the one-element worker queue.
#[cfg(feature = "yt-dlp")]
#[derive(Debug)]
struct ActiveYouTubePrewarm {
    generation: u64,
    media_id: MediaId,
    cancellation: YouTubePrewarmCancellation,
}

/// Fresh RAM-only direct stream eligible for one playback attempt.
#[cfg(feature = "yt-dlp")]
#[derive(Debug)]
struct ReadyYouTubePrewarm {
    media_id: MediaId,
    completed_at: Instant,
    audio: PrewarmedYouTubeAudio,
}

/// Selection identity retained while page one refreshes in the background.
#[derive(Debug)]
struct PendingSubscriptionRefresh {
    /// Channel whose page-one response may replace the visible cache.
    channel_id: String,
    /// Video reselected when it remains present after the refresh.
    selected_video_id: Option<String>,
    /// Previous index used when the selected video disappeared.
    fallback_index: usize,
    /// Replacement pages staged while filtered page one is temporarily empty.
    staged_cache: Option<CachedSubscriptionVideos>,
}

/// Maximum number of internal Details pages retained in either direction.
const MAX_DETAIL_NAVIGATION_HISTORY: usize = 16;
/// Maximum expanded Wikidata entities retained in process memory.
#[cfg(feature = "wikidata")]
const MAX_CACHED_WIKIDATA_ENTITIES: usize = 8;
/// Maximum detached system-opener tasks accepted before earlier ones finish.
const MAX_URL_OPEN_TASKS: usize = 4;
/// Maximum draft RSS feed URL retained by the in-app subscription editor.
const MAX_RSS_SUBSCRIPTION_URL_BYTES: usize = 8 * 1024;
/// Stable marker for a linked video whose provider details are still pending.
const LINKED_VIDEO_LOADING_TITLE: &str = "Loading linked YouTube video…";
/// Stable marker rendered below the pending linked-video title.
const LINKED_VIDEO_LOADING_DESCRIPTION: &str = "Loading video details…";

/// `YouTube` video currently displayed independently of the left-hand list.
#[derive(Debug, Eq, PartialEq)]
struct ActiveDescriptionVideo {
    video_id: String,
    start_seconds: Option<u64>,
}

/// URL-free result returned by one asynchronous `xdg-open` task.
///
/// The selected target is intentionally absent so it cannot enter diagnostics,
/// debug output, or long-lived controller state.
#[derive(Debug, Eq, PartialEq)]
enum UrlOpenCompletion {
    /// The system opener exited successfully.
    Succeeded,
    /// Starting, waiting for, or running the opener failed.
    Failed(String),
}

/// One bounded, move-only browser-style Details location.
///
/// Large description strings move between the current view and the two
/// history deques. They are never cloned when navigating.
#[derive(Debug)]
struct DetailNavigationSnapshot {
    screen: Screen,
    selected: usize,
    subscription_route: SubscriptionRoute,
    subscription_focus: SubscriptionPane,
    subscription_selected_source: usize,
    subscription_selected_item: usize,
    subscription_description_expanded: bool,
    details: Option<DetailView>,
    previous_detail: Option<DetailView>,
    details_scroll: usize,
    details_focused: bool,
    text_selection_mode: bool,
    details_text_selection: Option<DetailsTextSelection>,
    selected_detail_link: Option<usize>,
    right_panel_mode: RightPanelMode,
    active_description_video: Option<ActiveDescriptionVideo>,
}

/// Default application state used by the interactive terminal.
pub struct AppController {
    config: Config,
    store: StateStore,
    view: ViewModel,
    /// Query retained independently for the YouTube tab.
    youtube_search_query: String,
    /// Selected YouTube row retained while another tab is visible.
    youtube_selected: usize,
    /// Query retained independently for the `YouTube Music` tab.
    youtube_music_search_query: String,
    /// Selected `YouTube Music` row retained while another tab is visible.
    youtube_music_selected: usize,
    /// Query retained independently for the tracker-archive tab.
    tracker_search_query: String,
    /// Selected tracker row retained while another tab is visible.
    tracker_selected: usize,
    youtube_results: Vec<SearchItem>,
    /// Track summaries returned by the independent `YouTube Music` search.
    youtube_music_results: Vec<SearchItem>,
    direct_item: Option<DirectSourceInput>,
    resolved_direct: Option<ResolvedDirectMedia>,
    local_results: Vec<LocalMediaItem>,
    /// Current bounded, non-recursive directory snapshot for the Local tab.
    local_listing: Option<crate::local_browser::LocalDirectoryListing>,
    /// Watched percentages hydrated once for the current Local listing.
    local_progress_cache: HashMap<MediaId, u8>,
    /// Generation rejecting directory responses for an older Local route.
    local_generation: u64,
    /// Shared cancellation generation checked during recursive folder walks.
    local_folder_size_generation: Arc<AtomicU64>,
    /// Complete RAM-only sizes for real directories in the current listing.
    local_folder_size_cache: HashMap<PathBuf, CachedLocalFolderSize>,
    /// Least-recently-validated order for bounded folder-size eviction.
    local_folder_size_cache_order: VecDeque<PathBuf>,
    /// Bounded failures preventing repeated scans of unavailable folders.
    local_folder_size_failures: HashMap<PathBuf, CachedLocalFolderSizeFailure>,
    /// Least-recent failure order for bounded negative-cache eviction.
    local_folder_size_failure_order: VecDeque<PathBuf>,
    /// Next directory index for each recently visited listing's bounded batch.
    local_folder_size_batch_cursors: HashMap<PathBuf, usize>,
    /// Least-recent listing order for bounded cursor eviction.
    local_folder_size_batch_cursor_order: VecDeque<PathBuf>,
    /// Bounded current-listing directories awaiting lazy measurement.
    local_folder_size_queue: VecDeque<PathBuf>,
    /// The sole directory currently owned by the provider worker.
    local_folder_size_pending: Option<PathBuf>,
    /// Child path reselected after an asynchronous move to its parent.
    pending_local_reselection: Option<(u64, PathBuf)>,
    /// Monotonic owner for the sole embedded-artwork extraction request.
    #[cfg(all(feature = "local", feature = "thumbnails"))]
    local_artwork_generation: u64,
    /// Media path currently being inspected by the provider worker.
    #[cfg(all(feature = "local", feature = "thumbnails"))]
    pending_local_artwork: Option<(u64, PathBuf)>,
    /// Process-local positive and negative artwork results by exact path.
    #[cfg(all(feature = "local", feature = "thumbnails"))]
    local_artwork_cache: HashMap<PathBuf, Option<url::Url>>,
    /// Insertion order bounding the process-local artwork result cache.
    #[cfg(all(feature = "local", feature = "thumbnails"))]
    local_artwork_cache_order: VecDeque<PathBuf>,
    tracker_results: Vec<TrackerItem>,
    subscription_tree: SubscriptionTree,
    /// Current OPML leaves in stable folder order.
    subscription_entries: Vec<FlattenedSubscription>,
    /// Process-local bounded channel-video pages keyed by channel identifier.
    subscription_video_cache: HashMap<String, CachedSubscriptionVideos>,
    /// Least-recently-used eviction order for channel-video pages.
    subscription_cache_order: VecDeque<String>,
    /// Channel whose cached rows and in-flight response own the item pane.
    active_subscription_channel_id: Option<String>,
    /// Generation rejecting responses queued for an older source selection.
    subscription_generation: u64,
    /// Selection restored after an explicit page-one refresh completes.
    pending_subscription_refresh: Option<PendingSubscriptionRefresh>,
    next_youtube_page: Option<u32>,
    youtube_search_request: Option<SearchRequest>,
    youtube_provider_available: bool,
    youtube_provider_builder: Box<dyn YouTubeProviderBuilder>,
    provider_requests: Option<Sender<ProviderRequest>>,
    provider_responses: Receiver<ProviderResponse>,
    provider_thread: Option<JoinHandle<()>>,
    /// Foreground Local listings isolated from recursive and remote work.
    local_browse_requests: Option<Sender<LocalBrowseRequest>>,
    /// Completed foreground Local listings from the isolated worker.
    local_browse_responses: Receiver<LocalBrowseResponse>,
    /// Dedicated worker ensuring Local listings do not wait behind scans.
    local_browse_thread: Option<JoinHandle<()>>,
    provider_disconnect_reported: bool,
    /// Prevents repeated diagnostics after the Local worker disconnects.
    local_browse_disconnect_reported: bool,
    youtube_channel_statistics_mode: ChannelStatisticsMode,
    youtube_provider_generation: u64,
    channel_subscriber_cache: HashMap<String, Option<u64>>,
    pending_channel_subscribers: HashSet<String>,
    /// Compact positive and negative channel metadata cached for this process.
    channel_details_cache: HashMap<String, Option<ChannelSummary>>,
    /// Selected-only public profile fields and channel-advertised links.
    channel_profile_cache: HashMap<String, ChannelDetails>,
    /// Expiration timestamps for positive RAM entries hydrated from SQLite.
    channel_details_fresh_until: HashMap<String, i64>,
    /// Least-recently-used order for compact channel metadata.
    channel_details_cache_order: VecDeque<String>,
    /// Channel identifiers currently owned by the provider worker.
    pending_channel_details: HashSet<String>,
    /// Selection generation rejecting metadata for a different visible source.
    channel_details_generation: u64,
    /// Debounced metadata request for the stable subscription source.
    scheduled_channel_details: Option<ScheduledChannelDetails>,
    /// Debounced Wikidata lookup for the stable subscription source.
    #[cfg(feature = "wikidata")]
    scheduled_channel_wikidata: Option<ScheduledChannelWikidata>,
    search_generation: u64,
    details_generation: u64,
    #[cfg(feature = "wikidata")]
    wikidata_generation: u64,
    /// Bounded human-facing Wikidata entity statements retained in RAM.
    #[cfg(feature = "wikidata")]
    wikidata_entity_cache: HashMap<String, crate::providers::wikidata::WikidataEntityStatements>,
    /// Least-recently-used order for expanded Wikidata entities.
    #[cfg(feature = "wikidata")]
    wikidata_entity_cache_order: VecDeque<String>,
    /// Exact Wikidata Q-IDs currently owned by the provider worker.
    #[cfg(feature = "wikidata")]
    pending_wikidata_entities: HashSet<String>,
    /// Commands for the isolated selected-video resolver worker.
    #[cfg(feature = "yt-dlp")]
    youtube_prewarm_requests: Option<Sender<YouTubePrewarmCommand>>,
    /// Receiver clone used to replace a queued, not-yet-started request.
    #[cfg(feature = "yt-dlp")]
    youtube_prewarm_request_drain: Option<Receiver<YouTubePrewarmCommand>>,
    /// Bounded completions drained by the terminal tick.
    #[cfg(feature = "yt-dlp")]
    youtube_prewarm_responses: Option<Receiver<YouTubePrewarmCompletion>>,
    /// Dedicated worker handle joined during shutdown.
    #[cfg(feature = "yt-dlp")]
    youtube_prewarm_thread: Option<JoinHandle<()>>,
    /// Latest selection generation accepted for speculative resolution.
    #[cfg(feature = "yt-dlp")]
    youtube_prewarm_generation: u64,
    /// Debounced request for the stable selected `YouTube` row.
    #[cfg(feature = "yt-dlp")]
    scheduled_youtube_prewarm: Option<ScheduledYouTubePrewarm>,
    /// Sole active or one-element-queued resolver request.
    #[cfg(feature = "yt-dlp")]
    active_youtube_prewarm: Option<ActiveYouTubePrewarm>,
    /// Sole fresh resolved stream, never persisted or logged.
    #[cfg(feature = "yt-dlp")]
    ready_youtube_prewarm: Option<ReadyYouTubePrewarm>,
    /// One recent failure suppressing repeated extraction for the same row.
    #[cfg(feature = "yt-dlp")]
    youtube_prewarm_failure: Option<(MediaId, Instant)>,
    playback_factory: Option<PlaybackFactory>,
    player: Option<Box<dyn PlaybackBackend>>,
    #[cfg(feature = "yt-dlp")]
    download_launcher: Box<dyn DownloadLauncher>,
    #[cfg(feature = "yt-dlp")]
    active_download: Option<ActiveDownload>,
    report_actions: Box<dyn DiagnosticActionHandler>,
    diagnostic_helpers_cache: Option<Vec<ExternalHelper>>,
    /// URL-free completions from detached system-opener tasks.
    url_open_results: Receiver<UrlOpenCompletion>,
    /// Sender cloned into each detached system-opener task.
    url_open_result_sender: Sender<UrlOpenCompletion>,
    /// Number of system-opener tasks that have not reported completion.
    url_open_pending: usize,
    playback_queue: PlaybackQueue,
    playback_phase: PlaybackPhase,
    playback_load_kind: PlaybackLoadKind,
    pending_history: Option<HistoryEntry>,
    ignore_replaced_stop: bool,
    /// Advertisement chapter most recently skipped while backend status catches up.
    skipped_advertisement_chapter: Option<(MediaId, u64)>,
    current_media: Option<MediaId>,
    /// Same-source position owning the active queue item.
    current_autoplay_origin: Option<AutoplayOrigin>,
    /// Same-source position resumed after explicit queued items finish.
    queued_autoplay_resume_origin: Option<AutoplayOrigin>,
    /// Sole tracker preparation request and the action allowed on completion.
    #[cfg(feature = "tracker-music")]
    pending_tracker_preparation: Option<PendingTrackerPreparation>,
    selected_start_override: Option<u64>,
    youtube_music_start_override: Option<u64>,
    seek_back: VecDeque<(MediaId, Duration)>,
    previous_detail: Option<DetailView>,
    /// Internal Details pages behind the current description link.
    detail_navigation_back: VecDeque<DetailNavigationSnapshot>,
    /// Internal Details pages available after going back.
    detail_navigation_forward: VecDeque<DetailNavigationSnapshot>,
    /// Linked video whose provider response owns the current Details panel.
    active_description_video: Option<ActiveDescriptionVideo>,
    last_position_save: Instant,
    last_session_save: Instant,
    last_tick: Instant,
    session_dirty: bool,
    unflushed_listen_time: Duration,
    diagnostic_only: bool,
    quit_on_error_dismiss: bool,
}

impl AppController {
    /// Constructs the controller and restores the last durable screen state.
    ///
    /// The optional provider is moved to a worker thread. Supplying `None`
    /// leaves the UI fully usable for offline screens and displays a
    /// configuration hint when the user searches.
    #[must_use]
    pub fn new(
        config: Config,
        store: StateStore,
        youtube_provider: Option<Box<dyn Provider>>,
        playback_factory: Option<PlaybackFactory>,
    ) -> Self {
        let youtube_provider_available = youtube_provider.is_some();
        let youtube_channel_statistics_mode = youtube_provider
            .as_ref()
            .map_or(ChannelStatisticsMode::Unsupported, |provider| {
                provider.channel_statistics_mode()
            });
        let (subscription_tree, subscription_load_error) = match subscriptions::load(&config) {
            Ok(tree) => (tree, None),
            Err(error) => (SubscriptionTree::default(), Some(error)),
        };
        let (response_sender, provider_responses) = unbounded();
        let (request_sender, request_receiver) = unbounded();
        let (local_browse_response_sender, local_browse_responses) = unbounded();
        let (local_browse_request_sender, local_browse_request_receiver) = unbounded();
        let (url_open_result_sender, url_open_results) = unbounded();
        let allow_insecure_http = config.providers.allow_insecure_http;
        let mod_archive_api_key = config.providers.mod_archive_api_key.clone();
        let jamendo_client_id = config.providers.jamendo_client_id.clone();
        #[cfg(feature = "youtube-music")]
        let youtube_music_executable = config.providers.yt_dlp_executable.clone();
        let provider_storage_root = config.config_dir().to_owned();
        let provider_thread_result = thread::Builder::new()
            .name("youta-provider-worker".to_owned())
            .spawn(move || {
                provider_worker(
                    youtube_provider,
                    request_receiver,
                    response_sender,
                    allow_insecure_http,
                    mod_archive_api_key,
                    jamendo_client_id,
                    #[cfg(feature = "youtube-music")]
                    youtube_music_executable,
                    provider_storage_root,
                );
            });
        let (provider_thread, provider_thread_error) = match provider_thread_result {
            Ok(handle) => (Some(handle), None),
            Err(error) => (None, Some(error)),
        };
        let provider_requests = provider_thread.as_ref().map(|_| request_sender);
        let local_browse_thread_result = thread::Builder::new()
            .name("youta-local-browser".to_owned())
            .spawn(move || {
                local_browse_worker(local_browse_request_receiver, local_browse_response_sender);
            });
        let (local_browse_thread, local_browse_thread_error) = match local_browse_thread_result {
            Ok(handle) => (Some(handle), None),
            Err(error) => (None, Some(error)),
        };
        let local_browse_requests = local_browse_thread
            .as_ref()
            .map(|_| local_browse_request_sender);

        let (saved, session_restore_error) = match store.session() {
            Ok(saved) => (saved.unwrap_or_default(), None),
            Err(error) => (SessionState::default(), Some(error)),
        };
        let (saved_search, search_restore_error) = match store.youtube_search() {
            Ok(saved) => (saved, None),
            Err(error) => (None, Some(error)),
        };
        let (saved_music_search, music_search_restore_error) = match store.youtube_music_search() {
            Ok(saved) => (saved, None),
            Err(error) => (None, Some(error)),
        };
        let mut view = ViewModel {
            screen: tui_screen_from_stored(&saved.screen),
            local_path: saved.local_path.clone().unwrap_or_default(),
            search_query: if matches!(saved.screen, StoredScreen::YouTubeMusic) {
                saved.youtube_music_search_text.clone()
            } else {
                saved.search_text.clone()
            },
            selected: saved.selected_row,
            details_focused: saved.focus == PanelFocus::Right,
            details_scroll: usize::try_from(saved.details_scroll).unwrap_or(usize::MAX),
            right_panel_mode: if saved.waveform_visible {
                RightPanelMode::Waveform
            } else {
                RightPanelMode::Details
            },
            show_chapter_timestamps: !saved.chapter_timestamps_hidden,
            ..ViewModel::default()
        };
        view.subscriptions.layout = config.ui.subscriptions_layout;
        if let Some(search) = saved_search.as_ref() {
            if view.screen == Screen::Search {
                view.search_query.clone_from(&search.request.query);
            }
            view.search_kind = match search.request.target {
                SearchTarget::Videos => SearchKind::Videos,
                SearchTarget::Channels => SearchKind::Channels,
            };
            view.youtube_search_sort = match search.request.sort {
                ProviderSearchSort::UploadDate => YouTubeSearchSort::Newest,
                ProviderSearchSort::Relevance | ProviderSearchSort::Views => {
                    YouTubeSearchSort::Relevance
                }
            };
            view.youtube_creative_commons_only = search
                .request
                .filters
                .features
                .contains(&SearchFeature::CreativeCommons);
        }
        if view.screen == Screen::YouTubeMusic
            && let Some(search) = saved_music_search.as_ref()
        {
            view.search_query.clone_from(&search.query);
        }
        view.playback.volume = config.playback.volume_percent;
        view.playback.speed = f64::from(config.playback.speed_percent) / 100.0;
        view.skip_advertisement_chapters = config.playback.skip_advertisement_chapters;
        view.autoplay = config.playback.autoplay;
        view.local_folder_sizes_enabled = config.ui.show_local_folder_sizes;
        view.status_line = if youtube_provider_available {
            "Default search: YouTube videos only".to_owned()
        } else {
            "Default search: YouTube videos only; provider setup opens when needed".to_owned()
        };
        #[cfg(feature = "yt-dlp")]
        let download_launcher: Box<dyn DownloadLauncher> = Box::new(YtDlpDownloadLauncher {
            client: YtDlp::new(YtDlpConfig {
                executable: config.providers.yt_dlp_executable.clone(),
                ..YtDlpConfig::default()
            }),
        });
        let (youtube_search_request, mut youtube_results) = saved_search.map_or_else(
            || (None, Vec::new()),
            |search| (Some(search.request), search.results),
        );
        for item in &mut youtube_results {
            if let SearchItem::Video(video) = item {
                // Direct stream URLs can expire between processes. Restored
                // rows keep their canonical page and receive a fresh stream
                // only through the normal lazy-details request.
                video.stream_url = None;
            }
        }

        let youtube_search_query = saved.search_text.clone();
        let youtube_selected = saved.youtube_selected_row.unwrap_or_else(|| {
            if view.screen == Screen::Search {
                view.selected
            } else {
                0
            }
        });
        let youtube_music_search_query = saved.youtube_music_search_text.clone();
        let (youtube_music_search_query, mut youtube_music_results) = saved_music_search
            .map_or_else(
                || (youtube_music_search_query, Vec::new()),
                |search| (search.query, search.results),
            );
        for item in &mut youtube_music_results {
            if let SearchItem::Video(video) = item {
                video.stream_url = None;
            }
        }
        let youtube_music_selected = saved.youtube_music_selected_row.unwrap_or_else(|| {
            if view.screen == Screen::YouTubeMusic {
                view.selected
            } else {
                0
            }
        });
        let tracker_selected = if view.screen == Screen::TrackerMusic {
            view.selected
        } else {
            0
        };
        if view.screen == Screen::TrackerMusic {
            view.search_query.clear();
        }
        let mut controller = Self {
            config,
            store,
            view,
            youtube_search_query,
            youtube_selected,
            youtube_music_search_query,
            youtube_music_selected,
            tracker_search_query: String::new(),
            tracker_selected,
            youtube_results,
            youtube_music_results,
            direct_item: None,
            resolved_direct: None,
            local_results: Vec::new(),
            local_listing: None,
            local_progress_cache: HashMap::new(),
            local_generation: 0,
            local_folder_size_generation: Arc::new(AtomicU64::new(0)),
            local_folder_size_cache: HashMap::new(),
            local_folder_size_cache_order: VecDeque::new(),
            local_folder_size_failures: HashMap::new(),
            local_folder_size_failure_order: VecDeque::new(),
            local_folder_size_batch_cursors: HashMap::new(),
            local_folder_size_batch_cursor_order: VecDeque::new(),
            local_folder_size_queue: VecDeque::new(),
            local_folder_size_pending: None,
            pending_local_reselection: None,
            #[cfg(all(feature = "local", feature = "thumbnails"))]
            local_artwork_generation: 0,
            #[cfg(all(feature = "local", feature = "thumbnails"))]
            pending_local_artwork: None,
            #[cfg(all(feature = "local", feature = "thumbnails"))]
            local_artwork_cache: HashMap::new(),
            #[cfg(all(feature = "local", feature = "thumbnails"))]
            local_artwork_cache_order: VecDeque::new(),
            tracker_results: Vec::new(),
            subscription_tree,
            subscription_entries: Vec::new(),
            subscription_video_cache: HashMap::new(),
            subscription_cache_order: VecDeque::new(),
            active_subscription_channel_id: None,
            subscription_generation: 0,
            pending_subscription_refresh: None,
            // Official YouTube pagination uses opaque tokens retained only by
            // the live provider. Restored rows remain instant and safe, while
            // a new explicit search rebuilds the continuation chain.
            next_youtube_page: None,
            youtube_search_request,
            youtube_provider_available,
            youtube_provider_builder: Box::new(SystemYouTubeProviderBuilder),
            provider_requests,
            provider_responses,
            provider_thread,
            local_browse_requests,
            local_browse_responses,
            local_browse_thread,
            provider_disconnect_reported: false,
            local_browse_disconnect_reported: false,
            youtube_channel_statistics_mode,
            youtube_provider_generation: 0,
            channel_subscriber_cache: HashMap::new(),
            pending_channel_subscribers: HashSet::new(),
            channel_details_cache: HashMap::new(),
            channel_profile_cache: HashMap::new(),
            channel_details_fresh_until: HashMap::new(),
            channel_details_cache_order: VecDeque::new(),
            pending_channel_details: HashSet::new(),
            channel_details_generation: 0,
            scheduled_channel_details: None,
            #[cfg(feature = "wikidata")]
            scheduled_channel_wikidata: None,
            search_generation: 0,
            details_generation: 0,
            #[cfg(feature = "wikidata")]
            wikidata_generation: 0,
            #[cfg(feature = "wikidata")]
            wikidata_entity_cache: HashMap::new(),
            #[cfg(feature = "wikidata")]
            wikidata_entity_cache_order: VecDeque::new(),
            #[cfg(feature = "wikidata")]
            pending_wikidata_entities: HashSet::new(),
            #[cfg(feature = "yt-dlp")]
            youtube_prewarm_requests: None,
            #[cfg(feature = "yt-dlp")]
            youtube_prewarm_request_drain: None,
            #[cfg(feature = "yt-dlp")]
            youtube_prewarm_responses: None,
            #[cfg(feature = "yt-dlp")]
            youtube_prewarm_thread: None,
            #[cfg(feature = "yt-dlp")]
            youtube_prewarm_generation: 0,
            #[cfg(feature = "yt-dlp")]
            scheduled_youtube_prewarm: None,
            #[cfg(feature = "yt-dlp")]
            active_youtube_prewarm: None,
            #[cfg(feature = "yt-dlp")]
            ready_youtube_prewarm: None,
            #[cfg(feature = "yt-dlp")]
            youtube_prewarm_failure: None,
            playback_factory,
            player: None,
            #[cfg(feature = "yt-dlp")]
            download_launcher,
            #[cfg(feature = "yt-dlp")]
            active_download: None,
            report_actions: Box::new(SystemReportActions::new()),
            diagnostic_helpers_cache: None,
            url_open_results,
            url_open_result_sender,
            url_open_pending: 0,
            playback_queue: PlaybackQueue::default(),
            playback_phase: PlaybackPhase::Idle,
            playback_load_kind: PlaybackLoadKind::Regular,
            pending_history: None,
            ignore_replaced_stop: false,
            skipped_advertisement_chapter: None,
            current_media: saved.selected_media,
            current_autoplay_origin: None,
            queued_autoplay_resume_origin: None,
            #[cfg(feature = "tracker-music")]
            pending_tracker_preparation: None,
            selected_start_override: None,
            youtube_music_start_override: None,
            seek_back: VecDeque::new(),
            previous_detail: None,
            detail_navigation_back: VecDeque::new(),
            detail_navigation_forward: VecDeque::new(),
            active_description_video: None,
            last_position_save: Instant::now(),
            last_session_save: Instant::now(),
            last_tick: Instant::now(),
            session_dirty: false,
            unflushed_listen_time: Duration::ZERO,
            diagnostic_only: false,
            quit_on_error_dismiss: false,
        };
        controller.populate_local_screen();
        if controller.view.screen == Screen::Search && !controller.youtube_results.is_empty() {
            controller.cache_search_channel_subscriber_counts();
            controller.request_visible_channel_subscriber_counts();
            controller.request_selected_details();
            controller.view.status_line = format!(
                "{} saved YouTube result{} restored",
                controller.youtube_results.len(),
                if controller.youtube_results.len() == 1 {
                    ""
                } else {
                    "s"
                }
            );
        } else if controller.view.screen == Screen::YouTubeMusic
            && !controller.youtube_music_results.is_empty()
        {
            controller.refresh_youtube_music_rows();
            controller.request_selected_details();
            controller.view.status_line = format!(
                "{} saved YouTube Music track{} restored",
                controller.youtube_music_results.len(),
                if controller.youtube_music_results.len() == 1 {
                    ""
                } else {
                    "s"
                }
            );
        }
        if let Some(error) = subscription_load_error {
            controller.show_error("Could not restore local subscriptions", &error);
        }
        if let Some(error) = session_restore_error {
            controller.show_error("Could not restore the previous session", &error);
        }
        if let Some(error) = search_restore_error {
            controller.show_error("Could not restore the previous YouTube search", &error);
        }
        if let Some(error) = music_search_restore_error {
            controller.show_error(
                "Could not restore the previous YouTube Music search",
                &error,
            );
        }
        if let Some(error) = provider_thread_error {
            controller.show_error("Could not start the provider worker", &error);
        }
        if let Some(error) = local_browse_thread_error {
            controller.show_error("Could not start the Local browser worker", &error);
        }
        controller
    }

    fn send_provider_request(&mut self, request: ProviderRequest, operation: &str) -> bool {
        let Some(sender) = &self.provider_requests else {
            self.show_error_message(
                operation,
                "the provider worker is unavailable; it may have failed during startup",
            );
            return false;
        };
        if sender.send(request).is_err() {
            self.show_error_message(
                operation,
                "the provider worker stopped before accepting the request",
            );
            return false;
        }
        true
    }

    /// Sends one foreground directory listing to its isolated worker.
    fn send_local_browse_request(&mut self, request: LocalBrowseRequest, operation: &str) -> bool {
        let Some(sender) = &self.local_browse_requests else {
            self.show_error_message(
                operation,
                "the Local browser worker is unavailable; it may have failed during startup",
            );
            return false;
        };
        if sender.send(request).is_err() {
            self.show_error_message(
                operation,
                "the Local browser worker stopped before accepting the request",
            );
            return false;
        }
        true
    }

    /// Invalidates responses from an older input route and stops its animation.
    fn supersede_search_generation(&mut self) {
        self.search_generation = self.search_generation.wrapping_add(1);
        self.clear_search_activity();
        #[cfg(feature = "tracker-music")]
        if self.pending_tracker_preparation.take().is_some() {
            self.clear_playback_start_activity();
        }
    }

    /// Starts a submitted provider search at its first animation frame.
    fn begin_search_activity(&mut self, activity: SearchActivity) {
        self.view.search_activity = Some(activity);
        self.view.search_animation_frame = 0;
    }

    /// Stops the named search without disturbing a newer search route.
    fn finish_search_activity(&mut self, activity: SearchActivity) {
        if self.view.search_activity == Some(activity) {
            self.clear_search_activity();
        }
    }

    /// Restores the view's canonical idle-search state.
    fn clear_search_activity(&mut self) {
        self.view.search_activity = None;
        self.view.search_animation_frame = 0;
    }

    /// Advances the ASCII animation once per existing controller tick.
    fn advance_search_animation(&mut self) {
        if self.view.search_activity.is_some() {
            self.view.search_animation_frame = self.view.search_animation_frame.wrapping_add(1);
        }
    }

    /// Starts one event-authoritative playback attempt at its first ASCII frame.
    fn begin_playback_start_activity(&mut self) {
        self.view.playback_starting = true;
        self.view.playback_start_animation_frame = 0;
        self.view.playing_media_id = None;
    }

    /// Clears transient playback-start feedback without changing player state.
    fn clear_playback_start_activity(&mut self) {
        self.view.playback_starting = false;
        self.view.playback_start_animation_frame = 0;
    }

    /// Advances the playback-start animation once per existing controller tick.
    fn advance_playback_start_animation(&mut self) {
        if self.view.playback_starting {
            self.view.playback_start_animation_frame =
                self.view.playback_start_animation_frame.wrapping_add(1);
        }
    }

    fn open_youtube_setup(&mut self) {
        let selected_field = match self.config.providers.youtube_backend {
            YouTubeBackend::Invidious => YouTubeSetupField::InvidiousUrl,
            YouTubeBackend::Auto
                if self.config.providers.youtube_api_key.is_none()
                    && self.config.providers.invidious_base_url.is_some() =>
            {
                YouTubeSetupField::InvidiousUrl
            }
            YouTubeBackend::Official | YouTubeBackend::Auto => YouTubeSetupField::ApiKey,
        };
        self.view.youtube_setup_popup = Some(YouTubeSetupPopupView {
            selected_field,
            api_key: String::new(),
            invidious_url: self
                .config
                .providers
                .invidious_base_url
                .as_ref()
                .map_or_else(String::new, ToString::to_string),
            config_path: self.config.config_file().display().to_string(),
            validation_error: None,
        });
        self.view.search_editing = false;
        self.view.status_line =
            "Choose a YouTube API key or an Invidious instance in the setup popup".to_owned();
    }

    fn append_youtube_setup_character(&mut self, character: char) {
        if character.is_control() {
            return;
        }
        let Some(popup) = self.view.youtube_setup_popup.as_mut() else {
            return;
        };
        let (value, maximum_bytes) = match popup.selected_field {
            YouTubeSetupField::ApiKey => (&mut popup.api_key, 256),
            YouTubeSetupField::InvidiousUrl => (&mut popup.invidious_url, 2_048),
        };
        if value
            .len()
            .checked_add(character.len_utf8())
            .is_some_and(|length| length <= maximum_bytes)
        {
            value.push(character);
            popup.validation_error = None;
        }
    }

    fn delete_youtube_setup_character(&mut self) {
        let Some(popup) = self.view.youtube_setup_popup.as_mut() else {
            return;
        };
        match popup.selected_field {
            YouTubeSetupField::ApiKey => {
                popup.api_key.pop();
            }
            YouTubeSetupField::InvidiousUrl => {
                popup.invidious_url.pop();
            }
        }
        popup.validation_error = None;
    }

    fn set_youtube_setup_error(&mut self, error: impl Into<String>) {
        if let Some(popup) = self.view.youtube_setup_popup.as_mut() {
            popup.validation_error = Some(error.into());
        }
    }

    fn submit_youtube_setup(&mut self) {
        let Some(popup) = self.view.youtube_setup_popup.as_ref() else {
            return;
        };
        let selected_field = popup.selected_field;
        let api_key = popup.api_key.trim().to_owned();
        let invidious_url = popup.invidious_url.trim().to_owned();

        let (provider, setting, provider_name) = match selected_field {
            YouTubeSetupField::ApiKey => {
                let provider = match self.youtube_provider_builder.official(api_key.clone()) {
                    Ok(provider) => provider,
                    Err(error) => {
                        self.set_youtube_setup_error(error);
                        return;
                    }
                };
                (
                    provider,
                    YouTubeProviderSetting::OfficialApiKey(api_key),
                    "official YouTube Data API",
                )
            }
            YouTubeSetupField::InvidiousUrl => {
                let base_url = match url::Url::parse(&invidious_url) {
                    Ok(base_url) => base_url,
                    Err(error) => {
                        self.set_youtube_setup_error(format!(
                            "enter a complete HTTP(S) Invidious URL: {error}"
                        ));
                        return;
                    }
                };
                let provider = match self.youtube_provider_builder.invidious(base_url.clone()) {
                    Ok(provider) => provider,
                    Err(error) => {
                        self.set_youtube_setup_error(error);
                        return;
                    }
                };
                (
                    provider,
                    YouTubeProviderSetting::InvidiousUrl(base_url),
                    "Invidious",
                )
            }
        };

        let channel_statistics_mode = provider.channel_statistics_mode();
        if let Err(error) = self.config.save_youtube_provider(setting) {
            self.set_youtube_setup_error(error.to_string());
            self.show_error("Could not save YouTube provider configuration", &error);
            return;
        }
        if !self.send_provider_request(
            ProviderRequest::ReplaceYouTubeProvider { provider },
            "Could not initialize the YouTube provider",
        ) {
            return;
        }

        self.youtube_channel_statistics_mode = channel_statistics_mode;
        self.youtube_provider_generation = self.youtube_provider_generation.wrapping_add(1);
        self.channel_subscriber_cache.clear();
        self.pending_channel_subscribers.clear();
        self.channel_details_cache.clear();
        self.channel_profile_cache.clear();
        self.channel_details_cache_order.clear();
        self.pending_channel_details.clear();
        self.channel_details_generation = self.channel_details_generation.wrapping_add(1);
        self.scheduled_channel_details = None;
        self.subscription_video_cache.clear();
        self.subscription_cache_order.clear();
        self.youtube_provider_available = true;
        self.view.youtube_setup_popup = None;
        if self.view.screen == Screen::Subscriptions {
            self.view.status_line = format!("Using {provider_name}; retrying subscription videos…");
            self.load_selected_subscription_videos();
        } else {
            self.view.status_line = format!("Using {provider_name}; retrying YouTube search…");
            self.submit_youtube_search(1);
        }
    }

    fn submit_search(&mut self) {
        let query = self.view.search_query.trim().to_owned();
        if query.is_empty() {
            self.view.status_line = "Enter a search query".to_owned();
            return;
        }
        self.view.details_focused = false;
        self.view.details_scroll = 0;

        match search_route(self.view.screen) {
            SearchRoute::YouTube => match parse_local_path_input(&query) {
                Ok(Some(local)) => self.open_local_input(local),
                Ok(None) => match parse_direct_youtube_input(&query) {
                    Ok(Some(direct)) => self.open_direct_video(direct),
                    Ok(None) => match parse_direct_source_input(&query) {
                        Ok(Some(direct)) => {
                            let direct = self.classify_configured_instance(direct);
                            self.open_direct_source(direct);
                        }
                        Ok(None) => self.submit_youtube_search(1),
                        Err(error) => self.view.status_line = error.to_owned(),
                    },
                    Err(error) => self.view.status_line = error.to_owned(),
                },
                Err(error) => self.view.status_line = error.to_string(),
            },
            SearchRoute::YouTubeMusic => match parse_direct_youtube_input(&query) {
                Ok(Some(direct)) => self.open_direct_youtube_music(direct),
                Ok(None) => self.submit_youtube_music_search(query),
                Err(error) => self.view.status_line = error.to_owned(),
            },
            SearchRoute::TrackerArchives => self.submit_tracker_search(query),
            SearchRoute::None => {
                self.view.status_line = "Search is not available on this screen".to_owned();
            }
        }
    }

    fn open_direct_video(&mut self, direct: DirectVideoInput) {
        self.clear_youtube_search_snapshot();
        self.direct_item = None;
        self.resolved_direct = None;
        self.local_results.clear();
        self.supersede_search_generation();
        self.selected_start_override = direct.start_seconds;
        let webpage_url = url::Url::parse(&youtube_video_url(&direct.video_id)).ok();
        self.youtube_results = vec![SearchItem::Video(VideoSummary {
            video_id: direct.video_id.clone(),
            title: format!("YouTube video {}", direct.video_id),
            channel_name: "loading…".to_owned(),
            channel_id: String::new(),
            description: String::new(),
            duration_seconds: None,
            view_count: None,
            published_at: None,
            published_text: None,
            live: false,
            orientation: VideoOrientation::Unknown,
            thumbnails: Vec::new(),
            webpage_url,
            stream_url: None,
        })];
        self.refresh_youtube_rows();
        self.view.selected = 0;
        self.request_selected_details();
        self.view.status_line = direct.start_seconds.map_or_else(
            || format!("Loading YouTube video {}…", direct.video_id),
            |seconds| format!("Loading YouTube video at {}…", format_seconds(seconds)),
        );
    }

    fn open_direct_youtube_music(&mut self, direct: DirectVideoInput) {
        self.supersede_search_generation();
        self.youtube_music_results = vec![SearchItem::Video(VideoSummary {
            video_id: direct.video_id.clone(),
            title: format!("YouTube Music track {}", direct.video_id),
            channel_name: "loading…".to_owned(),
            channel_id: String::new(),
            description: String::new(),
            duration_seconds: None,
            view_count: None,
            published_at: None,
            published_text: None,
            live: false,
            orientation: VideoOrientation::Unknown,
            thumbnails: Vec::new(),
            webpage_url: url::Url::parse(&format!(
                "https://music.youtube.com/watch?v={}",
                direct.video_id
            ))
            .ok(),
            stream_url: None,
        })];
        self.youtube_music_start_override = direct.start_seconds;
        self.refresh_youtube_music_rows();
        self.view.selected = 0;
        self.youtube_music_selected = 0;
        self.request_selected_details();
        self.view.status_line = direct.start_seconds.map_or_else(
            || format!("Loaded YouTube Music track {}", direct.video_id),
            |seconds| {
                format!(
                    "Loaded YouTube Music track {} at {}",
                    direct.video_id,
                    format_seconds(seconds)
                )
            },
        );
    }

    fn open_direct_source(&mut self, direct: DirectSourceInput) {
        self.supersede_search_generation();
        self.clear_youtube_search_snapshot();
        self.youtube_results.clear();
        self.resolved_direct = None;
        self.local_results.clear();
        self.direct_item = Some(direct.clone());
        let host = direct.url.host_str().unwrap_or("remote source");
        self.view.rows = vec![RowView {
            media_id: Some(MediaId::new(direct.source.clone(), direct.url.to_string())),
            title: direct.url.to_string(),
            subtitle: "direct link".to_owned(),
            source: direct.source.to_string(),
            ..RowView::default()
        }];
        self.view.selected = 0;
        self.view.details = Some(DetailView {
            title: direct.url.to_string(),
            source: direct.source.to_string(),
            description: format!(
                "Direct link recognized for {host}. Press Enter to resolve and play it."
            ),
            license: "unknown".to_owned(),
            wikidata: "not loaded".to_owned(),
            ..DetailView::default()
        });
        self.view.status_line = if direct.source == SourceKind::ApplePodcasts {
            if !self.send_provider_request(
                ProviderRequest::ResolveApple {
                    generation: self.search_generation,
                    url: direct.url.clone(),
                },
                "Could not resolve the Apple Podcasts link",
            ) {
                return;
            }
            "Resolving Apple Podcasts metadata and RSS link…".to_owned()
        } else if requires_first_class_direct_resolution(&direct.source) {
            if !self.send_provider_request(
                ProviderRequest::ResolveFirstClass {
                    generation: self.search_generation,
                    direct: direct.clone(),
                },
                "Could not resolve the direct media link",
            ) {
                return;
            }
            format!(
                "Resolving {} metadata…",
                direct_source_label(&direct.source)
            )
        } else if direct.source == SourceKind::RemoteFiles {
            "Direct media-file URL recognized; press Enter to play it without a site extractor"
                .to_owned()
        } else {
            format!(
                "{} link recognized; installed yt-dlp support is checked on playback",
                direct.source
            )
        };
        self.request_direct_wikidata(&direct);
    }

    fn classify_configured_instance(&self, mut direct: DirectSourceInput) -> DirectSourceInput {
        let host = direct.url.host_str();
        if self
            .config
            .providers
            .peertube_instance_url
            .as_ref()
            .and_then(url::Url::host_str)
            == host
        {
            direct.source = SourceKind::PeerTube;
        } else if self
            .config
            .providers
            .funkwhale_instance_url
            .as_ref()
            .and_then(url::Url::host_str)
            == host
        {
            direct.source = SourceKind::Funkwhale;
        }
        direct
    }

    fn open_local_input(&mut self, local: DirectLocalInput) {
        self.supersede_search_generation();
        self.clear_youtube_search_snapshot();
        self.youtube_results.clear();
        self.direct_item = None;
        self.resolved_direct = None;
        self.local_results.clear();
        self.view.selected = 0;
        self.view.details = Some(DetailView {
            title: local.path.display().to_string(),
            source: "Local".to_owned(),
            description: if local.directory {
                "Scanning supported audio, video, and tracker-module files in place…".to_owned()
            } else {
                "Local media is read in place. Youta does not move or modify it.".to_owned()
            },
            license: "local file".to_owned(),
            wikidata: "not applicable".to_owned(),
            ..DetailView::default()
        });
        if local.directory {
            if !self.send_provider_request(
                ProviderRequest::ScanLocal {
                    generation: self.search_generation,
                    root: local.path.clone(),
                },
                "Could not scan the local folder",
            ) {
                return;
            }
            self.view.rows = vec![RowView {
                title: local.path.display().to_string(),
                subtitle: "scanning directory…".to_owned(),
                source: "Local folder".to_owned(),
                ..RowView::default()
            }];
            self.view.status_line = format!("Scanning {} in the background…", local.path.display());
        } else {
            self.local_results.push(local_media_item(local.path));
            self.refresh_local_rows();
            self.view.status_line = "Local media file recognized; press Enter to play".to_owned();
        }
    }

    fn submit_tracker_search(&mut self, query: String) {
        self.supersede_search_generation();
        self.tracker_search_query.clone_from(&query);
        self.tracker_results.clear();
        self.view.rows.clear();
        self.view.details = None;
        self.view.selected = 0;
        self.tracker_selected = 0;
        if !self.send_provider_request(
            ProviderRequest::TrackerSearch {
                generation: self.search_generation,
                query,
            },
            "Could not start the tracker archive search",
        ) {
            return;
        }
        self.begin_search_activity(SearchActivity::TrackerArchives);
        self.view.status_line =
            "Searching enabled MOD/tracker archives (separate from YouTube)…".to_owned();
    }

    #[cfg(feature = "youtube-music")]
    fn submit_youtube_music_search(&mut self, query: String) {
        self.supersede_search_generation();
        self.youtube_music_search_query.clone_from(&query);
        self.youtube_music_start_override = None;
        self.youtube_music_results.clear();
        if let Err(error) = self.store.clear_youtube_music_search() {
            self.show_error("Could not clear the saved YouTube Music search", &error);
        }
        self.view.rows.clear();
        self.view.details = None;
        self.view.selected = 0;
        self.youtube_music_selected = 0;
        if !self.send_provider_request(
            ProviderRequest::YouTubeMusicSearch {
                generation: self.search_generation,
                query,
            },
            "Could not start the YouTube Music search",
        ) {
            return;
        }
        self.begin_search_activity(SearchActivity::YouTubeMusic);
        self.view.status_line = "Searching YouTube Music through yt-dlp…".to_owned();
    }

    #[cfg(not(feature = "youtube-music"))]
    fn submit_youtube_music_search(&mut self, _query: String) {
        self.view.status_line =
            "This build omits the `youtube-music` feature; rebuild with it enabled".to_owned();
    }

    fn submit_youtube_search(&mut self, page: u32) {
        if !self.youtube_provider_available {
            self.open_youtube_setup();
            return;
        }
        if self.view.search_activity == Some(SearchActivity::YouTube) {
            return;
        }
        let supersedes_other_search = self.view.search_activity.is_some();
        if page == 1 || supersedes_other_search {
            self.supersede_search_generation();
        }
        if page == 1 {
            self.clear_youtube_search_snapshot();
            self.youtube_search_query
                .clone_from(&self.view.search_query);
            self.selected_start_override = None;
            self.direct_item = None;
            self.resolved_direct = None;
            self.local_results.clear();
            self.youtube_results.clear();
            self.view.rows.clear();
            self.view.details = None;
            self.view.selected = 0;
            self.youtube_selected = 0;
        }
        let target = match self.view.search_kind {
            SearchKind::Videos => SearchTarget::Videos,
            SearchKind::Channels => SearchTarget::Channels,
        };
        let mut request = if page > 1 {
            self.youtube_search_request
                .clone()
                .unwrap_or_else(|| SearchRequest::new(self.view.search_query.clone(), target))
        } else {
            SearchRequest::new(self.view.search_query.clone(), target)
        };
        request.page = page;
        if page == 1 {
            request.sort = match self.view.youtube_search_sort {
                YouTubeSearchSort::Relevance => ProviderSearchSort::Relevance,
                YouTubeSearchSort::Newest => ProviderSearchSort::UploadDate,
            };
            if target == SearchTarget::Videos && self.view.youtube_creative_commons_only {
                request
                    .filters
                    .features
                    .push(SearchFeature::CreativeCommons);
            }
        }
        if !self.send_provider_request(
            ProviderRequest::Search {
                generation: self.search_generation,
                request,
            },
            "Could not start the YouTube search",
        ) {
            return;
        }
        self.begin_search_activity(SearchActivity::YouTube);
        self.view.status_line = format!(
            "Searching YouTube {}…",
            match self.view.search_kind {
                SearchKind::Videos => "videos",
                SearchKind::Channels => "channels",
            }
        );
    }

    /// Drops a search snapshot when another input route replaces its rows.
    fn clear_youtube_search_snapshot(&mut self) {
        self.youtube_search_request = None;
        self.next_youtube_page = None;
        if let Err(error) = self.store.clear_youtube_search() {
            self.show_error("Could not clear the saved YouTube search", &error);
        }
    }

    /// Starts the isolated resolver worker only when an eligible selection
    /// actually reaches the end of its debounce.
    #[cfg(feature = "yt-dlp")]
    fn ensure_youtube_prewarm_worker(&mut self) -> bool {
        if self.youtube_prewarm_requests.is_some() {
            return true;
        }
        #[cfg(test)]
        if self.config.providers.yt_dlp_executable == Path::new("yt-dlp") {
            // Unit tests opt in with an explicit mock executable. Integration
            // and production builds use the configured helper normally.
            return false;
        }
        let resolver = YouTubePrewarmResolver::new(YouTubePrewarmConfig {
            executable: self.config.providers.yt_dlp_executable.clone(),
            ..YouTubePrewarmConfig::default()
        });
        let (request_sender, request_receiver) = bounded(1);
        let request_drain = request_receiver.clone();
        let (response_sender, response_receiver) = bounded(1);
        let response_drain = response_receiver.clone();
        let thread = thread::Builder::new()
            .name("youta-youtube-prewarm".to_owned())
            .spawn(move || {
                youtube_prewarm_worker(request_receiver, response_sender, response_drain, resolver);
            });
        let Ok(thread) = thread else {
            return false;
        };
        self.youtube_prewarm_requests = Some(request_sender);
        self.youtube_prewarm_request_drain = Some(request_drain);
        self.youtube_prewarm_responses = Some(response_receiver);
        self.youtube_prewarm_thread = Some(thread);
        true
    }

    /// Cancels and forgets every speculative result without touching playback.
    #[cfg(feature = "yt-dlp")]
    fn cancel_youtube_prewarm(&mut self) {
        self.youtube_prewarm_generation = self.youtube_prewarm_generation.wrapping_add(1);
        self.scheduled_youtube_prewarm = None;
        self.ready_youtube_prewarm = None;
        if let Some(active) = self.active_youtube_prewarm.take() {
            active.cancellation.cancel();
        }
        if let Some(receiver) = self.youtube_prewarm_request_drain.as_ref() {
            while let Ok(command) = receiver.try_recv() {
                if let YouTubePrewarmCommand::Resolve(job) = command {
                    job.cancellation.cancel();
                }
            }
        }
        if let Some(receiver) = self.youtube_prewarm_responses.as_ref() {
            while receiver.try_recv().is_ok() {}
        }
    }

    /// Cancels the extractor child, stops its worker, and reaps the thread.
    #[cfg(feature = "yt-dlp")]
    fn shutdown_youtube_prewarm(&mut self) {
        self.cancel_youtube_prewarm();
        if let Some(sender) = self.youtube_prewarm_requests.take() {
            let _ = sender.send(YouTubePrewarmCommand::Shutdown);
        }
        if let Some(thread) = self.youtube_prewarm_thread.take() {
            let _ = thread.join();
        }
        self.youtube_prewarm_request_drain = None;
        self.youtube_prewarm_responses = None;
    }

    /// Returns whether a resolved direct stream remains safe to try now.
    #[cfg(feature = "yt-dlp")]
    fn youtube_prewarm_is_fresh(ready: &ReadyYouTubePrewarm, now: Instant) -> bool {
        if now.saturating_duration_since(ready.completed_at) > YOUTUBE_PREWARM_TTL {
            return false;
        }
        let now_unix = u64::try_from(unix_time()).unwrap_or_default();
        ready.audio.expires_at_unix().is_none_or(|expires_at| {
            expires_at > now_unix.saturating_add(YOUTUBE_PREWARM_EXPIRY_MARGIN_SECONDS)
        })
    }

    /// Replaces speculative work with one debounced eligible `YouTube` video.
    #[cfg(feature = "yt-dlp")]
    fn update_youtube_prewarm_for_selection(&mut self, selected: Option<&SearchItem>) {
        if !self.config.playback.youtube_prewarm
            || (self.playback_factory.is_none() && self.player.is_none())
        {
            self.cancel_youtube_prewarm();
            return;
        }
        let Some(SearchItem::Video(video)) = selected else {
            self.cancel_youtube_prewarm();
            return;
        };
        let media_id = MediaId::new(SourceKind::YouTube, video.video_id.clone());
        if video.live
            || video.duration_seconds == Some(0)
            || (self.playback_phase != PlaybackPhase::Idle
                && self.current_media.as_ref() == Some(&media_id))
        {
            self.cancel_youtube_prewarm();
            return;
        }
        let now = Instant::now();
        if self.ready_youtube_prewarm.as_ref().is_some_and(|ready| {
            ready.media_id == media_id && Self::youtube_prewarm_is_fresh(ready, now)
        }) || self
            .scheduled_youtube_prewarm
            .as_ref()
            .is_some_and(|scheduled| scheduled.media_id == media_id)
            || self
                .active_youtube_prewarm
                .as_ref()
                .is_some_and(|active| active.media_id == media_id)
        {
            return;
        }
        if self
            .youtube_prewarm_failure
            .as_ref()
            .is_some_and(|(failed, retry_at)| failed == &media_id && now < *retry_at)
        {
            self.cancel_youtube_prewarm();
            return;
        }
        self.cancel_youtube_prewarm();
        let generation = self.youtube_prewarm_generation;
        let source_url = url::Url::parse(&youtube_video_url(&video.video_id))
            .expect("a validated YouTube video ID always forms a valid URL");
        self.scheduled_youtube_prewarm = Some(ScheduledYouTubePrewarm {
            generation,
            media_id,
            source_url,
            due_at: now + YOUTUBE_PREWARM_DEBOUNCE,
        });
    }

    /// Dispatches the stable selected video without blocking the terminal.
    #[cfg(feature = "yt-dlp")]
    fn request_due_youtube_prewarm(&mut self, now: Instant) {
        let Some(scheduled) = self.scheduled_youtube_prewarm.as_ref() else {
            return;
        };
        if now < scheduled.due_at {
            return;
        }
        let scheduled = self
            .scheduled_youtube_prewarm
            .take()
            .expect("a due speculative request was checked above");
        if scheduled.generation != self.youtube_prewarm_generation
            || !self.ensure_youtube_prewarm_worker()
        {
            return;
        }
        if let Some(receiver) = self.youtube_prewarm_request_drain.as_ref() {
            while let Ok(command) = receiver.try_recv() {
                if let YouTubePrewarmCommand::Resolve(job) = command {
                    job.cancellation.cancel();
                }
            }
        }
        let cancellation = YouTubePrewarmCancellation::new();
        let command = YouTubePrewarmCommand::Resolve(YouTubePrewarmJob {
            media_id: scheduled.media_id.clone(),
            request: YouTubePrewarmRequest::new(scheduled.generation, scheduled.source_url),
            cancellation: cancellation.clone(),
        });
        let Some(sender) = self.youtube_prewarm_requests.as_ref() else {
            return;
        };
        match sender.try_send(command) {
            Ok(()) => {
                self.active_youtube_prewarm = Some(ActiveYouTubePrewarm {
                    generation: scheduled.generation,
                    media_id: scheduled.media_id,
                    cancellation,
                });
            }
            Err(
                TrySendError::Full(YouTubePrewarmCommand::Resolve(job))
                | TrySendError::Disconnected(YouTubePrewarmCommand::Resolve(job)),
            ) => {
                job.cancellation.cancel();
            }
            Err(
                TrySendError::Full(YouTubePrewarmCommand::Shutdown)
                | TrySendError::Disconnected(YouTubePrewarmCommand::Shutdown),
            ) => {}
        }
    }

    /// Accepts only the latest exact completion and keeps failures silent.
    #[cfg(feature = "yt-dlp")]
    fn drain_youtube_prewarm_responses(&mut self) {
        loop {
            let completion = self
                .youtube_prewarm_responses
                .as_ref()
                .and_then(|receiver| receiver.try_recv().ok());
            let Some(completion) = completion else {
                break;
            };
            let generation = completion.result.generation();
            if self
                .active_youtube_prewarm
                .as_ref()
                .is_some_and(|active| active.generation == generation)
            {
                self.active_youtube_prewarm = None;
            }
            if !completion.result.is_latest(self.youtube_prewarm_generation) {
                continue;
            }
            match completion.result.into_outcome() {
                Ok(audio) => {
                    let ready = ReadyYouTubePrewarm {
                        media_id: completion.media_id,
                        completed_at: completion.completed_at,
                        audio,
                    };
                    if Self::youtube_prewarm_is_fresh(&ready, Instant::now()) {
                        self.ready_youtube_prewarm = Some(ready);
                        self.youtube_prewarm_failure = None;
                    }
                }
                Err(YouTubePrewarmError::Cancelled) => {}
                Err(_) => {
                    self.youtube_prewarm_failure = Some((
                        completion.media_id,
                        Instant::now() + YOUTUBE_PREWARM_FAILURE_BACKOFF,
                    ));
                }
            }
        }
    }

    /// Consumes one exact fresh direct stream and cancels competing extraction.
    #[cfg(feature = "yt-dlp")]
    fn take_ready_youtube_prewarm(&mut self, media_id: &MediaId) -> Option<PrewarmedYouTubeAudio> {
        self.drain_youtube_prewarm_responses();
        let ready = self
            .ready_youtube_prewarm
            .take()
            .filter(|ready| {
                &ready.media_id == media_id && Self::youtube_prewarm_is_fresh(ready, Instant::now())
            })
            .map(|ready| ready.audio);
        self.cancel_youtube_prewarm();
        ready
    }

    fn request_selected_details(&mut self) {
        self.clear_detail_navigation_history();
        self.previous_detail = None;
        self.view.details_scroll = 0;
        let Some(selected) = self.selected_youtube_item().cloned() else {
            self.details_generation = self.details_generation.wrapping_add(1);
            #[cfg(feature = "yt-dlp")]
            self.update_youtube_prewarm_for_selection(None);
            self.view.details = None;
            return;
        };
        #[cfg(feature = "yt-dlp")]
        self.update_youtube_prewarm_for_selection(Some(&selected));
        if matches!(selected, SearchItem::Channel(_)) {
            self.request_selected_channel_subscriber_count(&selected);
            self.show_selected_channel();
            return;
        }
        self.details_generation = self.details_generation.wrapping_add(1);
        let mut detail = preliminary_detail(&selected, &self.subscription_tree);
        if self.view.screen == Screen::YouTubeMusic {
            detail.source = "YouTube Music".to_owned();
        }
        self.view.details = Some(detail);
        if let SearchItem::Video(video) = &selected
            && self.youtube_provider_available
        {
            self.send_provider_request(
                ProviderRequest::Details {
                    generation: self.details_generation,
                    video_id: video.video_id.clone(),
                },
                "Could not load YouTube video details",
            );
        }
        self.request_selected_channel_subscriber_count(&selected);
        self.request_selected_wikidata(&selected);
    }

    /// Seeds subscriber counts already present in channel-search results.
    fn cache_search_channel_subscriber_counts(&mut self) {
        for item in &self.youtube_results {
            if let SearchItem::Channel(channel) = item
                && !channel.channel_id.is_empty()
            {
                self.channel_subscriber_cache
                    .insert(channel.channel_id.clone(), channel.subscriber_count);
            }
        }
    }

    /// Requests one selected channel from providers that cannot batch lookups.
    fn request_selected_channel_subscriber_count(&mut self, selected: &SearchItem) {
        let channel_id = match selected {
            SearchItem::Video(video) => &video.channel_id,
            SearchItem::Channel(channel) => &channel.channel_id,
        };
        if channel_id.is_empty() {
            return;
        }
        match self.youtube_channel_statistics_mode {
            ChannelStatisticsMode::Unsupported => {}
            ChannelStatisticsMode::SelectedOnly | ChannelStatisticsMode::Batch { .. } => {
                self.request_channel_subscriber_counts([channel_id.clone()]);
            }
        }
    }

    /// Batches uncached channel statistics for the currently loaded video rows.
    fn request_visible_channel_subscriber_counts(&mut self) {
        let ChannelStatisticsMode::Batch { max_ids } = self.youtube_channel_statistics_mode else {
            return;
        };
        let mut seen = HashSet::new();
        let ids = self
            .youtube_results
            .iter()
            .filter_map(|item| match item {
                SearchItem::Video(video) if !video.channel_id.is_empty() => {
                    Some(video.channel_id.clone())
                }
                _ => None,
            })
            .filter(|channel_id| seen.insert(channel_id.clone()))
            .filter(|channel_id| {
                !self.channel_subscriber_cache.contains_key(channel_id)
                    && !self.pending_channel_subscribers.contains(channel_id)
            })
            .take(max_ids)
            .collect::<Vec<_>>();
        self.request_channel_subscriber_counts(ids);
    }

    /// Sends one optional RAM-cached subscriber lookup without blocking the TUI.
    fn request_channel_subscriber_counts(&mut self, channel_ids: impl IntoIterator<Item = String>) {
        let channel_ids = channel_ids
            .into_iter()
            .filter(|channel_id| {
                !self.channel_subscriber_cache.contains_key(channel_id)
                    && !self.pending_channel_subscribers.contains(channel_id)
            })
            .collect::<Vec<_>>();
        if channel_ids.is_empty() {
            return;
        }
        self.pending_channel_subscribers
            .extend(channel_ids.iter().cloned());
        let request = ProviderRequest::ChannelSubscriberCounts {
            provider_generation: self.youtube_provider_generation,
            channel_ids: channel_ids.clone(),
        };
        let sent = self
            .provider_requests
            .as_ref()
            .is_some_and(|sender| sender.send(request).is_ok());
        if !sent {
            for channel_id in channel_ids {
                self.pending_channel_subscribers.remove(&channel_id);
            }
        }
    }

    /// Returns the exact channel represented by the visible Channel panel.
    fn visible_channel_id(&self) -> Option<&str> {
        (self.view.right_panel_mode == RightPanelMode::Channel)
            .then(|| self.view.details.as_ref())
            .flatten()
            .map(|details| details.channel_id.as_str())
            .filter(|channel_id| !channel_id.is_empty())
    }

    /// Returns the YouTube channel owning the active Subscriptions view.
    fn selected_subscription_channel_id(&self) -> Option<String> {
        self.subscription_entries
            .get(self.view.subscriptions.selected_source)
            .and_then(|entry| entry.subscription.youtube_channel_id())
            .or_else(|| self.active_subscription_channel_id.clone())
    }

    /// Schedules metadata only after the selected subscription source settles.
    fn schedule_selected_subscription_channel_details(&mut self, now: Instant) {
        self.scheduled_channel_details = None;
        let Some(entry) = self
            .subscription_entries
            .get(self.view.subscriptions.selected_source)
        else {
            #[cfg(feature = "wikidata")]
            self.invalidate_wikidata_lookup();
            return;
        };
        if entry.subscription.kind != SubscriptionKind::YouTube {
            #[cfg(feature = "wikidata")]
            self.invalidate_wikidata_lookup();
            return;
        }
        let Some(channel_id) = entry.subscription.youtube_channel_id() else {
            #[cfg(feature = "wikidata")]
            self.invalidate_wikidata_lookup();
            return;
        };
        self.schedule_channel_details(channel_id.clone(), now);
        #[cfg(feature = "wikidata")]
        self.schedule_channel_wikidata(channel_id, now);
    }

    /// Schedules metadata for the channel already visible in the Details pane.
    fn schedule_visible_channel_details(&mut self, now: Instant) {
        self.scheduled_channel_details = None;
        let Some(channel_id) = self.visible_channel_id().map(str::to_owned) else {
            return;
        };
        self.schedule_channel_details(channel_id, now);
    }

    /// Applies fresh cached metadata or schedules one bounded provider lookup.
    fn schedule_channel_details(&mut self, channel_id: String, now: Instant) {
        let now_unix = unix_time();
        let profile_cached =
            if let Some(profile) = self.channel_profile_cache.get(&channel_id).cloned() {
                self.touch_channel_details_cache(&channel_id);
                self.apply_channel_profile_to_view(&profile);
                true
            } else {
                false
            };
        if let Some(cached) = self.channel_details_cache.get(&channel_id).cloned() {
            self.touch_channel_details_cache(&channel_id);
            if let Some(channel) = cached {
                self.apply_channel_details_to_view(&channel);
                if self
                    .channel_details_fresh_until
                    .get(&channel_id)
                    .is_some_and(|expires_at| *expires_at > now_unix)
                    && profile_cached
                {
                    return;
                }
            }
        } else {
            match self.store.cached_channel_summary(&channel_id) {
                Ok(Some(cached)) => {
                    self.channel_details_fresh_until
                        .insert(channel_id.clone(), cached.expires_at);
                    self.cache_channel_details(channel_id.clone(), Some(cached.summary.clone()));
                    self.apply_channel_details_to_view(&cached.summary);
                    if cached.is_fresh_at(now_unix) && profile_cached {
                        return;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.show_error("Could not read cached channel info", &error);
                }
            }
        }
        if self.pending_channel_details.contains(&channel_id) {
            return;
        }
        self.scheduled_channel_details = Some(ScheduledChannelDetails {
            generation: self.channel_details_generation,
            channel_id,
            due_at: now + CHANNEL_DETAILS_DEBOUNCE,
        });
    }

    /// Starts one settled visible-channel request without blocking the TUI.
    fn request_due_channel_details(&mut self, now: Instant) {
        let Some(scheduled) = self.scheduled_channel_details.take() else {
            return;
        };
        if now < scheduled.due_at {
            self.scheduled_channel_details = Some(scheduled);
            return;
        }
        if scheduled.generation != self.channel_details_generation
            || self.visible_channel_id() != Some(scheduled.channel_id.as_str())
        {
            return;
        }
        self.request_channel_details(scheduled.channel_id, scheduled.generation);
    }

    /// Sends one exact channel-details request or applies the bounded RAM cache.
    fn request_channel_details(&mut self, channel_id: String, generation: u64) {
        if let Some(cached) = self.channel_details_cache.get(&channel_id).cloned() {
            if let Some(channel) = cached.as_ref() {
                self.touch_channel_details_cache(&channel_id);
                if generation == self.channel_details_generation
                    && self.visible_channel_id() == Some(channel_id.as_str())
                {
                    self.apply_channel_details_to_view(channel);
                    self.view.status_line =
                        format!("Using cached channel info for {}", channel.name);
                }
                if self
                    .channel_details_fresh_until
                    .get(&channel_id)
                    .is_some_and(|expires_at| *expires_at > unix_time())
                    && self.channel_profile_cache.contains_key(&channel_id)
                {
                    return;
                }
            }
            if cached.is_none() {
                return;
            }
        }
        if self.pending_channel_details.contains(&channel_id) {
            return;
        }
        if !self.youtube_provider_available {
            return;
        }
        let request = ProviderRequest::ChannelDetails {
            generation,
            provider_generation: self.youtube_provider_generation,
            channel_id: channel_id.clone(),
        };
        let sent = self
            .provider_requests
            .as_ref()
            .is_some_and(|sender| sender.send(request).is_ok());
        if !sent {
            return;
        }
        self.pending_channel_details.insert(channel_id.clone());
        if generation == self.channel_details_generation
            && self.visible_channel_id() == Some(channel_id.as_str())
        {
            self.view.status_line = "Loading channel info…".to_owned();
        }
    }

    /// Inserts one compact positive or negative entry into the bounded LRU.
    fn cache_channel_details(&mut self, channel_id: String, channel: Option<ChannelSummary>) {
        if !self.channel_details_cache.contains_key(&channel_id) {
            while self.channel_details_cache.len() >= MAX_CACHED_CHANNEL_DETAILS {
                let Some(oldest) = self.channel_details_cache_order.pop_front() else {
                    break;
                };
                self.channel_details_cache.remove(&oldest);
                self.channel_profile_cache.remove(&oldest);
                self.channel_details_fresh_until.remove(&oldest);
            }
        }
        self.channel_details_cache
            .insert(channel_id.clone(), channel);
        self.touch_channel_details_cache(&channel_id);
    }

    /// Marks one channel record as most recently used.
    fn touch_channel_details_cache(&mut self, channel_id: &str) {
        self.channel_details_cache_order
            .retain(|cached| cached != channel_id);
        self.channel_details_cache_order
            .push_back(channel_id.to_owned());
    }

    /// Applies provider metadata only to a matching visible Channel panel.
    fn apply_channel_details_to_view(&mut self, channel: &ChannelSummary) {
        self.apply_channel_artwork_to_subscription_source(channel);
        let is_visible_subscription_source = self.view.screen == Screen::Subscriptions
            && self.visible_channel_id() == Some(channel.channel_id.as_str());
        let Some(details) = self
            .view
            .details
            .as_mut()
            .filter(|details| details.channel_id == channel.channel_id)
        else {
            return;
        };
        if self.view.screen != Screen::Subscriptions {
            details.title.clone_from(&channel.name);
        }
        details.channel_name.clone_from(&channel.name);
        details.channel_subscriber_count = channel.subscriber_count;
        details.channel_video_count = channel.video_count;
        details.channel_created = channel
            .created_at
            .map(format_unix_utc_date)
            .unwrap_or_default();
        details.description.clone_from(&channel.description);
        details.thumbnail_url = preferred_thumbnail_url(&channel.thumbnails);
        details.channel_webpage_url =
            youtube_channel_webpage_url(&channel.channel_id, channel.webpage_url.clone());
        details.channel_subscribed = self
            .subscription_tree
            .contains_youtube_channel(&channel.channel_id);
        if is_visible_subscription_source {
            self.view.subscriptions.source_subscriber_count = channel.subscriber_count;
            self.view.subscriptions.source_created = channel
                .created_at
                .map(format_unix_utc_date)
                .unwrap_or_default();
        }
        self.refresh_youtube_rows();
        if self.active_subscription_channel_id.is_some() {
            self.refresh_subscription_video_rows();
        }
    }

    /// Applies selected-only counts, country, and channel-advertised links.
    fn apply_channel_profile_to_view(&mut self, profile: &ChannelDetails) {
        self.apply_channel_details_to_view(&profile.summary);
        let Some(details) = self
            .view
            .details
            .as_mut()
            .filter(|details| details.channel_id == profile.summary.channel_id)
        else {
            return;
        };
        details.channel_total_view_count = profile.total_view_count;
        details.channel_country = profile.country.clone().unwrap_or_default();
        details.channel_links_truncated = profile.external_links_truncated;

        let wikidata_links = details
            .links
            .iter()
            .filter(|link| link.wikidata_item_id.is_some())
            .cloned()
            .collect::<Vec<_>>();
        let mut seen_urls = HashSet::new();
        details.links = profile
            .external_links
            .iter()
            .filter(|link| seen_urls.insert(link.url.as_str().to_owned()))
            .map(|link| DetailLinkView {
                label: channel_external_link_label(link.kind, &link.label),
                url: link.url.to_string(),
                wikidata_item_id: None,
            })
            .collect();
        details.links.extend(wikidata_links);
        let link_count = details.links.len();
        if self
            .view
            .selected_detail_link
            .is_some_and(|selected| selected >= link_count)
        {
            self.view.selected_detail_link = (link_count > 0).then_some(0);
        }
    }

    /// Updates the matching source row so the thumbnail worker can warm channel
    /// artwork before that source becomes selected.
    fn apply_channel_artwork_to_subscription_source(&mut self, channel: &ChannelSummary) {
        let Some(index) = self.subscription_entries.iter().position(|entry| {
            entry.subscription.youtube_channel_id().as_deref() == Some(&channel.channel_id)
        }) else {
            return;
        };
        if let Some(row) = self.view.subscriptions.sources.get_mut(index) {
            row.thumbnail_url = preferred_thumbnail_url(&channel.thumbnails);
        }
    }

    #[cfg(feature = "wikidata")]
    fn request_selected_wikidata(&mut self, selected: &SearchItem) {
        use crate::providers::wikidata::WikidataExternalKind;

        match selected {
            SearchItem::Video(video) => {
                self.request_wikidata(WikidataExternalKind::YouTubeVideo, &video.video_id);
            }
            SearchItem::Channel(channel) if !channel.channel_id.is_empty() => {
                self.request_wikidata(WikidataExternalKind::YouTubeChannel, &channel.channel_id);
            }
            SearchItem::Channel(_) => {
                self.invalidate_wikidata_lookup();
                if let Some(details) = self.view.details.as_mut() {
                    details.wikidata = "channel ID unavailable".to_owned();
                }
            }
        }
    }

    #[cfg(not(feature = "wikidata"))]
    fn request_selected_wikidata(&mut self, _selected: &SearchItem) {
        if let Some(details) = self.view.details.as_mut() {
            details.wikidata = "disabled at build time".to_owned();
        }
    }

    #[cfg(feature = "wikidata")]
    fn request_direct_wikidata(&mut self, direct: &DirectSourceInput) {
        use crate::providers::wikidata::{
            WikidataExternalKind, bilibili_channel_external_id, bilibili_video_external_id,
            soundcloud_external_id,
        };

        let lookup = match direct.source {
            SourceKind::SoundCloud => soundcloud_external_id(&direct.url)
                .map(|external_id| (WikidataExternalKind::SoundCloud, external_id)),
            SourceKind::Bilibili => bilibili_video_external_id(&direct.url)
                .map(|external_id| (WikidataExternalKind::BilibiliVideo, external_id))
                .or_else(|| {
                    bilibili_channel_external_id(&direct.url)
                        .map(|external_id| (WikidataExternalKind::BilibiliChannel, external_id))
                }),
            _ => None,
        };
        if let Some((kind, external_id)) = lookup {
            self.request_wikidata(kind, &external_id);
        } else if matches!(direct.source, SourceKind::SoundCloud | SourceKind::Bilibili) {
            self.invalidate_wikidata_lookup();
            if let Some(details) = self.view.details.as_mut() {
                details.wikidata = "no exact external ID in this link".to_owned();
            }
        }
    }

    #[cfg(not(feature = "wikidata"))]
    fn request_direct_wikidata(&mut self, direct: &DirectSourceInput) {
        if matches!(direct.source, SourceKind::SoundCloud | SourceKind::Bilibili)
            && let Some(details) = self.view.details.as_mut()
        {
            details.wikidata = "disabled at build time".to_owned();
        }
    }

    #[cfg(feature = "wikidata")]
    fn request_wikidata(
        &mut self,
        kind: crate::providers::wikidata::WikidataExternalKind,
        external_id: &str,
    ) {
        self.invalidate_wikidata_lookup();
        let generation = self.wikidata_generation;
        if self.apply_fresh_cached_wikidata(kind, external_id) {
            return;
        }
        self.send_wikidata_request(generation, kind, external_id);
    }

    /// Invalidates pending work before a different Details identity is shown.
    #[cfg(feature = "wikidata")]
    fn invalidate_wikidata_lookup(&mut self) {
        self.wikidata_generation = self.wikidata_generation.wrapping_add(1);
        self.scheduled_channel_wikidata = None;
    }

    /// Reapplies a fresh positive or empty cache entry to the visible panel.
    #[cfg(feature = "wikidata")]
    fn apply_fresh_cached_wikidata(
        &mut self,
        kind: crate::providers::wikidata::WikidataExternalKind,
        external_id: &str,
    ) -> bool {
        let property_id = kind.property_id();
        let now = unix_time();
        match self.store.cached_wikidata(property_id, external_id) {
            Ok(Some(cached)) if cached.is_fresh_at(now) => {
                if let Some(details) = self.view.details.as_mut() {
                    apply_wikidata_links(details, &cached.items);
                }
                self.view.selected_detail_link = (!cached.items.is_empty()).then_some(0);
                true
            }
            Ok(_) => false,
            Err(error) => {
                self.show_error("Wikidata cache could not be read", &error);
                false
            }
        }
    }

    /// Sends one generation-owned Wikidata request without blocking the TUI.
    #[cfg(feature = "wikidata")]
    fn send_wikidata_request(
        &mut self,
        generation: u64,
        kind: crate::providers::wikidata::WikidataExternalKind,
        external_id: &str,
    ) {
        let property_id = kind.property_id();
        if let Some(details) = self.view.details.as_mut() {
            details.wikidata = format!("loading {property_id} lazily…");
        }
        if !self.send_provider_request(
            ProviderRequest::Wikidata {
                generation,
                kind,
                external_id: external_id.to_owned(),
            },
            "Could not start the Wikidata lookup",
        ) && let Some(details) = self.view.details.as_mut()
        {
            details.wikidata = "provider worker unavailable".to_owned();
        }
    }

    /// Restores a cached channel item immediately, otherwise deferring network
    /// work until the subscription selection has remained stable.
    #[cfg(feature = "wikidata")]
    fn schedule_channel_wikidata(&mut self, channel_id: String, now: Instant) {
        use crate::providers::wikidata::WikidataExternalKind;

        self.invalidate_wikidata_lookup();
        let generation = self.wikidata_generation;
        if self.apply_fresh_cached_wikidata(WikidataExternalKind::YouTubeChannel, &channel_id) {
            return;
        }
        if let Some(details) = self.view.details.as_mut() {
            details.wikidata.clear();
        }
        self.scheduled_channel_wikidata = Some(ScheduledChannelWikidata {
            generation,
            channel_id,
            due_at: now + CHANNEL_DETAILS_DEBOUNCE,
        });
    }

    /// Starts a stable subscription channel's deferred Wikidata lookup.
    #[cfg(feature = "wikidata")]
    fn request_due_channel_wikidata(&mut self, now: Instant) {
        use crate::providers::wikidata::WikidataExternalKind;

        let Some(scheduled) = self.scheduled_channel_wikidata.take() else {
            return;
        };
        if now < scheduled.due_at {
            self.scheduled_channel_wikidata = Some(scheduled);
            return;
        }
        if scheduled.generation != self.wikidata_generation
            || self.visible_channel_id() != Some(scheduled.channel_id.as_str())
        {
            return;
        }
        self.send_wikidata_request(
            scheduled.generation,
            WikidataExternalKind::YouTubeChannel,
            &scheduled.channel_id,
        );
    }

    fn handle_provider_response(&mut self, response: ProviderResponse) {
        match response {
            ProviderResponse::Search {
                generation,
                request,
                result,
            } => {
                if generation != self.search_generation {
                    return;
                }
                self.finish_search_activity(SearchActivity::YouTube);
                match result {
                    Ok(page) => {
                        if page.page != request.page
                            || page.next_page.is_some_and(|next_page| {
                                next_page <= request.page || next_page > 10_000
                            })
                        {
                            self.show_error_message(
                                "YouTube search failed",
                                "the provider returned inconsistent pagination state",
                            );
                            return;
                        }
                        let page_number = page.page;
                        if page_number == 1 {
                            self.youtube_results.clear();
                        }
                        let remaining = MAX_SAVED_YOUTUBE_SEARCH_RESULTS
                            .saturating_sub(self.youtube_results.len());
                        let received_items = page.items.len();
                        self.youtube_results
                            .extend(page.items.into_iter().take(remaining));
                        let search_limit_reached = received_items > remaining
                            || (self.youtube_results.len() >= MAX_SAVED_YOUTUBE_SEARCH_RESULTS
                                && page.next_page.is_some());
                        self.next_youtube_page = if search_limit_reached {
                            None
                        } else {
                            page.next_page
                        };
                        self.cache_search_channel_subscriber_counts();
                        let saved_search = SavedYouTubeSearch {
                            request: request.clone(),
                            results: std::mem::take(&mut self.youtube_results),
                            next_page: self.next_youtube_page,
                        };
                        self.youtube_search_request = Some(request);
                        let save_result =
                            self.store.save_youtube_search(&saved_search, unix_time());
                        self.youtube_results = saved_search.results;
                        if let Err(error) = save_result {
                            self.show_error("Could not save the YouTube search", &error);
                        }
                        if self.view.screen == Screen::Search {
                            self.refresh_youtube_rows();
                            self.request_visible_channel_subscriber_counts();
                            self.view.status_line = if search_limit_reached {
                                format!(
                                    "{} YouTube results loaded; restart-safe limit reached",
                                    self.youtube_results.len()
                                )
                            } else {
                                format!(
                                    "{} YouTube result{} loaded",
                                    self.youtube_results.len(),
                                    if self.youtube_results.len() == 1 {
                                        ""
                                    } else {
                                        "s"
                                    }
                                )
                            };
                            self.request_selected_details();
                        }
                    }
                    Err(error) => {
                        if self.view.screen == Screen::Search {
                            self.show_error_message("YouTube search failed", error);
                        }
                    }
                }
            }
            #[cfg(feature = "youtube-music")]
            ProviderResponse::YouTubeMusicSearch {
                generation,
                query,
                result,
            } => {
                if generation != self.search_generation {
                    return;
                }
                self.finish_search_activity(SearchActivity::YouTubeMusic);
                match result {
                    Ok(tracks) => {
                        self.youtube_music_search_query = query;
                        self.youtube_music_results = tracks
                            .into_iter()
                            .map(search_item_from_youtube_music_track)
                            .collect();
                        let saved_search = SavedYouTubeMusicSearch {
                            query: self.youtube_music_search_query.clone(),
                            results: self.youtube_music_results.clone(),
                        };
                        if let Err(error) = self
                            .store
                            .save_youtube_music_search(&saved_search, unix_time())
                            && self.view.screen == Screen::YouTubeMusic
                        {
                            self.show_error("Could not save the YouTube Music search", &error);
                        }
                        if self.view.screen == Screen::YouTubeMusic {
                            self.refresh_youtube_music_rows();
                            self.view.status_line = format!(
                                "{} YouTube Music track{} loaded",
                                self.youtube_music_results.len(),
                                if self.youtube_music_results.len() == 1 {
                                    ""
                                } else {
                                    "s"
                                }
                            );
                            self.request_selected_details();
                        }
                    }
                    Err(error) => {
                        if self.view.screen == Screen::YouTubeMusic {
                            self.show_error_message("YouTube Music search failed", error);
                        }
                    }
                }
            }
            ProviderResponse::ChannelVideos {
                generation,
                request,
                result,
            } => {
                let background_initial_load = self.view.screen != Screen::Subscriptions
                    && self.pending_subscription_refresh.is_none()
                    && self.active_subscription_channel_id.is_none();
                if generation != self.subscription_generation
                    || (self.active_subscription_channel_id.as_deref()
                        != Some(request.channel_id.as_str())
                        && !background_initial_load)
                {
                    return;
                }
                self.view.subscriptions.loading = false;
                match result {
                    Ok(mut page) => {
                        if page.page != request.page
                            || page.next_page.is_some_and(|next_page| {
                                request.page.checked_add(1) != Some(next_page) || next_page > 10_000
                            })
                            || page
                                .items
                                .iter()
                                .any(|item| !matches!(item, SearchItem::Video(_)))
                        {
                            if self
                                .pending_subscription_refresh
                                .as_ref()
                                .is_some_and(|pending| pending.channel_id == request.channel_id)
                            {
                                self.pending_subscription_refresh = None;
                            }
                            if self.view.screen == Screen::Subscriptions {
                                self.show_error_message(
                                    "Subscription videos failed",
                                    "the provider returned inconsistent channel-video pagination",
                                );
                            }
                            return;
                        }
                        page.items.retain(|item| {
                            !matches!(
                                item,
                                SearchItem::Video(video)
                                    if video.duration_seconds == Some(0) && !video.live
                            )
                        });
                        let received_page_empty = page.items.is_empty();
                        let stage_refresh =
                            self.pending_subscription_refresh
                                .as_ref()
                                .is_some_and(|pending| {
                                    pending.channel_id == request.channel_id
                                        && (pending.staged_cache.is_some()
                                            || (request.page == 1 && received_page_empty))
                                });
                        let persist_snapshot = if stage_refresh {
                            let pending = self
                                .pending_subscription_refresh
                                .as_mut()
                                .expect("a matching staged refresh was checked above");
                            let staged = pending
                                .staged_cache
                                .get_or_insert_with(CachedSubscriptionVideos::default);
                            Self::apply_subscription_video_page(staged, page);
                            let continue_empty_refresh = received_page_empty
                                && u32::from(staged.consecutive_empty_pages)
                                    < MAX_AUTOMATIC_EMPTY_SUBSCRIPTION_PAGES
                                && staged.next_page.is_some();
                            if continue_empty_refresh {
                                let next_page =
                                    staged.next_page.expect("continuation was checked above");
                                self.view.status_line = format!(
                                    "Refreshing {} through page {}…",
                                    self.view.subscriptions.source_title, request.page
                                );
                                self.request_subscription_videos(
                                    request.channel_id.clone(),
                                    next_page,
                                );
                                return;
                            }
                            let staged = pending
                                .staged_cache
                                .take()
                                .expect("the refresh cache was initialized above");
                            self.subscription_video_cache
                                .insert(request.channel_id.clone(), staged);
                            self.touch_subscription_cache(&request.channel_id);
                            self.enforce_subscription_cache_byte_budget(&request.channel_id);
                            true
                        } else {
                            let first_page = request.page == 1;
                            let first_playable_page_after_empty_prefix = request.page > 1
                                && !received_page_empty
                                && self
                                    .subscription_video_cache
                                    .get(&request.channel_id)
                                    .is_some_and(|cached| cached.items.is_empty());
                            self.cache_subscription_video_page(&request.channel_id, page);
                            first_page || first_playable_page_after_empty_prefix
                        };
                        if persist_snapshot
                            && let Err(error) =
                                self.persist_subscription_video_snapshot(&request.channel_id)
                            && self.view.screen == Screen::Subscriptions
                        {
                            self.show_error("Could not save cached subscription videos", &error);
                        }
                        if self.view.screen != Screen::Subscriptions {
                            return;
                        }
                        self.refresh_subscription_video_rows();
                        if self
                            .pending_subscription_refresh
                            .as_ref()
                            .is_some_and(|pending| {
                                pending.channel_id == request.channel_id
                                    && pending.staged_cache.is_none()
                            })
                        {
                            self.restore_subscription_refresh_selection(&request.channel_id);
                        }
                        let count = self.view.subscriptions.items.len();
                        self.view.status_line = format!(
                            "{count} video{} loaded for {}",
                            if count == 1 { "" } else { "s" },
                            self.view.subscriptions.source_title
                        );
                        let next_page = self
                            .subscription_video_cache
                            .get(&request.channel_id)
                            .and_then(|cached| cached.next_page);
                        let consecutive_empty_pages = self
                            .subscription_video_cache
                            .get(&request.channel_id)
                            .map_or(0, |cached| cached.consecutive_empty_pages);
                        if received_page_empty
                            && u32::from(consecutive_empty_pages)
                                < MAX_AUTOMATIC_EMPTY_SUBSCRIPTION_PAGES
                            && let Some(next_page) = next_page
                        {
                            self.request_subscription_videos(request.channel_id.clone(), next_page);
                            return;
                        }
                        if received_page_empty && next_page.is_some() {
                            self.view.status_line = if count == 0 {
                                format!(
                                    "No playable videos through page {}; press Enter to continue",
                                    request.page
                                )
                            } else {
                                format!(
                                    "No additional playable videos through page {}; press j to continue",
                                    request.page
                                )
                            };
                        }
                        if count > 0
                            && (self.view.subscriptions.layout == SubscriptionsLayout::DrillDown
                                || self.view.subscriptions.focus == SubscriptionPane::Items)
                        {
                            self.request_selected_details();
                        }
                    }
                    Err(error) => {
                        if self
                            .pending_subscription_refresh
                            .as_ref()
                            .is_some_and(|pending| pending.channel_id == request.channel_id)
                        {
                            self.pending_subscription_refresh = None;
                        }
                        if self.view.screen == Screen::Subscriptions {
                            self.show_error_message("Subscription videos failed", error);
                        }
                    }
                }
            }
            ProviderResponse::ChannelSubscriberCounts {
                provider_generation,
                requested_ids,
                result,
            } => {
                if provider_generation != self.youtube_provider_generation {
                    return;
                }
                for channel_id in &requested_ids {
                    self.pending_channel_subscribers.remove(channel_id);
                }
                match result {
                    Ok(counts) => {
                        for requested_id in requested_ids {
                            let count = counts
                                .iter()
                                .find(|count| count.channel_id == requested_id)
                                .and_then(|count| count.subscriber_count);
                            self.channel_subscriber_cache.insert(requested_id, count);
                        }
                    }
                    Err(_) => {
                        // Subscriber enrichment is optional. Cache an
                        // unavailable result for this process so a flaky
                        // provider cannot create repeated background traffic.
                        for requested_id in requested_ids {
                            self.channel_subscriber_cache.insert(requested_id, None);
                        }
                    }
                }
                self.refresh_youtube_rows();
                let visible_subscription_channel = if self.view.screen == Screen::Subscriptions {
                    self.selected_subscription_channel_id()
                } else {
                    None
                };
                if let Some(channel_id) = visible_subscription_channel {
                    self.view.subscriptions.source_subscriber_count = self
                        .channel_subscriber_cache
                        .get(&channel_id)
                        .copied()
                        .flatten();
                }
                if self.active_subscription_channel_id.is_some() {
                    self.refresh_subscription_video_rows();
                }
            }
            ProviderResponse::ChannelDetails {
                generation,
                provider_generation,
                channel_id,
                result,
            } => {
                if provider_generation != self.youtube_provider_generation {
                    return;
                }
                self.pending_channel_details.remove(&channel_id);
                match result {
                    Ok(mut profile) => {
                        if profile.summary.channel_id != channel_id {
                            if generation == self.channel_details_generation {
                                self.view.status_line =
                                    "Channel metadata returned a mismatched identifier".to_owned();
                            }
                            self.cache_channel_details(channel_id, None);
                            return;
                        }
                        compact_channel_summary(&mut profile.summary);
                        let channel = profile.summary.clone();
                        let fetched_at = unix_time();
                        let expires_at =
                            fetched_at.saturating_add(CHANNEL_DETAILS_CACHE_TTL_SECONDS);
                        if let Err(error) = self.store.put_cached_channel_summary(
                            &crate::persistence::CachedChannelSummary {
                                summary: channel.clone(),
                                fetched_at,
                                expires_at,
                            },
                        ) {
                            self.show_error("Could not cache channel info", &error);
                        }
                        self.channel_details_fresh_until
                            .insert(channel_id.clone(), expires_at);
                        self.channel_subscriber_cache
                            .insert(channel_id.clone(), channel.subscriber_count);
                        self.cache_channel_details(channel_id.clone(), Some(channel.clone()));
                        self.channel_profile_cache
                            .insert(channel_id.clone(), profile.clone());
                        if generation == self.channel_details_generation
                            && self.visible_channel_id() == Some(channel_id.as_str())
                        {
                            self.apply_channel_profile_to_view(&profile);
                            self.view.status_line =
                                format!("Loaded channel info for {}", channel.name);
                        } else {
                            self.apply_channel_artwork_to_subscription_source(&channel);
                        }
                    }
                    Err(_) => {
                        self.cache_channel_details(channel_id.clone(), None);
                        if generation == self.channel_details_generation
                            && self.visible_channel_id() == Some(channel_id.as_str())
                        {
                            if let Some(details) = self.view.details.as_mut().filter(|details| {
                                details.description == "Loading channel description…"
                            }) {
                                details.description.clear();
                            }
                            self.view.status_line = "Channel info is unavailable".to_owned();
                        }
                    }
                }
            }
            #[cfg(feature = "network")]
            ProviderResponse::ChannelPageMetadata {
                generation,
                provider_generation,
                channel_id,
                result,
            } => {
                if provider_generation != self.youtube_provider_generation {
                    return;
                }
                let Ok(page) = result else {
                    // Channel-page enrichment is optional. API/Invidious
                    // metadata was already delivered in the preceding response.
                    return;
                };
                let Some(mut profile) = self.channel_profile_cache.remove(&channel_id) else {
                    return;
                };
                page.merge_missing_into(&mut profile);
                let summary = profile.summary.clone();
                self.channel_profile_cache
                    .insert(channel_id.clone(), profile.clone());
                self.cache_channel_details(channel_id.clone(), Some(summary.clone()));
                let fetched_at = unix_time();
                let expires_at = self
                    .channel_details_fresh_until
                    .get(&channel_id)
                    .copied()
                    .unwrap_or_else(|| {
                        fetched_at.saturating_add(CHANNEL_DETAILS_CACHE_TTL_SECONDS)
                    });
                if let Err(error) = self.store.put_cached_channel_summary(
                    &crate::persistence::CachedChannelSummary {
                        summary,
                        fetched_at,
                        expires_at,
                    },
                ) {
                    self.show_error("Could not cache enriched channel info", &error);
                }
                if generation == self.channel_details_generation
                    && self.visible_channel_id() == Some(channel_id.as_str())
                {
                    self.apply_channel_profile_to_view(&profile);
                    self.view.status_line =
                        format!("Loaded public channel links for {}", profile.summary.name);
                }
            }
            ProviderResponse::Details { generation, result } => {
                if generation != self.details_generation {
                    return;
                }
                match result {
                    Ok(details) => {
                        let known_orientation = self
                            .youtube_results
                            .iter()
                            .chain(self.youtube_music_results.iter())
                            .chain(
                                self.subscription_video_cache
                                    .values()
                                    .flat_map(|cached| cached.items.iter()),
                            )
                            .find_map(|item| match item {
                                SearchItem::Video(summary)
                                    if summary.video_id == details.video_id
                                        && summary.orientation != VideoOrientation::Unknown =>
                                {
                                    Some(summary.orientation)
                                }
                                _ => None,
                            });
                        let mut updated_summary = summary_from_details(&details);
                        if updated_summary.orientation == VideoOrientation::Unknown
                            && let Some(orientation) = known_orientation
                        {
                            updated_summary.orientation = orientation;
                        }
                        let detailed_chapters =
                            description_chapters(&details.description, details.duration_seconds);
                        let detailed_media_id =
                            MediaId::new(SourceKind::YouTube, details.video_id.clone());
                        let orientation_changed = details.orientation != VideoOrientation::Unknown
                            && self.youtube_results.iter().any(|item| {
                                matches!(
                                    item,
                                    SearchItem::Video(summary)
                                        if summary.video_id == details.video_id
                                            && summary.orientation != details.orientation
                                )
                            });
                        if orientation_changed
                            && self.youtube_search_request.is_some()
                            && let Err(error) = self.store.update_saved_youtube_video_orientation(
                                &details.video_id,
                                details.orientation,
                                unix_time(),
                            )
                        {
                            self.show_error(
                                "Could not cache the YouTube video orientation",
                                &error,
                            );
                        }
                        for queued in &mut self.playback_queue.items {
                            if queued.media.id == detailed_media_id {
                                queued.media.chapters.clone_from(&detailed_chapters);
                            }
                        }
                        if self.current_media.as_ref() == Some(&detailed_media_id) {
                            self.view.playback_chapters.clone_from(&detailed_chapters);
                        }
                        let mut compacted_cache_item = SearchItem::Video(updated_summary.clone());
                        compact_subscription_item(&mut compacted_cache_item);
                        let SearchItem::Video(compacted_cache_summary) = compacted_cache_item
                        else {
                            unreachable!("video details must produce a video summary");
                        };
                        for item in &mut self.youtube_results {
                            if let SearchItem::Video(summary) = item
                                && summary.video_id == details.video_id
                            {
                                *summary = updated_summary.clone();
                            }
                        }
                        for item in &mut self.youtube_music_results {
                            if let SearchItem::Video(summary) = item
                                && summary.video_id == details.video_id
                            {
                                *summary = updated_summary.clone();
                                summary.webpage_url = url::Url::parse(&format!(
                                    "https://music.youtube.com/watch?v={}",
                                    details.video_id
                                ))
                                .ok();
                            }
                        }
                        let mut updated_cached_channels = Vec::new();
                        for (channel_id, cached) in &mut self.subscription_video_cache {
                            let mut channel_updated = false;
                            for item in &mut cached.items {
                                if let SearchItem::Video(summary) = item
                                    && summary.video_id == details.video_id
                                {
                                    *summary = compacted_cache_summary.clone();
                                    channel_updated = true;
                                }
                            }
                            if channel_updated {
                                updated_cached_channels.push(channel_id.clone());
                            }
                        }
                        let preserved_channel = self
                            .active_subscription_channel_id
                            .clone()
                            .filter(|channel_id| updated_cached_channels.contains(channel_id))
                            .or_else(|| updated_cached_channels.first().cloned());
                        if let Some(channel_id) = preserved_channel {
                            self.touch_subscription_cache(&channel_id);
                            self.enforce_subscription_cache_byte_budget(&channel_id);
                        }
                        if self.view.screen == Screen::Search {
                            self.refresh_youtube_rows();
                        } else if self.view.screen == Screen::YouTubeMusic {
                            self.refresh_youtube_music_rows();
                        } else if self.view.screen == Screen::Subscriptions {
                            self.refresh_subscription_video_rows();
                        }
                        let selected_matches = self.selected_youtube_item().is_some_and(|item| {
                            matches!(
                                item,
                                SearchItem::Video(video) if video.video_id == details.video_id
                            )
                        });
                        let linked_matches = self
                            .active_description_video
                            .as_ref()
                            .is_some_and(|linked| linked.video_id == details.video_id);
                        if (selected_matches || linked_matches)
                            && self.view.right_panel_mode != RightPanelMode::Channel
                        {
                            let wikidata = self
                                .view
                                .details
                                .as_ref()
                                .map(|view| view.wikidata.clone())
                                .unwrap_or_else(|| "not loaded".to_owned());
                            let links = self
                                .view
                                .details
                                .as_ref()
                                .map(|view| view.links.clone())
                                .unwrap_or_default();
                            let mut detail = detail_from_video(&details, &self.subscription_tree);
                            if !linked_matches && self.view.screen == Screen::YouTubeMusic {
                                detail.source = "YouTube Music".to_owned();
                            }
                            detail.wikidata = wikidata;
                            detail.links = links;
                            self.view.details = Some(detail);
                            if linked_matches {
                                self.view.status_line = self
                                    .active_description_video
                                    .as_ref()
                                    .and_then(|linked| linked.start_seconds)
                                    .map_or_else(
                                        || format!("Opened {} in Details", details.title),
                                        |seconds| {
                                            format!(
                                                "Opened {} in Details at {}",
                                                details.title,
                                                format_seconds(seconds)
                                            )
                                        },
                                    );
                            }
                        }
                        #[cfg(feature = "yt-dlp")]
                        {
                            let selected = self.selected_youtube_item().cloned();
                            self.update_youtube_prewarm_for_selection(selected.as_ref());
                        }
                    }
                    Err(error) => {
                        self.show_error_message("Video details failed", error);
                    }
                }
            }
            ProviderResponse::Apple { generation, result } => {
                if generation != self.search_generation {
                    return;
                }
                match result {
                    Ok(media) => {
                        apply_resolved_direct_view(&mut self.view, &media);
                        self.resolved_direct = Some(media);
                    }
                    Err(error) => {
                        self.show_error_message("Apple Podcasts link failed", error);
                    }
                }
            }
            ProviderResponse::FirstClass {
                generation,
                source,
                result,
            } => {
                if generation != self.search_generation {
                    return;
                }
                match result {
                    Ok(media) => {
                        apply_resolved_direct_view(&mut self.view, &media);
                        self.resolved_direct = Some(media);
                    }
                    Err(error) => {
                        self.show_error_message(
                            &format!("{} link failed", direct_source_label(&source)),
                            error,
                        );
                    }
                }
            }
            ProviderResponse::TrackerSource {
                generation,
                source,
                result,
            } => {
                if generation != self.search_generation {
                    return;
                }
                match result {
                    Ok(mut items) => {
                        self.tracker_results.append(&mut items);
                        if self.view.screen == Screen::TrackerMusic {
                            self.refresh_tracker_rows();
                            self.view.status_line = format!(
                                "Loaded {} result(s) through {source}",
                                self.view.rows.len()
                            );
                        }
                    }
                    Err(error) => {
                        if self.view.screen == Screen::TrackerMusic {
                            self.show_error_message(&format!("{source} search failed"), error);
                        }
                    }
                }
            }
            ProviderResponse::TrackerComplete { generation } => {
                if generation != self.search_generation {
                    return;
                }
                self.finish_search_activity(SearchActivity::TrackerArchives);
                if self.view.screen == Screen::TrackerMusic {
                    self.view.status_line = format!(
                        "{} MOD/tracker result(s) loaded from enabled archives",
                        self.tracker_results.len()
                    );
                }
            }
            #[cfg(feature = "tracker-music")]
            ProviderResponse::TrackerPrepared {
                generation,
                item_key,
                result,
            } => {
                let owns_response =
                    self.pending_tracker_preparation
                        .as_ref()
                        .is_some_and(|pending| {
                            pending.generation == generation && pending.item_key == item_key
                        });
                if !owns_response {
                    return;
                }
                let pending = self
                    .pending_tracker_preparation
                    .take()
                    .expect("matching tracker preparation was checked above");
                self.clear_playback_start_activity();
                if generation != self.search_generation {
                    return;
                }
                match result {
                    Ok(modules) => {
                        self.finish_tracker_preparation(&item_key, modules, pending.owner);
                    }
                    Err(error) => match pending.owner {
                        TrackerPreparationOwner::Autoplay(origin)
                            if self.config.playback.autoplay =>
                        {
                            self.view.status_line =
                                format!("Autoplay skipped an unavailable tracker item: {error}");
                            self.continue_autoplay(origin);
                        }
                        TrackerPreparationOwner::Autoplay(_)
                        | TrackerPreparationOwner::CanceledAutoplay => {
                            self.view.status_line =
                                "Canceled tracker preparation did not change playback".to_owned();
                        }
                        TrackerPreparationOwner::Manual => {
                            self.show_error_message("Tracker module preparation failed", error);
                        }
                    },
                }
            }
            ProviderResponse::LocalScan {
                generation,
                root,
                result,
            } => {
                if generation != self.search_generation {
                    return;
                }
                match result {
                    Ok(paths) => {
                        self.local_results = paths;
                        self.refresh_local_rows();
                        self.view.status_line = format!(
                            "{} playable file(s) found below {}",
                            self.local_results.len(),
                            root.display()
                        );
                    }
                    Err(error) => {
                        self.view.rows.clear();
                        self.show_error_message("Local folder scan failed", error);
                    }
                }
            }
            #[cfg(all(feature = "local", feature = "thumbnails"))]
            ProviderResponse::LocalArtwork {
                generation,
                path,
                result,
            } => {
                if self.pending_local_artwork.as_ref() != Some(&(generation, path.clone())) {
                    return;
                }
                self.pending_local_artwork = None;
                self.view.local_artwork_pending = false;
                self.cache_local_artwork(path.clone(), result.unwrap_or(None));
                self.apply_cached_local_artwork(&path);
                self.request_selected_local_artwork();
            }
            ProviderResponse::LocalFolderSize {
                generation,
                path,
                result,
            } => {
                if generation
                    != self
                        .local_folder_size_generation
                        .load(AtomicOrdering::Relaxed)
                    || self.local_folder_size_pending.as_deref() != Some(path.as_path())
                {
                    return;
                }
                self.local_folder_size_pending = None;
                let selected_path = self.selected_local_path();
                let still_visible = self.local_listing.as_ref().is_some_and(|listing| {
                    listing
                        .entries
                        .iter()
                        .any(|entry| entry.is_directory() && entry.path == path)
                });
                if self.config.ui.show_local_folder_sizes && still_visible {
                    match result {
                        Ok(worker_result) => {
                            let reused = worker_result.reused;
                            if self.store_local_folder_size(&path, generation, worker_result) {
                                self.sort_local_listing();
                                self.select_local_path(selected_path.as_deref());
                                self.refresh_local_browser_rows_without_detail();
                            } else if reused {
                                self.local_folder_size_queue.push_front(path);
                            }
                        }
                        Err(_) => self.store_local_folder_size_failure(path),
                    }
                }
                self.request_next_local_folder_size();
            }
            #[cfg(feature = "wikidata")]
            ProviderResponse::Wikidata {
                generation,
                property_id,
                external_id,
                result,
            } => {
                if generation != self.wikidata_generation {
                    return;
                }
                match result {
                    Ok(items) => {
                        let now = unix_time();
                        let expires_at = now.saturating_add(if items.is_empty() {
                            24 * 60 * 60
                        } else {
                            7 * 24 * 60 * 60
                        });
                        let cached = CachedWikidataLookup {
                            property_id,
                            external_id,
                            items,
                            fetched_at: now,
                            expires_at,
                        };
                        if let Err(error) = self.store.put_cached_wikidata(&cached) {
                            self.show_error("Wikidata cache write failed", &error);
                        }
                        if let Some(details) = self.view.details.as_mut() {
                            apply_wikidata_links(details, &cached.items);
                        }
                        self.view.selected_detail_link = (!cached.items.is_empty()).then_some(0);
                    }
                    Err(error) => {
                        if let Some(details) = self.view.details.as_mut() {
                            // Wikidata enrichment is optional and lazy. A remote
                            // timeout must not replace active media controls
                            // with a diagnostic popup.
                            details.wikidata.clear();
                        }
                        self.view.status_line = if error.to_ascii_lowercase().contains("timeout") {
                            "Wikidata lookup timed out; playback remains available".to_owned()
                        } else {
                            "Wikidata lookup unavailable; playback remains available".to_owned()
                        };
                    }
                }
            }
            #[cfg(feature = "wikidata")]
            ProviderResponse::WikidataStatements { item_id, result } => {
                self.pending_wikidata_entities.remove(&item_id);
                match result {
                    Ok(entity) => {
                        self.cache_wikidata_entity(entity.clone());
                        if let Some(details) = self.view.details.as_mut()
                            && details.expanded_wikidata_item.as_deref() == Some(item_id.as_str())
                        {
                            apply_wikidata_entity_to_details(details, &entity);
                            self.view.status_line =
                                format!("Expanded Wikidata properties for {item_id}");
                        }
                    }
                    Err(error) => {
                        if let Some(details) = self.view.details.as_mut()
                            && details.loading_wikidata_item.as_deref() == Some(item_id.as_str())
                        {
                            details.loading_wikidata_item = None;
                        }
                        self.view.status_line = if error.to_ascii_lowercase().contains("timeout") {
                            "Wikidata property lookup timed out".to_owned()
                        } else {
                            "Wikidata properties are unavailable".to_owned()
                        };
                    }
                }
            }
        }
    }

    fn refresh_youtube_rows(&mut self) {
        let today = Local::now().date_naive();
        self.view.rows = self
            .youtube_results
            .iter()
            .map(|item| {
                row_from_search_item(
                    item,
                    &self.store,
                    &self.subscription_tree,
                    &self.channel_subscriber_cache,
                    SearchRowContext::GlobalSearch,
                    today,
                )
            })
            .collect();
        self.view.selected = self
            .view
            .selected
            .min(self.view.rows.len().saturating_sub(1));
    }

    fn refresh_youtube_music_rows(&mut self) {
        let today = Local::now().date_naive();
        self.view.rows = self
            .youtube_music_results
            .iter()
            .map(|item| {
                row_from_search_item(
                    item,
                    &self.store,
                    &self.subscription_tree,
                    &self.channel_subscriber_cache,
                    SearchRowContext::YouTubeMusic,
                    today,
                )
            })
            .collect();
        self.view.selected = self
            .view
            .selected
            .min(self.view.rows.len().saturating_sub(1));
    }

    fn refresh_local_rows(&mut self) {
        self.view.rows = self
            .local_results
            .iter()
            .map(|item| RowView {
                media_id: Some(MediaId::new(
                    SourceKind::Local,
                    item.path.display().to_string(),
                )),
                title: item.title.clone(),
                subtitle: local_media_subtitle(item),
                source: "Local".to_owned(),
                ..RowView::default()
            })
            .collect();
        self.view.selected = self
            .view
            .selected
            .min(self.view.rows.len().saturating_sub(1));
        self.update_non_youtube_detail();
    }

    /// Starts one lazy artwork lookup for selected Local media or a folder.
    #[cfg(all(feature = "local", feature = "thumbnails"))]
    fn request_selected_local_artwork(&mut self) {
        let selected = if self.view.screen == Screen::Local {
            self.local_entry_index()
                .and_then(|index| self.local_listing.as_ref()?.entries.get(index))
                .and_then(|entry| {
                    if entry.is_directory() {
                        Some((entry.path.clone(), LocalArtworkKind::FolderCover))
                    } else if entry.kind.is_playable() {
                        Some((entry.path.clone(), LocalArtworkKind::EmbeddedMedia))
                    } else {
                        None
                    }
                })
        } else {
            self.view
                .details
                .as_ref()
                .and_then(|details| details.media_id.as_ref())
                .filter(|media_id| media_id.source == SourceKind::Local)
                .map(|media_id| {
                    (
                        PathBuf::from(&media_id.external_id),
                        LocalArtworkKind::EmbeddedMedia,
                    )
                })
        };
        let Some((path, kind)) = selected else {
            if self.pending_local_artwork.is_none() {
                self.view.local_artwork_pending = false;
            }
            return;
        };
        if self.local_artwork_cache.contains_key(&path) {
            self.apply_cached_local_artwork(&path);
            if self.pending_local_artwork.is_none() {
                self.view.local_artwork_pending = false;
            }
            return;
        }
        if self.pending_local_artwork.is_some() {
            self.view.local_artwork_pending = true;
            return;
        }
        let Some(sender) = self.provider_requests.as_ref() else {
            self.view.local_artwork_pending = false;
            return;
        };
        self.local_artwork_generation = self.local_artwork_generation.wrapping_add(1);
        let generation = self.local_artwork_generation;
        let request = ProviderRequest::LocalArtwork {
            generation,
            path: path.clone(),
            cache_directory: self.config.thumbnail_cache_dir(),
            kind,
        };
        if sender.send(request).is_ok() {
            self.pending_local_artwork = Some((generation, path));
            self.view.local_artwork_pending = true;
        } else {
            self.view.local_artwork_pending = false;
        }
    }

    /// Stores one process-local artwork result with deterministic FIFO eviction.
    #[cfg(all(feature = "local", feature = "thumbnails"))]
    fn cache_local_artwork(&mut self, path: PathBuf, artwork: Option<url::Url>) {
        self.local_artwork_cache.insert(path.clone(), artwork);
        self.local_artwork_cache_order
            .retain(|cached_path| cached_path != &path);
        self.local_artwork_cache_order.push_back(path);
        while self.local_artwork_cache.len() > MAX_CACHED_LOCAL_ARTWORK {
            let Some(oldest) = self.local_artwork_cache_order.pop_front() else {
                break;
            };
            self.local_artwork_cache.remove(&oldest);
        }
    }

    /// Applies a cached cover only when the same Local path still owns Details.
    #[cfg(all(feature = "local", feature = "thumbnails"))]
    fn apply_cached_local_artwork(&mut self, path: &Path) {
        let Some(artwork) = self.local_artwork_cache.get(path) else {
            return;
        };
        let selected_local_path_matches = self.selected_local_path().as_deref() == Some(path);
        let Some(details) = self.view.details.as_mut() else {
            return;
        };
        let selected_matches = selected_local_path_matches
            || details.media_id.as_ref().is_some_and(|media_id| {
                media_id.source == SourceKind::Local && Path::new(&media_id.external_id) == path
            });
        if selected_matches {
            details.thumbnail_url.clone_from(artwork);
        }
    }

    fn browse_local_directory(&mut self, directory: PathBuf) {
        self.browse_local_directory_with_reselection(directory, None);
    }

    /// Applies one isolated Local listing only when its route is still current.
    fn handle_local_browse_response(&mut self, response: LocalBrowseResponse) {
        if response.generation != self.local_generation {
            return;
        }
        self.view.local_browse_pending = false;
        match response.result {
            Ok(listing) => {
                let truncated = listing.truncated;
                let path = listing.path.display().to_string();
                let reselected_path = self
                    .pending_local_reselection
                    .take()
                    .filter(|(pending_generation, _)| *pending_generation == response.generation)
                    .map(|(_, child)| child);
                self.local_listing = Some(listing);
                self.sort_local_listing();
                self.view.selected = 0;
                self.select_local_path(reselected_path.as_deref());
                self.hydrate_local_progress_cache();
                self.refresh_local_browser_rows();
                self.schedule_local_folder_sizes();
                self.view.status_line = if truncated {
                    format!("Showing a bounded partial listing of {path}")
                } else {
                    format!("Local folder: {path}")
                };
            }
            Err(error) => {
                self.pending_local_reselection = None;
                self.view.rows.clear();
                self.view.selected = 0;
                self.show_error_message("Local folder could not be opened", error);
            }
        }
    }

    /// Starts one directory load and optionally remembers a direct child that
    /// should be selected when the response arrives.
    fn browse_local_directory_with_reselection(
        &mut self,
        directory: PathBuf,
        reselect_child: Option<PathBuf>,
    ) {
        let keep_noninteractive_rows = self.view.screen == Screen::Local
            && self.local_listing.is_some()
            && !self.view.rows.is_empty();
        self.local_generation = self.local_generation.wrapping_add(1);
        self.invalidate_local_folder_sizes();
        self.pending_local_reselection = reselect_child
            .clone()
            .map(|child| (self.local_generation, child));
        self.local_listing = None;
        if !keep_noninteractive_rows {
            self.view.rows.clear();
            self.view.selected = 0;
        }
        self.view.details = None;
        self.view.local_path = directory.display().to_string();
        let status_line = format!("Reading {}…", directory.display());
        self.view.local_browse_pending = false;
        if self.send_local_browse_request(
            LocalBrowseRequest::Browse {
                generation: self.local_generation,
                directory,
                preferred_child: reselect_child,
            },
            "Could not open the local folder",
        ) {
            self.view.local_browse_pending = true;
            self.view.status_line = status_line;
        } else {
            self.pending_local_reselection = None;
        }
    }

    /// Cancels older recursive work without discarding bounded RAM cache data.
    fn invalidate_local_folder_sizes(&mut self) {
        self.local_folder_size_generation
            .fetch_add(1, AtomicOrdering::Relaxed);
        self.local_folder_size_queue.clear();
        self.local_folder_size_pending = None;
    }

    /// Queues one rotating, fixed-size batch of current child directories.
    fn schedule_local_folder_sizes(&mut self) {
        if self.view.screen != Screen::Local || !self.config.ui.show_local_folder_sizes {
            return;
        }
        if self.local_folder_size_pending.is_some() || !self.local_folder_size_queue.is_empty() {
            return;
        }
        let Some(listing) = self.local_listing.as_ref() else {
            return;
        };
        let listing_path = listing.path.clone();
        let directories = listing
            .entries
            .iter()
            .filter(|entry| entry.is_directory())
            .map(|entry| (entry.path.clone(), entry.directory_identity.clone()))
            .collect::<Vec<_>>();
        if directories.is_empty() {
            return;
        }
        let generation = self
            .local_folder_size_generation
            .load(AtomicOrdering::Relaxed);
        let now = Instant::now();
        let mut reused_complete_size = false;

        for (path, identity) in &directories {
            let complete_is_fresh = self
                .local_folder_size_cache
                .get(path)
                .is_some_and(|cached| {
                    identity.as_ref() == Some(&cached.identity)
                        && now.saturating_duration_since(cached.measured_at)
                            <= LOCAL_FOLDER_SIZE_CACHE_TTL
                });
            if complete_is_fresh {
                if let Some(cached) = self.local_folder_size_cache.get_mut(path) {
                    cached.validated_generation = generation;
                }
                reused_complete_size = true;
                self.local_folder_size_cache_order
                    .retain(|cached_path| cached_path != path);
                self.local_folder_size_cache_order.push_back(path.clone());
            } else if self.local_folder_size_cache.remove(path).is_some() {
                self.local_folder_size_cache_order
                    .retain(|cached_path| cached_path != path);
            }

            let failure_is_fresh =
                self.local_folder_size_failures
                    .get(path)
                    .is_some_and(|failure| {
                        failure.identity.as_ref() == identity.as_ref()
                            && now.saturating_duration_since(failure.failed_at)
                                <= LOCAL_FOLDER_SIZE_FAILURE_TTL
                    });
            if !failure_is_fresh && self.local_folder_size_failures.remove(path).is_some() {
                self.local_folder_size_failure_order
                    .retain(|failed_path| failed_path != path);
            }
        }

        let start = self
            .local_folder_size_batch_cursors
            .get(&listing_path)
            .copied()
            .unwrap_or_default()
            % directories.len();
        let mut next_cursor = start;
        for offset in 0..directories.len() {
            let index = start.saturating_add(offset) % directories.len();
            next_cursor = index.saturating_add(1) % directories.len();
            let (path, _) = &directories[index];
            let complete = self
                .local_folder_size_cache
                .get(path)
                .is_some_and(|cached| cached.validated_generation == generation);
            if complete || self.local_folder_size_failures.contains_key(path) {
                continue;
            }
            self.local_folder_size_queue.push_back(path.clone());
            if self.local_folder_size_queue.len() >= MAX_LAZY_LOCAL_FOLDER_SIZES {
                break;
            }
        }
        self.local_folder_size_batch_cursors
            .insert(listing_path.clone(), next_cursor);
        self.local_folder_size_batch_cursor_order
            .retain(|path| path != &listing_path);
        self.local_folder_size_batch_cursor_order
            .push_back(listing_path);
        while self.local_folder_size_batch_cursors.len() > MAX_LOCAL_FOLDER_SIZE_BATCH_CURSORS {
            let Some(oldest) = self.local_folder_size_batch_cursor_order.pop_front() else {
                break;
            };
            self.local_folder_size_batch_cursors.remove(&oldest);
        }
        if reused_complete_size {
            let selected_path = self.selected_local_path();
            self.sort_local_listing();
            self.select_local_path(selected_path.as_deref());
            self.refresh_local_browser_rows_without_detail();
        }
        self.request_next_local_folder_size();
    }

    /// Dispatches at most one lazy folder-size request at a time.
    fn request_next_local_folder_size(&mut self) {
        if !self.config.ui.show_local_folder_sizes || self.local_folder_size_pending.is_some() {
            return;
        }
        let Some(path) = self.local_folder_size_queue.pop_front() else {
            return;
        };
        let generation = self
            .local_folder_size_generation
            .load(AtomicOrdering::Relaxed);
        let now = Instant::now();
        let cached = self
            .local_folder_size_cache
            .get(&path)
            .filter(|cached| {
                now.saturating_duration_since(cached.measured_at) <= LOCAL_FOLDER_SIZE_CACHE_TTL
            })
            .map(|cached| crate::local_browser::LocalFolderSizeMeasurement {
                bytes: cached.bytes,
                identity: cached.identity.clone(),
            });
        if self.send_provider_request(
            ProviderRequest::LocalFolderSize {
                generation,
                path: path.clone(),
                cancellation: Arc::clone(&self.local_folder_size_generation),
                cached,
            },
            "Could not measure the local folder",
        ) {
            self.local_folder_size_pending = Some(path);
        }
    }

    /// Returns a complete current-generation size for one Local entry.
    fn known_local_entry_size(&self, entry: &crate::local_browser::LocalEntry) -> Option<u64> {
        if !entry.is_directory() {
            return entry.size_bytes;
        }
        if !self.config.ui.show_local_folder_sizes {
            return None;
        }
        let generation = self
            .local_folder_size_generation
            .load(AtomicOrdering::Relaxed);
        let now = Instant::now();
        self.local_folder_size_cache
            .get(&entry.path)
            .filter(|cached| {
                cached.validated_generation == generation
                    && now.saturating_duration_since(cached.measured_at)
                        <= LOCAL_FOLDER_SIZE_CACHE_TTL
            })
            .map(|cached| cached.bytes)
    }

    /// Stores one worker-validated size and enforces the global RAM bound.
    fn store_local_folder_size(
        &mut self,
        path: &Path,
        generation: u64,
        result: LocalFolderSizeWorkerResult,
    ) -> bool {
        let now = Instant::now();
        if result.reused {
            let Some(cached) = self.local_folder_size_cache.get_mut(path) else {
                return false;
            };
            if cached.identity != result.measurement.identity
                || now.saturating_duration_since(cached.measured_at) > LOCAL_FOLDER_SIZE_CACHE_TTL
            {
                self.local_folder_size_cache.remove(path);
                self.local_folder_size_cache_order
                    .retain(|cached_path| cached_path != path);
                return false;
            }
            cached.bytes = result.measurement.bytes;
            cached.validated_generation = generation;
        } else {
            self.local_folder_size_cache.insert(
                path.to_owned(),
                CachedLocalFolderSize {
                    identity: result.measurement.identity,
                    bytes: result.measurement.bytes,
                    measured_at: now,
                    validated_generation: generation,
                },
            );
        }

        self.local_folder_size_cache_order
            .retain(|cached_path| cached_path != path);
        self.local_folder_size_cache_order
            .push_back(path.to_owned());
        self.local_folder_size_failures.remove(path);
        self.local_folder_size_failure_order
            .retain(|failed_path| failed_path != path);
        while self.local_folder_size_cache.len() > MAX_CACHED_LOCAL_FOLDER_SIZES {
            let Some(oldest) = self.local_folder_size_cache_order.pop_front() else {
                break;
            };
            self.local_folder_size_cache.remove(&oldest);
        }
        true
    }

    /// Caches one bounded failure so navigation does not hammer the same tree.
    fn store_local_folder_size_failure(&mut self, path: PathBuf) {
        let identity = self.local_listing.as_ref().and_then(|listing| {
            listing
                .entries
                .iter()
                .find(|entry| entry.path == path)
                .and_then(|entry| entry.directory_identity.clone())
        });
        self.local_folder_size_failures.insert(
            path.clone(),
            CachedLocalFolderSizeFailure {
                identity,
                failed_at: Instant::now(),
            },
        );
        self.local_folder_size_failure_order
            .retain(|failed_path| failed_path != &path);
        self.local_folder_size_failure_order.push_back(path);
        while self.local_folder_size_failures.len() > MAX_CACHED_LOCAL_FOLDER_SIZES {
            let Some(oldest) = self.local_folder_size_failure_order.pop_front() else {
                break;
            };
            self.local_folder_size_failures.remove(&oldest);
        }
    }

    /// Returns the path represented by the selected Local row.
    fn selected_local_path(&self) -> Option<PathBuf> {
        let listing = self.local_listing.as_ref()?;
        if listing.parent.is_some() && self.view.selected == 0 {
            return listing.parent.clone();
        }
        self.local_entry_index()
            .and_then(|index| listing.entries.get(index))
            .map(|entry| entry.path.clone())
    }

    /// Restores one exact Local path after lazy size sorting reorders rows.
    fn select_local_path(&mut self, selected_path: Option<&Path>) {
        let Some(selected_path) = selected_path else {
            return;
        };
        let Some(listing) = self.local_listing.as_ref() else {
            return;
        };
        if listing
            .parent
            .as_deref()
            .is_some_and(|parent| parent == selected_path)
        {
            self.view.selected = 0;
            return;
        }
        if let Some(index) = listing
            .entries
            .iter()
            .position(|entry| entry.path == selected_path)
        {
            self.view.selected = index.saturating_add(usize::from(listing.parent.is_some()));
        }
    }

    /// Applies the selected deterministic Local ordering to the listing.
    fn sort_local_listing(&mut self) {
        let sizes_enabled = self.config.ui.show_local_folder_sizes;
        let mode = if sizes_enabled {
            self.view.local_size_sort
        } else {
            LocalSizeSort::Off
        };
        let generation = self
            .local_folder_size_generation
            .load(AtomicOrdering::Relaxed);
        let now = Instant::now();
        let cache = &self.local_folder_size_cache;
        let Some(listing) = self.local_listing.as_mut() else {
            return;
        };
        listing.entries.sort_by(|left, right| {
            if mode == LocalSizeSort::Off {
                return right
                    .is_directory()
                    .cmp(&left.is_directory())
                    .then_with(|| left.name.cmp(&right.name));
            }
            let known_size = |entry: &crate::local_browser::LocalEntry| {
                if entry.is_directory() {
                    sizes_enabled
                        .then(|| cache.get(&entry.path))
                        .flatten()
                        .filter(|cached| {
                            cached.validated_generation == generation
                                && now.saturating_duration_since(cached.measured_at)
                                    <= LOCAL_FOLDER_SIZE_CACHE_TTL
                        })
                        .map(|cached| cached.bytes)
                } else {
                    entry.size_bytes
                }
            };
            match (known_size(left), known_size(right)) {
                (Some(left_size), Some(right_size)) => {
                    let size_order = match mode {
                        LocalSizeSort::Ascending => left_size.cmp(&right_size),
                        LocalSizeSort::Descending => right_size.cmp(&left_size),
                        LocalSizeSort::Off => std::cmp::Ordering::Equal,
                    };
                    size_order.then_with(|| left.name.cmp(&right.name))
                }
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => left.name.cmp(&right.name),
            }
        });
    }

    /// Rebuilds visible Local rows and refreshes the selected entry's metadata.
    fn refresh_local_browser_rows(&mut self) {
        self.rebuild_local_browser_rows();
        self.update_local_browser_detail();
    }

    /// Rebuilds Local rows while retaining details for the same selected path.
    ///
    /// Lazy folder-size responses may arrive hundreds of times for one
    /// listing. They update row subtitles and ordering but must not repeatedly
    /// reopen a selected media file to parse its tags.
    fn refresh_local_browser_rows_without_detail(&mut self) {
        self.rebuild_local_browser_rows();
    }

    /// Hydrates watched percentages once for the accepted Local listing.
    fn hydrate_local_progress_cache(&mut self) {
        self.local_progress_cache.clear();
        let Some(listing) = self.local_listing.as_ref() else {
            return;
        };
        let progress_ids = listing
            .entries
            .iter()
            .filter(|entry| entry.kind.is_playable())
            .map(|entry| MediaId::new(SourceKind::Local, entry.path.display().to_string()))
            .collect::<Vec<_>>();
        self.local_progress_cache
            .extend(progress_ids.iter().cloned().map(|media_id| (media_id, 0)));
        let Ok(progress) = self.store.progress_for_media_ids(&progress_ids) else {
            return;
        };
        self.local_progress_cache.extend(
            progress
                .into_iter()
                .map(|(media_id, progress)| (media_id, progress.watched_percent())),
        );
    }

    /// Updates one visible Local marker without rebuilding the directory or
    /// rereading media metadata.
    fn update_local_progress_view(&mut self, media_id: &MediaId, watched_percent: u8) {
        if media_id.source != SourceKind::Local {
            return;
        }
        let visible = self
            .view
            .rows
            .iter()
            .any(|row| row.media_id.as_ref() == Some(media_id));
        if self.local_progress_cache.contains_key(media_id) || visible {
            self.local_progress_cache
                .insert(media_id.clone(), watched_percent);
        }
        for row in &mut self.view.rows {
            if row.media_id.as_ref() == Some(media_id) {
                row.watched_percent = watched_percent;
            }
        }
    }

    /// Mirrors an actively playing backend position into the current Local row.
    ///
    /// A backend may temporarily omit the duration while replacing or opening
    /// media. Such a status cannot produce a trustworthy percentage and must
    /// not erase a previously hydrated marker.
    fn update_live_local_progress_view(&mut self) {
        let Some(media_id) = self
            .current_media
            .as_ref()
            .filter(|media_id| media_id.source == SourceKind::Local)
            .cloned()
        else {
            return;
        };
        let Some(duration) = self
            .view
            .playback
            .duration
            .map(|value| value.as_secs())
            .filter(|duration| *duration > 0)
        else {
            return;
        };
        let now = unix_time();
        let mut progress = PlaybackProgress::new(media_id.clone(), Some(duration), now);
        progress.record_position(self.view.playback.position.as_secs(), now);
        self.update_local_progress_view(&media_id, progress.watched_percent());
    }

    /// Rebuilds the Local row model from listing-scoped RAM state.
    fn rebuild_local_browser_rows(&mut self) {
        use crate::local_browser::LocalEntryKind;

        let Some(listing) = self.local_listing.as_ref() else {
            self.view.rows.clear();
            return;
        };
        self.view.local_path = listing.path.display().to_string();
        let mut rows = Vec::with_capacity(
            listing
                .entries
                .len()
                .saturating_add(usize::from(listing.parent.is_some())),
        );
        if listing.parent.is_some() {
            rows.push(RowView {
                title: "..".to_owned(),
                subtitle: String::new(),
                source: "Local folder".to_owned(),
                compact: true,
                ..RowView::default()
            });
        }
        rows.extend(listing.entries.iter().map(|entry| {
            let source = match entry.kind {
                LocalEntryKind::Directory => "Local folder",
                LocalEntryKind::Audio => "Local audio",
                LocalEntryKind::Video => "Local video (audio playback)",
                LocalEntryKind::TrackerModule => "Local tracker module",
                LocalEntryKind::Image => "Local image",
            };
            let media_id = entry
                .kind
                .is_playable()
                .then(|| MediaId::new(SourceKind::Local, entry.path.display().to_string()));
            let watched_percent = media_id
                .as_ref()
                .and_then(|id| self.local_progress_cache.get(id))
                .copied()
                .unwrap_or_default();
            let subtitle = match entry.kind {
                LocalEntryKind::Directory => self
                    .known_local_entry_size(entry)
                    .map_or_else(String::new, human_bytes),
                LocalEntryKind::Image => entry.image_dimensions.map_or_else(
                    || entry.size_bytes.map_or_else(String::new, human_bytes),
                    |dimensions| {
                        format!(
                            "{}×{} · {}",
                            dimensions.width,
                            dimensions.height,
                            human_bytes(entry.size_bytes.unwrap_or_default())
                        )
                    },
                ),
                _ => entry.size_bytes.map_or_else(String::new, human_bytes),
            };
            RowView {
                media_id,
                title: entry.display_name().into_owned(),
                subtitle,
                source: source.to_owned(),
                watched_percent,
                thumbnail_url: (entry.kind == LocalEntryKind::Image)
                    .then(|| url::Url::from_file_path(&entry.path).ok())
                    .flatten(),
                compact: true,
                ..RowView::default()
            }
        }));
        self.view.rows = rows;
        self.view.selected = self
            .view
            .selected
            .min(self.view.rows.len().saturating_sub(1));
    }

    fn local_entry_index(&self) -> Option<usize> {
        let listing = self.local_listing.as_ref()?;
        self.view
            .selected
            .checked_sub(usize::from(listing.parent.is_some()))
            .filter(|index| *index < listing.entries.len())
    }

    fn update_local_browser_detail(&mut self) {
        use crate::local_browser::LocalEntryKind;

        let Some(listing) = self.local_listing.as_ref() else {
            self.view.details = None;
            return;
        };
        if self.view.selected == 0
            && let Some(parent) = listing.parent.as_ref()
        {
            self.view.details = Some(DetailView {
                title: "..".to_owned(),
                source: "Local folder".to_owned(),
                description: format!("Full path: {}", parent.display()),
                ..DetailView::default()
            });
            return;
        }
        let Some(index) = self.local_entry_index() else {
            self.view.details = None;
            return;
        };
        let entry = &listing.entries[index];
        if entry.kind.is_playable() {
            let item = local_media_item(entry.path.clone());
            self.view.details = Some(DetailView {
                media_id: Some(MediaId::new(
                    SourceKind::Local,
                    entry.path.display().to_string(),
                )),
                title: item.title.clone(),
                source: match entry.kind {
                    LocalEntryKind::Video => "Local video (audio playback)",
                    LocalEntryKind::TrackerModule => "Local tracker module",
                    _ => "Local audio",
                }
                .to_owned(),
                length: item
                    .duration_seconds
                    .map_or_else(|| "unknown".to_owned(), format_seconds),
                description: local_media_description(&item),
                local_renamable: true,
                local_trashable: true,
                ..DetailView::default()
            });
        } else {
            let is_directory = entry.kind == LocalEntryKind::Directory;
            let known_size = (!is_directory)
                .then(|| self.known_local_entry_size(entry))
                .flatten();
            self.view.details = Some(DetailView {
                title: entry.display_name().into_owned(),
                source: if entry.kind == LocalEntryKind::Image {
                    "Local image"
                } else {
                    "Local folder"
                }
                .to_owned(),
                description: format!(
                    "Full path: {}{}{}",
                    entry.path.display(),
                    known_size
                        .map_or_else(String::new, |size| format!("\nSize: {}", human_bytes(size))),
                    entry
                        .image_dimensions
                        .map_or_else(String::new, |dimensions| {
                            format!("\nDimensions: {}×{}", dimensions.width, dimensions.height)
                        })
                ),
                thumbnail_url: (entry.kind == LocalEntryKind::Image)
                    .then(|| url::Url::from_file_path(&entry.path).ok())
                    .flatten(),
                local_renamable: !is_directory,
                local_trashable: true,
                ..DetailView::default()
            });
        }
        #[cfg(all(feature = "local", feature = "thumbnails"))]
        self.request_selected_local_artwork();
    }

    fn refresh_tracker_rows(&mut self) {
        self.view.rows =
            self.tracker_results
                .iter()
                .map(|item| RowView {
                    media_id: item.prepared_path.as_ref().map(|_| {
                        MediaId::new(SourceKind::ModArchive, item.webpage_url.to_string())
                    }),
                    title: item.title.clone(),
                    subtitle: item.subtitle.clone(),
                    source: item.source.clone(),
                    ..RowView::default()
                })
                .collect();
        self.view.selected = self
            .view
            .selected
            .min(self.view.rows.len().saturating_sub(1));
        self.update_non_youtube_detail();
    }

    #[cfg(feature = "tracker-music")]
    fn finish_tracker_preparation(
        &mut self,
        item_key: &str,
        modules: Vec<crate::tracker_media::PreparedTrackerModule>,
        owner: TrackerPreparationOwner,
    ) {
        let autoplay_origin = match &owner {
            TrackerPreparationOwner::Autoplay(origin) if self.config.playback.autoplay => {
                Some(origin.clone())
            }
            _ => None,
        };
        let Some(index) = self
            .tracker_results
            .iter()
            .position(|item| item.webpage_url.as_str() == item_key)
        else {
            if let Some(origin) = autoplay_origin {
                self.continue_autoplay(origin);
            } else if owner == TrackerPreparationOwner::Manual {
                self.show_error_message(
                    "Tracker module preparation failed",
                    "the prepared result no longer exists in the current tracker search",
                );
            }
            return;
        };
        let template = self.tracker_results[index].clone();
        let mut prepared = modules
            .into_iter()
            .map(|module| {
                let mut item = template.clone();
                item.title = module.display_name;
                item.expected_format = Some(module.format.clone());
                item.subtitle = format!(
                    "{} · {}",
                    module.format.to_ascii_uppercase(),
                    human_bytes(module.size_bytes)
                );
                item.prepared_path = Some(module.path);
                item.access = TrackerAccess::DirectModule;
                item
            })
            .collect::<Vec<_>>();
        if prepared.is_empty() {
            if let Some(origin) = autoplay_origin {
                self.view.status_line =
                    "Autoplay skipped a payload with no supported tracker module".to_owned();
                self.continue_autoplay(origin);
            } else if owner == TrackerPreparationOwner::Manual {
                self.show_error_message(
                    "Tracker module preparation failed",
                    "the selected payload contained no supported tracker module",
                );
            } else {
                self.view.status_line =
                    "Canceled tracker preparation contained no playable module".to_owned();
            }
            return;
        }
        let prepared_count = prepared.len();
        self.tracker_results
            .splice(index..=index, prepared.drain(..));
        self.view.selected = index;
        self.tracker_selected = index;
        if self.view.screen == Screen::TrackerMusic {
            self.refresh_tracker_rows();
        }
        if owner == TrackerPreparationOwner::CanceledAutoplay
            || (matches!(&owner, TrackerPreparationOwner::Autoplay(_))
                && !self.config.playback.autoplay)
        {
            self.view.status_line =
                format!("Prepared {prepared_count} tracker module(s); autoplay remains off");
            return;
        }
        if prepared_count > 1 && owner == TrackerPreparationOwner::Manual {
            self.view.status_line =
                format!("Prepared {prepared_count} modules; select the extracted entry to play");
            return;
        }
        let item = match queue_item_from_tracker(&self.tracker_results[index]) {
            Ok(item) => item,
            Err(error) => {
                self.show_error_message("Tracker module preparation failed", error);
                return;
            }
        };
        match owner {
            TrackerPreparationOwner::Autoplay(_) => self.play_queue_item_with_origin(
                item,
                false,
                Some(AutoplayOrigin::Tracker {
                    generation: self.search_generation,
                    index,
                }),
            ),
            TrackerPreparationOwner::Manual => self.play_queue_item(item, false),
            TrackerPreparationOwner::CanceledAutoplay => {
                unreachable!("canceled preparation returned before queue activation")
            }
        }
    }

    fn update_non_youtube_detail(&mut self) {
        match self.view.screen {
            Screen::Local => {
                self.update_local_browser_detail();
                return;
            }
            Screen::History => {
                self.update_history_detail();
                return;
            }
            Screen::Search | Screen::TrackerMusic => {}
            Screen::YouTubeMusic
            | Screen::Subscriptions
            | Screen::Downloaded
            | Screen::Playlists
            | Screen::Statistics => {
                self.view.details = None;
                return;
            }
        }
        if self.view.screen == Screen::Search
            && let Some(item) = self.local_results.get(self.view.selected)
        {
            self.view.details = Some(DetailView {
                media_id: Some(MediaId::new(
                    SourceKind::Local,
                    item.path.display().to_string(),
                )),
                title: item.title.clone(),
                source: "Local".to_owned(),
                length: item
                    .duration_seconds
                    .map_or_else(|| "unknown".to_owned(), format_seconds),
                description: local_media_description(item),
                license: "local file".to_owned(),
                wikidata: "not applicable".to_owned(),
                thumbnail_url: None,
                ..DetailView::default()
            });
            #[cfg(all(feature = "local", feature = "thumbnails"))]
            self.request_selected_local_artwork();
        } else if self.view.screen == Screen::TrackerMusic
            && let Some(item) = self.tracker_results.get(self.view.selected)
        {
            self.view.details = Some(DetailView {
                title: item.title.clone(),
                source: item.source.clone(),
                description: format!(
                    "{}\n{}",
                    item.subtitle,
                    if item.insecure_transport {
                        "Warning: this result uses plaintext HTTP."
                    } else {
                        item.webpage_url.as_str()
                    }
                ),
                license: "check source metadata".to_owned(),
                wikidata: "not loaded".to_owned(),
                ..DetailView::default()
            });
        }
    }

    /// Rebuilds History Details exclusively from the selected persisted row.
    ///
    /// History must not index into stale Local-search or tracker-search arrays:
    /// those arrays can retain unrelated URLs after the user changes screens.
    fn update_history_detail(&mut self) {
        let Some(row) = self.view.rows.get(self.view.selected).cloned() else {
            self.view.details = None;
            return;
        };
        self.view.details = Some(DetailView {
            media_id: row.media_id,
            title: row.title,
            source: row.source,
            description: row.subtitle,
            ..DetailView::default()
        });
    }

    fn move_selection(&mut self, delta: i32) {
        if self.view.screen == Screen::Subscriptions {
            match (
                self.view.subscriptions.layout,
                self.view.subscriptions.route,
                self.view.subscriptions.focus,
            ) {
                (SubscriptionsLayout::DrillDown, SubscriptionRoute::Sources, _)
                | (SubscriptionsLayout::Split, _, SubscriptionPane::Sources) => {
                    let Some(index) = moved_index(
                        self.view.subscriptions.selected_source,
                        self.subscription_entries.len(),
                        delta,
                    ) else {
                        return;
                    };
                    self.select_subscription_source(index);
                }
                _ => {
                    let Some(index) = moved_index(
                        self.view.subscriptions.selected_item,
                        self.view.subscriptions.items.len(),
                        delta,
                    ) else {
                        return;
                    };
                    self.select_subscription_item(index);
                }
            }
            return;
        }
        if self.view.rows.is_empty() {
            return;
        }
        let last = self.view.rows.len().saturating_sub(1);
        self.view.selected = if delta.is_negative() {
            self.view
                .selected
                .saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.view.selected.saturating_add(delta as usize).min(last)
        };
        if matches!(self.view.screen, Screen::Search | Screen::YouTubeMusic) {
            let youtube_list_active = self.view.screen == Screen::YouTubeMusic
                || (self.local_results.is_empty()
                    && self.direct_item.is_none()
                    && self.resolved_direct.is_none());
            if youtube_list_active {
                self.request_selected_details();
            } else {
                self.update_non_youtube_detail();
            }
            if self.view.screen == Screen::Search
                && self.local_results.is_empty()
                && self.direct_item.is_none()
                && self.view.selected.saturating_add(2) >= self.view.rows.len()
                && let Some(page) = self.next_youtube_page
            {
                self.submit_youtube_search(page);
            }
        } else if matches!(
            self.view.screen,
            Screen::TrackerMusic | Screen::Local | Screen::History
        ) {
            self.update_non_youtube_detail();
        }
    }

    fn select_row(&mut self, row: usize) {
        if row >= self.view.rows.len() {
            return;
        }
        self.view.selected = row;
        let youtube_list_active = self.view.screen == Screen::YouTubeMusic
            || (self.view.screen == Screen::Search
                && self.local_results.is_empty()
                && self.direct_item.is_none());
        if youtube_list_active {
            self.request_selected_details();
        } else {
            self.update_non_youtube_detail();
        }
    }

    /// Builds a queue entry using full visible description chapters when ready.
    fn selected_video_queue_item(
        &self,
        video: &VideoSummary,
        start_at_seconds: Option<u64>,
    ) -> QueueItem {
        let mut item = queue_item_from_video(video, start_at_seconds);
        let media_id = MediaId::new(SourceKind::YouTube, &video.video_id);
        if let Some(details) = self
            .view
            .details
            .as_ref()
            .filter(|details| details.media_id.as_ref() == Some(&media_id))
        {
            item.media.chapters =
                description_chapters(&details.description, video.duration_seconds);
        }
        item
    }

    fn selected_queue_item(&self) -> Result<QueueItem, String> {
        if self.view.screen == Screen::TrackerMusic {
            let item = self
                .tracker_results
                .get(self.view.selected)
                .ok_or_else(|| "No tracker item is selected".to_owned())?;
            return queue_item_from_tracker(item);
        }
        if self.view.screen == Screen::Subscriptions {
            let video_is_active = match self.view.subscriptions.layout {
                SubscriptionsLayout::DrillDown => {
                    self.view.subscriptions.route == SubscriptionRoute::Items
                }
                SubscriptionsLayout::Split => {
                    self.view.subscriptions.focus == SubscriptionPane::Items
                        || self.view.subscriptions.description_expanded
                }
            };
            if !video_is_active {
                return Err("Focus a subscription video before using queue actions".to_owned());
            }
            return match self.selected_subscription_item() {
                Some(SearchItem::Video(video)) => Ok(self.selected_video_queue_item(video, None)),
                Some(SearchItem::Channel(_)) => Err("YouTube channels cannot be queued".to_owned()),
                None => Err("No subscription video is selected".to_owned()),
            };
        }
        if !matches!(self.view.screen, Screen::Search | Screen::YouTubeMusic) {
            return Err("Queue actions are available for playable search results".to_owned());
        }
        if self.view.screen == Screen::YouTubeMusic {
            return match self.youtube_music_results.get(self.view.selected) {
                Some(SearchItem::Video(video)) => {
                    Ok(self.selected_video_queue_item(video, self.youtube_music_start_override))
                }
                Some(SearchItem::Channel(_)) => {
                    Err("YouTube Music channels cannot be queued".to_owned())
                }
                None => Err("No playable YouTube Music track is selected".to_owned()),
            };
        }
        if let Some(item) = self.local_results.get(self.view.selected) {
            return queue_item_from_local(item);
        }
        if let Some(media) = &self.resolved_direct {
            return queue_item_from_resolved(media);
        }
        if let Some(direct) = &self.direct_item {
            if requires_first_class_direct_resolution(&direct.source)
                || direct.source == SourceKind::ApplePodcasts
            {
                return Err(format!(
                    "{} metadata must resolve before it can be queued",
                    direct_source_label(&direct.source)
                ));
            }
            return Ok(queue_item_from_direct(direct));
        }
        match self.youtube_results.get(self.view.selected) {
            Some(SearchItem::Video(video)) => {
                Ok(self.selected_video_queue_item(video, self.selected_start_override))
            }
            Some(SearchItem::Channel(_)) => Err("YouTube channels cannot be queued".to_owned()),
            None => Err("No playable item is selected".to_owned()),
        }
    }

    fn queue_selected(&mut self, play_next: bool) {
        let item = match self.selected_queue_item() {
            Ok(item) => item,
            Err(error) => {
                self.view.status_line = error;
                return;
            }
        };
        let title = item.media.title.clone();
        if play_next {
            self.playback_queue.play_next(item);
            self.view.status_line = format!("{title} will play next");
        } else {
            self.playback_queue.push(item);
            self.view.status_line = format!("{title} added to the queue");
        }
    }

    #[cfg(feature = "yt-dlp")]
    fn start_selected_download(&mut self) {
        if self.active_download.is_some() {
            self.view.status_line =
                "One download is already running; wait for it to finish".to_owned();
            return;
        }
        let item = match self.selected_queue_item() {
            Ok(item) => item,
            Err(error) => {
                self.view.status_line = error;
                return;
            }
        };
        let source_url = item.media.webpage_url.clone();
        if !matches!(source_url.scheme(), "http" | "https")
            || source_url.host_str().is_none()
            || !source_url.username().is_empty()
            || source_url.password().is_some()
        {
            self.view.status_line =
                "Downloads require a credential-free remote HTTP(S) item".to_owned();
            return;
        }
        let destination = match prepare_download_destination(&self.config) {
            Ok(destination) => destination,
            Err(error) => {
                self.show_error_message("Download destination is unavailable", error);
                return;
            }
        };
        let format = match configured_download_format(&self.config.subscriptions.audio_format) {
            Ok(format) => format,
            Err(error) => {
                self.show_error_message("Download format is invalid", error);
                return;
            }
        };
        let request = DownloadRequest {
            source_url,
            destination: destination.clone(),
            format,
            write_thumbnail: self.config.subscriptions.download_thumbnails,
        };
        let process = match self.download_launcher.start(&request) {
            Ok(process) => process,
            Err(error) => {
                self.show_error_message("Download could not start", error);
                return;
            }
        };
        let title = item.media.title;
        let active = match ActiveDownload::start(title.clone(), destination, process) {
            Ok(active) => active,
            Err(error) => {
                self.show_error_message("Download supervision could not start", error);
                return;
            }
        };
        self.view.download = Some(DownloadView {
            title: title.clone(),
            active: true,
            ..DownloadView::default()
        });
        self.view.status_line = format!("Downloading {title}");
        self.active_download = Some(active);
    }

    #[cfg(not(feature = "yt-dlp"))]
    fn start_selected_download(&mut self) {
        self.view.status_line =
            "Download support was disabled when this Youta binary was built".to_owned();
    }

    #[cfg(feature = "yt-dlp")]
    fn poll_download(&mut self) {
        let Some(active) = self.active_download.as_mut() else {
            return;
        };
        let (progress, read_error) = {
            let output = active
                .output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (output.progress, output.read_error.clone())
        };
        apply_download_progress(&mut self.view, &active.title, progress);

        if let Some(error) = read_error {
            let mut active = self
                .active_download
                .take()
                .expect("an active download was checked above");
            active.cancel_and_join();
            let diagnostics = download_diagnostics(&active);
            mark_download_inactive(&mut self.view);
            self.show_error_message(
                "Download output could not be read",
                append_download_diagnostics(error, &diagnostics),
            );
            return;
        }

        let exit = match active.process.try_wait() {
            Ok(exit) => exit,
            Err(error) => {
                let mut active = self
                    .active_download
                    .take()
                    .expect("an active download was checked above");
                active.cancel_and_join();
                let diagnostics = download_diagnostics(&active);
                mark_download_inactive(&mut self.view);
                self.show_error_message(
                    "Download process could not be monitored",
                    append_download_diagnostics(error, &diagnostics),
                );
                return;
            }
        };
        let Some(exit) = exit else {
            return;
        };

        let mut active = self
            .active_download
            .take()
            .expect("the completed download is still active");
        active.join_readers();
        let (progress, completed_path, read_error, diagnostics) = {
            let output = active
                .output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                output.progress,
                output.completed_paths.back().cloned(),
                output.read_error.clone(),
                output.diagnostics(),
            )
        };
        apply_download_progress(&mut self.view, &active.title, progress);
        if let Some(error) = read_error {
            mark_download_inactive(&mut self.view);
            self.show_error_message(
                "Download output could not be read",
                append_download_diagnostics(error, &diagnostics),
            );
            return;
        }
        if !exit.success {
            mark_download_inactive(&mut self.view);
            self.show_error_message(
                "Download failed",
                append_download_diagnostics(
                    format!("yt-dlp exited with {}", exit.description),
                    &diagnostics,
                ),
            );
            return;
        }
        let Some(completed_path) = completed_path else {
            mark_download_inactive(&mut self.view);
            self.show_error_message(
                "Download result is incomplete",
                append_download_diagnostics(
                    "yt-dlp succeeded but did not report a completed media path".to_owned(),
                    &diagnostics,
                ),
            );
            return;
        };
        let completed_path =
            match validate_completed_download_path(&active.destination, &completed_path) {
                Ok(path) => path,
                Err(error) => {
                    mark_download_inactive(&mut self.view);
                    self.show_error_message("Download path failed validation", error);
                    return;
                }
            };
        self.view.download = Some(DownloadView {
            title: active.title,
            downloaded_bytes: progress.downloaded_bytes,
            total_bytes: progress.total_bytes,
            bytes_per_second: progress.bytes_per_second.map(rounded_download_rate),
            eta_seconds: Some(0),
            active: false,
            completed_path: Some(completed_path.display().to_string()),
        });
        if self.view.screen == Screen::Downloaded {
            self.populate_downloads();
        }
        self.view.status_line = format!("Downloaded {}", completed_path.display());
    }

    fn activate_selection(&mut self) {
        if self.view.screen == Screen::Local {
            self.activate_local_browser_selection();
            return;
        }
        if self.view.screen == Screen::Subscriptions {
            match (
                self.view.subscriptions.layout,
                self.view.subscriptions.route,
                self.view.subscriptions.focus,
            ) {
                (SubscriptionsLayout::DrillDown, SubscriptionRoute::Sources, _) => {
                    self.view.subscriptions.route = SubscriptionRoute::Items;
                    self.view.subscriptions.focus = SubscriptionPane::Items;
                    self.view.right_panel_mode = RightPanelMode::Details;
                    self.load_selected_subscription_videos();
                    if self.view.subscriptions.loading {
                        self.view.status_line = format!(
                            "Loading videos for {}…",
                            self.view.subscriptions.source_title
                        );
                    }
                    return;
                }
                (SubscriptionsLayout::Split, _, SubscriptionPane::Sources) => {
                    self.view.subscriptions.focus = SubscriptionPane::Items;
                    self.view.right_panel_mode = RightPanelMode::Details;
                    self.load_selected_subscription_videos();
                    if self.view.subscriptions.items.is_empty() && !self.view.subscriptions.loading
                    {
                        self.view.status_line =
                            "This subscription has no loaded playable videos".to_owned();
                    } else if !self.view.subscriptions.items.is_empty() {
                        self.request_selected_details();
                    }
                    return;
                }
                _ => {}
            }
            if self.view.subscriptions.focus == SubscriptionPane::Items
                && self.view.subscriptions.items.is_empty()
            {
                self.load_next_subscription_page_if_needed();
                if !self.view.subscriptions.loading {
                    self.view.status_line =
                        "This subscription has no loaded playable videos".to_owned();
                }
                return;
            }
        }
        #[cfg(feature = "tracker-music")]
        if self.view.screen == Screen::TrackerMusic && self.prepare_selected_tracker_item() {
            return;
        }
        let item = match self.selected_queue_item() {
            Ok(item) => item,
            Err(error) => {
                if matches!(self.selected_youtube_item(), Some(SearchItem::Channel(_))) {
                    self.show_selected_channel();
                } else {
                    self.view.status_line = error;
                }
                return;
            }
        };
        if item.media.id.source == SourceKind::YouTube {
            if self.view.screen == Screen::YouTubeMusic {
                self.youtube_music_start_override = None;
            } else {
                self.selected_start_override = None;
            }
        }
        self.play_queue_item(item, false);
    }

    fn activate_local_browser_selection(&mut self) {
        use crate::local_browser::LocalEntryKind;

        let Some(listing) = self.local_listing.as_ref() else {
            self.view.status_line = "No local folder is loaded".to_owned();
            return;
        };
        if listing.parent.is_some() && self.view.selected == 0 {
            let parent = listing.parent.clone().expect("parent checked above");
            let child = listing.path.clone();
            self.browse_local_directory_with_reselection(parent, Some(child));
            return;
        }
        let Some(index) = self.local_entry_index() else {
            self.view.status_line = "No local entry is selected".to_owned();
            return;
        };
        let entry = listing.entries[index].clone();
        match entry.kind {
            LocalEntryKind::Directory => self.browse_local_directory(entry.path),
            LocalEntryKind::Image => {
                self.view.status_line =
                    "Image preview selected; only audio and video entries are playable".to_owned();
            }
            LocalEntryKind::Audio | LocalEntryKind::Video | LocalEntryKind::TrackerModule => {
                let item = local_media_item(entry.path);
                match queue_item_from_local(&item) {
                    Ok(item) => self.play_queue_item(item, false),
                    Err(error) => self.show_error_message("Local media could not be played", error),
                }
            }
        }
    }

    /// Starts bounded background preparation for a selected remote module.
    ///
    /// Returns `true` when activation was fully handled without immediately
    /// entering the ordinary queue path.
    #[cfg(feature = "tracker-music")]
    fn prepare_selected_tracker_item(&mut self) -> bool {
        if self.view.selected >= self.tracker_results.len() {
            self.view.status_line = "No tracker item is selected".to_owned();
            return true;
        }
        self.prepare_tracker_item(self.view.selected, None)
    }

    /// Starts one selected or autoplay-owned tracker preparation.
    ///
    /// Returns `true` when the request was handled without entering the
    /// ordinary immediate-play path.
    #[cfg(feature = "tracker-music")]
    fn prepare_tracker_item(
        &mut self,
        index: usize,
        autoplay_origin: Option<AutoplayOrigin>,
    ) -> bool {
        if self.pending_tracker_preparation.is_some() {
            self.view.status_line =
                "One tracker module is already being prepared in the background".to_owned();
            return true;
        }
        let Some(item) = self.tracker_results.get(index) else {
            return true;
        };
        if item.prepared_path.is_some() {
            return false;
        }
        let Some(download_url) = item.download_url.clone() else {
            self.view.status_line = match item.access {
                TrackerAccess::Directory => {
                    "This tracker result is a directory; open its source page to browse it"
                        .to_owned()
                }
                _ => "This tracker result is metadata only; press o to open its source page"
                    .to_owned(),
            };
            return true;
        };
        let item_key = item.webpage_url.to_string();
        let title = item.title.clone();
        let mut request = crate::tracker_media::TrackerMediaRequest::new(download_url);
        request.source_label = Some(item.source.clone());
        request.expected_format.clone_from(&item.expected_format);
        request.display_name = Some(title.clone());
        request.allow_insecure_http =
            item.insecure_transport && self.config.providers.allow_insecure_http;
        if !self.send_provider_request(
            ProviderRequest::PrepareTracker {
                generation: self.search_generation,
                item_key: item_key.clone(),
                request,
            },
            "Could not prepare the tracker module",
        ) {
            return true;
        }
        let autoplay = autoplay_origin.is_some();
        let owner = autoplay_origin.map_or(
            TrackerPreparationOwner::Manual,
            TrackerPreparationOwner::Autoplay,
        );
        self.pending_tracker_preparation = Some(PendingTrackerPreparation {
            generation: self.search_generation,
            item_key,
            owner,
        });
        self.begin_playback_start_activity();
        self.view.status_line = if autoplay {
            format!("Autoplay is downloading and inspecting {title}…")
        } else {
            format!("Downloading and inspecting {title}…")
        };
        true
    }

    fn show_now_playing(&mut self) {
        let Some(item) = self.playback_queue.current().cloned() else {
            self.view.status_line = "Nothing is playing".to_owned();
            return;
        };
        let music_origin = item.media.webpage_url.host_str() == Some("music.youtube.com");

        if item.media.id.source == SourceKind::YouTube
            && (self.view.screen == Screen::YouTubeMusic || music_origin)
            && let Some(index) = self.youtube_music_results.iter().position(|candidate| {
                matches!(
                    candidate,
                    SearchItem::Video(video)
                        if video.video_id == item.media.id.external_id
                )
            })
        {
            self.view.screen = Screen::YouTubeMusic;
            self.refresh_youtube_music_rows();
            self.view.selected = index;
            self.view.right_panel_mode = RightPanelMode::Details;
            self.request_selected_details();
            self.view.status_line = format!("Selected playing item: {}", item.media.title);
            return;
        }

        if item.media.id.source == SourceKind::YouTube
            && let Some(index) = self.youtube_results.iter().position(|candidate| {
                matches!(
                    candidate,
                    SearchItem::Video(video)
                        if video.video_id == item.media.id.external_id
                )
            })
        {
            self.view.screen = Screen::Search;
            self.refresh_youtube_rows();
            self.view.selected = index;
            self.view.right_panel_mode = RightPanelMode::Details;
            self.request_selected_details();
            self.view.status_line = format!("Selected playing item: {}", item.media.title);
            return;
        }

        if item.media.id.source == SourceKind::YouTube
            && let Some(index) = self.youtube_music_results.iter().position(|candidate| {
                matches!(
                    candidate,
                    SearchItem::Video(video)
                        if video.video_id == item.media.id.external_id
                )
            })
        {
            self.view.screen = Screen::YouTubeMusic;
            self.refresh_youtube_music_rows();
            self.view.selected = index;
            self.view.right_panel_mode = RightPanelMode::Details;
            self.request_selected_details();
            self.view.status_line = format!("Selected playing item: {}", item.media.title);
            return;
        }

        if item.media.id.source == SourceKind::YouTube {
            let cached_subscription_match =
                self.subscription_video_cache
                    .iter()
                    .find_map(|(channel_id, cached)| {
                        cached
                            .items
                            .iter()
                            .position(|candidate| {
                                matches!(
                                    candidate,
                                    SearchItem::Video(video)
                                        if video.video_id == item.media.id.external_id
                                )
                            })
                            .map(|index| (channel_id.clone(), index))
                    });
            if let Some((channel_id, index)) = cached_subscription_match
                && self.subscription_tree.contains_youtube_channel(&channel_id)
            {
                self.subscription_entries = self.subscription_tree.flattened_subscriptions();
                if let Some(source_index) = self.subscription_entries.iter().position(|entry| {
                    entry.subscription.youtube_channel_id().as_deref() == Some(&channel_id)
                }) {
                    self.view.screen = Screen::Subscriptions;
                    self.view.subscriptions.sources = self
                        .subscription_entries
                        .iter()
                        .map(subscription_source_row)
                        .collect();
                    self.view.subscriptions.selected_source = source_index;
                    self.update_selected_subscription_source();
                    self.active_subscription_channel_id = Some(channel_id);
                    self.view.subscriptions.route = SubscriptionRoute::Items;
                    self.view.subscriptions.focus = SubscriptionPane::Items;
                    self.view.subscriptions.selected_item = index;
                    self.view.subscriptions.description_expanded =
                        self.view.subscriptions.layout == SubscriptionsLayout::Split;
                    self.refresh_subscription_video_rows();
                    self.view.right_panel_mode = RightPanelMode::Details;
                    self.request_selected_details();
                    self.view.status_line = format!("Selected playing item: {}", item.media.title);
                    return;
                }
            }
        }

        if item.media.id.source == SourceKind::Local
            && let Some(listing) = self.local_listing.as_ref()
            && let Some(index) = listing
                .entries
                .iter()
                .position(|entry| entry.path.display().to_string() == item.media.id.external_id)
        {
            self.view.screen = Screen::Local;
            self.view.selected = index.saturating_add(usize::from(listing.parent.is_some()));
            self.view.right_panel_mode = RightPanelMode::Details;
            self.update_local_browser_detail();
            self.view.status_line = format!("Selected playing item: {}", item.media.title);
            return;
        }

        if item.media.id.source == SourceKind::ModArchive
            && let Some(index) = self
                .tracker_results
                .iter()
                .position(|candidate| candidate.webpage_url.as_str() == item.media.id.external_id)
        {
            self.view.screen = Screen::TrackerMusic;
            self.view.selected = index;
            self.refresh_tracker_rows();
            self.view.right_panel_mode = RightPanelMode::Details;
            self.view.status_line = format!("Selected playing item: {}", item.media.title);
            return;
        }

        self.view.right_panel_mode = RightPanelMode::Details;
        self.view.details_focused = true;
        self.view.details_scroll = 0;
        self.view.selected_detail_link = None;
        self.view.details = Some(detail_from_media_item(&item.media));
        self.view.status_line = format!(
            "Showing queued details for {}; it is not in the current list",
            item.media.title
        );
    }

    fn show_selected_channel(&mut self) {
        if self.view.screen == Screen::Subscriptions
            && self.view.subscriptions.focus == SubscriptionPane::Sources
        {
            self.update_selected_subscription_source();
            self.view.details_focused = true;
            self.view.right_panel_mode = RightPanelMode::Channel;
            self.view.status_line =
                format!("Channel selected: {}", self.view.subscriptions.source_title);
            self.schedule_visible_channel_details(Instant::now());
            return;
        }
        let Some(selected) = self.selected_youtube_item().cloned() else {
            self.view.status_line = "No channel information is available for this item".to_owned();
            return;
        };
        let (channel_id, name, description, channel_webpage_url) = match selected {
            SearchItem::Video(video) if !video.channel_id.is_empty() => {
                self.previous_detail = self.view.details.take();
                let channel_webpage_url = canonical_youtube_channel_url(&video.channel_id);
                (
                    video.channel_id,
                    video.channel_name,
                    String::new(),
                    channel_webpage_url,
                )
            }
            SearchItem::Video(_) => {
                self.view.status_line =
                    "The video provider has not returned a channel ID yet".to_owned();
                return;
            }
            SearchItem::Channel(channel) => (
                channel.channel_id.clone(),
                channel.name,
                channel.description,
                youtube_channel_webpage_url(&channel.channel_id, channel.webpage_url),
            ),
        };
        self.details_generation = self.details_generation.wrapping_add(1);
        self.channel_details_generation = self.channel_details_generation.wrapping_add(1);
        self.scheduled_channel_details = None;
        self.view.details_focused = true;
        self.view.details_scroll = 0;
        self.view.details = Some(DetailView {
            title: name.clone(),
            source: "YouTube channel".to_owned(),
            channel_name: name.clone(),
            channel_id: channel_id.clone(),
            channel_webpage_url,
            channel_subscribed: self.subscription_tree.contains_youtube_channel(&channel_id),
            description: if description.is_empty() {
                format!("YouTube channel ID: {channel_id}")
            } else {
                description
            },
            license: "not applicable".to_owned(),
            wikidata: "not loaded".to_owned(),
            ..DetailView::default()
        });
        self.view.right_panel_mode = RightPanelMode::Channel;
        self.view.status_line = format!("Channel selected: {name}");
        #[cfg(feature = "wikidata")]
        self.request_wikidata(
            crate::providers::wikidata::WikidataExternalKind::YouTubeChannel,
            &channel_id,
        );
        #[cfg(not(feature = "wikidata"))]
        if let Some(details) = self.view.details.as_mut() {
            details.wikidata = "disabled at build time".to_owned();
        }
        self.schedule_visible_channel_details(Instant::now());
    }

    fn toggle_local_subscription(&mut self) {
        let Some(details) = self.view.details.as_ref() else {
            self.view.status_line = "No channel is selected".to_owned();
            return;
        };
        if details.channel_id.is_empty() {
            self.view.status_line =
                "The provider has not returned a subscribable channel ID".to_owned();
            return;
        }
        let channel_id = details.channel_id.clone();
        let channel_name = if details.channel_name.is_empty() {
            details.title.clone()
        } else {
            details.channel_name.clone()
        };
        let now_subscribed = !details.channel_subscribed;

        // Reload on each explicit mutation so external OPML edits are retained
        // and a malformed existing file can never be replaced by an empty tree.
        let mut candidate = match subscriptions::load(&self.config) {
            Ok(tree) => tree,
            Err(error) => {
                self.show_error("Cannot change local subscriptions", &error);
                return;
            }
        };
        let persisted_subscribed = candidate.contains_youtube_channel(&channel_id);
        if persisted_subscribed != now_subscribed {
            let changed = if now_subscribed {
                candidate.subscribe_youtube_channel(channel_name.clone(), &channel_id)
            } else {
                candidate.unsubscribe_youtube_channel(&channel_id)
            };
            if !changed {
                self.show_error_message(
                    "Cannot change local subscriptions",
                    "The requested local subscription change could not be represented in OPML",
                );
                return;
            }
            if let Err(error) = subscriptions::save(&self.config, &candidate) {
                self.show_error("Cannot save local subscriptions", &error);
                return;
            }
        }

        self.subscription_tree = candidate;
        if !now_subscribed {
            self.subscription_generation = self.subscription_generation.wrapping_add(1);
            self.pending_subscription_refresh = None;
            self.subscription_video_cache.remove(&channel_id);
            self.subscription_cache_order
                .retain(|cached| cached != &channel_id);
            if let Err(error) = self
                .store
                .delete_cached_subscription_items(&SourceKind::YouTube, &channel_id)
            {
                self.show_error("Cannot remove cached subscription videos", &error);
            }
        }
        if let Some(details) = self.view.details.as_mut()
            && details.channel_id == channel_id
        {
            details.channel_subscribed = now_subscribed;
        }
        if let Some(details) = self.previous_detail.as_mut()
            && details.channel_id == channel_id
        {
            details.channel_subscribed = now_subscribed;
        }
        if self.view.screen == Screen::Subscriptions {
            self.populate_subscriptions();
        } else {
            self.refresh_youtube_rows();
        }
        self.view.status_line = format!(
            "{} {channel_name} locally",
            if now_subscribed {
                "Subscribed to"
            } else {
                "Unsubscribed from"
            }
        );
    }

    /// Opens the focused RSS/Atom feed editor.
    ///
    /// Feed URLs can contain private query tokens, so the popup's custom
    /// `Debug` implementation redacts both its draft and validation message.
    fn open_rss_subscription_popup(&mut self) {
        if self.view.rss_subscription_popup.is_some() {
            return;
        }
        self.view.help_open = false;
        self.view.text_selection_mode = false;
        self.view.details_text_selection = None;
        self.view.rss_subscription_popup = Some(RssSubscriptionPopupView {
            url: String::new(),
            validation_error: None,
            config_path: self.config.subscriptions_file().display().to_string(),
        });
        self.view.status_line = "Enter an RSS or Atom podcast feed URL".to_owned();
    }

    /// Persists one validated feed without overwriting external OPML edits.
    fn submit_rss_subscription(&mut self) {
        let Some(url) = self
            .view
            .rss_subscription_popup
            .as_ref()
            .map(|popup| popup.url.clone())
        else {
            return;
        };
        let mut candidate = match subscriptions::load(&self.config) {
            Ok(tree) => tree,
            Err(error) => {
                if let Some(popup) = self.view.rss_subscription_popup.as_mut() {
                    popup.validation_error = Some(error.to_string());
                }
                return;
            }
        };
        let changed = match candidate.subscribe_rss_feed("", &url) {
            Ok(changed) => changed,
            Err(error) => {
                if let Some(popup) = self.view.rss_subscription_popup.as_mut() {
                    popup.validation_error = Some(error.to_string());
                }
                return;
            }
        };
        if !changed {
            self.view.rss_subscription_popup = None;
            self.subscription_tree = candidate;
            self.populate_subscriptions();
            self.view.status_line = "That RSS feed is already subscribed locally".to_owned();
            return;
        }
        if let Err(error) = subscriptions::save(&self.config, &candidate) {
            if let Some(popup) = self.view.rss_subscription_popup.as_mut() {
                popup.validation_error = Some(error.to_string());
            }
            return;
        }
        self.view.rss_subscription_popup = None;
        self.subscription_tree = candidate;
        self.populate_subscriptions();
        self.view.status_line = "RSS feed subscribed locally".to_owned();
    }

    /// Drops internal Details history when a list or top-level route changes.
    fn clear_detail_navigation_history(&mut self) {
        self.detail_navigation_back.clear();
        self.detail_navigation_forward.clear();
        self.active_description_video = None;
    }

    /// Moves the current Details location into a bounded history stack.
    fn push_detail_navigation_snapshot(
        history: &mut VecDeque<DetailNavigationSnapshot>,
        snapshot: DetailNavigationSnapshot,
    ) {
        if history.len() >= MAX_DETAIL_NAVIGATION_HISTORY {
            history.pop_front();
        }
        history.push_back(snapshot);
    }

    /// Takes the current location without cloning its potentially large text.
    fn take_detail_navigation_snapshot(&mut self) -> DetailNavigationSnapshot {
        DetailNavigationSnapshot {
            screen: self.view.screen,
            selected: self.view.selected,
            subscription_route: self.view.subscriptions.route,
            subscription_focus: self.view.subscriptions.focus,
            subscription_selected_source: self.view.subscriptions.selected_source,
            subscription_selected_item: self.view.subscriptions.selected_item,
            subscription_description_expanded: self.view.subscriptions.description_expanded,
            details: self.view.details.take(),
            previous_detail: self.previous_detail.take(),
            details_scroll: self.view.details_scroll,
            details_focused: self.view.details_focused,
            text_selection_mode: self.view.text_selection_mode,
            details_text_selection: self.view.details_text_selection.take(),
            selected_detail_link: self.view.selected_detail_link,
            right_panel_mode: self.view.right_panel_mode,
            active_description_video: self.active_description_video.take(),
        }
    }

    /// Restores one move-only Details location and invalidates stale responses.
    fn restore_detail_navigation_snapshot(&mut self, snapshot: DetailNavigationSnapshot) {
        self.details_generation = self.details_generation.wrapping_add(1);
        #[cfg(feature = "wikidata")]
        self.invalidate_wikidata_lookup();
        self.view.screen = snapshot.screen;
        self.view.selected = snapshot.selected;
        self.view.subscriptions.route = snapshot.subscription_route;
        self.view.subscriptions.focus = snapshot.subscription_focus;
        self.view.subscriptions.selected_source = snapshot.subscription_selected_source;
        self.view.subscriptions.selected_item = snapshot.subscription_selected_item;
        self.view.subscriptions.description_expanded = snapshot.subscription_description_expanded;
        self.view.details = snapshot.details;
        self.previous_detail = snapshot.previous_detail;
        self.view.details_scroll = snapshot.details_scroll;
        self.view.details_focused = snapshot.details_focused;
        self.view.text_selection_mode = snapshot.text_selection_mode;
        self.view.details_text_selection = snapshot.details_text_selection;
        self.view.selected_detail_link = snapshot.selected_detail_link;
        self.view.right_panel_mode = snapshot.right_panel_mode;
        self.active_description_video = snapshot.active_description_video;

        let pending_video_id = self.active_description_video.as_ref().and_then(|linked| {
            self.view
                .details
                .as_ref()
                .is_some_and(|details| details.title == LINKED_VIDEO_LOADING_TITLE)
                .then(|| linked.video_id.clone())
        });
        if let Some(video_id) = pending_video_id
            && self.youtube_provider_available
        {
            self.send_provider_request(
                ProviderRequest::Details {
                    generation: self.details_generation,
                    video_id: video_id.clone(),
                },
                "Could not resume loading linked YouTube video",
            );
        }
        #[cfg(feature = "wikidata")]
        if let Some(video_id) = self
            .active_description_video
            .as_ref()
            .map(|linked| linked.video_id.clone())
        {
            self.request_wikidata(
                crate::providers::wikidata::WikidataExternalKind::YouTubeVideo,
                &video_id,
            );
        }
    }

    /// Opens a description-linked `YouTube` video in the existing Details pane.
    fn activate_description_video(&mut self, video_id: String, start_seconds: Option<u64>) {
        if let Err(error) = validate_youtube_video_id(&video_id) {
            self.show_error_message("Cannot open description video", error.to_string());
            return;
        }
        if !self.youtube_provider_available {
            self.open_youtube_setup();
            return;
        }

        let generation = self.details_generation.wrapping_add(1);
        if !self.send_provider_request(
            ProviderRequest::Details {
                generation,
                video_id: video_id.clone(),
            },
            "Could not load linked YouTube video",
        ) {
            return;
        }

        let current = self.take_detail_navigation_snapshot();
        Self::push_detail_navigation_snapshot(&mut self.detail_navigation_back, current);
        self.detail_navigation_forward.clear();
        self.details_generation = generation;
        self.previous_detail = None;
        self.active_description_video = Some(ActiveDescriptionVideo {
            video_id: video_id.clone(),
            start_seconds,
        });
        self.view.details_focused = true;
        self.view.details_scroll = 0;
        self.view.text_selection_mode = false;
        self.view.details_text_selection = None;
        self.view.selected_detail_link = None;
        self.view.right_panel_mode = RightPanelMode::Details;
        self.view.details = Some(DetailView {
            media_id: Some(MediaId::new(SourceKind::YouTube, &video_id)),
            title: LINKED_VIDEO_LOADING_TITLE.to_owned(),
            source: "YouTube".to_owned(),
            description: LINKED_VIDEO_LOADING_DESCRIPTION.to_owned(),
            wikidata: "not loaded".to_owned(),
            ..DetailView::default()
        });
        self.view.status_line = start_seconds.map_or_else(
            || "Loading linked YouTube video…".to_owned(),
            |seconds| {
                format!(
                    "Loading linked YouTube video at {}…",
                    format_seconds(seconds)
                )
            },
        );
        #[cfg(feature = "wikidata")]
        self.request_wikidata(
            crate::providers::wikidata::WikidataExternalKind::YouTubeVideo,
            &video_id,
        );
    }

    /// Restores a previous or later internal Details location.
    fn move_detail_navigation(&mut self, forward: bool) -> bool {
        let destination = if forward {
            self.detail_navigation_forward.pop_back()
        } else {
            self.detail_navigation_back.pop_back()
        };
        let Some(destination) = destination else {
            return false;
        };
        let current = self.take_detail_navigation_snapshot();
        if forward {
            Self::push_detail_navigation_snapshot(&mut self.detail_navigation_back, current);
        } else {
            Self::push_detail_navigation_snapshot(&mut self.detail_navigation_forward, current);
        }
        self.restore_detail_navigation_snapshot(destination);
        self.view.status_line = if forward {
            "Moved forward in Details".to_owned()
        } else {
            "Moved back in Details".to_owned()
        };
        true
    }

    fn go_back(&mut self) {
        if self.move_detail_navigation(false) {
            return;
        }
        if self.view.screen == Screen::Subscriptions {
            if self.view.subscriptions.description_expanded {
                self.view.subscriptions.description_expanded = false;
                self.view.subscriptions.focus = SubscriptionPane::Items;
                self.view.details_focused = false;
                self.view.status_line = "Returned to subscription videos".to_owned();
                return;
            }
            if self.view.subscriptions.layout == SubscriptionsLayout::DrillDown
                && self.view.subscriptions.route == SubscriptionRoute::Items
            {
                self.pending_subscription_refresh = None;
                self.view.subscriptions.route = SubscriptionRoute::Sources;
                self.view.subscriptions.focus = SubscriptionPane::Sources;
                self.update_selected_subscription_source();
                self.view.status_line = "Returned to subscription sources".to_owned();
                return;
            }
            if self.view.subscriptions.layout == SubscriptionsLayout::Split
                && self.view.subscriptions.focus == SubscriptionPane::Items
            {
                self.pending_subscription_refresh = None;
                self.view.subscriptions.focus = SubscriptionPane::Sources;
                self.update_selected_subscription_source();
                self.view.status_line = "Subscription source list focused".to_owned();
                return;
            }
        }
        if let Some(media_id) = self.current_media.clone()
            && let Some((_, position)) = self
                .seek_back
                .back()
                .filter(|(candidate, _)| candidate == &media_id)
                .cloned()
        {
            self.seek_back.pop_back();
            self.player_command(PlayerCommand::SeekAbsolute(position));
            self.view.status_line = format!("Returned to {}", format_seconds(position.as_secs()));
            return;
        }
        if let Some(previous) = self.previous_detail.take() {
            self.view.details = Some(previous);
        }
        self.view.details_scroll = 0;
        self.view.right_panel_mode = RightPanelMode::Details;
        if let Some(selected) = self.selected_youtube_item().cloned() {
            self.request_selected_wikidata(&selected);
        }
    }

    fn go_forward(&mut self) {
        if !self.move_detail_navigation(true) {
            self.view.status_line = "No later Details page".to_owned();
        }
    }

    /// Seeks the active item, or starts the selected item, at a description timecode.
    fn activate_timecode(&mut self, media_id: MediaId, seconds: u64) {
        if self.current_media.as_ref() == Some(&media_id) && self.player.is_some() {
            if self
                .view
                .playback
                .duration
                .is_some_and(|duration| seconds >= duration.as_secs())
            {
                self.view.status_line = "That timecode is outside the media duration".to_owned();
                return;
            }
            let previous = self.view.playback.position;
            if previous != Duration::from_secs(seconds) {
                const MAX_SEEK_BACK_ENTRIES: usize = 32;
                if self.seek_back.len() == MAX_SEEK_BACK_ENTRIES {
                    self.seek_back.pop_front();
                }
                self.seek_back.push_back((media_id, previous));
            }
            self.player_command(PlayerCommand::SeekAbsolute(Duration::from_secs(seconds)));
            self.view.status_line = format!("Seeking to {}", format_seconds(seconds));
            return;
        }

        let selected_matches = self
            .view
            .details
            .as_ref()
            .and_then(|details| details.media_id.as_ref())
            == Some(&media_id)
            && self
                .selected_queue_item()
                .is_ok_and(|item| item.media.id == media_id);
        if selected_matches {
            self.selected_start_override = Some(seconds);
            self.activate_selection();
        } else {
            self.view.status_line =
                "That timecode does not belong to the selected or playing item".to_owned();
        }
    }

    /// Navigates parsed description chapters without relying on backend metadata.
    fn change_chapter(&mut self, delta: i32) {
        if self.view.playback_chapters.is_empty() {
            self.player_command(PlayerCommand::ChangeChapter(delta));
            return;
        }
        let navigable = self
            .view
            .playback_chapters
            .iter()
            .enumerate()
            .filter(|(_, chapter)| {
                !self.config.playback.skip_advertisement_chapters
                    || !is_advertisement_chapter_title(&chapter.title)
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if navigable.is_empty() {
            return;
        }
        let current = navigable
            .partition_point(|index| {
                self.view.playback_chapters[*index].start_seconds
                    <= self.view.playback.position.as_secs()
            })
            .saturating_sub(1);
        let destination_position = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            current
                .saturating_add(delta as usize)
                .min(navigable.len().saturating_sub(1))
        };
        let Some(chapter) = navigable
            .get(destination_position)
            .and_then(|index| self.view.playback_chapters.get(*index))
        else {
            return;
        };
        let Some(media_id) = self.current_media.clone() else {
            self.view.status_line = "Nothing is playing".to_owned();
            return;
        };
        self.activate_timecode(media_id, chapter.start_seconds);
    }

    /// Identifies the same-source cursor represented by a queue item.
    fn autoplay_origin_for_media(&self, media_id: &MediaId) -> Option<AutoplayOrigin> {
        let video_at = |items: &[SearchItem], index: usize| {
            items.get(index).is_some_and(|item| {
                matches!(
                    item,
                    SearchItem::Video(video)
                        if media_id.source == SourceKind::YouTube
                            && video.video_id == media_id.external_id
                )
            })
        };

        match self.view.screen {
            Screen::Search if video_at(&self.youtube_results, self.view.selected) => {
                return Some(AutoplayOrigin::YouTube {
                    generation: self.search_generation,
                    index: self.view.selected,
                });
            }
            Screen::YouTubeMusic if video_at(&self.youtube_music_results, self.view.selected) => {
                return Some(AutoplayOrigin::YouTubeMusic {
                    generation: self.search_generation,
                    index: self.view.selected,
                });
            }
            Screen::Subscriptions => {
                if let Some(channel_id) = self.active_subscription_channel_id.as_ref()
                    && let Some(items) = self.subscription_video_cache.get(channel_id)
                    && video_at(&items.items, self.view.subscriptions.selected_item)
                {
                    return Some(AutoplayOrigin::Subscription {
                        channel_id: channel_id.clone(),
                        index: self.view.subscriptions.selected_item,
                    });
                }
            }
            Screen::Local => {
                if let Some(listing) = self.local_listing.as_ref() {
                    let entries = listing
                        .entries
                        .iter()
                        .filter(|entry| entry.kind.is_playable())
                        .map(|entry| entry.path.clone())
                        .collect::<Vec<_>>();
                    if let Some(index) = entries
                        .iter()
                        .position(|path| local_path_matches_media_id(path, media_id))
                    {
                        return Some(AutoplayOrigin::LocalBrowser {
                            directory: listing.path.clone(),
                            entries: entries.into(),
                            index,
                        });
                    }
                }
            }
            Screen::TrackerMusic => {
                if self
                    .tracker_results
                    .get(self.view.selected)
                    .is_some_and(|item| tracker_item_matches_media_id(item, media_id))
                {
                    return Some(AutoplayOrigin::Tracker {
                        generation: self.search_generation,
                        index: self.view.selected,
                    });
                }
            }
            _ => {}
        }

        if media_id.source == SourceKind::YouTube {
            if let Some(index) = self.youtube_results.iter().position(|item| {
                matches!(item, SearchItem::Video(video) if video.video_id == media_id.external_id)
            }) {
                return Some(AutoplayOrigin::YouTube {
                    generation: self.search_generation,
                    index,
                });
            }
            if let Some(index) = self.youtube_music_results.iter().position(|item| {
                matches!(item, SearchItem::Video(video) if video.video_id == media_id.external_id)
            }) {
                return Some(AutoplayOrigin::YouTubeMusic {
                    generation: self.search_generation,
                    index,
                });
            }
            for channel_id in self.subscription_cache_order.iter().rev() {
                if let Some(items) = self.subscription_video_cache.get(channel_id)
                    && let Some(index) = items.items.iter().position(|item| {
                        matches!(
                            item,
                            SearchItem::Video(video) if video.video_id == media_id.external_id
                        )
                    })
                {
                    return Some(AutoplayOrigin::Subscription {
                        channel_id: channel_id.clone(),
                        index,
                    });
                }
            }
        }
        if media_id.source == SourceKind::Local {
            if let Some(index) = self
                .local_results
                .iter()
                .position(|item| local_path_matches_media_id(&item.path, media_id))
            {
                return Some(AutoplayOrigin::LocalScan {
                    generation: self.search_generation,
                    index,
                });
            }
        }
        if media_id.source == SourceKind::ModArchive
            && let Some(index) = self
                .tracker_results
                .iter()
                .position(|item| tracker_item_matches_media_id(item, media_id))
        {
            return Some(AutoplayOrigin::Tracker {
                generation: self.search_generation,
                index,
            });
        }
        None
    }

    /// Chooses the next playable entry without changing queue or player state.
    fn next_autoplay_step(&self, origin: &AutoplayOrigin) -> AutoplayStep {
        match origin {
            AutoplayOrigin::YouTube { generation, index } => {
                if *generation != self.search_generation {
                    return AutoplayStep::SourceChanged;
                }
                self.youtube_results
                    .iter()
                    .enumerate()
                    .skip(index.saturating_add(1))
                    .find_map(|(index, item)| match item {
                        SearchItem::Video(video) if video_is_autoplay_playable(video) => {
                            Some(AutoplayStep::Play {
                                item: Box::new(queue_item_from_video(video, None)),
                                origin: AutoplayOrigin::YouTube {
                                    generation: *generation,
                                    index,
                                },
                            })
                        }
                        _ => None,
                    })
                    .unwrap_or(AutoplayStep::Exhausted)
            }
            AutoplayOrigin::YouTubeMusic { generation, index } => {
                if *generation != self.search_generation {
                    return AutoplayStep::SourceChanged;
                }
                self.youtube_music_results
                    .iter()
                    .enumerate()
                    .skip(index.saturating_add(1))
                    .find_map(|(index, item)| match item {
                        SearchItem::Video(video) if video_is_autoplay_playable(video) => {
                            Some(AutoplayStep::Play {
                                item: Box::new(queue_item_from_video(video, None)),
                                origin: AutoplayOrigin::YouTubeMusic {
                                    generation: *generation,
                                    index,
                                },
                            })
                        }
                        _ => None,
                    })
                    .unwrap_or(AutoplayStep::Exhausted)
            }
            AutoplayOrigin::Subscription { channel_id, index } => self
                .subscription_video_cache
                .get(channel_id)
                .and_then(|cached| {
                    cached
                        .items
                        .iter()
                        .enumerate()
                        .skip(index.saturating_add(1))
                        .find_map(|(index, item)| match item {
                            SearchItem::Video(video) if video_is_autoplay_playable(video) => {
                                Some(AutoplayStep::Play {
                                    item: Box::new(queue_item_from_video(video, None)),
                                    origin: AutoplayOrigin::Subscription {
                                        channel_id: channel_id.clone(),
                                        index,
                                    },
                                })
                            }
                            _ => None,
                        })
                })
                .unwrap_or(AutoplayStep::Exhausted),
            AutoplayOrigin::LocalBrowser {
                directory,
                entries,
                index,
            } => entries
                .iter()
                .enumerate()
                .skip(index.saturating_add(1))
                .find_map(|(index, path)| {
                    queue_item_from_local(&local_media_item(path.clone()))
                        .ok()
                        .map(|item| AutoplayStep::Play {
                            item: Box::new(item),
                            origin: AutoplayOrigin::LocalBrowser {
                                directory: directory.clone(),
                                entries: Arc::clone(entries),
                                index,
                            },
                        })
                })
                .unwrap_or(AutoplayStep::Exhausted),
            AutoplayOrigin::LocalScan { generation, index } => {
                if *generation != self.search_generation {
                    return AutoplayStep::SourceChanged;
                }
                self.local_results
                    .iter()
                    .enumerate()
                    .skip(index.saturating_add(1))
                    .find_map(|(index, item)| {
                        queue_item_from_local(item)
                            .ok()
                            .map(|item| AutoplayStep::Play {
                                item: Box::new(item),
                                origin: AutoplayOrigin::LocalScan {
                                    generation: *generation,
                                    index,
                                },
                            })
                    })
                    .unwrap_or(AutoplayStep::Exhausted)
            }
            AutoplayOrigin::Tracker { generation, index } => {
                if *generation != self.search_generation {
                    return AutoplayStep::SourceChanged;
                }
                for (index, item) in self
                    .tracker_results
                    .iter()
                    .enumerate()
                    .skip(index.saturating_add(1))
                {
                    if let Ok(item) = queue_item_from_tracker(item) {
                        return AutoplayStep::Play {
                            item: Box::new(item),
                            origin: AutoplayOrigin::Tracker {
                                generation: *generation,
                                index,
                            },
                        };
                    }
                    #[cfg(feature = "tracker-music")]
                    if item.download_url.is_some()
                        && matches!(
                            item.access,
                            TrackerAccess::DirectModule | TrackerAccess::ArchiveNeedsInspection
                        )
                    {
                        return AutoplayStep::PrepareTracker {
                            index,
                            origin: AutoplayOrigin::Tracker {
                                generation: *generation,
                                index,
                            },
                        };
                    }
                }
                AutoplayStep::Exhausted
            }
        }
    }

    /// Continues an exhausted explicit queue from one same-source position.
    fn continue_autoplay(&mut self, origin: AutoplayOrigin) {
        match self.next_autoplay_step(&origin) {
            AutoplayStep::Play { item, origin } => {
                self.play_queue_item_with_origin(*item, false, Some(origin));
            }
            #[cfg(feature = "tracker-music")]
            AutoplayStep::PrepareTracker { index, origin } => {
                self.prepare_tracker_item(index, Some(origin));
            }
            AutoplayStep::SourceChanged => {
                self.view.status_line =
                    "Autoplay stopped because the source list changed".to_owned();
            }
            AutoplayStep::Exhausted => {
                self.view.status_line = "Autoplay reached the end of this list".to_owned();
            }
        }
    }

    fn play_queue_item(&mut self, item: QueueItem, queue_cursor_already_positioned: bool) {
        let origin = self
            .config
            .playback
            .autoplay
            .then(|| self.autoplay_origin_for_media(&item.media.id))
            .flatten();
        self.play_queue_item_with_origin(item, queue_cursor_already_positioned, origin);
    }

    /// Starts one queue item while retaining its same-source continuation
    /// position only after the playback backend accepts the input.
    fn play_queue_item_with_origin(
        &mut self,
        item: QueueItem,
        queue_cursor_already_positioned: bool,
        origin: Option<AutoplayOrigin>,
    ) {
        if !queue_cursor_already_positioned {
            self.queued_autoplay_resume_origin = None;
            #[cfg(feature = "tracker-music")]
            self.cancel_pending_tracker_autoplay();
        }
        if self.player.is_none() {
            let Some(factory) = self.playback_factory.as_mut() else {
                self.view.status_line = "No playback backend was compiled or configured".to_owned();
                return;
            };
            match factory() {
                Ok(mut player) => {
                    if !self.config.playback.audiophile.enabled {
                        let initial_speed = f64::from(self.config.playback.speed_percent) / 100.0;
                        let initialized = player
                            .command(PlayerCommand::SetVolume(
                                self.config.playback.volume_percent,
                            ))
                            .and_then(|()| player.command(PlayerCommand::SetSpeed(initial_speed)));
                        if let Err(error) = initialized {
                            let _ = player.shutdown();
                            self.show_error("Cannot configure playback", &error);
                            return;
                        }
                    }
                    self.player = Some(player);
                }
                Err(error) => {
                    self.show_error("Cannot start playback", &error);
                    return;
                }
            }
        }
        let media_id = item.media.id.clone();
        let media_changed = self.current_media.as_ref() != Some(&media_id);
        if media_changed {
            self.skipped_advertisement_chapter = None;
        }
        let chapters = item.media.chapters.clone();
        let start_at = if let Some(start_at) = item.start_at_seconds {
            start_at
        } else {
            match self.store.progress(&media_id) {
                Ok(progress) => progress.map_or(0, |progress| {
                    progress.resume_position_with_rewind(self.config.playback.resume_rewind_seconds)
                }),
                Err(error) => {
                    self.show_error("Could not restore the playback position", &error);
                    0
                }
            }
        };
        let mut canonical_input = PlaybackInput::new(item.playback_location.clone());
        canonical_input.start_at = Duration::from_secs(start_at);
        canonical_input.title = Some(item.media.title.clone());
        #[cfg(feature = "yt-dlp")]
        let mut input = canonical_input.clone();
        #[cfg(not(feature = "yt-dlp"))]
        let input = canonical_input.clone();
        let mut load_kind = if media_id.source == SourceKind::YouTube {
            PlaybackLoadKind::YouTubeCanonical
        } else {
            PlaybackLoadKind::Regular
        };
        #[cfg(feature = "yt-dlp")]
        if media_id.source == SourceKind::YouTube
            && let Some(prewarmed) = self.take_ready_youtube_prewarm(&media_id)
        {
            let (media_url, headers) = prewarmed.into_playback_parts();
            input.location = media_url.into();
            input.http_headers = headers;
            input.bypass_ytdl = true;
            load_kind = PlaybackLoadKind::YouTubeDirect;
        }
        let had_active_media = self.current_media.is_some();
        let first_result = self
            .player
            .as_mut()
            .expect("player was initialized above")
            .play(&input);
        let play_result = if first_result.is_err() && load_kind == PlaybackLoadKind::YouTubeDirect {
            // A speculative signed URL is an optimization only. Discard its
            // error without surfacing potentially sensitive URL context and
            // immediately use the canonical extractor-backed route.
            load_kind = PlaybackLoadKind::YouTubeCanonical;
            self.player
                .as_mut()
                .expect("player was initialized above")
                .play(&canonical_input)
        } else {
            first_result
        };
        match play_result {
            Ok(()) => {
                if media_changed {
                    self.seek_back.clear();
                }
                if !queue_cursor_already_positioned {
                    self.playback_queue
                        .begin_now(item.clone(), had_active_media);
                }
                self.current_media = Some(media_id.clone());
                self.current_autoplay_origin = origin;
                self.playback_phase = PlaybackPhase::Loading;
                self.playback_load_kind = load_kind;
                self.begin_playback_start_activity();
                self.ignore_replaced_stop = had_active_media;
                self.last_tick = Instant::now();
                let now = unix_time();
                self.pending_history = Some(HistoryEntry {
                    id: 0,
                    media_id,
                    title: item.media.title.clone(),
                    started_at: now,
                    last_played_at: now,
                    position_seconds: start_at,
                    duration_seconds: item.media.duration_seconds,
                    finished: false,
                });
                self.view.playback = PlaybackStatus {
                    idle: false,
                    position: Duration::from_secs(start_at),
                    duration: item.media.duration_seconds.map(Duration::from_secs),
                    paused: true,
                    volume: self.config.playback.volume_percent,
                    speed: f64::from(self.config.playback.speed_percent) / 100.0,
                    buffering: true,
                    title: Some(item.media.title.clone()),
                    ..PlaybackStatus::default()
                };
                self.view.playback_chapters = chapters;
                self.view.status_line = format!("Loading {}…", item.media.title);
                if self.playback_queue.repeat_one
                    && let Some(player) = self.player.as_mut()
                    && let Err(error) = player.command(PlayerCommand::SetRepeat(true))
                {
                    self.show_error("Loading, but repeat mode could not be enabled", &error);
                }
            }
            Err(error) => {
                self.playback_load_kind = PlaybackLoadKind::Regular;
                self.show_error("Playback failed", &error);
            }
        }
    }

    fn player_command(&mut self, command: PlayerCommand) {
        let Some(player) = self.player.as_mut() else {
            self.view.status_line = "Nothing is playing".to_owned();
            return;
        };
        if let Err(error) = player.command(command) {
            if matches!(error, PlaybackError::DirectProfileRestriction(_)) {
                self.view.status_line = error.to_string();
            } else {
                self.show_error("Playback command failed", &error);
            }
        }
    }

    fn update_player(&mut self) {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last_tick);
        self.last_tick = now;

        if self.drain_player_events(elapsed) {
            return;
        }
        let Some(status_result) = self.player.as_mut().map(|player| player.status()) else {
            return;
        };
        if self.drain_player_events(elapsed) {
            return;
        }
        match status_result {
            Ok(mut status) => {
                if self.playback_phase == PlaybackPhase::Idle {
                    self.view.playback = status;
                    return;
                }
                if status.idle {
                    // `idle-active` is not an end reason. In particular, it can
                    // change during yt-dlp loading. Only an `end-file` event may
                    // finish or fail the current queue item.
                    return;
                }
                if self.playback_phase != PlaybackPhase::Playing {
                    status.paused = true;
                    status.buffering = true;
                }
                // Backends may derive a title from a resolved CDN URL. Youta
                // already owns the stable user-facing title and must not
                // replace it with a long, expiring, potentially sensitive URL.
                status.title = Some(self.current_playback_title());
                self.view.playback = status;
                if self.playback_phase == PlaybackPhase::Playing {
                    self.update_live_local_progress_view();
                    self.skip_current_advertisement_chapter();
                    self.account_listen_time(elapsed);
                    if self.last_position_save.elapsed()
                        >= Duration::from_secs(
                            self.config
                                .persistence
                                .position_save_interval_seconds
                                .max(1),
                        )
                    {
                        self.persist_position();
                        self.last_position_save = Instant::now();
                    }
                }
            }
            Err(error) => {
                self.fail_player("Playback status failed", &error, elapsed);
            }
        }
    }

    /// Skips one exact `Реклама` chapter and suppresses duplicate commands
    /// while the backend still reports its stale pre-seek position.
    fn skip_current_advertisement_chapter(&mut self) {
        if !self.config.playback.skip_advertisement_chapters {
            self.skipped_advertisement_chapter = None;
            return;
        }
        let Some(media_id) = self.current_media.clone() else {
            return;
        };
        let position = self.view.playback.position.as_secs();
        let matching = self.view.playback_chapters.iter().find(|chapter| {
            is_advertisement_chapter_title(&chapter.title)
                && chapter.end_seconds.is_some_and(|end| {
                    chapter.start_seconds <= position
                        && position < end
                        && end > chapter.start_seconds
                })
        });
        let Some(chapter) = matching else {
            self.skipped_advertisement_chapter = None;
            return;
        };
        let key = (media_id, chapter.start_seconds);
        if self.skipped_advertisement_chapter.as_ref() == Some(&key) {
            return;
        }
        let Some(end) = chapter.end_seconds else {
            return;
        };
        self.skipped_advertisement_chapter = Some(key);
        self.player_command(PlayerCommand::SeekAbsolute(Duration::from_secs(end)));
        self.view.status_line = format!("Skipped advertisement section to {}", format_seconds(end));
    }

    fn drain_player_events(&mut self, elapsed: Duration) -> bool {
        const MAX_EVENTS_PER_TICK: usize = 64;

        for _ in 0..MAX_EVENTS_PER_TICK {
            let Some(event_result) = self.player.as_mut().map(|player| player.poll_event()) else {
                return true;
            };
            match event_result {
                Ok(Some(PlaybackEvent::MediaLoaded)) => {
                    if self.playback_phase == PlaybackPhase::Loading {
                        self.playback_phase = PlaybackPhase::Loaded;
                        self.ignore_replaced_stop = false;
                        let title = self.current_playback_title();
                        self.view.status_line = format!("Loaded {title}; starting audio…");
                    }
                }
                Ok(Some(PlaybackEvent::PlaybackStarted)) => {
                    if self.playback_phase != PlaybackPhase::Playing {
                        self.playback_phase = PlaybackPhase::Playing;
                        self.playback_load_kind = PlaybackLoadKind::Regular;
                        self.clear_playback_start_activity();
                        self.view.playing_media_id = self.current_media.clone();
                        self.view.playback.idle = false;
                        self.view.playback.paused = false;
                        self.view.playback.buffering = false;
                        let title = self.current_playback_title();
                        self.view.status_line = format!("Playing {title}");
                        if let Some(history) = self.pending_history.take()
                            && let Err(error) = self.store.insert_history(&history)
                        {
                            self.show_error("Playing, but history could not be saved", &error);
                        }
                    }
                }
                Ok(Some(PlaybackEvent::Ended(end))) => {
                    if end.reason == PlaybackEndReason::Stop && self.ignore_replaced_stop {
                        self.ignore_replaced_stop = false;
                        continue;
                    }
                    self.handle_playback_end(end, elapsed);
                    return true;
                }
                Ok(Some(PlaybackEvent::ProcessExited { diagnostic })) => {
                    let message = diagnostic
                        .unwrap_or_else(|| "mpv exited without diagnostic output".to_owned());
                    self.prepare_to_clear_playback(elapsed);
                    self.reset_playback_state();
                    if let Some(mut player) = self.player.take() {
                        let _ = player.shutdown();
                    }
                    self.show_error_message("Playback process stopped", message);
                    return true;
                }
                Ok(None) => return false,
                Err(error) => {
                    self.fail_player("Playback event polling failed", &error, elapsed);
                    return true;
                }
            }
        }

        self.prepare_to_clear_playback(elapsed);
        self.reset_playback_state();
        if let Some(mut player) = self.player.take() {
            let _ = player.shutdown();
        }
        self.show_error_message(
            "Playback event polling failed",
            "the playback backend emitted too many lifecycle events in one UI tick",
        );
        true
    }

    fn handle_playback_end(&mut self, end: PlaybackEnd, elapsed: Duration) {
        if self.retry_youtube_load(&end) {
            return;
        }
        let playback_started = self.playback_phase == PlaybackPhase::Playing;
        let tracker_module = self
            .current_media
            .as_ref()
            .is_some_and(|media| media.source == SourceKind::ModArchive);
        self.prepare_to_clear_playback(elapsed);
        match end.reason.clone() {
            PlaybackEndReason::Eof if !playback_started => {
                let message = playback_before_start_message_for_media(&end, tracker_module);
                self.queued_autoplay_resume_origin = None;
                #[cfg(feature = "tracker-music")]
                self.cancel_pending_tracker_autoplay();
                self.reset_playback_state();
                self.show_error_message("Playback did not start", message);
            }
            PlaybackEndReason::Eof => {
                if let Some(duration) = self.view.playback.duration {
                    self.view.playback.position = duration;
                    self.persist_position();
                }
                let autoplay_origin = self.current_autoplay_origin.clone();
                self.reset_playback_state();
                match self.playback_queue.advance().cloned() {
                    Some(next) => {
                        if self.config.playback.autoplay
                            && self.queued_autoplay_resume_origin.is_none()
                            && autoplay_origin.is_some()
                        {
                            self.queued_autoplay_resume_origin = autoplay_origin;
                        }
                        self.play_queue_item_with_origin(next, true, None);
                    }
                    None if self.config.playback.autoplay => {
                        let origin = self
                            .queued_autoplay_resume_origin
                            .take()
                            .or(autoplay_origin);
                        if let Some(origin) = origin {
                            self.continue_autoplay(origin);
                        } else {
                            self.view.status_line =
                                "Playback queue finished; this item has no autoplay list"
                                    .to_owned();
                        }
                    }
                    None => {
                        self.queued_autoplay_resume_origin = None;
                        self.view.status_line = "Playback queue finished".to_owned();
                    }
                }
            }
            PlaybackEndReason::Stop => {
                self.queued_autoplay_resume_origin = None;
                #[cfg(feature = "tracker-music")]
                self.cancel_pending_tracker_autoplay();
                self.reset_playback_state();
                self.view.status_line = "Playback stopped".to_owned();
            }
            PlaybackEndReason::Error => {
                let message = if tracker_module && playback_end_reports_unsupported_format(&end) {
                    tracker_decoder_help(&playback_end_message(&end))
                } else {
                    playback_end_message(&end)
                };
                self.queued_autoplay_resume_origin = None;
                #[cfg(feature = "tracker-music")]
                self.cancel_pending_tracker_autoplay();
                self.reset_playback_state();
                self.show_error_message("Playback failed", message);
            }
            PlaybackEndReason::Other(reason) => {
                let message = format!(
                    "the playback backend ended the media for an unexpected reason: {reason}"
                );
                self.queued_autoplay_resume_origin = None;
                #[cfg(feature = "tracker-music")]
                self.cancel_pending_tracker_autoplay();
                self.reset_playback_state();
                self.show_error_message("Playback ended unexpectedly", message);
            }
        }
    }

    /// Advances one pre-start `YouTube` load through its bounded fallback chain.
    ///
    /// A speculative direct stream may fail for any reason before audible
    /// playback begins and falls back to the canonical watch URL. This remains
    /// true when `mpv` emitted [`PlaybackEvent::MediaLoaded`] before discovering
    /// that the resolved payload cannot be demuxed. Only an HTTP 403 from a
    /// still-loading canonical attempt enables the slower checked-format route.
    /// Once [`PlaybackEvent::PlaybackStarted`] arrives, no automatic
    /// replacement is attempted.
    fn retry_youtube_load(&mut self, end: &PlaybackEnd) -> bool {
        if !matches!(
            end.reason,
            PlaybackEndReason::Eof | PlaybackEndReason::Error
        ) {
            return false;
        }
        let next_kind = match self.playback_load_kind {
            PlaybackLoadKind::YouTubeDirect
                if matches!(
                    self.playback_phase,
                    PlaybackPhase::Loading | PlaybackPhase::Loaded
                ) =>
            {
                PlaybackLoadKind::YouTubeCanonical
            }
            PlaybackLoadKind::YouTubeCanonical
                if self.playback_phase == PlaybackPhase::Loading
                    && playback_end_reports_http_403(end) =>
            {
                PlaybackLoadKind::YouTubeChecked
            }
            PlaybackLoadKind::Regular
            | PlaybackLoadKind::YouTubeDirect
            | PlaybackLoadKind::YouTubeCanonical
            | PlaybackLoadKind::YouTubeChecked => return false,
        };
        let Some(media_id) = self.current_media.as_ref() else {
            return false;
        };
        if media_id.source != SourceKind::YouTube {
            return false;
        }
        let Some(item) = self.playback_queue.current().cloned() else {
            return false;
        };
        if &item.media.id != media_id {
            return false;
        }

        let title = item.media.title.clone();
        let start_at = self
            .pending_history
            .as_ref()
            .map_or(self.view.playback.position.as_secs(), |history| {
                history.position_seconds
            });
        let mut input = PlaybackInput::new(item.playback_location);
        input.start_at = Duration::from_secs(start_at);
        input.title = Some(title.clone());
        input.verify_remote_format = next_kind == PlaybackLoadKind::YouTubeChecked;
        let result = self
            .player
            .as_mut()
            .expect("an active load always owns a player")
            .play(&input);
        match result {
            Ok(()) => {
                self.playback_load_kind = next_kind;
                self.playback_phase = PlaybackPhase::Loading;
                self.begin_playback_start_activity();
                self.ignore_replaced_stop = true;
                self.last_tick = Instant::now();
                self.view.playback.idle = false;
                self.view.playback.position = Duration::from_secs(start_at);
                self.view.playback.paused = true;
                self.view.playback.buffering = true;
                self.view.playback.buffered_ranges.clear();
                self.view.status_line = match next_kind {
                    PlaybackLoadKind::YouTubeCanonical => {
                        format!("Retrying {title} through YouTube…")
                    }
                    PlaybackLoadKind::YouTubeChecked => {
                        format!("Retrying {title} after validating alternate YouTube formats…")
                    }
                    PlaybackLoadKind::Regular | PlaybackLoadKind::YouTubeDirect => {
                        unreachable!("the fallback target is canonical or checked")
                    }
                };
            }
            Err(error) => {
                self.queued_autoplay_resume_origin = None;
                #[cfg(feature = "tracker-music")]
                self.cancel_pending_tracker_autoplay();
                self.reset_playback_state();
                self.show_error("Playback failed", &error);
            }
        }
        true
    }

    fn current_playback_title(&self) -> String {
        self.pending_history
            .as_ref()
            .map(|history| history.title.clone())
            .or_else(|| self.view.playback.title.clone())
            .unwrap_or_else(|| "media".to_owned())
    }

    fn prepare_to_clear_playback(&mut self, elapsed: Duration) {
        if self.playback_phase == PlaybackPhase::Playing {
            self.account_listen_time(elapsed);
            self.persist_position();
            self.flush_listen_time();
        }
    }

    fn reset_playback_state(&mut self) {
        self.playback_phase = PlaybackPhase::Idle;
        self.playback_load_kind = PlaybackLoadKind::Regular;
        self.clear_playback_start_activity();
        self.view.playing_media_id = None;
        self.pending_history = None;
        self.ignore_replaced_stop = false;
        self.current_media = None;
        self.current_autoplay_origin = None;
        self.seek_back.clear();
        self.view.playback_chapters.clear();
        self.view.playback = PlaybackStatus {
            volume: self.config.playback.volume_percent,
            speed: f64::from(self.config.playback.speed_percent) / 100.0,
            ..PlaybackStatus::default()
        };
    }

    fn fail_player<E>(&mut self, title: &str, error: &E, elapsed: Duration)
    where
        E: std::error::Error + 'static,
    {
        self.prepare_to_clear_playback(elapsed);
        self.reset_playback_state();
        if let Some(mut player) = self.player.take() {
            let _ = player.shutdown();
        }
        self.show_error(title, error);
    }

    fn account_listen_time(&mut self, elapsed: Duration) {
        if self.playback_phase == PlaybackPhase::Playing
            && self.current_media.is_some()
            && !self.view.playback.paused
        {
            self.unflushed_listen_time += elapsed;
        }
        if self.unflushed_listen_time >= Duration::from_secs(30) {
            self.flush_listen_time();
        }
    }

    fn flush_listen_time(&mut self) {
        let seconds = self.unflushed_listen_time.as_secs();
        if seconds == 0 {
            return;
        }
        if let Some(media) = &self.current_media {
            if let Err(error) = self.store.add_listen_seconds(&media.source, seconds) {
                self.show_error("Could not save listening statistics", &error);
                return;
            }
        }
        self.unflushed_listen_time -= Duration::from_secs(seconds);
    }

    fn persist_position(&mut self) {
        let Some(media_id) = self.current_media.clone() else {
            return;
        };
        let duration = self.view.playback.duration.map(|value| value.as_secs());
        let now = unix_time();
        let progress = self.store.progress(&media_id);
        let mut progress = match progress {
            Ok(progress) => {
                progress.unwrap_or_else(|| PlaybackProgress::new(media_id, duration, now))
            }
            Err(error) => {
                self.show_error("Could not load the saved playback position", &error);
                PlaybackProgress::new(media_id, duration, now)
            }
        };
        progress.duration_seconds = duration.or(progress.duration_seconds);
        progress.record_position(self.view.playback.position.as_secs(), now);
        match self.store.upsert_progress(&progress) {
            Ok(()) => {
                self.update_local_progress_view(&progress.media_id, progress.watched_percent());
            }
            Err(error) => self.show_error("Could not save playback position", &error),
        }
    }

    fn show_screen(&mut self, screen: Screen) {
        self.clear_detail_navigation_history();
        #[cfg(feature = "yt-dlp")]
        self.cancel_youtube_prewarm();
        if self.view.screen == Screen::Local && screen != Screen::Local {
            self.invalidate_local_folder_sizes();
        }
        match self.view.screen {
            Screen::Search => {
                self.youtube_search_query
                    .clone_from(&self.view.search_query);
                self.youtube_selected = self.view.selected;
            }
            Screen::YouTubeMusic => {
                self.youtube_music_search_query
                    .clone_from(&self.view.search_query);
                self.youtube_music_selected = self.view.selected;
            }
            Screen::TrackerMusic => {
                self.tracker_search_query
                    .clone_from(&self.view.search_query);
                self.tracker_selected = self.view.selected;
            }
            _ => {}
        }
        self.details_generation = self.details_generation.wrapping_add(1);
        #[cfg(feature = "wikidata")]
        self.invalidate_wikidata_lookup();
        self.channel_details_generation = self.channel_details_generation.wrapping_add(1);
        self.scheduled_channel_details = None;
        self.view.screen = screen;
        if self.view.right_panel_mode == RightPanelMode::Channel {
            self.view.right_panel_mode = RightPanelMode::Details;
        }
        match screen {
            Screen::Search => {
                self.view
                    .search_query
                    .clone_from(&self.youtube_search_query);
                self.view.selected = self.youtube_selected;
            }
            Screen::YouTubeMusic => {
                self.view
                    .search_query
                    .clone_from(&self.youtube_music_search_query);
                self.view.selected = self.youtube_music_selected;
            }
            Screen::TrackerMusic => {
                self.view
                    .search_query
                    .clone_from(&self.tracker_search_query);
                self.view.selected = self.tracker_selected;
            }
            _ => self.view.selected = 0,
        }
        if screen == Screen::Subscriptions {
            self.view.subscriptions.selected_source = 0;
            self.view.subscriptions.selected_item = 0;
            self.view.subscriptions.route = SubscriptionRoute::Sources;
            self.view.subscriptions.focus = SubscriptionPane::Sources;
            self.view.subscriptions.description_expanded = false;
        }
        if screen != Screen::Subscriptions {
            let cancelled_refresh = self.pending_subscription_refresh.take().is_some();
            self.active_subscription_channel_id = None;
            if cancelled_refresh {
                // A deliberate refresh must not replace the cache after its
                // channel context is abandoned. Ordinary initial loads may
                // still populate that channel's cache in the background.
                self.subscription_generation = self.subscription_generation.wrapping_add(1);
            }
            self.view.subscriptions.loading = false;
        }
        self.view.details = None;
        self.view.details_focused = false;
        self.view.details_scroll = 0;
        self.populate_local_screen();
        if screen == Screen::Search && !self.youtube_results.is_empty() {
            self.request_visible_channel_subscriber_counts();
            self.request_selected_details();
        } else if screen == Screen::YouTubeMusic && !self.youtube_music_results.is_empty() {
            self.request_selected_details();
        }
    }

    fn populate_local_screen(&mut self) {
        match self.view.screen {
            Screen::Search => {
                if !self.local_results.is_empty() {
                    self.refresh_local_rows();
                } else if let Some(media) = self.resolved_direct.clone() {
                    self.view.rows = vec![RowView {
                        media_id: Some(MediaId::new(
                            media.source.clone(),
                            media.external_id.clone(),
                        )),
                        title: media.title,
                        subtitle: media.duration_seconds.map_or_else(
                            || "podcast".to_owned(),
                            |duration| format!("episode · {}", format_seconds(duration)),
                        ),
                        source: media.source.to_string(),
                        ..RowView::default()
                    }];
                } else if let Some(direct) = &self.direct_item {
                    self.view.rows = vec![RowView {
                        media_id: Some(MediaId::new(direct.source.clone(), direct.url.to_string())),
                        title: direct.url.to_string(),
                        subtitle: "direct link".to_owned(),
                        source: direct.source.to_string(),
                        ..RowView::default()
                    }];
                } else {
                    self.refresh_youtube_rows();
                }
                self.view.status_line = "Default search: YouTube videos only".to_owned();
            }
            Screen::YouTubeMusic => {
                self.refresh_youtube_music_rows();
                self.view.status_line = if self.youtube_music_results.is_empty() {
                    "YouTube Music uses yt-dlp and does not require a YouTube API key; press / to search"
                        .to_owned()
                } else {
                    format!(
                        "{} YouTube Music track{}",
                        self.youtube_music_results.len(),
                        if self.youtube_music_results.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    )
                };
            }
            Screen::TrackerMusic => {
                self.refresh_tracker_rows();
                self.view.status_line =
                    "MOD/tracker search uses its own archive sources; press / to search".to_owned();
            }
            Screen::Local => {
                if self.local_listing.is_some() {
                    self.sort_local_listing();
                    self.refresh_local_browser_rows();
                    self.schedule_local_folder_sizes();
                    self.view.status_line = format!("Local folder: {}", self.view.local_path);
                } else {
                    let restored = PathBuf::from(&self.view.local_path);
                    let directory = if !self.view.local_path.is_empty() && restored.is_dir() {
                        restored
                    } else {
                        default_local_browse_root()
                    };
                    self.browse_local_directory(directory);
                }
            }
            Screen::Subscriptions => self.populate_subscriptions(),
            Screen::Downloaded => self.populate_downloads(),
            Screen::History => self.populate_history(),
            Screen::Playlists => {
                self.view.rows.clear();
                self.view.status_line = "No local playlists have been created yet".to_owned();
            }
            Screen::Statistics => self.populate_statistics(),
        }
    }

    fn populate_subscriptions(&mut self) {
        #[cfg(feature = "yt-dlp")]
        self.cancel_youtube_prewarm();
        match subscriptions::load(&self.config) {
            Ok(tree) => self.subscription_tree = tree,
            Err(error) => {
                self.show_error("Cannot load subscriptions", &error);
                return;
            }
        }
        self.subscription_entries = self.subscription_tree.flattened_subscriptions();
        self.view.subscriptions.sources = self
            .subscription_entries
            .iter()
            .map(subscription_source_row)
            .collect();
        self.restore_cached_subscription_source_artwork();
        self.view.subscriptions.selected_source = self
            .view
            .subscriptions
            .selected_source
            .min(self.subscription_entries.len().saturating_sub(1));
        self.view.subscriptions.route = SubscriptionRoute::Sources;
        self.view.subscriptions.focus = SubscriptionPane::Sources;
        self.view.subscriptions.description_expanded = false;
        self.view.subscriptions.loading = false;
        self.view.subscriptions.items.clear();
        self.view.subscriptions.selected_item = 0;
        self.active_subscription_channel_id = None;
        self.pending_subscription_refresh = None;
        self.subscription_generation = self.subscription_generation.wrapping_add(1);
        self.channel_details_generation = self.channel_details_generation.wrapping_add(1);
        self.scheduled_channel_details = None;
        self.update_selected_subscription_source();
        self.view.status_line = format!(
            "{} local subscription(s)",
            self.subscription_tree.subscription_count()
        );
        if self.view.subscriptions.layout == SubscriptionsLayout::Split
            && !self.subscription_entries.is_empty()
        {
            self.show_cached_selected_subscription_videos();
        }
    }

    /// Restores known channel-artwork URLs for every subscription source.
    ///
    /// Reading compact local SQLite rows here lets the thumbnail worker warm
    /// image bytes before keyboard or mouse navigation selects another
    /// channel. Provider requests remain lazy and independently debounced.
    fn restore_cached_subscription_source_artwork(&mut self) {
        let channel_rows = self
            .subscription_entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                entry
                    .subscription
                    .youtube_channel_id()
                    .map(|channel_id| (index, channel_id))
            })
            .collect::<Vec<_>>();
        for (index, channel_id) in channel_rows {
            let cached = match self.store.cached_channel_summary(&channel_id) {
                Ok(cached) => cached,
                Err(error) => {
                    self.show_error("Could not read cached channel artwork", &error);
                    return;
                }
            };
            let Some(cached) = cached else {
                continue;
            };
            if let Some(row) = self.view.subscriptions.sources.get_mut(index) {
                row.thumbnail_url = preferred_thumbnail_url(&cached.summary.thumbnails);
            }
        }
    }

    fn update_selected_subscription_source(&mut self) {
        let Some(entry) = self
            .subscription_entries
            .get(self.view.subscriptions.selected_source)
            .cloned()
        else {
            self.view.details = None;
            self.view.subscriptions.source_title.clear();
            self.view.subscriptions.source_subscriber_count = None;
            self.view.subscriptions.source_created.clear();
            return;
        };
        let subscription = entry.subscription;
        let channel_id = subscription.youtube_channel_id().unwrap_or_default();
        let channel_webpage_url = (subscription.kind == SubscriptionKind::YouTube)
            .then(|| youtube_channel_webpage_url(&channel_id, subscription.website_url.clone()))
            .flatten();
        self.view.subscriptions.source_title = subscription.title.clone();
        let cached_subscribers = self
            .channel_subscriber_cache
            .get(&channel_id)
            .copied()
            .flatten();
        self.view.subscriptions.source_subscriber_count = cached_subscribers;
        self.view.subscriptions.source_created.clear();
        self.view.details = Some(DetailView {
            title: subscription.title.clone(),
            source: subscription_kind_label(subscription.kind).to_owned(),
            channel_name: subscription.title,
            channel_id: channel_id.clone(),
            channel_webpage_url,
            channel_subscribed: true,
            channel_subscriber_count: cached_subscribers,
            description: subscription
                .description
                .unwrap_or_else(|| "Loading channel description…".to_owned()),
            ..DetailView::default()
        });
        self.view.right_panel_mode = RightPanelMode::Channel;
        self.view.details_scroll = 0;
        self.schedule_selected_subscription_channel_details(Instant::now());
    }

    fn select_subscription_source(&mut self, index: usize) {
        if index >= self.subscription_entries.len() {
            return;
        }
        #[cfg(feature = "yt-dlp")]
        self.cancel_youtube_prewarm();
        self.view.subscriptions.selected_source = index;
        self.view.subscriptions.selected_item = 0;
        self.view.subscriptions.description_expanded = false;
        self.view.subscriptions.focus = SubscriptionPane::Sources;
        self.view.subscriptions.loading = false;
        self.view.subscriptions.items.clear();
        self.active_subscription_channel_id = None;
        self.pending_subscription_refresh = None;
        self.subscription_generation = self.subscription_generation.wrapping_add(1);
        self.channel_details_generation = self.channel_details_generation.wrapping_add(1);
        self.scheduled_channel_details = None;
        self.update_selected_subscription_source();
        if self.view.subscriptions.layout == SubscriptionsLayout::Split {
            self.show_cached_selected_subscription_videos();
        }
    }

    /// Shows cached split-view rows without spending quota while sources move.
    ///
    /// An uncached source remains explicit: Enter starts its provider request.
    fn show_cached_selected_subscription_videos(&mut self) {
        let Some(entry) = self
            .subscription_entries
            .get(self.view.subscriptions.selected_source)
        else {
            return;
        };
        if entry.subscription.kind != SubscriptionKind::YouTube {
            self.view.status_line =
                "This source is retained in OPML; its episode list is not implemented yet"
                    .to_owned();
            return;
        }
        let Some(channel_id) = entry.subscription.youtube_channel_id() else {
            self.view.status_line =
                "This imported YouTube source has no exact channel ID to load".to_owned();
            return;
        };
        self.active_subscription_channel_id = Some(channel_id.clone());
        self.restore_subscription_video_snapshot(&channel_id);
        if self.subscription_video_cache.contains_key(&channel_id) {
            self.touch_subscription_cache(&channel_id);
            self.refresh_subscription_video_rows();
            self.view.status_line = format!(
                "{} cached video{} for {}",
                self.view.subscriptions.items.len(),
                if self.view.subscriptions.items.len() == 1 {
                    ""
                } else {
                    "s"
                },
                self.view.subscriptions.source_title
            );
        } else {
            self.view.status_line = format!(
                "Press Enter to load videos for {}",
                self.view.subscriptions.source_title
            );
        }
    }

    fn select_subscription_item(&mut self, index: usize) {
        if index >= self.view.subscriptions.items.len() {
            return;
        }
        self.view.subscriptions.selected_item = index;
        let selected_video_id = self
            .selected_subscription_item()
            .and_then(|item| match item {
                SearchItem::Video(video) => Some(video.video_id.clone()),
                SearchItem::Channel(_) => None,
            });
        if let Some(pending) = self.pending_subscription_refresh.as_mut()
            && self.active_subscription_channel_id.as_deref() == Some(pending.channel_id.as_str())
        {
            // A deliberate selection made while refresh is in flight takes
            // precedence over the snapshot captured when refresh started.
            pending.selected_video_id = selected_video_id;
            pending.fallback_index = index;
        }
        self.view.subscriptions.focus = SubscriptionPane::Items;
        self.view.right_panel_mode = RightPanelMode::Details;
        self.request_selected_details();
        self.load_next_subscription_page_if_needed();
    }

    fn load_selected_subscription_videos(&mut self) {
        let Some(entry) = self
            .subscription_entries
            .get(self.view.subscriptions.selected_source)
        else {
            return;
        };
        if entry.subscription.kind != SubscriptionKind::YouTube {
            self.view.subscriptions.items.clear();
            self.active_subscription_channel_id = None;
            self.view.status_line =
                "This source is retained in OPML; its episode list is not implemented yet"
                    .to_owned();
            return;
        }
        let Some(channel_id) = entry.subscription.youtube_channel_id() else {
            self.view.subscriptions.items.clear();
            self.active_subscription_channel_id = None;
            self.view.status_line =
                "This imported YouTube source has no exact channel ID to load".to_owned();
            return;
        };
        self.active_subscription_channel_id = Some(channel_id.clone());
        self.restore_subscription_video_snapshot(&channel_id);
        if self.subscription_video_cache.contains_key(&channel_id) {
            self.touch_subscription_cache(&channel_id);
            self.refresh_subscription_video_rows();
            if !self.view.subscriptions.items.is_empty()
                && (self.view.subscriptions.layout == SubscriptionsLayout::DrillDown
                    || self.view.subscriptions.focus == SubscriptionPane::Items)
            {
                self.request_selected_details();
            }
            self.refresh_selected_subscription_videos();
            return;
        }
        self.request_subscription_videos(channel_id, 1);
    }

    /// Hydrates one compact first-page channel snapshot before network refresh.
    ///
    /// Invalid or unavailable disk rows are reported through the normal
    /// recoverable-error popup. A process-local cache always wins because it
    /// may already contain later pages loaded during this session.
    fn restore_subscription_video_snapshot(&mut self, channel_id: &str) {
        if self.subscription_video_cache.contains_key(channel_id) {
            return;
        }
        let snapshot = match self
            .store
            .cached_subscription_items(&SourceKind::YouTube, channel_id)
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.show_error("Could not read cached subscription videos", &error);
                return;
            }
        };
        let Some(mut snapshot) = snapshot else {
            return;
        };
        for item in &mut snapshot.items {
            if let SearchItem::Video(video) = item {
                // Direct provider streams can expire between processes and may
                // contain signed query material. Only the canonical page is
                // restart-safe; lazy details obtains a current stream.
                video.stream_url = None;
            }
        }
        self.subscription_video_cache.insert(
            channel_id.to_owned(),
            CachedSubscriptionVideos {
                items: snapshot.items,
                next_page: None,
                consecutive_empty_pages: 0,
            },
        );
        self.touch_subscription_cache(channel_id);
        self.enforce_subscription_cache_byte_budget(channel_id);
    }

    /// Replaces the restart snapshot with the current compact first page.
    ///
    /// Later in-memory pages remain process-local. This keeps restart cost and
    /// database growth bounded while page-one refreshes still reconcile
    /// episodes removed by the provider.
    fn persist_subscription_video_snapshot(
        &self,
        channel_id: &str,
    ) -> Result<(), crate::persistence::PersistenceError> {
        let mut items: Vec<SearchItem> = self
            .subscription_video_cache
            .get(channel_id)
            .map_or(&[][..], |cached| cached.items.as_slice())
            .iter()
            .take(MAX_SAVED_SUBSCRIPTION_ITEMS)
            .cloned()
            .collect();
        for item in &mut items {
            if let SearchItem::Video(video) = item {
                video.stream_url = None;
            }
        }
        self.store
            .put_cached_subscription_items(&CachedSubscriptionItems {
                source: SourceKind::YouTube,
                source_id: channel_id.to_owned(),
                items,
                fetched_at: unix_time(),
            })
    }

    /// Queues one guarded sequential channel page and marks the pane loading.
    fn request_subscription_videos(&mut self, channel_id: String, page: u32) {
        if !self.youtube_provider_available {
            self.open_youtube_setup();
            return;
        }
        if self.view.subscriptions.loading {
            return;
        }
        self.subscription_generation = self.subscription_generation.wrapping_add(1);
        let request = ChannelVideosRequest { channel_id, page };
        if !self.send_provider_request(
            ProviderRequest::ChannelVideos {
                generation: self.subscription_generation,
                request,
            },
            "Could not load subscription videos",
        ) {
            return;
        }
        self.view.subscriptions.loading = true;
        self.view.status_line = format!(
            "Loading videos for {}…",
            self.view.subscriptions.source_title
        );
    }

    /// Reloads page one for the active channel without blanking cached rows.
    ///
    /// The current video identity remains visible while the provider works and
    /// is restored after replacement when the refreshed page still contains
    /// it. A second request is ignored until the first one completes.
    fn refresh_selected_subscription_videos(&mut self) {
        if self.view.screen != Screen::Subscriptions {
            self.view.status_line =
                "Subscription videos can be refreshed only inside a channel".to_owned();
            return;
        }
        if self.view.subscriptions.loading {
            self.view.status_line = "Subscription videos are already loading".to_owned();
            return;
        }
        let Some(channel_id) = self.active_subscription_channel_id.clone() else {
            self.view.status_line = "Open a subscribed channel before refreshing videos".to_owned();
            return;
        };
        let selected_video_id = self
            .selected_subscription_item()
            .and_then(|item| match item {
                SearchItem::Video(video) => Some(video.video_id.clone()),
                SearchItem::Channel(_) => None,
            });
        self.pending_subscription_refresh = Some(PendingSubscriptionRefresh {
            channel_id: channel_id.clone(),
            selected_video_id,
            fallback_index: self.view.subscriptions.selected_item,
            staged_cache: None,
        });
        self.request_subscription_videos(channel_id, 1);
        if self.view.subscriptions.loading {
            self.view.status_line = format!(
                "Refreshing videos for {}…",
                self.view.subscriptions.source_title
            );
        } else {
            self.pending_subscription_refresh = None;
        }
    }

    /// Restores the pre-refresh selection after page one replaces the cache.
    fn restore_subscription_refresh_selection(&mut self, channel_id: &str) {
        let Some(pending) = self
            .pending_subscription_refresh
            .take()
            .filter(|pending| pending.channel_id == channel_id)
        else {
            return;
        };
        let refreshed = self
            .subscription_video_cache
            .get(channel_id)
            .map_or(&[][..], |cached| cached.items.as_slice());
        let selected = pending
            .selected_video_id
            .as_deref()
            .and_then(|video_id| {
                refreshed.iter().position(|item| {
                    matches!(
                        item,
                        SearchItem::Video(video) if video.video_id == video_id
                    )
                })
            })
            .unwrap_or_else(|| {
                pending
                    .fallback_index
                    .min(refreshed.len().saturating_sub(1))
            });
        self.view.subscriptions.selected_item = selected;
    }

    /// Loads the next page near the visible end or after empty-list Enter.
    fn load_next_subscription_page_if_needed(&mut self) {
        if self.view.subscriptions.loading
            || self.view.subscriptions.selected_item.saturating_add(2)
                < self.view.subscriptions.items.len()
        {
            return;
        }
        let Some(channel_id) = self.active_subscription_channel_id.clone() else {
            return;
        };
        let next_page = self
            .subscription_video_cache
            .get(&channel_id)
            .and_then(|cached| cached.next_page);
        if let Some(page) = next_page {
            self.request_subscription_videos(channel_id, page);
        }
    }

    /// Replaces page one or appends a later page within the per-channel cap.
    ///
    /// Touching the separate order deque maintains bounded LRU eviction across
    /// channels. Empty pages retain their continuation because private videos
    /// can otherwise hide a later playable page.
    fn cache_subscription_video_page(&mut self, channel_id: &str, page: SearchPage) {
        if page.page == 1 && !self.subscription_video_cache.contains_key(channel_id) {
            while self.subscription_video_cache.len() >= MAX_CACHED_SUBSCRIPTION_CHANNELS {
                let Some(oldest) = self.subscription_cache_order.pop_front() else {
                    break;
                };
                if oldest != channel_id {
                    self.subscription_video_cache.remove(&oldest);
                }
            }
        }
        let cached = self
            .subscription_video_cache
            .entry(channel_id.to_owned())
            .or_default();
        Self::apply_subscription_video_page(cached, page);
        self.touch_subscription_cache(channel_id);
        self.enforce_subscription_cache_byte_budget(channel_id);
    }

    /// Applies one validated sequential page to a bounded cache value.
    fn apply_subscription_video_page(cached: &mut CachedSubscriptionVideos, mut page: SearchPage) {
        if page.page == 1 {
            cached.items.clear();
            cached.consecutive_empty_pages = 0;
        }
        for item in &mut page.items {
            compact_subscription_item(item);
        }
        let remaining =
            MAX_CACHED_SUBSCRIPTION_VIDEOS_PER_CHANNEL.saturating_sub(cached.items.len());
        let page_is_empty = page.items.is_empty();
        cached.items.extend(page.items.into_iter().take(remaining));
        cached.consecutive_empty_pages = if page_is_empty {
            cached.consecutive_empty_pages.saturating_add(1)
        } else {
            0
        };
        cached.next_page = (cached.items.len() < MAX_CACHED_SUBSCRIPTION_VIDEOS_PER_CHANNEL)
            .then_some(page.next_page)
            .flatten();
    }

    /// Evicts least-recently-used channels until the shared byte budget holds.
    fn enforce_subscription_cache_byte_budget(&mut self, preserved_channel_id: &str) {
        while self.subscription_cache_estimated_heap_bytes() > MAX_CACHED_SUBSCRIPTION_BYTES
            && self.subscription_video_cache.len() > 1
        {
            let Some(eviction_index) = self
                .subscription_cache_order
                .iter()
                .position(|channel_id| channel_id != preserved_channel_id)
            else {
                break;
            };
            let Some(oldest) = self.subscription_cache_order.remove(eviction_index) else {
                break;
            };
            self.subscription_video_cache.remove(&oldest);
        }
    }

    /// Returns the approximate heap bytes owned by all channel summaries.
    fn subscription_cache_estimated_heap_bytes(&self) -> usize {
        self.subscription_video_cache
            .values()
            .map(CachedSubscriptionVideos::estimated_heap_bytes)
            .fold(0usize, usize::saturating_add)
    }

    fn touch_subscription_cache(&mut self, channel_id: &str) {
        self.subscription_cache_order
            .retain(|cached| cached != channel_id);
        self.subscription_cache_order
            .push_back(channel_id.to_owned());
    }

    fn refresh_subscription_video_rows(&mut self) {
        let today = Local::now().date_naive();
        let items = self
            .active_subscription_channel_id
            .as_deref()
            .and_then(|channel_id| self.subscription_video_cache.get(channel_id))
            .map_or(&[][..], |cached| cached.items.as_slice());
        self.view.subscriptions.items = items
            .iter()
            .map(|item| {
                row_from_search_item(
                    item,
                    &self.store,
                    &self.subscription_tree,
                    &self.channel_subscriber_cache,
                    SearchRowContext::SubscriptionFeed,
                    today,
                )
            })
            .collect();
        self.view.subscriptions.selected_item = self
            .view
            .subscriptions
            .selected_item
            .min(self.view.subscriptions.items.len().saturating_sub(1));
    }

    fn selected_subscription_item(&self) -> Option<&SearchItem> {
        let channel_id = self.active_subscription_channel_id.as_deref()?;
        self.subscription_video_cache
            .get(channel_id)?
            .items
            .get(self.view.subscriptions.selected_item)
    }

    fn selected_youtube_item(&self) -> Option<&SearchItem> {
        match self.view.screen {
            Screen::Search
                if self.local_results.is_empty()
                    && self.direct_item.is_none()
                    && self.resolved_direct.is_none() =>
            {
                self.youtube_results.get(self.view.selected)
            }
            Screen::YouTubeMusic => self.youtube_music_results.get(self.view.selected),
            Screen::Subscriptions
                if self.view.subscriptions.route == SubscriptionRoute::Items
                    || (self.view.subscriptions.layout == SubscriptionsLayout::Split
                        && self.view.subscriptions.focus == SubscriptionPane::Items) =>
            {
                self.selected_subscription_item()
            }
            _ => None,
        }
    }

    fn populate_downloads(&mut self) {
        self.view.rows.clear();
        let path = self.config.downloads_dir();
        let entries = match std::fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.view.status_line = "No downloaded media yet".to_owned();
                return;
            }
            Err(error) => {
                self.show_error("Cannot read the downloads directory", &error);
                return;
            }
        };
        let mut read_failures = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    read_failures.push(error.to_string());
                    continue;
                }
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    read_failures.push(format!("{}: {error}", entry.file_name().to_string_lossy()));
                    continue;
                }
            };
            if !file_type.is_file() {
                continue;
            }
            let size = match entry.metadata() {
                Ok(metadata) => metadata.len(),
                Err(error) => {
                    read_failures.push(format!("{}: {error}", entry.file_name().to_string_lossy()));
                    continue;
                }
            };
            self.view.rows.push(RowView {
                media_id: Some(MediaId::new(
                    SourceKind::Local,
                    entry.path().display().to_string(),
                )),
                title: entry.file_name().to_string_lossy().into_owned(),
                subtitle: human_bytes(size),
                source: "Local download".to_owned(),
                ..RowView::default()
            });
        }
        self.view.status_line = format!("{} downloaded file(s)", self.view.rows.len());
        if !read_failures.is_empty() {
            self.show_error_message(
                "Some downloaded files could not be inspected",
                read_failures.join("\n"),
            );
        }
    }

    fn populate_history(&mut self) {
        self.view.rows.clear();
        match self.store.history(false, 500) {
            Ok(entries) => {
                self.view.rows = entries
                    .iter()
                    .map(|entry| RowView {
                        media_id: Some(entry.media_id.clone()),
                        title: entry.title.clone(),
                        subtitle: if entry.finished {
                            "played".to_owned()
                        } else {
                            format!("stopped at {}", format_seconds(entry.position_seconds))
                        },
                        source: entry.media_id.source.to_string(),
                        watched_percent: entry
                            .duration_seconds
                            .filter(|duration| *duration > 0)
                            .map_or(0, |duration| {
                                ((entry.position_seconds.min(duration) * 100) / duration) as u8
                            }),
                        ..RowView::default()
                    })
                    .collect();
                self.view.status_line = format!("{} history item(s)", self.view.rows.len());
                self.update_history_detail();
            }
            Err(error) => {
                self.view.details = None;
                self.show_error("Cannot load history", &error);
            }
        }
    }

    fn populate_statistics(&mut self) {
        self.view.rows.clear();
        match self.store.listen_totals() {
            Ok(totals) => {
                self.view.rows = totals
                    .into_iter()
                    .map(|total| RowView {
                        title: total.source.to_string(),
                        subtitle: format!("{:.2} hours", total.total_seconds as f64 / 3600.0),
                        source: total.source.to_string(),
                        ..RowView::default()
                    })
                    .collect();
                self.view.status_line = "Listening time is displayed in hours".to_owned();
            }
            Err(error) => self.show_error("Cannot load statistics", &error),
        }
    }

    fn current_url(&self) -> Option<String> {
        if let Some(linked) = self.active_description_video.as_ref() {
            return LinkTarget::YouTubeVideo {
                video_id: linked.video_id.clone(),
                start_seconds: linked.start_seconds,
            }
            .canonical_url()
            .map(|url| url.to_string());
        }
        if self.view.screen == Screen::TrackerMusic {
            return self
                .tracker_results
                .get(self.view.selected)
                .map(|item| item.webpage_url.to_string());
        }
        if self.view.screen == Screen::Subscriptions {
            if let Some(item) = self.selected_subscription_item()
                && (self.view.subscriptions.route == SubscriptionRoute::Items
                    || self.view.subscriptions.focus == SubscriptionPane::Items)
            {
                return search_item_url(item);
            }
            return self
                .subscription_entries
                .get(self.view.subscriptions.selected_source)
                .map(|entry| {
                    entry
                        .subscription
                        .website_url
                        .as_ref()
                        .unwrap_or(&entry.subscription.url)
                        .to_string()
                });
        }
        if self.view.screen == Screen::YouTubeMusic {
            return search_item_url(self.youtube_music_results.get(self.view.selected)?);
        }
        if self.view.screen != Screen::Search {
            return None;
        }
        if let Some(item) = self.local_results.get(self.view.selected) {
            return Some(item.path.display().to_string());
        }
        if let Some(media) = &self.resolved_direct
            && let Some(url) = &media.webpage_url
        {
            return Some(url.to_string());
        }
        if let Some(direct) = &self.direct_item {
            return Some(direct.url.to_string());
        }
        search_item_url(self.youtube_results.get(self.view.selected)?)
    }

    fn current_channel_url(&self) -> Option<String> {
        self.view
            .details
            .as_ref()?
            .channel_webpage_url
            .as_ref()
            .map(ToString::to_string)
    }

    fn move_detail_link(&mut self, delta: i32) {
        let link_count = self
            .view
            .details
            .as_ref()
            .map_or(0, |details| details.links.len());
        if link_count == 0 {
            self.view.selected_detail_link = None;
            self.view.status_line = "No external detail link is available".to_owned();
            return;
        }
        let current = self.view.selected_detail_link.unwrap_or_else(|| {
            if delta.is_negative() {
                0
            } else {
                link_count.saturating_sub(1)
            }
        });
        let link_count_i64 = i64::try_from(link_count).unwrap_or(i64::MAX);
        let next = (i64::try_from(current).unwrap_or_default() + i64::from(delta))
            .rem_euclid(link_count_i64);
        self.view.selected_detail_link = usize::try_from(next).ok();
    }

    fn select_detail_link(&mut self, index: usize) {
        if self
            .view
            .details
            .as_ref()
            .is_some_and(|details| index < details.links.len())
        {
            self.view.selected_detail_link = Some(index);
        }
    }

    fn activate_detail_link(&mut self, index: usize) {
        let Some(link) = self
            .view
            .details
            .as_ref()
            .and_then(|details| details.links.get(index))
            .cloned()
        else {
            self.view.status_line = "The selected detail link is no longer available".to_owned();
            return;
        };
        self.view.selected_detail_link = Some(index);
        self.open_external_url(&link.url);
    }

    /// Opens one provider-validated Wikidata value target.
    fn open_wikidata_value(&mut self, url: &str) {
        self.open_external_url(url);
    }

    /// Expands or collapses one linked Wikidata item's lazy property spoiler.
    fn toggle_wikidata_statements(&mut self, index: usize) {
        let Some(item_id) = self
            .view
            .details
            .as_ref()
            .and_then(|details| details.links.get(index))
            .and_then(|link| link.wikidata_item_id.clone())
        else {
            self.view.status_line =
                "The selected external link has no Wikidata properties".to_owned();
            return;
        };
        self.view.selected_detail_link = Some(index);
        let already_expanded = self
            .view
            .details
            .as_ref()
            .and_then(|details| details.expanded_wikidata_item.as_deref())
            == Some(item_id.as_str());
        if already_expanded {
            if let Some(details) = self.view.details.as_mut() {
                details.expanded_wikidata_item = None;
            }
            self.view.details_scroll = 0;
            self.view.status_line = format!("Collapsed Wikidata properties for {item_id}");
            return;
        }
        if let Some(details) = self.view.details.as_mut() {
            details.expanded_wikidata_item = Some(item_id.clone());
        }
        self.view.details_focused = true;
        self.view.details_scroll = 0;

        #[cfg(feature = "wikidata")]
        {
            if let Some(entity) = self.wikidata_entity_cache.get(&item_id).cloned() {
                self.touch_wikidata_entity_cache(&item_id);
                if let Some(details) = self.view.details.as_mut() {
                    apply_wikidata_entity_to_details(details, &entity);
                }
                self.view.status_line =
                    format!("Expanded cached Wikidata properties for {item_id}");
                return;
            }
            if self.pending_wikidata_entities.contains(&item_id) {
                if let Some(details) = self.view.details.as_mut() {
                    details.loading_wikidata_item = Some(item_id);
                }
                return;
            }
            let sent = self.provider_requests.as_ref().is_some_and(|sender| {
                sender
                    .send(ProviderRequest::WikidataStatements {
                        item_id: item_id.clone(),
                    })
                    .is_ok()
            });
            if sent {
                self.pending_wikidata_entities.insert(item_id.clone());
                if let Some(details) = self.view.details.as_mut() {
                    details.loading_wikidata_item = Some(item_id.clone());
                }
                self.view.status_line = format!("Loading Wikidata properties for {item_id}…");
            } else {
                if let Some(details) = self.view.details.as_mut() {
                    details.loading_wikidata_item = None;
                }
                self.view.status_line = "Wikidata provider worker is unavailable".to_owned();
            }
        }
        #[cfg(not(feature = "wikidata"))]
        {
            if let Some(details) = self.view.details.as_mut() {
                details.loading_wikidata_item = None;
            }
            self.view.status_line = "This build omits Wikidata property loading".to_owned();
        }
    }

    #[cfg(feature = "wikidata")]
    fn cache_wikidata_entity(
        &mut self,
        entity: crate::providers::wikidata::WikidataEntityStatements,
    ) {
        let item_id = entity.item_id.clone();
        if !self.wikidata_entity_cache.contains_key(&item_id) {
            while self.wikidata_entity_cache.len() >= MAX_CACHED_WIKIDATA_ENTITIES {
                let Some(oldest) = self.wikidata_entity_cache_order.pop_front() else {
                    break;
                };
                self.wikidata_entity_cache.remove(&oldest);
            }
        }
        self.wikidata_entity_cache.insert(item_id.clone(), entity);
        self.touch_wikidata_entity_cache(&item_id);
    }

    #[cfg(feature = "wikidata")]
    fn touch_wikidata_entity_cache(&mut self, item_id: &str) {
        self.wikidata_entity_cache_order
            .retain(|cached| cached != item_id);
        self.wikidata_entity_cache_order
            .push_back(item_id.to_owned());
    }

    fn diagnostic_helpers(&mut self) -> Vec<ExternalHelper> {
        if self.diagnostic_helpers_cache.is_none() {
            self.diagnostic_helpers_cache = Some(ExternalHelper::probe_many([
                (
                    ExternalHelperKind::Mpv,
                    Some(self.config.providers.mpv_executable.clone()),
                ),
                (
                    ExternalHelperKind::YtDlp,
                    Some(self.config.providers.yt_dlp_executable.clone()),
                ),
            ]));
        }
        self.diagnostic_helpers_cache
            .as_ref()
            .expect("diagnostic helper cache was initialized")
            .clone()
    }

    fn show_error<E>(&mut self, title: &str, error: &E)
    where
        E: std::error::Error + 'static,
    {
        let report = DiagnosticReport::capture_error(error, self.diagnostic_helpers()).render();
        self.show_diagnostic_report(title, report);
        self.view.status_line = format!("{title}: {error}");
    }

    fn show_error_message(&mut self, title: &str, message: impl std::fmt::Display) {
        let report =
            DiagnosticReport::capture_message(&message, self.diagnostic_helpers()).render();
        self.show_diagnostic_report(title, report);
        self.view.status_line = format!("{title}: {message}");
    }

    /// Opens the diagnostic popup with a complete, already-redacted report.
    ///
    /// This is also used by the process-level panic boundary after the normal
    /// terminal session has restored raw mode and the alternate screen.
    pub fn show_diagnostic_report(&mut self, title: impl Into<String>, report: impl Into<String>) {
        self.view.error_popup = Some(ErrorPopupView {
            title: title.into(),
            report: report.into(),
            scroll_offset: 0,
            gh_available: self.report_actions.gh_available(),
            action_status: None,
        });
    }

    /// Stops background activity and prepares a safe error-only TUI after a
    /// fatal panic or terminal-loop failure.
    pub fn enter_fatal_diagnostic_mode(
        &mut self,
        title: impl Into<String>,
        report: impl Into<String>,
    ) {
        #[cfg(feature = "yt-dlp")]
        self.shutdown_youtube_prewarm();
        if let Some(mut player) = self.player.take() {
            let _ = player.shutdown();
        }
        self.invalidate_local_folder_sizes();
        if let Some(sender) = self.local_browse_requests.take() {
            let _ = sender.send(LocalBrowseRequest::Shutdown);
        }
        if let Some(sender) = self.provider_requests.take() {
            let _ = sender.send(ProviderRequest::Shutdown);
        }
        if let Some(handle) = self.local_browse_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.provider_thread.take() {
            let _ = handle.join();
        }
        self.diagnostic_only = true;
        self.quit_on_error_dismiss = true;
        self.clear_search_activity();
        self.clear_playback_start_activity();
        self.view.playing_media_id = None;
        self.view.quitting = false;
        self.view.help_open = false;
        self.view.search_editing = false;
        self.show_diagnostic_report(title, report);
    }

    fn scroll_details(&mut self, movement: DetailsScroll) {
        self.view.details_focused = true;
        if self.view.details.is_none() {
            self.view.details_scroll = 0;
            return;
        }
        self.view.details_scroll = match movement {
            DetailsScroll::Lines(lines) => self
                .view
                .details_scroll
                .saturating_add_signed(isize::try_from(lines).unwrap_or_default()),
            DetailsScroll::Pages(pages) => self.view.details_scroll.saturating_add_signed(
                isize::try_from(pages)
                    .unwrap_or_default()
                    .saturating_mul(20),
            ),
            DetailsScroll::Home => 0,
            DetailsScroll::End => usize::MAX,
        };
    }

    fn scroll_error_popup(&mut self, movement: ErrorPopupScroll) {
        let Some(error) = self.view.error_popup.as_mut() else {
            return;
        };
        error.scroll_offset = match movement {
            ErrorPopupScroll::Lines(lines) => error
                .scroll_offset
                .saturating_add_signed(isize::try_from(lines).unwrap_or_default()),
            ErrorPopupScroll::Pages(pages) => error.scroll_offset.saturating_add_signed(
                isize::try_from(pages)
                    .unwrap_or_default()
                    .saturating_mul(20),
            ),
            ErrorPopupScroll::Home => 0,
            ErrorPopupScroll::End => usize::MAX,
        };
    }

    fn error_report_snapshot(&self) -> Option<(String, String)> {
        self.view
            .error_popup
            .as_ref()
            .map(|error| (error.title.clone(), error.report.clone()))
    }

    fn set_error_action_status(&mut self, status: impl Into<String>) {
        if let Some(error) = self.view.error_popup.as_mut() {
            error.action_status = Some(status.into());
        }
    }

    fn copy_error_report(&mut self) {
        let Some((_, report)) = self.error_report_snapshot() else {
            return;
        };
        match self.report_actions.copy_report(&report) {
            Ok(transport) => {
                self.set_error_action_status(format!("Copied with {transport}"));
            }
            Err(error) => {
                self.set_error_action_status(format!("Copy failed: {error}"));
            }
        }
    }

    fn fill_github_issue(&mut self) {
        let Some((title, report)) = self.error_report_snapshot() else {
            return;
        };
        match self
            .report_actions
            .fill_github_issue(&format!("Youta error: {title}"), &report)
        {
            Ok(()) => {
                self.set_error_action_status("Opened a pre-filled issue for review");
            }
            Err(error) => {
                self.set_error_action_status(format!("Could not open `gh`: {error}"));
            }
        }
    }

    fn copy_and_open_github_issue(&mut self) {
        let Some((title, report)) = self.error_report_snapshot() else {
            return;
        };
        match self
            .report_actions
            .copy_and_open_github_issue(&format!("Youta error: {title}"), &report)
        {
            Ok(transport) => {
                self.set_error_action_status(format!(
                    "Copied with {transport}; opened the issue form"
                ));
            }
            Err(error) => {
                self.set_error_action_status(format!("Issue form action failed: {error}"));
            }
        }
    }

    fn open_current_in_browser(&mut self) {
        let Some(url) = self.current_url() else {
            self.view.status_line = "No link is selected".to_owned();
            return;
        };
        self.spawn_url_opener(&url);
    }

    fn open_current_channel_in_browser(&mut self) {
        let Some(url) = self.current_channel_url() else {
            self.view.status_line = "No channel webpage is available".to_owned();
            return;
        };
        self.open_external_url(&url);
    }

    fn selected_local_regular_file(&self) -> Option<PathBuf> {
        let listing = self.local_listing.as_ref()?;
        let index = self.local_entry_index()?;
        let entry = listing.entries.get(index)?;
        (!entry.is_directory()).then(|| entry.path.clone())
    }

    fn selected_local_trash_target(&self) -> Option<(PathBuf, PathBuf)> {
        let listing = self.local_listing.as_ref()?;
        let index = self.local_entry_index()?;
        let entry = listing.entries.get(index)?;
        Some((listing.path.clone(), entry.path.clone()))
    }

    fn begin_local_rename(&mut self) {
        let Some(path) = self.selected_local_regular_file() else {
            self.view.status_line = "Select a local file before renaming".to_owned();
            return;
        };
        let Some(name) = path.file_name() else {
            self.view.status_line = "The selected file has no basename".to_owned();
            return;
        };
        let value = name.to_string_lossy().into_owned();
        self.view.local_file_popup = Some(LocalFilePopupView::Rename {
            cursor_byte: value.len(),
            value,
            error: None,
        });
    }

    fn append_local_rename_character(&mut self, character: char) {
        let Some(LocalFilePopupView::Rename {
            value,
            cursor_byte,
            error,
        }) = self.view.local_file_popup.as_mut()
        else {
            return;
        };
        if value.len().saturating_add(character.len_utf8()) <= 255 {
            let cursor = rename_cursor_boundary(value, *cursor_byte);
            value.insert(cursor, character);
            *cursor_byte = cursor.saturating_add(character.len_utf8());
            *error = None;
        }
    }

    /// Moves the rename insertion point by one whole displayed grapheme.
    fn move_local_rename_cursor(&mut self, direction: i8) {
        let Some(LocalFilePopupView::Rename {
            value, cursor_byte, ..
        }) = self.view.local_file_popup.as_mut()
        else {
            return;
        };
        let cursor = rename_cursor_boundary(value, *cursor_byte);
        *cursor_byte = if direction < 0 {
            value[..cursor]
                .grapheme_indices(true)
                .next_back()
                .map_or(0, |(index, _)| index)
        } else if direction > 0 {
            value[cursor..]
                .graphemes(true)
                .next()
                .map_or(cursor, |grapheme| cursor.saturating_add(grapheme.len()))
        } else {
            cursor
        };
    }

    fn delete_local_rename_character(&mut self) {
        if let Some(LocalFilePopupView::Rename {
            value,
            cursor_byte,
            error,
        }) = self.view.local_file_popup.as_mut()
        {
            let cursor = rename_cursor_boundary(value, *cursor_byte);
            let previous = value[..cursor]
                .grapheme_indices(true)
                .next_back()
                .map_or(cursor, |(index, _)| index);
            value.drain(previous..cursor);
            *cursor_byte = previous;
            *error = None;
        }
    }

    fn submit_local_rename(&mut self) {
        let value = match self.view.local_file_popup.as_ref() {
            Some(LocalFilePopupView::Rename { value, .. }) => value.clone(),
            _ => return,
        };
        let Some(source) = self.selected_local_regular_file() else {
            self.view.local_file_popup = None;
            self.view.status_line = "The selected local file is no longer available".to_owned();
            return;
        };
        #[cfg(not(feature = "local"))]
        let _ = (&value, &source);
        #[cfg(feature = "local")]
        {
            let mut actions = crate::local_browser::SystemLocalFileActions;
            match crate::local_browser::rename_local_file(
                &mut actions,
                &source,
                std::ffi::OsStr::new(&value),
            ) {
                Ok(target) => {
                    self.view.local_file_popup = None;
                    self.view.status_line = format!("Renamed to {}", target.display());
                    if let Some(directory) = self
                        .local_listing
                        .as_ref()
                        .map(|listing| listing.path.clone())
                    {
                        self.browse_local_directory(directory);
                    }
                }
                Err(error) => {
                    if let Some(LocalFilePopupView::Rename {
                        error: popup_error, ..
                    }) = self.view.local_file_popup.as_mut()
                    {
                        *popup_error = Some(error.to_string());
                    }
                }
            }
        }
        #[cfg(not(feature = "local"))]
        if let Some(LocalFilePopupView::Rename {
            error: popup_error, ..
        }) = self.view.local_file_popup.as_mut()
        {
            *popup_error = Some("this build omits the `local` feature".to_owned());
        }
    }

    fn request_local_trash(&mut self) {
        let Some((_, path)) = self.selected_local_trash_target() else {
            self.view.status_line =
                "Select a local file or folder before moving it to Trash".to_owned();
            return;
        };
        self.view.local_file_popup = Some(LocalFilePopupView::Trash {
            name: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            path: path.display().to_string(),
            error: None,
        });
    }

    /// Removes a confirmed Trash target from the visible snapshot immediately.
    ///
    /// The following background refresh remains authoritative, while this
    /// optimistic update avoids a blank or already-deleted row. Unchanged
    /// sibling cache entries remain available for identity validation.
    fn remove_confirmed_local_trash_target(&mut self, source: &Path) -> Option<PathBuf> {
        let listing = self.local_listing.as_mut()?;
        let removed_index = listing
            .entries
            .iter()
            .position(|entry| entry.path == source)?;
        listing.entries.remove(removed_index);
        let reselected_path = listing
            .entries
            .get(removed_index)
            .or_else(|| {
                removed_index
                    .checked_sub(1)
                    .and_then(|index| listing.entries.get(index))
            })
            .map(|entry| entry.path.clone())
            .or_else(|| listing.parent.clone());

        self.local_folder_size_cache.remove(source);
        self.local_folder_size_cache_order
            .retain(|cached_path| cached_path != source);
        self.local_folder_size_failures.remove(source);
        self.local_folder_size_failure_order
            .retain(|failed_path| failed_path != source);
        self.local_folder_size_queue
            .retain(|queued_path| queued_path != source);
        if self.local_folder_size_pending.as_deref() == Some(source) {
            self.local_folder_size_pending = None;
        }

        self.select_local_path(reselected_path.as_deref());
        self.refresh_local_browser_rows();
        reselected_path
    }

    fn confirm_local_trash(&mut self) {
        let Some((directory, source)) = self.selected_local_trash_target() else {
            self.view.local_file_popup = None;
            self.view.status_line = "The selected local entry is no longer available".to_owned();
            return;
        };
        #[cfg(not(feature = "local"))]
        let _ = (&directory, &source);
        #[cfg(feature = "local")]
        {
            let mut actions = crate::local_browser::SystemLocalFileActions;
            match crate::local_browser::trash_local_entry(&mut actions, &directory, &source) {
                Ok(()) => {
                    self.view.local_file_popup = None;
                    self.view.status_line = format!("Moved {} to system Trash", source.display());
                    let reselected_path = self.remove_confirmed_local_trash_target(&source);
                    self.browse_local_directory_with_reselection(directory, reselected_path);
                }
                Err(error) => {
                    if let Some(LocalFilePopupView::Trash {
                        error: popup_error, ..
                    }) = self.view.local_file_popup.as_mut()
                    {
                        *popup_error = Some(error.to_string());
                    }
                }
            }
        }
        #[cfg(not(feature = "local"))]
        if let Some(LocalFilePopupView::Trash {
            error: popup_error, ..
        }) = self.view.local_file_popup.as_mut()
        {
            *popup_error = Some("this build omits the `local` feature".to_owned());
        }
    }

    fn open_preferences(&mut self) {
        self.view.search_editing = false;
        self.view.help_open = false;
        self.view.text_selection_mode = false;
        let environment_override = [
            SUBSCRIPTIONS_LAYOUT_ENV,
            SKIP_ADVERTISEMENT_CHAPTERS_ENV,
            YOUTUBE_PREWARM_ENV,
            LOCAL_FOLDER_SIZES_ENV,
        ]
        .into_iter()
        .filter(|variable| std::env::var_os(variable).is_some())
        .collect::<Vec<_>>()
        .join(", ");
        self.view.preferences_popup = Some(PreferencesPopupView {
            subscriptions_layout: self.config.ui.subscriptions_layout,
            skip_advertisement_chapters: self.config.playback.skip_advertisement_chapters,
            youtube_prewarm: self.config.playback.youtube_prewarm,
            show_local_folder_sizes: self.config.ui.show_local_folder_sizes,
            config_path: self.config.config_file().display().to_string(),
            environment_override: (!environment_override.is_empty())
                .then_some(environment_override),
            validation_error: None,
        });
        self.view.status_line = "Editing Youta preferences".to_owned();
    }

    /// Persists and applies same-source automatic continuation immediately.
    fn toggle_autoplay(&mut self) {
        let autoplay = !self.config.playback.autoplay;
        if let Err(error) = self.config.save_autoplay(autoplay) {
            self.show_error("Could not save autoplay", &error);
            return;
        }
        if !autoplay {
            self.queued_autoplay_resume_origin = None;
            #[cfg(feature = "tracker-music")]
            self.cancel_pending_tracker_autoplay();
        } else if self.current_autoplay_origin.is_none()
            && let Some(media_id) = self.current_media.clone()
        {
            self.current_autoplay_origin = self.autoplay_origin_for_media(&media_id);
        }
        self.view.autoplay = autoplay;
        self.view.status_line =
            format!("Autoplay {}", if autoplay { "enabled" } else { "disabled" });
    }

    /// Revokes playback ownership without losing a safe completed extraction.
    #[cfg(feature = "tracker-music")]
    fn cancel_pending_tracker_autoplay(&mut self) {
        let Some(pending) = self.pending_tracker_preparation.as_mut() else {
            return;
        };
        if matches!(&pending.owner, TrackerPreparationOwner::Autoplay(_)) {
            pending.owner = TrackerPreparationOwner::CanceledAutoplay;
            self.clear_playback_start_activity();
        }
    }

    fn set_draft_subscriptions_layout(&mut self, layout: SubscriptionsLayout) {
        let Some(preferences) = self.view.preferences_popup.as_mut() else {
            return;
        };
        if preferences.environment_override.is_some() {
            preferences.validation_error = Some(format!(
                "{SUBSCRIPTIONS_LAYOUT_ENV} controls this preference"
            ));
            return;
        }
        preferences.subscriptions_layout = layout;
        preferences.validation_error = None;
    }

    fn toggle_draft_skip_advertisement_chapters(&mut self) {
        let Some(preferences) = self.view.preferences_popup.as_mut() else {
            return;
        };
        if preferences.environment_override.is_some() {
            preferences.validation_error =
                Some("an environment variable controls this preference".to_owned());
            return;
        }
        preferences.skip_advertisement_chapters = !preferences.skip_advertisement_chapters;
        preferences.validation_error = None;
    }

    fn toggle_draft_youtube_prewarm(&mut self) {
        let Some(preferences) = self.view.preferences_popup.as_mut() else {
            return;
        };
        if preferences.environment_override.is_some() {
            preferences.validation_error =
                Some("an environment variable controls this preference".to_owned());
            return;
        }
        preferences.youtube_prewarm = !preferences.youtube_prewarm;
        preferences.validation_error = None;
    }

    fn toggle_draft_local_folder_sizes(&mut self) {
        let Some(preferences) = self.view.preferences_popup.as_mut() else {
            return;
        };
        if preferences.environment_override.is_some() {
            preferences.validation_error =
                Some("an environment variable controls this preference".to_owned());
            return;
        }
        preferences.show_local_folder_sizes = !preferences.show_local_folder_sizes;
        preferences.validation_error = None;
    }

    /// Cycles Local size ordering while retaining the exact selected path.
    fn toggle_local_size_sort(&mut self) {
        if self.view.screen != Screen::Local || !self.config.ui.show_local_folder_sizes {
            return;
        }
        let selected_path = self.selected_local_path();
        self.view.local_size_sort = self.view.local_size_sort.next();
        self.sort_local_listing();
        self.select_local_path(selected_path.as_deref());
        self.refresh_local_browser_rows();
        self.view
            .local_size_sort
            .label()
            .clone_into(&mut self.view.status_line);
    }

    fn submit_preferences(&mut self) {
        let Some(preferences) = self.view.preferences_popup.as_ref() else {
            return;
        };
        if preferences.environment_override.is_some() {
            if let Some(preferences) = self.view.preferences_popup.as_mut() {
                preferences.validation_error = Some(
                    "change or remove the listed environment override before saving".to_owned(),
                );
            }
            return;
        }
        let layout = preferences.subscriptions_layout;
        let skip_advertisement_chapters = preferences.skip_advertisement_chapters;
        let youtube_prewarm = preferences.youtube_prewarm;
        let show_local_folder_sizes = preferences.show_local_folder_sizes;
        #[cfg(feature = "yt-dlp")]
        let youtube_prewarm_preference_changed =
            self.config.playback.youtube_prewarm != youtube_prewarm;
        let local_folder_size_preference_changed =
            self.config.ui.show_local_folder_sizes != show_local_folder_sizes;
        if let Err(error) = self.config.save_tui_preferences(
            layout,
            skip_advertisement_chapters,
            youtube_prewarm,
            show_local_folder_sizes,
        ) {
            if let Some(preferences) = self.view.preferences_popup.as_mut() {
                preferences.validation_error = Some(error.to_string());
            }
            self.show_error("Could not save Youta preferences", &error);
            return;
        }
        self.view.subscriptions.layout = layout;
        self.view.skip_advertisement_chapters = skip_advertisement_chapters;
        #[cfg(feature = "yt-dlp")]
        if youtube_prewarm_preference_changed {
            self.youtube_prewarm_failure = None;
            if youtube_prewarm {
                let selected = self.selected_youtube_item().cloned();
                self.update_youtube_prewarm_for_selection(selected.as_ref());
            } else {
                self.cancel_youtube_prewarm();
            }
        }
        if local_folder_size_preference_changed {
            let selected_path = self.selected_local_path();
            self.invalidate_local_folder_sizes();
            self.view.local_folder_sizes_enabled = show_local_folder_sizes;
            if !show_local_folder_sizes {
                self.view.local_size_sort = LocalSizeSort::Off;
            }
            self.sort_local_listing();
            self.select_local_path(selected_path.as_deref());
            if self.view.screen == Screen::Local {
                self.refresh_local_browser_rows();
            }
            self.schedule_local_folder_sizes();
        }
        self.view.preferences_popup = None;
        if self.view.screen == Screen::Subscriptions {
            self.populate_subscriptions();
        }
        self.view.status_line = format!(
            "Preferences saved: subscriptions {}; advertisement skipping {}; YouTube preparation {}",
            layout.as_config_value(),
            if skip_advertisement_chapters {
                "on"
            } else {
                "off"
            },
            if youtube_prewarm { "on" } else { "off" }
        );
    }

    fn toggle_subscription_description(&mut self) {
        if self.view.screen != Screen::Subscriptions || self.view.subscriptions.items.is_empty() {
            self.view.status_line = "No subscription video description is available".to_owned();
            return;
        }
        if self.view.subscriptions.layout == SubscriptionsLayout::Split {
            self.view.subscriptions.description_expanded =
                !self.view.subscriptions.description_expanded;
            self.view.subscriptions.focus = SubscriptionPane::Items;
            self.view.details_focused = self.view.subscriptions.description_expanded;
            self.view.right_panel_mode = RightPanelMode::Details;
            if self.view.subscriptions.description_expanded {
                self.request_selected_details();
                self.view.status_line =
                    "Expanded the selected subscription video description".to_owned();
            } else {
                self.view.status_line = "Returned to subscription videos".to_owned();
            }
        } else {
            self.view.details_focused = true;
            self.view.right_panel_mode = RightPanelMode::Details;
            self.view.status_line =
                "Details focused; use PageUp/PageDown or the mouse wheel to scroll".to_owned();
        }
    }

    fn open_external_url(&mut self, raw_url: &str) {
        let Ok(url) = url::Url::parse(raw_url) else {
            self.view.status_line = "External link is malformed".to_owned();
            return;
        };
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            self.view.status_line =
                "External links must be credential-free HTTP or HTTPS URLs".to_owned();
            return;
        }
        self.spawn_url_opener(url.as_str());
    }

    fn spawn_url_opener(&mut self, url: &str) {
        if url.starts_with('-') {
            self.show_error_message(
                "Cannot open webpage",
                "the selected target begins with a command-option marker",
            );
            return;
        }
        if self.url_open_pending >= MAX_URL_OPEN_TASKS {
            self.view.status_line =
                "Wait for an earlier system-opener request to finish".to_owned();
            return;
        }
        match spawn_url_opener_task(
            PathBuf::from("xdg-open"),
            url.to_owned(),
            self.url_open_result_sender.clone(),
        ) {
            Ok(()) => {
                self.url_open_pending = self.url_open_pending.saturating_add(1);
                self.view.status_line = "Opening selected webpage with xdg-open…".to_owned();
            }
            Err(error) => self.show_error("Cannot start webpage opener", &error),
        }
    }

    /// Applies URL-free system-opener completions without blocking the TUI.
    fn drain_url_open_results(&mut self) {
        loop {
            match self.url_open_results.try_recv() {
                Ok(UrlOpenCompletion::Succeeded) => {
                    self.url_open_pending = self.url_open_pending.saturating_sub(1);
                    self.view.status_line = "Opened selected webpage with xdg-open".to_owned();
                }
                Ok(UrlOpenCompletion::Failed(error)) => {
                    self.url_open_pending = self.url_open_pending.saturating_sub(1);
                    self.show_error_message("Could not open webpage", error);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
    }

    fn save_session(&mut self) {
        if self.view.screen == Screen::Search {
            self.youtube_search_query
                .clone_from(&self.view.search_query);
            self.youtube_selected = self.view.selected;
        } else if self.view.screen == Screen::YouTubeMusic {
            self.youtube_music_search_query
                .clone_from(&self.view.search_query);
            self.youtube_music_selected = self.view.selected;
        } else if self.view.screen == Screen::TrackerMusic {
            self.tracker_search_query
                .clone_from(&self.view.search_query);
            self.tracker_selected = self.view.selected;
        }
        let state = SessionState {
            screen: stored_screen_from_tui(self.view.screen),
            focus: if self.view.details_focused {
                PanelFocus::Right
            } else {
                PanelFocus::Left
            },
            selected_media: self.current_media.clone(),
            selected_row: self.view.selected,
            youtube_selected_row: Some(self.youtube_selected),
            youtube_music_selected_row: Some(self.youtube_music_selected),
            details_scroll: u64::try_from(self.view.details_scroll).unwrap_or(u64::MAX),
            search_text: self.youtube_search_query.clone(),
            youtube_music_search_text: self.youtube_music_search_query.clone(),
            local_path: (!self.view.local_path.is_empty()).then(|| self.view.local_path.clone()),
            waveform_visible: self.view.right_panel_mode == RightPanelMode::Waveform,
            chapter_timestamps_hidden: !self.view.show_chapter_timestamps,
            ..SessionState::default()
        };
        match self.store.save_session(&state, unix_time()) {
            Ok(()) => {
                self.session_dirty = false;
                self.last_session_save = Instant::now();
            }
            Err(error) => {
                self.show_error("Could not save screen state", &error);
            }
        }
    }

    fn shutdown(&mut self) {
        self.clear_search_activity();
        self.clear_playback_start_activity();
        self.view.playing_media_id = None;
        #[cfg(feature = "yt-dlp")]
        self.shutdown_youtube_prewarm();
        #[cfg(feature = "yt-dlp")]
        if let Some(mut download) = self.active_download.take() {
            download.cancel_and_join();
        }
        if !self.diagnostic_only {
            self.persist_position();
            self.flush_listen_time();
            self.session_dirty = true;
            self.save_session();
        }
        if let Some(player) = self.player.as_mut() {
            let _ = player.shutdown();
        }
        self.invalidate_local_folder_sizes();
        if let Some(sender) = self.local_browse_requests.take() {
            let _ = sender.send(LocalBrowseRequest::Shutdown);
        }
        if let Some(sender) = self.provider_requests.take() {
            let _ = sender.send(ProviderRequest::Shutdown);
        }
        if let Some(handle) = self.local_browse_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.provider_thread.take() {
            let _ = handle.join();
        }
    }
}

impl UiController for AppController {
    fn view(&self) -> &ViewModel {
        &self.view
    }

    fn dispatch(&mut self, action: UiAction) {
        match action {
            UiAction::Quit => {
                self.clear_playback_start_activity();
                self.view.quitting = true;
            }
            UiAction::ToggleHelp => self.view.help_open = !self.view.help_open,
            UiAction::ShowScreen(screen) => self.show_screen(screen),
            UiAction::BeginSearch => self.view.search_editing = true,
            UiAction::CancelSearch => self.view.search_editing = false,
            UiAction::AppendSearch(character) => self.view.search_query.push(character),
            UiAction::DeleteSearchCharacter => {
                self.view.search_query.pop();
            }
            UiAction::SubmitSearch => {
                self.view.search_editing = false;
                self.submit_search();
            }
            UiAction::ToggleSearchKind => {
                if self.view.screen != Screen::Search {
                    self.view.status_line =
                        "Video/channel mode applies only to YouTube search".to_owned();
                } else {
                    self.view.search_kind = match self.view.search_kind {
                        SearchKind::Videos => SearchKind::Channels,
                        SearchKind::Channels => SearchKind::Videos,
                    };
                    self.view.status_line = format!(
                        "YouTube search target: {}",
                        match self.view.search_kind {
                            SearchKind::Videos => "videos",
                            SearchKind::Channels => "channels",
                        }
                    );
                }
            }
            UiAction::ToggleYouTubeSearchSort => {
                if self.view.screen != Screen::Search {
                    self.view.status_line =
                        "Relevance/newest ordering applies only to YouTube search".to_owned();
                } else {
                    self.view.youtube_search_sort = match self.view.youtube_search_sort {
                        YouTubeSearchSort::Relevance => YouTubeSearchSort::Newest,
                        YouTubeSearchSort::Newest => YouTubeSearchSort::Relevance,
                    };
                    if self.view.search_query.trim().is_empty() {
                        self.view.status_line = format!(
                            "YouTube search ordering: {}",
                            match self.view.youtube_search_sort {
                                YouTubeSearchSort::Relevance => "relevance",
                                YouTubeSearchSort::Newest => "newest first",
                            }
                        );
                    } else {
                        // Invalidate an in-flight request before re-running the
                        // same input through normal direct-link/path routing.
                        self.clear_search_activity();
                        self.submit_search();
                    }
                }
            }
            UiAction::ToggleYouTubeCreativeCommons => {
                if self.view.screen != Screen::Search {
                    self.view.status_line =
                        "Creative Commons filtering applies only to YouTube search".to_owned();
                } else if self.view.search_kind != SearchKind::Videos {
                    self.view.status_line =
                        "Creative Commons filtering applies only to YouTube video search"
                            .to_owned();
                } else {
                    self.view.youtube_creative_commons_only =
                        !self.view.youtube_creative_commons_only;
                    if self.view.search_query.trim().is_empty() {
                        self.view.status_line = format!(
                            "YouTube Creative Commons-only search: {}",
                            if self.view.youtube_creative_commons_only {
                                "on"
                            } else {
                                "off"
                            }
                        );
                    } else {
                        // The filter is part of the provider's pagination
                        // identity, so restart at page one before accepting
                        // results from the new result set.
                        self.clear_search_activity();
                        self.submit_search();
                    }
                }
            }
            UiAction::MoveSelection(delta) => {
                self.view.details_focused = false;
                self.view.details_scroll = 0;
                self.view.details_text_selection = None;
                self.move_selection(delta);
            }
            UiAction::SelectRow(row) => {
                self.view.details_focused = false;
                self.view.details_scroll = 0;
                self.view.details_text_selection = None;
                self.select_row(row);
            }
            UiAction::ActivateSelection => self.activate_selection(),
            UiAction::ShowNowPlaying => self.show_now_playing(),
            UiAction::MoveDetailLink(delta) => {
                self.view.details_focused = true;
                self.move_detail_link(delta);
            }
            UiAction::SelectDetailLink(index) => {
                self.view.details_focused = true;
                self.select_detail_link(index);
            }
            UiAction::ActivateDetailLink(index) => {
                self.view.details_focused = true;
                self.activate_detail_link(index);
            }
            UiAction::ToggleWikidataStatements(index) => {
                self.toggle_wikidata_statements(index);
            }
            UiAction::OpenWikidataValue(url) => {
                self.open_wikidata_value(&url);
            }
            UiAction::SetDetailsFocus(focused) => self.view.details_focused = focused,
            UiAction::ScrollDetails(movement) => {
                self.view.details_text_selection = None;
                self.scroll_details(movement);
            }
            UiAction::SetDetailsScroll(offset) => {
                self.view.details_text_selection = None;
                self.view.details_focused = true;
                self.view.details_scroll = offset;
            }
            UiAction::ToggleTextSelectionMode => {
                if self.view.text_selection_mode {
                    self.view.text_selection_mode = false;
                    self.view.details_text_selection = None;
                    self.view.status_line = "Details text selection ended".to_owned();
                } else if self.view.details.is_some()
                    && self.view.right_panel_mode == RightPanelMode::Details
                {
                    self.view.text_selection_mode = true;
                    self.view.details_text_selection = None;
                    self.view.details_focused = true;
                    self.view.status_line =
                        "Select Details text: drag to copy; press t or Esc to exit".to_owned();
                } else {
                    self.view.status_line = "No Details text is available to select".to_owned();
                }
            }
            UiAction::BeginDetailsTextSelection(anchor) => {
                if self.view.text_selection_mode
                    && self.view.right_panel_mode == RightPanelMode::Details
                {
                    self.view.details_text_selection = Some(DetailsTextSelection {
                        anchor,
                        focus: anchor,
                        dragging: true,
                    });
                }
            }
            UiAction::UpdateDetailsTextSelection(focus) => {
                if !self.view.text_selection_mode {
                    return;
                }
                if let Some(selection) = self
                    .view
                    .details_text_selection
                    .as_mut()
                    .filter(|selection| selection.dragging)
                {
                    selection.focus = focus;
                }
            }
            UiAction::FinishDetailsTextSelection { focus, mut text } => {
                if !self.view.text_selection_mode {
                    return;
                }
                let Some(selection) = self
                    .view
                    .details_text_selection
                    .as_mut()
                    .filter(|selection| selection.dragging)
                else {
                    return;
                };
                selection.focus = focus;
                selection.dragging = false;
                truncate_utf8_bytes(&mut text, MAX_DETAILS_SELECTION_BYTES);
                if text.is_empty() {
                    self.view.status_line = "No Details text selected".to_owned();
                } else {
                    let character_count = text.chars().count();
                    self.view.status_line = match self.report_actions.copy_report(&text) {
                        Ok(transport) => {
                            format!("Copied {character_count} characters with {transport}")
                        }
                        Err(error) => format!("Could not copy Details text: {error}"),
                    };
                }
            }
            UiAction::ToggleSubscription => {
                self.view.details_focused = true;
                self.toggle_local_subscription();
            }
            UiAction::ActivateTimecode { media_id, seconds } => {
                self.activate_timecode(media_id, seconds);
            }
            UiAction::ActivateDescriptionVideo {
                video_id,
                start_seconds,
            } => self.activate_description_video(video_id, start_seconds),
            UiAction::TogglePause => self.player_command(PlayerCommand::TogglePause),
            UiAction::SeekRelative(seconds) => {
                self.player_command(PlayerCommand::SeekRelative(seconds));
            }
            UiAction::SeekPercent(percent) => {
                self.player_command(PlayerCommand::SeekPercent(percent));
            }
            UiAction::ChangeVolume(delta) => {
                let volume = i16::from(self.view.playback.volume)
                    .saturating_add(i16::from(delta))
                    .clamp(0, 100) as u8;
                self.player_command(PlayerCommand::SetVolume(volume));
            }
            UiAction::ChangeSpeed(delta) => {
                let speed = (self.view.playback.speed + delta).clamp(0.5, 3.0);
                self.player_command(PlayerCommand::SetSpeed(speed));
            }
            UiAction::ChangeChapter(delta) => {
                self.change_chapter(delta);
            }
            UiAction::ToggleChapterTimestamps => {
                self.view.show_chapter_timestamps = !self.view.show_chapter_timestamps;
                self.view.status_line = format!(
                    "Chapter timestamps {}",
                    if self.view.show_chapter_timestamps {
                        "shown"
                    } else {
                        "hidden"
                    }
                );
            }
            UiAction::ToggleRepeat => {
                self.view.repeating = !self.view.repeating;
                self.playback_queue.repeat_one = self.view.repeating;
                if self.player.is_some() {
                    self.player_command(PlayerCommand::SetRepeat(self.view.repeating));
                } else {
                    self.view.status_line = format!(
                        "Repeat current item {}",
                        if self.view.repeating {
                            "enabled"
                        } else {
                            "disabled"
                        }
                    );
                }
            }
            UiAction::ToggleAutoplay => self.toggle_autoplay(),
            UiAction::ToggleLocalSizeSort => self.toggle_local_size_sort(),
            UiAction::ToggleWaveform => {
                self.view.right_panel_mode =
                    if self.view.right_panel_mode == RightPanelMode::Waveform {
                        RightPanelMode::Details
                    } else {
                        RightPanelMode::Waveform
                    };
            }
            UiAction::ShowChannel => self.show_selected_channel(),
            UiAction::GoBack => self.go_back(),
            UiAction::GoForward => self.go_forward(),
            UiAction::OpenInBrowser => self.open_current_in_browser(),
            UiAction::OpenChannelInBrowser => self.open_current_channel_in_browser(),
            UiAction::CopyLink => {
                self.view.status_line = self.current_url().map_or_else(
                    || "No link is selected".to_owned(),
                    |url| format!("Link: {url}"),
                );
            }
            UiAction::PlayNext => self.queue_selected(true),
            UiAction::AddToQueue => self.queue_selected(false),
            UiAction::Download => self.start_selected_download(),
            UiAction::EditPrivateNote => {
                self.view.status_line = "Private note editor is not open yet".to_owned();
            }
            UiAction::OpenEqualizer => {
                self.view.status_line =
                    "Equalizer is disabled in direct audiophile mode".to_owned();
            }
            UiAction::DismissErrorPopup => {
                self.view.error_popup = None;
                if self.quit_on_error_dismiss {
                    self.view.quitting = true;
                }
            }
            UiAction::ScrollErrorPopup(movement) => self.scroll_error_popup(movement),
            UiAction::CopyErrorReport => self.copy_error_report(),
            UiAction::FillGitHubIssue => self.fill_github_issue(),
            UiAction::CopyAndOpenGitHubIssue => self.copy_and_open_github_issue(),
            UiAction::SelectYouTubeSetupField(field) => {
                if let Some(popup) = self.view.youtube_setup_popup.as_mut() {
                    popup.selected_field = field;
                    popup.validation_error = None;
                }
            }
            UiAction::AppendYouTubeSetupCharacter(character) => {
                self.append_youtube_setup_character(character);
            }
            UiAction::DeleteYouTubeSetupCharacter => {
                self.delete_youtube_setup_character();
            }
            UiAction::OpenYouTubeApiKeyGuide => {
                self.open_external_url(YOUTUBE_API_KEY_GUIDE_URL);
            }
            UiAction::OpenGoogleCloudCredentials => {
                self.open_external_url(GOOGLE_CLOUD_CREDENTIALS_URL);
            }
            UiAction::OpenInvidiousInstances => {
                self.open_external_url(INVIDIOUS_INSTANCES_URL);
            }
            UiAction::SubmitYouTubeSetup => self.submit_youtube_setup(),
            UiAction::DismissYouTubeSetup => {
                self.view.youtube_setup_popup = None;
                self.view.status_line =
                    "YouTube provider setup cancelled; your selection was kept".to_owned();
            }
            UiAction::OpenRssSubscriptionPopup => self.open_rss_subscription_popup(),
            UiAction::AppendRssSubscriptionCharacter(character) => {
                if let Some(popup) = self.view.rss_subscription_popup.as_mut()
                    && popup.url.len() < MAX_RSS_SUBSCRIPTION_URL_BYTES
                {
                    popup.url.push(character);
                    truncate_utf8_bytes(&mut popup.url, MAX_RSS_SUBSCRIPTION_URL_BYTES);
                    popup.validation_error = None;
                }
            }
            UiAction::DeleteRssSubscriptionCharacter => {
                if let Some(popup) = self.view.rss_subscription_popup.as_mut() {
                    popup.url.pop();
                    popup.validation_error = None;
                }
            }
            UiAction::SubmitRssSubscription => self.submit_rss_subscription(),
            UiAction::DismissRssSubscriptionPopup => {
                self.view.rss_subscription_popup = None;
                self.view.status_line = "RSS subscription canceled".to_owned();
            }
            UiAction::OpenPreferences => self.open_preferences(),
            UiAction::SetSubscriptionsLayout(layout) => {
                self.set_draft_subscriptions_layout(layout);
            }
            UiAction::ToggleSkipAdvertisementChapters => {
                self.toggle_draft_skip_advertisement_chapters();
            }
            UiAction::ToggleYouTubePrewarm => self.toggle_draft_youtube_prewarm(),
            UiAction::ToggleLocalFolderSizes => self.toggle_draft_local_folder_sizes(),
            UiAction::SubmitPreferences => self.submit_preferences(),
            UiAction::DismissPreferences => {
                self.view.preferences_popup = None;
                self.view.status_line = "Preferences were not changed".to_owned();
            }
            UiAction::BeginLocalRename => self.begin_local_rename(),
            UiAction::AppendLocalRenameCharacter(character) => {
                self.append_local_rename_character(character);
            }
            UiAction::MoveLocalRenameCursor(direction) => {
                self.move_local_rename_cursor(direction);
            }
            UiAction::DeleteLocalRenameCharacter => {
                self.delete_local_rename_character();
            }
            UiAction::SubmitLocalRename => self.submit_local_rename(),
            UiAction::RequestLocalTrash => self.request_local_trash(),
            UiAction::ConfirmLocalTrash => self.confirm_local_trash(),
            UiAction::DismissLocalFilePopup => {
                self.view.local_file_popup = None;
                self.view.status_line = "Local file was not changed".to_owned();
            }
            UiAction::SelectSubscriptionSource(index) => {
                self.view.details_focused = false;
                self.select_subscription_source(index);
            }
            UiAction::SelectSubscriptionItem(index) => {
                self.select_subscription_item(index);
            }
            UiAction::ToggleSubscriptionDescription => {
                self.toggle_subscription_description();
            }
            UiAction::RefreshSubscriptionVideos => {
                self.refresh_selected_subscription_videos();
            }
        }
        self.session_dirty |= !self.diagnostic_only;
        if self.view.quitting && !self.diagnostic_only {
            self.save_session();
        }
    }

    fn tick(&mut self) {
        if self.diagnostic_only {
            return;
        }
        self.drain_url_open_results();
        loop {
            match self.local_browse_responses.try_recv() {
                Ok(response) => self.handle_local_browse_response(response),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.view.local_browse_pending = false;
                    if !self.local_browse_disconnect_reported {
                        self.local_browse_disconnect_reported = true;
                        self.show_error_message(
                            "Local browser worker stopped",
                            "the background Local browser channel disconnected unexpectedly",
                        );
                    }
                    break;
                }
            }
        }
        loop {
            match self.provider_responses.try_recv() {
                Ok(response) => self.handle_provider_response(response),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.clear_search_activity();
                    if !self.provider_disconnect_reported {
                        self.provider_disconnect_reported = true;
                        self.show_error_message(
                            "Provider worker stopped",
                            "the background provider channel disconnected unexpectedly",
                        );
                    }
                    break;
                }
            }
        }
        self.advance_search_animation();
        self.advance_playback_start_animation();
        let now = Instant::now();
        self.request_due_channel_details(now);
        #[cfg(feature = "wikidata")]
        self.request_due_channel_wikidata(now);
        #[cfg(feature = "yt-dlp")]
        {
            self.drain_youtube_prewarm_responses();
            self.request_due_youtube_prewarm(now);
        }
        #[cfg(feature = "yt-dlp")]
        self.poll_download();
        self.update_player();
        if self.session_dirty && self.last_session_save.elapsed() >= Duration::from_secs(30) {
            self.save_session();
        }
    }
}

impl Drop for AppController {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Builds one system-opener command without a synthetic option separator.
///
/// `xdg-open` accepts exactly one target argument; unlike many command-line
/// tools, it does not recognize `--` as an option terminator.
fn url_opener_command(executable: &Path, target: &str) -> Command {
    let mut command = Command::new(executable);
    command
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Starts and reaps one system-opener process outside the terminal event loop.
///
/// The completion contains no URL so selected links cannot leak into
/// diagnostics. A successful thread spawn is not treated as a successful open:
/// the child must also report a zero exit status.
fn spawn_url_opener_task(
    executable: PathBuf,
    target: String,
    results: Sender<UrlOpenCompletion>,
) -> std::io::Result<()> {
    thread::Builder::new()
        .name("youta-url-opener".to_owned())
        .spawn(move || {
            let completion = match url_opener_command(&executable, &target).spawn() {
                Ok(mut child) => match child.wait() {
                    Ok(status) if status.success() => UrlOpenCompletion::Succeeded,
                    Ok(status) => UrlOpenCompletion::Failed(status.code().map_or_else(
                        || "xdg-open was terminated before reporting an exit status".to_owned(),
                        |code| format!("xdg-open exited with status {code}"),
                    )),
                    Err(error) => UrlOpenCompletion::Failed(format!(
                        "could not wait for xdg-open to finish: {error}"
                    )),
                },
                Err(error) => {
                    UrlOpenCompletion::Failed(format!("could not start xdg-open: {error}"))
                }
            };
            let _ = results.send(completion);
        })
        .map(|_| ())
}

/// Resolves one latest selected `YouTube` video without blocking the TUI.
///
/// Both channels contain at most one value. When the UI has not yet consumed
/// an obsolete completion, the worker replaces it before publishing the
/// latest result so shutdown cannot deadlock on a full response channel.
#[cfg(feature = "yt-dlp")]
fn youtube_prewarm_worker(
    requests: Receiver<YouTubePrewarmCommand>,
    responses: Sender<YouTubePrewarmCompletion>,
    response_drain: Receiver<YouTubePrewarmCompletion>,
    resolver: YouTubePrewarmResolver,
) {
    while let Ok(command) = requests.recv() {
        let YouTubePrewarmCommand::Resolve(job) = command else {
            break;
        };
        let result = resolver.resolve(job.request, &job.cancellation);
        let mut completion = YouTubePrewarmCompletion {
            media_id: job.media_id,
            completed_at: Instant::now(),
            result,
        };
        loop {
            match responses.try_send(completion) {
                Ok(()) => break,
                Err(TrySendError::Full(returned)) => {
                    completion = returned;
                    let _ = response_drain.try_recv();
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
    }
}

/// Runs foreground Local listings independently of recursive and remote work.
fn local_browse_worker(
    requests: Receiver<LocalBrowseRequest>,
    responses: Sender<LocalBrowseResponse>,
) {
    while let Ok(request) = requests.recv() {
        let LocalBrowseRequest::Browse {
            generation,
            directory,
            preferred_child,
        } = request
        else {
            break;
        };
        let result = crate::local_browser::list_local_directory_with_preferred_child(
            &directory,
            crate::local_browser::LocalBrowseLimits::default(),
            preferred_child.as_deref(),
        )
        .map_err(|error| error.to_string());
        if responses
            .send(LocalBrowseResponse { generation, result })
            .is_err()
        {
            break;
        }
    }
}

fn provider_worker(
    provider: Option<Box<dyn Provider>>,
    requests: Receiver<ProviderRequest>,
    responses: Sender<ProviderResponse>,
    allow_insecure_http: bool,
    mod_archive_api_key: Option<String>,
    jamendo_client_id: Option<String>,
    #[cfg(feature = "youtube-music")] youtube_music_executable: PathBuf,
    provider_storage_root: PathBuf,
) {
    let mut provider = provider;
    #[cfg(feature = "apple-podcasts")]
    let apple = crate::providers::apple_podcasts::ApplePodcastsResolver::new();
    #[cfg(feature = "soundstream")]
    let soundstream = crate::providers::soundstream::SoundStreamResolver::new();
    #[cfg(feature = "litres")]
    let litres = crate::providers::litres::LitresPublicResolver::new();
    #[cfg(feature = "jamendo")]
    let jamendo = jamendo_client_id
        .map(crate::providers::jamendo::JamendoProvider::new)
        .transpose()
        .map_err(|error| format!("invalid `providers.jamendo_client_id`: {error}"));
    #[cfg(feature = "tracker-music")]
    let tracker = crate::providers::tracker::TrackerArchiveHub::new(allow_insecure_http);
    #[cfg(feature = "tracker-music")]
    let mut tracker_preparer = {
        let limits = crate::tracker_media::TrackerMediaLimits::default();
        crate::tracker_media::UreqTrackerTransport::for_limits(
            crate::providers::DEFAULT_REQUEST_TIMEOUT,
            limits,
        )
        .map_err(|error| error.to_string())
        .and_then(|transport| {
            crate::tracker_media::TrackerMediaPreparer::new(
                &provider_storage_root,
                transport,
                limits,
            )
            .map_err(|error| error.to_string())
        })
    };
    #[cfg(feature = "tracker-music")]
    let mod_archive = mod_archive_api_key
        .and_then(|key| crate::providers::modarchive::ModArchiveProvider::new(key).ok());
    #[cfg(feature = "wikidata")]
    let wikidata = crate::providers::wikidata::WikidataProvider::new();
    #[cfg(feature = "network")]
    let youtube_channel_page =
        crate::providers::youtube_channel_page::YouTubeChannelPageClient::new();
    #[cfg(feature = "youtube-music")]
    let youtube_music = YouTubeMusicSearch::new(YouTubeMusicSearchConfig {
        executable: youtube_music_executable,
        ..YouTubeMusicSearchConfig::default()
    });
    #[cfg(not(feature = "tracker-music"))]
    let _ = (
        allow_insecure_http,
        mod_archive_api_key,
        provider_storage_root,
    );
    #[cfg(not(feature = "jamendo"))]
    let _ = jamendo_client_id;

    while let Ok(request) = requests.recv() {
        match request {
            ProviderRequest::ReplaceYouTubeProvider {
                provider: replacement,
            } => {
                provider = Some(replacement);
            }
            ProviderRequest::Search {
                generation,
                request,
            } => {
                let result = provider.as_ref().map_or_else(
                    || Err("YouTube provider is not configured".to_owned()),
                    |provider| provider.search(&request).map_err(|error| error.to_string()),
                );
                if responses
                    .send(ProviderResponse::Search {
                        generation,
                        request,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
            #[cfg(feature = "youtube-music")]
            ProviderRequest::YouTubeMusicSearch { generation, query } => {
                let result = youtube_music
                    .search(&query, 30)
                    .map_err(|error| error.to_string());
                if responses
                    .send(ProviderResponse::YouTubeMusicSearch {
                        generation,
                        query,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
            ProviderRequest::ChannelVideos {
                generation,
                request,
            } => {
                let result = provider.as_ref().map_or_else(
                    || Err("YouTube provider is not configured".to_owned()),
                    |provider| {
                        provider
                            .channel_videos(&request)
                            .map_err(|error| error.to_string())
                    },
                );
                if responses
                    .send(ProviderResponse::ChannelVideos {
                        generation,
                        request,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
            ProviderRequest::Details {
                generation,
                video_id,
            } => {
                let result = provider.as_ref().map_or_else(
                    || Err("YouTube provider is not configured".to_owned()),
                    |provider| {
                        provider
                            .video_details(&video_id)
                            .map_err(|error| error.to_string())
                    },
                );
                if responses
                    .send(ProviderResponse::Details { generation, result })
                    .is_err()
                {
                    break;
                }
            }
            ProviderRequest::ChannelDetails {
                generation,
                provider_generation,
                channel_id,
            } => {
                let result = provider.as_ref().map_or_else(
                    || Err("YouTube provider is not configured".to_owned()),
                    |provider| {
                        provider
                            .full_channel_details(&channel_id)
                            .map_err(|error| error.to_string())
                    },
                );
                let can_enrich_from_page = result.is_ok();
                if responses
                    .send(ProviderResponse::ChannelDetails {
                        generation,
                        provider_generation,
                        channel_id: channel_id.clone(),
                        result,
                    })
                    .is_err()
                {
                    break;
                }
                #[cfg(feature = "network")]
                if can_enrich_from_page {
                    let result = youtube_channel_page
                        .channel_metadata(&channel_id)
                        .map_err(|error| error.to_string());
                    if responses
                        .send(ProviderResponse::ChannelPageMetadata {
                            generation,
                            provider_generation,
                            channel_id,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                #[cfg(not(feature = "network"))]
                let _ = can_enrich_from_page;
            }
            ProviderRequest::ChannelSubscriberCounts {
                provider_generation,
                channel_ids,
            } => {
                let requested_ids = channel_ids.clone();
                let result = provider.as_ref().map_or_else(
                    || Err("YouTube provider is not configured".to_owned()),
                    |provider| {
                        provider
                            .channel_subscriber_counts(&channel_ids)
                            .map_err(|error| error.to_string())
                    },
                );
                if responses
                    .send(ProviderResponse::ChannelSubscriberCounts {
                        provider_generation,
                        requested_ids,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
            ProviderRequest::ResolveApple { generation, url } => {
                #[cfg(feature = "apple-podcasts")]
                let result = apple
                    .resolve(&url)
                    .map(resolved_apple_media)
                    .map_err(|error| error.to_string());
                #[cfg(not(feature = "apple-podcasts"))]
                let result = {
                    let _ = url;
                    Err("this build omits the `apple-podcasts` feature".to_owned())
                };
                if responses
                    .send(ProviderResponse::Apple { generation, result })
                    .is_err()
                {
                    break;
                }
            }
            ProviderRequest::ResolveFirstClass { generation, direct } => {
                let source = direct.source.clone();
                let result = match direct.source {
                    SourceKind::SoundStream => {
                        #[cfg(feature = "soundstream")]
                        {
                            soundstream
                                .resolve(&direct.url)
                                .map(resolved_soundstream_media)
                                .map_err(|error| error.to_string())
                        }
                        #[cfg(not(feature = "soundstream"))]
                        {
                            Err("this build omits the `soundstream` feature".to_owned())
                        }
                    }
                    SourceKind::LitRes => {
                        #[cfg(feature = "litres")]
                        {
                            litres
                                .resolve(&direct.url)
                                .map(resolved_litres_media)
                                .map_err(|error| error.to_string())
                        }
                        #[cfg(not(feature = "litres"))]
                        {
                            Err("this build omits the `litres` feature".to_owned())
                        }
                    }
                    SourceKind::Jamendo => {
                        #[cfg(feature = "jamendo")]
                        {
                            jamendo.as_ref().map_err(Clone::clone).and_then(|provider| {
                                let provider = provider.as_ref().ok_or_else(|| {
                                    "set `providers.jamendo_client_id` to a client ID issued \
                                     for your Jamendo application"
                                        .to_owned()
                                })?;
                                let track_id = jamendo_track_id(&direct.url)?;
                                provider
                                    .track(&track_id)
                                    .map(resolved_jamendo_media)
                                    .map_err(|error| error.to_string())
                            })
                        }
                        #[cfg(not(feature = "jamendo"))]
                        {
                            Err("this build omits the `jamendo` feature".to_owned())
                        }
                    }
                    _ => Err("the requested source has no first-class direct resolver".to_owned()),
                };
                if responses
                    .send(ProviderResponse::FirstClass {
                        generation,
                        source,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
            ProviderRequest::TrackerSearch { generation, query } => {
                #[cfg(feature = "tracker-music")]
                {
                    if let Some(provider) = &mod_archive {
                        let request =
                            crate::providers::modarchive::ModuleSearchRequest::new(query.clone());
                        let result = provider
                            .search(&request)
                            .map(|page| {
                                page.modules
                                    .into_iter()
                                    .map(|module| TrackerItem {
                                        source: "The Mod Archive".to_owned(),
                                        title: if module.song_title.trim().is_empty() {
                                            module.filename
                                        } else {
                                            module.song_title
                                        },
                                        subtitle: format!(
                                            "{} · {}",
                                            module.format,
                                            module.size_bytes.map_or_else(
                                                || "unknown size".to_owned(),
                                                human_bytes,
                                            )
                                        ),
                                        webpage_url: module.webpage_url,
                                        download_url: Some(module.download_url),
                                        expected_format: Some(module.format),
                                        access: TrackerAccess::DirectModule,
                                        prepared_path: None,
                                        insecure_transport: false,
                                    })
                                    .collect()
                            })
                            .map_err(|error| error.to_string());
                        if responses
                            .send(ProviderResponse::TrackerSource {
                                generation,
                                source: "The Mod Archive".to_owned(),
                                result,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    for source in crate::providers::tracker::TrackerArchiveSource::ALL {
                        let descriptor = source.descriptor();
                        let request =
                            crate::providers::tracker::TrackerSearchRequest::new(query.clone());
                        let result = tracker
                            .search(source, &request)
                            .map(|page| {
                                page.items
                                    .into_iter()
                                    .map(tracker_item_from_provider)
                                    .collect()
                            })
                            .map_err(|error| error.to_string());
                        if responses
                            .send(ProviderResponse::TrackerSource {
                                generation,
                                source: descriptor.display_name.to_owned(),
                                result,
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                #[cfg(not(feature = "tracker-music"))]
                {
                    let _ = query;
                    if responses
                        .send(ProviderResponse::TrackerSource {
                            generation,
                            source: "MOD/tracker".to_owned(),
                            result: Err("this build omits the `tracker-music` feature".to_owned()),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                if responses
                    .send(ProviderResponse::TrackerComplete { generation })
                    .is_err()
                {
                    break;
                }
            }
            #[cfg(feature = "tracker-music")]
            ProviderRequest::PrepareTracker {
                generation,
                item_key,
                request,
            } => {
                let result = match tracker_preparer.as_mut() {
                    Ok(preparer) => preparer
                        .prepare(&request)
                        .map_err(|error| error.to_string()),
                    Err(error) => Err(error.clone()),
                };
                if responses
                    .send(ProviderResponse::TrackerPrepared {
                        generation,
                        item_key,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
            ProviderRequest::ScanLocal { generation, root } => {
                let result = scan_local_media(&root);
                if responses
                    .send(ProviderResponse::LocalScan {
                        generation,
                        root,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
            #[cfg(all(feature = "local", feature = "thumbnails"))]
            ProviderRequest::LocalArtwork {
                generation,
                path,
                cache_directory,
                kind,
            } => {
                let result = match kind {
                    LocalArtworkKind::EmbeddedMedia => {
                        crate::thumbnails::cached_local_artwork(&path, &cache_directory)
                            .map_err(|error| error.to_string())
                    }
                    LocalArtworkKind::FolderCover => {
                        crate::local_browser::find_local_folder_cover(&path)
                            .map(|cover| {
                                cover.and_then(|cover| url::Url::from_file_path(cover).ok())
                            })
                            .map_err(|error| error.to_string())
                    }
                };
                if responses
                    .send(ProviderResponse::LocalArtwork {
                        generation,
                        path,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
            ProviderRequest::LocalFolderSize {
                generation,
                path,
                cancellation,
                cached,
            } => {
                let cancelled = || cancellation.load(AtomicOrdering::Relaxed) != generation;
                let reusable = (!cancelled())
                    .then_some(cached)
                    .flatten()
                    .filter(|measurement| {
                        crate::local_browser::local_directory_identity(&path)
                            .is_ok_and(|identity| identity == measurement.identity)
                    });
                let result = reusable
                    .map_or_else(
                        || {
                            crate::local_browser::measure_local_folder_size(
                                &path,
                                crate::local_browser::LocalFolderSizeLimits::default(),
                                cancelled,
                            )
                            .map(|measurement| {
                                LocalFolderSizeWorkerResult {
                                    measurement,
                                    reused: false,
                                }
                            })
                        },
                        |measurement| {
                            Ok(LocalFolderSizeWorkerResult {
                                measurement,
                                reused: true,
                            })
                        },
                    )
                    .map_err(|error| error.to_string());
                if responses
                    .send(ProviderResponse::LocalFolderSize {
                        generation,
                        path,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
            #[cfg(feature = "wikidata")]
            ProviderRequest::Wikidata {
                generation,
                kind,
                external_id,
            } => {
                let property_id = kind.property_id().to_owned();
                let result = wikidata
                    .lookup_external(kind, &external_id)
                    .map(|lookup| lookup.items)
                    .map_err(|error| error.to_string());
                if responses
                    .send(ProviderResponse::Wikidata {
                        generation,
                        property_id,
                        external_id,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
            #[cfg(feature = "wikidata")]
            ProviderRequest::WikidataStatements { item_id } => {
                let result = wikidata
                    .load_entity_statements(&item_id)
                    .map_err(|error| error.to_string());
                if responses
                    .send(ProviderResponse::WikidataStatements { item_id, result })
                    .is_err()
                {
                    break;
                }
            }
            ProviderRequest::Shutdown => break,
        }
    }
}

#[cfg(feature = "apple-podcasts")]
fn resolved_apple_media(
    resolved: crate::providers::apple_podcasts::ResolvedApplePodcast,
) -> ResolvedDirectMedia {
    if let Some(episode) = resolved.episode {
        let playable = episode.media_url.is_some();
        let description = episode.description.unwrap_or_else(|| {
            format!(
                "Episode of {}{}",
                episode.podcast_title,
                resolved
                    .podcast
                    .feed_url
                    .as_ref()
                    .map_or_else(String::new, |url| format!("\nRSS: {url}"))
            )
        });
        ResolvedDirectMedia {
            source: SourceKind::ApplePodcasts,
            external_id: episode.episode_id.to_string(),
            title: episode.title,
            row_subtitle: episode.duration_seconds.map_or_else(
                || "episode".to_owned(),
                |duration| format!("episode · {}", format_seconds(duration)),
            ),
            description,
            license: "publisher terms".to_owned(),
            published: episode.published_at,
            artwork_url: episode.artwork_url.or(resolved.podcast.artwork_url),
            duration_seconds: episode.duration_seconds,
            playback_url: episode.media_url,
            webpage_url: episode.webpage_url.or(resolved.podcast.webpage_url),
            status_line: if playable {
                "Apple Podcasts episode resolved; press Enter to play".to_owned()
            } else {
                "Apple Podcasts metadata resolved, but its RSS item has no public enclosure"
                    .to_owned()
            },
        }
    } else {
        let description = format!(
            "{}{}{}",
            resolved.podcast.author.unwrap_or_default(),
            resolved
                .podcast
                .feed_url
                .as_ref()
                .map_or_else(String::new, |url| format!("\nRSS: {url}")),
            if resolved.podcast.genres.is_empty() {
                String::new()
            } else {
                format!("\nGenres: {}", resolved.podcast.genres.join(", "))
            }
        );
        ResolvedDirectMedia {
            source: SourceKind::ApplePodcasts,
            external_id: resolved.podcast.collection_id.to_string(),
            title: resolved.podcast.title,
            row_subtitle: "podcast show".to_owned(),
            description,
            license: "publisher terms".to_owned(),
            published: None,
            artwork_url: resolved.podcast.artwork_url,
            duration_seconds: None,
            playback_url: None,
            webpage_url: resolved.podcast.webpage_url,
            status_line: "Apple podcast resolved to its public RSS feed; select an episode to play"
                .to_owned(),
        }
    }
}

fn direct_source_label(source: &SourceKind) -> &'static str {
    match source {
        SourceKind::ApplePodcasts => "Apple Podcasts",
        SourceKind::SoundStream => "SoundStream",
        SourceKind::LitRes => "LitRes",
        SourceKind::Jamendo => "Jamendo",
        _ => "Direct source",
    }
}

fn requires_first_class_direct_resolution(source: &SourceKind) -> bool {
    matches!(
        source,
        SourceKind::SoundStream | SourceKind::LitRes | SourceKind::Jamendo
    )
}

fn apply_resolved_direct_view(view: &mut ViewModel, media: &ResolvedDirectMedia) {
    let source = direct_source_label(&media.source);
    let media_id = MediaId::new(media.source.clone(), media.external_id.clone());
    view.rows = vec![RowView {
        media_id: Some(media_id.clone()),
        title: media.title.clone(),
        subtitle: media.row_subtitle.clone(),
        source: source.to_owned(),
        ..RowView::default()
    }];
    view.details = Some(DetailView {
        media_id: Some(media_id),
        title: media.title.clone(),
        source: source.to_owned(),
        length: media
            .duration_seconds
            .map_or_else(|| "unknown".to_owned(), format_seconds),
        description: media.description.clone(),
        timecodes: detail_timecodes(&media.description),
        published: media
            .published
            .clone()
            .unwrap_or_else(|| "unknown".to_owned()),
        license: media.license.clone(),
        wikidata: "not loaded".to_owned(),
        thumbnail_url: media.artwork_url.clone(),
        ..DetailView::default()
    });
    view.status_line.clone_from(&media.status_line);
}

#[cfg(feature = "soundstream")]
fn resolved_soundstream_media(
    resolved: crate::providers::soundstream::ResolvedSoundStream,
) -> ResolvedDirectMedia {
    use crate::providers::soundstream::SoundStreamMetadata;

    match resolved.metadata {
        SoundStreamMetadata::Playlist(playlist) => {
            let mut metadata = Vec::new();
            if let Some(source) = playlist.source_url {
                metadata.push(format!("Publisher page: {source}"));
            }
            if let Some(feed) = playlist.feed_url {
                metadata.push(format!("RSS: {feed}"));
            }
            if let Some(count) = playlist.clip_count {
                metadata.push(format!("Episodes: {count}"));
            }
            if let Some(explicit) = playlist.explicit {
                metadata.push(format!("Explicit: {}", if explicit { "yes" } else { "no" }));
            }
            ResolvedDirectMedia {
                source: SourceKind::SoundStream,
                external_id: playlist.playlist_id.to_string(),
                title: playlist.title,
                row_subtitle: playlist.clip_count.map_or_else(
                    || "playlist".to_owned(),
                    |count| format!("playlist · {count} episode(s)"),
                ),
                description: append_metadata(playlist.description, metadata),
                license: "publisher terms".to_owned(),
                published: None,
                artwork_url: playlist.artwork_url,
                duration_seconds: None,
                playback_url: None,
                webpage_url: Some(playlist.webpage_url),
                status_line: "SoundStream playlist metadata resolved; no public episode was \
                              selected for playback"
                    .to_owned(),
            }
        }
        SoundStreamMetadata::Clip(clip) => {
            let playable = clip.media_url.is_some();
            let mut metadata = Vec::new();
            for playlist in &clip.playlists {
                metadata.push(format!(
                    "Playlist: {} ({}){}",
                    playlist.title,
                    playlist.webpage_url,
                    playlist
                        .feed_url
                        .as_ref()
                        .map_or_else(String::new, |feed| format!(" · RSS: {feed}"))
                ));
            }
            if let Some(explicit) = clip.explicit {
                metadata.push(format!("Explicit: {}", if explicit { "yes" } else { "no" }));
            }
            ResolvedDirectMedia {
                source: SourceKind::SoundStream,
                external_id: clip.clip_id.to_string(),
                title: clip.title,
                row_subtitle: clip.duration_seconds.map_or_else(
                    || "clip".to_owned(),
                    |duration| format!("clip · {}", format_seconds(duration)),
                ),
                description: append_metadata(clip.description, metadata),
                license: "publisher terms".to_owned(),
                published: clip.published_at,
                artwork_url: clip.artwork_url,
                duration_seconds: clip.duration_seconds,
                playback_url: clip.media_url,
                webpage_url: Some(clip.webpage_url),
                status_line: if playable {
                    "SoundStream exposed a public media enclosure; press Enter to play".to_owned()
                } else {
                    "SoundStream clip metadata resolved, but no credential-free public media \
                     enclosure was exposed"
                        .to_owned()
                },
            }
        }
    }
}

#[cfg(feature = "litres")]
fn resolved_litres_media(page: crate::providers::litres::LitresPublicPage) -> ResolvedDirectMedia {
    use crate::providers::litres::LitresPublicMediaAccess;

    let selected_media = page
        .media
        .iter()
        .find(|media| media.access == LitresPublicMediaAccess::Full)
        .or_else(|| page.media.first())
        .cloned();
    let access = selected_media.as_ref().map(|media| media.access);
    let mut metadata = Vec::new();
    if !page.creators.is_empty() {
        metadata.push(format!("Creators: {}", page.creators.join(", ")));
    }
    if let Some(is_free) = page.is_free {
        metadata.push(format!(
            "Public page marks item free: {}",
            if is_free { "yes" } else { "no" }
        ));
    }
    if let Some(price) = &page.price {
        metadata.push(format!(
            "Price: {price}{}",
            page.price_currency
                .as_ref()
                .map_or_else(String::new, |currency| format!(" {currency}"))
        ));
    }
    match access {
        Some(LitresPublicMediaAccess::Preview) => {
            metadata.push("Playback media: preview only".to_owned());
        }
        Some(LitresPublicMediaAccess::Full) => {
            metadata.push("Playback media: explicitly public full item".to_owned());
        }
        None => metadata.push("Playback media: not exposed by the public page".to_owned()),
    }
    if let Some(mime_type) = selected_media
        .as_ref()
        .and_then(|media| media.mime_type.as_deref())
    {
        metadata.push(format!("Playback media type: {mime_type}"));
    }

    ResolvedDirectMedia {
        source: SourceKind::LitRes,
        external_id: page.link.item_id.to_string(),
        title: page.title,
        row_subtitle: match access {
            Some(LitresPublicMediaAccess::Preview) => "preview".to_owned(),
            Some(LitresPublicMediaAccess::Full) => "free public media".to_owned(),
            None => "metadata only".to_owned(),
        },
        description: append_metadata(page.description, metadata),
        license: "publisher terms".to_owned(),
        published: page.published_at,
        artwork_url: page.artwork_url,
        duration_seconds: page.duration_seconds,
        playback_url: selected_media.map(|media| media.url),
        webpage_url: Some(page.link.canonical_url),
        status_line: match access {
            Some(LitresPublicMediaAccess::Preview) => {
                "LitRes preview resolved; press Enter to play the preview".to_owned()
            }
            Some(LitresPublicMediaAccess::Full) => {
                "LitRes explicitly public media resolved; press Enter to play".to_owned()
            }
            None => {
                "LitRes metadata resolved; the public page exposes no playable media URL".to_owned()
            }
        },
    }
}

#[cfg(feature = "jamendo")]
fn resolved_jamendo_media(track: crate::providers::jamendo::JamendoTrack) -> ResolvedDirectMedia {
    let mut metadata = vec![
        format!("Artist: {}", track.artist_name),
        format!("Artist ID: {}", track.artist_id),
    ];
    if let Some(album) = &track.album_name {
        metadata.push(format!(
            "Album: {album}{}",
            track
                .album_id
                .as_ref()
                .map_or_else(String::new, |album_id| format!(" (ID: {album_id})"))
        ));
    }
    if !track.tags.is_empty() {
        metadata.push(format!("Tags: {}", track.tags.join(", ")));
    }
    if let Some(short_url) = &track.short_url {
        metadata.push(format!("Short link: {short_url}"));
    }
    metadata.push(if let Some(url) = &track.download_url {
        format!("Official download allowed: yes ({url})")
    } else {
        "Official download allowed: no".to_owned()
    });

    ResolvedDirectMedia {
        source: SourceKind::Jamendo,
        external_id: track.track_id,
        title: track.title,
        row_subtitle: format!(
            "{} · {}",
            track.artist_name,
            format_seconds(track.duration_seconds)
        ),
        description: append_metadata(None, metadata),
        license: track.license_ccurl,
        published: track.release_date,
        artwork_url: track.artwork_url,
        duration_seconds: Some(track.duration_seconds),
        playback_url: Some(track.audio_stream_url),
        webpage_url: Some(track.share_url),
        status_line: "Jamendo track resolved through the official API; press Enter to play"
            .to_owned(),
    }
}

#[cfg(any(feature = "soundstream", feature = "litres", feature = "jamendo"))]
fn append_metadata(description: Option<String>, metadata: Vec<String>) -> String {
    let mut parts = description
        .filter(|value| !value.trim().is_empty())
        .into_iter()
        .collect::<Vec<_>>();
    parts.extend(
        metadata
            .into_iter()
            .filter(|value| !value.trim().is_empty()),
    );
    parts.join("\n")
}

#[cfg(feature = "jamendo")]
fn jamendo_track_id(url: &url::Url) -> Result<String, String> {
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(
            "Jamendo track links must use credential-free HTTPS without a custom port".to_owned(),
        );
    }
    let host = url.host_str().unwrap_or_default();
    let mut segments = url
        .path_segments()
        .ok_or_else(|| "Jamendo track link must have path segments".to_owned())?
        .filter(|segment| !segment.is_empty());
    let prefix = segments.next();
    let track_id = segments.next();
    let remaining = segments.count();
    let expected_prefix = match host {
        "jamendo.com" | "www.jamendo.com" => "track",
        "jamen.do" | "www.jamen.do" => "t",
        _ => {
            return Err(
                "Jamendo direct lookup accepts jamendo.com track links and jamen.do short links"
                    .to_owned(),
            );
        }
    };
    if prefix != Some(expected_prefix) || remaining > 1 {
        return Err(format!(
            "expected an official Jamendo /{expected_prefix}/{{track-id}} link"
        ));
    }
    let track_id = track_id.unwrap_or_default();
    if track_id.is_empty()
        || track_id.len() > 20
        || !track_id.bytes().all(|byte| byte.is_ascii_digit())
        || track_id.bytes().all(|byte| byte == b'0')
    {
        return Err("Jamendo track ID must be a positive decimal integer".to_owned());
    }
    Ok(track_id.to_owned())
}

#[cfg(feature = "tracker-music")]
fn tracker_item_from_provider(item: crate::providers::tracker::TrackerSearchResult) -> TrackerItem {
    use crate::providers::tracker::TrackerMediaAccess;

    TrackerItem {
        source: item.source.descriptor().display_name.to_owned(),
        title: item.title,
        subtitle: format!(
            "{}{}{}",
            item.artist.unwrap_or_default(),
            item.format
                .as_ref()
                .map_or_else(String::new, |format| format!(" · {format}")),
            item.size_bytes
                .map_or_else(String::new, |size| format!(" · {}", human_bytes(size)))
        ),
        webpage_url: item.webpage_url,
        download_url: item.download_url,
        expected_format: item.format,
        access: match item.access {
            TrackerMediaAccess::DirectModule => TrackerAccess::DirectModule,
            TrackerMediaAccess::ArchiveNeedsInspection => TrackerAccess::ArchiveNeedsInspection,
            TrackerMediaAccess::MetadataOnly => TrackerAccess::MetadataOnly,
            TrackerMediaAccess::Directory => TrackerAccess::Directory,
        },
        prepared_path: None,
        insecure_transport: item.insecure_transport,
    }
}

fn local_media_item(path: PathBuf) -> LocalMediaItem {
    let size_bytes = std::fs::metadata(&path)
        .map(|metadata| metadata.len())
        .unwrap_or_default();
    let title = path
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .filter(|title| !title.is_empty())
        .unwrap_or("untitled local media")
        .to_owned();
    let codec = path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_ascii_uppercase)
        .unwrap_or_else(|| "unknown".to_owned());
    let mut item = LocalMediaItem {
        path,
        title,
        artist: None,
        album: None,
        genre: None,
        comment: None,
        metadata_url: None,
        acoustid_id: None,
        duration_seconds: None,
        size_bytes,
        codec,
        bitrate_kbps: None,
        sample_rate_hz: None,
        channels: None,
        embedded_artwork: false,
    };
    read_local_tags(&mut item);
    item
}

/// Selects `~/Music` when it exists, otherwise the user's home directory.
fn default_local_browse_root() -> PathBuf {
    directories::BaseDirs::new().map_or_else(
        || PathBuf::from("."),
        |directories| {
            let music = directories.home_dir().join("Music");
            if music.is_dir() {
                music
            } else {
                directories.home_dir().to_owned()
            }
        },
    )
}

#[cfg(feature = "local")]
fn read_local_tags(item: &mut LocalMediaItem) {
    use lofty::config::ParseOptions;
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::probe::Probe;
    use lofty::tag::{Accessor, ItemKey};

    if item
        .path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("flac"))
        && read_local_flac_tags(item)
    {
        return;
    }

    let options = ParseOptions::new()
        .read_properties(true)
        .read_cover_art(false);
    let Ok(tagged) = Probe::open(&item.path)
        .map(|probe| probe.options(options))
        .and_then(Probe::read)
    else {
        return;
    };
    let properties = tagged.properties();
    item.duration_seconds =
        (!properties.duration().is_zero()).then(|| properties.duration().as_secs());
    item.bitrate_kbps = properties.audio_bitrate().or(properties.overall_bitrate());
    item.sample_rate_hz = properties.sample_rate();
    item.channels = properties.channels();
    item.codec = format!("{:?}", tagged.file_type());

    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        if let Some(title) = trimmed_tag_value(tag.title().as_deref()) {
            item.title = title;
        }
        item.artist = trimmed_tag_value(tag.get_string(ItemKey::TrackArtists))
            .or_else(|| trimmed_tag_value(tag.artist().as_deref()));
        item.album = trimmed_tag_value(tag.album().as_deref());
        item.genre = trimmed_tag_value(tag.genre().as_deref());
        item.comment = trimmed_tag_value(tag.comment().as_deref());
        item.metadata_url = local_standard_tag_url(tag);
        item.acoustid_id = trimmed_tag_value(tag.get_string(ItemKey::AcoustId));
    }
}

#[cfg(not(feature = "local"))]
fn read_local_tags(_item: &mut LocalMediaItem) {}

#[cfg(feature = "local")]
fn read_local_flac_tags(item: &mut LocalMediaItem) -> bool {
    use lofty::config::ParseOptions;
    use lofty::file::AudioFile;
    use lofty::flac::FlacFile;

    let Ok(mut file) = std::fs::File::open(&item.path) else {
        return false;
    };
    let options = ParseOptions::new()
        .read_properties(true)
        .read_cover_art(false);
    let Ok(flac) = FlacFile::read_from(&mut file, options) else {
        return false;
    };
    let properties = flac.properties();
    item.duration_seconds =
        (!properties.duration().is_zero()).then(|| properties.duration().as_secs());
    item.bitrate_kbps = (properties.audio_bitrate() > 0)
        .then(|| properties.audio_bitrate())
        .or_else(|| (properties.overall_bitrate() > 0).then(|| properties.overall_bitrate()));
    item.sample_rate_hz = (properties.sample_rate() > 0).then(|| properties.sample_rate());
    item.channels = (properties.channels() > 0).then(|| properties.channels());
    item.codec = "FLAC".to_owned();

    if let Some(tag) = flac.vorbis_comments() {
        apply_local_vorbis_comments(item, tag);
    }
    true
}

#[cfg(feature = "local")]
fn apply_local_vorbis_comments(item: &mut LocalMediaItem, tag: &lofty::ogg::VorbisComments) {
    if let Some(title) = trimmed_tag_value(tag.get("TITLE")) {
        item.title = title;
    }
    item.artist = joined_tag_values(tag.get_all("ARTISTS"))
        .or_else(|| joined_tag_values(tag.get_all("ARTIST")));
    item.album = trimmed_tag_value(tag.get("ALBUM"));
    item.genre = joined_tag_values(tag.get_all("GENRE"));
    item.comment = joined_tag_values(tag.get_all("COMMENT"));
    item.metadata_url = trimmed_tag_value(tag.get("URL"));
    item.acoustid_id = trimmed_tag_value(tag.get("ACOUSTID_ID"));
}

#[cfg(feature = "local")]
fn trimmed_tag_value(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(feature = "local")]
fn joined_tag_values<'a>(values: impl Iterator<Item = &'a str>) -> Option<String> {
    let values = values
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    (!values.is_empty()).then(|| values.join("; "))
}

#[cfg(feature = "local")]
fn local_standard_tag_url(tag: &lofty::tag::Tag) -> Option<String> {
    use lofty::tag::ItemKey;

    [
        ItemKey::AudioFileUrl,
        ItemKey::AudioSourceUrl,
        ItemKey::TrackArtistUrl,
        ItemKey::PublisherUrl,
        ItemKey::CommercialInformationUrl,
        ItemKey::CopyrightUrl,
        ItemKey::PodcastUrl,
        ItemKey::RadioStationUrl,
    ]
    .into_iter()
    .find_map(|key| trimmed_tag_value(tag.get_string(key)))
}

fn local_media_subtitle(item: &LocalMediaItem) -> String {
    let mut fields = Vec::new();
    if let Some(artist) = &item.artist {
        fields.push(artist.clone());
    }
    if let Some(album) = &item.album {
        fields.push(album.clone());
    }
    if let Some(duration) = item.duration_seconds {
        fields.push(format_seconds(duration));
    }
    fields.push(item.codec.clone());
    if let Some(bitrate) = item.bitrate_kbps {
        fields.push(format!("{bitrate} kbps"));
    }
    if let Some(sample_rate) = item.sample_rate_hz {
        fields.push(format!("{sample_rate} Hz"));
    }
    if let Some(channels) = item.channels {
        fields.push(format!("{channels} ch"));
    }
    fields.push(human_bytes(item.size_bytes));
    if item.embedded_artwork {
        fields.push("embedded artwork".to_owned());
    }
    fields.join(" · ")
}

/// Formats human and technical metadata for one playable Local Details panel.
fn local_media_description(item: &LocalMediaItem) -> String {
    let mut lines = vec![format!("Full path: {}", item.path.display())];
    if let Some(artist) = &item.artist {
        lines.push(format!("Artists: {artist}"));
    }
    if let Some(album) = &item.album {
        lines.push(format!("Album: {album}"));
    }
    if let Some(genre) = &item.genre {
        lines.push(format!("Genre: {genre}"));
    }
    if let Some(comment) = &item.comment {
        lines.push(format!("Comment: {comment}"));
    }
    if let Some(url) = &item.metadata_url {
        lines.push(format!("URL: {url}"));
    }
    if let Some(acoustid_id) = &item.acoustid_id {
        lines.push("Identifiers:".to_owned());
        lines.push(format!("AcoustID ID: {acoustid_id}"));
    }
    lines.push(format!("Codec: {}", item.codec));
    lines.push(format!("Size: {}", human_bytes(item.size_bytes)));
    if let Some(value) = item.bitrate_kbps {
        lines.push(format!("Bitrate: {value} kb/s"));
    }
    if let Some(value) = item.sample_rate_hz {
        lines.push(format!("Sample rate: {value} Hz"));
    }
    if let Some(value) = item.channels {
        lines.push(format!("Channels: {value}"));
    }
    lines.join("\n")
}

fn scan_local_media(root: &Path) -> Result<Vec<LocalMediaItem>, String> {
    const MAX_VISITED_ENTRIES: usize = 100_000;
    const MAX_MEDIA_FILES: usize = 10_000;
    const MAX_DEPTH: usize = 64;

    let mut stack = vec![(root.to_owned(), 0_usize)];
    let mut media = Vec::new();
    let mut visited = 0_usize;
    while let Some((directory, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            continue;
        }
        let entries = std::fs::read_dir(&directory)
            .map_err(|error| format!("cannot read {}: {error}", directory.display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("cannot read {}: {error}", directory.display()))?;
            visited = visited.saturating_add(1);
            if visited > MAX_VISITED_ENTRIES {
                return Err(format!(
                    "scan stopped after {MAX_VISITED_ENTRIES} filesystem entries"
                ));
            }
            let file_type = entry
                .file_type()
                .map_err(|error| format!("cannot inspect {}: {error}", entry.path().display()))?;
            if file_type.is_dir() {
                stack.push((entry.path(), depth.saturating_add(1)));
            } else if file_type.is_file()
                && is_supported_media_path(&entry.path().to_string_lossy())
            {
                media.push(local_media_item(entry.path()));
                if media.len() >= MAX_MEDIA_FILES {
                    media.sort_by(|left, right| left.path.cmp(&right.path));
                    return Ok(media);
                }
            }
        }
    }
    media.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(media)
}

/// Presentation context for one YouTube result row.
///
/// A subscription feed omits channel facts already expressed by its heading.
#[derive(Clone, Copy)]
enum SearchRowContext {
    /// A cross-channel result on the global Search screen.
    GlobalSearch,
    /// A playable track returned by the dedicated `YouTube Music` tab.
    YouTubeMusic,
    /// A video inside one locally subscribed channel's feed.
    SubscriptionFeed,
}

/// Converts one provider item into a compact row for its presentation context.
fn row_from_search_item(
    item: &SearchItem,
    store: &StateStore,
    subscriptions: &SubscriptionTree,
    channel_subscribers: &HashMap<String, Option<u64>>,
    context: SearchRowContext,
    today: NaiveDate,
) -> RowView {
    match item {
        SearchItem::Video(video) => {
            let progress = store
                .progress(&MediaId::new(SourceKind::YouTube, &video.video_id))
                .ok()
                .flatten();
            let subscriber_count = match context {
                SearchRowContext::GlobalSearch => channel_subscribers
                    .get(&video.channel_id)
                    .copied()
                    .flatten(),
                SearchRowContext::YouTubeMusic | SearchRowContext::SubscriptionFeed => None,
            };
            RowView {
                media_id: Some(MediaId::new(SourceKind::YouTube, &video.video_id)),
                title: video.title.clone(),
                subtitle: youtube_video_row_subtitle(video, subscriber_count, context, today),
                source: if matches!(context, SearchRowContext::YouTubeMusic) {
                    "YouTube Music".to_owned()
                } else {
                    "YouTube".to_owned()
                },
                watched_percent: progress.map_or(0, |value| value.watched_percent()),
                subscribed: matches!(context, SearchRowContext::GlobalSearch)
                    && subscriptions.contains_youtube_channel(&video.channel_id),
                thumbnail_url: preferred_thumbnail_url(&video.thumbnails),
                vertical: video.orientation == VideoOrientation::Vertical,
                compact: false,
            }
        }
        SearchItem::Channel(channel) => RowView {
            title: channel.name.clone(),
            subtitle: channel.subscriber_count.map_or_else(
                || "channel".to_owned(),
                |count| format!("{} subscribers", format_count(count)),
            ),
            source: "YouTube channel".to_owned(),
            subscribed: subscriptions.contains_youtube_channel(&channel.channel_id),
            thumbnail_url: preferred_thumbnail_url(&channel.thumbnails),
            ..RowView::default()
        },
    }
}

/// Formats context-aware metadata beneath one YouTube video title.
///
/// Global Search retains the channel identity and selected-only subscriber
/// count. A subscription feed omits both because its heading already owns
/// those facts.
fn youtube_video_row_subtitle(
    video: &VideoSummary,
    subscriber_count: Option<u64>,
    context: SearchRowContext,
    today: NaiveDate,
) -> String {
    let mut fields = Vec::with_capacity(4);
    let channel_name = video.channel_name.trim();
    if matches!(
        context,
        SearchRowContext::GlobalSearch | SearchRowContext::YouTubeMusic
    ) && !channel_name.is_empty()
    {
        fields.push(channel_name.to_owned());
    }
    if matches!(context, SearchRowContext::GlobalSearch)
        && let Some(count) = subscriber_count
    {
        fields.push(format!("{} subscribers", format_count(count)));
    }
    if let Some(timestamp) = video.published_at {
        let date = format_unix_local_date_relative(timestamp, today);
        if date != "unknown" {
            fields.push(date);
        }
    }
    if let Some(duration) = video.duration_seconds {
        fields.push(format_seconds(duration));
    }
    fields.join(" · ")
}

#[cfg(feature = "youtube-music")]
fn search_item_from_youtube_music_track(track: YouTubeMusicTrack) -> SearchItem {
    SearchItem::Video(VideoSummary {
        video_id: track.video_id,
        title: track.title,
        channel_name: track.artist.unwrap_or_else(|| "YouTube Music".to_owned()),
        channel_id: String::new(),
        description: String::new(),
        duration_seconds: track.duration_seconds,
        view_count: None,
        published_at: None,
        published_text: None,
        live: false,
        orientation: VideoOrientation::Unknown,
        thumbnails: vec![Thumbnail {
            url: track.thumbnail_url,
            quality: Some("YouTube Music search".to_owned()),
            width: None,
            height: None,
        }],
        webpage_url: Some(track.webpage_url),
        stream_url: None,
    })
}

/// Returns bounded, clickable timestamp spans for a Details description.
fn detail_timecodes(description: &str) -> Vec<DetailTimecodeView> {
    const MAX_DESCRIPTION_TIMECODES: usize = 512;

    let chapter_timestamps = parse_description_chapters(description, None)
        .into_iter()
        .map(|chapter| (chapter.timestamp_start_byte, chapter.timestamp_end_byte))
        .collect::<HashSet<_>>();
    parse_description_links(description)
        .into_iter()
        .filter_map(|link| match link.target {
            LinkTarget::Timecode { seconds } => Some(DetailTimecodeView {
                start_byte: link.start_byte,
                end_byte: link.end_byte,
                seconds,
                is_chapter: chapter_timestamps.contains(&(link.start_byte, link.end_byte)),
            }),
            _ => None,
        })
        .take(MAX_DESCRIPTION_TIMECODES)
        .collect()
}

/// Returns bounded internal actions for `YouTube` video URLs in Details text.
fn detail_video_links(description: &str) -> Vec<DetailVideoLinkView> {
    const MAX_DESCRIPTION_VIDEO_LINKS: usize = 256;

    parse_description_video_links(description)
        .into_iter()
        .take(MAX_DESCRIPTION_VIDEO_LINKS)
        .map(|link| DetailVideoLinkView {
            start_byte: link.start_byte,
            end_byte: link.end_byte,
            video_id: link.video_id,
            start_seconds: link.start_seconds,
        })
        .collect()
}

/// Converts line-leading description timecodes into bounded playback chapters.
fn description_chapters(description: &str, duration_seconds: Option<u64>) -> Vec<Chapter> {
    const MAX_DESCRIPTION_CHAPTERS: usize = 512;

    parse_description_chapters(description, duration_seconds)
        .into_iter()
        .take(MAX_DESCRIPTION_CHAPTERS)
        .map(|chapter| Chapter {
            title: chapter.title,
            start_seconds: chapter.start_seconds,
            end_seconds: chapter.end_seconds,
        })
        .collect()
}

fn preliminary_detail(item: &SearchItem, subscriptions: &SubscriptionTree) -> DetailView {
    match item {
        SearchItem::Video(video) => {
            let description = normalize_description_chapter_lines(&video.description);
            DetailView {
                media_id: Some(MediaId::new(SourceKind::YouTube, &video.video_id)),
                title: video.title.clone(),
                source: "YouTube".to_owned(),
                channel_name: video.channel_name.clone(),
                channel_id: video.channel_id.clone(),
                channel_webpage_url: canonical_youtube_channel_url(&video.channel_id),
                channel_subscribed: subscriptions.contains_youtube_channel(&video.channel_id),
                length: video
                    .duration_seconds
                    .map_or_else(|| "unknown".to_owned(), format_seconds),
                timecodes: detail_timecodes(&description),
                video_links: detail_video_links(&description),
                description,
                views: video
                    .view_count
                    .map_or_else(|| "unknown".to_owned(), format_count),
                published: video
                    .published_text
                    .clone()
                    .or_else(|| video.published_at.map(format_unix_utc_date))
                    .unwrap_or_else(|| "unknown".to_owned()),
                license: "loading…".to_owned(),
                wikidata: "not loaded".to_owned(),
                thumbnail_url: preferred_thumbnail_url(&video.thumbnails),
                ..DetailView::default()
            }
        }
        SearchItem::Channel(channel) => detail_from_channel(channel, subscriptions),
    }
}

fn preferred_thumbnail_url(thumbnails: &[Thumbnail]) -> Option<url::Url> {
    const TARGET_WIDTH: u32 = 480;
    const TARGET_HEIGHT: u32 = 270;

    let known_dimensions = thumbnails
        .iter()
        .filter_map(|thumbnail| {
            thumbnail
                .width
                .zip(thumbnail.height)
                .map(|(width, height)| (thumbnail, width, height))
        })
        .collect::<Vec<_>>();
    let selected = known_dimensions
        .iter()
        .filter(|(_, width, height)| *width >= TARGET_WIDTH && *height >= TARGET_HEIGHT)
        .min_by_key(|(_, width, height)| u64::from(*width) * u64::from(*height))
        .map(|(thumbnail, _, _)| *thumbnail)
        .or_else(|| {
            known_dimensions
                .iter()
                .max_by_key(|(_, width, height)| u64::from(*width) * u64::from(*height))
                .map(|(thumbnail, _, _)| *thumbnail)
        })
        .or_else(|| thumbnails.last());
    selected.map(|thumbnail| thumbnail.url.clone())
}

/// Compacts selected-only channel metadata before process-local caching.
fn compact_channel_summary(channel: &mut ChannelSummary) {
    compact_cached_string(&mut channel.channel_id, 128);
    compact_cached_string(&mut channel.name, MAX_CACHED_SUBSCRIPTION_LABEL_BYTES);
    compact_cached_string(
        &mut channel.description,
        MAX_CACHED_SUBSCRIPTION_DESCRIPTION_BYTES,
    );
    channel
        .webpage_url
        .take_if(|url| url.as_str().len() > MAX_CACHED_SUBSCRIPTION_FIELD_BYTES);
    let preferred_thumbnail = preferred_thumbnail_url(&channel.thumbnails);
    channel.thumbnails.retain(|thumbnail| {
        preferred_thumbnail.as_ref() == Some(&thumbnail.url)
            && thumbnail.url.as_str().len() <= MAX_CACHED_SUBSCRIPTION_FIELD_BYTES
    });
    channel.thumbnails.truncate(1);
    channel.thumbnails.shrink_to_fit();
    if let Some(thumbnail) = channel.thumbnails.first_mut()
        && let Some(quality) = thumbnail.quality.as_mut()
    {
        compact_cached_string(quality, 128);
    }
}

/// Compacts provider list metadata before it enters the low-RAM channel cache.
fn compact_subscription_item(item: &mut SearchItem) {
    let SearchItem::Video(video) = item else {
        return;
    };
    compact_cached_string(&mut video.video_id, 128);
    compact_cached_string(&mut video.channel_id, 128);
    compact_cached_string(&mut video.title, MAX_CACHED_SUBSCRIPTION_LABEL_BYTES);
    compact_cached_string(&mut video.channel_name, MAX_CACHED_SUBSCRIPTION_LABEL_BYTES);
    compact_cached_string(
        &mut video.description,
        MAX_CACHED_SUBSCRIPTION_DESCRIPTION_BYTES,
    );
    if let Some(published) = video.published_text.as_mut() {
        compact_cached_string(published, MAX_CACHED_SUBSCRIPTION_FIELD_BYTES);
    }
    video
        .webpage_url
        .take_if(|url| url.as_str().len() > MAX_CACHED_SUBSCRIPTION_FIELD_BYTES);
    video
        .stream_url
        .take_if(|url| url.as_str().len() > MAX_CACHED_SUBSCRIPTION_FIELD_BYTES);

    let preferred_thumbnail = preferred_thumbnail_url(&video.thumbnails);
    video.thumbnails.retain(|thumbnail| {
        preferred_thumbnail.as_ref() == Some(&thumbnail.url)
            && thumbnail.url.as_str().len() <= MAX_CACHED_SUBSCRIPTION_FIELD_BYTES
    });
    video.thumbnails.truncate(1);
    video.thumbnails.shrink_to_fit();
    if let Some(thumbnail) = video.thumbnails.first_mut()
        && let Some(quality) = thumbnail.quality.as_mut()
    {
        compact_cached_string(quality, 128);
    }
}

/// Truncates one cached string at a UTF-8 boundary and releases spare capacity.
fn compact_cached_string(value: &mut String, maximum_bytes: usize) {
    truncate_utf8_bytes(value, maximum_bytes);
    value.shrink_to_fit();
}

/// Estimates heap ownership for enforcing the global subscription-cache cap.
fn subscription_item_estimated_heap_bytes(item: &SearchItem) -> usize {
    let base = std::mem::size_of::<SearchItem>();
    match item {
        SearchItem::Video(video) => {
            let strings = [
                video.video_id.capacity(),
                video.title.capacity(),
                video.channel_name.capacity(),
                video.channel_id.capacity(),
                video.description.capacity(),
                video.published_text.as_ref().map_or(0, String::capacity),
                video
                    .webpage_url
                    .as_ref()
                    .map_or(0, |url| url.as_str().len()),
                video
                    .stream_url
                    .as_ref()
                    .map_or(0, |url| url.as_str().len()),
            ]
            .into_iter()
            .fold(0usize, usize::saturating_add);
            let thumbnails = video
                .thumbnails
                .iter()
                .map(|thumbnail| {
                    thumbnail
                        .url
                        .as_str()
                        .len()
                        .saturating_add(thumbnail.quality.as_ref().map_or(0, String::capacity))
                })
                .fold(
                    video
                        .thumbnails
                        .capacity()
                        .saturating_mul(std::mem::size_of::<Thumbnail>()),
                    usize::saturating_add,
                );
            base.saturating_add(strings).saturating_add(thumbnails)
        }
        SearchItem::Channel(channel) => {
            let strings = channel
                .channel_id
                .capacity()
                .saturating_add(channel.name.capacity())
                .saturating_add(channel.description.capacity())
                .saturating_add(
                    channel
                        .webpage_url
                        .as_ref()
                        .map_or(0, |url| url.as_str().len()),
                );
            base.saturating_add(strings)
        }
    }
}

fn detail_from_channel(channel: &ChannelSummary, subscriptions: &SubscriptionTree) -> DetailView {
    DetailView {
        title: channel.name.clone(),
        source: "YouTube channel".to_owned(),
        channel_name: channel.name.clone(),
        channel_id: channel.channel_id.clone(),
        channel_webpage_url: youtube_channel_webpage_url(
            &channel.channel_id,
            channel.webpage_url.clone(),
        ),
        channel_subscribed: subscriptions.contains_youtube_channel(&channel.channel_id),
        channel_subscriber_count: channel.subscriber_count,
        description: channel.description.clone(),
        license: "not applicable".to_owned(),
        wikidata: "not loaded".to_owned(),
        thumbnail_url: preferred_thumbnail_url(&channel.thumbnails),
        ..DetailView::default()
    }
}

fn detail_from_video(video: &VideoDetails, subscriptions: &SubscriptionTree) -> DetailView {
    let description = normalize_description_chapter_lines(&video.description);
    DetailView {
        media_id: Some(MediaId::new(SourceKind::YouTube, &video.video_id)),
        title: video.title.clone(),
        source: "YouTube".to_owned(),
        channel_name: video.channel_name.clone(),
        channel_id: video.channel_id.clone(),
        channel_webpage_url: canonical_youtube_channel_url(&video.channel_id),
        channel_subscribed: subscriptions.contains_youtube_channel(&video.channel_id),
        channel_subscriber_count: None,
        length: video
            .duration_seconds
            .map_or_else(|| "unknown".to_owned(), format_seconds),
        timecodes: detail_timecodes(&description),
        video_links: detail_video_links(&description),
        description,
        likes: video
            .like_count
            .map_or_else(|| "unknown".to_owned(), format_count),
        views: video
            .view_count
            .map_or_else(|| "unknown".to_owned(), format_count),
        published: video
            .published_text
            .clone()
            .or_else(|| video.published_at.map(format_unix_utc_date))
            .unwrap_or_else(|| "unknown".to_owned()),
        license: video
            .license
            .clone()
            .unwrap_or_else(|| "unknown".to_owned()),
        wikidata: "not loaded (lazy)".to_owned(),
        thumbnail_url: preferred_thumbnail_url(&video.thumbnails),
        links: Vec::new(),
        ..DetailView::default()
    }
}

fn detail_from_media_item(media: &MediaItem) -> DetailView {
    let description = media.description.clone().unwrap_or_default();
    DetailView {
        media_id: Some(media.id.clone()),
        title: media.title.clone(),
        source: media.id.source.to_string(),
        channel_name: media.creator.clone().unwrap_or_default(),
        length: media
            .duration_seconds
            .map_or_else(String::new, format_seconds),
        description: description.clone(),
        timecodes: detail_timecodes(&description),
        video_links: detail_video_links(&description),
        likes: media
            .statistics
            .likes
            .map_or_else(String::new, format_count),
        views: media
            .statistics
            .views
            .map_or_else(String::new, format_count),
        published: media
            .published_at
            .map_or_else(String::new, format_unix_utc_date),
        license: match &media.license {
            MediaLicense::CreativeCommons(label) => label.clone(),
            _ => String::new(),
        },
        thumbnail_url: media.thumbnail_url.clone(),
        ..DetailView::default()
    }
}

fn summary_from_details(video: &VideoDetails) -> VideoSummary {
    VideoSummary {
        video_id: video.video_id.clone(),
        title: video.title.clone(),
        channel_name: video.channel_name.clone(),
        channel_id: video.channel_id.clone(),
        description: video.description.clone(),
        duration_seconds: video.duration_seconds,
        view_count: video.view_count,
        published_at: video.published_at,
        published_text: video.published_text.clone(),
        live: video.live,
        orientation: video.orientation,
        thumbnails: video.thumbnails.clone(),
        webpage_url: video.webpage_url.clone(),
        stream_url: video.stream_url.clone(),
    }
}

fn subscription_source_row(entry: &FlattenedSubscription) -> RowView {
    RowView {
        title: format!("{}{}", "  ".repeat(entry.depth), entry.subscription.title),
        subtitle: entry.subscription.url.to_string(),
        source: subscription_kind_label(entry.subscription.kind).to_owned(),
        subscribed: true,
        ..RowView::default()
    }
}

const fn subscription_kind_label(kind: SubscriptionKind) -> &'static str {
    match kind {
        SubscriptionKind::YouTube => "YouTube channel",
        SubscriptionKind::Rss => "RSS podcast",
        SubscriptionKind::Other => "Subscription",
    }
}

fn moved_index(current: usize, length: usize, delta: i32) -> Option<usize> {
    if length == 0 {
        return None;
    }
    let last = length.saturating_sub(1);
    Some(if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs() as usize)
    } else {
        current.saturating_add(delta as usize).min(last)
    })
}

/// Builds a canonical channel page only from a bounded path-safe identifier.
fn canonical_youtube_channel_url(channel_id: &str) -> Option<url::Url> {
    if channel_id.is_empty()
        || channel_id.len() > 128
        || !channel_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return None;
    }
    let mut url = url::Url::parse("https://www.youtube.com/channel/").ok()?;
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .push(channel_id);
    Some(url)
}

/// Accepts a provider channel page from a strict YouTube allowlist.
///
/// Unsafe, unrelated, or mismatched pages fall back to the canonical channel
/// identifier instead of reaching `xdg-open`.
fn youtube_channel_webpage_url(channel_id: &str, preferred: Option<url::Url>) -> Option<url::Url> {
    preferred
        .filter(|url| is_safe_youtube_channel_webpage(url, channel_id))
        .or_else(|| canonical_youtube_channel_url(channel_id))
}

/// Checks that a preferred URL is an exact credential-free YouTube channel page.
fn is_safe_youtube_channel_webpage(url: &url::Url, channel_id: &str) -> bool {
    if url.scheme() != "https"
        || !matches!(url.host_str(), Some("youtube.com" | "www.youtube.com"))
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    let Some(segments) = url.path_segments() else {
        return false;
    };
    let segments = segments.collect::<Vec<_>>();
    match segments.as_slice() {
        [handle] => handle
            .strip_prefix('@')
            .is_some_and(|handle| !handle.is_empty() && handle.len() <= 128),
        ["channel", candidate] => {
            canonical_youtube_channel_url(candidate).is_some()
                && (channel_id.is_empty() || *candidate == channel_id)
        }
        ["c" | "user", legacy_name] => !legacy_name.is_empty() && legacy_name.len() <= 128,
        _ => false,
    }
}

fn search_item_url(item: &SearchItem) -> Option<String> {
    match item {
        SearchItem::Video(video) => Some(
            video
                .webpage_url
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| youtube_video_url(&video.video_id)),
        ),
        SearchItem::Channel(channel) => channel
            .webpage_url
            .clone()
            .or_else(|| canonical_youtube_channel_url(&channel.channel_id))
            .map(|url| url.to_string()),
    }
}

fn stored_screen_from_tui(screen: Screen) -> StoredScreen {
    match screen {
        Screen::Search => StoredScreen::Search,
        #[cfg(feature = "youtube-music")]
        Screen::YouTubeMusic => StoredScreen::YouTubeMusic,
        #[cfg(not(feature = "youtube-music"))]
        Screen::YouTubeMusic => StoredScreen::Search,
        Screen::Local => StoredScreen::Local,
        Screen::Subscriptions => StoredScreen::Subscriptions,
        Screen::Downloaded => StoredScreen::Downloaded,
        Screen::History => StoredScreen::History,
        Screen::Playlists => StoredScreen::Playlists,
        Screen::Statistics => StoredScreen::Statistics,
        Screen::TrackerMusic => StoredScreen::TrackerMusic,
    }
}

fn tui_screen_from_stored(screen: &StoredScreen) -> Screen {
    match screen {
        StoredScreen::Search => Screen::Search,
        #[cfg(feature = "youtube-music")]
        StoredScreen::YouTubeMusic => Screen::YouTubeMusic,
        #[cfg(not(feature = "youtube-music"))]
        StoredScreen::YouTubeMusic => Screen::Search,
        StoredScreen::Local => Screen::Local,
        StoredScreen::Subscriptions => Screen::Subscriptions,
        StoredScreen::Downloaded => Screen::Downloaded,
        StoredScreen::History => Screen::History,
        StoredScreen::Playlists | StoredScreen::Playlist(_) | StoredScreen::Queue => {
            Screen::Playlists
        }
        StoredScreen::Statistics => Screen::Statistics,
        StoredScreen::TrackerMusic => Screen::TrackerMusic,
        StoredScreen::Channel(_) | StoredScreen::Waveform => Screen::Search,
    }
}

fn youtube_video_url(video_id: &str) -> String {
    format!("https://www.youtube.com/watch?v={video_id}")
}

#[cfg(feature = "yt-dlp")]
fn configured_download_format(value: &str) -> Result<DownloadFormat, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "opus" | "opus-copy" => Ok(DownloadFormat::OpusWithoutTranscoding),
        "original" | "best" | "best-audio" => Ok(DownloadFormat::OriginalBestAudio),
        "transcode-opus" | "opus-transcode" => Ok(DownloadFormat::TranscodeToOpus),
        value => Err(format!(
            "unsupported subscriptions.audio_format `{value}`; use `opus`, `original`, or `transcode-opus`"
        )),
    }
}

#[cfg(feature = "yt-dlp")]
fn prepare_download_destination(config: &Config) -> Result<PathBuf, String> {
    config
        .ensure_directories()
        .map_err(|error| format!("cannot prepare Youta's private directories: {error}"))?;
    let destination = config.downloads_dir();
    let root = std::fs::canonicalize(config.config_dir())
        .map_err(|error| format!("cannot resolve the Youta config directory: {error}"))?;
    let destination = std::fs::canonicalize(&destination)
        .map_err(|error| format!("cannot resolve the downloads directory: {error}"))?;
    if destination == root || !destination.starts_with(&root) {
        return Err(format!(
            "the downloads directory resolves outside the Youta config directory: {}",
            destination.display()
        ));
    }
    Ok(destination)
}

#[cfg(feature = "yt-dlp")]
fn validate_completed_download_path(
    destination: &Path,
    reported_path: &Path,
) -> Result<PathBuf, String> {
    let candidate = if reported_path.is_absolute() {
        reported_path.to_owned()
    } else {
        destination.join(reported_path)
    };
    let candidate = std::fs::canonicalize(&candidate).map_err(|error| {
        format!(
            "cannot resolve the completed media path `{}`: {error}",
            candidate.display()
        )
    })?;
    if !candidate.starts_with(destination) {
        return Err(format!(
            "yt-dlp reported a path outside Youta's downloads directory: {}",
            candidate.display()
        ));
    }
    if !candidate.is_file() {
        return Err(format!(
            "yt-dlp reported a completed path that is not a file: {}",
            candidate.display()
        ));
    }
    Ok(candidate)
}

#[cfg(feature = "yt-dlp")]
fn apply_download_progress(view: &mut ViewModel, title: &str, progress: DownloadProgress) {
    view.download = Some(DownloadView {
        title: title.to_owned(),
        downloaded_bytes: progress.downloaded_bytes,
        total_bytes: progress.total_bytes,
        bytes_per_second: progress.bytes_per_second.map(rounded_download_rate),
        eta_seconds: progress.eta_seconds,
        active: true,
        completed_path: None,
    });
}

#[cfg(feature = "yt-dlp")]
fn mark_download_inactive(view: &mut ViewModel) {
    if let Some(download) = view.download.as_mut() {
        download.active = false;
    }
}

#[cfg(feature = "yt-dlp")]
fn rounded_download_rate(value: f64) -> u64 {
    match Duration::try_from_secs_f64(value) {
        Ok(duration) => duration
            .as_secs()
            .saturating_add(u64::from(duration.subsec_nanos() >= 500_000_000)),
        Err(_) if value.is_sign_negative() || value.is_nan() => 0,
        Err(_) => u64::MAX,
    }
}

#[cfg(feature = "yt-dlp")]
fn download_diagnostics(active: &ActiveDownload) -> String {
    active
        .output
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .diagnostics()
}

#[cfg(feature = "yt-dlp")]
fn append_download_diagnostics(message: String, diagnostics: &str) -> String {
    if diagnostics.is_empty() {
        message
    } else {
        format!("{message}\n\nBounded yt-dlp output:\n{diagnostics}")
    }
}

fn queue_item_from_video(video: &VideoSummary, start_at_seconds: Option<u64>) -> QueueItem {
    let canonical_playback_url = url::Url::parse(&youtube_video_url(&video.video_id))
        .expect("a validated YouTube video ID always forms a valid URL");
    let webpage_url = video
        .webpage_url
        .clone()
        .unwrap_or_else(|| canonical_playback_url.clone());
    // Provider-advertised CDN and proxy URLs can be signed, short-lived, or
    // tied to request headers. Let yt-dlp resolve the stable watch page at the
    // moment playback starts instead of persisting such a locator in the queue.
    let playback_location = canonical_playback_url.to_string();
    QueueItem {
        media: MediaItem {
            id: MediaId::new(SourceKind::YouTube, video.video_id.clone()),
            kind: if video.live {
                MediaKind::LiveStream
            } else {
                MediaKind::Video
            },
            title: video.title.clone(),
            creator: (!video.channel_name.is_empty()).then(|| video.channel_name.clone()),
            description: (!video.description.is_empty()).then(|| video.description.clone()),
            webpage_url,
            thumbnail_url: video
                .thumbnails
                .first()
                .map(|thumbnail| thumbnail.url.clone()),
            duration_seconds: video.duration_seconds,
            published_at: video.published_at,
            statistics: MediaStatistics {
                views: video.view_count,
                likes: None,
            },
            license: MediaLicense::Unknown,
            chapters: description_chapters(&video.description, video.duration_seconds),
            captions: Vec::new(),
        },
        playback_location,
        start_at_seconds,
        added_at: unix_time(),
    }
}

/// Returns whether a YouTube result can start without representing a scheduled
/// zero-duration placeholder.
fn video_is_autoplay_playable(video: &VideoSummary) -> bool {
    video.duration_seconds != Some(0) || video.live
}

/// Matches the stable local path encoded in a queue media identifier.
fn local_path_matches_media_id(path: &Path, media_id: &MediaId) -> bool {
    media_id.source == SourceKind::Local && path.display().to_string() == media_id.external_id
}

/// Matches a prepared tracker row without relying on a possibly duplicated
/// extracted filename.
fn tracker_item_matches_media_id(item: &TrackerItem, media_id: &MediaId) -> bool {
    media_id.source == SourceKind::ModArchive && item.webpage_url.as_str() == media_id.external_id
}

fn queue_item_from_direct(direct: &DirectSourceInput) -> QueueItem {
    QueueItem {
        media: MediaItem {
            id: MediaId::new(direct.source.clone(), direct.url.to_string()),
            kind: media_kind_for_source(&direct.source, direct.url.path()),
            title: direct.url.to_string(),
            creator: None,
            description: None,
            webpage_url: direct.url.clone(),
            thumbnail_url: None,
            duration_seconds: None,
            published_at: None,
            statistics: MediaStatistics::default(),
            license: MediaLicense::Unknown,
            chapters: Vec::new(),
            captions: Vec::new(),
        },
        playback_location: direct.url.to_string(),
        start_at_seconds: None,
        added_at: unix_time(),
    }
}

fn queue_item_from_resolved(media: &ResolvedDirectMedia) -> Result<QueueItem, String> {
    let playback_url = media
        .playback_url
        .clone()
        .ok_or_else(|| media.status_line.clone())?;
    let webpage_url = media
        .webpage_url
        .clone()
        .unwrap_or_else(|| playback_url.clone());
    Ok(QueueItem {
        media: MediaItem {
            id: MediaId::new(media.source.clone(), media.external_id.clone()),
            kind: media_kind_for_source(&media.source, playback_url.path()),
            title: media.title.clone(),
            creator: None,
            description: (!media.description.is_empty()).then(|| media.description.clone()),
            webpage_url,
            thumbnail_url: media.artwork_url.clone(),
            duration_seconds: media.duration_seconds,
            published_at: None,
            statistics: MediaStatistics::default(),
            license: media_license_from_label(&media.license),
            chapters: description_chapters(&media.description, media.duration_seconds),
            captions: Vec::new(),
        },
        playback_location: playback_url.to_string(),
        start_at_seconds: None,
        added_at: unix_time(),
    })
}

fn queue_item_from_local(item: &LocalMediaItem) -> Result<QueueItem, String> {
    let webpage_url = url::Url::from_file_path(&item.path).map_err(|()| {
        format!(
            "Local path cannot be represented as a file URL: {}",
            item.path.display()
        )
    })?;
    Ok(QueueItem {
        media: MediaItem {
            id: MediaId::new(SourceKind::Local, item.path.display().to_string()),
            kind: media_kind_for_source(&SourceKind::Local, &item.path.to_string_lossy()),
            title: item.title.clone(),
            creator: item.artist.clone(),
            description: item.album.as_ref().map(|album| format!("Album: {album}")),
            webpage_url,
            thumbnail_url: None,
            duration_seconds: item.duration_seconds,
            published_at: None,
            statistics: MediaStatistics::default(),
            license: MediaLicense::Unknown,
            chapters: Vec::new(),
            captions: Vec::new(),
        },
        playback_location: item.path.display().to_string(),
        start_at_seconds: None,
        added_at: unix_time(),
    })
}

fn queue_item_from_tracker(item: &TrackerItem) -> Result<QueueItem, String> {
    let prepared_path = item
        .prepared_path
        .as_ref()
        .ok_or_else(|| match item.access {
            TrackerAccess::MetadataOnly => {
                "This tracker result exposes metadata only; press o to open its source page"
                    .to_owned()
            }
            TrackerAccess::Directory => {
                "This tracker result is a directory and cannot be played".to_owned()
            }
            TrackerAccess::DirectModule | TrackerAccess::ArchiveNeedsInspection => {
                "This tracker result must be prepared before playback".to_owned()
            }
        })?;
    Ok(QueueItem {
        media: MediaItem {
            id: MediaId::new(SourceKind::ModArchive, item.webpage_url.to_string()),
            kind: MediaKind::Audio,
            title: item.title.clone(),
            creator: None,
            description: (!item.subtitle.is_empty()).then(|| item.subtitle.clone()),
            webpage_url: item.webpage_url.clone(),
            thumbnail_url: None,
            duration_seconds: None,
            published_at: None,
            statistics: MediaStatistics::default(),
            license: MediaLicense::Unknown,
            chapters: Vec::new(),
            captions: Vec::new(),
        },
        playback_location: prepared_path.display().to_string(),
        start_at_seconds: None,
        added_at: unix_time(),
    })
}

fn media_kind_for_source(source: &SourceKind, path: &str) -> MediaKind {
    if matches!(
        source,
        SourceKind::ApplePodcasts | SourceKind::Rss | SourceKind::SoundStream | SourceKind::LitRes
    ) {
        MediaKind::PodcastEpisode
    } else if matches!(source, SourceKind::BbcRadio | SourceKind::Radio) {
        MediaKind::LiveStream
    } else if is_video_media_path(path)
        || matches!(
            source,
            SourceKind::YouTube
                | SourceKind::Vimeo
                | SourceKind::RuTube
                | SourceKind::PeerTube
                | SourceKind::Bilibili
                | SourceKind::Rumble
                | SourceKind::Odysee
        )
    {
        MediaKind::Video
    } else {
        MediaKind::Audio
    }
}

fn is_video_media_path(path: &str) -> bool {
    let without_query = path.split(['?', '#']).next().unwrap_or(path);
    let extension = Path::new(without_query)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "webm" | "mkv" | "mp4" | "m4v" | "mov" | "avi"
    )
}

fn media_license_from_label(label: &str) -> MediaLicense {
    let normalized = label.trim();
    let lowercase = normalized.to_ascii_lowercase();
    if normalized.is_empty() || lowercase == "unknown" {
        MediaLicense::Unknown
    } else if lowercase.contains("creativecommons.org") || lowercase.contains("creative commons") {
        MediaLicense::CreativeCommons(normalized.to_owned())
    } else if lowercase.contains("public domain") {
        MediaLicense::PublicDomain
    } else if lowercase.contains("youtube standard") {
        MediaLicense::YouTubeStandard
    } else {
        MediaLicense::Other(normalized.to_owned())
    }
}

fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let separator_count = digits.len().saturating_sub(1) / 3;
    let mut formatted = String::with_capacity(digits.len().saturating_add(separator_count));
    for (index, digit) in digits.bytes().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(char::from(digit));
    }
    formatted
}

/// Prefixes a channel-supplied link label with its recognized destination.
fn channel_external_link_label(kind: ChannelExternalLinkKind, label: &str) -> String {
    let destination = match kind {
        ChannelExternalLinkKind::Website => "Website",
        ChannelExternalLinkKind::Telegram => "Telegram",
        ChannelExternalLinkKind::Facebook => "Facebook",
        ChannelExternalLinkKind::XTwitter => "X/Twitter",
        ChannelExternalLinkKind::TikTok => "TikTok",
        ChannelExternalLinkKind::Instagram => "Instagram",
        ChannelExternalLinkKind::YouTube => "YouTube",
    };
    if label.eq_ignore_ascii_case(destination) {
        destination.to_owned()
    } else {
        format!("{destination}: {label}")
    }
}

fn format_unix_utc_date(timestamp: i64) -> String {
    const SECONDS_PER_DAY: i64 = 24 * 60 * 60;
    const MIN_SUPPORTED_TIMESTAMP: i64 = -62_135_596_800;
    const MAX_SUPPORTED_TIMESTAMP: i64 = 253_402_300_799;

    if !(MIN_SUPPORTED_TIMESTAMP..=MAX_SUPPORTED_TIMESTAMP).contains(&timestamp) {
        return "unknown".to_owned();
    }
    let days_since_epoch = timestamp.div_euclid(SECONDS_PER_DAY);
    let (year, month, day) = civil_date_from_unix_days(days_since_epoch);
    format_civil_date(year, month, day).unwrap_or_else(|| "unknown".to_owned())
}

/// Formats one publication timestamp in the user's local calendar.
///
/// Relative labels are computed from injected `today`, which keeps date
/// boundaries deterministic in tests and avoids consulting the clock per row.
fn format_unix_local_date_relative(timestamp: i64, today: NaiveDate) -> String {
    let Some(published) =
        DateTime::from_timestamp(timestamp, 0).map(|date| date.with_timezone(&Local).date_naive())
    else {
        return "unknown".to_owned();
    };
    let Some(mut formatted) = format_civil_date(
        i64::from(published.year()),
        i64::from(published.month()),
        i64::from(published.day()),
    ) else {
        return "unknown".to_owned();
    };
    if published == today {
        formatted.push_str(" (today)");
    } else if today.pred_opt() == Some(published) {
        formatted.push_str(" (yesterday)");
    }
    formatted
}

/// Formats a validated civil date with an English month name.
fn format_civil_date(year: i64, month: i64, day: i64) -> Option<String> {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];

    let month_name = month
        .checked_sub(1)
        .and_then(|index| usize::try_from(index).ok())
        .and_then(|index| MONTHS.get(index))?;
    (1..=31)
        .contains(&day)
        .then(|| format!("{year} {month_name} {day}"))
}

// Converts a day offset from 1970-01-01 using Howard Hinnant's civil-date
// algorithm. Euclidean division keeps dates before the Unix epoch correct.
fn civil_date_from_unix_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let shifted_days = days_since_epoch.saturating_add(719_468);
    let era = shifted_days.div_euclid(146_097);
    let day_of_era = shifted_days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn format_seconds(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn human_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format_binary_size(bytes, GIB, "GiB")
    } else if bytes >= MIB {
        format_binary_size(bytes, MIB, "MiB")
    } else if bytes >= KIB {
        format_binary_size(bytes, KIB, "KiB")
    } else {
        format!("{bytes} B")
    }
}

fn format_binary_size(bytes: u64, unit: u64, suffix: &str) -> String {
    let hundredths = (u128::from(bytes) * 100 + u128::from(unit / 2)) / u128::from(unit);
    format!("{}.{:02} {suffix}", hundredths / 100, hundredths % 100)
}

#[cfg(feature = "wikidata")]
fn apply_wikidata_links(details: &mut DetailView, items: &[crate::domain::WikidataLink]) {
    details.wikidata.clear();
    details.links.retain(|link| link.wikidata_item_id.is_none());
    details
        .links
        .extend(items.iter().map(|item| DetailLinkView {
            label: if item.label == item.item_id {
                item.item_id.clone()
            } else {
                format!("{} ({})", item.label, item.item_id)
            },
            url: item.url.to_string(),
            wikidata_item_id: Some(item.item_id.clone()),
        }));
}

#[cfg(feature = "wikidata")]
fn apply_wikidata_entity_to_details(
    details: &mut DetailView,
    entity: &crate::providers::wikidata::WikidataEntityStatements,
) {
    let view = format_wikidata_entity(entity);
    if let Some(existing) = details
        .wikidata_entities
        .iter_mut()
        .find(|cached| cached.item_id == entity.item_id)
    {
        *existing = view;
    } else {
        details.wikidata_entities.push(view);
    }
    if details.loading_wikidata_item.as_deref() == Some(entity.item_id.as_str()) {
        details.loading_wikidata_item = None;
    }
}

#[cfg(feature = "wikidata")]
fn format_wikidata_entity(
    entity: &crate::providers::wikidata::WikidataEntityStatements,
) -> DetailWikidataEntityView {
    let mut text = String::new();
    let mut value_links = Vec::new();
    let mut image_url = None;
    if entity.statements.is_empty() {
        text.push_str("No human-facing statements were found.");
    } else {
        for statement in &entity.statements {
            text.push_str(&format!(
                "{} ({}): ",
                statement.property_label, statement.property_id
            ));
            for (index, value) in statement.values.iter().enumerate() {
                if index > 0 {
                    text.push_str("; ");
                }
                let start_byte = text.len();
                if let Some(item_id) = value.item_id.as_ref() {
                    if value.display == *item_id {
                        text.push_str(item_id);
                    } else {
                        text.push_str(&format!("{} ({item_id})", value.display));
                    }
                } else {
                    text.push_str(&value.display);
                }
                let target_url = value
                    .item_id
                    .as_deref()
                    .and_then(canonical_wikidata_item_url)
                    .or_else(|| value.external_url.as_ref().map(url::Url::to_string));
                if let Some(url) = target_url {
                    value_links.push(DetailWikidataValueLinkView {
                        start_byte,
                        end_byte: text.len(),
                        url,
                    });
                }
                if statement.property_id == "P18" && image_url.is_none() {
                    image_url = value.preview_url.clone();
                }
            }
            text.push('\n');
        }
        while text.ends_with('\n') {
            text.pop();
        }
    }
    if entity.hard_bounds_reached {
        text.push_str(
            "\n\nHard bounds were reached: 256 properties, 128 values per property, \
             1,024 total values, or 4 KiB per displayed value or label.",
        );
    }
    if entity.unsupported_values_omitted {
        text.push_str(
            "\n\nEmpty values or structured value types that Youta cannot display as one \
             terminal field were omitted.",
        );
    }
    DetailWikidataEntityView {
        item_id: entity.item_id.clone(),
        text,
        value_links,
        image_url,
    }
}

/// Builds a canonical Wikidata page only for a bounded positive Q identifier.
fn canonical_wikidata_item_url(item_id: &str) -> Option<String> {
    let digits = item_id.strip_prefix('Q')?;
    (item_id.len() <= 32
        && !digits.is_empty()
        && !digits.starts_with('0')
        && digits.bytes().all(|byte| byte.is_ascii_digit()))
    .then(|| format!("https://www.wikidata.org/wiki/{item_id}"))
}

fn unix_time() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or_default()
}

fn playback_end_message(end: &PlaybackEnd) -> String {
    playback_end_details(end)
        .unwrap_or_else(|| "mpv reported a playback error without additional details".to_owned())
}

fn playback_end_reports_http_403(end: &PlaybackEnd) -> bool {
    [
        end.error.as_deref(),
        end.file_error.as_deref(),
        end.diagnostic.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::to_ascii_lowercase)
    .any(|message| {
        message.contains("http error 403")
            || message.contains("http 403")
            || message.contains("403 forbidden")
    })
}

fn playback_before_start_message(end: &PlaybackEnd) -> String {
    let summary = "mpv reached the end of the media before reporting that audio playback \
                   started. The source may be empty, unavailable, or unsupported.";
    playback_end_details(end).map_or_else(
        || summary.to_owned(),
        |details| format!("{summary}\n\n{details}"),
    )
}

fn playback_before_start_message_for_media(end: &PlaybackEnd, tracker_module: bool) -> String {
    let message = playback_before_start_message(end);
    if tracker_module && playback_end_reports_unsupported_format(end) {
        tracker_decoder_help(&message)
    } else {
        message
    }
}

fn playback_end_reports_unsupported_format(end: &PlaybackEnd) -> bool {
    [
        end.error.as_deref(),
        end.file_error.as_deref(),
        end.diagnostic.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::to_ascii_lowercase)
    .any(|message| {
        message.contains("unrecognized file format")
            || message.contains("unsupported format")
            || message.contains("no audio or video streams selected")
    })
}

fn tracker_decoder_help(details: &str) -> String {
    format!(
        "The selected tracker module was downloaded and inspected correctly, but this mpv/FFmpeg \
         build cannot decode it. On Gentoo, enable tracker-module support (typically FFmpeg's \
         `openmpt` USE flag), rebuild FFmpeg/mpv, then retry.\n\n{details}"
    )
}

fn playback_end_details(end: &PlaybackEnd) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(error) = end.error.as_deref() {
        parts.push(format!("Backend error: {error}"));
    }
    if let Some(file_error) = end.file_error.as_deref() {
        parts.push(format!("Media error: {file_error}"));
    }
    if let Some(diagnostic) = end.diagnostic.as_deref() {
        parts.push(format!("mpv diagnostics:\n{diagnostic}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// Returns whether a path is located under Youta's configured write root.
///
/// This helper is used by tests and future import actions before creating
/// derivative files.
#[must_use]
pub fn is_confined_path(root: &Path, candidate: &Path) -> bool {
    candidate.starts_with(root)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    #[cfg(feature = "yt-dlp")]
    use std::io::Cursor;
    #[cfg(feature = "yt-dlp")]
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    fn subscription_video_summary() -> VideoSummary {
        VideoSummary {
            video_id: "dQw4w9WgXcQ".to_owned(),
            title: "Fixture video".to_owned(),
            channel_name: "Fixture channel".to_owned(),
            channel_id: "UCfixture".to_owned(),
            description: "Fixture description".to_owned(),
            duration_seconds: Some(42),
            view_count: Some(7),
            published_at: Some(1_729_003_672),
            published_text: None,
            live: false,
            orientation: VideoOrientation::Unknown,
            thumbnails: Vec::new(),
            webpage_url: None,
            stream_url: None,
        }
    }

    fn subscription_video_details(title: &str) -> VideoDetails {
        VideoDetails {
            video_id: "dQw4w9WgXcQ".to_owned(),
            title: title.to_owned(),
            channel_name: "Fixture channel".to_owned(),
            channel_id: "UCfixture".to_owned(),
            description: "Fixture description".to_owned(),
            duration_seconds: Some(42),
            view_count: Some(7),
            like_count: Some(3),
            published_at: Some(1_729_003_672),
            published_text: None,
            license: Some("Standard YouTube License".to_owned()),
            rating: None,
            ratings_allowed: Some(true),
            live: false,
            keywords: Vec::new(),
            orientation: VideoOrientation::Unknown,
            thumbnails: Vec::new(),
            webpage_url: None,
            stream_url: None,
        }
    }

    fn linked_video_details(video_id: &str, title: &str, description: &str) -> VideoDetails {
        VideoDetails {
            video_id: video_id.to_owned(),
            title: title.to_owned(),
            description: description.to_owned(),
            ..subscription_video_details(title)
        }
    }

    #[test]
    fn public_counts_use_ascii_thousands_separators_at_boundaries() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1,000");
        assert_eq!(format_count(13_045), "13,045");
        assert_eq!(format_count(887_263), "887,263");
        assert_eq!(format_count(1_000_000), "1,000,000");
        assert_eq!(format_count(u64::MAX), "18,446,744,073,709,551,615");
    }

    #[test]
    fn xdg_open_receives_exactly_one_target_without_option_separator() {
        let command = url_opener_command(
            Path::new("xdg-open"),
            "https://www.youtube.com/channel/UCfixture",
        );
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            arguments,
            ["https://www.youtube.com/channel/UCfixture"],
            "`xdg-open` rejects the conventional synthetic `--` argument"
        );
    }

    #[cfg(unix)]
    #[test]
    fn xdg_open_task_reports_nonzero_exit_instead_of_false_success() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir().expect("temporary opener directory");
        let opener = temporary.path().join("xdg-open");
        std::fs::write(&opener, "#!/bin/sh\nexit 23\n").expect("opener fixture");
        std::fs::set_permissions(&opener, std::fs::Permissions::from_mode(0o700))
            .expect("executable opener fixture");
        let (sender, receiver) = unbounded();

        spawn_url_opener_task(
            opener,
            "https://www.youtube.com/channel/UCfixture".to_owned(),
            sender,
        )
        .expect("opener worker");

        assert_eq!(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("opener completion"),
            UrlOpenCompletion::Failed("xdg-open exited with status 23".to_owned())
        );
    }

    #[test]
    fn unix_dates_use_utc_calendar_dates_across_epoch_and_leap_boundaries() {
        assert_eq!(format_unix_utc_date(-1), "1969 December 31");
        assert_eq!(format_unix_utc_date(0), "1970 January 1");
        assert_eq!(format_unix_utc_date(951_827_696), "2000 February 29");
        assert_eq!(format_unix_utc_date(4_107_542_399), "2100 February 28");
        assert_eq!(format_unix_utc_date(4_107_542_400), "2100 March 1");
        assert_eq!(format_unix_utc_date(1_729_003_672), "2024 October 15");
        assert_eq!(format_unix_utc_date(1_783_209_600), "2026 July 5");
        assert_eq!(format_unix_utc_date(1_784_937_600), "2026 July 25");
    }

    #[test]
    fn unix_dates_reject_timestamps_outside_four_digit_calendar_years() {
        assert_eq!(format_unix_utc_date(i64::MIN), "unknown");
        assert_eq!(format_unix_utc_date(-62_135_596_801), "unknown");
        assert_eq!(format_unix_utc_date(253_402_300_800), "unknown");
        assert_eq!(format_unix_utc_date(i64::MAX), "unknown");
    }

    #[test]
    fn local_relative_dates_cross_month_and_year_boundaries() {
        let today = NaiveDate::from_ymd_opt(2027, 1, 1).expect("valid fixture date");
        let timestamp = |date: NaiveDate| {
            date.and_hms_opt(12, 0, 0)
                .expect("valid local noon")
                .and_local_timezone(Local)
                .single()
                .expect("local noon must be unambiguous")
                .timestamp()
        };

        assert_eq!(
            format_unix_local_date_relative(timestamp(today), today),
            "2027 January 1 (today)"
        );
        assert_eq!(
            format_unix_local_date_relative(
                timestamp(NaiveDate::from_ymd_opt(2026, 12, 31).expect("valid fixture date")),
                today,
            ),
            "2026 December 31 (yesterday)"
        );
        assert_eq!(
            format_unix_local_date_relative(
                timestamp(NaiveDate::from_ymd_opt(2026, 12, 30).expect("valid fixture date")),
                today,
            ),
            "2026 December 30"
        );
    }

    #[test]
    fn youtube_video_row_subtitle_orders_and_collapses_available_metadata() {
        let mut video = subscription_video_summary();
        let today = NaiveDate::from_ymd_opt(2026, 7, 26).expect("valid fixture date");
        video.published_at = Some(
            today
                .and_hms_opt(12, 0, 0)
                .expect("valid local noon")
                .and_local_timezone(Local)
                .single()
                .expect("local noon must be unambiguous")
                .timestamp(),
        );
        assert_eq!(
            youtube_video_row_subtitle(&video, Some(13_045), SearchRowContext::GlobalSearch, today,),
            "Fixture channel · 13,045 subscribers · 2026 July 26 (today) · 0:42"
        );
        assert_eq!(
            youtube_video_row_subtitle(
                &video,
                Some(13_045),
                SearchRowContext::SubscriptionFeed,
                today,
            ),
            "2026 July 26 (today) · 0:42",
            "a subscription heading already owns the channel name and subscriber count"
        );

        let yesterday = today.pred_opt().expect("fixture has a preceding date");
        video.published_at = Some(
            yesterday
                .and_hms_opt(12, 0, 0)
                .expect("valid local noon")
                .and_local_timezone(Local)
                .single()
                .expect("local noon must be unambiguous")
                .timestamp(),
        );
        assert_eq!(
            youtube_video_row_subtitle(&video, None, SearchRowContext::SubscriptionFeed, today,),
            "2026 July 25 (yesterday) · 0:42"
        );

        video.channel_name = "  ".to_owned();
        video.duration_seconds = None;
        video.published_at = Some(i64::MAX);
        assert_eq!(
            youtube_video_row_subtitle(&video, None, SearchRowContext::GlobalSearch, today,),
            "",
            "missing or invalid fields must not leave separators or `unknown`"
        );
    }

    #[test]
    fn subscriber_enrichment_batches_unique_video_channels_and_stays_in_ram() {
        let config = Config::for_dir("/tmp/youta-subscriber-cache-test");
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        let (requests, captured_requests) = unbounded();
        controller.provider_requests = Some(requests);
        controller.youtube_channel_statistics_mode = ChannelStatisticsMode::Batch { max_ids: 50 };
        controller.youtube_results = vec![
            SearchItem::Video(subscription_video_summary()),
            SearchItem::Video(VideoSummary {
                video_id: "abcdefghijk".to_owned(),
                title: "Second".to_owned(),
                channel_name: "Second channel".to_owned(),
                channel_id: "UCsecond".to_owned(),
                ..subscription_video_summary()
            }),
            SearchItem::Video(VideoSummary {
                video_id: "123456789ab".to_owned(),
                title: "Duplicate channel".to_owned(),
                ..subscription_video_summary()
            }),
        ];

        controller.request_visible_channel_subscriber_counts();
        let requested_ids = match captured_requests
            .recv_timeout(Duration::from_secs(1))
            .expect("subscriber request")
        {
            ProviderRequest::ChannelSubscriberCounts { channel_ids, .. } => channel_ids,
            _ => panic!("expected subscriber request"),
        };
        assert_eq!(requested_ids, ["UCfixture", "UCsecond"]);

        controller.handle_provider_response(ProviderResponse::ChannelSubscriberCounts {
            provider_generation: controller.youtube_provider_generation,
            requested_ids: requested_ids.clone(),
            result: Ok(vec![
                ChannelSubscriberCount {
                    channel_id: "UCfixture".to_owned(),
                    subscriber_count: Some(13_045),
                },
                ChannelSubscriberCount {
                    channel_id: "UCsecond".to_owned(),
                    subscriber_count: None,
                },
            ]),
        });
        assert_eq!(
            controller.channel_subscriber_cache.get("UCfixture"),
            Some(&Some(13_045))
        );
        assert_eq!(
            controller.channel_subscriber_cache.get("UCsecond"),
            Some(&None)
        );
        assert!(
            controller.view.rows[0]
                .subtitle
                .contains("13,045 subscribers")
        );
        assert!(
            !controller.view.rows[1].subtitle.contains("subscribers"),
            "hidden counts must not produce placeholder text"
        );

        controller.request_visible_channel_subscriber_counts();
        assert!(
            captured_requests.try_recv().is_err(),
            "positive and negative RAM cache entries must suppress refetches"
        );
    }

    #[test]
    fn selected_only_subscriber_mode_never_fans_out_across_search_results() {
        let config = Config::for_dir("/tmp/youta-selected-subscriber-test");
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        let (requests, captured_requests) = unbounded();
        controller.provider_requests = Some(requests);
        controller.youtube_channel_statistics_mode = ChannelStatisticsMode::SelectedOnly;
        let selected = SearchItem::Video(subscription_video_summary());
        controller.youtube_results = vec![
            selected.clone(),
            SearchItem::Video(VideoSummary {
                channel_id: "UCsecond".to_owned(),
                ..subscription_video_summary()
            }),
        ];

        controller.request_visible_channel_subscriber_counts();
        assert!(captured_requests.try_recv().is_err());
        controller.request_selected_channel_subscriber_count(&selected);
        let requested_ids = match captured_requests
            .recv_timeout(Duration::from_secs(1))
            .expect("selected-only request")
        {
            ProviderRequest::ChannelSubscriberCounts { channel_ids, .. } => channel_ids,
            _ => panic!("expected subscriber request"),
        };
        assert_eq!(requested_ids, ["UCfixture"]);
    }

    #[test]
    fn watched_percentage_is_restored_from_disk_for_search_rows() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let media_id = MediaId::new(SourceKind::YouTube, "dQw4w9WgXcQ");
        {
            let store = StateStore::open(&config).expect("disk state");
            let mut progress = PlaybackProgress::new(media_id, Some(100), 1);
            progress.record_position(95, 2);
            store.upsert_progress(&progress).expect("saved progress");
        }

        let store = StateStore::open(&config).expect("reopened disk state");
        let mut controller = AppController::new(config, store, None, None);
        controller.youtube_results = vec![SearchItem::Video(subscription_video_summary())];
        controller.refresh_youtube_rows();

        assert_eq!(controller.view.rows[0].watched_percent, 95);
    }

    #[test]
    fn saved_youtube_search_restores_without_rerunning_and_keeps_details_lazy() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let mut request = SearchRequest::new("restored fixture", SearchTarget::Videos);
        request.page = 2;
        request.sort = ProviderSearchSort::UploadDate;
        request.filters.features = vec![SearchFeature::CreativeCommons];
        let mut restored_video = subscription_video_summary();
        restored_video.stream_url =
            Some(url::Url::parse("https://streams.example/expired").expect("fixture stream URL"));
        let saved_search = SavedYouTubeSearch {
            request: request.clone(),
            results: vec![SearchItem::Video(restored_video)],
            next_page: Some(3),
        };
        {
            let store = StateStore::open(&config).expect("disk state");
            store
                .save_session(
                    &SessionState {
                        screen: StoredScreen::Search,
                        search_text: "stale session query".to_owned(),
                        ..SessionState::default()
                    },
                    1,
                )
                .expect("save session");
            store
                .save_youtube_search(&saved_search, 2)
                .expect("save search");
            #[cfg(feature = "wikidata")]
            store
                .put_cached_wikidata(&CachedWikidataLookup {
                    property_id: "P1651".to_owned(),
                    external_id: "dQw4w9WgXcQ".to_owned(),
                    items: Vec::new(),
                    fetched_at: 2,
                    expires_at: i64::MAX,
                })
                .expect("seed deterministic Wikidata cache");
        }

        let searches = Arc::new(AtomicUsize::new(0));
        let details = Arc::new(AtomicUsize::new(0));
        let store = StateStore::open(&config).expect("reopen disk state");
        let controller = AppController::new(
            config,
            store,
            Some(Box::new(CountingYouTubeProvider {
                searches: Arc::clone(&searches),
                details: Arc::clone(&details),
            })),
            None,
        );

        for _ in 0..100 {
            if details.load(Ordering::SeqCst) > 0 {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(searches.load(Ordering::SeqCst), 0);
        assert_eq!(
            details.load(Ordering::SeqCst),
            1,
            "the restored selection should retain lazy details enrichment"
        );
        let SearchItem::Video(restored_video) = &controller.youtube_results[0] else {
            panic!("restored result should be a video");
        };
        assert_eq!(restored_video.title, "Fixture video");
        assert_eq!(
            restored_video.stream_url, None,
            "an expiring stream locator must be refreshed lazily"
        );
        assert_eq!(controller.youtube_search_request, Some(request));
        assert_eq!(
            controller.next_youtube_page, None,
            "opaque Official API page tokens cannot survive a process restart"
        );
        assert_eq!(controller.view.search_query, "restored fixture");
        assert_eq!(
            controller.view.youtube_search_sort,
            YouTubeSearchSort::Newest
        );
        assert!(controller.view.youtube_creative_commons_only);
        assert_eq!(controller.view.rows.len(), 1);
        assert_eq!(
            controller.view.status_line,
            "1 saved YouTube result restored"
        );
    }

    #[cfg(feature = "youtube-music")]
    #[test]
    fn saved_youtube_music_search_restores_screen_query_rows_selection_and_drops_streams() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let music_video = |video_id: &str, title: &str| {
            let mut video = subscription_video_summary();
            video.video_id = video_id.to_owned();
            video.title = title.to_owned();
            video.webpage_url = Some(
                url::Url::parse(&format!("https://music.youtube.com/watch?v={video_id}"))
                    .expect("fixture music URL"),
            );
            video.stream_url = Some(
                url::Url::parse(&format!("https://streams.example/{video_id}.opus"))
                    .expect("fixture expiring stream URL"),
            );
            SearchItem::Video(video)
        };
        let saved_search = SavedYouTubeMusicSearch {
            query: "restored music fixture".to_owned(),
            results: vec![
                music_video("dQw4w9WgXcQ", "First restored track"),
                music_video("aqz-KE-bpKQ", "Selected restored track"),
            ],
        };
        {
            let store = StateStore::open(&config).expect("disk state");
            store
                .save_session(
                    &SessionState {
                        screen: StoredScreen::YouTubeMusic,
                        selected_row: 1,
                        youtube_music_selected_row: Some(1),
                        youtube_music_search_text: "stale session music query".to_owned(),
                        ..SessionState::default()
                    },
                    1,
                )
                .expect("save session");
            store
                .save_youtube_music_search(&saved_search, 2)
                .expect("save YouTube Music search");
        }

        let store = StateStore::open(&config).expect("reopen disk state");
        let controller = AppController::new(config, store, None, None);

        assert_eq!(controller.view.screen, Screen::YouTubeMusic);
        assert_eq!(controller.view.search_query, saved_search.query);
        assert_eq!(controller.youtube_music_search_query, saved_search.query);
        assert_eq!(controller.view.selected, 1);
        assert_eq!(controller.youtube_music_selected, 1);
        assert_eq!(controller.view.rows.len(), 2);
        assert_eq!(controller.view.rows[0].title, "First restored track");
        assert_eq!(controller.view.rows[1].title, "Selected restored track");
        assert!(
            controller.youtube_music_results.iter().all(|item| {
                matches!(item, SearchItem::Video(video) if video.stream_url.is_none())
            }),
            "restart-safe rows must discard every expiring direct stream URL"
        );
        assert_eq!(
            controller
                .view
                .details
                .as_ref()
                .map(|details| details.title.as_str()),
            Some("Selected restored track")
        );
        assert_eq!(
            controller.view.status_line,
            "2 saved YouTube Music tracks restored"
        );
        assert_eq!(controller.view.search_activity, None);
    }

    #[test]
    fn youtube_detail_conversion_formats_counts_and_fallback_publication_dates() {
        let summary = VideoSummary {
            video_id: "dQw4w9WgXcQ".to_owned(),
            title: "Fixture".to_owned(),
            channel_name: "Channel".to_owned(),
            channel_id: "UCfixture".to_owned(),
            description: String::new(),
            duration_seconds: Some(42),
            view_count: Some(887_263),
            published_at: Some(1_729_003_672),
            published_text: None,
            live: false,
            orientation: VideoOrientation::Unknown,
            thumbnails: Vec::new(),
            webpage_url: None,
            stream_url: None,
        };
        let preliminary =
            preliminary_detail(&SearchItem::Video(summary), &SubscriptionTree::default());
        assert_eq!(preliminary.views, "887,263");
        assert_eq!(preliminary.published, "2024 October 15");
        assert_eq!(
            preliminary
                .channel_webpage_url
                .as_ref()
                .map(url::Url::as_str),
            Some("https://www.youtube.com/channel/UCfixture")
        );

        let details = VideoDetails {
            video_id: "dQw4w9WgXcQ".to_owned(),
            title: "Fixture".to_owned(),
            channel_name: "Channel".to_owned(),
            channel_id: "UCfixture".to_owned(),
            description: String::new(),
            duration_seconds: Some(42),
            view_count: Some(887_263),
            like_count: Some(13_045),
            published_at: Some(1_729_003_672),
            published_text: None,
            license: Some("Creative Commons Attribution".to_owned()),
            rating: None,
            ratings_allowed: Some(true),
            live: false,
            keywords: Vec::new(),
            orientation: VideoOrientation::Unknown,
            thumbnails: Vec::new(),
            webpage_url: None,
            stream_url: None,
        };
        let rendered = detail_from_video(&details, &SubscriptionTree::default());
        assert_eq!(rendered.likes, "13,045");
        assert_eq!(rendered.views, "887,263");
        assert_eq!(rendered.published, "2024 October 15");
        assert_eq!(rendered.license, "Creative Commons Attribution");
        assert_eq!(rendered.channel_name, "Channel");
        assert_eq!(rendered.channel_id, "UCfixture");
        assert_eq!(
            rendered.channel_webpage_url.as_ref().map(url::Url::as_str),
            Some("https://www.youtube.com/channel/UCfixture")
        );
        assert!(!rendered.channel_subscribed);
    }

    #[test]
    fn channel_details_prefer_the_provider_webpage_over_a_synthesized_url() {
        let channel = ChannelSummary {
            channel_id: "UCfixture".to_owned(),
            name: "Fixture".to_owned(),
            description: "Channel description".to_owned(),
            subscriber_count: Some(10),
            video_count: Some(2),
            created_at: None,
            auto_generated: false,
            thumbnails: Vec::new(),
            webpage_url: Some(
                url::Url::parse("https://www.youtube.com/@fixture").expect("fixture channel URL"),
            ),
        };

        let details = detail_from_channel(&channel, &SubscriptionTree::default());

        assert_eq!(
            details.channel_webpage_url.as_ref().map(url::Url::as_str),
            Some("https://www.youtube.com/@fixture")
        );
    }

    #[test]
    fn channel_webpage_rejects_unsafe_provider_urls_and_channel_ids() {
        let unsafe_provider_url =
            url::Url::parse("https://youtube.com@example.org/channel/UCfixture")
                .expect("unsafe fixture URL");
        let watch_url =
            url::Url::parse("https://www.youtube.com/watch?v=fixture").expect("watch fixture URL");
        let redirect_url =
            url::Url::parse("https://www.youtube.com/redirect").expect("redirect fixture URL");
        let mismatched_channel_url = url::Url::parse("https://www.youtube.com/channel/UCother")
            .expect("mismatched channel fixture URL");

        assert_eq!(
            youtube_channel_webpage_url("UCfixture", Some(unsafe_provider_url))
                .as_ref()
                .map(url::Url::as_str),
            Some("https://www.youtube.com/channel/UCfixture")
        );
        for unsafe_url in [watch_url, redirect_url, mismatched_channel_url] {
            assert_eq!(
                youtube_channel_webpage_url("UCfixture", Some(unsafe_url))
                    .as_ref()
                    .map(url::Url::as_str),
                Some("https://www.youtube.com/channel/UCfixture")
            );
        }
        assert!(youtube_channel_webpage_url("../watch", None).is_none());
        assert!(canonical_youtube_channel_url(&"x".repeat(129)).is_none());
    }

    #[test]
    fn multilingual_description_timecodes_feed_details_and_queue_chapters() {
        let mut video = subscription_video_summary();
        video.duration_seconds = Some(4_000);
        video.description =
            "00:00 Introduction\n00:01:35 Батуми\n01:02:51 移住\ninline 01:03:00 ignored"
                .to_owned();

        let details = preliminary_detail(
            &SearchItem::Video(video.clone()),
            &SubscriptionTree::default(),
        );
        assert_eq!(
            details
                .timecodes
                .iter()
                .map(|timecode| (
                    &details.description[timecode.start_byte..timecode.end_byte],
                    timecode.seconds
                ))
                .collect::<Vec<_>>(),
            [
                ("00:00", 0),
                ("00:01:35", 95),
                ("01:02:51", 3_771),
                ("01:03:00", 3_780),
            ]
        );

        let item = queue_item_from_video(&video, None);
        assert_eq!(
            item.media
                .chapters
                .iter()
                .map(|chapter| (chapter.title.as_str(), chapter.start_seconds))
                .collect::<Vec<_>>(),
            [("Introduction", 0), ("Батуми", 95), ("移住", 3_771),],
            "only line-leading markers define seekbar chapters"
        );
        assert_eq!(item.media.chapters[1].end_seconds, Some(3_771));
        assert_eq!(item.media.chapters[2].end_seconds, Some(4_000));
    }

    #[test]
    fn description_video_navigation_restores_nested_details_and_clears_forward_history() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.youtube_provider_available = true;
        let (requests, _captured_requests) = unbounded();
        controller.provider_requests = Some(requests);

        let mut first = subscription_video_summary();
        first.title = "First list row".to_owned();
        let mut origin = subscription_video_summary();
        origin.video_id = "aaaaaaaaaaa".to_owned();
        origin.title = "Origin list row".to_owned();
        controller.youtube_results =
            vec![SearchItem::Video(first), SearchItem::Video(origin.clone())];
        controller.refresh_youtube_rows();
        controller.view.selected = 1;
        controller.view.details = Some(DetailView {
            media_id: Some(MediaId::new(SourceKind::YouTube, &origin.video_id)),
            title: "Original Details".to_owned(),
            description: "Original long description".to_owned(),
            ..DetailView::default()
        });
        controller.view.details_scroll = 7;

        controller.dispatch(UiAction::ActivateDescriptionVideo {
            video_id: "dQw4w9WgXcQ".to_owned(),
            start_seconds: Some(45),
        });
        let first_generation = controller.details_generation;
        controller.handle_provider_response(ProviderResponse::Details {
            generation: first_generation,
            result: Ok(linked_video_details(
                "dQw4w9WgXcQ",
                "Linked A",
                "Nested https://youtu.be/9bZkp7q19f0?t=12",
            )),
        });
        assert_eq!(
            controller
                .view
                .details
                .as_ref()
                .map(|details| details.title.as_str()),
            Some("Linked A")
        );
        assert_eq!(
            controller
                .view
                .details
                .as_ref()
                .map(|details| details.video_links.len()),
            Some(1)
        );
        assert_eq!(
            controller.current_url().as_deref(),
            Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=45")
        );
        assert_eq!(controller.view.selected, 1);
        assert_eq!(controller.view.screen, Screen::Search);
        controller.view.details_scroll = 4;

        controller.dispatch(UiAction::ActivateDescriptionVideo {
            video_id: "9bZkp7q19f0".to_owned(),
            start_seconds: Some(12),
        });
        let second_generation = controller.details_generation;
        controller.handle_provider_response(ProviderResponse::Details {
            generation: second_generation,
            result: Ok(linked_video_details(
                "9bZkp7q19f0",
                "Linked B",
                "Nested destination",
            )),
        });
        assert_eq!(controller.detail_navigation_back.len(), 2);
        assert!(controller.detail_navigation_forward.is_empty());

        controller.dispatch(UiAction::GoBack);
        assert_eq!(
            controller
                .view
                .details
                .as_ref()
                .map(|details| details.title.as_str()),
            Some("Linked A")
        );
        assert_eq!(controller.view.details_scroll, 4);
        assert_eq!(
            controller
                .active_description_video
                .as_ref()
                .map(|linked| (linked.video_id.as_str(), linked.start_seconds)),
            Some(("dQw4w9WgXcQ", Some(45)))
        );

        controller.dispatch(UiAction::GoBack);
        assert_eq!(
            controller
                .view
                .details
                .as_ref()
                .map(|details| details.title.as_str()),
            Some("Original Details")
        );
        assert_eq!(controller.view.details_scroll, 7);
        assert_eq!(controller.view.selected, 1);
        assert_eq!(controller.view.screen, Screen::Search);
        assert!(controller.active_description_video.is_none());

        controller.dispatch(UiAction::GoForward);
        controller.dispatch(UiAction::GoForward);
        assert_eq!(
            controller
                .view
                .details
                .as_ref()
                .map(|details| details.title.as_str()),
            Some("Linked B")
        );
        assert_eq!(
            controller
                .active_description_video
                .as_ref()
                .map(|linked| linked.video_id.as_str()),
            Some("9bZkp7q19f0")
        );

        controller.dispatch(UiAction::GoBack);
        assert_eq!(controller.detail_navigation_forward.len(), 1);
        controller.dispatch(UiAction::ActivateDescriptionVideo {
            video_id: "M7lc1UVf-VE".to_owned(),
            start_seconds: None,
        });
        assert!(
            controller.detail_navigation_forward.is_empty(),
            "a new nested navigation must discard the old forward branch"
        );
        assert_eq!(
            controller
                .active_description_video
                .as_ref()
                .map(|linked| linked.video_id.as_str()),
            Some("M7lc1UVf-VE")
        );
    }

    #[test]
    fn description_video_navigation_history_is_bounded() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.youtube_provider_available = true;
        let (requests, _captured_requests) = unbounded();
        controller.provider_requests = Some(requests);
        controller.view.details = Some(DetailView {
            title: "Starting Details".to_owned(),
            ..DetailView::default()
        });

        for index in 0..MAX_DETAIL_NAVIGATION_HISTORY.saturating_add(5) {
            controller.dispatch(UiAction::ActivateDescriptionVideo {
                video_id: format!("A{index:010}"),
                start_seconds: None,
            });
        }

        assert_eq!(
            controller.detail_navigation_back.len(),
            MAX_DETAIL_NAVIGATION_HISTORY
        );
        assert!(controller.detail_navigation_forward.is_empty());
    }

    #[test]
    fn forwarding_to_a_pending_link_reissues_details_and_rejects_the_stale_response() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.youtube_provider_available = true;
        let (requests, _captured_requests) = unbounded();
        controller.provider_requests = Some(requests);
        controller.view.details = Some(DetailView {
            title: "Origin".to_owned(),
            description: "Origin description".to_owned(),
            ..DetailView::default()
        });

        controller.dispatch(UiAction::ActivateDescriptionVideo {
            video_id: "dQw4w9WgXcQ".to_owned(),
            start_seconds: None,
        });
        let stale_generation = controller.details_generation;
        controller.dispatch(UiAction::GoBack);
        controller.dispatch(UiAction::GoForward);
        let resumed_generation = controller.details_generation;
        assert_ne!(resumed_generation, stale_generation);

        controller.handle_provider_response(ProviderResponse::Details {
            generation: stale_generation,
            result: Ok(linked_video_details(
                "dQw4w9WgXcQ",
                "Stale response",
                "stale",
            )),
        });
        assert_eq!(
            controller
                .view
                .details
                .as_ref()
                .map(|details| details.title.as_str()),
            Some(LINKED_VIDEO_LOADING_TITLE)
        );

        controller.handle_provider_response(ProviderResponse::Details {
            generation: resumed_generation,
            result: Ok(linked_video_details(
                "dQw4w9WgXcQ",
                "Resumed response",
                "fresh",
            )),
        });
        assert_eq!(
            controller
                .view
                .details
                .as_ref()
                .map(|details| details.title.as_str()),
            Some("Resumed response")
        );
    }

    #[test]
    fn local_channel_subscription_persists_and_updates_details_and_rows() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let store = StateStore::open(&config).expect("disk state");
        let mut controller = AppController::new(config.clone(), store, None, None);
        let video = subscription_video_summary();
        controller.youtube_results = vec![SearchItem::Video(video.clone())];
        controller.view.details = Some(preliminary_detail(
            &SearchItem::Video(video),
            &controller.subscription_tree,
        ));
        controller.refresh_youtube_rows();

        controller.dispatch(UiAction::ToggleSubscription);

        assert!(
            controller
                .view
                .details
                .as_ref()
                .expect("video details")
                .channel_subscribed
        );
        assert!(controller.view.rows[0].subscribed);
        assert!(
            controller
                .subscription_tree
                .contains_youtube_channel("UCfixture")
        );
        let persisted = subscriptions::load(&config).expect("persisted subscriptions");
        assert_eq!(persisted.subscription_count(), 1);
        assert!(persisted.contains_youtube_channel("UCfixture"));
        let SubscriptionNode::Subscription(subscription) = &persisted.items[0] else {
            panic!("expected top-level YouTube subscription");
        };
        assert_eq!(subscription.title, "Fixture channel");
        assert_eq!(
            subscription.url.as_str(),
            "https://www.youtube.com/feeds/videos.xml?channel_id=UCfixture"
        );
        assert_eq!(
            subscription.website_url.as_ref().map(url::Url::as_str),
            Some("https://www.youtube.com/channel/UCfixture")
        );

        let restored = AppController::new(
            config.clone(),
            StateStore::open_in_memory().expect("second in-memory state"),
            None,
            None,
        );
        assert!(
            restored
                .subscription_tree
                .contains_youtube_channel("UCfixture")
        );
        drop(restored);

        controller.cache_subscription_video_page(
            "UCfixture",
            SearchPage {
                page: 1,
                items: vec![SearchItem::Video(subscription_video_summary())],
                next_page: None,
            },
        );
        controller
            .persist_subscription_video_snapshot("UCfixture")
            .expect("persist subscribed channel snapshot");
        controller.dispatch(UiAction::ToggleSubscription);
        assert!(
            !controller
                .view
                .details
                .as_ref()
                .expect("video details")
                .channel_subscribed
        );
        assert!(!controller.view.rows[0].subscribed);
        assert_eq!(
            subscriptions::load(&config)
                .expect("subscriptions after unsubscribe")
                .subscription_count(),
            0
        );
        assert!(
            !controller
                .subscription_video_cache
                .contains_key("UCfixture")
        );
        assert_eq!(
            controller
                .store
                .cached_subscription_items(&SourceKind::YouTube, "UCfixture")
                .expect("read deleted snapshot"),
            None
        );
        drop(controller);
        assert_eq!(
            StateStore::open(&config)
                .expect("restart disk state")
                .cached_subscription_items(&SourceKind::YouTube, "UCfixture")
                .expect("read deleted snapshot after restart"),
            None
        );
    }

    fn save_fixture_subscriptions(config: &Config, channel_ids: &[&str]) {
        let mut tree = SubscriptionTree::default();
        for (index, channel_id) in channel_ids.iter().enumerate() {
            assert!(tree.subscribe_youtube_channel(
                format!("Fixture channel {}", index.saturating_add(1)),
                channel_id,
            ));
        }
        subscriptions::save(config, &tree).expect("save fixture subscriptions");
    }

    #[test]
    fn rss_subscription_popup_validates_and_persists_portable_feed() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config.clone(), store, None, None);
        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));

        controller.dispatch(UiAction::OpenRssSubscriptionPopup);
        let popup = controller
            .view
            .rss_subscription_popup
            .as_ref()
            .expect("RSS subscription popup");
        assert_eq!(
            popup.config_path,
            config.subscriptions_file().display().to_string()
        );
        controller
            .view
            .rss_subscription_popup
            .as_mut()
            .expect("RSS subscription popup")
            .url = "file:///tmp/not-a-network-feed.xml".to_owned();
        controller.dispatch(UiAction::SubmitRssSubscription);
        assert!(
            controller
                .view
                .rss_subscription_popup
                .as_ref()
                .and_then(|popup| popup.validation_error.as_deref())
                .is_some_and(|error| error.contains("HTTP or HTTPS"))
        );

        controller
            .view
            .rss_subscription_popup
            .as_mut()
            .expect("retained RSS subscription popup")
            .url = "https://podcasts.example/feed.xml".to_owned();
        controller.dispatch(UiAction::SubmitRssSubscription);
        assert!(controller.view.rss_subscription_popup.is_none());
        let persisted = subscriptions::load(&config).expect("persisted RSS subscription");
        assert_eq!(persisted.subscription_count(), 1);
        let SubscriptionNode::Subscription(subscription) = &persisted.items[0] else {
            panic!("expected top-level RSS subscription");
        };
        assert_eq!(subscription.kind, SubscriptionKind::Rss);
        assert_eq!(
            subscription.url.as_str(),
            "https://podcasts.example/feed.xml"
        );
        assert_eq!(controller.view.subscriptions.sources.len(), 1);
    }

    fn receive_channel_request(
        captured_requests: &Receiver<ProviderRequest>,
    ) -> (u64, ChannelVideosRequest) {
        for _ in 0..8 {
            if let ProviderRequest::ChannelVideos {
                generation,
                request,
            } = captured_requests
                .recv_timeout(Duration::from_secs(1))
                .expect("channel videos request")
            {
                return (generation, request);
            }
        }
        panic!("provider queue did not contain a channel videos request");
    }

    fn assert_no_channel_request(captured_requests: &Receiver<ProviderRequest>, context: &str) {
        while let Ok(request) = captured_requests.try_recv() {
            assert!(
                !matches!(request, ProviderRequest::ChannelVideos { .. }),
                "{context}"
            );
        }
    }

    #[test]
    fn drill_down_subscriptions_load_only_after_enter_and_tab_returns_to_sources() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        save_fixture_subscriptions(&config, &["UCfixture"]);
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.youtube_provider_available = true;
        let (requests, captured_requests) = unbounded();
        controller.provider_requests = Some(requests);

        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        assert_eq!(
            controller.view.subscriptions.route,
            SubscriptionRoute::Sources
        );
        assert_eq!(controller.view.subscriptions.sources.len(), 1);
        assert!(
            captured_requests.try_recv().is_err(),
            "drill-down mode must not spend quota before a source is activated"
        );
        assert_eq!(
            controller.current_channel_url().as_deref(),
            Some("https://www.youtube.com/channel/UCfixture")
        );

        controller.dispatch(UiAction::ActivateSelection);
        let (generation, request) = match captured_requests
            .recv_timeout(Duration::from_secs(1))
            .expect("channel videos request")
        {
            ProviderRequest::ChannelVideos {
                generation,
                request,
            } => (generation, request),
            _ => panic!("expected channel videos request"),
        };
        assert_eq!(request.channel_id, "UCfixture");
        assert_eq!(request.page, 1);
        assert_eq!(
            controller.view.subscriptions.route,
            SubscriptionRoute::Items
        );

        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation,
            request,
            result: Ok(SearchPage {
                page: 1,
                items: vec![SearchItem::Video(subscription_video_summary())],
                next_page: None,
            }),
        });
        assert_eq!(controller.view.subscriptions.items.len(), 1);
        assert_eq!(
            controller
                .view
                .details
                .as_ref()
                .and_then(|details| details.media_id.as_ref())
                .map(|media_id| media_id.external_id.as_str()),
            Some("dQw4w9WgXcQ")
        );
        assert!(matches!(
            captured_requests
                .recv_timeout(Duration::from_secs(1))
                .expect("lazy details request"),
            ProviderRequest::Details { .. }
        ));

        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        assert_eq!(
            controller.view.subscriptions.route,
            SubscriptionRoute::Sources,
            "global Tab's screen action must always return to the subscriptions root"
        );
        assert_no_channel_request(
            &captured_requests,
            "returning to a drill-down root must not refetch the channel",
        );
    }

    #[test]
    fn subscriptions_restore_disk_snapshot_before_refresh_and_reconcile_removed_videos() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        save_fixture_subscriptions(&config, &["UCfixture"]);
        let mut first_summary = subscription_video_summary();
        first_summary.stream_url = Some(
            url::Url::parse("https://streams.example/signed?token=private-fixture")
                .expect("fixture expiring stream URL"),
        );
        let first = SearchItem::Video(first_summary);
        let mut removed = subscription_video_summary();
        removed.video_id = "M7lc1UVf-VE".to_owned();
        removed.title = "Removed fixture video".to_owned();
        {
            let store = StateStore::open(&config).expect("initial disk state");
            store
                .put_cached_subscription_items(&CachedSubscriptionItems {
                    source: SourceKind::YouTube,
                    source_id: "UCfixture".to_owned(),
                    items: vec![first.clone(), SearchItem::Video(removed)],
                    fetched_at: 1,
                })
                .expect("seed subscription snapshot");
        }

        let store = StateStore::open(&config).expect("reopened disk state");
        let mut controller = AppController::new(config, store, None, None);
        controller.youtube_provider_available = true;
        let (requests, captured_requests) = unbounded();
        controller.provider_requests = Some(requests);

        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        assert_no_channel_request(&captured_requests, "the source root must remain quota-free");
        controller.dispatch(UiAction::ActivateSelection);
        assert_eq!(
            controller
                .view
                .subscriptions
                .items
                .iter()
                .map(|row| row.title.as_str())
                .collect::<Vec<_>>(),
            ["Fixture video", "Removed fixture video"],
            "the restart snapshot must render before the provider responds"
        );
        assert!(matches!(
            controller
                .subscription_video_cache
                .get("UCfixture")
                .and_then(|cached| cached.items.first()),
            Some(SearchItem::Video(video)) if video.stream_url.is_none()
        ));
        let (generation, request) = receive_channel_request(&captured_requests);
        assert_eq!(
            request.page, 1,
            "opening must refresh the cached first page"
        );

        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation,
            request,
            result: Ok(SearchPage {
                page: 1,
                items: vec![first],
                next_page: None,
            }),
        });
        assert_eq!(
            controller
                .view
                .subscriptions
                .items
                .iter()
                .map(|row| row.title.as_str())
                .collect::<Vec<_>>(),
            ["Fixture video"],
            "the refreshed page must remove provider-deleted rows"
        );
        let persisted = controller
            .store
            .cached_subscription_items(&SourceKind::YouTube, "UCfixture")
            .expect("read refreshed snapshot")
            .expect("refreshed snapshot");
        assert_eq!(persisted.items.len(), 1);
        assert!(matches!(
            persisted.items.first(),
            Some(SearchItem::Video(video))
                if video.video_id == "dQw4w9WgXcQ" && video.stream_url.is_none()
        ));
    }

    #[test]
    fn subscription_initial_empty_page_persists_first_later_playable_page() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        save_fixture_subscriptions(&config, &["UCfixture"]);
        let store = StateStore::open(&config).expect("initial disk state");
        let mut controller = AppController::new(config.clone(), store, None, None);
        controller.youtube_provider_available = true;
        let (requests, captured_requests) = unbounded();
        controller.provider_requests = Some(requests);

        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        controller.dispatch(UiAction::ActivateSelection);
        let (generation, request) = receive_channel_request(&captured_requests);
        let mut scheduled = subscription_video_summary();
        scheduled.video_id = "scheduled00".to_owned();
        scheduled.duration_seconds = Some(0);
        scheduled.live = false;
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation,
            request,
            result: Ok(SearchPage {
                page: 1,
                items: vec![SearchItem::Video(scheduled)],
                next_page: Some(2),
            }),
        });

        let (generation, request) = receive_channel_request(&captured_requests);
        assert_eq!(request.page, 2);
        let playable = subscription_video_summary();
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation,
            request,
            result: Ok(SearchPage {
                page: 2,
                items: vec![SearchItem::Video(playable)],
                next_page: None,
            }),
        });
        let persisted = controller
            .store
            .cached_subscription_items(&SourceKind::YouTube, "UCfixture")
            .expect("load first playable snapshot")
            .expect("first playable snapshot");
        assert!(matches!(
            persisted.items.as_slice(),
            [SearchItem::Video(video)] if video.video_id == "dQw4w9WgXcQ"
        ));
        drop(controller);

        let store = StateStore::open(&config).expect("reopen disk state");
        let mut restarted = AppController::new(config, store, None, None);
        restarted.youtube_provider_available = true;
        let (requests, _captured_requests) = unbounded();
        restarted.provider_requests = Some(requests);
        restarted.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        restarted.dispatch(UiAction::ActivateSelection);
        assert_eq!(
            restarted
                .view
                .subscriptions
                .items
                .iter()
                .map(|row| row.title.as_str())
                .collect::<Vec<_>>(),
            ["Fixture video"]
        );
    }

    #[test]
    fn subscription_disk_snapshot_survives_failure_and_clears_after_empty_success() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        save_fixture_subscriptions(&config, &["UCfixture"]);
        {
            let store = StateStore::open(&config).expect("initial disk state");
            store
                .put_cached_subscription_items(&CachedSubscriptionItems {
                    source: SourceKind::YouTube,
                    source_id: "UCfixture".to_owned(),
                    items: vec![SearchItem::Video(subscription_video_summary())],
                    fetched_at: 1,
                })
                .expect("seed subscription snapshot");
        }
        let store = StateStore::open(&config).expect("reopen disk state");
        let mut controller = AppController::new(config.clone(), store, None, None);
        controller.youtube_provider_available = true;
        let (requests, captured_requests) = unbounded();
        controller.provider_requests = Some(requests);

        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        controller.dispatch(UiAction::ActivateSelection);
        let (generation, request) = receive_channel_request(&captured_requests);
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation,
            request,
            result: Err("temporary provider failure".to_owned()),
        });
        assert_eq!(controller.view.subscriptions.items.len(), 1);
        assert_eq!(
            controller
                .store
                .cached_subscription_items(&SourceKind::YouTube, "UCfixture")
                .expect("read preserved snapshot")
                .expect("preserved snapshot")
                .items
                .len(),
            1
        );

        controller.view.error_popup = None;
        controller.dispatch(UiAction::RefreshSubscriptionVideos);
        let (generation, request) = receive_channel_request(&captured_requests);
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation,
            request,
            result: Ok(SearchPage {
                page: 1,
                items: Vec::new(),
                next_page: None,
            }),
        });
        assert!(controller.view.subscriptions.items.is_empty());
        assert!(
            controller
                .store
                .cached_subscription_items(&SourceKind::YouTube, "UCfixture")
                .expect("read cleared snapshot")
                .expect("authoritative empty snapshot")
                .items
                .is_empty()
        );
        drop(controller);

        let store = StateStore::open(&config).expect("second restart disk state");
        let mut restarted = AppController::new(config, store, None, None);
        restarted.youtube_provider_available = true;
        let (requests, _captured_requests) = unbounded();
        restarted.provider_requests = Some(requests);
        restarted.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        restarted.dispatch(UiAction::ActivateSelection);
        assert!(restarted.view.subscriptions.items.is_empty());
        assert!(
            restarted
                .subscription_video_cache
                .get("UCfixture")
                .is_some_and(|cached| cached.items.is_empty())
        );
    }

    #[test]
    fn subscription_refresh_bypasses_cache_and_preserves_video_identity() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        save_fixture_subscriptions(&config, &["UCfixture"]);
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.youtube_provider_available = true;
        let (requests, captured_requests) = unbounded();
        controller.provider_requests = Some(requests);

        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        controller.dispatch(UiAction::ActivateSelection);
        let (generation, request) = receive_channel_request(&captured_requests);
        let first = subscription_video_summary();
        let mut second = subscription_video_summary();
        second.video_id = "M7lc1UVf-VE".to_owned();
        second.title = "Second fixture video".to_owned();
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation,
            request,
            result: Ok(SearchPage {
                page: 1,
                items: vec![
                    SearchItem::Video(first.clone()),
                    SearchItem::Video(second.clone()),
                ],
                next_page: None,
            }),
        });
        while captured_requests.try_recv().is_ok() {}
        controller.dispatch(UiAction::SelectSubscriptionItem(1));
        while captured_requests.try_recv().is_ok() {}

        controller.dispatch(UiAction::RefreshSubscriptionVideos);
        let (refresh_generation, refresh_request) = receive_channel_request(&captured_requests);
        assert_eq!(refresh_request.page, 1);
        assert_eq!(controller.view.subscriptions.items.len(), 2);
        assert_eq!(controller.view.subscriptions.selected_item, 1);
        assert!(controller.view.subscriptions.loading);

        controller.dispatch(UiAction::RefreshSubscriptionVideos);
        assert_no_channel_request(
            &captured_requests,
            "a second explicit refresh must not overlap the first",
        );

        let mut newest = subscription_video_summary();
        newest.video_id = "newest00000".to_owned();
        newest.title = "Newest fixture video".to_owned();
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation: refresh_generation,
            request: refresh_request,
            result: Ok(SearchPage {
                page: 1,
                items: vec![
                    SearchItem::Video(first),
                    SearchItem::Video(newest),
                    SearchItem::Video(second),
                ],
                next_page: None,
            }),
        });
        assert_eq!(controller.view.subscriptions.items.len(), 3);
        assert_eq!(controller.view.subscriptions.selected_item, 2);
        assert_eq!(
            controller.view.subscriptions.items[2]
                .media_id
                .as_ref()
                .map(|media_id| media_id.external_id.as_str()),
            Some("M7lc1UVf-VE")
        );
        assert!(controller.pending_subscription_refresh.is_none());

        while captured_requests.try_recv().is_ok() {}
        let titles_before_failure = controller
            .view
            .subscriptions
            .items
            .iter()
            .map(|row| row.title.clone())
            .collect::<Vec<_>>();
        controller.dispatch(UiAction::RefreshSubscriptionVideos);
        let (failure_generation, failure_request) = receive_channel_request(&captured_requests);
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation: failure_generation,
            request: failure_request,
            result: Err("temporary provider failure".to_owned()),
        });
        assert_eq!(
            controller
                .view
                .subscriptions
                .items
                .iter()
                .map(|row| row.title.clone())
                .collect::<Vec<_>>(),
            titles_before_failure
        );
        assert_eq!(controller.view.subscriptions.selected_item, 2);
        assert!(controller.pending_subscription_refresh.is_none());

        controller.dispatch(UiAction::RefreshSubscriptionVideos);
        let (replacement_generation, replacement_request) =
            receive_channel_request(&captured_requests);
        let mut replacement = subscription_video_summary();
        replacement.video_id = "replacement0".to_owned();
        replacement.title = "Replacement fixture".to_owned();
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation: replacement_generation,
            request: replacement_request,
            result: Ok(SearchPage {
                page: 1,
                items: vec![SearchItem::Video(replacement)],
                next_page: None,
            }),
        });
        assert_eq!(
            controller.view.subscriptions.selected_item, 0,
            "a removed selected video must clamp its previous index"
        );
        assert!(controller.pending_subscription_refresh.is_none());

        controller.dispatch(UiAction::RefreshSubscriptionVideos);
        let (stale_generation, stale_request) = receive_channel_request(&captured_requests);
        controller.dispatch(UiAction::ShowScreen(Screen::Search));
        let mut stale = subscription_video_summary();
        stale.video_id = "stale000000".to_owned();
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation: stale_generation,
            request: stale_request,
            result: Ok(SearchPage {
                page: 1,
                items: vec![SearchItem::Video(stale)],
                next_page: None,
            }),
        });
        assert!(controller.pending_subscription_refresh.is_none());
        assert_eq!(controller.view.screen, Screen::Search);
        assert!(matches!(
            controller
                .subscription_video_cache
                .get("UCfixture")
                .and_then(|cached| cached.items.first()),
            Some(SearchItem::Video(video)) if video.video_id == "replacement0"
        ));
    }

    #[test]
    fn subscription_refresh_stages_filtered_empty_pages_without_blanking_rows() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        save_fixture_subscriptions(&config, &["UCfixture"]);
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.youtube_provider_available = true;
        let (requests, captured_requests) = unbounded();
        controller.provider_requests = Some(requests);

        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        controller.dispatch(UiAction::ActivateSelection);
        let (generation, request) = receive_channel_request(&captured_requests);
        let first = subscription_video_summary();
        let mut selected = subscription_video_summary();
        selected.video_id = "M7lc1UVf-VE".to_owned();
        selected.title = "Selected fixture".to_owned();
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation,
            request,
            result: Ok(SearchPage {
                page: 1,
                items: vec![
                    SearchItem::Video(first.clone()),
                    SearchItem::Video(selected.clone()),
                ],
                next_page: None,
            }),
        });
        while captured_requests.try_recv().is_ok() {}
        controller.dispatch(UiAction::SelectSubscriptionItem(1));
        while captured_requests.try_recv().is_ok() {}

        controller.dispatch(UiAction::RefreshSubscriptionVideos);
        let (generation, request) = receive_channel_request(&captured_requests);
        let mut scheduled = subscription_video_summary();
        scheduled.video_id = "scheduled00".to_owned();
        scheduled.duration_seconds = Some(0);
        scheduled.live = false;
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation,
            request,
            result: Ok(SearchPage {
                page: 1,
                items: vec![SearchItem::Video(scheduled)],
                next_page: Some(2),
            }),
        });

        assert_eq!(
            controller
                .view
                .subscriptions
                .items
                .iter()
                .map(|row| row.title.as_str())
                .collect::<Vec<_>>(),
            ["Fixture video", "Selected fixture"],
            "cached rows must remain visible during filtered refresh pagination"
        );
        assert_eq!(controller.view.subscriptions.selected_item, 1);
        assert!(controller.pending_subscription_refresh.is_some());
        let (generation, request) = receive_channel_request(&captured_requests);
        assert_eq!(request.page, 2);
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation,
            request,
            result: Ok(SearchPage {
                page: 2,
                items: vec![SearchItem::Video(first), SearchItem::Video(selected)],
                next_page: None,
            }),
        });

        assert_eq!(controller.view.subscriptions.selected_item, 1);
        assert_eq!(
            controller.view.subscriptions.items[1]
                .media_id
                .as_ref()
                .map(|media_id| media_id.external_id.as_str()),
            Some("M7lc1UVf-VE")
        );
        assert!(controller.pending_subscription_refresh.is_none());
    }

    #[test]
    fn subscription_refresh_keeps_a_selection_changed_while_loading() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        save_fixture_subscriptions(&config, &["UCfixture"]);
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.youtube_provider_available = true;
        let (requests, captured_requests) = unbounded();
        controller.provider_requests = Some(requests);

        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        controller.dispatch(UiAction::ActivateSelection);
        let (generation, request) = receive_channel_request(&captured_requests);
        let first = subscription_video_summary();
        let mut second = subscription_video_summary();
        second.video_id = "M7lc1UVf-VE".to_owned();
        second.title = "Second fixture video".to_owned();
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation,
            request,
            result: Ok(SearchPage {
                page: 1,
                items: vec![
                    SearchItem::Video(first.clone()),
                    SearchItem::Video(second.clone()),
                ],
                next_page: None,
            }),
        });
        while captured_requests.try_recv().is_ok() {}
        controller.dispatch(UiAction::SelectSubscriptionItem(1));
        while captured_requests.try_recv().is_ok() {}

        controller.dispatch(UiAction::RefreshSubscriptionVideos);
        let (generation, request) = receive_channel_request(&captured_requests);
        controller.dispatch(UiAction::SelectSubscriptionItem(0));
        while captured_requests.try_recv().is_ok() {}
        let mut newest = subscription_video_summary();
        newest.video_id = "newest00000".to_owned();
        newest.title = "Newest fixture video".to_owned();
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation,
            request,
            result: Ok(SearchPage {
                page: 1,
                items: vec![
                    SearchItem::Video(newest),
                    SearchItem::Video(second),
                    SearchItem::Video(first),
                ],
                next_page: None,
            }),
        });

        assert_eq!(
            controller.view.subscriptions.selected_item, 2,
            "the video chosen during refresh should win over its initial snapshot"
        );
        assert_eq!(
            controller.view.subscriptions.items[2]
                .media_id
                .as_ref()
                .map(|media_id| media_id.external_id.as_str()),
            Some("dQw4w9WgXcQ")
        );
    }

    #[cfg(feature = "wikidata")]
    #[test]
    fn wikidata_formatter_labels_item_links_without_duplicate_fallback_ids() {
        use crate::providers::wikidata::{
            WikidataEntityStatements, WikidataStatement, WikidataStatementValue,
        };

        let mut entity = WikidataEntityStatements {
            item_id: "Q61113".to_owned(),
            statements: vec![WikidataStatement {
                property_id: "P27".to_owned(),
                property_label: "country of citizenship".to_owned(),
                values: vec![
                    WikidataStatementValue {
                        display: "Soviet Union".to_owned(),
                        item_id: Some("Q15180".to_owned()),
                        external_url: None,
                        preview_url: None,
                    },
                    WikidataStatementValue {
                        display: "Q212".to_owned(),
                        item_id: Some("Q212".to_owned()),
                        external_url: None,
                        preview_url: None,
                    },
                ],
            }],
            truncated: false,
            hard_bounds_reached: false,
            unsupported_values_omitted: false,
        };

        let formatted = format_wikidata_entity(&entity);

        assert!(
            formatted
                .text
                .contains("country of citizenship (P27): Soviet Union (Q15180); Q212")
        );
        assert!(!formatted.text.contains("Q212 (Q212)"));
        assert_eq!(
            formatted
                .value_links
                .iter()
                .map(|link| {
                    (
                        &formatted.text[link.start_byte..link.end_byte],
                        link.url.as_str(),
                    )
                })
                .collect::<Vec<_>>(),
            [
                (
                    "Soviet Union (Q15180)",
                    "https://www.wikidata.org/wiki/Q15180"
                ),
                ("Q212", "https://www.wikidata.org/wiki/Q212"),
            ]
        );
        assert_eq!(
            canonical_wikidata_item_url("Q212").as_deref(),
            Some("https://www.wikidata.org/wiki/Q212")
        );
        assert_eq!(canonical_wikidata_item_url("../Q212"), None);
        assert_eq!(canonical_wikidata_item_url("Q0"), None);

        let mut details = DetailView::default();
        apply_wikidata_links(
            &mut details,
            &[crate::domain::WikidataLink {
                item_id: "Q212".to_owned(),
                label: "Q212".to_owned(),
                description: None,
                url: url::Url::parse("https://www.wikidata.org/wiki/Q212").expect("Wikidata URL"),
            }],
        );
        assert_eq!(details.links[0].label, "Q212");

        entity.truncated = true;
        entity.hard_bounds_reached = true;
        assert!(
            format_wikidata_entity(&entity)
                .text
                .contains("256 properties"),
            "the hard-bound footer must name the exact limits"
        );
    }

    #[cfg(feature = "wikidata")]
    #[test]
    fn wikidata_formatter_links_external_ids_and_exposes_p18_preview() {
        use crate::providers::wikidata::{
            WikidataEntityStatements, WikidataStatement, WikidataStatementValue,
        };

        let x_url = url::Url::parse("https://x.com/douglasadams").expect("X fixture URL");
        let image_page_url = url::Url::parse(
            "https://commons.wikimedia.org/wiki/File:Douglas%20adams%20portrait%20cropped.jpg",
        )
        .expect("Commons file-page fixture URL");
        let image_preview_url = url::Url::parse(
            "https://commons.wikimedia.org/wiki/Special:Redirect/file/Douglas%20adams%20portrait%20cropped.jpg?width=512",
        )
        .expect("Commons preview fixture URL");
        let entity = WikidataEntityStatements {
            item_id: "Q42".to_owned(),
            statements: vec![
                WikidataStatement {
                    property_id: "P2002".to_owned(),
                    property_label: "X (Twitter) username".to_owned(),
                    values: vec![WikidataStatementValue {
                        display: "douglasadams".to_owned(),
                        item_id: None,
                        external_url: Some(x_url.clone()),
                        preview_url: None,
                    }],
                },
                WikidataStatement {
                    property_id: "P18".to_owned(),
                    property_label: "image".to_owned(),
                    values: vec![WikidataStatementValue {
                        display: "Douglas adams portrait cropped.jpg".to_owned(),
                        item_id: None,
                        external_url: Some(image_page_url.clone()),
                        preview_url: Some(image_preview_url.clone()),
                    }],
                },
            ],
            truncated: false,
            hard_bounds_reached: false,
            unsupported_values_omitted: false,
        };

        let formatted = format_wikidata_entity(&entity);
        assert!(!formatted.text.contains("Wikidata properties for Q42"));
        assert_eq!(formatted.image_url.as_ref(), Some(&image_preview_url));
        assert_eq!(
            formatted
                .value_links
                .iter()
                .map(|link| link.url.as_str())
                .collect::<Vec<_>>(),
            [x_url.as_str(), image_page_url.as_str()]
        );
    }

    #[cfg(feature = "wikidata")]
    #[test]
    fn reopening_subscription_channel_reapplies_persisted_wikidata_link() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let channel_id = "UC6R1juCB5ArnJGMmUlEE_fg";
        save_fixture_subscriptions(&config, &[channel_id]);
        {
            let store = StateStore::open(&config).expect("disk state");
            store
                .put_cached_wikidata(&CachedWikidataLookup {
                    property_id: "P2397".to_owned(),
                    external_id: channel_id.to_owned(),
                    items: vec![crate::domain::WikidataLink {
                        item_id: "Q61113".to_owned(),
                        label: "Foster the People".to_owned(),
                        description: Some("American indie pop band".to_owned()),
                        url: url::Url::parse("https://www.wikidata.org/wiki/Q61113")
                            .expect("Wikidata fixture URL"),
                    }],
                    fetched_at: unix_time(),
                    expires_at: i64::MAX,
                })
                .expect("seed cached channel Wikidata link");
        }
        let store = StateStore::open(&config).expect("reopened disk state");
        let mut controller = AppController::new(config, store, None, None);

        let assert_fixture_link = |controller: &AppController| {
            let details = controller
                .view
                .details
                .as_ref()
                .expect("subscription channel details");
            assert_eq!(
                details
                    .links
                    .iter()
                    .filter_map(|link| link.wikidata_item_id.as_deref())
                    .collect::<Vec<_>>(),
                ["Q61113"],
                "rebuilding the channel panel must restore its fresh Wikidata cache"
            );
        };

        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        assert_fixture_link(&controller);

        controller.dispatch(UiAction::ShowScreen(Screen::Search));
        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        assert_fixture_link(&controller);
    }

    #[cfg(feature = "wikidata")]
    #[test]
    fn late_wikidata_response_cannot_replace_reopened_cached_channel() {
        use crate::providers::wikidata::WikidataExternalKind;

        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let cached_channel_id = "UC6R1juCB5ArnJGMmUlEE_fg";
        save_fixture_subscriptions(&config, &["UCfirst", cached_channel_id]);
        let store = StateStore::open_in_memory().expect("in-memory state");
        store
            .put_cached_wikidata(&CachedWikidataLookup {
                property_id: "P2397".to_owned(),
                external_id: cached_channel_id.to_owned(),
                items: vec![crate::domain::WikidataLink {
                    item_id: "Q61113".to_owned(),
                    label: "Foster the People".to_owned(),
                    description: Some("American indie pop band".to_owned()),
                    url: url::Url::parse("https://www.wikidata.org/wiki/Q61113")
                        .expect("Wikidata fixture URL"),
                }],
                fetched_at: unix_time(),
                expires_at: i64::MAX,
            })
            .expect("seed second channel Wikidata cache");
        let mut controller = AppController::new(config, store, None, None);
        let (requests, captured_requests) = unbounded();
        controller.provider_requests = Some(requests);

        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        let scheduled = controller
            .scheduled_channel_wikidata
            .clone()
            .expect("first channel Wikidata schedule");
        controller.request_due_channel_wikidata(scheduled.due_at);
        let ProviderRequest::Wikidata {
            generation,
            kind: WikidataExternalKind::YouTubeChannel,
            external_id,
        } = captured_requests
            .recv_timeout(Duration::from_secs(1))
            .expect("first channel Wikidata request")
        else {
            panic!("expected first channel Wikidata request");
        };
        assert_eq!(external_id, "UCfirst");

        controller.select_subscription_source(1);
        controller.handle_provider_response(ProviderResponse::Wikidata {
            generation,
            property_id: "P2397".to_owned(),
            external_id: "UCfirst".to_owned(),
            result: Ok(vec![crate::domain::WikidataLink {
                item_id: "Q42".to_owned(),
                label: "Douglas Adams".to_owned(),
                description: Some("English author and humorist".to_owned()),
                url: url::Url::parse("https://www.wikidata.org/wiki/Q42")
                    .expect("late Wikidata fixture URL"),
            }]),
        });

        let details = controller
            .view
            .details
            .as_ref()
            .expect("reopened cached channel details");
        assert_eq!(
            details
                .links
                .iter()
                .filter_map(|link| link.wikidata_item_id.as_deref())
                .collect::<Vec<_>>(),
            ["Q61113"],
            "a response owned by the previous channel must not replace the current row"
        );
    }

    #[cfg(feature = "wikidata")]
    #[test]
    fn subscription_channel_panel_debounces_exact_lazy_wikidata_lookup() {
        use crate::providers::wikidata::WikidataExternalKind;

        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        save_fixture_subscriptions(&config, &["UCfixture"]);
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        let (requests, captured_requests) = unbounded();
        controller.provider_requests = Some(requests);

        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        let scheduled = controller
            .scheduled_channel_wikidata
            .clone()
            .expect("settled-source Wikidata schedule");
        assert!(
            captured_requests.try_recv().is_err(),
            "moving across subscription sources must not immediately query Wikidata"
        );
        controller.request_due_channel_wikidata(scheduled.due_at);
        let request = captured_requests
            .recv_timeout(Duration::from_secs(1))
            .expect("Wikidata request");
        assert!(matches!(
            request,
            ProviderRequest::Wikidata {
                kind: WikidataExternalKind::YouTubeChannel,
                external_id,
                ..
            } if external_id == "UCfixture"
        ));
        assert!(
            controller
                .view
                .details
                .as_ref()
                .is_some_and(|details| details.wikidata.contains("loading P2397 lazily"))
        );
    }

    #[test]
    fn empty_subscription_pages_continue_boundedly_and_enter_resumes_pagination() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        save_fixture_subscriptions(&config, &["UCfixture"]);
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.youtube_provider_available = true;
        let (requests, captured_requests) = unbounded();
        controller.provider_requests = Some(requests);

        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        controller.dispatch(UiAction::ActivateSelection);
        for page in 1..=MAX_AUTOMATIC_EMPTY_SUBSCRIPTION_PAGES {
            let (generation, request) = receive_channel_request(&captured_requests);
            assert_eq!(request.page, page);
            controller.handle_provider_response(ProviderResponse::ChannelVideos {
                generation,
                request,
                result: Ok(SearchPage {
                    page,
                    items: Vec::new(),
                    next_page: Some(page.saturating_add(1)),
                }),
            });
        }
        assert!(captured_requests.try_recv().is_err());
        assert!(
            controller
                .view
                .status_line
                .contains("press Enter to continue")
        );

        controller.dispatch(UiAction::ActivateSelection);
        let (generation, request) = receive_channel_request(&captured_requests);
        assert_eq!(
            request.page,
            MAX_AUTOMATIC_EMPTY_SUBSCRIPTION_PAGES.saturating_add(1)
        );
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation,
            request: request.clone(),
            result: Ok(SearchPage {
                page: request.page,
                items: vec![SearchItem::Video(subscription_video_summary())],
                next_page: None,
            }),
        });
        assert_eq!(controller.view.subscriptions.items.len(), 1);
    }

    #[test]
    fn subscription_feed_hides_zero_duration_non_live_placeholders() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        save_fixture_subscriptions(&config, &["UCfixture"]);
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.youtube_provider_available = true;
        let (requests, captured_requests) = unbounded();
        controller.provider_requests = Some(requests);

        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        controller.dispatch(UiAction::ActivateSelection);
        let (generation, request) = receive_channel_request(&captured_requests);
        let mut planned = subscription_video_summary();
        planned.video_id = "planned0000".to_owned();
        planned.title = "Scheduled stream".to_owned();
        planned.duration_seconds = Some(0);
        planned.live = false;
        let mut live = subscription_video_summary();
        live.video_id = "live0000000".to_owned();
        live.title = "Live now".to_owned();
        live.duration_seconds = Some(0);
        live.live = true;
        let playable = subscription_video_summary();

        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation,
            request,
            result: Ok(SearchPage {
                page: 1,
                items: vec![
                    SearchItem::Video(planned),
                    SearchItem::Video(live),
                    SearchItem::Video(playable),
                ],
                next_page: None,
            }),
        });

        assert_eq!(
            controller
                .view
                .subscriptions
                .items
                .iter()
                .map(|row| row.title.as_str())
                .collect::<Vec<_>>(),
            ["Live now", "Fixture video"]
        );
    }

    #[test]
    fn empty_later_subscription_page_reaches_the_next_playable_page() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        save_fixture_subscriptions(&config, &["UCfixture"]);
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.youtube_provider_available = true;
        let (requests, captured_requests) = unbounded();
        controller.provider_requests = Some(requests);

        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        controller.dispatch(UiAction::ActivateSelection);
        let (generation, request) = receive_channel_request(&captured_requests);
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation,
            request,
            result: Ok(SearchPage {
                page: 1,
                items: vec![SearchItem::Video(subscription_video_summary())],
                next_page: Some(2),
            }),
        });
        controller.dispatch(UiAction::SelectSubscriptionItem(0));
        let (generation, request) = receive_channel_request(&captured_requests);
        assert_eq!(request.page, 2);
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation,
            request,
            result: Ok(SearchPage {
                page: 2,
                items: Vec::new(),
                next_page: Some(3),
            }),
        });

        let (generation, request) = receive_channel_request(&captured_requests);
        assert_eq!(request.page, 3);
        let mut later_video = subscription_video_summary();
        later_video.video_id = "abcdefghijk".to_owned();
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation,
            request,
            result: Ok(SearchPage {
                page: 3,
                items: vec![SearchItem::Video(later_video)],
                next_page: None,
            }),
        });

        assert_eq!(controller.view.subscriptions.items.len(), 2);
    }

    #[test]
    fn subscription_pages_reject_skipped_continuations() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        save_fixture_subscriptions(&config, &["UCfixture"]);
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.youtube_provider_available = true;
        let (requests, captured_requests) = unbounded();
        controller.provider_requests = Some(requests);

        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        controller.dispatch(UiAction::ActivateSelection);
        let (generation, request) = receive_channel_request(&captured_requests);
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation,
            request,
            result: Ok(SearchPage {
                page: 1,
                items: Vec::new(),
                next_page: Some(3),
            }),
        });

        assert_eq!(
            controller
                .view
                .error_popup
                .as_ref()
                .map(|popup| popup.title.as_str()),
            Some("Subscription videos failed")
        );
        assert!(controller.subscription_video_cache.is_empty());
    }

    #[test]
    fn subscription_response_is_cached_without_mutating_another_screen() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        save_fixture_subscriptions(&config, &["UCfixture"]);
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.youtube_provider_available = true;
        let (requests, captured_requests) = unbounded();
        controller.provider_requests = Some(requests);

        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        controller.dispatch(UiAction::ActivateSelection);
        let (generation, request) = receive_channel_request(&captured_requests);
        controller.dispatch(UiAction::ShowScreen(Screen::Search));
        controller.view.status_line = "Search state remains active".to_owned();
        controller.view.details = Some(DetailView {
            title: "Search details".to_owned(),
            ..DetailView::default()
        });

        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation,
            request,
            result: Ok(SearchPage {
                page: 1,
                items: vec![SearchItem::Video(subscription_video_summary())],
                next_page: None,
            }),
        });

        assert_eq!(controller.view.screen, Screen::Search);
        assert_eq!(controller.view.status_line, "Search state remains active");
        assert_eq!(
            controller
                .view
                .details
                .as_ref()
                .map(|details| details.title.as_str()),
            Some("Search details")
        );
        assert_eq!(
            controller
                .subscription_video_cache
                .get("UCfixture")
                .map(|cached| cached.items.len()),
            Some(1)
        );
    }

    #[test]
    fn subscription_cache_compacts_summaries_and_obeys_global_byte_budget() {
        let config = Config::for_dir("/tmp/youta-subscription-byte-cache-test");
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);

        for channel_index in 0..MAX_CACHED_SUBSCRIPTION_CHANNELS {
            let channel_id = format!("UCcache{channel_index:02}");
            let items = (0..80)
                .map(|item_index| {
                    let mut video = subscription_video_summary();
                    video.title = "T".repeat(MAX_CACHED_SUBSCRIPTION_LABEL_BYTES * 2);
                    video.description =
                        "D".repeat(MAX_CACHED_SUBSCRIPTION_DESCRIPTION_BYTES * 2);
                    video.thumbnails = ["one", "two"]
                        .into_iter()
                        .map(|quality| Thumbnail {
                            url: url::Url::parse(&format!(
                                "https://i.ytimg.com/vi/dQw4w9WgXcQ/{channel_index}-{item_index}-{quality}.jpg"
                            ))
                            .expect("fixture thumbnail"),
                            quality: Some(quality.repeat(100)),
                            width: Some(480),
                            height: Some(270),
                        })
                        .collect();
                    SearchItem::Video(video)
                })
                .collect();
            controller.cache_subscription_video_page(
                &channel_id,
                SearchPage {
                    page: 1,
                    items,
                    next_page: None,
                },
            );
        }

        assert!(
            controller.subscription_cache_estimated_heap_bytes() <= MAX_CACHED_SUBSCRIPTION_BYTES
        );
        assert!(
            controller.subscription_video_cache.len() < MAX_CACHED_SUBSCRIPTION_CHANNELS,
            "the byte budget must evict before the entry-count limit"
        );
        assert!(
            controller
                .subscription_video_cache
                .contains_key("UCcache23")
        );
        for cached in controller.subscription_video_cache.values() {
            for item in &cached.items {
                let SearchItem::Video(video) = item else {
                    panic!("channel cache contains a non-video item");
                };
                assert!(video.title.len() <= MAX_CACHED_SUBSCRIPTION_LABEL_BYTES);
                assert!(video.description.len() <= MAX_CACHED_SUBSCRIPTION_DESCRIPTION_BYTES);
                assert!(video.thumbnails.len() <= 1);
            }
        }
    }

    #[test]
    fn full_video_details_keep_visible_text_but_not_unbounded_cached_fields() {
        let config = Config::for_dir("/tmp/youta-subscription-details-cache-test");
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.view.screen = Screen::Subscriptions;
        controller.view.subscriptions.route = SubscriptionRoute::Items;
        controller.view.right_panel_mode = RightPanelMode::Details;
        controller.active_subscription_channel_id = Some("UCfixture".to_owned());
        controller.cache_subscription_video_page(
            "UCfixture",
            SearchPage {
                page: 1,
                items: vec![SearchItem::Video(subscription_video_summary())],
                next_page: None,
            },
        );
        controller.refresh_subscription_video_rows();

        let full_description = "D".repeat(MAX_CACHED_SUBSCRIPTION_DESCRIPTION_BYTES * 2);
        let mut details =
            subscription_video_details(&"T".repeat(MAX_CACHED_SUBSCRIPTION_LABEL_BYTES * 2));
        details.description = full_description.clone();
        details.published_text = Some("P".repeat(MAX_CACHED_SUBSCRIPTION_FIELD_BYTES * 2));
        details.thumbnails = ["first", "second"]
            .into_iter()
            .map(|quality| Thumbnail {
                url: url::Url::parse(&format!("https://i.ytimg.com/vi/dQw4w9WgXcQ/{quality}.jpg"))
                    .expect("fixture thumbnail"),
                quality: Some(quality.repeat(100)),
                width: Some(480),
                height: Some(270),
            })
            .collect();

        controller.handle_provider_response(ProviderResponse::Details {
            generation: controller.details_generation,
            result: Ok(details),
        });

        let cached = controller
            .subscription_video_cache
            .get("UCfixture")
            .and_then(|entry| entry.items.first())
            .expect("updated cached video");
        let SearchItem::Video(cached) = cached else {
            panic!("channel cache contains a non-video item");
        };
        assert!(cached.title.len() <= MAX_CACHED_SUBSCRIPTION_LABEL_BYTES);
        assert!(cached.description.len() <= MAX_CACHED_SUBSCRIPTION_DESCRIPTION_BYTES);
        assert!(
            cached
                .published_text
                .as_ref()
                .is_none_or(|published| published.len() <= MAX_CACHED_SUBSCRIPTION_FIELD_BYTES)
        );
        assert!(cached.thumbnails.len() <= 1);
        assert!(
            controller.subscription_cache_estimated_heap_bytes() <= MAX_CACHED_SUBSCRIPTION_BYTES
        );
        assert_eq!(
            controller
                .view
                .details
                .as_ref()
                .map(|view| view.description.as_str()),
            Some(full_description.as_str()),
            "the visible Details pane must retain the provider's full text"
        );
    }

    #[test]
    fn subscription_rows_omit_repeated_subscriber_count_and_subscription_marker() {
        let config = Config::for_dir("/tmp/youta-subscription-subscriber-test");
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.view.screen = Screen::Subscriptions;
        controller.active_subscription_channel_id = Some("UCfixture".to_owned());
        controller.cache_subscription_video_page(
            "UCfixture",
            SearchPage {
                page: 1,
                items: vec![SearchItem::Video(subscription_video_summary())],
                next_page: None,
            },
        );
        controller.refresh_subscription_video_rows();
        assert!(
            !controller.view.subscriptions.items[0]
                .subtitle
                .contains("subscribers")
        );
        assert!(
            !controller.view.subscriptions.items[0]
                .subtitle
                .contains("Fixture channel"),
            "the channel name must appear only in the subscription heading"
        );
        assert!(!controller.view.subscriptions.items[0].subscribed);

        controller.handle_provider_response(ProviderResponse::ChannelSubscriberCounts {
            provider_generation: controller.youtube_provider_generation,
            requested_ids: vec!["UCfixture".to_owned()],
            result: Ok(vec![ChannelSubscriberCount {
                channel_id: "UCfixture".to_owned(),
                subscriber_count: Some(13_045),
            }]),
        });

        assert_eq!(
            controller.view.subscriptions.source_subscriber_count,
            Some(13_045)
        );
        assert!(
            !controller.view.subscriptions.items[0]
                .subtitle
                .contains("subscribers"),
            "the channel-level count must not repeat on every subscription video"
        );
        assert!(
            !controller.view.subscriptions.items[0].subscribed,
            "subscription videos must omit the redundant diamond marker"
        );
    }

    #[test]
    fn subscription_channel_details_are_debounced_cached_and_applied_to_the_heading() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        save_fixture_subscriptions(&config, &["UCfixture"]);
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.youtube_provider_available = true;
        let (requests, captured_requests) = unbounded();
        controller.provider_requests = Some(requests);

        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        let scheduled = controller
            .scheduled_channel_details
            .clone()
            .expect("settled-source metadata schedule");
        let before_due = scheduled
            .due_at
            .checked_sub(Duration::from_millis(1))
            .expect("scheduled deadline has a preceding instant");
        controller.request_due_channel_details(before_due);
        assert!(
            captured_requests.try_recv().is_err(),
            "moving onto a source must not immediately spend provider quota"
        );
        controller.request_due_channel_details(scheduled.due_at);
        let ProviderRequest::ChannelDetails {
            generation,
            provider_generation,
            channel_id,
        } = captured_requests
            .recv_timeout(Duration::from_secs(1))
            .expect("channel details request")
        else {
            panic!("expected channel details request");
        };
        assert_eq!(channel_id, "UCfixture");
        let channel_artwork =
            url::Url::parse("https://yt3.ggpht.com/provider-channel=s800").expect("artwork URL");

        controller.handle_provider_response(ProviderResponse::ChannelDetails {
            generation,
            provider_generation,
            channel_id: channel_id.clone(),
            result: Ok(ChannelDetails {
                summary: ChannelSummary {
                    channel_id,
                    name: "Provider channel name".to_owned(),
                    description: "Complete public channel description".to_owned(),
                    subscriber_count: Some(1_850_000),
                    video_count: Some(412),
                    created_at: Some(1_100_000_000),
                    auto_generated: false,
                    thumbnails: vec![Thumbnail {
                        url: channel_artwork.clone(),
                        quality: Some("high".to_owned()),
                        width: Some(800),
                        height: Some(800),
                    }],
                    webpage_url: Some(
                        url::Url::parse("https://www.youtube.com/@fixture")
                            .expect("fixture channel URL"),
                    ),
                },
                total_view_count: Some(987_654_321),
                country: Some("Georgia".to_owned()),
                external_links: vec![crate::providers::ChannelExternalLink {
                    label: "Fixture Telegram".to_owned(),
                    url: url::Url::parse("https://t.me/fixture").expect("Telegram URL"),
                    kind: ChannelExternalLinkKind::Telegram,
                }],
                external_links_truncated: false,
            }),
        });

        let details = controller.view.details.as_ref().expect("channel details");
        assert_eq!(details.channel_name, "Provider channel name");
        assert_eq!(details.description, "Complete public channel description");
        assert_eq!(details.channel_subscriber_count, Some(1_850_000));
        assert_eq!(details.channel_video_count, Some(412));
        assert_eq!(details.channel_total_view_count, Some(987_654_321));
        assert_eq!(details.channel_created, "2004 November 9");
        assert_eq!(details.channel_country, "Georgia");
        assert_eq!(
            details.links,
            [DetailLinkView {
                label: "Telegram: Fixture Telegram".to_owned(),
                url: "https://t.me/fixture".to_owned(),
                wikidata_item_id: None,
            }]
        );
        assert_eq!(
            controller.view.subscriptions.source_subscriber_count,
            Some(1_850_000)
        );
        assert_eq!(
            controller.view.subscriptions.sources[0]
                .thumbnail_url
                .as_ref(),
            Some(&channel_artwork),
            "new channel metadata must join the subscription-artwork prefetch backlog"
        );

        controller.select_subscription_source(0);
        assert!(
            controller.scheduled_channel_details.is_none(),
            "a process-local cache hit must not schedule another provider request"
        );
        assert!(captured_requests.try_recv().is_err());
    }

    #[cfg(feature = "network")]
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one end-to-end controller fixture enumerates every requested channel link family"
    )]
    fn public_channel_page_enrichment_adds_about_counts_country_and_social_links() {
        use crate::providers::ChannelExternalLink;
        use crate::providers::youtube_channel_page::YouTubeChannelPageMetadata;

        let config = Config::for_dir("/tmp/youta-channel-page-enrichment-test");
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.channel_details_generation = 7;
        controller.view.right_panel_mode = RightPanelMode::Channel;
        controller.view.details = Some(DetailView {
            channel_id: "UCfixture".to_owned(),
            links: vec![DetailLinkView {
                label: "Fixture entity (Q42)".to_owned(),
                url: "https://www.wikidata.org/wiki/Q42".to_owned(),
                wikidata_item_id: Some("Q42".to_owned()),
            }],
            ..DetailView::default()
        });
        controller.channel_profile_cache.insert(
            "UCfixture".to_owned(),
            ChannelDetails {
                summary: ChannelSummary {
                    channel_id: "UCfixture".to_owned(),
                    name: "Fixture channel".to_owned(),
                    description: "Fixture description".to_owned(),
                    subscriber_count: Some(13_045),
                    video_count: None,
                    created_at: None,
                    auto_generated: false,
                    thumbnails: Vec::new(),
                    webpage_url: None,
                },
                total_view_count: None,
                country: Some("UA".to_owned()),
                external_links: Vec::new(),
                external_links_truncated: false,
            },
        );
        let external_link = |label: &str, url: &str, kind| ChannelExternalLink {
            label: label.to_owned(),
            url: url::Url::parse(url).expect("fixture external URL"),
            kind,
        };

        controller.handle_provider_response(ProviderResponse::ChannelPageMetadata {
            generation: 7,
            provider_generation: controller.youtube_provider_generation,
            channel_id: "UCfixture".to_owned(),
            result: Ok(YouTubeChannelPageMetadata {
                channel_id: "UCfixture".to_owned(),
                country: Some("Ukraine".to_owned()),
                joined_at: Some(1_398_297_600),
                video_count: Some(3_977),
                total_view_count: Some(1_094_367_204),
                external_links: vec![
                    external_link(
                        "Official website",
                        "https://example.org/",
                        ChannelExternalLinkKind::Website,
                    ),
                    external_link(
                        "Telegram",
                        "https://t.me/fixture",
                        ChannelExternalLinkKind::Telegram,
                    ),
                    external_link(
                        "Facebook",
                        "https://www.facebook.com/fixture",
                        ChannelExternalLinkKind::Facebook,
                    ),
                    external_link(
                        "Twitter",
                        "https://x.com/fixture",
                        ChannelExternalLinkKind::XTwitter,
                    ),
                    external_link(
                        "TikTok",
                        "https://www.tiktok.com/@fixture",
                        ChannelExternalLinkKind::TikTok,
                    ),
                    external_link(
                        "Instagram",
                        "https://www.instagram.com/fixture",
                        ChannelExternalLinkKind::Instagram,
                    ),
                ],
                external_links_truncated: false,
            }),
        });

        let details = controller.view.details.as_ref().expect("channel details");
        assert_eq!(details.channel_created, "2014 April 24");
        assert_eq!(details.channel_video_count, Some(3_977));
        assert_eq!(details.channel_total_view_count, Some(1_094_367_204));
        assert_eq!(details.channel_country, "Ukraine");
        assert_eq!(
            details
                .links
                .iter()
                .map(|link| link.label.as_str())
                .collect::<Vec<_>>(),
            [
                "Website: Official website",
                "Telegram",
                "Facebook",
                "X/Twitter: Twitter",
                "TikTok",
                "Instagram",
                "Fixture entity (Q42)",
            ],
            "public channel links and the independent Wikidata row must coexist"
        );
        assert_eq!(
            details
                .links
                .iter()
                .filter(|link| link.wikidata_item_id.as_deref() == Some("Q42"))
                .count(),
            1,
            "the About-page response must not duplicate the Wikidata row"
        );
    }

    #[test]
    fn subscription_sources_restore_persisted_artwork_before_channel_switches() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        save_fixture_subscriptions(&config, &["UCfixture", "UCsecond"]);
        let first_artwork =
            url::Url::parse("https://yt3.ggpht.com/first-channel=s800").expect("first artwork URL");
        let second_artwork = url::Url::parse("https://yt3.ggpht.com/second-channel=s800")
            .expect("second artwork URL");
        let now = unix_time();
        {
            let store = StateStore::open(&config).expect("open initial state");
            for (channel_id, name, artwork) in [
                ("UCfixture", "Fixture channel 1", first_artwork.clone()),
                ("UCsecond", "Fixture channel 2", second_artwork.clone()),
            ] {
                store
                    .put_cached_channel_summary(&crate::persistence::CachedChannelSummary {
                        summary: ChannelSummary {
                            channel_id: channel_id.to_owned(),
                            name: name.to_owned(),
                            description: format!("Cached description for {name}"),
                            subscriber_count: Some(13_045),
                            video_count: Some(42),
                            created_at: Some(1_100_000_000),
                            auto_generated: false,
                            thumbnails: vec![Thumbnail {
                                url: artwork,
                                quality: Some("high".to_owned()),
                                width: Some(800),
                                height: Some(800),
                            }],
                            webpage_url: canonical_youtube_channel_url(channel_id),
                        },
                        fetched_at: now.saturating_sub(1),
                        expires_at: now.saturating_add(60),
                    })
                    .expect("persist channel summary");
            }
        }

        let store = StateStore::open(&config).expect("reopen persisted state");
        let mut controller = AppController::new(config, store, None, None);
        controller.youtube_provider_available = true;
        let (requests, captured_requests) = unbounded();
        controller.provider_requests = Some(requests);

        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));

        assert_eq!(
            controller
                .view
                .subscriptions
                .sources
                .iter()
                .map(|row| row.thumbnail_url.as_ref())
                .collect::<Vec<_>>(),
            [Some(&first_artwork), Some(&second_artwork)],
            "every persisted source URL must be available to prefetch before selection"
        );
        assert_eq!(
            controller
                .view
                .details
                .as_ref()
                .and_then(|details| details.thumbnail_url.as_ref()),
            Some(&first_artwork)
        );
        assert_eq!(
            controller
                .scheduled_channel_details
                .as_ref()
                .map(|scheduled| scheduled.channel_id.as_str()),
            Some("UCfixture"),
            "persisted artwork is immediate while the richer process-local profile is loaded lazily"
        );
        assert!(
            captured_requests.try_recv().is_err(),
            "the debounced profile lookup must not block initial artwork"
        );

        controller.select_subscription_source(1);

        assert_eq!(
            controller
                .view
                .details
                .as_ref()
                .and_then(|details| details.thumbnail_url.as_ref()),
            Some(&second_artwork),
            "switching must reuse the persisted channel summary immediately"
        );
        assert_eq!(
            controller
                .scheduled_channel_details
                .as_ref()
                .map(|scheduled| scheduled.channel_id.as_str()),
            Some("UCsecond"),
            "switching replaces the pending enrichment with the newly visible channel"
        );
        assert!(
            captured_requests.try_recv().is_err(),
            "fresh persisted artwork must not trigger immediate provider network work"
        );
    }

    #[test]
    fn subscription_channel_metadata_cache_has_a_fixed_lru_bound() {
        let config = Config::for_dir("/tmp/youta-channel-metadata-cache-test");
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);

        for index in 0..=MAX_CACHED_CHANNEL_DETAILS {
            controller.cache_channel_details(format!("UC{index}"), None);
        }

        assert_eq!(
            controller.channel_details_cache.len(),
            MAX_CACHED_CHANNEL_DETAILS
        );
        assert!(!controller.channel_details_cache.contains_key("UC0"));
        assert!(
            controller
                .channel_details_cache
                .contains_key(&format!("UC{MAX_CACHED_CHANNEL_DETAILS}"))
        );
    }

    #[test]
    fn full_subscription_description_replaces_summary_chapters_for_seekbar_and_queue() {
        let config = Config::for_dir("/tmp/youta-subscription-chapter-details-test");
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        let mut summary = subscription_video_summary();
        summary.duration_seconds = Some(4_000);
        controller.view.screen = Screen::Subscriptions;
        controller.view.subscriptions.route = SubscriptionRoute::Items;
        controller.view.right_panel_mode = RightPanelMode::Details;
        controller.active_subscription_channel_id = Some("UCfixture".to_owned());
        controller.cache_subscription_video_page(
            "UCfixture",
            SearchPage {
                page: 1,
                items: vec![SearchItem::Video(summary.clone())],
                next_page: None,
            },
        );
        controller.refresh_subscription_video_rows();
        controller.view.details = Some(preliminary_detail(
            &SearchItem::Video(summary.clone()),
            &controller.subscription_tree,
        ));
        let media_id = MediaId::new(SourceKind::YouTube, &summary.video_id);
        controller.current_media = Some(media_id.clone());
        controller
            .playback_queue
            .push(queue_item_from_video(&summary, None));

        let mut details = subscription_video_details("Fixture video");
        details.duration_seconds = Some(4_000);
        details.description =
            "➤ 00:00 Introduction\n➤ 00:01:35 Батуми\n➤ 01:02:51 移住\ninline 01:03:00 ignored"
                .to_owned();
        controller.handle_provider_response(ProviderResponse::Details {
            generation: controller.details_generation,
            result: Ok(details),
        });

        let expected = [("Introduction", 0), ("Батуми", 95), ("移住", 3_771)];
        assert_eq!(
            controller
                .view
                .details
                .as_ref()
                .map(|details| details.description.as_str()),
            Some("00:00 Introduction\n00:01:35 Батуми\n01:02:51 移住\ninline 01:03:00 ignored"),
            "display normalization must remove chapter-list markers without changing other lines"
        );
        assert_eq!(
            controller
                .view
                .playback_chapters
                .iter()
                .map(|chapter| (chapter.title.as_str(), chapter.start_seconds))
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            controller.playback_queue.items[0]
                .media
                .chapters
                .iter()
                .map(|chapter| (chapter.title.as_str(), chapter.start_seconds))
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            controller
                .selected_queue_item()
                .expect("selected subscription queue item")
                .media
                .chapters
                .iter()
                .map(|chapter| (chapter.title.as_str(), chapter.start_seconds))
                .collect::<Vec<_>>(),
            expected,
            "queueing after metadata arrives must use the full visible description"
        );
        assert_eq!(controller.current_media, Some(media_id));
    }

    #[test]
    fn split_subscriptions_keep_independent_sources_videos_and_ram_cache() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let mut config = Config::for_dir(temporary.path().join("youta"));
        config.ui.subscriptions_layout = SubscriptionsLayout::Split;
        save_fixture_subscriptions(&config, &["UCfixture", "UCsecond"]);
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.youtube_results = vec![SearchItem::Video(VideoSummary {
            video_id: "stale123456".to_owned(),
            title: "Stale search result".to_owned(),
            ..subscription_video_summary()
        })];
        controller.youtube_provider_available = true;
        let (requests, captured_requests) = unbounded();
        controller.provider_requests = Some(requests);

        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        assert!(
            captured_requests.try_recv().is_err(),
            "split navigation must not spend quota until Enter"
        );
        controller.dispatch(UiAction::ActivateSelection);
        let (first_generation, first_request) = match captured_requests
            .recv_timeout(Duration::from_secs(1))
            .expect("first channel request")
        {
            ProviderRequest::ChannelVideos {
                generation,
                request,
            } => (generation, request),
            _ => panic!("expected channel videos request"),
        };
        assert_eq!(first_request.channel_id, "UCfixture");
        assert_eq!(
            controller.current_url().as_deref(),
            Some("https://www.youtube.com/channel/UCfixture"),
            "Subscriptions must never reuse a stale Search row"
        );

        controller.dispatch(UiAction::SelectSubscriptionSource(1));
        assert!(
            captured_requests.try_recv().is_err(),
            "moving across uncached sources must not queue provider work"
        );
        controller.dispatch(UiAction::ActivateSelection);
        let (second_generation, second_request) = match captured_requests
            .recv_timeout(Duration::from_secs(1))
            .expect("second channel request")
        {
            ProviderRequest::ChannelVideos {
                generation,
                request,
            } => (generation, request),
            _ => panic!("expected second channel videos request"),
        };
        assert_eq!(second_request.channel_id, "UCsecond");
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation: first_generation,
            request: first_request.clone(),
            result: Ok(SearchPage {
                page: 1,
                items: vec![SearchItem::Video(subscription_video_summary())],
                next_page: None,
            }),
        });
        assert!(controller.view.subscriptions.items.is_empty());
        assert_eq!(
            controller.active_subscription_channel_id.as_deref(),
            Some("UCsecond"),
            "a stale response cannot replace the newly selected source"
        );

        let mut second_video = subscription_video_summary();
        second_video.video_id = "abcdefghijk".to_owned();
        second_video.channel_id = "UCsecond".to_owned();
        second_video.channel_name = "Second channel".to_owned();
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation: second_generation,
            request: second_request,
            result: Ok(SearchPage {
                page: 1,
                items: vec![SearchItem::Video(second_video)],
                next_page: None,
            }),
        });
        assert_eq!(controller.view.subscriptions.items.len(), 1);
        assert_eq!(controller.view.subscriptions.focus, SubscriptionPane::Items);
        controller.dispatch(UiAction::ToggleSubscriptionDescription);
        assert!(controller.view.subscriptions.description_expanded);
        assert_eq!(
            controller.current_url().as_deref(),
            Some("https://www.youtube.com/watch?v=abcdefghijk")
        );
        while captured_requests.try_recv().is_ok() {}

        controller.dispatch(UiAction::SelectSubscriptionSource(0));
        assert!(
            captured_requests.try_recv().is_err(),
            "selecting an uncached source remains local until Enter"
        );
        controller.dispatch(UiAction::ActivateSelection);
        let (retry_generation, retry_request) = match captured_requests
            .recv_timeout(Duration::from_secs(1))
            .expect("uncached first channel retry")
        {
            ProviderRequest::ChannelVideos {
                generation,
                request,
            } => (generation, request),
            _ => panic!("expected first channel retry"),
        };
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation: retry_generation,
            request: retry_request,
            result: Ok(SearchPage {
                page: 1,
                items: vec![SearchItem::Video(subscription_video_summary())],
                next_page: None,
            }),
        });
        controller.dispatch(UiAction::SelectSubscriptionSource(1));
        assert_eq!(controller.view.subscriptions.items.len(), 1);
        assert_no_channel_request(
            &captured_requests,
            "reselecting a cached channel must not perform another channel request",
        );
    }

    #[test]
    fn preferences_popup_saves_runtime_preferences_and_applies_them_live() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config.clone(), store, None, None);

        controller.view.text_selection_mode = true;
        controller.dispatch(UiAction::OpenPreferences);
        assert!(!controller.view.text_selection_mode);
        assert_eq!(
            controller
                .view
                .preferences_popup
                .as_ref()
                .expect("preferences popup")
                .config_path,
            config.config_file().display().to_string()
        );
        controller.dispatch(UiAction::SetSubscriptionsLayout(SubscriptionsLayout::Split));
        controller.dispatch(UiAction::ToggleYouTubePrewarm);
        controller.view.local_size_sort = LocalSizeSort::Ascending;
        controller.dispatch(UiAction::ToggleLocalFolderSizes);
        controller.dispatch(UiAction::SubmitPreferences);

        assert!(controller.view.preferences_popup.is_none());
        assert_eq!(
            controller.view.subscriptions.layout,
            SubscriptionsLayout::Split
        );
        assert!(!controller.view.local_folder_sizes_enabled);
        assert_eq!(controller.view.local_size_sort, LocalSizeSort::Off);
        let contents =
            std::fs::read_to_string(config.config_file()).expect("saved preferences file");
        assert!(contents.contains("[ui]"));
        assert!(contents.contains("subscriptions_layout = \"split\""));
        assert!(contents.contains("show_local_folder_sizes = false"));
        assert!(contents.contains("[playback]"));
        assert!(contents.contains("youtube_prewarm = false"));
        let reloaded =
            Config::load_from_dir(config.config_dir()).expect("reload saved preferences");
        assert_eq!(reloaded.ui.subscriptions_layout, SubscriptionsLayout::Split);
        assert!(!reloaded.ui.show_local_folder_sizes);
        assert!(!reloaded.playback.youtube_prewarm);
    }

    #[test]
    fn subscribe_reconciles_an_external_opml_add_without_unsubscribing_it() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config.clone(), store, None, None);
        let video = subscription_video_summary();
        controller.youtube_results = vec![SearchItem::Video(video.clone())];
        controller.view.details = Some(preliminary_detail(
            &SearchItem::Video(video),
            &controller.subscription_tree,
        ));
        controller.refresh_youtube_rows();
        assert!(
            !controller
                .view
                .details
                .as_ref()
                .expect("video details")
                .channel_subscribed
        );

        let mut external_tree = SubscriptionTree::default();
        assert!(external_tree.subscribe_youtube_channel("External title", "UCfixture"));
        subscriptions::save(&config, &external_tree).expect("external OPML edit");

        controller.dispatch(UiAction::ToggleSubscription);

        let persisted = subscriptions::load(&config).expect("reconciled OPML");
        assert_eq!(persisted.subscription_count(), 1);
        assert!(persisted.contains_youtube_channel("UCfixture"));
        assert!(
            controller
                .view
                .details
                .as_ref()
                .expect("video details")
                .channel_subscribed
        );
        assert!(controller.view.rows[0].subscribed);
    }

    #[test]
    fn channel_view_subscription_updates_the_video_back_snapshot() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        let video = subscription_video_summary();
        controller.youtube_results = vec![SearchItem::Video(video.clone())];
        controller.view.details = Some(preliminary_detail(
            &SearchItem::Video(video),
            &controller.subscription_tree,
        ));
        controller.refresh_youtube_rows();

        controller.dispatch(UiAction::ShowChannel);
        assert_eq!(controller.view.right_panel_mode, RightPanelMode::Channel);
        controller.dispatch(UiAction::ToggleSubscription);
        assert!(
            controller
                .view
                .details
                .as_ref()
                .expect("channel details")
                .channel_subscribed
        );

        controller.dispatch(UiAction::GoBack);
        let restored = controller.view.details.as_ref().expect("video details");
        assert_eq!(restored.channel_name, "Fixture channel");
        assert!(restored.channel_subscribed);
        assert!(controller.view.rows[0].subscribed);
    }

    #[test]
    fn switching_to_local_closes_a_stale_channel_panel() {
        let (mut controller, _state) = controller_with_mock_statuses([]);
        controller.view.right_panel_mode = RightPanelMode::Channel;
        controller.view.details = Some(DetailView {
            title: "Remote channel".to_owned(),
            source: "YouTube channel".to_owned(),
            ..DetailView::default()
        });

        controller.show_screen(Screen::Local);

        assert_eq!(controller.view.screen, Screen::Local);
        assert_eq!(
            controller.view.right_panel_mode,
            RightPanelMode::Details,
            "Local metadata must never render through the Channel panel"
        );
    }

    #[test]
    fn rename_cursor_edits_whole_graphemes_in_the_middle_of_a_basename() {
        let (mut controller, _state) = controller_with_mock_statuses(Vec::<PlaybackStatus>::new());
        let original = "A👩‍💻Б.flac";
        let after_emoji = "A👩‍💻".len();
        controller.view.local_file_popup = Some(LocalFilePopupView::Rename {
            value: original.to_owned(),
            cursor_byte: after_emoji,
            error: None,
        });

        controller.dispatch(UiAction::MoveLocalRenameCursor(-1));
        let Some(LocalFilePopupView::Rename { cursor_byte, .. }) =
            controller.view.local_file_popup.as_ref()
        else {
            panic!("rename popup must remain open");
        };
        assert_eq!(
            *cursor_byte,
            "A".len(),
            "joined emoji must move as one unit"
        );

        controller.dispatch(UiAction::MoveLocalRenameCursor(1));
        controller.dispatch(UiAction::AppendLocalRenameCharacter('я'));
        let Some(LocalFilePopupView::Rename {
            value, cursor_byte, ..
        }) = controller.view.local_file_popup.as_ref()
        else {
            panic!("rename popup must remain open");
        };
        assert_eq!(value, "A👩‍💻яБ.flac");
        assert_eq!(*cursor_byte, "A👩‍💻я".len());

        controller.dispatch(UiAction::DeleteLocalRenameCharacter);
        let Some(LocalFilePopupView::Rename {
            value, cursor_byte, ..
        }) = controller.view.local_file_popup.as_ref()
        else {
            panic!("rename popup must remain open");
        };
        assert_eq!(value, original);
        assert_eq!(*cursor_byte, after_emoji);
    }

    #[test]
    fn local_browser_rows_and_folder_actions_are_source_specific() {
        let fixture = tempfile::tempdir().expect("temporary Local folder");
        let album = fixture.path().join("A long album folder");
        std::fs::create_dir(&album).expect("create album folder");
        std::fs::write(fixture.path().join("song.flac"), b"fixture")
            .expect("create local audio fixture");
        let listing = crate::local_browser::list_local_directory(
            fixture.path(),
            crate::local_browser::LocalBrowseLimits::default(),
        )
        .expect("local listing");
        let (mut controller, _state) = controller_with_mock_statuses([]);
        controller.view.screen = Screen::Local;
        controller.local_listing = Some(listing);

        controller.refresh_local_browser_rows();

        assert!(controller.view.rows.iter().all(|row| row.compact));
        let parent_offset = usize::from(
            controller
                .local_listing
                .as_ref()
                .expect("listing")
                .parent
                .is_some(),
        );
        controller.view.selected = parent_offset;
        controller.update_local_browser_detail();
        let folder = controller.view.details.as_ref().expect("folder details");
        assert_eq!(folder.title, "A long album folder");
        assert!(!folder.local_renamable);
        assert!(folder.local_trashable);

        controller.view.selected = parent_offset.saturating_add(1);
        controller.update_local_browser_detail();
        let file = controller.view.details.as_ref().expect("file details");
        assert!(file.local_renamable);
        assert!(file.local_trashable);
    }

    #[test]
    fn local_playback_updates_visible_progress_and_rehydrates_it() {
        let fixture = tempfile::tempdir().expect("temporary Local folder");
        let media_directory = fixture.path().join("Music");
        std::fs::create_dir(&media_directory).expect("create Local media folder");
        let track = media_directory.join("01 - Hello World.flac");
        std::fs::write(&track, b"fixture").expect("create local audio fixture");
        let listing = crate::local_browser::list_local_directory(
            &media_directory,
            crate::local_browser::LocalBrowseLimits::default(),
        )
        .expect("local listing");
        let stale_loading = PlaybackStatus {
            idle: false,
            position: Duration::from_secs(100),
            duration: Some(Duration::from_secs(100)),
            paused: false,
            ..PlaybackStatus::default()
        };
        let missing_duration = PlaybackStatus {
            idle: false,
            position: Duration::from_secs(45),
            duration: None,
            paused: false,
            ..PlaybackStatus::default()
        };
        let active = PlaybackStatus {
            idle: false,
            position: Duration::from_secs(45),
            duration: Some(Duration::from_secs(100)),
            paused: false,
            ..PlaybackStatus::default()
        };
        let config = Config::for_dir(fixture.path().join("state"));
        let store = StateStore::open(&config).expect("disk-backed state");
        let (factory, _state, _statuses, events) =
            mock_playback_factory([stale_loading, missing_duration, active], []);
        let mut controller = AppController::new(config.clone(), store, None, Some(factory));
        controller.view.screen = Screen::Local;
        controller.local_listing = Some(listing.clone());
        controller.hydrate_local_progress_cache();
        controller.refresh_local_browser_rows();
        let media_id = MediaId::new(SourceKind::Local, track.display().to_string());
        let row_index = controller
            .view
            .rows
            .iter()
            .position(|row| row.media_id.as_ref() == Some(&media_id))
            .expect("local track row");
        controller.view.selected = row_index;

        assert_eq!(
            controller.local_progress_cache.get(&media_id),
            Some(&0),
            "first-time local media must still own a cache entry"
        );
        assert_eq!(controller.view.rows[row_index].watched_percent, 0);

        controller.activate_local_browser_selection();
        controller.update_player();

        assert_eq!(controller.playback_phase, PlaybackPhase::Loading);
        assert_eq!(
            controller.view.rows[row_index].watched_percent, 0,
            "a status retained from replaced media must not advance the new file"
        );
        events
            .lock()
            .expect("mock events")
            .extend([PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted]);
        controller.update_player();

        assert_eq!(controller.playback_phase, PlaybackPhase::Playing);
        assert_eq!(
            controller.view.rows[row_index].watched_percent, 0,
            "a temporarily unknown duration must not erase or invent progress"
        );
        controller.show_screen(Screen::Playlists);
        controller.update_player();
        assert_eq!(controller.local_progress_cache.get(&media_id), Some(&45));
        assert!(
            controller.view.rows.is_empty(),
            "another screen must not retain the Local browser rows"
        );
        controller.show_screen(Screen::Local);
        let row_index = controller
            .view
            .rows
            .iter()
            .position(|row| row.media_id.as_ref() == Some(&media_id))
            .expect("restored Local track row");
        assert_eq!(
            controller.view.rows[row_index].watched_percent, 45,
            "returning to Local must rebuild from the live RAM cache"
        );

        controller.persist_position();
        assert_eq!(
            controller
                .store
                .progress(&media_id)
                .expect("saved partial progress")
                .map(|progress| progress.watched_percent()),
            Some(45)
        );

        events
            .lock()
            .expect("mock events")
            .push_back(PlaybackEvent::Ended(PlaybackEnd {
                reason: PlaybackEndReason::Eof,
                error: None,
                file_error: None,
                diagnostic: None,
            }));
        controller.update_player();

        assert_eq!(controller.view.rows[row_index].watched_percent, 100);
        assert_eq!(controller.local_progress_cache.get(&media_id), Some(&100));
        assert_eq!(
            controller
                .store
                .progress(&media_id)
                .expect("saved finished progress")
                .map(|progress| progress.watched_percent()),
            Some(100)
        );

        drop(controller);
        let store = StateStore::open(&config).expect("reopen disk-backed state");
        let mut restarted = AppController::new(config, store, None, None);
        restarted.view.screen = Screen::Local;
        restarted.local_listing = Some(listing);
        restarted.hydrate_local_progress_cache();
        restarted.rebuild_local_browser_rows();
        let rehydrated = restarted
            .view
            .rows
            .iter()
            .find(|row| row.media_id.as_ref() == Some(&media_id))
            .expect("local row rehydrated after a real database reopen");
        assert_eq!(rehydrated.watched_percent, 100);
    }

    #[cfg(all(feature = "local", feature = "thumbnails"))]
    #[test]
    fn local_artwork_worker_response_updates_only_the_matching_details_path() {
        let (mut controller, _state) = controller_with_mock_statuses([]);
        let media_path = PathBuf::from("/tmp/youta-local-artwork.flac");
        controller.view.details = Some(DetailView {
            media_id: Some(MediaId::new(
                SourceKind::Local,
                media_path.display().to_string(),
            )),
            ..DetailView::default()
        });
        controller.pending_local_artwork = Some((7, media_path.clone()));
        let artwork =
            url::Url::parse("file:///tmp/youta-local-artwork-cover.png").expect("artwork URL");

        controller.handle_provider_response(ProviderResponse::LocalArtwork {
            generation: 7,
            path: media_path.clone(),
            result: Ok(Some(artwork.clone())),
        });

        assert_eq!(
            controller
                .view
                .details
                .as_ref()
                .and_then(|details| details.thumbnail_url.as_ref()),
            Some(&artwork)
        );
        assert_eq!(
            controller
                .local_artwork_cache
                .get(&media_path)
                .and_then(Option::as_ref),
            Some(&artwork)
        );
        assert!(controller.pending_local_artwork.is_none());
        assert!(!controller.view.local_artwork_pending);
    }

    #[cfg(feature = "local")]
    #[test]
    fn local_vorbis_metadata_maps_human_fields_and_identifiers() {
        let mut item = LocalMediaItem {
            path: PathBuf::from("/tmp/mock.flac"),
            title: "mock".to_owned(),
            artist: None,
            album: None,
            genre: None,
            comment: None,
            metadata_url: None,
            acoustid_id: None,
            duration_seconds: None,
            size_bytes: 0,
            codec: "FLAC".to_owned(),
            bitrate_kbps: None,
            sample_rate_hz: None,
            channels: None,
            embedded_artwork: false,
        };
        let mut tag = lofty::ogg::VorbisComments::new();
        for (key, value) in [
            ("TITLE", "Mock title"),
            ("ARTISTS", "First artist"),
            ("ARTISTS", "Second artist"),
            ("ALBUM", "Mock album"),
            ("GENRE", "Trip hop"),
            ("COMMENT", "Visit https://artist.example"),
            ("URL", " https://release.example\n"),
            ("ACOUSTID_ID", "39e08d1e-ce43-465f-a802-041995e9d150"),
        ] {
            tag.push(key.to_owned(), value.to_owned());
        }

        apply_local_vorbis_comments(&mut item, &tag);

        assert_eq!(item.title, "Mock title");
        assert_eq!(item.artist.as_deref(), Some("First artist; Second artist"));
        assert_eq!(item.album.as_deref(), Some("Mock album"));
        assert_eq!(item.genre.as_deref(), Some("Trip hop"));
        assert_eq!(
            item.comment.as_deref(),
            Some("Visit https://artist.example")
        );
        assert_eq!(
            item.metadata_url.as_deref(),
            Some("https://release.example")
        );
        assert_eq!(
            item.acoustid_id.as_deref(),
            Some("39e08d1e-ce43-465f-a802-041995e9d150")
        );
    }

    #[test]
    fn navigating_to_parent_reselects_the_folder_just_left() {
        let fixture = tempfile::tempdir().expect("temporary Local folder");
        let child = fixture.path().join("album");
        std::fs::create_dir(&child).expect("create child folder");
        let child_listing = crate::local_browser::list_local_directory(
            &child,
            crate::local_browser::LocalBrowseLimits::default(),
        )
        .expect("child listing");
        let (mut controller, _state) = controller_with_mock_statuses([]);
        controller.view.screen = Screen::Local;
        controller.config.ui.show_local_folder_sizes = false;
        controller.view.local_folder_sizes_enabled = false;
        controller.local_listing = Some(child_listing);
        controller.refresh_local_browser_rows();
        controller.view.selected = 0;
        let (requests, captured) = unbounded();
        controller.local_browse_requests = Some(requests);

        controller.activate_local_browser_selection();

        let LocalBrowseRequest::Browse {
            generation,
            directory,
            preferred_child,
        } = captured.try_recv().expect("parent browse request")
        else {
            panic!("expected Local parent browse request");
        };
        assert_eq!(directory, fixture.path());
        assert_eq!(preferred_child.as_deref(), Some(child.as_path()));
        let parent_listing = crate::local_browser::list_local_directory_with_preferred_child(
            &directory,
            crate::local_browser::LocalBrowseLimits::default(),
            preferred_child.as_deref(),
        )
        .expect("parent listing");

        controller.handle_local_browse_response(LocalBrowseResponse {
            generation,
            result: Ok(parent_listing),
        });

        assert_eq!(
            controller.selected_local_path().as_deref(),
            Some(child.as_path())
        );
        assert_eq!(
            controller
                .view
                .rows
                .get(controller.view.selected)
                .map(|row| row.title.as_str()),
            Some("album")
        );
    }

    #[test]
    fn pending_local_navigation_keeps_rows_visible_but_noninteractive() {
        let fixture = tempfile::tempdir().expect("temporary Local folder");
        let child = fixture.path().join("album");
        std::fs::create_dir(&child).expect("create child folder");
        let listing = crate::local_browser::list_local_directory(
            fixture.path(),
            crate::local_browser::LocalBrowseLimits::default(),
        )
        .expect("listing");
        let (mut controller, _state) = controller_with_mock_statuses([]);
        controller.view.screen = Screen::Local;
        controller.local_listing = Some(listing);
        controller.refresh_local_browser_rows();
        let visible_rows = controller
            .view
            .rows
            .iter()
            .map(|row| row.title.clone())
            .collect::<Vec<_>>();
        let (requests, captured) = unbounded();
        controller.local_browse_requests = Some(requests);

        controller.browse_local_directory(child.clone());

        assert_eq!(
            controller
                .view
                .rows
                .iter()
                .map(|row| row.title.clone())
                .collect::<Vec<_>>(),
            visible_rows,
            "foreground navigation must not blank the existing Local rows"
        );
        assert!(
            controller.local_listing.is_none(),
            "stale visible rows must have no activatable listing owner"
        );
        assert!(
            controller.view.local_browse_pending,
            "the TUI must use its short response-poll budget while listing"
        );
        controller.activate_local_browser_selection();
        assert_eq!(controller.view.status_line, "No local folder is loaded");
        let Ok(LocalBrowseRequest::Browse {
            generation,
            directory,
            ..
        }) = captured.try_recv()
        else {
            panic!("expected Local browse request");
        };
        assert_eq!(directory, child);
        let child_listing = crate::local_browser::list_local_directory(
            &child,
            crate::local_browser::LocalBrowseLimits::default(),
        )
        .expect("child listing");
        controller.handle_local_browse_response(LocalBrowseResponse {
            generation,
            result: Ok(child_listing),
        });
        assert!(
            !controller.view.local_browse_pending,
            "accepted directory response must restore the idle cadence"
        );
    }

    #[test]
    fn trash_refresh_preserves_rows_and_reuses_unchanged_sibling_size() {
        let fixture = tempfile::tempdir().expect("temporary Local folder");
        let removed = fixture.path().join("remove-me");
        let sibling = fixture.path().join("sibling");
        std::fs::create_dir(&removed).expect("create removed folder");
        std::fs::create_dir(&sibling).expect("create sibling folder");
        let listing = crate::local_browser::list_local_directory(
            fixture.path(),
            crate::local_browser::LocalBrowseLimits::default(),
        )
        .expect("listing");
        let (mut controller, _state) = controller_with_mock_statuses([]);
        controller.view.screen = Screen::Local;
        controller.local_listing = Some(listing);
        controller.invalidate_local_folder_sizes();
        let generation = controller
            .local_folder_size_generation
            .load(AtomicOrdering::Relaxed);
        let sibling_identity =
            crate::local_browser::local_directory_identity(&sibling).expect("sibling identity");
        assert!(controller.store_local_folder_size(
            &sibling,
            generation,
            LocalFolderSizeWorkerResult {
                measurement: crate::local_browser::LocalFolderSizeMeasurement {
                    bytes: 42,
                    identity: sibling_identity,
                },
                reused: false,
            },
        ));
        controller.select_local_path(Some(&removed));
        controller.refresh_local_browser_rows();
        let (browse_requests, captured_browse) = unbounded();
        controller.local_browse_requests = Some(browse_requests);
        let (size_requests, captured_sizes) = unbounded();
        controller.provider_requests = Some(size_requests);
        std::fs::remove_dir(&removed).expect("simulate confirmed Trash");

        let reselected = controller.remove_confirmed_local_trash_target(&removed);
        controller.browse_local_directory_with_reselection(fixture.path().to_owned(), reselected);

        assert!(!controller.view.rows.is_empty());
        assert!(
            controller
                .view
                .rows
                .iter()
                .all(|row| row.title != "remove-me"),
            "a confirmed Trash target must disappear optimistically"
        );
        let LocalBrowseRequest::Browse {
            generation,
            directory,
            preferred_child,
        } = captured_browse.try_recv().expect("refresh request")
        else {
            panic!("expected Local browse request");
        };
        let refreshed = crate::local_browser::list_local_directory_with_preferred_child(
            &directory,
            crate::local_browser::LocalBrowseLimits::default(),
            preferred_child.as_deref(),
        )
        .expect("refreshed listing");
        controller.handle_local_browse_response(LocalBrowseResponse {
            generation,
            result: Ok(refreshed),
        });

        assert!(captured_sizes.try_recv().is_err());
        assert_eq!(
            controller
                .view
                .rows
                .iter()
                .find(|row| row.title == "sibling")
                .map(|row| row.subtitle.as_str()),
            Some("42 B"),
            "an identity-matching sibling must reuse its RAM-cached size"
        );
    }

    #[cfg(all(feature = "local", feature = "thumbnails"))]
    #[test]
    fn folder_details_omit_duplicate_size_and_load_cover_lazily() {
        let fixture = tempfile::tempdir().expect("temporary Local folder");
        let album = fixture.path().join("album");
        std::fs::create_dir(&album).expect("create album");
        let cover = album.join("CoVeR.JpG");
        std::fs::write(&cover, b"cover bytes").expect("cover fixture");
        let listing = crate::local_browser::list_local_directory(
            fixture.path(),
            crate::local_browser::LocalBrowseLimits::default(),
        )
        .expect("listing");
        let (mut controller, _state) = controller_with_mock_statuses([]);
        controller.view.screen = Screen::Local;
        controller.local_listing = Some(listing);
        controller.invalidate_local_folder_sizes();
        let generation = controller
            .local_folder_size_generation
            .load(AtomicOrdering::Relaxed);
        let identity =
            crate::local_browser::local_directory_identity(&album).expect("album identity");
        assert!(controller.store_local_folder_size(
            &album,
            generation,
            LocalFolderSizeWorkerResult {
                measurement: crate::local_browser::LocalFolderSizeMeasurement {
                    bytes: 11,
                    identity,
                },
                reused: false,
            },
        ));
        let (requests, captured) = unbounded();
        controller.provider_requests = Some(requests);
        controller.select_local_path(Some(&album));
        controller.refresh_local_browser_rows();

        let details = controller.view.details.as_ref().expect("folder details");
        assert!(!details.description.contains("\nSize:"));
        let ProviderRequest::LocalArtwork {
            generation,
            path,
            kind,
            ..
        } = captured.try_recv().expect("lazy folder artwork request")
        else {
            panic!("expected Local artwork request");
        };
        assert!(controller.view.local_artwork_pending);
        assert_eq!(path, album);
        assert!(matches!(kind, LocalArtworkKind::FolderCover));
        let cover_url = url::Url::from_file_path(&cover).expect("cover URL");
        controller.handle_provider_response(ProviderResponse::LocalArtwork {
            generation,
            path,
            result: Ok(Some(cover_url.clone())),
        });
        assert_eq!(
            controller
                .view
                .details
                .as_ref()
                .and_then(|details| details.thumbnail_url.as_ref()),
            Some(&cover_url)
        );
        assert!(!controller.view.local_artwork_pending);
    }

    #[test]
    fn local_folder_size_batches_rotate_and_failures_do_not_retry_within_ttl() {
        use crate::local_browser::{LocalDirectoryListing, LocalEntry, LocalEntryKind};

        let root = PathBuf::from("/tmp/youta-folder-size-batch-test");
        let entries = (0..300)
            .map(|index| {
                let name = std::ffi::OsString::from(format!("folder-{index:03}"));
                LocalEntry {
                    path: root.join(&name),
                    name,
                    kind: LocalEntryKind::Directory,
                    size_bytes: None,
                    image_dimensions: None,
                    directory_identity: None,
                }
            })
            .collect::<Vec<_>>();
        let (mut controller, _state) = controller_with_mock_statuses([]);
        controller.view.screen = Screen::Local;
        controller.local_listing = Some(LocalDirectoryListing {
            path: root.clone(),
            parent: root.parent().map(Path::to_owned),
            entries,
            truncated: false,
            inspected_entries: 300,
        });
        controller.invalidate_local_folder_sizes();
        let (requests, captured) = unbounded();
        controller.provider_requests = Some(requests);

        controller.schedule_local_folder_sizes();

        assert_eq!(
            controller.local_folder_size_queue.len()
                + usize::from(controller.local_folder_size_pending.is_some()),
            MAX_LAZY_LOCAL_FOLDER_SIZES
        );
        let ProviderRequest::LocalFolderSize {
            path: first_path, ..
        } = captured.try_recv().expect("first size request")
        else {
            panic!("expected Local folder-size request");
        };
        assert_eq!(first_path, root.join("folder-000"));

        for index in 0..MAX_LAZY_LOCAL_FOLDER_SIZES {
            controller.store_local_folder_size_failure(root.join(format!("folder-{index:03}")));
        }
        controller.invalidate_local_folder_sizes();
        while captured.try_recv().is_ok() {}
        controller.schedule_local_folder_sizes();

        assert_eq!(
            controller.local_folder_size_queue.len()
                + usize::from(controller.local_folder_size_pending.is_some()),
            300 - MAX_LAZY_LOCAL_FOLDER_SIZES,
            "the next visit must advance beyond the bounded first batch"
        );
        let ProviderRequest::LocalFolderSize {
            path: next_path, ..
        } = captured.try_recv().expect("rotated size request")
        else {
            panic!("expected rotated Local folder-size request");
        };
        assert_eq!(next_path, root.join("folder-256"));
        assert!(
            !controller
                .local_folder_size_queue
                .iter()
                .any(|path| path == &root.join("folder-000")),
            "a fresh negative cache entry must suppress immediate retries"
        );

        controller.config.ui.show_local_folder_sizes = false;
        controller.view.local_folder_sizes_enabled = false;
        controller.invalidate_local_folder_sizes();
        while captured.try_recv().is_ok() {}
        controller.schedule_local_folder_sizes();
        assert!(controller.local_folder_size_pending.is_none());
        assert!(controller.local_folder_size_queue.is_empty());
        assert!(captured.try_recv().is_err());
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one fixture verifies both sort directions and cache reuse invariants"
    )]
    fn local_size_sort_keeps_unknown_folders_last_and_preserves_selected_path() {
        use crate::local_browser::LocalEntryKind;

        let fixture = tempfile::tempdir().expect("temporary Local folder");
        let small = fixture.path().join("small");
        let large = fixture.path().join("large");
        let unknown = fixture.path().join("unknown");
        for directory in [&small, &large, &unknown] {
            std::fs::create_dir(directory).expect("create folder fixture");
        }
        std::fs::write(fixture.path().join("a.flac"), b"a").expect("small file fixture");
        std::fs::write(fixture.path().join("b.flac"), b"bb").expect("large file fixture");
        let listing = crate::local_browser::list_local_directory(
            fixture.path(),
            crate::local_browser::LocalBrowseLimits::default(),
        )
        .expect("local listing");
        let (mut controller, _state) = controller_with_mock_statuses([]);
        controller.view.screen = Screen::Local;
        controller.local_listing = Some(listing);
        controller.invalidate_local_folder_sizes();
        let generation = controller
            .local_folder_size_generation
            .load(AtomicOrdering::Relaxed);
        for (path, bytes) in [(&small, 10_u64), (&large, 100_u64)] {
            let identity =
                crate::local_browser::local_directory_identity(path).expect("folder identity");
            assert!(controller.store_local_folder_size(
                path,
                generation,
                LocalFolderSizeWorkerResult {
                    measurement: crate::local_browser::LocalFolderSizeMeasurement {
                        bytes,
                        identity,
                    },
                    reused: false,
                },
            ));
        }
        controller.sort_local_listing();
        controller.select_local_path(Some(&unknown));

        controller.toggle_local_size_sort();

        assert_eq!(controller.view.local_size_sort, LocalSizeSort::Ascending);
        assert_eq!(
            controller.selected_local_path().as_deref(),
            Some(unknown.as_path())
        );
        let ascending = controller
            .local_listing
            .as_ref()
            .expect("listing")
            .entries
            .iter()
            .map(|entry| entry.display_name().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(ascending, ["a.flac", "b.flac", "small", "large", "unknown"]);
        assert_eq!(
            controller
                .view
                .rows
                .iter()
                .find(|row| row.title == "small")
                .map(|row| row.subtitle.as_str()),
            Some("10 B")
        );

        controller.toggle_local_size_sort();
        let descending = controller
            .local_listing
            .as_ref()
            .expect("listing")
            .entries
            .iter()
            .map(|entry| entry.display_name().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            descending,
            ["large", "small", "b.flac", "a.flac", "unknown"]
        );
        assert_eq!(
            controller.selected_local_path().as_deref(),
            Some(unknown.as_path())
        );
        assert!(
            controller
                .local_listing
                .as_ref()
                .expect("listing")
                .entries
                .last()
                .is_some_and(|entry| {
                    entry.kind == LocalEntryKind::Directory && entry.path == unknown
                }),
            "unknown folders must remain after known sizes in descending order"
        );

        controller.invalidate_local_folder_sizes();
        controller.refresh_local_browser_rows();
        assert_eq!(
            controller
                .view
                .rows
                .iter()
                .find(|row| row.title == "small")
                .map(|row| row.subtitle.as_str()),
            Some(""),
            "a new generation must not display a cache entry before identity validation"
        );
        let (requests, captured) = unbounded();
        controller.provider_requests = Some(requests);
        controller.schedule_local_folder_sizes();
        let ProviderRequest::LocalFolderSize {
            path: requested, ..
        } = captured.try_recv().expect("one missing folder request")
        else {
            panic!("expected Local folder-size request");
        };
        assert_eq!(requested, unknown);
        assert_eq!(
            controller
                .local_folder_size_cache
                .get(&small)
                .map(|cached| cached.validated_generation),
            Some(
                controller
                    .local_folder_size_generation
                    .load(AtomicOrdering::Relaxed)
            ),
            "fresh identity-matching RAM cache entries must survive navigation"
        );
        assert_eq!(
            controller
                .view
                .rows
                .iter()
                .find(|row| row.title == "small")
                .map(|row| row.subtitle.as_str()),
            Some("10 B"),
            "synchronous RAM-cache validation must refresh visible folder sizes"
        );
    }

    #[test]
    fn lazy_folder_size_response_reorders_without_moving_selection() {
        let fixture = tempfile::tempdir().expect("temporary Local folder");
        let measured = fixture.path().join("measured");
        let selected = fixture.path().join("selected");
        std::fs::create_dir(&measured).expect("create measured folder");
        std::fs::create_dir(&selected).expect("create selected folder");
        let listing = crate::local_browser::list_local_directory(
            fixture.path(),
            crate::local_browser::LocalBrowseLimits::default(),
        )
        .expect("local listing");
        let (mut controller, _state) = controller_with_mock_statuses([]);
        controller.view.screen = Screen::Local;
        controller.view.local_size_sort = LocalSizeSort::Ascending;
        controller.local_listing = Some(listing);
        controller.invalidate_local_folder_sizes();
        controller.select_local_path(Some(&selected));
        let generation = controller
            .local_folder_size_generation
            .load(AtomicOrdering::Relaxed);
        controller.local_folder_size_pending = Some(measured.clone());
        let identity =
            crate::local_browser::local_directory_identity(&measured).expect("folder identity");

        controller.handle_provider_response(ProviderResponse::LocalFolderSize {
            generation,
            path: measured,
            result: Ok(LocalFolderSizeWorkerResult {
                measurement: crate::local_browser::LocalFolderSizeMeasurement {
                    bytes: 42,
                    identity,
                },
                reused: false,
            }),
        });

        assert_eq!(
            controller.selected_local_path().as_deref(),
            Some(selected.as_path())
        );
    }

    #[test]
    fn shutdown_cancels_an_inflight_local_folder_measurement_generation() {
        let (mut controller, _state) = controller_with_mock_statuses([]);
        let before = controller
            .local_folder_size_generation
            .load(AtomicOrdering::Relaxed);

        controller.shutdown();

        assert_ne!(
            controller
                .local_folder_size_generation
                .load(AtomicOrdering::Relaxed),
            before
        );
    }

    #[test]
    fn local_subscription_save_failure_rolls_back_cache_and_view() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let blocked_root = temporary.path().join("not-a-directory");
        std::fs::write(&blocked_root, b"fixture").expect("blocking file");
        let config = Config::for_dir(&blocked_root);
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        let video = subscription_video_summary();
        controller.youtube_results = vec![SearchItem::Video(video.clone())];
        controller.view.details = Some(preliminary_detail(
            &SearchItem::Video(video),
            &controller.subscription_tree,
        ));
        controller.refresh_youtube_rows();

        controller.dispatch(UiAction::ToggleSubscription);

        assert!(
            !controller
                .subscription_tree
                .contains_youtube_channel("UCfixture")
        );
        assert!(
            !controller
                .view
                .details
                .as_ref()
                .expect("video details")
                .channel_subscribed
        );
        assert!(!controller.view.rows[0].subscribed);
        assert_eq!(
            controller
                .view
                .error_popup
                .as_ref()
                .expect("save diagnostic")
                .title,
            "Cannot save local subscriptions"
        );
        assert_eq!(
            std::fs::read(&blocked_root).expect("blocking file remains"),
            b"fixture"
        );
    }

    #[test]
    fn malformed_existing_opml_is_never_replaced_by_subscribe() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        config
            .ensure_directories()
            .expect("configuration directories");
        let malformed = b"<opml><body><outline";
        std::fs::write(config.subscriptions_file(), malformed).expect("malformed OPML fixture");
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config.clone(), store, None, None);
        assert_eq!(
            controller
                .view
                .error_popup
                .as_ref()
                .expect("startup OPML diagnostic")
                .title,
            "Could not restore local subscriptions"
        );
        controller.view.error_popup = None;
        let video = subscription_video_summary();
        controller.youtube_results = vec![SearchItem::Video(video.clone())];
        controller.view.details = Some(preliminary_detail(
            &SearchItem::Video(video),
            &controller.subscription_tree,
        ));
        controller.refresh_youtube_rows();

        controller.dispatch(UiAction::ToggleSubscription);

        assert_eq!(
            std::fs::read(config.subscriptions_file()).expect("unchanged malformed OPML"),
            malformed
        );
        assert!(
            !controller
                .subscription_tree
                .contains_youtube_channel("UCfixture")
        );
        assert!(
            !controller
                .view
                .details
                .as_ref()
                .expect("video details")
                .channel_subscribed
        );
        assert_eq!(
            controller
                .view
                .error_popup
                .as_ref()
                .expect("mutation diagnostic")
                .title,
            "Cannot change local subscriptions"
        );
    }

    #[test]
    fn selecting_a_channel_invalidates_a_late_video_detail_response() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.details_generation = 8;
        controller.youtube_results = vec![SearchItem::Channel(ChannelSummary {
            channel_id: "UCchannel".to_owned(),
            name: "Selected channel".to_owned(),
            description: "Channel description".to_owned(),
            subscriber_count: Some(10),
            video_count: Some(2),
            created_at: None,
            auto_generated: false,
            thumbnails: Vec::new(),
            webpage_url: None,
        })];
        controller.request_selected_details();
        assert_eq!(controller.details_generation, 9);

        controller.handle_provider_response(ProviderResponse::Details {
            generation: 8,
            result: Ok(subscription_video_details("Late video response")),
        });

        let details = controller.view.details.as_ref().expect("channel details");
        assert_eq!(details.title, "Selected channel");
        assert_eq!(details.channel_name, "Selected channel");
        assert_eq!(details.channel_id, "UCchannel");
    }

    #[test]
    fn thumbnail_selection_avoids_downloading_an_unnecessary_maximum_resolution_image() {
        let thumbnail = |name: &str, width, height| Thumbnail {
            url: url::Url::parse(&format!("https://images.example/{name}.jpg"))
                .expect("fixture thumbnail URL"),
            quality: Some(name.to_owned()),
            width,
            height,
        };
        let candidates = [
            thumbnail("small", Some(320), Some(180)),
            thumbnail("maxres", Some(1_280), Some(720)),
            thumbnail("medium", Some(640), Some(360)),
        ];

        assert_eq!(
            preferred_thumbnail_url(&candidates)
                .as_ref()
                .map(url::Url::as_str),
            Some("https://images.example/medium.jpg")
        );
        assert_eq!(preferred_thumbnail_url(&[]), None);
        assert_eq!(
            preferred_thumbnail_url(&[
                thumbnail("unknown-first", None, None),
                thumbnail("unknown-last", None, None),
            ])
            .as_ref()
            .map(url::Url::as_str),
            Some("https://images.example/unknown-last.jpg")
        );
    }

    #[test]
    fn global_search_rows_expose_preferred_video_and_channel_thumbnails_for_prefetch() {
        let thumbnail = |name: &str, width| Thumbnail {
            url: url::Url::parse(&format!("https://images.example/{name}.jpg"))
                .expect("fixture thumbnail URL"),
            quality: Some(name.to_owned()),
            width,
            height: width.map(|value| value / 2),
        };
        let candidates = vec![
            thumbnail("small", Some(320)),
            thumbnail("maxres", Some(1_280)),
            thumbnail("medium", Some(640)),
        ];
        let store = StateStore::open_in_memory().expect("in-memory state");
        let subscriptions = SubscriptionTree::default();
        let subscribers = HashMap::new();
        let mut video = subscription_video_summary();
        video.thumbnails.clone_from(&candidates);
        let video_row = row_from_search_item(
            &SearchItem::Video(video),
            &store,
            &subscriptions,
            &subscribers,
            SearchRowContext::GlobalSearch,
            NaiveDate::from_ymd_opt(2026, 7, 26).expect("valid fixture date"),
        );
        let channel_row = row_from_search_item(
            &SearchItem::Channel(ChannelSummary {
                channel_id: "UCfixture".to_owned(),
                name: "Fixture channel".to_owned(),
                description: String::new(),
                subscriber_count: None,
                video_count: None,
                created_at: None,
                auto_generated: false,
                thumbnails: candidates,
                webpage_url: None,
            }),
            &store,
            &subscriptions,
            &subscribers,
            SearchRowContext::GlobalSearch,
            NaiveDate::from_ymd_opt(2026, 7, 26).expect("valid fixture date"),
        );

        for row in [video_row, channel_row] {
            assert_eq!(
                row.thumbnail_url.as_ref().map(url::Url::as_str),
                Some("https://images.example/medium.jpg")
            );
        }
    }

    #[test]
    fn lazy_video_details_recolor_a_confirmed_vertical_search_row() {
        let config = Config::for_dir("/tmp/youta-vertical-details-test");
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.youtube_results = vec![SearchItem::Video(subscription_video_summary())];
        controller.refresh_youtube_rows();
        assert!(!controller.view.rows[0].vertical);

        let mut details = subscription_video_details("Vertical fixture");
        details.orientation = VideoOrientation::Vertical;
        controller.handle_provider_response(ProviderResponse::Details {
            generation: controller.details_generation,
            result: Ok(details),
        });

        assert!(controller.view.rows[0].vertical);
        assert!(matches!(
            &controller.youtube_results[0],
            SearchItem::Video(video) if video.orientation == VideoOrientation::Vertical
        ));
    }

    #[test]
    fn incomplete_details_preserve_known_and_saved_video_orientation() {
        let config = Config::for_dir("/tmp/youta-preserved-orientation-test");
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        let request = SearchRequest::new("vertical fixture", SearchTarget::Videos);
        let mut summary = subscription_video_summary();
        summary.orientation = VideoOrientation::Vertical;
        controller.youtube_search_request = Some(request.clone());
        controller.youtube_results = vec![SearchItem::Video(summary.clone())];
        controller
            .store
            .save_youtube_search(
                &SavedYouTubeSearch {
                    request,
                    results: vec![SearchItem::Video(summary)],
                    next_page: None,
                },
                1,
            )
            .expect("save search fixture");
        controller.refresh_youtube_rows();

        let mut details = subscription_video_details("Incomplete orientation fixture");
        details.orientation = VideoOrientation::Unknown;
        controller.handle_provider_response(ProviderResponse::Details {
            generation: controller.details_generation,
            result: Ok(details),
        });

        assert!(controller.view.rows[0].vertical);
        assert!(matches!(
            &controller.youtube_results[0],
            SearchItem::Video(video) if video.orientation == VideoOrientation::Vertical
        ));
        let saved = controller
            .store
            .youtube_search()
            .expect("read saved search")
            .expect("saved search fixture");
        assert!(matches!(
            &saved.results[0],
            SearchItem::Video(video) if video.orientation == VideoOrientation::Vertical
        ));
    }

    #[test]
    fn only_provider_confirmed_vertical_videos_mark_youtube_rows() {
        let store = StateStore::open_in_memory().expect("in-memory state");
        let subscriptions = SubscriptionTree::default();
        let subscribers = HashMap::new();
        let today = NaiveDate::from_ymd_opt(2026, 7, 26).expect("valid fixture date");
        let row_for = |orientation| {
            let mut video = subscription_video_summary();
            video.orientation = orientation;
            row_from_search_item(
                &SearchItem::Video(video),
                &store,
                &subscriptions,
                &subscribers,
                SearchRowContext::GlobalSearch,
                today,
            )
        };

        assert!(row_for(VideoOrientation::Vertical).vertical);
        assert!(!row_for(VideoOrientation::Horizontal).vertical);
        assert!(!row_for(VideoOrientation::Square).vertical);
        assert!(!row_for(VideoOrientation::Unknown).vertical);
    }

    #[test]
    fn hiding_non_cc_license_rows_does_not_change_commons_upload_gating() {
        assert!(
            media_license_from_label("Creative Commons Attribution")
                .is_potentially_commons_compatible()
        );
        assert!(
            media_license_from_label("https://creativecommons.org/licenses/by/4.0/")
                .is_potentially_commons_compatible()
        );
        assert!(media_license_from_label("public domain").is_potentially_commons_compatible());
        assert!(
            !media_license_from_label("Standard YouTube License")
                .is_potentially_commons_compatible()
        );
        assert!(!media_license_from_label("unknown").is_potentially_commons_compatible());
    }

    #[derive(Default)]
    struct MockPlaybackState {
        played: Vec<PlaybackInput>,
        commands: Vec<PlayerCommand>,
    }

    struct MockPlaybackBackend {
        state: Arc<Mutex<MockPlaybackState>>,
        statuses: Arc<Mutex<VecDeque<crate::playback::PlaybackStatus>>>,
        events: Arc<Mutex<VecDeque<PlaybackEvent>>>,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum DiagnosticCall {
        Copy(String),
        Fill { title: String, report: String },
        CopyAndOpen { title: String, report: String },
    }

    struct MockDiagnosticActions {
        calls: Arc<Mutex<Vec<DiagnosticCall>>>,
        gh_available: bool,
    }

    #[cfg(any(feature = "youtube-official", feature = "invidious"))]
    struct MockYouTubeProviderBuilder;

    #[cfg(any(feature = "youtube-official", feature = "invidious"))]
    struct EmptyYouTubeProvider;

    struct CountingYouTubeProvider {
        searches: Arc<AtomicUsize>,
        details: Arc<AtomicUsize>,
    }

    #[cfg(any(feature = "youtube-official", feature = "invidious"))]
    impl YouTubeProviderBuilder for MockYouTubeProviderBuilder {
        fn official(&self, _api_key: String) -> Result<Box<dyn Provider>, String> {
            Ok(Box::new(EmptyYouTubeProvider))
        }

        fn invidious(&self, _base_url: url::Url) -> Result<Box<dyn Provider>, String> {
            Ok(Box::new(EmptyYouTubeProvider))
        }
    }

    #[cfg(any(feature = "youtube-official", feature = "invidious"))]
    impl Provider for EmptyYouTubeProvider {
        fn id(&self) -> &'static str {
            "mock-youtube"
        }

        fn display_name(&self) -> &'static str {
            "Mock YouTube"
        }

        fn capabilities(&self) -> crate::providers::ProviderCapabilities {
            crate::providers::ProviderCapabilities {
                video_search: true,
                channel_search: true,
                pagination: true,
                video_details: true,
                thumbnails: true,
                ..crate::providers::ProviderCapabilities::default()
            }
        }

        fn search(
            &self,
            request: &SearchRequest,
        ) -> Result<SearchPage, crate::providers::ProviderError> {
            Ok(SearchPage {
                page: request.page,
                items: Vec::new(),
                next_page: None,
            })
        }

        fn video_details(
            &self,
            _video_id: &str,
        ) -> Result<VideoDetails, crate::providers::ProviderError> {
            Err(crate::providers::ProviderError::Unsupported)
        }
    }

    impl Provider for CountingYouTubeProvider {
        fn id(&self) -> &'static str {
            "counting-youtube"
        }

        fn display_name(&self) -> &'static str {
            "Counting YouTube"
        }

        fn capabilities(&self) -> crate::providers::ProviderCapabilities {
            crate::providers::ProviderCapabilities {
                video_search: true,
                pagination: true,
                video_details: true,
                ..crate::providers::ProviderCapabilities::default()
            }
        }

        fn search(
            &self,
            request: &SearchRequest,
        ) -> Result<SearchPage, crate::providers::ProviderError> {
            self.searches.fetch_add(1, Ordering::SeqCst);
            Ok(SearchPage {
                page: request.page,
                items: Vec::new(),
                next_page: None,
            })
        }

        fn video_details(
            &self,
            _video_id: &str,
        ) -> Result<VideoDetails, crate::providers::ProviderError> {
            self.details.fetch_add(1, Ordering::SeqCst);
            Ok(subscription_video_details("Lazy restored details"))
        }
    }

    #[cfg(feature = "yt-dlp")]
    struct MockRunningDownload {
        progress: Option<Cursor<Vec<u8>>>,
        errors: Option<Cursor<Vec<u8>>>,
        exits: VecDeque<Result<Option<DownloadExit>, String>>,
        cancelled: Arc<AtomicBool>,
    }

    #[cfg(feature = "yt-dlp")]
    impl RunningDownload for MockRunningDownload {
        fn take_progress_reader(&mut self) -> Option<Box<dyn BufRead + Send>> {
            self.progress
                .take()
                .map(|reader| Box::new(reader) as Box<dyn BufRead + Send>)
        }

        fn take_error_reader(&mut self) -> Option<Box<dyn BufRead + Send>> {
            self.errors
                .take()
                .map(|reader| Box::new(reader) as Box<dyn BufRead + Send>)
        }

        fn try_wait(&mut self) -> Result<Option<DownloadExit>, String> {
            self.exits.pop_front().unwrap_or(Ok(None))
        }

        fn cancel(&mut self) -> Result<(), String> {
            self.cancelled.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[cfg(feature = "yt-dlp")]
    struct MockDownloadLauncher {
        requests: Arc<Mutex<Vec<DownloadRequest>>>,
        process: Option<Box<dyn RunningDownload>>,
    }

    #[cfg(feature = "yt-dlp")]
    impl DownloadLauncher for MockDownloadLauncher {
        fn start(&mut self, request: &DownloadRequest) -> Result<Box<dyn RunningDownload>, String> {
            self.requests
                .lock()
                .expect("download requests")
                .push(request.clone());
            self.process
                .take()
                .ok_or_else(|| "mock launcher has no second process".to_owned())
        }
    }

    impl DiagnosticActionHandler for MockDiagnosticActions {
        fn gh_available(&self) -> bool {
            self.gh_available
        }

        fn copy_report(&self, report: &str) -> Result<String, String> {
            self.calls
                .lock()
                .expect("diagnostic calls")
                .push(DiagnosticCall::Copy(report.to_owned()));
            Ok("mock clipboard".to_owned())
        }

        fn fill_github_issue(&self, title: &str, report: &str) -> Result<(), String> {
            self.calls
                .lock()
                .expect("diagnostic calls")
                .push(DiagnosticCall::Fill {
                    title: title.to_owned(),
                    report: report.to_owned(),
                });
            Ok(())
        }

        fn copy_and_open_github_issue(&self, title: &str, report: &str) -> Result<String, String> {
            self.calls
                .lock()
                .expect("diagnostic calls")
                .push(DiagnosticCall::CopyAndOpen {
                    title: title.to_owned(),
                    report: report.to_owned(),
                });
            Ok("mock clipboard".to_owned())
        }
    }

    impl PlaybackBackend for MockPlaybackBackend {
        fn play(&mut self, input: &PlaybackInput) -> PlaybackResult<()> {
            self.state
                .lock()
                .expect("mock state")
                .played
                .push(input.clone());
            Ok(())
        }

        fn command(&mut self, command: PlayerCommand) -> PlaybackResult<()> {
            self.state
                .lock()
                .expect("mock state")
                .commands
                .push(command);
            Ok(())
        }

        fn status(&mut self) -> PlaybackResult<crate::playback::PlaybackStatus> {
            Ok(self
                .statuses
                .lock()
                .expect("mock statuses")
                .pop_front()
                .unwrap_or_default())
        }

        fn poll_event(&mut self) -> PlaybackResult<Option<PlaybackEvent>> {
            Ok(self.events.lock().expect("mock events").pop_front())
        }

        fn shutdown(&mut self) -> PlaybackResult<()> {
            Ok(())
        }
    }

    fn controller_with_mock_statuses(
        statuses: impl IntoIterator<Item = crate::playback::PlaybackStatus>,
    ) -> (AppController, Arc<Mutex<MockPlaybackState>>) {
        let (controller, state, _, _) =
            controller_with_mock_lifecycle(statuses, Vec::<PlaybackEvent>::new());
        (controller, state)
    }

    /// Mock player factory together with handles that let tests advance its
    /// observable status and event queues.
    type MockPlaybackHarness = (
        PlaybackFactory,
        Arc<Mutex<MockPlaybackState>>,
        Arc<Mutex<VecDeque<crate::playback::PlaybackStatus>>>,
        Arc<Mutex<VecDeque<PlaybackEvent>>>,
    );

    /// Builds a mock player independently from controller persistence.
    fn mock_playback_factory(
        statuses: impl IntoIterator<Item = crate::playback::PlaybackStatus>,
        events: impl IntoIterator<Item = PlaybackEvent>,
    ) -> MockPlaybackHarness {
        let state = Arc::new(Mutex::new(MockPlaybackState::default()));
        let status_queue = Arc::new(Mutex::new(statuses.into_iter().collect::<VecDeque<_>>()));
        let event_queue = Arc::new(Mutex::new(events.into_iter().collect::<VecDeque<_>>()));
        let factory_state = Arc::clone(&state);
        let factory_statuses = Arc::clone(&status_queue);
        let factory_events = Arc::clone(&event_queue);
        let factory: PlaybackFactory = Box::new(move || {
            Ok(Box::new(MockPlaybackBackend {
                state: Arc::clone(&factory_state),
                statuses: Arc::clone(&factory_statuses),
                events: Arc::clone(&factory_events),
            }))
        });
        (factory, state, status_queue, event_queue)
    }

    fn controller_with_mock_lifecycle(
        statuses: impl IntoIterator<Item = crate::playback::PlaybackStatus>,
        events: impl IntoIterator<Item = PlaybackEvent>,
    ) -> (
        AppController,
        Arc<Mutex<MockPlaybackState>>,
        Arc<Mutex<VecDeque<crate::playback::PlaybackStatus>>>,
        Arc<Mutex<VecDeque<PlaybackEvent>>>,
    ) {
        let (factory, state, status_queue, event_queue) = mock_playback_factory(statuses, events);
        let config = Config::for_dir("/tmp/youta-queue-controller-test");
        let store = StateStore::open_in_memory().expect("in-memory state");
        (
            AppController::new(config, store, None, Some(factory)),
            state,
            status_queue,
            event_queue,
        )
    }

    #[test]
    fn chapter_timestamp_toggle_changes_only_labels_and_survives_restart() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let store = StateStore::open(&config).expect("disk state");
        let mut controller = AppController::new(config, store, None, None);
        controller.view.playback_chapters = vec![Chapter {
            title: "Introduction".to_owned(),
            start_seconds: 10,
            end_seconds: None,
        }];
        let chapters = controller.view.playback_chapters.clone();

        controller.dispatch(UiAction::ToggleChapterTimestamps);
        assert!(!controller.view.show_chapter_timestamps);
        assert_eq!(controller.view.playback_chapters, chapters);
        assert_eq!(controller.view.status_line, "Chapter timestamps hidden");
        controller.save_session();
        let config = controller.config.clone();
        drop(controller);

        let store = StateStore::open(&config).expect("reopen disk state");
        let mut restored = AppController::new(config, store, None, None);
        assert!(!restored.view.show_chapter_timestamps);
        restored.dispatch(UiAction::ToggleChapterTimestamps);
        assert!(restored.view.show_chapter_timestamps);
        assert_eq!(restored.view.status_line, "Chapter timestamps shown");
    }

    #[test]
    fn timecode_seek_and_back_use_absolute_backend_commands_and_bounded_chapters() {
        let (mut controller, state) = controller_with_mock_statuses([]);
        let mut video = subscription_video_summary();
        video.duration_seconds = Some(180);
        video.description = "00:00 Intro\n00:01:35 Main section\n00:02:30 End".to_owned();
        let media_id = MediaId::new(SourceKind::YouTube, &video.video_id);
        controller.youtube_results = vec![SearchItem::Video(video.clone())];
        controller.view.details = Some(preliminary_detail(
            &SearchItem::Video(video),
            &controller.subscription_tree,
        ));
        controller.refresh_youtube_rows();

        controller.dispatch(UiAction::ActivateSelection);
        assert_eq!(
            controller
                .view
                .playback_chapters
                .iter()
                .map(|chapter| chapter.start_seconds)
                .collect::<Vec<_>>(),
            [0, 95, 150]
        );
        controller.view.playback.position = Duration::from_secs(40);
        controller.dispatch(UiAction::ActivateTimecode {
            media_id: media_id.clone(),
            seconds: 95,
        });
        controller.dispatch(UiAction::GoBack);

        let commands = &state.lock().expect("mock state").commands;
        assert_eq!(
            &commands[commands.len().saturating_sub(2)..],
            [
                PlayerCommand::SeekAbsolute(Duration::from_secs(95)),
                PlayerCommand::SeekAbsolute(Duration::from_secs(40)),
            ]
        );
        assert!(controller.seek_back.is_empty());
    }

    #[test]
    fn inactive_description_timecode_starts_the_selected_media_at_that_position() {
        let (mut controller, state) = controller_with_mock_statuses([]);
        let mut video = subscription_video_summary();
        video.duration_seconds = Some(180);
        video.description = "00:00 Intro\n00:01:35 Main section".to_owned();
        let media_id = MediaId::new(SourceKind::YouTube, &video.video_id);
        controller.youtube_results = vec![SearchItem::Video(video.clone())];
        controller.view.details = Some(preliminary_detail(
            &SearchItem::Video(video),
            &controller.subscription_tree,
        ));
        controller.refresh_youtube_rows();

        controller.dispatch(UiAction::ActivateTimecode {
            media_id,
            seconds: 95,
        });

        let state = state.lock().expect("mock state");
        assert_eq!(state.played.len(), 1);
        assert_eq!(state.played[0].start_at, Duration::from_secs(95));
        assert!(controller.selected_start_override.is_none());
    }

    #[test]
    fn advertisement_chapter_is_skipped_once_until_playback_leaves_it() {
        let (mut controller, state) = controller_with_mock_statuses([]);
        controller.play_queue_item(fixture_direct_item("advertisement-skip"), false);
        controller.view.playback_chapters = vec![
            Chapter {
                title: "Introduction".to_owned(),
                start_seconds: 0,
                end_seconds: Some(30),
            },
            Chapter {
                title: "Реклама".to_owned(),
                start_seconds: 30,
                end_seconds: Some(45),
            },
            Chapter {
                title: "Main section".to_owned(),
                start_seconds: 45,
                end_seconds: None,
            },
        ];
        state.lock().expect("mock state").commands.clear();
        controller.view.playback.position = Duration::from_secs(35);

        controller.skip_current_advertisement_chapter();
        controller.skip_current_advertisement_chapter();

        assert_eq!(
            state.lock().expect("mock state").commands,
            [PlayerCommand::SeekAbsolute(Duration::from_secs(45))],
            "a stale backend position inside the same advertisement must not issue duplicate seeks"
        );
        controller.view.playback.position = Duration::from_secs(50);
        controller.skip_current_advertisement_chapter();
        controller.view.playback.position = Duration::from_secs(35);
        controller.skip_current_advertisement_chapter();
        assert_eq!(state.lock().expect("mock state").commands.len(), 2);
    }

    /// Avoids external diagnostic probes in controller error-path tests.
    fn use_mock_diagnostics(controller: &mut AppController) {
        controller.diagnostic_helpers_cache = Some(Vec::new());
        controller.report_actions = Box::new(MockDiagnosticActions {
            calls: Arc::new(Mutex::new(Vec::new())),
            gh_available: false,
        });
    }

    #[test]
    fn submitted_youtube_search_animates_until_its_terminal_response() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        let (request_sender, requests) = unbounded();
        controller.provider_requests = Some(request_sender);
        controller.youtube_provider_available = true;
        controller.view.search_query = "ambient".to_owned();

        controller.submit_youtube_search(1);

        let ProviderRequest::Search {
            generation,
            request,
        } = requests.try_recv().expect("submitted search request")
        else {
            panic!("unexpected provider request");
        };
        assert_eq!(request.sort, ProviderSearchSort::Relevance);
        assert!(
            request.filters.features.is_empty(),
            "the default YouTube search must not filter by licence"
        );
        assert!(!controller.view.youtube_creative_commons_only);
        assert_eq!(
            controller.view.search_activity,
            Some(SearchActivity::YouTube)
        );
        assert_eq!(controller.view.search_animation_frame, 0);
        controller.advance_search_animation();
        assert_eq!(controller.view.search_animation_frame, 1);

        controller.handle_provider_response(ProviderResponse::Search {
            generation,
            request,
            result: Ok(SearchPage {
                page: 1,
                items: Vec::new(),
                next_page: None,
            }),
        });

        assert!(controller.view.search_activity.is_none());
        assert_eq!(controller.view.search_animation_frame, 0);
        assert_eq!(controller.view.status_line, "0 YouTube results loaded");
        let saved = controller
            .store
            .youtube_search()
            .expect("saved search read")
            .expect("successful search should be saved");
        assert_eq!(saved.request.query, "ambient");
        assert_eq!(saved.request.target, SearchTarget::Videos);
        assert_eq!(saved.request.page, 1);
        assert!(saved.results.is_empty());
        assert_eq!(saved.next_page, None);

        use_mock_diagnostics(&mut controller);
        controller.supersede_search_generation();
        controller.begin_search_activity(SearchActivity::YouTube);
        let error_generation = controller.search_generation;
        controller.handle_provider_response(ProviderResponse::Search {
            generation: error_generation,
            request: SearchRequest::new("ambient", SearchTarget::Videos),
            result: Err("mock failure".to_owned()),
        });
        assert!(controller.view.search_activity.is_none());
        assert_eq!(controller.view.search_animation_frame, 0);
        assert_eq!(
            controller
                .view
                .error_popup
                .as_ref()
                .map(|popup| popup.title.as_str()),
            Some("YouTube search failed")
        );
    }

    #[test]
    fn accumulated_youtube_search_stops_at_the_restart_safe_result_limit() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        let (request_sender, requests) = unbounded();
        controller.provider_requests = Some(request_sender);
        controller.youtube_provider_available = true;
        controller.view.search_query = "bounded".to_owned();
        controller.submit_youtube_search(1);
        let ProviderRequest::Search {
            generation,
            request,
        } = requests.try_recv().expect("search request")
        else {
            panic!("unexpected provider request");
        };
        let items = (0..MAX_SAVED_YOUTUBE_SEARCH_RESULTS + 10)
            .map(|index| {
                SearchItem::Video(VideoSummary {
                    video_id: format!("fixture-{index}"),
                    title: format!("Fixture {index}"),
                    ..subscription_video_summary()
                })
            })
            .collect();

        controller.handle_provider_response(ProviderResponse::Search {
            generation,
            request,
            result: Ok(SearchPage {
                page: 1,
                items,
                next_page: Some(2),
            }),
        });

        assert_eq!(
            controller.youtube_results.len(),
            MAX_SAVED_YOUTUBE_SEARCH_RESULTS
        );
        assert_eq!(controller.next_youtube_page, None);
        assert!(
            controller
                .view
                .status_line
                .contains("restart-safe limit reached")
        );
        let saved = controller
            .store
            .youtube_search()
            .expect("saved search")
            .expect("snapshot");
        assert_eq!(saved.results.len(), MAX_SAVED_YOUTUBE_SEARCH_RESULTS);
        assert_eq!(saved.next_page, None);
    }

    #[test]
    fn newest_toggle_restarts_page_one_and_keeps_order_on_later_pages() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        let (request_sender, requests) = unbounded();
        controller.provider_requests = Some(request_sender);
        controller.youtube_provider_available = true;
        controller.view.search_query = "ambient".to_owned();

        controller.dispatch(UiAction::ToggleYouTubeSearchSort);
        assert_eq!(
            controller.view.youtube_search_sort,
            YouTubeSearchSort::Newest
        );
        let ProviderRequest::Search {
            generation,
            request,
        } = requests.try_recv().expect("newest page-one request")
        else {
            panic!("unexpected provider request");
        };
        assert_eq!(request.page, 1);
        assert_eq!(request.sort, ProviderSearchSort::UploadDate);

        controller.handle_provider_response(ProviderResponse::Search {
            generation,
            request,
            result: Ok(SearchPage {
                page: 1,
                items: Vec::new(),
                next_page: Some(2),
            }),
        });
        controller.submit_youtube_search(2);
        let ProviderRequest::Search { request, .. } =
            requests.try_recv().expect("newest page-two request")
        else {
            panic!("unexpected provider request");
        };
        assert_eq!(request.page, 2);
        assert_eq!(request.sort, ProviderSearchSort::UploadDate);

        // Switching while page two is in flight must invalidate that response
        // and start a fresh relevance page rather than mixing result orders.
        controller.dispatch(UiAction::ToggleYouTubeSearchSort);
        let ProviderRequest::Search { request, .. } =
            requests.try_recv().expect("relevance page-one request")
        else {
            panic!("unexpected provider request");
        };
        assert_eq!(
            controller.view.youtube_search_sort,
            YouTubeSearchSort::Relevance
        );
        assert_eq!(request.page, 1);
        assert_eq!(request.sort, ProviderSearchSort::Relevance);
    }

    #[test]
    fn creative_commons_toggle_restarts_and_survives_pagination_and_sort_changes() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        let (request_sender, requests) = unbounded();
        controller.provider_requests = Some(request_sender);
        controller.youtube_provider_available = true;
        controller.view.search_query = "open music".to_owned();

        controller.dispatch(UiAction::ToggleYouTubeCreativeCommons);
        assert!(controller.view.youtube_creative_commons_only);
        let ProviderRequest::Search {
            generation,
            request,
        } = requests.try_recv().expect("CC page-one request")
        else {
            panic!("unexpected provider request");
        };
        assert_eq!(request.page, 1);
        assert_eq!(request.sort, ProviderSearchSort::Relevance);
        assert_eq!(request.filters.features, [SearchFeature::CreativeCommons]);

        controller.handle_provider_response(ProviderResponse::Search {
            generation,
            request,
            result: Ok(SearchPage {
                page: 1,
                items: Vec::new(),
                next_page: Some(2),
            }),
        });
        controller.submit_youtube_search(2);
        let ProviderRequest::Search { request, .. } =
            requests.try_recv().expect("CC page-two request")
        else {
            panic!("unexpected provider request");
        };
        assert_eq!(request.page, 2);
        assert_eq!(
            request.filters.features,
            [SearchFeature::CreativeCommons],
            "pagination must stay inside the filtered result set"
        );

        controller.dispatch(UiAction::ToggleYouTubeSearchSort);
        let ProviderRequest::Search { request, .. } = requests
            .try_recv()
            .expect("newest CC page-one request after sort change")
        else {
            panic!("unexpected provider request");
        };
        assert_eq!(request.page, 1);
        assert_eq!(request.sort, ProviderSearchSort::UploadDate);
        assert_eq!(
            request.filters.features,
            [SearchFeature::CreativeCommons],
            "changing sort must preserve the CC-only preference"
        );

        controller.dispatch(UiAction::ToggleYouTubeCreativeCommons);
        let ProviderRequest::Search { request, .. } = requests
            .try_recv()
            .expect("unfiltered page-one request after disabling CC")
        else {
            panic!("unexpected provider request");
        };
        assert!(!controller.view.youtube_creative_commons_only);
        assert_eq!(request.page, 1);
        assert_eq!(request.sort, ProviderSearchSort::UploadDate);
        assert!(request.filters.features.is_empty());
    }

    #[test]
    fn creative_commons_toggle_does_not_apply_a_video_filter_to_channel_search() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        let (request_sender, requests) = unbounded();
        controller.provider_requests = Some(request_sender);
        controller.youtube_provider_available = true;
        controller.view.search_query = "channel".to_owned();
        controller.view.search_kind = SearchKind::Channels;

        controller.dispatch(UiAction::ToggleYouTubeCreativeCommons);

        assert!(!controller.view.youtube_creative_commons_only);
        assert!(requests.try_recv().is_err());
        assert_eq!(
            controller.view.status_line,
            "Creative Commons filtering applies only to YouTube video search"
        );
    }

    #[test]
    fn superseded_generation_cannot_leave_or_clear_the_wrong_animation() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.search_generation = 10;
        controller.begin_search_activity(SearchActivity::YouTube);
        let stale_generation = controller.search_generation;

        controller.supersede_search_generation();
        assert!(controller.view.search_activity.is_none());
        controller.begin_search_activity(SearchActivity::TrackerArchives);
        controller.handle_provider_response(ProviderResponse::Search {
            generation: stale_generation,
            request: SearchRequest::new("stale", SearchTarget::Videos),
            result: Ok(SearchPage {
                page: 1,
                items: Vec::new(),
                next_page: None,
            }),
        });

        assert_eq!(
            controller.view.search_activity,
            Some(SearchActivity::TrackerArchives)
        );
    }

    #[test]
    fn tracker_search_stops_only_on_matching_completion_even_after_navigation() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.view.screen = Screen::TrackerMusic;
        let (request_sender, requests) = unbounded();
        controller.provider_requests = Some(request_sender);
        controller.submit_tracker_search("fixture".to_owned());
        let ProviderRequest::TrackerSearch { generation, .. } =
            requests.try_recv().expect("submitted tracker request")
        else {
            panic!("unexpected provider request");
        };

        controller.handle_provider_response(ProviderResponse::TrackerSource {
            generation,
            source: "Mock archive".to_owned(),
            result: Ok(Vec::new()),
        });
        assert_eq!(
            controller.view.search_activity,
            Some(SearchActivity::TrackerArchives)
        );

        controller.handle_provider_response(ProviderResponse::TrackerComplete {
            generation: generation.wrapping_add(1),
        });
        assert_eq!(
            controller.view.search_activity,
            Some(SearchActivity::TrackerArchives)
        );

        controller.view.screen = Screen::Subscriptions;
        controller.handle_provider_response(ProviderResponse::TrackerComplete { generation });

        assert!(controller.view.search_activity.is_none());
        assert_eq!(controller.view.search_animation_frame, 0);
    }

    #[test]
    fn failed_send_and_provider_disconnect_restore_idle_search_state() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        use_mock_diagnostics(&mut controller);
        let (request_sender, request_receiver) = unbounded();
        drop(request_receiver);
        controller.provider_requests = Some(request_sender);

        controller.submit_tracker_search("fixture".to_owned());

        assert!(controller.view.search_activity.is_none());
        assert_eq!(controller.view.search_animation_frame, 0);

        controller.begin_search_activity(SearchActivity::YouTube);
        let (response_sender, provider_responses) = unbounded();
        drop(response_sender);
        controller.provider_responses = provider_responses;
        controller.provider_disconnect_reported = true;
        controller.tick();

        assert!(controller.view.search_activity.is_none());
        assert_eq!(controller.view.search_animation_frame, 0);
    }

    #[test]
    fn missing_youtube_provider_opens_setup_popup_with_exact_storage_path() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let expected_path = config.config_file().display().to_string();
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.view.search_query = "ambient focus".to_owned();

        controller.dispatch(UiAction::SubmitSearch);

        let popup = controller
            .view
            .youtube_setup_popup
            .as_ref()
            .expect("provider setup popup");
        assert_eq!(popup.config_path, expected_path);
        assert_eq!(popup.selected_field, YouTubeSetupField::ApiKey);
        assert!(popup.api_key.is_empty());
        assert!(controller.view.error_popup.is_none());
        assert!(
            !controller
                .view
                .status_line
                .contains("providers.invidious_base_url")
        );
    }

    #[test]
    fn setup_input_is_bounded_and_cancel_keeps_the_pending_query() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.view.search_query = "fixture query".to_owned();
        controller.dispatch(UiAction::SubmitSearch);

        controller.dispatch(UiAction::AppendYouTubeSetupCharacter('\n'));
        for _ in 0..300 {
            controller.dispatch(UiAction::AppendYouTubeSetupCharacter('A'));
        }
        assert_eq!(
            controller
                .view
                .youtube_setup_popup
                .as_ref()
                .expect("setup popup")
                .api_key
                .len(),
            256
        );
        controller.dispatch(UiAction::DismissYouTubeSetup);
        assert!(controller.view.youtube_setup_popup.is_none());
        assert_eq!(controller.view.search_query, "fixture query");
    }

    #[cfg(feature = "youtube-official")]
    #[test]
    fn invalid_official_key_stays_in_setup_and_is_not_persisted() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let config_file = config.config_file();
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.view.search_query = "fixture query".to_owned();
        controller.dispatch(UiAction::SubmitSearch);
        controller
            .view
            .youtube_setup_popup
            .as_mut()
            .expect("setup popup")
            .api_key = "short".to_owned();

        controller.dispatch(UiAction::SubmitYouTubeSetup);

        let popup = controller
            .view
            .youtube_setup_popup
            .as_ref()
            .expect("invalid setup remains open");
        assert!(
            popup
                .validation_error
                .as_deref()
                .is_some_and(|error| error.contains("API key"))
        );
        assert!(!config_file.exists());
        assert!(!controller.youtube_provider_available);
    }

    #[cfg(feature = "youtube-official")]
    #[test]
    fn saved_official_setup_replaces_provider_and_retries_search() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let config_file = config.config_file();
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.youtube_provider_builder = Box::new(MockYouTubeProviderBuilder);
        controller.view.search_query = "API fixture".to_owned();
        controller.dispatch(UiAction::SubmitSearch);
        controller
            .view
            .youtube_setup_popup
            .as_mut()
            .expect("setup popup")
            .api_key = "AIzaSyFixture_key_123456789012345678".to_owned();

        controller.dispatch(UiAction::SubmitYouTubeSetup);

        assert!(controller.view.youtube_setup_popup.is_none());
        assert!(controller.youtube_provider_available);
        assert_eq!(
            controller.view.search_activity,
            Some(SearchActivity::YouTube)
        );
        let saved = std::fs::read_to_string(&config_file).expect("saved config");
        assert!(saved.contains("youtube_backend = \"official\""));
        assert!(saved.contains("AIzaSyFixture_key_123456789012345678"));

        for _ in 0..100 {
            controller.tick();
            if controller.view.search_activity.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            controller.view.search_activity.is_none(),
            "retry should complete"
        );
        assert_eq!(controller.view.status_line, "0 YouTube results loaded");
    }

    #[cfg(feature = "invidious")]
    #[test]
    fn saved_invidious_setup_replaces_provider_and_retries_search() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let config_file = config.config_file();
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.youtube_provider_builder = Box::new(MockYouTubeProviderBuilder);
        controller.view.search_query = "ambient focus".to_owned();
        controller.dispatch(UiAction::SubmitSearch);
        {
            let popup = controller
                .view
                .youtube_setup_popup
                .as_mut()
                .expect("setup popup");
            popup.selected_field = YouTubeSetupField::InvidiousUrl;
            popup.invidious_url = "https://invidious.example.test".to_owned();
        }

        controller.dispatch(UiAction::SubmitYouTubeSetup);

        assert!(controller.view.youtube_setup_popup.is_none());
        assert!(controller.youtube_provider_available);
        assert_eq!(
            controller.view.search_activity,
            Some(SearchActivity::YouTube)
        );
        let saved = std::fs::read_to_string(&config_file).expect("saved config");
        assert!(saved.contains("youtube_backend = \"invidious\""));
        assert!(saved.contains("https://invidious.example.test/"));

        for _ in 0..100 {
            controller.tick();
            if controller.view.search_activity.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            controller.view.search_activity.is_none(),
            "retry should complete"
        );
        assert_eq!(controller.view.status_line, "0 YouTube results loaded");
    }

    fn fixture_direct_item(name: &str) -> QueueItem {
        let direct = DirectSourceInput {
            url: url::Url::parse(&format!("https://media.example/{name}.opus"))
                .expect("fixture URL"),
            source: SourceKind::RemoteFiles,
        };
        let mut item = queue_item_from_direct(&direct);
        item.media.title = name.to_owned();
        item
    }

    fn fixture_youtube_item(name: &str) -> QueueItem {
        let mut video = subscription_video_summary();
        video.title = name.to_owned();
        queue_item_from_video(&video, None)
    }

    #[cfg(feature = "yt-dlp")]
    fn fixture_download_video() -> VideoSummary {
        VideoSummary {
            video_id: "dQw4w9WgXcQ".to_owned(),
            title: "Download fixture".to_owned(),
            channel_name: "Fixture channel".to_owned(),
            channel_id: "UCfixture".to_owned(),
            description: String::new(),
            duration_seconds: Some(120),
            view_count: None,
            published_at: None,
            published_text: None,
            live: false,
            orientation: VideoOrientation::Unknown,
            thumbnails: Vec::new(),
            webpage_url: Some(
                url::Url::parse("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
                    .expect("fixture page URL"),
            ),
            stream_url: None,
        }
    }

    #[cfg(feature = "yt-dlp")]
    fn controller_with_mock_download(
        config: Config,
        process: MockRunningDownload,
    ) -> (
        AppController,
        Arc<Mutex<Vec<DownloadRequest>>>,
        Arc<AtomicBool>,
    ) {
        let cancelled = Arc::clone(&process.cancelled);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.download_launcher = Box::new(MockDownloadLauncher {
            requests: Arc::clone(&requests),
            process: Some(Box::new(process)),
        });
        controller.youtube_results = vec![SearchItem::Video(fixture_download_video())];
        controller.refresh_youtube_rows();
        (controller, requests, cancelled)
    }

    #[test]
    fn operational_error_opens_a_redacted_complete_diagnostic_popup() {
        let (mut controller, _) =
            controller_with_mock_statuses(Vec::<crate::playback::PlaybackStatus>::new());
        controller.show_error_message(
            "Provider request failed",
            "Authorization: Bearer diagnostic-secret",
        );

        let popup = controller
            .view
            .error_popup
            .as_ref()
            .expect("diagnostic popup");
        assert_eq!(popup.title, "Provider request failed");
        assert_eq!(popup.scroll_offset, 0);
        assert!(popup.report.contains("Youta diagnostic report"));
        assert!(popup.report.contains("Youta version:"));
        assert!(popup.report.contains("Operating system:"));
        assert!(popup.report.contains("Cargo.lock packages"));
        assert!(popup.report.contains("Forced backtrace:"));
        assert!(!popup.report.contains("diagnostic-secret"));
    }

    #[cfg(feature = "wikidata")]
    #[test]
    fn lazy_wikidata_timeout_does_not_interrupt_a_seek_with_a_popup() {
        let (mut controller, playback) =
            controller_with_mock_statuses(Vec::<crate::playback::PlaybackStatus>::new());
        controller.player = Some(
            controller
                .playback_factory
                .as_mut()
                .expect("mock playback factory")()
            .expect("mock playback backend"),
        );
        controller.view.details = Some(DetailView {
            title: "Fixture".to_owned(),
            wikidata: "loading P1651 lazily…".to_owned(),
            ..DetailView::default()
        });
        controller.wikidata_generation = 7;

        controller.dispatch(UiAction::SeekPercent(35.0));
        controller.handle_provider_response(ProviderResponse::Wikidata {
            generation: 7,
            property_id: "P1651".to_owned(),
            external_id: "dQw4w9WgXcQ".to_owned(),
            result: Err("provider transport failed: timeout: global".to_owned()),
        });

        assert_eq!(
            playback.lock().expect("mock playback state").commands,
            [PlayerCommand::SeekPercent(35.0)]
        );
        assert!(
            controller.view.error_popup.is_none(),
            "optional lazy metadata must not replace media controls"
        );
        assert_eq!(
            controller
                .view
                .details
                .as_ref()
                .expect("details remain visible")
                .wikidata,
            ""
        );
        assert!(controller.view.status_line.contains("timed out"));
        assert!(
            controller
                .view
                .status_line
                .contains("playback remains available")
        );
    }

    #[test]
    fn diagnostic_buttons_remain_distinct_controller_actions() {
        let (mut controller, _) =
            controller_with_mock_statuses(Vec::<crate::playback::PlaybackStatus>::new());
        let calls = Arc::new(Mutex::new(Vec::new()));
        controller.report_actions = Box::new(MockDiagnosticActions {
            calls: Arc::clone(&calls),
            gh_available: true,
        });
        controller.show_diagnostic_report("Playback failed", "complete report");
        assert!(
            controller
                .view
                .error_popup
                .as_ref()
                .is_some_and(|popup| popup.gh_available)
        );

        controller.dispatch(UiAction::CopyErrorReport);
        controller.dispatch(UiAction::CopyAndOpenGitHubIssue);
        controller.dispatch(UiAction::FillGitHubIssue);

        assert_eq!(
            *calls.lock().expect("diagnostic calls"),
            [
                DiagnosticCall::Copy("complete report".to_owned()),
                DiagnosticCall::CopyAndOpen {
                    title: "Youta error: Playback failed".to_owned(),
                    report: "complete report".to_owned(),
                },
                DiagnosticCall::Fill {
                    title: "Youta error: Playback failed".to_owned(),
                    report: "complete report".to_owned(),
                },
            ]
        );
        assert!(
            controller
                .view
                .error_popup
                .as_ref()
                .and_then(|popup| popup.action_status.as_deref())
                .is_some_and(|status| status.contains("pre-filled issue"))
        );
    }

    #[test]
    fn diagnostic_scrolling_is_saturating_and_dismissible() {
        let (mut controller, _) =
            controller_with_mock_statuses(Vec::<crate::playback::PlaybackStatus>::new());
        controller.show_diagnostic_report("Error", "line\n".repeat(100));
        controller.dispatch(UiAction::ScrollErrorPopup(ErrorPopupScroll::Lines(-10)));
        assert_eq!(
            controller
                .view
                .error_popup
                .as_ref()
                .map(|popup| popup.scroll_offset),
            Some(0)
        );
        controller.dispatch(UiAction::ScrollErrorPopup(ErrorPopupScroll::Pages(2)));
        assert_eq!(
            controller
                .view
                .error_popup
                .as_ref()
                .map(|popup| popup.scroll_offset),
            Some(40)
        );
        controller.dispatch(UiAction::ScrollErrorPopup(ErrorPopupScroll::End));
        assert_eq!(
            controller
                .view
                .error_popup
                .as_ref()
                .map(|popup| popup.scroll_offset),
            Some(usize::MAX)
        );
        controller.dispatch(UiAction::DismissErrorPopup);
        assert!(controller.view.error_popup.is_none());
    }

    #[test]
    fn details_scrolling_focuses_and_saturates_without_stealing_row_navigation() {
        let (mut controller, _) =
            controller_with_mock_statuses(Vec::<crate::playback::PlaybackStatus>::new());
        controller.view.details = Some(DetailView {
            title: "Fixture".to_owned(),
            description: "line\n".repeat(100),
            ..DetailView::default()
        });

        controller.dispatch(UiAction::ScrollDetails(DetailsScroll::Lines(-10)));
        assert!(controller.view.details_focused);
        assert_eq!(controller.view.details_scroll, 0);

        controller.dispatch(UiAction::ScrollDetails(DetailsScroll::Pages(2)));
        assert_eq!(controller.view.details_scroll, 40);
        controller.dispatch(UiAction::ScrollDetails(DetailsScroll::End));
        assert_eq!(controller.view.details_scroll, usize::MAX);
        controller.view.details_text_selection = Some(DetailsTextSelection::default());
        controller.dispatch(UiAction::SetDetailsScroll(7));
        assert_eq!(controller.view.details_scroll, 7);
        assert!(controller.view.details_text_selection.is_none());
        controller.dispatch(UiAction::ScrollDetails(DetailsScroll::Home));
        assert_eq!(controller.view.details_scroll, 0);

        controller.view.rows = vec![RowView::default(), RowView::default()];
        controller.dispatch(UiAction::MoveSelection(1));
        assert_eq!(controller.view.selected, 1);
        assert!(!controller.view.details_focused);
        assert_eq!(controller.view.details_scroll, 0);
    }

    #[test]
    fn details_text_selection_mode_preserves_navigation_and_playback_state() {
        let (mut controller, _) =
            controller_with_mock_statuses(Vec::<crate::playback::PlaybackStatus>::new());
        controller.view.details = Some(DetailView {
            title: "Selectable fixture".to_owned(),
            links: vec![DetailLinkView {
                label: "Fixture link".to_owned(),
                url: "https://example.com".to_owned(),
                ..DetailLinkView::default()
            }],
            ..DetailView::default()
        });
        controller.view.details_scroll = 17;
        controller.view.selected_detail_link = Some(0);
        let playback = controller.view.playback.clone();

        controller.dispatch(UiAction::ToggleTextSelectionMode);

        assert!(controller.view.text_selection_mode);
        assert!(controller.view.details_focused);
        assert_eq!(controller.view.details_scroll, 17);
        assert_eq!(controller.view.selected_detail_link, Some(0));
        assert_eq!(controller.view.playback, playback);
        assert!(controller.view.status_line.contains("drag to copy"));

        controller.dispatch(UiAction::ToggleTextSelectionMode);

        assert!(!controller.view.text_selection_mode);
        assert!(controller.view.details_text_selection.is_none());
        assert_eq!(controller.view.details_scroll, 17);
        assert_eq!(controller.view.selected_detail_link, Some(0));
        assert_eq!(controller.view.playback, playback);
        assert!(controller.view.status_line.contains("selection ended"));

        controller.view.details = None;
        controller.dispatch(UiAction::ToggleTextSelectionMode);
        assert!(!controller.view.text_selection_mode);
        assert!(controller.view.status_line.contains("No Details text"));
    }

    #[test]
    fn finished_details_selection_uses_injectable_bounded_clipboard_action() {
        let (mut controller, _) =
            controller_with_mock_statuses(Vec::<crate::playback::PlaybackStatus>::new());
        controller.view.details = Some(DetailView {
            title: "Selectable fixture".to_owned(),
            ..DetailView::default()
        });
        controller.view.text_selection_mode = true;
        let calls = Arc::new(Mutex::new(Vec::new()));
        controller.report_actions = Box::new(MockDiagnosticActions {
            calls: Arc::clone(&calls),
            gh_available: false,
        });
        let anchor = crate::tui::DetailsTextPosition { row: 0, column: 0 };
        let focus = crate::tui::DetailsTextPosition { row: 1, column: 3 };

        controller.dispatch(UiAction::BeginDetailsTextSelection(anchor));
        controller.dispatch(UiAction::UpdateDetailsTextSelection(focus));
        controller.dispatch(UiAction::FinishDetailsTextSelection {
            focus,
            text: "Title\nmeta".to_owned(),
        });

        assert_eq!(
            calls.lock().expect("clipboard calls").as_slice(),
            [DiagnosticCall::Copy("Title\nmeta".to_owned())]
        );
        assert_eq!(
            controller.view.details_text_selection,
            Some(DetailsTextSelection {
                anchor,
                focus,
                dragging: false,
            })
        );
        assert!(controller.view.status_line.contains("mock clipboard"));

        controller.dispatch(UiAction::BeginDetailsTextSelection(anchor));
        controller.dispatch(UiAction::FinishDetailsTextSelection {
            focus,
            text: "é".repeat(MAX_DETAILS_SELECTION_BYTES),
        });
        let calls = calls.lock().expect("bounded clipboard calls");
        let DiagnosticCall::Copy(bounded) = calls.last().expect("second clipboard call") else {
            panic!("unexpected diagnostic action")
        };
        assert!(bounded.len() <= MAX_DETAILS_SELECTION_BYTES);
        assert!(bounded.is_char_boundary(bounded.len()));
    }

    #[cfg(feature = "yt-dlp")]
    #[test]
    fn download_controller_supervises_progress_and_refreshes_completed_file() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let mut config = Config::for_dir(temporary.path().join("youta"));
        config.subscriptions.download_thumbnails = false;
        let download_dir = config.downloads_dir();
        std::fs::create_dir_all(&download_dir).expect("download directory");
        let completed = download_dir.join("fixture [dQw4w9WgXcQ].opus");
        std::fs::write(&completed, b"mock opus").expect("completed fixture");
        let output = format!(
            "youta-progress|2048|4096|NA|512|4\nyouta-file|{}\n",
            completed.display()
        );
        let process = MockRunningDownload {
            progress: Some(Cursor::new(output.into_bytes())),
            errors: Some(Cursor::new(b"[download] fixture diagnostic\n".to_vec())),
            exits: VecDeque::from([Ok(Some(DownloadExit {
                success: true,
                description: "exit status: 0".to_owned(),
            }))]),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let (mut controller, requests, _) = controller_with_mock_download(config.clone(), process);

        controller.dispatch(UiAction::Download);
        assert!(
            controller
                .view
                .download
                .as_ref()
                .is_some_and(|download| download.active)
        );
        let request = requests
            .lock()
            .expect("download requests")
            .first()
            .cloned()
            .expect("one request");
        assert_eq!(
            request.source_url.as_str(),
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        );
        assert_eq!(request.format, DownloadFormat::OpusWithoutTranscoding);
        assert!(!request.write_thumbnail);
        assert_eq!(
            request.destination,
            std::fs::canonicalize(config.downloads_dir()).expect("canonical downloads")
        );

        controller.dispatch(UiAction::ShowScreen(Screen::Downloaded));
        controller.tick();

        let download = controller
            .view
            .download
            .as_ref()
            .expect("completed download view");
        assert!(!download.active);
        assert_eq!(download.downloaded_bytes, 2048);
        assert_eq!(download.total_bytes, Some(4096));
        assert_eq!(download.bytes_per_second, Some(512));
        assert_eq!(
            download.completed_path.as_deref(),
            Some(completed.display().to_string().as_str())
        );
        assert!(
            controller
                .view
                .rows
                .iter()
                .any(|row| row.title == "fixture [dQw4w9WgXcQ].opus")
        );
    }

    #[cfg(feature = "yt-dlp")]
    #[test]
    fn second_download_is_refused_and_shutdown_cancels_the_running_child() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let cancelled = Arc::new(AtomicBool::new(false));
        let process = MockRunningDownload {
            progress: Some(Cursor::new(Vec::new())),
            errors: Some(Cursor::new(Vec::new())),
            exits: VecDeque::from([Ok(None), Ok(None)]),
            cancelled: Arc::clone(&cancelled),
        };
        let (mut controller, requests, _) = controller_with_mock_download(config, process);

        controller.dispatch(UiAction::Download);
        controller.dispatch(UiAction::Download);

        assert_eq!(requests.lock().expect("download requests").len(), 1);
        assert!(controller.view.status_line.contains("already running"));
        controller.shutdown();
        assert!(cancelled.load(Ordering::SeqCst));
        assert!(controller.active_download.is_none());
    }

    #[cfg(feature = "yt-dlp")]
    #[test]
    fn generic_ytdlp_selection_uses_its_canonical_remote_url() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let process = MockRunningDownload {
            progress: Some(Cursor::new(Vec::new())),
            errors: Some(Cursor::new(Vec::new())),
            exits: VecDeque::from([Ok(None)]),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let (mut controller, requests, _) = controller_with_mock_download(config, process);
        controller.youtube_results.clear();
        controller.direct_item = Some(DirectSourceInput {
            url: url::Url::parse("https://media.example/watch/fixture")
                .expect("generic fixture URL"),
            source: SourceKind::GenericYtDlp,
        });

        controller.dispatch(UiAction::Download);

        let requests = requests.lock().expect("download requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].source_url.as_str(),
            "https://media.example/watch/fixture"
        );
        drop(requests);
        controller.shutdown();
    }

    #[cfg(feature = "yt-dlp")]
    #[test]
    fn failed_download_opens_the_complete_diagnostic_popup() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let process = MockRunningDownload {
            progress: Some(Cursor::new(
                b"youta-progress|1024|4096|NA|256|12\n".to_vec(),
            )),
            errors: Some(Cursor::new(b"fixture extractor failure\n".to_vec())),
            exits: VecDeque::from([Ok(Some(DownloadExit {
                success: false,
                description: "exit status: 1".to_owned(),
            }))]),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let (mut controller, _, _) = controller_with_mock_download(config, process);

        controller.dispatch(UiAction::Download);
        controller.tick();

        let popup = controller
            .view
            .error_popup
            .as_ref()
            .expect("download diagnostic popup");
        assert_eq!(popup.title, "Download failed");
        assert!(popup.report.contains("exit status: 1"));
        assert!(popup.report.contains("fixture extractor failure"));
        assert!(controller.active_download.is_none());
        assert!(
            controller
                .view
                .download
                .as_ref()
                .is_some_and(|download| !download.active)
        );
    }

    #[cfg(feature = "yt-dlp")]
    #[test]
    fn download_paths_and_output_buffers_remain_confined_and_bounded() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let destination =
            prepare_download_destination(&config).expect("confined download destination");
        let outside = temporary.path().join("outside.opus");
        std::fs::write(&outside, b"outside").expect("outside fixture");
        assert!(
            validate_completed_download_path(&destination, &outside)
                .expect_err("outside path")
                .contains("outside")
        );

        let output = Arc::new(Mutex::new(DownloadOutputBuffer::default()));
        drain_download_reader(
            Box::new(Cursor::new(vec![b'x'; DOWNLOAD_DIAGNOSTIC_BYTES * 3])),
            &output,
            false,
        );
        let output = output.lock().expect("bounded output");
        assert!(output.diagnostic_bytes <= DOWNLOAD_DIAGNOSTIC_BYTES);
        assert!(output.diagnostics().contains("[line truncated]"));
    }

    #[cfg(feature = "yt-dlp")]
    #[test]
    fn download_rate_rounding_handles_fractional_and_invalid_values() {
        assert_eq!(rounded_download_rate(0.49), 0);
        assert_eq!(rounded_download_rate(0.5), 1);
        assert_eq!(rounded_download_rate(512.4), 512);
        assert_eq!(rounded_download_rate(-1.0), 0);
        assert_eq!(rounded_download_rate(f64::NAN), 0);
        assert_eq!(rounded_download_rate(f64::INFINITY), u64::MAX);
    }

    #[cfg(feature = "yt-dlp")]
    #[test]
    fn download_format_configuration_has_explicit_non_transcoding_default() {
        assert_eq!(
            configured_download_format("opus"),
            Ok(DownloadFormat::OpusWithoutTranscoding)
        );
        assert_eq!(
            configured_download_format("original"),
            Ok(DownloadFormat::OriginalBestAudio)
        );
        assert_eq!(
            configured_download_format("transcode-opus"),
            Ok(DownloadFormat::TranscodeToOpus)
        );
        assert!(configured_download_format("mp3").is_err());
    }

    #[test]
    fn queue_conversions_preserve_playable_locations_for_each_selection_kind() {
        let video = VideoSummary {
            video_id: "dQw4w9WgXcQ".to_owned(),
            title: "Video".to_owned(),
            channel_name: "Channel".to_owned(),
            channel_id: "UCfixture".to_owned(),
            description: "Description".to_owned(),
            duration_seconds: Some(42),
            view_count: Some(7),
            published_at: Some(1),
            published_text: None,
            live: false,
            orientation: VideoOrientation::Unknown,
            thumbnails: Vec::new(),
            webpage_url: None,
            stream_url: Some(
                url::Url::parse("https://cdn.example/expired-signed-audio.webm")
                    .expect("fixture stream URL"),
            ),
        };
        let queued_video = queue_item_from_video(&video, Some(12));
        assert_eq!(queued_video.media.id.source, SourceKind::YouTube);
        assert_eq!(queued_video.start_at_seconds, Some(12));
        assert_eq!(
            queued_video.playback_location,
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        );
        assert!(
            !queued_video
                .playback_location
                .contains("expired-signed-audio"),
            "short-lived provider stream URLs must never enter the playback queue"
        );

        let direct = DirectSourceInput {
            url: url::Url::parse("https://cdn.example/episode.opus").expect("direct URL"),
            source: SourceKind::RemoteFiles,
        };
        let queued_direct = queue_item_from_direct(&direct);
        assert_eq!(queued_direct.media.kind, MediaKind::Audio);
        assert_eq!(queued_direct.playback_location, direct.url.as_str());

        let resolved = ResolvedDirectMedia {
            source: SourceKind::ApplePodcasts,
            external_id: "episode-1".to_owned(),
            title: "Episode".to_owned(),
            row_subtitle: "podcast".to_owned(),
            description: "Description".to_owned(),
            license: "unknown".to_owned(),
            published: None,
            artwork_url: None,
            duration_seconds: Some(60),
            playback_url: Some(
                url::Url::parse("https://cdn.example/episode.m4a").expect("media URL"),
            ),
            webpage_url: Some(
                url::Url::parse("https://podcasts.apple.com/episode").expect("page URL"),
            ),
            status_line: "ready".to_owned(),
        };
        let queued_resolved = queue_item_from_resolved(&resolved).expect("resolved queue item");
        assert_eq!(queued_resolved.media.kind, MediaKind::PodcastEpisode);
        assert_eq!(
            queued_resolved.playback_location,
            "https://cdn.example/episode.m4a"
        );

        let local = LocalMediaItem {
            path: PathBuf::from("/tmp/youta-fixture.flac"),
            title: "Local".to_owned(),
            artist: Some("Artist One; Artist Two".to_owned()),
            album: Some("Album".to_owned()),
            genre: Some("Trip hop".to_owned()),
            comment: Some("Visit the artist website".to_owned()),
            metadata_url: Some("https://artist.example".to_owned()),
            acoustid_id: Some("39e08d1e-ce43-465f-a802-041995e9d150".to_owned()),
            duration_seconds: Some(10),
            size_bytes: 1,
            codec: "FLAC".to_owned(),
            bitrate_kbps: None,
            sample_rate_hz: None,
            channels: None,
            embedded_artwork: false,
        };
        let queued_local = queue_item_from_local(&local).expect("local queue item");
        assert_eq!(queued_local.media.id.source, SourceKind::Local);
        let local_details = local_media_description(&local);
        for expected in [
            "Artists: Artist One; Artist Two",
            "Album: Album",
            "Genre: Trip hop",
            "Comment: Visit the artist website",
            "URL: https://artist.example",
            "Identifiers:\nAcoustID ID: 39e08d1e-ce43-465f-a802-041995e9d150",
        ] {
            assert!(
                local_details.contains(expected),
                "Local Details omitted {expected:?}"
            );
        }
        assert_eq!(queued_local.playback_location, "/tmp/youta-fixture.flac");

        let tracker = TrackerItem {
            source: "The Mod Archive".to_owned(),
            title: "Module".to_owned(),
            subtitle: "XM".to_owned(),
            webpage_url: url::Url::parse("https://modarchive.org/module/1").expect("page URL"),
            download_url: Some(
                url::Url::parse("https://cdn.example/module.xm").expect("module URL"),
            ),
            expected_format: Some("xm".to_owned()),
            access: TrackerAccess::DirectModule,
            prepared_path: Some(PathBuf::from("/tmp/youta-module.xm")),
            insecure_transport: false,
        };
        let queued_tracker = queue_item_from_tracker(&tracker).expect("tracker queue item");
        assert_eq!(queued_tracker.media.id.source, SourceKind::ModArchive);
        assert_eq!(queued_tracker.playback_location, "/tmp/youta-module.xm");
    }

    #[test]
    fn show_now_playing_reselects_the_youtube_row_and_description() {
        let (mut controller, _state) = controller_with_mock_statuses([]);
        let playing = subscription_video_summary();
        let mut other = subscription_video_summary();
        other.video_id = "aqz-KE-bpKQ".to_owned();
        other.title = "Other result".to_owned();
        other.description = "Other description".to_owned();
        controller.youtube_results =
            vec![SearchItem::Video(playing.clone()), SearchItem::Video(other)];
        controller.refresh_youtube_rows();
        controller.play_queue_item(queue_item_from_video(&playing, None), false);
        controller.view.selected = 1;
        controller.request_selected_details();

        controller.dispatch(UiAction::ShowNowPlaying);

        assert_eq!(controller.view.screen, Screen::Search);
        assert_eq!(controller.view.selected, 0);
        assert_eq!(
            controller
                .view
                .details
                .as_ref()
                .map(|details| details.title.as_str()),
            Some("Fixture video")
        );
        assert_eq!(
            controller
                .view
                .details
                .as_ref()
                .map(|details| details.description.as_str()),
            Some("Fixture description")
        );
        assert_eq!(controller.view.right_panel_mode, RightPanelMode::Details);
    }

    #[test]
    fn history_details_never_reuse_a_stale_tracker_result() {
        let (mut controller, _state) = controller_with_mock_statuses([]);
        let stale_url =
            url::Url::parse("https://aminet.net/package/mods/atmos/Life").expect("tracker URL");
        controller.tracker_results = vec![TrackerItem {
            source: "Aminet mods".to_owned(),
            title: "Unrelated module".to_owned(),
            subtitle: "lha · 80.00 KiB".to_owned(),
            webpage_url: stale_url.clone(),
            download_url: None,
            expected_format: Some("lha".to_owned()),
            access: TrackerAccess::ArchiveNeedsInspection,
            prepared_path: None,
            insecure_transport: false,
        }];
        controller.view.screen = Screen::TrackerMusic;
        controller.refresh_tracker_rows();
        controller
            .store
            .insert_history(&HistoryEntry {
                id: 0,
                media_id: MediaId::new(SourceKind::Local, "/music/Дед инсайд.flac"),
                title: "Дед инсайд".to_owned(),
                started_at: 10,
                last_played_at: 11,
                position_seconds: 0,
                duration_seconds: None,
                finished: false,
            })
            .expect("history fixture");

        controller.show_screen(Screen::History);

        let details = controller.view.details.as_ref().expect("History details");
        assert_eq!(details.title, "Дед инсайд");
        assert_eq!(details.source, "local");
        assert_eq!(details.description, "stopped at 0:00");
        assert_eq!(
            details.media_id,
            Some(MediaId::new(SourceKind::Local, "/music/Дед инсайд.flac"))
        );
        assert!(!details.description.contains(stale_url.as_str()));
    }

    #[cfg(feature = "youtube-music")]
    #[test]
    fn show_now_playing_prefers_music_tab_for_music_youtube_url() {
        let (mut controller, _state) = controller_with_mock_statuses([]);
        let mut music = subscription_video_summary();
        music.webpage_url = Some(
            url::Url::parse("https://music.youtube.com/watch?v=dQw4w9WgXcQ").expect("music URL"),
        );
        controller.youtube_results = vec![SearchItem::Video(music.clone())];
        controller.youtube_music_results = vec![SearchItem::Video(music.clone())];
        controller.play_queue_item(queue_item_from_video(&music, None), false);
        controller.show_screen(Screen::History);

        controller.dispatch(UiAction::ShowNowPlaying);

        assert_eq!(controller.view.screen, Screen::YouTubeMusic);
        assert_eq!(controller.view.selected, 0);
        assert_eq!(
            controller
                .view
                .details
                .as_ref()
                .map(|details| details.title.as_str()),
            Some("Fixture video")
        );
    }

    #[test]
    fn show_now_playing_reconciles_subscription_heading_and_ignores_unsubscribed_cache() {
        let (mut controller, _state) = controller_with_mock_statuses([]);
        let playing = subscription_video_summary();
        assert!(
            controller
                .subscription_tree
                .subscribe_youtube_channel("Fixture channel", "UCfixture")
        );
        controller.subscription_entries = controller.subscription_tree.flattened_subscriptions();
        controller.cache_subscription_video_page(
            "UCfixture",
            SearchPage {
                page: 1,
                items: vec![SearchItem::Video(playing.clone())],
                next_page: None,
            },
        );
        controller.play_queue_item(queue_item_from_video(&playing, None), false);
        controller.view.subscriptions.source_title = "Wrong channel".to_owned();

        controller.dispatch(UiAction::ShowNowPlaying);

        assert_eq!(controller.view.screen, Screen::Subscriptions);
        assert_eq!(
            controller.view.subscriptions.source_title,
            "Fixture channel"
        );
        assert_eq!(controller.view.subscriptions.selected_item, 0);

        assert!(
            controller
                .subscription_tree
                .unsubscribe_youtube_channel("UCfixture")
        );
        controller.show_screen(Screen::History);
        controller.dispatch(UiAction::ShowNowPlaying);

        assert_eq!(
            controller.view.screen,
            Screen::History,
            "an unsubscribed channel's stale RAM cache must not select another source"
        );
        assert_eq!(
            controller
                .view
                .details
                .as_ref()
                .map(|details| details.title.as_str()),
            Some("Fixture video")
        );
    }

    #[test]
    fn show_now_playing_falls_back_to_queued_metadata_outside_the_current_list() {
        let (mut controller, _state) = controller_with_mock_statuses([]);
        let mut item = fixture_direct_item("spoken-word");
        item.media.description = Some("Queued description".to_owned());
        controller.play_queue_item(item, false);
        controller.show_screen(Screen::History);

        controller.dispatch(UiAction::ShowNowPlaying);

        let details = controller.view.details.as_ref().expect("queued details");
        assert_eq!(details.title, "spoken-word");
        assert_eq!(details.description, "Queued description");
        assert!(
            controller
                .view
                .status_line
                .contains("not in the current list")
        );
    }

    #[test]
    fn autoplay_advances_youtube_and_skips_scheduled_zero_duration_rows() {
        let active = PlaybackStatus {
            idle: false,
            position: Duration::from_secs(42),
            duration: Some(Duration::from_secs(42)),
            paused: false,
            ..PlaybackStatus::default()
        };
        let (mut controller, state, _, events) = controller_with_mock_lifecycle(
            [active],
            [PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted],
        );
        controller.config.playback.autoplay = true;
        controller.view.autoplay = true;
        controller.view.screen = Screen::Search;
        let first = subscription_video_summary();
        let mut scheduled = first.clone();
        scheduled.video_id = "planned0000".to_owned();
        scheduled.title = "Scheduled placeholder".to_owned();
        scheduled.duration_seconds = Some(0);
        let mut second = first.clone();
        second.video_id = "aqz-KE-bpKQ".to_owned();
        second.title = "Second playable video".to_owned();
        controller.youtube_results = vec![
            SearchItem::Video(first.clone()),
            SearchItem::Video(scheduled),
            SearchItem::Video(second),
        ];
        controller.view.selected = 0;
        controller.play_queue_item(queue_item_from_video(&first, None), false);
        controller.update_player();
        events
            .lock()
            .expect("mock events")
            .push_back(PlaybackEvent::Ended(PlaybackEnd {
                reason: PlaybackEndReason::Eof,
                error: None,
                file_error: None,
                diagnostic: None,
            }));

        controller.update_player();

        assert_eq!(
            state
                .lock()
                .expect("mock state")
                .played
                .iter()
                .filter_map(|input| input.title.as_deref())
                .collect::<Vec<_>>(),
            ["Fixture video", "Second playable video"]
        );
        assert_eq!(
            controller.current_autoplay_origin,
            Some(AutoplayOrigin::YouTube {
                generation: controller.search_generation,
                index: 2,
            })
        );
    }

    #[test]
    fn explicit_queue_item_has_priority_over_autoplay_source_continuation() {
        let active = PlaybackStatus {
            idle: false,
            position: Duration::from_secs(42),
            duration: Some(Duration::from_secs(42)),
            paused: false,
            ..PlaybackStatus::default()
        };
        let (mut controller, state, _, events) = controller_with_mock_lifecycle(
            [active],
            [PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted],
        );
        controller.config.playback.autoplay = true;
        controller.view.autoplay = true;
        controller.view.screen = Screen::Search;
        let first = subscription_video_summary();
        let mut second = first.clone();
        second.video_id = "aqz-KE-bpKQ".to_owned();
        second.title = "Automatic second video".to_owned();
        let mut explicit_first = first.clone();
        explicit_first.video_id = "queued00001".to_owned();
        explicit_first.title = "Explicit YouTube item one".to_owned();
        let mut explicit_second = first.clone();
        explicit_second.video_id = "queued00002".to_owned();
        explicit_second.title = "Explicit YouTube item two".to_owned();
        controller.youtube_results = vec![
            SearchItem::Video(first.clone()),
            SearchItem::Video(second),
            SearchItem::Video(explicit_first.clone()),
            SearchItem::Video(explicit_second.clone()),
        ];
        controller.play_queue_item(queue_item_from_video(&first, None), false);
        controller
            .playback_queue
            .push(queue_item_from_video(&explicit_first, None));
        controller
            .playback_queue
            .push(queue_item_from_video(&explicit_second, None));
        controller.update_player();
        events
            .lock()
            .expect("mock events")
            .push_back(PlaybackEvent::Ended(PlaybackEnd {
                reason: PlaybackEndReason::Eof,
                error: None,
                file_error: None,
                diagnostic: None,
            }));

        controller.update_player();

        assert_eq!(
            state
                .lock()
                .expect("mock state")
                .played
                .iter()
                .filter_map(|input| input.title.as_deref())
                .collect::<Vec<_>>(),
            ["Fixture video", "Explicit YouTube item one"]
        );

        for _ in 0..2 {
            events.lock().expect("mock events").extend([
                PlaybackEvent::MediaLoaded,
                PlaybackEvent::PlaybackStarted,
                PlaybackEvent::Ended(PlaybackEnd {
                    reason: PlaybackEndReason::Eof,
                    error: None,
                    file_error: None,
                    diagnostic: None,
                }),
            ]);
            controller.update_player();
        }

        assert_eq!(
            state
                .lock()
                .expect("mock state")
                .played
                .iter()
                .filter_map(|input| input.title.as_deref())
                .collect::<Vec<_>>(),
            [
                "Fixture video",
                "Explicit YouTube item one",
                "Explicit YouTube item two",
                "Automatic second video"
            ],
            "queued items with their own list origins must not replace the original resume point"
        );
    }

    #[test]
    fn disabled_autoplay_stops_at_the_end_of_a_youtube_list() {
        let active = PlaybackStatus {
            idle: false,
            position: Duration::from_secs(42),
            duration: Some(Duration::from_secs(42)),
            paused: false,
            ..PlaybackStatus::default()
        };
        let (mut controller, state, _, events) = controller_with_mock_lifecycle(
            [active],
            [PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted],
        );
        controller.view.screen = Screen::Search;
        let first = subscription_video_summary();
        let mut second = first.clone();
        second.video_id = "aqz-KE-bpKQ".to_owned();
        controller.youtube_results =
            vec![SearchItem::Video(first.clone()), SearchItem::Video(second)];
        controller.play_queue_item(queue_item_from_video(&first, None), false);
        controller.update_player();
        events
            .lock()
            .expect("mock events")
            .push_back(PlaybackEvent::Ended(PlaybackEnd {
                reason: PlaybackEndReason::Eof,
                error: None,
                file_error: None,
                diagnostic: None,
            }));

        controller.update_player();

        assert_eq!(
            state.lock().expect("mock state").played.len(),
            1,
            "the default must never start a second list item"
        );
        assert_eq!(controller.view.status_line, "Playback queue finished");
    }

    #[test]
    fn autoplay_never_uses_replaced_search_results() {
        let active = PlaybackStatus {
            idle: false,
            position: Duration::from_secs(42),
            duration: Some(Duration::from_secs(42)),
            paused: false,
            ..PlaybackStatus::default()
        };
        let (mut controller, state, _, events) = controller_with_mock_lifecycle(
            [active],
            [PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted],
        );
        controller.config.playback.autoplay = true;
        controller.view.autoplay = true;
        controller.view.screen = Screen::Search;
        let first = subscription_video_summary();
        let mut former_next = first.clone();
        former_next.video_id = "former-next".to_owned();
        former_next.title = "Former next result".to_owned();
        controller.youtube_results = vec![
            SearchItem::Video(first.clone()),
            SearchItem::Video(former_next),
        ];
        controller.play_queue_item(queue_item_from_video(&first, None), false);
        controller.update_player();

        controller.supersede_search_generation();
        let mut unrelated = first.clone();
        unrelated.video_id = "unrelated01".to_owned();
        unrelated.title = "Unrelated replacement".to_owned();
        controller.youtube_results = vec![SearchItem::Video(unrelated)];
        events
            .lock()
            .expect("mock events")
            .push_back(PlaybackEvent::Ended(PlaybackEnd {
                reason: PlaybackEndReason::Eof,
                error: None,
                file_error: None,
                diagnostic: None,
            }));

        controller.update_player();

        assert_eq!(state.lock().expect("mock state").played.len(), 1);
        assert_eq!(
            controller.view.status_line,
            "Autoplay stopped because the source list changed"
        );
    }

    #[test]
    fn autoplay_advances_local_browser_files_in_listing_order() {
        let directory = tempfile::tempdir().expect("temporary local folder");
        let first_path = directory.path().join("first.mp3");
        let second_path = directory.path().join("second.flac");
        std::fs::write(&first_path, b"first").expect("first fixture");
        std::fs::write(&second_path, b"second").expect("second fixture");
        let listing = crate::local_browser::list_local_directory(
            directory.path(),
            crate::local_browser::LocalBrowseLimits::default(),
        )
        .expect("local listing");
        let active = PlaybackStatus {
            idle: false,
            position: Duration::from_secs(1),
            duration: Some(Duration::from_secs(1)),
            paused: false,
            ..PlaybackStatus::default()
        };
        let (mut controller, state, _, events) = controller_with_mock_lifecycle(
            [active],
            [PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted],
        );
        controller.config.playback.autoplay = true;
        controller.view.autoplay = true;
        controller.view.screen = Screen::Local;
        controller.local_listing = Some(listing);
        controller.refresh_local_browser_rows();
        controller.view.selected = usize::from(
            controller
                .local_listing
                .as_ref()
                .is_some_and(|listing| listing.parent.is_some()),
        );
        controller.activate_local_browser_selection();
        controller.update_player();
        events
            .lock()
            .expect("mock events")
            .push_back(PlaybackEvent::Ended(PlaybackEnd {
                reason: PlaybackEndReason::Eof,
                error: None,
                file_error: None,
                diagnostic: None,
            }));

        controller.update_player();

        assert_eq!(
            state
                .lock()
                .expect("mock state")
                .played
                .iter()
                .map(|input| input.location.clone())
                .collect::<Vec<_>>(),
            [
                first_path.display().to_string(),
                second_path.display().to_string()
            ]
        );
    }

    #[test]
    fn autoplay_advances_prepared_tracker_modules_and_skips_metadata_rows() {
        let directory = tempfile::tempdir().expect("temporary tracker folder");
        let first_path = directory.path().join("first.mod");
        let second_path = directory.path().join("second.xm");
        std::fs::write(&first_path, b"first").expect("first fixture");
        std::fs::write(&second_path, b"second").expect("second fixture");
        let tracker = |title: &str, page: &str, path: Option<PathBuf>, access| TrackerItem {
            source: "Fixture archive".to_owned(),
            title: title.to_owned(),
            subtitle: String::new(),
            webpage_url: url::Url::parse(page).expect("tracker page URL"),
            download_url: None,
            expected_format: None,
            access,
            prepared_path: path,
            insecure_transport: false,
        };
        let active = PlaybackStatus {
            idle: false,
            position: Duration::from_secs(1),
            duration: Some(Duration::from_secs(1)),
            paused: false,
            ..PlaybackStatus::default()
        };
        let (mut controller, state, _, events) = controller_with_mock_lifecycle(
            [active],
            [PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted],
        );
        controller.config.playback.autoplay = true;
        controller.view.autoplay = true;
        controller.view.screen = Screen::TrackerMusic;
        controller.tracker_results = vec![
            tracker(
                "First module",
                "https://mods.example/first",
                Some(first_path),
                TrackerAccess::DirectModule,
            ),
            tracker(
                "Metadata only",
                "https://mods.example/info",
                None,
                TrackerAccess::MetadataOnly,
            ),
            tracker(
                "Second module",
                "https://mods.example/second",
                Some(second_path),
                TrackerAccess::DirectModule,
            ),
        ];
        controller.view.selected = 0;
        controller.play_queue_item(
            queue_item_from_tracker(&controller.tracker_results[0]).expect("first queue item"),
            false,
        );
        controller.update_player();
        events
            .lock()
            .expect("mock events")
            .push_back(PlaybackEvent::Ended(PlaybackEnd {
                reason: PlaybackEndReason::Eof,
                error: None,
                file_error: None,
                diagnostic: None,
            }));

        controller.update_player();

        assert_eq!(
            state
                .lock()
                .expect("mock state")
                .played
                .iter()
                .filter_map(|input| input.title.as_deref())
                .collect::<Vec<_>>(),
            ["First module", "Second module"]
        );
        assert_eq!(
            controller.current_autoplay_origin,
            Some(AutoplayOrigin::Tracker {
                generation: controller.search_generation,
                index: 2,
            })
        );
    }

    #[cfg(feature = "tracker-music")]
    #[test]
    fn manual_playback_revokes_pending_tracker_autoplay_ownership() {
        let (mut controller, state) = controller_with_mock_statuses([]);
        controller.config.playback.autoplay = true;
        controller.view.autoplay = true;
        let item_key = "https://mods.example/remote-archive";
        controller.tracker_results = vec![TrackerItem {
            source: "Fixture archive".to_owned(),
            title: "Remote archive".to_owned(),
            subtitle: String::new(),
            webpage_url: url::Url::parse(item_key).expect("tracker page URL"),
            download_url: url::Url::parse("https://mods.example/archive.zip").ok(),
            expected_format: None,
            access: TrackerAccess::ArchiveNeedsInspection,
            prepared_path: None,
            insecure_transport: false,
        }];
        controller.pending_tracker_preparation = Some(PendingTrackerPreparation {
            generation: controller.search_generation,
            item_key: item_key.to_owned(),
            owner: TrackerPreparationOwner::Autoplay(AutoplayOrigin::Tracker {
                generation: controller.search_generation,
                index: 0,
            }),
        });

        controller.play_queue_item(fixture_direct_item("manual replacement"), false);
        assert_eq!(
            controller
                .pending_tracker_preparation
                .as_ref()
                .map(|pending| &pending.owner),
            Some(&TrackerPreparationOwner::CanceledAutoplay)
        );

        controller.handle_provider_response(ProviderResponse::TrackerPrepared {
            generation: controller.search_generation,
            item_key: item_key.to_owned(),
            result: Ok(vec![crate::tracker_media::PreparedTrackerModule {
                path: PathBuf::from("/tmp/youta-prepared-fixture.mod"),
                display_name: "Prepared module".to_owned(),
                format: "mod".to_owned(),
                size_bytes: 42,
            }]),
        });

        assert_eq!(
            state
                .lock()
                .expect("mock state")
                .played
                .iter()
                .filter_map(|input| input.title.as_deref())
                .collect::<Vec<_>>(),
            ["manual replacement"],
            "a canceled background response must never replace manual playback"
        );
        assert!(controller.pending_tracker_preparation.is_none());
        assert_eq!(
            controller.tracker_results[0]
                .prepared_path
                .as_deref()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str()),
            Some("youta-prepared-fixture.mod")
        );
    }

    #[test]
    fn authoritative_eof_advances_the_queue_only_once() {
        let active = crate::playback::PlaybackStatus {
            idle: false,
            position: Duration::from_secs(10),
            duration: Some(Duration::from_secs(10)),
            paused: false,
            ..crate::playback::PlaybackStatus::default()
        };
        let (mut controller, state, _, events) = controller_with_mock_lifecycle(
            [active],
            [PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted],
        );
        controller.play_queue_item(fixture_direct_item("first"), false);
        controller
            .playback_queue
            .push(fixture_direct_item("second"));

        controller.update_player();
        events
            .lock()
            .expect("mock events")
            .push_back(PlaybackEvent::Ended(PlaybackEnd {
                reason: PlaybackEndReason::Eof,
                error: None,
                file_error: None,
                diagnostic: None,
            }));
        controller.update_player();

        let state = state.lock().expect("mock state");
        assert_eq!(
            state
                .played
                .iter()
                .filter_map(|input| input.title.as_deref())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert_eq!(
            controller
                .playback_queue
                .current()
                .map(|item| item.media.title.as_str()),
            Some("second")
        );
    }

    #[test]
    fn authoritative_eof_restarts_current_item_in_repeat_one_mode() {
        let active = crate::playback::PlaybackStatus {
            idle: false,
            position: Duration::from_secs(10),
            duration: Some(Duration::from_secs(10)),
            paused: false,
            ..crate::playback::PlaybackStatus::default()
        };
        let (mut controller, state, _, events) = controller_with_mock_lifecycle(
            [active],
            [PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted],
        );
        controller.playback_queue.repeat_one = true;
        controller.play_queue_item(fixture_direct_item("first"), false);
        controller
            .playback_queue
            .push(fixture_direct_item("second"));

        controller.update_player();
        events
            .lock()
            .expect("mock events")
            .push_back(PlaybackEvent::Ended(PlaybackEnd {
                reason: PlaybackEndReason::Eof,
                error: None,
                file_error: None,
                diagnostic: None,
            }));
        controller.update_player();

        let state = state.lock().expect("mock state");
        assert_eq!(
            state
                .played
                .iter()
                .filter_map(|input| input.title.as_deref())
                .collect::<Vec<_>>(),
            ["first", "first"]
        );
        assert_eq!(
            controller
                .playback_queue
                .current()
                .map(|item| item.media.title.as_str()),
            Some("first")
        );
    }

    #[test]
    fn transient_idle_during_loading_is_not_treated_as_queue_completion() {
        let apparent_activity = PlaybackStatus {
            idle: false,
            paused: true,
            ..PlaybackStatus::default()
        };
        let (mut controller, state, _, _) =
            controller_with_mock_lifecycle([apparent_activity, PlaybackStatus::default()], []);
        controller.play_queue_item(fixture_direct_item("first"), false);
        controller
            .playback_queue
            .push(fixture_direct_item("second"));

        assert!(controller.view.status_line.starts_with("Loading first"));
        assert!(controller.view.playback_starting);
        assert!(
            controller
                .store
                .history(false, 10)
                .expect("history")
                .is_empty()
        );
        controller.update_player();
        controller.update_player();

        assert_eq!(controller.playback_phase, PlaybackPhase::Loading);
        assert!(controller.view.status_line.starts_with("Loading first"));
        assert!(
            !controller
                .view
                .status_line
                .contains("Playback queue finished")
        );
        assert_eq!(
            state
                .lock()
                .expect("mock state")
                .played
                .iter()
                .filter_map(|input| input.title.as_deref())
                .collect::<Vec<_>>(),
            ["first"]
        );
        assert_eq!(
            controller
                .playback_queue
                .current()
                .map(|item| item.media.title.as_str()),
            Some("first")
        );
        assert!(
            controller
                .store
                .history(false, 10)
                .expect("history")
                .is_empty()
        );
    }

    #[test]
    fn asynchronous_load_failure_opens_diagnostics_without_history_or_queue_advance() {
        let failure = PlaybackEvent::Ended(PlaybackEnd {
            reason: PlaybackEndReason::Error,
            error: Some("loading failed".to_owned()),
            file_error: Some("HTTP 403".to_owned()),
            diagnostic: Some("ytdl_hook: fixture unavailable".to_owned()),
        });
        let (mut controller, state, _, _) = controller_with_mock_lifecycle([], [failure]);
        controller.diagnostic_helpers_cache = Some(Vec::new());
        controller.play_queue_item(fixture_direct_item("first"), false);
        controller
            .playback_queue
            .push(fixture_direct_item("second"));

        assert!(controller.view.status_line.starts_with("Loading first"));
        assert!(
            controller
                .store
                .history(false, 10)
                .expect("history")
                .is_empty()
        );
        controller.update_player();

        let popup = controller
            .view
            .error_popup
            .as_ref()
            .expect("playback error popup");
        assert_eq!(popup.title, "Playback failed");
        assert!(popup.report.contains("loading failed"));
        assert!(popup.report.contains("HTTP 403"));
        assert!(popup.report.contains("ytdl_hook: fixture unavailable"));
        assert_eq!(controller.playback_phase, PlaybackPhase::Idle);
        assert!(controller.current_media.is_none());
        assert!(controller.view.playback.idle);
        assert!(!controller.view.playback_starting);
        assert_eq!(controller.view.playing_media_id, None);
        assert!(
            !controller
                .view
                .status_line
                .contains("Playback queue finished")
        );
        assert_eq!(
            controller
                .playback_queue
                .current()
                .map(|item| item.media.title.as_str()),
            Some("first")
        );
        assert_eq!(
            state
                .lock()
                .expect("mock state")
                .played
                .iter()
                .filter_map(|input| input.title.as_deref())
                .collect::<Vec<_>>(),
            ["first"]
        );
        assert!(
            controller
                .store
                .history(false, 10)
                .expect("history")
                .is_empty()
        );
    }

    #[test]
    fn youtube_http_403_before_start_retries_once_with_format_validation() {
        let failure = PlaybackEnd {
            reason: PlaybackEndReason::Error,
            error: Some("loading failed".to_owned()),
            file_error: Some("HTTP error 403 Forbidden".to_owned()),
            diagnostic: Some("ffmpeg: https request was forbidden".to_owned()),
        };
        let (mut controller, state, _, events) =
            controller_with_mock_lifecycle([], [PlaybackEvent::Ended(failure.clone())]);
        controller.diagnostic_helpers_cache = Some(Vec::new());
        controller.play_queue_item(fixture_youtube_item("fixture video"), false);

        controller.update_player();

        assert!(
            controller.view.error_popup.is_none(),
            "the first pre-playback CDN rejection should use the bounded fallback"
        );
        assert_eq!(controller.playback_phase, PlaybackPhase::Loading);
        assert!(
            controller
                .view
                .status_line
                .contains("validating alternate YouTube formats")
        );
        {
            let state = state.lock().expect("mock state");
            let played = &state.played;
            assert_eq!(played.len(), 2);
            assert!(!played[0].verify_remote_format);
            assert!(played[1].verify_remote_format);
            assert_eq!(played[0].location, played[1].location);
            assert_eq!(
                played[1].location,
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
            );
        }

        events
            .lock()
            .expect("mock events")
            .push_back(PlaybackEvent::Ended(failure));
        controller.update_player();

        let popup = controller
            .view
            .error_popup
            .as_ref()
            .expect("the second rejection should remain visible for diagnosis");
        assert_eq!(popup.title, "Playback failed");
        assert!(popup.report.contains("HTTP error 403 Forbidden"));
        assert_eq!(
            state.lock().expect("mock state").played.len(),
            2,
            "a failing checked-format retry must not loop"
        );
    }

    #[cfg(feature = "yt-dlp")]
    #[test]
    fn youtube_prewarm_debounce_keeps_not_yet_due_selection_scheduled() {
        let (mut controller, _, _, _) = controller_with_mock_lifecycle([], []);
        let video = subscription_video_summary();
        let media_id = MediaId::new(SourceKind::YouTube, &video.video_id);

        controller.update_youtube_prewarm_for_selection(Some(&SearchItem::Video(video)));
        let scheduled = controller
            .scheduled_youtube_prewarm
            .as_ref()
            .expect("eligible selection should be debounced");
        assert_eq!(scheduled.media_id, media_id);
        let before_deadline = scheduled
            .due_at
            .checked_sub(Duration::from_millis(1))
            .expect("fixture deadline");

        controller.request_due_youtube_prewarm(before_deadline);

        assert!(
            controller.scheduled_youtube_prewarm.is_some(),
            "an early UI tick must not discard the debounced request"
        );
        assert!(controller.active_youtube_prewarm.is_none());
    }

    #[cfg(feature = "yt-dlp")]
    #[test]
    fn youtube_prewarm_is_one_shot_and_rejects_stale_or_expiring_streams() {
        let (mut controller, _, _, _) = controller_with_mock_lifecycle([], []);
        let media_id = MediaId::new(SourceKind::YouTube, "dQw4w9WgXcQ");
        let fixture_audio = |expires_at_unix| {
            PrewarmedYouTubeAudio::for_test(
                url::Url::parse("https://signed.example/audio?token=secret")
                    .expect("fixture signed URL"),
                expires_at_unix,
            )
        };

        controller.ready_youtube_prewarm = Some(ReadyYouTubePrewarm {
            media_id: media_id.clone(),
            completed_at: Instant::now(),
            audio: fixture_audio(None),
        });
        assert!(
            controller.take_ready_youtube_prewarm(&media_id).is_some(),
            "a fresh exact result should be consumed"
        );
        assert!(
            controller.take_ready_youtube_prewarm(&media_id).is_none(),
            "the signed stream must be eligible for only one playback attempt"
        );

        controller.ready_youtube_prewarm = Some(ReadyYouTubePrewarm {
            media_id: media_id.clone(),
            completed_at: Instant::now()
                .checked_sub(YOUTUBE_PREWARM_TTL + Duration::from_millis(1))
                .expect("fixture instant"),
            audio: fixture_audio(None),
        });
        assert!(
            controller.take_ready_youtube_prewarm(&media_id).is_none(),
            "a result older than the controller TTL must be discarded"
        );

        let now_unix = u64::try_from(unix_time()).expect("fixture Unix time");
        controller.ready_youtube_prewarm = Some(ReadyYouTubePrewarm {
            media_id: media_id.clone(),
            completed_at: Instant::now(),
            audio: fixture_audio(Some(
                now_unix.saturating_add(YOUTUBE_PREWARM_EXPIRY_MARGIN_SECONDS),
            )),
        });
        assert!(
            controller.take_ready_youtube_prewarm(&media_id).is_none(),
            "a URL expiring inside the safety margin must not reach playback"
        );
    }

    #[cfg(feature = "yt-dlp")]
    #[test]
    fn youtube_prewarm_response_rejects_an_obsolete_generation() {
        let (mut controller, _, _, _) = controller_with_mock_lifecycle([], []);
        let media_id = MediaId::new(SourceKind::YouTube, "dQw4w9WgXcQ");
        controller.youtube_prewarm_generation = 8;
        controller.active_youtube_prewarm = Some(ActiveYouTubePrewarm {
            generation: 7,
            media_id: media_id.clone(),
            cancellation: YouTubePrewarmCancellation::new(),
        });
        let (responses, response_receiver) = bounded(1);
        controller.youtube_prewarm_responses = Some(response_receiver);
        responses
            .send(YouTubePrewarmCompletion {
                media_id,
                completed_at: Instant::now(),
                result: YouTubePrewarmResult::for_test(
                    7,
                    Ok(PrewarmedYouTubeAudio::for_test(
                        url::Url::parse("https://signed.example/audio?token=stale")
                            .expect("fixture signed URL"),
                        None,
                    )),
                ),
            })
            .expect("fixture response");

        controller.drain_youtube_prewarm_responses();

        assert!(controller.active_youtube_prewarm.is_none());
        assert!(controller.ready_youtube_prewarm.is_none());
        assert!(controller.youtube_prewarm_failure.is_none());
    }

    #[cfg(feature = "yt-dlp")]
    #[test]
    fn prewarmed_youtube_load_uses_bounded_fallbacks_without_losing_queue_state() {
        let (mut controller, state, _, events) = controller_with_mock_lifecycle([], []);
        controller.diagnostic_helpers_cache = Some(Vec::new());
        let mut item = fixture_youtube_item("prewarmed fixture");
        item.start_at_seconds = Some(45);
        let media_id = item.media.id.clone();
        controller.ready_youtube_prewarm = Some(ReadyYouTubePrewarm {
            media_id: media_id.clone(),
            completed_at: Instant::now(),
            audio: PrewarmedYouTubeAudio::for_test(
                url::Url::parse("https://signed.example/audio?token=secret")
                    .expect("fixture signed URL"),
                None,
            ),
        });
        let origin = AutoplayOrigin::YouTube {
            generation: 7,
            index: 3,
        };

        controller.play_queue_item_with_origin(item, false, Some(origin.clone()));

        assert_eq!(
            controller.playback_load_kind,
            PlaybackLoadKind::YouTubeDirect
        );
        assert_eq!(controller.current_autoplay_origin, Some(origin.clone()));
        assert_eq!(
            controller
                .pending_history
                .as_ref()
                .map(|history| history.position_seconds),
            Some(45)
        );
        {
            let state = state.lock().expect("mock state");
            assert_eq!(state.played.len(), 1);
            assert!(state.played[0].bypass_ytdl);
            assert_eq!(
                state.played[0].location,
                "https://signed.example/audio?token=secret"
            );
            assert!(!format!("{:?}", state.played[0]).contains("signed.example"));
            assert!(!format!("{:?}", state.played[0]).contains("secret"));
        }

        {
            let mut events = events.lock().expect("mock events");
            events.push_back(PlaybackEvent::MediaLoaded);
            events.push_back(PlaybackEvent::Ended(PlaybackEnd {
                reason: PlaybackEndReason::Error,
                error: None,
                file_error: Some("unrecognized file format".to_owned()),
                diagnostic: Some("https://signed.example/audio?token=secret".to_owned()),
            }));
        }
        controller.update_player();

        assert!(controller.view.error_popup.is_none());
        assert_eq!(
            controller.playback_load_kind,
            PlaybackLoadKind::YouTubeCanonical
        );
        assert_eq!(controller.current_autoplay_origin, Some(origin.clone()));
        assert_eq!(controller.current_media.as_ref(), Some(&media_id));
        assert_eq!(
            controller
                .playback_queue
                .current()
                .map(|queued| &queued.media.id),
            Some(&media_id)
        );
        {
            let state = state.lock().expect("mock state");
            assert_eq!(state.played.len(), 2);
            assert!(!state.played[1].bypass_ytdl);
            assert!(!state.played[1].verify_remote_format);
            assert_eq!(
                state.played[1].location,
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
            );
            assert_eq!(state.played[1].start_at, Duration::from_secs(45));
        }

        events
            .lock()
            .expect("mock events")
            .push_back(PlaybackEvent::Ended(PlaybackEnd {
                reason: PlaybackEndReason::Error,
                error: Some("canonical stream failed".to_owned()),
                file_error: Some("HTTP error 403 Forbidden".to_owned()),
                diagnostic: None,
            }));
        controller.update_player();

        assert!(controller.view.error_popup.is_none());
        assert_eq!(
            controller.playback_load_kind,
            PlaybackLoadKind::YouTubeChecked
        );
        assert_eq!(controller.current_autoplay_origin, Some(origin));
        {
            let state = state.lock().expect("mock state");
            assert_eq!(state.played.len(), 3);
            assert!(!state.played[2].bypass_ytdl);
            assert!(state.played[2].verify_remote_format);
            assert_eq!(state.played[2].start_at, Duration::from_secs(45));
        }

        events
            .lock()
            .expect("mock events")
            .push_back(PlaybackEvent::MediaLoaded);
        controller.update_player();
        assert_eq!(controller.playback_phase, PlaybackPhase::Loaded);
        events
            .lock()
            .expect("mock events")
            .push_back(PlaybackEvent::Ended(PlaybackEnd {
                reason: PlaybackEndReason::Error,
                error: Some("decoder failed after load".to_owned()),
                file_error: None,
                diagnostic: None,
            }));
        controller.update_player();

        assert_eq!(state.lock().expect("mock state").played.len(), 3);
        assert_eq!(
            controller
                .view
                .error_popup
                .as_ref()
                .map(|popup| popup.title.as_str()),
            Some("Playback failed")
        );
    }

    #[test]
    fn youtube_non_403_load_failure_does_not_enable_format_validation() {
        let failure = PlaybackEvent::Ended(PlaybackEnd {
            reason: PlaybackEndReason::Error,
            error: Some("loading failed".to_owned()),
            file_error: Some("HTTP error 404 Not Found".to_owned()),
            diagnostic: None,
        });
        let (mut controller, state, _, _) = controller_with_mock_lifecycle([], [failure]);
        controller.diagnostic_helpers_cache = Some(Vec::new());
        controller.play_queue_item(fixture_youtube_item("missing video"), false);

        controller.update_player();

        assert!(controller.view.error_popup.is_some());
        let state = state.lock().expect("mock state");
        let played = &state.played;
        assert_eq!(played.len(), 1);
        assert!(!played[0].verify_remote_format);
    }

    #[test]
    fn activating_a_playable_row_starts_ascii_feedback_immediately() {
        let (mut controller, _state, _, _) = controller_with_mock_lifecycle([], []);
        let video = subscription_video_summary();
        let expected_media = MediaId::new(SourceKind::YouTube, &video.video_id);
        controller.youtube_results = vec![SearchItem::Video(video)];
        controller.refresh_youtube_rows();

        controller.dispatch(UiAction::ActivateSelection);

        assert_eq!(controller.playback_phase, PlaybackPhase::Loading);
        assert!(controller.view.playback_starting);
        assert_eq!(controller.view.playback_start_animation_frame, 0);
        assert_eq!(controller.view.playing_media_id, None);
        assert_eq!(
            controller.view.rows[0].media_id.as_ref(),
            Some(&expected_media)
        );
        assert!(
            controller
                .view
                .status_line
                .starts_with("Loading Fixture video")
        );
    }

    #[test]
    fn playback_feedback_tracks_loaded_started_paused_and_stopped_lifecycle() {
        let active = PlaybackStatus {
            idle: false,
            paused: true,
            ..PlaybackStatus::default()
        };
        let (mut controller, _state, _, events) =
            controller_with_mock_lifecycle([active], [PlaybackEvent::MediaLoaded]);
        let item = fixture_direct_item("first");
        let media_id = item.media.id.clone();
        controller.play_queue_item(item, false);

        controller.advance_playback_start_animation();
        assert!(controller.view.playback_starting);
        assert_eq!(controller.view.playback_start_animation_frame, 1);
        assert_eq!(controller.view.playing_media_id, None);

        controller.update_player();
        assert_eq!(controller.playback_phase, PlaybackPhase::Loaded);
        assert!(controller.view.playback_starting);
        assert_eq!(controller.view.playing_media_id, None);

        events
            .lock()
            .expect("mock events")
            .push_back(PlaybackEvent::PlaybackStarted);
        controller.update_player();
        assert_eq!(controller.playback_phase, PlaybackPhase::Playing);
        assert!(!controller.view.playback_starting);
        assert_eq!(controller.view.playback_start_animation_frame, 0);
        assert_eq!(controller.view.playing_media_id.as_ref(), Some(&media_id));

        controller.view.playback.paused = true;
        assert_eq!(
            controller.view.playing_media_id.as_ref(),
            Some(&media_id),
            "pausing must not remove the authoritative playing marker"
        );

        events
            .lock()
            .expect("mock events")
            .push_back(PlaybackEvent::Ended(PlaybackEnd {
                reason: PlaybackEndReason::Stop,
                error: None,
                file_error: None,
                diagnostic: None,
            }));
        controller.update_player();
        assert_eq!(controller.playback_phase, PlaybackPhase::Idle);
        assert!(!controller.view.playback_starting);
        assert_eq!(controller.view.playing_media_id, None);
    }

    #[test]
    fn backend_stream_url_cannot_replace_the_queue_title() {
        let derived_stream_title = "webm&rqh=1&gir=yes&clen=101236984&dur=7717.401\
                                    &lmt=1784786265509357&c=ANDROID_VR";
        let active = PlaybackStatus {
            idle: false,
            paused: false,
            title: Some(derived_stream_title.to_owned()),
            ..PlaybackStatus::default()
        };
        let (mut controller, _state, _statuses, events) =
            controller_with_mock_lifecycle([active.clone(), active], [PlaybackEvent::MediaLoaded]);
        controller.play_queue_item(fixture_direct_item("Hello World"), false);

        controller.update_player();
        assert_eq!(
            controller.view.playback.title.as_deref(),
            Some("Hello World")
        );

        events
            .lock()
            .expect("mock events")
            .push_back(PlaybackEvent::PlaybackStarted);
        controller.update_player();

        assert_eq!(controller.view.status_line, "Playing Hello World");
        assert_eq!(
            controller.view.playback.title.as_deref(),
            Some("Hello World")
        );
        assert!(
            !controller
                .view
                .playback
                .title
                .as_deref()
                .is_some_and(|title| { title.contains("clen=") || title.contains("ANDROID_VR") })
        );
    }

    #[test]
    fn playback_start_is_required_before_playing_status_and_history() {
        let active = PlaybackStatus {
            idle: false,
            position: Duration::from_secs(3),
            duration: Some(Duration::from_secs(30)),
            paused: false,
            ..PlaybackStatus::default()
        };
        let (mut controller, state, _, events) =
            controller_with_mock_lifecycle([active], [PlaybackEvent::MediaLoaded]);
        controller.play_queue_item(fixture_direct_item("first"), false);

        assert!(controller.view.status_line.starts_with("Loading first"));
        assert!(
            controller
                .store
                .history(false, 10)
                .expect("history")
                .is_empty()
        );
        controller.update_player();
        assert!(controller.view.status_line.starts_with("Loaded first"));
        assert!(
            controller
                .store
                .history(false, 10)
                .expect("history")
                .is_empty()
        );

        events
            .lock()
            .expect("mock events")
            .push_back(PlaybackEvent::PlaybackStarted);
        controller.update_player();

        assert_eq!(controller.playback_phase, PlaybackPhase::Playing);
        assert_eq!(controller.view.status_line, "Playing first");
        assert_eq!(
            controller.store.history(false, 10).expect("history").len(),
            1
        );
        let commands = &state.lock().expect("mock state").commands;
        assert_eq!(
            commands.get(0..2),
            Some([PlayerCommand::SetVolume(80), PlayerCommand::SetSpeed(1.0)].as_slice())
        );
    }

    #[test]
    fn loaded_media_without_playback_start_remains_buffering_and_unrecorded() {
        let apparent_activity = PlaybackStatus {
            idle: false,
            position: Duration::from_secs(7),
            duration: Some(Duration::from_secs(30)),
            paused: false,
            buffering: false,
            ..PlaybackStatus::default()
        };
        let (mut controller, state, _, _) = controller_with_mock_lifecycle(
            [
                apparent_activity.clone(),
                PlaybackStatus::default(),
                apparent_activity,
            ],
            [PlaybackEvent::MediaLoaded],
        );
        controller.play_queue_item(fixture_direct_item("silent"), false);
        controller
            .playback_queue
            .push(fixture_direct_item("second"));

        assert!(controller.view.playback_starting);
        controller.update_player();
        controller.update_player();
        controller.update_player();

        assert_eq!(controller.playback_phase, PlaybackPhase::Loaded);
        assert_eq!(
            controller.view.status_line,
            "Loaded silent; starting audio…"
        );
        assert!(controller.view.playback.paused);
        assert!(controller.view.playback.buffering);
        assert!(
            !controller
                .view
                .status_line
                .contains("Playback queue finished")
        );
        assert_eq!(
            controller
                .playback_queue
                .current()
                .map(|item| item.media.title.as_str()),
            Some("silent")
        );
        assert_eq!(
            state
                .lock()
                .expect("mock state")
                .played
                .iter()
                .filter_map(|input| input.title.as_deref())
                .collect::<Vec<_>>(),
            ["silent"]
        );
        assert!(
            controller
                .store
                .history(false, 10)
                .expect("history")
                .is_empty()
        );
    }

    #[test]
    fn audio_output_failure_after_media_loaded_never_records_playback() {
        let failure = PlaybackEnd {
            reason: PlaybackEndReason::Error,
            error: Some("audio output initialization failed".to_owned()),
            file_error: None,
            diagnostic: Some("ao/alsa: device is busy".to_owned()),
        };
        let (mut controller, state, _, _) = controller_with_mock_lifecycle(
            [],
            [PlaybackEvent::MediaLoaded, PlaybackEvent::Ended(failure)],
        );
        controller.diagnostic_helpers_cache = Some(Vec::new());
        controller.play_queue_item(fixture_direct_item("silent"), false);
        controller
            .playback_queue
            .push(fixture_direct_item("second"));

        controller.update_player();

        let popup = controller
            .view
            .error_popup
            .as_ref()
            .expect("audio-output error popup");
        assert_eq!(popup.title, "Playback failed");
        assert!(popup.report.contains("audio output initialization failed"));
        assert!(popup.report.contains("ao/alsa: device is busy"));
        assert_eq!(controller.playback_phase, PlaybackPhase::Idle);
        assert!(controller.current_media.is_none());
        assert!(controller.view.playback.idle);
        assert!(!controller.view.playback_starting);
        assert_eq!(controller.view.playing_media_id, None);
        assert!(
            !controller
                .view
                .status_line
                .contains("Playback queue finished")
        );
        assert_eq!(
            controller
                .playback_queue
                .current()
                .map(|item| item.media.title.as_str()),
            Some("silent")
        );
        assert_eq!(
            state
                .lock()
                .expect("mock state")
                .played
                .iter()
                .filter_map(|input| input.title.as_deref())
                .collect::<Vec<_>>(),
            ["silent"]
        );
        assert!(
            controller
                .store
                .history(false, 10)
                .expect("history")
                .is_empty()
        );
    }

    #[test]
    fn eof_before_playback_start_is_reported_without_history_or_queue_advance() {
        let premature_eof = PlaybackEnd {
            reason: PlaybackEndReason::Eof,
            error: None,
            file_error: None,
            diagnostic: Some("cplayer: no audio or video data played".to_owned()),
        };
        let (mut controller, state, _, _) = controller_with_mock_lifecycle(
            [],
            [
                PlaybackEvent::MediaLoaded,
                PlaybackEvent::Ended(premature_eof),
            ],
        );
        controller.diagnostic_helpers_cache = Some(Vec::new());
        controller.play_queue_item(fixture_direct_item("silent"), false);
        controller
            .playback_queue
            .push(fixture_direct_item("second"));

        controller.update_player();

        let popup = controller
            .view
            .error_popup
            .as_ref()
            .expect("premature-EOF error popup");
        assert_eq!(popup.title, "Playback did not start");
        assert!(
            popup
                .report
                .contains("before reporting that audio playback started")
        );
        assert!(popup.report.contains("no audio or video data played"));
        assert_eq!(controller.playback_phase, PlaybackPhase::Idle);
        assert!(controller.current_media.is_none());
        assert!(controller.view.playback.idle);
        assert!(
            !controller
                .view
                .status_line
                .contains("Playback queue finished")
        );
        assert_eq!(
            controller
                .playback_queue
                .current()
                .map(|item| item.media.title.as_str()),
            Some("silent")
        );
        assert_eq!(
            state
                .lock()
                .expect("mock state")
                .played
                .iter()
                .filter_map(|input| input.title.as_deref())
                .collect::<Vec<_>>(),
            ["silent"]
        );
        assert!(
            controller
                .store
                .history(false, 10)
                .expect("history")
                .is_empty()
        );
    }

    #[test]
    fn explicit_stop_does_not_advance_the_queue_or_show_an_error() {
        let active = PlaybackStatus {
            idle: false,
            paused: false,
            ..PlaybackStatus::default()
        };
        let (mut controller, _state, _, events) = controller_with_mock_lifecycle(
            [active],
            [PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted],
        );
        controller.play_queue_item(fixture_direct_item("first"), false);
        controller
            .playback_queue
            .push(fixture_direct_item("second"));
        controller.update_player();
        events
            .lock()
            .expect("mock events")
            .push_back(PlaybackEvent::Ended(PlaybackEnd {
                reason: PlaybackEndReason::Stop,
                error: None,
                file_error: None,
                diagnostic: None,
            }));

        controller.update_player();

        assert_eq!(controller.playback_phase, PlaybackPhase::Idle);
        assert_eq!(controller.view.status_line, "Playback stopped");
        assert!(controller.view.error_popup.is_none());
        assert_eq!(
            controller
                .playback_queue
                .current()
                .map(|item| item.media.title.as_str()),
            Some("first")
        );
    }

    #[test]
    fn playback_process_exit_opens_diagnostics_without_advancing() {
        let (mut controller, _state, _, _) = controller_with_mock_lifecycle(
            [],
            [PlaybackEvent::ProcessExited {
                diagnostic: Some("mpv exited with status 23".to_owned()),
            }],
        );
        controller.diagnostic_helpers_cache = Some(Vec::new());
        controller.play_queue_item(fixture_direct_item("first"), false);
        controller
            .playback_queue
            .push(fixture_direct_item("second"));

        controller.update_player();

        let popup = controller
            .view
            .error_popup
            .as_ref()
            .expect("process-exit popup");
        assert_eq!(popup.title, "Playback process stopped");
        assert!(popup.report.contains("status 23"));
        assert!(controller.player.is_none());
        assert_eq!(controller.playback_phase, PlaybackPhase::Idle);
        assert_eq!(
            controller
                .playback_queue
                .current()
                .map(|item| item.media.title.as_str()),
            Some("first")
        );
        assert!(
            controller
                .store
                .history(false, 10)
                .expect("history")
                .is_empty()
        );
    }

    #[test]
    fn replacement_stop_event_does_not_cancel_the_new_media() {
        let active = PlaybackStatus {
            idle: false,
            paused: false,
            ..PlaybackStatus::default()
        };
        let (mut controller, _state, statuses, events) = controller_with_mock_lifecycle(
            [active.clone()],
            [PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted],
        );
        controller.play_queue_item(fixture_direct_item("first"), false);
        controller.update_player();
        assert_eq!(
            controller
                .view
                .playing_media_id
                .as_ref()
                .map(|media| media.external_id.as_str()),
            Some("https://media.example/first.opus")
        );
        statuses.lock().expect("mock statuses").push_back(active);
        controller.play_queue_item(fixture_direct_item("second"), false);
        assert!(controller.view.playback_starting);
        assert_eq!(controller.view.playback_start_animation_frame, 0);
        assert_eq!(controller.view.playing_media_id, None);
        events
            .lock()
            .expect("mock events")
            .push_back(PlaybackEvent::Ended(PlaybackEnd {
                reason: PlaybackEndReason::Stop,
                error: None,
                file_error: None,
                diagnostic: None,
            }));

        controller.update_player();

        assert_eq!(controller.playback_phase, PlaybackPhase::Loading);
        assert!(controller.view.playback_starting);
        assert_eq!(controller.view.playing_media_id, None);
        events
            .lock()
            .expect("mock events")
            .extend([PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted]);

        controller.update_player();

        assert_eq!(controller.playback_phase, PlaybackPhase::Playing);
        assert!(!controller.view.playback_starting);
        assert_eq!(controller.view.status_line, "Playing second");
        assert!(controller.view.error_popup.is_none());
        assert_eq!(
            controller
                .current_media
                .as_ref()
                .map(|media| media.external_id.as_str()),
            Some("https://media.example/second.opus")
        );
    }

    #[test]
    fn queue_actions_replace_status_placeholders_with_real_ordering() {
        let (mut controller, _state) =
            controller_with_mock_statuses(Vec::<crate::playback::PlaybackStatus>::new());
        controller.view.screen = Screen::Search;
        controller.direct_item = Some(DirectSourceInput {
            url: url::Url::parse("https://media.example/first.opus").expect("first URL"),
            source: SourceKind::RemoteFiles,
        });
        controller.dispatch(UiAction::AddToQueue);

        controller.direct_item = Some(DirectSourceInput {
            url: url::Url::parse("https://media.example/next.opus").expect("next URL"),
            source: SourceKind::RemoteFiles,
        });
        controller.dispatch(UiAction::PlayNext);

        controller.direct_item = Some(DirectSourceInput {
            url: url::Url::parse("https://media.example/last.opus").expect("last URL"),
            source: SourceKind::RemoteFiles,
        });
        controller.dispatch(UiAction::AddToQueue);

        assert_eq!(
            controller
                .playback_queue
                .items
                .iter()
                .map(|item| item.media.webpage_url.path())
                .collect::<Vec<_>>(),
            ["/first.opus", "/next.opus", "/last.opus"]
        );
        assert!(controller.view.status_line.contains("added to the queue"));
    }

    #[test]
    fn provider_search_routes_are_strictly_separate() {
        assert_eq!(search_route(Screen::Search), SearchRoute::YouTube);
        assert_eq!(
            search_route(Screen::YouTubeMusic),
            SearchRoute::YouTubeMusic
        );
        assert_eq!(
            search_route(Screen::TrackerMusic),
            SearchRoute::TrackerArchives
        );
        assert_eq!(search_route(Screen::Subscriptions), SearchRoute::None);
    }

    #[cfg(feature = "youtube-music")]
    #[test]
    fn youtube_and_youtube_music_keep_independent_queries_rows_and_selection() {
        let (mut controller, _state) = controller_with_mock_statuses([]);
        let youtube = subscription_video_summary();
        let mut second_youtube = youtube.clone();
        second_youtube.video_id = "aqz-KE-bpKQ".to_owned();
        second_youtube.title = "Second YouTube result".to_owned();
        controller.youtube_results = vec![
            SearchItem::Video(youtube.clone()),
            SearchItem::Video(second_youtube.clone()),
        ];
        controller.youtube_search_query = "documentary".to_owned();
        controller.view.search_query = "documentary".to_owned();
        controller.refresh_youtube_rows();
        controller.view.selected = 1;
        controller.tracker_search_query = "tracker fixture".to_owned();
        controller.tracker_results = ["first.mod", "second.xm"]
            .into_iter()
            .map(|name| TrackerItem {
                source: "Mock archive".to_owned(),
                title: name.to_owned(),
                subtitle: "mock module".to_owned(),
                webpage_url: url::Url::parse(&format!("https://mods.example/{name}"))
                    .expect("tracker page"),
                download_url: None,
                expected_format: None,
                access: TrackerAccess::MetadataOnly,
                prepared_path: None,
                insecure_transport: false,
            })
            .collect();
        controller.tracker_selected = 1;

        controller.show_screen(Screen::YouTubeMusic);
        controller.view.search_query = "mock music".to_owned();
        controller.youtube_music_search_query = "mock music".to_owned();
        let music_track = |video_id: &str, title: &str| YouTubeMusicTrack {
            video_id: video_id.to_owned(),
            title: title.to_owned(),
            artist: Some("Mock artist".to_owned()),
            duration_seconds: Some(213),
            webpage_url: url::Url::parse(&format!("https://music.youtube.com/watch?v={video_id}"))
                .expect("music URL"),
            thumbnail_url: url::Url::parse(&format!(
                "https://i.ytimg.com/vi/{video_id}/hqdefault.jpg"
            ))
            .expect("thumbnail URL"),
        };
        controller.handle_provider_response(ProviderResponse::YouTubeMusicSearch {
            generation: controller.search_generation,
            query: "mock music".to_owned(),
            result: Ok(vec![
                music_track("dQw4w9WgXcQ", "Mock track"),
                music_track("aqz-KE-bpKQ", "Second mock track"),
            ]),
        });
        assert_eq!(controller.view.rows[0].title, "Mock track");
        assert_eq!(controller.view.rows[0].source, "YouTube Music");
        controller.view.selected = 1;

        controller.show_screen(Screen::TrackerMusic);
        assert_eq!(controller.view.search_query, "tracker fixture");
        assert_eq!(controller.view.selected, 1);
        controller.show_screen(Screen::Search);
        assert_eq!(controller.view.search_query, "documentary");
        assert_eq!(controller.view.rows[1].title, second_youtube.title);
        assert_eq!(controller.view.selected, 1);
        controller.show_screen(Screen::YouTubeMusic);
        assert_eq!(controller.view.search_query, "mock music");
        assert_eq!(controller.view.rows[1].title, "Second mock track");
        assert_eq!(controller.view.selected, 1);
        controller.show_screen(Screen::TrackerMusic);
        assert_eq!(controller.view.search_query, "tracker fixture");
        assert_eq!(controller.view.rows[1].title, "second.xm");
        assert_eq!(controller.view.selected, 1);

        controller.show_screen(Screen::Search);
        let visible_youtube = controller.view.clone();
        controller.handle_provider_response(ProviderResponse::YouTubeMusicSearch {
            generation: controller.search_generation,
            query: "offscreen replacement".to_owned(),
            result: Ok(vec![music_track("9bZkp7q19f0", "Offscreen track")]),
        });
        assert_eq!(
            controller.view, visible_youtube,
            "an offscreen Music response must not mutate the visible YouTube tab"
        );
        assert_eq!(
            controller
                .store
                .youtube_music_search()
                .expect("load saved Music search")
                .expect("saved Music search")
                .query,
            "offscreen replacement"
        );
    }

    #[test]
    fn restored_tracker_and_playlist_screens_round_trip() {
        for screen in [
            Screen::Search,
            #[cfg(feature = "youtube-music")]
            Screen::YouTubeMusic,
            Screen::Subscriptions,
            Screen::Downloaded,
            Screen::History,
            Screen::Playlists,
            Screen::Statistics,
            Screen::TrackerMusic,
        ] {
            assert_eq!(
                tui_screen_from_stored(&stored_screen_from_tui(screen)),
                screen
            );
        }
        #[cfg(not(feature = "youtube-music"))]
        assert_eq!(
            tui_screen_from_stored(&StoredScreen::YouTubeMusic),
            Screen::Search
        );
    }

    #[test]
    fn output_paths_must_remain_below_the_config_root() {
        let root = Path::new("/tmp/youta-test");
        assert!(is_confined_path(
            root,
            Path::new("/tmp/youta-test/downloads/a.opus")
        ));
        assert!(!is_confined_path(root, Path::new("/tmp/other/a.opus")));
    }

    #[test]
    fn direct_input_accepts_ids_and_official_url_forms() {
        assert_eq!(
            parse_direct_youtube_input("dQw4w9WgXcQ"),
            Ok(Some(DirectVideoInput {
                video_id: "dQw4w9WgXcQ".to_owned(),
                start_seconds: None,
            }))
        );
        assert_eq!(
            parse_direct_youtube_input("youtu.be/dQw4w9WgXcQ?t=42"),
            Ok(Some(DirectVideoInput {
                video_id: "dQw4w9WgXcQ".to_owned(),
                start_seconds: Some(42),
            }))
        );
        assert_eq!(
            parse_direct_youtube_input("https://www.youtube.com/watch?v=dQw4w9WgXcQ&start=1m5s"),
            Ok(Some(DirectVideoInput {
                video_id: "dQw4w9WgXcQ".to_owned(),
                start_seconds: Some(65),
            }))
        );
    }

    #[test]
    fn direct_input_rejects_lookalikes_and_malformed_ids() {
        assert!(
            parse_direct_youtube_input("ordinary search words").is_ok_and(|value| value.is_none())
        );
        assert!(
            parse_direct_youtube_input("https://youtube.com.evil.test/watch?v=dQw4w9WgXcQ")
                .is_err()
        );
        assert!(parse_direct_youtube_input("https://youtu.be/not-an-id").is_err());
    }

    #[test]
    fn source_links_are_classified_without_changing_plain_searches() {
        let apple = parse_direct_source_input(
            "podcasts.apple.com/us/podcast/example/id123456789?i=987654321",
        )
        .expect("Apple URL should parse")
        .expect("Apple URL should be direct");
        assert_eq!(apple.source, SourceKind::ApplePodcasts);

        let remote = parse_direct_source_input("https://cdn.example.test/audio/episode.OPUS?x=1")
            .expect("remote media URL should parse")
            .expect("remote media URL should be direct");
        assert_eq!(remote.source, SourceKind::RemoteFiles);

        let bandcamp = parse_direct_source_input("https://artist.bandcamp.com/track/a-piece")
            .expect("Bandcamp URL should parse")
            .expect("Bandcamp URL should be direct");
        assert_eq!(bandcamp.source, SourceKind::Bandcamp);

        let soundstream =
            parse_direct_source_input("https://soundstream.media/playlist/roditel-skiy-chat")
                .expect("SoundStream URL should parse")
                .expect("SoundStream URL should be direct");
        assert_eq!(soundstream.source, SourceKind::SoundStream);
        assert!(requires_first_class_direct_resolution(&soundstream.source));

        let jamendo = parse_direct_source_input("https://jamen.do/t/1848357")
            .expect("Jamendo short URL should parse")
            .expect("Jamendo short URL should be direct");
        assert_eq!(jamendo.source, SourceKind::Jamendo);
        assert!(requires_first_class_direct_resolution(&jamendo.source));
        assert!(!requires_first_class_direct_resolution(&remote.source));

        assert!(
            parse_direct_source_input("ordinary YouTube search").is_ok_and(|value| value.is_none())
        );
        assert!(parse_direct_source_input("https://user@example.test/a.mp3").is_err());
    }

    #[test]
    fn local_paths_expand_home_and_scan_supported_media_only() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let home = temporary.path().join("home");
        let album = home.join("Music").join("meanna");
        std::fs::create_dir_all(album.join("disc")).expect("album directories");
        std::fs::write(album.join("01.flac"), b"fixture").expect("FLAC fixture");
        std::fs::write(album.join("disc").join("02.OPUS"), b"fixture").expect("Opus fixture");
        std::fs::write(album.join("cover.jpg"), b"fixture").expect("cover fixture");

        let input = parse_local_path_input_from("~/Music/meanna", Some(&home), temporary.path())
            .expect("home path should parse")
            .expect("home path should be direct");
        assert!(input.directory);
        assert_eq!(
            input.path,
            std::fs::canonicalize(&album).expect("canonical album")
        );

        let scanned = scan_local_media(&input.path).expect("local scan");
        assert_eq!(scanned.len(), 2);
        assert!(
            scanned
                .iter()
                .all(|item| is_supported_media_path(&item.path.to_string_lossy()))
        );
        assert!(
            parse_local_path_input_from("music to search", Some(&home), temporary.path())
                .is_ok_and(|value| value.is_none())
        );
    }

    #[test]
    fn direct_local_file_and_file_url_are_accepted() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let media = temporary.path().join("spoken episode.m4a");
        std::fs::write(&media, b"fixture").expect("media fixture");

        let by_path = parse_local_path_input_from(
            media.to_str().expect("UTF-8 test path"),
            Some(temporary.path()),
            temporary.path(),
        )
        .expect("absolute path should parse")
        .expect("absolute path should be direct");
        assert!(!by_path.directory);

        let file_url = url::Url::from_file_path(&media).expect("file URL");
        let by_url = parse_local_path_input_from(
            file_url.as_str(),
            Some(temporary.path()),
            temporary.path(),
        )
        .expect("file URL should parse")
        .expect("file URL should be direct");
        assert_eq!(by_url.path, by_path.path);
    }

    #[cfg(feature = "soundstream")]
    #[test]
    fn soundstream_conversion_never_invents_token_gated_playback() {
        use crate::providers::soundstream::{
            ResolvedSoundStream, SoundStreamClipMetadata, SoundStreamLink, SoundStreamLinkKind,
            SoundStreamMetadata,
        };

        let media = resolved_soundstream_media(ResolvedSoundStream {
            link: SoundStreamLink {
                kind: SoundStreamLinkKind::Clip,
                alias: "fixture-clip".to_owned(),
            },
            metadata: SoundStreamMetadata::Clip(SoundStreamClipMetadata {
                clip_id: 42,
                alias: "fixture-clip".to_owned(),
                title: "Fixture clip".to_owned(),
                description: Some("Public metadata".to_owned()),
                webpage_url: url::Url::parse("https://soundstream.media/clip/fixture-clip")
                    .expect("fixture page URL"),
                media_url: None,
                artwork_url: Some(
                    url::Url::parse("https://media.soundstream.media/artwork.jpg")
                        .expect("fixture artwork URL"),
                ),
                published_at: Some("2026-01-02T03:04:05Z".to_owned()),
                duration_seconds: Some(125),
                explicit: Some(false),
                playlists: Vec::new(),
            }),
        });

        assert_eq!(media.external_id, "42");
        assert!(media.playback_url.is_none());
        assert!(
            media
                .status_line
                .contains("no credential-free public media")
        );
        assert_eq!(media.duration_seconds, Some(125));
    }

    #[cfg(feature = "litres")]
    #[test]
    fn litres_conversion_prefers_explicit_full_media_and_labels_preview() {
        use crate::providers::litres::{
            LitresLink, LitresPublicMedia, LitresPublicMediaAccess, LitresPublicPage,
        };

        let page_url = url::Url::parse("https://www.litres.ru/podcast/author/fixture-123/")
            .expect("fixture LitRes URL");
        let preview_url =
            url::Url::parse("https://cdn.litres.ru/preview.mp3").expect("fixture preview URL");
        let full_url = url::Url::parse("https://cdn.litres.ru/full.mp3").expect("fixture full URL");
        let page = LitresPublicPage {
            link: LitresLink {
                item_id: 123,
                canonical_url: page_url,
            },
            title: "Fixture episode".to_owned(),
            creators: vec!["Fixture Author".to_owned()],
            description: Some("Fixture description".to_owned()),
            artwork_url: None,
            is_free: Some(true),
            price: Some("0".to_owned()),
            price_currency: Some("RUB".to_owned()),
            published_at: Some("2026-01-02".to_owned()),
            duration_seconds: Some(600),
            media: vec![
                LitresPublicMedia {
                    url: preview_url.clone(),
                    access: LitresPublicMediaAccess::Preview,
                    mime_type: Some("audio/mpeg".to_owned()),
                },
                LitresPublicMedia {
                    url: full_url.clone(),
                    access: LitresPublicMediaAccess::Full,
                    mime_type: Some("audio/mpeg".to_owned()),
                },
            ],
        };

        let full = resolved_litres_media(page.clone());
        assert_eq!(full.playback_url.as_ref(), Some(&full_url));
        assert_eq!(full.row_subtitle, "free public media");

        let mut preview_page = page;
        preview_page.media.truncate(1);
        let preview = resolved_litres_media(preview_page);
        assert_eq!(preview.playback_url.as_ref(), Some(&preview_url));
        assert_eq!(preview.row_subtitle, "preview");
        assert!(preview.status_line.contains("play the preview"));
        assert!(preview.description.contains("preview only"));
    }

    #[cfg(feature = "jamendo")]
    #[test]
    fn jamendo_conversion_preserves_license_and_official_stream_metadata() {
        use crate::providers::jamendo::JamendoTrack;

        let stream_url = url::Url::parse("https://prod-1.storage.jamendo.com/?trackid=1848357")
            .expect("fixture stream URL");
        let track = JamendoTrack {
            track_id: "1848357".to_owned(),
            title: "Fixture track".to_owned(),
            artist_id: "77".to_owned(),
            artist_name: "Fixture Artist".to_owned(),
            album_id: Some("88".to_owned()),
            album_name: Some("Fixture Album".to_owned()),
            duration_seconds: 245,
            release_date: Some("2026-01-02".to_owned()),
            license_ccurl: "https://creativecommons.org/licenses/by/4.0/".to_owned(),
            artwork_url: None,
            share_url: url::Url::parse("https://www.jamendo.com/track/1848357")
                .expect("fixture page URL"),
            short_url: Some(
                url::Url::parse("https://jamen.do/t/1848357").expect("fixture short URL"),
            ),
            audio_stream_url: stream_url.clone(),
            audiodownload_allowed: true,
            download_url: Some(
                url::Url::parse("https://prod-1.storage.jamendo.com/download/track/1848357/mp32/")
                    .expect("fixture download URL"),
            ),
            tags: vec!["ambient".to_owned()],
        };

        let media = resolved_jamendo_media(track);
        assert_eq!(media.playback_url.as_ref(), Some(&stream_url));
        assert_eq!(
            media.license,
            "https://creativecommons.org/licenses/by/4.0/"
        );
        assert!(media.description.contains("Fixture Album"));
        assert!(media.description.contains("Official download allowed: yes"));
    }

    #[cfg(feature = "jamendo")]
    #[test]
    fn jamendo_track_links_are_narrowly_parsed() {
        assert_eq!(
            jamendo_track_id(
                &url::Url::parse("https://www.jamendo.com/track/1848357/fixture")
                    .expect("fixture long URL")
            ),
            Ok("1848357".to_owned())
        );
        assert_eq!(
            jamendo_track_id(
                &url::Url::parse("https://jamen.do/t/1848357").expect("fixture short URL")
            ),
            Ok("1848357".to_owned())
        );
        assert!(
            jamendo_track_id(
                &url::Url::parse("https://jamendo.com/album/1848357")
                    .expect("fixture non-track URL")
            )
            .is_err()
        );
        assert!(
            jamendo_track_id(
                &url::Url::parse("http://www.jamendo.com/track/1848357")
                    .expect("fixture insecure URL")
            )
            .is_err()
        );
    }

    #[cfg(feature = "jamendo")]
    #[test]
    fn provider_worker_reports_missing_jamendo_configuration_without_network() {
        let (requests, request_receiver) = unbounded();
        let (response_sender, responses) = unbounded();
        let storage = tempfile::tempdir().expect("tracker storage");
        let storage_path = storage.path().to_owned();
        let worker = thread::spawn(move || {
            provider_worker(
                None,
                request_receiver,
                response_sender,
                false,
                None,
                None,
                #[cfg(feature = "youtube-music")]
                PathBuf::from("yt-dlp"),
                storage_path,
            );
        });
        let direct = DirectSourceInput {
            url: url::Url::parse("https://www.jamendo.com/track/1848357")
                .expect("fixture Jamendo URL"),
            source: SourceKind::Jamendo,
        };
        requests
            .send(ProviderRequest::ResolveFirstClass {
                generation: 7,
                direct,
            })
            .expect("worker request");
        let response = responses
            .recv_timeout(Duration::from_secs(2))
            .expect("worker response");
        match response {
            ProviderResponse::FirstClass {
                generation,
                source,
                result,
            } => {
                assert_eq!(generation, 7);
                assert_eq!(source, SourceKind::Jamendo);
                let error = result.expect_err("missing client ID must fail before network");
                assert!(error.contains("providers.jamendo_client_id"));
            }
            _ => panic!("unexpected provider response"),
        }
        requests
            .send(ProviderRequest::Shutdown)
            .expect("worker shutdown");
        worker.join().expect("worker thread");
    }
}
