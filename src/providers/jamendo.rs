//! Jamendo music discovery through the official v3 tracks API.
//!
//! The API requires an application client ID issued by Jamendo. Youta never
//! bundles Jamendo's public documentation/testing ID, and this adapter does not
//! use OAuth or any undocumented website endpoint. Requests are blocking,
//! time-bounded, and response-size-bounded, so callers must run them on a
//! provider worker.
//!
//! Jamendo returns `license_ccurl` as the exact Creative Commons licence
//! identifier attached to a track. Some catalogue records still use an
//! `http://creativecommons.org` identifier, so it is retained as metadata
//! rather than rewritten. Actionable artwork, page, stream, and download URLs
//! are accepted only when they are credential-free HTTPS URLs.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use url::Url;

use super::{
    DEFAULT_MAX_JSON_BYTES, DEFAULT_REQUEST_TIMEOUT, Provider, ProviderCapabilities, ProviderError,
    SearchDate, SearchDuration, SearchFeature, SearchItem, SearchPage, SearchRequest, SearchSort,
    SearchTarget, Thumbnail, VideoDetails, VideoSummary, get_bounded_json, provider_agent,
};

const API_ENDPOINT: &str = "https://api.jamendo.com/v3.0/tracks/";
const RESULTS_PER_PAGE: u32 = 50;
const MAX_PAGE: u32 = 10_000;
const MAX_CONFIGURED_JSON_BYTES: usize = 64 * 1024 * 1024;
const MAX_CLIENT_ID_BYTES: usize = 128;

/// A page of Jamendo tracks with provider-specific music metadata intact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct JamendoTrackPage {
    /// One-based page number requested from Jamendo.
    pub page: u32,
    /// Normalized tracks returned by the official API.
    pub tracks: Vec<JamendoTrack>,
    /// Next page to request, or `None` after a short/empty page.
    pub next_page: Option<u32>,
}

/// Track metadata returned by Jamendo's official v3 tracks API.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct JamendoTrack {
    /// Positive decimal track identifier.
    pub track_id: String,
    /// Track title.
    pub title: String,
    /// Positive decimal artist identifier.
    pub artist_id: String,
    /// Artist display name.
    pub artist_name: String,
    /// Album identifier, or `None` for a single.
    pub album_id: Option<String>,
    /// Album name, or `None` for a single.
    pub album_name: Option<String>,
    /// Duration in seconds.
    pub duration_seconds: u64,
    /// Release date in Jamendo's `YYYY-MM-DD` representation, when present.
    pub release_date: Option<String>,
    /// Exact `license_ccurl` value returned by Jamendo.
    ///
    /// This is a licence identifier, not a URL that Youta fetches. Legacy
    /// catalogue entries may retain an HTTP Creative Commons identifier.
    /// Consumers must inspect the licence terms themselves: NC and ND licences
    /// do not automatically qualify for Wikimedia Commons.
    pub license_ccurl: String,
    /// Track artwork advertised by Jamendo, when present.
    pub artwork_url: Option<Url>,
    /// Canonical Jamendo track page.
    pub share_url: Url,
    /// Jamendo's compact track link, when present.
    pub short_url: Option<Url>,
    /// Direct HTTPS playback URL returned by the API.
    pub audio_stream_url: Url,
    /// Upstream `audiodownload_allowed` decision for this track.
    pub audiodownload_allowed: bool,
    /// Direct HTTPS download URL, only when `audiodownload_allowed` is true.
    pub download_url: Option<Url>,
    /// Genre, instrument, and descriptive tags returned by `musicinfo`.
    pub tags: Vec<String>,
}

impl JamendoTrack {
    fn into_video_summary(self) -> VideoSummary {
        let description = self
            .album_name
            .as_ref()
            .map_or_else(String::new, |album| format!("Album: {album}"));
        let published_at = self
            .release_date
            .as_deref()
            .and_then(parse_release_date_epoch);
        let thumbnails = self.artwork_url.map_or_else(Vec::new, |url| {
            vec![Thumbnail {
                url,
                quality: Some("300".to_owned()),
                width: Some(300),
                height: Some(300),
            }]
        });

        VideoSummary {
            video_id: self.track_id,
            title: self.title,
            channel_name: self.artist_name,
            channel_id: self.artist_id,
            description,
            duration_seconds: Some(self.duration_seconds),
            // Jamendo can sort by total listens but does not return that count
            // unless the heavier optional stats expansion is requested.
            view_count: None,
            published_at,
            published_text: self.release_date,
            live: false,
            thumbnails,
            webpage_url: Some(self.share_url),
            stream_url: Some(self.audio_stream_url),
        }
    }

