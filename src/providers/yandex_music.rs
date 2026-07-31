//! Feature-gated access to the private Yandex Music API.
//!
//! Yandex Music does not publish a stable player API. This adapter therefore
//! keeps its wire models deliberately tolerant, applies explicit allocation
//! bounds, and exposes a small transport boundary for fixture-based tests.
//! Callers provide a user OAuth token; the token is never included in request
//! values or debug output.
//!
//! Media resolution asks for lossless raw audio first and falls back through
//! the service's normal and low tiers only when a tier is unavailable. Returned
//! CDN URLs are restricted to credential-free HTTPS URLs on Yandex-owned
//! domains before they leave the provider boundary.

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use hmac::{Hmac, Mac};
use serde::de::{self, DeserializeOwned, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use url::Url;

pub use crate::domain::YandexMusicReaction;

use super::{DEFAULT_MAX_JSON_BYTES, DEFAULT_REQUEST_TIMEOUT, ProviderError};

const API_ORIGIN: &str = "https://api.music.yandex.net/";
const WEB_ORIGIN: &str = "https://music.yandex.ru/";
const MUSIC_CLIENT_HEADER: &str = "YandexMusicAndroid/24023621";
const FILE_INFO_CLIENT_HEADER: &str = "YandexMusicWebNext/1.0.0";
const FILE_INFO_SIGNING_KEY: &str = "7tvSmFbyf5hJnIHhCimDDD";
const MY_WAVE_SEED: &str = "user:onyourwave";
const RAW_TRANSPORT: &str = "raw";
const MAX_CONFIGURED_JSON_BYTES: usize = 16 * 1024 * 1024;
const MAX_TOKEN_BYTES: usize = 8 * 1024;
const MAX_QUERY_BYTES: usize = 512;
const MAX_SEARCH_RESULTS: usize = 100;
const MAX_REMOTE_SEARCH_ITEMS: usize = 200;
/// Maximum album references retained from one track's repeated wire fields.
const MAX_TRACK_ALBUMS: usize = 200;
const MAX_RECOMMENDATIONS: usize = 100;
/// Number of unique My Wave tracks loaded for the default recommendations page.
pub const DEFAULT_MY_WAVE_RECOMMENDATIONS: usize = 20;
/// Maximum total requests used to assemble the default My Wave page.
const MAX_MY_WAVE_REQUESTS: usize = 4;
const MAX_ALBUM_VOLUMES: usize = 100;
const MAX_ALBUM_TRACKS: usize = 2_000;
const MAX_QUEUE_ITEMS: usize = 100;
const MAX_IDENTIFIER_BYTES: usize = 100;
/// Maximum bytes retained for one opaque rotor feedback-batch identity.
const MAX_RECOMMENDATION_BATCH_ID_BYTES: usize = 1_024;
const MAX_TITLE_BYTES: usize = 1_024;
const MAX_NAME_BYTES: usize = 512;
const MAX_SERVICE_TEXT_BYTES: usize = 1_024;
const ARTWORK_PREVIEW_SIZE: &str = "400x400";
const ARTWORK_LARGE_PREVIEW_SIZE: &str = "800x800";
const ARTWORK_4K_PREVIEW_SIZE: &str = "1000x1000";
const ARTWORK_EXPANDED_VARIANT: &str = "orig";
const LOSSLESS_CODECS: &[&str] = &[
    "flac-mp4",
    "flac",
    "aac-mp4",
    "aac",
    "he-aac",
    "mp3",
    "he-aac-mp4",
];
const MEDIA_HOST_SUFFIXES: &[&str] = &[
    "yandex.net",
    "yandex.ru",
    "yandex.com",
    "yandexcdn.net",
    "yastatic.net",
];
const ARTWORK_HOSTS: &[&str] = &[
    "avatars.yandex.net",
    "avatars.mds.yandex.net",
    "avatars.mds.yandex.ru",
    "avatars.mds.yandex.com",
];

/// Search category exposed by Yandex Music.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum YandexMusicSearchScope {
    /// Music tracks and releases.
    Music,
    /// Podcast shows and episodes.
    Podcasts,
    /// Audiobooks and chapters identified by exact service metadata.
    Audiobooks,
    /// Every supported result category.
    All,
}

impl YandexMusicSearchScope {
    const fn mixed_filter(self) -> Option<&'static str> {
        match self {
            Self::Podcasts => Some("podcast"),
            Self::Audiobooks => Some("book"),
            Self::Music | Self::All => None,
        }
    }

    fn accepts(self, kind: YandexMusicContentKind) -> bool {
        match self {
            Self::Music => kind == YandexMusicContentKind::Music,
            Self::Podcasts => kind == YandexMusicContentKind::Podcast,
            Self::Audiobooks => kind == YandexMusicContentKind::Audiobook,
            Self::All => true,
        }
    }
}

/// Exact content class reported by Yandex Music metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum YandexMusicContentKind {
    /// A music track or release.
    Music,
    /// A podcast show or episode.
    Podcast,
    /// An audiobook or chapter.
    Audiobook,
}

/// Allowlisted artwork presets used by the ordinary details panel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum YandexMusicArtworkSize {
    /// Compact 400×400 artwork for smaller or incompletely reported terminals.
    Standard,
    /// Larger 800×800 artwork for spacious terminal windows.
    Large,
    /// Highest bounded 1000×1000 panel artwork for 4K terminal windows.
    FourK,
}

impl YandexMusicArtworkSize {
    /// Returns the square pixel dimensions represented by this panel preset.
    pub const fn dimensions(self) -> (u32, u32) {
        let edge = match self {
            Self::Standard => 400,
            Self::Large => 800,
            Self::FourK => 1_000,
        };
        (edge, edge)
    }

    const fn path_variant(self) -> &'static str {
        match self {
            Self::Standard => ARTWORK_PREVIEW_SIZE,
            Self::Large => ARTWORK_LARGE_PREVIEW_SIZE,
            Self::FourK => ARTWORK_4K_PREVIEW_SIZE,
        }
    }
}

/// Audio codec and container returned by current Yandex Music file info.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum YandexMusicCodec {
    /// Native FLAC.
    Flac,
    /// FLAC carried in an MP4 container.
    FlacMp4,
    /// Raw AAC.
    Aac,
    /// AAC carried in an MP4 container.
    AacMp4,
    /// Raw High-Efficiency AAC.
    HeAac,
    /// High-Efficiency AAC carried in an MP4 container.
    HeAacMp4,
    /// MPEG Layer III audio.
    Mp3,
}

impl YandexMusicCodec {
    fn from_api(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "flac" => Some(Self::Flac),
            "flac-mp4" => Some(Self::FlacMp4),
            "aac" => Some(Self::Aac),
            "aac-mp4" => Some(Self::AacMp4),
            "he-aac" => Some(Self::HeAac),
            "he-aac-mp4" => Some(Self::HeAacMp4),
            "mp3" => Some(Self::Mp3),
            _ => None,
        }
    }

    /// Returns a conventional filename extension for the resolved container.
    #[must_use]
    pub const fn file_extension(self) -> &'static str {
        match self {
            Self::Flac => "flac",
            Self::FlacMp4 | Self::AacMp4 | Self::HeAacMp4 => "m4a",
            Self::Aac | Self::HeAac => "aac",
            Self::Mp3 => "mp3",
        }
    }
}

/// Actual quality tier reported for a resolved media URL.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum YandexMusicQuality {
    /// Lossless entitlement tier.
    Lossless,
    /// Normal-quality entitlement tier.
    Normal,
    /// Low-bandwidth entitlement tier.
    Low,
}

impl YandexMusicQuality {
    const ORDERED: [Self; 3] = [Self::Lossless, Self::Normal, Self::Low];

    fn request_value(self) -> &'static str {
        match self {
            Self::Lossless => "lossless",
            Self::Normal => "nq",
            Self::Low => "lq",
        }
    }

    fn from_api(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "lossless" => Some(Self::Lossless),
            "nq" | "normal" | "high" | "hq" => Some(Self::Normal),
            "lq" | "low" => Some(Self::Low),
            _ => None,
        }
    }
}

/// Validated Yandex Music account data associated with an OAuth token.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct YandexMusicAccount {
    /// Stable account identifier.
    pub uid: String,
    /// Best available account display name.
    pub display_name: Option<String>,
    /// Whether the service reports music access as available.
    pub service_available: Option<bool>,
}

/// One bounded artist reference.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct YandexMusicArtist {
    /// Stable artist identifier when the response exposes one.
    pub id: Option<String>,
    /// Human-readable artist name.
    pub name: String,
}

impl YandexMusicArtist {
    /// Returns the canonical public artist page for a validated provider ID.
    #[must_use]
    pub fn webpage_url(&self) -> Option<Url> {
        artist_webpage_url(self.id.as_deref()?)
    }
}

/// Album metadata embedded in a track or search row.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct YandexMusicAlbumSummary {
    /// Stable album identifier.
    pub id: String,
    /// Human-readable album title.
    pub title: String,
    /// Album artists.
    pub artists: Vec<YandexMusicArtist>,
    /// Exact content category.
    pub content_kind: YandexMusicContentKind,
    /// Validated Yandex artwork URL when available.
    pub artwork_url: Option<Url>,
    /// Canonical browser page.
    pub webpage_url: Url,
}

impl YandexMusicAlbumSummary {
    /// Returns a validated 400×400 or 800×800 artwork URL for the details panel.
    #[must_use]
    pub fn panel_artwork_url(&self, size: YandexMusicArtworkSize) -> Option<Url> {
        self.artwork_url
            .as_ref()
            .and_then(|artwork_url| panel_artwork_url(artwork_url, size))
    }

    /// Returns the validated native artwork URL for full-screen display.
    #[must_use]
    pub fn expanded_artwork_url(&self) -> Option<Url> {
        self.artwork_url.as_ref().and_then(expanded_artwork_url)
    }
}

/// One playable track, podcast episode, or audiobook chapter.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct YandexMusicTrack {
    /// Stable track identifier.
    pub id: String,
    /// Human-readable title.
    pub title: String,
    /// Track artists or spoken-word authors.
    pub artists: Vec<YandexMusicArtist>,
    /// Primary album, show, or audiobook when available.
    pub album: Option<YandexMusicAlbumSummary>,
    /// Duration in milliseconds when the service exposes a valid value.
    pub duration_ms: Option<u64>,
    /// Validated Yandex artwork URL when available.
    pub artwork_url: Option<Url>,
    /// Exact content category.
    pub content_kind: YandexMusicContentKind,
    /// Current mutually exclusive library reaction.
    pub reaction: YandexMusicReaction,
    /// Canonical browser page.
    pub webpage_url: Url,
}

impl YandexMusicTrack {
    /// Returns a validated 400×400 or 800×800 artwork URL for the details panel.
    #[must_use]
    pub fn panel_artwork_url(&self, size: YandexMusicArtworkSize) -> Option<Url> {
        self.artwork_url
            .as_ref()
            .and_then(|artwork_url| panel_artwork_url(artwork_url, size))
    }

    /// Returns the validated native artwork URL for full-screen display.
    #[must_use]
    pub fn expanded_artwork_url(&self) -> Option<Url> {
        self.artwork_url.as_ref().and_then(expanded_artwork_url)
    }
}

/// One typed search result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "item", rename_all = "snake_case")]
pub enum YandexMusicSearchItem {
    /// A playable track, episode, or chapter.
    Track(Box<YandexMusicTrack>),
    /// An album, podcast show, or audiobook.
    Album(Box<YandexMusicAlbumSummary>),
}

/// One bounded search page.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct YandexMusicSearchPage {
    /// Trimmed query used for the request.
    pub query: String,
    /// Requested exact content scope.
    pub scope: YandexMusicSearchScope,
    /// Zero-based provider page.
    pub page: u32,
    /// Deduplicated results in the API's bucket order.
    pub items: Vec<YandexMusicSearchItem>,
}

/// One bounded Yandex Music artist page.
///
/// The provider artist must match the numeric identifier requested by the
/// caller. Popular tracks and albums are normalized, deduplicated by stable
/// identifier, and retained in provider order.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct YandexMusicArtistPage {
    /// Exact artist identity returned for the requested artist identifier.
    pub artist: YandexMusicArtist,
    /// Bounded popular tracks in provider order.
    pub popular_tracks: Vec<YandexMusicTrack>,
    /// Bounded artist albums in provider order.
    pub albums: Vec<YandexMusicAlbumSummary>,
}

/// One recommendation with its optional feedback-batch identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct YandexMusicRecommendedTrack {
    /// Normalized recommended track.
    pub track: YandexMusicTrack,
    /// Batch identifier associated with this recommendation, when supplied.
    ///
    /// Playback and queue continuation do not require this value. A future
    /// rotor-feedback sender must omit feedback that requires an unavailable
    /// batch identity rather than borrowing one from another page.
    pub batch_id: Option<String>,
}

/// My Wave recommendations assembled from one bounded provider session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct YandexMusicRecommendationBatch {
    /// Opaque rotor session identifier.
    pub session_id: String,
    /// Opaque initial recommendation batch identifier, when supplied.
    ///
    /// Each [`YandexMusicRecommendedTrack`] retains the batch identifier for
    /// the request that supplied it.
    pub batch_id: Option<String>,
    /// Available playable recommendations.
    pub tracks: Vec<YandexMusicRecommendedTrack>,
}

/// Track plus its one-based position in a flattened multi-volume album.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct YandexMusicAlbumTrack {
    /// One-based source volume number.
    pub volume_number: u32,
    /// One-based track number within the source volume.
    pub track_number: u32,
    /// Normalized playable track.
    pub track: YandexMusicTrack,
}

/// One album with tracks flattened across every source volume.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct YandexMusicAlbum {
    /// Album metadata.
    pub summary: YandexMusicAlbumSummary,
    /// Source-order tracks with explicit volume positions.
    pub tracks: Vec<YandexMusicAlbumTrack>,
}

/// Validated direct media metadata.
#[derive(Clone, Eq, PartialEq)]
pub struct YandexMusicMedia {
    /// Short-lived credential-free Yandex CDN URL.
    pub url: Url,
    /// Actual codec and container returned by the service.
    pub codec: YandexMusicCodec,
    /// Actual quality returned by the service.
    pub quality: YandexMusicQuality,
    /// Reported bitrate in kilobits per second, when available.
    pub bitrate_kbps: Option<u32>,
    /// Reported file size, when available.
    pub size_bytes: Option<u64>,
    decryption_key: Option<Vec<u8>>,
}

impl YandexMusicMedia {
    /// Returns the optional AES-CTR key supplied even for a raw transport.
    ///
    /// The key is ephemeral media metadata. Callers must not persist or log it.
    #[must_use]
    pub fn decryption_key(&self) -> Option<&[u8]> {
        self.decryption_key.as_deref()
    }
}

