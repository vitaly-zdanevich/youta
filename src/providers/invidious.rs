//! Invidious search and video-metadata provider.
//!
//! The provider uses only the documented JSON API. Stream extraction remains a
//! playback concern so an Invidious instance can be combined with `yt-dlp` or a
//! different playback backend.

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use url::Url;

use super::ChannelSummary;
use super::{
    ChannelStatisticsMode, ChannelSubscriberCount, DEFAULT_MAX_JSON_BYTES, DEFAULT_REQUEST_TIMEOUT,
    Provider, ProviderCapabilities, ProviderError, SearchDate, SearchDuration, SearchFeature,
    SearchItem, SearchPage, SearchRequest, SearchSort, SearchTarget, Thumbnail, VideoDetails,
    VideoSummary, get_bounded_json, provider_agent, resolve_http_url, validate_base_url,
    validate_youtube_video_id,
};

const MAX_CONFIGURED_JSON_BYTES: usize = 64 * 1024 * 1024;

/// Blocking client for a configurable Invidious instance.
///
/// Clone the provider only when multiple worker threads need access; `ureq`
/// clones share their connection pool.
#[derive(Clone)]
pub struct InvidiousProvider {
    base_url: Url,
    agent: ureq::Agent,
    max_json_bytes: usize,
}

impl InvidiousProvider {
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
                "provider timeout must be greater than zero".to_owned(),
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
        request.validate()?;
        let mut url = self.endpoint("api/v1/search")?;
        let features = request
            .filters
            .features
            .iter()
            .map(|feature| search_feature_value(*feature))
            .collect::<Vec<_>>()
            .join(",");

