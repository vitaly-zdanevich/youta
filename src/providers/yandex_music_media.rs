//! Streaming decryption and durable publication for Yandex Music media.
//!
//! Yandex Music may return its highest-quality media encrypted with AES-CTR.
//! The service uses a zero 128-bit initial counter and increments the counter
//! in big-endian order. CTR encryption and decryption are the same operation,
//! so this module calls the operation "decryption" to make its intended use
//! explicit.
//!
//! The primitives deliberately accept borrowed ephemeral keys and never
//! implement [`Debug`] by exposing cipher state. Callers must not persist,
//! display, or log a key returned by the provider.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use aes::{Aes128, Aes192, Aes256};
use ctr::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use thiserror::Error;
use url::Url;

use super::DEFAULT_REQUEST_TIMEOUT;
use super::yandex_music::{YandexMusicCodec, YandexMusicMedia, is_allowed_media_url};

const AES_BLOCK_BYTES: usize = 16;
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const MAX_STAGING_ATTEMPTS: u16 = 128;
/// Maximum CDN hops followed after the signed file-info URL.
const MAX_MEDIA_REDIRECTS: usize = 2;
/// Upper bound for one downloaded track, episode, or audiobook chapter.
///
/// This remains intentionally generous for long lossless spoken-word media,
/// while preventing an unbounded or malformed CDN response from filling the
/// user's filesystem when neither API nor HTTP metadata supplies a size.
pub const MAX_YANDEX_MUSIC_MEDIA_BYTES: u64 = 8 * 1024 * 1024 * 1024;
static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

type Aes128Ctr = ctr::Ctr128BE<Aes128>;
type Aes192Ctr = ctr::Ctr128BE<Aes192>;
type Aes256Ctr = ctr::Ctr128BE<Aes256>;

/// A Yandex Music AES-CTR stream positioned at one absolute media byte.
///
/// Create a fresh cipher with the HTTP response's absolute starting byte when
/// decrypting a range response. For example, a response beginning with
/// `Content-Range: bytes 65536-...` must use offset `65536`, not zero. Feed
/// consecutive chunks from that response to one instance without seeking it
/// again.
pub struct YandexMusicMediaCipher {
    cipher: YandexMusicMediaCipherInner,
}

enum YandexMusicMediaCipherInner {
    Aes128(Aes128Ctr),
    Aes192(Aes192Ctr),
    Aes256(Aes256Ctr),
}

impl std::fmt::Debug for YandexMusicMediaCipher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("YandexMusicMediaCipher")
            .field("key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl YandexMusicMediaCipher {
    /// Creates a decryptor at `absolute_byte_offset` in the original payload.
    ///
    /// Yandex currently supplies 128-, 192-, or 256-bit AES keys. The initial
    /// 128-bit counter is zero, as required by the service protocol.
    ///
    /// # Errors
    ///
    /// Returns [`YandexMusicMediaError::InvalidKeyLength`] when `key` is not
    /// 16, 24, or 32 bytes.
    pub fn new(key: &[u8], absolute_byte_offset: u64) -> Result<Self, YandexMusicMediaError> {
        let initial_counter = [0_u8; AES_BLOCK_BYTES];
        let mut cipher = match key.len() {
            16 => YandexMusicMediaCipherInner::Aes128(
                Aes128Ctr::new_from_slices(key, &initial_counter)
                    .map_err(|_| YandexMusicMediaError::InvalidKeyLength(key.len()))?,
            ),
            24 => YandexMusicMediaCipherInner::Aes192(
                Aes192Ctr::new_from_slices(key, &initial_counter)
                    .map_err(|_| YandexMusicMediaError::InvalidKeyLength(key.len()))?,
            ),
            32 => YandexMusicMediaCipherInner::Aes256(
                Aes256Ctr::new_from_slices(key, &initial_counter)
                    .map_err(|_| YandexMusicMediaError::InvalidKeyLength(key.len()))?,
            ),
            length => return Err(YandexMusicMediaError::InvalidKeyLength(length)),
        };
        cipher.seek(absolute_byte_offset);
        Ok(Self { cipher })
    }

    /// Decrypts one consecutive ciphertext chunk in place.
    ///
    /// The cipher advances by `bytes.len()`, so the next call must contain the
    /// immediately following ciphertext bytes.
    ///
    /// # Errors
    ///
    /// Returns [`YandexMusicMediaError::CounterOverflow`] if the stream would
    /// exceed the AES-CTR counter space.
    pub fn decrypt(&mut self, bytes: &mut [u8]) -> Result<(), YandexMusicMediaError> {
        let result = match &mut self.cipher {
            YandexMusicMediaCipherInner::Aes128(cipher) => cipher.try_apply_keystream(bytes),
            YandexMusicMediaCipherInner::Aes192(cipher) => cipher.try_apply_keystream(bytes),
            YandexMusicMediaCipherInner::Aes256(cipher) => cipher.try_apply_keystream(bytes),
        };
        result.map_err(|_| YandexMusicMediaError::CounterOverflow)
    }
}

trait SeekCipher {
    fn seek(&mut self, absolute_byte_offset: u64);
}

impl SeekCipher for YandexMusicMediaCipherInner {
    fn seek(&mut self, absolute_byte_offset: u64) {
        match self {
            Self::Aes128(cipher) => cipher.seek(absolute_byte_offset),
            Self::Aes192(cipher) => cipher.seek(absolute_byte_offset),
            Self::Aes256(cipher) => cipher.seek(absolute_byte_offset),
        }
    }
}

/// Decrypts an independently fetched media range in place.
///
/// `absolute_byte_offset` is the first byte's offset in the complete encrypted
/// object. Pass zero only for a response that begins at byte zero.
///
/// # Errors
///
/// Returns an error for an unsupported key length or an exhausted AES counter.
pub fn decrypt_yandex_music_range(
    key: &[u8],
    absolute_byte_offset: u64,
    bytes: &mut [u8],
) -> Result<(), YandexMusicMediaError> {
    YandexMusicMediaCipher::new(key, absolute_byte_offset)?.decrypt(bytes)
}

/// Result of one successfully published media download.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct YandexMusicDownload {
    /// Number of decrypted or copied payload bytes written to the destination.
    pub bytes_written: u64,
    /// Whether the source payload required AES-CTR decryption.
    pub decrypted: bool,
}

/// One streamed download progress observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct YandexMusicDownloadProgress {
    /// Complete payload bytes copied or decrypted so far.
    pub bytes_written: u64,
    /// Best known complete payload size from API or HTTP metadata.
    pub total_bytes: Option<u64>,
}

/// Blocking downloader for resolved Yandex Music media.
///
/// Clone this value when several worker jobs should share one HTTP connection
/// pool. The downloader never sends the user's OAuth token: a resolved media
/// URL is short-lived and credential-free.
#[derive(Clone, Debug)]
pub struct YandexMusicMediaFetcher {
    agent: ureq::Agent,
}

impl Default for YandexMusicMediaFetcher {
    fn default() -> Self {
        Self::new(DEFAULT_REQUEST_TIMEOUT)
    }
}

