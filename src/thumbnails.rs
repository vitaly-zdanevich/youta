//! Lazy, bounded terminal-thumbnail loading, cache prefetching, and protocol
//! encoding.
//!
//! Capability detection is conservative: automatic mode never writes a probe
//! to the terminal, and unsupported terminals never start the network worker.
//! Fetching, bounded image validation, resizing, and protocol encoding all
//! happen away from the TUI render thread. Background prefetch uses an
//! independent worker, persists validated bytes only, and never delays selected
//! artwork or requests a redraw. Recently encoded local and remote terminal
//! images remain in an entry- and decoded-byte-bounded in-memory cache so
//! revisiting media is immediate. Local images always revalidate their
//! filesystem fingerprint before reuse.

use std::collections::{HashSet, VecDeque};
use std::fs;
#[cfg(test)]
use std::fs::OpenOptions;
use std::io::{self, BufRead, Cursor, IsTerminal, Read, Seek};
use std::path::{Path, PathBuf};
#[cfg(feature = "local-video-thumbnails")]
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
#[cfg(feature = "local-video-thumbnails")]
use std::time::Instant;
use std::time::{Duration, SystemTime};

use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded};
use image::{
    DynamicImage, GenericImageView, GrayAlphaImage, GrayImage, ImageFormat, ImageReader, Limits,
    RgbImage, RgbaImage,
};
use jpeg_decoder::{CodingProcess, Decoder as JpegDecoder, PixelFormat};
use ratatui::layout::{Rect, Size};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{Resize, ResizeEncodeRender};
use sha2::{Digest, Sha256};
use url::Url;

use crate::config::ThumbnailMode;
use crate::terminal_environment::{TerminalAttachment, is_linux_virtual_console};

const MAX_DOWNLOAD_BYTES: usize = 4 * 1024 * 1024;
use crate::artwork::{
    HttpThumbnailTransport, ThumbnailCache, ThumbnailTransport, is_safe_thumbnail_source,
};
// The renderer's tests still exercise the pipeline end to end, so they reach
// into the neutral half for its fetch entry points and guarded resolver.
#[cfg(test)]
use crate::artwork::{
    ActiveCacheTemporary, ThumbnailCachePolicy, fetch_thumbnail, fetch_thumbnail_with_policy,
    is_cache_entry_name, mock_thumbnail_agent, thumbnail_agent,
};

// `ThumbnailFailure` moved to the neutral half; keep its published path.
pub use crate::artwork::ThumbnailFailure;

const MAX_IMAGE_DIMENSION: u32 = 4_096;
const MAX_DECODE_ALLOC_BYTES: u64 = 32 * 1024 * 1024;
const REQUEST_DEBOUNCE: Duration = Duration::from_millis(150);
const FALLBACK_FONT_SIZE: (u16, u16) = (10, 20);
const MAX_PREFETCH_SOURCES: usize = 512;
const MAX_PREFETCH_URL_BYTES: usize = 4 * 1024;
const PREPARED_THUMBNAIL_CACHE_ENTRIES: usize = 16;
const PREPARED_THUMBNAIL_CACHE_MAX_DECODED_BYTES: usize = 16 * 1024 * 1024;
const LOCAL_PREVIEW_CACHE_KEY_VERSION: &[u8] = b"youta-local-preview-v1\0";
const LOCAL_PREVIEW_MAGIC: &[u8; 8] = b"YTPRV001";
const LOCAL_PREVIEW_HEADER_BYTES: usize = LOCAL_PREVIEW_MAGIC.len() + 4 + 4 + 1;
const LOCAL_VIDEO_FRAME_CACHE_KEY_VERSION: &[u8] = b"youta-local-video-frame-v1\0";
const LOCAL_VIDEO_PREVIEW_CACHE_KEY_VERSION: &[u8] = b"youta-local-video-preview-v1\0";
const LOCAL_VIDEO_EXTRACTION_PROFILE: &[u8] = b"mjpeg-q5-fit-1280-v1";
#[cfg(feature = "local-video-thumbnails")]
const LOCAL_VIDEO_FRAME_MAX_DIMENSION: u32 = 1_280;
#[cfg(feature = "local-video-thumbnails")]
const LOCAL_VIDEO_EXTRACT_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(feature = "local-video-thumbnails")]
const LOCAL_VIDEO_EXTRACT_POLL: Duration = Duration::from_millis(10);
#[cfg(feature = "local-video-thumbnails")]
const MAX_LOCAL_VIDEO_STDERR_BYTES: usize = 64 * 1024;
#[cfg(test)]
static LOCAL_SOURCE_DECODE_COUNTS: std::sync::Mutex<Vec<(PathBuf, usize)>> =
    std::sync::Mutex::new(Vec::new());

/// Graphics protocol selected for terminal artwork.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThumbnailProtocol {
    /// Kitty graphics protocol.
    Kitty,
    /// iTerm2 inline-image protocol, also supported by `WezTerm`.
    Iterm2,
    /// DEC Sixel graphics.
    Sixel,
    /// Unicode upper-half-block rendering for a confirmed Linux virtual console.
    Halfblocks,
}

impl ThumbnailProtocol {
    fn ratatui(self) -> ProtocolType {
        match self {
            Self::Kitty => ProtocolType::Kitty,
            Self::Iterm2 => ProtocolType::Iterm2,
            Self::Sixel => ProtocolType::Sixel,
            Self::Halfblocks => ProtocolType::Halfblocks,
        }
    }
}

/// Result of applying the configured thumbnail policy to the current terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThumbnailCapability {
    /// Artwork was disabled in configuration.
    Disabled,
    /// No safely detectable graphics protocol is available.
    Unsupported,
    /// Artwork can be encoded with the enclosed protocol.
    Supported(ThumbnailProtocol),
}

/// Current state of the selected item's lazy thumbnail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThumbnailState {
    /// Artwork was disabled by configuration.
    Disabled,
    /// The terminal has no supported graphics protocol.
    Unsupported,
    /// No selected item currently requests artwork.
    Idle,
    /// The selected item's artwork is being prepared.
    Loading,
    /// A protocol image is ready for the selected item and panel size.
    Ready,
    /// The selected artwork could not be displayed.
    Failed(ThumbnailFailure),
}

/// Terminal facts used by the side-effect-free automatic capability policy.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "these independently observed terminal facts are not a state machine"
)]
pub struct TerminalInfo {
    /// Whether this binary is running on Linux.
    ///
    /// An observed fact like every other field here rather than a `cfg!` read
    /// at the point of use, because the virtual-console policy below is
    /// ordinary logic that every platform's build should be able to exercise.
    /// Deciding it from the compilation target instead left the Linux console
    /// rules untested anywhere but Linux.
    pub linux: bool,
    /// Whether standard input is attached to a terminal.
    pub stdin_is_terminal: bool,
    /// Whether standard output is attached to a terminal.
    pub stdout_is_terminal: bool,
    /// Value of `TERM`, when present.
    pub term: Option<String>,
    /// Value of `TERM_PROGRAM`, when present.
    pub term_program: Option<String>,
    /// Value of `LC_TERMINAL`, when present.
    pub lc_terminal: Option<String>,
    /// Whether Kitty exported `KITTY_WINDOW_ID`.
    pub kitty_window: bool,
    /// Whether `WezTerm` exported `WEZTERM_PANE`.
    pub wezterm_pane: bool,
    /// Whether the process is nested inside tmux.
    pub tmux: bool,
    /// Whether an SSH transport is present in the process environment.
    pub ssh: bool,
    /// Resolved terminal device for standard output, when available.
    pub output_device: Option<PathBuf>,
    /// Cell width and height in pixels, when the terminal ioctl reports them.
    pub font_size: Option<(u16, u16)>,
}

impl TerminalInfo {
    /// Captures terminal environment and ioctl facts without emitting probes.
    #[must_use]
    pub fn current() -> Self {
        let term = std::env::var("TERM").ok();
        let term_program = std::env::var("TERM_PROGRAM").ok();
        let lc_terminal = std::env::var("LC_TERMINAL").ok();
        let tmux = std::env::var_os("TMUX").is_some()
            || term
                .as_deref()
                .is_some_and(|value| value.starts_with("tmux"))
            || term_program.as_deref() == Some("tmux");
        let ssh = ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
            .into_iter()
            .any(|name| std::env::var_os(name).is_some());
        Self {
            linux: cfg!(target_os = "linux"),
            stdin_is_terminal: io::stdin().is_terminal(),
            stdout_is_terminal: io::stdout().is_terminal(),
            term,
            term_program,
            lc_terminal,
            kitty_window: std::env::var_os("KITTY_WINDOW_ID").is_some(),
            wezterm_pane: std::env::var_os("WEZTERM_PANE").is_some(),
            tmux,
            ssh,
            output_device: std::fs::read_link("/proc/self/fd/1").ok(),
            font_size: terminal_font_size(),
        }
    }

    /// Returns whether output is a directly attached Linux virtual console.
    fn confirmed_linux_virtual_console(&self) -> bool {
        TerminalAttachment {
            linux: self.linux,
            stdin_is_terminal: self.stdin_is_terminal,
            stdout_is_terminal: self.stdout_is_terminal,
            term: self.term.clone(),
            ssh: self.ssh,
            tmux: self.tmux,
            output_device: self.output_device.clone(),
        }
        .is_physical_linux_virtual_console()
    }

    fn hard_unsupported(&self) -> bool {
        if !self.stdin_is_terminal || !self.stdout_is_terminal || self.tmux || self.ssh {
            return true;
        }
        let Some(term) = self.term.as_deref() else {
            return true;
        };
        let normalized = term.to_ascii_lowercase();
        normalized == "dumb"
            || normalized.starts_with("vt")
            || self.output_device.as_deref().is_some_and(|path| {
                is_serial_terminal(path)
                    || (is_linux_virtual_console(path) && !self.confirmed_linux_virtual_console())
            })
            || (normalized == "linux" && !self.confirmed_linux_virtual_console())
    }

    fn environment_protocol(&self) -> Option<ThumbnailProtocol> {
        if self.hard_unsupported() {
            return None;
        }
        let term = self
            .term
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let term_program = self
            .term_program
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let lc_terminal = self
            .lc_terminal
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();

        if self.confirmed_linux_virtual_console() {
            Some(ThumbnailProtocol::Halfblocks)
        } else if self.kitty_window || term.contains("kitty") {
            Some(ThumbnailProtocol::Kitty)
        } else if self.wezterm_pane
            || term_program == "wezterm"
            || term_program == "iterm.app"
            || lc_terminal == "iterm2"
        {
            Some(ThumbnailProtocol::Iterm2)
        } else if term.contains("sixel")
            || term_program == "foot"
            || term_program == "mlterm"
            || lc_terminal == "foot"
        {
            Some(ThumbnailProtocol::Sixel)
        } else {
            None
        }
    }
}

/// Millisecond offset used to extract one representative local-video frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LocalVideoMidpoint(u64);

#[derive(Clone, Debug, Eq, PartialEq)]
struct ThumbnailTarget {
    source: Url,
    local_video_midpoint: Option<LocalVideoMidpoint>,
    area: Rect,
}

#[derive(Clone, Debug)]
struct WorkerRequest {
    generation: u64,
    target: ThumbnailTarget,
}

struct WorkerResult {
    generation: u64,
    result: Result<EncodedThumbnail, ThumbnailFailure>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PreparedThumbnailKey {
    source: Url,
    local_video_midpoint: Option<LocalVideoMidpoint>,
    width: u16,
    height: u16,
    local_fingerprint: Option<LocalThumbnailFingerprint>,
}

impl From<&ThumbnailTarget> for PreparedThumbnailKey {
    fn from(target: &ThumbnailTarget) -> Self {
        Self {
            source: target.source.clone(),
            local_video_midpoint: target.local_video_midpoint,
            width: target.area.width,
            height: target.area.height,
            local_fingerprint: None,
        }
    }
}

impl PreparedThumbnailKey {
    /// Captures the current replacement-sensitive identity for a local target.
    ///
    /// Remote targets need no filesystem identity because their URL is already
    /// the cache key. A local failure remains a worker-visible error rather
    /// than reusing an entry whose source can no longer be verified.
    fn current(target: &ThumbnailTarget) -> Option<Self> {
        let mut key = Self::from(target);
        if target.source.scheme() == "file" {
            let path = target.source.to_file_path().ok()?;
            key.local_fingerprint = Some(LocalThumbnailFingerprint::capture(&path).ok()?);
        }
        Some(key)
    }

    fn from_loaded(
        target: &ThumbnailTarget,
        local_fingerprint: Option<LocalThumbnailFingerprint>,
    ) -> Option<Self> {
        let mut key = Self::from(target);
        if target.source.scheme() == "file" {
            key.local_fingerprint = Some(local_fingerprint?);
        }
        Some(key)
    }

    fn same_target(&self, other: &Self) -> bool {
        self.source == other.source
            && self.local_video_midpoint == other.local_video_midpoint
            && self.width == other.width
            && self.height == other.height
    }
}

/// One encoded terminal image retained for fast keyboard navigation.
///
/// Local entries include the exact filesystem fingerprint captured by the
/// worker that decoded their pixels.
struct PreparedThumbnail {
    key: PreparedThumbnailKey,
    protocol: StatefulProtocol,
    render_size: Size,
    decoded_bytes: usize,
}

/// One encoded protocol and the decoded source allocation it retains.
struct EncodedThumbnail {
    protocol: StatefulProtocol,
    render_size: Size,
    decoded_bytes: usize,
    local_fingerprint: Option<LocalThumbnailFingerprint>,
}

/// One terminal protocol plus an optional local derivative to persist after
/// the ready result has been published.
struct LoadedThumbnail {
    protocol: StatefulProtocol,
    render_size: Size,
    decoded_bytes: usize,
    local_fingerprint: Option<LocalThumbnailFingerprint>,
    deferred_local_frame: Option<DeferredLocalPreview>,
    deferred_local_preview: Option<DeferredLocalPreview>,
}

/// Bounded local preview bytes whose disk write must not delay first display.
struct DeferredLocalPreview {
    cache_key: [u8; 32],
    record: Vec<u8>,
    fingerprint: LocalThumbnailFingerprint,
}

/// Exact pixel box requested by one terminal-cell thumbnail area.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LocalPreviewTarget {
    width: u32,
    height: u32,
}

/// Cancellation token shared by one visible thumbnail request and its worker.
#[derive(Clone)]
struct RequestCancellation {
    generation: u64,
    current_generation: Arc<AtomicU64>,
}

impl RequestCancellation {
    fn is_cancelled(&self) -> bool {
        self.current_generation.load(Ordering::Acquire) != self.generation
    }
}

/// Extracts one bounded encoded frame without coupling the manager to FFmpeg.
trait LocalVideoFrameExtractor: Send + 'static {
    fn extract(
        &mut self,
        path: &Path,
        midpoint: LocalVideoMidpoint,
        cancellation: &RequestCancellation,
    ) -> Result<Vec<u8>, ThumbnailFailure>;
}

/// Shell-free FFmpeg process used by the production local-video worker.
struct FfmpegVideoFrameExtractor {
    program: PathBuf,
}

impl Default for FfmpegVideoFrameExtractor {
    fn default() -> Self {
        Self::new(PathBuf::from("ffmpeg"))
    }
}

impl FfmpegVideoFrameExtractor {
    /// Extracts frames with a specific `FFmpeg` build.
    const fn new(program: PathBuf) -> Self {
        Self { program }
    }
}

#[cfg(not(feature = "local-video-thumbnails"))]
impl LocalVideoFrameExtractor for FfmpegVideoFrameExtractor {
    fn extract(
        &mut self,
        _path: &Path,
        _midpoint: LocalVideoMidpoint,
        _cancellation: &RequestCancellation,
    ) -> Result<Vec<u8>, ThumbnailFailure> {
        Err(ThumbnailFailure::LocalVideoFrameExtractionFailed)
    }
}

#[cfg(feature = "local-video-thumbnails")]
impl LocalVideoFrameExtractor for FfmpegVideoFrameExtractor {
    fn extract(
        &mut self,
        path: &Path,
        midpoint: LocalVideoMidpoint,
        cancellation: &RequestCancellation,
    ) -> Result<Vec<u8>, ThumbnailFailure> {
        if cancellation.is_cancelled() {
            return Err(ThumbnailFailure::LocalVideoFrameExtractionFailed);
        }
        let mut child = local_video_frame_command(&self.program, path, midpoint)
            .spawn()
            .map_err(|_| ThumbnailFailure::LocalVideoFrameExtractionFailed)?;
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ThumbnailFailure::LocalVideoFrameExtractionFailed);
        };
        let Some(stderr) = child.stderr.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ThumbnailFailure::LocalVideoFrameExtractionFailed);
        };
        let stdout_reader =
            thread::spawn(move || read_bounded_process_pipe(stdout, MAX_DOWNLOAD_BYTES));
        let stderr_reader =
            thread::spawn(move || read_bounded_process_pipe(stderr, MAX_LOCAL_VIDEO_STDERR_BYTES));
        let deadline = Instant::now() + LOCAL_VIDEO_EXTRACT_TIMEOUT;

        let status = loop {
            if cancellation.is_cancelled() || Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(ThumbnailFailure::LocalVideoFrameExtractionFailed);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => thread::sleep(LOCAL_VIDEO_EXTRACT_POLL),
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    return Err(ThumbnailFailure::LocalVideoFrameExtractionFailed);
                }
            }
        };

        let bytes = stdout_reader
            .join()
            .map_err(|_| ThumbnailFailure::LocalVideoFrameExtractionFailed)?
            .map_err(|_| ThumbnailFailure::LocalVideoFrameExtractionFailed)?;
        // Diagnostics are intentionally discarded: local paths and helper
        // arguments must never escape through user-facing thumbnail failures.
        let _ = stderr_reader.join();
        if cancellation.is_cancelled() {
            return Err(ThumbnailFailure::LocalVideoFrameExtractionFailed);
        }
        if bytes.len() > MAX_DOWNLOAD_BYTES {
            return Err(ThumbnailFailure::ResponseTooLarge);
        }
        if !status.success() || bytes.is_empty() {
            return Err(ThumbnailFailure::LocalVideoFrameExtractionFailed);
        }
        Ok(bytes)
    }
}

/// Builds a shell-free, one-frame FFmpeg invocation with bounded output size.
#[cfg(feature = "local-video-thumbnails")]
fn local_video_frame_command(program: &Path, path: &Path, midpoint: LocalVideoMidpoint) -> Command {
    let mut command = Command::new(program);
    crate::child_process::quiet(&mut command);
    command
        .arg("-nostdin")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-ss")
        .arg(format_ffmpeg_timestamp(midpoint))
        .arg("-i")
        .arg(path)
        .arg("-map")
        .arg("0:v:0")
        .arg("-frames:v")
        .arg("1")
        .arg("-an")
        .arg("-sn")
        .arg("-dn")
        .arg("-vf")
        .arg(format!(
            "scale=w={LOCAL_VIDEO_FRAME_MAX_DIMENSION}:h={LOCAL_VIDEO_FRAME_MAX_DIMENSION}:\
             force_original_aspect_ratio=decrease:force_divisible_by=2:flags=fast_bilinear,\
             format=yuvj420p"
        ))
        .arg("-c:v")
        .arg("mjpeg")
        .arg("-q:v")
        .arg("5")
        .arg("-f")
        .arg("image2pipe")
        .arg("pipe:1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Formats a millisecond offset without locale-dependent decimal separators.
#[cfg(feature = "local-video-thumbnails")]
fn format_ffmpeg_timestamp(midpoint: LocalVideoMidpoint) -> String {
    format!("{}.{:03}", midpoint.0 / 1_000, midpoint.0 % 1_000)
}

/// Reads at most `limit + 1` bytes so callers can detect and reject overflow.
#[cfg(feature = "local-video-thumbnails")]
fn read_bounded_process_pipe(reader: impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    reader
        .take(u64::try_from(limit.saturating_add(1)).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Stable identity for one regular local image at a point in time.
///
/// The canonical path and change metadata keep derivatives private, invalidate
/// them after replacement or mutation, and reject symlinks before any decode.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalThumbnailFingerprint {
    canonical_path: PathBuf,
    length: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    /// Number the filesystem assigned, which a replacement cannot reuse.
    filesystem: Option<crate::file_identity::FilesystemIdentity>,
}

impl LocalThumbnailFingerprint {
    fn capture(path: &Path) -> Result<Self, ThumbnailFailure> {
        let supplied_metadata =
            fs::symlink_metadata(path).map_err(|_| ThumbnailFailure::DownloadFailed)?;
        if !supplied_metadata.file_type().is_file() {
            return Err(ThumbnailFailure::InvalidSource);
        }
        let canonical_path =
            fs::canonicalize(path).map_err(|_| ThumbnailFailure::DownloadFailed)?;
        let canonical_metadata =
            fs::symlink_metadata(&canonical_path).map_err(|_| ThumbnailFailure::DownloadFailed)?;
        if !canonical_metadata.file_type().is_file() {
            return Err(ThumbnailFailure::InvalidSource);
        }
        let supplied = Self::from_metadata(canonical_path.clone(), &supplied_metadata);
        let canonical = Self::from_metadata(canonical_path, &canonical_metadata);
        if supplied != canonical {
            return Err(ThumbnailFailure::InvalidSource);
        }
        Ok(canonical)
    }

    fn from_metadata(canonical_path: PathBuf, metadata: &fs::Metadata) -> Self {
        Self {
            filesystem: crate::file_identity::filesystem_identity(&canonical_path, metadata),
            canonical_path,
            length: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
        }
    }

    fn is_current(&self) -> bool {
        Self::capture(&self.canonical_path).is_ok_and(|current| current == *self)
    }

    fn preview_cache_key(&self, target: LocalPreviewTarget) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(LOCAL_PREVIEW_CACHE_KEY_VERSION);
        self.update_cache_digest(&mut digest);
        digest.update(target.width.to_le_bytes());
        digest.update(target.height.to_le_bytes());
        digest.finalize().into()
    }

    /// Returns the source-frame cache key for one local-video midpoint.
    fn video_frame_cache_key(&self, midpoint: LocalVideoMidpoint) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(LOCAL_VIDEO_FRAME_CACHE_KEY_VERSION);
        self.update_cache_digest(&mut digest);
        digest.update(midpoint.0.to_le_bytes());
        digest.update(LOCAL_VIDEO_EXTRACTION_PROFILE);
        digest.finalize().into()
    }

    /// Returns the fitted derivative key for one local-video midpoint and area.
    fn video_preview_cache_key(
        &self,
        midpoint: LocalVideoMidpoint,
        target: LocalPreviewTarget,
    ) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(LOCAL_VIDEO_PREVIEW_CACHE_KEY_VERSION);
        self.update_cache_digest(&mut digest);
        digest.update(midpoint.0.to_le_bytes());
        digest.update(LOCAL_VIDEO_EXTRACTION_PROFILE);
        digest.update(target.width.to_le_bytes());
        digest.update(target.height.to_le_bytes());
        digest.finalize().into()
    }

    /// Adds replacement-sensitive filesystem identity to a private cache key.
    fn update_cache_digest(&self, digest: &mut Sha256) {
        hash_local_thumbnail_path(digest, &self.canonical_path);
        digest.update(self.length.to_le_bytes());
        hash_local_thumbnail_system_time(digest, self.modified);
        hash_local_thumbnail_system_time(digest, self.created);
        // A cache key is only as sharp as the identity behind it, so the
        // filesystem's own numbers go in whenever the platform supplies them.
        // The tag keeps "no identity" from colliding with an identity of zeros.
        match self.filesystem {
            Some(filesystem) => {
                digest.update([1]);
                digest.update(filesystem.volume.to_le_bytes());
                digest.update(filesystem.file.to_le_bytes());
                let (seconds, nanoseconds) = filesystem.changed.unwrap_or((0, 0));
                digest.update([u8::from(filesystem.changed.is_some())]);
                digest.update(seconds.to_le_bytes());
                digest.update(nanoseconds.to_le_bytes());
            }
            None => digest.update([0]),
        }
    }
}

