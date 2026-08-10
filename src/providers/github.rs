//! Bounded access to Youta's public GitHub commit history.
//!
//! The provider compares an embedded build commit with `main` instead of
//! reading an unbounded repository log. This lets the caller distinguish
//! genuinely newer commits from unrelated history after a branch divergence.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use url::Url;

use super::{DEFAULT_MAX_JSON_BYTES, DEFAULT_REQUEST_TIMEOUT, ProviderError, validate_base_url};

/// GitHub REST API origin used outside tests.
pub const DEFAULT_GITHUB_API_URL: &str = "https://api.github.com/";

/// Maximum number of newer commits returned to the user interface.
pub const MAX_RECENT_COMMITS: usize = 10;

/// Maximum encoded byte length accepted for one full commit message.
pub const MAX_COMMIT_MESSAGE_BYTES: usize = 64 * 1024;

/// Maximum Unicode scalar count accepted for one full commit message.
pub const MAX_COMMIT_MESSAGE_CHARS: usize = 32 * 1024;

const MAX_CONFIGURED_JSON_BYTES: usize = 8 * 1024 * 1024;
const MAX_TIMESTAMP_BYTES: usize = 64;
const MAX_DEFAULT_COMPARE_COMMITS: usize = 250;
const MAX_TOTAL_COMPARE_COMMITS: usize = 1_000_000;
const REPOSITORY_COMPARE_PATH: &str = "repos/vitaly-zdanevich/youta/compare";

/// One validated commit newer than the embedded build revision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GitHubCommit {
    /// Full 40-character Git object identifier.
    pub sha: String,
    /// GitHub's RFC 3339 committer timestamp.
    pub committed_at: String,
    /// Complete commit subject and body, with line breaks preserved.
    pub message: String,
}

/// Blocking client for Youta's public GitHub comparison endpoint.
#[derive(Clone)]
pub struct GitHubCommitClient {
    api_base_url: Url,
    agent: ureq::Agent,
    max_json_bytes: usize,
}

impl Default for GitHubCommitClient {
    fn default() -> Self {
        Self::with_options(
            Url::parse(DEFAULT_GITHUB_API_URL).expect("the built-in GitHub API URL is valid"),
            DEFAULT_REQUEST_TIMEOUT,
            DEFAULT_MAX_JSON_BYTES,
        )
        .expect("the built-in GitHub client configuration is valid")
    }
}

impl GitHubCommitClient {
    /// Creates a client for GitHub's public API.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a client with injectable transport limits and API origin.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the base URL is unsafe, the timeout is
    /// zero, or the response limit falls outside the supported range.
    pub fn with_options(
        api_base_url: Url,
        timeout: Duration,
        max_json_bytes: usize,
    ) -> Result<Self, ProviderError> {
        let api_base_url = validate_base_url(api_base_url)?;
        if timeout.is_zero() {
            return Err(ProviderError::InvalidRequest(
                "GitHub timeout must be greater than zero".to_owned(),
            ));
        }
        if !(1..=MAX_CONFIGURED_JSON_BYTES).contains(&max_json_bytes) {
            return Err(ProviderError::InvalidRequest(format!(
                "GitHub JSON response limit must be between 1 and {MAX_CONFIGURED_JSON_BYTES} bytes"
            )));
        }
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .user_agent(concat!(
                "youta/",
                env!("CARGO_PKG_VERSION"),
                " (+",
                env!("CARGO_PKG_REPOSITORY"),
                ")"
            ))
            .build()
            .into();
        Ok(Self {
            api_base_url,
            agent,
            max_json_bytes,
        })
    }

