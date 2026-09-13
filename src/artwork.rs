//! Artwork bytes and their private on-disk cache.
//!
//! This is the half of Youta's artwork pipeline that no renderer needs: it
//! turns a provider URL into bytes, guarding the request, and keeps those bytes
//! in a confined cache directory. Decoding, terminal graphics protocols, and
//! `ratatui-image` live in [`crate::thumbnails`], which is why that module
//! requires a terminal and this one does not.
//!
//! Every network protection Youta documents lives here rather than in a
//! front-end: connections are pinned to public addresses, redirects are not
//! followed to a different safety class, responses are size-capped, and cache
//! files are private, hashed, and written atomically. A window that rendered
//! artwork by fetching it itself would lose all of that.
//!
//! The cache and the format sniffer are shared with `crate::local_artwork`,
//! which finds covers inside and beside local media files. That half needs no
//! network, so everything that reaches one is behind `remote-artwork` and a
//! text-only local build links no HTTP client.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};
#[cfg(feature = "remote-artwork")]
use std::time::Instant;
#[cfg(feature = "remote-artwork")]
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
#[cfg(feature = "remote-artwork")]
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};
use url::Url;

#[cfg(feature = "remote-artwork")]
use crate::domain::{ip_address_is_non_public, remote_url_has_non_public_host};

/// Largest artwork response accepted from a provider.
pub(crate) const MAX_DOWNLOAD_BYTES: usize = 4 * 1024 * 1024;
/// Bounded wait for one artwork request.
#[cfg(feature = "remote-artwork")]
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum explicitly validated CDN redirects for one Archive.org image.
#[cfg(feature = "remote-artwork")]
const MAX_ARCHIVE_ARTWORK_REDIRECTS: usize = 3;
/// Bound for a redirect target before parsing or following its Location value.
#[cfg(feature = "remote-artwork")]
const MAX_ARTWORK_REDIRECT_URL_BYTES: usize = 4 * 1024;
/// Age after which a cached entry is discarded.
const CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Byte budget for the whole cache directory.
const CACHE_MAX_BYTES: u64 = 64 * 1024 * 1024;
// Retain a complete maximum search prefetch unless the independent 64 MiB
// budget requires byte-based eviction.
/// Entry-count budget for the cache directory.
const CACHE_MAX_ENTRIES: usize = 512;
/// Extension given to every cache entry.
const CACHE_FILE_EXTENSION: &str = "image";

/// Distinguishes concurrent atomic writes inside one process.
static CACHE_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// Temporary files an eviction pass must not delete out from under a writer.
static ACTIVE_CACHE_TEMPORARIES: LazyLock<Mutex<HashSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Safe, URL-free reason why selected artwork could not be displayed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThumbnailFailure {
    /// The provider returned an unsupported or unsafe URL.
    InvalidSource,
    /// The image could not be downloaded before the bounded timeout.
    DownloadFailed,
    /// The response exceeded Youta's thumbnail byte limit.
    ResponseTooLarge,
    /// The response was not JPEG, PNG, or WebP.
    UnsupportedFormat,
    /// The image was malformed or exceeded decode limits.
    InvalidImage,
    /// FFmpeg could not extract a representative local-video frame.
    LocalVideoFrameExtractionFailed,
    /// The terminal protocol encoder rejected the image.
    EncodingFailed,
    /// The background thumbnail worker stopped unexpectedly.
    WorkerStopped,
}

impl std::fmt::Display for ThumbnailFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidSource => "thumbnail source is not HTTP or HTTPS",
            Self::DownloadFailed => "thumbnail download failed",
            Self::ResponseTooLarge => "thumbnail exceeds the 4 MiB download limit",
            Self::UnsupportedFormat => "thumbnail is not JPEG, PNG, or WebP",
            Self::InvalidImage => "thumbnail is invalid or exceeds decode limits",
            Self::LocalVideoFrameExtractionFailed => "video frame extraction failed",
            Self::EncodingFailed => "terminal thumbnail encoding failed",
            Self::WorkerStopped => "thumbnail worker stopped",
        };
        formatter.write_str(message)
    }
}

/// Indirection the terminal thumbnail workers are tested through.
///
/// Only [`crate::thumbnails`] fetches on a worker thread, so the abstraction —
/// and the HTTP implementation of it below — belongs to `images` rather than to
/// every build that can reach the network for artwork.
#[cfg(feature = "images")]
pub(crate) trait ThumbnailTransport: Send + 'static {
    fn fetch(&mut self, source: &Url) -> Result<Vec<u8>, ThumbnailFailure>;
}

/// Builds the agent every artwork request uses.
///
/// Automatic redirects stay disabled. Archive.org CDN redirects are checked
/// explicitly by the fetcher, and every new connection still uses the public
/// address resolver so a redirect cannot reach a private destination.
#[cfg(feature = "remote-artwork")]
pub(crate) fn thumbnail_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .max_redirects(0)
        .http_status_as_error(false)
        .user_agent(concat!(
            "youta/",
            env!("CARGO_PKG_VERSION"),
            " (+",
            env!("CARGO_PKG_REPOSITORY"),
            ")"
        ))
        .build();
    ureq::Agent::with_parts(
        config,
        DefaultConnector::default(),
        PublicThumbnailResolver::default(),
    )
}

/// Agent used by tests that must reach a loopback mock server.
///
/// It keeps every other guard — no redirects, bounded timeout — and relaxes
/// only the public-address pin, which no production path can do.
#[cfg(all(test, feature = "remote-artwork"))]
pub(crate) fn mock_thumbnail_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .max_redirects(0)
        .http_status_as_error(false)
        .build();
    ureq::Agent::with_parts(
        config,
        DefaultConnector::default(),
        PublicThumbnailResolver {
            resolver: DefaultResolver::default(),
            allow_non_public: true,
        },
    )
}

#[cfg(feature = "images")]
pub(crate) struct HttpThumbnailTransport {
    agent: ureq::Agent,
}

/// DNS resolver that pins thumbnail connections to public addresses only.
#[cfg(feature = "remote-artwork")]
#[derive(Debug, Default)]
pub(crate) struct PublicThumbnailResolver {
    resolver: DefaultResolver,
    #[cfg(test)]
    allow_non_public: bool,
}

#[cfg(feature = "remote-artwork")]
impl Resolver for PublicThumbnailResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let resolved = self.resolver.resolve(uri, config, timeout)?;
        #[cfg(test)]
        if self.allow_non_public {
            return Ok(resolved);
        }
        let mut public = self.empty();
        for address in &resolved {
            if !ip_address_is_non_public(address.ip()) {
                public.push(*address);
            }
        }
        if public.is_empty() {
            Err(ureq::Error::HostNotFound)
        } else {
            Ok(public)
        }
    }
}

