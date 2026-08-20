//! Bounded update metadata for the external `yt-dlp` executable.
//!
//! The installed version is local process output. The latest upstream release
//! comes from GitHub's documented releases API, while Gentoo's architecture-
//! specific stable version comes from the JSON representation of its official
//! package page. Callers should run these independent probes on worker threads
//! after presenting any user-facing error popup.

use std::fmt;

#[cfg(feature = "network")]
use std::time::Duration;

use serde::{Deserialize, Serialize};
#[cfg(feature = "network")]
use url::Url;

use super::ProviderError;
#[cfg(feature = "network")]
use super::{DEFAULT_MAX_JSON_BYTES, DEFAULT_REQUEST_TIMEOUT, validate_base_url};

/// Browser page for upstream `yt-dlp` releases and update instructions.
pub const YT_DLP_PROJECT_URL: &str = "https://github.com/yt-dlp/yt-dlp";

/// Browser page for Gentoo's `net-misc/yt-dlp` package.
pub const GENTOO_YT_DLP_PACKAGE_URL: &str = "https://packages.gentoo.org/packages/net-misc/yt-dlp";

/// GitHub REST API origin used outside tests.
#[cfg(feature = "network")]
pub const DEFAULT_GITHUB_API_URL: &str = "https://api.github.com/";

/// Gentoo Packages origin used outside tests.
#[cfg(feature = "network")]
pub const DEFAULT_GENTOO_PACKAGES_URL: &str = "https://packages.gentoo.org/";

#[cfg(feature = "network")]
const MAX_CONFIGURED_JSON_BYTES: usize = 8 * 1024 * 1024;
const MAX_VERSION_BYTES: usize = 64;
#[cfg(feature = "network")]
const MAX_TIMESTAMP_BYTES: usize = 64;
#[cfg(feature = "network")]
const MAX_GENTOO_ARCH_BYTES: usize = 32;
#[cfg(feature = "network")]
const MAX_GENTOO_VERSIONS: usize = 1024;
#[cfg(feature = "network")]
const GITHUB_LATEST_RELEASE_PATH: &str = "repos/yt-dlp/yt-dlp/releases/latest";
#[cfg(feature = "network")]
const GENTOO_PACKAGE_JSON_PATH: &str = "packages/net-misc/yt-dlp.json";

/// Calendar date encoded by a `yt-dlp` date-based version.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct YtDlpReleaseDate {
    /// Four-digit release year.
    pub year: u16,
    /// One-based release month.
    pub month: u8,
    /// One-based release day.
    pub day: u8,
}

impl fmt::Display for YtDlpReleaseDate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:04}-{:02}-{:02}",
            self.year, self.month, self.day
        )
    }
}

/// Validated version reported by the executable Youta invokes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstalledYtDlpVersion {
    /// Exact numeric dotted version printed by `yt-dlp --version`.
    pub version: String,
    /// Date encoded by the first three numeric version components.
    pub release_date: YtDlpReleaseDate,
}

/// Latest full release returned by GitHub for `yt-dlp/yt-dlp`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GitHubYtDlpRelease {
    /// Validated stable release tag.
    pub version: String,
    /// GitHub's validated UTC publication timestamp.
    pub published_at: String,
}

/// Latest non-masked stable Gentoo version for one supplied architecture.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GentooStableYtDlpVersion {
    /// Gentoo version from the official package database.
    pub version: String,
    /// Bare Gentoo keyword used to establish stable status.
    pub arch: String,
}

