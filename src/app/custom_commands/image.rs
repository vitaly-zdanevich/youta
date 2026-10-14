//! Captured command artwork, isolated from Youta's ordinary thumbnail cache.

use std::path::{Path, PathBuf};
#[cfg(any(feature = "remote-artwork", feature = "local-artwork"))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(any(feature = "remote-artwork", feature = "local-artwork"))]
use std::time::{SystemTime, UNIX_EPOCH};

/// Artwork frozen with the selected target before typing or downloading media.
#[derive(Clone)]
pub(super) enum CommandImageSource {
    /// Local selection; explicit image files do not require an artwork feature.
    Local {
        /// Full selected filesystem path, never an abbreviated display path.
        path: PathBuf,
        /// Whether the selected entry itself is an image rather than media or a folder.
        is_image: bool,
    },
    /// Provider-advertised artwork, fetched through the guarded shared cache.
    Remote(url::Url),
}

/// Sequence separating editable exports produced by one process.
#[cfg(any(feature = "remote-artwork", feature = "local-artwork"))]
static IMAGE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Resolves one captured image without executing any user-authored command.
///
/// User-owned images and sidecars retain their original editable paths. Remote
/// images and embedded covers are copied into a separately bounded private
/// cache so an editor cannot modify the thumbnail another frontend displays.
/// Copies persist for forked editors and use the ordinary cache's age, count,
/// and byte limits rather than disappearing when the launching shell exits.
///
/// This helper may perform bounded network or tag reads and belongs on a
/// command-preparation worker, never a renderer or the reducer's input thread.
///
/// # Errors
/// Returns a concise error when the captured image is missing, unsafe, cannot
/// be cached, or needs artwork support omitted from this build.
pub(super) fn prepare(
    source: CommandImageSource,
    cache_directory: &Path,
) -> Result<PathBuf, String> {
    match source {
        CommandImageSource::Local {
            path,
            is_image: true,
        } => regular_image_path(&path),
        CommandImageSource::Local {
            path,
            is_image: false,
        } => prepare_local_media(&path, cache_directory),
        CommandImageSource::Remote(source) => prepare_remote(&source, cache_directory),
    }
}

/// Accepts only a real file and returns the crate's canonical path spelling.
fn regular_image_path(path: &Path) -> Result<PathBuf, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| "The selected image could not be read for %i".to_owned())?;
    if !metadata.file_type().is_file() {
        return Err("%i requires a regular image file, not a directory or symlink".to_owned());
    }
    crate::fs_path::canonicalize(path)
        .map_err(|_| "The selected image path could not be resolved for %i".to_owned())
}

/// Preserves original sidecars while isolating cache-owned embedded pictures.
#[cfg(feature = "local-artwork")]
fn prepare_local_media(path: &Path, cache_directory: &Path) -> Result<PathBuf, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| "The selected local item could not be read for %i".to_owned())?;
    let source = if metadata.file_type().is_dir() {
        crate::local_browser::find_local_folder_cover(path)
            .map_err(|_| "The selected folder artwork could not be read for %i".to_owned())?
            .and_then(|path| url::Url::from_file_path(path).ok())
    } else if metadata.file_type().is_file() {
        crate::local_artwork::local_media_artwork(path, cache_directory)
            .map_err(|error| format!("Cannot prepare %i: {error}"))?
    } else {
        return Err("%i requires a regular local item, not a symlink".to_owned());
    }
    .ok_or_else(|| "No image is available for the selected item (%i)".to_owned())?;
    let image_path = source
        .to_file_path()
        .map_err(|()| "The selected artwork has no local path for %i".to_owned())?;
    let image_path = regular_image_path(&image_path)?;
    if crate::fs_path::canonicalize(cache_directory)
        .is_ok_and(|directory| image_path.starts_with(directory))
    {
        let artwork = crate::artwork::local_artwork(&source)
            .map_err(|error| format!("Cannot prepare %i: {error}"))?;
        export_image(&artwork.bytes, cache_directory)
    } else {
        Ok(image_path)
    }
}

/// Keeps command-only builds free of tag parsers and artwork cache dependencies.
#[cfg(not(feature = "local-artwork"))]
fn prepare_local_media(_path: &Path, _cache_directory: &Path) -> Result<PathBuf, String> {
    Err("%i requires local-artwork support for local media or folders".to_owned())
}

