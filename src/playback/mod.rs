//! Playback abstractions and external process adapters.
//!
//! Youta owns the user interface. Playback engines such as mpv run without a
//! terminal interface and are controlled through this module.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;

#[cfg(feature = "backend-mpv")]
pub mod mpv;

#[cfg(feature = "yt-dlp")]
pub mod ytdlp;

#[cfg(feature = "yt-dlp")]
pub mod youtube_prewarm;

/// Errors returned by a playback or extraction backend.
#[derive(Debug, Error)]
pub enum PlaybackError {
    /// A required executable was not found or could not be started.
    #[error("required executable `{0}` is unavailable")]
    ExecutableUnavailable(String),
    /// An operating-system operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A backend returned malformed JSON.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// The backend returned an unsuccessful protocol response.
    #[error("backend protocol error: {0}")]
    Protocol(String),
    /// A requested command is incompatible with the selected profile.
    #[error("the {0} command is disabled by the direct playback profile")]
    DirectProfileRestriction(&'static str),
    /// The caller supplied an invalid value.
    #[error("invalid playback value: {0}")]
    InvalidValue(String),
    /// The backend process exited unexpectedly.
    #[error("playback process exited unexpectedly{0}")]
    ProcessExited(String),
}

/// Convenient result type for playback operations.
pub type Result<T> = std::result::Result<T, PlaybackError>;

/// HTTP headers required by one short-lived resolved media URL.
///
/// Values are deliberately redacted from [`Debug`] output because yt-dlp may
/// return cookies or authorization material. Controllers must keep this value
/// in memory only and discard it with the resolved URL.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct PlaybackHttpHeaders(BTreeMap<String, String>);

impl PlaybackHttpHeaders {
    /// Wraps extractor-provided headers without logging their values.
    #[must_use]
    pub fn new(headers: BTreeMap<String, String>) -> Self {
        Self(headers)
    }

    /// Returns whether no additional request headers are required.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterates over header names and their sensitive values.
    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }
}

impl fmt::Debug for PlaybackHttpHeaders {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = self.0.keys().map(String::as_str).collect::<Vec<_>>();
        formatter
            .debug_struct("PlaybackHttpHeaders")
            .field("names", &names)
            .field("values", &"<redacted>")
            .finish()
    }
}

/// A media item to load into a playback backend.
#[derive(Clone, PartialEq, Eq)]
pub struct PlaybackInput {
    /// Local path or remote URL accepted by the selected backend.
    pub location: String,
    /// Initial playback position.
    pub start_at: Duration,
    /// Optional title used by desktop media integrations.
    pub title: Option<String>,
    /// Ask an extractor-backed remote load to verify candidate media URLs.
    ///
    /// Controllers should enable this only for a retry after a media CDN
    /// rejects the extractor's initial URL. Normal loads keep it disabled to
    /// avoid an extra remote format check.
    pub verify_remote_format: bool,
    /// Sensitive headers required by a freshly resolved direct media URL.
    pub http_headers: PlaybackHttpHeaders,
    /// Skip mpv's yt-dlp hook because [`Self::location`] is already resolved.
    pub bypass_ytdl: bool,
}

impl fmt::Debug for PlaybackInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlaybackInput")
            .field(
                "location",
                &if self.bypass_ytdl {
                    "<redacted-resolved-media-url>"
                } else {
                    self.location.as_str()
                },
            )
            .field("start_at", &self.start_at)
            .field("title", &self.title)
            .field("verify_remote_format", &self.verify_remote_format)
            .field("http_headers", &self.http_headers)
            .field("bypass_ytdl", &self.bypass_ytdl)
            .finish()
    }
}

impl PlaybackInput {
    /// Constructs a playback request starting at the beginning.
    #[must_use]
    pub fn new(location: impl Into<String>) -> Self {
        Self {
            location: location.into(),
            start_at: Duration::ZERO,
            title: None,
            verify_remote_format: false,
            http_headers: PlaybackHttpHeaders::default(),
            bypass_ytdl: false,
        }
    }
}

/// Playback tuning preset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlaybackProfile {
    /// Balanced defaults suitable for interactive playback.
    #[default]
    Balanced,
    /// Larger buffers and fewer UI wakeups for battery-powered systems.
    Battery,
    /// Direct output without software volume, speed processing, or equalization.
    Direct,
}

