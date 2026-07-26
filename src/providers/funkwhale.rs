//! Funkwhale provider backed by one configured instance's official API.
//!
//! Funkwhale's deployed stable releases and current API schema do not all
//! expose the same music API version. Youta probes the stable v1 route first,
//! falls back to v2 only on `404`, and remembers the successful version for the
//! provider lifetime. Both versions use the same documented paginated media
//! representation.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use url::Url;

use super::{
    ChannelSummary, DEFAULT_MAX_JSON_BYTES, DEFAULT_REQUEST_TIMEOUT, Provider,
    ProviderCapabilities, ProviderError, SearchFilters, SearchItem, SearchPage, SearchRequest,
    SearchSort, SearchTarget, Thumbnail, VideoDetails, VideoOrientation, VideoSummary,
    get_bounded_json, parse_rfc3339_epoch, provider_agent, resolve_http_url, validate_base_url,
};

const RESULTS_PER_PAGE: u32 = 20;
const MAX_CONFIGURED_JSON_BYTES: usize = 64 * 1024 * 1024;
const API_UNKNOWN: u8 = 0;
const API_V1: u8 = 1;
const API_V2: u8 = 2;

/// Blocking client for one configurable Funkwhale instance.
#[derive(Clone)]
pub struct FunkwhaleProvider {
    base_url: Url,
    agent: ureq::Agent,
    max_json_bytes: usize,
    detected_api: Arc<AtomicU8>,
}

impl FunkwhaleProvider {
    /// Creates a provider with conservative timeout and response limits.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] when the instance URL is invalid.
    pub fn new(base_url: Url) -> Result<Self, ProviderError> {
        Self::with_options(base_url, DEFAULT_REQUEST_TIMEOUT, DEFAULT_MAX_JSON_BYTES)
    }

    /// Creates a provider with explicit end-to-end timeout and JSON size limit.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] when the URL, timeout, or response bound is
    /// invalid.
    pub fn with_options(
        base_url: Url,
        timeout: Duration,
        max_json_bytes: usize,
    ) -> Result<Self, ProviderError> {
        let base_url = validate_base_url(base_url)?;
        if timeout.is_zero() {
            return Err(ProviderError::InvalidRequest(
                "Funkwhale timeout must be greater than zero".to_owned(),
            ));
        }
        if !(1..=MAX_CONFIGURED_JSON_BYTES).contains(&max_json_bytes) {
            return Err(ProviderError::InvalidRequest(format!(
                "JSON response limit must be between 1 and {MAX_CONFIGURED_JSON_BYTES} bytes"
            )));
        }

        Ok(Self {
            base_url,
            agent: provider_agent(timeout),
            max_json_bytes,
            detected_api: Arc::new(AtomicU8::new(API_UNKNOWN)),
        })
    }

    /// Returns the normalized configured instance URL.
    #[must_use]
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    fn validate_search(request: &SearchRequest) -> Result<(), ProviderError> {
        request.validate()?;
        if request.sort != SearchSort::Relevance {
            return Err(ProviderError::InvalidRequest(
                "Funkwhale does not expose the requested result ordering".to_owned(),
            ));
        }
        if request.filters != SearchFilters::default() {
            return Err(ProviderError::InvalidRequest(
                "Funkwhale does not support YouTube-style search filters".to_owned(),
            ));
        }
        Ok(())
    }

