//! Lazy, bounded terminal-thumbnail loading and protocol encoding.
//!
//! Capability detection is conservative: automatic mode never writes a probe
//! to the terminal, and unsupported terminals never start the network worker.
//! Fetching, decoding, resizing, and protocol encoding all happen away from the
//! TUI render thread.

use std::fs::{self, OpenOptions};
use std::io::{self, Cursor, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime};

use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded};
use image::{DynamicImage, ImageFormat, ImageReader, Limits};
use ratatui::layout::Rect;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{Resize, ResizeEncodeRender};
use sha2::{Digest, Sha256};
use url::Url;

use crate::config::ThumbnailMode;

const MAX_DOWNLOAD_BYTES: usize = 4 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 4_096;
const MAX_DECODE_ALLOC_BYTES: u64 = 32 * 1024 * 1024;
const REQUEST_DEBOUNCE: Duration = Duration::from_millis(150);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const FALLBACK_FONT_SIZE: (u16, u16) = (10, 20);
const CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const CACHE_MAX_BYTES: u64 = 64 * 1024 * 1024;
const CACHE_MAX_ENTRIES: usize = 256;
const CACHE_FILE_EXTENSION: &str = "image";
static CACHE_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Graphics protocol selected for terminal artwork.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThumbnailProtocol {
    /// Kitty graphics protocol.
    Kitty,
    /// iTerm2 inline-image protocol, also supported by `WezTerm`.
    Iterm2,
    /// DEC Sixel graphics.
    Sixel,
}