    fn into_video_details(self) -> VideoDetails {
        let description = self
            .album_name
            .as_ref()
            .map_or_else(String::new, |album| format!("Album: {album}"));
        let published_at = self
            .release_date
            .as_deref()
            .and_then(parse_release_date_epoch);
        let thumbnails = self.artwork_url.map_or_else(Vec::new, |url| {
            vec![Thumbnail {
                url,
                quality: Some("300".to_owned()),
                width: Some(300),
                height: Some(300),
            }]
        });

        VideoDetails {
            video_id: self.track_id,
            title: self.title,
            channel_name: self.artist_name,
            channel_id: self.artist_id,
            description,
            duration_seconds: Some(self.duration_seconds),
            view_count: None,
            like_count: None,
            published_at,
            published_text: self.release_date,
            license: Some(self.license_ccurl),
            rating: None,
            ratings_allowed: None,
            live: false,
            keywords: self.tags,
            thumbnails,
            webpage_url: Some(self.share_url),
            stream_url: Some(self.audio_stream_url),
        }
    }
}

/// Blocking client for Jamendo's official v3 tracks API.
#[derive(Clone)]
pub struct JamendoProvider {
    client_id: String,
    agent: ureq::Agent,
    max_json_bytes: usize,
}

impl fmt::Debug for JamendoProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JamendoProvider")
            .field("client_id", &"[redacted]")
            .field("max_json_bytes", &self.max_json_bytes)
            .finish_non_exhaustive()
    }
}

impl JamendoProvider {
    /// Creates a provider with conservative timeout and response limits.
    ///
    /// `client_id` must be a client ID issued for the user's own application
    /// by Jamendo's developer portal.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] when the client ID is empty,
    /// too long, or contains characters outside the accepted URL-safe set.
    pub fn new(client_id: impl Into<String>) -> Result<Self, ProviderError> {
        Self::with_options(client_id, DEFAULT_REQUEST_TIMEOUT, DEFAULT_MAX_JSON_BYTES)
    }

    /// Creates a provider with explicit blocking-call and response-size bounds.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] when the client ID, timeout,
    /// or JSON response bound is invalid.
    pub fn with_options(
        client_id: impl Into<String>,
        timeout: Duration,
        max_json_bytes: usize,
    ) -> Result<Self, ProviderError> {
        let client_id = client_id.into();
        validate_client_id(&client_id)?;
        if timeout.is_zero() {
            return Err(ProviderError::InvalidRequest(
                "Jamendo timeout must be greater than zero".to_owned(),
            ));
        }
        if !(1..=MAX_CONFIGURED_JSON_BYTES).contains(&max_json_bytes) {
            return Err(ProviderError::InvalidRequest(format!(
                "JSON response limit must be between 1 and {MAX_CONFIGURED_JSON_BYTES} bytes"
            )));
        }

        Ok(Self {
            client_id,
            agent: provider_agent(timeout),
            max_json_bytes,
        })
    }

    /// Searches Jamendo tracks while retaining album, licence, and download
    /// metadata that the provider-neutral search row cannot represent.
    ///
    /// Pagination is bounded to 50 results per request and 10,000 pages.
    /// Relevance and total-listen ordering are supported. Duration and release
    /// date filters map to the official `durationbetween` and `datebetween`
    /// parameters; unsupported video-specific filters are rejected.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] for invalid search input, an unsupported
    /// filter, transport failure, unsuccessful API status, oversized JSON, or
    /// malformed/untrusted result metadata.
    pub fn search_tracks(
        &self,
        request: &SearchRequest,
    ) -> Result<JamendoTrackPage, ProviderError> {
        let url = self.build_search_url(request)?;
        let envelope: RawEnvelope = get_bounded_json(&self.agent, &url, self.max_json_bytes)?;
        normalize_page(envelope, request.page, RESULTS_PER_PAGE)
    }

