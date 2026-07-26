//! `LitRes` podcast catalog and public-page support.
//!
//! Catalog calls use the documented `CataLit` 2.0 API with a `LitRes`-issued
//! application ID and secret. The client creates only the documented
//! `Anonymous`/`0` session, sends at most one request per second, keeps that
//! anonymous SID in memory, and never accepts account credentials.
//!
//! Direct links are resolved from bounded public HTML. Only schema.org metadata
//! and explicit `<audio src>` values are parsed. React hydration data, file IDs,
//! paid downloads, DRM keys, signed URLs, and undocumented URL templates are
//! deliberately ignored. `LitRes` currently exposes useful public podcast
//! metadata without necessarily advertising a reusable audio URL, so a valid
//! result commonly has an empty [`LitresPublicPage::media`] list.

use std::fmt::{self, Write as _};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use ureq::ResponseExt as _;
use url::Url;

use super::{DEFAULT_MAX_JSON_BYTES, DEFAULT_REQUEST_TIMEOUT, ProviderError, provider_agent};

const API_ENDPOINT: &str = "https://catalit.litres.ru/catalitv2";
const MAX_API_JSON_BYTES: usize = 8 * 1024 * 1024;
const MAX_PUBLIC_HTML_BYTES: usize = 2 * 1024 * 1024;
const MAX_OUTGOING_JSON_BYTES: usize = 1024 * 1024;
const MAX_SEARCH_RESULTS: usize = 50;
const MAX_EPISODE_RESULTS: usize = 50;
const MAX_JSON_LD_SCRIPTS: usize = 32;
const MAX_JSON_LD_SCRIPT_BYTES: usize = 256 * 1024;
const MIN_API_INTERVAL: Duration = Duration::from_secs(1);

/// A `LitRes`-issued `CataLit` application identity.
///
/// This is an application credential, not a user's `LitRes` account. Youta uses
/// it only to sign metadata requests and never serializes or logs the secret.
#[derive(Clone, Eq, PartialEq)]
pub struct LitresApplication {
    app_id: String,
    secret_key: String,
}

impl LitresApplication {
    /// Validates a `LitRes` application ID and secret key.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] unless the application ID is
    /// a positive decimal identifier and the secret is a bounded printable
    /// ASCII value.
    pub fn new(
        app_id: impl Into<String>,
        secret_key: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        let app_id = app_id.into();
        let secret_key = secret_key.into();
        if app_id.is_empty()
            || app_id.len() > 32
            || !app_id.bytes().all(|byte| byte.is_ascii_digit())
            || app_id.bytes().all(|byte| byte == b'0')
        {
            return Err(ProviderError::InvalidRequest(
                "LitRes application ID must be a positive decimal value of at most 32 digits"
                    .to_owned(),
            ));
        }
        if secret_key.is_empty()
            || secret_key.len() > 512
            || !secret_key.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(ProviderError::InvalidRequest(
                "LitRes application secret must be 1 to 512 printable ASCII characters".to_owned(),
            ));
        }
        Ok(Self { app_id, secret_key })
    }

    /// Returns the non-secret `LitRes` application identifier.
    #[must_use]
    pub fn app_id(&self) -> &str {
        &self.app_id
    }
}

impl fmt::Debug for LitresApplication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LitresApplication")
            .field("app_id", &self.app_id)
            .field("secret_key", &"[REDACTED]")
            .finish()
    }
}

/// A validated public `LitRes` podcast or podcast-episode link.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LitresLink {
    /// Numeric `LitRes` catalog item identifier from the final path segment.
    pub item_id: u64,
    /// Credential-free canonical page URL supplied by the caller.
    pub canonical_url: Url,
}

impl LitresLink {
    /// Parses a public `LitRes` podcast URL.
    ///
    /// Accepted URLs have the exact shape
    /// `https://www.litres.ru/podcast/{creator}/{slug}-{id}/`. The creator
    /// segment is intentionally opaque: it is not used as an identity.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] for non-HTTPS URLs, lookalike
    /// hosts, credentials, ports, queries, fragments, extra path segments, or
    /// malformed IDs.
    pub fn parse(url: &Url) -> Result<Self, ProviderError> {
        validate_public_page_url(url)?;
        let mut segments = url
            .path_segments()
            .ok_or_else(|| invalid_link("URL must contain path segments"))?
            .collect::<Vec<_>>();
        if segments.last() == Some(&"") {
            segments.pop();
        }
        let ["podcast", creator, item] = segments.as_slice() else {
            return Err(invalid_link(
                "expected /podcast/{creator}/{slug}-{positive-id}/",
            ));
        };
        validate_slug_segment(creator, "creator")?;
        validate_slug_segment(item, "item")?;
        let (_, raw_id) = item
            .rsplit_once('-')
            .ok_or_else(|| invalid_link("item path segment must end in -{id}"))?;
        let item_id = parse_positive_id(raw_id, "item")?;
        Ok(Self {
            item_id,
            canonical_url: url.clone(),
        })
    }
}

/// Whether a public media URL is explicitly a preview or a free full item.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LitresPublicMediaAccess {
    /// The page advertises the URL without an explicit free-full marker.
    Preview,
    /// Schema.org metadata explicitly marks the media as freely accessible.
    Full,
}

/// One credential-free media URL explicitly advertised by a public page.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LitresPublicMedia {
    /// Public HTTPS media URL under a `LitRes`-controlled host.
    pub url: Url,
    /// Access level explicitly supported by the surrounding page metadata.
    pub access: LitresPublicMediaAccess,
    /// MIME type advertised alongside the URL, when present.
    pub mime_type: Option<String>,
}

/// Bounded metadata parsed from one public `LitRes` page.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LitresPublicPage {
    /// Validated page identity.
    pub link: LitresLink,
    /// Public item title.
    pub title: String,
    /// Public creator names.
    pub creators: Vec<String>,
    /// Plain-text description, when advertised.
    pub description: Option<String>,
    /// Public cover URL under a `LitRes`-controlled host, when advertised.
    pub artwork_url: Option<Url>,
    /// Whether the schema.org offer explicitly has a zero price.
    pub is_free: Option<bool>,
    /// Price text from the schema.org offer, when present.
    pub price: Option<String>,
    /// Currency code from the schema.org offer, when present.
    pub price_currency: Option<String>,
    /// Publication date or timestamp, when advertised.
    pub published_at: Option<String>,
    /// Duration decoded from a schema.org ISO 8601 duration, when present.
    pub duration_seconds: Option<u64>,
    /// Explicit, unsigned public media URLs. This is often empty.
    pub media: Vec<LitresPublicMedia>,
}

/// Blocking resolver for public `LitRes` podcast pages.
pub struct LitresPublicResolver {
    agent: ureq::Agent,
    max_html_bytes: usize,
}

impl LitresPublicResolver {
    /// Creates a resolver with conservative timeout and response limits.
    #[must_use]
    pub fn new() -> Self {
        Self {
            agent: provider_agent(DEFAULT_REQUEST_TIMEOUT),
            max_html_bytes: MAX_PUBLIC_HTML_BYTES,
        }
    }

    /// Creates a resolver with explicit request limits.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] when the timeout is zero or
    /// the HTML limit is outside `1..=2 MiB`.
    pub fn with_options(timeout: Duration, max_html_bytes: usize) -> Result<Self, ProviderError> {
        if timeout.is_zero() {
            return Err(ProviderError::InvalidRequest(
                "LitRes request timeout must be greater than zero".to_owned(),
            ));
        }
        if !(1..=MAX_PUBLIC_HTML_BYTES).contains(&max_html_bytes) {
            return Err(ProviderError::InvalidRequest(format!(
                "LitRes HTML response limit must be between 1 and {MAX_PUBLIC_HTML_BYTES} bytes"
            )));
        }
        Ok(Self {
            agent: provider_agent(timeout),
            max_html_bytes,
        })
    }

