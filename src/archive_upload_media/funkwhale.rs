//! Original-pod resolution and bounded native staging for reviewed Funkwhale exports.

use std::path::Path;
#[cfg(any(feature = "commons-upload", feature = "evernote", test))]
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use url::Url;

use super::{PreparedArchiveMedia, StagingPlan};
use crate::config::Config;
use crate::domain::{MediaItem, MediaKind, SourceKind};
use crate::providers::funkwhale::FunkwhaleProvider;

/// Stages a reviewed Funkwhale track as private Opus without invoking a downloader.
///
/// Resolution uses the selected canonical page's original pod, not a later
/// preference or a saved audio URL. Only the fresh advertised HTTP(S) stream
/// reaches FFmpeg; cancellation, private cleanup and resource limits use the
/// same supervisor as other reviewed Internet Archive exports.
///
/// # Errors
/// Rejects nonaudio, mismatched or noncanonical selections, video requests,
/// unavailable public audio, cancellation, helper failures and staging limits.
pub fn prepare_funkwhale_archive_media(
    config: &Config,
    media: &MediaItem,
    playback_location: &str,
    upload_video: bool,
    cancellation: &Arc<AtomicBool>,
    progress: impl FnMut(u64, Option<u64>),
) -> Result<PreparedArchiveMedia, String> {
    prepare_with_limits(
        config,
        media,
        playback_location,
        upload_video,
        cancellation,
        progress,
        super::MAX_STAGING_BYTES,
        super::MAX_PREPARATION_TIME,
    )
}

/// Retains the source identity check before creating staging or resolving audio.
fn prepare_with_limits(
    config: &Config,
    media: &MediaItem,
    playback_location: &str,
    upload_video: bool,
    cancellation: &Arc<AtomicBool>,
    progress: impl FnMut(u64, Option<u64>),
    max_bytes: u64,
    deadline: Duration,
) -> Result<PreparedArchiveMedia, String> {
    let started = Instant::now();
    super::local::check_work(cancellation, started, deadline)?;
    if media.id.source != SourceKind::Funkwhale || media.kind != MediaKind::Audio || upload_video {
        return Err("Funkwhale exports support selected audio tracks only".to_owned());
    }
    if media.id.external_id != media.webpage_url.as_str()
        || playback_location != media.webpage_url.as_str()
    {
        return Err(
            "The Funkwhale identity, original page and playback location must match".to_owned(),
        );
    }
    canonical_source(&media.webpage_url)?;
    super::prepare_staged_media(
        "media",
        false,
        cancellation,
        progress,
        max_bytes,
        deadline,
        |directory, _| {
            let audio = resolve_audio(&media.webpage_url, cancellation, started, deadline)?;
            let output = directory.join("media.opus");
            Ok(StagingPlan {
                command: Some(ffmpeg_command(
                    &config.providers.ffmpeg_executable,
                    &audio,
                    &output,
                )),
                expected_output: Some(output),
            })
        },
    )
}

/// Uses the Commons caller's already-private directory with the same bounded supervisor.
#[cfg(any(feature = "commons-upload", feature = "evernote"))]
pub(crate) fn prepare_opus_in_directory(
    source: &Url,
    directory: &Path,
    ffmpeg_executable: &Path,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf, String> {
    let cancellation = AtomicBool::new(false);
    let started = Instant::now();
    let audio = resolve_audio(source, &cancellation, started, super::MAX_PREPARATION_TIME)?;
    let output = directory.join("audio.opus");
    let command = ffmpeg_command(ffmpeg_executable, &audio, &output);
    let path = super::supervise_staging_command(
        command,
        Some(output),
        directory,
        &cancellation,
        &mut progress,
        super::MAX_STAGING_BYTES,
        started,
        super::MAX_PREPARATION_TIME,
    )?;
    super::validate_completed_file(&path, directory, false)?;
    let bytes = path
        .metadata()
        .map_err(|error| format!("Cannot inspect prepared Funkwhale audio: {error}"))?
        .len();
    progress(bytes, Some(bytes));
    Ok(path)
}

/// Accepts original track pages with or without a trailing slash and returns the pod.
///
/// Provider IDs use the slashless spelling, while browser links may include a
/// slash. Either remains unchanged as publication provenance; the numeric ID
/// must still use its canonical decimal spelling and cannot carry credentials,
/// query parameters, fragments or a different resource path.
pub(crate) fn canonical_source(source: &Url) -> Result<Url, String> {
    let pod = FunkwhaleProvider::instance_url_from_track_id(source.as_str()).map_err(|_| {
        "Funkwhale export requires a canonical, credential-free track page".to_owned()
    })?;
    let prefix = pod
        .join("library/tracks/")
        .map_err(|_| "Invalid Funkwhale track page")?;
    let number = source
        .as_str()
        .strip_prefix(prefix.as_str())
        .map(|value| value.strip_suffix('/').unwrap_or(value))
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|number| *number > 0)
        .ok_or("Funkwhale export requires a canonical track page")?;
    let suffix = if source.path().ends_with('/') {
        "/"
    } else {
        ""
    };
    let canonical = prefix
        .join(&format!("{number}{suffix}"))
        .map_err(|_| "Invalid Funkwhale track page")?;
    if source != &canonical {
        return Err("Funkwhale export requires a canonical track page".to_owned());
    }
    Ok(pod)
}

