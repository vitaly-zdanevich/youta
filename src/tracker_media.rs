//! Safe, bounded preparation of remote tracker-module payloads.
//!
//! Tracker archives often expose compressed downloads instead of media streams.
//! This module downloads one explicitly selected result, inspects its container,
//! and publishes playable module files inside a private persistent cache. It is
//! deliberately independent from the TUI and playback backend: callers perform
//! preparation on a background worker and pass the returned local paths to a
//! decoder that supports tracker modules.

use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use thiserror::Error;
use ureq::ResponseExt as _;
use url::Url;

const CACHE_DIRECTORY_NAME: &str = "tracker-media-cache";
const COMPLETE_MARKER: &str = ".complete";
const DOWNLOADED_PAYLOAD: &str = ".payload";
const PREFIX_BYTES: usize = 1_084;
const XPK_HEADER_BYTES: usize = 36;
const XPK_PREVIEW_START: usize = 16;
const XPK_PREVIEW_END: usize = 32;
const MAX_REDIRECTS: usize = 8;
const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
static NEXT_STAGING_ID: AtomicU64 = AtomicU64::new(0);

/// Tracker formats accepted from direct payloads and inspected archives.
///
/// This list follows libopenmpt's common formats. A decoder remains the final
/// authority: several legacy formats do not have a reliable short signature,
/// so archive names provide a bounded fallback hint.
pub const SUPPORTED_TRACKER_EXTENSIONS: &[&str] = &[
    "669", "ahx", "amf", "ams", "dbm", "digi", "dmf", "dsm", "far", "hvl", "imf", "it", "j2b",
    "med", "mdl", "mo3", "mod", "mptm", "mt2", "mtm", "okt", "psm", "ptm", "s3m", "stm", "ult",
    "umx", "wow", "xm",
];

/// Resource limits applied before a prepared cache entry becomes visible.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrackerMediaLimits {
    /// Maximum compressed or direct download size.
    pub max_download_bytes: u64,
    /// Maximum sum of declared or produced uncompressed bytes.
    pub max_uncompressed_bytes: u64,
    /// Maximum LHA or ZIP entries inspected in one selected archive.
    pub max_archive_entries: usize,
    /// Maximum playable modules published from one selected result.
    pub max_modules: usize,
}

impl Default for TrackerMediaLimits {
    fn default() -> Self {
        Self {
            max_download_bytes: 32 * 1024 * 1024,
            max_uncompressed_bytes: 64 * 1024 * 1024,
            max_archive_entries: 256,
            max_modules: 64,
        }
    }
}

impl TrackerMediaLimits {
    fn validate(self) -> Result<Self, TrackerPrepareError> {
        if self.max_download_bytes == 0
            || self.max_uncompressed_bytes == 0
            || self.max_archive_entries == 0
            || self.max_modules == 0
        {
            return Err(TrackerPrepareError::InvalidLimits);
        }
        Ok(self)
    }
}

/// One selected remote tracker result to prepare.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackerMediaRequest {
    /// Provider-controlled download URL.
    pub source_url: Url,
    /// Human-readable provider label used only in credential-free diagnostics.
    pub source_label: Option<String>,
    /// Provider format label used only after stronger byte/name detection.
    pub expected_format: Option<String>,
    /// Human-readable result name used to label a raw or gzip payload.
    pub display_name: Option<String>,
    /// Permit plaintext HTTP for a source explicitly configured that way.
    pub allow_insecure_http: bool,
}

impl TrackerMediaRequest {
    /// Creates a secure-by-default request for one HTTPS download.
    #[must_use]
    pub fn new(source_url: Url) -> Self {
        Self {
            source_url,
            source_label: None,
            expected_format: None,
            display_name: None,
            allow_insecure_http: false,
        }
    }
}

/// A playable module prepared below Youta's private cache root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedTrackerModule {
    /// Local cache path. This path remains stable across process restarts.
    pub path: PathBuf,
    /// Sanitized member or result name suitable for terminal presentation.
    pub display_name: String,
    /// Canonical lowercase tracker extension.
    pub format: String,
    /// Prepared file size on disk; decoder-supported wrappers may retain compression.
    pub size_bytes: u64,
}

/// A bounded response returned by an injected tracker transport.
///
/// The preparer validates every supplied redirect before reading the body.
/// Implementations must include redirects in request order and must not hide a
/// plaintext downgrade. A same-host redirect that spells an otherwise
/// credential-free target as plain HTTP is upgraded to HTTPS before it is
/// requested; this accommodates legacy archive links without sending bytes
/// over plaintext.
pub struct TrackerTransportResponse {
    final_url: Url,
    redirects: Vec<Url>,
    content_length: Option<u64>,
    body: Box<dyn Read + Send>,
}

impl TrackerTransportResponse {
    /// Constructs a response with no redirects or advertised length.
    #[must_use]
    pub fn new(final_url: Url, body: impl Read + Send + 'static) -> Self {
        Self {
            final_url,
            redirects: Vec::new(),
            content_length: None,
            body: Box::new(body),
        }
    }

    /// Records the response's declared body length.
    #[must_use]
    pub const fn with_content_length(mut self, content_length: u64) -> Self {
        self.content_length = Some(content_length);
        self
    }

    /// Records every redirect followed by the transport.
    #[must_use]
    pub fn with_redirects(mut self, redirects: Vec<Url>) -> Self {
        self.redirects = redirects;
        self
    }
}

/// Transport failure with a token-free diagnostic message.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("{message}")]
pub struct TrackerTransportError {
    message: String,
}

impl TrackerTransportError {
    /// Creates a transport error. Callers must not include URLs or credentials.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Injectable HTTP transport used by [`TrackerMediaPreparer`].
///
/// Implementations may use any bounded HTTP client. The preparer independently
/// enforces content length, streamed byte limits, redirect policy, and cache
/// confinement.
pub trait TrackerTransport {
    /// Fetches one provider-controlled media URL.
    ///
    /// # Errors
    ///
    /// Returns a credential-free transport diagnostic when the request fails.
    fn fetch(&mut self, url: &Url) -> Result<TrackerTransportResponse, TrackerTransportError>;
}

/// Bounded synchronous HTTP transport for remote tracker payloads.
///
/// Automatic redirects are disabled in `ureq`. Each redirect is instead
/// resolved against the preceding URL and validated before the next request:
/// credentials and non-HTTP(S) schemes are rejected, the original host cannot
/// change, and an HTTPS hop cannot downgrade to HTTP.
///
/// The owned response reader stops after `max_response_bytes + 1` bytes. The
/// extra byte lets [`TrackerMediaPreparer`] distinguish an exactly-full
/// response from an oversized response without allocating the entire payload.
#[derive(Clone)]
pub struct UreqTrackerTransport {
    agent: ureq::Agent,
    max_response_bytes: u64,
}

impl UreqTrackerTransport {
    /// Creates a transport with one global timeout and a streamed body limit.
    ///
    /// `max_response_bytes` should normally equal
    /// [`TrackerMediaLimits::max_download_bytes`].
    ///
    /// # Errors
    ///
    /// Returns an error when the timeout or response limit is zero.
    pub fn new(timeout: Duration, max_response_bytes: u64) -> Result<Self, TrackerTransportError> {
        if timeout.is_zero() {
            return Err(TrackerTransportError::new(
                "tracker HTTP timeout must be greater than zero",
            ));
        }
        if max_response_bytes == 0 {
            return Err(TrackerTransportError::new(
                "tracker HTTP response limit must be greater than zero",
            ));
        }
        Ok(Self {
            agent: tracker_http_agent(timeout),
            max_response_bytes,
        })
    }

    /// Creates a transport using the preparer's compressed download limit.
    ///
    /// # Errors
    ///
    /// Returns an error when the timeout or compressed download limit is zero.
    pub fn for_limits(
        timeout: Duration,
        limits: TrackerMediaLimits,
    ) -> Result<Self, TrackerTransportError> {
        Self::new(timeout, limits.max_download_bytes)
    }

    /// Returns the maximum number of response bytes accepted before the
    /// preparer reports an oversized download.
    #[must_use]
    pub const fn max_response_bytes(&self) -> u64 {
        self.max_response_bytes
    }
}

impl Default for UreqTrackerTransport {
    fn default() -> Self {
        Self {
            agent: tracker_http_agent(DEFAULT_HTTP_TIMEOUT),
            max_response_bytes: TrackerMediaLimits::default().max_download_bytes,
        }
    }
}

impl TrackerTransport for UreqTrackerTransport {
    fn fetch(&mut self, url: &Url) -> Result<TrackerTransportResponse, TrackerTransportError> {
        if !is_credential_free_http_url(url) {
            return Err(TrackerTransportError::new(
                "tracker HTTP request URL is not permitted",
            ));
        }

        let initial = url.clone();
        let mut current = initial.clone();
        let mut redirects = Vec::new();
        loop {
            let response = self
                .agent
                .get(current.as_str())
                .header("Accept", "application/octet-stream, */*;q=0.1")
                .call()
                .map_err(|error| sanitized_ureq_error(&error))?;
            let status = response.status().as_u16();

            if is_redirect_status(status) {
                if redirects.len() == MAX_REDIRECTS {
                    return Err(TrackerTransportError::new(format!(
                        "tracker HTTP response exceeded the {MAX_REDIRECTS}-redirect limit"
                    )));
                }
                let location = response
                    .headers()
                    .get("location")
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| {
                        TrackerTransportError::new(
                            "tracker HTTP redirect omitted a valid Location header",
                        )
                    })?;
                let target = validated_redirect_target(&initial, &current, location)?;
                redirects.push(target.clone());
                current = target;
                continue;
            }
            if (300..400).contains(&status) {
                return Err(TrackerTransportError::new(format!(
                    "tracker HTTP request returned unsupported redirect status {status}"
                )));
            }

            let final_url = Url::parse(&response.get_uri().to_string()).map_err(|_| {
                TrackerTransportError::new("tracker HTTP response returned an invalid final URL")
            })?;
            validate_redirect_hop(&initial, &current, &final_url)?;
            let content_length = response.body().content_length();
            let (_, body) = response.into_parts();
            let reader = body
                .into_with_config()
                .limit(self.max_response_bytes.saturating_add(1))
                .reader();
            let mut result =
                TrackerTransportResponse::new(final_url, reader).with_redirects(redirects);
            if let Some(content_length) = content_length {
                result = result.with_content_length(content_length);
            }
            return Ok(result);
        }
    }
}