    /// Fetches and parses one public podcast or episode page.
    ///
    /// Redirects remain constrained to a valid `LitRes` podcast URL with the
    /// same numeric item ID. The resolver does not request a media URL.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] for an invalid URL, transport or HTTP
    /// failure, oversized/non-UTF-8 HTML, unsafe redirects, or malformed public
    /// metadata.
    pub fn resolve(&self, url: &Url) -> Result<LitresPublicPage, ProviderError> {
        let requested = LitresLink::parse(url)?;
        let mut response = self
            .agent
            .get(url.as_str())
            .header(
                "Accept",
                "text/html,application/xhtml+xml;q=0.9,application/json;q=0.1",
            )
            .call()
            .map_err(map_ureq_error)?;

        let final_url = Url::parse(&response.get_uri().to_string()).map_err(|error| {
            ProviderError::InvalidResponse(format!(
                "LitRes returned an invalid redirect URL: {error}"
            ))
        })?;
        let final_link = LitresLink::parse(&final_url).map_err(|_| {
            ProviderError::InvalidResponse(
                "LitRes redirected outside a public podcast page".to_owned(),
            )
        })?;
        if final_link.item_id != requested.item_id {
            return Err(ProviderError::InvalidResponse(
                "LitRes redirected to a different podcast item".to_owned(),
            ));
        }

        if response
            .body()
            .content_length()
            .is_some_and(|length| length > self.max_html_bytes as u64)
        {
            return Err(ProviderError::ResponseTooLarge {
                limit: self.max_html_bytes,
            });
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit(u64::try_from(self.max_html_bytes.saturating_add(1)).unwrap_or(u64::MAX))
            .read_to_vec()
            .map_err(|error| map_body_error(error, self.max_html_bytes))?;
        if bytes.len() > self.max_html_bytes {
            return Err(ProviderError::ResponseTooLarge {
                limit: self.max_html_bytes,
            });
        }
        let html = std::str::from_utf8(&bytes).map_err(|error| {
            ProviderError::InvalidResponse(format!("LitRes page is not UTF-8: {error}"))
        })?;
        parse_public_page_html(&final_url, html)
    }
}

impl Default for LitresPublicResolver {
    fn default() -> Self {
        Self::new()
    }
}

/// Parses bounded schema.org metadata from a previously fetched public page.
///
/// This parser is intentionally narrow because `LitRes`'s HTML rendering is not
/// a documented API. It reads JSON-LD, description meta tags, and explicit
/// `<audio src>` values; all framework hydration state is ignored.
///
/// # Errors
///
/// Returns [`ProviderError`] when the source URL is invalid, the HTML exceeds
/// 2 MiB, no matching schema.org `Product` exists, or required metadata is
/// malformed.
pub fn parse_public_page_html(
    source_url: &Url,
    html: &str,
) -> Result<LitresPublicPage, ProviderError> {
    let source_link = LitresLink::parse(source_url)?;
    if html.len() > MAX_PUBLIC_HTML_BYTES {
        return Err(ProviderError::ResponseTooLarge {
            limit: MAX_PUBLIC_HTML_BYTES,
        });
    }

    let json_ld = json_ld_script_bodies(html)?
        .into_iter()
        .map(|body| {
            serde_json::from_str::<Value>(body).map_err(|error| {
                ProviderError::InvalidResponse(format!("invalid LitRes JSON-LD: {error}"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut matching_products = Vec::new();
    for value in &json_ld {
        collect_matching_products(value, source_link.item_id, &mut matching_products, 0)?;
    }
    let product = matching_products.into_iter().next().ok_or_else(|| {
        ProviderError::InvalidResponse(
            "LitRes page did not contain matching schema.org Product metadata".to_owned(),
        )
    })?;

    let canonical_url = product
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::InvalidResponse("LitRes Product URL is missing".to_owned()))
        .and_then(parse_remote_url)?;
    let canonical_link = LitresLink::parse(&canonical_url).map_err(|_| {
        ProviderError::InvalidResponse("LitRes Product URL is not a public podcast URL".to_owned())
    })?;
    if canonical_link.item_id != source_link.item_id {
        return Err(ProviderError::InvalidResponse(
            "LitRes Product URL identifies another item".to_owned(),
        ));
    }

    let title = required_bounded_text(
        product.get("name").and_then(Value::as_str),
        "LitRes Product name",
        1024,
    )?;
    let creators = parse_creators(product.get("author"))?;
    let description = product
        .get("description")
        .and_then(Value::as_str)
        .map(html_to_plain_text)
        .or_else(|| extract_meta_description(html))
        .map(|text| bounded_owned_text(text, 32 * 1024))
        .filter(|text| !text.is_empty());
    let artwork_url = extract_image_url(product.get("image"))?;
    let offer = first_object(product.get("offers"));
    let price = offer
        .and_then(|value| value.get("price"))
        .and_then(value_as_scalar_text)
        .map(|value| bounded_owned_text(value, 64))
        .filter(|value| !value.is_empty());
    let is_free = offer
        .and_then(|value| value.get("price"))
        .and_then(value_is_zero);
    let price_currency = offer
        .and_then(|value| value.get("priceCurrency"))
        .and_then(Value::as_str)
        .map(|value| bounded_owned_text(value.trim().to_owned(), 16))
        .filter(|value| !value.is_empty());
    let published_at = product
        .get("datePublished")
        .and_then(Value::as_str)
        .map(|value| bounded_owned_text(value.trim().to_owned(), 64))
        .filter(|value| !value.is_empty());
    let duration_seconds = product
        .get("duration")
        .and_then(Value::as_str)
        .and_then(parse_iso8601_duration);

    let mut media = Vec::new();
    collect_product_media(product, &mut media)?;
    collect_audio_tag_media(html, &mut media)?;

    Ok(LitresPublicPage {
        link: canonical_link,
        title,
        creators,
        description,
        artwork_url,
        is_free,
        price,
        price_currency,
        published_at,
        duration_seconds,
        media,
    })
}

/// Catalog object type requested from `CataLit`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LitresSearchTarget {
    /// Podcast collections.
    Podcasts,
    /// Individual podcast episodes.
    Episodes,
}

/// Title matching mode supported by `CataLit`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LitresMatchMode {
    /// Broad word matching; requires at least three characters.
    #[default]
    Broad,
    /// Match titles beginning with the query; requires three characters.
    StartsWith,
    /// Match the exact title; allows a one-character query.
    Exact,
}

/// One bounded `LitRes` catalog-search request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LitresSearchRequest {
    /// Search text.
    pub query: String,
    /// Whether to search podcast collections or episodes.
    pub target: LitresSearchTarget,
    /// Zero-based result offset.
    pub offset: u32,
    /// Number of results, from 1 through 50.
    pub limit: u8,
    /// `CataLit` title-matching mode.
    pub match_mode: LitresMatchMode,
}

impl LitresSearchRequest {
    /// Creates a first-page broad search for up to 20 results.
    #[must_use]
    pub fn new(query: impl Into<String>, target: LitresSearchTarget) -> Self {
        Self {
            query: query.into(),
            target,
            offset: 0,
            limit: 20,
            match_mode: LitresMatchMode::Broad,
        }
    }

    /// Validates documented query minima and Youta resource bounds.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] for an invalid query, offset,
    /// or result limit.
    pub fn validate(&self) -> Result<(), ProviderError> {
        let query = self.query.trim();
        let minimum = if self.match_mode == LitresMatchMode::Exact {
            1
        } else {
            3
        };
        if query.chars().count() < minimum {
            return Err(ProviderError::InvalidRequest(format!(
                "LitRes search query must contain at least {minimum} characters for this match mode"
            )));
        }
        if query.len() > 512 {
            return Err(ProviderError::InvalidRequest(
                "LitRes search query cannot exceed 512 bytes".to_owned(),
            ));
        }
        validate_page_bounds(self.offset, self.limit, "search")
    }
}

/// Sort order for episodes inside one `LitRes` podcast.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LitresEpisodeSort {
    /// Original episode order, oldest first.
    #[default]
    Default,
    /// Most popular episodes first.
    Popular,
    /// Newest episodes first.
    Newest,
}

/// One bounded episode-list request.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LitresEpisodeRequest {
    /// Positive podcast collection ID.
    pub podcast_id: u64,
    /// Zero-based episode offset.
    pub offset: u32,
    /// Number of episodes, from 1 through 50.
    pub limit: u8,
    /// Result order.
    pub sort: LitresEpisodeSort,
}

impl LitresEpisodeRequest {
    /// Creates a first page in original episode order.
    #[must_use]
    pub fn new(podcast_id: u64) -> Self {
        Self {
            podcast_id,
            offset: 0,
            limit: 20,
            sort: LitresEpisodeSort::Default,
        }
    }