/// Resolves in a bounded HTTP worker so cancellation never waits for a stalled pod.
///
/// A canceled request's worker may finish its bounded REST calls in the
/// background, but it cannot start FFmpeg or retain a staging directory.
fn resolve_audio(
    source: &Url,
    cancellation: &AtomicBool,
    started: Instant,
    deadline: Duration,
) -> Result<Url, String> {
    super::local::check_work(cancellation, started, deadline)?;
    let pod = canonical_source(source)?;
    let timeout =
        crate::providers::DEFAULT_REQUEST_TIMEOUT.min(deadline.saturating_sub(started.elapsed()));
    let provider =
        FunkwhaleProvider::with_options(pod, timeout, crate::providers::DEFAULT_MAX_JSON_BYTES)
            .map_err(|error| error.to_string())?;
    let identity = source.to_string();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("funkwhale-export-resolve".to_owned())
        .spawn(move || {
            let result = provider
                .resolve_stream_url(&identity)
                .map_err(|error| format!("Could not resolve Funkwhale audio: {error}"));
            let _ = sender.send(result);
        })
        .map_err(|error| format!("Cannot start Funkwhale audio resolution: {error}"))?;
    wait_for_audio(receiver, cancellation, started, deadline)
}

/// Waits only for a current bounded resolution, honoring whole-export cancellation.
fn wait_for_audio(
    receiver: Receiver<Result<Url, String>>,
    cancellation: &AtomicBool,
    started: Instant,
    deadline: Duration,
) -> Result<Url, String> {
    loop {
        super::local::check_work(cancellation, started, deadline)?;
        match receiver
            .recv_timeout(Duration::from_millis(25).min(deadline.saturating_sub(started.elapsed())))
        {
            Ok(result) => {
                super::local::check_work(cancellation, started, deadline)?;
                let url = result?;
                if !matches!(url.scheme(), "http" | "https")
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                {
                    return Err("Funkwhale advertised an unsafe audio URL".to_owned());
                }
                return Ok(url);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err("Funkwhale audio resolution stopped without a result".to_owned());
            }
        }
    }
}

/// Confines remote input protocols and produces one fixed, audio-only Opus output.
fn ffmpeg_command(executable: &Path, source: &Url, output: &Path) -> Command {
    let mut command = Command::new(executable);
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
            "http,https,tcp,tls,crypto",
            "-rw_timeout",
            "15000000",
            "-i",
        ])
        .arg(source.as_str())
        .args([
            "-map", "0:a:0", "-vn", "-sn", "-dn", "-c:a", "libopus", "-f", "opus",
        ])
        .arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

