//! Blocking media-provider interfaces and shared network data types.
//!
//! Provider calls deliberately remain synchronous. The terminal event loop
//! should send requests to a dedicated worker thread and receive results over a
//! channel; this keeps the dependency graph and idle CPU use smaller than an
//! asynchronous runtime would for Youta's request rate.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

#[cfg(feature = "apple-podcasts")]
pub mod apple_podcasts;
#[cfg(feature = "bandcamp")]
pub mod bandcamp;
#[cfg(feature = "bbc-radio")]
pub mod bbc;
#[cfg(feature = "dearrow")]
pub mod dearrow;
#[cfg(feature = "funkwhale")]
pub mod funkwhale;
#[cfg(feature = "invidious")]
pub mod invidious;
#[cfg(feature = "jamendo")]
pub mod jamendo;
#[cfg(feature = "litres")]
pub mod litres;
#[cfg(feature = "tracker-music")]
pub mod modarchive;
#[cfg(feature = "peertube")]
pub mod peertube;
#[cfg(feature = "radio")]
pub mod radio;
#[cfg(all(feature = "radio", feature = "wikidata"))]
pub mod radio_wikidata;
#[cfg(feature = "rss")]
pub mod rss;
#[cfg(feature = "soundstream")]
pub mod soundstream;
#[cfg(feature = "sponsorblock")]
pub mod sponsorblock;
#[cfg(feature = "tracker-music")]
pub mod tracker;
#[cfg(feature = "wikidata")]
pub mod wikidata;
#[cfg(feature = "network")]
pub mod youtube_channel_page;
#[cfg(feature = "youtube-music")]
pub mod youtube_music;
#[cfg(feature = "youtube-official")]
pub mod youtube_official;

/// Default upper bound for a provider JSON response.
pub const DEFAULT_MAX_JSON_BYTES: usize = 2 * 1024 * 1024;

/// Default end-to-end timeout for a provider request.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Features implemented by a provider.
#[allow(
    clippy::struct_excessive_bools,
    reason = "a capability matrix is clearer as named independent flags"
)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    /// The provider can search for videos.
    pub video_search: bool,
    /// The provider can search for channels independently of videos.
    pub channel_search: bool,
    /// The provider accepts page numbers for lazy result loading.
    pub pagination: bool,
    /// The provider accepts date, duration, feature, or region filters.
    pub search_filters: bool,
    /// The provider accepts an explicit result ordering.
    pub search_sorting: bool,
    /// The provider can load full video metadata after selection.
    pub video_details: bool,
    /// The provider can load a bounded list of public top-level comments.
    pub video_comments: bool,
    /// Search results or details can contain thumbnail URLs.
    pub thumbnails: bool,
}

/// Maximum number of public comments returned by one provider request.
///
/// Keeping this bound in the provider contract prevents a remote response from
/// turning the comments popup into an unbounded allocation.
pub const MAX_VIDEO_COMMENTS: usize = 20;

/// Maximum encoded byte length of one public comment identifier.
pub const MAX_VIDEO_COMMENT_ID_BYTES: usize = 256;

/// Maximum Unicode scalar count of one public comment author name.
pub const MAX_VIDEO_COMMENT_AUTHOR_CHARS: usize = 256;

/// Maximum encoded byte length of one public comment author name.
pub const MAX_VIDEO_COMMENT_AUTHOR_BYTES: usize = 1_024;

/// Maximum Unicode scalar count of one public comment body.
pub const MAX_VIDEO_COMMENT_TEXT_CHARS: usize = 10_000;

/// Maximum encoded byte length of one public comment body.
pub const MAX_VIDEO_COMMENT_TEXT_BYTES: usize = 40_000;

/// Validates one remote public-comment author for terminal-safe rendering.
///
/// Display names are single-line UI labels, so every Unicode control
/// character is rejected in addition to the documented size bounds.
///
/// # Errors
///
/// Returns [`ProviderError::InvalidResponse`] for empty, oversized, or
/// control-bearing display names.
#[cfg(any(test, feature = "invidious", feature = "youtube-official"))]
pub(crate) fn validate_video_comment_author(
    provider: &str,
    value: String,
) -> Result<String, ProviderError> {
    if value.trim().is_empty()
        || value.len() > MAX_VIDEO_COMMENT_AUTHOR_BYTES
        || value.chars().count() > MAX_VIDEO_COMMENT_AUTHOR_CHARS
        || value.chars().any(char::is_control)
    {
        return Err(ProviderError::InvalidResponse(format!(
            "{provider} returned an invalid or oversized comment author name"
        )));
    }
    Ok(value)
}

/// Normalizes and validates one remote public-comment body for terminal use.
///
/// CRLF and lone carriage returns become line feeds so multiline comments
/// remain readable across provider APIs. Every other control character,
/// including tabs, escape sequences, and NUL bytes, is rejected.
///
/// # Errors
///
/// Returns [`ProviderError::InvalidResponse`] for empty, oversized, or
/// terminal-unsafe comment bodies.
#[cfg(any(test, feature = "invidious", feature = "youtube-official"))]
pub(crate) fn normalize_video_comment_text(
    provider: &str,
    value: String,
) -> Result<String, ProviderError> {
    if value.trim().is_empty()
        || value.len() > MAX_VIDEO_COMMENT_TEXT_BYTES
        || value.chars().count() > MAX_VIDEO_COMMENT_TEXT_CHARS
    {
        return Err(ProviderError::InvalidResponse(format!(
            "{provider} returned invalid or oversized comment text"
        )));
    }
    let value = if value.contains('\r') {
        value.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        value
    };
    if value
        .chars()
        .any(|character| character != '\n' && character.is_control())
    {
        return Err(ProviderError::InvalidResponse(format!(
            "{provider} returned invalid or oversized comment text"
        )));
    }
    Ok(value)
}

