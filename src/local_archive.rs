//! Safe, bounded materialization of ZIP and RAR files for Local browsing.
//!
//! Archive contents are extracted into a private, regenerable cache keyed by
//! the source path and file metadata. The Local controller treats that cache
//! as a read-only virtual folder: playback and metadata probing receive normal
//! paths, while rename, move, and Trash actions remain disabled.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read as _, Write as _};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};

/// Maximum archive entries inspected during one materialization.
pub const DEFAULT_MAX_ARCHIVE_ENTRIES: usize = 100_000;

/// Maximum uncompressed size accepted for one archive member.
pub const DEFAULT_MAX_ARCHIVE_MEMBER_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Maximum combined uncompressed size accepted for one archive.
pub const DEFAULT_MAX_ARCHIVE_TOTAL_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// Maximum nested directory depth accepted inside an archive.
pub const DEFAULT_MAX_ARCHIVE_DEPTH: usize = 64;

/// Maximum UTF-8 byte length accepted for one member path.
pub const DEFAULT_MAX_ARCHIVE_PATH_BYTES: usize = 4_096;

/// Maximum technical-listing output accepted from the RAR helper.
pub const DEFAULT_MAX_RAR_LISTING_BYTES: u64 = 64 * 1024 * 1024;

const CACHE_COMPLETE_MARKER: &str = ".youta-complete";
const CACHE_CONTENTS_DIRECTORY: &str = "contents";
const POSIX_FILE_TYPE_MASK: u32 = 0o170_000;
const POSIX_REGULAR_FILE: u32 = 0o100_000;

/// Resource bounds for one archive opened through the Local browser.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalArchiveLimits {
    /// Maximum number of member headers inspected.
    pub max_entries: usize,
    /// Maximum bytes extracted from one regular-file member.
    pub max_member_bytes: u64,
    /// Maximum bytes extracted from all regular-file members.
    pub max_total_bytes: u64,
    /// Maximum number of normal path components in one member.
    pub max_depth: usize,
    /// Maximum UTF-8 byte length of one member path.
    pub max_path_bytes: usize,
}

impl Default for LocalArchiveLimits {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ARCHIVE_ENTRIES,
            max_member_bytes: DEFAULT_MAX_ARCHIVE_MEMBER_BYTES,
            max_total_bytes: DEFAULT_MAX_ARCHIVE_TOTAL_BYTES,
            max_depth: DEFAULT_MAX_ARCHIVE_DEPTH,
            max_path_bytes: DEFAULT_MAX_ARCHIVE_PATH_BYTES,
        }
    }
}

/// One archive and the private directory that represents its contents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedLocalArchive {
    /// Canonical source ZIP or RAR path.
    pub source_path: PathBuf,
    /// Canonical directory exposed to the existing Local folder browser.
    pub root_path: PathBuf,
    /// Whether an already complete cache entry was reused.
    pub reused_cache: bool,
}

impl MaterializedLocalArchive {
    /// Returns the user-facing archive path for one directory inside the cache.
    #[must_use]
    pub fn display_path(&self, directory: &Path) -> String {
        let relative = directory
            .strip_prefix(&self.root_path)
            .unwrap_or_else(|_| Path::new(""));
        if relative.as_os_str().is_empty() {
            format!("{}!/", self.source_path.display())
        } else {
            format!("{}!/{}", self.source_path.display(), relative.display())
        }
    }

    /// Returns whether a path belongs to this materialized archive.
    #[must_use]
    pub fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.root_path)
    }
}

/// Failures produced while opening a ZIP or RAR as a Local folder.
#[derive(Debug, thiserror::Error)]
pub enum LocalArchiveError {
    /// At least one configured safety bound was zero.
    #[error("local archive limits must be greater than zero")]
    InvalidLimits,
    /// The selected extension is not part of the ZIP/RAR feature.
    #[error("only ZIP and RAR archives can be opened as local folders: `{0}`")]
    UnsupportedExtension(PathBuf),
    /// The selected source is a symbolic link.
    #[error("symbolic-link archive sources are disabled: `{0}`")]
    SymbolicLink(PathBuf),
    /// The selected source is not a regular file.
    #[error("local archive source is not a regular file: `{0}`")]
    NotRegularFile(PathBuf),
    /// A member path was absolute, traversed upward, or otherwise ambiguous.
    #[error("unsafe archive member path `{0}`")]
    UnsafeMemberPath(String),
    /// An archive contains a link, device, or another non-file entry.
    #[error("unsupported archive member type for `{0}`")]
    UnsupportedMemberType(String),
    /// One configured resource bound was exceeded.
    #[error("archive resource limit exceeded: {0}")]
    LimitExceeded(String),
    /// Two members would occupy the same extracted path.
    #[error("archive contains a duplicate or conflicting path: `{0}`")]
    DuplicateMember(PathBuf),
    /// Filesystem access failed.
    #[error("could not access `{path}`: {source}")]
    Io {
        /// Path being accessed.
        path: PathBuf,
        /// Operating-system failure.
        #[source]
        source: io::Error,
    },
    /// An archive decoder or helper could not read the source.
    #[error("could not read the archive: {0}")]
    Archive(String),
}

