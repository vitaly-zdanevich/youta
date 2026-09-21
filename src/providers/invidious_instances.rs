//! Bounded discovery of public, API-capable Invidious instances.
//!
//! The official directory is fetched only when the caller requests it. Returned
//! origins are suggestions, not verified playback guarantees: the directory's
//! `api` flag records its last API probe. This adapter never probes candidates.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;
use url::{Host, Url};

use super::{DEFAULT_REQUEST_TIMEOUT, ProviderError};
use crate::domain::remote_url_has_non_public_host;

/// Official machine-readable directory linked from <https://docs.invidious.io/instances/>.
pub const INVIDIOUS_INSTANCES_URL: &str = "https://api.invidious.io/instances.json";

/// Maximum directory response size, including metadata that Youta does not use.
const MAX_DIRECTORY_BYTES: usize = 1024 * 1024;
/// Maximum number of raw directory entries, before filtering and deduplication.
const MAX_DIRECTORY_ENTRIES: usize = 256;
/// Maximum number of characters retained from an underlying transport error.
const MAX_TRANSPORT_ERROR_CHARS: usize = 512;

/// A public HTTPS origin whose most recent directory API probe succeeded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvidiousInstance {
    /// Credential-free, root-only HTTPS URL; no request has been sent to it.
    pub url: Url,
    /// Validated hostname with an optional two-letter directory region code.
    pub label: String,
}

/// Blocking client for the fixed official Invidious instance directory.
///
/// Run [`Self::fetch`] on a worker, not the UI thread. Requests have a 15-second
/// global timeout, a 1 MiB body limit and a 256-entry limit. Redirects are never
/// followed. Endpoint injection is private to tests so remote directory content
/// cannot select a subsequent request destination.
#[derive(Clone)]
pub struct InvidiousInstancesClient {
    endpoint: Url,
    agent: ureq::Agent,
    max_json_bytes: usize,
}

impl Default for InvidiousInstancesClient {
    fn default() -> Self {
        Self::new()
    }
}

impl InvidiousInstancesClient {
    /// Creates a bounded client using the official HTTPS directory endpoint.
    ///
    /// # Panics
    ///
    /// Panics only if the compile-time directory URL is changed to an invalid URL.
    #[must_use]
    pub fn new() -> Self {
        Self {
            endpoint: Url::parse(INVIDIOUS_INSTANCES_URL)
                .expect("the fixed Invidious directory URL is valid"),
            agent: directory_agent(DEFAULT_REQUEST_TIMEOUT, true),
            max_json_bytes: MAX_DIRECTORY_BYTES,
        }
    }

    /// Fetches sorted, deduplicated public HTTPS instances reporting `api: true`.
    ///
    /// Disabled, unavailable (`null` or missing), malformed, non-public and
    /// non-HTTPS entries are omitted. An empty result is valid and does not imply
    /// that a manually configured instance is unavailable. DNS is not resolved
    /// for candidates: hostname checks are syntactic, not rebinding protection.
    ///
    /// # Errors
    ///
    /// Returns a bounded provider error for transport failures, non-200 status,
    /// oversized responses or malformed/oversized top-level directory data.
    pub fn fetch(&self) -> Result<Vec<InvidiousInstance>, ProviderError> {
        let mut response = self
            .agent
            .get(self.endpoint.as_str())
            .header("Accept", "application/json")
            .call()
            .map_err(directory_transport_error)?;
        let status = response.status().as_u16();
        // With max_redirects(0), ureq returns redirects as ordinary responses.
        // Reject them even if the body happens to contain valid directory JSON.
        if status != 200 {
            return Err(ProviderError::HttpStatus(status));
        }
        let limit = self.max_json_bytes;
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
            .limit(limit as u64 + 1)
            .read_to_vec()
            .map_err(|error| match error {
                ureq::Error::BodyExceedsLimit(_) => ProviderError::ResponseTooLarge { limit },
                other => directory_transport_error(other),
            })?;
        if bytes.len() > limit {
            return Err(ProviderError::ResponseTooLarge { limit });
        }
        parse_directory(&bytes)
    }
}

/// Creates an agent that cannot silently leave the fixed directory origin.
fn directory_agent(timeout: Duration, https_only: bool) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .https_only(https_only)
        .max_redirects(0)
        .http_status_as_error(false)
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

