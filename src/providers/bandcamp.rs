//! Public Bandcamp discovery and explicitly action-triggered media resolution.
//!
//! Bandcamp does not publish a credential-free search API for player clients.
//! Search therefore uses the public HTML page on a best-effort basis, with a
//! fixed HTTPS origin, no redirects, a response-size limit, and a strict
//! canonical-result allowlist. HTML changes can yield an empty page or a
//! provider error; they can never redirect Youta to an arbitrary origin.
//!
//! Search results contain only public metadata and canonical track/album
//! pages. They do not resolve download URLs. [`BandcampResolver`] invokes
//! `yt-dlp` only for an explicit playback, download, or autoplay action. It
//! passes no cookies and claims no access to authenticated purchases. A
//! free-download release may still consume an artist-configured download
//! allocation when the user performs one of those explicit actions.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use url::Url;

use crate::config::BandcampAudioFormat;
use crate::playback::ytdlp::ResolvedMedia;
use crate::playback::{PlaybackError, Result as PlaybackResult};

use super::ProviderError;

const SEARCH_ENDPOINT: &str = "https://bandcamp.com/search";
const DEFAULT_MAX_SEARCH_HTML_BYTES: usize = 2 * 1024 * 1024;
const MAX_CONFIGURED_SEARCH_HTML_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_MAX_SEARCH_RESULTS: usize = 32;
const MAX_SEARCH_RESULTS: usize = 64;
const MAX_SEARCH_QUERY_BYTES: usize = 256;
const MAX_SEARCH_PAGE: u16 = 100;
const MAX_TITLE_BYTES: usize = 512;
const MAX_ARTIST_BYTES: usize = 256;
const DEFAULT_RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RESOLVE_TIMEOUT: Duration = Duration::from_mins(2);
const DEFAULT_MAX_RESOLVE_JSON_BYTES: usize = 4 * 1024 * 1024;
const MAX_RESOLVE_JSON_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESOLVE_STDERR_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_ALBUM_TRACKS: u16 = 200;
const MAX_ALBUM_TRACKS: u16 = 500;
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Canonical Bandcamp page kind accepted by Youta.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BandcampMediaKind {
    /// One playable release track.
    Track,
    /// One release album whose tracks are resolved only for an explicit action.
    Album,
}

impl BandcampMediaKind {
    const fn path_component(self) -> &'static str {
        match self {
            Self::Track => "track",
            Self::Album => "album",
        }
    }
}

/// Validated canonical `https://artist.bandcamp.com/{track|album}/slug` URL.
///
/// Query strings, fragments, credentials, ports, bare/global Bandcamp hosts,
/// nested subdomains, non-HTTPS schemes, and noncanonical path characters are
/// rejected. Deserialization repeats the same validation.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BandcampMediaUrl {
    url: Url,
    kind: BandcampMediaKind,
    artist_slug: String,
    release_slug: String,
}

impl BandcampMediaUrl {
    /// Parses one strict canonical Bandcamp track or album URL.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] when any canonical URL rule
    /// is violated.
    pub fn parse(url: Url) -> Result<Self, ProviderError> {
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid_bandcamp_url());
        }
        let host = url
            .host_str()
            .and_then(|host| host.strip_suffix(".bandcamp.com"))
            .filter(|artist| !artist.contains('.'))
            .filter(|artist| *artist != "www")
            .filter(|artist| valid_dns_label(artist))
            .ok_or_else(invalid_bandcamp_url)?
            .to_owned();
        let segments = url
            .path_segments()
            .map(|segments| segments.map(str::to_owned).collect::<Vec<_>>())
            .ok_or_else(invalid_bandcamp_url)?;
        let [kind, release_slug] = segments.as_slice() else {
            return Err(invalid_bandcamp_url());
        };
        let kind = match kind.as_str() {
            "track" => BandcampMediaKind::Track,
            "album" => BandcampMediaKind::Album,
            _ => return Err(invalid_bandcamp_url()),
        };
        if !valid_release_slug(release_slug) {
            return Err(invalid_bandcamp_url());
        }

        Ok(Self {
            url,
            kind,
            artist_slug: host,
            release_slug: release_slug.clone(),
        })
    }

    /// Parses one strict canonical Bandcamp URL string.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] for malformed or
    /// noncanonical input.
    pub fn parse_str(value: &str) -> Result<Self, ProviderError> {
        let url = Url::parse(value).map_err(|_| invalid_bandcamp_url())?;
        Self::parse(url)
    }

    /// Returns the canonical URL.
    #[must_use]
    pub const fn as_url(&self) -> &Url {
        &self.url
    }

    /// Returns whether this page identifies a track or album.
    #[must_use]
    pub const fn kind(&self) -> BandcampMediaKind {
        self.kind
    }

    /// Returns the canonical artist/label subdomain.
    #[must_use]
    pub fn artist_slug(&self) -> &str {
        &self.artist_slug
    }

    /// Returns the canonical track or album slug.
    #[must_use]
    pub fn release_slug(&self) -> &str {
        &self.release_slug
    }

    /// Returns a provider-stable ID suitable for history and queue identity.
    #[must_use]
    pub fn stable_id(&self) -> String {
        format!(
            "{}/{}/{}",
            self.artist_slug,
            self.kind.path_component(),
            self.release_slug
        )
    }

    /// Consumes the wrapper and returns its validated URL.
    #[must_use]
    pub fn into_url(self) -> Url {
        self.url
    }
}

impl Serialize for BandcampMediaUrl {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.url.as_str())
    }
}

impl<'de> Deserialize<'de> for BandcampMediaUrl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse_str(&value).map_err(serde::de::Error::custom)
    }
}

/// One bounded public Bandcamp search result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BandcampSearchResult {
    /// Canonical track or album page; no stream is resolved during search.
    pub media: BandcampMediaUrl,
    /// Bounded human-readable release title.
    pub title: String,
    /// Bounded artist or label display name, when exposed by the search page.
    pub artist: Option<String>,
    /// Strictly allowlisted public Bandcamp CDN artwork, when exposed.
    pub artwork_url: Option<Url>,
}