impl fmt::Debug for YandexMusicMedia {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redacted_url = self.url.host_str().map_or_else(
            || "<redacted>".to_owned(),
            |host| format!("{}://{host}/<redacted>", self.url.scheme()),
        );
        formatter
            .debug_struct("YandexMusicMedia")
            .field("url", &redacted_url)
            .field("codec", &self.codec)
            .field("quality", &self.quality)
            .field("bitrate_kbps", &self.bitrate_kbps)
            .field("size_bytes", &self.size_bytes)
            .field(
                "decryption_key",
                &self.decryption_key.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HttpMethod {
    Get,
    Post,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RequestBody {
    Json(Vec<u8>),
    Form(Vec<u8>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RequestProfile {
    Standard,
    FileInfo { account_uid: String },
}

/// One credential-free request passed to an injectable transport.
///
/// The OAuth token is passed separately to
/// [`YandexMusicTransport::execute`] so ordinary request logging cannot expose
/// it accidentally.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct YandexMusicTransportRequest {
    method: HttpMethod,
    url: Url,
    body: Option<RequestBody>,
    profile: RequestProfile,
    max_response_bytes: usize,
}

impl YandexMusicTransportRequest {
    /// Returns `GET` or `POST`.
    #[must_use]
    pub const fn method(&self) -> &'static str {
        match self.method {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
        }
    }

    /// Returns the fixed-origin API URL.
    #[must_use]
    pub const fn url(&self) -> &Url {
        &self.url
    }

    /// Returns the encoded request body, when present.
    #[must_use]
    pub fn body(&self) -> Option<&[u8]> {
        self.body.as_ref().map(|body| match body {
            RequestBody::Json(bytes) | RequestBody::Form(bytes) => bytes.as_slice(),
        })
    }

    /// Returns the body media type, when present.
    #[must_use]
    pub const fn content_type(&self) -> Option<&'static str> {
        match self.body {
            Some(RequestBody::Json(_)) => Some("application/json"),
            Some(RequestBody::Form(_)) => Some("application/x-www-form-urlencoded"),
            None => None,
        }
    }

    /// Returns the maximum accepted response byte count.
    #[must_use]
    pub const fn max_response_bytes(&self) -> usize {
        self.max_response_bytes
    }

    fn profile(&self) -> &RequestProfile {
        &self.profile
    }
}

/// Injectable blocking boundary used by the Yandex Music provider.
pub trait YandexMusicTransport: Send + Sync {
    /// Executes one bounded request.
    ///
    /// Implementations must add `Authorization: OAuth <token>` without
    /// retaining or logging the token. They should reject bodies larger than
    /// [`YandexMusicTransportRequest::max_response_bytes`]; the client checks
    /// the bound again defensively.
    ///
    /// # Errors
    ///
    /// Returns a provider error for transport, status, or body-limit failures.
    fn execute(
        &self,
        request: &YandexMusicTransportRequest,
        oauth_token: &str,
    ) -> Result<Vec<u8>, ProviderError>;
}

#[derive(Clone)]
struct UreqYandexMusicTransport {
    agent: ureq::Agent,
}

impl YandexMusicTransport for UreqYandexMusicTransport {
    fn execute(
        &self,
        request: &YandexMusicTransportRequest,
        oauth_token: &str,
    ) -> Result<Vec<u8>, ProviderError> {
        validate_api_url(request.url())?;
        let authorization = format!("OAuth {oauth_token}");
        let response = match (&request.method, &request.body) {
            (HttpMethod::Get, None) => {
                let client_header = match request.profile() {
                    RequestProfile::Standard => MUSIC_CLIENT_HEADER,
                    RequestProfile::FileInfo { .. } => FILE_INFO_CLIENT_HEADER,
                };
                let mut builder = self
                    .agent
                    .get(request.url.as_str())
                    .header("Accept", "application/json")
                    .header("Authorization", &authorization)
                    .header("X-Yandex-Music-Client", client_header);
                if let RequestProfile::FileInfo { account_uid } = request.profile() {
                    builder = builder
                        .header("X-Yandex-Music-Without-Invocation-Info", "1")
                        .header("X-Yandex-Music-Multi-Auth-User-Id", account_uid)
                        .header("Origin", "https://music.yandex.ru")
                        .header("Referer", WEB_ORIGIN);
                }
                builder.call()
            }
            (HttpMethod::Post, Some(RequestBody::Json(body))) => self
                .agent
                .post(request.url.as_str())
                .header("Accept", "application/json")
                .header("Authorization", &authorization)
                .header("X-Yandex-Music-Client", MUSIC_CLIENT_HEADER)
                .header("Content-Type", "application/json")
                .send(body.as_slice()),
            (HttpMethod::Post, Some(RequestBody::Form(body))) => self
                .agent
                .post(request.url.as_str())
                .header("Accept", "application/json")
                .header("Authorization", &authorization)
                .header("X-Yandex-Music-Client", MUSIC_CLIENT_HEADER)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .send(body.as_slice()),
            _ => {
                return Err(ProviderError::InvalidRequest(
                    "Yandex Music request method and body do not match".to_owned(),
                ));
            }
        }
        .map_err(map_ureq_error)?;

        read_bounded_response(response, request.max_response_bytes)
    }
}

/// Blocking, resource-bounded Yandex Music client.
#[derive(Clone)]
pub struct YandexMusicClient {
    transport: Arc<dyn YandexMusicTransport>,
    oauth_token: String,
    account_uid: Arc<OnceLock<String>>,
    max_json_bytes: usize,
}

impl fmt::Debug for YandexMusicClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("YandexMusicClient")
            .field("oauth_token", &"<redacted>")
            .field("max_json_bytes", &self.max_json_bytes)
            .finish_non_exhaustive()
    }
}

impl YandexMusicClient {
    /// Creates a client for one user OAuth token.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] for an empty, whitespace,
    /// control-bearing, or oversized token.
    pub fn new(oauth_token: impl Into<String>) -> Result<Self, ProviderError> {
        let oauth_token = validate_oauth_token(oauth_token.into())?;
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(DEFAULT_REQUEST_TIMEOUT))
            .https_only(true)
            .max_redirects(0)
            .user_agent(concat!(
                "youta/",
                env!("CARGO_PKG_VERSION"),
                " (+",
                env!("CARGO_PKG_REPOSITORY"),
                ")"
            ))
            .build()
            .into();
        Ok(Self {
            transport: Arc::new(UreqYandexMusicTransport { agent }),
            oauth_token,
            account_uid: Arc::new(OnceLock::new()),
            max_json_bytes: DEFAULT_MAX_JSON_BYTES,
        })
    }

    /// Creates a client around an injectable transport and explicit body limit.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] for an invalid token or for a
    /// zero or excessive response limit.
    pub fn with_transport(
        oauth_token: impl Into<String>,
        transport: Arc<dyn YandexMusicTransport>,
        max_json_bytes: usize,
    ) -> Result<Self, ProviderError> {
        if !(1..=MAX_CONFIGURED_JSON_BYTES).contains(&max_json_bytes) {
            return Err(ProviderError::InvalidRequest(format!(
                "Yandex Music JSON limit must be between 1 and {MAX_CONFIGURED_JSON_BYTES} bytes"
            )));
        }
        Ok(Self {
            transport,
            oauth_token: validate_oauth_token(oauth_token.into())?,
            account_uid: Arc::new(OnceLock::new()),
            max_json_bytes,
        })
    }

    /// Validates the configured token by loading its account status.
    ///
    /// # Errors
    ///
    /// Returns a provider error for transport/status failures or when the
    /// bounded response omits a valid account identifier.
    pub fn validate_account(&self) -> Result<YandexMusicAccount, ProviderError> {
        let request = self.get_request("account/status")?;
        let result: RawAccountStatus = self.execute_envelope(&request)?;
        let account = normalize_account(&result)?;
        let _ = self.account_uid.set(account.uid.clone());
        Ok(account)
    }

    /// Starts the user's `My Wave` recommendation session.
    ///
    /// Youta makes one initial request and at most three continuation requests,
    /// stopping early after twenty unique playable tracks or when a response
    /// adds no new track. The provider controls each response size, so Youta
    /// cannot require exactly five tracks from an individual request.
    ///
    /// # Errors
    ///
    /// Returns a provider error for transport/status failures, an oversized
    /// response, or incomplete session metadata.
    pub fn my_wave(&self) -> Result<YandexMusicRecommendationBatch, ProviderError> {
        let mut recommendations = self.start_my_wave()?;
        let mut seen = recommendations
            .tracks
            .iter()
            .map(|recommendation| recommendation.track.id.clone())
            .collect::<HashSet<_>>();

        for _ in 1..MAX_MY_WAVE_REQUESTS {
            if recommendations.tracks.len() >= DEFAULT_MY_WAVE_RECOMMENDATIONS {
                break;
            }
            let queue = recommendations
                .tracks
                .iter()
                .map(|recommendation| recommendation.track.id.clone())
                .collect::<Vec<_>>();
            let continuation = self.more_my_wave(&recommendations.session_id, &queue)?;
            let previous_len = recommendations.tracks.len();
            for recommendation in continuation.tracks {
                if seen.insert(recommendation.track.id.clone()) {
                    recommendations.tracks.push(recommendation);
                    if recommendations.tracks.len() >= DEFAULT_MY_WAVE_RECOMMENDATIONS {
                        break;
                    }
                }
            }
            if recommendations.tracks.len() == previous_len {
                break;
            }
        }
        recommendations
            .tracks
            .truncate(DEFAULT_MY_WAVE_RECOMMENDATIONS);
        Ok(recommendations)
    }

    fn start_my_wave(&self) -> Result<YandexMusicRecommendationBatch, ProviderError> {
        let body = json!({
            "seeds": [MY_WAVE_SEED],
            "queue": [],
            "includeTracksInResponse": true,
            "includeWaveModel": true,
            "interactive": true,
        });
        let request = self.json_request("rotor/session/new", &body)?;
        let result: RawRecommendationBatch = self.execute_envelope(&request)?;
        normalize_recommendation_batch(result, None)
    }

    /// Continues one My Wave session using its recent provider queue.
    ///
    /// # Errors
    ///
    /// Returns a provider error for invalid session/queue identifiers,
    /// transport/status failures, or malformed bounded response data.
    pub fn more_my_wave(
        &self,
        session_id: &str,
        queue: &[String],
    ) -> Result<YandexMusicRecommendationBatch, ProviderError> {
        let session_id = validate_identifier(session_id, "recommendation session")?;
        if queue.len() > MAX_QUEUE_ITEMS {
            return Err(ProviderError::InvalidRequest(format!(
                "Yandex Music recommendation queue exceeds {MAX_QUEUE_ITEMS} entries"
            )));
        }
        let queue = queue
            .iter()
            .map(|item| validate_queue_identifier(item))
            .collect::<Result<Vec<_>, _>>()?;
        let body = json!({ "queue": queue });
        let request = self.json_request(&format!("rotor/session/{session_id}/tracks"), &body)?;
        let result: RawRecommendationBatch = self.execute_envelope(&request)?;
        normalize_recommendation_batch(result, Some(session_id))
    }

    /// Searches one exact content scope.
    ///
    /// Audiobook filtering uses only exact API `type`/`metaType` values. Titles,
    /// artists, genres, and free-form descriptions never influence the
    /// classification.
    ///
    /// `limit` is a local result cap. Scoped podcast and audiobook searches
    /// also pass it as the mixed endpoint's remote `pageSize`.
    ///
    /// # Errors
    ///
    /// Returns a provider error for invalid query/page/limit input,
    /// transport/status failures, an oversized response, or malformed data.
    pub fn search(
        &self,
        query: &str,
        scope: YandexMusicSearchScope,
        page: u32,
        limit: usize,
    ) -> Result<YandexMusicSearchPage, ProviderError> {
        let query = validate_query(query)?;
        if !(1..=MAX_SEARCH_RESULTS).contains(&limit) {
            return Err(ProviderError::InvalidRequest(format!(
                "Yandex Music search limit must be between 1 and {MAX_SEARCH_RESULTS}"
            )));
        }
        if let Some(filter) = scope.mixed_filter() {
            return self.search_spoken_word(query, scope, filter, page, limit);
        }
        let mut url = Self::api_url("search")?;
        url.query_pairs_mut()
            .append_pair("text", query)
            .append_pair("page", &page.to_string())
            .append_pair("type", "all")
            .append_pair("nocorrect", "false");
        let request = YandexMusicTransportRequest {
            method: HttpMethod::Get,
            url,
            body: None,
            profile: RequestProfile::Standard,
            max_response_bytes: self.max_json_bytes,
        };
        let result: RawSearchResponse = self.execute_envelope(&request)?;
        normalize_search(result, query, scope, page, limit)
    }

    /// Searches Yandex's current mixed spoken-word index while preserving its
    /// interleaved show/episode order.
    fn search_spoken_word(
        &self,
        query: &str,
        scope: YandexMusicSearchScope,
        filter: &str,
        page: u32,
        limit: usize,
    ) -> Result<YandexMusicSearchPage, ProviderError> {
        let mut url = Self::api_url("search/instant/mixed")?;
        url.query_pairs_mut()
            .append_pair("text", query)
            .append_pair("type", "all")
            .append_pair("filter", filter)
            .append_pair("page", &page.to_string())
            .append_pair("pageSize", &limit.to_string())
            .append_pair("nocorrect", "false")
            .append_pair("withLikesCount", "true")
            .append_pair("withBestResults", "false");
        let request = YandexMusicTransportRequest {
            method: HttpMethod::Get,
            url,
            body: None,
            profile: RequestProfile::Standard,
            max_response_bytes: self.max_json_bytes,
        };
        let result: RawMixedSearchResponse = self.execute_envelope(&request)?;
        normalize_mixed_search(result, query, scope, page, limit)
    }

    /// Loads one artist's bounded brief page.
    ///
    /// # Errors
    ///
    /// Returns a provider error for an invalid numeric artist identifier,
    /// transport/status failures, excessive track or album counts, malformed
    /// artist metadata, or a returned artist identity that differs from the
    /// requested identifier.
    pub fn artist_page(&self, artist_id: &str) -> Result<YandexMusicArtistPage, ProviderError> {
        let artist_id = validate_numeric_identifier(artist_id, "artist")?;
        let request = self.get_request(&format!("artists/{artist_id}/brief-info"))?;
        let result: RawArtistBriefInfo = self.execute_envelope(&request)?;
        normalize_artist_page(result, artist_id)
    }

    /// Loads one album and flattens every source volume in order.
    ///
    /// # Errors
    ///
    /// Returns a provider error for an invalid album identifier,
    /// transport/status failures, excessive volume/track counts, or incomplete
    /// album metadata.
    pub fn album_with_tracks(&self, album_id: &str) -> Result<YandexMusicAlbum, ProviderError> {
        let album_id = validate_numeric_identifier(album_id, "album")?;
        let request = self.get_request(&format!("albums/{album_id}/with-tracks"))?;
        let result: RawAlbum = self.execute_envelope(&request)?;
        normalize_album_with_tracks(result)
    }

    /// Sets one mutually exclusive liked/disliked/neutral state.
    ///
    /// The client removes the opposite state before adding the requested one.
    /// Neutral removes both states. Each call is idempotent on the service.
    ///
    /// # Errors
    ///
    /// Returns a provider error for invalid identifiers or when any mutation
    /// request fails. A partially completed sequence remains safe: it can only
    /// leave the track neutral, never both liked and disliked.
    pub fn set_reaction(
        &self,
        account_uid: &str,
        track_id: &str,
        reaction: YandexMusicReaction,
    ) -> Result<(), ProviderError> {
        let account_uid = validate_identifier(account_uid, "account")?;
        let track_id = validate_identifier(track_id, "track")?;
        match reaction {
            YandexMusicReaction::Liked => {
                self.mutate_reaction(account_uid, track_id, "dislikes", "remove")?;
                self.mutate_reaction(account_uid, track_id, "likes", "add-multiple")
            }
            YandexMusicReaction::Disliked => {
                self.mutate_reaction(account_uid, track_id, "likes", "remove")?;
                self.mutate_reaction(account_uid, track_id, "dislikes", "add-multiple")
            }
            YandexMusicReaction::Neutral => {
                self.mutate_reaction(account_uid, track_id, "likes", "remove")?;
                self.mutate_reaction(account_uid, track_id, "dislikes", "remove")
            }
        }
    }

    /// Resolves the highest available raw audio URL.
    ///
    /// The request order is lossless, normal, then low quality. Lower tiers are
    /// attempted only for HTTP/service errors that indicate an unavailable
    /// tier. Returned codec and quality values always come from the response,
    /// not from the requested tier.
    ///
    /// # Errors
    ///
    /// Returns a provider error for invalid identifiers, transport/auth
    /// failures, malformed media metadata, unsupported codecs, or unsafe CDN
    /// URLs.
    pub fn resolve_media(&self, track_id: &str) -> Result<YandexMusicMedia, ProviderError> {
        let track_id = validate_identifier(track_id, "track")?;
        let account_uid = self.validated_account_uid()?;
        let mut unavailable = None;
        for quality in YandexMusicQuality::ORDERED {
            let request = self.file_info_request(&account_uid, track_id, quality)?;
            match self.execute_bytes(&request) {
                Ok(bytes) => return parse_media_response(&bytes),
                Err(error) if quality_is_unavailable(&error) => unavailable = Some(error),
                Err(error) => return Err(error),
            }
        }
        Err(unavailable.unwrap_or_else(|| {
            ProviderError::InvalidResponse(
                "Yandex Music returned no supported audio quality".to_owned(),
            )
        }))
    }

    fn mutate_reaction(
        &self,
        account_uid: &str,
        track_id: &str,
        collection: &str,
        action: &str,
    ) -> Result<(), ProviderError> {
        let request = self.form_request(
            &format!("users/{account_uid}/{collection}/tracks/{action}"),
            &[("track-ids", track_id)],
        )?;
        let _: Value = self.execute_envelope(&request)?;
        Ok(())
    }

    fn file_info_request(
        &self,
        account_uid: &str,
        track_id: &str,
        quality: YandexMusicQuality,
    ) -> Result<YandexMusicTransportRequest, ProviderError> {
        let account_uid = validate_identifier(account_uid, "account")?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ProviderError::Transport("system time is before Unix epoch".to_owned()))?
            .as_secs();
        let codecs = LOSSLESS_CODECS.join(",");
        let signature = sign_file_info(track_id, timestamp, quality.request_value());
        let mut url = Self::api_url("get-file-info")?;
        url.query_pairs_mut()
            .append_pair("ts", &timestamp.to_string())
            .append_pair("trackId", track_id)
            .append_pair("quality", quality.request_value())
            .append_pair("codecs", &codecs)
            .append_pair("transports", RAW_TRANSPORT)
            .append_pair("sign", &signature);
        Ok(YandexMusicTransportRequest {
            method: HttpMethod::Get,
            url,
            body: None,
            profile: RequestProfile::FileInfo {
                account_uid: account_uid.to_owned(),
            },
            max_response_bytes: self.max_json_bytes,
        })
    }

    fn validated_account_uid(&self) -> Result<String, ProviderError> {
        match self.account_uid.get() {
            Some(account_uid) => Ok(account_uid.clone()),
            None => self.validate_account().map(|account| account.uid),
        }
    }

    fn get_request(&self, path: &str) -> Result<YandexMusicTransportRequest, ProviderError> {
        Ok(YandexMusicTransportRequest {
            method: HttpMethod::Get,
            url: Self::api_url(path)?,
            body: None,
            profile: RequestProfile::Standard,
            max_response_bytes: self.max_json_bytes,
        })
    }

    fn json_request(
        &self,
        path: &str,
        value: &Value,
    ) -> Result<YandexMusicTransportRequest, ProviderError> {
        let bytes = serde_json::to_vec(value).map_err(|error| {
            ProviderError::InvalidRequest(format!(
                "cannot encode Yandex Music JSON request: {error}"
            ))
        })?;
        Ok(YandexMusicTransportRequest {
            method: HttpMethod::Post,
            url: Self::api_url(path)?,
            body: Some(RequestBody::Json(bytes)),
            profile: RequestProfile::Standard,
            max_response_bytes: self.max_json_bytes,
        })
    }

    fn form_request(
        &self,
        path: &str,
        values: &[(&str, &str)],
    ) -> Result<YandexMusicTransportRequest, ProviderError> {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        serializer.extend_pairs(values.iter().copied());
        Ok(YandexMusicTransportRequest {
            method: HttpMethod::Post,
            url: Self::api_url(path)?,
            body: Some(RequestBody::Form(serializer.finish().into_bytes())),
            profile: RequestProfile::Standard,
            max_response_bytes: self.max_json_bytes,
        })
    }

    fn api_url(path: &str) -> Result<Url, ProviderError> {
        let base = Url::parse(API_ORIGIN)
            .map_err(|error| ProviderError::InvalidBaseUrl(error.to_string()))?;
        let url = base.join(path).map_err(|error| {
            ProviderError::InvalidRequest(format!("invalid Yandex Music API path: {error}"))
        })?;
        validate_api_url(&url)?;
        Ok(url)
    }

    fn execute_bytes(
        &self,
        request: &YandexMusicTransportRequest,
    ) -> Result<Vec<u8>, ProviderError> {
        validate_api_url(request.url())?;
        let bytes = self.transport.execute(request, &self.oauth_token)?;
        if bytes.len() > request.max_response_bytes {
            return Err(ProviderError::ResponseTooLarge {
                limit: request.max_response_bytes,
            });
        }
        Ok(bytes)
    }

    fn execute_envelope<T: DeserializeOwned>(
        &self,
        request: &YandexMusicTransportRequest,
    ) -> Result<T, ProviderError> {
        let bytes = self.execute_bytes(request)?;
        parse_envelope(&bytes)
    }
}

