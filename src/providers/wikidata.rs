//! Lazy Wikidata lookups for media and channel identifiers.
//!
//! Youta queries the public Wikidata Query Service only after an item is
//! selected. Calls are blocking and bounded, so callers must run them on the
//! provider worker and cache both positive and empty results.

use serde::{Deserialize, Serialize};
use url::Url;

use super::{
    DEFAULT_REQUEST_TIMEOUT, ProviderError, get_bounded_json, provider_agent,
    validate_youtube_video_id,
};
use crate::domain::WikidataLink;

const ENDPOINT: &str = "https://query.wikidata.org/sparql";
const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const MAX_RESULTS: usize = 20;

/// External media identifier property used for an exact Wikidata lookup.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WikidataExternalKind {
    /// YouTube video ID, represented by Wikidata property P1651.
    YouTubeVideo,
    /// YouTube channel ID, represented by Wikidata property P2397.
    YouTubeChannel,
    /// SoundCloud path identifier, represented by Wikidata property P3040.
    SoundCloud,
    /// Bilibili video ID, represented by Wikidata property P6456.
    BilibiliVideo,
    /// Bilibili user/channel ID, represented by Wikidata property P6455.
    BilibiliChannel,
}

impl WikidataExternalKind {
    /// Returns the stable Wikidata property identifier.
    #[must_use]
    pub const fn property_id(self) -> &'static str {
        match self {
            Self::YouTubeVideo => "P1651",
            Self::YouTubeChannel => "P2397",
            Self::SoundCloud => "P3040",
            Self::BilibiliVideo => "P6456",
            Self::BilibiliChannel => "P6455",
        }
    }
}

/// Exact external-ID lookup result returned by Wikidata.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WikidataExternalLookup {
    /// External-ID property queried.
    pub kind: WikidataExternalKind,
    /// Validated provider identifier.
    pub external_id: String,
    /// Matching Wikidata items, capped at twenty.
    pub items: Vec<WikidataLink>,
}

/// Bounded client for the public Wikidata Query Service.
#[derive(Clone)]
pub struct WikidataProvider {
    agent: ureq::Agent,
    max_response_bytes: usize,
}

impl WikidataProvider {
    /// Creates a client with the common provider timeout and a 512 KiB result
    /// limit.
    #[must_use]
    pub fn new() -> Self {
        Self {
            agent: provider_agent(DEFAULT_REQUEST_TIMEOUT),
            max_response_bytes: MAX_RESPONSE_BYTES,
        }
    }

    /// Looks up items whose exact external identifier matches the selected
    /// media or channel property.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid identifier, a failed or oversized
    /// Wikidata response, or malformed entity data.
    pub fn lookup_external(
        &self,
        kind: WikidataExternalKind,
        external_id: &str,
    ) -> Result<WikidataExternalLookup, ProviderError> {
        validate_external_id(kind, external_id)?;
        let url = build_query_url(kind, external_id)?;
        let response: SparqlResponse =
            get_bounded_json(&self.agent, &url, self.max_response_bytes)?;
        normalize_response(kind, external_id, response)
    }
}

