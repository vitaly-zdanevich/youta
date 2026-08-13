#!/usr/bin/env bash
#
# Build the Youta desktop window's standalone executable and installable
# bundles for the host platform, with checksums, into one output directory.
#
# Usage:
#   scripts/package-desktop.sh [output-directory]
#
# Unlike scripts/package-release.sh, this does not cross-compile. A desktop
# bundle embeds a platform installer format — `.deb`, `.rpm`, AppImage, `.dmg`,
# NSIS — and every one of those is produced by a tool that only runs on its own
# operating system. The release workflow therefore runs this once per runner
# rather than once per target triple.

set -Eeuo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repository_root=$(CDPATH= cd -- "${script_dir}/.." && pwd)
cd "${repository_root}"

version=$(
	awk -F '"' '
		/^\[package\]$/ { in_package = 1; next }
		in_package && /^\[/ { exit }
		in_package && /^version = / { print $2; exit }
	' gui/Cargo.toml
)
[[ -n ${version} ]] || {
	echo 'Unable to read the desktop version from gui/Cargo.toml.' >&2
	exit 1
}

host_target=$(rustc -vV | sed -n 's/^host: //p')

case "${host_target}" in
	x86_64-unknown-linux-gnu)
		operating_system=linux
		architecture=amd64
		executable_extension=
		;;
	aarch64-unknown-linux-gnu)
		operating_system=linux
		architecture=arm64
		executable_extension=
		;;
	x86_64-apple-darwin)
		operating_system=macos
		architecture=amd64
		executable_extension=
		;;
	aarch64-apple-darwin)
		operating_system=macos
		architecture=arm64
		executable_extension=
		;;
	x86_64-pc-windows-msvc)
		operating_system=windows
		architecture=amd64
		executable_extension=.exe
		;;
	aarch64-pc-windows-msvc)
		operating_system=windows
		architecture=arm64
		executable_extension=.exe
		;;
	*)
		echo "Unsupported desktop host: ${host_target}" >&2
		echo 'Supported hosts: x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu,' >&2
		echo 'x86_64-apple-darwin, aarch64-apple-darwin,' >&2
		echo 'x86_64-pc-windows-msvc, aarch64-pc-windows-msvc' >&2
		exit 1
		;;
esac

output_argument=${1:-dist-desktop}
mkdir -p "${output_argument}"
output_directory=$(CDPATH= cd -- "${output_argument}" && pwd)

# Write a checksum beside one release asset without assuming GNU coreutils on
# macOS. The executable and installers consequently share one checksum format.
write_checksum() {
	local asset_path=$1
	(
		cd "${output_directory}"
		asset=$(basename -- "${asset_path}")
		if command -v sha256sum > /dev/null 2>&1; then
			sha256sum "${asset}" > "${asset}.sha256"
		elif command -v shasum > /dev/null 2>&1; then
			shasum -a 256 "${asset}" > "${asset}.sha256"
		else
			echo 'Desktop packaging requires `sha256sum` or `shasum`.' >&2
			exit 1
		fi
	)
}

command -v npm > /dev/null 2>&1 || {
	echo 'The desktop window needs Node and npm to build its page.' >&2
	exit 1
}

export YOUTA_BUILD_ORIGIN=github-release

# The window's page is compiled into the binary, so it is built first and its
# exit status is checked: a TypeScript failure that only shows in the log would
# otherwise ship the previous bundle unchanged.
npm --prefix gui/ui ci
npm --prefix gui/ui run build

# `--no-bundle` is deliberately absent: the bundles are the artefact. Signing is
# driven entirely by environment variables the workflow supplies from secrets,
# so an unsigned local build and a signed release build run the same command.
#
# The bundler runs from `gui/`, where its configuration lives: every path inside
# that file, including the page directory and the icons, is written relative to
# it. The workspace target directory stays at the repository root regardless.
(cd gui && npx --yes "@tauri-apps/cli@2" build)

produced_executable="target/release/youta-gui${executable_extension}"
[[ -f ${produced_executable} ]] || {
	echo "The desktop build produced no standalone executable at ${produced_executable}." >&2
	exit 1
}
executable="${output_directory}/youta-gui-${version}-${operating_system}-${architecture}${executable_extension}"
install -m 755 "${produced_executable}" "${executable}"
write_checksum "${executable}"
echo "Created ${executable}"

bundle_root="target/release/bundle"
[[ -d ${bundle_root} ]] || {
	echo "The Tauri bundler produced nothing under ${bundle_root}." >&2
	exit 1
}

# Only the bundler's per-format output directories are collected from, never
# the whole tree. `bundle/macos` holds the `.app` and, next to it, the
# read-write staging image the DMG is assembled in — a `.dmg` by name and by
# extension, five times the size, and not something anyone should install.
collected=0
for format in deb rpm appimage dmg nsis msi; do
	format_directory="${bundle_root}/${format}"
	[[ -d ${format_directory} ]] || continue
	while IFS= read -r produced; do
		extension=${produced##*.}
		case "${extension}" in
			deb | rpm | AppImage | dmg | exe | msi) ;;
			*) continue ;;
		esac
		destination="${output_directory}/youta-desktop-${version}-${operating_system}-${architecture}.${extension}"
		# Two bundles claiming one name means the bundler produced something
		# this script does not understand. Reporting the last one silently
		# would publish an arbitrary choice.
		[[ -e ${destination} ]] && {
			echo "Two bundles claim ${destination}; refusing to choose between them." >&2
			exit 1
		}
		collected=$((collected + 1))
		install -m 644 "${produced}" "${destination}"
		write_checksum "${destination}"
		echo "Created ${destination}"
	done < <(find "${format_directory}" -maxdepth 1 -type f \
		\( -name '*.deb' -o -name '*.rpm' -o -name '*.AppImage' \
		-o -name '*.dmg' -o -name '*-setup.exe' -o -name '*.msi' \) | sort)
done

[[ ${collected} -gt 0 ]] || {
	echo 'No installable bundle was produced; refusing to report success.' >&2
	exit 1
}
