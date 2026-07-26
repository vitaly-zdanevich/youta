//! `PeerTube` provider backed by an instance's official REST API.
//!
//! A configured instance can return local and federated results. Youta does not
//! silently opt into a third-party global search index; that policy remains
//! under control of the `PeerTube` administrator and user configuration.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;
use url::Url;

use super::{
    ChannelSummary, DEFAULT_MAX_JSON_BYTES, DEFAULT_REQUEST_TIMEOUT, Provider,
    ProviderCapabilities, ProviderError, SearchDate, SearchDuration, SearchFeature, SearchItem,
    SearchPage, SearchRequest, SearchSort, SearchTarget, Thumbnail, VideoDetails, VideoSummary,
    get_bounded_json, parse_rfc3339_epoch, provider_agent, resolve_http_url, validate_base_url,
};

const RESULTS_PER_PAGE: u32 = 20;
const MAX_CONFIGURED_JSON_BYTES: usize = 64 * 1024 * 1024;

/// Blocking client for one configurable `PeerTube` instance.
#[derive(Clone)]
pub struct PeerTubeProvider {
    base_url: Url,
    agent: ureq::Agent,
    max_json_bytes: usize,
}

impl PeerTubeProvider {
    /// Creates a provider with conservative response and timeout limits.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] when the base URL is invalid.
    pub fn new(base_url: Url) -> Result<Self, ProviderError> {
        Self::with_options(base_url, DEFAULT_REQUEST_TIMEOUT, DEFAULT_MAX_JSON_BYTES)
    }

