//! Staged, verified copies that never detach or modify their sources.
//!
//! Sources and destinations must be ordinary user-controlled filesystem paths.
//! Symlinks and special files are rejected, and observed identity/content changes
//! abort publication. This is not an atomic snapshot of concurrently edited files.

use super::*;

/// Validated source snapshots for a copy that has not written any output yet.
#[derive(Clone, Debug)]
pub struct LocalCopyPlan {
    source_directory: PathBuf,
    destination_directory: PathBuf,
    source_identity: DirectoryIdentity,
    destination_identity: DirectoryIdentity,
    entries: Vec<CopyEntry>,
    limits: LocalMoveLimits,
    total_bytes: u64,
}

impl LocalCopyPlan {
    /// Returns the canonical folder containing all selected sources.
    #[must_use]
    pub fn source_directory(&self) -> &Path {
        &self.source_directory
    }

    /// Returns the canonical folder receiving the copies.
    #[must_use]
    pub fn destination_directory(&self) -> &Path {
        &self.destination_directory
    }

    /// Returns source/output pairs for display, not durable identity remapping.
    #[must_use]
    pub fn mappings(&self) -> Vec<LocalMoveMapping> {
        self.entries
            .iter()
            .map(|entry| entry.mapping.clone())
            .collect()
    }
}

#[derive(Clone, Debug)]
struct CopyEntry {
    mapping: LocalMoveMapping,
    manifest: Vec<TreeNode>,
    directories: std::collections::HashMap<PathBuf, FilesystemIdentity>,
}

/// Completed copies and any destination staging paths requiring manual cleanup.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LocalCopyReport {
    /// Published copies whose original sources remain in place.
    pub completed: Vec<LocalMoveMapping>,
    /// Empty staging containers retained if cleanup after publication failed.
    pub recovery: Vec<LocalMoveRecovery>,
}

/// A failed copy with the exact completed outputs and retained recovery paths.
#[derive(Debug)]
pub struct LocalCopyFailure {
    /// Copies published before the failed entry.
    pub completed: Vec<LocalMoveMapping>,
    /// Source that could not be copied.
    pub source_path: PathBuf,
    /// Intended output path, never overwritten by a copy.
    pub target_path: PathBuf,
    /// Underlying filesystem or verification failure.
    pub cause: io::Error,
    /// Original source and, if cleanup failed, the retained staging tree.
    pub recovery: LocalMoveRecovery,
}

impl std::fmt::Display for LocalCopyFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "cannot copy `{}` to `{}`: {}; {} entries copied; source retained at `{}`",
            self.source_path.display(),
            self.target_path.display(),
            self.cause,
            self.completed.len(),
            self.source_path.display()
        )?;
        if let LocalMoveRecovery::SourceAndStagingRetained { staging, .. } = &self.recovery {
            write!(formatter, "; staging retained at `{}`", staging.display())?;
        }
        Ok(())
    }
}

impl std::error::Error for LocalCopyFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

/// Validation errors write nothing; execution errors report partial publication.
#[derive(Debug, thiserror::Error)]
pub enum LocalCopyError {
    /// A batch or stale plan was rejected before any destination was written.
    #[error("cannot prepare local copy `{path}`: {cause}")]
    Validation {
        /// Path involved in the rejected request.
        path: PathBuf,
        /// Validation or filesystem error.
        #[source]
        cause: io::Error,
    },
    /// Copying failed after execution began; original sources remain untouched.
    #[error(transparent)]
    Execution(Box<LocalCopyFailure>),
}

impl LocalCopyError {
    /// Returns outputs published before this failure, if any.
    #[must_use]
    pub fn completed(&self) -> &[LocalMoveMapping] {
        match self {
            Self::Validation { .. } => &[],
            Self::Execution(error) => &error.completed,
        }
    }

    /// Returns retained source/staging paths for an execution failure.
    #[must_use]
    pub fn recovery(&self) -> Option<&LocalMoveRecovery> {
        match self {
            Self::Validation { .. } => None,
            Self::Execution(error) => Some(&error.recovery),
        }
    }
}

