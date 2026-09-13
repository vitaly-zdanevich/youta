//! Bounded private preparation of selected local or provider media for S3 uploads.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::archive_upload_media::{
    MAX_PREPARATION_TIME, MAX_STAGING_BYTES, PreparedArchiveMedia, StagingPlan,
    prepare_staged_media, provider_staging_command,
};
use crate::config::Config;
use crate::domain::{MediaItem, MediaKind, SourceKind};

/// A staged S3 object retained until upload ends and removed when its guard drops.
pub type PreparedS3Media = PreparedArchiveMedia;

/// Cheap selection capabilities; this check performs no I/O or authentication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct S3MediaCapabilities {
    /// Whether this selection can also be staged as original-quality MKV video.
    pub upload_video: bool,
}

/// Returns supported finite-media actions without probing a file or network.
#[must_use]
pub fn s3_media_capabilities(
    media: &MediaItem,
    playback_location: &str,
) -> Option<S3MediaCapabilities> {
    if !matches!(
        media.kind,
        MediaKind::Audio | MediaKind::Video | MediaKind::PodcastEpisode
    ) {
        return None;
    }
    selected_source(media, playback_location)?;
    Some(S3MediaCapabilities {
        upload_video: media.kind == MediaKind::Video
            && !matches!(
                media.id.source,
                SourceKind::YandexMusic | SourceKind::ModArchive
            ),
    })
}

/// Creates privately owned Opus audio, or stream-copy-only MKV video.
///
/// Call on a worker with the selected queue item's validated playback location.
/// Local and materialized archive files are copied, never moved or changed.
///
/// # Errors
/// Returns a bounded error for unsupported sources, canceled work, missing
/// credentials, invalid outputs, helper failure, or resource-limit violations.
pub fn prepare_s3_media(
    config: &Config,
    media: &MediaItem,
    playback_location: &str,
    upload_video: bool,
    cancellation: &Arc<AtomicBool>,
    progress: impl FnMut(u64, Option<u64>),
) -> Result<PreparedS3Media, String> {
    prepare_with_limits(
        config,
        media,
        playback_location,
        upload_video,
        cancellation,
        progress,
        MAX_STAGING_BYTES,
        MAX_PREPARATION_TIME,
    )
}

/// Locators are deliberately not Debug: resolved query strings can be sensitive.
enum MediaSource {
    Local(PathBuf),
    Remote(url::Url),
    #[cfg(feature = "yandex-music")]
    Yandex(String),
}

/// Only caller-selected absolute files, safe remote URLs and native authenticated
/// Yandex identities are accepted. Container rows and live streams are not objects.
fn selected_source(media: &MediaItem, location: &str) -> Option<MediaSource> {
    if location.len() > 16 * 1024 || location.chars().any(char::is_control) {
        return None;
    }
    if Path::new(location).is_absolute() {
        return Some(MediaSource::Local(PathBuf::from(location)));
    }
    if let Ok(url) = url::Url::parse(location)
        && url.scheme() == "file"
    {
        return (url.host_str().is_none() || url.host_str() == Some("localhost"))
            .then(|| url.to_file_path().ok())
            .flatten()
            .filter(|path| path.is_absolute())
            .map(MediaSource::Local);
    }
    #[cfg(feature = "yandex-music")]
    if media.id.source == SourceKind::YandexMusic {
        let id = &media.id.external_id;
        return (!id.is_empty() && id.len() <= 64 && id.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| MediaSource::Yandex(id.clone()));
    }
    if matches!(
        media.id.source,
        SourceKind::YandexMusic | SourceKind::ModArchive
    ) {
        return None;
    }
    if media.id.source == SourceKind::YouTube {
        let source = &media.webpage_url;
        let Some(crate::links::LinkTarget::YouTubeVideo { video_id, .. }) =
            crate::links::parse_youtube_url(source)
        else {
            return None;
        };
        if video_id != media.id.external_id {
            return None;
        }
        return crate::links::LinkTarget::YouTubeVideo {
            video_id,
            start_seconds: None,
        }
        .canonical_url()
        .map(MediaSource::Remote);
    }
    let source = url::Url::parse(location).ok()?;
    (matches!(source.scheme(), "http" | "https")
        && source.host_str().is_some()
        && source.username().is_empty()
        && source.password().is_none())
    .then_some(MediaSource::Remote(source))
}

