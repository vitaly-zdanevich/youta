//! Bounded `SoundCloud` metadata and proxied audio through a configurable Soundcloak instance.
//!
//! Public `SoundCloud` permalinks remain the durable identity. Instance URLs and
//! search continuation tokens are ephemeral; signed CDN playback URLs are never
//! exposed by this adapter. Protocol: <https://git.maid.zone/stuff/soundcloak/src/branch/master/docs/API.md>.

use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use super::{DEFAULT_MAX_JSON_BYTES, DEFAULT_REQUEST_TIMEOUT, ProviderError, validate_base_url};

const MAX_QUERY_BYTES: usize = 512;
const MAX_QUERY_URN_BYTES: usize = 512;
const MAX_URL_BYTES: usize = 4_096;
const MAX_LABEL_BYTES: usize = 1_024;
const MAX_DESCRIPTION_BYTES: usize = 64 * 1_024;
const MAX_SEARCH_PAGE: u32 = 1_000;

/// Public default; users may explicitly configure another trusted instance.
pub const DEFAULT_SOUNDCLOAK_INSTANCE: &str = "https://sc1.maid.zone/";

/// One bounded offset page with an optional provider-issued search-session token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SoundcloakSearchRequest {
    /// User-entered search text, encoded as one query value.
    pub query: String,
    /// One-based page, bounded to prevent offset overflow.
    pub page: u32,
    /// Results per page; keep fixed across one result set.
    pub limit: usize,
    /// Validated token from the previous page, never a URL or client credential.
    pub query_urn: Option<String>,
}

impl SoundcloakSearchRequest {
    /// Validates the bounded query/page contract before contacting an instance.
    ///
    /// # Errors
    /// Rejects empty, oversized or control-bearing queries, invalid page sizes and tokens.
    pub fn validate(&self) -> Result<(), ProviderError> {
        if safe_label(&self.query, MAX_QUERY_BYTES).is_none()
            || !(1..=MAX_SEARCH_PAGE).contains(&self.page)
            || !(1..=100).contains(&self.limit)
            || self
                .query_urn
                .as_deref()
                .is_some_and(|token| !valid_query_urn(token))
        {
            return Err(ProviderError::InvalidRequest(
                "invalid Soundcloak search query, page, limit or continuation token".into(),
            ));
        }
        Ok(())
    }
}

/// Canonical `SoundCloud` metadata; no signed or third-party playback locator is retained.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SoundcloakTrack {
    /// `SoundCloud`'s numeric public track identifier, not the queue replay identity.
    pub id: String,
    /// Terminal-safe track title.
    pub title: String,
    /// Terminal-safe public artist display name.
    pub artist: String,
    /// Canonical public `SoundCloud` permalink used for queue, playlists and History.
    pub webpage_url: Url,
    /// Artwork routed through the configured instance, when valid CDN artwork exists.
    pub artwork_url: Option<Url>,
    /// Full track duration in whole seconds, when reported.
    pub duration_seconds: Option<u64>,
    /// Bounded terminal-safe description, when reported.
    pub description: Option<String>,
    /// True only for explicitly allowed, full-length, unencrypted playable audio.
    pub streamable: bool,
}

/// Normalized bounded results and an instance-independent continuation token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SoundcloakSearchPage {
    /// Valid track records, retaining upstream order.
    pub items: Vec<SoundcloakTrack>,
    /// Requested page number.
    pub page: u32,
    /// Next offset page only when the upstream continuation validates.
    pub next_page: Option<u32>,
    /// Validated search-session token to reuse with the next page.
    pub query_urn: Option<String>,
}

/// Injectable bounded metadata transport; production requests stay on the configured origin.
pub trait SoundcloakTransport: Send + Sync {
    /// Reads a single metadata response without following redirects.
    ///
    /// # Errors
    /// Returns a provider error for HTTP, transport or body-limit failures.
    fn fetch(&self, url: &Url, max_bytes: usize) -> Result<Vec<u8>, ProviderError>;
}

/// Blocking client; callers should run metadata requests on their provider worker.
#[derive(Clone)]
pub struct SoundcloakClient {
    base_url: Url,
    transport: Arc<dyn SoundcloakTransport>,
}