impl Default for WikidataProvider {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_external_id(
    kind: WikidataExternalKind,
    external_id: &str,
) -> Result<(), ProviderError> {
    match kind {
        WikidataExternalKind::YouTubeVideo => validate_youtube_video_id(external_id),
        WikidataExternalKind::YouTubeChannel => {
            let valid = external_id.len() == 24
                && external_id.starts_with("UC")
                && external_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
            if valid {
                Ok(())
            } else {
                Err(ProviderError::InvalidRequest(
                    "YouTube channel ID must be a 24-character UC identifier".to_owned(),
                ))
            }
        }
        WikidataExternalKind::SoundCloud => {
            let valid = !external_id.is_empty()
                && external_id.len() <= 512
                && !external_id.starts_with('/')
                && !external_id.ends_with('/')
                && external_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'/'));
            if valid {
                Ok(())
            } else {
                Err(ProviderError::InvalidRequest(
                    "SoundCloud ID must contain only path letters, digits, slash, dash, or underscore"
                        .to_owned(),
                ))
            }
        }
        WikidataExternalKind::BilibiliVideo => {
            let valid_av = external_id
                .strip_prefix("av")
                .is_some_and(is_positive_decimal);
            let valid_bv = external_id.len() == 12
                && external_id.starts_with("BV")
                && external_id[2..]
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric());
            if valid_av || valid_bv {
                Ok(())
            } else {
                Err(ProviderError::InvalidRequest(
                    "Bilibili video ID must be an av number or a 12-character BV identifier"
                        .to_owned(),
                ))
            }
        }
        WikidataExternalKind::BilibiliChannel => {
            if is_positive_decimal(external_id) {
                Ok(())
            } else {
                Err(ProviderError::InvalidRequest(
                    "Bilibili channel ID must be a positive numeric UID".to_owned(),
                ))
            }
        }
    }
}

fn is_positive_decimal(value: &str) -> bool {
    !value.is_empty() && !value.starts_with('0') && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// Extracts a Wikidata-compatible SoundCloud account or track path.
///
/// Short redirect links are intentionally excluded because resolving them
/// belongs on the provider worker and must retain the same network bounds.
#[must_use]
pub fn soundcloud_external_id(url: &Url) -> Option<String> {
    if !matches!(
        url.host_str(),
        Some("soundcloud.com" | "www.soundcloud.com" | "m.soundcloud.com")
    ) {
        return None;
    }
    let segments = url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if !(1..=2).contains(&segments.len()) {
        return None;
    }
    let external_id = segments.join("/");
    validate_external_id(WikidataExternalKind::SoundCloud, &external_id)
        .is_ok()
        .then_some(external_id)
}

/// Extracts an exact Bilibili video ID from a canonical video URL.
#[must_use]
pub fn bilibili_video_external_id(url: &Url) -> Option<String> {
    if !matches!(url.host_str(), Some("bilibili.com" | "www.bilibili.com")) {
        return None;
    }
    let mut segments = url.path_segments()?.filter(|segment| !segment.is_empty());
    if segments.next()? != "video" {
        return None;
    }
    let external_id = segments.next()?.to_owned();
    validate_external_id(WikidataExternalKind::BilibiliVideo, &external_id)
        .is_ok()
        .then_some(external_id)
}

/// Extracts an exact Bilibili user/channel UID from a canonical space URL.
#[must_use]
pub fn bilibili_channel_external_id(url: &Url) -> Option<String> {
    if url.host_str() != Some("space.bilibili.com") {
        return None;
    }
    let external_id = url
        .path_segments()?
        .find(|segment| !segment.is_empty())?
        .to_owned();
    validate_external_id(WikidataExternalKind::BilibiliChannel, &external_id)
        .is_ok()
        .then_some(external_id)
}

fn build_query_url(kind: WikidataExternalKind, external_id: &str) -> Result<Url, ProviderError> {
    let identifiers = if kind == WikidataExternalKind::SoundCloud {
        let account = external_id.split('/').next().unwrap_or(external_id);
        if account == external_id {
            format!(r#""{external_id}""#)
        } else {
            format!(r#""{external_id}" "{account}""#)
        }
    } else {
        format!(r#""{external_id}""#)
    };
    let query = format!(
        r#"SELECT ?item ?itemLabel ?itemDescription WHERE {{
  VALUES ?externalId {{ {identifiers} }}
  ?item wdt:{} ?externalId .
  SERVICE wikibase:label {{
    bd:serviceParam wikibase:language "[AUTO_LANGUAGE],en" .
  }}
}}
LIMIT {MAX_RESULTS}"#,
        kind.property_id()
    );
    let mut url =
        Url::parse(ENDPOINT).map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    url.query_pairs_mut()
        .append_pair("query", &query)
        .append_pair("format", "json");
    Ok(url)
}

fn normalize_response(
    kind: WikidataExternalKind,
    external_id: &str,
    response: SparqlResponse,
) -> Result<WikidataExternalLookup, ProviderError> {
    if response.results.bindings.len() > MAX_RESULTS {
        return Err(ProviderError::InvalidResponse(format!(
            "Wikidata returned more than {MAX_RESULTS} items"
        )));
    }
    let mut items = Vec::with_capacity(response.results.bindings.len());
    for binding in response.results.bindings {
        let item_id = wikidata_item_id(&binding.item.value)?;
        if items
            .iter()
            .any(|item: &WikidataLink| item.item_id == item_id)
        {
            continue;
        }
        let label = binding
            .item_label
            .map(|value| value.value)
            .filter(|value| !value.trim().is_empty() && value != &item_id)
            .unwrap_or_else(|| item_id.clone());
        let description = binding
            .item_description
            .map(|value| value.value)
            .filter(|value| !value.trim().is_empty());
        let url = Url::parse(&format!("https://www.wikidata.org/wiki/{item_id}"))
            .expect("a validated Q identifier always forms a valid Wikidata URL");
        items.push(WikidataLink {
            item_id,
            label,
            description,
            url,
        });
    }
    Ok(WikidataExternalLookup {
        kind,
        external_id: external_id.to_owned(),
        items,
    })
}

fn wikidata_item_id(uri: &str) -> Result<String, ProviderError> {
    let url = Url::parse(uri)
        .map_err(|error| ProviderError::InvalidResponse(format!("invalid item URI: {error}")))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str() != Some("www.wikidata.org")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ProviderError::InvalidResponse(
            "Wikidata result contains a foreign item URI".to_owned(),
        ));
    }
    let Some(item_id) = url.path().strip_prefix("/entity/") else {
        return Err(ProviderError::InvalidResponse(
            "Wikidata item URI has an unexpected path".to_owned(),
        ));
    };
    let valid = item_id.len() >= 2
        && item_id.starts_with('Q')
        && item_id[1..].bytes().all(|byte| byte.is_ascii_digit())
        && item_id != "Q0";
    if !valid {
        return Err(ProviderError::InvalidResponse(
            "Wikidata result contains an invalid Q identifier".to_owned(),
        ));
    }
    Ok(item_id.to_owned())
}