/// Validates all selected trees before creating any destination entry.
///
/// Hidden and empty directories are included. Symlinks, special files,
/// collisions, duplicate selections and descendant destinations are rejected.
/// `max_tree_entries` bounds the entire copy batch, including pending children;
/// no unbounded directory collection or media metadata probing is performed.
///
/// # Errors
/// Returns a validation error for unsafe paths, unreadable/stale entries,
/// invalid limits, collisions, unsupported entries or exceeded resource limits.
pub fn validate_local_copy(
    source_directory: &Path,
    sources: &[PathBuf],
    destination_directory: &Path,
    limits: LocalMoveLimits,
) -> Result<LocalCopyPlan, LocalCopyError> {
    let invalid = |cause| LocalCopyError::Validation {
        path: source_directory.to_owned(),
        cause,
    };
    if limits.max_sources == 0
        || limits.max_tree_entries == 0
        || limits.max_depth == 0
        || sources.is_empty()
        || sources.len() > limits.max_sources
    {
        return Err(invalid(invalid_copy(
            "copy requires a nonempty selection within positive resource limits",
        )));
    }
    let source_directory = copy_directory(source_directory).map_err(invalid)?;
    let destination_directory =
        copy_directory(destination_directory).map_err(|cause| LocalCopyError::Validation {
            path: destination_directory.to_owned(),
            cause,
        })?;
    let prepare = || -> io::Result<LocalCopyPlan> {
        if source_directory == destination_directory {
            return Err(invalid_copy(
                "copy destination is already the source folder",
            ));
        }
        let source_identity = DirectoryIdentity::read(&source_directory)?;
        let destination_identity = DirectoryIdentity::read(&destination_directory)?;
        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        let mut total_nodes = 0usize;
        let mut total_bytes = 0u64;
        for source in sources {
            if !is_normalized_absolute(source)
                || source.parent().is_none_or(|parent| {
                    crate::fs_path::canonicalize(parent).ok().as_ref() != Some(&source_directory)
                })
            {
                return Err(invalid_copy(
                    "copy source must be one normalized immediate child",
                ));
            }
            let name = source
                .file_name()
                .ok_or_else(|| invalid_copy("copy source has no basename"))?;
            let source = source_directory.join(name);
            let target = destination_directory.join(name);
            if !seen.insert(source.clone()) {
                return Err(invalid_copy("copy source selected more than once"));
            }
            copy_target_absent(&target)?;
            if destination_directory.starts_with(&source) {
                return Err(invalid_copy(
                    "cannot copy a folder into itself or a descendant",
                ));
            }
            let remaining = limits.max_tree_entries.saturating_sub(total_nodes);
            let manifest = copy_snapshot(
                &source,
                LocalMoveLimits {
                    max_tree_entries: remaining,
                    ..limits
                },
            )?;
            total_nodes += manifest.len();
            for node in &manifest {
                if node.kind == NodeKind::File {
                    total_bytes =
                        total_bytes
                            .checked_add(node.identity.length)
                            .ok_or_else(|| {
                                invalid_copy("copy byte count exceeds its supported range")
                            })?;
                }
            }
            let directories = manifest
                .iter()
                .filter(|node| node.kind == NodeKind::Directory)
                .map(|node| (node.relative.clone(), node.identity.clone()))
                .collect();
            entries.push(CopyEntry {
                mapping: LocalMoveMapping { source, target },
                manifest,
                directories,
            });
        }
        Ok(LocalCopyPlan {
            source_directory: source_directory.clone(),
            destination_directory: destination_directory.clone(),
            source_identity,
            destination_identity,
            entries,
            limits,
            total_bytes,
        })
    };
    prepare().map_err(|cause| LocalCopyError::Validation {
        path: source_directory,
        cause,
    })
}

/// Executes a validated copy, leaving all source paths and identities intact.
///
/// # Errors
/// Rejects stale plans, changed sources and target collisions. A later failure
/// can leave earlier copies published; the error lists those outputs and any
/// staging path that could not be cleaned up.
pub fn execute_local_copy(plan: &LocalCopyPlan) -> Result<LocalCopyReport, LocalCopyError> {
    execute_local_copy_with_progress(plan, |_| {})
}

