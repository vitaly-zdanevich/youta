//! Bounded, non-recursive filesystem browsing for the Local screen.
//!
//! This module deliberately separates read-only directory discovery from file
//! mutations. Directory listings never follow symbolic links, and callers must
//! provide an explicit [`LocalFileActions`] implementation before rename or
//! Trash operations can run.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

/// Default maximum number of directory entries inspected by one listing.
pub const DEFAULT_MAX_INSPECTED_ENTRIES: usize = 100_000;

/// Default maximum number of visible entries returned by one listing.
pub const DEFAULT_MAX_VISIBLE_ENTRIES: usize = 10_000;

/// Default maximum number of entries inspected while measuring one folder.
pub const DEFAULT_FOLDER_SIZE_MAX_ENTRIES: usize = 25_000;

/// Default maximum recursion depth while measuring one folder.
pub const DEFAULT_FOLDER_SIZE_MAX_DEPTH: usize = 64;

/// Resource limits applied to a single directory listing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalBrowseLimits {
    /// Maximum number of raw filesystem entries to inspect.
    pub max_inspected_entries: usize,
    /// Maximum number of entries visible under the selected options to return.
    pub max_visible_entries: usize,
}

impl Default for LocalBrowseLimits {
    fn default() -> Self {
        Self {
            max_inspected_entries: DEFAULT_MAX_INSPECTED_ENTRIES,
            max_visible_entries: DEFAULT_MAX_VISIBLE_ENTRIES,
        }
    }
}

/// Optional behavior for one local-directory listing.
///
/// Dot-prefixed directories and supported media files have always been
/// eligible for Local listings, so this option deliberately does not alter
/// their visibility. Enabling [`Self::show_all_files`] additionally returns
/// unsupported regular files, classified as [`LocalEntryKind::Text`] or
/// [`LocalEntryKind::Other`]. Symbolic links and special files remain hidden.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LocalBrowseOptions {
    /// Whether otherwise unsupported regular files should be returned.
    pub show_all_files: bool,
}

/// Resource limits applied to one recursive folder-size measurement.
///
/// A measurement that reaches either limit is discarded rather than shown as
/// a misleading partial size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalFolderSizeLimits {
    /// Maximum number of child entries inspected recursively.
    pub max_inspected_entries: usize,
    /// Maximum directory depth below the selected folder.
    pub max_depth: usize,
}

impl Default for LocalFolderSizeLimits {
    fn default() -> Self {
        Self {
            max_inspected_entries: DEFAULT_FOLDER_SIZE_MAX_ENTRIES,
            max_depth: DEFAULT_FOLDER_SIZE_MAX_DEPTH,
        }
    }
}

/// Stable identity of a real directory at one point in time.
///
/// The identity accompanies asynchronous folder-size results. Callers compare
/// it with a fresh identity before displaying a result, preventing an older
/// measurement from being attached to a path that was replaced meanwhile.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LocalDirectoryIdentity {
    path: PathBuf,
    length: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    /// Number the filesystem assigned, which a replacement cannot reuse.
    filesystem: Option<crate::file_identity::FilesystemIdentity>,
}

/// A complete recursive folder-size measurement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalFolderSizeMeasurement {
    /// Logical sum of regular-file lengths below the measured directory.
    pub bytes: u64,
    /// Directory identity captured before and after the stable traversal.
    pub identity: LocalDirectoryIdentity,
}

/// The type of an entry that the Local screen can display.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalEntryKind {
    /// A real directory that can be opened without traversing a symbolic link.
    Directory,
    /// A ZIP or RAR archive that behaves as a read-only local folder.
    #[cfg(feature = "local-archives")]
    Archive,
    /// A supported audio file.
    Audio,
    /// A supported video file. Youta may play audio from it.
    Video,
    /// A supported tracker-module music file.
    TrackerModule,
    /// A supported image file.
    Image,
    /// A conservatively recognized text file.
    Text,
    /// Another regular file included by the show-all-files option.
    Other,
}

impl LocalEntryKind {
    /// Returns whether the entry represents playable media.
    #[must_use]
    pub const fn is_playable(self) -> bool {
        matches!(self, Self::Audio | Self::Video | Self::TrackerModule)
    }
}

/// Pixel dimensions discovered from an image header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalImageDimensions {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
}

/// One visible entry in a local directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalEntry {
    /// Exact filesystem basename, including non-UTF-8 names on Unix.
    pub name: OsString,
    /// Absolute path beneath the canonical directory being listed.
    pub path: PathBuf,
    /// Display and activation behavior for this entry.
    pub kind: LocalEntryKind,
    /// File size in bytes, or `None` for directories.
    pub size_bytes: Option<u64>,
    /// Image dimensions when the thumbnail feature can read the image header.
    pub image_dimensions: Option<LocalImageDimensions>,
    /// Real-directory identity captured by the asynchronous listing worker.
    pub directory_identity: Option<LocalDirectoryIdentity>,
}

impl LocalEntry {
    /// Returns a lossy display label without changing the preserved basename.
    #[must_use]
    pub fn display_name(&self) -> std::borrow::Cow<'_, str> {
        self.name.to_string_lossy()
    }

    /// Returns whether this entry is a directory.
    #[must_use]
    pub const fn is_directory(&self) -> bool {
        matches!(self.kind, LocalEntryKind::Directory)
    }
}

/// A bounded snapshot of one local directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalDirectoryListing {
    /// Canonical absolute path of the listed directory.
    pub path: PathBuf,
    /// Canonical parent directory, or `None` at a filesystem root.
    pub parent: Option<PathBuf>,
    /// Visible child entries, with directories before files.
    pub entries: Vec<LocalEntry>,
    /// Whether an inspection or visible-entry limit stopped the listing.
    pub truncated: bool,
    /// Number of raw directory entries inspected.
    pub inspected_entries: usize,
}

/// Failures produced by local browsing or an explicitly requested file action.
#[derive(Debug, thiserror::Error)]
pub enum LocalBrowserError {
    /// A resource limit was zero and would make listing behavior ambiguous.
    #[error("local browser limits must be greater than zero")]
    InvalidLimits,
    /// A folder-size traversal reached its configured entry or depth bound.
    #[error("local folder size exceeds the configured traversal limits")]
    FolderSizeLimitReached,
    /// A newer Local route or disabled preference cancelled the measurement.
    #[error("local folder size measurement was cancelled")]
    FolderSizeCancelled,
    /// The directory changed while its recursive size was being measured.
    #[error("local folder changed while its size was being measured: `{0}`")]
    FolderChanged(PathBuf),
    /// The requested path points to a symbolic link.
    #[error("symbolic-link traversal is disabled for `{0}`")]
    SymbolicLink(PathBuf),
    /// The requested listing path is not a directory.
    #[error("local browser path is not a directory: `{0}`")]
    NotDirectory(PathBuf),
    /// The requested file action target is not a regular file.
    #[error("local file action target is not a regular file: `{0}`")]
    NotRegularFile(PathBuf),
    /// The requested Trash target is neither a regular file nor a directory.
    #[error("local Trash target is not a regular file or directory: `{0}`")]
    NotTrashableEntry(PathBuf),
    /// The requested Trash target is not an immediate child of the open folder.
    #[error("local Trash target is outside the open folder: `{0}`")]
    TrashTargetOutsideDirectory(PathBuf),
    /// A rename basename was empty, special, or contained a path component.
    #[error("new file name must be one non-empty basename")]
    InvalidRenameName,
    /// The requested rename would not change the basename.
    #[error("new file name is unchanged")]
    UnchangedRename,
    /// A rename target already exists and must not be replaced.
    #[error("rename target already exists: `{0}`")]
    RenameTargetExists(PathBuf),
    /// Filesystem metadata or directory contents could not be read.
    #[error("cannot inspect local path `{path}`: {source}")]
    Inspect {
        /// Path involved in the failed inspection.
        path: PathBuf,
        /// Operating-system error.
        #[source]
        source: io::Error,
    },
    /// The selected backend could not rename a validated file.
    #[error("cannot rename local file `{path}`: {source}")]
    Rename {
        /// Source path supplied to the backend.
        path: PathBuf,
        /// Backend error.
        #[source]
        source: io::Error,
    },
    /// The selected backend could not move a validated entry to Trash.
    #[error("cannot move local entry `{path}` to Trash: {source}")]
    Trash {
        /// File path supplied to the backend.
        path: PathBuf,
        /// Backend error.
        #[source]
        source: io::Error,
    },
}