/// Audio output driver requested from a process playback backend.
///
/// [`AudioOutputDriver::Auto`] leaves selection to the backend. The named
/// variants map to mpv's stable audio-output driver names without accepting
/// arbitrary command-line fragments.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AudioOutputDriver {
    /// Let the playback backend select the platform default.
    #[default]
    Auto,
    /// Decode audio without opening a hardware or desktop audio device.
    ///
    /// This output is intended for opt-in integration tests that exercise a
    /// real decoder in headless environments. It is not exposed as a normal
    /// user preference.
    Null,
    /// Use the Advanced Linux Sound Architecture output.
    Alsa,
    /// Use the JACK Audio Connection Kit output.
    Jack,
    /// Use the `PulseAudio` output.
    PulseAudio,
    /// Use the `PipeWire` output.
    PipeWire,
}

impl AudioOutputDriver {
    /// Returns the mpv audio-output driver name, or `None` for automatic
    /// selection.
    #[must_use]
    pub const fn mpv_name(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Null => Some("null"),
            Self::Alsa => Some("alsa"),
            Self::Jack => Some("jack"),
            Self::PulseAudio => Some("pulse"),
            Self::PipeWire => Some("pipewire"),
        }
    }
}

/// Optional direct-output tuning applied by process playback backends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudiophilePlaybackOptions {
    /// Request exclusive device access where the selected output supports it.
    pub exclusive_device: bool,
    /// Ask the backend to follow the source rate instead of choosing a fixed
    /// software-resampled rate.
    pub avoid_resampling: bool,
    /// Fix the output sample rate when resampling is an intentional choice.
    ///
    /// A fixed rate takes precedence over [`Self::avoid_resampling`].
    pub output_sample_rate_hz: Option<u32>,
}

/// Commands understood by a playback backend.
#[derive(Clone, Debug, PartialEq)]
pub enum PlayerCommand {
    /// Toggle between paused and playing.
    TogglePause,
    /// Set the pause state explicitly.
    SetPaused(bool),
    /// Seek by a signed number of seconds.
    SeekRelative(i64),
    /// Seek to an absolute position.
    SeekAbsolute(Duration),
    /// Seek to a position expressed as `0.0..=100.0`.
    SeekPercent(f64),
    /// Set software volume in the range `0..=100`.
    SetVolume(u8),
    /// Set playback speed in the supported range `0.5..=3.0`.
    SetSpeed(f64),
    /// Select the previous or next chapter.
    ChangeChapter(i32),
    /// Repeat the current item indefinitely.
    SetRepeat(bool),
    /// Stop the current item while keeping the backend available.
    Stop,
}

/// Current state reported by a playback backend.
#[derive(Clone, Debug, PartialEq)]
pub struct PlaybackStatus {
    /// Whether the backend is alive but has no media loaded.
    ///
    /// Controllers use the transition from active media to this state as the
    /// end-of-file signal. An idle status observed before media becomes active
    /// is not treated as completion.
    pub idle: bool,
    /// Current position.
    pub position: Duration,
    /// Total duration when known.
    pub duration: Option<Duration>,
    /// Whether playback is paused.
    pub paused: bool,
    /// Software volume percentage.
    pub volume: u8,
    /// Playback speed multiplier.
    pub speed: f64,
    /// Zero-based chapter index when the source exposes chapters.
    pub chapter: Option<i64>,
    /// Whether playback is waiting for network data.
    pub buffering: bool,
    /// Normalized media-time ranges already available for seeking.
    ///
    /// Backends report these in ascending order without overlapping or empty
    /// ranges. An empty collection means the information is unavailable, not
    /// necessarily that no bytes have been buffered.
    pub buffered_ranges: Vec<BufferedRange>,
    /// Backend-reported media title.
    pub title: Option<String>,
    /// Bounded current-track text carried by a live stream.
    ///
    /// Process backends should keep this separate from [`Self::title`]:
    /// Youta owns the stable station title, while ICY or equivalent metadata
    /// may change for every song.
    pub stream_title: Option<String>,
}

/// A half-open media-time interval available without another network fetch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BufferedRange {
    /// First buffered media timestamp, inclusive.
    pub start: Duration,
    /// Last buffered media timestamp, exclusive.
    pub end: Duration,
}

