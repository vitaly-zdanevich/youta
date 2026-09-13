//! Bounded, credential-free Internet Archive audio search and item metadata.
//!
//! Uses the public [search](https://archive.org/advancedsearch.php) and
//! [metadata](https://archive.org/developers/md-read.html) endpoints. File URLs
//! are constructed locally; server names and download links in remote metadata
//! are never trusted. Restricted items are not exposed for playback.

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, NaiveDateTime};
use html5gum::{DefaultEmitter, Token, Tokenizer};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::domain::remote_url_has_non_public_host;

use super::{
    DEFAULT_REQUEST_TIMEOUT, MAX_VIDEO_COMMENT_AUTHOR_BYTES, MAX_VIDEO_COMMENT_AUTHOR_CHARS,
    MAX_VIDEO_COMMENT_TEXT_BYTES, MAX_VIDEO_COMMENT_TEXT_CHARS, MAX_VIDEO_COMMENTS, ProviderError,
    VideoComment,
};

const MAX_SEARCH_JSON_BYTES: usize = 4 * 1024 * 1024;
/// Audio collections include per-track waveform and spectrogram metadata.
const MAX_ITEM_JSON_BYTES: usize = 8 * 1024 * 1024;
const MAX_HTML_BYTES: usize = 2 * 1024 * 1024;
const MAX_QUERY_BYTES: usize = 512;
const MAX_SEARCH_RESULTS: usize = 100;
/// Bounds all metadata file records, including non-playable derivatives.
const MAX_FILES: usize = 16_384;
/// Independently bounds the actual track list retained by the UI and its cache.
const MAX_TRACKS: usize = 4096;
/// A pathological encoding family fails explicitly rather than hiding download choices.
const MAX_DOWNLOAD_VARIANTS_PER_TRACK: usize = 64;
const MAX_REVIEWS: usize = 4096;
const MAX_LABEL_BYTES: usize = 4096;
const MAX_DESCRIPTION_BYTES: usize = 64 * 1024;
const MAX_LIST_VALUES: usize = 128;
const MAX_FILENAME_BYTES: usize = 2048;
const MAX_HTML_TOKENS: usize = 100_000;
const MAX_DERIVATIVE_DEPTH: usize = 32;
/// Match the artwork transport/decode ceilings without enabling a renderer.
const MAX_COVER_BYTES: u64 = 4 * 1024 * 1024;
const MAX_COVER_DIMENSION: u64 = 4096;
const MAX_COVER_DECODED_BYTES: u64 = 32 * 1024 * 1024;

/// One bounded, one-based Internet Archive audio search.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArchiveOrgSearchRequest {
    /// Plain search text; an empty query browses recent public audio.
    pub query: String,
    /// One-based page number.
    pub page: u32,
    /// Maximum number of results, between one and one hundred.
    pub limit: usize,
}

/// A normalized public profile or collection link.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArchiveOrgLink {
    /// Public display name, or the collection identifier when no title is known.
    pub name: String,
    /// Canonical HTTPS Archive.org page.
    pub url: Url,
}

/// One public audio item, which can contain several playable tracks.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArchiveOrgItem {
    /// Stable Internet Archive identifier.
    pub identifier: String,
    /// Human-readable item title.
    pub title: String,
    /// Credited artist, author, or other creator; not necessarily the uploader.
    pub creator: Option<String>,
    /// Bounded, terminal-safe plain-text description.
    pub description: Option<String>,
    /// Original public item page.
    pub webpage_url: Url,
    /// Canonical original cover, a representative first-track waveform, or the item image service.
    pub artwork_url: Option<Url>,
    /// Recording or publication date, when supplied, as a Unix timestamp.
    pub published_at: Option<i64>,
    /// First public upload date, distinct from the recording/publication date.
    pub uploaded_at: Option<i64>,
    /// Public uploader identity only when Archive.org supplies a profile link.
    pub uploader: Option<ArchiveOrgLink>,
    /// Public collections containing the item, in metadata order.
    pub collections: Vec<ArchiveOrgLink>,
    /// Topic labels supplied by the item's subject metadata.
    pub topics: Vec<String>,
    /// Language names or codes supplied by the item metadata.
    pub languages: Vec<String>,
    /// Public license or rights label, including non-URL rights statements.
    pub license: Option<String>,
    /// Validated public HTTPS license URL, if explicitly supplied by the item.
    pub license_url: Option<Url>,
    /// Total size of all item files, not only the selected audio encodings.
    pub size_bytes: Option<u64>,
    /// Archive.org's combined views/downloads counter, when supplied.
    pub download_count: Option<u64>,
    /// Public favourites count; absence is unknown, not zero.
    pub favorite_count: Option<u64>,
    /// Total public reviews, before limiting the comments returned to the UI.
    pub review_count: Option<u64>,
}

/// One bounded page of public audio search results.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArchiveOrgSearchPage {
    /// Valid, unrestricted audio items in server order.
    pub items: Vec<ArchiveOrgItem>,
    /// One-based requested page.
    pub page: u32,
    /// Next page only when the raw API page and total indicate more results.
    pub next_page: Option<u32>,
    /// Search API's unmodified matching-item total.
    pub total: u64,
}

/// Provenance explicitly reported by Internet Archive file metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArchiveOrgFileProvenance {
    /// An uploaded original, not a generated derivative.
    Original,
    /// A file generated by Archive.org from another uploaded encoding.
    Derivative,
    /// Provenance was absent, unrecognized, or internally inconsistent.
    Unknown,
}

/// One existing file available unchanged within an accepted track's derivative family.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArchiveOrgDownloadVariant {
    /// Original case-sensitive file path within the item.
    pub filename: String,
    /// Canonical HTTPS file URL, never a server-provided arbitrary URL.
    pub download_url: Url,
    /// Bounded public format label, falling back to the filename extension.
    pub format: String,
    /// Size of this exact existing file, if reported.
    pub size_bytes: Option<u64>,
    /// Explicit original/derivative identity; MP3 alone does not imply a derivative.
    pub provenance: ArchiveOrgFileProvenance,
    /// Whether this existing file is a video container rather than audio-only.
    pub is_video: bool,
}

/// One preferred playable encoding of an item track.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArchiveOrgTrack {
    /// Original case-sensitive file path inside the Archive.org item.
    pub filename: String,
    /// Human-readable track title, falling back to the filename.
    pub title: String,
    /// Canonical file URL, also suitable for a user-requested download.
    pub download_url: Url,
    /// Whole seconds, when a valid duration is supplied.
    pub duration_seconds: Option<u64>,
    /// Size of this selected file, when supplied.
    pub size_bytes: Option<u64>,
    /// Native-size waveform of this accepted audio family when no real cover exists.
    /// Missing fields in older serialized snapshots retain the item-image fallback.
    #[serde(default)]
    pub waveform_url: Option<Url>,
    /// Existing original and derivative files, independent of playback preference.
    /// Older snapshots must refresh metadata before offering a variant choice.
    #[serde(default)]
    pub download_variants: Vec<ArchiveOrgDownloadVariant>,
}

/// Complete bounded item metadata and selected tracks.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArchiveOrgItemDetails {
    /// Public item metadata, optionally enriched from its original page.
    pub item: ArchiveOrgItem,
    /// One encoding per original track, ordered by track number and filename.
    pub tracks: Vec<ArchiveOrgTrack>,
    /// At most twenty public reviews represented as comments; stars stay text.
    pub comments: Vec<VideoComment>,
}

/// Reads one canonical public Archive.org URL with a hard response limit.
pub trait ArchiveOrgTransport: Send + Sync {
    /// Returns response bytes without following redirects or authenticating.
    ///
    /// # Errors
    ///
    /// Reports network failures, unsuccessful status codes, and oversized bodies.
    fn fetch(&self, url: &Url, max_bytes: usize) -> Result<Vec<u8>, ProviderError>;
}

/// Cloneable blocking client; the application runs provider work off its UI loop.
#[derive(Clone)]
pub struct ArchiveOrgClient {
    transport: Arc<dyn ArchiveOrgTransport>,
}

impl fmt::Debug for ArchiveOrgClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArchiveOrgClient")
            .finish_non_exhaustive()
    }
}

impl Default for ArchiveOrgClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ArchiveOrgClient {
    /// Creates a credential-free client with bounded timeouts and responses.
    #[must_use]
    pub fn new() -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(DEFAULT_REQUEST_TIMEOUT))
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
        Self::with_transport(Arc::new(UreqArchiveOrgTransport { agent }))
    }

    /// Supplies a transport for deterministic tests or host application adapters.
    #[must_use]
    pub fn with_transport(transport: Arc<dyn ArchiveOrgTransport>) -> Self {
        Self { transport }
    }

    /// Searches public audio and live-music items without following remote URLs.
    ///
    /// # Errors
    ///
    /// Reports invalid pagination/text, network errors, or malformed API records.
    pub fn search(
        &self,
        request: &ArchiveOrgSearchRequest,
    ) -> Result<ArchiveOrgSearchPage, ProviderError> {
        request.validate()?;
        let mut url = archive_url(&["advancedsearch.php"])?;
        let terms = request
            .query
            .split_whitespace()
            .map(|term| {
                let escaped = term.chars().fold(String::new(), |mut text, character| {
                    if "+-&|!(){}[]^\"~*?:\\/".contains(character) {
                        text.push('\\');
                    }
                    text.push(character);
                    text
                });
                format!("\"{escaped}\"")
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        let mut query = "(mediatype:audio OR mediatype:etree) AND -access-restricted-item:true AND -access-restricted:true".to_owned();
        if !terms.is_empty() {
            query.push_str(" AND (");
            query.push_str(&terms);
            query.push(')');
        }
        {
            let mut pairs = url.query_pairs_mut();
            pairs
                .append_pair("q", &query)
                .append_pair("output", "json")
                .append_pair("rows", &request.limit.to_string())
                .append_pair("page", &request.page.to_string());
            for field in [
                "identifier",
                "title",
                "creator",
                "description",
                "mediatype",
                "date",
                "publicdate",
                "addeddate",
                "downloads",
                "num_favorites",
                "num_reviews",
                "item_size",
                "subject",
                "language",
                "collection",
                "licenseurl",
                "rights",
                "access-restricted-item",
                "access-restricted",
            ] {
                pairs.append_pair("fl[]", field);
            }
            if terms.is_empty() {
                pairs.append_pair("sort[]", "publicdate desc");
            }
        }
        let value = self.fetch_json(&url, MAX_SEARCH_JSON_BYTES)?;
        let response = value
            .get("response")
            .filter(|value| value.is_object())
            .ok_or_else(|| invalid_response("missing search response"))?;
        let total = number(&response["numFound"])
            .ok_or_else(|| invalid_response("invalid search total"))?;
        let docs = response["docs"]
            .as_array()
            .ok_or_else(|| invalid_response("missing search documents"))?;
        if docs.len() > request.limit {
            return Err(invalid_response("too many search documents"));
        }
        let offset = u64::from(request.page - 1) * u64::try_from(request.limit).unwrap_or(u64::MAX);
        if number(&response["start"]).is_some_and(|start| start != offset) {
            return Err(invalid_response("search response has an unexpected offset"));
        }
        let items = docs
            .iter()
            .filter_map(|doc| normalize_item(doc).ok())
            .collect();
        let next_page = (docs.len() == request.limit
            && offset.saturating_add(docs.len() as u64) < total)
            .then(|| request.page.checked_add(1))
            .flatten();
        Ok(ArchiveOrgSearchPage {
            items,
            page: request.page,
            next_page,
            total,
        })
    }

    /// Loads public tracks, item metadata, and up to twenty reviews.
    ///
    /// Optional public-page enrichment must not make an otherwise playable item
    /// fail when the page is unavailable. No credentials or restricted file APIs
    /// are used.
    ///
    /// # Errors
    ///
    /// Reports invalid identifiers, inaccessible/restricted items, network errors,
    /// malformed metadata, and hard response or collection bounds.
    pub fn item_details(&self, identifier: &str) -> Result<ArchiveOrgItemDetails, ProviderError> {
        if !valid_identifier(identifier) {
            return Err(ProviderError::InvalidRequest(
                "invalid Archive.org item identifier".into(),
            ));
        }
        let value = self.fetch_json(
            &archive_url(&["metadata", identifier])?,
            MAX_ITEM_JSON_BYTES,
        )?;
        if restricted(&value) {
            return Err(invalid_response(
                "item is restricted or unavailable for download",
            ));
        }
        let metadata = value
            .get("metadata")
            .filter(|value| value.is_object())
            .ok_or_else(|| invalid_response("missing item metadata"))?;
        let mut item = normalize_item(metadata)?;
        if item.identifier != identifier {
            return Err(invalid_response("item identifier does not match request"));
        }
        let files = value["files"]
            .as_array()
            .ok_or_else(|| invalid_response("missing item files"))?;
        if files.len() > MAX_FILES {
            return Err(invalid_response("too many item files"));
        }
        let mut tracks = normalize_tracks(identifier, files)?;
        repair_corroborated_short_track_titles(&mut tracks, &item);
        if let Some(cover) = original_cover_url(identifier, files) {
            // Real cover artwork always wins, including in selected-track Details.
            for track in &mut tracks {
                track.waveform_url = None;
            }
            item.artwork_url = Some(cover);
        } else if let Some(waveform) = tracks.first().and_then(|track| track.waveform_url.clone()) {
            // This is representative track artwork, not a claim about the tile's provenance.
            item.artwork_url = Some(waveform);
        }
        item.size_bytes = number(&value["item_size"]).or(item.size_bytes).or_else(|| {
            (!files.is_empty())
                .then(|| {
                    files
                        .iter()
                        .try_fold(0_u64, |sum, file| sum.checked_add(number(&file["size"])?))
                })
                .flatten()
        });
        let comments = match value.get("reviews") {
            Some(Value::Array(reviews)) => {
                if reviews.len() > MAX_REVIEWS {
                    return Err(invalid_response("too many item reviews"));
                }
                item.review_count = Some(reviews.len() as u64);
                normalize_reviews(identifier, reviews)
            }
            None | Some(Value::Null) => Vec::new(),
            _ => return Err(invalid_response("invalid item reviews")),
        };
        // Enrichment is optional: web-page availability must not gate public audio.
        if let Ok(page) = self.fetch_bounded(&item.webpage_url, MAX_HTML_BYTES) {
            enrich_from_page(&mut item, &page);
        }
        Ok(ArchiveOrgItemDetails {
            item,
            tracks,
            comments,
        })
    }

    /// Enforces the response limit even when an injected transport ignores it.
    fn fetch_bounded(&self, url: &Url, limit: usize) -> Result<Vec<u8>, ProviderError> {
        let bytes = self.transport.fetch(url, limit)?;
        if bytes.len() > limit {
            return Err(ProviderError::ResponseTooLarge { limit });
        }
        Ok(bytes)
    }

    /// Parses only size-bounded JSON; API error payloads are not item records.
    fn fetch_json(&self, url: &Url, limit: usize) -> Result<Value, ProviderError> {
        let bytes = self.fetch_bounded(url, limit)?;
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| invalid_response("malformed JSON"))?;
        if !value.is_object() || value.get("error").is_some_and(|error| !error.is_null()) {
            return Err(invalid_response(
                "API item or search response is unavailable",
            ));
        }
        Ok(value)
    }
}