#[cfg(feature = "images")]
impl HttpThumbnailTransport {
    /// Builds a transport over the guarded agent.
    pub(crate) fn new() -> Self {
        Self {
            agent: thumbnail_agent(),
        }
    }
}

#[cfg(feature = "images")]
impl ThumbnailTransport for HttpThumbnailTransport {
    fn fetch(&mut self, source: &Url) -> Result<Vec<u8>, ThumbnailFailure> {
        fetch_thumbnail(&self.agent, source)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ThumbnailCachePolicy {
    pub(crate) max_age: Duration,
    pub(crate) max_bytes: u64,
    pub(crate) max_entries: usize,
}

impl Default for ThumbnailCachePolicy {
    fn default() -> Self {
        Self {
            max_age: CACHE_MAX_AGE,
            max_bytes: CACHE_MAX_BYTES,
            max_entries: CACHE_MAX_ENTRIES,
        }
    }
}

pub(crate) struct ThumbnailCache {
    pub(crate) directory: PathBuf,
    policy: ThumbnailCachePolicy,
}

impl ThumbnailCache {
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            policy: ThumbnailCachePolicy::default(),
        }
    }

    #[cfg(all(test, feature = "remote-artwork"))]
    pub(crate) fn with_policy(directory: PathBuf, policy: ThumbnailCachePolicy) -> Self {
        Self { directory, policy }
    }

    /// Reads the entry a remote source is cached under.
    #[cfg(feature = "remote-artwork")]
    pub(crate) fn read(&self, source: &Url) -> io::Result<Option<Vec<u8>>> {
        self.read_key(source.as_str().as_bytes())
    }

    pub(crate) fn read_key(&self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        if !self.directory.exists() {
            return Ok(None);
        }
        self.secure_directory()?;
        let path = self.entry_path_for_key(key);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_file()
            || metadata.len() == 0
            || metadata.len() > MAX_DOWNLOAD_BYTES as u64
            || self.is_expired(&metadata)
        {
            remove_cache_entry(&path);
            return Ok(None);
        }
        if !self.is_confined_entry(&path)? {
            remove_cache_entry(&path);
            return Ok(None);
        }

        let file = fs::File::open(&path)?;
        let mut bytes = Vec::with_capacity(
            usize::try_from(metadata.len())
                .unwrap_or(MAX_DOWNLOAD_BYTES)
                .min(MAX_DOWNLOAD_BYTES),
        );
        file.take(u64::try_from(MAX_DOWNLOAD_BYTES + 1).unwrap_or(u64::MAX))
            .read_to_end(&mut bytes)?;
        if bytes.is_empty() || bytes.len() > MAX_DOWNLOAD_BYTES {
            remove_cache_entry(&path);
            return Ok(None);
        }
        Ok(Some(bytes))
    }

    #[cfg(feature = "remote-artwork")]
    pub(crate) fn prepare(&self) -> io::Result<()> {
        self.secure_directory()?;
        self.evict()
    }

    #[cfg(feature = "remote-artwork")]
    pub(crate) fn store(&self, source: &Url, bytes: &[u8]) -> io::Result<()> {
        self.store_key(source.as_str().as_bytes(), bytes)
    }

    pub(crate) fn store_key(&self, key: &[u8], bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() || bytes.len() > MAX_DOWNLOAD_BYTES {
            return Ok(());
        }
        self.secure_directory()?;
        let path = self.entry_path_for_key(key);
        self.write_atomic(&path, bytes)?;
        self.evict()
    }

    #[cfg(feature = "images")]
    pub(crate) fn remove(&self, source: &Url) {
        self.remove_key(source.as_str().as_bytes());
    }

    pub(crate) fn remove_key(&self, key: &[u8]) {
        remove_cache_entry(&self.entry_path_for_key(key));
    }

    #[cfg(all(test, feature = "remote-artwork"))]
    pub(crate) fn entry_path(&self, source: &Url) -> PathBuf {
        self.entry_path_for_key(source.as_str().as_bytes())
    }

    pub(crate) fn entry_path_for_key(&self, key: &[u8]) -> PathBuf {
        let digest = Sha256::digest(key);
        self.directory
            .join(format!("{digest:x}.{CACHE_FILE_EXTENSION}"))
    }

    fn secure_directory(&self) -> io::Result<()> {
        fs::create_dir_all(&self.directory)?;
        let metadata = fs::symlink_metadata(&self.directory)?;
        if !metadata.file_type().is_dir() {
            return Err(io::Error::other("thumbnail cache path is not a directory"));
        }
        set_private_directory_permissions(&self.directory)
    }

    fn is_confined_entry(&self, path: &Path) -> io::Result<bool> {
        let directory = crate::fs_path::canonicalize(&self.directory)?;
        let entry = crate::fs_path::canonicalize(path)?;
        Ok(entry.parent() == Some(directory.as_path()))
    }

    fn is_expired(&self, metadata: &fs::Metadata) -> bool {
        metadata
            .modified()
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age > self.policy.max_age)
    }

    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let sequence = CACHE_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = self.directory.join(format!(
            ".thumbnail.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        let active_temporary = ActiveCacheTemporary::register(temporary.clone());
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            let mut file = crate::private_files::open_privately(&mut options).open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            set_private_file_permissions(&temporary)?;
            fs::rename(&temporary, path)?;
            set_private_file_permissions(path)?;
            let _ = crate::durability::sync_directory(&self.directory);
            Ok(())
        })();
        drop(active_temporary);
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub(crate) fn evict(&self) -> io::Result<()> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            if is_cache_temp_name(&entry.file_name()) {
                let path = entry.path();
                if cache_temporary_is_active(&path) {
                    continue;
                }
                if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_file())
                {
                    remove_cache_entry(&path);
                }
                continue;
            }
            if !is_cache_entry_name(&entry.file_name()) {
                continue;
            }
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if !metadata.file_type().is_file()
                || metadata.len() == 0
                || metadata.len() > MAX_DOWNLOAD_BYTES as u64
                || self.is_expired(&metadata)
            {
                remove_cache_entry(&path);
                continue;
            }
            entries.push(CacheEntry {
                path,
                modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                bytes: metadata.len(),
            });
        }

        entries.sort_by(|left, right| {
            left.modified
                .cmp(&right.modified)
                .then_with(|| left.path.cmp(&right.path))
        });
        let mut total_bytes = entries.iter().map(|entry| entry.bytes).sum::<u64>();
        let mut total_entries = entries.len();
        for entry in entries {
            if total_entries <= self.policy.max_entries && total_bytes <= self.policy.max_bytes {
                break;
            }
            if fs::remove_file(&entry.path).is_ok() {
                total_entries = total_entries.saturating_sub(1);
                total_bytes = total_bytes.saturating_sub(entry.bytes);
            }
        }
        Ok(())
    }
}