    /// Validates the podcast ID and resource bounds.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] for a zero ID, excessive
    /// offset, or invalid result limit.
    pub fn validate(&self) -> Result<(), ProviderError> {
        if self.podcast_id == 0 {
            return Err(ProviderError::InvalidRequest(
                "LitRes podcast ID must be positive".to_owned(),
            ));
        }
        validate_page_bounds(self.offset, self.limit, "episode")
    }
}

/// Podcast object type returned by `CataLit`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LitresPodcastKind {
    /// A podcast collection.
    Podcast,
    /// An individual podcast episode.
    Episode,
}

/// Normalized public catalog metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LitresPodcastItem {
    /// Numeric `LitRes` catalog ID.
    pub id: u64,
    /// Podcast collection or episode.
    pub kind: LitresPodcastKind,
    /// Item title.
    pub title: String,
    /// Optional subtitle.
    pub subtitle: Option<String>,
    /// Creator and contributor names returned by `CataLit`.
    pub people: Vec<String>,
    /// Plain-text catalog annotation.
    pub description: Option<String>,
    /// Audio duration in seconds, when returned.
    pub duration_seconds: Option<u64>,
    /// Whether `CataLit` marks the item free.
    pub is_free: Option<bool>,
    /// Whether `CataLit` reports DRM.
    pub drm_protected: Option<bool>,
    /// Raw `CataLit` availability code.
    pub availability: Option<i64>,
    /// Recording/publication date returned by `CataLit`.
    pub published_at: Option<String>,
    /// Public genre and tag labels.
    pub genres: Vec<String>,
    /// Parent podcast ID for an episode.
    pub parent_podcast_id: Option<u64>,
    /// Parent podcast title for an episode.
    pub parent_podcast_name: Option<String>,
    /// Episode number in the parent podcast.
    pub episode_number: Option<u64>,
    /// Documented public cover URL, only for available catalog items.
    pub artwork_url: Option<Url>,
    /// Validated page URL when it came from a caller-supplied direct link.
    pub webpage_url: Option<Url>,
}

/// One page of `LitRes` podcast search results.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LitresSearchPage {
    /// Zero-based offset returned.
    pub offset: u32,
    /// Normalized results.
    pub items: Vec<LitresPodcastItem>,
    /// Next offset when the current page was full.
    pub next_offset: Option<u32>,
    /// Total page count reported by `CataLit`, when present.
    pub total_pages: Option<u32>,
}

/// One page of episodes within a `LitRes` podcast.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LitresEpisodePage {
    /// Parent podcast ID.
    pub podcast_id: u64,
    /// Zero-based offset returned.
    pub offset: u32,
    /// Normalized episodes.
    pub episodes: Vec<LitresPodcastItem>,
    /// Next offset when more episodes are known or likely.
    pub next_offset: Option<u32>,
    /// Total episode count returned by `CataLit`, when present.
    pub total_episodes: Option<u64>,
    /// Whether `CataLit` marks the podcast complete.
    pub podcast_complete: Option<bool>,
}

/// Blocking `CataLit` podcast-catalog client.
///
/// Calls are serialized and paced to one request per second as recommended by
/// the public API documentation. The anonymous SID is memory-only and is
/// refreshed once after a documented `invalid sid` response.
pub struct LitresCatalogClient {
    application: LitresApplication,
    agent: ureq::Agent,
    max_json_bytes: usize,
    anonymous_sid: Mutex<Option<String>>,
    request_pacer: Mutex<RequestPacer>,
}

impl LitresCatalogClient {
    /// Creates a client with conservative timeout and response limits.
    #[must_use]
    pub fn new(application: LitresApplication) -> Self {
        Self {
            application,
            agent: provider_agent(DEFAULT_REQUEST_TIMEOUT),
            max_json_bytes: DEFAULT_MAX_JSON_BYTES,
            anonymous_sid: Mutex::new(None),
            request_pacer: Mutex::new(RequestPacer::default()),
        }
    }

    /// Creates a client with explicit request limits.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] when the timeout is zero or
    /// the JSON response limit is outside `1..=8 MiB`.
    pub fn with_options(
        application: LitresApplication,
        timeout: Duration,
        max_json_bytes: usize,
    ) -> Result<Self, ProviderError> {
        if timeout.is_zero() {
            return Err(ProviderError::InvalidRequest(
                "LitRes request timeout must be greater than zero".to_owned(),
            ));
        }
        if !(1..=MAX_API_JSON_BYTES).contains(&max_json_bytes) {
            return Err(ProviderError::InvalidRequest(format!(
                "LitRes JSON response limit must be between 1 and {MAX_API_JSON_BYTES} bytes"
            )));
        }
        Ok(Self {
            application,
            agent: provider_agent(timeout),
            max_json_bytes,
            anonymous_sid: Mutex::new(None),
            request_pacer: Mutex::new(RequestPacer::default()),
        })
    }

    /// Searches podcast collections or episodes through documented `CataLit`.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] for invalid input, authentication/session
    /// failure, rate-limited transport failure, or malformed bounded JSON.
    pub fn search(&self, request: &LitresSearchRequest) -> Result<LitresSearchPage, ProviderError> {
        request.validate()?;
        let call = self.call_with_anonymous_sid(
            "r_search_arts",
            "searchArts",
            &json!({
                "q": request.query.trim(),
                "strict": match request.match_mode {
                    LitresMatchMode::Broad => "no",
                    LitresMatchMode::StartsWith => "start",
                    LitresMatchMode::Exact => "exact",
                },
                "limit": [request.offset.to_string(), request.limit.to_string()],
                "anno": "1",
                "atype": match request.target {
                    LitresSearchTarget::Podcasts => "10",
                    LitresSearchTarget::Episodes => "11",
                },
            }),
        )?;
        let raw: RawSearchCall = serde_json::from_value(call)
            .map_err(|error| invalid_api_response("searchArts", &error))?;
        normalize_search_page(request, raw)
    }

