//! `SoundStream` direct-link metadata resolver.
//!
//! The resolver accepts public `https://soundstream.media/playlist/{alias}`
//! and `https://soundstream.media/clip/{alias}` links. It reads only the
//! unauthenticated, read-only metadata endpoints used by `SoundStream`'s public
//! web pages. Requests are blocking, HTTPS-only, redirect-free, and response
//! bodies are bounded; callers should run them on a provider worker.
//!
//! `SoundStream` does not publish third-party API documentation for these
//! endpoints, so their schema can change without notice. Search and the media
//! URL signing endpoint require an anonymous bearer token and are deliberately
//! not automated here. When the API returns that auth-gated locator, the
//! resolver reports no media URL. A public RSS feed or direct enclosure is
//! retained only when the response actually exposes one.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use url::Url;

use super::{DEFAULT_MAX_JSON_BYTES, DEFAULT_REQUEST_TIMEOUT, ProviderError, get_bounded_json};

const SITE_ORIGIN: &str = "https://soundstream.media/";
const API_CLIP_ENDPOINT: &str = "https://api.soundstream.media/v3/clip";
const API_PLAYLIST_ENDPOINT: &str = "https://api.soundstream.media/v3/playlist";
const MAX_ALIAS_BYTES: usize = 256;
const MAX_CONFIGURED_JSON_BYTES: usize = 64 * 1024 * 1024;

/// Kind of public `SoundStream` page accepted by the resolver.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoundStreamLinkKind {
    /// A podcast or audiobook playlist page.
    Playlist,
    /// An individual episode or audio clip page.
    Clip,
}

impl SoundStreamLinkKind {
    const fn path_segment(self) -> &'static str {
        match self {
            Self::Playlist => "playlist",
            Self::Clip => "clip",
        }
    }

    const fn api_endpoint(self) -> &'static str {
        match self {
            Self::Playlist => API_PLAYLIST_ENDPOINT,
            Self::Clip => API_CLIP_ENDPOINT,
        }
    }
}

/// Identifiers parsed from a public `SoundStream` playlist or clip URL.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SoundStreamLink {
    /// Whether the URL points to a playlist or an individual clip.
    pub kind: SoundStreamLinkKind,
    /// Stable lowercase alias from the final path segment.
    pub alias: String,
}

impl SoundStreamLink {
    /// Parses an official `SoundStream` playlist or clip URL.
    ///
    /// Accepted paths are `/playlist/{alias}` and `/clip/{alias}`, with an
    /// optional trailing slash. The URL must use the exact public HTTPS origin
    /// and cannot contain credentials, a port, query parameters, or a fragment.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] for a lookalike origin,
    /// malformed path, unsupported page kind, or unsafe alias.
    pub fn parse(url: &Url) -> Result<Self, ProviderError> {
        validate_page_origin(url)?;
        if url.query().is_some() || url.fragment().is_some() {
            return Err(invalid_link(
                "query parameters and fragments are not supported",
            ));
        }

        let mut segments = url
            .path_segments()
            .ok_or_else(|| invalid_link("URL must have path segments"))?
            .collect::<Vec<_>>();
        if segments.last() == Some(&"") {
            segments.pop();
        }
        let [kind, alias] = segments.as_slice() else {
            return Err(invalid_link("expected /playlist/{alias} or /clip/{alias}"));
        };
        let kind = match *kind {
            "playlist" => SoundStreamLinkKind::Playlist,
            "clip" => SoundStreamLinkKind::Clip,
            _ => {
                return Err(invalid_link("expected /playlist/{alias} or /clip/{alias}"));
            }
        };
        validate_alias(alias).map_err(|message| invalid_link(&message))?;

        Ok(Self {
            kind,
            alias: (*alias).to_owned(),
        })
    }

    /// Returns the normalized public webpage URL for this link.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidResponse`] only if the compile-time
    /// `SoundStream` origin cannot be used as a hierarchical URL.
    pub fn webpage_url(&self) -> Result<Url, ProviderError> {
        build_webpage_url(self.kind, &self.alias)
    }
}

