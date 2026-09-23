//! Bounded `SoundCloud` metadata and proxied audio through a configurable Soundcloak instance.
//!
//! Public `SoundCloud` permalinks remain the durable identity. Instance URLs and
//! search continuation tokens are ephemeral; signed CDN playback URLs are never
//! exposed by this adapter. Protocol: <https://git.maid.zone/stuff/soundcloak/src/branch/master/docs/API.md>.

use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use super::{DEFAULT_MAX_JSON_BYTES, DEFAULT_REQUEST_TIMEOUT, ProviderError, validate_base_url};

mod catalog;
pub use catalog::{
    SoundcloakAlbum, SoundcloakAlbumDetails, SoundcloakAlbumTrack, SoundcloakAlbumTrackPage,
    SoundcloakArtist, SoundcloakArtistAlbumPage, SoundcloakArtistTrackPage,
    SoundcloakCatalogCursor,
};

const MAX_QUERY_BYTES: usize = 512;
const MAX_QUERY_URN_BYTES: usize = 512;
const MAX_URL_BYTES: usize = 4_096;
const MAX_LABEL_BYTES: usize = 1_024;
const MAX_DESCRIPTION_BYTES: usize = 64 * 1_024;
const MAX_TAG_LIST_BYTES: usize = 8_192;
const MAX_TAG_BYTES: usize = 256;
const MAX_TAGS: usize = 64;
const MAX_COMMENTS: usize = 20;
const MAX_COMMENT_BYTES: usize = 16_384;
const MAX_DURATION_MILLIS: u64 = 7 * 24 * 60 * 60 * 1_000;
const MAX_PREVIEW_MILLIS: u64 = 60_000;
const MAX_SEARCH_PAGE: u32 = 1_000;

/// Public default; users may explicitly configure another trusted instance.
pub const DEFAULT_SOUNDCLOAK_INSTANCE: &str = "https://sc1.maid.zone/";

/// One bounded offset page with an optional provider-issued search-session token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SoundcloakSearchRequest {
    /// User-entered search text, encoded as one query value.
    pub query: String,
    /// One-based page, bounded to prevent offset overflow.
    pub page: u32,
    /// Results per page; keep fixed across one result set.
    pub limit: usize,
    /// Validated token from the previous page, never a URL or client credential.
    pub query_urn: Option<String>,
}

impl SoundcloakSearchRequest {
    /// Validates the bounded query/page contract before contacting an instance.
    ///
    /// # Errors
    /// Rejects empty, oversized or control-bearing queries, invalid page sizes and tokens.
    pub fn validate(&self) -> Result<(), ProviderError> {
        if safe_label(&self.query, MAX_QUERY_BYTES).is_none()
            || !(1..=MAX_SEARCH_PAGE).contains(&self.page)
            || !(1..=100).contains(&self.limit)
            || self
                .query_urn
                .as_deref()
                .is_some_and(|token| !valid_query_urn(token))
        {
            return Err(ProviderError::InvalidRequest(
                "invalid Soundcloak search query, page, limit or continuation token".into(),
            ));
        }
        Ok(())
    }
}

/// Public playback access advertised by metadata, without attempting to unlock restrictions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum SoundcloakPlayback {
    /// An explicitly allowed, unencrypted full-length HLS rendition exists.
    Full,
    /// Only an explicitly advertised, bounded public progressive preview is playable.
    Preview,
    /// No supported public rendition is available; no playback endpoint should be used.
    #[default]
    Unavailable,
}

/// Canonical `SoundCloud` metadata; no signed or third-party playback locator is retained.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SoundcloakTrack {
    /// `SoundCloud`'s numeric public track identifier, not the queue replay identity.
    pub id: String,
    /// Terminal-safe track title.
    pub title: String,
    /// Terminal-safe public artist display name.
    pub artist: String,
    /// Canonical public uploader profile, never derived from its display name.
    #[serde(default)]
    pub artist_url: Option<Url>,
    /// Canonical public `SoundCloud` permalink used for queue, playlists and History.
    pub webpage_url: Url,
    /// Artwork routed through the configured instance, when valid CDN artwork exists.
    pub artwork_url: Option<Url>,
    /// Larger artwork locator for explicit expansion only; constructing it does not fetch it.
    #[serde(default)]
    pub expanded_artwork_url: Option<Url>,
    /// Playable duration in whole seconds; previews never inherit the full track length.
    pub duration_seconds: Option<u64>,
    /// Full work's separately reported duration, including when playback is preview-only.
    #[serde(default)]
    pub full_duration_seconds: Option<u64>,
    /// Bounded terminal-safe description, when reported.
    pub description: Option<String>,
    /// True only when either the full track or an explicitly labeled public preview is playable.
    pub streamable: bool,
    /// Admission and endpoint policy; callers must retain the preview/full distinction.
    #[serde(default)]
    pub playback: SoundcloakPlayback,
    /// Public like count, including a known zero, or unknown when absent/invalid.
    #[serde(default)]
    pub likes_count: Option<u64>,
    /// Public play count, including a known zero, or unknown when absent/invalid.
    #[serde(default)]
    pub playback_count: Option<u64>,
    /// Public repost count, including a known zero, or unknown when absent/invalid.
    #[serde(default)]
    pub reposts_count: Option<u64>,
    /// Public comment count, distinct from the bounded comments fetched on demand.
    #[serde(default)]
    pub comment_count: Option<u64>,
    /// Validated creation timestamp normalized to RFC 3339 UTC.
    #[serde(default)]
    pub created_at: Option<String>,
    /// Validated modification timestamp normalized to RFC 3339 UTC.
    #[serde(default)]
    pub last_modified: Option<String>,
    /// Provider-reported license label; it is not a grant inferred by this adapter.
    #[serde(default)]
    pub license: Option<String>,
    /// Bounded tags, retaining quoted multiword groups and commas within each tag.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Public genre label, suitable for [`SoundcloakClient::genre_url`].
    #[serde(default)]
    pub genre: Option<String>,
}

/// One bounded public comment; markup is retained as literal text, never executable HTML.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SoundcloakComment {
    /// Terminal-safe public author display name.
    pub author: String,
    /// Canonical public author profile when the response advertises one safely.
    #[serde(default)]
    pub author_url: Option<Url>,
    /// Bounded plain body with normalized line endings and no terminal controls.
    pub body: String,
    /// Validated publication timestamp normalized to RFC 3339 UTC, when available.
    pub created_at: Option<String>,
    /// Optional position within the track, converted from milliseconds.
    pub timestamp_seconds: Option<u64>,
}

/// Normalized bounded results and an instance-independent continuation token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SoundcloakSearchPage {
    /// Valid track records, retaining upstream order.
    pub items: Vec<SoundcloakTrack>,
    /// Requested page number.
    pub page: u32,
    /// Next offset page only when the upstream continuation validates.
    pub next_page: Option<u32>,
    /// Validated search-session token to reuse with the next page.
    pub query_urn: Option<String>,
}

/// Injectable bounded metadata transport; production requests stay on the configured origin.
pub trait SoundcloakTransport: Send + Sync {
    /// Reads a single metadata response without following redirects.
    ///
    /// # Errors
    /// Returns a provider error for HTTP, transport or body-limit failures.
    fn fetch(&self, url: &Url, max_bytes: usize) -> Result<Vec<u8>, ProviderError>;
}

/// Blocking client; callers should run metadata requests on their provider worker.
#[derive(Clone)]
pub struct SoundcloakClient {
    base_url: Url,
    transport: Arc<dyn SoundcloakTransport>,
}