    /// Loads one track by its positive decimal Jamendo ID.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] for an invalid ID, transport failure,
    /// unsuccessful API status, oversized JSON, a missing track, or malformed
    /// and unsafe remote metadata.
    pub fn track(&self, track_id: &str) -> Result<JamendoTrack, ProviderError> {
        let url = self.build_track_url(track_id)?;
        let envelope: RawEnvelope = get_bounded_json(&self.agent, &url, self.max_json_bytes)?;
        normalize_track_lookup(envelope, track_id)
    }

    fn build_search_url(&self, request: &SearchRequest) -> Result<Url, ProviderError> {
        validate_search(request)?;
        let offset = request
            .page
            .checked_sub(1)
            .and_then(|page| page.checked_mul(RESULTS_PER_PAGE))
            .ok_or_else(|| {
                ProviderError::InvalidRequest("Jamendo search offset is too large".to_owned())
            })?;
        let mut url = self.api_url();
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("search", request.query.trim());
            query.append_pair("offset", &offset.to_string());
            query.append_pair("limit", &RESULTS_PER_PAGE.to_string());
            query.append_pair(
                "order",
                match request.sort {
                    SearchSort::Relevance => "relevance",
                    // Total listens are the closest music-catalogue analogue
                    // to the provider-neutral view-count order.
                    SearchSort::Views => "listens_total_desc",
                    SearchSort::UploadDate => return Err(ProviderError::Unsupported),
                },
            );
            if let Some(duration) = request.filters.duration {
                query.append_pair("durationbetween", duration_range(duration));
            }
            if let Some(date) = request.filters.date {
                let range = release_date_range(date)?;
                query.append_pair("datebetween", &range);
            }
        }
        Ok(url)
    }

    fn build_track_url(&self, track_id: &str) -> Result<Url, ProviderError> {
        validate_track_id(track_id)?;
        let mut url = self.api_url();
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("id", track_id);
            query.append_pair("limit", "1");
        }
        Ok(url)
    }

    fn api_url(&self) -> Url {
        let mut url = api_endpoint();
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("client_id", &self.client_id);
            query.append_pair("format", "json");
            // Singles otherwise disappear because Jamendo defaults to album
            // tracks.
            query.append_pair("type", "single albumtrack");
            query.append_pair("include", "musicinfo");
            query.append_pair("imagesize", "300");
            query.append_pair("audioformat", "mp32");
            query.append_pair("audiodlformat", "mp32");
        }
        url
    }
}

impl Provider for JamendoProvider {
    fn id(&self) -> &'static str {
        "jamendo"
    }

    fn display_name(&self) -> &'static str {
        "Jamendo"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            video_search: true,
            channel_search: false,
            pagination: true,
            search_filters: true,
            search_sorting: true,
            video_details: true,
            thumbnails: true,
        }
    }

    fn search(&self, request: &SearchRequest) -> Result<SearchPage, ProviderError> {
        let page = self.search_tracks(request)?;
        Ok(SearchPage {
            page: page.page,
            items: page
                .tracks
                .into_iter()
                .map(JamendoTrack::into_video_summary)
                .map(SearchItem::Video)
                .collect(),
            next_page: page.next_page,
        })
    }

    fn video_details(&self, video_id: &str) -> Result<VideoDetails, ProviderError> {
        self.track(video_id).map(JamendoTrack::into_video_details)
    }
}

fn api_endpoint() -> Url {
    Url::parse(API_ENDPOINT).expect("the compile-time Jamendo v3 endpoint must be valid")
}

fn validate_client_id(client_id: &str) -> Result<(), ProviderError> {
    if client_id.trim() != client_id
        || client_id.is_empty()
        || client_id.len() > MAX_CLIENT_ID_BYTES
        || !client_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(ProviderError::InvalidRequest(
            "Jamendo client ID must contain 1 to 128 URL-safe ASCII characters".to_owned(),
        ));
    }
    Ok(())
}

fn validate_track_id(track_id: &str) -> Result<(), ProviderError> {
    if track_id.is_empty()
        || track_id.len() > 20
        || !track_id.bytes().all(|byte| byte.is_ascii_digit())
        || track_id.bytes().all(|byte| byte == b'0')
    {
        return Err(ProviderError::InvalidRequest(
            "Jamendo track ID must be a positive decimal integer".to_owned(),
        ));
    }
    Ok(())
}

