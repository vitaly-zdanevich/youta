//! Native original-pod downloads behind the existing manual-download supervisor.
//!
//! Durable intent contains a canonical track page only. This worker resolves
//! fresh upload metadata, stages complete audio privately and publishes without
//! replacement. Neither the HTML/embed page nor a signed listen URL reaches
//! yt-dlp or persistence. Only an explicit transcode policy enables an encoder.

mod artwork;

use super::*;
use crate::providers::ProviderError;
use crate::providers::funkwhale::{FunkwhaleDownloadSource, FunkwhaleProvider};
use std::io::{Read, Write};

const MAX_AUDIO_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const TRANSFER_DEADLINE: Duration = Duration::from_secs(30 * 60);
const PREFIX_BYTES: usize = 64 * 1024;
/// Cancelled network calls may retire off-thread; repeated cancellation stays bounded.
static LIVE_TRANSFERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Real native-transfer fixtures share production's process-wide worker budget.
#[cfg(test)]
static TRANSFER_TESTS: Mutex<()> = Mutex::new(());

/// Serializes only tests that start native workers, including their cancelled retirement.
#[cfg(test)]
pub(super) fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    let guard = TRANSFER_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let deadline = Instant::now() + Duration::from_secs(5);
    while LIVE_TRANSFERS.load(AtomicOrdering::Acquire) != 0 {
        assert!(
            Instant::now() < deadline,
            "The previous native fixture did not retire its transfer worker"
        );
        thread::sleep(Duration::from_millis(2));
    }
    guard
}

/// Limits active plus retiring HTTP workers without blocking the foreground reducer.
struct TransferPermit;

impl TransferPermit {
    fn acquire() -> Result<Self, String> {
        LIVE_TRANSFERS
            .fetch_update(AtomicOrdering::AcqRel, AtomicOrdering::Acquire, |count| {
                (count < 4).then_some(count + 1)
            })
            .map(|_| Self)
            .map_err(|_| {
                "Earlier cancelled Funkwhale transfers are still stopping; retry shortly".to_owned()
            })
    }
}

impl Drop for TransferPermit {
    fn drop(&mut self) {
        LIVE_TRANSFERS.fetch_sub(1, AtomicOrdering::AcqRel);
    }
}

/// One cancelled worker may retire bounded HTTP I/O without retaining a UI owner.
pub(super) struct NativeDownload {
    cancellation: Arc<AtomicBool>,
    publication: Arc<Mutex<()>>,
    progress: Option<LineReader>,
    errors: Option<LineReader>,
    result: Receiver<DownloadExit>,
}

impl NativeDownload {
    /// Starts only an already-confirmed, stable original-pod audio download.
    pub(super) fn start(
        item: &QueueItem,
        request: &DownloadRequest,
        config: &Config,
    ) -> Result<Box<dyn RunningDownload>, String> {
        let pod = validate_identity(item, request)?;
        if request.scope != DownloadScope::SingleItem || request.format == DownloadFormat::BestVideo
        {
            return Err("Funkwhale downloads support one selected audio track".to_owned());
        }
        let permit = TransferPermit::acquire()?;
        let cancellation = Arc::new(AtomicBool::new(false));
        let publication = Arc::new(Mutex::new(()));
        let (progress_sender, progress_receiver) = bounded(64);
        let (error_sender, error_receiver) = bounded(1);
        let (result_sender, result) = bounded(1);
        let progress = LineReader::new(progress_receiver, Arc::clone(&cancellation));
        let errors = LineReader::new(error_receiver, Arc::clone(&cancellation));
        let worker_cancel = Arc::clone(&cancellation);
        let worker_publication = Arc::clone(&publication);
        let request = request.clone();
        let title = item.media.title.clone();
        let number = item
            .media
            .webpage_url
            .path()
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .ok_or("Invalid Funkwhale track identity")?
            .to_owned();
        let ffmpeg = config.providers.ffmpeg_executable.clone();
        thread::Builder::new()
            .name("youta-funkwhale-download".to_owned())
            .spawn(move || {
                let _permit = permit;
                let result = prepare_and_publish(
                    &pod,
                    &request,
                    &title,
                    &number,
                    &ffmpeg,
                    &worker_cancel,
                    &worker_publication,
                    &progress_sender,
                );
                let exit = match result {
                    Ok(path) => {
                        let _ = progress_sender
                            .send(format!("youta-file|{}\n", path.display()).into_bytes());
                        DownloadExit {
                            success: true,
                            description: "Funkwhale download completed".to_owned(),
                        }
                    }
                    Err(error) => {
                        let _ = error_sender.try_send(format!("{error}\n").into_bytes());
                        DownloadExit {
                            success: false,
                            description: error,
                        }
                    }
                };
                let _ = result_sender.send(exit);
                drop(progress_sender);
                drop(error_sender);
            })
            .map_err(|_| "Could not start the Funkwhale download worker".to_owned())?;
        Ok(Box::new(Self {
            cancellation,
            publication,
            progress: Some(progress),
            errors: Some(errors),
            result,
        }))
    }
}

impl RunningDownload for NativeDownload {
    fn take_progress_reader(&mut self) -> Option<Box<dyn BufRead + Send>> {
        self.progress
            .take()
            .map(|reader| Box::new(reader) as Box<dyn BufRead + Send>)
    }

