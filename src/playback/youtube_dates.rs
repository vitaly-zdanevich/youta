//! Lightweight, optional publication-date lookups for public YouTube episodes.
//!
//! YouTube's undocumented player endpoint supplies the same exact microformat
//! dates as yt-dlp, without starting an extractor or downloading a watch page.
//! This is only an optimization: callers must retain their extractor fallback
//! when the endpoint or schema changes.

use std::time::Duration;

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use serde::Deserialize;

use super::ytdlp::{
    MAX_PODCAST_DESCRIPTION_BYTES, MAX_PODCAST_METADATA_BYTES, YouTubeEpisodeMetadata,
};
use super::{PlaybackError, Result};

const PLAYER_ENDPOINT: &str = "https://www.youtube.com/youtubei/v1/player";
const MAX_RESPONSE_BYTES: u64 = 16 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const DATE_FIELDS: &str = "videoDetails(videoId),microformat/playerMicroformatRenderer(uploadDate,publishDate,liveBroadcastDetails,externalVideoId)";

/// Public WEB client version from yt-dlp's `INNERTUBE_CLIENTS`, release 2026.08.19.
///
/// See <https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/youtube/_base.py>.
/// A stale version must fail back to yt-dlp, never require private credentials.
const WEB_CLIENT_VERSION: &str = "2.20260708.00.00";

/// Full text and date share one response, excluding all stream formats and URLs.
const PODCAST_FIELDS: &str = "videoDetails(videoId,shortDescription),microformat/playerMicroformatRenderer(uploadDate,publishDate,liveBroadcastDetails,externalVideoId)";

/// Connection-pooled requests for exact YouTube episode dates without login.
///
/// Clones share the same HTTP agent and its idle connections. Responses are
/// limited to 16 KiB for dates or 512 KiB for full podcast metadata, with a shared
/// five-second deadline; redirects are disabled. No saved/browser
/// cookies or account credentials are loaded or required. With cookie support
/// enabled, the fresh agent may retain anonymous visitor cookies from YouTube.
#[derive(Clone)]
pub(crate) struct YouTubeDateClient {
    agent: ureq::Agent,
    endpoint: String,
}

impl YouTubeDateClient {
    /// Creates a client restricted to the fixed public HTTPS player endpoint.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(REQUEST_TIMEOUT))
                .timeout_connect(Some(Duration::from_secs(2)))
                .https_only(!cfg!(test))
                .max_redirects(0)
                .http_status_as_error(false)
                .user_agent("Mozilla/5.0")
                .build()
                .into(),
            endpoint: PLAYER_ENDPOINT.to_owned(),
        }
    }

    /// Fetches full episode text and its exact date in one public metadata request.
    ///
    /// Player `shortDescription` is the video's complete plain-text description,
    /// unlike the truncated `descriptionSnippet` returned by flat channel lists.
    /// A missing/null field requires the extractor fallback, while empty text is
    /// complete. Oversized descriptions are rejected rather than truncated.
    pub(crate) fn podcast_metadata(&self, video_id: &str) -> Result<YouTubeEpisodeMetadata> {
        let bytes =
            self.fetch_response(video_id, PODCAST_FIELDS, MAX_PODCAST_METADATA_BYTES as u64)?;
        let response = parse_response(&bytes)?;
        let published_at = response_publication_date(&response, video_id)?;
        let description = response
            .video_details
            .short_description
            .ok_or_else(|| protocol_error("YouTube did not provide a full episode description"))?;
        if description.len() > MAX_PODCAST_DESCRIPTION_BYTES {
            return Err(protocol_error(
                "YouTube full episode description exceeds the metadata limit",
            ));
        }
        Ok(YouTubeEpisodeMetadata {
            published_at: Some(published_at),
            description: Some(description),
        })
    }

    /// Fetches a public video's exact UTC publication time without media work.
    ///
    /// Live/premiere start timestamps take precedence over upload timestamps,
    /// matching yt-dlp. Date-only metadata uses UTC midnight; relative dates and
    /// timestamps without an explicit timezone are never guessed.
    ///
    /// # Errors
    ///
    /// Rejects invalid IDs before connecting, unsuccessful or oversized responses,
    /// mismatched video identities, and missing or malformed dates. Error messages
    /// exclude remote response bodies and transport details so callers can safely
    /// fall back to their existing metadata extractor.
    pub(crate) fn publication_date(&self, video_id: &str) -> Result<i64> {
        parse_publication_date(
            &self.fetch_response(video_id, DATE_FIELDS, MAX_RESPONSE_BYTES)?,
            video_id,
        )
    }

    /// Requests only the needed public fields under the existing connection/deadline policy.
    fn fetch_response(&self, video_id: &str, fields: &str, limit: u64) -> Result<Vec<u8>> {
        if video_id.len() != 11
            || !video_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(PlaybackError::InvalidValue(
                "YouTube episode date lookup needs a valid video ID".to_owned(),
            ));
        }
        let payload = serde_json::json!({
            "context": {"client": {"clientName": "WEB", "clientVersion": WEB_CLIENT_VERSION, "hl": "en"}},
            "videoId": video_id,
            "contentCheckOk": true,
            "racyCheckOk": true,
        });
        let mut response = self
            .agent
            .post(&self.endpoint)
            .query("prettyPrint", "false")
            .query("fields", fields)
            .header("Origin", "https://www.youtube.com")
            .header("Accept", "application/json")
            .send_json(&payload)
            .map_err(|_| protocol_error("YouTube publication-date request failed"))?;
        if !response.status().is_success() {
            return Err(protocol_error(&format!(
                "YouTube publication-date endpoint returned HTTP {}",
                response.status().as_u16()
            )));
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .map_err(|_| {
                protocol_error("YouTube publication-date response was unavailable or too large")
            })?;
        Ok(bytes)
    }
}

