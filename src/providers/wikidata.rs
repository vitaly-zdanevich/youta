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
const ENTITY_DATA_BASE: &str = "https://www.wikidata.org/wiki/Special:EntityData/";
const ENTITY_API_ENDPOINT: &str = "https://www.wikidata.org/w/api.php";
const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const MAX_RESULTS: usize = 20;
const MAX_ENTITY_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_LABEL_RESPONSE_BYTES: usize = 512 * 1024;
const MAX_STATEMENT_PROPERTIES: usize = 256;
const MAX_VALUES_PER_PROPERTY: usize = 128;
const MAX_STATEMENT_VALUES: usize = 1_024;
const MAX_LABEL_IDS: usize = 50;
const MAX_VALUE_BYTES: usize = 4 * 1024;

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

/// Human-facing statements loaded for one exact Wikidata item.
///
/// Claims are grouped by property while preserving the order of values within
/// each property. Qualifiers, references, statement IDs, ranks, numeric entity
/// IDs, hashes, and other Wikibase implementation metadata are deliberately
/// omitted.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WikidataEntityStatements {
    /// Validated Wikidata item identifier.
    pub item_id: String,
    /// Bounded property groups in stable property-ID order.
    pub statements: Vec<WikidataStatement>,
    /// Whether a property, value, label, or overlong display value was omitted.
    ///
    /// An oversized HTTP response remains an error because it cannot be safely
    /// parsed far enough to return a trustworthy partial result.
    pub truncated: bool,
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
}

/// Bounded client for the public Wikidata Query Service.
#[derive(Clone)]
pub struct WikidataProvider {
    agent: ureq::Agent,
    max_response_bytes: usize,
    entity_data_base: Url,
    entity_api_endpoint: Url,
    max_entity_response_bytes: usize,
    max_label_response_bytes: usize,
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
            entity_data_base: Url::parse(ENTITY_DATA_BASE)
                .expect("the compile-time Wikidata EntityData base URL is valid"),
            entity_api_endpoint: Url::parse(ENTITY_API_ENDPOINT)
                .expect("the compile-time Wikidata entity API URL is valid"),
            max_entity_response_bytes: MAX_ENTITY_RESPONSE_BYTES,
            max_label_response_bytes: MAX_LABEL_RESPONSE_BYTES,
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
    /// The first request uses Wikidata's HTTPS `Special:EntityData` JSON
    /// representation. One additional bounded `wbgetentities` request resolves
    /// as many labels as the anonymous API limit permits. Direct item-valued
    /// claims are prioritized over supporting entities and property labels, so
    /// useful linked-item labels are not starved by property-rich entities.
    /// Unresolved IDs remain visible rather than causing data loss.
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
        let entity_url = build_entity_data_url(&self.entity_data_base, item_id)?;
        let response: EntityDataResponse =
            get_bounded_json(&self.agent, &entity_url, self.max_entity_response_bytes)?;
        let mut pending = normalize_entity_claims(item_id, response)?;

        let (label_ids, labels_truncated) = collect_label_ids(&pending);
        pending.truncated |= labels_truncated;
        let (labels, label_values_truncated) = if label_ids.is_empty() {
            (BTreeMap::new(), false)
        } else {
            let label_url = build_label_url(&self.entity_api_endpoint, &label_ids)?;
            let response: EntityLabelResponse =
                get_bounded_json(&self.agent, &label_url, self.max_label_response_bytes)?;
            normalize_labels(&label_ids, response)?
        };
        pending.truncated |= label_values_truncated;
        Ok(render_entity_statements(pending, &labels))
    }

    #[cfg(test)]
    fn with_statement_endpoints(
        entity_data_base: Url,
        entity_api_endpoint: Url,
        max_entity_response_bytes: usize,
        max_label_response_bytes: usize,
    ) -> Self {
        Self {
            agent: provider_agent(DEFAULT_REQUEST_TIMEOUT),
            max_response_bytes: MAX_RESPONSE_BYTES,
            entity_data_base,
            entity_api_endpoint,
            max_entity_response_bytes,
            max_label_response_bytes,
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

fn build_entity_data_url(base: &Url, item_id: &str) -> Result<Url, ProviderError> {
    validate_item_id(item_id)?;
    base.join(&format!("{item_id}.json"))
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))
}

