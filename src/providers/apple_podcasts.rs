//! Apple Podcasts search and direct-link resolution using Apple's public
//! iTunes Search API.
//!
//! Show search uses Apple's documented, unauthenticated
//! [`/search`](https://developer.apple.com/library/archive/documentation/AudioVideo/Conceptual/iTuneSearchAPI/Searching.html)
//! endpoint with `media=podcast` and `entity=podcast`. Apple documents podcast
//! and podcast-author search entities, but not an episode-search entity, so
//! this module deliberately searches shows only. The resolver accepts public
//! `https://podcasts.apple.com` show and episode links, then performs an
//! ID-based lookup through Apple's documented `https://itunes.apple.com/lookup`
//! endpoint.
//!
//! Calls are blocking and bounded; they belong on Youta's capacity-one,
//! latest-only Apple worker and should be cached by the caller. Apple's
//! documentation describes a limit of roughly 20 Search API calls per minute
//! and recommends caching.
//!
//! This module does not authenticate an Apple account and does not claim to
//! read or synchronize follows, playback history, or played status.

use std::collections::BTreeSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::domain::remote_url_has_non_public_host;

use super::{DEFAULT_MAX_JSON_BYTES, DEFAULT_REQUEST_TIMEOUT, ProviderError};

const LOOKUP_ENDPOINT: &str = "https://itunes.apple.com/lookup";
const SEARCH_ENDPOINT: &str = "https://itunes.apple.com/search";
const MAX_CONFIGURED_JSON_BYTES: usize = 64 * 1024 * 1024;
const LOOKUP_EPISODE_LIMIT: u16 = 200;
// One collection record plus the requested maximum of 200 episodes.
const MAX_LOOKUP_RESULTS: usize = 201;
const MAX_SEARCH_RESULTS: u16 = 200;
const DEFAULT_SEARCH_RESULTS: u16 = 30;
const MAX_SEARCH_QUERY_BYTES: usize = 512;
const MAX_SEARCH_TEXT_BYTES: usize = 4 * 1024;
const MAX_SEARCH_GENRES: usize = 64;
const MAX_SEARCH_GENRE_BYTES: usize = 512;
const MAX_REMOTE_URL_BYTES: usize = 16 * 1024;
const MAX_API_REDIRECTS: usize = 3;

/// One bounded Apple Podcasts show-search request.
///
/// Apple Search API results are storefront-specific. `country` is therefore
/// explicit instead of silently using Apple's US default.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApplePodcastsSearchRequest {
    /// User-entered show search text.
    pub query: String,
    /// Two-letter ISO 3166-1 storefront code, such as `us` or `gb`.
    pub country: String,
    /// Maximum number of shows to return, from 1 through 200.
    pub limit: u16,
    /// Whether Apple may include shows marked explicit.
    pub include_explicit: bool,
}

impl ApplePodcastsSearchRequest {
    /// Creates a show search returning at most 30 results.
    #[must_use]
    pub fn new(query: impl Into<String>, country: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            country: country.into(),
            limit: DEFAULT_SEARCH_RESULTS,
            include_explicit: true,
        }
    }

    /// Validates the query, storefront, and documented result limit.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] for an empty or oversized
    /// query, a malformed two-letter storefront, or a result limit outside
    /// `1..=200`.
    pub fn validate(&self) -> Result<(), ProviderError> {
        if self.query.trim().is_empty() {
            return Err(ProviderError::InvalidRequest(
                "Apple Podcasts search query cannot be empty".to_owned(),
            ));
        }
        if self.query.len() > MAX_SEARCH_QUERY_BYTES {
            return Err(ProviderError::InvalidRequest(format!(
                "Apple Podcasts search query cannot exceed {MAX_SEARCH_QUERY_BYTES} bytes"
            )));
        }
        validate_storefront(&self.country)?;
        if !(1..=MAX_SEARCH_RESULTS).contains(&self.limit) {
            return Err(ProviderError::InvalidRequest(format!(
                "Apple Podcasts result limit must be between 1 and {MAX_SEARCH_RESULTS}"
            )));
        }
        Ok(())
    }
}

/// One non-pageable Apple Podcasts show-search response.
///
/// Apple's documented Search API exposes a result limit but no offset or page
/// token. Callers should treat this as a single ranked result set.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApplePodcastsSearchResults {
    /// Lowercase storefront used for the search.
    pub country: String,
    /// Normalized podcast shows in Apple's returned order.
    pub podcasts: Vec<ApplePodcastMetadata>,
}

/// Blocking client for public Apple Podcasts show search.
///
/// The public Search API does not require an API key or Apple account. This
/// client does not accept credentials.
#[derive(Clone)]
pub struct ApplePodcastsSearchClient {
    search_endpoint: Url,
    agent: ureq::Agent,
    max_json_bytes: usize,
}

impl ApplePodcastsSearchClient {
    /// Creates a client with conservative timeout and response limits.
    ///
    /// # Panics
    ///
    /// Panics only if Youta's compile-time Apple endpoint or built-in resource
    /// limits are invalid, which indicates a programming error.
    #[must_use]
    pub fn new() -> Self {
        Self::with_options(DEFAULT_REQUEST_TIMEOUT, DEFAULT_MAX_JSON_BYTES)
            .expect("built-in Apple Podcasts search limits must be valid")
    }

    /// Creates a client with explicit request limits.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] when the timeout is zero or
    /// the response bound is outside `1..=64 MiB`.
    pub fn with_options(timeout: Duration, max_json_bytes: usize) -> Result<Self, ProviderError> {
        let search_endpoint = Url::parse(SEARCH_ENDPOINT)
            .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
        Self::with_search_endpoint(search_endpoint, timeout, max_json_bytes)
    }

    /// Searches Apple Podcasts shows in one storefront.
    ///
    /// Apple does not document pagination or episode search for this API. The
    /// response therefore contains at most the request's `limit` shows and no
    /// continuation token.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] for invalid input, transport or HTTP
    /// failure, an oversized response, or malformed/unexpected Apple metadata.
    pub fn search(
        &self,
        request: &ApplePodcastsSearchRequest,
    ) -> Result<ApplePodcastsSearchResults, ProviderError> {
        let search_url = build_search_url(&self.search_endpoint, request)?;
        let response: RawLookupResponse = get_bounded_apple_json(
            &self.agent,
            &search_url,
            &self.search_endpoint,
            self.max_json_bytes,
        )?;
        normalize_search(request, &response)
    }

    fn with_search_endpoint(
        search_endpoint: Url,
        timeout: Duration,
        max_json_bytes: usize,
    ) -> Result<Self, ProviderError> {
        if timeout.is_zero() {
            return Err(ProviderError::InvalidRequest(
                "Apple Podcasts timeout must be greater than zero".to_owned(),
            ));
        }
        if !(1..=MAX_CONFIGURED_JSON_BYTES).contains(&max_json_bytes) {
            return Err(ProviderError::InvalidRequest(format!(
                "JSON response limit must be between 1 and {MAX_CONFIGURED_JSON_BYTES} bytes"
            )));
        }
        validate_api_endpoint(&search_endpoint)?;
        let agent = apple_provider_agent(timeout, search_endpoint.scheme() == "https");
        Ok(Self {
            search_endpoint,
            agent,
            max_json_bytes,
        })
    }
}

impl Default for ApplePodcastsSearchClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Identifiers extracted from a public Apple Podcasts URL.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApplePodcastLink {
    /// Lowercase two-letter Apple storefront country code.
    pub country: String,
    /// Numeric podcast collection identifier from the `id…` path segment.
    pub collection_id: u64,
    /// Numeric episode identifier from the optional `i` query parameter.
    pub episode_id: Option<u64>,
}