impl Default for SoundcloakClient {
    fn default() -> Self {
        Self::new(Url::parse(DEFAULT_SOUNDCLOAK_INSTANCE).expect("fixed Soundcloak instance URL"))
            .expect("fixed Soundcloak instance is a valid base URL")
    }
}

impl SoundcloakClient {
    /// Creates a redirect-free bounded metadata client for an explicitly trusted instance.
    ///
    /// HTTP and private-network instances are allowed only through this manual
    /// configuration, matching other self-hosted provider adapters.
    ///
    /// # Errors
    /// Rejects invalid, credential-bearing, oversized or ambiguous base URLs.
    pub fn new(base_url: Url) -> Result<Self, ProviderError> {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(DEFAULT_REQUEST_TIMEOUT))
            .max_redirects(0)
            .http_status_as_error(false)
            .user_agent(concat!("youta/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Self::with_transport(base_url, Arc::new(UreqSoundcloakTransport { agent }))
    }

    /// Installs an alternative bounded transport for deterministic offline callers/tests.
    ///
    /// # Errors
    /// Rejects an unsafe configured base before the transport can be contacted.
    pub fn with_transport(
        base_url: Url,
        transport: Arc<dyn SoundcloakTransport>,
    ) -> Result<Self, ProviderError> {
        if base_url.as_str().len() > 2_048 {
            return Err(ProviderError::InvalidBaseUrl(
                "Soundcloak base URL is too long".into(),
            ));
        }
        Ok(Self {
            base_url: validate_base_url(base_url)?,
            transport,
        })
    }

    /// Returns the normalized configured instance.
    #[must_use]
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// Searches through Soundcloak, reconstructing pagination without following upstream URLs.
    ///
    /// # Errors
    /// Returns an error for invalid requests, transport failures, oversized JSON or bad envelopes.
    pub fn search(
        &self,
        request: &SoundcloakSearchRequest,
    ) -> Result<SoundcloakSearchPage, ProviderError> {
        self.search_filtered(request, None)
    }

    /// Shares bounded parsing/pagination while keeping ordinary full-text requests unchanged.
    fn search_filtered(
        &self,
        request: &SoundcloakSearchRequest,
        tag: Option<&str>,
    ) -> Result<SoundcloakSearchPage, ProviderError> {
        request.validate()?;
        let mut endpoint = self.endpoint("_/api/v2/search/tracks")?;
        let offset = u64::from(request.page - 1)
            * u64::try_from(request.limit).map_err(|_| {
                ProviderError::InvalidRequest("Soundcloak page size overflows".into())
            })?;
        {
            let mut query = endpoint.query_pairs_mut();
            query.append_pair("q", request.query.trim());
            query.append_pair("limit", &request.limit.to_string());
            query.append_pair("offset", &offset.to_string());
            query.append_pair("linked_partitioning", "1");
            if let Some(tag) = tag {
                query.append_pair("filter.genre_or_tag", tag);
                query.append_pair("sort", "popular");
            }
            if let Some(token) = &request.query_urn {
                query.append_pair("query_urn", token);
            }
        }
        let value = self.fetch_json(&endpoint)?;
        let collection = value["collection"]
            .as_array()
            .ok_or_else(|| invalid_response("missing track collection"))?;
        if collection.len() > request.limit {
            return Err(invalid_response("too many tracks in search page"));
        }
        let (next_page, query_urn) = continuation(&value, request, tag)?;
        let items = collection
            .iter()
            .filter_map(|value| self.normalize_track(value))
            .collect();
        Ok(SoundcloakSearchPage {
            items,
            page: request.page,
            next_page,
            query_urn,
        })
    }

    /// Searches tracks tagged with an exact bounded tag using Soundcloak's popular-tag route.
    ///
    /// Supply `query: "*"` in the unchanged search request; page size, numeric
    /// offsets and validated search-session tokens retain their usual semantics.
    ///
    /// # Errors
    /// Rejects invalid tags, non-wildcard queries, changed filters and ordinary search failures.
    pub fn search_tag(
        &self,
        request: &SoundcloakSearchRequest,
        tag: &str,
    ) -> Result<SoundcloakSearchPage, ProviderError> {
        let tag = safe_label(tag, MAX_TAG_BYTES)
            .filter(|_| request.query.trim() == "*")
            .ok_or_else(|| {
                ProviderError::InvalidRequest(
                    "tag search requires a bounded tag and wildcard query".into(),
                )
            })?;
        self.search_filtered(request, Some(&tag))
    }

    /// Resolves a canonical public track without retaining a CDN playback locator.
    ///
    /// # Errors
    /// Rejects non-track URLs, invalid metadata, mismatched identity and network failures.
    pub fn resolve(&self, canonical_url: &Url) -> Result<SoundcloakTrack, ProviderError> {
        canonical_segments(canonical_url)?;
        let mut endpoint = self.endpoint("_/api/v2/resolve")?;
        endpoint
            .query_pairs_mut()
            .append_pair("url", canonical_url.as_str());
        let value = self.fetch_json(&endpoint)?;
        let track = self
            .normalize_track(&value)
            .ok_or_else(|| invalid_response("invalid resolved track"))?;
        if track.webpage_url != *canonical_url {
            return Err(invalid_response(
                "resolved track identity does not match requested permalink",
            ));
        }
        Ok(track)
    }

    /// Constructs the instance-owned HLS endpoint without initiating playback or a fetch.
    ///
    /// Explicit flags request AAC and same-instance manifests/segments. The
    /// chosen instance must support stream proxying to honor the segment flag.
    ///
    /// # Errors
    /// Rejects anything other than a canonical two-segment public `SoundCloud` track URL.
    pub fn stream_url(&self, canonical_url: &Url) -> Result<Url, ProviderError> {
        let (artist, track) = canonical_segments(canonical_url)?;
        let mut url = self.endpoint(&format!("_/api/hls/{artist}/{track}"))?;
        url.query_pairs_mut()
            .append_pair("audio", "aac")
            .append_pair("redirect", "false")
            .append_pair("redirect_parts", "false");
        Ok(url)
    }

    /// Constructs the ordinary public progressive endpoint for an admitted preview.
    ///
    /// This does not request full-track access or follow a signed CDN redirect.
    /// Callers must use it only after metadata explicitly advertises a preview.
    ///
    /// # Errors
    /// Rejects noncanonical public track URLs before constructing the instance endpoint.
    pub fn preview_stream_url(&self, canonical_url: &Url) -> Result<Url, ProviderError> {
        let (artist, track) = canonical_segments(canonical_url)?;
        let mut url = self.endpoint(&format!("_/api/progressive/{artist}/{track}"))?;
        url.query_pairs_mut().append_pair("redirect", "false");
        Ok(url)
    }

    /// Chooses only the endpoint explicitly supported by normalized public metadata.
    ///
    /// # Errors
    /// Rejects unavailable tracks, inconsistent admission state and invalid canonical URLs.
    pub fn playback_url(&self, track: &SoundcloakTrack) -> Result<Url, ProviderError> {
        if !track.streamable {
            return Err(ProviderError::InvalidRequest(
                "SoundCloud track is unavailable".into(),
            ));
        }
        match track.playback {
            SoundcloakPlayback::Full => self.stream_url(&track.webpage_url),
            SoundcloakPlayback::Preview => self.preview_stream_url(&track.webpage_url),
            SoundcloakPlayback::Unavailable => Err(ProviderError::InvalidRequest(
                "SoundCloud track is unavailable".into(),
            )),
        }
    }

    /// Maps a canonical track to its human-readable page on the configured instance.
    ///
    /// # Errors
    /// Rejects noncanonical or non-track `SoundCloud` URLs before constructing the instance link.
    pub fn page_url(&self, canonical_url: &Url) -> Result<Url, ProviderError> {
        let (artist, track) = canonical_segments(canonical_url)?;
        self.endpoint(&format!("{artist}/{track}"))
    }

    /// Links a genre to the configured instance, encoding it as exactly one path segment.
    ///
    /// # Errors
    /// Rejects empty, oversized, control-bearing or dot-segment labels.
    pub fn genre_url(&self, genre: &str) -> Result<Url, ProviderError> {
        let genre = safe_label(genre, MAX_TAG_BYTES)
            .filter(|genre| !matches!(genre.as_str(), "." | ".."))
            .ok_or_else(|| ProviderError::InvalidRequest("invalid SoundCloud genre".into()))?;
        let mut url = self.endpoint("tags/")?;
        url.path_segments_mut()
            .map_err(|()| {
                ProviderError::InvalidBaseUrl("invalid Soundcloak genre endpoint".into())
            })?
            .pop_if_empty()
            .push(&genre);
        Ok(url)
    }

    /// Fetches at most twenty recent public top-level comments, without following continuation.
    ///
    /// Unsupported/disabled API responses remain ordinary provider errors. No signed
    /// URLs, private comments, user profiles or author images are requested.
    ///
    /// # Errors
    /// Rejects invalid numeric identifiers, oversized/malformed envelopes and HTTP failures.
    pub fn comments(&self, track_id: &str) -> Result<Vec<SoundcloakComment>, ProviderError> {
        if track_id.len() > 20
            || !track_id.bytes().all(|byte| byte.is_ascii_digit())
            || !track_id.parse::<u64>().is_ok_and(|id| id > 0)
        {
            return Err(ProviderError::InvalidRequest(
                "invalid SoundCloud track identifier".into(),
            ));
        }
        let mut endpoint = self.endpoint(&format!("_/api/v2/tracks/{track_id}/comments"))?;
        endpoint
            .query_pairs_mut()
            .append_pair("limit", &MAX_COMMENTS.to_string())
            .append_pair("threaded", "0")
            .append_pair("filter_replies", "1")
            .append_pair("sort", "created_at");
        let value = self.fetch_json(&endpoint)?;
        let collection = value["collection"]
            .as_array()
            .ok_or_else(|| invalid_response("missing comment collection"))?;
        if collection.len() > MAX_COMMENTS {
            return Err(invalid_response("too many comments in response"));
        }
        Ok(collection.iter().filter_map(normalize_comment).collect())
    }

    /// Joins only adapter-owned paths onto the manually configured base.
    fn endpoint(&self, path: &str) -> Result<Url, ProviderError> {
        self.base_url
            .join(path)
            .map_err(|_| ProviderError::InvalidBaseUrl("invalid Soundcloak endpoint".into()))
    }

    /// Applies the response byte limit even to injected transports before JSON allocation.
    fn fetch_json(&self, url: &Url) -> Result<Value, ProviderError> {
        let bytes = self.transport.fetch(url, DEFAULT_MAX_JSON_BYTES)?;
        if bytes.len() > DEFAULT_MAX_JSON_BYTES {
            return Err(ProviderError::ResponseTooLarge {
                limit: DEFAULT_MAX_JSON_BYTES,
            });
        }
        serde_json::from_slice(&bytes).map_err(|_| invalid_response("malformed JSON metadata"))
    }

    /// Converts display fields only, never retaining authorization or signed transcoding data.
    fn normalize_track(&self, value: &Value) -> Option<SoundcloakTrack> {
        if value["kind"].as_str() != Some("track") {
            return None;
        }
        let id = value["id"].as_u64().filter(|id| *id > 0)?.to_string();
        let title = safe_label(value["title"].as_str()?, MAX_LABEL_BYTES)?;
        let artist = safe_label(value["user"]["username"].as_str()?, MAX_LABEL_BYTES)?;
        let raw_url = value["permalink_url"].as_str()?;
        if raw_url.len() > MAX_URL_BYTES {
            return None;
        }
        let webpage_url = Url::parse(raw_url).ok()?;
        canonical_segments(&webpage_url).ok()?;
        let artwork_url = value["artwork_url"]
            .as_str()
            .and_then(|raw| self.artwork_url(raw, "t500x500"));
        let expanded_artwork_url = value["artwork_url"]
            .as_str()
            .and_then(|raw| self.artwork_url(raw, "t1080x1080"));
        let duration = value
            .get("full_duration")
            .filter(|value| !value.is_null())
            .or_else(|| value.get("duration").filter(|value| !value.is_null()));
        let mut full_duration_seconds = if let Some(duration) = duration {
            Some(
                duration
                    .as_u64()
                    .filter(|milliseconds| *milliseconds <= MAX_DURATION_MILLIS)?
                    / 1_000,
            )
        } else {
            None
        };
        let description = value["description"]
            .as_str()
            .and_then(|text| safe_multiline(text, MAX_DESCRIPTION_BYTES));
        let (playback, preview_seconds) = playback_admission(value);
        let duration_seconds = if playback == SoundcloakPlayback::Preview {
            if value["full_duration"].is_null() {
                full_duration_seconds = None;
            }
            preview_seconds
        } else {
            full_duration_seconds
        };
        Some(SoundcloakTrack {
            id,
            title,
            artist,
            artist_url: catalog::user_profile_url(&value["user"]),
            webpage_url,
            artwork_url,
            expanded_artwork_url,
            duration_seconds,
            full_duration_seconds,
            description,
            streamable: playback != SoundcloakPlayback::Unavailable,
            playback,
            likes_count: value["likes_count"].as_u64(),
            playback_count: value["playback_count"].as_u64(),
            reposts_count: value["reposts_count"].as_u64(),
            comment_count: value["comment_count"].as_u64(),
            created_at: normalized_timestamp(&value["created_at"]),
            last_modified: normalized_timestamp(&value["last_modified"]),
            license: value["license"]
                .as_str()
                .and_then(|text| safe_label(text, 128)),
            tags: value["tag_list"]
                .as_str()
                .map(parse_tags)
                .unwrap_or_default(),
            genre: value["genre"]
                .as_str()
                .and_then(|text| safe_label(text, MAX_TAG_BYTES)),
        })
    }

    /// Proxies only bounded credential-free HTTPS artwork on `SoundCloud`'s own image CDN.
    fn artwork_url(&self, raw: &str, variant: &str) -> Option<Url> {
        if raw.len() > MAX_URL_BYTES {
            return None;
        }
        let mut artwork = Url::parse(raw).ok()?;
        if artwork.scheme() != "https"
            || artwork.port().is_some()
            || !artwork.username().is_empty()
            || artwork.password().is_some()
            || artwork.fragment().is_some()
            || artwork.query().is_some()
            || !artwork
                .host_str()
                .is_some_and(|host| host.ends_with(".sndcdn.com"))
            || crate::domain::remote_url_has_non_public_host(&artwork)
        {
            return None;
        }
        resize_known_artwork(&mut artwork, variant);
        let mut url = self.endpoint("_/proxy/images").ok()?;
        url.query_pairs_mut().append_pair("url", artwork.as_str());
        Some(url)
    }
}

/// Admits only supported public renditions; preview duration comes from the actual snippet.
fn playback_admission(value: &Value) -> (SoundcloakPlayback, Option<u64>) {
    let unavailable = (SoundcloakPlayback::Unavailable, None);
    if value["streamable"].as_bool() != Some(true)
        || value["sharing"]
            .as_str()
            .is_some_and(|sharing| sharing != "public")
    {
        return unavailable;
    }
    let Some(transcodings) = value["media"]["transcodings"]
        .as_array()
        .filter(|items| items.len() <= 32)
    else {
        return unavailable;
    };
    for transcoding in transcodings {
        let format = &transcoding["format"];
        if !format["mime_type"]
            .as_str()
            .is_some_and(|mime| mime.starts_with("audio/"))
        {
            continue;
        }
        if value["policy"].as_str() == Some("ALLOW")
            && format["protocol"].as_str() == Some("hls")
            && matches!(
                transcoding.get("snipped"),
                None | Some(Value::Null | Value::Bool(false))
            )
        {
            return (SoundcloakPlayback::Full, None);
        }
        if value["policy"].as_str() == Some("SNIP")
            && format["protocol"].as_str() == Some("progressive")
            && format["mime_type"].as_str() == Some("audio/mpeg")
            && transcoding["snipped"].as_bool() == Some(true)
        {
            // SoundCloud's preview metadata normally reports 30,000 ms in both
            // places. Never infer its length from the full work, nor admit an
            // unbounded rendition as a snippet when duration metadata is absent.
            let duration = transcoding
                .get("duration")
                .filter(|duration| !duration.is_null())
                .unwrap_or(&value["duration"])
                .as_u64()
                .filter(|millis| (1_000..=MAX_PREVIEW_MILLIS).contains(millis));
            if let Some(duration) = duration {
                return (SoundcloakPlayback::Preview, Some(duration / 1_000));
            }
        }
    }
    unavailable
}

/// Rewrites only `SoundCloud`'s known root-level artwork/avatar filename variants.
/// Custom image names, unknown variants and other CDN paths remain untouched.
fn resize_known_artwork(artwork: &mut Url, variant: &str) {
    let Some(image_host) = artwork
        .host_str()
        .and_then(|host| host.strip_suffix(".sndcdn.com"))
        .and_then(|host| host.strip_prefix('i'))
    else {
        return;
    };
    if image_host.is_empty() || !image_host.bytes().all(|byte| byte.is_ascii_digit()) {
        return;
    }
    let Some(filename) = artwork.path().strip_prefix('/') else {
        return;
    };
    if filename.contains('/')
        || !(filename.starts_with("artworks-") || filename.starts_with("avatars-"))
    {
        return;
    }
    let Some((stem, extension)) = filename.rsplit_once('.') else {
        return;
    };
    if !matches!(extension, "jpg" | "jpeg" | "png") {
        return;
    }
    let Some((identifier, old_variant)) = stem.rsplit_once('-') else {
        return;
    };
    let has_identifier = identifier
        .strip_prefix("artworks-")
        .or_else(|| identifier.strip_prefix("avatars-"))
        .is_some_and(|id| id.bytes().any(|byte| byte.is_ascii_alphanumeric()));
    if !matches!(
        old_variant,
        "large" | "t200x200" | "t500x500" | "t1080x1080"
    ) || !has_identifier
        || !identifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return;
    }
    artwork.set_path(&format!("/{identifier}-{variant}.{extension}"));
}