#[cfg(all(test, unix))]
mod network_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{MediaId, MediaLicense, MediaStatistics};
    use std::sync::atomic::Ordering;

    /// Builds a stable selected page, never a cached or signed playback locator.
    pub(super) fn selected(page: &str) -> MediaItem {
        MediaItem {
            id: MediaId::new(SourceKind::Funkwhale, page),
            kind: MediaKind::Audio,
            title: "Selected recording".to_owned(),
            creator: None,
            description: None,
            webpage_url: Url::parse(page).unwrap(),
            thumbnail_url: None,
            duration_seconds: None,
            published_at: None,
            statistics: MediaStatistics::default(),
            license: MediaLicense::Unknown,
            chapters: Vec::new(),
            captions: Vec::new(),
        }
    }

    /// Forged identities and nonaudio choices never reach a provider or helper.
    #[test]
    fn rejects_mismatched_or_noncanonical_selections_before_network() {
        let config = Config::for_dir("/tmp/youta-funkwhale-export-validation");
        let page = "https://pod.invalid/library/tracks/42/";
        let cancellation = Arc::new(AtomicBool::new(false));
        for change in ["source", "kind", "identity", "locator", "video"] {
            let mut media = selected(page);
            let mut location = page;
            match change {
                "source" => media.id.source = SourceKind::YouTube,
                "kind" => media.kind = MediaKind::Video,
                "identity" => media.id.external_id = "42".to_owned(),
                "locator" => location = "https://cdn.invalid/cached-audio.flac",
                _ => {}
            }
            let error = prepare_funkwhale_archive_media(
                &config,
                &media,
                location,
                change == "video",
                &cancellation,
                |_, _| {},
            )
            .unwrap_err();
            assert!(
                error.contains("Funkwhale") || error.contains("audio"),
                "{change}: {error}"
            );
        }
        for page in [
            "https://pod.invalid/library/tracks/042/",
            "https://pod.invalid/library/tracks/042",
            "https://pod.invalid/library/tracks/0",
            "https://pod.invalid/library/tracks/42//",
            "https://pod.invalid/library/tracks/42/?token=bad",
            "https://pod.invalid/library/tracks/42?token=bad",
            "https://pod.invalid/library/tracks/42#fragment",
            "https://user:secret@pod.invalid/library/tracks/42/",
            "file:///library/tracks/42/",
            "https://pod.invalid/library/artists/42/",
        ] {
            assert!(
                canonical_source(&Url::parse(page).unwrap()).is_err(),
                "{page}"
            );
        }
    }

    /// Provider-generated IDs omit a slash; pasted original pages may include one.
    #[test]
    fn canonical_source_accepts_both_original_track_spellings_without_rewriting_them() {
        for page in [
            "https://pod.example/library/tracks/42",
            "https://pod.example/library/tracks/42/",
            "https://pod.example/prefix/library/tracks/42",
            "https://pod.example/prefix/library/tracks/42/",
        ] {
            let source = Url::parse(page).unwrap();
            let original = source.clone();
            let expected = if page.contains("/prefix/") {
                "https://pod.example/prefix/"
            } else {
                "https://pod.example/"
            };
            assert_eq!(canonical_source(&source).unwrap().as_str(), expected);
            assert_eq!(source, original, "public provenance must remain exact");
        }
    }

    /// Cancellation and elapsed deadlines reject work before any resolution starts.
    #[test]
    fn cancellation_and_deadlines_reject_unstarted_exports() {
        let source = Url::parse("https://pod.invalid/library/tracks/42/").unwrap();
        let cancellation = Arc::new(AtomicBool::new(true));
        assert!(
            resolve_audio(
                &source,
                &cancellation,
                Instant::now(),
                Duration::from_secs(1)
            )
            .unwrap_err()
            .contains("canceled")
        );
        cancellation.store(false, Ordering::Relaxed);
        assert!(
            resolve_audio(&source, &cancellation, Instant::now(), Duration::ZERO)
                .unwrap_err()
                .contains("timed out")
        );
    }

    /// The helper's only input is network audio and its output is fixed Opus.
    #[test]
    fn ffmpeg_command_restricts_input_protocols_and_ignores_downloader_configuration() {
        let source = Url::parse("https://cdn.example/fresh.flac?token=ephemeral").unwrap();
        let output = Path::new("/tmp/private-staging/media.opus");
        let command = ffmpeg_command(Path::new("configured-ffmpeg"), &source, output);
        assert_eq!(command.get_program(), "configured-ffmpeg");
        let args = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.windows(2).any(|args| args == ["-i", source.as_str()]));
        assert!(
            args.windows(2)
                .any(|args| args == ["-protocol_whitelist", "http,https,tcp,tls,crypto"])
        );
        assert!(args.windows(2).any(|args| args == ["-c:a", "libopus"]));
        assert!(args.windows(2).any(|args| args == ["-f", "opus"]));
        assert_eq!(
            args.last().map(String::as_str),
            Some(output.to_str().unwrap())
        );
    }

    /// Resolver waiting is cancellable even while its bounded HTTP call completes.
    #[test]
    fn cancel_during_resolution_does_not_wait_for_the_network_worker() {
        let (_sender, receiver) = std::sync::mpsc::sync_channel(1);
        let cancellation = Arc::new(AtomicBool::new(false));
        let signal = cancellation.clone();
        let cancel = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            signal.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        assert!(
            wait_for_audio(receiver, &cancellation, started, Duration::from_secs(2))
                .unwrap_err()
                .contains("canceled")
        );
        cancel.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