    /// Creates a provider with explicit end-to-end timeout and JSON size limit.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] when the URL, timeout, or size limit is
    /// invalid.
    pub fn with_options(
        base_url: Url,
        timeout: Duration,
        max_json_bytes: usize,
    ) -> Result<Self, ProviderError> {
        let base_url = validate_base_url(base_url)?;
        if timeout.is_zero() {
            return Err(ProviderError::InvalidRequest(
                "PeerTube timeout must be greater than zero".to_owned(),
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
        })
    }

    /// Returns the normalized configured instance URL.
    #[must_use]
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    fn build_search_url(&self, request: &SearchRequest) -> Result<Url, ProviderError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| ProviderError::Transport(error.to_string()))?
            .as_secs();
        self.build_search_url_at(request, i64::try_from(now).unwrap_or(i64::MAX))
    }

    fn build_search_url_at(
        &self,
        request: &SearchRequest,
        now_epoch: i64,
    ) -> Result<Url, ProviderError> {
        request.validate()?;
        if request.target == SearchTarget::Channels {
            if request.sort == SearchSort::Views {
                return Err(ProviderError::InvalidRequest(
                    "PeerTube channel search does not support view ordering".to_owned(),
                ));
            }
            if request.filters != super::SearchFilters::default() {
                return Err(ProviderError::InvalidRequest(
                    "PeerTube channel search does not accept video filters".to_owned(),
                ));
            }
        }
        let endpoint = match request.target {
            SearchTarget::Videos => "api/v1/search/videos",
            SearchTarget::Channels => "api/v1/search/video-channels",
        };
        let mut url = self
            .base_url
            .join(endpoint)
            .map_err(|error| ProviderError::InvalidBaseUrl(error.to_string()))?;
        let start = request
            .page
            .saturating_sub(1)
            .checked_mul(RESULTS_PER_PAGE)
            .ok_or_else(|| ProviderError::InvalidRequest("search page is too large".to_owned()))?;

        {
            let mut query = url.query_pairs_mut();
            query.append_pair("search", request.query.trim());
            query.append_pair("start", &start.to_string());
            query.append_pair("count", &RESULTS_PER_PAGE.to_string());
            query.append_pair("skipCount", "false");

            match (request.target, request.sort) {
                (_, SearchSort::Relevance) => {}
                (SearchTarget::Videos, SearchSort::Views) => {
                    query.append_pair("sort", "-views");
                }
                (SearchTarget::Channels, SearchSort::Views) => {
                    unreachable!("channel view ordering was rejected before URL construction")
                }
                (_, SearchSort::UploadDate) => return Err(ProviderError::Unsupported),
            }

            if request.target == SearchTarget::Channels {
                // Channel search has no media-specific filters.
            } else if request.filters.region.is_some() {
                return Err(ProviderError::InvalidRequest(
                    "PeerTube search does not support country filters".to_owned(),
                ));
            } else if let Some(date) = request.filters.date {
                let seconds = match date {
                    SearchDate::Hour => 60 * 60,
                    SearchDate::Today => 24 * 60 * 60,
                    SearchDate::Week => 7 * 24 * 60 * 60,
                    SearchDate::Month => 30 * 24 * 60 * 60,
                    SearchDate::Year => 365 * 24 * 60 * 60,
                };
                let start_epoch = now_epoch.saturating_sub(seconds);
                query.append_pair("startDate", &format_epoch_utc(start_epoch));
            }
            if request.target == SearchTarget::Videos
                && let Some(duration) = request.filters.duration
            {
                match duration {
                    SearchDuration::Short => {
                        query.append_pair("durationMax", "240");
                    }
                    SearchDuration::Medium => {
                        query.append_pair("durationMin", "240");
                        query.append_pair("durationMax", "1200");
                    }
                    SearchDuration::Long => {
                        query.append_pair("durationMin", "1200");
                    }
                }
            }
            for feature in request
                .filters
                .features
                .iter()
                .filter(|_| request.target == SearchTarget::Videos)
            {
                match feature {
                    SearchFeature::Live => {
                        query.append_pair("isLive", "true");
                    }
                    SearchFeature::CreativeCommons => {
                        for license_id in 1..=8 {
                            query.append_pair("licenceOneOf", &license_id.to_string());
                        }
                    }
                    unsupported => {
                        return Err(ProviderError::InvalidRequest(format!(
                            "PeerTube does not expose the {unsupported:?} search feature"
                        )));
                    }
                }
            }
        }
        Ok(url)
    }

    fn build_video_url(&self, video_id: &str) -> Result<Url, ProviderError> {
        validate_peertube_identifier(video_id, "video identifier")?;
        let mut url = self
            .base_url
            .join("api/v1/videos/")
            .map_err(|error| ProviderError::InvalidBaseUrl(error.to_string()))?;
        url.path_segments_mut()
            .map_err(|()| ProviderError::InvalidBaseUrl("URL cannot contain API paths".to_owned()))?
            .push(video_id);
        Ok(url)
    }

    fn parse_video_page(
        &self,
        raw: RawPage<RawVideo>,
        page: u32,
    ) -> Result<SearchPage, ProviderError> {
        let item_count = raw.data.len();
        let items = raw
            .data
            .into_iter()
            .map(|video| self.convert_video_summary(video).map(SearchItem::Video))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(SearchPage {
            page,
            items,
            next_page: page_has_more(page, item_count, raw.total).then_some(page + 1),
        })
    }

    fn parse_channel_page(
        &self,
        raw: RawPage<RawChannel>,
        page: u32,
    ) -> Result<SearchPage, ProviderError> {
        let item_count = raw.data.len();
        let items = raw
            .data
            .into_iter()
            .map(|channel| {
                self.convert_channel_summary(channel)
                    .map(SearchItem::Channel)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(SearchPage {
            page,
            items,
            next_page: page_has_more(page, item_count, raw.total).then_some(page + 1),
        })
    }

    fn convert_video_summary(&self, raw: RawVideo) -> Result<VideoSummary, ProviderError> {
        validate_peertube_identifier(&raw.uuid, "video UUID")?;
        require_nonempty(&raw.name, "video name")?;
        let channel = raw.channel.ok_or_else(|| {
            ProviderError::InvalidResponse("PeerTube video has no channel".to_owned())
        })?;
        let channel_id = channel_handle(&channel)?;
        let channel_name = channel
            .display_name
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(channel.name);
        let webpage_url = self.video_webpage_url(&raw.uuid);

        Ok(VideoSummary {
            video_id: raw.uuid,
            title: raw.name,
            channel_name,
            channel_id,
            description: raw.description.unwrap_or_default(),
            duration_seconds: raw.duration,
            view_count: raw.views,
            published_at: raw.published_at.as_deref().and_then(parse_rfc3339_epoch),
            published_text: raw.published_at,
            live: raw.is_live,
            thumbnails: self.convert_images(raw.thumbnails, raw.thumbnail_path),
            webpage_url,
            stream_url: None,
        })
    }

    fn convert_channel_summary(&self, raw: RawChannel) -> Result<ChannelSummary, ProviderError> {
        let channel_id = channel_handle(&raw)?;
        let name = raw
            .display_name
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| raw.name.clone());
        require_nonempty(&name, "channel display name")?;
        let webpage_url = self.channel_webpage_url(&channel_id);

        Ok(ChannelSummary {
            channel_id,
            name,
            description: raw.description.unwrap_or_default(),
            subscriber_count: raw.followers_count,
            video_count: raw.videos_count,
            auto_generated: false,
            thumbnails: self.convert_images(raw.avatars, None),
            webpage_url,
        })
    }

    fn convert_video_details(&self, raw: RawVideoDetails) -> Result<VideoDetails, ProviderError> {
        validate_peertube_identifier(&raw.uuid, "video UUID")?;
        require_nonempty(&raw.name, "video name")?;
        let channel = raw.channel.ok_or_else(|| {
            ProviderError::InvalidResponse("PeerTube video has no channel".to_owned())
        })?;
        let channel_id = channel_handle(&channel)?;
        let channel_name = channel
            .display_name
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(channel.name);
        let license = raw
            .licence
            .as_ref()
            .and_then(extract_label)
            .filter(|value| !value.trim().is_empty());
        let webpage_url = self.video_webpage_url(&raw.uuid);

        Ok(VideoDetails {
            video_id: raw.uuid,
            title: raw.name,
            channel_name,
            channel_id,
            description: raw.description.unwrap_or_default(),
            duration_seconds: raw.duration,
            view_count: raw.views,
            like_count: raw.likes,
            published_at: raw.published_at.as_deref().and_then(parse_rfc3339_epoch),
            published_text: raw.published_at,
            license,
            rating: None,
            ratings_allowed: Some(true),
            live: raw.is_live,
            keywords: raw.tags,
            thumbnails: self.convert_images(raw.thumbnails, raw.thumbnail_path),
            webpage_url,
            stream_url: None,
        })
    }

    fn convert_images(
        &self,
        images: Vec<RawImage>,
        fallback_path: Option<String>,
    ) -> Vec<Thumbnail> {
        let mut thumbnails = images
            .into_iter()
            .filter_map(|image| {
                let raw_url = image.file_url.or(image.path)?;
                let url = resolve_http_url(&self.base_url, &raw_url).ok()?;
                Some(Thumbnail {
                    url,
                    quality: None,
                    width: image.width.and_then(|value| u32::try_from(value).ok()),
                    height: image.height.and_then(|value| u32::try_from(value).ok()),
                })
            })
            .collect::<Vec<_>>();
        if thumbnails.is_empty()
            && let Some(path) = fallback_path
            && let Ok(url) = resolve_http_url(&self.base_url, &path)
        {
            thumbnails.push(Thumbnail {
                url,
                quality: None,
                width: None,
                height: None,
            });
        }
        thumbnails
    }

    fn video_webpage_url(&self, video_id: &str) -> Option<Url> {
        let mut url = self.base_url.join("w/").ok()?;
        url.path_segments_mut().ok()?.pop_if_empty().push(video_id);
        Some(url)
    }

    fn channel_webpage_url(&self, channel_id: &str) -> Option<Url> {
        let mut url = self.base_url.join("video-channels/").ok()?;
        url.path_segments_mut()
            .ok()?
            .pop_if_empty()
            .push(channel_id);
        Some(url)
    }
}

