#!/usr/bin/env bash
#
# Cross-build one unbundled desktop executable without native installers.
#
# Usage:
#   scripts/package-desktop-executable.sh TARGET [output-directory]
#
# Native installers remain the responsibility of package-desktop.sh. This
# boundary exists for architectures such as Linux i686: Rust and the target's
# GUI libraries can cross-build the dynamically linked executable, while
# Tauri's installer tools are only supported as native host tools.

set -Eeuo pipefail

target=${1:?Usage: package-desktop-executable.sh TARGET [output-directory]}
output_argument=${2:-dist-desktop}

case "${target}" in
	i686-unknown-linux-gnu)
		operating_system=linux
		architecture=i686
		executable_extension=
		;;
	*)
		echo "Unsupported standalone desktop target: ${target}" >&2
		echo 'Supported target: i686-unknown-linux-gnu' >&2
		exit 1
		;;
esac

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

command -v npm > /dev/null 2>&1 || {
	echo 'The desktop window needs Node and npm to build its page.' >&2
	exit 1
}

mkdir -p "${output_argument}"
output_directory=$(CDPATH= cd -- "${output_argument}" && pwd)

export YOUTA_BUILD_ORIGIN=github-release
npm --prefix gui/ui ci
npm --prefix gui/ui run build

# Tauri CLI supplies its production-only `custom-protocol` feature. A direct
# `cargo build` would compile successfully while retaining the development
# asset path, so keep the same pinned CLI as native desktop packaging and only
# disable its host-specific installer phase.
(cd gui && npx --yes "@tauri-apps/cli@2.11.4" build \
	--target "${target}" \
	--no-bundle)

produced_executable="target/${target}/release/youta-gui${executable_extension}"
[[ -f ${produced_executable} ]] || {
	echo "The desktop build produced no standalone executable at ${produced_executable}." >&2
	exit 1
}

executable="${output_directory}/youta-gui-${version}-${operating_system}-${architecture}${executable_extension}"
install -m 755 "${produced_executable}" "${executable}"
(
	cd "${output_directory}"
	asset=$(basename -- "${executable}")
	sha256sum "${asset}" > "${asset}.sha256"
)
echo "Created ${executable}"
