#!/usr/bin/env bash
#
# Build a versioned Linux binary archive for one explicit Rust target.
# Usage: scripts/package-release.sh [target-triple] [output-directory]

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

case "${target}" in
	x86_64-unknown-linux-gnu)
		architecture=amd64
		;;
	aarch64-unknown-linux-gnu)
		architecture=arm64
		;;
	*)
		echo "Unsupported release target: ${target}" >&2
		echo 'Supported targets: x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu' >&2
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

package_name="youta-${version}-linux-${architecture}"
package_root="${staging_directory}/${package_name}"

cargo build \
	--locked \
	--release \
	--target "${target}" \
	--features bundled-sqlite

install -Dm755 \
	"target/${target}/release/youta" \
	"${package_root}/bin/youta"
install -Dm644 README.md "${package_root}/README.md"
install -Dm644 LICENSE "${package_root}/LICENSE"
install -Dm644 config.example.toml "${package_root}/config.example.toml"
install -Dm644 docs/ARCHITECTURE.md "${package_root}/docs/ARCHITECTURE.md"
install -Dm644 docs/FEASIBILITY.md "${package_root}/docs/FEASIBILITY.md"
install -Dm644 docs/AUDIOPHILE.md "${package_root}/docs/AUDIOPHILE.md"

archive="${output_directory}/${package_name}.tar.gz"
source_date_epoch=${SOURCE_DATE_EPOCH:-0}

tar \
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
	sha256sum "$(basename -- "${archive}")" > "$(basename -- "${archive}").sha256"
)
echo "Created ${archive}"
