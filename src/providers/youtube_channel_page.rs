//! Bounded parser for public metadata on a `YouTube` channel About page.
//!
//! The official Data API exposes channel creation time, country, public video
//! count, and aggregate views, but it does not expose the public website/social
//! links configured by a channel owner. `yt-dlp` likewise omits those About-page
//! links from its channel playlist JSON. This adapter reads only the embedded
//! `ytInitialData` object and is intended as a best-effort supplement to a
//! provider's [`super::ChannelDetails`].

use std::collections::BTreeSet;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value};
use url::Url;

use super::{
    ChannelDetails, ChannelExternalLink, ChannelExternalLinkKind, DEFAULT_REQUEST_TIMEOUT,
    ProviderError,
};

const YOUTUBE_BASE_URL: &str = "https://www.youtube.com/";
const MAX_HTML_BYTES: usize = 8 * 1024 * 1024;
const MAX_EXTERNAL_LINKS: usize = 32;
const MAX_LINK_LABEL_CHARS: usize = 128;
const MAX_VISITED_JSON_VALUES: usize = 100_000;

/// Public channel fields extracted from `YouTube`'s bounded About-page payload.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct YouTubeChannelPageMetadata {
    /// Exact channel ID carried by the About-page model.
    pub channel_id: String,
    /// Human-readable country label rendered by `YouTube`.
    pub country: Option<String>,
    /// Channel creation date at midnight UTC, when the English date parses.
    pub joined_at: Option<i64>,
    /// Public video count rendered by `YouTube`.
    pub video_count: Option<u64>,
    /// Aggregate public video views rendered by `YouTube`.
    pub total_view_count: Option<u64>,
    /// Public websites and social profiles configured by the channel owner.
    pub external_links: Vec<ChannelExternalLink>,
    /// Whether links beyond the in-memory safety bound were omitted.
    pub external_links_truncated: bool,
}

impl YouTubeChannelPageMetadata {
    /// Fills optional fields absent from a provider's full channel details.
    ///
    /// About-page links are used only when the provider did not already return
    /// links. A human-readable About-page country replaces a two-letter country
    /// code, while all other nonempty provider values retain precedence.
    pub fn merge_missing_into(self, details: &mut ChannelDetails) {
        if details.summary.channel_id != self.channel_id {
            return;
        }
        details.summary.created_at = details.summary.created_at.or(self.joined_at);
        details.summary.video_count = details.summary.video_count.or(self.video_count);
        details.total_view_count = details.total_view_count.or(self.total_view_count);
        if self.country.is_some()
            && details.country.as_ref().is_none_or(|country| {
                country.len() == 2 && country.bytes().all(|byte| byte.is_ascii_uppercase())
            })
        {
            details.country = self.country;
        }
        if details.external_links.is_empty() {
            details.external_links = self.external_links;
            details.external_links_truncated = self.external_links_truncated;
        }
    }
}

/// Blocking client for one public `YouTube` channel About page.
#[derive(Clone)]
pub struct YouTubeChannelPageClient {
    agent: ureq::Agent,
    base_url: Url,
    max_html_bytes: usize,
}

impl YouTubeChannelPageClient {
    /// Creates a client using `YouTube`'s public HTTPS website.
    ///
    /// # Panics
    ///
    /// Panics only when the compile-time `YouTube` base URL is invalid.
    #[must_use]
    pub fn new() -> Self {
        Self {
            agent: page_agent(DEFAULT_REQUEST_TIMEOUT),
            base_url: Url::parse(YOUTUBE_BASE_URL)
                .expect("the compile-time YouTube base URL is valid"),
            max_html_bytes: MAX_HTML_BYTES,
        }
    }

    /// Creates a client with an explicit timeout and HTML response bound.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] for a zero timeout or response
    /// bound.
    ///
    /// # Panics
    ///
    /// Panics only when the compile-time `YouTube` base URL is invalid.
    pub fn with_options(timeout: Duration, max_html_bytes: usize) -> Result<Self, ProviderError> {
        if timeout.is_zero() || max_html_bytes == 0 {
            return Err(ProviderError::InvalidRequest(
                "YouTube channel-page timeout and response limit must be greater than zero"
                    .to_owned(),
            ));
        }
        Ok(Self {
            agent: page_agent(timeout),
            base_url: Url::parse(YOUTUBE_BASE_URL)
                .expect("the compile-time YouTube base URL is valid"),
            max_html_bytes,
        })
    }