fn prepare_with_limits(
    config: &Config,
    media: &MediaItem,
    playback_location: &str,
    upload_video: bool,
    cancellation: &Arc<AtomicBool>,
    progress: impl FnMut(u64, Option<u64>),
    max_bytes: u64,
    deadline: Duration,
) -> Result<PreparedS3Media, String> {
    let started = Instant::now();
    check_work(cancellation, started, deadline)?;
    let capability = s3_media_capabilities(media, playback_location).ok_or(
        "Only finite supported audio/video selections can be uploaded; record live radio first",
    )?;
    if upload_video && !capability.upload_video {
        return Err("This selection has no supported video to upload".to_owned());
    }
    let source = selected_source(media, playback_location)
        .ok_or("The selected S3 media source is unavailable")?;
    let filename_stem = safe_filename_stem(&media.title);
    let staged = prepare_staged_media(
        &filename_stem,
        upload_video,
        cancellation,
        progress,
        max_bytes,
        deadline,
        |directory, progress| match source {
            MediaSource::Local(path) => prepare_local_plan(
                config,
                &path,
                directory,
                upload_video,
                cancellation,
                progress,
                max_bytes,
                started,
                deadline,
            ),
            MediaSource::Remote(url) => Ok(StagingPlan {
                command: Some(provider_staging_command(
                    config,
                    url,
                    directory,
                    upload_video,
                    max_bytes,
                    upload_video,
                    Some("!is_live & live_status !=? is_upcoming"),
                )),
                expected_output: None,
            }),
            #[cfg(feature = "yandex-music")]
            MediaSource::Yandex(track_id) => prepare_yandex_plan(
                config,
                &track_id,
                directory,
                cancellation,
                progress,
                max_bytes,
                started,
                deadline,
            ),
        },
    )?;
    // Review defaults use one stable extension. A helper must not silently
    // turn a reviewed video object into a different container or an audio file.
    let expected = if upload_video { "mkv" } else { "opus" };
    if staged.path().extension().and_then(|value| value.to_str()) != Some(expected) {
        return Err("S3 media preparation produced an unexpected output format".to_owned());
    }
    Ok(staged)
}

/// Object labels remain readable, bounded and free from path/control syntax.
fn safe_filename_stem(title: &str) -> String {
    let mut stem = String::new();
    for character in title.chars() {
        if stem.len().saturating_add(character.len_utf8()) > 160 {
            break;
        }
        if character.is_control()
            || matches!(
                character,
                '/' | '\\' | ':' | '"' | '<' | '>' | '|' | '?' | '*'
            )
        {
            stem.push('_');
        } else {
            stem.push(character);
        }
    }
    let stem = stem.trim().trim_matches('.').trim();
    if stem.is_empty() {
        "media".to_owned()
    } else {
        stem.to_owned()
    }
}

fn check_work(
    cancellation: &AtomicBool,
    started: Instant,
    deadline: Duration,
) -> Result<(), String> {
    if cancellation.load(Ordering::Relaxed) {
        return Err("Media preparation canceled".to_owned());
    }
    if started.elapsed() >= deadline {
        return Err("Media preparation timed out".to_owned());
    }
    Ok(())
}