/// Backend boundary for explicit mutations initiated from the Local screen.
///
/// A production implementation can integrate a platform Trash library without
/// coupling it to directory discovery. `rename_file_no_replace` must fail with
/// [`io::ErrorKind::AlreadyExists`] if `target` exists; using
/// [`fs::rename`] alone is not a compliant Unix implementation because it can
/// replace an existing file.
pub trait LocalFileActions {
    /// Renames `source` to `target` without replacing an existing target.
    ///
    /// # Errors
    ///
    /// Returns a backend or operating-system error. An existing target must
    /// produce [`io::ErrorKind::AlreadyExists`].
    fn rename_file_no_replace(&mut self, source: &Path, target: &Path) -> io::Result<()>;

    /// Moves a regular file or directory to the platform's recoverable Trash.
    ///
    /// # Errors
    ///
    /// Returns a backend or operating-system error when the move fails.
    fn move_file_to_trash(&mut self, path: &Path) -> io::Result<()>;
}

/// Production local-file actions selected by the granular mutation features.
///
/// On Unix, a same-directory regular-file rename is implemented by creating a
/// hard link at the validated new name and then unlinking the old name. The
/// link creation is atomic and fails if the destination appears after
/// validation, so an existing file is never overwritten. A method whose
/// corresponding `local-rename` or `local-trash` feature is omitted returns
/// [`io::ErrorKind::Unsupported`]; the TUI does not expose that action.
#[cfg(any(feature = "local-rename", feature = "local-trash"))]
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemLocalFileActions;

#[cfg(any(feature = "local-rename", feature = "local-trash"))]
impl LocalFileActions for SystemLocalFileActions {
    fn rename_file_no_replace(&mut self, source: &Path, target: &Path) -> io::Result<()> {
        #[cfg(feature = "local-rename")]
        {
            #[cfg(unix)]
            {
                fs::hard_link(source, target)?;
                if let Err(error) = fs::remove_file(source) {
                    let _ = fs::remove_file(target);
                    return Err(error);
                }
                Ok(())
            }
            #[cfg(not(unix))]
            {
                if target.exists() {
                    return Err(io::Error::from(io::ErrorKind::AlreadyExists));
                }
                fs::rename(source, target)
            }
        }
        #[cfg(not(feature = "local-rename"))]
        {
            let _ = (source, target);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this build omits local rename support",
            ))
        }
    }

    fn move_file_to_trash(&mut self, path: &Path) -> io::Result<()> {
        #[cfg(feature = "local-trash")]
        {
            trash::delete(path).map_err(io::Error::other)
        }
        #[cfg(not(feature = "local-trash"))]
        {
            let _ = path;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this build omits system Trash support",
            ))
        }
    }
}

/// Classifies a supported regular file by extension.
///
/// Non-UTF-8 basenames remain supported when their extension itself is valid
/// UTF-8. Unknown extensions return `None`.
#[must_use]
pub fn classify_local_file(path: &Path) -> Option<LocalEntryKind> {
    let extension = path.extension()?.to_str()?;
    if matches_ascii_case_insensitive(
        extension,
        &[
            "opus", "m4a", "aac", "flac", "wav", "mp3", "ogg", "oga", "mka",
        ],
    ) {
        Some(LocalEntryKind::Audio)
    } else if matches_ascii_case_insensitive(
        extension,
        &["webm", "mkv", "mp4", "m4v", "mov", "avi"],
    ) {
        Some(LocalEntryKind::Video)
    } else if matches_ascii_case_insensitive(
        extension,
        &["mod", "xm", "it", "s3m", "mptm", "stm", "mtm", "669"],
    ) {
        Some(LocalEntryKind::TrackerModule)
    } else if matches_ascii_case_insensitive(extension, &["jpg", "jpeg", "png", "webp"]) {
        Some(LocalEntryKind::Image)
    } else if cfg!(feature = "local-archives")
        && matches_ascii_case_insensitive(extension, &["zip", "rar"])
    {
        #[cfg(feature = "local-archives")]
        return Some(LocalEntryKind::Archive);
        #[cfg(not(feature = "local-archives"))]
        return None;
    } else {
        None
    }
}

/// Returns whether a basename conservatively identifies a text file.
///
/// Detection is intentionally based only on well-known extensions and a small
/// set of conventional extensionless documentation names. Directory listing
/// must stay non-blocking, so this function never opens a file to sniff its
/// contents or encoding. Callers should therefore treat a `true` result as an
/// editor hint rather than a guarantee that every byte is valid Unicode text.
#[must_use]
pub fn is_local_text_file(path: &Path) -> bool {
    if path.extension().is_some_and(|extension| {
        extension.to_str().is_some_and(|extension| {
            matches_ascii_case_insensitive(
                extension,
                &[
                    "txt",
                    "text",
                    "md",
                    "markdown",
                    "rst",
                    "org",
                    "adoc",
                    "asciidoc",
                    "nfo",
                    "log",
                    "csv",
                    "tsv",
                    "json",
                    "json5",
                    "yaml",
                    "yml",
                    "toml",
                    "xml",
                    "ini",
                    "cfg",
                    "conf",
                    "properties",
                    "cue",
                    "m3u",
                    "m3u8",
                    "rs",
                    "c",
                    "h",
                    "cc",
                    "cpp",
                    "cxx",
                    "hpp",
                    "py",
                    "rb",
                    "go",
                    "java",
                    "kt",
                    "kts",
                    "js",
                    "jsx",
                    "ts",
                    "tsx",
                    "sh",
                    "bash",
                    "zsh",
                    "fish",
                    "html",
                    "htm",
                    "css",
                    "scss",
                    "sass",
                    "less",
                    "sql",
                    "php",
                    "lua",
                    "vim",
                    "el",
                    "tex",
                    "bib",
                ],
            )
        })
    }) {
        return true;
    }

    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| {
            [
                "readme",
                "license",
                "copying",
                "notice",
                "authors",
                "contributors",
                "changelog",
                "changes",
                "install",
                "todo",
                "makefile",
                "dockerfile",
                ".gitignore",
                ".gitattributes",
                ".gitmodules",
                ".editorconfig",
                ".env",
                ".npmrc",
                ".bashrc",
                ".zshrc",
                ".profile",
                ".vimrc",
            ]
            .iter()
            .any(|candidate| name.eq_ignore_ascii_case(candidate))
        })
}