impl ApplePodcastLink {
    /// Parses an official Apple Podcasts show or episode URL.
    ///
    /// Accepted paths are
    /// `/{country}/podcast/{slug}/id{collection_id}` and
    /// `/{country}/podcast/id{collection_id}`. An optional positive numeric
    /// `i` query parameter selects an episode.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] for non-HTTPS URLs,
    /// lookalike hosts, embedded credentials, malformed paths, ambiguous
    /// episode parameters, or invalid numeric IDs.
    pub fn parse(url: &Url) -> Result<Self, ProviderError> {
        validate_apple_host(url)?;
        let mut segments = url
            .path_segments()
            .ok_or_else(|| invalid_link("Apple Podcasts URL must have path segments"))?
            .collect::<Vec<_>>();
        if segments.last() == Some(&"") {
            segments.pop();
        }
        if segments.iter().any(|segment| segment.is_empty()) {
            return Err(invalid_link(
                "expected /{country}/podcast/{optional-slug}/id{collection_id}",
            ));
        }
        let (country, id_segment) = match segments.as_slice() {
            [country, "podcast", id_segment] | [country, "podcast", _, id_segment] => {
                (*country, *id_segment)
            }
            _ => {
                return Err(invalid_link(
                    "expected /{country}/podcast/{optional-slug}/id{collection_id}",
                ));
            }
        };
        if country.len() != 2 || !country.bytes().all(|byte| byte.is_ascii_alphabetic()) {
            return Err(invalid_link(
                "Apple Podcasts country must be a two-letter code",
            ));
        }
        let collection_id = id_segment
            .strip_prefix("id")
            .ok_or_else(|| invalid_link("collection path segment must start with id"))
            .and_then(|value| parse_positive_id(value, "collection"))?;

        let mut episode_id = None;
        for (_, value) in url.query_pairs().filter(|(key, _)| key == "i") {
            if episode_id.is_some() {
                return Err(invalid_link(
                    "Apple Podcasts URL contains multiple episode IDs",
                ));
            }
            episode_id = Some(parse_positive_id(value.as_ref(), "episode")?);
        }

        Ok(Self {
            country: country.to_ascii_lowercase(),
            collection_id,
            episode_id,
        })
    }
}

/// Normalized podcast collection metadata returned by Apple.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApplePodcastMetadata {
    /// Apple collection identifier.
    pub collection_id: u64,
    /// Storefront used for the lookup.
    pub country: String,
    /// Podcast title.
    pub title: String,
    /// Podcast creator or network, when returned.
    pub author: Option<String>,
    /// Public RSS feed URL, when Apple returns one.
    pub feed_url: Option<Url>,
    /// Public Apple Podcasts page, when returned.
    pub webpage_url: Option<Url>,
    /// Largest artwork URL returned by the lookup response.
    pub artwork_url: Option<Url>,
    /// Episode count reported for the collection.
    pub episode_count: Option<u64>,
    /// Genre labels supplied by Apple.
    pub genres: Vec<String>,
    /// Whether Apple labels the collection explicit.
    pub explicit: Option<bool>,
}

/// Normalized podcast episode metadata returned by Apple.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApplePodcastEpisodeMetadata {
    /// Apple episode identifier.
    pub episode_id: u64,
    /// Parent podcast collection identifier.
    pub collection_id: u64,
    /// Episode title.
    pub title: String,
    /// Parent podcast title.
    pub podcast_title: String,
    /// Podcast creator or network, when returned.
    pub author: Option<String>,
    /// Long or short episode description, when returned.
    pub description: Option<String>,
    /// Publication timestamp as returned by Apple.
    pub published_at: Option<String>,
    /// Episode duration rounded down to whole seconds.
    pub duration_seconds: Option<u64>,
    /// Enclosure/media URL returned by Apple.
    ///
    /// Literal local/private destinations are rejected before this value is
    /// handed to playback. The playback backend owns subsequent DNS resolution
    /// and media redirects, so this does not claim redirect-time SSRF
    /// protection outside Youta's Apple API client.
    pub media_url: Option<Url>,
    /// Public Apple Podcasts episode page, when returned.
    pub webpage_url: Option<Url>,
    /// Largest artwork URL returned by Apple.
    pub artwork_url: Option<Url>,
    /// Whether Apple labels the episode explicit.
    pub explicit: Option<bool>,
}

/// Metadata resolved from one Apple Podcasts direct link.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResolvedApplePodcast {
    /// Identifiers parsed from the input URL.
    pub link: ApplePodcastLink,
    /// Parent podcast metadata.
    pub podcast: ApplePodcastMetadata,
    /// Requested episode metadata, or `None` for a show link.
    pub episode: Option<ApplePodcastEpisodeMetadata>,
}

/// One podcast show and the bounded episode window returned by Apple.
///
/// Apple's documented lookup endpoint returns the collection record followed
/// by at most 200 associated episodes. The episode order is preserved.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResolvedApplePodcastShow {
    /// Collection identity and storefront used for the lookup.
    pub link: ApplePodcastLink,
    /// Parent podcast metadata.
    pub podcast: ApplePodcastMetadata,
    /// All normalized episodes returned for the collection, at most 200.
    pub episodes: Vec<ApplePodcastEpisodeMetadata>,
}

/// Blocking client for Apple Podcasts direct-link resolution.
#[derive(Clone)]
pub struct ApplePodcastsResolver {
    lookup_endpoint: Url,
    agent: ureq::Agent,
    max_json_bytes: usize,
}

impl ApplePodcastsResolver {
    /// Creates a resolver with conservative timeout and response limits.
    #[must_use]
    pub fn new() -> Self {
        let lookup_endpoint =
            Url::parse(LOOKUP_ENDPOINT).expect("built-in Apple lookup endpoint must be valid");
        Self {
            agent: apple_provider_agent(DEFAULT_REQUEST_TIMEOUT, true),
            lookup_endpoint,
            max_json_bytes: DEFAULT_MAX_JSON_BYTES,
        }
    }

    /// Creates a resolver with explicit request limits.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] when the timeout is zero or
    /// the response bound is outside `1..=64 MiB`.
    pub fn with_options(timeout: Duration, max_json_bytes: usize) -> Result<Self, ProviderError> {
        if timeout.is_zero() {
            return Err(ProviderError::InvalidRequest(
                "Apple Podcasts timeout must be greater than zero".to_owned(),
            ));
        }
        if !(1..=MAX_CONFIGURED_JSON_BYTES).contains(&max_json_bytes) {
            return Err(ProviderError::InvalidRequest(format!(
                "JSON response limit must be between 1 and {MAX_CONFIGURED_JSON_BYTES} bytes"
            )));
        }
        let lookup_endpoint = Url::parse(LOOKUP_ENDPOINT)
            .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
        Ok(Self {
            agent: apple_provider_agent(timeout, true),
            lookup_endpoint,
            max_json_bytes,
        })
    }

    /// Resolves a public Apple Podcasts show or episode URL.
    ///
    /// The lookup contains the podcast plus at most Apple's documented maximum
    /// of 200 associated episodes. An episode Apple omits from that bounded
    /// response cannot be resolved; the resolver reports that explicitly
    /// instead of calling an undocumented Apple endpoint.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] when the direct URL is invalid, the bounded
    /// HTTPS lookup fails, Apple returns malformed metadata, the collection
    /// does not match the URL, or a requested episode is outside the returned
    /// lookup window.
    pub fn resolve(&self, url: &Url) -> Result<ResolvedApplePodcast, ProviderError> {
        let link = ApplePodcastLink::parse(url)?;
        let response = self.fetch_lookup(&link)?;
        normalize_lookup(link, &response)
    }

    /// Resolves a show URL and lists Apple's bounded associated episode window.
    ///
    /// This performs one documented lookup request and normalizes both the
    /// parent podcast and every returned episode. Episode URLs containing an
    /// `i` parameter are rejected because this operation addresses a complete
    /// show.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] when the URL is not a canonical show URL,
    /// the bounded lookup fails, or Apple returns malformed collection or
    /// episode metadata.
    pub fn resolve_show(&self, url: &Url) -> Result<ResolvedApplePodcastShow, ProviderError> {
        let link = ApplePodcastLink::parse(url)?;
        if link.episode_id.is_some() {
            return Err(ProviderError::InvalidRequest(
                "Apple Podcasts show listing URL must not contain an episode ID".to_owned(),
            ));
        }
        let response = self.fetch_lookup(&link)?;
        normalize_show_lookup(link, &response)
    }

    /// Resolves one storefront collection and lists up to 200 episodes.
    ///
    /// This form avoids synthesizing a provider page URL when a preceding
    /// search already supplied the collection ID and storefront.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] for an invalid storefront or collection ID,
    /// a failed bounded lookup, or malformed Apple metadata.
    pub fn resolve_collection(
        &self,
        country: &str,
        collection_id: u64,
    ) -> Result<ResolvedApplePodcastShow, ProviderError> {
        validate_storefront(country)?;
        if collection_id == 0 {
            return Err(ProviderError::InvalidRequest(
                "Apple Podcasts collection ID must be positive".to_owned(),
            ));
        }
        let link = ApplePodcastLink {
            country: country.to_ascii_lowercase(),
            collection_id,
            episode_id: None,
        };
        let response = self.fetch_lookup(&link)?;
        normalize_show_lookup(link, &response)
    }

