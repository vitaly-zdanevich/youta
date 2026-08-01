//! Offline build history and runtime installation provenance.
//!
//! Recent commits are generated at compile time from Git when available and
//! from deterministic archive metadata otherwise. Runtime provenance uses
//! bounded Portage ownership checks before falling back to the embedded build
//! origin, so copying an installed executable does not mislabel that copy.

use std::fs::{self, File};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

const DEFAULT_PORTAGE_DATABASE: &str = "/var/db/pkg";
const GITHUB_RELEASE_ORIGIN: &str = "github-release";
const MAX_CONTENTS_BYTES: u64 = 4 * 1024 * 1024;
const MAX_PORTAGE_CATEGORY_ENTRIES: usize = 8 * 1024;
const MAX_PORTAGE_PACKAGE_CANDIDATES: usize = 16;

/// One commit embedded in the executable for offline release-history display.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuildCommit {
    /// Full hexadecimal Git object name.
    pub hash: &'static str,
    /// Commit timestamp in ISO 8601 format.
    pub committed_at: &'static str,
    /// Full commit message, including its body and trailers.
    pub message: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/build_info_generated.rs"));

/// How the running executable appears to have been installed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallationSource {
    /// Built and installed by the Gentoo `media-sound/youta` source package.
    PortageSourcePackage,
    /// Installed by the Gentoo `media-sound/youta-bin` binary package.
    PortageBinaryPackage,
    /// Downloaded from Youta's official GitHub release artifacts.
    OfficialGithubRelease,
    /// Compiled from a local source checkout or unpacked source tree.
    LocalCompilation,
}

impl InstallationSource {
    /// Returns the stable user-facing description used by build details.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::PortageSourcePackage => "Portage source package (media-sound/youta)",
            Self::PortageBinaryPackage => "Portage binary package (media-sound/youta-bin)",
            Self::OfficialGithubRelease => "Downloaded GitHub release binary",
            Self::LocalCompilation => "Locally compiled binary",
        }
    }
}

/// Runtime paths and the detected installation source shown in build details.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeProvenance {
    /// Detected package or build source.
    pub installation_source: InstallationSource,
    /// Canonical executable path when the operating system can resolve it.
    pub executable_path: PathBuf,
    /// Working directory from which Youta was launched.
    pub launch_directory: PathBuf,
    /// Compile-time source directory, exposed only for local compilations.
    pub build_source_directory: Option<PathBuf>,
}

/// Returns up to ten recent commits embedded at build time.
#[must_use]
pub fn embedded_commits() -> &'static [BuildCommit] {
    EMBEDDED_BUILD_COMMITS
}

/// Returns the exact Git SHA used for this build when build metadata supplied it.
#[must_use]
pub const fn current_build_sha() -> &'static str {
    EMBEDDED_CURRENT_BUILD_SHA
}

/// Detects how the running executable was installed and records its runtime paths.
///
/// Portage detection is deliberately ownership-based: a package directory alone
/// is insufficient because the user may be running a copied release binary.
///
/// # Errors
///
/// Returns an operating-system error when the current executable path or launch
/// directory cannot be resolved.
pub fn detect_runtime_provenance() -> io::Result<RuntimeProvenance> {
    let executable_path = canonicalize_or_original(std::env::current_exe()?);
    let launch_directory = canonicalize_or_original(std::env::current_dir()?);
    Ok(detect_runtime_provenance_from(
        executable_path,
        launch_directory,
        Path::new(DEFAULT_PORTAGE_DATABASE),
        EMBEDDED_BUILD_ORIGIN,
        Path::new(EMBEDDED_BUILD_SOURCE_DIRECTORY),
    ))
}

