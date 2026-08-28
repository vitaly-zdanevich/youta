//! Official `YouTube` Data API v3 search and metadata provider.
//!
//! Search uses `search.list`, then enriches the ordered identifiers with one
//! batched `videos.list` or `channels.list` request. Channel uploads use the
//! documented uploads playlist: one cached `channels.list` lookup followed by
//! `playlistItems.list` at its 50-item maximum and a batched `videos.list`.
//! Search retains its smaller 25-item pages. `YouTube` exposes opaque page
//! tokens rather than page numbers, so this adapter keeps small, bounded token
//! caches. Callers must request pages sequentially, starting with page one.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;
use url::Url;

use super::{
    ChannelDetails, ChannelStatisticsMode, ChannelSubscriberCount, ChannelSummary,
    ChannelVideosRequest, DEFAULT_MAX_JSON_BYTES, DEFAULT_REQUEST_TIMEOUT,
    MAX_VIDEO_COMMENT_ID_BYTES, MAX_VIDEO_COMMENTS, Provider, ProviderCapabilities, ProviderError,
    SearchDate, SearchDuration, SearchFeature, SearchItem, SearchPage, SearchRequest, SearchSort,
    SearchTarget, Thumbnail, VideoComment, VideoDetails, VideoOrientation, VideoSummary,
    normalize_video_comment_text, parse_rfc3339_epoch, provider_agent, validate_base_url,
    validate_video_comment_author, validate_youtube_video_id,
};

const API_BASE_URL: &str = "https://www.googleapis.com/youtube/v3/";
const MAX_CONFIGURED_JSON_BYTES: usize = 64 * 1024 * 1024;
/// Search pages retain the existing result count used by the interactive UI.
const SEARCH_RESULTS_PER_PAGE: u8 = 25;
/// `playlistItems.list` and ID-filtered `videos.list` accept at most 50 rows.
const CHANNEL_UPLOADS_RESULTS_PER_PAGE: u8 = 50;
/// Bounds the comma-separated `id` filter accepted by `videos.list`.
const MAX_VIDEO_RESOURCE_IDS: usize = CHANNEL_UPLOADS_RESULTS_PER_PAGE as usize;
/// Limits the cold channel lookup to its uploads-playlist identifier.
const CHANNEL_UPLOADS_FIELDS: &str = "items(id,contentDetails/relatedPlaylists/uploads)";
/// Limits an uploads-playlist response to pagination data consumed here.
const PLAYLIST_ITEMS_FIELDS: &str = "nextPageToken,items(contentDetails/videoId)";
/// Limits video enrichment to the fields retained by search and upload rows.
const VIDEO_SUMMARY_FIELDS: &str = "items(id,snippet(publishedAt,channelId,title,description,channelTitle,liveBroadcastContent,thumbnails),contentDetails/duration,statistics/viewCount,player(embedWidth,embedHeight))";
/// Retains every field projected into an explicitly requested video Details view.
const VIDEO_DETAILS_FIELDS: &str = "items(id,snippet(publishedAt,channelId,title,description,channelTitle,tags,liveBroadcastContent,thumbnails),contentDetails/duration,statistics(viewCount,likeCount,commentCount),status/license,player(embedWidth,embedHeight))";
const MAX_API_KEY_BYTES: usize = 256;
const MIN_API_KEY_BYTES: usize = 16;
const MAX_PAGE_TOKEN_BYTES: usize = 2 * 1024;
const MAX_CACHED_SEARCHES: usize = 32;
const MAX_TOKENS_PER_SEARCH: usize = 32;
const MAX_CACHED_CHANNELS: usize = 32;
const MAX_TOKENS_PER_CHANNEL: usize = 32;
const MAX_SERVICE_REASON_CHARS: usize = 96;
const MAX_SERVICE_MESSAGE_CHARS: usize = 512;
const MAX_CHANNEL_STATISTICS_IDS: usize = 50;
/// Equal player bounds let the API preserve landscape and portrait ratios.
const PLAYER_EMBED_BOUND: &str = "1920";

/// Selects the smallest `videos.list` partial response needed by a caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VideoResourceProjection {
    /// Metadata retained by search results and channel-upload rows.
    Summary,
    /// Metadata rendered after the user explicitly opens video Details.
    FullDetails,
}

impl VideoResourceProjection {
    fn parts(self) -> &'static str {
        match self {
            Self::Summary => "snippet,contentDetails,statistics,player",
            Self::FullDetails => "snippet,contentDetails,statistics,status,player",
        }
    }

    fn fields(self) -> &'static str {
        match self {
            Self::Summary => VIDEO_SUMMARY_FIELDS,
            Self::FullDetails => VIDEO_DETAILS_FIELDS,
        }
    }
}

/// Blocking provider backed by the official `YouTube` Data API v3.
///
/// Clones share the HTTP connection pool and opaque pagination-token cache.
/// The API key is deliberately absent from `Debug` output and all returned
/// errors.
#[derive(Clone)]
pub struct YouTubeOfficialProvider {
    base_url: Url,
    api_key: Arc<ApiKey>,
    agent: ureq::Agent,
    max_json_bytes: usize,
    page_tokens: Arc<Mutex<PageTokenCache>>,
    channel_page_tokens: Arc<Mutex<ChannelPageTokenCache>>,
}

impl YouTubeOfficialProvider {
    /// Creates a provider using the public `YouTube` Data API v3 endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] when the key is empty, too
    /// long, too short, or contains characters outside the URL-safe API-key
    /// alphabet.
    pub fn new(api_key: impl Into<String>) -> Result<Self, ProviderError> {
        Self::with_options(api_key, DEFAULT_REQUEST_TIMEOUT, DEFAULT_MAX_JSON_BYTES)
    }

    /// Creates a provider with explicit request timeout and response limit.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] when the key, timeout, or
    /// response limit is invalid.
    pub fn with_options(
        api_key: impl Into<String>,
        timeout: Duration,
        max_json_bytes: usize,
    ) -> Result<Self, ProviderError> {
        let base_url = Url::parse(API_BASE_URL)
            .map_err(|_| ProviderError::InvalidBaseUrl("invalid built-in endpoint".to_owned()))?;
        Self::with_base_url(api_key, base_url, timeout, max_json_bytes)
    }

