//! Private audio staging shared by explicit remote and local-file exports.
//!
//! Public pages are delegated to the user's configured `yt-dlp`; authenticated
//! Yandex Music tracks retain Youta's native resolver and fetcher. Local files
//! are copied or transcoded without a downloader. Every output is confined to
//! a caller-owned private directory and normalized to Opus.

use std::fs;
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(feature = "yandex-music")]
use std::sync::Arc;
#[cfg(feature = "yandex-music")]
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::Duration;

use url::Url;

use crate::playback::ytdlp::{
    DownloadEvent, DownloadFormat, DownloadRequest, YtDlp, YtDlpConfig, parse_download_event,
};
#[cfg(feature = "yandex-music")]
use crate::providers::yandex_music::YandexMusicClient;
#[cfg(feature = "yandex-music")]
use crate::providers::yandex_music_media::YandexMusicMediaFetcher;

const MAXIMUM_PREPARATION_DIAGNOSTIC_BYTES: usize = 64 * 1024;

/// Provider media locator retained privately while an export is reviewed.
#[derive(Clone, Debug)]
pub enum OpusAudioSource {
    /// Credential-free public page or direct media URL handled by `yt-dlp`.
    PublicPage(Url),
    /// Authenticated Yandex Music track resolved through Youta's native client.
    YandexMusic {
        /// Stable track identifier.
        track_id: String,
        /// OAuth token copied only into the worker.
        token: String,
    },
    /// Existing local media copied or transcoded through the configured `FFmpeg`.
    LocalFile(PathBuf),
}

/// Copies, downloads, or transcodes selected media into one private Opus file.
///
/// The caller owns `staging_directory`, which must not already exist. Completed
/// output remains inside that directory so it can be removed after export.
///
/// # Errors
///
/// Returns an explanation when the private staging directory cannot be
/// created, provider audio cannot be fetched, transcoding fails, or the
/// resulting path does not identify an Opus file inside the staging directory.
pub fn prepare_provider_opus(
    source: &OpusAudioSource,
    staging_directory: &Path,
    yt_dlp_executable: &Path,
    ffmpeg_executable: &Path,
    progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf, String> {
    crate::private_files::create_private_directory(staging_directory)
        .map_err(|error| format!("Cannot create the private audio staging directory: {error}"))?;
    match source {
        OpusAudioSource::PublicPage(source_url) => {
            prepare_public_page_opus(source_url, staging_directory, yt_dlp_executable, progress)
        }
        OpusAudioSource::YandexMusic { track_id, token } => {
            #[cfg(feature = "yandex-music")]
            {
                prepare_yandex_music_opus(
                    track_id,
                    token,
                    staging_directory,
                    ffmpeg_executable,
                    progress,
                )
            }
            #[cfg(not(feature = "yandex-music"))]
            {
                let _ = (track_id, token, ffmpeg_executable, progress);
                Err("This build omits Yandex Music support".to_owned())
            }
        }
        OpusAudioSource::LocalFile(path) => {
            prepare_local_file_opus(path, staging_directory, ffmpeg_executable, progress)
        }
    }
}

fn prepare_local_file_opus(
    source: &Path,
    staging_directory: &Path,
    ffmpeg_executable: &Path,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf, String> {
    let source = fs::canonicalize(source)
        .map_err(|error| format!("Cannot resolve the selected local audio: {error}"))?;
    let metadata = fs::metadata(&source)
        .map_err(|error| format!("Cannot inspect the selected local audio: {error}"))?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err("The selected local audio must be a non-empty regular file".to_owned());
    }
    let output = staging_directory.join("audio.opus");
    if source
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("opus"))
    {
        fs::copy(&source, &output)
            .map_err(|error| format!("Could not stage the selected local Opus file: {error}"))?;
        progress(metadata.len(), Some(metadata.len()));
        return validate_staged_opus_path(staging_directory, output);
    }

    let command = Command::new(ffmpeg_executable)
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-n", "-i"])
        .arg(&source)
        .args(["-vn", "-c:a", "libopus", "-f", "opus"])
        .arg(&output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("Could not start FFmpeg for local audio: {error}"))?;
    if !command.status.success() {
        let diagnostics = String::from_utf8_lossy(&command.stderr);
        return Err(format!(
            "FFmpeg could not create Opus audio from the local file: {}",
            diagnostics.trim()
        ));
    }
    let output_bytes = fs::metadata(&output)
        .map_err(|error| format!("Cannot inspect the prepared local Opus audio: {error}"))?
        .len();
    progress(output_bytes, Some(output_bytes));
    validate_staged_opus_path(staging_directory, output)
}

