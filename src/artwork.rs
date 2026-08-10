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
use ureq::ResponseExt;
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
/// Redirects are refused outright rather than followed, because a redirect can
/// cross from a public host to a private one after the first check passed.
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
    let mut response = agent
        .get(source.as_str())
        .header("Accept", "image/jpeg, image/png, image/webp")
        .call()
        .map_err(|_| ThumbnailFailure::DownloadFailed)?;
    if !(200..300).contains(&response.status().as_u16()) {
        return Err(ThumbnailFailure::DownloadFailed);
    }
    let final_url =
        Url::parse(&response.get_uri().to_string()).map_err(|_| ThumbnailFailure::InvalidSource)?;
    if !is_safe_remote_thumbnail_source(&final_url, allow_non_public_test_source) {
        return Err(ThumbnailFailure::InvalidSource);
    }
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
    if bytes.len() > MAX_DOWNLOAD_BYTES {
        Err(ThumbnailFailure::ResponseTooLarge)
    } else {
        Ok(bytes)
    }
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
/// only public `http`/`https` origins are accepted, redirects are refused
/// rather than followed, the response is size-capped, the bytes must actually
/// be an image, and the result lands in the confined private cache. A window
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