/// Copies with byte progress and completed top-level entry counts for a worker UI.
///
/// Progress reports copied bytes; verification can continue after the byte
/// counter reaches its total. Only completed entries have been published.
///
/// # Errors
/// Returns the same failures and recovery information as [`execute_local_copy`].
pub fn execute_local_copy_with_progress(
    plan: &LocalCopyPlan,
    mut progress: impl FnMut(LocalTransferProgress),
) -> Result<LocalCopyReport, LocalCopyError> {
    execute_copy(plan, &SystemNoReplaceRenamer, &mut progress)
}

/// Validates and executes a source-preserving copy on an application worker.
///
/// # Errors
/// Returns validation, stale-source, copy, verification or publication errors.
pub fn copy_local_entries(
    source_directory: &Path,
    sources: &[PathBuf],
    destination_directory: &Path,
    limits: LocalMoveLimits,
) -> Result<LocalCopyReport, LocalCopyError> {
    execute_local_copy(&validate_local_copy(
        source_directory,
        sources,
        destination_directory,
        limits,
    )?)
}

#[cfg(test)]
fn execute_with_publisher(
    plan: &LocalCopyPlan,
    publisher: &impl NoReplaceRenamer,
) -> Result<LocalCopyReport, LocalCopyError> {
    execute_copy(plan, publisher, &mut |_| {})
}

fn execute_copy(
    plan: &LocalCopyPlan,
    publisher: &impl NoReplaceRenamer,
    progress: &mut impl FnMut(LocalTransferProgress),
) -> Result<LocalCopyReport, LocalCopyError> {
    let preflight = || -> io::Result<()> {
        verify_copy_parents(plan)?;
        for entry in &plan.entries {
            copy_target_absent(&entry.mapping.target)?;
            verify_copy_source(entry, plan.limits)?;
        }
        Ok(())
    };
    preflight().map_err(|cause| LocalCopyError::Validation {
        path: plan.source_directory.clone(),
        cause,
    })?;
    let mut state = LocalTransferProgress {
        total_bytes: Some(plan.total_bytes),
        total_entries: plan.entries.len(),
        ..LocalTransferProgress::default()
    };
    progress(state);
    let mut report = LocalCopyReport::default();
    for entry in &plan.entries {
        let result = copy_entry(plan, entry, publisher, &mut state, progress);
        match result {
            Ok(recovery) => {
                report.completed.push(entry.mapping.clone());
                report.recovery.extend(recovery);
                state.completed_entries += 1;
                progress(state);
            }
            Err((cause, recovery)) => {
                return Err(LocalCopyError::Execution(Box::new(LocalCopyFailure {
                    completed: report.completed,
                    source_path: entry.mapping.source.clone(),
                    target_path: entry.mapping.target.clone(),
                    cause,
                    recovery,
                })));
            }
        }
    }
    Ok(report)
}