    fn take_error_reader(&mut self) -> Option<Box<dyn BufRead + Send>> {
        self.errors
            .take()
            .map(|reader| Box::new(reader) as Box<dyn BufRead + Send>)
    }

    fn try_wait(&mut self) -> Result<Option<DownloadExit>, String> {
        match self.result.try_recv() {
            Ok(exit) => Ok(Some(exit)),
            Err(crossbeam_channel::TryRecvError::Empty) => Ok(None),
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                Err("The Funkwhale download worker stopped unexpectedly".to_owned())
            }
        }
    }

    fn cancel(&mut self) -> Result<(), String> {
        let _publication = self
            .publication
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.cancellation.store(true, AtomicOrdering::Release);
        Ok(())
    }

    fn failure_description(&self, exit: &DownloadExit) -> String {
        exit.description.clone()
    }

    fn concise_errors(&self) -> bool {
        true
    }
}

impl Drop for NativeDownload {
    fn drop(&mut self) {
        let _ = self.cancel();
    }
}

/// Bounded channel-backed output lets the existing reader threads retire on cancellation.
struct LineReader {
    receiver: Receiver<Vec<u8>>,
    cancellation: Arc<AtomicBool>,
    bytes: Vec<u8>,
    offset: usize,
}

impl LineReader {
    fn new(receiver: Receiver<Vec<u8>>, cancellation: Arc<AtomicBool>) -> Self {
        Self {
            receiver,
            cancellation,
            bytes: Vec::new(),
            offset: 0,
        }
    }
}

impl Read for LineReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let buffer = self.fill_buf()?;
        let count = output.len().min(buffer.len());
        output[..count].copy_from_slice(&buffer[..count]);
        self.consume(count);
        Ok(count)
    }
}

impl BufRead for LineReader {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        while self.offset >= self.bytes.len() {
            if self.cancellation.load(AtomicOrdering::Acquire) {
                return Ok(&[]);
            }
            match self.receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(bytes) => {
                    self.bytes = bytes;
                    self.offset = 0;
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return Ok(&[]),
            }
        }
        Ok(&self.bytes[self.offset..])
    }

    fn consume(&mut self, amount: usize) {
        self.offset = self.offset.saturating_add(amount).min(self.bytes.len());
    }
}

/// Requires canonical matching identity/page/intent before creating a network worker.
fn validate_identity(item: &QueueItem, request: &DownloadRequest) -> Result<url::Url, String> {
    let source = &item.media.webpage_url;
    if item.media.id.source != SourceKind::Funkwhale
        || item.media.kind != MediaKind::Audio
        || item.media.id.external_id != source.as_str()
        || request.source_url != *source
        || source.query().is_some()
        || source.fragment().is_some()
    {
        return Err("Select an original Funkwhale audio track for download".to_owned());
    }
    let pod = FunkwhaleProvider::instance_url_from_track_id(source.as_str())
        .map_err(|_| "Select a canonical original-pod Funkwhale track".to_owned())?;
    let prefix = pod
        .join("library/tracks/")
        .map_err(|_| "Invalid Funkwhale track page")?;
    let suffix = source
        .as_str()
        .strip_prefix(prefix.as_str())
        .ok_or("Invalid Funkwhale track page")?;
    let number = suffix.strip_suffix('/').unwrap_or(suffix);
    if !number
        .parse::<u64>()
        .is_ok_and(|id| id > 0 && id.to_string() == number)
    {
        return Err("Select a canonical original-pod Funkwhale track".to_owned());
    }
    Ok(pod)
}

