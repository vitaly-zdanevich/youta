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
[[ -f gui/frontend/index.html ]] &&
	[[ -f gui/frontend/app.js ]] &&
	[[ -f gui/frontend/app.css ]] || {
	echo 'Build the production GUI frontend before creating the vendor archive.' >&2
	echo 'Run: npm --prefix gui/ui ci && npm --prefix gui/ui run build' >&2
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

# Gentoo and other offline source builders cannot resolve npm packages inside
# their build sandbox. Carry only Vite's deterministic production output beside
# the vendored Rust graph; the source archive supplies the editable UI sources.
mkdir -p "${package_root}/gui/frontend"
cp -a gui/frontend/. "${package_root}/gui/frontend/"

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

# Prove that the generated source replacement is complete before publishing it.
# Fresh Cargo and target directories prevent an existing registry/source cache
# from hiding an omitted dependency, and both remain outside the archive root.
verification_cargo_home="${staging_directory}/offline-cargo-home"
verification_target="${staging_directory}/offline-target"
mkdir -p "${verification_cargo_home}" "${verification_target}"
(
	cd "${package_root}"
	CARGO_HOME="${verification_cargo_home}" \
		CARGO_TARGET_DIR="${verification_target}" \
		cargo build \
			--manifest-path "${repository_root}/Cargo.toml" \
			--locked \
			--offline \
			--workspace \
			--all-features
)

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