        {
            let mut query = url.query_pairs_mut();
            query.append_pair("q", request.query.trim());
            query.append_pair("page", &request.page.to_string());
            query.append_pair(
                "type",
                match request.target {
                    SearchTarget::Videos => "video",
                    SearchTarget::Channels => "channel",
                },
            );
            query.append_pair(
                "sort",
                match request.sort {
                    SearchSort::Relevance => "relevance",
                    SearchSort::Views => "views",
                    // Current Invidious search accepts relevance and views,
                    // but no upload-date ordering. Fetch the exact relevance
                    // page and apply a stable page-local order below.
                    SearchSort::UploadDate => "relevance",
                },
            );
            if let Some(date) = request.filters.date {
                query.append_pair("date", search_date_value(date));
            }
            if let Some(duration) = request.filters.duration {
                query.append_pair("duration", search_duration_value(duration));
            }
            if !features.is_empty() {
                query.append_pair("features", &features);
            }
            if let Some(region) = &request.filters.region {
                query.append_pair("region", &region.to_ascii_uppercase());
            }
        }
        Ok(url)
    }

    fn build_video_url(&self, video_id: &str) -> Result<Url, ProviderError> {
        validate_youtube_video_id(video_id)?;
        let mut url = self.endpoint("api/v1/videos/")?;
        url.path_segments_mut()
            .map_err(|()| ProviderError::InvalidBaseUrl("URL cannot contain API paths".to_owned()))?
            .push(video_id);
        Ok(url)
    }

    fn build_channel_url(&self, channel_id: &str) -> Result<Url, ProviderError> {
        validate_resource_id(channel_id, "channel ID").map_err(|_| {
            ProviderError::InvalidRequest(
                "Invidious channel ID contains invalid characters".to_owned(),
            )
        })?;
        let mut url = self.endpoint("api/v1/channels/")?;
        url.path_segments_mut()
            .map_err(|()| ProviderError::InvalidBaseUrl("URL cannot contain API paths".to_owned()))?
            .pop_if_empty()
            .push(channel_id);
        Ok(url)
    }

    fn endpoint(&self, path: &str) -> Result<Url, ProviderError> {
        self.base_url
            .join(path)
            .map_err(|error| ProviderError::InvalidBaseUrl(error.to_string()))
    }

    fn parse_search_values(
        &self,
        values: Vec<Value>,
        request: &SearchRequest,
    ) -> Result<SearchPage, ProviderError> {
        let mut items = Vec::with_capacity(values.len());
        let expected_kind = match request.target {
            SearchTarget::Videos => "video",
            SearchTarget::Channels => "channel",
        };

        for (index, value) in values.into_iter().enumerate() {
            let Some(kind) = value.get("type").and_then(Value::as_str) else {
                continue;
            };
            if kind != expected_kind {
                // Search APIs occasionally add promoted or new result kinds.
                // They are safely ignored instead of breaking the entire page.
                continue;
            }
            let item = match request.target {
                SearchTarget::Videos => {
                    let raw: RawVideoSearch = serde_json::from_value(value).map_err(|error| {
                        ProviderError::InvalidResponse(format!(
                            "search result {index} is not a video: {error}"
                        ))
                    })?;
                    SearchItem::Video(self.convert_video_summary(raw)?)
                }
                SearchTarget::Channels => {
                    let raw: RawChannelSearch = serde_json::from_value(value).map_err(|error| {
                        ProviderError::InvalidResponse(format!(
                            "search result {index} is not a channel: {error}"
                        ))
                    })?;
                    SearchItem::Channel(self.convert_channel_summary(raw)?)
                }
            };
            items.push(item);
        }

        if request.sort == SearchSort::UploadDate && request.target == SearchTarget::Videos {
            sort_videos_by_upload_date(&mut items);
        }
        let next_page =
            (!items.is_empty() && request.page < 10_000).then_some(request.page.saturating_add(1));
        Ok(SearchPage {
            page: request.page,
            items,
            next_page,
        })
    }

    fn convert_video_summary(&self, raw: RawVideoSearch) -> Result<VideoSummary, ProviderError> {
        validate_youtube_video_id(&raw.video_id).map_err(|_| {
            ProviderError::InvalidResponse("video result contains an invalid videoId".to_owned())
        })?;
        validate_resource_id(&raw.author_id, "authorId")?;
        require_nonempty(&raw.title, "video title")?;
        require_nonempty(&raw.author, "video author")?;
        let webpage_url = youtube_video_url(&raw.video_id);

        Ok(VideoSummary {
            video_id: raw.video_id,
            title: raw.title,
            channel_name: raw.author,
            channel_id: raw.author_id,
            description: raw.description,
            duration_seconds: raw.length_seconds,
            view_count: raw.view_count,
            published_at: raw.published,
            published_text: nonempty(raw.published_text),
            live: raw.live_now,
            thumbnails: self.convert_thumbnails(raw.video_thumbnails),
            webpage_url,
            stream_url: None,
        })
    }

    fn convert_channel_summary(
        &self,
        raw: RawChannelSearch,
    ) -> Result<ChannelSummary, ProviderError> {
        validate_resource_id(&raw.author_id, "authorId")?;
        require_nonempty(&raw.author, "channel author")?;
        let webpage_url = youtube_channel_url(&raw.author_id);

        Ok(ChannelSummary {
            channel_id: raw.author_id,
            name: raw.author,
            description: raw.description,
            subscriber_count: raw.sub_count,
            video_count: raw.video_count,
            auto_generated: raw.auto_generated,
            thumbnails: self.convert_thumbnails(raw.author_thumbnails),
            webpage_url,
        })
    }

    fn convert_channel_subscriber_count(
        raw: RawChannelDetails,
        requested_id: &str,
    ) -> Result<ChannelSubscriberCount, ProviderError> {
        validate_resource_id(&raw.author_id, "channel authorId")?;
        if raw.author_id != requested_id {
            return Err(ProviderError::InvalidResponse(
                "channel response identifier does not match the requested channel".to_owned(),
            ));
        }
        Ok(ChannelSubscriberCount {
            channel_id: raw.author_id,
            subscriber_count: raw.sub_count,
        })
    }

    fn convert_video_details(&self, raw: RawVideoDetails) -> Result<VideoDetails, ProviderError> {
        validate_youtube_video_id(&raw.video_id).map_err(|_| {
            ProviderError::InvalidResponse("video details contain an invalid videoId".to_owned())
        })?;
        validate_resource_id(&raw.author_id, "authorId")?;
        require_nonempty(&raw.title, "video title")?;
        require_nonempty(&raw.author, "video author")?;
        let webpage_url = youtube_video_url(&raw.video_id);

        Ok(VideoDetails {
            video_id: raw.video_id,
            title: raw.title,
            channel_name: raw.author,
            channel_id: raw.author_id,
            description: raw.description,
            duration_seconds: raw.length_seconds,
            view_count: raw.view_count,
            like_count: raw.like_count,
            published_at: raw.published,
            published_text: nonempty(raw.published_text),
            license: nonempty(raw.license),
            rating: raw.rating,
            ratings_allowed: raw.allow_ratings,
            live: raw.live_now,
            keywords: raw.keywords,
            thumbnails: self.convert_thumbnails(raw.video_thumbnails),
            webpage_url,
            stream_url: None,
        })
    }

    fn convert_thumbnails(&self, raw: Vec<RawThumbnail>) -> Vec<Thumbnail> {
        raw.into_iter()
            .filter_map(|thumbnail| {
                let url = resolve_http_url(&self.base_url, &thumbnail.url).ok()?;
                Some(Thumbnail {
                    url,
                    quality: nonempty(thumbnail.quality),
                    width: thumbnail.width.and_then(|value| u32::try_from(value).ok()),
                    height: thumbnail.height.and_then(|value| u32::try_from(value).ok()),
                })
            })
            .collect()
    }
}