    fn build_search_url(
        &self,
        request: &SearchRequest,
        api_version: u8,
    ) -> Result<Url, ProviderError> {
        Self::validate_search(request)?;
        let resource = match request.target {
            SearchTarget::Videos => "tracks",
            // Funkwhale artists are the music-source analogue displayed by
            // Youta's generic channel-search panel.
            SearchTarget::Channels => "artists",
        };
        let mut url = self.api_url(api_version, resource)?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("q", request.query.trim());
            query.append_pair("page", &request.page.to_string());
            query.append_pair("page_size", &RESULTS_PER_PAGE.to_string());
            query.append_pair("playable", "true");
            query.append_pair("include_channels", "true");
        }
        Ok(url)
    }

    fn build_track_url(&self, track_id: &str, api_version: u8) -> Result<Url, ProviderError> {
        validate_track_id(track_id)?;
        let mut url = self.api_url(api_version, "tracks")?;
        url.path_segments_mut()
            .map_err(|()| ProviderError::InvalidBaseUrl("URL cannot contain API paths".to_owned()))?
            .pop_if_empty()
            .push(track_id);
        if !url.path().ends_with('/') {
            let path = format!("{}/", url.path());
            url.set_path(&path);
        }
        Ok(url)
    }

    fn api_url(&self, api_version: u8, resource: &str) -> Result<Url, ProviderError> {
        if !matches!(api_version, API_V1 | API_V2) {
            return Err(ProviderError::InvalidRequest(
                "unsupported Funkwhale API version".to_owned(),
            ));
        }
        self.base_url
            .join(&format!("api/v{api_version}/{resource}/"))
            .map_err(|error| ProviderError::InvalidBaseUrl(error.to_string()))
    }

    fn fetch_search_page(
        &self,
        request: &SearchRequest,
        api_version: u8,
    ) -> Result<SearchPage, ProviderError> {
        let url = self.build_search_url(request, api_version)?;
        match request.target {
            SearchTarget::Videos => {
                let raw: RawPage<RawTrack> =
                    get_bounded_json(&self.agent, &url, self.max_json_bytes)?;
                self.convert_track_page(raw, request.page)
            }
            SearchTarget::Channels => {
                let raw: RawPage<RawArtist> =
                    get_bounded_json(&self.agent, &url, self.max_json_bytes)?;
                self.convert_artist_page(raw, request.page)
            }
        }
    }

    fn fetch_track_details(
        &self,
        track_id: &str,
        api_version: u8,
    ) -> Result<VideoDetails, ProviderError> {
        let url = self.build_track_url(track_id, api_version)?;
        let raw: RawTrack = get_bounded_json(&self.agent, &url, self.max_json_bytes)?;
        self.convert_track_details(raw)
    }

    fn with_detected_api<T>(
        &self,
        request: impl Fn(u8) -> Result<T, ProviderError>,
    ) -> Result<T, ProviderError> {
        let detected = self.detected_api.load(Ordering::Relaxed);
        if matches!(detected, API_V1 | API_V2) {
            return request(detected);
        }

        match request(API_V1) {
            Ok(value) => {
                self.detected_api.store(API_V1, Ordering::Relaxed);
                Ok(value)
            }
            Err(ProviderError::HttpStatus(404)) => {
                let value = request(API_V2)?;
                self.detected_api.store(API_V2, Ordering::Relaxed);
                Ok(value)
            }
            Err(error) => Err(error),
        }
    }

    fn convert_track_page(
        &self,
        raw: RawPage<RawTrack>,
        page: u32,
    ) -> Result<SearchPage, ProviderError> {
        let returned = raw.results.len();
        let has_more = page_has_more(page, returned, raw.count, raw.next.as_deref());
        let items = raw
            .results
            .into_iter()
            .map(|track| self.convert_track_summary(track).map(SearchItem::Video))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(SearchPage {
            page,
            items,
            next_page: has_more.then_some(page.saturating_add(1)),
        })
    }

    fn convert_artist_page(
        &self,
        raw: RawPage<RawArtist>,
        page: u32,
    ) -> Result<SearchPage, ProviderError> {
        let returned = raw.results.len();
        let has_more = page_has_more(page, returned, raw.count, raw.next.as_deref());
        let items = raw
            .results
            .into_iter()
            .map(|artist| self.convert_artist_summary(artist).map(SearchItem::Channel))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(SearchPage {
            page,
            items,
            next_page: has_more.then_some(page.saturating_add(1)),
        })
    }

    fn convert_track_summary(&self, raw: RawTrack) -> Result<VideoSummary, ProviderError> {
        validate_raw_track(&raw)?;
        let (channel_id, channel_name) = track_artist(&raw)?;
        let webpage_url = self.local_webpage("tracks", raw.id);
        let stream_url = self.track_stream_url(&raw)?;
        let published_at = raw.creation_date.as_deref().and_then(parse_rfc3339_epoch);
        let thumbnails = self.track_thumbnails(&raw);

        Ok(VideoSummary {
            video_id: raw.id.to_string(),
            title: raw.title,
            channel_name,
            channel_id,
            description: content_text(raw.description),
            duration_seconds: track_duration(&raw.uploads),
            // Funkwhale exposes download counts, not equivalent view counts.
            view_count: None,
            published_at,
            published_text: raw.creation_date,
            live: false,
            orientation: VideoOrientation::Unknown,
            thumbnails,
            webpage_url,
            stream_url,
        })
    }

    fn convert_artist_summary(&self, raw: RawArtist) -> Result<ChannelSummary, ProviderError> {
        if raw.id == 0 || raw.name.trim().is_empty() {
            return Err(ProviderError::InvalidResponse(
                "Funkwhale artist requires a positive ID and name".to_owned(),
            ));
        }

        Ok(ChannelSummary {
            channel_id: raw.id.to_string(),
            name: raw.name,
            description: content_text(raw.description),
            subscriber_count: None,
            video_count: raw.tracks_count,
            created_at: None,
            auto_generated: false,
            thumbnails: self.cover_thumbnails(raw.cover.as_ref()),
            webpage_url: self.local_webpage("artists", raw.id),
        })
    }

    fn convert_track_details(&self, raw: RawTrack) -> Result<VideoDetails, ProviderError> {
        validate_raw_track(&raw)?;
        let (channel_id, channel_name) = track_artist(&raw)?;
        let webpage_url = self.local_webpage("tracks", raw.id);
        let stream_url = self.track_stream_url(&raw)?;
        let published_at = raw.creation_date.as_deref().and_then(parse_rfc3339_epoch);
        let thumbnails = self.track_thumbnails(&raw);

        Ok(VideoDetails {
            video_id: raw.id.to_string(),
            title: raw.title,
            channel_name,
            channel_id,
            description: content_text(raw.description),
            duration_seconds: track_duration(&raw.uploads),
            view_count: None,
            like_count: None,
            published_at,
            published_text: raw.creation_date,
            license: raw.license.filter(|value| !value.trim().is_empty()),
            rating: None,
            ratings_allowed: None,
            live: false,
            keywords: raw.tags,
            orientation: VideoOrientation::Unknown,
            thumbnails,
            webpage_url,
            stream_url,
        })
    }

    fn track_stream_url(&self, raw: &RawTrack) -> Result<Option<Url>, ProviderError> {
        raw.listen_url
            .as_deref()
            .or_else(|| {
                raw.uploads
                    .iter()
                    .find_map(|upload| upload.listen_url.as_deref())
            })
            .map(|url| resolve_http_url(&self.base_url, url))
            .transpose()
    }

    fn track_thumbnails(&self, raw: &RawTrack) -> Vec<Thumbnail> {
        self.cover_thumbnails(
            raw.cover
                .as_ref()
                .or_else(|| raw.album.as_ref().and_then(|album| album.cover.as_ref())),
        )
    }

    fn cover_thumbnails(&self, cover: Option<&RawCover>) -> Vec<Thumbnail> {
        let Some(cover) = cover else {
            return Vec::new();
        };
        let preferred = [
            "large_square_crop",
            "medium_square_crop",
            "small_square_crop",
            "original",
            "source",
        ];
        let mut thumbnails = Vec::new();
        for key in preferred {
            let Some(raw_url) = cover.urls.get(key).and_then(Value::as_str) else {
                continue;
            };
            let Ok(url) = resolve_http_url(&self.base_url, raw_url) else {
                continue;
            };
            if thumbnails
                .iter()
                .any(|thumbnail: &Thumbnail| thumbnail.url == url)
            {
                continue;
            }
            thumbnails.push(Thumbnail {
                url,
                quality: Some(key.to_owned()),
                width: None,
                height: None,
            });
        }
        thumbnails
    }

    fn local_webpage(&self, resource: &str, id: u64) -> Option<Url> {
        let mut url = self.base_url.join(&format!("library/{resource}/")).ok()?;
        url.path_segments_mut()
            .ok()?
            .pop_if_empty()
            .push(&id.to_string());
        Some(url)
    }
}

