//! Making a rename durable, on the platforms that can and the one that cannot.
//!
//! Every state file Youta writes is published the same way: write a temporary
//! file, flush it, rename it over the real name. The flush makes the *contents*
//! durable, but on Unix the rename itself lives in the parent directory and
//! survives a host crash only once that directory is synchronized too.
//!
//! Doing that means opening the directory as a file, which is a POSIX
//! affordance and not a portable one. On Windows `File::open` on a directory
//! fails outright with `ERROR_ACCESS_DENIED`, so the same line that hardens the
//! write on Linux breaks every write on Windows — and it broke them silently,
//! because the failure looks like an ordinary I/O error from the save path.
//!
//! There is no user-mode Windows equivalent to reach for instead. Flushing NTFS
//! metadata means `FlushFileBuffers` on a *volume* handle, which needs
//! administrator rights and would flush every process's writes, not Youta's.
//! So on Windows this is a documented no-op: contents are still flushed before
//! the rename, and the ordering guarantee that the rename is either fully
//! visible or not visible at all is provided by the filesystem itself. What is
//! given up is only the promise that a rename already observed by this process
//! survives a power cut — the same promise most Windows applications never
//! made.
//!
//! Every directory synchronization in Youta goes through this one function, so
//! the platform question is asked once instead of at each save site.

use std::io;
use std::path::Path;

/// Flushes the directory entry created or replaced inside `path`.
///
/// Call this after renaming a temporary file into its final name, with the
/// directory that holds the final name.
///
/// # Errors
///
/// Returns the underlying error when the directory cannot be opened or
/// synchronized. On Windows this never fails, because it does nothing.
pub fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Flushes the directory that holds `path`, when `path` has one.
///
/// A path with no parent — a bare file name — is synchronized against the
/// current directory, which is where such a name resolves.
///
/// # Errors
///
/// Returns the underlying error when the directory cannot be opened or
/// synchronized.
pub fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    sync_directory(parent)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn a_published_name_is_synchronized_through_its_own_directory() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let published = directory.path().join("state.toml");
        fs::write(&published, b"volume = 40\n").expect("write");

        sync_parent_directory(&published).expect("the parent directory is synchronized");
        sync_directory(directory.path()).expect("the directory is synchronized");
    }

    #[test]
    fn a_bare_name_is_synchronized_against_the_current_directory() {
        // A relative name with no parent resolves in the working directory, so
        // that is the directory whose entry has to be made durable. The empty
        // path is what `Path::parent` hands back here, and opening it fails.
        sync_parent_directory(Path::new("state.toml"))
            .expect("a bare name falls back to the current directory");
    }

    #[test]
    fn a_directory_that_is_not_there_is_reported_rather_than_ignored() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let missing = directory.path().join("absent");

        let result = sync_directory(&missing);
        #[cfg(unix)]
        assert_eq!(
            result
                .expect_err("a missing directory cannot be synchronized")
                .kind(),
            io::ErrorKind::NotFound
        );
        #[cfg(not(unix))]
        result.expect("platforms without directory synchronization report success");
    }
}