fn build_label_url(endpoint: &Url, entity_ids: &[String]) -> Result<Url, ProviderError> {
    if entity_ids.is_empty() || entity_ids.len() > MAX_LABEL_IDS {
        return Err(ProviderError::InvalidRequest(format!(
            "Wikidata label request must contain 1 to {MAX_LABEL_IDS} entity IDs"
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
        .append_pair("props", "labels")
        .append_pair("languages", "en")
        .append_pair("languagefallback", "1")
        .append_pair("ids", &entity_ids.join("|"));
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
    truncated: bool,
}

#[derive(Debug)]
struct PendingStatement {
    property_id: String,
    values: Vec<PendingStatementValue>,
}

#[derive(Debug)]
enum PendingStatementValue {
    Plain(String),
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
}

impl PendingStatementValue {
    /// Returns the directly linked entity ID, which is the most useful label
    /// for callers that expose item-valued claims as navigable links.
    fn direct_entity_id(&self) -> Option<&str> {
        match self {
            Self::Entity(entity_id) => Some(entity_id),
            Self::Plain(_)
            | Self::Quantity { .. }
            | Self::Time { .. }
            | Self::Coordinate { .. } => None,
        }
    }

    /// Returns an entity ID used to explain a composite scalar value.
    fn supporting_entity_id(&self) -> Option<&str> {
        match self {
            Self::Quantity { unit_id, .. } => unit_id.as_deref(),
            Self::Time { calendar_id, .. } => calendar_id.as_deref(),
            Self::Coordinate { globe_id, .. } => globe_id.as_deref(),
            Self::Plain(_) | Self::Entity(_) => None,
        }
    }

    fn render(&self, labels: &BTreeMap<String, String>) -> WikidataStatementValue {
        match self {
            Self::Plain(display) => WikidataStatementValue {
                display: display.clone(),
                item_id: None,
            },
            Self::Entity(entity_id) => WikidataStatementValue {
                display: labels
                    .get(entity_id)
                    .cloned()
                    .unwrap_or_else(|| entity_id.clone()),
                item_id: entity_id.starts_with('Q').then(|| entity_id.clone()),
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
                }
            }
        }
    }
}

fn normalize_entity_claims(
    item_id: &str,
    mut response: EntityDataResponse,
) -> Result<PendingEntityStatements, ProviderError> {
    if response.entities.len() != 1 {
        return Err(ProviderError::InvalidResponse(
            "Wikidata EntityData response must contain exactly one entity".to_owned(),
        ));
    }
    let entity = response.entities.remove(item_id).ok_or_else(|| {
        ProviderError::InvalidResponse(
            "Wikidata EntityData response does not match the requested item".to_owned(),
        )
    })?;
    let property_count = entity.claims.len();
    let mut statements = Vec::with_capacity(property_count.min(MAX_STATEMENT_PROPERTIES));
    let mut total_values = 0usize;
    let mut truncated = property_count > MAX_STATEMENT_PROPERTIES;

    for (property_id, claims) in entity.claims.into_iter().take(MAX_STATEMENT_PROPERTIES) {
        validate_property_id(&property_id)?;
        let claim_count = claims.len();
        if claim_count > MAX_VALUES_PER_PROPERTY {
            truncated = true;
        }
        let mut values = Vec::with_capacity(claim_count.min(MAX_VALUES_PER_PROPERTY));
        for claim in claims.into_iter().take(MAX_VALUES_PER_PROPERTY) {
            if total_values >= MAX_STATEMENT_VALUES {
                truncated = true;
                break;
            }
            if claim.mainsnak.property != property_id {
                return Err(ProviderError::InvalidResponse(
                    "Wikidata claim property does not match its containing group".to_owned(),
                ));
            }
            let (value, value_truncated) = normalize_snak(claim.mainsnak)?;
            truncated |= value_truncated;
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
            truncated |= statements.len() < property_count;
            break;
        }
    }

    Ok(PendingEntityStatements {
        item_id: item_id.to_owned(),
        statements,
        truncated,
    })
}

fn normalize_snak(
    snak: RawMainSnak,
) -> Result<(Option<PendingStatementValue>, bool), ProviderError> {
    match snak.snak_type.as_str() {
        "novalue" => Ok((
            Some(PendingStatementValue::Plain("no value".to_owned())),
            false,
        )),
        "somevalue" => Ok((
            Some(PendingStatementValue::Plain("unknown value".to_owned())),
            false,
        )),
        "value" => {
            let data_value = snak.data_value.ok_or_else(|| {
                ProviderError::InvalidResponse(
                    "Wikidata value claim is missing its data value".to_owned(),
                )
            })?;
            normalize_data_value(data_value)
        }
        _ => Err(ProviderError::InvalidResponse(
            "Wikidata claim contains an unknown snak type".to_owned(),
        )),
    }
}

fn normalize_data_value(
    data_value: RawDataValue,
) -> Result<(Option<PendingStatementValue>, bool), ProviderError> {
    match data_value.value_type.as_str() {
        "wikibase-entityid" => {
            let raw: RawEntityId = serde_json::from_value(data_value.value)
                .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
            if !valid_entity_reference_id(&raw.id) {
                return Err(ProviderError::InvalidResponse(
                    "Wikidata claim contains an invalid entity identifier".to_owned(),
                ));
            }
            Ok((Some(PendingStatementValue::Entity(raw.id)), false))
        }
        "monolingualtext" => {
            let raw: RawMonolingualText = serde_json::from_value(data_value.value)
                .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
            let language = normalize_language_code(&raw.language)?;
            let combined = format!("{} [{language}]", raw.text);
            bounded_display(&combined)
                .map(|(display, truncated)| (display.map(PendingStatementValue::Plain), truncated))
        }
        "quantity" => {
            let raw: RawQuantity = serde_json::from_value(data_value.value)
                .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
            let unit_id = unit_entity_id(&raw.unit)?;
            let (amount, amount_truncated) = required_bounded_text(&raw.amount, "quantity amount")?;
            let (lower_bound, lower_truncated) = optional_bounded_text(raw.lower_bound.as_deref())?;
            let (upper_bound, upper_truncated) = optional_bounded_text(raw.upper_bound.as_deref())?;
            Ok((
                Some(PendingStatementValue::Quantity {
                    amount,
                    lower_bound,
                    upper_bound,
                    unit_id,
                }),
                amount_truncated || lower_truncated || upper_truncated,
            ))
        }
        "time" => {
            let raw: RawTime = serde_json::from_value(data_value.value)
                .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
            let calendar_id = unit_entity_id(&raw.calendar_model)?;
            let time = human_time(&raw.time, raw.precision)?;
            let (time, truncated) = required_bounded_text(&time, "time value")?;
            Ok((
                Some(PendingStatementValue::Time { time, calendar_id }),
                truncated,
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
                false,
            ))
        }
        _ => bounded_plain_value(data_value.value),
    }
}

fn bounded_plain_value(
    value: Value,
) -> Result<(Option<PendingStatementValue>, bool), ProviderError> {
    let text = match value {
        Value::String(text) => text,
        Value::Number(number) => number.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Null | Value::Array(_) | Value::Object(_) => return Ok((None, true)),
    };
    bounded_display(&text)
        .map(|(display, truncated)| (display.map(PendingStatementValue::Plain), truncated))
}

fn bounded_display(text: &str) -> Result<(Option<String>, bool), ProviderError> {
    let normalized = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_owned();
    if normalized.is_empty() {
        return Ok((None, true));
    }
    if normalized.len() <= MAX_VALUE_BYTES {
        return Ok((Some(normalized), false));
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
    Ok((Some(bounded), true))
}

fn required_bounded_text(text: &str, field: &str) -> Result<(String, bool), ProviderError> {
    let (text, truncated) = bounded_display(text)?;
    text.map(|text| (text, truncated))
        .ok_or_else(|| ProviderError::InvalidResponse(format!("Wikidata {field} cannot be empty")))
}

fn optional_bounded_text(text: Option<&str>) -> Result<(Option<String>, bool), ProviderError> {
    text.map_or(Ok((None, false)), bounded_display)
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

/// Selects one bounded label batch in descending order of UI value.
///
/// Direct entity-valued claims are collected across every statement before
/// supporting unit/calendar/globe entities and property IDs. This ordering
/// keeps late linked values readable without increasing the request count or
/// exceeding Wikidata's anonymous 50-ID batch limit.
fn collect_label_ids(pending: &PendingEntityStatements) -> (Vec<String>, bool) {
    let mut identifiers = Vec::with_capacity(MAX_LABEL_IDS);
    let mut seen = BTreeSet::new();
    let mut truncated = false;
    let candidates = pending
        .statements
        .iter()
        .flat_map(|statement| statement.values.iter())
        .filter_map(PendingStatementValue::direct_entity_id)
        .filter(|entity_id| is_label_entity_id(entity_id))
        .chain(
            pending
                .statements
                .iter()
                .flat_map(|statement| statement.values.iter())
                .filter_map(PendingStatementValue::supporting_entity_id)
                .filter(|entity_id| is_label_entity_id(entity_id)),
        );
    let candidates = candidates.chain(
        pending
            .statements
            .iter()
            .map(|statement| statement.property_id.as_str()),
    );
    for entity_id in candidates {
        if !seen.insert(entity_id.to_owned()) {
            continue;
        }
        if identifiers.len() >= MAX_LABEL_IDS {
            truncated = true;
            continue;
        }
        identifiers.push(entity_id.to_owned());
    }
    (identifiers, truncated)
}

fn normalize_labels(
    requested_ids: &[String],
    response: EntityLabelResponse,
) -> Result<(BTreeMap<String, String>, bool), ProviderError> {
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
    let mut truncated = false;
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
            let (label, label_truncated) = bounded_display(&label)?;
            truncated |= label_truncated;
            if let Some(label) = label {
                labels.insert(entity_id, label);
            }
        }
    }
    Ok((labels, truncated))
}

fn render_entity_statements(
    pending: PendingEntityStatements,
    labels: &BTreeMap<String, String>,
) -> WikidataEntityStatements {
    WikidataEntityStatements {
        item_id: pending.item_id,
        statements: pending
            .statements
            .into_iter()
            .map(|statement| WikidataStatement {
                property_label: labels
                    .get(&statement.property_id)
                    .cloned()
                    .unwrap_or_else(|| statement.property_id.clone()),
                property_id: statement.property_id,
                values: statement
                    .values
                    .iter()
                    .map(|value| value.render(labels))
                    .collect(),
            })
            .collect(),
        truncated: pending.truncated,
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
    if !valid_prefixed_decimal_id(item_id, 'Q') {
        return Err(ProviderError::InvalidResponse(
            "Wikidata result contains an invalid Q identifier".to_owned(),
        ));
    }
    Ok(item_id.to_owned())
}

#[derive(Debug, Deserialize)]
struct EntityDataResponse {
    entities: BTreeMap<String, RawEntityData>,
}

#[derive(Debug, Deserialize)]
struct RawEntityData {
    #[serde(default)]
    claims: BTreeMap<String, Vec<RawClaim>>,
}

#[derive(Debug, Deserialize)]
struct RawClaim {
    mainsnak: RawMainSnak,
}

#[derive(Debug, Deserialize)]
struct RawMainSnak {
    #[serde(rename = "snaktype")]
    snak_type: String,
    property: String,
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

    #[test]
    fn entity_statements_resolve_labels_and_omit_internal_metadata() {
        let server = MockServer::spawn(vec![
            json_response("200 OK", ENTITY_STATEMENTS_FIXTURE),
            json_response("200 OK", ENTITY_LABELS_FIXTURE),
        ]);
        let provider = WikidataProvider::with_statement_endpoints(
            server
                .base_url
                .join("wiki/Special:EntityData/")
                .expect("entity-data URL"),
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
        assert_eq!(requests[0], "/wiki/Special:EntityData/Q42.json");
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
                },
                WikidataStatementValue {
                    display: "person".to_owned(),
                    item_id: Some("Q215627".to_owned()),
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
            server
                .base_url
                .join("wiki/Special:EntityData/")
                .expect("entity-data URL"),
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
        ]);
        let provider = WikidataProvider::with_statement_endpoints(
            server
                .base_url
                .join("wiki/Special:EntityData/")
                .expect("entity-data URL"),
            server.base_url.join("w/api.php").expect("label API URL"),
            MAX_ENTITY_RESPONSE_BYTES,
            MAX_LABEL_RESPONSE_BYTES,
        );

        let result = provider
            .load_entity_statements("Q42")
            .expect("property-rich fixture should load");
        let requests = server.finish();

        assert_eq!(requests.len(), 2);
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
        assert!(result.truncated);

        let late_values = result
            .statements
            .iter()
            .find(|statement| statement.property_id == "P999")
            .map(|statement| &statement.values)
            .expect("late direct-item statement");
        assert_eq!(
            late_values,
            &[
                WikidataStatementValue {
                    display: "Soviet Union".to_owned(),
                    item_id: Some("Q15180".to_owned()),
                },
                WikidataStatementValue {
                    display: "Ukraine".to_owned(),
                    item_id: Some("Q212".to_owned()),
                },
                WikidataStatementValue {
                    display: "Q999".to_owned(),
                    item_id: Some("Q999".to_owned()),
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
                    data_value: Some(RawDataValue {
                        value_type: "string".to_owned(),
                        value: Value::String(format!("value {index}")),
                    }),
                },
            })
            .collect();
        let response = EntityDataResponse {
            entities: BTreeMap::from([(
                "Q42".to_owned(),
                RawEntityData {
                    claims: BTreeMap::from([("P1".to_owned(), claims)]),
                },
            )]),
        };

        let pending =
            normalize_entity_claims("Q42", response).expect("bounded statements should normalize");

        assert!(pending.truncated);
        assert_eq!(pending.statements.len(), 1);
        assert_eq!(pending.statements[0].values.len(), MAX_VALUES_PER_PROPERTY);
    }

    #[test]
    fn label_request_is_capped_and_marked_truncated() {
        let pending = PendingEntityStatements {
            item_id: "Q42".to_owned(),
            statements: (1..=MAX_LABEL_IDS + 1)
                .map(|index| PendingStatement {
                    property_id: format!("P{index}"),
                    values: vec![PendingStatementValue::Plain("value".to_owned())],
                })
                .collect(),
            truncated: false,
        };

        let (identifiers, truncated) = collect_label_ids(&pending);

        assert!(truncated);
        assert_eq!(identifiers.len(), MAX_LABEL_IDS);
        assert_eq!(identifiers.first().map(String::as_str), Some("P1"));
        assert_eq!(
            identifiers.last().map(String::as_str),
            Some(&format!("P{MAX_LABEL_IDS}")).map(String::as_str)
        );
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
            server
                .base_url
                .join("wiki/Special:EntityData/")
                .expect("entity-data URL"),
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
        assert_eq!(server.finish(), vec!["/wiki/Special:EntityData/Q42.json"]);
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