/// Registration preventing concurrent cache eviction from deleting a live
/// atomic-write temporary.
pub(crate) struct ActiveCacheTemporary {
    path: PathBuf,
}

impl ActiveCacheTemporary {
    pub(crate) fn register(path: PathBuf) -> Self {
        ACTIVE_CACHE_TEMPORARIES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(path.clone());
        Self { path }
    }
}

impl Drop for ActiveCacheTemporary {
    fn drop(&mut self) {
        ACTIVE_CACHE_TEMPORARIES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.path);
    }
}

pub(crate) fn cache_temporary_is_active(path: &Path) -> bool {
    ACTIVE_CACHE_TEMPORARIES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(path)
}

pub(crate) struct CacheEntry {
    path: PathBuf,
    modified: SystemTime,
    bytes: u64,
}

pub(crate) fn is_cache_entry_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(digest) = name.strip_suffix(&format!(".{CACHE_FILE_EXTENSION}")) else {
        return false;
    };
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub(crate) fn is_cache_temp_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(sequence) = name
        .strip_prefix(".thumbnail.")
        .and_then(|name| name.strip_suffix(".tmp"))
    else {
        return false;
    };
    let mut components = sequence.split('.');
    matches!(
        (components.next(), components.next(), components.next()),
        (Some(process), Some(sequence), None)
            if !process.is_empty()
                && !sequence.is_empty()
                && process.bytes().all(|byte| byte.is_ascii_digit())
                && sequence.bytes().all(|byte| byte.is_ascii_digit())
    )
}

pub(crate) fn remove_cache_entry(path: &Path) {
    let _ = fs::remove_file(path);
}

pub(crate) use crate::private_files::{
    set_private_directory_permissions, set_private_file_permissions,
};

#[cfg(feature = "remote-artwork")]
pub(crate) fn fetch_thumbnail(
    agent: &ureq::Agent,
    source: &Url,
) -> Result<Vec<u8>, ThumbnailFailure> {
    fetch_thumbnail_with_policy(agent, source, false)
}

/// Fetches one thumbnail, optionally allowing a loopback test fixture.
#[cfg(feature = "remote-artwork")]
pub(crate) fn fetch_thumbnail_with_policy(
    agent: &ureq::Agent,
    source: &Url,
    allow_non_public_test_source: bool,
) -> Result<Vec<u8>, ThumbnailFailure> {
    if source.scheme() == "file" {
        let path = source
            .to_file_path()
            .map_err(|()| ThumbnailFailure::InvalidSource)?;
        let metadata = fs::symlink_metadata(&path).map_err(|_| ThumbnailFailure::DownloadFailed)?;
        if !metadata.file_type().is_file() {
            return Err(ThumbnailFailure::InvalidSource);
        }
        if metadata.len() > MAX_DOWNLOAD_BYTES as u64 {
            return Err(ThumbnailFailure::ResponseTooLarge);
        }
        return fs::read(path).map_err(|_| ThumbnailFailure::DownloadFailed);
    }
    if !is_safe_remote_thumbnail_source(source, allow_non_public_test_source) {
        return Err(ThumbnailFailure::InvalidSource);
    }
    fetch_remote_thumbnail_with_fallback(agent, source, Instant::now(), REQUEST_TIMEOUT)
}

/// Gives the full image four fifths of one deadline, reserving time for its tile.
///
/// Only the exact Archive waveform route enables fallback. Invalid redirect
/// targets fail closed; transport, status, byte-limit, and format failures can
/// use the canonical item tile without resetting the redirect or time budget.
#[cfg(feature = "remote-artwork")]
fn fetch_remote_thumbnail_with_fallback(
    agent: &ureq::Agent,
    source: &Url,
    started: Instant,
    timeout: Duration,
) -> Result<Vec<u8>, ThumbnailFailure> {
    let deadline = started
        .checked_add(timeout)
        .ok_or(ThumbnailFailure::DownloadFailed)?;
    let fallback = archive_iiif_thumbnail_fallback(source);
    let primary_deadline = if fallback.is_some() {
        started + timeout.saturating_sub(timeout / 5)
    } else {
        deadline
    };
    let mut redirects = 0;
    let result = fetch_remote_thumbnail(agent, source, primary_deadline, &mut redirects);
    let Some(fallback) = fallback else {
        return result;
    };
    match result {
        Ok(bytes) if ArtworkFormat::sniff(&bytes).is_some() => Ok(bytes),
        Err(ThumbnailFailure::InvalidSource) => Err(ThumbnailFailure::InvalidSource),
        _ => {
            let bytes = fetch_remote_thumbnail(agent, &fallback, deadline, &mut redirects)?;
            if ArtworkFormat::sniff(&bytes).is_some() {
                Ok(bytes)
            } else {
                Err(ThumbnailFailure::UnsupportedFormat)
            }
        }
    }
}

/// Fetches one remote image with an operation-wide deadline and redirect count.
#[cfg(feature = "remote-artwork")]
fn fetch_remote_thumbnail(
    agent: &ureq::Agent,
    source: &Url,
    deadline: Instant,
    redirects: &mut usize,
) -> Result<Vec<u8>, ThumbnailFailure> {
    let mut current = source.clone();
    let mut response = loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(ThumbnailFailure::DownloadFailed)?;
        // Disabling redirects per request guarantees the URL validated before
        // I/O is the one requested, even if a caller supplies a different agent.
        let response = agent
            .get(current.as_str())
            .header("Accept", "image/jpeg, image/png, image/webp")
            .config()
            .max_redirects(0)
            .timeout_global(Some(remaining))
            .build()
            .call()
            .map_err(|_| ThumbnailFailure::DownloadFailed)?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            break response;
        }
        if !matches!(status, 301 | 302 | 303 | 307 | 308)
            || *redirects == MAX_ARCHIVE_ARTWORK_REDIRECTS
            || !is_archive_artwork_redirect_url(&current)
        {
            return Err(ThumbnailFailure::DownloadFailed);
        }
        let location = response
            .headers()
            .get("Location")
            .and_then(|value| value.to_str().ok())
            .ok_or(ThumbnailFailure::DownloadFailed)?;
        if location.len() > MAX_ARTWORK_REDIRECT_URL_BYTES {
            return Err(ThumbnailFailure::InvalidSource);
        }
        let target = current
            .join(location)
            .map_err(|_| ThumbnailFailure::InvalidSource)?;
        if !is_archive_artwork_redirect_url(&target) {
            return Err(ThumbnailFailure::InvalidSource);
        }
        current = target;
        *redirects += 1;
    };
    if response
        .body()
        .content_length()
        .is_some_and(|length| length > MAX_DOWNLOAD_BYTES as u64)
    {
        return Err(ThumbnailFailure::ResponseTooLarge);
    }
    let bytes = response
        .body_mut()
        .with_config()
        .limit(u64::try_from(MAX_DOWNLOAD_BYTES.saturating_add(1)).unwrap_or(u64::MAX))
        .read_to_vec()
        .map_err(|error| match error {
            ureq::Error::BodyExceedsLimit(_) => ThumbnailFailure::ResponseTooLarge,
            _ => ThumbnailFailure::DownloadFailed,
        })?;
    if Instant::now() >= deadline {
        Err(ThumbnailFailure::DownloadFailed)
    } else if bytes.len() > MAX_DOWNLOAD_BYTES {
        Err(ThumbnailFailure::ResponseTooLarge)
    } else {
        Ok(bytes)
    }
}

