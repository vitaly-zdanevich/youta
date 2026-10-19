#!/usr/bin/env bash
#
# Check the finished AppImage's launch-chain permissions without opening the GUI.
# The runtime extracts only AppRun* and usr/bin/youta-gui, so this requires neither
# a FUSE mount nor a separately installed SquashFS utility.
#
# Usage:
#   bash scripts/validate-appimage.sh path/to/youta.AppImage

set -Eeuo pipefail

[[ $# == 1 ]] || {
	echo 'Usage: validate-appimage.sh path/to/youta.AppImage' >&2
	exit 2
}

# Keep the image address valid after changing into the isolated extraction path.
case "$1" in
	/*) appimage=$1 ;;
	*) appimage="${PWD}/$1" ;;
esac

# Test mode bits rather than the current user's access: an owner can execute a
# mode-0770 wrapper that every unrelated user is forbidden to launch. GNU and
# BSD find both support the all-bits-set spelling of -perm used here.
require_public_executable() {
	local path=$1
	local label=$2
	[[ -f ${path} ]] || {
		echo "Missing AppImage executable: ${label}" >&2
		exit 1
	}
	[[ -n $(find -L "${path}" -prune -type f -perm -0555 -print) ]] || {
		echo "AppImage executable must be readable and executable by all users (mode bits 0555): ${label}" >&2
		exit 1
	}
}

# The runtime creates extraction directories with mode 0700 rather than their
# stored modes. Check their presence only; extracted modes cannot establish the
# embedded directories' accessibility to other users.
require_directory() {
	local path=$1
	local label=$2
	[[ -d ${path} ]] || {
		echo "Missing AppImage directory: ${label}" >&2
		exit 1
	}
}

require_public_executable "${appimage}" "${appimage}"

validation_directory=$(mktemp -d "${TMPDIR:-/tmp}/youta-appimage-validation.XXXXXX")
readonly validation_directory
# Only the exact directory returned by mktemp is removed, including on failure.
trap 'rm -rf -- "${validation_directory}"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if ! (
	cd "${validation_directory}" &&
		"${appimage}" --appimage-extract 'AppRun*' > /dev/null &&
		"${appimage}" --appimage-extract usr/bin/youta-gui > /dev/null
); then
	echo "Unable to extract AppImage launch-chain members: ${appimage}" >&2
	exit 1
fi

appdir="${validation_directory}/squashfs-root"
for directory in . usr usr/bin; do
	require_directory "${appdir}/${directory}" "${directory}"
done
for executable in AppRun AppRun.wrapped usr/bin/youta-gui; do
	require_public_executable "${appdir}/${executable}" "${executable}"
done

echo "Validated AppImage launch permissions: ${appimage}"