/// One public top-level comment returned for a selected video.
///
/// Provider implementations must reject response values which exceed their
/// documented field bounds rather than silently retaining arbitrarily large
/// remote text. [`Provider::video_comments`] additionally guarantees that no
/// response contains more than [`MAX_VIDEO_COMMENTS`] entries.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VideoComment {
    /// Stable opaque comment identifier in the provider's namespace, bounded
    /// by [`MAX_VIDEO_COMMENT_ID_BYTES`].
    pub comment_id: String,
    /// Public author display name, bounded by
    /// [`MAX_VIDEO_COMMENT_AUTHOR_CHARS`] and
    /// [`MAX_VIDEO_COMMENT_AUTHOR_BYTES`].
    pub author_name: String,
    /// Credential-free HTTP(S) author page, when safely exposed.
    pub author_channel_url: Option<Url>,
    /// Provider-supplied plain-text comment body, bounded by
    /// [`MAX_VIDEO_COMMENT_TEXT_CHARS`] and [`MAX_VIDEO_COMMENT_TEXT_BYTES`].
    pub text: String,
    /// Public like count attached to this comment.
    pub like_count: u64,
    /// Unix publication timestamp, when valid and exposed.
    pub published_at: Option<i64>,
    /// Unix last-update timestamp, when valid and exposed.
    pub updated_at: Option<i64>,
}

/// How a provider can load subscriber statistics for video search rows.
///
/// Youta uses this distinction to batch the official `YouTube` request while
/// avoiding a fan-out request for every result returned by an Invidious
/// instance.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelStatisticsMode {
    /// The provider exposes no channel subscriber lookup.
    #[default]
    Unsupported,
    /// The provider can load one selected channel per request.
    SelectedOnly,
    /// The provider can load several channels in one request.
    Batch {
        /// Maximum number of channel identifiers accepted in one request.
        max_ids: usize,
    },
}

/// Subscriber statistics returned for one requested channel.
///
/// A missing count means that the provider hid or did not return subscriber
/// statistics. Providers still return the channel record so callers can cache
/// that negative result and avoid repeated network requests.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChannelSubscriberCount {
    /// Stable channel identifier in the provider's namespace.
    pub channel_id: String,
    /// Public subscriber count, when the channel exposes it.
    pub subscriber_count: Option<u64>,
    /// Validated public channel page returned by the same metadata lookup.
    ///
    /// Providers should retain a human-readable handle or legacy alias when
    /// their response associates it with `channel_id`. Callers must still
    /// validate this URL before exposing it to an external opener.
    #[serde(default)]
    pub webpage_url: Option<Url>,
}

/// The kind of object to search for.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchTarget {
    /// Search only for videos.
    Videos,
    /// Search only for channels.
    Channels,
}

/// Result order supported by the provider interface.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchSort {
    /// Let the service rank results for relevance.
    #[default]
    Relevance,
    /// Put videos with more views first.
    Views,
    /// Put the most recently uploaded results first.
    UploadDate,
}

/// Upload-date range used when searching for videos.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchDate {
    /// Uploaded in the last hour.
    Hour,
    /// Uploaded today.
    Today,
    /// Uploaded in the last week.
    Week,
    /// Uploaded in the last month.
    Month,
    /// Uploaded in the last year.
    Year,
}

/// Coarse duration range exposed by YouTube-compatible search APIs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchDuration {
    /// Less than four minutes.
    Short,
    /// Between four and twenty minutes.
    Medium,
    /// More than twenty minutes.
    Long,
}

/// Optional YouTube-compatible result feature.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchFeature {
    /// High-definition video.
    Hd,
    /// At least one subtitle track.
    Subtitles,
    /// A Creative Commons licence.
    CreativeCommons,
    /// Stereoscopic video.
    ThreeD,
    /// A live broadcast.
    Live,
    /// Purchased content.
    Purchased,
    /// 4K video.
    FourK,
    /// 360-degree video.
    ThreeSixty,
    /// Location metadata.
    Location,
    /// High-dynamic-range video.
    Hdr,
    /// VR180 video.
    Vr180,
}

/// Optional filters for a search request.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SearchFilters {
    /// Restrict results by upload date.
    pub date: Option<SearchDate>,
    /// Restrict results by duration.
    pub duration: Option<SearchDuration>,
    /// Require all listed media features.
    pub features: Vec<SearchFeature>,
    /// ISO 3166-1 alpha-2 region used for localized results.
    pub region: Option<String>,
}

/// A validated-on-use provider search request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SearchRequest {
    /// User-entered search text.
    pub query: String,
    /// Whether to search for videos or channels.
    pub target: SearchTarget,
    /// One-based page number.
    pub page: u32,
    /// Result ordering.
    pub sort: SearchSort,
    /// Optional result filters.
    pub filters: SearchFilters,
}

impl SearchRequest {
    /// Creates a first-page search request with relevance ordering.
    #[must_use]
    pub fn new(query: impl Into<String>, target: SearchTarget) -> Self {
        Self {
            query: query.into(),
            target,
            page: 1,
            sort: SearchSort::default(),
            filters: SearchFilters::default(),
        }
    }

    /// Checks limits that protect providers from accidental or hostile input.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] when the query, page, feature
    /// count, or region is outside the documented bounds.
    pub fn validate(&self) -> Result<(), ProviderError> {
        let query = self.query.trim();
        if query.is_empty() {
            return Err(ProviderError::InvalidRequest(
                "search query cannot be empty".to_owned(),
            ));
        }
        if query.len() > 512 {
            return Err(ProviderError::InvalidRequest(
                "search query cannot exceed 512 bytes".to_owned(),
            ));
        }
        if !(1..=10_000).contains(&self.page) {
            return Err(ProviderError::InvalidRequest(
                "search page must be between 1 and 10000".to_owned(),
            ));
        }
        if self.filters.features.len() > 16 {
            return Err(ProviderError::InvalidRequest(
                "at most 16 search features are allowed".to_owned(),
            ));
        }
        if let Some(region) = &self.filters.region
            && (region.len() != 2 || !region.bytes().all(|byte| byte.is_ascii_alphabetic()))
        {
            return Err(ProviderError::InvalidRequest(
                "search region must be a two-letter country code".to_owned(),
            ));
        }
        Ok(())
    }
}

/// A validated-on-use request for one channel's newest uploaded videos.
///
/// Providers expose numbered pages even when the remote API uses opaque
/// continuation tokens. Callers must request pages sequentially, beginning
/// with page one, so an adapter can retain those tokens without exposing
/// provider-specific pagination details.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChannelVideosRequest {
    /// Stable channel identifier in the provider's namespace.
    pub channel_id: String,
    /// One-based page number.
    pub page: u32,
}