/// Normalizes bounded public timestamps once, leaving malformed optional dates unknown.
fn normalized_timestamp(value: &Value) -> Option<String> {
    let raw = value.as_str().filter(|text| text.len() <= 64)?;
    let date = chrono::DateTime::parse_from_rfc3339(raw).ok()?;
    Some(
        date.with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
    )
}

/// Normalizes whitespace while rejecting complete text fields containing terminal controls.
fn safe_multiline(text: &str, max_bytes: usize) -> Option<String> {
    if text.len() > max_bytes {
        return None;
    }
    let text = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', " ");
    (!text.trim().is_empty()
        && !text
            .chars()
            .any(|character| character.is_control() && character != '\n'))
    .then_some(text)
}

/// Parses `SoundCloud`'s whitespace-separated tags, preserving quoted phrases and commas.
/// Malformed quotes or oversized input are omitted rather than partially misrepresented.
fn parse_tags(raw: &str) -> Vec<String> {
    if raw.len() > MAX_TAG_LIST_BYTES || raw.chars().any(char::is_control) {
        return Vec::new();
    }
    let mut tags = Vec::new();
    let mut tag = String::new();
    let mut quoted = false;
    for character in raw.chars() {
        if character == '"' {
            quoted = !quoted;
        } else if character.is_whitespace() && !quoted {
            if let Some(tag) = safe_label(&tag, MAX_TAG_BYTES) {
                tags.push(tag);
            }
            tag.clear();
        } else {
            tag.push(character);
        }
        if tag.len() > MAX_TAG_BYTES || tags.len() > MAX_TAGS {
            return Vec::new();
        }
    }
    if quoted {
        return Vec::new();
    }
    if let Some(tag) = safe_label(&tag, MAX_TAG_BYTES) {
        tags.push(tag);
    }
    if tags.len() > MAX_TAGS {
        Vec::new()
    } else {
        tags
    }
}