/// One bounded page returned by public Bandcamp search.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BandcampSearchPage {
    /// One-based public search page.
    pub page: u16,
    /// Canonical track and album results only.
    pub results: Vec<BandcampSearchResult>,
    /// Next sequential page when the public HTML advertises one.
    pub next_page: Option<u16>,
}

/// Fetches one already validated public Bandcamp search URL.
///
/// Implementations may be supplied for deterministic tests. The client checks
/// the endpoint allowlist and body-size bound again after every call.
pub trait BandcampSearchTransport: Send + Sync {
    /// Returns the raw public search HTML.
    ///
    /// # Errors
    ///
    /// Returns a provider error for transport, status, or size failures.
    fn fetch(&self, url: &Url, max_bytes: usize) -> Result<Vec<u8>, ProviderError>;
}

#[derive(Clone)]
struct UreqBandcampSearchTransport {
    agent: ureq::Agent,
}

impl BandcampSearchTransport for UreqBandcampSearchTransport {
    fn fetch(&self, url: &Url, max_bytes: usize) -> Result<Vec<u8>, ProviderError> {
        validate_search_endpoint(url)?;
        let mut response = self
            .agent
            .get(url.as_str())
            .header("Accept", "text/html")
            .call()
            .map_err(map_ureq_error)?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
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
            .limit(u64::try_from(max_bytes.saturating_add(1)).unwrap_or(u64::MAX))
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

/// Low-resource, best-effort client for Bandcamp's public search page.
#[derive(Clone)]
pub struct BandcampSearchClient {
    transport: Arc<dyn BandcampSearchTransport>,
    max_html_bytes: usize,
    max_results: usize,
}

impl fmt::Debug for BandcampSearchClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BandcampSearchClient")
            .field("max_html_bytes", &self.max_html_bytes)
            .field("max_results", &self.max_results)
            .finish_non_exhaustive()
    }
}

impl Default for BandcampSearchClient {
    fn default() -> Self {
        Self::new()
    }
}

impl BandcampSearchClient {
    /// Creates a public search client with conservative network and size bounds.
    #[must_use]
    pub fn new() -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(super::DEFAULT_REQUEST_TIMEOUT))
            .https_only(true)
            .max_redirects(0)
            .user_agent(concat!(
                "youta/",
                env!("CARGO_PKG_VERSION"),
                " (+",
                env!("CARGO_PKG_REPOSITORY"),
                ")"
            ))
            .build()
            .into();
        Self {
            transport: Arc::new(UreqBandcampSearchTransport { agent }),
            max_html_bytes: DEFAULT_MAX_SEARCH_HTML_BYTES,
            max_results: DEFAULT_MAX_SEARCH_RESULTS,
        }
    }

    /// Creates a client around a custom transport with explicit hard bounds.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] for a zero or excessive HTML
    /// or result limit.
    pub fn with_transport(
        transport: Arc<dyn BandcampSearchTransport>,
        max_html_bytes: usize,
        max_results: usize,
    ) -> Result<Self, ProviderError> {
        if !(1..=MAX_CONFIGURED_SEARCH_HTML_BYTES).contains(&max_html_bytes) {
            return Err(ProviderError::InvalidRequest(format!(
                "Bandcamp search HTML limit must be between 1 and {MAX_CONFIGURED_SEARCH_HTML_BYTES} bytes"
            )));
        }
        if !(1..=MAX_SEARCH_RESULTS).contains(&max_results) {
            return Err(ProviderError::InvalidRequest(format!(
                "Bandcamp search result limit must be between 1 and {MAX_SEARCH_RESULTS}"
            )));
        }
        Ok(Self {
            transport,
            max_html_bytes,
            max_results,
        })
    }

    /// Searches public Bandcamp track and album pages without resolving media.
    ///
    /// Search is deliberately best-effort because the public HTML is not a
    /// stable API. Unsupported result types and noncanonical URLs are ignored.
    ///
    /// # Errors
    ///
    /// Returns a provider error for invalid query/page bounds, transport or
    /// status failure, an oversized/non-UTF-8 response, or an invalid outbound
    /// endpoint.
    pub fn search(&self, query: &str, page: u16) -> Result<BandcampSearchPage, ProviderError> {
        let query = query.trim();
        validate_search_input(query, page)?;
        let mut url = Url::parse(SEARCH_ENDPOINT)
            .map_err(|error| ProviderError::InvalidBaseUrl(error.to_string()))?;
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("page", &page.to_string());
        validate_search_endpoint(&url)?;
        let bytes = self.transport.fetch(&url, self.max_html_bytes)?;
        if bytes.len() > self.max_html_bytes {
            return Err(ProviderError::ResponseTooLarge {
                limit: self.max_html_bytes,
            });
        }
        let html = std::str::from_utf8(&bytes).map_err(|error| {
            ProviderError::InvalidResponse(format!(
                "Bandcamp search HTML was not valid UTF-8: {error}"
            ))
        })?;
        let (results, has_next) = parse_search_html(html, self.max_results);
        Ok(BandcampSearchPage {
            page,
            results,
            next_page: has_next
                .then(|| page.saturating_add(1))
                .filter(|next| *next <= MAX_SEARCH_PAGE),
        })
    }
}

/// User action allowed to trigger short-lived Bandcamp URL resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BandcampResolvePurpose {
    /// Resolve immediately before starting selected-track playback.
    Playback,
    /// Resolve immediately before an explicit download.
    Download,
    /// Resolve the next queued Bandcamp entry during enabled autoplay.
    Autoplay,
}

/// Bounded result of one explicit Bandcamp resolution action.
#[derive(Clone, Debug, PartialEq)]
pub struct BandcampResolution {
    /// Canonical page from which the media was resolved.
    pub source: BandcampMediaUrl,
    /// Explicit action that authorized resolution.
    pub purpose: BandcampResolvePurpose,
    /// Closed format preference used to choose the returned stream(s).
    pub format: BandcampAudioFormat,
    /// One track, or the bounded playable entries of an album.
    pub tracks: Vec<ResolvedMedia>,
    /// Whether the configured album-entry ceiling may have truncated results.
    pub possibly_truncated: bool,
}

#[derive(Clone, Debug)]
struct YtDlpInvocation {
    executable: PathBuf,
    arguments: Vec<String>,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
}