impl ThumbnailProtocol {
    fn ratatui(self) -> ProtocolType {
        match self {
            Self::Kitty => ProtocolType::Kitty,
            Self::Iterm2 => ProtocolType::Iterm2,
            Self::Sixel => ProtocolType::Sixel,
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

/// Safe, URL-free reason why selected artwork could not be displayed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThumbnailFailure {
    /// The provider returned an unsupported or unsafe URL.
    InvalidSource,
    /// The image could not be downloaded before the bounded timeout.
    DownloadFailed,
    /// The response exceeded Youta's thumbnail byte limit.
    ResponseTooLarge,
    /// The response was not JPEG, PNG, or WebP.
    UnsupportedFormat,
    /// The image was malformed or exceeded decode limits.
    InvalidImage,
    /// The terminal protocol encoder rejected the image.
    EncodingFailed,
    /// The background thumbnail worker stopped unexpectedly.
    WorkerStopped,
}

impl std::fmt::Display for ThumbnailFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidSource => "thumbnail source is not HTTP or HTTPS",
            Self::DownloadFailed => "thumbnail download failed",
            Self::ResponseTooLarge => "thumbnail exceeds the 4 MiB download limit",
            Self::UnsupportedFormat => "thumbnail is not JPEG, PNG, or WebP",
            Self::InvalidImage => "thumbnail is invalid or exceeds decode limits",
            Self::EncodingFailed => "terminal thumbnail encoding failed",
            Self::WorkerStopped => "thumbnail worker stopped",
        };
        formatter.write_str(message)
    }
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
        Self {
            stdin_is_terminal: io::stdin().is_terminal(),
            stdout_is_terminal: io::stdout().is_terminal(),
            term,
            term_program,
            lc_terminal,
            kitty_window: std::env::var_os("KITTY_WINDOW_ID").is_some(),
            wezterm_pane: std::env::var_os("WEZTERM_PANE").is_some(),
            tmux,
            output_device: std::fs::read_link("/proc/self/fd/1").ok(),
            font_size: terminal_font_size(),
        }
    }

    fn hard_unsupported(&self) -> bool {
        if !self.stdin_is_terminal || !self.stdout_is_terminal || self.tmux {
            return true;
        }
        let Some(term) = self.term.as_deref() else {
            return true;
        };
        let normalized = term.to_ascii_lowercase();
        normalized == "dumb"
            || normalized == "linux"
            || normalized.starts_with("vt")
            || self
                .output_device
                .as_deref()
                .is_some_and(is_plain_linux_console_or_serial)
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

        if self.kitty_window || term.contains("kitty") {
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct ThumbnailTarget {
    source: Url,
    area: Rect,
}

#[derive(Clone, Debug)]
struct WorkerRequest {
    generation: u64,
    target: ThumbnailTarget,
}

struct WorkerResult {
    generation: u64,
    result: Result<StatefulProtocol, ThumbnailFailure>,
}

trait ThumbnailTransport: Send + 'static {
    fn fetch(&mut self, source: &Url) -> Result<Vec<u8>, ThumbnailFailure>;
}

struct HttpThumbnailTransport {
    agent: ureq::Agent,
}

impl ThumbnailTransport for HttpThumbnailTransport {
    fn fetch(&mut self, source: &Url) -> Result<Vec<u8>, ThumbnailFailure> {
        fetch_thumbnail(&self.agent, source)
    }
}

#[derive(Clone, Copy)]
struct ThumbnailCachePolicy {
    max_age: Duration,
    max_bytes: u64,
    max_entries: usize,
}

impl Default for ThumbnailCachePolicy {
    fn default() -> Self {
        Self {
            max_age: CACHE_MAX_AGE,
            max_bytes: CACHE_MAX_BYTES,
            max_entries: CACHE_MAX_ENTRIES,
        }
    }
}

struct ThumbnailCache {
    directory: PathBuf,
    policy: ThumbnailCachePolicy,
}

impl ThumbnailCache {
    fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            policy: ThumbnailCachePolicy::default(),
        }
    }

    #[cfg(test)]
    fn with_policy(directory: PathBuf, policy: ThumbnailCachePolicy) -> Self {
        Self { directory, policy }
    }

    fn read(&self, source: &Url) -> io::Result<Option<Vec<u8>>> {
        if !self.directory.exists() {
            return Ok(None);
        }
        self.secure_directory()?;
        let path = self.entry_path(source);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_file()
            || metadata.len() == 0
            || metadata.len() > MAX_DOWNLOAD_BYTES as u64
            || self.is_expired(&metadata)
        {
            remove_cache_entry(&path);
            return Ok(None);
        }
        if !self.is_confined_entry(&path)? {
            remove_cache_entry(&path);
            return Ok(None);
        }

        let file = fs::File::open(&path)?;
        let mut bytes = Vec::with_capacity(
            usize::try_from(metadata.len())
                .unwrap_or(MAX_DOWNLOAD_BYTES)
                .min(MAX_DOWNLOAD_BYTES),
        );
        file.take(u64::try_from(MAX_DOWNLOAD_BYTES + 1).unwrap_or(u64::MAX))
            .read_to_end(&mut bytes)?;
        if bytes.is_empty() || bytes.len() > MAX_DOWNLOAD_BYTES {
            remove_cache_entry(&path);
            return Ok(None);
        }
        Ok(Some(bytes))
    }

    fn prepare(&self) -> io::Result<()> {
        self.secure_directory()?;
        self.evict()
    }

    fn store(&self, source: &Url, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() || bytes.len() > MAX_DOWNLOAD_BYTES {
            return Ok(());
        }
        self.secure_directory()?;
        let path = self.entry_path(source);
        self.write_atomic(&path, bytes)?;
        self.evict()
    }

    fn remove(&self, source: &Url) {
        remove_cache_entry(&self.entry_path(source));
    }

    fn entry_path(&self, source: &Url) -> PathBuf {
        let digest = Sha256::digest(source.as_str().as_bytes());
        self.directory
            .join(format!("{digest:x}.{CACHE_FILE_EXTENSION}"))
    }

    fn secure_directory(&self) -> io::Result<()> {
        fs::create_dir_all(&self.directory)?;
        let metadata = fs::symlink_metadata(&self.directory)?;
        if !metadata.file_type().is_dir() {
            return Err(io::Error::other("thumbnail cache path is not a directory"));
        }
        set_private_directory_permissions(&self.directory)
    }

    fn is_confined_entry(&self, path: &Path) -> io::Result<bool> {
        let directory = fs::canonicalize(&self.directory)?;
        let entry = fs::canonicalize(path)?;
        Ok(entry.parent() == Some(directory.as_path()))
    }

    fn is_expired(&self, metadata: &fs::Metadata) -> bool {
        metadata
            .modified()
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age > self.policy.max_age)
    }

    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let sequence = CACHE_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = self.directory.join(format!(
            ".thumbnail.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;

                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            set_private_file_permissions(&temporary)?;
            fs::rename(&temporary, path)?;
            set_private_file_permissions(path)?;
            let _ = fs::File::open(&self.directory).and_then(|directory| directory.sync_all());
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn evict(&self) -> io::Result<()> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            if is_cache_temp_name(&entry.file_name()) {
                let path = entry.path();
                if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_file())
                {
                    remove_cache_entry(&path);
                }
                continue;
            }
            if !is_cache_entry_name(&entry.file_name()) {
                continue;
            }
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if !metadata.file_type().is_file()
                || metadata.len() == 0
                || metadata.len() > MAX_DOWNLOAD_BYTES as u64
                || self.is_expired(&metadata)
            {
                remove_cache_entry(&path);
                continue;
            }
            entries.push(CacheEntry {
                path,
                modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                bytes: metadata.len(),
            });
        }

        entries.sort_by(|left, right| {
            left.modified
                .cmp(&right.modified)
                .then_with(|| left.path.cmp(&right.path))
        });
        let mut total_bytes = entries.iter().map(|entry| entry.bytes).sum::<u64>();
        let mut total_entries = entries.len();
        for entry in entries {
            if total_entries <= self.policy.max_entries && total_bytes <= self.policy.max_bytes {
                break;
            }
            if fs::remove_file(&entry.path).is_ok() {
                total_entries = total_entries.saturating_sub(1);
                total_bytes = total_bytes.saturating_sub(entry.bytes);
            }
        }
        Ok(())
    }
}

