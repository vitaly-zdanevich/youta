#!/usr/bin/env bash
#
# Seed Tauri's project-local AppRun cache before AppImage compression.
# This file only defines a function; sourcing it does not download or change files.

# Prepare a verified AppRun in CARGO_TARGET_DIRECTORY/.tauri for x86_64 or
# aarch64. The caller must enable bundle.useLocalToolsDir in the Tauri build.
#
# Tauri CLI 2.11.4 downloads AppRun with mode 0770 and preserves that mode when
# copying it into the AppDir. linuxdeploy then wraps it as AppRun.wrapped, so
# users outside the build account's group cannot execute the packaged launcher.
# Seed the same upstream binary with mode 0755 before Tauri makes that copy.
#
# The checksums pin the apprun-old assets selected by this CLI version. Existing
# cache entries must match too; unknown files are rejected without replacement.
# Download failures never leave a partial file at the path Tauri trusts.
#
# Sources:
# https://github.com/tauri-apps/tauri/blob/tauri-cli-v2.11.4/crates/tauri-bundler/src/bundle/linux/appimage/mod.rs
# https://github.com/tauri-apps/tauri/blob/tauri-cli-v2.11.4/crates/tauri-bundler/src/bundle/linux/appimage/linuxdeploy.rs
# https://v2.tauri.app/reference/config/#uselocaltoolsdir
prepare_appimage_tools() (
	set -Eeuo pipefail
	local repository_target_directory=${1:?Usage: prepare_appimage_tools TARGET_DIRECTORY ARCHITECTURE}
	local appimage_architecture=${2:?Usage: prepare_appimage_tools TARGET_DIRECTORY ARCHITECTURE}
	local expected_checksum
	case "${appimage_architecture}" in
		x86_64)
			expected_checksum=f30140a43a0a59e46db21bdefdf749b9e9f2c6946e92afabbacf98b8ae73fb4f
			;;
		aarch64)
			expected_checksum=072f17c0895a85c490282fe5395c5007e5fc75da727e553b3b8fb680feb11578
			;;
		*)
			echo "Unsupported AppImage tools architecture: ${appimage_architecture}" >&2
			return 1
			;;
	esac

	local tools_directory="${repository_target_directory}/.tauri"
	local apprun_path="${tools_directory}/AppRun-${appimage_architecture}"
	local candidate_path=${apprun_path}
	local download_path=
	mkdir -p -- "${tools_directory}" || return 1

	if [[ -e ${apprun_path} || -L ${apprun_path} ]]; then
		if [[ ! -f ${apprun_path} || -L ${apprun_path} ]]; then
			echo "Refusing non-regular AppRun cache entry: ${apprun_path}" >&2
			return 1
		fi
	else
		download_path=$(mktemp "${tools_directory}/.AppRun-${appimage_architecture}.XXXXXX") || return 1
		trap 'rm -f -- "${download_path}"' EXIT
		trap 'exit 130' INT
		trap 'exit 143' TERM
		curl --fail --location --silent --show-error \
			--connect-timeout 15 --max-time 120 \
			--retry 3 --retry-delay 2 --retry-max-time 180 \
			--output "${download_path}" \
			"https://github.com/tauri-apps/binary-releases/releases/download/apprun-old/AppRun-${appimage_architecture}" || return 1
		candidate_path=${download_path}
	fi

	local actual_checksum
	actual_checksum=$(sha256sum < "${candidate_path}") || return 1
	if [[ ${actual_checksum%% *} != "${expected_checksum}" ]]; then
		echo "AppRun SHA-256 mismatch for ${appimage_architecture}; refusing to package it." >&2
		return 1
	fi
	chmod 755 -- "${candidate_path}" || return 1
	if [[ -n ${download_path} ]]; then
		mv -- "${download_path}" "${apprun_path}" || return 1
	fi
)