/// Parses bounded output produced by `yt-dlp --version`.
///
/// Stable and nightly `yt-dlp` versions encode their release date in the first
/// three numeric components. For example, both `2026.07.04` and
/// `2026.07.04.232900` map to `2026-07-04`. One trailing LF or CRLF is
/// accepted, as is an already-normalized line from Youta's helper probe.
///
/// # Errors
///
/// Returns [`ProviderError::InvalidResponse`] for multiline, oversized,
/// non-numeric, or calendar-invalid output.
pub fn parse_installed_version(output: &str) -> Result<InstalledYtDlpVersion, ProviderError> {
    if output.len() > MAX_VERSION_BYTES.saturating_add(2) {
        return Err(invalid_response(
            "installed yt-dlp returned an oversized version",
        ));
    }
    let version = output
        .strip_suffix("\r\n")
        .or_else(|| output.strip_suffix('\n'))
        .unwrap_or(output);
    if version.contains(['\r', '\n']) {
        return Err(invalid_response(
            "installed yt-dlp version must contain one line",
        ));
    }
    let release_date = parse_version_date(version)?;
    Ok(InstalledYtDlpVersion {
        version: version.to_owned(),
        release_date,
    })
}

/// Blocking client for GitHub's latest published `yt-dlp` release.
#[cfg(feature = "network")]
#[derive(Clone)]
pub struct GitHubYtDlpReleaseClient {
    api_base_url: Url,
    agent: ureq::Agent,
    max_json_bytes: usize,
}

#[cfg(feature = "network")]
impl Default for GitHubYtDlpReleaseClient {
    fn default() -> Self {
        Self::with_options(
            Url::parse(DEFAULT_GITHUB_API_URL).expect("the built-in GitHub API URL is valid"),
            DEFAULT_REQUEST_TIMEOUT,
            DEFAULT_MAX_JSON_BYTES,
        )
        .expect("the built-in GitHub release client configuration is valid")
    }
}

#[cfg(feature = "network")]
impl GitHubYtDlpReleaseClient {
    /// Creates a client for GitHub's public API.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a client with an injectable API origin and transport bounds.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] when the URL, timeout, or size limit is
    /// invalid.
    pub fn with_options(
        api_base_url: Url,
        timeout: Duration,
        max_json_bytes: usize,
    ) -> Result<Self, ProviderError> {
        let api_base_url =
            validate_client_options(api_base_url, timeout, max_json_bytes, "GitHub release")?;
        Ok(Self {
            api_base_url,
            agent: update_agent(timeout),
            max_json_bytes,
        })
    }

    /// Fetches the latest published non-prerelease, non-draft release.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] for a failed bounded request or invalid
    /// release fields.
    pub fn latest_release(&self) -> Result<GitHubYtDlpRelease, ProviderError> {
        let url = self
            .api_base_url
            .join(GITHUB_LATEST_RELEASE_PATH)
            .map_err(|error| ProviderError::InvalidBaseUrl(error.to_string()))?;
        let raw: RawGitHubRelease =
            get_bounded_json(&self.agent, &url, self.max_json_bytes, JsonSource::GitHub)?;
        validate_github_release(raw)
    }
}

/// Blocking client for Gentoo's official `net-misc/yt-dlp` package metadata.
#[cfg(feature = "network")]
#[derive(Clone)]
pub struct GentooYtDlpPackageClient {
    packages_base_url: Url,
    agent: ureq::Agent,
    max_json_bytes: usize,
}

#[cfg(feature = "network")]
impl Default for GentooYtDlpPackageClient {
    fn default() -> Self {
        Self::with_options(
            Url::parse(DEFAULT_GENTOO_PACKAGES_URL)
                .expect("the built-in Gentoo Packages URL is valid"),
            DEFAULT_REQUEST_TIMEOUT,
            DEFAULT_MAX_JSON_BYTES,
        )
        .expect("the built-in Gentoo package client configuration is valid")
    }
}

#[cfg(feature = "network")]
impl GentooYtDlpPackageClient {
    /// Creates a client for the official Gentoo Packages service.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a client with an injectable service origin and transport bounds.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] when the URL, timeout, or size limit is
    /// invalid.
    pub fn with_options(
        packages_base_url: Url,
        timeout: Duration,
        max_json_bytes: usize,
    ) -> Result<Self, ProviderError> {
        let packages_base_url =
            validate_client_options(packages_base_url, timeout, max_json_bytes, "Gentoo package")?;
        Ok(Self {
            packages_base_url,
            agent: update_agent(timeout),
            max_json_bytes,
        })
    }

