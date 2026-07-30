//! Bounded local-file waveform extraction through an external `FFmpeg` process.
//!
//! `FFmpeg` decodes the selected local audio stream to signed 16-bit PCM. Rust
//! calculates one cross-channel minimum/maximum pair for each fixed-size
//! source-frame bucket and progressively compacts the stream, so neither
//! decoded PCM nor whole-file helper output is retained in memory. A bounded
//! textual-statistics compatibility path supports callers without known
//! channel count. Known long timelines start at the aligned power-of-two
//! bucket size that the builder would otherwise reach through inevitable
//! compactions.

use crate::waveform::{Peak, PeakPyramid, PeakPyramidBuilder};
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// Number of source audio frames summarized by one `FFmpeg` statistics record.
pub const DEFAULT_FRAMES_PER_PEAK: usize = 4_096;
/// Maximum number of peaks retained at the finest in-memory resolution.
pub const DEFAULT_MAXIMUM_PEAKS: usize = 4_096;
/// Largest configurable finest-level peak allocation.
pub const MAXIMUM_PEAK_LIMIT: usize = 1_048_576;
/// Maximum time spent decoding one local file.
pub const DEFAULT_EXTRACTION_TIMEOUT: Duration = Duration::from_mins(2);

const DEFAULT_PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(20);
const CHILD_REAP_GRACE_PERIOD: Duration = Duration::from_millis(250);
const MAX_METADATA_LINE_BYTES: usize = 512;
const PCM_READ_BUFFER_BYTES: usize = 64 * 1024;

/// Replacement-sensitive identity of one regular local file.
///
/// The identity is checked before `FFmpeg` starts and after it exits. A result
/// from a file replaced in place is therefore never accepted or cached.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LocalWaveformIdentity {
    length: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
}

impl LocalWaveformIdentity {
    /// Reads the current identity of a regular file.
    ///
    /// # Errors
    ///
    /// Returns [`LocalWaveformError::FileUnavailable`] when metadata cannot be
    /// read, or [`LocalWaveformError::NotRegularFile`] for a non-file path.
    pub fn from_path(path: &Path) -> Result<Self, LocalWaveformError> {
        let metadata = fs::metadata(path).map_err(LocalWaveformError::FileUnavailable)?;
        if !metadata.is_file() {
            return Err(LocalWaveformError::NotRegularFile);
        }
        Ok(Self {
            length: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            changed_seconds: metadata.ctime(),
            #[cfg(unix)]
            changed_nanoseconds: metadata.ctime_nsec(),
        })
    }

    /// Returns the exact byte length captured by this identity.
    #[must_use]
    pub const fn length(&self) -> u64 {
        self.length
    }
}

/// One identity-bound local waveform request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalWaveformRequest {
    path: PathBuf,
    identity: LocalWaveformIdentity,
    timeline_duration: Option<Duration>,
    sample_rate_hz: Option<u32>,
    audio_channels: Option<u16>,
}

impl LocalWaveformRequest {
    /// Captures a request for the current regular file at `path`.
    ///
    /// # Errors
    ///
    /// Returns the identity error from [`LocalWaveformIdentity::from_path`].
    pub fn from_path(path: PathBuf) -> Result<Self, LocalWaveformError> {
        let identity = LocalWaveformIdentity::from_path(&path)?;
        Ok(Self {
            path,
            identity,
            timeline_duration: None,
            sample_rate_hz: None,
            audio_channels: None,
        })
    }

    /// Builds a request from an identity already captured by the caller.
    #[must_use]
    pub fn new(path: PathBuf, identity: LocalWaveformIdentity) -> Self {
        Self {
            path,
            identity,
            timeline_duration: None,
            sample_rate_hz: None,
            audio_channels: None,
        }
    }

    /// Aligns the generated envelope with a known whole-media timeline.
    ///
    /// The extractor inserts silence for delayed audio, timestamp gaps, and a
    /// short audio stream so waveform columns keep the player's time scale.
    #[must_use]
    pub fn with_timeline_duration(mut self, duration: Duration) -> Self {
        self.timeline_duration = (!duration.is_zero()).then_some(duration);
        self
    }

    /// Supplies the decoded audio sample rate used to avoid inevitable peak compactions.
    ///
    /// A zero rate is treated as unavailable, preserving the extractor's
    /// conservative fixed-size buckets.
    #[must_use]
    pub fn with_sample_rate_hz(mut self, sample_rate_hz: u32) -> Self {
        self.sample_rate_hz = (sample_rate_hz > 0).then_some(sample_rate_hz);
        self
    }

    /// Supplies the decoded channel count used by the binary PCM fast path.
    ///
    /// A zero count is treated as unavailable. The extractor then retains its
    /// metadata-based compatibility path for callers without probed audio
    /// shape, rather than guessing how interleaved samples form frames.
    #[must_use]
    pub fn with_audio_channels(mut self, audio_channels: u16) -> Self {
        self.audio_channels = (audio_channels > 0).then_some(audio_channels);
        self
    }

    /// Returns the exact local path passed to `FFmpeg` as one shell-free argument.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the identity required before and after extraction.
    #[must_use]
    pub const fn identity(&self) -> &LocalWaveformIdentity {
        &self.identity
    }

    /// Returns the media timeline that the waveform must fill, when known.
    #[must_use]
    pub const fn timeline_duration(&self) -> Option<Duration> {
        self.timeline_duration
    }

    /// Returns the decoded audio sample rate when metadata provided it.
    #[must_use]
    pub const fn sample_rate_hz(&self) -> Option<u32> {
        self.sample_rate_hz
    }

    /// Returns the decoded audio channel count when metadata provided it.
    #[must_use]
    pub const fn audio_channels(&self) -> Option<u16> {
        self.audio_channels
    }
}

/// Successfully generated identity-bound local waveform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalWaveformResult {
    identity: LocalWaveformIdentity,
    pyramid: PeakPyramid,
}

impl LocalWaveformResult {
    /// Binds a generated peak pyramid to the file identity that produced it.
    ///
    /// Custom extractors can use this constructor without exposing or
    /// depending on Youta's worker implementation.
    #[must_use]
    pub fn new(identity: LocalWaveformIdentity, pyramid: PeakPyramid) -> Self {
        Self { identity, pyramid }
    }

    /// Returns the filesystem identity that owns this waveform.
    #[must_use]
    pub const fn identity(&self) -> &LocalWaveformIdentity {
        &self.identity
    }

    /// Returns the bounded multiresolution peak envelope.
    #[must_use]
    pub const fn pyramid(&self) -> &PeakPyramid {
        &self.pyramid
    }

    /// Moves the bounded multiresolution peak envelope out of this result.
    #[must_use]
    pub fn into_pyramid(self) -> PeakPyramid {
        self.pyramid
    }
}

/// Cooperative cancellation shared with one active extraction.
#[derive(Clone, Debug, Default)]
pub struct WaveformCancellation {
    cancelled: Arc<AtomicBool>,
}