#[cfg(unix)]
fn hash_local_thumbnail_path(digest: &mut Sha256, path: &Path) {
    use std::os::unix::ffi::OsStrExt;

    let bytes = path.as_os_str().as_bytes();
    digest.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes());
    digest.update(bytes);
}

#[cfg(windows)]
fn hash_local_thumbnail_path(digest: &mut Sha256, path: &Path) {
    use std::os::windows::ffi::OsStrExt;

    let length = path.as_os_str().encode_wide().count();
    digest.update(u64::try_from(length).unwrap_or(u64::MAX).to_le_bytes());
    for word in path.as_os_str().encode_wide() {
        digest.update(word.to_le_bytes());
    }
}

#[cfg(not(any(unix, windows)))]
fn hash_local_thumbnail_path(digest: &mut Sha256, path: &Path) {
    let path = path.as_os_str().to_string_lossy();
    digest.update(u64::try_from(path.len()).unwrap_or(u64::MAX).to_le_bytes());
    digest.update(path.as_bytes());
}

fn hash_local_thumbnail_system_time(digest: &mut Sha256, time: Option<SystemTime>) {
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

/// Serializes a pre-fitted image into one strictly bounded private cache
/// record. Unsupported high-bit-depth variants are normalized to RGBA8.
fn encode_local_preview_record(image: &DynamicImage) -> Option<Vec<u8>> {
    let converted;
    let (color, pixels): (u8, &[u8]) = match image {
        DynamicImage::ImageLuma8(image) => (1, image.as_raw()),
        DynamicImage::ImageLumaA8(image) => (2, image.as_raw()),
        DynamicImage::ImageRgb8(image) => (3, image.as_raw()),
        DynamicImage::ImageRgba8(image) => (4, image.as_raw()),
        _ => {
            converted = image.to_rgba8();
            (4, converted.as_raw())
        }
    };
    let total = LOCAL_PREVIEW_HEADER_BYTES.checked_add(pixels.len())?;
    if image.width() == 0
        || image.height() == 0
        || image.width() > MAX_IMAGE_DIMENSION
        || image.height() > MAX_IMAGE_DIMENSION
        || total > MAX_DOWNLOAD_BYTES
    {
        return None;
    }
    let mut record = Vec::with_capacity(total);
    record.extend_from_slice(LOCAL_PREVIEW_MAGIC);
    record.extend_from_slice(&image.width().to_le_bytes());
    record.extend_from_slice(&image.height().to_le_bytes());
    record.push(color);
    record.extend_from_slice(pixels);
    Some(record)
}

/// Parses one private preview record after validating dimensions and its exact
/// payload length before allocating an image buffer.
fn decode_local_preview_record(bytes: &[u8]) -> Option<DynamicImage> {
    if bytes.len() < LOCAL_PREVIEW_HEADER_BYTES
        || bytes.len() > MAX_DOWNLOAD_BYTES
        || bytes.get(..LOCAL_PREVIEW_MAGIC.len())? != LOCAL_PREVIEW_MAGIC
    {
        return None;
    }
    let width = u32::from_le_bytes(
        bytes
            .get(8..12)?
            .try_into()
            .expect("fixed-width preview width"),
    );
    let height = u32::from_le_bytes(
        bytes
            .get(12..16)?
            .try_into()
            .expect("fixed-width preview height"),
    );
    if width == 0 || height == 0 || width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        return None;
    }
    let color = *bytes.get(16)?;
    let channels = match color {
        1 => 1_usize,
        2 => 2,
        3 => 3,
        4 => 4,
        _ => return None,
    };
    let expected = usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?
        .checked_mul(channels)?
        .checked_add(LOCAL_PREVIEW_HEADER_BYTES)?;
    if expected != bytes.len() {
        return None;
    }
    let pixels = bytes.get(LOCAL_PREVIEW_HEADER_BYTES..)?.to_vec();
    match color {
        1 => GrayImage::from_raw(width, height, pixels).map(DynamicImage::ImageLuma8),
        2 => GrayAlphaImage::from_raw(width, height, pixels).map(DynamicImage::ImageLumaA8),
        3 => RgbImage::from_raw(width, height, pixels).map(DynamicImage::ImageRgb8),
        4 => RgbaImage::from_raw(width, height, pixels).map(DynamicImage::ImageRgba8),
        _ => None,
    }
}

/// Owns the selected thumbnail's bounded background pipeline and ready image.
pub struct ThumbnailManager {
    capability: ThumbnailCapability,
    state: ThumbnailState,
    generation: u64,
    current_generation: Arc<AtomicU64>,
    target: Option<ThumbnailTarget>,
    protocol: Option<StatefulProtocol>,
    protocol_render_size: Option<Size>,
    protocol_decoded_bytes: usize,
    protocol_key: Option<PreparedThumbnailKey>,
    prepared: VecDeque<PreparedThumbnail>,
    prepared_decoded_bytes: usize,
    picker: Option<Picker>,
    cache_directory: Option<PathBuf>,
    video_frame_program: PathBuf,
    request_sender: Option<Sender<WorkerRequest>>,
    request_discarder: Option<Receiver<WorkerRequest>>,
    prefetch_sender: Option<Sender<Vec<Url>>>,
    prefetch_discarder: Option<Receiver<Vec<Url>>>,
    prefetch_sources: Vec<Url>,
    result_receiver: Option<Receiver<WorkerResult>>,
}

impl ThumbnailManager {
    /// Points local video-frame extraction at a specific `FFmpeg`.
    ///
    /// Applied after construction rather than through every constructor: the
    /// worker that reads it is started lazily, on the first local video, so a
    /// manager configured at any point before then is configured in time. The
    /// default is the bare name, which is what a Unix installation puts on
    /// `PATH`; Windows installations usually need the full path.
    #[must_use]
    pub fn with_video_frame_program(mut self, program: PathBuf) -> Self {
        self.video_frame_program = program;
        self
    }

    /// Detects the current terminal and starts a worker only when useful.
    ///
    /// `Auto` relies only on environment variables and terminal ioctls. `On`
    /// may issue a capability query when the environment is inconclusive, so
    /// callers should construct the manager after entering the alternate
    /// screen and before reading terminal events.
    #[must_use]
    pub fn from_current_terminal(mode: ThumbnailMode) -> Self {
        Self::from_current_terminal_with_tty_images(mode, true)
    }

    /// Detects the current terminal while applying the physical-TTY artwork
    /// preference.
    ///
    /// Disabling TTY artwork affects only a confirmed local Linux virtual
    /// console. Graphical terminal protocols continue to follow `mode`.
    #[must_use]
    pub fn from_current_terminal_with_tty_images(
        mode: ThumbnailMode,
        show_images_in_tty: bool,
    ) -> Self {
        Self::from_terminal_info_with_cache(
            mode,
            &TerminalInfo::current(),
            None,
            show_images_in_tty,
        )
    }

    /// Detects the current terminal and lazily enables a persistent byte cache.
    ///
    /// The cache directory is not created until a supported terminal requests
    /// visible artwork. Unsupported terminals never read or write the cache.
    #[must_use]
    pub fn from_current_terminal_with_cache(mode: ThumbnailMode, cache_directory: PathBuf) -> Self {
        Self::from_current_terminal_with_cache_and_tty_images(mode, cache_directory, true)
    }

    /// Detects the current terminal, applies the physical-TTY artwork
    /// preference, and lazily enables a persistent byte cache.
    ///
    /// The cache remains untouched when `show_images_in_tty` disables artwork
    /// on a confirmed physical Linux console.
    #[must_use]
    pub fn from_current_terminal_with_cache_and_tty_images(
        mode: ThumbnailMode,
        cache_directory: PathBuf,
        show_images_in_tty: bool,
    ) -> Self {
        Self::from_terminal_info_with_cache(
            mode,
            &TerminalInfo::current(),
            Some(cache_directory),
            show_images_in_tty,
        )
    }

    #[cfg(test)]
    fn from_terminal_info(mode: ThumbnailMode, terminal: &TerminalInfo) -> Self {
        Self::from_terminal_info_with_cache(mode, terminal, None, true)
    }

    fn from_terminal_info_with_cache(
        mode: ThumbnailMode,
        terminal: &TerminalInfo,
        cache_directory: Option<PathBuf>,
        show_images_in_tty: bool,
    ) -> Self {
        if mode == ThumbnailMode::Off {
            return Self::inactive(ThumbnailCapability::Disabled);
        }
        if terminal.confirmed_linux_virtual_console() && !show_images_in_tty {
            return Self::inactive(ThumbnailCapability::Disabled);
        }
        if terminal.hard_unsupported() {
            return Self::inactive(ThumbnailCapability::Unsupported);
        }

        let detected = terminal.environment_protocol();
        let picker_and_protocol = detected
            .map(|protocol| {
                (
                    picker_for_protocol(protocol, terminal.font_size.unwrap_or(FALLBACK_FONT_SIZE)),
                    protocol,
                )
            })
            .or_else(|| {
                if mode == ThumbnailMode::On {
                    queried_picker()
                } else {
                    None
                }
            });
        let Some((picker, protocol)) = picker_and_protocol else {
            return Self::inactive(ThumbnailCapability::Unsupported);
        };

        Self {
            capability: ThumbnailCapability::Supported(protocol),
            state: ThumbnailState::Idle,
            generation: 0,
            current_generation: Arc::new(AtomicU64::new(0)),
            target: None,
            protocol: None,
            protocol_render_size: None,
            protocol_decoded_bytes: 0,
            protocol_key: None,
            prepared: VecDeque::new(),
            prepared_decoded_bytes: 0,
            picker: Some(picker),
            cache_directory,
            video_frame_program: PathBuf::from("ffmpeg"),
            request_sender: None,
            request_discarder: None,
            prefetch_sender: None,
            prefetch_discarder: None,
            prefetch_sources: Vec::new(),
            result_receiver: None,
        }
    }

    fn inactive(capability: ThumbnailCapability) -> Self {
        let state = match capability {
            ThumbnailCapability::Disabled => ThumbnailState::Disabled,
            ThumbnailCapability::Unsupported => ThumbnailState::Unsupported,
            ThumbnailCapability::Supported(_) => ThumbnailState::Idle,
        };
        Self {
            capability,
            state,
            generation: 0,
            current_generation: Arc::new(AtomicU64::new(0)),
            target: None,
            protocol: None,
            protocol_render_size: None,
            protocol_decoded_bytes: 0,
            protocol_key: None,
            prepared: VecDeque::new(),
            prepared_decoded_bytes: 0,
            picker: None,
            cache_directory: None,
            video_frame_program: PathBuf::from("ffmpeg"),
            request_sender: None,
            request_discarder: None,
            prefetch_sender: None,
            prefetch_discarder: None,
            prefetch_sources: Vec::new(),
            result_receiver: None,
        }
    }

    /// Returns the configured and detected terminal capability.
    #[must_use]
    pub const fn capability(&self) -> ThumbnailCapability {
        self.capability
    }

