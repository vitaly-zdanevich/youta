//! Safe, no-overwrite moves for entries selected in the Local browser.
//!
//! A move is validated as a complete batch before the first source changes.
//! Same-filesystem moves use the operating system's atomic no-replace rename
//! primitive. Cross-filesystem moves copy into a private destination-side
//! staging path, compare the complete copy with the source, publish it with a
//! no-replace rename, and only then detach and delete the source.
//!
//! Symbolic links are never followed. A symbolic link selected as a source is
//! rejected. A directory containing a symbolic link can be renamed on one
//! filesystem because the rename does not traverse it; a cross-filesystem
//! copy rejects that directory and leaves it untouched.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use crate::domain::{MediaId, SourceKind};

const COPY_BUFFER_BYTES: usize = 64 * 1024;
const HIDDEN_NAME_ATTEMPTS: usize = 128;
static HIDDEN_NAME_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Resource limits for one explicitly requested Local move.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalMoveLimits {
    /// Maximum number of directly selected entries in one batch.
    pub max_sources: usize,
    /// Maximum number of files and directories inspected for one source.
    pub max_tree_entries: usize,
    /// Maximum directory depth inspected during a cross-filesystem copy.
    pub max_depth: usize,
}

impl Default for LocalMoveLimits {
    fn default() -> Self {
        Self {
            max_sources: 10_000,
            max_tree_entries: 1_000_000,
            max_depth: 256,
        }
    }
}

/// Resource limits for one destination-chooser directory listing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalMoveDestinationLimits {
    /// Maximum number of raw immediate children inspected.
    pub max_inspected_entries: usize,
    /// Maximum number of real child directories returned.
    pub max_visible_directories: usize,
}

impl Default for LocalMoveDestinationLimits {
    fn default() -> Self {
        Self {
            max_inspected_entries: 100_000,
            max_visible_directories: 10_000,
        }
    }
}

/// One real immediate child shown by the Local move destination chooser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalMoveDestination {
    /// Exact filesystem basename, including non-UTF-8 names on Unix.
    pub name: OsString,
    /// Absolute path formed from the canonical listed directory and basename.
    pub path: PathBuf,
}

/// A bounded, directory-only destination-chooser snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalMoveDestinationListing {
    /// Canonical absolute directory being listed.
    pub path: PathBuf,
    /// Canonical parent directory, or `None` at a filesystem root.
    pub parent: Option<PathBuf>,
    /// Sorted real immediate child directories.
    pub directories: Vec<LocalMoveDestination>,
    /// Whether either listing bound prevented a complete result.
    pub truncated: bool,
    /// Number of raw immediate children inspected.
    pub inspected_entries: usize,
}

/// One source path and its final path after a successful move.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LocalMoveMapping {
    /// Canonical absolute source path.
    pub source: PathBuf,
    /// Canonical absolute destination path.
    pub target: PathBuf,
}

/// A validated Local move that has not mutated the filesystem yet.
///
/// Plans retain source identities. [`execute_local_move`] rechecks every
/// identity and collision before moving the first entry, so an asynchronous UI
/// worker cannot act on a stale directory listing.
#[derive(Clone, Debug)]
pub struct LocalMovePlan {
    source_directory: PathBuf,
    destination_directory: PathBuf,
    entries: Vec<PlannedEntry>,
    limits: LocalMoveLimits,
}

impl LocalMovePlan {
    /// Returns the canonical folder containing every selected source.
    #[must_use]
    pub fn source_directory(&self) -> &Path {
        &self.source_directory
    }

    /// Returns the canonical folder that will receive every selected source.
    #[must_use]
    pub fn destination_directory(&self) -> &Path {
        &self.destination_directory
    }

    /// Returns path-prefix mappings suitable for remapping durable identities.
    #[must_use]
    pub fn mappings(&self) -> Vec<LocalMoveMapping> {
        self.entries
            .iter()
            .map(|entry| entry.mapping.clone())
            .collect()
    }
}

#[derive(Clone, Debug)]
struct PlannedEntry {
    mapping: LocalMoveMapping,
    identity: FilesystemIdentity,
}

/// Why a Local move request was rejected before filesystem mutation.
#[derive(Debug, thiserror::Error)]
pub enum LocalMoveValidationError {
    /// At least one resource limit was zero.
    #[error("local move limits must be greater than zero")]
    InvalidLimits,
    /// No sources were selected.
    #[error("select at least one local file or folder to move")]
    EmptyBatch,
    /// The requested batch exceeds its explicit source-count bound.
    #[error("local move selected {selected} entries; the limit is {limit}")]
    TooManySources {
        /// Number of paths supplied by the caller.
        selected: usize,
        /// Configured source-count bound.
        limit: usize,
    },
    /// A browser directory was relative or contained lexical traversal.
    #[error("local move directory must be a normalized absolute path: `{0}`")]
    UnsafeDirectory(PathBuf),
    /// A selected source was relative, `..`, a descendant, or otherwise unsafe.
    #[error("local move source must be one normalized immediate child: `{0}`")]
    UnsafeSource(PathBuf),
    /// A selected source cannot be represented by durable Local media IDs.
    #[error("local move source path is not valid UTF-8: `{0}`")]
    NonUtf8Source(PathBuf),
    /// A destination path cannot be represented by durable Local media IDs.
    #[error("local move target path is not valid UTF-8: `{0}`")]
    NonUtf8Target(PathBuf),
    /// A selected source appeared more than once.
    #[error("local move source was selected more than once: `{0}`")]
    DuplicateSource(PathBuf),
    /// A source or destination is a symbolic link.
    #[error("symbolic-link traversal is disabled for local move path `{0}`")]
    SymbolicLink(PathBuf),
    /// The source or destination does not have the required filesystem type.
    #[error("unsupported local move filesystem entry: `{0}`")]
    UnsupportedEntry(PathBuf),
    /// The destination is the currently open source folder.
    #[error("local move destination is already the current folder: `{0}`")]
    CurrentDirectory(PathBuf),
    /// A directory cannot be moved into itself or one of its descendants.
    #[error("cannot move directory `{source_path}` into its descendant `{destination}`")]
    DescendantDestination {
        /// Selected source directory.
        source_path: PathBuf,
        /// Requested destination directory.
        destination: PathBuf,
    },
    /// A target name already exists, including as a dangling symbolic link.
    #[error("local move target already exists: `{0}`")]
    TargetExists(PathBuf),
    /// Metadata or a canonical path could not be inspected.
    #[error("cannot inspect local move path `{path}`: {source}")]
    Inspect {
        /// Path involved in the failed inspection.
        path: PathBuf,
        /// Operating-system failure.
        #[source]
        source: io::Error,
    },
}

/// A retained path that makes an interrupted cross-filesystem move recoverable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalMoveRecovery {
    /// No destination was published and the original source remains.
    SourceIntact {
        /// Original source path.
        source: PathBuf,
    },
    /// Copying failed before publication and cleanup could not remove staging.
    SourceAndStagingRetained {
        /// Original source path.
        source: PathBuf,
        /// Private destination-side staging path.
        staging: PathBuf,
    },
    /// A verified destination was published but the original source remains.
    PublishedTargetAndSourceRetained {
        /// Original source path.
        source: PathBuf,
        /// Published destination path.
        target: PathBuf,
    },
    /// The move was published and detached, but an old source-side quarantine
    /// could not be deleted.
    PublishedTargetAndQuarantineRetained {
        /// Published destination path.
        target: PathBuf,
        /// Private source-side path containing the redundant old tree.
        quarantine: PathBuf,
    },
}

impl LocalMoveRecovery {
    /// Returns all retained paths that a diagnostic popup should show.
    #[must_use]
    pub fn paths(&self) -> Vec<&Path> {
        match self {
            Self::SourceIntact { source } => vec![source],
            Self::SourceAndStagingRetained { source, staging } => {
                vec![source, staging]
            }
            Self::PublishedTargetAndSourceRetained { source, target } => {
                vec![source, target]
            }
            Self::PublishedTargetAndQuarantineRetained { target, quarantine } => {
                vec![target, quarantine]
            }
        }
    }
}

/// A filesystem failure after validation started executing a batch.
#[derive(Debug, thiserror::Error)]
#[error("cannot move local entry `{source_path}` to `{target_path}`: {cause}")]
pub struct LocalMoveFailure {
    /// Entries completed earlier in the same batch.
    pub completed: Vec<LocalMoveMapping>,
    /// Source path whose move failed.
    pub source_path: PathBuf,
    /// Intended destination path.
    pub target_path: PathBuf,
    /// Filesystem failure.
    #[source]
    pub cause: io::Error,
    /// Paths retained for recovery.
    pub recovery: LocalMoveRecovery,
}