/// Shares one absolute deadline across resolution, redirects, copy and optional conversion.
#[allow(clippy::too_many_arguments)]
fn prepare_and_publish(
    pod: &url::Url,
    request: &DownloadRequest,
    title: &str,
    number: &str,
    ffmpeg: &Path,
    cancellation: &AtomicBool,
    publication: &Mutex<()>,
    progress: &Sender<Vec<u8>>,
) -> Result<PathBuf, String> {
    let started = Instant::now();
    check_work(cancellation, started)?;
    let provider = FunkwhaleProvider::new(pod.clone()).map_err(|_| "Invalid Funkwhale pod")?;
    let audio = provider
        .download_source(
            request.source_url.as_str(),
            request.format == DownloadFormat::OpusWithoutTranscoding,
        )
        .map_err(|error| match error {
            ProviderError::HttpStatus(status) => {
                format!("Funkwhale audio metadata returned HTTP status {status}")
            }
            _ => "Could not resolve this Funkwhale track's public audio upload".to_owned(),
        })?;
    check_work(cancellation, started)?;
    let destination = crate::fs_path::canonicalize(&request.destination)
        .map_err(|_| "The download destination is unavailable")?;
    let directory = tempfile::Builder::new()
        .prefix(".youta-funkwhale-download-")
        .tempdir_in(&destination)
        .map_err(|_| "Cannot create private download staging")?;
    crate::private_files::set_private_directory_permissions(directory.path())
        .map_err(|_| "Cannot protect download staging")?;
    let original = directory.path().join("original");
    let (length, extension) = transfer_audio(&audio, &original, cancellation, started, progress)?;
    let (completed, extension) = match request.format {
        DownloadFormat::OriginalBestAudio
        | DownloadFormat::ExactFile
        | DownloadFormat::AudioOnlyWithoutReencoding => (original, extension),
        DownloadFormat::OpusWithoutTranscoding if extension == "opus" => (original, extension),
        DownloadFormat::OpusWithoutTranscoding | DownloadFormat::TranscodeToOpus => {
            let output = directory.path().join("audio.opus");
            convert_audio(
                ffmpeg,
                &original,
                &output,
                &extension,
                request.format,
                cancellation,
                started,
            )?;
            (output, "opus".to_owned())
        }
        DownloadFormat::BestVideo => return Err("Funkwhale tracks are audio-only".to_owned()),
    };
    check_work(cancellation, started)?;
    let metadata =
        std::fs::symlink_metadata(&completed).map_err(|_| "The downloaded audio is unavailable")?;
    if !metadata.file_type().is_file() || metadata.len() == 0 || metadata.len() > MAX_AUDIO_BYTES {
        return Err("The downloaded audio is empty or exceeds the 8 GiB limit".to_owned());
    }
    // Sidecars belong to the explicit download preference, never selection/prefetch.
    let artwork = request
        .write_thumbnail
        .then(|| {
            audio
                .thumbnail_url
                .as_ref()
                .and_then(|url| artwork::prepare(url, directory.path(), cancellation).ok())
        })
        .flatten();
    let _publication = publication
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    check_work(cancellation, started)?;
    if crate::fs_path::canonicalize(&request.destination)
        .ok()
        .as_ref()
        != Some(&destination)
    {
        return Err("The download destination changed before publication".to_owned());
    }
    let path = publish_no_replace(&completed, &destination, title, number, &extension)?;
    if let Some(artwork) = artwork {
        let _ = artwork::publish(artwork, &path);
    }
    let _ =
        progress.try_send(format!("youta-progress|{length}|{length}|NA|NA|0|NA|NA\n").into_bytes());
    Ok(path)
}

/// Follows only bounded HTTP(S) redirects and refuses HTML, playlists and partial objects.
fn transfer_audio(
    audio: &FunkwhaleDownloadSource,
    path: &Path,
    cancellation: &AtomicBool,
    started: Instant,
    progress: &Sender<Vec<u8>>,
) -> Result<(u64, String), String> {
    if audio
        .size
        .is_some_and(|size| size == 0 || size > MAX_AUDIO_BYTES)
    {
        return Err("This audio upload is empty or exceeds the 8 GiB download limit".to_owned());
    }
    let agent = audio_agent(Duration::from_secs(15));
    let mut url = audio.url.clone();
    for _ in 0..=5 {
        check_work(cancellation, started)?;
        validate_audio_url(&url)?;
        let mut response = agent
            .get(url.as_str())
            .header("Accept", "audio/*, application/octet-stream")
            .header("Accept-Encoding", "identity")
            .config()
            .timeout_global(Some(TRANSFER_DEADLINE.saturating_sub(started.elapsed())))
            .build()
            .call()
            .map_err(|_| "The Funkwhale audio transfer failed".to_owned())?;
        check_work(cancellation, started)?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .ok_or("The audio redirect has no valid destination")?;
            url = url
                .join(location)
                .map_err(|_| "The audio redirect is invalid")?;
            continue;
        }
        if response.status().as_u16() != 200 {
            return Err(format!(
                "Funkwhale audio returned HTTP status {}",
                response.status().as_u16()
            ));
        }
        let mimetype = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .or_else(|| audio.mimetype.clone());
        if mimetype.as_deref().is_some_and(|value| {
            let value = value.to_ascii_lowercase();
            value.starts_with("text/")
                || value.contains("html")
                || value.contains("json")
                || value.contains("xml")
                || value.contains("mpegurl")
        }) {
            return Err(
                "Funkwhale returned a page or playlist instead of an audio file".to_owned(),
            );
        }
        let declared = response.body().content_length();
        if declared.is_some_and(|length| length == 0 || length > MAX_AUDIO_BYTES)
            || matches!((declared, audio.size), (Some(http), Some(api)) if http != api)
        {
            return Err("The audio size differs from the advertised original upload".to_owned());
        }
        let expected = declared.or(audio.size);
        let mut output = crate::private_files::open_privately(
            std::fs::OpenOptions::new().write(true).create_new(true),
        )
        .open(path)
        .map_err(|_| "Cannot create private download audio")?;
        let mut reader = response.body_mut().as_reader();
        let mut buffer = [0_u8; 64 * 1024];
        let mut prefix = Vec::with_capacity(PREFIX_BYTES);
        let mut count = 0_u64;
        let mut last_progress = started;
        loop {
            check_work(cancellation, started)?;
            let read = read_audio_chunk(&mut reader, &mut buffer, cancellation, started)?;
            check_work(cancellation, started)?;
            if read == 0 {
                break;
            }
            count = count
                .checked_add(read as u64)
                .filter(|count| *count <= MAX_AUDIO_BYTES)
                .ok_or("The audio upload exceeds the 8 GiB download limit")?;
            if expected.is_some_and(|length| count > length) {
                return Err("The audio exceeded its declared size".to_owned());
            }
            prefix.extend_from_slice(&buffer[..read.min(PREFIX_BYTES - prefix.len())]);
            output
                .write_all(&buffer[..read])
                .map_err(|_| "Cannot write the downloaded audio")?;
            if last_progress.elapsed() >= Duration::from_millis(100) {
                let total = expected.map_or_else(|| "NA".to_owned(), |value| value.to_string());
                let _ = progress.try_send(
                    format!("youta-progress|{count}|{total}|NA|NA|NA|NA|NA\n").into_bytes(),
                );
                last_progress = Instant::now();
            }
        }
        if count == 0 || expected.is_some_and(|length| count != length) {
            return Err("The audio transfer is empty or truncated".to_owned());
        }
        let extension = audio_extension(&prefix, audio.extension.as_deref(), mimetype.as_deref())?;
        output
            .sync_all()
            .map_err(|_| "Cannot sync the downloaded audio")?;
        return Ok((count, extension));
    }
    Err("The audio URL redirected too many times".to_owned())
}

