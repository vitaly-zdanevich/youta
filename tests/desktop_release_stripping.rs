//! Regression coverage for stripping desktop releases before bundling and signing.

/// Every host keeps the release-only overrides before the Tauri build boundary.
#[test]
fn desktop_packaging_overrides_strip_and_debug_before_tauri() {
    for relative in [
        "scripts/package-desktop.sh",
        "scripts/package-desktop-executable.sh",
    ] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
        let script = std::fs::read_to_string(path).expect("desktop packaging script");
        let build = script.find("npx --yes").expect("Tauri build invocation");
        for setting in [
            "export CARGO_PROFILE_RELEASE_STRIP=symbols",
            "export CARGO_PROFILE_RELEASE_DEBUG=0",
        ] {
            let setting_position = script
                .find(setting)
                .unwrap_or_else(|| panic!("{relative} must declare {setting}"));
            assert!(
                setting_position < build,
                "{relative}: {setting} is too late"
            );
        }
    }
}

#[cfg(unix)]
mod unix {
    use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

    /// Adds one executable tool mock without depending on Cargo, Node, or a network.
    fn mock_tool(directory: &Path, name: &str, source: &str) {
        let path = directory.join(name);
        fs::write(&path, source).expect("write tool fixture");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .expect("make tool fixture executable");
    }