/// Playlist metadata returned for a direct `SoundStream` link.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SoundStreamPlaylistMetadata {
    /// Numeric `SoundStream` playlist identifier.
    pub playlist_id: u64,
    /// Stable playlist alias.
    pub alias: String,
    /// Playlist title.
    pub title: String,
    /// Full or abbreviated playlist description, when returned.
    pub description: Option<String>,
    /// Normalized public `SoundStream` playlist page.
    pub webpage_url: Url,
    /// Publisher-supplied source page, when returned.
    pub source_url: Option<Url>,
    /// Public RSS feed URL, when the current response exposes one.
    pub feed_url: Option<Url>,
    /// Playlist artwork URL, when returned.
    pub artwork_url: Option<Url>,
    /// Number of clips reported by `SoundStream`.
    pub clip_count: Option<u64>,
    /// Whether `SoundStream` marks the playlist explicit.
    pub explicit: Option<bool>,
}

/// Parent-playlist metadata attached to a `SoundStream` clip.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SoundStreamPlaylistReference {
    /// Numeric `SoundStream` playlist identifier.
    pub playlist_id: u64,
    /// Stable playlist alias.
    pub alias: String,
    /// Playlist title.
    pub title: String,
    /// Normalized public `SoundStream` playlist page.
    pub webpage_url: Url,
    /// Public RSS feed URL, when the current response exposes one.
    pub feed_url: Option<Url>,
}

/// Clip metadata returned for a direct `SoundStream` link.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SoundStreamClipMetadata {
    /// Numeric `SoundStream` clip identifier.
    pub clip_id: u64,
    /// Stable clip alias.
    pub alias: String,
    /// Clip or episode title.
    pub title: String,
    /// Full or abbreviated clip description, when returned.
    pub description: Option<String>,
    /// Normalized public `SoundStream` clip page.
    pub webpage_url: Url,
    /// Public RSS enclosure or direct media URL, when one is exposed.
    ///
    /// The current `SoundStream` `get-stream-presigned-url` locator needs an
    /// anonymous bearer token and is intentionally omitted rather than
    /// misrepresented as a playable public URL.
    pub media_url: Option<Url>,
    /// Clip artwork URL, when returned.
    pub artwork_url: Option<Url>,
    /// Publication time string as returned by `SoundStream`.
    pub published_at: Option<String>,
    /// Duration in whole seconds, when returned.
    pub duration_seconds: Option<u64>,
    /// Whether `SoundStream` marks the clip explicit.
    pub explicit: Option<bool>,
    /// Parent playlists returned with the clip.
    pub playlists: Vec<SoundStreamPlaylistReference>,
}

/// Metadata variant resolved from a public `SoundStream` URL.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "metadata", rename_all = "snake_case")]
pub enum SoundStreamMetadata {
    /// Metadata for a playlist page.
    Playlist(SoundStreamPlaylistMetadata),
    /// Metadata for an individual clip page.
    Clip(SoundStreamClipMetadata),
}

/// Result of resolving one public `SoundStream` direct link.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResolvedSoundStream {
    /// Parsed identity of the requested public page.
    pub link: SoundStreamLink,
    /// Normalized metadata returned by the read-only API.
    pub metadata: SoundStreamMetadata,
}

/// Blocking client for `SoundStream` direct-link metadata resolution.
#[derive(Clone)]
pub struct SoundStreamResolver {
    agent: ureq::Agent,
    max_json_bytes: usize,
}