impl YandexMusicMediaFetcher {
    /// Creates a downloader with a timeout for setup and each idle body read.
    ///
    /// DNS resolution, connection/TLS setup, request transmission, and waiting
    /// for response headers are bounded independently by `setup_timeout`.
    /// `ureq` resets its receive-body timeout before each blocking input wait,
    /// so a continuously progressing lossless release or audiobook has no
    /// whole-transfer deadline, while a CDN that stops sending bytes cannot
    /// block cancellation or graceful shutdown indefinitely. At most two
    /// redirects are followed, and every target must remain inside the same
    /// credential-free Yandex CDN boundary as a directly resolved URL.
    #[must_use]
    pub fn new(setup_timeout: Duration) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(None)
            .timeout_per_call(None)
            .timeout_resolve(Some(setup_timeout))
            .timeout_connect(Some(setup_timeout))
            .timeout_send_request(Some(setup_timeout))
            .timeout_recv_response(Some(setup_timeout))
            .timeout_recv_body(Some(setup_timeout))
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
        Self { agent }
    }

    /// Fetches and atomically publishes one resolved media object.
    ///
    /// Plain payloads are copied as-is. Encrypted lossless payloads are
    /// decrypted while streaming, so neither ciphertext nor the ephemeral key
    /// is persisted. An existing destination is never replaced.
    ///
    /// When both Yandex metadata and the HTTP response declare a size, they
    /// must agree. The final streamed byte count is also checked before
    /// publication.
    ///
    /// # Errors
    ///
    /// Returns an error for a failed HTTP request, conflicting declared sizes,
    /// decryption failure, truncated or oversized payload, destination I/O, or
    /// an already-existing destination.
    pub fn fetch(
        &self,
        media: &YandexMusicMedia,
        destination: &Path,
    ) -> Result<YandexMusicDownload, YandexMusicMediaFetchError> {
        self.fetch_with_progress(media, destination, |_| {})
    }

    /// Fetches and atomically publishes media while reporting byte progress.
    ///
    /// The callback first receives zero bytes after the response size is
    /// validated, then receives one update for every streamed chunk written to
    /// the staging file. Callers updating a terminal UI may coalesce these
    /// synchronous observations before sending them to the event loop.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::fetch`].
    pub fn fetch_with_progress<F>(
        &self,
        media: &YandexMusicMedia,
        destination: &Path,
        progress: F,
    ) -> Result<YandexMusicDownload, YandexMusicMediaFetchError>
    where
        F: FnMut(YandexMusicDownloadProgress),
    {
        fetch_yandex_music_media_with_progress(
            &media.url,
            Some(media.codec),
            media.decryption_key(),
            media.size_bytes,
            destination,
            |url| fetch_media_response(&self.agent, url),
            progress,
        )
    }

    /// Fetches media with progress reporting and cooperative cancellation.
    ///
    /// `cancelled` is checked before the network request, before and after
    /// every streamed chunk, and immediately before publication. When
    /// cancellation is observed, the staging file is removed and
    /// `destination` is never created or replaced.
    ///
    /// The synchronous `ureq` API cannot interrupt a read already blocked
    /// inside the operating system. Cancellation takes effect as soon as that
    /// read returns, and [`Self::new`] bounds every setup phase and idle body
    /// read.
    ///
    /// # Errors
    ///
    /// Returns [`YandexMusicMediaFetchError::Cancelled`] when cancellation is
    /// observed, or the same transport and publication errors as
    /// [`Self::fetch`].
    pub fn fetch_with_progress_and_cancellation<F>(
        &self,
        media: &YandexMusicMedia,
        destination: &Path,
        cancelled: &AtomicBool,
        progress: F,
    ) -> Result<YandexMusicDownload, YandexMusicMediaFetchError>
    where
        F: FnMut(YandexMusicDownloadProgress),
    {
        fetch_yandex_music_media_plan(
            MediaFetchPlan {
                source: &media.url,
                expected_codec: Some(media.codec),
                decryption_key: media.decryption_key(),
                provider_size: media.size_bytes,
                destination,
                cancelled: Some(cancelled),
            },
            |url| fetch_media_response(&self.agent, url),
            progress,
        )
    }
}

/// Errors produced while fetching resolved Yandex Music media.
///
/// Error messages deliberately omit the signed media URL and the ephemeral
/// decryption key.
#[derive(Debug, Error)]
pub enum YandexMusicMediaFetchError {
    /// The controller requested cancellation before publication.
    #[error("Yandex Music media download was cancelled")]
    Cancelled,
    /// The resolved URL was modified after provider validation.
    #[error("Yandex Music media URL is outside the allowed HTTPS CDN boundary")]
    InvalidSource,
    /// The resolved CDN request returned a non-success status.
    #[error("Yandex Music media server returned HTTP status {0}")]
    HttpStatus(u16),
    /// The resolved CDN request failed before a body could be read.
    #[error("Yandex Music media request failed: {0}")]
    Transport(&'static str),
    /// A successful response declared a non-media representation.
    #[error("Yandex Music media server returned a non-media content type")]
    UnexpectedContentType,
    /// The API and CDN disagreed about the complete object size.
    #[error(
        "Yandex Music media size declarations disagree: API reported {provider} bytes, HTTP reported {http} bytes"
    )]
    DeclaredSizeMismatch {
        /// Byte count returned by file-info resolution.
        provider: u64,
        /// Byte count returned in the CDN response.
        http: u64,
    },
    /// Streaming decryption or publication failed.
    #[error("could not publish Yandex Music media: {0}")]
    Publish(#[from] YandexMusicMediaError),
}

/// Errors produced while decrypting or atomically publishing Yandex media.
#[derive(Debug, Error)]
pub enum YandexMusicMediaError {
    /// A cooperative cancellation flag was observed before publication.
    #[error("Yandex Music media download was cancelled")]
    Cancelled,
    /// The service supplied an unsupported AES key size.
    #[error("Yandex Music media key has unsupported length {0}; expected 16, 24, or 32 bytes")]
    InvalidKeyLength(usize),
    /// The AES-CTR stream exceeded its counter space.
    #[error("Yandex Music media cipher counter overflowed")]
    CounterOverflow,
    /// The source length did not match the provider's declared length.
    #[error("Yandex Music media size mismatch: expected {expected} bytes, received {actual}")]
    SizeMismatch {
        /// Provider-declared encrypted payload length.
        expected: u64,
        /// Bytes actually read from the source.
        actual: u64,
    },
    /// The source exceeded the maximum accepted media-object size.
    #[error("Yandex Music media exceeds the {limit}-byte safety limit")]
    SizeLimitExceeded {
        /// Maximum accepted bytes for one media object.
        limit: u64,
    },
    /// The decoded payload header did not match the negotiated codec/container.
    #[error("Yandex Music media payload does not match the negotiated codec")]
    InvalidPayload,
    /// The destination does not name a file inside an existing directory.
    #[error("Yandex Music download destination must name a file inside a directory")]
    InvalidDestination,
    /// Reading, writing, syncing, or publishing the media failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

struct FetchedMedia {
    reader: Box<dyn Read + Send>,
    content_length: Option<u64>,
    content_type: Option<String>,
}

/// Immutable validation and publication inputs for one resolved media fetch.
#[derive(Clone, Copy)]
struct MediaFetchPlan<'a> {
    source: &'a Url,
    expected_codec: Option<YandexMusicCodec>,
    decryption_key: Option<&'a [u8]>,
    provider_size: Option<u64>,
    destination: &'a Path,
    cancelled: Option<&'a AtomicBool>,
}