impl ChannelVideosRequest {
    /// Creates a first-page request for a stable channel identifier.
    #[must_use]
    pub fn new(channel_id: impl Into<String>) -> Self {
        Self {
            channel_id: channel_id.into(),
            page: 1,
        }
    }

    /// Checks bounds shared by all channel-video providers.
    ///
    /// Provider adapters may apply a narrower identifier alphabet after these
    /// transport-independent checks.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] when the channel identifier
    /// is empty, has surrounding whitespace, contains control characters,
    /// exceeds 128 bytes, or when the page is outside `1..=10_000`.
    pub fn validate(&self) -> Result<(), ProviderError> {
        if self.channel_id.trim().is_empty() {
            return Err(ProviderError::InvalidRequest(
                "channel identifier cannot be empty".to_owned(),
            ));
        }
        if self.channel_id.len() > 128 {
            return Err(ProviderError::InvalidRequest(
                "channel identifier cannot exceed 128 bytes".to_owned(),
            ));
        }
        if self.channel_id.trim() != self.channel_id
            || self.channel_id.chars().any(char::is_control)
        {
            return Err(ProviderError::InvalidRequest(
                "channel identifier cannot contain surrounding whitespace or control characters"
                    .to_owned(),
            ));
        }
        if !(1..=10_000).contains(&self.page) {
            return Err(ProviderError::InvalidRequest(
                "channel video page must be between 1 and 10000".to_owned(),
            ));
        }
        Ok(())
    }
}

/// A thumbnail candidate returned by a provider.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Thumbnail {
    /// Remote image URL.
    pub url: Url,
    /// Provider quality label, when available.
    pub quality: Option<String>,
    /// Pixel width, when supplied by the provider.
    pub width: Option<u32>,
    /// Pixel height, when supplied by the provider.
    pub height: Option<u32>,
}

/// Display orientation derived from provider-reported video dimensions.
///
/// Thumbnail proportions are deliberately not used because providers can crop
/// or letterbox thumbnails independently of the encoded video. Unknown and
/// future serialized values safely fall back to [`Self::Unknown`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoOrientation {
    /// Width is greater than height.
    Horizontal,
    /// Height is greater than width.
    Vertical,
    /// Width and height are equal.
    Square,
    /// The provider did not expose usable dimensions.
    #[default]
    #[serde(other)]
    Unknown,
}

impl VideoOrientation {
    /// Classifies non-zero pixel dimensions.
    ///
    /// Zero dimensions are treated as unknown because they cannot describe a
    /// decodable video frame.
    #[must_use]
    pub const fn from_dimensions(width: u32, height: u32) -> Self {
        if width == 0 || height == 0 {
            Self::Unknown
        } else if width > height {
            Self::Horizontal
        } else if height > width {
            Self::Vertical
        } else {
            Self::Square
        }
    }
}

/// Compact video metadata suitable for a search-result list.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VideoSummary {
    /// Stable video identifier in the provider's namespace.
    pub video_id: String,
    /// Original provider title.
    pub title: String,
    /// Channel display name.
    pub channel_name: String,
    /// Stable channel identifier in the provider's namespace.
    pub channel_id: String,
    /// Plain-text description excerpt.
    pub description: String,
    /// Duration in seconds.
    pub duration_seconds: Option<u64>,
    /// View count, when exposed.
    pub view_count: Option<u64>,
    /// Unix publication timestamp, when exposed.
    pub published_at: Option<i64>,
    /// Human-readable publication age supplied by the provider.
    pub published_text: Option<String>,
    /// Whether this is currently live.
    pub live: bool,
    /// Provider-derived display orientation, when dimensions are available.
    #[serde(default)]
    pub orientation: VideoOrientation,
    /// Available thumbnails, in provider order.
    pub thumbnails: Vec<Thumbnail>,
    /// Canonical browser page advertised by the provider, when available.
    ///
    /// Provider adapters validate remote values as credential-free HTTP(S)
    /// URLs before exposing them. Consumers must still treat the URL as
    /// untrusted data when passing it to an external process.
    pub webpage_url: Option<Url>,
    /// Direct or instance-proxied playback URL advertised by the provider.
    ///
    /// This is only a locator. Fetching and decoder selection belong to the
    /// playback worker, and consumers must not interpret its path as trusted
    /// local input.
    pub stream_url: Option<Url>,
}

/// Compact channel metadata suitable for a channel-search list.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChannelSummary {
    /// Stable channel identifier in the provider's namespace.
    pub channel_id: String,
    /// Channel display name.
    pub name: String,
    /// Plain-text channel description.
    pub description: String,
    /// Subscriber count, when exposed.
    pub subscriber_count: Option<u64>,
    /// Public video count, when exposed.
    pub video_count: Option<u64>,
    /// Channel creation time as Unix seconds, when exposed.
    #[serde(default)]
    pub created_at: Option<i64>,
    /// Whether `YouTube` generated the channel automatically.
    pub auto_generated: bool,
    /// Available channel avatars, in provider order.
    pub thumbnails: Vec<Thumbnail>,
    /// Canonical browser page advertised by the provider, when available.
    pub webpage_url: Option<Url>,
}

/// Compact RSS/Atom episode metadata suitable for subscription snapshots.
///
/// The direct enclosure is transient and cleared before a restart snapshot is
/// persisted. Feed and episode pages, descriptive metadata, and the
/// feed-scoped identifier remain sufficient for an immediate stale view while
/// a fresh feed request resolves the current enclosure.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PodcastEpisodeSummary {
    /// Exact normalized feed URL that scopes the episode GUID.
    pub feed_url: Url,
    /// Stable feed-supplied or deterministically generated episode identifier.
    pub episode_id: String,
    /// Feed title displayed in source-neutral contexts.
    pub feed_title: String,
    /// Original publisher title.
    pub title: String,
    /// Publisher or episode authors in source order.
    pub authors: Vec<String>,
    /// Episode description.
    pub description: String,
    /// Episode language tag, when supplied.
    pub language: Option<String>,
    /// Publisher categories in source order.
    pub categories: Vec<String>,
    /// Duration in whole seconds, when supplied.
    pub duration_seconds: Option<u64>,
    /// Unix publication timestamp, when supplied.
    pub published_at: Option<i64>,
    /// Canonical episode page, when supplied.
    pub webpage_url: Option<Url>,
    /// Canonical feed publisher page, when supplied.
    pub feed_webpage_url: Option<Url>,
    /// Episode or feed artwork.
    pub artwork_url: Option<Url>,
    /// Current playable enclosure. Cleared in restart snapshots.
    pub stream_url: Option<Url>,
    /// Advertised MIME type for the selected enclosure.
    pub stream_mime_type: Option<String>,
    /// Advertised byte length for the selected enclosure.
    pub stream_byte_length: Option<u64>,
}