/// Any validation or execution error produced by a Local move.
#[derive(Debug, thiserror::Error)]
pub enum LocalMoveError {
    /// The complete request was rejected without changing any source.
    #[error(transparent)]
    Validation(#[from] LocalMoveValidationError),
    /// Execution failed; inspect its completed mappings and recovery paths.
    #[error(transparent)]
    Execution(Box<LocalMoveFailure>),
}

impl From<LocalMoveFailure> for LocalMoveError {
    fn from(failure: LocalMoveFailure) -> Self {
        Self::Execution(Box::new(failure))
    }
}

/// Result of a completed Local move batch.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LocalMoveReport {
    /// Successfully published source-to-target mappings.
    pub completed: Vec<LocalMoveMapping>,
    /// Redundant, recoverable artifacts whose cleanup failed.
    ///
    /// A move can still complete when its verified destination is published
    /// and the original source has been atomically detached to quarantine.
    pub recovery: Vec<LocalMoveRecovery>,
}

/// Validates a complete batch without changing any filesystem entry.
///
/// Every selected source must be a normalized absolute immediate child of
/// `source_directory`. Sources must exist as real regular files or
/// directories, and `destination_directory` must be a real existing
/// directory. The function rejects duplicates, no-op moves, descendant moves,
/// and all pre-existing destination names.
///
/// # Errors
///
/// Returns [`LocalMoveValidationError`] for unsafe or non-UTF-8 paths, stale or
/// unsupported entries, collisions, descendant moves, or inspection failures.
pub fn validate_local_move(
    source_directory: &Path,
    sources: &[PathBuf],
    destination_directory: &Path,
    limits: LocalMoveLimits,
) -> Result<LocalMovePlan, LocalMoveValidationError> {
    validate_limits(limits)?;
    if sources.is_empty() {
        return Err(LocalMoveValidationError::EmptyBatch);
    }
    if sources.len() > limits.max_sources {
        return Err(LocalMoveValidationError::TooManySources {
            selected: sources.len(),
            limit: limits.max_sources,
        });
    }

    let source_directory = canonical_real_directory(source_directory)?;
    let destination_directory = canonical_real_directory(destination_directory)?;
    if source_directory == destination_directory {
        return Err(LocalMoveValidationError::CurrentDirectory(
            destination_directory,
        ));
    }

    let mut seen = HashSet::with_capacity(sources.len());
    let mut entries = Vec::with_capacity(sources.len());
    for supplied_source in sources {
        if !is_normalized_absolute(supplied_source) {
            return Err(LocalMoveValidationError::UnsafeSource(
                supplied_source.clone(),
            ));
        }
        let basename = supplied_source
            .file_name()
            .ok_or_else(|| LocalMoveValidationError::UnsafeSource(supplied_source.clone()))?;
        let supplied_parent = supplied_source
            .parent()
            .ok_or_else(|| LocalMoveValidationError::UnsafeSource(supplied_source.clone()))?;
        let canonical_parent = crate::fs_path::canonicalize(supplied_parent).map_err(|source| {
            LocalMoveValidationError::Inspect {
                path: supplied_parent.to_owned(),
                source,
            }
        })?;
        if canonical_parent != source_directory {
            return Err(LocalMoveValidationError::UnsafeSource(
                supplied_source.clone(),
            ));
        }

        let source = source_directory.join(basename);
        let target = destination_directory.join(basename);
        validate_persistable_mapping_paths(&source, &target)?;
        if !seen.insert(source.clone()) {
            return Err(LocalMoveValidationError::DuplicateSource(source));
        }
        let metadata = inspected_metadata(&source)?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            return Err(LocalMoveValidationError::SymbolicLink(source));
        }
        if !file_type.is_file() && !file_type.is_dir() {
            return Err(LocalMoveValidationError::UnsupportedEntry(source));
        }
        if file_type.is_dir() && destination_directory.starts_with(&source) {
            return Err(LocalMoveValidationError::DescendantDestination {
                source_path: source,
                destination: destination_directory,
            });
        }

        ensure_target_absent(&target)?;
        entries.push(PlannedEntry {
            mapping: LocalMoveMapping { source, target },
            identity: FilesystemIdentity::from_metadata(&metadata),
        });
    }

    Ok(LocalMovePlan {
        source_directory,
        destination_directory,
        entries,
        limits,
    })
}

/// Executes a previously validated Local move.
///
/// The function revalidates the entire plan before its first mutation. Earlier
/// mappings in a multi-entry batch can already be complete when a later entry
/// encounters an operating-system error; those mappings are returned in
/// [`LocalMoveFailure::completed`].
///
/// # Errors
///
/// Returns [`LocalMoveError`] when a source became stale, a collision appeared,
/// a no-replace rename failed, or a cross-filesystem copy could not be safely
/// verified and published.
pub fn execute_local_move(plan: &LocalMovePlan) -> Result<LocalMoveReport, LocalMoveError> {
    execute_with_renamer(plan, &SystemNoReplaceRenamer)
}

/// Validates and executes one Local move batch.
///
/// This is the production convenience API for an asynchronous application
/// worker. Call [`validate_local_move`] separately when the UI needs to preview
/// the exact mappings before execution.
///
/// # Errors
///
/// Returns [`LocalMoveError`] under the conditions documented by
/// [`validate_local_move`] and [`execute_local_move`].
pub fn move_local_entries(
    source_directory: &Path,
    sources: &[PathBuf],
    destination_directory: &Path,
    limits: LocalMoveLimits,
) -> Result<LocalMoveReport, LocalMoveError> {
    let plan = validate_local_move(source_directory, sources, destination_directory, limits)?;
    execute_local_move(&plan)
}

/// Lists safe destination folders without probing media files or images.
///
/// The result contains only real immediate child directories beneath the
/// canonical selected directory. Symbolic links and every non-directory entry
/// are skipped without being opened. This makes the function suitable for the
/// isolated Local browser worker used by a destination chooser.
///
/// # Errors
///
/// Returns [`LocalMoveValidationError`] when limits are zero, the selected
/// directory is unsafe, symbolic, missing, or not a directory, or its immediate
/// entries cannot be enumerated.
pub fn list_local_move_destinations(
    directory: &Path,
    limits: LocalMoveDestinationLimits,
) -> Result<LocalMoveDestinationListing, LocalMoveValidationError> {
    if limits.max_inspected_entries == 0 || limits.max_visible_directories == 0 {
        return Err(LocalMoveValidationError::InvalidLimits);
    }
    let directory = canonical_real_directory(directory)?;
    let parent = directory
        .parent()
        .map(crate::fs_path::canonicalize)
        .transpose()
        .map_err(|source| LocalMoveValidationError::Inspect {
            path: directory.clone(),
            source,
        })?;
    let iterator =
        fs::read_dir(&directory).map_err(|source| LocalMoveValidationError::Inspect {
            path: directory.clone(),
            source,
        })?;
    let mut inspected_entries = 0_usize;
    let mut truncated = false;
    let mut directories = Vec::new();
    for entry in iterator {
        if inspected_entries >= limits.max_inspected_entries {
            truncated = true;
            break;
        }
        inspected_entries = inspected_entries.saturating_add(1);
        let entry = entry.map_err(|source| LocalMoveValidationError::Inspect {
            path: directory.clone(),
            source,
        })?;
        let file_type = entry
            .file_type()
            .map_err(|source| LocalMoveValidationError::Inspect {
                path: entry.path(),
                source,
            })?;
        if file_type.is_symlink() || !file_type.is_dir() {
            continue;
        }
        if directories.len() >= limits.max_visible_directories {
            truncated = true;
            break;
        }
        let name = entry.file_name();
        directories.push(LocalMoveDestination {
            path: directory.join(&name),
            name,
        });
    }
    directories.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(LocalMoveDestinationListing {
        path: directory,
        parent,
        directories,
        truncated,
        inspected_entries,
    })
}