/// Derives a tile only from the canonical, single-encoded PNG waveform route.
///
/// The decoded identifier is bounded ASCII and never a remote URL. Filenames
/// reject traversal, controls, ambiguous separators, and further percent
/// escapes before the entire route is re-encoded and compared exactly.
#[cfg(feature = "remote-artwork")]
fn archive_iiif_thumbnail_fallback(source: &Url) -> Option<Url> {
    if source.host_str() != Some("iiif.archive.org") || !is_archive_artwork_redirect_url(source) {
        return None;
    }
    let encoded = source
        .path()
        .strip_prefix("/image/iiif/3/")?
        .strip_suffix("/full/max/0/default.jpg")?;
    if encoded.contains('/') {
        return None;
    }
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut bytes = encoded.bytes();
    while let Some(byte) = bytes.next() {
        decoded.push(if byte == b'%' {
            let high = char::from(bytes.next()?).to_digit(16)?;
            let low = char::from(bytes.next()?).to_digit(16)?;
            u8::try_from(high * 16 + low).ok()?
        } else {
            byte
        });
    }
    let decoded = String::from_utf8(decoded).ok()?;
    let (identifier, filename) = decoded.split_once('/')?;
    if identifier.is_empty()
        || identifier.len() > 100
        || !(identifier.as_bytes()[0].is_ascii_alphanumeric() || identifier.as_bytes()[0] == b'@')
        || !identifier
            .bytes()
            .skip(1)
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        || filename.is_empty()
        || filename.len() > 2048
        || filename.contains(['\\', '%'])
        || filename.chars().any(char::is_control)
        || filename.split('/').count() > 32
        || filename
            .split('/')
            .any(|part| matches!(part, "" | "." | ".."))
        || !filename.rsplit_once('.')?.1.eq_ignore_ascii_case("png")
    {
        return None;
    }
    let mut canonical = Url::parse("https://iiif.archive.org/").ok()?;
    canonical.path_segments_mut().ok()?.clear().extend([
        "image",
        "iiif",
        "3",
        &decoded,
        "full",
        "max",
        "0",
        "default.jpg",
    ]);
    if canonical != *source {
        return None;
    }
    let mut fallback = Url::parse("https://archive.org/").ok()?;
    fallback
        .path_segments_mut()
        .ok()?
        .clear()
        .extend(["services", "img", identifier]);
    Some(fallback)
}
/// Admits only credential-free HTTPS URLs on Internet Archive's own domains.
///
/// Covers commonly redirect from archive.org/download to a geographic CDN
/// subdomain. Dot-delimited suffix matching rejects lookalike hosts; query,
/// fragment, and non-default-port targets are deliberately not followed.
/// The agent independently resolves and pins each connection to public IPs.
#[cfg(feature = "remote-artwork")]
fn is_archive_artwork_redirect_url(source: &Url) -> bool {
    source.as_str().len() <= MAX_ARTWORK_REDIRECT_URL_BYTES
        && source.scheme() == "https"
        && source.port().is_none()
        && source.query().is_none()
        && source.fragment().is_none()
        && is_safe_remote_thumbnail_source(source, false)
        && source
            .host_str()
            .is_some_and(|host| host == "archive.org" || host.ends_with(".archive.org"))
}

#[cfg(feature = "images")]
pub(crate) fn is_safe_thumbnail_source(source: &Url) -> bool {
    is_safe_remote_thumbnail_source(source, false)
        || (source.scheme() == "file" && source.to_file_path().is_ok())
}

#[cfg(feature = "remote-artwork")]
pub(crate) fn is_safe_remote_thumbnail_source(
    source: &Url,
    allow_non_public_test_source: bool,
) -> bool {
    matches!(source.scheme(), "http" | "https")
        && source.username().is_empty()
        && source.password().is_none()
        && source.host_str().is_some()
        && (allow_non_public_test_source || !remote_url_has_non_public_host(source))
}

/// Image formats an out-of-process front-end is allowed to receive.
///
/// The type is decided by the bytes, never by the URL or a provider header:
/// a front-end that trusted a claimed type could be told to interpret an
/// image response as something else.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtworkFormat {
    /// PNG.
    Png,
    /// JPEG.
    Jpeg,
    /// WebP.
    WebP,
}

impl ArtworkFormat {
    /// Returns the MIME type to serve these bytes as.
    #[must_use]
    pub const fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::WebP => "image/webp",
        }
    }

    /// Identifies the format from its leading bytes, or rejects it.
    #[must_use]
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Self::Png)
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(Self::Jpeg)
        } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
            Some(Self::WebP)
        } else {
            None
        }
    }
}

/// One piece of artwork, ready to hand to a front-end.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Artwork {
    /// Encoded image bytes, exactly as cached.
    pub bytes: Vec<u8>,
    /// Format determined from those bytes.
    pub format: ArtworkFormat,
}