    /// Returns whether this manager can fetch and render artwork.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        matches!(self.capability, ThumbnailCapability::Supported(_))
    }

    /// Synchronizes the worker with the selected URL and exact image area.
    ///
    /// Returns `true` when the visible target changed. Repeated calls with the
    /// same URL and area do no work.
    pub fn synchronize(&mut self, source: Option<&Url>, area: Rect) -> bool {
        if !self.is_enabled() {
            return false;
        }
        let Some(source) = source.filter(|_| area.width > 0 && area.height > 0) else {
            return self.clear();
        };
        let target = ThumbnailTarget {
            source: source.clone(),
            local_video_midpoint: None,
            area,
        };
        self.synchronize_target(target)
    }

    /// Synchronizes one local video with a lazily extracted midpoint frame.
    ///
    /// `midpoint_ms` is the caller-computed half-duration offset. The source
    /// must be an absolute regular-file path; validation, extraction, image
    /// decoding, and persistent-cache I/O all remain on the thumbnail worker.
    /// Repeated calls with the same path, midpoint, and area do no work.
    pub fn synchronize_local_video(&mut self, path: &Path, midpoint_ms: u64, area: Rect) -> bool {
        if !self.is_enabled() {
            return false;
        }
        if area.width == 0 || area.height == 0 {
            return self.clear();
        }
        let Ok(source) = Url::from_file_path(path) else {
            self.retain_current_protocol();
            self.generation = self.generation.wrapping_add(1);
            self.current_generation
                .store(self.generation, Ordering::Release);
            self.target = None;
            self.protocol = None;
            self.protocol_render_size = None;
            self.protocol_decoded_bytes = 0;
            self.protocol_key = None;
            self.state = ThumbnailState::Failed(ThumbnailFailure::InvalidSource);
            return true;
        };
        self.synchronize_target(ThumbnailTarget {
            source,
            local_video_midpoint: Some(LocalVideoMidpoint(midpoint_ms)),
            area,
        })
    }

    /// Applies a normalized visible target to the bounded worker pipeline.
    fn synchronize_target(&mut self, target: ThumbnailTarget) -> bool {
        if self.target.as_ref() == Some(&target) {
            return false;
        }

        self.retain_current_protocol();
        self.generation = self.generation.wrapping_add(1);
        self.current_generation
            .store(self.generation, Ordering::Release);
        self.target = Some(target.clone());
        if !is_safe_thumbnail_source(&target.source) {
            self.state = ThumbnailState::Failed(ThumbnailFailure::InvalidSource);
            return true;
        }
        if let Some(prepared) = self.take_prepared_thumbnail(&target) {
            self.protocol_key = Some(prepared.key);
            self.protocol = Some(prepared.protocol);
            self.protocol_render_size = Some(prepared.render_size);
            self.protocol_decoded_bytes = prepared.decoded_bytes;
            self.state = ThumbnailState::Ready;
            return true;
        }
        self.state = ThumbnailState::Loading;
        let request = WorkerRequest {
            generation: self.generation,
            target,
        };
        if !self.ensure_visible_worker() || !self.send_latest(request) {
            self.state = ThumbnailState::Failed(ThumbnailFailure::WorkerStopped);
        }
        true
    }

    /// Replaces the bounded background backlog of artwork to persist.
    ///
    /// Sources retain their caller-provided order after unsafe, oversized, and
    /// duplicate URLs are removed. The normalized list is remembered across
    /// visible-selection changes, while the currently visible source is
    /// omitted only from the delivered workload because [`Self::synchronize`]
    /// already gives it display priority. A background request validates and
    /// stores original image bytes in the existing persistent cache; it does
    /// not encode a terminal image, change [`Self::state`], or produce a redraw
    /// result.
    ///
    /// Passing an empty slice cancels work that has not started. At most one
    /// blocking transfer can already be in progress. Unsupported terminals,
    /// disabled artwork, and managers without a persistent cache ignore the
    /// request without starting a worker.
    ///
    /// Returns `true` when the accepted backlog changed and was delivered to
    /// the worker.
    pub fn synchronize_prefetch(&mut self, sources: &[Url]) -> bool {
        if !self.is_enabled() || self.cache_directory.is_none() {
            return false;
        }

        let mut seen = HashSet::with_capacity(sources.len().min(MAX_PREFETCH_SOURCES));
        let normalized = sources
            .iter()
            .filter(|source| {
                source.as_str().len() <= MAX_PREFETCH_URL_BYTES
                    && matches!(source.scheme(), "http" | "https")
                    && is_safe_thumbnail_source(source)
                    && seen.insert(source.as_str())
            })
            .take(MAX_PREFETCH_SOURCES)
            .cloned()
            .collect::<Vec<_>>();
        if normalized == self.prefetch_sources {
            return false;
        }
        let active_source = self.target.as_ref().map(|target| &target.source);
        let accepted = normalized
            .iter()
            .filter(|source| active_source != Some(*source))
            .cloned()
            .collect::<Vec<_>>();
        if accepted.is_empty() {
            if self.prefetch_sender.is_some() && !self.send_latest_prefetch(Vec::new()) {
                return false;
            }
            self.prefetch_sources = normalized;
            return true;
        }
        if !self.ensure_prefetch_worker() || !self.send_latest_prefetch(accepted) {
            return false;
        }
        self.prefetch_sources = normalized;
        true
    }

    fn ensure_visible_worker(&mut self) -> bool {
        if self.request_sender.is_some() {
            return true;
        }
        let Some(picker) = self.picker.take() else {
            return false;
        };
        let (request_sender, request_receiver) = bounded(1);
        let request_discarder = request_receiver.clone();
        let (result_sender, result_receiver) = bounded(1);
        let spawned = spawn_visible_worker(
            picker,
            request_receiver,
            result_sender,
            self.cache_directory.clone(),
            self.video_frame_program.clone(),
            Arc::clone(&self.current_generation),
        );
        if !spawned {
            return false;
        }
        self.request_sender = Some(request_sender);
        self.request_discarder = Some(request_discarder);
        self.result_receiver = Some(result_receiver);
        true
    }

    fn ensure_prefetch_worker(&mut self) -> bool {
        if self.prefetch_sender.is_some() {
            return true;
        }
        let Some(cache_directory) = self.cache_directory.clone() else {
            return false;
        };
        let (prefetch_sender, prefetch_receiver) = bounded(1);
        let prefetch_discarder = prefetch_receiver.clone();
        if !spawn_prefetch_worker(prefetch_receiver, cache_directory) {
            return false;
        }
        self.prefetch_sender = Some(prefetch_sender);
        self.prefetch_discarder = Some(prefetch_discarder);
        true
    }

    fn send_latest(&mut self, request: WorkerRequest) -> bool {
        let (Some(sender), Some(discarder)) = (
            self.request_sender.as_ref(),
            self.request_discarder.as_ref(),
        ) else {
            return false;
        };
        match sender.try_send(request.clone()) {
            Ok(()) => true,
            Err(crossbeam_channel::TrySendError::Full(_)) => {
                let _ = discarder.try_recv();
                sender.try_send(request).is_ok()
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => false,
        }
    }

    fn send_latest_prefetch(&mut self, sources: Vec<Url>) -> bool {
        let (Some(sender), Some(discarder)) = (
            self.prefetch_sender.as_ref(),
            self.prefetch_discarder.as_ref(),
        ) else {
            return false;
        };
        match sender.try_send(sources) {
            Ok(()) => true,
            Err(crossbeam_channel::TrySendError::Full(sources)) => {
                let _ = discarder.try_recv();
                sender.try_send(sources).is_ok()
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => false,
        }
    }

    /// Applies completed work for the current selection and discards stale work.
    ///
    /// Returns `true` when the visible state changed.
    pub fn poll(&mut self) -> bool {
        if self.result_receiver.is_none() {
            if self.state == ThumbnailState::Loading {
                self.protocol = None;
                self.protocol_render_size = None;
                self.protocol_decoded_bytes = 0;
                self.state = ThumbnailState::Failed(ThumbnailFailure::WorkerStopped);
                return true;
            }
            return false;
        }
        let mut changed = false;
        let mut disconnected = false;
        while let Some(result_receiver) = self.result_receiver.as_ref() {
            let result = result_receiver.try_recv();
            match result {
                Ok(result) if result.generation == self.generation => {
                    changed = true;
                    match result.result {
                        Ok(encoded) => {
                            self.protocol_key = self.target.as_ref().and_then(|target| {
                                PreparedThumbnailKey::from_loaded(target, encoded.local_fingerprint)
                            });
                            self.protocol = Some(encoded.protocol);
                            self.protocol_render_size = Some(encoded.render_size);
                            self.protocol_decoded_bytes = encoded.decoded_bytes;
                            self.state = ThumbnailState::Ready;
                        }
                        Err(error) => {
                            self.protocol_key = None;
                            self.protocol = None;
                            self.protocol_render_size = None;
                            self.protocol_decoded_bytes = 0;
                            self.state = ThumbnailState::Failed(error);
                        }
                    }
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if disconnected {
            self.request_sender = None;
            self.request_discarder = None;
            self.result_receiver = None;
            if self.state == ThumbnailState::Loading {
                self.protocol = None;
                self.protocol_render_size = None;
                self.protocol_decoded_bytes = 0;
                self.protocol_key = None;
                self.state = ThumbnailState::Failed(ThumbnailFailure::WorkerStopped);
                changed = true;
            }
        }
        changed
    }

    /// Returns the encoded image state for inspection.
    #[must_use]
    pub const fn protocol(&self) -> Option<&StatefulProtocol> {
        self.protocol.as_ref()
    }

    /// Returns the encoded image state for a stateful `ratatui-image` widget.
    pub fn protocol_mut(&mut self) -> Option<&mut StatefulProtocol> {
        self.protocol.as_mut()
    }

    /// Returns the exact terminal-cell size encoded by the thumbnail worker.
    ///
    /// Rendering the ready protocol into this size avoids `ratatui-image`
    /// performing a synchronous resize and encode on the TUI thread. The
    /// manager's target continues to retain the caller's full available area
    /// so prepared-image cache identity remains stable.
    #[must_use]
    pub const fn render_size(&self) -> Option<Size> {
        self.protocol_render_size
    }

    /// Returns the safe user-facing state of the selected thumbnail.
    #[must_use]
    pub const fn state(&self) -> &ThumbnailState {
        &self.state
    }

    /// Drops the selected thumbnail and invalidates any in-flight result.
    ///
    /// Returns `true` when visible state was cleared.
    pub fn clear(&mut self) -> bool {
        let changed = self.target.is_some()
            || self.protocol.is_some()
            || self.state == ThumbnailState::Loading
            || matches!(
                self.state,
                ThumbnailState::Ready | ThumbnailState::Failed(_)
            );
        if changed {
            self.retain_current_protocol();
            self.target = None;
            self.protocol = None;
            self.protocol_render_size = None;
            self.protocol_decoded_bytes = 0;
            self.protocol_key = None;
            self.generation = self.generation.wrapping_add(1);
            self.current_generation
                .store(self.generation, Ordering::Release);
            self.state = ThumbnailState::Idle;
        }
        changed
    }

    /// Moves the visible encoded image into the bounded recency cache.
    fn retain_current_protocol(&mut self) {
        let (Some(key), Some(protocol), Some(render_size)) = (
            self.protocol_key.take(),
            self.protocol.take(),
            self.protocol_render_size.take(),
        ) else {
            return;
        };
        let decoded_bytes = std::mem::take(&mut self.protocol_decoded_bytes);
        self.cache_prepared_protocol(key, protocol, render_size, decoded_bytes);
    }

    /// Inserts one prepared protocol while enforcing count and RAM bounds.
    fn cache_prepared_protocol(
        &mut self,
        key: PreparedThumbnailKey,
        protocol: StatefulProtocol,
        render_size: Size,
        decoded_bytes: usize,
    ) {
        let mut index = 0;
        while index < self.prepared.len() {
            if self.prepared[index].key.same_target(&key) {
                let replaced = self
                    .prepared
                    .remove(index)
                    .expect("prepared cache index checked above");
                self.prepared_decoded_bytes = self
                    .prepared_decoded_bytes
                    .saturating_sub(replaced.decoded_bytes);
            } else {
                index += 1;
            }
        }
        if decoded_bytes == 0 || decoded_bytes > PREPARED_THUMBNAIL_CACHE_MAX_DECODED_BYTES {
            return;
        }
        self.prepared.push_front(PreparedThumbnail {
            key,
            protocol,
            render_size,
            decoded_bytes,
        });
        self.prepared_decoded_bytes = self.prepared_decoded_bytes.saturating_add(decoded_bytes);
        while self.prepared.len() > PREPARED_THUMBNAIL_CACHE_ENTRIES
            || self.prepared_decoded_bytes > PREPARED_THUMBNAIL_CACHE_MAX_DECODED_BYTES
        {
            let Some(evicted) = self.prepared.pop_back() else {
                break;
            };
            self.prepared_decoded_bytes = self
                .prepared_decoded_bytes
                .saturating_sub(evicted.decoded_bytes);
        }
    }

    /// Takes an encoded image matching this source, cell size, and local
    /// filesystem fingerprint.
    fn take_prepared_thumbnail(&mut self, target: &ThumbnailTarget) -> Option<PreparedThumbnail> {
        let base_key = PreparedThumbnailKey::from(target);
        if !self
            .prepared
            .iter()
            .any(|entry| entry.key.same_target(&base_key))
        {
            return None;
        }
        let key = PreparedThumbnailKey::current(target)?;
        let index = self.prepared.iter().position(|entry| entry.key == key)?;
        let prepared = self.prepared.remove(index)?;
        self.prepared_decoded_bytes = self
            .prepared_decoded_bytes
            .saturating_sub(prepared.decoded_bytes);
        Some(prepared)
    }
}

fn spawn_visible_worker(
    picker: Picker,
    requests: Receiver<WorkerRequest>,
    results: Sender<WorkerResult>,
    cache_directory: Option<PathBuf>,
    video_frame_program: PathBuf,
    current_generation: Arc<AtomicU64>,
) -> bool {
    spawn_visible_worker_with_transport_and_extractor(
        picker,
        requests,
        results,
        HttpThumbnailTransport::new(),
        FfmpegVideoFrameExtractor::new(video_frame_program),
        cache_directory.map(ThumbnailCache::new),
        REQUEST_DEBOUNCE,
        current_generation,
    )
}

#[cfg(test)]
fn spawn_visible_worker_with_transport<T: ThumbnailTransport>(
    picker: Picker,
    requests: Receiver<WorkerRequest>,
    results: Sender<WorkerResult>,
    transport: T,
    cache: Option<ThumbnailCache>,
    debounce: Duration,
) -> bool {
    spawn_visible_worker_with_transport_and_extractor(
        picker,
        requests,
        results,
        transport,
        FfmpegVideoFrameExtractor::default(),
        cache,
        debounce,
        Arc::new(AtomicU64::new(0)),
    )
}

fn spawn_visible_worker_with_transport_and_extractor<
    T: ThumbnailTransport,
    E: LocalVideoFrameExtractor,
>(
    picker: Picker,
    requests: Receiver<WorkerRequest>,
    results: Sender<WorkerResult>,
    mut transport: T,
    mut extractor: E,
    mut cache: Option<ThumbnailCache>,
    debounce: Duration,
    current_generation: Arc<AtomicU64>,
) -> bool {
    thread::Builder::new()
        .name("youta-thumbnail-visible".to_owned())
        .spawn(move || {
            loop {
                let request = match requests.recv() {
                    Ok(mut request) => {
                        for newer in requests.try_iter() {
                            request = newer;
                        }
                        request
                    }
                    Err(_) => break,
                };
                if !render_worker_request(
                    request,
                    &requests,
                    &results,
                    &mut transport,
                    &mut extractor,
                    cache.as_mut(),
                    &picker,
                    debounce,
                    &current_generation,
                ) {
                    break;
                }
            }
        })
        .is_ok()
}

fn spawn_prefetch_worker(prefetch_updates: Receiver<Vec<Url>>, cache_directory: PathBuf) -> bool {
    spawn_prefetch_worker_with_transport(
        prefetch_updates,
        HttpThumbnailTransport::new(),
        ThumbnailCache::new(cache_directory),
    )
}

fn spawn_prefetch_worker_with_transport<T: ThumbnailTransport>(
    prefetch_updates: Receiver<Vec<Url>>,
    mut transport: T,
    mut cache: ThumbnailCache,
) -> bool {
    thread::Builder::new()
        .name("youta-thumbnail-prefetch".to_owned())
        .spawn(move || {
            if cache.prepare().is_err() {
                return;
            }
            let mut backlog = VecDeque::new();
            loop {
                match latest_prefetch_update(&prefetch_updates) {
                    WorkerInput::Item(sources) => backlog = sources.into(),
                    WorkerInput::Disconnected => break,
                    WorkerInput::Empty => {}
                }

                if let Some(source) = backlog.pop_front() {
                    if prefetch_thumbnail(&mut transport, &mut cache, &source).is_err() {
                        break;
                    }
                    continue;
                }

                let Ok(sources) = prefetch_updates.recv() else {
                    break;
                };
                backlog = sources.into();
            }
        })
        .is_ok()
}

#[cfg(test)]
fn spawn_worker_with_transport<T: ThumbnailTransport>(
    picker: Picker,
    requests: Receiver<WorkerRequest>,
    prefetch_updates: Receiver<Vec<Url>>,
    results: Sender<WorkerResult>,
    mut transport: T,
    mut cache: Option<ThumbnailCache>,
    debounce: Duration,
) -> bool {
    thread::Builder::new()
        .name("youta-thumbnail".to_owned())
        .spawn(move || {
            let mut prefetch_cache_ready =
                cache.as_ref().is_some_and(|cache| cache.prepare().is_ok());
            let mut prefetch_backlog = VecDeque::new();
            let mut extractor = FfmpegVideoFrameExtractor::default();
            let current_generation = Arc::new(AtomicU64::new(0));
            loop {
                match latest_worker_request(&requests) {
                    WorkerInput::Item(request) => {
                        if !render_worker_request(
                            request,
                            &requests,
                            &results,
                            &mut transport,
                            &mut extractor,
                            cache.as_mut(),
                            &picker,
                            debounce,
                            &current_generation,
                        ) {
                            break;
                        }
                        continue;
                    }
                    WorkerInput::Disconnected => break,
                    WorkerInput::Empty => {}
                }

                match latest_prefetch_update(&prefetch_updates) {
                    WorkerInput::Item(sources) => {
                        prefetch_backlog = sources.into();
                    }
                    WorkerInput::Disconnected => break,
                    WorkerInput::Empty => {}
                }

                if let Some(source) = prefetch_backlog.pop_front() {
                    // A visible selection arriving between backlog updates and
                    // transfers wins before the next blocking prefetch.
                    match latest_worker_request(&requests) {
                        WorkerInput::Item(request) => {
                            prefetch_backlog.push_front(source);
                            if !render_worker_request(
                                request,
                                &requests,
                                &results,
                                &mut transport,
                                &mut extractor,
                                cache.as_mut(),
                                &picker,
                                debounce,
                                &current_generation,
                            ) {
                                break;
                            }
                        }
                        WorkerInput::Disconnected => break,
                        WorkerInput::Empty if prefetch_cache_ready => {
                            let Some(cache) = cache.as_mut() else {
                                prefetch_cache_ready = false;
                                continue;
                            };
                            if prefetch_thumbnail(&mut transport, cache, &source).is_err() {
                                // Fetch and decode failures are intentionally
                                // swallowed by the helper. An error here means
                                // persistence itself stopped working, so avoid
                                // downloading bytes that cannot be retained.
                                prefetch_cache_ready = false;
                                prefetch_backlog.clear();
                            }
                        }
                        WorkerInput::Empty => {}
                    }
                    continue;
                }

                crossbeam_channel::select! {
                    recv(requests) -> request => {
                        let Ok(request) = request else {
                            break;
                        };
                        if !render_worker_request(
                            request,
                            &requests,
                            &results,
                            &mut transport,
                            &mut extractor,
                            cache.as_mut(),
                            &picker,
                            debounce,
                            &current_generation,
                        ) {
                            break;
                        }
                    }
                    recv(prefetch_updates) -> sources => {
                        let Ok(sources) = sources else {
                            break;
                        };
                        prefetch_backlog = sources.into();
                    }
                }
            }
        })
        .is_ok()
}

enum WorkerInput<T> {
    Item(T),
    Empty,
    Disconnected,
}

#[cfg(test)]
fn latest_worker_request(requests: &Receiver<WorkerRequest>) -> WorkerInput<WorkerRequest> {
    match requests.try_recv() {
        Ok(mut request) => {
            for newer in requests.try_iter() {
                request = newer;
            }
            WorkerInput::Item(request)
        }
        Err(TryRecvError::Empty) => WorkerInput::Empty,
        Err(TryRecvError::Disconnected) => WorkerInput::Disconnected,
    }
}

fn latest_prefetch_update(prefetch: &Receiver<Vec<Url>>) -> WorkerInput<Vec<Url>> {
    match prefetch.try_recv() {
        Ok(mut sources) => {
            for newer in prefetch.try_iter() {
                sources = newer;
            }
            WorkerInput::Item(sources)
        }
        Err(TryRecvError::Empty) => WorkerInput::Empty,
        Err(TryRecvError::Disconnected) => WorkerInput::Disconnected,
    }
}

fn render_worker_request<T: ThumbnailTransport>(
    mut request: WorkerRequest,
    requests: &Receiver<WorkerRequest>,
    results: &Sender<WorkerResult>,
    transport: &mut T,
    extractor: &mut impl LocalVideoFrameExtractor,
    cache: Option<&mut ThumbnailCache>,
    picker: &Picker,
    debounce: Duration,
    current_generation: &Arc<AtomicU64>,
) -> bool {
    let mut cache = cache;
    if matches!(request.target.source.scheme(), "http" | "https")
        && let Some(result) = load_cached_thumbnail(cache.as_deref_mut(), picker, &request.target)
    {
        return results
            .send(WorkerResult {
                generation: request.generation,
                result,
            })
            .is_ok();
    }

    // Selection churn is useful to debounce before network I/O, but local
    // files and validated disk-cache hits should never pay this delay.
    if matches!(request.target.source.scheme(), "http" | "https") && !debounce.is_zero() {
        thread::sleep(debounce);
    }
    for newer in requests.try_iter() {
        request = newer;
    }
    let cancellation = RequestCancellation {
        generation: request.generation,
        current_generation: Arc::clone(current_generation),
    };
    let loaded = load_thumbnail(
        transport,
        extractor,
        cache.as_deref_mut(),
        picker,
        &request.target,
        &cancellation,
    );
    let (result, deferred_local_frame, deferred_local_preview) = match loaded {
        Ok(LoadedThumbnail {
            protocol,
            render_size,
            decoded_bytes,
            local_fingerprint,
            deferred_local_frame,
            deferred_local_preview,
        }) => (
            Ok(EncodedThumbnail {
                protocol,
                render_size,
                decoded_bytes,
                local_fingerprint,
            }),
            deferred_local_frame,
            deferred_local_preview,
        ),
        Err(error) => (Err(error), None, None),
    };
    if results
        .send(WorkerResult {
            generation: request.generation,
            result,
        })
        .is_err()
    {
        return false;
    }

    // A cold local thumbnail becomes visible before the atomic cache write,
    // directory sync, and eviction scan can add latency.
    if let Some(cache) = cache {
        if let Some(frame) = deferred_local_frame {
            persist_local_preview(cache, &frame);
        }
        if let Some(preview) = deferred_local_preview {
            persist_local_preview(cache, &preview);
        }
    }
    true
}

/// Fetches one background source into the persistent byte cache.
///
/// Remote and decode failures are best-effort misses. The error return is
/// reserved for cache I/O failures so the worker can stop downloading data it
/// cannot retain.
fn prefetch_thumbnail(
    transport: &mut impl ThumbnailTransport,
    cache: &mut ThumbnailCache,
    source: &Url,
) -> io::Result<()> {
    match cache.read(source) {
        Ok(Some(bytes)) => {
            if decode_thumbnail(&bytes).is_ok() {
                return Ok(());
            }
            cache.remove(source);
        }
        Ok(None) => {}
        Err(error) => return Err(error),
    }

    let Ok(bytes) = transport.fetch(source) else {
        return Ok(());
    };
    if decode_thumbnail(&bytes).is_err() {
        return Ok(());
    }
    cache.store(source, &bytes)
}

fn load_thumbnail(
    transport: &mut impl ThumbnailTransport,
    extractor: &mut impl LocalVideoFrameExtractor,
    mut cache: Option<&mut ThumbnailCache>,
    picker: &Picker,
    target: &ThumbnailTarget,
    cancellation: &RequestCancellation,
) -> Result<LoadedThumbnail, ThumbnailFailure> {
    if target.source.scheme() == "file" {
        if let Some(midpoint) = target.local_video_midpoint {
            return load_local_video_thumbnail(
                cache,
                picker,
                target,
                midpoint,
                extractor,
                cancellation,
            );
        }
        return load_local_thumbnail(cache, picker, target);
    }

    let persistent_cache_allowed = matches!(target.source.scheme(), "http" | "https");
    if persistent_cache_allowed
        && let Some(result) = load_cached_thumbnail(cache.as_deref_mut(), picker, target)
    {
        return result.map(|encoded| LoadedThumbnail {
            protocol: encoded.protocol,
            render_size: encoded.render_size,
            decoded_bytes: encoded.decoded_bytes,
            local_fingerprint: None,
            deferred_local_frame: None,
            deferred_local_preview: None,
        });
    }

    let bytes = transport.fetch(&target.source)?;
    let image = decode_thumbnail(&bytes)?;
    if persistent_cache_allowed && let Some(cache) = cache {
        let _ = cache.store(&target.source, &bytes);
    }
    encode_remote_thumbnail(picker, target, image).map(|encoded| LoadedThumbnail {
        protocol: encoded.protocol,
        render_size: encoded.render_size,
        decoded_bytes: encoded.decoded_bytes,
        local_fingerprint: None,
        deferred_local_frame: None,
        deferred_local_preview: None,
    })
}

/// Loads one local derivative or builds a new bounded preview for this exact
/// terminal pixel box.
fn load_local_thumbnail(
    mut cache: Option<&mut ThumbnailCache>,
    picker: &Picker,
    target: &ThumbnailTarget,
) -> Result<LoadedThumbnail, ThumbnailFailure> {
    let path = target
        .source
        .to_file_path()
        .map_err(|()| ThumbnailFailure::InvalidSource)?;
    let fingerprint = LocalThumbnailFingerprint::capture(&path)?;
    let preview_target = local_preview_target(picker, target.area);
    let cache_key = fingerprint.preview_cache_key(preview_target);

    if let Some(cache) = cache.as_deref_mut()
        && let Some(bytes) = cache.read_key(&cache_key).ok().flatten()
    {
        if let Some(image) = decode_local_preview_record(&bytes)
            && fingerprint.is_current()
        {
            let encoded = encode_thumbnail(picker, target.area, image)?;
            if !fingerprint.is_current() {
                return Err(ThumbnailFailure::InvalidImage);
            }
            return Ok(LoadedThumbnail {
                protocol: encoded.protocol,
                render_size: encoded.render_size,
                decoded_bytes: encoded.decoded_bytes,
                local_fingerprint: Some(fingerprint),
                deferred_local_frame: None,
                deferred_local_preview: None,
            });
        }
        cache.remove_key(&cache_key);
    }

    let image = decode_local_thumbnail_path(&fingerprint.canonical_path, preview_target)?;
    let image = prefit_thumbnail(image, preview_target);
    if !fingerprint.is_current() {
        return Err(ThumbnailFailure::InvalidImage);
    }
    let record = cache
        .is_some()
        .then(|| encode_local_preview_record(&image))
        .flatten();
    let encoded = encode_thumbnail(picker, target.area, image)?;
    if !fingerprint.is_current() {
        return Err(ThumbnailFailure::InvalidImage);
    }
    Ok(LoadedThumbnail {
        protocol: encoded.protocol,
        render_size: encoded.render_size,
        decoded_bytes: encoded.decoded_bytes,
        local_fingerprint: Some(fingerprint.clone()),
        deferred_local_frame: None,
        deferred_local_preview: record.map(|record| DeferredLocalPreview {
            cache_key,
            record,
            fingerprint,
        }),
    })
}

/// Loads a cached local-video derivative or extracts one bounded midpoint
/// frame and derives the exact terminal-size preview from it.
fn load_local_video_thumbnail(
    mut cache: Option<&mut ThumbnailCache>,
    picker: &Picker,
    target: &ThumbnailTarget,
    midpoint: LocalVideoMidpoint,
    extractor: &mut impl LocalVideoFrameExtractor,
    cancellation: &RequestCancellation,
) -> Result<LoadedThumbnail, ThumbnailFailure> {
    let path = target
        .source
        .to_file_path()
        .map_err(|()| ThumbnailFailure::InvalidSource)?;
    let fingerprint = LocalThumbnailFingerprint::capture(&path)?;
    let preview_target = local_preview_target(picker, target.area);
    let preview_cache_key = fingerprint.video_preview_cache_key(midpoint, preview_target);

    if let Some(cache) = cache.as_deref_mut()
        && let Some(bytes) = cache.read_key(&preview_cache_key).ok().flatten()
    {
        if let Some(image) = decode_local_preview_record(&bytes)
            && fingerprint.is_current()
            && !cancellation.is_cancelled()
        {
            let encoded = encode_thumbnail(picker, target.area, image)?;
            if cancellation.is_cancelled() {
                return Err(ThumbnailFailure::LocalVideoFrameExtractionFailed);
            }
            if !fingerprint.is_current() {
                return Err(ThumbnailFailure::InvalidImage);
            }
            return Ok(LoadedThumbnail {
                protocol: encoded.protocol,
                render_size: encoded.render_size,
                decoded_bytes: encoded.decoded_bytes,
                local_fingerprint: Some(fingerprint),
                deferred_local_frame: None,
                deferred_local_preview: None,
            });
        }
        cache.remove_key(&preview_cache_key);
    }

    if cancellation.is_cancelled() {
        return Err(ThumbnailFailure::LocalVideoFrameExtractionFailed);
    }
    let frame_cache_key = fingerprint.video_frame_cache_key(midpoint);
    let cache_enabled = cache.is_some();
    let mut deferred_local_frame = None;
    let cached_frame = cache
        .as_deref_mut()
        .and_then(|cache| cache.read_key(&frame_cache_key).ok().flatten());
    let frame = if let Some(bytes) = cached_frame {
        match decode_thumbnail(&bytes) {
            Ok(image) if fingerprint.is_current() && !cancellation.is_cancelled() => image,
            _ => {
                if let Some(cache) = cache.as_deref_mut() {
                    cache.remove_key(&frame_cache_key);
                }
                extract_local_video_frame(
                    extractor,
                    &fingerprint,
                    midpoint,
                    cancellation,
                    cache_enabled,
                    frame_cache_key,
                    &mut deferred_local_frame,
                )?
            }
        }
    } else {
        extract_local_video_frame(
            extractor,
            &fingerprint,
            midpoint,
            cancellation,
            cache_enabled,
            frame_cache_key,
            &mut deferred_local_frame,
        )?
    };

    let image = prefit_thumbnail(frame, preview_target);
    if cancellation.is_cancelled() {
        return Err(ThumbnailFailure::LocalVideoFrameExtractionFailed);
    }
    if !fingerprint.is_current() {
        return Err(ThumbnailFailure::InvalidImage);
    }
    let preview_record = cache
        .is_some()
        .then(|| encode_local_preview_record(&image))
        .flatten();
    let encoded = encode_thumbnail(picker, target.area, image)?;
    if cancellation.is_cancelled() {
        return Err(ThumbnailFailure::LocalVideoFrameExtractionFailed);
    }
    if !fingerprint.is_current() {
        return Err(ThumbnailFailure::InvalidImage);
    }
    Ok(LoadedThumbnail {
        protocol: encoded.protocol,
        render_size: encoded.render_size,
        decoded_bytes: encoded.decoded_bytes,
        local_fingerprint: Some(fingerprint.clone()),
        deferred_local_frame,
        deferred_local_preview: preview_record.map(|record| DeferredLocalPreview {
            cache_key: preview_cache_key,
            record,
            fingerprint,
        }),
    })
}

/// Extracts and validates a source frame before allowing any persistent write.
fn extract_local_video_frame(
    extractor: &mut impl LocalVideoFrameExtractor,
    fingerprint: &LocalThumbnailFingerprint,
    midpoint: LocalVideoMidpoint,
    cancellation: &RequestCancellation,
    cache_enabled: bool,
    frame_cache_key: [u8; 32],
    deferred: &mut Option<DeferredLocalPreview>,
) -> Result<DynamicImage, ThumbnailFailure> {
    let bytes = extractor.extract(&fingerprint.canonical_path, midpoint, cancellation)?;
    if bytes.is_empty()
        || bytes.len() > MAX_DOWNLOAD_BYTES
        || cancellation.is_cancelled()
        || !fingerprint.is_current()
    {
        return Err(ThumbnailFailure::InvalidImage);
    }
    let image = decode_thumbnail(&bytes)?;
    if cancellation.is_cancelled() || !fingerprint.is_current() {
        return Err(ThumbnailFailure::InvalidImage);
    }
    if cache_enabled {
        *deferred = Some(DeferredLocalPreview {
            cache_key: frame_cache_key,
            record: bytes,
            fingerprint: fingerprint.clone(),
        });
    }
    Ok(image)
}

/// Persists one already-rendered local derivative only while its source still
/// matches the key fingerprint.
fn persist_local_preview(cache: &ThumbnailCache, preview: &DeferredLocalPreview) {
    if !preview.fingerprint.is_current() {
        return;
    }
    if cache
        .store_key(&preview.cache_key, &preview.record)
        .is_err()
    {
        return;
    }
    if !preview.fingerprint.is_current() {
        cache.remove_key(&preview.cache_key);
    }
}

/// Loads and encodes one validated persistent-cache entry without network I/O.
///
/// `None` means the worker should proceed through its debounced network path.
/// Corrupt cached bytes are removed so the subsequent fetch can repair them.
fn load_cached_thumbnail(
    cache: Option<&mut ThumbnailCache>,
    picker: &Picker,
    target: &ThumbnailTarget,
) -> Option<Result<EncodedThumbnail, ThumbnailFailure>> {
    let cache = cache?;
    let bytes = cache.read(&target.source).ok().flatten()?;
    let image = match decode_thumbnail(&bytes) {
        Ok(image) => image,
        Err(_) => {
            cache.remove(&target.source);
            return None;
        }
    };
    Some(encode_remote_thumbnail(picker, target, image))
}

/// Removes confident YouTube-owned letterbox bands before terminal fitting.
///
/// Some YouTube `default`, `high`, and `standard` JPEGs use 4:3 canvases around
/// visible 16:9 artwork. Cropping only symmetric near-black bands avoids
/// changing non-dark 4:3 thumbnails while preventing those embedded pixels
/// from becoming empty terminal rows.
fn crop_youtube_letterbox(source: &Url, image: DynamicImage) -> DynamicImage {
    const MAX_DARK_LUMA: u32 = 24;
    const MAX_NON_DARK_PERCENT: u64 = 2;

    let eligible_source = source.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("img.youtube.com")
            || host.eq_ignore_ascii_case("ytimg.com")
            || host.ends_with(".ytimg.com")
    }) && source
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .is_some_and(|filename| {
            matches!(filename, "default.jpg" | "hqdefault.jpg" | "sddefault.jpg")
        });
    let width = image.width();
    let height = image.height();
    if !eligible_source || u64::from(width).saturating_mul(3) != u64::from(height).saturating_mul(4)
    {
        return image;
    }

    let visible_height =
        u32::try_from(u64::from(width).saturating_mul(9).saturating_add(8) / 16).unwrap_or(height);
    let total_padding = height.saturating_sub(visible_height);
    let top_padding = total_padding / 2;
    let bottom_padding = total_padding.saturating_sub(top_padding);
    if top_padding < 2 {
        return image;
    }

    let band_is_dark = |start_y: u32, rows: u32| {
        let total_pixels = u64::from(width).saturating_mul(u64::from(rows));
        let maximum_non_dark = total_pixels.saturating_mul(MAX_NON_DARK_PERCENT) / 100;
        let mut non_dark = 0_u64;
        for y in start_y..start_y.saturating_add(rows) {
            for x in 0..width {
                let [red, green, blue, _] = image.get_pixel(x, y).0;
                let luma = (u32::from(red).saturating_mul(54)
                    + u32::from(green).saturating_mul(183)
                    + u32::from(blue).saturating_mul(19))
                    / 256;
                if luma > MAX_DARK_LUMA {
                    non_dark = non_dark.saturating_add(1);
                    if non_dark > maximum_non_dark {
                        return false;
                    }
                }
            }
        }
        true
    };
    if !band_is_dark(0, top_padding)
        || !band_is_dark(height.saturating_sub(bottom_padding), bottom_padding)
    {
        return image;
    }

    image.crop_imm(0, top_padding, width, visible_height)
}

/// Applies remote-source normalization before the ordinary terminal fit.
fn encode_remote_thumbnail(
    picker: &Picker,
    target: &ThumbnailTarget,
    image: DynamicImage,
) -> Result<EncodedThumbnail, ThumbnailFailure> {
    encode_thumbnail(
        picker,
        target.area,
        crop_youtube_letterbox(&target.source, image),
    )
}

/// Aspect-fits and encodes a decoded image for its exact render area.
fn encode_thumbnail(
    picker: &Picker,
    area: Rect,
    image: DynamicImage,
) -> Result<EncodedThumbnail, ThumbnailFailure> {
    // `StatefulProtocol` owns its source image. Retaining only the pixels that
    // can fit this terminal area bounds the prepared-image LRU and avoids
    // repeating a full-resolution resize in protocol encoders such as Sixel.
    let image = prefit_thumbnail(image, local_preview_target(picker, area));
    let decoded_bytes = image.as_bytes().len();
    let mut protocol = picker.new_resize_protocol(image);
    let render_size = protocol.size_for(Resize::Fit(None), area.into());
    if render_size.width == 0 || render_size.height == 0 {
        return Err(ThumbnailFailure::EncodingFailed);
    }
    // `StatefulImage` re-encodes synchronously whenever its render rectangle
    // differs from the protocol's latest encoded size. Preparing the fitted
    // size here lets the TUI render this protocol without doing image work.
    protocol.resize_encode(&Resize::Fit(None), render_size);
    match protocol.last_encoding_result() {
        Some(Ok(())) => Ok(EncodedThumbnail {
            protocol,
            render_size,
            decoded_bytes,
            local_fingerprint: None,
        }),
        Some(Err(_)) | None => Err(ThumbnailFailure::EncodingFailed),
    }
}

/// Converts one terminal-cell rectangle into the exact corresponding pixel
/// box, using the terminal's detected font dimensions.
fn local_preview_target(picker: &Picker, area: Rect) -> LocalPreviewTarget {
    let font = picker.font_size();
    LocalPreviewTarget {
        width: u32::from(area.width)
            .saturating_mul(u32::from(font.width))
            .max(1),
        height: u32::from(area.height)
            .saturating_mul(u32::from(font.height))
            .max(1),
    }
}

/// Decodes a regular local image without first copying its encoded file into
/// a bounded network-download buffer.
///
/// JPEG uses decoder-side DCT scaling toward the requested terminal pixels.
/// PNG and WebP retain [`decode_thumbnail_reader`]'s original dimension and
/// allocation policy.
fn decode_local_thumbnail_path(
    path: &Path,
    target: LocalPreviewTarget,
) -> Result<DynamicImage, ThumbnailFailure> {
    #[cfg(test)]
    record_local_source_decode(path);
    let reader = ImageReader::open(path).map_err(|_| ThumbnailFailure::DownloadFailed)?;
    let reader = reader
        .with_guessed_format()
        .map_err(|_| ThumbnailFailure::InvalidImage)?;
    match reader.format() {
        Some(ImageFormat::Jpeg) => decode_scaled_local_jpeg(reader.into_inner(), target),
        Some(ImageFormat::Png | ImageFormat::WebP) => decode_thumbnail_reader(reader),
        Some(_) => Err(ThumbnailFailure::UnsupportedFormat),
        None => Err(ThumbnailFailure::InvalidImage),
    }
}

#[cfg(test)]
fn record_local_source_decode(path: &Path) {
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut counts = LOCAL_SOURCE_DECODE_COUNTS
        .lock()
        .expect("local thumbnail decode counter");
    if let Some((_, count)) = counts.iter_mut().find(|(candidate, _)| candidate == &path) {
        *count = count.saturating_add(1);
    } else {
        counts.push((path, 1));
    }
}

/// Uses the JPEG decoder's bounded 1/8, 1/4, or 1/2 IDCT path before the
/// ordinary exact fit. Oversized source dimensions are accepted only when the
/// scaled output itself remains inside Youta's existing decode budget.
fn decode_scaled_local_jpeg<R: Read>(
    reader: R,
    target: LocalPreviewTarget,
) -> Result<DynamicImage, ThumbnailFailure> {
    let mut decoder = JpegDecoder::new(reader);
    decoder.set_max_decoding_buffer_size(
        usize::try_from(MAX_DECODE_ALLOC_BYTES).unwrap_or(usize::MAX),
    );
    decoder
        .read_info()
        .map_err(|_| ThumbnailFailure::InvalidImage)?;
    let original = decoder.info().ok_or(ThumbnailFailure::InvalidImage)?;

    let output = if original.coding_process == CodingProcess::Lossless {
        original
    } else {
        let mut requested_width =
            u16::try_from(target.width.min(u32::from(u16::MAX))).unwrap_or(u16::MAX);
        let mut requested_height =
            u16::try_from(target.height.min(u32::from(u16::MAX))).unwrap_or(u16::MAX);
        loop {
            decoder
                .scale(requested_width.max(1), requested_height.max(1))
                .map_err(|_| ThumbnailFailure::InvalidImage)?;
            let scaled = decoder.info().ok_or(ThumbnailFailure::InvalidImage)?;
            if jpeg_output_is_bounded(scaled) {
                break scaled;
            }
            if requested_width == 1 && requested_height == 1 {
                return Err(ThumbnailFailure::InvalidImage);
            }
            requested_width = (requested_width / 2).max(1);
            requested_height = (requested_height / 2).max(1);
        }
    };
    if !jpeg_output_is_bounded(output) {
        return Err(ThumbnailFailure::InvalidImage);
    }

    let pixels = decoder
        .decode()
        .map_err(|_| ThumbnailFailure::InvalidImage)?;
    let output = decoder.info().ok_or(ThumbnailFailure::InvalidImage)?;
    jpeg_pixels_to_image(output, pixels)
}

/// Checks both retained output and temporary conversion bytes against the
/// decoded-allocation budget.
fn jpeg_output_is_bounded(info: jpeg_decoder::ImageInfo) -> bool {
    let working_bytes_per_pixel = match info.pixel_format {
        PixelFormat::L8 => 1_u64,
        PixelFormat::L16 => 4,
        PixelFormat::RGB24 => 3,
        PixelFormat::CMYK32 => 7,
    };
    u32::from(info.width) <= MAX_IMAGE_DIMENSION
        && u32::from(info.height) <= MAX_IMAGE_DIMENSION
        && u64::from(info.width)
            .checked_mul(u64::from(info.height))
            .and_then(|pixels| pixels.checked_mul(working_bytes_per_pixel))
            .is_some_and(|bytes| bytes <= MAX_DECODE_ALLOC_BYTES)
}

/// Converts one validated JPEG decoder output without another copy for the
/// common grayscale and RGB paths.
fn jpeg_pixels_to_image(
    info: jpeg_decoder::ImageInfo,
    pixels: Vec<u8>,
) -> Result<DynamicImage, ThumbnailFailure> {
    let width = u32::from(info.width);
    let height = u32::from(info.height);
    match info.pixel_format {
        PixelFormat::L8 => GrayImage::from_raw(width, height, pixels)
            .map(DynamicImage::ImageLuma8)
            .ok_or(ThumbnailFailure::InvalidImage),
        PixelFormat::RGB24 => RgbImage::from_raw(width, height, pixels)
            .map(DynamicImage::ImageRgb8)
            .ok_or(ThumbnailFailure::InvalidImage),
        PixelFormat::L16 => {
            let samples = pixels
                .chunks_exact(2)
                .map(|sample| u16::from_ne_bytes([sample[0], sample[1]]))
                .collect::<Vec<_>>();
            if !pixels.len().is_multiple_of(2) {
                return Err(ThumbnailFailure::InvalidImage);
            }
            image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_raw(width, height, samples)
                .map(DynamicImage::ImageLuma16)
                .ok_or(ThumbnailFailure::InvalidImage)
        }
        PixelFormat::CMYK32 => {
            if !pixels.len().is_multiple_of(4) {
                return Err(ThumbnailFailure::InvalidImage);
            }
            let mut rgb = Vec::with_capacity(pixels.len().saturating_sub(pixels.len() / 4));
            for pixel in pixels.chunks_exact(4) {
                let black = u16::from(255_u8.saturating_sub(pixel[3]));
                for component in &pixel[..3] {
                    let inverted = u16::from(255_u8.saturating_sub(*component));
                    rgb.push(u8::try_from(inverted * black / 255).unwrap_or(u8::MAX));
                }
            }
            RgbImage::from_raw(width, height, rgb)
                .map(DynamicImage::ImageRgb8)
                .ok_or(ThumbnailFailure::InvalidImage)
        }
    }
}

/// Fits a decoded image before constructing `StatefulProtocol`, preventing the
/// protocol from retaining a full-resolution source that it will never show.
fn prefit_thumbnail(image: DynamicImage, target: LocalPreviewTarget) -> DynamicImage {
    let width = target.width.min(image.width()).max(1);
    let height = target.height.min(image.height()).max(1);
    if image.width() <= width && image.height() <= height {
        image
    } else {
        image.resize(width, height, image::imageops::FilterType::Nearest)
    }
}

fn decode_thumbnail(bytes: &[u8]) -> Result<DynamicImage, ThumbnailFailure> {
    decode_thumbnail_reader(ImageReader::new(Cursor::new(bytes)))
}

/// Applies Youta's format, dimension, and decoded-allocation policy to an
/// image reader backed by memory or a streaming local file.
fn decode_thumbnail_reader<R>(reader: ImageReader<R>) -> Result<DynamicImage, ThumbnailFailure>
where
    R: BufRead + Seek,
{
    let mut reader = reader
        .with_guessed_format()
        .map_err(|_| ThumbnailFailure::InvalidImage)?;
    if !matches!(
        reader.format(),
        Some(ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP)
    ) {
        return Err(ThumbnailFailure::UnsupportedFormat);
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
    reader.limits(limits);
    reader.decode().map_err(|_| ThumbnailFailure::InvalidImage)
}

fn queried_picker() -> Option<(Picker, ThumbnailProtocol)> {
    let picker = Picker::from_query_stdio().ok()?;
    let protocol = match picker.protocol_type() {
        ProtocolType::Kitty => ThumbnailProtocol::Kitty,
        ProtocolType::Iterm2 => ThumbnailProtocol::Iterm2,
        ProtocolType::Sixel => ThumbnailProtocol::Sixel,
        ProtocolType::Halfblocks => return None,
    };
    Some((picker, protocol))
}

fn picker_for_protocol(protocol: ThumbnailProtocol, font_size: (u16, u16)) -> Picker {
    #[allow(deprecated)]
    let mut picker = Picker::from_fontsize(font_size.into());
    picker.set_protocol_type(protocol.ratatui());
    picker
}

fn terminal_font_size() -> Option<(u16, u16)> {
    let size = crossterm::terminal::window_size().ok()?;
    if size.columns == 0 || size.rows == 0 || size.width == 0 || size.height == 0 {
        return None;
    }
    Some((
        (size.width / size.columns).max(1),
        (size.height / size.rows).max(1),
    ))
}

fn is_serial_terminal(path: &Path) -> bool {
    let text = path.to_string_lossy();
    if text == "/dev/console" {
        return true;
    }
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    ["ttyS", "ttyUSB", "ttyACM", "rfcomm"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Instant;

    use super::*;

    pub(crate) type MockManagerParts = (
        ThumbnailManager,
        Sender<Result<Vec<u8>, ThumbnailFailure>>,
        Receiver<Url>,
    );

    struct MockTransport {
        observed: Sender<Url>,
        replies: Receiver<Result<Vec<u8>, ThumbnailFailure>>,
    }

    impl ThumbnailTransport for MockTransport {
        fn fetch(&mut self, source: &Url) -> Result<Vec<u8>, ThumbnailFailure> {
            self.observed
                .send(source.clone())
                .map_err(|_| ThumbnailFailure::WorkerStopped)?;
            self.replies
                .recv_timeout(Duration::from_secs(2))
                .unwrap_or(Err(ThumbnailFailure::WorkerStopped))
        }
    }

    struct RejectingTransport;

    impl ThumbnailTransport for RejectingTransport {
        fn fetch(&mut self, _source: &Url) -> Result<Vec<u8>, ThumbnailFailure> {
            Err(ThumbnailFailure::DownloadFailed)
        }
    }

    struct MockVideoExtractor {
        observed: Sender<(PathBuf, u64)>,
        replies: Receiver<Result<Vec<u8>, ThumbnailFailure>>,
        cancelled: Sender<PathBuf>,
    }

    impl LocalVideoFrameExtractor for MockVideoExtractor {
        fn extract(
            &mut self,
            path: &Path,
            midpoint: LocalVideoMidpoint,
            cancellation: &RequestCancellation,
        ) -> Result<Vec<u8>, ThumbnailFailure> {
            self.observed
                .send((path.to_path_buf(), midpoint.0))
                .map_err(|_| ThumbnailFailure::WorkerStopped)?;
            loop {
                if cancellation.is_cancelled() {
                    let _ = self.cancelled.send(path.to_path_buf());
                    return Err(ThumbnailFailure::DownloadFailed);
                }
                match self.replies.recv_timeout(Duration::from_millis(5)) {
                    Ok(reply) => return reply,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        return Err(ThumbnailFailure::WorkerStopped);
                    }
                }
            }
        }
    }

    fn graphical_terminal() -> TerminalInfo {
        TerminalInfo {
            // The console rules are what several of these tests are about, so
            // the base fixture claims Linux and the build target is not
            // allowed to decide the answer for them.
            linux: true,
            stdin_is_terminal: true,
            stdout_is_terminal: true,
            term: Some("xterm-kitty".to_owned()),
            term_program: None,
            lc_terminal: None,
            kitty_window: true,
            wezterm_pane: false,
            tmux: false,
            ssh: false,
            output_device: Some(PathBuf::from("/dev/pts/7")),
            font_size: Some((9, 18)),
        }
    }

    #[test]
    fn automatic_detection_accepts_only_known_graphics_protocols() {
        let kitty = graphical_terminal();
        assert_eq!(kitty.environment_protocol(), Some(ThumbnailProtocol::Kitty));

        let wezterm = TerminalInfo {
            term: Some("xterm-256color".to_owned()),
            term_program: Some("WezTerm".to_owned()),
            kitty_window: false,
            wezterm_pane: true,
            ..kitty.clone()
        };
        assert_eq!(
            wezterm.environment_protocol(),
            Some(ThumbnailProtocol::Iterm2)
        );

        let foot = TerminalInfo {
            term: Some("foot".to_owned()),
            term_program: Some("foot".to_owned()),
            kitty_window: false,
            ..kitty.clone()
        };
        assert_eq!(foot.environment_protocol(), Some(ThumbnailProtocol::Sixel));

        let linux_console = TerminalInfo {
            term: Some("linux".to_owned()),
            kitty_window: false,
            output_device: Some(PathBuf::from("/dev/tty3")),
            ..kitty.clone()
        };
        assert_eq!(
            linux_console.environment_protocol(),
            Some(ThumbnailProtocol::Halfblocks)
        );

        let unknown = TerminalInfo {
            term: Some("xterm-256color".to_owned()),
            kitty_window: false,
            ..kitty
        };
        assert_eq!(unknown.environment_protocol(), None);
    }

    #[test]
    fn automatic_detection_rejects_unconfirmed_consoles_serial_ssh_and_tmux() {
        for terminal in [
            TerminalInfo {
                term: Some("linux".to_owned()),
                kitty_window: false,
                output_device: Some(PathBuf::from("/dev/pts/1")),
                ..graphical_terminal()
            },
            TerminalInfo {
                term: Some("xterm-kitty".to_owned()),
                output_device: Some(PathBuf::from("/dev/ttyUSB0")),
                ..graphical_terminal()
            },
            TerminalInfo {
                tmux: true,
                ..graphical_terminal()
            },
            TerminalInfo {
                term: Some("linux".to_owned()),
                kitty_window: false,
                ssh: true,
                output_device: Some(PathBuf::from("/dev/tty4")),
                ..graphical_terminal()
            },
            TerminalInfo {
                term: None,
                ..graphical_terminal()
            },
            TerminalInfo {
                stdout_is_terminal: false,
                ..graphical_terminal()
            },
        ] {
            assert!(terminal.hard_unsupported());
            assert_eq!(terminal.environment_protocol(), None);
        }
    }

    #[test]
    fn disabled_and_unsupported_managers_never_start_loading() {
        let disabled =
            ThumbnailManager::from_terminal_info(ThumbnailMode::Off, &graphical_terminal());
        assert_eq!(disabled.capability(), ThumbnailCapability::Disabled);
        assert_eq!(disabled.state(), &ThumbnailState::Disabled);
        assert!(!disabled.is_enabled());

        let unsupported_info = TerminalInfo {
            term: Some("linux".to_owned()),
            kitty_window: false,
            output_device: Some(PathBuf::from("/dev/pts/2")),
            ..graphical_terminal()
        };
        let mut unsupported =
            ThumbnailManager::from_terminal_info(ThumbnailMode::Auto, &unsupported_info);
        let url = Url::parse("https://images.example/thumbnail.jpg").expect("fixture URL");
        assert!(!unsupported.synchronize(Some(&url), Rect::new(0, 0, 20, 8)));
        assert_eq!(unsupported.state(), &ThumbnailState::Unsupported);
    }

    #[test]
    fn confirmed_linux_console_uses_halfblocks_without_terminal_queries() {
        let console = TerminalInfo {
            term: Some("linux".to_owned()),
            kitty_window: false,
            output_device: Some(PathBuf::from("/dev/tty2")),
            font_size: None,
            ..graphical_terminal()
        };

        let manager = ThumbnailManager::from_terminal_info(ThumbnailMode::Auto, &console);

        assert_eq!(
            manager.capability(),
            ThumbnailCapability::Supported(ThumbnailProtocol::Halfblocks)
        );
        assert_eq!(manager.state(), &ThumbnailState::Idle);
        assert_eq!(
            manager.picker.as_ref().map(Picker::protocol_type),
            Some(ProtocolType::Halfblocks)
        );
    }

    #[test]
    fn tty_image_preference_disables_only_confirmed_linux_console_artwork() {
        let directory = tempfile::tempdir().expect("temporary config directory");
        let cache_directory = directory.path().join("thumbnail-cache");
        let console = TerminalInfo {
            term: Some("linux".to_owned()),
            kitty_window: false,
            output_device: Some(PathBuf::from("/dev/tty2")),
            font_size: None,
            ..graphical_terminal()
        };
        let mut disabled = ThumbnailManager::from_terminal_info_with_cache(
            ThumbnailMode::Auto,
            &console,
            Some(cache_directory.clone()),
            false,
        );
        let source = Url::parse("https://images.example/unused.png").expect("fixture URL");

        assert_eq!(disabled.capability(), ThumbnailCapability::Disabled);
        assert_eq!(disabled.state(), &ThumbnailState::Disabled);
        assert!(!disabled.is_enabled());
        assert!(!disabled.synchronize(Some(&source), Rect::new(0, 0, 20, 8)));
        assert!(!disabled.synchronize_prefetch(std::slice::from_ref(&source)));
        assert!(
            !cache_directory.exists(),
            "disabled physical-TTY artwork must not initialize its cache"
        );

        let graphical = ThumbnailManager::from_terminal_info_with_cache(
            ThumbnailMode::Auto,
            &graphical_terminal(),
            None,
            false,
        );
        assert_eq!(
            graphical.capability(),
            ThumbnailCapability::Supported(ThumbnailProtocol::Kitty),
            "the physical-TTY preference must not disable graphical terminals"
        );
    }

    #[test]
    fn supported_manager_defers_its_worker_until_artwork_is_visible() {
        let manager =
            ThumbnailManager::from_terminal_info(ThumbnailMode::Auto, &graphical_terminal());

        assert!(manager.is_enabled());
        assert_eq!(manager.state(), &ThumbnailState::Idle);
        assert!(manager.picker.is_some());
        assert!(manager.request_sender.is_none());
        assert!(manager.result_receiver.is_none());
    }

    #[test]
    fn unsupported_terminal_never_creates_the_persistent_cache() {
        let directory = tempfile::tempdir().expect("temporary config directory");
        let cache_directory = directory.path().join("thumbnail-cache");
        let terminal = TerminalInfo {
            term: Some("linux".to_owned()),
            kitty_window: false,
            output_device: Some(PathBuf::from("/dev/pts/3")),
            ..graphical_terminal()
        };
        let mut manager = ThumbnailManager::from_terminal_info_with_cache(
            ThumbnailMode::Auto,
            &terminal,
            Some(cache_directory.clone()),
            true,
        );
        let source = Url::parse("https://images.example/unused.png").expect("fixture URL");

        assert!(!manager.synchronize(Some(&source), Rect::new(0, 0, 20, 8)));
        assert!(!cache_directory.exists());
    }

    #[test]
    fn decoder_accepts_bounded_png_and_rejects_other_or_oversized_images() {
        let mut png = Cursor::new(Vec::new());
        DynamicImage::new_rgba8(4, 3)
            .write_to(&mut png, ImageFormat::Png)
            .expect("encode fixture PNG");
        let decoded = decode_thumbnail(png.get_ref()).expect("decode fixture PNG");
        assert_eq!((decoded.width(), decoded.height()), (4, 3));

        assert_eq!(
            decode_thumbnail(b"GIF89a").expect_err("GIF must be rejected"),
            ThumbnailFailure::UnsupportedFormat
        );
        assert_eq!(
            decode_thumbnail(b"not an image").expect_err("garbage must be rejected"),
            ThumbnailFailure::UnsupportedFormat
        );

        let mut oversized = Cursor::new(Vec::new());
        DynamicImage::new_luma8(MAX_IMAGE_DIMENSION + 1, 1)
            .write_to(&mut oversized, ImageFormat::Png)
            .expect("encode oversized fixture");
        assert_eq!(
            decode_thumbnail(oversized.get_ref()).expect_err("dimensions must be bounded"),
            ThumbnailFailure::InvalidImage
        );
    }

    #[test]
    fn thumbnail_fetch_accepts_mock_image_bytes_and_rejects_oversized_responses() {
        let mut png = Cursor::new(Vec::new());
        DynamicImage::new_rgba8(3, 2)
            .write_to(&mut png, ImageFormat::Png)
            .expect("encode fixture PNG");
        let (source, server) = serve_once("200 OK", Vec::new(), png.into_inner());
        let bytes = fetch_thumbnail_with_policy(&mock_thumbnail_agent(), &source, true)
            .expect("fetch bounded fixture image");
        server.join().expect("fixture image server");
        let decoded = decode_thumbnail(&bytes).expect("decode fetched fixture");
        assert_eq!((decoded.width(), decoded.height()), (3, 2));

        let (oversized, server) = serve_once(
            "200 OK",
            vec![(
                "Content-Length".to_owned(),
                MAX_DOWNLOAD_BYTES.saturating_add(1).to_string(),
            )],
            Vec::new(),
        );
        assert_eq!(
            fetch_thumbnail_with_policy(&mock_thumbnail_agent(), &oversized, true)
                .expect_err("oversized response must be rejected"),
            ThumbnailFailure::ResponseTooLarge
        );
        server.join().expect("oversized fixture server");

        let directory = tempfile::tempdir().expect("local thumbnail directory");
        let local_path = directory.path().join("cover.png");
        fs::write(&local_path, &bytes).expect("write local thumbnail fixture");
        let file = Url::from_file_path(&local_path).expect("fixture file URL");
        assert_eq!(
            fetch_thumbnail(&thumbnail_agent(), &file).expect("read local image in place"),
            bytes
        );

        let ftp = Url::parse("ftp://example.com/cover.png").expect("fixture FTP URL");
        assert_eq!(
            fetch_thumbnail(&thumbnail_agent(), &ftp)
                .expect_err("non-HTTP and non-file source must be rejected"),
            ThumbnailFailure::InvalidSource
        );
    }

    #[test]
    fn thumbnail_fetch_rejects_non_public_hosts_and_does_not_follow_redirects() {
        for raw in [
            "http://127.0.0.1/private.png",
            "https://10.0.0.1/private.png",
            "https://169.254.169.254/private.png",
            "https://[::1]/private.png",
            "https://artwork.local/private.png",
            "https://artwork.service.internal/private.png",
            "https://intranet/private.png",
        ] {
            let source = Url::parse(raw).expect("non-public thumbnail URL");
            assert_eq!(
                fetch_thumbnail(&thumbnail_agent(), &source)
                    .expect_err("non-public thumbnail host must be rejected"),
                ThumbnailFailure::InvalidSource,
                "unsafe thumbnail source was accepted: {raw}"
            );
        }

        let (redirect, server) = serve_once(
            "302 Found",
            vec![(
                "Location".to_owned(),
                "http://169.254.169.254/private.png".to_owned(),
            )],
            Vec::new(),
        );
        assert_eq!(
            fetch_thumbnail_with_policy(&mock_thumbnail_agent(), &redirect, true)
                .expect_err("thumbnail redirects must not be followed"),
            ThumbnailFailure::DownloadFailed
        );
        server.join().expect("redirect fixture server");
    }

    #[test]
    fn local_thumbnail_streams_encoded_files_larger_than_the_remote_limit() {
        let directory = tempfile::tempdir().expect("local thumbnail directory");
        let local_path = directory.path().join("large-cover.png");
        let mut png = Cursor::new(Vec::new());
        DynamicImage::new_rgba8(3, 2)
            .write_to(&mut png, ImageFormat::Png)
            .expect("encode fixture PNG");
        fs::write(&local_path, png.into_inner()).expect("write local thumbnail fixture");
        OpenOptions::new()
            .write(true)
            .open(&local_path)
            .expect("open local thumbnail fixture")
            .set_len(
                u64::try_from(MAX_DOWNLOAD_BYTES.saturating_add(1)).expect("remote limit fits u64"),
            )
            .expect("pad local image beyond remote download limit");
        let decoded = decode_local_thumbnail_path(
            &local_path,
            LocalPreviewTarget {
                width: 100,
                height: 100,
            },
        )
        .expect("stream local image without encoded-size limit");

        assert_eq!((decoded.width(), decoded.height()), (3, 2));
        assert!(
            fs::metadata(local_path)
                .expect("local thumbnail metadata")
                .len()
                > MAX_DOWNLOAD_BYTES as u64
        );
    }

    #[test]
    fn oversized_local_jpeg_scales_within_the_existing_decode_budget() {
        let directory = tempfile::tempdir().expect("large local JPEG directory");
        let local_path = directory.path().join("5663x2753.jpg");
        write_jpeg_fixture(&local_path, 5_663, 2_753);
        let target = LocalPreviewTarget {
            width: 1_200,
            height: 800,
        };

        assert_eq!(
            decode_thumbnail_reader(
                ImageReader::open(&local_path).expect("open oversized JPEG through image crate")
            )
            .expect_err("the generic decoder must retain its original dimension limit"),
            ThumbnailFailure::InvalidImage
        );

        let scaled = decode_local_thumbnail_path(&local_path, target)
            .expect("decoder-side scaling must accept the oversized local JPEG");
        assert!(scaled.width() <= MAX_IMAGE_DIMENSION);
        assert!(scaled.height() <= MAX_IMAGE_DIMENSION);
        assert!(
            u64::from(scaled.width())
                .checked_mul(u64::from(scaled.height()))
                .and_then(|pixels| pixels.checked_mul(u64::from(scaled.color().bytes_per_pixel())))
                .is_some_and(|bytes| bytes <= MAX_DECODE_ALLOC_BYTES)
        );

        let fitted = prefit_thumbnail(scaled, target);
        assert!(fitted.width() <= target.width);
        assert!(fitted.height() <= target.height);
    }

    #[test]
    fn local_preview_cache_survives_restart_without_decoding_source_again() {
        let directory = tempfile::tempdir().expect("local preview cache directory");
        let local_path = directory.path().join("cover.jpg");
        let cache_directory = directory.path().join("thumbnail-cache");
        write_jpeg_fixture(&local_path, 1_600, 900);
        let source = Url::from_file_path(&local_path).expect("local JPEG URL");
        let area = Rect::new(0, 0, 80, 24);

        let mut first = local_thumbnail_manager(cache_directory.clone());
        assert!(first.synchronize(Some(&source), area));
        assert_eq!(wait_for_terminal_state(&mut first), ThumbnailState::Ready);
        let cache_key = local_preview_cache_key(&local_path, area);
        wait_for_local_preview(&cache_directory, &cache_key);
        assert_eq!(local_source_decode_count(&local_path), 1);
        drop(first);

        let mut restarted = local_thumbnail_manager(cache_directory);
        assert!(restarted.synchronize(Some(&source), area));
        assert_eq!(
            wait_for_terminal_state(&mut restarted),
            ThumbnailState::Ready
        );
        assert_eq!(
            local_source_decode_count(&local_path),
            1,
            "the restarted manager must decode the persisted derivative, not the source"
        );
    }

    #[test]
    fn local_preview_cache_invalidates_after_source_mutation() {
        let directory = tempfile::tempdir().expect("local preview mutation directory");
        let local_path = directory.path().join("cover.jpg");
        let cache_directory = directory.path().join("thumbnail-cache");
        let area = Rect::new(0, 0, 64, 20);
        write_jpeg_fixture(&local_path, 1_200, 675);
        render_and_wait_for_local_preview(&local_path, &cache_directory, area);
        let old_key = local_preview_cache_key(&local_path, area);
        assert_eq!(local_source_decode_count(&local_path), 1);

        write_jpeg_fixture(&local_path, 1_280, 720);
        let new_key = local_preview_cache_key(&local_path, area);
        assert_ne!(old_key, new_key);
        render_and_wait_for_local_preview(&local_path, &cache_directory, area);
        assert_eq!(
            local_source_decode_count(&local_path),
            2,
            "changed source metadata must select a new derivative key"
        );
    }

    #[test]
    fn corrupt_local_preview_is_removed_and_regenerated() {
        let directory = tempfile::tempdir().expect("corrupt local preview directory");
        let local_path = directory.path().join("cover.jpg");
        let cache_directory = directory.path().join("thumbnail-cache");
        let area = Rect::new(0, 0, 64, 20);
        write_jpeg_fixture(&local_path, 1_200, 675);
        render_and_wait_for_local_preview(&local_path, &cache_directory, area);
        let cache_key = local_preview_cache_key(&local_path, area);
        let cache = ThumbnailCache::new(cache_directory.clone());
        fs::write(cache.entry_path_for_key(&cache_key), b"corrupt preview")
            .expect("corrupt persisted local preview");

        render_and_wait_for_local_preview(&local_path, &cache_directory, area);
        assert_eq!(
            local_source_decode_count(&local_path),
            2,
            "a corrupt derivative must fall back to the original local image"
        );
        let repaired = cache
            .read_key(&cache_key)
            .expect("read repaired derivative")
            .expect("repaired derivative exists");
        assert!(decode_local_preview_record(&repaired).is_some());
    }

    #[cfg(feature = "local-video-thumbnails")]
    #[test]
    fn ffmpeg_midpoint_command_is_shell_free_and_extracts_one_bounded_frame() {
        use std::ffi::{OsStr, OsString};

        let path = Path::new("/tmp/a movie;not-a-command.MOV");
        let command =
            local_video_frame_command(Path::new("ffmpeg"), path, LocalVideoMidpoint(60_500));
        let expected = [
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-ss",
            "60.500",
            "-i",
            "/tmp/a movie;not-a-command.MOV",
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-an",
            "-sn",
            "-dn",
            "-vf",
            "scale=w=1280:h=1280:force_original_aspect_ratio=decrease:\
             force_divisible_by=2:flags=fast_bilinear,format=yuvj420p",
            "-c:v",
            "mjpeg",
            "-q:v",
            "5",
            "-f",
            "image2pipe",
            "pipe:1",
        ]
        .map(OsString::from);

        assert_eq!(command.get_program(), OsStr::new("ffmpeg"));
        assert_eq!(
            command.get_args().map(OsStr::to_owned).collect::<Vec<_>>(),
            expected
        );
    }

    #[cfg(all(feature = "local-video-thumbnails", unix))]
    #[test]
    fn ffmpeg_extracts_the_midpoint_frame_from_a_real_mov_fixture() {
        let available = Command::new("ffmpeg")
            .arg("-version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !available {
            return;
        }
        let directory = tempfile::tempdir().expect("real MOV fixture directory");
        let movie = directory.path().join("two-colours.mov");
        let status = Command::new("ffmpeg")
            .arg("-nostdin")
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-f")
            .arg("lavfi")
            .arg("-i")
            .arg("color=c=red:s=64x36:d=1:r=10")
            .arg("-f")
            .arg("lavfi")
            .arg("-i")
            .arg("color=c=green:s=64x36:d=3:r=10")
            .arg("-filter_complex")
            .arg("[0:v][1:v]concat=n=2:v=1:a=0,format=yuv420p")
            .arg("-c:v")
            .arg("mpeg4")
            .arg("-y")
            .arg(&movie)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run FFmpeg MOV fixture encoder");
        assert!(status.success(), "FFmpeg must encode the MOV fixture");

        let generation = Arc::new(AtomicU64::new(7));
        let cancellation = RequestCancellation {
            generation: 7,
            current_generation: generation,
        };
        let bytes = FfmpegVideoFrameExtractor::default()
            .extract(&movie, LocalVideoMidpoint(2_000), &cancellation)
            .expect("extract real MOV midpoint");
        let image = decode_thumbnail(&bytes)
            .expect("decode extracted real MOV midpoint")
            .to_rgb8();
        let pixel = image.get_pixel(image.width() / 2, image.height() / 2);

        assert!(
            pixel[1] > pixel[0].saturating_mul(2) && pixel[1] > pixel[2].saturating_mul(2),
            "the two-second midpoint must come from the green segment: {pixel:?}"
        );
    }

    #[test]
    fn mov_thumbnail_uses_the_typed_millisecond_midpoint() {
        let directory = tempfile::tempdir().expect("MOV thumbnail directory");
        let movie = directory.path().join("holiday.MOV");
        fs::write(&movie, b"mock MOV container").expect("write mock MOV source");
        let (mut manager, replies, observed, _cancelled) = manager_with_mock_video_extractor(None);
        let area = Rect::new(0, 0, 40, 12);

        assert!(manager.synchronize_local_video(&movie, 60_500, area));
        assert_eq!(manager.state(), &ThumbnailState::Loading);
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("observe MOV extraction"),
            (
                fs::canonicalize(&movie).expect("canonical MOV path"),
                60_500
            )
        );
        replies
            .send(Ok(fixture_jpeg()))
            .expect("release MOV extraction");

        assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);
        assert_eq!(
            manager
                .target
                .as_ref()
                .and_then(|target| target.local_video_midpoint),
            Some(LocalVideoMidpoint(60_500))
        );
    }

    #[test]
    fn local_video_source_frame_cache_survives_restart_and_area_change() {
        let directory = tempfile::tempdir().expect("video frame cache directory");
        let movie = directory.path().join("cached.mov");
        let cache_directory = directory.path().join("thumbnail-cache");
        fs::write(&movie, b"stable mock MOV container").expect("write mock MOV source");
        let midpoint = LocalVideoMidpoint(9_250);
        let first_area = Rect::new(0, 0, 32, 9);
        let second_area = Rect::new(0, 0, 48, 14);
        let (mut first, replies, observed, _cancelled) =
            manager_with_mock_video_extractor(Some(cache_directory.clone()));

        assert!(first.synchronize_local_video(&movie, midpoint.0, first_area));
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("cold video extraction")
                .1,
            midpoint.0
        );
        replies
            .send(Ok(fixture_jpeg()))
            .expect("release cold extraction");
        assert_eq!(wait_for_terminal_state(&mut first), ThumbnailState::Ready);
        let frame_key = local_video_frame_cache_key(&movie, midpoint);
        let first_preview_key = local_video_preview_cache_key(&movie, midpoint, first_area);
        wait_for_local_video_frame(&cache_directory, &frame_key);
        wait_for_local_preview(&cache_directory, &first_preview_key);
        drop(first);

        let (mut restarted, _replies, restarted_observed, _cancelled) =
            manager_with_mock_video_extractor(Some(cache_directory.clone()));
        assert!(restarted.synchronize_local_video(&movie, midpoint.0, second_area));
        assert_eq!(
            wait_for_terminal_state(&mut restarted),
            ThumbnailState::Ready
        );
        assert!(
            matches!(
                restarted_observed.recv_timeout(Duration::from_millis(100)),
                Err(crossbeam_channel::RecvTimeoutError::Timeout)
            ),
            "a restart with a new area must derive from the cached source frame"
        );
        let second_preview_key = local_video_preview_cache_key(&movie, midpoint, second_area);
        assert_ne!(first_preview_key, second_preview_key);
        wait_for_local_preview(&cache_directory, &second_preview_key);
    }

    #[test]
    fn replacing_a_local_video_invalidates_its_source_frame_cache() {
        let directory = tempfile::tempdir().expect("video replacement directory");
        let movie = directory.path().join("replace.mov");
        let cache_directory = directory.path().join("thumbnail-cache");
        let area = Rect::new(0, 0, 32, 9);
        let midpoint = LocalVideoMidpoint(3_000);
        fs::write(&movie, b"first mock MOV container").expect("write first MOV identity");
        let (mut first, replies, observed, _cancelled) =
            manager_with_mock_video_extractor(Some(cache_directory.clone()));
        assert!(first.synchronize_local_video(&movie, midpoint.0, area));
        observed
            .recv_timeout(Duration::from_secs(1))
            .expect("first extraction");
        replies
            .send(Ok(fixture_jpeg()))
            .expect("release first extraction");
        assert_eq!(wait_for_terminal_state(&mut first), ThumbnailState::Ready);
        let first_frame_key = local_video_frame_cache_key(&movie, midpoint);
        wait_for_local_video_frame(&cache_directory, &first_frame_key);
        drop(first);

        fs::write(
            &movie,
            b"replacement mock MOV container with a distinct length",
        )
        .expect("replace MOV identity");
        let replacement_frame_key = local_video_frame_cache_key(&movie, midpoint);
        assert_ne!(first_frame_key, replacement_frame_key);
        let (mut replacement, replies, observed, _cancelled) =
            manager_with_mock_video_extractor(Some(cache_directory.clone()));
        assert!(replacement.synchronize_local_video(&movie, midpoint.0, area));
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("replacement extraction")
                .1,
            midpoint.0
        );
        replies
            .send(Ok(fixture_jpeg()))
            .expect("release replacement extraction");
        assert_eq!(
            wait_for_terminal_state(&mut replacement),
            ThumbnailState::Ready
        );
        wait_for_local_video_frame(&cache_directory, &replacement_frame_key);
    }

    #[test]
    fn selecting_another_local_video_cancels_the_inflight_extractor() {
        let directory = tempfile::tempdir().expect("video cancellation directory");
        let first = directory.path().join("first.mov");
        let second = directory.path().join("second.mov");
        fs::write(&first, b"first mock MOV").expect("write first MOV");
        fs::write(&second, b"second mock MOV").expect("write second MOV");
        let (mut manager, replies, observed, cancelled) = manager_with_mock_video_extractor(None);
        let area = Rect::new(0, 0, 40, 12);

        assert!(manager.synchronize_local_video(&first, 1_000, area));
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("observe first extraction")
                .0,
            fs::canonicalize(&first).expect("canonical first MOV")
        );
        assert!(manager.synchronize_local_video(&second, 2_000, area));
        assert_eq!(
            cancelled
                .recv_timeout(Duration::from_secs(1))
                .expect("first extraction cancellation"),
            fs::canonicalize(&first).expect("canonical first MOV")
        );
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("observe replacement extraction"),
            (
                fs::canonicalize(&second).expect("canonical second MOV"),
                2_000
            )
        );
        assert!(!manager.poll(), "the cancelled result must remain stale");
        assert_eq!(manager.state(), &ThumbnailState::Loading);
        replies
            .send(Ok(fixture_jpeg()))
            .expect("release replacement extraction");

        assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);
        assert_eq!(
            manager.target.as_ref().map(|target| &target.source),
            Some(&Url::from_file_path(&second).expect("second MOV URL"))
        );
    }

    #[test]
    fn oversized_local_png_and_webp_keep_the_generic_decode_limits() {
        let directory = tempfile::tempdir().expect("oversized local raster directory");
        let target = LocalPreviewTarget {
            width: 100,
            height: 100,
        };
        for (name, format) in [
            ("oversized.png", ImageFormat::Png),
            ("oversized.webp", ImageFormat::WebP),
        ] {
            let path = directory.path().join(name);
            let mut bytes = Cursor::new(Vec::new());
            DynamicImage::new_rgba8(MAX_IMAGE_DIMENSION + 1, 1)
                .write_to(&mut bytes, format)
                .expect("encode oversized local raster");
            fs::write(&path, bytes.into_inner()).expect("write oversized local raster");
            assert_eq!(
                decode_local_thumbnail_path(&path, target)
                    .expect_err("PNG and WebP must retain the generic dimension limit"),
                ThumbnailFailure::InvalidImage
            );
        }
    }

    /// Compares source decode with the persisted derivative on a developer
    /// supplied large image without imposing a wall-clock threshold on CI.
    #[test]
    #[ignore = "set YOUTA_LARGE_LOCAL_IMAGE to a large JPEG and run explicitly"]
    fn local_preview_cache_relative_benchmark() {
        let path = PathBuf::from(
            std::env::var_os("YOUTA_LARGE_LOCAL_IMAGE")
                .expect("set YOUTA_LARGE_LOCAL_IMAGE to a local JPEG path"),
        );
        let directory = tempfile::tempdir().expect("benchmark cache directory");
        let cache = ThumbnailCache::new(directory.path().join("thumbnail-cache"));
        let picker = picker_for_protocol(ThumbnailProtocol::Kitty, FALLBACK_FONT_SIZE);
        let target = ThumbnailTarget {
            source: Url::from_file_path(&path).expect("benchmark file URL"),
            local_video_midpoint: None,
            area: Rect::new(0, 0, 120, 40),
        };

        let cold_started = Instant::now();
        let cold = load_local_thumbnail(
            Some(&mut ThumbnailCache::new(cache.directory.clone())),
            &picker,
            &target,
        )
        .expect("cold local preview");
        let cold_elapsed = cold_started.elapsed();
        let deferred = cold
            .deferred_local_preview
            .expect("benchmark preview fits the persistent cache");
        persist_local_preview(&cache, &deferred);
        drop(cold.protocol);

        let warm_started = Instant::now();
        let warm = load_local_thumbnail(
            Some(&mut ThumbnailCache::new(cache.directory.clone())),
            &picker,
            &target,
        )
        .expect("warm local preview");
        let warm_elapsed = warm_started.elapsed();
        assert!(warm.deferred_local_preview.is_none());
        eprintln!("cold={cold_elapsed:?}, warm={warm_elapsed:?}");
        assert!(
            warm_elapsed < cold_elapsed,
            "the persisted derivative should avoid the source JPEG decode"
        );
    }

    /// Exercises the production HTTP, decoder, Kitty encoder, and worker path.
    #[test]
    #[ignore = "requires public YouTube thumbnail network access"]
    fn live_youtube_thumbnail_leaves_loading_with_a_ready_protocol() {
        assert_eq!(
            std::env::var("YOUTA_RUN_LIVE_THUMBNAIL_TEST").as_deref(),
            Ok("1"),
            "set YOUTA_RUN_LIVE_THUMBNAIL_TEST=1 when invoking this live test"
        );
        let mut manager =
            ThumbnailManager::from_terminal_info(ThumbnailMode::Auto, &graphical_terminal());
        let source = Url::parse("https://i.ytimg.com/vi/y19sUqgqZoI/mqdefault.jpg")
            .expect("stable YouTube thumbnail URL");

        assert!(manager.synchronize(Some(&source), Rect::new(40, 4, 40, 10)));
        assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);
        assert!(manager.protocol().is_some());
    }

    #[test]
    fn worker_encodes_the_exact_fitted_area_used_for_ready_rendering() {
        let picker = picker_for_protocol(ThumbnailProtocol::Halfblocks, FALLBACK_FONT_SIZE);
        let requested_area = Rect::new(4, 5, 60, 24);
        let mut encoded =
            encode_thumbnail(&picker, requested_area, DynamicImage::new_rgb8(1_600, 900))
                .expect("encode wide artwork fixture");

        assert_eq!(encoded.render_size, Size::new(60, 17));
        assert_eq!(
            encoded
                .protocol
                .needs_resize(&Resize::Fit(None), encoded.render_size),
            None,
            "the worker result must already match the StatefulImage render area"
        );

        let render_area = Rect::new(
            requested_area.x,
            requested_area.y,
            encoded.render_size.width,
            encoded.render_size.height,
        );
        let mut buffer = ratatui::buffer::Buffer::empty(requested_area);
        encoded
            .protocol
            .resize_encode_render(&Resize::Fit(None), render_area, &mut buffer);
        assert!(
            encoded.protocol.last_encoding_result().is_none(),
            "rendering the worker-prepared area must not synchronously resize and encode"
        );
    }

    #[test]
    fn youtube_standard_letterbox_does_not_reserve_blank_terminal_rows() {
        let (mut manager, replies, observed) = manager_with_mock_transport();
        let source =
            Url::parse("https://i.ytimg.com/vi/fixture/sddefault.jpg").expect("thumbnail URL");
        let requested_area = Rect::new(4, 5, 64, 24);

        assert!(manager.synchronize(Some(&source), requested_area));
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("visible request"),
            source
        );
        replies
            .send(Ok(youtube_letterbox_fixture_png()))
            .expect("letterboxed response");
        assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);

        assert_eq!(
            manager.render_size(),
            Some(Size::new(64, 18)),
            "embedded YouTube letterbox bands must not become empty rows around Details artwork"
        );
    }

    #[test]
    fn genuine_four_by_three_youtube_standard_thumbnail_is_not_cropped() {
        let (mut manager, replies, observed) = manager_with_mock_transport();
        let source =
            Url::parse("https://i.ytimg.com/vi/fixture/sddefault.jpg").expect("thumbnail URL");
        let requested_area = Rect::new(4, 5, 64, 24);
        let image = RgbImage::from_pixel(640, 480, image::Rgb([80, 120, 160]));

        assert!(manager.synchronize(Some(&source), requested_area));
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("visible request"),
            source
        );
        replies
            .send(Ok(encode_rgb_png(image)))
            .expect("four-by-three response");
        assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);
        assert_eq!(
            manager.render_size(),
            Some(Size::new(64, 24)),
            "non-dark 4:3 artwork must retain its original composition"
        );
    }

    #[test]
    fn non_youtube_letterbox_is_not_cropped() {
        let (mut manager, replies, observed) = manager_with_mock_transport();
        let source =
            Url::parse("https://images.example/fixture/sddefault.jpg").expect("thumbnail URL");
        let requested_area = Rect::new(4, 5, 64, 24);

        assert!(manager.synchronize(Some(&source), requested_area));
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("visible request"),
            source
        );
        replies
            .send(Ok(youtube_letterbox_fixture_png()))
            .expect("letterboxed response");
        assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);
        assert_eq!(
            manager.render_size(),
            Some(Size::new(64, 24)),
            "generic remote images must not receive YouTube-specific normalization"
        );
    }

    #[test]
    fn persistent_youtube_letterbox_cache_hit_is_cropped() {
        let directory = tempfile::tempdir().expect("temporary config directory");
        let cache_directory = directory.path().join("thumbnail-cache");
        let source =
            Url::parse("https://i.ytimg.com/vi/fixture/sddefault.jpg").expect("thumbnail URL");
        ThumbnailCache::new(cache_directory.clone())
            .store(&source, &youtube_letterbox_fixture_png())
            .expect("prime persistent thumbnail cache");
        let (mut manager, _replies, observed) =
            manager_with_mock_transport_in_cache(Some(cache_directory));

        assert!(manager.synchronize(Some(&source), Rect::new(4, 5, 64, 24)));
        assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);
        assert_eq!(
            manager.render_size(),
            Some(Size::new(64, 18)),
            "a warm persistent-cache hit must use the same crop as a fresh response"
        );
        assert!(
            matches!(
                observed.recv_timeout(Duration::from_millis(50)),
                Err(crossbeam_channel::RecvTimeoutError::Timeout)
            ),
            "a warm persistent-cache hit must not reach the network transport"
        );
    }

    #[test]
    fn manager_keeps_requested_area_as_identity_and_exposes_worker_render_size() {
        let (mut manager, replies, observed) = manager_with_mock_transport();
        let source = Url::parse("https://images.example/fitted-worker.png").expect("thumbnail URL");
        let requested_area = Rect::new(7, 9, 60, 24);

        assert!(manager.synchronize(Some(&source), requested_area));
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("visible request"),
            source
        );
        replies
            .send(Ok(fixture_thumbnail_png()))
            .expect("landscape fixture response");
        assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);

        assert_eq!(
            manager.target.as_ref().map(|target| target.area),
            Some(requested_area),
            "prepared-cache identity must retain the caller's requested area"
        );
        assert_eq!(
            manager.render_size(),
            Some(Size::new(32, 9)),
            "the small source remains natural-size while fitting inside the target"
        );
    }

    #[test]
    fn stale_worker_results_cannot_replace_the_current_selection() {
        let (request_sender, request_receiver) = bounded(1);
        let request_discarder = request_receiver.clone();
        let (result_sender, result_receiver) = bounded(1);
        let mut manager = ThumbnailManager {
            capability: ThumbnailCapability::Supported(ThumbnailProtocol::Kitty),
            state: ThumbnailState::Idle,
            generation: 0,
            current_generation: Arc::new(AtomicU64::new(0)),
            target: None,
            protocol: None,
            protocol_render_size: None,
            protocol_decoded_bytes: 0,
            protocol_key: None,
            prepared: VecDeque::new(),
            prepared_decoded_bytes: 0,
            picker: None,
            cache_directory: None,
            video_frame_program: PathBuf::from("ffmpeg"),
            request_sender: Some(request_sender),
            request_discarder: Some(request_discarder),
            prefetch_sender: None,
            prefetch_discarder: None,
            prefetch_sources: Vec::new(),
            result_receiver: Some(result_receiver),
        };
        let first = Url::parse("https://images.example/first.jpg").expect("first URL");
        let second = Url::parse("https://images.example/second.jpg").expect("second URL");
        assert!(manager.synchronize(Some(&first), Rect::new(1, 1, 20, 8)));
        let stale_generation = manager.generation;
        assert!(manager.synchronize(Some(&second), Rect::new(1, 1, 20, 8)));
        let current_generation = manager.generation;

        result_sender
            .send(WorkerResult {
                generation: stale_generation,
                result: Err(ThumbnailFailure::DownloadFailed),
            })
            .expect("stale result");
        assert!(!manager.poll());
        assert_eq!(manager.state(), &ThumbnailState::Loading);

        result_sender
            .send(WorkerResult {
                generation: current_generation,
                result: Err(ThumbnailFailure::InvalidImage),
            })
            .expect("current result");
        assert!(manager.poll());
        assert_eq!(
            manager.state(),
            &ThumbnailState::Failed(ThumbnailFailure::InvalidImage)
        );
    }

    #[test]
    fn mock_worker_resolves_loading_for_success_fetch_failure_and_decode_failure() {
        let (mut manager, replies, observed) = manager_with_mock_transport();
        let area = Rect::new(1, 1, 20, 8);

        let success = Url::parse("https://images.example/success.png").expect("success URL");
        assert!(manager.synchronize(Some(&success), area));
        assert_eq!(manager.state(), &ThumbnailState::Loading);
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("success request"),
            success
        );
        replies
            .send(Ok(fixture_png()))
            .expect("successful mock response");
        assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);
        assert!(manager.protocol().is_some());

        let fetch_failure =
            Url::parse("https://images.example/fetch-failure.png").expect("fetch failure URL");
        assert!(manager.synchronize(Some(&fetch_failure), area));
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("fetch failure request"),
            fetch_failure
        );
        replies
            .send(Err(ThumbnailFailure::DownloadFailed))
            .expect("failed mock response");
        assert_eq!(
            wait_for_terminal_state(&mut manager),
            ThumbnailState::Failed(ThumbnailFailure::DownloadFailed)
        );
        assert!(manager.protocol().is_none());

        let decode_failure =
            Url::parse("https://images.example/decode-failure.png").expect("decode failure URL");
        assert!(manager.synchronize(Some(&decode_failure), area));
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("decode failure request"),
            decode_failure
        );
        let mut malformed_png = fixture_png();
        malformed_png.truncate(24);
        replies.send(Ok(malformed_png)).expect("invalid mock image");
        assert_eq!(
            wait_for_terminal_state(&mut manager),
            ThumbnailState::Failed(ThumbnailFailure::InvalidImage)
        );
        assert!(manager.protocol().is_none());
    }

    #[test]
    fn visible_thumbnail_precedes_sanitized_silent_prefetch_backlog() {
        let directory = tempfile::tempdir().expect("temporary config directory");
        let cache_directory = directory.path().join("thumbnail-cache");
        let (mut manager, replies, observed) =
            manager_with_mock_transport_in_cache(Some(cache_directory.clone()));
        let selected = Url::parse("https://images.example/selected.png").expect("selected URL");
        let first = Url::parse("https://images.example/first.png").expect("first URL");
        let second = Url::parse("https://images.example/second.png").expect("second URL");
        let unsafe_source = Url::parse("file:///tmp/not-remote.png").expect("unsafe URL");
        let oversized = Url::parse(&format!(
            "https://images.example/{}.png",
            "x".repeat(MAX_PREFETCH_URL_BYTES)
        ))
        .expect("oversized URL");

        assert!(manager.synchronize(Some(&selected), Rect::new(1, 1, 20, 8)));
        assert!(manager.synchronize_prefetch(&[
            selected.clone(),
            first.clone(),
            first.clone(),
            unsafe_source,
            oversized,
            second.clone(),
        ]));
        assert_eq!(
            manager.prefetch_sources,
            [selected.clone(), first.clone(), second.clone()],
            "normalized sources must retain the active item while omitting unsafe, oversized, and duplicate URLs"
        );
        assert!(
            !manager.synchronize_prefetch(&[
                selected.clone(),
                first.clone(),
                first.clone(),
                second.clone(),
            ]),
            "an equivalent accepted backlog must not be sent again"
        );

        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("visible request"),
            selected
        );
        replies
            .send(Ok(fixture_png()))
            .expect("release visible request");
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("first prefetch request"),
            first
        );
        replies
            .send(Ok(fixture_png()))
            .expect("release first prefetch");
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("second prefetch request"),
            second.clone()
        );
        replies
            .send(Ok(fixture_png()))
            .expect("release second prefetch");

        assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);
        wait_for_cached_source(&cache_directory, &second);
        assert!(
            !manager.poll(),
            "background completion must not emit a redraw result"
        );
        assert_eq!(manager.state(), &ThumbnailState::Ready);
    }

    #[test]
    fn newest_prefetch_update_replaces_only_work_that_has_not_started() {
        let directory = tempfile::tempdir().expect("temporary config directory");
        let cache_directory = directory.path().join("thumbnail-cache");
        let (mut manager, replies, observed) =
            manager_with_mock_transport_in_cache(Some(cache_directory.clone()));
        let active = Url::parse("https://images.example/active.png").expect("active URL");
        let stale = Url::parse("https://images.example/stale.png").expect("stale URL");
        let replacement =
            Url::parse("https://images.example/replacement.png").expect("replacement URL");
        let selected =
            Url::parse("https://images.example/selected-next.png").expect("selected URL");

        assert!(manager.synchronize_prefetch(&[active.clone(), stale.clone()]));
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("active prefetch"),
            active
        );
        assert!(manager.synchronize_prefetch(std::slice::from_ref(&replacement)));
        assert!(manager.synchronize(Some(&selected), Rect::new(1, 1, 20, 8)));
        replies
            .send(Ok(fixture_png()))
            .expect("release active prefetch");
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("visible selection between prefetches"),
            selected
        );
        replies
            .send(Ok(fixture_png()))
            .expect("release visible selection");
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("replacement prefetch after visible selection"),
            replacement.clone()
        );
        replies
            .send(Ok(fixture_png()))
            .expect("release replacement prefetch");
        wait_for_cached_source(&cache_directory, &replacement);

        assert!(
            matches!(
                observed.recv_timeout(Duration::from_millis(50)),
                Err(crossbeam_channel::RecvTimeoutError::Timeout)
            ),
            "the stale queued source must not be fetched"
        );
        assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);
        assert!(!manager.poll());
    }

    #[test]
    fn prefetch_failures_are_silent_and_do_not_stop_later_sources() {
        let directory = tempfile::tempdir().expect("temporary config directory");
        let cache_directory = directory.path().join("thumbnail-cache");
        let (mut manager, replies, observed) =
            manager_with_mock_transport_in_cache(Some(cache_directory.clone()));
        let failed = Url::parse("https://images.example/failed.png").expect("failed URL");
        let successful =
            Url::parse("https://images.example/successful.png").expect("successful URL");

        assert!(manager.synchronize_prefetch(&[failed.clone(), successful.clone()]));
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("failed prefetch"),
            failed
        );
        replies
            .send(Err(ThumbnailFailure::DownloadFailed))
            .expect("release failed prefetch");
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("successful prefetch"),
            successful.clone()
        );
        replies
            .send(Ok(fixture_png()))
            .expect("release successful prefetch");
        wait_for_cached_source(&cache_directory, &successful);

        assert!(!manager.poll());
        assert_eq!(manager.state(), &ThumbnailState::Idle);
        assert!(manager.protocol().is_none());
    }

    #[test]
    fn prefetched_bytes_are_reused_by_a_restarted_visible_manager() {
        let directory = tempfile::tempdir().expect("temporary config directory");
        let cache_directory = directory.path().join("thumbnail-cache");
        let source = Url::parse("https://images.example/prefetched.png").expect("fixture URL");

        {
            let (mut manager, replies, observed) =
                manager_with_mock_transport_in_cache(Some(cache_directory.clone()));
            assert!(manager.synchronize_prefetch(std::slice::from_ref(&source)));
            assert_eq!(
                observed
                    .recv_timeout(Duration::from_secs(1))
                    .expect("prefetch request"),
                source
            );
            replies
                .send(Ok(fixture_png()))
                .expect("release prefetch request");
            wait_for_cached_source(&cache_directory, &source);
            assert!(!manager.poll());
        }

        let (mut restarted, _replies, observed) =
            manager_with_mock_transport_in_cache(Some(cache_directory));
        assert!(restarted.synchronize(Some(&source), Rect::new(1, 1, 20, 8)));
        assert_eq!(
            wait_for_terminal_state(&mut restarted),
            ThumbnailState::Ready
        );
        assert!(
            matches!(
                observed.recv_timeout(Duration::from_millis(50)),
                Err(crossbeam_channel::RecvTimeoutError::Timeout)
            ),
            "a visible restart cache hit must not repeat the prefetched transfer"
        );
    }

    #[test]
    fn prefetch_requires_supported_artwork_and_a_persistent_cache() {
        let source = Url::parse("https://images.example/unused.png").expect("fixture URL");
        let mut no_cache =
            ThumbnailManager::from_terminal_info(ThumbnailMode::Auto, &graphical_terminal());
        assert!(!no_cache.synchronize_prefetch(std::slice::from_ref(&source)));
        assert!(no_cache.request_sender.is_none());

        let directory = tempfile::tempdir().expect("temporary config directory");
        let cache_directory = directory.path().join("thumbnail-cache");
        let mut disabled = ThumbnailManager::from_terminal_info_with_cache(
            ThumbnailMode::Off,
            &graphical_terminal(),
            Some(cache_directory.clone()),
            true,
        );
        assert!(!disabled.synchronize_prefetch(std::slice::from_ref(&source)));
        assert!(!cache_directory.exists());
    }

    #[test]
    fn prefetch_backlog_has_a_fixed_source_limit() {
        assert!(
            ThumbnailCachePolicy::default().max_entries >= MAX_PREFETCH_SOURCES,
            "entry-count eviction must not defeat one complete bounded prefetch"
        );
        let directory = tempfile::tempdir().expect("temporary config directory");
        let (mut manager, _replies, _observed) =
            manager_with_mock_transport_in_cache(Some(directory.path().join("thumbnail-cache")));
        let sources = (0..MAX_PREFETCH_SOURCES + 20)
            .map(|index| {
                Url::parse(&format!("https://images.example/{index}.png")).expect("fixture URL")
            })
            .collect::<Vec<_>>();

        assert!(manager.synchronize_prefetch(&sources));
        assert_eq!(manager.prefetch_sources.len(), MAX_PREFETCH_SOURCES);
        assert_eq!(manager.prefetch_sources.first(), sources.first());
        assert_eq!(
            manager.prefetch_sources.last(),
            sources.get(MAX_PREFETCH_SOURCES - 1)
        );
    }

    #[test]
    fn persistent_cache_reuses_valid_bytes_across_manager_instances() {
        let directory = tempfile::tempdir().expect("temporary config directory");
        let cache_directory = directory.path().join("thumbnail-cache");
        let source = Url::parse("https://images.example/restart.png").expect("fixture URL");
        let area = Rect::new(1, 1, 20, 8);

        {
            let (mut manager, replies, observed) =
                manager_with_mock_transport_in_cache(Some(cache_directory.clone()));
            assert!(manager.synchronize(Some(&source), area));
            assert_eq!(
                observed
                    .recv_timeout(Duration::from_secs(1))
                    .expect("first manager must fetch"),
                source
            );
            replies
                .send(Ok(fixture_png()))
                .expect("release first manager");
            assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);
        }

        let (mut restarted, _replies, observed) =
            manager_with_mock_transport_in_cache(Some(cache_directory));
        assert!(restarted.synchronize(Some(&source), area));
        assert_eq!(
            wait_for_terminal_state(&mut restarted),
            ThumbnailState::Ready
        );
        assert!(
            matches!(
                observed.recv_timeout(Duration::from_millis(50)),
                Err(crossbeam_channel::RecvTimeoutError::Timeout)
            ),
            "a valid restart cache hit must not reach the network transport"
        );
    }

    #[test]
    fn persistent_cache_hit_bypasses_the_network_debounce() {
        let directory = tempfile::tempdir().expect("temporary config directory");
        let cache_directory = directory.path().join("thumbnail-cache");
        let source = Url::parse("https://images.example/fast-restart.png").expect("fixture URL");
        let cache = ThumbnailCache::new(cache_directory.clone());
        cache
            .store(&source, &fixture_png())
            .expect("prime persistent thumbnail cache");
        let (mut manager, _replies, observed) =
            manager_with_mock_transport_and_cache(Some(cache_directory), Duration::from_secs(2));

        let started = Instant::now();
        assert!(manager.synchronize(Some(&source), Rect::new(1, 1, 20, 8)));
        assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a cache hit must not wait for the two-second network debounce"
        );
        assert!(
            matches!(
                observed.recv_timeout(Duration::from_millis(50)),
                Err(crossbeam_channel::RecvTimeoutError::Timeout)
            ),
            "a cache hit must not reach the network transport"
        );
    }

    #[test]
    fn persisted_subscription_artwork_switches_without_network_transport() {
        let directory = tempfile::tempdir().expect("temporary config directory");
        let cache_directory = directory.path().join("thumbnail-cache");
        let first =
            Url::parse("https://yt3.ggpht.com/first-channel=s800").expect("first artwork URL");
        let second =
            Url::parse("https://yt3.ggpht.com/second-channel=s800").expect("second artwork URL");
        let cache = ThumbnailCache::new(cache_directory.clone());
        for source in [&first, &second] {
            cache
                .store(source, &fixture_png())
                .expect("prime persisted channel artwork");
        }
        let (mut manager, _replies, observed) =
            manager_with_mock_transport_and_cache(Some(cache_directory), Duration::from_secs(2));
        assert!(manager.synchronize_prefetch(&[first.clone(), second.clone()]));
        let area = Rect::new(1, 1, 20, 8);

        assert!(manager.synchronize(Some(&first), area));
        assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);
        assert!(manager.synchronize(Some(&second), area));
        assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);

        assert!(
            matches!(observed.try_recv(), Err(TryRecvError::Empty)),
            "switching between warmed subscription sources must not reach network transport"
        );
    }

    #[test]
    fn revisiting_subscription_artwork_reuses_the_encoded_protocol_immediately() {
        let (mut manager, replies, observed) = manager_with_mock_transport();
        let first =
            Url::parse("https://yt3.ggpht.com/first-encoded=s800").expect("first artwork URL");
        let second =
            Url::parse("https://yt3.ggpht.com/second-encoded=s800").expect("second artwork URL");
        let area = Rect::new(1, 1, 40, 16);

        for source in [&first, &second] {
            assert!(manager.synchronize(Some(source), area));
            assert_eq!(
                observed
                    .recv_timeout(Duration::from_secs(1))
                    .expect("cold artwork request"),
                *source
            );
            replies
                .send(Ok(fixture_png()))
                .expect("release cold artwork request");
            assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);
        }
        let expected_render_size = manager.render_size().expect("prepared render size");

        assert!(manager.synchronize(Some(&first), area));
        assert_eq!(
            manager.state(),
            &ThumbnailState::Ready,
            "a revisited channel must not expose a loading frame"
        );
        assert!(manager.protocol().is_some());
        assert_eq!(
            manager.render_size(),
            Some(expected_render_size),
            "the prepared cache must restore the worker's exact encoded size"
        );
        assert!(
            matches!(observed.try_recv(), Err(TryRecvError::Empty)),
            "encoded in-memory artwork must bypass the worker and transport"
        );
    }

    #[test]
    fn prepared_thumbnail_cache_is_bounded_and_evicts_least_recently_used() {
        let (mut manager, replies, observed) = manager_with_mock_transport();
        let area = Rect::new(1, 1, 20, 8);
        let sources = (0..PREPARED_THUMBNAIL_CACHE_ENTRIES + 2)
            .map(|index| {
                Url::parse(&format!("https://yt3.ggpht.com/channel-{index}=s800"))
                    .expect("artwork URL")
            })
            .collect::<Vec<_>>();

        for source in &sources {
            assert!(manager.synchronize(Some(source), area));
            assert_eq!(
                observed
                    .recv_timeout(Duration::from_secs(1))
                    .expect("cold artwork request"),
                *source
            );
            replies
                .send(Ok(fixture_png()))
                .expect("release cold artwork request");
            assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);
        }
        assert_eq!(manager.prepared.len(), PREPARED_THUMBNAIL_CACHE_ENTRIES);

        assert!(manager.synchronize(Some(&sources[0]), area));
        assert_eq!(
            manager.state(),
            &ThumbnailState::Loading,
            "the least-recently-used protocol must be evicted at the fixed bound"
        );
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("evicted artwork request"),
            sources[0]
        );
        replies
            .send(Ok(fixture_png()))
            .expect("release evicted artwork request");
        assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);
    }

    #[test]
    fn prepared_thumbnail_cache_evicts_by_decoded_bytes_and_rejects_one_oversized_entry() {
        let mut manager =
            ThumbnailManager::from_terminal_info(ThumbnailMode::Auto, &graphical_terminal());
        let area = Rect::new(0, 0, 1, 1);
        let protocol = || {
            encode_thumbnail(
                &picker_for_protocol(ThumbnailProtocol::Kitty, FALLBACK_FONT_SIZE),
                area,
                DynamicImage::new_rgba8(1, 1),
            )
            .expect("encode cache-policy fixture")
            .protocol
        };
        let first = PreparedThumbnailKey {
            source: Url::parse("https://images.example/first-budget.png").expect("first URL"),
            local_video_midpoint: None,
            width: area.width,
            height: area.height,
            local_fingerprint: None,
        };
        let second = PreparedThumbnailKey {
            source: Url::parse("https://images.example/second-budget.png").expect("second URL"),
            local_video_midpoint: None,
            width: area.width,
            height: area.height,
            local_fingerprint: None,
        };
        let oversized = PreparedThumbnailKey {
            source: Url::parse("https://images.example/oversized-budget.png")
                .expect("oversized URL"),
            local_video_midpoint: None,
            width: area.width,
            height: area.height,
            local_fingerprint: None,
        };
        let more_than_half = PREPARED_THUMBNAIL_CACHE_MAX_DECODED_BYTES / 2 + 1;

        manager.cache_prepared_protocol(first.clone(), protocol(), area.into(), more_than_half);
        manager.cache_prepared_protocol(second.clone(), protocol(), area.into(), more_than_half);

        assert_eq!(manager.prepared.len(), 1);
        assert_eq!(
            manager.prepared.front().map(|entry| &entry.key),
            Some(&second)
        );
        assert_eq!(manager.prepared_decoded_bytes, more_than_half);

        manager.cache_prepared_protocol(
            oversized.clone(),
            protocol(),
            area.into(),
            PREPARED_THUMBNAIL_CACHE_MAX_DECODED_BYTES + 1,
        );

        assert_eq!(manager.prepared.len(), 1);
        assert!(manager.prepared.iter().all(|entry| entry.key != oversized));
        assert!(manager.prepared_decoded_bytes <= PREPARED_THUMBNAIL_CACHE_MAX_DECODED_BYTES);
    }

    #[test]
    fn revisiting_a_small_local_jpeg_reuses_protocol_and_replacement_invalidates_it() {
        let directory = tempfile::tempdir().expect("temporary local replacement fixture");
        let cache_directory = directory.path().join("thumbnail-cache");
        let first_path = directory.path().join("first.jpg");
        let second_path = directory.path().join("second.jpg");
        write_jpeg_fixture(&first_path, 80, 40);
        write_jpeg_fixture(&second_path, 64, 32);
        let first = Url::from_file_path(&first_path).expect("first local URL");
        let second = Url::from_file_path(&second_path).expect("second local URL");
        let area = Rect::new(1, 1, 40, 10);
        let mut manager = local_thumbnail_manager(cache_directory.clone());
        assert!(
            fs::metadata(&first_path)
                .expect("small JPEG metadata")
                .len()
                < 64 * 1024,
            "regression fixture must remain comparable to a small local cover"
        );

        assert!(manager.synchronize(Some(&first), area));
        assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);
        let old_cache_key = local_preview_cache_key(&first_path, area);
        wait_for_local_preview(&cache_directory, &old_cache_key);
        assert_eq!(local_source_decode_count(&first_path), 1);

        assert!(manager.synchronize(None, area));
        assert_eq!(manager.state(), &ThumbnailState::Idle);
        assert!(manager.synchronize(Some(&first), area));
        assert_eq!(
            manager.state(),
            &ThumbnailState::Ready,
            "returning from a file without artwork must reuse the local protocol"
        );
        assert_eq!(local_source_decode_count(&first_path), 1);

        assert!(manager.synchronize(Some(&second), area));
        assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);
        assert_eq!(local_source_decode_count(&second_path), 1);
        assert!(
            manager
                .prepared
                .iter()
                .any(|entry| entry.key.source == first),
            "leaving a local JPEG must retain its fingerprinted protocol"
        );

        assert!(manager.synchronize(Some(&first), area));
        assert_eq!(
            manager.state(),
            &ThumbnailState::Ready,
            "A→B→A navigation must reuse the prepared local protocol synchronously"
        );
        assert!(manager.protocol().is_some());
        assert_eq!(
            local_source_decode_count(&first_path),
            1,
            "an unchanged revisit must bypass source decode and terminal re-encoding"
        );

        assert!(manager.synchronize(Some(&second), area));
        assert_eq!(
            manager.state(),
            &ThumbnailState::Ready,
            "the second unchanged local image must also remain prepared"
        );
        write_jpeg_fixture(&first_path, 96, 48);
        let replacement_cache_key = local_preview_cache_key(&first_path, area);
        assert_ne!(replacement_cache_key, old_cache_key);

        assert!(manager.synchronize(Some(&first), area));
        assert_eq!(
            manager.state(),
            &ThumbnailState::Loading,
            "a replaced local image must be fingerprinted by the worker"
        );
        assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);
        assert_eq!(
            local_source_decode_count(&first_path),
            2,
            "the same-path replacement must be decoded instead of reusing stale pixels"
        );
    }

    #[test]
    fn blocking_prefetch_cannot_delay_a_visible_thumbnail_request() {
        let directory = tempfile::tempdir().expect("temporary config directory");
        let cache_directory = directory.path().join("thumbnail-cache");
        let (visible_request_sender, visible_request_receiver) = bounded(1);
        let visible_request_discarder = visible_request_receiver.clone();
        let (result_sender, result_receiver) = bounded(1);
        let (visible_observed_sender, visible_observed) = bounded(1);
        let (visible_reply_sender, visible_reply_receiver) = bounded(1);
        assert!(spawn_visible_worker_with_transport(
            picker_for_protocol(ThumbnailProtocol::Kitty, FALLBACK_FONT_SIZE),
            visible_request_receiver,
            result_sender,
            MockTransport {
                observed: visible_observed_sender,
                replies: visible_reply_receiver,
            },
            Some(ThumbnailCache::new(cache_directory.clone())),
            Duration::ZERO,
        ));

        let (prefetch_sender, prefetch_receiver) = bounded(1);
        let prefetch_discarder = prefetch_receiver.clone();
        let (prefetch_observed_sender, prefetch_observed) = bounded(1);
        let (prefetch_reply_sender, prefetch_reply_receiver) = bounded(1);
        assert!(spawn_prefetch_worker_with_transport(
            prefetch_receiver,
            MockTransport {
                observed: prefetch_observed_sender,
                replies: prefetch_reply_receiver,
            },
            ThumbnailCache::new(cache_directory.clone()),
        ));

        let mut manager = ThumbnailManager {
            capability: ThumbnailCapability::Supported(ThumbnailProtocol::Kitty),
            state: ThumbnailState::Idle,
            generation: 0,
            current_generation: Arc::new(AtomicU64::new(0)),
            target: None,
            protocol: None,
            protocol_render_size: None,
            protocol_decoded_bytes: 0,
            protocol_key: None,
            prepared: VecDeque::new(),
            prepared_decoded_bytes: 0,
            picker: None,
            cache_directory: Some(cache_directory),
            video_frame_program: PathBuf::from("ffmpeg"),
            request_sender: Some(visible_request_sender),
            request_discarder: Some(visible_request_discarder),
            prefetch_sender: Some(prefetch_sender),
            prefetch_discarder: Some(prefetch_discarder),
            prefetch_sources: Vec::new(),
            result_receiver: Some(result_receiver),
        };
        let background =
            Url::parse("https://images.example/blocked-prefetch.png").expect("prefetch URL");
        let selected =
            Url::parse("https://images.example/visible-selection.png").expect("visible URL");

        assert!(manager.synchronize_prefetch(std::slice::from_ref(&background)));
        assert_eq!(
            prefetch_observed
                .recv_timeout(Duration::from_secs(1))
                .expect("blocking prefetch must start"),
            background
        );
        assert!(manager.synchronize(Some(&selected), Rect::new(1, 1, 20, 8)));
        assert_eq!(
            visible_observed
                .recv_timeout(Duration::from_secs(1))
                .expect("visible transport must remain independent"),
            selected
        );
        visible_reply_sender
            .send(Ok(fixture_png()))
            .expect("release visible request");
        assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);

        prefetch_reply_sender
            .send(Ok(fixture_png()))
            .expect("release background prefetch");
    }

    #[test]
    fn corrupt_persistent_entry_is_removed_fetched_and_replaced() {
        let directory = tempfile::tempdir().expect("temporary config directory");
        let cache_directory = directory.path().join("thumbnail-cache");
        let cache = ThumbnailCache::new(cache_directory.clone());
        cache.prepare().expect("prepare thumbnail cache");
        let source = Url::parse("https://images.example/corrupt.png").expect("fixture URL");
        let cache_path = cache.entry_path(&source);
        fs::write(&cache_path, b"corrupt").expect("write corrupt cache fixture");

        let (mut manager, replies, observed) =
            manager_with_mock_transport_in_cache(Some(cache_directory));
        assert!(manager.synchronize(Some(&source), Rect::new(1, 1, 20, 8)));
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("corrupt entry must fall back to transport"),
            source
        );
        replies
            .send(Ok(fixture_png()))
            .expect("release replacement fetch");
        assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);

        let repaired = cache
            .read(&source)
            .expect("read repaired cache entry")
            .expect("replacement must be cached");
        assert!(decode_thumbnail(&repaired).is_ok());
        assert_ne!(repaired, b"corrupt");
    }

    #[test]
    fn cache_uses_confined_hashed_names_and_atomic_private_files() {
        let directory = tempfile::tempdir().expect("temporary config directory");
        let cache_directory = directory.path().join("thumbnail-cache");
        let cache = ThumbnailCache::new(cache_directory.clone());
        let source =
            Url::parse("https://images.example/../../art.png?private-token=not-a-filename")
                .expect("fixture URL");
        cache
            .store(&source, &fixture_png())
            .expect("persist thumbnail");

        let entry_path = cache.entry_path(&source);
        assert_eq!(entry_path.parent(), Some(cache_directory.as_path()));
        let file_name = entry_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("UTF-8 cache filename");
        assert!(is_cache_entry_name(
            entry_path.file_name().expect("cache filename")
        ));
        assert!(!file_name.contains("private-token"));
        assert!(!file_name.contains("images.example"));
        let files = fs::read_dir(&cache_directory)
            .expect("read cache directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("cache entries");
        assert_eq!(files.len(), 1, "atomic temporary file must be removed");
        assert_eq!(files[0].path(), entry_path);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(&cache_directory)
                    .expect("cache directory metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&entry_path)
                    .expect("cache entry metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn cache_prunes_only_recognized_regular_atomic_temporary_files() {
        let directory = tempfile::tempdir().expect("temporary config directory");
        let cache_directory = directory.path().join("thumbnail-cache");
        let cache = ThumbnailCache::new(cache_directory.clone());
        cache.prepare().expect("prepare thumbnail cache");
        let abandoned = cache_directory.join(".thumbnail.123.456.tmp");
        let unrelated = cache_directory.join("keep.tmp");
        fs::write(&abandoned, b"partial image").expect("write abandoned temporary file");
        fs::write(&unrelated, b"user file").expect("write unrelated file");

        #[cfg(unix)]
        let (linked, outside) = {
            use std::os::unix::fs::symlink;

            let outside = directory.path().join("outside");
            fs::write(&outside, b"outside").expect("write symlink target");
            let linked = cache_directory.join(".thumbnail.789.012.tmp");
            symlink(&outside, &linked).expect("create cache-shaped symlink");
            (linked, outside)
        };

        cache.prepare().expect("prune abandoned temporary file");
        assert!(!abandoned.exists());
        assert!(unrelated.exists());
        #[cfg(unix)]
        {
            assert!(
                fs::symlink_metadata(&linked)
                    .expect("cache-shaped symlink must remain untouched")
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(fs::read(outside).expect("read symlink target"), b"outside");
        }
    }

    #[test]
    fn cache_eviction_never_removes_a_concurrent_atomic_write() {
        let directory = tempfile::tempdir().expect("temporary config directory");
        let cache_directory = directory.path().join("thumbnail-cache");
        let cache = ThumbnailCache::new(cache_directory.clone());
        cache.prepare().expect("prepare thumbnail cache");
        let temporary =
            cache_directory.join(format!(".thumbnail.{}.987654.tmp", std::process::id()));
        fs::write(&temporary, b"in-flight image").expect("write active temporary");
        let registration = ActiveCacheTemporary::register(temporary.clone());

        cache.prepare().expect("evict beside active write");
        assert!(
            temporary.exists(),
            "another cache worker must not delete an active atomic write"
        );

        drop(registration);
        cache.prepare().expect("prune abandoned temporary");
        assert!(!temporary.exists());
    }

    #[test]
    fn cache_evicts_expired_excess_count_and_excess_bytes() {
        let directory = tempfile::tempdir().expect("temporary cache roots");
        let image = fixture_png();
        let roomy = ThumbnailCachePolicy {
            max_age: Duration::from_secs(60),
            max_bytes: u64::MAX,
            max_entries: usize::MAX,
        };

        let expired_root = directory.path().join("expired");
        let source = Url::parse("https://images.example/expired.png").expect("expired URL");
        ThumbnailCache::with_policy(expired_root.clone(), roomy)
            .store(&source, &image)
            .expect("write entry before expiry");
        let expiring = ThumbnailCache::with_policy(
            expired_root.clone(),
            ThumbnailCachePolicy {
                max_age: Duration::ZERO,
                ..roomy
            },
        );
        expiring.evict().expect("evict expired entry");
        assert_eq!(cache_entry_count(&expired_root), 0);

        let count_root = directory.path().join("count");
        let count_cache = ThumbnailCache::with_policy(
            count_root.clone(),
            ThumbnailCachePolicy {
                max_entries: 2,
                ..roomy
            },
        );
        store_numbered_fixtures(&count_cache, &image, 3);
        assert!(cache_entry_count(&count_root) <= 2);

        let bytes_root = directory.path().join("bytes");
        let byte_cache = ThumbnailCache::with_policy(
            bytes_root.clone(),
            ThumbnailCachePolicy {
                max_bytes: u64::try_from(image.len() * 2).expect("fixture cache limit"),
                max_entries: usize::MAX,
                ..roomy
            },
        );
        store_numbered_fixtures(&byte_cache, &image, 3);
        assert!(
            cache_total_bytes(&bytes_root)
                <= u64::try_from(image.len() * 2).expect("fixture cache limit")
        );
    }

    #[test]
    fn cancellation_and_stale_mock_results_cannot_leave_loading_or_replace_selection() {
        let (mut manager, replies, observed) = manager_with_mock_transport();
        let area = Rect::new(1, 1, 20, 8);
        let first = Url::parse("https://images.example/first.png").expect("first URL");
        let second = Url::parse("https://images.example/second.png").expect("second URL");

        assert!(manager.synchronize(Some(&first), area));
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("first request"),
            first
        );
        assert!(manager.synchronize(Some(&second), area));
        replies
            .send(Err(ThumbnailFailure::DownloadFailed))
            .expect("stale failure");
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("second request"),
            second
        );
        assert!(!manager.poll(), "stale failure must not change UI state");
        assert_eq!(manager.state(), &ThumbnailState::Loading);

        replies.send(Ok(fixture_png())).expect("current image");
        assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);

        let cancelled = Url::parse("https://images.example/cancelled.png").expect("cancelled URL");
        assert!(manager.synchronize(Some(&cancelled), area));
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("cancelled request"),
            cancelled
        );
        assert!(manager.clear());
        assert_eq!(manager.state(), &ThumbnailState::Idle);
        replies
            .send(Ok(fixture_png()))
            .expect("cancelled image result");
        wait_for_queued_result(&manager);
        assert!(!manager.poll(), "cancelled result must remain stale");
        assert_eq!(manager.state(), &ThumbnailState::Idle);
        assert!(manager.protocol().is_none());
    }

    #[test]
    fn disconnected_worker_changes_loading_to_url_free_failure() {
        let (request_sender, request_receiver) = bounded(1);
        let (result_sender, result_receiver) = bounded(1);
        drop(request_receiver);
        drop(result_sender);
        let source = Url::parse("https://secret.example/thumbnail.png").expect("fixture URL");
        let mut manager = ThumbnailManager {
            capability: ThumbnailCapability::Supported(ThumbnailProtocol::Kitty),
            state: ThumbnailState::Loading,
            generation: 1,
            current_generation: Arc::new(AtomicU64::new(1)),
            target: Some(ThumbnailTarget {
                source,
                local_video_midpoint: None,
                area: Rect::new(0, 0, 20, 8),
            }),
            protocol: None,
            protocol_render_size: None,
            protocol_decoded_bytes: 0,
            protocol_key: None,
            prepared: VecDeque::new(),
            prepared_decoded_bytes: 0,
            picker: None,
            cache_directory: None,
            video_frame_program: PathBuf::from("ffmpeg"),
            request_sender: Some(request_sender),
            request_discarder: None,
            prefetch_sender: None,
            prefetch_discarder: None,
            prefetch_sources: Vec::new(),
            result_receiver: Some(result_receiver),
        };

        assert!(manager.poll());
        assert_eq!(
            manager.state(),
            &ThumbnailState::Failed(ThumbnailFailure::WorkerStopped)
        );
        let ThumbnailState::Failed(failure) = manager.state() else {
            panic!("disconnected worker must expose a failure");
        };
        assert!(!failure.to_string().contains("secret.example"));
        assert!(manager.result_receiver.is_none());

        let mut missing_receiver =
            ThumbnailManager::inactive(ThumbnailCapability::Supported(ThumbnailProtocol::Kitty));
        missing_receiver.state = ThumbnailState::Loading;
        assert!(missing_receiver.poll());
        assert_eq!(
            missing_receiver.state(),
            &ThumbnailState::Failed(ThumbnailFailure::WorkerStopped)
        );
    }

    #[test]
    fn invalid_or_credential_bearing_sources_fail_without_reaching_transport() {
        let (mut manager, _replies, observed) = manager_with_mock_transport();
        let credentialed = Url::parse("https://api-key:secret@images.example/image.png")
            .expect("credentialed URL");

        assert!(manager.synchronize(Some(&credentialed), Rect::new(0, 0, 20, 8)));
        assert_eq!(
            manager.state(),
            &ThumbnailState::Failed(ThumbnailFailure::InvalidSource)
        );
        assert!(matches!(observed.try_recv(), Err(TryRecvError::Empty)));
        let ThumbnailState::Failed(failure) = manager.state() else {
            panic!("credential-bearing source must expose a failure");
        };
        assert!(!failure.to_string().contains("api-key"));
        assert!(!failure.to_string().contains("secret"));
    }

    #[test]
    fn user_facing_failures_never_include_source_urls() {
        let rendered = [
            ThumbnailFailure::InvalidSource,
            ThumbnailFailure::DownloadFailed,
            ThumbnailFailure::ResponseTooLarge,
            ThumbnailFailure::UnsupportedFormat,
            ThumbnailFailure::InvalidImage,
            ThumbnailFailure::LocalVideoFrameExtractionFailed,
            ThumbnailFailure::EncodingFailed,
            ThumbnailFailure::WorkerStopped,
        ]
        .map(|failure| failure.to_string())
        .join("\n");
        assert!(!rendered.contains("http"));
        assert!(!rendered.contains("images.example"));
    }

    pub(crate) fn manager_with_mock_transport() -> MockManagerParts {
        manager_with_mock_transport_in_cache(None)
    }

    /// Builds an idle half-block manager without consulting the host terminal.
    pub(crate) fn halfblock_manager_for_tui(cache_directory: Option<PathBuf>) -> ThumbnailManager {
        let terminal = TerminalInfo {
            term: Some("linux".to_owned()),
            kitty_window: false,
            output_device: Some(PathBuf::from("/dev/tty2")),
            font_size: None,
            ..graphical_terminal()
        };
        ThumbnailManager::from_terminal_info_with_cache(
            ThumbnailMode::Auto,
            &terminal,
            cache_directory,
            true,
        )
    }

    fn manager_with_mock_transport_in_cache(cache_directory: Option<PathBuf>) -> MockManagerParts {
        manager_with_mock_transport_and_cache(cache_directory, Duration::ZERO)
    }

    fn manager_with_mock_transport_and_cache(
        cache_directory: Option<PathBuf>,
        debounce: Duration,
    ) -> MockManagerParts {
        let (request_sender, request_receiver) = bounded(1);
        let request_discarder = request_receiver.clone();
        let (prefetch_sender, prefetch_receiver) = bounded(1);
        let prefetch_discarder = prefetch_receiver.clone();
        let (result_sender, result_receiver) = bounded(1);
        let (observed_sender, observed_receiver) = bounded(4);
        let (reply_sender, reply_receiver) = bounded(4);
        let picker = picker_for_protocol(ThumbnailProtocol::Kitty, FALLBACK_FONT_SIZE);
        assert!(spawn_worker_with_transport(
            picker,
            request_receiver,
            prefetch_receiver,
            result_sender,
            MockTransport {
                observed: observed_sender,
                replies: reply_receiver,
            },
            cache_directory.clone().map(ThumbnailCache::new),
            debounce,
        ));
        (
            ThumbnailManager {
                capability: ThumbnailCapability::Supported(ThumbnailProtocol::Kitty),
                state: ThumbnailState::Idle,
                generation: 0,
                current_generation: Arc::new(AtomicU64::new(0)),
                target: None,
                protocol: None,
                protocol_render_size: None,
                protocol_decoded_bytes: 0,
                protocol_key: None,
                prepared: VecDeque::new(),
                prepared_decoded_bytes: 0,
                picker: None,
                cache_directory,
                video_frame_program: PathBuf::from("ffmpeg"),
                request_sender: Some(request_sender),
                request_discarder: Some(request_discarder),
                prefetch_sender: Some(prefetch_sender),
                prefetch_discarder: Some(prefetch_discarder),
                prefetch_sources: Vec::new(),
                result_receiver: Some(result_receiver),
            },
            reply_sender,
            observed_receiver,
        )
    }

    type MockVideoManagerParts = (
        ThumbnailManager,
        Sender<Result<Vec<u8>, ThumbnailFailure>>,
        Receiver<(PathBuf, u64)>,
        Receiver<PathBuf>,
    );

    fn manager_with_mock_video_extractor(
        cache_directory: Option<PathBuf>,
    ) -> MockVideoManagerParts {
        let (request_sender, request_receiver) = bounded(1);
        let request_discarder = request_receiver.clone();
        let (result_sender, result_receiver) = bounded(1);
        let (observed_sender, observed_receiver) = bounded(4);
        let (reply_sender, reply_receiver) = bounded(4);
        let (cancelled_sender, cancelled_receiver) = bounded(4);
        let current_generation = Arc::new(AtomicU64::new(0));
        assert!(spawn_visible_worker_with_transport_and_extractor(
            picker_for_protocol(ThumbnailProtocol::Kitty, FALLBACK_FONT_SIZE),
            request_receiver,
            result_sender,
            RejectingTransport,
            MockVideoExtractor {
                observed: observed_sender,
                replies: reply_receiver,
                cancelled: cancelled_sender,
            },
            cache_directory.clone().map(ThumbnailCache::new),
            Duration::ZERO,
            Arc::clone(&current_generation),
        ));
        (
            ThumbnailManager {
                capability: ThumbnailCapability::Supported(ThumbnailProtocol::Kitty),
                state: ThumbnailState::Idle,
                generation: 0,
                current_generation,
                target: None,
                protocol: None,
                protocol_render_size: None,
                protocol_decoded_bytes: 0,
                protocol_key: None,
                prepared: VecDeque::new(),
                prepared_decoded_bytes: 0,
                picker: None,
                cache_directory,
                video_frame_program: PathBuf::from("ffmpeg"),
                request_sender: Some(request_sender),
                request_discarder: Some(request_discarder),
                prefetch_sender: None,
                prefetch_discarder: None,
                prefetch_sources: Vec::new(),
                result_receiver: Some(result_receiver),
            },
            reply_sender,
            observed_receiver,
            cancelled_receiver,
        )
    }

    fn store_numbered_fixtures(cache: &ThumbnailCache, bytes: &[u8], count: usize) {
        for number in 0..count {
            let source = Url::parse(&format!("https://images.example/{number}.png"))
                .expect("numbered fixture URL");
            cache
                .store(&source, bytes)
                .expect("store numbered cache fixture");
        }
    }

    fn cache_entry_count(directory: &Path) -> usize {
        fs::read_dir(directory)
            .expect("read cache directory")
            .filter_map(Result::ok)
            .filter(|entry| is_cache_entry_name(&entry.file_name()))
            .count()
    }

    fn cache_total_bytes(directory: &Path) -> u64 {
        fs::read_dir(directory)
            .expect("read cache directory")
            .filter_map(Result::ok)
            .filter(|entry| is_cache_entry_name(&entry.file_name()))
            .filter_map(|entry| entry.metadata().ok())
            .map(|metadata| metadata.len())
            .sum()
    }

    fn wait_for_cached_source(cache_directory: &Path, source: &Url) {
        let cache = ThumbnailCache::new(cache_directory.to_path_buf());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if cache
                .read(source)
                .expect("read prefetched cache entry")
                .is_some()
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "prefetch did not persist the expected source before its deadline"
            );
            thread::yield_now();
        }
    }

    fn local_thumbnail_manager(cache_directory: PathBuf) -> ThumbnailManager {
        ThumbnailManager::from_terminal_info_with_cache(
            ThumbnailMode::Auto,
            &graphical_terminal(),
            Some(cache_directory),
            true,
        )
    }

    fn local_preview_cache_key(path: &Path, area: Rect) -> [u8; 32] {
        let picker = picker_for_protocol(ThumbnailProtocol::Kitty, (9, 18));
        LocalThumbnailFingerprint::capture(path)
            .expect("capture local preview fixture")
            .preview_cache_key(local_preview_target(&picker, area))
    }

    fn local_video_frame_cache_key(path: &Path, midpoint: LocalVideoMidpoint) -> [u8; 32] {
        LocalThumbnailFingerprint::capture(path)
            .expect("capture local video fixture")
            .video_frame_cache_key(midpoint)
    }

    fn local_video_preview_cache_key(
        path: &Path,
        midpoint: LocalVideoMidpoint,
        area: Rect,
    ) -> [u8; 32] {
        let picker = picker_for_protocol(ThumbnailProtocol::Kitty, FALLBACK_FONT_SIZE);
        LocalThumbnailFingerprint::capture(path)
            .expect("capture local video fixture")
            .video_preview_cache_key(midpoint, local_preview_target(&picker, area))
    }

    fn wait_for_local_preview(cache_directory: &Path, cache_key: &[u8]) {
        let cache = ThumbnailCache::new(cache_directory.to_path_buf());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if cache
                .read_key(cache_key)
                .expect("read local preview cache")
                .is_some_and(|bytes| decode_local_preview_record(&bytes).is_some())
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "local preview was not persisted before its test deadline"
            );
            thread::yield_now();
        }
    }

    fn wait_for_local_video_frame(cache_directory: &Path, cache_key: &[u8]) {
        let cache = ThumbnailCache::new(cache_directory.to_path_buf());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if cache
                .read_key(cache_key)
                .expect("read local video frame cache")
                .is_some_and(|bytes| decode_thumbnail(&bytes).is_ok())
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "local video frame was not persisted before its test deadline"
            );
            thread::yield_now();
        }
    }

    fn render_and_wait_for_local_preview(path: &Path, cache_directory: &Path, area: Rect) {
        let source = Url::from_file_path(path).expect("local preview URL");
        let mut manager = local_thumbnail_manager(cache_directory.to_path_buf());
        assert!(manager.synchronize(Some(&source), area));
        assert_eq!(wait_for_terminal_state(&mut manager), ThumbnailState::Ready);
        let cache_key = local_preview_cache_key(path, area);
        wait_for_local_preview(cache_directory, &cache_key);
    }

    fn local_source_decode_count(path: &Path) -> usize {
        let path = fs::canonicalize(path).expect("canonical local thumbnail fixture");
        LOCAL_SOURCE_DECODE_COUNTS
            .lock()
            .expect("local thumbnail decode counts")
            .iter()
            .find_map(|(candidate, count)| (candidate == &path).then_some(*count))
            .unwrap_or(0)
    }

    fn write_jpeg_fixture(path: &Path, width: u32, height: u32) {
        let image = RgbImage::from_fn(width, height, |x, y| {
            let mut value = x
                .wrapping_mul(0x9E37_79B9)
                .wrapping_add(y.wrapping_mul(0x85EB_CA6B));
            value ^= value >> 16;
            value = value.wrapping_mul(0x7FEB_352D);
            value ^= value >> 15;
            image::Rgb([
                value as u8,
                value.rotate_left(11) as u8,
                value.rotate_left(23) as u8,
            ])
        });
        let mut jpeg = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut jpeg, ImageFormat::Jpeg)
            .expect("encode deterministic JPEG fixture");
        fs::write(path, jpeg.into_inner()).expect("write deterministic JPEG fixture");
    }

    fn fixture_jpeg() -> Vec<u8> {
        let image = RgbImage::from_fn(64, 36, |x, y| {
            image::Rgb([
                u8::try_from(x.saturating_mul(3)).unwrap_or(u8::MAX),
                u8::try_from(y.saturating_mul(5)).unwrap_or(u8::MAX),
                127,
            ])
        });
        let mut jpeg = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut jpeg, ImageFormat::Jpeg)
            .expect("encode deterministic video-frame JPEG");
        jpeg.into_inner()
    }

    /// Encodes one RGB fixture with a lossless format so darkness thresholds
    /// remain deterministic across thumbnail-normalization tests.
    fn encode_rgb_png(image: RgbImage) -> Vec<u8> {
        let mut png = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut png, ImageFormat::Png)
            .expect("encode RGB fixture PNG");
        png.into_inner()
    }

    /// Models YouTube's 640×480 standard canvas around 640×360 artwork.
    fn youtube_letterbox_fixture_png() -> Vec<u8> {
        let mut image = RgbImage::from_pixel(640, 480, image::Rgb([0, 0, 0]));
        for (_, y, pixel) in image.enumerate_pixels_mut() {
            if (60..420).contains(&y) {
                *pixel = image::Rgb([220, 80, 120]);
            }
        }
        encode_rgb_png(image)
    }

    /// Encodes a small PNG fixture for thumbnail worker tests.
    pub(crate) fn fixture_png() -> Vec<u8> {
        let mut png = Cursor::new(Vec::new());
        DynamicImage::new_rgba8(4, 3)
            .write_to(&mut png, ImageFormat::Png)
            .expect("encode fixture PNG");
        png.into_inner()
    }

    /// Encodes a landscape fixture large enough to span several terminal rows.
    pub(crate) fn fixture_thumbnail_png() -> Vec<u8> {
        let mut png = Cursor::new(Vec::new());
        DynamicImage::new_rgba8(320, 180)
            .write_to(&mut png, ImageFormat::Png)
            .expect("encode terminal-sized fixture PNG");
        png.into_inner()
    }

    fn wait_for_terminal_state(manager: &mut ThumbnailManager) -> ThumbnailState {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            manager.poll();
            if manager.state() != &ThumbnailState::Loading {
                return manager.state().clone();
            }
            assert!(
                Instant::now() < deadline,
                "thumbnail remained Loading after the worker deadline"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_for_queued_result(manager: &ThumbnailManager) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while manager
            .result_receiver
            .as_ref()
            .is_some_and(Receiver::is_empty)
        {
            assert!(
                Instant::now() < deadline,
                "thumbnail worker did not publish its cancelled result"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn serve_once(
        status: &'static str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> (Url, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock thumbnail server");
        let address = listener.local_addr().expect("mock server address");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept mock request");
            let mut request = [0_u8; 2_048];
            let _ = stream.read(&mut request).expect("read mock request");
            let has_content_length = headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("content-length"));
            write!(stream, "HTTP/1.1 {status}\r\nConnection: close\r\n")
                .expect("write mock status");
            for (name, value) in headers {
                write!(stream, "{name}: {value}\r\n").expect("write mock header");
            }
            if !has_content_length {
                write!(stream, "Content-Length: {}\r\n", body.len())
                    .expect("write mock body length");
            }
            stream.write_all(b"\r\n").expect("finish mock headers");
            stream.write_all(&body).expect("write mock body");
            stream.flush().expect("flush mock response");
        });
        (
            Url::parse(&format!("http://{address}/thumbnail")).expect("mock thumbnail URL"),
            handle,
        )
    }
}