impl Default for SoundcloakClient {
    fn default() -> Self {
        Self::new(Url::parse(DEFAULT_SOUNDCLOAK_INSTANCE).expect("fixed Soundcloak instance URL"))
            .expect("fixed Soundcloak instance is a valid base URL")
    }
}

impl SoundcloakClient {
    /// Creates a redirect-free bounded metadata client for an explicitly trusted instance.
    ///
    /// HTTP and private-network instances are allowed only through this manual
    /// configuration, matching other self-hosted provider adapters.
    ///
    /// # Errors
    /// Rejects invalid, credential-bearing, oversized or ambiguous base URLs.
    pub fn new(base_url: Url) -> Result<Self, ProviderError> {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(DEFAULT_REQUEST_TIMEOUT))
            .max_redirects(0)
            .http_status_as_error(false)
            .user_agent(concat!("youta/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Self::with_transport(base_url, Arc::new(UreqSoundcloakTransport { agent }))
    }

    /// Installs an alternative bounded transport for deterministic offline callers/tests.
    ///
    /// # Errors
    /// Rejects an unsafe configured base before the transport can be contacted.
    pub fn with_transport(
        base_url: Url,
        transport: Arc<dyn SoundcloakTransport>,
    ) -> Result<Self, ProviderError> {
        if base_url.as_str().len() > 2_048 {
            return Err(ProviderError::InvalidBaseUrl(
                "Soundcloak base URL is too long".into(),
            ));
        }
        Ok(Self {
            base_url: validate_base_url(base_url)?,
            transport,
        })
    }

    /// Returns the normalized configured instance.
    #[must_use]
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// Searches through Soundcloak, reconstructing pagination without following upstream URLs.
    ///
    /// # Errors
    /// Returns an error for invalid requests, transport failures, oversized JSON or bad envelopes.
    pub fn search(
        &self,
        request: &SoundcloakSearchRequest,
    ) -> Result<SoundcloakSearchPage, ProviderError> {
        request.validate()?;
        let mut endpoint = self.endpoint("_/api/v2/search/tracks")?;
        let offset = u64::from(request.page - 1)
            * u64::try_from(request.limit).map_err(|_| {
                ProviderError::InvalidRequest("Soundcloak page size overflows".into())
            })?;
        {
            let mut query = endpoint.query_pairs_mut();
            query.append_pair("q", request.query.trim());
            query.append_pair("limit", &request.limit.to_string());
            query.append_pair("offset", &offset.to_string());
            query.append_pair("linked_partitioning", "1");
            if let Some(token) = &request.query_urn {
                query.append_pair("query_urn", token);
            }
        }
        let value = self.fetch_json(&endpoint)?;
        let collection = value["collection"]
            .as_array()
            .ok_or_else(|| invalid_response("missing track collection"))?;
        if collection.len() > request.limit {
            return Err(invalid_response("too many tracks in search page"));
        }
        let (next_page, query_urn) = continuation(&value, request)?;
        let items = collection
            .iter()
            .filter_map(|value| self.normalize_track(value))
            .collect();
        Ok(SoundcloakSearchPage {
            items,
            page: request.page,
            next_page,
            query_urn,
        })
    }

    /// Resolves a canonical public track without retaining a CDN playback locator.
    ///
    /// # Errors
    /// Rejects non-track URLs, invalid metadata, mismatched identity and network failures.
    pub fn resolve(&self, canonical_url: &Url) -> Result<SoundcloakTrack, ProviderError> {
        canonical_segments(canonical_url)?;
        let mut endpoint = self.endpoint("_/api/v2/resolve")?;
        endpoint
            .query_pairs_mut()
            .append_pair("url", canonical_url.as_str());
        let value = self.fetch_json(&endpoint)?;
        let track = self
            .normalize_track(&value)
            .ok_or_else(|| invalid_response("invalid resolved track"))?;
        if track.webpage_url != *canonical_url {
            return Err(invalid_response(
                "resolved track identity does not match requested permalink",
            ));
        }
        Ok(track)
    }

    /// Constructs the instance-owned HLS endpoint without initiating playback or a fetch.
    ///
    /// Explicit flags request AAC and same-instance manifests/segments. The
    /// chosen instance must support stream proxying to honor the segment flag.
    ///
    /// # Errors
    /// Rejects anything other than a canonical two-segment public `SoundCloud` track URL.
    pub fn stream_url(&self, canonical_url: &Url) -> Result<Url, ProviderError> {
        let (artist, track) = canonical_segments(canonical_url)?;
        let mut url = self.endpoint(&format!("_/api/hls/{artist}/{track}"))?;
        url.query_pairs_mut()
            .append_pair("audio", "aac")
            .append_pair("redirect", "false")
            .append_pair("redirect_parts", "false");
        Ok(url)
    }

    /// Maps a canonical track to its human-readable page on the configured instance.
    ///
    /// # Errors
    /// Rejects noncanonical or non-track `SoundCloud` URLs before constructing the instance link.
    pub fn page_url(&self, canonical_url: &Url) -> Result<Url, ProviderError> {
        let (artist, track) = canonical_segments(canonical_url)?;
        self.endpoint(&format!("{artist}/{track}"))
    }

    /// Joins only adapter-owned paths onto the manually configured base.
    fn endpoint(&self, path: &str) -> Result<Url, ProviderError> {
        self.base_url
            .join(path)
            .map_err(|_| ProviderError::InvalidBaseUrl("invalid Soundcloak endpoint".into()))
    }

    /// Applies the response byte limit even to injected transports before JSON allocation.
    fn fetch_json(&self, url: &Url) -> Result<Value, ProviderError> {
        let bytes = self.transport.fetch(url, DEFAULT_MAX_JSON_BYTES)?;
        if bytes.len() > DEFAULT_MAX_JSON_BYTES {
            return Err(ProviderError::ResponseTooLarge {
                limit: DEFAULT_MAX_JSON_BYTES,
            });
        }
        serde_json::from_slice(&bytes).map_err(|_| invalid_response("malformed JSON metadata"))
    }

    /// Converts display fields only, never retaining authorization or signed transcoding data.
    fn normalize_track(&self, value: &Value) -> Option<SoundcloakTrack> {
        if value["kind"].as_str() != Some("track") {
            return None;
        }
        let id = value["id"].as_u64().filter(|id| *id > 0)?.to_string();
        let title = safe_label(value["title"].as_str()?, MAX_LABEL_BYTES)?;
        let artist = safe_label(value["user"]["username"].as_str()?, MAX_LABEL_BYTES)?;
        let raw_url = value["permalink_url"].as_str()?;
        if raw_url.len() > MAX_URL_BYTES {
            return None;
        }
        let webpage_url = Url::parse(raw_url).ok()?;
        canonical_segments(&webpage_url).ok()?;
        let artwork_url = value["artwork_url"]
            .as_str()
            .and_then(|raw| self.artwork_url(raw));
        let duration = value
            .get("full_duration")
            .filter(|value| !value.is_null())
            .or_else(|| value.get("duration").filter(|value| !value.is_null()));
        let duration_seconds = if let Some(duration) = duration {
            Some(
                duration
                    .as_u64()
                    .filter(|milliseconds| *milliseconds <= 7 * 24 * 60 * 60 * 1_000)?
                    / 1_000,
            )
        } else {
            None
        };
        let description = value["description"]
            .as_str()
            .filter(|text| text.len() <= MAX_DESCRIPTION_BYTES)
            .map(|text| {
                text.replace("\r\n", "\n")
                    .replace('\r', "\n")
                    .replace('\t', " ")
            })
            .filter(|text| {
                !text
                    .chars()
                    .any(|character| character.is_control() && character != '\n')
            })
            .filter(|text| !text.trim().is_empty());
        let streamable = value["streamable"].as_bool() == Some(true)
            && value["policy"].as_str() == Some("ALLOW")
            && value["media"]["transcodings"]
                .as_array()
                .is_some_and(|transcodings| {
                    transcodings.len() <= 32
                        && transcodings.iter().any(|transcoding| {
                            matches!(
                                transcoding.get("snipped"),
                                None | Some(Value::Null | Value::Bool(false))
                            ) && transcoding["format"]["protocol"].as_str() == Some("hls")
                                && transcoding["format"]["mime_type"]
                                    .as_str()
                                    .is_some_and(|mime| mime.starts_with("audio/"))
                        })
                });
        Some(SoundcloakTrack {
            id,
            title,
            artist,
            webpage_url,
            artwork_url,
            duration_seconds,
            description,
            streamable,
        })
    }

    /// Proxies only bounded credential-free HTTPS artwork on `SoundCloud`'s own image CDN.
    fn artwork_url(&self, raw: &str) -> Option<Url> {
        if raw.len() > MAX_URL_BYTES {
            return None;
        }
        let artwork = Url::parse(raw).ok()?;
        if artwork.scheme() != "https"
            || artwork.port().is_some()
            || !artwork.username().is_empty()
            || artwork.password().is_some()
            || artwork.fragment().is_some()
            || artwork.query().is_some()
            || !artwork
                .host_str()
                .is_some_and(|host| host.ends_with(".sndcdn.com"))
            || crate::domain::remote_url_has_non_public_host(&artwork)
        {
            return None;
        }
        let mut url = self.endpoint("_/proxy/images").ok()?;
        url.query_pairs_mut().append_pair("url", artwork.as_str());
        Some(url)
    }
}

/// Enforces canonical durable identity before using either path segment in an endpoint.
fn canonical_segments(url: &Url) -> Result<(&str, &str), ProviderError> {
    let invalid = || {
        ProviderError::InvalidRequest(
            "expected a canonical https://soundcloud.com/artist/track URL".into(),
        )
    };
    if url.as_str().len() > MAX_URL_BYTES
        || url.scheme() != "https"
        || url.host_str() != Some("soundcloud.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    let mut segments = url.path_segments().ok_or_else(invalid)?;
    let artist = segments.next().ok_or_else(invalid)?;
    let track = segments.next().ok_or_else(invalid)?;
    if segments.next().is_some()
        || !valid_slug(artist)
        || !valid_slug(track)
        || matches!(
            artist,
            "search"
                | "discover"
                | "you"
                | "stream"
                | "upload"
                | "charts"
                | "stations"
                | "settings"
                | "people"
                | "groups"
                | "tags"
                | "pages"
        )
        || matches!(
            track,
            "sets" | "tracks" | "albums" | "reposts" | "likes" | "popular-tracks"
        )
    {
        return Err(invalid());
    }
    Ok((artist, track))
}

/// Public `SoundCloud` aliases use bounded URL-safe ASCII, never encoded separators.
fn valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Display labels are trimmed once and reject terminal controls before retention.
fn safe_label(value: &str, max_bytes: usize) -> Option<String> {
    (!value.trim().is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control))
        .then(|| value.trim().to_owned())
}

/// Search-session tokens are opaque query values, not executable paths or HTTP credentials.
fn valid_query_urn(token: &str) -> bool {
    !token.trim().is_empty()
        && token.len() <= MAX_QUERY_URN_BYTES
        && !token.chars().any(char::is_control)
}

/// Validates the next offset and carries only the search token, never its upstream URL.
fn continuation(
    value: &Value,
    request: &SoundcloakSearchRequest,
) -> Result<(Option<u32>, Option<String>), ProviderError> {
    let token = value
        .get("query_urn")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .filter(|token| valid_query_urn(token))
                .map(str::to_owned)
                .ok_or_else(|| invalid_response("invalid search continuation token"))
        })
        .transpose()?;
    let Some(raw_next) = value.get("next_href").filter(|value| !value.is_null()) else {
        return Ok((None, token.or_else(|| request.query_urn.clone())));
    };
    let raw_next = raw_next
        .as_str()
        .filter(|value| value.len() <= MAX_URL_BYTES)
        .ok_or_else(|| invalid_response("invalid search continuation URL"))?;
    let next =
        Url::parse(raw_next).map_err(|_| invalid_response("invalid search continuation URL"))?;
    if next.scheme() != "https"
        || next.host_str() != Some("api-v2.soundcloud.com")
        || next.path() != "/search/tracks"
        || next.port().is_some()
        || !next.username().is_empty()
        || next.password().is_some()
        || next.fragment().is_some()
    {
        return Err(invalid_response("untrusted search continuation URL"));
    }
    let mut seen = HashSet::new();
    let mut next_offset = None;
    let mut next_limit = None;
    let mut next_token = None;
    for (key, value) in next.query_pairs() {
        if matches!(key.as_ref(), "q" | "limit" | "offset" | "query_urn")
            && !seen.insert(key.to_string())
        {
            return Err(invalid_response("duplicate search continuation parameter"));
        }
        match key.as_ref() {
            "q" if value != request.query.trim() => {
                return Err(invalid_response("search continuation changed its query"));
            }
            "limit" => next_limit = value.parse::<usize>().ok(),
            "offset" => next_offset = value.parse::<u64>().ok(),
            "query_urn" => {
                if !valid_query_urn(&value) {
                    return Err(invalid_response("invalid search continuation token"));
                }
                next_token = Some(value.into_owned());
            }
            _ => {} // In particular, never copy upstream client_id into the instance request.
        }
    }
    let expected_offset = u64::from(request.page)
        * u64::try_from(request.limit).map_err(|_| invalid_response("search limit overflows"))?;
    if next_offset != Some(expected_offset) || next_limit != Some(request.limit) {
        return Err(invalid_response(
            "search continuation has an unexpected offset or limit",
        ));
    }
    if let (Some(left), Some(right)) = (&token, &next_token)
        && left != right
    {
        return Err(invalid_response("inconsistent search continuation token"));
    }
    Ok((
        (request.page < MAX_SEARCH_PAGE).then_some(request.page + 1),
        next_token.or(token).or_else(|| request.query_urn.clone()),
    ))
}