fn prepare_public_page_opus(
    source_url: &Url,
    staging_directory: &Path,
    yt_dlp_executable: &Path,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf, String> {
    let client = YtDlp::new(YtDlpConfig {
        executable: yt_dlp_executable.to_owned(),
        allow_plugins: false,
        ..YtDlpConfig::default()
    });
    let mut process = client
        .download(&DownloadRequest {
            source_url: source_url.clone(),
            destination: staging_directory.to_owned(),
            format: DownloadFormat::TranscodeToOpus,
            write_thumbnail: false,
        })
        .map_err(|error| format!("Could not start yt-dlp audio preparation: {error}"))?;
    let stderr = process.take_error_reader();
    let stderr_thread = stderr.map(|mut stderr| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr
                .by_ref()
                .take(MAXIMUM_PREPARATION_DIAGNOSTIC_BYTES as u64)
                .read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).trim().to_owned()
        })
    });
    let mut completed_path = None;
    if let Some(reader) = process.take_progress_reader() {
        for line in reader.lines() {
            let line = line.map_err(|error| format!("Could not read yt-dlp progress: {error}"))?;
            match parse_download_event(&line) {
                Some(DownloadEvent::Progress {
                    downloaded_bytes,
                    total_bytes,
                    ..
                }) => progress(downloaded_bytes, total_bytes),
                Some(DownloadEvent::CompletedFile(path)) => completed_path = Some(path),
                None => {}
            }
        }
    }
    let status = loop {
        match process
            .try_wait()
            .map_err(|error| format!("Could not monitor yt-dlp: {error}"))?
        {
            Some(status) => break status,
            None => thread::sleep(Duration::from_millis(10)),
        }
    };
    let diagnostics = stderr_thread
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    if !status.success() {
        return Err(if diagnostics.is_empty() {
            format!("yt-dlp audio preparation exited with {status}")
        } else {
            format!("yt-dlp audio preparation exited with {status}: {diagnostics}")
        });
    }
    let path =
        completed_path.ok_or_else(|| "yt-dlp did not report the prepared Opus path".to_owned())?;
    validate_staged_opus_path(staging_directory, path)
}