struct BoundedMemberWriter<'a> {
    path: PathBuf,
    file: File,
    written_bytes: u64,
    total_written: &'a mut u64,
    limits: LocalArchiveLimits,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RarMemberKind {
    Directory,
    File,
}

#[derive(Debug, Eq, PartialEq)]
struct RarMember {
    raw_name: String,
    relative_path: PathBuf,
    kind: RarMemberKind,
    declared_bytes: u64,
}

#[derive(Default)]
struct PendingRarMember {
    name: Option<String>,
    kind: Option<RarMemberKind>,
    declared_bytes: Option<u64>,
}

/// Materializes one ZIP or RAR into a private, content-addressed cache.
///
/// Member bodies are streamed, never accumulated in memory. Paths containing
/// traversal components and all links or special files are rejected before
/// their data can be written. A complete cached extraction is reused while the
/// canonical source path, length, and modification timestamp stay unchanged.
///
/// # Errors
///
/// Returns [`LocalArchiveError`] when the source is unsafe, malformed,
/// unsupported, exceeds a bound, or cannot be cached privately.
pub fn materialize_local_archive(
    source: &Path,
    cache_directory: &Path,
    limits: LocalArchiveLimits,
) -> Result<MaterializedLocalArchive, LocalArchiveError> {
    materialize_local_archive_with_rar_program(source, cache_directory, limits, Path::new("unrar"))
}

fn materialize_local_archive_with_rar_program(
    source: &Path,
    cache_directory: &Path,
    limits: LocalArchiveLimits,
    rar_program: &Path,
) -> Result<MaterializedLocalArchive, LocalArchiveError> {
    validate_limits(limits)?;
    validate_archive_extension(source)?;
    let source_metadata =
        fs::symlink_metadata(source).map_err(|source_error| LocalArchiveError::Io {
            path: source.to_owned(),
            source: source_error,
        })?;
    if source_metadata.file_type().is_symlink() {
        return Err(LocalArchiveError::SymbolicLink(source.to_owned()));
    }
    if !source_metadata.is_file() {
        return Err(LocalArchiveError::NotRegularFile(source.to_owned()));
    }
    let source_path =
        crate::fs_path::canonicalize(source).map_err(|source_error| LocalArchiveError::Io {
            path: source.to_owned(),
            source: source_error,
        })?;
    crate::private_files::create_private_directory(cache_directory).map_err(|source_error| {
        LocalArchiveError::Io {
            path: cache_directory.to_owned(),
            source: source_error,
        }
    })?;

    let key = archive_cache_key(&source_path);
    let revision = archive_cache_revision(&source_metadata);
    let cache_entry = cache_directory.join(key);
    if let Some(root_path) = complete_cache_root(&cache_entry, &revision) {
        return Ok(MaterializedLocalArchive {
            source_path,
            root_path,
            reused_cache: true,
        });
    }
    if cache_entry.exists() {
        fs::remove_dir_all(&cache_entry).map_err(|source_error| LocalArchiveError::Io {
            path: cache_entry.clone(),
            source: source_error,
        })?;
    }

    let temporary = unique_temporary_directory(cache_directory, &cache_entry)?;
    let root = temporary.join(CACHE_CONTENTS_DIRECTORY);
    crate::private_files::create_private_directory(&root).map_err(|source_error| {
        LocalArchiveError::Io {
            path: root.clone(),
            source: source_error,
        }
    })?;
    if let Err(error) = extract_archive(&source_path, &root, limits, rar_program) {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }
    let marker = temporary.join(CACHE_COMPLETE_MARKER);
    let mut marker_options = OpenOptions::new();
    marker_options.write(true).create_new(true);
    let mut marker_file = crate::private_files::open_privately(&mut marker_options)
        .open(&marker)
        .map_err(|source_error| LocalArchiveError::Io {
            path: marker.clone(),
            source: source_error,
        })?;
    marker_file
        .write_all(revision.as_bytes())
        .and_then(|()| marker_file.sync_all())
        .map_err(|source_error| LocalArchiveError::Io {
            path: marker.clone(),
            source: source_error,
        })?;
    // Windows refuses to rename a directory while a file beneath it is open.
    drop(marker_file);
    if let Err(source_error) = fs::rename(&temporary, &cache_entry) {
        let _ = fs::remove_dir_all(&temporary);
        return Err(LocalArchiveError::Io {
            path: cache_entry,
            source: source_error,
        });
    }
    let root_path = crate::fs_path::canonicalize(cache_entry.join(CACHE_CONTENTS_DIRECTORY))
        .map_err(|source_error| LocalArchiveError::Io {
            path: cache_entry.join(CACHE_CONTENTS_DIRECTORY),
            source: source_error,
        })?;
    Ok(MaterializedLocalArchive {
        source_path,
        root_path,
        reused_cache: false,
    })
}