impl WaveformCancellation {
    /// Creates an uncancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests prompt termination of the active `FFmpeg` child.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Returns whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Resource limits for one local waveform extraction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalWaveformLimits {
    /// Source audio frames summarized by one emitted peak.
    pub frames_per_peak: usize,
    /// Finest peaks retained after progressive compaction, capped by
    /// [`MAXIMUM_PEAK_LIMIT`].
    pub maximum_peaks: usize,
    /// Wall-clock deadline for the `FFmpeg` child.
    pub timeout: Duration,
}

impl Default for LocalWaveformLimits {
    fn default() -> Self {
        Self {
            frames_per_peak: DEFAULT_FRAMES_PER_PEAK,
            maximum_peaks: DEFAULT_MAXIMUM_PEAKS,
            timeout: DEFAULT_EXTRACTION_TIMEOUT,
        }
    }
}

impl LocalWaveformLimits {
    fn validate(self) -> Result<Self, LocalWaveformConfigurationError> {
        if self.frames_per_peak == 0 {
            return Err(LocalWaveformConfigurationError::ZeroFramesPerPeak);
        }
        if self.maximum_peaks < 2 {
            return Err(LocalWaveformConfigurationError::PeakLimitTooSmall);
        }
        if self.maximum_peaks > MAXIMUM_PEAK_LIMIT {
            return Err(LocalWaveformConfigurationError::PeakLimitTooLarge);
        }
        if self.timeout.is_zero() {
            return Err(LocalWaveformConfigurationError::ZeroTimeout);
        }
        Ok(self)
    }
}

/// Invalid local-waveform extractor configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalWaveformConfigurationError {
    /// A statistics bucket cannot contain zero frames.
    ZeroFramesPerPeak,
    /// Progressive compaction requires room for at least two peaks.
    PeakLimitTooSmall,
    /// A larger allocation would violate the extractor's resource bound.
    PeakLimitTooLarge,
    /// A zero deadline would cancel every extraction before it starts.
    ZeroTimeout,
}

impl fmt::Display for LocalWaveformConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroFramesPerPeak => formatter.write_str("frames per peak must be positive"),
            Self::PeakLimitTooSmall => formatter.write_str("peak limit must be at least two"),
            Self::PeakLimitTooLarge => formatter.write_str("peak limit exceeds the fixed maximum"),
            Self::ZeroTimeout => formatter.write_str("extraction timeout must be positive"),
        }
    }
}

impl Error for LocalWaveformConfigurationError {}

/// Safe failure from one local waveform extraction.
#[derive(Debug)]
pub enum LocalWaveformError {
    /// The caller superseded or closed the active request.
    Cancelled,
    /// `FFmpeg` exceeded its configured wall-clock deadline.
    TimedOut,
    /// The selected path could not be inspected.
    FileUnavailable(io::Error),
    /// The selected path is not a regular file.
    NotRegularFile,
    /// The selected file changed before extraction completed.
    FileChanged,
    /// `FFmpeg` could not be started.
    SpawnFailed(io::Error),
    /// The bounded stdout reader could not be started.
    ReaderSpawnFailed(io::Error),
    /// `FFmpeg` process supervision failed.
    ProcessFailed(io::Error),
    /// `FFmpeg` rejected or could not decode the selected file.
    DecodeFailed,
    /// `FFmpeg` emitted malformed, incomplete, or overlong statistics.
    InvalidOutput,
    /// The bounded reader thread terminated unexpectedly.
    ReaderStopped,
}

impl fmt::Display for LocalWaveformError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("waveform generation was cancelled"),
            Self::TimedOut => formatter.write_str("waveform generation timed out"),
            Self::FileUnavailable(_) => formatter.write_str("the local file is unavailable"),
            Self::NotRegularFile => formatter.write_str("the local path is not a regular file"),
            Self::FileChanged => {
                formatter.write_str("the local file changed during waveform generation")
            }
            Self::SpawnFailed(_) => formatter.write_str("FFmpeg could not be started"),
            Self::ReaderSpawnFailed(_) => {
                formatter.write_str("the waveform reader could not be started")
            }
            Self::ProcessFailed(_) => formatter.write_str("FFmpeg process supervision failed"),
            Self::DecodeFailed => formatter.write_str("FFmpeg could not decode the local audio"),
            Self::InvalidOutput => formatter.write_str("FFmpeg returned invalid waveform data"),
            Self::ReaderStopped => formatter.write_str("the waveform reader stopped unexpectedly"),
        }
    }
}

impl Error for LocalWaveformError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::FileUnavailable(error)
            | Self::SpawnFailed(error)
            | Self::ReaderSpawnFailed(error)
            | Self::ProcessFailed(error) => Some(error),
            Self::Cancelled
            | Self::TimedOut
            | Self::NotRegularFile
            | Self::FileChanged
            | Self::DecodeFailed
            | Self::InvalidOutput
            | Self::ReaderStopped => None,
        }
    }
}

/// Mockable boundary for local waveform generation.
pub trait LocalWaveformExtractor: Send + Sync {
    /// Generates a bounded waveform or returns a safe failure.
    ///
    /// # Errors
    ///
    /// Returns [`LocalWaveformError`] when the request is cancelled, the file
    /// changes, decoding fails, output is invalid, or a resource limit is hit.
    fn extract(
        &self,
        request: &LocalWaveformRequest,
        cancellation: &WaveformCancellation,
    ) -> Result<LocalWaveformResult, LocalWaveformError>;
}

/// Production shell-free `FFmpeg` waveform extractor.
#[derive(Clone, Debug)]
pub struct FfmpegLocalWaveformExtractor {
    program: OsString,
    limits: LocalWaveformLimits,
    poll_interval: Duration,
}

impl Default for FfmpegLocalWaveformExtractor {
    fn default() -> Self {
        Self {
            program: OsString::from("ffmpeg"),
            limits: LocalWaveformLimits::default(),
            poll_interval: DEFAULT_PROCESS_POLL_INTERVAL,
        }
    }
}

