//! Byte-preserving, cancellable publication of completed original-file caches.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use url::Url;

const MAX_ORIGINAL_BYTES: u64 = 256 * 1024 * 1024;
const COPY_DEADLINE: Duration = Duration::from_secs(60);

/// A private original-file copy ready for no-replacement publication.
pub(crate) struct PreparedOriginalDownload {
    _directory: tempfile::TempDir,
    path: PathBuf,
    destination: PathBuf,
    length: u64,
    extension: String,
}

impl PreparedOriginalDownload {
    /// Publishes without replacement; all copying and syncing happened on the worker.
    pub(crate) fn publish(
        self,
        destination: &Path,
        title: &str,
        id: &str,
    ) -> Result<PathBuf, String> {
        let destination = crate::fs_path::canonicalize(destination)
            .map_err(|_| "original download destination is unavailable")?;
        if destination != self.destination {
            return Err("original download destination changed".to_owned());
        }
        original_metadata(&self.path, self.length)?;
        let title = crate::playback_cache_download::filename_component(title, 140, "media");
        let id = crate::playback_cache_download::filename_component(id, 60, "cache");
        for collision in 0..1000 {
            let suffix = if collision == 0 {
                String::new()
            } else {
                format!(" ({collision})")
            };
            let path = destination.join(format!("{title} [{id}]{suffix}.{}", self.extension));
            match std::fs::hard_link(&self.path, &path) {
                Ok(()) => return Ok(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => {
                    return Err(
                        "original cache could not be published without replacement".to_owned()
                    );
                }
            }
        }
        Err("original cache has too many conflicting filenames".to_owned())
    }
}

/// Stages bytes from an explicitly complete, immutable original-file lease.
///
/// The caller retains the lease through publication and rechecks cancellation
/// and its owner before making the file visible. No media parser, converter,
/// network request, or external helper is invoked by this copy operation.
pub(crate) fn prepare_cached_original(
    source: &Path,
    expected_len: u64,
    source_url: &Url,
    destination: &Path,
    cancelled: &AtomicBool,
) -> Result<PreparedOriginalDownload, String> {
    let started = Instant::now();
    check_copy(cancelled, started)?;
    if !crate::domain::is_canonical_archive_org_audio_url(source_url) {
        return Err("original cache source is not a canonical Archive file".to_owned());
    }
    let extension = source_url
        .path()
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .filter(|extension| {
            matches!(
                extension.as_str(),
                "mp3"
                    | "flac"
                    | "opus"
                    | "ogg"
                    | "oga"
                    | "m4a"
                    | "m4b"
                    | "aac"
                    | "wav"
                    | "wave"
                    | "aiff"
                    | "aif"
                    | "wma"
                    | "webm"
            )
        })
        .ok_or("original cache has an unsupported audio extension")?;
    let before = original_metadata(source, expected_len)?;
    let identity = crate::file_identity::filesystem_identity(source, &before);
    let destination = crate::fs_path::canonicalize(destination)
        .map_err(|_| "original download destination is unavailable")?;
    if !destination.is_dir() {
        return Err("original download destination is not a directory".to_owned());
    }
    let directory = tempfile::Builder::new()
        .prefix(".youta-original-download-")
        .tempdir_in(&destination)
        .map_err(|_| "cannot create original download staging directory")?;
    crate::private_files::set_private_directory_permissions(directory.path())
        .map_err(|_| "cannot make original download staging private")?;
    let path = directory.path().join("original");
    let mut output = crate::private_files::open_privately(
        std::fs::OpenOptions::new().write(true).create_new(true),
    )
    .open(&path)
    .map_err(|_| "cannot stage original download")?;
    let mut input =
        std::fs::File::open(source).map_err(|_| "original cache file is unavailable")?;
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    while copied < expected_len {
        check_copy(cancelled, started)?;
        let capacity = (expected_len - copied).min(buffer.len() as u64) as usize;
        let read = input
            .read(&mut buffer[..capacity])
            .map_err(|_| "cannot read original cache")?;
        if read == 0 {
            return Err("original cache was truncated".to_owned());
        }
        output
            .write_all(&buffer[..read])
            .map_err(|_| "cannot copy original cache")?;
        copied += read as u64;
    }
    if input
        .read(&mut buffer[..1])
        .map_err(|_| "cannot finish original cache read")?
        != 0
    {
        return Err("original cache length changed".to_owned());
    }
    let after = original_metadata(source, expected_len)?;
    if before.modified().ok() != after.modified().ok()
        || identity != crate::file_identity::filesystem_identity(source, &after)
    {
        return Err("original cache identity changed".to_owned());
    }
    output
        .sync_all()
        .map_err(|_| "cannot sync original download")?;
    check_copy(cancelled, started)?;
    Ok(PreparedOriginalDownload {
        _directory: directory,
        path,
        destination,
        length: expected_len,
        extension,
    })
}

/// Checks size/type only; the caller must prove block coverage with a complete lease.
fn original_metadata(path: &Path, expected_len: u64) -> Result<std::fs::Metadata, String> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|_| "original cache file is unavailable")?;
    if expected_len == 0
        || expected_len > MAX_ORIGINAL_BYTES
        || !metadata.file_type().is_file()
        || metadata.len() != expected_len
    {
        return Err("original cache size is incomplete or unsupported".to_owned());
    }
    Ok(metadata)
}

