#!/usr/bin/env bash
#
# Exercise Youta's production mpv/yt-dlp path against a real YouTube video.
# Playback is silent by default. Pass --audible to use the configured default
# audio output, and set YOUTA_LIVE_YOUTUBE_URL to choose another public video.

set -euo pipefail

case "${1:-}" in
	'')
		;;
	--audible)
		export YOUTA_LIVE_YOUTUBE_AUDIBLE=1
		;;
	*)
		printf 'Usage: %s [--audible]\n' "$0" >&2
		exit 2
		;;
esac

export YOUTA_RUN_LIVE_YOUTUBE_TEST=1

exec cargo test \
	--locked \
	--test live_youtube \
	--all-features \
	-- \
	--ignored \
	--exact youtube_audio_playback_advances_and_shuts_down \
	--nocapture