impl Default for PlaybackStatus {
    fn default() -> Self {
        Self {
            idle: true,
            position: Duration::ZERO,
            duration: None,
            paused: true,
            volume: 100,
            speed: 1.0,
            chapter: None,
            buffering: false,
            buffered_ranges: Vec::new(),
            title: None,
            stream_title: None,
        }
    }
}

/// An asynchronous state transition reported by a playback backend.
///
/// Command acceptance is intentionally not represented here: a backend can
/// acknowledge a load request before it knows whether the media can be
/// decoded or an audio output can be opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaybackEvent {
    /// The backend successfully loaded the requested media.
    MediaLoaded,
    /// Decoding and output started or resumed after buffering.
    PlaybackStarted,
    /// The current media ended or failed after its load request was accepted.
    Ended(PlaybackEnd),
    /// The playback process exited without a later event being available.
    ProcessExited {
        /// Bounded, redacted context suitable for a diagnostic popup.
        diagnostic: Option<String>,
    },
}

/// Details from an asynchronous end-of-file notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaybackEnd {
    /// Backend-independent classification of the end reason.
    pub reason: PlaybackEndReason,
    /// Bounded backend error code or message, when supplied.
    pub error: Option<String>,
    /// Bounded file-specific error, when supplied separately by the backend.
    pub file_error: Option<String>,
    /// Bounded, redacted warning context collected immediately before the end.
    pub diagnostic: Option<String>,
}

/// Backend-independent reason why a media item ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaybackEndReason {
    /// The media reached its natural end.
    Eof,
    /// Playback was stopped or replaced intentionally.
    Stop,
    /// Loading, decoding, networking, or audio output failed.
    Error,
    /// A backend-specific non-error reason.
    Other(String),
}

/// Common interface implemented by playback engines.
pub trait PlaybackBackend {
    /// Loads and starts a media item.
    ///
    /// # Errors
    ///
    /// Returns an error when the input is invalid or the backend cannot load
    /// it.
    fn play(&mut self, input: &PlaybackInput) -> Result<()>;

    /// Applies a playback command.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is invalid, the profile forbids the
    /// operation, or the backend rejects it.
    fn command(&mut self, command: PlayerCommand) -> Result<()>;

    /// Reads the latest backend state.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend exits or its state cannot be read.
    fn status(&mut self) -> Result<PlaybackStatus>;

    /// Returns the next asynchronous lifecycle event without treating command
    /// acknowledgement as successful playback.
    ///
    /// Backends without an event channel may retain the default implementation
    /// and report state through [`Self::status`].
    ///
    /// # Errors
    ///
    /// Returns an error when the backend's event channel cannot be queried.
    fn poll_event(&mut self) -> Result<Option<PlaybackEvent>> {
        Ok(None)
    }

    /// Stops the backend and releases its resources.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend cannot be stopped or cleaned up.
    fn shutdown(&mut self) -> Result<()>;
}

/// Runtime configuration shared by process-based playback backends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessPlaybackConfig {
    /// Path or executable name for mpv.
    pub mpv_executable: PathBuf,
    /// Path or executable name for yt-dlp.
    pub yt_dlp_executable: PathBuf,
    /// Directory in which private IPC state may be created.
    pub runtime_dir: PathBuf,
    /// Audio output driver selected by the user.
    pub audio_output: AudioOutputDriver,
    /// Optional mpv audio device, for example `alsa/hw:1,0`.
    pub audio_device: Option<String>,
    /// Selected tuning profile.
    pub profile: PlaybackProfile,
    /// Direct-output tuning used when [`Self::profile`] is
    /// [`PlaybackProfile::Direct`].
    pub audiophile: AudiophilePlaybackOptions,
}

#[cfg(test)]
mod tests {
    use super::AudioOutputDriver;

    #[test]
    fn audio_output_names_match_mpv_drivers() {
        assert_eq!(AudioOutputDriver::Auto.mpv_name(), None);
        assert_eq!(AudioOutputDriver::Null.mpv_name(), Some("null"));
        assert_eq!(AudioOutputDriver::Alsa.mpv_name(), Some("alsa"));
        assert_eq!(AudioOutputDriver::Jack.mpv_name(), Some("jack"));
        assert_eq!(AudioOutputDriver::PulseAudio.mpv_name(), Some("pulse"));
        assert_eq!(AudioOutputDriver::PipeWire.mpv_name(), Some("pipewire"));
    }
}