    fn fetch_lookup(&self, link: &ApplePodcastLink) -> Result<RawLookupResponse, ProviderError> {
        let lookup_url = build_lookup_url(&self.lookup_endpoint, link);
        get_bounded_apple_json(
            &self.agent,
            &lookup_url,
            &self.lookup_endpoint,
            self.max_json_bytes,
        )
    }

    #[cfg(test)]
    fn with_lookup_endpoint(
        lookup_endpoint: Url,
        timeout: Duration,
        max_json_bytes: usize,
    ) -> Result<Self, ProviderError> {
        if timeout.is_zero() {
            return Err(ProviderError::InvalidRequest(
                "Apple Podcasts timeout must be greater than zero".to_owned(),
            ));
        }
        if !(1..=MAX_CONFIGURED_JSON_BYTES).contains(&max_json_bytes) {
            return Err(ProviderError::InvalidRequest(format!(
                "JSON response limit must be between 1 and {MAX_CONFIGURED_JSON_BYTES} bytes"
            )));
        }
        validate_api_endpoint(&lookup_endpoint)?;
        let agent = apple_provider_agent(timeout, lookup_endpoint.scheme() == "https");
        Ok(Self {
            lookup_endpoint,
            agent,
            max_json_bytes,
        })
    }
}

impl Default for ApplePodcastsResolver {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_apple_host(url: &Url) -> Result<(), ProviderError> {
    if url.scheme() != "https" {
        return Err(invalid_link("Apple Podcasts links must use HTTPS"));
    }
    if url.host_str() != Some("podcasts.apple.com") {
        return Err(invalid_link("host must be podcasts.apple.com"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid_link("embedded credentials are not allowed"));
    }
    if url.port().is_some() {
        return Err(invalid_link(
            "Apple Podcasts links must not specify a non-default port",
        ));
    }
    if url.fragment().is_some() {
        return Err(invalid_link(
            "Apple Podcasts links must not have a fragment",
        ));
    }
    Ok(())
}

fn invalid_link(message: &str) -> ProviderError {
    ProviderError::InvalidRequest(format!("invalid Apple Podcasts link: {message}"))
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

fn validate_storefront(country: &str) -> Result<(), ProviderError> {
    if country.len() != 2 || !country.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return Err(ProviderError::InvalidRequest(
            "Apple Podcasts storefront must be a two-letter country code".to_owned(),
        ));
    }
    Ok(())
}

fn validate_api_endpoint(endpoint: &Url) -> Result<(), ProviderError> {
    if !matches!(endpoint.scheme(), "http" | "https")
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(ProviderError::InvalidRequest(
            "Apple Podcasts API endpoint must be a credential-free HTTP(S) URL without a query or fragment"
                .to_owned(),
        ));
    }
    Ok(())
}

/// Creates an Apple API agent that exposes every redirect for validation.
fn apple_provider_agent(timeout: Duration, https_only: bool) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .https_only(https_only)
        .max_redirects(0)
        .http_status_as_error(false)
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

/// Fetches bounded Apple JSON while retaining one exact API origin.
///
/// Ureq's automatic redirect handling is disabled for this agent. Every hop is
/// resolved explicitly and must retain the original scheme, host, and effective
/// port before another request is sent.
fn get_bounded_apple_json<T: serde::de::DeserializeOwned>(
    agent: &ureq::Agent,
    url: &Url,
    endpoint: &Url,
    limit: usize,
) -> Result<T, ProviderError> {
    if limit == 0 {
        return Err(ProviderError::InvalidRequest(
            "JSON response limit must be greater than zero".to_owned(),
        ));
    }
    validate_api_redirect_target(endpoint, url)?;
    let mut current = url.clone();
    for redirect_count in 0..=MAX_API_REDIRECTS {
        let mut response = agent
            .get(current.as_str())
            .header("Accept", "application/json")
            .call()
            .map_err(map_apple_ureq_error)?;
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            if redirect_count == MAX_API_REDIRECTS {
                return Err(ProviderError::InvalidResponse(format!(
                    "Apple Podcasts API exceeded the {MAX_API_REDIRECTS}-redirect limit"
                )));
            }
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| {
                    ProviderError::InvalidResponse(
                        "Apple Podcasts API redirect omitted a valid Location header".to_owned(),
                    )
                })?;
            let next = current.join(location).map_err(|error| {
                ProviderError::InvalidResponse(format!(
                    "Apple Podcasts API redirect URL is invalid: {error}"
                ))
            })?;
            validate_api_redirect_target(endpoint, &next)?;
            current = next;
            continue;
        }
        if !(200..300).contains(&status) {
            return Err(ProviderError::HttpStatus(status));
        }
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
        return serde_json::from_slice(&bytes)
            .map_err(|error| ProviderError::InvalidResponse(error.to_string()));
    }
    unreachable!("the bounded redirect loop always returns")
}

fn validate_api_redirect_target(endpoint: &Url, target: &Url) -> Result<(), ProviderError> {
    if target.scheme() != endpoint.scheme()
        || target.host_str() != endpoint.host_str()
        || target.port_or_known_default() != endpoint.port_or_known_default()
        || !target.username().is_empty()
        || target.password().is_some()
        || target.fragment().is_some()
    {
        return Err(ProviderError::InvalidResponse(
            "Apple Podcasts API redirect left the validated origin".to_owned(),
        ));
    }
    Ok(())
}

fn map_apple_ureq_error(error: ureq::Error) -> ProviderError {
    match error {
        ureq::Error::StatusCode(code) => ProviderError::HttpStatus(code),
        ureq::Error::BodyExceedsLimit(limit) => ProviderError::ResponseTooLarge {
            limit: usize::try_from(limit).unwrap_or(usize::MAX),
        },
        other => ProviderError::Transport(other.to_string()),
    }
}

fn build_search_url(
    endpoint: &Url,
    request: &ApplePodcastsSearchRequest,
) -> Result<Url, ProviderError> {
    request.validate()?;
    let mut url = endpoint.clone();
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("term", request.query.trim());
        query.append_pair("country", &request.country.to_ascii_lowercase());
        query.append_pair("media", "podcast");
        query.append_pair("entity", "podcast");
        query.append_pair("limit", &request.limit.to_string());
        query.append_pair("version", "2");
        query.append_pair(
            "explicit",
            if request.include_explicit {
                "Yes"
            } else {
                "No"
            },
        );
    }
    Ok(url)
}

fn build_lookup_url(endpoint: &Url, link: &ApplePodcastLink) -> Url {
    let mut url = endpoint.clone();
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("id", &link.collection_id.to_string());
        query.append_pair("country", &link.country);
        query.append_pair("media", "podcast");
        query.append_pair("entity", "podcastEpisode");
        query.append_pair("limit", &LOOKUP_EPISODE_LIMIT.to_string());
    }
    url
}

fn normalize_search(
    request: &ApplePodcastsSearchRequest,
    response: &RawLookupResponse,
) -> Result<ApplePodcastsSearchResults, ProviderError> {
    request.validate()?;
    if usize::try_from(response.result_count).ok() != Some(response.results.len()) {
        return Err(ProviderError::InvalidResponse(
            "Apple search resultCount does not match the results array".to_owned(),
        ));
    }
    if response.results.len() > usize::from(request.limit)
        || response.results.len() > usize::from(MAX_SEARCH_RESULTS)
    {
        return Err(ProviderError::InvalidResponse(format!(
            "Apple search returned more than the requested {} results",
            request.limit
        )));
    }

    let country = request.country.to_ascii_lowercase();
    let mut seen_collection_ids = BTreeSet::new();
    let mut podcasts = Vec::with_capacity(response.results.len());
    for item in &response.results {
        if !item.is_podcast() {
            return Err(ProviderError::InvalidResponse(
                "Apple podcast search returned a non-podcast result".to_owned(),
            ));
        }
        let podcast = normalize_podcast(item, &country)?;
        validate_search_podcast(&podcast)?;
        if seen_collection_ids.insert(podcast.collection_id) {
            podcasts.push(podcast);
        }
    }

    Ok(ApplePodcastsSearchResults { country, podcasts })
}

fn validate_search_podcast(podcast: &ApplePodcastMetadata) -> Result<(), ProviderError> {
    validate_search_text("podcast title", &podcast.title)?;
    if let Some(author) = podcast.author.as_deref() {
        validate_search_text("podcast author", author)?;
    }
    if podcast.genres.len() > MAX_SEARCH_GENRES {
        return Err(ProviderError::InvalidResponse(format!(
            "Apple podcast has more than {MAX_SEARCH_GENRES} genres"
        )));
    }
    for genre in &podcast.genres {
        if genre.len() > MAX_SEARCH_GENRE_BYTES {
            return Err(ProviderError::InvalidResponse(format!(
                "Apple podcast genre exceeds {MAX_SEARCH_GENRE_BYTES} bytes"
            )));
        }
    }
    Ok(())
}

