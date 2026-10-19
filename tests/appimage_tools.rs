//! Isolated regression coverage for Tauri's cached AppRun permissions.

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        path::{Path, PathBuf},
        process::{Command, Output},
    };

    /// Supplies mock release assets while executing the real preparation function.
    struct Fixture {
        root: tempfile::TempDir,
    }

    impl Fixture {
        /// Build tools emulate downloads and recognize only the known mock payload.
        fn new() -> Self {
            let root = tempfile::Builder::new()
                .prefix("youta AppRun ")
                .tempdir()
                .expect("isolated AppRun fixture");
            let tools = root.path().join("tools");
            fs::create_dir(&tools).unwrap();
            write_tool(
                &tools,
                "curl",
                r#"#!/usr/bin/env bash
set -Eeuo pipefail
printf 'download\n' >> "${FIXTURE_ROOT}/downloads"
output=
while [[ $# -gt 0 ]]; do
	case "$1" in
		--output) output=$2; shift 2 ;;
		--connect-timeout|--max-time|--retry|--retry-delay|--retry-max-time) shift 2 ;;
		--fail|--location|--silent|--show-error) shift ;;
		https://*)
			[[ $1 == "https://github.com/tauri-apps/binary-releases/releases/download/apprun-old/AppRun-${FIXTURE_ARCH}" ]]
			shift
			;;
		*) exit 99 ;;
	esac
done
[[ -n ${output} ]]
case "${FIXTURE_DOWNLOAD_MODE}" in
	good) printf 'verified %s AppRun\n' "${FIXTURE_ARCH}" > "${output}" ;;
	corrupt) printf 'corrupted payload\n' > "${output}" ;;
	partial) printf 'incomplete download\n' > "${output}"; exit 22 ;;
	*) exit 99 ;;