/// Returns remote artwork for `source`, from the private cache or the network.
///
/// This is the whole surface an out-of-process front-end needs, and it is
/// deliberately narrow. Every protection stays on this side of the boundary:
/// only public `http`/`https` origins are accepted, redirects are refused except
/// for bounded Archive.org CDN hops, responses are size-capped, and the bytes
/// must be an image stored in the confined private cache. A window
/// that fetched artwork itself would keep none of that, and would additionally
/// hand a provider a request from the user's browser stack.
///
/// `file:` sources are rejected here even though the terminal pipeline accepts
/// them, because this entry point is reachable from a web view and must never
/// become a way to read the filesystem.
///
/// # Errors
///
/// Returns why the artwork is unavailable, without echoing the URL.
#[cfg(feature = "remote-artwork")]
pub fn remote_artwork(cache_directory: &Path, source: &Url) -> Result<Artwork, ThumbnailFailure> {
    if !is_safe_remote_thumbnail_source(source, false) {
        return Err(ThumbnailFailure::InvalidSource);
    }

    let cache = ThumbnailCache::new(cache_directory.to_path_buf());
    if let Ok(Some(bytes)) = cache.read(source)
        && let Some(format) = ArtworkFormat::sniff(&bytes)
    {
        return Ok(Artwork { bytes, format });
    }

    let bytes = fetch_thumbnail(&thumbnail_agent(), source)?;
    let Some(format) = ArtworkFormat::sniff(&bytes) else {
        // A response that is not an image is a failure, not something to cache
        // and certainly not something to pass to a renderer.
        return Err(ThumbnailFailure::UnsupportedFormat);
    };
    // A cache write failure only costs a refetch later.
    let _ = cache.prepare().and_then(|()| cache.store(source, &bytes));
    Ok(Artwork { bytes, format })
}

/// Returns artwork Youta itself discovered in or beside a local media file.
///
/// This is the local counterpart of [`remote_artwork`], and it exists because
/// a window cannot read the file with an ordinary `<img src>`: local covers are
/// either an opaque entry in Youta's private cache or a sidecar image next to
/// the user's media, and neither is a URL a web view may be handed.
///
/// The read is bounded exactly as a download is — a regular file, never a
/// symlink, never larger than the artwork limit — and the media type comes from
/// the leading bytes rather than the file extension.
///
/// SECURITY: this deliberately performs no confinement check, because there is
/// no path pattern that distinguishes the user's own `cover.jpg` from any other
/// file. The caller must therefore accept only a URL the reducer published in a
/// view it rendered — see `youta-gui`'s `PublishedArtwork` — so that a string
/// arriving from a provider can never select the file. A caller that cannot
/// prove that must use [`remote_artwork`], which refuses `file:` outright.
///
/// # Errors
///
/// Returns why the artwork is unavailable, without echoing the path.
#[cfg(feature = "local-artwork")]
pub fn local_artwork(source: &Url) -> Result<Artwork, ThumbnailFailure> {
    if source.scheme() != "file" {
        return Err(ThumbnailFailure::InvalidSource);
    }
    let path = source
        .to_file_path()
        .map_err(|()| ThumbnailFailure::InvalidSource)?;
    let metadata = fs::symlink_metadata(&path).map_err(|_| ThumbnailFailure::DownloadFailed)?;
    if !metadata.file_type().is_file() {
        return Err(ThumbnailFailure::InvalidSource);
    }
    if metadata.len() > MAX_DOWNLOAD_BYTES as u64 {
        return Err(ThumbnailFailure::ResponseTooLarge);
    }
    let bytes = fs::read(&path).map_err(|_| ThumbnailFailure::DownloadFailed)?;
    if bytes.len() > MAX_DOWNLOAD_BYTES {
        return Err(ThumbnailFailure::ResponseTooLarge);
    }
    let format = ArtworkFormat::sniff(&bytes).ok_or(ThumbnailFailure::UnsupportedFormat)?;
    Ok(Artwork { bytes, format })
}

#[cfg(all(test, feature = "local-artwork"))]
mod local_surface_tests {
    use super::{ArtworkFormat, ThumbnailFailure, local_artwork};

    use url::Url;

    /// The type is decided by the bytes here too: a sidecar written by another
    /// program can carry any extension at all.
    #[test]
    fn a_sidecar_is_served_by_its_leading_bytes_and_not_its_extension() {
        let directory = tempfile::tempdir().expect("temporary artwork directory");
        let path = directory.path().join("cover.jpg");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\nbody").expect("write mislabelled sidecar");
        let url = Url::from_file_path(&path).expect("absolute sidecar URL");

        let artwork = local_artwork(&url).expect("serve mislabelled sidecar");
        assert_eq!(artwork.format, ArtworkFormat::Png);
    }

    /// Everything that is not one regular image file is refused identically.
    #[test]
    fn directories_missing_files_and_non_images_are_refused() {
        let directory = tempfile::tempdir().expect("temporary artwork directory");
        let text = directory.path().join("notes.txt");
        std::fs::write(&text, b"<!doctype html>").expect("write non-image file");

        for (url, expected) in [
            (
                Url::from_directory_path(directory.path()).expect("directory URL"),
                ThumbnailFailure::InvalidSource,
            ),
            (
                Url::from_file_path(directory.path().join("absent.png")).expect("absent URL"),
                ThumbnailFailure::DownloadFailed,
            ),
            (
                Url::from_file_path(&text).expect("text URL"),
                ThumbnailFailure::UnsupportedFormat,
            ),
            (
                Url::parse("https://images.example/cover.png").expect("remote URL"),
                ThumbnailFailure::InvalidSource,
            ),
        ] {
            assert_eq!(local_artwork(&url), Err(expected), "{url}");
        }
    }

    /// A symlink is never followed, so a cover cannot stand in for another file.
    #[cfg(unix)]
    #[test]
    fn a_symlink_is_refused_even_when_its_target_is_an_image() {
        let directory = tempfile::tempdir().expect("temporary artwork directory");
        let target = directory.path().join("real.png");
        let link = directory.path().join("cover.png");
        std::fs::write(&target, b"\x89PNG\r\n\x1a\nbody").expect("write symlink target");
        std::os::unix::fs::symlink(&target, &link).expect("create artwork symlink");

        assert_eq!(
            local_artwork(&Url::from_file_path(&link).expect("symlink URL")),
            Err(ThumbnailFailure::InvalidSource)
        );
    }
}

#[cfg(all(test, feature = "remote-artwork"))]
mod public_surface_tests {
    use super::{ArtworkFormat, ThumbnailFailure, remote_artwork};

    use std::path::Path;
    use url::Url;