/// Stops staging without publishing a partial download or blocking the controller.
fn check_copy(cancelled: &AtomicBool, started: Instant) -> Result<(), String> {
    if cancelled.load(Ordering::Acquire) || started.elapsed() >= COPY_DEADLINE {
        return Err("original cache preparation cancelled or timed out".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// Distinctive tag bytes at both ends must survive; decoded audio equality is insufficient.
    const TAGGED_MP3: &[u8] =
        b"ID3\x03\0\0\0\0\0\x13TIT2\0original-title\xff\xfbfixture-audioTAGoriginal-tail";

    #[test]
    fn original_cache_publication_preserves_all_bytes_and_never_overwrites() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("cached");
        std::fs::write(&source, TAGGED_MP3).unwrap();
        let destination = directory.path().join("downloads");
        std::fs::create_dir(&destination).unwrap();
        let url = Url::parse("https://archive.org/download/fixture/original.mp3").unwrap();
        let cancelled = AtomicBool::new(false);
        let prepare = || {
            prepare_cached_original(
                &source,
                TAGGED_MP3.len() as u64,
                &url,
                &destination,
                &cancelled,
            )
            .unwrap()
        };
        let first = prepare()
            .publish(&destination, "Original title", "fixture")
            .unwrap();
        let second = prepare()
            .publish(&destination, "Original title", "fixture")
            .unwrap();
        assert_ne!(first, second);
        assert_eq!(first.extension().unwrap(), "mp3");
        assert_eq!(std::fs::read(&first).unwrap(), TAGGED_MP3);
        assert_eq!(std::fs::read(&second).unwrap(), TAGGED_MP3);
        assert_eq!(std::fs::read(&source).unwrap(), TAGGED_MP3);
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 2);
    }

    #[test]
    fn original_cache_preparation_rejects_partial_wrong_format_and_cancelled_inputs() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("cached");
        std::fs::write(&source, TAGGED_MP3).unwrap();
        let destination = directory.path().join("downloads");
        std::fs::create_dir(&destination).unwrap();
        let url = Url::parse("https://archive.org/download/fixture/original.flac").unwrap();
        let cancelled = AtomicBool::new(false);
        for length in [
            0,
            TAGGED_MP3.len() as u64 - 1,
            TAGGED_MP3.len() as u64 + 1,
            256 * 1024 * 1024 + 1,
        ] {
            assert!(
                prepare_cached_original(&source, length, &url, &destination, &cancelled).is_err()
            );
        }
        let unsafe_url = Url::parse("https://archive.org/download/fixture/file.exe").unwrap();
        assert!(
            prepare_cached_original(
                &source,
                TAGGED_MP3.len() as u64,
                &unsafe_url,
                &destination,
                &cancelled
            )
            .is_err()
        );
        cancelled.store(true, Ordering::Release);
        assert!(
            prepare_cached_original(
                &source,
                TAGGED_MP3.len() as u64,
                &url,
                &destination,
                &cancelled
            )
            .is_err()
        );
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
    }
}