impl SoundStreamResolver {
    /// Creates a resolver with conservative timeout and response limits.
    #[must_use]
    pub fn new() -> Self {
        Self {
            agent: soundstream_agent(DEFAULT_REQUEST_TIMEOUT),
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
                "SoundStream timeout must be greater than zero".to_owned(),
            ));
        }
        if !(1..=MAX_CONFIGURED_JSON_BYTES).contains(&max_json_bytes) {
            return Err(ProviderError::InvalidRequest(format!(
                "SoundStream JSON response limit must be between 1 and \
                 {MAX_CONFIGURED_JSON_BYTES} bytes"
            )));
        }
        Ok(Self {
            agent: soundstream_agent(timeout),
            max_json_bytes,
        })
    }

    /// Resolves a public `SoundStream` playlist or clip URL.
    ///
    /// This performs exactly one bounded metadata request. It does not search,
    /// enumerate a playlist, register an anonymous account, or call the
    /// auth-gated media URL signing endpoint.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] when the direct URL is invalid, the bounded
    /// HTTPS request fails, `SoundStream` returns malformed metadata, or the
    /// response identity/canonical page does not match the requested link.
    pub fn resolve(&self, url: &Url) -> Result<ResolvedSoundStream, ProviderError> {
        let link = SoundStreamLink::parse(url)?;
        let endpoint = build_api_url(&link)?;
        let metadata = match link.kind {
            SoundStreamLinkKind::Playlist => {
                let response: RawEnvelope<RawPlaylist> =
                    get_bounded_json(&self.agent, &endpoint, self.max_json_bytes)?;
                SoundStreamMetadata::Playlist(normalize_playlist(&link, response)?)
            }
            SoundStreamLinkKind::Clip => {
                let response: RawEnvelope<RawClip> =
                    get_bounded_json(&self.agent, &endpoint, self.max_json_bytes)?;
                SoundStreamMetadata::Clip(normalize_clip(&link, response)?)
            }
        };
        Ok(ResolvedSoundStream { link, metadata })
    }
}

impl Default for SoundStreamResolver {
    fn default() -> Self {
        Self::new()
    }
}

fn soundstream_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
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
        .into()
}

fn validate_page_origin(url: &Url) -> Result<(), ProviderError> {
    if url.scheme() != "https" {
        return Err(invalid_link("SoundStream links must use HTTPS"));
    }
    if url.host_str() != Some("soundstream.media") {
        return Err(invalid_link("host must be soundstream.media"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid_link("embedded credentials are not allowed"));
    }
    if url.port().is_some() {
        return Err(invalid_link("SoundStream links must not specify a port"));
    }
    Ok(())
}

fn invalid_link(message: &str) -> ProviderError {
    ProviderError::InvalidRequest(format!("invalid SoundStream link: {message}"))
}

fn validate_alias(alias: &str) -> Result<(), String> {
    if alias.is_empty() || alias.len() > MAX_ALIAS_BYTES {
        return Err(format!(
            "alias must contain between 1 and {MAX_ALIAS_BYTES} bytes"
        ));
    }
    if !alias.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
    }) {
        return Err(
            "alias must contain only lowercase ASCII letters, digits, hyphens, or underscores"
                .to_owned(),
        );
    }
    if !alias
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        || !alias
            .bytes()
            .next_back()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Err("alias must start and end with a letter or digit".to_owned());
    }
    Ok(())
}

fn build_api_url(link: &SoundStreamLink) -> Result<Url, ProviderError> {
    let mut url = Url::parse(link.kind.api_endpoint())
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("alias", &link.alias);
        query.append_pair("askPaywallParams", "1");
    }
    validate_api_url(&url, link.kind)?;
    Ok(url)
}

fn validate_api_url(url: &Url, kind: SoundStreamLinkKind) -> Result<(), ProviderError> {
    if url.scheme() != "https"
        || url.host_str() != Some("api.soundstream.media")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.path() != format!("/v3/{}", kind.path_segment())
    {
        return Err(ProviderError::InvalidResponse(
            "SoundStream API URL left the fixed HTTPS API origin".to_owned(),
        ));
    }
    Ok(())
}

fn build_webpage_url(kind: SoundStreamLinkKind, alias: &str) -> Result<Url, ProviderError> {
    let mut url = Url::parse(SITE_ORIGIN)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    url.path_segments_mut()
        .map_err(|()| {
            ProviderError::InvalidResponse(
                "SoundStream site origin cannot contain path segments".to_owned(),
            )
        })?
        .pop_if_empty()
        .push(kind.path_segment())
        .push(alias);
    Ok(url)
}

