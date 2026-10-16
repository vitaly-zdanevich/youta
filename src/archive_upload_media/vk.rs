//! Native, bounded VK audio staging with private credentials and canonical provenance.

use std::path::Path;
#[cfg(feature = "controller")]
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use url::Url;

use super::{PreparedArchiveMedia, StagingPlan};
use crate::config::Config;
use crate::domain::{MediaItem, MediaKind, SourceKind};
use crate::providers::vk::{VkAudioId, VkClient, VkPage, VkPlaybackUrl, VkTrack};

/// One selected recording and the authenticated client that owns its private lookup key.
///
/// This snapshot stays in memory and is never serializable. The native resolver
/// verifies the track's session scope before requesting a fresh media locator.
#[derive(Clone)]
pub struct VkExportSource {
    client: VkClient,
    track: VkTrack,
}

impl VkExportSource {
    /// Captures an already authenticated client and its exact selected catalogue row.
    #[must_use]
    pub fn new(client: VkClient, track: VkTrack) -> Self {
        Self { client, track }
    }

    /// Returns only the canonical public identity, never a private reload or media URL.
    #[must_use]
    pub fn webpage_url(&self) -> Url {
        self.track.id.webpage_url()
    }
}

impl std::fmt::Debug for VkExportSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VkExportSource")
            .field("id", &self.track.id.to_string())
            .finish_non_exhaustive()
    }
}

/// Keeps request sequencing independently testable without reading cookies or using a network.
trait AudioResolver {
    fn authenticate(&mut self) -> Result<(), String>;
    fn search(&mut self, query: &str) -> Result<VkPage, String>;
    fn resolve(&mut self, track: &VkTrack) -> Result<VkPlaybackUrl, String>;
}

impl AudioResolver for VkClient {
    fn authenticate(&mut self) -> Result<(), String> {
        VkClient::authenticate(self).map_err(|error| error.to_string())
    }

    fn search(&mut self, query: &str) -> Result<VkPage, String> {
        VkClient::search(self, query).map_err(|error| error.to_string())
    }

    fn resolve(&mut self, track: &VkTrack) -> Result<VkPlaybackUrl, String> {
        VkClient::resolve(self, track).map_err(|error| error.to_string())
    }
}

/// Dropping the waiter revokes later requests even if a bounded HTTP call is still finishing.
struct ResolutionGuard(Arc<AtomicBool>);

impl Drop for ResolutionGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Stages the exact reviewed recording directly from its retained authenticated session.
///
/// Unlike queue-only recovery, this path performs no catalogue search and reads
/// no cookie file. The canonical public provenance must still match the review.
///
/// # Errors
/// Rejects changed identities, incompatible sessions, unavailable media,
/// cancellation, helper errors and shared staging resource limits.
pub fn prepare_vk_archive_media_from_session(
    config: &Config,
    media: &MediaItem,
    playback_location: &str,
    upload_video: bool,
    cancellation: &Arc<AtomicBool>,
    progress: impl FnMut(u64, Option<u64>),
    source: &VkExportSource,
) -> Result<PreparedArchiveMedia, String> {
    if source.webpage_url() != media.webpage_url {
        return Err("The retained VK recording no longer matches the reviewed source".to_owned());
    }
    let started = Instant::now();
    prepare_with_resolver(
        config,
        media,
        playback_location,
        upload_video,
        cancellation,
        progress,
        || resolve_session_audio(source, cancellation, started, super::MAX_PREPARATION_TIME),
    )
}

/// Prepares the reviewed VK audio through its native authenticated resolver.
///
/// Only the explicitly configured cookie file is read. A bounded fresh catalogue
/// lookup must prove the exact selected identity, and only its vetted CDN locator
/// reaches FFmpeg. Cookies and private lookup hashes never leave the VK client.
///
/// # Errors
/// Rejects changed identities, video requests, unavailable credentials or audio,
/// cancellation, helper errors and shared staging resource limits.
pub fn prepare_vk_archive_media(
    config: &Config,
    media: &MediaItem,
    playback_location: &str,
    upload_video: bool,
    cancellation: &Arc<AtomicBool>,
    progress: impl FnMut(u64, Option<u64>),
) -> Result<PreparedArchiveMedia, String> {
    let started = Instant::now();
    prepare_with_resolver(
        config,
        media,
        playback_location,
        upload_video,
        cancellation,
        progress,
        || {
            let cookie_file = config
                .providers
                .vk_cookies_file
                .as_deref()
                .ok_or("Configure an explicit VK cookie file before exporting audio")?;
            resolve_audio(
                &media.webpage_url,
                &media.title,
                media.creator.as_deref().unwrap_or_default(),
                cookie_file,
                cancellation,
                started,
                super::MAX_PREPARATION_TIME,
            )
        },
    )
}

