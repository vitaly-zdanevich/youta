//! Permission and extraction regressions for the finished Linux AppImage.

#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

/// A mock AppImage runtime extracts only the requested launch-chain members.
const MOCK_APPIMAGE: &str = r#"#!/usr/bin/env bash
set -Eeuo pipefail
[[ $# == 2 && $1 == --appimage-extract ]] || {
	echo 'The validator attempted to launch the GUI or extract unbounded contents.' >&2
	exit 91
}
printf '%s\n' "$2" >> "${FIXTURE_LOG}"
mkdir -p squashfs-root/usr/bin
# The actual AppImage runtime does not preserve embedded directory modes.
chmod 700 squashfs-root squashfs-root/usr squashfs-root/usr/bin
case "$2" in
	'AppRun*') members=(AppRun AppRun.wrapped) ;;
	usr/bin/youta-gui) members=(usr/bin/youta-gui) ;;
	*) exit 92 ;;
esac
for member in "${members[@]}"; do
	[[ ${FIXTURE_MISSING:-} != "${member}" ]] || continue
	printf '#!/usr/bin/env bash\nexit 93\n' > "squashfs-root/${member}"
	chmod 755 "squashfs-root/${member}"
	if [[ ${FIXTURE_RESTRICTED:-} == "${member}" ]]; then
		chmod "${FIXTURE_MODE}" "squashfs-root/${member}"
	fi
done
[[ ${FIXTURE_FAIL_EXTRACTION:-} != "$2" ]] || exit 94
"#;

/// Owns an isolated runtime and extraction directory, including paths with spaces.
struct AppImageFixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    image: PathBuf,
    extraction_parent: PathBuf,
}

impl AppImageFixture {
    /// Creates an executable fixture without any Tauri, FUSE, GUI, or network dependency.
    fn new() -> Self {
        let temporary = tempfile::Builder::new()
            .prefix("youta AppImage fixture ")
            .tempdir()
            .expect("isolated AppImage fixture");
        let root = temporary.path().to_path_buf();
        let image = root.join("desktop release.AppImage");
        fs::write(&image, MOCK_APPIMAGE).expect("write mock AppImage runtime");
        fs::set_permissions(&image, fs::Permissions::from_mode(0o755))
            .expect("make runtime executable for all users");
        let extraction_parent = root.join("validation scratch");
        fs::create_dir(&extraction_parent).expect("create validation temporary parent");
        Self {
            _temporary: temporary,
            root,
            image,
            extraction_parent,
        }
    }

    /// Runs the real validator and verifies that success and failures clean up extraction.
    fn validate(&self, relative: bool, settings: &[(&str, &str)]) -> Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/validate-appimage.sh");
        let image = if relative {
            self.image.strip_prefix(&self.root).unwrap()
        } else {
            &self.image
        };
        let output = Command::new("bash")
            .arg(script)
            .arg(image)
            .current_dir(&self.root)
            .env("TMPDIR", &self.extraction_parent)
            .env("FIXTURE_LOG", self.root.join("extractions"))
            .envs(settings.iter().copied())
            .output()
            .expect("run actual AppImage validator");
        assert_eq!(
            fs::read_dir(&self.extraction_parent).unwrap().count(),
            0,
            "the validator must remove its exact temporary extraction directory"
        );
        output
    }

    /// Reports the bounded extraction patterns requested by the validator.
    fn extractions(&self) -> String {
        fs::read_to_string(self.root.join("extractions")).unwrap_or_default()
    }
}

/// Requires the diagnostic to identify the failed launch-chain member.
fn assert_rejected(output: Output, diagnostic: &str) {
    assert!(!output.status.success(), "invalid AppImage was accepted");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(diagnostic),
        "unexpected diagnostic: {stderr}"
    );
}

/// Finished images can be checked through either path form without opening a window.
#[test]
fn valid_appimage_extracts_only_launch_members_with_spaces_in_paths() {
    for relative in [false, true] {
        let fixture = AppImageFixture::new();
        let output = fixture.validate(relative, &[]);
        assert!(
            output.status.success(),
            "valid AppImage rejected: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fixture.extractions(), "AppRun*\nusr/bin/youta-gui\n");
    }
}

/// An owner's executable access must not hide Tauri's embedded mode-0770 regression.
#[test]
fn launch_members_must_be_readable_and_executable_by_all_users() {
    for member in ["AppRun", "AppRun.wrapped", "usr/bin/youta-gui"] {
        for mode in ["0770", "0711", "0744"] {
            let fixture = AppImageFixture::new();
            assert_rejected(
                fixture.validate(
                    false,
                    &[("FIXTURE_RESTRICTED", member), ("FIXTURE_MODE", mode)],
                ),
                member,
            );
        }
    }
}

/// Each executable in the launch chain must actually exist in the finished image.
#[test]
fn missing_launch_members_are_rejected() {
    for member in ["AppRun", "AppRun.wrapped", "usr/bin/youta-gui"] {
        let fixture = AppImageFixture::new();
        assert_rejected(
            fixture.validate(false, &[("FIXTURE_MISSING", member)]),
            member,
        );
    }
}

/// Release permissions are checked before attempting to execute the outer runtime.
#[test]
fn outer_appimage_requires_read_and_execute_permissions_for_all_users() {
    for mode in [0o644, 0o770, 0o711, 0o744] {
        let fixture = AppImageFixture::new();
        fs::set_permissions(&fixture.image, fs::Permissions::from_mode(mode)).unwrap();
        assert_rejected(fixture.validate(false, &[]), "desktop release.AppImage");
        assert!(
            fixture.extractions().is_empty(),
            "invalid runtime was executed"
        );
    }
}

/// Read-only executables remain valid; publication does not require write permission.
#[test]
fn public_read_only_launch_members_are_valid() {
    for member in ["AppRun", "AppRun.wrapped", "usr/bin/youta-gui"] {
        let fixture = AppImageFixture::new();
        let output = fixture.validate(
            false,
            &[("FIXTURE_RESTRICTED", member), ("FIXTURE_MODE", "0555")],
        );
        assert!(
            output.status.success(),
            "read-only AppImage member rejected: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// An extraction failure must propagate even when the runtime wrote its output first.
#[test]
fn extraction_errors_are_rejected_and_cleaned_up() {
    for pattern in ["AppRun*", "usr/bin/youta-gui"] {
        let fixture = AppImageFixture::new();
        assert_rejected(
            fixture.validate(false, &[("FIXTURE_FAIL_EXTRACTION", pattern)]),
            "extract",
        );
    }
}