/// Classifies injected runtime facts for deterministic tests and UI adapters.
#[must_use]
pub(crate) fn detect_runtime_provenance_from(
    executable_path: PathBuf,
    launch_directory: PathBuf,
    portage_database: &Path,
    build_origin: &str,
    build_source_directory: &Path,
) -> RuntimeProvenance {
    let installation_source =
        portage_owner(portage_database, &executable_path).unwrap_or_else(|| {
            if build_origin == GITHUB_RELEASE_ORIGIN {
                InstallationSource::OfficialGithubRelease
            } else {
                InstallationSource::LocalCompilation
            }
        });
    let build_source_directory = (installation_source == InstallationSource::LocalCompilation)
        .then(|| build_source_directory.to_path_buf());

    RuntimeProvenance {
        installation_source,
        executable_path,
        launch_directory,
        build_source_directory,
    }
}

fn portage_owner(portage_database: &Path, executable_path: &Path) -> Option<InstallationSource> {
    let category = portage_database.join("media-sound");
    let mut candidates = fs::read_dir(category)
        .ok()?
        .take(MAX_PORTAGE_CATEGORY_ENTRIES)
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let source = if name.starts_with("youta-bin-") {
                InstallationSource::PortageBinaryPackage
            } else if name.starts_with("youta-") {
                InstallationSource::PortageSourcePackage
            } else {
                return None;
            };
            Some((name, entry.path().join("CONTENTS"), source))
        })
        .take(MAX_PORTAGE_PACKAGE_CANDIDATES)
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.0.cmp(&right.0));

    candidates.into_iter().find_map(|(_, contents, source)| {
        contents_owns_executable(&contents, executable_path).then_some(source)
    })
}