fn validate_search(request: &SearchRequest) -> Result<(), ProviderError> {
    request.validate()?;
    if request.page > MAX_PAGE {
        return Err(ProviderError::InvalidRequest(format!(
            "Jamendo search page cannot exceed {MAX_PAGE}"
        )));
    }
    if request.target != SearchTarget::Videos {
        return Err(ProviderError::Unsupported);
    }
    if request.filters.region.is_some() {
        return Err(ProviderError::InvalidRequest(
            "Jamendo track search does not support region filtering".to_owned(),
        ));
    }
    if request
        .filters
        .features
        .iter()
        .any(|feature| *feature != SearchFeature::CreativeCommons)
    {
        return Err(ProviderError::InvalidRequest(
            "Jamendo track search supports only the Creative Commons feature filter".to_owned(),
        ));
    }
    Ok(())
}

const fn duration_range(duration: SearchDuration) -> &'static str {
    match duration {
        SearchDuration::Short => "0_239",
        SearchDuration::Medium => "240_1200",
        SearchDuration::Long => "1201_4294967295",
    }
}

fn release_date_range(date: SearchDate) -> Result<String, ProviderError> {
    let days_ago = match date {
        SearchDate::Hour => {
            return Err(ProviderError::InvalidRequest(
                "Jamendo exposes release dates by day and cannot filter the last hour".to_owned(),
            ));
        }
        SearchDate::Today => 0,
        SearchDate::Week => 6,
        SearchDate::Month => 30,
        SearchDate::Year => 365,
    };
    let today_days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| ProviderError::Transport(error.to_string()))?
        .as_secs()
        / 86_400;
    let start_days = today_days.saturating_sub(days_ago);
    let start = format_epoch_day(start_days)?;
    let end = format_epoch_day(today_days)?;
    Ok(format!("{start}_{end}"))
}

fn format_epoch_day(epoch_day: u64) -> Result<String, ProviderError> {
    let epoch_day = i64::try_from(epoch_day).map_err(|_| {
        ProviderError::InvalidRequest("release-date range is outside supported years".to_owned())
    })?;
    let (year, month, day) = civil_from_days(epoch_day);
    if !(0..=9999).contains(&year) {
        return Err(ProviderError::InvalidRequest(
            "release-date range is outside supported years".to_owned(),
        ));
    }
    Ok(format!("{year:04}-{month:02}-{day:02}"))
}

fn normalize_page(
    envelope: RawEnvelope,
    page: u32,
    page_size: u32,
) -> Result<JamendoTrackPage, ProviderError> {
    validate_envelope(&envelope)?;
    let returned = u32::try_from(envelope.results.len()).unwrap_or(u32::MAX);
    let tracks = envelope
        .results
        .into_iter()
        .map(normalize_track)
        .collect::<Result<Vec<_>, _>>()?;
    let next_page = (returned == page_size && page < MAX_PAGE).then_some(page + 1);
    Ok(JamendoTrackPage {
        page,
        tracks,
        next_page,
    })
}

fn normalize_track_lookup(
    envelope: RawEnvelope,
    expected_track_id: &str,
) -> Result<JamendoTrack, ProviderError> {
    validate_envelope(&envelope)?;
    if envelope.results.len() != 1 {
        return Err(ProviderError::InvalidResponse(
            "Jamendo track lookup must return exactly one result".to_owned(),
        ));
    }
    let raw = envelope
        .results
        .into_iter()
        .next()
        .expect("length was checked above");
    if raw.id != expected_track_id {
        return Err(ProviderError::InvalidResponse(
            "Jamendo returned a different track ID than requested".to_owned(),
        ));
    }
    normalize_track(raw)
}

fn validate_envelope(envelope: &RawEnvelope) -> Result<(), ProviderError> {
    if envelope.headers.status != "success" || envelope.headers.code != 0 {
        let message = envelope.headers.error_message.trim();
        return Err(ProviderError::InvalidResponse(if message.is_empty() {
            format!(
                "Jamendo API reported status {:?} with code {}",
                envelope.headers.status, envelope.headers.code
            )
        } else {
            format!("Jamendo API error {}: {message}", envelope.headers.code)
        }));
    }
    if envelope.headers.results_count != envelope.results.len() {
        return Err(ProviderError::InvalidResponse(
            "Jamendo result count does not match the response header".to_owned(),
        ));
    }
    Ok(())
}