    fn with_base_url(
        api_key: impl Into<String>,
        base_url: Url,
        timeout: Duration,
        max_json_bytes: usize,
    ) -> Result<Self, ProviderError> {
        let api_key = ApiKey::new(api_key.into())?;
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
            api_key: Arc::new(api_key),
            agent: provider_agent(timeout),
            max_json_bytes,
            page_tokens: Arc::new(Mutex::new(PageTokenCache::default())),
            channel_page_tokens: Arc::new(Mutex::new(ChannelPageTokenCache::default())),
        })
    }

    fn build_search_url(
        &self,
        request: &SearchRequest,
        now_epoch: i64,
    ) -> Result<(Url, SearchKey, Option<String>), ProviderError> {
        request.validate()?;
        validate_official_filters(request)?;
        let key = SearchKey::from_request(request);
        let (page_token, published_after) =
            self.page_context(&key, request.page, request.filters.date, now_epoch)?;
        let mut url = self.endpoint("search")?;

        {
            let mut query = url.query_pairs_mut();
            query.append_pair("part", "snippet");
            query.append_pair("q", request.query.trim());
            query.append_pair("maxResults", &SEARCH_RESULTS_PER_PAGE.to_string());
            query.append_pair(
                "type",
                match request.target {
                    SearchTarget::Videos => "video",
                    SearchTarget::Channels => "channel",
                },
            );
            query.append_pair(
                "order",
                match request.sort {
                    SearchSort::Relevance => "relevance",
                    SearchSort::Views => "viewCount",
                    SearchSort::UploadDate => "date",
                },
            );
            if let Some(token) = page_token.as_deref() {
                query.append_pair("pageToken", token);
            }
            if let Some(region) = &request.filters.region {
                query.append_pair("regionCode", &region.to_ascii_uppercase());
            }
            if let Some(published_after) = published_after.as_deref() {
                query.append_pair("publishedAfter", published_after);
            }
            if let Some(duration) = request.filters.duration {
                query.append_pair("videoDuration", search_duration_value(duration));
            }
            for feature in canonical_features(&request.filters.features) {
                let (name, value) = official_feature_pair(feature)?;
                query.append_pair(name, value);
            }
        }
        Ok((url, key, published_after))
    }

    fn endpoint(&self, path: &str) -> Result<Url, ProviderError> {
        self.base_url
            .join(path)
            .map_err(|_| ProviderError::InvalidBaseUrl("invalid API endpoint path".to_owned()))
    }

    fn authenticated_url(&self, url: &Url) -> Url {
        let mut url = url.clone();
        url.query_pairs_mut()
            .append_pair("key", self.api_key.expose());
        url
    }

    fn page_context(
        &self,
        key: &SearchKey,
        page: u32,
        date: Option<SearchDate>,
        now_epoch: i64,
    ) -> Result<(Option<String>, Option<String>), ProviderError> {
        if page == 1 {
            return Ok((
                None,
                date.map(|date| published_after(date, now_epoch))
                    .transpose()?,
            ));
        }
        let cache = self.lock_page_tokens()?;
        cache
            .cursor(key, page)
            .cloned()
            .map(|cursor| (Some(cursor.token), cursor.published_after))
            .ok_or_else(|| {
                ProviderError::InvalidRequest(format!(
                    "YouTube API pages must be loaded sequentially; load page {} first",
                    page.saturating_sub(1)
                ))
            })
    }

    fn remember_next_page(
        &self,
        key: &SearchKey,
        page: u32,
        next_page_token: Option<String>,
        published_after: Option<String>,
    ) -> Result<Option<u32>, ProviderError> {
        let next_page_token = next_page_token
            .map(validate_page_token)
            .transpose()?
            .flatten();
        let next_page = next_page_token
            .as_ref()
            .filter(|_| page < 10_000)
            .map(|_| page.saturating_add(1));
        self.lock_page_tokens()?.remember(
            key,
            page,
            next_page_token.map(|token| PageCursor {
                token,
                published_after,
            }),
        );
        Ok(next_page)
    }

    fn lock_page_tokens(&self) -> Result<MutexGuard<'_, PageTokenCache>, ProviderError> {
        self.page_tokens.lock().map_err(|_| {
            ProviderError::Transport("YouTube pagination state lock was poisoned".to_owned())
        })
    }

    fn lock_channel_page_tokens(
        &self,
    ) -> Result<MutexGuard<'_, ChannelPageTokenCache>, ProviderError> {
        self.channel_page_tokens.lock().map_err(|_| {
            ProviderError::Transport(
                "YouTube channel pagination state lock was poisoned".to_owned(),
            )
        })
    }

    fn request_json<T: serde::de::DeserializeOwned>(&self, url: &Url) -> Result<T, ProviderError> {
        let authenticated = self.authenticated_url(url);
        let mut response = self
            .agent
            .get(authenticated.as_str())
            .header("Accept", "application/json")
            .config()
            .http_status_as_error(false)
            .build()
            .call()
            .map_err(|error| self.transport_error(&error))?;
        let status = response.status().as_u16();

        if response
            .body()
            .content_length()
            .is_some_and(|length| length > self.max_json_bytes as u64)
        {
            return Err(ProviderError::ResponseTooLarge {
                limit: self.max_json_bytes,
            });
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit(u64::try_from(self.max_json_bytes.saturating_add(1)).unwrap_or(u64::MAX))
            .read_to_vec()
            .map_err(|error| match error {
                ureq::Error::BodyExceedsLimit(_) => ProviderError::ResponseTooLarge {
                    limit: self.max_json_bytes,
                },
                other => self.transport_error(&other),
            })?;
        if bytes.len() > self.max_json_bytes {
            return Err(ProviderError::ResponseTooLarge {
                limit: self.max_json_bytes,
            });
        }
        if !(200..300).contains(&status) {
            return Err(self.service_error(status, &bytes));
        }

        serde_json::from_slice(&bytes)
            .map_err(|error| ProviderError::InvalidResponse(error.to_string()))
    }

    fn transport_error(&self, error: &ureq::Error) -> ProviderError {
        match error {
            ureq::Error::BodyExceedsLimit(_) => ProviderError::ResponseTooLarge {
                limit: self.max_json_bytes,
            },
            // Do not forward ureq's text: it may include the authenticated URL.
            _ => ProviderError::Transport("YouTube API request failed".to_owned()),
        }
    }

    fn service_error(&self, status: u16, bytes: &[u8]) -> ProviderError {
        let Ok(envelope) = serde_json::from_slice::<RawErrorEnvelope>(bytes) else {
            return ProviderError::HttpStatus(status);
        };
        let reason = envelope
            .error
            .errors
            .first()
            .and_then(|item| item.reason.as_deref())
            .or(envelope.error.status.as_deref())
            .unwrap_or("unknown");
        let message = envelope
            .error
            .errors
            .first()
            .and_then(|item| item.message.as_deref())
            .or(Some(envelope.error.message.as_str()))
            .unwrap_or("YouTube API request failed");
        ProviderError::Service {
            status,
            reason: sanitize_reason(reason, self.api_key.expose()),
            message: sanitize_service_text(
                message,
                self.api_key.expose(),
                MAX_SERVICE_MESSAGE_CHARS,
            ),
        }
    }

    /// Enriches at most one official 50-ID batch with a partial response.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] when the caller exceeds the
    /// documented ID batch limit, and [`ProviderError::InvalidResponse`] when
    /// the service returns more resources than requested.
    fn fetch_video_resources(
        &self,
        ids: &[String],
        projection: VideoResourceProjection,
    ) -> Result<HashMap<String, RawVideoResource>, ProviderError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        if ids.len() > MAX_VIDEO_RESOURCE_IDS {
            return Err(ProviderError::InvalidRequest(format!(
                "YouTube video enrichment accepts at most {MAX_VIDEO_RESOURCE_IDS} identifiers"
            )));
        }
        let mut url = self.endpoint("videos")?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("part", projection.parts());
            query.append_pair("id", &ids.join(","));
            query.append_pair("fields", projection.fields());
            // Equal bounds preserve either orientation while requesting the
            // player dimensions needed to classify the encoded video.
            query.append_pair("maxWidth", PLAYER_EMBED_BOUND);
            query.append_pair("maxHeight", PLAYER_EMBED_BOUND);
        }
        let response: RawVideoListResponse = self.request_json(&url)?;
        if response.items.len() > ids.len() {
            return Err(ProviderError::InvalidResponse(format!(
                "YouTube returned more video resources than requested ({} > {})",
                response.items.len(),
                ids.len()
            )));
        }
        response
            .items
            .into_iter()
            .map(|item| {
                validate_response_video_id(&item.id)?;
                Ok((item.id.clone(), item))
            })
            .collect()
    }

    /// Fetches a single bounded page of relevant, public top-level comments.
    fn fetch_video_comments(&self, video_id: &str) -> Result<Vec<VideoComment>, ProviderError> {
        validate_youtube_video_id(video_id)?;
        let mut url = self.endpoint("commentThreads")?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("part", "snippet");
            query.append_pair("videoId", video_id);
            query.append_pair("maxResults", &MAX_VIDEO_COMMENTS.to_string());
            query.append_pair("order", "relevance");
            query.append_pair("textFormat", "plainText");
        }
        let response: RawCommentThreadListResponse = self.request_json(&url)?;
        if response.items.len() > MAX_VIDEO_COMMENTS {
            return Err(ProviderError::InvalidResponse(format!(
                "YouTube returned more than {MAX_VIDEO_COMMENTS} comments"
            )));
        }
        response
            .items
            .into_iter()
            .map(video_comment_from_thread)
            .collect()
    }

    fn fetch_channel_resources(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, RawChannelResource>, ProviderError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut url = self.endpoint("channels")?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("part", "snippet,statistics");
            query.append_pair("id", &ids.join(","));
            query.append_pair("maxResults", &ids.len().to_string());
        }
        let response: RawChannelListResponse = self.request_json(&url)?;
        response
            .items
            .into_iter()
            .map(|item| {
                validate_channel_id(&item.id, "channel resource ID")?;
                Ok((item.id.clone(), item))
            })
            .collect()
    }

    fn fetch_exact_channel_resource(
        &self,
        channel_id: &str,
    ) -> Result<RawChannelResource, ProviderError> {
        validate_channel_id(channel_id, "requested channel ID").map_err(|_| {
            ProviderError::InvalidRequest(
                "YouTube channel ID contains invalid characters".to_owned(),
            )
        })?;
        let resources = self.fetch_channel_resources(&[channel_id.to_owned()])?;
        let mut resources = resources.into_iter();
        let Some((returned_id, resource)) = resources.next() else {
            return Err(ProviderError::InvalidResponse(
                "YouTube channel was not found".to_owned(),
            ));
        };
        if returned_id != channel_id || resources.next().is_some() {
            return Err(ProviderError::InvalidResponse(
                "channel response identifier does not match the requested channel".to_owned(),
            ));
        }
        Ok(resource)
    }

    /// Resolves and caches the system uploads playlist advertised by a
    /// channel resource.
    ///
    /// This one-unit `channels.list` lookup replaces the substantially more
    /// expensive `search.list` route for channel uploads. Subsequent pages can
    /// reuse the playlist identifier.
    fn uploads_playlist_id(&self, channel_id: &str) -> Result<String, ProviderError> {
        if let Some(playlist_id) = self
            .lock_channel_page_tokens()?
            .uploads_playlist_id(channel_id)
        {
            return Ok(playlist_id);
        }

        let mut url = self.endpoint("channels")?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("part", "contentDetails");
            query.append_pair("id", channel_id);
            query.append_pair("maxResults", "1");
            query.append_pair("fields", CHANNEL_UPLOADS_FIELDS);
        }
        let response: RawChannelUploadsListResponse = self.request_json(&url)?;
        let mut items = response.items.into_iter();
        let resource = items.next().ok_or_else(|| {
            ProviderError::InvalidResponse(
                "YouTube channel was not found or has no uploads playlist".to_owned(),
            )
        })?;
        validate_channel_id(&resource.id, "channel uploads resource ID")?;
        if resource.id != channel_id {
            return Err(ProviderError::InvalidResponse(
                "channel uploads response identifier does not match the requested channel"
                    .to_owned(),
            ));
        }
        let playlist_id = resource.content_details.related_playlists.uploads;
        validate_playlist_id(&playlist_id)?;
        self.lock_channel_page_tokens()?
            .remember_uploads_playlist(channel_id, &playlist_id);
        Ok(playlist_id)
    }

    /// Returns the uploads playlist and opaque token needed for one numbered
    /// channel page.
    fn channel_page_context(
        &self,
        request: &ChannelVideosRequest,
    ) -> Result<(String, Option<String>), ProviderError> {
        request.validate()?;
        validate_channel_id(&request.channel_id, "requested channel ID").map_err(|_| {
            ProviderError::InvalidRequest(
                "YouTube channel ID contains invalid characters".to_owned(),
            )
        })?;
        if request.page == 1 {
            return self
                .uploads_playlist_id(&request.channel_id)
                .map(|playlist_id| (playlist_id, None));
        }
        self.lock_channel_page_tokens()?
            .page_context(&request.channel_id, request.page)
            .map(|(playlist_id, token)| (playlist_id, Some(token)))
            .ok_or_else(|| {
                ProviderError::InvalidRequest(format!(
                    "YouTube channel pages must be loaded sequentially; load page {} first",
                    request.page.saturating_sub(1)
                ))
            })
    }

    /// Fetches one uploads-playlist page and enriches its ordered video IDs in
    /// one batched `videos.list` request.
    fn channel_videos_page(
        &self,
        request: &ChannelVideosRequest,
    ) -> Result<SearchPage, ProviderError> {
        let (playlist_id, page_token) = self.channel_page_context(request)?;
        let mut url = self.endpoint("playlistItems")?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("part", "contentDetails");
            query.append_pair("playlistId", &playlist_id);
            query.append_pair("maxResults", &CHANNEL_UPLOADS_RESULTS_PER_PAGE.to_string());
            query.append_pair("fields", PLAYLIST_ITEMS_FIELDS);
            if let Some(token) = page_token.as_deref() {
                query.append_pair("pageToken", token);
            }
        }
        let response: RawPlaylistItemsResponse = self.request_json(&url)?;
        if response.items.len() > usize::from(CHANNEL_UPLOADS_RESULTS_PER_PAGE) {
            return Err(ProviderError::InvalidResponse(format!(
                "YouTube returned more than {CHANNEL_UPLOADS_RESULTS_PER_PAGE} channel uploads"
            )));
        }
        let next_token = response
            .next_page_token
            .map(validate_page_token)
            .transpose()?
            .flatten();
        let next_page = next_token
            .as_ref()
            .filter(|_| request.page < 10_000)
            .map(|_| request.page.saturating_add(1));
        let mut video_ids = Vec::with_capacity(response.items.len());
        for item in response.items {
            validate_response_video_id(&item.content_details.video_id)?;
            video_ids.push(item.content_details.video_id);
        }
        let mut resources =
            self.fetch_video_resources(&video_ids, VideoResourceProjection::Summary)?;
        let items = video_ids
            .into_iter()
            .filter_map(|video_id| resources.remove(&video_id))
            .map(video_summary_from_resource)
            .map(|result| result.map(SearchItem::Video))
            .collect::<Result<Vec<_>, _>>()?;
        // Publish the opaque continuation only after the complete page has
        // validated. A failed refresh must leave the last usable chain intact.
        self.lock_channel_page_tokens()?.remember_next_page(
            &request.channel_id,
            request.page,
            next_token,
        );
        Ok(SearchPage {
            page: request.page,
            items,
            next_page,
        })
    }

    fn fetch_channel_subscriber_counts(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, ChannelSubscriberCount>, ProviderError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        if ids.len() > MAX_CHANNEL_STATISTICS_IDS {
            return Err(ProviderError::InvalidRequest(format!(
                "YouTube channel statistics accepts at most {MAX_CHANNEL_STATISTICS_IDS} identifiers"
            )));
        }
        for channel_id in ids {
            validate_channel_id(channel_id, "requested channel ID").map_err(|_| {
                ProviderError::InvalidRequest(
                    "YouTube channel ID contains invalid characters".to_owned(),
                )
            })?;
        }

        let mut url = self.endpoint("channels")?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("part", "snippet,statistics");
            query.append_pair("id", &ids.join(","));
            query.append_pair("maxResults", &ids.len().to_string());
        }
        let response: RawChannelStatisticsListResponse = self.request_json(&url)?;
        response
            .items
            .into_iter()
            .map(|item| {
                validate_channel_id(&item.id, "channel statistics resource ID")?;
                let count = (!item.statistics.hidden_subscriber_count)
                    .then_some(item.statistics.subscriber_count)
                    .flatten();
                let webpage_url = item
                    .snippet
                    .and_then(|snippet| snippet.custom_url)
                    .as_deref()
                    .and_then(youtube_custom_channel_url);
                let channel_id = item.id;
                Ok((
                    channel_id.clone(),
                    ChannelSubscriberCount {
                        channel_id,
                        subscriber_count: count,
                        webpage_url,
                    },
                ))
            })
            .collect()
    }

    fn search_at(
        &self,
        request: &SearchRequest,
        now_epoch: i64,
    ) -> Result<SearchPage, ProviderError> {
        let (url, key, published_after) = self.build_search_url(request, now_epoch)?;
        let response: RawSearchResponse = self.request_json(&url)?;
        let items = match request.target {
            SearchTarget::Videos => self.enrich_video_search(response.items)?,
            SearchTarget::Channels => self.enrich_channel_search(response.items)?,
        };
        let next_page = self.remember_next_page(
            &key,
            request.page,
            response.next_page_token,
            published_after,
        )?;
        Ok(SearchPage {
            page: request.page,
            items,
            next_page,
        })
    }

    /// Enriches the usable rows from one bounded video-only search page.
    ///
    /// Although `search.list?type=video` documents `id.videoId` for every
    /// result, an isolated non-video or malformed row is omitted when another
    /// valid video survives. A genuinely empty upstream page remains valid,
    /// while a nonempty page containing no usable videos is rejected.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidResponse`] when the upstream page
    /// exceeds the requested result count, contains no usable video rows, or
    /// its batch enrichment response is malformed.
    fn enrich_video_search(
        &self,
        raw_items: Vec<RawSearchItem>,
    ) -> Result<Vec<SearchItem>, ProviderError> {
        if raw_items.len() > usize::from(SEARCH_RESULTS_PER_PAGE) {
            return Err(ProviderError::InvalidResponse(format!(
                "YouTube returned more than {SEARCH_RESULTS_PER_PAGE} video search results"
            )));
        }
        let raw_item_count = raw_items.len();
        let mut ordered = Vec::with_capacity(raw_items.len());
        let mut first_rejection = None;
        for (index, raw) in raw_items.into_iter().enumerate() {
            let candidate = (|| {
                let video_id = raw.id.video_id.ok_or_else(|| {
                    ProviderError::InvalidResponse(format!(
                        "video search result {index} omitted its video ID"
                    ))
                })?;
                validate_response_video_id(&video_id)?;
                validate_search_snippet(&raw.snippet, index)?;
                Ok((video_id, raw.snippet))
            })();
            match candidate {
                Ok(candidate) => ordered.push(candidate),
                Err(error) => {
                    first_rejection.get_or_insert(error);
                }
            }
        }
        if ordered.is_empty() && raw_item_count > 0 {
            return Err(first_rejection.unwrap_or_else(|| {
                ProviderError::InvalidResponse(
                    "YouTube returned no usable video search results".to_owned(),
                )
            }));
        }
        let ids = ordered.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>();
        let mut resources = self.fetch_video_resources(&ids, VideoResourceProjection::Summary)?;
        ordered
            .into_iter()
            .map(|(id, fallback)| {
                let summary = resources.remove(&id).map_or_else(
                    || video_summary_from_search(id, fallback),
                    video_summary_from_resource,
                )?;
                Ok(SearchItem::Video(summary))
            })
            .collect()
    }

    fn enrich_channel_search(
        &self,
        raw_items: Vec<RawSearchItem>,
    ) -> Result<Vec<SearchItem>, ProviderError> {
        let mut ordered = Vec::with_capacity(raw_items.len());
        for (index, raw) in raw_items.into_iter().enumerate() {
            let channel_id = raw.id.channel_id.ok_or_else(|| {
                ProviderError::InvalidResponse(format!(
                    "channel search result {index} omitted its channel ID"
                ))
            })?;
            validate_channel_id(&channel_id, "channel search result ID")?;
            validate_search_snippet(&raw.snippet, index)?;
            ordered.push((channel_id, raw.snippet));
        }
        let ids = ordered.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>();
        let mut resources = self.fetch_channel_resources(&ids)?;
        ordered
            .into_iter()
            .map(|(id, fallback)| {
                let summary = resources.remove(&id).map_or_else(
                    || Ok(channel_summary_from_search(id, fallback)),
                    channel_summary_from_resource,
                )?;
                Ok(SearchItem::Channel(summary))
            })
            .collect()
    }
}

impl fmt::Debug for YouTubeOfficialProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("YouTubeOfficialProvider")
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .field("max_json_bytes", &self.max_json_bytes)
            .field("page_tokens", &"[OPAQUE]")
            .field("channel_page_tokens", &"[OPAQUE]")
            .finish_non_exhaustive()
    }
}

impl Provider for YouTubeOfficialProvider {
    fn id(&self) -> &'static str {
        "youtube-official"
    }

    fn display_name(&self) -> &'static str {
        "YouTube Data API"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            video_search: true,
            channel_search: true,
            pagination: true,
            search_filters: true,
            search_sorting: true,
            video_details: true,
            video_comments: true,
            thumbnails: true,
        }
    }

    fn channel_statistics_mode(&self) -> ChannelStatisticsMode {
        ChannelStatisticsMode::Batch {
            max_ids: MAX_CHANNEL_STATISTICS_IDS,
        }
    }

    fn search(&self, request: &SearchRequest) -> Result<SearchPage, ProviderError> {
        let now_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ProviderError::Transport("system time is before Unix epoch".to_owned()))?
            .as_secs();
        let now_epoch = i64::try_from(now_epoch)
            .map_err(|_| ProviderError::Transport("system time is out of range".to_owned()))?;
        self.search_at(request, now_epoch)
    }

    fn channel_videos(&self, request: &ChannelVideosRequest) -> Result<SearchPage, ProviderError> {
        self.channel_videos_page(request)
    }

    fn channel_details(&self, channel_id: &str) -> Result<ChannelSummary, ProviderError> {
        channel_summary_from_resource(self.fetch_exact_channel_resource(channel_id)?)
    }

    fn full_channel_details(&self, channel_id: &str) -> Result<ChannelDetails, ProviderError> {
        full_channel_details_from_resource(self.fetch_exact_channel_resource(channel_id)?)
    }

    fn video_details(&self, video_id: &str) -> Result<VideoDetails, ProviderError> {
        validate_youtube_video_id(video_id)?;
        let mut resources = self
            .fetch_video_resources(&[video_id.to_owned()], VideoResourceProjection::FullDetails)?;
        let resource = resources.remove(video_id).ok_or_else(|| {
            ProviderError::InvalidResponse("YouTube video was not found or is private".to_owned())
        })?;
        video_details_from_resource(resource)
    }

    fn video_comments(&self, video_id: &str) -> Result<Vec<VideoComment>, ProviderError> {
        self.fetch_video_comments(video_id)
    }

    fn channel_subscriber_counts(
        &self,
        channel_ids: &[String],
    ) -> Result<Vec<ChannelSubscriberCount>, ProviderError> {
        let counts = self.fetch_channel_subscriber_counts(channel_ids)?;
        Ok(channel_ids
            .iter()
            .map(|channel_id| {
                counts
                    .get(channel_id)
                    .cloned()
                    .unwrap_or_else(|| ChannelSubscriberCount {
                        channel_id: channel_id.clone(),
                        subscriber_count: None,
                        webpage_url: None,
                    })
            })
            .collect())
    }
}