/// Lists supported entries in one directory without recursing or following
/// symbolic links.
///
/// Directories are sorted first, followed by supported files. Sorting preserves
/// exact [`OsString`] names and does not require a UTF-8 conversion. The listed
/// directory is canonicalized once; each returned child path is an absolute
/// path formed from that canonical directory and the exact child basename.
///
/// # Errors
///
/// Returns [`LocalBrowserError`] when limits are invalid, the selected path is
/// a symbolic link or non-directory, or filesystem inspection fails.
pub fn list_local_directory(
    path: &Path,
    limits: LocalBrowseLimits,
) -> Result<LocalDirectoryListing, LocalBrowserError> {
    list_local_directory_with_options(path, limits, LocalBrowseOptions::default())
}

/// Lists local entries using explicit visibility options.
///
/// This is the opt-in counterpart to [`list_local_directory`]. Its default
/// options produce the same media-and-directory-only listing as that function.
///
/// # Errors
///
/// Returns [`LocalBrowserError`] under the same conditions as
/// [`list_local_directory`].
pub fn list_local_directory_with_options(
    path: &Path,
    limits: LocalBrowseLimits,
    options: LocalBrowseOptions,
) -> Result<LocalDirectoryListing, LocalBrowserError> {
    list_local_directory_with_preferred_child_and_options(path, limits, None, options)
}

/// Lists supported entries while reserving space for one direct child.
///
/// `preferred_child` is a best-effort navigation hint, intended for restoring
/// selection after moving from a child directory to its parent. When it is a
/// supported, real, immediate child of the canonical listed directory, the
/// result includes it even if arbitrary [`fs::read_dir`] order would otherwise
/// place it beyond `max_visible_entries` or `max_inspected_entries`.
///
/// The hint does not broaden filesystem access: relative paths, descendants,
/// paths outside the listed directory, symbolic links, special files, missing
/// entries, and unsupported regular files are ignored. Direct validation adds
/// at most one bounded metadata lookup; [`LocalDirectoryListing::inspected_entries`]
/// continues to count only raw entries yielded by [`fs::read_dir`].
///
/// # Errors
///
/// Returns [`LocalBrowserError`] under the same conditions as
/// [`list_local_directory`]. An invalid preferred-child hint does not make an
/// otherwise valid directory listing fail.
pub fn list_local_directory_with_preferred_child(
    path: &Path,
    limits: LocalBrowseLimits,
    preferred_child: Option<&Path>,
) -> Result<LocalDirectoryListing, LocalBrowserError> {
    list_local_directory_with_preferred_child_and_options(
        path,
        limits,
        preferred_child,
        LocalBrowseOptions::default(),
    )
}

/// Lists local entries with a preferred-child hint and visibility options.
///
/// When `options.show_all_files` is enabled, an otherwise unsupported regular
/// file is also eligible for the preferred-child reservation. All safety and
/// resource-limit behavior matches
/// [`list_local_directory_with_preferred_child`].
///
/// # Errors
///
/// Returns [`LocalBrowserError`] under the same conditions as
/// [`list_local_directory_with_preferred_child`].
pub fn list_local_directory_with_preferred_child_and_options(
    path: &Path,
    limits: LocalBrowseLimits,
    preferred_child: Option<&Path>,
    options: LocalBrowseOptions,
) -> Result<LocalDirectoryListing, LocalBrowserError> {
    if limits.max_inspected_entries == 0 || limits.max_visible_entries == 0 {
        return Err(LocalBrowserError::InvalidLimits);
    }

    let requested_metadata =
        fs::symlink_metadata(path).map_err(|source| LocalBrowserError::Inspect {
            path: path.to_owned(),
            source,
        })?;
    if requested_metadata.file_type().is_symlink() {
        return Err(LocalBrowserError::SymbolicLink(path.to_owned()));
    }
    if !requested_metadata.is_dir() {
        return Err(LocalBrowserError::NotDirectory(path.to_owned()));
    }

    let directory =
        crate::fs_path::canonicalize(path).map_err(|source| LocalBrowserError::Inspect {
            path: path.to_owned(),
            source,
        })?;
    let preferred_entry = preferred_child
        .and_then(|path| direct_child_name(&directory, path))
        .and_then(|name| {
            inspect_visible_child(&directory, name, options)
                .ok()
                .flatten()
        });
    let preferred_name = preferred_entry.as_ref().map(|entry| entry.name.clone());
    let mut entries = preferred_entry.into_iter().collect::<Vec<_>>();
    let mut inspected_entries = 0_usize;
    let mut truncated = false;
    let read_directory = fs::read_dir(&directory).map_err(|source| LocalBrowserError::Inspect {
        path: directory.clone(),
        source,
    })?;

    for result in read_directory {
        if inspected_entries >= limits.max_inspected_entries {
            truncated = true;
            break;
        }
        let entry = result.map_err(|source| LocalBrowserError::Inspect {
            path: directory.clone(),
            source,
        })?;
        inspected_entries = inspected_entries.saturating_add(1);

        let name = entry.file_name();
        if preferred_name
            .as_ref()
            .is_some_and(|preferred| *preferred == name)
        {
            continue;
        }
        let Some(entry) = inspect_visible_child(&directory, name, options)? else {
            continue;
        };

        if entries.len() >= limits.max_visible_entries {
            truncated = true;
            break;
        }
        entries.push(entry);
    }

    entries.sort_by(|left, right| {
        let left_directory = left.kind == LocalEntryKind::Directory;
        let right_directory = right.kind == LocalEntryKind::Directory;
        right_directory
            .cmp(&left_directory)
            .then_with(|| left.name.cmp(&right.name))
    });

    Ok(LocalDirectoryListing {
        parent: directory.parent().map(Path::to_owned),
        path: directory,
        entries,
        truncated,
        inspected_entries,
    })
}

fn direct_child_name(directory: &Path, candidate: &Path) -> Option<OsString> {
    let relative = candidate.strip_prefix(directory).ok()?;
    let mut components = relative.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) => Some(name.to_os_string()),
        _ => None,
    }
}

fn inspect_visible_child(
    directory: &Path,
    name: OsString,
    options: LocalBrowseOptions,
) -> Result<Option<LocalEntry>, LocalBrowserError> {
    let entry_path = directory.join(&name);
    let metadata =
        fs::symlink_metadata(&entry_path).map_err(|source| LocalBrowserError::Inspect {
            path: entry_path.clone(),
            source,
        })?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Ok(None);
    }

    let kind = if file_type.is_dir() {
        Some(LocalEntryKind::Directory)
    } else if file_type.is_file() {
        classify_local_file(&entry_path).or_else(|| {
            options.show_all_files.then(|| {
                if is_local_text_file(&entry_path) {
                    LocalEntryKind::Text
                } else {
                    LocalEntryKind::Other
                }
            })
        })
    } else {
        None
    };
    let Some(kind) = kind else {
        return Ok(None);
    };

    let size_bytes = file_type.is_file().then_some(metadata.len());
    let directory_identity = (kind == LocalEntryKind::Directory)
        .then(|| directory_identity_from_metadata(&entry_path, &metadata));
    Ok(Some(LocalEntry {
        name,
        path: entry_path,
        kind,
        size_bytes,
        image_dimensions: None,
        directory_identity,
    }))
}