    /// Returns at most ten commits which `main` contains after `base_sha`.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] for an invalid revision, failed bounded
    /// request, unsafe response field, or non-descendant `main` history.
    pub fn commits_newer_than(&self, base_sha: &str) -> Result<Vec<GitHubCommit>, ProviderError> {
        let base_sha = validate_sha(base_sha, "embedded build commit")?;
        let first = self.fetch_comparison(&base_sha, None)?;
        match comparison_relation(&first)? {
            ComparisonRelation::Identical => Ok(Vec::new()),
            ComparisonRelation::Ahead => {
                let total_commits = validated_total_commits(&first)?;
                let raw_commits = if total_commits <= first.commits.len() {
                    first
                        .commits
                        .into_iter()
                        .rev()
                        .take(MAX_RECENT_COMMITS)
                        .collect::<Vec<_>>()
                } else {
                    self.fetch_newest_page_commits(&base_sha, total_commits)?
                };
                validate_commits(raw_commits)
            }
        }
    }

    fn fetch_newest_page_commits(
        &self,
        base_sha: &str,
        total_commits: usize,
    ) -> Result<Vec<RawCommit>, ProviderError> {
        let last_page = total_commits.div_ceil(MAX_RECENT_COMMITS);
        let last_page_size = total_commits % MAX_RECENT_COMMITS;
        let last_page_size = if last_page_size == 0 {
            MAX_RECENT_COMMITS
        } else {
            last_page_size
        };
        let mut raw_commits = if last_page_size < MAX_RECENT_COMMITS && last_page > 1 {
            self.fetch_validated_page(base_sha, total_commits, last_page - 1, MAX_RECENT_COMMITS)?
                .commits
        } else {
            Vec::new()
        };
        let last = self.fetch_validated_page(base_sha, total_commits, last_page, last_page_size)?;
        raw_commits.extend(last.commits);
        Ok(raw_commits
            .into_iter()
            .rev()
            .take(MAX_RECENT_COMMITS)
            .collect())
    }

    fn fetch_validated_page(
        &self,
        base_sha: &str,
        expected_total: usize,
        page: usize,
        expected_commits: usize,
    ) -> Result<RawCompareResponse, ProviderError> {
        let response = self.fetch_comparison(base_sha, Some(page))?;
        if comparison_relation(&response)? != ComparisonRelation::Ahead
            || response.total_commits != expected_total
            || response.commits.len() != expected_commits
        {
            return Err(ProviderError::InvalidResponse(
                "GitHub comparison changed or returned an incomplete page".to_owned(),
            ));
        }
        Ok(response)
    }

    fn fetch_comparison(
        &self,
        base_sha: &str,
        page: Option<usize>,
    ) -> Result<RawCompareResponse, ProviderError> {
        let mut url = self
            .api_base_url
            .join(REPOSITORY_COMPARE_PATH)
            .map_err(|error| ProviderError::InvalidBaseUrl(error.to_string()))?;
        url.path_segments_mut()
            .map_err(|()| {
                ProviderError::InvalidBaseUrl(
                    "GitHub API URL cannot accept endpoint paths".to_owned(),
                )
            })?
            .push(&format!("{base_sha}...main"));
        if let Some(page) = page {
            url.query_pairs_mut()
                .append_pair("per_page", &MAX_RECENT_COMMITS.to_string())
                .append_pair("page", &page.to_string());
        }
        self.get_bounded_json(&url)
    }

    fn get_bounded_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &Url,
    ) -> Result<T, ProviderError> {
        let mut response = self
            .agent
            .get(url.as_str())
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .call()
            .map_err(map_ureq_error)?;
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
                other => ProviderError::Transport(other.to_string()),
            })?;
        if bytes.len() > self.max_json_bytes {
            return Err(ProviderError::ResponseTooLarge {
                limit: self.max_json_bytes,
            });
        }
        serde_json::from_slice(&bytes)
            .map_err(|error| ProviderError::InvalidResponse(error.to_string()))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ComparisonRelation {
    Ahead,
    Identical,
}

#[derive(Debug, Deserialize)]
struct RawCompareResponse {
    status: String,
    total_commits: usize,
    #[serde(default)]
    commits: Vec<RawCommit>,
}

#[derive(Debug, Deserialize)]
struct RawCommit {
    sha: String,
    commit: RawCommitMetadata,
}