impl FfmpegLocalWaveformExtractor {
    /// Uses the supplied `FFmpeg` executable with default resource limits.
    ///
    /// `program` should be a direct `FFmpeg`-compatible executable. A wrapper
    /// that gives its stdout pipe to descendants is outside the supervised
    /// process boundary; the deadline still returns, but the detached bounded
    /// reader can exit only after every inherited pipe handle is closed.
    #[must_use]
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            ..Self::default()
        }
    }

    /// Uses the supplied `FFmpeg` executable and validated resource limits.
    ///
    /// The executable has the same direct-process requirement as [`Self::new`].
    ///
    /// # Errors
    ///
    /// Returns [`LocalWaveformConfigurationError`] when a limit is zero or the
    /// peak limit cannot safely support progressive compaction.
    pub fn with_limits(
        program: impl Into<OsString>,
        limits: LocalWaveformLimits,
    ) -> Result<Self, LocalWaveformConfigurationError> {
        Ok(Self {
            program: program.into(),
            limits: limits.validate()?,
            poll_interval: DEFAULT_PROCESS_POLL_INTERVAL,
        })
    }

    /// Returns the configured `FFmpeg` executable.
    #[must_use]
    pub fn program(&self) -> &OsStr {
        &self.program
    }

    /// Returns the configured extraction limits.
    #[must_use]
    pub const fn limits(&self) -> LocalWaveformLimits {
        self.limits
    }

    /// Builds the compatibility command that emits bounded textual statistics.
    fn statistics_command(
        &self,
        path: &Path,
        timeline_duration: Option<Duration>,
        frames_per_peak: usize,
    ) -> Command {
        let timeline_filters = timeline_duration.map_or_else(String::new, |duration| {
            let duration = ffmpeg_duration(duration);
            format!(
                "aresample=async=1:min_hard_comp=0:first_pts=0,\
                 apad=whole_dur={duration},\
                 atrim=end={duration},"
            )
        });
        let filter = format!(
            "[0:a:0]{timeline_filters}aformat=sample_fmts=s16,\
             asetnsamples=n={}:p=0,\
             astats=metadata=1:reset=1:measure_perchannel=none:\
             measure_overall=Min_level+Max_level+Number_of_samples,\
             ametadata=mode=print:file=-[stats]",
            frames_per_peak
        );
        let mut command = Command::new(&self.program);
        command
            .arg("-nostdin")
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-threads")
            .arg("1")
            .arg("-i")
            .arg(path)
            .arg("-filter_complex")
            .arg(filter)
            .arg("-map")
            .arg("[stats]")
            .arg("-vn")
            .arg("-sn")
            .arg("-dn")
            .arg("-f")
            .arg("null")
            .arg("-")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // FFmpeg diagnostics can contain private paths and command
            // arguments, so this adapter never captures or surfaces them.
            .stderr(Stdio::null());
        command
    }

    /// Builds the fast command that streams normalized signed 16-bit PCM.
    ///
    /// Rust consumes this output incrementally and never retains decoded audio
    /// beyond one fixed read buffer. Keeping the same `s16` normalization as
    /// the compatibility command preserves its signed cross-channel extrema.
    fn pcm_command(&self, path: &Path, timeline_duration: Option<Duration>) -> Command {
        let timeline_filters = timeline_duration.map_or_else(String::new, |duration| {
            let duration = ffmpeg_duration(duration);
            format!(
                "aresample=async=1:min_hard_comp=0:first_pts=0,\
                 apad=whole_dur={duration},\
                 atrim=end={duration},"
            )
        });
        let filter = format!("[0:a:0]{timeline_filters}aformat=sample_fmts=s16[pcm]");
        let mut command = Command::new(&self.program);
        command
            .arg("-nostdin")
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-threads")
            .arg("1")
            .arg("-i")
            .arg(path)
            .arg("-filter_complex")
            .arg(filter)
            .arg("-map")
            .arg("[pcm]")
            .arg("-vn")
            .arg("-sn")
            .arg("-dn")
            .arg("-c:a")
            .arg("pcm_s16le")
            .arg("-f")
            .arg("s16le")
            .arg("-")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // FFmpeg diagnostics can contain private paths and command
            // arguments, so this adapter never captures or surfaces them.
            .stderr(Stdio::null());
        command
    }
}

/// Pre-applies only peak compactions guaranteed by the known media timeline.
///
/// The streaming builder merges aligned adjacent buckets by powers of two
/// after reaching its retained-peak limit. Starting FFmpeg at that inevitable
/// bucket size yields the same min/max envelope while avoiding metadata and
/// filter resets for intermediate peaks that would be discarded immediately.
fn adaptive_frames_per_peak(
    base_frames_per_peak: usize,
    maximum_peaks: usize,
    timeline_duration: Option<Duration>,
    sample_rate_hz: Option<u32>,
) -> usize {
    let Some(duration) = timeline_duration.filter(|duration| !duration.is_zero()) else {
        return base_frames_per_peak;
    };
    let Some(sample_rate_hz) = sample_rate_hz.filter(|sample_rate_hz| *sample_rate_hz > 0) else {
        return base_frames_per_peak;
    };
    let retained_peaks = maximum_peaks.saturating_sub(maximum_peaks % 2);
    if base_frames_per_peak == 0 || retained_peaks < 2 {
        return base_frames_per_peak;
    }

    // Match `ffmpeg_duration`: the normalized timeline is rounded upward to a
    // whole microsecond, then contains every audio frame before that endpoint.
    let sample_rate_hz = u128::from(sample_rate_hz);
    let duration_microseconds = duration.as_nanos().div_ceil(1_000);
    let normalized_frames = duration_microseconds
        .saturating_mul(sample_rate_hz)
        .div_ceil(1_000_000);
    let mut frames_per_peak = base_frames_per_peak;
    let mut retained_frame_capacity =
        (base_frames_per_peak as u128).saturating_mul(retained_peaks as u128);
    while normalized_frames > retained_frame_capacity {
        let Some(doubled) = frames_per_peak.checked_mul(2) else {
            return base_frames_per_peak;
        };
        frames_per_peak = doubled;
        retained_frame_capacity = retained_frame_capacity.saturating_mul(2);
    }
    frames_per_peak
}

fn ffmpeg_duration(duration: Duration) -> String {
    let total_microseconds = duration.as_nanos().div_ceil(1_000);
    format!(
        "{}.{:06}",
        total_microseconds / 1_000_000,
        total_microseconds % 1_000_000
    )
}

impl LocalWaveformExtractor for FfmpegLocalWaveformExtractor {
    fn extract(
        &self,
        request: &LocalWaveformRequest,
        cancellation: &WaveformCancellation,
    ) -> Result<LocalWaveformResult, LocalWaveformError> {
        if cancellation.is_cancelled() {
            return Err(LocalWaveformError::Cancelled);
        }
        ensure_identity(request)?;

        let frames_per_peak = adaptive_frames_per_peak(
            self.limits.frames_per_peak,
            self.limits.maximum_peaks,
            request.timeline_duration(),
            request.sample_rate_hz(),
        );
        let audio_channels = request.audio_channels().map(usize::from);
        let mut command = if audio_channels.is_some() {
            self.pcm_command(request.path(), request.timeline_duration())
        } else {
            self.statistics_command(request.path(), request.timeline_duration(), frames_per_peak)
        };
        let mut child = command.spawn().map_err(LocalWaveformError::SpawnFailed)?;
        let Some(stdout) = child.stdout.take() else {
            terminate_child(child);
            return Err(LocalWaveformError::InvalidOutput);
        };
        let maximum_peaks = self.limits.maximum_peaks;
        let (reader_result_sender, reader_result_receiver) = mpsc::sync_channel(1);
        let reader = match thread::Builder::new()
            .name("youta-local-waveform-reader".to_owned())
            .spawn(move || {
                let result = match audio_channels {
                    Some(audio_channels) => {
                        parse_s16le_pcm(stdout, audio_channels, frames_per_peak, maximum_peaks)
                            .map_err(|_| ())
                    }
                    None => parse_ffmpeg_statistics(
                        BufReader::new(stdout),
                        frames_per_peak,
                        maximum_peaks,
                    )
                    .map_err(|_| ()),
                };
                let _ = reader_result_sender.send(result);
            }) {
            Ok(reader) => reader,
            Err(error) => {
                terminate_child(child);
                return Err(LocalWaveformError::ReaderSpawnFailed(error));
            }
        };
        // A descendant can retain an inherited copy of stdout after the
        // supervised child exits. Detaching the reader and receiving its one
        // bounded result keeps cancellation and the deadline enforceable.
        drop(reader);

        let started = Instant::now();
        let status = loop {
            if cancellation.is_cancelled() {
                terminate_child(child);
                return Err(LocalWaveformError::Cancelled);
            }
            if started.elapsed() >= self.limits.timeout {
                terminate_child(child);
                return Err(LocalWaveformError::TimedOut);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => thread::sleep(
                    self.poll_interval
                        .min(self.limits.timeout.saturating_sub(started.elapsed())),
                ),
                Err(error) => {
                    terminate_child(child);
                    return Err(LocalWaveformError::ProcessFailed(error));
                }
            }
        };

        let parsed = loop {
            if cancellation.is_cancelled() {
                return Err(LocalWaveformError::Cancelled);
            }
            let remaining = self.limits.timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(LocalWaveformError::TimedOut);
            }
            match reader_result_receiver.recv_timeout(self.poll_interval.min(remaining)) {
                Ok(result) => break result,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(LocalWaveformError::ReaderStopped);
                }
            }
        };
        if cancellation.is_cancelled() {
            return Err(LocalWaveformError::Cancelled);
        }
        if started.elapsed() >= self.limits.timeout {
            return Err(LocalWaveformError::TimedOut);
        }
        if !status.success() {
            return Err(LocalWaveformError::DecodeFailed);
        }
        let pyramid = parsed.map_err(|_| LocalWaveformError::InvalidOutput)?;
        ensure_identity(request)?;
        Ok(LocalWaveformResult::new(request.identity.clone(), pyramid))
    }
}

