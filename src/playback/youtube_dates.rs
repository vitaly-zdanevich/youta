//! Lightweight, optional publication-date lookups for public YouTube episodes.
//!
//! YouTube's undocumented player endpoint supplies the same exact microformat
//! dates as yt-dlp, without starting an extractor or downloading a watch page.
//! This is only an optimization: callers must retain their extractor fallback
//! when the endpoint or schema changes.

use std::time::Duration;

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use serde::Deserialize;

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

/// Connection-pooled requests for exact YouTube episode dates without login.
///
/// Clones share the same HTTP agent and its idle connections. Responses are
/// limited to 16 KiB and five seconds; redirects are disabled. No saved/browser
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
            .query("fields", DATE_FIELDS)
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
            .limit(MAX_RESPONSE_BYTES)
            .read_to_vec()
            .map_err(|_| {
                protocol_error("YouTube publication-date response was unavailable or too large")
            })?;
        parse_publication_date(&bytes, video_id)
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
    let response: PlayerResponse = serde_json::from_slice(bytes)
        .map_err(|_| protocol_error("YouTube publication-date endpoint returned invalid JSON"))?;
    let dates = response.microformat.player_microformat_renderer;
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
        dates.live_broadcast_details.start_timestamp,
        dates.upload_date,
        dates.publish_date,
    ]
    .into_iter()
    .flatten()
    .find_map(|raw| parse_exact_date(&raw))
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