/// Applies completed move mappings to one stored local filesystem path.
///
/// Exact source paths and descendants of moved directories are remapped. The
/// longest matching source prefix wins, making the helper deterministic even
/// for mappings constructed outside [`LocalMovePlan`]. Non-matching paths
/// return `None`.
#[must_use]
pub fn remap_local_path_prefix(path: &Path, mappings: &[LocalMoveMapping]) -> Option<PathBuf> {
    // The match must not hinge on which of Windows' two spellings each side
    // arrived in. A mapping holds what `fs::canonicalize` said — a `\\?\`
    // verbatim path — while a stored locator decodes flat, and the file it
    // names has just been moved away, so no amount of asking the filesystem
    // can settle the two onto each other. Comparing both sides flat is the
    // only meeting point that still exists.
    let path = flat_spelling(path);
    mappings
        .iter()
        .filter_map(|mapping| {
            let source = flat_spelling(&mapping.source);
            path.strip_prefix(source.as_ref())
                .ok()
                .map(|suffix| (source.components().count(), mapping, suffix))
        })
        .max_by_key(|(components, _, _)| *components)
        .map(|(_, mapping, suffix)| join_relative(&flat_spelling(&mapping.target), suffix))
}

/// Returns `path` without a Windows verbatim prefix, when it carries one.
///
/// `\\?\C:\x` and `C:\x` name the same file, so a comparison between them is a
/// comparison between one file and itself and must succeed. Only the exact
/// spellings [`std::fs::canonicalize`] produces are translated — a verbatim
/// disk or a verbatim UNC share — and only when the path is UTF-8, which every
/// persistable mapping and decoded locator here already is; anything else
/// passes through untouched and compares exactly as it did before.
#[cfg(windows)]
fn flat_spelling(path: &Path) -> std::borrow::Cow<'_, Path> {
    use std::borrow::Cow;
    let Some(text) = path.to_str() else {
        return Cow::Borrowed(path);
    };
    let Some(rest) = text.strip_prefix(r"\\?\") else {
        return Cow::Borrowed(path);
    };
    if let Some(share) = rest.strip_prefix(r"UNC\") {
        Cow::Owned(PathBuf::from(format!(r"\\{share}")))
    } else if rest.as_bytes().get(1) == Some(&b':') {
        Cow::Borrowed(Path::new(rest))
    } else {
        Cow::Borrowed(path)
    }
}

/// Returns `path` unchanged: only Windows spells one file two ways.
///
/// On Unix a leading `\\?\` is not a prefix but four ordinary bytes a file is
/// entitled to be named by, so stripping it here would corrupt a real name.
#[cfg(not(windows))]
fn flat_spelling(path: &Path) -> std::borrow::Cow<'_, Path> {
    std::borrow::Cow::Borrowed(path)
}

/// Applies completed move mappings to a provider-qualified media identity.
///
/// Non-local identities and non-matching local paths remain unchanged. The
/// function returns `true` only when it changed `external_id`.
///
/// # Errors
///
/// Returns [`LocalIdentityRemapError`] if the destination path cannot be
/// represented as an absolute file URL.
pub fn remap_local_media_id(
    media_id: &mut MediaId,
    mappings: &[LocalMoveMapping],
) -> Result<bool, LocalIdentityRemapError> {
    if media_id.source != SourceKind::Local {
        return Ok(false);
    }
    let file_url_identity =
        url::Url::parse(&media_id.external_id).is_ok_and(|url| url.scheme() == "file");
    let Some(path) = local_locator_path(&media_id.external_id) else {
        return Ok(false);
    };
    let Some(remapped) = remap_local_path_prefix(&path, mappings) else {
        return Ok(false);
    };
    media_id.external_id = if !file_url_identity {
        remapped.to_str().map(ToOwned::to_owned).unwrap_or_else(|| {
            url::Url::from_file_path(&remapped)
                .expect("an absolute remapped path must form a file URL")
                .to_string()
        })
    } else {
        url::Url::from_file_path(&remapped)
            .map_err(|()| LocalIdentityRemapError::NonUtf8Destination(remapped))?
            .to_string()
    };
    Ok(true)
}

/// Applies completed move mappings to a stored local replay locator.
///
/// Current file URLs and legacy absolute paths are both accepted. Non-matching
/// locators remain unchanged and return `false`.
///
/// # Errors
///
/// Returns [`LocalIdentityRemapError`] if a non-UTF-8 destination cannot be
/// represented as an absolute file URL.
pub fn remap_local_replay_locator(
    replay_locator: &mut String,
    mappings: &[LocalMoveMapping],
) -> Result<bool, LocalIdentityRemapError> {
    let Some(path) = local_locator_path(replay_locator) else {
        return Ok(false);
    };
    let Some(remapped) = remap_local_path_prefix(&path, mappings) else {
        return Ok(false);
    };
    *replay_locator = match remapped.to_str() {
        Some(path) => path.to_owned(),
        None => url::Url::from_file_path(&remapped)
            .map_err(|()| LocalIdentityRemapError::NonUtf8Destination(remapped.clone()))?
            .to_string(),
    };
    Ok(true)
}

/// A durable Local locator cannot represent the supplied destination path.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LocalIdentityRemapError {
    /// The filesystem path cannot be represented by a persisted file URL.
    #[error("moved local path cannot be represented as a file URL: `{0}`")]
    NonUtf8Destination(PathBuf),
}

/// Decodes a current file-URL or legacy absolute-path Local locator.
fn local_locator_path(locator: &str) -> Option<PathBuf> {
    let path = if let Ok(url) = url::Url::parse(locator)
        && url.scheme() == "file"
    {
        url.to_file_path().ok()?
    } else {
        let path = PathBuf::from(locator);
        if !path.is_absolute() {
            return None;
        }
        path
    };
    Some(settled_local_path(path))
}

/// Returns `path` in the one spelling the mappings below are stated in.
///
/// Windows spells one file two ways: [`std::fs::canonicalize`] answers with a
/// `\\?\` verbatim prefix, and a file URL cannot carry that prefix, so a locator
/// decodes into the other spelling while a mapping holds the canonical one and
/// the prefix match never fires.
///
/// A path naming nothing is returned exactly as it decoded. That is the ordinary
/// case here — remapping runs after the file has already moved away — so the
/// source side of a mapping still has to be matched by the caller in whatever
/// spelling it was stated.
#[cfg(windows)]
fn settled_local_path(path: PathBuf) -> PathBuf {
    crate::fs_path::canonicalize(&path).unwrap_or(path)
}

/// Returns `path` unchanged, because this platform spells a file one way.
///
/// A file URL round trip is already lossless here, and canonicalising would
/// additionally resolve symbolic links, which this module never follows.
#[cfg(not(windows))]
fn settled_local_path(path: PathBuf) -> PathBuf {
    path
}

trait NoReplaceRenamer {
    fn rename_no_replace(&self, source: &Path, target: &Path) -> io::Result<()>;
}

#[derive(Clone, Copy, Debug)]
struct SystemNoReplaceRenamer;

impl NoReplaceRenamer for SystemNoReplaceRenamer {
    fn rename_no_replace(&self, source: &Path, target: &Path) -> io::Result<()> {
        #[cfg(any(
            target_os = "android",
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "tvos",
            target_os = "watchos",
            target_os = "visionos",
            target_os = "redox"
        ))]
        {
            use rustix::fs::{CWD, RenameFlags, renameat_with};

            renameat_with(CWD, source, CWD, target, RenameFlags::NOREPLACE).map_err(io::Error::from)
        }

        #[cfg(not(any(
            target_os = "android",
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "tvos",
            target_os = "watchos",
            target_os = "visionos",
            target_os = "redox"
        )))]
        {
            let metadata = fs::symlink_metadata(source)?;
            // A directory rename on Windows is no-replace by construction:
            // `MoveFileEx` refuses to replace an existing target of either
            // kind with a directory, so the plain rename already carries the
            // whole guarantee. This is Windows-only — POSIX `rename` would
            // quietly replace an *empty* target directory, so the other
            // fallback platforms keep refusing instead of guessing.
            #[cfg(windows)]
            if metadata.file_type().is_dir() {
                return fs::rename(source, target);
            }
            if !metadata.file_type().is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "atomic no-replace directory moves are unavailable on this platform",
                ));
            }
            fs::hard_link(source, target)?;
            if let Err(error) = fs::remove_file(source) {
                let _ = fs::remove_file(target);
                return Err(error);
            }
            Ok(())
        }
    }
}