fn ensure_identity(request: &LocalWaveformRequest) -> Result<(), LocalWaveformError> {
    match LocalWaveformIdentity::from_path(request.path()) {
        Ok(identity) if &identity == request.identity() => Ok(()),
        Ok(_) | Err(LocalWaveformError::FileUnavailable(_)) => Err(LocalWaveformError::FileChanged),
        Err(error) => Err(error),
    }
}

/// Kills one supervised child without allowing cleanup to defeat the deadline.
fn terminate_child(mut child: Child) {
    let _ = child.kill();
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) if started.elapsed() < CHILD_REAP_GRACE_PERIOD => {
                thread::sleep(DEFAULT_PROCESS_POLL_INTERVAL);
            }
            Ok(None) => {
                defer_child_reap(child);
                return;
            }
        }
    }
}

/// Hands an unusually slow child to one bounded, process-wide reaper.
///
/// The single-slot queue and non-blocking send prevent repeated failed
/// cleanups from creating unbounded threads or delaying waveform cancellation.
fn defer_child_reap(child: Child) {
    static REAPER: OnceLock<mpsc::SyncSender<Child>> = OnceLock::new();
    let reaper = REAPER.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel::<Child>(1);
        let _ = thread::Builder::new()
            .name("youta-local-waveform-reaper".to_owned())
            .spawn(move || {
                while let Ok(mut child) = receiver.recv() {
                    let _ = child.wait();
                }
            });
        sender
    });
    let _ = reaper.try_send(child);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PcmParseError {
    InvalidShape,
    ReadFailed,
    IncompleteSample,
    IncompleteFrame,
    Empty,
}

/// Incrementally groups interleaved samples into exact source-frame peaks.
struct PcmPeakAccumulator {
    minimum: i16,
    maximum: i16,
    samples_in_frame: usize,
    frames_in_peak: usize,
    peak_count: usize,
}

impl PcmPeakAccumulator {
    fn new() -> Self {
        Self {
            minimum: i16::MAX,
            maximum: i16::MIN,
            samples_in_frame: 0,
            frames_in_peak: 0,
            peak_count: 0,
        }
    }

    #[inline]
    fn push_sample(
        &mut self,
        sample: i16,
        audio_channels: usize,
        frames_per_peak: usize,
        builder: &mut PeakPyramidBuilder,
    ) {
        self.minimum = self.minimum.min(sample);
        self.maximum = self.maximum.max(sample);
        self.samples_in_frame += 1;
        if self.samples_in_frame != audio_channels {
            return;
        }
        self.samples_in_frame = 0;
        self.frames_in_peak += 1;
        if self.frames_in_peak == frames_per_peak {
            self.flush_peak(builder);
        }
    }

    fn flush_peak(&mut self, builder: &mut PeakPyramidBuilder) {
        if self.frames_in_peak == 0 {
            return;
        }
        builder.push(
            Peak {
                minimum: self.minimum,
                maximum: self.maximum,
            },
            self.frames_in_peak,
        );
        self.minimum = i16::MAX;
        self.maximum = i16::MIN;
        self.frames_in_peak = 0;
        self.peak_count = self.peak_count.saturating_add(1);
    }
}

/// Builds the bounded peak pyramid directly from interleaved signed PCM.
///
/// The fixed buffer and streaming builder keep memory independent of media
/// length. A peak counts source audio frames rather than individual channel
/// samples, matching FFmpeg's `asetnsamples`/`astats` compatibility path.
fn parse_s16le_pcm(
    mut reader: impl Read,
    audio_channels: usize,
    frames_per_peak: usize,
    maximum_peaks: usize,
) -> Result<PeakPyramid, PcmParseError> {
    if audio_channels == 0 {
        return Err(PcmParseError::InvalidShape);
    }
    let mut builder = PeakPyramidBuilder::new(frames_per_peak, maximum_peaks)
        .ok_or(PcmParseError::InvalidShape)?;
    let mut accumulator = PcmPeakAccumulator::new();
    let mut buffer = [0_u8; PCM_READ_BUFFER_BYTES];
    let mut pending_low_byte = None;

    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|_| PcmParseError::ReadFailed)?;
        if read == 0 {
            break;
        }
        let mut bytes = &buffer[..read];
        if let Some(low) = pending_low_byte.take() {
            let Some((&high, remainder)) = bytes.split_first() else {
                pending_low_byte = Some(low);
                continue;
            };
            accumulator.push_sample(
                i16::from_le_bytes([low, high]),
                audio_channels,
                frames_per_peak,
                &mut builder,
            );
            bytes = remainder;
        }
        let mut samples = bytes.chunks_exact(2);
        for sample in &mut samples {
            accumulator.push_sample(
                i16::from_le_bytes([sample[0], sample[1]]),
                audio_channels,
                frames_per_peak,
                &mut builder,
            );
        }
        pending_low_byte = samples.remainder().first().copied();
    }

    if pending_low_byte.is_some() {
        return Err(PcmParseError::IncompleteSample);
    }
    if accumulator.samples_in_frame != 0 {
        return Err(PcmParseError::IncompleteFrame);
    }
    accumulator.flush_peak(&mut builder);
    if accumulator.peak_count == 0 {
        return Err(PcmParseError::Empty);
    }
    Ok(builder.finish())
}