/// Failure preparing a selected tracker result.
#[derive(Debug, Error)]
pub enum TrackerPrepareError {
    /// One or more configured resource limits are zero.
    #[error("tracker media limits must all be greater than zero")]
    InvalidLimits,
    /// A URL is not safe for an unauthenticated tracker download.
    #[error("tracker download URL is not a credential-free permitted HTTP(S) URL")]
    UnsafeUrl,
    /// A redirect changed host, downgraded HTTPS, or exceeded the redirect cap.
    #[error("tracker download followed an unsafe redirect")]
    UnsafeRedirect,
    /// The cache resolved outside the caller-supplied storage root.
    #[error("tracker cache resolved outside its supplied storage root")]
    CacheEscapedRoot,
    /// An existing cache entry is malformed or contains an unsafe file type.
    #[error("tracker cache entry is incomplete or invalid")]
    InvalidCacheEntry,
    /// The transport could not fetch the selected result.
    #[error("tracker download failed: {0}")]
    Transport(#[from] TrackerTransportError),
    /// The advertised or streamed response exceeded its compressed limit.
    #[error("tracker download exceeds the {limit}-byte compressed limit")]
    DownloadTooLarge {
        /// Configured compressed byte limit.
        limit: u64,
    },
    /// Archive output exceeded its declared or streamed expansion limit.
    #[error("tracker payload exceeds the {limit}-byte uncompressed limit")]
    UncompressedTooLarge {
        /// Configured uncompressed byte limit.
        limit: u64,
    },
    /// An LHA or ZIP archive contains too many entries.
    #[error("tracker archive exceeds the {limit}-entry inspection limit")]
    TooManyArchiveEntries {
        /// Configured entry limit.
        limit: usize,
    },
    /// One selected result expands to too many modules.
    #[error("tracker result exceeds the {limit}-module limit")]
    TooManyModules {
        /// Configured module limit.
        limit: usize,
    },
    /// The selected payload contains no recognizable supported module.
    #[error(
        "tracker payload from {provider} contains no supported module \
         (expected format: {expected_format}; detected: {detected})"
    )]
    NoSupportedModule {
        /// Sanitized provider label; never a request URL.
        provider: String,
        /// Sanitized provider format hint.
        expected_format: String,
        /// Byte-derived payload classification.
        detected: &'static str,
    },
    /// The payload is an archive type this build deliberately does not inspect.
    #[error(
        "tracker payload from {provider} is a {archive_type} archive, \
         which this build cannot inspect"
    )]
    UnsupportedArchive {
        /// Sanitized provider label; never a request URL.
        provider: String,
        /// Byte- or extension-derived archive type.
        archive_type: &'static str,
    },
    /// A gzip, LHA, or ZIP payload is malformed, truncated, or fails validation.
    #[error("tracker archive is invalid: {0}")]
    InvalidArchive(String),
    /// A filesystem operation failed inside the confined cache.
    #[error("tracker cache operation failed: {0}")]
    Io(#[from] io::Error),
}

/// Downloads and prepares tracker modules in a persistent private cache.
pub struct TrackerMediaPreparer<T> {
    cache_root: PathBuf,
    limits: TrackerMediaLimits,
    transport: T,
}

impl<T: TrackerTransport> TrackerMediaPreparer<T> {
    /// Creates a preparer below `storage_root/tracker-media-cache`.
    ///
    /// The child cache directory is created with owner-only permissions on
    /// Unix. Existing symlinks that resolve outside `storage_root` are rejected.
    ///
    /// # Errors
    ///
    /// Returns an error when the root/cache cannot be created, resolved, or
    /// confined, or when a resource limit is zero.
    pub fn new(
        storage_root: impl AsRef<Path>,
        transport: T,
        limits: TrackerMediaLimits,
    ) -> Result<Self, TrackerPrepareError> {
        let limits = limits.validate()?;
        fs::create_dir_all(storage_root.as_ref())?;
        let storage_root = crate::fs_path::canonicalize(storage_root.as_ref())?;
        let cache_candidate = storage_root.join(CACHE_DIRECTORY_NAME);
        fs::create_dir_all(&cache_candidate)?;
        let cache_root = crate::fs_path::canonicalize(&cache_candidate)?;
        if cache_root == storage_root || !cache_root.starts_with(&storage_root) {
            return Err(TrackerPrepareError::CacheEscapedRoot);
        }
        set_private_directory_permissions(&cache_root)?;
        Ok(Self {
            cache_root,
            limits,
            transport,
        })
    }

    /// Returns the confined persistent cache directory.
    #[must_use]
    pub fn cache_root(&self) -> &Path {
        &self.cache_root
    }

    /// Returns a shared reference to the injected transport.
    #[must_use]
    pub const fn transport(&self) -> &T {
        &self.transport
    }

    /// Prepares all playable modules contained by one selected result.
    ///
    /// A completed cache entry is reused without network access. New entries
    /// are assembled in a private staging directory and renamed only after all
    /// limits, archive checksums, and module names have been validated.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe URLs/redirects, resource-limit violations,
    /// invalid archives, unsupported payloads, transport failures, or cache I/O.
    pub fn prepare(
        &mut self,
        request: &TrackerMediaRequest,
    ) -> Result<Vec<PreparedTrackerModule>, TrackerPrepareError> {
        validate_initial_url(&request.source_url, request.allow_insecure_http)?;
        let cache_key = cache_key(&request.source_url);
        let entry_path = self.cache_root.join(&cache_key);
        if let Some(cached) = self.load_cached_entry(&entry_path)? {
            return Ok(cached);
        }

        let staging_path = self.create_staging_directory(&cache_key)?;
        let preparation = self.prepare_in_staging(request, &staging_path);
        let modules = match preparation {
            Ok(modules) => modules,
            Err(error) => {
                let _ = remove_owned_directory(&self.cache_root, &staging_path);
                return Err(error);
            }
        };

        create_private_file(&staging_path.join(COMPLETE_MARKER))?.sync_all()?;
        if entry_path.exists() {
            if let Some(cached) = self.load_cached_entry(&entry_path)? {
                remove_owned_directory(&self.cache_root, &staging_path)?;
                return Ok(cached);
            }
            remove_owned_directory(&self.cache_root, &entry_path)?;
        }
        fs::rename(&staging_path, &entry_path)?;
        let cached = self
            .load_cached_entry(&entry_path)?
            .ok_or(TrackerPrepareError::InvalidCacheEntry)?;
        debug_assert_eq!(cached.len(), modules.len());
        Ok(cached)
    }

    fn prepare_in_staging(
        &mut self,
        request: &TrackerMediaRequest,
        staging_path: &Path,
    ) -> Result<Vec<PreparedTrackerModule>, TrackerPrepareError> {
        let response = self.transport.fetch(&request.source_url)?;
        validate_response_urls(&request.source_url, &response, request.allow_insecure_http)?;
        if response
            .content_length
            .is_some_and(|length| length > self.limits.max_download_bytes)
        {
            return Err(TrackerPrepareError::DownloadTooLarge {
                limit: self.limits.max_download_bytes,
            });
        }

        let payload_path = staging_path.join(DOWNLOADED_PAYLOAD);
        let mut payload = create_private_file(&payload_path)?;
        let downloaded = copy_with_limit(
            response.body,
            &mut payload,
            self.limits.max_download_bytes,
            LimitKind::Download,
        )?;
        payload.flush()?;
        payload.sync_all()?;
        drop(payload);
        if downloaded == 0 {
            return Err(no_supported_module(request, "empty response"));
        }

        let prefix = read_prefix(&payload_path)?;
        let source_name = request
            .display_name
            .as_deref()
            .or_else(|| file_name_from_url(&response.final_url))
            .unwrap_or("tracker module");
        let source_container =
            container_extension_from_name(file_name_from_url(&response.final_url));
        let payload_kind = tracker_payload_kind(
            &prefix,
            source_container,
            request.expected_format.as_deref(),
        );
        let mut modules = match payload_kind {
            TrackerPayloadKind::Gzip => {
                self.extract_gzip(&payload_path, staging_path, source_name, request)?
            }
            TrackerPayloadKind::Lha => self.extract_lha(&payload_path, staging_path, request)?,
            TrackerPayloadKind::Zip => self.extract_zip(&payload_path, staging_path, request)?,
            TrackerPayloadKind::UnsupportedArchive(archive_type) => {
                return Err(unsupported_archive(request, archive_type));
            }
            TrackerPayloadKind::Raw => {
                self.prepare_raw(&payload_path, staging_path, source_name, request)?
            }
        };
        if payload_path.exists() {
            fs::remove_file(&payload_path)?;
        }
        if modules.is_empty() {
            return Err(no_supported_module(request, payload_kind.description()));
        }
        modules.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(modules)
    }

