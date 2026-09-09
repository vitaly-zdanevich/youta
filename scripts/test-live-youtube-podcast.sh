#!/usr/bin/env bash
# Run the opt-in, metadata-only large-channel podcast integration test.
# Usage: scripts/test-live-youtube-podcast.sh UC_CHANNEL_ID
# Optional: YOUTA_LIVE_PODCAST_API_KEY and YOUTA_LIVE_PODCAST_MIN_EPISODES.
# Credentials are never loaded from Youta or yt-dlp configuration files.
set -euo pipefail

if [[ ${1:-} == --help && $# == 1 ]]; then
	printf '%s\n' 'Usage: scripts/test-live-youtube-podcast.sh [UC_CHANNEL_ID]' 'Alternatively set YOUTA_LIVE_PODCAST_CHANNEL_ID.' 'Minimum episodes: YOUTA_LIVE_PODCAST_MIN_EPISODES (default 500; range 500..10000).' 'Optional API key: YOUTA_LIVE_PODCAST_API_KEY. No episode audio is downloaded.'
	exit 0
fi
if (( $# > 1 )); then
	printf '%s\n' 'Expected at most one canonical YouTube channel ID.' >&2
	exit 2
fi

channel_id=${1:-${YOUTA_LIVE_PODCAST_CHANNEL_ID:-}}
if [[ ! $channel_id =~ ^UC[A-Za-z0-9_-]{22}$ ]]; then
	printf '%s\n' 'Set YOUTA_LIVE_PODCAST_CHANNEL_ID or pass a canonical 24-character UC channel ID.' >&2
	exit 2
fi
minimum=${YOUTA_LIVE_PODCAST_MIN_EPISODES:-500}
if [[ ! $minimum =~ ^[0-9]{3,5}$ ]] || (( 10#$minimum < 500 || 10#$minimum > 10000 )); then
	printf '%s\n' 'YOUTA_LIVE_PODCAST_MIN_EPISODES must be an integer from 500 to 10000.' >&2
	exit 2
fi

# The guard above prevents accidental network tests when invoked without a channel.
export YOUTA_LIVE_PODCAST_CHANNEL_ID="$channel_id"
export YOUTA_RUN_LIVE_YOUTUBE_PODCAST_TEST=1
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
exec cargo test --locked --test live_youtube_podcast \
	--features lan-sharing,rss,youtube-official \
	-- --ignored --exact youtube_large_channel_podcast_dates_and_cache --nocapture