/// Immutable bounds and cryptographic inputs for one atomic publication.
#[derive(Clone, Copy)]
struct MediaPublishPlan<'a> {
    destination: &'a Path,
    decryption_key: Option<&'a [u8]>,
    expected_codec: Option<YandexMusicCodec>,
    expected_size: Option<u64>,
    max_size: u64,
    cancelled: Option<&'a AtomicBool>,
}

fn fetch_media_response(
    agent: &ureq::Agent,
    source: &Url,
) -> Result<FetchedMedia, YandexMusicMediaFetchError> {
    let mut current = source.clone();
    for redirects_followed in 0..=MAX_MEDIA_REDIRECTS {
        let response = agent
            .get(current.as_str())
            .header("Accept", "audio/*, application/octet-stream")
            .header("Accept-Encoding", "identity")
            .call()
            .map_err(|error| map_fetch_error(&error))?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .ok_or(YandexMusicMediaFetchError::Transport(
                    "CDN returned an invalid redirect",
                ))?;
            current = resolve_media_redirect(&current, location, redirects_followed)?;
            continue;
        }

        let content_length = response.body().content_length();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let (_, body) = response.into_parts();
        return Ok(FetchedMedia {
            reader: Box::new(body.into_reader()),
            content_length,
            content_type,
        });
    }
    unreachable!("the bounded redirect loop always returns")
}

/// Resolves one relative or absolute CDN redirect without widening the media
/// host allow-list used for the original file-info response.
fn resolve_media_redirect(
    current: &Url,
    location: &str,
    redirects_followed: usize,
) -> Result<Url, YandexMusicMediaFetchError> {
    if redirects_followed >= MAX_MEDIA_REDIRECTS {
        return Err(YandexMusicMediaFetchError::Transport(
            "CDN returned too many redirects",
        ));
    }
    let next = current
        .join(location)
        .map_err(|_| YandexMusicMediaFetchError::Transport("CDN returned an invalid redirect"))?;
    if !is_allowed_media_url(&next) {
        return Err(YandexMusicMediaFetchError::InvalidSource);
    }
    Ok(next)
}

fn map_fetch_error(error: &ureq::Error) -> YandexMusicMediaFetchError {
    match error {
        ureq::Error::StatusCode(status) => YandexMusicMediaFetchError::HttpStatus(*status),
        ureq::Error::Timeout(_) => YandexMusicMediaFetchError::Transport("request timed out"),
        ureq::Error::HostNotFound => {
            YandexMusicMediaFetchError::Transport("CDN host was not found")
        }
        ureq::Error::ConnectionFailed => {
            YandexMusicMediaFetchError::Transport("CDN connection failed")
        }
        ureq::Error::Tls(_) => YandexMusicMediaFetchError::Transport("CDN TLS connection failed"),
        ureq::Error::RedirectFailed | ureq::Error::TooManyRedirects => {
            YandexMusicMediaFetchError::Transport("CDN redirect failed")
        }
        _ => YandexMusicMediaFetchError::Transport("CDN request failed"),
    }
}

#[cfg(test)]
fn fetch_yandex_music_media_with<F>(
    source: &Url,
    decryption_key: Option<&[u8]>,
    provider_size: Option<u64>,
    destination: &Path,
    fetch: F,
) -> Result<YandexMusicDownload, YandexMusicMediaFetchError>
where
    F: FnOnce(&Url) -> Result<FetchedMedia, YandexMusicMediaFetchError>,
{
    fetch_yandex_music_media_with_progress(
        source,
        None,
        decryption_key,
        provider_size,
        destination,
        fetch,
        |_| {},
    )
}

#[cfg(test)]
fn fetch_yandex_music_media_with_codec<F>(
    source: &Url,
    codec: YandexMusicCodec,
    decryption_key: Option<&[u8]>,
    provider_size: Option<u64>,
    destination: &Path,
    fetch: F,
) -> Result<YandexMusicDownload, YandexMusicMediaFetchError>
where
    F: FnOnce(&Url) -> Result<FetchedMedia, YandexMusicMediaFetchError>,
{
    fetch_yandex_music_media_with_progress(
        source,
        Some(codec),
        decryption_key,
        provider_size,
        destination,
        fetch,
        |_| {},
    )
}

fn fetch_yandex_music_media_with_progress<F, P>(
    source: &Url,
    expected_codec: Option<YandexMusicCodec>,
    decryption_key: Option<&[u8]>,
    provider_size: Option<u64>,
    destination: &Path,
    fetch: F,
    progress: P,
) -> Result<YandexMusicDownload, YandexMusicMediaFetchError>
where
    F: FnOnce(&Url) -> Result<FetchedMedia, YandexMusicMediaFetchError>,
    P: FnMut(YandexMusicDownloadProgress),
{
    fetch_yandex_music_media_plan(
        MediaFetchPlan {
            source,
            expected_codec,
            decryption_key,
            provider_size,
            destination,
            cancelled: None,
        },
        fetch,
        progress,
    )
}

#[cfg(test)]
fn fetch_yandex_music_media_with_progress_and_cancellation<F, P>(
    source: &Url,
    expected_codec: Option<YandexMusicCodec>,
    decryption_key: Option<&[u8]>,
    provider_size: Option<u64>,
    destination: &Path,
    fetch: F,
    cancelled: Option<&AtomicBool>,
    progress: P,
) -> Result<YandexMusicDownload, YandexMusicMediaFetchError>
where
    F: FnOnce(&Url) -> Result<FetchedMedia, YandexMusicMediaFetchError>,
    P: FnMut(YandexMusicDownloadProgress),
{
    fetch_yandex_music_media_plan(
        MediaFetchPlan {
            source,
            expected_codec,
            decryption_key,
            provider_size,
            destination,
            cancelled,
        },
        fetch,
        progress,
    )
}

fn fetch_yandex_music_media_plan<F, P>(
    plan: MediaFetchPlan<'_>,
    fetch: F,
    progress: P,
) -> Result<YandexMusicDownload, YandexMusicMediaFetchError>
where
    F: FnOnce(&Url) -> Result<FetchedMedia, YandexMusicMediaFetchError>,
    P: FnMut(YandexMusicDownloadProgress),
{
    let MediaFetchPlan {
        source,
        expected_codec,
        decryption_key,
        provider_size,
        destination,
        cancelled,
    } = plan;
    if !is_allowed_media_url(source) {
        return Err(YandexMusicMediaFetchError::InvalidSource);
    }
    if cancellation_requested(cancelled) {
        return Err(YandexMusicMediaFetchError::Cancelled);
    }
    ensure_yandex_music_media_size(provider_size, MAX_YANDEX_MUSIC_MEDIA_BYTES)?;
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            return Err(YandexMusicMediaError::Io(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Yandex Music download destination already exists",
            ))
            .into());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(YandexMusicMediaError::Io(error).into()),
    }
    let response = fetch(source)?;
    validate_media_content_type(response.content_type.as_deref())?;
    let expected_size = match (provider_size, response.content_length) {
        (Some(provider), Some(http)) if provider != http => {
            return Err(YandexMusicMediaFetchError::DeclaredSizeMismatch { provider, http });
        }
        (Some(provider), _) => Some(provider),
        (None, http) => http,
    };
    ensure_yandex_music_media_size(expected_size, MAX_YANDEX_MUSIC_MEDIA_BYTES)?;
    write_yandex_music_media_atomically_with_progress(
        response.reader,
        MediaPublishPlan {
            destination,
            decryption_key,
            expected_codec,
            expected_size,
            max_size: MAX_YANDEX_MUSIC_MEDIA_BYTES,
            cancelled,
        },
        progress,
    )
    .map_err(|error| match error {
        YandexMusicMediaError::Cancelled => YandexMusicMediaFetchError::Cancelled,
        error => YandexMusicMediaFetchError::Publish(error),
    })
}