    /// Fetches the newest non-masked version with a bare stable `arch` keyword.
    ///
    /// Gentoo stability is architecture-specific. Testing (`~arch`), negative
    /// (`-arch`), wildcard-negative (`-*`), and package-masked versions are not
    /// stable results.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] for an invalid architecture, failed bounded
    /// request, or malformed package metadata.
    pub fn latest_stable(
        &self,
        arch: &str,
    ) -> Result<Option<GentooStableYtDlpVersion>, ProviderError> {
        validate_gentoo_arch(arch)?;
        let url = self
            .packages_base_url
            .join(GENTOO_PACKAGE_JSON_PATH)
            .map_err(|error| ProviderError::InvalidBaseUrl(error.to_string()))?;
        let package: RawGentooPackage =
            get_bounded_json(&self.agent, &url, self.max_json_bytes, JsonSource::Gentoo)?;
        select_gentoo_stable(package, arch)
    }
}

#[cfg(feature = "network")]
#[derive(Debug, Deserialize)]
struct RawGitHubRelease {
    tag_name: String,
    published_at: String,
}

#[cfg(feature = "network")]
#[derive(Debug, Deserialize)]
struct RawGentooPackage {
    atom: String,
    #[serde(default)]
    versions: Vec<RawGentooVersion>,
}

#[cfg(feature = "network")]
#[derive(Debug, Deserialize)]
struct RawGentooVersion {
    version: String,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    masks: Vec<serde_json::Value>,
}

#[cfg(feature = "network")]
#[derive(Clone, Copy)]
enum JsonSource {
    GitHub,
    Gentoo,
}

#[cfg(feature = "network")]
fn validate_client_options(
    base_url: Url,
    timeout: Duration,
    max_json_bytes: usize,
    provider: &str,
) -> Result<Url, ProviderError> {
    let base_url = validate_base_url(base_url)?;
    if timeout.is_zero() {
        return Err(ProviderError::InvalidRequest(format!(
            "{provider} timeout must be greater than zero"
        )));
    }
    if !(1..=MAX_CONFIGURED_JSON_BYTES).contains(&max_json_bytes) {
        return Err(ProviderError::InvalidRequest(format!(
            "{provider} JSON response limit must be between 1 and {MAX_CONFIGURED_JSON_BYTES} bytes"
        )));
    }
    Ok(base_url)
}

#[cfg(feature = "network")]
fn update_agent(timeout: Duration) -> ureq::Agent {
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

#[cfg(feature = "network")]
fn get_bounded_json<T: serde::de::DeserializeOwned>(
    agent: &ureq::Agent,
    url: &Url,
    limit: usize,
    source: JsonSource,
) -> Result<T, ProviderError> {
    let request = agent.get(url.as_str());
    let request = match source {
        JsonSource::GitHub => request
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2026-03-10"),
        JsonSource::Gentoo => request.header("Accept", "application/json"),
    };
    let mut response = request.call().map_err(map_ureq_error)?;
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
    serde_json::from_slice(&bytes)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))
}

#[cfg(feature = "network")]
fn validate_github_release(raw: RawGitHubRelease) -> Result<GitHubYtDlpRelease, ProviderError> {
    parse_version_date(&raw.tag_name)?;
    validate_github_timestamp(&raw.published_at)?;
    Ok(GitHubYtDlpRelease {
        version: raw.tag_name,
        published_at: raw.published_at,
    })
}

#[cfg(feature = "network")]
fn select_gentoo_stable(
    package: RawGentooPackage,
    arch: &str,
) -> Result<Option<GentooStableYtDlpVersion>, ProviderError> {
    if package.atom != "net-misc/yt-dlp" {
        return Err(invalid_response(
            "Gentoo returned metadata for a different package",
        ));
    }
    if package.versions.len() > MAX_GENTOO_VERSIONS {
        return Err(invalid_response("Gentoo returned too many yt-dlp versions"));
    }
    for version in package.versions {
        if version.version == "9999"
            || !version.masks.is_empty()
            || !version.keywords.iter().any(|keyword| keyword == arch)
        {
            continue;
        }
        parse_gentoo_version(&version.version)?;
        return Ok(Some(GentooStableYtDlpVersion {
            version: version.version,
            arch: arch.to_owned(),
        }));
    }
    Ok(None)
}

