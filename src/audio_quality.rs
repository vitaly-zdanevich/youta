//! Bounded local-audio target discovery and spectral quality analysis.
//!
//! A container and its advertised bitrate say how the current file is stored;
//! they cannot establish what existed before it. This module therefore does
//! not claim to recover an "original bitrate". It asks the configured `FFmpeg`
//! helper for a short, normalized PCM stream and looks for a repeatable sharp
//! high-frequency cutoff. The result is evidence: confidence, measured
//! bandwidth, and explicit inconclusive states. It deliberately does not map
//! a cutoff to a codec-neutral bitrate because encoder behaviour varies.
//!
//! Decoded audio is processed in overlapping FFT windows and discarded as it
//! arrives. The request is bound to a replacement-sensitive filesystem
//! identity, while cancellation, a wall-clock deadline, and a decoded-duration
//! ceiling bound each helper invocation. A separate deterministic collector
//! expands explicitly selected files and folders without following symbolic
//! links or returning partial results after a limit or inspection failure.

use rustfft::Fft;
use rustfft::FftPlanner;
use rustfft::num_complex::Complex;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// Sample rate requested from `FFmpeg` for every analysis.
pub const ANALYSIS_SAMPLE_RATE_HZ: u32 = 48_000;
/// Maximum audio inspected by the default analyzer.
pub const DEFAULT_MAXIMUM_AUDIO_DURATION: Duration = Duration::from_secs(30);
/// Whole-operation deadline for the default analyzer.
pub const DEFAULT_ANALYSIS_TIMEOUT: Duration = Duration::from_secs(45);
/// Default number of filesystem entries one target collection may inspect.
pub const DEFAULT_MAXIMUM_INSPECTED_TARGET_ENTRIES: usize = 10_000;
/// Default recursion depth below each explicitly selected folder.
pub const DEFAULT_MAXIMUM_TARGET_DEPTH: usize = 32;
/// Default number of audio files accepted by one target collection.
pub const DEFAULT_MAXIMUM_AUDIO_TARGETS: usize = 256;

const MAXIMUM_AUDIO_DURATION: Duration = Duration::from_mins(2);
const FFT_SIZE: usize = 8_192;
const FFT_SIZE_F32: f32 = 8_192.0;
const FFT_LAST_INDEX_F32: f32 = 8_191.0;
const ANALYSIS_SAMPLE_RATE_F32: f32 = 48_000.0;
const FFT_HOP: usize = FFT_SIZE / 2;
const PCM_READ_BUFFER_BYTES: usize = 64 * 1_024;
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(20);
const MINIMUM_ACTIVE_WINDOWS: usize = 3;
const MINIMUM_WINDOW_RMS: f32 = 0.000_1;
const MINIMUM_CLIFF_DB: f32 = 18.0;
const MINIMUM_SUSTAINED_ATTENUATION_DB: f32 = 18.0;
const MINIMUM_POWER_RATIO: f32 = 1.0e-12;
const MAXIMUM_POWER_RATIO: f32 = 1.0e12;
const MINIMUM_CUTOFF_HZ: f32 = 8_000.0;
const MAXIMUM_CUTOFF_HZ: f32 = 21_500.0;
const SUSTAINED_ATTENUATION_OFFSET_HZ: f32 = 500.0;
const SUSTAINED_ATTENUATION_END_HZ: f32 = 23_500.0;
const CUTOFF_AGREEMENT_HZ: f32 = 500.0;
const CUTOFF_AGREEMENT_HZ_INTEGER: u32 = 500;
const SAMPLE_RATE_NYQUIST_TOLERANCE_HZ: u32 = 750;
const MINIMUM_CUTOFF_AGREEMENT_PERCENT: u8 = 70;

/// Replacement-sensitive identity of one regular local audio file.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AudioQualityIdentity {
    length: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    filesystem: Option<crate::file_identity::FilesystemIdentity>,
}

impl AudioQualityIdentity {
    /// Captures the current identity of one regular file.
    ///
    /// # Errors
    ///
    /// Returns [`AudioQualityError::FileUnavailable`] when metadata cannot be
    /// read, or [`AudioQualityError::NotRegularFile`] for a non-file path.
    pub fn from_path(path: &Path) -> Result<Self, AudioQualityError> {
        let metadata = fs::symlink_metadata(path).map_err(AudioQualityError::FileUnavailable)?;
        if !metadata.is_file() {
            return Err(AudioQualityError::NotRegularFile);
        }
        Ok(Self {
            length: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            filesystem: crate::file_identity::filesystem_identity(path, &metadata),
        })
    }

    /// Returns the byte length recorded when this identity was captured.
    #[must_use]
    pub const fn length(&self) -> u64 {
        self.length
    }
}

/// Factual encoding information already observed in the selected file.
///
/// It is kept separate from the spectral inference so the report never
/// presents a container label or nominal bitrate as proof of source quality.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum DeclaredEncoding {
    /// The caller has no trustworthy codec classification.
    #[default]
    Unknown,
    /// The current stream uses a lossless codec.
    Lossless,
    /// The current stream uses a lossy codec and may advertise a bitrate.
    Lossy {
        /// Nominal average bitrate from metadata, when known.
        bitrate_kbps: Option<u32>,
    },
}

impl fmt::Display for DeclaredEncoding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown => formatter.write_str("unknown encoding"),
            Self::Lossless => formatter.write_str("lossless encoding"),
            Self::Lossy {
                bitrate_kbps: Some(bitrate),
            } => write!(formatter, "lossy encoding at {bitrate} kbps"),
            Self::Lossy { bitrate_kbps: None } => formatter.write_str("lossy encoding"),
        }
    }
}

/// One identity-bound quality-analysis request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioQualityRequest {
    path: PathBuf,
    identity: AudioQualityIdentity,
    declared_encoding: DeclaredEncoding,
    source_sample_rate_hz: Option<u32>,
    source_channels: Option<u8>,
}

impl AudioQualityRequest {
    /// Captures a request for the regular file currently at `path`.
    ///
    /// # Errors
    ///
    /// Returns the identity error from [`AudioQualityIdentity::from_path`].
    pub fn from_path(path: PathBuf) -> Result<Self, AudioQualityError> {
        let identity = AudioQualityIdentity::from_path(&path)?;
        Ok(Self::new(path, identity))
    }

    /// Builds a request from an identity already captured by the caller.
    #[must_use]
    pub const fn new(path: PathBuf, identity: AudioQualityIdentity) -> Self {
        Self {
            path,
            identity,
            declared_encoding: DeclaredEncoding::Unknown,
            source_sample_rate_hz: None,
            source_channels: None,
        }
    }

    /// Adds the current file's factual codec/bitrate classification.
    #[must_use]
    pub const fn with_declared_encoding(mut self, encoding: DeclaredEncoding) -> Self {
        self.declared_encoding = encoding;
        self
    }

    /// Adds the current stream's factual sample rate from file metadata.
    ///
    /// A zero rate is treated as unavailable. Supplying the rate lets the
    /// analyzer distinguish a codec cutoff from the current stream's own
    /// Nyquist limit after `FFmpeg` normalizes PCM to 48 kHz.
    #[must_use]
    pub const fn with_source_sample_rate_hz(mut self, sample_rate_hz: u32) -> Self {
        self.source_sample_rate_hz = if sample_rate_hz == 0 {
            None
        } else {
            Some(sample_rate_hz)
        };
        self
    }

    /// Adds the current stream's factual channel count from file metadata.
    ///
    /// A zero count is treated as unavailable. The analyzer normalizes helper
    /// output to stereo, so this value prevents unsafe cutoff interpretation
    /// when a multichannel input may have lost spectrum during downmixing.
    #[must_use]
    pub const fn with_source_channels(mut self, channels: u8) -> Self {
        self.source_channels = if channels == 0 { None } else { Some(channels) };
        self
    }

    /// Returns the local path passed to `FFmpeg` as one shell-free argument.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the identity that must still match after analysis.
    #[must_use]
    pub const fn identity(&self) -> &AudioQualityIdentity {
        &self.identity
    }

    /// Returns the current stream's independently observed encoding.
    #[must_use]
    pub const fn declared_encoding(&self) -> DeclaredEncoding {
        self.declared_encoding
    }

    /// Returns the current stream's independently observed sample rate.
    #[must_use]
    pub const fn source_sample_rate_hz(&self) -> Option<u32> {
        self.source_sample_rate_hz
    }

    /// Returns the current stream's independently observed channel count.
    #[must_use]
    pub const fn source_channels(&self) -> Option<u8> {
        self.source_channels
    }
}

/// Meaning of the spectral evidence in an analysis report.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AudioQualityAssessment {
    /// A stable cutoff was found without asserting unknowable codec ancestry.
    BandLimited,
    /// The cutoff agrees with the current stream's Nyquist limit.
    SampleRateLimited,
    /// A cutoff was measured, but the current stream's sample rate is unknown.
    SampleRateUnavailable,
    /// A cutoff was measured, but the original channel count is unknown.
    ChannelCountUnavailable,
    /// A multichannel stream was normalized to stereo before spectral analysis.
    MultichannelDownmix,
    /// Broad high-frequency content was present and no stable sharp cutoff was found.
    NoSuspiciousSignature,
    /// Audio was present, but it could not support a reliable conclusion.
    Inconclusive,
    /// Too few non-silent FFT windows were available.
    InsufficientSignal,
}

impl fmt::Display for AudioQualityAssessment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BandLimited => "band-limited audio",
            Self::SampleRateLimited => "limited by current sample rate",
            Self::SampleRateUnavailable => "sample rate unavailable",
            Self::ChannelCountUnavailable => "channel count unavailable",
            Self::MultichannelDownmix => "multichannel downmix is inconclusive",
            Self::NoSuspiciousSignature => "no suspicious spectral cutoff detected",
            Self::Inconclusive => "inconclusive",
            Self::InsufficientSignal => "insufficient signal",
        })
    }
}

/// Strength of the repeatable spectral evidence, not certainty about history.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AudioQualityConfidence {
    /// Evidence is sparse or only excludes a strong cutoff.
    Low,
    /// Multiple windows support the reported spectral verdict.
    Medium,
    /// Nearly all windows agree on a steep qualifying signature.
    High,
}

impl fmt::Display for AudioQualityConfidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        })
    }
}

/// Completed, identity-bound spectral analysis.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioQualityReport {
    identity: AudioQualityIdentity,
    declared_encoding: DeclaredEncoding,
    source_sample_rate_hz: Option<u32>,
    source_channels: Option<u8>,
    assessment: AudioQualityAssessment,
    effective_bandwidth_hz: Option<u32>,
    confidence: AudioQualityConfidence,
    cliff_db: Option<f32>,
    window_agreement_percent: u8,
    active_windows: u32,
    analyzed_frames: u64,
}

impl AudioQualityReport {
    /// Creates a report without spectral measurements.
    ///
    /// This constructor is primarily useful to front-end adapters and mock
    /// analyzers. Production analysis adds measurements through
    /// [`Self::with_spectral_evidence`].
    #[must_use]
    pub const fn new(
        identity: AudioQualityIdentity,
        declared_encoding: DeclaredEncoding,
        assessment: AudioQualityAssessment,
        confidence: AudioQualityConfidence,
    ) -> Self {
        Self {
            identity,
            declared_encoding,
            source_sample_rate_hz: None,
            source_channels: None,
            assessment,
            effective_bandwidth_hz: None,
            confidence,
            cliff_db: None,
            window_agreement_percent: 0,
            active_windows: 0,
            analyzed_frames: 0,
        }
    }