fn validate_limits(limits: LocalArchiveLimits) -> Result<(), LocalArchiveError> {
    if limits.max_entries == 0
        || limits.max_member_bytes == 0
        || limits.max_total_bytes == 0
        || limits.max_depth == 0
        || limits.max_path_bytes == 0
    {
        return Err(LocalArchiveError::InvalidLimits);
    }
    Ok(())
}

fn validate_archive_extension(source: &Path) -> Result<(), LocalArchiveError> {
    let supported = source
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("zip") || extension.eq_ignore_ascii_case("rar")
        });
    if supported {
        Ok(())
    } else {
        Err(LocalArchiveError::UnsupportedExtension(source.to_owned()))
    }
}

fn archive_cache_key(source: &Path) -> String {
    let mut digest = Sha256::new();
    hash_os_str(&mut digest, source.as_os_str());
    let mut key = String::with_capacity(64);
    for byte in digest.finalize() {
        let _ = write!(&mut key, "{byte:02x}");
    }
    key
}

fn archive_cache_revision(metadata: &fs::Metadata) -> String {
    let mut revision = format!("Youta local archive cache v1\nlength={}\n", metadata.len());
    if let Ok(modified) = metadata.modified() {
        let duration = modified.duration_since(UNIX_EPOCH).unwrap_or_default();
        let _ = writeln!(
            revision,
            "modified={}.{}",
            duration.as_secs(),
            duration.subsec_nanos()
        );
    } else {
        revision.push_str("modified=unknown\n");
    }
    revision
}

#[cfg(unix)]
fn hash_os_str(digest: &mut Sha256, value: &OsStr) {
    use std::os::unix::ffi::OsStrExt;
    digest.update(value.as_bytes());
}

#[cfg(windows)]
fn hash_os_str(digest: &mut Sha256, value: &OsStr) {
    use std::os::windows::ffi::OsStrExt;
    for unit in value.encode_wide() {
        digest.update(unit.to_le_bytes());
    }
}

#[cfg(not(any(unix, windows)))]
fn hash_os_str(digest: &mut Sha256, value: &OsStr) {
    digest.update(value.to_string_lossy().as_bytes());
}

fn complete_cache_root(cache_entry: &Path, expected_revision: &str) -> Option<PathBuf> {
    let marker = fs::symlink_metadata(cache_entry.join(CACHE_COMPLETE_MARKER)).ok()?;
    let contents = fs::symlink_metadata(cache_entry.join(CACHE_CONTENTS_DIRECTORY)).ok()?;
    if marker.file_type().is_symlink()
        || !marker.is_file()
        || marker.len() != u64::try_from(expected_revision.len()).ok()?
        || fs::read(cache_entry.join(CACHE_COMPLETE_MARKER)).ok()? != expected_revision.as_bytes()
    {
        return None;
    }
    if contents.file_type().is_symlink() || !contents.is_dir() {
        return None;
    }
    crate::fs_path::canonicalize(cache_entry.join(CACHE_CONTENTS_DIRECTORY)).ok()
}

fn unique_temporary_directory(
    cache_directory: &Path,
    cache_entry: &Path,
) -> Result<PathBuf, LocalArchiveError> {
    let stem = cache_entry
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("archive");
    for attempt in 0_u32..1_024 {
        let candidate =
            cache_directory.join(format!(".{stem}.partial-{}-{attempt}", std::process::id()));
        match fs::create_dir(&candidate) {
            Ok(()) => {
                crate::private_files::set_private_directory_permissions(&candidate).map_err(
                    |source_error| LocalArchiveError::Io {
                        path: candidate.clone(),
                        source: source_error,
                    },
                )?;
                return Ok(candidate);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(source_error) => {
                return Err(LocalArchiveError::Io {
                    path: candidate,
                    source: source_error,
                });
            }
        }
    }
    Err(LocalArchiveError::LimitExceeded(
        "could not reserve a private staging directory".to_owned(),
    ))
}

fn extract_archive(
    source_path: &Path,
    root: &Path,
    limits: LocalArchiveLimits,
    rar_program: &Path,
) -> Result<(), LocalArchiveError> {
    if source_path
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        extract_zip_archive(source_path, root, limits)
    } else {
        extract_rar_archive(source_path, root, limits, rar_program)
    }
}