/// Keeps remote/network diagnostics finite and safe for a one-line UI message.
fn directory_transport_error(error: ureq::Error) -> ProviderError {
    let detail: String = error
        .to_string()
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_TRANSPORT_ERROR_CHARS)
        .collect();
    ProviderError::Transport(format!("Invidious instance directory: {detail}"))
}

/// Only the small subset of directory metadata used by the picker.
#[derive(Deserialize)]
struct DirectoryMetadata {
    uri: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    api: Option<bool>,
    #[serde(default)]
    region: Option<String>,
}

/// Filters untrusted directory records without trusting display names or flags.
fn parse_directory(bytes: &[u8]) -> Result<Vec<InvidiousInstance>, ProviderError> {
    let entries: Vec<serde_json::Value> = serde_json::from_slice(bytes).map_err(|_| {
        ProviderError::InvalidResponse(
            "Invidious instance directory must contain a JSON array".to_owned(),
        )
    })?;
    if entries.len() > MAX_DIRECTORY_ENTRIES {
        return Err(ProviderError::InvalidResponse(format!(
            "Invidious instance directory exceeded the {MAX_DIRECTORY_ENTRIES}-entry limit"
        )));
    }
    let mut instances = BTreeMap::new();
    for entry in entries {
        let Ok((hostname, metadata)) = serde_json::from_value::<(String, DirectoryMetadata)>(entry)
        else {
            continue;
        };
        if metadata.kind != "https" || metadata.api != Some(true) {
            continue;
        }
        let Some(url) = public_instance_url(&hostname, &metadata.uri) else {
            continue;
        };
        let host = url.host_str().expect("validated directory hostname");
        let label = match metadata.region {
            Some(region)
                if region.len() == 2 && region.bytes().all(|byte| byte.is_ascii_alphabetic()) =>
            {
                format!("{host} ({})", region.to_ascii_uppercase())
            }
            _ => host.to_owned(),
        };
        instances
            .entry(url.as_str().to_owned())
            .or_insert(InvidiousInstance { url, label });
    }
    Ok(instances.into_values().collect())
}