impl Provider for PeerTubeProvider {
    fn id(&self) -> &'static str {
        "peertube"
    }

    fn display_name(&self) -> &'static str {
        "PeerTube"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            video_search: true,
            channel_search: true,
            pagination: true,
            search_filters: true,
            search_sorting: true,
            video_details: true,
            thumbnails: true,
        }
    }

    fn search(&self, request: &SearchRequest) -> Result<SearchPage, ProviderError> {
        let url = self.build_search_url(request)?;
        match request.target {
            SearchTarget::Videos => {
                let page: RawPage<RawVideo> =
                    get_bounded_json(&self.agent, &url, self.max_json_bytes)?;
                self.parse_video_page(page, request.page)
            }
            SearchTarget::Channels => {
                let page: RawPage<RawChannel> =
                    get_bounded_json(&self.agent, &url, self.max_json_bytes)?;
                self.parse_channel_page(page, request.page)
            }
        }
    }

    fn video_details(&self, video_id: &str) -> Result<VideoDetails, ProviderError> {
        let url = self.build_video_url(video_id)?;
        let details: RawVideoDetails = get_bounded_json(&self.agent, &url, self.max_json_bytes)?;
        self.convert_video_details(details)
    }
}

fn validate_peertube_identifier(value: &str, field: &str) -> Result<(), ProviderError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'@' | b'.'))
    {
        return Err(ProviderError::InvalidRequest(format!(
            "{field} contains invalid characters"
        )));
    }
    Ok(())
}