#[derive(Clone, Debug)]
struct SupervisedOutput {
    success: bool,
    status: String,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

trait BandcampCommandRunner: Send + Sync {
    fn run(&self, invocation: &YtDlpInvocation) -> PlaybackResult<SupervisedOutput>;
}

#[derive(Debug, Default)]
struct ProcessBandcampCommandRunner;

impl BandcampCommandRunner for ProcessBandcampCommandRunner {
    fn run(&self, invocation: &YtDlpInvocation) -> PlaybackResult<SupervisedOutput> {
        run_supervised(invocation)
    }
}

/// Supervised public Bandcamp resolver backed by the external `yt-dlp`.
#[derive(Clone)]
pub struct BandcampResolver {
    executable: PathBuf,
    timeout: Duration,
    max_json_bytes: usize,
    max_album_tracks: u16,
    runner: Arc<dyn BandcampCommandRunner>,
}

impl fmt::Debug for BandcampResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BandcampResolver")
            .field("executable", &self.executable)
            .field("timeout", &self.timeout)
            .field("max_json_bytes", &self.max_json_bytes)
            .field("max_album_tracks", &self.max_album_tracks)
            .finish_non_exhaustive()
    }
}

impl BandcampResolver {
    /// Creates a resolver with conservative process, output, and album bounds.
    #[must_use]
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            timeout: DEFAULT_RESOLVE_TIMEOUT,
            max_json_bytes: DEFAULT_MAX_RESOLVE_JSON_BYTES,
            max_album_tracks: DEFAULT_MAX_ALBUM_TRACKS,
            runner: Arc::new(ProcessBandcampCommandRunner),
        }
    }

    /// Creates a resolver with explicit process and output bounds.
    ///
    /// # Errors
    ///
    /// Returns [`PlaybackError::InvalidValue`] for an empty executable, zero
    /// or excessive timeout/output limit, or zero/excessive album limit.
    pub fn with_limits(
        executable: impl Into<PathBuf>,
        timeout: Duration,
        max_json_bytes: usize,
        max_album_tracks: u16,
    ) -> PlaybackResult<Self> {
        let executable = executable.into();
        validate_resolver_limits(&executable, timeout, max_json_bytes, max_album_tracks)?;
        Ok(Self {
            executable,
            timeout,
            max_json_bytes,
            max_album_tracks,
            runner: Arc::new(ProcessBandcampCommandRunner),
        })
    }

    #[cfg(test)]
    fn with_runner(
        executable: impl Into<PathBuf>,
        timeout: Duration,
        max_json_bytes: usize,
        max_album_tracks: u16,
        runner: Arc<dyn BandcampCommandRunner>,
    ) -> PlaybackResult<Self> {
        let executable = executable.into();
        validate_resolver_limits(&executable, timeout, max_json_bytes, max_album_tracks)?;
        Ok(Self {
            executable,
            timeout,
            max_json_bytes,
            max_album_tracks,
            runner,
        })
    }

    /// Resolves public media only for an explicit playback/download/autoplay
    /// action.
    ///
    /// The command ignores user configuration and plugins, receives one
    /// already canonical URL as a distinct argument, and uses only the static
    /// selector owned by [`BandcampAudioFormat`]. No cookies are supplied, so
    /// this API does not provide authenticated-purchase access.
    ///
    /// # Errors
    ///
    /// Returns a playback error when `yt-dlp` is missing, exceeds the deadline
    /// or output bounds, exits unsuccessfully, emits invalid JSON, reports a
    /// non-Bandcamp extractor, or returns unsafe media metadata.
    pub fn resolve(
        &self,
        source: &BandcampMediaUrl,
        format: BandcampAudioFormat,
        purpose: BandcampResolvePurpose,
    ) -> PlaybackResult<BandcampResolution> {
        validate_resolver_limits(
            &self.executable,
            self.timeout,
            self.max_json_bytes,
            self.max_album_tracks,
        )?;
        let invocation = YtDlpInvocation {
            executable: self.executable.clone(),
            arguments: vec![
                "--ignore-config".to_owned(),
                "--no-plugin-dirs".to_owned(),
                "--no-cache-dir".to_owned(),
                "--dump-single-json".to_owned(),
                "--skip-download".to_owned(),
                "--playlist-end".to_owned(),
                self.max_album_tracks.to_string(),
                "--format".to_owned(),
                format.yt_dlp_selector().to_owned(),
                "--".to_owned(),
                source.as_url().as_str().to_owned(),
            ],
            timeout: self.timeout,
            stdout_limit: self.max_json_bytes,
            stderr_limit: MAX_RESOLVE_STDERR_BYTES,
        };
        let output = self.runner.run(&invocation)?;
        if output.stdout.len() > self.max_json_bytes {
            return Err(PlaybackError::Protocol(format!(
                "Bandcamp yt-dlp metadata exceeded {} bytes",
                self.max_json_bytes
            )));
        }
        if !output.success {
            return Err(PlaybackError::ProcessExited(format!(
                " ({}; {} diagnostic bytes retained)",
                output.status,
                output.stderr.len()
            )));
        }
        let raw: RawResolvedMedia = serde_json::from_slice(&output.stdout)?;
        let root_is_bandcamp = raw.extractor_name().is_some_and(is_bandcamp_extractor);
        if !root_is_bandcamp {
            return Err(PlaybackError::Protocol(
                "yt-dlp did not use a Bandcamp extractor".to_owned(),
            ));
        }
        let raw_tracks = if raw.entries.is_empty() {
            vec![raw]
        } else {
            raw.entries
        };
        if raw_tracks.is_empty() {
            return Err(PlaybackError::Protocol(
                "Bandcamp resolution returned no playable tracks".to_owned(),
            ));
        }
        if raw_tracks.len() > usize::from(self.max_album_tracks) {
            return Err(PlaybackError::Protocol(format!(
                "Bandcamp album exceeded the {}-track limit",
                self.max_album_tracks
            )));
        }
        let possibly_truncated = source.kind() == BandcampMediaKind::Album
            && raw_tracks.len() == usize::from(self.max_album_tracks);
        let tracks = raw_tracks
            .into_iter()
            .map(|raw| normalize_resolved_track(raw, source, root_is_bandcamp))
            .collect::<PlaybackResult<Vec<_>>>()?;

        Ok(BandcampResolution {
            source: source.clone(),
            purpose,
            format,
            tracks,
            possibly_truncated,
        })
    }
}

