#!/usr/bin/env bash
#
# Package the exact Cargo.lock dependency graph for offline/external builders.
# Usage: scripts/package-vendor.sh [output-directory]

set -Eeuo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repository_root=$(CDPATH= cd -- "${script_dir}/.." && pwd)
cd "${repository_root}"

[[ -f Cargo.lock ]] || {
	echo 'Cargo.lock is required to create a reproducible vendor archive.' >&2
	exit 1
}

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

output_argument=${1:-dist}
mkdir -p "${output_argument}"
output_directory=$(CDPATH= cd -- "${output_argument}" && pwd)
staging_directory=$(mktemp -d)

cleanup() {
	rm -rf -- "${staging_directory}"
}
trap cleanup EXIT

# Match the directory produced by GitHub's v<version> source archive. Extracting
# both release files therefore merges the sources and vendor tree under ${P}.
package_root="${staging_directory}/youta-${version}"
mkdir -p "${package_root}/.cargo"

(
	cd "${package_root}"
	cargo vendor \
		--locked \
		--versioned-dirs \
		--manifest-path "${repository_root}/Cargo.toml" \
		vendor > .cargo/config.toml
)

# Make network isolation explicit for consumers that use the supplied config.
printf '\n[net]\noffline = true\n' >> "${package_root}/.cargo/config.toml"

archive="${output_directory}/youta-${version}-vendor.tar.xz"
source_date_epoch=${SOURCE_DATE_EPOCH:-0}

tar \
	--sort=name \
	--mtime="@${source_date_epoch}" \
	--owner=0 \
	--group=0 \
	--numeric-owner \
	-C "${staging_directory}" \
	-cJf "${archive}" \
	"youta-${version}"

(
	cd "${output_directory}"
	sha256sum "$(basename -- "${archive}")" > "$(basename -- "${archive}").sha256"
)
echo "Created ${archive}"