#[derive(Clone, Copy, Debug, Default)]
struct PendingStatistics {
    minimum: Option<i16>,
    maximum: Option<i16>,
    frames: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StatisticsParseError {
    InvalidLine,
    IncompleteFrame,
    DuplicateValue,
    Empty,
}

fn parse_ffmpeg_statistics(
    reader: impl BufRead,
    frames_per_peak: usize,
    maximum_peaks: usize,
) -> Result<PeakPyramid, StatisticsParseError> {
    let mut builder = PeakPyramidBuilder::new(frames_per_peak, maximum_peaks)
        .ok_or(StatisticsParseError::InvalidLine)?;
    let mut reader = reader;
    let mut line = Vec::with_capacity(128);
    let mut pending = PendingStatistics::default();
    let mut saw_frame = false;
    let mut peak_count = 0_usize;

    while read_bounded_line(&mut reader, &mut line, MAX_METADATA_LINE_BYTES)
        .map_err(|_| StatisticsParseError::InvalidLine)?
    {
        let line = std::str::from_utf8(strip_line_ending(&line))
            .map_err(|_| StatisticsParseError::InvalidLine)?;
        if line.starts_with("frame:") {
            if saw_frame {
                finish_statistics_frame(
                    &mut pending,
                    &mut builder,
                    &mut peak_count,
                    frames_per_peak,
                    false,
                )?;
            }
            saw_frame = true;
            continue;
        }
        if let Some(value) = line.strip_prefix("lavfi.astats.Overall.Min_level=") {
            if pending.minimum.replace(parse_sample(value)?).is_some() {
                return Err(StatisticsParseError::DuplicateValue);
            }
        } else if let Some(value) = line.strip_prefix("lavfi.astats.Overall.Max_level=") {
            if pending.maximum.replace(parse_sample(value)?).is_some() {
                return Err(StatisticsParseError::DuplicateValue);
            }
        } else if let Some(value) = line.strip_prefix("lavfi.astats.Overall.Number_of_samples=")
            && pending
                .frames
                .replace(parse_frame_count(value, frames_per_peak)?)
                .is_some()
        {
            return Err(StatisticsParseError::DuplicateValue);
        }
    }

    if saw_frame {
        finish_statistics_frame(
            &mut pending,
            &mut builder,
            &mut peak_count,
            frames_per_peak,
            true,
        )?;
    }
    if peak_count == 0 {
        return Err(StatisticsParseError::Empty);
    }
    Ok(builder.finish())
}

fn finish_statistics_frame(
    pending: &mut PendingStatistics,
    builder: &mut PeakPyramidBuilder,
    peak_count: &mut usize,
    frames_per_peak: usize,
    allow_partial: bool,
) -> Result<(), StatisticsParseError> {
    let PendingStatistics {
        minimum: Some(minimum),
        maximum: Some(maximum),
        frames: Some(frames),
    } = *pending
    else {
        return Err(StatisticsParseError::IncompleteFrame);
    };
    if minimum > maximum
        || frames > frames_per_peak
        || (!allow_partial && frames != frames_per_peak)
    {
        return Err(StatisticsParseError::InvalidLine);
    }
    builder.push(Peak { minimum, maximum }, frames);
    *peak_count = peak_count.saturating_add(1);
    *pending = PendingStatistics::default();
    Ok(())
}

fn parse_sample(value: &str) -> Result<i16, StatisticsParseError> {
    let value = parse_integral_decimal(value)?;
    i16::try_from(value).map_err(|_| StatisticsParseError::InvalidLine)
}

fn parse_frame_count(value: &str, frames_per_peak: usize) -> Result<usize, StatisticsParseError> {
    let value = usize::try_from(parse_integral_decimal(value)?)
        .map_err(|_| StatisticsParseError::InvalidLine)?;
    if value == 0 || value > frames_per_peak {
        return Err(StatisticsParseError::InvalidLine);
    }
    Ok(value)
}

fn parse_integral_decimal(value: &str) -> Result<i64, StatisticsParseError> {
    let (whole, fractional) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !fractional.bytes().all(|digit| digit == b'0')
        || fractional.contains('.')
    {
        return Err(StatisticsParseError::InvalidLine);
    }
    whole
        .parse::<i64>()
        .map_err(|_| StatisticsParseError::InvalidLine)
}

fn strip_line_ending(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Reads one line without ever growing `line` beyond `maximum_bytes`.
fn read_bounded_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    maximum_bytes: usize,
) -> io::Result<bool> {
    line.clear();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(!line.is_empty());
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(consumed) > maximum_bytes {
            reader.consume(consumed);
            if newline.is_none() {
                discard_through_newline(reader)?;
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "waveform metadata line exceeds the fixed limit",
            ));
        }
        line.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(true);
        }
    }
}