#[cfg(feature = "network")]
fn validate_gentoo_arch(arch: &str) -> Result<(), ProviderError> {
    if arch.is_empty()
        || arch.len() > MAX_GENTOO_ARCH_BYTES
        || arch.starts_with(['~', '-'])
        || !arch.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        return Err(ProviderError::InvalidRequest(
            "Gentoo ARCH must be a bare lowercase architecture keyword".to_owned(),
        ));
    }
    Ok(())
}

fn parse_version_date(version: &str) -> Result<YtDlpReleaseDate, ProviderError> {
    if version.is_empty()
        || version.len() > MAX_VERSION_BYTES
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return Err(invalid_response(
            "yt-dlp version must be bounded numeric dotted text",
        ));
    }
    let components = version.split('.').collect::<Vec<_>>();
    if components.len() < 3
        || components.iter().any(|component| component.is_empty())
        || components[0].len() != 4
        || components[1].len() != 2
        || components[2].len() != 2
    {
        return Err(invalid_response(
            "yt-dlp version must begin with YYYY.MM.DD",
        ));
    }
    let year = components[0]
        .parse::<u16>()
        .map_err(|_| invalid_response("yt-dlp version contains an invalid year"))?;
    let month = components[1]
        .parse::<u8>()
        .map_err(|_| invalid_response("yt-dlp version contains an invalid month"))?;
    let day = components[2]
        .parse::<u8>()
        .map_err(|_| invalid_response("yt-dlp version contains an invalid day"))?;
    if year == 0 || month == 0 || day == 0 || day > days_in_month(year, month) {
        return Err(invalid_response(
            "yt-dlp version contains an invalid calendar date",
        ));
    }
    Ok(YtDlpReleaseDate { year, month, day })
}

#[cfg(feature = "network")]
fn parse_gentoo_version(version: &str) -> Result<(), ProviderError> {
    let (upstream, revision) = version
        .split_once("-r")
        .map_or((version, None), |(upstream, revision)| {
            (upstream, Some(revision))
        });
    parse_version_date(upstream)?;
    if revision.is_some_and(|revision| {
        revision.is_empty() || !revision.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        return Err(invalid_response(
            "Gentoo returned an invalid yt-dlp revision",
        ));
    }
    Ok(())
}

#[cfg(feature = "network")]
fn validate_github_timestamp(timestamp: &str) -> Result<(), ProviderError> {
    let bytes = timestamp.as_bytes();
    if timestamp.len() > MAX_TIMESTAMP_BYTES
        || bytes.len() != 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
        || bytes.get(19) != Some(&b'Z')
        || ![0..4, 5..7, 8..10, 11..13, 14..16, 17..19]
            .into_iter()
            .all(|range| bytes[range].iter().all(u8::is_ascii_digit))
    {
        return Err(invalid_response(
            "GitHub returned an invalid release timestamp",
        ));
    }
    parse_iso_date(&timestamp[..10])?;
    let hour = parse_two_digits(&timestamp[11..13])?;
    let minute = parse_two_digits(&timestamp[14..16])?;
    let second = parse_two_digits(&timestamp[17..19])?;
    if hour > 23 || minute > 59 || second > 60 {
        return Err(invalid_response(
            "GitHub returned an invalid release timestamp",
        ));
    }
    Ok(())
}

#[cfg(feature = "network")]
fn parse_iso_date(value: &str) -> Result<YtDlpReleaseDate, ProviderError> {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || ![0..4, 5..7, 8..10]
            .into_iter()
            .all(|range| bytes[range].iter().all(u8::is_ascii_digit))
    {
        return Err(invalid_response("invalid ISO calendar date"));
    }
    let year = value[..4]
        .parse::<u16>()
        .map_err(|_| invalid_response("invalid ISO calendar year"))?;
    let month = parse_two_digits(&value[5..7])?;
    let day = parse_two_digits(&value[8..10])?;
    if year == 0 || month == 0 || day == 0 || day > days_in_month(year, month) {
        return Err(invalid_response("invalid ISO calendar date"));
    }
    Ok(YtDlpReleaseDate { year, month, day })
}

#[cfg(feature = "network")]
fn parse_two_digits(value: &str) -> Result<u8, ProviderError> {
    if value.len() != 2 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_response("invalid two-digit date component"));
    }
    value
        .parse()
        .map_err(|_| invalid_response("invalid two-digit date component"))
}