impl ArchiveOrgSearchRequest {
    /// Checks query/pagination bounds before making a network request.
    ///
    /// # Errors
    ///
    /// Returns an invalid-request error for controls, oversized text, zero-based
    /// pages, or limits outside one through one hundred.
    pub fn validate(&self) -> Result<(), ProviderError> {
        if self.page == 0
            || !(1..=MAX_SEARCH_RESULTS).contains(&self.limit)
            || self.query.len() > MAX_QUERY_BYTES
            || self.query.chars().any(char::is_control)
        {
            return Err(ProviderError::InvalidRequest("Archive.org requires a query of at most 512 bytes, a positive page, and a limit between 1 and 100".into()));
        }
        Ok(())
    }
}

/// Production transport never follows redirects to unvalidated hosts.
struct UreqArchiveOrgTransport {
    agent: ureq::Agent,
}

impl ArchiveOrgTransport for UreqArchiveOrgTransport {
    fn fetch(&self, url: &Url, max_bytes: usize) -> Result<Vec<u8>, ProviderError> {
        if url.scheme() != "https"
            || url.host_str() != Some("archive.org")
            || url.port().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(ProviderError::InvalidRequest(
                "Archive.org transport accepts canonical HTTPS URLs only".into(),
            ));
        }
        let mut response = self
            .agent
            .get(url.as_str())
            .header("Accept", "application/json, text/html;q=0.9")
            .call()
            .map_err(|error| match error {
                ureq::Error::StatusCode(code) => ProviderError::HttpStatus(code),
                other => ProviderError::Transport(other.to_string()),
            })?;
        if !(200..300).contains(&response.status().as_u16()) {
            return Err(ProviderError::HttpStatus(response.status().as_u16()));
        }
        if response
            .body()
            .content_length()
            .is_some_and(|length| length > max_bytes as u64)
        {
            return Err(ProviderError::ResponseTooLarge { limit: max_bytes });
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit(max_bytes.saturating_add(1) as u64)
            .read_to_vec()
            .map_err(|error| match error {
                ureq::Error::BodyExceedsLimit(_) => {
                    ProviderError::ResponseTooLarge { limit: max_bytes }
                }
                other => ProviderError::Transport(other.to_string()),
            })?;
        if bytes.len() > max_bytes {
            return Err(ProviderError::ResponseTooLarge { limit: max_bytes });
        }
        Ok(bytes)
    }
}

/// Creates fixed-origin URLs by percent-encoding each individual path segment.
fn archive_url(segments: &[&str]) -> Result<Url, ProviderError> {
    let mut url = Url::parse("https://archive.org/")
        .map_err(|_| invalid_response("invalid built-in Archive.org URL"))?;
    url.path_segments_mut()
        .map_err(|()| invalid_response("invalid built-in URL path"))?
        .clear()
        .extend(segments.iter().copied());
    Ok(url)
}

/// Ordinary item identifiers are bounded ASCII, not URLs or filesystem paths.
fn valid_identifier(identifier: &str) -> bool {
    !identifier.is_empty()
        && identifier.len() <= 100
        && (identifier.as_bytes()[0].is_ascii_alphanumeric() || identifier.as_bytes()[0] == b'@')
        && identifier
            .bytes()
            .skip(1)
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

/// Optional public user identities carry an explicit at-sign prefix from the API.
fn profile_url(itemname: &str) -> Option<Url> {
    let name = itemname.strip_prefix('@')?;
    if itemname.len() > 100 || !valid_identifier(name) || name.starts_with('@') {
        return None;
    }
    archive_url(&["details", itemname]).ok()
}

/// Treats unknown nonempty restriction flags conservatively as restricted.
fn restricted(value: &Value) -> bool {
    [
        "is_dark",
        "nodownload",
        "private",
        "access-restricted-item",
        "access-restricted",
    ]
    .iter()
    .any(|key| flag_enabled(&value[*key]))
}

/// Handles the API's boolean, integer, string, and repeated flag encodings.
fn flag_enabled(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_u64() != Some(0),
        Value::String(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no"
        ),
        Value::Array(values) => values.iter().any(flag_enabled),
        Value::Object(_) => true,
    }
}

/// Parses repeated or nullable metadata without accepting unbounded labels.
fn values(value: &Value, limit: usize) -> Vec<&str> {
    let candidates = match value {
        Value::String(text) => vec![text.as_str()],
        Value::Array(values) if values.len() <= MAX_LIST_VALUES => {
            values.iter().filter_map(Value::as_str).collect()
        }
        _ => return Vec::new(),
    };
    let total = candidates
        .iter()
        .try_fold(0_usize, |sum, text| sum.checked_add(text.len()));
    if total.is_none_or(|total| total > limit) {
        return Vec::new();
    }
    candidates
}

/// Converts bounded HTML metadata to a nonempty plain-text label or paragraph.
fn text_value(value: &Value, limit: usize, multiline: bool) -> Option<String> {
    let text = values(value, limit)
        .iter()
        .map(|text| html_text(text, multiline))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(if multiline { "\n" } else { "; " });
    (!text.is_empty() && text.len() <= limit).then_some(text)
}

/// Splits semicolon-separated subjects while keeping multi-word labels intact.
fn label_list(value: &Value) -> Vec<String> {
    let mut labels = Vec::new();
    for raw in values(value, MAX_DESCRIPTION_BYTES) {
        let plain = html_text(raw, false);
        for part in plain.split(';') {
            let label = part.trim().to_owned();
            if !label.is_empty() && label.len() <= MAX_LABEL_BYTES && !labels.contains(&label) {
                labels.push(label);
                if labels.len() == MAX_LIST_VALUES {
                    return labels;
                }
            }
        }
    }
    labels
}

/// Normalizes shared search/metadata fields and constructs canonical item links.
fn normalize_item(metadata: &Value) -> Result<ArchiveOrgItem, ProviderError> {
    if !metadata.is_object() || restricted(metadata) {
        return Err(invalid_response("item is restricted or malformed"));
    }
    let identifier = metadata["identifier"]
        .as_str()
        .filter(|identifier| valid_identifier(identifier))
        .ok_or_else(|| invalid_response("invalid item identifier"))?;
    let media = values(&metadata["mediatype"], MAX_LABEL_BYTES);
    if (identifier.starts_with('@') || !media.is_empty())
        && !media
            .iter()
            .any(|value| matches!(*value, "audio" | "etree"))
    {
        return Err(invalid_response("item is not audio or live music"));
    }
    let collections = values(&metadata["collection"], MAX_DESCRIPTION_BYTES)
        .into_iter()
        .filter(|identifier| valid_identifier(identifier))
        .filter_map(|name| {
            archive_url(&["details", name])
                .ok()
                .map(|url| ArchiveOrgLink {
                    name: name.into(),
                    url,
                })
        })
        .collect();
    let license_url = values(&metadata["licenseurl"], MAX_LABEL_BYTES)
        .into_iter()
        .chain(values(&metadata["license"], MAX_LABEL_BYTES))
        .find_map(public_license_url);
    let license = text_value(&metadata["rights"], MAX_LABEL_BYTES, false)
        .or_else(|| text_value(&metadata["license"], MAX_LABEL_BYTES, false))
        .or_else(|| license_url.as_ref().map(license_label));
    Ok(ArchiveOrgItem {
        identifier: identifier.into(),
        title: text_value(&metadata["title"], MAX_LABEL_BYTES, false)
            .unwrap_or_else(|| identifier.into()),
        creator: text_value(&metadata["creator"], MAX_LABEL_BYTES, false),
        description: text_value(&metadata["description"], MAX_DESCRIPTION_BYTES, true),
        webpage_url: archive_url(&["details", identifier])?,
        artwork_url: Some(archive_url(&["services", "img", identifier])?),
        published_at: timestamp(&metadata["date"]),
        uploaded_at: timestamp(&metadata["publicdate"])
            .or_else(|| timestamp(&metadata["addeddate"])),
        uploader: None,
        collections,
        topics: label_list(&metadata["subject"]),
        languages: label_list(&metadata["language"]),
        license,
        license_url,
        size_bytes: number(&metadata["item_size"]),
        download_count: number(&metadata["downloads"]),
        favorite_count: number(&metadata["num_favorites"]),
        review_count: number(&metadata["num_reviews"]),
    })
}

/// Selects a bounded original cover, never a waveform, scan, or generated tile.
///
/// Search results retain the lightweight image service. Loaded item metadata can
/// identify a full-resolution cover by a conventional filename or the item
/// tile's explicit original reference. Ambiguous images retain that fallback.
/// Metadata limits only reject unsuitable candidates early; the artwork worker
/// still enforces its own byte, dimension, allocation, DNS, and redirect policy.
fn original_cover_url(identifier: &str, files: &[Value]) -> Option<Url> {
    let tile_original = files
        .iter()
        .filter(|file| !restricted(file))
        .filter(|file| {
            file["name"].as_str() == Some("__ia_thumb.jpg")
                || file["format"].as_str() == Some("Item Tile")
        })
        .filter_map(|file| file["original"].as_str())
        .filter(|name| valid_filename(name))
        .min();
    let (_, _, _, filename) = files
        .iter()
        .filter_map(|file| {
            let filename = file["name"].as_str().filter(|name| valid_filename(name))?;
            if restricted(file)
                || file["source"].as_str() != Some("original")
                || file["original"]
                    .as_str()
                    .is_some_and(|name| !name.is_empty())
            {
                return None;
            }
            let (stem, extension) = filename.rsplit_once('.')?;
            if !matches!(
                extension.to_ascii_lowercase().as_str(),
                "jpg" | "jpeg" | "png" | "webp"
            ) || file["format"].as_str().is_some_and(|format| {
                !["JPEG", "PNG", "WebP"]
                    .iter()
                    .any(|allowed| format.eq_ignore_ascii_case(allowed))
            }) {
                return None;
            }
            let stem = stem.to_ascii_lowercase();
            let words: Vec<_> = stem
                .split(|character: char| !character.is_ascii_alphanumeric())
                .collect();
            if words.iter().any(|word| {
                matches!(
                    *word,
                    "back"
                        | "rear"
                        | "booklet"
                        | "sheet"
                        | "sheets"
                        | "score"
                        | "scores"
                        | "spectrogram"
                        | "spectro"
                        | "waveform"
                        | "wave"
                        | "thumb"
                        | "thumbnail"
                ) || word.starts_with("page")
            }) {
                return None;
            }
            let rank = if tile_original == Some(filename) {
                0
            } else if words.contains(&"cover") || words.contains(&"front") {
                1
            } else if words
                .iter()
                .any(|word| matches!(*word, "art" | "artwork" | "albumart"))
            {
                2
            } else if words.contains(&"folder") {
                3
            } else {
                return None;
            };
            let size = number(&file["size"]).filter(|size| (1..=MAX_COVER_BYTES).contains(size))?;
            let mut dimensions = [0; 2];
            for (index, key) in ["width", "height"].iter().enumerate() {
                if !file[key].is_null() {
                    dimensions[index] = number(&file[key])
                        .filter(|dimension| (1..=MAX_COVER_DIMENSION).contains(dimension))?;
                }
            }
            let pixels = dimensions[0] * dimensions[1];
            if pixels > MAX_COVER_DECODED_BYTES / 4 {
                return None;
            }
            Some((rank, Reverse(pixels), Reverse(size), filename))
        })
        .min()?;
    let mut segments = vec!["download", identifier];
    segments.extend(filename.split('/'));
    archive_url(&segments).ok()
}

/// License links are displayed only; metadata-supplied external URLs are not fetched.
fn public_license_url(raw: &str) -> Option<Url> {
    let mut url = Url::parse(raw.trim()).ok()?;
    // Historic metadata uses HTTP CC URLs; the official site supports HTTPS.
    if url.scheme() == "http"
        && matches!(
            url.host_str(),
            Some("creativecommons.org" | "www.creativecommons.org")
        )
    {
        url.set_scheme("https").ok()?;
    }
    (url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && !remote_url_has_non_public_host(&url))
    .then_some(url)
}

/// Gives standard Creative Commons URLs useful names without inventing rights.
fn license_label(url: &Url) -> String {
    if matches!(
        url.host_str(),
        Some("creativecommons.org" | "www.creativecommons.org")
    ) {
        let parts: Vec<_> = url
            .path()
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        match parts.as_slice() {
            ["licenses", code, version, ..] => {
                return format!("CC {} {version}", code.to_ascii_uppercase());
            }
            ["publicdomain", "zero", version, ..] => {
                return format!("CC0 {version} (Public Domain)");
            }
            ["publicdomain", "mark", ..] => return "Public Domain Mark".into(),
            _ => {}
        }
    }
    url.as_str().into()
}

