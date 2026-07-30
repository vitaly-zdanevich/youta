//! Lazy Wikidata lookups for media and channel identifiers.
//!
//! Youta queries the public Wikidata Query Service only after an item is
//! selected. Calls are blocking and bounded, so callers must run them on the
//! provider worker and cache both positive and empty results.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use super::{
    DEFAULT_REQUEST_TIMEOUT, ProviderError, get_bounded_json, provider_agent,
    validate_youtube_video_id,
};
use crate::domain::WikidataLink;

const ENDPOINT: &str = "https://query.wikidata.org/sparql";
const ENTITY_API_ENDPOINT: &str = "https://www.wikidata.org/w/api.php";
const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const MAX_RESULTS: usize = 20;
const MAX_ENTITY_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_LABEL_RESPONSE_BYTES: usize = 512 * 1024;
const MAX_FORMATTER_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_STATEMENT_PROPERTIES: usize = 256;
const MAX_VALUES_PER_PROPERTY: usize = 128;
const MAX_STATEMENT_VALUES: usize = 1_024;
const MAX_WIKIPEDIA_SITELINKS: usize = 512;
const MAX_LABEL_IDS: usize = 50;
const MAX_LABEL_ENTITY_IDS: usize = MAX_STATEMENT_PROPERTIES + MAX_STATEMENT_VALUES;
/// Maximum qualifier snaks inspected for one P8687 follower observation.
const MAX_FOLLOWER_QUALIFIER_SNAKS: usize = 16;
const MAX_VALUE_BYTES: usize = 4 * 1024;
const COMMONS_CATEGORY_PAGE_BASE: &str = "https://commons.wikimedia.org/wiki/Category:";
const COMMONS_FILE_PAGE_BASE: &str = "https://commons.wikimedia.org/wiki/File:";
const COMMONS_FILE_PREVIEW_BASE: &str = "https://commons.wikimedia.org/wiki/Special:Redirect/file/";
const COMMONS_PREVIEW_WIDTH: &str = "512";

/// External media identifier property used for an exact Wikidata lookup.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WikidataExternalKind {
    /// `YouTube` video ID, represented by Wikidata property P1651.
    YouTubeVideo,
    /// `YouTube` channel ID, represented by Wikidata property P2397.
    YouTubeChannel,
    /// `SoundCloud` path identifier, represented by Wikidata property P3040.
    SoundCloud,
    /// MusicBrainz recording UUID, represented by Wikidata property P4404.
    MusicBrainzRecording,
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
            Self::MusicBrainzRecording => "P4404",
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

/// Human-facing statements loaded for one exact Wikidata item.
///
/// Claims are grouped by property while preserving the order of values within
/// each property. P8687 service/date qualifiers and deprecated rank are
/// consumed to produce readable follower-history rows. Raw qualifier values,
/// references, statement IDs, numeric entity IDs, hashes, and other Wikibase
/// implementation metadata are deliberately omitted.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WikidataEntityStatements {
    /// Validated Wikidata item identifier.
    pub item_id: String,
    /// Bounded property groups in stable property-ID order.
    pub statements: Vec<WikidataStatement>,
    /// Canonical Wikipedia articles supplied by Wikidata for this item.
    ///
    /// Only validated HTTPS `*.wikipedia.org/wiki/…` targets are retained.
    /// The collection has an independent bound so a broadly translated item
    /// cannot consume the statement-value budget.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wikipedia_sitelinks: Vec<WikidataWikipediaSitelink>,
    /// Whether otherwise valid Wikipedia sitelinks exceeded their own bound.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub wikipedia_sitelinks_omitted: bool,
    /// Whether a property, value, label, or overlong display value was omitted.
    ///
    /// An oversized HTTP response remains an error because it cannot be safely
    /// parsed far enough to return a trustworthy partial result.
    pub truncated: bool,
    /// Whether a documented structural or display-size hard bound was reached.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hard_bounds_reached: bool,
    /// Whether an empty or unsupported structured value was omitted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unsupported_values_omitted: bool,
}

/// One canonical Wikipedia article linked from an exact Wikidata item.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WikidataWikipediaSitelink {
    /// Stable Wikibase site identifier, such as `enwiki`.
    pub site_id: String,
    /// Readable canonical Wikipedia hostname.
    pub project_label: String,
    /// Human-facing article title supplied by Wikidata.
    pub title: String,
    /// Validated canonical HTTPS article URL supplied by Wikidata.
    pub url: Url,
}

/// One Wikidata property and all bounded, human-facing main values.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WikidataStatement {
    /// Stable property identifier, such as `P31`.
    pub property_id: String,
    /// English or language-fallback label, or the property ID when unresolved.
    pub property_label: String,
    /// Main statement values in source order.
    pub values: Vec<WikidataStatementValue>,
}

/// One rendered Wikidata statement value.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WikidataStatementValue {
    /// Compact human-facing text with internal Wikibase metadata removed.
    pub display: String,
    /// Referenced item ID when this is a direct item-valued claim.
    ///
    /// The ID remains present when its human-facing label could not be loaded,
    /// so callers can still offer a stable link to the Wikidata item.
    pub item_id: Option<String>,
    /// Provider page for an external identifier or Wikimedia Commons file.
    ///
    /// The field defaults to absent when older serialized statement data is
    /// loaded, preserving compatibility with caches written before links were
    /// exposed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_url: Option<Url>,
    /// Bounded-size raster preview distinct from the human-facing target.
    ///
    /// This is currently populated for Wikimedia Commons media. Older cached
    /// values deserialize with no preview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_url: Option<Url>,
}

impl WikidataStatementValue {
    /// Derives a stable, credential-free Commons playback target when this is
    /// a supported P51 audio or P10 video value.
    ///
    /// The URL is derived on demand instead of serialized, so caches never
    /// retain a resolved CDN location. The human-facing Commons file page in
    /// [`Self::external_url`] remains the navigation target.
    #[must_use]
    pub fn commons_playback(&self, property_id: &str) -> Option<WikidataPlayableMedia> {
        let expected_page = commons_file_page_url(&self.display)?;
        if self.external_url.as_ref() != Some(&expected_page) {
            return None;
        }
        let kind = commons_playable_media_kind(property_id, &self.display)?;
        Some(WikidataPlayableMedia {
            kind,
            playback_url: commons_file_redirect_url(&self.display)?,
        })
    }
}

/// Playable Commons media class derived from a Wikidata property.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WikidataPlayableMediaKind {
    /// Audio represented by Wikidata property P51.
    Audio,
    /// Video represented by Wikidata property P10; Youta still plays audio only.
    Video,
}

/// Stable direct Commons input derived for one supported statement value.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WikidataPlayableMedia {
    /// Whether the Wikidata property represents audio or video.
    pub kind: WikidataPlayableMediaKind,
    /// Canonical Commons redirect without credentials or transient query data.
    pub playback_url: Url,
}

/// Bounded client for the public Wikidata Query Service.
#[derive(Clone)]
pub struct WikidataProvider {
    agent: ureq::Agent,
    max_response_bytes: usize,
    entity_api_endpoint: Url,
    formatter_query_endpoint: Url,
    max_entity_response_bytes: usize,
    max_label_response_bytes: usize,
    max_formatter_response_bytes: usize,
}