struct CacheEntry {
    path: PathBuf,
    modified: SystemTime,
    bytes: u64,
}

fn is_cache_entry_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(digest) = name.strip_suffix(&format!(".{CACHE_FILE_EXTENSION}")) else {
        return false;
    };
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn is_cache_temp_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(sequence) = name
        .strip_prefix(".thumbnail.")
        .and_then(|name| name.strip_suffix(".tmp"))
    else {
        return false;
    };
    let mut components = sequence.split('.');
    matches!(
        (components.next(), components.next(), components.next()),
        (Some(process), Some(sequence), None)
            if !process.is_empty()
                && !sequence.is_empty()
                && process.bytes().all(|byte| byte.is_ascii_digit())
                && sequence.bytes().all(|byte| byte.is_ascii_digit())
    )
}

fn remove_cache_entry(path: &Path) {
    let _ = fs::remove_file(path);
}

fn set_private_directory_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Owns the selected thumbnail's bounded background pipeline and ready image.
pub struct ThumbnailManager {
    capability: ThumbnailCapability,
    state: ThumbnailState,
    generation: u64,
    target: Option<ThumbnailTarget>,
    protocol: Option<StatefulProtocol>,
    picker: Option<Picker>,
    cache_directory: Option<PathBuf>,
    request_sender: Option<Sender<WorkerRequest>>,
    request_discarder: Option<Receiver<WorkerRequest>>,
    result_receiver: Option<Receiver<WorkerResult>>,
}

impl ThumbnailManager {
    /// Detects the current terminal and starts a worker only when useful.
    ///
    /// `Auto` relies only on environment variables and terminal ioctls. `On`
    /// may issue a capability query when the environment is inconclusive, so
    /// callers should construct the manager after entering the alternate
    /// screen and before reading terminal events.
    #[must_use]
    pub fn from_current_terminal(mode: ThumbnailMode) -> Self {
        Self::from_terminal_info_with_cache(mode, &TerminalInfo::current(), None)
    }

    /// Detects the current terminal and lazily enables a persistent byte cache.
    ///
    /// The cache directory is not created until a supported terminal requests
    /// visible artwork. Unsupported terminals never read or write the cache.
    #[must_use]
    pub fn from_current_terminal_with_cache(mode: ThumbnailMode, cache_directory: PathBuf) -> Self {
        Self::from_terminal_info_with_cache(mode, &TerminalInfo::current(), Some(cache_directory))
    }

    #[cfg(test)]
    fn from_terminal_info(mode: ThumbnailMode, terminal: &TerminalInfo) -> Self {
        Self::from_terminal_info_with_cache(mode, terminal, None)
    }

