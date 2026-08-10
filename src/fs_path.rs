//! Canonical filesystem paths in the crate's one spelling.
//!
//! Windows spells one file two ways. [`std::fs::canonicalize`] answers with a
//! `\\?\` verbatim prefix, while every locator that has made the round trip
//! through a `file://` URL — durable state, media identities, session
//! snapshots — comes back without it, because a URL cannot carry the prefix.
//! Letting both spellings circulate turns every comparison between a stored
//! path and a canonicalized one into a coin toss, and those comparisons are
//! what playback matching, playlist validation, and move remapping are made
//! of. The crate therefore has one rule: canonicalization happens here, and
//! the verbatim prefix does not leave this module. Nothing is given up by
//! dropping it — Rust's own filesystem calls re-apply the prefix internally
//! whenever a path outgrows the legacy length limit.
//!
//! On Unix there is one spelling and this module is [`std::fs::canonicalize`]
//! with a longer name.

use std::io;
use std::path::{Path, PathBuf};

/// Canonicalizes `path` into the crate's one spelling.
///
/// # Errors
///
/// Exactly the errors of [`std::fs::canonicalize`]: the path has to name
/// something real to be canonicalized at all.
pub(crate) fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    std::fs::canonicalize(path).map(flattened)
}

/// Returns `path` without its verbatim prefix, when that loses nothing.
///
/// Only the two prefixes [`std::fs::canonicalize`] actually produces are
/// rewritten — a verbatim disk and a verbatim UNC share — and only while the
/// remainder is ordinary named components, which canonical output always is.
/// Anything else is returned untouched rather than reinterpreted.
#[cfg(windows)]
fn flattened(path: PathBuf) -> PathBuf {
    use std::ffi::OsString;
    use std::path::{Component, Prefix};

    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return path;
    };
    let mut flat = match prefix.kind() {
        Prefix::VerbatimDisk(disk) => PathBuf::from(format!(r"{}:\", disk as char)),
        Prefix::VerbatimUNC(server, share) => {
            let mut authority = OsString::from(r"\\");
            authority.push(server);
            authority.push(r"\");
            authority.push(share);
            PathBuf::from(authority)
        }
        _ => return path,
    };
    let mut literal = true;
    for component in components {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => flat.push(name),
            // Canonical output never contains these; a path that does is not
            // ours to respell.
            Component::Prefix(_) | Component::CurDir | Component::ParentDir => {
                literal = false;
                break;
            }
        }
    }
    if literal { flat } else { path }
}

/// Returns `path` unchanged: this platform spells a file one way already.
#[cfg(not(windows))]
fn flattened(path: PathBuf) -> PathBuf {
    path
}

#[cfg(all(test, windows))]
mod tests {
    use std::path::PathBuf;

    use super::flattened;

    #[test]
    fn a_verbatim_disk_prefix_is_dropped() {
        assert_eq!(
            flattened(PathBuf::from(r"\\?\C:\Users\kt\track.flac")),
            PathBuf::from(r"C:\Users\kt\track.flac")
        );
    }

    #[test]
    fn a_verbatim_share_keeps_its_authority() {
        assert_eq!(
            flattened(PathBuf::from(r"\\?\UNC\stereo\music\track.flac")),
            PathBuf::from(r"\\stereo\music\track.flac")
        );
    }

    #[test]
    fn an_already_flat_path_is_returned_as_it_came() {
        assert_eq!(
            flattened(PathBuf::from(r"C:\Users\kt\track.flac")),
            PathBuf::from(r"C:\Users\kt\track.flac")
        );
    }
}