fn validate_oauth_token(token: String) -> Result<String, ProviderError> {
    if token.is_empty()
        || token.len() > MAX_TOKEN_BYTES
        || token.trim() != token
        || token.chars().any(char::is_whitespace)
        || token.chars().any(char::is_control)
    {
        return Err(ProviderError::InvalidRequest(
            "Yandex Music OAuth token is empty or invalid".to_owned(),
        ));
    }
    Ok(token)
}

fn validate_query(query: &str) -> Result<&str, ProviderError> {
    let query = query.trim();
    if query.is_empty()
        || query.len() > MAX_QUERY_BYTES
        || query
            .chars()
            .any(|character| character.is_control() && !character.is_whitespace())
    {
        return Err(ProviderError::InvalidRequest(format!(
            "Yandex Music search query must contain 1 to {MAX_QUERY_BYTES} safe bytes"
        )));
    }
    Ok(query)
}

fn validate_identifier<'a>(value: &'a str, purpose: &str) -> Result<&'a str, ProviderError> {
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':' | b'-'))
    {
        return Err(ProviderError::InvalidRequest(format!(
            "invalid Yandex Music {purpose} identifier"
        )));
    }
    Ok(value)
}

/// Normalizes an optional provider-owned feedback identity without assuming
/// that its punctuation follows Yandex Music's URL-identifier grammar.
fn normalize_optional_recommendation_batch_id(
    value: Option<&str>,
) -> Result<Option<String>, ProviderError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.trim().is_empty() {
        return Ok(None);
    }
    if value.len() > MAX_RECOMMENDATION_BATCH_ID_BYTES || value.chars().any(char::is_control) {
        return Err(ProviderError::InvalidResponse(
            "Yandex Music recommendation returned an invalid batch id".to_owned(),
        ));
    }
    Ok(Some(value.to_owned()))
}

fn validate_numeric_identifier<'a>(
    value: &'a str,
    purpose: &str,
) -> Result<&'a str, ProviderError> {
    let value = validate_identifier(value, purpose)?;
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ProviderError::InvalidRequest(format!(
            "Yandex Music {purpose} identifier must be numeric"
        )));
    }
    Ok(value)
}

fn validate_queue_identifier(value: &str) -> Result<String, ProviderError> {
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES * 2 + 1
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':' | b'-'))
    {
        return Err(ProviderError::InvalidRequest(
            "invalid Yandex Music recommendation queue identifier".to_owned(),
        ));
    }
    Ok(value.to_owned())
}

fn validate_api_url(url: &Url) -> Result<(), ProviderError> {
    let base =
        Url::parse(API_ORIGIN).map_err(|error| ProviderError::InvalidBaseUrl(error.to_string()))?;
    if url.scheme() != "https"
        || url.host_str() != base.host_str()
        || url.port_or_known_default() != base.port_or_known_default()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(ProviderError::InvalidRequest(
            "Yandex Music request left the fixed HTTPS API origin".to_owned(),
        ));
    }
    Ok(())
}

fn read_bounded_response(
    mut response: ureq::http::Response<ureq::Body>,
    limit: usize,
) -> Result<Vec<u8>, ProviderError> {
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
    Ok(bytes)
}

fn map_ureq_error(error: ureq::Error) -> ProviderError {
    match error {
        ureq::Error::StatusCode(code) => ProviderError::HttpStatus(code),
        ureq::Error::BodyExceedsLimit(limit) => ProviderError::ResponseTooLarge {
            limit: usize::try_from(limit).unwrap_or(usize::MAX),
        },
        other => ProviderError::Transport(other.to_string()),
    }
}