fn execute_with_renamer<R: NoReplaceRenamer>(
    plan: &LocalMovePlan,
    renamer: &R,
) -> Result<LocalMoveReport, LocalMoveError> {
    revalidate_plan(plan)?;

    let mut report = LocalMoveReport::default();
    for entry in &plan.entries {
        if let Err(cause) = revalidate_entry_for_execution(entry) {
            return Err(LocalMoveFailure {
                completed: report.completed,
                source_path: entry.mapping.source.clone(),
                target_path: entry.mapping.target.clone(),
                cause,
                recovery: LocalMoveRecovery::SourceIntact {
                    source: entry.mapping.source.clone(),
                },
            }
            .into());
        }
        match renamer.rename_no_replace(&entry.mapping.source, &entry.mapping.target) {
            Ok(()) => report.completed.push(entry.mapping.clone()),
            Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
                match copy_publish_and_remove(entry, plan.limits, renamer) {
                    Ok(recovery) => {
                        report.completed.push(entry.mapping.clone());
                        report.recovery.extend(recovery);
                    }
                    Err((cause, recovery)) => {
                        return Err(LocalMoveFailure {
                            completed: report.completed,
                            source_path: entry.mapping.source.clone(),
                            target_path: entry.mapping.target.clone(),
                            cause,
                            recovery,
                        }
                        .into());
                    }
                }
            }
            Err(cause) => {
                return Err(LocalMoveFailure {
                    completed: report.completed,
                    source_path: entry.mapping.source.clone(),
                    target_path: entry.mapping.target.clone(),
                    cause,
                    recovery: LocalMoveRecovery::SourceIntact {
                        source: entry.mapping.source.clone(),
                    },
                }
                .into());
            }
        }
    }
    Ok(report)
}

fn revalidate_plan(plan: &LocalMovePlan) -> Result<(), LocalMoveError> {
    let source_directory = canonical_real_directory(&plan.source_directory)?;
    let destination_directory = canonical_real_directory(&plan.destination_directory)?;
    if source_directory != plan.source_directory
        || destination_directory != plan.destination_directory
    {
        return Err(
            LocalMoveValidationError::UnsafeDirectory(plan.source_directory.clone()).into(),
        );
    }

    for entry in &plan.entries {
        validate_persistable_tree_paths(&entry.mapping.source, &entry.mapping.target, plan.limits)?;
    }
    for entry in &plan.entries {
        let metadata = inspected_metadata(&entry.mapping.source)?;
        if FilesystemIdentity::from_metadata(&metadata) != entry.identity {
            return Err(LocalMoveFailure {
                completed: Vec::new(),
                source_path: entry.mapping.source.clone(),
                target_path: entry.mapping.target.clone(),
                cause: io::Error::new(
                    io::ErrorKind::InvalidData,
                    "selected source changed after it was listed",
                ),
                recovery: LocalMoveRecovery::SourceIntact {
                    source: entry.mapping.source.clone(),
                },
            }
            .into());
        }
        ensure_target_absent(&entry.mapping.target)?;
    }
    Ok(())
}

fn validate_persistable_mapping_paths(
    source: &Path,
    target: &Path,
) -> Result<(), LocalMoveValidationError> {
    if source.to_str().is_none() {
        return Err(LocalMoveValidationError::NonUtf8Source(source.to_owned()));
    }
    if target.to_str().is_none() {
        return Err(LocalMoveValidationError::NonUtf8Target(target.to_owned()));
    }
    Ok(())
}

fn validate_persistable_tree_paths(
    source: &Path,
    target: &Path,
    limits: LocalMoveLimits,
) -> Result<(), LocalMoveValidationError> {
    let mut pending = vec![(source.to_owned(), target.to_owned(), 0_usize)];
    let mut inspected = 0_usize;
    while let Some((source_path, target_path, depth)) = pending.pop() {
        if depth > limits.max_depth {
            return Err(LocalMoveValidationError::Inspect {
                path: source_path,
                source: io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("local move tree exceeds depth limit {}", limits.max_depth),
                ),
            });
        }
        if inspected >= limits.max_tree_entries {
            return Err(LocalMoveValidationError::Inspect {
                path: source_path,
                source: io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "local move tree exceeds entry limit {}",
                        limits.max_tree_entries
                    ),
                ),
            });
        }
        inspected = inspected.saturating_add(1);
        validate_persistable_mapping_paths(&source_path, &target_path)?;
        let metadata = fs::symlink_metadata(&source_path).map_err(|source| {
            LocalMoveValidationError::Inspect {
                path: source_path.clone(),
                source,
            }
        })?;
        if !metadata.file_type().is_dir() {
            continue;
        }
        let iterator =
            fs::read_dir(&source_path).map_err(|source| LocalMoveValidationError::Inspect {
                path: source_path.clone(),
                source,
            })?;
        let mut children = iterator
            .map(|entry| {
                entry.map(|entry| entry.file_name()).map_err(|source| {
                    LocalMoveValidationError::Inspect {
                        path: source_path.clone(),
                        source,
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        children.sort();
        for name in children.into_iter().rev() {
            pending.push((
                source_path.join(&name),
                target_path.join(name),
                depth.saturating_add(1),
            ));
        }
    }
    Ok(())
}

fn revalidate_entry_for_execution(entry: &PlannedEntry) -> io::Result<()> {
    let metadata = fs::symlink_metadata(&entry.mapping.source)?;
    if FilesystemIdentity::from_metadata(&metadata) != entry.identity {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "selected source changed after it was listed",
        ));
    }
    match fs::symlink_metadata(&entry.mapping.target) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "local move target appeared after validation",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn copy_publish_and_remove<R: NoReplaceRenamer>(
    entry: &PlannedEntry,
    limits: LocalMoveLimits,
    renamer: &R,
) -> Result<Vec<LocalMoveRecovery>, (io::Error, LocalMoveRecovery)> {
    let manifest = snapshot_tree(&entry.mapping.source, limits).map_err(|cause| {
        (
            cause,
            LocalMoveRecovery::SourceIntact {
                source: entry.mapping.source.clone(),
            },
        )
    })?;
    let staging = create_staging_root(
        entry
            .mapping
            .target
            .parent()
            .expect("validated move target has a parent"),
        manifest[0].kind,
    )
    .map_err(|cause| {
        (
            cause,
            LocalMoveRecovery::SourceIntact {
                source: entry.mapping.source.clone(),
            },
        )
    })?;

    if let Err(cause) = populate_staging(&entry.mapping.source, &staging, &manifest)
        .and_then(|()| verify_source_manifest(&entry.mapping.source, &manifest, limits))
        .and_then(|()| verify_staged_copy(&entry.mapping.source, &staging, &manifest, limits))
    {
        return Err((
            cause,
            cleanup_staging_recovery(&entry.mapping.source, &staging),
        ));
    }

    if let Err(cause) = renamer.rename_no_replace(&staging, &entry.mapping.target) {
        return Err((
            cause,
            cleanup_staging_recovery(&entry.mapping.source, &staging),
        ));
    }

    if let Err(cause) = verify_source_manifest(&entry.mapping.source, &manifest, limits) {
        return Err((
            cause,
            LocalMoveRecovery::PublishedTargetAndSourceRetained {
                source: entry.mapping.source.clone(),
                target: entry.mapping.target.clone(),
            },
        ));
    }

    let quarantine =
        detach_source_to_quarantine(&entry.mapping.source, renamer).map_err(|cause| {
            (
                cause,
                LocalMoveRecovery::PublishedTargetAndSourceRetained {
                    source: entry.mapping.source.clone(),
                    target: entry.mapping.target.clone(),
                },
            )
        })?;
    if let Err(_cleanup_error) = remove_owned_tree(&quarantine, manifest[0].kind) {
        return Ok(vec![
            LocalMoveRecovery::PublishedTargetAndQuarantineRetained {
                target: entry.mapping.target.clone(),
                quarantine,
            },
        ]);
    }
    Ok(Vec::new())
}

fn cleanup_staging_recovery(source: &Path, staging: &Path) -> LocalMoveRecovery {
    let kind = fs::symlink_metadata(staging)
        .ok()
        .and_then(|metadata| NodeKind::from_metadata(&metadata).ok());
    if kind.is_some_and(|kind| remove_owned_tree(staging, kind).is_err()) || path_lexists(staging) {
        LocalMoveRecovery::SourceAndStagingRetained {
            source: source.to_owned(),
            staging: staging.to_owned(),
        }
    } else {
        LocalMoveRecovery::SourceIntact {
            source: source.to_owned(),
        }
    }
}

fn detach_source_to_quarantine<R: NoReplaceRenamer>(
    source: &Path,
    renamer: &R,
) -> io::Result<PathBuf> {
    let parent = source.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "validated source has no parent directory",
        )
    })?;
    for _ in 0..HIDDEN_NAME_ATTEMPTS {
        let quarantine = hidden_path(parent, "source");
        match renamer.rename_no_replace(source, &quarantine) {
            Ok(()) => return Ok(quarantine),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "cannot allocate a private source quarantine path",
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NodeKind {
    File,
    Directory,
}

impl NodeKind {
    fn from_metadata(metadata: &Metadata) -> io::Result<Self> {
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cross-filesystem local move refuses symbolic links",
            ));
        }
        if file_type.is_file() {
            Ok(Self::File)
        } else if file_type.is_dir() {
            Ok(Self::Directory)
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cross-filesystem local move refuses special files",
            ))
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TreeNode {
    relative: PathBuf,
    kind: NodeKind,
    identity: FilesystemIdentity,
}