#[derive(Debug, Deserialize)]
struct RawResolvedMedia {
    #[serde(default)]
    entries: Vec<RawResolvedMedia>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    http_headers: BTreeMap<String, String>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    webpage_url: Option<String>,
    #[serde(default)]
    thumbnail: Option<String>,
    #[serde(default)]
    id: String,
    #[serde(default)]
    format_id: Option<String>,
    #[serde(default)]
    acodec: Option<String>,
    #[serde(default)]
    extractor: Option<String>,
    #[serde(default)]
    extractor_key: Option<String>,
}

impl RawResolvedMedia {
    fn extractor_name(&self) -> Option<&str> {
        self.extractor.as_deref().or(self.extractor_key.as_deref())
    }
}

fn normalize_resolved_track(
    raw: RawResolvedMedia,
    source: &BandcampMediaUrl,
    inherited_bandcamp_extractor: bool,
) -> PlaybackResult<ResolvedMedia> {
    if !raw
        .extractor_name()
        .map_or(inherited_bandcamp_extractor, is_bandcamp_extractor)
    {
        return Err(PlaybackError::Protocol(
            "album entry did not use a Bandcamp extractor".to_owned(),
        ));
    }
    let media_url = raw
        .url
        .as_deref()
        .ok_or_else(|| PlaybackError::Protocol("Bandcamp track has no media URL".to_owned()))
        .and_then(parse_public_bandcamp_asset)?;
    let title = bounded_metadata(&raw.title, MAX_TITLE_BYTES, "Bandcamp track title")?;
    let id = bounded_metadata(&raw.id, 128, "Bandcamp track ID")?;
    validate_headers(&raw.http_headers)?;
    let webpage_url = raw
        .webpage_url
        .as_deref()
        .map(BandcampMediaUrl::parse_str)
        .transpose()
        .map_err(|error| PlaybackError::Protocol(error.to_string()))?
        .map(BandcampMediaUrl::into_url)
        .or_else(|| Some(source.as_url().clone()));
    let thumbnail_url = raw
        .thumbnail
        .as_deref()
        .and_then(|value| Url::parse(value).ok())
        .filter(valid_bandcamp_artwork_url);

    Ok(ResolvedMedia {
        media_url,
        http_headers: raw.http_headers,
        title,
        duration_seconds: raw
            .duration
            .filter(|duration| duration.is_finite() && *duration >= 0.0),
        webpage_url,
        thumbnail_url,
        id,
        format_id: raw
            .format_id
            .map(|value| truncate_utf8(&value, 128))
            .filter(|value| !value.is_empty()),
        audio_codec: raw
            .acodec
            .map(|value| truncate_utf8(&value, 64))
            .filter(|value| !value.is_empty()),
        extractor: raw.extractor.or(raw.extractor_key),
    })
}

fn validate_resolver_limits(
    executable: &std::path::Path,
    timeout: Duration,
    max_json_bytes: usize,
    max_album_tracks: u16,
) -> PlaybackResult<()> {
    if executable.as_os_str().is_empty() {
        return Err(PlaybackError::InvalidValue(
            "Bandcamp yt-dlp executable cannot be empty".to_owned(),
        ));
    }
    if timeout.is_zero() || timeout > MAX_RESOLVE_TIMEOUT {
        return Err(PlaybackError::InvalidValue(format!(
            "Bandcamp resolve timeout must be between 1 ns and {} seconds",
            MAX_RESOLVE_TIMEOUT.as_secs()
        )));
    }
    if !(1..=MAX_RESOLVE_JSON_BYTES).contains(&max_json_bytes) {
        return Err(PlaybackError::InvalidValue(format!(
            "Bandcamp resolve JSON limit must be between 1 and {MAX_RESOLVE_JSON_BYTES} bytes"
        )));
    }
    if !(1..=MAX_ALBUM_TRACKS).contains(&max_album_tracks) {
        return Err(PlaybackError::InvalidValue(format!(
            "Bandcamp album track limit must be between 1 and {MAX_ALBUM_TRACKS}"
        )));
    }
    Ok(())
}

fn run_supervised(invocation: &YtDlpInvocation) -> PlaybackResult<SupervisedOutput> {
    let mut command = Command::new(&invocation.executable);
    crate::child_process::supervised(&mut command);
    command
        .args(&invocation.arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            PlaybackError::ExecutableUnavailable(invocation.executable.display().to_string())
        } else {
            PlaybackError::Io(error)
        }
    })?;
    let Some(stdout) = child.stdout.take() else {
        terminate_child(&mut child);
        return Err(PlaybackError::Protocol(
            "yt-dlp stdout was not captured".to_owned(),
        ));
    };
    let Some(stderr) = child.stderr.take() else {
        terminate_child(&mut child);
        return Err(PlaybackError::Protocol(
            "yt-dlp stderr was not captured".to_owned(),
        ));
    };
    let output_exceeded = Arc::new(AtomicBool::new(false));
    let stdout_reader = spawn_bounded_reader(
        stdout,
        invocation.stdout_limit,
        Arc::clone(&output_exceeded),
    );
    let stderr_reader = spawn_bounded_reader(
        stderr,
        invocation.stderr_limit,
        Arc::clone(&output_exceeded),
    );
    let deadline = Instant::now() + invocation.timeout;
    let mut exit_status = None;
    let status = loop {
        if output_exceeded.load(Ordering::Acquire) {
            terminate_child(&mut child);
            return Err(PlaybackError::Protocol(format!(
                "Bandcamp yt-dlp output exceeded stdout/stderr limits of {}/{} bytes",
                invocation.stdout_limit, invocation.stderr_limit
            )));
        }
        if exit_status.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => exit_status = Some(status),
                Ok(None) => {}
                Err(error) => {
                    terminate_child(&mut child);
                    return Err(PlaybackError::Io(error));
                }
            }
        }
        if stdout_reader.is_finished()
            && stderr_reader.is_finished()
            && let Some(status) = exit_status
        {
            break status;
        }
        if Instant::now() >= deadline {
            terminate_child(&mut child);
            return Err(PlaybackError::Protocol(format!(
                "Bandcamp yt-dlp timed out after {} ms",
                invocation.timeout.as_millis()
            )));
        }
        thread::sleep(
            PROCESS_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
        );
    };
    let stdout = join_bounded_reader(stdout_reader)?;
    let stderr = join_bounded_reader(stderr_reader)?;
    if output_exceeded.load(Ordering::Acquire) {
        return Err(PlaybackError::Protocol(format!(
            "Bandcamp yt-dlp output exceeded stdout/stderr limits of {}/{} bytes",
            invocation.stdout_limit, invocation.stderr_limit
        )));
    }
    Ok(SupervisedOutput {
        success: status.success(),
        status: status.to_string(),
        stdout,
        stderr,
    })
}