    /// Adds bounded spectral measurements to a report.
    ///
    /// Values are retained as evidence rather than converted into a stronger
    /// verdict. The analyzer remains responsible for choosing the assessment.
    #[must_use]
    pub const fn with_spectral_evidence(
        mut self,
        effective_bandwidth_hz: Option<u32>,
        cliff_db: Option<f32>,
        window_agreement_percent: u8,
        active_windows: u32,
        analyzed_frames: u64,
    ) -> Self {
        self.effective_bandwidth_hz = effective_bandwidth_hz;
        self.cliff_db = cliff_db;
        self.window_agreement_percent = if window_agreement_percent > 100 {
            100
        } else {
            window_agreement_percent
        };
        self.active_windows = active_windows;
        self.analyzed_frames = analyzed_frames;
        self
    }

    /// Adds the current stream's independently observed sample rate.
    #[must_use]
    pub const fn with_source_sample_rate_hz(mut self, sample_rate_hz: u32) -> Self {
        self.source_sample_rate_hz = if sample_rate_hz == 0 {
            None
        } else {
            Some(sample_rate_hz)
        };
        self
    }

    /// Adds the current stream's independently observed channel count.
    #[must_use]
    pub const fn with_source_channels(mut self, channels: u8) -> Self {
        self.source_channels = if channels == 0 { None } else { Some(channels) };
        self
    }

    /// Returns the file identity that owns this report.
    #[must_use]
    pub const fn identity(&self) -> &AudioQualityIdentity {
        &self.identity
    }

    /// Returns the current file encoding supplied independently by metadata.
    #[must_use]
    pub const fn declared_encoding(&self) -> DeclaredEncoding {
        self.declared_encoding
    }

    /// Returns the current stream's factual sample rate, when supplied.
    #[must_use]
    pub const fn source_sample_rate_hz(&self) -> Option<u32> {
        self.source_sample_rate_hz
    }

    /// Returns the current stream's factual channel count, when supplied.
    #[must_use]
    pub const fn source_channels(&self) -> Option<u8> {
        self.source_channels
    }

    /// Returns the evidence-based verdict.
    #[must_use]
    pub const fn assessment(&self) -> AudioQualityAssessment {
        self.assessment
    }

    /// Returns the repeatable cutoff, rounded to hertz, when one was found.
    #[must_use]
    pub const fn effective_bandwidth_hz(&self) -> Option<u32> {
        self.effective_bandwidth_hz
    }

    /// Returns confidence in the spectral verdict, not historical certainty.
    #[must_use]
    pub const fn confidence(&self) -> AudioQualityConfidence {
        self.confidence
    }

    /// Returns the median qualifying local spectral drop in decibels.
    #[must_use]
    pub const fn cliff_db(&self) -> Option<f32> {
        self.cliff_db
    }

    /// Returns the percentage of active windows agreeing on the cutoff.
    #[must_use]
    pub const fn window_agreement_percent(&self) -> u8 {
        self.window_agreement_percent
    }

    /// Returns the number of non-silent windows used for the verdict.
    #[must_use]
    pub const fn active_windows(&self) -> u32 {
        self.active_windows
    }

    /// Returns normalized stereo PCM frames consumed from `FFmpeg`.
    #[must_use]
    pub const fn analyzed_frames(&self) -> u64 {
        self.analyzed_frames
    }

    /// Returns cautious user-facing interpretation of the verdict.
    ///
    /// The wording depends on the independently declared encoding and never
    /// promotes absence of one signature into proof of a lossless source.
    #[must_use]
    pub const fn interpretation(&self) -> &'static str {
        match (self.assessment, self.declared_encoding) {
            (AudioQualityAssessment::BandLimited, DeclaredEncoding::Lossless) => {
                "possible lossy ancestry or a deliberately band-limited master; neither is proven"
            }
            (AudioQualityAssessment::BandLimited, DeclaredEncoding::Lossy { .. }) => {
                "the current lossy encode, an earlier source, or an intentional low-pass may explain this bandwidth; provenance is not proven"
            }
            (AudioQualityAssessment::BandLimited, _) => {
                "repeatable bandwidth limit; this alone does not prove an earlier encode"
            }
            (AudioQualityAssessment::SampleRateLimited, _) => {
                "bandwidth matches the current stream's sample-rate limit; no earlier lossy source is inferred"
            }
            (AudioQualityAssessment::SampleRateUnavailable, _) => {
                "the current sample rate is unavailable, so the measured cutoff cannot be interpreted safely"
            }
            (AudioQualityAssessment::ChannelCountUnavailable, _) => {
                "the current channel count is unavailable, so possible multichannel downmixing prevents a safe cutoff interpretation"
            }
            (AudioQualityAssessment::MultichannelDownmix, _) => {
                "the multichannel stream was normalized to stereo, so downmixing may have removed spectrum and the cutoff cannot be interpreted safely"
            }
            (AudioQualityAssessment::NoSuspiciousSignature, _) => {
                "no suspicious spectral cutoff detected; this does not prove a lossless source"
            }
            (AudioQualityAssessment::Inconclusive, _) => {
                "the available spectrum does not support a reliable conclusion"
            }
            (AudioQualityAssessment::InsufficientSignal, _) => {
                "not enough non-silent audio was available for analysis"
            }
        }
    }
}

/// Cooperative cancellation shared with one active analysis.
#[derive(Clone, Debug, Default)]
pub struct AudioQualityCancellation {
    cancelled: Arc<AtomicBool>,
}

impl AudioQualityCancellation {
    /// Creates an unset cancellation signal.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests prompt termination of the active `FFmpeg` helper.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Returns whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Hard bounds for expanding selected audio files and folders.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioQualityTargetLimits {
    /// Maximum unique roots and child directory entries inspected.
    pub maximum_inspected_entries: usize,
    /// Maximum directory depth below an explicitly selected root.
    pub maximum_depth: usize,
    /// Maximum unique audio files returned.
    pub maximum_audio_files: usize,
}

impl Default for AudioQualityTargetLimits {
    fn default() -> Self {
        Self {
            maximum_inspected_entries: DEFAULT_MAXIMUM_INSPECTED_TARGET_ENTRIES,
            maximum_depth: DEFAULT_MAXIMUM_TARGET_DEPTH,
            maximum_audio_files: DEFAULT_MAXIMUM_AUDIO_TARGETS,
        }
    }
}

/// Failure while expanding explicitly selected analysis targets.
#[derive(Debug, thiserror::Error)]
pub enum AudioQualityTargetCollectionError {
    /// A zero limit would make a complete traversal impossible to distinguish.
    #[error("audio quality target limits must be greater than zero")]
    InvalidLimits,
    /// The caller superseded or closed the collection request.
    #[error("audio quality target collection was cancelled")]
    Cancelled,
    /// A root or child entry could not be inspected completely.
    #[error("cannot inspect audio quality target `{path}`: {source}")]
    Inspect {
        /// Path whose metadata or directory stream failed.
        path: PathBuf,
        /// Underlying operating-system failure.
        #[source]
        source: io::Error,
    },
    /// Traversal would need to inspect more entries than configured.
    #[error("audio quality target collection exceeds its {maximum}-entry limit")]
    InspectedEntryLimitReached {
        /// Configured maximum number of inspected entries.
        maximum: usize,
    },
    /// Traversal found a directory below the configured depth.
    #[error("audio quality target directory `{path}` exceeds the configured depth of {maximum}")]
    DepthLimitReached {
        /// Directory that would require a deeper traversal.
        path: PathBuf,
        /// Configured maximum depth below a selected root.
        maximum: usize,
    },
    /// A complete result would contain more audio files than configured.
    #[error("audio quality target collection exceeds its {maximum}-file limit")]
    AudioFileLimitReached {
        /// Configured maximum number of returned files.
        maximum: usize,
    },
    /// A selected file or traversed directory changed during collection.
    #[error("audio quality target changed during collection: `{0}`")]
    TargetChanged(PathBuf),
}

/// Expands explicitly selected local files and folders into audio files.
///
/// The returned paths are unique and sorted by the platform's native path
/// ordering. Symbolic links, special files, and every regular file not
/// classified as [`crate::local_browser::LocalEntryKind::Audio`] are skipped.
/// Any limit, inspection failure, cancellation, or unstable directory rejects
/// the whole collection rather than returning misleading partial output.
///
/// # Errors
///
/// Returns [`AudioQualityTargetCollectionError`] when limits are invalid or
/// reached, work is cancelled, inspection fails, or a target changes during
/// traversal.
pub fn collect_audio_quality_targets(
    roots: &[PathBuf],
    limits: AudioQualityTargetLimits,
    cancellation: &AudioQualityCancellation,
) -> Result<Vec<PathBuf>, AudioQualityTargetCollectionError> {
    collect_audio_quality_targets_with_hook(roots, limits, cancellation, |_| {})
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CollectedTargetKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CollectedTargetIdentity {
    kind: CollectedTargetKind,
    length: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    filesystem: Option<crate::file_identity::FilesystemIdentity>,
}

fn collect_audio_quality_targets_with_hook<F>(
    roots: &[PathBuf],
    limits: AudioQualityTargetLimits,
    cancellation: &AudioQualityCancellation,
    mut after_directory: F,
) -> Result<Vec<PathBuf>, AudioQualityTargetCollectionError>
where
    F: FnMut(&Path),
{
    validate_target_limits(limits)?;
    check_target_collection_cancellation(cancellation)?;

    let unique_roots: BTreeSet<PathBuf> = roots.iter().cloned().collect();
    let mut inspected_entries = 0_usize;
    let mut pending_directories = Vec::new();
    let mut root_identities = BTreeMap::new();
    let mut traversed_directory_identities = BTreeMap::new();
    let mut audio_files = BTreeMap::new();

    for root in unique_roots {
        check_target_collection_cancellation(cancellation)?;
        inspect_one_target(&mut inspected_entries, limits.maximum_inspected_entries)?;
        let Some(identity) = inspect_collectable_target(&root)? else {
            continue;
        };
        match identity.kind {
            CollectedTargetKind::Directory => {
                root_identities.insert(root.clone(), identity);
                pending_directories.push((root, 0_usize));
            }
            CollectedTargetKind::File
                if crate::local_browser::classify_local_file(&root)
                    == Some(crate::local_browser::LocalEntryKind::Audio) =>
            {
                ensure_collected_target_unchanged(&root, &identity)?;
                root_identities.insert(root.clone(), identity.clone());
                insert_audio_target(&mut audio_files, root, identity, limits)?;
            }
            CollectedTargetKind::File => {}
        }
    }

    // `pop` processes the lowest native path first, making traversal and the
    // first reported failure deterministic even though `read_dir` is not.
    pending_directories.sort_by(|left, right| right.0.cmp(&left.0));
    let mut visited_directories = BTreeSet::new();
    while let Some((directory, depth)) = pending_directories.pop() {
        check_target_collection_cancellation(cancellation)?;
        if !visited_directories.insert(directory.clone()) {
            continue;
        }
        let before = required_directory_identity(&directory)?;
        traversed_directory_identities.insert(directory.clone(), before.clone());
        let entries = fs::read_dir(&directory).map_err(|source| {
            AudioQualityTargetCollectionError::Inspect {
                path: directory.clone(),
                source,
            }
        })?;
        let mut child_directories = Vec::new();
        for result in entries {
            check_target_collection_cancellation(cancellation)?;
            inspect_one_target(&mut inspected_entries, limits.maximum_inspected_entries)?;
            let entry = result.map_err(|source| AudioQualityTargetCollectionError::Inspect {
                path: directory.clone(),
                source,
            })?;
            let path = entry.path();
            let Some(identity) = inspect_collectable_target(&path)? else {
                continue;
            };
            match identity.kind {
                CollectedTargetKind::Directory => {
                    if visited_directories.contains(&path) {
                        continue;
                    }
                    if depth >= limits.maximum_depth {
                        return Err(AudioQualityTargetCollectionError::DepthLimitReached {
                            path,
                            maximum: limits.maximum_depth,
                        });
                    }
                    child_directories.push((path, depth.saturating_add(1)));
                }
                CollectedTargetKind::File
                    if crate::local_browser::classify_local_file(&path)
                        == Some(crate::local_browser::LocalEntryKind::Audio) =>
                {
                    ensure_collected_target_unchanged(&path, &identity)?;
                    insert_audio_target(&mut audio_files, path, identity, limits)?;
                }
                CollectedTargetKind::File => {}
            }
        }

        after_directory(&directory);
        check_target_collection_cancellation(cancellation)?;
        ensure_collected_target_unchanged(&directory, &before)?;
        child_directories.sort_by(|left, right| right.0.cmp(&left.0));
        pending_directories.extend(child_directories);
    }

    for (root, identity) in &root_identities {
        check_target_collection_cancellation(cancellation)?;
        ensure_collected_target_unchanged(root, identity)?;
    }
    for (directory, identity) in &traversed_directory_identities {
        check_target_collection_cancellation(cancellation)?;
        ensure_collected_target_unchanged(directory, identity)?;
    }
    for (path, identity) in &audio_files {
        check_target_collection_cancellation(cancellation)?;
        ensure_collected_target_unchanged(path, identity)?;
    }
    Ok(audio_files.into_keys().collect())
}

fn validate_target_limits(
    limits: AudioQualityTargetLimits,
) -> Result<(), AudioQualityTargetCollectionError> {
    if limits.maximum_inspected_entries == 0
        || limits.maximum_depth == 0
        || limits.maximum_audio_files == 0
    {
        Err(AudioQualityTargetCollectionError::InvalidLimits)
    } else {
        Ok(())
    }
}

fn check_target_collection_cancellation(
    cancellation: &AudioQualityCancellation,
) -> Result<(), AudioQualityTargetCollectionError> {
    if cancellation.is_cancelled() {
        Err(AudioQualityTargetCollectionError::Cancelled)
    } else {
        Ok(())
    }
}

fn inspect_one_target(
    inspected_entries: &mut usize,
    maximum: usize,
) -> Result<(), AudioQualityTargetCollectionError> {
    *inspected_entries = inspected_entries.saturating_add(1);
    if *inspected_entries > maximum {
        Err(AudioQualityTargetCollectionError::InspectedEntryLimitReached { maximum })
    } else {
        Ok(())
    }
}

fn inspect_collectable_target(
    path: &Path,
) -> Result<Option<CollectedTargetIdentity>, AudioQualityTargetCollectionError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| {
        AudioQualityTargetCollectionError::Inspect {
            path: path.to_owned(),
            source,
        }
    })?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() || !(file_type.is_file() || file_type.is_dir()) {
        return Ok(None);
    }
    Ok(Some(collected_target_identity(path, &metadata)))
}