    /// Replays exact HTTPS requests in memory without relaxing production DNS rules.
    fn scripted_thumbnail_agent(
        responses: Vec<(&str, u16, Option<&str>, Vec<u8>)>,
    ) -> (ureq::Agent, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::collections::VecDeque;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        let responses = Mutex::new(
            responses
                .into_iter()
                .map(|(url, status, location, body)| {
                    (url.to_owned(), status, location.map(str::to_owned), body)
                })
                .collect::<VecDeque<_>>(),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let agent = ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .middleware(
                move |request: ureq::http::Request<ureq::SendBody>,
                      _next: ureq::middleware::MiddlewareNext| {
                    observed.fetch_add(1, Ordering::Relaxed);
                    let (url, status, location, body) = responses
                        .lock()
                        .expect("response fixture")
                        .pop_front()
                        .expect("unexpected request");
                    assert_eq!(request.uri().to_string(), url);
                    let mut response = ureq::http::Response::builder().status(status);
                    if let Some(location) = location {
                        response = response.header("Location", location);
                    }
                    Ok(response
                        .body(ureq::Body::builder().data(body))
                        .expect("mock response"))
                },
            )
            .build()
            .into();
        (agent, calls)
    }

    /// A failed full-resolution waveform uses the same item's bounded image tile.
    #[test]
    fn archive_iiif_waveform_failures_fall_back_to_the_item_tile() {
        let source = Url::parse("https://iiif.archive.org/image/iiif/3/public_book%2Fchapter.png/full/max/0/default.jpg").expect("waveform URL");
        let fallback = "https://archive.org/services/img/public_book";
        let image = b"\xFF\xD8\xFFtile";
        for (status, body) in [
            (404, Vec::new()),
            (503, Vec::new()),
            (200, b"<!doctype html>not an image".to_vec()),
            (200, vec![0; super::MAX_DOWNLOAD_BYTES + 1]),
        ] {
            let (agent, calls) = scripted_thumbnail_agent(vec![
                (source.as_str(), status, None, body),
                (fallback, 200, None, image.to_vec()),
            ]);
            assert_eq!(super::fetch_thumbnail(&agent, &source), Ok(image.to_vec()));
            assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
        }
    }

    /// Successful full-resolution images are not replaced or fetched twice.
    #[test]
    fn archive_iiif_waveform_success_does_not_request_a_fallback() {
        let source = Url::parse("https://iiif.archive.org/image/iiif/3/public_book%2Fchapter.png/full/max/0/default.jpg").expect("waveform URL");
        let image = b"\xFF\xD8\xFFwaveform";
        let (agent, calls) =
            scripted_thumbnail_agent(vec![(source.as_str(), 200, None, image.to_vec())]);
        assert_eq!(super::fetch_thumbnail(&agent, &source), Ok(image.to_vec()));
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    /// URL lookalikes, other IIIF modes, and ambiguous paths cannot enable fallback.
    #[test]
    fn archive_iiif_waveform_fallback_requires_the_exact_safe_route() {
        for raw in [
            "http://iiif.archive.org/image/iiif/3/book%2Fa.png/full/max/0/default.jpg",
            "https://iiif.archive.org.evil.test/image/iiif/3/book%2Fa.png/full/max/0/default.jpg",
            "https://archive.org/image/iiif/3/book%2Fa.png/full/max/0/default.jpg",
            "https://iiif.archive.org:8443/image/iiif/3/book%2Fa.png/full/max/0/default.jpg",
            "https://user:password@iiif.archive.org/image/iiif/3/book%2Fa.png/full/max/0/default.jpg",
            "https://iiif.archive.org/image/iiif/3/book%2Fa.png/full/max/0/default.jpg?download=1",
            "https://iiif.archive.org/image/iiif/3/book%2Fa.png/full/max/0/default.jpg#image",
            "https://iiif.archive.org/image/iiif/2/book%2Fa.png/full/max/0/default.jpg",
            "https://iiif.archive.org/image/iiif/3/book%2Fa.png/full/!1024,1024/0/default.jpg",
            "https://iiif.archive.org/image/iiif/3/book/a.png/full/max/0/default.jpg",
            "https://iiif.archive.org/image/iiif/3/book%2F..%2Fa.png/full/max/0/default.jpg",
            "https://iiif.archive.org/image/iiif/3/book%2F.%2Fa.png/full/max/0/default.jpg",
            "https://iiif.archive.org/image/iiif/3/book%2F%2Fa.png/full/max/0/default.jpg",
            "https://iiif.archive.org/image/iiif/3/book%2Fa%5Cb.png/full/max/0/default.jpg",
            "https://iiif.archive.org/image/iiif/3/book%252Fa.png/full/max/0/default.jpg",
            "https://iiif.archive.org/image/iiif/3/book%2F%252e%252e%252Fa.png/full/max/0/default.jpg",
            "https://iiif.archive.org/image/iiif/3/book%2Fa%00.png/full/max/0/default.jpg",
            "https://iiif.archive.org/image/iiif/3/book%2Fa%FF.png/full/max/0/default.jpg",
            "https://iiif.archive.org/image/iiif/3/book%2Fa%GG.png/full/max/0/default.jpg",
            "https://iiif.archive.org/image/iiif/3/book%2Fa.jpg/full/max/0/default.jpg",
            "https://iiif.archive.org/image/iiif/3/.book%2Fa.png/full/max/0/default.jpg",
        ] {
            let source = Url::parse(raw).expect("URL fixture");
            let mut requested = source.clone();
            requested.set_fragment(None);
            let (agent, calls) =
                scripted_thumbnail_agent(vec![(requested.as_str(), 404, None, Vec::new())]);
            assert!(super::fetch_thumbnail(&agent, &source).is_err(), "{raw}");
            assert!(
                calls.load(std::sync::atomic::Ordering::Relaxed) <= 1,
                "{raw}"
            );
        }
    }

    /// A failed IIIF redirect never grants permission to request a private host.
    #[test]
    fn archive_iiif_waveform_unsafe_redirect_remains_fail_closed() {
        let source =
            Url::parse("https://iiif.archive.org/image/iiif/3/book%2Fa.png/full/max/0/default.jpg")
                .expect("waveform URL");
        let (agent, calls) = scripted_thumbnail_agent(vec![(
            source.as_str(),
            302,
            Some("https://127.0.0.1/private.png"),
            Vec::new(),
        )]);
        assert_eq!(
            super::fetch_thumbnail(&agent, &source),
            Err(ThumbnailFailure::InvalidSource)
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    /// Fallback does not relax the image format or per-response download limit.
    #[test]
    fn archive_iiif_waveform_fallback_keeps_response_guards() {
        let source =
            Url::parse("https://iiif.archive.org/image/iiif/3/book%2Fa.png/full/max/0/default.jpg")
                .expect("waveform URL");
        let fallback = "https://archive.org/services/img/book";
        for (body, expected) in [
            (
                vec![0; super::MAX_DOWNLOAD_BYTES + 1],
                ThumbnailFailure::ResponseTooLarge,
            ),
            (
                b"<html>missing</html>".to_vec(),
                ThumbnailFailure::UnsupportedFormat,
            ),
        ] {
            let (agent, calls) = scripted_thumbnail_agent(vec![
                (source.as_str(), 404, None, Vec::new()),
                (fallback, 200, None, body),
            ]);
            assert_eq!(super::fetch_thumbnail(&agent, &source), Err(expected));
            assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
        }
    }

    /// Initial and fallback requests share one redirect allowance.
    #[test]
    fn archive_iiif_waveform_fallback_shares_the_redirect_limit() {
        let source =
            Url::parse("https://iiif.archive.org/image/iiif/3/book%2Fa.png/full/max/0/default.jpg")
                .expect("waveform URL");
        let fallback = "https://archive.org/services/img/book";
        let (agent, calls) = scripted_thumbnail_agent(vec![
            (
                source.as_str(),
                302,
                Some("https://cdn.archive.org/full.jpg"),
                Vec::new(),
            ),
            ("https://cdn.archive.org/full.jpg", 404, None, Vec::new()),
            (
                fallback,
                302,
                Some("https://cdn.archive.org/tile.jpg"),
                Vec::new(),
            ),
            (
                "https://cdn.archive.org/tile.jpg",
                302,
                Some("https://cdn.archive.org/tile2.jpg"),
                Vec::new(),
            ),
            (
                "https://cdn.archive.org/tile2.jpg",
                302,
                Some("https://cdn.archive.org/tile3.jpg"),
                Vec::new(),
            ),
        ]);
        assert_eq!(
            super::fetch_thumbnail(&agent, &source),
            Err(ThumbnailFailure::DownloadFailed)
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 5);
    }

    /// Canonical nested Unicode filenames keep only the validated item identity.
    #[test]
    fn archive_iiif_waveform_fallback_decodes_one_safe_path_segment() {
        let mut source = Url::parse("https://iiif.archive.org/").expect("IIIF origin");
        source
            .path_segments_mut()
            .expect("hierarchical URL")
            .clear()
            .extend([
                "image",
                "iiif",
                "3",
                "public_book/album/ქართული + waveform.PNG",
                "full",
                "max",
                "0",
                "default.jpg",
            ]);
        let fallback = "https://archive.org/services/img/public_book";
        let image = b"\xFF\xD8\xFFtile";
        let (agent, calls) = scripted_thumbnail_agent(vec![
            (source.as_str(), 404, None, Vec::new()),
            (fallback, 200, None, image.to_vec()),
        ]);
        assert_eq!(super::fetch_thumbnail(&agent, &source), Ok(image.to_vec()));
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    /// An immediate transport timeout can spend the remaining budget on the tile.
    #[test]
    fn archive_iiif_waveform_timeout_uses_the_fallback() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let source =
            Url::parse("https://iiif.archive.org/image/iiif/3/book%2Fa.png/full/max/0/default.jpg")
                .expect("waveform URL");
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let expected = source.to_string();
        let image = b"\xFF\xD8\xFFtile";
        let agent = ureq::Agent::config_builder()
            .middleware(
                move |request: ureq::http::Request<ureq::SendBody>,
                      _next: ureq::middleware::MiddlewareNext| {
                    match observed.fetch_add(1, Ordering::Relaxed) {
                        0 => {
                            assert_eq!(request.uri().to_string(), expected);
                            Err(ureq::Error::Timeout(ureq::Timeout::Global))
                        }
                        1 => {
                            assert_eq!(
                                request.uri().to_string(),
                                "https://archive.org/services/img/book"
                            );
                            Ok(ureq::http::Response::builder()
                                .status(200)
                                .body(ureq::Body::builder().data(image.to_vec()))
                                .expect("tile response"))
                        }
                        _ => panic!("unexpected artwork request"),
                    }
                },
            )
            .build()
            .into();
        assert_eq!(super::fetch_thumbnail(&agent, &source), Ok(image.to_vec()));
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    /// Neither an initial response nor a fallback body can reset the total deadline.
    #[test]
    fn archive_iiif_waveform_fallback_keeps_one_overall_deadline() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, Instant};
        let source =
            Url::parse("https://iiif.archive.org/image/iiif/3/book%2Fa.png/full/max/0/default.jpg")
                .expect("waveform URL");
        for slow_fallback in [false, true] {
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&calls);
            let expected = source.to_string();
            let agent = ureq::Agent::config_builder()
                .middleware(
                    move |request: ureq::http::Request<ureq::SendBody>,
                          _next: ureq::middleware::MiddlewareNext| {
                        let index = observed.fetch_add(1, Ordering::Relaxed);
                        if index == 0 {
                            assert_eq!(request.uri().to_string(), expected);
                        } else {
                            assert_eq!(index, 1, "at most one fallback");
                            assert_eq!(
                                request.uri().to_string(),
                                "https://archive.org/services/img/book"
                            );
                        }
                        if (index == 1) == slow_fallback {
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        Ok(ureq::http::Response::builder()
                            .status(if index == 0 { 404 } else { 200 })
                            .body(ureq::Body::builder().data(b"\xFF\xD8\xFFtile".to_vec()))
                            .expect("timed response"))
                    },
                )
                .build()
                .into();
            assert_eq!(
                super::fetch_remote_thumbnail_with_fallback(
                    &agent,
                    &source,
                    Instant::now(),
                    Duration::from_millis(50)
                ),
                Err(ThumbnailFailure::DownloadFailed),
            );
            assert_eq!(
                calls.load(Ordering::Relaxed),
                if slow_fallback { 2 } else { 1 }
            );
        }
        let (agent, calls) = scripted_thumbnail_agent(Vec::new());
        assert_eq!(
            super::fetch_remote_thumbnail_with_fallback(
                &agent,
                &source,
                Instant::now() - Duration::from_secs(1),
                Duration::from_millis(50)
            ),
            Err(ThumbnailFailure::DownloadFailed),
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
    /// LibriVox's stable cover URL redirects to the Archive.org image CDN.
    #[test]
    fn archive_thumbnail_redirects_fetch_the_cover_for_standard_redirect_statuses() {
        let source =
            Url::parse("https://archive.org/download/Covers/book_thumb.jpg").expect("cover URL");
        let target = "https://dn710703.ca.archive.org/0/items/Covers/book_thumb.jpg";
        let image = b"\xFF\xD8\xFF\xE0cover";
        for status in [301, 302, 303, 307, 308] {
            let (agent, calls) = scripted_thumbnail_agent(vec![
                (source.as_str(), status, Some(target), Vec::new()),
                (target, 200, None, image.to_vec()),
            ]);
            assert_eq!(
                super::fetch_thumbnail(&agent, &source).expect("redirected cover"),
                image
            );
            assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
        }
    }

    /// A legitimate initial URL cannot authorize credentials or another origin.
    #[test]
    fn archive_thumbnail_redirects_refuse_unsafe_targets_before_requesting_them() {
        let source = Url::parse("https://archive.org/download/Covers/book.jpg").expect("cover URL");
        for target in [
            "http://cdn.archive.org/book.jpg",
            "https://archive.org.evil.test/book.jpg",
            "https://notarchive.org/book.jpg",
            "https://example.org/book.jpg",
            "https://127.0.0.1/book.jpg",
            "https://[::1]/book.jpg",
            "https://169.254.169.254/book.jpg",
            "https://localhost/book.jpg",
            "https://user:password@cdn.archive.org/book.jpg",
            "https://cdn.archive.org:8443/book.jpg",
            "file:///tmp/book.jpg",
            "https://cdn.archive.org/book.jpg?token=secret",
            "https://cdn.archive.org/book.jpg#part",
        ] {
            let (agent, calls) =
                scripted_thumbnail_agent(vec![(source.as_str(), 302, Some(target), Vec::new())]);
            assert_eq!(
                super::fetch_thumbnail(&agent, &source),
                Err(ThumbnailFailure::InvalidSource),
                "{target}"
            );
            assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        }
    }

    /// The allow-list applies to every hop, not just the first Location header.
    #[test]
    fn archive_thumbnail_redirects_recheck_intermediate_servers() {
        let source = Url::parse("https://archive.org/services/img/public_book").expect("image URL");
        let target = "https://cdn.archive.org/public_book.jpg";
        let (agent, calls) = scripted_thumbnail_agent(vec![
            (source.as_str(), 302, Some(target), Vec::new()),
            (
                target,
                302,
                Some("https://example.org/private.jpg"),
                Vec::new(),
            ),
        ]);
        assert_eq!(
            super::fetch_thumbnail(&agent, &source),
            Err(ThumbnailFailure::InvalidSource)
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    /// An endless redirect chain remains bounded without changing other origins.
    #[test]
    fn archive_thumbnail_redirects_are_bounded_and_other_origins_remain_refused() {
        let source = Url::parse("https://archive.org/services/img/public_book").expect("image URL");
        let (agent, calls) = scripted_thumbnail_agent(vec![
            (
                source.as_str(),
                302,
                Some(source.as_str()),
                Vec::new()
            );
            4
        ]);
        assert_eq!(
            super::fetch_thumbnail(&agent, &source),
            Err(ThumbnailFailure::DownloadFailed)
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 4);

        let source = Url::parse("https://example.org/book.jpg").expect("other origin");
        let (agent, calls) = scripted_thumbnail_agent(vec![(
            source.as_str(),
            302,
            Some("https://archive.org/services/img/public_book"),
            Vec::new(),
        )]);
        assert_eq!(
            super::fetch_thumbnail(&agent, &source),
            Err(ThumbnailFailure::DownloadFailed)
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    /// Redirected image bytes keep the same 4 MiB budget as direct responses.
    #[test]
    fn archive_thumbnail_redirects_preserve_the_download_size_limit() {
        let source = Url::parse("https://archive.org/download/Covers/book.jpg").expect("cover URL");
        let target = "https://cdn.archive.org/book.jpg";
        let (agent, calls) = scripted_thumbnail_agent(vec![
            (source.as_str(), 302, Some(target), Vec::new()),
            (target, 200, None, vec![0; super::MAX_DOWNLOAD_BYTES + 1]),
        ]);
        assert_eq!(
            super::fetch_thumbnail(&agent, &source),
            Err(ThumbnailFailure::ResponseTooLarge)
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    /// Live coverage of the canonical LibriVox cover URL and its CDN redirect.
    #[test]
    #[ignore = "requires live Internet Archive artwork and public network access"]
    fn librivox_cover_artwork_live_redirect_smoke() {
        let source = Url::parse(
            "https://archive.org/download/LibrivoxCdCoverArt27/withturkspalestine_1301_thumb.jpg",
        )
        .expect("public LibriVox cover URL");
        let directory = tempfile::tempdir().expect("private artwork fixture cache");
        let artwork = remote_artwork(directory.path(), &source).expect("live LibriVox cover");
        assert_eq!(artwork.format, ArtworkFormat::Jpeg);
        assert!(!artwork.bytes.is_empty());
    }

    /// This entry point is reachable from a web view, so it must never become a
    /// way to read the filesystem — even though the terminal pipeline accepts
    /// `file:` sources for local artwork.
    #[test]
    fn a_file_source_is_refused_even_though_the_terminal_accepts_one() {
        for source in [
            "file:///etc/passwd",
            "file:///Users/someone/.config/youta/secrets/credentials.toml",
        ] {
            let url = Url::parse(source).expect("parse fixture");
            assert_eq!(
                remote_artwork(Path::new("/nonexistent"), &url),
                Err(ThumbnailFailure::InvalidSource),
                "{source}"
            );
        }
    }

    /// Private and link-local destinations stay refused on this path too, so a
    /// provider cannot use artwork to reach the user's network.
    #[test]
    fn non_public_and_non_http_sources_are_refused_without_touching_the_network() {
        for source in [
            "http://127.0.0.1/cover.png",
            "http://192.168.1.1/cover.png",
            "http://[::1]/cover.png",
            "http://localhost/cover.png",
            "http://printer.local/cover.png",
            "ftp://example.com/cover.png",
            "http://user:secret@example.com/cover.png",
        ] {
            let url = Url::parse(source).expect("parse fixture");
            assert_eq!(
                remote_artwork(Path::new("/nonexistent"), &url),
                Err(ThumbnailFailure::InvalidSource),
                "{source}"
            );
        }
    }

    /// The served type comes from the bytes, never from a URL or a header.
    #[test]
    fn the_media_type_is_decided_by_the_leading_bytes() {
        assert_eq!(
            ArtworkFormat::sniff(b"\x89PNG\r\n\x1a\nrest"),
            Some(ArtworkFormat::Png)
        );
        assert_eq!(
            ArtworkFormat::sniff(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some(ArtworkFormat::Jpeg)
        );
        assert_eq!(
            ArtworkFormat::sniff(b"RIFF\0\0\0\0WEBPVP8 "),
            Some(ArtworkFormat::WebP)
        );
        assert_eq!(ArtworkFormat::sniff(b"<svg onload=alert(1)>"), None);
        assert_eq!(ArtworkFormat::sniff(b"<!doctype html>"), None);
        assert_eq!(ArtworkFormat::sniff(b""), None);
        assert_eq!(ArtworkFormat::Png.media_type(), "image/png");
    }
}
