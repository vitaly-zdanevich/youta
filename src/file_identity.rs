//! The part of a file's identity the filesystem assigns, not the user.
//!
//! Youta caches things against files — waveform envelopes, thumbnails, probed
//! metadata, state documents — and every one of those caches has to notice when
//! the file behind it was *replaced* rather than edited. Length and modification
//! time are not enough: a replacement can land with the same length, and a
//! modification time can be set to anything.
//!
//! What settles it is the number the filesystem itself hands out. On Unix that
//! is `(device, inode)`, straight out of metadata Youta has already read.
//! Windows has the same notion under different names — the volume serial number
//! and the file index — but `std` keeps them behind the unstable
//! `windows_by_handle` feature, so on stable Rust they are unreachable from the
//! standard library. Before this module the Windows build simply dropped the
//! fields, which left every one of those caches unable to tell a replacement
//! from an edit.
//!
//! The gap is closed with `file-id`, a small crate that asks
//! `GetFileInformationByHandle` for exactly those two numbers. It is declared
//! for `cfg(windows)` only, so the platforms that already had an answer link
//! nothing new.
//!
//! Unix carries one thing Windows does not: the inode change time, which moves
//! whenever the file's metadata changes and, unlike the modification time,
//! cannot be set by a program. It stays [`Option`]al rather than being faked, so
//! a comparison that has it is strictly stronger and one that does not is still
//! correct.
//!
//! Identifiers are reused after a file is deleted, which is why every caller
//! keeps length and timestamps beside this rather than instead of it.

use std::fs::Metadata;
use std::path::Path;

/// A file's filesystem-assigned identity.
///
/// Two files are the same file when both numbers match. Equality also covers
/// [`Self::changed`], so an in-place metadata change is a difference on the
/// platforms that record one.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FilesystemIdentity {
    /// Unix device number, or the Windows volume serial number.
    pub volume: u64,
    /// Unix inode number, or the Windows file index.
    pub file: u128,
    /// Whole and fractional seconds of the last inode change, where the
    /// platform records one separately from the modification time.
    pub changed: Option<(i64, i64)>,
}

/// Reads the filesystem-assigned identity of one path.
///
/// `metadata` is the caller's already-read metadata for the same path, used on
/// the platforms that carry the identity inside it. Returns `None` when the
/// platform cannot supply one, or when the file went away between the two
/// calls: every caller keeps length and timestamps alongside this, so `None`
/// weakens the comparison rather than breaking it.
#[must_use]
pub fn filesystem_identity(path: &Path, metadata: &Metadata) -> Option<FilesystemIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let _ = path;
        Some(FilesystemIdentity {
            volume: metadata.dev(),
            file: u128::from(metadata.ino()),
            changed: Some((metadata.ctime(), metadata.ctime_nsec())),
        })
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        // NTFS records no inode-change time; the modification time the caller
        // already keeps is the whole of what Windows offers.
        let (volume, file) = match file_id::get_file_id(path).ok()? {
            file_id::FileId::Inode {
                device_id,
                inode_number,
            } => (device_id, u128::from(inode_number)),
            file_id::FileId::LowRes {
                volume_serial_number,
                file_index,
            } => (u64::from(volume_serial_number), u128::from(file_index)),
            file_id::FileId::HighRes {
                volume_serial_number,
                file_id,
            } => (volume_serial_number, file_id),
        };
        Some(FilesystemIdentity {
            volume,
            file,
            changed: None,
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, metadata);
        None
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn identity_of(path: &Path) -> FilesystemIdentity {
        let metadata = fs::metadata(path).expect("metadata");
        filesystem_identity(path, &metadata).expect("this platform assigns file identities")
    }

    #[test]
    fn a_file_is_the_same_file_as_itself_and_not_as_its_neighbour() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        fs::write(&first, b"same length").expect("first");
        fs::write(&second, b"same length").expect("second");

        assert_eq!(identity_of(&first), identity_of(&first));
        // Equal length and near-equal timestamps; only the assigned number
        // separates them, which is the whole reason this exists.
        assert_ne!(identity_of(&first).file, identity_of(&second).file);
    }

    #[test]
    fn a_replacement_is_not_the_file_it_replaced() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("state.toml");
        fs::write(&path, b"volume = 40").expect("original");
        let before = identity_of(&path);

        let replacement = directory.path().join("state.toml.tmp");
        fs::write(&replacement, b"volume = 41").expect("replacement");
        fs::rename(&replacement, &path).expect("publish");

        assert_ne!(before, identity_of(&path));
    }

    #[test]
    fn a_directory_has_an_identity_too() {
        // Local folder-size measurement depends on this: on Windows a
        // directory can only be opened for identity with backup semantics, and
        // a helper that forgot them would return `None` here and nowhere else.
        let directory = tempfile::tempdir().expect("temporary directory");
        let nested = directory.path().join("nested");
        fs::create_dir(&nested).expect("nested directory");

        assert_ne!(identity_of(directory.path()), identity_of(&nested));
    }

    #[test]
    fn a_file_that_went_away_yields_no_identity_rather_than_a_wrong_one() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("transient");
        fs::write(&path, b"here").expect("fixture");
        let metadata = fs::metadata(&path).expect("metadata");
        fs::remove_file(&path).expect("remove");

        // Unix answers from the metadata already in hand, so it still has an
        // identity; Windows has to reopen the path and correctly has none.
        let identity = filesystem_identity(&path, &metadata);
        assert_eq!(identity.is_some(), cfg!(unix));
    }
}