impl WikidataProvider {
    /// Creates a client with the common provider timeout and a 512 KiB result
    /// limit.
    ///
    /// # Panics
    ///
    /// Panics only if a compile-time Wikidata HTTPS endpoint is not a valid
    /// URL.
    #[must_use]
    pub fn new() -> Self {
        Self {
            agent: provider_agent(DEFAULT_REQUEST_TIMEOUT),
            max_response_bytes: MAX_RESPONSE_BYTES,
            entity_api_endpoint: Url::parse(ENTITY_API_ENDPOINT)
                .expect("the compile-time Wikidata entity API URL is valid"),
            formatter_query_endpoint: Url::parse(ENDPOINT)
                .expect("the compile-time Wikidata query endpoint is valid"),
            max_entity_response_bytes: MAX_ENTITY_RESPONSE_BYTES,
            max_label_response_bytes: MAX_LABEL_RESPONSE_BYTES,
            max_formatter_response_bytes: MAX_FORMATTER_RESPONSE_BYTES,
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

    /// Lazily loads bounded, human-facing statements for one Wikidata item.
    ///
    /// The first bounded `wbgetentities` request retrieves claims plus
    /// canonical Wikipedia sitelink URLs. Additional bounded requests resolve
    /// labels in anonymous-API-sized batches and retrieve P1630 formatter URLs
    /// for external-ID properties. Direct item-valued claims are requested
    /// before supporting entities and property labels. Unresolved IDs remain
    /// visible rather than causing data loss.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid Q-ID, failed or oversized response,
    /// mismatched entity data, or malformed claims/labels.
    pub fn load_entity_statements(
        &self,
        item_id: &str,
    ) -> Result<WikidataEntityStatements, ProviderError> {
        validate_item_id(item_id)?;
        let entity_url = build_entity_api_url(
            &self.entity_api_endpoint,
            &[item_id.to_owned()],
            "claims|sitelinks/urls",
        )?;
        let response: EntityStatementsResponse =
            get_bounded_json(&self.agent, &entity_url, self.max_entity_response_bytes)?;
        let mut pending = normalize_entity_claims(item_id, response)?;

        let (label_ids, labels_truncated) = collect_label_ids(&pending);
        pending.omissions.hard_bounds_reached |= labels_truncated;
        let mut labels = BTreeMap::new();
        for label_batch in label_ids.chunks(MAX_LABEL_IDS) {
            let label_url = build_label_url(&self.entity_api_endpoint, label_batch)?;
            let response: EntityLabelResponse =
                get_bounded_json(&self.agent, &label_url, self.max_label_response_bytes)?;
            let (batch_labels, batch_omissions) = normalize_labels(label_batch, response)?;
            pending.omissions.merge(batch_omissions);
            labels.extend(batch_labels);
        }

        let formatter_property_ids = collect_formatter_property_ids(&pending);
        let mut formatter_urls = BTreeMap::new();
        for formatter_batch in formatter_property_ids.chunks(MAX_LABEL_IDS) {
            let formatter_url =
                build_formatter_url(&self.formatter_query_endpoint, formatter_batch)?;
            let response: FormatterSparqlResponse = get_bounded_json(
                &self.agent,
                &formatter_url,
                self.max_formatter_response_bytes,
            )?;
            let (batch_formatters, formatter_omissions) =
                normalize_formatter_urls(formatter_batch, response)?;
            pending.omissions.merge(formatter_omissions);
            formatter_urls.extend(batch_formatters);
        }

        Ok(render_entity_statements(pending, &labels, &formatter_urls))
    }

    #[cfg(test)]
    fn with_statement_endpoints(
        entity_api_endpoint: Url,
        max_entity_response_bytes: usize,
        max_label_response_bytes: usize,
    ) -> Self {
        let formatter_query_endpoint = entity_api_endpoint
            .join("/sparql")
            .expect("a test entity API URL can form a sibling SPARQL endpoint");
        Self {
            agent: provider_agent(DEFAULT_REQUEST_TIMEOUT),
            max_response_bytes: MAX_RESPONSE_BYTES,
            entity_api_endpoint,
            formatter_query_endpoint,
            max_entity_response_bytes,
            max_label_response_bytes,
            max_formatter_response_bytes: max_label_response_bytes
                .min(MAX_FORMATTER_RESPONSE_BYTES),
        }
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
        WikidataExternalKind::MusicBrainzRecording => {
            if is_canonical_lowercase_uuid(external_id) {
                Ok(())
            } else {
                Err(ProviderError::InvalidRequest(
                    "MusicBrainz recording ID must be a lowercase canonical UUID".to_owned(),
                ))
            }
        }
        WikidataExternalKind::BilibiliVideo => {
            let valid_numeric_id = external_id
                .strip_prefix("av")
                .is_some_and(is_positive_decimal);
            let valid_prefixed_id = external_id.len() == 12
                && external_id.starts_with("BV")
                && external_id[2..]
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric());
            if valid_numeric_id || valid_prefixed_id {
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

/// Returns whether a value is a canonical lowercase UUID string.
///
/// MusicBrainz recording identifiers use the fixed `8-4-4-4-12` UUID layout.
/// Accepting only lowercase hexadecimal digits also keeps the value identical
/// to Wikidata's P4404 canonical representation.
fn is_canonical_lowercase_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
}

fn is_positive_decimal(value: &str) -> bool {
    !value.is_empty() && !value.starts_with('0') && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// Extracts a Wikidata-compatible `SoundCloud` account or track path.
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

fn build_label_url(endpoint: &Url, entity_ids: &[String]) -> Result<Url, ProviderError> {
    build_entity_api_url(endpoint, entity_ids, "labels")
}

/// Builds one bounded SPARQL request for P1630 formatter URLs.
fn build_formatter_url(endpoint: &Url, property_ids: &[String]) -> Result<Url, ProviderError> {
    if property_ids.is_empty() || property_ids.len() > MAX_LABEL_IDS {
        return Err(ProviderError::InvalidRequest(format!(
            "Wikidata formatter request must contain 1 to {MAX_LABEL_IDS} property IDs"
        )));
    }
    for property_id in property_ids {
        validate_property_id(property_id)?;
    }
    let properties = property_ids
        .iter()
        .map(|property_id| format!("wd:{property_id}"))
        .collect::<Vec<_>>()
        .join(" ");
    let query = format!(
        r"SELECT ?property (SAMPLE(?formatterUrl) AS ?formatter) WHERE {{
  VALUES ?property {{ {properties} }}
  ?property wdt:P1630 ?formatterUrl .
}}
GROUP BY ?property
LIMIT {MAX_LABEL_IDS}"
    );
    let mut url = endpoint.clone();
    url.query_pairs_mut()
        .append_pair("query", &query)
        .append_pair("format", "json")
        .append_pair("maxlag", "5");
    Ok(url)
}

/// Builds a `wbgetentities` request within the anonymous 50-ID limit.
fn build_entity_api_url(
    endpoint: &Url,
    entity_ids: &[String],
    props: &str,
) -> Result<Url, ProviderError> {
    if entity_ids.is_empty() || entity_ids.len() > MAX_LABEL_IDS {
        return Err(ProviderError::InvalidRequest(format!(
            "Wikidata entity request must contain 1 to {MAX_LABEL_IDS} entity IDs"
        )));
    }
    for entity_id in entity_ids {
        validate_label_entity_id(entity_id)?;
    }
    let mut url = endpoint.clone();
    url.query_pairs_mut()
        .append_pair("action", "wbgetentities")
        .append_pair("format", "json")
        .append_pair("formatversion", "2")
        .append_pair("props", props)
        .append_pair("ids", &entity_ids.join("|"));
    if props == "labels" {
        url.query_pairs_mut()
            .append_pair("languages", "en")
            .append_pair("languagefallback", "1");
    }
    Ok(url)
}

fn validate_item_id(item_id: &str) -> Result<(), ProviderError> {
    if valid_prefixed_decimal_id(item_id, 'Q') {
        Ok(())
    } else {
        Err(ProviderError::InvalidRequest(
            "Wikidata item ID must be a positive Q identifier".to_owned(),
        ))
    }
}

fn validate_property_id(property_id: &str) -> Result<(), ProviderError> {
    if valid_prefixed_decimal_id(property_id, 'P') {
        Ok(())
    } else {
        Err(ProviderError::InvalidResponse(
            "Wikidata claim contains an invalid property identifier".to_owned(),
        ))
    }
}

fn validate_label_entity_id(entity_id: &str) -> Result<(), ProviderError> {
    if is_label_entity_id(entity_id) {
        Ok(())
    } else {
        Err(ProviderError::InvalidRequest(
            "Wikidata label ID must be a positive P or Q identifier".to_owned(),
        ))
    }
}

fn is_label_entity_id(entity_id: &str) -> bool {
    valid_prefixed_decimal_id(entity_id, 'P') || valid_prefixed_decimal_id(entity_id, 'Q')
}

fn valid_prefixed_decimal_id(value: &str, prefix: char) -> bool {
    let Some(digits) = value.strip_prefix(prefix) else {
        return false;
    };
    value.len() <= 32
        && !digits.is_empty()
        && !digits.starts_with('0')
        && digits.bytes().all(|byte| byte.is_ascii_digit())
}

#[derive(Debug)]
struct PendingEntityStatements {
    item_id: String,
    statements: Vec<PendingStatement>,
    wikipedia_sitelinks: Vec<WikidataWikipediaSitelink>,
    wikipedia_sitelinks_omitted: bool,
    omissions: OmissionState,
}

#[derive(Clone, Copy, Debug, Default)]
struct OmissionState {
    hard_bounds_reached: bool,
    unsupported_values_omitted: bool,
}

impl OmissionState {
    fn merge(&mut self, other: Self) {
        self.hard_bounds_reached |= other.hard_bounds_reached;
        self.unsupported_values_omitted |= other.unsupported_values_omitted;
    }

    fn is_truncated(self) -> bool {
        self.hard_bounds_reached || self.unsupported_values_omitted
    }
}

#[derive(Debug)]
struct PendingStatement {
    property_id: String,
    values: Vec<PendingStatementValue>,
}

#[derive(Debug)]
enum PendingStatementValue {
    Plain(String),
    ExternalId(String),
    CommonsCategory(String),
    CommonsMedia(String),
    Entity(String),
    Quantity {
        amount: String,
        lower_bound: Option<String>,
        upper_bound: Option<String>,
        unit_id: Option<String>,
    },
    Time {
        time: String,
        calendar_id: Option<String>,
    },
    Coordinate {
        latitude: String,
        longitude: String,
        altitude: Option<String>,
        globe_id: Option<String>,
    },
    /// One P8687 observation stripped of account identifiers and raw metadata.
    SocialFollowers {
        amount: Option<String>,
        service_property_ids: Vec<String>,
        dates: Vec<PendingFollowerDate>,
        rank: RawStatementRank,
    },
}

/// One bounded P585 qualifier used to explain a follower observation.
#[derive(Debug)]
enum PendingFollowerDate {
    /// A precise enough date with its calendar retained for label resolution.
    Known {
        display: String,
        sort_key: String,
        calendar_id: Option<String>,
    },
    /// A `somevalue` or `novalue` P585 qualifier.
    Unknown,
}

impl PendingStatementValue {
    /// Returns the directly linked entity ID, which is the most useful label
    /// for callers that expose item-valued claims as navigable links.
    fn direct_entity_id(&self) -> Option<&str> {
        match self {
            Self::Entity(entity_id) => Some(entity_id),
            Self::Plain(_)
            | Self::ExternalId(_)
            | Self::CommonsCategory(_)
            | Self::CommonsMedia(_)
            | Self::Quantity { .. }
            | Self::Time { .. }
            | Self::Coordinate { .. }
            | Self::SocialFollowers { .. } => None,
        }
    }

    /// Returns an entity ID used to explain a composite scalar value.
    fn supporting_entity_id(&self) -> Option<&str> {
        match self {
            Self::Quantity { unit_id, .. } => unit_id.as_deref(),
            Self::Time { calendar_id, .. } => calendar_id.as_deref(),
            Self::Coordinate { globe_id, .. } => globe_id.as_deref(),
            Self::Plain(_)
            | Self::ExternalId(_)
            | Self::CommonsCategory(_)
            | Self::CommonsMedia(_)
            | Self::Entity(_)
            | Self::SocialFollowers { .. } => None,
        }
    }

    fn render(
        &self,
        property_id: &str,
        labels: &BTreeMap<String, String>,
        formatter_urls: &BTreeMap<String, String>,
    ) -> WikidataStatementValue {
        match self {
            Self::Plain(display) => WikidataStatementValue {
                display: display.clone(),
                item_id: None,
                external_url: None,
                preview_url: None,
            },
            Self::ExternalId(display) => WikidataStatementValue {
                display: display.clone(),
                item_id: None,
                external_url: formatter_urls
                    .get(property_id)
                    .and_then(|formatter| formatted_external_url(formatter, display)),
                preview_url: None,
            },
            Self::CommonsCategory(display) => WikidataStatementValue {
                display: display.clone(),
                item_id: None,
                external_url: commons_category_page_url(display),
                preview_url: None,
            },
            Self::CommonsMedia(display) => WikidataStatementValue {
                display: display.clone(),
                item_id: None,
                external_url: commons_file_page_url(display),
                preview_url: commons_file_preview_url(display),
            },
            Self::Entity(entity_id) => WikidataStatementValue {
                display: labels
                    .get(entity_id)
                    .cloned()
                    .unwrap_or_else(|| entity_id.clone()),
                item_id: entity_id.starts_with('Q').then(|| entity_id.clone()),
                external_url: None,
                preview_url: None,
            },
            Self::Quantity {
                amount,
                lower_bound,
                upper_bound,
                unit_id,
            } => {
                let bounds = lower_bound
                    .as_deref()
                    .zip(upper_bound.as_deref())
                    .filter(|(lower, upper)| *lower != amount || *upper != amount)
                    .map_or_else(String::new, |(lower, upper)| format!(" ({lower}–{upper})"));
                let unit = unit_id.as_ref().map_or_else(String::new, |unit_id| {
                    format!(
                        " {}",
                        labels.get(unit_id).map_or(unit_id.as_str(), String::as_str)
                    )
                });
                WikidataStatementValue {
                    display: format!("{amount}{bounds}{unit}"),
                    item_id: None,
                    external_url: None,
                    preview_url: None,
                }
            }
            Self::Time { time, calendar_id } => {
                let calendar = calendar_id
                    .as_ref()
                    .map_or_else(String::new, |calendar_id| {
                        format!(
                            " ({})",
                            labels
                                .get(calendar_id)
                                .map_or(calendar_id.as_str(), String::as_str)
                        )
                    });
                WikidataStatementValue {
                    display: format!("{time}{calendar}"),
                    item_id: None,
                    external_url: None,
                    preview_url: None,
                }
            }
            Self::Coordinate {
                latitude,
                longitude,
                altitude,
                globe_id,
            } => {
                let altitude = altitude
                    .as_ref()
                    .map_or_else(String::new, |altitude| format!(", altitude {altitude}"));
                let globe = globe_id.as_ref().map_or_else(String::new, |globe_id| {
                    format!(
                        " ({})",
                        labels
                            .get(globe_id)
                            .map_or(globe_id.as_str(), String::as_str)
                    )
                });
                WikidataStatementValue {
                    display: format!("{latitude}, {longitude}{altitude}{globe}"),
                    item_id: None,
                    external_url: None,
                    preview_url: None,
                }
            }
            Self::SocialFollowers {
                amount,
                service_property_ids,
                dates,
                rank,
            } => {
                let service = follower_service_display(service_property_ids, labels);
                let date = follower_date_display(dates, labels);
                let count = amount.as_deref().map_or_else(
                    || "follower count unknown".to_owned(),
                    |amount| format!("{} followers", grouped_quantity(amount)),
                );
                let rank = if *rank == RawStatementRank::Deprecated {
                    " · deprecated"
                } else {
                    ""
                };
                WikidataStatementValue {
                    display: format!("{service} · {date} · {count}{rank}"),
                    item_id: None,
                    external_url: None,
                    preview_url: None,
                }
            }
        }
    }
}

fn normalize_entity_claims(
    item_id: &str,
    mut response: EntityStatementsResponse,
) -> Result<PendingEntityStatements, ProviderError> {
    if response.entities.len() != 1 {
        return Err(ProviderError::InvalidResponse(
            "Wikidata entity response must contain exactly one entity".to_owned(),
        ));
    }
    let entity = response.entities.remove(item_id).ok_or_else(|| {
        ProviderError::InvalidResponse(
            "Wikidata entity response does not match the requested item".to_owned(),
        )
    })?;
    let (wikipedia_sitelinks, wikipedia_sitelinks_omitted) =
        normalize_wikipedia_sitelinks(entity.sitelinks);
    let property_count = entity.claims.len();
    let mut statements = Vec::with_capacity(property_count.min(MAX_STATEMENT_PROPERTIES));
    let mut total_values = 0usize;
    let mut omissions = OmissionState {
        hard_bounds_reached: property_count > MAX_STATEMENT_PROPERTIES,
        unsupported_values_omitted: false,
    };

    for (property_id, claims) in entity.claims.into_iter().take(MAX_STATEMENT_PROPERTIES) {
        validate_property_id(&property_id)?;
        let claim_count = claims.len();
        if claim_count > MAX_VALUES_PER_PROPERTY {
            omissions.hard_bounds_reached = true;
        }
        let mut values = Vec::with_capacity(claim_count.min(MAX_VALUES_PER_PROPERTY));
        for claim in claims.into_iter().take(MAX_VALUES_PER_PROPERTY) {
            if total_values >= MAX_STATEMENT_VALUES {
                omissions.hard_bounds_reached = true;
                break;
            }
            if claim.mainsnak.property != property_id {
                return Err(ProviderError::InvalidResponse(
                    "Wikidata claim property does not match its containing group".to_owned(),
                ));
            }
            let (value, value_omissions) = if property_id == "P8687" {
                normalize_social_followers(claim)?
            } else {
                normalize_snak(claim.mainsnak)?
            };
            omissions.merge(value_omissions);
            if let Some(value) = value {
                values.push(value);
                total_values = total_values.saturating_add(1);
            }
        }
        if !values.is_empty() {
            statements.push(PendingStatement {
                property_id,
                values,
            });
        }
        if total_values >= MAX_STATEMENT_VALUES {
            omissions.hard_bounds_reached |= statements.len() < property_count;
            break;
        }
    }

    Ok(PendingEntityStatements {
        item_id: item_id.to_owned(),
        statements,
        wikipedia_sitelinks,
        wikipedia_sitelinks_omitted,
        omissions,
    })
}

/// Retains canonical Wikipedia sitelinks without consuming statement limits.
fn normalize_wikipedia_sitelinks(
    sitelinks: BTreeMap<String, RawSitelink>,
) -> (Vec<WikidataWikipediaSitelink>, bool) {
    let mut normalized = Vec::new();
    let mut omitted = false;

    for (map_site_id, sitelink) in sitelinks {
        let candidate_is_wikipedia = sitelink
            .url
            .as_deref()
            .and_then(|raw| Url::parse(raw).ok())
            .and_then(|url| url.host_str().map(str::to_owned))
            .is_some_and(|host| wikipedia_project_host(&host));
        let Some(value) = normalize_wikipedia_sitelink(&map_site_id, sitelink) else {
            omitted |= candidate_is_wikipedia;
            continue;
        };
        normalized.push(value);
    }

    normalized.sort_by(|left, right| {
        (left.site_id != "enwiki")
            .cmp(&(right.site_id != "enwiki"))
            .then_with(|| left.project_label.cmp(&right.project_label))
            .then_with(|| left.title.cmp(&right.title))
            .then_with(|| left.url.as_str().cmp(right.url.as_str()))
    });
    let mut seen_urls = BTreeSet::new();
    normalized.retain(|value| seen_urls.insert(value.url.as_str().to_owned()));
    if normalized.len() > MAX_WIKIPEDIA_SITELINKS {
        normalized.truncate(MAX_WIKIPEDIA_SITELINKS);
        omitted = true;
    }
    (normalized, omitted)
}

/// Validates one API-supplied Wikipedia sitelink as a display and click target.
fn normalize_wikipedia_sitelink(
    map_site_id: &str,
    sitelink: RawSitelink,
) -> Option<WikidataWikipediaSitelink> {
    if map_site_id != sitelink.site
        || map_site_id.is_empty()
        || map_site_id.len() > 128
        || !map_site_id.ends_with("wiki")
        || !map_site_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return None;
    }
    let raw_url = sitelink.url?;
    if raw_url.len() > MAX_VALUE_BYTES {
        return None;
    }
    let url = Url::parse(&raw_url).ok()?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let host = url.host_str()?;
    if !wikipedia_project_host(host) {
        return None;
    }
    let raw_authority = raw_url
        .strip_prefix("https://")?
        .split_once('/')
        .map(|(authority, _)| authority)?;
    if raw_authority != host {
        return None;
    }
    let article_path = url.path().strip_prefix("/wiki/")?;
    if article_path.is_empty() {
        return None;
    }
    let (title, title_omissions) = bounded_display(&sitelink.title).ok()?;
    if title_omissions.hard_bounds_reached || title_omissions.unsupported_values_omitted {
        return None;
    }
    Some(WikidataWikipediaSitelink {
        site_id: map_site_id.to_owned(),
        project_label: host.to_owned(),
        title: title?,
        url,
    })
}

/// Accepts exactly one canonical Wikipedia project subdomain.
fn wikipedia_project_host(host: &str) -> bool {
    let Some(project) = host.strip_suffix(".wikipedia.org") else {
        return false;
    };
    !project.is_empty()
        && !project.contains('.')
        && project
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Normalizes one P8687 observation without retaining account identifiers.
fn normalize_social_followers(
    claim: RawClaim,
) -> Result<(Option<PendingStatementValue>, OmissionState), ProviderError> {
    let amount = match claim.mainsnak.snak_type.as_str() {
        "novalue" | "somevalue" => None,
        "value" => {
            let data_value = claim.mainsnak.data_value.ok_or_else(|| {
                ProviderError::InvalidResponse(
                    "Wikidata follower claim is missing its data value".to_owned(),
                )
            })?;
            if data_value.value_type != "quantity" {
                return Err(ProviderError::InvalidResponse(
                    "Wikidata follower claim must contain a quantity".to_owned(),
                ));
            }
            let raw: RawQuantity = serde_json::from_value(data_value.value)
                .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
            if raw.unit != "1" {
                return Err(ProviderError::InvalidResponse(
                    "Wikidata follower count must be dimensionless".to_owned(),
                ));
            }
            Some(required_bounded_text(&raw.amount, "follower count")?)
        }
        _ => {
            return Err(ProviderError::InvalidResponse(
                "Wikidata follower claim contains an unknown snak type".to_owned(),
            ));
        }
    };
    let (amount, mut omissions) = amount.map_or((None, OmissionState::default()), |value| {
        (Some(value.0), value.1)
    });
    let mut service_property_ids = Vec::new();
    let mut dates = Vec::new();
    let mut inspected = 0usize;

    for (property_id, qualifier_snaks) in claim.qualifiers {
        validate_property_id(&property_id)?;
        for qualifier_value in qualifier_snaks {
            if inspected >= MAX_FOLLOWER_QUALIFIER_SNAKS {
                omissions.hard_bounds_reached = true;
                continue;
            }
            inspected = inspected.saturating_add(1);
            let qualifier: RawMainSnak = serde_json::from_value(qualifier_value)
                .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
            if qualifier.property != property_id {
                return Err(ProviderError::InvalidResponse(
                    "Wikidata qualifier property does not match its containing group".to_owned(),
                ));
            }
            if property_id == "P585" {
                let (date, date_omissions) = normalize_follower_date(qualifier)?;
                omissions.merge(date_omissions);
                dates.push(date);
            } else if qualifier.data_type.as_deref() == Some("external-id") {
                if !matches!(
                    qualifier.snak_type.as_str(),
                    "value" | "somevalue" | "novalue"
                ) {
                    return Err(ProviderError::InvalidResponse(
                        "Wikidata service qualifier contains an unknown snak type".to_owned(),
                    ));
                }
                // Retain only the property that names the service. The account
                // ID itself must never reach caches, diagnostics, or the UI.
                service_property_ids.push(property_id.clone());
            }
        }
    }
    service_property_ids.sort();
    dates.sort_by(|left, right| follower_date_sort_key(right).cmp(follower_date_sort_key(left)));

    Ok((
        Some(PendingStatementValue::SocialFollowers {
            amount,
            service_property_ids,
            dates,
            rank: claim.rank,
        }),
        omissions,
    ))
}

/// Normalizes one P585 qualifier while preserving precision and calendar.
fn normalize_follower_date(
    snak: RawMainSnak,
) -> Result<(PendingFollowerDate, OmissionState), ProviderError> {
    match snak.snak_type.as_str() {
        "novalue" | "somevalue" => Ok((PendingFollowerDate::Unknown, OmissionState::default())),
        "value" => {
            let data_value = snak.data_value.ok_or_else(|| {
                ProviderError::InvalidResponse(
                    "Wikidata point-in-time qualifier is missing its data value".to_owned(),
                )
            })?;
            if data_value.value_type != "time" {
                return Err(ProviderError::InvalidResponse(
                    "Wikidata point-in-time qualifier must contain a time value".to_owned(),
                ));
            }
            let raw: RawTime = serde_json::from_value(data_value.value)
                .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
            let calendar_id = unit_entity_id(&raw.calendar_model)?;
            let display = follower_human_time(&raw.time, raw.precision)?;
            let (display, omissions) =
                required_bounded_text(&display, "follower observation date")?;
            Ok((
                PendingFollowerDate::Known {
                    display,
                    sort_key: raw.time,
                    calendar_id,
                },
                omissions,
            ))
        }
        _ => Err(ProviderError::InvalidResponse(
            "Wikidata point-in-time qualifier contains an unknown snak type".to_owned(),
        )),
    }
}

/// Produces a year-first, precision-aware date for a P585 qualifier.
fn follower_human_time(time: &str, precision: u8) -> Result<String, ProviderError> {
    let normalized = human_time(time, precision)?;
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
    if precision >= 11 {
        let (year_month, day) = normalized.rsplit_once('-').ok_or_else(|| {
            ProviderError::InvalidResponse(
                "Wikidata follower date is missing a day component".to_owned(),
            )
        })?;
        let (year, month) = year_month.rsplit_once('-').ok_or_else(|| {
            ProviderError::InvalidResponse(
                "Wikidata follower date is missing a month component".to_owned(),
            )
        })?;
        let month_index = month
            .parse::<usize>()
            .ok()
            .and_then(|month| month.checked_sub(1).filter(|index| *index < MONTHS.len()));
        let month = month_index.map(|index| MONTHS[index]).ok_or_else(|| {
            ProviderError::InvalidResponse(
                "Wikidata follower date contains an invalid month".to_owned(),
            )
        })?;
        return Ok(format!("{year} {month} {}", day.trim_start_matches('0')));
    }
    if precision == 10 {
        let (year, month) = normalized.rsplit_once('-').ok_or_else(|| {
            ProviderError::InvalidResponse(
                "Wikidata follower date is missing a month component".to_owned(),
            )
        })?;
        let month_index = month
            .parse::<usize>()
            .ok()
            .and_then(|month| month.checked_sub(1).filter(|index| *index < MONTHS.len()));
        let month = month_index.map(|index| MONTHS[index]).ok_or_else(|| {
            ProviderError::InvalidResponse(
                "Wikidata follower date contains an invalid month".to_owned(),
            )
        })?;
        return Ok(format!("{year} {month}"));
    }
    if precision == 9 {
        Ok(normalized)
    } else {
        Ok(format!("{normalized} (precision {precision})"))
    }
}

fn follower_date_sort_key(date: &PendingFollowerDate) -> &str {
    match date {
        PendingFollowerDate::Known { sort_key, .. } => sort_key,
        PendingFollowerDate::Unknown => "",
    }
}

/// Resolves one or more service-identifier qualifier properties.
fn follower_service_display(
    service_property_ids: &[String],
    labels: &BTreeMap<String, String>,
) -> String {
    if service_property_ids.is_empty() {
        return "Unknown service".to_owned();
    }
    let unique_ids = service_property_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let services = unique_ids
        .iter()
        .map(|property_id| follower_service_label(property_id, labels))
        .collect::<Vec<_>>();
    if services.len() > 1 {
        format!("Multiple services: {}", services.join(", "))
    } else if service_property_ids.len() > 1 {
        format!("Multiple service accounts: {}", services[0])
    } else {
        services[0].clone()
    }
}

/// Converts an identifier-property label into a concise service name.
fn follower_service_label(property_id: &str, labels: &BTreeMap<String, String>) -> String {
    match property_id {
        "P2397" => return "YouTube".to_owned(),
        "P4033" => return "Mastodon".to_owned(),
        "P6552" => return "X (Twitter)".to_owned(),
        _ => {}
    }
    let label = labels
        .get(property_id)
        .map(String::as_str)
        .unwrap_or(property_id);
    const SUFFIXES: [&str; 8] = [
        " numeric user ID",
        " channel ID",
        " username",
        " address",
        " artist ID",
        " user ID",
        " profile ID",
        " ID",
    ];
    SUFFIXES
        .iter()
        .find_map(|suffix| label.strip_suffix(suffix))
        .filter(|service| !service.is_empty())
        .unwrap_or(label)
        .to_owned()
}

/// Renders one or more P585 qualifier dates without hiding ambiguity.
fn follower_date_display(
    dates: &[PendingFollowerDate],
    labels: &BTreeMap<String, String>,
) -> String {
    if dates.is_empty() {
        return "date unknown".to_owned();
    }
    let rendered = dates
        .iter()
        .map(|date| match date {
            PendingFollowerDate::Unknown => "date unknown".to_owned(),
            PendingFollowerDate::Known {
                display,
                calendar_id,
                ..
            } => {
                let calendar = calendar_id
                    .as_ref()
                    .map_or_else(String::new, |calendar_id| {
                        if calendar_id == "Q1985727" {
                            String::new()
                        } else {
                            format!(
                                " ({})",
                                labels
                                    .get(calendar_id)
                                    .map_or(calendar_id.as_str(), String::as_str)
                            )
                        }
                    });
                format!("{display}{calendar}")
            }
        })
        .collect::<Vec<_>>();
    if rendered.len() == 1 {
        rendered[0].clone()
    } else {
        format!("multiple dates: {}", rendered.join(", "))
    }
}

/// Groups integral follower counts without carrying Wikibase uncertainty bounds.
fn grouped_quantity(amount: &str) -> String {
    let (sign, digits) = if let Some(digits) = amount.strip_prefix('+') {
        ("", digits)
    } else if let Some(digits) = amount.strip_prefix('-') {
        ("-", digits)
    } else {
        ("", amount)
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return amount.strip_prefix('+').unwrap_or(amount).to_owned();
    }
    let mut grouped = String::with_capacity(amount.len().saturating_add(amount.len() / 3));
    grouped.push_str(sign);
    let first_group = digits.len() % 3;
    if first_group > 0 {
        grouped.push_str(&digits[..first_group]);
    }
    for chunk in digits.as_bytes()[first_group..].chunks(3) {
        if grouped.len() > sign.len() {
            grouped.push(',');
        }
        grouped.push_str(std::str::from_utf8(chunk).expect("ASCII digits are valid UTF-8"));
    }
    grouped
}

fn normalize_snak(
    snak: RawMainSnak,
) -> Result<(Option<PendingStatementValue>, OmissionState), ProviderError> {
    match snak.snak_type.as_str() {
        "novalue" => Ok((
            Some(PendingStatementValue::Plain("no value".to_owned())),
            OmissionState::default(),
        )),
        "somevalue" => Ok((
            Some(PendingStatementValue::Plain("unknown value".to_owned())),
            OmissionState::default(),
        )),
        "value" => {
            let data_value = snak.data_value.ok_or_else(|| {
                ProviderError::InvalidResponse(
                    "Wikidata value claim is missing its data value".to_owned(),
                )
            })?;
            let (value, omissions) = normalize_data_value(data_value)?;
            let value = match (snak.property.as_str(), snak.data_type.as_deref(), value) {
                ("P373", _, Some(PendingStatementValue::Plain(display))) => {
                    Some(PendingStatementValue::CommonsCategory(display))
                }
                (_, Some("external-id"), Some(PendingStatementValue::Plain(display))) => {
                    Some(PendingStatementValue::ExternalId(display))
                }
                (_, Some("commonsMedia"), Some(PendingStatementValue::Plain(display))) => {
                    Some(PendingStatementValue::CommonsMedia(display))
                }
                (_, _, value) => value,
            };
            Ok((value, omissions))
        }
        _ => Err(ProviderError::InvalidResponse(
            "Wikidata claim contains an unknown snak type".to_owned(),
        )),
    }
}

/// Applies a Wikidata P1630 formatter URL to one percent-encoded identifier.
fn formatted_external_url(formatter: &str, external_id: &str) -> Option<Url> {
    if !formatter.contains("$1") {
        return None;
    }
    let encoded_id = percent_encode_component(external_id);
    let rendered = formatter.replace("$1", &encoded_id);
    safe_external_url(&rendered)
}

/// Builds Wikimedia Commons' human-facing category page for a P373 value.
fn commons_category_page_url(category: &str) -> Option<Url> {
    Url::parse(&format!(
        "{COMMONS_CATEGORY_PAGE_BASE}{}",
        percent_encode_component(category)
    ))
    .ok()
}

/// Builds Wikimedia Commons' human-facing file page for a P18 value.
fn commons_file_page_url(filename: &str) -> Option<Url> {
    Url::parse(&format!(
        "{COMMONS_FILE_PAGE_BASE}{}",
        percent_encode_component(filename)
    ))
    .ok()
}

/// Builds Commons' stable file redirect without resolving a CDN location.
fn commons_file_redirect_url(filename: &str) -> Option<Url> {
    Url::parse(&format!(
        "{COMMONS_FILE_PREVIEW_BASE}{}",
        percent_encode_component(filename)
    ))
    .ok()
}

/// Builds a bounded-width Commons raster redirect suitable for TUI previews.
fn commons_file_preview_url(filename: &str) -> Option<Url> {
    let mut url = commons_file_redirect_url(filename)?;
    url.query_pairs_mut()
        .append_pair("width", COMMONS_PREVIEW_WIDTH);
    Some(url)
}

/// Classifies only open-format P51/P10 Commons media extensions.
fn commons_playable_media_kind(
    property_id: &str,
    filename: &str,
) -> Option<WikidataPlayableMediaKind> {
    let extension = filename.rsplit_once('.')?.1.to_ascii_lowercase();
    match property_id {
        "P51"
            if matches!(
                extension.as_str(),
                "flac" | "oga" | "ogg" | "opus" | "wav" | "webm"
            ) =>
        {
            Some(WikidataPlayableMediaKind::Audio)
        }
        "P10" if matches!(extension.as_str(), "ogg" | "ogv" | "webm") => {
            Some(WikidataPlayableMediaKind::Video)
        }
        _ => None,
    }
}

/// Accepts only credential-free HTTP(S) links with a network host.
fn safe_external_url(raw_url: &str) -> Option<Url> {
    let url = Url::parse(raw_url).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

/// Percent-encodes one external identifier as a URL component.
fn percent_encode_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(encoded, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    encoded
}

fn normalize_data_value(
    data_value: RawDataValue,
) -> Result<(Option<PendingStatementValue>, OmissionState), ProviderError> {
    match data_value.value_type.as_str() {
        "wikibase-entityid" => {
            let raw: RawEntityId = serde_json::from_value(data_value.value)
                .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
            if !valid_entity_reference_id(&raw.id) {
                return Err(ProviderError::InvalidResponse(
                    "Wikidata claim contains an invalid entity identifier".to_owned(),
                ));
            }
            Ok((
                Some(PendingStatementValue::Entity(raw.id)),
                OmissionState::default(),
            ))
        }
        "monolingualtext" => {
            let raw: RawMonolingualText = serde_json::from_value(data_value.value)
                .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
            let language = normalize_language_code(&raw.language)?;
            let combined = format!("{} [{language}]", raw.text);
            bounded_display(&combined)
                .map(|(display, omissions)| (display.map(PendingStatementValue::Plain), omissions))
        }
        "quantity" => {
            let raw: RawQuantity = serde_json::from_value(data_value.value)
                .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
            let unit_id = unit_entity_id(&raw.unit)?;
            let (amount, amount_omissions) = required_bounded_text(&raw.amount, "quantity amount")?;
            let (lower_bound, lower_omissions) = optional_bounded_text(raw.lower_bound.as_deref())?;
            let (upper_bound, upper_omissions) = optional_bounded_text(raw.upper_bound.as_deref())?;
            let mut omissions = amount_omissions;
            omissions.merge(lower_omissions);
            omissions.merge(upper_omissions);
            Ok((
                Some(PendingStatementValue::Quantity {
                    amount,
                    lower_bound,
                    upper_bound,
                    unit_id,
                }),
                omissions,
            ))
        }
        "time" => {
            let raw: RawTime = serde_json::from_value(data_value.value)
                .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
            let calendar_id = unit_entity_id(&raw.calendar_model)?;
            let time = human_time(&raw.time, raw.precision)?;
            let (time, omissions) = required_bounded_text(&time, "time value")?;
            Ok((
                Some(PendingStatementValue::Time { time, calendar_id }),
                omissions,
            ))
        }
        "globecoordinate" => {
            let raw: RawCoordinate = serde_json::from_value(data_value.value)
                .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
            if !raw.latitude.is_finite()
                || !raw.longitude.is_finite()
                || raw.altitude.is_some_and(|altitude| !altitude.is_finite())
            {
                return Err(ProviderError::InvalidResponse(
                    "Wikidata coordinate contains a non-finite number".to_owned(),
                ));
            }
            let globe_id = unit_entity_id(&raw.globe)?;
            Ok((
                Some(PendingStatementValue::Coordinate {
                    latitude: raw.latitude.to_string(),
                    longitude: raw.longitude.to_string(),
                    altitude: raw.altitude.map(|value| value.to_string()),
                    globe_id,
                }),
                OmissionState::default(),
            ))
        }
        _ => bounded_plain_value(data_value.value),
    }
}

fn bounded_plain_value(
    value: Value,
) -> Result<(Option<PendingStatementValue>, OmissionState), ProviderError> {
    let text = match value {
        Value::String(text) => text,
        Value::Number(number) => number.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Null | Value::Array(_) | Value::Object(_) => {
            return Ok((
                None,
                OmissionState {
                    hard_bounds_reached: false,
                    unsupported_values_omitted: true,
                },
            ));
        }
    };
    bounded_display(&text)
        .map(|(display, omissions)| (display.map(PendingStatementValue::Plain), omissions))
}

fn bounded_display(text: &str) -> Result<(Option<String>, OmissionState), ProviderError> {
    let normalized = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_owned();
    if normalized.is_empty() {
        return Ok((
            None,
            OmissionState {
                hard_bounds_reached: false,
                unsupported_values_omitted: true,
            },
        ));
    }
    if normalized.len() <= MAX_VALUE_BYTES {
        return Ok((Some(normalized), OmissionState::default()));
    }
    let boundary = normalized
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= MAX_VALUE_BYTES.saturating_sub('…'.len_utf8()))
        .last()
        .unwrap_or(0);
    if boundary == 0 {
        return Err(ProviderError::InvalidResponse(
            "Wikidata value cannot be safely bounded".to_owned(),
        ));
    }
    let mut bounded = normalized[..boundary].to_owned();
    bounded.push('…');
    Ok((
        Some(bounded),
        OmissionState {
            hard_bounds_reached: true,
            unsupported_values_omitted: false,
        },
    ))
}

fn required_bounded_text(
    text: &str,
    field: &str,
) -> Result<(String, OmissionState), ProviderError> {
    let (text, omissions) = bounded_display(text)?;
    text.map(|text| (text, omissions))
        .ok_or_else(|| ProviderError::InvalidResponse(format!("Wikidata {field} cannot be empty")))
}

fn optional_bounded_text(
    text: Option<&str>,
) -> Result<(Option<String>, OmissionState), ProviderError> {
    text.map_or(Ok((None, OmissionState::default())), bounded_display)
}

fn normalize_language_code(language: &str) -> Result<String, ProviderError> {
    let language = language.trim();
    if language.is_empty()
        || language.len() > 32
        || !language
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(ProviderError::InvalidResponse(
            "Wikidata monolingual text has an invalid language code".to_owned(),
        ));
    }
    Ok(language.to_owned())
}

fn valid_entity_reference_id(entity_id: &str) -> bool {
    ['Q', 'P', 'L', 'M']
        .into_iter()
        .any(|prefix| valid_prefixed_decimal_id(entity_id, prefix))
        || entity_id
            .strip_prefix('L')
            .and_then(|value| value.split_once('-'))
            .is_some_and(|(lexeme, subentity)| {
                !lexeme.is_empty()
                    && !lexeme.starts_with('0')
                    && lexeme.bytes().all(|byte| byte.is_ascii_digit())
                    && subentity
                        .strip_prefix('F')
                        .or_else(|| subentity.strip_prefix('S'))
                        .is_some_and(|digits| {
                            !digits.is_empty()
                                && !digits.starts_with('0')
                                && digits.bytes().all(|byte| byte.is_ascii_digit())
                        })
            })
}

fn unit_entity_id(unit: &str) -> Result<Option<String>, ProviderError> {
    if unit == "1" {
        return Ok(None);
    }
    let url = Url::parse(unit)
        .map_err(|error| ProviderError::InvalidResponse(format!("invalid entity URI: {error}")))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str() != Some("www.wikidata.org")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ProviderError::InvalidResponse(
            "Wikidata value contains a foreign entity URI".to_owned(),
        ));
    }
    let entity_id = url.path().strip_prefix("/entity/").ok_or_else(|| {
        ProviderError::InvalidResponse("Wikidata entity URI has an unexpected path".to_owned())
    })?;
    if !valid_prefixed_decimal_id(entity_id, 'Q') {
        return Err(ProviderError::InvalidResponse(
            "Wikidata entity URI contains an invalid Q identifier".to_owned(),
        ));
    }
    Ok(Some(entity_id.to_owned()))
}