fn validate_media_content_type(
    content_type: Option<&str>,
) -> Result<(), YandexMusicMediaFetchError> {
    let Some(content_type) = content_type else {
        return Ok(());
    };
    let media_type = content_type
        .split_once(';')
        .map_or(content_type, |(media_type, _)| media_type)
        .trim()
        .to_ascii_lowercase();
    if media_type.starts_with("audio/")
        || matches!(
            media_type.as_str(),
            "video/mp4" | "application/octet-stream" | "binary/octet-stream"
        )
    {
        return Ok(());
    }
    Err(YandexMusicMediaFetchError::UnexpectedContentType)
}

fn ensure_yandex_music_media_size(
    declared_size: Option<u64>,
    limit: u64,
) -> Result<(), YandexMusicMediaFetchError> {
    if declared_size.is_some_and(|size| size > limit) {
        return Err(YandexMusicMediaFetchError::Publish(
            YandexMusicMediaError::SizeLimitExceeded { limit },
        ));
    }
    Ok(())
}

fn cancellation_requested(cancelled: Option<&AtomicBool>) -> bool {
    cancelled.is_some_and(|cancelled| cancelled.load(Ordering::Acquire))
}

/// Copies a complete resolved payload to `destination`, decrypting when needed.
///
/// The function writes and syncs a uniquely named staging file in the
/// destination directory, then publishes it with an atomic hard link. It
/// never overwrites an existing destination. A failure before publication
/// leaves an existing destination unchanged and removes the staging file.
///
/// `decryption_key` must be the ephemeral key returned with the same resolved
/// URL. `expected_size` should be the provider's byte count when present. AES-
/// CTR does not change the payload length, so the same count validates both
/// encrypted and decrypted data.
///
/// This helper accepts any [`Read`], including the body reader of a validated
/// Yandex CDN response. Redirects and response status/range validation remain
/// the caller's responsibility.
///
/// # Errors
///
/// Returns an error for an invalid destination or key, source/destination I/O,
/// a length mismatch, or an already-existing destination.
pub fn write_yandex_music_media_atomically<R: Read>(
    source: R,
    destination: &Path,
    decryption_key: Option<&[u8]>,
    expected_size: Option<u64>,
) -> Result<YandexMusicDownload, YandexMusicMediaError> {
    write_yandex_music_media_atomically_with_progress(
        source,
        MediaPublishPlan {
            destination,
            decryption_key,
            expected_codec: None,
            expected_size,
            max_size: MAX_YANDEX_MUSIC_MEDIA_BYTES,
            cancelled: None,
        },
        |_| {},
    )
}

#[allow(
    clippy::too_many_lines,
    reason = "the streaming state machine keeps validation, decryption, and publication order explicit"
)]
fn write_yandex_music_media_atomically_with_progress<R, P>(
    mut source: R,
    plan: MediaPublishPlan<'_>,
    mut progress: P,
) -> Result<YandexMusicDownload, YandexMusicMediaError>
where
    R: Read,
    P: FnMut(YandexMusicDownloadProgress),
{
    let MediaPublishPlan {
        destination,
        decryption_key,
        expected_codec,
        expected_size,
        max_size,
        cancelled,
    } = plan;
    if cancellation_requested(cancelled) {
        return Err(YandexMusicMediaError::Cancelled);
    }
    if expected_size.is_some_and(|size| size > max_size) {
        return Err(YandexMusicMediaError::SizeLimitExceeded { limit: max_size });
    }
    let mut cipher = decryption_key
        .map(|key| YandexMusicMediaCipher::new(key, 0))
        .transpose()?;
    let (mut staging, staging_path) = create_staging_file(destination)?;
    let mut cleanup = StagingCleanup::new(staging_path);
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES].into_boxed_slice();
    let mut bytes_written = 0_u64;
    let mut payload_prefix = expected_codec.map(|_| Vec::with_capacity(12));
    progress(YandexMusicDownloadProgress {
        bytes_written,
        total_bytes: expected_size,
    });
    if cancellation_requested(cancelled) {
        return Err(YandexMusicMediaError::Cancelled);
    }

    loop {
        if cancellation_requested(cancelled) {
            return Err(YandexMusicMediaError::Cancelled);
        }
        let read_count = loop {
            match source.read(&mut buffer) {
                Ok(read) => break read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        };
        if read_count == 0 {
            break;
        }
        if cancellation_requested(cancelled) {
            return Err(YandexMusicMediaError::Cancelled);
        }

        let read = u64::try_from(read_count).map_err(|_| YandexMusicMediaError::CounterOverflow)?;
        bytes_written = bytes_written
            .checked_add(read)
            .ok_or(YandexMusicMediaError::CounterOverflow)?;
        if bytes_written > max_size {
            return Err(YandexMusicMediaError::SizeLimitExceeded { limit: max_size });
        }
        if let Some(expected) = expected_size
            && bytes_written > expected
        {
            return Err(YandexMusicMediaError::SizeMismatch {
                expected,
                actual: bytes_written,
            });
        }

        let chunk = &mut buffer[..read_count];
        if let Some(cipher) = cipher.as_mut() {
            cipher.decrypt(chunk)?;
        }
        if let Some(payload_prefix) = payload_prefix.as_mut()
            && payload_prefix.len() < 12
        {
            let remaining = 12 - payload_prefix.len();
            payload_prefix.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        }
        staging.write_all(chunk)?;
        progress(YandexMusicDownloadProgress {
            bytes_written,
            total_bytes: expected_size,
        });
        if cancellation_requested(cancelled) {
            return Err(YandexMusicMediaError::Cancelled);
        }
    }

    if let Some(expected) = expected_size
        && bytes_written != expected
    {
        return Err(YandexMusicMediaError::SizeMismatch {
            expected,
            actual: bytes_written,
        });
    }
    if let (Some(codec), Some(payload_prefix)) = (expected_codec, payload_prefix)
        && !media_payload_matches_codec(codec, &payload_prefix)
    {
        return Err(YandexMusicMediaError::InvalidPayload);
    }

    if cancellation_requested(cancelled) {
        return Err(YandexMusicMediaError::Cancelled);
    }
    staging.sync_all()?;
    if cancellation_requested(cancelled) {
        return Err(YandexMusicMediaError::Cancelled);
    }
    drop(staging);
    if cancellation_requested(cancelled) {
        return Err(YandexMusicMediaError::Cancelled);
    }
    fs::hard_link(cleanup.path(), destination)?;
    sync_destination_directory(destination)?;
    cleanup.remove()?;
    sync_destination_directory(destination)?;

    Ok(YandexMusicDownload {
        bytes_written,
        decrypted: decryption_key.is_some(),
    })
}