fn snapshot_tree(root: &Path, limits: LocalMoveLimits) -> io::Result<Vec<TreeNode>> {
    let mut pending = vec![(root.to_owned(), PathBuf::new(), 0_usize)];
    let mut nodes = Vec::new();
    while let Some((path, relative, depth)) = pending.pop() {
        if depth > limits.max_depth {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("local move tree exceeds depth limit {}", limits.max_depth),
            ));
        }
        if nodes.len() >= limits.max_tree_entries {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "local move tree exceeds entry limit {}",
                    limits.max_tree_entries
                ),
            ));
        }
        let metadata = fs::symlink_metadata(&path)?;
        let kind = NodeKind::from_metadata(&metadata).map_err(|error| {
            io::Error::new(error.kind(), format!("{}: {error}", path.display()))
        })?;
        nodes.push(TreeNode {
            relative: relative.clone(),
            kind,
            identity: FilesystemIdentity::from_metadata(&metadata),
        });

        if kind == NodeKind::Directory {
            let mut children = fs::read_dir(&path)?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<io::Result<Vec<OsString>>>()?;
            children.sort();
            for name in children.into_iter().rev() {
                pending.push((
                    path.join(&name),
                    relative.join(name),
                    depth.saturating_add(1),
                ));
            }
        }
    }
    Ok(nodes)
}

fn create_staging_root(parent: &Path, kind: NodeKind) -> io::Result<PathBuf> {
    for _ in 0..HIDDEN_NAME_ATTEMPTS {
        let staging = hidden_path(parent, "stage");
        let result = match kind {
            NodeKind::File => OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staging)
                .map(drop),
            NodeKind::Directory => fs::create_dir(&staging),
        };
        match result {
            Ok(()) => return Ok(staging),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "cannot allocate a private destination staging path",
    ))
}

fn hidden_path(parent: &Path, role: &str) -> PathBuf {
    let sequence = HIDDEN_NAME_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(
        ".youta-move-{role}-{}-{sequence}.part",
        std::process::id()
    ))
}

fn populate_staging(source: &Path, staging: &Path, manifest: &[TreeNode]) -> io::Result<()> {
    for (index, node) in manifest.iter().enumerate() {
        let source_path = join_relative(source, &node.relative);
        let staging_path = join_relative(staging, &node.relative);
        match node.kind {
            NodeKind::Directory => {
                if index != 0 {
                    fs::create_dir(&staging_path)?;
                }
            }
            NodeKind::File => {
                if index == 0 {
                    copy_file_into_existing(&source_path, &staging_path, &node.identity)?;
                } else {
                    copy_file_new(&source_path, &staging_path, &node.identity)?;
                }
            }
        }
    }

    for node in manifest.iter().rev() {
        let source_path = join_relative(source, &node.relative);
        let staging_path = join_relative(staging, &node.relative);
        let permissions = fs::symlink_metadata(source_path)?.permissions();
        fs::set_permissions(staging_path, permissions)?;
    }
    Ok(())
}

fn copy_file_new(source: &Path, target: &Path, identity: &FilesystemIdentity) -> io::Result<()> {
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    copy_file(source, output, identity)
}

fn copy_file_into_existing(
    source: &Path,
    target: &Path,
    identity: &FilesystemIdentity,
) -> io::Result<()> {
    let output = OpenOptions::new().write(true).open(target)?;
    copy_file(source, output, identity)
}

fn copy_file(source: &Path, output: File, identity: &FilesystemIdentity) -> io::Result<()> {
    let input = File::open(source)?;
    if FilesystemIdentity::from_metadata(&input.metadata()?) != *identity {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("source changed while copying `{}`", source.display()),
        ));
    }
    let mut input = BufReader::with_capacity(COPY_BUFFER_BYTES, input);
    let mut output = BufWriter::with_capacity(COPY_BUFFER_BYTES, output);
    io::copy(&mut input, &mut output)?;
    output.flush()?;
    output.get_ref().sync_all()
}

fn verify_source_manifest(
    source: &Path,
    manifest: &[TreeNode],
    limits: LocalMoveLimits,
) -> io::Result<()> {
    let current = snapshot_tree(source, limits)?;
    if current == manifest {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "source changed during cross-filesystem move `{}`",
                source.display()
            ),
        ))
    }
}

fn verify_staged_copy(
    source: &Path,
    staging: &Path,
    manifest: &[TreeNode],
    limits: LocalMoveLimits,
) -> io::Result<()> {
    let staged = snapshot_tree(staging, limits)?;
    if staged.len() != manifest.len()
        || staged.iter().zip(manifest).any(|(actual, expected)| {
            actual.relative != expected.relative
                || actual.kind != expected.kind
                || (actual.kind == NodeKind::File
                    && actual.identity.length != expected.identity.length)
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "destination staging tree differs from source manifest",
        ));
    }

    for node in manifest.iter().filter(|node| node.kind == NodeKind::File) {
        compare_files(
            &join_relative(source, &node.relative),
            &join_relative(staging, &node.relative),
        )?;
    }
    Ok(())
}

fn compare_files(first: &Path, second: &Path) -> io::Result<()> {
    let mut first = BufReader::with_capacity(COPY_BUFFER_BYTES, File::open(first)?);
    let mut second = BufReader::with_capacity(COPY_BUFFER_BYTES, File::open(second)?);
    let mut first_buffer = vec![0_u8; COPY_BUFFER_BYTES];
    let mut second_buffer = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        let first_read = first.read(&mut first_buffer)?;
        let second_read = second.read(&mut second_buffer)?;
        if first_read != second_read || first_buffer[..first_read] != second_buffer[..second_read] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "copied local media failed byte-for-byte verification",
            ));
        }
        if first_read == 0 {
            return Ok(());
        }
    }
}

fn remove_owned_tree(path: &Path, kind: NodeKind) -> io::Result<()> {
    match kind {
        NodeKind::File => fs::remove_file(path),
        NodeKind::Directory => fs::remove_dir_all(path),
    }
}

fn validate_limits(limits: LocalMoveLimits) -> Result<(), LocalMoveValidationError> {
    if limits.max_sources == 0 || limits.max_tree_entries == 0 || limits.max_depth == 0 {
        Err(LocalMoveValidationError::InvalidLimits)
    } else {
        Ok(())
    }
}

fn canonical_real_directory(path: &Path) -> Result<PathBuf, LocalMoveValidationError> {
    if !is_normalized_absolute(path) {
        return Err(LocalMoveValidationError::UnsafeDirectory(path.to_owned()));
    }
    let metadata = inspected_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(LocalMoveValidationError::SymbolicLink(path.to_owned()));
    }
    if !metadata.file_type().is_dir() {
        return Err(LocalMoveValidationError::UnsupportedEntry(path.to_owned()));
    }
    crate::fs_path::canonicalize(path).map_err(|source| LocalMoveValidationError::Inspect {
        path: path.to_owned(),
        source,
    })
}

fn inspected_metadata(path: &Path) -> Result<Metadata, LocalMoveValidationError> {
    fs::symlink_metadata(path).map_err(|source| LocalMoveValidationError::Inspect {
        path: path.to_owned(),
        source,
    })
}

fn ensure_target_absent(path: &Path) -> Result<(), LocalMoveValidationError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(LocalMoveValidationError::TargetExists(path.to_owned())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(LocalMoveValidationError::Inspect {
            path: path.to_owned(),
            source,
        }),
    }
}

fn path_lexists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn join_relative(root: &Path, relative: &Path) -> PathBuf {
    if relative.as_os_str().is_empty() {
        root.to_owned()
    } else {
        root.join(relative)
    }
}