fn normalize_track(raw: RawTrack) -> Result<JamendoTrack, ProviderError> {
    validate_response_id(&raw.id, "track")?;
    validate_response_id(&raw.artist_id, "artist")?;
    if raw.name.trim().is_empty() || raw.artist_name.trim().is_empty() {
        return Err(ProviderError::InvalidResponse(
            "Jamendo track requires a title and artist name".to_owned(),
        ));
    }

    let (album_id, album_name) = normalize_album(raw.album_id, raw.album_name)?;
    let release_date = normalize_release_date(raw.releasedate)?;
    let artwork_url = nonempty(raw.image.as_deref())
        .or_else(|| nonempty(raw.album_image.as_deref()))
        .map(|value| parse_https_url(value, "artwork"))
        .transpose()?;
    let share_url = parse_https_url(&raw.shareurl, "track page")?;
    let short_url = nonempty(raw.shorturl.as_deref())
        .map(|value| parse_https_url(value, "short track page"))
        .transpose()?;
    let audio_stream_url = parse_https_url(&raw.audio, "audio stream")?;
    let download_url = if raw.audiodownload_allowed {
        nonempty(raw.audiodownload.as_deref())
            .map(|value| parse_https_url(value, "audio download"))
            .transpose()?
    } else {
        // The permission flag is authoritative even if an inconsistent or
        // stale API response happens to include a URL.
        None
    };
    let license_ccurl = validate_license_identifier(&raw.license_ccurl)?;
    let tags = normalize_tags(raw.musicinfo);

    Ok(JamendoTrack {
        track_id: raw.id,
        title: raw.name,
        artist_id: raw.artist_id,
        artist_name: raw.artist_name,
        album_id,
        album_name,
        duration_seconds: raw.duration,
        release_date,
        license_ccurl,
        artwork_url,
        share_url,
        short_url,
        audio_stream_url,
        audiodownload_allowed: raw.audiodownload_allowed,
        download_url,
        tags,
    })
}

fn validate_response_id(value: &str, kind: &str) -> Result<(), ProviderError> {
    if value.is_empty()
        || value.len() > 20
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || value.bytes().all(|byte| byte == b'0')
    {
        return Err(ProviderError::InvalidResponse(format!(
            "Jamendo {kind} ID must be a positive decimal integer"
        )));
    }
    Ok(())
}

fn normalize_album(
    raw_id: Option<String>,
    raw_name: Option<String>,
) -> Result<(Option<String>, Option<String>), ProviderError> {
    let id = raw_id.and_then(|value| nonempty(Some(&value)).map(ToOwned::to_owned));
    let name = raw_name.and_then(|value| nonempty(Some(&value)).map(ToOwned::to_owned));
    match (id, name) {
        (None, None) => Ok((None, None)),
        (Some(id), Some(name)) => {
            validate_response_id(&id, "album")?;
            Ok((Some(id), Some(name)))
        }
        _ => Err(ProviderError::InvalidResponse(
            "Jamendo album ID and name must both be present or both be empty".to_owned(),
        )),
    }
}

fn normalize_release_date(value: Option<String>) -> Result<Option<String>, ProviderError> {
    let Some(value) = value.and_then(|value| nonempty(Some(&value)).map(ToOwned::to_owned)) else {
        return Ok(None);
    };
    if parse_release_date_epoch(&value).is_none() {
        return Err(ProviderError::InvalidResponse(
            "Jamendo release date must use a valid YYYY-MM-DD date".to_owned(),
        ));
    }
    Ok(Some(value))
}

fn normalize_tags(musicinfo: Option<RawMusicInfo>) -> Vec<String> {
    let Some(musicinfo) = musicinfo else {
        return Vec::new();
    };
    let mut tags = Vec::new();
    for value in musicinfo
        .tags
        .genres
        .into_iter()
        .chain(musicinfo.tags.instruments)
        .chain(musicinfo.tags.vartags)
    {
        let value = value.trim();
        if !value.is_empty() && !tags.iter().any(|tag| tag == value) {
            tags.push(value.to_owned());
        }
    }
    tags
}

fn parse_https_url(raw: &str, field: &str) -> Result<Url, ProviderError> {
    let url = Url::parse(raw).map_err(|error| {
        ProviderError::InvalidResponse(format!("invalid Jamendo {field} URL: {error}"))
    })?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(ProviderError::InvalidResponse(format!(
            "Jamendo {field} URL must be credential-free HTTPS without a fragment"
        )));
    }
    Ok(url)
}