/// Discards malformed comments without retaining any unrelated upstream profile fields.
fn normalize_comment(value: &Value) -> Option<SoundcloakComment> {
    if value["kind"].as_str() != Some("comment") {
        return None;
    }
    Some(SoundcloakComment {
        author: safe_label(value["user"]["username"].as_str()?, MAX_LABEL_BYTES)?,
        author_url: catalog::user_profile_url(&value["user"]),
        body: safe_multiline(value["body"].as_str()?, MAX_COMMENT_BYTES)?,
        created_at: normalized_timestamp(&value["created_at"]),
        timestamp_seconds: value["timestamp"]
            .as_u64()
            .filter(|timestamp| *timestamp <= MAX_DURATION_MILLIS)
            .map(|timestamp| timestamp / 1_000),
    })
}

/// Enforces canonical durable identity before using either path segment in an endpoint.
fn canonical_segments(url: &Url) -> Result<(&str, &str), ProviderError> {
    let invalid = || {
        ProviderError::InvalidRequest(
            "expected a canonical https://soundcloud.com/artist/track URL".into(),
        )
    };
    if url.as_str().len() > MAX_URL_BYTES
        || url.scheme() != "https"
        || url.host_str() != Some("soundcloud.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    let mut segments = url.path_segments().ok_or_else(invalid)?;
    let artist = segments.next().ok_or_else(invalid)?;
    let track = segments.next().ok_or_else(invalid)?;
    if segments.next().is_some()
        || !valid_slug(artist)
        || !valid_slug(track)
        || matches!(
            artist,
            "search"
                | "discover"
                | "you"
                | "stream"
                | "upload"
                | "charts"
                | "stations"
                | "settings"
                | "people"
                | "groups"
                | "tags"
                | "pages"
        )
        || matches!(
            track,
            "sets" | "tracks" | "albums" | "reposts" | "likes" | "popular-tracks"
        )
    {
        return Err(invalid());
    }
    Ok((artist, track))
}

/// Public `SoundCloud` aliases use bounded URL-safe ASCII, never encoded separators.
fn valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Display labels are trimmed once and reject terminal controls before retention.
fn safe_label(value: &str, max_bytes: usize) -> Option<String> {
    (!value.trim().is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control))
        .then(|| value.trim().to_owned())
}

/// Search-session tokens are opaque query values, not executable paths or HTTP credentials.
fn valid_query_urn(token: &str) -> bool {
    !token.trim().is_empty()
        && token.len() <= MAX_QUERY_URN_BYTES
        && !token.chars().any(char::is_control)
}