/// Finds a conventional cover image inside one real Local folder.
///
/// Filenames are matched case-insensitively with `cover.jpg` preferred over
/// `folder.jpg`, `cover.jpeg`, and `cover.png`. The function reads directory
/// metadata only: it neither opens nor decodes image contents, and it ignores
/// symbolic links and non-regular files.
///
/// # Errors
///
/// Returns [`LocalBrowserError`] when the folder is unsafe or cannot be read.
pub fn find_local_folder_cover(path: &Path) -> Result<Option<PathBuf>, LocalBrowserError> {
    let directory = validate_real_directory(path)?;
    let mut selected: Option<(u8, PathBuf)> = None;
    let entries = fs::read_dir(&directory).map_err(|source| LocalBrowserError::Inspect {
        path: directory.clone(),
        source,
    })?;
    for result in entries {
        let entry = result.map_err(|source| LocalBrowserError::Inspect {
            path: directory.clone(),
            source,
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_ascii_lowercase) else {
            continue;
        };
        let priority = match name.as_str() {
            "cover.jpg" => 0,
            "folder.jpg" => 1,
            "cover.jpeg" => 2,
            "cover.png" => 3,
            _ => continue,
        };
        if selected
            .as_ref()
            .is_some_and(|(selected_priority, _)| *selected_priority <= priority)
        {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|source| LocalBrowserError::Inspect {
                path: entry.path(),
                source,
            })?;
        if file_type.is_file() {
            selected = Some((priority, entry.path()));
        }
    }
    Ok(selected.map(|(_, path)| path))
}

/// Returns the current identity of a real directory without following a
/// symbolic link at the selected path.
///
/// # Errors
///
/// Returns [`LocalBrowserError`] when the path is a symbolic link, is not a
/// directory, or cannot be inspected.
pub fn local_directory_identity(path: &Path) -> Result<LocalDirectoryIdentity, LocalBrowserError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| LocalBrowserError::Inspect {
        path: path.to_owned(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(LocalBrowserError::SymbolicLink(path.to_owned()));
    }
    if !metadata.file_type().is_dir() {
        return Err(LocalBrowserError::NotDirectory(path.to_owned()));
    }
    Ok(directory_identity_from_metadata(path, &metadata))
}

fn directory_identity_from_metadata(
    path: &Path,
    metadata: &fs::Metadata,
) -> LocalDirectoryIdentity {
    LocalDirectoryIdentity {
        path: path.to_owned(),
        length: metadata.len(),
        modified: metadata.modified().ok(),
        created: metadata.created().ok(),
        filesystem: crate::file_identity::filesystem_identity(path, metadata),
    }
}

/// Measures the logical size of regular files below one real directory.
///
/// Symbolic links and special files are skipped, and directory symlinks are
/// never traversed. `cancelled` is checked before every filesystem entry so a
/// newer Local route or a disabled preference can stop work promptly. A
/// bounded or unstable traversal returns an error instead of a partial size.
///
/// # Errors
///
/// Returns [`LocalBrowserError`] when the root is invalid, an inspection
/// fails, the directory changes during traversal, the callback cancels the
/// work, or a configured resource limit is reached.
pub fn measure_local_folder_size<F>(
    path: &Path,
    limits: LocalFolderSizeLimits,
    cancelled: F,
) -> Result<LocalFolderSizeMeasurement, LocalBrowserError>
where
    F: Fn() -> bool,
{
    if limits.max_inspected_entries == 0 || limits.max_depth == 0 {
        return Err(LocalBrowserError::InvalidLimits);
    }
    if cancelled() {
        return Err(LocalBrowserError::FolderSizeCancelled);
    }

    let root_identity = local_directory_identity(path)?;
    let mut pending = vec![(path.to_owned(), 0_usize)];
    let mut inspected_entries = 0_usize;
    let mut bytes = 0_u64;

    while let Some((directory, depth)) = pending.pop() {
        if cancelled() {
            return Err(LocalBrowserError::FolderSizeCancelled);
        }
        let before = local_directory_identity(&directory)?;
        let entries = fs::read_dir(&directory).map_err(|source| LocalBrowserError::Inspect {
            path: directory.clone(),
            source,
        })?;
        for result in entries {
            if cancelled() {
                return Err(LocalBrowserError::FolderSizeCancelled);
            }
            inspected_entries = inspected_entries.saturating_add(1);
            if inspected_entries > limits.max_inspected_entries {
                return Err(LocalBrowserError::FolderSizeLimitReached);
            }
            let entry = result.map_err(|source| LocalBrowserError::Inspect {
                path: directory.clone(),
                source,
            })?;
            let entry_path = entry.path();
            let metadata =
                fs::symlink_metadata(&entry_path).map_err(|source| LocalBrowserError::Inspect {
                    path: entry_path.clone(),
                    source,
                })?;
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if depth >= limits.max_depth {
                    return Err(LocalBrowserError::FolderSizeLimitReached);
                }
                pending.push((entry_path, depth.saturating_add(1)));
            } else if file_type.is_file() {
                bytes = bytes
                    .checked_add(metadata.len())
                    .ok_or(LocalBrowserError::FolderSizeLimitReached)?;
                let after = fs::symlink_metadata(&entry_path).map_err(|source| {
                    LocalBrowserError::Inspect {
                        path: entry_path.clone(),
                        source,
                    }
                })?;
                if after.file_type().is_symlink()
                    || !after.file_type().is_file()
                    || after.len() != metadata.len()
                    || after.modified().ok() != metadata.modified().ok()
                {
                    return Err(LocalBrowserError::FolderChanged(entry_path));
                }
            }
        }
        if local_directory_identity(&directory)? != before {
            return Err(LocalBrowserError::FolderChanged(directory));
        }
    }

    if local_directory_identity(path)? != root_identity {
        return Err(LocalBrowserError::FolderChanged(path.to_owned()));
    }
    Ok(LocalFolderSizeMeasurement {
        bytes,
        identity: root_identity,
    })
}

/// Validates a safe, same-directory rename and returns its absolute target.
///
/// The validation rejects path separators, `.` and `..`, symbolic links,
/// non-files, unchanged names, and any existing target. The action backend must
/// still enforce no-replace semantics to close the check/action race.
///
/// # Errors
///
/// Returns [`LocalBrowserError`] when the source or basename is invalid, the
/// target exists, or filesystem inspection fails.
pub fn validate_local_rename(
    source: &Path,
    new_basename: &OsStr,
) -> Result<PathBuf, LocalBrowserError> {
    let mut components = Path::new(new_basename).components();
    let valid_basename = matches!(components.next(), Some(Component::Normal(name)) if name == new_basename)
        && components.next().is_none();
    if !valid_basename {
        return Err(LocalBrowserError::InvalidRenameName);
    }

    validate_regular_file(source)?;
    if source.file_name().is_some_and(|name| name == new_basename) {
        return Err(LocalBrowserError::UnchangedRename);
    }
    let parent = source
        .parent()
        .ok_or_else(|| LocalBrowserError::NotRegularFile(source.to_owned()))?;
    let target = parent.join(new_basename);
    match fs::symlink_metadata(&target) {
        Ok(_) => Err(LocalBrowserError::RenameTargetExists(target)),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(target),
        Err(source) => Err(LocalBrowserError::Inspect {
            path: target,
            source,
        }),
    }
}

