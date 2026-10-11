#!/usr/bin/env bash
#
# Build a versioned, stripped Linux or macOS executable for one Rust target.
# Usage:
#   scripts/package-release.sh [target-triple] [output-directory] [variant]
#
# Variants are images, text, images-no-qr, text-no-qr, and their Linux-only
# images-no-gpm, text-no-gpm, images-no-qr-no-gpm, and
# text-no-qr-no-gpm counterparts.
# The images variant also ships an optimized, unstripped -debug executable;
# macOS includes its line tables in a matching -debug.dSYM.tar.xz archive.
# Cross-compilers can set YOUTA_RELEASE_STRIP to a compatible strip executable.

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
		cargo_features=app,archive-org,archive-upload,ascii-visualizer,audio-quality,cache,commons-upload,evernote,gpm,images,lan-sharing,local-archives,nyan-cat,qr,sponsorblock,summary,url-info,web-browser,youtube-captions
		;;
	text)
		executable_suffix=-text
		cargo_features=app,archive-org,archive-upload,ascii-visualizer,audio-quality,cache,commons-upload,evernote,gpm,lan-sharing,local-archives,nyan-cat,qr,sponsorblock,summary,url-info,web-browser,youtube-captions
		;;
	images-no-qr)
		executable_suffix=-no-qr
		cargo_features=app,archive-org,archive-upload,ascii-visualizer,audio-quality,cache,commons-upload,evernote,gpm,images,local-archives,nyan-cat,sponsorblock,summary,url-info,web-browser,youtube-captions
		;;
	text-no-qr)
		executable_suffix=-text-no-qr
		cargo_features=app,archive-org,archive-upload,ascii-visualizer,audio-quality,cache,commons-upload,evernote,gpm,local-archives,nyan-cat,sponsorblock,summary,url-info,web-browser,youtube-captions
		;;
	images-no-gpm)
		executable_suffix=-no-gpm
		cargo_features=app,archive-org,archive-upload,ascii-visualizer,audio-quality,cache,commons-upload,evernote,images,lan-sharing,local-archives,nyan-cat,qr,sponsorblock,summary,url-info,web-browser,youtube-captions
		;;
	text-no-gpm)
		executable_suffix=-text-no-gpm
		cargo_features=app,archive-org,archive-upload,ascii-visualizer,audio-quality,cache,commons-upload,evernote,lan-sharing,local-archives,nyan-cat,qr,sponsorblock,summary,url-info,web-browser,youtube-captions
		;;
	images-no-qr-no-gpm)
		executable_suffix=-no-qr-no-gpm
		cargo_features=app,archive-org,archive-upload,ascii-visualizer,audio-quality,cache,commons-upload,evernote,images,local-archives,nyan-cat,sponsorblock,summary,url-info,web-browser,youtube-captions
		;;
	text-no-qr-no-gpm)
		executable_suffix=-text-no-qr-no-gpm
		cargo_features=app,archive-org,archive-upload,ascii-visualizer,audio-quality,cache,commons-upload,evernote,local-archives,nyan-cat,sponsorblock,summary,url-info,web-browser,youtube-captions
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

# Native binutils can also strip i686 on amd64, and Apple's strip handles both
# macOS architectures. Other cross-target builds require an explicit tool.
strip_tool=${YOUTA_RELEASE_STRIP:-strip}
if [[ -z ${YOUTA_RELEASE_STRIP:-} && ${host_target} != "${target}" ]]; then
	case "${host_target}:${target}" in
		x86_64-unknown-linux-gnu:i686-unknown-linux-gnu|*-apple-darwin:*-apple-darwin)
			;;
		*)
			echo "Set YOUTA_RELEASE_STRIP to a strip executable for ${target}." >&2
			exit 1
			;;
	esac
fi
if ! command -v "${strip_tool}" > /dev/null 2>&1; then
	echo "Release packaging requires a strip executable: ${strip_tool}" >&2
	exit 1
fi
if [[ ${operating_system} == macos && ${variant} == images ]]; then
	for tool in dsymutil tar; do
		if ! command -v "${tool}" > /dev/null 2>&1; then
			echo "macOS debug packaging requires ${tool}." >&2
			exit 1
		fi
	done
fi

mkdir -p "${output_argument}"
output_directory=$(CDPATH= cd -- "${output_argument}" && pwd)
package_name="youta-${version}-${operating_system}-${architecture}${executable_suffix}"
export YOUTA_BUILD_ORIGIN=github-release

# Compile once with symbols intact, independent of inherited profile overrides.
# Linux keeps line tables in the executable; macOS collects them with dsymutil
# before a later feature variant can replace the build's object files.
export CARGO_PROFILE_RELEASE_STRIP=none
export CARGO_PROFILE_RELEASE_DEBUG=line-tables-only
if [[ ${operating_system} == linux ]]; then
	export CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO=off
else
	export CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO=unpacked
fi

cargo build \
	--locked \
	--release \
	--target "${target}" \
	--no-default-features \
	--features "${cargo_features}"

executable="${output_directory}/${package_name}"
artifacts=("${executable}")
if [[ ${variant} == images ]]; then
	debug_executable="${executable}-debug"
	install -m 755 "target/${target}/release/youta" "${debug_executable}"
	artifacts+=("${debug_executable}")
	if [[ ${operating_system} == macos ]]; then
		symbols_name="${package_name}-debug.dSYM"
		# The cleanup target is the unique directory returned by mktemp inside
		# the resolved output directory; it never points at the output root.
		symbols_directory=$(mktemp -d "${output_directory}/.youta-dsym.XXXXXX")
		trap 'rm -rf -- "${symbols_directory}"' EXIT
		# LLDB's adjacent-bundle lookup also needs the DWARF member to have
		# the downloaded executable's name. Copying preserves its UUID and
		# object map, so collect from that copy while the objects still exist.
		dsymutil "${debug_executable}" -o "${symbols_directory}/${symbols_name}"
		symbols_archive="${output_directory}/${symbols_name}.tar.xz"
		tar -cJf "${symbols_archive}" -C "${symbols_directory}" "${symbols_name}"
		artifacts+=("${symbols_archive}")
	fi
fi

# Strip only the installed normal copy: Cargo's output and the debug download
# retain symbols. Apple's native strip preserves globals used by dynamic code
# and regenerates the linker's ad-hoc signature when it updates the binary.
install -m 755 \
	"target/${target}/release/youta" \
	"${executable}"
if [[ ${operating_system} == linux ]]; then
	"${strip_tool}" --strip-all "${executable}"
else
	"${strip_tool}" -S -x "${executable}"
fi

(
	cd "${output_directory}"
	for artifact in "${artifacts[@]}"; do
		artifact_name=$(basename -- "${artifact}")
		if command -v sha256sum > /dev/null 2>&1; then
			sha256sum "${artifact_name}" > "${artifact_name}.sha256"
		elif command -v shasum > /dev/null 2>&1; then
			shasum -a 256 "${artifact_name}" > "${artifact_name}.sha256"
		else
			echo 'Release packaging requires `sha256sum` or `shasum`.' >&2
			exit 1
		fi
	done
)
for artifact in "${artifacts[@]}"; do
	echo "Created ${artifact}"
done