/// Keeps remote schema failures finite and independent of untrusted response text.
fn invalid_response(message: &str) -> ProviderError {
    ProviderError::InvalidResponse(format!("Soundcloak: {message}"))
}

/// Redirects are rejected before they can move a metadata request off the trusted instance.
struct UreqSoundcloakTransport {
    agent: ureq::Agent,
}

impl SoundcloakTransport for UreqSoundcloakTransport {
    fn fetch(&self, url: &Url, max_bytes: usize) -> Result<Vec<u8>, ProviderError> {
        let mut response = self
            .agent
            .get(url.as_str())
            .header("Accept", "application/json")
            .call()
            .map_err(|error| ProviderError::Transport(error.to_string()))?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(ProviderError::HttpStatus(status));
        }
        if response
            .body()
            .content_length()
            .is_some_and(|length| length > max_bytes as u64)
        {
            return Err(ProviderError::ResponseTooLarge { limit: max_bytes });
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit(max_bytes.saturating_add(1) as u64)
            .read_to_vec()
            .map_err(|error| match error {
                ureq::Error::BodyExceedsLimit(_) => {
                    ProviderError::ResponseTooLarge { limit: max_bytes }
                }
                other => ProviderError::Transport(other.to_string()),
            })?;
        if bytes.len() > max_bytes {
            return Err(ProviderError::ResponseTooLarge { limit: max_bytes });
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpListener};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// Records exact proxy requests and supplies only finite fixture bodies.
    #[derive(Default)]
    struct MockTransport {
        requests: Mutex<Vec<Url>>,
        responses: Mutex<VecDeque<Vec<u8>>>,
    }

    impl SoundcloakTransport for MockTransport {
        fn fetch(&self, url: &Url, _: usize) -> Result<Vec<u8>, ProviderError> {
            self.requests.lock().unwrap().push(url.clone());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(ProviderError::HttpStatus(503))
        }
    }

    fn fixture_track() -> Value {
        json!({"id": 123456, "kind": "track", "title": "Ambient – fixture", "duration": 120500,
            "full_duration": 120500, "permalink_url": "https://soundcloud.com/artist/track",
            "artwork_url": "https://i1.sndcdn.com/art-large.jpg", "description": "Line one\r\nLine two",
            "streamable": true, "policy": "ALLOW", "user": {"username": "Artist"},
            "media": {"transcodings": [{"snipped": false,
                "format": {"protocol": "hls", "mime_type": "audio/aac"},
                "url": "https://signed.example.test/do-not-retain?secret=1"}]}})
    }

    fn request() -> SoundcloakSearchRequest {
        SoundcloakSearchRequest {
            query: "ambient & piano".into(),
            page: 1,
            limit: 2,
            query_urn: None,
        }
    }

    fn client(values: Vec<Value>) -> (SoundcloakClient, Arc<MockTransport>) {
        let transport = Arc::new(MockTransport {
            requests: Mutex::default(),
            responses: Mutex::new(
                values
                    .iter()
                    .map(|value| serde_json::to_vec(value).unwrap())
                    .collect(),
            ),
        });
        (
            SoundcloakClient::with_transport(
                Url::parse(DEFAULT_SOUNDCLOAK_INSTANCE).unwrap(),
                transport.clone(),
            )
            .unwrap(),
            transport,
        )
    }

    /// Serves one finite HTTP response with bounded socket and accept waits.
    fn http_fixture(response: Vec<u8>) -> (Url, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "mock Soundcloak accept timed out"
                        );
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("mock Soundcloak accept failed: {error}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 8_192, "oversized mock request");
                let mut byte = [0_u8; 1];
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let _ = socket.write_all(&response);
            String::from_utf8(request).unwrap()
        });
        (
            Url::parse(&format!("http://{address}/prefix/")).unwrap(),
            worker,
        )
    }

    #[test]
    fn configured_instance_http_is_bounded_and_never_follows_redirects() {
        let body = serde_json::to_string(&json!({"collection": [fixture_track()]})).unwrap();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
        let (base, worker) = http_fixture(response);
        let client = SoundcloakClient::new(base).unwrap();
        assert_eq!(client.search(&request()).unwrap().items.len(), 1);
        let request = worker.join().unwrap();
        assert!(request.starts_with("GET /prefix/_/api/v2/search/tracks?"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("\r\naccept: application/json\r\n")
        );
        assert!(!request.to_ascii_lowercase().contains("\r\nauthorization:"));

        let destination = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        destination.set_nonblocking(true).unwrap();
        let response = format!("HTTP/1.1 302 Found\r\nLocation: http://{}/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", destination.local_addr().unwrap()).into_bytes();
        let (base, worker) = http_fixture(response);
        assert!(matches!(
            SoundcloakClient::new(base)
                .unwrap()
                .search(&self::request()),
            Err(ProviderError::HttpStatus(302))
        ));
        worker.join().unwrap();
        assert_eq!(
            destination.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn http_transport_rejects_status_and_bodies_beyond_the_bound() {
        for response in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\n123456789".to_vec(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n9\r\n123456789\r\n0\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n123456789".to_vec(),
        ] {
            let (base, worker) = http_fixture(response);
            let client = SoundcloakClient::new(base.clone()).unwrap();
            assert!(matches!(client.transport.fetch(&base, 8), Err(ProviderError::ResponseTooLarge { limit: 8 })));
            worker.join().unwrap();
        }
        let (base, worker) = http_fixture(
            b"HTTP/1.1 500 Error\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".to_vec(),
        );
        assert!(matches!(
            SoundcloakClient::new(base).unwrap().search(&request()),
            Err(ProviderError::HttpStatus(500))
        ));
        worker.join().unwrap();
    }

    #[test]
    fn search_normalizes_canonical_metadata_and_proxies_artwork() {
        let (client, transport) = client(vec![
            json!({"collection": [fixture_track()], "next_href": null}),
        ]);
        let page = client.search(&request()).unwrap();
        let track = &page.items[0];
        assert_eq!(track.id, "123456");
        assert_eq!(track.artist, "Artist");
        assert_eq!(track.duration_seconds, Some(120));
        assert_eq!(track.description.as_deref(), Some("Line one\nLine two"));
        assert!(track.streamable);
        assert_eq!(
            track.webpage_url.as_str(),
            "https://soundcloud.com/artist/track"
        );
        let artwork = track.artwork_url.as_ref().unwrap();
        assert_eq!(artwork.host_str(), Some("sc1.maid.zone"));
        assert_eq!(artwork.path(), "/_/proxy/images");
        assert!(
            serde_json::to_string(track)
                .unwrap()
                .find("secret")
                .is_none()
        );
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests[0].path(), "/_/api/v2/search/tracks");
        assert!(
            requests[0]
                .query_pairs()
                .any(|(key, value)| key == "q" && value == "ambient & piano")
        );
    }

    #[test]
    fn pagination_keeps_validated_query_token_but_never_follows_upstream_urls() {
        let next = "https://api-v2.soundcloud.com/search/tracks?q=ambient%20%26%20piano&limit=2&offset=2&query_urn=soundcloud%3Asearch%3Afixture&client_id=secret";
        let (client, transport) = client(vec![
            json!({"collection": [fixture_track(), fixture_track()], "next_href": next}),
            json!({"collection": [], "next_href": null}),
        ]);
        let page = client.search(&request()).unwrap();
        assert_eq!(page.next_page, Some(2));
        assert_eq!(page.query_urn.as_deref(), Some("soundcloud:search:fixture"));
        let mut next_request = request();
        next_request.page = page.next_page.unwrap();
        next_request.query_urn = page.query_urn;
        client.search(&next_request).unwrap();
        let requests = transport.requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .all(|url| url.host_str() == Some("sc1.maid.zone"))
        );
        assert!(!requests[1].as_str().contains("client_id"));
        assert!(
            requests[1]
                .query_pairs()
                .any(|(key, value)| key == "offset" && value == "2")
        );
        assert!(
            requests[1]
                .query_pairs()
                .any(|(key, value)| key == "query_urn" && value == "soundcloud:search:fixture")
        );
    }

    #[test]
    fn restricted_preview_and_encrypted_tracks_are_not_full_length_playback() {
        for policy in ["SNIP", "BLOCK", "MONETIZE", "UNKNOWN"] {
            let mut track = fixture_track();
            track["policy"] = json!(policy);
            let (client, _) = client(vec![json!({"collection": [track]})]);
            assert!(
                !client.search(&request()).unwrap().items[0].streamable,
                "{policy}"
            );
        }
        for (field, value) in [
            ("streamable", Value::Null),
            ("media", json!({"transcodings": []})),
        ] {
            let mut track = fixture_track();
            track[field] = value;
            let (client, _) = client(vec![json!({"collection": [track]})]);
            assert!(!client.search(&request()).unwrap().items[0].streamable);
        }
        for protocol in [
            "encrypted-hls",
            "ctr-encrypted-hls",
            "cbc-encrypted-hls",
            "progressive",
        ] {
            let mut track = fixture_track();
            track["media"]["transcodings"][0]["format"]["protocol"] = json!(protocol);
            let (client, _) = client(vec![json!({"collection": [track]})]);
            assert!(!client.search(&request()).unwrap().items[0].streamable);
        }
        for snipped in [json!(true), json!("true"), json!(1)] {
            let mut track = fixture_track();
            track["media"]["transcodings"][0]["snipped"] = snipped;
            let (client, _) = client(vec![json!({"collection": [track]})]);
            assert!(!client.search(&request()).unwrap().items[0].streamable);
        }
    }

    #[test]
    fn stream_url_is_instance_owned_and_canonical_validation_is_strict() {
        let client = SoundcloakClient::default();
        let url = client
            .stream_url(&Url::parse("https://soundcloud.com/artist/track").unwrap())
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://sc1.maid.zone/_/api/hls/artist/track?audio=aac&redirect=false&redirect_parts=false"
        );
        assert_eq!(
            client
                .page_url(&Url::parse("https://soundcloud.com/artist/track").unwrap())
                .unwrap()
                .as_str(),
            "https://sc1.maid.zone/artist/track"
        );
        for invalid in [
            "http://soundcloud.com/artist/track",
            "https://soundcloud.com.evil.test/artist/track",
            "https://evil@soundcloud.com/artist/track",
            "https://soundcloud.com:444/artist/track",
            "https://soundcloud.com/artist/sets",
            "https://soundcloud.com/artist/track/extra",
            "https://soundcloud.com/artist/track?secret_token=secret",
            "https://soundcloud.com/artist/%2Ftrack",
            "https://soundcloud.com/search/tracks",
            "https://soundcloud.com/artist/track#fragment",
        ] {
            assert!(
                client.stream_url(&Url::parse(invalid).unwrap()).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn resolve_uses_proxy_and_refuses_mismatched_identity() {
        let (client, transport) = client(vec![fixture_track(), fixture_track()]);
        let expected = Url::parse("https://soundcloud.com/artist/track").unwrap();
        assert_eq!(client.resolve(&expected).unwrap().webpage_url, expected);
        assert_eq!(
            transport.requests.lock().unwrap()[0].path(),
            "/_/api/v2/resolve"
        );
        assert!(
            client
                .resolve(&Url::parse("https://soundcloud.com/other/track").unwrap())
                .is_err()
        );
    }

    #[test]
    fn invalid_requests_and_untrusted_pagination_are_rejected() {
        let (client, transport) = client(vec![]);
        for (page, limit, query) in [
            (0, 2, "ok"),
            (1, 0, "ok"),
            (1, 101, "ok"),
            (1, 2, ""),
            (1, 2, "bad\u{1b}"),
        ] {
            let request = SoundcloakSearchRequest {
                query: query.into(),
                page,
                limit,
                query_urn: None,
            };
            assert!(client.search(&request).is_err());
        }
        assert!(transport.requests.lock().unwrap().is_empty());
        for next in [
            "https://localhost/search/tracks?offset=2&limit=2",
            "https://api-v2.soundcloud.com/resolve?offset=2&limit=2",
            "https://api-v2.soundcloud.com/search/tracks?offset=0&limit=2",
            "https://api-v2.soundcloud.com/search/tracks?offset=4&limit=2",
            "https://api-v2.soundcloud.com/search/tracks?offset=2&limit=100",
        ] {
            let (client, _) = self::client(vec![
                json!({"collection": [fixture_track()], "next_href": next}),
            ]);
            assert!(client.search(&request()).is_err(), "{next}");
        }
    }

    /// Opt-in smoke check of the production transport and current public API schema.
    #[test]
    #[ignore = "contacts the public Soundcloak instance; no audio is played"]
    fn live_soundcloak_search_and_continuation() {
        let client = SoundcloakClient::default();
        let mut request = SoundcloakSearchRequest {
            query: "ambient".to_owned(),
            page: 1,
            limit: 2,
            query_urn: None,
        };
        let first = client.search(&request).expect("public Soundcloak search");
        assert!(!first.items.is_empty());
        let track = &first.items[0];
        assert_eq!(
            client.page_url(&track.webpage_url).unwrap().origin(),
            client.base_url().origin()
        );
        assert_eq!(
            client.stream_url(&track.webpage_url).unwrap().origin(),
            client.base_url().origin()
        );
        if let Some(page) = first.next_page {
            request.page = page;
            request.query_urn = first.query_urn;
            let second = client
                .search(&request)
                .expect("public Soundcloak continuation");
            assert!(!second.items.is_empty());
            assert_ne!(first.items[0].id, second.items[0].id);
        }
    }

    #[test]
    fn malformed_oversized_and_unsafe_metadata_cannot_escape_bounds() {
        for value in [
            json!({}),
            json!({"collection": {}}),
            json!({"collection": [fixture_track(), fixture_track(), fixture_track()]}),
        ] {
            let (client, _) = client(vec![value]);
            assert!(client.search(&request()).is_err());
        }
        for (field, value) in [
            ("title", json!("bad\u{1b}name")),
            ("title", json!("a".repeat(4097))),
            ("permalink_url", json!("https://localhost/a/b")),
            ("kind", json!("playlist")),
        ] {
            let mut track = fixture_track();
            track[field] = value;
            let (client, _) = client(vec![json!({"collection": [track]})]);
            assert!(client.search(&request()).unwrap().items.is_empty());
        }
        let mut track = fixture_track();
        track["artwork_url"] = json!("https://127.0.0.1/private");
        let (client, _) = client(vec![json!({"collection": [track]})]);
        assert!(
            client.search(&request()).unwrap().items[0]
                .artwork_url
                .is_none()
        );
        let (_, transport) = self::client(vec![]);
        transport
            .responses
            .lock()
            .unwrap()
            .push_back(vec![b' '; DEFAULT_MAX_JSON_BYTES + 1]);
        let client = SoundcloakClient::with_transport(
            Url::parse(DEFAULT_SOUNDCLOAK_INSTANCE).unwrap(),
            transport,
        )
        .unwrap();
        assert!(matches!(
            client.search(&request()),
            Err(ProviderError::ResponseTooLarge { .. })
        ));
    }
}