/// Pins an open regular input and copies it into private staging. The source
/// inode is never moved, truncated or passed to a helper after its owner drops.
fn copy_local_input(
    source: &Path,
    destination: &Path,
    cancellation: &AtomicBool,
    progress: &mut dyn FnMut(u64, Option<u64>),
    max_bytes: u64,
    started: Instant,
    deadline: Duration,
) -> Result<u64, String> {
    check_work(cancellation, started, deadline)?;
    let source = crate::fs_path::canonicalize(source)
        .map_err(|error| format!("Cannot resolve selected local media: {error}"))?;
    let inspected = std::fs::symlink_metadata(&source)
        .map_err(|error| format!("Cannot inspect selected local media: {error}"))?;
    if !inspected.file_type().is_file() {
        return Err("The selected media must be a nonempty regular file".to_owned());
    }
    // A reviewed path can be replaced before this worker opens it. Nonblocking
    // + no-follow prevents a FIFO/symlink swap from hanging or changing the input.
    #[cfg(unix)]
    let mut input = std::fs::File::from(
        rustix::fs::open(
            &source,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|error| format!("Cannot open selected local media safely: {error}"))?,
    );
    #[cfg(not(unix))]
    let mut input = std::fs::File::open(source)
        .map_err(|error| format!("Cannot open selected local media: {error}"))?;
    let before = input
        .metadata()
        .map_err(|error| format!("Cannot inspect selected local media: {error}"))?;
    if !before.is_file() || before.len() == 0 {
        return Err("The selected media must be a nonempty regular file".to_owned());
    }
    if before.len() > max_bytes {
        return Err("Media preparation staging size limit exceeded".to_owned());
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options
        .open(destination)
        .map_err(|error| format!("Cannot create private input copy: {error}"))?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut copied = 0_u64;
    loop {
        check_work(cancellation, started, deadline)?;
        let count = input
            .read(&mut buffer)
            .map_err(|error| format!("Cannot read selected local media: {error}"))?;
        if count == 0 {
            break;
        }
        copied = copied
            .checked_add(count as u64)
            .ok_or("Media preparation staging size limit exceeded")?;
        if copied > max_bytes {
            return Err("Media preparation staging size limit exceeded".to_owned());
        }
        output
            .write_all(&buffer[..count])
            .map_err(|error| format!("Cannot stage selected local media: {error}"))?;
        progress(copied, Some(before.len()));
    }
    output
        .flush()
        .map_err(|error| format!("Cannot finish private input copy: {error}"))?;
    let after = input
        .metadata()
        .map_err(|error| format!("Cannot recheck selected local media: {error}"))?;
    if copied != before.len()
        || after.len() != before.len()
        || after.modified().ok() != before.modified().ok()
    {
        return Err("The selected local media changed during preparation; try again".to_owned());
    }
    check_work(cancellation, started, deadline)?;
    Ok(copied)
}

fn prepare_local_plan(
    config: &Config,
    source: &Path,
    directory: &Path,
    upload_video: bool,
    cancellation: &AtomicBool,
    progress: &mut dyn FnMut(u64, Option<u64>),
    max_bytes: u64,
    started: Instant,
    deadline: Duration,
) -> Result<StagingPlan, String> {
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("media")
        .to_ascii_lowercase();
    let extension =
        if extension.len() <= 12 && extension.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            extension.as_str()
        } else {
            "media"
        };
    let copy_only = !upload_video && extension == "opus";
    let input = directory.join(if copy_only {
        "media.opus".to_owned()
    } else {
        format!("source.{extension}")
    });
    copy_local_input(
        source,
        &input,
        cancellation,
        progress,
        max_bytes,
        started,
        deadline,
    )?;
    if copy_only {
        return Ok(StagingPlan {
            command: None,
            expected_output: Some(input),
        });
    }
    Ok(ffmpeg_plan(config, &input, directory, upload_video))
}

/// FFmpeg reads only a complete private file; video output always uses copy.
fn ffmpeg_plan(config: &Config, input: &Path, directory: &Path, upload_video: bool) -> StagingPlan {
    let output = directory.join(if upload_video {
        "media.mkv"
    } else {
        "media.opus"
    });
    let mut command = Command::new(&config.providers.ffmpeg_executable);
    command
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-n",
            "-progress",
            "pipe:1",
            "-protocol_whitelist",
            "file,pipe",
            "-i",
        ])
        .arg(input);
    if upload_video {
        command.args([
            "-map", "0:v:0", "-map", "0:a?", "-c", "copy", "-f", "matroska",
        ]);
    } else {
        command.args([
            "-map", "0:a:0", "-vn", "-sn", "-dn", "-c:a", "libopus", "-f", "opus",
        ]);
    }
    command
        .arg(&output)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    StagingPlan {
        command: Some(command),
        expected_output: Some(output),
    }
}