fn copy_entry(
    plan: &LocalCopyPlan,
    entry: &CopyEntry,
    publisher: &impl NoReplaceRenamer,
    state: &mut LocalTransferProgress,
    progress: &mut impl FnMut(LocalTransferProgress),
) -> Result<Option<LocalMoveRecovery>, (io::Error, LocalMoveRecovery)> {
    let source_intact = |cause| {
        (
            cause,
            LocalMoveRecovery::SourceIntact {
                source: entry.mapping.source.clone(),
            },
        )
    };
    verify_copy_parents(plan)
        .and_then(|()| verify_copy_source(entry, plan.limits))
        .map_err(source_intact)?;
    let staging = create_copy_stage(&plan.destination_directory).map_err(source_intact)?;
    let stage_identity = DirectoryIdentity::read(&staging).map_err(|cause| {
        (
            cause,
            LocalMoveRecovery::SourceAndStagingRetained {
                source: entry.mapping.source.clone(),
                staging: staging.clone(),
            },
        )
    })?;
    let payload = staging.join("payload");
    let mut copy = || -> io::Result<()> {
        for node in &entry.manifest {
            verify_copy_parents(plan)?;
            stage_identity.check(&staging)?;
            check_source_ancestors(entry, &node.relative)?;
            let source = join_relative(&entry.mapping.source, &node.relative);
            let target = join_relative(&payload, &node.relative);
            if node.kind == NodeKind::Directory {
                fs::create_dir(&target)?;
            } else {
                copy_regular_file(&source, &target, &node.identity, state, progress)?;
            }
        }
        verify_copy_source(entry, plan.limits)?;
        let staged = copy_snapshot(&payload, plan.limits)?;
        if staged.len() != entry.manifest.len()
            || staged
                .iter()
                .zip(&entry.manifest)
                .any(|(copied, original)| {
                    copied.relative != original.relative
                        || copied.kind != original.kind
                        || (copied.kind == NodeKind::File
                            && copied.identity.length != original.identity.length)
                })
        {
            return Err(invalid_copy("staging tree differs from copy source"));
        }
        for node in &entry.manifest {
            if node.kind == NodeKind::File {
                check_source_ancestors(entry, &node.relative)?;
                compare_copy_file(
                    &join_relative(&entry.mapping.source, &node.relative),
                    &join_relative(&payload, &node.relative),
                    &node.identity,
                )?;
            }
        }
        verify_copy_source(entry, plan.limits)?;
        for node in entry.manifest.iter().rev() {
            let original =
                fs::symlink_metadata(join_relative(&entry.mapping.source, &node.relative))?;
            if FilesystemIdentity::from_metadata(&original) != node.identity {
                return Err(invalid_copy("source changed while copying permissions"));
            }
            fs::set_permissions(
                join_relative(&payload, &node.relative),
                original.permissions(),
            )?;
        }
        verify_copy_parents(plan)?;
        stage_identity.check(&staging)?;
        publisher.rename_no_replace(&payload, &entry.mapping.target)
    };
    if let Err(cause) = copy() {
        return Err((
            cause,
            cleanup_copy_stage(&entry.mapping.source, &staging, &stage_identity),
        ));
    }
    if stage_identity
        .check(&staging)
        .and_then(|()| fs::remove_dir(&staging))
        .is_err()
    {
        return Ok(Some(LocalMoveRecovery::SourceAndStagingRetained {
            source: entry.mapping.source.clone(),
            staging,
        }));
    }
    Ok(None)
}

fn invalid_copy(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn copy_directory(path: &Path) -> io::Result<PathBuf> {
    if !is_normalized_absolute(path) {
        return Err(invalid_copy(
            "copy directory must be a normalized absolute path",
        ));
    }
    DirectoryIdentity::read(path)?;
    crate::fs_path::canonicalize(path)
}

fn copy_target_absent(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("copy target already exists: `{}`", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DirectoryIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    created: Option<SystemTime>,
}

impl DirectoryIdentity {
    fn read(path: &Path) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_dir() {
            return Err(invalid_copy(
                "copy path is not a real directory (symlinks are disabled)",
            ));
        }
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(not(unix))]
            created: metadata.created().ok(),
        })
    }

    fn check(&self, path: &Path) -> io::Result<()> {
        if *self == Self::read(path)? {
            Ok(())
        } else {
            Err(invalid_copy("copy directory identity changed"))
        }
    }
}

fn verify_copy_parents(plan: &LocalCopyPlan) -> io::Result<()> {
    plan.source_identity.check(&plan.source_directory)?;
    plan.destination_identity.check(&plan.destination_directory)
}

fn copy_snapshot(root: &Path, limits: LocalMoveLimits) -> io::Result<Vec<TreeNode>> {
    let mut pending = vec![(PathBuf::new(), 0usize)];
    let mut nodes = Vec::new();
    while let Some((relative, depth)) = pending.pop() {
        if depth > limits.max_depth || nodes.len() >= limits.max_tree_entries {
            return Err(invalid_copy("copy tree exceeds its entry or depth limit"));
        }
        let path = join_relative(root, &relative);
        let metadata = fs::symlink_metadata(&path)?;
        let kind = if metadata.file_type().is_file() {
            NodeKind::File
        } else if metadata.file_type().is_dir() {
            NodeKind::Directory
        } else {
            return Err(invalid_copy(&format!(
                "copy refuses symbolic links and special files: `{}`",
                path.display()
            )));
        };
        nodes.push(TreeNode {
            relative: relative.clone(),
            kind,
            identity: FilesystemIdentity::from_metadata(&metadata),
        });
        if kind == NodeKind::Directory {
            let mut children = Vec::new();
            for child in fs::read_dir(&path)? {
                if nodes.len() + pending.len() + children.len() >= limits.max_tree_entries {
                    return Err(invalid_copy("copy tree exceeds its entry limit"));
                }
                children.push(child?.file_name());
            }
            children.sort();
            for child in children.into_iter().rev() {
                pending.push((relative.join(child), depth + 1));
            }
        }
    }
    Ok(nodes)
}