    /// Runs the real packaging boundary against a controlled, signed build output.
    fn package_fixture(target: &str, platform: &str, cross: bool, signed: bool) {
        let temporary = tempfile::tempdir().expect("isolated packaging fixture");
        let root = temporary.path();
        for directory in ["scripts", "gui", "tools"] {
            fs::create_dir(root.join(directory)).expect("fixture directory");
        }
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
        for script in [
            "package-desktop.sh",
            "package-desktop-executable.sh",
            "desktop-signing-mode.sh",
        ] {
            fs::copy(
                repository.join("scripts").join(script),
                root.join("scripts").join(script),
            )
            .expect("copy actual packaging script");
        }
        fs::write(
            root.join("gui/Cargo.toml"),
            "[package]\nversion = \"9.8.7\"\n",
        )
        .expect("fixture version");
        let tools = root.join("tools");
        mock_tool(
            &root.join("scripts"),
            "prepare-appimage-tools.sh",
            r#"#!/usr/bin/env bash
prepare_appimage_tools() {
	[[ $1 == "${FIXTURE_ROOT}/target" ]]
	[[ $2 == "${FIXTURE_TARGET%%-*}" ]]
	mkdir -p "$1/.tauri"
	printf 'launcher fixture\n' > "$1/.tauri/AppRun-$2"
	chmod 755 "$1/.tauri/AppRun-$2"
	printf 'prepared\n' > "${FIXTURE_ROOT}/appimage-prepared"
}
"#,
        );
        mock_tool(
            &root.join("scripts"),
            "validate-appimage.sh",
            r#"#!/usr/bin/env bash
set -Eeuo pipefail
[[ $1 == "${FIXTURE_ROOT}/dist/"*.AppImage && -x $1 ]]
printf 'validated\n' > "${FIXTURE_ROOT}/appimage-validated"
"#,
        );
        mock_tool(
            &tools,
            "rustc",
            "#!/usr/bin/env bash\nset -Eeuo pipefail\n[[ $* == -vV ]]\nprintf 'host: %s\\n' \"${FIXTURE_TARGET}\"\n",
        );
        mock_tool(
            &tools,
            "npm",
            "#!/usr/bin/env bash\nset -Eeuo pipefail\n[[ $* == '--prefix gui/ui ci' || $* == '--prefix gui/ui run build' ]]\n",
        );
        mock_tool(
            &tools,
            "npx",
            r#"#!/usr/bin/env bash
set -Eeuo pipefail
[[ ${CARGO_PROFILE_RELEASE_STRIP:-} == symbols ]] || {
	echo 'Tauri must receive strip=symbols before building or signing.' >&2
	exit 1
}
[[ ${CARGO_PROFILE_RELEASE_DEBUG:-} == 0 ]] || {
	echo 'Tauri must receive debug=0 before building or signing.' >&2
	exit 1
}
[[ ${YOUTA_BUILD_ORIGIN:-} == github-release ]]
[[ $PWD == "${FIXTURE_ROOT}/gui" ]]
[[ $1 == --yes && $2 == @tauri-apps/cli@2.11.4 && $3 == build ]]
shift 3
if [[ ${FIXTURE_CROSS} == 1 ]]; then
	[[ $* == "--target ${FIXTURE_TARGET} --no-bundle" ]]
	build_root="${FIXTURE_ROOT}/target/${FIXTURE_TARGET}/release"
else
	if [[ ${FIXTURE_PLATFORM} == macos-* && -z ${APPLE_CERTIFICATE:-} ]]; then
		[[ $* == --no-sign ]]
	elif [[ ${FIXTURE_PLATFORM} == linux-* ]]; then
		[[ $# == 2 && $1 == --config && $2 == '{"bundle":{"useLocalToolsDir":true}}' ]]
		[[ -x ${FIXTURE_ROOT}/target/.tauri/AppRun-${FIXTURE_TARGET%%-*} ]]
		[[ -f ${FIXTURE_ROOT}/appimage-prepared ]]
	else
		[[ $# == 0 ]]
	fi
	build_root="${FIXTURE_ROOT}/target/release"
fi
mkdir -p "${build_root}"
extension=
[[ ${FIXTURE_PLATFORM} != windows-* ]] || extension=.exe
printf 'signed executable fixture\n' > "${build_root}/youta-gui${extension}"
if [[ ${FIXTURE_CROSS} == 0 ]]; then
	case "${FIXTURE_PLATFORM}" in
		linux-*) formats=(deb rpm appimage); names=(test.deb test.rpm test.AppImage) ;;
		macos-*) formats=(dmg); names=(test.dmg) ;;
		windows-*) formats=(nsis); names=(test-setup.exe) ;;
		*) exit 1 ;;
	esac
	for index in "${!formats[@]}"; do
		mkdir -p "${build_root}/bundle/${formats[index]}"
		printf 'signed installer fixture\n' > "${build_root}/bundle/${formats[index]}/${names[index]}"
	done
fi
printf 'tauri build\n' >> "${FIXTURE_ROOT}/invocations"
"#,
        );
        let mut paths = vec![tools];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").expect("host PATH"),
        ));
        let mut command = Command::new("bash");
        command
            .current_dir(root)
            .env("PATH", std::env::join_paths(paths).expect("fixture PATH"))
            .env("FIXTURE_ROOT", root)
            .env("FIXTURE_TARGET", target)
            .env("FIXTURE_PLATFORM", platform)
            .env("FIXTURE_CROSS", if cross { "1" } else { "0" })
            .env(
                "APPLE_CERTIFICATE",
                if signed { "test-certificate" } else { "" },
            )
            // Explicit local preferences must not leak into published GUI assets.
            .env("CARGO_PROFILE_RELEASE_STRIP", "none")
            .env("CARGO_PROFILE_RELEASE_DEBUG", "2");
        if cross {
            command
                .arg("scripts/package-desktop-executable.sh")
                .arg(target);
        } else {
            command.arg("scripts/package-desktop.sh");
        }
        let output = command
            .arg("dist")
            .output()
            .expect("execute packaging fixture");
        assert!(
            output.status.success(),
            "{target}:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(root.join("invocations")).unwrap(),
            "tauri build\n"
        );
        let executable_extension = if platform.starts_with("windows-") {
            ".exe"
        } else {
            ""
        };
        let executable = format!("youta-gui-9.8.7-{platform}{executable_extension}");
        let mut expected = vec![(executable, "signed executable fixture\n")];
        if !cross {
            let extensions: &[&str] = if platform.starts_with("linux-") {
                &["deb", "rpm", "AppImage"]
            } else if platform.starts_with("macos-") {
                &["dmg"]
            } else {
                &["exe"]
            };
            for extension in extensions {
                let bundle_platform = if *extension == "AppImage" {
                    target.split('-').next().unwrap()
                } else {
                    platform
                };
                expected.push((
                    format!("youta-desktop-9.8.7-{bundle_platform}.{extension}"),
                    "signed installer fixture\n",
                ));
            }
        }
        let appimage_expected = !cross && platform.starts_with("linux-");
        assert_eq!(root.join("appimage-prepared").exists(), appimage_expected);
        assert_eq!(root.join("appimage-validated").exists(), appimage_expected);
        let destination = root.join("dist");
        assert_eq!(
            fs::read_dir(&destination).unwrap().count(),
            expected.len() * 2
        );
        for (name, content) in expected {
            if name.ends_with(".AppImage") {
                assert_eq!(
                    fs::metadata(destination.join(&name))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o755,
                    "AppImages must be executable by users other than the build owner"
                );
            }
            assert_eq!(
                fs::read(destination.join(&name)).unwrap(),
                content.as_bytes(),
                "{name} changed after signing"
            );
            assert!(
                fs::metadata(destination.join(&name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o444
                    != 0
            );
            let checksum = fs::read_to_string(destination.join(format!("{name}.sha256"))).unwrap();
            assert!(
                checksum.ends_with(&format!("  {name}\n")),
                "checksum names the published asset"
            );
            let verification = Command::new("bash")
				.current_dir(&destination)
				.arg("-c")
				.arg("if command -v sha256sum >/dev/null 2>&1; then sha256sum --check \"$1\"; else shasum -a 256 --check \"$1\"; fi")
				.arg("verify-checksum")
				.arg(format!("{name}.sha256"))
				.output()
				.expect("verify published checksum");
            assert!(
                verification.status.success(),
                "incorrect checksum for {name}"
            );
        }
    }

    /// Platform naming, installer contents, and checksums survive native packaging.
    #[test]
    fn native_desktop_builds_strip_before_signing_on_each_supported_host() {
        for (target, platform) in [
            ("x86_64-unknown-linux-gnu", "linux-amd64"),
            ("aarch64-unknown-linux-gnu", "linux-arm64"),
            ("x86_64-apple-darwin", "macos-amd64"),
            ("aarch64-apple-darwin", "macos-arm64"),
            ("x86_64-pc-windows-msvc", "windows-amd64"),
            ("aarch64-pc-windows-msvc", "windows-arm64"),
        ] {
            package_fixture(target, platform, false, false);
        }
        package_fixture("aarch64-apple-darwin", "macos-arm64", false, true);
    }

    /// Cross-built GUI releases receive the same overrides without native bundles.
    #[test]
    fn cross_desktop_build_strips_before_copying_the_executable() {
        package_fixture("i686-unknown-linux-gnu", "linux-i686", true, false);
    }
}