/// Persists link creation or removal in the destination directory.
///
/// The media bytes are synchronized before publication. Synchronizing the
/// directory after each link operation makes both the published name and
/// staging-name cleanup durable across a host crash, on the platforms that
/// expose it — see [`crate::durability`].
fn sync_destination_directory(destination: &Path) -> io::Result<()> {
    crate::durability::sync_parent_directory(destination)
}

fn media_payload_matches_codec(codec: YandexMusicCodec, prefix: &[u8]) -> bool {
    let has_id3 = prefix.starts_with(b"ID3");
    // ADTS reserves the MPEG layer bits as zero after its 12-bit sync word.
    let has_adts_frame_sync = prefix.len() >= 2 && prefix[0] == 0xff && prefix[1] & 0xf6 == 0xf0;
    // MPEG audio rejects the reserved version and requires Layer III (01).
    let has_mpeg_layer_three_sync = prefix.len() >= 2
        && prefix[0] == 0xff
        && prefix[1] & 0xe0 == 0xe0
        && prefix[1] & 0x18 != 0x08
        && prefix[1] & 0x06 == 0x02;
    match codec {
        YandexMusicCodec::Flac => prefix.starts_with(b"fLaC"),
        YandexMusicCodec::FlacMp4 | YandexMusicCodec::AacMp4 | YandexMusicCodec::HeAacMp4 => {
            prefix.get(4..8) == Some(b"ftyp")
        }
        YandexMusicCodec::Aac | YandexMusicCodec::HeAac => {
            prefix.starts_with(b"ADIF") || has_id3 || has_adts_frame_sync
        }
        YandexMusicCodec::Mp3 => has_id3 || has_mpeg_layer_three_sync,
    }
}

fn create_staging_file(destination: &Path) -> Result<(File, PathBuf), YandexMusicMediaError> {
    let file_name = destination
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or(YandexMusicMediaError::InvalidDestination)?;
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(YandexMusicMediaError::InvalidDestination);
    }

    for _ in 0..MAX_STAGING_ATTEMPTS {
        let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut staging_name = OsString::from(".");
        staging_name.push(file_name);
        staging_name.push(format!(
            ".youta-yandex-{}-{sequence}.part",
            std::process::id()
        ));
        let path = parent.join(staging_name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique Yandex Music staging file",
    )
    .into())
}

struct StagingCleanup {
    path: PathBuf,
    present: bool,
}

impl StagingCleanup {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            present: true,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn remove(&mut self) -> io::Result<()> {
        fs::remove_file(&self.path)?;
        self.present = false;
        Ok(())
    }
}

impl Drop for StagingCleanup {
    fn drop(&mut self) {
        if self.present {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{self, Cursor, Read};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    use tempfile::tempdir;
    use url::Url;

    use super::{
        COPY_BUFFER_BYTES, FetchedMedia, YandexMusicDownloadProgress, YandexMusicMediaCipher,
        YandexMusicMediaError, YandexMusicMediaFetchError, decrypt_yandex_music_range,
        fetch_yandex_music_media_with, fetch_yandex_music_media_with_codec,
        fetch_yandex_music_media_with_progress,
        fetch_yandex_music_media_with_progress_and_cancellation, resolve_media_redirect,
        write_yandex_music_media_atomically,
    };
    use crate::providers::yandex_music::YandexMusicCodec;

    const PLAINTEXT: &[u8] = b"Youta Yandex Music AES-CTR vector";

    fn decode_hex(hex: &str) -> Vec<u8> {
        hex.as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let text = std::str::from_utf8(pair).expect("ASCII fixture");
                u8::from_str_radix(text, 16).expect("hex fixture")
            })
            .collect()
    }

    #[test]
    fn media_timeout_policy_bounds_setup_and_each_idle_body_read() {
        let setup_timeout = Duration::from_millis(37);
        let fetcher = super::YandexMusicMediaFetcher::new(setup_timeout);
        let timeouts = fetcher.agent.config().timeouts();

        assert_eq!(timeouts.global, None);
        assert_eq!(timeouts.per_call, None);
        assert_eq!(timeouts.resolve, Some(setup_timeout));
        assert_eq!(timeouts.connect, Some(setup_timeout));
        assert_eq!(timeouts.send_request, Some(setup_timeout));
        assert_eq!(timeouts.recv_response, Some(setup_timeout));
        assert_eq!(
            timeouts.recv_body,
            Some(setup_timeout),
            "each stalled CDN body read must be bounded without imposing a whole-transfer deadline"
        );
        assert_eq!(
            fetcher.agent.config().max_redirects(),
            0,
            "ureq must not follow an unvalidated redirect before Youta checks its host"
        );
    }

    #[test]
    fn media_redirects_remain_inside_the_yandex_cdn_boundary() {
        let current =
            Url::parse("https://audio.storage.yandex.net/current/object").expect("fixture URL");

        assert_eq!(
            resolve_media_redirect(&current, "../next/object", 0)
                .expect("relative Yandex redirect"),
            Url::parse("https://audio.storage.yandex.net/next/object").expect("expected URL")
        );
        assert_eq!(
            resolve_media_redirect(&current, "https://music.yandexcdn.net/final/object", 1)
                .expect("cross-CDN Yandex redirect"),
            Url::parse("https://music.yandexcdn.net/final/object").expect("expected URL")
        );

        assert!(matches!(
            resolve_media_redirect(&current, "https://example.com/stolen", 0),
            Err(YandexMusicMediaFetchError::InvalidSource)
        ));
        assert!(matches!(
            resolve_media_redirect(&current, "https://audio.yandex.net/third", 2),
            Err(YandexMusicMediaFetchError::Transport(
                "CDN returned too many redirects"
            ))
        ));
    }

    #[test]
    fn aes_ctr_matches_independent_openssl_vectors_for_every_key_size() {
        let fixtures = [
            (
                "00112233445566778899aabbccddeeff",
                "a48b8eda2b29b941819347eebfcef658edb7e981ce1c656b35b189b0a638b0b66f",
            ),
            (
                "00112233445566778899aabbccddeeff0001020304050607",
                "35ae4586e9a4691fc71903222c5c92a3a8f1e4dabda418c9721e342cf33ba53c0c",
            ),
            (
                "00112233445566778899aabbccddeeff000102030405060708090a0b0c0d0e0f",
                "09f0038bf6a45d07f9583830b74ea2018896c2f7fad403a25236b0ae99c9a4c739",
            ),
        ];

        for (key, expected_ciphertext) in fixtures {
            let key = decode_hex(key);
            let mut bytes = PLAINTEXT.to_vec();
            YandexMusicMediaCipher::new(&key, 0)
                .expect("supported key")
                .decrypt(&mut bytes)
                .expect("vector length cannot overflow");
            assert_eq!(bytes, decode_hex(expected_ciphertext));
        }
    }