    /// Loads richer `CataLit` metadata for a validated direct link.
    ///
    /// This metadata call never requests files. Playback remains limited to
    /// explicit public media returned by [`LitresPublicResolver`].
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] for session/transport failure, a missing or
    /// non-podcast item, or malformed bounded JSON.
    pub fn details(&self, link: &LitresLink) -> Result<LitresPodcastItem, ProviderError> {
        let call = self.call_with_anonymous_sid(
            "r_browse_arts",
            "browseArts",
            &json!({
                "id": [link.item_id.to_string()],
                "anno": "1",
            }),
        )?;
        let raw: RawBrowseArtsCall = serde_json::from_value(call)
            .map_err(|error| invalid_api_response("browseArts", &error))?;
        if raw.arts.len() != 1 {
            return Err(ProviderError::InvalidResponse(
                "LitRes details did not return exactly the requested item".to_owned(),
            ));
        }
        let art = raw.arts.into_iter().next().ok_or_else(|| {
            ProviderError::InvalidResponse(
                "LitRes details response unexpectedly became empty".to_owned(),
            )
        })?;
        let mut item = normalize_art(art, None)?;
        if item.id != link.item_id {
            return Err(ProviderError::InvalidResponse(
                "LitRes details returned a different item ID".to_owned(),
            ));
        }
        item.webpage_url = Some(link.canonical_url.clone());
        Ok(item)
    }

    /// Lists episodes in a podcast through documented `r_browse_podcast`.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] for invalid bounds, session/transport
    /// failure, or malformed bounded JSON.
    pub fn episodes(
        &self,
        request: &LitresEpisodeRequest,
    ) -> Result<LitresEpisodePage, ProviderError> {
        request.validate()?;
        let call = self.call_with_anonymous_sid(
            "r_browse_podcast",
            "browsePodcast",
            &json!({
                "id": request.podcast_id.to_string(),
                "limit": [request.offset.to_string(), request.limit.to_string()],
                "sort": match request.sort {
                    LitresEpisodeSort::Default => "default",
                    LitresEpisodeSort::Popular => "pop",
                    LitresEpisodeSort::Newest => "new",
                },
                "anno": "1",
            }),
        )?;
        let raw: RawBrowsePodcastCall = serde_json::from_value(call)
            .map_err(|error| invalid_api_response("browsePodcast", &error))?;
        normalize_episode_page(request, raw)
    }

    fn call_with_anonymous_sid(
        &self,
        function: &'static str,
        request_id: &'static str,
        parameters: &Value,
    ) -> Result<Value, ProviderError> {
        for attempt in 0..=1 {
            let sid = self.ensure_anonymous_sid()?;
            let response = self.post_call(function, request_id, parameters, Some(&sid))?;
            if api_error_code(&response, request_id) == Some(101_000) && attempt == 0 {
                let mut stored = lock_mutex(&self.anonymous_sid, "anonymous SID")?;
                if stored.as_deref() == Some(sid.as_str()) {
                    *stored = None;
                }
                continue;
            }
            return extract_call(&response, request_id);
        }
        Err(ProviderError::InvalidResponse(
            "LitRes anonymous session could not be refreshed".to_owned(),
        ))
    }

    fn ensure_anonymous_sid(&self) -> Result<String, ProviderError> {
        let mut stored = lock_mutex(&self.anonymous_sid, "anonymous SID")?;
        if let Some(sid) = stored.as_ref() {
            return Ok(sid.clone());
        }
        let response = self.post_call(
            "w_create_sid",
            "anonymousSid",
            &json!({
                "login": "Anonymous",
                "pwd": "0",
            }),
            None,
        )?;
        let call = extract_call(&response, "anonymousSid")?;
        let sid = call
            .get("sid")
            .and_then(Value::as_str)
            .filter(|value| {
                !value.is_empty()
                    && value.len() <= 256
                    && value.bytes().all(|byte| byte.is_ascii_graphic())
            })
            .ok_or_else(|| {
                ProviderError::InvalidResponse(
                    "LitRes anonymous authorization returned an invalid SID".to_owned(),
                )
            })?
            .to_owned();
        *stored = Some(sid.clone());
        Ok(sid)
    }

    fn post_call(
        &self,
        function: &'static str,
        request_id: &'static str,
        parameters: &Value,
        sid: Option<&str>,
    ) -> Result<Value, ProviderError> {
        let mut pacer = lock_mutex(&self.request_pacer, "request pacer")?;
        let timestamp = pacer.next_timestamp()?;
        let signature = make_signature(&timestamp, &self.application.secret_key);
        let mut payload = json!({
            "app": self.application.app_id,
            "time": timestamp,
            "sha": signature,
            "requests": [{
                "func": function,
                "id": request_id,
                "param": parameters,
            }],
        });
        if let Some(sid) = sid {
            payload
                .as_object_mut()
                .expect("literal JSON object")
                .insert("sid".to_owned(), Value::String(sid.to_owned()));
        }
        let encoded = serde_json::to_string(&payload).map_err(|error| {
            ProviderError::InvalidRequest(format!("cannot encode LitRes request: {error}"))
        })?;
        if encoded.len() > MAX_OUTGOING_JSON_BYTES {
            return Err(ProviderError::InvalidRequest(
                "LitRes request exceeds the documented 1 MiB limit".to_owned(),
            ));
        }

        let mut response = self
            .agent
            .post(API_ENDPOINT)
            .header("Accept", "application/json")
            .send_form([("jdata", encoded.as_str())])
            .map_err(map_ureq_error)?;
        if response
            .body()
            .content_length()
            .is_some_and(|length| length > self.max_json_bytes as u64)
        {
            return Err(ProviderError::ResponseTooLarge {
                limit: self.max_json_bytes,
            });
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit(u64::try_from(self.max_json_bytes.saturating_add(1)).unwrap_or(u64::MAX))
            .read_to_vec()
            .map_err(|error| map_body_error(error, self.max_json_bytes))?;
        if bytes.len() > self.max_json_bytes {
            return Err(ProviderError::ResponseTooLarge {
                limit: self.max_json_bytes,
            });
        }
        serde_json::from_slice(&bytes)
            .map_err(|error| ProviderError::InvalidResponse(error.to_string()))
    }
}

#[derive(Default)]
struct RequestPacer {
    last_request: Option<Instant>,
    last_epoch_second: Option<i64>,
}

impl RequestPacer {
    fn next_timestamp(&mut self) -> Result<String, ProviderError> {
        if let Some(last_request) = self.last_request {
            let elapsed = last_request.elapsed();
            if let Some(wait) = MIN_API_INTERVAL.checked_sub(elapsed)
                && !wait.is_zero()
            {
                thread::sleep(wait);
            }
        }
        let epoch = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| {
                    ProviderError::Transport(format!(
                        "system clock is before the Unix epoch: {error}"
                    ))
                })?
                .as_secs(),
        )
        .map_err(|_| ProviderError::Transport("system time is out of range".to_owned()))?;
        let unique_epoch = self
            .last_epoch_second
            .map_or(epoch, |last| epoch.max(last.saturating_add(1)));
        self.last_request = Some(Instant::now());
        self.last_epoch_second = Some(unique_epoch);
        Ok(format_epoch_utc(unique_epoch))
    }
}

#[derive(Debug, Deserialize)]
struct RawSearchCall {
    #[serde(default)]
    arts: Vec<RawArt>,
    #[serde(default, deserialize_with = "deserialize_optional_u32")]
    pages: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct RawBrowseArtsCall {
    #[serde(default)]
    arts: Vec<RawArt>,
}

#[derive(Debug, Deserialize)]
struct RawBrowsePodcastCall {
    #[serde(default)]
    podcasts: Vec<RawArt>,
    #[serde(default)]
    podcast_info: Option<RawPodcastInfo>,
}

#[derive(Debug, Deserialize)]
struct RawPodcastInfo {
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    cnt: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_i64")]
    podcast_complete: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct RawArt {
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    id: Option<u64>,
    #[serde(
        default,
        rename = "type",
        deserialize_with = "deserialize_optional_i64"
    )]
    art_type: Option<i64>,
    #[serde(default, alias = "name")]
    title: Option<String>,
    #[serde(default)]
    subtitle: Option<String>,
    #[serde(default)]
    persons: Vec<RawNamedValue>,
    #[serde(default)]
    genres: Vec<RawNamedValue>,
    #[serde(default)]
    annotation: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    chars: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_i64")]
    free: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_optional_i64")]
    drm: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_optional_i64")]
    available: Option<i64>,
    #[serde(default)]
    available_date: Option<String>,
    #[serde(default)]
    date_written: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    parent_podcast_id: Option<u64>,
    #[serde(default)]
    parent_podcast_name: Option<String>,
    #[serde(
        default,
        alias = "serial_number",
        deserialize_with = "deserialize_optional_u64"
    )]
    podcast_serial_number: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawNamedValue {
    #[serde(default, alias = "full_name")]
    name: Option<String>,
}

fn normalize_search_page(
    request: &LitresSearchRequest,
    raw: RawSearchCall,
) -> Result<LitresSearchPage, ProviderError> {
    if raw.arts.len() > MAX_SEARCH_RESULTS || raw.arts.len() > usize::from(request.limit) {
        return Err(ProviderError::InvalidResponse(
            "LitRes returned more search results than requested".to_owned(),
        ));
    }
    let expected_kind = match request.target {
        LitresSearchTarget::Podcasts => LitresPodcastKind::Podcast,
        LitresSearchTarget::Episodes => LitresPodcastKind::Episode,
    };
    let items = raw
        .arts
        .into_iter()
        .map(|art| normalize_art(art, Some(expected_kind)))
        .collect::<Result<Vec<_>, _>>()?;
    let next_offset = if items.len() == usize::from(request.limit) {
        request
            .offset
            .checked_add(u32::try_from(items.len()).unwrap_or(u32::MAX))
    } else {
        None
    };
    Ok(LitresSearchPage {
        offset: request.offset,
        items,
        next_offset,
        total_pages: raw.pages,
    })
}