fn spawn_bounded_reader<R: Read + Send + 'static>(
    mut reader: R,
    limit: usize,
    exceeded: Arc<AtomicBool>,
) -> thread::JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut output = Vec::new();
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                return Ok(output);
            }
            if output.len().saturating_add(read) > limit {
                exceeded.store(true, Ordering::Release);
                return Ok(output);
            }
            output.extend_from_slice(&buffer[..read]);
        }
    })
}

fn join_bounded_reader(handle: thread::JoinHandle<io::Result<Vec<u8>>>) -> PlaybackResult<Vec<u8>> {
    handle
        .join()
        .map_err(|_| PlaybackError::Protocol("yt-dlp output reader panicked".to_owned()))?
        .map_err(PlaybackError::Io)
}

use crate::child_process::terminate_tree as terminate_child;

fn parse_search_html(html: &str, max_results: usize) -> (Vec<BandcampSearchResult>, bool) {
    let mut results = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = 0;
    let lower = html.to_ascii_lowercase();
    while results.len() < max_results {
        let Some((block, next_cursor)) = next_search_result_block(html, &lower, cursor) else {
            break;
        };
        cursor = next_cursor;
        let Some(media) = attribute_values(block, "href")
            .into_iter()
            .find_map(|raw| canonicalize_search_result_url(&raw))
        else {
            continue;
        };
        if !seen.insert(media.as_url().as_str().to_owned()) {
            continue;
        }
        let title = class_text(block, "heading", MAX_TITLE_BYTES)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| media.release_slug().replace('-', " "));
        let artist = class_text(block, "subhead", MAX_ARTIST_BYTES)
            .map(|value| {
                value
                    .strip_prefix("by ")
                    .unwrap_or(&value)
                    .trim()
                    .to_owned()
            })
            .filter(|value| !value.is_empty());
        let artwork_url = attribute_values(block, "src")
            .into_iter()
            .filter_map(|raw| Url::parse(&decode_html_entities(&raw)).ok())
            .find(valid_bandcamp_artwork_url);
        results.push(BandcampSearchResult {
            media,
            title,
            artist,
            artwork_url,
        });
    }
    let has_next = lower.contains("class=\"next\"")
        || lower.contains("class='next'")
        || lower.contains(">next</a>");
    (results, has_next)
}

fn next_search_result_block<'a>(
    html: &'a str,
    lower: &str,
    mut cursor: usize,
) -> Option<(&'a str, usize)> {
    while let Some(relative_start) = lower[cursor..].find("<li") {
        let start = cursor.saturating_add(relative_start);
        let tag_end = start
            .saturating_add(lower[start..].find('>')?)
            .saturating_add(1);
        let opening_tag = &html[start..tag_end];
        if attribute_values(opening_tag, "class")
            .iter()
            .any(|classes| {
                classes
                    .split_ascii_whitespace()
                    .any(|class| class == "searchresult")
            })
        {
            let relative_end = lower[tag_end..].find("</li>")?;
            let end = tag_end
                .saturating_add(relative_end)
                .saturating_add("</li>".len());
            return Some((&html[start..end], end));
        }
        cursor = tag_end;
    }
    None
}

fn class_text(block: &str, class_name: &str, max_bytes: usize) -> Option<String> {
    let lower = block.to_ascii_lowercase();
    let mut cursor = 0;
    while let Some(relative) = lower[cursor..].find("class=") {
        let class_at = cursor.saturating_add(relative);
        let tag_start = lower[..class_at].rfind('<')?;
        let tag_end = class_at.saturating_add(lower[class_at..].find('>')?);
        let opening_tag = &block[tag_start..=tag_end];
        if attribute_values(opening_tag, "class")
            .iter()
            .any(|classes| {
                classes
                    .split_ascii_whitespace()
                    .any(|class| class == class_name)
            })
        {
            let tag_name = lower[tag_start.saturating_add(1)..]
                .split(|character: char| character.is_ascii_whitespace() || character == '>')
                .next()?;
            let close = format!("</{tag_name}>");
            let content_start = tag_end.saturating_add(1);
            let content_end = lower[content_start..]
                .find(&close)
                .map_or(block.len(), |relative| {
                    content_start.saturating_add(relative)
                });
            return Some(normalize_html_text(
                &block[content_start..content_end],
                max_bytes,
            ));
        }
        cursor = tag_end.saturating_add(1);
    }
    None
}

fn attribute_values(html: &str, attribute: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let needle = format!("{attribute}=");
    let mut values = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = lower[cursor..].find(&needle) {
        let value_at = cursor.saturating_add(relative).saturating_add(needle.len());
        let Some(quote) = html[value_at..].chars().next() else {
            break;
        };
        if !matches!(quote, '"' | '\'') {
            cursor = value_at.saturating_add(quote.len_utf8());
            continue;
        }
        let start = value_at.saturating_add(quote.len_utf8());
        let Some(relative_end) = html[start..].find(quote) else {
            break;
        };
        let end = start.saturating_add(relative_end);
        values.push(html[start..end].to_owned());
        cursor = end.saturating_add(quote.len_utf8());
    }
    values
}

fn canonicalize_search_result_url(raw: &str) -> Option<BandcampMediaUrl> {
    let decoded = decode_html_entities(raw);
    let mut url = Url::parse(&decoded).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    BandcampMediaUrl::parse(url).ok()
}