fn validate_license_identifier(raw: &str) -> Result<String, ProviderError> {
    if raw.trim() != raw || raw.is_empty() || raw.len() > 2048 {
        return Err(ProviderError::InvalidResponse(
            "Jamendo license_ccurl is empty, padded, or too long".to_owned(),
        ));
    }
    let url = Url::parse(raw).map_err(|error| {
        ProviderError::InvalidResponse(format!("invalid Jamendo license_ccurl: {error}"))
    })?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ProviderError::InvalidResponse(
            "Jamendo license_ccurl must be a credential-free HTTP(S) identifier".to_owned(),
        ));
    }
    Ok(raw.to_owned())
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn parse_release_date_epoch(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes.get(4) != Some(&b'-') || bytes.get(7) != Some(&b'-') {
        return None;
    }
    let year = i64::from(parse_decimal(bytes.get(0..4)?)?);
    let month = parse_decimal(bytes.get(5..7)?)?;
    let day = parse_decimal(bytes.get(8..10)?)?;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    days_from_civil(year, month, day).checked_mul(86_400)
}

fn parse_decimal(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0_u32, |value, byte| {
        byte.is_ascii_digit()
            .then(|| value.checked_mul(10)?.checked_add(u32::from(byte - b'0')))
            .flatten()
    })
}

const fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.rem_euclid(4) == 0
            && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0) =>
        {
            29
        }
        2 => 28,
        _ => 0,
    }
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(epoch_day: i64) -> (i64, i64, i64) {
    let zero_day = epoch_day + 719_468;
    let era = zero_day.div_euclid(146_097);
    let day_of_era = zero_day - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_piece = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_piece + 2) / 5 + 1;
    let month = month_piece + if month_piece < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[derive(Clone, Debug, Deserialize)]
struct RawEnvelope {
    headers: RawHeaders,
    results: Vec<RawTrack>,
}

#[derive(Clone, Debug, Deserialize)]
struct RawHeaders {
    status: String,
    code: u32,
    #[serde(default)]
    error_message: String,
    results_count: usize,
}