#[derive(Debug, Deserialize)]
struct SparqlResponse {
    results: SparqlResults,
}

#[derive(Debug, Deserialize)]
struct SparqlResults {
    #[serde(default)]
    bindings: Vec<SparqlBinding>,
}

#[derive(Debug, Deserialize)]
struct SparqlBinding {
    item: SparqlValue,
    #[serde(rename = "itemLabel")]
    item_label: Option<SparqlValue>,
    #[serde(rename = "itemDescription")]
    item_description: Option<SparqlValue>,
}

#[derive(Debug, Deserialize)]
struct SparqlValue {
    value: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_uses_exact_property_and_encoded_identifier() {
        let url =
            build_query_url(WikidataExternalKind::YouTubeVideo, "dQw4w9WgXcQ").expect("query URL");
        let pairs = url.query_pairs().collect::<Vec<_>>();
        let query = pairs
            .iter()
            .find(|(key, _)| key == "query")
            .map(|(_, value)| value.as_ref())
            .expect("SPARQL query");
        assert!(query.contains("VALUES ?externalId { \"dQw4w9WgXcQ\" }"));
        assert!(query.contains("wdt:P1651 ?externalId"));
        assert!(query.contains("LIMIT 20"));
    }

    #[test]
    fn video_and_channel_validation_are_strict() {
        assert!(validate_external_id(WikidataExternalKind::YouTubeVideo, "dQw4w9WgXcQ").is_ok());
        assert!(
            validate_external_id(
                WikidataExternalKind::YouTubeChannel,
                "UCXUCegBr2GL7mo6O1l-xvVw"
            )
            .is_ok()
        );
        assert!(
            validate_external_id(WikidataExternalKind::YouTubeChannel, "channel-name").is_err()
        );
        assert!(
            validate_external_id(
                WikidataExternalKind::SoundCloud,
                "oliviagobrien/trust-issues"
            )
            .is_ok()
        );
        assert!(validate_external_id(WikidataExternalKind::BilibiliVideo, "BV1xx411c7mD").is_ok());
        assert!(validate_external_id(WikidataExternalKind::BilibiliVideo, "av170001").is_ok());
        assert!(validate_external_id(WikidataExternalKind::BilibiliVideo, "AV170001").is_err());
        assert!(validate_external_id(WikidataExternalKind::BilibiliChannel, "546195").is_ok());
        assert!(validate_external_id(WikidataExternalKind::BilibiliChannel, "0").is_err());
    }