impl Provider for FunkwhaleProvider {
    fn id(&self) -> &'static str {
        "funkwhale"
    }

    fn display_name(&self) -> &'static str {
        "Funkwhale"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            video_search: true,
            channel_search: true,
            pagination: true,
            search_filters: false,
            search_sorting: false,
            video_details: true,
            thumbnails: true,
        }
    }

    fn search(&self, request: &SearchRequest) -> Result<SearchPage, ProviderError> {
        Self::validate_search(request)?;
        self.with_detected_api(|version| self.fetch_search_page(request, version))
    }

    fn video_details(&self, video_id: &str) -> Result<VideoDetails, ProviderError> {
        validate_track_id(video_id)?;
        self.with_detected_api(|version| self.fetch_track_details(video_id, version))
    }
}

fn validate_track_id(track_id: &str) -> Result<(), ProviderError> {
    if track_id.is_empty()
        || track_id.len() > 20
        || !track_id.bytes().all(|byte| byte.is_ascii_digit())
        || track_id == "0"
    {
        return Err(ProviderError::InvalidRequest(
            "Funkwhale track ID must be a positive decimal integer".to_owned(),
        ));
    }
    Ok(())
}

fn validate_raw_track(raw: &RawTrack) -> Result<(), ProviderError> {
    if raw.id == 0 || raw.title.trim().is_empty() {
        return Err(ProviderError::InvalidResponse(
            "Funkwhale track requires a positive ID and title".to_owned(),
        ));
    }
    Ok(())
}