fn is_normalized_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path.components().all(|component| match component {
            Component::Prefix(_) | Component::RootDir => true,
            // Windows parses a verbatim (`\\?\`) path literally: `/` is not a
            // separator there and `..` is an ordinary name, so a traversal
            // written against a verbatim base arrives as one "normal"
            // component instead of `ParentDir`. No real entry is spelled that
            // way — no filesystem here permits `.`, `..`, or `/` in a name —
            // so the spelling alone marks the path unsafe.
            Component::Normal(name) => {
                let bytes = name.as_encoded_bytes();
                bytes != b".." && bytes != b"." && !bytes.contains(&b'/')
            }
            Component::CurDir | Component::ParentDir => false,
        })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FilesystemIdentity {
    kind: NodeKind,
    length: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
}

impl FilesystemIdentity {
    fn from_metadata(metadata: &Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        Self {
            kind: if metadata.file_type().is_dir() {
                NodeKind::Directory
            } else {
                NodeKind::File
            },
            length: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            changed_seconds: metadata.ctime(),
            #[cfg(unix)]
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use tempfile::TempDir;

    use super::*;

    #[derive(Default)]
    struct ForceCrossDeviceRenamer {
        direct_cross_device_failures: Cell<usize>,
        quarantine_failures: Cell<usize>,
    }

    impl ForceCrossDeviceRenamer {
        fn cross_device_once() -> Self {
            Self {
                direct_cross_device_failures: Cell::new(1),
                quarantine_failures: Cell::new(0),
            }
        }
    }

    impl NoReplaceRenamer for ForceCrossDeviceRenamer {
        fn rename_no_replace(&self, source: &Path, target: &Path) -> io::Result<()> {
            if self.direct_cross_device_failures.get() > 0
                && !source
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(".youta-move-"))
            {
                self.direct_cross_device_failures
                    .set(self.direct_cross_device_failures.get() - 1);
                return Err(io::Error::from(io::ErrorKind::CrossesDevices));
            }
            if self.quarantine_failures.get() > 0
                && target
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().contains("-source-"))
            {
                self.quarantine_failures
                    .set(self.quarantine_failures.get() - 1);
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "mock quarantine denial",
                ));
            }
            SystemNoReplaceRenamer.rename_no_replace(source, target)
        }
    }

    #[derive(Default)]
    struct FailSecondRenamer {
        calls: Cell<usize>,
    }

    impl NoReplaceRenamer for FailSecondRenamer {
        fn rename_no_replace(&self, source: &Path, target: &Path) -> io::Result<()> {
            let call = self.calls.get();
            self.calls.set(call.saturating_add(1));
            if call == 1 {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "mock second-entry denial",
                ))
            } else {
                SystemNoReplaceRenamer.rename_no_replace(source, target)
            }
        }
    }

    #[derive(Default)]
    struct PublishCollisionRenamer {
        calls: Cell<usize>,
    }

    impl NoReplaceRenamer for PublishCollisionRenamer {
        fn rename_no_replace(&self, source: &Path, target: &Path) -> io::Result<()> {
            let call = self.calls.get();
            self.calls.set(call.saturating_add(1));
            if call == 0 {
                return Err(io::Error::from(io::ErrorKind::CrossesDevices));
            }
            if call == 1 {
                fs::write(target, b"racing destination").expect("mock target race");
            }
            SystemNoReplaceRenamer.rename_no_replace(source, target)
        }
    }

    /// Builds a fixture whose paths are all rooted at the *canonical* temporary
    /// directory.
    ///
    /// Moves report canonical sources and destinations, so expectations have to
    /// be built from a canonical root too. On Windows the raw [`TempDir`] path
    /// never compares equal to one: canonicalization rewrites 8.3 short
    /// components (`RUNNER~1` into `runneradmin`) and adds the `\\?\` verbatim
    /// prefix.
    fn directories() -> (TempDir, PathBuf, PathBuf) {
        let fixture = tempfile::tempdir().expect("temporary fixture");
        let root = crate::fs_path::canonicalize(fixture.path()).expect("canonical fixture root");
        let source = root.join("source");
        let destination = root.join("destination");
        fs::create_dir(&source).expect("source folder");
        fs::create_dir(&destination).expect("destination folder");
        (fixture, source, destination)
    }

    #[test]
    fn same_filesystem_batch_moves_without_overwriting() {
        let (_fixture, source, destination) = directories();
        let first = source.join("first.flac");
        let second = source.join("album");
        fs::write(&first, b"first bytes").expect("first file");
        fs::create_dir(&second).expect("album folder");
        fs::write(second.join("track.opus"), b"track bytes").expect("track");

        let report = move_local_entries(
            &source,
            &[first.clone(), second.clone()],
            &destination,
            LocalMoveLimits::default(),
        )
        .expect("safe batch move");

        assert_eq!(report.completed.len(), 2);
        assert!(report.recovery.is_empty());
        assert!(!first.exists());
        assert!(!second.exists());
        assert_eq!(
            fs::read(destination.join("first.flac")).expect("moved file"),
            b"first bytes"
        );
        assert_eq!(
            fs::read(destination.join("album/track.opus")).expect("moved tree"),
            b"track bytes"
        );
    }

    #[test]
    fn validation_rejects_duplicate_stale_traversal_and_current_folder() {
        let (_fixture, source, destination) = directories();
        let track = source.join("track.flac");
        fs::write(&track, b"audio").expect("track");

        assert!(matches!(
            validate_local_move(
                &source,
                &[track.clone(), track.clone()],
                &destination,
                LocalMoveLimits::default(),
            ),
            Err(LocalMoveValidationError::DuplicateSource(_))
        ));
        assert!(matches!(
            validate_local_move(
                &source,
                &[spelled_under(&source, &["..", "source", "track.flac"])],
                &destination,
                LocalMoveLimits::default(),
            ),
            Err(LocalMoveValidationError::UnsafeSource(_))
        ));
        assert!(matches!(
            validate_local_move(
                &source,
                std::slice::from_ref(&track),
                &source,
                LocalMoveLimits::default(),
            ),
            Err(LocalMoveValidationError::CurrentDirectory(_))
        ));

        let plan = validate_local_move(
            &source,
            std::slice::from_ref(&track),
            &destination,
            LocalMoveLimits::default(),
        )
        .expect("initially valid");
        fs::remove_file(&track).expect("remove stale selection");
        fs::write(&track, b"replacement").expect("replace selection");
        let error = execute_local_move(&plan).expect_err("stale identity");
        assert!(matches!(error, LocalMoveError::Execution(_)));
        assert!(!destination.join("track.flac").exists());
    }

    #[test]
    fn complete_batch_is_validated_before_first_source_moves() {
        let (_fixture, source, destination) = directories();
        let first = source.join("first.flac");
        let second = source.join("second.flac");
        fs::write(&first, b"first").expect("first source");
        fs::write(&second, b"second").expect("second source");
        fs::write(destination.join("second.flac"), b"collision").expect("late batch collision");

        let error = move_local_entries(
            &source,
            &[first.clone(), second],
            &destination,
            LocalMoveLimits::default(),
        )
        .expect_err("collision rejects complete batch");

        assert!(matches!(
            error,
            LocalMoveError::Validation(LocalMoveValidationError::TargetExists(_))
        ));
        assert_eq!(fs::read(&first).expect("first source intact"), b"first");
        assert!(!destination.join("first.flac").exists());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_source_rejects_the_complete_batch_before_mutation() {
        use std::os::unix::ffi::OsStringExt;

        let (_fixture, source, destination) = directories();
        let first = source.join("first.flac");
        let non_utf8 = source.join(OsString::from_vec(b"second-\xff.flac".to_vec()));
        fs::write(&first, b"first").expect("first source");
        fs::write(&non_utf8, b"second").expect("non-UTF-8 source");

        let error = move_local_entries(
            &source,
            &[first.clone(), non_utf8.clone()],
            &destination,
            LocalMoveLimits::default(),
        )
        .expect_err("non-UTF-8 source must be rejected");

        assert!(matches!(
            error,
            LocalMoveError::Validation(LocalMoveValidationError::NonUtf8Source(path))
                if path == non_utf8
        ));
        assert_eq!(fs::read(&first).expect("first source intact"), b"first");
        assert_eq!(
            fs::read(&non_utf8).expect("non-UTF-8 source intact"),
            b"second"
        );
        assert!(
            fs::read_dir(&destination)
                .expect("destination listing")
                .next()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_directory_descendant_rejects_move_before_mutation() {
        use std::os::unix::ffi::OsStringExt;

        let (_fixture, source, destination) = directories();
        let album = source.join("album");
        let non_utf8 = album.join(OsString::from_vec(b"track-\xff.flac".to_vec()));
        fs::create_dir(&album).expect("album source");
        fs::write(&non_utf8, b"audio").expect("non-UTF-8 descendant");

        let error = move_local_entries(
            &source,
            std::slice::from_ref(&album),
            &destination,
            LocalMoveLimits::default(),
        )
        .expect_err("non-UTF-8 descendant must be rejected");

        assert!(matches!(
            error,
            LocalMoveError::Validation(LocalMoveValidationError::NonUtf8Source(path))
                if path == non_utf8
        ));
        assert!(album.exists());
        assert!(!destination.join("album").exists());
    }

    #[cfg(unix)]
    #[test]
    fn newly_added_non_utf8_descendant_is_rechecked_before_mutation() {
        use std::os::unix::ffi::OsStringExt;

        let (_fixture, source, destination) = directories();
        let album = source.join("album");
        fs::create_dir(&album).expect("album source");
        fs::write(album.join("track.flac"), b"audio").expect("initial descendant");
        let plan = validate_local_move(
            &source,
            std::slice::from_ref(&album),
            &destination,
            LocalMoveLimits::default(),
        )
        .expect("initially persistable tree");
        let non_utf8 = album.join(OsString::from_vec(b"new-\xff.flac".to_vec()));
        fs::write(&non_utf8, b"new").expect("new non-UTF-8 descendant");

        let error = execute_local_move(&plan).expect_err("stale tree must be rejected");

        assert!(matches!(
            error,
            LocalMoveError::Validation(LocalMoveValidationError::NonUtf8Source(path))
                if path == non_utf8
        ));
        assert!(album.exists());
        assert!(!destination.join("album").exists());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_target_rejects_the_move_before_mutation() {
        use std::os::unix::ffi::OsStringExt;

        let (fixture, source, _destination) = directories();
        let track = source.join("track.flac");
        let destination = fixture
            .path()
            .join(OsString::from_vec(b"destination-\xff".to_vec()));
        let target = destination.join("track.flac");
        fs::write(&track, b"audio").expect("source");
        fs::create_dir(&destination).expect("non-UTF-8 destination");

        let error = move_local_entries(
            &source,
            std::slice::from_ref(&track),
            &destination,
            LocalMoveLimits::default(),
        )
        .expect_err("non-UTF-8 target must be rejected");

        assert!(matches!(
            error,
            LocalMoveError::Validation(LocalMoveValidationError::NonUtf8Target(path))
                if path == target
        ));
        assert_eq!(fs::read(&track).expect("source intact"), b"audio");
        assert!(!target.exists());
    }

    #[test]
    fn partial_batch_failure_reports_completed_mapping_and_retains_failed_source() {
        let (_fixture, source, destination) = directories();
        let first = source.join("first.flac");
        let second = source.join("second.flac");
        fs::write(&first, b"first").expect("first source");
        fs::write(&second, b"second").expect("second source");
        let plan = validate_local_move(
            &source,
            &[first.clone(), second.clone()],
            &destination,
            LocalMoveLimits::default(),
        )
        .expect("valid batch");

        let error = execute_with_renamer(&plan, &FailSecondRenamer::default())
            .expect_err("second move fails");
        let LocalMoveError::Execution(failure) = error else {
            panic!("expected execution failure");
        };
        assert_eq!(
            failure.completed,
            vec![LocalMoveMapping {
                source: first.clone(),
                target: destination.join("first.flac"),
            }]
        );
        assert_eq!(
            failure.recovery,
            LocalMoveRecovery::SourceIntact {
                source: second.clone()
            }
        );
        assert!(!first.exists());
        assert!(destination.join("first.flac").exists());
        assert_eq!(fs::read(second).expect("failed source intact"), b"second");
    }

    #[test]
    fn destination_listing_is_bounded_sorted_and_directory_only() {
        let (fixture, source, destination) = directories();
        fs::create_dir(destination.join("z-last")).expect("last directory");
        fs::create_dir(destination.join("a-first")).expect("first directory");
        fs::write(destination.join("cover.jpg"), b"not decoded").expect("ignored media file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            symlink(&source, destination.join("linked-directory")).expect("directory symlink");
        }

        let listing =
            list_local_move_destinations(&destination, LocalMoveDestinationLimits::default())
                .expect("destination-only listing");
        assert_eq!(
            listing.path,
            crate::fs_path::canonicalize(&destination).expect("canonical")
        );
        assert_eq!(
            listing.parent,
            Some(crate::fs_path::canonicalize(fixture.path()).expect("canonical parent"))
        );
        assert_eq!(
            listing
                .directories
                .iter()
                .map(|entry| entry.name.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            ["a-first", "z-last"]
        );

        let bounded = list_local_move_destinations(
            &destination,
            LocalMoveDestinationLimits {
                max_inspected_entries: 100,
                max_visible_directories: 1,
            },
        )
        .expect("bounded listing");
        assert!(bounded.truncated);
        assert_eq!(bounded.directories.len(), 1);
    }

    #[test]
    fn validation_rejects_collisions_symlinks_and_descendant_targets() {
        let (_fixture, source, destination) = directories();
        let track = source.join("track.flac");
        fs::write(&track, b"audio").expect("track");
        fs::write(destination.join("track.flac"), b"keep me").expect("collision");
        assert!(matches!(
            validate_local_move(
                &source,
                std::slice::from_ref(&track),
                &destination,
                LocalMoveLimits::default(),
            ),
            Err(LocalMoveValidationError::TargetExists(_))
        ));
        assert_eq!(
            fs::read(destination.join("track.flac")).expect("collision intact"),
            b"keep me"
        );

        let folder = source.join("folder");
        let child = folder.join("child");
        fs::create_dir_all(&child).expect("descendant");
        assert!(matches!(
            validate_local_move(
                &source,
                std::slice::from_ref(&folder),
                &child,
                LocalMoveLimits::default(),
            ),
            Err(LocalMoveValidationError::DescendantDestination { .. })
        ));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let link = source.join("linked.flac");
            symlink(&track, &link).expect("source symlink");
            assert!(matches!(
                validate_local_move(&source, &[link], &destination, LocalMoveLimits::default(),),
                Err(LocalMoveValidationError::SymbolicLink(_))
            ));

            fs::remove_file(destination.join("track.flac")).expect("remove collision");
            let dangling_target = destination.join("track.flac");
            symlink(destination.join("missing"), &dangling_target).expect("dangling collision");
            assert!(matches!(
                validate_local_move(
                    &source,
                    std::slice::from_ref(&track),
                    &destination,
                    LocalMoveLimits::default(),
                ),
                Err(LocalMoveValidationError::TargetExists(_))
            ));
        }
    }

    #[test]
    fn cross_filesystem_file_and_tree_are_verified_then_published() {
        let (_fixture, source, destination) = directories();
        let album = source.join("album");
        fs::create_dir(&album).expect("album");
        fs::write(album.join("one.flac"), b"one").expect("one");
        fs::create_dir(album.join("disc-two")).expect("disc two");
        fs::write(album.join("disc-two/two.flac"), b"two").expect("two");
        let plan = validate_local_move(
            &source,
            std::slice::from_ref(&album),
            &destination,
            LocalMoveLimits::default(),
        )
        .expect("valid cross-device fixture");

        let report = execute_with_renamer(&plan, &ForceCrossDeviceRenamer::cross_device_once())
            .expect("cross-device copy");

        assert_eq!(report.completed.len(), 1);
        assert!(report.recovery.is_empty());
        assert!(!album.exists());
        assert_eq!(
            fs::read(destination.join("album/one.flac")).expect("first copied"),
            b"one"
        );
        assert_eq!(
            fs::read(destination.join("album/disc-two/two.flac")).expect("nested copied"),
            b"two"
        );
        assert!(fs::read_dir(&source).expect("source listing").all(|entry| {
            !entry
                .expect("source entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".youta-move-")
        }));
    }

    #[test]
    fn cross_filesystem_nested_symlink_keeps_source_and_cleans_staging() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let (_fixture, source, destination) = directories();
            let album = source.join("album");
            fs::create_dir(&album).expect("album");
            fs::write(album.join("track.flac"), b"audio").expect("track");
            symlink("track.flac", album.join("alias.flac")).expect("nested symlink");
            let plan = validate_local_move(
                &source,
                std::slice::from_ref(&album),
                &destination,
                LocalMoveLimits::default(),
            )
            .expect("top-level directory is valid");

            let error = execute_with_renamer(&plan, &ForceCrossDeviceRenamer::cross_device_once())
                .expect_err("cross-device symlink rejection");
            let LocalMoveError::Execution(failure) = error else {
                panic!("expected execution failure");
            };
            assert_eq!(
                failure.recovery,
                LocalMoveRecovery::SourceIntact {
                    source: album.clone()
                }
            );
            assert!(album.exists());
            assert!(!destination.join("album").exists());
            assert!(
                fs::read_dir(&destination)
                    .expect("destination listing")
                    .next()
                    .is_none()
            );
        }
    }

    #[test]
    fn cross_filesystem_publish_then_detach_failure_reports_both_paths() {
        let (_fixture, source, destination) = directories();
        let track = source.join("track.flac");
        fs::write(&track, b"audio").expect("track");
        let plan = validate_local_move(
            &source,
            std::slice::from_ref(&track),
            &destination,
            LocalMoveLimits::default(),
        )
        .expect("valid move");
        let renamer = ForceCrossDeviceRenamer {
            direct_cross_device_failures: Cell::new(1),
            quarantine_failures: Cell::new(1),
        };

        let error = execute_with_renamer(&plan, &renamer).expect_err("detach denial");
        let LocalMoveError::Execution(failure) = error else {
            panic!("expected execution failure");
        };
        assert_eq!(
            failure.recovery,
            LocalMoveRecovery::PublishedTargetAndSourceRetained {
                source: track.clone(),
                target: destination.join("track.flac"),
            },
            "{}",
            failure.cause
        );
        assert_eq!(fs::read(&track).expect("source retained"), b"audio");
        assert_eq!(
            fs::read(destination.join("track.flac")).expect("target published"),
            b"audio"
        );
    }

    #[test]
    fn cross_filesystem_publish_collision_never_overwrites_and_cleans_staging() {
        let (_fixture, source, destination) = directories();
        let track = source.join("track.flac");
        fs::write(&track, b"source audio").expect("track");
        let plan = validate_local_move(
            &source,
            std::slice::from_ref(&track),
            &destination,
            LocalMoveLimits::default(),
        )
        .expect("valid before race");

        let error = execute_with_renamer(&plan, &PublishCollisionRenamer::default())
            .expect_err("publish race");
        let LocalMoveError::Execution(failure) = error else {
            panic!("expected execution failure");
        };
        assert_eq!(
            failure.recovery,
            LocalMoveRecovery::SourceIntact {
                source: track.clone()
            }
        );
        assert_eq!(fs::read(&track).expect("source retained"), b"source audio");
        assert_eq!(
            fs::read(destination.join("track.flac")).expect("racing target intact"),
            b"racing destination"
        );
        assert_eq!(
            fs::read_dir(&destination)
                .expect("destination listing")
                .count(),
            1,
            "private staging must be cleaned after the no-replace collision"
        );
    }

    #[test]
    fn remapping_uses_longest_prefix_and_only_changes_local_ids() {
        let mappings = vec![
            LocalMoveMapping {
                source: fixture_absolute("/music"),
                target: fixture_absolute("/archive"),
            },
            LocalMoveMapping {
                source: fixture_absolute("/music/album"),
                target: fixture_absolute("/library/favourite"),
            },
        ];
        assert_eq!(
            remap_local_path_prefix(&fixture_absolute("/music/album/disc/one.flac"), &mappings),
            Some(fixture_absolute("/library/favourite/disc/one.flac"))
        );
        assert_eq!(
            remap_local_path_prefix(&fixture_absolute("/music/album"), &mappings),
            Some(fixture_absolute("/library/favourite"))
        );
        assert_eq!(
            remap_local_path_prefix(&fixture_absolute("/musicology/one.flac"), &mappings),
            None
        );

        let old_locator = fixture_absolute("/music/album/one.flac");
        let old_locator = old_locator.to_str().expect("UTF-8 fixture locator");
        let new_locator = fixture_absolute("/library/favourite/one.flac");
        let new_locator = new_locator.to_str().expect("UTF-8 fixture locator");

        let mut local = MediaId::new(SourceKind::Local, old_locator);
        assert!(remap_local_media_id(&mut local, &mappings).expect("UTF-8 remap"));
        assert_eq!(local.external_id, new_locator);

        let old_url = url::Url::from_file_path(fixture_absolute("/music/album/one.flac"))
            .expect("absolute fixture URL");
        let new_url = url::Url::from_file_path(fixture_absolute("/library/favourite/one.flac"))
            .expect("absolute fixture URL");
        let mut current = MediaId::new(SourceKind::Local, old_url.as_str());
        assert!(remap_local_media_id(&mut current, &mappings).expect("file-URL remap"));
        assert_eq!(current.external_id, new_url.as_str());

        let mut youtube = MediaId::new(SourceKind::YouTube, old_locator);
        assert!(!remap_local_media_id(&mut youtube, &mappings).expect("non-local unchanged"));
        assert_eq!(youtube.external_id, old_locator);

        let mut locator = old_locator.to_owned();
        assert!(remap_local_replay_locator(&mut locator, &mappings).expect("locator remap"));
        assert_eq!(locator, new_locator);
    }

    /// Returns `path` as this platform spells an absolute path.
    ///
    /// `/music` is absolute only where the filesystem has one root; Windows
    /// needs a drive letter or the decoders correctly answer that the fixture
    /// names nothing.
    fn fixture_absolute(path: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"C:{}", path.replace('/', r"\")))
        } else {
            PathBuf::from(path)
        }
    }

    /// Writes a traversal into the path's text, past `join`'s good manners.
    ///
    /// `PathBuf::push` onto a verbatim base *resolves* `.` and `..` and
    /// re-separates what it is given — std keeps verbatim paths literal by
    /// normalising at push time — so `join("../source/x")` quietly becomes the
    /// clean path it points at and asserts nothing about validation. A hostile
    /// locator does not arrive through `join`; it arrives as text. This builds
    /// that text.
    fn spelled_under(base: &Path, suffix_parts: &[&str]) -> PathBuf {
        let mut spelled = base.as_os_str().to_owned();
        for part in suffix_parts {
            spelled.push(std::path::MAIN_SEPARATOR_STR);
            spelled.push(part);
        }
        PathBuf::from(spelled)
    }

    /// A traversal spelled against a verbatim base is still refused as unsafe.
    ///
    /// Under `\\?\` Windows parses literally: `/` is not a separator, so a
    /// slash-spelled traversal arrives as one "normal" component and the
    /// rejection has to come from the component's spelling rather than its
    /// kind — a literal `..` between backslashes is still `ParentDir` and is
    /// covered by the portable test above.
    #[cfg(windows)]
    #[test]
    fn a_traversal_spelled_against_a_verbatim_base_is_still_unsafe() {
        let (_fixture, source, destination) = directories();
        let track = source.join("track.flac");
        fs::write(&track, b"audio").expect("track");

        assert!(matches!(
            validate_local_move(
                &source,
                &[spelled_under(&source, &["../source/track.flac"])],
                &destination,
                LocalMoveLimits::default(),
            ),
            Err(LocalMoveValidationError::UnsafeSource(_))
        ));
    }

    /// A mapping written down verbatim still matches a locator that decoded
    /// flat, because both spell the same file.
    ///
    /// This is the exact shape a completed move produces: the plan holds what
    /// `fs::canonicalize` said — `\\?\C:\…` — while the stored identity made a
    /// round trip through a file URL, which cannot carry that prefix, and the
    /// file itself has already moved away, so nothing can be re-canonicalised.
    #[cfg(windows)]
    #[test]
    fn a_verbatim_mapping_still_remaps_a_flat_locator() {
        let mappings = vec![LocalMoveMapping {
            source: PathBuf::from(r"\\?\C:\music"),
            target: PathBuf::from(r"\\?\C:\archive"),
        }];
        assert_eq!(
            remap_local_path_prefix(Path::new(r"C:\music\one.flac"), &mappings),
            Some(PathBuf::from(r"C:\archive\one.flac"))
        );

        let mut current = MediaId::new(SourceKind::Local, "file:///C:/music/one.flac");
        assert!(remap_local_media_id(&mut current, &mappings).expect("file-URL remap"));
        assert_eq!(current.external_id, "file:///C:/archive/one.flac");
    }
}