    fn from_terminal_info_with_cache(
        mode: ThumbnailMode,
        terminal: &TerminalInfo,
        cache_directory: Option<PathBuf>,
    ) -> Self {
        if mode == ThumbnailMode::Off {
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
            target: None,
            protocol: None,
            picker: Some(picker),
            cache_directory,
            request_sender: None,
            request_discarder: None,
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
            target: None,
            protocol: None,
            picker: None,
            cache_directory: None,
            request_sender: None,
            request_discarder: None,
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
            area,
        };
        if self.target.as_ref() == Some(&target) {
            return false;
        }

        self.generation = self.generation.wrapping_add(1);
        self.target = Some(target.clone());
        self.protocol = None;
        if !is_safe_thumbnail_source(&target.source) {
            self.state = ThumbnailState::Failed(ThumbnailFailure::InvalidSource);
            return true;
        }
        self.state = ThumbnailState::Loading;
        let request = WorkerRequest {
            generation: self.generation,
            target,
        };
        if !self.ensure_worker() || !self.send_latest(request) {
            self.state = ThumbnailState::Failed(ThumbnailFailure::WorkerStopped);
        }
        true
    }

    fn ensure_worker(&mut self) -> bool {
        if self.request_sender.is_some() {
            return true;
        }
        let Some(picker) = self.picker.take() else {
            return false;
        };
        let (request_sender, request_receiver) = bounded(1);
        let request_discarder = request_receiver.clone();
        let (result_sender, result_receiver) = bounded(1);
        let spawned = spawn_worker(
            picker,
            request_receiver,
            result_sender,
            self.cache_directory.clone(),
        );
        self.request_sender = Some(request_sender);
        self.request_discarder = Some(request_discarder);
        self.result_receiver = Some(result_receiver);
        spawned
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

    /// Applies completed work for the current selection and discards stale work.
    ///
    /// Returns `true` when the visible state changed.
    pub fn poll(&mut self) -> bool {
        if self.result_receiver.is_none() {
            if self.state == ThumbnailState::Loading {
                self.protocol = None;
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
                        Ok(protocol) => {
                            self.protocol = Some(protocol);
                            self.state = ThumbnailState::Ready;
                        }
                        Err(error) => {
                            self.protocol = None;
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

    /// Returns the safe user-facing state of the selected thumbnail.
    #[must_use]
    pub const fn state(&self) -> &ThumbnailState {
        &self.state
    }

    /// Drops the selected thumbnail and invalidates any in-flight result.
    ///
    /// Returns `true` when visible state was cleared.
    pub fn clear(&mut self) -> bool {
        let changed = self.target.take().is_some()
            || self.protocol.take().is_some()
            || self.state == ThumbnailState::Loading
            || matches!(
                self.state,
                ThumbnailState::Ready | ThumbnailState::Failed(_)
            );
        if changed {
            self.generation = self.generation.wrapping_add(1);
            self.state = ThumbnailState::Idle;
        }
        changed
    }
}

fn spawn_worker(
    picker: Picker,
    requests: Receiver<WorkerRequest>,
    results: Sender<WorkerResult>,
    cache_directory: Option<PathBuf>,
) -> bool {
    spawn_worker_with_transport(
        picker,
        requests,
        results,
        HttpThumbnailTransport {
            agent: thumbnail_agent(),
        },
        cache_directory.map(ThumbnailCache::new),
        REQUEST_DEBOUNCE,
    )
}

fn spawn_worker_with_transport<T: ThumbnailTransport>(
    picker: Picker,
    requests: Receiver<WorkerRequest>,
    results: Sender<WorkerResult>,
    mut transport: T,
    mut cache: Option<ThumbnailCache>,
    debounce: Duration,
) -> bool {
    thread::Builder::new()
        .name("youta-thumbnail".to_owned())
        .spawn(move || {
            if let Some(cache) = cache.as_ref() {
                let _ = cache.prepare();
            }
            while let Ok(mut request) = requests.recv() {
                for newer in requests.try_iter() {
                    request = newer;
                }
                if let Some(result) =
                    load_cached_thumbnail(cache.as_mut(), &picker, &request.target)
                {
                    if results
                        .send(WorkerResult {
                            generation: request.generation,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }

                // Selection churn is useful to debounce before network I/O,
                // but a validated disk-cache hit should never pay this delay.
                if !debounce.is_zero() {
                    thread::sleep(debounce);
                }
                for newer in requests.try_iter() {
                    request = newer;
                }
                let result =
                    load_thumbnail(&mut transport, cache.as_mut(), &picker, &request.target);
                if results
                    .send(WorkerResult {
                        generation: request.generation,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .is_ok()
}

fn thumbnail_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .user_agent(concat!(
            "youta/",
            env!("CARGO_PKG_VERSION"),
            " (+",
            env!("CARGO_PKG_REPOSITORY"),
            ")"
        ))
        .build()
        .into()
}

fn load_thumbnail(
    transport: &mut impl ThumbnailTransport,
    mut cache: Option<&mut ThumbnailCache>,
    picker: &Picker,
    target: &ThumbnailTarget,
) -> Result<StatefulProtocol, ThumbnailFailure> {
    if let Some(result) = load_cached_thumbnail(cache.as_deref_mut(), picker, target) {
        return result;
    }

    let bytes = transport.fetch(&target.source)?;
    let image = decode_thumbnail(&bytes)?;
    if let Some(cache) = cache {
        let _ = cache.store(&target.source, &bytes);
    }
    encode_thumbnail(picker, target.area, image)
}

/// Loads and encodes one validated persistent-cache entry without network I/O.
///
/// `None` means the worker should proceed through its debounced network path.
/// Corrupt cached bytes are removed so the subsequent fetch can repair them.
fn load_cached_thumbnail(
    cache: Option<&mut ThumbnailCache>,
    picker: &Picker,
    target: &ThumbnailTarget,
) -> Option<Result<StatefulProtocol, ThumbnailFailure>> {
    let cache = cache?;
    let bytes = cache.read(&target.source).ok().flatten()?;
    let image = match decode_thumbnail(&bytes) {
        Ok(image) => image,
        Err(_) => {
            cache.remove(&target.source);
            return None;
        }
    };
    Some(encode_thumbnail(picker, target.area, image))
}

/// Resizes and encodes a decoded image for one exact terminal-cell area.
fn encode_thumbnail(
    picker: &Picker,
    area: Rect,
    image: DynamicImage,
) -> Result<StatefulProtocol, ThumbnailFailure> {
    let mut protocol = picker.new_resize_protocol(image);
    protocol.resize_encode(&Resize::Fit(None), area.into());
    match protocol.last_encoding_result() {
        Some(Ok(())) => Ok(protocol),
        Some(Err(_)) | None => Err(ThumbnailFailure::EncodingFailed),
    }
}

fn fetch_thumbnail(agent: &ureq::Agent, source: &Url) -> Result<Vec<u8>, ThumbnailFailure> {
    if !is_safe_thumbnail_source(source) {
        return Err(ThumbnailFailure::InvalidSource);
    }
    let mut response = agent
        .get(source.as_str())
        .header("Accept", "image/jpeg, image/png, image/webp")
        .call()
        .map_err(|_| ThumbnailFailure::DownloadFailed)?;
    if response
        .body()
        .content_length()
        .is_some_and(|length| length > MAX_DOWNLOAD_BYTES as u64)
    {
        return Err(ThumbnailFailure::ResponseTooLarge);
    }
    let bytes = response
        .body_mut()
        .with_config()
        .limit(u64::try_from(MAX_DOWNLOAD_BYTES.saturating_add(1)).unwrap_or(u64::MAX))
        .read_to_vec()
        .map_err(|error| match error {
            ureq::Error::BodyExceedsLimit(_) => ThumbnailFailure::ResponseTooLarge,
            _ => ThumbnailFailure::DownloadFailed,
        })?;
    if bytes.len() > MAX_DOWNLOAD_BYTES {
        Err(ThumbnailFailure::ResponseTooLarge)
    } else {
        Ok(bytes)
    }
}

fn is_safe_thumbnail_source(source: &Url) -> bool {
    matches!(source.scheme(), "http" | "https")
        && source.username().is_empty()
        && source.password().is_none()
}

fn decode_thumbnail(bytes: &[u8]) -> Result<DynamicImage, ThumbnailFailure> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
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

fn is_plain_linux_console_or_serial(path: &Path) -> bool {
    let text = path.to_string_lossy();
    if text == "/dev/console" {
        return true;
    }
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    (name.strip_prefix("tty").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })) || ["ttyS", "ttyUSB", "ttyACM", "rfcomm"]
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

    fn graphical_terminal() -> TerminalInfo {
        TerminalInfo {
            stdin_is_terminal: true,
            stdout_is_terminal: true,
            term: Some("xterm-kitty".to_owned()),
            term_program: None,
            lc_terminal: None,
            kitty_window: true,
            wezterm_pane: false,
            tmux: false,
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

        let unknown = TerminalInfo {
            term: Some("xterm-256color".to_owned()),
            kitty_window: false,
            ..kitty
        };
        assert_eq!(unknown.environment_protocol(), None);
    }

    #[test]
    fn automatic_detection_rejects_ttys_serial_tmux_and_missing_term() {
        for terminal in [
            TerminalInfo {
                term: Some("linux".to_owned()),
                kitty_window: false,
                output_device: Some(PathBuf::from("/dev/tty1")),
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
            output_device: Some(PathBuf::from("/dev/tty2")),
            ..graphical_terminal()
        };
        let mut unsupported =
            ThumbnailManager::from_terminal_info(ThumbnailMode::Auto, &unsupported_info);
        let url = Url::parse("https://images.example/thumbnail.jpg").expect("fixture URL");
        assert!(!unsupported.synchronize(Some(&url), Rect::new(0, 0, 20, 8)));
        assert_eq!(unsupported.state(), &ThumbnailState::Unsupported);
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
            output_device: Some(PathBuf::from("/dev/tty3")),
            ..graphical_terminal()
        };
        let mut manager = ThumbnailManager::from_terminal_info_with_cache(
            ThumbnailMode::Auto,
            &terminal,
            Some(cache_directory.clone()),
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
        let bytes =
            fetch_thumbnail(&thumbnail_agent(), &source).expect("fetch bounded fixture image");
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
            fetch_thumbnail(&thumbnail_agent(), &oversized)
                .expect_err("oversized response must be rejected"),
            ThumbnailFailure::ResponseTooLarge
        );
        server.join().expect("oversized fixture server");

        let file = Url::parse("file:///tmp/not-remote.png").expect("fixture file URL");
        assert_eq!(
            fetch_thumbnail(&thumbnail_agent(), &file)
                .expect_err("non-HTTP source must be rejected"),
            ThumbnailFailure::InvalidSource
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
    fn stale_worker_results_cannot_replace_the_current_selection() {
        let (request_sender, request_receiver) = bounded(1);
        let request_discarder = request_receiver.clone();
        let (result_sender, result_receiver) = bounded(1);
        let mut manager = ThumbnailManager {
            capability: ThumbnailCapability::Supported(ThumbnailProtocol::Kitty),
            state: ThumbnailState::Idle,
            generation: 0,
            target: None,
            protocol: None,
            picker: None,
            cache_directory: None,
            request_sender: Some(request_sender),
            request_discarder: Some(request_discarder),
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
            target: Some(ThumbnailTarget {
                source,
                area: Rect::new(0, 0, 20, 8),
            }),
            protocol: None,
            picker: None,
            cache_directory: None,
            request_sender: Some(request_sender),
            request_discarder: None,
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

    fn manager_with_mock_transport_in_cache(cache_directory: Option<PathBuf>) -> MockManagerParts {
        manager_with_mock_transport_and_cache(cache_directory, Duration::ZERO)
    }

    fn manager_with_mock_transport_and_cache(
        cache_directory: Option<PathBuf>,
        debounce: Duration,
    ) -> MockManagerParts {
        let (request_sender, request_receiver) = bounded(1);
        let request_discarder = request_receiver.clone();
        let (result_sender, result_receiver) = bounded(1);
        let (observed_sender, observed_receiver) = bounded(4);
        let (reply_sender, reply_receiver) = bounded(4);
        let picker = picker_for_protocol(ThumbnailProtocol::Kitty, FALLBACK_FONT_SIZE);
        assert!(spawn_worker_with_transport(
            picker,
            request_receiver,
            result_sender,
            MockTransport {
                observed: observed_sender,
                replies: reply_receiver,
            },
            cache_directory.map(ThumbnailCache::new),
            debounce,
        ));
        (
            ThumbnailManager {
                capability: ThumbnailCapability::Supported(ThumbnailProtocol::Kitty),
                state: ThumbnailState::Idle,
                generation: 0,
                target: None,
                protocol: None,
                picker: None,
                cache_directory: None,
                request_sender: Some(request_sender),
                request_discarder: Some(request_discarder),
                result_receiver: Some(result_receiver),
            },
            reply_sender,
            observed_receiver,
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