    fn prepare_raw(
        &self,
        payload_path: &Path,
        staging_path: &Path,
        source_name: &str,
        request: &TrackerMediaRequest,
    ) -> Result<Vec<PreparedTrackerModule>, TrackerPrepareError> {
        let prefix = read_prefix(payload_path)?;
        if looks_like_error_document(&prefix) {
            return Err(no_supported_module(request, payload_description(&prefix)));
        }
        let format = sniff_tracker_format(
            &prefix,
            Some(source_name),
            request.expected_format.as_deref(),
        )
        .ok_or_else(|| no_supported_module(request, payload_description(&prefix)))?;
        let output_path = module_output_path(staging_path, 0, source_name, format);
        fs::rename(payload_path, &output_path)?;
        set_private_file_permissions(&output_path)?;
        let size_bytes = fs::metadata(&output_path)?.len();
        if size_bytes > self.limits.max_uncompressed_bytes {
            return Err(TrackerPrepareError::UncompressedTooLarge {
                limit: self.limits.max_uncompressed_bytes,
            });
        }
        Ok(vec![PreparedTrackerModule {
            path: output_path,
            display_name: sanitized_display_name(source_name),
            format: format.to_owned(),
            size_bytes,
        }])
    }

    fn extract_gzip(
        &self,
        payload_path: &Path,
        staging_path: &Path,
        fallback_name: &str,
        request: &TrackerMediaRequest,
    ) -> Result<Vec<PreparedTrackerModule>, TrackerPrepareError> {
        let source = File::open(payload_path)?;
        let mut decoder = GzDecoder::new(BufReader::new(source));
        let member_name = decoder
            .header()
            .and_then(|header| header.filename())
            .map(String::from_utf8_lossy)
            .map(std::borrow::Cow::into_owned)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| fallback_name.to_owned());
        let expanded_path = staging_path.join(".expanded");
        let mut expanded = create_private_file(&expanded_path)?;
        let size_bytes = copy_with_limit(
            &mut decoder,
            &mut expanded,
            self.limits.max_uncompressed_bytes,
            LimitKind::Uncompressed,
        )
        .map_err(|error| match error {
            TrackerPrepareError::Io(source) => {
                TrackerPrepareError::InvalidArchive(source.to_string())
            }
            other => other,
        })?;
        expanded.flush()?;
        expanded.sync_all()?;
        drop(expanded);
        let prefix = read_prefix(&expanded_path)?;
        if let TrackerPayloadKind::UnsupportedArchive(archive_type) = tracker_payload_kind(
            &prefix,
            container_extension_from_name(Some(&member_name)),
            None,
        ) {
            return Err(unsupported_archive(request, archive_type));
        }
        if looks_like_error_document(&prefix) {
            return Err(no_supported_module(request, payload_description(&prefix)));
        }
        let format = sniff_tracker_format(
            &prefix,
            Some(&member_name),
            request.expected_format.as_deref(),
        )
        .ok_or_else(|| no_supported_module(request, payload_description(&prefix)))?;
        let output_path = module_output_path(staging_path, 0, &member_name, format);
        fs::rename(&expanded_path, &output_path)?;
        Ok(vec![PreparedTrackerModule {
            path: output_path,
            display_name: sanitized_display_name(&member_name),
            format: format.to_owned(),
            size_bytes,
        }])
    }

    fn extract_lha(
        &self,
        payload_path: &Path,
        staging_path: &Path,
        request: &TrackerMediaRequest,
    ) -> Result<Vec<PreparedTrackerModule>, TrackerPrepareError> {
        let mut archive = delharc::parse_file(payload_path)
            .map_err(|error| TrackerPrepareError::InvalidArchive(error.to_string()))?;
        let mut modules = Vec::new();
        let mut entries = 0usize;
        let mut declared_uncompressed = 0u64;
        loop {
            entries = entries.saturating_add(1);
            if entries > self.limits.max_archive_entries {
                return Err(TrackerPrepareError::TooManyArchiveEntries {
                    limit: self.limits.max_archive_entries,
                });
            }
            let header = archive.header();
            let entry_name = header.parse_pathname_to_str();
            let original_size = header.original_size;
            let directory = header.is_directory();
            declared_uncompressed = declared_uncompressed.checked_add(original_size).ok_or(
                TrackerPrepareError::UncompressedTooLarge {
                    limit: self.limits.max_uncompressed_bytes,
                },
            )?;
            if declared_uncompressed > self.limits.max_uncompressed_bytes {
                return Err(TrackerPrepareError::UncompressedTooLarge {
                    limit: self.limits.max_uncompressed_bytes,
                });
            }

            if !directory {
                let name_format = format_hint_from_name(Some(&entry_name));
                if !archive.is_decoder_supported() {
                    if name_format.is_none() {
                        if !archive.next_file().map_err(|error| {
                            TrackerPrepareError::InvalidArchive(error.to_string())
                        })? {
                            break;
                        }
                        continue;
                    }
                    return Err(TrackerPrepareError::InvalidArchive(
                        "a playable LHA member uses an unsupported compression method".to_owned(),
                    ));
                }
                let temporary_path = staging_path.join(format!(".lha-member-{entries:03}"));
                let mut output = create_private_file(&temporary_path)?;
                let size_bytes = copy_with_limit(
                    &mut archive,
                    &mut output,
                    self.limits.max_uncompressed_bytes,
                    LimitKind::Uncompressed,
                )?;
                output.flush()?;
                output.sync_all()?;
                drop(output);
                let prefix = read_prefix(&temporary_path)?;
                let xpk_tracker = sniff_xpk_sqsh_tracker(&prefix, size_bytes);
                let format = xpk_tracker.map_or_else(
                    || sniff_tracker_format(&prefix, Some(&entry_name), name_format),
                    |tracker| Some(tracker.format),
                );
                if let Some(format) = format {
                    if let Some(tracker) = xpk_tracker {
                        declared_uncompressed = account_xpk_sqsh_expansion(
                            declared_uncompressed,
                            tracker,
                            size_bytes,
                            self.limits.max_uncompressed_bytes,
                        )?;
                    }
                    if modules.len() >= self.limits.max_modules {
                        return Err(TrackerPrepareError::TooManyModules {
                            limit: self.limits.max_modules,
                        });
                    }
                    archive
                        .crc_check()
                        .map_err(|error| TrackerPrepareError::InvalidArchive(error.to_string()))?;
                    let output_path =
                        module_output_path(staging_path, modules.len(), &entry_name, format);
                    fs::rename(&temporary_path, &output_path)?;
                    modules.push(PreparedTrackerModule {
                        path: output_path,
                        display_name: sanitized_display_name(&entry_name),
                        format: format.to_owned(),
                        size_bytes,
                    });
                } else {
                    fs::remove_file(&temporary_path)?;
                }
            }

            if !archive
                .next_file()
                .map_err(|error| TrackerPrepareError::InvalidArchive(error.to_string()))?
            {
                break;
            }
        }
        if modules.is_empty() {
            return Err(no_supported_module(request, "LHA archive"));
        }
        Ok(modules)
    }

    fn extract_zip(
        &self,
        payload_path: &Path,
        staging_path: &Path,
        request: &TrackerMediaRequest,
    ) -> Result<Vec<PreparedTrackerModule>, TrackerPrepareError> {
        let source = File::open(payload_path)?;
        let mut archive = zip::ZipArchive::new(BufReader::new(source)).map_err(|_| {
            TrackerPrepareError::InvalidArchive(
                "ZIP structure or compression method is invalid".to_owned(),
            )
        })?;
        if archive.len() > self.limits.max_archive_entries {
            return Err(TrackerPrepareError::TooManyArchiveEntries {
                limit: self.limits.max_archive_entries,
            });
        }

        let mut modules = Vec::new();
        let mut declared_uncompressed = 0u64;
        let mut extracted_uncompressed = 0u64;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).map_err(|_| {
                TrackerPrepareError::InvalidArchive(
                    "ZIP structure or compression method is invalid".to_owned(),
                )
            })?;
            declared_uncompressed = declared_uncompressed.checked_add(entry.size()).ok_or(
                TrackerPrepareError::UncompressedTooLarge {
                    limit: self.limits.max_uncompressed_bytes,
                },
            )?;
            if declared_uncompressed > self.limits.max_uncompressed_bytes {
                return Err(TrackerPrepareError::UncompressedTooLarge {
                    limit: self.limits.max_uncompressed_bytes,
                });
            }
            if entry.is_dir() || entry.size() == 0 {
                continue;
            }

            let entry_name = entry.name().to_owned();
            let Some(name_format) = format_hint_from_name(Some(&entry_name)) else {
                continue;
            };
            if modules.len() >= self.limits.max_modules {
                return Err(TrackerPrepareError::TooManyModules {
                    limit: self.limits.max_modules,
                });
            }

            let temporary_path = staging_path.join(format!(".member-{:03}", modules.len()));
            let mut output = create_private_file(&temporary_path)?;
            let remaining = self
                .limits
                .max_uncompressed_bytes
                .saturating_sub(extracted_uncompressed);
            let size_bytes =
                copy_with_limit(&mut entry, &mut output, remaining, LimitKind::Uncompressed)
                    .map_err(|error| match error {
                        TrackerPrepareError::Io(_) => TrackerPrepareError::InvalidArchive(
                            "ZIP member could not be decompressed".to_owned(),
                        ),
                        other => other,
                    })?;
            extracted_uncompressed = extracted_uncompressed.checked_add(size_bytes).ok_or(
                TrackerPrepareError::UncompressedTooLarge {
                    limit: self.limits.max_uncompressed_bytes,
                },
            )?;
            if size_bytes != entry.size() {
                return Err(TrackerPrepareError::InvalidArchive(
                    "ZIP member size did not match its directory entry".to_owned(),
                ));
            }
            output.flush()?;
            output.sync_all()?;
            drop(output);

            let prefix = read_prefix(&temporary_path)?;
            if looks_like_error_document(&prefix) {
                return Err(no_supported_module(request, payload_description(&prefix)));
            }
            let format = sniff_tracker_format(&prefix, Some(&entry_name), Some(name_format))
                .ok_or_else(|| no_supported_module(request, payload_description(&prefix)))?;
            let output_path = module_output_path(staging_path, modules.len(), &entry_name, format);
            fs::rename(&temporary_path, &output_path)?;
            modules.push(PreparedTrackerModule {
                path: output_path,
                display_name: sanitized_display_name(&entry_name),
                format: format.to_owned(),
                size_bytes,
            });
        }

        if modules.is_empty() {
            return Err(no_supported_module(request, "ZIP archive"));
        }
        Ok(modules)
    }

    fn create_staging_directory(&self, cache_key: &str) -> Result<PathBuf, TrackerPrepareError> {
        for _ in 0..16 {
            let sequence = NEXT_STAGING_ID.fetch_add(1, Ordering::Relaxed);
            let name = format!(".{cache_key}-{}-{sequence}.tmp", std::process::id());
            let path = self.cache_root.join(name);
            match fs::create_dir(&path) {
                Ok(()) => {
                    set_private_directory_permissions(&path)?;
                    return Ok(path);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate tracker cache staging directory",
        )
        .into())
    }

    fn load_cached_entry(
        &self,
        entry_path: &Path,
    ) -> Result<Option<Vec<PreparedTrackerModule>>, TrackerPrepareError> {
        if !entry_path.exists() {
            return Ok(None);
        }
        ensure_owned_directory(&self.cache_root, entry_path)?;
        let marker = entry_path.join(COMPLETE_MARKER);
        if !is_regular_nonsymlink(&marker)? {
            return Ok(None);
        }
        let mut modules = Vec::new();
        for entry in fs::read_dir(entry_path)? {
            let entry = entry?;
            let path = entry.path();
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if !file_name.starts_with("module-") {
                continue;
            }
            if modules.len() >= self.limits.max_modules || !is_regular_nonsymlink(&path)? {
                return Err(TrackerPrepareError::InvalidCacheEntry);
            }
            let size_bytes = entry.metadata()?.len();
            if size_bytes > self.limits.max_uncompressed_bytes {
                return Err(TrackerPrepareError::InvalidCacheEntry);
            }
            let prefix = read_prefix(&path)?;
            let format = sniff_tracker_format(&prefix, Some(&file_name), None)
                .ok_or(TrackerPrepareError::InvalidCacheEntry)?;
            modules.push(PreparedTrackerModule {
                path,
                display_name: cached_display_name(&file_name, format),
                format: format.to_owned(),
                size_bytes,
            });
        }
        if modules.is_empty() {
            return Err(TrackerPrepareError::InvalidCacheEntry);
        }
        modules.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(Some(modules))
    }
}