/// Applies Invidious's upload-date fallback without changing equal or unknown
/// timestamp ordering.
///
/// Invidious currently returns relevance pages for this mode because its
/// documented search filter supports no date sort. The stable sort is
/// intentionally page-local so pagination continues to request the provider's
/// exact page numbers.
fn sort_videos_by_upload_date(items: &mut [SearchItem]) {
    items.sort_by(|left, right| {
        let published = |item: &SearchItem| match item {
            SearchItem::Video(video) => video.published_at,
            SearchItem::Channel(_) => None,
        };
        match (published(left), published(right)) {
            (Some(left), Some(right)) => right.cmp(&left),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    });
}

fn youtube_video_url(video_id: &str) -> Option<Url> {
    Url::parse_with_params("https://www.youtube.com/watch", [("v", video_id)]).ok()
}

fn youtube_channel_url(channel_id: &str) -> Option<Url> {
    let mut url = Url::parse("https://www.youtube.com/channel/").ok()?;
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .push(channel_id);
    Some(url)
}

impl Provider for InvidiousProvider {
    fn id(&self) -> &'static str {
        "invidious"
    }

    fn display_name(&self) -> &'static str {
        "Invidious"
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

    fn channel_statistics_mode(&self) -> ChannelStatisticsMode {
        ChannelStatisticsMode::SelectedOnly
    }

    fn search(&self, request: &SearchRequest) -> Result<SearchPage, ProviderError> {
        let url = self.build_search_url(request)?;
        let values: Vec<Value> = get_bounded_json(&self.agent, &url, self.max_json_bytes)?;
        self.parse_search_values(values, request)
    }

    fn video_details(&self, video_id: &str) -> Result<VideoDetails, ProviderError> {
        let url = self.build_video_url(video_id)?;
        let raw: RawVideoDetails = get_bounded_json(&self.agent, &url, self.max_json_bytes)?;
        self.convert_video_details(raw)
    }

    fn channel_subscriber_counts(
        &self,
        channel_ids: &[String],
    ) -> Result<Vec<ChannelSubscriberCount>, ProviderError> {
        if channel_ids.len() > 1 {
            return Err(ProviderError::InvalidRequest(
                "Invidious channel statistics accepts one identifier per request".to_owned(),
            ));
        }
        let Some(channel_id) = channel_ids.first() else {
            return Ok(Vec::new());
        };
        let url = self.build_channel_url(channel_id)?;
        let raw: RawChannelDetails = get_bounded_json(&self.agent, &url, self.max_json_bytes)?;
        Ok(vec![Self::convert_channel_subscriber_count(
            raw, channel_id,
        )?])
    }
}