#[cfg(feature = "yandex-music")]
static YANDEX_RESOLUTION_ACTIVE: AtomicBool = AtomicBool::new(false);

/// A canceled lookup retains its one process-wide slot until native HTTP ends.
#[cfg(feature = "yandex-music")]
struct YandexResolutionSlot;

#[cfg(feature = "yandex-music")]
impl Drop for YandexResolutionSlot {
    fn drop(&mut self) {
        YANDEX_RESOLUTION_ACTIVE.store(false, Ordering::Release);
    }
}

#[cfg(feature = "yandex-music")]
fn resolve_yandex(
    config: &Config,
    track_id: &str,
    cancellation: &AtomicBool,
    started: Instant,
    deadline: Duration,
) -> Result<crate::providers::yandex_music::YandexMusicMedia, String> {
    use crate::providers::yandex_music::YandexMusicClient;
    use std::sync::mpsc::{RecvTimeoutError, sync_channel};
    check_work(cancellation, started, deadline)?;
    let token = config
        .providers
        .yandex_music_token
        .clone()
        .ok_or("Configure Yandex Music before exporting its audio")?;
    YANDEX_RESOLUTION_ACTIVE
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(
            |_| "A previous Yandex Music metadata lookup is still finishing; try again shortly",
        )?;
    let slot = YandexResolutionSlot;
    let (sender, receiver) = sync_channel(1);
    let track_id = track_id.to_owned();
    std::thread::Builder::new()
        .name("s3-yandex-metadata".to_owned())
        .spawn(move || {
            let _slot = slot;
            let result = YandexMusicClient::new(token)
                .and_then(|client| client.resolve_media(&track_id))
                .map_err(|_| "Could not resolve Yandex Music audio".to_owned());
            let _ = sender.send(result);
        })
        .map_err(|_| "Cannot start Yandex Music metadata lookup")?;
    loop {
        check_work(cancellation, started, deadline)?;
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(result) => {
                check_work(cancellation, started, deadline)?;
                return result;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err("Yandex Music metadata lookup stopped".to_owned());
            }
        }
    }
}