fn verify_copy_source(entry: &CopyEntry, limits: LocalMoveLimits) -> io::Result<()> {
    if copy_snapshot(&entry.mapping.source, limits)? == entry.manifest {
        Ok(())
    } else {
        Err(invalid_copy("source changed after copy validation"))
    }
}

fn check_source_ancestors(entry: &CopyEntry, relative: &Path) -> io::Result<()> {
    for ancestor in relative.ancestors() {
        let Some(identity) = entry.directories.get(ancestor) else {
            continue;
        };
        let metadata = fs::symlink_metadata(join_relative(&entry.mapping.source, ancestor))?;
        if !metadata.file_type().is_dir()
            || FilesystemIdentity::from_metadata(&metadata) != *identity
        {
            return Err(invalid_copy("copy source directory changed"));
        }
    }
    Ok(())
}

/// Creates a unique staging directory, applying owner-only permissions on Unix.
fn create_copy_stage(parent: &Path) -> io::Result<PathBuf> {
    for _ in 0..HIDDEN_NAME_ATTEMPTS {
        let sequence = HIDDEN_NAME_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".youta-copy-stage-{}-{sequence}.part",
            std::process::id()
        ));
        let builder = fs::DirBuilder::new();
        #[cfg(unix)]
        let mut builder = builder;
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "cannot allocate copy staging directory",
    ))
}

fn open_copy_file(path: &Path, identity: &FilesystemIdentity) -> io::Result<File> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || FilesystemIdentity::from_metadata(&metadata) != *identity
    {
        return Err(invalid_copy("copy source file changed"));
    }
    #[cfg(unix)]
    let file = File::from(
        rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(io::Error::from)?,
    );
    #[cfg(not(unix))]
    let file = {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_FLAG_OPEN_REPARSE_POINT opens the entry, not a swapped link target.
            options.custom_flags(0x0020_0000);
        }
        options.open(path)?
    };
    let opened = file.metadata()?;
    if !opened.file_type().is_file() || FilesystemIdentity::from_metadata(&opened) != *identity {
        return Err(invalid_copy("opened copy source changed"));
    }
    Ok(file)
}

fn copy_regular_file(
    source: &Path,
    target: &Path,
    identity: &FilesystemIdentity,
    state: &mut LocalTransferProgress,
    progress: &mut impl FnMut(LocalTransferProgress),
) -> io::Result<()> {
    let mut input = open_copy_file(source, identity)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
    let mut remaining = identity.length;
    while remaining > 0 {
        let limit =
            usize::try_from(remaining.min(COPY_BUFFER_BYTES as u64)).expect("bounded buffer");
        input.read_exact(&mut buffer[..limit])?;
        output.write_all(&buffer[..limit])?;
        remaining -= limit as u64;
        state.completed_bytes = state.completed_bytes.saturating_add(limit as u64);
        progress(*state);
    }
    if FilesystemIdentity::from_metadata(&input.metadata()?) != *identity {
        return Err(invalid_copy("copy source changed during read"));
    }
    output.sync_all()
}

fn compare_copy_file(
    source: &Path,
    target: &Path,
    identity: &FilesystemIdentity,
) -> io::Result<()> {
    let mut input = open_copy_file(source, identity)?;
    let mut copied = File::open(target)?;
    if copied.metadata()?.len() != identity.length {
        return Err(invalid_copy("copy length differs from source"));
    }
    let mut first = vec![0u8; COPY_BUFFER_BYTES];
    let mut second = vec![0u8; COPY_BUFFER_BYTES];
    let mut remaining = identity.length;
    while remaining > 0 {
        let length =
            usize::try_from(remaining.min(COPY_BUFFER_BYTES as u64)).expect("bounded buffer");
        input.read_exact(&mut first[..length])?;
        copied.read_exact(&mut second[..length])?;
        if first[..length] != second[..length] {
            return Err(invalid_copy("copy failed byte-for-byte verification"));
        }
        remaining -= length as u64;
    }
    Ok(())
}