fn normalize_playlist(
    link: &SoundStreamLink,
    response: RawEnvelope<RawPlaylist>,
) -> Result<SoundStreamPlaylistMetadata, ProviderError> {
    let item = response_item(response, "playlist")?;
    validate_response_identity(link, item.id, &item.alias, &item.name)?;

    Ok(SoundStreamPlaylistMetadata {
        playlist_id: item.id,
        alias: item.alias,
        title: item.name,
        description: owned_nonempty(item.text.or(item.short_text)),
        webpage_url: canonical_page_url(link, item.webpage_url.as_deref())?,
        source_url: parse_optional_remote_url(item.link.as_deref(), "playlist source URL")?,
        feed_url: parse_optional_remote_url(item.feed_url.as_deref(), "playlist feed URL")?,
        artwork_url: parse_optional_remote_url(item.image.as_deref(), "playlist artwork URL")?,
        clip_count: item.clips,
        explicit: item.explicit,
    })
}

fn normalize_clip(
    link: &SoundStreamLink,
    response: RawEnvelope<RawClip>,
) -> Result<SoundStreamClipMetadata, ProviderError> {
    let item = response_item(response, "clip")?;
    validate_response_identity(link, item.id, &item.alias, &item.name)?;
    if item.playlists.len() > 32 {
        return Err(ProviderError::InvalidResponse(
            "SoundStream clip returned more than 32 parent playlists".to_owned(),
        ));
    }
    let playlists = item
        .playlists
        .into_iter()
        .map(normalize_playlist_reference)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(SoundStreamClipMetadata {
        clip_id: item.id,
        alias: item.alias,
        title: item.name,
        description: owned_nonempty(item.text.or(item.short_text)),
        webpage_url: canonical_page_url(link, item.webpage_url.as_deref())?,
        media_url: parse_optional_public_media_url(item.url.as_deref())?,
        artwork_url: parse_optional_remote_url(item.image.as_deref(), "clip artwork URL")?,
        published_at: owned_nonempty(item.published),
        duration_seconds: item.duration,
        explicit: item.explicit,
        playlists,
    })
}

fn normalize_playlist_reference(
    item: RawPlaylistReference,
) -> Result<SoundStreamPlaylistReference, ProviderError> {
    if item.id == 0 {
        return Err(ProviderError::InvalidResponse(
            "SoundStream parent playlist has a zero ID".to_owned(),
        ));
    }
    validate_alias(&item.alias).map_err(|message| {
        ProviderError::InvalidResponse(format!(
            "SoundStream parent playlist alias is invalid: {message}"
        ))
    })?;
    if item.name.trim().is_empty() {
        return Err(ProviderError::InvalidResponse(
            "SoundStream parent playlist is missing a title".to_owned(),
        ));
    }
    Ok(SoundStreamPlaylistReference {
        playlist_id: item.id,
        webpage_url: build_webpage_url(SoundStreamLinkKind::Playlist, &item.alias)?,
        feed_url: parse_optional_remote_url(item.feed_url.as_deref(), "parent playlist feed URL")?,
        alias: item.alias,
        title: item.name,
    })
}

fn response_item<T>(response: RawEnvelope<T>, kind: &str) -> Result<T, ProviderError> {
    if !response.status {
        return Err(ProviderError::InvalidResponse(format!(
            "SoundStream did not resolve the requested {kind}"
        )));
    }
    response.item.ok_or_else(|| {
        ProviderError::InvalidResponse(format!("SoundStream {kind} response is missing its item"))
    })
}

fn validate_response_identity(
    link: &SoundStreamLink,
    id: u64,
    alias: &str,
    title: &str,
) -> Result<(), ProviderError> {
    if id == 0 {
        return Err(ProviderError::InvalidResponse(
            "SoundStream response has a zero ID".to_owned(),
        ));
    }
    validate_alias(alias).map_err(|message| {
        ProviderError::InvalidResponse(format!("SoundStream response alias is invalid: {message}"))
    })?;
    if alias != link.alias {
        return Err(ProviderError::InvalidResponse(
            "SoundStream response alias does not match the requested page".to_owned(),
        ));
    }
    if title.trim().is_empty() {
        return Err(ProviderError::InvalidResponse(
            "SoundStream response is missing a title".to_owned(),
        ));
    }
    Ok(())
}