fn required_directory_identity(
    path: &Path,
) -> Result<CollectedTargetIdentity, AudioQualityTargetCollectionError> {
    match inspect_collectable_target(path)? {
        Some(identity) if identity.kind == CollectedTargetKind::Directory => Ok(identity),
        _ => Err(AudioQualityTargetCollectionError::TargetChanged(
            path.to_owned(),
        )),
    }
}

fn collected_target_identity(path: &Path, metadata: &fs::Metadata) -> CollectedTargetIdentity {
    CollectedTargetIdentity {
        kind: if metadata.file_type().is_dir() {
            CollectedTargetKind::Directory
        } else {
            CollectedTargetKind::File
        },
        length: metadata.len(),
        modified: metadata.modified().ok(),
        created: metadata.created().ok(),
        filesystem: crate::file_identity::filesystem_identity(path, metadata),
    }
}

fn ensure_collected_target_unchanged(
    path: &Path,
    expected: &CollectedTargetIdentity,
) -> Result<(), AudioQualityTargetCollectionError> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if !metadata.file_type().is_symlink()
                && collected_target_identity(path, &metadata) == *expected =>
        {
            Ok(())
        }
        Ok(_) => Err(AudioQualityTargetCollectionError::TargetChanged(
            path.to_owned(),
        )),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Err(
            AudioQualityTargetCollectionError::TargetChanged(path.to_owned()),
        ),
        Err(source) => Err(AudioQualityTargetCollectionError::Inspect {
            path: path.to_owned(),
            source,
        }),
    }
}

fn insert_audio_target(
    audio_files: &mut BTreeMap<PathBuf, CollectedTargetIdentity>,
    path: PathBuf,
    identity: CollectedTargetIdentity,
    limits: AudioQualityTargetLimits,
) -> Result<(), AudioQualityTargetCollectionError> {
    if let Some(previous) = audio_files.get(&path) {
        return if previous == &identity {
            Ok(())
        } else {
            Err(AudioQualityTargetCollectionError::TargetChanged(path))
        };
    }
    if let Some(existing_path) = audio_files.iter().find_map(|(existing_path, existing)| {
        same_filesystem_file(existing, &identity).then(|| existing_path.clone())
    }) {
        if existing_path <= path {
            return Ok(());
        }
        audio_files.remove(&existing_path);
    }
    if audio_files.len() >= limits.maximum_audio_files {
        return Err(AudioQualityTargetCollectionError::AudioFileLimitReached {
            maximum: limits.maximum_audio_files,
        });
    }
    audio_files.insert(path, identity);
    Ok(())
}

fn same_filesystem_file(left: &CollectedTargetIdentity, right: &CollectedTargetIdentity) -> bool {
    left.kind == CollectedTargetKind::File
        && right.kind == CollectedTargetKind::File
        && matches!(
            (left.filesystem, right.filesystem),
            (Some(left), Some(right)) if left.volume == right.volume && left.file == right.file
        )
}

/// Hard resource limits for one spectral analysis.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioQualityLimits {
    /// Largest decoded timeline accepted from `FFmpeg`.
    pub maximum_audio_duration: Duration,
    /// Whole-operation wall-clock deadline.
    pub timeout: Duration,
}

impl Default for AudioQualityLimits {
    fn default() -> Self {
        Self {
            maximum_audio_duration: DEFAULT_MAXIMUM_AUDIO_DURATION,
            timeout: DEFAULT_ANALYSIS_TIMEOUT,
        }
    }
}

impl AudioQualityLimits {
    fn validate(self) -> Result<Self, AudioQualityConfigurationError> {
        if self.maximum_audio_duration.is_zero() {
            return Err(AudioQualityConfigurationError::ZeroAudioDuration);
        }
        if self.maximum_audio_duration > MAXIMUM_AUDIO_DURATION {
            return Err(AudioQualityConfigurationError::AudioDurationTooLong);
        }
        if self.timeout.is_zero() {
            return Err(AudioQualityConfigurationError::ZeroTimeout);
        }
        Ok(self)
    }

    fn maximum_frames(self) -> u64 {
        let frames = self
            .maximum_audio_duration
            .as_nanos()
            .saturating_mul(u128::from(ANALYSIS_SAMPLE_RATE_HZ))
            .div_ceil(1_000_000_000);
        u64::try_from(frames).unwrap_or(u64::MAX)
    }
}

/// Invalid analyzer resource configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioQualityConfigurationError {
    /// Zero decoded audio cannot contain an FFT window.
    ZeroAudioDuration,
    /// The decoded-audio ceiling exceeds the process-wide maximum.
    AudioDurationTooLong,
    /// A zero deadline would cancel every request before it starts.
    ZeroTimeout,
}

impl fmt::Display for AudioQualityConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ZeroAudioDuration => "maximum audio duration must be positive",
            Self::AudioDurationTooLong => "maximum audio duration exceeds two minutes",
            Self::ZeroTimeout => "audio quality timeout must be positive",
        })
    }
}

impl Error for AudioQualityConfigurationError {}

/// Safe failure from one quality analysis.
#[derive(Debug)]
pub enum AudioQualityError {
    /// The caller superseded or closed the request.
    Cancelled,
    /// `FFmpeg` exceeded the configured wall-clock deadline.
    TimedOut,
    /// The selected path could not be inspected.
    FileUnavailable(io::Error),
    /// The selected path is not a regular file.
    NotRegularFile,
    /// The selected file changed before analysis completed.
    FileChanged,
    /// The configured `FFmpeg` executable was not found.
    FfmpegUnavailable(io::Error),
    /// `FFmpeg` could not be started for another operating-system reason.
    SpawnFailed(io::Error),
    /// The bounded PCM reader thread could not be started.
    ReaderSpawnFailed(io::Error),
    /// `FFmpeg` process supervision failed.
    ProcessFailed(io::Error),
    /// `FFmpeg` rejected or could not decode the selected audio.
    DecodeFailed,
    /// `FFmpeg` emitted malformed, non-finite, or over-limit float PCM.
    InvalidPcm,
    /// The bounded PCM reader stopped without returning a result.
    ReaderStopped,
}

impl AudioQualityError {
    /// Returns whether installing or configuring `FFmpeg` can resolve the failure.
    #[must_use]
    pub const fn is_missing_executable(&self) -> bool {
        matches!(self, Self::FfmpegUnavailable(_))
    }
}

impl fmt::Display for AudioQualityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Cancelled => "audio quality analysis was cancelled",
            Self::TimedOut => "audio quality analysis timed out",
            Self::FileUnavailable(_) => "the local file is unavailable",
            Self::NotRegularFile => "the local path is not a regular file",
            Self::FileChanged => "the local file changed during audio quality analysis",
            Self::FfmpegUnavailable(_) => {
                "FFmpeg is unavailable; install it or configure its executable path"
            }
            Self::SpawnFailed(_) => "FFmpeg could not be started",
            Self::ReaderSpawnFailed(_) => "the audio quality reader could not be started",
            Self::ProcessFailed(_) => "FFmpeg process supervision failed",
            Self::DecodeFailed => "FFmpeg could not decode the local audio",
            Self::InvalidPcm => "FFmpeg returned invalid audio for quality analysis",
            Self::ReaderStopped => "the audio quality reader stopped unexpectedly",
        })
    }
}

impl Error for AudioQualityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::FileUnavailable(error)
            | Self::FfmpegUnavailable(error)
            | Self::SpawnFailed(error)
            | Self::ReaderSpawnFailed(error)
            | Self::ProcessFailed(error) => Some(error),
            Self::Cancelled
            | Self::TimedOut
            | Self::NotRegularFile
            | Self::FileChanged
            | Self::DecodeFailed
            | Self::InvalidPcm
            | Self::ReaderStopped => None,
        }
    }
}

/// Mockable boundary for local audio-quality analysis.
pub trait AudioQualityAnalyzer: Send + Sync {
    /// Analyzes one identity-bound local file.
    ///
    /// # Errors
    ///
    /// Returns [`AudioQualityError`] when the request is cancelled, its file
    /// changes, `FFmpeg` fails, output is invalid, or a resource limit is reached.
    fn analyze(
        &self,
        request: &AudioQualityRequest,
        cancellation: &AudioQualityCancellation,
    ) -> Result<AudioQualityReport, AudioQualityError>;
}

/// Production shell-free analyzer using the configured `FFmpeg` executable.
#[derive(Clone, Debug)]
pub struct FfmpegAudioQualityAnalyzer {
    program: OsString,
    limits: AudioQualityLimits,
    poll_interval: Duration,
}

impl Default for FfmpegAudioQualityAnalyzer {
    fn default() -> Self {
        Self {
            program: OsString::from("ffmpeg"),
            limits: AudioQualityLimits::default(),
            poll_interval: PROCESS_POLL_INTERVAL,
        }
    }
}