/// Validates the next offset and carries only the search token, never its upstream URL.
fn continuation(
    value: &Value,
    request: &SoundcloakSearchRequest,
    tag: Option<&str>,
) -> Result<(Option<u32>, Option<String>), ProviderError> {
    let token = value
        .get("query_urn")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .filter(|token| valid_query_urn(token))
                .map(str::to_owned)
                .ok_or_else(|| invalid_response("invalid search continuation token"))
        })
        .transpose()?;
    let Some(raw_next) = value.get("next_href").filter(|value| !value.is_null()) else {
        return Ok((None, token.or_else(|| request.query_urn.clone())));
    };
    let raw_next = raw_next
        .as_str()
        .filter(|value| value.len() <= MAX_URL_BYTES)
        .ok_or_else(|| invalid_response("invalid search continuation URL"))?;
    let next =
        Url::parse(raw_next).map_err(|_| invalid_response("invalid search continuation URL"))?;
    if next.scheme() != "https"
        || next.host_str() != Some("api-v2.soundcloud.com")
        || next.path() != "/search/tracks"
        || next.port().is_some()
        || !next.username().is_empty()
        || next.password().is_some()
        || next.fragment().is_some()
    {
        return Err(invalid_response("untrusted search continuation URL"));
    }
    let mut seen = HashSet::new();
    let mut next_offset = None;
    let mut next_limit = None;
    let mut next_token = None;
    let mut next_tag = None;
    let mut next_sort = None;
    for (key, value) in next.query_pairs() {
        if (matches!(key.as_ref(), "q" | "limit" | "offset" | "query_urn")
            || (tag.is_some() && matches!(key.as_ref(), "filter.genre_or_tag" | "sort")))
            && !seen.insert(key.to_string())
        {
            return Err(invalid_response("duplicate search continuation parameter"));
        }
        match key.as_ref() {
            "q" if value != request.query.trim() => {
                return Err(invalid_response("search continuation changed its query"));
            }
            "limit" => next_limit = value.parse::<usize>().ok(),
            "offset" => next_offset = value.parse::<u64>().ok(),
            "query_urn" => {
                if !valid_query_urn(&value) {
                    return Err(invalid_response("invalid search continuation token"));
                }
                next_token = Some(value.into_owned());
            }
            "filter.genre_or_tag" if tag.is_some() => next_tag = Some(value.into_owned()),
            "sort" if tag.is_some() => next_sort = Some(value.into_owned()),
            _ => {} // In particular, never copy upstream client_id into the instance request.
        }
    }
    if let Some(tag) = tag
        && (next_tag.as_deref() != Some(tag) || next_sort.as_deref() != Some("popular"))
    {
        return Err(invalid_response(
            "tag continuation changed or omitted its filter",
        ));
    }
    let expected_offset = u64::from(request.page)
        * u64::try_from(request.limit).map_err(|_| invalid_response("search limit overflows"))?;
    if next_offset != Some(expected_offset) || next_limit != Some(request.limit) {
        return Err(invalid_response(
            "search continuation has an unexpected offset or limit",
        ));
    }
    if let (Some(left), Some(right)) = (&token, &next_token)
        && left != right
    {
        return Err(invalid_response("inconsistent search continuation token"));
    }
    Ok((
        (request.page < MAX_SEARCH_PAGE).then_some(request.page + 1),
        next_token.or(token).or_else(|| request.query_urn.clone()),
    ))
}

/// Keeps remote schema failures finite and independent of untrusted response text.
fn invalid_response(message: &str) -> ProviderError {
    ProviderError::InvalidResponse(format!("Soundcloak: {message}"))
}

/// Redirects are rejected before they can move a metadata request off the trusted instance.
struct UreqSoundcloakTransport {
    agent: ureq::Agent,
}

impl SoundcloakTransport for UreqSoundcloakTransport {
    fn fetch(&self, url: &Url, max_bytes: usize) -> Result<Vec<u8>, ProviderError> {
        let mut response = self
            .agent
            .get(url.as_str())
            .header("Accept", "application/json")
            .call()
            .map_err(|error| ProviderError::Transport(error.to_string()))?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(ProviderError::HttpStatus(status));
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpListener};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// Records exact proxy requests and supplies only finite fixture bodies.
    #[derive(Default)]
    struct MockTransport {
        requests: Mutex<Vec<Url>>,
        responses: Mutex<VecDeque<Vec<u8>>>,
    }

    impl SoundcloakTransport for MockTransport {
        fn fetch(&self, url: &Url, _: usize) -> Result<Vec<u8>, ProviderError> {
            self.requests.lock().unwrap().push(url.clone());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(ProviderError::HttpStatus(503))
        }
    }

    fn fixture_track() -> Value {
        json!({"id": 123456, "kind": "track", "title": "Ambient – fixture", "duration": 120500,
            "full_duration": 120500, "permalink_url": "https://soundcloud.com/artist/track",
            "artwork_url": "https://i1.sndcdn.com/art-large.jpg", "description": "Line one\r\nLine two",
            "streamable": true, "policy": "ALLOW", "user": {"username": "Artist"},
            "media": {"transcodings": [{"snipped": false,
                "format": {"protocol": "hls", "mime_type": "audio/aac"},
                "url": "https://signed.example.test/do-not-retain?secret=1"}]}})
    }

    fn request() -> SoundcloakSearchRequest {
        SoundcloakSearchRequest {
            query: "ambient & piano".into(),
            page: 1,
            limit: 2,
            query_urn: None,
        }
    }

    fn client(values: Vec<Value>) -> (SoundcloakClient, Arc<MockTransport>) {
        let transport = Arc::new(MockTransport {
            requests: Mutex::default(),
            responses: Mutex::new(
                values
                    .iter()
                    .map(|value| serde_json::to_vec(value).unwrap())
                    .collect(),
            ),
        });
        (
            SoundcloakClient::with_transport(
                Url::parse(DEFAULT_SOUNDCLOAK_INSTANCE).unwrap(),
                transport.clone(),
            )
            .unwrap(),
            transport,
        )
    }