/// Validates the immutable selection before a resolver or helper can perform work.
fn prepare_with_resolver(
    config: &Config,
    media: &MediaItem,
    playback_location: &str,
    upload_video: bool,
    cancellation: &Arc<AtomicBool>,
    progress: impl FnMut(u64, Option<u64>),
    resolve: impl FnOnce() -> Result<VkPlaybackUrl, String>,
) -> Result<PreparedArchiveMedia, String> {
    super::local::check_work(cancellation, Instant::now(), super::MAX_PREPARATION_TIME)?;
    let id = canonical_source(&media.webpage_url)?;
    if media.id.source != SourceKind::Vk
        || media.kind != MediaKind::Audio
        || upload_video
        || media.id.external_id != id.to_string()
        || playback_location != media.webpage_url.as_str()
    {
        return Err("VK exports require one matching canonical audio selection".to_owned());
    }
    super::prepare_staged_media(
        "media",
        false,
        cancellation,
        progress,
        super::MAX_STAGING_BYTES,
        super::MAX_PREPARATION_TIME,
        |directory, _| {
            let audio = resolve()?;
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

/// Stages one explicitly requested VK source in its caller's private directory.
///
/// This queue-only recovery path is shared by Commons and Evernote. Selected
/// recordings use their retained provider session instead of searching again.
#[cfg(any(feature = "commons-upload", feature = "evernote"))]
pub(crate) fn prepare_opus_in_directory(
    source: &Url,
    title: &str,
    artist: &str,
    cookie_file: &Path,
    directory: &Path,
    ffmpeg_executable: &Path,
    progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf, String> {
    prepare_bounded_opus(
        source,
        title,
        artist,
        cookie_file,
        directory,
        ffmpeg_executable,
        &AtomicBool::new(false),
        super::MAX_STAGING_BYTES,
        super::MAX_PREPARATION_TIME,
        progress,
    )
}

/// Applies shared staging limits to queue-only recovery for audio exports.
#[cfg(any(feature = "commons-upload", feature = "evernote"))]
pub(crate) fn prepare_bounded_opus(
    source: &Url,
    title: &str,
    artist: &str,
    cookie_file: &Path,
    directory: &Path,
    ffmpeg_executable: &Path,
    cancellation: &AtomicBool,
    max_bytes: u64,
    deadline: Duration,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf, String> {
    stage_bounded_opus(
        directory,
        ffmpeg_executable,
        cancellation,
        max_bytes,
        deadline,
        &mut progress,
        |started| {
            resolve_audio(
                source,
                title,
                artist,
                cookie_file,
                cancellation,
                started,
                deadline,
            )
        },
    )
}

/// Identifies or exports the selected session-owned recording without a new global search.
#[cfg(any(
    feature = "commons-upload",
    feature = "evernote",
    all(feature = "controller", feature = "acoustid")
))]
pub(crate) fn prepare_bounded_session_opus(
    source: &VkExportSource,
    directory: &Path,
    ffmpeg_executable: &Path,
    cancellation: &AtomicBool,
    max_bytes: u64,
    deadline: Duration,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf, String> {
    stage_bounded_opus(
        directory,
        ffmpeg_executable,
        cancellation,
        max_bytes,
        deadline,
        &mut progress,
        |started| resolve_session_audio(source, cancellation, started, deadline),
    )
}

/// Shares bounded, cookie-free FFmpeg supervision across native VK source variants.
#[cfg(any(
    feature = "commons-upload",
    feature = "evernote",
    all(feature = "controller", feature = "acoustid")
))]
fn stage_bounded_opus(
    directory: &Path,
    ffmpeg_executable: &Path,
    cancellation: &AtomicBool,
    max_bytes: u64,
    deadline: Duration,
    mut progress: impl FnMut(u64, Option<u64>),
    resolve: impl FnOnce(Instant) -> Result<VkPlaybackUrl, String>,
) -> Result<PathBuf, String> {
    let started = Instant::now();
    super::local::check_work(cancellation, started, deadline)?;
    let audio = resolve(started)?;
    super::local::check_work(cancellation, started, deadline)?;
    let output = directory.join("audio.opus");
    let path = super::supervise_staging_command(
        ffmpeg_command(ffmpeg_executable, &audio, &output),
        Some(output),
        directory,
        cancellation,
        &mut progress,
        max_bytes,
        started,
        deadline,
    )?;
    super::validate_completed_file(&path, directory, false)?;
    let bytes = path
        .metadata()
        .map_err(|_| "Cannot inspect prepared VK audio")?
        .len();
    progress(bytes, Some(bytes));
    super::local::check_work(cancellation, started, deadline)?;
    Ok(path)
}

/// Public provenance must use the exact canonical page and cannot contain secrets.
pub(crate) fn canonical_source(source: &Url) -> Result<VkAudioId, String> {
    let id = VkAudioId::from_page_url(source)
        .map_err(|_| "VK export requires a canonical audio page".to_owned())?;
    if id.webpage_url() != *source {
        return Err("VK export requires a canonical audio page".to_owned());
    }
    Ok(id)
}