/// Bridges cancellation/deadline to the existing native decrypting downloader.
/// The bounded native HTTP calls remain in charge of TLS and CDN validation.
#[cfg(feature = "yandex-music")]
fn prepare_yandex_plan(
    config: &Config,
    track_id: &str,
    directory: &Path,
    cancellation: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(u64, Option<u64>),
    max_bytes: u64,
    started: Instant,
    deadline: Duration,
) -> Result<StagingPlan, String> {
    use crate::providers::yandex_music_media::YandexMusicMediaFetcher;
    let media = resolve_yandex(config, track_id, cancellation, started, deadline)?;
    if media.size_bytes.is_some_and(|bytes| bytes > max_bytes) {
        return Err("Media preparation staging size limit exceeded".to_owned());
    }
    let input = directory.join(format!("source.{}", media.codec.file_extension()));
    let fetch_cancel = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let fetch_result =
        std::thread::scope(|scope| {
            let monitor_cancel = fetch_cancel.clone();
            let monitor_finished = finished.clone();
            let source_cancel = cancellation.clone();
            scope.spawn(move || {
                while !monitor_finished.load(Ordering::Relaxed) {
                    if source_cancel.load(Ordering::Relaxed) || started.elapsed() >= deadline {
                        monitor_cancel.store(true, Ordering::Relaxed);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            });
            // RAII releases the scoped monitor even if the caller's progress callback panics.
            struct Finish(Arc<AtomicBool>);
            impl Drop for Finish {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Relaxed);
                }
            }
            let _finish = Finish(finished.clone());
            YandexMusicMediaFetcher::new(Duration::from_secs(5))
                .fetch_with_progress_and_cancellation(&media, &input, &fetch_cancel, |update| {
                    if update.bytes_written > max_bytes {
                        fetch_cancel.store(true, Ordering::Relaxed);
                    }
                    progress(update.bytes_written, update.total_bytes);
                })
        });
    check_work(cancellation, started, deadline)?;
    fetch_result.map_err(|_| {
        if fetch_cancel.load(Ordering::Relaxed) {
            "Media preparation staging size limit exceeded".to_owned()
        } else {
            "Could not fetch Yandex Music audio".to_owned()
        }
    })?;
    let bytes = std::fs::metadata(&input)
        .map_err(|_| "Cannot inspect prepared Yandex Music audio")?
        .len();
    if bytes > max_bytes {
        return Err("Media preparation staging size limit exceeded".to_owned());
    }
    if media.codec.file_extension() == "opus" {
        Ok(StagingPlan {
            command: None,
            expected_output: Some(input),
        })
    } else {
        Ok(ffmpeg_plan(config, &input, directory, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{MediaId, MediaKind, MediaLicense, MediaStatistics, SourceKind};
    use std::path::PathBuf;

    fn media(source: SourceKind, kind: MediaKind) -> MediaItem {
        MediaItem {
            id: MediaId::new(source, "BaW_jenozKc"),
            kind,
            title: "S3 fixture".to_owned(),
            creator: None,
            description: None,
            webpage_url: url::Url::parse("https://www.youtube.com/watch?v=BaW_jenozKc").unwrap(),
            thumbnail_url: None,
            duration_seconds: Some(10),
            published_at: None,
            statistics: MediaStatistics::default(),
            license: MediaLicense::Unknown,
            chapters: Vec::new(),
            captions: Vec::new(),
        }
    }

    #[test]
    fn finite_local_remote_and_materialized_tracker_sources_are_eligible() {
        for (source, kind, location, video) in [
            (
                SourceKind::Local,
                MediaKind::Audio,
                if cfg!(windows) {
                    r"C:\music\track.flac"
                } else {
                    "/music/track.flac"
                },
                false,
            ),
            (
                SourceKind::Local,
                MediaKind::Video,
                if cfg!(windows) {
                    r"C:\music\movie.mov"
                } else {
                    "/music/movie.mov"
                },
                true,
            ),
            (
                SourceKind::ModArchive,
                MediaKind::Audio,
                if cfg!(windows) {
                    r"C:\private\materialized.mod"
                } else {
                    "/private/materialized.mod"
                },
                false,
            ),
            (
                SourceKind::RemoteFiles,
                MediaKind::Audio,
                "http://192.168.1.2/music.mp3",
                false,
            ),
            (
                SourceKind::ArchiveOrg,
                MediaKind::Audio,
                "https://archive.org/download/item/track.mp3",
                false,
            ),
            (
                SourceKind::YouTube,
                MediaKind::Video,
                "https://www.youtube.com/watch?v=BaW_jenozKc",
                true,
            ),
            (
                SourceKind::ApplePodcasts,
                MediaKind::PodcastEpisode,
                "https://media.example.test/episode.m4a",
                false,
            ),
        ] {
            assert_eq!(
                s3_media_capabilities(&media(source, kind), location),
                Some(S3MediaCapabilities {
                    upload_video: video
                }),
                "{location}"
            );
        }
    }

    #[test]
    fn nonfinite_container_and_credential_bearing_selections_are_ineligible() {
        for kind in [
            MediaKind::Folder,
            MediaKind::Channel,
            MediaKind::Playlist,
            MediaKind::LiveStream,
        ] {
            assert!(
                s3_media_capabilities(
                    &media(SourceKind::YouTube, kind),
                    "https://example.test/audio"
                )
                .is_none()
            );
        }
        for location in [
            "",
            "../secret",
            "ftp://example.test/audio",
            "https://user:secret@example.test/audio",
            "file://other-computer/share/audio.opus",
        ] {
            assert!(
                s3_media_capabilities(&media(SourceKind::RemoteFiles, MediaKind::Audio), location)
                    .is_none(),
                "{location}"
            );
        }
    }

    #[test]
    fn local_opus_is_copied_without_helpers_and_drops_with_guard() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("local.opus");
        std::fs::write(&source, b"original opus fixture").unwrap();
        let mut config = Config::for_dir(directory.path().join("config"));
        config.providers.yt_dlp_executable = PathBuf::from("must-not-run-yt-dlp");
        config.providers.ffmpeg_executable = PathBuf::from("must-not-run-ffmpeg");
        let staged = prepare_s3_media(
            &config,
            &media(SourceKind::Local, MediaKind::Audio),
            source.to_str().unwrap(),
            false,
            &Arc::new(AtomicBool::new(false)),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(
            std::fs::read(staged.path()).unwrap(),
            b"original opus fixture"
        );
        assert_eq!(staged.filename(), "S3 fixture.opus");
        let staged_path = staged.path().to_owned();
        drop(staged);
        assert!(!staged_path.exists());
        assert_eq!(std::fs::read(source).unwrap(), b"original opus fixture");
    }

    #[test]
    fn local_staging_rejects_missing_empty_directory_and_oversized_inputs() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config::for_dir(directory.path().join("config"));
        let empty = directory.path().join("empty.opus");
        let oversized = directory.path().join("oversized.opus");
        std::fs::write(&empty, []).unwrap();
        std::fs::write(&oversized, b"original bytes").unwrap();
        for (path, expected) in [
            (directory.path().join("missing.opus"), "resolve"),
            (empty, "regular file"),
            (directory.path().to_owned(), "regular file"),
            (oversized.clone(), "size limit"),
        ] {
            let error = prepare_with_limits(
                &config,
                &media(SourceKind::Local, MediaKind::Audio),
                path.to_str().unwrap(),
                false,
                &Arc::new(AtomicBool::new(false)),
                |_, _| {},
                4,
                Duration::from_secs(5),
            )
            .unwrap_err();
            assert!(error.contains(expected), "{path:?}: {error}");
        }
        assert_eq!(std::fs::read(oversized).unwrap(), b"original bytes");
    }

    #[test]
    fn cancellation_during_local_copy_stops_before_completion() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("selected.opus");
        let bytes = vec![0x5a; 192 * 1024];
        std::fs::write(&source, &bytes).unwrap();
        let config = Config::for_dir(directory.path().join("config"));
        let cancellation = Arc::new(AtomicBool::new(false));
        let mut observed = Vec::new();
        let error = prepare_s3_media(
            &config,
            &media(SourceKind::Local, MediaKind::Audio),
            source.to_str().unwrap(),
            false,
            &cancellation,
            |copied, total| {
                observed.push((copied, total));
                cancellation.store(true, Ordering::Relaxed);
            },
        )
        .unwrap_err();
        assert!(error.contains("canceled"), "{error}");
        assert_eq!(observed, [(64 * 1024, Some(bytes.len() as u64))]);
        assert_eq!(std::fs::read(source).unwrap(), bytes);
    }

    #[test]
    fn local_file_urls_and_bounded_readable_filenames_are_supported() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("original.opus");
        std::fs::write(&source, b"unchanged opus").unwrap();
        let config = Config::for_dir(directory.path().join("config"));
        let source_url = url::Url::from_file_path(&source).unwrap();
        let mut selected = media(SourceKind::Local, MediaKind::Audio);
        selected.title = "../Музыка\\track:one?".to_owned();
        let prepared = prepare_s3_media(
            &config,
            &selected,
            source_url.as_str(),
            false,
            &Arc::new(AtomicBool::new(false)),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(prepared.filename(), "_Музыка_track_one_.opus");
        assert_eq!(std::fs::read(prepared.path()).unwrap(), b"unchanged opus");
        assert_eq!(safe_filename_stem("... "), "media");
        assert!(safe_filename_stem(&"音".repeat(100)).len() <= 160);
    }

    #[cfg(feature = "yandex-music")]
    #[test]
    fn yandex_uses_native_identity_and_requires_configuration_before_network() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config::for_dir(directory.path().join("config"));
        let mut selected = media(SourceKind::YandexMusic, MediaKind::Audio);
        selected.id.external_id = "123456".to_owned();
        let location = "https://music.yandex.ru/track/123456";
        assert_eq!(
            s3_media_capabilities(&selected, location),
            Some(S3MediaCapabilities {
                upload_video: false
            })
        );
        assert!(matches!(
            selected_source(&selected, location),
            Some(MediaSource::Yandex(id)) if id == "123456"
        ));
        let error = prepare_s3_media(
            &config,
            &selected,
            location,
            false,
            &Arc::new(AtomicBool::new(false)),
            |_, _| {},
        )
        .unwrap_err();
        assert!(error.contains("Configure Yandex Music"), "{error}");
        selected.id.external_id = "invalid-id".to_owned();
        assert!(s3_media_capabilities(&selected, location).is_none());
    }

    #[cfg(unix)]
    fn fake_ffmpeg(directory: &Path, body: &str) -> Config {
        use std::os::unix::fs::PermissionsExt;
        let helper = directory.join("ffmpeg-fixture");
        std::fs::write(&helper, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = Config::for_dir(directory.join("config"));
        config.providers.ffmpeg_executable = helper;
        config.providers.yt_dlp_executable = PathBuf::from("must-not-run-yt-dlp");
        config
    }

    #[cfg(unix)]
    #[test]
    fn local_non_opus_and_materialized_modules_use_configured_ffmpeg() {
        let directory = tempfile::tempdir().unwrap();
        let config = fake_ffmpeg(
            directory.path(),
            "for argument in \"$@\"; do output=$argument; done\nprintf 'encoded opus' > \"$output\"",
        );
        for (source_kind, extension) in
            [(SourceKind::Local, "flac"), (SourceKind::ModArchive, "mod")]
        {
            let source = directory.path().join(format!("selected.{extension}"));
            std::fs::write(&source, b"source bytes").unwrap();
            let staged = prepare_s3_media(
                &config,
                &media(source_kind, MediaKind::Audio),
                source.to_str().unwrap(),
                false,
                &Arc::new(AtomicBool::new(false)),
                |_, _| {},
            )
            .unwrap();
            assert_eq!(std::fs::read(staged.path()).unwrap(), b"encoded opus");
            assert_eq!(std::fs::read(source).unwrap(), b"source bytes");
        }
    }

    #[cfg(unix)]
    #[test]
    fn local_video_uses_only_stream_copy_and_mkv_output() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("selected.mov");
        std::fs::write(&source, b"movie input").unwrap();
        let config = fake_ffmpeg(
            directory.path(),
            "seen_copy=0; seen_matroska=0; for argument in \"$@\"; do case \"$argument\" in copy) seen_copy=1;; matroska) seen_matroska=1;; libopus|libx264|libaom-av1) exit 9;; esac; output=$argument; done; [ \"$seen_copy\" = 1 ]; [ \"$seen_matroska\" = 1 ]; printf 'copied video' > \"$output\"",
        );
        let staged = prepare_s3_media(
            &config,
            &media(SourceKind::Local, MediaKind::Video),
            source.to_str().unwrap(),
            true,
            &Arc::new(AtomicBool::new(false)),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(staged.filename(), "S3 fixture.mkv");
        assert_eq!(std::fs::read(staged.path()).unwrap(), b"copied video");
        assert_eq!(std::fs::read(source).unwrap(), b"movie input");
    }

    #[test]
    fn video_request_for_an_audio_only_selection_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config::for_dir(directory.path().join("config"));
        let error = prepare_s3_media(
            &config,
            &media(SourceKind::Local, MediaKind::Audio),
            directory.path().join("audio.opus").to_str().unwrap(),
            true,
            &Arc::new(AtomicBool::new(false)),
            |_, _| {},
        )
        .unwrap_err();
        assert!(error.contains("video"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn active_s3_cancel_stops_ffmpeg_and_does_not_modify_source() {
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("input.flac");
        std::fs::write(&source, b"original bytes").unwrap();
        let config = fake_ffmpeg(directory.path(), "sleep 30 &\nwait");
        let cancellation = Arc::new(AtomicBool::new(false));
        let cancel = cancellation.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            cancel.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let error = prepare_s3_media(
            &config,
            &media(SourceKind::Local, MediaKind::Audio),
            source.to_str().unwrap(),
            false,
            &cancellation,
            |_, _| {},
        )
        .unwrap_err();
        thread.join().unwrap();
        assert!(error.contains("canceled"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(std::fs::read(source).unwrap(), b"original bytes");
    }

    #[cfg(unix)]
    #[test]
    fn provider_staging_rejects_live_sources_and_forces_copy_only_mkv_for_video() {
        let directory = tempfile::tempdir().unwrap();
        for video in [false, true] {
            let extension = if video { "mkv" } else { "opus" };
            let body = format!(
                "filter=0; remux=0; output=; while [ $# -gt 0 ]; do case \"$1\" in --paths) shift; output=$1;; --match-filters) shift; [ \"$1\" = '!is_live & live_status !=? is_upcoming' ]; filter=1;; --remux-video) shift; [ \"$1\" = mkv ]; remux=1;; esac; shift; done; [ \"$filter\" = 1 ]; [ \"$remux\" = {} ]; printf 'provider media' > \"$output/media.{extension}\"; printf 'youta-file|%s/media.{extension}\\n' \"$output\"",
                u8::from(video)
            );
            let mut config = fake_ffmpeg(directory.path(), &body);
            config.providers.yt_dlp_executable = config.providers.ffmpeg_executable.clone();
            let staged = prepare_s3_media(
                &config,
                &media(SourceKind::YouTube, MediaKind::Video),
                "https://www.youtube.com/watch?v=BaW_jenozKc",
                video,
                &Arc::new(AtomicBool::new(false)),
                |_, _| {},
            )
            .unwrap();
            assert_eq!(
                staged.path().extension().and_then(|value| value.to_str()),
                Some(extension)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn fifo_source_is_rejected_without_waiting_for_a_writer() {
        use std::sync::mpsc;
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("not-a-media-file.opus");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .unwrap();
        let config = Config::for_dir(directory.path().join("config"));
        let source = fifo.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let result = prepare_s3_media(
                &config,
                &media(SourceKind::Local, MediaKind::Audio),
                source.to_str().unwrap(),
                false,
                &Arc::new(AtomicBool::new(false)),
                |_, _| {},
            )
            .map(|_| ());
            sender.send(result).unwrap();
        });
        let initial = receiver.recv_timeout(Duration::from_secs(1));
        let had_to_unblock = initial.is_err();
        // The RED implementation opens a FIFO before checking its metadata.
        // Release that open deterministically so a failing test leaks no worker.
        let writer = had_to_unblock.then(|| {
            rustix::fs::open(
                &fifo,
                rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NONBLOCK,
                rustix::fs::Mode::empty(),
            )
            .unwrap()
        });
        let result =
            initial.unwrap_or_else(|_| receiver.recv_timeout(Duration::from_secs(2)).unwrap());
        worker.join().unwrap();
        drop(writer);
        assert!(
            !had_to_unblock,
            "a nonregular input must be rejected before a blocking open"
        );
        assert!(result.unwrap_err().contains("regular file"));
    }

    #[test]
    fn precancelled_staging_does_not_inspect_or_spawn() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config::for_dir(directory.path().join("config"));
        let error = prepare_s3_media(
            &config,
            &media(SourceKind::Local, MediaKind::Audio),
            "/must-not-read.opus",
            false,
            &Arc::new(AtomicBool::new(true)),
            |_, _| {},
        )
        .unwrap_err();
        assert!(error.contains("canceled"), "{error}");
    }
}