    /// Loads public About-page metadata for one exact `UC…` channel ID.
    ///
    /// This is a best-effort supplement: callers should retain official API or
    /// Invidious metadata when this page changes shape or cannot be reached.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid channel ID, transport failure,
    /// unsuccessful or oversized response, missing initial data, mismatched
    /// channel identity, or malformed bounded metadata.
    pub fn channel_metadata(
        &self,
        channel_id: &str,
    ) -> Result<YouTubeChannelPageMetadata, ProviderError> {
        validate_channel_id(channel_id)?;
        let mut url = self
            .base_url
            .join(&format!("channel/{channel_id}/about"))
            .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
        url.query_pairs_mut().append_pair("hl", "en");
        let html = get_bounded_html(&self.agent, &url, self.max_html_bytes)?;
        let metadata = parse_channel_about_html(&html)?;
        if metadata.channel_id != channel_id {
            return Err(ProviderError::InvalidResponse(
                "YouTube About page channel ID does not match the requested channel".to_owned(),
            ));
        }
        Ok(metadata)
    }

    #[cfg(test)]
    fn with_base_url(base_url: Url, max_html_bytes: usize) -> Self {
        Self {
            agent: page_agent(DEFAULT_REQUEST_TIMEOUT),
            base_url,
            max_html_bytes,
        }
    }
}

impl Default for YouTubeChannelPageClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Parses one bounded `YouTube` channel About-page document.
///
/// # Errors
///
/// Returns [`ProviderError::InvalidResponse`] when the document has no
/// parseable `ytInitialData` assignment, no About-page model, or invalid
/// requested metadata.
pub fn parse_channel_about_html(html: &str) -> Result<YouTubeChannelPageMetadata, ProviderError> {
    let initial_data = parse_initial_data(html)?;
    let model = find_about_model(&initial_data)?.ok_or_else(|| {
        ProviderError::InvalidResponse("YouTube channel page omitted its About metadata".to_owned())
    })?;
    normalize_about_model(model)
}

fn get_bounded_html(agent: &ureq::Agent, url: &Url, limit: usize) -> Result<String, ProviderError> {
    let mut response = agent
        .get(url.as_str())
        .header("Accept", "text/html")
        .header("Accept-Language", "en-US,en;q=0.9")
        .call()
        .map_err(map_page_error)?;
    if response
        .body()
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(ProviderError::ResponseTooLarge { limit });
    }
    let bytes = response
        .body_mut()
        .with_config()
        .limit(u64::try_from(limit.saturating_add(1)).unwrap_or(u64::MAX))
        .read_to_vec()
        .map_err(|error| match error {
            ureq::Error::BodyExceedsLimit(_) => ProviderError::ResponseTooLarge { limit },
            other => ProviderError::Transport(other.to_string()),
        })?;
    if bytes.len() > limit {
        return Err(ProviderError::ResponseTooLarge { limit });
    }
    String::from_utf8(bytes)
        .map_err(|_| ProviderError::InvalidResponse("YouTube returned non-UTF-8 HTML".to_owned()))
}

fn page_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
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

fn map_page_error(error: ureq::Error) -> ProviderError {
    match error {
        ureq::Error::StatusCode(code) => ProviderError::HttpStatus(code),
        ureq::Error::BodyExceedsLimit(limit) => ProviderError::ResponseTooLarge {
            limit: usize::try_from(limit).unwrap_or(usize::MAX),
        },
        other => ProviderError::Transport(other.to_string()),
    }
}

fn parse_initial_data(html: &str) -> Result<Value, ProviderError> {
    const MARKERS: [&str; 3] = [
        "var ytInitialData =",
        "window[\"ytInitialData\"] =",
        "ytInitialData =",
    ];
    for marker in MARKERS {
        let Some(after_marker) = html.split_once(marker).map(|(_, suffix)| suffix) else {
            continue;
        };
        let Some(json_start) = after_marker.find('{') else {
            continue;
        };
        let mut deserializer = serde_json::Deserializer::from_str(&after_marker[json_start..]);
        return Value::deserialize(&mut deserializer)
            .map_err(|error| ProviderError::InvalidResponse(error.to_string()));
    }
    Err(ProviderError::InvalidResponse(
        "YouTube channel page omitted ytInitialData".to_owned(),
    ))
}

fn find_about_model(root: &Value) -> Result<Option<&Map<String, Value>>, ProviderError> {
    let mut stack = vec![root];
    let mut visited = 0usize;
    while let Some(value) = stack.pop() {
        visited = visited.saturating_add(1);
        if visited > MAX_VISITED_JSON_VALUES {
            return Err(ProviderError::InvalidResponse(
                "YouTube channel metadata exceeds its traversal bound".to_owned(),
            ));
        }
        match value {
            Value::Object(object) => {
                if let Some(model) = object
                    .get("aboutChannelViewModel")
                    .and_then(Value::as_object)
                {
                    return Ok(Some(model));
                }
                stack.extend(object.values());
            }
            Value::Array(values) => stack.extend(values),
            _ => {}
        }
    }
    Ok(None)
}