    #[test]
    fn provider_urls_yield_exact_external_ids() {
        let soundcloud = Url::parse("https://soundcloud.com/oliviagobrien/trust-issues?ref=share")
            .expect("SoundCloud URL");
        assert_eq!(
            soundcloud_external_id(&soundcloud).as_deref(),
            Some("oliviagobrien/trust-issues")
        );

        let bilibili_video =
            Url::parse("https://www.bilibili.com/video/BV1xx411c7mD/").expect("Bilibili URL");
        assert_eq!(
            bilibili_video_external_id(&bilibili_video).as_deref(),
            Some("BV1xx411c7mD")
        );
        let bilibili_channel =
            Url::parse("https://space.bilibili.com/546195/video").expect("Bilibili space URL");
        assert_eq!(
            bilibili_channel_external_id(&bilibili_channel).as_deref(),
            Some("546195")
        );
        assert!(bilibili_video_external_id(&bilibili_channel).is_none());
    }

    #[test]
    fn bilibili_queries_use_the_distinct_video_and_channel_properties() {
        let video =
            build_query_url(WikidataExternalKind::BilibiliVideo, "av170001").expect("video query");
        let channel = build_query_url(WikidataExternalKind::BilibiliChannel, "546195")
            .expect("channel query");
        assert!(
            video
                .query_pairs()
                .any(|(key, value)| key == "query" && value.contains("wdt:P6456"))
        );
        assert!(
            channel
                .query_pairs()
                .any(|(key, value)| key == "query" && value.contains("wdt:P6455"))
        );
    }

    #[test]
    fn fixture_normalizes_https_links_and_optional_text() {
        let response: SparqlResponse = serde_json::from_str(
            r#"{
              "results": {
                "bindings": [{
                  "item": {"type": "uri", "value": "http://www.wikidata.org/entity/Q60231842"},
                  "itemLabel": {"type": "literal", "value": "Thank U, Next"},
                  "itemDescription": {"type": "literal", "value": "music video"}
                }]
              }
            }"#,
        )
        .expect("fixture JSON");
        let lookup =
            normalize_response(WikidataExternalKind::YouTubeVideo, "gl1aHhXnN1k", response)
                .expect("normalized lookup");
        assert_eq!(lookup.items[0].item_id, "Q60231842");
        assert_eq!(
            lookup.items[0].url.as_str(),
            "https://www.wikidata.org/wiki/Q60231842"
        );
        assert_eq!(lookup.items[0].description.as_deref(), Some("music video"));
    }

    #[test]
    fn foreign_and_malformed_entity_uris_are_rejected() {
        assert!(wikidata_item_id("https://evil.test/entity/Q42").is_err());
        assert!(wikidata_item_id("https://www.wikidata.org/wiki/Q42").is_err());
        assert!(wikidata_item_id("http://www.wikidata.org/entity/not-q").is_err());
    }
}