    #[test]
    fn arbitrary_unaligned_range_uses_the_absolute_media_offset() {
        let key = decode_hex("00112233445566778899aabbccddeeff");
        let mut ciphertext = vec![0_u8; 150_000];
        for (index, byte) in ciphertext.iter_mut().enumerate() {
            *byte = u8::try_from(index % 251).expect("fixture remainder fits u8");
        }
        let plaintext = ciphertext.clone();
        YandexMusicMediaCipher::new(&key, 0)
            .expect("cipher")
            .decrypt(&mut ciphertext)
            .expect("fixture cannot overflow");

        let offset = 65_543_usize;
        let mut range = ciphertext[offset..offset + 4099].to_vec();
        decrypt_yandex_music_range(
            &key,
            u64::try_from(offset).expect("offset fits u64"),
            &mut range,
        )
        .expect("range decrypts");
        assert_eq!(range, plaintext[offset..offset + 4099]);
    }

    #[test]
    fn consecutive_chunks_share_cipher_position_across_aes_blocks() {
        let key = decode_hex("00112233445566778899aabbccddeeff");
        let mut ciphertext = PLAINTEXT.repeat(9);
        YandexMusicMediaCipher::new(&key, 0)
            .expect("cipher")
            .decrypt(&mut ciphertext)
            .expect("encrypt fixture");

        let mut cipher = YandexMusicMediaCipher::new(&key, 0).expect("cipher");
        for chunk in ciphertext.chunks_mut(7) {
            cipher.decrypt(chunk).expect("decrypt chunk");
        }
        assert_eq!(ciphertext, PLAINTEXT.repeat(9));
    }