fn require_nonempty(value: &str, field: &str) -> Result<(), ProviderError> {
    if value.trim().is_empty() {
        return Err(ProviderError::InvalidResponse(format!(
            "{field} cannot be empty"
        )));
    }
    Ok(())
}

fn validate_resource_id(value: &str, field: &str) -> Result<(), ProviderError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'@'))
    {
        return Err(ProviderError::InvalidResponse(format!(
            "{field} contains invalid characters"
        )));
    }
    Ok(())
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.trim().is_empty())
}

const fn search_date_value(date: SearchDate) -> &'static str {
    match date {
        SearchDate::Hour => "hour",
        SearchDate::Today => "today",
        SearchDate::Week => "week",
        SearchDate::Month => "month",
        SearchDate::Year => "year",
    }
}

const fn search_duration_value(duration: SearchDuration) -> &'static str {
    match duration {
        SearchDuration::Short => "short",
        SearchDuration::Medium => "medium",
        SearchDuration::Long => "long",
    }
}

const fn search_feature_value(feature: SearchFeature) -> &'static str {
    match feature {
        SearchFeature::Hd => "hd",
        SearchFeature::Subtitles => "subtitles",
        SearchFeature::CreativeCommons => "creative_commons",
        SearchFeature::ThreeD => "3d",
        SearchFeature::Live => "live",
        SearchFeature::Purchased => "purchased",
        SearchFeature::FourK => "4k",
        SearchFeature::ThreeSixty => "360",
        SearchFeature::Location => "location",
        SearchFeature::Hdr => "hdr",
        SearchFeature::Vr180 => "vr180",
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawVideoSearch {
    video_id: String,
    title: String,
    author: String,
    author_id: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    video_thumbnails: Vec<RawThumbnail>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    view_count: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_i64")]
    published: Option<i64>,
    #[serde(default)]
    published_text: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    length_seconds: Option<u64>,
    #[serde(default)]
    live_now: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawChannelSearch {
    author: String,
    author_id: String,
    #[serde(default)]
    author_thumbnails: Vec<RawThumbnail>,
    #[serde(default)]
    auto_generated: bool,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    sub_count: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    video_count: Option<u64>,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawChannelDetails {
    author_id: String,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    sub_count: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawVideoDetails {
    video_id: String,
    title: String,
    author: String,
    author_id: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    video_thumbnails: Vec<RawThumbnail>,
    #[serde(default, deserialize_with = "deserialize_optional_i64")]
    published: Option<i64>,
    #[serde(default)]
    published_text: Option<String>,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    view_count: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    like_count: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    length_seconds: Option<u64>,
    #[serde(default)]
    allow_ratings: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_f64")]
    rating: Option<f64>,
    #[serde(default)]
    live_now: bool,
    #[serde(default)]
    license: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawThumbnail {
    url: String,
    #[serde(default)]
    quality: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    width: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    height: Option<u64>,
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

fn deserialize_optional_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_i64()
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom("expected an integer")),
        Some(Value::String(text)) => text
            .parse::<i64>()
            .map(Some)
            .map_err(serde::de::Error::custom),
        Some(_) => Err(serde::de::Error::custom(
            "expected an integer, numeric string, or null",
        )),
    }
}

fn deserialize_optional_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_f64()
            .filter(|number| number.is_finite())
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom("expected a finite number")),
        Some(Value::String(text)) => text
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite())
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom("expected a finite number")),
        Some(_) => Err(serde::de::Error::custom(
            "expected a number, numeric string, or null",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH_FIXTURE: &str = r#"[
		{
			"type": "video",
			"title": "A video",
			"videoId": "dQw4w9WgXcQ",
			"author": "Example channel",
			"authorId": "UC_x5XG1OV2P6uZZ5FSM9Ttw",
			"description": "Description",
			"viewCount": "1234",
			"published": 1700000000,
			"publishedText": "2 years ago",
			"lengthSeconds": 212,
			"liveNow": false,
			"videoThumbnails": [
				{"quality": "medium", "url": "//i.ytimg.com/vi/dQw4w9WgXcQ/mqdefault.jpg", "width": 320, "height": 180},
				{"quality": "broken", "url": "file:///etc/passwd", "width": 1, "height": 1}
			]
		},
		{"type": "playlist", "title": "Ignored"}
	]"#;

    const CHANNEL_FIXTURE: &str = r#"[
		{
			"type": "channel",
			"author": "Example channel",
			"authorId": "UC_x5XG1OV2P6uZZ5FSM9Ttw",
			"authorThumbnails": [{"url": "/ggpht/channel.jpg", "width": "176", "height": 176}],
			"autoGenerated": false,
			"subCount": 42,
			"videoCount": "9",
			"description": "Channel description"
		}
	]"#;

    const DETAILS_FIXTURE: &str = r#"{
		"type": "video",
		"title": "A video",
		"videoId": "dQw4w9WgXcQ",
		"videoThumbnails": [{"quality": "maxres", "url": "https://i.ytimg.com/vi/dQw4w9WgXcQ/maxresdefault.jpg", "width": 1280, "height": 720}],
		"description": "Full description",
		"published": "1700000000",
		"publishedText": "2 years ago",
		"keywords": ["music", "example"],
		"viewCount": 1000,
		"likeCount": "50",
		"author": "Example channel",
		"authorId": "UC_x5XG1OV2P6uZZ5FSM9Ttw",
		"lengthSeconds": 212,
		"allowRatings": true,
		"rating": "4.75",
		"liveNow": false,
		"license": "Creative Commons Attribution licence"
	}"#;

    const CHANNEL_DETAILS_FIXTURE: &str = r#"{
		"author": "Example channel",
		"authorId": "UC_x5XG1OV2P6uZZ5FSM9Ttw",
		"subCount": "12345",
		"description": "Fields unrelated to this lookup are ignored"
	}"#;

    fn provider() -> InvidiousProvider {
        InvidiousProvider::new(
            Url::parse("https://invidious.example.test/prefix")
                .expect("fixture base URL should parse"),
        )
        .expect("fixture provider should construct")
    }

    #[test]
    fn search_url_encodes_scope_sort_filters_and_page() {
        let provider = provider();
        let mut request = SearchRequest::new("ambient & drone", SearchTarget::Videos);
        request.page = 3;
        request.sort = SearchSort::Views;
        request.filters.date = Some(SearchDate::Month);
        request.filters.duration = Some(SearchDuration::Long);
        request.filters.features = vec![SearchFeature::CreativeCommons, SearchFeature::FourK];
        request.filters.region = Some("ge".to_owned());

        let url = provider
            .build_search_url(&request)
            .expect("valid request should make a URL");
        let pairs = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();

        assert_eq!(
            url.path(),
            "/prefix/api/v1/search",
            "configured subpath must be retained"
        );
        assert_eq!(pairs.get("q").map(AsRef::as_ref), Some("ambient & drone"));
        assert_eq!(pairs.get("type").map(AsRef::as_ref), Some("video"));
        assert_eq!(pairs.get("sort").map(AsRef::as_ref), Some("views"));
        assert_eq!(pairs.get("page").map(AsRef::as_ref), Some("3"));
        assert_eq!(pairs.get("date").map(AsRef::as_ref), Some("month"));
        assert_eq!(pairs.get("duration").map(AsRef::as_ref), Some("long"));
        assert_eq!(
            pairs.get("features").map(AsRef::as_ref),
            Some("creative_commons,4k")
        );
        assert_eq!(pairs.get("region").map(AsRef::as_ref), Some("GE"));
    }

    #[test]
    fn upload_date_search_keeps_supported_query_and_stably_sorts_each_page() {
        let provider = provider();
        let mut request = SearchRequest::new("new releases", SearchTarget::Videos);
        request.page = 4;
        request.sort = SearchSort::UploadDate;
        let url = provider
            .build_search_url(&request)
            .expect("newest fallback should make a supported URL");
        let pairs = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(pairs.get("sort").map(AsRef::as_ref), Some("relevance"));
        assert_eq!(pairs.get("page").map(AsRef::as_ref), Some("4"));

        let values = serde_json::json!([
            {
                "type": "video", "title": "Old", "videoId": "aaaaaaaaaaa",
                "author": "Channel", "authorId": "UC_fixture", "published": 100
            },
            {
                "type": "video", "title": "Newest first", "videoId": "bbbbbbbbbbb",
                "author": "Channel", "authorId": "UC_fixture", "published": 300
            },
            {
                "type": "video", "title": "Unknown", "videoId": "ccccccccccc",
                "author": "Channel", "authorId": "UC_fixture"
            },
            {
                "type": "video", "title": "Newest second", "videoId": "ddddddddddd",
                "author": "Channel", "authorId": "UC_fixture", "published": 300
            }
        ]);
        let page = provider
            .parse_search_values(values.as_array().expect("array fixture").clone(), &request)
            .expect("page-local newest fallback should parse");
        let titles = page
            .items
            .iter()
            .map(|item| match item {
                SearchItem::Video(video) => video.title.as_str(),
                SearchItem::Channel(_) => unreachable!("video fixture"),
            })
            .collect::<Vec<_>>();

        assert_eq!(
            titles,
            ["Newest first", "Newest second", "Old", "Unknown"],
            "equal timestamps must retain provider order and missing timestamps go last"
        );
        assert_eq!(page.page, 4);
        assert_eq!(page.next_page, Some(5));
    }

    #[test]
    fn parses_video_search_fixture_and_ignores_other_result_kinds() {
        let provider = provider();
        let values = serde_json::from_str(SEARCH_FIXTURE).expect("fixture should be JSON");
        let request = SearchRequest::new("video", SearchTarget::Videos);

        let page = provider
            .parse_search_values(values, &request)
            .expect("fixture should parse");
        assert_eq!(page.page, 1);
        assert_eq!(page.next_page, Some(2));
        let [SearchItem::Video(video)] = page.items.as_slice() else {
            panic!("expected one video");
        };
        assert_eq!(video.video_id, "dQw4w9WgXcQ");
        assert_eq!(video.view_count, Some(1234));
        assert_eq!(video.duration_seconds, Some(212));
        assert_eq!(video.thumbnails.len(), 1, "unsafe URL must be discarded");
        assert_eq!(
            video.thumbnails[0].url.as_str(),
            "https://i.ytimg.com/vi/dQw4w9WgXcQ/mqdefault.jpg"
        );
    }

    #[test]
    fn creative_commons_filter_is_preserved_with_newest_page_requests() {
        let provider = provider();
        let mut request = SearchRequest::new("open music", SearchTarget::Videos);
        request.page = 2;
        request.sort = SearchSort::UploadDate;
        request
            .filters
            .features
            .push(SearchFeature::CreativeCommons);

        let url = provider
            .build_search_url(&request)
            .expect("Creative Commons page should make a supported URL");
        let pairs = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();

        assert_eq!(pairs.get("page").map(AsRef::as_ref), Some("2"));
        assert_eq!(
            pairs.get("features").map(AsRef::as_ref),
            Some("creative_commons")
        );
        assert_eq!(
            pairs.get("sort").map(AsRef::as_ref),
            Some("relevance"),
            "newest remains the documented page-local fallback"
        );
    }

    #[test]
    fn parses_channel_search_fixture_with_flexible_numbers() {
        let provider = provider();
        let values = serde_json::from_str(CHANNEL_FIXTURE).expect("fixture should be JSON");
        let request = SearchRequest::new("channel", SearchTarget::Channels);

        let page = provider
            .parse_search_values(values, &request)
            .expect("fixture should parse");
        let [SearchItem::Channel(channel)] = page.items.as_slice() else {
            panic!("expected one channel");
        };
        assert_eq!(channel.subscriber_count, Some(42));
        assert_eq!(channel.video_count, Some(9));
        assert_eq!(
            channel.thumbnails[0].url.as_str(),
            "https://invidious.example.test/ggpht/channel.jpg"
        );
    }

    #[test]
    fn selected_channel_statistics_use_documented_endpoint_and_fixture() {
        let provider = provider();
        assert_eq!(
            provider.channel_statistics_mode(),
            ChannelStatisticsMode::SelectedOnly
        );
        let url = provider
            .build_channel_url("UC_x5XG1OV2P6uZZ5FSM9Ttw")
            .expect("fixture channel should produce a URL");
        assert_eq!(
            url.as_str(),
            "https://invidious.example.test/prefix/api/v1/channels/UC_x5XG1OV2P6uZZ5FSM9Ttw"
        );

        let raw =
            serde_json::from_str(CHANNEL_DETAILS_FIXTURE).expect("fixture should deserialize");
        let statistics =
            InvidiousProvider::convert_channel_subscriber_count(raw, "UC_x5XG1OV2P6uZZ5FSM9Ttw")
                .expect("fixture should map to subscriber statistics");
        assert_eq!(
            statistics,
            ChannelSubscriberCount {
                channel_id: "UC_x5XG1OV2P6uZZ5FSM9Ttw".to_owned(),
                subscriber_count: Some(12_345),
            }
        );
    }

    #[test]
    fn selected_channel_statistics_reject_fanout_and_mismatched_responses() {
        let provider = provider();
        assert!(
            provider
                .channel_subscriber_counts(&[])
                .expect("empty lookups should avoid network access")
                .is_empty()
        );
        assert!(matches!(
            provider.channel_subscriber_counts(&[
                "UC_x5XG1OV2P6uZZ5FSM9Ttw".to_owned(),
                "UCaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            ]),
            Err(ProviderError::InvalidRequest(message)) if message.contains("one identifier")
        ));

        let raw =
            serde_json::from_str(CHANNEL_DETAILS_FIXTURE).expect("fixture should deserialize");
        assert!(matches!(
            InvidiousProvider::convert_channel_subscriber_count(
                raw,
                "UCaaaaaaaaaaaaaaaaaaaaaa",
            ),
            Err(ProviderError::InvalidResponse(message)) if message.contains("does not match")
        ));
    }

    #[test]
    fn parses_full_video_details_and_optional_license() {
        let provider = provider();
        let raw = serde_json::from_str(DETAILS_FIXTURE).expect("fixture should parse");
        let details = provider
            .convert_video_details(raw)
            .expect("fixture should convert");

        assert_eq!(details.like_count, Some(50));
        assert_eq!(details.published_at, Some(1_700_000_000));
        assert_eq!(details.rating, Some(4.75));
        assert_eq!(
            details.license.as_deref(),
            Some("Creative Commons Attribution licence")
        );
        assert_eq!(details.keywords, ["music", "example"]);
    }

    #[test]
    fn malformed_expected_result_reports_its_index() {
        let values: Vec<Value> =
            serde_json::from_str(r#"[{"type":"video","title":"Missing required identifiers"}]"#)
                .expect("fixture should be JSON");
        let error = provider()
            .parse_search_values(values, &SearchRequest::new("broken", SearchTarget::Videos))
            .expect_err("malformed expected item should fail");

        assert!(matches!(
            error,
            ProviderError::InvalidResponse(message) if message.contains("result 0")
        ));
    }

    #[test]
    fn video_endpoint_rejects_path_injection() {
        let error = provider()
            .build_video_url("../api/stats")
            .expect_err("non-video identifier should fail");
        assert!(matches!(error, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn capabilities_describe_separate_channel_search() {
        let capabilities = provider().capabilities();
        assert!(capabilities.video_search);
        assert!(capabilities.channel_search);
        assert!(capabilities.video_details);
        assert!(capabilities.pagination);
    }
}