fn cleanup_copy_stage(
    source: &Path,
    staging: &Path,
    identity: &DirectoryIdentity,
) -> LocalMoveRecovery {
    if identity.check(staging).is_err() || fs::remove_dir_all(staging).is_err() {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates canonical fixture folders without relying on platform path spelling.
    fn directories() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temporary = crate::test_support::canonical_tempdir("copy 音楽");
        let source = temporary.path().join("source");
        let target = temporary.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        (temporary, source, target)
    }

    #[test]
    fn copies_files_hidden_contents_and_empty_directories_without_moving_sources() {
        let (_temporary, source, target) = directories();
        let track = source.join("track.opus");
        let album = source.join("album");
        fs::write(&track, b"first track").unwrap();
        fs::create_dir_all(album.join(".hidden/empty")).unwrap();
        fs::write(album.join(".hidden/notes.txt"), b"hidden notes").unwrap();
        let report = copy_local_entries(
            &source,
            &[track.clone(), album.clone()],
            &target,
            LocalMoveLimits::default(),
        )
        .unwrap();
        assert_eq!(report.completed.len(), 2);
        assert!(report.recovery.is_empty());
        for path in ["track.opus", "album/.hidden/notes.txt"] {
            assert_eq!(
                fs::read(source.join(path)).unwrap(),
                fs::read(target.join(path)).unwrap()
            );
        }
        assert!(album.join(".hidden/empty").is_dir());
        assert!(target.join("album/.hidden/empty").is_dir());
        assert_eq!(fs::read_dir(&target).unwrap().count(), 2);
        fs::write(target.join("track.opus"), b"independent copy").unwrap();
        assert_eq!(fs::read(&track).unwrap(), b"first track");
    }

    #[test]
    fn rejects_complete_batch_before_output_for_collisions_and_unsafe_paths() {
        let (_temporary, source, target) = directories();
        let first = source.join("first");
        let second = source.join("second");
        fs::write(&first, b"first").unwrap();
        fs::write(&second, b"second").unwrap();
        fs::write(target.join("second"), b"existing").unwrap();
        assert!(
            copy_local_entries(
                &source,
                &[first.clone(), second],
                &target,
                LocalMoveLimits::default()
            )
            .is_err()
        );
        assert!(!target.join("first").exists());
        assert_eq!(fs::read(target.join("second")).unwrap(), b"existing");
        for selected in [
            vec![],
            vec![first.clone(), first.clone()],
            vec![source.join("../source/first")],
            vec![source.clone()],
        ] {
            assert!(
                validate_local_copy(&source, &selected, &target, LocalMoveLimits::default())
                    .is_err()
            );
        }
        assert!(
            validate_local_copy(&source, &[first], &source, LocalMoveLimits::default()).is_err()
        );
        let album = source.join("album");
        fs::create_dir_all(album.join("child")).unwrap();
        assert!(
            validate_local_copy(
                &source,
                std::slice::from_ref(&album),
                &album.join("child"),
                LocalMoveLimits::default()
            )
            .is_err()
        );
    }

    #[test]
    fn stale_plan_rechecks_nested_source_and_all_destination_names() {
        let (_temporary, source, target) = directories();
        let first = source.join("first");
        let album = source.join("album");
        fs::write(&first, b"first").unwrap();
        fs::create_dir(&album).unwrap();
        fs::write(album.join("song"), b"before").unwrap();
        let plan = validate_local_copy(
            &source,
            &[first.clone(), album.clone()],
            &target,
            LocalMoveLimits::default(),
        )
        .unwrap();
        fs::write(album.join("song"), b"after with changed size").unwrap();
        assert!(execute_local_copy(&plan).is_err());
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        let plan = validate_local_copy(
            &source,
            &[first, album],
            &target,
            LocalMoveLimits::default(),
        )
        .unwrap();
        fs::write(target.join("album"), b"racing target").unwrap();
        assert!(execute_local_copy(&plan).is_err());
        assert!(!target.join("first").exists());
        assert_eq!(fs::read(target.join("album")).unwrap(), b"racing target");
    }

    #[test]
    fn tree_entry_and_depth_limits_reject_before_any_output() {
        let (_temporary, source, target) = directories();
        let album = source.join("album");
        fs::create_dir_all(album.join("disc/nested")).unwrap();
        for index in 0..8 {
            fs::write(album.join(format!("file-{index}")), b"fixture").unwrap();
        }
        for limits in [
            LocalMoveLimits {
                max_tree_entries: 3,
                ..LocalMoveLimits::default()
            },
            LocalMoveLimits {
                max_depth: 1,
                ..LocalMoveLimits::default()
            },
            LocalMoveLimits {
                max_sources: 0,
                ..LocalMoveLimits::default()
            },
        ] {
            assert!(
                validate_local_copy(&source, std::slice::from_ref(&album), &target, limits)
                    .is_err()
            );
            assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_selected_nested_and_destination_symlinks_without_following_them() {
        use std::os::unix::fs::symlink;
        let (_temporary, source, target) = directories();
        let track = source.join("track");
        fs::write(&track, b"track").unwrap();
        let link = source.join("linked");
        symlink(&track, &link).unwrap();
        assert!(copy_local_entries(&source, &[link], &target, LocalMoveLimits::default()).is_err());
        let album = source.join("album");
        fs::create_dir(&album).unwrap();
        symlink(&track, album.join("linked")).unwrap();
        assert!(
            copy_local_entries(
                &source,
                &[track.clone(), album],
                &target,
                LocalMoveLimits::default()
            )
            .is_err()
        );
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        symlink(target.join("missing"), target.join("track")).unwrap();
        assert!(
            copy_local_entries(
                &source,
                std::slice::from_ref(&track),
                &target,
                LocalMoveLimits::default()
            )
            .is_err()
        );
        assert_eq!(fs::read(track).unwrap(), b"track");
    }

    #[cfg(unix)]
    #[test]
    fn copies_lossless_non_utf8_names_and_preserves_permissions() {
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::PermissionsExt;
        let (_temporary, source, target) = directories();
        let name = OsString::from_vec(b"track-\xff.opus".to_vec());
        let track = source.join(&name);
        fs::write(&track, b"track").unwrap();
        fs::set_permissions(&track, fs::Permissions::from_mode(0o640)).unwrap();
        copy_local_entries(
            &source,
            std::slice::from_ref(&track),
            &target,
            LocalMoveLimits::default(),
        )
        .unwrap();
        assert_eq!(fs::read(target.join(&name)).unwrap(), b"track");
        assert_eq!(
            fs::metadata(target.join(&name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        assert_eq!(fs::read(track).unwrap(), b"track");
    }

    /// Fails or races publication without permitting replacement of existing data.
    struct FailingPublisher {
        calls: std::cell::Cell<usize>,
        collision: bool,
    }

    impl NoReplaceRenamer for FailingPublisher {
        fn rename_no_replace(&self, source: &Path, target: &Path) -> io::Result<()> {
            let count = self.calls.get();
            self.calls.set(count + 1);
            if count == 1 {
                if self.collision {
                    fs::write(target, b"racing destination")?;
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "injected publish failure",
                    ));
                }
            }
            SystemNoReplaceRenamer.rename_no_replace(source, target)
        }
    }

    #[test]
    fn partial_batch_failure_reports_completed_copies_and_keeps_all_sources() {
        for collision in [false, true] {
            let (_temporary, source, target) = directories();
            let first = source.join("first");
            let second = source.join("second");
            fs::write(&first, b"first").unwrap();
            fs::write(&second, b"second").unwrap();
            let plan = validate_local_copy(
                &source,
                &[first.clone(), second.clone()],
                &target,
                LocalMoveLimits::default(),
            )
            .unwrap();
            let error = execute_with_publisher(
                &plan,
                &FailingPublisher {
                    calls: std::cell::Cell::new(0),
                    collision,
                },
            )
            .unwrap_err();
            assert_eq!(error.completed().len(), 1);
            assert_eq!(error.completed()[0].source, first);
            assert_eq!(fs::read(&first).unwrap(), b"first");
            assert_eq!(fs::read(&second).unwrap(), b"second");
            assert_eq!(fs::read(target.join("first")).unwrap(), b"first");
            if collision {
                assert_eq!(
                    fs::read(target.join("second")).unwrap(),
                    b"racing destination"
                );
            } else {
                assert!(!target.join("second").exists());
            }
            assert_eq!(
                fs::read_dir(&target).unwrap().count(),
                if collision { 2 } else { 1 }
            );
            assert!(matches!(
                error.recovery(),
                Some(LocalMoveRecovery::SourceIntact { .. })
            ));
        }
    }

    #[test]
    fn progress_counts_bounded_chunks_and_published_top_level_entries() {
        let (_temporary, source, target) = directories();
        let first = source.join("first");
        let empty = source.join("empty");
        let bytes = vec![b'x'; COPY_BUFFER_BYTES * 2 + 3];
        fs::write(&first, &bytes).unwrap();
        fs::create_dir(&empty).unwrap();
        let plan = validate_local_copy(
            &source,
            &[first, empty],
            &target,
            LocalMoveLimits::default(),
        )
        .unwrap();
        let mut progress = Vec::new();
        execute_local_copy_with_progress(&plan, |state| progress.push(state)).unwrap();
        assert_eq!(progress[0].total_bytes, Some(bytes.len() as u64));
        assert_eq!(progress[0].completed_bytes, 0);
        assert!(
            progress
                .windows(2)
                .all(|pair| pair[1].completed_bytes >= pair[0].completed_bytes
                    && pair[1].completed_entries >= pair[0].completed_entries)
        );
        let final_state = progress.last().unwrap();
        assert_eq!(final_state.completed_bytes, bytes.len() as u64);
        assert_eq!(final_state.completed_entries, 2);
        assert_eq!(final_state.total_entries, 2);
        assert!(
            progress
                .iter()
                .all(|state| state.completed_bytes <= bytes.len() as u64)
        );
    }

    #[test]
    fn concurrent_source_growth_is_rejected_without_publishing_or_unbounded_reading() {
        let (_temporary, source, target) = directories();
        let track = source.join("track");
        let original = vec![b'x'; COPY_BUFFER_BYTES * 2];
        fs::write(&track, &original).unwrap();
        let plan = validate_local_copy(
            &source,
            std::slice::from_ref(&track),
            &target,
            LocalMoveLimits::default(),
        )
        .unwrap();
        let mut mutated = false;
        let error = execute_local_copy_with_progress(&plan, |state| {
            assert!(state.completed_bytes <= original.len() as u64);
            if !mutated && state.completed_bytes > 0 {
                OpenOptions::new()
                    .append(true)
                    .open(&track)
                    .unwrap()
                    .write_all(b"concurrent data")
                    .unwrap();
                mutated = true;
            }
        })
        .unwrap_err();
        assert!(error.completed().is_empty());
        assert_eq!(
            fs::metadata(&track).unwrap().len(),
            (original.len() + b"concurrent data".len()) as u64
        );
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    }

    #[test]
    fn altered_staging_tree_is_rejected_and_cleaned_without_changing_source() {
        let (_temporary, source, target) = directories();
        let album = source.join("album");
        fs::create_dir(&album).unwrap();
        fs::write(album.join("track"), b"original").unwrap();
        let plan = validate_local_copy(
            &source,
            std::slice::from_ref(&album),
            &target,
            LocalMoveLimits::default(),
        )
        .unwrap();
        let mut altered = false;
        let error = execute_local_copy_with_progress(&plan, |state| {
            if !altered && state.completed_bytes > 0 {
                let stage = fs::read_dir(&target)
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path();
                fs::write(stage.join("payload/unexpected"), b"not from source").unwrap();
                altered = true;
            }
        })
        .unwrap_err();
        assert!(error.completed().is_empty());
        assert_eq!(fs::read(album.join("track")).unwrap(), b"original");
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn source_swapped_to_symlink_after_validation_is_not_opened() {
        use std::os::unix::fs::symlink;
        let (_temporary, source, target) = directories();
        let track = source.join("track");
        let kept = source.join("kept");
        fs::write(&track, b"original").unwrap();
        let plan = validate_local_copy(
            &source,
            std::slice::from_ref(&track),
            &target,
            LocalMoveLimits::default(),
        )
        .unwrap();
        fs::rename(&track, &kept).unwrap();
        symlink(&kept, &track).unwrap();
        assert!(execute_local_copy(&plan).is_err());
        assert_eq!(fs::read(kept).unwrap(), b"original");
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    }
}