/// Full public metadata loaded for one selected channel.
///
/// The compact [`ChannelSummary`] remains stable for search results and old
/// on-disk cache rows. Fields that are either provider-specific or more
/// expensive to obtain live here so callers can request them lazily.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChannelDetails {
    /// Compact identity, description, counts, artwork, and canonical webpage.
    pub summary: ChannelSummary,
    /// Aggregate public view count, when exposed.
    #[serde(default)]
    pub total_view_count: Option<u64>,
    /// Provider-supplied country code or human-readable country label.
    #[serde(default)]
    pub country: Option<String>,
    /// Public websites and social profiles advertised by the channel.
    #[serde(default)]
    pub external_links: Vec<ChannelExternalLink>,
    /// Whether additional external links were omitted by a safety bound.
    #[serde(default)]
    pub external_links_truncated: bool,
}

/// One public website or social profile advertised by a channel.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChannelExternalLink {
    /// Channel-supplied label, bounded and normalized for terminal display.
    pub label: String,
    /// Direct credential-free HTTP(S) target.
    pub url: Url,
    /// Recognized destination family used for stable icons and colors.
    pub kind: ChannelExternalLinkKind,
}

/// Recognized family of a channel's public external link.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelExternalLinkKind {
    /// A general website or an unrecognized HTTP(S) host.
    #[default]
    Website,
    /// A Telegram channel, group, bot, or public profile.
    Telegram,
    /// A Facebook page or profile.
    Facebook,
    /// An X or legacy Twitter profile.
    XTwitter,
    /// A `TikTok` profile.
    TikTok,
    /// An Instagram profile.
    Instagram,
    /// Another `YouTube` channel or page.
    YouTube,
}

/// One entry in a provider search page.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SearchItem {
    /// A video result.
    Video(VideoSummary),
    /// A channel result.
    Channel(ChannelSummary),
    /// An RSS, Atom, or JSON Feed podcast episode.
    ///
    /// The summary is boxed so this metadata-rich variant does not inflate
    /// every compact video and channel result in a search page.
    PodcastEpisode(Box<PodcastEpisodeSummary>),
}

/// A page of search results.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SearchPage {
    /// One-based page number returned.
    pub page: u32,
    /// Parsed results.
    pub items: Vec<SearchItem>,
    /// Next sequential page to request, or `None` at the end.
    ///
    /// A page can contain no playable items and still provide a continuation
    /// when the remote collection contains private or unavailable entries.
    pub next_page: Option<u32>,
}

/// Full metadata loaded after selecting a video.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoDetails {
    /// Stable video identifier in the provider's namespace.
    pub video_id: String,
    /// Original video title.
    pub title: String,
    /// Channel display name.
    pub channel_name: String,
    /// Stable channel identifier in the provider's namespace.
    pub channel_id: String,
    /// Plain-text video description.
    pub description: String,
    /// Duration in seconds.
    pub duration_seconds: Option<u64>,
    /// View count, when exposed.
    pub view_count: Option<u64>,
    /// Like count, when exposed.
    pub like_count: Option<u64>,
    /// Public top-level comment count, when exposed.
    #[serde(default)]
    pub comment_count: Option<u64>,
    /// Unix publication timestamp, when exposed.
    pub published_at: Option<i64>,
    /// Human-readable publication age supplied by the provider.
    pub published_text: Option<String>,
    /// Provider-supplied licence label, when available.
    ///
    /// Invidious does not guarantee this field, so upload workflows must verify
    /// the licence through another metadata source before publishing.
    pub license: Option<String>,
    /// Provider rating, when available.
    pub rating: Option<f64>,
    /// Whether ratings are enabled for the video.
    pub ratings_allowed: Option<bool>,
    /// Whether this is currently live.
    pub live: bool,
    /// Provider-derived display orientation, when dimensions are available.
    #[serde(default)]
    pub orientation: VideoOrientation,
    /// Video keywords.
    pub keywords: Vec<String>,
    /// Available thumbnails, in provider order.
    pub thumbnails: Vec<Thumbnail>,
    /// Canonical browser page advertised by the provider, when available.
    ///
    /// Provider adapters validate remote values as credential-free HTTP(S)
    /// URLs before exposing them.
    pub webpage_url: Option<Url>,
    /// Direct or instance-proxied playback URL advertised by the provider.
    ///
    /// The URL remains untrusted input and must be passed to a playback backend
    /// as one argument, never interpolated into a shell command.
    pub stream_url: Option<Url>,
}

/// Failure returned by a provider.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// A configured service URL is not a safe HTTP(S) base URL.
    #[error("invalid provider base URL: {0}")]
    InvalidBaseUrl(String),
    /// A caller supplied an invalid search or media identifier.
    #[error("invalid provider request: {0}")]
    InvalidRequest(String),
    /// A network, DNS, TLS, or timeout operation failed.
    #[error("provider transport failed: {0}")]
    Transport(String),
    /// The server returned an unsuccessful HTTP status.
    #[error("provider returned HTTP status {0}")]
    HttpStatus(u16),
    /// The server returned a structured service error.
    #[error("provider returned HTTP status {status} ({reason}): {message}")]
    Service {
        /// HTTP status returned by the service.
        status: u16,
        /// Stable machine-readable service reason.
        reason: String,
        /// Bounded, sanitized service message.
        message: String,
    },
    /// The response exceeded the configured memory bound.
    #[error("provider response exceeded the {limit}-byte limit")]
    ResponseTooLarge {
        /// Configured maximum response size.
        limit: usize,
    },
    /// The response was JSON but did not match the expected API schema.
    #[error("invalid provider response: {0}")]
    InvalidResponse(String),
    /// The selected provider does not implement an operation.
    #[error("provider operation is not supported")]
    Unsupported,
}