/// Returns a canonical tracker extension from strong bytes or safe name hints.
///
/// Byte signatures take precedence over `name_hint` and `expected_format`.
/// The expected format is accepted only when it belongs to
/// [`SUPPORTED_TRACKER_EXTENSIONS`].
#[must_use]
pub fn sniff_tracker_format(
    prefix: &[u8],
    name_hint: Option<&str>,
    expected_format: Option<&str>,
) -> Option<&'static str> {
    sniff_tracker_signature(prefix, expected_format)
        .or_else(|| format_hint_from_name(name_hint))
        .or_else(|| normalized_supported_extension(expected_format))
}

fn sniff_tracker_signature(prefix: &[u8], expected_format: Option<&str>) -> Option<&'static str> {
    if prefix.starts_with(b"Extended Module: ") {
        return Some("xm");
    }
    if prefix.starts_with(b"IMPM") {
        return Some(
            if normalized_supported_extension(expected_format) == Some("mptm") {
                "mptm"
            } else {
                "it"
            },
        );
    }
    if prefix.get(44..48) == Some(b"SCRM") {
        return Some("s3m");
    }
    if prefix.get(1080..1084).is_some_and(is_mod_signature) {
        return Some("mod");
    }
    if prefix.starts_with(b"MTM") {
        return Some("mtm");
    }
    if prefix.get(20..28) == Some(b"!Scream!") {
        return Some("stm");
    }
    if prefix.starts_with(b"if") || prefix.starts_with(b"JN") {
        return Some("669");
    }
    if prefix.starts_with(b"OKTASONG") {
        return Some("okt");
    }
    if prefix.starts_with(b"MMD0")
        || prefix.starts_with(b"MMD1")
        || prefix.starts_with(b"MMD2")
        || prefix.starts_with(b"MMD3")
    {
        return Some("med");
    }
    if prefix.starts_with(b"DBM0") {
        return Some("dbm");
    }
    if prefix.starts_with(b"FAR\xfe") {
        return Some("far");
    }
    if prefix.starts_with(b"THX") {
        return Some("ahx");
    }
    if prefix.starts_with(b"HVL") {
        return Some("hvl");
    }
    if matches!(
        prefix.get(..4),
        Some([0xc1, 0x83, 0x2a, 0x9e] | [0x9e, 0x2a, 0x83, 0xc1])
    ) {
        return Some("umx");
    }
    None
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct XpkSqshTracker {
    format: &'static str,
    expanded_size: u64,
}

/// Recognizes the bounded XPK/SQSH header preview emitted for wrapped modules.
///
/// The source length must describe the complete member, and the embedded
/// preview must carry a supported tracker signature. The caller separately
/// applies the declared expanded size to its archive limit.
fn sniff_xpk_sqsh_tracker(prefix: &[u8], member_size: u64) -> Option<XpkSqshTracker> {
    if prefix.len() < XPK_HEADER_BYTES
        || prefix.get(..4) != Some(b"XPKF")
        || prefix.get(8..12) != Some(b"SQSH")
    {
        return None;
    }
    let source_length = u64::from(u32::from_be_bytes(prefix.get(4..8)?.try_into().ok()?));
    if source_length.checked_add(8)? != member_size {
        return None;
    }
    let expanded_size = u64::from(u32::from_be_bytes(prefix.get(12..16)?.try_into().ok()?));
    if expanded_size == 0 {
        return None;
    }
    let format = sniff_tracker_signature(prefix.get(XPK_PREVIEW_START..XPK_PREVIEW_END)?, None)?;
    Some(XpkSqshTracker {
        format,
        expanded_size,
    })
}

/// Replaces one XPK member's packed contribution with its declared expansion.
fn account_xpk_sqsh_expansion(
    declared_uncompressed: u64,
    tracker: XpkSqshTracker,
    member_size: u64,
    limit: u64,
) -> Result<u64, TrackerPrepareError> {
    let expansion = tracker.expanded_size.saturating_sub(member_size);
    let total = declared_uncompressed
        .checked_add(expansion)
        .ok_or(TrackerPrepareError::UncompressedTooLarge { limit })?;
    if total > limit {
        return Err(TrackerPrepareError::UncompressedTooLarge { limit });
    }
    Ok(total)
}

/// Returns whether a case-insensitive extension is supported for preparation.
#[must_use]
pub fn is_supported_tracker_extension(extension: &str) -> bool {
    normalized_supported_extension(Some(extension)).is_some()
}

fn validate_initial_url(url: &Url, allow_insecure_http: bool) -> Result<(), TrackerPrepareError> {
    if !is_credential_free_http_url(url) || (url.scheme() == "http" && !allow_insecure_http) {
        return Err(TrackerPrepareError::UnsafeUrl);
    }
    Ok(())
}

fn validate_response_urls(
    initial: &Url,
    response: &TrackerTransportResponse,
    allow_insecure_http: bool,
) -> Result<(), TrackerPrepareError> {
    if response.redirects.len() > MAX_REDIRECTS {
        return Err(TrackerPrepareError::UnsafeRedirect);
    }
    let mut previous = initial;
    for candidate in response
        .redirects
        .iter()
        .chain(std::iter::once(&response.final_url))
    {
        if !is_credential_free_http_url(candidate)
            || candidate.host_str() != initial.host_str()
            || (candidate.scheme() == "http"
                && (initial.scheme() == "https" || !allow_insecure_http))
            || (previous.scheme() == "https" && candidate.scheme() == "http")
        {
            return Err(TrackerPrepareError::UnsafeRedirect);
        }
        previous = candidate;
    }
    Ok(())
}

fn tracker_http_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .max_redirects(0)
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

fn is_redirect_status(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn validated_redirect_target(
    initial: &Url,
    current: &Url,
    location: &str,
) -> Result<Url, TrackerTransportError> {
    let mut target = current.join(location).map_err(|_| {
        TrackerTransportError::new("tracker HTTP redirect contained an invalid target")
    })?;
    if initial.scheme() == "https"
        && current.scheme() == "https"
        && target.scheme() == "http"
        && target.host_str() == initial.host_str()
        && target.port().is_none()
    {
        target.set_scheme("https").map_err(|()| {
            TrackerTransportError::new("tracker HTTP redirect could not be upgraded to HTTPS")
        })?;
    }
    validate_redirect_hop(initial, current, &target)?;
    Ok(target)
}

fn validate_redirect_hop(
    initial: &Url,
    current: &Url,
    target: &Url,
) -> Result<(), TrackerTransportError> {
    if !is_credential_free_http_url(target)
        || target.host_str() != initial.host_str()
        || (current.scheme() == "https" && target.scheme() == "http")
    {
        return Err(TrackerTransportError::new(
            "tracker HTTP redirect was rejected by the security policy",
        ));
    }
    Ok(())
}

fn sanitized_ureq_error(error: &ureq::Error) -> TrackerTransportError {
    let message = match error {
        ureq::Error::StatusCode(status) => {
            return TrackerTransportError::new(format!(
                "tracker HTTP request returned status {status}"
            ));
        }
        ureq::Error::Timeout(_) => "tracker HTTP request timed out",
        ureq::Error::HostNotFound => "tracker HTTP host was not found",
        ureq::Error::ConnectionFailed => "tracker HTTP connection failed",
        ureq::Error::Tls(_) => "tracker HTTPS connection failed",
        ureq::Error::BodyExceedsLimit(_) => "tracker HTTP response exceeded its byte limit",
        _ => "tracker HTTP request failed",
    };
    TrackerTransportError::new(message)
}

fn is_credential_free_http_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
}