impl FfmpegAudioQualityAnalyzer {
    /// Uses the supplied `FFmpeg` executable with default limits.
    #[must_use]
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            ..Self::default()
        }
    }

    /// Uses the supplied `FFmpeg` executable and validated resource limits.
    ///
    /// # Errors
    ///
    /// Returns [`AudioQualityConfigurationError`] when a duration or deadline is
    /// zero, or the decoded duration exceeds the fixed two-minute ceiling.
    pub fn with_limits(
        program: impl Into<OsString>,
        limits: AudioQualityLimits,
    ) -> Result<Self, AudioQualityConfigurationError> {
        Ok(Self {
            program: program.into(),
            limits: limits.validate()?,
            poll_interval: PROCESS_POLL_INTERVAL,
        })
    }

    /// Returns the configured `FFmpeg` executable.
    #[must_use]
    pub fn program(&self) -> &OsStr {
        &self.program
    }

    /// Returns the configured resource limits.
    #[must_use]
    pub const fn limits(&self) -> AudioQualityLimits {
        self.limits
    }

    fn command(&self, path: &Path) -> Result<Command, AudioQualityError> {
        let path = std::path::absolute(path).map_err(AudioQualityError::FileUnavailable)?;
        let mut command = Command::new(&self.program);
        crate::child_process::supervised(&mut command);
        command
            .arg("-nostdin")
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-threads")
            .arg("1")
            .arg("-i")
            .arg(path)
            .arg("-map")
            .arg("0:a:0")
            .arg("-vn")
            .arg("-sn")
            .arg("-dn")
            .arg("-t")
            .arg(ffmpeg_duration(self.limits.maximum_audio_duration))
            .arg("-ac")
            .arg("2")
            .arg("-ar")
            .arg(ANALYSIS_SAMPLE_RATE_HZ.to_string())
            .arg("-c:a")
            .arg("pcm_f32le")
            .arg("-f")
            .arg("f32le")
            .arg("-")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // Diagnostics can contain private paths and arguments. The adapter
            // exposes stable error categories instead of retaining stderr.
            .stderr(Stdio::null());
        Ok(command)
    }
}

impl AudioQualityAnalyzer for FfmpegAudioQualityAnalyzer {
    fn analyze(
        &self,
        request: &AudioQualityRequest,
        cancellation: &AudioQualityCancellation,
    ) -> Result<AudioQualityReport, AudioQualityError> {
        if cancellation.is_cancelled() {
            return Err(AudioQualityError::Cancelled);
        }
        ensure_identity(request)?;

        let mut command = self.command(request.path())?;
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                AudioQualityError::FfmpegUnavailable(error)
            } else {
                AudioQualityError::SpawnFailed(error)
            }
        })?;
        let Some(stdout) = child.stdout.take() else {
            crate::child_process::terminate_tree(&mut child);
            return Err(AudioQualityError::InvalidPcm);
        };
        let maximum_frames = self.limits.maximum_frames();
        let (reader_sender, reader_receiver) = mpsc::sync_channel(1);
        let reader = match thread::Builder::new()
            .name("youta-audio-quality-reader".to_owned())
            .spawn(move || {
                let result = analyze_f32le_pcm(stdout, maximum_frames);
                let _ = reader_sender.send(result);
            }) {
            Ok(reader) => reader,
            Err(error) => {
                crate::child_process::terminate_tree(&mut child);
                return Err(AudioQualityError::ReaderSpawnFailed(error));
            }
        };
        // A malformed wrapper can leave an inherited stdout handle open. The
        // bounded result channel preserves cancellation and timeout semantics.
        drop(reader);

        let started = Instant::now();
        let status = loop {
            if cancellation.is_cancelled() {
                crate::child_process::terminate_tree(&mut child);
                return Err(AudioQualityError::Cancelled);
            }
            if started.elapsed() >= self.limits.timeout {
                crate::child_process::terminate_tree(&mut child);
                return Err(AudioQualityError::TimedOut);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => thread::sleep(
                    self.poll_interval
                        .min(self.limits.timeout.saturating_sub(started.elapsed())),
                ),
                Err(error) => {
                    crate::child_process::terminate_tree(&mut child);
                    return Err(AudioQualityError::ProcessFailed(error));
                }
            }
        };

        let spectral = loop {
            if cancellation.is_cancelled() {
                crate::child_process::terminate_tree(&mut child);
                return Err(AudioQualityError::Cancelled);
            }
            let remaining = self.limits.timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                crate::child_process::terminate_tree(&mut child);
                return Err(AudioQualityError::TimedOut);
            }
            match reader_receiver.recv_timeout(self.poll_interval.min(remaining)) {
                Ok(Ok(result)) => break result,
                Ok(Err(_)) => {
                    crate::child_process::terminate_tree(&mut child);
                    return Err(AudioQualityError::InvalidPcm);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    crate::child_process::terminate_tree(&mut child);
                    return Err(AudioQualityError::ReaderStopped);
                }
            }
        };
        // A configured executable may be a wrapper. End any descendants after
        // its direct process exits so a helper cannot outlive the analysis.
        crate::child_process::terminate_tree(&mut child);
        if cancellation.is_cancelled() {
            return Err(AudioQualityError::Cancelled);
        }
        if started.elapsed() >= self.limits.timeout {
            return Err(AudioQualityError::TimedOut);
        }
        if !status.success() {
            return Err(AudioQualityError::DecodeFailed);
        }
        ensure_identity(request)?;
        Ok(spectral.into_report(request))
    }
}

fn ensure_identity(request: &AudioQualityRequest) -> Result<(), AudioQualityError> {
    match AudioQualityIdentity::from_path(request.path()) {
        Ok(identity) if &identity == request.identity() => Ok(()),
        Ok(_) | Err(AudioQualityError::FileUnavailable(_)) => Err(AudioQualityError::FileChanged),
        Err(error) => Err(error),
    }
}

fn ffmpeg_duration(duration: Duration) -> String {
    let microseconds = duration.as_nanos().div_ceil(1_000);
    format!(
        "{}.{:06}",
        microseconds / 1_000_000,
        microseconds % 1_000_000
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PcmAnalysisError {
    ReadFailed,
    IncompleteSample,
    IncompleteFrame,
    NonFiniteSample,
    OutputLimitExceeded,
}

#[derive(Clone, Copy, Debug)]
struct CutoffEvidence {
    cutoff_hz: f32,
    cliff_db: f32,
}

#[derive(Clone, Copy, Debug)]
struct WindowEvidence {
    cutoff: Option<CutoffEvidence>,
    high_band_db: f32,
}

#[derive(Debug)]
struct SpectralSummary {
    assessment: AudioQualityAssessment,
    effective_bandwidth_hz: Option<u32>,
    confidence: AudioQualityConfidence,
    cliff_db: Option<f32>,
    window_agreement_percent: u8,
    active_windows: u32,
    analyzed_frames: u64,
}

impl SpectralSummary {
    fn into_report(self, request: &AudioQualityRequest) -> AudioQualityReport {
        let has_cutoff = self.assessment == AudioQualityAssessment::BandLimited
            && self.effective_bandwidth_hz.is_some();
        let sample_rate_limited = has_cutoff
            && self.effective_bandwidth_hz.is_some_and(|cutoff_hz| {
                request
                    .source_sample_rate_hz()
                    .is_some_and(|sample_rate_hz| {
                        cutoff_matches_source_nyquist(cutoff_hz, sample_rate_hz)
                    })
            });
        let sample_rate_unavailable = has_cutoff && request.source_sample_rate_hz().is_none();
        let channel_count_unavailable = has_cutoff && request.source_channels().is_none();
        let multichannel_downmix =
            has_cutoff && request.source_channels().is_some_and(|count| count > 2);
        let assessment = if sample_rate_limited {
            AudioQualityAssessment::SampleRateLimited
        } else if sample_rate_unavailable {
            AudioQualityAssessment::SampleRateUnavailable
        } else if channel_count_unavailable {
            AudioQualityAssessment::ChannelCountUnavailable
        } else if multichannel_downmix {
            AudioQualityAssessment::MultichannelDownmix
        } else {
            self.assessment
        };
        AudioQualityReport {
            identity: request.identity.clone(),
            declared_encoding: request.declared_encoding,
            source_sample_rate_hz: request.source_sample_rate_hz,
            source_channels: request.source_channels,
            assessment,
            effective_bandwidth_hz: self.effective_bandwidth_hz,
            confidence: self.confidence,
            cliff_db: self.cliff_db,
            window_agreement_percent: self.window_agreement_percent,
            active_windows: self.active_windows,
            analyzed_frames: self.analyzed_frames,
        }
    }
}

fn cutoff_matches_source_nyquist(cutoff_hz: u32, sample_rate_hz: u32) -> bool {
    let nyquist_hz = sample_rate_hz / 2;
    nyquist_hz > 0 && cutoff_hz.abs_diff(nyquist_hz) <= SAMPLE_RATE_NYQUIST_TOLERANCE_HZ
}

struct StreamingSpectrum {
    fft: Arc<dyn Fft<f32>>,
    hann: Vec<f32>,
    window: Vec<f32>,
    fft_buffer: Vec<Complex<f32>>,
    evidence: Vec<WindowEvidence>,
    analyzed_frames: u64,
}

struct StereoStreamingSpectrum {
    left: StreamingSpectrum,
    right: StreamingSpectrum,
    analyzed_frames: u64,
}

impl StereoStreamingSpectrum {
    fn new() -> Self {
        Self {
            left: StreamingSpectrum::new(),
            right: StreamingSpectrum::new(),
            analyzed_frames: 0,
        }
    }

    fn push_frame(
        &mut self,
        left: f32,
        right: f32,
        maximum_frames: u64,
    ) -> Result<(), PcmAnalysisError> {
        if self.analyzed_frames >= maximum_frames {
            return Err(PcmAnalysisError::OutputLimitExceeded);
        }
        self.left.push(std::slice::from_ref(&left))?;
        self.right.push(std::slice::from_ref(&right))?;
        self.analyzed_frames = self.analyzed_frames.saturating_add(1);
        Ok(())
    }

    fn finish(self) -> SpectralSummary {
        combine_channel_summaries(
            self.left.finish(),
            self.right.finish(),
            self.analyzed_frames,
        )
    }
}

/// Combines the two normalized output channels without downmixing them again.
///
/// A full-band channel disproves a whole-file cutoff even when the other
/// channel is silent or limited. A positive cutoff, in contrast, requires all
/// output channels containing usable signal to agree within the same
/// tolerance. [`SpectralSummary::into_report`] suppresses cutoff interpretation
/// when metadata says `FFmpeg` downmixed an input with more than two channels
/// to produce this stereo stream.
fn combine_channel_summaries(
    mut left: SpectralSummary,
    mut right: SpectralSummary,
    analyzed_frames: u64,
) -> SpectralSummary {
    left.analyzed_frames = analyzed_frames;
    right.analyzed_frames = analyzed_frames;
    let left_usable = left.assessment != AudioQualityAssessment::InsufficientSignal;
    let right_usable = right.assessment != AudioQualityAssessment::InsufficientSignal;
    match (left_usable, right_usable) {
        (false, false) => {
            left.active_windows = left.active_windows.max(right.active_windows);
            return left;
        }
        (true, false) => return left,
        (false, true) => return right,
        (true, true) => {}
    }

    if left.assessment == AudioQualityAssessment::NoSuspiciousSignature
        || right.assessment == AudioQualityAssessment::NoSuspiciousSignature
    {
        let active_windows = left.active_windows.max(right.active_windows);
        let mut report = if left.assessment == AudioQualityAssessment::NoSuspiciousSignature
            && (right.assessment != AudioQualityAssessment::NoSuspiciousSignature
                || left.active_windows >= right.active_windows)
        {
            left
        } else {
            right
        };
        report.active_windows = active_windows;
        return report;
    }

    if left.assessment == AudioQualityAssessment::BandLimited
        && right.assessment == AudioQualityAssessment::BandLimited
        && let (Some(left_cutoff), Some(right_cutoff)) =
            (left.effective_bandwidth_hz, right.effective_bandwidth_hz)
        && left_cutoff.abs_diff(right_cutoff) <= CUTOFF_AGREEMENT_HZ_INTEGER
    {
        let cutoff_hz = u32::midpoint(left_cutoff, right_cutoff);
        return SpectralSummary {
            assessment: AudioQualityAssessment::BandLimited,
            effective_bandwidth_hz: Some(cutoff_hz),
            confidence: minimum_confidence(left.confidence, right.confidence),
            cliff_db: match (left.cliff_db, right.cliff_db) {
                (Some(left), Some(right)) => Some(left.min(right)),
                _ => None,
            },
            window_agreement_percent: left
                .window_agreement_percent
                .min(right.window_agreement_percent),
            active_windows: left.active_windows.max(right.active_windows),
            analyzed_frames,
        };
    }

    SpectralSummary {
        assessment: AudioQualityAssessment::Inconclusive,
        effective_bandwidth_hz: None,
        confidence: AudioQualityConfidence::Low,
        cliff_db: None,
        window_agreement_percent: left
            .window_agreement_percent
            .min(right.window_agreement_percent),
        active_windows: left.active_windows.max(right.active_windows),
        analyzed_frames,
    }
}

fn minimum_confidence(
    left: AudioQualityConfidence,
    right: AudioQualityConfidence,
) -> AudioQualityConfidence {
    use AudioQualityConfidence::{High, Low, Medium};
    match (left, right) {
        (Low, _) | (_, Low) => Low,
        (Medium, _) | (_, Medium) => Medium,
        (High, High) => High,
    }
}

impl StreamingSpectrum {
    fn new() -> Self {
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let hann = (0..FFT_SIZE)
            .map(|index| {
                let phase =
                    2.0 * std::f32::consts::PI * bounded_usize_to_f32(index) / FFT_LAST_INDEX_F32;
                0.5 - 0.5 * phase.cos()
            })
            .collect();
        Self {
            fft,
            hann,
            window: Vec::with_capacity(FFT_SIZE),
            fft_buffer: vec![Complex::new(0.0, 0.0); FFT_SIZE],
            evidence: Vec::new(),
            analyzed_frames: 0,
        }
    }

    fn push(&mut self, samples: &[f32]) -> Result<(), PcmAnalysisError> {
        for &sample in samples {
            if !sample.is_finite() {
                return Err(PcmAnalysisError::NonFiniteSample);
            }
            self.window.push(sample);
            self.analyzed_frames = self.analyzed_frames.saturating_add(1);
            if self.window.len() == FFT_SIZE {
                if let Some(evidence) =
                    analyze_window(&self.window, &self.hann, &self.fft, &mut self.fft_buffer)
                {
                    self.evidence.push(evidence);
                }
                self.window.copy_within(FFT_HOP.., 0);
                self.window.truncate(FFT_SIZE - FFT_HOP);
            }
        }
        Ok(())
    }

    fn finish(self) -> SpectralSummary {
        summarize_evidence(&self.evidence, self.analyzed_frames)
    }
}

fn analyze_f32le_pcm(
    mut reader: impl Read,
    maximum_frames: u64,
) -> Result<SpectralSummary, PcmAnalysisError> {
    let mut analyzer = StereoStreamingSpectrum::new();
    let mut buffer = vec![0_u8; PCM_READ_BUFFER_BYTES].into_boxed_slice();
    let mut pending = [0_u8; 4];
    let mut pending_bytes = 0_usize;
    let mut pending_left = None;

    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|_| PcmAnalysisError::ReadFailed)?;
        if read == 0 {
            break;
        }
        let mut offset = 0_usize;
        if pending_bytes > 0 {
            let required = 4 - pending_bytes;
            let copied = required.min(read);
            pending[pending_bytes..pending_bytes + copied].copy_from_slice(&buffer[..copied]);
            pending_bytes += copied;
            offset += copied;
            if pending_bytes < 4 {
                continue;
            }
            push_interleaved_sample(
                &mut analyzer,
                &mut pending_left,
                f32::from_le_bytes(pending),
                maximum_frames,
            )?;
        }
        let complete_bytes = (read - offset) / 4 * 4;
        for bytes in buffer[offset..offset + complete_bytes].chunks_exact(4) {
            push_interleaved_sample(
                &mut analyzer,
                &mut pending_left,
                f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
                maximum_frames,
            )?;
        }
        offset += complete_bytes;
        let remainder = &buffer[offset..read];
        pending[..remainder.len()].copy_from_slice(remainder);
        pending_bytes = remainder.len();
    }

    if pending_bytes != 0 {
        return Err(PcmAnalysisError::IncompleteSample);
    }
    if pending_left.is_some() {
        return Err(PcmAnalysisError::IncompleteFrame);
    }
    Ok(analyzer.finish())
}