fn validate_search_text(field: &str, value: &str) -> Result<(), ProviderError> {
    if value.len() > MAX_SEARCH_TEXT_BYTES {
        return Err(ProviderError::InvalidResponse(format!(
            "Apple {field} exceeds {MAX_SEARCH_TEXT_BYTES} bytes"
        )));
    }
    Ok(())
}

fn normalize_show_lookup(
    link: ApplePodcastLink,
    response: &RawLookupResponse,
) -> Result<ResolvedApplePodcastShow, ProviderError> {
    validate_lookup_response(response)?;
    let show = response
        .results
        .iter()
        .find(|item| item.is_podcast() && item.collection_id == Some(link.collection_id))
        .ok_or_else(|| {
            ProviderError::InvalidResponse(
                "Apple lookup did not return the requested podcast collection".to_owned(),
            )
        })?;
    let podcast = normalize_podcast(show, &link.country)?;
    let mut episode_ids = BTreeSet::new();
    let mut episodes = Vec::new();
    for item in response
        .results
        .iter()
        .filter(|item| item.is_episode() && item.collection_id == Some(link.collection_id))
    {
        let episode = normalize_episode(item)?;
        if !episode_ids.insert(episode.episode_id) {
            return Err(ProviderError::InvalidResponse(format!(
                "Apple lookup returned episode {} more than once",
                episode.episode_id
            )));
        }
        episodes.push(episode);
    }
    if episodes.len() > usize::from(LOOKUP_EPISODE_LIMIT) {
        return Err(ProviderError::InvalidResponse(format!(
            "Apple lookup returned more than {LOOKUP_EPISODE_LIMIT} episodes"
        )));
    }
    Ok(ResolvedApplePodcastShow {
        link,
        podcast,
        episodes,
    })
}

fn normalize_lookup(
    link: ApplePodcastLink,
    response: &RawLookupResponse,
) -> Result<ResolvedApplePodcast, ProviderError> {
    validate_lookup_response(response)?;
    let show = response
        .results
        .iter()
        .find(|item| item.is_podcast() && item.collection_id == Some(link.collection_id))
        .ok_or_else(|| {
            ProviderError::InvalidResponse(
                "Apple lookup did not return the requested podcast collection".to_owned(),
            )
        })?;
    let episode_item = match link.episode_id {
        Some(episode_id) => Some(
            response
                .results
                .iter()
                .find(|item| {
                    item.is_episode()
                        && item.collection_id == Some(link.collection_id)
                        && item.track_id == Some(episode_id)
                })
                .ok_or_else(|| {
                    ProviderError::InvalidResponse(format!(
                        "Apple lookup did not return requested episode {episode_id}; Apple may have omitted it from the bounded {LOOKUP_EPISODE_LIMIT}-episode response"
                    ))
                })?,
        ),
        None => None,
    };

    let podcast = normalize_podcast(show, &link.country)?;
    let episode = episode_item.map(normalize_episode).transpose()?;
    Ok(ResolvedApplePodcast {
        link,
        podcast,
        episode,
    })
}

fn validate_lookup_response(response: &RawLookupResponse) -> Result<(), ProviderError> {
    if response.results.len() > MAX_LOOKUP_RESULTS {
        return Err(ProviderError::InvalidResponse(format!(
            "Apple lookup returned more than {MAX_LOOKUP_RESULTS} results"
        )));
    }
    if usize::try_from(response.result_count).ok() != Some(response.results.len()) {
        return Err(ProviderError::InvalidResponse(
            "Apple lookup resultCount does not match the results array".to_owned(),
        ));
    }
    Ok(())
}

fn normalize_podcast(
    item: &RawLookupItem,
    country: &str,
) -> Result<ApplePodcastMetadata, ProviderError> {
    let collection_id = item.collection_id.ok_or_else(|| {
        ProviderError::InvalidResponse("Apple podcast is missing collectionId".to_owned())
    })?;
    let title = nonempty(item.collection_name.as_deref())
        .or_else(|| nonempty(item.track_name.as_deref()))
        .ok_or_else(|| {
            ProviderError::InvalidResponse("Apple podcast is missing a title".to_owned())
        })?
        .to_owned();
    let webpage_url = parse_optional_apple_page_url(
        item.collection_view_url
            .as_deref()
            .or(item.track_view_url.as_deref()),
        "podcast view URL",
    )?;
    if let Some(url) = webpage_url.as_ref() {
        validate_returned_apple_page(url, collection_id, None, "podcast view URL")?;
    }

    Ok(ApplePodcastMetadata {
        collection_id,
        country: country.to_owned(),
        title,
        author: nonempty(item.artist_name.as_deref()).map(ToOwned::to_owned),
        feed_url: parse_optional_remote_url(item.feed_url.as_deref(), "feedUrl")?,
        webpage_url,
        artwork_url: parse_artwork_url(item)?,
        episode_count: item.track_count,
        genres: item
            .genres
            .iter()
            .filter_map(|genre| nonempty(Some(genre.name())).map(ToOwned::to_owned))
            .collect(),
        explicit: parse_explicitness(
            item.collection_explicitness
                .as_deref()
                .or(item.track_explicitness.as_deref()),
        ),
    })
}

fn normalize_episode(item: &RawLookupItem) -> Result<ApplePodcastEpisodeMetadata, ProviderError> {
    let episode_id = item.track_id.ok_or_else(|| {
        ProviderError::InvalidResponse("Apple episode is missing trackId".to_owned())
    })?;
    let collection_id = item.collection_id.ok_or_else(|| {
        ProviderError::InvalidResponse("Apple episode is missing collectionId".to_owned())
    })?;
    let title = nonempty(item.track_name.as_deref())
        .ok_or_else(|| {
            ProviderError::InvalidResponse("Apple episode is missing a title".to_owned())
        })?
        .to_owned();
    let podcast_title = nonempty(item.collection_name.as_deref())
        .ok_or_else(|| {
            ProviderError::InvalidResponse("Apple episode is missing a podcast title".to_owned())
        })?
        .to_owned();
    let webpage_url =
        parse_optional_apple_page_url(item.track_view_url.as_deref(), "episode view URL")?;
    if let Some(url) = webpage_url.as_ref() {
        validate_returned_apple_page(url, collection_id, Some(episode_id), "episode view URL")?;
    }

    Ok(ApplePodcastEpisodeMetadata {
        episode_id,
        collection_id,
        title,
        podcast_title,
        author: nonempty(item.artist_name.as_deref()).map(ToOwned::to_owned),
        description: nonempty(item.description.as_deref())
            .or_else(|| nonempty(item.short_description.as_deref()))
            .map(ToOwned::to_owned),
        published_at: nonempty(item.release_date.as_deref()).map(ToOwned::to_owned),
        duration_seconds: item.track_time_millis.map(|millis| millis / 1_000),
        media_url: parse_optional_remote_url(item.episode_url.as_deref(), "episodeUrl")?,
        webpage_url,
        artwork_url: parse_artwork_url(item)?,
        explicit: parse_explicitness(item.track_explicitness.as_deref()),
    })
}

fn parse_artwork_url(item: &RawLookupItem) -> Result<Option<Url>, ProviderError> {
    parse_optional_remote_url(
        item.artwork_url_600
            .as_deref()
            .or(item.artwork_url_100.as_deref())
            .or(item.artwork_url_60.as_deref()),
        "artwork URL",
    )
}

/// Parses a provider-returned Apple Podcasts page with an exact official host.
fn parse_optional_apple_page_url(
    raw: Option<&str>,
    field: &str,
) -> Result<Option<Url>, ProviderError> {
    let url = parse_optional_remote_url(raw, field)?;
    if url
        .as_ref()
        .is_some_and(|url| url.scheme() != "https" || url.host_str() != Some("podcasts.apple.com"))
    {
        return Err(ProviderError::InvalidResponse(format!(
            "Apple {field} must use the exact https://podcasts.apple.com host"
        )));
    }
    Ok(url)
}

fn validate_returned_apple_page(
    url: &Url,
    collection_id: u64,
    episode_id: Option<u64>,
    field: &str,
) -> Result<(), ProviderError> {
    let link = ApplePodcastLink::parse(url).map_err(|error| {
        ProviderError::InvalidResponse(format!("Apple {field} is not canonical: {error}"))
    })?;
    if link.collection_id != collection_id || link.episode_id != episode_id {
        return Err(ProviderError::InvalidResponse(format!(
            "Apple {field} does not match the returned collection/episode identity"
        )));
    }
    Ok(())
}