fn human_time(time: &str, precision: u8) -> Result<String, ProviderError> {
    if precision > 14 {
        return Err(ProviderError::InvalidResponse(
            "Wikidata time contains an unsupported precision".to_owned(),
        ));
    }
    let (negative, unsigned) = if let Some(unsigned) = time.strip_prefix('+') {
        (false, unsigned)
    } else if let Some(unsigned) = time.strip_prefix('-') {
        (true, unsigned)
    } else {
        return Err(ProviderError::InvalidResponse(
            "Wikidata time must start with a sign".to_owned(),
        ));
    };
    let date = unsigned.split('T').next().unwrap_or_default();
    let mut components = date.split('-');
    let year = components.next().unwrap_or_default();
    let month = components.next().unwrap_or_default();
    let day = components.next().unwrap_or_default();
    if year.is_empty()
        || !year.bytes().all(|byte| byte.is_ascii_digit())
        || month.len() != 2
        || !month.bytes().all(|byte| byte.is_ascii_digit())
        || day.len() != 2
        || !day.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(ProviderError::InvalidResponse(
            "Wikidata time contains an invalid date".to_owned(),
        ));
    }
    let normalized_year = year.trim_start_matches('0');
    let normalized_year = if normalized_year.is_empty() {
        "0"
    } else {
        normalized_year
    };
    let sign = if negative { "-" } else { "" };
    Ok(match precision {
        11.. => format!("{sign}{normalized_year}-{month}-{day}"),
        10 => format!("{sign}{normalized_year}-{month}"),
        _ => format!("{sign}{normalized_year}"),
    })
}

