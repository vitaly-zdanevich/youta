//! Cover discovery for local media, without a renderer.
//!
//! A local file carries its artwork in one of two places: inside its tags, or
//! beside it as an image written by whatever produced the file — `yt-dlp
//! --write-thumbnail` leaves `Title [id].webp` next to `Title [id].opus`, and
//! tagging tools leave `cover.jpg` in the album folder. This module finds both
//! and publishes one URL for a front-end to render.
//!
//! Embedded pictures are copied into Youta's private artwork cache, because the
//! alternative is handing a renderer a byte range inside the user's media file.
//! Sidecar images are published where they lie: they are already ordinary image
//! files, and copying a 20 MiB scan into a 4 MiB cache would lose it.
//!
//! Nothing here decodes. The format is decided by the leading bytes, the byte
//! budget is fixed, and pixel limits belong to whichever renderer actually
//! decodes — [`crate::thumbnails`] for a terminal, the web view for a window.
//! That is what keeps this module free of both `image` and Ratatui, so a build
//! with `local-artwork` and no `images` still shows covers.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sha2::{Digest, Sha256};
use url::Url;

use crate::artwork::{ArtworkFormat, MAX_DOWNLOAD_BYTES, ThumbnailCache};

/// Cumulative tag bytes one media file may be read for.
const MAX_LOCAL_ARTWORK_READ_BYTES: usize = 8 * 1024 * 1024;
/// Largest single tag item Lofty may allocate, with container overhead.
const MAX_LOCAL_ARTWORK_TAG_ITEM_BYTES: usize = MAX_DOWNLOAD_BYTES + 64 * 1024;
/// Embedded pictures considered before the rest are ignored.
const MAX_LOCAL_ARTWORK_PICTURES: usize = 64;
/// Versioned prefix isolating these cache keys from every other kind.
const LOCAL_ARTWORK_CACHE_KEY_VERSION: &[u8] = b"youta-local-art-v1\0";
/// Sidecar image extensions, in the order a tie is broken.
///
/// WebP is included because it is what `yt-dlp` writes beside a download.
const SIDECAR_EXTENSIONS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];
/// Bytes read from a sidecar to identify its format.
const SIDECAR_SNIFF_BYTES: usize = 16;
/// Sidecars collected from one directory before the scan stops.
const MAX_SIDECAR_COVERS: usize = 4_096;