#[cfg(feature = "yandex-music")]
fn prepare_yandex_music_opus(
    track_id: &str,
    token: &str,
    staging_directory: &Path,
    ffmpeg_executable: &Path,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf, String> {
    let client = YandexMusicClient::new(token.to_owned()).map_err(|error| error.to_string())?;
    let media = client
        .resolve_media(track_id)
        .map_err(|error| format!("Could not resolve Yandex Music audio: {error}"))?;
    let extension = media.codec.file_extension();
    let original = staging_directory.join(format!("source.{extension}"));
    let cancellation = Arc::new(AtomicBool::new(false));
    YandexMusicMediaFetcher::default()
        .fetch_with_progress_and_cancellation(&media, &original, &cancellation, |update| {
            progress(update.bytes_written, update.total_bytes);
        })
        .map_err(|error| format!("Could not fetch Yandex Music audio: {error}"))?;
    if extension.eq_ignore_ascii_case("opus") {
        return validate_staged_opus_path(staging_directory, original);
    }
    let output = staging_directory.join("audio.opus");
    let command = Command::new(ffmpeg_executable)
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-n", "-i"])
        .arg(&original)
        .args(["-vn", "-c:a", "libopus", "-f", "opus"])
        .arg(&output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("Could not start FFmpeg for Yandex Music audio: {error}"))?;
    if !command.status.success() {
        let diagnostics = String::from_utf8_lossy(&command.stderr);
        return Err(format!(
            "FFmpeg could not create Opus audio: {}",
            diagnostics.trim()
        ));
    }
    validate_staged_opus_path(staging_directory, output)
}

fn validate_staged_opus_path(directory: &Path, path: PathBuf) -> Result<PathBuf, String> {
    let directory = crate::fs_path::canonicalize(directory)
        .map_err(|error| format!("Cannot resolve the audio staging directory: {error}"))?;
    let path = crate::fs_path::canonicalize(path)
        .map_err(|error| format!("Cannot resolve the prepared Opus file: {error}"))?;
    if path.parent() != Some(directory.as_path())
        || path.extension().and_then(|extension| extension.to_str()) != Some("opus")
        || !path.is_file()
    {
        return Err("The prepared Opus path escaped its private staging directory".to_owned());
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::io::Write as _;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn local_opus_is_copied_into_private_staging_without_a_downloader() {
        let temporary = crate::test_support::canonical_tempdir("temporary directory");
        let source = temporary.path().join("field-recording.opus");
        fs::write(&source, b"fixture opus bytes").expect("local Opus fixture");
        let staging = temporary.path().join("staging");
        let mut progress = Vec::new();

        let prepared = prepare_provider_opus(
            &OpusAudioSource::LocalFile(source),
            &staging,
            Path::new("yt-dlp-must-not-run"),
            Path::new("ffmpeg-must-not-run"),
            |written, total| progress.push((written, total)),
        )
        .expect("staged local Opus");

        assert_eq!(prepared.parent(), Some(staging.as_path()));
        assert_eq!(
            fs::read(prepared).expect("prepared bytes"),
            b"fixture opus bytes"
        );
        assert_eq!(progress, vec![(18, Some(18))]);
    }

    #[cfg(unix)]
    #[test]
    fn local_non_opus_media_is_transcoded_with_ffmpeg() {
        let temporary = crate::test_support::canonical_tempdir("temporary directory");
        let source = temporary.path().join("field-recording.flac");
        fs::write(&source, b"fixture source bytes").expect("local audio fixture");
        let ffmpeg = temporary.path().join("fixture-ffmpeg");
        let writing_ffmpeg = temporary.path().join("fixture-ffmpeg.writing");
        {
            let mut fixture = fs::File::create(&writing_ffmpeg).expect("fixture FFmpeg staging");
            fixture
                .write_all(
                    b"#!/bin/sh\n\
if [ \"${1-}\" = '__youta_mock_ffmpeg_ready__' ]; then exit 0; fi\n\
cp \"$7\" \"${13}\"\n",
                )
                .expect("fixture FFmpeg contents");
            fixture.sync_all().expect("sync fixture FFmpeg");
        }
        fs::set_permissions(&writing_ffmpeg, fs::Permissions::from_mode(0o700))
            .expect("executable fixture FFmpeg");
        fs::rename(&writing_ffmpeg, &ffmpeg).expect("publish closed fixture FFmpeg atomically");

        // Parallel instrumented tests and security tooling can briefly retain
        // a write handle after publication. Prove the script is executable so
        // an ETXTBSY fixture race cannot be mistaken for an export failure.
        let mut readiness_error = None;
        for _ in 0..50 {
            match Command::new(&ffmpeg)
                .arg("__youta_mock_ffmpeg_ready__")
                .status()
            {
                Ok(status) if status.success() => {
                    readiness_error = None;
                    break;
                }
                Ok(status) => panic!("fixture FFmpeg readiness exited with {status}"),
                Err(error) => {
                    readiness_error = Some(error);
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
        if let Some(error) = readiness_error {
            panic!("fixture FFmpeg stayed busy after publication: {error}");
        }
        let staging = temporary.path().join("staging");
        let mut progress = Vec::new();

        let prepared = prepare_provider_opus(
            &OpusAudioSource::LocalFile(source),
            &staging,
            Path::new("yt-dlp-must-not-run"),
            &ffmpeg,
            |written, total| progress.push((written, total)),
        )
        .expect("transcoded local audio");

        assert_eq!(
            fs::read(prepared).expect("prepared bytes"),
            b"fixture source bytes"
        );
        assert_eq!(progress, vec![(20, Some(20))]);
    }
}