fn normalize_episode_page(
    request: &LitresEpisodeRequest,
    raw: RawBrowsePodcastCall,
) -> Result<LitresEpisodePage, ProviderError> {
    if raw.podcasts.len() > MAX_EPISODE_RESULTS || raw.podcasts.len() > usize::from(request.limit) {
        return Err(ProviderError::InvalidResponse(
            "LitRes returned more episodes than requested".to_owned(),
        ));
    }
    let mut episodes = Vec::with_capacity(raw.podcasts.len());
    for art in raw.podcasts {
        let mut item = normalize_art(art, Some(LitresPodcastKind::Episode))?;
        match item.parent_podcast_id {
            Some(parent) if parent != request.podcast_id => {
                return Err(ProviderError::InvalidResponse(
                    "LitRes episode belongs to a different podcast".to_owned(),
                ));
            }
            None => item.parent_podcast_id = Some(request.podcast_id),
            _ => {}
        }
        episodes.push(item);
    }
    let total_episodes = raw.podcast_info.as_ref().and_then(|info| info.cnt);
    let consumed =
        u64::from(request.offset).saturating_add(u64::try_from(episodes.len()).unwrap_or(u64::MAX));
    let has_more = total_episodes.map_or(episodes.len() == usize::from(request.limit), |total| {
        consumed < total
    });
    let next_offset = has_more.then(|| {
        request
            .offset
            .saturating_add(u32::try_from(episodes.len()).unwrap_or(u32::MAX))
    });
    let podcast_complete = raw
        .podcast_info
        .as_ref()
        .and_then(|info| info.podcast_complete)
        .map(|value| value == 1);
    Ok(LitresEpisodePage {
        podcast_id: request.podcast_id,
        offset: request.offset,
        episodes,
        next_offset,
        total_episodes,
        podcast_complete,
    })
}

fn normalize_art(
    raw: RawArt,
    expected_kind: Option<LitresPodcastKind>,
) -> Result<LitresPodcastItem, ProviderError> {
    let id = raw
        .id
        .filter(|id| *id > 0)
        .ok_or_else(|| ProviderError::InvalidResponse("LitRes item ID is missing".to_owned()))?;
    let kind = match (raw.art_type, expected_kind) {
        (Some(22), Some(LitresPodcastKind::Episode))
        | (Some(23), Some(LitresPodcastKind::Podcast)) => {
            return Err(ProviderError::InvalidResponse(
                "LitRes returned the wrong podcast object type".to_owned(),
            ));
        }
        (Some(22), _) => LitresPodcastKind::Podcast,
        (Some(23), _) => LitresPodcastKind::Episode,
        (None, Some(kind)) => kind,
        (Some(_), _) => {
            return Err(ProviderError::InvalidResponse(
                "LitRes returned a non-podcast catalog item".to_owned(),
            ));
        }
        (None, None) => {
            return Err(ProviderError::InvalidResponse(
                "LitRes item type is missing".to_owned(),
            ));
        }
    };
    let title = required_bounded_text(raw.title.as_deref(), "LitRes item title", 1024)?;
    let subtitle = optional_bounded_text(raw.subtitle, 1024);
    let people = normalize_names(raw.persons, 32, "people")?;
    let genres = normalize_names(raw.genres, 64, "genres")?;
    let description = raw
        .annotation
        .map(|value| bounded_owned_text(html_to_plain_text(&value), 32 * 1024))
        .filter(|value| !value.is_empty());
    let published_at = raw
        .available_date
        .or(raw.date_written)
        .map(|value| bounded_owned_text(value.trim().to_owned(), 64))
        .filter(|value| !value.is_empty());
    let parent_podcast_name = optional_bounded_text(raw.parent_podcast_name, 1024);
    let artwork_url = raw
        .available
        .filter(|availability| *availability > 0)
        .map(|_| documented_cover_url(id))
        .transpose()?;

    Ok(LitresPodcastItem {
        id,
        kind,
        title,
        subtitle,
        people,
        description,
        duration_seconds: raw.chars,
        is_free: raw.free.map(|value| value == 1),
        drm_protected: raw.drm.map(|value| value != 0),
        availability: raw.available,
        published_at,
        genres,
        parent_podcast_id: raw.parent_podcast_id,
        parent_podcast_name,
        episode_number: raw.podcast_serial_number,
        artwork_url,
        webpage_url: None,
    })
}

