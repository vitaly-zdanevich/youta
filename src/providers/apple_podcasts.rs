//! Apple Podcasts direct-link resolver using the public iTunes Lookup API.
//!
//! The resolver accepts public `https://podcasts.apple.com` show and episode
//! links, then performs an ID-based lookup through Apple's documented
//! `https://itunes.apple.com/lookup` endpoint. Calls are blocking and bounded;
//! they belong on Youta's provider worker and should be cached by the caller to
//! respect Apple's public API limits.
//!
//! This module does not authenticate an Apple account and does not claim to
//! read or synchronize follows, playback history, or played status.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use super::{
    DEFAULT_MAX_JSON_BYTES, DEFAULT_REQUEST_TIMEOUT, ProviderError, get_bounded_json,
    provider_agent,
};

const LOOKUP_ENDPOINT: &str = "https://itunes.apple.com/lookup";
const MAX_CONFIGURED_JSON_BYTES: usize = 64 * 1024 * 1024;
const LOOKUP_EPISODE_LIMIT: u16 = 200;
// One collection record plus the requested maximum of 200 episodes.
const MAX_LOOKUP_RESULTS: usize = 201;

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

/// Blocking client for Apple Podcasts direct-link resolution.
#[derive(Clone)]
pub struct ApplePodcastsResolver {
    agent: ureq::Agent,
    max_json_bytes: usize,
}

impl ApplePodcastsResolver {
    /// Creates a resolver with conservative timeout and response limits.
    #[must_use]
    pub fn new() -> Self {
        Self {
            agent: provider_agent(DEFAULT_REQUEST_TIMEOUT),
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
        Ok(Self {
            agent: provider_agent(timeout),
            max_json_bytes,
        })
    }

    /// Resolves a public Apple Podcasts show or episode URL.
    ///
    /// The lookup contains the podcast plus at most Apple's documented maximum
    /// of 200 associated episodes. A direct episode older than that window may
    /// therefore be absent; the resolver reports that explicitly instead of
    /// calling an undocumented Apple endpoint.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] when the direct URL is invalid, the bounded
    /// HTTPS lookup fails, Apple returns malformed metadata, the collection
    /// does not match the URL, or a requested episode is outside the returned
    /// lookup window.
    pub fn resolve(&self, url: &Url) -> Result<ResolvedApplePodcast, ProviderError> {
        let link = ApplePodcastLink::parse(url)?;
        let lookup_url = build_lookup_url(&link)?;
        let response: RawLookupResponse =
            get_bounded_json(&self.agent, &lookup_url, self.max_json_bytes)?;
        normalize_lookup(link, &response)
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
        return Err(invalid_link("Apple Podcasts links must not specify a port"));
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

fn build_lookup_url(link: &ApplePodcastLink) -> Result<Url, ProviderError> {
    let mut url = Url::parse(LOOKUP_ENDPOINT)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("id", &link.collection_id.to_string());
        query.append_pair("country", &link.country);
        query.append_pair("media", "podcast");
        query.append_pair("entity", "podcastEpisode");
        query.append_pair("limit", &LOOKUP_EPISODE_LIMIT.to_string());
    }
    Ok(url)
}

fn normalize_lookup(
    link: ApplePodcastLink,
    response: &RawLookupResponse,
) -> Result<ResolvedApplePodcast, ProviderError> {
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
                        "Apple lookup did not return requested episode {episode_id}; it may be outside the latest {LOOKUP_EPISODE_LIMIT} episodes"
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

    Ok(ApplePodcastMetadata {
        collection_id,
        country: country.to_owned(),
        title,
        author: nonempty(item.artist_name.as_deref()).map(ToOwned::to_owned),
        feed_url: parse_optional_remote_url(item.feed_url.as_deref(), "feedUrl")?,
        webpage_url: parse_optional_remote_url(
            item.collection_view_url
                .as_deref()
                .or(item.track_view_url.as_deref()),
            "podcast view URL",
        )?,
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
        webpage_url: parse_optional_remote_url(item.track_view_url.as_deref(), "episode view URL")?,
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

fn parse_optional_remote_url(raw: Option<&str>, field: &str) -> Result<Option<Url>, ProviderError> {
    let Some(raw) = nonempty(raw) else {
        return Ok(None);
    };
    let url = Url::parse(raw).map_err(|error| {
        ProviderError::InvalidResponse(format!("Apple {field} is invalid: {error}"))
    })?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(ProviderError::InvalidResponse(format!(
            "Apple {field} must be a credential-free HTTP(S) URL"
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
    use super::*;

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
        let url = build_lookup_url(&link).expect("lookup URL should build");
        let pairs = url.query_pairs().collect::<Vec<_>>();

        assert_eq!(url.as_str().split('?').next(), Some(LOOKUP_ENDPOINT));
        assert!(pairs.contains(&("id".into(), "1756129194".into())));
        assert!(pairs.contains(&("country".into(), "gb".into())));
        assert!(pairs.contains(&("media".into(), "podcast".into())));
        assert!(pairs.contains(&("entity".into(), "podcastEpisode".into())));
        assert!(pairs.contains(&("limit".into(), "200".into())));
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
        let mut fixture: Value =
            serde_json::from_str(EPISODE_LOOKUP_FIXTURE).expect("fixture should parse");
        fixture["results"][0]["feedUrl"] = Value::String("file:///tmp/feed.xml".to_owned());
        let response = serde_json::from_value(fixture).expect("modified fixture should parse");
        let link = parse_link("https://podcasts.apple.com/us/podcast/show/id1756129194")
            .expect("fixture link should parse");

        assert!(matches!(
            normalize_lookup(link, &response),
            Err(ProviderError::InvalidResponse(_))
        ));
    }
}