/// Accepts integer counts without silently rounding floats or negative values.
fn number(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) if text.len() <= 32 => text.trim().parse().ok(),
        Value::Array(values) if values.len() == 1 => number(&values[0]),
        _ => None,
    }
}

/// Parses documented UTC metadata dates and ISO dates, never the cache timestamp.
fn timestamp(value: &Value) -> Option<i64> {
    let texts = values(value, 128);
    let raw = texts.first()?.trim();
    if let Ok(date) = DateTime::parse_from_rfc3339(raw) {
        return Some(date.timestamp());
    }
    if let Ok(date) = NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S") {
        return Some(date.and_utc().timestamp());
    }
    let date = match raw.len() {
        4 => format!("{raw}-01-01"),
        7 => format!("{raw}-01"),
        _ => raw.into(),
    };
    NaiveDate::parse_from_str(&date, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)
        .map(|date| date.and_utc().timestamp())
}

/// Converts decimal seconds or colon-separated durations without float overflow.
fn duration(value: &Value) -> Option<u64> {
    let raw = match value {
        Value::String(raw) => raw.clone(),
        Value::Number(raw) => raw.to_string(),
        _ => return None,
    };
    if raw.len() > 64 {
        return None;
    }
    let parts: Vec<_> = raw.trim().split(':').collect();
    if !(1..=3).contains(&parts.len()) {
        return None;
    }
    let mut result = 0_u64;
    for (index, part) in parts.iter().enumerate() {
        let (whole, fraction) = part
            .split_once('.')
            .map_or((*part, None), |(whole, fraction)| (whole, Some(fraction)));
        if whole.is_empty()
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || fraction.is_some_and(|fraction| {
                index + 1 != parts.len()
                    || fraction.is_empty()
                    || !fraction.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            return None;
        }
        let number = whole.parse::<u64>().ok()?;
        if parts.len() > 1 && index > 0 && number >= 60 {
            return None;
        }
        result = result.checked_mul(60)?.checked_add(number)?;
    }
    Some(result)
}

/// Reads numeric track/disc tags, including common fraction notation.
fn track_number(value: &Value) -> Option<u64> {
    if let Some(number) = number(value) {
        return (number > 0).then_some(number);
    }
    let raw = value.as_str().filter(|raw| raw.len() <= 32)?;
    let number = raw.split('/').next()?.trim().parse::<u64>().ok()?;
    (number > 0).then_some(number)
}

/// Disallows traversal and ambiguous separators before percent-encoding paths.
fn valid_filename(filename: &str) -> bool {
    !filename.is_empty()
        && filename.len() <= MAX_FILENAME_BYTES
        && !filename.contains('\\')
        && !filename.chars().any(char::is_control)
        && filename.split('/').count() <= 32
        && filename
            .split('/')
            .all(|part| !matches!(part, "" | "." | ".."))
}

/// Prefers practical compressed encodings, avoiding low-bitrate MP3 derivatives.
fn audio_rank(file: &Value, filename: &str) -> Option<u8> {
    let extension = filename.rsplit('.').next()?.to_ascii_lowercase();
    let format = file["format"].as_str().unwrap_or_default();
    match extension.as_str() {
        "mp3" if format.eq_ignore_ascii_case("VBR MP3") => Some(0),
        "opus" => Some(1),
        "mp3" if format.starts_with("64Kbps") || format.starts_with("32Kbps") => Some(6),
        "mp3" => Some(2),
        "ogg" | "oga" => Some(3),
        "m4a" | "m4b" | "aac" => Some(4),
        "flac" => Some(7),
        "wav" | "wave" | "aif" | "aiff" => Some(8),
        "mp2" | "wma" | "ape" | "shn" => Some(9),
        _ => None,
    }
}

/// Resolves derivative ancestry with a bounded cycle guard and restriction checks.
fn original_file<'a>(
    filename: &'a str,
    files: &HashMap<&'a str, &'a Value>,
) -> Option<(&'a str, &'a Value)> {
    let mut current = filename;
    let mut seen = Vec::new();
    for _ in 0..MAX_DERIVATIVE_DEPTH {
        if seen.contains(&current) {
            return None;
        }
        seen.push(current);
        let file = *files.get(current)?;
        if restricted(file) {
            return None;
        }
        let Some(original) = file["original"].as_str().filter(|value| !value.is_empty()) else {
            return Some((current, file));
        };
        if !valid_filename(original) {
            return None;
        }
        if !files.contains_key(original) {
            return Some((original, file));
        }
        current = original;
    }
    None
}

/// Retains original metadata when a preferred encoding omits chapter tags.
struct TrackCandidate<'a> {
    filename: &'a str,
    file: &'a Value,
    original: &'a Value,
    root_name: &'a str,
    rank: u8,
}

impl TrackCandidate<'_> {
    /// Uses disc and track tags on either the encoding or its original file.
    fn order(&self) -> (Option<u64>, Option<u64>) {
        (
            track_number(&self.file["disc"]).or_else(|| track_number(&self.original["disc"])),
            track_number(&self.file["track"]).or_else(|| track_number(&self.original["track"])),
        )
    }
}