fn discard_through_newline(reader: &mut impl BufRead) -> io::Result<()> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(());
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    #[cfg(unix)]
    use std::sync::Mutex;

    #[cfg(unix)]
    static EXECUTABLE_HELPER_LOCK: Mutex<()> = Mutex::new(());

    fn statistics_frame(index: usize, minimum: i16, maximum: i16, frames: usize) -> String {
        format!(
            "frame:{index} pts:{index} pts_time:0\n\
             lavfi.astats.Overall.Min_level={minimum}.000000\n\
             lavfi.astats.Overall.Max_level={maximum}.000000\n\
             lavfi.astats.Overall.Number_of_samples={frames}.000000\n"
        )
    }

    /// Reader fixture forcing sample bytes to cross arbitrary read boundaries.
    struct ShortRead<R> {
        inner: R,
        maximum_bytes: usize,
    }

    impl<R: Read> Read for ShortRead<R> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let maximum = buffer.len().min(self.maximum_bytes);
            self.inner.read(&mut buffer[..maximum])
        }
    }

    fn s16le(samples: &[i16]) -> Vec<u8> {
        samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect()
    }

    #[test]
    fn pcm_parser_preserves_cross_channel_extrema_and_partial_final_peak() {
        let bytes = s16le(&[
            -10, 20, // stereo frame 0
            -30, 5, // stereo frame 1
            7, 40, // stereo frame 2
        ]);

        let pyramid = parse_s16le_pcm(Cursor::new(bytes), 2, 2, 4).expect("valid interleaved PCM");

        assert_eq!(pyramid.total_frames(), 3);
        assert_eq!(
            pyramid.levels()[0].peaks,
            [
                Peak {
                    minimum: -30,
                    maximum: 20,
                },
                Peak {
                    minimum: 7,
                    maximum: 40,
                },
            ]
        );
    }

    #[test]
    fn pcm_parser_handles_one_byte_reads_without_losing_sample_alignment() {
        let samples = [-32_768, 32_767, -7, 8, -300, 200, 11, 12];
        let bytes = s16le(&samples);
        let expected =
            parse_s16le_pcm(Cursor::new(bytes.clone()), 2, 2, 4).expect("contiguous PCM");
        let fragmented = parse_s16le_pcm(
            ShortRead {
                inner: Cursor::new(bytes),
                maximum_bytes: 1,
            },
            2,
            2,
            4,
        )
        .expect("fragmented PCM");

        assert_eq!(fragmented, expected);
    }

    #[test]
    fn pcm_parser_rejects_incomplete_samples_frames_and_empty_output() {
        assert_eq!(
            parse_s16le_pcm(Cursor::new([1_u8]), 2, 2, 4),
            Err(PcmParseError::IncompleteSample)
        );
        assert_eq!(
            parse_s16le_pcm(Cursor::new(s16le(&[1])), 2, 2, 4),
            Err(PcmParseError::IncompleteFrame)
        );
        assert_eq!(
            parse_s16le_pcm(Cursor::new(Vec::<u8>::new()), 2, 2, 4),
            Err(PcmParseError::Empty)
        );
        assert_eq!(
            parse_s16le_pcm(Cursor::new(s16le(&[1, 2])), 0, 2, 4),
            Err(PcmParseError::InvalidShape)
        );
    }

    #[test]
    fn parser_preserves_signed_cross_channel_extrema_and_exact_frames() {
        let output = format!(
            "{}{}",
            statistics_frame(0, -16_384, 8_192, 4_096),
            statistics_frame(1, -300, 700, 1_000)
        );

        let pyramid =
            parse_ffmpeg_statistics(Cursor::new(output), 4_096, 4_096).expect("valid output");

        assert_eq!(pyramid.total_frames(), 5_096);
        assert_eq!(
            pyramid.levels()[0].peaks,
            [
                Peak {
                    minimum: -16_384,
                    maximum: 8_192,
                },
                Peak {
                    minimum: -300,
                    maximum: 700,
                },
            ]
        );
    }

    #[test]
    fn parser_rejects_incomplete_and_overlong_metadata() {
        let incomplete = "frame:0 pts:0 pts_time:0\nlavfi.astats.Overall.Min_level=-1.000000\n";
        assert_eq!(
            parse_ffmpeg_statistics(Cursor::new(incomplete), 4_096, 4_096),
            Err(StatisticsParseError::IncompleteFrame)
        );

        let overlong = format!("{}\n", "x".repeat(MAX_METADATA_LINE_BYTES + 1));
        assert_eq!(
            parse_ffmpeg_statistics(Cursor::new(overlong), 4_096, 4_096),
            Err(StatisticsParseError::InvalidLine)
        );
    }

    #[test]
    fn parser_rejects_fractional_and_out_of_range_statistics() {
        let fractional_sample = statistics_frame(0, -1, 1, 4_096).replacen(
            "lavfi.astats.Overall.Min_level=-1.000000",
            "lavfi.astats.Overall.Min_level=-1.500000",
            1,
        );
        assert_eq!(
            parse_ffmpeg_statistics(Cursor::new(fractional_sample), 4_096, 4_096),
            Err(StatisticsParseError::InvalidLine)
        );

        let fractional_frames = statistics_frame(0, -1, 1, 4_096).replacen(
            "lavfi.astats.Overall.Number_of_samples=4096.000000",
            "lavfi.astats.Overall.Number_of_samples=4095.500000",
            1,
        );
        assert_eq!(
            parse_ffmpeg_statistics(Cursor::new(fractional_frames), 4_096, 4_096),
            Err(StatisticsParseError::InvalidLine)
        );

        assert_eq!(
            parse_sample("32768.000000"),
            Err(StatisticsParseError::InvalidLine)
        );
        assert_eq!(
            parse_frame_count("4097.000000", 4_096),
            Err(StatisticsParseError::InvalidLine)
        );

        let short_non_final = format!(
            "{}{}",
            statistics_frame(0, -1, 1, 1),
            statistics_frame(1, -2, 2, 4_096)
        );
        assert_eq!(
            parse_ffmpeg_statistics(Cursor::new(short_non_final), 4_096, 4_096),
            Err(StatisticsParseError::InvalidLine),
            "only the final FFmpeg statistics bucket may contain fewer source frames"
        );
    }

    #[test]
    fn parser_progressively_compacts_long_output() {
        let mut output = String::new();
        for index in 0..10 {
            output.push_str(&statistics_frame(index, -(index as i16), index as i16, 4));
        }

        let pyramid = parse_ffmpeg_statistics(Cursor::new(output), 4, 4).expect("valid output");

        assert_eq!(pyramid.total_frames(), 40);
        assert!(pyramid.levels()[0].peaks.len() <= 4);
        assert_eq!(pyramid.levels()[0].frames_per_peak, 16);
    }

    #[test]
    fn adaptive_buckets_skip_only_inevitable_power_of_two_compactions() {
        assert_eq!(
            adaptive_frames_per_peak(4, 4, Some(Duration::from_secs(16)), Some(1)),
            4,
            "an exactly full retained level must keep its base resolution"
        );
        assert_eq!(
            adaptive_frames_per_peak(4, 4, Some(Duration::from_secs(17)), Some(1)),
            8,
            "one frame beyond the retained level requires one compaction"
        );
        assert_eq!(
            adaptive_frames_per_peak(4, 4, Some(Duration::from_secs(32)), Some(1)),
            8
        );
        assert_eq!(
            adaptive_frames_per_peak(4, 4, Some(Duration::from_secs(33)), Some(1)),
            16
        );
        assert_eq!(
            adaptive_frames_per_peak(
                DEFAULT_FRAMES_PER_PEAK,
                DEFAULT_MAXIMUM_PEAKS,
                Some(Duration::from_secs(10 * 60)),
                Some(48_000),
            ),
            8_192
        );
        assert_eq!(
            adaptive_frames_per_peak(
                DEFAULT_FRAMES_PER_PEAK,
                DEFAULT_MAXIMUM_PEAKS,
                Some(Duration::from_secs(2 * 60 * 60)),
                Some(48_000),
            ),
            131_072
        );
        assert_eq!(
            adaptive_frames_per_peak(4, 5, Some(Duration::from_secs(17)), Some(1)),
            8,
            "the capacity must match the builder's even retained-peak limit"
        );
    }

    #[test]
    fn adaptive_buckets_fall_back_safely_without_complete_timing_metadata() {
        assert_eq!(
            adaptive_frames_per_peak(4_096, 4_096, None, Some(48_000)),
            4_096
        );
        assert_eq!(
            adaptive_frames_per_peak(4_096, 4_096, Some(Duration::from_secs(600)), None,),
            4_096
        );
        assert_eq!(
            adaptive_frames_per_peak(4_096, 4_096, Some(Duration::from_secs(600)), Some(0),),
            4_096
        );
        assert_eq!(
            adaptive_frames_per_peak(4, 4, Some(Duration::from_nanos(1)), Some(44_100)),
            4,
            "sub-microsecond timelines must follow FFmpeg's rounded endpoint"
        );
        let overflowing_base = usize::MAX / 2 + 1;
        assert_eq!(
            adaptive_frames_per_peak(
                overflowing_base,
                2,
                Some(Duration::new(u64::MAX, 999_999_999)),
                Some(u32::MAX),
            ),
            overflowing_base,
            "an unrepresentable adaptive bucket must retain the configured base"
        );
    }

    #[test]
    fn adaptive_buckets_produce_the_same_compacted_peak_pyramid() {
        for (bucket_count, final_frames) in [(4, 4), (5, 1), (8, 4), (9, 1), (17, 3)] {
            let buckets = (0..bucket_count)
                .map(|index| {
                    let magnitude = i16::try_from(index + 1).expect("bounded fixture magnitude");
                    let frames = if index + 1 == bucket_count {
                        final_frames
                    } else {
                        4
                    };
                    (
                        Peak {
                            minimum: -magnitude,
                            maximum: magnitude.saturating_mul(2),
                        },
                        frames,
                    )
                })
                .collect::<Vec<_>>();
            let total_frames = buckets.iter().map(|(_, frames)| *frames).sum::<usize>();
            let mut reference = PeakPyramidBuilder::new(4, 4).expect("reference builder");
            for (peak, frames) in &buckets {
                reference.push(*peak, *frames);
            }

            let adaptive_frames = adaptive_frames_per_peak(
                4,
                4,
                Some(Duration::from_secs(total_frames as u64)),
                Some(1),
            );
            let buckets_per_peak = adaptive_frames / 4;
            let mut optimized =
                PeakPyramidBuilder::new(adaptive_frames, 4).expect("adaptive builder");
            for group in buckets.chunks(buckets_per_peak) {
                let peak = group
                    .iter()
                    .map(|(peak, _)| *peak)
                    .reduce(|left, right| Peak {
                        minimum: left.minimum.min(right.minimum),
                        maximum: left.maximum.max(right.maximum),
                    })
                    .expect("non-empty adaptive group");
                let frames = group.iter().map(|(_, frames)| *frames).sum();
                optimized.push(peak, frames);
            }

            assert_eq!(
                optimized.finish(),
                reference.finish(),
                "precompaction must preserve {bucket_count} base buckets"
            );
        }
    }

    #[test]
    fn command_keeps_unusual_path_in_one_argument_and_requests_bounded_statistics() {
        let extractor = FfmpegLocalWaveformExtractor::default();
        let path = Path::new("music/$HOME; not a shell.flac");
        let command = extractor.statistics_command(path, None, DEFAULT_FRAMES_PER_PEAK);
        let arguments = command.get_args().collect::<Vec<_>>();

        assert_eq!(
            arguments
                .iter()
                .filter(|argument| **argument == path.as_os_str())
                .count(),
            1
        );
        let filter = arguments
            .iter()
            .filter_map(|argument| argument.to_str())
            .find(|argument| argument.contains("asetnsamples"))
            .expect("filter argument");
        assert!(filter.contains("asetnsamples=n=4096:p=0"));
        assert!(filter.contains("measure_perchannel=none"));
        assert!(filter.contains("Min_level+Max_level+Number_of_samples"));
        assert!(filter.contains("ametadata=mode=print:file=-"));
    }

    #[test]
    fn pcm_command_streams_normalized_binary_samples_without_statistics_filtering() {
        let extractor = FfmpegLocalWaveformExtractor::default();
        let path = Path::new("music/fast waveform.mp3");
        let command = extractor.pcm_command(path, Some(Duration::from_millis(1_500)));
        let arguments = command.get_args().collect::<Vec<_>>();
        let filter = arguments
            .iter()
            .filter_map(|argument| argument.to_str())
            .find(|argument| argument.contains("aformat"))
            .expect("PCM filter argument");

        assert_eq!(
            arguments
                .iter()
                .filter(|argument| **argument == path.as_os_str())
                .count(),
            1
        );
        assert!(filter.contains("aformat=sample_fmts=s16"));
        assert!(filter.contains("apad=whole_dur=1.500000"));
        assert!(!filter.contains("astats"));
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == [OsStr::new("-f"), OsStr::new("s16le")])
        );
    }

    #[test]
    fn command_normalizes_delayed_and_short_audio_to_the_media_timeline() {
        let extractor = FfmpegLocalWaveformExtractor::default();
        let command = extractor.statistics_command(
            Path::new("delayed-audio.mp4"),
            Some(Duration::new(10, 250_000_000)),
            DEFAULT_FRAMES_PER_PEAK,
        );
        let filter = command
            .get_args()
            .filter_map(|argument| argument.to_str())
            .find(|argument| argument.contains("asetnsamples"))
            .expect("filter argument");

        assert!(filter.contains("aresample=async=1:min_hard_comp=0:first_pts=0"));
        assert!(filter.contains("apad=whole_dur=10.250000"));
        assert!(filter.contains("atrim=end=10.250000"));
        assert!(
            filter.find("aresample").expect("timestamp normalization")
                < filter.find("asetnsamples").expect("peak buckets")
        );

        let sub_microsecond = extractor.statistics_command(
            Path::new("short-audio.wav"),
            Some(Duration::from_nanos(1)),
            DEFAULT_FRAMES_PER_PEAK,
        );
        let filter = sub_microsecond
            .get_args()
            .filter_map(|argument| argument.to_str())
            .find(|argument| argument.contains("asetnsamples"))
            .expect("sub-microsecond filter argument");
        assert!(filter.contains("apad=whole_dur=0.000001"));
        assert!(filter.contains("atrim=end=0.000001"));
    }

    #[test]
    fn command_uses_the_adaptive_bucket_selected_for_a_long_timeline() {
        let extractor = FfmpegLocalWaveformExtractor::default();
        let duration = Duration::from_secs(10 * 60);
        let frames_per_peak = adaptive_frames_per_peak(
            extractor.limits.frames_per_peak,
            extractor.limits.maximum_peaks,
            Some(duration),
            Some(48_000),
        );
        let command =
            extractor.statistics_command(Path::new("long.flac"), Some(duration), frames_per_peak);
        let filter = command
            .get_args()
            .filter_map(|argument| argument.to_str())
            .find(|argument| argument.contains("asetnsamples"))
            .expect("adaptive filter argument");

        assert!(filter.contains("asetnsamples=n=8192:p=0"));
    }

    #[test]
    fn configuration_rejects_an_unbounded_peak_allocation() {
        let result = FfmpegLocalWaveformExtractor::with_limits(
            "ffmpeg",
            LocalWaveformLimits {
                maximum_peaks: usize::MAX,
                ..LocalWaveformLimits::default()
            },
        );

        assert!(matches!(
            result,
            Err(LocalWaveformConfigurationError::PeakLimitTooLarge)
        ));
    }

    #[test]
    fn cancelled_request_does_not_spawn_missing_helper() {
        let file = tempfile::NamedTempFile::new().expect("temporary media");
        let request =
            LocalWaveformRequest::from_path(file.path().to_owned()).expect("regular file");
        let cancellation = WaveformCancellation::new();
        cancellation.cancel();
        let extractor = FfmpegLocalWaveformExtractor::new("missing-youta-test-ffmpeg");

        assert!(matches!(
            extractor.extract(&request, &cancellation),
            Err(LocalWaveformError::Cancelled)
        ));
    }

    #[test]
    fn identity_rejects_same_path_replacement() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("audio.bin");
        fs::write(&path, b"first").expect("initial file");
        let request = LocalWaveformRequest::from_path(path.clone()).expect("regular file");
        fs::remove_file(&path).expect("remove initial file");
        fs::write(&path, b"other").expect("replacement file");

        assert!(matches!(
            ensure_identity(&request),
            Err(LocalWaveformError::FileChanged)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn shell_free_mock_process_produces_a_waveform() {
        use std::os::unix::fs::PermissionsExt;

        let _helper_guard = EXECUTABLE_HELPER_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let directory = tempfile::tempdir().expect("temporary directory");
        let media = directory.path().join("audio with spaces.bin");
        fs::write(&media, b"unchanged media").expect("media fixture");
        let helper = directory.path().join("mock ffmpeg");
        let output = statistics_frame(0, -123, 456, 512);
        fs::write(
            &helper,
            format!(
                "#!/bin/sh\nprintf '%s' '{}'\n",
                output.replace('\'', "'\\''")
            ),
        )
        .expect("mock helper");
        let mut permissions = fs::metadata(&helper)
            .expect("helper metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&helper, permissions).expect("executable helper");

        let request = LocalWaveformRequest::from_path(media).expect("regular media");
        let extractor = FfmpegLocalWaveformExtractor::new(helper.into_os_string());
        let result = extractor
            .extract(&request, &WaveformCancellation::new())
            .expect("mock extraction");

        assert_eq!(result.pyramid().total_frames(), 512);
        assert_eq!(
            result.pyramid().levels()[0].peaks,
            [Peak {
                minimum: -123,
                maximum: 456,
            }]
        );
    }

    #[cfg(unix)]
    #[test]
    fn shell_free_mock_process_uses_binary_pcm_when_channels_are_known() {
        use std::os::unix::fs::PermissionsExt;

        let _helper_guard = EXECUTABLE_HELPER_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let directory = tempfile::tempdir().expect("temporary directory");
        let media = directory.path().join("known-shape audio.bin");
        fs::write(&media, b"unchanged media").expect("media fixture");
        let helper = directory.path().join("mock PCM ffmpeg");
        fs::write(&helper, "#!/bin/sh\nprintf '\\205\\377\\310\\001'\n").expect("mock PCM helper");
        let mut permissions = fs::metadata(&helper)
            .expect("helper metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&helper, permissions).expect("executable helper");

        let request = LocalWaveformRequest::from_path(media)
            .expect("regular media")
            .with_audio_channels(2);
        let extractor = FfmpegLocalWaveformExtractor::new(helper.into_os_string());
        let result = extractor
            .extract(&request, &WaveformCancellation::new())
            .expect("mock PCM extraction");

        assert_eq!(result.pyramid().total_frames(), 1);
        assert_eq!(
            result.pyramid().levels()[0].peaks,
            [Peak {
                minimum: -123,
                maximum: 456,
            }]
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_decoder_is_reported_before_its_empty_statistics() {
        use std::os::unix::fs::PermissionsExt;

        let _helper_guard = EXECUTABLE_HELPER_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let directory = tempfile::tempdir().expect("temporary directory");
        let media = directory.path().join("unsupported media.bin");
        fs::write(&media, b"unsupported media").expect("media fixture");
        let helper = directory.path().join("failing ffmpeg");
        fs::write(&helper, "#!/bin/sh\nexit 1\n").expect("mock helper");
        let mut permissions = fs::metadata(&helper)
            .expect("helper metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&helper, permissions).expect("executable helper");

        let request = LocalWaveformRequest::from_path(media).expect("regular media");
        let extractor = FfmpegLocalWaveformExtractor::new(helper.into_os_string());

        assert!(matches!(
            extractor.extract(&request, &WaveformCancellation::new()),
            Err(LocalWaveformError::DecodeFailed)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn exited_child_cannot_extend_deadline_via_inherited_stdout() {
        use std::os::unix::fs::PermissionsExt;

        let _helper_guard = EXECUTABLE_HELPER_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let directory = tempfile::tempdir().expect("temporary directory");
        let media = directory.path().join("local media.bin");
        fs::write(&media, b"unchanged media").expect("media fixture");
        let helper = directory.path().join("wrapper ffmpeg");
        let output = statistics_frame(0, -123, 456, 512);
        fs::write(
            &helper,
            format!(
                "#!/bin/sh\nsleep 2 &\nprintf '%s' '{}'\n",
                output.replace('\'', "'\\''")
            ),
        )
        .expect("mock helper");
        let mut permissions = fs::metadata(&helper)
            .expect("helper metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&helper, permissions).expect("executable helper");

        let request = LocalWaveformRequest::from_path(media).expect("regular media");
        let mut extractor = FfmpegLocalWaveformExtractor::with_limits(
            helper.into_os_string(),
            LocalWaveformLimits {
                timeout: Duration::from_millis(250),
                ..LocalWaveformLimits::default()
            },
        )
        .expect("valid limits");
        extractor.poll_interval = Duration::from_millis(5);
        let started = Instant::now();

        assert!(matches!(
            extractor.extract(&request, &WaveformCancellation::new()),
            Err(LocalWaveformError::TimedOut)
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_kills_and_reaps_the_decoder_without_blocking() {
        use std::os::unix::fs::PermissionsExt;

        let _helper_guard = EXECUTABLE_HELPER_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let directory = tempfile::tempdir().expect("temporary directory");
        let media = directory.path().join("local media.bin");
        fs::write(&media, b"unchanged media").expect("media fixture");
        let helper = directory.path().join("sleeping ffmpeg");
        fs::write(&helper, "#!/bin/sh\nexec sleep 5\n").expect("mock helper");
        let mut permissions = fs::metadata(&helper)
            .expect("helper metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&helper, permissions).expect("executable helper");

        let request = LocalWaveformRequest::from_path(media).expect("regular media");
        let mut extractor = FfmpegLocalWaveformExtractor::new(helper.into_os_string());
        extractor.poll_interval = Duration::from_millis(5);
        let cancellation = WaveformCancellation::new();
        let worker_cancellation = cancellation.clone();
        let worker = thread::spawn(move || extractor.extract(&request, &worker_cancellation));
        thread::sleep(Duration::from_millis(50));
        let started = Instant::now();

        cancellation.cancel();
        let result = worker.join().expect("waveform extraction worker");

        assert!(matches!(result, Err(LocalWaveformError::Cancelled)));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "child cleanup must not defeat prompt cancellation"
        );
    }

    #[test]
    fn extractor_trait_is_mockable_without_a_process() {
        struct MockExtractor;

        impl LocalWaveformExtractor for MockExtractor {
            fn extract(
                &self,
                request: &LocalWaveformRequest,
                _cancellation: &WaveformCancellation,
            ) -> Result<LocalWaveformResult, LocalWaveformError> {
                Ok(LocalWaveformResult::new(
                    request.identity.clone(),
                    PeakPyramid::from_peaks(
                        vec![Peak {
                            minimum: -1,
                            maximum: 1,
                        }],
                        1,
                        1,
                    ),
                ))
            }
        }

        let file = tempfile::NamedTempFile::new().expect("temporary media");
        let request =
            LocalWaveformRequest::from_path(file.path().to_owned()).expect("regular file");
        let extractor: &dyn LocalWaveformExtractor = &MockExtractor;

        assert_eq!(
            extractor
                .extract(&request, &WaveformCancellation::new())
                .expect("mock result")
                .pyramid()
                .total_frames(),
            1
        );
    }
}
