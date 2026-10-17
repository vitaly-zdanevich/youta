//! Test-only fixtures shared by the module test suites.
//!
//! Declared `#[cfg(test)]` in the crate root, so nothing here reaches a
//! distribution build.

#[cfg(feature = "tui")]
pub(crate) mod performance;

use std::path::PathBuf;

use tempfile::TempDir;

/// Restores blocking HTTP fixture I/O with finite read and write budgets.
///
/// Windows inherits a listener's nonblocking mode on accepted connections;
/// request parsers need blocking reads to wait for bytes within their timeout.
#[cfg(feature = "funkwhale")]
pub(crate) fn configure_http_fixture_stream(
    stream: &std::net::TcpStream,
    timeout: std::time::Duration,
) {
    stream
        .set_nonblocking(false)
        .expect("blocking fixture stream");
    stream
        .set_read_timeout(Some(timeout))
        .expect("fixture read timeout");
    stream
        .set_write_timeout(Some(timeout))
        .expect("fixture write timeout");
}

/// Creates a temporary directory whose own path is already canonical.
///
/// Local listings, moves, and media identifiers report paths that came back
/// from [`std::fs::canonicalize`], so a fixture path has to be canonical too or
/// it never compares equal to them. A plain [`TempDir`] is not: macOS resolves
/// `/var` to `/private/var`, and Windows resolves 8.3 components (`RUNNER~1`
/// into `runneradmin`) and reports the result behind a `\\?\` verbatim prefix.
/// Canonicalizing the *parent* first makes every path derived from the fixture
/// canonical, without each test having to convert its own expectations.
///
/// `context` names the fixture in the panic message.
pub(crate) fn canonical_tempdir(context: &str) -> TempDir {
    let root = canonical_temporary_root();
    TempDir::new_in(&root).unwrap_or_else(|error| {
        panic!("{context} under {}: {error}", root.display());
    })
}

/// Returns `path` in the one shape a fixture path compares equal to.
///
/// Windows has two spellings for the same file: [`std::fs::canonicalize`]
/// answers with a `\\?\` verbatim prefix, and a `file://` URL cannot carry that
/// prefix, so a path that has made the round trip out to a URL and back comes
/// home in the other spelling and stops comparing equal to the fixture it was
/// built from. Canonicalising it again settles it back on the one shape.
///
/// This does not weaken the assertion it is used in. A URL naming the wrong
/// file still canonicalises to a different path, and a path that names nothing
/// is returned unchanged so the comparison still fails and still prints what it
/// actually saw. Unix has one spelling and passes straight through.
///
/// Published artwork is the only thing that leaves as a URL and comes back as a
/// path, so the feature that produces it is what decides whether this exists.
#[cfg(feature = "local-artwork")]
pub(crate) fn one_path_shape(path: &std::path::Path) -> PathBuf {
    crate::fs_path::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

/// Canonical form of the platform temporary directory.
fn canonical_temporary_root() -> PathBuf {
    let root = std::env::temp_dir();
    crate::fs_path::canonicalize(&root).unwrap_or_else(|error| {
        panic!("canonical temporary root {}: {error}", root.display());
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simulates Windows' inherited socket mode without depending on packet timing.
    #[cfg(all(unix, feature = "funkwhale", feature = "acoustid"))]
    #[test]
    fn http_fixture_stream_clears_inherited_nonblocking_mode() {
        use rustix::fs::{OFlags, fcntl_getfl};
        use std::net::{TcpListener, TcpStream};
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
        let _client =
            TcpStream::connect_timeout(&listener.local_addr().unwrap(), Duration::from_secs(2))
                .expect("fixture client");
        let (stream, _) = listener.accept().expect("fixture connection");
        stream.set_nonblocking(true).expect("inherited socket mode");
        assert!(fcntl_getfl(&stream).unwrap().contains(OFlags::NONBLOCK));

        configure_http_fixture_stream(&stream, Duration::from_secs(2));

        assert!(
            !fcntl_getfl(&stream).unwrap().contains(OFlags::NONBLOCK),
            "fixture parsing must wait for request bytes on every platform"
        );
        assert!(stream.read_timeout().unwrap().is_some());
        assert!(stream.write_timeout().unwrap().is_some());
    }

    #[test]
    fn fixture_paths_survive_canonicalization_unchanged() {
        let fixture = canonical_tempdir("canonicalization fixture");
        let nested = fixture.path().join("album");
        std::fs::create_dir(&nested).expect("nested fixture directory");

        assert_eq!(
            crate::fs_path::canonicalize(fixture.path()).expect("canonical fixture root"),
            fixture.path()
        );
        assert_eq!(
            crate::fs_path::canonicalize(&nested).expect("canonical nested directory"),
            nested
        );
    }
}