fn extract_zip_archive(
    source_path: &Path,
    root: &Path,
    limits: LocalArchiveLimits,
) -> Result<(), LocalArchiveError> {
    let source = File::open(source_path).map_err(|source_error| LocalArchiveError::Io {
        path: source_path.to_owned(),
        source: source_error,
    })?;
    let mut archive = zip::ZipArchive::new(source)
        .map_err(|error| LocalArchiveError::Archive(error.to_string()))?;
    if archive.len() > limits.max_entries {
        return Err(LocalArchiveError::LimitExceeded(format!(
            "more than {} entries",
            limits.max_entries
        )));
    }

    let mut total_declared = 0_u64;
    let mut total_written = 0_u64;
    for index in 0..archive.len() {
        let mut member = archive
            .by_index(index)
            .map_err(|error| LocalArchiveError::Archive(error.to_string()))?;
        let name = member.name().to_owned();
        let relative = normalize_member_path(&name, limits)?;
        let target = root.join(relative);
        let file_type = member.unix_mode().map(|mode| mode & POSIX_FILE_TYPE_MASK);
        if member.is_symlink()
            || file_type
                .is_some_and(|kind| kind != 0 && kind != POSIX_REGULAR_FILE && !member.is_dir())
        {
            return Err(LocalArchiveError::UnsupportedMemberType(name));
        }
        if member.is_dir() {
            create_archive_directory(&target)?;
            continue;
        }
        if !member.is_file() {
            return Err(LocalArchiveError::UnsupportedMemberType(name));
        }
        check_declared_member(&name, member.size(), &mut total_declared, limits)?;
        let mut writer = open_bounded_member(&target, &mut total_written, limits)?;
        if let Err(source_error) = io::copy(&mut member, &mut writer) {
            return Err(writer_error(&target, source_error));
        }
        writer.finish(member.size())?;
    }
    Ok(())
}

fn extract_rar_archive(
    source_path: &Path,
    root: &Path,
    limits: LocalArchiveLimits,
    rar_program: &Path,
) -> Result<(), LocalArchiveError> {
    let members = list_rar_members(source_path, limits, rar_program)?;
    for member in &members {
        if member.kind == RarMemberKind::Directory {
            create_archive_directory(&root.join(&member.relative_path))?;
        }
    }
    if members
        .iter()
        .all(|member| member.kind == RarMemberKind::Directory)
    {
        return Ok(());
    }

    let selector_path = root.parent().unwrap_or(root).join(".youta-rar-members");
    let mut selector_options = OpenOptions::new();
    selector_options.write(true).create_new(true);
    let mut selector = crate::private_files::open_privately(&mut selector_options)
        .open(&selector_path)
        .map_err(|source_error| LocalArchiveError::Io {
            path: selector_path.clone(),
            source: source_error,
        })?;
    for member in &members {
        if member.kind == RarMemberKind::File {
            writeln!(selector, "{}", member.raw_name).map_err(|source_error| {
                LocalArchiveError::Io {
                    path: selector_path.clone(),
                    source: source_error,
                }
            })?;
        }
    }
    selector
        .flush()
        .map_err(|source_error| LocalArchiveError::Io {
            path: selector_path.clone(),
            source: source_error,
        })?;
    drop(selector);

    let result = stream_rar_members(
        source_path,
        root,
        &members,
        limits,
        rar_program,
        &selector_path,
    );
    let cleanup = fs::remove_file(&selector_path).map_err(|source_error| LocalArchiveError::Io {
        path: selector_path,
        source: source_error,
    });
    result?;
    cleanup
}