esac
"#,
            );
            write_tool(
                &tools,
                "sha256sum",
                r#"#!/usr/bin/env bash
set -Eeuo pipefail
payload=$(cat)
if [[ ${payload} == 'verified x86_64 AppRun' ]]; then
	printf '%s  -\n' f30140a43a0a59e46db21bdefdf749b9e9f2c6946e92afabbacf98b8ae73fb4f
elif [[ ${payload} == 'verified aarch64 AppRun' ]]; then
	printf '%s  -\n' 072f17c0895a85c490282fe5395c5007e5fc75da727e553b3b8fb680feb11578
else
	printf '%s\n' "${payload}" | "${REAL_SHA256SUM}"
fi
"#,
            );
            Self { root }
        }

        /// Resolve the cache path Tauri copies into its AppDir.
        fn cache(&self, architecture: &str) -> PathBuf {
            self.root
                .path()
                .join("target/.tauri")
                .join(format!("AppRun-{architecture}"))
        }

        /// Execute inside a condition too, so shell errexit cannot hide missing checks.
        fn run(&self, architecture: &str, download_mode: &str) -> Output {
            let hash_tool = Command::new("bash")
                .args(["-c", "command -v sha256sum"])
                .output()
                .expect("find real checksum tool");
            assert!(hash_tool.status.success());
            let mut paths = vec![self.root.path().join("tools")];
            paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
            Command::new("bash")
				.args([
					"-c",
					"set -Eeuo pipefail; source \"$1\"; if prepare_appimage_tools \"${FIXTURE_ROOT}/target\" \"${FIXTURE_ARCH}\"; then exit 0; else exit 1; fi",
					"appimage-tools-test",
				])
				.arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/prepare-appimage-tools.sh"))
				.env("PATH", std::env::join_paths(paths).unwrap())
				.env("FIXTURE_ROOT", self.root.path())
				.env("FIXTURE_ARCH", architecture)
				.env("FIXTURE_DOWNLOAD_MODE", download_mode)
				.env("REAL_SHA256SUM", String::from_utf8(hash_tool.stdout).unwrap().trim())
				.output()
				.expect("execute actual AppRun preparation helper")
        }

        /// Confirm failed downloads leave neither trusted cache files nor partial files.
        fn assert_empty_cache(&self) {
            assert_eq!(
                fs::read_dir(self.root.path().join("target/.tauri"))
                    .unwrap()
                    .count(),
                0
            );
        }
    }

    /// Make one test tool executable without depending on a network or Tauri.
    fn write_tool(directory: &Path, name: &str, source: &str) {
        let path = directory.join(name);
        fs::write(&path, source).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Both native Linux architectures install a verified launcher readable by all users.
    #[test]
    fn downloaded_launchers_are_installed_with_0755_permissions() {
        for architecture in ["x86_64", "aarch64"] {
            let fixture = Fixture::new();
            let output = fixture.run(architecture, "good");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let cache = fixture.cache(architecture);
            assert_eq!(
                fs::read_to_string(&cache).unwrap(),
                format!("verified {architecture} AppRun\n")
            );
            assert_eq!(
                fs::metadata(&cache).unwrap().permissions().mode() & 0o7777,
                0o755
            );
            assert_eq!(fs::read_dir(cache.parent().unwrap()).unwrap().count(), 1);
        }
    }

    /// Reused caches reproduce the original 0770 bug and must be fixed without downloading.
    #[test]
    fn verified_cached_launcher_permissions_are_normalized() {
        let fixture = Fixture::new();
        let cache = fixture.cache("x86_64");
        fs::create_dir_all(cache.parent().unwrap()).unwrap();
        fs::write(&cache, "verified x86_64 AppRun\n").unwrap();
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o770)).unwrap();
        for _ in 0..2 {
            let output = fixture.run("x86_64", "partial");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                fs::metadata(&cache).unwrap().permissions().mode() & 0o7777,
                0o755
            );
        }
        assert!(!fixture.root.path().join("downloads").exists());
    }

    /// Unknown cache contents retain their bytes and permissions when verification fails.
    #[test]
    fn corrupt_cached_launcher_is_rejected_without_replacement() {
        let fixture = Fixture::new();
        let cache = fixture.cache("x86_64");
        fs::create_dir_all(cache.parent().unwrap()).unwrap();
        fs::write(&cache, "unknown cached launcher\n").unwrap();
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o770)).unwrap();
        let output = fixture.run("x86_64", "good");
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("SHA-256 mismatch"));
        assert_eq!(
            fs::read_to_string(&cache).unwrap(),
            "unknown cached launcher\n"
        );
        assert_eq!(
            fs::metadata(&cache).unwrap().permissions().mode() & 0o7777,
            0o770
        );
        assert!(!fixture.root.path().join("downloads").exists());
    }

    /// Neither a truncated transfer nor a completed response with wrong bytes reaches Tauri.
    #[test]
    fn failed_downloads_are_removed_before_the_helper_returns() {
        for download_mode in ["partial", "corrupt"] {
            let fixture = Fixture::new();
            let output = fixture.run("aarch64", download_mode);
            assert!(!output.status.success());
            fixture.assert_empty_cache();
        }
    }

    /// Cache preparation never changes an arbitrary file reached through a symlink.
    #[test]
    fn symlinked_cache_entries_are_rejected() {
        let fixture = Fixture::new();
        let cache = fixture.cache("x86_64");
        let outside = fixture.root.path().join("untouched-launcher");
        fs::write(&outside, "verified x86_64 AppRun\n").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o770)).unwrap();
        fs::create_dir_all(cache.parent().unwrap()).unwrap();
        symlink(&outside, &cache).unwrap();
        let output = fixture.run("x86_64", "good");
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("non-regular"));
        assert_eq!(
            fs::metadata(&outside).unwrap().permissions().mode() & 0o7777,
            0o770
        );
        assert!(!fixture.root.path().join("downloads").exists());
    }
}