#[derive(Debug, Deserialize)]
struct RawCommitMetadata {
    message: String,
    committer: RawCommitter,
}

#[derive(Debug, Deserialize)]
struct RawCommitter {
    date: String,
}

fn comparison_relation(response: &RawCompareResponse) -> Result<ComparisonRelation, ProviderError> {
    match response.status.as_str() {
        "ahead" => Ok(ComparisonRelation::Ahead),
        "identical" if response.total_commits == 0 && response.commits.is_empty() => {
            Ok(ComparisonRelation::Identical)
        }
        "identical" => Err(ProviderError::InvalidResponse(
            "GitHub reported identical history with non-empty commits".to_owned(),
        )),
        "behind" | "diverged" => Err(ProviderError::InvalidResponse(format!(
            "GitHub main does not descend from the embedded build commit (status: {})",
            response.status
        ))),
        _ => Err(ProviderError::InvalidResponse(
            "GitHub returned an unknown comparison status".to_owned(),
        )),
    }
}

fn validated_total_commits(response: &RawCompareResponse) -> Result<usize, ProviderError> {
    if response.total_commits == 0
        || response.total_commits > MAX_TOTAL_COMPARE_COMMITS
        || response.commits.len() > MAX_DEFAULT_COMPARE_COMMITS
        || response.commits.len() > response.total_commits
    {
        return Err(ProviderError::InvalidResponse(
            "GitHub returned an invalid comparison commit count".to_owned(),
        ));
    }
    Ok(response.total_commits)
}

fn validate_commits(raw_commits: Vec<RawCommit>) -> Result<Vec<GitHubCommit>, ProviderError> {
    let mut commits = Vec::with_capacity(raw_commits.len());
    for raw in raw_commits {
        let sha = validate_sha(&raw.sha, "GitHub commit")?;
        if commits
            .iter()
            .any(|commit: &GitHubCommit| commit.sha == sha)
        {
            return Err(ProviderError::InvalidResponse(
                "GitHub returned a duplicate commit".to_owned(),
            ));
        }
        let committed_at = validate_timestamp(raw.commit.committer.date)?;
        let message = normalize_commit_message(raw.commit.message)?;
        commits.push(GitHubCommit {
            sha,
            committed_at,
            message,
        });
    }
    Ok(commits)
}

fn validate_sha(value: &str, label: &str) -> Result<String, ProviderError> {
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        let message = format!("{label} SHA must contain exactly 40 hexadecimal characters");
        return if label == "embedded build commit" {
            Err(ProviderError::InvalidRequest(message))
        } else {
            Err(ProviderError::InvalidResponse(message))
        };
    }
    Ok(value.to_ascii_lowercase())
}

fn normalize_commit_message(value: String) -> Result<String, ProviderError> {
    if value.trim().is_empty()
        || value.len() > MAX_COMMIT_MESSAGE_BYTES
        || value.chars().count() > MAX_COMMIT_MESSAGE_CHARS
    {
        return Err(ProviderError::InvalidResponse(
            "GitHub returned an empty or oversized commit message".to_owned(),
        ));
    }
    let value = if value.contains('\r') {
        value.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        value
    };
    if value
        .chars()
        .any(|character| character != '\n' && character.is_control())
    {
        return Err(ProviderError::InvalidResponse(
            "GitHub returned a terminal-unsafe commit message".to_owned(),
        ));
    }
    Ok(value)
}

fn validate_timestamp(value: String) -> Result<String, ProviderError> {
    if value.len() > MAX_TIMESTAMP_BYTES || !is_rfc3339_timestamp(&value) {
        return Err(ProviderError::InvalidResponse(
            "GitHub returned an invalid commit timestamp".to_owned(),
        ));
    }
    Ok(value)
}