fn list_rar_members(
    source_path: &Path,
    limits: LocalArchiveLimits,
    rar_program: &Path,
) -> Result<Vec<RarMember>, LocalArchiveError> {
    let mut child = Command::new(rar_program)
        .args(["lt", "-idp", "-c-", "-p-", "-cfg-"])
        .arg(source_path)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| LocalArchiveError::Archive(format!("could not start `unrar`: {error}")))?;
    let stdout = child.stdout.take().ok_or_else(|| {
        LocalArchiveError::Archive("could not read `unrar` member listing".to_owned())
    })?;
    let mut reader = BufReader::new(stdout.take(DEFAULT_MAX_RAR_LISTING_BYTES.saturating_add(1)));
    let mut line = Vec::new();
    let mut pending = PendingRarMember::default();
    let mut members = Vec::new();
    let mut seen_paths = HashSet::new();
    let mut total_declared = 0_u64;
    let mut listing_bytes = 0_u64;
    let parse_result = loop {
        match read_rar_listing_line(&mut reader, &mut line, &mut listing_bytes, limits) {
            Ok(false) => {
                break finish_rar_record(
                    &mut pending,
                    &mut members,
                    &mut seen_paths,
                    &mut total_declared,
                    limits,
                );
            }
            Ok(true) => {}
            Err(error) => break Err(error),
        }
        let text = match std::str::from_utf8(&line) {
            Ok(text) => text,
            Err(error) => {
                break Err(LocalArchiveError::Archive(format!(
                    "`unrar` returned a non-UTF-8 member listing: {error}"
                )));
            }
        };
        let field = text.trim_start();
        if let Some(name) = field.strip_prefix("Name: ") {
            if pending.name.is_some()
                && let Err(error) = finish_rar_record(
                    &mut pending,
                    &mut members,
                    &mut seen_paths,
                    &mut total_declared,
                    limits,
                )
            {
                break Err(error);
            }
            pending.name = Some(name.to_owned());
        } else if let Some(kind) = field.strip_prefix("Type: ") {
            pending.kind = match kind {
                "File" => Some(RarMemberKind::File),
                "Directory" => Some(RarMemberKind::Directory),
                _ if pending.name.is_some() => {
                    break Err(LocalArchiveError::UnsupportedMemberType(
                        pending.name.clone().unwrap_or_else(|| kind.to_owned()),
                    ));
                }
                _ => None,
            };
        } else if let Some(size) = field.strip_prefix("Size: ")
            && pending.name.is_some()
        {
            pending.declared_bytes = match size.parse() {
                Ok(size) => Some(size),
                Err(error) => {
                    break Err(LocalArchiveError::Archive(format!(
                        "invalid RAR member size `{size}`: {error}"
                    )));
                }
            };
        }
    };
    if parse_result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|error| {
        LocalArchiveError::Archive(format!("could not wait for `unrar`: {error}"))
    })?;
    parse_result?;
    if !status.success() {
        return Err(LocalArchiveError::Archive(format!(
            "`unrar` could not list `{}` (status {status})",
            source_path.display()
        )));
    }
    Ok(members)
}

fn read_rar_listing_line<R: io::BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
    listing_bytes: &mut u64,
    limits: LocalArchiveLimits,
) -> Result<bool, LocalArchiveError> {
    line.clear();
    let read_bytes = reader.read_until(b'\n', line).map_err(|error| {
        LocalArchiveError::Archive(format!("could not read `unrar` member listing: {error}"))
    })?;
    if read_bytes == 0 {
        return Ok(false);
    }
    *listing_bytes = listing_bytes.saturating_add(u64::try_from(read_bytes).unwrap_or(u64::MAX));
    if *listing_bytes > DEFAULT_MAX_RAR_LISTING_BYTES {
        return Err(LocalArchiveError::LimitExceeded(format!(
            "RAR listing exceeds {DEFAULT_MAX_RAR_LISTING_BYTES} bytes"
        )));
    }
    if line.len() > limits.max_path_bytes.saturating_add(256) {
        return Err(LocalArchiveError::LimitExceeded(
            "RAR listing contains an overlong line".to_owned(),
        ));
    }
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    Ok(true)
}

fn finish_rar_record(
    pending: &mut PendingRarMember,
    members: &mut Vec<RarMember>,
    seen_paths: &mut HashSet<PathBuf>,
    total_declared: &mut u64,
    limits: LocalArchiveLimits,
) -> Result<(), LocalArchiveError> {
    let Some(name) = pending.name.take() else {
        return Ok(());
    };
    let kind = pending.kind.take().ok_or_else(|| {
        LocalArchiveError::Archive(format!("RAR member `{name}` has no supported type"))
    })?;
    let declared_bytes = match kind {
        RarMemberKind::Directory => pending.declared_bytes.take().unwrap_or_default(),
        RarMemberKind::File => pending.declared_bytes.take().ok_or_else(|| {
            LocalArchiveError::Archive(format!("RAR member `{name}` has no size"))
        })?,
    };
    if name.contains(['\n', '\r']) || name.contains(['*', '?']) {
        return Err(LocalArchiveError::UnsafeMemberPath(name));
    }
    let relative_path = normalize_member_path(&name, limits)?;
    if !seen_paths.insert(relative_path.clone()) {
        return Err(LocalArchiveError::DuplicateMember(relative_path));
    }
    if kind == RarMemberKind::File {
        check_declared_member(&name, declared_bytes, total_declared, limits)?;
    }
    if members.len() >= limits.max_entries {
        return Err(LocalArchiveError::LimitExceeded(format!(
            "more than {} entries",
            limits.max_entries
        )));
    }
    members.push(RarMember {
        raw_name: name,
        relative_path,
        kind,
        declared_bytes,
    });
    Ok(())
}

