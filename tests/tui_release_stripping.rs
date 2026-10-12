//! Exercises TUI release packaging with mock compilers and a mutating strip tool.

#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const ORIGINAL_EXECUTABLE: &str = "executable\nfunction-symbol\nline-tables\n";
const STRIPPED_EXECUTABLE: &str = "executable\n";

/// An isolated repository records commands without invoking a real Rust build.
struct ReleaseFixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl ReleaseFixture {
    /// Installs the actual packager beside fake Cargo, rustc, and strip commands.
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repository with spaces");
        fs::create_dir_all(root.join("scripts")).unwrap();
        fs::create_dir_all(root.join("mock tools")).unwrap();
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/package-release.sh"),
            root.join("scripts/package-release.sh"),
        )
        .unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nversion = \"1.2.3\"\n").unwrap();
        let fixture = Self {
            _directory: directory,
            root,
        };
        fixture.write_tool(
            "rustc",
            "#!/usr/bin/env bash\nprintf 'host: %s\\n' \"${MOCK_HOST_TARGET}\"\n",
        );
        fixture.write_tool(
            "cargo",
            r#"#!/usr/bin/env bash
set -Eeuo pipefail
printf '%s\n' "$*" >> cargo.log
printf 'strip=%s\ndebug=%s\nsplit=%s\norigin=%s\n' \
	"${CARGO_PROFILE_RELEASE_STRIP-unset}" \
	"${CARGO_PROFILE_RELEASE_DEBUG-unset}" \
	"${CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO-unset}" \
	"${YOUTA_BUILD_ORIGIN-unset}" > profile.log
while [[ $# -gt 0 ]]; do
	if [[ $1 == --target ]]; then
		target=$2
		break
	fi
	shift
done
mkdir -p "target/${target}/release"
printf 'executable\nfunction-symbol\nline-tables\n' > "target/${target}/release/youta"
"#,
        );
        fixture.write_tool(
            "strip",
            r#"#!/usr/bin/env bash
set -Eeuo pipefail
printf '%s\n' "$@" >> strip.log
if [[ ${MOCK_STRIP_FAIL:-0} == 1 ]]; then
	echo 'mock strip failed' >&2
	exit 71
fi
for executable in "$@"; do :; done
printf 'executable\n' > "${executable}"
"#,
        );
        fixture.write_tool(
            "dsymutil",
            r#"#!/usr/bin/env bash
set -Eeuo pipefail
printf '%s\n' "$@" >> dsymutil.log
if [[ ${MOCK_DSYM_FAIL:-0} == 1 ]]; then
	echo 'mock dsymutil failed' >&2
	exit 72
fi
[[ $2 == -o ]]
mkdir -p "$3/Contents/Resources/DWARF"
printf 'collected line tables\n' > "$3/Contents/Resources/DWARF/$(basename -- "$1")"
"#,
        );
        fixture
    }

    /// Writes an executable mock, including paths containing spaces.
    fn write_tool(&self, name: &str, source: &str) {
        let path = self.root.join("mock tools").join(name);
        fs::write(&path, source).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Seeds conflicting inherited settings to check packaging's explicit profile.
    fn command(&self, host: &str, target: &str, variant: &str) -> Command {
        let mut command = Command::new("bash");
        let path = std::env::join_paths(std::iter::once(self.root.join("mock tools")).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();
        command
            .current_dir(&self.root)
            .arg("scripts/package-release.sh")
            .args([target, "release files", variant])
            .env("PATH", path)
            .env("MOCK_HOST_TARGET", host)
            .env("CARGO_PROFILE_RELEASE_STRIP", "symbols")
            .env("CARGO_PROFILE_RELEASE_DEBUG", "0")
            .env("CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO", "packed")
            .env_remove("YOUTA_RELEASE_STRIP")
            .env_remove("MOCK_STRIP_FAIL")
            .env_remove("MOCK_DSYM_FAIL");
        command
    }

    /// Reads an artifact or command log relative to this temporary repository.
    fn read(&self, relative: &str) -> String {
        fs::read_to_string(self.root.join(relative))
            .unwrap_or_else(|error| panic!("reading {relative}: {error}"))
    }

    /// Checks a checksum using the same standard tools available on release hosts.
    fn verify_checksum(&self, name: &str) {
        let output = Command::new("bash")
			.current_dir(self.root.join("release files"))
			.args([
				"-c",
				"if command -v sha256sum >/dev/null 2>&1; then sha256sum -c \"$1\"; else shasum -a 256 -c \"$1\"; fi",
				"checksum",
				&format!("{name}.sha256"),
			])
			.output()
			.unwrap();
        assert_success(&output);
    }

    /// Lists only the final files to catch unwanted debug copies of small variants.
    fn artifact_names(&self) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(self.root.join("release files"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
}

/// Reports the mock command's diagnostics when packaging unexpectedly fails.
fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// One optimized full-featured build produces independent normal/debug downloads.
#[test]
fn full_featured_releases_strip_only_the_normal_copy_on_all_supported_targets() {
    for (host, target, platform, strip_flags) in [
        (
            "x86_64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
            "linux-amd64",
            "--strip-all\n",
        ),
        (
            "x86_64-unknown-linux-gnu",
            "i686-unknown-linux-gnu",
            "linux-i686",
            "--strip-all\n",
        ),
        (
            "aarch64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "linux-arm64",
            "--strip-all\n",
        ),
        (
            "x86_64-apple-darwin",
            "x86_64-apple-darwin",
            "macos-amd64",
            "-S\n-x\n",
        ),
        (
            "aarch64-apple-darwin",
            "aarch64-apple-darwin",
            "macos-arm64",
            "-S\n-x\n",
        ),
    ] {
        let fixture = ReleaseFixture::new();
        assert_success(&fixture.command(host, target, "images").output().unwrap());
        let name = format!("youta-1.2.3-{platform}");
        assert_eq!(
            fixture.read(&format!("release files/{name}")),
            STRIPPED_EXECUTABLE
        );
        assert_eq!(
            fixture.read(&format!("release files/{name}-debug")),
            ORIGINAL_EXECUTABLE
        );
        assert_eq!(
            fixture.read(&format!("target/{target}/release/youta")),
            ORIGINAL_EXECUTABLE
        );
        assert_eq!(fixture.read("cargo.log").lines().count(), 1, "{target}");
        let cargo = fixture.read("cargo.log");
        assert!(cargo.starts_with("build --locked --release --target "));
        assert!(cargo.contains("--no-default-features --features app,archive-org,"));
        assert!(cargo.contains(",gpm,images,lan-sharing,"));
        let profile = fixture.read("profile.log");
        assert!(profile.contains("strip=none\ndebug=line-tables-only\n"));
        assert!(profile.contains("origin=github-release\n"));
        if platform.starts_with("linux-") {
            assert!(profile.contains("split=off\n"));
            assert_eq!(fixture.artifact_names().len(), 4);
            assert!(!fixture.root.join("dsymutil.log").exists());
        } else {
            assert!(profile.contains("split=unpacked\n"));
            let symbols = format!("{name}-debug.dSYM.tar.xz");
            fixture.verify_checksum(&symbols);
            let archive = Command::new("tar")
                .current_dir(fixture.root.join("release files"))
                .args(["-tJf", &symbols])
                .output()
                .unwrap();
            assert_success(&archive);
            assert!(String::from_utf8_lossy(&archive.stdout).contains(&format!(
                "{name}-debug.dSYM/Contents/Resources/DWARF/{name}-debug"
            )));
            let dsymutil = fixture.read("dsymutil.log");
            let debug_executable = fixture
                .root
                .join("release files")
                .join(format!("{name}-debug"));
            assert!(dsymutil.starts_with(&format!("{}\n-o\n", debug_executable.display())));
            assert_eq!(dsymutil.lines().count(), 3);
            assert_eq!(
                fixture.artifact_names().len(),
                6,
                "temporary dSYM directories must be cleaned up"
            );
        }
        let executable = fixture.root.join("release files").join(&name);
        assert_eq!(
            fixture.read("strip.log"),
            format!("{strip_flags}{}\n", executable.display())
        );
        assert_ne!(
            fs::metadata(&executable).unwrap().permissions().mode() & 0o111,
            0
        );
        assert_ne!(
            fs::metadata(executable.with_file_name(format!("{name}-debug")))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
        fixture.verify_checksum(&name);
        fixture.verify_checksum(&format!("{name}-debug"));
        assert_ne!(
            fixture
                .read(&format!("release files/{name}.sha256"))
                .split_whitespace()
                .next(),
            fixture
                .read(&format!("release files/{name}-debug.sha256"))
                .split_whitespace()
                .next(),
        );
    }
}

/// Optional feature combinations keep their existing download names and count.
#[test]
fn smaller_variants_emit_one_stripped_executable_and_checksum() {
    for (variant, suffix) in [
        ("text", "-text"),
        ("images-no-qr", "-no-qr"),
        ("text-no-qr", "-text-no-qr"),
        ("images-no-gpm", "-no-gpm"),
        ("text-no-gpm", "-text-no-gpm"),
        ("images-no-qr-no-gpm", "-no-qr-no-gpm"),
        ("text-no-qr-no-gpm", "-text-no-qr-no-gpm"),
    ] {
        let fixture = ReleaseFixture::new();
        let target = "x86_64-unknown-linux-gnu";
        assert_success(&fixture.command(target, target, variant).output().unwrap());
        let name = format!("youta-1.2.3-linux-amd64{suffix}");
        assert_eq!(
            fixture.artifact_names(),
            [name.clone(), format!("{name}.sha256")]
        );
        assert_eq!(
            fixture.read(&format!("release files/{name}")),
            STRIPPED_EXECUTABLE
        );
        assert_eq!(
            fixture.read(&format!("target/{target}/release/youta")),
            ORIGINAL_EXECUTABLE
        );
        assert_eq!(fixture.read("cargo.log").lines().count(), 1);
        fixture.verify_checksum(&name);
    }
}

/// Release variants keep every default capability except their documented omissions.
#[test]
fn release_variants_keep_default_features_including_global_commands() {
    let manifest: toml::Table =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .unwrap()
            .parse()
            .unwrap();
    let defaults = manifest["features"]["default"]
        .as_array()
        .unwrap()
        .iter()
        .map(|feature| feature.as_str().unwrap().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    for variant in [
        "images",
        "text",
        "images-no-qr",
        "text-no-qr",
        "images-no-gpm",
        "text-no-gpm",
        "images-no-qr-no-gpm",
        "text-no-qr-no-gpm",
    ] {
        let fixture = ReleaseFixture::new();
        let target = "x86_64-unknown-linux-gnu";
        assert_success(&fixture.command(target, target, variant).output().unwrap());
        let invocation = fixture.read("cargo.log");
        let (_, features) = invocation.split_once("--features ").unwrap();
        let actual = features
            .trim()
            .split(',')
            .map(str::to_owned)
            .collect::<std::collections::BTreeSet<_>>();
        let mut expected = defaults.clone();
        if variant.starts_with("text") {
            expected.remove("images");
        }
        if variant.contains("no-qr") {
            expected.remove("qr");
            expected.remove("lan-sharing");
        }
        if variant.contains("no-gpm") {
            expected.remove("gpm");
        }
        assert_eq!(
            actual, expected,
            "{variant} must retain default-on commands and other unrelated capabilities"
        );
    }
}

/// A strip error cannot be reported as successful packaging or receive checksums.
#[test]
fn strip_failure_stops_packaging_without_touching_the_cargo_executable() {
    let fixture = ReleaseFixture::new();
    let target = "x86_64-unknown-linux-gnu";
    let output = fixture
        .command(target, target, "images")
        .env("MOCK_STRIP_FAIL", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("mock strip failed"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Created "));
    assert_eq!(
        fixture.read(&format!("target/{target}/release/youta")),
        ORIGINAL_EXECUTABLE
    );
    assert!(
        fixture
            .artifact_names()
            .iter()
            .all(|name| !name.ends_with(".sha256"))
    );
}

/// Unsupported cross-host stripping requires an explicit compatible tool.
#[test]
fn cross_compiling_requires_a_compatible_strip_override() {
    let fixture = ReleaseFixture::new();
    let host = "x86_64-unknown-linux-gnu";
    let target = "aarch64-unknown-linux-gnu";
    let output = fixture.command(host, target, "text").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("YOUTA_RELEASE_STRIP"));
    assert!(!fixture.root.join("cargo.log").exists());
    assert_success(
        &fixture
            .command(host, target, "text")
            .env("YOUTA_RELEASE_STRIP", fixture.root.join("mock tools/strip"))
            .output()
            .unwrap(),
    );
    assert_eq!(
        fixture.read("release files/youta-1.2.3-linux-arm64-text"),
        STRIPPED_EXECUTABLE
    );
}

/// A typo in a configured strip path fails before any expensive build.
#[test]
fn missing_strip_tool_fails_before_building() {
    let fixture = ReleaseFixture::new();
    let target = "x86_64-unknown-linux-gnu";
    let output = fixture
        .command(target, target, "images")
        .env("YOUTA_RELEASE_STRIP", fixture.root.join("missing strip"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("strip"));
    assert!(!fixture.root.join("cargo.log").exists());
}

/// A missing dSYM must not silently produce a debug release without line tables.
#[test]
fn dsymutil_failure_stops_packaging_and_cleans_temporary_symbols() {
    let fixture = ReleaseFixture::new();
    let target = "aarch64-apple-darwin";
    let output = fixture
        .command(target, target, "images")
        .env("MOCK_DSYM_FAIL", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("mock dsymutil failed"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Created "));
    assert_eq!(
        fixture.read(&format!("target/{target}/release/youta")),
        ORIGINAL_EXECUTABLE
    );
    assert!(
        fixture
            .artifact_names()
            .iter()
            .all(|name| !name.ends_with(".sha256") && !name.starts_with(".youta-dsym."))
    );
}

/// macOS text packages neither invoke dsymutil nor create a debug sidecar.
#[test]
fn macos_text_release_omits_debug_symbols() {
    let fixture = ReleaseFixture::new();
    let target = "x86_64-apple-darwin";
    assert_success(&fixture.command(target, target, "text").output().unwrap());
    assert_eq!(
        fixture.artifact_names(),
        [
            "youta-1.2.3-macos-amd64-text",
            "youta-1.2.3-macos-amd64-text.sha256"
        ]
    );
    assert!(!fixture.root.join("dsymutil.log").exists());
}