/// Failure while choosing or constructing a configured `YouTube` provider.
#[derive(Debug, Error)]
pub enum YouTubeProviderConfigurationError {
    /// The selected adapter was intentionally omitted at compile time.
    #[error("{provider} is configured, but this build omits the `{feature}` feature")]
    FeatureDisabled {
        /// Human-readable adapter name.
        provider: &'static str,
        /// Cargo feature that enables the adapter.
        feature: &'static str,
    },
    /// A configured value was rejected by its provider constructor.
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

/// Blocking provider API intended for ownership by a worker thread.
///
/// Implementations must be `Send + Sync`, but calls are not expected to be
/// non-blocking. Never invoke [`Provider::search`] or
/// [`Provider::channel_videos`], [`Provider::channel_details`],
/// [`Provider::video_details`], [`Provider::video_comments`], or
/// [`Provider::channel_subscriber_counts`] from the terminal rendering/event
/// thread.
pub trait Provider: Send + Sync {
    /// Stable provider identifier used in configuration and diagnostics.
    fn id(&self) -> &'static str;

    /// Human-readable provider name.
    fn display_name(&self) -> &'static str;

    /// Operations supported by this provider.
    fn capabilities(&self) -> ProviderCapabilities;

    /// Describes whether channel statistics are unsupported, selected-only,
    /// or batchable.
    ///
    /// The default keeps existing and non-YouTube providers source-compatible
    /// without claiming an operation they cannot perform.
    fn channel_statistics_mode(&self) -> ChannelStatisticsMode {
        ChannelStatisticsMode::Unsupported
    }

    /// Performs one blocking page fetch.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] for invalid input, transport failure, an
    /// unsuccessful status, or an invalid bounded response.
    fn search(&self, request: &SearchRequest) -> Result<SearchPage, ProviderError>;

    /// Loads one sequential page of videos uploaded by a channel.
    ///
    /// Successful implementations return only [`SearchItem::Video`] entries.
    /// Providers backed by opaque continuation tokens may reject a page when
    /// its preceding page has not been loaded by this provider instance.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Unsupported`] by default after validating the
    /// request. Implementations can also return a provider error for an
    /// invalid identifier, missing pagination context, transport failure,
    /// unsuccessful status, or malformed bounded response.
    fn channel_videos(&self, request: &ChannelVideosRequest) -> Result<SearchPage, ProviderError> {
        request.validate()?;
        Err(ProviderError::Unsupported)
    }

    /// Loads full public metadata for one exact channel identifier.
    ///
    /// The returned [`ChannelSummary`] contains the provider's current name,
    /// description, public subscriber and video counts, thumbnails, and
    /// canonical webpage when those fields are exposed. This selected-only
    /// lookup is separate from [`Provider::channel_subscriber_counts`] so
    /// callers can retain the latter's low-bandwidth batching behavior for
    /// search rows.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Unsupported`] by default. Implementations can
    /// also return a provider error for an invalid identifier, transport
    /// failure, an unsuccessful status, a missing channel, or malformed
    /// bounded data.
    fn channel_details(&self, _channel_id: &str) -> Result<ChannelSummary, ProviderError> {
        Err(ProviderError::Unsupported)
    }

    /// Loads full public metadata for one exact channel identifier.
    ///
    /// The default wraps [`Provider::channel_details`] and leaves optional
    /// aggregate views, country, and external links empty. Adapters that expose
    /// those fields should override this method without issuing a second
    /// channel lookup.
    ///
    /// # Errors
    ///
    /// Returns the same provider errors as [`Provider::channel_details`].
    fn full_channel_details(&self, channel_id: &str) -> Result<ChannelDetails, ProviderError> {
        self.channel_details(channel_id)
            .map(|summary| ChannelDetails {
                summary,
                total_view_count: None,
                country: None,
                external_links: Vec::new(),
                external_links_truncated: false,
            })
    }

    /// Performs one blocking video-details fetch.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] for an invalid identifier, transport
    /// failure, an unsuccessful status, or an invalid bounded response.
    fn video_details(&self, video_id: &str) -> Result<VideoDetails, ProviderError>;

    /// Loads at most [`MAX_VIDEO_COMMENTS`] public top-level comments.
    ///
    /// Providers should use their service's relevance or "top comments"
    /// ordering and return plain text. The default keeps providers without a
    /// public-comments API source-compatible and explicitly unsupported.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Unsupported`] by default. Implementations can
    /// also return a provider error for an invalid video identifier, transport
    /// failure, an unsuccessful status, or malformed or oversized data.
    fn video_comments(&self, _video_id: &str) -> Result<Vec<VideoComment>, ProviderError> {
        Err(ProviderError::Unsupported)
    }