/// Bounds headers and individual body receives without capping a progressing full upload.
///
/// In ureq 3.3, `recv_response` is also checked as the body's predecessor
/// budget. Setting it here would impose an unintended whole-body limit. The
/// send-request budget bounds the following header wait instead; body receives
/// have their own short timeout inside the shared whole-transfer deadline.
fn audio_agent(io_timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(TRANSFER_DEADLINE))
        .timeout_connect(Some(io_timeout))
        .timeout_send_request(Some(io_timeout))
        .timeout_recv_body(Some(io_timeout))
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into()
}

/// Retries interrupted reads on the same body, retaining cancellation and deadline checks.
fn read_audio_chunk(
    reader: &mut impl Read,
    buffer: &mut [u8],
    cancellation: &AtomicBool,
    started: Instant,
) -> Result<usize, String> {
    loop {
        check_work(cancellation, started)?;
        match reader.read(buffer) {
            // Read's contract permits retrying this operation without replaying
            // an HTTP request or losing already copied audio bytes.
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                return Err("The Funkwhale audio transfer was interrupted or truncated".to_owned());
            }
            Ok(read) => return Ok(read),
        }
    }
}

/// Rejects signed locators from errors while allowing the pod's public media CDN.
fn validate_audio_url(url: &url::Url) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.as_str().len() > 16_384
    {
        return Err("The pod advertised an unsafe audio URL".to_owned());
    }
    Ok(())
}

/// Derives a safe filename extension from actual audio headers, then bounded API/MIME hints.
fn audio_extension(
    prefix: &[u8],
    advertised: Option<&str>,
    mimetype: Option<&str>,
) -> Result<String, String> {
    let text = String::from_utf8_lossy(&prefix[..prefix.len().min(256)])
        .trim_start()
        .to_ascii_lowercase();
    if ["<", "{", "[", "#extm3u"]
        .iter()
        .any(|value| text.starts_with(value))
    {
        return Err("The audio endpoint returned a page or playlist instead of audio".to_owned());
    }
    let detected = if prefix.starts_with(b"ID3") {
        Some("mp3")
    } else if prefix.starts_with(b"fLaC") {
        Some("flac")
    } else if prefix.starts_with(b"OggS") {
        // Only the first packet identifies the codec; a Vorbis comment can
        // contain the word OpusHead without making its audio safe to copy as Opus.
        let first_packet = prefix.get(26).map(|segments| 27 + usize::from(*segments));
        Some(
            if first_packet
                .is_some_and(|offset| prefix.get(offset..offset + 8) == Some(b"OpusHead"))
            {
                "opus"
            } else {
                "ogg"
            },
        )
    } else if prefix.starts_with(b"RIFF") && prefix.get(8..12) == Some(b"WAVE") {
        Some("wav")
    } else if prefix.starts_with(b"FORM") && matches!(prefix.get(8..12), Some(b"AIFF" | b"AIFC")) {
        Some("aiff")
    } else if prefix.get(4..8) == Some(b"ftyp") {
        Some("m4a")
    } else if prefix.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        Some("webm")
    } else if prefix.len() >= 2 && prefix[0] == 0xff && prefix[1] & 0xe0 == 0xe0 {
        Some(if prefix[1] & 0xf6 == 0xf0 {
            "aac"
        } else {
            "mp3"
        })
    } else {
        None
    };
    if let Some(extension) = detected {
        return Ok(extension.to_owned());
    }
    let advertised = advertised.map(|value| value.trim_start_matches('.').to_ascii_lowercase());
    if let Some(extension) = advertised.filter(|value| {
        matches!(
            value.as_str(),
            "mp3"
                | "flac"
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
    }) {
        return Ok(extension);
    }
    let extension = match mimetype
        .and_then(|value| value.split(';').next())
        .map(str::trim)
    {
        Some("audio/mpeg") => "mp3",
        Some("audio/flac" | "audio/x-flac") => "flac",
        Some("audio/ogg" | "application/ogg") => "ogg",
        Some("audio/mp4") => "m4a",
        Some("audio/aac") => "aac",
        Some("audio/wav" | "audio/x-wav") => "wav",
        _ => return Err("The pod did not advertise a supported audio file format".to_owned()),
    };
    Ok(extension.to_owned())
}

