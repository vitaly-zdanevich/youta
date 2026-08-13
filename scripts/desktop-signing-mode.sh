#!/usr/bin/env bash
#
# Select Tauri's explicit signing mode for desktop packaging.
#
# This file is sourced by scripts/package-desktop.sh and has no side effects;
# callers explicitly invoke `configure_desktop_tauri_signing_args` after they
# have identified the host operating system.

# Populates `tauri_signing_args` for one normalized host operating-system name.
#
# GitHub Actions exports a missing secret as an empty environment variable.
# On macOS, Tauri interprets an empty APPLE_CERTIFICATE as signing material and
# attempts to import it. Its supported `--no-sign` switch states the unsigned
# local/CI intent directly. A nonempty certificate retains Tauri's normal
# signing and notarization behavior, and other platforms receive no override.
configure_desktop_tauri_signing_args() {
	local operating_system=$1
	tauri_signing_args=()

	if [[ ${operating_system} == macos && -z ${APPLE_CERTIFICATE:-} ]]; then
		tauri_signing_args+=(--no-sign)
	fi
}