    /// Loads public subscriber counts for the requested channel identifiers.
    ///
    /// Successful implementations return one record per input identifier in
    /// the same order, including records whose count is hidden or unavailable.
    /// An empty input returns an empty result without network access.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Unsupported`] by default. Implementations can
    /// also return a provider error for invalid identifiers, an oversized
    /// batch, transport failure, an unsuccessful status, or malformed data.
    fn channel_subscriber_counts(
        &self,
        channel_ids: &[String],
    ) -> Result<Vec<ChannelSubscriberCount>, ProviderError> {
        if channel_ids.is_empty() {
            Ok(Vec::new())
        } else {
            Err(ProviderError::Unsupported)
        }
    }
}

/// Constructs the official `YouTube` Data API metadata provider.
///
/// # Errors
///
/// Returns an error when the key is invalid or this binary omits the
/// `youtube-official` Cargo feature. The key is never included in the error.
pub fn official_youtube_provider(
    api_key: String,
) -> Result<Box<dyn Provider>, YouTubeProviderConfigurationError> {
    #[cfg(feature = "youtube-official")]
    {
        let provider = youtube_official::YouTubeOfficialProvider::new(api_key)?;
        Ok(Box::new(provider))
    }
    #[cfg(not(feature = "youtube-official"))]
    {
        drop(api_key);
        Err(YouTubeProviderConfigurationError::FeatureDisabled {
            provider: "the official YouTube Data API",
            feature: "youtube-official",
        })
    }
}

/// Constructs a configurable Invidious metadata provider.
///
/// # Errors
///
/// Returns an error when the URL is not a safe base URL or this binary omits
/// the `invidious` Cargo feature.
pub fn invidious_youtube_provider(
    base_url: Url,
) -> Result<Box<dyn Provider>, YouTubeProviderConfigurationError> {
    #[cfg(feature = "invidious")]
    {
        let provider = invidious::InvidiousProvider::new(base_url)?;
        Ok(Box::new(provider))
    }
    #[cfg(not(feature = "invidious"))]
    {
        drop(base_url);
        Err(YouTubeProviderConfigurationError::FeatureDisabled {
            provider: "Invidious",
            feature: "invidious",
        })
    }
}

/// Chooses the configured `YouTube` search/details provider.
///
/// In `auto` mode an official API key is preferred when that adapter is
/// compiled in, followed by a configured Invidious instance. Playback remains
/// independent and can continue to use `yt-dlp` and `mpv`.
///
/// # Errors
///
/// Returns an error when the selected adapter was omitted from the build or a
/// configured credential/URL is invalid.
pub fn configured_youtube_provider(
    config: &crate::config::ProviderConfig,
) -> Result<Option<Box<dyn Provider>>, YouTubeProviderConfigurationError> {
    use crate::config::YouTubeBackend;

    match config.youtube_backend {
        YouTubeBackend::Official => config
            .youtube_api_key
            .as_ref()
            .map(|api_key| official_youtube_provider(api_key.clone()))
            .transpose(),
        YouTubeBackend::Invidious => config
            .invidious_base_url
            .as_ref()
            .map(|base_url| invidious_youtube_provider(base_url.clone()))
            .transpose(),
        YouTubeBackend::Auto => {
            #[cfg(feature = "youtube-official")]
            if let Some(api_key) = config.youtube_api_key.as_ref() {
                return official_youtube_provider(api_key.clone()).map(Some);
            }
            #[cfg(feature = "invidious")]
            if let Some(base_url) = config.invidious_base_url.as_ref() {
                return invidious_youtube_provider(base_url.clone()).map(Some);
            }

            if config.youtube_api_key.is_some() {
                return Err(YouTubeProviderConfigurationError::FeatureDisabled {
                    provider: "the official YouTube Data API",
                    feature: "youtube-official",
                });
            }
            if config.invidious_base_url.is_some() {
                return Err(YouTubeProviderConfigurationError::FeatureDisabled {
                    provider: "Invidious",
                    feature: "invidious",
                });
            }
            Ok(None)
        }
    }
}

/// Validates and normalizes a configurable provider base URL.
///
/// HTTP is accepted for self-hosted instances on trusted networks. Credentials,
/// queries, and fragments are rejected so endpoint construction cannot inherit
/// secrets or ambiguous parameters.
///
/// # Errors
///
/// Returns [`ProviderError::InvalidBaseUrl`] when the URL is not a
/// credential-free HTTP(S) base URL without a query or fragment.
pub fn validate_base_url(mut url: Url) -> Result<Url, ProviderError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ProviderError::InvalidBaseUrl(
            "scheme must be http or https".to_owned(),
        ));
    }
    if url.host_str().is_none() || url.cannot_be_a_base() {
        return Err(ProviderError::InvalidBaseUrl(
            "URL must contain a host".to_owned(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ProviderError::InvalidBaseUrl(
            "embedded credentials are not allowed".to_owned(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ProviderError::InvalidBaseUrl(
            "query strings and fragments are not allowed".to_owned(),
        ));
    }
    if !url.path().ends_with('/') {
        let normalized = format!("{}/", url.path());
        url.set_path(&normalized);
    }
    Ok(url)
}

/// Checks a `YouTube` video identifier before putting it into a path or query.
///
/// # Errors
///
/// Returns [`ProviderError::InvalidRequest`] unless the value is an
/// eleven-character URL-safe `YouTube` video identifier.
pub fn validate_youtube_video_id(video_id: &str) -> Result<(), ProviderError> {
    if video_id.len() != 11
        || !video_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ProviderError::InvalidRequest(
            "YouTube video ID must contain 11 URL-safe characters".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(any(
    feature = "bbc-radio",
    feature = "dearrow",
    feature = "funkwhale",
    feature = "invidious",
    feature = "jamendo",
    feature = "litres",
    feature = "peertube",
    feature = "radio",
    feature = "sponsorblock",
    feature = "tracker-music",
    feature = "wikidata",
    feature = "youtube-official"
))]
pub(crate) fn provider_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
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

#[cfg(any(
    feature = "dearrow",
    feature = "funkwhale",
    feature = "invidious",
    feature = "jamendo",
    feature = "peertube",
    feature = "radio",
    feature = "soundstream",
    feature = "sponsorblock",
    feature = "wikidata"
))]
pub(crate) fn get_bounded_json<T: serde::de::DeserializeOwned>(
    agent: &ureq::Agent,
    url: &Url,
    limit: usize,
) -> Result<T, ProviderError> {
    if limit == 0 {
        return Err(ProviderError::InvalidRequest(
            "JSON response limit must be greater than zero".to_owned(),
        ));
    }

    let mut response = agent
        .get(url.as_str())
        .header("Accept", "application/json")
        .call()
        .map_err(map_ureq_error)?;

    if response
        .body()
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(ProviderError::ResponseTooLarge { limit });
    }

    let bytes = response
        .body_mut()
        .with_config()
        .limit(u64::try_from(limit.saturating_add(1)).unwrap_or(u64::MAX))
        .read_to_vec()
        .map_err(|error| match error {
            ureq::Error::BodyExceedsLimit(_) => ProviderError::ResponseTooLarge { limit },
            other => ProviderError::Transport(other.to_string()),
        })?;
    if bytes.len() > limit {
        return Err(ProviderError::ResponseTooLarge { limit });
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))
}

#[cfg(any(
    feature = "dearrow",
    feature = "funkwhale",
    feature = "invidious",
    feature = "jamendo",
    feature = "peertube",
    feature = "radio",
    feature = "soundstream",
    feature = "sponsorblock",
    feature = "wikidata"
))]
fn map_ureq_error(error: ureq::Error) -> ProviderError {
    match error {
        ureq::Error::StatusCode(code) => ProviderError::HttpStatus(code),
        ureq::Error::BodyExceedsLimit(limit) => ProviderError::ResponseTooLarge {
            limit: usize::try_from(limit).unwrap_or(usize::MAX),
        },
        other => ProviderError::Transport(other.to_string()),
    }
}