#[derive(Clone, Debug, Deserialize)]
struct RawTrack {
    id: String,
    name: String,
    #[serde(default, deserialize_with = "deserialize_u64")]
    duration: u64,
    artist_id: String,
    artist_name: String,
    #[serde(default)]
    album_name: Option<String>,
    #[serde(default)]
    album_id: Option<String>,
    license_ccurl: String,
    #[serde(default)]
    releasedate: Option<String>,
    #[serde(default)]
    album_image: Option<String>,
    audio: String,
    #[serde(default)]
    audiodownload: Option<String>,
    #[serde(default)]
    shorturl: Option<String>,
    shareurl: String,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    musicinfo: Option<RawMusicInfo>,
    #[serde(default)]
    audiodownload_allowed: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawMusicInfo {
    #[serde(default)]
    tags: RawTags,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawTags {
    #[serde(default)]
    genres: Vec<String>,
    #[serde(default)]
    instruments: Vec<String>,
    #[serde(default)]
    vartags: Vec<String>,
}

fn deserialize_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Number {
        Integer(u64),
        Text(String),
    }

    match Number::deserialize(deserializer)? {
        Number::Integer(value) => Ok(value),
        Number::Text(value) => value.parse().map_err(serde::de::Error::custom),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACKS_FIXTURE: &str = r#"{
        "headers": {
            "status": "success",
            "code": 0,
            "error_message": "",
            "warnings": "",
            "results_count": 2
        },
        "results": [
            {
                "id": "1848357",
                "name": "Open Track",
                "duration": 272,
                "artist_id": "421168",
                "artist_name": "Fixture Artist",
                "album_name": "Fixture Album",
                "album_id": "368084",
                "license_ccurl": "http://creativecommons.org/licenses/by-nc-nd/3.0/",
                "releasedate": "2021-04-11",
                "album_image": "https://usercontent.jamendo.com/album.jpg",
                "audio": "https://prod-1.storage.jamendo.com/?trackid=1848357&format=mp32",
                "audiodownload": "https://prod-1.storage.jamendo.com/download/track/1848357/mp32/",
                "shorturl": "https://jamen.do/t/1848357",
                "shareurl": "https://www.jamendo.com/track/1848357",
                "image": "https://usercontent.jamendo.com/track.jpg",
                "musicinfo": {
                    "tags": {
                        "genres": ["rock"],
                        "instruments": ["guitar"],
                        "vartags": ["rock", "hopeful"]
                    }
                },
                "audiodownload_allowed": true
            },
            {
                "id": "1880336",
                "name": "Download Disabled",
                "duration": "147",
                "artist_id": "540929",
                "artist_name": "Second Artist",
                "album_name": "",
                "album_id": "",
                "license_ccurl": "https://creativecommons.org/licenses/by/4.0/",
                "releasedate": "2021-08-24",
                "album_image": "",
                "audio": "https://prod-1.storage.jamendo.com/?trackid=1880336&format=mp32",
                "audiodownload": "https://attacker.example/should-not-be-exposed",
                "shorturl": "",
                "shareurl": "https://www.jamendo.com/track/1880336",
                "image": "https://usercontent.jamendo.com/single.jpg",
                "audiodownload_allowed": false
            }
        ]
    }"#;

    fn provider() -> JamendoProvider {
        JamendoProvider::new("fixture_client_id").expect("fixture client ID should be accepted")
    }

    #[test]
    fn search_url_uses_only_the_official_v3_endpoint_and_bounded_pagination() {
        let mut request = SearchRequest::new("ambient guitar", SearchTarget::Videos);
        request.page = 3;
        request.sort = SearchSort::Views;
        request.filters.duration = Some(SearchDuration::Medium);
        request
            .filters
            .features
            .push(SearchFeature::CreativeCommons);

        let url = provider()
            .build_search_url(&request)
            .expect("search URL should build");
        let pairs = url.query_pairs().collect::<Vec<_>>();

        assert_eq!(url.as_str().split('?').next(), Some(API_ENDPOINT));
        assert!(
            pairs
                .iter()
                .any(|pair| pair == &("offset".into(), "100".into()))
        );
        assert!(
            pairs
                .iter()
                .any(|pair| pair == &("limit".into(), "50".into()))
        );
        assert!(
            pairs
                .iter()
                .any(|pair| pair == &("order".into(), "listens_total_desc".into()))
        );
        assert!(
            pairs
                .iter()
                .any(|pair| pair == &("durationbetween".into(), "240_1200".into()))
        );
        assert!(
            pairs
                .iter()
                .any(|pair| pair == &("type".into(), "single albumtrack".into()))
        );
        assert!(!pairs.iter().any(|pair| pair.0 == "access_token"));
    }

    #[test]
    fn fixture_preserves_music_metadata_and_enforces_download_permission() {
        let envelope: RawEnvelope =
            serde_json::from_str(TRACKS_FIXTURE).expect("fixture should deserialize");
        let page = normalize_page(envelope, 1, 2).expect("fixture should normalize");

        assert_eq!(page.next_page, Some(2));
        let allowed = &page.tracks[0];
        assert_eq!(allowed.album_id.as_deref(), Some("368084"));
        assert_eq!(allowed.album_name.as_deref(), Some("Fixture Album"));
        assert_eq!(allowed.duration_seconds, 272);
        assert_eq!(allowed.release_date.as_deref(), Some("2021-04-11"));
        assert_eq!(
            allowed.license_ccurl,
            "http://creativecommons.org/licenses/by-nc-nd/3.0/"
        );
        assert_eq!(allowed.tags, ["rock", "guitar", "hopeful"]);
        assert!(allowed.audiodownload_allowed);
        assert_eq!(
            allowed
                .download_url
                .as_ref()
                .expect("allowed fixture has download URL")
                .scheme(),
            "https"
        );

        let denied = &page.tracks[1];
        assert!(denied.album_id.is_none());
        assert!(denied.album_name.is_none());
        assert!(!denied.audiodownload_allowed);
        assert!(denied.download_url.is_none());
    }

    #[test]
    fn provider_neutral_details_keep_exact_license_stream_and_release_date() {
        let envelope: RawEnvelope =
            serde_json::from_str(TRACKS_FIXTURE).expect("fixture should deserialize");
        let track = normalize_track(envelope.results[0].clone())
            .expect("fixture track should normalize")
            .into_video_details();

        assert_eq!(
            track.license.as_deref(),
            Some("http://creativecommons.org/licenses/by-nc-nd/3.0/")
        );
        assert_eq!(track.published_text.as_deref(), Some("2021-04-11"));
        assert_eq!(track.published_at, Some(1_618_099_200));
        assert_eq!(
            track
                .stream_url
                .as_ref()
                .expect("fixture stream should be exposed")
                .scheme(),
            "https"
        );
        assert_eq!(track.thumbnails[0].width, Some(300));
    }

    #[test]
    fn direct_lookup_requires_one_matching_track() {
        let envelope: RawEnvelope =
            serde_json::from_str(TRACKS_FIXTURE).expect("fixture should deserialize");
        assert!(matches!(
            normalize_track_lookup(envelope.clone(), "1848357"),
            Err(ProviderError::InvalidResponse(_))
        ));

        let mut single = envelope;
        single.headers.results_count = 1;
        single.results.truncate(1);
        assert!(matches!(
            normalize_track_lookup(single.clone(), "999"),
            Err(ProviderError::InvalidResponse(_))
        ));
        let track =
            normalize_track_lookup(single, "1848357").expect("matching direct lookup should work");
        assert_eq!(track.track_id, "1848357");
    }

    #[test]
    fn actionable_urls_must_be_credential_free_https() {
        let envelope: RawEnvelope =
            serde_json::from_str(TRACKS_FIXTURE).expect("fixture should deserialize");
        for unsafe_url in [
            "http://media.example/track.mp3",
            "https://user:secret@media.example/track.mp3",
            "file:///tmp/track.mp3",
            "https://media.example/track.mp3#fragment",
        ] {
            let mut raw = envelope.results[0].clone();
            raw.audio = unsafe_url.to_owned();
            assert!(matches!(
                normalize_track(raw),
                Err(ProviderError::InvalidResponse(_))
            ));
        }
    }

    #[test]
    fn invalid_credentials_limits_and_video_filters_are_rejected() {
        assert!(JamendoProvider::new("").is_err());
        assert!(JamendoProvider::new(" client ").is_err());
        assert!(
            JamendoProvider::with_options("client", Duration::ZERO, DEFAULT_MAX_JSON_BYTES)
                .is_err()
        );
        assert!(
            JamendoProvider::with_options(
                "client",
                DEFAULT_REQUEST_TIMEOUT,
                MAX_CONFIGURED_JSON_BYTES + 1
            )
            .is_err()
        );

        let channels = SearchRequest::new("artist", SearchTarget::Channels);
        assert!(matches!(
            provider().build_search_url(&channels),
            Err(ProviderError::Unsupported)
        ));
        let mut region = SearchRequest::new("track", SearchTarget::Videos);
        region.filters.region = Some("GB".to_owned());
        assert!(provider().build_search_url(&region).is_err());
        let mut feature = SearchRequest::new("track", SearchTarget::Videos);
        feature.filters.features.push(SearchFeature::Hd);
        assert!(provider().build_search_url(&feature).is_err());
        let mut hourly = SearchRequest::new("track", SearchTarget::Videos);
        hourly.filters.date = Some(SearchDate::Hour);
        assert!(provider().build_search_url(&hourly).is_err());
    }

    #[test]
    fn api_error_headers_and_count_mismatches_are_rejected() {
        let mut envelope: RawEnvelope =
            serde_json::from_str(TRACKS_FIXTURE).expect("fixture should deserialize");
        envelope.headers.status = "failed".to_owned();
        envelope.headers.code = 7;
        envelope.headers.error_message = "bad client id".to_owned();
        assert!(matches!(
            normalize_page(envelope, 1, 2),
            Err(ProviderError::InvalidResponse(_))
        ));

        let mut envelope: RawEnvelope =
            serde_json::from_str(TRACKS_FIXTURE).expect("fixture should deserialize");
        envelope.headers.results_count = 99;
        assert!(matches!(
            normalize_page(envelope, 1, 2),
            Err(ProviderError::InvalidResponse(_))
        ));
    }

    #[test]
    fn date_conversion_handles_epoch_and_leap_days() {
        assert_eq!(format_epoch_day(0).expect("epoch day"), "1970-01-01");
        assert_eq!(parse_release_date_epoch("2024-02-29"), Some(1_709_164_800));
        assert_eq!(parse_release_date_epoch("2023-02-29"), None);
    }

    #[test]
    fn debug_output_redacts_the_client_id() {
        let debug = format!("{:?}", provider());
        assert!(debug.contains("[redacted]"));
        assert!(!debug.contains("fixture_client_id"));
    }
}
