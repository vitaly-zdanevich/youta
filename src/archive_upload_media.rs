//! Private, cancellable media staging for explicitly reviewed Internet Archive uploads.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::domain::{MediaItem, SourceKind};
use crate::links::{LinkTarget, parse_youtube_url};
use crate::playback::ytdlp::{
    DownloadEvent, DownloadFormat, DownloadRequest, DownloadScope, YtDlpConfig,
    build_download_command, parse_download_event,
};

/// A private staged file whose temporary directory is removed when this guard drops.
#[derive(Debug)]
pub struct PreparedArchiveMedia {
    _directory: tempfile::TempDir,
    path: PathBuf,
    filename: String,
}

impl PreparedArchiveMedia {
    /// Returns the validated regular file inside this guard's private directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the safe file name used by the reviewed upload request.
    #[must_use]
    pub fn filename(&self) -> &str {
        &self.filename
    }
}

/// Maximum combined staging size, including partial downloads and merge inputs.
pub(crate) const MAX_STAGING_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub(crate) const MAX_PREPARATION_TIME: Duration = Duration::from_secs(30 * 60);

/// Prepares Opus audio, or the best available video/audio without video re-encoding.
///
/// This synchronous function belongs on the upload worker. Cancellation, a
/// deadline, disk checks and bounded output readers supervise the entire helper
/// process group. The caller retains this guard until the upload completes.
///
/// # Errors
/// Returns an explanation for invalid sources, cancellation, staging limits,
/// helper failures or output escaping the private temporary directory.
pub fn prepare_archive_media(
    config: &Config,
    media: &MediaItem,
    upload_video: bool,
    cancellation: &Arc<AtomicBool>,
    progress: impl FnMut(u64, Option<u64>),
) -> Result<PreparedArchiveMedia, String> {
    prepare_with_limits(
        config,
        media,
        upload_video,
        cancellation,
        progress,
        MAX_STAGING_BYTES,
        MAX_PREPARATION_TIME,
    )
}

/// Upload-only sorting keeps resolution and frame rate ahead of codec preferences.
const VIDEO_FORMAT_SORT: &str = "res,fps,hdr:12,vcodec:av1,channels,acodec:opus";
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 16 * 1024;
const MAX_STAGING_FILES: usize = 128;
const SUPERVISOR_POLL: Duration = Duration::from_millis(25);

/// Owns the process group even when a reader, callback or validation fails.
struct HelperGuard(Child, bool);

impl HelperGuard {
    fn stop(&mut self) {
        if !self.1 {
            crate::child_process::terminate_tree(&mut self.0);
            self.1 = true;
        }
    }
}

impl Drop for HelperGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Readers retain only the latest progress and one completed path, never a log queue.
#[derive(Default)]
struct OutputState {
    progress: Option<(u64, Option<u64>)>,
    completed: Option<PathBuf>,
    error: Option<String>,
    bytes: usize,
    done: usize,
}

fn prepare_with_limits(
    config: &Config,
    media: &MediaItem,
    upload_video: bool,
    cancellation: &Arc<AtomicBool>,
    progress: impl FnMut(u64, Option<u64>),
    max_bytes: u64,
    deadline: Duration,
) -> Result<PreparedArchiveMedia, String> {
    check_cancellation(cancellation)?;
    let source = validated_source(media)?;
    prepare_staged_media(
        &media.id.external_id,
        upload_video,
        cancellation,
        progress,
        max_bytes,
        deadline,
        |directory, _| {
            Ok(StagingPlan {
                command: Some(staging_command(
                    config,
                    source,
                    directory,
                    upload_video,
                    max_bytes,
                )),
                expected_output: None,
            })
        },
    )
}

/// One exporter-owned command, or an already completed bounded private copy.
pub(crate) struct StagingPlan {
    /// The configured supervised helper, absent for a byte-for-byte local Opus copy.
    pub command: Option<Command>,
    /// Fixed output for FFmpeg/copies; yt-dlp reports its postprocessed path instead.
    pub expected_output: Option<PathBuf>,
}