fn contents_owns_executable(contents_path: &Path, executable_path: &Path) -> bool {
    let Ok(file) = File::open(contents_path) else {
        return false;
    };
    let mut bytes = Vec::new();
    if file
        .take(MAX_CONTENTS_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .is_err()
        || u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_CONTENTS_BYTES
    {
        return false;
    }

    let expected = canonicalize_or_original(executable_path.to_path_buf());
    String::from_utf8_lossy(&bytes).lines().any(|line| {
        contents_object_path(line)
            .is_some_and(|candidate| canonicalize_or_original(PathBuf::from(candidate)) == expected)
    })
}

fn contents_object_path(line: &str) -> Option<&str> {
    let payload = line.strip_prefix("obj ")?;
    let mut fields = payload.rsplitn(3, ' ');
    let modified_at = fields.next()?;
    let digest = fields.next()?;
    let path = fields.next()?;
    (!path.is_empty() && !digest.is_empty() && !modified_at.is_empty()).then_some(path)
}

fn canonicalize_or_original(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_portage_contents(
        temporary_directory: &tempfile::TempDir,
        package: &str,
        executable: &Path,
    ) -> PathBuf {
        let database = temporary_directory.path().join("var/db/pkg");
        let package_directory = database.join("media-sound").join(package);
        fs::create_dir_all(&package_directory).expect("create fake Portage package");
        fs::write(
            package_directory.join("CONTENTS"),
            format!("obj {} digest 1785528000\n", executable.display()),
        )
        .expect("write fake Portage contents");
        database
    }

    #[test]
    fn embedded_history_is_bounded_and_marks_the_current_build() {
        let commits = embedded_commits();

        assert!(!commits.is_empty());
        assert!(commits.len() <= 10);
        assert!(commits.iter().any(|commit| {
            commit.message.contains("\n\n") && commit.message.lines().count() > 1
        }));
        assert!(commits.iter().any(|commit| {
            commit.hash == current_build_sha()
                && !commit.committed_at.is_empty()
                && !commit.message.is_empty()
        }));
    }

    #[test]
    fn portage_source_package_ownership_takes_priority_over_build_origin() {
        let temporary_directory = tempfile::tempdir().expect("create temp directory");
        let executable = temporary_directory.path().join("usr/bin/youta");
        fs::create_dir_all(executable.parent().expect("executable parent"))
            .expect("create executable parent");
        fs::write(&executable, b"binary").expect("create executable");
        let database = write_portage_contents(&temporary_directory, "youta-0.24.1-r1", &executable);

        let provenance = detect_runtime_provenance_from(
            executable.clone(),
            temporary_directory.path().to_path_buf(),
            &database,
            GITHUB_RELEASE_ORIGIN,
            Path::new("/build/source"),
        );

        assert_eq!(
            provenance.installation_source,
            InstallationSource::PortageSourcePackage
        );
        assert_eq!(provenance.executable_path, executable);
        assert_eq!(provenance.build_source_directory, None);
    }

    #[test]
    fn portage_binary_package_is_distinguished_from_source_package() {
        let temporary_directory = tempfile::tempdir().expect("create temp directory");
        let executable = temporary_directory.path().join("usr/bin/youta");
        fs::create_dir_all(executable.parent().expect("executable parent"))
            .expect("create executable parent");
        fs::write(&executable, b"binary").expect("create executable");
        let database =
            write_portage_contents(&temporary_directory, "youta-bin-0.24.1", &executable);

        let provenance = detect_runtime_provenance_from(
            executable,
            temporary_directory.path().to_path_buf(),
            &database,
            "local",
            Path::new("/build/source"),
        );

        assert_eq!(
            provenance.installation_source,
            InstallationSource::PortageBinaryPackage
        );
        assert_eq!(provenance.build_source_directory, None);
    }

    #[test]
    fn stale_portage_entry_does_not_claim_a_different_executable() {
        let temporary_directory = tempfile::tempdir().expect("create temp directory");
        let installed = temporary_directory.path().join("usr/bin/youta");
        let copied = temporary_directory.path().join("opt/youta");
        for executable in [&installed, &copied] {
            fs::create_dir_all(executable.parent().expect("executable parent"))
                .expect("create executable parent");
            fs::write(executable, b"binary").expect("create executable");
        }
        let database = write_portage_contents(&temporary_directory, "youta-bin-0.24.1", &installed);

        let provenance = detect_runtime_provenance_from(
            copied,
            temporary_directory.path().to_path_buf(),
            &database,
            GITHUB_RELEASE_ORIGIN,
            Path::new("/build/source"),
        );

        assert_eq!(
            provenance.installation_source,
            InstallationSource::OfficialGithubRelease
        );
    }

    #[test]
    fn local_compile_preserves_build_and_launch_locations() {
        let temporary_directory = tempfile::tempdir().expect("create temp directory");
        let executable = temporary_directory.path().join("target/release/youta");
        let launch_directory = temporary_directory.path().join("music");
        let build_directory = temporary_directory.path().join("source");

        let provenance = detect_runtime_provenance_from(
            executable.clone(),
            launch_directory.clone(),
            &temporary_directory.path().join("missing-portage-db"),
            "local",
            &build_directory,
        );

        assert_eq!(
            provenance,
            RuntimeProvenance {
                installation_source: InstallationSource::LocalCompilation,
                executable_path: executable,
                launch_directory,
                build_source_directory: Some(build_directory),
            }
        );
    }

    #[test]
    fn oversized_portage_contents_is_ignored() {
        let temporary_directory = tempfile::tempdir().expect("create temp directory");
        let executable = temporary_directory.path().join("usr/bin/youta");
        fs::create_dir_all(executable.parent().expect("executable parent"))
            .expect("create executable parent");
        fs::write(&executable, b"binary").expect("create executable");
        let database = write_portage_contents(&temporary_directory, "youta-0.24.1", &executable);
        let contents = database.join("media-sound/youta-0.24.1/CONTENTS");
        fs::OpenOptions::new()
            .write(true)
            .open(contents)
            .expect("open fake Portage contents")
            .set_len(MAX_CONTENTS_BYTES + 1)
            .expect("enlarge fake Portage contents");

        let provenance = detect_runtime_provenance_from(
            executable,
            temporary_directory.path().to_path_buf(),
            &database,
            "local",
            Path::new("/build/source"),
        );

        assert_eq!(
            provenance.installation_source,
            InstallationSource::LocalCompilation
        );
    }
}