fn push_interleaved_sample(
    analyzer: &mut StereoStreamingSpectrum,
    pending_left: &mut Option<f32>,
    sample: f32,
    maximum_frames: u64,
) -> Result<(), PcmAnalysisError> {
    if !sample.is_finite() {
        return Err(PcmAnalysisError::NonFiniteSample);
    }
    if let Some(left) = pending_left.take() {
        analyzer.push_frame(left, sample, maximum_frames)
    } else {
        *pending_left = Some(sample);
        Ok(())
    }
}

fn analyze_window(
    samples: &[f32],
    hann: &[f32],
    fft: &Arc<dyn Fft<f32>>,
    fft_buffer: &mut [Complex<f32>],
) -> Option<WindowEvidence> {
    let squared_sum = samples.iter().map(|sample| sample * sample).sum::<f32>();
    let rms = (squared_sum / bounded_usize_to_f32(samples.len())).sqrt();
    if rms < MINIMUM_WINDOW_RMS {
        return None;
    }

    for (((output, &sample), &weight), _) in fft_buffer
        .iter_mut()
        .zip(samples)
        .zip(hann)
        .zip(0..FFT_SIZE)
    {
        *output = Complex::new(sample * weight, 0.0);
    }
    fft.process(fft_buffer);
    let powers: Vec<f32> = fft_buffer[..=FFT_SIZE / 2]
        .iter()
        .map(Complex::norm_sqr)
        .collect();
    let prefix = power_prefix(&powers);
    let reference = band_mean(&prefix, 1_000.0, 8_000.0);
    if reference <= f32::EPSILON {
        return Some(WindowEvidence {
            cutoff: None,
            high_band_db: f32::NEG_INFINITY,
        });
    }
    let high_band_db = power_ratio_db(band_mean(&prefix, 19_000.0, 22_000.0), reference);
    let mut best = None;
    let start = frequency_bin(MINIMUM_CUTOFF_HZ);
    let end = frequency_bin(MAXIMUM_CUTOFF_HZ).min(FFT_SIZE / 2 - 1);
    for bin in start..=end {
        let cutoff_hz = bin_frequency(bin);
        let before = band_mean(&prefix, cutoff_hz - 300.0, cutoff_hz - 75.0);
        let after = band_mean(&prefix, cutoff_hz + 75.0, cutoff_hz + 300.0);
        let before_reference_db = power_ratio_db(before, reference);
        let cliff_db = power_ratio_db(before, after);
        let sustained_after = band_mean(
            &prefix,
            cutoff_hz + SUSTAINED_ATTENUATION_OFFSET_HZ,
            SUSTAINED_ATTENUATION_END_HZ,
        );
        let sustained_attenuation_db = power_ratio_db(before, sustained_after);
        if before_reference_db < -30.0
            || cliff_db < MINIMUM_CLIFF_DB
            || sustained_attenuation_db < MINIMUM_SUSTAINED_ATTENUATION_DB
        {
            continue;
        }
        if best.is_none_or(|evidence: CutoffEvidence| cliff_db > evidence.cliff_db) {
            best = Some(CutoffEvidence {
                cutoff_hz,
                cliff_db,
            });
        }
    }
    Some(WindowEvidence {
        cutoff: best,
        high_band_db,
    })
}

fn power_prefix(powers: &[f32]) -> Vec<f32> {
    let mut prefix = Vec::with_capacity(powers.len() + 1);
    prefix.push(0.0);
    for &power in powers {
        prefix.push(prefix.last().copied().unwrap_or(0.0) + power);
    }
    prefix
}