fn canonical_page_url(link: &SoundStreamLink, raw: Option<&str>) -> Result<Url, ProviderError> {
    let canonical = link.webpage_url()?;
    let Some(raw) = nonempty(raw) else {
        return Ok(canonical);
    };
    let advertised = Url::parse(raw).map_err(|error| {
        ProviderError::InvalidResponse(format!(
            "SoundStream canonical page URL is invalid: {error}"
        ))
    })?;
    let advertised_link = SoundStreamLink::parse(&advertised).map_err(|_| {
        ProviderError::InvalidResponse(
            "SoundStream canonical page URL must remain on the requested service origin".to_owned(),
        )
    })?;
    if advertised_link != *link {
        return Err(ProviderError::InvalidResponse(
            "SoundStream canonical page URL does not match the requested page".to_owned(),
        ));
    }
    Ok(canonical)
}

fn parse_optional_public_media_url(raw: Option<&str>) -> Result<Option<Url>, ProviderError> {
    let Some(url) = parse_optional_remote_url(raw, "clip media URL")? else {
        return Ok(None);
    };
    if url.scheme() == "https"
        && url.host_str() == Some("media.soundstream.media")
        && url.port().is_none()
        && url.path().starts_with("/get-stream-presigned-url/")
    {
        return Ok(None);
    }
    Ok(Some(url))
}

fn parse_optional_remote_url(raw: Option<&str>, field: &str) -> Result<Option<Url>, ProviderError> {
    let Some(raw) = nonempty(raw) else {
        return Ok(None);
    };
    let url = Url::parse(raw).map_err(|error| {
        ProviderError::InvalidResponse(format!("SoundStream {field} is invalid: {error}"))
    })?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(ProviderError::InvalidResponse(format!(
            "SoundStream {field} must be a credential-free HTTP(S) URL"
        )));
    }
    Ok(Some(url))
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn owned_nonempty(value: Option<String>) -> Option<String> {
    value.and_then(|value| (!value.trim().is_empty()).then_some(value))
}

#[derive(Debug, Deserialize)]
struct RawEnvelope<T> {
    status: bool,
    item: Option<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPlaylist {
    id: u64,
    name: String,
    alias: String,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    short_text: Option<String>,
    #[serde(default)]
    link: Option<String>,
    #[serde(default, rename = "feedUrl", alias = "rss", alias = "rssUrl")]
    feed_url: Option<String>,
    #[serde(default, rename = "canonicalUrl", alias = "webpageUrl")]
    webpage_url: Option<String>,
    #[serde(default)]
    clips: Option<u64>,
    #[serde(default)]
    explicit: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawClip {
    id: u64,
    name: String,
    alias: String,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    short_text: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default, rename = "canonicalUrl", alias = "webpageUrl")]
    webpage_url: Option<String>,
    #[serde(default)]
    published: Option<String>,
    #[serde(default)]
    duration: Option<u64>,
    #[serde(default)]
    explicit: Option<bool>,
    #[serde(default)]
    playlists: Vec<RawPlaylistReference>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPlaylistReference {
    id: u64,
    name: String,
    alias: String,
    #[serde(default, rename = "feedUrl", alias = "rss", alias = "rssUrl")]
    feed_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    const PLAYLIST_FIXTURE: &str = r#"{
        "status": true,
        "item": {
            "id": 1736,
            "name": "Fixture Podcast",
            "alias": "fixture-podcast",
            "image": "https://cdn.example.test/artwork.jpg",
            "text": "A fixture playlist.",
            "link": "https://publisher.example.test/podcast",
            "feedUrl": "https://feeds.example.test/podcast.xml",
            "canonicalUrl": "https://soundstream.media/playlist/fixture-podcast",
            "clips": 42,
            "explicit": false
        }
    }"#;