fn require_nonempty(value: &str, field: &str) -> Result<(), ProviderError> {
    if value.trim().is_empty() {
        return Err(ProviderError::InvalidResponse(format!(
            "{field} cannot be empty"
        )));
    }
    Ok(())
}

fn channel_handle(channel: &RawChannel) -> Result<String, ProviderError> {
    require_nonempty(&channel.name, "channel name")?;
    let handle = match channel.host.as_deref() {
        Some(host) if !host.is_empty() && !channel.name.contains('@') => {
            format!("{}@{host}", channel.name)
        }
        _ => channel.name.clone(),
    };
    validate_peertube_identifier(&handle, "channel handle")
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    Ok(handle)
}

fn page_has_more(page: u32, returned: usize, total: u64) -> bool {
    let start = u64::from(page.saturating_sub(1)) * u64::from(RESULTS_PER_PAGE);
    start.saturating_add(u64::try_from(returned).unwrap_or(u64::MAX)) < total
}

fn extract_label(value: &Value) -> Option<String> {
    match value {
        Value::String(label) => Some(label.clone()),
        Value::Object(object) => object
            .get("label")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        _ => None,
    }
}

fn format_epoch_utc(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let seconds = epoch.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds / 3600;
    let minute = (seconds % 3600) / 60;
    let second = seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[derive(Debug, Deserialize)]
struct RawPage<T> {
    #[serde(default, deserialize_with = "deserialize_u64")]
    total: u64,
    data: Vec<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawVideo {
    uuid: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    duration: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    views: Option<u64>,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    is_live: bool,
    #[serde(default)]
    thumbnail_path: Option<String>,
    #[serde(default)]
    thumbnails: Vec<RawImage>,
    #[serde(default, alias = "videoChannel")]
    channel: Option<RawChannel>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawVideoDetails {
    uuid: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    duration: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    views: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    likes: Option<u64>,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    is_live: bool,
    #[serde(default)]
    thumbnail_path: Option<String>,
    #[serde(default)]
    thumbnails: Vec<RawImage>,
    #[serde(default, alias = "videoChannel")]
    channel: Option<RawChannel>,
    #[serde(default)]
    licence: Option<Value>,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawChannel {
    name: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    followers_count: Option<u64>,
    #[serde(
        default,
        alias = "videoCount",
        deserialize_with = "deserialize_optional_u64"
    )]
    videos_count: Option<u64>,
    #[serde(default)]
    avatars: Vec<RawImage>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawImage {
    #[serde(default)]
    file_url: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    width: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    height: Option<u64>,
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

    const VIDEO_PAGE_FIXTURE: &str = r#"{
        "total": 21,
        "data": [{
            "uuid": "9c9de5e8-0a1e-484a-b099-e80766180a6d",
            "name": "Federated video",
            "description": "Description",
            "duration": 125,
            "views": "42",
            "publishedAt": "2024-01-02T03:04:05Z",
            "isLive": false,
            "thumbnailPath": "/lazy-static/thumbnails/video.jpg",
            "channel": {
                "name": "example",
                "displayName": "Example Channel",
                "host": "remote.example",
                "avatars": []
            }
        }]
    }"#;

    const CHANNEL_PAGE_FIXTURE: &str = r#"{
        "total": 1,
        "data": [{
            "name": "example",
            "displayName": "Example Channel",
            "host": "remote.example",
            "description": "Channel description",
            "followersCount": 123,
            "videosCount": "7",
            "avatars": [{"fileUrl": "https://remote.example/avatar.jpg", "width": 120, "height": 120}]
        }]
    }"#;

    const DETAILS_FIXTURE: &str = r#"{
        "uuid": "9c9de5e8-0a1e-484a-b099-e80766180a6d",
        "name": "Federated video",
        "description": "Long description",
        "duration": 125,
        "views": 42,
        "likes": "9",
        "publishedAt": "2024-01-02T03:04:05+02:30",
        "isLive": false,
        "thumbnailPath": "/lazy-static/thumbnails/video.jpg",
        "channel": {
            "name": "example",
            "displayName": "Example Channel",
            "host": "remote.example"
        },
        "licence": {"id": 2, "label": "Attribution - Share Alike"},
        "tags": ["music", "federated"]
    }"#;

    fn provider() -> PeerTubeProvider {
        PeerTubeProvider::new(
            Url::parse("https://peertube.example.test/prefix").expect("fixture URL should parse"),
        )
        .expect("fixture provider should construct")
    }

    #[test]
    fn video_search_url_maps_paging_sort_and_supported_filters() {
        let mut request = SearchRequest::new("free music", SearchTarget::Videos);
        request.page = 2;
        request.sort = SearchSort::Views;
        request.filters.date = Some(SearchDate::Week);
        request.filters.duration = Some(SearchDuration::Medium);
        request.filters.features = vec![SearchFeature::Live, SearchFeature::CreativeCommons];
        let url = provider()
            .build_search_url_at(&request, 1_704_164_645)
            .expect("request should map to PeerTube");
        let pairs = url.query_pairs().collect::<Vec<_>>();

        assert_eq!(url.path(), "/prefix/api/v1/search/videos");
        assert!(pairs.contains(&("start".into(), "20".into())));
        assert!(pairs.contains(&("sort".into(), "-views".into())));
        assert!(pairs.contains(&("durationMin".into(), "240".into())));
        assert!(pairs.contains(&("durationMax".into(), "1200".into())));
        assert!(pairs.contains(&("isLive".into(), "true".into())));
        assert_eq!(
            pairs
                .iter()
                .filter(|(key, _)| key == "licenceOneOf")
                .count(),
            8
        );
        assert!(pairs.contains(&("startDate".into(), "2023-12-26T03:04:05Z".into())));
    }

    #[test]
    fn channel_search_is_a_separate_endpoint() {
        let request = SearchRequest::new("channel", SearchTarget::Channels);
        let url = provider()
            .build_search_url_at(&request, 0)
            .expect("channel request should map");

        assert_eq!(url.path(), "/prefix/api/v1/search/video-channels");
    }

    #[test]
    fn video_page_uses_total_for_exact_lazy_pagination() {
        let raw = serde_json::from_str(VIDEO_PAGE_FIXTURE).expect("fixture should parse");
        let page = provider()
            .parse_video_page(raw, 1)
            .expect("fixture should convert");
        let [SearchItem::Video(video)] = page.items.as_slice() else {
            panic!("expected one video");
        };

        assert_eq!(page.next_page, Some(2));
        assert_eq!(video.channel_id, "example@remote.example");
        assert_eq!(video.view_count, Some(42));
        assert_eq!(video.published_at, Some(1_704_164_645));
        assert_eq!(
            video.thumbnails[0].url.as_str(),
            "https://peertube.example.test/lazy-static/thumbnails/video.jpg"
        );
    }

    #[test]
    fn channel_page_parses_federated_handle_and_avatar() {
        let raw = serde_json::from_str(CHANNEL_PAGE_FIXTURE).expect("fixture should parse");
        let page = provider()
            .parse_channel_page(raw, 1)
            .expect("fixture should convert");
        let [SearchItem::Channel(channel)] = page.items.as_slice() else {
            panic!("expected one channel");
        };

        assert_eq!(page.next_page, None);
        assert_eq!(channel.channel_id, "example@remote.example");
        assert_eq!(channel.subscriber_count, Some(123));
        assert_eq!(channel.video_count, Some(7));
    }

    #[test]
    fn details_retain_license_likes_tags_and_timezone_correct_date() {
        let raw = serde_json::from_str(DETAILS_FIXTURE).expect("fixture should parse");
        let details = provider()
            .convert_video_details(raw)
            .expect("fixture should convert");

        assert_eq!(details.like_count, Some(9));
        assert_eq!(
            details.license.as_deref(),
            Some("Attribution - Share Alike")
        );
        assert_eq!(details.keywords, ["music", "federated"]);
        assert_eq!(details.published_at, Some(1_704_155_645));
    }

    #[test]
    fn rfc3339_parser_validates_calendar_and_offsets() {
        assert_eq!(parse_rfc3339_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_epoch("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(parse_rfc3339_epoch("2023-02-29T00:00:00Z"), None);
        assert_eq!(format_epoch_utc(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn video_identifier_rejects_path_injection() {
        assert!(matches!(
            provider().build_video_url("../config"),
            Err(ProviderError::InvalidRequest(_))
        ));
    }
}