/// Stream-copy cannot activate an encoder; only the explicit transcode variant uses libopus.
fn conversion_command(
    ffmpeg: &Path,
    original: &Path,
    output: &Path,
    extension: &str,
    format: DownloadFormat,
) -> Command {
    let mut command = Command::new(ffmpeg);
    command
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-n",
            "-protocol_whitelist",
            "file,pipe",
            "-f",
            match extension {
                "m4a" | "m4b" => "mov",
                "ogg" | "oga" | "opus" => "ogg",
                "aif" | "aiff" => "aiff",
                "wave" => "wav",
                "wma" => "asf",
                "webm" => "matroska",
                "flac" => "flac",
                "aac" => "aac",
                "wav" => "wav",
                _ => "mp3",
            },
            "-i",
        ])
        .arg(original)
        .args(["-map", "0:a:0", "-vn", "-sn", "-dn", "-c:a"])
        .arg(if format == DownloadFormat::TranscodeToOpus {
            "libopus"
        } else {
            "copy"
        })
        .args(["-f", "opus"])
        .arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Holds a supervised local helper until success, cancellation or the whole-download deadline.
fn convert_audio(
    ffmpeg: &Path,
    original: &Path,
    output: &Path,
    extension: &str,
    format: DownloadFormat,
    cancellation: &AtomicBool,
    started: Instant,
) -> Result<(), String> {
    let mut command = conversion_command(ffmpeg, original, output, extension, format);
    let child = crate::child_process::supervised(&mut command)
        .spawn()
        .map_err(|_| "Could not start FFmpeg for the requested audio format")?;
    struct Guard(std::process::Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            crate::child_process::terminate_tree(&mut self.0);
        }
    }
    let mut child = Guard(child);
    loop {
        check_work(cancellation, started)?;
        if output
            .metadata()
            .is_ok_and(|metadata| metadata.len() > MAX_AUDIO_BYTES)
        {
            return Err("Prepared audio exceeds the 8 GiB download limit".to_owned());
        }
        match child.0.try_wait().map_err(|_| "Could not monitor FFmpeg audio preparation")? {
            Some(status) if status.success() => return Ok(()),
            Some(_) if format == DownloadFormat::OpusWithoutTranscoding => return Err("Cannot copy this upload as Opus; set subscriptions.audio_format to original or transcode-opus".to_owned()),
            Some(_) => return Err("FFmpeg could not prepare the requested Opus audio".to_owned()),
            None => thread::sleep(Duration::from_millis(25)),
        }
    }
}

/// Publishes complete bytes atomically, without overwriting any existing user file.
fn publish_no_replace(
    source: &Path,
    destination: &Path,
    title: &str,
    number: &str,
    extension: &str,
) -> Result<PathBuf, String> {
    let title = filename_component(title, 140, "track");
    for collision in 0..1000 {
        let suffix = if collision == 0 {
            String::new()
        } else {
            format!(" ({collision})")
        };
        let path = destination.join(format!("{title} [{number}]{suffix}.{extension}"));
        match std::fs::hard_link(source, &path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                return Err("Cannot publish the completed audio without replacement".to_owned());
            }
        }
    }
    Err("Too many existing files have this download name".to_owned())
}

/// Keeps source titles as bounded filename data rather than paths or shell text.
fn filename_component(value: &str, limit: usize, fallback: &str) -> String {
    let mut result = value
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                )
            {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    let mut boundary = result.len().min(limit);
    while !result.is_char_boundary(boundary) {
        boundary -= 1;
    }
    result.truncate(boundary);
    let result = result.trim_matches([' ', '.']);
    if result.is_empty() {
        fallback.to_owned()
    } else {
        result.to_owned()
    }
}