struct ApiKey(String);

impl ApiKey {
    fn new(value: String) -> Result<Self, ProviderError> {
        if !(MIN_API_KEY_BYTES..=MAX_API_KEY_BYTES).contains(&value.len())
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(ProviderError::InvalidRequest(format!(
                "YouTube API key must contain {MIN_API_KEY_BYTES} to {MAX_API_KEY_BYTES} URL-safe ASCII characters"
            )));
        }
        Ok(Self(value))
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SearchKey {
    query: String,
    target: u8,
    sort: u8,
    date: Option<u8>,
    duration: Option<u8>,
    features: Vec<u8>,
    region: Option<String>,
}

impl SearchKey {
    fn from_request(request: &SearchRequest) -> Self {
        Self {
            query: request.query.trim().to_owned(),
            target: match request.target {
                SearchTarget::Videos => 0,
                SearchTarget::Channels => 1,
            },
            sort: match request.sort {
                SearchSort::Relevance => 0,
                SearchSort::Views => 1,
                SearchSort::UploadDate => 2,
            },
            date: request.filters.date.map(search_date_key),
            duration: request.filters.duration.map(search_duration_key),
            features: canonical_features(&request.filters.features)
                .into_iter()
                .map(search_feature_key)
                .collect(),
            region: request
                .filters
                .region
                .as_deref()
                .map(str::to_ascii_uppercase),
        }
    }
}

#[derive(Default)]
struct PageTokenCache {
    searches: HashMap<SearchKey, BTreeMap<u32, PageCursor>>,
    order: VecDeque<SearchKey>,
}

impl PageTokenCache {
    fn cursor(&self, key: &SearchKey, page: u32) -> Option<&PageCursor> {
        self.searches.get(key).and_then(|pages| pages.get(&page))
    }

    fn remember(&mut self, key: &SearchKey, page: u32, next_cursor: Option<PageCursor>) {
        if page == 1 {
            self.searches.remove(key);
            self.order.retain(|cached| cached != key);
        }
        if !self.searches.contains_key(key) {
            while self.searches.len() >= MAX_CACHED_SEARCHES {
                if let Some(oldest) = self.order.pop_front() {
                    self.searches.remove(&oldest);
                } else {
                    break;
                }
            }
            self.order.push_back(key.clone());
            self.searches.insert(key.clone(), BTreeMap::new());
        }
        let pages = self.searches.get_mut(key).expect("key was inserted");
        if let Some(cursor) = next_cursor {
            pages.insert(page.saturating_add(1), cursor);
        } else {
            pages.remove(&page.saturating_add(1));
        }
        while pages.len() > MAX_TOKENS_PER_SEARCH {
            let Some(oldest) = pages.keys().next().copied() else {
                break;
            };
            pages.remove(&oldest);
        }
    }
}

#[derive(Clone)]
struct PageCursor {
    token: String,
    published_after: Option<String>,
}

/// Bounded uploads-playlist and continuation state keyed by channel ID.
#[derive(Default)]
struct ChannelPageTokenCache {
    channels: HashMap<String, CachedChannelPages>,
    order: VecDeque<String>,
}

impl ChannelPageTokenCache {
    fn uploads_playlist_id(&mut self, channel_id: &str) -> Option<String> {
        let playlist_id = self
            .channels
            .get(channel_id)
            .map(|channel| channel.uploads_playlist_id.clone())?;
        self.touch_channel(channel_id);
        Some(playlist_id)
    }

    fn page_context(&mut self, channel_id: &str, page: u32) -> Option<(String, String)> {
        let channel = self.channels.get(channel_id)?;
        let playlist_id = channel.uploads_playlist_id.clone();
        let token = channel.page_tokens.get(&page)?.clone();
        self.touch_channel(channel_id);
        Some((playlist_id, token))
    }

    fn remember_uploads_playlist(&mut self, channel_id: &str, playlist_id: &str) {
        if let Some(channel) = self.channels.get_mut(channel_id) {
            if channel.uploads_playlist_id != playlist_id {
                playlist_id.clone_into(&mut channel.uploads_playlist_id);
                channel.page_tokens.clear();
            }
            self.touch_channel(channel_id);
            return;
        }
        while self.channels.len() >= MAX_CACHED_CHANNELS {
            if let Some(oldest) = self.order.pop_front() {
                self.channels.remove(&oldest);
            } else {
                break;
            }
        }
        self.order.push_back(channel_id.to_owned());
        self.channels.insert(
            channel_id.to_owned(),
            CachedChannelPages {
                uploads_playlist_id: playlist_id.to_owned(),
                page_tokens: BTreeMap::new(),
            },
        );
    }

    fn remember_next_page(&mut self, channel_id: &str, page: u32, next_token: Option<String>) {
        let Some(channel) = self.channels.get_mut(channel_id) else {
            return;
        };
        // Opaque descendants belong to the earlier response chain. Replaying
        // any page replaces that chain from the following page onward.
        channel
            .page_tokens
            .retain(|cached_page, _| *cached_page <= page);
        if let Some(token) = next_token {
            channel.page_tokens.insert(page.saturating_add(1), token);
        } else {
            channel.page_tokens.remove(&page.saturating_add(1));
        }
        while channel.page_tokens.len() > MAX_TOKENS_PER_CHANNEL {
            let Some(oldest) = channel.page_tokens.keys().next().copied() else {
                break;
            };
            channel.page_tokens.remove(&oldest);
        }
        self.touch_channel(channel_id);
    }

    /// Marks one cached channel as the most recently used owner of its tokens.
    fn touch_channel(&mut self, channel_id: &str) {
        self.order.retain(|cached| cached != channel_id);
        if self.channels.contains_key(channel_id) {
            self.order.push_back(channel_id.to_owned());
        }
    }
}

struct CachedChannelPages {
    uploads_playlist_id: String,
    page_tokens: BTreeMap<u32, String>,
}

fn validate_official_filters(request: &SearchRequest) -> Result<(), ProviderError> {
    if request.target == SearchTarget::Channels
        && (request.filters.duration.is_some() || !request.filters.features.is_empty())
    {
        return Err(ProviderError::InvalidRequest(
            "YouTube video duration and feature filters cannot be used for channel search"
                .to_owned(),
        ));
    }
    for feature in canonical_features(&request.filters.features) {
        official_feature_pair(feature)?;
    }
    Ok(())
}

fn canonical_features(features: &[SearchFeature]) -> Vec<SearchFeature> {
    let mut features = features.to_vec();
    features.sort_unstable_by_key(|feature| search_feature_key(*feature));
    features.dedup();
    features
}

const fn official_feature_pair(
    feature: SearchFeature,
) -> Result<(&'static str, &'static str), ProviderError> {
    match feature {
        SearchFeature::Hd => Ok(("videoDefinition", "high")),
        SearchFeature::Subtitles => Ok(("videoCaption", "closedCaption")),
        SearchFeature::CreativeCommons => Ok(("videoLicense", "creativeCommon")),
        SearchFeature::ThreeD => Ok(("videoDimension", "3d")),
        SearchFeature::Live => Ok(("eventType", "live")),
        SearchFeature::Purchased
        | SearchFeature::FourK
        | SearchFeature::ThreeSixty
        | SearchFeature::Location
        | SearchFeature::Hdr
        | SearchFeature::Vr180 => Err(ProviderError::Unsupported),
    }
}

const fn search_date_key(date: SearchDate) -> u8 {
    match date {
        SearchDate::Hour => 0,
        SearchDate::Today => 1,
        SearchDate::Week => 2,
        SearchDate::Month => 3,
        SearchDate::Year => 4,
    }
}

const fn search_duration_key(duration: SearchDuration) -> u8 {
    match duration {
        SearchDuration::Short => 0,
        SearchDuration::Medium => 1,
        SearchDuration::Long => 2,
    }
}

const fn search_feature_key(feature: SearchFeature) -> u8 {
    match feature {
        SearchFeature::Hd => 0,
        SearchFeature::Subtitles => 1,
        SearchFeature::CreativeCommons => 2,
        SearchFeature::ThreeD => 3,
        SearchFeature::Live => 4,
        SearchFeature::Purchased => 5,
        SearchFeature::FourK => 6,
        SearchFeature::ThreeSixty => 7,
        SearchFeature::Location => 8,
        SearchFeature::Hdr => 9,
        SearchFeature::Vr180 => 10,
    }
}

const fn search_duration_value(duration: SearchDuration) -> &'static str {
    match duration {
        SearchDuration::Short => "short",
        SearchDuration::Medium => "medium",
        SearchDuration::Long => "long",
    }
}

fn published_after(date: SearchDate, now_epoch: i64) -> Result<String, ProviderError> {
    let age = match date {
        SearchDate::Hour => 60 * 60,
        SearchDate::Today => 24 * 60 * 60,
        SearchDate::Week => 7 * 24 * 60 * 60,
        SearchDate::Month => 30 * 24 * 60 * 60,
        SearchDate::Year => 365 * 24 * 60 * 60,
    };
    let epoch = now_epoch.checked_sub(age).ok_or_else(|| {
        ProviderError::Transport("system time cannot represent the search date".to_owned())
    })?;
    format_rfc3339_utc(epoch)
}

fn format_rfc3339_utc(epoch: i64) -> Result<String, ProviderError> {
    let days = epoch.div_euclid(86_400);
    let seconds = epoch.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    if !(0..=9999).contains(&year) {
        return Err(ProviderError::Transport(
            "system time is outside the YouTube API date range".to_owned(),
        ));
    }
    let hour = seconds / 3_600;
    let minute = (seconds % 3_600) / 60;
    let second = seconds % 60;
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let days = days_since_epoch + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn validate_page_token(token: String) -> Result<Option<String>, ProviderError> {
    if token.is_empty() {
        return Ok(None);
    }
    if token.len() > MAX_PAGE_TOKEN_BYTES
        || token
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(ProviderError::InvalidResponse(
            "YouTube returned an invalid page token".to_owned(),
        ));
    }
    Ok(Some(token))
}

fn validate_playlist_id(playlist_id: &str) -> Result<(), ProviderError> {
    if playlist_id.is_empty()
        || playlist_id.len() > 128
        || !playlist_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ProviderError::InvalidResponse(
            "YouTube response contains an invalid uploads playlist ID".to_owned(),
        ));
    }
    Ok(())
}

fn validate_response_video_id(video_id: &str) -> Result<(), ProviderError> {
    validate_youtube_video_id(video_id).map_err(|_| {
        ProviderError::InvalidResponse("YouTube response contains an invalid video ID".to_owned())
    })
}

fn validate_channel_id(channel_id: &str, field: &str) -> Result<(), ProviderError> {
    if channel_id.is_empty()
        || channel_id.len() > 128
        || !channel_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ProviderError::InvalidResponse(format!(
            "{field} contains invalid characters"
        )));
    }
    Ok(())
}

fn validate_search_snippet(snippet: &RawSnippet, index: usize) -> Result<(), ProviderError> {
    if snippet.title.trim().is_empty() {
        return Err(ProviderError::InvalidResponse(format!(
            "search result {index} has an empty title"
        )));
    }
    Ok(())
}

fn video_summary_from_search(
    video_id: String,
    snippet: RawSnippet,
) -> Result<VideoSummary, ProviderError> {
    validate_channel_id(&snippet.channel_id, "video channel ID")?;
    if snippet.channel_title.trim().is_empty() {
        return Err(ProviderError::InvalidResponse(
            "video channel title cannot be empty".to_owned(),
        ));
    }
    Ok(VideoSummary {
        webpage_url: youtube_video_url(&video_id),
        video_id,
        title: snippet.title,
        channel_name: snippet.channel_title,
        channel_id: snippet.channel_id,
        description: snippet.description,
        duration_seconds: None,
        view_count: None,
        published_at: snippet
            .published_at
            .as_deref()
            .and_then(parse_rfc3339_epoch),
        published_text: None,
        live: snippet.live_broadcast_content.as_deref() == Some("live"),
        orientation: VideoOrientation::Unknown,
        thumbnails: convert_thumbnails(snippet.thumbnails),
        stream_url: None,
    })
}

fn video_summary_from_resource(raw: RawVideoResource) -> Result<VideoSummary, ProviderError> {
    let details = video_details_from_resource(raw)?;
    Ok(VideoSummary {
        video_id: details.video_id,
        title: details.title,
        channel_name: details.channel_name,
        channel_id: details.channel_id,
        description: details.description,
        duration_seconds: details.duration_seconds,
        view_count: details.view_count,
        published_at: details.published_at,
        published_text: details.published_text,
        live: details.live,
        orientation: details.orientation,
        thumbnails: details.thumbnails,
        webpage_url: details.webpage_url,
        stream_url: None,
    })
}

fn channel_summary_from_search(channel_id: String, snippet: RawSnippet) -> ChannelSummary {
    let created_at = snippet
        .published_at
        .as_deref()
        .and_then(parse_rfc3339_epoch);
    ChannelSummary {
        webpage_url: youtube_channel_url_with_custom(&channel_id, snippet.custom_url.as_deref()),
        channel_id,
        name: snippet.title,
        description: snippet.description,
        subscriber_count: None,
        video_count: None,
        created_at,
        auto_generated: false,
        thumbnails: convert_thumbnails(snippet.thumbnails),
    }
}

fn channel_summary_from_resource(raw: RawChannelResource) -> Result<ChannelSummary, ProviderError> {
    validate_channel_id(&raw.id, "channel resource ID")?;
    if raw.snippet.title.trim().is_empty() {
        return Err(ProviderError::InvalidResponse(
            "channel title cannot be empty".to_owned(),
        ));
    }
    let created_at = raw
        .snippet
        .published_at
        .as_deref()
        .and_then(parse_rfc3339_epoch);
    Ok(ChannelSummary {
        webpage_url: youtube_channel_url_with_custom(&raw.id, raw.snippet.custom_url.as_deref()),
        channel_id: raw.id,
        name: raw.snippet.title,
        description: raw.snippet.description,
        subscriber_count: (!raw.statistics.hidden_subscriber_count)
            .then_some(raw.statistics.subscriber_count)
            .flatten(),
        video_count: raw.statistics.video_count,
        created_at,
        auto_generated: false,
        thumbnails: convert_thumbnails(raw.snippet.thumbnails),
    })
}

fn full_channel_details_from_resource(
    raw: RawChannelResource,
) -> Result<ChannelDetails, ProviderError> {
    let total_view_count = raw.statistics.view_count;
    let country = normalize_country(raw.snippet.country.as_deref());
    channel_summary_from_resource(raw).map(|summary| ChannelDetails {
        summary,
        total_view_count,
        country,
        external_links: Vec::new(),
        external_links_truncated: false,
    })
}

fn normalize_country(country: Option<&str>) -> Option<String> {
    country.and_then(|country| {
        let country = country.trim();
        (!country.is_empty() && country.chars().count() <= 128).then(|| country.to_owned())
    })
}