fn normalize_about_model(
    model: &Map<String, Value>,
) -> Result<YouTubeChannelPageMetadata, ProviderError> {
    let channel_id = model
        .get("channelId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ProviderError::InvalidResponse("YouTube About page omitted channelId".to_owned())
        })?;
    validate_channel_id(channel_id).map_err(|_| {
        ProviderError::InvalidResponse(
            "YouTube About page contains an invalid channelId".to_owned(),
        )
    })?;
    let country = bounded_text(model.get("country").and_then(Value::as_str), 128);
    let joined_text = model.get("joinedDateText").and_then(|value| {
        value
            .as_str()
            .or_else(|| value.get("content").and_then(Value::as_str))
    });
    let links = model
        .get("links")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let external_links_truncated = links.len() > MAX_EXTERNAL_LINKS;
    let mut external_links = Vec::with_capacity(links.len().min(MAX_EXTERNAL_LINKS));
    let mut seen = BTreeSet::new();
    for value in links.iter().take(MAX_EXTERNAL_LINKS) {
        if let Some(link) = normalize_external_link(value)
            && seen.insert(link.url.as_str().to_owned())
        {
            external_links.push(link);
        }
    }
    Ok(YouTubeChannelPageMetadata {
        channel_id: channel_id.to_owned(),
        country,
        joined_at: joined_text.and_then(parse_english_joined_date),
        video_count: display_count(model.get("videoCountText"))?,
        total_view_count: display_count(model.get("viewCountText"))?,
        external_links,
        external_links_truncated,
    })
}

fn normalize_external_link(value: &Value) -> Option<ChannelExternalLink> {
    let model = value
        .get("channelExternalLinkViewModel")
        .filter(|model| model.is_object())?;
    let raw_url = model
        .pointer(
            "/link/commandRuns/0/onTap/innertubeCommand/commandMetadata/webCommandMetadata/url",
        )
        .or_else(|| model.pointer("/link/commandRuns/0/onTap/innertubeCommand/urlEndpoint/url"))
        .and_then(Value::as_str);
    let url = raw_url.and_then(direct_external_url)?;
    let label = bounded_text(
        model.pointer("/title/content").and_then(Value::as_str),
        MAX_LINK_LABEL_CHARS,
    )
    .or_else(|| {
        bounded_text(
            model.pointer("/link/content").and_then(Value::as_str),
            MAX_LINK_LABEL_CHARS,
        )
    })
    .unwrap_or_else(|| url.host_str().unwrap_or("Website").to_owned());
    let kind = external_link_kind(&url);
    Some(ChannelExternalLink { label, url, kind })
}

fn direct_external_url(raw: &str) -> Option<Url> {
    let parsed = Url::parse(raw).ok()?;
    let target = if is_youtube_host(parsed.host_str()?)
        && parsed.path().trim_end_matches('/') == "/redirect"
    {
        parsed
            .query_pairs()
            .find_map(|(name, value)| {
                matches!(name.as_ref(), "q" | "url").then(|| value.into_owned())
            })
            .and_then(|target| Url::parse(&target).ok())?
    } else {
        parsed
    };
    (matches!(target.scheme(), "http" | "https")
        && target.host_str().is_some()
        && target.username().is_empty()
        && target.password().is_none())
    .then_some(target)
}

fn external_link_kind(url: &Url) -> ChannelExternalLinkKind {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if host_matches(&host, "t.me") || host_matches(&host, "telegram.me") {
        ChannelExternalLinkKind::Telegram
    } else if host_matches(&host, "facebook.com") || host_matches(&host, "fb.com") {
        ChannelExternalLinkKind::Facebook
    } else if host_matches(&host, "twitter.com") || host_matches(&host, "x.com") {
        ChannelExternalLinkKind::XTwitter
    } else if host_matches(&host, "tiktok.com") {
        ChannelExternalLinkKind::TikTok
    } else if host_matches(&host, "instagram.com") {
        ChannelExternalLinkKind::Instagram
    } else if is_youtube_host(&host) || host_matches(&host, "youtu.be") {
        ChannelExternalLinkKind::YouTube
    } else {
        ChannelExternalLinkKind::Website
    }
}