/// Uses returned bytes, including safe Archive fallbacks, rather than guessing a cache path.
#[cfg(feature = "remote-artwork")]
fn prepare_remote(source: &url::Url, cache_directory: &Path) -> Result<PathBuf, String> {
    let artwork = crate::artwork::remote_artwork(cache_directory, source)
        .map_err(|error| format!("Cannot prepare %i: {error}"))?;
    export_image(&artwork.bytes, cache_directory)
}

/// Remote fetching stays independently removable from explicit shell commands.
#[cfg(not(feature = "remote-artwork"))]
fn prepare_remote(_source: &url::Url, _cache_directory: &Path) -> Result<PathBuf, String> {
    Err("%i requires remote-artwork support for online images".to_owned())
}

/// Writes an invocation-specific editable image with the shared private-cache policy.
#[cfg(any(feature = "remote-artwork", feature = "local-artwork"))]
fn export_image(bytes: &[u8], cache_directory: &Path) -> Result<PathBuf, String> {
    if bytes.is_empty()
        || bytes.len() > crate::artwork::MAX_DOWNLOAD_BYTES
        || crate::artwork::ArtworkFormat::sniff(bytes).is_none()
    {
        return Err("The selected image is invalid or exceeds the %i artwork limit".to_owned());
    }
    let sequence = IMAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "Cannot determine the image export time for %i".to_owned())?
        .as_nanos();
    // A later Youta process may reuse this PID while an earlier editor still
    // holds its image. The timestamp keeps those restarts' exports separate.
    let key = format!(
        "youta-command-image-v1:{}:{created_at}:{sequence}",
        std::process::id()
    );
    let cache = crate::artwork::ThumbnailCache::new(cache_directory.join("command-images"));
    cache
        .store_key(key.as_bytes(), bytes)
        .map_err(|_| "The selected image could not be saved for %i".to_owned())?;
    regular_image_path(&cache.entry_path_for_key(key.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Opaque image fixtures exercise file and cache handling without a decoder.
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\ncommand image fixture";

    /// An explicitly selected image is already the user's editable original.
    #[test]
    fn local_image_returns_its_original_absolute_path_without_creating_a_cache() {
        let directory = crate::test_support::canonical_tempdir("command image fixture");
        let path = directory.path().join("selected image.png");
        let cache_directory = directory.path().join("thumbnail-cache");
        fs::write(&path, PNG).unwrap();
        assert_eq!(
            prepare(
                CommandImageSource::Local {
                    path: path.clone(),
                    is_image: true
                },
                &cache_directory
            )
            .unwrap(),
            path,
        );
        assert!(!cache_directory.exists());
    }

    /// Direct images must not turn a directory or symlink into a file argument.
    #[test]
    fn local_image_rejects_missing_files_and_directories() {
        let directory = crate::test_support::canonical_tempdir("command image validation");
        for path in [
            directory.path().to_path_buf(),
            directory.path().join("missing.png"),
        ] {
            assert!(
                prepare(
                    CommandImageSource::Local {
                        path,
                        is_image: true
                    },
                    directory.path()
                )
                .is_err()
            );
        }
    }

    /// Refusing a linked source preserves the target and the private cache.
    #[cfg(unix)]
    #[test]
    fn local_image_rejects_symlinks() {
        let directory = crate::test_support::canonical_tempdir("linked command image");
        let original = directory.path().join("original.png");
        let link = directory.path().join("link.png");
        fs::write(&original, PNG).unwrap();
        std::os::unix::fs::symlink(&original, &link).unwrap();
        assert!(
            prepare(
                CommandImageSource::Local {
                    path: link,
                    is_image: true
                },
                directory.path()
            )
            .is_err()
        );
        assert_eq!(fs::read(original).unwrap(), PNG);
    }

    /// Two editors receive separate cached exports without another provider request.
    #[cfg(feature = "remote-artwork")]
    #[test]
    fn cached_remote_artwork_has_distinct_editable_copies_and_preserves_the_source() {
        let directory = crate::test_support::canonical_tempdir("cached command image");
        let source = url::Url::parse("https://images.example/selected.png").unwrap();
        let cache = crate::artwork::ThumbnailCache::new(directory.path().to_path_buf());
        cache.store(&source, PNG).unwrap();
        let first = prepare(CommandImageSource::Remote(source.clone()), directory.path()).unwrap();
        let second = prepare(CommandImageSource::Remote(source.clone()), directory.path()).unwrap();
        assert_ne!(first, second);
        assert_eq!(
            first.parent(),
            Some(directory.path().join("command-images").as_path())
        );
        assert_eq!(fs::read(&first).unwrap(), PNG);
        assert_eq!(fs::read(&second).unwrap(), PNG);
        fs::write(first, b"edited by the user's image editor").unwrap();
        assert_eq!(cache.read(&source).unwrap().as_deref(), Some(PNG));
        assert_eq!(fs::read(second).unwrap(), PNG);
    }

    /// Cached bytes never authorize an otherwise unsafe remote locator.
    #[cfg(feature = "remote-artwork")]
    #[test]
    fn remote_artwork_rejects_local_and_private_urls_before_export() {
        let directory = crate::test_support::canonical_tempdir("unsafe command image");
        for source in [
            "http://127.0.0.1/image.png",
            "file:///tmp/image.png",
            "https://localhost/image.png",
        ] {
            assert!(
                prepare(
                    CommandImageSource::Remote(url::Url::parse(source).unwrap()),
                    directory.path()
                )
                .is_err()
            );
        }
        assert!(!directory.path().join("command-images").exists());
    }

    /// Folder covers remain editable beside the user's album rather than in a cache.
    #[cfg(feature = "local-artwork")]
    #[test]
    fn local_folder_uses_the_original_cover_and_rejects_a_folder_without_artwork() {
        let directory = crate::test_support::canonical_tempdir("folder command artwork");
        let path = directory.path().join("album");
        let cache_directory = directory.path().join("thumbnail-cache");
        fs::create_dir(&path).unwrap();
        assert!(
            prepare(
                CommandImageSource::Local {
                    path: path.clone(),
                    is_image: false
                },
                &cache_directory
            )
            .is_err()
        );
        let cover = path.join("cover.png");
        fs::write(&cover, PNG).unwrap();
        assert_eq!(
            prepare(
                CommandImageSource::Local {
                    path,
                    is_image: false
                },
                &cache_directory
            )
            .unwrap(),
            cover
        );
        assert!(!cache_directory.exists());
    }

    /// An embedded cover is copied before an editor can change the cached thumbnail.
    #[cfg(feature = "local-artwork")]
    #[test]
    fn embedded_local_artwork_is_isolated_from_the_media_and_the_shared_cache() {
        use lofty::config::WriteOptions;
        use lofty::picture::{MimeType, Picture, PictureType};
        use lofty::tag::{Tag, TagExt, TagType};

        let directory = crate::test_support::canonical_tempdir("embedded command artwork");
        let path = directory.path().join("track.mp3");
        let cache_directory = directory.path().join("thumbnail-cache");
        let mut tag = Tag::new(TagType::Id3v2);
        tag.push_picture(
            Picture::unchecked(PNG.to_vec())
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Png)
                .build(),
        );
        let mut media_bytes = Vec::new();
        tag.dump_to(&mut media_bytes, WriteOptions::default())
            .unwrap();
        media_bytes.extend_from_slice(&[0_u8; 256]);
        fs::write(&path, &media_bytes).unwrap();
        let cached = crate::local_artwork::local_media_artwork(&path, &cache_directory)
            .unwrap()
            .unwrap()
            .to_file_path()
            .unwrap();
        let exported = prepare(
            CommandImageSource::Local {
                path: path.clone(),
                is_image: false,
            },
            &cache_directory,
        )
        .unwrap();
        assert_ne!(exported, cached);
        assert_eq!(fs::read(&exported).unwrap(), PNG);
        fs::write(exported, b"edited artwork").unwrap();
        assert_eq!(fs::read(cached).unwrap(), PNG);
        assert_eq!(fs::read(path).unwrap(), media_bytes);
    }

    /// Local media discovery can be omitted while ordinary selected images still work.
    #[cfg(not(feature = "local-artwork"))]
    #[test]
    fn disabled_local_artwork_reports_a_clear_error() {
        assert_eq!(
            prepare(
                CommandImageSource::Local {
                    path: PathBuf::from("track.mp3"),
                    is_image: false
                },
                Path::new("cache")
            ),
            Err("%i requires local-artwork support for local media or folders".to_owned())
        );
    }

    /// Remote fetching stays independently removable from command-only builds.
    #[cfg(not(feature = "remote-artwork"))]
    #[test]
    fn disabled_remote_artwork_reports_a_clear_error() {
        assert_eq!(
            prepare(
                CommandImageSource::Remote(
                    url::Url::parse("https://images.example/selected.png").unwrap()
                ),
                Path::new("cache")
            ),
            Err("%i requires remote-artwork support for online images".to_owned())
        );
    }
}