fn sign_file_info(track_id: &str, timestamp: u64, quality: &str) -> String {
    let codecs = LOSSLESS_CODECS.concat();
    let message = format!("{timestamp}{track_id}{quality}{codecs}{RAW_TRANSPORT}");
    let mut mac = Hmac::<Sha256>::new_from_slice(FILE_INFO_SIGNING_KEY.as_bytes())
        .expect("HMAC accepts protocol signing keys of any length");
    mac.update(message.as_bytes());
    STANDARD_NO_PAD.encode(mac.finalize().into_bytes())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEnvelope<T> {
    result: Option<T>,
    error: Option<RawServiceError>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawServiceError {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

fn parse_envelope<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ProviderError> {
    let envelope = serde_json::from_slice::<RawEnvelope<T>>(bytes)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    if let Some(error) = envelope.error {
        return Err(service_error(&error));
    }
    envelope.result.ok_or_else(|| {
        ProviderError::InvalidResponse("Yandex Music response omitted its result".to_owned())
    })
}

fn service_error(error: &RawServiceError) -> ProviderError {
    ProviderError::Service {
        status: 400,
        reason: bounded_service_text(error.name.as_deref().unwrap_or("api_error"), "api_error"),
        message: bounded_service_text(
            error
                .message
                .as_deref()
                .unwrap_or("Yandex Music rejected the request"),
            "Yandex Music rejected the request",
        ),
    }
}

fn bounded_service_text(value: &str, fallback: &str) -> String {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return fallback.to_owned();
    }
    value.chars().take(MAX_SERVICE_TEXT_BYTES).collect()
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAccountStatus {
    #[serde(default)]
    account: Option<RawAccount>,
    #[serde(default)]
    uid: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAccount {
    #[serde(default)]
    uid: Option<Value>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    full_name: Option<String>,
    #[serde(default)]
    login: Option<String>,
    #[serde(default)]
    service_available: Option<bool>,
}

fn normalize_account(raw: &RawAccountStatus) -> Result<YandexMusicAccount, ProviderError> {
    let uid = raw
        .account
        .as_ref()
        .and_then(|account| account.uid.as_ref())
        .or(raw.uid.as_ref())
        .and_then(json_identifier)
        .and_then(|value| {
            validate_identifier(&value, "account")
                .ok()
                .map(str::to_owned)
        })
        .ok_or_else(|| {
            ProviderError::InvalidResponse(
                "Yandex Music account status omitted a valid uid".to_owned(),
            )
        })?;
    let display_name = raw.account.as_ref().and_then(|account| {
        account
            .display_name
            .as_deref()
            .or(account.full_name.as_deref())
            .or(account.login.as_deref())
            .and_then(|value| bounded_text(value, MAX_NAME_BYTES))
    });
    let service_available = raw
        .account
        .as_ref()
        .and_then(|account| account.service_available);
    Ok(YandexMusicAccount {
        uid,
        display_name,
        service_available,
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawArtist {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawArtistBriefInfo {
    #[serde(default)]
    artist: RawArtist,
    #[serde(default)]
    popular_tracks: Vec<RawTrack>,
    #[serde(default)]
    albums: Vec<RawAlbum>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAlbum {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    artists: Vec<RawArtist>,
    #[serde(default)]
    available: Option<bool>,
    #[serde(default)]
    cover_uri: Option<String>,
    #[serde(default)]
    og_image: Option<String>,
    #[serde(default)]
    meta_type: Option<String>,
    #[serde(default, rename = "type")]
    item_type: Option<String>,
    #[serde(default)]
    volumes: Vec<Vec<RawTrack>>,
}

#[derive(Debug, Default)]
struct RawTrack {
    id: Option<Value>,
    real_id: Option<Value>,
    title: Option<String>,
    artists: Vec<RawArtist>,
    albums: Vec<RawAlbum>,
    duration_ms: Option<Value>,
    cover_uri: Option<String>,
    og_image: Option<String>,
    available: Option<bool>,
    item_type: Option<String>,
}

impl<'de> Deserialize<'de> for RawTrack {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(RawTrackVisitor)
    }
}

struct RawTrackVisitor;

impl<'de> Visitor<'de> for RawTrackVisitor {
    type Value = RawTrack;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a Yandex Music track object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut id = None;
        let mut real_id = None;
        let mut title = None;
        let mut artists = None;
        let mut albums = Vec::new();
        let mut duration_ms = None;
        let mut cover_uri = None;
        let mut og_image = None;
        let mut available = None;
        let mut item_type = None;

        while let Some(field) = map.next_key::<String>()? {
            match field.as_str() {
                "id" => deserialize_field_once(&mut id, &mut map, "id")?,
                "realId" => deserialize_field_once(&mut real_id, &mut map, "realId")?,
                "title" => deserialize_field_once(&mut title, &mut map, "title")?,
                "artists" => deserialize_field_once(&mut artists, &mut map, "artists")?,
                "albums" => {
                    let incoming = map.next_value::<Vec<RawAlbum>>()?;
                    extend_bounded(
                        &mut albums,
                        incoming,
                        MAX_TRACK_ALBUMS,
                        "track album references",
                    )?;
                }
                "durationMs" => {
                    deserialize_field_once(&mut duration_ms, &mut map, "durationMs")?;
                }
                "coverUri" => {
                    deserialize_field_once(&mut cover_uri, &mut map, "coverUri")?;
                }
                "ogImage" => deserialize_field_once(&mut og_image, &mut map, "ogImage")?,
                "available" => {
                    deserialize_field_once(&mut available, &mut map, "available")?;
                }
                "type" => deserialize_field_once(&mut item_type, &mut map, "type")?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }

        Ok(RawTrack {
            id: id.unwrap_or_default(),
            real_id: real_id.unwrap_or_default(),
            title: title.unwrap_or_default(),
            artists: artists.unwrap_or_default(),
            albums,
            duration_ms: duration_ms.unwrap_or_default(),
            cover_uri: cover_uri.unwrap_or_default(),
            og_image: og_image.unwrap_or_default(),
            available: available.unwrap_or_default(),
            item_type: item_type.unwrap_or_default(),
        })
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSearchBucket<T> {
    #[serde(default, rename = "type")]
    item_type: Option<String>,
    #[serde(default)]
    results: Vec<T>,
}

#[derive(Debug, Default)]
struct RawSearchResponse {
    tracks: Option<RawSearchBucket<RawTrack>>,
    albums: Option<RawSearchBucket<RawAlbum>>,
    podcasts: Option<RawSearchBucket<RawAlbum>>,
    podcast_episodes: Option<RawSearchBucket<RawTrack>>,
}

/// One response from Yandex's interleaved spoken-word search endpoint.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMixedSearchResponse {
    #[serde(default)]
    results: Vec<RawMixedSearchItem>,
}

/// One show or episode wrapper from the interleaved search result list.
#[derive(Debug, Default, Deserialize)]
struct RawMixedSearchItem {
    #[serde(default, rename = "type")]
    item_type: Option<String>,
    #[serde(default)]
    podcast: Option<RawAlbum>,
    #[serde(default, rename = "podcast_episode", alias = "podcastEpisode")]
    podcast_episode: Option<RawTrack>,
}

impl<'de> Deserialize<'de> for RawSearchResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(RawSearchResponseVisitor)
    }
}

struct RawSearchResponseVisitor;

impl<'de> Visitor<'de> for RawSearchResponseVisitor {
    type Value = RawSearchResponse;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a Yandex Music search result object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut tracks = None;
        let mut albums = None;
        let mut podcasts = None;
        let mut podcast_episodes = None;

        while let Some(field) = map.next_key::<String>()? {
            match field.as_str() {
                "tracks" => deserialize_field_once(&mut tracks, &mut map, "tracks")?,
                "albums" => {
                    let incoming = map.next_value::<Option<RawSearchBucket<RawAlbum>>>()?;
                    merge_search_album_bucket(&mut albums, incoming)?;
                }
                "podcasts" => deserialize_field_once(&mut podcasts, &mut map, "podcasts")?,
                "podcastEpisodes" | "podcast_episodes" => {
                    deserialize_field_once(&mut podcast_episodes, &mut map, "podcastEpisodes")?;
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }

        Ok(RawSearchResponse {
            tracks: tracks.unwrap_or_default(),
            albums,
            podcasts: podcasts.unwrap_or_default(),
            podcast_episodes: podcast_episodes.unwrap_or_default(),
        })
    }
}

/// Reads one ordinary wire field while retaining serde's duplicate rejection.
fn deserialize_field_once<'de, A, T>(
    slot: &mut Option<T>,
    map: &mut A,
    field: &'static str,
) -> Result<(), A::Error>
where
    A: MapAccess<'de>,
    T: Deserialize<'de>,
{
    if slot.is_some() {
        return Err(de::Error::duplicate_field(field));
    }
    *slot = Some(map.next_value()?);
    Ok(())
}

/// Extends a repeated provider array without dropping either occurrence.
fn extend_bounded<T, E>(
    target: &mut Vec<T>,
    incoming: Vec<T>,
    limit: usize,
    context: &str,
) -> Result<(), E>
where
    E: de::Error,
{
    let merged_len = target
        .len()
        .checked_add(incoming.len())
        .ok_or_else(|| E::custom(format!("Yandex Music {context} count overflowed")))?;
    if merged_len > limit {
        return Err(E::custom(format!(
            "Yandex Music returned more than {limit} {context}"
        )));
    }
    target.extend(incoming);
    Ok(())
}

/// Merges repeated search-level album buckets in their original wire order.
fn merge_search_album_bucket<E>(
    target: &mut Option<RawSearchBucket<RawAlbum>>,
    incoming: Option<RawSearchBucket<RawAlbum>>,
) -> Result<(), E>
where
    E: de::Error,
{
    let Some(mut incoming) = incoming else {
        return Ok(());
    };
    if incoming.results.len() > MAX_REMOTE_SEARCH_ITEMS {
        return Err(E::custom(format!(
            "Yandex Music search returned more than {MAX_REMOTE_SEARCH_ITEMS} items"
        )));
    }
    let Some(existing) = target.as_mut() else {
        *target = Some(incoming);
        return Ok(());
    };

    match (&existing.item_type, incoming.item_type.take()) {
        (Some(existing_type), Some(incoming_type)) if existing_type != &incoming_type => {
            return Err(E::custom(
                "Yandex Music repeated album buckets with conflicting types",
            ));
        }
        (None, Some(incoming_type)) => existing.item_type = Some(incoming_type),
        _ => {}
    }
    extend_bounded(
        &mut existing.results,
        incoming.results,
        MAX_REMOTE_SEARCH_ITEMS,
        "search items",
    )
}

fn normalize_search(
    raw: RawSearchResponse,
    query: &str,
    scope: YandexMusicSearchScope,
    page: u32,
    limit: usize,
) -> Result<YandexMusicSearchPage, ProviderError> {
    let remote_count = raw
        .tracks
        .as_ref()
        .map_or(0, |bucket| bucket.results.len())
        .saturating_add(raw.albums.as_ref().map_or(0, |bucket| bucket.results.len()))
        .saturating_add(
            raw.podcasts
                .as_ref()
                .map_or(0, |bucket| bucket.results.len()),
        )
        .saturating_add(
            raw.podcast_episodes
                .as_ref()
                .map_or(0, |bucket| bucket.results.len()),
        );
    if remote_count > MAX_REMOTE_SEARCH_ITEMS {
        return Err(ProviderError::InvalidResponse(format!(
            "Yandex Music search returned more than {MAX_REMOTE_SEARCH_ITEMS} items"
        )));
    }

    let mut items = Vec::with_capacity(limit);
    let mut seen = HashSet::new();
    if let Some(bucket) = raw.tracks {
        append_search_tracks(
            &mut items,
            &mut seen,
            bucket.results,
            bucket.item_type.as_deref(),
            scope,
            limit,
        );
    }
    if let Some(bucket) = raw.albums {
        append_search_albums(
            &mut items,
            &mut seen,
            bucket.results,
            bucket.item_type.as_deref(),
            scope,
            limit,
        );
    }
    if let Some(bucket) = raw.podcasts {
        append_search_albums(
            &mut items,
            &mut seen,
            bucket.results,
            Some(bucket.item_type.as_deref().unwrap_or("podcast")),
            scope,
            limit,
        );
    }
    if let Some(bucket) = raw.podcast_episodes {
        append_search_tracks(
            &mut items,
            &mut seen,
            bucket.results,
            Some(bucket.item_type.as_deref().unwrap_or("podcast_episode")),
            scope,
            limit,
        );
    }

    Ok(YandexMusicSearchPage {
        query: query.to_owned(),
        scope,
        page,
        items,
    })
}

/// Normalizes interleaved mixed-search wrappers without regrouping shows and
/// episodes, so the terminal list retains Yandex's relevance order.
fn normalize_mixed_search(
    raw: RawMixedSearchResponse,
    query: &str,
    scope: YandexMusicSearchScope,
    page: u32,
    limit: usize,
) -> Result<YandexMusicSearchPage, ProviderError> {
    if raw.results.len() > MAX_REMOTE_SEARCH_ITEMS {
        return Err(ProviderError::InvalidResponse(format!(
            "Yandex Music search returned more than {MAX_REMOTE_SEARCH_ITEMS} items"
        )));
    }

    let mut items = Vec::with_capacity(limit.min(raw.results.len()));
    let mut seen = HashSet::new();
    for raw_item in raw.results {
        if items.len() == limit {
            break;
        }
        let outer_type = raw_item.item_type.as_deref();
        let normalized_outer_type = outer_type.map(|value| value.trim().to_ascii_lowercase());
        match normalized_outer_type.as_deref() {
            Some("podcast") => {
                append_mixed_album(&mut items, &mut seen, raw_item.podcast, outer_type, scope)
            }
            Some("podcast_episode" | "podcast-episode") => append_mixed_track(
                &mut items,
                &mut seen,
                raw_item.podcast_episode,
                outer_type,
                scope,
            ),
            _ if raw_item.podcast_episode.is_some() => append_mixed_track(
                &mut items,
                &mut seen,
                raw_item.podcast_episode,
                outer_type,
                scope,
            ),
            _ => append_mixed_album(&mut items, &mut seen, raw_item.podcast, outer_type, scope),
        }
    }

    Ok(YandexMusicSearchPage {
        query: query.to_owned(),
        scope,
        page,
        items,
    })
}

fn append_mixed_track(
    items: &mut Vec<YandexMusicSearchItem>,
    seen: &mut HashSet<String>,
    raw: Option<RawTrack>,
    wrapper_type: Option<&str>,
    scope: YandexMusicSearchScope,
) {
    let Some(raw) = raw else {
        return;
    };
    let kind = exact_track_kind(&raw, wrapper_type);
    if !scope.accepts(kind) {
        return;
    }
    let Some(track) = normalize_track(&raw, kind, None) else {
        return;
    };
    if seen.insert(format!("track:{}", track.id)) {
        items.push(YandexMusicSearchItem::Track(Box::new(track)));
    }
}

fn append_mixed_album(
    items: &mut Vec<YandexMusicSearchItem>,
    seen: &mut HashSet<String>,
    raw: Option<RawAlbum>,
    wrapper_type: Option<&str>,
    scope: YandexMusicSearchScope,
) {
    let Some(raw) = raw else {
        return;
    };
    let kind = exact_album_kind(&raw, wrapper_type);
    if !scope.accepts(kind) {
        return;
    }
    let Some(album) = normalize_album_summary(&raw, kind) else {
        return;
    };
    if seen.insert(format!("album:{}", album.id)) {
        items.push(YandexMusicSearchItem::Album(Box::new(album)));
    }
}

fn append_search_tracks(
    items: &mut Vec<YandexMusicSearchItem>,
    seen: &mut HashSet<String>,
    tracks: Vec<RawTrack>,
    bucket_type: Option<&str>,
    scope: YandexMusicSearchScope,
    limit: usize,
) {
    for track in tracks {
        if items.len() == limit {
            break;
        }
        let kind = exact_track_kind(&track, bucket_type);
        if !scope.accepts(kind) {
            continue;
        }
        let Some(track) = normalize_track(&track, kind, None) else {
            continue;
        };
        if seen.insert(format!("track:{}", track.id)) {
            items.push(YandexMusicSearchItem::Track(Box::new(track)));
        }
    }
}

fn append_search_albums(
    items: &mut Vec<YandexMusicSearchItem>,
    seen: &mut HashSet<String>,
    albums: Vec<RawAlbum>,
    bucket_type: Option<&str>,
    scope: YandexMusicSearchScope,
    limit: usize,
) {
    for album in albums {
        if items.len() == limit {
            break;
        }
        let kind = exact_album_kind(&album, bucket_type);
        if !scope.accepts(kind) {
            continue;
        }
        let Some(album) = normalize_album_summary(&album, kind) else {
            continue;
        };
        if seen.insert(format!("album:{}", album.id)) {
            items.push(YandexMusicSearchItem::Album(Box::new(album)));
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRecommendationBatch {
    #[serde(default)]
    radio_session_id: Option<String>,
    #[serde(default)]
    batch_id: Option<String>,
    #[serde(default)]
    sequence: Vec<RawRecommendationItem>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRecommendationItem {
    #[serde(default, rename = "type")]
    item_type: Option<String>,
    #[serde(default)]
    liked: Option<bool>,
    #[serde(default)]
    disliked: Option<bool>,
    #[serde(default)]
    track: Option<RawTrack>,
}

fn normalize_recommendation_batch(
    raw: RawRecommendationBatch,
    fallback_session_id: Option<&str>,
) -> Result<YandexMusicRecommendationBatch, ProviderError> {
    if raw.sequence.len() > MAX_RECOMMENDATIONS {
        return Err(ProviderError::InvalidResponse(format!(
            "Yandex Music returned more than {MAX_RECOMMENDATIONS} recommendations"
        )));
    }
    let session_id = raw
        .radio_session_id
        .as_deref()
        .or(fallback_session_id)
        .and_then(|value| {
            validate_identifier(value, "recommendation session")
                .ok()
                .map(str::to_owned)
        })
        .ok_or_else(|| {
            ProviderError::InvalidResponse(
                "Yandex Music recommendation omitted its session id".to_owned(),
            )
        })?;
    let batch_id = normalize_optional_recommendation_batch_id(raw.batch_id.as_deref())?;
    let mut seen = HashSet::new();
    let mut tracks = Vec::new();
    for item in raw.sequence {
        if item
            .item_type
            .as_deref()
            .is_some_and(|item_type| !item_type.eq_ignore_ascii_case("track"))
        {
            continue;
        }
        let Some(raw_track) = item.track else {
            continue;
        };
        if raw_track.available == Some(false) {
            continue;
        }
        let kind = exact_track_kind(&raw_track, item.item_type.as_deref());
        let Some(mut track) = normalize_track(&raw_track, kind, None) else {
            continue;
        };
        track.reaction = reaction_from_flags(item.liked, item.disliked);
        if seen.insert(track.id.clone()) {
            tracks.push(YandexMusicRecommendedTrack {
                track,
                batch_id: batch_id.clone(),
            });
        }
    }
    Ok(YandexMusicRecommendationBatch {
        session_id,
        batch_id,
        tracks,
    })
}

fn normalize_album_with_tracks(raw: RawAlbum) -> Result<YandexMusicAlbum, ProviderError> {
    if raw.available == Some(false) {
        return Err(ProviderError::InvalidResponse(
            "Yandex Music album is unavailable".to_owned(),
        ));
    }
    if raw.volumes.len() > MAX_ALBUM_VOLUMES {
        return Err(ProviderError::InvalidResponse(format!(
            "Yandex Music album returned more than {MAX_ALBUM_VOLUMES} volumes"
        )));
    }
    let track_count = raw
        .volumes
        .iter()
        .map(Vec::len)
        .fold(0usize, usize::saturating_add);
    if track_count > MAX_ALBUM_TRACKS {
        return Err(ProviderError::InvalidResponse(format!(
            "Yandex Music album returned more than {MAX_ALBUM_TRACKS} tracks"
        )));
    }
    let kind = exact_album_kind(&raw, raw.item_type.as_deref());
    let summary = normalize_album_summary(&raw, kind).ok_or_else(|| {
        ProviderError::InvalidResponse("Yandex Music album omitted a valid id or title".to_owned())
    })?;
    let mut tracks = Vec::with_capacity(track_count);
    for (volume_index, volume) in raw.volumes.into_iter().enumerate() {
        for (track_index, raw_track) in volume.into_iter().enumerate() {
            let track_kind =
                exact_track_kind_with_fallback(&raw_track, None, Some(summary.content_kind));
            let Some(track) = normalize_track(&raw_track, track_kind, Some(summary.clone())) else {
                continue;
            };
            tracks.push(YandexMusicAlbumTrack {
                volume_number: u32::try_from(volume_index + 1).unwrap_or(u32::MAX),
                track_number: u32::try_from(track_index + 1).unwrap_or(u32::MAX),
                track,
            });
        }
    }
    Ok(YandexMusicAlbum { summary, tracks })
}

fn normalize_artist_page(
    raw: RawArtistBriefInfo,
    requested_artist_id: &str,
) -> Result<YandexMusicArtistPage, ProviderError> {
    if raw.popular_tracks.len() > MAX_REMOTE_SEARCH_ITEMS {
        return Err(ProviderError::InvalidResponse(format!(
            "Yandex Music artist returned more than {MAX_REMOTE_SEARCH_ITEMS} popular tracks"
        )));
    }
    if raw.albums.len() > MAX_REMOTE_SEARCH_ITEMS {
        return Err(ProviderError::InvalidResponse(format!(
            "Yandex Music artist returned more than {MAX_REMOTE_SEARCH_ITEMS} albums"
        )));
    }

    let returned_artist_id = raw
        .artist
        .id
        .as_ref()
        .and_then(json_identifier)
        .filter(|artist_id| validate_numeric_identifier(artist_id, "artist").is_ok())
        .filter(|artist_id| artist_id == requested_artist_id)
        .ok_or_else(|| {
            ProviderError::InvalidResponse(
                "Yandex Music artist identity is missing, malformed, or mismatched".to_owned(),
            )
        })?;
    let artist_name = raw
        .artist
        .name
        .as_deref()
        .and_then(|name| bounded_text(name, MAX_NAME_BYTES))
        .ok_or_else(|| {
            ProviderError::InvalidResponse(
                "Yandex Music artist omitted a valid display name".to_owned(),
            )
        })?;
    let artist = YandexMusicArtist {
        id: Some(returned_artist_id),
        name: artist_name,
    };

    let mut seen_tracks = HashSet::new();
    let mut popular_tracks = Vec::with_capacity(raw.popular_tracks.len());
    for raw_track in raw.popular_tracks {
        let kind = exact_track_kind(&raw_track, None);
        let Some(track) = normalize_track(&raw_track, kind, None) else {
            continue;
        };
        if seen_tracks.insert(track.id.clone()) {
            popular_tracks.push(track);
        }
    }

    let mut seen_albums = HashSet::new();
    let mut albums = Vec::with_capacity(raw.albums.len());
    for raw_album in raw.albums {
        let kind = exact_album_kind(&raw_album, None);
        let Some(album) = normalize_album_summary(&raw_album, kind) else {
            continue;
        };
        if seen_albums.insert(album.id.clone()) {
            albums.push(album);
        }
    }

    Ok(YandexMusicArtistPage {
        artist,
        popular_tracks,
        albums,
    })
}

fn normalize_track(
    raw: &RawTrack,
    kind: YandexMusicContentKind,
    fallback_album: Option<YandexMusicAlbumSummary>,
) -> Option<YandexMusicTrack> {
    if raw.available == Some(false) {
        return None;
    }
    let id = normalized_track_identifier(raw.real_id.as_ref())
        .or_else(|| normalized_track_identifier(raw.id.as_ref()))?;
    let title = raw
        .title
        .as_deref()
        .and_then(|value| bounded_text(value, MAX_TITLE_BYTES))
        .unwrap_or_else(|| "Untitled track".to_owned());
    let artists = normalize_artists_ref(&raw.artists);
    let album = raw
        .albums
        .first()
        .and_then(|album| normalize_album_summary(album, exact_album_kind(album, None)))
        .or(fallback_album);
    let duration_ms = raw.duration_ms.as_ref().and_then(json_u64);
    let artwork_url = raw
        .cover_uri
        .as_deref()
        .or(raw.og_image.as_deref())
        .and_then(normalize_artwork_url);
    let webpage_url = track_webpage_url(&id, album.as_ref().map(|album| album.id.as_str()))?;
    Some(YandexMusicTrack {
        id,
        title,
        artists,
        album,
        duration_ms,
        artwork_url,
        content_kind: kind,
        reaction: YandexMusicReaction::Neutral,
        webpage_url,
    })
}

fn normalized_track_identifier(value: Option<&Value>) -> Option<String> {
    value
        .and_then(json_identifier)
        .and_then(|value| validate_identifier(&value, "track").ok().map(str::to_owned))
}

fn normalize_album_summary(
    raw: &RawAlbum,
    kind: YandexMusicContentKind,
) -> Option<YandexMusicAlbumSummary> {
    if raw.available == Some(false) {
        return None;
    }
    let id = raw
        .id
        .as_ref()
        .and_then(json_identifier)
        .and_then(|value| validate_identifier(&value, "album").ok().map(str::to_owned))?;
    let title = raw
        .title
        .as_deref()
        .and_then(|value| bounded_text(value, MAX_TITLE_BYTES))?;
    let artwork_url = raw
        .cover_uri
        .as_deref()
        .or(raw.og_image.as_deref())
        .and_then(normalize_artwork_url);
    Some(YandexMusicAlbumSummary {
        webpage_url: album_webpage_url(&id)?,
        id,
        title,
        artists: normalize_artists_ref(&raw.artists),
        content_kind: kind,
        artwork_url,
    })
}

fn normalize_artists_ref(raw: &[RawArtist]) -> Vec<YandexMusicArtist> {
    raw.iter()
        .take(32)
        .filter_map(|artist| {
            let name = artist
                .name
                .as_deref()
                .and_then(|value| bounded_text(value, MAX_NAME_BYTES))?;
            let id = artist
                .id
                .as_ref()
                .and_then(json_identifier)
                .and_then(|value| {
                    validate_identifier(&value, "artist")
                        .ok()
                        .map(str::to_owned)
                });
            Some(YandexMusicArtist { id, name })
        })
        .collect()
}

fn bounded_text(value: &str, max_bytes: usize) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > max_bytes
        || value
            .chars()
            .any(|character| character.is_control() && character != '\n')
    {
        return None;
    }
    Some(value.replace(['\r', '\n'], " "))
}

fn exact_track_kind(raw: &RawTrack, bucket_type: Option<&str>) -> YandexMusicContentKind {
    exact_track_kind_with_fallback(raw, bucket_type, None)
}

fn exact_track_kind_with_fallback(
    raw: &RawTrack,
    bucket_type: Option<&str>,
    fallback: Option<YandexMusicContentKind>,
) -> YandexMusicContentKind {
    exact_content_kind(
        [
            raw.item_type.as_deref(),
            raw.albums
                .first()
                .and_then(|album| album.meta_type.as_deref()),
            raw.albums
                .first()
                .and_then(|album| album.item_type.as_deref()),
            bucket_type,
        ]
        .into_iter()
        .flatten(),
    )
    .or(fallback)
    .unwrap_or(YandexMusicContentKind::Music)
}

fn exact_album_kind(raw: &RawAlbum, bucket_type: Option<&str>) -> YandexMusicContentKind {
    exact_content_kind(
        [
            raw.item_type.as_deref(),
            // Live audiobook albums report `type=audiobook` together with the
            // legacy transport grouping `metaType=podcast`. The explicit item
            // type must win over that compatibility label.
            raw.meta_type.as_deref(),
            bucket_type,
        ]
        .into_iter()
        .flatten(),
    )
    .unwrap_or(YandexMusicContentKind::Music)
}

fn exact_content_kind<'a>(
    values: impl IntoIterator<Item = &'a str>,
) -> Option<YandexMusicContentKind> {
    for value in values {
        match value.trim().to_ascii_lowercase().as_str() {
            "audiobook" | "audiobook_episode" | "audiobook-episode" | "audio_book"
            | "audio_book_episode" | "audio-book" | "audio-book-episode" => {
                return Some(YandexMusicContentKind::Audiobook);
            }
            "podcast" | "podcast_episode" | "podcast-episode" => {
                return Some(YandexMusicContentKind::Podcast);
            }
            "music" => return Some(YandexMusicContentKind::Music),
            _ => {}
        }
    }
    None
}

fn reaction_from_flags(liked: Option<bool>, disliked: Option<bool>) -> YandexMusicReaction {
    match (liked == Some(true), disliked == Some(true)) {
        (true, false) => YandexMusicReaction::Liked,
        (false, true) => YandexMusicReaction::Disliked,
        _ => YandexMusicReaction::Neutral,
    }
}

fn json_identifier(value: &Value) -> Option<String> {
    match value {
        Value::String(value) if !value.trim().is_empty() => Some(value.trim().to_owned()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn json_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(value) => value.as_u64(),
        Value::String(value) => value.trim().parse().ok(),
        _ => None,
    }
}

fn album_webpage_url(album_id: &str) -> Option<Url> {
    let base = Url::parse(WEB_ORIGIN).ok()?;
    base.join(&format!("album/{album_id}")).ok()
}

fn artist_webpage_url(artist_id: &str) -> Option<Url> {
    let artist_id = validate_identifier(artist_id, "artist").ok()?;
    let base = Url::parse(WEB_ORIGIN).ok()?;
    base.join(&format!("artist/{artist_id}")).ok()
}

fn track_webpage_url(track_id: &str, album_id: Option<&str>) -> Option<Url> {
    let base = Url::parse(WEB_ORIGIN).ok()?;
    match album_id {
        Some(album_id) => base
            .join(&format!("album/{album_id}/track/{track_id}"))
            .ok(),
        None => base.join(&format!("track/{track_id}")).ok(),
    }
}

fn normalize_artwork_url(value: &str) -> Option<Url> {
    let expanded = value.trim().replace("%%", ARTWORK_PREVIEW_SIZE);
    let raw = if expanded.starts_with("//") {
        format!("https:{expanded}")
    } else if expanded.starts_with("http://") {
        format!("https://{}", expanded.trim_start_matches("http://"))
    } else if expanded.starts_with("https://") {
        expanded
    } else {
        format!("https://{expanded}")
    };
    let url = Url::parse(&raw).ok()?;
    if is_allowed_artwork_url(&url) {
        Some(url)
    } else {
        None
    }
}

fn expanded_artwork_url(preview_url: &Url) -> Option<Url> {
    rewrite_artwork_variant(preview_url, ARTWORK_EXPANDED_VARIANT)
}

fn panel_artwork_url(preview_url: &Url, size: YandexMusicArtworkSize) -> Option<Url> {
    rewrite_artwork_variant(preview_url, size.path_variant())
}

/// Rewrites only a validated numeric Yandex artwork variant.
fn rewrite_artwork_variant(preview_url: &Url, variant: &str) -> Option<Url> {
    if !is_allowed_artwork_url(preview_url) {
        return None;
    }
    let path = preview_url.path();
    let size = path.rsplit('/').next()?;
    let (width, height) = size.split_once('x')?;
    if width.is_empty()
        || height.is_empty()
        || !width.bytes().all(|byte| byte.is_ascii_digit())
        || !height.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let prefix = path.strip_suffix(size)?;
    let mut expanded = preview_url.clone();
    expanded.set_path(&format!("{prefix}{variant}"));
    is_allowed_artwork_url(&expanded).then_some(expanded)
}

fn is_allowed_artwork_url(url: &Url) -> bool {
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|port| port != 443)
        || url.fragment().is_some()
    {
        return false;
    }
    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    ARTWORK_HOSTS.contains(&host.as_str())
        && (url.path().starts_with("/get-music-content/") || url.path().starts_with("/get/"))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawFileInfo {
    #[serde(default)]
    bitrate: Option<Value>,
    #[serde(default)]
    codec: Option<String>,
    #[serde(default)]
    quality: Option<String>,
    #[serde(default)]
    size: Option<Value>,
    #[serde(default)]
    file_size: Option<Value>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    urls: Vec<String>,
    #[serde(default)]
    key: Option<String>,
}

fn parse_media_response(bytes: &[u8]) -> Result<YandexMusicMedia, ProviderError> {
    let root = serde_json::from_slice::<Value>(bytes)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    if let Some(error) = root.get("error") {
        let error =
            serde_json::from_value::<RawServiceError>(error.clone()).unwrap_or(RawServiceError {
                name: None,
                message: None,
            });
        return Err(service_error(&error));
    }
    let info_value = root
        .pointer("/result/downloadInfo")
        .or_else(|| root.pointer("/result/download_info"))
        .or_else(|| root.get("downloadInfo"))
        .or_else(|| root.get("download_info"))
        .ok_or_else(|| {
            ProviderError::InvalidResponse("Yandex Music file info omitted downloadInfo".to_owned())
        })?;
    let info = serde_json::from_value::<RawFileInfo>(info_value.clone())
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    normalize_media(info)
}

fn normalize_media(info: RawFileInfo) -> Result<YandexMusicMedia, ProviderError> {
    let codec = info
        .codec
        .as_deref()
        .and_then(YandexMusicCodec::from_api)
        .ok_or_else(|| {
            ProviderError::InvalidResponse(
                "Yandex Music file info returned an unsupported codec".to_owned(),
            )
        })?;
    let quality = info
        .quality
        .as_deref()
        .and_then(YandexMusicQuality::from_api)
        .ok_or_else(|| {
            ProviderError::InvalidResponse(
                "Yandex Music file info omitted its actual quality".to_owned(),
            )
        })?;
    let url = info
        .url
        .into_iter()
        .chain(info.urls)
        .filter_map(|value| Url::parse(value.trim()).ok())
        .find(is_allowed_media_url)
        .ok_or_else(|| {
            ProviderError::InvalidResponse(
                "Yandex Music file info omitted a safe media URL".to_owned(),
            )
        })?;
    let decryption_key = info
        .key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(decode_hex_key)
        .transpose()?;
    Ok(YandexMusicMedia {
        url,
        codec,
        quality,
        bitrate_kbps: info
            .bitrate
            .as_ref()
            .and_then(json_u64)
            .and_then(|value| u32::try_from(value).ok()),
        size_bytes: info
            .size
            .as_ref()
            .or(info.file_size.as_ref())
            .and_then(json_u64),
        decryption_key,
    })
}

/// Returns whether a resolved URL remains inside the credential-free Yandex
/// CDN boundary.
pub(super) fn is_allowed_media_url(url: &Url) -> bool {
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|port| port != 443)
        || url.fragment().is_some()
    {
        return false;
    }
    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    MEDIA_HOST_SUFFIXES
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
}

fn decode_hex_key(value: &str) -> Result<Vec<u8>, ProviderError> {
    let bytes = value.as_bytes();
    if !matches!(bytes.len(), 32 | 48 | 64) {
        return Err(ProviderError::InvalidResponse(
            "Yandex Music file info returned an invalid media key".to_owned(),
        ));
    }
    let mut decoded = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let high = hex_nibble(pair[0]).ok_or_else(invalid_media_key)?;
        let low = hex_nibble(pair[1]).ok_or_else(invalid_media_key)?;
        decoded.push((high << 4) | low);
    }
    Ok(decoded)
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn invalid_media_key() -> ProviderError {
    ProviderError::InvalidResponse(
        "Yandex Music file info returned an invalid media key".to_owned(),
    )
}

fn quality_is_unavailable(error: &ProviderError) -> bool {
    matches!(
        error,
        ProviderError::HttpStatus(400 | 404 | 409 | 422)
            | ProviderError::Service {
                status: 400 | 404 | 409 | 422,
                ..
            }
    )
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;

    use super::*;

    enum FixtureResponse {
        Bytes(Vec<u8>),
        Error(ProviderError),
    }

    struct FixtureTransport {
        responses: Mutex<VecDeque<FixtureResponse>>,
        requests: Mutex<Vec<YandexMusicTransportRequest>>,
    }

    impl FixtureTransport {
        fn new(responses: impl IntoIterator<Item = FixtureResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn one_json(json: impl Into<Vec<u8>>) -> Self {
            Self::new([FixtureResponse::Bytes(json.into())])
        }

        fn requests(&self) -> Vec<YandexMusicTransportRequest> {
            self.requests.lock().expect("fixture requests").clone()
        }
    }

    impl YandexMusicTransport for FixtureTransport {
        fn execute(
            &self,
            request: &YandexMusicTransportRequest,
            oauth_token: &str,
        ) -> Result<Vec<u8>, ProviderError> {
            assert_eq!(oauth_token, "fixture-oauth-token");
            self.requests
                .lock()
                .expect("fixture requests")
                .push(request.clone());
            match self
                .responses
                .lock()
                .expect("fixture responses")
                .pop_front()
                .expect("fixture response")
            {
                FixtureResponse::Bytes(bytes) => Ok(bytes),
                FixtureResponse::Error(error) => Err(error),
            }
        }
    }

    fn fixture_client(
        transport: Arc<FixtureTransport>,
        max_json_bytes: usize,
    ) -> YandexMusicClient {
        YandexMusicClient::with_transport("fixture-oauth-token", transport, max_json_bytes)
            .expect("fixture client")
    }

    fn fixture_client_with_account(
        transport: Arc<FixtureTransport>,
        max_json_bytes: usize,
    ) -> YandexMusicClient {
        let client = fixture_client(transport, max_json_bytes);
        client
            .account_uid
            .set("fixture-account".to_owned())
            .expect("fixture account is initialized once");
        client
    }

    fn json_response(value: &Value) -> FixtureResponse {
        FixtureResponse::Bytes(serde_json::to_vec(&value).expect("fixture JSON"))
    }

    fn recommendation_response(
        session_id: Option<&str>,
        batch_id: &str,
        track_ids: &[&str],
    ) -> FixtureResponse {
        let mut result = json!({
            "batchId": batch_id,
            "sequence": track_ids
                .iter()
                .map(|track_id| json!({
                    "type": "track",
                    "track": {
                        "id": track_id,
                        "title": format!("Track {track_id}"),
                        "available": true
                    }
                }))
                .collect::<Vec<_>>()
        });
        if let Some(session_id) = session_id {
            result["radioSessionId"] = json!(session_id);
        }
        json_response(&json!({ "result": result }))
    }

    #[test]
    fn account_validation_accepts_numeric_ids_without_exposing_the_token() {
        let transport = Arc::new(FixtureTransport::one_json(
            br#"{
                "result": {
                    "account": {
                        "uid": 123456,
                        "displayName": "Fixture Listener",
                        "serviceAvailable": true
                    }
                }
            }"#,
        ));
        let client = fixture_client(transport.clone(), 16 * 1024);

        let account = client.validate_account().expect("valid account");

        assert_eq!(account.uid, "123456");
        assert_eq!(account.display_name.as_deref(), Some("Fixture Listener"));
        assert_eq!(account.service_available, Some(true));
        assert!(!format!("{client:?}").contains("fixture-oauth-token"));
        let requests = transport.requests();
        assert_eq!(requests[0].method(), "GET");
        assert_eq!(requests[0].url().path(), "/account/status");
        assert!(requests[0].body().is_none());
    }

    #[test]
    fn token_validation_rejects_whitespace_controls_and_excessive_input() {
        let transport = Arc::new(FixtureTransport::new([]));
        for token in [
            "",
            " token",
            "token ",
            "two words",
            "token\nvalue",
            "token\tvalue",
        ] {
            assert!(
                YandexMusicClient::with_transport(token, transport.clone(), 1_024).is_err(),
                "{token:?} must be rejected"
            );
        }
        assert!(
            YandexMusicClient::with_transport("x".repeat(MAX_TOKEN_BYTES + 1), transport, 1_024)
                .is_err()
        );
    }

    #[test]
    fn file_info_signature_matches_the_current_web_protocol() {
        assert_eq!(
            sign_file_info("12345", 1_700_000_000, "lossless"),
            "cIr27Nz/vx8itCxjo2MQwhi49eA5o8WpLN2GAbUCgW0"
        );
    }

    #[test]
    fn media_resolution_validates_and_caches_the_file_info_account() {
        let file_info = json!({
            "result": {
                "downloadInfo": {
                    "quality": "lossless",
                    "codec": "flac",
                    "url": "https://audio.storage.yandex.net/current.flac"
                }
            }
        });
        let transport = Arc::new(FixtureTransport::new([
            json_response(&json!({
                "result": {"account": {"uid": "account-current"}}
            })),
            json_response(&file_info),
            json_response(&file_info),
        ]));
        let client = fixture_client(transport.clone(), 16 * 1024);

        client.resolve_media("123").expect("first resolution");
        client.resolve_media("456").expect("second resolution");

        let requests = transport.requests();
        assert_eq!(
            requests
                .iter()
                .map(|request| request.url().path())
                .collect::<Vec<_>>(),
            ["/account/status", "/get-file-info", "/get-file-info"]
        );
        for request in &requests[1..] {
            assert_eq!(
                request.profile(),
                &RequestProfile::FileInfo {
                    account_uid: "account-current".to_owned()
                }
            );
        }
    }

    #[test]
    fn initial_my_wave_is_bounded_deduplicated_and_skips_malformed_nodes() {
        let transport = Arc::new(FixtureTransport::new([FixtureResponse::Bytes(
            br#"{
                "result": {
                    "radioSessionId": "session-1",
                    "batchId": "batch-1",
                    "sequence": [
                        {
                            "type": "track",
                            "liked": true,
                            "track": {
                                "id": 11,
                                "title": "One",
                                "available": true,
                                "durationMs": "1234",
                                "artists": [{"id": 5, "name": "Artist"}]
                            }
                        },
                        {
                            "type": "track",
                            "track": {"id": {"bad": true}, "title": "Malformed"}
                        },
                        {
                            "type": "track",
                            "track": {"id": "12", "title": "Unavailable", "available": false}
                        },
                        {
                            "type": "track",
                            "track": {"id": "11", "title": "Duplicate"}
                        },
                        {
                            "type": "ad",
                            "track": {"id": "13", "title": "Advertisement"}
                        }
                    ]
                }
            }"#
            .to_vec(),
        )]));
        let client = fixture_client(transport.clone(), 32 * 1024);

        let batch = client.start_my_wave().expect("recommendations");

        assert_eq!(batch.session_id, "session-1");
        assert_eq!(batch.batch_id.as_deref(), Some("batch-1"));
        assert_eq!(batch.tracks.len(), 1);
        assert_eq!(batch.tracks[0].track.id, "11");
        assert_eq!(batch.tracks[0].track.duration_ms, Some(1_234));
        assert_eq!(batch.tracks[0].track.reaction, YandexMusicReaction::Liked);
        let requests = transport.requests();
        assert_eq!(requests[0].method(), "POST");
        assert_eq!(requests[0].url().path(), "/rotor/session/new");
        let body: Value =
            serde_json::from_slice(requests[0].body().expect("JSON body")).expect("JSON");
        assert_eq!(body["seeds"], json!([MY_WAVE_SEED]));
        assert_eq!(body["includeTracksInResponse"], true);
        assert_eq!(
            requests.len(),
            1,
            "normalizing a short batch must not trigger another request"
        );
    }

    #[test]
    fn recommendation_batch_without_feedback_id_remains_playable() {
        for batch_id in [None, Some("")] {
            let mut result = json!({
                "radioSessionId": "session-without-batch",
                "sequence": [{
                    "type": "track",
                    "track": {
                        "id": "1",
                        "title": "Playable without feedback identity",
                        "available": true
                    }
                }]
            });
            if let Some(batch_id) = batch_id {
                result["batchId"] = json!(batch_id);
            }
            let raw: RawRecommendationBatch =
                serde_json::from_value(result).expect("recommendation fixture");

            let batch = normalize_recommendation_batch(raw, None)
                .expect("batchId is optional for playback and continuation");

            assert_eq!(batch.session_id, "session-without-batch");
            assert_eq!(batch.batch_id, None);
            assert_eq!(batch.tracks.len(), 1);
            assert_eq!(batch.tracks[0].track.id, "1");
            assert_eq!(batch.tracks[0].batch_id, None);
        }
    }

    #[test]
    fn recommendation_batch_rejects_a_present_malformed_feedback_id() {
        let raw: RawRecommendationBatch = serde_json::from_value(json!({
            "radioSessionId": "session-with-invalid-batch",
            "batchId": "batch\nidentity",
            "sequence": []
        }))
        .expect("recommendation fixture");

        assert!(matches!(
            normalize_recommendation_batch(raw, None),
            Err(ProviderError::InvalidResponse(message))
                if message == "Yandex Music recommendation returned an invalid batch id"
        ));
    }

    #[test]
    fn recommendation_batch_accepts_a_bounded_opaque_feedback_id() {
        let feedback_id = "batch/2026.07+token=value~opaque";
        let raw: RawRecommendationBatch = serde_json::from_value(json!({
            "radioSessionId": "session-with-opaque-batch",
            "batchId": feedback_id,
            "sequence": []
        }))
        .expect("recommendation fixture");

        let batch = normalize_recommendation_batch(raw, None)
            .expect("provider-owned feedback identity is opaque");

        assert_eq!(batch.batch_id.as_deref(), Some(feedback_id));
    }

    #[test]
    fn continuation_without_feedback_id_does_not_borrow_the_initial_id() {
        let continuation_tracks = (2..=20)
            .map(|track_id| {
                json!({
                    "type": "track",
                    "track": {
                        "id": track_id.to_string(),
                        "title": format!("Track {track_id}"),
                        "available": true
                    }
                })
            })
            .collect::<Vec<_>>();
        let transport = Arc::new(FixtureTransport::new([
            recommendation_response(Some("session-fill"), "batch-initial", &["1"]),
            json_response(&json!({
                "result": {
                    "sequence": continuation_tracks
                }
            })),
        ]));
        let client = fixture_client(transport.clone(), 32 * 1024);

        let initial = client.start_my_wave().expect("initial recommendations");
        let continuation = client
            .more_my_wave(&initial.session_id, &["1".to_owned()])
            .expect("explicit continuation does not require batchId");

        assert_eq!(initial.batch_id.as_deref(), Some("batch-initial"));
        assert_eq!(initial.tracks.len(), 1);
        assert_eq!(initial.tracks[0].batch_id.as_deref(), Some("batch-initial"));
        assert_eq!(continuation.tracks.len(), 19);
        assert!(
            continuation
                .tracks
                .iter()
                .all(|recommended| recommended.batch_id.is_none()),
            "a continuation without batchId must not inherit another page's feedback identity"
        );
        assert_eq!(transport.requests().len(), 2);
    }

    #[test]
    fn my_wave_uses_at_most_four_requests_to_fill_twenty_unique_tracks() {
        let transport = Arc::new(FixtureTransport::new([
            recommendation_response(Some("session-fill"), "batch-0", &["1", "2", "3", "4", "5"]),
            recommendation_response(None, "batch-1", &["6", "7", "8", "9", "10"]),
            recommendation_response(None, "batch-2", &["11", "12", "13", "14", "15"]),
            recommendation_response(None, "batch-3", &["16", "17", "18", "19", "20"]),
        ]));
        let client = fixture_client(transport.clone(), 32 * 1024);

        let batch = client.my_wave().expect("assembled recommendations");

        assert_eq!(batch.session_id, "session-fill");
        assert_eq!(batch.batch_id.as_deref(), Some("batch-0"));
        assert_eq!(
            batch
                .tracks
                .iter()
                .map(|recommended| recommended.track.id.as_str())
                .collect::<Vec<_>>(),
            [
                "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15",
                "16", "17", "18", "19", "20"
            ]
        );
        assert_eq!(batch.tracks[0].batch_id.as_deref(), Some("batch-0"));
        assert_eq!(batch.tracks[5].batch_id.as_deref(), Some("batch-1"));
        assert_eq!(batch.tracks[19].batch_id.as_deref(), Some("batch-3"));
        assert_eq!(
            transport.requests().len(),
            4,
            "loading My Wave must use one initial request and at most three continuations"
        );
    }

    #[test]
    fn my_wave_stops_when_a_continuation_adds_no_unique_tracks() {
        let transport = Arc::new(FixtureTransport::new([
            recommendation_response(
                Some("session-stalled"),
                "batch-0",
                &["1", "2", "3", "4", "5"],
            ),
            recommendation_response(None, "batch-1", &["1", "2", "3", "4", "5"]),
        ]));
        let client = fixture_client(transport.clone(), 32 * 1024);

        let batch = client.my_wave().expect("stalled recommendations");

        assert_eq!(batch.tracks.len(), 5);
        assert_eq!(transport.requests().len(), 2);
    }

    #[test]
    fn my_wave_never_exceeds_four_requests_when_twenty_tracks_are_unavailable() {
        let transport = Arc::new(FixtureTransport::new([
            recommendation_response(Some("session-short"), "batch-0", &["1", "2", "3", "4", "5"]),
            recommendation_response(None, "batch-1", &["6", "7", "8", "9", "10"]),
            recommendation_response(None, "batch-2", &["11", "12", "13", "14", "15"]),
            recommendation_response(None, "batch-3", &["16", "17", "18"]),
            recommendation_response(None, "batch-4", &["19", "20"]),
        ]));
        let client = fixture_client(transport.clone(), 32 * 1024);

        let batch = client.my_wave().expect("bounded recommendations");

        assert_eq!(batch.tracks.len(), 18);
        assert_eq!(transport.requests().len(), 4);
    }

    #[test]
    fn my_wave_truncates_an_oversized_initial_page_to_the_default() {
        let track_ids = (1_u8..=25).map(|id| id.to_string()).collect::<Vec<_>>();
        let borrowed_ids = track_ids.iter().map(String::as_str).collect::<Vec<_>>();
        let transport = Arc::new(FixtureTransport::new([recommendation_response(
            Some("session-large"),
            "batch-large",
            &borrowed_ids,
        )]));
        let client = fixture_client(transport.clone(), 64 * 1024);

        let batch = client.my_wave().expect("bounded recommendations");

        assert_eq!(batch.tracks.len(), DEFAULT_MY_WAVE_RECOMMENDATIONS);
        assert_eq!(
            batch.tracks.last().map(|item| item.track.id.as_str()),
            Some("20")
        );
        assert_eq!(transport.requests().len(), 1);
    }

    #[test]
    fn continued_my_wave_validates_and_encodes_the_recent_queue() {
        let transport = Arc::new(FixtureTransport::one_json(
            br#"{"result":{"batchId":"batch-2","sequence":[]}}"#,
        ));
        let client = fixture_client(transport.clone(), 4 * 1024);

        let batch = client
            .more_my_wave("session:one", &["11:22".to_owned(), "33".to_owned()])
            .expect("continued recommendations");

        assert_eq!(batch.session_id, "session:one");
        assert!(batch.tracks.is_empty());
        let request = &transport.requests()[0];
        assert_eq!(request.url().path(), "/rotor/session/session:one/tracks");
        let body: Value =
            serde_json::from_slice(request.body().expect("queue body")).expect("queue JSON");
        assert_eq!(body["queue"], json!(["11:22", "33"]));
    }

    #[test]
    fn unicode_search_is_encoded_and_audiobooks_use_only_exact_metadata() {
        let transport = Arc::new(FixtureTransport::one_json(
            br#"{
                "result": {
                    "responseType": "mixed",
                    "results": [
                        {
                            "type": "podcast_episode",
                            "podcast_episode": {
                                "id": "1",
                                "title": "Audiobook in the title is still music",
                                "type": "track",
                                "albums": [{"id": "10", "title": "Music", "metaType": "music"}]
                            }
                        },
                        {
                            "type": "podcast_episode",
                            "podcast_episode": {
                                "id": "2",
                                "title": "Exact audiobook chapter",
                                "type": "audiobook_episode",
                                "albums": [{"id": "20", "title": "Book", "metaType": "audiobook"}]
                            }
                        },
                        {
                            "type": "podcast_episode",
                            "podcast_episode": {
                                "id": {"nested": "bad"},
                                "title": "Malformed ID",
                                "type": "audiobook_episode"
                            }
                        },
                        {
                            "type": "podcast",
                            "podcast": {"id": 30, "title": "Exact Book", "type": "audiobook"}
                        },
                        {
                            "type": "podcast",
                            "podcast": {"id": 31, "title": "Audiobook-looking Music", "type": "music"}
                        }
                    ],
                    "perPage": 10,
                    "lastPage": true
                }
            }"#,
        ));
        let client = fixture_client(transport.clone(), 64 * 1024);

        let page = client
            .search("Борис & 世界", YandexMusicSearchScope::Audiobooks, 0, 10)
            .expect("audiobook search");

        assert_eq!(page.items.len(), 2);
        assert!(matches!(
            &page.items[0],
            YandexMusicSearchItem::Track(track)
                if track.id == "2"
                    && track.content_kind == YandexMusicContentKind::Audiobook
        ));
        assert!(matches!(
            &page.items[1],
            YandexMusicSearchItem::Album(album)
                if album.id == "30"
                    && album.content_kind == YandexMusicContentKind::Audiobook
        ));
        let request = &transport.requests()[0];
        assert_eq!(
            request
                .url()
                .query_pairs()
                .find(|(name, _)| name == "text")
                .map(|(_, value)| value.into_owned())
                .as_deref(),
            Some("Борис & 世界")
        );
        let raw_query = request.url().query().expect("encoded query");
        assert!(raw_query.contains("%D0%91"));
        assert!(raw_query.contains("%E4%B8%96%E7%95%8C"));
        assert_eq!(request.url().path(), "/search/instant/mixed");
        assert!(raw_query.contains("type=all"));
        assert!(raw_query.contains("filter=book"));
        assert!(raw_query.contains("pageSize=10"));
        assert!(
            request
                .url()
                .query_pairs()
                .all(|(name, _)| name != "page-size"),
            "the private endpoint has no supported remote page-size parameter"
        );
    }

    #[test]
    fn track_normalization_prefers_valid_real_id_and_falls_back_from_invalid_real_id() {
        let transport = Arc::new(FixtureTransport::one_json(
            br#"{
                "result": {
                    "tracks": {
                        "type": "track",
                        "results": [
                            {
                                "id": "legacy-1",
                                "realId": "canonical-1",
                                "title": "Canonical"
                            },
                            {
                                "id": "fallback-2",
                                "realId": "",
                                "title": "Fallback"
                            }
                        ]
                    }
                }
            }"#,
        ));
        let client = fixture_client(transport, 16 * 1024);

        let page = client
            .search("canonical ids", YandexMusicSearchScope::Music, 0, 10)
            .expect("track search");
        let ids = page
            .items
            .iter()
            .filter_map(|item| match item {
                YandexMusicSearchItem::Track(track) => Some(track.id.as_str()),
                YandexMusicSearchItem::Album(_) => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(ids, ["canonical-1", "fallback-2"]);
        assert!(matches!(
            &page.items[0],
            YandexMusicSearchItem::Track(track)
                if track.webpage_url.path().ends_with("/track/canonical-1")
        ));
    }

    #[test]
    fn duplicate_track_album_keys_are_merged_in_wire_order() {
        let raw = parse_envelope::<RawSearchResponse>(
            br#"{
                "result": {
                    "providerEnvelopeExtension": {"keptOpaque": [1, 2, 3]},
                    "tracks": {
                        "type": "track",
                        "results": [{
                            "id": "1",
                            "title": "Duplicate album keys",
                            "providerTrackExtension": {"keptOpaque": true},
                            "albums": [{"id": "10", "title": "First"}],
                            "albums": [{"id": "11", "title": "Second"}]
                        }]
                    }
                }
            }"#,
        )
        .expect("duplicate track albums are a valid provider quirk");

        let tracks = raw.tracks.expect("track bucket");
        let albums = &tracks.results.first().expect("track").albums;
        assert_eq!(albums.len(), 2);
        assert_eq!(
            albums[0].id.as_ref().and_then(json_identifier).as_deref(),
            Some("10")
        );
        assert_eq!(
            albums[1].id.as_ref().and_then(json_identifier).as_deref(),
            Some("11")
        );
    }

    #[test]
    fn duplicate_search_album_buckets_are_merged_in_wire_order() {
        let raw = parse_envelope::<RawSearchResponse>(
            br#"{
                "result": {
                    "providerEnvelopeExtension": {"keptOpaque": [1, 2, 3]},
                    "albums": {
                        "type": "album",
                        "results": [{"id": "10", "title": "First"}]
                    },
                    "albums": {
                        "type": "album",
                        "results": [{"id": "11", "title": "Second"}]
                    }
                }
            }"#,
        )
        .expect("duplicate search album buckets are a valid provider quirk");

        let albums = raw.albums.expect("merged album bucket");
        assert_eq!(albums.item_type.as_deref(), Some("album"));
        assert_eq!(albums.results.len(), 2);
        assert_eq!(
            albums.results[0]
                .id
                .as_ref()
                .and_then(json_identifier)
                .as_deref(),
            Some("10")
        );
        assert_eq!(
            albums.results[1]
                .id
                .as_ref()
                .and_then(json_identifier)
                .as_deref(),
            Some("11")
        );
    }

    #[test]
    fn duplicate_track_album_keys_keep_the_track_album_bound_effective() {
        let first = (0..=MAX_TRACK_ALBUMS / 2)
            .map(|index| json!({"id": index + 1, "title": "First album reference"}))
            .collect::<Vec<_>>();
        let second = (first.len()..=MAX_TRACK_ALBUMS)
            .map(|index| json!({"id": index + 1, "title": "Second album reference"}))
            .collect::<Vec<_>>();
        let payload = format!(
            r#"{{"result":{{"tracks":{{"results":[{{
                "id":"1",
                "title":"Bounded duplicate albums",
                "albums":{},
                "albums":{}
            }}]}}}}}}"#,
            serde_json::to_string(&first).expect("first track-album fixture"),
            serde_json::to_string(&second).expect("second track-album fixture")
        );

        assert!(matches!(
            parse_envelope::<RawSearchResponse>(payload.as_bytes()),
            Err(ProviderError::InvalidResponse(message))
                if message.contains(&format!("more than {MAX_TRACK_ALBUMS}"))
        ));
    }

    #[test]
    fn duplicate_search_album_buckets_reject_conflicting_metadata() {
        assert!(matches!(
            parse_envelope::<RawSearchResponse>(br#"{
                "result": {
                    "albums": {"type": "album", "results": []},
                    "albums": {"type": "podcast", "results": []}
                }
            }"#),
            Err(ProviderError::InvalidResponse(message))
                if message.contains("conflicting types")
        ));
    }

    #[test]
    fn duplicate_search_album_buckets_keep_the_remote_item_bound_effective() {
        let first = (0..=MAX_REMOTE_SEARCH_ITEMS / 2)
            .map(|index| json!({"id": index + 1, "title": "First page album"}))
            .collect::<Vec<_>>();
        let second = (first.len()..=MAX_REMOTE_SEARCH_ITEMS)
            .map(|index| json!({"id": index + 1, "title": "Second page album"}))
            .collect::<Vec<_>>();
        let payload = format!(
            r#"{{"result":{{
                "albums":{{"type":"album","results":{}}},
                "albums":{{"type":"album","results":{}}}
            }}}}"#,
            serde_json::to_string(&first).expect("first album fixture"),
            serde_json::to_string(&second).expect("second album fixture")
        );
        let transport = Arc::new(FixtureTransport::one_json(payload.into_bytes()));
        let client = fixture_client(transport, 256 * 1024);

        assert!(matches!(
            client.search("bounded duplicate", YandexMusicSearchScope::All, 0, 10),
            Err(ProviderError::InvalidResponse(message))
                if message.contains(&format!("more than {MAX_REMOTE_SEARCH_ITEMS}"))
        ));
    }

    #[test]
    fn podcast_scope_accepts_only_exact_show_and_episode_metadata() {
        let transport = Arc::new(FixtureTransport::one_json(
            br#"{
                "result": {
                    "responseType": "mixed",
                    "results": [
                        {
                            "type": "podcast",
                            "podcast": {"id": "8", "title": "Show", "type": "podcast"}
                        },
                        {
                            "type": "podcast_episode",
                            "podcast_episode": {"id": "7", "title": "Episode", "type": "podcast-episode"}
                        },
                        {
                            "type": "podcast_episode",
                            "podcast_episode": {"id": "6", "title": "Music", "type": "music"}
                        }
                    ],
                    "perPage": 10,
                    "lastPage": true
                }
            }"#,
        ));
        let client = fixture_client(transport.clone(), 16 * 1024);

        let page = client
            .search("fixture", YandexMusicSearchScope::Podcasts, 3, 10)
            .expect("podcast search");

        assert_eq!(page.items.len(), 2);
        assert!(page.items.iter().all(|item| match item {
            YandexMusicSearchItem::Track(track) => {
                track.content_kind == YandexMusicContentKind::Podcast
            }
            YandexMusicSearchItem::Album(album) => {
                album.content_kind == YandexMusicContentKind::Podcast
            }
        }));
        let request = &transport.requests()[0];
        assert_eq!(request.url().path(), "/search/instant/mixed");
        let query = request.url().query_pairs().collect::<HashMap<_, _>>();
        assert_eq!(query.get("type").map(AsRef::as_ref), Some("all"));
        assert_eq!(query.get("filter").map(AsRef::as_ref), Some("podcast"));
        assert_eq!(query.get("pageSize").map(AsRef::as_ref), Some("10"));
    }

    #[test]
    fn explicit_audiobook_type_outweighs_legacy_podcast_meta_type() {
        let transport = Arc::new(FixtureTransport::one_json(
            br#"{
                "result": {
                    "responseType": "mixed",
                    "results": [{
                        "type": "podcast",
                        "podcast": {
                            "id": "24370394",
                            "title": "Arthur Schopenhauer",
                            "type": "audiobook",
                            "metaType": "podcast"
                        }
                    }],
                    "perPage": 10,
                    "lastPage": true
                }
            }"#,
        ));
        let client = fixture_client(transport, 8 * 1024);

        let page = client
            .search("Шопенгауэр", YandexMusicSearchScope::Audiobooks, 0, 10)
            .expect("audiobook search");

        assert!(matches!(
            page.items.as_slice(),
            [YandexMusicSearchItem::Album(album)]
                if album.id == "24370394"
                    && album.content_kind == YandexMusicContentKind::Audiobook
        ));
    }

    #[test]
    fn live_hyphenated_podcast_episode_type_is_exact_metadata() {
        assert_eq!(
            exact_content_kind(["podcast-episode"]),
            Some(YandexMusicContentKind::Podcast)
        );
    }

    #[test]
    fn legacy_all_search_accepts_snake_and_camel_episode_bucket_names() {
        for field_name in ["podcast_episodes", "podcastEpisodes"] {
            let payload = format!(
                r#"{{
                    "result": {{
                        "{field_name}": {{
                            "type": "podcast_episode",
                            "results": [{{"id": "7", "title": "Episode"}}]
                        }}
                    }}
                }}"#
            );
            let transport = Arc::new(FixtureTransport::one_json(payload.into_bytes()));
            let client = fixture_client(transport, 8 * 1024);

            let page = client
                .search("episode", YandexMusicSearchScope::All, 0, 10)
                .expect("podcast episode search");

            assert!(
                matches!(
                    page.items.as_slice(),
                    [YandexMusicSearchItem::Track(track)]
                        if track.id == "7"
                            && track.content_kind == YandexMusicContentKind::Podcast
                ),
                "{field_name} must deserialize as the podcast episode bucket"
            );
        }
    }

    #[test]
    fn exact_audiobook_metadata_wins_over_legacy_podcast_bucket_names() {
        let transport = Arc::new(FixtureTransport::one_json(
            br#"{
                "result": {
                    "podcasts": {
                        "type": "podcast",
                        "results": [
                            {
                                "id": "80",
                                "title": "Book",
                                "metaType": "audiobook"
                            },
                            {
                                "id": "82",
                                "title": "Music release",
                                "metaType": "music"
                            }
                        ]
                    },
                    "podcastEpisodes": {
                        "type": "podcast_episode",
                        "results": [
                            {
                                "id": "81",
                                "title": "Chapter",
                                "type": "audiobook_episode"
                            },
                            {
                                "id": "83",
                                "title": "Music track",
                                "type": "music"
                            }
                        ]
                    }
                }
            }"#,
        ));
        let client = fixture_client(transport, 16 * 1024);

        let page = client
            .search("mixed", YandexMusicSearchScope::All, 0, 10)
            .expect("mixed search");

        let kinds = page
            .items
            .iter()
            .map(|item| match item {
                YandexMusicSearchItem::Track(track) => (track.id.as_str(), track.content_kind),
                YandexMusicSearchItem::Album(album) => (album.id.as_str(), album.content_kind),
            })
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(kinds["80"], YandexMusicContentKind::Audiobook);
        assert_eq!(kinds["81"], YandexMusicContentKind::Audiobook);
        assert_eq!(kinds["82"], YandexMusicContentKind::Music);
        assert_eq!(kinds["83"], YandexMusicContentKind::Music);
    }

    #[test]
    fn empty_search_response_is_a_successful_empty_page() {
        let transport = Arc::new(FixtureTransport::one_json(
            br#"{"result":{"tracks":{"results":[]},"albums":{"results":[]}}}"#,
        ));
        let client = fixture_client(transport, 4 * 1024);

        let page = client
            .search("no results", YandexMusicSearchScope::All, 0, 20)
            .expect("empty search");

        assert!(page.items.is_empty());
        assert_eq!(page.query, "no results");
    }

    #[test]
    fn response_bytes_and_remote_result_counts_have_hard_bounds() {
        let transport = Arc::new(FixtureTransport::one_json(vec![b'x'; 33]));
        let client = fixture_client(transport, 32);
        assert!(matches!(
            client.validate_account(),
            Err(ProviderError::ResponseTooLarge { limit: 32 })
        ));

        let results = (0..=MAX_REMOTE_SEARCH_ITEMS)
            .map(|index| json!({"id": index + 1, "title": "Track"}))
            .collect::<Vec<_>>();
        let transport = Arc::new(FixtureTransport::new([json_response(&json!({
            "result": {"tracks": {"results": results}}
        }))]));
        let client = fixture_client(transport, 256 * 1024);
        assert!(matches!(
            client.search("too many", YandexMusicSearchScope::All, 0, 10),
            Err(ProviderError::InvalidResponse(message))
                if message.contains("more than")
        ));
    }

    #[test]
    fn artist_page_uses_exact_brief_info_path_and_deduplicates_in_wire_order() {
        let transport = Arc::new(FixtureTransport::one_json(
            br#"{
                "result": {
                    "artist": {"id": 42, "name": "Fixture Artist"},
                    "popularTracks": [
                        {"id": "1", "title": "First", "available": true},
                        {"id": "1", "title": "Duplicate", "available": true},
                        {"id": "2", "title": "Second", "available": true},
                        {"id": {"bad": true}, "title": "Malformed"},
                        {"id": "3", "title": "Unavailable", "available": false}
                    ],
                    "albums": [
                        {"id": "10", "title": "First Album", "metaType": "music"},
                        {"id": "10", "title": "Duplicate Album", "metaType": "music"},
                        {"id": "11", "title": "Second Album", "metaType": "music"},
                        {"id": {"bad": true}, "title": "Malformed Album"},
                        {"id": "12", "title": "Unavailable Album", "available": false}
                    ],
                    "similarArtists": [{"id": "999", "name": "Ignored"}],
                    "providerExtension": {"ignored": true}
                }
            }"#,
        ));
        let client = fixture_client(transport.clone(), 32 * 1024);

        let page = client.artist_page("42").expect("artist page");

        assert_eq!(
            page.artist,
            YandexMusicArtist {
                id: Some("42".to_owned()),
                name: "Fixture Artist".to_owned(),
            }
        );
        assert_eq!(
            page.popular_tracks
                .iter()
                .map(|track| track.id.as_str())
                .collect::<Vec<_>>(),
            ["1", "2"]
        );
        assert_eq!(page.popular_tracks[0].title, "First");
        assert_eq!(
            page.albums
                .iter()
                .map(|album| album.id.as_str())
                .collect::<Vec<_>>(),
            ["10", "11"]
        );
        assert_eq!(page.albums[0].title, "First Album");
        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method(), "GET");
        assert_eq!(requests[0].url().path(), "/artists/42/brief-info");
        assert!(requests[0].body().is_none());
    }

    #[test]
    fn artist_page_rejects_malformed_requested_and_mismatched_returned_ids() {
        let malformed_transport = Arc::new(FixtureTransport::new([]));
        let malformed_client = fixture_client(malformed_transport.clone(), 4 * 1024);
        assert!(matches!(
            malformed_client.artist_page("artist-42"),
            Err(ProviderError::InvalidRequest(message))
                if message.contains("artist identifier must be numeric")
        ));
        assert!(malformed_transport.requests().is_empty());

        for returned_artist in [
            json!({"id": 43, "name": "Wrong Artist"}),
            json!({"id": {"bad": true}, "name": "Malformed Artist"}),
        ] {
            let transport = Arc::new(FixtureTransport::new([json_response(&json!({
                "result": {"artist": returned_artist}
            }))]));
            let client = fixture_client(transport.clone(), 4 * 1024);

            assert!(matches!(
                client.artist_page("42"),
                Err(ProviderError::InvalidResponse(message))
                    if message.contains("artist identity")
            ));
            assert_eq!(transport.requests().len(), 1);
        }
    }

    #[test]
    fn artist_page_rejects_oversized_popular_track_and_album_collections() {
        for field in ["popularTracks", "albums"] {
            let items = (0..=MAX_REMOTE_SEARCH_ITEMS)
                .map(|index| json!({"id": index + 1, "title": "Bounded item"}))
                .collect::<Vec<_>>();
            let mut result = json!({
                "artist": {"id": 42, "name": "Fixture Artist"}
            });
            result[field] = json!(items);
            let transport = Arc::new(FixtureTransport::new([json_response(
                &json!({"result": result}),
            )]));
            let client = fixture_client(transport, 256 * 1024);

            assert!(matches!(
                client.artist_page("42"),
                Err(ProviderError::InvalidResponse(message))
                    if message.contains(&format!("more than {MAX_REMOTE_SEARCH_ITEMS}"))
            ));
        }
    }

    #[test]
    fn album_tracks_are_flattened_across_volumes_and_bad_rows_are_skipped() {
        let transport = Arc::new(FixtureTransport::one_json(
            br#"{
                "result": {
                    "id": 99,
                    "title": "Two volumes",
                    "metaType": "music",
                    "artists": [{"id": "3", "name": "Album Artist"}],
                    "volumes": [
                        [
                            {"id": "1", "title": "First"},
                            {"id": {"bad": true}, "title": "Malformed"}
                        ],
                        [
                            {"id": 2, "title": "Second", "durationMs": 2000}
                        ]
                    ]
                }
            }"#,
        ));
        let client = fixture_client(transport.clone(), 32 * 1024);

        let album = client.album_with_tracks("99").expect("album");

        assert_eq!(album.summary.id, "99");
        assert_eq!(album.tracks.len(), 2);
        assert_eq!(
            (
                album.tracks[0].volume_number,
                album.tracks[0].track_number,
                album.tracks[0].track.id.as_str(),
            ),
            (1, 1, "1")
        );
        assert_eq!(
            (
                album.tracks[1].volume_number,
                album.tracks[1].track_number,
                album.tracks[1].track.id.as_str(),
            ),
            (2, 1, "2")
        );
        assert!(
            album
                .tracks
                .iter()
                .all(|track| track.track.album.as_ref() == Some(&album.summary))
        );
        assert_eq!(
            transport.requests()[0].url().path(),
            "/albums/99/with-tracks"
        );
        assert!(client.album_with_tracks("../99").is_err());
    }

    #[test]
    fn sparse_album_children_inherit_the_spoken_word_parent_kind() {
        for meta_type in ["audiobook", "podcast"] {
            let payload = format!(
                r#"{{
                    "result": {{
                        "id": 99,
                        "title": "Spoken word",
                        "metaType": "{meta_type}",
                        "volumes": [[{{"id": "1", "title": "Chapter"}}]]
                    }}
                }}"#
            );
            let transport = Arc::new(FixtureTransport::one_json(payload.into_bytes()));
            let client = fixture_client(transport, 8 * 1024);

            let album = client.album_with_tracks("99").expect("spoken-word album");

            assert_eq!(
                album.tracks[0].track.content_kind, album.summary.content_kind,
                "{meta_type} child must inherit its parent kind"
            );
        }
    }

    #[test]
    fn reaction_mutations_remove_the_opposite_state_before_add_or_neutralize() {
        let transport = Arc::new(FixtureTransport::new(
            (0..6).map(|_| json_response(&json!({"result": {}}))),
        ));
        let client = fixture_client(transport.clone(), 4 * 1024);

        client
            .set_reaction("account-1", "track:album", YandexMusicReaction::Liked)
            .expect("like");
        client
            .set_reaction("account-1", "track:album", YandexMusicReaction::Disliked)
            .expect("dislike");
        client
            .set_reaction("account-1", "track:album", YandexMusicReaction::Neutral)
            .expect("neutral");

        let requests = transport.requests();
        let paths = requests
            .iter()
            .map(|request| request.url().path())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            [
                "/users/account-1/dislikes/tracks/remove",
                "/users/account-1/likes/tracks/add-multiple",
                "/users/account-1/likes/tracks/remove",
                "/users/account-1/dislikes/tracks/add-multiple",
                "/users/account-1/likes/tracks/remove",
                "/users/account-1/dislikes/tracks/remove",
            ]
        );
        assert!(requests.iter().all(|request| {
            request.content_type() == Some("application/x-www-form-urlencoded")
                && request.body() == Some(b"track-ids=track%3Aalbum")
        }));
    }

    #[test]
    fn media_resolution_is_lossless_first_and_accepts_missing_size() {
        let transport = Arc::new(FixtureTransport::one_json(
            br#"{
                "result": {
                    "downloadInfo": {
                        "quality": "lossless",
                        "codec": "flac-mp4",
                        "urls": ["https://audio.storage.yandex.net/path/file?signature=one"],
                        "bitrate": "1411"
                    }
                }
            }"#,
        ));
        let client = fixture_client_with_account(transport.clone(), 16 * 1024);

        let media = client.resolve_media("12345").expect("lossless media");

        assert_eq!(media.codec, YandexMusicCodec::FlacMp4);
        assert_eq!(media.quality, YandexMusicQuality::Lossless);
        assert_eq!(media.bitrate_kbps, Some(1_411));
        assert_eq!(media.size_bytes, None);
        assert_eq!(media.decryption_key(), None);
        let request = &transport.requests()[0];
        assert_eq!(request.url().path(), "/get-file-info");
        let query = request
            .url()
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(query.get("quality").map(AsRef::as_ref), Some("lossless"));
        assert_eq!(query.get("transports").map(AsRef::as_ref), Some("raw"));
        assert_eq!(
            query.get("codecs").map(AsRef::as_ref),
            Some(LOSSLESS_CODECS.join(",").as_str())
        );
        assert!(query.get("sign").is_some_and(|value| !value.is_empty()));
        assert_eq!(
            request.profile(),
            &RequestProfile::FileInfo {
                account_uid: "fixture-account".to_owned()
            }
        );
    }

    #[test]
    fn legacy_high_quality_labels_are_lossy_normal_quality() {
        assert_eq!(
            YandexMusicQuality::from_api("hq"),
            Some(YandexMusicQuality::Normal)
        );
        assert_eq!(
            YandexMusicQuality::from_api("high"),
            Some(YandexMusicQuality::Normal)
        );
    }

    #[test]
    fn unavailable_lossless_falls_back_and_preserves_actual_quality() {
        let transport = Arc::new(FixtureTransport::new([
            FixtureResponse::Error(ProviderError::HttpStatus(404)),
            json_response(&json!({
                "result": {
                    "downloadInfo": {
                        "quality": "nq",
                        "codec": "aac",
                        "url": "https://audio.yandexcdn.net/normal.aac",
                        "fileSize": 1234
                    }
                }
            })),
        ]));
        let client = fixture_client_with_account(transport.clone(), 16 * 1024);

        let media = client.resolve_media("12345").expect("normal fallback");

        assert_eq!(media.codec, YandexMusicCodec::Aac);
        assert_eq!(media.quality, YandexMusicQuality::Normal);
        assert_eq!(media.size_bytes, Some(1_234));
        let qualities = transport
            .requests()
            .iter()
            .map(|request| {
                request
                    .url()
                    .query_pairs()
                    .find(|(name, _)| name == "quality")
                    .map(|(_, value)| value.into_owned())
                    .expect("quality")
            })
            .collect::<Vec<_>>();
        assert_eq!(qualities, ["lossless", "nq"]);
    }

    #[test]
    fn media_resolution_rejects_malformed_or_non_yandex_urls() {
        for url in [
            "http://audio.yandex.net/file.flac",
            "https://evil-yandex.net/file.flac",
            "https://user@audio.yandex.net/file.flac",
            "https://audio.yandex.net:444/file.flac",
            "https://audio.yandex.net/file.flac#fragment",
            "not a URL",
        ] {
            let transport = Arc::new(FixtureTransport::new([json_response(&json!({
                "result": {
                    "downloadInfo": {
                        "quality": "lossless",
                        "codec": "flac",
                        "url": url
                    }
                }
            }))]));
            let client = fixture_client_with_account(transport, 8 * 1024);
            assert!(
                matches!(
                    client.resolve_media("123"),
                    Err(ProviderError::InvalidResponse(message))
                        if message.contains("safe media URL")
                ),
                "{url} must be rejected"
            );
        }
    }

    #[test]
    fn optional_media_key_is_decoded_but_redacted_from_debug_output() {
        let transport = Arc::new(FixtureTransport::one_json(
            br#"{
                "download_info": {
                    "quality": "lossless",
                    "codec": "flac",
                    "url": "https://audio.yandex.net/file.flac",
                    "key": "00112233445566778899aabbccddeeff"
                }
            }"#,
        ));
        let client = fixture_client_with_account(transport, 8 * 1024);

        let media = client.resolve_media("123").expect("encrypted metadata");

        assert_eq!(media.decryption_key().map(<[u8]>::len), Some(16));
        let debug = format!("{media:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("001122"));
    }

    #[test]
    fn media_debug_redacts_signed_url_query_capabilities() {
        let transport = Arc::new(FixtureTransport::one_json(
            br#"{
                "download_info": {
                    "quality": "lossless",
                    "codec": "flac",
                    "url": "https://audio.yandex.net/private/file.flac?sign=super-secret&access_token=bearer-capability"
                }
            }"#,
        ));
        let client = fixture_client_with_account(transport, 8 * 1024);

        let media = client.resolve_media("123").expect("signed media metadata");
        let debug = format!("{media:?}");

        assert!(debug.contains("audio.yandex.net"));
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("super-secret"));
        assert!(!debug.contains("bearer-capability"));
        assert!(!debug.contains("access_token"));
    }

    #[test]
    fn artwork_uses_a_bounded_preview_and_the_original_full_screen_image() {
        let preview =
            normalize_artwork_url("//avatars.yandex.net/get-music-content/fixture/%%?webp=false")
                .expect("validated artwork preview");

        assert_eq!(
            preview.as_str(),
            "https://avatars.yandex.net/get-music-content/fixture/400x400?webp=false"
        );
        assert_eq!(
            panel_artwork_url(&preview, YandexMusicArtworkSize::Large)
                .as_ref()
                .map(Url::as_str),
            Some("https://avatars.yandex.net/get-music-content/fixture/800x800?webp=false")
        );
        assert_eq!(
            panel_artwork_url(&preview, YandexMusicArtworkSize::FourK)
                .as_ref()
                .map(Url::as_str),
            Some("https://avatars.yandex.net/get-music-content/fixture/1000x1000?webp=false")
        );
        assert_eq!(
            expanded_artwork_url(&preview).as_ref().map(Url::as_str),
            Some("https://avatars.yandex.net/get-music-content/fixture/orig?webp=false")
        );
    }

    #[test]
    fn artwork_expansion_rejects_untrusted_or_non_resizable_urls() {
        for raw in [
            "https://example.com/get-music-content/fixture/400x400",
            "https://avatars.yandex.net/unexpected/fixture/400x400",
            "https://avatars.yandex.net/get-music-content/fixture/original.jpg",
            "https://avatars.yandex.net/get-music-content/fixture/400-wide",
        ] {
            let url = Url::parse(raw).expect("fixture URL");
            assert!(
                expanded_artwork_url(&url).is_none(),
                "{raw} must not become a full-screen artwork URL"
            );
            assert!(
                panel_artwork_url(&url, YandexMusicArtworkSize::FourK).is_none(),
                "{raw} must not become a 4K panel artwork URL"
            );
        }
    }
}