fn cache_key(url: &Url) -> String {
    let digest = Sha256::digest(url.as_str().as_bytes());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn format_hint_from_name(name: Option<&str>) -> Option<&'static str> {
    let name = name?;
    let base_name = name.rsplit(['/', '\\']).next().unwrap_or(name).trim();
    let lower = base_name.to_ascii_lowercase();
    let suffix = lower.rsplit_once('.').map(|(_, extension)| extension);
    normalized_supported_extension(suffix).or_else(|| {
        lower
            .split_once('.')
            .and_then(|(prefix, _)| normalized_supported_extension(Some(prefix)))
    })
}

fn normalized_supported_extension(extension: Option<&str>) -> Option<&'static str> {
    let extension = extension?.trim().trim_start_matches('.');
    SUPPORTED_TRACKER_EXTENSIONS
        .iter()
        .copied()
        .find(|supported| extension.eq_ignore_ascii_case(supported))
}

fn is_mod_signature(signature: &[u8]) -> bool {
    matches!(
        signature,
        b"M.K."
            | b"M!K!"
            | b"M&K!"
            | b"N.T."
            | b"FLT4"
            | b"FLT8"
            | b"CD81"
            | b"OKTA"
            | b"16CN"
            | b"32CN"
    ) || (signature.len() == 4
        && signature[0].is_ascii_digit()
        && signature[1].is_ascii_digit()
        && matches!(&signature[2..], b"CH" | b"CN"))
        || (signature.len() == 4
            && signature[0].is_ascii_digit()
            && matches!(&signature[1..], b"CHN"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrackerPayloadKind {
    Gzip,
    Lha,
    Zip,
    UnsupportedArchive(&'static str),
    Raw,
}

impl TrackerPayloadKind {
    const fn description(self) -> &'static str {
        match self {
            Self::Gzip => "gzip archive",
            Self::Lha => "LHA archive",
            Self::Zip => "ZIP archive",
            Self::UnsupportedArchive(archive_type) => archive_type,
            Self::Raw => "unrecognized raw payload",
        }
    }
}

fn tracker_payload_kind(
    prefix: &[u8],
    container_extension: Option<&str>,
    expected_format: Option<&str>,
) -> TrackerPayloadKind {
    if prefix.starts_with(&[0x1f, 0x8b]) {
        return TrackerPayloadKind::Gzip;
    }
    if looks_like_lha(prefix) {
        return TrackerPayloadKind::Lha;
    }
    if looks_like_zip(prefix) {
        return TrackerPayloadKind::Zip;
    }
    if let Some(archive_type) = unsupported_archive_magic(prefix) {
        return TrackerPayloadKind::UnsupportedArchive(archive_type);
    }
    if sniff_tracker_format(prefix, None, None).is_some() || looks_like_error_document(prefix) {
        return TrackerPayloadKind::Raw;
    }

    container_extension
        .and_then(payload_kind_from_extension)
        .or_else(|| expected_format.and_then(payload_kind_from_extension))
        .unwrap_or(TrackerPayloadKind::Raw)
}

fn payload_kind_from_extension(extension: &str) -> Option<TrackerPayloadKind> {
    let extension = extension
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    match extension.as_str() {
        "gz" | "tgz" => Some(TrackerPayloadKind::Gzip),
        "lha" | "lzh" => Some(TrackerPayloadKind::Lha),
        "zip" | "zipx" => Some(TrackerPayloadKind::Zip),
        "7z" => Some(TrackerPayloadKind::UnsupportedArchive("7z")),
        "bz2" | "tbz" | "tbz2" => Some(TrackerPayloadKind::UnsupportedArchive("bzip2")),
        "lzx" => Some(TrackerPayloadKind::UnsupportedArchive("LZX")),
        "rar" => Some(TrackerPayloadKind::UnsupportedArchive("RAR")),
        "tar" => Some(TrackerPayloadKind::UnsupportedArchive("tar")),
        "xz" | "txz" => Some(TrackerPayloadKind::UnsupportedArchive("XZ")),
        _ => None,
    }
}

fn looks_like_lha(prefix: &[u8]) -> bool {
    prefix
        .get(2..7)
        .is_some_and(|method| method.len() == 5 && method[0] == b'-' && method[4] == b'-')
}

fn looks_like_zip(prefix: &[u8]) -> bool {
    matches!(
        prefix.get(..4),
        Some(b"PK\x03\x04" | b"PK\x05\x06" | b"PK\x07\x08")
    )
}

fn unsupported_archive_magic(prefix: &[u8]) -> Option<&'static str> {
    if prefix.starts_with(b"7z\xbc\xaf\x27\x1c") {
        Some("7z")
    } else if prefix.starts_with(b"BZh") {
        Some("bzip2")
    } else if prefix.starts_with(b"Rar!\x1a\x07") {
        Some("RAR")
    } else if prefix.starts_with(b"\xfd7zXZ\0") {
        Some("XZ")
    } else if prefix.starts_with(b"LZX") {
        Some("LZX")
    } else if prefix.get(257..262) == Some(b"ustar") {
        Some("tar")
    } else {
        None
    }
}

fn looks_like_error_document(prefix: &[u8]) -> bool {
    let trimmed = trim_document_prefix(prefix);
    let text_like =
        std::str::from_utf8(trimmed.get(..trimmed.len().min(512)).unwrap_or(trimmed)).is_ok();
    text_like
        && (starts_with_ascii_case_insensitive(trimmed, b"<!doctype html")
            || starts_with_ascii_case_insensitive(trimmed, b"<html")
            || trimmed.starts_with(b"<?xml")
            || matches!(trimmed.first(), Some(b'{' | b'[')))
}

fn trim_document_prefix(mut prefix: &[u8]) -> &[u8] {
    if let Some(without_bom) = prefix.strip_prefix(b"\xef\xbb\xbf") {
        prefix = without_bom;
    }
    while prefix.first().is_some_and(u8::is_ascii_whitespace) {
        prefix = &prefix[1..];
    }
    prefix
}

fn starts_with_ascii_case_insensitive(candidate: &[u8], expected: &[u8]) -> bool {
    candidate
        .get(..expected.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(expected))
}

fn payload_description(prefix: &[u8]) -> &'static str {
    let trimmed = trim_document_prefix(prefix);
    if trimmed.is_empty() {
        "empty response"
    } else if starts_with_ascii_case_insensitive(trimmed, b"<!doctype html")
        || starts_with_ascii_case_insensitive(trimmed, b"<html")
    {
        "HTML response"
    } else if trimmed.starts_with(b"<?xml") {
        "XML response"
    } else if std::str::from_utf8(trimmed.get(..trimmed.len().min(512)).unwrap_or(trimmed)).is_ok()
        && matches!(trimmed.first(), Some(b'{' | b'['))
    {
        "JSON response"
    } else if looks_like_zip(prefix) {
        "ZIP archive"
    } else if looks_like_lha(prefix) {
        "LHA archive"
    } else if prefix.starts_with(&[0x1f, 0x8b]) {
        "gzip archive"
    } else if let Some(archive_type) = unsupported_archive_magic(prefix) {
        archive_type
    } else if sniff_tracker_format(prefix, None, None).is_some() {
        "tracker module"
    } else if std::str::from_utf8(trimmed.get(..trimmed.len().min(512)).unwrap_or(trimmed)).is_ok()
    {
        "text response"
    } else {
        "unrecognized binary data"
    }
}

fn no_supported_module(
    request: &TrackerMediaRequest,
    detected: &'static str,
) -> TrackerPrepareError {
    TrackerPrepareError::NoSupportedModule {
        provider: safe_source_label(request.source_label.as_deref()),
        expected_format: safe_expected_format(request.expected_format.as_deref()),
        detected,
    }
}

fn unsupported_archive(
    request: &TrackerMediaRequest,
    archive_type: &'static str,
) -> TrackerPrepareError {
    TrackerPrepareError::UnsupportedArchive {
        provider: safe_source_label(request.source_label.as_deref()),
        archive_type,
    }
}

fn safe_source_label(label: Option<&str>) -> String {
    let Some(label) = label.map(str::trim).filter(|label| {
        !label.is_empty()
            && !label.contains("://")
            && !label.contains(['?', '&', '='])
            && !label.chars().any(char::is_control)
    }) else {
        return "unknown tracker source".to_owned();
    };
    let mut sanitized = String::new();
    for character in label.chars().take(80) {
        if character.is_alphanumeric()
            || character.is_whitespace()
            || matches!(character, '-' | '_' | '.' | '(' | ')')
        {
            sanitized.push(character);
        }
    }
    let sanitized = sanitized.trim();
    if sanitized.is_empty() {
        "unknown tracker source".to_owned()
    } else {
        sanitized.to_owned()
    }
}

fn safe_expected_format(format: Option<&str>) -> String {
    let Some(format) = format.map(str::trim).filter(|format| {
        !format.is_empty()
            && format.len() <= 32
            && format.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '+' | '-')
            })
    }) else {
        return "unspecified".to_owned();
    };
    format.to_owned()
}

fn file_name_from_url(url: &Url) -> Option<&str> {
    url.path_segments()?
        .next_back()
        .filter(|name| !name.is_empty())
}

fn container_extension_from_name(name: Option<&str>) -> Option<&str> {
    name?
        .rsplit(['/', '\\'])
        .next()
        .and_then(|name| name.rsplit_once('.').map(|(_, extension)| extension))
        .map(str::trim)
}

fn read_prefix(path: &Path) -> Result<Vec<u8>, io::Error> {
    let file = File::open(path)?;
    let mut prefix = Vec::with_capacity(PREFIX_BYTES);
    file.take(PREFIX_BYTES as u64).read_to_end(&mut prefix)?;
    Ok(prefix)
}

#[derive(Clone, Copy)]
enum LimitKind {
    Download,
    Uncompressed,
}