/// Shares the same private-directory, output, cancellation and resource policy
/// between explicitly reviewed exporters. Builders may only prepare their own
/// inputs inside the supplied directory and must honor the supplied deadline.
pub(crate) fn prepare_staged_media(
    filename_stem: &str,
    upload_video: bool,
    cancellation: &Arc<AtomicBool>,
    mut progress: impl FnMut(u64, Option<u64>),
    max_bytes: u64,
    deadline: Duration,
    build: impl FnOnce(&Path, &mut dyn FnMut(u64, Option<u64>)) -> Result<StagingPlan, String>,
) -> Result<PreparedArchiveMedia, String> {
    check_cancellation(cancellation)?;
    let started = Instant::now();
    let mut builder = tempfile::Builder::new();
    builder.prefix("youta-upload-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let directory = builder
        .tempdir()
        .map_err(|error| format!("Cannot create private staging directory: {error}"))?;
    let plan = build(directory.path(), &mut progress)?;
    check_cancellation(cancellation)?;
    if started.elapsed() >= deadline {
        return Err("Media preparation timed out".to_owned());
    }
    let path = match plan.command {
        Some(command) => supervise_staging_command(
            command,
            plan.expected_output,
            directory.path(),
            cancellation,
            &mut progress,
            max_bytes,
            started,
            deadline,
        )?,
        None => plan
            .expected_output
            .ok_or("Media preparation produced no completed file")?,
    };
    check_staging(directory.path(), max_bytes)?;
    let extension = validate_completed_file(&path, directory.path(), upload_video)?;
    let filename = format!("{filename_stem}.{extension}");
    let bytes = path
        .metadata()
        .map_err(|error| format!("Cannot inspect prepared media: {error}"))?
        .len();
    progress(bytes, Some(bytes));
    check_cancellation(cancellation)?;
    Ok(PreparedArchiveMedia {
        _directory: directory,
        path,
        filename,
    })
}

/// Runs one fixed command off the UI thread, retaining no unbounded output queue.
fn supervise_staging_command(
    mut command: Command,
    expected_output: Option<PathBuf>,
    directory: &Path,
    cancellation: &AtomicBool,
    mut progress: impl FnMut(u64, Option<u64>),
    max_bytes: u64,
    started: Instant,
    deadline: Duration,
) -> Result<PathBuf, String> {
    let child = crate::child_process::supervised(&mut command)
        .spawn()
        .map_err(|error| format!("Cannot start media preparation helper: {error}"))?;
    let mut helper = HelperGuard(child, false);
    let stdout = helper
        .0
        .stdout
        .take()
        .ok_or("Missing media preparation output")?;
    let stderr = helper
        .0
        .stderr
        .take()
        .ok_or("Missing media preparation diagnostics")?;
    let state = Arc::new(Mutex::new(OutputState::default()));
    // These bounded readers cannot make cancellation wait on inherited pipes.
    // Group termination normally closes them; an orphaned Windows descendant
    // can outlive taskkill's parent-chain lookup, so joins must never block.
    let readers = [
        spawn_reader(stdout, true, state.clone())?,
        spawn_reader(stderr, false, state.clone())?,
    ];
    let mut exited = None;
    loop {
        check_cancellation(cancellation)?;
        if started.elapsed() >= deadline {
            return Err("Media preparation timed out".to_owned());
        }
        check_staging(directory, max_bytes)?;
        let (update, error, done) = {
            let mut output = state
                .lock()
                .map_err(|_| "Media preparation reader failed")?;
            (output.progress.take(), output.error.clone(), output.done)
        };
        if let Some(error) = error {
            return Err(error);
        }
        if let Some((bytes, total)) = update {
            progress(bytes, total);
        }
        if exited.is_none()
            && let Some(status) = helper
                .0
                .try_wait()
                .map_err(|error| format!("Cannot inspect media preparation: {error}"))?
        {
            helper.stop();
            exited = Some((status, Instant::now()));
        }
        if let Some((status, at)) = exited {
            if done == 2 {
                if !status.success() {
                    return Err(format!("media preparation helper failed ({status})"));
                }
                break;
            }
            if at.elapsed() > Duration::from_secs(1) {
                return Err("Media preparation output did not close after helper exit".to_owned());
            }
        }
        thread::sleep(SUPERVISOR_POLL);
    }
    for reader in readers {
        if reader.is_finished() {
            let _ = reader.join();
        }
    }
    check_staging(directory, max_bytes)?;
    let reported = state
        .lock()
        .map_err(|_| "Media preparation reader failed")?
        .completed
        .clone();
    match (reported, expected_output) {
        (Some(actual), Some(expected)) if actual != expected => {
            Err("Media preparation reported an unexpected completed file".to_owned())
        }
        (_, Some(expected)) => Ok(expected),
        (Some(actual), None) => Ok(actual),
        (None, None) => Err("Media preparation produced no completed file".to_owned()),
    }
}

/// Only the selected public YouTube identity can reach the fixed download command.
fn validated_source(media: &MediaItem) -> Result<url::Url, String> {
    if media.id.source != SourceKind::YouTube
        || crate::providers::validate_youtube_video_id(&media.id.external_id).is_err()
        || !matches!(parse_youtube_url(&media.webpage_url), Some(LinkTarget::YouTubeVideo { video_id, .. }) if video_id == media.id.external_id)
    {
        return Err("Archive upload requires a matching YouTube video URL and ID".to_owned());
    }
    LinkTarget::YouTubeVideo {
        video_id: media.id.external_id.clone(),
        start_seconds: None,
    }
    .canonical_url()
    .ok_or_else(|| "Invalid YouTube upload source".to_owned())
}

fn staging_command(
    config: &Config,
    source_url: url::Url,
    destination: &Path,
    upload_video: bool,
    max_bytes: u64,
) -> Command {
    provider_staging_command(
        config,
        source_url,
        destination,
        upload_video,
        max_bytes,
        false,
        None,
    )
}

/// Reuses the normal download format policy; an exporter may require final MKV
/// remuxing without changing the existing manual/Archive upload behavior.
pub(crate) fn provider_staging_command(
    config: &Config,
    source_url: url::Url,
    destination: &Path,
    upload_video: bool,
    max_bytes: u64,
    force_mkv: bool,
    match_filter: Option<&str>,
) -> Command {
    let request = DownloadRequest {
        source_url,
        destination: destination.to_owned(),
        format: if upload_video {
            DownloadFormat::BestVideo
        } else {
            DownloadFormat::TranscodeToOpus
        },
        scope: DownloadScope::SingleItem,
        playlist_start: None,
        skip_shorts: false,
        write_thumbnail: false,
        archive_path: None,
    };
    let mut command = build_download_command(
        &YtDlpConfig {
            executable: config.providers.yt_dlp_executable.clone(),
            allow_plugins: false,
            ..YtDlpConfig::default()
        },
        &request,
    );
    if upload_video {
        command.arg("--format-sort").arg(VIDEO_FORMAT_SORT);
    }
    if let Some(filter) = match_filter {
        command.arg("--match-filters").arg(filter);
    }
    if upload_video && force_mkv {
        command
            .arg("--remux-video")
            .arg("mkv")
            .arg("--postprocessor-args")
            .arg("VideoRemuxer+ffmpeg_o:-c copy");
    }
    command
        .arg("--ffmpeg-location")
        .arg(&config.providers.ffmpeg_executable)
        .arg("--encoding")
        .arg("utf-8")
        .arg("--output")
        .arg("media.%(ext)s")
        .arg("--max-filesize")
        .arg(max_bytes.to_string())
        .arg("--")
        .arg(request.source_url.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn check_cancellation(cancellation: &AtomicBool) -> Result<(), String> {
    if cancellation.load(Ordering::Relaxed) {
        Err("Media preparation canceled".to_owned())
    } else {
        Ok(())
    }
}

/// Polling limits aggregate partial/merge files without changing normal download policy.
/// This is a bounded watchdog, not a filesystem quota; writes can overshoot one poll.
fn check_staging(directory: &Path, max_bytes: u64) -> Result<(), String> {
    let mut bytes = 0_u64;
    for (index, entry) in std::fs::read_dir(directory)
        .map_err(|error| format!("Cannot inspect staging directory: {error}"))?
        .enumerate()
    {
        if index >= MAX_STAGING_FILES {
            return Err("Media preparation staging file count limit exceeded".to_owned());
        }
        let path = entry
            .map_err(|error| format!("Cannot inspect staging entry: {error}"))?
            .path();
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            // FFmpeg and yt-dlp rename/remove partial inputs during normal merging.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("Cannot inspect staged file: {error}")),
        };
        if !metadata.file_type().is_file() {
            return Err(
                "Media output must be a regular file in the private staging directory".to_owned(),
            );
        }
        bytes = bytes
            .checked_add(metadata.len())
            .ok_or("Media preparation staging size limit exceeded")?;
        if bytes > max_bytes {
            return Err(
                "Media preparation staging size limit exceeded (including partial and merge files)"
                    .to_owned(),
            );
        }
    }
    Ok(())
}

fn validate_completed_file(
    path: &Path,
    directory: &Path,
    upload_video: bool,
) -> Result<String, String> {
    if path.parent() != Some(directory) {
        return Err("Media output escaped the private staging directory".to_owned());
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("Cannot inspect completed media: {error}"))?;
    if !metadata.file_type().is_file() || metadata.len() == 0 {
        return Err(
            "Media output must be a nonempty regular file in the private staging directory"
                .to_owned(),
        );
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if if upload_video {
        !matches!(extension, "mkv" | "mp4" | "webm")
    } else {
        extension != "opus"
    } {
        return Err("Media preparation produced an unexpected output format".to_owned());
    }
    Ok(extension.to_owned())
}

fn spawn_reader(
    reader: impl Read + Send + 'static,
    progress_stream: bool,
    state: Arc<Mutex<OutputState>>,
) -> Result<thread::JoinHandle<()>, String> {
    thread::Builder::new()
        .name("archive-upload-output".to_owned())
        .spawn(move || {
            let result = read_output(reader, progress_stream, &state);
            if let Ok(mut output) = state.lock() {
                if let Err(error) = result {
                    output.error.get_or_insert(error);
                }
                output.done += 1;
            }
        })
        .map_err(|error| format!("Cannot start media preparation reader: {error}"))
}

/// Both streams are drained in small chunks; only progress lines are interpreted.
fn read_output(
    mut reader: impl Read,
    progress_stream: bool,
    state: &Mutex<OutputState>,
) -> Result<(), String> {
    let mut chunk = [0_u8; 4096];
    let mut line = Vec::new();
    loop {
        let count = match reader.read(&mut chunk) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("Cannot read media preparation output: {error}")),
        };
        {
            let mut output = state
                .lock()
                .map_err(|_| "Media preparation reader failed")?;
            output.bytes = output.bytes.saturating_add(count);
            if output.bytes > MAX_OUTPUT_BYTES {
                return Err("Media preparation output limit exceeded".to_owned());
            }
        }
        if count == 0 {
            if progress_stream && !line.is_empty() {
                consume_line(&line, state)?;
            }
            return Ok(());
        }
        for byte in &chunk[..count] {
            if *byte == b'\n' || *byte == b'\r' {
                if progress_stream {
                    consume_line(&line, state)?;
                }
                line.clear();
            } else {
                if line.len() >= MAX_LINE_BYTES {
                    return Err("Media preparation output limit exceeded".to_owned());
                }
                line.push(*byte);
            }
        }
    }
}

fn consume_line(line: &[u8], state: &Mutex<OutputState>) -> Result<(), String> {
    let text =
        std::str::from_utf8(line).map_err(|_| "Media preparation output is not valid UTF-8")?;
    let mut output = state
        .lock()
        .map_err(|_| "Media preparation reader failed")?;
    match parse_download_event(text) {
        Some(DownloadEvent::Progress {
            downloaded_bytes,
            total_bytes,
            ..
        }) => {
            output.progress = Some((downloaded_bytes, total_bytes));
        }
        Some(DownloadEvent::CompletedFile(path)) => {
            if output
                .completed
                .as_ref()
                .is_some_and(|existing| existing != &path)
            {
                return Err("Media preparation produced more than one output file".to_owned());
            }
            output.completed = Some(path);
        }
        None => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{MediaId, MediaKind, MediaLicense, MediaStatistics, SourceKind};

    fn media() -> MediaItem {
        MediaItem {
            id: MediaId::new(SourceKind::YouTube, "BaW_jenozKc"),
            kind: MediaKind::Video,
            title: "An upload fixture".to_owned(),
            creator: None,
            description: None,
            webpage_url: url::Url::parse("https://www.youtube.com/watch?v=BaW_jenozKc").unwrap(),
            thumbnail_url: None,
            duration_seconds: None,
            published_at: None,
            statistics: MediaStatistics::default(),
            license: MediaLicense::Unknown,
            chapters: Vec::new(),
            captions: Vec::new(),
        }
    }

    /// Publishes a fresh closed script and checks readiness without running its body.
    #[cfg(unix)]
    fn mock_config(directory: &Path, body: &str) -> Config {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        const READY_ARGUMENT: &str = "__youta_archive_fixture_ready__";
        let mut fixture = tempfile::Builder::new()
            .prefix("mock-yt-dlp-")
            .tempfile_in(directory)
            .unwrap();
        fixture.write_all(format!("#!/bin/sh\nif [ \"${{1-}}\" = '{READY_ARGUMENT}' ]; then exit 0; fi\nset -eu\noutput=\nwhile [ $# -gt 0 ]; do\n\tif [ \"$1\" = --paths ]; then shift; output=$1; fi\n\tshift\ndone\n{body}\n").as_bytes()).unwrap();
        fixture.as_file().sync_all().unwrap();
        fixture
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o700))
            .unwrap();
        // Closing the writer before publishing a unique path avoids rewriting
        // an inode still held by another fixture or an instrumented child.
        let helper = fixture.into_temp_path().keep().unwrap();
        let started = Instant::now();
        loop {
            match Command::new(&helper)
                .arg(READY_ARGUMENT)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
            {
                Ok(status) => {
                    assert!(status.success(), "mock helper readiness failed: {status}");
                    break;
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                        && started.elapsed() < Duration::from_secs(1) =>
                {
                    // Yield only for the observed fixture-publication race;
                    // production spawn failures retain their original policy.
                    thread::yield_now();
                }
                Err(error) => panic!("mock helper did not become executable: {error}"),
            }
        }
        let mut config = Config::for_dir(directory.join("config"));
        config.providers.yt_dlp_executable = helper;
        config.providers.ffmpeg_executable = directory.join("configured-ffmpeg");
        config
    }

    #[cfg(unix)]
    #[test]
    fn prepared_audio_is_private_and_removed_with_its_guard() {
        use std::os::unix::fs::PermissionsExt;
        let temporary = tempfile::tempdir().unwrap();
        let config = mock_config(
            temporary.path(),
            "printf 'fixture opus' > \"$output/media.opus\"\nprintf 'youta-progress|12|12|12|1|0\nyouta-file|%s/media.opus\n' \"$output\"",
        );
        let mut updates = Vec::new();
        let prepared = prepare_archive_media(
            &config,
            &media(),
            false,
            &Arc::new(AtomicBool::new(false)),
            |done, total| updates.push((done, total)),
        )
        .unwrap();
        assert_eq!(std::fs::read(prepared.path()).unwrap(), b"fixture opus");
        assert_eq!(prepared.filename(), "BaW_jenozKc.opus");
        assert_eq!(
            prepared
                ._directory
                .path()
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0
        );
        assert!(updates.iter().any(|(done, _)| *done == 12));
        let directory = prepared._directory.path().to_owned();
        drop(prepared);
        assert!(!directory.exists());
    }

    #[cfg(unix)]
    #[test]
    fn staging_rejects_oversized_files_and_reports_the_limit() {
        let temporary = tempfile::tempdir().unwrap();
        let config = mock_config(
            temporary.path(),
            "printf 'too many bytes' > \"$output/media.opus\"\nprintf 'youta-file|%s/media.opus\n' \"$output\"",
        );
        let error = prepare_with_limits(
            &config,
            &media(),
            false,
            &Arc::new(AtomicBool::new(false)),
            |_, _| {},
            4,
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(error.contains("staging size limit"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn staging_rejects_escaped_or_symlinked_completed_paths() {
        let temporary = tempfile::tempdir().unwrap();
        for body in [
            "printf 'youta-file|/etc/passwd\n'",
            "ln -s /etc/passwd \"$output/media.opus\"; printf 'youta-file|%s/media.opus\n' \"$output\"",
        ] {
            let config = mock_config(temporary.path(), body);
            let error = prepare_archive_media(
                &config,
                &media(),
                false,
                &Arc::new(AtomicBool::new(false)),
                |_, _| {},
            )
            .unwrap_err();
            assert!(error.contains("private staging directory"), "{error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn silent_helper_and_descendant_are_killed_at_the_preparation_deadline() {
        let temporary = tempfile::tempdir().unwrap();
        let config = mock_config(temporary.path(), "sleep 30 &\nwait");
        let started = std::time::Instant::now();
        let error = prepare_with_limits(
            &config,
            &media(),
            false,
            &Arc::new(AtomicBool::new(false)),
            |_, _| {},
            1024,
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "descendant pipes must not hold cleanup open"
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_without_line_boundaries_cannot_grow_without_limit() {
        let temporary = tempfile::tempdir().unwrap();
        let config = mock_config(temporary.path(), "printf '%100000d' 0");
        let error = prepare_with_limits(
            &config,
            &media(),
            false,
            &Arc::new(AtomicBool::new(false)),
            |_, _| {},
            1024,
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(error.contains("output limit"), "{error}");
    }

    /// Upload video preferences never force a lower-resolution open-codec format.
    #[test]
    fn upload_commands_preserve_quality_and_use_configured_ffmpeg() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config::for_dir(directory.path().join("config"));
        config.providers.ffmpeg_executable = directory.path().join("custom ffmpeg");
        for video in [false, true] {
            let command =
                staging_command(&config, media().webpage_url, directory.path(), video, 1234);
            let arguments: Vec<_> = command
                .get_args()
                .map(|value| value.to_string_lossy().into_owned())
                .collect();
            let pair = |option: &str, value: &str| {
                arguments.windows(2).any(|args| args == [option, value])
            };
            assert!(pair(
                "--ffmpeg-location",
                config.providers.ffmpeg_executable.to_str().unwrap()
            ));
            assert!(pair("--output", "media.%(ext)s"));
            assert!(pair("--max-filesize", "1234"));
            assert!(arguments.iter().any(|value| value == "--no-playlist"));
            assert!(arguments.iter().any(|value| value == "--ignore-config"));
            assert!(arguments.iter().any(|value| value == "--no-plugin-dirs"));
            assert!(!arguments.iter().any(|value| value == "--recode-video"));
            if video {
                assert!(pair("--format", "bestvideo+bestaudio/best"));
                assert!(pair(
                    "--format-sort",
                    "res,fps,hdr:12,vcodec:av1,channels,acodec:opus"
                ));
                assert!(pair("--merge-output-format", "mkv"));
                assert!(pair("--postprocessor-args", "Merger+ffmpeg_o:-c copy"));
                assert!(pair("--fixup", "never"));
                assert!(!arguments.iter().any(|value| value == "--extract-audio"));
            } else {
                assert!(pair("--format", "bestaudio"));
                assert!(pair("--audio-format", "opus"));
                assert!(!arguments.iter().any(|value| value == "--format-sort"));
            }
        }
    }

    #[test]
    fn staging_rejects_mismatched_sources_and_canonicalizes_youtube_links() {
        let mut selected = media();
        selected.webpage_url =
            url::Url::parse("https://youtu.be/BaW_jenozKc?t=25&list=private-query").unwrap();
        assert_eq!(
            validated_source(&selected).unwrap().as_str(),
            "https://www.youtube.com/watch?v=BaW_jenozKc"
        );
        for source in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://example.test/watch?v=BaW_jenozKc",
            "https://name:secret@www.youtube.com/watch?v=BaW_jenozKc",
            "file:///tmp/video",
        ] {
            selected.webpage_url = url::Url::parse(source).unwrap();
            assert!(validated_source(&selected).is_err(), "{source}");
        }
        selected = media();
        selected.id.source = SourceKind::Local;
        assert!(validated_source(&selected).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn active_cancel_terminates_silent_helper_and_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let config = mock_config(directory.path(), "sleep 30 &\nwait");
        let cancellation = Arc::new(AtomicBool::new(false));
        let cancel = cancellation.clone();
        let signal = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            cancel.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let error =
            prepare_archive_media(&config, &media(), false, &cancellation, |_, _| {}).unwrap_err();
        signal.join().unwrap();
        assert!(error.contains("canceled"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn staging_rejects_failed_empty_missing_and_duplicate_outputs() {
        let directory = tempfile::tempdir().unwrap();
        for (body, expected) in [
            ("exit 1", "failed"),
            ("true", "no completed file"),
            (
                "touch \"$output/media.opus\"; printf 'youta-file|%s/media.opus\\n' \"$output\"",
                "nonempty",
            ),
            (
                "printf x > \"$output/media.mp3\"; printf 'youta-file|%s/media.mp3\\n' \"$output\"",
                "unexpected output format",
            ),
            (
                "printf 'youta-file|%s/a.opus\\nyouta-file|%s/b.opus\\n' \"$output\" \"$output\"",
                "more than one",
            ),
        ] {
            let config = mock_config(directory.path(), body);
            let error = prepare_archive_media(
                &config,
                &media(),
                false,
                &Arc::new(AtomicBool::new(false)),
                |_, _| {},
            )
            .unwrap_err();
            assert!(error.contains(expected), "{body}: {error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_during_final_progress_does_not_return_prepared_media() {
        let directory = tempfile::tempdir().unwrap();
        let config = mock_config(
            directory.path(),
            "printf x > \"$output/media.opus\"; printf 'youta-file|%s/media.opus\\n' \"$output\"",
        );
        let cancellation = Arc::new(AtomicBool::new(false));
        let error = prepare_archive_media(&config, &media(), false, &cancellation, |_, _| {
            cancellation.store(true, Ordering::Relaxed);
        })
        .unwrap_err();
        assert!(error.contains("canceled"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn staging_video_retains_original_container_bytes() {
        let directory = tempfile::tempdir().unwrap();
        for extension in ["mkv", "mp4", "webm"] {
            let body = format!(
                "printf 'video bytes' > \"$output/media.{extension}\"; printf 'youta-file|%s/media.{extension}\\n' \"$output\""
            );
            let config = mock_config(directory.path(), &body);
            let prepared = prepare_archive_media(
                &config,
                &media(),
                true,
                &Arc::new(AtomicBool::new(false)),
                |_, _| {},
            )
            .unwrap();
            assert_eq!(std::fs::read(prepared.path()).unwrap(), b"video bytes");
            assert_eq!(prepared.filename(), format!("BaW_jenozKc.{extension}"));
        }
    }

    /// Reusing a script inode can retain another writer's Linux executable lock.
    #[cfg(target_os = "linux")]
    #[test]
    fn successive_mock_helpers_do_not_reuse_a_write_locked_executable() {
        let directory = tempfile::tempdir().unwrap();
        let previous = mock_config(directory.path(), "exit 99");
        let writer = std::fs::OpenOptions::new()
            .write(true)
            .open(&previous.providers.yt_dlp_executable)
            .unwrap();
        let config = mock_config(
            directory.path(),
            "printf 'video bytes' > \"$output/media.mkv\"; printf 'youta-file|%s/media.mkv\\n' \"$output\"",
        );
        let prepared = prepare_archive_media(
            &config,
            &media(),
            true,
            &Arc::new(AtomicBool::new(false)),
            |_, _| {},
        )
        .expect("the next mock must not execute the previous fixture's write-locked inode");
        drop(writer);
        assert_eq!(std::fs::read(prepared.path()).unwrap(), b"video bytes");
        assert_eq!(prepared.filename(), "BaW_jenozKc.mkv");
    }

    #[test]
    fn precancelled_preparation_does_not_start_any_helper() {
        let temporary = tempfile::tempdir().unwrap();
        let mut config = Config::for_dir(temporary.path().join("config"));
        config.providers.yt_dlp_executable = PathBuf::from("must-not-start-this-helper");
        let error = prepare_archive_media(
            &config,
            &media(),
            false,
            &Arc::new(AtomicBool::new(true)),
            |_, _| {},
        )
        .unwrap_err();
        assert!(error.contains("canceled"), "{error}");
    }
}