    #[test]
    fn invalid_key_is_rejected_without_exposing_it_in_debug_output() {
        let error =
            YandexMusicMediaCipher::new(b"too short", 0).expect_err("invalid key must fail");
        assert!(matches!(error, YandexMusicMediaError::InvalidKeyLength(9)));

        let key = decode_hex("00112233445566778899aabbccddeeff");
        let debug = format!(
            "{:?}",
            YandexMusicMediaCipher::new(&key, 0).expect("cipher")
        );
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("001122"));
    }

    #[test]
    fn atomic_writer_decrypts_and_publishes_complete_payload() {
        let directory = tempdir().expect("temporary directory");
        let destination = directory.path().join("track.flac");
        let key = decode_hex("00112233445566778899aabbccddeeff");
        let plaintext = PLAINTEXT.repeat(4_000);
        let mut ciphertext = plaintext.clone();
        YandexMusicMediaCipher::new(&key, 0)
            .expect("cipher")
            .decrypt(&mut ciphertext)
            .expect("encrypt fixture");

        let result = write_yandex_music_media_atomically(
            Cursor::new(ciphertext),
            &destination,
            Some(&key),
            Some(u64::try_from(plaintext.len()).expect("fixture length fits u64")),
        )
        .expect("download");

        assert_eq!(
            result.bytes_written,
            u64::try_from(plaintext.len()).expect("fixture length fits u64")
        );
        assert!(result.decrypted);
        assert_eq!(fs::read(&destination).expect("published media"), plaintext);
        assert_eq!(
            fs::read_dir(directory.path()).expect("directory").count(),
            1,
            "staging file must be removed"
        );
    }

    #[test]
    fn atomic_writer_copies_unencrypted_payload() {
        let directory = tempdir().expect("temporary directory");
        let destination = directory.path().join("track.mp3");

        let result =
            write_yandex_music_media_atomically(Cursor::new(PLAINTEXT), &destination, None, None)
                .expect("download");

        assert_eq!(result.bytes_written, PLAINTEXT.len() as u64);
        assert!(!result.decrypted);
        assert_eq!(fs::read(destination).expect("published media"), PLAINTEXT);
    }

    #[test]
    fn fetcher_streams_plain_resolved_media_and_checks_http_size() {
        let directory = tempdir().expect("temporary directory");
        let destination = directory.path().join("track.mp3");
        let source = Url::parse("https://music.yandex.net/get-mp3/test").expect("fixture URL");

        let result =
            fetch_yandex_music_media_with(&source, None, None, &destination, |requested| {
                assert_eq!(requested, &source);
                Ok(FetchedMedia {
                    reader: Box::new(Cursor::new(PLAINTEXT)),
                    content_length: Some(PLAINTEXT.len() as u64),
                    content_type: None,
                })
            })
            .expect("plain media download");

        assert_eq!(result.bytes_written, PLAINTEXT.len() as u64);
        assert!(!result.decrypted);
        assert_eq!(fs::read(destination).expect("published media"), PLAINTEXT);
    }

    #[test]
    fn fetcher_rejects_a_successful_non_media_response() {
        let directory = tempdir().expect("temporary directory");
        let destination = directory.path().join("track.flac");
        let source = Url::parse("https://music.yandex.net/get-file/test").expect("fixture URL");
        let error = fetch_yandex_music_media_with(&source, None, None, &destination, |_| {
            Ok(FetchedMedia {
                reader: Box::new(Cursor::new(b"<html>expired</html>")),
                content_length: Some(20),
                content_type: Some("text/html; charset=utf-8".to_owned()),
            })
        })
        .expect_err("an HTML error page must not be published as media");

        assert!(matches!(
            error,
            YandexMusicMediaFetchError::UnexpectedContentType
        ));
        assert!(!destination.exists());
    }

    #[test]
    fn fetcher_rejects_octet_stream_data_that_does_not_match_the_codec() {
        let directory = tempdir().expect("temporary directory");
        let destination = directory.path().join("track.flac");
        let source = Url::parse("https://music.yandex.net/get-file/test").expect("fixture URL");
        let body = br#"{"error":"expired"}"#;
        let error = fetch_yandex_music_media_with_codec(
            &source,
            YandexMusicCodec::Flac,
            None,
            Some(body.len() as u64),
            &destination,
            |_| {
                Ok(FetchedMedia {
                    reader: Box::new(Cursor::new(body)),
                    content_length: Some(body.len() as u64),
                    content_type: Some("application/octet-stream".to_owned()),
                })
            },
        )
        .expect_err("a JSON body must not be published as FLAC");

        assert!(matches!(
            error,
            YandexMusicMediaFetchError::Publish(YandexMusicMediaError::InvalidPayload)
        ));
        assert!(!destination.exists());
    }

    #[test]
    fn payload_signatures_cover_every_negotiated_codec_family() {
        let fixtures: &[(YandexMusicCodec, &[u8])] = &[
            (YandexMusicCodec::Flac, b"fLaC\x00\x00\x00\x22"),
            (YandexMusicCodec::FlacMp4, b"\x00\x00\x00\x18ftypM4A "),
            (YandexMusicCodec::Aac, b"\xff\xf1\x50\x80"),
            (YandexMusicCodec::HeAac, b"ADIF\x00\x00"),
            (YandexMusicCodec::AacMp4, b"\x00\x00\x00\x18ftypM4A "),
            (YandexMusicCodec::HeAacMp4, b"\x00\x00\x00\x18ftypM4A "),
            (YandexMusicCodec::Mp3, b"ID3\x04\x00\x00"),
        ];

        for (codec, prefix) in fixtures {
            assert!(
                super::media_payload_matches_codec(*codec, prefix),
                "{codec:?} must accept its canonical payload signature"
            );
            assert!(
                !super::media_payload_matches_codec(*codec, br#"{"error":1}"#),
                "{codec:?} must reject a JSON error body"
            );
        }
    }

    #[test]
    fn raw_aac_and_mp3_frame_headers_are_not_interchangeable() {
        let adts_aac = b"\xff\xf1\x50\x80";
        let mpeg_layer_three = b"\xff\xfb\x90\x64";

        assert!(super::media_payload_matches_codec(
            YandexMusicCodec::Aac,
            adts_aac
        ));
        assert!(!super::media_payload_matches_codec(
            YandexMusicCodec::Mp3,
            adts_aac
        ));
        assert!(super::media_payload_matches_codec(
            YandexMusicCodec::Mp3,
            mpeg_layer_three
        ));
        assert!(!super::media_payload_matches_codec(
            YandexMusicCodec::Aac,
            mpeg_layer_three
        ));
    }

    struct DelayedChunkReader {
        source: Cursor<Vec<u8>>,
        delay: Duration,
        chunk_bytes: usize,
    }

    impl Read for DelayedChunkReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            thread::sleep(self.delay);
            let limit = buffer.len().min(self.chunk_bytes);
            self.source.read(&mut buffer[..limit])
        }
    }

    #[test]
    fn injected_sustained_transfer_can_outlive_the_setup_timeout() {
        let directory = tempdir().expect("temporary directory");
        let destination = directory.path().join("track.flac");
        let source = Url::parse("https://music.yandex.net/get-file/test").expect("fixture URL");
        let body = PLAINTEXT.repeat(2);
        let total = body.len() as u64;
        let setup_timeout = Duration::from_millis(1);
        let mut progress = Vec::new();

        fetch_yandex_music_media_with_progress(
            &source,
            None,
            None,
            Some(total),
            &destination,
            |_| {
                Ok(FetchedMedia {
                    reader: Box::new(DelayedChunkReader {
                        source: Cursor::new(body.clone()),
                        delay: setup_timeout.saturating_mul(2),
                        chunk_bytes: 7,
                    }),
                    content_length: Some(total),
                    content_type: None,
                })
            },
            |observation| progress.push(observation.bytes_written),
        )
        .expect("continuous transfer is not governed by a setup deadline");

        assert_eq!(fs::read(destination).expect("published media"), body);
        assert_eq!(progress.first(), Some(&0));
        assert_eq!(progress.last(), Some(&total));
        assert!(
            progress.len() > 2,
            "fixture must exercise several delayed body reads"
        );
    }

    #[test]
    fn fetcher_decrypts_highest_quality_media_while_streaming() {
        let directory = tempdir().expect("temporary directory");
        let destination = directory.path().join("track.flac");
        let source = Url::parse("https://avatars.mds.yandex.net/get-music-content/test")
            .expect("fixture URL");
        let key = decode_hex("00112233445566778899aabbccddeeff");
        let plaintext = PLAINTEXT.repeat(4_000);
        let mut ciphertext = plaintext.clone();
        YandexMusicMediaCipher::new(&key, 0)
            .expect("cipher")
            .decrypt(&mut ciphertext)
            .expect("encrypt fixture");
        let size = u64::try_from(ciphertext.len()).expect("fixture length fits u64");

        let result =
            fetch_yandex_music_media_with(&source, Some(&key), Some(size), &destination, |_| {
                Ok(FetchedMedia {
                    reader: Box::new(Cursor::new(ciphertext)),
                    content_length: Some(size),
                    content_type: None,
                })
            })
            .expect("encrypted media download");

        assert_eq!(result.bytes_written, size);
        assert!(result.decrypted);
        assert_eq!(fs::read(destination).expect("published media"), plaintext);
    }

    #[test]
    fn progress_reports_zero_and_each_published_staging_chunk() {
        let directory = tempdir().expect("temporary directory");
        let destination = directory.path().join("track.flac");
        let source = Url::parse("https://music.yandex.net/get-file/test").expect("fixture URL");
        let plaintext = vec![7_u8; COPY_BUFFER_BYTES + 3];
        let total = plaintext.len() as u64;
        let mut observations = Vec::new();

        fetch_yandex_music_media_with_progress(
            &source,
            None,
            None,
            Some(total),
            &destination,
            |_| {
                Ok(FetchedMedia {
                    reader: Box::new(Cursor::new(plaintext.clone())),
                    content_length: Some(total),
                    content_type: None,
                })
            },
            |progress| observations.push(progress),
        )
        .expect("media download");

        assert_eq!(
            observations,
            vec![
                YandexMusicDownloadProgress {
                    bytes_written: 0,
                    total_bytes: Some(total),
                },
                YandexMusicDownloadProgress {
                    bytes_written: COPY_BUFFER_BYTES as u64,
                    total_bytes: Some(total),
                },
                YandexMusicDownloadProgress {
                    bytes_written: total,
                    total_bytes: Some(total),
                },
            ]
        );
    }

    #[test]
    fn cancellation_before_fetch_never_opens_the_network_or_destination() {
        let directory = tempdir().expect("temporary directory");
        let destination = directory.path().join("track.flac");
        let source = Url::parse("https://music.yandex.net/get-file/test").expect("fixture URL");
        let cancelled = AtomicBool::new(true);

        let error = fetch_yandex_music_media_with_progress_and_cancellation(
            &source,
            None,
            None,
            None,
            &destination,
            |_| panic!("pre-cancelled download must not make a network request"),
            Some(&cancelled),
            |_| panic!("pre-cancelled download must not report progress"),
        )
        .expect_err("pre-cancelled download");

        assert!(matches!(error, YandexMusicMediaFetchError::Cancelled));
        assert!(!destination.exists());
        assert_eq!(
            fs::read_dir(directory.path()).expect("directory").count(),
            0
        );
    }

    #[test]
    fn cancellation_between_chunks_removes_the_staging_file() {
        let directory = tempdir().expect("temporary directory");
        let destination = directory.path().join("track.flac");
        let source = Url::parse("https://music.yandex.net/get-file/test").expect("fixture URL");
        let body = vec![7_u8; COPY_BUFFER_BYTES * 2];
        let total = body.len() as u64;
        let cancelled = AtomicBool::new(false);
        let mut observations = Vec::new();

        let error = fetch_yandex_music_media_with_progress_and_cancellation(
            &source,
            None,
            None,
            Some(total),
            &destination,
            |_| {
                Ok(FetchedMedia {
                    reader: Box::new(Cursor::new(body)),
                    content_length: Some(total),
                    content_type: None,
                })
            },
            Some(&cancelled),
            |progress| {
                observations.push(progress.bytes_written);
                if progress.bytes_written > 0 {
                    cancelled.store(true, Ordering::Release);
                }
            },
        )
        .expect_err("download cancelled after its first chunk");

        assert!(matches!(error, YandexMusicMediaFetchError::Cancelled));
        assert_eq!(observations, [0, COPY_BUFFER_BYTES as u64]);
        assert!(!destination.exists());
        assert_eq!(
            fs::read_dir(directory.path()).expect("directory").count(),
            0,
            "cooperative cancellation must remove the staging file"
        );
    }

    #[test]
    fn unknown_size_body_cannot_exceed_the_media_limit() {
        let directory = tempdir().expect("temporary directory");
        let destination = directory.path().join("track.flac");
        let limit = 17_u64;
        let error = super::write_yandex_music_media_atomically_with_progress(
            Cursor::new(vec![7_u8; 18]),
            super::MediaPublishPlan {
                destination: &destination,
                decryption_key: None,
                expected_codec: None,
                expected_size: None,
                max_size: limit,
                cancelled: None,
            },
            |_| {},
        )
        .expect_err("unknown-size body above the limit");

        assert!(matches!(
            error,
            YandexMusicMediaError::SizeLimitExceeded { limit: 17 }
        ));
        assert!(!destination.exists());
        assert_eq!(
            fs::read_dir(directory.path()).expect("directory").count(),
            0,
            "an oversized unknown-length body must remove its staging file"
        );
    }

    #[test]
    fn oversized_provider_declaration_is_rejected_before_network_access() {
        let directory = tempdir().expect("temporary directory");
        let destination = directory.path().join("track.flac");
        let source = Url::parse("https://music.yandex.net/get-file/test").expect("fixture URL");
        let declared = super::MAX_YANDEX_MUSIC_MEDIA_BYTES.saturating_add(1);

        let error =
            fetch_yandex_music_media_with(&source, None, Some(declared), &destination, |_| {
                panic!("oversized provider metadata must prevent the network request")
            })
            .expect_err("oversized provider declaration");

        assert!(matches!(
            error,
            YandexMusicMediaFetchError::Publish(YandexMusicMediaError::SizeLimitExceeded { .. })
        ));
        assert!(!destination.exists());
    }

    #[test]
    fn conflicting_api_and_http_sizes_fail_before_destination_creation() {
        let directory = tempdir().expect("temporary directory");
        let destination = directory.path().join("track.flac");
        let source = Url::parse("https://music.yandex.net/get-file/test").expect("fixture URL");

        let error = fetch_yandex_music_media_with(&source, None, Some(1_000), &destination, |_| {
            Ok(FetchedMedia {
                reader: Box::new(Cursor::new(PLAINTEXT)),
                content_length: Some(999),
                content_type: None,
            })
        })
        .expect_err("conflicting sizes must fail");

        assert!(matches!(
            error,
            YandexMusicMediaFetchError::DeclaredSizeMismatch {
                provider: 1_000,
                http: 999
            }
        ));
        assert!(!destination.exists());
        assert_eq!(
            fs::read_dir(directory.path()).expect("directory").count(),
            0
        );
    }

    #[test]
    fn modified_resolved_url_cannot_leave_the_yandex_cdn_boundary() {
        let directory = tempdir().expect("temporary directory");
        let destination = directory.path().join("track.flac");
        let source = Url::parse("https://example.com/signed-media").expect("fixture URL");

        let error = fetch_yandex_music_media_with(&source, None, None, &destination, |_| {
            panic!("invalid source must fail before an HTTP request")
        })
        .expect_err("non-Yandex source must fail");

        assert!(matches!(error, YandexMusicMediaFetchError::InvalidSource));
        assert!(!destination.exists());
    }

    #[test]
    fn fetch_failure_does_not_create_destination() {
        let directory = tempdir().expect("temporary directory");
        let destination = directory.path().join("track.flac");
        let source = Url::parse("https://music.yandex.net/get-file/test").expect("fixture URL");

        let error = fetch_yandex_music_media_with(&source, None, None, &destination, |_| {
            Err(YandexMusicMediaFetchError::Transport(
                "mock connection failed",
            ))
        })
        .expect_err("transport failure");

        assert!(matches!(
            error,
            YandexMusicMediaFetchError::Transport("mock connection failed")
        ));
        assert!(!destination.exists());
        assert_eq!(
            fs::read_dir(directory.path()).expect("directory").count(),
            0
        );
    }

    #[test]
    fn fetcher_never_clobbers_an_existing_destination() {
        let directory = tempdir().expect("temporary directory");
        let destination = directory.path().join("track.mp3");
        fs::write(&destination, b"known-good").expect("existing destination");
        let source = Url::parse("https://music.yandex.net/get-file/test").expect("fixture URL");

        let error = fetch_yandex_music_media_with(
            &source,
            None,
            Some(PLAINTEXT.len() as u64),
            &destination,
            |_| panic!("an existing destination must fail before an HTTP request"),
        )
        .expect_err("existing destination must not be replaced");

        assert!(matches!(
            error,
            YandexMusicMediaFetchError::Publish(YandexMusicMediaError::Io(ref source))
                if source.kind() == io::ErrorKind::AlreadyExists
        ));
        assert_eq!(
            fs::read(destination).expect("existing destination"),
            b"known-good"
        );
        assert_eq!(
            fs::read_dir(directory.path()).expect("directory").count(),
            1,
            "failed staging file must be removed"
        );
    }

    #[test]
    fn existing_destination_is_never_replaced() {
        let directory = tempdir().expect("temporary directory");
        let destination = directory.path().join("track.mp3");
        fs::write(&destination, b"keep me").expect("existing destination");

        let error =
            write_yandex_music_media_atomically(Cursor::new(PLAINTEXT), &destination, None, None)
                .expect_err("no-clobber publication");

        assert!(matches!(
            error,
            YandexMusicMediaError::Io(ref source)
                if source.kind() == io::ErrorKind::AlreadyExists
        ));
        assert_eq!(
            fs::read(&destination).expect("existing destination"),
            b"keep me"
        );
        assert_eq!(
            fs::read_dir(directory.path()).expect("directory").count(),
            1,
            "failed staging file must be removed"
        );
    }

    #[test]
    fn size_mismatch_does_not_publish_partial_media() {
        let directory = tempdir().expect("temporary directory");
        let destination = directory.path().join("track.mp3");

        let error = write_yandex_music_media_atomically(
            Cursor::new(PLAINTEXT),
            &destination,
            None,
            Some((PLAINTEXT.len() + 1) as u64),
        )
        .expect_err("short source must fail");

        assert!(matches!(error, YandexMusicMediaError::SizeMismatch { .. }));
        assert!(!destination.exists());
        assert_eq!(
            fs::read_dir(directory.path()).expect("directory").count(),
            0,
            "failed staging file must be removed"
        );
    }

    struct FailingReader {
        cursor: Cursor<Vec<u8>>,
        successful_reads: usize,
    }

    impl Read for FailingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.successful_reads == 0 {
                self.successful_reads += 1;
                let read_limit = buffer.len().min(8);
                return self.cursor.read(&mut buffer[..read_limit]);
            }
            Err(io::Error::other("mock upstream failed"))
        }
    }

    #[test]
    fn source_failure_removes_staging_and_preserves_existing_destination() {
        let directory = tempdir().expect("temporary directory");
        let destination = directory.path().join("track.mp3");
        fs::write(&destination, b"known-good").expect("existing destination");
        let source = FailingReader {
            cursor: Cursor::new(PLAINTEXT.to_vec()),
            successful_reads: 0,
        };

        let error = write_yandex_music_media_atomically(source, &destination, None, None)
            .expect_err("source failure");

        assert!(matches!(error, YandexMusicMediaError::Io(_)));
        assert_eq!(
            fs::read(&destination).expect("existing destination"),
            b"known-good"
        );
        assert_eq!(
            fs::read_dir(directory.path()).expect("directory").count(),
            1,
            "failed staging file must be removed"
        );
    }
}