fn track_artist(raw: &RawTrack) -> Result<(String, String), ProviderError> {
    let credits = if raw.artist_credit.is_empty() {
        raw.album
            .as_ref()
            .map_or(&[][..], |album| album.artist_credit.as_slice())
    } else {
        raw.artist_credit.as_slice()
    };
    let mut names = Vec::new();
    let mut first_id = None;
    for credit in credits {
        let name = credit
            .name
            .as_deref()
            .or(credit.credit.as_deref())
            .or_else(|| credit.artist.as_ref().map(|artist| artist.name.as_str()))
            .map(str::trim)
            .filter(|name| !name.is_empty());
        if let Some(name) = name {
            names.push(name.to_owned());
        }
        if first_id.is_none() {
            first_id = credit
                .artist
                .as_ref()
                .and_then(|artist| (artist.id > 0).then(|| artist.id.to_string()));
        }
    }
    if names.is_empty() {
        return Err(ProviderError::InvalidResponse(
            "Funkwhale track has no artist credit".to_owned(),
        ));
    }
    let display_name = names.join(", ");
    Ok((
        first_id.unwrap_or_else(|| display_name.clone()),
        display_name,
    ))
}

fn track_duration(uploads: &[RawUpload]) -> Option<u64> {
    uploads.iter().find_map(|upload| upload.duration)
}

fn content_text(content: Option<RawContent>) -> String {
    match content {
        None => String::new(),
        Some(RawContent::Text(text)) => text,
        Some(RawContent::Object {
            text,
            content,
            html,
        }) => text
            .or(content)
            .or_else(|| html.map(|value| strip_markup(&value)))
            .unwrap_or_default(),
    }
}

fn strip_markup(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut in_tag = false;
    for character in value.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(character),
            _ => {}
        }
    }
    result
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn page_has_more(page: u32, returned: usize, total: u64, next: Option<&str>) -> bool {
    if next.is_some_and(|url| !url.trim().is_empty()) {
        return true;
    }
    let start = u64::from(page.saturating_sub(1)) * u64::from(RESULTS_PER_PAGE);
    start.saturating_add(u64::try_from(returned).unwrap_or(u64::MAX)) < total
}

#[derive(Debug, Deserialize)]
struct RawPage<T> {
    #[serde(default, deserialize_with = "deserialize_u64")]
    count: u64,
    #[serde(default)]
    next: Option<String>,
    results: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct RawTrack {
    #[serde(default, deserialize_with = "deserialize_u64")]
    id: u64,
    title: String,
    #[serde(default)]
    creation_date: Option<String>,
    #[serde(default)]
    listen_url: Option<String>,
    #[serde(default)]
    artist_credit: Vec<RawArtistCredit>,
    #[serde(default)]
    album: Option<RawAlbum>,
    #[serde(default)]
    uploads: Vec<RawUpload>,
    #[serde(default)]
    description: Option<RawContent>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    cover: Option<RawCover>,
}

#[derive(Debug, Deserialize)]
struct RawArtist {
    #[serde(default, deserialize_with = "deserialize_u64")]
    id: u64,
    name: String,
    #[serde(default)]
    description: Option<RawContent>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    tracks_count: Option<u64>,
    #[serde(default)]
    cover: Option<RawCover>,
}

#[derive(Debug, Deserialize)]
struct RawAlbum {
    #[serde(default)]
    artist_credit: Vec<RawArtistCredit>,
    #[serde(default)]
    cover: Option<RawCover>,
}

#[derive(Debug, Deserialize)]
struct RawArtistCredit {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    credit: Option<String>,
    #[serde(default)]
    artist: Option<RawArtistReference>,
}

#[derive(Debug, Deserialize)]
struct RawArtistReference {
    #[serde(default, deserialize_with = "deserialize_u64")]
    id: u64,
    name: String,
}

#[derive(Debug, Deserialize)]
struct RawUpload {
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    duration: Option<u64>,
    #[serde(default)]
    listen_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawCover {
    #[serde(default)]
    urls: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawContent {
    Text(String),
    Object {
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        content: Option<String>,
        #[serde(default)]
        html: Option<String>,
    },
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

    const TRACK_PAGE_FIXTURE: &str = r#"{
        "count": 21,
        "next": "https://funk.example/api/v1/tracks/?page=2",
        "results": [{
            "id": 42,
            "title": "Open Track",
            "creation_date": "2024-01-02T03:04:05Z",
            "listen_url": "/api/v1/listen/12345678-1234-1234-1234-123456789abc/",
            "artist_credit": [{
                "artist": {"id": 7, "name": "Free Artist"}
            }],
            "album": {
                "cover": {
                    "urls": {
                        "medium_square_crop": "/media/albums/cover.jpg"
                    }
                }
            },
            "uploads": [{"duration": "185"}],
            "description": {"text": "Track description"},
            "license": "cc-by-sa-4.0",
            "tags": ["instrumental"]
        }]
    }"#;

    const ARTIST_PAGE_FIXTURE: &str = r#"{
        "count": 1,
        "next": null,
        "results": [{
            "id": "7",
            "name": "Free Artist",
            "description": {"html": "<p>Artist &amp; composer</p>"},
            "tracks_count": 12,
            "cover": {
                "urls": {
                    "large_square_crop": "https://cdn.example/artist.jpg"
                }
            }
        }]
    }"#;