fn band_mean(prefix: &[f32], lower_hz: f32, upper_hz: f32) -> f32 {
    let maximum_bin = prefix.len().saturating_sub(2);
    let lower = frequency_bin(lower_hz.max(0.0)).min(maximum_bin);
    let upper = frequency_bin(upper_hz.max(lower_hz)).min(maximum_bin + 1);
    if upper <= lower {
        return 0.0;
    }
    (prefix[upper] - prefix[lower]) / bounded_usize_to_f32(upper - lower)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn frequency_bin(frequency_hz: f32) -> usize {
    (frequency_hz * FFT_SIZE_F32 / ANALYSIS_SAMPLE_RATE_F32).round() as usize
}

fn bin_frequency(bin: usize) -> f32 {
    bounded_usize_to_f32(bin) * ANALYSIS_SAMPLE_RATE_F32 / FFT_SIZE_F32
}

fn bounded_usize_to_f32(value: usize) -> f32 {
    f32::from(u16::try_from(value).expect("spectral index stays below 65,536"))
}

fn power_ratio_db(numerator: f32, denominator: f32) -> f32 {
    let denominator = denominator.max(f32::MIN_POSITIVE);
    let ratio = (numerator.max(f32::MIN_POSITIVE) / denominator)
        .clamp(MINIMUM_POWER_RATIO, MAXIMUM_POWER_RATIO);
    10.0 * ratio.log10()
}

fn summarize_evidence(evidence: &[WindowEvidence], analyzed_frames: u64) -> SpectralSummary {
    let active_windows = u32::try_from(evidence.len()).unwrap_or(u32::MAX);
    if evidence.len() < MINIMUM_ACTIVE_WINDOWS {
        return SpectralSummary {
            assessment: AudioQualityAssessment::InsufficientSignal,
            effective_bandwidth_hz: None,
            confidence: AudioQualityConfidence::Low,
            cliff_db: None,
            window_agreement_percent: 0,
            active_windows,
            analyzed_frames,
        };
    }

    let mut cutoffs: Vec<f32> = evidence
        .iter()
        .filter_map(|window| window.cutoff.map(|cutoff| cutoff.cutoff_hz))
        .collect();
    cutoffs.sort_by(f32::total_cmp);
    let centre = median(&cutoffs);
    let agreeing: Vec<CutoffEvidence> = centre.map_or_else(Vec::new, |centre| {
        evidence
            .iter()
            .filter_map(|window| window.cutoff)
            .filter(|cutoff| (cutoff.cutoff_hz - centre).abs() <= CUTOFF_AGREEMENT_HZ)
            .collect()
    });
    let agreement = percentage(agreeing.len(), evidence.len());
    if agreement >= MINIMUM_CUTOFF_AGREEMENT_PERCENT {
        let mut agreeing_cutoffs: Vec<f32> =
            agreeing.iter().map(|cutoff| cutoff.cutoff_hz).collect();
        let mut cliffs: Vec<f32> = agreeing.iter().map(|cutoff| cutoff.cliff_db).collect();
        agreeing_cutoffs.sort_by(f32::total_cmp);
        cliffs.sort_by(f32::total_cmp);
        let cutoff_hz = median(&agreeing_cutoffs).unwrap_or(0.0);
        let cliff_db = median(&cliffs).unwrap_or(0.0);
        let confidence = if agreement >= 90 && cliff_db >= 30.0 {
            AudioQualityConfidence::High
        } else if agreement >= 80 && cliff_db >= 24.0 {
            AudioQualityConfidence::Medium
        } else {
            AudioQualityConfidence::Low
        };
        return SpectralSummary {
            assessment: AudioQualityAssessment::BandLimited,
            effective_bandwidth_hz: Some(rounded_hz(cutoff_hz)),
            confidence,
            cliff_db: Some(cliff_db),
            window_agreement_percent: agreement,
            active_windows,
            analyzed_frames,
        };
    }

    let mut high_bands: Vec<f32> = evidence.iter().map(|window| window.high_band_db).collect();
    high_bands.sort_by(f32::total_cmp);
    let broad_high_frequency_content = median(&high_bands).is_some_and(|level| level >= -12.0);
    SpectralSummary {
        assessment: if broad_high_frequency_content {
            AudioQualityAssessment::NoSuspiciousSignature
        } else {
            AudioQualityAssessment::Inconclusive
        },
        effective_bandwidth_hz: None,
        confidence: if broad_high_frequency_content && evidence.len() >= 10 {
            AudioQualityConfidence::Medium
        } else {
            AudioQualityConfidence::Low
        },
        cliff_db: None,
        window_agreement_percent: agreement,
        active_windows,
        analyzed_frames,
    }
}

fn percentage(numerator: usize, denominator: usize) -> u8 {
    if denominator == 0 {
        return 0;
    }
    u8::try_from(numerator.saturating_mul(100) / denominator).unwrap_or(100)
}

fn median(sorted: &[f32]) -> Option<f32> {
    if sorted.is_empty() {
        return None;
    }
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        Some(f32::midpoint(sorted[middle - 1], sorted[middle]))
    } else {
        Some(sorted[middle])
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn rounded_hz(frequency_hz: f32) -> u32 {
    debug_assert!(frequency_hz.is_finite() && frequency_hz >= 0.0);
    frequency_hz.round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn periodic_noise(maximum_hz: f32, rolloff_start_hz: Option<f32>) -> Vec<f32> {
        let mut planner = FftPlanner::new();
        let inverse = planner.plan_fft_inverse(FFT_SIZE);
        let mut spectrum = vec![Complex::new(0.0, 0.0); FFT_SIZE];
        let mut state = 0xD1B5_4A32_D192_ED03_u64;
        for bin in 1..FFT_SIZE / 2 {
            let frequency = bin_frequency(bin);
            if frequency > maximum_hz {
                continue;
            }
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let phase = (state >> 32) as f32 / u32::MAX as f32 * 2.0 * std::f32::consts::PI;
            let amplitude = rolloff_start_hz.map_or(1.0, |start| {
                if frequency <= start {
                    1.0
                } else {
                    let fraction = ((frequency - start) / (maximum_hz - start)).clamp(0.0, 1.0);
                    10.0_f32.powf(-24.0 * fraction / 20.0)
                }
            });
            let value = Complex::from_polar(amplitude, phase);
            spectrum[bin] = value;
            spectrum[FFT_SIZE - bin] = value.conj();
        }
        inverse.process(&mut spectrum);
        let scale = spectrum
            .iter()
            .map(|value| value.re.abs())
            .fold(0.0_f32, f32::max)
            .max(f32::EPSILON);
        let period: Vec<f32> = spectrum
            .iter()
            .map(|value| value.re / scale * 0.5)
            .collect();
        period.repeat(8)
    }

    fn periodic_noise_with_notch(notch_start_hz: f32, notch_end_hz: f32) -> Vec<f32> {
        let mut planner = FftPlanner::new();
        let inverse = planner.plan_fft_inverse(FFT_SIZE);
        let mut spectrum = vec![Complex::new(0.0, 0.0); FFT_SIZE];
        let mut state = 0xA24B_AED4_963E_E407_u64;
        for bin in 1..FFT_SIZE / 2 {
            let frequency = bin_frequency(bin);
            if frequency > 23_000.0 || (notch_start_hz..=notch_end_hz).contains(&frequency) {
                continue;
            }
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let phase = (state >> 32) as f32 / u32::MAX as f32 * 2.0 * std::f32::consts::PI;
            let value = Complex::from_polar(1.0, phase);
            spectrum[bin] = value;
            spectrum[FFT_SIZE - bin] = value.conj();
        }
        inverse.process(&mut spectrum);
        let scale = spectrum
            .iter()
            .map(|value| value.re.abs())
            .fold(0.0_f32, f32::max)
            .max(f32::EPSILON);
        let period = spectrum
            .iter()
            .map(|value| value.re / scale * 0.5)
            .collect::<Vec<_>>();
        period.repeat(8)
    }

    fn analyze_samples(samples: &[f32], declared_encoding: DeclaredEncoding) -> AudioQualityReport {
        analyze_samples_with_rate(samples, declared_encoding, None)
    }

    fn analyze_samples_with_rate(
        samples: &[f32],
        declared_encoding: DeclaredEncoding,
        source_sample_rate_hz: Option<u32>,
    ) -> AudioQualityReport {
        let mut analyzer = StreamingSpectrum::new();
        analyzer.push(samples).expect("finite test PCM");
        let identity = AudioQualityIdentity {
            length: 1,
            modified: None,
            created: None,
            filesystem: None,
        };
        let mut request = AudioQualityRequest::new(PathBuf::from("fixture.flac"), identity)
            .with_declared_encoding(declared_encoding)
            .with_source_channels(1);
        if let Some(sample_rate_hz) = source_sample_rate_hz {
            request = request.with_source_sample_rate_hz(sample_rate_hz);
        }
        analyzer.finish().into_report(&request)
    }

    fn bandlimited_fixture_report(
        declared_bitrate_kbps: u32,
        confidence: AudioQualityConfidence,
        source_channels: u8,
    ) -> AudioQualityReport {
        let identity = AudioQualityIdentity {
            length: 1,
            modified: None,
            created: None,
            filesystem: None,
        };
        let request = AudioQualityRequest::new(PathBuf::from("fixture.mp3"), identity)
            .with_declared_encoding(DeclaredEncoding::Lossy {
                bitrate_kbps: Some(declared_bitrate_kbps),
            })
            .with_source_sample_rate_hz(48_000)
            .with_source_channels(source_channels);
        SpectralSummary {
            assessment: AudioQualityAssessment::BandLimited,
            effective_bandwidth_hz: Some(16_000),
            confidence,
            cliff_db: Some(24.0),
            window_agreement_percent: 80,
            active_windows: 8,
            analyzed_frames: FFT_SIZE as u64,
        }
        .into_report(&request)
    }

    fn stereo_f32le(left: &[f32], right: &[f32]) -> Vec<u8> {
        assert_eq!(left.len(), right.len());
        left.iter()
            .zip(right)
            .flat_map(|(&left, &right)| left.to_le_bytes().into_iter().chain(right.to_le_bytes()))
            .collect()
    }

    #[test]
    fn silence_is_insufficient_instead_of_being_called_low_quality() {
        let report = analyze_samples(&vec![0.0; FFT_SIZE * 3], DeclaredEncoding::Unknown);

        assert_eq!(
            report.assessment(),
            AudioQualityAssessment::InsufficientSignal
        );
        assert_eq!(report.effective_bandwidth_hz(), None);
    }

    #[test]
    fn seeded_full_band_noise_has_no_suspicious_cutoff() {
        let report = analyze_samples(&periodic_noise(23_000.0, None), DeclaredEncoding::Lossless);

        assert_eq!(
            report.assessment(),
            AudioQualityAssessment::NoSuspiciousSignature
        );
        assert_eq!(report.effective_bandwidth_hz(), None);
    }

    #[test]
    fn full_band_audio_resuming_above_a_deep_notch_is_not_a_cutoff() {
        let report = analyze_samples_with_rate(
            &periodic_noise_with_notch(16_000.0, 16_500.0),
            DeclaredEncoding::Lossless,
            Some(48_000),
        );

        assert_eq!(
            report.assessment(),
            AudioQualityAssessment::NoSuspiciousSignature
        );
        assert_eq!(report.effective_bandwidth_hz(), None);
    }

    #[test]
    fn repeatable_sixteen_kilohertz_cliff_is_reported_as_measured_evidence() {
        let report = analyze_samples_with_rate(
            &periodic_noise(16_000.0, None),
            DeclaredEncoding::Lossless,
            Some(48_000),
        );

        assert_eq!(report.assessment(), AudioQualityAssessment::BandLimited);
        assert!(
            report
                .effective_bandwidth_hz()
                .is_some_and(|cutoff| { (15_500..=16_500).contains(&cutoff) })
        );
        assert!(report.window_agreement_percent() >= 70);
        assert!(
            report
                .cliff_db()
                .is_some_and(|cliff| cliff.is_finite() && (18.0..=120.0).contains(&cliff))
        );
        assert!(report.interpretation().contains("possible lossy ancestry"));
    }

    #[test]
    fn declared_bitrate_does_not_turn_a_cutoff_into_a_recovered_source_bitrate() {
        let samples = periodic_noise(16_000.0, None);
        let nominal_128 = analyze_samples_with_rate(
            &samples,
            DeclaredEncoding::Lossy {
                bitrate_kbps: Some(128),
            },
            Some(48_000),
        );
        let nominal_320 = analyze_samples_with_rate(
            &samples,
            DeclaredEncoding::Lossy {
                bitrate_kbps: Some(320),
            },
            Some(48_000),
        );

        assert_eq!(
            nominal_128.assessment(),
            AudioQualityAssessment::BandLimited
        );
        assert_eq!(
            nominal_320.assessment(),
            AudioQualityAssessment::BandLimited
        );
        assert!(
            nominal_320
                .interpretation()
                .contains("current lossy encode")
        );
    }

    #[test]
    fn evidence_strength_never_promotes_a_bitrate_provenance_claim() {
        let one_kbps_gap = bandlimited_fixture_report(161, AudioQualityConfidence::High, 2);
        let low_confidence = bandlimited_fixture_report(320, AudioQualityConfidence::Low, 2);
        let boundary = bandlimited_fixture_report(192, AudioQualityConfidence::Medium, 2);

        assert_eq!(
            one_kbps_gap.assessment(),
            AudioQualityAssessment::BandLimited
        );
        assert_eq!(
            low_confidence.assessment(),
            AudioQualityAssessment::BandLimited
        );
        assert_eq!(boundary.assessment(), AudioQualityAssessment::BandLimited);
    }

    #[test]
    fn multichannel_downmix_marks_the_cutoff_inconclusive() {
        let report = bandlimited_fixture_report(320, AudioQualityConfidence::High, 6);

        assert_eq!(
            report.assessment(),
            AudioQualityAssessment::MultichannelDownmix
        );
        assert_eq!(report.source_channels(), Some(6));
        assert!(report.interpretation().contains("multichannel"));
    }

    #[test]
    fn observed_codec_calibration_cutoffs_remain_qualitative() {
        // Representative FFmpeg outputs from Opus, Vorbis, MP2, AAC, and AC-3
        // calibration runs. Their cutoff-to-bitrate behaviour differs enough
        // that none may escape as a codec-neutral numeric source estimate.
        for cutoff_hz in [20_250, 18_700, 12_500, 16_000, 20_250] {
            let identity = AudioQualityIdentity {
                length: 1,
                modified: None,
                created: None,
                filesystem: None,
            };
            let request = AudioQualityRequest::new(PathBuf::from("fixture.lossy"), identity)
                .with_declared_encoding(DeclaredEncoding::Lossy {
                    bitrate_kbps: Some(320),
                })
                .with_source_sample_rate_hz(48_000)
                .with_source_channels(2);
            let report = SpectralSummary {
                assessment: AudioQualityAssessment::BandLimited,
                effective_bandwidth_hz: Some(cutoff_hz),
                confidence: AudioQualityConfidence::High,
                cliff_db: Some(40.0),
                window_agreement_percent: 100,
                active_windows: 10,
                analyzed_frames: FFT_SIZE as u64,
            }
            .into_report(&request);

            assert_eq!(report.assessment(), AudioQualityAssessment::BandLimited);
        }
    }

    #[test]
    fn missing_sample_rate_keeps_the_measured_cutoff_but_marks_it_inconclusive() {
        let report = analyze_samples(
            &periodic_noise(16_000.0, None),
            DeclaredEncoding::Lossy {
                bitrate_kbps: Some(320),
            },
        );

        assert_eq!(
            report.assessment(),
            AudioQualityAssessment::SampleRateUnavailable
        );
        assert!(
            report
                .effective_bandwidth_hz()
                .is_some_and(|cutoff| (15_500..=16_500).contains(&cutoff))
        );
        assert!(
            report
                .interpretation()
                .contains("sample rate is unavailable")
        );
    }

    #[test]
    fn missing_channel_count_keeps_the_cutoff_but_suppresses_interpretation() {
        let identity = AudioQualityIdentity {
            length: 1,
            modified: None,
            created: None,
            filesystem: None,
        };
        let request = AudioQualityRequest::new(PathBuf::from("fixture.flac"), identity)
            .with_declared_encoding(DeclaredEncoding::Lossless)
            .with_source_sample_rate_hz(48_000);
        let report = SpectralSummary {
            assessment: AudioQualityAssessment::BandLimited,
            effective_bandwidth_hz: Some(16_000),
            confidence: AudioQualityConfidence::High,
            cliff_db: Some(40.0),
            window_agreement_percent: 100,
            active_windows: 10,
            analyzed_frames: FFT_SIZE as u64,
        }
        .into_report(&request);

        assert_eq!(
            report.assessment(),
            AudioQualityAssessment::ChannelCountUnavailable
        );
        assert_eq!(report.effective_bandwidth_hz(), Some(16_000));
        assert!(report.interpretation().contains("channel count"));
    }

    #[test]
    fn lossless_32_kilohertz_nyquist_is_not_called_lossy_ancestry() {
        let report = analyze_samples_with_rate(
            &periodic_noise(16_000.0, None),
            DeclaredEncoding::Lossless,
            Some(32_000),
        );

        assert_eq!(
            report.assessment(),
            AudioQualityAssessment::SampleRateLimited
        );
        assert_eq!(report.source_sample_rate_hz(), Some(32_000));
        assert!(report.interpretation().contains("sample-rate limit"));
    }

    #[test]
    fn nominal_320_at_32_kilohertz_is_recognized_as_sample_rate_limited() {
        let report = analyze_samples_with_rate(
            &periodic_noise(16_000.0, None),
            DeclaredEncoding::Lossy {
                bitrate_kbps: Some(320),
            },
            Some(32_000),
        );

        assert_eq!(
            report.assessment(),
            AudioQualityAssessment::SampleRateLimited
        );
    }

    #[test]
    fn gentle_rolloff_is_inconclusive_instead_of_being_called_lossy() {
        let report = analyze_samples(
            &periodic_noise(23_000.0, Some(8_000.0)),
            DeclaredEncoding::Lossless,
        );

        assert_eq!(report.assessment(), AudioQualityAssessment::Inconclusive);
    }

    #[test]
    fn fragmented_float_pcm_is_streamed_without_losing_alignment() {
        struct ShortRead<R> {
            inner: R,
            maximum: usize,
        }
        impl<R: Read> Read for ShortRead<R> {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let maximum = buffer.len().min(self.maximum);
                self.inner.read(&mut buffer[..maximum])
            }
        }
        let samples = periodic_noise(16_000.0, None);
        let bytes = stereo_f32le(&samples, &samples);
        let contiguous =
            analyze_f32le_pcm(Cursor::new(&bytes), samples.len() as u64).expect("contiguous PCM");
        let fragmented = analyze_f32le_pcm(
            ShortRead {
                inner: Cursor::new(&bytes),
                maximum: 1,
            },
            samples.len() as u64,
        )
        .expect("one-byte reads");

        assert_eq!(
            fragmented.effective_bandwidth_hz,
            contiguous.effective_bandwidth_hz
        );
        assert!(fragmented.window_agreement_percent >= 70);
    }

    #[test]
    fn opposite_phase_stereo_high_frequencies_are_not_cancelled_by_a_downmix() {
        let left = periodic_noise(23_000.0, None);
        let right: Vec<f32> = left.iter().map(|sample| -*sample).collect();
        let bytes = stereo_f32le(&left, &right);

        let summary = analyze_f32le_pcm(Cursor::new(bytes), left.len() as u64)
            .expect("valid opposite-phase stereo PCM");

        assert_eq!(
            summary.assessment,
            AudioQualityAssessment::NoSuspiciousSignature
        );
    }

    #[test]
    fn parser_rejects_partial_non_finite_and_over_limit_pcm() {
        assert!(matches!(
            analyze_f32le_pcm(Cursor::new([0_u8; 3]), 10),
            Err(PcmAnalysisError::IncompleteSample)
        ));
        assert!(matches!(
            analyze_f32le_pcm(Cursor::new(f32::NAN.to_le_bytes()), 10),
            Err(PcmAnalysisError::NonFiniteSample)
        ));
        assert!(matches!(
            analyze_f32le_pcm(Cursor::new(0.0_f32.to_le_bytes()), 10),
            Err(PcmAnalysisError::IncompleteFrame)
        ));
        let two_frames: Vec<u8> = [0.0_f32, 0.0, 0.0, 0.0]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        assert!(matches!(
            analyze_f32le_pcm(Cursor::new(two_frames), 1),
            Err(PcmAnalysisError::OutputLimitExceeded)
        ));
    }

    #[test]
    fn command_is_shell_free_private_and_resource_bounded() {
        let analyzer = FfmpegAudioQualityAnalyzer::new("custom ffmpeg");
        let path = Path::new("/music/name with `shell` syntax.flac");
        let command = analyzer.command(path).expect("FFmpeg command");
        let arguments: Vec<&OsStr> = command.get_args().collect();

        assert_eq!(command.get_program(), OsStr::new("custom ffmpeg"));
        assert!(
            arguments
                .windows(2)
                .any(|pair| { pair[0] == OsStr::new("-i") && pair[1] == path.as_os_str() })
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| { pair[0] == OsStr::new("-t") && pair[1] == OsStr::new("30.000000") })
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| { pair[0] == OsStr::new("-ar") && pair[1] == OsStr::new("48000") })
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| { pair[0] == OsStr::new("-ac") && pair[1] == OsStr::new("2") })
        );
    }

    #[test]
    fn command_absolutizes_a_protocol_like_relative_filename() {
        let analyzer = FfmpegAudioQualityAnalyzer::new("custom ffmpeg");
        let path = Path::new("concat:literal audio.wav");
        let command = analyzer.command(path).expect("FFmpeg command");
        let arguments: Vec<&OsStr> = command.get_args().collect();
        let input = arguments
            .windows(2)
            .find_map(|pair| (pair[0] == OsStr::new("-i")).then_some(pair[1]))
            .expect("FFmpeg input path");

        assert!(Path::new(input).is_absolute());
        assert!(Path::new(input).ends_with(path));
    }

    #[test]
    fn limits_reject_zero_and_unbounded_work() {
        assert_eq!(
            FfmpegAudioQualityAnalyzer::with_limits(
                "ffmpeg",
                AudioQualityLimits {
                    maximum_audio_duration: Duration::ZERO,
                    timeout: Duration::from_secs(1),
                }
            )
            .expect_err("zero duration"),
            AudioQualityConfigurationError::ZeroAudioDuration
        );
        assert_eq!(
            FfmpegAudioQualityAnalyzer::with_limits(
                "ffmpeg",
                AudioQualityLimits {
                    maximum_audio_duration: Duration::from_mins(3),
                    timeout: Duration::from_secs(1),
                }
            )
            .expect_err("fixed duration ceiling"),
            AudioQualityConfigurationError::AudioDurationTooLong
        );
    }

    #[test]
    fn cancelled_request_does_not_spawn_a_missing_helper() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let audio = directory.path().join("audio.flac");
        fs::write(&audio, b"fixture").expect("fixture");
        let request = AudioQualityRequest::from_path(audio).expect("request");
        let cancellation = AudioQualityCancellation::new();
        cancellation.cancel();
        let analyzer = FfmpegAudioQualityAnalyzer::new("missing-youta-audio-quality-helper");

        assert!(matches!(
            analyzer.analyze(&request, &cancellation),
            Err(AudioQualityError::Cancelled)
        ));
    }

    #[test]
    fn missing_ffmpeg_has_an_actionable_stable_error() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let audio = directory.path().join("audio.flac");
        fs::write(&audio, b"fixture").expect("fixture");
        let request = AudioQualityRequest::from_path(audio).expect("request");
        let analyzer = FfmpegAudioQualityAnalyzer::new(directory.path().join("missing ffmpeg"));

        let error = analyzer
            .analyze(&request, &AudioQualityCancellation::new())
            .expect_err("missing helper");
        assert!(error.is_missing_executable());
        assert!(error.to_string().contains("install"));
    }

    #[cfg(unix)]
    fn executable_script(path: &Path, source: &str) {
        fs::write(path, source).expect("mock helper");
        let mut permissions = fs::metadata(path).expect("mock metadata").permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).expect("executable helper");
    }

    /// Waits until one Linux fixture process has exited or become a zombie.
    #[cfg(target_os = "linux")]
    fn assert_process_terminated(pid: &str) {
        let stat_path = PathBuf::from(format!("/proc/{pid}/stat"));
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match fs::read_to_string(&stat_path) {
                Ok(stat) => {
                    let state = stat
                        .rsplit_once(") ")
                        .and_then(|(_, fields)| fields.chars().next())
                        .expect("Linux process stat contains a state");
                    if state == 'Z' {
                        return;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "audio-quality helper descendant survived in state {state}"
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => return,
                Err(error) => panic!("read audio-quality descendant state: {error}"),
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(unix)]
    #[test]
    fn mock_ffmpeg_pcm_reaches_the_streaming_analyzer() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let audio = directory.path().join("audio with spaces.flac");
        fs::write(&audio, b"fixture").expect("fixture");
        let helper = directory.path().join("mock ffmpeg");
        executable_script(&helper, "#!/bin/sh\nhead -c 98304 /dev/zero\n");
        let request = AudioQualityRequest::from_path(audio).expect("request");
        let analyzer = FfmpegAudioQualityAnalyzer::new(helper);

        let report = analyzer
            .analyze(&request, &AudioQualityCancellation::new())
            .expect("mock PCM analysis");
        assert_eq!(
            report.assessment(),
            AudioQualityAssessment::InsufficientSignal
        );
        assert_eq!(report.analyzed_frames(), 12_288);
    }

    #[cfg(unix)]
    #[test]
    fn helper_deadline_is_enforced_and_the_child_is_reaped() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let audio = directory.path().join("audio.flac");
        fs::write(&audio, b"fixture").expect("fixture");
        let helper = directory.path().join("sleeping ffmpeg");
        executable_script(&helper, "#!/bin/sh\nexec sleep 30\n");
        let request = AudioQualityRequest::from_path(audio).expect("request");
        let analyzer = FfmpegAudioQualityAnalyzer::with_limits(
            helper,
            AudioQualityLimits {
                maximum_audio_duration: Duration::from_secs(1),
                timeout: Duration::from_millis(60),
            },
        )
        .expect("limits");

        let started = Instant::now();
        assert!(matches!(
            analyzer.analyze(&request, &AudioQualityCancellation::new()),
            Err(AudioQualityError::TimedOut)
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn timeout_terminates_a_wrapper_descendant_holding_pcm_stdout() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let audio = directory.path().join("audio.flac");
        fs::write(&audio, b"fixture").expect("fixture");
        let marker = directory.path().join("descendant.pid");
        let helper = directory.path().join("wrapper ffmpeg");
        executable_script(
            &helper,
            &format!(
                "#!/bin/sh\nsleep 30 &\nprintf '%s\\n' \"$!\" > '{}'\nexit 0\n",
                marker.display()
            ),
        );
        let request = AudioQualityRequest::from_path(audio).expect("request");
        let analyzer = FfmpegAudioQualityAnalyzer::with_limits(
            helper,
            AudioQualityLimits {
                maximum_audio_duration: Duration::from_secs(1),
                timeout: Duration::from_millis(100),
            },
        )
        .expect("limits");

        assert!(matches!(
            analyzer.analyze(&request, &AudioQualityCancellation::new()),
            Err(AudioQualityError::TimedOut)
        ));
        let descendant = fs::read_to_string(marker).expect("descendant PID marker");
        assert_process_terminated(descendant.trim());
    }

    #[cfg(unix)]
    #[test]
    fn active_analysis_can_be_cancelled_promptly() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let audio = directory.path().join("audio.flac");
        fs::write(&audio, b"fixture").expect("fixture");
        let helper = directory.path().join("waiting ffmpeg");
        executable_script(&helper, "#!/bin/sh\nexec sleep 30\n");
        let request = AudioQualityRequest::from_path(audio).expect("request");
        let analyzer = FfmpegAudioQualityAnalyzer::new(helper);
        let cancellation = AudioQualityCancellation::new();
        let worker_cancellation = cancellation.clone();
        let worker = thread::spawn(move || analyzer.analyze(&request, &worker_cancellation));

        thread::sleep(Duration::from_millis(60));
        let started = Instant::now();
        cancellation.cancel();

        assert!(matches!(
            worker.join().expect("analysis worker"),
            Err(AudioQualityError::Cancelled)
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn replacing_the_file_while_ffmpeg_runs_discards_the_report() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let audio = directory.path().join("audio.flac");
        fs::write(&audio, b"original").expect("fixture");
        let helper = directory.path().join("delayed ffmpeg");
        executable_script(&helper, "#!/bin/sh\nsleep 0.2\nhead -c 98304 /dev/zero\n");
        let request = AudioQualityRequest::from_path(audio.clone()).expect("request");
        let analyzer = FfmpegAudioQualityAnalyzer::new(helper);
        let worker =
            thread::spawn(move || analyzer.analyze(&request, &AudioQualityCancellation::new()));

        thread::sleep(Duration::from_millis(60));
        let replacement = directory.path().join("replacement.flac");
        fs::write(&replacement, b"replaced").expect("replacement");
        fs::rename(replacement, &audio).expect("publish replacement");

        assert!(matches!(
            worker.join().expect("analysis worker"),
            Err(AudioQualityError::FileChanged)
        ));
    }

    #[test]
    fn target_collection_accepts_one_explicit_audio_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let audio = directory.path().join("track.flac");
        fs::write(&audio, b"fixture").expect("audio fixture");

        let targets = collect_audio_quality_targets(
            std::slice::from_ref(&audio),
            AudioQualityTargetLimits::default(),
            &AudioQualityCancellation::new(),
        )
        .expect("audio target");

        assert_eq!(targets, [audio]);
    }

    #[test]
    fn target_collection_recurses_and_returns_native_path_order() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path().join("library");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).expect("nested directory");
        let first = root.join("a.mp3");
        let middle = nested.join("middle.FLAC");
        let last = root.join("z.wav");
        fs::write(&last, b"audio").expect("last audio");
        fs::write(&middle, b"audio").expect("middle audio");
        fs::write(&first, b"audio").expect("first audio");
        fs::write(root.join("cover.png"), b"image").expect("image");
        fs::write(root.join("notes.txt"), b"text").expect("text");

        let targets = collect_audio_quality_targets(
            &[root],
            AudioQualityTargetLimits::default(),
            &AudioQualityCancellation::new(),
        )
        .expect("recursive targets");

        let mut expected = vec![first, middle, last];
        expected.sort();
        assert_eq!(targets, expected);
    }

    #[test]
    fn mixed_marked_files_and_folders_are_deduplicated() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path().join("library");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).expect("nested directory");
        let first = root.join("first.mp3");
        let second = nested.join("second.ogg");
        fs::write(&first, b"audio").expect("first audio");
        fs::write(&second, b"audio").expect("second audio");

        let targets = collect_audio_quality_targets(
            &[first.clone(), root, nested, first.clone(), second.clone()],
            AudioQualityTargetLimits::default(),
            &AudioQualityCancellation::new(),
        )
        .expect("deduplicated targets");

        assert_eq!(targets, [first, second]);
    }

    #[test]
    fn hard_link_aliases_are_analyzed_once_using_the_first_sorted_path() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let first = directory.path().join("a.flac");
        let alias = directory.path().join("z.flac");
        fs::write(&first, b"one filesystem file").expect("audio");
        fs::hard_link(&first, &alias).expect("hard-link alias");

        let targets = collect_audio_quality_targets(
            &[alias, first.clone()],
            AudioQualityTargetLimits::default(),
            &AudioQualityCancellation::new(),
        )
        .expect("deduplicated hard link");

        assert_eq!(targets, [first]);
    }

    #[cfg(unix)]
    #[test]
    fn target_collection_never_follows_file_or_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temporary directory");
        let outside = tempfile::tempdir().expect("outside directory");
        let outside_audio = outside.path().join("outside.mp3");
        fs::write(&outside_audio, b"outside audio").expect("outside audio");
        let root = directory.path().join("library");
        fs::create_dir(&root).expect("library");
        let real_audio = root.join("inside.flac");
        fs::write(&real_audio, b"inside audio").expect("inside audio");
        let file_link = root.join("linked.mp3");
        let directory_link = root.join("linked-directory");
        symlink(&outside_audio, &file_link).expect("file symlink");
        symlink(outside.path(), &directory_link).expect("directory symlink");

        let targets = collect_audio_quality_targets(
            &[root, file_link, directory_link],
            AudioQualityTargetLimits::default(),
            &AudioQualityCancellation::new(),
        )
        .expect("symlinks skipped");

        assert_eq!(targets, [real_audio]);
    }

    #[cfg(unix)]
    #[test]
    fn one_file_request_rejects_a_symbolic_link() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temporary directory");
        let target = directory.path().join("target.flac");
        let link = directory.path().join("link.flac");
        fs::write(&target, b"audio").expect("target audio");
        symlink(&target, &link).expect("audio symlink");

        assert!(matches!(
            AudioQualityRequest::from_path(link),
            Err(AudioQualityError::NotRegularFile)
        ));
    }

    #[test]
    fn target_collection_reports_each_resource_limit_without_partial_output() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path().join("library");
        let child = root.join("child");
        let grandchild = child.join("grandchild");
        fs::create_dir_all(&grandchild).expect("deep directory");
        fs::write(root.join("one.mp3"), b"audio").expect("first audio");
        fs::write(root.join("two.mp3"), b"audio").expect("second audio");
        fs::write(grandchild.join("deep.flac"), b"audio").expect("deep audio");
        let cancellation = AudioQualityCancellation::new();

        assert!(matches!(
            collect_audio_quality_targets(
                std::slice::from_ref(&root),
                AudioQualityTargetLimits {
                    maximum_inspected_entries: 2,
                    ..AudioQualityTargetLimits::default()
                },
                &cancellation,
            ),
            Err(AudioQualityTargetCollectionError::InspectedEntryLimitReached { maximum: 2 })
        ));
        assert!(matches!(
            collect_audio_quality_targets(
                std::slice::from_ref(&root),
                AudioQualityTargetLimits {
                    maximum_audio_files: 1,
                    ..AudioQualityTargetLimits::default()
                },
                &cancellation,
            ),
            Err(AudioQualityTargetCollectionError::AudioFileLimitReached { maximum: 1 })
        ));
        assert!(matches!(
            collect_audio_quality_targets(
                &[root],
                AudioQualityTargetLimits {
                    maximum_depth: 1,
                    ..AudioQualityTargetLimits::default()
                },
                &cancellation,
            ),
            Err(AudioQualityTargetCollectionError::DepthLimitReached { maximum: 1, .. })
        ));
    }

    #[test]
    fn cancelled_target_collection_does_not_inspect_missing_roots() {
        let cancellation = AudioQualityCancellation::new();
        cancellation.cancel();

        assert!(matches!(
            collect_audio_quality_targets(
                &[PathBuf::from("missing-target")],
                AudioQualityTargetLimits::default(),
                &cancellation,
            ),
            Err(AudioQualityTargetCollectionError::Cancelled)
        ));
    }

    #[test]
    fn invalid_limits_and_inspection_failures_are_typed() {
        assert!(matches!(
            collect_audio_quality_targets(
                &[],
                AudioQualityTargetLimits {
                    maximum_audio_files: 0,
                    ..AudioQualityTargetLimits::default()
                },
                &AudioQualityCancellation::new(),
            ),
            Err(AudioQualityTargetCollectionError::InvalidLimits)
        ));
        let missing = PathBuf::from("missing-audio-quality-target");
        assert!(matches!(
            collect_audio_quality_targets(
                std::slice::from_ref(&missing),
                AudioQualityTargetLimits::default(),
                &AudioQualityCancellation::new(),
            ),
            Err(AudioQualityTargetCollectionError::Inspect { path, .. }) if path == missing
        ));
    }

    #[test]
    fn target_collection_detects_a_directory_replaced_during_traversal() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path().join("library");
        fs::create_dir(&root).expect("library");
        fs::write(root.join("track.mp3"), b"audio").expect("audio");
        let displaced = directory.path().join("displaced-library");
        let mut replaced = false;

        let error = collect_audio_quality_targets_with_hook(
            std::slice::from_ref(&root),
            AudioQualityTargetLimits::default(),
            &AudioQualityCancellation::new(),
            |directory| {
                if !replaced && directory == root {
                    fs::rename(directory, &displaced).expect("move traversed directory");
                    fs::create_dir(directory).expect("replacement directory");
                    replaced = true;
                }
            },
        )
        .expect_err("unstable traversal");

        assert!(matches!(
            error,
            AudioQualityTargetCollectionError::TargetChanged(path) if path == root
        ));
    }

    #[test]
    fn target_collection_revalidates_every_previously_traversed_directory() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path().join("library");
        let earlier = root.join("a-earlier");
        let later = root.join("z-later");
        fs::create_dir_all(&earlier).expect("earlier directory");
        fs::create_dir_all(&later).expect("later directory");
        fs::write(earlier.join("known.mp3"), b"known audio").expect("known audio");
        let late_audio = earlier.join("appeared-late.flac");

        let error = collect_audio_quality_targets_with_hook(
            std::slice::from_ref(&root),
            AudioQualityTargetLimits::default(),
            &AudioQualityCancellation::new(),
            |traversed| {
                if traversed == later && !late_audio.exists() {
                    fs::write(&late_audio, b"late audio").expect("late audio");
                }
            },
        )
        .expect_err("a completed traversal must reject a later directory change");

        assert!(matches!(
            error,
            AudioQualityTargetCollectionError::TargetChanged(path) if path == earlier
        ));
    }

    #[test]
    fn analyzer_trait_is_mockable_without_ffmpeg() {
        struct MockAnalyzer;
        impl AudioQualityAnalyzer for MockAnalyzer {
            fn analyze(
                &self,
                request: &AudioQualityRequest,
                _cancellation: &AudioQualityCancellation,
            ) -> Result<AudioQualityReport, AudioQualityError> {
                Ok(AudioQualityReport {
                    identity: request.identity.clone(),
                    declared_encoding: request.declared_encoding,
                    source_sample_rate_hz: request.source_sample_rate_hz,
                    source_channels: request.source_channels,
                    assessment: AudioQualityAssessment::Inconclusive,
                    effective_bandwidth_hz: None,
                    confidence: AudioQualityConfidence::Low,
                    cliff_db: None,
                    window_agreement_percent: 0,
                    active_windows: 4,
                    analyzed_frames: FFT_SIZE as u64,
                })
            }
        }
        let directory = tempfile::tempdir().expect("temporary directory");
        let audio = directory.path().join("audio.flac");
        fs::write(&audio, b"fixture").expect("fixture");
        let request = AudioQualityRequest::from_path(audio).expect("request");

        assert!(
            MockAnalyzer
                .analyze(&request, &AudioQualityCancellation::new())
                .is_ok()
        );
    }
}
