//! Funkwhale provider backed by one configured instance's official API.
//!
//! Funkwhale's deployed stable releases and current API schema do not all
//! expose the same music API version. Youta probes the stable v1 route first,
//! falls back to v2 only on `404`, and remembers the successful version for the
//! provider lifetime. Both versions use the same documented paginated media
//! representation.

mod albums;
mod discovery;
mod text;
mod upload;
mod uploader;

pub use albums::{FunkwhaleAlbum, FunkwhaleAlbumPage};
pub use discovery::{FunkwhalePod, PUBLIC_PODS_URL, discover_public_pods, parse_public_pods};
pub use upload::FunkwhaleUploadMetadata;
pub use uploader::FunkwhaleUploader;

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
    page_size: u32,
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
            page_size: RESULTS_PER_PAGE,
        })
    }

    /// Returns the normalized configured instance URL.
    #[must_use]
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// Recovers the exact pod base from a pod-scoped canonical track identity.
    ///
    /// # Errors
    ///
    /// Rejects legacy numeric IDs without an owner, unsafe URLs, and invalid track paths.
    pub fn instance_url_from_track_id(identity: &str) -> Result<Url, ProviderError> {
        instance_url_from_identity(identity, "tracks")
    }

    /// Recovers the original pod from an exact, credential-free artist page.
    ///
    /// # Errors
    ///
    /// Rejects unowned numeric IDs, unsafe URLs, and noncanonical artist paths.
    pub fn instance_url_from_artist_id(identity: &str) -> Result<Url, ProviderError> {
        instance_url_from_identity(identity, "artists")
    }

    /// Recovers the original pod from an exact, credential-free album page.
    ///
    /// # Errors
    ///
    /// Rejects unsafe URLs and paths without a positive pod-local album identity.
    pub fn instance_url_from_album_id(identity: &str) -> Result<Url, ProviderError> {
        instance_url_from_identity(identity, "albums")
    }

    /// Selects one to one hundred results per page; an unchanged provider uses twenty.
    #[must_use]
    pub fn with_page_size(mut self, page_size: usize) -> Self {
        self.page_size = u32::try_from(page_size).unwrap_or(u32::MAX).clamp(1, 100);
        self
    }

    /// Returns playable tracks in newest catalogue-record order without a search term.
    ///
    /// # Errors
    ///
    /// Returns a provider error for invalid pagination or an unavailable pod API.
    pub fn recent_tracks(&self, page: u32) -> Result<SearchPage, ProviderError> {
        self.with_detected_api(|version| self.fetch_catalogue_page(None, page, version))
    }

    /// Lists this pod's exact artist catalogue grouped by album and track position.
    ///
    /// # Errors
    ///
    /// Rejects another pod's artist identity and propagates bounded API failures.
    pub fn artist_tracks(&self, artist: &str, page: u32) -> Result<SearchPage, ProviderError> {
        self.with_detected_api(|version| self.fetch_catalogue_page(Some(artist), page, version))
    }

    /// Lists one original-pod album in disc and track order without a title search.
    ///
    /// # Errors
    ///
    /// Rejects noncanonical or cross-pod album URLs and propagates bounded API failures.
    pub fn album_tracks(&self, album: &str, page: u32) -> Result<SearchPage, ProviderError> {
        self.with_detected_api(|version| {
            let url = self.build_album_catalogue_url(album, page, version)?;
            let raw = get_bounded_json(&self.agent, &url, self.max_json_bytes)?;
            self.convert_track_page(raw, page)
        })
    }

    /// Builds the documented exact album filter with deterministic multi-disc ordering.
    fn build_album_catalogue_url(
        &self,
        album: &str,
        page: u32,
        version: u8,
    ) -> Result<Url, ProviderError> {
        let prefix = self
            .base_url
            .join("library/albums/")
            .map_err(|error| ProviderError::InvalidBaseUrl(error.to_string()))?;
        let id = album
            .strip_prefix(prefix.as_str())
            .map(|id| id.strip_suffix('/').unwrap_or(id))
            .filter(|id| validate_track_id(id).is_ok())
            .filter(|id| {
                id.parse::<u64>()
                    .is_ok_and(|value| value.to_string() == *id)
            })
            .ok_or_else(|| {
                ProviderError::InvalidRequest(
                    "Funkwhale album must be this pod's original album page".to_owned(),
                )
            })?;
        let mut url = self.build_catalogue_url(None, page, version)?;
        let pairs = url
            .query_pairs()
            .filter(|(key, _)| key != "ordering")
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        url.set_query(None);
        url.query_pairs_mut()
            .extend_pairs(pairs)
            .append_pair("album", id)
            .append_pair("ordering", "disc_number,position");
        Ok(url)
    }

    /// Browses the exact advertised tag without converting it into a title query.
    ///
    /// # Errors
    ///
    /// Rejects empty/control-containing tags and propagates bounded pod API failures.
    pub fn tag_tracks(&self, tag: &str, page: u32) -> Result<SearchPage, ProviderError> {
        self.with_detected_api(|version| {
            let url = self.build_tag_catalogue_url(tag, page, version)?;
            let raw = get_bounded_json(&self.agent, &url, self.max_json_bytes)?;
            self.convert_track_page(raw, page)
        })
    }

    /// URL-encodes one exact Funkwhale tag filter, preserving spaces and Unicode.
    fn build_tag_catalogue_url(
        &self,
        tag: &str,
        page: u32,
        version: u8,
    ) -> Result<Url, ProviderError> {
        if !valid_tag(tag) {
            return Err(ProviderError::InvalidRequest(
                "Funkwhale tag must be a bounded nonempty visible value".to_owned(),
            ));
        }
        let mut url = self.build_catalogue_url(None, page, version)?;
        url.query_pairs_mut().append_pair("tag", tag);
        Ok(url)
    }

    /// Validates a canonical artist page without allowing credentials or another pod.
    ///
    /// # Errors
    ///
    /// Returns a provider error unless the URL identifies a positive pod-local artist ID.
    pub fn validate_artist_identity(&self, identity: &str) -> Result<(), ProviderError> {
        self.artist_numeric_id(identity).map(|_| ())
    }

    /// Converts only the configured pod's canonical artist identity into an API filter.
    fn artist_numeric_id(&self, identity: &str) -> Result<String, ProviderError> {
        let prefix = self
            .base_url
            .join("library/artists/")
            .map_err(|error| ProviderError::InvalidBaseUrl(error.to_string()))?;
        let id = identity
            .strip_prefix(prefix.as_str())
            .map(|id| id.strip_suffix('/').unwrap_or(id))
            .filter(|id| validate_track_id(id).is_ok())
            .ok_or_else(|| {
                ProviderError::InvalidRequest(
                    "Funkwhale artist must be this pod's original artist page".to_owned(),
                )
            })?;
        Ok(id.parse::<u64>().expect("validated artist ID").to_string())
    }

    /// Creates an empty-query catalogue request with an explicit stable ordering.
    fn build_catalogue_url(
        &self,
        artist: Option<&str>,
        page: u32,
        version: u8,
    ) -> Result<Url, ProviderError> {
        if page == 0 {
            return Err(ProviderError::InvalidRequest(
                "Funkwhale page must be positive".to_owned(),
            ));
        }
        let artist_id = artist
            .map(|identity| self.artist_numeric_id(identity))
            .transpose()?;
        let mut url = self.api_url(version, "tracks")?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("q", "");
            query.append_pair("page", &page.to_string());
            query.append_pair("page_size", &self.page_size.to_string());
            query.append_pair("playable", "true");
            query.append_pair("include_channels", "true");
            query.append_pair(
                "ordering",
                if artist_id.is_some() {
                    "album__title,disc_number,position"
                } else {
                    "-creation_date"
                },
            );
            if let Some(id) = artist_id {
                query.append_pair("artist", &id);
            }
        }
        Ok(url)
    }

    /// Reads one bounded catalogue page using the same visibility filter as searches.
    fn fetch_catalogue_page(
        &self,
        artist: Option<&str>,
        page: u32,
        version: u8,
    ) -> Result<SearchPage, ProviderError> {
        let url = self.build_catalogue_url(artist, page, version)?;
        let raw = get_bounded_json(&self.agent, &url, self.max_json_bytes)?;
        self.convert_track_page(raw, page)
    }

    /// Resolves an advertised audio URL without treating a missing URL as authentication.
    ///
    /// Tracks may have metadata but no accessible upload. Explicit pod authentication
    /// failures remain [`ProviderError::HttpStatus`] values, distinct from this case.
    /// The returned listen URL supports direct streaming and the API's default download.
    ///
    /// # Errors
    ///
    /// Returns a provider error for invalid/cross-pod identities, API failures, or
    /// a track that does not advertise an accessible audio stream.
    pub fn resolve_stream_url(&self, video_id: &str) -> Result<Url, ProviderError> {
        self.video_details(video_id)?.stream_url.ok_or_else(|| {
            ProviderError::InvalidResponse(
                "This Funkwhale track has no accessible audio upload or advertised stream URL"
                    .to_owned(),
            )
        })
    }

    /// Adds optional actual uploader/date facts only for an explicitly selected track.
    ///
    /// Search pages and playback resolution never request this extra upload record.
    /// Pods may deny its optional endpoint; missing facts do not prevent playback.
    ///
    /// # Errors
    ///
    /// Returns an error only when the track itself cannot be read or validated.
    pub fn video_details_with_upload_metadata(
        &self,
        video_id: &str,
    ) -> Result<(VideoDetails, Option<FunkwhaleUploadMetadata>), ProviderError> {
        let track_id = self.numeric_track_id(video_id)?;
        self.with_detected_api(|version| {
            let url = self.build_track_url(&track_id, version)?;
            let raw: RawTrack = get_bounded_json(&self.agent, &url, self.max_json_bytes)?;
            validate_raw_track(&raw)?;
            let metadata = self.selected_upload_metadata(&raw, version, |url| {
                upload::fetch_upload_metadata(&self.agent, url, self.max_json_bytes)
            });
            self.convert_track_details(raw)
                .map(|details| (details, metadata))
        })
    }

    /// Keeps denied/unsupported upload metadata optional and tied to the first advertised upload.
    fn selected_upload_metadata(
        &self,
        track: &RawTrack,
        version: u8,
        fetch: impl FnOnce(&Url) -> Result<FunkwhaleUploadMetadata, ProviderError>,
    ) -> Option<FunkwhaleUploadMetadata> {
        let uuid = track.uploads.first()?.uuid.as_deref()?;
        let url = self.build_upload_url(uuid, version)?;
        fetch(&url).ok()
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
            query.append_pair("page_size", &self.page_size.to_string());
            query.append_pair("playable", "true");
            query.append_pair("include_channels", "true");
        }
        Ok(url)
    }

    fn build_track_url(&self, track_id: &str, api_version: u8) -> Result<Url, ProviderError> {
        let track_id = self.numeric_track_id(track_id)?;
        let mut url = self.api_url(api_version, "tracks")?;
        url.path_segments_mut()
            .map_err(|()| ProviderError::InvalidBaseUrl("URL cannot contain API paths".to_owned()))?
            .pop_if_empty()
            .push(&track_id);
        if !url.path().ends_with('/') {
            let path = format!("{}/", url.path());
            url.set_path(&path);
        }
        Ok(url)
    }

    /// Accepts only an advertised UUID component, never a remote upload URL or path.
    fn build_upload_url(&self, uuid: &str, version: u8) -> Option<Url> {
        if uuid.len() != 36
            || !uuid.bytes().enumerate().all(|(index, byte)| {
                if matches!(index, 8 | 13 | 18 | 23) {
                    byte == b'-'
                } else {
                    byte.is_ascii_hexdigit()
                }
            })
        {
            return None;
        }
        self.api_url(version, "uploads")
            .ok()?
            .join(&format!("{uuid}/"))
            .ok()
    }

    /// Accepts legacy numeric IDs or canonical page identities owned by this exact pod.
    fn numeric_track_id(&self, identity: &str) -> Result<String, ProviderError> {
        if validate_track_id(identity).is_ok() {
            return Ok(identity
                .parse::<u64>()
                .expect("validated decimal track ID")
                .to_string());
        }
        let url = Url::parse(identity).map_err(|_| {
            ProviderError::InvalidRequest(
                "Funkwhale track identity must be a positive ID or this pod's track page URL"
                    .to_owned(),
            )
        })?;
        let prefix = self
            .base_url
            .join("library/tracks/")
            .map_err(|error| ProviderError::InvalidBaseUrl(error.to_string()))?;
        let id = url
            .as_str()
            .strip_prefix(prefix.as_str())
            .map(|id| id.strip_suffix('/').unwrap_or(id))
            .filter(|id| validate_track_id(id).is_ok())
            .ok_or_else(|| {
                ProviderError::InvalidRequest(
                    "Funkwhale track page belongs to another pod or has an invalid track ID"
                        .to_owned(),
                )
            })?;
        Ok(id
            .parse::<u64>()
            .expect("validated decimal track ID")
            .to_string())
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
        let has_more = page_has_more(
            page,
            returned,
            raw.count,
            raw.next.as_deref(),
            self.page_size,
        );
        let items = raw
            .results
            .into_iter()
            .filter(|track| track.is_playable != Some(false))
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
        let has_more = page_has_more(
            page,
            returned,
            raw.count,
            raw.next.as_deref(),
            self.page_size,
        );
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
        let description = self.track_description(&raw);

        Ok(VideoSummary {
            video_id: self.scoped_identity("tracks", raw.id),
            title: display_text(&raw.title),
            channel_name: display_text(&channel_name),
            channel_id: self.artist_identity(&channel_id),
            description,
            duration_seconds: track_duration(&raw.uploads),
            // Funkwhale exposes download counts, not equivalent view counts.
            view_count: None,
            published_at,
            published_text: raw.creation_date,
            live: false,
            was_live: false,
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
            channel_id: self.scoped_identity("artists", raw.id),
            name: display_text(&raw.name),
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
        let description = self.track_description(&raw);

        Ok(VideoDetails {
            video_id: self.scoped_identity("tracks", raw.id),
            title: display_text(&raw.title),
            channel_name: display_text(&channel_name),
            channel_id: self.artist_identity(&channel_id),
            description,
            duration_seconds: track_duration(&raw.uploads),
            view_count: None,
            like_count: None,
            comment_count: None,
            published_at,
            published_text: raw.creation_date,
            license: raw.license.filter(|value| !value.trim().is_empty()),
            rating: None,
            ratings_allowed: None,
            live: false,
            was_live: false,
            keywords: normalized_tags(raw.tags),
            orientation: VideoOrientation::Unknown,
            thumbnails,
            webpage_url,
            stream_url,
        })
    }

    /// Uses advertised track/upload URLs because numeric IDs cannot reconstruct UUID listen routes.
    fn track_stream_url(&self, raw: &RawTrack) -> Result<Option<Url>, ProviderError> {
        if raw.is_playable == Some(false) {
            return Ok(None);
        }
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

    /// Retains album artwork as lazy fallback without replacing an advertised track cover.
    ///
    /// Federated pods can advertise a broken track attachment beside a usable
    /// album attachment. Preserve both identities so renderers can try the
    /// album only after the selected track's own image fails validation.
    fn track_thumbnails(&self, raw: &RawTrack) -> Vec<Thumbnail> {
        let mut thumbnails = self.cover_thumbnails(raw.cover.as_ref());
        for mut thumbnail in
            self.cover_thumbnails(raw.album.as_ref().and_then(|album| album.cover.as_ref()))
        {
            if thumbnails
                .iter()
                .any(|existing| existing.url == thumbnail.url)
            {
                continue;
            }
            thumbnail.quality = thumbnail.quality.map(|quality| format!("album_{quality}"));
            thumbnails.push(thumbnail);
        }
        thumbnails
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

    /// Keeps pod-local numeric IDs distinct in persistent queues, notes, and caches.
    fn scoped_identity(&self, resource: &str, id: u64) -> String {
        format!("{}library/{resource}/{id}", self.base_url)
    }

    /// Scopes artist credits even when a pod omits their optional numeric identity.
    fn artist_identity(&self, artist: &str) -> String {
        if let Ok(id) = artist.parse::<u64>()
            && id > 0
        {
            return self.scoped_identity("artists", id);
        }
        let mut url = self.base_url.clone();
        url.set_fragment(Some(&format!("artist:{artist}")));
        url.to_string()
    }

    /// Keeps album identity and advertised upload facts beside visible description text.
    fn track_description(&self, track: &RawTrack) -> String {
        let mut description = track_description(track);
        if let Some(album) = track.album.as_ref().filter(|album| album.id > 0)
            && let Some(page) = self.local_webpage("albums", album.id)
        {
            if !description.is_empty() {
                description.push('\n');
            }
            description.push_str(&format!("Album page: {page}"));
        }
        description
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
            video_comments: false,
            thumbnails: true,
        }
    }

    fn search(&self, request: &SearchRequest) -> Result<SearchPage, ProviderError> {
        Self::validate_search(request)?;
        self.with_detected_api(|version| self.fetch_search_page(request, version))
    }

    fn video_details(&self, video_id: &str) -> Result<VideoDetails, ProviderError> {
        let track_id = self.numeric_track_id(video_id)?;
        self.with_detected_api(|version| self.fetch_track_details(&track_id, version))
    }
}

/// Parses a canonical library identity without allocating an HTTP client.
fn instance_url_from_identity(identity: &str, resource: &str) -> Result<Url, ProviderError> {
    let mut url = Url::parse(identity).map_err(|error| {
        ProviderError::InvalidRequest(format!(
            "Funkwhale identity requires its original pod URL: {error}"
        ))
    })?;
    validate_base_url(url.clone())?;
    let marker = format!("/library/{resource}/");
    let (prefix, id) = url.path().rsplit_once(&marker).ok_or_else(|| {
        ProviderError::InvalidRequest(format!(
            "Funkwhale identity has no canonical {resource} path"
        ))
    })?;
    validate_track_id(id.strip_suffix('/').unwrap_or(id))?;
    let path = format!("{prefix}/");
    url.set_path(&path);
    validate_base_url(url)
}

fn validate_track_id(track_id: &str) -> Result<(), ProviderError> {
    if track_id.is_empty()
        || track_id.len() > 20
        || !track_id.bytes().all(|byte| byte.is_ascii_digit())
        || !track_id.parse::<u64>().is_ok_and(|id| id > 0)
    {
        return Err(ProviderError::InvalidRequest(
            "Funkwhale track ID must be a positive decimal integer".to_owned(),
        ));
    }
    Ok(())
}

/// Retains one exact display/filter value only when it can produce a bounded action.
fn valid_tag(tag: &str) -> bool {
    !tag.trim().is_empty() && tag.len() <= 1024 && !tag.chars().any(char::is_control)
}

/// Bounds inline link targets without changing accepted Unicode spelling or spacing.
fn normalized_tags(tags: Vec<String>) -> Vec<String> {
    let mut values = Vec::new();
    for tag in tags.into_iter().take(4096) {
        if valid_tag(&tag) && !values.contains(&tag) {
            values.push(tag);
            if values.len() == 128 {
                break;
            }
        }
    }
    values
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
            .filter(|name| !name.trim().is_empty())
            .or_else(|| {
                credit
                    .credit
                    .as_deref()
                    .filter(|name| !name.trim().is_empty())
            })
            .or_else(|| {
                credit
                    .artist
                    .as_ref()
                    .and_then(|artist| artist.name.as_deref())
            })
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
    if names.is_empty()
        && let Some(artist) = raw
            .artist
            .as_ref()
            .or_else(|| raw.album.as_ref().and_then(|album| album.artist.as_ref()))
    {
        if let Some(name) = artist
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            names.push(name.to_owned());
        }
        first_id = (artist.id > 0).then(|| artist.id.to_string());
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
    let (value, declared_html) = match content {
        None => return String::new(),
        Some(RawContent::Text(text)) => return text::plain_description(&text, false),
        Some(RawContent::Object {
            text,
            content,
            html,
            content_type,
        }) => {
            let declared_html = content_type
                .as_deref()
                .is_some_and(|kind| kind.eq_ignore_ascii_case("text/html"));
            if let Some(value) = text.or(content) {
                (value, declared_html)
            } else {
                (html.unwrap_or_default(), true)
            }
        }
    };
    text::plain_description(&value, declared_html)
}

/// Repairs only a strong legacy encoding signature; identities and locators stay exact.
fn display_text(value: &str) -> String {
    crate::legacy_text::normalized_legacy_windows_1251_value(Some(value)).unwrap_or_default()
}

/// Adds facts advertised by one upload without inventing listening counts from downloads.
fn track_description(track: &RawTrack) -> String {
    let mut lines = Vec::new();
    if let Some(album) = track
        .album
        .as_ref()
        .and_then(|album| album.title.as_deref())
        .filter(|title| !title.trim().is_empty())
    {
        lines.push(format!("Album: {}", display_text(album)));
    }
    if let Some(upload) = track.uploads.first() {
        if let Some(size) = upload.size {
            let size = if size < 1024 {
                format!("{size} B")
            } else if size < 1024 * 1024 {
                format!("{:.2} KiB", size as f64 / 1024.0)
            } else if size < 1024 * 1024 * 1024 {
                format!("{:.2} MiB", size as f64 / (1024.0 * 1024.0))
            } else {
                format!("{:.2} GiB", size as f64 / (1024.0 * 1024.0 * 1024.0))
            };
            lines.push(format!("Size: {size}"));
        }
        let container = upload
            .extension
            .as_deref()
            .and_then(audio_fact)
            .or_else(|| {
                upload
                    .mimetype
                    .as_deref()
                    .and_then(|value| value.strip_prefix("audio/"))
                    .and_then(audio_fact)
            })
            .map(audio_format_label);
        let codec = upload
            .codec
            .as_deref()
            .and_then(audio_fact)
            .map(audio_format_label);
        match (container, codec) {
            (Some(container), Some(codec)) if container != codec => {
                lines.push(format!("Format: {container}, {codec}"));
            }
            (Some(container), _) => lines.push(format!("Format: {container}")),
            (None, Some(codec)) => lines.push(format!("Codec: {codec}")),
            (None, None) => {}
        }
        if let Some(bitrate) = upload.bitrate.filter(|value| *value > 0) {
            lines.push(format!(
                "Bitrate: {} kbit/s",
                bitrate.saturating_add(500) / 1000
            ));
        }
    }
    if let Some(listenings) = track.listenings_count {
        lines.push(format!("Listenings: {listenings}"));
    }
    let description = content_text(track.description.clone());
    if !description.is_empty() {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(description);
    }
    lines.join("\n")
}

/// Labels supplied identifiers without probing audio or inferring a missing codec.
fn audio_format_label(value: &str) -> String {
    match value.to_ascii_lowercase().as_str() {
        "ogg" | "oga" => "Ogg".to_owned(),
        "opus" => "Opus".to_owned(),
        "vorbis" => "Vorbis".to_owned(),
        "wav" | "wave" | "vnd.wave" | "x-wav" => "WAV".to_owned(),
        "flac" | "x-flac" => "FLAC".to_owned(),
        "mp3" => "MP3".to_owned(),
        _ => value.to_ascii_uppercase(),
    }
}

fn page_has_more(
    page: u32,
    returned: usize,
    total: u64,
    next: Option<&str>,
    page_size: u32,
) -> bool {
    if next.is_some_and(|url| !url.trim().is_empty()) {
        return true;
    }
    let start = u64::from(page.saturating_sub(1)) * u64::from(page_size);
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
    is_playable: Option<bool>,
    #[serde(default)]
    artist: Option<RawArtistReference>,
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
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    listenings_count: Option<u64>,
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
    #[serde(default, deserialize_with = "deserialize_u64")]
    id: u64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    artist: Option<RawArtistReference>,
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
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawUpload {
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    duration: Option<u64>,
    #[serde(default)]
    listen_url: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    size: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    bitrate: Option<u64>,
    #[serde(default)]
    mimetype: Option<String>,
    #[serde(default)]
    extension: Option<String>,
    #[serde(default)]
    codec: Option<String>,
}

/// Omits provider placeholders rather than presenting them as audio facts.
fn audio_fact(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()
        && !["none", "null", "unknown", "n/a"]
            .iter()
            .any(|placeholder| value.eq_ignore_ascii_case(placeholder)))
    .then_some(value)
}

#[derive(Debug, Deserialize)]
struct RawCover {
    #[serde(default)]
    urls: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize)]
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
        #[serde(default)]
        content_type: Option<String>,
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
    fn explicit_unplayable_tracks_are_excluded_even_when_the_pod_ignores_the_filter() {
        let raw = serde_json::from_value(serde_json::json!({
            "count": 2,
            "results": [
                {"id": 27158, "title": "Intro", "artist_credit": [{"name": "Joy in Resistance"}], "is_playable": false, "uploads": [], "listen_url": "/api/v2/listen/missing/"},
                {"id": 42, "title": "Available", "artist_credit": [{"name": "Artist"}], "is_playable": true, "listen_url": "/api/v2/listen/available/"}
            ]
        })).unwrap();
        let page = provider().convert_track_page(raw, 1).unwrap();
        assert_eq!(page.items.len(), 1);
        assert!(matches!(&page.items[0], SearchItem::Video(track) if track.title == "Available"));
    }

    #[test]
    fn description_declared_html_is_plain_text_with_visible_clickable_urls() {
        let content = serde_json::from_value(serde_json::json!({
            "text": "<p><a href=\"http://www.jamendo.com\">Jamendo</a> cc_standard</p><script>hidden()</script>",
            "content_type": "text/html",
            "html": "<p>fallback must not replace the original</p>"
        })).unwrap();
        assert_eq!(
            content_text(Some(content)),
            "Jamendo (http://www.jamendo.com) cc_standard"
        );
        let content = serde_json::from_value(serde_json::json!(
            "<p><a href=\"http://www.jamendo.com\">http://www.jamendo.com</a> cc_standard</p>"
        ))
        .unwrap();
        assert_eq!(
            content_text(Some(content)),
            "http://www.jamendo.com cc_standard"
        );
    }

    #[test]
    fn description_repairs_confirmed_cp1251_mojibake_without_corrupting_latin_text() {
        for (input, expected) in [
            ("Òîëüêî äëÿ îçíàêîìëåíèÿ", "Только для ознакомления"),
            ("Björk · déjà vu", "Björk · déjà vu"),
            ("Только для ознакомления", "Только для ознакомления"),
        ] {
            assert_eq!(
                content_text(Some(RawContent::Text(input.to_owned()))),
                expected
            );
        }
    }

    #[test]
    fn upload_details_expose_audio_facts_but_never_call_downloads_listenings() {
        let mut raw: RawTrack = serde_json::from_value(serde_json::json!({
            "id": 27084, "title": "This is love!", "artist_credit": [{"artist": {"id": 5794}, "credit": "Wadi Dubroffski"}],
            "downloads_count": 16,
            "uploads": [{"size": 27941291, "duration": 146, "bitrate": 1529248, "mimetype": "audio/flac", "extension": "flac"}]
        })).unwrap();
        let details = provider().convert_track_details(raw).unwrap();
        assert!(details.description.contains("Size: 26.65 MiB"));
        assert!(!details.description.contains("Advertised audio upload:"));
        assert!(details.description.contains("Format: FLAC"));
        assert!(!details.description.contains("Codec:"));
        assert!(details.description.contains("Bitrate: 1529 kbit/s"));
        assert_eq!(details.duration_seconds, Some(146));
        assert!(!details.description.contains("Listenings:"));
        raw = serde_json::from_value(serde_json::json!({"id": 1, "title": "Track", "artist_credit": [{"name": "Artist"}], "listenings_count": 7})).unwrap();
        assert!(
            provider()
                .convert_track_details(raw)
                .unwrap()
                .description
                .contains("Listenings: 7")
        );
    }

    /// Pod placeholders cannot mask a real MIME-derived audio container.
    #[test]
    fn unknown_extension_falls_back_to_the_real_audio_mime_format() {
        for extension in ["none", "NONE", " null ", "unknown", "n/a", ""] {
            let raw: RawTrack = serde_json::from_value(serde_json::json!({
                "id": 27378,
                "title": "Strata",
                "artist_credit": [{"name": "Sonoridad"}],
                "uploads": [{"extension": extension, "mimetype": "audio/vnd.wave", "codec": "none"}]
            }))
            .unwrap();
            let description = provider().convert_track_details(raw).unwrap().description;
            assert!(
                description.contains("Format: WAV"),
                "{extension}: {description}"
            );
            assert!(!description.contains("Format: NONE"));
            assert!(!description.contains("Codec: NONE"));
        }
        let raw: RawTrack = serde_json::from_value(serde_json::json!({
            "id": 1, "title": "Unknown audio", "artist_credit": [{"name": "Artist"}], "uploads": [{"extension": "none", "codec": "null"}]
        })).unwrap();
        let description = provider().convert_track_details(raw).unwrap().description;
        assert!(!description.contains("Format:"));
        assert!(!description.contains("Codec:"));
    }

    /// Containers such as Ogg do not imply Opus or Vorbis without advertised codec metadata.
    #[test]
    fn advertised_container_and_codec_share_one_human_readable_format_line() {
        for (extension, mimetype, codec, expected) in [
            (
                Some("ogg"),
                Some("audio/ogg"),
                Some("opus"),
                "Format: Ogg, Opus",
            ),
            (
                Some("oga"),
                Some("audio/ogg"),
                Some("vorbis"),
                "Format: Ogg, Vorbis",
            ),
            (Some("ogg"), Some("audio/ogg"), None, "Format: Ogg"),
            (
                Some("opus"),
                Some("audio/opus"),
                Some("Opus"),
                "Format: Opus",
            ),
            (
                Some("flac"),
                Some("audio/flac"),
                Some("FLAC"),
                "Format: FLAC",
            ),
            (Some("wave"), Some("audio/vnd.wave"), None, "Format: WAV"),
            (
                Some("none"),
                Some("audio/mpeg"),
                Some("none"),
                "Format: MPEG",
            ),
            (Some("mp3"), Some("audio/mpeg"), None, "Format: MP3"),
            (Some("unknown"), None, Some("opus"), "Codec: Opus"),
            (None, Some("audio/ogg"), Some("null"), "Format: Ogg"),
        ] {
            let raw: RawTrack = serde_json::from_value(serde_json::json!({
                "id": 27, "title": "Advertised audio", "artist_credit": [{"name": "Artist"}],
                "uploads": [{"extension": extension, "mimetype": mimetype, "codec": codec}]
            }))
            .unwrap();
            let description = provider().convert_track_details(raw).unwrap().description;
            let facts = description
                .lines()
                .filter(|line| line.starts_with("Format: ") || line.starts_with("Codec: "))
                .collect::<Vec<_>>();
            assert_eq!(
                facts,
                vec![expected],
                "{extension:?}/{mimetype:?}/{codec:?}"
            );
        }
    }

    #[test]
    fn recent_and_artist_catalogues_have_exact_filters_and_stable_order() {
        let provider = provider().with_page_size(10);
        let recent = provider.build_catalogue_url(None, 1, API_V2).unwrap();
        assert!(
            recent
                .query_pairs()
                .any(|(key, value)| key == "ordering" && value == "-creation_date")
        );
        assert!(
            recent
                .query_pairs()
                .any(|(key, value)| key == "page_size" && value == "10")
        );
        let artist = provider
            .build_catalogue_url(
                Some("https://funk.example.test/prefix/library/artists/5811/"),
                2,
                API_V2,
            )
            .unwrap();
        assert!(
            artist
                .query_pairs()
                .any(|(key, value)| key == "artist" && value == "5811")
        );
        assert!(
            artist
                .query_pairs()
                .any(|(key, value)| key == "ordering"
                    && value == "album__title,disc_number,position")
        );
        assert!(
            provider
                .build_catalogue_url(
                    Some("https://other.example/library/artists/5811"),
                    1,
                    API_V2
                )
                .is_err()
        );
        assert!(provider.build_catalogue_url(None, 0, API_V2).is_err());
    }

    #[test]
    fn tag_filters_preserve_the_exact_unicode_value_and_never_become_title_searches() {
        let tag = "Ambient & музыка / #field recording";
        let url = provider().build_tag_catalogue_url(tag, 2, API_V2).unwrap();
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "tag" && value == tag)
        );
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "q" && value.is_empty())
        );
        assert!(!url.query_pairs().any(|(key, _)| key == "artist"));
        assert!(provider().build_tag_catalogue_url("\n", 1, API_V2).is_err());
    }

    #[test]
    fn album_catalogue_uses_exact_local_identity_and_disc_track_order() {
        let provider = provider().with_page_size(20);
        let url = provider
            .build_album_catalogue_url(
                "https://funk.example.test/prefix/library/albums/42/",
                2,
                API_V2,
            )
            .unwrap();
        for (key, value) in [
            ("album", "42"),
            ("ordering", "disc_number,position"),
            ("page_size", "20"),
            ("page", "2"),
            ("playable", "true"),
        ] {
            assert!(
                url.query_pairs()
                    .any(|(actual_key, actual_value)| actual_key == key && actual_value == value)
            );
        }
        assert!(
            !url.query_pairs()
                .any(|(key, _)| matches!(key.as_ref(), "artist" | "tag"))
        );
        for album in [
            "https://other.example/prefix/library/albums/42/",
            "https://funk.example.test/prefix/library/albums/042/",
            "https://funk.example.test/prefix/library/albums/42/?token=bad",
        ] {
            assert!(
                provider
                    .build_album_catalogue_url(album, 1, API_V2)
                    .is_err()
            );
        }
    }

    #[test]
    fn tags_reject_unactionable_values_deduplicate_and_bound_inline_targets() {
        let original = " Ambient & музыка ";
        let mut tags = vec![
            original.to_owned(),
            original.to_owned(),
            String::new(),
            "\t".to_owned(),
            "x".repeat(1025),
        ];
        tags.extend((0..200).map(|index| format!("tag{index}")));
        let tags = normalized_tags(tags);
        assert_eq!(tags.len(), 128);
        assert_eq!(tags[0], original);
        assert_eq!(
            tags.iter().filter(|tag| tag.as_str() == original).count(),
            1
        );
        assert!(tags.iter().all(|tag| valid_tag(tag)));
    }

    #[test]
    fn selected_upload_metadata_uses_only_an_advertised_canonical_uuid_on_the_same_pod() {
        let provider = provider();
        let id = "17c770f0-1a57-40e9-85de-d2ca75b1d42d";
        assert_eq!(
            provider.build_upload_url(id, API_V2).unwrap().as_str(),
            "https://funk.example.test/prefix/api/v2/uploads/17c770f0-1a57-40e9-85de-d2ca75b1d42d/"
        );
        for invalid in [
            "",
            "../private",
            "https://other.example/id",
            "17c770f0-1a57-40e9-85de-d2ca75b1d42d?token=x",
        ] {
            assert!(provider.build_upload_url(invalid, API_V2).is_none());
        }
    }

    #[test]
    fn inaccessible_optional_upload_metadata_never_changes_track_playability() {
        let mut page: RawPage<RawTrack> = serde_json::from_str(TRACK_PAGE_FIXTURE).unwrap();
        let mut raw = page.results.remove(0);
        raw.uploads[0].uuid = Some("17c770f0-1a57-40e9-85de-d2ca75b1d42d".to_owned());
        let provider = provider();
        let calls = std::cell::Cell::new(0);
        let metadata = provider.selected_upload_metadata(&raw, API_V2, |_| {
            calls.set(calls.get() + 1);
            Err(ProviderError::HttpStatus(403))
        });
        assert!(metadata.is_none());
        assert_eq!(calls.get(), 1);
        assert!(
            provider
                .convert_track_details(raw)
                .unwrap()
                .stream_url
                .is_some()
        );
        let mut page: RawPage<RawTrack> = serde_json::from_str(TRACK_PAGE_FIXTURE).unwrap();
        let raw = page.results.remove(0);
        assert!(
            provider
                .selected_upload_metadata(&raw, API_V2, |_| {
                    panic!("no advertised upload UUID must not trigger another request")
                })
                .is_none()
        );
    }

    #[test]
    fn modern_artist_credit_reference_does_not_require_a_nested_name() {
        let raw: RawPage<RawTrack> = serde_json::from_str(r#"{"results":[{"id":42,"title":"Modern track","artist_credit":[{"artist":{"id":7,"fid":"https://pod.example/artists/uuid"},"credit":"Credited Artist"}],"uploads":[],"listen_url":"/api/v2/listen/12345678-1234-1234-1234-123456789abc/"}]}"#).expect("official modern artist reference shape");
        let page = provider()
            .convert_track_page(raw, 1)
            .expect("modern track conversion");
        let [SearchItem::Video(track)] = page.items.as_slice() else {
            panic!("one track");
        };
        assert_eq!(track.channel_name, "Credited Artist");
    }

    #[test]
    fn legacy_v1_tracks_accept_direct_and_album_artist_shapes() {
        for artist in [
            "\"artist\":{\"id\":7,\"name\":\"Legacy Artist\"}",
            "\"album\":{\"artist\":{\"id\":7,\"name\":\"Legacy Artist\"}}",
        ] {
            let raw: RawTrack = serde_json::from_str(&format!(
                "{{\"id\":42,\"title\":\"Legacy track\",{artist},\"uploads\":[]}}"
            ))
            .expect("official legacy artist shape");
            let track = provider()
                .convert_track_summary(raw)
                .expect("legacy track conversion");
            assert_eq!(track.channel_name, "Legacy Artist");
        }
    }

    #[test]
    fn equal_numeric_ids_on_different_pods_have_distinct_media_and_artist_identity() {
        let first = FunkwhaleProvider::new(Url::parse("https://one.example/").unwrap()).unwrap();
        let second = FunkwhaleProvider::new(Url::parse("https://two.example/").unwrap()).unwrap();
        let convert = |provider: &FunkwhaleProvider| {
            let raw: RawPage<RawTrack> = serde_json::from_str(TRACK_PAGE_FIXTURE).unwrap();
            let page = provider.convert_track_page(raw, 1).unwrap();
            let SearchItem::Video(track) = page.items.into_iter().next().unwrap() else {
                panic!("track");
            };
            track
        };
        let first = convert(&first);
        let second = convert(&second);
        assert_eq!(first.video_id, "https://one.example/library/tracks/42");
        assert_eq!(second.video_id, "https://two.example/library/tracks/42");
        assert_ne!(first.channel_id, second.channel_id);
    }

    #[test]
    fn scoped_track_ids_preserve_pod_prefix_and_reject_other_pod_owners() {
        let provider = provider();
        let url = provider
            .build_track_url("https://funk.example.test/prefix/library/tracks/42", API_V1)
            .expect("own-pod identity");
        assert_eq!(url.path(), "/prefix/api/v1/tracks/42/");
        assert!(
            provider
                .build_track_url("https://other.example/library/tracks/42", API_V1)
                .is_err()
        );
        assert!(
            provider
                .build_track_url(
                    "https://funk.example.test/wrong-prefix/library/tracks/42",
                    API_V1
                )
                .is_err()
        );
        assert!(
            provider
                .build_track_url(
                    "https://funk.example.test/prefix/library/tracks/42?token=secret",
                    API_V1
                )
                .is_err()
        );
        assert_eq!(provider.build_track_url("42", API_V1).unwrap(), url);
    }

    #[test]
    fn scoped_identity_reconstructs_original_pod_and_rejects_unowned_or_unsafe_ids() {
        let base = FunkwhaleProvider::instance_url_from_track_id(
            "https://POD.example:443/prefix/library/tracks/42",
        )
        .expect("scoped pod identity");
        assert_eq!(base.as_str(), "https://pod.example/prefix/");
        for identity in [
            "42",
            "https://pod.example/library/tracks/0",
            "https://pod.example/library/artists/42",
            "https://pod.example/library/tracks/42?token=private",
            "https://user:password@pod.example/library/tracks/42",
            "file:///library/tracks/42",
            "https://pod.example/library/tracks/18446744073709551616",
        ] {
            assert!(
                FunkwhaleProvider::instance_url_from_track_id(identity).is_err(),
                "{identity}"
            );
        }
    }

    #[test]
    fn page_capacity_controls_query_and_pagination_without_changing_default() {
        let request = SearchRequest::new("music", SearchTarget::Videos);
        let default = provider().build_search_url(&request, API_V1).unwrap();
        assert!(
            default
                .query_pairs()
                .any(|pair| pair == ("page_size".into(), "20".into()))
        );
        let provider = provider().with_page_size(3);
        let url = provider.build_search_url(&request, API_V1).unwrap();
        assert!(
            url.query_pairs()
                .any(|pair| pair == ("page_size".into(), "3".into()))
        );
        let raw = serde_json::from_str(r#"{"count":5,"results":[{"id":1,"title":"Final","artist_credit":[{"name":"Composer"}]}]}"#).unwrap();
        assert_eq!(
            provider.convert_track_page(raw, 2).unwrap().next_page,
            Some(3)
        );
    }

    #[test]
    fn advertised_upload_stream_is_preserved_and_unplayable_tracks_have_no_locator() {
        let mut raw: RawPage<RawTrack> = serde_json::from_str(TRACK_PAGE_FIXTURE).unwrap();
        let track = &mut raw.results[0];
        track.listen_url = None;
        track.uploads[0].listen_url = Some("/api/v2/listen/12345678-1234-1234-1234-123456789abc/?upload=abcdefab-abcd-abcd-abcd-abcdefabcdef&download=true".to_owned());
        assert_eq!(
            provider().track_stream_url(track).unwrap().unwrap().query(),
            Some("upload=abcdefab-abcd-abcd-abcd-abcdefabcdef&download=true")
        );
        track.is_playable = Some(false);
        assert_eq!(provider().track_stream_url(track).unwrap(), None);
        track.is_playable = None;
        track.uploads[0].listen_url = None;
        assert_eq!(provider().track_stream_url(track).unwrap(), None);
    }

    #[test]
    fn api_not_found_falls_back_once_but_authentication_never_tries_another_version() {
        let provider = provider();
        let calls = std::cell::RefCell::new(Vec::new());
        let selected = provider
            .with_detected_api(|version| {
                calls.borrow_mut().push(version);
                if version == API_V1 {
                    Err(ProviderError::HttpStatus(404))
                } else {
                    Ok(version)
                }
            })
            .unwrap();
        assert_eq!(selected, API_V2);
        provider
            .with_detected_api(|version| {
                calls.borrow_mut().push(version);
                Ok(())
            })
            .unwrap();
        assert_eq!(*calls.borrow(), [API_V1, API_V2, API_V2]);
        for status in [401, 403] {
            let provider =
                FunkwhaleProvider::new(Url::parse("https://auth.example/").unwrap()).unwrap();
            let attempts = std::cell::Cell::new(0);
            let response: Result<(), _> = provider.with_detected_api(|_| {
                attempts.set(attempts.get() + 1);
                Err(ProviderError::HttpStatus(status))
            });
            assert!(matches!(response, Err(ProviderError::HttpStatus(code)) if code == status));
            assert_eq!(attempts.get(), 1);
        }
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
    fn track_artwork_retains_album_fallbacks_without_replacing_the_track_cover() {
        let raw: RawTrack = serde_json::from_value(serde_json::json!({
            "id": 27378,
            "title": "Strata",
            "cover": {"urls": {
                "large_square_crop": "/media/track-large.jpg",
                "original": "/media/track-original.png"
            }},
            "album": {"cover": {"urls": {
                "large_square_crop": "/media/album-large.jpg",
                "original": "/media/album-original.png"
            }}}
        }))
        .expect("realistic track and album covers");

        let thumbnails = provider().track_thumbnails(&raw);
        assert_eq!(
            thumbnails
                .iter()
                .map(|image| image.url.path())
                .collect::<Vec<_>>(),
            vec![
                "/media/track-large.jpg",
                "/media/track-original.png",
                "/media/album-large.jpg",
                "/media/album-original.png",
            ]
        );
        assert_eq!(thumbnails[0].quality.as_deref(), Some("large_square_crop"));
        assert_eq!(
            thumbnails[2].quality.as_deref(),
            Some("album_large_square_crop")
        );
        assert_eq!(thumbnails[3].quality.as_deref(), Some("album_original"));
    }

    /// Shared track/album attachments do not multiply network attempts.
    #[test]
    fn track_artwork_deduplicates_shared_album_cover_urls() {
        let raw: RawTrack = serde_json::from_value(serde_json::json!({
            "id": 1, "title": "Track",
            "cover": {"urls": {"large_square_crop": "/media/shared.jpg"}},
            "album": {"cover": {"urls": {"large_square_crop": "/media/shared.jpg"}}}
        }))
        .unwrap();
        let thumbnails = provider().track_thumbnails(&raw);
        assert_eq!(thumbnails.len(), 1);
        assert_eq!(thumbnails[0].quality.as_deref(), Some("large_square_crop"));
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
        assert_eq!(
            track.channel_id,
            "https://funk.example.test/prefix/library/artists/7"
        );
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