/// Accepts only matching, root-only public HTTPS DNS names from the directory.
///
/// Validate raw syntax before parsing: URL normalization would otherwise erase
/// empty credentials, explicit default ports and dot paths. Private/manual
/// instance configuration is intentionally outside this discovery policy.
fn public_instance_url(hostname: &str, raw: &str) -> Option<Url> {
    let authority = raw.strip_prefix("https://")?;
    let authority = authority.strip_suffix('/').unwrap_or(authority);
    if authority.len() > 253
        || !authority.contains('.')
        || !authority.eq_ignore_ascii_case(hostname)
        || authority.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    let Some(Host::Domain(host)) = url.host() else {
        return None;
    };
    if remote_url_has_non_public_host(&url)
        || host == "home.arpa"
        || [".onion", ".i2p", ".ygg", ".home.arpa", ".home", ".lan"]
            .iter()
            .any(|suffix| host.ends_with(suffix))
    {
        return None;
    }
    Some(url)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpListener};
    use std::thread::JoinHandle;
    use std::time::Instant;

    use serde_json::{Value, json};

    use super::*;

    /// Creates one representative directory tuple without unrelated large stats.
    fn entry(hostname: &str, uri: &str, api: Value) -> Value {
        json!([hostname, {"uri": uri, "type": "https", "api": api}])
    }

    #[test]
    fn directory_keeps_only_explicit_api_success_and_deduplicates_sorted_origins() {
        let bytes = serde_json::to_vec(&json!([
            ["z.example.org", {"uri": "https://z.example.org", "type": "https", "api": true, "region": "de", "stats": null}],
            entry("a.example.org", "https://A.Example.org/", json!(true)),
            entry("z.example.org", "https://z.example.org/", json!(true)),
            entry("disabled.example.org", "https://disabled.example.org", json!(false)),
            entry("unknown.example.org", "https://unknown.example.org", Value::Null),
            ["missing.example.org", {"uri": "https://missing.example.org", "type": "https"}],
            entry("string.example.org", "https://string.example.org", json!("true")),
            ["a.example.org", {"uri": "https://a.example.org", "type": "onion", "api": true}],
            ["malformed"],
            {"not": "a tuple"}
        ]))
        .unwrap();
        let instances = parse_directory(&bytes).unwrap();
        assert_eq!(instances.len(), 2);
        assert_eq!(instances[0].url.as_str(), "https://a.example.org/");
        assert_eq!(instances[0].label, "a.example.org");
        assert_eq!(instances[1].url.as_str(), "https://z.example.org/");
        assert_eq!(instances[1].label, "z.example.org (DE)");
    }

    #[test]
    fn directory_omits_untrusted_region_text_from_labels() {
        for region in ["<script>", "\nUS", "USA", "1A", "é", ""] {
            let bytes = serde_json::to_vec(&json!([
                ["inv.example.org", {"uri": "https://inv.example.org", "type": "https", "api": true, "region": region, "flag": "malicious label"}]
            ]))
            .unwrap();
            assert_eq!(parse_directory(&bytes).unwrap()[0].label, "inv.example.org");
        }
    }

    #[test]
    fn directory_rejects_unsafe_or_normalized_candidate_urls() {
        for raw in [
            "http://inv.example.org/",
            "https://user:password@inv.example.org/",
            "https://@inv.example.org/",
            "https://inv.example.org:443/",
            "https://inv.example.org:/",
            "https://inv.example.org:8443/",
            "https://inv.example.org/path",
            "https://inv.example.org/a/..",
            "https://inv.example.org/%2e",
            "https://inv.example.org//",
            "https://inv.example.org\\",
            "https://inv.example.org/?",
            "https://inv.example.org/#",
            "https://inv.example.org/?token=secret",
            "https://inv.example.org/#fragment",
            " https://inv.example.org/",
            "https://inv.example.org/\n",
            "https://different.example.org/",
            "https://inv.example.org./",
        ] {
            assert!(
                public_instance_url("inv.example.org", raw).is_none(),
                "{raw}"
            );
        }
    }

    #[test]
    fn directory_rejects_non_public_and_non_dns_hosts() {
        let long_label = format!("{}.example.org", "x".repeat(64));
        let long_host = format!(
            "{}.{}.{}.org",
            "x".repeat(63),
            "y".repeat(63),
            "z".repeat(63)
        );
        let too_long_host = format!("{}.{}", "a".repeat(63), long_host);
        for host in [
            "localhost",
            "inv.localhost",
            "inv.local",
            "inv.internal",
            "home.arpa",
            "inv.home.arpa",
            "inv.home",
            "inv.lan",
            "inv.onion",
            "inv.i2p",
            "inv.ygg",
            "127.0.0.1",
            "10.1.2.3",
            "8.8.8.8",
            "0x7f.0.0.1",
            "2130706433",
            "[::1]",
            "[2606:4700:4700::1111]",
            "inv..example.org",
            "-inv.example.org",
            "inv-.example.org",
            "inv_test.example.org",
            &long_label,
            &too_long_host,
        ] {
            let raw = format!("https://{host}/");
            assert!(public_instance_url(host, &raw).is_none(), "{host}");
        }
    }

    #[test]
    fn directory_accepts_empty_and_unavailable_directory_without_fake_candidates() {
        assert!(parse_directory(b"[]").unwrap().is_empty());
        let bytes = serde_json::to_vec(&json!([
            entry("off.example.org", "https://off.example.org", json!(false)),
            entry(
                "unknown.example.org",
                "https://unknown.example.org",
                Value::Null
            )
        ]))
        .unwrap();
        assert!(parse_directory(&bytes).unwrap().is_empty());
    }

    #[test]
    fn directory_rejects_invalid_json_and_bounds_raw_entry_count() {
        for bytes in [b"not JSON".as_slice(), b"{}", b"null", b"[}"] {
            assert!(matches!(
                parse_directory(bytes),
                Err(ProviderError::InvalidResponse(_))
            ));
        }
        let entries = vec![Value::Null; MAX_DIRECTORY_ENTRIES + 1];
        assert!(matches!(
            parse_directory(&serde_json::to_vec(&entries).unwrap()),
            Err(ProviderError::InvalidResponse(_))
        ));
        assert!(
            parse_directory(&serde_json::to_vec(&entries[..MAX_DIRECTORY_ENTRIES]).unwrap())
                .unwrap()
                .is_empty()
        );
    }

    /// Serves exactly one loopback response with bounded accept/read/write waits.
    fn mock_directory(response: Vec<u8>, delay: Duration) -> (Url, JoinHandle<String>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "mock directory accept timed out");
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("mock directory accept failed: {error}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 8192, "oversized mock request headers");
                let mut byte = [0_u8; 1];
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            std::thread::sleep(delay);
            // A size/status/timeout failure may close the client before the body.
            let _ = socket.write_all(&response);
            String::from_utf8(request).unwrap()
        });
        (
            Url::parse(&format!("http://{address}/instances.json")).unwrap(),
            worker,
        )
    }

    /// Injects only a loopback test endpoint; production has no endpoint override.
    fn mock_client(endpoint: Url, timeout: Duration, limit: usize) -> InvidiousInstancesClient {
        InvidiousInstancesClient {
            endpoint,
            agent: directory_agent(timeout, false),
            max_json_bytes: limit,
        }
    }

    /// Encodes one complete successful response, including its advertised size.
    fn json_response(bytes: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            bytes.len()
        ).into_bytes();
        response.extend_from_slice(bytes);
        response
    }

    #[test]
    fn directory_fetch_uses_one_bounded_json_get_without_candidate_probes() {
        let body = serde_json::to_vec(&json!([entry(
            "inv.example.org",
            "https://inv.example.org",
            json!(true)
        )]))
        .unwrap();
        let (endpoint, worker) = mock_directory(json_response(&body), Duration::ZERO);
        let result = mock_client(endpoint, Duration::from_secs(2), MAX_DIRECTORY_BYTES).fetch();
        let request = worker.join().unwrap();
        assert_eq!(result.unwrap()[0].url.as_str(), "https://inv.example.org/");
        assert!(request.starts_with("GET /instances.json HTTP/1.1\r\n"));
        let lower = request.to_ascii_lowercase();
        assert!(lower.contains("\r\naccept: application/json\r\n"));
        assert!(lower.contains("\r\nuser-agent: youta/"));
        assert!(!lower.contains("\r\nauthorization:"));
        assert!(!lower.contains("\r\ncookie:"));
    }

    #[test]
    fn directory_fetch_rejects_redirect_without_contacting_location() {
        let destination = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        destination.set_nonblocking(true).unwrap();
        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{}/redirected\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]",
            destination.local_addr().unwrap()
        ).into_bytes();
        let (endpoint, worker) = mock_directory(response, Duration::ZERO);
        let result = mock_client(endpoint, Duration::from_secs(2), MAX_DIRECTORY_BYTES).fetch();
        worker.join().unwrap();
        assert!(matches!(result, Err(ProviderError::HttpStatus(302))));
        assert_eq!(
            destination.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn directory_fetch_rejects_error_status_even_with_valid_json() {
        let response =
            b"HTTP/1.1 503 Unavailable\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]"
                .to_vec();
        let (endpoint, worker) = mock_directory(response, Duration::ZERO);
        let result = mock_client(endpoint, Duration::from_secs(2), MAX_DIRECTORY_BYTES).fetch();
        worker.join().unwrap();
        assert!(matches!(result, Err(ProviderError::HttpStatus(503))));
    }

    #[test]
    fn directory_fetch_bounds_advertised_chunked_and_unadvertised_bodies() {
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n9\r\n123456789\r\n0\r\n\r\n".to_vec();
        let unadvertised = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n123456789".to_vec();
        for response in [json_response(b"123456789"), chunked, unadvertised] {
            let (endpoint, worker) = mock_directory(response, Duration::ZERO);
            let result = mock_client(endpoint, Duration::from_secs(2), 8).fetch();
            worker.join().unwrap();
            assert!(matches!(
                result,
                Err(ProviderError::ResponseTooLarge { limit: 8 })
            ));
        }
    }

    #[test]
    fn directory_fetch_accepts_response_at_exact_byte_limit() {
        let (endpoint, worker) = mock_directory(json_response(b"[]"), Duration::ZERO);
        let result = mock_client(endpoint, Duration::from_secs(2), 2).fetch();
        worker.join().unwrap();
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn directory_fetch_has_a_global_timeout() {
        let (endpoint, worker) = mock_directory(json_response(b"[]"), Duration::from_millis(250));
        let result = mock_client(endpoint, Duration::from_millis(100), MAX_DIRECTORY_BYTES).fetch();
        worker.join().unwrap();
        assert!(matches!(result, Err(ProviderError::Transport(_))));
    }
}