const fn days_in_month(year: u16, month: u8) -> u8 {
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

fn invalid_response(message: &str) -> ProviderError {
    ProviderError::InvalidResponse(message.to_owned())
}

#[cfg(feature = "network")]
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
    #[cfg(feature = "network")]
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

    #[cfg(feature = "network")]
    use serde_json::json;
    #[cfg(feature = "network")]
    use url::Url;

    #[cfg(feature = "network")]
    use super::{GentooYtDlpPackageClient, GitHubYtDlpReleaseClient};
    use super::{YtDlpReleaseDate, parse_installed_version};
    use crate::providers::ProviderError;

    #[test]
    fn installed_stable_and_nightly_versions_expose_their_calendar_date() {
        let stable = parse_installed_version("2026.07.04").expect("stable version should parse");
        assert_eq!(stable.version, "2026.07.04");
        assert_eq!(
            stable.release_date,
            YtDlpReleaseDate {
                year: 2026,
                month: 7,
                day: 4,
            }
        );
        assert_eq!(stable.release_date.to_string(), "2026-07-04");

        let nightly =
            parse_installed_version("2028.02.29.232900\r\n").expect("nightly version should parse");
        assert_eq!(nightly.release_date.to_string(), "2028-02-29");
    }

    #[test]
    fn installed_version_rejects_malformed_unsafe_or_invalid_dates() {
        for output in [
            "",
            " 2026.07.04\n",
            "2026.07.04 extra\n",
            "2026.07.04\nsecond line\n",
            "2026.07.04\n\n",
            "2026.02.29\n",
            "2026.13.01\n",
            "2026..04\n",
            "stable\n",
        ] {
            assert!(
                matches!(
                    parse_installed_version(output),
                    Err(ProviderError::InvalidResponse(_))
                ),
                "fixture should fail: {output:?}"
            );
        }
    }

    #[cfg(feature = "network")]
    #[test]
    fn github_client_uses_latest_release_endpoint_and_validates_fields() {
        let body = json!({
            "tag_name": "2026.08.19",
            "published_at": "2026-08-19T23:48:43Z",
            "assets": [{"ignored": true}],
        })
        .to_string();
        let server = MockServer::spawn(vec![json_response(200, &body)]);
        let client = github_client(&server, 4096);

        let release = client.latest_release().expect("release should parse");

        assert_eq!(release.version, "2026.08.19");
        assert_eq!(release.published_at, "2026-08-19T23:48:43Z");
        let requests = server.finish();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].target, "/repos/yt-dlp/yt-dlp/releases/latest");
        assert_eq!(
            requests[0].header("accept"),
            Some("application/vnd.github+json")
        );
        assert_eq!(
            requests[0].header("x-github-api-version"),
            Some("2026-03-10")
        );
        assert!(
            requests[0]
                .header("user-agent")
                .is_some_and(|value| value.starts_with("youta/"))
        );
    }

    #[cfg(feature = "network")]
    #[test]
    fn github_client_rejects_bad_json_fields_http_errors_and_oversized_bodies() {
        for body in [
            r#"{"tag_name":"latest","published_at":"2026-08-19T23:48:43Z"}"#,
            r#"{"tag_name":"2026.08.19","published_at":"not-a-date"}"#,
            r#"{"tag_name":"2026.08.19","published_at":"２０26-08-19T23:48Z"}"#,
            r#"{"tag_name":"2026.08.19"}"#,
            "not json",
        ] {
            let server = MockServer::spawn(vec![json_response(200, body)]);
            let error = github_client(&server, 4096)
                .latest_release()
                .expect_err("invalid GitHub response should fail");
            assert!(matches!(error, ProviderError::InvalidResponse(_)));
            server.finish();
        }

        let rate_server = MockServer::spawn(vec![json_response(403, r#"{"message":"rate"}"#)]);
        let error = github_client(&rate_server, 4096)
            .latest_release()
            .expect_err("HTTP error should stay typed");
        assert!(matches!(error, ProviderError::HttpStatus(403)));
        rate_server.finish();

        let large_server = MockServer::spawn(vec![json_response_without_length(
            200,
            r#"{"tag_name":"2026.08.19","published_at":"2026-08-19T23:48:43Z"}"#,
        )]);
        let error = github_client(&large_server, 16)
            .latest_release()
            .expect_err("oversized JSON should fail before parsing");
        assert!(matches!(
            error,
            ProviderError::ResponseTooLarge { limit: 16 }
        ));
        large_server.finish();
    }

    #[cfg(feature = "network")]
    #[test]
    fn gentoo_client_selects_first_unmasked_bare_keyword_for_supplied_arch() {
        let body = gentoo_fixture(&json!([
            {"version": "9999", "keywords": [""]},
            {"version": "2026.09.01", "keywords": ["~amd64", "arm64"]},
            {
                "version": "2026.08.25",
                "keywords": ["amd64"],
                "masks": [{"reason": "masked"}]
            },
            {"version": "2026.08.19-r1", "keywords": ["amd64", "~riscv"]},
            {"version": "2026.07.04", "keywords": ["amd64"]}
        ]));
        let server = MockServer::spawn(vec![json_response(200, &body)]);
        let client = gentoo_client(&server, 4096);

        let stable = client
            .latest_stable("amd64")
            .expect("package should parse")
            .expect("amd64 stable should exist");

        assert_eq!(stable.version, "2026.08.19-r1");
        assert_eq!(stable.arch, "amd64");
        let requests = server.finish();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].target, "/packages/net-misc/yt-dlp.json");
        assert_eq!(requests[0].header("accept"), Some("application/json"));
    }

    #[cfg(feature = "network")]
    #[test]
    fn gentoo_stability_does_not_leak_across_architectures() {
        let body = gentoo_fixture(&json!([
            {"version": "2026.09.01", "keywords": ["~amd64", "arm64"]},
            {"version": "2026.08.19", "keywords": ["-amd64", "~arm64"]}
        ]));
        let server = MockServer::spawn(vec![json_response(200, &body)]);
        let client = gentoo_client(&server, 4096);

        assert_eq!(
            client
                .latest_stable("amd64")
                .expect("no stable version is a valid result"),
            None
        );
        server.finish();
    }

    #[cfg(feature = "network")]
    #[test]
    fn gentoo_client_validates_arch_atom_schema_and_response_bound() {
        let client = GentooYtDlpPackageClient::new();
        for arch in ["", "~amd64", "-amd64", "amd64\n", "a".repeat(33).as_str()] {
            assert!(matches!(
                client.latest_stable(arch),
                Err(ProviderError::InvalidRequest(_))
            ));
        }

        for body in [
            r#"{"atom":"net-misc/not-yt-dlp","versions":[]}"#,
            r#"{"atom":"net-misc/yt-dlp","versions":"wrong"}"#,
            "<html>not JSON</html>",
        ] {
            let server = MockServer::spawn(vec![json_response(200, body)]);
            let error = gentoo_client(&server, 4096)
                .latest_stable("amd64")
                .expect_err("invalid Gentoo response should fail");
            assert!(matches!(error, ProviderError::InvalidResponse(_)));
            server.finish();
        }

        let large_server = MockServer::spawn(vec![json_response(
            200,
            &gentoo_fixture(&json!([{"version": "2026.08.19", "keywords": ["amd64"]}])),
        )]);
        let error = gentoo_client(&large_server, 24)
            .latest_stable("amd64")
            .expect_err("oversized JSON should fail");
        assert!(matches!(
            error,
            ProviderError::ResponseTooLarge { limit: 24 }
        ));
        large_server.finish();
    }

    #[cfg(feature = "network")]
    #[test]
    fn clients_reject_unsafe_urls_zero_timeouts_and_invalid_limits() {
        let unsafe_url = Url::parse("https://user@example.com/").expect("fixture URL");
        assert!(matches!(
            GitHubYtDlpReleaseClient::with_options(
                unsafe_url.clone(),
                Duration::from_secs(1),
                4096,
            ),
            Err(ProviderError::InvalidBaseUrl(_))
        ));
        assert!(matches!(
            GentooYtDlpPackageClient::with_options(unsafe_url, Duration::from_secs(1), 4096,),
            Err(ProviderError::InvalidBaseUrl(_))
        ));
        assert!(matches!(
            GitHubYtDlpReleaseClient::with_options(
                Url::parse("https://example.com/").expect("fixture URL"),
                Duration::ZERO,
                4096,
            ),
            Err(ProviderError::InvalidRequest(_))
        ));
        assert!(matches!(
            GentooYtDlpPackageClient::with_options(
                Url::parse("https://example.com/").expect("fixture URL"),
                Duration::from_secs(1),
                0,
            ),
            Err(ProviderError::InvalidRequest(_))
        ));
    }

    #[cfg(feature = "network")]
    #[derive(Debug)]
    struct RecordedRequest {
        target: String,
        headers: Vec<(String, String)>,
    }

    #[cfg(feature = "network")]
    impl RecordedRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }
    }

    #[cfg(feature = "network")]
    struct MockServer {
        base_url: Url,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<Vec<RecordedRequest>>>,
    }

    #[cfg(feature = "network")]
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
                    requests.push(read_request(&stream));
                    stream
                        .write_all(response.as_bytes())
                        .expect("mock should write response");
                    stream.flush().expect("mock should flush response");
                }
                requests
            });
            Self {
                base_url: Url::parse(&format!("http://{address}/")).expect("mock URL should parse"),
                stop,
                thread: Some(thread),
            }
        }

        fn finish(mut self) -> Vec<RecordedRequest> {
            self.thread
                .take()
                .expect("mock server thread should exist")
                .join()
                .expect("mock server should stop")
        }
    }

    #[cfg(feature = "network")]
    impl Drop for MockServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                thread.join().expect("mock server should stop");
            }
        }
    }

    #[cfg(feature = "network")]
    fn read_request(stream: &TcpStream) -> RecordedRequest {
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
        let mut headers = Vec::new();
        loop {
            let mut header = String::new();
            reader
                .read_line(&mut header)
                .expect("mock header should be readable");
            if header == "\r\n" || header.is_empty() {
                break;
            }
            let (name, value) = header
                .trim_end()
                .split_once(':')
                .expect("request header should contain a colon");
            headers.push((name.to_owned(), value.trim().to_owned()));
        }
        RecordedRequest { target, headers }
    }

    #[cfg(feature = "network")]
    fn json_response(status: u16, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[cfg(feature = "network")]
    fn json_response_without_length(status: u16, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}"
        )
    }

    #[cfg(feature = "network")]
    fn github_client(server: &MockServer, limit: usize) -> GitHubYtDlpReleaseClient {
        GitHubYtDlpReleaseClient::with_options(
            server.base_url.clone(),
            Duration::from_secs(2),
            limit,
        )
        .expect("mock GitHub client should be valid")
    }

    #[cfg(feature = "network")]
    fn gentoo_client(server: &MockServer, limit: usize) -> GentooYtDlpPackageClient {
        GentooYtDlpPackageClient::with_options(
            server.base_url.clone(),
            Duration::from_secs(2),
            limit,
        )
        .expect("mock Gentoo client should be valid")
    }

    #[cfg(feature = "network")]
    fn gentoo_fixture(versions: &serde_json::Value) -> String {
        json!({
            "atom": "net-misc/yt-dlp",
            "versions": versions,
        })
        .to_string()
    }
}