fn stream_rar_members(
    source_path: &Path,
    root: &Path,
    members: &[RarMember],
    limits: LocalArchiveLimits,
    rar_program: &Path,
    selector_path: &Path,
) -> Result<(), LocalArchiveError> {
    let mut selector_argument = OsString::from("-n@");
    selector_argument.push(selector_path);
    let mut child = Command::new(rar_program)
        .args(["p", "-inul", "-c-", "-p-", "-cfg-"])
        .arg(selector_argument)
        .arg(source_path)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| LocalArchiveError::Archive(format!("could not start `unrar`: {error}")))?;
    let Some(mut stdout) = child.stdout.take() else {
        terminate_child(&mut child);
        return Err(LocalArchiveError::Archive(
            "could not read `unrar` member output".to_owned(),
        ));
    };
    let mut total_written = 0_u64;
    for member in members {
        if member.kind == RarMemberKind::Directory {
            continue;
        }
        let target = root.join(&member.relative_path);
        let mut writer = match open_bounded_member(&target, &mut total_written, limits) {
            Ok(writer) => writer,
            Err(error) => {
                terminate_child(&mut child);
                return Err(error);
            }
        };
        let copied = match io::copy(&mut (&mut stdout).take(member.declared_bytes), &mut writer) {
            Ok(copied) => copied,
            Err(source_error) => {
                terminate_child(&mut child);
                return Err(writer_error(&target, source_error));
            }
        };
        if copied != member.declared_bytes {
            terminate_child(&mut child);
            return Err(LocalArchiveError::Archive(format!(
                "RAR member `{}` declared {} bytes but produced {copied}",
                member.raw_name, member.declared_bytes
            )));
        }
        if let Err(error) = writer.finish(member.declared_bytes) {
            terminate_child(&mut child);
            return Err(error);
        }
    }
    let mut trailing = [0_u8; 1];
    match stdout.read(&mut trailing) {
        Ok(0) => {}
        Ok(_) => {
            terminate_child(&mut child);
            return Err(LocalArchiveError::Archive(
                "`unrar` produced data beyond the declared member sizes".to_owned(),
            ));
        }
        Err(error) => {
            terminate_child(&mut child);
            return Err(LocalArchiveError::Archive(format!(
                "could not finish reading `unrar` member output: {error}"
            )));
        }
    }
    let status = child.wait().map_err(|error| {
        LocalArchiveError::Archive(format!("could not wait for `unrar`: {error}"))
    })?;
    if !status.success() {
        return Err(LocalArchiveError::Archive(format!(
            "`unrar` could not extract selected members (status {status})"
        )));
    }
    Ok(())
}

fn terminate_child(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn check_declared_member(
    name: &str,
    declared_bytes: u64,
    total_declared: &mut u64,
    limits: LocalArchiveLimits,
) -> Result<(), LocalArchiveError> {
    if declared_bytes > limits.max_member_bytes {
        return Err(LocalArchiveError::LimitExceeded(format!(
            "member `{name}` declares {declared_bytes} bytes"
        )));
    }
    *total_declared = total_declared.checked_add(declared_bytes).ok_or_else(|| {
        LocalArchiveError::LimitExceeded("declared member sizes overflowed".to_owned())
    })?;
    if *total_declared > limits.max_total_bytes {
        return Err(LocalArchiveError::LimitExceeded(format!(
            "members declare more than {} total bytes",
            limits.max_total_bytes
        )));
    }
    Ok(())
}

fn open_bounded_member<'a>(
    target: &Path,
    total_written: &'a mut u64,
    limits: LocalArchiveLimits,
) -> Result<BoundedMemberWriter<'a>, LocalArchiveError> {
    if let Some(parent) = target.parent() {
        create_archive_directory(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let file = crate::private_files::open_privately(&mut options)
        .open(target)
        .map_err(|source_error| {
            if source_error.kind() == io::ErrorKind::AlreadyExists {
                LocalArchiveError::DuplicateMember(target.to_owned())
            } else {
                LocalArchiveError::Io {
                    path: target.to_owned(),
                    source: source_error,
                }
            }
        })?;
    Ok(BoundedMemberWriter {
        path: target.to_owned(),
        file,
        written_bytes: 0,
        total_written,
        limits,
    })
}

impl io::Write for BoundedMemberWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let requested = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if self.written_bytes.saturating_add(requested) > self.limits.max_member_bytes
            || self.total_written.saturating_add(requested) > self.limits.max_total_bytes
        {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "archive extraction byte limit exceeded",
            ));
        }
        let written = self.file.write(bytes)?;
        let written = u64::try_from(written).unwrap_or(u64::MAX);
        self.written_bytes = self.written_bytes.saturating_add(written);
        *self.total_written = self.total_written.saturating_add(written);
        usize::try_from(written).map_err(io::Error::other)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl BoundedMemberWriter<'_> {
    fn finish(mut self, declared_bytes: u64) -> Result<(), LocalArchiveError> {
        if self.written_bytes != declared_bytes {
            return Err(LocalArchiveError::Archive(format!(
                "member `{}` declared {declared_bytes} bytes but produced {}",
                self.path.display(),
                self.written_bytes
            )));
        }
        self.file
            .flush()
            .map_err(|source_error| LocalArchiveError::Io {
                path: self.path,
                source: source_error,
            })
    }
}