#[cfg(any(feature = "funkwhale", feature = "invidious", feature = "peertube"))]
pub(crate) fn resolve_http_url(base_url: &Url, raw: &str) -> Result<Url, ProviderError> {
    let url = if raw.starts_with("//") {
        Url::parse(&format!("{}:{raw}", base_url.scheme()))
    } else {
        base_url.join(raw)
    }
    .map_err(|error| ProviderError::InvalidResponse(format!("invalid remote URL: {error}")))?;

    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(ProviderError::InvalidResponse(
            "remote URL must be credential-free HTTP(S)".to_owned(),
        ));
    }
    Ok(url)
}

/// Parses the RFC 3339 subset returned by media providers into Unix seconds.
///
/// Fractional seconds and numeric timezone offsets are accepted. Invalid
/// calendar dates return `None` rather than being normalized.
#[must_use]
#[cfg(any(
    feature = "funkwhale",
    feature = "peertube",
    feature = "youtube-official"
))]
pub(crate) fn parse_rfc3339_epoch(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(b'T' | b't' | b' '))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    let year = i64::from(parse_decimal(bytes.get(0..4)?)?);
    let month = parse_decimal(bytes.get(5..7)?)?;
    let day = parse_decimal(bytes.get(8..10)?)?;
    let hour = parse_decimal(bytes.get(11..13)?)?;
    let minute = parse_decimal(bytes.get(14..16)?)?;
    let second = parse_decimal(bytes.get(17..19)?)?;
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    let mut zone_index = 19;
    if bytes.get(zone_index) == Some(&b'.') {
        zone_index += 1;
        let fraction_start = zone_index;
        while bytes.get(zone_index).is_some_and(u8::is_ascii_digit) {
            zone_index += 1;
        }
        if zone_index == fraction_start {
            return None;
        }
    }
    let offset = match bytes.get(zone_index) {
        Some(b'Z' | b'z') if zone_index + 1 == bytes.len() => 0_i64,
        Some(sign @ (b'+' | b'-'))
            if bytes.len() == zone_index + 6 && bytes.get(zone_index + 3) == Some(&b':') =>
        {
            let offset_hour = parse_decimal(bytes.get(zone_index + 1..zone_index + 3)?)?;
            let offset_minute = parse_decimal(bytes.get(zone_index + 4..zone_index + 6)?)?;
            if offset_hour > 23 || offset_minute > 59 {
                return None;
            }
            let seconds = i64::from(offset_hour * 3600 + offset_minute * 60);
            if *sign == b'+' { seconds } else { -seconds }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    days.checked_mul(86_400)?
        .checked_add(i64::from(hour * 3600 + minute * 60 + second))?
        .checked_sub(offset)
}

#[cfg(any(
    feature = "funkwhale",
    feature = "peertube",
    feature = "youtube-official"
))]
fn parse_decimal(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0_u32, |value, byte| {
        byte.is_ascii_digit()
            .then(|| value.checked_mul(10)?.checked_add(u32::from(byte - b'0')))
            .flatten()
    })
}

#[cfg(any(
    feature = "funkwhale",
    feature = "peertube",
    feature = "youtube-official"
))]
const fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.rem_euclid(4) == 0
            && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0) =>
        {
            29
        }
        2 => 28,
        _ => 0,
    }
}