/// Every state transition rechecks cancellation before publishing or spawning a helper.
fn check_work(cancellation: &AtomicBool, started: Instant) -> Result<(), String> {
    if cancellation.load(AtomicOrdering::Acquire) {
        return Err("Funkwhale download cancelled".to_owned());
    }
    if started.elapsed() >= TRANSFER_DEADLINE {
        return Err("Funkwhale download timed out".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A partial successful read followed by EINTR must not lose or duplicate encoded bytes.
    #[test]
    fn interrupted_audio_reads_resume_the_same_body_without_duplicate_bytes() {
        struct InterruptedBody {
            input: std::io::Cursor<&'static [u8]>,
            calls: usize,
        }
        impl Read for InterruptedBody {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                self.calls += 1;
                if self.calls == 2 {
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                let limit = output.len().min(3);
                self.input.read(&mut output[..limit])
            }
        }
        let bytes = b"fLaCoriginal encoded audio and tags";
        let mut reader = InterruptedBody {
            input: std::io::Cursor::new(bytes.as_slice()),
            calls: 0,
        };
        let mut copied = Vec::new();
        let mut buffer = [0; 64];
        let cancelled = AtomicBool::new(false);
        let started = Instant::now();
        loop {
            let count = read_audio_chunk(&mut reader, &mut buffer, &cancelled, started).unwrap();
            if count == 0 {
                break;
            }
            copied.extend_from_slice(&buffer[..count]);
        }
        assert_eq!(copied, bytes);
        assert_eq!(reader.input.position(), bytes.len() as u64);
    }

    /// Interrupted reads recheck cancellation; other errors remain terminal after one call.
    #[test]
    fn interrupted_audio_read_retry_observes_cancellation_and_does_not_retry_timeouts() {
        struct FailingBody<'a> {
            cancellation: &'a AtomicBool,
            kind: std::io::ErrorKind,
            calls: usize,
        }
        impl Read for FailingBody<'_> {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                self.calls += 1;
                if self.kind == std::io::ErrorKind::Interrupted {
                    self.cancellation.store(true, AtomicOrdering::Release);
                }
                Err(self.kind.into())
            }
        }
        let cancelled = AtomicBool::new(false);
        let mut reader = FailingBody {
            cancellation: &cancelled,
            kind: std::io::ErrorKind::Interrupted,
            calls: 0,
        };
        let error =
            read_audio_chunk(&mut reader, &mut [0; 8], &cancelled, Instant::now()).unwrap_err();
        assert!(error.contains("cancelled"));
        assert_eq!(reader.calls, 1);
        cancelled.store(false, AtomicOrdering::Release);
        reader.kind = std::io::ErrorKind::TimedOut;
        reader.calls = 0;
        assert!(read_audio_chunk(&mut reader, &mut [0; 8], &cancelled, Instant::now()).is_err());
        assert_eq!(reader.calls, 1);
    }

    /// Uses the exact stable page held by durable intent, never a resolved stream.
    fn fixture_item(page: &str) -> QueueItem {
        QueueItem {
            media: MediaItem {
                id: MediaId::new(SourceKind::Funkwhale, page),
                kind: MediaKind::Audio,
                title: "Selected track".to_owned(),
                creator: None,
                description: None,
                webpage_url: url::Url::parse(page).unwrap(),
                thumbnail_url: None,
                duration_seconds: Some(123),
                published_at: None,
                statistics: MediaStatistics::default(),
                license: MediaLicense::Unknown,
                chapters: Vec::new(),
                captions: Vec::new(),
            },
            playback_location: page.to_owned(),
            start_at_seconds: None,
            added_at: 0,
        }
    }

    /// Fixture traffic is limited to one localhost audio response and contains no real media.
    fn audio_server(
        body: Vec<u8>,
        content_type: &'static str,
        delay: Duration,
    ) -> (url::Url, JoinHandle<()>) {
        audio_server_with_header_delay(body, content_type, delay, Duration::ZERO)
    }

    /// A separate header delay distinguishes response stalls from progressing body data.
    fn audio_server_with_header_delay(
        body: Vec<u8>,
        content_type: &'static str,
        delay: Duration,
        header_delay: Duration,
    ) -> (url::Url, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = url::Url::parse(&format!(
            "http://{}/listen?signature=fixture-private",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "audio fixture was not requested");
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                }
            };
            crate::test_support::configure_http_fixture_stream(&stream, Duration::from_secs(2));
            let mut input = Vec::new();
            while !input.ends_with(b"\r\n\r\n") && input.len() < 8192 {
                let mut byte = [0];
                assert_eq!(stream.read(&mut byte).unwrap(), 1);
                input.push(byte[0]);
            }
            thread::sleep(header_delay);
            if write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).is_err() {
                return;
            }
            for chunk in body.chunks(8) {
                if !delay.is_zero() {
                    thread::sleep(delay);
                }
                if stream.write_all(chunk).is_err() {
                    break;
                }
            }
        });
        (url, server)
    }

    /// With ureq 3.3, recv_response can constrain the following body as a predecessor budget.
    #[test]
    fn header_wait_budget_does_not_cut_off_a_healthy_progressing_audio_body() {
        let body =
            b"ID3\0original encoded audio bytes split into slow fixture chunks and unchanged tags"
                .to_vec();
        // Fall between chunk arrivals, not exactly on a receive boundary.
        let budget = Duration::from_millis(450);
        let (url, server) = audio_server(body.clone(), "audio/mpeg", Duration::from_millis(100));
        let agent = audio_agent(budget);
        let started = Instant::now();
        let mut response = agent.get(url.as_str()).call().unwrap();
        let mut bytes = Vec::new();
        let result = response.body_mut().as_reader().read_to_end(&mut bytes);
        server.join().unwrap();
        assert!(
            started.elapsed() > budget,
            "The body must outlive the header wait budget"
        );
        result.expect("Healthy audio must retain the whole transfer deadline");
        assert_eq!(bytes, body);
    }

    /// Correct body handling must not remove the short bound for a server withholding headers.
    #[test]
    fn native_audio_header_wait_remains_bounded() {
        let (url, server) = audio_server_with_header_delay(
            b"ID3fixture".to_vec(),
            "audio/mpeg",
            Duration::ZERO,
            Duration::from_secs(1),
        );
        let agent = audio_agent(Duration::from_millis(500));
        let result = agent.get(url.as_str()).call();
        server.join().unwrap();
        assert!(
            matches!(result, Err(ureq::Error::Timeout(_))),
            "Headers must not consume the 30-minute transfer allowance"
        );
    }

    /// Healthy full audio transfers may take longer than a short receive budget.
    #[test]
    fn healthy_native_audio_transfer_can_take_longer_than_two_seconds() {
        let body =
            b"ID3\0original encoded audio bytes split into slow fixture chunks and unchanged tags"
                .to_vec();
        let (url, server) = audio_server(body.clone(), "audio/mpeg", Duration::from_millis(250));
        let source = FunkwhaleDownloadSource {
            url,
            extension: Some("mp3".to_owned()),
            mimetype: Some("audio/mpeg".to_owned()),
            size: Some(body.len() as u64),
            thumbnail_url: None,
            advertised_opus: false,
        };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("original");
        let (sender, _receiver) = bounded(64);
        let started = Instant::now();
        let result = transfer_audio(&source, &path, &AtomicBool::new(false), started, &sender);
        server.join().unwrap();
        assert!(started.elapsed() > Duration::from_secs(2));
        assert_eq!(result.unwrap(), (body.len() as u64, "mp3".to_owned()));
        assert_eq!(std::fs::read(path).unwrap(), body);
    }

    /// HTML error bodies cannot become apparently successful .mp3 files.
    #[test]
    fn native_audio_transfer_rejects_html_even_when_the_api_claims_mp3() {
        let body = b"<!doctype html><html>Service unavailable</html>".to_vec();
        for content_type in ["text/html", "audio/mpeg"] {
            let (url, server) = audio_server(body.clone(), content_type, Duration::ZERO);
            let source = FunkwhaleDownloadSource {
                url,
                extension: Some("mp3".to_owned()),
                mimetype: Some("audio/mpeg".to_owned()),
                size: Some(body.len() as u64),
                thumbnail_url: None,
                advertised_opus: false,
            };
            let directory = tempfile::tempdir().unwrap();
            let (sender, _receiver) = bounded(64);
            let error = transfer_audio(
                &source,
                &directory.path().join("private"),
                &AtomicBool::new(false),
                Instant::now(),
                &sender,
            )
            .unwrap_err();
            server.join().unwrap();
            assert!(error.contains("page") || error.contains("playlist"));
            assert!(!error.contains("fixture-private"));
        }
    }

    /// A metadata reply and stalled body prove that UI cancellation does not wait for the pod.
    #[test]
    fn active_native_transfer_cancellation_retires_readers_and_never_publishes_partial_audio() {
        let _guard = test_guard();
        let directory = crate::test_support::canonical_tempdir("cancel native Funkwhale audio");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let page = format!("{base}library/tracks/27");
        let (body_started, started) = bounded(1);
        let (release, body_release) = bounded(1);
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            for index in 0..2 {
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "native transfer did not request its fixture"
                            );
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("fixture accept: {error}"),
                    }
                };
                crate::test_support::configure_http_fixture_stream(&stream, Duration::from_secs(2));
                let mut input = Vec::new();
                while !input.ends_with(b"\r\n\r\n") && input.len() < 8192 {
                    let mut byte = [0];
                    assert_eq!(stream.read(&mut byte).unwrap(), 1);
                    input.push(byte[0]);
                }
                let input = String::from_utf8(input).unwrap();
                if index == 0 {
                    assert!(input.starts_with("GET /api/v1/tracks/27/ "));
                    let metadata = serde_json::json!({
                        "id": 27, "title": "Track", "listen_url": "/listen/original?signature=private",
                        "uploads": [{"extension": "mp3", "size": 1_000_000}]
                    }).to_string();
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{metadata}", metadata.len()).unwrap();
                } else {
                    assert!(input.starts_with("GET /listen/original?signature=private "));
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\nID3partial").unwrap();
                    body_started.send(()).unwrap();
                    body_release.recv_timeout(Duration::from_secs(5)).unwrap();
                }
            }
        });
        let request = DownloadRequest {
            source_url: url::Url::parse(&page).unwrap(),
            destination: directory.path().to_owned(),
            format: DownloadFormat::OriginalBestAudio,
            scope: DownloadScope::SingleItem,
            playlist_start: None,
            skip_shorts: false,
            write_thumbnail: false,
            archive_path: None,
        };
        let item = fixture_item(&page);
        let config = Config::for_dir(directory.path().join("unused-config"));
        let process = NativeDownload::start(&item, &request, &config).unwrap();
        let mut active = ActiveDownload::start(
            item.media.title,
            directory.path().to_owned(),
            false,
            None,
            process,
            WorkerNotifier::default(),
        )
        .unwrap();
        started.recv_timeout(Duration::from_secs(3)).unwrap();
        let cancel_started = Instant::now();
        active.cancel_and_join();
        assert!(
            cancel_started.elapsed() < Duration::from_secs(1),
            "cancellation must not wait for a stalled remote body"
        );
        release.send(()).unwrap();
        server.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while std::fs::read_dir(directory.path())
            .unwrap()
            .next()
            .is_some()
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        assert_eq!(active.output.lock().unwrap().completed_files, 0);
    }

    /// No implicit converter may change an original request or enable an encoder for copy.
    #[test]
    fn format_conversion_enables_an_encoder_only_for_explicit_transcoding() {
        for (format, expected) in [
            (DownloadFormat::OpusWithoutTranscoding, "copy"),
            (DownloadFormat::TranscodeToOpus, "libopus"),
        ] {
            let command = conversion_command(
                Path::new("ffmpeg-fixture"),
                Path::new("private-original"),
                Path::new("private-output.opus"),
                "mp3",
                format,
            );
            let arguments = command
                .get_args()
                .map(|value| value.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            assert!(arguments.windows(2).any(|pair| pair == ["-c:a", expected]));
            assert!(
                arguments
                    .windows(2)
                    .any(|pair| pair == ["-protocol_whitelist", "file,pipe"])
            );
            assert!(arguments.windows(2).any(|pair| pair == ["-f", "opus"]));
            assert!(
                arguments.windows(2).any(|pair| pair == ["-f", "mp3"]),
                "A local audio demuxer must be explicit instead of probing a possible playlist"
            );
            assert!(!arguments.iter().any(|value| value.starts_with("http")));
        }
    }

    /// Test fixtures do not weaken the production limit on active and retiring workers.
    #[test]
    fn native_worker_budget_remains_four_and_releases_every_reservation() {
        let _guard = test_guard();
        let permits = (0..4)
            .map(|_| TransferPermit::acquire().unwrap())
            .collect::<Vec<_>>();
        assert!(TransferPermit::acquire().is_err());
        assert_eq!(LIVE_TRANSFERS.load(AtomicOrdering::Acquire), 4);
        drop(permits);
        assert_eq!(LIVE_TRANSFERS.load(AtomicOrdering::Acquire), 0);
    }

    /// Explicit byte evidence, not an untrusted suffix, proves that copied audio is Opus.
    #[test]
    fn actual_audio_headers_override_advertised_extensions_and_reject_web_pages() {
        let mut opus = b"OggS".to_vec();
        opus.resize(26, 0);
        opus.extend_from_slice(b"\x01\x13OpusHead");
        assert_eq!(
            audio_extension(&opus, Some("mp3"), Some("audio/ogg")).unwrap(),
            "opus"
        );
        assert_eq!(
            audio_extension(b"OggS\0fixture\x01vorbis", Some("opus"), Some("audio/ogg")).unwrap(),
            "ogg"
        );
        assert_eq!(
            audio_extension(
                b"OggS\0vorbis comments containing OpusHead",
                Some("opus"),
                Some("audio/ogg")
            )
            .unwrap(),
            "ogg"
        );
        assert_eq!(
            audio_extension(
                b"ID3\0tagged encoded bytes",
                Some("opus"),
                Some("audio/opus")
            )
            .unwrap(),
            "mp3"
        );
        for bytes in [
            b"<!doctype html><html>challenge".as_slice(),
            b" {\"error\":\"forbidden\"}",
            b"#EXTM3U\nhttp://example.test/audio",
        ] {
            assert!(audio_extension(bytes, Some("mp3"), Some("audio/mpeg")).is_err());
        }
        assert!(
            audio_extension(
                b"unknown",
                Some("../../exe"),
                Some("application/octet-stream")
            )
            .is_err()
        );
    }

    /// Complete private originals are linked without changing bytes or replacing user files.
    #[test]
    fn publication_preserves_original_bytes_and_existing_files() {
        let directory = tempfile::tempdir().unwrap();
        let private = directory.path().join("private");
        let bytes = b"ID3\0original tags and encoded audio TAG\0";
        std::fs::write(&private, bytes).unwrap();
        let existing = directory.path().join("Selected track [27].mp3");
        std::fs::write(&existing, b"keep this user file").unwrap();
        let completed =
            publish_no_replace(&private, directory.path(), "Selected track", "27", "mp3").unwrap();
        assert_eq!(
            completed.file_name().unwrap(),
            "Selected track [27] (1).mp3"
        );
        assert_eq!(std::fs::read(completed).unwrap(), bytes);
        assert_eq!(std::fs::read(existing).unwrap(), b"keep this user file");
        assert_eq!(
            filename_component("../unsafe\\track:name", 140, "track"),
            "_unsafe_track_name"
        );
    }

    /// Cancelling readers must not wait for a stalled remote body before freeing the UI.
    #[test]
    fn cancelled_reader_retires_without_waiting_for_its_worker() {
        let cancellation = Arc::new(AtomicBool::new(false));
        let (_sender, receiver) = bounded(1);
        let mut reader = LineReader::new(receiver, Arc::clone(&cancellation));
        let (finished_sender, finished) = bounded(1);
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = reader.read_to_end(&mut bytes);
            let _ = finished_sender.send(result);
        });
        cancellation.store(true, AtomicOrdering::Release);
        assert_eq!(
            finished
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap(),
            0
        );
    }

    /// No cancellation outcome can be confused with a successful completed file.
    #[test]
    fn cancelled_transfer_cannot_create_staging_or_publish() {
        let directory = tempfile::tempdir().unwrap();
        let request = DownloadRequest {
            source_url: url::Url::parse("https://pod.example/library/tracks/27").unwrap(),
            destination: directory.path().to_owned(),
            format: DownloadFormat::OriginalBestAudio,
            scope: DownloadScope::SingleItem,
            playlist_start: None,
            skip_shorts: false,
            write_thumbnail: false,
            archive_path: None,
        };
        let (sender, _receiver) = bounded(1);
        let cancelled = AtomicBool::new(true);
        let error = prepare_and_publish(
            &url::Url::parse("https://pod.example/").unwrap(),
            &request,
            "Track",
            "27",
            Path::new("ffmpeg-must-not-run"),
            &cancelled,
            &Mutex::new(()),
            &sender,
        )
        .unwrap_err();
        assert_eq!(error, "Funkwhale download cancelled");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    /// Redirect validation cannot accept credentials, fragments or nonweb protocols.
    #[test]
    fn unsafe_advertised_audio_locations_are_rejected_without_disclosure() {
        for value in [
            "file:///etc/passwd",
            "https://user:secret@cdn.example/audio",
            "https://cdn.example/audio#private",
        ] {
            let error = validate_audio_url(&url::Url::parse(value).unwrap()).unwrap_err();
            assert!(!error.contains(value));
            assert!(!error.contains("secret"));
        }
    }
}