/// Failure while extracting and persisting optional artwork from local media.
///
/// Messages intentionally omit the media path so callers can safely surface a
/// concise failure without disclosing a private filesystem layout.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LocalArtworkError {
    /// The requested path was a symlink, directory, or another non-file object.
    #[error("local artwork source is not a regular file")]
    InvalidSource,
    /// The file changed between validation, parsing, and cache publication.
    #[error("local artwork source changed while it was being read")]
    SourceChanged,
    /// Lofty or the cumulative reader reached an artwork safety limit.
    #[error("embedded artwork exceeds Youta's bounded extraction limits")]
    LimitExceeded,
    /// Reading or inspecting the source failed.
    #[error("unable to read the local artwork source")]
    SourceIo(#[source] io::Error),
    /// The media container or its tags were malformed.
    #[error("unable to parse embedded artwork")]
    Tag(#[source] lofty::error::LoftyError),
    /// The private thumbnail cache could not be read or updated.
    #[error("unable to access the local artwork cache")]
    CacheIo(#[source] io::Error),
    /// The cache entry could not be represented as an absolute file URL.
    #[error("unable to represent cached local artwork as a file URL")]
    CacheUrl,
}

/// Returns the artwork for one local media file, or `Ok(None)` when it has none.
///
/// Embedded pictures win: they belong to the file itself, while a sidecar was
/// written next to it and may describe a whole download batch. A container that
/// cannot be parsed still falls back to its sidecar rather than reporting a
/// failure, because a broken tag is no reason to hide an image that is sitting
/// right there; the parse failure is only returned when nothing else was found.
///
/// # Errors
///
/// Returns [`LocalArtworkError`] when the media file is unsafe, changes during
/// extraction, exceeds a hard limit, or cannot be persisted in the cache — and
/// only when no sidecar covers for it.
pub(crate) fn local_media_artwork(
    media_path: &Path,
    cache_directory: &Path,
) -> Result<Option<Url>, LocalArtworkError> {
    match cached_local_artwork(media_path, cache_directory) {
        Ok(Some(embedded)) => Ok(Some(embedded)),
        Ok(None) => Ok(sidecar_artwork_url(media_path)),
        Err(error) => sidecar_artwork_url(media_path).map_or(Err(error), |url| Ok(Some(url))),
    }
}

/// Finds the image written beside one media file, as an absolute file URL.
fn sidecar_artwork_url(media_path: &Path) -> Option<Url> {
    Url::from_file_path(find_sidecar_cover(media_path)?).ok()
}

/// Finds an image sharing a media file's name, such as `Track.webp` for
/// `Track.opus`.
///
/// An unreadable directory is not an error here: it means no sidecar, exactly
/// as an absent one does.
fn find_sidecar_cover(media_path: &Path) -> Option<PathBuf> {
    let stem = media_path.file_stem()?.to_str()?.to_ascii_lowercase();
    sidecar_covers_in(media_path.parent()?).remove(&stem)
}

/// Collects one directory's sidecar images, keyed by the media stem they cover.
///
/// A list asks about every file in the same directory at once, so this is one
/// pass rather than one scan per row.
///
/// Extensions are matched case-insensitively and only regular files count, so a
/// symlink beside the media never becomes artwork. The leading bytes must
/// identify a supported image, which keeps a stray `Track.jpg` that is really a
/// text file from reaching a renderer as a broken picture. A directory holding
/// more sidecars than the bound is read until the bound, so an enormous folder
/// costs a bounded scan rather than an unbounded one.
pub(crate) fn sidecar_covers_in(directory: &Path) -> HashMap<String, PathBuf> {
    let mut covers: HashMap<String, (usize, PathBuf)> = HashMap::new();
    let Ok(entries) = fs::read_dir(directory) else {
        return HashMap::new();
    };
    for entry in entries.flatten() {
        if covers.len() >= MAX_SIDECAR_COVERS {
            break;
        }
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Some(priority) = path
            .extension()
            .and_then(|extension| extension.to_str())
            .and_then(|extension| {
                SIDECAR_EXTENSIONS
                    .iter()
                    .position(|candidate| extension.eq_ignore_ascii_case(candidate))
            })
        else {
            continue;
        };
        let stem = stem.to_ascii_lowercase();
        if covers
            .get(&stem)
            .is_some_and(|(selected, _)| *selected <= priority)
        {
            continue;
        }
        if entry.file_type().is_ok_and(|file_type| file_type.is_file()) && sidecar_is_image(&path) {
            covers.insert(stem, (priority, path));
        }
    }
    covers
        .into_iter()
        .map(|(stem, (_, path))| (stem, path))
        .collect()
}

/// Returns the sidecar covering one media file from an already-scanned
/// directory.
pub(crate) fn sidecar_cover_for(
    covers: &HashMap<String, PathBuf>,
    media_path: &Path,
) -> Option<Url> {
    let stem = media_path.file_stem()?.to_str()?.to_ascii_lowercase();
    Url::from_file_path(covers.get(&stem)?).ok()
}

/// Identifies a sidecar from its leading bytes without reading the whole file.
fn sidecar_is_image(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut header = Vec::with_capacity(SIDECAR_SNIFF_BYTES);
    if file
        .take(SIDECAR_SNIFF_BYTES as u64)
        .read_to_end(&mut header)
        .is_err()
    {
        return false;
    }
    ArtworkFormat::sniff(&header).is_some()
}

/// Extracts one bounded embedded cover and returns its persistent cache URL.
///
/// The source must be a regular file rather than a symlink. Youta opens it
/// read-only, limits cumulative tag reads to 8 MiB, limits any single Lofty tag
/// allocation to slightly over 4 MiB for container overhead, and accepts only
/// JPEG, PNG, or WebP pictures within the artwork byte limit. At most 64
/// embedded pictures are considered, with front cover preferred over `Other`,
/// then the remaining picture types.
///
/// Cache keys contain a versioned digest of the canonical path and stable file
/// metadata. An unchanged source therefore reuses its private opaque cache
/// entry across restarts, while a tag edit or file replacement gets a new
/// entry. The media file is never written. Unsupported, malformed, or absent
/// pictures are an ordinary `Ok(None)`.
///
/// This helper is synchronous by design and must run on Youta's bounded
/// background provider worker, never on a front-end's render thread.
///
/// # Errors
///
/// Returns [`LocalArtworkError`] when the source is unsafe, changes during
/// extraction, exceeds a hard limit, cannot be parsed, or cannot be persisted
/// in the private cache.
pub(crate) fn cached_local_artwork(
    media_path: &Path,
    cache_directory: &Path,
) -> Result<Option<Url>, LocalArtworkError> {
    cached_local_artwork_with_extractor(media_path, cache_directory, extract_local_artwork)
}

fn cached_local_artwork_with_extractor<F>(
    media_path: &Path,
    cache_directory: &Path,
    extractor: F,
) -> Result<Option<Url>, LocalArtworkError>
where
    F: FnOnce(&LocalMediaFingerprint) -> Result<Option<ValidatedArtwork>, LocalArtworkError>,
{
    let fingerprint = LocalMediaFingerprint::capture(media_path)?;
    let cache_key = fingerprint.cache_key();
    let cache = ThumbnailCache::new(cache_directory.to_path_buf());

    match cache
        .read_key(&cache_key)
        .map_err(LocalArtworkError::CacheIo)?
    {
        Some(bytes) if ArtworkFormat::sniff(&bytes).is_some() => {
            fingerprint.ensure_current()?;
            return cached_local_artwork_url(&cache, &cache_key).map(Some);
        }
        Some(_) => cache.remove_key(&cache_key),
        None => {}
    }

    let Some(artwork) = extractor(&fingerprint)? else {
        fingerprint.ensure_current()?;
        return Ok(None);
    };
    fingerprint.ensure_current()?;
    cache
        .store_key(&cache_key, &artwork.0)
        .map_err(LocalArtworkError::CacheIo)?;
    fingerprint.ensure_current().inspect_err(|_| {
        cache.remove_key(&cache_key);
    })?;
    cached_local_artwork_url(&cache, &cache_key).map(Some)
}

fn cached_local_artwork_url(
    cache: &ThumbnailCache,
    cache_key: &[u8],
) -> Result<Url, LocalArtworkError> {
    let path = fs::canonicalize(cache.entry_path_for_key(cache_key))
        .map_err(LocalArtworkError::CacheIo)?;
    Url::from_file_path(path).map_err(|()| LocalArtworkError::CacheUrl)
}

fn extract_local_artwork(
    fingerprint: &LocalMediaFingerprint,
) -> Result<Option<ValidatedArtwork>, LocalArtworkError> {
    use lofty::config::{GlobalOptions, ParseOptions, apply_global_options};
    use lofty::file::{FileType, TaggedFileExt};
    use lofty::probe::Probe;

    let file = File::open(&fingerprint.canonical_path).map_err(LocalArtworkError::SourceIo)?;
    let opened_metadata = file.metadata().map_err(LocalArtworkError::SourceIo)?;
    let opened =
        LocalMediaFingerprint::from_metadata(fingerprint.canonical_path.clone(), &opened_metadata);
    if &opened != fingerprint {
        return Err(LocalArtworkError::SourceChanged);
    }

    let _options_reset = LoftyGlobalOptionsReset;
    apply_global_options(
        GlobalOptions::new()
            .allocation_limit(MAX_LOCAL_ARTWORK_TAG_ITEM_BYTES)
            .use_custom_resolvers(false)
            .preserve_format_specific_items(false),
    );
    let options = ParseOptions::new()
        .read_properties(false)
        .read_cover_art(true);
    let reader = ReadBudget::new(BufReader::new(file), MAX_LOCAL_ARTWORK_READ_BYTES);
    let probe = Probe::new(reader).options(options);
    let mut probe = probe.guess_file_type().map_err(map_local_artwork_io)?;
    if probe.file_type().is_none()
        && let Some(file_type) = FileType::from_path(&fingerprint.canonical_path)
    {
        probe = probe.set_file_type(file_type);
    }
    let tagged_file = probe.read().map_err(map_local_artwork_tag_error)?;

    let mut pictures = tagged_file
        .tags()
        .iter()
        .flat_map(lofty::tag::Tag::pictures)
        .take(MAX_LOCAL_ARTWORK_PICTURES)
        .collect::<Vec<_>>();
    pictures.sort_by_key(|picture| local_picture_priority(picture.pic_type()));
    let artwork = pictures
        .into_iter()
        .find_map(|picture| ValidatedArtwork::from_slice(picture.data()));
    drop(tagged_file);
    fingerprint.ensure_current()?;
    Ok(artwork)
}

fn local_picture_priority(picture_type: lofty::picture::PictureType) -> u8 {
    use lofty::picture::PictureType;

    match picture_type {
        PictureType::CoverFront => 0,
        PictureType::Other => 1,
        _ => 2,
    }
}

struct ValidatedArtwork(Vec<u8>);

impl ValidatedArtwork {
    /// Accepts one embedded picture that is a supported image within budget.
    ///
    /// The picture is identified by its own leading bytes rather than by the
    /// MIME type its tag claims, and it is not decoded: whichever renderer
    /// draws it applies its own pixel and allocation limits, and it is the only
    /// side that knows what those are.
    fn from_slice(bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() || bytes.len() > MAX_DOWNLOAD_BYTES {
            return None;
        }
        ArtworkFormat::sniff(bytes).map(|_| Self(bytes.to_vec()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalMediaFingerprint {
    canonical_path: PathBuf,
    length: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    change_seconds: i64,
    #[cfg(unix)]
    change_nanoseconds: i64,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanoseconds: i64,
}

impl LocalMediaFingerprint {
    fn capture(path: &Path) -> Result<Self, LocalArtworkError> {
        let supplied_metadata = fs::symlink_metadata(path).map_err(LocalArtworkError::SourceIo)?;
        if !supplied_metadata.file_type().is_file() {
            return Err(LocalArtworkError::InvalidSource);
        }
        let canonical_path = fs::canonicalize(path).map_err(LocalArtworkError::SourceIo)?;
        let canonical_metadata =
            fs::symlink_metadata(&canonical_path).map_err(LocalArtworkError::SourceIo)?;
        if !canonical_metadata.file_type().is_file() {
            return Err(LocalArtworkError::InvalidSource);
        }
        let supplied = Self::from_metadata(canonical_path.clone(), &supplied_metadata);
        let canonical = Self::from_metadata(canonical_path, &canonical_metadata);
        if supplied != canonical {
            return Err(LocalArtworkError::SourceChanged);
        }
        Ok(canonical)
    }

    fn from_metadata(canonical_path: PathBuf, metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        Self {
            canonical_path,
            length: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            change_seconds: metadata.ctime(),
            #[cfg(unix)]
            change_nanoseconds: metadata.ctime_nsec(),
            #[cfg(unix)]
            modified_seconds: metadata.mtime(),
            #[cfg(unix)]
            modified_nanoseconds: metadata.mtime_nsec(),
        }
    }

    fn ensure_current(&self) -> Result<(), LocalArtworkError> {
        if &Self::capture(&self.canonical_path)? == self {
            Ok(())
        } else {
            Err(LocalArtworkError::SourceChanged)
        }
    }

    fn cache_key(&self) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(LOCAL_ARTWORK_CACHE_KEY_VERSION);
        hash_local_path(&mut digest, &self.canonical_path);
        digest.update(self.length.to_le_bytes());
        hash_system_time(&mut digest, self.modified);
        hash_system_time(&mut digest, self.created);
        #[cfg(unix)]
        {
            digest.update(self.device.to_le_bytes());
            digest.update(self.inode.to_le_bytes());
            digest.update(self.change_seconds.to_le_bytes());
            digest.update(self.change_nanoseconds.to_le_bytes());
            digest.update(self.modified_seconds.to_le_bytes());
            digest.update(self.modified_nanoseconds.to_le_bytes());
        }
        digest.finalize().into()
    }
}

#[cfg(unix)]
fn hash_local_path(digest: &mut Sha256, path: &Path) {
    use std::os::unix::ffi::OsStrExt;

    let bytes = path.as_os_str().as_bytes();
    digest.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes());
    digest.update(bytes);
}

#[cfg(windows)]
fn hash_local_path(digest: &mut Sha256, path: &Path) {
    use std::os::windows::ffi::OsStrExt;

    let length = path.as_os_str().encode_wide().count();
    digest.update(u64::try_from(length).unwrap_or(u64::MAX).to_le_bytes());
    for word in path.as_os_str().encode_wide() {
        digest.update(word.to_le_bytes());
    }
}

#[cfg(not(any(unix, windows)))]
fn hash_local_path(digest: &mut Sha256, path: &Path) {
    let path = path.as_os_str().to_string_lossy();
    digest.update(u64::try_from(path.len()).unwrap_or(u64::MAX).to_le_bytes());
    digest.update(path.as_bytes());
}

fn hash_system_time(digest: &mut Sha256, time: Option<SystemTime>) {
    let Some(time) = time else {
        digest.update([0]);
        return;
    };
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => {
            digest.update([1]);
            digest.update(duration.as_secs().to_le_bytes());
            digest.update(duration.subsec_nanos().to_le_bytes());
        }
        Err(error) => {
            let duration = error.duration();
            digest.update([2]);
            digest.update(duration.as_secs().to_le_bytes());
            digest.update(duration.subsec_nanos().to_le_bytes());
        }
    }
}

struct ReadBudget<R> {
    inner: R,
    remaining: usize,
}

impl<R> ReadBudget<R> {
    const fn new(inner: R, limit: usize) -> Self {
        Self {
            inner,
            remaining: limit,
        }
    }
}

impl<R: Read> Read for ReadBudget<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut overflow = [0_u8; 1];
            return match self.inner.read(&mut overflow) {
                Ok(0) => Ok(0),
                Ok(_) => Err(io::Error::other(LocalArtworkReadLimit)),
                Err(error) => Err(error),
            };
        }
        let allowed = buffer.len().min(self.remaining);
        let read = self.inner.read(&mut buffer[..allowed])?;
        self.remaining = self.remaining.saturating_sub(read);
        Ok(read)
    }
}