/// Selects all structurally bounded label IDs in descending order of UI value.
///
/// Direct entity-valued claims are collected across every statement before
/// supporting unit/calendar/globe entities and property IDs. The caller chunks
/// the result to Wikidata's anonymous 50-ID request limit, so reaching a batch
/// boundary does not make the entity result partial.
fn collect_label_ids(pending: &PendingEntityStatements) -> (Vec<String>, bool) {
    let mut identifiers = Vec::with_capacity(MAX_LABEL_ENTITY_IDS);
    let mut seen = BTreeSet::new();
    let mut truncated = false;
    let mut push_identifier = |entity_id: &str| {
        if !is_label_entity_id(entity_id) || !seen.insert(entity_id.to_owned()) {
            return;
        }
        if identifiers.len() >= MAX_LABEL_ENTITY_IDS {
            truncated = true;
            return;
        }
        identifiers.push(entity_id.to_owned());
    };
    for entity_id in pending
        .statements
        .iter()
        .flat_map(|statement| statement.values.iter())
        .filter_map(PendingStatementValue::direct_entity_id)
    {
        push_identifier(entity_id);
    }
    for entity_id in pending
        .statements
        .iter()
        .flat_map(|statement| statement.values.iter())
        .filter_map(PendingStatementValue::supporting_entity_id)
    {
        push_identifier(entity_id);
    }
    for value in pending
        .statements
        .iter()
        .flat_map(|statement| statement.values.iter())
    {
        if let PendingStatementValue::SocialFollowers {
            service_property_ids,
            dates,
            ..
        } = value
        {
            for property_id in service_property_ids {
                push_identifier(property_id);
            }
            for date in dates {
                if let PendingFollowerDate::Known {
                    calendar_id: Some(calendar_id),
                    ..
                } = date
                {
                    push_identifier(calendar_id);
                }
            }
        }
    }
    for statement in &pending.statements {
        push_identifier(&statement.property_id);
    }
    (identifiers, truncated)
}

/// Finds properties whose external-ID values may have a P1630 formatter.
fn collect_formatter_property_ids(pending: &PendingEntityStatements) -> Vec<String> {
    pending
        .statements
        .iter()
        .filter(|statement| {
            statement
                .values
                .iter()
                .any(|value| matches!(value, PendingStatementValue::ExternalId(_)))
        })
        .map(|statement| statement.property_id.clone())
        .collect()
}

fn normalize_labels(
    requested_ids: &[String],
    response: EntityLabelResponse,
) -> Result<(BTreeMap<String, String>, OmissionState), ProviderError> {
    if response.entities.len() > requested_ids.len() {
        return Err(ProviderError::InvalidResponse(
            "Wikidata returned more label entities than requested".to_owned(),
        ));
    }
    let requested = requested_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut labels = BTreeMap::new();
    let mut omissions = OmissionState::default();
    for (entity_id, entity) in response.entities {
        if !requested.contains(entity_id.as_str()) {
            return Err(ProviderError::InvalidResponse(
                "Wikidata returned an unrequested label entity".to_owned(),
            ));
        }
        let label = entity
            .labels
            .get("en")
            .or_else(|| entity.labels.values().next())
            .map(|label| label.value.clone());
        if let Some(label) = label {
            let (label, label_omissions) = bounded_display(&label)?;
            omissions.merge(label_omissions);
            if let Some(label) = label {
                labels.insert(entity_id, label);
            }
        }
    }
    Ok((labels, omissions))
}