fn host_matches(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

fn is_youtube_host(host: &str) -> bool {
    host_matches(&host.to_ascii_lowercase(), "youtube.com")
}

fn display_count(value: Option<&Value>) -> Result<Option<u64>, ProviderError> {
    let text = value.and_then(|value| {
        value
            .as_str()
            .or_else(|| value.get("content").and_then(Value::as_str))
            .or_else(|| value.get("simpleText").and_then(Value::as_str))
    });
    let Some(text) = text else {
        return Ok(None);
    };
    let digits = text
        .bytes()
        .filter(u8::is_ascii_digit)
        .map(char::from)
        .collect::<String>();
    if digits.is_empty() {
        return Ok(None);
    }
    digits
        .parse::<u64>()
        .map(Some)
        .map_err(|_| ProviderError::InvalidResponse("YouTube channel count exceeds u64".to_owned()))
}

fn bounded_text(value: Option<&str>, max_chars: usize) -> Option<String> {
    let normalized = value?.split_whitespace().collect::<Vec<_>>().join(" ");
    (!normalized.is_empty()).then(|| normalized.chars().take(max_chars).collect())
}

fn parse_english_joined_date(value: &str) -> Option<i64> {
    let mut parts = value
        .trim()
        .strip_prefix("Joined ")?
        .split_ascii_whitespace();
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let day = parts.next()?.trim_end_matches(',').parse::<u32>().ok()?;
    let year = parts.next()?.parse::<i32>().ok()?;
    if parts.next().is_some() || !valid_date(year, month, day) {
        return None;
    }
    days_from_civil(year, month, day).checked_mul(86_400)
}

fn valid_date(year: i32, month: u32, day: u32) -> bool {
    if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) || day == 0 {
        return false;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = [
        31,
        28 + u32::from(leap),
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    day <= month_days[usize::try_from(month - 1).ok().unwrap_or_default()]
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let adjusted_year = i64::from(year) - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn validate_channel_id(channel_id: &str) -> Result<(), ProviderError> {
    if channel_id.len() == 24
        && channel_id.starts_with("UC")
        && channel_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Ok(())
    } else {
        Err(ProviderError::InvalidRequest(
            "YouTube channel ID must be a 24-character UC identifier".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread::{self, JoinHandle};

    use super::*;

    const CHANNEL_ID: &str = "UC_x5XG1OV2P6uZZ5FSM9Ttw";
    const ABOUT_FIXTURE: &str = r#"
      <html><script>
      var ytInitialData = {
        "contents": {"aboutChannelRenderer": {"metadata": {
          "aboutChannelViewModel": {
            "country": "Ukraine",
            "viewCountText": "1,094,367,204 views",
            "joinedDateText": {"content": "Joined Apr 24, 2014"},
            "channelId": "UC_x5XG1OV2P6uZZ5FSM9Ttw",
            "videoCountText": "3,977 videos",
            "links": [
              {"channelExternalLinkViewModel": {
                "title": {"content": "Telegram"},
                "link": {"content": "t.me/example", "commandRuns": [{"onTap": {
                  "innertubeCommand": {"commandMetadata": {"webCommandMetadata": {
                    "url": "https://www.youtube.com/redirect?event=channel_description&q=https%3A%2F%2Ft.me%2Fexample"
                  }}}
                }}]}
              }},
              {"channelExternalLinkViewModel": {
                "title": {"content": "Website"},
                "link": {"commandRuns": [{"onTap": {"innertubeCommand": {
                  "commandMetadata": {"webCommandMetadata": {
                    "url": "https://example.org/about"
                  }}
                }}}]}
              }}
            ]
          }
        }}}
      };
      </script></html>
    "#;

    #[test]
    fn about_fixture_maps_counts_date_country_and_direct_links() {
        let metadata =
            parse_channel_about_html(ABOUT_FIXTURE).expect("fixture metadata should parse");

        assert_eq!(metadata.channel_id, CHANNEL_ID);
        assert_eq!(metadata.country.as_deref(), Some("Ukraine"));
        assert_eq!(metadata.joined_at, Some(1_398_297_600));
        assert_eq!(metadata.video_count, Some(3_977));
        assert_eq!(metadata.total_view_count, Some(1_094_367_204));
        assert_eq!(metadata.external_links.len(), 2);
        assert_eq!(
            metadata.external_links[0],
            ChannelExternalLink {
                label: "Telegram".to_owned(),
                url: Url::parse("https://t.me/example").expect("fixture URL"),
                kind: ChannelExternalLinkKind::Telegram,
            }
        );
        assert_eq!(
            metadata.external_links[1].kind,
            ChannelExternalLinkKind::Website
        );
        assert!(!metadata.external_links_truncated);
    }

    #[test]
    fn client_requests_the_english_about_page_and_parses_fixture() {
        let (base_url, server) = mock_html_server(ABOUT_FIXTURE);
        let client = YouTubeChannelPageClient::with_base_url(base_url, MAX_HTML_BYTES);

        let metadata = client
            .channel_metadata(CHANNEL_ID)
            .expect("mock About page should load");
        let target = server.join().expect("mock server should stop");

        assert_eq!(metadata.channel_id, CHANNEL_ID);
        assert_eq!(
            target,
            format!("/channel/{CHANNEL_ID}/about?hl=en"),
            "the fallback must request the stable ID route in English"
        );
    }

    #[test]
    fn client_rejects_an_oversized_about_page() {
        let (base_url, server) = mock_html_server(ABOUT_FIXTURE);
        let client = YouTubeChannelPageClient::with_base_url(base_url, 64);

        assert!(matches!(
            client.channel_metadata(CHANNEL_ID),
            Err(ProviderError::ResponseTooLarge { limit: 64 })
        ));
        server.join().expect("mock server should stop");
    }

    #[test]
    fn unsafe_and_malformed_external_links_are_skipped() {
        for raw in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://user:secret@example.org/",
            "https://www.youtube.com/redirect?event=channel_description",
        ] {
            assert_eq!(direct_external_url(raw), None);
        }
    }

    #[test]
    fn merge_fills_missing_provider_fields_without_replacing_core_metadata() {
        let metadata =
            parse_channel_about_html(ABOUT_FIXTURE).expect("fixture metadata should parse");
        let mut details = ChannelDetails {
            summary: crate::providers::ChannelSummary {
                channel_id: CHANNEL_ID.to_owned(),
                name: "API name".to_owned(),
                description: "API description".to_owned(),
                subscriber_count: Some(1_850_000),
                video_count: None,
                created_at: None,
                auto_generated: false,
                thumbnails: Vec::new(),
                webpage_url: Url::parse(&format!("https://www.youtube.com/channel/{CHANNEL_ID}"))
                    .ok(),
            },
            total_view_count: None,
            country: Some("UA".to_owned()),
            external_links: Vec::new(),
            external_links_truncated: false,
        };

        metadata.merge_missing_into(&mut details);

        assert_eq!(details.summary.name, "API name");
        assert_eq!(details.summary.subscriber_count, Some(1_850_000));
        assert_eq!(details.summary.video_count, Some(3_977));
        assert_eq!(details.summary.created_at, Some(1_398_297_600));
        assert_eq!(details.total_view_count, Some(1_094_367_204));
        assert_eq!(details.country.as_deref(), Some("Ukraine"));
        assert_eq!(details.external_links.len(), 2);
    }

    #[test]
    fn common_social_hosts_are_classified_without_matching_suffix_attacks() {
        let kind =
            |url: &str| external_link_kind(&Url::parse(url).expect("classification fixture URL"));
        assert_eq!(
            kind("https://t.me/example"),
            ChannelExternalLinkKind::Telegram
        );
        assert_eq!(
            kind("https://www.facebook.com/example"),
            ChannelExternalLinkKind::Facebook
        );
        assert_eq!(
            kind("https://x.com/example"),
            ChannelExternalLinkKind::XTwitter
        );
        assert_eq!(
            kind("https://twitter.com/example"),
            ChannelExternalLinkKind::XTwitter
        );
        assert_eq!(
            kind("https://tiktok.com/@example"),
            ChannelExternalLinkKind::TikTok
        );
        assert_eq!(
            kind("https://instagram.com/example"),
            ChannelExternalLinkKind::Instagram
        );
        assert_eq!(
            kind("https://twitter.com.evil.example/"),
            ChannelExternalLinkKind::Website
        );
    }

    #[test]
    fn joined_date_parser_validates_calendar_dates() {
        assert_eq!(parse_english_joined_date("Joined Jan 1, 1970"), Some(0));
        assert_eq!(
            parse_english_joined_date("Joined Feb 29, 2000"),
            Some(951_782_400)
        );
        assert_eq!(parse_english_joined_date("Joined Feb 29, 2001"), None);
        assert_eq!(parse_english_joined_date("Joined yesterday"), None);
    }

    fn mock_html_server(body: &'static str) -> (Url, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock server should bind");
        let address = listener.local_addr().expect("mock address should exist");
        let thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("mock should accept one request");
            let target = request_target(&stream);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("mock should write response");
            stream.flush().expect("mock should flush response");
            target
        });
        (
            Url::parse(&format!("http://{address}/")).expect("mock URL should parse"),
            thread,
        )
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
}