#[cfg(any(
    feature = "funkwhale",
    feature = "peertube",
    feature = "youtube-official"
))]
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MinimalProvider;

    impl Provider for MinimalProvider {
        fn id(&self) -> &'static str {
            "minimal"
        }

        fn display_name(&self) -> &'static str {
            "Minimal"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        fn search(&self, _request: &SearchRequest) -> Result<SearchPage, ProviderError> {
            Err(ProviderError::Unsupported)
        }

        fn video_details(&self, _video_id: &str) -> Result<VideoDetails, ProviderError> {
            Err(ProviderError::Unsupported)
        }
    }

    #[test]
    fn full_channel_details_are_unsupported_by_default() {
        assert!(matches!(
            MinimalProvider.full_channel_details("channel"),
            Err(ProviderError::Unsupported)
        ));
    }

    #[test]
    fn public_video_comments_are_bounded_and_unsupported_by_default() {
        assert_eq!(MAX_VIDEO_COMMENTS, 20);
        assert!(!MinimalProvider.capabilities().video_comments);
        assert!(matches!(
            MinimalProvider.video_comments("video"),
            Err(ProviderError::Unsupported)
        ));
    }

    #[test]
    fn public_video_comment_text_preserves_lines_but_rejects_terminal_controls() {
        assert_eq!(
            normalize_video_comment_text(
                "Fixture",
                "first line\r\nsecond line\rlast line".to_owned(),
            )
            .expect("portable multiline comment"),
            "first line\nsecond line\nlast line"
        );
        for invalid in [
            "tab\tinjected",
            "escape\u{1b}[31mred",
            "nul\0byte",
            "backspace\u{8}text",
        ] {
            assert!(matches!(
                normalize_video_comment_text("Fixture", invalid.to_owned()),
                Err(ProviderError::InvalidResponse(_))
            ));
        }
        assert!(matches!(
            normalize_video_comment_text(
                "Fixture",
                "\r\n".repeat((MAX_VIDEO_COMMENT_TEXT_CHARS / 2) + 1),
            ),
            Err(ProviderError::InvalidResponse(_))
        ));
    }

    #[test]
    fn public_video_comment_author_rejects_all_controls() {
        assert_eq!(
            validate_video_comment_author("Fixture", "Author name".to_owned())
                .expect("plain author"),
            "Author name"
        );
        for invalid in [
            "Author\nName",
            "Author\tName",
            "Author\u{1b}[2J",
            "Author\0",
        ] {
            assert!(matches!(
                validate_video_comment_author("Fixture", invalid.to_owned()),
                Err(ProviderError::InvalidResponse(_))
            ));
        }
    }

    #[test]
    fn base_url_is_normalized_without_losing_a_subpath() {
        let url = validate_base_url(
            Url::parse("http://localhost:3000/invidious").expect("test URL should parse"),
        )
        .expect("HTTP self-hosted URL should be accepted");

        assert_eq!(url.as_str(), "http://localhost:3000/invidious/");
        assert_eq!(
            url.join("api/v1/search")
                .expect("endpoint should join")
                .as_str(),
            "http://localhost:3000/invidious/api/v1/search"
        );
    }

    #[test]
    fn base_url_rejects_credentials_query_and_non_http_schemes() {
        for raw in [
            "https://user:secret@example.test/",
            "https://example.test/?instance=other",
            "file:///tmp/invidious",
        ] {
            let error = validate_base_url(Url::parse(raw).expect("test URL should parse"));
            assert!(matches!(error, Err(ProviderError::InvalidBaseUrl(_))));
        }
    }

    #[test]
    fn search_request_checks_page_query_and_region() {
        let mut request = SearchRequest::new("music", SearchTarget::Videos);
        request.page = 0;
        assert!(matches!(
            request.validate(),
            Err(ProviderError::InvalidRequest(_))
        ));

        request.page = 1;
        request.filters.region = Some("USA".to_owned());
        assert!(matches!(
            request.validate(),
            Err(ProviderError::InvalidRequest(_))
        ));

        request.filters.region = Some("ge".to_owned());
        assert!(request.validate().is_ok());
    }

    #[test]
    fn channel_videos_request_checks_identifier_and_page_bounds() {
        let request = ChannelVideosRequest::new("UC_x5XG1OV2P6uZZ5FSM9Ttw");
        assert!(request.validate().is_ok());
        assert_eq!(request.page, 1);

        for channel_id in [
            "",
            " \t",
            " UC_x5XG1OV2P6uZZ5FSM9Ttw",
            "UC_x5XG1OV2P6uZZ5FSM9Ttw ",
            "UC_fixture\ninjected",
        ] {
            assert!(
                matches!(
                    ChannelVideosRequest::new(channel_id).validate(),
                    Err(ProviderError::InvalidRequest(_))
                ),
                "{channel_id:?}"
            );
        }
        assert!(matches!(
            ChannelVideosRequest::new("x".repeat(129)).validate(),
            Err(ProviderError::InvalidRequest(_))
        ));

        let mut request = ChannelVideosRequest::new("UC_fixture");
        request.page = 0;
        assert!(matches!(
            request.validate(),
            Err(ProviderError::InvalidRequest(_))
        ));
        request.page = 10_001;
        assert!(matches!(
            request.validate(),
            Err(ProviderError::InvalidRequest(_))
        ));
    }

    #[test]
    fn youtube_video_id_validation_is_strict() {
        assert!(validate_youtube_video_id("dQw4w9WgXcQ").is_ok());
        for invalid in ["short", "dQw4w9WgXc!", "dQw4w9WgXcQsuffix"] {
            assert!(matches!(
                validate_youtube_video_id(invalid),
                Err(ProviderError::InvalidRequest(_))
            ));
        }
    }

    #[test]
    fn video_orientation_classifies_dimensions_and_defaults_during_deserialization() {
        assert_eq!(
            VideoOrientation::from_dimensions(1_920, 1_080),
            VideoOrientation::Horizontal
        );
        assert_eq!(
            VideoOrientation::from_dimensions(1_080, 1_920),
            VideoOrientation::Vertical
        );
        assert_eq!(
            VideoOrientation::from_dimensions(1_080, 1_080),
            VideoOrientation::Square
        );
        assert_eq!(
            VideoOrientation::from_dimensions(0, 1_080),
            VideoOrientation::Unknown
        );
        assert_eq!(
            serde_json::from_str::<VideoOrientation>(r#""future_shape""#)
                .expect("future variants should remain readable"),
            VideoOrientation::Unknown
        );

        let summary: VideoSummary = serde_json::from_str(
            r#"{
                "video_id":"dQw4w9WgXcQ",
                "title":"Legacy",
                "channel_name":"Channel",
                "channel_id":"UC_fixture",
                "description":"",
                "duration_seconds":null,
                "view_count":null,
                "published_at":null,
                "published_text":null,
                "live":false,
                "thumbnails":[],
                "webpage_url":null,
                "stream_url":null
            }"#,
        )
        .expect("saved summaries from before orientation support should remain readable");
        assert_eq!(summary.orientation, VideoOrientation::Unknown);
    }

    #[test]
    fn unconfigured_youtube_provider_is_a_normal_none_result() {
        let providers = crate::config::ProviderConfig::default();
        assert!(
            configured_youtube_provider(&providers)
                .expect("empty automatic selection")
                .is_none()
        );
    }

    #[cfg(all(feature = "youtube-official", feature = "invidious"))]
    #[test]
    fn automatic_youtube_selection_prefers_the_official_key() {
        let providers = crate::config::ProviderConfig {
            youtube_api_key: Some("AIzaSyFixture_key_123456789012345678".to_owned()),
            invidious_base_url: Some(
                Url::parse("https://invidious.example.test/").expect("fixture URL"),
            ),
            ..crate::config::ProviderConfig::default()
        };

        let provider = configured_youtube_provider(&providers)
            .expect("configured provider")
            .expect("one provider");
        assert_eq!(provider.id(), "youtube-official");
    }

    #[cfg(feature = "invidious")]
    #[test]
    fn explicit_invidious_selection_uses_the_configured_instance() {
        let mut providers = crate::config::ProviderConfig {
            youtube_backend: crate::config::YouTubeBackend::Invidious,
            ..crate::config::ProviderConfig::default()
        };
        providers.invidious_base_url =
            Some(Url::parse("https://invidious.example.test/").expect("fixture URL"));

        let provider = configured_youtube_provider(&providers)
            .expect("configured provider")
            .expect("one provider");
        assert_eq!(provider.id(), "invidious");
    }

    #[cfg(not(feature = "youtube-official"))]
    #[test]
    fn explicit_official_selection_reports_an_omitted_build_feature() {
        let providers = crate::config::ProviderConfig {
            youtube_backend: crate::config::YouTubeBackend::Official,
            youtube_api_key: Some("AIzaSyFixture_key_123456789012345678".to_owned()),
            ..crate::config::ProviderConfig::default()
        };

        assert!(matches!(
            configured_youtube_provider(&providers),
            Err(YouTubeProviderConfigurationError::FeatureDisabled {
                feature: "youtube-official",
                ..
            })
        ));
    }
}