/// Selected public fields; unrelated player/media metadata is ignored.
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct PlayerResponse {
    video_details: VideoIdentity,
    microformat: Microformat,
}

/// A separate identity check prevents an incorrect video being dated.
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct VideoIdentity {
    video_id: Option<String>,
    short_description: Option<String>,
}

/// Microformat envelope supplied by the public WEB player.
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Microformat {
    player_microformat_renderer: EpisodeDates,
}

/// Original publication dates, never relative playlist labels.
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct EpisodeDates {
    external_video_id: Option<String>,
    upload_date: Option<String>,
    publish_date: Option<String>,
    live_broadcast_details: LiveDates,
}

/// Premiere and livestream release time takes precedence over upload time.
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct LiveDates {
    start_timestamp: Option<String>,
}

/// Checks every supplied identity before interpreting the matched episode dates.
fn parse_publication_date(bytes: &[u8], video_id: &str) -> Result<i64> {
    response_publication_date(&parse_response(bytes)?, video_id)
}

/// Decodes selected public fields once for either the date-only or full-text path.
fn parse_response(bytes: &[u8]) -> Result<PlayerResponse> {
    serde_json::from_slice(bytes)
        .map_err(|_| protocol_error("YouTube publication-date endpoint returned invalid JSON"))
}

/// Validates all response identities before using any associated publication metadata.
fn response_publication_date(response: &PlayerResponse, video_id: &str) -> Result<i64> {
    let dates = &response.microformat.player_microformat_renderer;
    let identities = [
        response.video_details.video_id.as_deref(),
        dates.external_video_id.as_deref(),
    ];
    if identities.iter().all(Option::is_none)
        || identities.into_iter().flatten().any(|id| id != video_id)
    {
        return Err(protocol_error(
            "YouTube publication-date response did not match the requested video",
        ));
    }
    [
        dates.live_broadcast_details.start_timestamp.as_deref(),
        dates.upload_date.as_deref(),
        dates.publish_date.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find_map(parse_exact_date)
    .ok_or_else(|| {
        protocol_error("YouTube did not provide an exact publication date for the episode")
    })
}

/// Accepts valid calendar dates or timezone-qualified timestamps within RSS bounds.
fn parse_exact_date(raw: &str) -> Option<i64> {
    if let Ok(date) = DateTime::parse_from_rfc3339(raw) {
        return (1900..=9999)
            .contains(&date.with_timezone(&Utc).year())
            .then_some(date.timestamp());
    }
    if raw.len() != 10
        || !raw.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 4 | 7) {
                byte == b'-'
            } else {
                byte.is_ascii_digit()
            }
        })
    {
        return None;
    }
    let date = NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()?;
    if !(1900..=9999).contains(&date.year()) {
        return None;
    }
    Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp())
}