    const CLIP_FIXTURE: &str = r#"{
        "status": true,
        "item": {
            "id": 1905666,
            "name": "A Fixture Episode",
            "alias": "a-fixture-episode",
            "image": "https://cdn.example.test/episode.jpg",
            "text": "Long fixture description.",
            "url": "https://media.soundstream.media/get-stream-presigned-url/opaque-token?",
            "canonicalUrl": "https://soundstream.media/clip/a-fixture-episode",
            "published": "2026-03-13 17:00:00",
            "duration": 2549,
            "explicit": true,
            "playlists": [{
                "id": 1736,
                "name": "Fixture Podcast",
                "alias": "fixture-podcast",
                "rssUrl": "https://feeds.example.test/podcast.xml"
            }]
        }
    }"#;

    fn parse_link(raw: &str) -> Result<SoundStreamLink, ProviderError> {
        SoundStreamLink::parse(&Url::parse(raw).expect("fixture URL should parse"))
    }

    #[test]
    fn parses_playlist_and_clip_links() {
        assert_eq!(
            parse_link("https://soundstream.media/playlist/fixture-podcast/")
                .expect("playlist URL should parse"),
            SoundStreamLink {
                kind: SoundStreamLinkKind::Playlist,
                alias: "fixture-podcast".to_owned(),
            }
        );
        assert_eq!(
            parse_link("https://soundstream.media/clip/a-fixture-episode")
                .expect("clip URL should parse"),
            SoundStreamLink {
                kind: SoundStreamLinkKind::Clip,
                alias: "a-fixture-episode".to_owned(),
            }
        );
    }

    #[test]
    fn rejects_lookalikes_credentials_queries_and_malformed_paths() {
        for raw in [
            "http://soundstream.media/clip/example",
            "https://soundstream.media.evil.test/clip/example",
            "https://user:secret@soundstream.media/clip/example",
            "https://soundstream.media:8443/clip/example",
            "https://soundstream.media/",
            "https://soundstream.media/channel/example",
            "https://soundstream.media/clip/",
            "https://soundstream.media/clip/Example",
            "https://soundstream.media/clip/-example",
            "https://soundstream.media/clip/example?token=secret",
            "https://soundstream.media/clip/example#fragment",
            "https://soundstream.media/clip/example/extra",
        ] {
            assert!(
                parse_link(raw).is_err(),
                "fixture link should be rejected: {raw}"
            );
        }
    }

    #[test]
    fn api_urls_remain_on_the_fixed_https_origin() {
        let playlist = parse_link("https://soundstream.media/playlist/fixture-podcast")
            .expect("fixture link should parse");
        let clip = parse_link("https://soundstream.media/clip/a-fixture-episode")
            .expect("fixture link should parse");

        for (link, expected_path) in [(&playlist, "/v3/playlist"), (&clip, "/v3/clip")] {
            let url = build_api_url(link).expect("API URL should build");
            assert_eq!(url.scheme(), "https");
            assert_eq!(url.host_str(), Some("api.soundstream.media"));
            assert_eq!(url.path(), expected_path);
            assert!(
                url.query_pairs()
                    .any(|pair| { pair.0 == "alias" && pair.1 == link.alias })
            );
            assert!(
                url.query_pairs()
                    .any(|pair| { pair.0 == "askPaywallParams" && pair.1 == "1" })
            );
        }
    }

    #[test]
    fn fixture_normalizes_playlist_source_feed_and_canonical_page() {
        let response =
            serde_json::from_str(PLAYLIST_FIXTURE).expect("playlist fixture should parse");
        let link = parse_link("https://soundstream.media/playlist/fixture-podcast")
            .expect("fixture link should parse");
        let playlist =
            normalize_playlist(&link, response).expect("playlist fixture should normalize");

        assert_eq!(playlist.playlist_id, 1736);
        assert_eq!(playlist.title, "Fixture Podcast");
        assert_eq!(playlist.clip_count, Some(42));
        assert_eq!(
            playlist.webpage_url.as_str(),
            "https://soundstream.media/playlist/fixture-podcast"
        );
        assert_eq!(
            playlist
                .source_url
                .expect("fixture has source URL")
                .as_str(),
            "https://publisher.example.test/podcast"
        );
        assert_eq!(
            playlist.feed_url.expect("fixture has feed URL").as_str(),
            "https://feeds.example.test/podcast.xml"
        );
    }

    #[test]
    fn fixture_suppresses_auth_gated_stream_and_keeps_parent_feed() {
        let response = serde_json::from_str(CLIP_FIXTURE).expect("clip fixture should parse");
        let link = parse_link("https://soundstream.media/clip/a-fixture-episode")
            .expect("fixture link should parse");
        let clip = normalize_clip(&link, response).expect("clip fixture should normalize");

        assert_eq!(clip.clip_id, 1_905_666);
        assert_eq!(clip.duration_seconds, Some(2_549));
        assert_eq!(clip.explicit, Some(true));
        assert!(clip.media_url.is_none());
        assert_eq!(clip.playlists.len(), 1);
        assert_eq!(
            clip.playlists[0]
                .feed_url
                .as_ref()
                .expect("fixture parent has feed URL")
                .as_str(),
            "https://feeds.example.test/podcast.xml"
        );
    }

    #[test]
    fn public_direct_media_url_is_retained() {
        let mut fixture: Value =
            serde_json::from_str(CLIP_FIXTURE).expect("clip fixture should parse");
        fixture["item"]["url"] = Value::String("https://cdn.example.test/episode.mp3".to_owned());
        let response = serde_json::from_value(fixture).expect("modified fixture should parse");
        let link = parse_link("https://soundstream.media/clip/a-fixture-episode")
            .expect("fixture link should parse");
        let clip = normalize_clip(&link, response).expect("clip fixture should normalize");

        assert_eq!(
            clip.media_url.expect("direct media should remain").as_str(),
            "https://cdn.example.test/episode.mp3"
        );
    }

    #[test]
    fn response_alias_and_canonical_page_must_match_request() {
        let link = parse_link("https://soundstream.media/playlist/fixture-podcast")
            .expect("fixture link should parse");
        for (field, value) in [
            ("alias", "another-podcast"),
            (
                "canonicalUrl",
                "https://soundstream.media/playlist/another-podcast",
            ),
        ] {
            let mut fixture: Value =
                serde_json::from_str(PLAYLIST_FIXTURE).expect("fixture should parse");
            fixture["item"][field] = Value::String(value.to_owned());
            let response = serde_json::from_value(fixture).expect("modified fixture should parse");

            assert!(matches!(
                normalize_playlist(&link, response),
                Err(ProviderError::InvalidResponse(_))
            ));
        }
    }

    #[test]
    fn unsafe_remote_urls_are_rejected() {
        let link = parse_link("https://soundstream.media/playlist/fixture-podcast")
            .expect("fixture link should parse");
        for (field, value) in [
            ("image", "file:///tmp/artwork.jpg"),
            ("link", "https://user:secret@example.test/podcast"),
            (
                "canonicalUrl",
                "https://soundstream.media.evil.test/playlist/fixture-podcast",
            ),
        ] {
            let mut fixture: Value =
                serde_json::from_str(PLAYLIST_FIXTURE).expect("fixture should parse");
            fixture["item"][field] = Value::String(value.to_owned());
            let response = serde_json::from_value(fixture).expect("modified fixture should parse");

            assert!(matches!(
                normalize_playlist(&link, response),
                Err(ProviderError::InvalidResponse(_))
            ));
        }
    }

    #[test]
    fn unsuccessful_envelope_and_invalid_options_are_rejected() {
        let response: RawEnvelope<RawClip> =
            serde_json::from_str(r#"{"status": false}"#).expect("fixture should parse");
        let link = parse_link("https://soundstream.media/clip/a-fixture-episode")
            .expect("fixture link should parse");

        assert!(matches!(
            normalize_clip(&link, response),
            Err(ProviderError::InvalidResponse(_))
        ));
        assert!(SoundStreamResolver::with_options(Duration::ZERO, 1).is_err());
        assert!(
            SoundStreamResolver::with_options(
                Duration::from_secs(1),
                MAX_CONFIGURED_JSON_BYTES + 1
            )
            .is_err()
        );
    }
}