/// Validates and executes an explicit no-overwrite rename.
///
/// # Errors
///
/// Returns [`LocalBrowserError`] when validation fails or the selected action
/// backend cannot complete the rename.
pub fn rename_local_file<A: LocalFileActions + ?Sized>(
    actions: &mut A,
    source: &Path,
    new_basename: &OsStr,
) -> Result<PathBuf, LocalBrowserError> {
    let target = validate_local_rename(source, new_basename)?;
    actions
        .rename_file_no_replace(source, &target)
        .map_err(|source_error| {
            if source_error.kind() == io::ErrorKind::AlreadyExists {
                LocalBrowserError::RenameTargetExists(target.clone())
            } else {
                LocalBrowserError::Rename {
                    path: source.to_owned(),
                    source: source_error,
                }
            }
        })?;
    Ok(target)
}

/// Validates and moves one regular file to recoverable Trash.
///
/// # Errors
///
/// Returns [`LocalBrowserError`] when the source is invalid or the selected
/// action backend cannot complete the move.
pub fn trash_local_file<A: LocalFileActions + ?Sized>(
    actions: &mut A,
    path: &Path,
) -> Result<(), LocalBrowserError> {
    validate_regular_file(path)?;
    actions
        .move_file_to_trash(path)
        .map_err(|source| LocalBrowserError::Trash {
            path: path.to_owned(),
            source,
        })
}

/// Validates and moves one immediate child entry to recoverable Trash.
///
/// Both regular files and real directories are accepted. The target must be
/// an immediate child of `open_directory`; symbolic links, special files,
/// filesystem roots, `..`, and paths outside the visible listing are rejected.
///
/// # Errors
///
/// Returns [`LocalBrowserError`] when either path is unsafe or the selected
/// action backend cannot complete the move.
pub fn trash_local_entry<A: LocalFileActions + ?Sized>(
    actions: &mut A,
    open_directory: &Path,
    path: &Path,
) -> Result<(), LocalBrowserError> {
    let directory = validate_real_directory(open_directory)?;
    let parent = path
        .parent()
        .ok_or_else(|| LocalBrowserError::TrashTargetOutsideDirectory(path.to_owned()))?;
    let canonical_parent =
        crate::fs_path::canonicalize(parent).map_err(|source| LocalBrowserError::Inspect {
            path: parent.to_owned(),
            source,
        })?;
    if canonical_parent != directory {
        return Err(LocalBrowserError::TrashTargetOutsideDirectory(
            path.to_owned(),
        ));
    }
    validate_trashable_entry(path)?;
    actions
        .move_file_to_trash(path)
        .map_err(|source| LocalBrowserError::Trash {
            path: path.to_owned(),
            source,
        })
}

fn validate_regular_file(path: &Path) -> Result<(), LocalBrowserError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| LocalBrowserError::Inspect {
        path: path.to_owned(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(LocalBrowserError::SymbolicLink(path.to_owned()));
    }
    if !metadata.file_type().is_file() {
        return Err(LocalBrowserError::NotRegularFile(path.to_owned()));
    }
    Ok(())
}

fn validate_real_directory(path: &Path) -> Result<PathBuf, LocalBrowserError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| LocalBrowserError::Inspect {
        path: path.to_owned(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(LocalBrowserError::SymbolicLink(path.to_owned()));
    }
    if !metadata.file_type().is_dir() {
        return Err(LocalBrowserError::NotDirectory(path.to_owned()));
    }
    crate::fs_path::canonicalize(path).map_err(|source| LocalBrowserError::Inspect {
        path: path.to_owned(),
        source,
    })
}

fn validate_trashable_entry(path: &Path) -> Result<(), LocalBrowserError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| LocalBrowserError::Inspect {
        path: path.to_owned(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(LocalBrowserError::SymbolicLink(path.to_owned()));
    }
    if !metadata.file_type().is_file() && !metadata.file_type().is_dir() {
        return Err(LocalBrowserError::NotTrashableEntry(path.to_owned()));
    }
    Ok(())
}