fn video_details_from_resource(raw: RawVideoResource) -> Result<VideoDetails, ProviderError> {
    validate_response_video_id(&raw.id)?;
    validate_channel_id(&raw.snippet.channel_id, "video channel ID")?;
    if raw.snippet.title.trim().is_empty() || raw.snippet.channel_title.trim().is_empty() {
        return Err(ProviderError::InvalidResponse(
            "video title and channel title cannot be empty".to_owned(),
        ));
    }
    let duration_seconds = raw
        .content_details
        .duration
        .as_deref()
        .and_then(parse_iso8601_duration);
    let license = raw.status.license.as_deref().and_then(map_license);
    let orientation = raw.player.orientation();
    Ok(VideoDetails {
        webpage_url: youtube_video_url(&raw.id),
        video_id: raw.id,
        title: raw.snippet.title,
        channel_name: raw.snippet.channel_title,
        channel_id: raw.snippet.channel_id,
        description: raw.snippet.description,
        duration_seconds,
        view_count: raw.statistics.view_count,
        like_count: raw.statistics.like_count,
        comment_count: raw.statistics.comment_count,
        published_at: raw
            .snippet
            .published_at
            .as_deref()
            .and_then(parse_rfc3339_epoch),
        published_text: None,
        license,
        rating: None,
        ratings_allowed: None,
        live: raw.snippet.live_broadcast_content.as_deref() == Some("live"),
        orientation,
        keywords: raw.snippet.tags,
        thumbnails: convert_thumbnails(raw.snippet.thumbnails),
        stream_url: None,
    })
}

fn video_comment_from_thread(raw: RawCommentThread) -> Result<VideoComment, ProviderError> {
    let comment = raw.snippet.top_level_comment;
    validate_comment_id(&comment.id)?;
    let author_name =
        validate_video_comment_author("YouTube", comment.snippet.author_display_name)?;
    let text = normalize_video_comment_text("YouTube", comment.snippet.text_display)?;
    let published_at = parse_comment_timestamp(
        comment.snippet.published_at.as_deref(),
        "publication timestamp",
    )?;
    let updated_at =
        parse_comment_timestamp(comment.snippet.updated_at.as_deref(), "update timestamp")?;
    Ok(VideoComment {
        comment_id: comment.id,
        author_name,
        author_channel_url: comment
            .snippet
            .author_channel_url
            .as_deref()
            .and_then(safe_remote_url),
        text,
        like_count: comment.snippet.like_count.unwrap_or(0),
        published_at,
        updated_at,
    })
}

fn validate_comment_id(comment_id: &str) -> Result<(), ProviderError> {
    if comment_id.is_empty()
        || comment_id.len() > MAX_VIDEO_COMMENT_ID_BYTES
        || comment_id
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(ProviderError::InvalidResponse(
            "YouTube returned an invalid comment ID".to_owned(),
        ));
    }
    Ok(())
}

fn parse_comment_timestamp(value: Option<&str>, field: &str) -> Result<Option<i64>, ProviderError> {
    value
        .map(|value| {
            parse_rfc3339_epoch(value).ok_or_else(|| {
                ProviderError::InvalidResponse(format!(
                    "YouTube returned an invalid comment {field}"
                ))
            })
        })
        .transpose()
}

fn map_license(value: &str) -> Option<String> {
    match value.trim() {
        "creativeCommon" => Some("Creative Commons Attribution".to_owned()),
        "youtube" => Some("Standard YouTube License".to_owned()),
        "" => None,
        other => Some(other.to_owned()),
    }
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

/// Returns the safest public YouTube channel URL exposed by an API response.
///
/// Official channel resources may expose a human-readable `customUrl`. Only
/// channel-path shapes used by YouTube are accepted; malformed values fall
/// back to the stable channel-ID URL.
fn youtube_channel_url_with_custom(channel_id: &str, custom_url: Option<&str>) -> Option<Url> {
    custom_url
        .and_then(youtube_custom_channel_url)
        .or_else(|| youtube_channel_url(channel_id))
}

/// Converts an official `snippet.customUrl` value into a public YouTube URL.
fn youtube_custom_channel_url(custom_url: &str) -> Option<Url> {
    let path = custom_url.trim().trim_start_matches('/');
    if path.is_empty()
        || path.len() > 256
        || path
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        || path.contains(['\\', '?', '#'])
    {
        return None;
    }
    let segments = path.split('/').collect::<Vec<_>>();
    let valid_shape = match segments.as_slice() {
        [handle] => handle
            .strip_prefix('@')
            .is_some_and(valid_youtube_channel_alias),
        ["c" | "user", name] => valid_youtube_channel_alias(name),
        _ => false,
    };
    if !valid_shape {
        return None;
    }

    let mut url = Url::parse("https://www.youtube.com/").ok()?;
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .extend(segments);
    Some(url)
}

/// Checks a human-readable YouTube channel path segment without guessing routes.
///
/// The official API can return Unicode handles and legacy custom names, so the
/// check retains Unicode while excluding URL delimiters, traversal markers,
/// control characters, and unreasonably large response values.
fn valid_youtube_channel_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias.len() <= 128
        && !matches!(alias, "." | "..")
        && !alias.chars().any(|character| {
            character.is_control()
                || character.is_whitespace()
                || matches!(character, '/' | '\\' | '?' | '#' | '%' | '@' | ':')
        })
}

fn convert_thumbnails(raw: BTreeMap<String, RawThumbnail>) -> Vec<Thumbnail> {
    const QUALITY_ORDER: [&str; 5] = ["default", "medium", "high", "standard", "maxres"];
    let mut thumbnails = raw
        .into_iter()
        .filter_map(|(quality, thumbnail)| {
            let url = safe_remote_url(&thumbnail.url)?;
            Some((
                quality_rank(&quality, &QUALITY_ORDER),
                quality.clone(),
                Thumbnail {
                    url,
                    quality: Some(quality),
                    width: thumbnail.width,
                    height: thumbnail.height,
                },
            ))
        })
        .collect::<Vec<_>>();
    thumbnails.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    thumbnails
        .into_iter()
        .map(|(_, _, thumbnail)| thumbnail)
        .collect()
}

fn quality_rank(quality: &str, known: &[&str]) -> usize {
    known
        .iter()
        .position(|candidate| *candidate == quality)
        .unwrap_or(known.len())
}

fn safe_remote_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

fn parse_iso8601_duration(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    if bytes.first() != Some(&b'P') || bytes.len() < 2 {
        return None;
    }
    let mut index = 1;
    let mut in_time = false;
    let mut saw_value = false;
    let mut last_rank = 0_u8;
    let mut total = 0_u64;

    while index < bytes.len() {
        if bytes[index] == b'T' && !in_time {
            in_time = true;
            index += 1;
            if index == bytes.len() {
                return None;
            }
            continue;
        }
        let start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == start {
            return None;
        }
        let number = std::str::from_utf8(&bytes[start..index])
            .ok()?
            .parse::<u64>()
            .ok()?;
        let mut fractional = false;
        if bytes.get(index) == Some(&b'.') {
            fractional = true;
            index += 1;
            let fraction_start = index;
            while bytes.get(index).is_some_and(u8::is_ascii_digit) {
                index += 1;
            }
            if index == fraction_start {
                return None;
            }
        }
        let designator = *bytes.get(index)?;
        index += 1;
        let (rank, multiplier) = match (in_time, designator) {
            (false, b'D') if !fractional => (1, 86_400),
            (true, b'H') if !fractional => (2, 3_600),
            (true, b'M') if !fractional => (3, 60),
            (true, b'S') => (4, 1),
            _ => return None,
        };
        if rank <= last_rank {
            return None;
        }
        last_rank = rank;
        total = total.checked_add(number.checked_mul(multiplier)?)?;
        saw_value = true;
    }
    saw_value.then_some(total)
}

fn sanitize_reason(value: &str, api_key: &str) -> String {
    let redacted = value.replace(api_key, "REDACTED");
    let sanitized = redacted
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
        .take(MAX_SERVICE_REASON_CHARS)
        .collect::<String>();
    if sanitized.is_empty() {
        "unknown".to_owned()
    } else {
        sanitized
    }
}