/// Refreshes only one exact identity using an explicitly selected cookie file.
fn resolve_audio(
    source: &Url,
    title: &str,
    artist: &str,
    cookie_file: &Path,
    cancellation: &AtomicBool,
    started: Instant,
    deadline: Duration,
) -> Result<VkPlaybackUrl, String> {
    let id = canonical_source(source)?;
    let deadline = deadline.min(started.elapsed().saturating_add(Duration::from_secs(120)));
    super::local::check_work(cancellation, started, deadline)?;
    if !cookie_file.is_absolute() {
        return Err("VK audio export requires an explicit absolute cookie-file path".to_owned());
    }
    let query = format!("{title} {artist}")
        .chars()
        .filter(|value| !value.is_control())
        .take(256)
        .collect::<String>();
    if query.trim().is_empty() {
        return Err("VK audio export requires the selected track's title or artist".to_owned());
    }
    let cookie_file = cookie_file.to_owned();
    wait_for_resolution(
        cancellation,
        started,
        deadline,
        move |cancelled, started, deadline| {
            super::local::check_work(&cancelled, started, deadline)?;
            let mut client =
                VkClient::from_cookie_file(&cookie_file).map_err(|error| error.to_string())?;
            refresh_audio(&mut client, id, &query, &cancelled, started, deadline)
        },
    )
}

/// Refreshes a queue-only identity, checking ownership before each external request.
fn refresh_audio(
    client: &mut impl AudioResolver,
    id: VkAudioId,
    query: &str,
    cancellation: &AtomicBool,
    started: Instant,
    deadline: Duration,
) -> Result<VkPlaybackUrl, String> {
    super::local::check_work(cancellation, started, deadline)?;
    client.authenticate()?;
    super::local::check_work(cancellation, started, deadline)?;
    let track = exact_track(client.search(query)?, id)?;
    resolve_selected(client, &track, cancellation, started, deadline)
}

/// Only the exact retained session and recording may resolve a selected source.
fn resolve_session_audio(
    source: &VkExportSource,
    cancellation: &AtomicBool,
    started: Instant,
    deadline: Duration,
) -> Result<VkPlaybackUrl, String> {
    let mut source = source.clone();
    wait_for_resolution(
        cancellation,
        started,
        deadline,
        move |cancelled, started, deadline| {
            resolve_selected(
                &mut source.client,
                &source.track,
                &cancelled,
                started,
                deadline,
            )
        },
    )
}

/// Retained catalogue rows never need a new search, even when global results omit them.
fn resolve_selected(
    client: &mut impl AudioResolver,
    track: &VkTrack,
    cancellation: &AtomicBool,
    started: Instant,
    deadline: Duration,
) -> Result<VkPlaybackUrl, String> {
    super::local::check_work(cancellation, started, deadline)?;
    let audio = client.resolve(track)?;
    super::local::check_work(cancellation, started, deadline)?;
    Ok(audio)
}

/// Cancellation and deadlines revoke any later HTTP calls in an already started worker.
fn wait_for_resolution(
    cancellation: &AtomicBool,
    started: Instant,
    deadline: Duration,
    resolve: impl FnOnce(Arc<AtomicBool>, Instant, Duration) -> Result<VkPlaybackUrl, String>
    + Send
    + 'static,
) -> Result<VkPlaybackUrl, String> {
    let deadline = deadline.min(started.elapsed().saturating_add(Duration::from_secs(120)));
    super::local::check_work(cancellation, started, deadline)?;
    let guard = ResolutionGuard(Arc::new(AtomicBool::new(false)));
    let cancelled = guard.0.clone();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("vk-export-resolve".to_owned())
        .spawn(move || {
            let result = resolve(cancelled, started, deadline);
            let _ = sender.send(result);
        })
        .map_err(|_| "Cannot start native VK audio resolution".to_owned())?;
    loop {
        super::local::check_work(cancellation, started, deadline)?;
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(result) => {
                super::local::check_work(cancellation, started, deadline)?;
                return result;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err("VK audio resolution stopped without a result".to_owned());
            }
        }
    }
}

/// Similar titles or artists never substitute for the reviewed owner/audio identity.
fn exact_track(page: VkPage, id: VkAudioId) -> Result<VkTrack, String> {
    page.tracks
        .into_iter()
        .find(|track| track.id == id)
        .ok_or_else(|| {
            "The exact VK track is unavailable in the refreshed catalogue; search for it again"
                .to_owned()
        })
}

/// Sends only a vetted audio locator to FFmpeg, with no authentication headers or file protocols.
fn ffmpeg_command(executable: &Path, source: &VkPlaybackUrl, output: &Path) -> Command {
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
            "https,tcp,tls,crypto",
            "-rw_timeout",
            "15000000",
            "-i",
        ])
        .arg(source.as_url().as_str())
        .args([
            "-map", "0:a:0", "-vn", "-sn", "-dn", "-c:a", "libopus", "-f", "opus",
        ])
        .arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

#[cfg(test)]
mod tests;