/// Extracts at most one safe, bounded P1630 template per requested property.
fn normalize_formatter_urls(
    requested_ids: &[String],
    response: FormatterSparqlResponse,
) -> Result<(BTreeMap<String, String>, OmissionState), ProviderError> {
    if response.results.bindings.len() > requested_ids.len() {
        return Err(ProviderError::InvalidResponse(
            "Wikidata returned more formatter rows than requested".to_owned(),
        ));
    }
    let requested = requested_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut formatters = BTreeMap::new();
    let mut omissions = OmissionState::default();
    for binding in response.results.bindings {
        let property_id = wikidata_property_id(&binding.property.value)?;
        if !requested.contains(property_id.as_str()) {
            return Err(ProviderError::InvalidResponse(
                "Wikidata returned an unrequested formatter property".to_owned(),
            ));
        }
        let (formatter, formatter_omissions) = bounded_display(&binding.formatter.value)?;
        omissions.merge(formatter_omissions);
        if let Some(formatter) = formatter.filter(|value| value.contains("$1")) {
            formatters.entry(property_id).or_insert(formatter);
        }
    }
    Ok((formatters, omissions))
}

/// Gives follower observations a stable service/rank/date ordering.
fn compare_follower_values(
    left: &PendingStatementValue,
    right: &PendingStatementValue,
) -> std::cmp::Ordering {
    let (
        PendingStatementValue::SocialFollowers {
            amount: left_amount,
            service_property_ids: left_services,
            dates: left_dates,
            rank: left_rank,
        },
        PendingStatementValue::SocialFollowers {
            amount: right_amount,
            service_property_ids: right_services,
            dates: right_dates,
            rank: right_rank,
        },
    ) = (left, right)
    else {
        return std::cmp::Ordering::Equal;
    };
    left_services
        .cmp(right_services)
        .then_with(|| follower_rank_order(*left_rank).cmp(&follower_rank_order(*right_rank)))
        .then_with(|| {
            let left_date = left_dates.first().map_or("", follower_date_sort_key);
            let right_date = right_dates.first().map_or("", follower_date_sort_key);
            right_date.cmp(left_date)
        })
        .then_with(|| left_amount.cmp(right_amount))
}

const fn follower_rank_order(rank: RawStatementRank) -> u8 {
    match rank {
        RawStatementRank::Preferred => 0,
        RawStatementRank::Normal => 1,
        RawStatementRank::Deprecated => 2,
    }
}

fn render_entity_statements(
    pending: PendingEntityStatements,
    labels: &BTreeMap<String, String>,
    formatter_urls: &BTreeMap<String, String>,
) -> WikidataEntityStatements {
    let omissions = pending.omissions;
    WikidataEntityStatements {
        item_id: pending.item_id,
        statements: pending
            .statements
            .into_iter()
            .map(|statement| {
                let property_id = statement.property_id;
                let mut pending_values = statement.values;
                if property_id == "P8687" {
                    pending_values.sort_by(compare_follower_values);
                }
                let values = pending_values
                    .iter()
                    .map(|value| value.render(&property_id, labels, formatter_urls))
                    .collect();
                WikidataStatement {
                    property_label: labels
                        .get(&property_id)
                        .cloned()
                        .unwrap_or_else(|| property_id.clone()),
                    property_id,
                    values,
                }
            })
            .collect(),
        wikipedia_sitelinks: pending.wikipedia_sitelinks,
        wikipedia_sitelinks_omitted: pending.wikipedia_sitelinks_omitted,
        truncated: omissions.is_truncated(),
        hard_bounds_reached: omissions.hard_bounds_reached,
        unsupported_values_omitted: omissions.unsupported_values_omitted,
    }
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
    wikidata_entity_id(uri, 'Q', "item")
}

fn wikidata_property_id(uri: &str) -> Result<String, ProviderError> {
    wikidata_entity_id(uri, 'P', "property")
}

fn wikidata_entity_id(
    uri: &str,
    expected_prefix: char,
    kind: &str,
) -> Result<String, ProviderError> {
    let url = Url::parse(uri)
        .map_err(|error| ProviderError::InvalidResponse(format!("invalid {kind} URI: {error}")))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str() != Some("www.wikidata.org")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ProviderError::InvalidResponse(format!(
            "Wikidata result contains a foreign {kind} URI"
        )));
    }
    let Some(entity_id) = url.path().strip_prefix("/entity/") else {
        return Err(ProviderError::InvalidResponse(format!(
            "Wikidata {kind} URI has an unexpected path"
        )));
    };
    if !valid_prefixed_decimal_id(entity_id, expected_prefix) {
        return Err(ProviderError::InvalidResponse(format!(
            "Wikidata result contains an invalid {kind} identifier"
        )));
    }
    Ok(entity_id.to_owned())
}

#[derive(Debug, Deserialize)]
struct EntityStatementsResponse {
    entities: BTreeMap<String, RawStatementEntity>,
}

#[derive(Debug, Deserialize)]
struct RawStatementEntity {
    #[serde(default)]
    claims: BTreeMap<String, Vec<RawClaim>>,
    #[serde(default)]
    sitelinks: BTreeMap<String, RawSitelink>,
}

#[derive(Debug, Deserialize)]
struct RawSitelink {
    site: String,
    title: String,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawClaim {
    mainsnak: RawMainSnak,
    #[serde(default)]
    qualifiers: BTreeMap<String, Vec<Value>>,
    #[serde(default)]
    rank: RawStatementRank,
}

/// Wikibase statement rank retained only where it changes follower history.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum RawStatementRank {
    Preferred,
    #[default]
    Normal,
    Deprecated,
}

#[derive(Debug, Deserialize)]
struct RawMainSnak {
    #[serde(rename = "snaktype")]
    snak_type: String,
    property: String,
    #[serde(default, rename = "datatype")]
    data_type: Option<String>,
    #[serde(rename = "datavalue")]
    data_value: Option<RawDataValue>,
}

#[derive(Debug, Deserialize)]
struct RawDataValue {
    #[serde(rename = "type")]
    value_type: String,
    value: Value,
}

#[derive(Debug, Deserialize)]
struct RawEntityId {
    id: String,
}

#[derive(Debug, Deserialize)]
struct RawMonolingualText {
    text: String,
    language: String,
}

#[derive(Debug, Deserialize)]
struct RawQuantity {
    amount: String,
    unit: String,
    #[serde(rename = "lowerBound")]
    lower_bound: Option<String>,
    #[serde(rename = "upperBound")]
    upper_bound: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawTime {
    time: String,
    precision: u8,
    #[serde(rename = "calendarmodel")]
    calendar_model: String,
}

#[derive(Debug, Deserialize)]
struct RawCoordinate {
    latitude: f64,
    longitude: f64,
    altitude: Option<f64>,
    globe: String,
}

#[derive(Debug, Deserialize)]
struct EntityLabelResponse {
    #[serde(default)]
    entities: BTreeMap<String, RawLabelEntity>,
}

#[derive(Debug, Deserialize)]
struct RawLabelEntity {
    #[serde(default)]
    labels: BTreeMap<String, RawLabel>,
}

#[derive(Debug, Deserialize)]
struct RawLabel {
    value: String,
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

#[derive(Debug, Deserialize)]
struct FormatterSparqlResponse {
    results: FormatterSparqlResults,
}

#[derive(Debug, Deserialize)]
struct FormatterSparqlResults {
    #[serde(default)]
    bindings: Vec<FormatterSparqlBinding>,
}

#[derive(Debug, Deserialize)]
struct FormatterSparqlBinding {
    property: SparqlValue,
    formatter: SparqlValue,
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    use super::*;

    const ENTITY_STATEMENTS_FIXTURE: &str = r#"{
      "entities": {
        "Q42": {
          "id": "Q42",
          "claims": {
            "P31": [
              {
                "mainsnak": {
                  "snaktype": "value",
                  "property": "P31",
                  "datatype": "wikibase-item",
                  "datavalue": {
                    "value": {"entity-type": "item", "numeric-id": 5, "id": "Q5"},
                    "type": "wikibase-entityid"
                  }
                },
                "id": "metadata-must-not-leak",
                "rank": "preferred",
                "qualifiers": {"P580": [{"hash": "qualifier-hash-must-not-leak"}]},
                "references": [{"hash": "reference-hash-must-not-leak"}]
              },
              {
                "mainsnak": {
                  "snaktype": "value",
                  "property": "P31",
                  "datavalue": {
                    "value": {"entity-type": "item", "numeric-id": 215627, "id": "Q215627"},
                    "type": "wikibase-entityid"
                  }
                }
              }
            ],
            "P1476": [{
              "mainsnak": {
                "snaktype": "value",
                "property": "P1476",
                "datavalue": {
                  "value": {"text": "The Hitchhiker's Guide to the Galaxy", "language": "en"},
                  "type": "monolingualtext"
                }
              }
            }],
            "P2048": [{
              "mainsnak": {
                "snaktype": "value",
                "property": "P2048",
                "datavalue": {
                  "value": {
                    "amount": "+1.80",
                    "unit": "http://www.wikidata.org/entity/Q11573",
                    "lowerBound": "+1.79",
                    "upperBound": "+1.81"
                  },
                  "type": "quantity"
                }
              }
            }],
            "P569": [{
              "mainsnak": {
                "snaktype": "value",
                "property": "P569",
                "datavalue": {
                  "value": {
                    "time": "+00000001952-03-11T00:00:00Z",
                    "timezone": 0,
                    "before": 0,
                    "after": 0,
                    "precision": 11,
                    "calendarmodel": "http://www.wikidata.org/entity/Q1985727"
                  },
                  "type": "time"
                }
              }
            }],
            "P625": [{
              "mainsnak": {
                "snaktype": "value",
                "property": "P625",
                "datavalue": {
                  "value": {
                    "latitude": 51.501,
                    "longitude": -0.142,
                    "altitude": null,
                    "precision": 0.001,
                    "globe": "http://www.wikidata.org/entity/Q2"
                  },
                  "type": "globecoordinate"
                }
              }
            }],
            "P999": [
              {"mainsnak": {"snaktype": "novalue", "property": "P999"}},
              {"mainsnak": {"snaktype": "somevalue", "property": "P999"}}
            ]
          }
        }
      }
    }"#;