    /// Serves one finite HTTP response with bounded socket and accept waits.
    fn http_fixture(response: Vec<u8>) -> (Url, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "mock Soundcloak accept timed out"
                        );
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("mock Soundcloak accept failed: {error}"),
                }
            };
            // Windows can inherit the listener's nonblocking mode; use bounded reads.
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 8_192, "oversized mock request");
                let mut byte = [0_u8; 1];
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let _ = socket.write_all(&response);
            String::from_utf8(request).unwrap()
        });
        (
            Url::parse(&format!("http://{address}/prefix/")).unwrap(),
            worker,
        )
    }

    #[test]
    fn configured_instance_http_is_bounded_and_never_follows_redirects() {
        let body = serde_json::to_string(&json!({"collection": [fixture_track()]})).unwrap();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
        let (base, worker) = http_fixture(response);
        let client = SoundcloakClient::new(base).unwrap();
        assert_eq!(client.search(&request()).unwrap().items.len(), 1);
        let request = worker.join().unwrap();
        assert!(request.starts_with("GET /prefix/_/api/v2/search/tracks?"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("\r\naccept: application/json\r\n")
        );
        assert!(!request.to_ascii_lowercase().contains("\r\nauthorization:"));

        let destination = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        destination.set_nonblocking(true).unwrap();
        let response = format!("HTTP/1.1 302 Found\r\nLocation: http://{}/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", destination.local_addr().unwrap()).into_bytes();
        let (base, worker) = http_fixture(response);
        assert!(matches!(
            SoundcloakClient::new(base)
                .unwrap()
                .search(&self::request()),
            Err(ProviderError::HttpStatus(302))
        ));
        worker.join().unwrap();
        assert_eq!(
            destination.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn http_transport_rejects_status_and_bodies_beyond_the_bound() {
        for response in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\n123456789".to_vec(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n9\r\n123456789\r\n0\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n123456789".to_vec(),
        ] {
            let (base, worker) = http_fixture(response);
            let client = SoundcloakClient::new(base.clone()).unwrap();
            assert!(matches!(client.transport.fetch(&base, 8), Err(ProviderError::ResponseTooLarge { limit: 8 })));
            worker.join().unwrap();
        }
        let (base, worker) = http_fixture(
            b"HTTP/1.1 500 Error\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".to_vec(),
        );
        assert!(matches!(
            SoundcloakClient::new(base).unwrap().search(&request()),
            Err(ProviderError::HttpStatus(500))
        ));
        worker.join().unwrap();
    }

    #[test]
    fn search_normalizes_canonical_metadata_and_proxies_artwork() {
        let (client, transport) = client(vec![
            json!({"collection": [fixture_track()], "next_href": null}),
        ]);
        let page = client.search(&request()).unwrap();
        let track = &page.items[0];
        assert_eq!(track.id, "123456");
        assert_eq!(track.artist, "Artist");
        assert_eq!(track.duration_seconds, Some(120));
        assert_eq!(track.description.as_deref(), Some("Line one\nLine two"));
        assert!(track.streamable);
        assert_eq!(
            track.webpage_url.as_str(),
            "https://soundcloud.com/artist/track"
        );
        let artwork = track.artwork_url.as_ref().unwrap();
        assert_eq!(artwork.host_str(), Some("sc1.maid.zone"));
        assert_eq!(artwork.path(), "/_/proxy/images");
        assert!(
            serde_json::to_string(track)
                .unwrap()
                .find("secret")
                .is_none()
        );
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests[0].path(), "/_/api/v2/search/tracks");
        assert!(
            requests[0]
                .query_pairs()
                .any(|(key, value)| key == "q" && value == "ambient & piano")
        );
    }

    #[test]
    fn pagination_keeps_validated_query_token_but_never_follows_upstream_urls() {
        let next = "https://api-v2.soundcloud.com/search/tracks?q=ambient%20%26%20piano&limit=2&offset=2&query_urn=soundcloud%3Asearch%3Afixture&client_id=secret";
        let (client, transport) = client(vec![
            json!({"collection": [fixture_track(), fixture_track()], "next_href": next}),
            json!({"collection": [], "next_href": null}),
        ]);
        let page = client.search(&request()).unwrap();
        assert_eq!(page.next_page, Some(2));
        assert_eq!(page.query_urn.as_deref(), Some("soundcloud:search:fixture"));
        let mut next_request = request();
        next_request.page = page.next_page.unwrap();
        next_request.query_urn = page.query_urn;
        client.search(&next_request).unwrap();
        let requests = transport.requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .all(|url| url.host_str() == Some("sc1.maid.zone"))
        );
        assert!(!requests[1].as_str().contains("client_id"));
        assert!(
            requests[1]
                .query_pairs()
                .any(|(key, value)| key == "offset" && value == "2")
        );
        assert!(
            requests[1]
                .query_pairs()
                .any(|(key, value)| key == "query_urn" && value == "soundcloud:search:fixture")
        );
    }

    #[test]
    fn detail_metadata_retains_bounded_counts_dates_license_and_quoted_tags() {
        let mut source = fixture_track();
        for (field, value) in [
            ("likes_count", json!(0)),
            ("playback_count", json!(1_832_732)),
            ("reposts_count", json!(1_510)),
            ("comment_count", json!(720)),
            ("created_at", json!("2017-07-12T08:41:32Z")),
            ("last_modified", json!("2022-06-01T04:19:40+04:00")),
            ("license", json!("all-rights-reserved")),
            ("genre", json!("Hip-hop & Rap")),
            (
                "tag_list",
                json!("Underground Minsk \"минский андеграунд\" \"Post-Punk,Synthwave\""),
            ),
        ] {
            source[field] = value;
        }
        let (client, _) = client(vec![json!({"collection": [source]})]);
        let track = serde_json::to_value(&client.search(&request()).unwrap().items[0]).unwrap();
        assert_eq!(track["likes_count"], json!(0));
        assert_eq!(track["playback_count"], json!(1_832_732));
        assert_eq!(track["reposts_count"], json!(1_510));
        assert_eq!(track["comment_count"], json!(720));
        assert_eq!(track["created_at"], json!("2017-07-12T08:41:32Z"));
        assert_eq!(track["last_modified"], json!("2022-06-01T00:19:40Z"));
        assert_eq!(track["license"], json!("all-rights-reserved"));
        assert_eq!(track["genre"], json!("Hip-hop & Rap"));
        assert_eq!(
            track["tags"],
            json!([
                "Underground",
                "Minsk",
                "минский андеграунд",
                "Post-Punk,Synthwave"
            ])
        );
    }

    #[test]
    fn detail_artwork_uses_500_and_exposes_lazy_1080_without_fetching_either() {
        let mut source = fixture_track();
        source["artwork_url"] =
            json!("https://i1.sndcdn.com/artworks-000233237713-ycrkrp-large.jpg");
        let (client, transport) = client(vec![json!({"collection": [source]})]);
        let track = client.search(&request()).unwrap().items.remove(0);
        let artwork = track.artwork_url.as_ref().unwrap();
        assert_eq!(
            artwork
                .query_pairs()
                .find(|(key, _)| key == "url")
                .unwrap()
                .1,
            "https://i1.sndcdn.com/artworks-000233237713-ycrkrp-t500x500.jpg"
        );
        let serialized = serde_json::to_value(&track).unwrap();
        let expanded = Url::parse(serialized["expanded_artwork_url"].as_str().unwrap()).unwrap();
        assert_eq!(
            expanded
                .query_pairs()
                .find(|(key, _)| key == "url")
                .unwrap()
                .1,
            "https://i1.sndcdn.com/artworks-000233237713-ycrkrp-t1080x1080.jpg"
        );
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn public_progressive_preview_is_playable_with_actual_not_full_duration() {
        let mut source = fixture_track();
        source["policy"] = json!("SNIP");
        source["duration"] = json!(30_000);
        source["full_duration"] = json!(218_593);
        source["media"]["transcodings"][0] = json!({
            "snipped": true, "duration": 30_000,
            "format": {"protocol": "progressive", "mime_type": "audio/mpeg"}
        });
        let (client, _) = client(vec![json!({"collection": [source]})]);
        let track = client.search(&request()).unwrap().items.remove(0);
        assert!(
            track.streamable,
            "an explicitly advertised public preview must be playable"
        );
        assert_eq!(track.duration_seconds, Some(30));
        let serialized = serde_json::to_value(&track).unwrap();
        assert_eq!(serialized["full_duration_seconds"], json!(218));
        assert_eq!(serialized["playback"], json!("Preview"));
        assert_eq!(
            client.playback_url(&track).unwrap().as_str(),
            "https://sc1.maid.zone/_/api/progressive/artist/track?redirect=false"
        );
    }

    #[test]
    fn artwork_variant_rewriting_preserves_custom_originals_and_stays_on_the_instance() {
        let (client, transport) = client(vec![]);
        for raw in [
            "https://i1.sndcdn.com/custom-large.jpg",
            "https://i1.sndcdn.com/artworks-123-custom.jpg",
            "https://i1.sndcdn.com/artworks-123-original.jpg",
            "https://i1.sndcdn.com/custom/artworks-123-large.jpg",
            "https://i1.sndcdn.com//artworks-123-large.jpg",
            "https://i1.sndcdn.com/artworks-large.jpg",
            "https://files.sndcdn.com/artworks-123-large.jpg",
        ] {
            for variant in ["t500x500", "t1080x1080"] {
                let url = client.artwork_url(raw, variant).unwrap();
                assert_eq!(url.origin(), client.base_url.origin());
                assert_eq!(
                    url.query_pairs().find(|(key, _)| key == "url").unwrap().1,
                    raw
                );
            }
        }
        for raw in [
            "http://i1.sndcdn.com/artworks-123-large.jpg",
            "https://i1.sndcdn.com.evil.test/artworks-123-large.jpg",
            "https://i1.sndcdn.com/artworks-123-large.jpg?token=secret",
            "https://secret@i1.sndcdn.com/artworks-123-large.jpg",
        ] {
            assert!(client.artwork_url(raw, "t1080x1080").is_none(), "{raw}");
        }
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn genre_urls_encode_one_segment_and_preserve_configured_instance_prefix() {
        let (_, transport) = client(vec![]);
        let client = SoundcloakClient::with_transport(
            Url::parse("https://soundcloak.example/instance/").unwrap(),
            transport.clone(),
        )
        .unwrap();
        assert_eq!(
            client.genre_url("Hip-hop & Rap").unwrap().as_str(),
            "https://soundcloak.example/instance/tags/Hip-hop%20&%20Rap"
        );
        let genre = client
            .genre_url("../?next=https://evil.example/#Jazz")
            .unwrap();
        assert_eq!(
            genre.path(),
            "/instance/tags/..%2F%3Fnext=https:%2F%2Fevil.example%2F%23Jazz"
        );
        assert!(genre.query().is_none());
        assert!(genre.fragment().is_none());
        for invalid in [
            "",
            " ",
            ".",
            "..",
            "Jazz\u{1b}",
            &"x".repeat(MAX_TAG_BYTES + 1),
        ] {
            assert!(client.genre_url(invalid).is_err(), "{invalid:?}");
        }
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn optional_metadata_and_tags_are_bounded_without_discarding_the_track() {
        let mut source = fixture_track();
        for (key, value) in [
            ("likes_count", json!(-1)),
            ("playback_count", json!("12")),
            ("reposts_count", json!(1.5)),
            ("comment_count", Value::Null),
            ("created_at", json!("2026-02-30T00:00:00Z")),
            ("last_modified", json!("x".repeat(100))),
            ("license", json!("copyright\u{1b}")),
            ("genre", json!("x".repeat(MAX_TAG_BYTES + 1))),
            ("tag_list", json!("x".repeat(MAX_TAG_LIST_BYTES + 1))),
        ] {
            source[key] = value;
        }
        let track = client(vec![]).0.normalize_track(&source).unwrap();
        assert_eq!(track.playback, SoundcloakPlayback::Full);
        assert!(
            track.likes_count.is_none()
                && track.playback_count.is_none()
                && track.reposts_count.is_none()
                && track.comment_count.is_none()
        );
        assert!(track.created_at.is_none() && track.last_modified.is_none());
        assert!(track.license.is_none() && track.genre.is_none() && track.tags.is_empty());
        for invalid in [
            "\"unclosed",
            "bad\u{1b}tag",
            &"a ".repeat(MAX_TAGS + 1),
            &format!("\"{}\"", "x".repeat(MAX_TAG_BYTES + 1)),
        ] {
            assert!(parse_tags(invalid).is_empty(), "{invalid:?}");
        }
        assert_eq!(
            parse_tags("  \"Hip-hop & Rap\" rock  \"Post-Punk,Synthwave\"  "),
            ["Hip-hop & Rap", "rock", "Post-Punk,Synthwave"]
        );
        assert_eq!(parse_tags("\"\" rock"), ["rock"]);
    }

    #[test]
    fn preview_admission_never_promotes_restricted_private_or_ambiguous_renditions() {
        let mut preview = fixture_track();
        preview["policy"] = json!("SNIP");
        preview["duration"] = json!(30_000);
        preview["full_duration"] = json!(218_593);
        preview["media"]["transcodings"][0] = json!({"snipped": true, "duration": 30_000,
            "format": {"protocol": "progressive", "mime_type": "audio/mpeg"}});
        let client = client(vec![]).0;
        for (key, value) in [
            ("policy", json!("BLOCK")),
            ("policy", json!("MONETIZE")),
            ("policy", json!("ALLOW")),
            ("sharing", json!("private")),
            ("streamable", json!(false)),
            ("streamable", Value::Null),
        ] {
            let mut source = preview.clone();
            source[key] = value;
            let track = client.normalize_track(&source).unwrap();
            assert_eq!(track.playback, SoundcloakPlayback::Unavailable, "{key}");
            assert!(!track.streamable);
            assert!(client.playback_url(&track).is_err());
        }
        for (key, value) in [
            ("snipped", Value::Null),
            ("snipped", json!(false)),
            ("duration", json!(0)),
            ("duration", json!(61_000)),
            ("duration", json!("30000")),
            (
                "format",
                json!({"protocol": "hls", "mime_type": "audio/mpeg"}),
            ),
            (
                "format",
                json!({"protocol": "encrypted-hls", "mime_type": "audio/mpeg"}),
            ),
            (
                "format",
                json!({"protocol": "progressive", "mime_type": "video/mp4"}),
            ),
        ] {
            let mut source = preview.clone();
            source["media"]["transcodings"][0][key] = value;
            assert_eq!(
                client.normalize_track(&source).unwrap().playback,
                SoundcloakPlayback::Unavailable,
                "{key}"
            );
        }
        // Missing rendition duration can use the bounded actual metadata length,
        // but the full work's duration must never be substituted for that length.
        preview["media"]["transcodings"][0]["duration"] = Value::Null;
        preview["full_duration"] = Value::Null;
        let track = client.normalize_track(&preview).unwrap();
        assert_eq!(track.duration_seconds, Some(30));
        assert_eq!(track.full_duration_seconds, None);
        preview["duration"] = Value::Null;
        preview["full_duration"] = json!(218_593);
        assert_eq!(
            client.normalize_track(&preview).unwrap().playback,
            SoundcloakPlayback::Unavailable
        );
    }

    #[test]
    fn recent_comments_use_bounded_public_route_and_ignore_unsafe_text_and_continuation() {
        let valid = json!({"kind": "comment", "body": "First\r\nSecond\tline <literal>",
            "created_at": "2026-09-19T14:48:20+04:00", "timestamp": 63_133,
            "user": {"username": "Public listener"}});
        let mut unsafe_body = valid.clone();
        unsafe_body["body"] = json!("Hello\u{1b}]52;secret");
        let mut unsafe_author = valid.clone();
        unsafe_author["user"]["username"] = json!("Unsafe\nAuthor");
        let mut oversized = valid.clone();
        oversized["body"] = json!("x".repeat(MAX_COMMENT_BYTES + 1));
        let (client, transport) = client(vec![
            json!({"collection": [valid, unsafe_body, unsafe_author,
            oversized, {"kind": "track"}], "next_href": "http://127.0.0.1/private"}),
        ]);
        let comments = client.comments("332838846").unwrap();
        assert_eq!(
            comments,
            [SoundcloakComment {
                author: "Public listener".into(),
                author_url: None,
                body: "First\nSecond line <literal>".into(),
                created_at: Some("2026-09-19T10:48:20Z".into()),
                timestamp_seconds: Some(63)
            }]
        );
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path(), "/_/api/v2/tracks/332838846/comments");
        assert_eq!(
            requests[0].query(),
            Some("limit=20&threaded=0&filter_replies=1&sort=created_at")
        );
    }

    #[test]
    fn comments_reject_invalid_ids_and_oversized_envelopes_without_followups() {
        let (client, transport) = client(vec![
            json!({"collection": vec![json!({}); MAX_COMMENTS + 1]}),
            json!({"collection": {}}),
            json!({"collection": []}),
        ]);
        for invalid in [
            "",
            "0",
            "-1",
            "1/comments",
            "123?url=private",
            "18446744073709551616",
        ] {
            assert!(client.comments(invalid).is_err(), "{invalid}");
        }
        assert!(transport.requests.lock().unwrap().is_empty());
        assert!(client.comments("123").is_err());
        assert!(client.comments("123").is_err());
        assert!(client.comments("123").unwrap().is_empty());
        assert!(matches!(
            client.comments("123"),
            Err(ProviderError::HttpStatus(503))
        ));
        assert_eq!(transport.requests.lock().unwrap().len(), 4);
    }

    #[test]
    fn restricted_preview_and_encrypted_tracks_are_not_full_length_playback() {
        for policy in ["SNIP", "BLOCK", "MONETIZE", "UNKNOWN"] {
            let mut track = fixture_track();
            track["policy"] = json!(policy);
            let (client, _) = client(vec![json!({"collection": [track]})]);
            assert!(
                !client.search(&request()).unwrap().items[0].streamable,
                "{policy}"
            );
        }
        for (field, value) in [
            ("streamable", Value::Null),
            ("media", json!({"transcodings": []})),
        ] {
            let mut track = fixture_track();
            track[field] = value;
            let (client, _) = client(vec![json!({"collection": [track]})]);
            assert!(!client.search(&request()).unwrap().items[0].streamable);
        }
        for protocol in [
            "encrypted-hls",
            "ctr-encrypted-hls",
            "cbc-encrypted-hls",
            "progressive",
        ] {
            let mut track = fixture_track();
            track["media"]["transcodings"][0]["format"]["protocol"] = json!(protocol);
            let (client, _) = client(vec![json!({"collection": [track]})]);
            assert!(!client.search(&request()).unwrap().items[0].streamable);
        }
        for snipped in [json!(true), json!("true"), json!(1)] {
            let mut track = fixture_track();
            track["media"]["transcodings"][0]["snipped"] = snipped;
            let (client, _) = client(vec![json!({"collection": [track]})]);
            assert!(!client.search(&request()).unwrap().items[0].streamable);
        }
    }

    #[test]
    fn stream_url_is_instance_owned_and_canonical_validation_is_strict() {
        let client = SoundcloakClient::default();
        let url = client
            .stream_url(&Url::parse("https://soundcloud.com/artist/track").unwrap())
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://sc1.maid.zone/_/api/hls/artist/track?audio=aac&redirect=false&redirect_parts=false"
        );
        assert_eq!(
            client
                .page_url(&Url::parse("https://soundcloud.com/artist/track").unwrap())
                .unwrap()
                .as_str(),
            "https://sc1.maid.zone/artist/track"
        );
        for invalid in [
            "http://soundcloud.com/artist/track",
            "https://soundcloud.com.evil.test/artist/track",
            "https://evil@soundcloud.com/artist/track",
            "https://soundcloud.com:444/artist/track",
            "https://soundcloud.com/artist/sets",
            "https://soundcloud.com/artist/track/extra",
            "https://soundcloud.com/artist/track?secret_token=secret",
            "https://soundcloud.com/artist/%2Ftrack",
            "https://soundcloud.com/search/tracks",
            "https://soundcloud.com/artist/track#fragment",
        ] {
            assert!(
                client.stream_url(&Url::parse(invalid).unwrap()).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn resolve_uses_proxy_and_refuses_mismatched_identity() {
        let (client, transport) = client(vec![fixture_track(), fixture_track()]);
        let expected = Url::parse("https://soundcloud.com/artist/track").unwrap();
        assert_eq!(client.resolve(&expected).unwrap().webpage_url, expected);
        assert_eq!(
            transport.requests.lock().unwrap()[0].path(),
            "/_/api/v2/resolve"
        );
        assert!(
            client
                .resolve(&Url::parse("https://soundcloud.com/other/track").unwrap())
                .is_err()
        );
    }

    #[test]
    fn invalid_requests_and_untrusted_pagination_are_rejected() {
        let (client, transport) = client(vec![]);
        for (page, limit, query) in [
            (0, 2, "ok"),
            (1, 0, "ok"),
            (1, 101, "ok"),
            (1, 2, ""),
            (1, 2, "bad\u{1b}"),
        ] {
            let request = SoundcloakSearchRequest {
                query: query.into(),
                page,
                limit,
                query_urn: None,
            };
            assert!(client.search(&request).is_err());
        }
        assert!(transport.requests.lock().unwrap().is_empty());
        for next in [
            "https://localhost/search/tracks?offset=2&limit=2",
            "https://api-v2.soundcloud.com/resolve?offset=2&limit=2",
            "https://api-v2.soundcloud.com/search/tracks?offset=0&limit=2",
            "https://api-v2.soundcloud.com/search/tracks?offset=4&limit=2",
            "https://api-v2.soundcloud.com/search/tracks?offset=2&limit=100",
        ] {
            let (client, _) = self::client(vec![
                json!({"collection": [fixture_track()], "next_href": next}),
            ]);
            assert!(client.search(&request()).is_err(), "{next}");
        }
    }

    /// Opt-in smoke check of the production transport and current public API schema.
    #[test]
    #[ignore = "contacts the public Soundcloak instance; no audio is played"]
    fn live_soundcloak_search_and_continuation() {
        let client = SoundcloakClient::default();
        let mut request = SoundcloakSearchRequest {
            query: "ambient".to_owned(),
            page: 1,
            limit: 2,
            query_urn: None,
        };
        let first = client.search(&request).expect("public Soundcloak search");
        assert!(!first.items.is_empty());
        let track = &first.items[0];
        assert_eq!(
            client.page_url(&track.webpage_url).unwrap().origin(),
            client.base_url().origin()
        );
        assert_eq!(
            client.stream_url(&track.webpage_url).unwrap().origin(),
            client.base_url().origin()
        );
        if let Some(page) = first.next_page {
            request.page = page;
            request.query_urn = first.query_urn;
            let second = client
                .search(&request)
                .expect("public Soundcloak continuation");
            assert!(!second.items.is_empty());
            assert_ne!(first.items[0].id, second.items[0].id);
        }
    }

    #[test]
    fn malformed_oversized_and_unsafe_metadata_cannot_escape_bounds() {
        for value in [
            json!({}),
            json!({"collection": {}}),
            json!({"collection": [fixture_track(), fixture_track(), fixture_track()]}),
        ] {
            let (client, _) = client(vec![value]);
            assert!(client.search(&request()).is_err());
        }
        for (field, value) in [
            ("title", json!("bad\u{1b}name")),
            ("title", json!("a".repeat(4097))),
            ("permalink_url", json!("https://localhost/a/b")),
            ("kind", json!("playlist")),
        ] {
            let mut track = fixture_track();
            track[field] = value;
            let (client, _) = client(vec![json!({"collection": [track]})]);
            assert!(client.search(&request()).unwrap().items.is_empty());
        }
        let mut track = fixture_track();
        track["artwork_url"] = json!("https://127.0.0.1/private");
        let (client, _) = client(vec![json!({"collection": [track]})]);
        assert!(
            client.search(&request()).unwrap().items[0]
                .artwork_url
                .is_none()
        );
        let (_, transport) = self::client(vec![]);
        transport
            .responses
            .lock()
            .unwrap()
            .push_back(vec![b' '; DEFAULT_MAX_JSON_BYTES + 1]);
        let client = SoundcloakClient::with_transport(
            Url::parse(DEFAULT_SOUNDCLOAK_INSTANCE).unwrap(),
            transport,
        )
        .unwrap();
        assert!(matches!(
            client.search(&request()),
            Err(ProviderError::ResponseTooLarge { .. })
        ));
    }
}