fn writer_error(path: &Path, source: io::Error) -> LocalArchiveError {
    if source.kind() == io::ErrorKind::FileTooLarge {
        LocalArchiveError::LimitExceeded(format!(
            "member `{}` or the archive total exceeded its byte limit",
            path.display()
        ))
    } else {
        LocalArchiveError::Io {
            path: path.to_owned(),
            source,
        }
    }
}

fn normalize_member_path(
    name: &str,
    limits: LocalArchiveLimits,
) -> Result<PathBuf, LocalArchiveError> {
    if name.is_empty() || name.len() > limits.max_path_bytes || name.as_bytes().contains(&0) {
        return Err(LocalArchiveError::UnsafeMemberPath(name.to_owned()));
    }
    let mut normalized = PathBuf::new();
    let mut depth = 0_usize;
    let portable_name = name.replace('\\', "/");
    let bytes = portable_name.as_bytes();
    if portable_name.starts_with('/')
        || bytes.get(1) == Some(&b':') && bytes[0].is_ascii_alphabetic()
    {
        return Err(LocalArchiveError::UnsafeMemberPath(name.to_owned()));
    }
    for component in Path::new(&portable_name).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => {
                depth = depth.saturating_add(1);
                if depth > limits.max_depth {
                    return Err(LocalArchiveError::LimitExceeded(format!(
                        "member `{name}` is deeper than {} directories",
                        limits.max_depth
                    )));
                }
                normalized.push(value);
            }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(LocalArchiveError::UnsafeMemberPath(name.to_owned()));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(LocalArchiveError::UnsafeMemberPath(name.to_owned()));
    }
    Ok(normalized)
}