fn is_rfc3339_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return false;
    }
    let Some(year) = parse_decimal(bytes.get(0..4)) else {
        return false;
    };
    let Some(month) = parse_decimal(bytes.get(5..7)) else {
        return false;
    };
    let Some(day) = parse_decimal(bytes.get(8..10)) else {
        return false;
    };
    let Some(hour) = parse_decimal(bytes.get(11..13)) else {
        return false;
    };
    let Some(minute) = parse_decimal(bytes.get(14..16)) else {
        return false;
    };
    let Some(second) = parse_decimal(bytes.get(17..19)) else {
        return false;
    };
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return false;
    }

    let mut zone_index = 19;
    if bytes.get(zone_index) == Some(&b'.') {
        zone_index += 1;
        let fraction_start = zone_index;
        while bytes.get(zone_index).is_some_and(u8::is_ascii_digit) {
            zone_index += 1;
        }
        if zone_index == fraction_start {
            return false;
        }
    }
    match bytes.get(zone_index) {
        Some(b'Z') => zone_index + 1 == bytes.len(),
        Some(b'+' | b'-')
            if bytes.len() == zone_index + 6 && bytes.get(zone_index + 3) == Some(&b':') =>
        {
            parse_decimal(bytes.get(zone_index + 1..zone_index + 3)).is_some_and(|hour| hour <= 23)
                && parse_decimal(bytes.get(zone_index + 4..zone_index + 6))
                    .is_some_and(|minute| minute <= 59)
        }
        _ => false,
    }
}

fn parse_decimal(bytes: Option<&[u8]>) -> Option<u32> {
    bytes?.iter().try_fold(0_u32, |value, byte| {
        byte.is_ascii_digit()
            .then(|| value.checked_mul(10)?.checked_add(u32::from(byte - b'0')))
            .flatten()
    })
}

const fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(400) || (year.is_multiple_of(4) && !year.is_multiple_of(100)) => {
            29
        }
        2 => 28,
        _ => 0,
    }
}

