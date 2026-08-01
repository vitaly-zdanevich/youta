#!/usr/bin/env bash
#
# Build a versioned Linux or macOS binary archive for one explicit Rust target.
# Usage:
#   scripts/package-release.sh [target-triple] [output-directory] [images|text]

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
		echo 'Supported targets: x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu,' >&2
		echo 'x86_64-apple-darwin, aarch64-apple-darwin' >&2
		exit 1
		;;
esac

case "${variant}" in
	images)
		archive_suffix=
		;;
	text)
		archive_suffix=-text
		;;
	*)
		echo "Unsupported release variant: ${variant}" >&2
		echo 'Supported variants: images, text' >&2
		exit 1
		;;
esac

mkdir -p "${output_argument}"
output_directory=$(CDPATH= cd -- "${output_argument}" && pwd)
staging_directory=$(mktemp -d)

cleanup() {
	rm -rf -- "${staging_directory}"
}
trap cleanup EXIT

package_name="youta-${version}-${operating_system}-${architecture}${archive_suffix}"
package_root="${staging_directory}/${package_name}"
export YOUTA_BUILD_ORIGIN=github-release

if [[ ${variant} == text ]]; then
	cargo build \
		--locked \
		--release \
		--target "${target}" \
		--no-default-features \
		--features app
else
	cargo build \
		--locked \
		--release \
		--target "${target}"
fi

mkdir -p "${package_root}/bin" "${package_root}/docs"
install -m 755 \
	"target/${target}/release/youta" \
	"${package_root}/bin/youta"
install -m 644 README.md "${package_root}/README.md"
install -m 644 LICENSE "${package_root}/LICENSE"
install -m 644 config.example.toml "${package_root}/config.example.toml"
install -m 644 docs/ARCHITECTURE.md "${package_root}/docs/ARCHITECTURE.md"
install -m 644 docs/FEASIBILITY.md "${package_root}/docs/FEASIBILITY.md"
install -m 644 docs/AUDIOPHILE.md "${package_root}/docs/AUDIOPHILE.md"

archive="${output_directory}/${package_name}.tar.gz"
source_date_epoch=${SOURCE_DATE_EPOCH:-0}

if command -v gtar > /dev/null 2>&1; then
	tar_command=gtar
else
	tar_command=tar
fi
tar_version=$("${tar_command}" --version 2> /dev/null || true)
if [[ ${tar_version} != *'GNU tar'* ]]; then
	echo 'Reproducible release packaging requires GNU tar (`tar` or `gtar`).' >&2
	exit 1
fi

"${tar_command}" \
	--sort=name \
	--mtime="@${source_date_epoch}" \
	--owner=0 \
	--group=0 \
	--numeric-owner \
	-C "${staging_directory}" \
	-cf - \
	"${package_name}" |
	gzip -n > "${archive}"

(
	cd "${output_directory}"
	archive_name=$(basename -- "${archive}")
	if command -v sha256sum > /dev/null 2>&1; then
		sha256sum "${archive_name}" > "${archive_name}.sha256"
	elif command -v shasum > /dev/null 2>&1; then
		shasum -a 256 "${archive_name}" > "${archive_name}.sha256"
	else
		echo 'Release packaging requires `sha256sum` or `shasum`.' >&2
		exit 1
	fi
)
echo "Created ${archive}"
