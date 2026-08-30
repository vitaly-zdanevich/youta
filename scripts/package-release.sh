#!/usr/bin/env bash
#
# Build a versioned Linux or macOS executable for one explicit Rust target.
# Usage:
#   scripts/package-release.sh [target-triple] [output-directory] [variant]
#
# Variants are images, text, images-no-qr, text-no-qr, and their Linux-only
# images-no-gpm, text-no-gpm, images-no-qr-no-gpm, and
# text-no-qr-no-gpm counterparts.

set -Eeuo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repository_root=$(CDPATH= cd -- "${script_dir}/.." && pwd)
cd "${repository_root}"

version=$(
	awk -F '"' '
		/^\[package\]$/ { in_package = 1; next }
		in_package && /^\[/ { exit }
		in_package && /^version = / { print $2; exit }
	' Cargo.toml
)
[[ -n ${version} ]] || {
	echo 'Unable to read the package version from Cargo.toml.' >&2
	exit 1
}

host_target=$(rustc -vV | sed -n 's/^host: //p')
target=${1:-${host_target}}
output_argument=${2:-dist}
variant=${3:-images}

case "${target}" in
	x86_64-unknown-linux-gnu)
		operating_system=linux
		architecture=amd64
		;;
	i686-unknown-linux-gnu)
		operating_system=linux
		architecture=i686
		;;
	aarch64-unknown-linux-gnu)
		operating_system=linux
		architecture=arm64
		;;
	x86_64-apple-darwin)
		operating_system=macos
		architecture=amd64
		;;
	aarch64-apple-darwin)
		operating_system=macos
		architecture=arm64
		;;
	*)
		echo "Unsupported release target: ${target}" >&2
		echo 'Supported targets: x86_64-unknown-linux-gnu, i686-unknown-linux-gnu,' >&2
		echo 'aarch64-unknown-linux-gnu,' >&2
		echo 'x86_64-apple-darwin, aarch64-apple-darwin' >&2
		exit 1
		;;
esac

case "${variant}" in
	images)
		executable_suffix=
		cargo_features=app,audio-quality,commons-upload,evernote,gpm,images,local-archives,nyan-cat,qr,sponsorblock,summary
		;;
	text)
		executable_suffix=-text
		cargo_features=app,audio-quality,commons-upload,evernote,gpm,local-archives,nyan-cat,qr,sponsorblock,summary
		;;
	images-no-qr)
		executable_suffix=-no-qr
		cargo_features=app,audio-quality,commons-upload,evernote,gpm,images,local-archives,nyan-cat,sponsorblock,summary
		;;
	text-no-qr)
		executable_suffix=-text-no-qr
		cargo_features=app,audio-quality,commons-upload,evernote,gpm,local-archives,nyan-cat,sponsorblock,summary
		;;
	images-no-gpm)
		executable_suffix=-no-gpm
		cargo_features=app,audio-quality,commons-upload,evernote,images,local-archives,nyan-cat,qr,sponsorblock,summary
		;;
	text-no-gpm)
		executable_suffix=-text-no-gpm
		cargo_features=app,audio-quality,commons-upload,evernote,local-archives,nyan-cat,qr,sponsorblock,summary
		;;
	images-no-qr-no-gpm)
		executable_suffix=-no-qr-no-gpm
		cargo_features=app,audio-quality,commons-upload,evernote,images,local-archives,nyan-cat,sponsorblock,summary
		;;
	text-no-qr-no-gpm)
		executable_suffix=-text-no-qr-no-gpm
		cargo_features=app,audio-quality,commons-upload,evernote,local-archives,nyan-cat,sponsorblock,summary
		;;
	*)
		echo "Unsupported release variant: ${variant}" >&2
		echo 'Supported variants: images, text, images-no-qr, text-no-qr,' >&2
		echo 'images-no-gpm, text-no-gpm, images-no-qr-no-gpm, text-no-qr-no-gpm' >&2
		exit 1
		;;
esac

if [[ ${variant} == *-no-gpm && ${operating_system} != linux ]]; then
	echo 'No-GPM release variants are supported only on Linux.' >&2
	exit 1
fi

mkdir -p "${output_argument}"
output_directory=$(CDPATH= cd -- "${output_argument}" && pwd)
package_name="youta-${version}-${operating_system}-${architecture}${executable_suffix}"
export YOUTA_BUILD_ORIGIN=github-release

cargo build \
	--locked \
	--release \
	--target "${target}" \
	--no-default-features \
	--features "${cargo_features}"

executable="${output_directory}/${package_name}"
install -m 755 \
	"target/${target}/release/youta" \
	"${executable}"

(
	cd "${output_directory}"
	executable_name=$(basename -- "${executable}")
	if command -v sha256sum > /dev/null 2>&1; then
		sha256sum "${executable_name}" > "${executable_name}.sha256"
	elif command -v shasum > /dev/null 2>&1; then
		shasum -a 256 "${executable_name}" > "${executable_name}.sha256"
	else
		echo 'Release packaging requires `sha256sum` or `shasum`.' >&2
		exit 1
	fi
)
echo "Created ${executable}"