fn copy_with_limit(
    input: impl Read,
    output: &mut impl Write,
    limit: u64,
    kind: LimitKind,
) -> Result<u64, TrackerPrepareError> {
    let mut limited = input.take(limit.saturating_add(1));
    let copied = io::copy(&mut limited, output)?;
    if copied > limit {
        return Err(match kind {
            LimitKind::Download => TrackerPrepareError::DownloadTooLarge { limit },
            LimitKind::Uncompressed => TrackerPrepareError::UncompressedTooLarge { limit },
        });
    }
    Ok(copied)
}

fn module_output_path(directory: &Path, index: usize, source_name: &str, format: &str) -> PathBuf {
    let stem = sanitized_file_stem(source_name, format);
    directory.join(format!("module-{index:03}-{stem}.{format}"))
}

fn sanitized_file_stem(source_name: &str, format: &str) -> String {
    let base_name = source_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(source_name)
        .trim();
    let lower = base_name.to_ascii_lowercase();
    let without_format = lower
        .strip_prefix(&format!("{format}."))
        .or_else(|| lower.strip_suffix(&format!(".{format}")))
        .unwrap_or(&lower);
    let mut output = String::with_capacity(without_format.len().min(80));
    let mut previous_dash = false;
    for character in without_format.chars() {
        if output.chars().count() >= 80 {
            break;
        }
        if character.is_alphanumeric() {
            output.push(character);
            previous_dash = false;
        } else if !previous_dash && !output.is_empty() {
            output.push('-');
            previous_dash = true;
        }
    }
    while output.ends_with('-') {
        output.pop();
    }
    if output.is_empty() {
        "module".to_owned()
    } else {
        output
    }
}

fn sanitized_display_name(source_name: &str) -> String {
    let base_name = source_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(source_name)
        .trim();
    let mut output = String::new();
    for character in base_name
        .chars()
        .filter(|character| !character.is_control())
    {
        if output.chars().count() >= 200 {
            break;
        }
        output.push(character);
    }
    if output.is_empty() {
        "tracker module".to_owned()
    } else {
        output
    }
}

fn cached_display_name(file_name: &str, format: &str) -> String {
    file_name
        .strip_prefix("module-")
        .and_then(|name| name.split_once('-').map(|(_, title)| title))
        .and_then(|title| title.strip_suffix(&format!(".{format}")))
        .unwrap_or("tracker module")
        .replace('-', " ")
}

use crate::private_files::{set_private_directory_permissions, set_private_file_permissions};

fn create_private_file(path: &Path) -> Result<File, io::Error> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    crate::private_files::open_privately(&mut options).open(path)
}

fn is_regular_nonsymlink(path: &Path) -> Result<bool, io::Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_file() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn ensure_owned_directory(cache_root: &Path, path: &Path) -> Result<(), TrackerPrepareError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(TrackerPrepareError::InvalidCacheEntry);
    }
    let canonical = crate::fs_path::canonicalize(path)?;
    if canonical == cache_root || !canonical.starts_with(cache_root) {
        return Err(TrackerPrepareError::CacheEscapedRoot);
    }
    Ok(())
}

