//! Playback abstractions and external process adapters.
//!
//! Youta owns the user interface. Playback engines such as mpv run without a
//! terminal interface and are controlled through this module.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;

use crate::config::{
    AudioOutput as ConfiguredAudioOutput, Config, PlaybackBackend as ConfiguredBackend,
};

#[cfg(feature = "backend-mpv")]
pub mod mpv;

#[cfg(feature = "backend-mpv")]
mod mpv_ipc;

pub mod threaded;

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
    /// rejects the extractor's initial URL or yt-dlp finds no requested
    /// audio-only format. The retry may use a muxed stream when no audio-only
    /// stream exists. Normal loads keep it disabled to avoid an extra remote
    /// format check.
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
    /// Start recording a stream to this path, or stop and finalize recording.
    ///
    /// A [`Some`] path starts or redirects recording. [`None`] stops the
    /// active recording and lets the backend finalize the output file.
    SetStreamRecording(Option<PathBuf>),
    /// Stop the current item while keeping the backend available.
    Stop,
}

/// Current state reported by a playback backend.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PlaybackStatus {
    /// Whether the backend is alive but has no media loaded.
    ///
    /// Controllers use the transition from active media to this state as the
    /// end-of-file signal. An idle status observed before media becomes active
    /// is not treated as completion.
    pub idle: bool,
    /// Whether the active item is an endless live stream.
    ///
    /// Controllers own this semantic flag because a backend may expose live
    /// transport timestamps as a large, apparently finite duration. Seeking
    /// is available only when [`Self::live_seekable_range`] is populated.
    pub live: bool,
    /// Backend media-time interval currently available for live cache seeking.
    ///
    /// Controllers may normalize [`Self::position`] and [`Self::duration`] to
    /// this rolling interval for display, but retain these absolute timestamps
    /// when translating a seek-bar percentage back to a backend command.
    pub live_seekable_range: Option<BufferedRange>,
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

impl PlaybackStatus {
    /// Returns whether keyboard or mouse seeking has a usable timeline.
    ///
    /// An idle reusable backend owns no media timeline. Active finite media
    /// remains seekable while duration is unknown, and an active live stream
    /// becomes seekable only after its backend reports an exact cached
    /// interval.
    #[must_use]
    pub const fn seeking_available(&self) -> bool {
        !self.idle && (!self.live || self.live_seekable_range.is_some())
    }
}

/// A half-open media-time interval available without another network fetch.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
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
            live: false,
            live_seekable_range: None,
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
    /// Returns the operating-system process that owns backend playback.
    ///
    /// Frontends use this optional identity only to correlate an audio-server
    /// stream with the private player they started. In-process backends and
    /// adapters without a stable process retain the default `None` value.
    fn process_id(&self) -> Option<u32> {
        None
    }

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

impl ProcessPlaybackConfig {
    /// Derives process-backend settings from the user's configuration.
    ///
    /// Every front-end starts the same engine, so this mapping belongs beside
    /// the backend rather than inside one executable.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let audio_output = match config.playback.output {
            ConfiguredAudioOutput::Auto => AudioOutputDriver::Auto,
            ConfiguredAudioOutput::Alsa => AudioOutputDriver::Alsa,
            ConfiguredAudioOutput::Jack => AudioOutputDriver::Jack,
            ConfiguredAudioOutput::PulseAudio => AudioOutputDriver::PulseAudio,
            ConfiguredAudioOutput::PipeWire => AudioOutputDriver::PipeWire,
        };
        Self {
            mpv_executable: config.providers.mpv_executable.clone(),
            yt_dlp_executable: config.providers.yt_dlp_executable.clone(),
            runtime_dir: config.config_dir().join("runtime"),
            audio_output,
            audio_device: config.playback.device.clone(),
            profile: if config.playback.audiophile.enabled {
                PlaybackProfile::Direct
            } else {
                PlaybackProfile::Balanced
            },
            audiophile: AudiophilePlaybackOptions {
                exclusive_device: config.playback.audiophile.exclusive_device,
                avoid_resampling: config.playback.audiophile.avoid_resampling,
                output_sample_rate_hz: config.playback.audiophile.output_sample_rate_hz,
            },
        }
    }
}

/// Factory used to start a playback engine only when the user presses Play.
///
/// Delaying process creation keeps startup fast and avoids an idle decoder for
/// users who only browse subscriptions or metadata.
pub type PlaybackFactory = Box<dyn FnMut() -> Result<Box<dyn PlaybackBackend>> + Send + 'static>;