impl<R: Seek> Seek for ReadBudget<R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.inner.seek(position)
    }
}

#[derive(Debug)]
struct LocalArtworkReadLimit;

impl fmt::Display for LocalArtworkReadLimit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("embedded artwork read limit exceeded")
    }
}

impl std::error::Error for LocalArtworkReadLimit {}

fn is_local_artwork_read_limit(error: &io::Error) -> bool {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<LocalArtworkReadLimit>())
        .is_some()
}

fn map_local_artwork_io(error: io::Error) -> LocalArtworkError {
    if is_local_artwork_read_limit(&error) {
        LocalArtworkError::LimitExceeded
    } else {
        LocalArtworkError::SourceIo(error)
    }
}

fn map_local_artwork_tag_error(error: lofty::error::LoftyError) -> LocalArtworkError {
    use lofty::error::ErrorKind;

    match error.kind() {
        ErrorKind::TooMuchData => LocalArtworkError::LimitExceeded,
        ErrorKind::Io(error) if is_local_artwork_read_limit(error) => {
            LocalArtworkError::LimitExceeded
        }
        _ => LocalArtworkError::Tag(error),
    }
}

struct LoftyGlobalOptionsReset;

impl Drop for LoftyGlobalOptionsReset {
    fn drop(&mut self) {
        lofty::config::apply_global_options(lofty::config::GlobalOptions::default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;
    use std::ffi::OsStr;
    use std::io::Cursor;

    use lofty::config::WriteOptions;
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::tag::{Accessor, Tag, TagExt, TagType};

    #[test]
    fn embedded_artwork_prefers_front_cover_then_other_then_first_picture() {
        let directory = tempfile::tempdir().expect("temporary local-media directory");
        let cache_directory = directory.path().join("thumbnail-cache");
        let media_path = directory.path().join("covers.mp3");
        let first = fixture_png(*b"one\0");
        let other = fixture_png(*b"two\0");
        let front = fixture_png(*b"three");
        write_tagged_mp3(
            &media_path,
            [
                fixture_picture(first, PictureType::Band),
                fixture_picture(other, PictureType::Other),
                fixture_picture(front.clone(), PictureType::CoverFront),
            ],
        );

        let url = cached_local_artwork(&media_path, &cache_directory)
            .expect("extract front cover")
            .expect("front cover must exist");
        assert_eq!(read_file_url(&url), front);

        let fallback_path = directory.path().join("fallback.mp3");
        let fallback = fixture_png(*b"four");
        let preferred_other = fixture_png(*b"five");
        write_tagged_mp3(
            &fallback_path,
            [
                fixture_picture(fallback, PictureType::Composer),
                fixture_picture(preferred_other.clone(), PictureType::Other),
            ],
        );
        let url = cached_local_artwork(&fallback_path, &cache_directory)
            .expect("extract Other artwork")
            .expect("Other artwork must exist");
        assert_eq!(read_file_url(&url), preferred_other);

        let first_only_path = directory.path().join("first-only.mp3");
        let first_only = fixture_png(*b"six");
        write_tagged_mp3(
            &first_only_path,
            [fixture_picture(
                first_only.clone(),
                PictureType::Illustration,
            )],
        );
        let url = cached_local_artwork(&first_only_path, &cache_directory)
            .expect("extract first available artwork")
            .expect("fallback artwork must exist");
        assert_eq!(read_file_url(&url), first_only);
    }

    #[test]
    fn absent_malformed_unsupported_and_oversized_artwork_are_cache_misses() {
        let directory = tempfile::tempdir().expect("temporary local-media directory");
        let cache_directory = directory.path().join("thumbnail-cache");

        let no_art_path = directory.path().join("no-art.mp3");
        write_tagged_mp3(&no_art_path, []);
        assert!(
            cached_local_artwork(&no_art_path, &cache_directory)
                .expect("read media without artwork")
                .is_none()
        );

        let malformed_path = directory.path().join("malformed.mp3");
        write_tagged_mp3(
            &malformed_path,
            [fixture_picture(
                b"not a PNG despite its tag".to_vec(),
                PictureType::CoverFront,
            )],
        );
        assert!(
            cached_local_artwork(&malformed_path, &cache_directory)
                .expect("ignore malformed artwork")
                .is_none()
        );

        let unsupported_path = directory.path().join("unsupported.mp3");
        let unsupported = Picture::unchecked(b"GIF89a unsupported".to_vec())
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::Gif)
            .build();
        write_tagged_mp3(&unsupported_path, [unsupported]);
        assert!(
            cached_local_artwork(&unsupported_path, &cache_directory)
                .expect("ignore unsupported artwork")
                .is_none()
        );

        let oversized_path = directory.path().join("oversized.mp3");
        write_tagged_mp3(
            &oversized_path,
            [fixture_picture(
                vec![0_u8; MAX_DOWNLOAD_BYTES + 1],
                PictureType::CoverFront,
            )],
        );
        assert!(
            matches!(
                cached_local_artwork(&oversized_path, &cache_directory),
                Ok(None) | Err(LocalArtworkError::LimitExceeded)
            ),
            "oversized tag data must never be cached"
        );
        assert!(
            !cache_directory.exists() || cache_entry_count(&cache_directory) == 0,
            "invalid optional artwork must not create cache entries"
        );
    }

    #[test]
    fn extraction_preserves_source_and_uses_opaque_private_cache_files() {
        let directory = tempfile::tempdir().expect("temporary local-media directory");
        let cache_directory = directory.path().join("thumbnail-cache");
        let media_path = directory.path().join("private album name.mp3");
        let artwork = fixture_png(*b"art\0");
        write_tagged_mp3(
            &media_path,
            [fixture_picture(artwork.clone(), PictureType::CoverFront)],
        );
        let source_before = fs::read(&media_path).expect("snapshot source bytes");
        let metadata_before = fs::metadata(&media_path).expect("snapshot source metadata");

        let url = cached_local_artwork(&media_path, &cache_directory)
            .expect("extract embedded artwork")
            .expect("embedded artwork");
        let cache_path = url.to_file_path().expect("absolute cache file URL");

        assert_eq!(read_file_url(&url), artwork);
        assert_eq!(
            fs::read(&media_path).expect("read source after extraction"),
            source_before
        );
        let metadata_after = fs::metadata(&media_path).expect("source metadata after extraction");
        assert_eq!(metadata_after.len(), metadata_before.len());
        assert_eq!(
            metadata_after.modified().ok(),
            metadata_before.modified().ok()
        );
        assert_eq!(metadata_after.permissions(), metadata_before.permissions());
        assert_eq!(
            cache_path.parent(),
            Some(
                fs::canonicalize(&cache_directory)
                    .expect("canonical cache directory")
                    .as_path()
            )
        );
        let name = cache_path
            .file_name()
            .and_then(OsStr::to_str)
            .expect("cache entry name");
        assert!(
            !name.contains("album"),
            "a cache entry must not disclose the media file name"
        );
    }

    #[test]
    fn unchanged_source_reuses_cache_and_modified_source_gets_a_new_key() {
        let directory = tempfile::tempdir().expect("temporary local-media directory");
        let cache_directory = directory.path().join("thumbnail-cache");
        let media_path = directory.path().join("album.mp3");
        let first_artwork = fixture_png(*b"aaa\0");
        let second_artwork = fixture_png(*b"bbb\0");
        write_tagged_mp3(
            &media_path,
            [fixture_picture(
                first_artwork.clone(),
                PictureType::CoverFront,
            )],
        );

        let calls = Cell::new(0);
        let extract = |bytes: Vec<u8>| {
            let calls = &calls;
            move |_: &LocalMediaFingerprint| {
                calls.set(calls.get() + 1);
                Ok(ValidatedArtwork::from_slice(&bytes))
            }
        };

        let first = cached_local_artwork_with_extractor(
            &media_path,
            &cache_directory,
            extract(first_artwork.clone()),
        )
        .expect("cache local artwork")
        .expect("first cache URL");
        assert_eq!(calls.get(), 1);
        assert_eq!(read_file_url(&first), first_artwork);

        let reused = cached_local_artwork_with_extractor(
            &media_path,
            &cache_directory,
            extract(second_artwork.clone()),
        )
        .expect("reuse cached local artwork")
        .expect("reused cache URL");
        assert_eq!(reused, first);
        assert_eq!(calls.get(), 1, "an unchanged source must not be re-read");

        write_tagged_mp3(
            &media_path,
            [
                fixture_picture(second_artwork.clone(), PictureType::CoverFront),
                fixture_picture(fixture_png(*b"ccc\0"), PictureType::Other),
            ],
        );
        let modified = cached_local_artwork_with_extractor(
            &media_path,
            &cache_directory,
            extract(second_artwork.clone()),
        )
        .expect("cache modified local artwork")
        .expect("modified cache URL");
        assert_ne!(modified, first);
        assert_eq!(calls.get(), 2, "source edit must repeat tag extraction");
        assert_eq!(read_file_url(&modified), second_artwork);
    }

    #[test]
    fn cumulative_reader_reports_data_beyond_its_fixed_budget() {
        let mut reader = ReadBudget::new(
            Cursor::new(vec![0_u8; MAX_LOCAL_ARTWORK_READ_BYTES + 1]),
            MAX_LOCAL_ARTWORK_READ_BYTES,
        );
        let mut sink = Vec::new();
        let error = reader
            .read_to_end(&mut sink)
            .expect_err("read beyond fixed artwork budget");
        assert!(is_local_artwork_read_limit(&error));
        assert_eq!(sink.len(), MAX_LOCAL_ARTWORK_READ_BYTES);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_source_is_rejected_without_touching_cache_or_target() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temporary local-media directory");
        let cache_directory = directory.path().join("thumbnail-cache");
        let media_path = directory.path().join("target.mp3");
        let symlink_path = directory.path().join("linked.mp3");
        let source = b"private source bytes";
        fs::write(&media_path, source).expect("write symlink target");
        symlink(&media_path, &symlink_path).expect("create media symlink");

        assert!(matches!(
            cached_local_artwork(&symlink_path, &cache_directory),
            Err(LocalArtworkError::InvalidSource)
        ));
        assert_eq!(
            fs::read(&media_path).expect("read untouched symlink target"),
            source
        );
        assert!(!cache_directory.exists());
    }

    /// This is what `yt-dlp --write-thumbnail` leaves beside a download, and
    /// downloads carry no embedded picture at all.
    #[test]
    fn a_sidecar_image_covers_media_that_has_no_embedded_picture() {
        let directory = tempfile::tempdir().expect("temporary local-media directory");
        let cache_directory = directory.path().join("thumbnail-cache");
        let media_path = directory.path().join("Popular Monster [ydK1vjQBvp0].mp3");
        let sidecar = directory.path().join("Popular Monster [ydK1vjQBvp0].webp");
        write_tagged_mp3(&media_path, []);
        fs::write(&sidecar, fixture_webp()).expect("write sidecar thumbnail");

        let url = local_media_artwork(&media_path, &cache_directory)
            .expect("discover sidecar artwork")
            .expect("sidecar artwork must exist");
        assert_eq!(url.to_file_path().as_deref(), Ok(sidecar.as_path()));
        assert!(
            !cache_directory.exists(),
            "a sidecar is published where it lies rather than copied"
        );
    }

    /// The file's own picture describes the file; a sidecar may describe a whole
    /// batch of downloads that happen to share a directory.
    #[test]
    fn an_embedded_picture_wins_over_a_sidecar() {
        let directory = tempfile::tempdir().expect("temporary local-media directory");
        let cache_directory = directory.path().join("thumbnail-cache");
        let media_path = directory.path().join("track.mp3");
        let embedded = fixture_png(*b"emb\0");
        write_tagged_mp3(
            &media_path,
            [fixture_picture(embedded.clone(), PictureType::CoverFront)],
        );
        fs::write(directory.path().join("track.jpg"), fixture_png(*b"side"))
            .expect("write sidecar image");

        let url = local_media_artwork(&media_path, &cache_directory)
            .expect("prefer embedded artwork")
            .expect("embedded artwork must exist");
        assert_eq!(read_file_url(&url), embedded);
    }

    #[test]
    fn sidecar_discovery_ignores_other_names_and_non_images() {
        let directory = tempfile::tempdir().expect("temporary local-media directory");
        let media_path = directory.path().join("track.opus");
        fs::write(&media_path, b"media").expect("write media fixture");
        fs::write(directory.path().join("other.png"), fixture_png(*b"nope"))
            .expect("write unrelated image");
        fs::write(directory.path().join("track.txt"), b"notes").expect("write unrelated text");
        fs::write(directory.path().join("track.png"), b"<!doctype html>")
            .expect("write mislabelled sidecar");
        assert_eq!(find_sidecar_cover(&media_path), None);

        // A JPEG outranks a WebP, and a differing case still matches.
        let jpeg = directory.path().join("track.JPG");
        fs::write(&jpeg, fixture_jpeg()).expect("write JPEG sidecar");
        fs::write(directory.path().join("track.webp"), fixture_webp()).expect("write WebP sidecar");
        assert_eq!(find_sidecar_cover(&media_path), Some(jpeg));
    }

    fn write_tagged_mp3(path: &Path, pictures: impl IntoIterator<Item = Picture>) {
        let mut tag = Tag::new(TagType::Id3v2);
        tag.set_title("Youta embedded-artwork fixture".to_owned());
        for picture in pictures {
            tag.push_picture(picture);
        }
        let mut bytes = Vec::new();
        tag.dump_to(&mut bytes, WriteOptions::default())
            .expect("encode ID3v2 fixture");
        bytes.extend_from_slice(&[0_u8; 256]);
        fs::write(path, bytes).expect("write tagged MP3 fixture");
    }

    fn fixture_picture(bytes: Vec<u8>, picture_type: PictureType) -> Picture {
        Picture::unchecked(bytes)
            .pic_type(picture_type)
            .mime_type(MimeType::Png)
            .build()
    }

    /// A distinguishable picture that sniffs as PNG.
    ///
    /// Extraction identifies a picture by its leading bytes and never decodes
    /// it, so a fixture only has to carry the signature and a distinct tail —
    /// which keeps these tests independent of the optional image decoder.
    fn fixture_png(tag: impl AsRef<[u8]>) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(tag.as_ref());
        bytes
    }

    fn fixture_jpeg() -> Vec<u8> {
        vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]
    }

    fn fixture_webp() -> Vec<u8> {
        let mut bytes = b"RIFF\0\0\0\0WEBP".to_vec();
        bytes.extend_from_slice(b"VP8 fixture");
        bytes
    }

    fn read_file_url(url: &Url) -> Vec<u8> {
        fs::read(url.to_file_path().expect("absolute file URL")).expect("read cached local artwork")
    }

    fn cache_entry_count(directory: &Path) -> usize {
        fs::read_dir(directory)
            .expect("read cache directory")
            .filter_map(Result::ok)
            .count()
    }
}