fn sanitize_service_text(value: &str, api_key: &str, max_chars: usize) -> String {
    let redacted = value.replace(api_key, "[REDACTED]");
    let normalized = redacted.split_whitespace().collect::<Vec<_>>().join(" ");
    let bounded = normalized.chars().take(max_chars).collect::<String>();
    if bounded.is_empty() {
        "YouTube API request failed".to_owned()
    } else {
        bounded
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSearchResponse {
    #[serde(default)]
    next_page_token: Option<String>,
    #[serde(default)]
    items: Vec<RawSearchItem>,
}

#[derive(Debug, Deserialize)]
struct RawChannelUploadsListResponse {
    #[serde(default)]
    items: Vec<RawChannelUploadsResource>,
}

#[derive(Debug, Deserialize)]
struct RawChannelUploadsResource {
    id: String,
    #[serde(rename = "contentDetails")]
    content_details: RawChannelContentDetails,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawChannelContentDetails {
    related_playlists: RawRelatedPlaylists,
}

#[derive(Debug, Deserialize)]
struct RawRelatedPlaylists {
    uploads: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPlaylistItemsResponse {
    #[serde(default)]
    next_page_token: Option<String>,
    #[serde(default)]
    items: Vec<RawPlaylistItem>,
}

#[derive(Debug, Deserialize)]
struct RawPlaylistItem {
    #[serde(rename = "contentDetails")]
    content_details: RawPlaylistItemContentDetails,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPlaylistItemContentDetails {
    video_id: String,
}

#[derive(Debug, Deserialize)]
struct RawSearchItem {
    id: RawSearchId,
    snippet: RawSnippet,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSearchId {
    #[serde(default)]
    video_id: Option<String>,
    #[serde(default)]
    channel_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSnippet {
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    channel_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    channel_title: String,
    #[serde(default)]
    custom_url: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    live_broadcast_content: Option<String>,
    #[serde(default)]
    thumbnails: BTreeMap<String, RawThumbnail>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawThumbnail {
    url: String,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct RawVideoListResponse {
    #[serde(default)]
    items: Vec<RawVideoResource>,
}

#[derive(Debug, Deserialize)]
struct RawVideoResource {
    id: String,
    snippet: RawSnippet,
    #[serde(default, rename = "contentDetails")]
    content_details: RawContentDetails,
    #[serde(default)]
    statistics: RawVideoStatistics,
    #[serde(default)]
    status: RawVideoStatus,
    #[serde(default)]
    player: RawVideoPlayer,
}

#[derive(Debug, Default, Deserialize)]
struct RawContentDetails {
    #[serde(default)]
    duration: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(
    clippy::struct_field_names,
    reason = "the fields intentionally mirror YouTube's statistics schema"
)]
struct RawVideoStatistics {
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    view_count: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    like_count: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    comment_count: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct RawVideoStatus {
    #[serde(default)]
    license: Option<String>,
}

/// Player dimensions returned when a square embed bound is requested.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawVideoPlayer {
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    embed_width: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    embed_height: Option<u64>,
}

impl RawVideoPlayer {
    /// Classifies usable embed dimensions without consulting artwork.
    fn orientation(&self) -> VideoOrientation {
        self.embed_width
            .zip(self.embed_height)
            .and_then(|(width, height)| {
                Some((u32::try_from(width).ok()?, u32::try_from(height).ok()?))
            })
            .map_or(VideoOrientation::Unknown, |(width, height)| {
                VideoOrientation::from_dimensions(width, height)
            })
    }
}

#[derive(Debug, Deserialize)]
struct RawChannelListResponse {
    #[serde(default)]
    items: Vec<RawChannelResource>,
}

#[derive(Debug, Deserialize)]
struct RawChannelStatisticsListResponse {
    #[serde(default)]
    items: Vec<RawChannelStatisticsResource>,
}

#[derive(Debug, Deserialize)]
struct RawChannelStatisticsResource {
    id: String,
    #[serde(default)]
    snippet: Option<RawSnippet>,
    #[serde(default)]
    statistics: RawChannelStatistics,
}

#[derive(Debug, Deserialize)]
struct RawChannelResource {
    id: String,
    snippet: RawSnippet,
    #[serde(default)]
    statistics: RawChannelStatistics,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(
    clippy::struct_field_names,
    reason = "the fields intentionally mirror YouTube's statistics schema"
)]
struct RawChannelStatistics {
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    subscriber_count: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    video_count: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    view_count: Option<u64>,
    #[serde(default)]
    hidden_subscriber_count: bool,
}

#[derive(Debug, Deserialize)]
struct RawCommentThreadListResponse {
    #[serde(default)]
    items: Vec<RawCommentThread>,
}

#[derive(Debug, Deserialize)]
struct RawCommentThread {
    snippet: RawCommentThreadSnippet,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCommentThreadSnippet {
    top_level_comment: RawComment,
}

#[derive(Debug, Deserialize)]
struct RawComment {
    id: String,
    snippet: RawCommentSnippet,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCommentSnippet {
    author_display_name: String,
    #[serde(default)]
    author_channel_url: Option<String>,
    text_display: String,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    like_count: Option<u64>,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawErrorEnvelope {
    error: RawApiError,
}

#[derive(Debug, Deserialize)]
struct RawApiError {
    #[serde(default)]
    message: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    errors: Vec<RawApiErrorItem>,
}

#[derive(Debug, Deserialize)]
struct RawApiErrorItem {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    reason: Option<String>,
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
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};

    use crate::providers::MAX_VIDEO_COMMENT_TEXT_BYTES;

    use super::*;

    const TEST_KEY: &str = "test_api_key_123456789";
    const VIDEO_ID: &str = "dQw4w9WgXcQ";
    const CHANNEL_ID: &str = "UC_x5XG1OV2P6uZZ5FSM9Ttw";

    const SEARCH_VIDEO: &str = r#"{
        "nextPageToken": "opaque_next_2",
        "items": [{
            "id": {"videoId": "dQw4w9WgXcQ"},
            "snippet": {
                "publishedAt": "2024-01-02T03:04:05Z",
                "channelId": "UC_x5XG1OV2P6uZZ5FSM9Ttw",
                "title": "Search title",
                "description": "Search description",
                "channelTitle": "Search channel",
                "liveBroadcastContent": "none",
                "thumbnails": {
                    "default": {
                        "url": "https://i.ytimg.com/vi/dQw4w9WgXcQ/default.jpg",
                        "width": 120,
                        "height": 90
                    }
                }
            }
        }]
    }"#;

    const VIDEO_RESOURCE: &str = r#"{
        "items": [{
            "id": "dQw4w9WgXcQ",
            "snippet": {
                "publishedAt": "2024-01-02T03:04:05.123Z",
                "channelId": "UC_x5XG1OV2P6uZZ5FSM9Ttw",
                "title": "Enriched title",
                "description": "Full description",
                "channelTitle": "Enriched channel",
                "tags": ["open", "music"],
                "liveBroadcastContent": "none",
                "thumbnails": {
                    "maxres": {
                        "url": "https://i.ytimg.com/vi/dQw4w9WgXcQ/maxresdefault.jpg",
                        "width": 1280,
                        "height": 720
                    },
                    "default": {
                        "url": "https://i.ytimg.com/vi/dQw4w9WgXcQ/default.jpg",
                        "width": 120,
                        "height": 90
                    },
                    "unsafe": {"url": "file:///etc/passwd"}
                }
            },
            "contentDetails": {"duration": "P1DT2H3M4S"},
            "statistics": {
                "viewCount": "123456",
                "likeCount": 789,
                "commentCount": "20"
            },
            "status": {"license": "creativeCommon"},
            "player": {"embedWidth": "1080", "embedHeight": 1920}
        }]
    }"#;

    const COMMENT_THREADS: &str = r#"{
        "items": [{
            "id": "Ugz-thread-one",
            "snippet": {
                "topLevelComment": {
                    "id": "Ugz-comment-one",
                    "snippet": {
                        "authorDisplayName": "First author",
                        "authorChannelUrl": "https://www.youtube.com/@first-author",
                        "textDisplay": "First line\nSecond line & plain text",
                        "likeCount": "42",
                        "publishedAt": "2024-03-04T05:06:07Z",
                        "updatedAt": "2024-03-05T06:07:08Z"
                    }
                }
            }
        }, {
            "id": "Ugz-thread-two",
            "snippet": {
                "topLevelComment": {
                    "id": "Ugz-comment-two",
                    "snippet": {
                        "authorDisplayName": "Second author",
                        "authorChannelUrl": "file:///etc/passwd",
                        "textDisplay": "Another public comment",
                        "likeCount": 3,
                        "publishedAt": "2024-04-05T06:07:08Z"
                    }
                }
            }
        }]
    }"#;

    const SEARCH_CHANNEL: &str = r#"{
        "items": [{
            "id": {"channelId": "UC_x5XG1OV2P6uZZ5FSM9Ttw"},
            "snippet": {
                "channelId": "UC_x5XG1OV2P6uZZ5FSM9Ttw",
                "title": "Search channel",
                "description": "Search channel description",
                "channelTitle": "Search channel",
                "thumbnails": {}
            }
        }]
    }"#;

    const CHANNEL_RESOURCE: &str = r#"{
        "items": [{
            "id": "UC_x5XG1OV2P6uZZ5FSM9Ttw",
            "snippet": {
                "title": "Enriched channel",
                "description": "Full channel description",
                "customUrl": "@enriched",
                "publishedAt": "2014-04-24T10:11:12Z",
                "country": "UA",
                "thumbnails": {
                    "high": {
                        "url": "https://yt3.ggpht.com/channel=s800",
                        "width": 800,
                        "height": 800
                    }
                }
            },
            "statistics": {
                "subscriberCount": "9001",
                "videoCount": "42",
                "viewCount": "1094367204",
                "hiddenSubscriberCount": false
            }
        }]
    }"#;

    const UPLOADS_PLAYLIST_ID: &str = "UU_x5XG1OV2P6uZZ5FSM9Ttw";

    const CHANNEL_UPLOADS_RESOURCE: &str = r#"{
        "items": [{
            "id": "UC_x5XG1OV2P6uZZ5FSM9Ttw",
            "contentDetails": {
                "relatedPlaylists": {
                    "uploads": "UU_x5XG1OV2P6uZZ5FSM9Ttw"
                }
            }
        }]
    }"#;

    const UPLOADS_PAGE_ONE: &str = r#"{
        "nextPageToken": "uploads_page_2",
        "items": [{
            "contentDetails": {"videoId": "dQw4w9WgXcQ"}
        }]
    }"#;

    const UPLOADS_PAGE_TWO: &str = r#"{
        "items": [{
            "contentDetails": {"videoId": "aaaaaaaaaaa"}
        }]
    }"#;

    const SECOND_VIDEO_RESOURCE: &str = r#"{
        "items": [{
            "id": "aaaaaaaaaaa",
            "snippet": {
                "publishedAt": "2025-02-03T04:05:06Z",
                "channelId": "UC_x5XG1OV2P6uZZ5FSM9Ttw",
                "title": "Second upload",
                "description": "Second description",
                "channelTitle": "Enriched channel",
                "liveBroadcastContent": "none",
                "thumbnails": {}
            },
            "contentDetails": {"duration": "PT3M2S"},
            "statistics": {"viewCount": "456"},
            "status": {"license": "youtube"},
            "player": {"embedWidth": 1920, "embedHeight": 1080}
        }]
    }"#;

    fn uploads_page(video_ids: &[String], next_page_token: Option<&str>) -> String {
        let token = next_page_token
            .map(|token| format!(r#""nextPageToken":"{token}","#))
            .unwrap_or_default();
        let items = video_ids
            .iter()
            .map(|video_id| format!(r#"{{"contentDetails":{{"videoId":"{video_id}"}}}}"#))
            .collect::<Vec<_>>()
            .join(",");
        format!(r#"{{{token}"items":[{items}]}}"#)
    }

    fn video_resources(video_ids: &[String]) -> String {
        let items = video_ids
            .iter()
            .enumerate()
            .map(|(index, video_id)| {
                format!(
                    r#"{{"id":"{video_id}","snippet":{{"channelId":"{CHANNEL_ID}","title":"Upload {index}","channelTitle":"Fixture channel"}}}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(r#"{{"items":[{items}]}}"#)
    }

    fn provider_with_server(responses: Vec<String>) -> (YouTubeOfficialProvider, MockServer) {
        let server = MockServer::spawn(responses);
        let provider = YouTubeOfficialProvider::with_base_url(
            TEST_KEY,
            server.base_url.clone(),
            Duration::from_secs(2),
            DEFAULT_MAX_JSON_BYTES,
        )
        .expect("mock provider should construct");
        (provider, server)
    }

    #[test]
    fn constructor_rejects_empty_short_long_and_unsafe_keys() {
        for key in [
            "",
            "short",
            "contains a whitespace",
            "contains?query=yes",
            "x/y/looks/like/a/path",
        ] {
            assert!(
                matches!(
                    YouTubeOfficialProvider::new(key),
                    Err(ProviderError::InvalidRequest(_))
                ),
                "{key:?}"
            );
        }
        assert!(matches!(
            YouTubeOfficialProvider::new("x".repeat(MAX_API_KEY_BYTES + 1)),
            Err(ProviderError::InvalidRequest(_))
        ));
    }

    #[test]
    fn debug_output_redacts_api_key_and_token_cache() {
        let provider =
            YouTubeOfficialProvider::new(TEST_KEY).expect("test API key should be accepted");
        let debug = format!("{provider:?}");

        assert!(!debug.contains(TEST_KEY));
        assert!(debug.contains("[REDACTED]"));
        assert!(debug.contains("[OPAQUE]"));
    }

    #[test]
    fn search_url_maps_supported_filters_without_authentication_material() {
        let provider =
            YouTubeOfficialProvider::new(TEST_KEY).expect("test API key should be accepted");
        let mut request = SearchRequest::new("ambient & drone", SearchTarget::Videos);
        request.sort = SearchSort::Views;
        request.filters.date = Some(SearchDate::Week);
        request.filters.duration = Some(SearchDuration::Long);
        request.filters.region = Some("ge".to_owned());
        request.filters.features = vec![
            SearchFeature::Live,
            SearchFeature::Hd,
            SearchFeature::CreativeCommons,
            SearchFeature::Subtitles,
            SearchFeature::ThreeD,
        ];

        let (url, _, _) = provider
            .build_search_url(&request, 1_704_067_200)
            .expect("supported search should produce a URL");
        let pairs = url.query_pairs().collect::<HashMap<_, _>>();

        assert_eq!(pairs.get("q").map(AsRef::as_ref), Some("ambient & drone"));
        assert_eq!(pairs.get("maxResults").map(AsRef::as_ref), Some("25"));
        assert_eq!(pairs.get("type").map(AsRef::as_ref), Some("video"));
        assert_eq!(pairs.get("order").map(AsRef::as_ref), Some("viewCount"));
        assert_eq!(pairs.get("regionCode").map(AsRef::as_ref), Some("GE"));
        assert_eq!(pairs.get("videoDuration").map(AsRef::as_ref), Some("long"));
        assert_eq!(
            pairs.get("publishedAfter").map(AsRef::as_ref),
            Some("2023-12-25T00:00:00Z")
        );
        assert_eq!(
            pairs.get("videoDefinition").map(AsRef::as_ref),
            Some("high")
        );
        assert_eq!(
            pairs.get("videoCaption").map(AsRef::as_ref),
            Some("closedCaption")
        );
        assert_eq!(
            pairs.get("videoLicense").map(AsRef::as_ref),
            Some("creativeCommon")
        );
        assert_eq!(pairs.get("videoDimension").map(AsRef::as_ref), Some("3d"));
        assert_eq!(pairs.get("eventType").map(AsRef::as_ref), Some("live"));
        assert!(
            !pairs.contains_key("key"),
            "builders must not retain the key"
        );
    }

    #[test]
    fn upload_date_search_uses_official_date_order_and_a_distinct_page_key() {
        let provider =
            YouTubeOfficialProvider::new(TEST_KEY).expect("test API key should be accepted");
        let relevance = SearchRequest::new("new releases", SearchTarget::Videos);
        let mut newest = relevance.clone();
        newest.sort = SearchSort::UploadDate;

        let (url, newest_key, _) = provider
            .build_search_url(&newest, 1_704_067_200)
            .expect("newest-first search should produce a URL");
        let pairs = url.query_pairs().collect::<HashMap<_, _>>();

        assert_eq!(pairs.get("order").map(AsRef::as_ref), Some("date"));
        assert_ne!(
            newest_key,
            SearchKey::from_request(&relevance),
            "page tokens must remain scoped to the requested ordering"
        );
    }

    #[test]
    fn creative_commons_search_uses_video_license_and_a_distinct_page_key() {
        let provider =
            YouTubeOfficialProvider::new(TEST_KEY).expect("test API key should be accepted");
        let unfiltered = SearchRequest::new("open music", SearchTarget::Videos);
        let mut creative_commons = unfiltered.clone();
        creative_commons
            .filters
            .features
            .push(SearchFeature::CreativeCommons);

        let (url, creative_commons_key, _) = provider
            .build_search_url(&creative_commons, 1_704_067_200)
            .expect("Creative Commons search should produce a URL");
        let pairs = url.query_pairs().collect::<HashMap<_, _>>();

        assert_eq!(
            pairs.get("videoLicense").map(AsRef::as_ref),
            Some("creativeCommon")
        );
        assert_ne!(
            creative_commons_key,
            SearchKey::from_request(&unfiltered),
            "page tokens must not cross between filtered and unfiltered searches"
        );
    }

    #[test]
    fn unsupported_filters_fail_before_network_access() {
        let provider =
            YouTubeOfficialProvider::new(TEST_KEY).expect("test API key should be accepted");
        let mut channels = SearchRequest::new("channel", SearchTarget::Channels);
        channels.filters.duration = Some(SearchDuration::Short);
        assert!(matches!(
            provider.search_at(&channels, 1_704_067_200),
            Err(ProviderError::InvalidRequest(_))
        ));

        let mut video = SearchRequest::new("video", SearchTarget::Videos);
        video.filters.features = vec![SearchFeature::FourK];
        assert!(matches!(
            provider.search_at(&video, 1_704_067_200),
            Err(ProviderError::Unsupported)
        ));
    }

    #[test]
    fn video_search_is_batch_enriched_and_preserves_ordered_metadata() {
        let (provider, server) = provider_with_server(vec![
            json_response("200 OK", SEARCH_VIDEO),
            json_response("200 OK", VIDEO_RESOURCE),
        ]);
        let page = provider
            .search_at(
                &SearchRequest::new("open music", SearchTarget::Videos),
                1_704_067_200,
            )
            .expect("mock search should succeed");
        let requests = server.finish();

        assert_eq!(page.page, 1);
        assert_eq!(page.next_page, Some(2));
        let [SearchItem::Video(video)] = page.items.as_slice() else {
            panic!("expected one video");
        };
        assert_eq!(video.title, "Enriched title");
        assert_eq!(video.channel_name, "Enriched channel");
        assert_eq!(video.duration_seconds, Some(93_784));
        assert_eq!(video.view_count, Some(123_456));
        assert_eq!(video.orientation, VideoOrientation::Vertical);
        assert_eq!(video.thumbnails.len(), 2);
        assert_eq!(
            video.thumbnails[0].quality.as_deref(),
            Some("default"),
            "thumbnail order should be deterministic"
        );
        assert!(requests[0].starts_with("/search?"));
        assert!(requests[1].starts_with("/videos?"));
        assert!(requests_contain_part(
            &requests,
            "snippet%2CcontentDetails%2Cstatistics%2Cplayer"
        ));
        let resource_pairs = query_pairs(&requests[1]);
        assert_eq!(resource_pairs.get("id").map(String::as_str), Some(VIDEO_ID));
        assert!(
            !resource_pairs.contains_key("maxResults"),
            "videos.list does not need maxResults when an ID filter is supplied"
        );
        assert_eq!(
            resource_pairs.get("fields").map(String::as_str),
            Some(
                "items(id,snippet(publishedAt,channelId,title,description,channelTitle,liveBroadcastContent,thumbnails),contentDetails/duration,statistics/viewCount,player(embedWidth,embedHeight))"
            )
        );
        assert_eq!(
            resource_pairs.get("maxWidth").map(String::as_str),
            Some(PLAYER_EMBED_BOUND)
        );
        assert_eq!(
            resource_pairs.get("maxHeight").map(String::as_str),
            Some(PLAYER_EMBED_BOUND)
        );
        for request in &requests {
            assert!(request.contains(&format!("key={TEST_KEY}")));
        }
    }

    #[test]
    fn video_search_accepts_a_genuinely_empty_page_without_enrichment() {
        let (provider, server) =
            provider_with_server(vec![json_response("200 OK", r#"{"items":[]}"#)]);

        let page = provider
            .search_at(
                &SearchRequest::new("no matches", SearchTarget::Videos),
                1_704_067_200,
            )
            .expect("an empty upstream page is a valid search result");
        let requests = server.finish();

        assert!(page.items.is_empty());
        assert_eq!(page.next_page, None);
        assert_eq!(
            requests.len(),
            1,
            "an empty page must not request video enrichment"
        );
    }

    #[test]
    fn video_search_skips_one_missing_video_id_and_keeps_valid_rows() {
        let non_video = r#"{
            "id": {
                "kind": "youtube#channel",
                "channelId": "UC_x5XG1OV2P6uZZ5FSM9Ttw"
            },
            "snippet": {
                "title": "Unexpected channel row"
            }
        }"#;
        let mixed_page =
            SEARCH_VIDEO.replacen(r#""items": ["#, &format!(r#""items": [{non_video}, "#), 1);
        let (provider, server) = provider_with_server(vec![
            json_response("200 OK", &mixed_page),
            json_response("200 OK", VIDEO_RESOURCE),
        ]);

        let page = provider
            .search_at(
                &SearchRequest::new("open music", SearchTarget::Videos),
                1_704_067_200,
            )
            .expect("one malformed row must not hide valid videos");
        let requests = server.finish();

        let [SearchItem::Video(video)] = page.items.as_slice() else {
            panic!("only the valid video row should survive");
        };
        assert_eq!(video.video_id, VIDEO_ID);
        assert_eq!(video.title, "Enriched title");
        assert_eq!(page.next_page, Some(2));
        assert_eq!(requests.len(), 2);
        assert_eq!(
            query_pairs(&requests[1]).get("id").map(String::as_str),
            Some(VIDEO_ID),
            "batch enrichment must contain only surviving video IDs"
        );
    }

    #[test]
    fn video_search_rejects_an_all_malformed_page_without_caching_its_cursor() {
        let malformed_page = r#"{
            "nextPageToken": "must_not_survive",
            "items": [{
                "id": {
                    "kind": "youtube#channel",
                    "channelId": "UC_x5XG1OV2P6uZZ5FSM9Ttw"
                },
                "snippet": {
                    "title": "Unexpected channel row"
                }
            }]
        }"#;
        let (provider, server) =
            provider_with_server(vec![json_response("200 OK", malformed_page)]);
        let mut request = SearchRequest::new("broken page", SearchTarget::Videos);

        let error = provider
            .search_at(&request, 1_704_067_200)
            .expect_err("a nonempty page without usable videos must fail");
        let requests = server.finish();

        assert!(matches!(
            error,
            ProviderError::InvalidResponse(message)
                if message.contains("result 0 omitted its video ID")
        ));
        assert_eq!(
            requests.len(),
            1,
            "an all-malformed page must not start batch enrichment"
        );
        request.page = 2;
        assert!(matches!(
            provider.search_at(&request, 1_704_067_200),
            Err(ProviderError::InvalidRequest(message)) if message.contains("page 1")
        ));
    }

    #[test]
    fn video_search_rejects_more_rows_than_requested_before_enrichment() {
        let item = r#"{
            "id": {"videoId": "dQw4w9WgXcQ"},
            "snippet": {"title": "Bounded fixture"}
        }"#;
        let oversized_page = format!(
            r#"{{"nextPageToken":"must_not_survive","items":[{}]}}"#,
            vec![item; usize::from(SEARCH_RESULTS_PER_PAGE) + 1].join(",")
        );
        let (provider, server) =
            provider_with_server(vec![json_response("200 OK", &oversized_page)]);
        let mut request = SearchRequest::new("oversized page", SearchTarget::Videos);

        let error = provider
            .search_at(&request, 1_704_067_200)
            .expect_err("the upstream page must honor requested maxResults");
        let requests = server.finish();

        assert!(matches!(
            error,
            ProviderError::InvalidResponse(message)
                if message.contains("more than 25 video search results")
        ));
        assert_eq!(
            requests.len(),
            1,
            "an oversized page must fail before batch enrichment"
        );
        request.page = 2;
        assert!(matches!(
            provider.search_at(&request, 1_704_067_200),
            Err(ProviderError::InvalidRequest(message)) if message.contains("page 1")
        ));
    }

    #[test]
    fn channel_search_uses_one_batch_and_respects_hidden_subscribers() {
        let (provider, server) = provider_with_server(vec![
            json_response("200 OK", SEARCH_CHANNEL),
            json_response("200 OK", CHANNEL_RESOURCE),
        ]);
        let page = provider
            .search_at(
                &SearchRequest::new("example", SearchTarget::Channels),
                1_704_067_200,
            )
            .expect("mock search should succeed");
        let requests = server.finish();

        let [SearchItem::Channel(channel)] = page.items.as_slice() else {
            panic!("expected one channel");
        };
        assert_eq!(channel.channel_id, CHANNEL_ID);
        assert_eq!(channel.name, "Enriched channel");
        assert_eq!(channel.subscriber_count, Some(9_001));
        assert_eq!(channel.video_count, Some(42));
        assert_eq!(channel.created_at, Some(1_398_334_272));
        assert_eq!(requests.len(), 2);
        assert!(requests[1].starts_with("/channels?"));

        let raw: RawChannelResource = serde_json::from_str(
            r#"{
                "id":"UC_x5XG1OV2P6uZZ5FSM9Ttw",
                "snippet":{"title":"Hidden","description":"","thumbnails":{}},
                "statistics":{"subscriberCount":"99","hiddenSubscriberCount":true}
            }"#,
        )
        .expect("fixture should parse");
        assert_eq!(
            channel_summary_from_resource(raw)
                .expect("fixture should convert")
                .subscriber_count,
            None
        );
    }

    #[test]
    fn channel_details_use_exact_id_and_map_public_metadata() {
        let (provider, server) =
            provider_with_server(vec![json_response("200 OK", CHANNEL_RESOURCE)]);

        let channel = provider
            .channel_details(CHANNEL_ID)
            .expect("exact channel metadata should parse");
        let requests = server.finish();

        assert_eq!(channel.channel_id, CHANNEL_ID);
        assert_eq!(channel.name, "Enriched channel");
        assert_eq!(channel.description, "Full channel description");
        assert_eq!(channel.subscriber_count, Some(9_001));
        assert_eq!(channel.video_count, Some(42));
        assert_eq!(channel.thumbnails.len(), 1);
        assert_eq!(
            channel.webpage_url.as_ref().map(Url::as_str),
            Some("https://www.youtube.com/@enriched")
        );

        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("/channels?"));
        let pairs = query_pairs(&requests[0]);
        assert_eq!(
            pairs.get("part").map(String::as_str),
            Some("snippet,statistics")
        );
        assert_eq!(pairs.get("id").map(String::as_str), Some(CHANNEL_ID));
        assert_eq!(pairs.get("maxResults").map(String::as_str), Some("1"));
        assert_eq!(pairs.get("key").map(String::as_str), Some(TEST_KEY));
        assert_eq!(
            pairs.len(),
            4,
            "exact lookup must not add search parameters"
        );
    }

    #[test]
    fn official_custom_channel_url_accepts_only_channel_route_shapes() {
        for (custom_url, expected) in [
            ("@fixture", "https://www.youtube.com/@fixture"),
            (
                "/@ქართული",
                "https://www.youtube.com/@%E1%83%A5%E1%83%90%E1%83%A0%E1%83%97%E1%83%A3%E1%83%9A%E1%83%98",
            ),
            (
                "c/Fixture-Channel",
                "https://www.youtube.com/c/Fixture-Channel",
            ),
            (
                "user/fixture_name",
                "https://www.youtube.com/user/fixture_name",
            ),
        ] {
            assert_eq!(
                youtube_custom_channel_url(custom_url)
                    .as_ref()
                    .map(Url::as_str),
                Some(expected),
                "{custom_url:?} should remain a human-readable channel route"
            );
        }

        for custom_url in [
            "https://evil.example/@fixture",
            "../@fixture",
            "@fixture?redirect=1",
            "watch",
            "redirect",
            "@",
            "@fixture/shorts",
            "channel/UCdifferent",
            "c/../watch",
            "user/name#fragment",
            "@fixture%2Fwatch",
        ] {
            assert!(
                youtube_custom_channel_url(custom_url).is_none(),
                "{custom_url:?} must not become a channel page"
            );
            assert_eq!(
                youtube_channel_url_with_custom(CHANNEL_ID, Some(custom_url))
                    .as_ref()
                    .map(Url::as_str),
                Some("https://www.youtube.com/channel/UC_x5XG1OV2P6uZZ5FSM9Ttw")
            );
        }
    }

    #[test]
    fn full_channel_details_add_country_and_aggregate_views_in_one_request() {
        let (provider, server) =
            provider_with_server(vec![json_response("200 OK", CHANNEL_RESOURCE)]);

        let details = provider
            .full_channel_details(CHANNEL_ID)
            .expect("full channel metadata should parse");
        let requests = server.finish();

        assert_eq!(details.summary.channel_id, CHANNEL_ID);
        assert_eq!(details.summary.video_count, Some(42));
        assert_eq!(details.summary.created_at, Some(1_398_334_272));
        assert_eq!(details.total_view_count, Some(1_094_367_204));
        assert_eq!(details.country.as_deref(), Some("UA"));
        assert!(details.external_links.is_empty());
        assert!(!details.external_links_truncated);
        assert_eq!(requests.len(), 1);
        assert_eq!(
            query_pairs(&requests[0]).get("part").map(String::as_str),
            Some("snippet,statistics")
        );
    }

    #[test]
    fn channel_details_preserve_hidden_and_missing_public_metadata() {
        let hidden = format!(
            r#"{{
                "items": [{{
                    "id": "{CHANNEL_ID}",
                    "snippet": {{
                        "title": "Hidden channel",
                        "description": "",
                        "thumbnails": {{}}
                    }},
                    "statistics": {{
                        "subscriberCount": "99",
                        "hiddenSubscriberCount": true
                    }}
                }}]
            }}"#
        );
        let (provider, server) = provider_with_server(vec![json_response("200 OK", &hidden)]);

        let channel = provider
            .channel_details(CHANNEL_ID)
            .expect("hidden optional metadata is still a valid channel");
        server.finish();

        assert_eq!(channel.description, "");
        assert_eq!(channel.subscriber_count, None);
        assert_eq!(channel.video_count, None);
        assert!(channel.thumbnails.is_empty());
    }

    #[test]
    fn channel_details_reject_invalid_mismatched_and_missing_identifiers() {
        let provider =
            YouTubeOfficialProvider::new(TEST_KEY).expect("test API key should be accepted");
        for invalid in ["", "../channels", "UC fixture", "UCfixture?key=leak"] {
            assert!(
                matches!(
                    provider.channel_details(invalid),
                    Err(ProviderError::InvalidRequest(_))
                ),
                "{invalid:?}"
            );
        }

        let mismatched = CHANNEL_RESOURCE.replace(CHANNEL_ID, "UCaaaaaaaaaaaaaaaaaaaaaa");
        let (provider, server) = provider_with_server(vec![json_response("200 OK", &mismatched)]);
        assert!(matches!(
            provider.channel_details(CHANNEL_ID),
            Err(ProviderError::InvalidResponse(message)) if message.contains("does not match")
        ));
        server.finish();

        let (provider, server) =
            provider_with_server(vec![json_response("200 OK", r#"{"items":[]}"#)]);
        assert!(matches!(
            provider.channel_details(CHANNEL_ID),
            Err(ProviderError::InvalidResponse(message)) if message.contains("not found")
        ));
        server.finish();

        let malformed = CHANNEL_RESOURCE.replace(CHANNEL_ID, "invalid channel id");
        let (provider, server) = provider_with_server(vec![json_response("200 OK", &malformed)]);
        assert!(matches!(
            provider.channel_details(CHANNEL_ID),
            Err(ProviderError::InvalidResponse(_))
        ));
        server.finish();
    }

    #[test]
    fn channel_uploads_use_low_quota_playlist_pagination_and_video_batches() {
        let (provider, server) = provider_with_server(vec![
            json_response("200 OK", CHANNEL_UPLOADS_RESOURCE),
            json_response("200 OK", UPLOADS_PAGE_ONE),
            json_response("200 OK", VIDEO_RESOURCE),
            json_response("200 OK", UPLOADS_PAGE_TWO),
            json_response("200 OK", SECOND_VIDEO_RESOURCE),
        ]);
        let mut request = ChannelVideosRequest::new(CHANNEL_ID);
        let first = provider
            .channel_videos(&request)
            .expect("first uploads page should succeed");
        request.page = 2;
        let second = provider
            .channel_videos(&request)
            .expect("cached continuation should load page two");
        let requests = server.finish();

        assert_eq!(first.page, 1);
        assert_eq!(first.next_page, Some(2));
        let [SearchItem::Video(first_video)] = first.items.as_slice() else {
            panic!("channel uploads must contain only videos");
        };
        assert_eq!(first_video.video_id, VIDEO_ID);
        assert_eq!(first_video.title, "Enriched title");
        assert_eq!(first_video.duration_seconds, Some(93_784));

        assert_eq!(second.page, 2);
        assert_eq!(second.next_page, None);
        let [SearchItem::Video(second_video)] = second.items.as_slice() else {
            panic!("channel uploads must contain only videos");
        };
        assert_eq!(second_video.video_id, "aaaaaaaaaaa");
        assert_eq!(second_video.duration_seconds, Some(182));

        assert_eq!(requests.len(), 5);
        assert!(
            requests
                .iter()
                .all(|request| !request.starts_with("/search?")),
            "channel uploads must not spend search.list quota"
        );
        assert!(requests[0].starts_with("/channels?"));
        let channel_pairs = query_pairs(&requests[0]);
        assert_eq!(
            channel_pairs.get("part").map(String::as_str),
            Some("contentDetails")
        );
        assert_eq!(
            channel_pairs.get("id").map(String::as_str),
            Some(CHANNEL_ID)
        );
        assert_eq!(
            channel_pairs.get("fields").map(String::as_str),
            Some("items(id,contentDetails/relatedPlaylists/uploads)")
        );

        assert!(requests[1].starts_with("/playlistItems?"));
        let first_playlist_pairs = query_pairs(&requests[1]);
        assert_eq!(
            first_playlist_pairs.get("playlistId").map(String::as_str),
            Some(UPLOADS_PLAYLIST_ID)
        );
        assert_eq!(
            first_playlist_pairs.get("part").map(String::as_str),
            Some("contentDetails")
        );
        assert_eq!(
            first_playlist_pairs.get("maxResults").map(String::as_str),
            Some("50")
        );
        assert_eq!(
            first_playlist_pairs.get("fields").map(String::as_str),
            Some("nextPageToken,items(contentDetails/videoId)")
        );
        assert!(!first_playlist_pairs.contains_key("pageToken"));
        assert!(requests[2].starts_with("/videos?"));
        let first_video_pairs = query_pairs(&requests[2]);
        assert!(!first_video_pairs.contains_key("maxResults"));
        assert_eq!(
            first_video_pairs.get("fields").map(String::as_str),
            Some(
                "items(id,snippet(publishedAt,channelId,title,description,channelTitle,liveBroadcastContent,thumbnails),contentDetails/duration,statistics/viewCount,player(embedWidth,embedHeight))"
            )
        );

        assert!(requests[3].starts_with("/playlistItems?"));
        let second_playlist_pairs = query_pairs(&requests[3]);
        assert_eq!(
            second_playlist_pairs.get("pageToken").map(String::as_str),
            Some("uploads_page_2")
        );
        assert!(requests[4].starts_with("/videos?"));
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("/channels?"))
                .count(),
            1,
            "the uploads playlist ID must be reused for continuation pages"
        );
    }

    #[test]
    fn channel_uploads_require_valid_identifiers_and_sequential_pages() {
        let provider =
            YouTubeOfficialProvider::new(TEST_KEY).expect("test API key should be accepted");
        for channel_id in ["../channels", "UC fixture", "UCfixture?key=leak"] {
            let error = provider
                .channel_videos(&ChannelVideosRequest::new(channel_id))
                .expect_err("path-like channel identifier must fail before transport");
            assert!(matches!(error, ProviderError::InvalidRequest(_)));
        }

        let mut second_page = ChannelVideosRequest::new(CHANNEL_ID);
        second_page.page = 2;
        let error = provider
            .channel_videos(&second_page)
            .expect_err("page two needs the page-one continuation");
        assert!(
            matches!(
                &error,
                ProviderError::InvalidRequest(message) if message.contains("page 1")
            ),
            "{error}"
        );
    }

    #[test]
    fn channel_uploads_accept_exactly_fifty_rows_in_one_video_batch() {
        let video_ids = (0..50)
            .map(|index| format!("v{index:010}"))
            .collect::<Vec<_>>();
        let playlist = uploads_page(&video_ids, None);
        let resources = video_resources(&video_ids);
        let (provider, server) = provider_with_server(vec![
            json_response("200 OK", CHANNEL_UPLOADS_RESOURCE),
            json_response("200 OK", &playlist),
            json_response("200 OK", &resources),
        ]);

        let page = provider
            .channel_videos(&ChannelVideosRequest::new(CHANNEL_ID))
            .expect("the documented 50-item uploads page must be accepted");
        let requests = server.finish();

        assert_eq!(page.items.len(), 50);
        assert_eq!(requests.len(), 3);
        let playlist_pairs = query_pairs(&requests[1]);
        assert_eq!(
            playlist_pairs.get("maxResults").map(String::as_str),
            Some("50")
        );
        let video_pairs = query_pairs(&requests[2]);
        let requested_ids = video_ids.join(",");
        assert_eq!(
            video_pairs.get("id").map(String::as_str),
            Some(requested_ids.as_str())
        );
        assert!(!video_pairs.contains_key("maxResults"));
    }

    #[test]
    fn channel_uploads_reject_fifty_one_rows_before_video_enrichment() {
        let video_ids = (0..51)
            .map(|index| format!("v{index:010}"))
            .collect::<Vec<_>>();
        let playlist = uploads_page(&video_ids, Some("must_not_survive"));
        let (provider, server) = provider_with_server(vec![
            json_response("200 OK", CHANNEL_UPLOADS_RESOURCE),
            json_response("200 OK", &playlist),
        ]);

        let error = provider
            .channel_videos(&ChannelVideosRequest::new(CHANNEL_ID))
            .expect_err("an oversized uploads page must be rejected");
        let mut second_page = ChannelVideosRequest::new(CHANNEL_ID);
        second_page.page = 2;
        assert!(matches!(
            provider.channel_videos(&second_page),
            Err(ProviderError::InvalidRequest(message)) if message.contains("page 1")
        ));
        let requests = server.finish();

        assert!(matches!(
            error,
            ProviderError::InvalidResponse(message)
                if message.contains("more than 50 channel uploads")
        ));
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|request| !request.starts_with("/videos?")),
            "an oversized playlist page must fail before video enrichment"
        );
    }

    #[test]
    fn failed_page_one_enrichment_preserves_the_previous_continuation_chain() {
        let page_one_old = uploads_page(&[VIDEO_ID.to_owned()], Some("old_page_2"));
        let page_one_new = uploads_page(&[VIDEO_ID.to_owned()], Some("new_page_2"));
        let (provider, server) = provider_with_server(vec![
            json_response("200 OK", CHANNEL_UPLOADS_RESOURCE),
            json_response("200 OK", &page_one_old),
            json_response("200 OK", VIDEO_RESOURCE),
            json_response("200 OK", &page_one_new),
            json_response(
                "500 Internal Server Error",
                r#"{"error":{"message":"temporary enrichment failure"}}"#,
            ),
            json_response("200 OK", r#"{"items":[]}"#),
        ]);

        provider
            .channel_videos(&ChannelVideosRequest::new(CHANNEL_ID))
            .expect("the original page should seed its continuation");
        provider
            .channel_videos(&ChannelVideosRequest::new(CHANNEL_ID))
            .expect_err("the replacement enrichment should fail");
        let mut continuation = ChannelVideosRequest::new(CHANNEL_ID);
        continuation.page = 2;
        provider
            .channel_videos(&continuation)
            .expect("the previous continuation should remain usable");
        let requests = server.finish();

        let continuation_pairs = query_pairs(&requests[5]);
        assert_eq!(
            continuation_pairs.get("pageToken").map(String::as_str),
            Some("old_page_2"),
            "an incomplete refresh must not replace the last committed token chain"
        );
    }

    #[test]
    fn video_enrichment_rejects_more_than_fifty_identifiers_before_network() {
        let (provider, server) = provider_with_server(Vec::new());
        let video_ids = (0..51)
            .map(|index| format!("v{index:010}"))
            .collect::<Vec<_>>();

        let error = provider
            .fetch_video_resources(&video_ids, VideoResourceProjection::Summary)
            .expect_err("videos.list accepts at most 50 identifiers");
        let requests = server.finish();

        assert!(matches!(
            error,
            ProviderError::InvalidRequest(message) if message.contains("at most 50 identifiers")
        ));
        assert!(requests.is_empty());
    }

    #[test]
    fn video_enrichment_rejects_more_resources_than_requested() {
        let returned_ids = [VIDEO_ID.to_owned(), "aaaaaaaaaaa".to_owned()];
        let response = video_resources(&returned_ids);
        let (provider, server) = provider_with_server(vec![json_response("200 OK", &response)]);

        let error = provider
            .fetch_video_resources(&[VIDEO_ID.to_owned()], VideoResourceProjection::Summary)
            .expect_err("an oversized videos.list response must be rejected");
        let requests = server.finish();

        assert!(matches!(
            error,
            ProviderError::InvalidResponse(message)
                if message.contains("more video resources than requested (2 > 1)")
        ));
        assert_eq!(requests.len(), 1);
    }

    #[test]
    fn channel_uploads_reject_malformed_playlist_metadata_and_tokens() {
        let malformed_playlist =
            CHANNEL_UPLOADS_RESOURCE.replace(UPLOADS_PLAYLIST_ID, "invalid uploads playlist");
        let (provider, server) =
            provider_with_server(vec![json_response("200 OK", &malformed_playlist)]);
        let error = provider
            .channel_videos(&ChannelVideosRequest::new(CHANNEL_ID))
            .expect_err("unsafe playlist identifier must fail");
        server.finish();
        assert!(matches!(error, ProviderError::InvalidResponse(_)));

        let malformed_token = UPLOADS_PAGE_ONE.replace("uploads_page_2", "bad continuation token");
        let (provider, server) = provider_with_server(vec![
            json_response("200 OK", CHANNEL_UPLOADS_RESOURCE),
            json_response("200 OK", &malformed_token),
        ]);
        let error = provider
            .channel_videos(&ChannelVideosRequest::new(CHANNEL_ID))
            .expect_err("unsafe continuation token must fail");
        server.finish();
        assert!(matches!(error, ProviderError::InvalidResponse(_)));
    }

    #[test]
    fn unavailable_channel_upload_resources_are_omitted_without_changing_continuation() {
        let (provider, server) = provider_with_server(vec![
            json_response("200 OK", CHANNEL_UPLOADS_RESOURCE),
            json_response("200 OK", UPLOADS_PAGE_ONE),
            json_response("200 OK", r#"{"items":[]}"#),
        ]);
        let page = provider
            .channel_videos(&ChannelVideosRequest::new(CHANNEL_ID))
            .expect("private or deleted upload resources may be absent");
        server.finish();

        assert!(page.items.is_empty());
        assert_eq!(page.next_page, Some(2));
    }

    #[test]
    fn subscriber_lookup_batches_in_input_order_and_caches_negative_results() {
        const HIDDEN_ID: &str = "UCaaaaaaaaaaaaaaaaaaaaaa";
        const MISSING_ID: &str = "UCbbbbbbbbbbbbbbbbbbbbbb";
        let statistics = format!(
            r#"{{
                "items": [
                    {{
                        "id": "{HIDDEN_ID}",
                        "statistics": {{
                            "subscriberCount": "99",
                            "hiddenSubscriberCount": true
                        }}
                    }},
                    {{
                        "id": "{CHANNEL_ID}",
                        "snippet": {{
                            "customUrl": "@example-channel"
                        }},
                        "statistics": {{
                            "subscriberCount": "12345",
                            "hiddenSubscriberCount": false
                        }}
                    }}
                ]
            }}"#
        );
        let (provider, server) = provider_with_server(vec![json_response("200 OK", &statistics)]);
        let requested = vec![
            CHANNEL_ID.to_owned(),
            HIDDEN_ID.to_owned(),
            MISSING_ID.to_owned(),
            CHANNEL_ID.to_owned(),
        ];

        let counts = provider
            .channel_subscriber_counts(&requested)
            .expect("mock statistics should parse");
        let requests = server.finish();

        assert_eq!(
            counts,
            [
                ChannelSubscriberCount {
                    channel_id: CHANNEL_ID.to_owned(),
                    subscriber_count: Some(12_345),
                    webpage_url: Some(
                        Url::parse("https://www.youtube.com/@example-channel")
                            .expect("fixture channel handle"),
                    ),
                },
                ChannelSubscriberCount {
                    channel_id: HIDDEN_ID.to_owned(),
                    subscriber_count: None,
                    webpage_url: None,
                },
                ChannelSubscriberCount {
                    channel_id: MISSING_ID.to_owned(),
                    subscriber_count: None,
                    webpage_url: None,
                },
                ChannelSubscriberCount {
                    channel_id: CHANNEL_ID.to_owned(),
                    subscriber_count: Some(12_345),
                    webpage_url: Some(
                        Url::parse("https://www.youtube.com/@example-channel")
                            .expect("fixture channel handle"),
                    ),
                },
            ]
        );
        assert_eq!(requests.len(), 1, "one channels.list batch is sufficient");
        let pairs = query_pairs(&requests[0]);
        assert_eq!(
            pairs.get("part").map(String::as_str),
            Some("snippet,statistics")
        );
        assert_eq!(pairs.get("maxResults").map(String::as_str), Some("4"));
        let requested_ids = format!("{CHANNEL_ID},{HIDDEN_ID},{MISSING_ID},{CHANNEL_ID}");
        assert_eq!(
            pairs.get("id").map(String::as_str),
            Some(requested_ids.as_str())
        );
        assert!(requests[0].contains(&format!("key={TEST_KEY}")));
    }

    #[test]
    fn subscriber_lookup_enforces_official_batch_bound_before_network() {
        let provider =
            YouTubeOfficialProvider::new(TEST_KEY).expect("test API key should be accepted");
        assert_eq!(
            provider.channel_statistics_mode(),
            ChannelStatisticsMode::Batch { max_ids: 50 }
        );
        assert!(
            provider
                .channel_subscriber_counts(&[])
                .expect("an empty lookup should avoid the network")
                .is_empty()
        );

        let oversized = (0..=MAX_CHANNEL_STATISTICS_IDS)
            .map(|index| format!("UC{index:022}"))
            .collect::<Vec<_>>();
        assert!(matches!(
            provider.channel_subscriber_counts(&oversized),
            Err(ProviderError::InvalidRequest(message)) if message.contains("at most 50")
        ));
    }

    #[test]
    fn video_details_returns_statistics_keywords_and_license() {
        let (provider, server) =
            provider_with_server(vec![json_response("200 OK", VIDEO_RESOURCE)]);
        let details = provider
            .video_details(VIDEO_ID)
            .expect("mock details should succeed");
        let requests = server.finish();

        assert_eq!(details.video_id, VIDEO_ID);
        assert_eq!(details.like_count, Some(789));
        assert_eq!(details.comment_count, Some(20));
        assert_eq!(details.keywords, ["open", "music"]);
        assert_eq!(
            details.license.as_deref(),
            Some("Creative Commons Attribution")
        );
        assert_eq!(details.published_at, Some(1_704_164_645));
        assert_eq!(details.orientation, VideoOrientation::Vertical);
        let pairs = query_pairs(&requests[0]);
        assert_eq!(pairs.get("id").map(String::as_str), Some(VIDEO_ID));
        assert_eq!(
            pairs.get("part").map(String::as_str),
            Some("snippet,contentDetails,statistics,status,player")
        );
        let fields = pairs
            .get("fields")
            .expect("a video-details request should use a partial response");
        for retained_field in ["tags", "likeCount", "commentCount", "status/license"] {
            assert!(
                fields.contains(retained_field),
                "the full-details projection must retain {retained_field}"
            );
        }
        assert!(requests[0].contains(&format!("key={TEST_KEY}")));
    }

    #[test]
    fn comments_request_returns_bounded_relevant_plain_text() {
        let (provider, server) =
            provider_with_server(vec![json_response("200 OK", COMMENT_THREADS)]);
        let comments = provider
            .video_comments(VIDEO_ID)
            .expect("mock comments should succeed");
        let requests = server.finish();

        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].comment_id, "Ugz-comment-one");
        assert_eq!(comments[0].author_name, "First author");
        assert_eq!(
            comments[0].author_channel_url.as_ref().map(Url::as_str),
            Some("https://www.youtube.com/@first-author")
        );
        assert_eq!(comments[0].text, "First line\nSecond line & plain text");
        assert_eq!(comments[0].like_count, 42);
        assert_eq!(comments[0].published_at, Some(1_709_528_767));
        assert_eq!(comments[0].updated_at, Some(1_709_618_828));
        assert_eq!(
            comments[1].author_channel_url, None,
            "unsafe author URLs must not reach the public DTO"
        );
        assert_eq!(comments[1].updated_at, None);

        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("/commentThreads?"));
        let pairs = query_pairs(&requests[0]);
        assert_eq!(pairs.get("part").map(String::as_str), Some("snippet"));
        assert_eq!(pairs.get("videoId").map(String::as_str), Some(VIDEO_ID));
        assert_eq!(pairs.get("maxResults").map(String::as_str), Some("20"));
        assert_eq!(pairs.get("order").map(String::as_str), Some("relevance"));
        assert_eq!(
            pairs.get("textFormat").map(String::as_str),
            Some("plainText")
        );
        assert_eq!(pairs.get("key").map(String::as_str), Some(TEST_KEY));
    }

    #[test]
    fn comments_reject_invalid_identifiers_and_remote_bounds() {
        let provider =
            YouTubeOfficialProvider::new(TEST_KEY).expect("test API key should be accepted");
        assert!(matches!(
            provider.video_comments("../not-a-video"),
            Err(ProviderError::InvalidRequest(_))
        ));

        let item = r#"{
            "snippet": {
                "topLevelComment": {
                    "id": "Ugz-comment",
                    "snippet": {
                        "authorDisplayName": "Author",
                        "textDisplay": "Comment"
                    }
                }
            }
        }"#;
        let oversized_page = format!(
            r#"{{"items":[{}]}}"#,
            [item; MAX_VIDEO_COMMENTS + 1].join(",")
        );
        let (provider, server) =
            provider_with_server(vec![json_response("200 OK", &oversized_page)]);
        let error = provider
            .video_comments(VIDEO_ID)
            .expect_err("an oversized comment page must fail");
        server.finish();
        assert!(matches!(
            error,
            ProviderError::InvalidResponse(message) if message.contains("more than 20")
        ));

        let oversized_text = "x".repeat(MAX_VIDEO_COMMENT_TEXT_BYTES + 1);
        let body = COMMENT_THREADS.replace("Another public comment", &oversized_text);
        let (provider, server) = provider_with_server(vec![json_response("200 OK", &body)]);
        let error = provider
            .video_comments(VIDEO_ID)
            .expect_err("oversized comment text must fail");
        server.finish();
        assert!(matches!(
            error,
            ProviderError::InvalidResponse(message) if message.contains("comment text")
        ));
    }

    #[test]
    fn comments_reject_malformed_fields_instead_of_leaking_partial_data() {
        for body in [
            COMMENT_THREADS.replace("Ugz-comment-one", "invalid comment id"),
            COMMENT_THREADS.replace("2024-03-04T05:06:07Z", "not-a-date"),
            COMMENT_THREADS.replace("First author", " "),
            COMMENT_THREADS.replace("First author", "Author\\u001b[2J"),
            COMMENT_THREADS.replace("Another public comment", "tab\\tinjection"),
            COMMENT_THREADS.replace("Another public comment", "nul\\u0000injection"),
        ] {
            let (provider, server) = provider_with_server(vec![json_response("200 OK", &body)]);
            let error = provider
                .video_comments(VIDEO_ID)
                .expect_err("malformed comment metadata must fail");
            server.finish();
            assert!(matches!(error, ProviderError::InvalidResponse(_)));
        }
    }

    #[test]
    fn comments_normalize_portable_multiline_bodies() {
        let body = COMMENT_THREADS.replace(
            "First line\\nSecond line & plain text",
            "First line\\r\\nSecond line\\rThird line",
        );
        let (provider, server) = provider_with_server(vec![json_response("200 OK", &body)]);

        let comments = provider
            .video_comments(VIDEO_ID)
            .expect("portable line endings should remain multiline");
        server.finish();

        assert_eq!(
            comments[0].text, "First line\nSecond line\nThird line",
            "line normalization must not flatten the public comment"
        );
    }

    #[test]
    fn opaque_page_tokens_require_sequential_requests_and_stay_query_scoped() {
        let second_search = r#"{"items":[]}"#;
        let (provider, server) = provider_with_server(vec![
            json_response("200 OK", SEARCH_VIDEO),
            json_response("200 OK", VIDEO_RESOURCE),
            json_response("200 OK", second_search),
        ]);
        let mut request = SearchRequest::new("open music", SearchTarget::Videos);
        request.filters.date = Some(SearchDate::Week);
        request.page = 2;
        assert!(matches!(
            provider.search_at(&request, 1_704_067_200),
            Err(ProviderError::InvalidRequest(message)) if message.contains("page 1")
        ));

        request.page = 1;
        provider
            .search_at(&request, 1_704_067_200)
            .expect("first page should cache its opaque token");
        request.page = 2;
        let page = provider
            .search_at(&request, 1_704_153_600)
            .expect("second page should use the cached token");
        assert_eq!(page.next_page, None);

        let requests = server.finish();
        assert!(
            requests[2].contains("pageToken=opaque_next_2"),
            "{:?}",
            requests[2]
        );
        let first_pairs = query_pairs(&requests[0]);
        let second_pairs = query_pairs(&requests[2]);
        assert_eq!(
            first_pairs.get("publishedAfter"),
            second_pairs.get("publishedAfter"),
            "page-token requests must reuse the first page's exact date boundary"
        );
        let mut other_query = request;
        other_query.query = "different".to_owned();
        assert!(matches!(
            provider.search_at(&other_query, 1_704_067_200),
            Err(ProviderError::InvalidRequest(_))
        ));
        let mut other_sort = other_query;
        other_sort.query = "open music".to_owned();
        other_sort.sort = SearchSort::UploadDate;
        assert!(matches!(
            provider.search_at(&other_sort, 1_704_067_200),
            Err(ProviderError::InvalidRequest(_))
        ));
    }

    #[test]
    fn missing_enrichment_resource_falls_back_to_search_snippet() {
        let (provider, server) = provider_with_server(vec![
            json_response("200 OK", SEARCH_VIDEO),
            json_response("200 OK", r#"{"items":[]}"#),
        ]);
        let page = provider
            .search_at(
                &SearchRequest::new("open", SearchTarget::Videos),
                1_704_067_200,
            )
            .expect("missing private resource should use search metadata");
        server.finish();

        let [SearchItem::Video(video)] = page.items.as_slice() else {
            panic!("expected one video");
        };
        assert_eq!(video.title, "Search title");
        assert_eq!(video.duration_seconds, None);
        assert_eq!(video.orientation, VideoOrientation::Unknown);
    }

    #[test]
    fn quota_and_auth_payloads_are_structured_bounded_and_redacted() {
        for (status, reason) in [
            ("403 Forbidden", "quotaExceeded"),
            ("400 Bad Request", "keyInvalid"),
        ] {
            let body = format!(
                r#"{{
                    "error": {{
                        "code": 403,
                        "message": "key {TEST_KEY} failed\nwith details",
                        "errors": [{{
                            "message": "key {TEST_KEY} failed\nwith details",
                            "reason": "{reason}"
                        }}]
                    }}
                }}"#
            );
            let (provider, server) = provider_with_server(vec![json_response(status, &body)]);
            let error = provider
                .video_details(VIDEO_ID)
                .expect_err("service failure should be returned");
            server.finish();

            let rendered = error.to_string();
            assert!(!rendered.contains(TEST_KEY));
            assert!(rendered.contains("[REDACTED]"));
            assert!(rendered.contains(reason));
            assert!(!rendered.contains('\n'));
            assert!(matches!(
                error,
                ProviderError::Service {
                    reason: actual,
                    ..
                } if actual == reason
            ));
        }

        let huge = format!("prefix {TEST_KEY} {}", "x".repeat(2_000));
        let sanitized = sanitize_service_text(&huge, TEST_KEY, MAX_SERVICE_MESSAGE_CHARS);
        assert!(!sanitized.contains(TEST_KEY));
        assert!(sanitized.chars().count() <= MAX_SERVICE_MESSAGE_CHARS);
        assert_eq!(sanitize_reason(TEST_KEY, TEST_KEY), "REDACTED");
    }

    #[test]
    fn malformed_error_payload_retains_only_http_status() {
        let (provider, server) =
            provider_with_server(vec![json_response("500 Internal Server Error", "<html>")]);
        let error = provider
            .video_details(VIDEO_ID)
            .expect_err("HTTP failure should be returned");
        server.finish();
        assert!(matches!(error, ProviderError::HttpStatus(500)));
    }

    #[test]
    fn response_size_limit_is_enforced_before_json_parsing() {
        let server = MockServer::spawn(vec![json_response("200 OK", r#"{"items":[]}"#)]);
        let provider = YouTubeOfficialProvider::with_base_url(
            TEST_KEY,
            server.base_url.clone(),
            Duration::from_secs(2),
            4,
        )
        .expect("small bounded provider should construct");
        let error = provider
            .video_details(VIDEO_ID)
            .expect_err("response should exceed the configured bound");
        server.finish();
        assert!(matches!(
            error,
            ProviderError::ResponseTooLarge { limit: 4 }
        ));

        let server = MockServer::spawn(vec![json_response("200 OK", CHANNEL_RESOURCE)]);
        let provider = YouTubeOfficialProvider::with_base_url(
            TEST_KEY,
            server.base_url.clone(),
            Duration::from_secs(2),
            32,
        )
        .expect("small bounded provider should construct");
        let error = provider
            .channel_details(CHANNEL_ID)
            .expect_err("channel metadata must use the same response bound");
        server.finish();
        assert!(matches!(
            error,
            ProviderError::ResponseTooLarge { limit: 32 }
        ));
    }

    #[test]
    fn malformed_json_ids_numbers_and_page_tokens_are_rejected() {
        let cases = [
            "{",
            r#"{"items":[{"id":{"videoId":"short"},"snippet":{"title":"x"}}]}"#,
            r#"{"nextPageToken":"bad token","items":[]}"#,
        ];
        for body in cases {
            let (provider, server) = provider_with_server(vec![json_response("200 OK", body)]);
            let error = provider
                .search_at(
                    &SearchRequest::new("broken", SearchTarget::Videos),
                    1_704_067_200,
                )
                .expect_err("malformed search response should fail");
            server.finish();
            assert!(matches!(error, ProviderError::InvalidResponse(_)));
        }

        let malformed_number =
            VIDEO_RESOURCE.replace(r#""viewCount": "123456""#, r#""viewCount": -1"#);
        let (provider, server) =
            provider_with_server(vec![json_response("200 OK", &malformed_number)]);
        let error = provider
            .video_details(VIDEO_ID)
            .expect_err("negative count should fail schema parsing");
        server.finish();
        assert!(matches!(error, ProviderError::InvalidResponse(_)));
    }

    #[test]
    fn empty_or_private_video_details_are_reported() {
        let (provider, server) =
            provider_with_server(vec![json_response("200 OK", r#"{"items":[]}"#)]);
        let error = provider
            .video_details(VIDEO_ID)
            .expect_err("missing video should not produce empty details");
        server.finish();
        assert!(matches!(
            error,
            ProviderError::InvalidResponse(message) if message.contains("private")
        ));
    }

    #[test]
    fn date_and_duration_parsers_cover_valid_and_malformed_boundaries() {
        assert_eq!(
            format_rfc3339_utc(0).expect("epoch should format"),
            "1970-01-01T00:00:00Z"
        );
        assert_eq!(
            format_rfc3339_utc(951_827_696).expect("leap date should format"),
            "2000-02-29T12:34:56Z"
        );
        assert_eq!(parse_iso8601_duration("PT0S"), Some(0));
        assert_eq!(parse_iso8601_duration("PT1H2M3S"), Some(3_723));
        assert_eq!(parse_iso8601_duration("P1DT2H3M4.9S"), Some(93_784));
        for malformed in [
            "",
            "1H",
            "P",
            "PT",
            "P1DT",
            "PT1M2H",
            "P1M",
            "PT1.5M",
            "PT1S1S",
            "PT18446744073709551615H",
        ] {
            assert_eq!(parse_iso8601_duration(malformed), None, "{malformed}");
        }
        assert_eq!(parse_rfc3339_epoch("2024-13-01T00:00:00Z"), None);
    }

    #[test]
    fn capabilities_cover_official_search_and_metadata() {
        let provider =
            YouTubeOfficialProvider::new(TEST_KEY).expect("test API key should be accepted");
        assert_eq!(provider.id(), "youtube-official");
        assert_eq!(provider.display_name(), "YouTube Data API");
        let capabilities = provider.capabilities();
        assert!(capabilities.video_search);
        assert!(capabilities.channel_search);
        assert!(capabilities.pagination);
        assert!(capabilities.search_filters);
        assert!(capabilities.search_sorting);
        assert!(capabilities.video_details);
        assert!(capabilities.video_comments);
        assert!(capabilities.thumbnails);
    }

    #[test]
    fn token_cache_is_bounded() {
        let mut cache = PageTokenCache::default();
        for query in 0..(MAX_CACHED_SEARCHES + 5) {
            let request = SearchRequest::new(format!("query {query}"), SearchTarget::Videos);
            cache.remember(
                &SearchKey::from_request(&request),
                1,
                Some(PageCursor {
                    token: format!("token-{query}"),
                    published_after: None,
                }),
            );
        }
        assert_eq!(cache.searches.len(), MAX_CACHED_SEARCHES);

        let key = SearchKey::from_request(&SearchRequest::new(
            format!("query {}", MAX_CACHED_SEARCHES + 4),
            SearchTarget::Videos,
        ));
        let final_page =
            u32::try_from(MAX_TOKENS_PER_SEARCH).expect("test cache limit should fit u32") + 8;
        for page in 2..=final_page {
            cache.remember(
                &key,
                page,
                Some(PageCursor {
                    token: format!("token-{page}"),
                    published_after: None,
                }),
            );
        }
        assert!(cache.searches[&key].len() <= MAX_TOKENS_PER_SEARCH);
    }

    #[test]
    fn channel_token_cache_is_bounded_and_first_page_restarts_the_chain() {
        let mut cache = ChannelPageTokenCache::default();
        for index in 0..(MAX_CACHED_CHANNELS + 5) {
            cache.remember_uploads_playlist(&format!("UC{index:022}"), &format!("UU{index:022}"));
        }
        assert_eq!(cache.channels.len(), MAX_CACHED_CHANNELS);

        let channel_id = format!("UC{:022}", MAX_CACHED_CHANNELS + 4);
        let final_page =
            u32::try_from(MAX_TOKENS_PER_CHANNEL).expect("test cache limit should fit u32") + 8;
        for page in 1..=final_page {
            cache.remember_next_page(&channel_id, page, Some(format!("token-{page}")));
        }
        assert!(cache.channels[&channel_id].page_tokens.len() <= MAX_TOKENS_PER_CHANNEL);

        cache.remember_next_page(&channel_id, 1, Some("fresh-token".to_owned()));
        let pages = &cache.channels[&channel_id].page_tokens;
        assert_eq!(pages.len(), 1);
        assert_eq!(pages.get(&2).map(String::as_str), Some("fresh-token"));

        cache.remember_next_page(&channel_id, 2, Some("page-three-old".to_owned()));
        cache.remember_next_page(&channel_id, 3, Some("page-four-stale".to_owned()));
        cache.remember_next_page(&channel_id, 2, Some("page-three-new".to_owned()));
        let pages = &cache.channels[&channel_id].page_tokens;
        assert_eq!(pages.get(&2).map(String::as_str), Some("fresh-token"));
        assert_eq!(pages.get(&3).map(String::as_str), Some("page-three-new"));
        assert!(
            !pages.contains_key(&4),
            "replaying a page must invalidate every opaque descendant token"
        );
    }

    #[test]
    fn channel_token_cache_continuation_reads_refresh_lru_ownership() {
        let mut cache = ChannelPageTokenCache::default();
        let retained_channel = format!("UC{:022}", 0);
        for index in 0..MAX_CACHED_CHANNELS {
            let channel_id = format!("UC{index:022}");
            cache.remember_uploads_playlist(&channel_id, &format!("UU{index:022}"));
            cache.remember_next_page(&channel_id, 1, Some(format!("token-{index}")));
        }

        assert!(cache.page_context(&retained_channel, 2).is_some());
        let newest_channel = format!("UC{:022}", MAX_CACHED_CHANNELS);
        cache.remember_uploads_playlist(&newest_channel, "UU0000000000000000000032");

        assert!(
            cache.channels.contains_key(&retained_channel),
            "using a continuation must keep its opaque token chain resident"
        );
        assert!(
            !cache.channels.contains_key(&format!("UC{:022}", 1)),
            "the least recently used untouched channel should be evicted"
        );
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
                            // BSD and macOS let an accepted socket inherit the
                            // listener's non-blocking flag, while Linux does
                            // not. Clearing it keeps the blocking reads below
                            // identical on every platform.
                            Ok((stream, _)) => {
                                stream
                                    .set_nonblocking(false)
                                    .expect("mock stream should become blocking");
                                break stream;
                            }
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

    fn requests_contain_part(requests: &[String], encoded_part: &str) -> bool {
        requests
            .iter()
            .any(|request| request.contains(encoded_part))
    }

    fn query_pairs(request_target: &str) -> HashMap<String, String> {
        Url::parse(&format!("http://mock.test{request_target}"))
            .expect("captured target should be a relative URL")
            .query_pairs()
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect()
    }
}