    const ENTITY_LABELS_FIXTURE: &str = r#"{
      "entities": {
        "P31": {"id": "P31", "labels": {"en": {"language": "en", "value": "instance of"}}},
        "P1476": {"id": "P1476", "labels": {"en": {"language": "en", "value": "title"}}},
        "P2048": {"id": "P2048", "labels": {"en": {"language": "en", "value": "height"}}},
        "P569": {"id": "P569", "labels": {"en": {"language": "en", "value": "date of birth"}}},
        "P625": {"id": "P625", "labels": {"en": {"language": "en", "value": "coordinate location"}}},
        "P999": {"id": "P999", "labels": {"en": {"language": "en", "value": "fixture property"}}},
        "Q5": {"id": "Q5", "labels": {"en": {"language": "en", "value": "human"}}},
        "Q215627": {"id": "Q215627", "labels": {"en": {"language": "en", "value": "person"}}},
        "Q11573": {"id": "Q11573", "labels": {"en": {"language": "en", "value": "metre"}}},
        "Q1985727": {"id": "Q1985727", "labels": {"en": {"language": "en", "value": "proleptic Gregorian calendar"}}},
        "Q2": {"id": "Q2", "labels": {"en": {"language": "en", "value": "Earth"}}}
      },
      "success": 1
    }"#;

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
    fn musicbrainz_recording_query_uses_p4404_and_the_exact_uuid() {
        const RECORDING_ID: &str = "bcf01a23-5fcc-4a59-96b3-817da5f37077";

        let url = build_query_url(WikidataExternalKind::MusicBrainzRecording, RECORDING_ID)
            .expect("MusicBrainz query URL");
        let query = url
            .query_pairs()
            .find(|(key, _)| key == "query")
            .map(|(_, value)| value.into_owned())
            .expect("SPARQL query");

        assert!(query.contains(&format!("VALUES ?externalId {{ \"{RECORDING_ID}\" }}")));
        assert!(query.contains("wdt:P4404 ?externalId"));
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
    fn musicbrainz_recording_validation_accepts_only_lowercase_canonical_uuids() {
        const RECORDING_ID: &str = "bcf01a23-5fcc-4a59-96b3-817da5f37077";
        let provider = WikidataProvider::new();

        assert!(
            validate_external_id(WikidataExternalKind::MusicBrainzRecording, RECORDING_ID).is_ok()
        );
        for invalid in [
            "BCF01A23-5FCC-4A59-96B3-817DA5F37077",
            "bcf01a235fcc4a5996b3817da5f37077",
            "{bcf01a23-5fcc-4a59-96b3-817da5f37077}",
            "urn:uuid:bcf01a23-5fcc-4a59-96b3-817da5f37077",
            "bcf01a23-5fcc-4a59-96b3-817da5f3707g",
            "bcf01a23-5fcc-4a59-96b3-817da5f3707",
            " bcf01a23-5fcc-4a59-96b3-817da5f37077",
        ] {
            let error = provider
                .lookup_external(WikidataExternalKind::MusicBrainzRecording, invalid)
                .expect_err("invalid input must fail before a network lookup");
            assert!(
                matches!(error, ProviderError::InvalidRequest(_)),
                "noncanonical MusicBrainz recording ID should be rejected: {invalid}"
            );
        }
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
    fn nonpolitical_musicbrainz_recording_fixture_uses_generic_normalization() {
        const RECORDING_ID: &str = "bcf01a23-5fcc-4a59-96b3-817da5f37077";
        let response: SparqlResponse = serde_json::from_str(
            r#"{
              "results": {
                "bindings": [{
                  "item": {"type": "uri", "value": "http://www.wikidata.org/entity/Q1747485"},
                  "itemLabel": {"type": "literal", "value": "Wanna Get to Know You"},
                  "itemDescription": {"type": "literal", "value": "2003 song by G-Unit"}
                }]
              }
            }"#,
        )
        .expect("MusicBrainz fixture JSON");

        let lookup = normalize_response(
            WikidataExternalKind::MusicBrainzRecording,
            RECORDING_ID,
            response,
        )
        .expect("MusicBrainz fixture should normalize");

        assert_eq!(lookup.kind, WikidataExternalKind::MusicBrainzRecording);
        assert_eq!(lookup.external_id, RECORDING_ID);
        assert_eq!(lookup.items.len(), 1);
        assert_eq!(lookup.items[0].item_id, "Q1747485");
        assert_eq!(lookup.items[0].label, "Wanna Get to Know You");
        assert_eq!(
            lookup.items[0].url.as_str(),
            "https://www.wikidata.org/wiki/Q1747485"
        );
    }

    #[test]
    fn foreign_and_malformed_entity_uris_are_rejected() {
        assert!(wikidata_item_id("https://evil.test/entity/Q42").is_err());
        assert!(wikidata_item_id("https://www.wikidata.org/wiki/Q42").is_err());
        assert!(wikidata_item_id("http://www.wikidata.org/entity/not-q").is_err());
    }

    #[test]
    fn entity_statements_resolve_labels_and_omit_internal_metadata() {
        let server = MockServer::spawn(vec![
            json_response("200 OK", ENTITY_STATEMENTS_FIXTURE),
            json_response("200 OK", ENTITY_LABELS_FIXTURE),
        ]);
        let provider = WikidataProvider::with_statement_endpoints(
            server.base_url.join("w/api.php").expect("label API URL"),
            MAX_ENTITY_RESPONSE_BYTES,
            MAX_LABEL_RESPONSE_BYTES,
        );

        let result = provider
            .load_entity_statements("Q42")
            .expect("fixture statements should load");
        let requests = server.finish();

        assert_eq!(result.item_id, "Q42");
        assert!(!result.truncated);
        assert_eq!(requests.len(), 2);
        let entity_url = server_url(&requests[0]);
        let entity_pairs = entity_url.query_pairs().collect::<BTreeMap<_, _>>();
        assert_eq!(entity_url.path(), "/w/api.php");
        assert_eq!(
            entity_pairs.get("action").map(AsRef::as_ref),
            Some("wbgetentities")
        );
        assert_eq!(entity_pairs.get("ids").map(AsRef::as_ref), Some("Q42"));
        assert_eq!(
            entity_pairs.get("props").map(AsRef::as_ref),
            Some("claims|sitelinks/urls")
        );
        let label_url = server_url(&requests[1]);
        let label_pairs = label_url.query_pairs().collect::<BTreeMap<_, _>>();
        assert_eq!(
            label_pairs.get("action").map(AsRef::as_ref),
            Some("wbgetentities")
        );
        assert_eq!(label_pairs.get("props").map(AsRef::as_ref), Some("labels"));
        let requested_labels = label_pairs
            .get("ids")
            .expect("label IDs")
            .split('|')
            .collect::<Vec<_>>();
        assert!(requested_labels.contains(&"P31"));
        assert!(requested_labels.contains(&"Q5"));
        assert!(requested_labels.len() <= MAX_LABEL_IDS);

        let instance_of = result
            .statements
            .iter()
            .find(|statement| statement.property_id == "P31")
            .expect("instance-of statement");
        assert_eq!(instance_of.property_label, "instance of");
        assert_eq!(
            instance_of.values,
            vec![
                WikidataStatementValue {
                    display: "human".to_owned(),
                    item_id: Some("Q5".to_owned()),
                    external_url: None,
                    preview_url: None,
                },
                WikidataStatementValue {
                    display: "person".to_owned(),
                    item_id: Some("Q215627".to_owned()),
                    external_url: None,
                    preview_url: None,
                },
            ]
        );
        assert_eq!(
            statement_value(&result, "P2048"),
            "+1.80 (+1.79–+1.81) metre"
        );
        assert_eq!(
            statement_value(&result, "P569"),
            "1952-03-11 (proleptic Gregorian calendar)"
        );
        assert_eq!(statement_value(&result, "P625"), "51.501, -0.142 (Earth)");
        let special_values = result
            .statements
            .iter()
            .find(|statement| statement.property_id == "P999")
            .expect("special values");
        assert_eq!(
            special_values
                .values
                .iter()
                .map(|value| value.display.as_str())
                .collect::<Vec<_>>(),
            vec!["no value", "unknown value"]
        );

        let diagnostic = format!("{result:?}");
        assert!(!diagnostic.contains("metadata-must-not-leak"));
        assert!(!diagnostic.contains("qualifier-hash-must-not-leak"));
        assert!(!diagnostic.contains("reference-hash-must-not-leak"));
        assert!(!diagnostic.contains("preferred"));
    }

    #[test]
    fn follower_history_uses_service_and_date_qualifiers_without_leaking_accounts() {
        let response: EntityStatementsResponse = serde_json::from_value(serde_json::json!({
            "entities": {
                "Q42": {
                    "claims": {
                        "P8687": [
                            follower_claim_fixture(
                                "+490181",
                                "P6552",
                                "account-secret-x",
                                "+00000002023-02-06T00:00:00Z",
                                "normal",
                            ),
                            follower_claim_fixture(
                                "+1880000",
                                "P2397",
                                "account-secret-youtube",
                                "+00000002025-03-01T00:00:00Z",
                                "preferred",
                            ),
                            follower_claim_fixture(
                                "+136000",
                                "P2397",
                                "deprecated-account-secret",
                                "+00000002020-01-01T00:00:00Z",
                                "deprecated",
                            ),
                            follower_claim_fixture(
                                "+604",
                                "P4033",
                                "account-secret-mastodon",
                                "+00000002024-04-11T00:00:00Z",
                                "normal",
                            ),
                        ]
                    }
                }
            }
        }))
        .expect("follower fixture JSON");
        let pending =
            normalize_entity_claims("Q42", response).expect("follower claims should normalize");
        let (label_ids, truncated) = collect_label_ids(&pending);
        assert!(!truncated);
        for expected in ["P8687", "P2397", "P4033", "P6552", "Q1985727"] {
            assert!(label_ids.iter().any(|label_id| label_id == expected));
        }
        let labels = BTreeMap::from([
            ("P8687".to_owned(), "social media followers".to_owned()),
            ("P2397".to_owned(), "YouTube channel ID".to_owned()),
            ("P4033".to_owned(), "Mastodon address".to_owned()),
            ("P6552".to_owned(), "X numeric user ID".to_owned()),
            (
                "Q1985727".to_owned(),
                "proleptic Gregorian calendar".to_owned(),
            ),
        ]);

        let result = render_entity_statements(pending, &labels, &BTreeMap::new());
        let followers = result
            .statements
            .iter()
            .find(|statement| statement.property_id == "P8687")
            .expect("follower statement");
        assert_eq!(
            followers
                .values
                .iter()
                .map(|value| value.display.as_str())
                .collect::<Vec<_>>(),
            [
                "YouTube · 2025 March 1 · 1,880,000 followers",
                "YouTube · 2020 January 1 · 136,000 followers · deprecated",
                "Mastodon · 2024 April 11 · 604 followers",
                "X (Twitter) · 2023 February 6 · 490,181 followers",
            ]
        );
        let diagnostic = format!("{result:?}");
        for secret in [
            "account-secret-x",
            "account-secret-youtube",
            "deprecated-account-secret",
            "account-secret-mastodon",
            "qualifier-secret-hash",
            "reference-secret-hash",
        ] {
            assert!(!diagnostic.contains(secret));
        }
        assert!(
            !followers.values[0].display.contains('–'),
            "quantity uncertainty bounds are not part of follower history"
        );
    }

    #[test]
    fn follower_qualifiers_are_bounded_and_ambiguity_is_explicit() {
        let account_qualifiers = (0..=MAX_FOLLOWER_QUALIFIER_SNAKS)
            .map(|index| {
                serde_json::json!({
                    "snaktype": "value",
                    "property": "P2397",
                    "datatype": "external-id",
                    "datavalue": {
                        "value": format!("private-account-{index}"),
                        "type": "string"
                    }
                })
            })
            .collect();
        let claim = RawClaim {
            mainsnak: RawMainSnak {
                snak_type: "value".to_owned(),
                property: "P8687".to_owned(),
                data_type: Some("quantity".to_owned()),
                data_value: Some(RawDataValue {
                    value_type: "quantity".to_owned(),
                    value: serde_json::json!({
                        "amount": "+1000",
                        "unit": "1",
                        "lowerBound": "+999",
                        "upperBound": "+1001"
                    }),
                }),
            },
            qualifiers: BTreeMap::from([("P2397".to_owned(), account_qualifiers)]),
            rank: RawStatementRank::Normal,
        };

        let (value, omissions) =
            normalize_social_followers(claim).expect("bounded qualifiers should normalize");
        assert!(omissions.hard_bounds_reached);
        let rendered =
            value
                .expect("follower value")
                .render("P8687", &BTreeMap::new(), &BTreeMap::new());
        assert_eq!(
            rendered.display,
            "Multiple service accounts: YouTube · date unknown · 1,000 followers"
        );
        assert!(!format!("{rendered:?}").contains("private-account"));
    }

    /// Builds a realistic P8687 claim with private qualifier metadata.
    fn follower_claim_fixture(
        amount: &str,
        service_property: &str,
        account_id: &str,
        point_in_time: &str,
        rank: &str,
    ) -> Value {
        serde_json::json!({
            "mainsnak": {
                "snaktype": "value",
                "property": "P8687",
                "datatype": "quantity",
                "datavalue": {
                    "value": {
                        "amount": amount,
                        "unit": "1",
                        "lowerBound": format!("{amount}0"),
                        "upperBound": format!("{amount}9")
                    },
                    "type": "quantity"
                }
            },
            "rank": rank,
            "qualifiers": {
                (service_property): [{
                    "snaktype": "value",
                    "property": service_property,
                    "datatype": "external-id",
                    "datavalue": {"value": account_id, "type": "string"},
                    "hash": "qualifier-secret-hash"
                }],
                "P585": [{
                    "snaktype": "value",
                    "property": "P585",
                    "datatype": "time",
                    "datavalue": {
                        "value": {
                            "time": point_in_time,
                            "timezone": 0,
                            "before": 0,
                            "after": 0,
                            "precision": 11,
                            "calendarmodel": "http://www.wikidata.org/entity/Q1985727"
                        },
                        "type": "time"
                    }
                }]
            },
            "references": [{"hash": "reference-secret-hash"}]
        })
    }

    #[test]
    fn external_ids_and_multiple_p18_values_expose_safe_provider_links() {
        let entity_fixture = serde_json::json!({
            "entities": {
                "Q42": {
                    "claims": {
                        "P18": [
                            {
                                "mainsnak": {
                                    "snaktype": "value",
                                    "property": "P18",
                                    "datatype": "commonsMedia",
                                    "datavalue": {
                                        "value": "Portrait with a space.jpg",
                                        "type": "string"
                                    }
                                }
                            },
                            {
                                "mainsnak": {
                                    "snaktype": "value",
                                    "property": "P18",
                                    "datatype": "commonsMedia",
                                    "datavalue": {
                                        "value": "Second portrait.jpg",
                                        "type": "string"
                                    }
                                }
                            }
                        ],
                        "P51": [
                            {
                                "mainsnak": {
                                    "snaktype": "value",
                                    "property": "P51",
                                    "datatype": "commonsMedia",
                                    "datavalue": {
                                        "value": "First recording.opus",
                                        "type": "string"
                                    }
                                }
                            },
                            {
                                "mainsnak": {
                                    "snaktype": "value",
                                    "property": "P51",
                                    "datatype": "commonsMedia",
                                    "datavalue": {
                                        "value": "Second recording.FLAC",
                                        "type": "string"
                                    }
                                }
                            },
                            {
                                "mainsnak": {
                                    "snaktype": "value",
                                    "property": "P51",
                                    "datatype": "commonsMedia",
                                    "datavalue": {
                                        "value": "Unsupported recording.mp3",
                                        "type": "string"
                                    }
                                }
                            }
                        ],
                        "P10": [
                            {
                                "mainsnak": {
                                    "snaktype": "value",
                                    "property": "P10",
                                    "datatype": "commonsMedia",
                                    "datavalue": {
                                        "value": "First video.webm",
                                        "type": "string"
                                    }
                                }
                            },
                            {
                                "mainsnak": {
                                    "snaktype": "value",
                                    "property": "P10",
                                    "datatype": "commonsMedia",
                                    "datavalue": {
                                        "value": "Second video.ogv",
                                        "type": "string"
                                    }
                                }
                            },
                            {
                                "mainsnak": {
                                    "snaktype": "value",
                                    "property": "P10",
                                    "datatype": "commonsMedia",
                                    "datavalue": {
                                        "value": "Unsupported video.mp4",
                                        "type": "string"
                                    }
                                }
                            }
                        ],
                        "P2002": [{
                            "mainsnak": {
                                "snaktype": "value",
                                "property": "P2002",
                                "datatype": "external-id",
                                "datavalue": {
                                    "value": "name/with space",
                                    "type": "string"
                                }
                            }
                        }],
                        "P373": [{
                            "mainsnak": {
                                "snaktype": "value",
                                "property": "P373",
                                "datatype": "string",
                                "datavalue": {
                                    "value": "Douglas Adams / portraits",
                                    "type": "string"
                                }
                            }
                        }]
                    }
                }
            }
        })
        .to_string();
        let label_fixture = serde_json::json!({
            "entities": {
                "P18": {
                    "labels": {"en": {"value": "image"}}
                },
                "P51": {
                    "labels": {"en": {"value": "audio"}}
                },
                "P10": {
                    "labels": {"en": {"value": "video"}}
                },
                "P2002": {
                    "labels": {"en": {"value": "X username"}}
                },
                "P373": {
                    "labels": {"en": {"value": "Commons category"}}
                }
            }
        })
        .to_string();
        let formatter_fixture = serde_json::json!({
            "results": {
                "bindings": [{
                    "property": {
                        "type": "uri",
                        "value": "http://www.wikidata.org/entity/P2002"
                    },
                    "formatter": {
                        "type": "uri",
                        "value": "https://x.com/$1"
                    }
                }]
            }
        })
        .to_string();
        let server = MockServer::spawn(vec![
            json_response("200 OK", &entity_fixture),
            json_response("200 OK", &label_fixture),
            json_response("200 OK", &formatter_fixture),
        ]);
        let provider = WikidataProvider::with_statement_endpoints(
            server.base_url.join("w/api.php").expect("entity API URL"),
            MAX_ENTITY_RESPONSE_BYTES,
            MAX_LABEL_RESPONSE_BYTES,
        );

        let result = provider
            .load_entity_statements("Q42")
            .expect("linked statements should load");
        let requests = server.finish();

        assert_eq!(requests.len(), 3);
        let formatter_url = server_url(&requests[2]);
        let formatter_pairs = formatter_url.query_pairs().collect::<BTreeMap<_, _>>();
        let formatter_query = formatter_pairs.get("query").expect("formatter query");
        assert!(formatter_query.contains("VALUES ?property { wd:P2002 }"));
        assert!(!formatter_query.contains("wd:P373"));
        assert!(formatter_query.contains("?property wdt:P1630 ?formatterUrl"));
        let images = result
            .statements
            .iter()
            .find(|statement| statement.property_id == "P18")
            .map(|statement| statement.values.as_slice())
            .expect("image statement values");
        assert_eq!(
            images
                .iter()
                .map(|image| image.external_url.as_ref().map(Url::as_str))
                .collect::<Vec<_>>(),
            [
                Some("https://commons.wikimedia.org/wiki/File:Portrait%20with%20a%20space.jpg"),
                Some("https://commons.wikimedia.org/wiki/File:Second%20portrait.jpg"),
            ]
        );
        assert!(
            images
                .iter()
                .all(|image| image.commons_playback("P18").is_none()),
            "P18 must remain image-only even though it is Commons media"
        );
        let audio = result
            .statements
            .iter()
            .find(|statement| statement.property_id == "P51")
            .map(|statement| statement.values.as_slice())
            .expect("audio statement values");
        assert_eq!(
            audio
                .iter()
                .map(|value| {
                    value
                        .commons_playback("P51")
                        .map(|media| (media.kind, media.playback_url))
                })
                .collect::<Vec<_>>(),
            [
                Some((
                    WikidataPlayableMediaKind::Audio,
                    Url::parse(
                        "https://commons.wikimedia.org/wiki/Special:Redirect/file/\
                         First%20recording.opus"
                    )
                    .expect("first audio playback URL"),
                )),
                Some((
                    WikidataPlayableMediaKind::Audio,
                    Url::parse(
                        "https://commons.wikimedia.org/wiki/Special:Redirect/file/\
                         Second%20recording.FLAC"
                    )
                    .expect("second audio playback URL"),
                )),
                None,
            ]
        );
        assert!(
            audio.iter().all(|value| value.external_url.is_some()),
            "unsupported audio must remain a clickable Commons file page"
        );
        let video = result
            .statements
            .iter()
            .find(|statement| statement.property_id == "P10")
            .map(|statement| statement.values.as_slice())
            .expect("video statement values");
        assert_eq!(
            video
                .iter()
                .map(|value| value.commons_playback("P10").map(|media| media.kind))
                .collect::<Vec<_>>(),
            [
                Some(WikidataPlayableMediaKind::Video),
                Some(WikidataPlayableMediaKind::Video),
                None,
            ]
        );
        assert!(
            video.iter().all(|value| value.external_url.is_some()),
            "unsupported video must remain a clickable Commons file page"
        );
        assert_eq!(
            images
                .iter()
                .map(|image| image.preview_url.as_ref().map(Url::as_str))
                .collect::<Vec<_>>(),
            [
                Some(
                    "https://commons.wikimedia.org/wiki/Special:Redirect/file/\
                     Portrait%20with%20a%20space.jpg?width=512"
                ),
                Some(
                    "https://commons.wikimedia.org/wiki/Special:Redirect/file/\
                     Second%20portrait.jpg?width=512"
                ),
            ]
        );
        let username = result
            .statements
            .iter()
            .find(|statement| statement.property_id == "P2002")
            .and_then(|statement| statement.values.first())
            .expect("external-ID statement value");
        assert_eq!(
            username.external_url.as_ref().map(Url::as_str),
            Some("https://x.com/name%2Fwith%20space")
        );
        let commons_category = result
            .statements
            .iter()
            .find(|statement| statement.property_id == "P373")
            .and_then(|statement| statement.values.first())
            .expect("Commons-category statement value");
        assert_eq!(commons_category.display, "Douglas Adams / portraits");
        assert_eq!(
            commons_category.external_url.as_ref().map(Url::as_str),
            Some(
                "https://commons.wikimedia.org/wiki/Category:\
                 Douglas%20Adams%20%2F%20portraits"
            )
        );
        assert!(commons_category.preview_url.is_none());
        assert!(!result.truncated);
    }

    #[test]
    fn many_external_ids_use_one_minimal_formatter_query() {
        const EXTERNAL_PROPERTY_COUNT: usize = 20;
        let mut claims = serde_json::Map::new();
        let mut formatter_bindings = Vec::new();
        for index in 1..=EXTERNAL_PROPERTY_COUNT {
            let property_id = format!("P{index}");
            claims.insert(
                property_id.clone(),
                serde_json::json!([{
                    "mainsnak": {
                        "snaktype": "value",
                        "property": property_id,
                        "datatype": "external-id",
                        "datavalue": {
                            "value": format!("fixture-{index}"),
                            "type": "string"
                        }
                    }
                }]),
            );
            formatter_bindings.push(serde_json::json!({
                "property": {
                    "type": "uri",
                    "value": format!("http://www.wikidata.org/entity/P{index}")
                },
                "formatter": {
                    "type": "uri",
                    "value": format!("https://catalog.example/{index}/$1")
                }
            }));
        }
        let entity_fixture = serde_json::json!({
            "entities": {
                "Q42": {"claims": Value::Object(claims)}
            }
        })
        .to_string();
        let formatter_fixture = serde_json::json!({
            "results": {"bindings": formatter_bindings}
        })
        .to_string();
        let server = MockServer::spawn(vec![
            json_response("200 OK", &entity_fixture),
            json_response("200 OK", r#"{"entities": {}}"#),
            json_response("200 OK", &formatter_fixture),
        ]);
        let provider = WikidataProvider::with_statement_endpoints(
            server.base_url.join("w/api.php").expect("entity API URL"),
            MAX_ENTITY_RESPONSE_BYTES,
            MAX_LABEL_RESPONSE_BYTES,
        );

        let result = provider
            .load_entity_statements("Q42")
            .expect("many formatter-backed IDs should remain below the bounded response");
        let requests = server.finish();

        assert_eq!(requests.len(), 3);
        assert!(requests[2].starts_with("/sparql?"));
        let formatter_query = server_url(&requests[2])
            .query_pairs()
            .find(|(key, _)| key == "query")
            .map(|(_, value)| value.into_owned())
            .expect("formatter query");
        assert_eq!(
            formatter_query.matches("wd:P").count(),
            EXTERNAL_PROPERTY_COUNT
        );
        assert!(!formatter_query.contains("props=claims"));
        assert_eq!(
            result
                .statements
                .iter()
                .filter_map(|statement| statement.values.first())
                .filter(|value| value.external_url.is_some())
                .count(),
            EXTERNAL_PROPERTY_COUNT
        );
        assert!(!result.truncated);
    }

    #[test]
    fn serialized_statement_values_remain_compatible_without_external_url() {
        let value: WikidataStatementValue =
            serde_json::from_str(r#"{"display":"fixture","item_id":null}"#)
                .expect("older cached value should deserialize");

        assert_eq!(value.display, "fixture");
        assert!(value.external_url.is_none());
        assert!(value.preview_url.is_none());
        assert_eq!(
            serde_json::to_value(&value).expect("statement value should serialize"),
            serde_json::json!({"display": "fixture", "item_id": null})
        );

        let entity: WikidataEntityStatements = serde_json::from_value(serde_json::json!({
            "item_id": "Q42",
            "statements": [],
            "truncated": true
        }))
        .expect("older cached entity should deserialize");
        assert!(!entity.hard_bounds_reached);
        assert!(!entity.unsupported_values_omitted);
        assert!(entity.wikipedia_sitelinks.is_empty());
        assert!(!entity.wikipedia_sitelinks_omitted);

        let linked = WikidataWikipediaSitelink {
            site_id: "be_x_oldwiki".to_owned(),
            project_label: "be-tarask.wikipedia.org".to_owned(),
            title: "Дуглас Адамз".to_owned(),
            url: Url::parse("https://be-tarask.wikipedia.org/wiki/Дуглас_Адамз")
                .expect("Wikipedia fixture URL"),
        };
        let mut linked_entity = entity;
        linked_entity.wikipedia_sitelinks.push(linked.clone());
        assert_eq!(
            serde_json::from_value::<WikidataEntityStatements>(
                serde_json::to_value(&linked_entity).expect("linked entity should serialize")
            )
            .expect("linked entity should deserialize")
            .wikipedia_sitelinks,
            vec![linked]
        );
    }

    #[test]
    fn canonical_wikipedia_sitelinks_are_bounded_sorted_and_strictly_validated() {
        let mut sitelinks = BTreeMap::from([
            (
                "simplewiki".to_owned(),
                raw_sitelink(
                    "simplewiki",
                    "Douglas Adams",
                    "https://simple.wikipedia.org/wiki/Douglas_Adams",
                ),
            ),
            (
                "enwiki".to_owned(),
                raw_sitelink(
                    "enwiki",
                    "Douglas Adams",
                    "https://en.wikipedia.org/wiki/Douglas_Adams",
                ),
            ),
            (
                "be_x_oldwiki".to_owned(),
                raw_sitelink(
                    "be_x_oldwiki",
                    "Дуглас Адамз",
                    "https://be-tarask.wikipedia.org/wiki/%D0%94%D1%83%D0%B3%D0%BB%D0%B0%D1%81_%D0%90%D0%B4%D0%B0%D0%BC%D0%B7",
                ),
            ),
            (
                "zh_classicalwiki".to_owned(),
                raw_sitelink(
                    "zh_classicalwiki",
                    "道格拉斯·亞當斯",
                    "https://zh-classical.wikipedia.org/wiki/%E9%81%93%E6%A0%BC%E6%8B%89%E6%96%AF%C2%B7%E4%BA%9E%E7%95%B6%E6%96%AF",
                ),
            ),
            (
                "duplicatewiki".to_owned(),
                raw_sitelink(
                    "duplicatewiki",
                    "Duplicate",
                    "https://en.wikipedia.org/wiki/Douglas_Adams",
                ),
            ),
            (
                "commonswiki".to_owned(),
                raw_sitelink(
                    "commonswiki",
                    "Douglas Adams",
                    "https://commons.wikimedia.org/wiki/Douglas_Adams",
                ),
            ),
            (
                "evilwiki".to_owned(),
                raw_sitelink(
                    "evilwiki",
                    "Foreign",
                    "https://en.wikipedia.org.evil.test/wiki/Douglas_Adams",
                ),
            ),
            (
                "httpwiki".to_owned(),
                raw_sitelink(
                    "httpwiki",
                    "HTTP",
                    "http://en.wikipedia.org/wiki/Douglas_Adams",
                ),
            ),
            (
                "credentialwiki".to_owned(),
                raw_sitelink(
                    "credentialwiki",
                    "Credentials",
                    "https://user@en.wikipedia.org/wiki/Douglas_Adams",
                ),
            ),
            (
                "portwiki".to_owned(),
                raw_sitelink(
                    "portwiki",
                    "Port",
                    "https://en.wikipedia.org:443/wiki/Douglas_Adams",
                ),
            ),
            (
                "querywiki".to_owned(),
                raw_sitelink(
                    "querywiki",
                    "Query",
                    "https://en.wikipedia.org/wiki/Douglas_Adams?oldid=1",
                ),
            ),
            (
                "fragmentwiki".to_owned(),
                raw_sitelink(
                    "fragmentwiki",
                    "Fragment",
                    "https://en.wikipedia.org/wiki/Douglas_Adams#Life",
                ),
            ),
            (
                "emptywiki".to_owned(),
                raw_sitelink("emptywiki", "Empty", "https://en.wikipedia.org/wiki/"),
            ),
            (
                "mismatchwiki".to_owned(),
                raw_sitelink(
                    "anotherwiki",
                    "Mismatch",
                    "https://en.wikipedia.org/wiki/Douglas_Adams",
                ),
            ),
        ]);
        sitelinks.insert(
            "blankwiki".to_owned(),
            raw_sitelink("blankwiki", "\n\t ", "https://en.wikipedia.org/wiki/Blank"),
        );

        let (normalized, omitted) = normalize_wikipedia_sitelinks(sitelinks);

        assert!(omitted, "unsafe Wikipedia candidates must be reported");
        assert_eq!(
            normalized
                .iter()
                .map(|link| {
                    (
                        link.site_id.as_str(),
                        link.project_label.as_str(),
                        link.title.as_str(),
                    )
                })
                .collect::<Vec<_>>(),
            [
                ("enwiki", "en.wikipedia.org", "Douglas Adams"),
                ("be_x_oldwiki", "be-tarask.wikipedia.org", "Дуглас Адамз"),
                ("simplewiki", "simple.wikipedia.org", "Douglas Adams"),
                (
                    "zh_classicalwiki",
                    "zh-classical.wikipedia.org",
                    "道格拉斯·亞當斯"
                ),
            ]
        );
        assert!(normalized.iter().all(|link| link.url.scheme() == "https"
            && link.url.host_str().is_some_and(wikipedia_project_host)));
    }

    #[test]
    fn wikipedia_sitelink_bound_is_independent_from_statement_limits() {
        let sitelinks = (0..=MAX_WIKIPEDIA_SITELINKS)
            .map(|index| {
                let site_id = format!("x{index:03}wiki");
                (
                    site_id.clone(),
                    raw_sitelink(
                        &site_id,
                        &format!("Fixture {index}"),
                        &format!("https://x{index:03}.wikipedia.org/wiki/Fixture_{index}"),
                    ),
                )
            })
            .collect();

        let (normalized, omitted) = normalize_wikipedia_sitelinks(sitelinks);

        assert_eq!(normalized.len(), MAX_WIKIPEDIA_SITELINKS);
        assert!(omitted);
    }

    #[test]
    fn lexeme_values_keep_their_id_without_aborting_the_label_batch() {
        let entity_fixture = serde_json::json!({
            "entities": {
                "Q42": {
                    "id": "Q42",
                    "claims": {
                        "P1": [{
                            "mainsnak": {
                                "snaktype": "value",
                                "property": "P1",
                                "datavalue": {
                                    "value": {"entity-type": "lexeme", "id": "L42"},
                                    "type": "wikibase-entityid"
                                }
                            }
                        }]
                    }
                }
            }
        })
        .to_string();
        let label_fixture = serde_json::json!({
            "entities": {
                "P1": {
                    "labels": {
                        "en": {"language": "en", "value": "fixture lexeme property"}
                    }
                }
            },
            "success": 1
        })
        .to_string();
        let server = MockServer::spawn(vec![
            json_response("200 OK", &entity_fixture),
            json_response("200 OK", &label_fixture),
        ]);
        let provider = WikidataProvider::with_statement_endpoints(
            server.base_url.join("w/api.php").expect("label API URL"),
            MAX_ENTITY_RESPONSE_BYTES,
            MAX_LABEL_RESPONSE_BYTES,
        );

        let result = provider
            .load_entity_statements("Q42")
            .expect("a lexeme value must not invalidate the P/Q label request");
        let requests = server.finish();

        assert_eq!(requests.len(), 2);
        let label_url = server_url(&requests[1]);
        assert_eq!(
            label_url
                .query_pairs()
                .find(|(key, _)| key == "ids")
                .map(|(_, value)| value.into_owned()),
            Some("P1".to_owned())
        );
        assert_eq!(
            result.statements[0].property_label,
            "fixture lexeme property"
        );
        assert_eq!(
            result.statements[0].values,
            [WikidataStatementValue {
                display: "L42".to_owned(),
                item_id: None,
                external_url: None,
                preview_url: None,
            }]
        );
    }

    #[test]
    fn late_item_values_win_label_capacity_and_preserve_unresolved_ids() {
        let mut claims = serde_json::Map::new();
        for index in 1..=MAX_LABEL_IDS {
            let property_id = format!("P{index}");
            claims.insert(
                property_id.clone(),
                serde_json::json!([{
                    "mainsnak": {
                        "snaktype": "value",
                        "property": property_id,
                        "datavalue": {
                            "value": format!("plain value {index}"),
                            "type": "string"
                        }
                    }
                }]),
            );
        }
        claims.insert(
            "P999".to_owned(),
            serde_json::json!([
                {
                    "mainsnak": {
                        "snaktype": "value",
                        "property": "P999",
                        "datavalue": {
                            "value": {"id": "Q15180"},
                            "type": "wikibase-entityid"
                        }
                    }
                },
                {
                    "mainsnak": {
                        "snaktype": "value",
                        "property": "P999",
                        "datavalue": {
                            "value": {"id": "Q212"},
                            "type": "wikibase-entityid"
                        }
                    }
                },
                {
                    "mainsnak": {
                        "snaktype": "value",
                        "property": "P999",
                        "datavalue": {
                            "value": {"id": "Q999"},
                            "type": "wikibase-entityid"
                        }
                    }
                }
            ]),
        );
        let entity_fixture = serde_json::json!({
            "entities": {
                "Q42": {
                    "id": "Q42",
                    "claims": Value::Object(claims)
                }
            }
        })
        .to_string();
        let label_fixture = serde_json::json!({
            "entities": {
                "Q15180": {
                    "labels": {
                        "en": {"language": "en", "value": "Soviet Union"}
                    }
                },
                "Q212": {
                    "labels": {
                        "en": {"language": "en", "value": "Ukraine"}
                    }
                }
            },
            "success": 1
        })
        .to_string();
        let server = MockServer::spawn(vec![
            json_response("200 OK", &entity_fixture),
            json_response("200 OK", &label_fixture),
            json_response(
                "200 OK",
                r#"{
                  "entities": {
                    "P999": {"labels": {"en": {"value": "late fixture property"}}}
                  },
                  "success": 1
                }"#,
            ),
        ]);
        let provider = WikidataProvider::with_statement_endpoints(
            server.base_url.join("w/api.php").expect("label API URL"),
            MAX_ENTITY_RESPONSE_BYTES,
            MAX_LABEL_RESPONSE_BYTES,
        );

        let result = provider
            .load_entity_statements("Q42")
            .expect("property-rich fixture should load");
        let requests = server.finish();

        assert_eq!(requests.len(), 3);
        let label_url = server_url(&requests[1]);
        let requested_labels = label_url
            .query_pairs()
            .find(|(key, _)| key == "ids")
            .map(|(_, value)| value.split('|').map(str::to_owned).collect::<Vec<_>>())
            .expect("label request IDs");
        assert_eq!(requested_labels.len(), MAX_LABEL_IDS);
        assert_eq!(
            &requested_labels[..3],
            ["Q15180".to_owned(), "Q212".to_owned(), "Q999".to_owned()]
        );
        let second_label_url = server_url(&requests[2]);
        let second_requested_labels = second_label_url
            .query_pairs()
            .find(|(key, _)| key == "ids")
            .map(|(_, value)| value.split('|').map(str::to_owned).collect::<Vec<_>>())
            .expect("second label request IDs");
        assert_eq!(second_requested_labels.len(), 4);
        assert!(!result.truncated);

        let late_values = result
            .statements
            .iter()
            .find(|statement| statement.property_id == "P999")
            .inspect(|statement| {
                assert_eq!(statement.property_label, "late fixture property");
            })
            .map(|statement| &statement.values)
            .expect("late direct-item statement");
        assert_eq!(
            late_values,
            &[
                WikidataStatementValue {
                    display: "Soviet Union".to_owned(),
                    item_id: Some("Q15180".to_owned()),
                    external_url: None,
                    preview_url: None,
                },
                WikidataStatementValue {
                    display: "Ukraine".to_owned(),
                    item_id: Some("Q212".to_owned()),
                    external_url: None,
                    preview_url: None,
                },
                WikidataStatementValue {
                    display: "Q999".to_owned(),
                    item_id: Some("Q999".to_owned()),
                    external_url: None,
                    preview_url: None,
                },
            ]
        );
    }

    #[test]
    fn statement_values_are_capped_and_marked_truncated() {
        let claims = (0..=MAX_VALUES_PER_PROPERTY)
            .map(|index| RawClaim {
                mainsnak: RawMainSnak {
                    snak_type: "value".to_owned(),
                    property: "P1".to_owned(),
                    data_type: Some("string".to_owned()),
                    data_value: Some(RawDataValue {
                        value_type: "string".to_owned(),
                        value: Value::String(format!("value {index}")),
                    }),
                },
                qualifiers: BTreeMap::new(),
                rank: RawStatementRank::Normal,
            })
            .collect();
        let response = EntityStatementsResponse {
            entities: BTreeMap::from([(
                "Q42".to_owned(),
                RawStatementEntity {
                    claims: BTreeMap::from([("P1".to_owned(), claims)]),
                    sitelinks: BTreeMap::new(),
                },
            )]),
        };

        let pending =
            normalize_entity_claims("Q42", response).expect("bounded statements should normalize");

        assert!(pending.omissions.hard_bounds_reached);
        assert_eq!(pending.statements.len(), 1);
        assert_eq!(pending.statements[0].values.len(), MAX_VALUES_PER_PROPERTY);
    }

    #[test]
    fn unsupported_values_are_distinct_from_hard_bounds() {
        let response = EntityStatementsResponse {
            entities: BTreeMap::from([(
                "Q42".to_owned(),
                RawStatementEntity {
                    claims: BTreeMap::from([(
                        "P1".to_owned(),
                        vec![RawClaim {
                            mainsnak: RawMainSnak {
                                snak_type: "value".to_owned(),
                                property: "P1".to_owned(),
                                data_type: Some("future-structured-type".to_owned()),
                                data_value: Some(RawDataValue {
                                    value_type: "future-structured-type".to_owned(),
                                    value: serde_json::json!({"future": "shape"}),
                                }),
                            },
                            qualifiers: BTreeMap::new(),
                            rank: RawStatementRank::Normal,
                        }],
                    )]),
                    sitelinks: BTreeMap::new(),
                },
            )]),
        };

        let pending =
            normalize_entity_claims("Q42", response).expect("unknown structured value is omitted");
        let result = render_entity_statements(pending, &BTreeMap::new(), &BTreeMap::new());

        assert!(result.truncated);
        assert!(!result.hard_bounds_reached);
        assert!(result.unsupported_values_omitted);
    }

    #[test]
    fn label_ids_crossing_an_api_batch_are_not_marked_truncated() {
        let pending = PendingEntityStatements {
            item_id: "Q42".to_owned(),
            statements: (1..=MAX_LABEL_IDS + 1)
                .map(|index| PendingStatement {
                    property_id: format!("P{index}"),
                    values: vec![PendingStatementValue::Plain("value".to_owned())],
                })
                .collect(),
            wikipedia_sitelinks: Vec::new(),
            wikipedia_sitelinks_omitted: false,
            omissions: OmissionState::default(),
        };

        let (identifiers, truncated) = collect_label_ids(&pending);

        assert!(!truncated);
        assert_eq!(identifiers.len(), MAX_LABEL_IDS + 1);
        assert_eq!(identifiers.first().map(String::as_str), Some("P1"));
        let expected_last = format!("P{}", MAX_LABEL_IDS + 1);
        assert_eq!(
            identifiers.last().map(String::as_str),
            Some(expected_last.as_str())
        );
        assert_eq!(identifiers.chunks(MAX_LABEL_IDS).count(), 2);
    }

    #[test]
    fn invalid_item_id_is_rejected_before_transport() {
        let error = WikidataProvider::new()
            .load_entity_statements("../Q42")
            .expect_err("invalid item ID must fail");
        assert!(matches!(error, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn oversized_entity_response_is_rejected_before_label_lookup() {
        let server = MockServer::spawn(vec![json_response("200 OK", ENTITY_STATEMENTS_FIXTURE)]);
        let provider = WikidataProvider::with_statement_endpoints(
            server.base_url.join("w/api.php").expect("label API URL"),
            64,
            MAX_LABEL_RESPONSE_BYTES,
        );

        let error = provider
            .load_entity_statements("Q42")
            .expect_err("oversized response must fail");
        assert!(matches!(
            error,
            ProviderError::ResponseTooLarge { limit: 64 }
        ));
        let requests = server.finish();
        assert_eq!(requests.len(), 1);
        let request = server_url(&requests[0]);
        let pairs = request.query_pairs().collect::<BTreeMap<_, _>>();
        assert_eq!(request.path(), "/w/api.php");
        assert_eq!(
            pairs.get("props").map(AsRef::as_ref),
            Some("claims|sitelinks/urls")
        );
    }

    fn statement_value<'a>(result: &'a WikidataEntityStatements, property_id: &str) -> &'a str {
        result
            .statements
            .iter()
            .find(|statement| statement.property_id == property_id)
            .and_then(|statement| statement.values.first())
            .map(|value| value.display.as_str())
            .expect("fixture statement value")
    }

    fn raw_sitelink(site: &str, title: &str, url: &str) -> RawSitelink {
        RawSitelink {
            site: site.to_owned(),
            title: title.to_owned(),
            url: Some(url.to_owned()),
        }
    }

    fn server_url(target: &str) -> Url {
        Url::parse("http://127.0.0.1/")
            .expect("base URL")
            .join(target)
            .expect("request target URL")
    }

    struct MockServer {
        base_url: Url,
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
                            Ok((stream, _)) => break stream,
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
                base_url: Url::parse(&format!("http://{address}/")).expect("mock URL should parse"),
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
}