/// Selects one audio encoding per derivative family, then applies natural order.
fn normalize_tracks(
    identifier: &str,
    files: &[Value],
) -> Result<Vec<ArchiveOrgTrack>, ProviderError> {
    let mut by_name = HashMap::new();
    for file in files {
        if let Some(name) = file["name"].as_str().filter(|name| valid_filename(name))
            && by_name.insert(name, file).is_some()
        {
            return Err(invalid_response("duplicate file names in item metadata"));
        }
    }
    let mut selected = BTreeMap::<&str, TrackCandidate<'_>>::new();
    for (&filename, &file) in &by_name {
        let Some(rank) = audio_rank(file, filename) else {
            continue;
        };
        let Some((root_name, original)) = original_file(filename, &by_name) else {
            continue;
        };
        let candidate = TrackCandidate {
            filename,
            file,
            original,
            root_name,
            rank,
        };
        let replace = selected.get(root_name).is_none_or(|previous| {
            (rank, filename != root_name, filename)
                < (
                    previous.rank,
                    previous.filename != root_name,
                    previous.filename,
                )
        });
        if replace {
            selected.insert(root_name, candidate);
            if selected.len() > MAX_TRACKS {
                return Err(invalid_response("too many playable audio tracks"));
            }
        }
    }
    let waveforms = accepted_family_waveforms(&by_name, &selected);
    let mut download_variants = accepted_family_download_variants(identifier, &by_name, &selected)?;
    let mut candidates: Vec<_> = selected.into_values().collect();
    candidates.sort_by(|left, right| {
        let (left_disc, left_track) = left.order();
        let (right_disc, right_track) = right.order();
        left_disc
            .unwrap_or(1)
            .cmp(&right_disc.unwrap_or(1))
            .then_with(|| {
                left_track
                    .unwrap_or(u64::MAX)
                    .cmp(&right_track.unwrap_or(u64::MAX))
            })
            .then_with(|| natural_cmp(left.root_name, right.root_name))
            .then_with(|| left.filename.cmp(right.filename))
    });
    candidates
        .into_iter()
        .map(|candidate| {
            let mut segments = vec!["download", identifier];
            segments.extend(candidate.filename.split('/'));
            Ok(ArchiveOrgTrack {
                filename: candidate.filename.into(),
                title: text_value(&candidate.file["title"], MAX_LABEL_BYTES, false)
                    .or_else(|| text_value(&candidate.original["title"], MAX_LABEL_BYTES, false))
                    .and_then(|title| {
                        crate::legacy_text::normalized_legacy_windows_1251_value(Some(&title))
                    })
                    .unwrap_or_else(|| candidate.filename.into()),
                download_url: archive_url(&segments)?,
                duration_seconds: duration(&candidate.file["length"])
                    .or_else(|| duration(&candidate.original["length"])),
                size_bytes: number(&candidate.file["size"]),
                waveform_url: waveforms
                    .get(candidate.root_name)
                    .and_then(|filename| waveform_jpeg_url(identifier, filename)),
                download_variants: download_variants
                    .remove(candidate.root_name)
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// Repairs only otherwise-ambiguous short legacy titles independently corroborated
/// by a whole Cyrillic word in this same item's already-normalized description/title.
///
/// The shared local-tag heuristic remains unchanged. A single bounded word set
/// avoids rescanning an item description for each of thousands of audio tracks;
/// filenames, URLs and download variants are never modified.
fn repair_corroborated_short_track_titles(tracks: &mut [ArchiveOrgTrack], item: &ArchiveOrgItem) {
    let words: HashSet<&str> = std::iter::once(item.title.as_str())
        .chain(item.description.as_deref())
        .flat_map(|text| text.split(|character: char| !character.is_alphabetic()))
        .filter(|word| {
            (4..=5).contains(&word.chars().count())
                && word
                    .chars()
                    .all(|character| matches!(character, '\u{0400}'..='\u{052f}'))
        })
        .collect();
    if words.is_empty() {
        return;
    }
    for track in tracks {
        if !(4..=5).contains(&track.title.chars().count()) {
            continue;
        }
        let bytes: Option<Vec<u8>> = track
            .title
            .chars()
            .map(|character| {
                u8::try_from(u32::from(character))
                    .ok()
                    .filter(|byte| *byte >= 0xc0)
            })
            .collect();
        let Some(bytes) = bytes else {
            continue;
        };
        let (decoded, had_errors) = encoding_rs::WINDOWS_1251.decode_without_bom_handling(&bytes);
        if !had_errors && words.contains(decoded.as_ref()) {
            track.title = decoded.into_owned();
        }
    }
}

/// Preserves exact existing audio/video files without changing the playback encoding choice.
///
/// The existing ancestry resolver excludes restrictions, cycles and unsafe paths.
/// Only accepted audio families are retained, so a video original can accompany
/// its playable audio derivative without turning unrelated movies into tracks.
/// The metadata record bound and per-family cap bound the retained menu; overflow
/// fails instead of silently selecting or hiding an encoding.
fn accepted_family_download_variants<'a>(
    identifier: &str,
    files: &HashMap<&'a str, &'a Value>,
    selected: &BTreeMap<&'a str, TrackCandidate<'a>>,
) -> Result<HashMap<&'a str, Vec<ArchiveOrgDownloadVariant>>, ProviderError> {
    let mut variants = HashMap::<&str, Vec<ArchiveOrgDownloadVariant>>::new();
    for (&filename, &file) in files {
        let Some(is_video) = download_media_kind(file, filename) else {
            continue;
        };
        let Some((root, _)) = original_file(filename, files) else {
            continue;
        };
        if !selected.contains_key(root) {
            continue;
        }
        let family = variants.entry(root).or_default();
        if family.len() >= MAX_DOWNLOAD_VARIANTS_PER_TRACK {
            return Err(invalid_response(
                "too many download variants for one audio track",
            ));
        }
        let mut segments = vec!["download", identifier];
        segments.extend(filename.split('/'));
        let provenance = match file["source"].as_str() {
            Some("original")
                if file["original"].is_null() || file["original"].as_str() == Some("") =>
            {
                ArchiveOrgFileProvenance::Original
            }
            Some("derivative") => ArchiveOrgFileProvenance::Derivative,
            _ => ArchiveOrgFileProvenance::Unknown,
        };
        family.push(ArchiveOrgDownloadVariant {
            filename: filename.into(),
            download_url: archive_url(&segments)?,
            format: text_value(&file["format"], MAX_LABEL_BYTES, false).unwrap_or_else(|| {
                filename
                    .rsplit('.')
                    .next()
                    .unwrap_or_default()
                    .to_ascii_uppercase()
            }),
            size_bytes: number(&file["size"]),
            provenance,
            is_video,
        });
    }
    for family in variants.values_mut() {
        family.sort_by(|left, right| {
            provenance_rank(left.provenance)
                .cmp(&provenance_rank(right.provenance))
                .then_with(|| left.format.cmp(&right.format))
                .then_with(|| left.filename.cmp(&right.filename))
        });
    }
    Ok(variants)
}

/// Sorts uploaded originals before generated and unknown-provenance files.
const fn provenance_rank(provenance: ArchiveOrgFileProvenance) -> u8 {
    match provenance {
        ArchiveOrgFileProvenance::Original => 0,
        ArchiveOrgFileProvenance::Derivative => 1,
        ArchiveOrgFileProvenance::Unknown => 2,
    }
}

/// Recognizes media containers while refusing explicitly marked analysis/metadata files.
/// Video recognition is limited to variants of an already accepted audio family.
fn download_media_kind(file: &Value, filename: &str) -> Option<bool> {
    let format = file["format"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if ["analysis", "metadata", "spectrogram", "waveform", "tile"]
        .iter()
        .any(|denied| format.contains(denied))
        || matches!(format.as_str(), "png" | "jpeg" | "gif" | "text" | "xml")
    {
        return None;
    }
    if audio_rank(file, filename).is_some() {
        return Some(false);
    }
    matches!(
        filename.rsplit('.').next()?.to_ascii_lowercase().as_str(),
        "mp4"
            | "m4v"
            | "webm"
            | "mkv"
            | "mov"
            | "avi"
            | "mpg"
            | "mpeg"
            | "ogv"
            | "wmv"
            | "3gp"
            | "vob"
            | "m2ts"
            | "ts"
    )
    .then_some(true)
}

/// Selects an explicitly generated PNG for each accepted audio family in one bounded pass.
///
/// Archive waveforms use the exact audio parent's path with a PNG extension.
/// Source/format, matching stem, ancestry and resource bounds prevent arbitrary
/// images, spectrograms or tiles from becoming waveform fallbacks. Root-original
/// waveforms rank ahead of selected-encoding or other family derivatives, with
/// deterministic filename ties independent of metadata/HashMap iteration order.
fn accepted_family_waveforms<'a>(
    files: &HashMap<&'a str, &'a Value>,
    selected: &BTreeMap<&'a str, TrackCandidate<'a>>,
) -> HashMap<&'a str, &'a str> {
    let mut waveforms = HashMap::<&str, (u8, &str)>::new();
    for (&filename, &file) in files {
        if restricted(file)
            || file["source"].as_str() != Some("derivative")
            || !file["format"]
                .as_str()
                .is_some_and(|format| format.eq_ignore_ascii_case("PNG"))
        {
            continue;
        }
        let Some((stem, extension)) = filename.rsplit_once('.') else {
            continue;
        };
        if !extension.eq_ignore_ascii_case("png") {
            continue;
        }
        let Some(parent) = file["original"]
            .as_str()
            .filter(|name| valid_filename(name))
        else {
            continue;
        };
        let Some(parent_file) = files.get(parent) else {
            continue;
        };
        if parent.rsplit_once('.').map(|(stem, _)| stem) != Some(stem)
            || audio_rank(parent_file, parent).is_none()
            || !bounded_waveform_dimensions(file)
        {
            continue;
        }
        let Some((root, _)) = original_file(filename, files) else {
            continue;
        };
        let Some(track) = selected.get(root) else {
            continue;
        };
        let rank = if parent == root {
            0
        } else if parent == track.filename {
            1
        } else {
            2
        };
        let candidate = (rank, filename);
        waveforms
            .entry(root)
            .and_modify(|current| {
                if candidate < *current {
                    *current = candidate;
                }
            })
            .or_insert(candidate);
    }
    waveforms
        .into_iter()
        .map(|(root, (_, filename))| (root, filename))
        .collect()
}

/// Metadata can reject over-budget waveforms early; image decoding repeats these checks.
fn bounded_waveform_dimensions(file: &Value) -> bool {
    if !number(&file["size"]).is_some_and(|size| (1..=MAX_COVER_BYTES).contains(&size)) {
        return false;
    }
    let mut dimensions = [0; 2];
    for (index, key) in ["width", "height"].iter().enumerate() {
        if !file[key].is_null() {
            let Some(dimension) =
                number(&file[key]).filter(|size| (1..=MAX_COVER_DIMENSION).contains(size))
            else {
                return false;
            };
            dimensions[index] = dimension;
        }
    }
    dimensions[0] * dimensions[1] <= MAX_COVER_DECODED_BYTES / 4
}

/// Requests one explicit waveform at native size, flattened to JPEG for visible alpha.
///
/// Raw Archive PNG waveforms can contain black RGB with the signal only in alpha.
/// The verified IIIF JPEG route preserves the visible waveform without upscaling;
/// the artwork transport retains byte/decode/DNS limits and item-tile fallback.
/// See <https://iiif.io/api/image/3.0/#42-size>.
fn waveform_jpeg_url(identifier: &str, filename: &str) -> Option<Url> {
    if !valid_identifier(identifier) || !valid_filename(filename) {
        return None;
    }
    let mut url = Url::parse("https://iiif.archive.org/image/iiif/3/").ok()?;
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .push(&format!("{identifier}/{filename}"))
        .extend(["full", "max", "0", "default.jpg"]);
    Some(url)
}

/// Compares numeric filename runs without converting arbitrarily long integers.
fn natural_cmp(left: &str, right: &str) -> Ordering {
    let (mut left, mut right) = (left.as_bytes(), right.as_bytes());
    while let (Some(&a), Some(&b)) = (left.first(), right.first()) {
        if a.is_ascii_digit() && b.is_ascii_digit() {
            let a_len = left.iter().take_while(|byte| byte.is_ascii_digit()).count();
            let b_len = right
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            let a_digits = &left[..a_len];
            let b_digits = &right[..b_len];
            let a_number = &a_digits[a_digits.iter().take_while(|byte| **byte == b'0').count()..];
            let b_number = &b_digits[b_digits.iter().take_while(|byte| **byte == b'0').count()..];
            let order = a_number
                .len()
                .cmp(&b_number.len())
                .then_with(|| a_number.cmp(b_number));
            if order != Ordering::Equal {
                return order;
            }
            left = &left[a_len..];
            right = &right[b_len..];
        } else {
            let order = a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase());
            if order != Ordering::Equal {
                return order;
            }
            left = &left[1..];
            right = &right[1..];
        }
    }
    left.len().cmp(&right.len())
}

/// Builds bounded comments in API order; missing review likes are not star counts.
fn normalize_reviews(identifier: &str, reviews: &[Value]) -> Vec<VideoComment> {
    reviews
        .iter()
        .filter_map(|review| {
            let author_name =
                text_value(&review["reviewer"], MAX_VIDEO_COMMENT_AUTHOR_BYTES, false)?;
            if author_name.chars().count() > MAX_VIDEO_COMMENT_AUTHOR_CHARS {
                return None;
            }
            let title = text_value(&review["reviewtitle"], MAX_LABEL_BYTES, false);
            let body = text_value(&review["reviewbody"], MAX_VIDEO_COMMENT_TEXT_BYTES, true);
            let stars = number(&review["stars"])
                .filter(|stars| (1..=5).contains(stars))
                .map(|stars| format!("{stars}/5 stars"));
            let text = [title, body, stars]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("\n\n");
            if text.is_empty()
                || text.len() > MAX_VIDEO_COMMENT_TEXT_BYTES
                || text.chars().count() > MAX_VIDEO_COMMENT_TEXT_CHARS
            {
                return None;
            }
            let profile = review["reviewer_itemname"].as_str().and_then(profile_url);
            let hash = author_name
                .bytes()
                .chain(review["createdate"].as_str().unwrap_or_default().bytes())
                .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                    (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
                });
            Some(VideoComment {
                comment_id: format!("archive:{identifier}:{hash:016x}"),
                author_name,
                author_channel_url: profile,
                text,
                like_count: 0,
                published_at: timestamp(&review["createdate"])
                    .or_else(|| timestamp(&review["reviewdate"])),
                updated_at: timestamp(&review["reviewdate"]),
            })
        })
        .take(MAX_VIDEO_COMMENTS)
        .collect()
}

/// Drops terminal controls and bidi overrides while preserving readable breaks.
fn plain_text(raw: &str, multiline: bool) -> String {
    let safe: String = raw
        .chars()
        .filter(|character| {
            (!character.is_control() || matches!(character, '\n' | '\r' | '\t'))
                && !matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .collect();
    if multiline {
        safe.replace('\r', "\n")
            .lines()
            .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        safe.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

/// Content inside these elements is not user-visible descriptive text.
fn hidden_tag(tag: &[u8]) -> bool {
    matches!(
        tag,
        b"script" | b"style" | b"template" | b"noscript" | b"svg" | b"math"
    )
}

/// Tokenizes HTML with entity decoding; scripts, attributes, and comments vanish.
fn html_text(raw: &str, multiline: bool) -> String {
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    let mut output = String::new();
    let mut hidden = Vec::<Vec<u8>>::new();
    for token in Tokenizer::new_with_emitter(raw, emitter).take(MAX_HTML_TOKENS) {
        let Ok(token) = token;
        match token {
            Token::StartTag(tag) if hidden_tag(tag.name.as_slice()) => {
                if hidden.len() >= 64 {
                    break;
                }
                hidden.push(tag.name.to_vec());
            }
            Token::EndTag(tag)
                if hidden
                    .last()
                    .is_some_and(|name| name.as_slice() == tag.name.as_slice()) =>
            {
                hidden.pop();
            }
            Token::String(text) if hidden.is_empty() => {
                output.push_str(&String::from_utf8_lossy(text.value.as_ref()));
            }
            Token::StartTag(tag)
                if hidden.is_empty()
                    && matches!(tag.name.as_slice(), b"br" | b"p" | b"div" | b"li") =>
            {
                output.push('\n');
            }
            Token::EndTag(tag)
                if hidden.is_empty() && matches!(tag.name.as_slice(), b"p" | b"div" | b"li") =>
            {
                output.push('\n');
            }
            _ => {}
        }
    }
    plain_text(&output, multiline)
}

/// One recognized, optional public-page metadata element.
enum PageField {
    Uploader(Url),
    Collection(usize),
    Favorites,
    Views,
}

/// Accumulates a bounded element without trusting attributes as visible text.
struct PageCapture {
    field: PageField,
    tag: Vec<u8>,
    depth: usize,
    text: String,
}

/// Enriches only recognized page fields and already-known collection links.
fn enrich_from_page(item: &mut ArchiveOrgItem, page: &[u8]) {
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    let mut hidden = Vec::<Vec<u8>>::new();
    let mut capture: Option<PageCapture> = None;
    for token in Tokenizer::new_with_emitter(page, emitter).take(MAX_HTML_TOKENS) {
        let Ok(token) = token;
        match token {
            Token::StartTag(tag) if hidden_tag(tag.name.as_slice()) => {
                if hidden.len() >= 64 {
                    break;
                }
                hidden.push(tag.name.to_vec());
            }
            Token::EndTag(tag)
                if hidden
                    .last()
                    .is_some_and(|name| name.as_slice() == tag.name.as_slice()) =>
            {
                hidden.pop();
            }
            Token::String(text) if hidden.is_empty() => {
                if let Some(capture) = &mut capture {
                    if capture.text.len().saturating_add(text.value.len()) > MAX_LABEL_BYTES {
                        capture.text.clear();
                        break;
                    }
                    capture
                        .text
                        .push_str(&String::from_utf8_lossy(text.value.as_ref()));
                }
            }
            Token::StartTag(tag) if hidden.is_empty() => {
                if let Some(capture) = &mut capture {
                    if tag.name.as_slice() == capture.tag.as_slice() {
                        capture.depth = capture.depth.saturating_add(1);
                    }
                    continue;
                }
                let attribute = |key: &[u8]| {
                    tag.attributes
                        .get(key)
                        .and_then(|value| std::str::from_utf8(value.value.as_ref()).ok())
                };
                let classes = attribute(b"class").unwrap_or_default();
                let class = |name| classes.split_ascii_whitespace().any(|value| value == name);
                let field =
                    if tag.name.as_slice() == b"a" && class("item-upload-info__uploader-name") {
                        attribute(b"href")
                            .and_then(|href| canonical_page_link(href, true))
                            .map(PageField::Uploader)
                    } else if tag.name.as_slice() == b"a" && class("collection-item") {
                        attribute(b"href")
                            .and_then(|href| canonical_page_link(href, false))
                            .and_then(|url| {
                                item.collections
                                    .iter()
                                    .position(|collection| collection.url == url)
                            })
                            .map(PageField::Collection)
                    } else if class("favorite-count") {
                        Some(PageField::Favorites)
                    } else if attribute(b"itemprop") == Some("userInteractionCount") {
                        Some(PageField::Views)
                    } else {
                        None
                    };
                if let Some(field) = field {
                    capture = Some(PageCapture {
                        field,
                        tag: tag.name.to_vec(),
                        depth: 1,
                        text: String::new(),
                    });
                }
            }
            Token::EndTag(tag) if hidden.is_empty() => {
                if let Some(active) = &mut capture
                    && active.tag.as_slice() == tag.name.as_slice()
                {
                    active.depth = active.depth.saturating_sub(1);
                    if active.depth == 0
                        && let Some(finished) = capture.take()
                    {
                        apply_page_field(item, finished);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Accepts only canonical item/profile links; ignores bases and remote hosts.
fn canonical_page_link(href: &str, profile: bool) -> Option<Url> {
    if href.len() > 512 {
        return None;
    }
    let path = href.strip_prefix("https://archive.org").unwrap_or(href);
    let identifier = path.strip_prefix("/details/")?;
    if profile {
        profile_url(identifier)
    } else {
        valid_identifier(identifier)
            .then(|| archive_url(&["details", identifier]).ok())
            .flatten()
    }
}

/// Applies parsed display text without fabricating values for missing markup.
fn apply_page_field(item: &mut ArchiveOrgItem, capture: PageCapture) {
    let text = plain_text(&capture.text, false);
    if text.is_empty() {
        return;
    }
    match capture.field {
        PageField::Uploader(url) => {
            item.uploader = Some(ArchiveOrgLink { name: text, url });
        }
        PageField::Collection(index) => {
            item.collections[index].name = text;
        }
        PageField::Favorites => {
            item.favorite_count = display_count(&text).or(item.favorite_count);
        }
        PageField::Views => {
            item.download_count = display_count(&text).or(item.download_count);
        }
    }
}

/// Reads exact decimal page counters, never rounded display abbreviations.
fn display_count(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    let end = raw
        .bytes()
        .take_while(|byte| byte.is_ascii_digit() || *byte == b',')
        .count();
    let (token, label) = raw.split_at(end);
    if token.is_empty()
        || (!label.trim().is_empty() && !label.trim().eq_ignore_ascii_case("favorites"))
    {
        return None;
    }
    if token.contains(',') {
        let mut groups = token.split(',');
        let first = groups.next()?;
        if first.is_empty() || first.len() > 3 || groups.any(|group| group.len() != 3) {
            return None;
        }
    }
    token.replace(',', "").parse().ok()
}

/// Keeps errors actionable without reflecting untrusted HTML or control bytes.
fn invalid_response(message: &str) -> ProviderError {
    ProviderError::InvalidResponse(format!("Archive.org {message}"))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::{Value, json};

    use super::*;

    /// The reported item has a full-size original alongside tiny generated tiles.
    #[test]
    fn item_cover_prefers_original_art_over_waveforms_and_small_thumbnails() {
        let identifier = "005-vitaly-zdanevich-and-geo-gorgiladze";
        let mut value = metadata(json!([
            {"name": "005-vitaly-zdanevich-and-geo-gorgiladze.ogg", "source": "original"},
            {"name": "005-vitaly-zdanevich-and-geo-gorgiladze.png", "source": "derivative", "format": "PNG", "size": "45534", "original": "005-vitaly-zdanevich-and-geo-gorgiladze.ogg"},
            {"name": "005-vitaly-zdanevich-and-geo-gorgiladze_spectrogram.png", "source": "derivative", "format": "Spectrogram", "size": "335528"},
            {"name": "__ia_thumb.jpg", "source": "original", "format": "Item Tile", "size": "14484"},
            {"name": "art.png", "source": "original", "format": "PNG", "size": "3853849"},
            {"name": "art_thumb.jpg", "source": "derivative", "format": "JPEG Thumb", "size": "10299", "original": "art.png"}
        ]));
        value["metadata"]["identifier"] = json!(identifier);
        let (client, transport) = mock_client(vec![bytes(&value)]);
        let details = client.item_details(identifier).expect("public item");
        assert_eq!(
            details.item.artwork_url.expect("original cover").as_str(),
            format!("https://archive.org/download/{identifier}/art.png")
        );
        assert_eq!(details.tracks.len(), 1);
        assert_eq!(transport.requests.lock().expect("requests").len(), 2);
    }

    /// Original covers are ranked deterministically, with known resolution preferred.
    #[test]
    fn item_cover_selection_is_deterministic_and_constructs_canonical_paths() {
        let mut files = vec![
            json!({"name": "cover.jpg", "source": "original", "size": 512, "width": 320, "height": 320}),
            json!({"name": "images/Front Cover ?#.webp", "source": "original", "size": 256, "width": 2048, "height": 2048}),
            json!({"name": "cover-back.png", "source": "original", "size": 1024, "width": 4096, "height": 4096}),
            json!({"name": "cover-sheet.png", "source": "original", "size": 1024}),
        ];
        for _ in 0..files.len() {
            let (client, _) = mock_client(vec![bytes(&metadata(json!(files)))]);
            let cover = client
                .item_details("mock_audio")
                .expect("item")
                .item
                .artwork_url
                .expect("cover");
            assert_eq!(
                cover.as_str(),
                "https://archive.org/download/mock_audio/images/Front%20Cover%20%3F%23.webp"
            );
            assert!(cover.query().is_none());
            assert!(cover.fragment().is_none());
            files.rotate_left(1);
        }
    }

    /// An explicit item tile may identify a cover whose original filename is arbitrary.
    #[test]
    fn item_cover_follows_only_a_safe_original_named_by_the_item_tile() {
        for (original, expected) in [
            ("IMG_123.jpg", "download/mock_audio/IMG_123.jpg"),
            ("../IMG_123.jpg", "services/img/mock_audio"),
            (
                "https://evil.example/IMG_123.jpg",
                "services/img/mock_audio",
            ),
        ] {
            let (client, _) = mock_client(vec![bytes(&metadata(json!([
                {"name": "__ia_thumb.jpg", "source": "derivative", "format": "Item Tile", "size": 50, "original": original},
                {"name": "IMG_123.jpg", "source": "original", "format": "JPEG", "size": 1024}
            ])))]);
            assert_eq!(
                client
                    .item_details("mock_audio")
                    .expect("item")
                    .item
                    .artwork_url
                    .expect("cover")
                    .as_str(),
                format!("https://archive.org/{expected}")
            );
        }
    }

    /// Metadata can reject unsuitable originals early without weakening download/decode guards.
    #[test]
    fn item_cover_rejects_unsafe_oversized_and_non_cover_candidates() {
        for invalid in [
            json!({"name": "cover.jpg", "source": "original", "size": 4 * 1024 * 1024 + 1}),
            json!({"name": "cover.jpg", "source": "original", "size": 0}),
            json!({"name": "cover.jpg", "source": "original", "size": null}),
            json!({"name": "cover.jpg", "source": "original", "size": 128, "width": 4097}),
            json!({"name": "cover.jpg", "source": "original", "size": 128, "width": 4000, "height": 4000}),
            json!({"name": "cover.jpg", "source": "original", "size": 128, "height": 0}),
            json!({"name": "cover.jpg", "source": "original", "size": 128, "width": "invalid"}),
            json!({"name": "cover.jpg", "source": "derivative", "size": 128}),
            json!({"name": "cover.jpg", "source": "original", "size": 128, "original": "audio.ogg"}),
            json!({"name": "cover.jpg", "source": "original", "size": 128, "private": true}),
            json!({"name": "cover.jpg", "source": "original", "size": 128, "format": "Spectrogram"}),
            json!({"name": "cover.svg", "source": "original", "size": 128}),
            json!({"name": "cover-page.jpg", "source": "original", "size": 128}),
            json!({"name": "cover-sheet.jpg", "source": "original", "size": 128}),
            json!({"name": "cover-waveform.jpg", "source": "original", "size": 128}),
            json!({"name": "../cover.jpg", "source": "original", "size": 128}),
            json!({"name": "https://evil.example/cover.jpg", "source": "original", "size": 128}),
        ] {
            let (client, _) = mock_client(vec![bytes(&metadata(json!([
                invalid, {"name": "art.png", "source": "original", "size": 1024}
            ])))]);
            assert_eq!(
                client
                    .item_details("mock_audio")
                    .expect("item")
                    .item
                    .artwork_url
                    .expect("safe cover")
                    .as_str(),
                "https://archive.org/download/mock_audio/art.png",
                "unsuitable candidate: {invalid}"
            );
        }
    }

    /// Items without a clearly identified, bounded original retain the image-service fallback.
    #[test]
    fn item_cover_keeps_service_fallback_for_ambiguous_images_and_generated_art() {
        let (client, _) = mock_client(vec![bytes(&metadata(json!([
            {"name": "IMG_123.jpg", "source": "original", "size": 1024},
            {"name": "page001.png", "source": "original", "size": 1024},
            {"name": "art.png", "source": "derivative", "size": 1024, "original": "audio.ogg"},
            {"name": "__ia_thumb.jpg", "source": "original", "format": "Item Tile", "size": 128}
        ])))]);
        assert_eq!(
            client
                .item_details("mock_audio")
                .expect("item")
                .item
                .artwork_url
                .expect("fallback")
                .as_str(),
            "https://archive.org/services/img/mock_audio"
        );
    }

    /// Opt-in metadata-only regression for the exact low-resolution artwork report.
    #[test]
    #[ignore = "explicit public-network artwork metadata test; no media downloads"]
    fn live_original_archive_cover_metadata_smoke() {
        let details = ArchiveOrgClient::new()
            .item_details("005-vitaly-zdanevich-and-geo-gorgiladze")
            .expect("public item metadata");
        assert_eq!(
            details.item.artwork_url.expect("original cover").as_str(),
            "https://archive.org/download/005-vitaly-zdanevich-and-geo-gorgiladze/art.png"
        );
    }

    #[test]
    fn mixed_track_tags_have_a_total_deterministic_order() {
        let files = vec![
            json!({"name": "z.mp3", "track": "2"}),
            json!({"name": "a.mp3", "track": "10"}),
            json!({"name": "m.mp3"}),
        ];
        for rotation in 0..3 {
            let mut input = files.clone();
            input.rotate_left(rotation);
            let tracks = normalize_tracks("mock_audio", &input).expect("tracks");
            assert_eq!(
                tracks
                    .iter()
                    .map(|track| track.filename.as_str())
                    .collect::<Vec<_>>(),
                ["z.mp3", "a.mp3", "m.mp3"]
            );
        }
    }

    #[test]
    fn license_rights_and_topics_preserve_actual_public_metadata() {
        let mut value = metadata(json!([]));
        value["metadata"]["licenseurl"] =
            json!("http://creativecommons.org/licenses/by-nc-sa/4.0/");
        value["metadata"]["subject"] = json!("Music &amp; Radio; Live");
        let item = normalize_item(&value["metadata"]).expect("metadata");
        assert_eq!(item.license.as_deref(), Some("CC BY-NC-SA 4.0"));
        assert_eq!(
            item.license_url.as_ref().expect("license").as_str(),
            "https://creativecommons.org/licenses/by-nc-sa/4.0/"
        );
        assert_eq!(item.topics, ["Music & Radio", "Live"]);
        value["metadata"]["rights"] = json!("Permission granted for noncommercial listening.");
        value["metadata"]["licenseurl"] = json!("http://127.0.0.1/private");
        let item = normalize_item(&value["metadata"]).expect("rights");
        assert_eq!(
            item.license.as_deref(),
            Some("Permission granted for noncommercial listening.")
        );
        assert_eq!(item.license_url, None);
        for raw in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "https://user:secret@example.com/license",
            "https://localhost/license",
            "https://127.0.0.1/license",
        ] {
            assert_eq!(public_license_url(raw), None, "{raw}");
        }
    }

    #[test]
    fn numeric_parsers_reject_invalid_or_rounded_values() {
        for (raw, expected) in [
            ("0", Some(0)),
            ("1,234", Some(1234)),
            ("1,234 Favorites", Some(1234)),
            ("1,234Favorites", Some(1234)),
            ("1.2K", None),
            ("unknown", None),
            ("12,34", None),
        ] {
            assert_eq!(display_count(raw), expected, "{raw}");
        }
        for raw in [
            "NaN",
            "inf",
            "-1",
            "1:60",
            "1:02:60",
            "1.5:02",
            "18446744073709551616",
            "0:01.",
        ] {
            assert_eq!(duration(&json!(raw)), None, "{raw}");
        }
        assert_eq!(duration(&json!("12:34.5")), Some(754));
        assert_eq!(duration(&json!(123.5)), Some(123));
        assert_eq!(number(&json!(-1)), None);
        assert_eq!(number(&json!(1.5)), None);
        assert_eq!(timestamp(&json!("invalid")), None);
    }

    #[test]
    fn count_bounds_and_oversized_optional_values_stay_bounded() {
        let mut value = metadata(json!([]));
        value["files"] = json!(
            (0..=MAX_FILES)
                .map(|index| json!({"name": format!("track{index}.mp3")}))
                .collect::<Vec<_>>()
        );
        let (client, _) = mock_client(vec![bytes(&value)]);
        assert!(client.item_details("mock_audio").is_err());
        value = metadata(json!([]));
        value["reviews"] = json!(vec![Value::Null; MAX_REVIEWS + 1]);
        let (client, _) = mock_client(vec![bytes(&value)]);
        assert!(client.item_details("mock_audio").is_err());
        value = metadata(json!([]));
        value["metadata"]["description"] = json!("x".repeat(MAX_DESCRIPTION_BYTES + 1));
        value["metadata"]["creator"] = json!(vec!["reader"; MAX_LIST_VALUES + 1]);
        let (client, _) = mock_client(vec![bytes(&value), vec![b'x'; MAX_HTML_BYTES + 1]]);
        let details = client
            .item_details("mock_audio")
            .expect("oversized optional fields omitted");
        assert_eq!(details.item.description, None);
        assert_eq!(details.item.creator, None);
        assert!(details.tracks.is_empty());
        assert_eq!(details.item.uploader, None);
    }

    #[test]
    fn search_rejects_bad_envelopes_but_skips_invalid_and_restricted_rows() {
        let request = ArchiveOrgSearchRequest {
            query: "jazz".into(),
            page: 1,
            limit: 2,
        };
        for value in [
            json!({"response": {"numFound": 1, "docs": {}}}),
            json!({"response": {"numFound": -1, "docs": []}}),
            json!({"response": {"numFound": 3, "docs": [{}, {}, {}]}}),
            json!({"response": {"numFound": 3, "start": 10, "docs": []}}),
        ] {
            let (client, _) = mock_client(vec![bytes(&value)]);
            assert!(client.search(&request).is_err());
        }
        let (client, _) = mock_client(vec![bytes(&json!({"response": {"numFound": 3, "docs": [
            {"identifier": "restricted", "mediatype": "audio", "access-restricted-item": true},
            {"identifier": "movie", "mediatype": "movies"}
        ]}}))]);
        let page = client.search(&request).expect("filtered page");
        assert!(page.items.is_empty());
        assert_eq!(page.next_page, Some(2));
    }

    #[test]
    fn hidden_html_cannot_manufacture_profile_or_collection_links() {
        let mut value = metadata(json!([]));
        value["metadata"]["collection"] = json!("known_collection");
        let mut item = normalize_item(&value["metadata"]).expect("item");
        enrich_from_page(&mut item, br#"<script><a class='item-upload-info__uploader-name' href='/details/@bad'>Bad</a></script><template><a class='collection-item' href='/details/known_collection'>Bad</a></template><a class='collection-item' href='/details/another_collection'>Unknown</a><a class='item-upload-info__uploader-name' href='//evil.example/details/@bad'>Bad</a>"#);
        assert_eq!(item.uploader, None);
        assert_eq!(item.collections[0].name, "known_collection");
        assert_eq!(item.collections.len(), 1);
        assert_eq!(
            html_text(
                "A &lt;B&gt;<style>hidden</style><!-- hidden --><p>C&#x1b;&#x202e;</p>",
                true
            ),
            "A <B>\nC"
        );
    }

    #[test]
    fn duplicate_file_names_are_rejected_and_transport_errors_remain_explicit() {
        let (client, _) = mock_client(vec![bytes(&metadata(json!([
            {"name": "same.mp3", "private": true}, {"name": "same.mp3", "private": false}
        ])))]);
        assert!(client.item_details("mock_audio").is_err());
        let (client, _) = mock_client(vec![]);
        assert!(matches!(
            client.item_details("mock_audio"),
            Err(ProviderError::HttpStatus(503))
        ));
    }

    #[test]
    fn oversized_reviews_are_skipped_and_missing_profiles_are_not_guessed() {
        let mut value = metadata(json!([]));
        value["reviews"] = json!([
            {"reviewer": "x".repeat(MAX_VIDEO_COMMENT_AUTHOR_BYTES + 1), "reviewbody": "body"},
            {"reviewer": "Public reader", "reviewbody": "body", "reviewer_itemname": "@../evil", "stars": "99"}
        ]);
        let (client, _) = mock_client(vec![bytes(&value)]);
        let details = client.item_details("mock_audio").expect("comments");
        assert_eq!(details.comments.len(), 1);
        assert_eq!(details.comments[0].author_channel_url, None);
        assert_eq!(details.comments[0].text, "body");
    }

    #[test]
    #[ignore = "explicit public-network smoke test; normal suites use mock data"]
    fn live_public_archive_search_and_item_smoke() {
        let client = ArchiveOrgClient::new();
        let page = client
            .search(&ArchiveOrgSearchRequest {
                query: "pride prejudice".into(),
                page: 1,
                limit: 2,
            })
            .expect("live search");
        assert!(!page.items.is_empty());
        let details = client
            .item_details("pride_and_prejudice_librivox")
            .expect("live metadata");
        assert!(!details.tracks.is_empty());
        assert!(
            details
                .tracks
                .iter()
                .all(|track| track.download_url.host_str() == Some("archive.org"))
        );
        assert!(details.item.uploader.is_some());
        assert!(details.item.favorite_count.is_some());
        assert!(details.item.size_bytes.is_some());
        println!(
            "public live smoke: {} search results, {} audio tracks, {} comments, uploader={}, favorites={:?}",
            page.items.len(),
            details.tracks.len(),
            details.comments.len(),
            details.item.uploader.as_ref().expect("uploader").name,
            details.item.favorite_count
        );
    }

    #[test]
    fn user_identifiers_require_explicit_audio_metadata_and_profiles_stay_distinct() {
        assert!(valid_identifier("@public_user"));
        assert!(valid_identifier(&"a".repeat(100)));
        assert!(!valid_identifier(&"a".repeat(101)));
        assert!(profile_url("@@public_user").is_none());
        assert!(normalize_item(&json!({"identifier":"@public_user"})).is_err());
        assert!(
            normalize_item(&json!({"identifier":"@public_user","mediatype":"account"})).is_err()
        );
        let mut value = metadata(json!([{"name":"recording.mp3"}]));
        value["metadata"]["identifier"] = json!("@public_user");
        let (client, _) = mock_client(vec![bytes(&value)]);
        assert_eq!(
            client
                .item_details("@public_user")
                .expect("explicit audio")
                .tracks
                .len(),
            1
        );
    }

    /// Large public audio collections include many non-playable analysis files.
    #[test]
    fn large_audio_collections_keep_all_tracks_despite_auxiliary_files() {
        const TRACK_COUNT: usize = 2_226;
        const FILE_COUNT: usize = 9_494;
        let files = (0..FILE_COUNT)
            .map(|index| {
                if index < TRACK_COUNT {
                    json!({
                        "name": format!("1996/{index:04} recording.mp3"),
                        "format": "VBR MP3",
                        "source": "original",
                        "title": format!("Recording {index}"),
                        "length": "1552.28",
                        "size": "18629888"
                    })
                } else {
                    json!({
                        "name": format!("analysis/{index:04}_spectrogram.png"),
                        "format": "Spectrogram",
                        "source": "derivative",
                        "original": format!("1996/{:04} recording.mp3", index % TRACK_COUNT)
                    })
                }
            })
            .collect::<Vec<_>>();
        for include_large_metadata in [false, true] {
            let mut value = metadata(json!(files));
            if include_large_metadata {
                // Match the reported item's public response size without a network fixture.
                let padding = 4_477_801_usize.saturating_sub(bytes(&value).len());
                value["fixture_metadata"] = json!("x".repeat(padding));
                assert!(bytes(&value).len() > 4 * 1024 * 1024);
            }
            let (client, _) = mock_client(vec![bytes(&value)]);
            let details = client
                .item_details("mock_audio")
                .expect("large audio collection");
            assert_eq!(details.tracks.len(), TRACK_COUNT);
            assert_eq!(details.tracks[0].filename, "1996/0000 recording.mp3");
            assert_eq!(
                details.tracks[TRACK_COUNT - 1].filename,
                "1996/2225 recording.mp3"
            );
            assert!(
                details
                    .tracks
                    .iter()
                    .all(|track| track.duration_seconds == Some(1552))
            );
        }
    }

    /// Manual verification of the originally reported item performs no media download.
    #[test]
    #[ignore = "explicit public-network regression; normal suites use mock metadata"]
    fn live_large_archive_audio_collection_smoke() {
        let details = ArchiveOrgClient::new()
            .item_details("20201105_20201105_1330")
            .expect("large public audio collection");
        assert_eq!(details.tracks.len(), 2_226);
        assert!(
            details
                .tracks
                .iter()
                .all(|track| track.filename.ends_with(".mp3"))
        );
        println!(
            "large public item: {} selected audio tracks",
            details.tracks.len()
        );
    }

    #[test]
    fn search_and_item_response_limits_remain_separate_and_enforced() {
        let (client, _) = mock_client(vec![vec![b' '; MAX_SEARCH_JSON_BYTES + 1]]);
        let request = ArchiveOrgSearchRequest {
            query: String::new(),
            page: 1,
            limit: 1,
        };
        assert!(matches!(
            client.search(&request),
            Err(ProviderError::ResponseTooLarge { limit }) if limit == MAX_SEARCH_JSON_BYTES
        ));
        let (client, _) = mock_client(vec![vec![b' '; MAX_ITEM_JSON_BYTES + 1]]);
        assert!(matches!(
            client.item_details("mock_audio"),
            Err(ProviderError::ResponseTooLarge { limit }) if limit == MAX_ITEM_JSON_BYTES
        ));
    }

    #[test]
    fn playable_limit_counts_derivative_families_and_never_silently_truncates() {
        let files = (0..MAX_TRACKS)
            .flat_map(|index| {
                [
                    json!({"name": format!("track{index}.mp3"), "source": "original"}),
                    json!({"name": format!("track{index}.ogg"), "source": "derivative",
                "original": format!("track{index}.mp3")}),
                ]
            })
            .collect::<Vec<_>>();
        let (client, _) = mock_client(vec![bytes(&metadata(json!(files)))]);
        assert_eq!(
            client
                .item_details("mock_audio")
                .expect("bounded derivative families")
                .tracks
                .len(),
            MAX_TRACKS
        );
        let files = (0..=MAX_TRACKS)
            .map(|index| json!({"name": format!("track{index}.mp3"), "source": "original"}))
            .collect::<Vec<_>>();
        let (client, _) = mock_client(vec![bytes(&metadata(json!(files)))]);
        assert!(matches!(
            client.item_details("mock_audio"),
            Err(ProviderError::InvalidResponse(message)) if message.contains("too many playable audio tracks")
        ));
    }

    #[derive(Default)]
    struct MockTransport {
        responses: Mutex<std::collections::VecDeque<Result<Vec<u8>, ProviderError>>>,
        requests: Mutex<Vec<Url>>,
    }

    impl ArchiveOrgTransport for MockTransport {
        fn fetch(&self, url: &Url, _max_bytes: usize) -> Result<Vec<u8>, ProviderError> {
            self.requests
                .lock()
                .expect("requests lock")
                .push(url.clone());
            self.responses
                .lock()
                .expect("responses lock")
                .pop_front()
                .unwrap_or(Err(ProviderError::HttpStatus(503)))
        }
    }

    fn mock_client(responses: Vec<Vec<u8>>) -> (ArchiveOrgClient, Arc<MockTransport>) {
        let transport = Arc::new(MockTransport {
            responses: Mutex::new(responses.into_iter().map(Ok).collect()),
            requests: Mutex::default(),
        });
        (
            ArchiveOrgClient::with_transport(transport.clone()),
            transport,
        )
    }

    fn bytes(value: &Value) -> Vec<u8> {
        serde_json::to_vec(value).expect("fixture JSON")
    }

    fn metadata(files: Value) -> Value {
        json!({
            "metadata": {"identifier": "mock_audio", "title": "Mock audio", "mediatype": "audio"},
            "files": files, "reviews": []
        })
    }

    /// Archive's public JSON already contains this Latin-1-decoded legacy title.
    #[test]
    fn legacy_cyrillic_archive_track_titles_are_repaired_for_display() {
        let identifier = "bis24518_miauj_1701";
        let mut value = metadata(json!([
            {"name":"01/01_01.mp3", "format":"VBR MP3", "title":"Áåòîíîìåøàëêà", "artist":"Ðýé Áðýäáåðè"},
            {"name":"01/01_01.ogg", "original":"01/01_01.mp3"},
            {"name":"01/01_02.mp3", "format":"VBR MP3", "title":"2"}
        ]));
        value["metadata"]["identifier"] = json!(identifier);
        value["metadata"]["title"] = json!("Брэдбери Рэй - Бетономешалка и другие рассказы");
        let (client, _) = mock_client(vec![bytes(&value)]);
        let details = client.item_details(identifier).expect("mock public item");
        assert_eq!(details.tracks[0].title, "Бетономешалка");
        assert_eq!(details.tracks[1].title, "2");
        assert_eq!(details.tracks[0].filename, "01/01_01.mp3");
        assert_eq!(
            details.tracks[0].download_url.as_str(),
            "https://archive.org/download/bis24518_miauj_1701/01/01_01.mp3"
        );
        assert_eq!(
            details.item.title,
            "Брэдбери Рэй - Бетономешалка и другие рассказы"
        );
    }

    /// An explicit two-request smoke check reads metadata only, never audio.
    #[test]
    #[ignore = "explicit public-network title regression; normal suites use mock metadata"]
    fn live_legacy_cyrillic_archive_track_titles_smoke() {
        let details = ArchiveOrgClient::new()
            .item_details("bis24518_miauj_1701")
            .expect("public metadata");
        assert_eq!(
            details.tracks.first().expect("first track").title,
            "Бетономешалка"
        );
        assert!(details.tracks.iter().any(|track| track.title == "Озеро"));
        assert!(details.tracks.iter().any(|track| track.title == "Скелет"));
        assert_eq!(details.tracks[0].filename, "01/01_01.mp3");
        println!(
            "public metadata: {} tracks; Бетономешалка, Озеро and Скелет repaired without changing remote filenames",
            details.tracks.len()
        );
    }

    /// Short titles need independent same-item whole-word evidence.
    #[test]
    fn legacy_cyrillic_short_title_requires_same_item_word_corroboration() {
        for (description, expected) in [
            ("Рассказы: «Озеро», «Скелет»", "Озеро"),
            ("Рассказы: «Озеровой»", "Îçåðî"),
            ("No Cyrillic title evidence", "Îçåðî"),
        ] {
            let mut value = metadata(json!([
                {"name":"01.mp3", "title":"Îçåðî"},
                {"name":"02.mp3", "title":"Ñêåëåò"},
                {"name":"03.mp3", "title":"Ôîí"},
                {"name":"04.mp3", "title":"Björk"}
            ]));
            value["metadata"]["description"] = json!(description);
            let (client, _) = mock_client(vec![bytes(&value)]);
            let details = client.item_details("mock_audio").expect("item context");
            assert_eq!(details.tracks[0].title, expected);
            assert_eq!(details.tracks[1].title, "Скелет");
            assert_eq!(details.tracks[2].title, "Ôîí");
            assert_eq!(details.tracks[3].title, "Björk");
        }
    }

    /// Repair also applies to a selected derivative's original metadata fallback.
    #[test]
    fn legacy_cyrillic_original_title_fallback_keeps_exact_remote_file_identity() {
        let filename = "Áåòîíîìåøàëêà.opus";
        let files = vec![
            json!({"name":"original.wav", "source":"original", "title":"Áåòîíîìåøàëêà"}),
            json!({"name":filename, "source":"derivative", "original":"original.wav"}),
        ];
        let tracks = normalize_tracks("mock_audio", &files).expect("audio family");
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "Бетономешалка");
        assert_eq!(tracks[0].filename, filename);
        assert_eq!(
            tracks[0].download_url,
            archive_url(&["download", "mock_audio", filename]).expect("unchanged exact path")
        );
        assert_eq!(tracks[0].download_variants[0].filename, "original.wav");
    }

    /// Accented Latin, real Unicode and ambiguous short text are not guessed.
    #[test]
    fn legacy_cyrillic_repair_preserves_latin_unicode_and_short_archive_titles() {
        for title in [
            "Björk",
            "François",
            "Beyoncé",
            "Motörhead",
            "Sigur Rós",
            "Café del Mar",
            "Été",
            "Áéî",
            "2",
            "Бетономешалка",
            "Ελληνικά",
            "ქართული",
            "日本語 😀",
        ] {
            let tracks =
                normalize_tracks("mock_audio", &[json!({"name":"audio.mp3", "title":title})])
                    .expect("audio");
            assert_eq!(tracks[0].title, title, "legitimate title must be preserved");
        }
        let filename = "Áåòîíîìåøàëêà.mp3";
        let tracks =
            normalize_tracks("mock_audio", &[json!({"name":filename})]).expect("filename fallback");
        assert_eq!(tracks[0].filename, filename);
        assert_eq!(
            tracks[0].title, filename,
            "missing metadata must not rewrite the filename fallback"
        );
    }

    #[test]
    fn search_normalizes_arrays_counts_dates_and_pagination() {
        let (client, transport) = mock_client(vec![bytes(&json!({"response": {
            "numFound": 3, "start": 0, "docs": [
                {"identifier": "mock_audio", "mediatype": "audio", "title": ["Mock &amp; music"],
                 "creator": ["Artist", "Reader"], "description": "<p>Hello<br>world</p>",
                 "date": "2001-01-01", "publicdate": "2024-01-02T03:04:05Z",
                 "downloads": 42, "num_favorites": "7", "num_reviews": 2,
                 "subject": ["Jazz; Live", "Music"], "language": "eng", "item_size": "900"},
                {"identifier": "second", "mediatype": "etree", "title": null}
            ]
        }}))]);
        let page = client
            .search(&ArchiveOrgSearchRequest {
                query: "jazz OR mediatype:movies".into(),
                page: 1,
                limit: 2,
            })
            .expect("search");
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.next_page, Some(2));
        assert_eq!(page.total, 3);
        assert_eq!(page.items[0].title, "Mock & music");
        assert_eq!(page.items[0].creator.as_deref(), Some("Artist; Reader"));
        assert_eq!(page.items[0].description.as_deref(), Some("Hello\nworld"));
        assert_eq!(page.items[0].favorite_count, Some(7));
        assert_eq!(page.items[0].review_count, Some(2));
        assert_eq!(page.items[0].download_count, Some(42));
        assert_eq!(page.items[0].size_bytes, Some(900));
        assert_eq!(page.items[0].topics, ["Jazz", "Live", "Music"]);
        assert_eq!(page.items[0].languages, ["eng"]);
        assert_eq!(page.items[0].published_at, Some(978_307_200));
        assert_eq!(page.items[0].uploaded_at, Some(1_704_164_645));
        assert_eq!(page.items[1].title, "second");
        assert_eq!(page.items[1].favorite_count, None);
        let requests = transport.requests.lock().expect("requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].host_str(), Some("archive.org"));
        let pairs: std::collections::HashMap<_, _> = requests[0].query_pairs().collect();
        let query = pairs.get("q").expect("query");
        assert!(query.contains("(mediatype:audio OR mediatype:etree)"));
        assert!(query.contains("-access-restricted-item:true"));
        assert!(
            query.contains("mediatype\\:movies"),
            "caller field syntax is escaped"
        );
    }

    #[test]
    fn invalid_requests_do_not_contact_transport() {
        let (client, transport) = mock_client(vec![]);
        for identifier in [
            "",
            "../secret",
            "https://example.com",
            "@@someone",
            "has space",
            "a/b",
            "a?b",
            ".dot",
            "é",
        ] {
            assert!(
                matches!(
                    client.item_details(identifier),
                    Err(ProviderError::InvalidRequest(_))
                ),
                "{identifier}"
            );
        }
        for request in [
            ArchiveOrgSearchRequest {
                query: String::new(),
                page: 0,
                limit: 10,
            },
            ArchiveOrgSearchRequest {
                query: String::new(),
                page: 1,
                limit: 0,
            },
            ArchiveOrgSearchRequest {
                query: String::new(),
                page: 1,
                limit: 101,
            },
            ArchiveOrgSearchRequest {
                query: "x".repeat(513),
                page: 1,
                limit: 10,
            },
            ArchiveOrgSearchRequest {
                query: "x\u{1b}".into(),
                page: 1,
                limit: 10,
            },
        ] {
            assert!(matches!(
                client.search(&request),
                Err(ProviderError::InvalidRequest(_))
            ));
        }
        assert!(transport.requests.lock().expect("requests").is_empty());
    }

    /// Choosing a playback encoding must not discard the user's original download option.
    #[test]
    fn download_variants_preserve_original_and_derivatives_in_stable_order() {
        let files = vec![
            json!({"name": "Звук/track.flac", "source": "original", "format": "Flac", "size": "9000"}),
            json!({"name": "Звук/track.mp3", "source": "derivative", "original": "Звук/track.flac", "format": "VBR MP3", "size": "1000"}),
            json!({"name": "Звук/track.opus", "source": "derivative", "original": "Звук/track.mp3", "format": "Ogg Opus", "size": "800"}),
        ];
        for offset in 0..files.len() {
            let mut reordered = files.clone();
            reordered.rotate_left(offset);
            let tracks = normalize_tracks("mock_audio", &reordered).expect("tracks");
            assert_eq!(tracks.len(), 1);
            assert_eq!(tracks[0].filename, "Звук/track.mp3");
            let variants = &tracks[0].download_variants;
            assert_eq!(
                variants
                    .iter()
                    .map(|item| item.filename.as_str())
                    .collect::<Vec<_>>(),
                ["Звук/track.flac", "Звук/track.opus", "Звук/track.mp3"]
            );
            assert_eq!(variants[0].provenance, ArchiveOrgFileProvenance::Original);
            assert_eq!(variants[1].provenance, ArchiveOrgFileProvenance::Derivative);
            assert_eq!(variants[2].size_bytes, Some(1000));
            assert!(variants.iter().all(|item| !item.is_video));
            assert_eq!(
                variants[0].download_url.as_str(),
                "https://archive.org/download/mock_audio/%D0%97%D0%B2%D1%83%D0%BA/track.flac"
            );
        }
    }

    /// A single original MP3 is not mislabeled as an Archive-generated alternative.
    #[test]
    fn download_variants_keep_original_mp3_and_unknown_provenance_distinct() {
        let tracks = normalize_tracks(
            "mock_audio",
            &[
                json!({"name": "original.mp3", "source": "original", "format": "MP3"}),
                json!({"name": "unknown.mp3", "source": "unexpected"}),
            ],
        )
        .expect("tracks");
        assert_eq!(tracks[0].download_variants.len(), 1);
        assert_eq!(
            tracks[0].download_variants[0].provenance,
            ArchiveOrgFileProvenance::Original
        );
        assert_eq!(tracks[1].download_variants.len(), 1);
        assert_eq!(
            tracks[1].download_variants[0].provenance,
            ArchiveOrgFileProvenance::Unknown
        );
        assert_eq!(tracks[1].download_variants[0].format, "MP3");
        assert_eq!(tracks[1].download_variants[0].size_bytes, None);
    }

    /// Conflicting ancestry cannot silently activate an original-file preference.
    #[test]
    fn download_variants_do_not_claim_original_provenance_for_inconsistent_metadata() {
        let tracks = normalize_tracks(
            "mock_audio",
            &[
                json!({"name": "root.flac", "source": "original"}),
                json!({"name": "misleading.mp3", "source": "original", "original": "root.flac"}),
                json!({"name": "malformed.mp3", "source": "original", "original": 42}),
            ],
        )
        .expect("tracks");
        for filename in ["misleading.mp3", "malformed.mp3"] {
            let variant = tracks
                .iter()
                .flat_map(|track| &track.download_variants)
                .find(|variant| variant.filename == filename)
                .expect("existing media");
            assert_eq!(
                variant.provenance,
                ArchiveOrgFileProvenance::Unknown,
                "{filename}"
            );
        }
        let serialized = serde_json::to_value(&tracks[0]).expect("snapshot");
        let restored: ArchiveOrgTrack = serde_json::from_value(serialized).expect("round trip");
        assert_eq!(restored, tracks[0]);
    }

    /// An accepted audio derivative exposes its uploaded video without adding movie-only tracks.
    #[test]
    fn download_variants_include_video_ancestors_only_for_accepted_audio_families() {
        let tracks = normalize_tracks("mock_audio", &[
            json!({"name": "recording.mov", "source": "original", "format": "QuickTime", "size": 9000}),
            json!({"name": "recording.mp4", "source": "derivative", "original": "recording.mov", "format": "MPEG4"}),
            json!({"name": "recording.mp3", "source": "derivative", "original": "recording.mp4", "format": "VBR MP3"}),
            json!({"name": "unrelated.mp4", "source": "original", "format": "MPEG4"}),
        ]).expect("tracks");
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].filename, "recording.mp3");
        let variants = &tracks[0].download_variants;
        assert_eq!(
            variants
                .iter()
                .map(|item| item.filename.as_str())
                .collect::<Vec<_>>(),
            ["recording.mov", "recording.mp4", "recording.mp3"]
        );
        assert!(variants[0].is_video);
        assert!(variants[1].is_video);
        assert!(!variants[2].is_video);
    }

    /// Existing path, restriction and ancestry gates apply to every alternative file.
    #[test]
    fn download_variants_exclude_nonmedia_unsafe_restricted_and_cyclic_records() {
        let tracks = normalize_tracks("mock_audio", &[
            json!({"name": "track.flac", "source": "original"}),
            json!({"name": "track.mp3", "source": "derivative", "original": "track.flac", "format": "VBR MP3"}),
            json!({"name": "track.png", "source": "derivative", "original": "track.flac", "format": "PNG"}),
            json!({"name": "track_spectrogram.png", "source": "derivative", "original": "track.flac"}),
            json!({"name": "analysis.mp3", "source": "derivative", "original": "track.flac", "format": "Audio Analysis"}),
            json!({"name": "../escape.mp3", "source": "derivative", "original": "track.flac"}),
            json!({"name": "https://evil.example/track.mp3", "source": "derivative", "original": "track.flac"}),
            json!({"name": "private.opus", "source": "derivative", "original": "track.flac", "private": true}),
            json!({"name": "private-child.mp3", "source": "derivative", "original": "private.opus"}),
            json!({"name": "disabled.wav", "source": "derivative", "original": "track.flac", "nodownload": true}),
            json!({"name": "cycle-a.mp3", "source": "derivative", "original": "cycle-b.mp3"}),
            json!({"name": "cycle-b.mp3", "source": "derivative", "original": "cycle-a.mp3"}),
        ]).expect("tracks");
        assert_eq!(tracks.len(), 1);
        assert_eq!(
            tracks[0]
                .download_variants
                .iter()
                .map(|item| item.filename.as_str())
                .collect::<Vec<_>>(),
            ["track.flac", "track.mp3"]
        );
    }

    /// A bounded menu fails explicitly instead of silently removing encodings.
    #[test]
    fn download_variants_reject_family_overflow_without_truncation() {
        let mut files = vec![json!({"name": "root.flac", "source": "original"})];
        for index in 0..MAX_DOWNLOAD_VARIANTS_PER_TRACK - 1 {
            files.push(json!({"name": format!("{index}.mp3"), "source": "derivative", "original": "root.flac"}));
        }
        let tracks = normalize_tracks("mock_audio", &files).expect("bounded family");
        assert_eq!(
            tracks[0].download_variants.len(),
            MAX_DOWNLOAD_VARIANTS_PER_TRACK
        );
        files
            .push(json!({"name": "overflow.mp3", "source": "derivative", "original": "root.flac"}));
        let error = normalize_tracks("mock_audio", &files).expect_err("oversized family");
        assert!(error.to_string().contains("too many download variants"));
    }

    /// A provider-generated native waveform, not an arbitrary PNG or item tile.
    fn waveform_fixture(filename: &str, original: &str) -> Value {
        json!({
            "name": filename, "source": "derivative", "format": "PNG",
            "original": original, "size": 10965, "width": 800, "height": 200,
        })
    }

    #[test]
    fn item_waveform_uses_original_family_despite_opus_playback_and_metadata_order() {
        let files = vec![
            json!({"name": "track.mp3", "source": "original"}),
            json!({"name": "preferred.opus", "source": "derivative", "original": "track.mp3"}),
            waveform_fixture("preferred.png", "preferred.opus"),
            waveform_fixture("track.png", "track.mp3"),
        ];
        for offset in 0..files.len() {
            let mut reordered = files.clone();
            reordered.rotate_left(offset);
            let (client, _) = mock_client(vec![bytes(&metadata(json!(reordered)))]);
            let details = client.item_details("mock_audio").unwrap();
            assert_eq!(details.tracks.len(), 1);
            assert_eq!(details.tracks[0].filename, "preferred.opus");
            assert_eq!(
                details.tracks[0].waveform_url.as_ref().map(Url::as_str),
                Some(
                    "https://iiif.archive.org/image/iiif/3/mock_audio%2Ftrack.png/full/max/0/default.jpg"
                )
            );
            assert_eq!(details.item.artwork_url, details.tracks[0].waveform_url);
        }
    }

    #[test]
    fn item_waveform_is_track_specific_and_item_uses_first_playable_track() {
        let (client, _) = mock_client(vec![bytes(&metadata(json!([
            {"name": "track10.mp3", "source": "original"},
            waveform_fixture("track10.png", "track10.mp3"),
            {"name": "track2.mp3", "source": "original"},
            waveform_fixture("track2.png", "track2.mp3")
        ])))]);
        let details = client.item_details("mock_audio").unwrap();
        assert_eq!(details.tracks[0].filename, "track2.mp3");
        assert_eq!(details.item.artwork_url, details.tracks[0].waveform_url);
        assert_ne!(
            details.tracks[0].waveform_url,
            details.tracks[1].waveform_url
        );
        assert!(
            details
                .tracks
                .iter()
                .all(|track| track.waveform_url.is_some())
        );
    }

    #[test]
    fn item_waveform_retains_real_cover_priority_and_old_snapshot_compatibility() {
        let (client, _) = mock_client(vec![bytes(&metadata(json!([
            {"name": "track.mp3", "source": "original"},
            waveform_fixture("track.png", "track.mp3"),
            {"name": "cover.jpg", "source": "original", "format": "JPEG", "size": 1024}
        ])))]);
        let details = client.item_details("mock_audio").unwrap();
        assert_eq!(
            details.item.artwork_url.as_ref().map(Url::as_str),
            Some("https://archive.org/download/mock_audio/cover.jpg")
        );
        assert!(
            details
                .tracks
                .iter()
                .all(|track| track.waveform_url.is_none())
        );
        let old: ArchiveOrgTrack = serde_json::from_value(json!({
            "filename": "track.mp3", "title": "Track", "download_url": "https://archive.org/download/mock_audio/track.mp3",
            "duration_seconds": null, "size_bytes": null
        })).unwrap();
        assert_eq!(old.waveform_url, None);
        assert!(old.download_variants.is_empty());
    }

    #[test]
    fn item_waveform_rejects_untrusted_unrelated_generated_images_and_oversized_metadata() {
        let valid = waveform_fixture("track.png", "track.mp3");
        for (field, value) in [
            ("name", json!("../track.png")),
            ("name", json!("nested/../track.png")),
            ("name", json!("https://evil.example/track.png")),
            ("name", json!("track\\bad.png")),
            ("name", json!("track.jpg")),
            ("name", json!("cover.png")),
            ("name", json!("track_spectrogram.png")),
            ("name", json!("__ia_thumb.png")),
            ("format", json!("Spectrogram")),
            ("format", json!("Item Tile")),
            ("format", Value::Null),
            ("source", json!("original")),
            ("source", Value::Null),
            ("private", json!(true)),
            ("nodownload", json!("true")),
            ("original", json!("other.mp3")),
            ("original", json!("../track.mp3")),
            ("original", json!("missing.mp3")),
            ("original", Value::Null),
            ("size", json!(0)),
            ("size", Value::Null),
            ("size", json!(MAX_COVER_BYTES + 1)),
            ("width", json!(MAX_COVER_DIMENSION + 1)),
            ("width", json!("invalid")),
            ("height", json!(0)),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value.clone();
            let (client, _) = mock_client(vec![bytes(&metadata(json!([
                {"name": "track.mp3", "source": "original"}, invalid
            ])))]);
            let details = client.item_details("mock_audio").unwrap();
            assert!(
                details.tracks[0].waveform_url.is_none(),
                "accepted {field}={value}"
            );
            assert_eq!(
                details.item.artwork_url.as_ref().map(Url::as_str),
                Some("https://archive.org/services/img/mock_audio")
            );
        }
        let mut oversized = valid;
        oversized["width"] = json!(4000);
        oversized["height"] = json!(4000);
        let (client, _) = mock_client(vec![bytes(&metadata(json!([
            {"name": "track.mp3", "source": "original"}, oversized
        ])))]);
        assert!(
            client.item_details("mock_audio").unwrap().tracks[0]
                .waveform_url
                .is_none()
        );
    }

    #[test]
    fn item_waveform_traverses_bounded_ancestry_without_crossing_restricted_or_cyclic_files() {
        let (client, _) = mock_client(vec![bytes(&metadata(json!([
            {"name": "track.wav", "source": "original"},
            {"name": "track.mp3", "source": "derivative", "original": "track.wav"},
            {"name": "preferred.opus", "source": "derivative", "original": "track.mp3"},
            waveform_fixture("track.png", "track.mp3"),
            {"name": "private.mp3", "source": "derivative", "original": "track.wav", "private": true},
            waveform_fixture("private.png", "private.mp3"),
            {"name": "cycle.mp3", "original": "cycle2.mp3"},
            {"name": "cycle2.mp3", "original": "cycle.mp3"},
            waveform_fixture("cycle.png", "cycle.mp3")
        ])))]);
        let details = client.item_details("mock_audio").unwrap();
        assert_eq!(details.tracks.len(), 1);
        assert_eq!(details.tracks[0].filename, "preferred.opus");
        assert_eq!(
            details.tracks[0].waveform_url.as_ref().map(Url::as_str),
            Some(
                "https://iiif.archive.org/image/iiif/3/mock_audio%2Ftrack.png/full/max/0/default.jpg"
            )
        );
    }

    #[test]
    fn item_waveform_encodes_unicode_path_as_one_explicit_iiif_file_component() {
        let (client, _) = mock_client(vec![bytes(&metadata(json!([
            {"name": "Звук/01 ?#%.mp3", "source": "original"},
            waveform_fixture("Звук/01 ?#%.png", "Звук/01 ?#%.mp3")
        ])))]);
        let details = client.item_details("mock_audio").unwrap();
        let url = details.tracks[0].waveform_url.as_ref().expect("waveform");
        assert_eq!(
            url.as_str(),
            "https://iiif.archive.org/image/iiif/3/mock_audio%2F%D0%97%D0%B2%D1%83%D0%BA%2F01%20%3F%23%25.png/full/max/0/default.jpg"
        );
        assert!(url.query().is_none() && url.fragment().is_none());
        assert_eq!(url.host_str(), Some("iiif.archive.org"));
    }

    #[test]
    fn item_waveform_does_not_substitute_later_tracks_and_bounds_derivative_depth() {
        let (client, _) = mock_client(vec![bytes(&metadata(json!([
            {"name": "track1.mp3", "source": "original"},
            {"name": "track2.mp3", "source": "original"},
            waveform_fixture("track2.png", "track2.mp3")
        ])))]);
        let details = client.item_details("mock_audio").unwrap();
        assert_eq!(details.tracks[0].waveform_url, None);
        assert!(details.tracks[1].waveform_url.is_some());
        assert_eq!(
            details.item.artwork_url.as_ref().map(Url::as_str),
            Some("https://archive.org/services/img/mock_audio")
        );

        let mut files = vec![json!({"name": "root.wav", "source": "original"})];
        let mut previous = "root.wav".to_owned();
        for depth in 0..MAX_DERIVATIVE_DEPTH {
            let filename = format!("depth{depth}.mp3");
            files.push(json!({"name": filename, "source": "derivative", "original": previous}));
            previous = filename;
        }
        files.push(waveform_fixture(
            &format!("depth{}.png", MAX_DERIVATIVE_DEPTH - 1),
            &previous,
        ));
        let (client, _) = mock_client(vec![bytes(&metadata(json!(files)))]);
        let details = client.item_details("mock_audio").unwrap();
        assert_eq!(details.tracks.len(), 1);
        assert_eq!(details.tracks[0].waveform_url, None);
    }

    #[test]
    fn derivatives_are_deduplicated_and_inherit_track_metadata() {
        let (client, _) = mock_client(vec![bytes(&metadata(json!([
            {"name": "disc/track10.flac", "source": "original", "title": "Final", "track": "10/10", "length": "01:02:03.9", "size": "1000"},
            {"name": "disc/track2.wav", "source": "original", "title": "Second", "track": "2/10", "length": "62.9", "size": "900"},
            {"name": "disc/track2.mp3", "original": "disc/track2.wav", "format": "VBR MP3", "size": "123"},
            {"name": "disc/track2_low.mp3", "original": "disc/track2.mp3", "format": "64Kbps MP3"},
            {"name": "disc/track2.ogg", "original": "disc/track2.wav", "format": "Ogg Vorbis"},
            {"name": "cover.jpg", "size": "12"}
        ])))]);
        let details = client.item_details("mock_audio").expect("details");
        assert_eq!(details.tracks.len(), 2);
        assert_eq!(details.tracks[0].filename, "disc/track2.mp3");
        assert_eq!(details.tracks[0].title, "Second");
        assert_eq!(details.tracks[0].duration_seconds, Some(62));
        assert_eq!(details.tracks[0].size_bytes, Some(123));
        assert_eq!(details.tracks[1].duration_seconds, Some(3723));
    }

    #[test]
    fn filenames_are_naturally_sorted_and_encoded_without_changing_origin() {
        let (client, _) = mock_client(vec![bytes(&metadata(json!([
            {"name": "track10.mp3"}, {"name": "track2.mp3"},
            {"name": "nested/track1 ?#%.opus"},
            {"name": "../escape.mp3"}, {"name": "/absolute.mp3"},
            {"name": "nested/../escape.mp3"}, {"name": "a\\bad.mp3"},
            {"name": "https://evil.example/audio.mp3"}
        ])))]);
        let details = client.item_details("mock_audio").expect("details");
        assert_eq!(details.tracks.len(), 3);
        assert_eq!(details.tracks[1].filename, "track2.mp3");
        assert_eq!(details.tracks[2].filename, "track10.mp3");
        let url = &details.tracks[0].download_url;
        assert_eq!(
            url.as_str(),
            "https://archive.org/download/mock_audio/nested/track1%20%3F%23%25.opus"
        );
        assert!(url.query().is_none());
        assert!(url.fragment().is_none());
    }

    #[test]
    fn restricted_items_and_tracks_are_never_playable() {
        for flag in [
            "is_dark",
            "nodownload",
            "access-restricted-item",
            "access-restricted",
            "private",
        ] {
            let mut value = metadata(json!([{ "name": "audio.mp3" }]));
            value[flag] = json!("true");
            let (client, transport) = mock_client(vec![bytes(&value)]);
            assert!(client.item_details("mock_audio").is_err(), "{flag}");
            assert_eq!(transport.requests.lock().expect("requests").len(), 1);
        }
        let (client, _) = mock_client(vec![bytes(&metadata(json!([
            {"name": "private.wav", "private": "true"},
            {"name": "private.mp3", "original": "private.wav"},
            {"name": "no.mp3", "nodownload": true},
            {"name": "yes.mp3", "private": "false"}
        ])))]);
        let details = client.item_details("mock_audio").expect("public track");
        assert_eq!(details.tracks.len(), 1);
        assert_eq!(details.tracks[0].filename, "yes.mp3");
    }

    #[test]
    fn metadata_and_public_page_expose_attribution_without_uploader_email() {
        let mut value = metadata(json!([{ "name": "audio.mp3", "size": "100" }]));
        value["metadata"]["uploader"] = json!("private@example.com");
        value["metadata"]["collection"] = json!(["audio", "mock_collection", "../invalid"]);
        value["metadata"]["publicdate"] = json!("2024-01-02 03:04:05");
        value["metadata"]["date"] = json!("2001");
        value["item_size"] = json!(1234);
        value["created"] = json!(1_999_999_999);
        let page = br#"<section><p class='favorite-count'><span class='item-stats-summary__count'>1,234</span>Favorites</p><p itemprop='interactionStatistic'><span itemprop='userInteractionCount'>5,678</span>Views</p><a class='item-upload-info__uploader-name' href='/details/@public_user'>Public &amp; user</a><a href='/details/mock_collection' class='collection-item'><span>Mock collection title</span><img src='https://evil.example/a'></a></section>"#.to_vec();
        let (client, transport) = mock_client(vec![bytes(&value), page]);
        let details = client.item_details("mock_audio").expect("details");
        assert_eq!(details.item.favorite_count, Some(1234));
        assert_eq!(details.item.download_count, Some(5678));
        assert_eq!(details.item.review_count, Some(0));
        assert_eq!(details.item.size_bytes, Some(1234));
        assert_eq!(details.item.uploaded_at, Some(1_704_164_645));
        assert_eq!(details.item.published_at, Some(978_307_200));
        assert_eq!(
            details.item.uploader.as_ref().expect("uploader").name,
            "Public & user"
        );
        assert_eq!(
            details
                .item
                .uploader
                .as_ref()
                .expect("uploader")
                .url
                .as_str(),
            "https://archive.org/details/@public_user"
        );
        assert_eq!(details.item.collections.len(), 2);
        assert_eq!(details.item.collections[1].name, "Mock collection title");
        assert_eq!(transport.requests.lock().expect("requests").len(), 2);
    }

    #[test]
    fn page_failure_or_unsafe_links_do_not_invent_uploader_or_counts() {
        let mut value = metadata(json!([{ "name": "audio.mp3" }]));
        value["metadata"]["uploader"] = json!("person@example.com");
        for page in [None, Some(br#"<a class='item-upload-info__uploader-name' href='https://evil.example/details/@user'>User</a><p class='favorite-count'><span>unknown</span></p>"#.to_vec())] {
			let mut responses = vec![bytes(&value)];
			responses.extend(page);
			let (client, _) = mock_client(responses);
			let details = client.item_details("mock_audio").expect("metadata remains useful");
			assert_eq!(details.item.uploader, None);
			assert_eq!(details.item.favorite_count, None);
			assert_eq!(details.item.download_count, None);
		}
    }

    #[test]
    fn reviews_are_bounded_sanitized_and_stars_are_not_likes() {
        let mut value = metadata(json!([{ "name": "audio.mp3" }]));
        value["reviews"] = json!((0..25).map(|index| json!({
			"reviewer": format!("Reader {index}"), "reviewer_itemname": "@reader",
			"reviewtitle": "Good &amp; clear", "reviewbody": "<p>Hello<br>world</p><script>hidden</script>\u{1b}",
			"stars": "5", "createdate": "2024-01-02 03:04:05", "reviewdate": "2024-01-03 03:04:05"
		})).collect::<Vec<_>>());
        let (client, _) = mock_client(vec![bytes(&value)]);
        let details = client.item_details("mock_audio").expect("reviews");
        assert_eq!(details.item.review_count, Some(25));
        assert_eq!(details.comments.len(), 20);
        let comment = &details.comments[0];
        assert_eq!(
            comment
                .author_channel_url
                .as_ref()
                .expect("explicit profile")
                .as_str(),
            "https://archive.org/details/@reader"
        );
        assert_eq!(comment.like_count, 0);
        assert!(comment.text.contains("5/5 stars"));
        assert!(comment.text.contains("Hello\nworld"));
        assert!(!comment.text.contains("hidden"));
        assert!(!comment.text.contains('\u{1b}'));
        assert_eq!(comment.published_at, Some(1_704_164_645));
        assert_eq!(comment.updated_at, Some(1_704_251_045));
    }

    #[test]
    fn absent_unknown_and_overflowing_sizes_are_not_fake_zeroes() {
        for (files, expected) in [
            (
                json!([{ "name": "a.mp3", "size": "12" }, { "name": "cover.jpg", "size": 8 }]),
                Some(20),
            ),
            (json!([{ "name": "a.mp3" }]), None),
            (
                json!([{ "name": "a.mp3", "size": u64::MAX }, { "name": "cover.jpg", "size": 1 }]),
                None,
            ),
        ] {
            let (client, _) = mock_client(vec![bytes(&metadata(files))]);
            assert_eq!(
                client
                    .item_details("mock_audio")
                    .expect("details")
                    .item
                    .size_bytes,
                expected
            );
        }
    }

    #[test]
    fn malformed_oversized_and_identifier_mismatched_payloads_are_rejected() {
        for payload in [
            b"not json".to_vec(),
            bytes(&json!([])),
            bytes(&json!({"error": "Unavailable"})),
            bytes(&json!({"metadata": {"identifier": "other", "mediatype": "audio"}, "files": []})),
            vec![b' '; MAX_ITEM_JSON_BYTES + 1],
        ] {
            let (client, _) = mock_client(vec![payload]);
            assert!(client.item_details("mock_audio").is_err());
        }
    }

    #[test]
    fn derivative_cycles_do_not_hang_or_expose_tracks() {
        let (client, _) = mock_client(vec![bytes(&metadata(json!([
            {"name": "a.mp3", "original": "b.mp3"},
            {"name": "b.mp3", "original": "a.mp3"},
            {"name": "normal.mp3"}
        ])))]);
        let details = client.item_details("mock_audio").expect("details");
        assert_eq!(details.tracks.len(), 1);
        assert_eq!(details.tracks[0].filename, "normal.mp3");
    }
}