/// Builds local error text without exposing upstream transport diagnostics.
fn protocol_error(message: &str) -> PlaybackError {
    PlaybackError::Protocol(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread::JoinHandle;

    /// Serves one bounded local response and returns the exact incoming request.
    fn mock_response(status: &str, body: &str) -> (YouTubeDateClient, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/player", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut bytes = [0; 4096];
            loop {
                let count = socket.read(&mut bytes).unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&bytes[..count]);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let _ = socket.write_all(response.as_bytes());
            String::from_utf8(request).unwrap()
        });
        let mut client = YouTubeDateClient::new();
        client.endpoint = endpoint;
        (client, worker)
    }

    #[test]
    fn podcast_metadata_keeps_full_unicode_description_in_one_anonymous_request() {
        let full = format!(
            "{}END OF FULL DESCRIPTION",
            "История & <chapter> 世界\n".repeat(800)
        );
        let body = serde_json::json!({
            "videoDetails": {"videoId": "00000000000", "shortDescription": full},
            "microformat": {"playerMicroformatRenderer": {"uploadDate": "2024-02-29"}}
        })
        .to_string();
        assert!(body.len() > 16 * 1024);
        let (client, worker) = mock_response("200 OK", &body);
        let metadata = client.podcast_metadata("00000000000");
        let request = worker.join().unwrap();
        let metadata = metadata.expect("full descriptions use a bounded larger response budget");
        assert_eq!(metadata.description.as_deref(), Some(full.as_str()));
        assert_eq!(metadata.published_at, Some(1_709_164_800));
        assert!(request.contains("shortDescription"));
        assert!(!request.to_ascii_lowercase().contains("\r\ncookie:"));
        assert!(!request.to_ascii_lowercase().contains("\r\nauthorization:"));
    }

    #[test]
    fn podcast_metadata_distinguishes_known_empty_from_missing_text() {
        for description in [serde_json::Value::Null, serde_json::json!("")] {
            let body = serde_json::json!({
                "videoDetails": {"videoId": "00000000000", "shortDescription": description},
                "microformat": {"playerMicroformatRenderer": {"uploadDate": "2024-02-29"}}
            })
            .to_string();
            let (client, worker) = mock_response("200 OK", &body);
            let result = client.podcast_metadata("00000000000");
            worker.join().unwrap();
            if description.is_null() {
                assert!(
                    result.is_err(),
                    "missing description must use the extractor fallback"
                );
            } else {
                assert_eq!(result.unwrap().description.as_deref(), Some(""));
            }
        }
    }

    #[test]
    fn podcast_metadata_rejects_wrong_identity_and_oversized_full_text() {
        for (id, description) in [
            ("00000000001", "Wrong item".to_owned()),
            ("00000000000", "x".repeat(MAX_PODCAST_DESCRIPTION_BYTES + 1)),
            ("00000000000", "x".repeat(MAX_PODCAST_METADATA_BYTES + 1)),
        ] {
            let body = serde_json::json!({
                "videoDetails": {"videoId": id, "shortDescription": description},
                "microformat": {"playerMicroformatRenderer": {"uploadDate": "2024-02-29"}}
            })
            .to_string();
            let (client, worker) = mock_response("200 OK", &body);
            assert!(client.podcast_metadata("00000000000").is_err());
            worker.join().unwrap();
        }
    }

    #[test]
    #[ignore = "explicit public-network smoke test; no saved credentials or media"]
    fn live_public_player_dates_match_known_video_metadata() {
        let client = YouTubeDateClient::new();
        for (id, expected) in [
            ("jNQXAC9IVRw", 1_114_313_512),
            ("YE7VzlLtp-4", 1_212_060_266),
        ] {
            let started = std::time::Instant::now();
            assert_eq!(client.publication_date(id).unwrap(), expected);
            eprintln!("Public date {id}: {:?}", started.elapsed());
        }
    }

    #[test]
    fn player_metadata_uses_a_tiny_request_without_credentials_or_media() {
        let (client, server) = mock_response(
            "200 OK",
            r#"{"videoDetails":{"videoId":"jNQXAC9IVRw"},"microformat":{"playerMicroformatRenderer":{"externalVideoId":"jNQXAC9IVRw","uploadDate":"2005-04-23T20:31:52-07:00"}}}"#,
        );
        assert_eq!(
            client.publication_date("jNQXAC9IVRw").unwrap(),
            1_114_313_512
        );
        let request = server.join().unwrap();
        let (headers, body) = request.split_once("\r\n\r\n").unwrap();
        assert!(headers.starts_with("POST /player?"));
        assert!(headers.contains("fields="));
        assert!(!headers.to_ascii_lowercase().contains("cookie:"));
        assert!(!headers.to_ascii_lowercase().contains("authorization:"));
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["videoId"], "jNQXAC9IVRw");
        assert_eq!(body["context"]["client"]["clientName"], "WEB");
        assert_eq!(body["context"]["client"]["hl"], "en");
        assert_eq!(client.agent.config().max_redirects(), 0);
        assert_eq!(
            client.agent.config().timeouts().global,
            Some(Duration::from_secs(5))
        );
    }

    #[test]
    fn live_start_and_date_only_metadata_keep_exact_publication_dates() {
        for (metadata, expected) in [
            (
                r#""uploadDate":"2005-04-23T20:31:52-07:00","liveBroadcastDetails":{"startTimestamp":"2024-01-02T03:04:05Z"}"#,
                1_704_164_645,
            ),
            (r#""uploadDate":"2005-04-24""#, 1_114_300_800),
            (
                r#""publishDate":"2005-04-23T20:31:52-07:00""#,
                1_114_313_512,
            ),
        ] {
            let body = format!(
                r#"{{"microformat":{{"playerMicroformatRenderer":{{"externalVideoId":"jNQXAC9IVRw",{metadata}}}}}}}"#
            );
            let (client, server) = mock_response("200 OK", &body);
            assert_eq!(client.publication_date("jNQXAC9IVRw").unwrap(), expected);
            server.join().unwrap();
        }
    }

    #[test]
    fn malformed_or_mismatched_metadata_never_assigns_a_date() {
        for body in [
            r#"{}"#,
            r#"{"microformat":{"playerMicroformatRenderer":{"uploadDate":"2005-04-24"}}}"#,
            r#"{"videoDetails":{"videoId":"YE7VzlLtp-4"},"microformat":{"playerMicroformatRenderer":{"externalVideoId":"jNQXAC9IVRw","uploadDate":"2005-04-24"}}}"#,
            r#"{"videoDetails":{"videoId":"jNQXAC9IVRw"},"microformat":{"playerMicroformatRenderer":{"externalVideoId":"YE7VzlLtp-4","uploadDate":"2005-04-24"}}}"#,
            r#"{"videoDetails":{"videoId":"jNQXAC9IVRw"},"microformat":{"playerMicroformatRenderer":{"uploadDate":"2023-02-29"}}}"#,
            r#"{"videoDetails":{"videoId":"jNQXAC9IVRw"},"microformat":{"playerMicroformatRenderer":{"uploadDate":"2005-04-24T12:00:00"}}}"#,
            r#"{"videoDetails":{"videoId":"jNQXAC9IVRw"},"microformat":{"playerMicroformatRenderer":{"uploadDate":"1800-01-01"}}}"#,
            "untrusted response body",
        ] {
            let (client, server) = mock_response("200 OK", body);
            assert!(
                client.publication_date("jNQXAC9IVRw").is_err(),
                "accepted {body}"
            );
            server.join().unwrap();
        }
    }

    #[test]
    fn invalid_video_ids_are_rejected_before_network_access() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut client = YouTubeDateClient::new();
        client.endpoint = format!("http://{}/player", listener.local_addr().unwrap());
        for id in [
            "",
            "too-short",
            "jNQXAC9IVRw?secret",
            "jNQXAC9IVR/",
            "éNQXAC9IVRw",
        ] {
            assert!(client.publication_date(id).is_err());
        }
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn oversized_and_http_error_responses_are_bounded_and_sanitized() {
        for (status, body) in [
            ("200 OK", "x".repeat(16 * 1024 + 1)),
            ("403 Forbidden", "private_remote_error_detail".to_owned()),
            ("302 Found", "private_remote_error_detail".to_owned()),
        ] {
            let (client, server) = mock_response(status, &body);
            let error = client
                .publication_date("jNQXAC9IVRw")
                .unwrap_err()
                .to_string();
            assert!(!error.contains("private_remote_error_detail"));
            assert!(!error.contains("127.0.0.1"));
            assert!(error.len() < 200);
            server.join().unwrap();
        }
    }
}