fn normalize_names(
    values: Vec<RawNamedValue>,
    maximum: usize,
    field: &str,
) -> Result<Vec<String>, ProviderError> {
    if values.len() > maximum {
        return Err(ProviderError::InvalidResponse(format!(
            "LitRes returned too many {field}"
        )));
    }
    let mut names = Vec::new();
    for value in values {
        let Some(name) = value.name else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        if name.len() > 1024 {
            return Err(ProviderError::InvalidResponse(format!(
                "LitRes {field} value is too long"
            )));
        }
        if !names.iter().any(|known| known == name) {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

fn documented_cover_url(item_id: u64) -> Result<Url, ProviderError> {
    let server = (item_id / 10) % 10;
    Url::parse(&format!(
        "https://cv{server}.litres.ru/pub/c/cover_200/{item_id}.jpg"
    ))
    .map_err(|error| ProviderError::InvalidResponse(error.to_string()))
}

fn validate_page_bounds(offset: u32, limit: u8, noun: &str) -> Result<(), ProviderError> {
    if offset > 1_000_000 {
        return Err(ProviderError::InvalidRequest(format!(
            "LitRes {noun} offset cannot exceed 1000000"
        )));
    }
    if !(1..=50).contains(&limit) {
        return Err(ProviderError::InvalidRequest(format!(
            "LitRes {noun} limit must be between 1 and 50"
        )));
    }
    Ok(())
}

fn make_signature(timestamp: &str, secret_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(timestamp.as_bytes());
    hasher.update(secret_key.as_bytes());
    let bytes = hasher.finalize();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn format_epoch_utc(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let seconds = epoch.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds / 3600;
    let minute = (seconds % 3600) / 60;
    let second = seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}+00:00")
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn extract_call(response: &Value, request_id: &str) -> Result<Value, ProviderError> {
    if let Some((code, message)) = api_error(response, request_id) {
        return Err(ProviderError::InvalidResponse(format!(
            "LitRes API error {code}: {}",
            bounded_owned_text(message, 512)
        )));
    }
    response.get(request_id).cloned().ok_or_else(|| {
        ProviderError::InvalidResponse(format!("LitRes response omitted request {request_id}"))
    })
}

fn api_error_code(response: &Value, request_id: &str) -> Option<u64> {
    api_error(response, request_id).map(|(code, _)| code)
}

fn api_error(response: &Value, request_id: &str) -> Option<(u64, String)> {
    for value in [Some(response), response.get(request_id)]
        .into_iter()
        .flatten()
    {
        if value.get("success").and_then(Value::as_bool) == Some(false) {
            let code = value.get("error_code").and_then(flexible_u64).unwrap_or(0);
            let message = value
                .get("error_message")
                .and_then(Value::as_str)
                .unwrap_or("unspecified error")
                .to_owned();
            return Some((code, message));
        }
    }
    None
}

fn invalid_api_response(request_id: &str, error: &serde_json::Error) -> ProviderError {
    ProviderError::InvalidResponse(format!("invalid LitRes {request_id} response: {error}"))
}

fn lock_mutex<'a, T>(
    mutex: &'a Mutex<T>,
    purpose: &str,
) -> Result<MutexGuard<'a, T>, ProviderError> {
    mutex
        .lock()
        .map_err(|_| ProviderError::Transport(format!("LitRes {purpose} lock was poisoned")))
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

fn map_body_error(error: ureq::Error, limit: usize) -> ProviderError {
    match error {
        ureq::Error::BodyExceedsLimit(_) => ProviderError::ResponseTooLarge { limit },
        other => ProviderError::Transport(other.to_string()),
    }
}

fn validate_public_page_url(url: &Url) -> Result<(), ProviderError> {
    if url.scheme() != "https" {
        return Err(invalid_link("LitRes links must use HTTPS"));
    }
    if !matches!(url.host_str(), Some("litres.ru" | "www.litres.ru")) {
        return Err(invalid_link("host must be litres.ru or www.litres.ru"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid_link("embedded credentials are not allowed"));
    }
    if url.port().is_some() {
        return Err(invalid_link("LitRes links must not specify a port"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(invalid_link("queries and fragments are not allowed"));
    }
    Ok(())
}

fn invalid_link(message: &str) -> ProviderError {
    ProviderError::InvalidRequest(format!("invalid LitRes link: {message}"))
}

fn validate_slug_segment(segment: &str, name: &str) -> Result<(), ProviderError> {
    if segment.is_empty()
        || segment.len() > 255
        || matches!(segment, "." | "..")
        || !segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'%' | b'.'))
    {
        return Err(invalid_link(&format!(
            "{name} path segment contains unsupported characters"
        )));
    }
    Ok(())
}

fn parse_positive_id(value: &str, field: &str) -> Result<u64, ProviderError> {
    if value.is_empty() || value.len() > 20 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_link(&format!(
            "{field} ID must contain decimal digits"
        )));
    }
    let id = value
        .parse::<u64>()
        .map_err(|_| invalid_link(&format!("{field} ID is too large")))?;
    if id == 0 {
        return Err(invalid_link(&format!("{field} ID must be positive")));
    }
    Ok(id)
}

fn json_ld_script_bodies(html: &str) -> Result<Vec<&str>, ProviderError> {
    let mut scripts = Vec::new();
    let mut cursor = 0;
    while let Some(relative_start) = find_ascii_case_insensitive(&html[cursor..], "<script") {
        let start = cursor + relative_start;
        let Some(relative_open_end) = html[start..].find('>') else {
            break;
        };
        let open_end = start + relative_open_end;
        let opening = &html[start..=open_end];
        cursor = open_end.saturating_add(1);
        let is_json_ld = tag_attribute(opening, "type")
            .is_some_and(|value| value.eq_ignore_ascii_case("application/ld+json"));
        if !is_json_ld {
            continue;
        }
        let relative_close =
            find_ascii_case_insensitive(&html[cursor..], "</script>").ok_or_else(|| {
                ProviderError::InvalidResponse(
                    "LitRes JSON-LD script has no closing tag".to_owned(),
                )
            })?;
        let body = &html[cursor..cursor + relative_close];
        if body.len() > MAX_JSON_LD_SCRIPT_BYTES {
            return Err(ProviderError::InvalidResponse(
                "LitRes JSON-LD script is too large".to_owned(),
            ));
        }
        scripts.push(body.trim());
        if scripts.len() > MAX_JSON_LD_SCRIPTS {
            return Err(ProviderError::InvalidResponse(
                "LitRes page contains too many JSON-LD scripts".to_owned(),
            ));
        }
        cursor += relative_close + "</script>".len();
    }
    Ok(scripts)
}

fn collect_matching_products<'a>(
    value: &'a Value,
    item_id: u64,
    products: &mut Vec<&'a Map<String, Value>>,
    depth: usize,
) -> Result<(), ProviderError> {
    if depth > 16 {
        return Err(ProviderError::InvalidResponse(
            "LitRes JSON-LD nesting is too deep".to_owned(),
        ));
    }
    match value {
        Value::Array(values) => {
            if values.len() > 256 {
                return Err(ProviderError::InvalidResponse(
                    "LitRes JSON-LD array is too large".to_owned(),
                ));
            }
            for value in values {
                collect_matching_products(value, item_id, products, depth + 1)?;
            }
        }
        Value::Object(object) => {
            if schema_type_is(object.get("@type"), "Product")
                && object
                    .get("url")
                    .and_then(Value::as_str)
                    .and_then(|raw| Url::parse(raw).ok())
                    .and_then(|url| LitresLink::parse(&url).ok())
                    .is_some_and(|link| link.item_id == item_id)
            {
                products.push(object);
            }
            if let Some(graph) = object.get("@graph") {
                collect_matching_products(graph, item_id, products, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn schema_type_is(value: Option<&Value>, expected: &str) -> bool {
    match value {
        Some(Value::String(value)) => value.eq_ignore_ascii_case(expected),
        Some(Value::Array(values)) => values.iter().any(|value| {
            value
                .as_str()
                .is_some_and(|value| value.eq_ignore_ascii_case(expected))
        }),
        _ => false,
    }
}

fn parse_creators(value: Option<&Value>) -> Result<Vec<String>, ProviderError> {
    fn collect(value: &Value, names: &mut Vec<String>) -> Result<(), ProviderError> {
        match value {
            Value::String(name) => {
                let name = required_bounded_text(Some(name), "LitRes creator", 1024)?;
                if !names.contains(&name) {
                    names.push(name);
                }
            }
            Value::Object(object) => {
                if let Some(name) = object.get("name").and_then(Value::as_str) {
                    let name = required_bounded_text(Some(name), "LitRes creator", 1024)?;
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
            }
            Value::Array(values) => {
                if values.len() > 32 {
                    return Err(ProviderError::InvalidResponse(
                        "LitRes Product has too many creators".to_owned(),
                    ));
                }
                for value in values {
                    collect(value, names)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    let mut names = Vec::new();
    if let Some(value) = value {
        collect(value, &mut names)?;
    }
    Ok(names)
}

fn extract_image_url(value: Option<&Value>) -> Result<Option<Url>, ProviderError> {
    let raw = match value {
        Some(Value::String(value)) => Some(value.as_str()),
        Some(Value::Object(object)) => object
            .get("url")
            .or_else(|| object.get("contentUrl"))
            .and_then(Value::as_str),
        Some(Value::Array(values)) => values.iter().find_map(|value| match value {
            Value::String(value) => Some(value.as_str()),
            Value::Object(object) => object
                .get("url")
                .or_else(|| object.get("contentUrl"))
                .and_then(Value::as_str),
            _ => None,
        }),
        _ => None,
    };
    raw.map(parse_remote_url)
        .transpose()?
        .map(validate_litres_asset_url)
        .transpose()
}

fn collect_product_media(
    product: &Map<String, Value>,
    media: &mut Vec<LitresPublicMedia>,
) -> Result<(), ProviderError> {
    let product_free = product
        .get("isAccessibleForFree")
        .and_then(value_as_bool)
        .unwrap_or(false);
    for (property, forced_access) in [
        ("preview", Some(LitresPublicMediaAccess::Preview)),
        ("audio", None),
        ("associatedMedia", None),
        ("subjectOf", None),
    ] {
        if let Some(value) = product.get(property) {
            collect_media_values(value, forced_access, product_free, media, 0)?;
        }
    }
    Ok(())
}

fn collect_media_values(
    value: &Value,
    forced_access: Option<LitresPublicMediaAccess>,
    parent_free: bool,
    media: &mut Vec<LitresPublicMedia>,
    depth: usize,
) -> Result<(), ProviderError> {
    if depth > 8 {
        return Err(ProviderError::InvalidResponse(
            "LitRes media metadata nesting is too deep".to_owned(),
        ));
    }
    match value {
        Value::Array(values) => {
            if values.len() > 32 {
                return Err(ProviderError::InvalidResponse(
                    "LitRes Product advertises too many media values".to_owned(),
                ));
            }
            for value in values {
                collect_media_values(value, forced_access, parent_free, media, depth + 1)?;
            }
        }
        Value::String(raw_url) => {
            add_public_media(
                media,
                raw_url,
                forced_access.unwrap_or(LitresPublicMediaAccess::Preview),
                None,
            )?;
        }
        Value::Object(object) => {
            let explicitly_free = object
                .get("isAccessibleForFree")
                .and_then(value_as_bool)
                .unwrap_or(parent_free);
            let access = forced_access.unwrap_or(if explicitly_free {
                LitresPublicMediaAccess::Full
            } else {
                LitresPublicMediaAccess::Preview
            });
            let mime_type = object
                .get("encodingFormat")
                .and_then(Value::as_str)
                .or_else(|| object.get("fileFormat").and_then(Value::as_str));
            if let Some(raw_url) = object
                .get("contentUrl")
                .or_else(|| object.get("url"))
                .and_then(Value::as_str)
            {
                add_public_media(media, raw_url, access, mime_type)?;
            }
            if let Some(nested) = object.get("encoding") {
                collect_media_values(nested, forced_access, explicitly_free, media, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn collect_audio_tag_media(
    html: &str,
    media: &mut Vec<LitresPublicMedia>,
) -> Result<(), ProviderError> {
    let mut cursor = 0;
    let mut count = 0;
    while let Some(relative_start) = find_ascii_case_insensitive(&html[cursor..], "<audio") {
        let start = cursor + relative_start;
        let Some(relative_end) = html[start..].find('>') else {
            break;
        };
        let end = start + relative_end;
        let tag = &html[start..=end];
        cursor = end.saturating_add(1);
        count += 1;
        if count > 32 {
            return Err(ProviderError::InvalidResponse(
                "LitRes page contains too many audio elements".to_owned(),
            ));
        }
        if let Some(raw_url) = tag_attribute(tag, "src") {
            add_public_media(
                media,
                raw_url,
                LitresPublicMediaAccess::Preview,
                tag_attribute(tag, "type"),
            )?;
        }
    }
    Ok(())
}

fn add_public_media(
    media: &mut Vec<LitresPublicMedia>,
    raw_url: &str,
    access: LitresPublicMediaAccess,
    mime_type: Option<&str>,
) -> Result<(), ProviderError> {
    let url = parse_remote_url(raw_url)?;
    let Some(url) = validate_public_media_url(url)? else {
        return Ok(());
    };
    if media.iter().any(|known| known.url == url) {
        return Ok(());
    }
    if media.len() >= 32 {
        return Err(ProviderError::InvalidResponse(
            "LitRes page advertises too many media URLs".to_owned(),
        ));
    }
    let mime_type = mime_type
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| bounded_owned_text(value.to_owned(), 128));
    media.push(LitresPublicMedia {
        url,
        access,
        mime_type,
    });
    Ok(())
}

fn parse_remote_url(raw: &str) -> Result<Url, ProviderError> {
    Url::parse(raw).map_err(|error| {
        ProviderError::InvalidResponse(format!("LitRes advertised an invalid URL: {error}"))
    })
}

fn validate_litres_asset_url(url: Url) -> Result<Url, ProviderError> {
    if url.scheme() != "https"
        || !is_litres_controlled_host(url.host_str())
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ProviderError::InvalidResponse(
            "LitRes asset URL must be unsigned credential-free HTTPS on a LitRes host".to_owned(),
        ));
    }
    Ok(url)
}

fn validate_public_media_url(url: Url) -> Result<Option<Url>, ProviderError> {
    if url.query().is_some() || url.fragment().is_some() {
        // Query-bearing media commonly carries an expiry or authorization
        // signature. Never surface it, even when embedded in public HTML.
        return Ok(None);
    }
    validate_litres_asset_url(url).map(Some)
}

fn is_litres_controlled_host(host: Option<&str>) -> bool {
    host.is_some_and(|host| host == "litres.ru" || host.ends_with(".litres.ru"))
}

fn first_object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    match value {
        Some(Value::Object(object)) => Some(object),
        Some(Value::Array(values)) => values.iter().find_map(Value::as_object),
        _ => None,
    }
}

fn value_as_scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.trim().to_owned()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn value_is_zero(value: &Value) -> Option<bool> {
    match value {
        Value::Number(value) => value.as_f64().map(|value| value == 0.0),
        Value::String(value) => value.trim().parse::<f64>().ok().map(|value| value == 0.0),
        _ => None,
    }
}

fn value_as_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(value) => Some(*value),
        Value::Number(value) => value.as_i64().map(|value| value != 0),
        Value::String(value) if value.eq_ignore_ascii_case("true") || value == "1" => Some(true),
        Value::String(value) if value.eq_ignore_ascii_case("false") || value == "0" => Some(false),
        _ => None,
    }
}

fn required_bounded_text(
    value: Option<&str>,
    field: &str,
    maximum: usize,
) -> Result<String, ProviderError> {
    let value = value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ProviderError::InvalidResponse(format!("{field} is missing")))?;
    if value.len() > maximum {
        return Err(ProviderError::InvalidResponse(format!(
            "{field} exceeds {maximum} bytes"
        )));
    }
    Ok(value.to_owned())
}

fn optional_bounded_text(value: Option<String>, maximum: usize) -> Option<String> {
    value
        .map(|value| bounded_owned_text(value.trim().to_owned(), maximum))
        .filter(|value| !value.is_empty())
}

fn bounded_owned_text(mut value: String, maximum: usize) -> String {
    if value.len() <= maximum {
        return value;
    }
    let mut boundary = maximum;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value
}

fn extract_meta_description(html: &str) -> Option<String> {
    let mut cursor = 0;
    while let Some(relative_start) = find_ascii_case_insensitive(&html[cursor..], "<meta") {
        let start = cursor + relative_start;
        let relative_end = html[start..].find('>')?;
        let end = start + relative_end;
        let tag = &html[start..=end];
        cursor = end.saturating_add(1);
        let key = tag_attribute(tag, "property").or_else(|| tag_attribute(tag, "name"));
        if key.is_some_and(|value| {
            value.eq_ignore_ascii_case("og:description")
                || value.eq_ignore_ascii_case("description")
        }) && let Some(content) = tag_attribute(tag, "content")
        {
            let text = html_to_plain_text(content);
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

fn tag_attribute<'a>(tag: &'a str, wanted: &str) -> Option<&'a str> {
    let bytes = tag.as_bytes();
    let mut index = 1;
    while index < bytes.len()
        && !bytes[index].is_ascii_whitespace()
        && !matches!(bytes[index], b'>' | b'/')
    {
        index += 1;
    }
    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() || matches!(bytes[index], b'>' | b'/') {
            break;
        }
        let name_start = index;
        while index < bytes.len()
            && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'-' | b'_' | b':'))
        {
            index += 1;
        }
        if name_start == index {
            index += 1;
            continue;
        }
        let name = &tag[name_start..index];
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if bytes.get(index) != Some(&b'=') {
            continue;
        }
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let quote = bytes.get(index).copied();
        let (value_start, value_end) = if matches!(quote, Some(b'"' | b'\'')) {
            index += 1;
            let start = index;
            while index < bytes.len() && Some(bytes[index]) != quote {
                index += 1;
            }
            (start, index)
        } else {
            let start = index;
            while index < bytes.len() && !bytes[index].is_ascii_whitespace() && bytes[index] != b'>'
            {
                index += 1;
            }
            (start, index)
        };
        if name.eq_ignore_ascii_case(wanted) {
            return tag.get(value_start..value_end);
        }
        if matches!(quote, Some(b'"' | b'\'')) && index < bytes.len() {
            index += 1;
        }
    }
    None
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let needle = needle.as_bytes();
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

fn html_to_plain_text(html: &str) -> String {
    let mut output = String::with_capacity(html.len().min(32 * 1024));
    let mut in_tag = false;
    let mut pending_space = false;
    for character in html.chars() {
        match character {
            '<' => {
                in_tag = true;
                pending_space = true;
            }
            '>' if in_tag => in_tag = false,
            _ if in_tag => {}
            character if character.is_whitespace() => pending_space = !output.is_empty(),
            character => {
                if pending_space && !output.ends_with(' ') {
                    output.push(' ');
                }
                output.push(character);
                pending_space = false;
            }
        }
        if output.len() >= 32 * 1024 {
            break;
        }
    }
    decode_basic_html_entities(output.trim())
}

fn decode_basic_html_entities(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
}

fn parse_iso8601_duration(value: &str) -> Option<u64> {
    let mut value = value.strip_prefix("PT")?;
    if value.is_empty() || value.len() > 32 {
        return None;
    }
    let mut seconds = 0_u64;
    let mut seen = false;
    while !value.is_empty() {
        let digits = value.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let amount = value.get(..digits)?.parse::<u64>().ok()?;
        let unit = value.as_bytes().get(digits).copied()?;
        let multiplier = match unit {
            b'H' => 3600,
            b'M' => 60,
            b'S' => 1,
            _ => return None,
        };
        seconds = seconds.checked_add(amount.checked_mul(multiplier)?)?;
        value = value.get(digits + 1..)?;
        seen = true;
    }
    seen.then_some(seconds)
}

fn deserialize_optional_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    value
        .as_ref()
        .map(|value| {
            flexible_u64(value)
                .ok_or_else(|| serde::de::Error::custom("expected a non-negative integer"))
        })
        .transpose()
}

fn deserialize_optional_u32<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_optional_u64(deserializer)?
        .map(|value| {
            u32::try_from(value)
                .map_err(|_| serde::de::Error::custom("integer does not fit in u32"))
        })
        .transpose()
}

fn deserialize_optional_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    value
        .as_ref()
        .map(|value| {
            flexible_i64(value).ok_or_else(|| serde::de::Error::custom("expected an integer"))
        })
        .transpose()
}

fn flexible_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(value) => value.as_u64(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    }
}

fn flexible_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(value) => value.as_i64(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPISODE_URL: &str = "https://www.litres.ru/podcast/irina-gibermann-32381637/epizod-2-kak-knigi-chitaut-nas-71811878/";

    #[test]
    fn parses_show_and_episode_links() {
        let episode = LitresLink::parse(&Url::parse(EPISODE_URL).unwrap()).unwrap();
        assert_eq!(episode.item_id, 71_811_878);

        let show = LitresLink::parse(
            &Url::parse("https://litres.ru/podcast/polkastudiya/polka-litra-72137134/").unwrap(),
        )
        .unwrap();
        assert_eq!(show.item_id, 72_137_134);
    }

    #[test]
    fn rejects_unsafe_or_ambiguous_links() {
        for raw in [
            "http://www.litres.ru/podcast/a/show-1/",
            "https://www.litres.ru.example/podcast/a/show-1/",
            "https://user@www.litres.ru/podcast/a/show-1/",
            "https://www.litres.ru/podcast/a/show-1/?sid=secret",
            "https://www.litres.ru/podcast/a/show-1/extra/",
            "https://www.litres.ru/podcast/a/show-0/",
        ] {
            let error = LitresLink::parse(&Url::parse(raw).unwrap()).unwrap_err();
            assert!(matches!(error, ProviderError::InvalidRequest(_)), "{raw}");
        }
    }

    #[test]
    fn application_debug_redacts_secret() {
        let application = LitresApplication::new("659558", "secret-value").unwrap();
        let debug = format!("{application:?}");
        assert!(debug.contains("659558"));
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("secret-value"));
    }

    #[test]
    fn matches_documented_signature_fixture() {
        assert_eq!(
            make_signature("2014-11-07T16:21:02+03:00", "659558"),
            "952643397153e9e816836742e906718e47aa7b67a8bdd647c8c9f6167fbed78a"
        );
    }

    #[test]
    fn formats_catalit_timestamp() {
        assert_eq!(format_epoch_utc(0), "1970-01-01T00:00:00+00:00");
        assert_eq!(format_epoch_utc(1_720_999_999), "2024-07-14T23:33:19+00:00");
    }

    #[test]
    fn parses_public_json_ld_and_explicit_free_media() {
        let html = r#"
            <html><head>
              <meta property="og:description" content="A &amp; B">
              <script type="application/ld+json">
              {
                "@context": "https://schema.org",
                "@type": "Product",
                "name": "Episode",
                "author": [{"@type":"Person","name":"Creator"}],
                "url": "https://www.litres.ru/podcast/creator/episode-71811878/",
                "image": "https://cdn.litres.ru/pub/c/cover/71811878.jpg",
                "offers": {"@type":"Offer","price":0,"priceCurrency":"RUB"},
                "datePublished": "2025-03-26",
                "duration": "PT43M18S",
                "audio": {
                  "@type": "AudioObject",
                  "isAccessibleForFree": true,
                  "encodingFormat": "audio/ogg",
                  "contentUrl": "https://cdn.litres.ru/public/episode.oga"
                }
              }
              </script>
            </head></html>
        "#;
        let page = parse_public_page_html(
            &Url::parse("https://www.litres.ru/podcast/creator/episode-71811878/").unwrap(),
            html,
        )
        .unwrap();
        assert_eq!(page.title, "Episode");
        assert_eq!(page.creators, ["Creator"]);
        assert_eq!(page.description.as_deref(), Some("A & B"));
        assert_eq!(page.is_free, Some(true));
        assert_eq!(page.duration_seconds, Some(2598));
        assert_eq!(page.media.len(), 1);
        assert_eq!(page.media[0].access, LitresPublicMediaAccess::Full);
    }

    #[test]
    fn unsigned_audio_element_is_only_a_preview() {
        let html = r#"
          <script type='application/ld+json'>
          {"@type":"Product","name":"Episode",
           "url":"https://www.litres.ru/podcast/a/episode-71811878/"}
          </script>
          <audio src="https://listen.litres.ru/public/sample.mp3" type="audio/mpeg"></audio>
        "#;
        let page = parse_public_page_html(
            &Url::parse("https://www.litres.ru/podcast/a/episode-71811878/").unwrap(),
            html,
        )
        .unwrap();
        assert_eq!(page.media.len(), 1);
        assert_eq!(page.media[0].access, LitresPublicMediaAccess::Preview);
    }

    #[test]
    fn ignores_query_bearing_media_urls() {
        let html = r#"
          <script type="application/ld+json">
          {"@type":"Product","name":"Episode",
           "url":"https://www.litres.ru/podcast/a/episode-71811878/",
           "audio":{"@type":"AudioObject","isAccessibleForFree":true,
                    "contentUrl":"https://cdn.litres.ru/file.mp3?token=secret"}}
          </script>
        "#;
        let page = parse_public_page_html(
            &Url::parse("https://www.litres.ru/podcast/a/episode-71811878/").unwrap(),
            html,
        )
        .unwrap();
        assert!(page.media.is_empty());
    }

    #[test]
    fn normalizes_catalog_search_fixture() {
        let request = LitresSearchRequest::new("полка", LitresSearchTarget::Podcasts);
        let raw: RawSearchCall = serde_json::from_value(json!({
            "pages": "2",
            "arts": [{
                "id": "72137134",
                "type": "22",
                "title": "Полка.Литра",
                "persons": [{"full_name": "Полка・Студия"}],
                "genres": [{"name": "литература"}],
                "annotation": "<p>Книжный подкаст</p>",
                "chars": "600",
                "free": "1",
                "drm": "0",
                "available": "1",
                "date_written": "2026-06-26"
            }]
        }))
        .unwrap();
        let page = normalize_search_page(&request, raw).unwrap();
        assert_eq!(page.total_pages, Some(2));
        assert_eq!(page.items[0].kind, LitresPodcastKind::Podcast);
        assert_eq!(page.items[0].people, ["Полка・Студия"]);
        assert_eq!(
            page.items[0].description.as_deref(),
            Some("Книжный подкаст")
        );
        assert!(page.items[0].artwork_url.is_some());
    }

    #[test]
    fn normalizes_episode_fixture_without_undocumented_type() {
        let request = LitresEpisodeRequest {
            podcast_id: 57_426_554,
            offset: 0,
            limit: 3,
            sort: LitresEpisodeSort::Default,
        };
        let raw: RawBrowsePodcastCall = serde_json::from_value(json!({
            "podcasts": [{
                "id": "29798333",
                "name": "Episode one",
                "annotation": "<p>Notes</p>",
                "serial_number": "1",
                "chars": "1234",
                "free": "1"
            }],
            "podcast_info": {"cnt": "1", "podcast_complete": "1"}
        }))
        .unwrap();
        let page = normalize_episode_page(&request, raw).unwrap();
        assert_eq!(page.total_episodes, Some(1));
        assert_eq!(page.podcast_complete, Some(true));
        assert_eq!(page.episodes[0].episode_number, Some(1));
        assert_eq!(page.episodes[0].parent_podcast_id, Some(57_426_554));
    }

    #[test]
    fn rejects_oversized_public_html() {
        let html = "x".repeat(MAX_PUBLIC_HTML_BYTES + 1);
        let error = parse_public_page_html(
            &Url::parse("https://www.litres.ru/podcast/a/episode-1/").unwrap(),
            &html,
        )
        .unwrap_err();
        assert!(matches!(error, ProviderError::ResponseTooLarge { .. }));
    }

    #[test]
    fn validates_search_bounds() {
        let mut request = LitresSearchRequest::new("ab", LitresSearchTarget::Podcasts);
        assert!(request.validate().is_err());
        request.match_mode = LitresMatchMode::Exact;
        assert!(request.validate().is_ok());
        request.limit = 51;
        assert!(request.validate().is_err());
    }
}