fn matches_ascii_case_insensitive(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::time::{Duration, Instant};

    use tempfile::TempDir;

    use super::*;

    #[derive(Default)]
    struct FakeFileActions {
        renames: RefCell<Vec<(PathBuf, PathBuf)>>,
        trashed: RefCell<Vec<PathBuf>>,
        rename_error: Option<io::ErrorKind>,
        trash_error: Option<io::ErrorKind>,
    }

    impl LocalFileActions for FakeFileActions {
        fn rename_file_no_replace(&mut self, source: &Path, target: &Path) -> io::Result<()> {
            if let Some(kind) = self.rename_error {
                return Err(io::Error::from(kind));
            }
            self.renames
                .borrow_mut()
                .push((source.to_owned(), target.to_owned()));
            Ok(())
        }

        fn move_file_to_trash(&mut self, path: &Path) -> io::Result<()> {
            if let Some(kind) = self.trash_error {
                return Err(io::Error::from(kind));
            }
            self.trashed.borrow_mut().push(path.to_owned());
            Ok(())
        }
    }

    fn write_file(path: &Path, contents: &[u8]) {
        fs::write(path, contents).expect("write fixture");
    }

    /// Temporary directory whose root is canonicalized once, up front.
    ///
    /// Listings report canonical paths, so expectations have to be built from a
    /// canonical root too. On Windows the raw [`TempDir`] path never compares
    /// equal to one: canonicalization rewrites 8.3 short components
    /// (`RUNNER~1` into `runneradmin`) and adds the `\\?\` verbatim prefix.
    struct Fixture {
        root: PathBuf,
        _directory: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = TempDir::new().expect("temporary fixture");
            let root =
                crate::fs_path::canonicalize(directory.path()).expect("canonical fixture root");
            Self {
                root,
                _directory: directory,
            }
        }

        fn path(&self) -> &Path {
            &self.root
        }
    }

    #[test]
    fn classifies_supported_media_and_image_extensions() {
        assert_eq!(
            classify_local_file(Path::new("voice.OPUS")),
            Some(LocalEntryKind::Audio)
        );
        assert_eq!(
            classify_local_file(Path::new("clip.mKv")),
            Some(LocalEntryKind::Video)
        );
        assert_eq!(
            classify_local_file(Path::new("demo.669")),
            Some(LocalEntryKind::TrackerModule)
        );
        assert_eq!(
            classify_local_file(Path::new("cover.WebP")),
            Some(LocalEntryKind::Image)
        );
        assert_eq!(classify_local_file(Path::new("notes.txt")), None);
    }

    #[cfg(feature = "local-archives")]
    #[test]
    fn classifies_zip_and_rar_archives_as_enterable_local_entries() {
        assert_eq!(
            classify_local_file(Path::new("album.ZIP")),
            Some(LocalEntryKind::Archive)
        );
        assert_eq!(
            classify_local_file(Path::new("collection.rAr")),
            Some(LocalEntryKind::Archive)
        );
    }

    #[test]
    fn conservatively_classifies_text_basenames_without_reading_contents() {
        assert!(is_local_text_file(Path::new("notes.TXT")));
        assert!(is_local_text_file(Path::new("settings.toml")));
        assert!(is_local_text_file(Path::new("script.rs")));
        assert!(is_local_text_file(Path::new("README")));
        assert!(is_local_text_file(Path::new("license")));
        assert!(is_local_text_file(Path::new("Makefile")));
        assert!(is_local_text_file(Path::new(".gitignore")));
        assert!(!is_local_text_file(Path::new("manual.pdf")));
        assert!(!is_local_text_file(Path::new("payload.bin")));
        assert!(!is_local_text_file(Path::new("README.backup")));
    }

    #[test]
    fn lists_supported_entries_nonrecursively_with_directories_first() {
        let fixture = Fixture::new();
        let album = fixture.path().join("album");
        fs::create_dir(&album).expect("create album");
        write_file(&album.join("nested.mp3"), b"nested");
        write_file(&fixture.path().join("a.mp3"), b"audio");
        write_file(&fixture.path().join("b.MKV"), b"video");
        write_file(&fixture.path().join("c.MOD"), b"tracker");
        write_file(&fixture.path().join("cover.PNG"), b"not a real image");
        write_file(&fixture.path().join("ignore.txt"), b"ignored");

        let listing =
            list_local_directory(fixture.path(), LocalBrowseLimits::default()).expect("listing");

        assert_eq!(
            listing.path,
            crate::fs_path::canonicalize(fixture.path()).unwrap()
        );
        assert_eq!(listing.parent, listing.path.parent().map(Path::to_owned));
        assert!(!listing.truncated);
        assert_eq!(listing.inspected_entries, 6);
        assert_eq!(
            listing
                .entries
                .iter()
                .map(|entry| (&entry.name, entry.kind))
                .collect::<Vec<_>>(),
            vec![
                (&OsString::from("album"), LocalEntryKind::Directory),
                (&OsString::from("a.mp3"), LocalEntryKind::Audio),
                (&OsString::from("b.MKV"), LocalEntryKind::Video),
                (&OsString::from("c.MOD"), LocalEntryKind::TrackerModule),
                (&OsString::from("cover.PNG"), LocalEntryKind::Image),
            ]
        );
        assert_eq!(listing.entries[1].size_bytes, Some(5));
        assert!(listing.entries.iter().all(|entry| entry.path.is_absolute()));
        assert!(
            listing
                .entries
                .iter()
                .all(|entry| entry.name != "nested.mp3")
        );
    }

    #[test]
    fn show_all_files_adds_text_and_other_regular_files_without_changing_default() {
        let fixture = Fixture::new();
        let album = fixture.path().join("album");
        fs::create_dir(&album).expect("create album");
        write_file(&fixture.path().join("song.mp3"), b"audio");
        write_file(&fixture.path().join("notes.TXT"), b"notes");
        write_file(&fixture.path().join("manual.pdf"), b"pdf");

        let default_listing =
            list_local_directory(fixture.path(), LocalBrowseLimits::default()).expect("listing");
        assert_eq!(
            default_listing
                .entries
                .iter()
                .map(|entry| (&entry.name, entry.kind))
                .collect::<Vec<_>>(),
            vec![
                (&OsString::from("album"), LocalEntryKind::Directory),
                (&OsString::from("song.mp3"), LocalEntryKind::Audio),
            ]
        );

        let all_listing = list_local_directory_with_options(
            fixture.path(),
            LocalBrowseLimits::default(),
            LocalBrowseOptions {
                show_all_files: true,
            },
        )
        .expect("show-all listing");
        assert_eq!(
            all_listing
                .entries
                .iter()
                .map(|entry| (&entry.name, entry.kind))
                .collect::<Vec<_>>(),
            vec![
                (&OsString::from("album"), LocalEntryKind::Directory),
                (&OsString::from("manual.pdf"), LocalEntryKind::Other),
                (&OsString::from("notes.TXT"), LocalEntryKind::Text),
                (&OsString::from("song.mp3"), LocalEntryKind::Audio),
            ]
        );
        assert_eq!(all_listing.entries[1].size_bytes, Some(3));
        assert_eq!(all_listing.entries[2].size_bytes, Some(5));
    }

    #[test]
    fn dot_prefixed_supported_entries_remain_visible_in_both_modes() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.path().join(".album")).expect("create hidden album");
        write_file(&fixture.path().join(".song.mp3"), b"audio");
        write_file(&fixture.path().join(".notes.txt"), b"notes");

        let default_listing =
            list_local_directory(fixture.path(), LocalBrowseLimits::default()).expect("listing");
        assert_eq!(
            default_listing
                .entries
                .iter()
                .map(|entry| (&entry.name, entry.kind))
                .collect::<Vec<_>>(),
            vec![
                (&OsString::from(".album"), LocalEntryKind::Directory),
                (&OsString::from(".song.mp3"), LocalEntryKind::Audio),
            ]
        );

        let all_listing = list_local_directory_with_options(
            fixture.path(),
            LocalBrowseLimits::default(),
            LocalBrowseOptions {
                show_all_files: true,
            },
        )
        .expect("show-all listing");
        assert_eq!(
            all_listing
                .entries
                .iter()
                .map(|entry| (&entry.name, entry.kind))
                .collect::<Vec<_>>(),
            vec![
                (&OsString::from(".album"), LocalEntryKind::Directory),
                (&OsString::from(".notes.txt"), LocalEntryKind::Text),
                (&OsString::from(".song.mp3"), LocalEntryKind::Audio),
            ]
        );
    }

    #[test]
    fn large_local_directory_listing_stays_within_wall_clock_budget() {
        const IMAGE_FILES: usize = 2_048;
        const AUDIO_FILES: usize = 1_024;
        const VIDEO_FILES: usize = 512;
        const TRACKER_FILES: usize = 256;
        const IGNORED_FILES: usize = 256;
        const EXPECTED_VISIBLE: usize = IMAGE_FILES + AUDIO_FILES + VIDEO_FILES + TRACKER_FILES;
        const EXPECTED_INSPECTED: usize = EXPECTED_VISIBLE + IGNORED_FILES;
        const INTERACTIVE_BUDGET: Duration = Duration::from_secs(1);

        let fixture = Fixture::new();
        for (count, extension) in [
            (IMAGE_FILES, "jpg"),
            (AUDIO_FILES, "flac"),
            (VIDEO_FILES, "mp4"),
            (TRACKER_FILES, "mod"),
            (IGNORED_FILES, "txt"),
        ] {
            for index in 0..count {
                write_file(
                    &fixture
                        .path()
                        .join(format!("{extension}-{index:04}.{extension}")),
                    b"x",
                );
            }
        }

        let started = Instant::now();
        let listing =
            list_local_directory(fixture.path(), LocalBrowseLimits::default()).expect("listing");
        let elapsed = started.elapsed();

        println!(
            "listed {EXPECTED_INSPECTED} mock Local entries ({EXPECTED_VISIBLE} visible) in {elapsed:?}"
        );
        assert_eq!(listing.inspected_entries, EXPECTED_INSPECTED);
        assert_eq!(listing.entries.len(), EXPECTED_VISIBLE);
        assert!(!listing.truncated);
        assert!(
            listing
                .entries
                .iter()
                .all(|entry| entry.image_dimensions.is_none()),
            "the foreground listing path must not open image payloads"
        );
        assert!(
            elapsed <= INTERACTIVE_BUDGET,
            "listing {EXPECTED_INSPECTED} Local entries took {elapsed:?}, exceeding the interactive {INTERACTIVE_BUDGET:?} budget"
        );
    }

    #[test]
    fn applies_visible_and_inspection_limits() {
        let visible_fixture = Fixture::new();
        for index in 0..3 {
            write_file(
                &visible_fixture.path().join(format!("{index}.mp3")),
                b"audio",
            );
        }
        let visible = list_local_directory(
            visible_fixture.path(),
            LocalBrowseLimits {
                max_inspected_entries: 10,
                max_visible_entries: 2,
            },
        )
        .expect("bounded visible listing");
        assert_eq!(visible.entries.len(), 2);
        assert!(visible.truncated);
        assert_eq!(visible.inspected_entries, 3);

        let inspected_fixture = Fixture::new();
        for index in 0..3 {
            write_file(
                &inspected_fixture.path().join(format!("{index}.txt")),
                b"ignored",
            );
        }
        let inspected = list_local_directory(
            inspected_fixture.path(),
            LocalBrowseLimits {
                max_inspected_entries: 2,
                max_visible_entries: 10,
            },
        )
        .expect("bounded inspection");
        assert!(inspected.entries.is_empty());
        assert!(inspected.truncated);
        assert_eq!(inspected.inspected_entries, 2);
    }

    #[test]
    fn preferred_direct_child_survives_tiny_listing_limits() {
        let fixture = Fixture::new();
        write_file(&fixture.path().join("first.mp3"), b"first");
        write_file(&fixture.path().join("middle.mp3"), b"middle");
        let preferred = fixture.path().join("selected.mp3");
        write_file(&preferred, b"selected");

        let listing = list_local_directory_with_preferred_child(
            fixture.path(),
            LocalBrowseLimits {
                max_inspected_entries: 1,
                max_visible_entries: 1,
            },
            Some(&preferred),
        )
        .expect("listing with reserved preferred child");

        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].path, preferred);
        assert!(listing.truncated);
        assert!(listing.inspected_entries <= 1);
    }

    #[test]
    fn show_all_files_can_reserve_an_unsupported_preferred_child() {
        let fixture = Fixture::new();
        write_file(&fixture.path().join("first.mp3"), b"first");
        let preferred = fixture.path().join("notes.txt");
        write_file(&preferred, b"notes");

        let listing = list_local_directory_with_preferred_child_and_options(
            fixture.path(),
            LocalBrowseLimits {
                max_inspected_entries: 1,
                max_visible_entries: 1,
            },
            Some(&preferred),
            LocalBrowseOptions {
                show_all_files: true,
            },
        )
        .expect("show-all listing with reserved preferred child");

        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].path, preferred);
        assert_eq!(listing.entries[0].kind, LocalEntryKind::Text);
        assert!(listing.truncated);
        assert!(listing.inspected_entries <= 1);
    }

    #[cfg(unix)]
    #[test]
    fn show_all_files_still_ignores_symbolic_links() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let target = fixture.path().join("target.bin");
        write_file(&target, b"target");
        symlink(&target, fixture.path().join("link.bin")).expect("create symlink");

        let listing = list_local_directory_with_options(
            fixture.path(),
            LocalBrowseLimits::default(),
            LocalBrowseOptions {
                show_all_files: true,
            },
        )
        .expect("show-all listing");

        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].path, target);
        assert_eq!(listing.entries[0].kind, LocalEntryKind::Other);
    }

    #[test]
    fn invalid_preferred_hints_do_not_reserve_a_visible_slot() {
        let fixture = Fixture::new();
        let outside = Fixture::new();
        let playable = fixture.path().join("song.mp3");
        write_file(&playable, b"audio");
        let unsupported = fixture.path().join("notes.txt");
        write_file(&unsupported, b"notes");
        let missing = fixture.path().join("missing.mp3");
        let outside_file = outside.path().join("outside.mp3");
        write_file(&outside_file, b"outside");
        let nested_directory = fixture.path().join("album");
        fs::create_dir(&nested_directory).expect("create nested directory");
        let nested = nested_directory.join("nested.mp3");
        write_file(&nested, b"nested");
        let limits = LocalBrowseLimits {
            max_inspected_entries: 10,
            max_visible_entries: 1,
        };

        for invalid_hint in [
            unsupported.as_path(),
            missing.as_path(),
            outside_file.as_path(),
            nested.as_path(),
            Path::new("song.mp3"),
        ] {
            let listing = list_local_directory_with_preferred_child(
                fixture.path(),
                limits,
                Some(invalid_hint),
            )
            .expect("invalid hint must not break listing");
            assert_eq!(listing.entries.len(), 1);
            assert_ne!(listing.entries[0].path, invalid_hint);
        }
    }

    #[cfg(unix)]
    #[test]
    fn preferred_symbolic_link_is_ignored_without_following_its_target() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let outside = Fixture::new();
        let playable = fixture.path().join("song.mp3");
        write_file(&playable, b"audio");
        let outside_file = outside.path().join("outside.mp3");
        write_file(&outside_file, b"outside");
        let preferred_link = fixture.path().join("selected.mp3");
        symlink(&outside_file, &preferred_link).expect("create preferred symlink");

        let listing = list_local_directory_with_preferred_child(
            fixture.path(),
            LocalBrowseLimits {
                max_inspected_entries: 10,
                max_visible_entries: 1,
            },
            Some(&preferred_link),
        )
        .expect("symlink hint must not break listing");

        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].path, playable);
        assert_ne!(listing.entries[0].path, outside_file);
    }

    #[test]
    fn measures_nested_folder_size_with_strict_resource_bounds() {
        let fixture = Fixture::new();
        let album = fixture.path().join("album");
        let disc = album.join("disc");
        fs::create_dir_all(&disc).expect("create nested directories");
        write_file(&album.join("song.mp3"), b"audio");
        write_file(&disc.join("notes.txt"), b"metadata");

        let measured = measure_local_folder_size(
            &album,
            LocalFolderSizeLimits {
                max_inspected_entries: 4,
                max_depth: 2,
            },
            || false,
        )
        .expect("bounded complete measurement");

        assert_eq!(measured.bytes, 13);
        assert_eq!(
            measured.identity,
            local_directory_identity(&album).expect("stable directory identity")
        );
    }

    #[test]
    fn folder_size_returns_no_partial_value_after_limit_or_cancellation() {
        let fixture = Fixture::new();
        write_file(&fixture.path().join("one.bin"), b"one");
        write_file(&fixture.path().join("two.bin"), b"two");

        assert!(matches!(
            measure_local_folder_size(
                fixture.path(),
                LocalFolderSizeLimits {
                    max_inspected_entries: 1,
                    max_depth: 1,
                },
                || false,
            ),
            Err(LocalBrowserError::FolderSizeLimitReached)
        ));

        let checks = Cell::new(0_usize);
        assert!(matches!(
            measure_local_folder_size(fixture.path(), LocalFolderSizeLimits::default(), || {
                checks.set(checks.get().saturating_add(1));
                checks.get() >= 3
            },),
            Err(LocalBrowserError::FolderSizeCancelled)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn folder_size_never_counts_or_traverses_symbolic_links() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let outside = Fixture::new();
        write_file(&fixture.path().join("inside.bin"), b"in");
        write_file(&outside.path().join("outside.bin"), b"outside");
        symlink(
            outside.path().join("outside.bin"),
            fixture.path().join("file-link.bin"),
        )
        .expect("create file symlink");
        symlink(outside.path(), fixture.path().join("directory-link"))
            .expect("create directory symlink");

        let measured =
            measure_local_folder_size(fixture.path(), LocalFolderSizeLimits::default(), || false)
                .expect("measure without following links");
        assert_eq!(measured.bytes, 2);
        assert!(matches!(
            measure_local_folder_size(
                &fixture.path().join("directory-link"),
                LocalFolderSizeLimits::default(),
                || false,
            ),
            Err(LocalBrowserError::SymbolicLink(_))
        ));
    }

    #[test]
    fn directory_identity_changes_when_the_path_is_replaced() {
        let fixture = Fixture::new();
        let selected = fixture.path().join("selected");
        let replacement = fixture.path().join("replacement");
        fs::create_dir(&selected).expect("create selected directory");
        fs::create_dir(&replacement).expect("create replacement directory");
        let before = local_directory_identity(&selected).expect("initial identity");
        fs::rename(&selected, fixture.path().join("old")).expect("move original directory");
        fs::rename(&replacement, &selected).expect("replace selected path");
        let after = local_directory_identity(&selected).expect("replacement identity");

        assert_ne!(before, after);
    }

    #[test]
    fn listing_rejects_zero_limits() {
        let fixture = Fixture::new();
        let error = list_local_directory(
            fixture.path(),
            LocalBrowseLimits {
                max_inspected_entries: 0,
                max_visible_entries: 1,
            },
        )
        .unwrap_err();
        assert!(matches!(error, LocalBrowserError::InvalidLimits));
    }

    #[test]
    fn rename_validation_rejects_components_and_existing_targets() {
        let fixture = Fixture::new();
        let source = fixture.path().join("source.mp3");
        let target = fixture.path().join("target.mp3");
        write_file(&source, b"source");
        write_file(&target, b"target");

        assert!(matches!(
            validate_local_rename(&source, OsStr::new("../escape.mp3")),
            Err(LocalBrowserError::InvalidRenameName)
        ));
        assert!(matches!(
            validate_local_rename(&source, OsStr::new("source.mp3")),
            Err(LocalBrowserError::UnchangedRename)
        ));
        assert!(matches!(
            validate_local_rename(&source, OsStr::new("target.mp3")),
            Err(LocalBrowserError::RenameTargetExists(path)) if path == target
        ));
    }

    #[test]
    fn explicit_actions_are_dispatched_only_after_validation() {
        let fixture = Fixture::new();
        let source = fixture.path().join("source.mp3");
        write_file(&source, b"source");
        let mut actions = FakeFileActions::default();

        let target = rename_local_file(&mut actions, &source, OsStr::new("renamed.mp3"))
            .expect("validated rename");
        trash_local_file(&mut actions, &source).expect("validated Trash");

        assert_eq!(
            actions.renames.into_inner(),
            vec![(source.clone(), target.clone())]
        );
        assert_eq!(actions.trashed.into_inner(), vec![source]);
        assert_eq!(target.file_name(), Some(OsStr::new("renamed.mp3")));
    }

    #[test]
    fn trash_accepts_only_immediate_file_and_directory_children() {
        let fixture = Fixture::new();
        let outside = Fixture::new();
        let file = fixture.path().join("song.mp3");
        let directory = fixture.path().join("album");
        let outside_file = outside.path().join("outside.mp3");
        write_file(&file, b"audio");
        fs::create_dir(&directory).expect("create album directory");
        write_file(&outside_file, b"outside");
        let mut actions = FakeFileActions::default();

        trash_local_entry(&mut actions, fixture.path(), &file).expect("Trash local file");
        trash_local_entry(&mut actions, fixture.path(), &directory).expect("Trash local directory");
        let error = trash_local_entry(&mut actions, fixture.path(), &outside_file)
            .expect_err("reject outside entry");

        assert!(matches!(
            error,
            LocalBrowserError::TrashTargetOutsideDirectory(path) if path == outside_file
        ));
        assert_eq!(actions.trashed.into_inner(), vec![file, directory]);
    }

    #[test]
    fn backend_collision_is_preserved_as_no_overwrite_error() {
        let fixture = Fixture::new();
        let source = fixture.path().join("source.mp3");
        write_file(&source, b"source");
        let mut actions = FakeFileActions {
            rename_error: Some(io::ErrorKind::AlreadyExists),
            ..FakeFileActions::default()
        };

        let error = rename_local_file(&mut actions, &source, OsStr::new("raced.mp3"))
            .expect_err("backend collision");
        assert!(matches!(
            error,
            LocalBrowserError::RenameTargetExists(path)
                if path == fixture.path().join("raced.mp3")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn skips_child_symlinks_and_rejects_a_symlink_root() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let real = fixture.path().join("real");
        fs::create_dir(&real).expect("create real directory");
        write_file(&real.join("song.mp3"), b"audio");
        symlink(real.join("song.mp3"), fixture.path().join("song-link.mp3"))
            .expect("create file symlink");
        symlink(&real, fixture.path().join("directory-link")).expect("create directory symlink");

        let listing =
            list_local_directory(fixture.path(), LocalBrowseLimits::default()).expect("listing");
        assert!(listing.entries.iter().all(|entry| {
            entry.name != OsStr::new("song-link.mp3") && entry.name != OsStr::new("directory-link")
        }));
        assert!(matches!(
            list_local_directory(
                &fixture.path().join("directory-link"),
                LocalBrowseLimits::default()
            ),
            Err(LocalBrowserError::SymbolicLink(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn preserves_a_non_utf8_basename() {
        use std::os::unix::ffi::OsStringExt;

        let fixture = Fixture::new();
        let name = OsString::from_vec(b"song-\xff.mp3".to_vec());
        write_file(&fixture.path().join(&name), b"audio");

        let listing =
            list_local_directory(fixture.path(), LocalBrowseLimits::default()).expect("listing");
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].name, name);
        assert_eq!(listing.entries[0].kind, LocalEntryKind::Audio);
    }

    #[test]
    fn listing_does_not_open_images_for_dimensions() {
        let fixture = Fixture::new();
        let path = fixture.path().join("cover.png");
        write_file(&path, b"not read by directory listing");

        let listing =
            list_local_directory(fixture.path(), LocalBrowseLimits::default()).expect("listing");
        assert_eq!(listing.entries[0].image_dimensions, None);
    }

    #[test]
    fn folder_cover_discovery_is_case_insensitive_and_prefers_cover_jpg() {
        let fixture = Fixture::new();
        let album = fixture.path().join("album");
        fs::create_dir(&album).expect("create album");
        write_file(&album.join("cover.png"), b"png");
        write_file(&album.join("FoLdEr.JpG"), b"folder");
        write_file(&album.join("CoVeR.JpG"), b"jpg");

        assert_eq!(
            find_local_folder_cover(&album).expect("discover cover"),
            Some(album.join("CoVeR.JpG"))
        );
    }

    #[test]
    fn folder_jpg_is_used_when_cover_jpg_is_absent() {
        let fixture = Fixture::new();
        let album = fixture.path().join("album");
        fs::create_dir(&album).expect("create album");
        write_file(&album.join("FOLDER.JPG"), b"folder");

        assert_eq!(
            find_local_folder_cover(&album).expect("discover folder artwork"),
            Some(album.join("FOLDER.JPG"))
        );
    }
}