/// Parses a bounded-use remote URL without credentials or local literals.
fn parse_optional_remote_url(raw: Option<&str>, field: &str) -> Result<Option<Url>, ProviderError> {
    let Some(raw) = nonempty(raw) else {
        return Ok(None);
    };
    if raw.len() > MAX_REMOTE_URL_BYTES {
        return Err(ProviderError::InvalidResponse(format!(
            "Apple {field} exceeds {MAX_REMOTE_URL_BYTES} bytes"
        )));
    }
    let url = Url::parse(raw).map_err(|error| {
        ProviderError::InvalidResponse(format!("Apple {field} is invalid: {error}"))
    })?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
        || remote_url_has_non_public_host(&url)
    {
        return Err(ProviderError::InvalidResponse(format!(
            "Apple {field} must be a bounded credential-free HTTP(S) URL without a non-default port, fragment, or literal local/private host"
        )));
    }
    Ok(Some(url))
}

fn parse_explicitness(value: Option<&str>) -> Option<bool> {
    match value?.to_ascii_lowercase().as_str() {
        "explicit" => Some(true),
        "cleaned" | "notexplicit" => Some(false),
        _ => None,
    }
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLookupResponse {
    #[serde(default, deserialize_with = "deserialize_u64")]
    result_count: u64,
    results: Vec<RawLookupItem>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLookupItem {
    #[serde(default)]
    wrapper_type: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    collection_id: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    track_id: Option<u64>,
    #[serde(default)]
    artist_name: Option<String>,
    #[serde(default)]
    collection_name: Option<String>,
    #[serde(default)]
    track_name: Option<String>,
    #[serde(default)]
    feed_url: Option<String>,
    #[serde(default)]
    episode_url: Option<String>,
    #[serde(default)]
    collection_view_url: Option<String>,
    #[serde(default)]
    track_view_url: Option<String>,
    #[serde(default)]
    artwork_url_600: Option<String>,
    #[serde(default)]
    artwork_url_100: Option<String>,
    #[serde(default)]
    artwork_url_60: Option<String>,
    #[serde(default)]
    release_date: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    track_time_millis: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    track_count: Option<u64>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    short_description: Option<String>,
    #[serde(default)]
    genres: Vec<RawGenre>,
    #[serde(default)]
    collection_explicitness: Option<String>,
    #[serde(default)]
    track_explicitness: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawGenre {
    Name(String),
    Detailed { name: String },
}

impl RawGenre {
    fn name(&self) -> &str {
        match self {
            Self::Name(name) | Self::Detailed { name } => name,
        }
    }
}

impl RawLookupItem {
    fn is_podcast(&self) -> bool {
        self.kind.as_deref() == Some("podcast")
    }

    fn is_episode(&self) -> bool {
        self.kind.as_deref() == Some("podcast-episode")
            || self.wrapper_type.as_deref() == Some("podcastEpisode")
    }
}

fn deserialize_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_optional_u64(deserializer).map(Option::unwrap_or_default)
}

fn deserialize_optional_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom("expected a non-negative integer")),
        Some(Value::String(text)) => text
            .parse::<u64>()
            .map(Some)
            .map_err(serde::de::Error::custom),
        Some(_) => Err(serde::de::Error::custom(
            "expected an integer, numeric string, or null",
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};

    use super::*;

    const SEARCH_FIXTURE: &str = r#"{
        "resultCount": 1,
        "results": [{
            "wrapperType": "track",
            "kind": "podcast",
            "artistName": "Fixture Network",
            "collectionId": 123456789,
            "trackId": 123456789,
            "collectionName": "Science & History",
            "trackName": "Science & History",
            "collectionViewUrl": "https://podcasts.apple.com/ge/podcast/science-history/id123456789",
            "feedUrl": "https://feeds.example.test/science.xml",
            "artworkUrl600": "https://is1-ssl.example.test/image/thumb/600x600.jpg",
            "trackCount": "42",
            "genres": ["Science", {"name": "History", "id": "1487"}],
            "collectionExplicitness": "notExplicit"
        }]
    }"#;

    const SHOW_EPISODES_FIXTURE: &str = r#"{
        "resultCount": 3,
        "results": [{
            "wrapperType": "track",
            "kind": "podcast",
            "artistName": "Example Network",
            "collectionId": 1756129194,
            "trackId": 1756129194,
            "collectionName": "Example Show",
            "trackName": "Example Show",
            "collectionViewUrl": "https://podcasts.apple.com/gb/podcast/example-show/id1756129194",
            "feedUrl": "https://feeds.example.test/show.xml",
            "artworkUrl600": "https://is1-ssl.example.test/artwork/600x600.jpg",
            "trackCount": 19,
            "genres": ["Society & Culture", "Podcasts"],
            "collectionExplicitness": "notExplicit"
        }, {
            "wrapperType": "podcastEpisode",
            "kind": "podcast-episode",
            "artistName": "Example Network",
            "collectionId": 1756129194,
            "trackId": 1000719462606,
            "collectionName": "Example Show",
            "trackName": "Newest Fixture Episode",
            "description": "Newest description",
            "releaseDate": "2025-07-28T07:00:00Z",
            "trackTimeMillis": 5157000,
            "episodeUrl": "https://cdn.example.test/newest.mp3",
            "trackViewUrl": "https://podcasts.apple.com/gb/podcast/newest/id1756129194?i=1000719462606",
            "artworkUrl100": "https://is1-ssl.example.test/artwork/100x100.jpg",
            "trackExplicitness": "explicit"
        }, {
            "wrapperType": "podcastEpisode",
            "kind": "podcast-episode",
            "artistName": "Example Network",
            "collectionId": "1756129194",
            "trackId": "1000719462605",
            "collectionName": "Example Show",
            "trackName": "Older Fixture Episode",
            "shortDescription": "Older description",
            "releaseDate": "2025-07-21T07:00:00Z",
            "trackTimeMillis": "3600000",
            "episodeUrl": "https://cdn.example.test/older.m4a",
            "trackViewUrl": "https://podcasts.apple.com/gb/podcast/older/id1756129194?i=1000719462605",
            "artworkUrl60": "https://is1-ssl.example.test/artwork/60x60.jpg",
            "trackExplicitness": "notExplicit"
        }]
    }"#;

    const EPISODE_LOOKUP_FIXTURE: &str = r#"{
        "resultCount": 2,
        "results": [{
            "wrapperType": "track",
            "kind": "podcast",
            "artistName": "Example Network",
            "collectionId": 1756129194,
            "trackId": 1756129194,
            "collectionName": "Example Show",
            "trackName": "Example Show",
            "collectionViewUrl": "https://podcasts.apple.com/us/podcast/example-show/id1756129194",
            "feedUrl": "https://feeds.example.test/show.xml",
            "artworkUrl600": "https://is1-ssl.example.test/artwork/600x600.jpg",
            "trackCount": "19",
            "genres": ["Society & Culture", "Podcasts"],
            "collectionExplicitness": "notExplicit"
        }, {
            "wrapperType": "podcastEpisode",
            "kind": "podcast-episode",
            "artistName": "Example Network",
            "collectionId": "1756129194",
            "trackId": 1000719462606,
            "collectionName": "Example Show",
            "trackName": "A Fixture Episode",
            "description": "Long episode description",
            "releaseDate": "2025-07-28T07:00:00Z",
            "trackTimeMillis": 5157000,
            "episodeUrl": "https://cdn.example.test/episode.mp3",
            "trackViewUrl": "https://podcasts.apple.com/us/podcast/a-fixture-episode/id1756129194?i=1000719462606",
            "artworkUrl100": "https://is1-ssl.example.test/artwork/100x100.jpg",
            "genres": [{"name": "Society & Culture", "id": "1324"}],
            "trackExplicitness": "explicit"
        }]
    }"#;

    fn parse_link(raw: &str) -> Result<ApplePodcastLink, ProviderError> {
        ApplePodcastLink::parse(&Url::parse(raw).expect("fixture URL should parse"))
    }

    #[test]
    fn search_url_uses_only_documented_show_parameters() {
        let endpoint = Url::parse(SEARCH_ENDPOINT).expect("built-in search URL should parse");
        let mut request = ApplePodcastsSearchRequest::new("  science & history  ", "GE");
        request.limit = 25;
        request.include_explicit = false;

        let url = build_search_url(&endpoint, &request).expect("search URL should build");
        let pairs = url.query_pairs().collect::<HashMap<_, _>>();

        assert_eq!(url.as_str().split('?').next(), Some(SEARCH_ENDPOINT));
        assert_eq!(
            pairs.get("term").map(AsRef::as_ref),
            Some("science & history")
        );
        assert_eq!(pairs.get("country").map(AsRef::as_ref), Some("ge"));
        assert_eq!(pairs.get("media").map(AsRef::as_ref), Some("podcast"));
        assert_eq!(pairs.get("entity").map(AsRef::as_ref), Some("podcast"));
        assert_eq!(pairs.get("limit").map(AsRef::as_ref), Some("25"));
        assert_eq!(pairs.get("version").map(AsRef::as_ref), Some("2"));
        assert_eq!(pairs.get("explicit").map(AsRef::as_ref), Some("No"));
        assert!(
            !pairs.contains_key("podcastEpisode"),
            "Apple does not document episode search"
        );
    }

    #[test]
    fn search_request_rejects_invalid_query_storefront_and_limit() {
        let fixtures = [
            ApplePodcastsSearchRequest::new(" ", "us"),
            ApplePodcastsSearchRequest::new("x".repeat(MAX_SEARCH_QUERY_BYTES + 1), "us"),
            ApplePodcastsSearchRequest::new("science", "usa"),
            ApplePodcastsSearchRequest::new("science", "u1"),
            ApplePodcastsSearchRequest {
                limit: 0,
                ..ApplePodcastsSearchRequest::new("science", "us")
            },
            ApplePodcastsSearchRequest {
                limit: MAX_SEARCH_RESULTS + 1,
                ..ApplePodcastsSearchRequest::new("science", "us")
            },
        ];

        for request in fixtures {
            assert!(
                matches!(request.validate(), Err(ProviderError::InvalidRequest(_))),
                "invalid search request should be rejected: {request:?}"
            );
        }
    }

    #[test]
    fn blocking_search_sends_storefront_query_and_normalizes_mock_response() {
        let server = MockServer::spawn(vec![json_response("200 OK", SEARCH_FIXTURE)]);
        let client = ApplePodcastsSearchClient::with_search_endpoint(
            server.endpoint.clone(),
            Duration::from_secs(2),
            DEFAULT_MAX_JSON_BYTES,
        )
        .expect("mock client should build");
        let mut request = ApplePodcastsSearchRequest::new("science & history", "GE");
        request.limit = 12;
        request.include_explicit = false;

        let results = client.search(&request).expect("mock search should succeed");
        let requests = server.finish();

        assert_eq!(results.country, "ge");
        assert_eq!(results.podcasts.len(), 1);
        let podcast = &results.podcasts[0];
        assert_eq!(podcast.collection_id, 123_456_789);
        assert_eq!(podcast.title, "Science & History");
        assert_eq!(podcast.author.as_deref(), Some("Fixture Network"));
        assert_eq!(podcast.episode_count, Some(42));
        assert_eq!(podcast.genres, ["Science", "History"]);
        assert_eq!(podcast.explicit, Some(false));
        assert_eq!(
            podcast.feed_url.as_ref().map(Url::as_str),
            Some("https://feeds.example.test/science.xml")
        );

        assert_eq!(requests.len(), 1);
        let pairs = request_query_pairs(&requests[0]);
        assert_eq!(
            pairs.get("term").map(String::as_str),
            Some("science & history")
        );
        assert_eq!(pairs.get("country").map(String::as_str), Some("ge"));
        assert_eq!(pairs.get("entity").map(String::as_str), Some("podcast"));
        assert_eq!(pairs.get("explicit").map(String::as_str), Some("No"));
        assert_eq!(pairs.get("limit").map(String::as_str), Some("12"));
    }

    #[test]
    fn blocking_search_enforces_the_configured_response_bound() {
        let server = MockServer::spawn(vec![json_response("200 OK", SEARCH_FIXTURE)]);
        let client = ApplePodcastsSearchClient::with_search_endpoint(
            server.endpoint.clone(),
            Duration::from_secs(2),
            32,
        )
        .expect("mock client should build");
        let request = ApplePodcastsSearchRequest::new("science", "us");

        assert!(matches!(
            client.search(&request),
            Err(ProviderError::ResponseTooLarge { limit: 32 })
        ));
        assert_eq!(server.finish().len(), 1);
    }

    #[test]
    fn blocking_search_follows_only_same_origin_redirects() {
        let server = MockServer::spawn(vec![
            redirect_response("/redirected"),
            json_response("200 OK", SEARCH_FIXTURE),
        ]);
        let client = ApplePodcastsSearchClient::with_search_endpoint(
            server.endpoint.clone(),
            Duration::from_secs(2),
            DEFAULT_MAX_JSON_BYTES,
        )
        .expect("mock client should build");

        let results = client
            .search(&ApplePodcastsSearchRequest::new("science", "us"))
            .expect("same-origin redirect should succeed");
        let requests = server.finish();

        assert_eq!(results.podcasts.len(), 1);
        assert_eq!(requests.len(), 2);
        assert!(requests[0].starts_with("/search?"));
        assert_eq!(requests[1], "/redirected");
    }

    #[test]
    fn blocking_search_rejects_cross_origin_redirect_before_following_it() {
        let server =
            MockServer::spawn(vec![redirect_response("http://127.0.0.2/private-metadata")]);
        let client = ApplePodcastsSearchClient::with_search_endpoint(
            server.endpoint.clone(),
            Duration::from_secs(2),
            DEFAULT_MAX_JSON_BYTES,
        )
        .expect("mock client should build");

        assert!(matches!(
            client.search(&ApplePodcastsSearchRequest::new("science", "us")),
            Err(ProviderError::InvalidResponse(_))
        ));
        assert_eq!(server.finish().len(), 1);
    }

    #[test]
    fn search_response_count_kind_and_result_limit_are_strict() {
        let mut mismatch: Value =
            serde_json::from_str(SEARCH_FIXTURE).expect("fixture should parse");
        mismatch["resultCount"] = Value::from(2);
        let mismatch =
            serde_json::from_value(mismatch).expect("modified response should deserialize");
        let request = ApplePodcastsSearchRequest::new("science", "us");
        assert!(matches!(
            normalize_search(&request, &mismatch),
            Err(ProviderError::InvalidResponse(_))
        ));

        let mut wrong_kind: Value =
            serde_json::from_str(SEARCH_FIXTURE).expect("fixture should parse");
        wrong_kind["results"][0]["kind"] = Value::String("podcast-episode".to_owned());
        let wrong_kind =
            serde_json::from_value(wrong_kind).expect("modified response should deserialize");
        assert!(matches!(
            normalize_search(&request, &wrong_kind),
            Err(ProviderError::InvalidResponse(_))
        ));

        let mut duplicate: Value =
            serde_json::from_str(SEARCH_FIXTURE).expect("fixture should parse");
        let item = duplicate["results"][0].clone();
        duplicate["resultCount"] = Value::from(2);
        duplicate["results"]
            .as_array_mut()
            .expect("fixture results should be an array")
            .push(item);
        let duplicate =
            serde_json::from_value(duplicate).expect("modified response should deserialize");
        let one_result = ApplePodcastsSearchRequest {
            limit: 1,
            ..ApplePodcastsSearchRequest::new("science", "us")
        };
        assert!(matches!(
            normalize_search(&one_result, &duplicate),
            Err(ProviderError::InvalidResponse(_))
        ));

        let two_results = ApplePodcastsSearchRequest {
            limit: 2,
            ..ApplePodcastsSearchRequest::new("science", "us")
        };
        assert_eq!(
            normalize_search(&two_results, &duplicate)
                .expect("duplicate IDs are collapsed")
                .podcasts
                .len(),
            1
        );
    }

    #[test]
    fn search_rejects_oversized_normalized_fields_and_unsafe_urls() {
        let request = ApplePodcastsSearchRequest::new("science", "us");
        let mut oversized: Value =
            serde_json::from_str(SEARCH_FIXTURE).expect("fixture should parse");
        oversized["results"][0]["collectionName"] =
            Value::String("x".repeat(MAX_SEARCH_TEXT_BYTES + 1));
        oversized["results"][0]["trackName"] = Value::String("x".repeat(MAX_SEARCH_TEXT_BYTES + 1));
        let oversized =
            serde_json::from_value(oversized).expect("modified response should deserialize");
        assert!(matches!(
            normalize_search(&request, &oversized),
            Err(ProviderError::InvalidResponse(_))
        ));

        let mut unsafe_url: Value =
            serde_json::from_str(SEARCH_FIXTURE).expect("fixture should parse");
        unsafe_url["results"][0]["feedUrl"] =
            Value::String("file:///tmp/private-feed.xml".to_owned());
        let unsafe_url =
            serde_json::from_value(unsafe_url).expect("modified response should deserialize");
        assert!(matches!(
            normalize_search(&request, &unsafe_url),
            Err(ProviderError::InvalidResponse(_))
        ));

        for raw in [
            "https://feeds.example.test:8443/show.xml",
            "https://feeds.example.test/show.xml#private",
            "http://127.0.0.1/private-feed.xml",
            "https://10.0.0.1/private-feed.xml",
            "https://169.254.169.254/private-feed.xml",
            "https://[::1]/private-feed.xml",
            "https://feeds.localhost/private-feed.xml",
        ] {
            let mut unsafe_feed: Value =
                serde_json::from_str(SEARCH_FIXTURE).expect("fixture should parse");
            unsafe_feed["results"][0]["feedUrl"] = Value::String(raw.to_owned());
            let unsafe_feed =
                serde_json::from_value(unsafe_feed).expect("modified response should deserialize");
            assert!(
                matches!(
                    normalize_search(&request, &unsafe_feed),
                    Err(ProviderError::InvalidResponse(_))
                ),
                "unsafe feed URL should be rejected: {raw}"
            );
        }
    }

    #[test]
    fn search_rejects_noncanonical_apple_page_hosts_ports_and_fragments() {
        let request = ApplePodcastsSearchRequest::new("science", "us");
        for raw in [
            "https://podcasts.apple.com.evil.test/us/podcast/show/id123456789",
            "https://us.podcasts.apple.com/us/podcast/show/id123456789",
            "http://podcasts.apple.com/us/podcast/show/id123456789",
            "https://podcasts.apple.com:8443/us/podcast/show/id123456789",
            "https://podcasts.apple.com/us/podcast/show/id123456789#private",
        ] {
            let mut unsafe_page: Value =
                serde_json::from_str(SEARCH_FIXTURE).expect("fixture should parse");
            unsafe_page["results"][0]["collectionViewUrl"] = Value::String(raw.to_owned());
            let unsafe_page =
                serde_json::from_value(unsafe_page).expect("modified response should deserialize");
            assert!(
                matches!(
                    normalize_search(&request, &unsafe_page),
                    Err(ProviderError::InvalidResponse(_))
                ),
                "noncanonical Apple page should be rejected: {raw}"
            );
        }
    }

    #[test]
    fn search_client_options_are_bounded_and_need_no_credentials() {
        assert!(ApplePodcastsSearchClient::new().max_json_bytes > 0);
        assert!(matches!(
            ApplePodcastsSearchClient::with_options(Duration::ZERO, DEFAULT_MAX_JSON_BYTES),
            Err(ProviderError::InvalidRequest(_))
        ));
        assert!(matches!(
            ApplePodcastsSearchClient::with_options(Duration::from_secs(1), 0),
            Err(ProviderError::InvalidRequest(_))
        ));
        assert!(matches!(
            ApplePodcastsSearchClient::with_options(
                Duration::from_secs(1),
                MAX_CONFIGURED_JSON_BYTES + 1
            ),
            Err(ProviderError::InvalidRequest(_))
        ));
    }

    #[test]
    fn parses_show_and_episode_url_forms() {
        assert_eq!(
            parse_link("https://podcasts.apple.com/US/podcast/example-show/id1756129194")
                .expect("show link should parse"),
            ApplePodcastLink {
                country: "us".to_owned(),
                collection_id: 1_756_129_194,
                episode_id: None,
            }
        );
        assert_eq!(
            parse_link(
                "https://podcasts.apple.com/gb/podcast/a-fixture-episode/id1756129194?i=1000719462606&uo=4"
            )
            .expect("episode link should parse"),
            ApplePodcastLink {
                country: "gb".to_owned(),
                collection_id: 1_756_129_194,
                episode_id: Some(1_000_719_462_606),
            }
        );
        assert_eq!(
            parse_link("https://podcasts.apple.com/id/podcast/id1732052641/")
                .expect("slugless link should parse")
                .country,
            "id"
        );
    }

    #[test]
    fn rejects_lookalikes_credentials_and_malformed_ids() {
        for raw in [
            "https://podcasts.apple.com.evil.test/us/podcast/show/id123",
            "https://user:secret@podcasts.apple.com/us/podcast/show/id123",
            "http://podcasts.apple.com/us/podcast/show/id123",
            "https://podcasts.apple.com:8443/us/podcast/show/id123",
            "https://podcasts.apple.com/us/podcast/show/id123#private",
            "https://podcasts.apple.com/usa/podcast/show/id123",
            "https://podcasts.apple.com/us/channel/show/id123",
            "https://podcasts.apple.com/us/podcast/show/id0",
            "https://podcasts.apple.com/us/podcast/show/id123?i=abc",
            "https://podcasts.apple.com/us/podcast/show/id123?i=1&i=2",
        ] {
            assert!(
                parse_link(raw).is_err(),
                "fixture link should be rejected: {raw}"
            );
        }
    }

    #[test]
    fn lookup_url_uses_documented_https_parameters() {
        let link =
            parse_link("https://podcasts.apple.com/gb/podcast/show/id1756129194?i=1000719462606")
                .expect("fixture link should parse");
        let endpoint = Url::parse(LOOKUP_ENDPOINT).expect("built-in lookup URL should parse");
        let url = build_lookup_url(&endpoint, &link);
        let pairs = url.query_pairs().collect::<Vec<_>>();

        assert_eq!(url.as_str().split('?').next(), Some(LOOKUP_ENDPOINT));
        assert!(pairs.contains(&("id".into(), "1756129194".into())));
        assert!(pairs.contains(&("country".into(), "gb".into())));
        assert!(pairs.contains(&("media".into(), "podcast".into())));
        assert!(pairs.contains(&("entity".into(), "podcastEpisode".into())));
        assert!(pairs.contains(&("limit".into(), "200".into())));
    }

    #[test]
    fn blocking_collection_lookup_returns_all_mock_episodes_in_order() {
        let server = MockServer::spawn(vec![
            json_response("200 OK", SHOW_EPISODES_FIXTURE),
            json_response("200 OK", SHOW_EPISODES_FIXTURE),
        ]);
        let resolver = ApplePodcastsResolver::with_lookup_endpoint(
            server.endpoint.clone(),
            Duration::from_secs(2),
            DEFAULT_MAX_JSON_BYTES,
        )
        .expect("mock resolver should build");

        let resolved = resolver
            .resolve_collection("GB", 1_756_129_194)
            .expect("mock collection should resolve");
        let show_url =
            Url::parse("https://podcasts.apple.com/gb/podcast/example-show/id1756129194")
                .expect("fixture show URL should parse");
        let resolved_from_url = resolver
            .resolve_show(&show_url)
            .expect("mock show URL should resolve");
        let requests = server.finish();

        assert_eq!(resolved.link.country, "gb");
        assert_eq!(resolved.link.episode_id, None);
        assert_eq!(resolved.podcast.title, "Example Show");
        assert_eq!(resolved.episodes.len(), 2);
        assert_eq!(
            resolved
                .episodes
                .iter()
                .map(|episode| episode.title.as_str())
                .collect::<Vec<_>>(),
            ["Newest Fixture Episode", "Older Fixture Episode"]
        );
        assert_eq!(resolved.episodes[0].duration_seconds, Some(5_157));
        assert_eq!(resolved.episodes[1].duration_seconds, Some(3_600));
        assert_eq!(
            resolved.episodes[1].media_url.as_ref().map(Url::as_str),
            Some("https://cdn.example.test/older.m4a")
        );
        assert_eq!(resolved_from_url, resolved);

        assert_eq!(requests.len(), 2);
        let pairs = request_query_pairs(&requests[0]);
        assert_eq!(pairs.get("id").map(String::as_str), Some("1756129194"));
        assert_eq!(pairs.get("country").map(String::as_str), Some("gb"));
        assert_eq!(pairs.get("media").map(String::as_str), Some("podcast"));
        assert_eq!(
            pairs.get("entity").map(String::as_str),
            Some("podcastEpisode")
        );
        assert_eq!(pairs.get("limit").map(String::as_str), Some("200"));
        assert_eq!(request_query_pairs(&requests[1]), pairs);
    }

    #[test]
    fn show_listing_rejects_episode_urls_invalid_collections_and_duplicates() {
        let resolver = ApplePodcastsResolver::new();
        let episode_url =
            Url::parse("https://podcasts.apple.com/us/podcast/show/id1756129194?i=1000719462606")
                .expect("fixture URL should parse");
        assert!(matches!(
            resolver.resolve_show(&episode_url),
            Err(ProviderError::InvalidRequest(_))
        ));
        assert!(matches!(
            resolver.resolve_collection("usa", 1),
            Err(ProviderError::InvalidRequest(_))
        ));
        assert!(matches!(
            resolver.resolve_collection("us", 0),
            Err(ProviderError::InvalidRequest(_))
        ));

        let mut duplicate: Value =
            serde_json::from_str(SHOW_EPISODES_FIXTURE).expect("fixture should parse");
        let duplicate_id = duplicate["results"][1]["trackId"].clone();
        duplicate["results"][2]["trackId"] = duplicate_id;
        let response =
            serde_json::from_value(duplicate).expect("modified response should deserialize");
        let link = ApplePodcastLink {
            country: "us".to_owned(),
            collection_id: 1_756_129_194,
            episode_id: None,
        };
        assert!(matches!(
            normalize_show_lookup(link, &response),
            Err(ProviderError::InvalidResponse(_))
        ));
    }

    #[test]
    fn lookup_rejects_noncanonical_episode_page_from_mock_payload() {
        for raw in [
            "https://episodes.podcasts.apple.com/gb/podcast/episode/id1756129194?i=1000719462606",
            "https://podcasts.apple.com:8443/gb/podcast/episode/id1756129194?i=1000719462606",
        ] {
            let mut unsafe_response: Value =
                serde_json::from_str(SHOW_EPISODES_FIXTURE).expect("fixture should parse");
            unsafe_response["results"][1]["trackViewUrl"] = Value::String(raw.to_owned());
            let response = serde_json::from_value(unsafe_response)
                .expect("modified response should deserialize");
            let link = ApplePodcastLink {
                country: "gb".to_owned(),
                collection_id: 1_756_129_194,
                episode_id: None,
            };
            assert!(
                matches!(
                    normalize_show_lookup(link, &response),
                    Err(ProviderError::InvalidResponse(_))
                ),
                "noncanonical episode page should be rejected: {raw}"
            );
        }
    }

    #[test]
    fn blocking_show_listing_enforces_the_configured_response_bound() {
        let server = MockServer::spawn(vec![json_response("200 OK", SHOW_EPISODES_FIXTURE)]);
        let resolver = ApplePodcastsResolver::with_lookup_endpoint(
            server.endpoint.clone(),
            Duration::from_secs(2),
            64,
        )
        .expect("mock resolver should build");

        assert!(matches!(
            resolver.resolve_collection("us", 1_756_129_194),
            Err(ProviderError::ResponseTooLarge { limit: 64 })
        ));
        assert_eq!(server.finish().len(), 1);
    }

    #[test]
    fn fixture_normalizes_show_episode_feed_and_media_metadata() {
        let response = serde_json::from_str(EPISODE_LOOKUP_FIXTURE).expect("fixture should parse");
        let link =
            parse_link("https://podcasts.apple.com/us/podcast/show/id1756129194?i=1000719462606")
                .expect("fixture link should parse");
        let resolved = normalize_lookup(link, &response).expect("fixture should normalize");

        assert_eq!(resolved.podcast.title, "Example Show");
        assert_eq!(resolved.podcast.episode_count, Some(19));
        assert_eq!(resolved.podcast.genres, ["Society & Culture", "Podcasts"]);
        assert_eq!(
            resolved
                .podcast
                .feed_url
                .as_ref()
                .expect("fixture has feedUrl")
                .as_str(),
            "https://feeds.example.test/show.xml"
        );
        let episode = resolved
            .episode
            .expect("episode link should resolve episode");
        assert_eq!(episode.episode_id, 1_000_719_462_606);
        assert_eq!(episode.duration_seconds, Some(5_157));
        assert_eq!(episode.explicit, Some(true));
        assert_eq!(
            episode.media_url.expect("fixture has episodeUrl").as_str(),
            "https://cdn.example.test/episode.mp3"
        );
    }

    #[test]
    fn show_link_does_not_select_an_episode() {
        let response = serde_json::from_str(EPISODE_LOOKUP_FIXTURE).expect("fixture should parse");
        let link = parse_link("https://podcasts.apple.com/us/podcast/show/id1756129194")
            .expect("fixture link should parse");
        let resolved = normalize_lookup(link, &response).expect("fixture should normalize");

        assert!(resolved.episode.is_none());
    }

    #[test]
    fn requested_episode_must_match_collection_and_window() {
        let response = serde_json::from_str(EPISODE_LOOKUP_FIXTURE).expect("fixture should parse");
        let link =
            parse_link("https://podcasts.apple.com/us/podcast/show/id1756129194?i=1000000000000")
                .expect("fixture link should parse");

        assert!(matches!(
            normalize_lookup(link, &response),
            Err(ProviderError::InvalidResponse(_))
        ));
    }

    #[test]
    fn unsafe_urls_in_lookup_metadata_are_rejected() {
        let link = parse_link("https://podcasts.apple.com/us/podcast/show/id1756129194")
            .expect("fixture link should parse");

        for (result_index, field, raw) in [
            (0, "feedUrl", "file:///tmp/feed.xml"),
            (0, "feedUrl", "http://127.0.0.1/private-feed.xml"),
            (0, "artworkUrl600", "https://10.0.0.1/private.jpg"),
            (1, "episodeUrl", "https://169.254.169.254/private.mp3"),
            (1, "artworkUrl100", "https://[::1]/private.jpg"),
        ] {
            let mut fixture: Value =
                serde_json::from_str(EPISODE_LOOKUP_FIXTURE).expect("fixture should parse");
            fixture["results"][result_index][field] = Value::String(raw.to_owned());
            let response = serde_json::from_value(fixture).expect("modified fixture should parse");
            assert!(
                matches!(
                    normalize_show_lookup(link.clone(), &response),
                    Err(ProviderError::InvalidResponse(_))
                ),
                "unsafe {field} should be rejected: {raw}"
            );
        }
    }

    struct MockServer {
        endpoint: Url,
        requests: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl MockServer {
        fn spawn(responses: Vec<String>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("mock server should bind");
            let address = listener.local_addr().expect("mock address should exist");
            listener
                .set_nonblocking(true)
                .expect("mock listener should become nonblocking");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let thread_requests = Arc::clone(&requests);
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = Arc::clone(&stop);
            let thread = thread::spawn(move || {
                for response in responses {
                    let mut stream = loop {
                        match listener.accept() {
                            // BSD and macOS let an accepted socket inherit the
                            // listener's non-blocking flag, while Linux does
                            // not. Clearing it keeps the blocking reads below
                            // identical on every platform.
                            Ok((stream, _)) => {
                                stream
                                    .set_nonblocking(false)
                                    .expect("mock stream should become blocking");
                                break stream;
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                if thread_stop.load(Ordering::Relaxed) {
                                    return;
                                }
                                thread::sleep(Duration::from_millis(2));
                            }
                            Err(error) => panic!("mock should accept request: {error}"),
                        }
                    };
                    let request = request_target(&stream);
                    thread_requests
                        .lock()
                        .expect("request lock should not be poisoned")
                        .push(request);
                    stream
                        .write_all(response.as_bytes())
                        .expect("mock should write response");
                    stream.flush().expect("mock should flush response");
                }
            });
            Self {
                endpoint: Url::parse(&format!("http://{address}/search"))
                    .expect("mock URL should parse"),
                requests,
                stop,
                thread: Some(thread),
            }
        }

        fn completed_requests(&self) -> Vec<String> {
            self.requests
                .lock()
                .expect("request lock should not be poisoned")
                .clone()
        }

        fn finish(mut self) -> Vec<String> {
            if let Some(thread) = self.thread.take() {
                thread.join().expect("mock server should stop");
            }
            self.completed_requests()
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                thread.join().expect("mock server should stop");
            }
        }
    }

    fn request_target(stream: &TcpStream) -> String {
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        reader
            .read_line(&mut request_line)
            .expect("mock request line should be readable");
        loop {
            let mut header = String::new();
            reader
                .read_line(&mut header)
                .expect("mock header should be readable");
            if header == "\r\n" || header.is_empty() {
                break;
            }
        }
        request_line
            .split_ascii_whitespace()
            .nth(1)
            .expect("request target should exist")
            .to_owned()
    }

    fn json_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn redirect_response(location: &str) -> String {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
        )
    }

    fn request_query_pairs(request_target: &str) -> HashMap<String, String> {
        Url::parse(&format!("http://mock.test{request_target}"))
            .expect("captured target should be a relative URL")
            .query_pairs()
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect()
    }
}