fn map_ureq_error(error: ureq::Error) -> ProviderError {
    match error {
        ureq::Error::StatusCode(code) => ProviderError::HttpStatus(code),
        ureq::Error::BodyExceedsLimit(limit) => ProviderError::ResponseTooLarge {
            limit: usize::try_from(limit).unwrap_or(usize::MAX),
        },
        other => ProviderError::Transport(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Write},
        net::{TcpListener, TcpStream},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, JoinHandle},
        time::Duration,
    };

    use serde_json::json;
    use url::Url;

    use super::{
        GitHubCommitClient, MAX_COMMIT_MESSAGE_BYTES, MAX_RECENT_COMMITS, RawCommit,
        RawCommitMetadata, RawCommitter, is_rfc3339_timestamp, normalize_commit_message,
        validate_commits,
    };
    use crate::providers::ProviderError;

    #[test]
    fn compare_returns_latest_ten_newest_first_with_full_messages() {
        let commits = (1..=12)
            .map(|number| {
                json!({
                    "sha": format!("{number:040x}"),
                    "commit": {
                        "message": format!("subject {number}\n\nbody {number}"),
                        "committer": {"date": format!("2026-07-{number:02}T12:00:00Z")}
                    }
                })
            })
            .collect::<Vec<_>>();
        let response = json!({
            "status": "ahead",
            "total_commits": commits.len(),
            "commits": commits,
        });
        let server = MockServer::spawn(vec![json_response(200, &response.to_string())]);
        let client = GitHubCommitClient::with_options(
            server.base_url.clone(),
            Duration::from_secs(2),
            256 * 1024,
        )
        .expect("mock client should be valid");

        let commits = client
            .commits_newer_than("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .expect("comparison should parse");

        assert_eq!(commits.len(), MAX_RECENT_COMMITS);
        assert_eq!(commits[0].sha, format!("{:040x}", 12));
        assert_eq!(commits[0].message, "subject 12\n\nbody 12");
        assert_eq!(commits[9].sha, format!("{:040x}", 3));
        let requests = server.finish();
        assert_eq!(
            requests,
            [
                "/repos/vitaly-zdanevich/youta/compare/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa...main"
            ]
        );
    }

    #[test]
    fn identical_comparison_returns_no_updates() {
        let response = json!({
            "status": "identical",
            "total_commits": 0,
            "commits": [],
        });
        let server = MockServer::spawn(vec![json_response(200, &response.to_string())]);
        let client = mock_client(&server, 4096);

        let commits = client
            .commits_newer_than("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .expect("identical history should be a successful empty update");

        assert!(commits.is_empty());
        assert_eq!(server.finish().len(), 1);
    }

    #[test]
    fn truncated_comparison_fetches_tail_pages_and_keeps_exact_latest_ten() {
        let first = json!({
            "status": "ahead",
            "total_commits": 261,
            "commits": [],
        });
        let previous_page = json!({
            "status": "ahead",
            "total_commits": 261,
            "commits": (251..=260).map(raw_commit_value).collect::<Vec<_>>(),
        });
        let last_page = json!({
            "status": "ahead",
            "total_commits": 261,
            "commits": [raw_commit_value(261)],
        });
        let server = MockServer::spawn(vec![
            json_response(200, &first.to_string()),
            json_response(200, &previous_page.to_string()),
            json_response(200, &last_page.to_string()),
        ]);
        let client = mock_client(&server, 256 * 1024);

        let commits = client
            .commits_newer_than("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .expect("tail pages should parse");

        assert_eq!(commits.len(), 10);
        assert_eq!(commits[0].sha, format!("{:040x}", 261));
        assert_eq!(commits[9].sha, format!("{:040x}", 252));
        assert_eq!(
            server.finish(),
            [
                "/repos/vitaly-zdanevich/youta/compare/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa...main",
                "/repos/vitaly-zdanevich/youta/compare/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa...main?per_page=10&page=26",
                "/repos/vitaly-zdanevich/youta/compare/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa...main?per_page=10&page=27",
            ]
        );
    }

    #[test]
    fn diverged_or_behind_history_is_not_presented_as_newer() {
        for status in ["diverged", "behind"] {
            let response = json!({
                "status": status,
                "total_commits": 1,
                "commits": [raw_commit_value(1)],
            });
            let server = MockServer::spawn(vec![json_response(200, &response.to_string())]);
            let client = mock_client(&server, 4096);

            let error = client
                .commits_newer_than("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .expect_err("non-descendant history must be rejected");

            assert!(matches!(error, ProviderError::InvalidResponse(_)));
            server.finish();
        }
    }

    #[test]
    fn rate_limit_and_response_limit_remain_typed_provider_errors() {
        let rate_server = MockServer::spawn(vec![json_response(429, "{}")]);
        let rate_client = mock_client(&rate_server, 4096);
        let error = rate_client
            .commits_newer_than("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .expect_err("rate limit should fail");
        assert!(matches!(error, ProviderError::HttpStatus(429)));
        rate_server.finish();

        let large_server = MockServer::spawn(vec![json_response(200, "{\"padding\":\"large\"}")]);
        let large_client = mock_client(&large_server, 8);
        let error = large_client
            .commits_newer_than("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .expect_err("oversized response should fail");
        assert!(matches!(
            error,
            ProviderError::ResponseTooLarge { limit: 8 }
        ));
        large_server.finish();
    }

    #[test]
    fn configuration_and_base_revision_are_validated_before_transport() {
        let unsafe_url = Url::parse("https://user@example.com/").expect("fixture URL");
        assert!(matches!(
            GitHubCommitClient::with_options(unsafe_url, Duration::from_secs(1), 4096),
            Err(ProviderError::InvalidBaseUrl(_))
        ));
        assert!(matches!(
            GitHubCommitClient::with_options(
                Url::parse("https://example.com/").expect("fixture URL"),
                Duration::ZERO,
                4096,
            ),
            Err(ProviderError::InvalidRequest(_))
        ));

        let client = GitHubCommitClient::new();
        assert!(matches!(
            client.commits_newer_than("main"),
            Err(ProviderError::InvalidRequest(_))
        ));
    }

    #[test]
    fn commit_fields_are_bounded_normalized_and_terminal_safe() {
        let commits = validate_commits(vec![RawCommit {
            sha: "ABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCD".to_owned(),
            commit: RawCommitMetadata {
                message: "subject\r\n\rbody".to_owned(),
                committer: RawCommitter {
                    date: "2026-07-31T20:17:08+04:00".to_owned(),
                },
            },
        }])
        .expect("valid fields should normalize");
        assert_eq!(commits[0].sha, "abcdefabcdefabcdefabcdefabcdefabcdefabcd");
        assert_eq!(commits[0].message, "subject\n\nbody");

        assert!(normalize_commit_message("unsafe\u{1b}".to_owned()).is_err());
        assert!(normalize_commit_message("x".repeat(MAX_COMMIT_MESSAGE_BYTES + 1)).is_err());
        assert!(!is_rfc3339_timestamp("2026-02-29T20:17:08Z"));
        assert!(is_rfc3339_timestamp("2028-02-29T20:17:08.123Z"));
    }

    #[test]
    fn malformed_remote_commit_is_rejected_without_partial_results() {
        let response = json!({
            "status": "ahead",
            "total_commits": 2,
            "commits": [
                raw_commit_value(1),
                {
                    "sha": "not-a-sha",
                    "commit": {
                        "message": "unsafe\u{0000}message",
                        "committer": {"date": "not-a-date"}
                    }
                }
            ],
        });
        let server = MockServer::spawn(vec![json_response(200, &response.to_string())]);
        let client = mock_client(&server, 4096);

        let error = client
            .commits_newer_than("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .expect_err("malformed rows must reject the complete update");

        assert!(matches!(error, ProviderError::InvalidResponse(_)));
        server.finish();
    }

    struct MockServer {
        base_url: Url,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<Vec<String>>>,
    }

    impl MockServer {
        fn spawn(responses: Vec<String>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("mock server should bind");
            let address = listener.local_addr().expect("mock address should exist");
            listener
                .set_nonblocking(true)
                .expect("mock listener should become nonblocking");
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = Arc::clone(&stop);
            let thread = thread::spawn(move || {
                let mut requests = Vec::new();
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
                                    return requests;
                                }
                                thread::sleep(Duration::from_millis(2));
                            }
                            Err(error) => panic!("mock should accept request: {error}"),
                        }
                    };
                    let target = request_target(&stream);
                    stream
                        .write_all(response.as_bytes())
                        .expect("mock should write response");
                    stream.flush().expect("mock should flush response");
                    requests.push(target);
                }
                requests
            });
            Self {
                base_url: Url::parse(&format!("http://{address}/")).expect("mock URL should parse"),
                stop,
                thread: Some(thread),
            }
        }

        fn finish(mut self) -> Vec<String> {
            self.thread
                .take()
                .expect("mock server thread should exist")
                .join()
                .expect("mock server should stop")
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
        let target = request_line
            .split_ascii_whitespace()
            .nth(1)
            .expect("request target should exist")
            .to_owned();
        loop {
            let mut header = String::new();
            reader
                .read_line(&mut header)
                .expect("mock header should be readable");
            if header == "\r\n" || header.is_empty() {
                break;
            }
        }
        target
    }

    fn json_response(status: u16, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn mock_client(server: &MockServer, max_json_bytes: usize) -> GitHubCommitClient {
        GitHubCommitClient::with_options(
            server.base_url.clone(),
            Duration::from_secs(2),
            max_json_bytes,
        )
        .expect("mock client should be valid")
    }

    fn raw_commit_value(number: usize) -> serde_json::Value {
        json!({
            "sha": format!("{number:040x}"),
            "commit": {
                "message": format!("subject {number}\n\nbody {number}"),
                "committer": {"date": "2026-07-31T12:00:00Z"}
            }
        })
    }
}