fn create_archive_directory(path: &Path) -> Result<(), LocalArchiveError> {
    if path.exists() && !path.is_dir() {
        return Err(LocalArchiveError::DuplicateMember(path.to_owned()));
    }
    crate::private_files::create_private_directory(path).map_err(|source_error| {
        LocalArchiveError::Io {
            path: path.to_owned(),
            source: source_error,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn zip_fixture(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut archive = zip::ZipWriter::new(cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in entries {
            archive
                .start_file(*name, options)
                .expect("start ZIP member");
            archive.write_all(bytes).expect("write ZIP member");
        }
        archive.finish().expect("finish ZIP fixture").into_inner()
    }

    #[test]
    fn materializes_nested_zip_media_and_reuses_the_complete_cache() {
        let fixture = crate::test_support::canonical_tempdir("local ZIP materialization");
        let source = fixture.path().join("album.zip");
        fs::write(
            &source,
            zip_fixture(&[("disc/track.opus", b"mock opus"), ("cover.jpg", b"image")]),
        )
        .expect("write ZIP fixture");
        let cache = fixture.path().join("cache");

        let first = materialize_local_archive(&source, &cache, LocalArchiveLimits::default())
            .expect("materialize ZIP");
        assert!(!first.reused_cache);
        assert_eq!(
            fs::read(first.root_path.join("disc/track.opus")).expect("read extracted track"),
            b"mock opus"
        );
        assert_eq!(
            first.display_path(&first.root_path),
            format!("{}!/", source.display())
        );

        let second = materialize_local_archive(&source, &cache, LocalArchiveLimits::default())
            .expect("reuse ZIP cache");
        assert!(second.reused_cache);
        assert_eq!(second.root_path, first.root_path);

        fs::write(
            &source,
            zip_fixture(&[("disc/track.opus", b"replacement mock opus")]),
        )
        .expect("replace ZIP fixture");
        let replaced = materialize_local_archive(&source, &cache, LocalArchiveLimits::default())
            .expect("replace stale ZIP cache");
        assert!(!replaced.reused_cache);
        assert_eq!(replaced.root_path, first.root_path);
        assert_eq!(
            fs::read(replaced.root_path.join("disc/track.opus"))
                .expect("read replacement extracted track"),
            b"replacement mock opus"
        );
    }

    #[cfg(unix)]
    #[test]
    fn materializes_rar_media_through_the_bounded_helper_protocol() {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = crate::test_support::canonical_tempdir("local RAR materialization");
        let source = fixture.path().join("album.rar");
        fs::write(&source, b"mock RAR fixture").expect("write RAR fixture");
        let helper = fixture.path().join("mock-unrar");
        fs::write(
            &helper,
            r#"#!/bin/sh
case "$1" in
	lt)
		printf '%s\n' \
			'        Name: tree/branch1/leaf' \
			'        Type: File' \
			'        Size: 12' \
			'' \
			'        Name: tree/branch2/leaf' \
			'        Type: File' \
			'        Size: 14' \
			'' \
			'        Name: tree/branch2' \
			'        Type: Directory' \
			''
		;;
	p)
		printf 'p\n' >> "${0}.calls"
		for argument do
			case "$argument" in
				-n@*) members=${argument#-n@} ;;
			esac
		done
		[ -n "$members" ] || exit 3
		while IFS= read -r member; do
			case "$member" in
				tree/branch1/leaf) printf 'Hello World\n' ;;
				tree/branch2/leaf) printf 'Goodbye World\n' ;;
				*) exit 4 ;;
			esac
		done < "$members"
		;;
	*) exit 2 ;;
esac
"#,
        )
        .expect("write mock unrar helper");
        let mut permissions = fs::metadata(&helper)
            .expect("mock unrar metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&helper, permissions).expect("make mock unrar executable");

        let extracted = materialize_local_archive_with_rar_program(
            &source,
            &fixture.path().join("cache"),
            LocalArchiveLimits::default(),
            &helper,
        )
        .expect("materialize RAR");
        assert_eq!(
            fs::read(extracted.root_path.join("tree/branch1/leaf"))
                .expect("read first extracted RAR member"),
            b"Hello World\n"
        );
        assert_eq!(
            fs::read(extracted.root_path.join("tree/branch2/leaf"))
                .expect("read extracted RAR member"),
            b"Goodbye World\n"
        );
        assert_eq!(
            fs::read_to_string(format!("{}.calls", helper.display()))
                .expect("read mock unrar call count"),
            "p\n",
            "all selected RAR members must stream through one helper process"
        );
    }

    #[test]
    fn rejects_parent_traversal_without_writing_outside_the_cache() {
        let fixture = crate::test_support::canonical_tempdir("unsafe local ZIP");
        let source = fixture.path().join("unsafe.zip");
        fs::write(&source, zip_fixture(&[("../escape.opus", b"escape")]))
            .expect("write unsafe ZIP fixture");

        let error = materialize_local_archive(
            &source,
            &fixture.path().join("cache"),
            LocalArchiveLimits::default(),
        )
        .expect_err("parent traversal must fail");
        assert!(matches!(error, LocalArchiveError::UnsafeMemberPath(_)));
        assert!(!fixture.path().join("escape.opus").exists());
    }

    #[test]
    fn rejects_portable_absolute_and_backslash_traversal_paths() {
        let limits = LocalArchiveLimits::default();
        for unsafe_name in ["..\\escape.opus", "/absolute.opus", "C:\\absolute.opus"] {
            assert!(matches!(
                normalize_member_path(unsafe_name, limits),
                Err(LocalArchiveError::UnsafeMemberPath(_))
            ));
        }
    }

    #[test]
    fn enforces_actual_uncompressed_byte_limits_while_streaming() {
        let fixture = crate::test_support::canonical_tempdir("bounded local ZIP");
        let source = fixture.path().join("large.zip");
        fs::write(&source, zip_fixture(&[("large.opus", b"123456")]))
            .expect("write bounded ZIP fixture");
        let limits = LocalArchiveLimits {
            max_member_bytes: 5,
            ..LocalArchiveLimits::default()
        };

        let error = materialize_local_archive(&source, &fixture.path().join("cache"), limits)
            .expect_err("member limit must fail");
        assert!(matches!(error, LocalArchiveError::LimitExceeded(_)));
    }
}