/// Builds the playback factory selected by the user's configuration.
///
/// Returns [`None`] when the configured engine is absent from this build. A
/// controller without a factory stays completely browsable and reports the
/// missing engine when the user presses Play, so callers should not treat this
/// as a startup failure.
#[must_use]
pub fn configured_playback_factory(config: &Config) -> Option<PlaybackFactory> {
    match config.playback.backend {
        ConfiguredBackend::Mpv => {
            #[cfg(feature = "backend-mpv")]
            {
                let process_config = ProcessPlaybackConfig::from_config(config);
                let factory: PlaybackFactory = Box::new(move || {
                    // Spawning stays synchronous so a missing mpv or an unusable
                    // runtime directory is still reported to the caller. Only the
                    // per-tick IPC polling moves off the caller's thread.
                    let player = mpv::MpvBackend::spawn(&process_config)?;
                    let supervised: Box<dyn PlaybackBackend> =
                        Box::new(threaded::ThreadedBackend::new(player));
                    Ok(supervised)
                });
                Some(factory)
            }
            #[cfg(not(feature = "backend-mpv"))]
            {
                None
            }
        }
        ConfiguredBackend::Native => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AudioOutputDriver, Config, PlaybackProfile, PlaybackStatus, ProcessPlaybackConfig,
    };
    use crate::config::AudioOutput as ConfiguredAudioOutput;
    use tempfile::tempdir;

    #[test]
    fn audio_output_names_match_mpv_drivers() {
        assert_eq!(AudioOutputDriver::Auto.mpv_name(), None);
        assert_eq!(AudioOutputDriver::Null.mpv_name(), Some("null"));
        assert_eq!(AudioOutputDriver::Alsa.mpv_name(), Some("alsa"));
        assert_eq!(AudioOutputDriver::Jack.mpv_name(), Some("jack"));
        assert_eq!(AudioOutputDriver::PulseAudio.mpv_name(), Some("pulse"));
        assert_eq!(AudioOutputDriver::PipeWire.mpv_name(), Some("pipewire"));
    }

    #[test]
    fn seeking_requires_an_active_usable_timeline() {
        let idle = PlaybackStatus::default();
        let finite = PlaybackStatus {
            idle: false,
            ..PlaybackStatus::default()
        };
        let live_without_cache = PlaybackStatus {
            idle: false,
            live: true,
            ..PlaybackStatus::default()
        };
        let live_with_cache = PlaybackStatus {
            live_seekable_range: Some(super::BufferedRange {
                start: std::time::Duration::from_secs(100),
                end: std::time::Duration::from_secs(200),
            }),
            ..live_without_cache.clone()
        };

        assert!(!idle.seeking_available());
        assert!(finite.seeking_available());
        assert!(!live_without_cache.seeking_available());
        assert!(live_with_cache.seeking_available());
    }

    #[test]
    fn process_config_maps_every_audio_output_driver() {
        let temporary = tempdir().expect("temporary directory");
        let mut config = Config::for_dir(temporary.path());
        for (configured, expected) in [
            (ConfiguredAudioOutput::Auto, AudioOutputDriver::Auto),
            (ConfiguredAudioOutput::Alsa, AudioOutputDriver::Alsa),
            (ConfiguredAudioOutput::Jack, AudioOutputDriver::Jack),
            (
                ConfiguredAudioOutput::PulseAudio,
                AudioOutputDriver::PulseAudio,
            ),
            (ConfiguredAudioOutput::PipeWire, AudioOutputDriver::PipeWire),
        ] {
            config.playback.output = configured;
            assert_eq!(
                ProcessPlaybackConfig::from_config(&config).audio_output,
                expected
            );
        }
    }

    #[test]
    fn process_config_preserves_audiophile_tuning() {
        let temporary = tempdir().expect("temporary directory");
        let mut config = Config::for_dir(temporary.path());
        config.playback.audiophile.enabled = true;
        config.playback.audiophile.exclusive_device = true;
        config.playback.audiophile.avoid_resampling = true;
        config.playback.audiophile.output_sample_rate_hz = Some(96_000);

        let process = ProcessPlaybackConfig::from_config(&config);
        assert_eq!(process.profile, PlaybackProfile::Direct);
        assert!(process.audiophile.exclusive_device);
        assert!(process.audiophile.avoid_resampling);
        assert_eq!(process.audiophile.output_sample_rate_hz, Some(96_000));
    }

    /// The native backend is a configuration placeholder with no adapter, so a
    /// controller must be told there is nothing to spawn rather than be handed
    /// a factory that fails at the moment the user presses Play.
    #[test]
    fn the_native_backend_yields_no_factory() {
        use crate::config::PlaybackBackend as ConfiguredBackend;

        let temporary = tempdir().expect("temporary directory");
        let mut config = Config::for_dir(temporary.path());
        config.playback.backend = ConfiguredBackend::Native;

        assert!(super::configured_playback_factory(&config).is_none());
    }
}