fn normalize_html_text(html: &str, max_bytes: usize) -> String {
    let mut without_tags = String::new();
    let mut inside_tag = false;
    for character in html.chars() {
        match character {
            '<' => inside_tag = true,
            '>' => inside_tag = false,
            _ if !inside_tag => without_tags.push(character),
            _ => {}
        }
    }
    let decoded = decode_html_entities(&without_tags);
    let normalized = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_utf8(&normalized, max_bytes)
}

fn decode_html_entities(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut remaining = value;
    while let Some(ampersand) = remaining.find('&') {
        output.push_str(&remaining[..ampersand]);
        remaining = &remaining[ampersand..];
        let Some(semicolon) = remaining.find(';').filter(|index| *index <= 12) else {
            output.push('&');
            remaining = &remaining[1..];
            continue;
        };
        let entity = &remaining[1..semicolon];
        let decoded = match entity {
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "lt" => Some('<'),
            "gt" => Some('>'),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| {
                    entity
                        .strip_prefix('#')
                        .and_then(|decimal| decimal.parse().ok())
                })
                .and_then(char::from_u32),
        };
        if let Some(character) = decoded {
            output.push(character);
        } else {
            output.push_str(&remaining[..=semicolon]);
        }
        remaining = &remaining[semicolon.saturating_add(1)..];
    }
    output.push_str(remaining);
    output
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value[..boundary].to_owned()
}

fn validate_search_input(query: &str, page: u16) -> Result<(), ProviderError> {
    if query.is_empty()
        || query.len() > MAX_SEARCH_QUERY_BYTES
        || query.chars().any(char::is_control)
    {
        return Err(ProviderError::InvalidRequest(format!(
            "Bandcamp query must contain 1 to {MAX_SEARCH_QUERY_BYTES} non-control UTF-8 bytes"
        )));
    }
    if !(1..=MAX_SEARCH_PAGE).contains(&page) {
        return Err(ProviderError::InvalidRequest(format!(
            "Bandcamp search page must be between 1 and {MAX_SEARCH_PAGE}"
        )));
    }
    Ok(())
}

fn validate_search_endpoint(url: &Url) -> Result<(), ProviderError> {
    if url.scheme() != "https"
        || url.host_str() != Some("bandcamp.com")
        || url.path() != "/search"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
    {
        return Err(ProviderError::InvalidRequest(
            "Bandcamp search endpoint is outside the HTTPS allowlist".to_owned(),
        ));
    }
    let pairs = url.query_pairs().collect::<Vec<_>>();
    let query = pairs
        .iter()
        .filter(|(name, _)| name == "q")
        .map(|(_, value)| value.as_ref())
        .collect::<Vec<_>>();
    let pages = pairs
        .iter()
        .filter(|(name, _)| name == "page")
        .map(|(_, value)| value.as_ref())
        .collect::<Vec<_>>();
    if pairs.len() != 2 || query.len() != 1 || pages.len() != 1 {
        return Err(ProviderError::InvalidRequest(
            "Bandcamp search endpoint contains unexpected parameters".to_owned(),
        ));
    }
    let page = pages[0].parse::<u16>().map_err(|_| {
        ProviderError::InvalidRequest("Bandcamp search page is not numeric".to_owned())
    })?;
    validate_search_input(query[0], page)
}

fn valid_dns_label(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn valid_release_slug(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=200).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn invalid_bandcamp_url() -> ProviderError {
    ProviderError::InvalidRequest(
        "expected canonical https://artist.bandcamp.com/track/slug or /album/slug URL".to_owned(),
    )
}

fn parse_public_bandcamp_asset(value: &str) -> PlaybackResult<Url> {
    let url = Url::parse(value)
        .map_err(|error| PlaybackError::Protocol(format!("invalid Bandcamp media URL: {error}")))?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || !url.host_str().is_some_and(valid_bandcamp_asset_host)
    {
        return Err(PlaybackError::Protocol(
            "yt-dlp returned a media URL outside Bandcamp's HTTPS CDN allowlist".to_owned(),
        ));
    }
    Ok(url)
}

fn valid_bandcamp_asset_host(host: &str) -> bool {
    host == "bandcamp.com"
        || host
            .strip_suffix(".bandcamp.com")
            .is_some_and(valid_host_prefix)
        || host == "bcbits.com"
        || host
            .strip_suffix(".bcbits.com")
            .is_some_and(valid_host_prefix)
}

fn valid_host_prefix(prefix: &str) -> bool {
    !prefix.is_empty()
        && prefix
            .split('.')
            .all(|label| valid_dns_label(&label.to_ascii_lowercase()))
}

fn valid_bandcamp_artwork_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url.host_str().is_some_and(|host| {
            host == "bcbits.com"
                || host
                    .strip_suffix(".bcbits.com")
                    .is_some_and(valid_host_prefix)
        })
        && url.path().starts_with("/img/")
}

fn is_bandcamp_extractor(value: &str) -> bool {
    value
        .split(':')
        .next()
        .is_some_and(|name| name.eq_ignore_ascii_case("bandcamp"))
}

fn bounded_metadata(value: &str, max_bytes: usize, field: &str) -> PlaybackResult<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(PlaybackError::Protocol(format!(
            "{field} must contain 1 to {max_bytes} non-control UTF-8 bytes"
        )));
    }
    Ok(value.to_owned())
}

fn validate_headers(headers: &BTreeMap<String, String>) -> PlaybackResult<()> {
    if headers.len() > 32 {
        return Err(PlaybackError::Protocol(
            "Bandcamp media returned too many HTTP headers".to_owned(),
        ));
    }
    let mut total = 0_usize;
    for (name, value) in headers {
        total = total.saturating_add(name.len()).saturating_add(value.len());
        if name.is_empty()
            || name.len() > 64
            || !name.bytes().all(is_http_token_byte)
            || value.len() > 4096
            || value.contains(['\r', '\n'])
        {
            return Err(PlaybackError::Protocol(
                "Bandcamp media returned an invalid HTTP header".to_owned(),
            ));
        }
    }
    if total > 16 * 1024 {
        return Err(PlaybackError::Protocol(
            "Bandcamp media HTTP headers exceeded 16 KiB".to_owned(),
        ));
    }
    Ok(())
}

fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
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
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const SEARCH_FIXTURE: &str = r#"
<ul>
  <li class="searchresult data-search">
    <div class="art"><img src="https://f4.bcbits.com/img/a123_16.jpg"></div>
    <div class="heading"><a href="https://artist-one.bandcamp.com/track/first-song?from=search&amp;search_item_id=1">First &amp; Song</a></div>
    <div class="subhead">by Artist One</div>
  </li>
  <li class="searchresult">
    <div class="heading"><a href="https://label-two.bandcamp.com/album/second-release?from=search">Second Release</a></div>
    <div class="subhead">by Label Two</div>
  </li>
  <li class="searchresult">
    <div class="heading"><a href="https://attacker.example/track/nope">Nope</a></div>
  </li>
</ul>
<a class="next" href="/search?q=fixture&amp;page=2">next</a>
"#;

    #[derive(Debug)]
    struct MockSearchTransport {
        body: Vec<u8>,
        calls: Mutex<Vec<Url>>,
    }

    impl MockSearchTransport {
        fn new(body: impl Into<Vec<u8>>) -> Self {
            Self {
                body: body.into(),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl BandcampSearchTransport for MockSearchTransport {
        fn fetch(&self, url: &Url, _max_bytes: usize) -> Result<Vec<u8>, ProviderError> {
            self.calls.lock().expect("mock calls").push(url.clone());
            Ok(self.body.clone())
        }
    }

    #[derive(Debug)]
    struct MockCommandRunner {
        output: SupervisedOutput,
        calls: AtomicUsize,
        invocations: Mutex<Vec<YtDlpInvocation>>,
    }

    impl MockCommandRunner {
        fn new(stdout: impl Into<Vec<u8>>) -> Self {
            Self {
                output: SupervisedOutput {
                    success: true,
                    status: "exit status: 0".to_owned(),
                    stdout: stdout.into(),
                    stderr: Vec::new(),
                },
                calls: AtomicUsize::new(0),
                invocations: Mutex::new(Vec::new()),
            }
        }
    }

    impl BandcampCommandRunner for MockCommandRunner {
        fn run(&self, invocation: &YtDlpInvocation) -> PlaybackResult<SupervisedOutput> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.invocations
                .lock()
                .expect("mock invocations")
                .push(invocation.clone());
            Ok(self.output.clone())
        }
    }

    #[test]
    fn canonical_urls_accept_only_strict_track_and_album_pages() {
        let track = BandcampMediaUrl::parse_str("https://artist-one.bandcamp.com/track/first-song")
            .expect("canonical track");
        assert_eq!(track.kind(), BandcampMediaKind::Track);
        assert_eq!(track.stable_id(), "artist-one/track/first-song");

        let album = BandcampMediaUrl::parse_str("https://label2.bandcamp.com/album/release-2026")
            .expect("canonical album");
        assert_eq!(album.kind(), BandcampMediaKind::Album);
        let serialized = serde_json::to_string(&album).expect("serialize canonical URL");
        assert_eq!(
            serde_json::from_str::<BandcampMediaUrl>(&serialized)
                .expect("deserialize canonical URL"),
            album
        );
        assert!(
            serde_json::from_str::<BandcampMediaUrl>(
                "\"https://artist.bandcamp.com/track/song?unsafe=true\""
            )
            .is_err()
        );

        for invalid in [
            "http://artist.bandcamp.com/track/song",
            "https://bandcamp.com/track/song",
            "https://www.bandcamp.com/track/song",
            "https://nested.artist.bandcamp.com/track/song",
            "https://artist.bandcamp.com/music/song",
            "https://artist.bandcamp.com/track/song/",
            "https://artist.bandcamp.com/track/Song",
            "https://artist.bandcamp.com/track/song?from=elsewhere",
            "https://user:secret@artist.bandcamp.com/track/song",
            "https://artist.bandcamp.com.evil.example/track/song",
        ] {
            assert!(
                BandcampMediaUrl::parse_str(invalid).is_err(),
                "{invalid} must be rejected"
            );
        }
    }

    #[test]
    fn public_search_is_bounded_allowlisted_and_does_not_resolve_media() {
        let transport = Arc::new(MockSearchTransport::new(SEARCH_FIXTURE));
        let client = BandcampSearchClient::with_transport(transport.clone(), 16 * 1024, 10)
            .expect("search client");

        let page = client.search("fixture & ambient", 1).expect("search page");

        assert_eq!(page.results.len(), 2);
        assert_eq!(page.results[0].title, "First & Song");
        assert_eq!(page.results[0].artist.as_deref(), Some("Artist One"));
        assert_eq!(
            page.results[0].media.as_url().as_str(),
            "https://artist-one.bandcamp.com/track/first-song"
        );
        assert_eq!(page.results[1].media.kind(), BandcampMediaKind::Album);
        assert_eq!(page.next_page, Some(2));
        let calls = transport.calls.lock().expect("search calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].scheme(), "https");
        assert_eq!(calls[0].host_str(), Some("bandcamp.com"));
        assert_eq!(calls[0].path(), "/search");
        assert_eq!(
            calls[0]
                .query_pairs()
                .find(|(name, _)| name == "q")
                .map(|(_, value)| value.into_owned())
                .as_deref(),
            Some("fixture & ambient")
        );
    }

    #[test]
    fn search_rechecks_transport_size_and_input_bounds() {
        let transport = Arc::new(MockSearchTransport::new(vec![b'x'; 17]));
        let client = BandcampSearchClient::with_transport(transport, 16, 2).expect("search client");
        assert!(matches!(
            client.search("fixture", 1),
            Err(ProviderError::ResponseTooLarge { limit: 16 })
        ));
        assert!(client.search("", 1).is_err());
        assert!(client.search("fixture", 0).is_err());
        assert!(
            client
                .search(&"q".repeat(MAX_SEARCH_QUERY_BYTES + 1), 1)
                .is_err()
        );
    }

    #[test]
    fn typed_formats_are_static_and_best_available_is_lossless_first() {
        assert_eq!(BandcampAudioFormat::ALL.len(), 10);
        assert_eq!(
            BandcampAudioFormat::BestAvailable.yt_dlp_selector(),
            "flac/wav/aiff-lossless/falac/[acodec^=alac]/mp3-320/mp3-v0/aac-hi/vorbis/mp3-128/bestaudio"
        );
        for format in BandcampAudioFormat::ALL {
            assert!(format.yt_dlp_selector().ends_with("mp3-128/bestaudio"));
            assert!(!format.label().is_empty());
        }
    }

    #[test]
    fn resolver_invokes_mock_only_for_explicit_action_with_closed_selector() {
        let output = br#"{
            "url":"https://t4.bcbits.com/stream/fixture/mp3-128",
            "http_headers":{"Referer":"https://artist.bandcamp.com/"},
            "title":"Artist - Track",
            "duration":123.5,
            "webpage_url":"https://artist.bandcamp.com/track/track",
            "thumbnail":"https://f4.bcbits.com/img/a123_16.jpg",
            "id":"123",
            "format_id":"mp3-128",
            "acodec":"mp3",
            "extractor":"Bandcamp"
        }"#;
        let runner = Arc::new(MockCommandRunner::new(output.as_slice()));
        let resolver = BandcampResolver::with_runner(
            "mock-yt-dlp",
            Duration::from_secs(1),
            16 * 1024,
            20,
            runner.clone(),
        )
        .expect("resolver");
        let source =
            BandcampMediaUrl::parse_str("https://artist.bandcamp.com/track/track").expect("source");
        assert_eq!(runner.calls.load(Ordering::SeqCst), 0);

        let resolved = resolver
            .resolve(
                &source,
                BandcampAudioFormat::Flac,
                BandcampResolvePurpose::Playback,
            )
            .expect("resolve");

        assert_eq!(runner.calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolved.tracks.len(), 1);
        assert_eq!(resolved.tracks[0].format_id.as_deref(), Some("mp3-128"));
        let invocations = runner.invocations.lock().expect("invocations");
        let arguments = &invocations[0].arguments;
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--format", "flac/mp3-128/bestaudio"])
        );
        assert_eq!(
            arguments.last().map(String::as_str),
            Some("https://artist.bandcamp.com/track/track")
        );
        assert!(arguments.contains(&"--ignore-config".to_owned()));
        assert!(arguments.contains(&"--no-plugin-dirs".to_owned()));
    }

    #[test]
    fn resolver_normalizes_bounded_album_entries_from_mock_json() {
        let output = br#"{
            "id":"album",
            "title":"Album",
            "extractor_key":"Bandcamp:album",
            "entries":[{
                "url":"https://t4.bcbits.com/stream/one/mp3-128",
                "title":"One",
                "webpage_url":"https://artist.bandcamp.com/track/one",
                "id":"1",
                "format_id":"mp3-128",
                "acodec":"mp3"
            },{
                "url":"https://t4.bcbits.com/stream/two/mp3-128",
                "title":"Two",
                "webpage_url":"https://artist.bandcamp.com/track/two",
                "id":"2",
                "format_id":"mp3-128",
                "acodec":"mp3"
            }]
        }"#;
        let runner = Arc::new(MockCommandRunner::new(output.as_slice()));
        let resolver = BandcampResolver::with_runner(
            "mock-yt-dlp",
            Duration::from_secs(1),
            16 * 1024,
            2,
            runner,
        )
        .expect("resolver");
        let source =
            BandcampMediaUrl::parse_str("https://artist.bandcamp.com/album/album").expect("album");

        let resolution = resolver
            .resolve(
                &source,
                BandcampAudioFormat::BestAvailable,
                BandcampResolvePurpose::Autoplay,
            )
            .expect("album resolution");

        assert_eq!(resolution.tracks.len(), 2);
        assert!(resolution.possibly_truncated);
        assert_eq!(resolution.tracks[1].title, "Two");
    }

    #[test]
    fn resolver_rejects_non_bandcamp_extractors_and_asset_hosts() {
        for output in [
            br#"{"url":"https://t4.bcbits.com/stream/a","title":"A","id":"1","extractor":"Generic"}"#
                .as_slice(),
            br#"{"url":"https://attacker.example/a","title":"A","id":"1","extractor":"Bandcamp"}"#
                .as_slice(),
        ] {
            let runner = Arc::new(MockCommandRunner::new(output));
            let resolver = BandcampResolver::with_runner(
                "mock-yt-dlp",
                Duration::from_secs(1),
                16 * 1024,
                2,
                runner,
            )
            .expect("resolver");
            let source =
                BandcampMediaUrl::parse_str("https://artist.bandcamp.com/track/a")
                    .expect("source");
            assert!(
                resolver
                    .resolve(
                        &source,
                        BandcampAudioFormat::PublicStreamMp3Kbps128,
                        BandcampResolvePurpose::Download,
                    )
                    .is_err()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn real_process_supervision_enforces_deadline_and_output_bound() {
        let timeout = YtDlpInvocation {
            executable: PathBuf::from("sh"),
            arguments: vec!["-c".to_owned(), "sleep 2".to_owned()],
            timeout: Duration::from_millis(30),
            stdout_limit: 1024,
            stderr_limit: 1024,
        };
        let started = Instant::now();
        let error = run_supervised(&timeout).expect_err("timeout");
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));

        let orphaned_pipe = YtDlpInvocation {
            executable: PathBuf::from("sh"),
            arguments: vec!["-c".to_owned(), "sleep 2 & exit 0".to_owned()],
            timeout: Duration::from_millis(30),
            stdout_limit: 1024,
            stderr_limit: 1024,
        };
        let started = Instant::now();
        let error = run_supervised(&orphaned_pipe).expect_err("orphaned output pipe");
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));

        let oversized = YtDlpInvocation {
            executable: PathBuf::from("sh"),
            arguments: vec!["-c".to_owned(), "head -c 4096 /dev/zero".to_owned()],
            timeout: Duration::from_secs(1),
            stdout_limit: 32,
            stderr_limit: 32,
        };
        let error = run_supervised(&oversized).expect_err("oversized output");
        assert!(error.to_string().contains("exceeded"));
    }
}