    fn provider() -> FunkwhaleProvider {
        FunkwhaleProvider::new(
            Url::parse("https://funk.example.test/prefix").expect("fixture URL should parse"),
        )
        .expect("fixture provider should construct")
    }

    #[test]
    fn search_urls_use_official_versioned_resources() {
        let mut tracks = SearchRequest::new("open music", SearchTarget::Videos);
        tracks.page = 2;
        let tracks_url = provider()
            .build_search_url(&tracks, API_V1)
            .expect("track search URL should build");
        let artists_url = provider()
            .build_search_url(
                &SearchRequest::new("artist", SearchTarget::Channels),
                API_V2,
            )
            .expect("artist search URL should build");

        assert_eq!(tracks_url.path(), "/prefix/api/v1/tracks/");
        assert!(
            tracks_url
                .query_pairs()
                .any(|pair| pair == ("page".into(), "2".into()))
        );
        assert_eq!(artists_url.path(), "/prefix/api/v2/artists/");
    }

    #[test]
    fn track_fixture_exposes_validated_stream_and_cover_urls() {
        let raw = serde_json::from_str(TRACK_PAGE_FIXTURE).expect("fixture should parse");
        let page = provider()
            .convert_track_page(raw, 1)
            .expect("fixture page should convert");
        let [SearchItem::Video(track)] = page.items.as_slice() else {
            panic!("expected one track");
        };

        assert_eq!(page.next_page, Some(2));
        assert_eq!(track.duration_seconds, Some(185));
        assert_eq!(track.channel_id, "7");
        assert_eq!(track.published_at, Some(1_704_164_645));
        assert_eq!(
            track
                .stream_url
                .as_ref()
                .expect("fixture has a stream")
                .as_str(),
            "https://funk.example.test/api/v1/listen/12345678-1234-1234-1234-123456789abc/"
        );
        assert_eq!(
            track.thumbnails[0].url.as_str(),
            "https://funk.example.test/media/albums/cover.jpg"
        );
    }

    #[test]
    fn artist_fixture_maps_to_channel_search_shape() {
        let raw = serde_json::from_str(ARTIST_PAGE_FIXTURE).expect("fixture should parse");
        let page = provider()
            .convert_artist_page(raw, 1)
            .expect("fixture page should convert");
        let [SearchItem::Channel(artist)] = page.items.as_slice() else {
            panic!("expected one artist");
        };

        assert_eq!(page.next_page, None);
        assert_eq!(artist.video_count, Some(12));
        assert_eq!(artist.description, "Artist & composer");
        assert_eq!(
            artist
                .webpage_url
                .as_ref()
                .expect("local artist route should exist")
                .as_str(),
            "https://funk.example.test/prefix/library/artists/7"
        );
    }

    #[test]
    fn unsafe_advertised_stream_is_rejected() {
        let mut raw: RawPage<RawTrack> =
            serde_json::from_str(TRACK_PAGE_FIXTURE).expect("fixture should parse");
        raw.results[0].listen_url = Some("file:///etc/passwd".to_owned());

        assert!(matches!(
            provider().convert_track_page(raw, 1),
            Err(ProviderError::InvalidResponse(_))
        ));
    }

    #[test]
    fn unsupported_filter_and_track_path_are_rejected() {
        let mut request = SearchRequest::new("music", SearchTarget::Videos);
        request.filters.region = Some("GB".to_owned());
        assert!(matches!(
            provider().build_search_url(&request, API_V1),
            Err(ProviderError::InvalidRequest(_))
        ));
        assert!(matches!(
            provider().build_track_url("../secret", API_V1),
            Err(ProviderError::InvalidRequest(_))
        ));
    }
}