fn remove_owned_directory(cache_root: &Path, path: &Path) -> Result<(), TrackerPrepareError> {
    ensure_owned_directory(cache_root, path)?;
    fs::remove_dir_all(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{Cursor, Write};

    use delharc::crc::Crc16;
    use flate2::{Compression, GzBuilder};

    use super::*;

    #[derive(Default)]
    struct MockTransport {
        calls: usize,
        responses: VecDeque<Result<MockResponse, TrackerTransportError>>,
    }

    struct MockResponse {
        final_url: Url,
        redirects: Vec<Url>,
        advertised_length: Option<u64>,
        body: Vec<u8>,
    }

    impl MockTransport {
        fn once(url: &str, body: Vec<u8>) -> Self {
            Self {
                calls: 0,
                responses: VecDeque::from([Ok(MockResponse {
                    final_url: Url::parse(url).expect("mock final URL"),
                    redirects: Vec::new(),
                    advertised_length: None,
                    body,
                })]),
            }
        }
    }

    impl TrackerTransport for MockTransport {
        fn fetch(&mut self, _url: &Url) -> Result<TrackerTransportResponse, TrackerTransportError> {
            self.calls += 1;
            let response = self
                .responses
                .pop_front()
                .expect("mock response must be configured")?;
            let mut result =
                TrackerTransportResponse::new(response.final_url, Cursor::new(response.body))
                    .with_redirects(response.redirects);
            if let Some(length) = response.advertised_length {
                result = result.with_content_length(length);
            }
            Ok(result)
        }
    }

    fn s3m_fixture() -> Vec<u8> {
        let mut bytes = vec![0; 96];
        bytes[..15].copy_from_slice(b"Fixture module ");
        bytes[44..48].copy_from_slice(b"SCRM");
        bytes
    }

    fn xm_fixture() -> Vec<u8> {
        let mut bytes = b"Extended Module: ".to_vec();
        bytes.extend_from_slice(b"Fixture XM");
        bytes
    }

    fn xpk_sqsh_fixture(preview: &[u8], payload: &[u8], destination_length: u32) -> Vec<u8> {
        assert!(preview.len() <= 16);
        let packed_size = u16::try_from(payload.len()).expect("small XPK fixture");
        let source_length = u32::try_from(36usize + payload.len()).expect("small XPK fixture");
        let mut bytes = Vec::with_capacity(44 + payload.len());
        bytes.extend_from_slice(b"XPKF");
        bytes.extend_from_slice(&source_length.to_be_bytes());
        bytes.extend_from_slice(b"SQSH");
        bytes.extend_from_slice(&destination_length.to_be_bytes());
        bytes.extend_from_slice(preview);
        bytes.resize(32, 0);
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(&packed_size.to_be_bytes());
        bytes.extend_from_slice(&packed_size.to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    fn gzip_fixture(name: &str, bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzBuilder::new()
            .filename(name)
            .write(Vec::new(), Compression::default());
        encoder.write_all(bytes).expect("write gzip fixture");
        encoder.finish().expect("finish gzip fixture")
    }

    fn lha_fixture(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut archive = Vec::new();
        for (name, bytes) in entries {
            let name = name.as_bytes();
            let header_length = 22usize
                .checked_add(name.len())
                .expect("fixture header length");
            assert!(header_length <= usize::from(u8::MAX));
            let mut header = Vec::with_capacity(header_length);
            header.extend_from_slice(b"-lh0-");
            let size = u32::try_from(bytes.len()).expect("small fixture");
            header.extend_from_slice(&size.to_le_bytes());
            header.extend_from_slice(&size.to_le_bytes());
            header.extend_from_slice(&0u32.to_le_bytes());
            header.push(0x20);
            header.push(0);
            header.push(u8::try_from(name.len()).expect("short fixture name"));
            header.extend_from_slice(name);
            let mut crc = Crc16::default();
            crc.digest(bytes);
            header.extend_from_slice(&crc.sum16().to_le_bytes());
            let checksum = header.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte));
            archive.push(u8::try_from(header_length).expect("short fixture header"));
            archive.push(checksum);
            archive.extend_from_slice(&header);
            archive.extend_from_slice(bytes);
        }
        archive.push(0);
        archive
    }

    fn zip_fixture(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut archive = zip::ZipWriter::new(cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in entries {
            archive
                .start_file(*name, options)
                .expect("start ZIP member");
            archive.write_all(bytes).expect("write ZIP member");
        }
        archive.finish().expect("finish ZIP fixture").into_inner()
    }

    fn request(url: &str) -> TrackerMediaRequest {
        TrackerMediaRequest::new(Url::parse(url).expect("request URL"))
    }

    #[test]
    fn common_tracker_signatures_and_name_hints_are_detected() {
        assert_eq!(
            sniff_tracker_format(&xm_fixture(), Some("wrong.mod"), None),
            Some("xm")
        );
        assert_eq!(
            sniff_tracker_format(&s3m_fixture(), Some("wrong.xm"), None),
            Some("s3m")
        );
        assert_eq!(
            sniff_tracker_format(b"unmarked", Some("MOD.Clockwork Life"), None),
            Some("mod")
        );
        assert_eq!(
            sniff_tracker_format(b"unmarked", None, Some(".MPTM")),
            Some("mptm")
        );
        assert!(is_supported_tracker_extension("XM"));
        assert!(!is_supported_tracker_extension("mp3"));
        assert_eq!(
            safe_source_label(Some("https://archive.example/?token=secret")),
            "unknown tracker source"
        );
        assert_eq!(
            safe_expected_format(Some("s3m?token=secret")),
            "unspecified"
        );
    }

    #[test]
    fn raw_module_is_cached_privately_and_reused_without_transport() {
        // The cache root below is compared against this path by prefix, and the
        // preparer reports a canonical one, so the fixture has to be canonical
        // too — macOS resolves `/var` to `/private/var` and Windows answers
        // behind a `\\?\` prefix.
        let temporary = crate::test_support::canonical_tempdir("temporary tracker cache root");
        let url = "https://modules.example/music?id=42";
        let transport = MockTransport::once(url, s3m_fixture());
        let mut preparer =
            TrackerMediaPreparer::new(temporary.path(), transport, TrackerMediaLimits::default())
                .expect("preparer");
        let mut selected = request(url);
        selected.display_name = Some("S3M. Clockwork Life".to_owned());

        let first = preparer.prepare(&selected).expect("first preparation");
        let second = preparer.prepare(&selected).expect("cached preparation");

        assert_eq!(preparer.transport().calls, 1);
        assert_eq!(first, second);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].format, "s3m");
        assert!(first[0].path.starts_with(preparer.cache_root()));
        assert!(preparer.cache_root().starts_with(temporary.path()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(preparer.cache_root())
                    .expect("cache metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&first[0].path)
                    .expect("module metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn cache_survives_a_new_preparer_instance() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let url = "https://modules.example/music.s3m";
        let first_transport = MockTransport::once(url, s3m_fixture());
        let selected = request(url);
        {
            let mut preparer = TrackerMediaPreparer::new(
                temporary.path(),
                first_transport,
                TrackerMediaLimits::default(),
            )
            .expect("first preparer");
            preparer.prepare(&selected).expect("populate cache");
        }
        let second_transport = MockTransport::default();
        let mut preparer = TrackerMediaPreparer::new(
            temporary.path(),
            second_transport,
            TrackerMediaLimits::default(),
        )
        .expect("second preparer");

        let cached = preparer.prepare(&selected).expect("persistent cache hit");

        assert_eq!(preparer.transport().calls, 0);
        assert_eq!(cached.len(), 1);
    }

    #[test]
    fn gzip_is_sniffed_and_expansion_is_strictly_bounded() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let url = "https://amp.example/downmod.php?id=1";
        let compressed = gzip_fixture("S3M. Clockwork Life", &s3m_fixture());
        let transport = MockTransport::once(url, compressed);
        let limits = TrackerMediaLimits {
            max_uncompressed_bytes: 95,
            ..TrackerMediaLimits::default()
        };
        let mut preparer =
            TrackerMediaPreparer::new(temporary.path(), transport, limits).expect("preparer");

        let error = preparer
            .prepare(&request(url))
            .expect_err("96-byte expansion must be rejected");

        assert!(matches!(
            error,
            TrackerPrepareError::UncompressedTooLarge { limit: 95 }
        ));
    }

    #[test]
    fn gzip_member_name_and_signature_produce_a_playable_cache_file() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let url = "https://amp.example/downmod.php?id=1";
        let transport =
            MockTransport::once(url, gzip_fixture("S3M. Clockwork Life", &s3m_fixture()));
        let mut preparer =
            TrackerMediaPreparer::new(temporary.path(), transport, TrackerMediaLimits::default())
                .expect("preparer");

        let modules = preparer.prepare(&request(url)).expect("gzip preparation");

        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].format, "s3m");
        assert!(
            modules[0]
                .path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(".s3m"))
        );
    }

    #[test]
    fn extensionless_zip_is_sniffed_and_extracts_only_modules() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let url = "https://modules.example/download?id=42";
        let archive = zip_fixture(&[
            ("../../outside.s3m", &s3m_fixture()),
            ("album/second.xm", &xm_fixture()),
            ("album/README.txt", b"not music"),
        ]);
        let transport = MockTransport::once(url, archive);
        let mut preparer =
            TrackerMediaPreparer::new(temporary.path(), transport, TrackerMediaLimits::default())
                .expect("preparer");
        let mut selected = request(url);
        selected.source_label = Some("modules.pl".to_owned());
        selected.expected_format = Some("archive".to_owned());

        let modules = preparer
            .prepare(&selected)
            .expect("extensionless ZIP preparation");

        assert_eq!(
            modules
                .iter()
                .map(|module| module.format.as_str())
                .collect::<Vec<_>>(),
            ["s3m", "xm"]
        );
        assert!(modules.iter().all(|module| {
            module.path.starts_with(preparer.cache_root())
                && !module
                    .path
                    .components()
                    .any(|component| component.as_os_str() == "..")
        }));
    }

    #[test]
    fn zip_declared_expansion_and_entry_counts_are_bounded() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let url = "https://scene.example/package.zip";
        let archive = zip_fixture(&[("fixture.s3m", &s3m_fixture())]);
        let transport = MockTransport::once(url, archive);
        let limits = TrackerMediaLimits {
            max_uncompressed_bytes: 95,
            ..TrackerMediaLimits::default()
        };
        let mut preparer =
            TrackerMediaPreparer::new(temporary.path(), transport, limits).expect("preparer");
        assert!(matches!(
            preparer
                .prepare(&request(url))
                .expect_err("declared ZIP expansion"),
            TrackerPrepareError::UncompressedTooLarge { limit: 95 }
        ));

        let second_root = temporary.path().join("entries");
        let archive = zip_fixture(&[("fixture.s3m", &s3m_fixture()), ("README.txt", b"ignored")]);
        let transport = MockTransport::once(url, archive);
        let limits = TrackerMediaLimits {
            max_archive_entries: 1,
            ..TrackerMediaLimits::default()
        };
        let mut preparer =
            TrackerMediaPreparer::new(second_root, transport, limits).expect("preparer");
        assert!(matches!(
            preparer
                .prepare(&request(url))
                .expect_err("ZIP entry count"),
            TrackerPrepareError::TooManyArchiveEntries { limit: 1 }
        ));
    }

    #[test]
    fn html_interstitial_cannot_be_mislabeled_as_an_expected_module() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let url = "https://amp.example/downmod.php?token=do-not-report";
        let transport = MockTransport::once(
            url,
            b"\xef\xbb\xbf  <!DOCTYPE html><title>download unavailable</title>".to_vec(),
        );
        let mut preparer =
            TrackerMediaPreparer::new(temporary.path(), transport, TrackerMediaLimits::default())
                .expect("preparer");
        let mut selected = request(url);
        selected.source_label = Some("Amiga Music Preservation".to_owned());
        selected.expected_format = Some("s3m".to_owned());
        selected.display_name = Some("S3M. Clockwork Life".to_owned());

        let error = preparer
            .prepare(&selected)
            .expect_err("HTML must not become an S3M file");
        let message = error.to_string();

        assert!(matches!(
            error,
            TrackerPrepareError::NoSupportedModule {
                detected: "HTML response",
                ..
            }
        ));
        assert!(message.contains("Amiga Music Preservation"));
        assert!(message.contains("expected format: s3m"));
        assert!(message.contains("detected: HTML response"));
        assert!(!message.contains("do-not-report"));
        assert!(!message.contains("https://"));
    }

    #[test]
    fn unsupported_archive_magic_has_an_explicit_safe_diagnostic() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let url = "https://scene.example/download?token=do-not-report";
        let transport = MockTransport::once(url, b"Rar!\x1a\x07\x01\0payload".to_vec());
        let mut preparer =
            TrackerMediaPreparer::new(temporary.path(), transport, TrackerMediaLimits::default())
                .expect("preparer");
        let mut selected = request(url);
        selected.source_label = Some("scene.org".to_owned());

        let error = preparer
            .prepare(&selected)
            .expect_err("RAR is intentionally unsupported");
        let message = error.to_string();

        assert!(matches!(
            error,
            TrackerPrepareError::UnsupportedArchive {
                archive_type: "RAR",
                ..
            }
        ));
        assert!(message.contains("scene.org"));
        assert!(message.contains("RAR archive"));
        assert!(!message.contains("do-not-report"));
    }

    #[test]
    fn extensionless_legacy_module_can_use_a_valid_provider_format_hint() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let url = "https://modules.example/dl.php?id=7";
        let transport =
            MockTransport::once(url, b"legacy module without a short signature".to_vec());
        let mut preparer =
            TrackerMediaPreparer::new(temporary.path(), transport, TrackerMediaLimits::default())
                .expect("preparer");
        let mut selected = request(url);
        selected.source_label = Some("modules.pl".to_owned());
        selected.expected_format = Some("mod".to_owned());
        selected.display_name = Some("Clockwork Life".to_owned());

        let modules = preparer
            .prepare(&selected)
            .expect("trusted supported format hint");

        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].format, "mod");
    }

    #[test]
    fn invalid_gzip_never_publishes_a_cache_entry_and_can_be_retried() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let url = "https://amp.example/downmod.php?id=2";
        let mut corrupt = gzip_fixture("fixture.s3m", &s3m_fixture());
        let last = corrupt.last_mut().expect("gzip trailer");
        *last ^= 0xff;
        let transport = MockTransport {
            calls: 0,
            responses: VecDeque::from([
                Ok(MockResponse {
                    final_url: Url::parse(url).expect("URL"),
                    redirects: Vec::new(),
                    advertised_length: None,
                    body: corrupt,
                }),
                Ok(MockResponse {
                    final_url: Url::parse(url).expect("URL"),
                    redirects: Vec::new(),
                    advertised_length: None,
                    body: gzip_fixture("fixture.s3m", &s3m_fixture()),
                }),
            ]),
        };
        let mut preparer =
            TrackerMediaPreparer::new(temporary.path(), transport, TrackerMediaLimits::default())
                .expect("preparer");

        assert!(matches!(
            preparer.prepare(&request(url)).expect_err("invalid gzip"),
            TrackerPrepareError::InvalidArchive(_)
        ));
        let modules = preparer.prepare(&request(url)).expect("clean retry");

        assert_eq!(preparer.transport().calls, 2);
        assert_eq!(modules.len(), 1);
    }

    #[test]
    fn lha_extracts_multiple_modules_without_trusting_archive_paths() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let url = "https://aminet.example/mods/package.lha";
        let archive = lha_fixture(&[
            ("../../outside.s3m", &s3m_fixture()),
            ("album/second.xm", &xm_fixture()),
            ("album/README", b"not music"),
        ]);
        let transport = MockTransport::once(url, archive);
        let mut preparer =
            TrackerMediaPreparer::new(temporary.path(), transport, TrackerMediaLimits::default())
                .expect("preparer");

        let modules = preparer.prepare(&request(url)).expect("LHA preparation");

        assert_eq!(modules.len(), 2);
        assert_eq!(
            modules
                .iter()
                .map(|module| module.format.as_str())
                .collect::<Vec<_>>(),
            ["s3m", "xm"]
        );
        assert!(
            modules
                .iter()
                .all(|module| module.path.starts_with(preparer.cache_root()))
        );
        assert!(modules.iter().all(|module| {
            !module
                .path
                .components()
                .any(|component| component.as_os_str() == "..")
        }));
    }

    #[test]
    fn lha_sniffs_an_extensionless_module_inside_nested_directories() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let url = "https://aminet.example/mods/clockwork-life.lha";
        let archive = lha_fixture(&[
            ("Clockwork Life/README", b"Aminet package documentation"),
            ("Clockwork Life/Modules/Clockwork Life", &s3m_fixture()),
            ("Clockwork Life/screenshot.iff", b"not tracker music"),
        ]);
        let transport = MockTransport::once(url, archive);
        let mut preparer =
            TrackerMediaPreparer::new(temporary.path(), transport, TrackerMediaLimits::default())
                .expect("preparer");
        let mut selected = request(url);
        selected.source_label = Some("Aminet mods".to_owned());
        selected.expected_format = Some("lha".to_owned());

        let modules = preparer
            .prepare(&selected)
            .expect("nested extensionless LHA module");

        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].format, "s3m");
        assert_eq!(modules[0].size_bytes, s3m_fixture().len() as u64);
        assert!(modules[0].path.starts_with(preparer.cache_root()));
    }

    #[test]
    fn lha_accepts_xpk_sqsh_wrapped_extensionless_module() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let url = "https://aminet.example/mods/atmos/life.lha";
        let payload = b"MMD1 fixture OctaMED module";
        let xpk = xpk_sqsh_fixture(
            b"MMD1",
            payload,
            u32::try_from(payload.len()).expect("small fixture"),
        );
        let archive = lha_fixture(&[("Highlander/Life", &xpk)]);
        let transport = MockTransport::once(url, archive);
        let mut preparer =
            TrackerMediaPreparer::new(temporary.path(), transport, TrackerMediaLimits::default())
                .expect("preparer");
        let mut selected = request(url);
        selected.source_label = Some("Aminet mods".to_owned());
        selected.expected_format = Some("lha".to_owned());

        let modules = preparer
            .prepare(&selected)
            .expect("XPK-wrapped OctaMED preparation");

        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].format, "med");
        assert_eq!(modules[0].size_bytes, xpk.len() as u64);
        assert_eq!(
            modules[0].path.extension().and_then(|value| value.to_str()),
            Some("med")
        );
    }

    #[test]
    fn lha_rejects_xpk_expansion_beyond_the_archive_limit() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let url = "https://aminet.example/mods/atmos/oversized.lha";
        let xpk = xpk_sqsh_fixture(b"MMD1", b"compressed fixture", 256);
        let archive = lha_fixture(&[("Highlander/Oversized", &xpk)]);
        let limit = xpk.len() as u64 + 1;
        let limits = TrackerMediaLimits {
            max_uncompressed_bytes: limit,
            ..TrackerMediaLimits::default()
        };
        let transport = MockTransport::once(url, archive);
        let mut preparer =
            TrackerMediaPreparer::new(temporary.path(), transport, limits).expect("preparer");

        let error = preparer
            .prepare(&request(url))
            .expect_err("declared XPK expansion must exceed the archive limit");

        assert!(matches!(
            error,
            TrackerPrepareError::UncompressedTooLarge {
                limit: actual_limit
            } if actual_limit == limit
        ));
    }

    #[test]
    fn lha_keeps_amiga_prefix_module_names_without_byte_signatures() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let url = "https://aminet.example/mods/amiga-name.lha";
        let archive = lha_fixture(&[
            ("package/docs/readme", b"documentation"),
            ("package/music/MOD.Clockwork Life", b"legacy module fixture"),
        ]);
        let transport = MockTransport::once(url, archive);
        let mut preparer =
            TrackerMediaPreparer::new(temporary.path(), transport, TrackerMediaLimits::default())
                .expect("preparer");

        let modules = preparer
            .prepare(&request(url))
            .expect("Amiga-prefixed LHA module");

        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].format, "mod");
        assert_eq!(modules[0].display_name, "clockwork life");
    }

    #[test]
    fn lha_module_count_is_bounded_before_cache_publication() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let url = "https://aminet.example/mods/package.lha";
        let archive = lha_fixture(&[("first.s3m", &s3m_fixture()), ("second.xm", &xm_fixture())]);
        let limits = TrackerMediaLimits {
            max_modules: 1,
            ..TrackerMediaLimits::default()
        };
        let transport = MockTransport::once(url, archive);
        let mut preparer =
            TrackerMediaPreparer::new(temporary.path(), transport, limits).expect("preparer");

        let error = preparer
            .prepare(&request(url))
            .expect_err("second module must exceed the cap");

        assert!(matches!(
            error,
            TrackerPrepareError::TooManyModules { limit: 1 }
        ));
    }

    #[test]
    fn advertised_and_streamed_download_limits_are_both_enforced() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let url = "https://modules.example/large.s3m";
        let limits = TrackerMediaLimits {
            max_download_bytes: 8,
            ..TrackerMediaLimits::default()
        };
        let advertised = MockTransport {
            calls: 0,
            responses: VecDeque::from([Ok(MockResponse {
                final_url: Url::parse(url).expect("URL"),
                redirects: Vec::new(),
                advertised_length: Some(9),
                body: Vec::new(),
            })]),
        };
        let mut preparer =
            TrackerMediaPreparer::new(temporary.path(), advertised, limits).expect("preparer");
        assert!(matches!(
            preparer
                .prepare(&request(url))
                .expect_err("advertised length"),
            TrackerPrepareError::DownloadTooLarge { limit: 8 }
        ));

        let streamed = MockTransport::once(url, vec![0; 9]);
        let second_root = temporary.path().join("second");
        let mut preparer =
            TrackerMediaPreparer::new(second_root, streamed, limits).expect("preparer");
        assert!(matches!(
            preparer
                .prepare(&request(url))
                .expect_err("streamed length"),
            TrackerPrepareError::DownloadTooLarge { limit: 8 }
        ));
    }

    #[test]
    fn unsafe_urls_and_redirects_are_rejected_before_publication() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let mut insecure = request("http://modules.example/file.s3m");
        let transport = MockTransport::default();
        let mut preparer =
            TrackerMediaPreparer::new(temporary.path(), transport, TrackerMediaLimits::default())
                .expect("preparer");
        assert!(matches!(
            preparer.prepare(&insecure).expect_err("HTTP is opt-in"),
            TrackerPrepareError::UnsafeUrl
        ));

        insecure.allow_insecure_http = true;
        let other_host = MockTransport {
            calls: 0,
            responses: VecDeque::from([Ok(MockResponse {
                final_url: Url::parse("https://cdn.example/file.s3m").expect("URL"),
                redirects: vec![Url::parse("https://cdn.example/file.s3m").expect("redirect URL")],
                advertised_length: None,
                body: s3m_fixture(),
            })]),
        };
        let other_root = temporary.path().join("redirect");
        let mut preparer =
            TrackerMediaPreparer::new(other_root, other_host, TrackerMediaLimits::default())
                .expect("preparer");
        assert!(matches!(
            preparer
                .prepare(&insecure)
                .expect_err("cross-host redirect"),
            TrackerPrepareError::UnsafeRedirect
        ));
    }

    #[test]
    fn ureq_transport_rejects_zero_timeout_and_response_limit() {
        assert!(UreqTrackerTransport::new(Duration::ZERO, 1).is_err());
        assert!(UreqTrackerTransport::new(Duration::from_secs(1), 0).is_err());

        let transport = UreqTrackerTransport::for_limits(
            Duration::from_secs(1),
            TrackerMediaLimits {
                max_download_bytes: 17,
                ..TrackerMediaLimits::default()
            },
        )
        .expect("valid bounded transport");
        assert_eq!(transport.max_response_bytes(), 17);
    }

    #[test]
    fn redirect_helper_resolves_relative_same_host_targets() {
        let initial = Url::parse("https://modules.example/download/start").expect("initial URL");
        let current = Url::parse("https://modules.example/download/step").expect("current URL");

        let target = validated_redirect_target(&initial, &current, "../music/file.s3m")
            .expect("same-host relative redirect");

        assert_eq!(target.as_str(), "https://modules.example/music/file.s3m");
    }

    #[test]
    fn redirect_helper_upgrades_legacy_same_host_http_location() {
        let initial = Url::parse("https://amp.example/downmod.php?id=1").expect("initial URL");
        let target = validated_redirect_target(
            &initial,
            &initial,
            "http://amp.example/modules/S3M.Clockwork%20Life.gz",
        )
        .expect("same-host legacy redirect");

        assert_eq!(
            target.as_str(),
            "https://amp.example/modules/S3M.Clockwork%20Life.gz"
        );
    }

    #[test]
    fn redirect_helper_rejects_host_changes_credentials_and_unsafe_schemes() {
        let initial = Url::parse("https://modules.example/start").expect("initial URL");
        let current = initial.clone();

        for location in [
            "https://cdn.example/file.s3m",
            "https://user:secret@modules.example/file.s3m",
            "http://modules.example:8080/file.s3m",
            "file:///tmp/file.s3m",
        ] {
            assert!(
                validated_redirect_target(&initial, &current, location).is_err(),
                "redirect must be rejected: {location}"
            );
        }
    }

    #[test]
    fn response_validation_rejects_a_late_https_to_http_downgrade() {
        let initial = Url::parse("http://modules.example/start").expect("initial URL");
        let secure = Url::parse("https://modules.example/step").expect("secure redirect URL");
        let downgraded =
            Url::parse("http://modules.example/file.s3m").expect("downgraded final URL");
        let response = TrackerTransportResponse::new(downgraded, Cursor::new(s3m_fixture()))
            .with_redirects(vec![secure]);

        assert!(matches!(
            validate_response_urls(&initial, &response, true),
            Err(TrackerPrepareError::UnsafeRedirect)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn cache_symlink_cannot_escape_the_supplied_root() {
        use std::os::unix::fs::symlink;

        let storage = tempfile::tempdir().expect("storage root");
        let outside = tempfile::tempdir().expect("outside directory");
        symlink(outside.path(), storage.path().join(CACHE_DIRECTORY_NAME)).expect("cache symlink");

        let error = TrackerMediaPreparer::new(
            storage.path(),
            MockTransport::default(),
            TrackerMediaLimits::default(),
        )
        .err()
        .expect("escaping cache must fail");

        assert!(matches!(error, TrackerPrepareError::CacheEscapedRoot));
    }
}
