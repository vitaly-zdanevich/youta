//! Curated, account-free internet-radio presets.
//!
//! The catalogue deliberately stores only station metadata and public playback
//! entry points. It does not fetch a directory at startup, so enabling the
//! `radio` feature adds no idle network traffic. A station's inclusion here
//! does not imply that its broadcast content is freely licensed.

use std::{
    collections::HashMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;
use url::Url;

use super::{DEFAULT_REQUEST_TIMEOUT, ProviderError, get_bounded_json, provider_agent};

const DEFAULT_MAX_NOW_PLAYING_BYTES: usize = 16 * 1024;
const MAX_CONFIGURED_NOW_PLAYING_BYTES: usize = 64 * 1024;
const MAX_NOW_PLAYING_TEXT_BYTES: usize = 512;
const MAX_START_TIME_BYTES: usize = 32;
const MAX_TRACK_DURATION_SECONDS: u64 = 7 * 24 * 60 * 60;
const MIN_RADIO_FRANCE_REFRESH: Duration = Duration::from_secs(60);

/// Shortest interval accepted from a station's refresh advice.
pub const MIN_NOW_PLAYING_REFRESH: Duration = Duration::from_secs(15);

/// Longest interval accepted from a station's refresh advice.
pub const MAX_NOW_PLAYING_REFRESH: Duration = Duration::from_secs(10 * 60);

/// Refresh interval used when a station omits its recommendation.
pub const DEFAULT_NOW_PLAYING_REFRESH: Duration = Duration::from_secs(60);

/// How a player should interpret a radio playback URL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RadioStreamKind {
    /// The URL points directly to a continuous audio stream.
    Direct,
    /// The URL points to an M3U playlist that resolves to an audio stream.
    M3u,
}

/// Audio codec advertised by a station for one preset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RadioCodec {
    /// Advanced Audio Coding, including station-specific AAC profiles.
    Aac,
    /// Free Lossless Audio Codec.
    Flac,
    /// MPEG-1/2 Audio Layer III.
    Mp3,
    /// Opus interactive audio codec.
    Opus,
}

/// Wire format exposed by a station's optional now-playing endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RadioNowPlayingFormat {
    /// JSON returned by `4duk`'s public now-playing endpoint.
    FourDukJson,
    /// Layered schedule JSON returned by Radio France's live-metadata API.
    RadioFranceLiveMeta,
}

/// Meaning of one optional radio metadata record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RadioNowPlayingKind {
    /// Artist and title metadata for a song or recording.
    Track,
    /// Programme and segment metadata for a broadcast schedule.
    OnAir,
}

/// Static descriptor for a station's optional now-playing endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RadioNowPlayingEndpoint {
    /// Public endpoint returning current-track metadata.
    pub url: &'static str,
    /// Response format used by the endpoint.
    pub format: RadioNowPlayingFormat,
}

impl RadioNowPlayingEndpoint {
    /// Parses the endpoint URL.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidResponse`] if the compile-time URL is
    /// malformed.
    pub fn parsed_url(self) -> Result<Url, ProviderError> {
        Url::parse(self.url).map_err(|error| ProviderError::InvalidResponse(error.to_string()))
    }
}

/// Bounded now-playing metadata returned by a radio station.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RadioNowPlaying {
    /// Whether the record describes a track or an on-air programme.
    pub kind: RadioNowPlayingKind,
    /// Current title, when the endpoint supplies a non-blank value.
    pub title: Option<String>,
    /// Current artist, when the endpoint supplies a non-blank value.
    pub artist: Option<String>,
    /// Current programme, when a schedule endpoint supplies one.
    pub programme: Option<String>,
    /// Station-local start time, when supplied.
    pub station_start_time: Option<String>,
    /// Advertised track duration, when supplied and valid.
    pub duration: Option<Duration>,
    /// Clamped delay before the next metadata request.
    pub refresh_after: Duration,
}

/// Reusable blocking client for optional radio now-playing endpoints.
///
/// Construct this client once in a provider worker. Reusing its HTTP agent
/// avoids repeated connection setup, while the TUI remains responsible for
/// scheduling calls no more often than [`RadioNowPlaying::refresh_after`].
#[derive(Clone)]
pub struct RadioNowPlayingClient {
    agent: ureq::Agent,
    max_json_bytes: usize,
}

impl Default for RadioNowPlayingClient {
    fn default() -> Self {
        Self::new()
    }
}

impl RadioNowPlayingClient {
    /// Creates a client with Youta's default request timeout and response cap.
    #[must_use]
    pub fn new() -> Self {
        Self {
            agent: provider_agent(DEFAULT_REQUEST_TIMEOUT),
            max_json_bytes: DEFAULT_MAX_NOW_PLAYING_BYTES,
        }
    }

    /// Creates a client with explicit timeout and response cap.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] when the timeout is zero or
    /// the response cap is outside the supported range.
    pub fn with_options(timeout: Duration, max_json_bytes: usize) -> Result<Self, ProviderError> {
        if timeout.is_zero() {
            return Err(ProviderError::InvalidRequest(
                "radio metadata timeout must be greater than zero".to_owned(),
            ));
        }
        if !(1..=MAX_CONFIGURED_NOW_PLAYING_BYTES).contains(&max_json_bytes) {
            return Err(ProviderError::InvalidRequest(format!(
                "radio metadata response limit must be between 1 and \
                 {MAX_CONFIGURED_NOW_PLAYING_BYTES} bytes"
            )));
        }
        Ok(Self {
            agent: provider_agent(timeout),
            max_json_bytes,
        })
    }

    /// Fetches current-track metadata from one preset endpoint.
    ///
    /// Remote text remains untrusted: fields are trimmed, control characters
    /// and oversized values are rejected, and refresh advice is clamped to a
    /// battery-friendly interval.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] for transport/status failures, oversized or
    /// malformed JSON, remote error responses, or invalid remote fields.
    pub fn fetch(
        &self,
        endpoint: RadioNowPlayingEndpoint,
    ) -> Result<RadioNowPlaying, ProviderError> {
        let url = endpoint.parsed_url()?;
        match endpoint.format {
            RadioNowPlayingFormat::FourDukJson => {
                let response: FourDukNowPlayingResponse =
                    get_bounded_json(&self.agent, &url, self.max_json_bytes)?;
                normalize_four_duk_response(response)
            }
            RadioNowPlayingFormat::RadioFranceLiveMeta => {
                let response: RadioFranceLiveMetaResponse =
                    get_bounded_json(&self.agent, &url, self.max_json_bytes)?;
                normalize_radio_france_response(response, unix_time())
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FourDukNowPlayingResponse {
    title: Option<String>,
    artist: Option<String>,
    start: Option<String>,
    millis_until_next_request: Option<i64>,
    duration: Option<FourDukDuration>,
    #[serde(default)]
    error: bool,
}

#[derive(Debug, Deserialize)]
struct FourDukDuration {
    minutes: u64,
    seconds: u64,
}

fn normalize_four_duk_response(
    response: FourDukNowPlayingResponse,
) -> Result<RadioNowPlaying, ProviderError> {
    if response.error {
        return Err(ProviderError::InvalidResponse(
            "4duk now-playing endpoint reported an error".to_owned(),
        ));
    }

    Ok(RadioNowPlaying {
        kind: RadioNowPlayingKind::Track,
        title: normalize_remote_text(response.title, "title", MAX_NOW_PLAYING_TEXT_BYTES)?,
        artist: normalize_remote_text(response.artist, "artist", MAX_NOW_PLAYING_TEXT_BYTES)?,
        programme: None,
        station_start_time: normalize_remote_text(
            response.start,
            "start time",
            MAX_START_TIME_BYTES,
        )?,
        duration: response
            .duration
            .map(normalize_four_duk_duration)
            .transpose()?,
        refresh_after: clamp_refresh_interval(response.millis_until_next_request),
    })
}

#[derive(Debug, Deserialize)]
struct RadioFranceLiveMetaResponse {
    steps: HashMap<String, RadioFranceStep>,
    levels: Vec<RadioFranceLevel>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RadioFranceStep {
    title: Option<String>,
    start: Option<i64>,
    end: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct RadioFranceLevel {
    items: Vec<String>,
    position: i64,
}

fn normalize_radio_france_response(
    response: RadioFranceLiveMetaResponse,
    now_epoch_seconds: u64,
) -> Result<RadioNowPlaying, ProviderError> {
    let programme_step = current_radio_france_step(&response, 0);
    let segment_step = current_radio_france_step(&response, 1);
    let programme = normalize_remote_text(
        programme_step.and_then(|step| step.title.clone()),
        "programme",
        MAX_NOW_PLAYING_TEXT_BYTES,
    )?;
    let mut title = normalize_remote_text(
        segment_step.and_then(|step| step.title.clone()),
        "segment",
        MAX_NOW_PLAYING_TEXT_BYTES,
    )?;
    if title == programme {
        title = None;
    }
    if programme.is_none() && title.is_none() {
        return Err(ProviderError::InvalidResponse(
            "Radio France live metadata has no current programme or segment".to_owned(),
        ));
    }
    let end_epoch_seconds = segment_step
        .and_then(|step| step.end)
        .or_else(|| programme_step.and_then(|step| step.end))
        .and_then(|end| u64::try_from(end).ok());

    Ok(RadioNowPlaying {
        kind: RadioNowPlayingKind::OnAir,
        title,
        artist: None,
        programme,
        station_start_time: programme_step
            .and_then(|step| step.start)
            .and_then(|start| u64::try_from(start).ok())
            .map(|start| start.to_string()),
        duration: None,
        refresh_after: radio_france_refresh_after(end_epoch_seconds, now_epoch_seconds),
    })
}

fn current_radio_france_step(
    response: &RadioFranceLiveMetaResponse,
    level_index: usize,
) -> Option<&RadioFranceStep> {
    let level = response.levels.get(level_index)?;
    let position = usize::try_from(level.position).ok()?;
    let step_id = level.items.get(position)?;
    response.steps.get(step_id)
}

fn radio_france_refresh_after(end_epoch_seconds: Option<u64>, now_epoch_seconds: u64) -> Duration {
    let advised = end_epoch_seconds
        .map(|end| Duration::from_secs(end.saturating_sub(now_epoch_seconds)))
        .unwrap_or(DEFAULT_NOW_PLAYING_REFRESH);
    advised.clamp(MIN_RADIO_FRANCE_REFRESH, MAX_NOW_PLAYING_REFRESH)
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn normalize_four_duk_duration(raw: FourDukDuration) -> Result<Duration, ProviderError> {
    if raw.seconds >= 60 {
        return Err(ProviderError::InvalidResponse(
            "4duk duration seconds must be less than 60".to_owned(),
        ));
    }
    let total_seconds = raw
        .minutes
        .checked_mul(60)
        .and_then(|minutes| minutes.checked_add(raw.seconds))
        .filter(|seconds| *seconds <= MAX_TRACK_DURATION_SECONDS)
        .ok_or_else(|| {
            ProviderError::InvalidResponse("4duk duration is outside supported bounds".to_owned())
        })?;
    Ok(Duration::from_secs(total_seconds))
}

fn normalize_remote_text(
    value: Option<String>,
    field: &str,
    max_bytes: usize,
) -> Result<Option<String>, ProviderError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > max_bytes {
        return Err(ProviderError::InvalidResponse(format!(
            "radio {field} exceeds the {max_bytes}-byte limit"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(ProviderError::InvalidResponse(format!(
            "radio {field} contains control characters"
        )));
    }
    Ok(Some(value.to_owned()))
}

fn clamp_refresh_interval(milliseconds: Option<i64>) -> Duration {
    let minimum = i64::try_from(MIN_NOW_PLAYING_REFRESH.as_millis()).unwrap_or(i64::MAX);
    let maximum = i64::try_from(MAX_NOW_PLAYING_REFRESH.as_millis()).unwrap_or(i64::MAX);
    let default = i64::try_from(DEFAULT_NOW_PLAYING_REFRESH.as_millis()).unwrap_or(maximum);
    let milliseconds = milliseconds.unwrap_or(default).clamp(minimum, maximum);
    Duration::from_millis(u64::try_from(milliseconds).unwrap_or(u64::MAX))
}

#[cfg(test)]
fn parse_four_duk_payload(
    payload: &[u8],
    max_json_bytes: usize,
) -> Result<RadioNowPlaying, ProviderError> {
    if payload.len() > max_json_bytes {
        return Err(ProviderError::ResponseTooLarge {
            limit: max_json_bytes,
        });
    }
    let response = serde_json::from_slice(payload)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    normalize_four_duk_response(response)
}

#[cfg(test)]
fn parse_radio_france_payload(
    payload: &[u8],
    max_json_bytes: usize,
    now_epoch_seconds: u64,
) -> Result<RadioNowPlaying, ProviderError> {
    if payload.len() > max_json_bytes {
        return Err(ProviderError::ResponseTooLarge {
            limit: max_json_bytes,
        });
    }
    let response = serde_json::from_slice(payload)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    normalize_radio_france_response(response, now_epoch_seconds)
}

/// Static metadata for one built-in internet-radio station.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RadioStationPreset {
    /// Stable configuration and persistence identifier.
    pub id: &'static str,
    /// Station name displayed in the source list.
    pub name: &'static str,
    /// Public station or channel homepage.
    pub homepage: &'static str,
    /// Direct audio stream or playlist entry point.
    pub stream: &'static str,
    /// Broad genre or programming summary.
    pub summary: &'static str,
    /// Audio codec advertised for this stream, when known.
    pub codec: Option<RadioCodec>,
    /// Nominal stream bitrate in kilobits per second, when known.
    pub bitrate_kbps: Option<u16>,
    /// Advertised or probed sample rate, when trustworthy.
    pub sample_rate_hz: Option<u32>,
    /// Advertised or probed audio channel count, when trustworthy.
    pub channels: Option<u8>,
    /// Whether [`Self::stream`] is direct audio or a playlist.
    pub stream_kind: RadioStreamKind,
    /// Optional endpoint for current-track metadata.
    pub now_playing: Option<RadioNowPlayingEndpoint>,
}

impl RadioStationPreset {
    /// Parses the station homepage.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidResponse`] if the compile-time URL is
    /// malformed.
    pub fn homepage_url(self) -> Result<Url, ProviderError> {
        Url::parse(self.homepage).map_err(|error| ProviderError::InvalidResponse(error.to_string()))
    }

    /// Parses the station's playback entry point.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidResponse`] if the compile-time URL is
    /// malformed.
    pub fn stream_url(self) -> Result<Url, ProviderError> {
        Url::parse(self.stream).map_err(|error| ProviderError::InvalidResponse(error.to_string()))
    }
}

/// Account-free stations bundled with Youta.
///
/// Some stations intentionally use HTTP because that is the endpoint published
/// by the station. Users should treat those streams as unauthenticated and
/// susceptible to network interception.
pub const STATIONS: &[RadioStationPreset] = &[
    RadioStationPreset {
        id: "sector-radio-progressive-flac",
        name: "Sector Radio — Progressive",
        homepage: "https://sectorradio.com/",
        stream: "http://89.223.45.5:8000/progressive-flac",
        summary: "Lossless progressive electronic music.",
        codec: Some(RadioCodec::Flac),
        bitrate_kbps: None,
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "4duk-radio",
        name: "4duk Radio",
        homepage: "https://4duk.ru/",
        stream: "http://radio.4duk.ru/4duk256.mp3",
        summary: "Community internet radio.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(256),
        sample_rate_hz: None,
        channels: None,
        stream_kind: RadioStreamKind::Direct,
        now_playing: Some(RadioNowPlayingEndpoint {
            url: "http://www.4duk.ru/4duk/whatsPlaying.action",
            format: RadioNowPlayingFormat::FourDukJson,
        }),
    },
    RadioStationPreset {
        id: "somafm-groove-salad",
        name: "SomaFM Groove Salad",
        homepage: "https://somafm.com/groovesalad/",
        stream: "https://somafm.com/m3u/groovesalad256.m3u",
        summary: "Ambient and downtempo grooves.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(256),
        sample_rate_hz: None,
        channels: None,
        stream_kind: RadioStreamKind::M3u,
        now_playing: None,
    },
    RadioStationPreset {
        id: "kexp",
        name: "KEXP",
        homepage: "https://www.kexp.org/",
        stream: "https://kexp.streamguys1.com/kexp160.aac",
        summary: "Listener-powered eclectic music from Seattle.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(160),
        sample_rate_hz: None,
        channels: None,
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "nts-1",
        name: "NTS 1",
        homepage: "https://www.nts.live/",
        stream: "https://stream-relay-geo.ntslive.net/stream?client=direct",
        summary: "Independent global music, culture, and radio.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(256),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "nts-2",
        name: "NTS 2",
        homepage: "https://www.nts.live/",
        stream: "https://stream-relay-geo.ntslive.net/stream2?client=direct",
        summary: "The second NTS channel for adventurous music.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(256),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "wfmu-freeform",
        name: "WFMU Freeform Radio",
        homepage: "https://wfmu.org/",
        stream: "http://stream0.wfmu.org/freeform-128k.mp3",
        summary: "Listener-supported freeform radio.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(128),
        sample_rate_hz: None,
        channels: None,
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "radio-paradise-main-mix",
        name: "Radio Paradise — Main Mix",
        homepage: "https://radioparadise.com/",
        stream: "http://stream.radioparadise.com/aac-320",
        summary: "Listener-supported eclectic music.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(320),
        sample_rate_hz: None,
        channels: None,
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "r-a-dio",
        name: "R/a/dio",
        homepage: "https://r-a-d.io/",
        stream: "https://r-a-d.io/main",
        summary: "Mostly anime, Japanese game, doujin, and J-pop music.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(192),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "animeradio-de",
        name: "AnimeRadio.de",
        homepage: "https://www.animeradio.de/",
        stream: "https://www.animeradio.de/streams/hd.m3u",
        summary: "J-Pop, J-Rock, and anime soundtracks.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(192),
        sample_rate_hz: None,
        channels: None,
        stream_kind: RadioStreamKind::M3u,
        now_playing: None,
    },
    RadioStationPreset {
        id: "anison-fm",
        name: "Anison.FM",
        homepage: "https://en.anison.fm/",
        stream: "https://pool.anison.fm/AniSonFM(320)",
        summary: "Anime songs and related Japanese music.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(320),
        sample_rate_hz: None,
        channels: None,
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "listen-moe",
        name: "LISTEN.moe",
        homepage: "https://listen.moe/",
        stream: "https://listen.moe/stream",
        summary: "Japanese pop music.",
        codec: Some(RadioCodec::Opus),
        bitrate_kbps: None,
        sample_rate_hz: Some(48_000),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "fip",
        name: "FIP",
        homepage: "https://www.radiofrance.fr/fip",
        stream: "https://icecast.radiofrance.fr/fip-hifi.aac?id=radiofrance",
        summary: "Eclectic music from Radio France.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(192),
        sample_rate_hz: None,
        channels: None,
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "radio-swiss-classic",
        name: "Radio Swiss Classic",
        homepage: "https://www.radioswissclassic.ch/en",
        stream: "https://stream.srg-ssr.ch/srgssr/rsc_de/aac/96",
        summary: "Classical music with German-language announcements.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(96),
        sample_rate_hz: Some(48_000),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "france-musique",
        name: "France Musique",
        homepage: "https://www.radiofrance.fr/francemusique",
        stream: "https://icecast.radiofrance.fr/francemusique-hifi.aac?id=radiofrance",
        summary: "Classical music and related programming from Radio France.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(192),
        sample_rate_hz: Some(48_000),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: Some(RadioNowPlayingEndpoint {
            url: "https://api.radiofrance.fr/livemeta/pull/4",
            format: RadioNowPlayingFormat::RadioFranceLiveMeta,
        }),
    },
    RadioStationPreset {
        id: "all-classical-radio",
        name: "All Classical Radio",
        homepage: "https://www.allclassical.org/",
        stream: "https://allclassical.streamguys1.com/ac96k",
        summary: "Listener-supported classical music from Portland.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(96),
        sample_rate_hz: Some(48_000),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "npo-klassiek",
        name: "NPO Klassiek",
        homepage: "https://www.npoklassiek.nl/",
        stream: "https://icecast.omroep.nl/radio4-bb-aac",
        summary: "Dutch public classical music radio.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(64),
        sample_rate_hz: Some(48_000),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "deutschlandfunk",
        name: "Deutschlandfunk",
        homepage: "https://www.deutschlandfunk.de/",
        stream: "https://st01.sslstream.dlf.de/dlf/01/high/aac/stream.aac?aggregator=web",
        summary: "German news, analysis, culture, and current affairs.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(192),
        sample_rate_hz: Some(48_000),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
];

/// Finds a bundled radio station by its stable identifier.
#[must_use]
pub fn station_by_id(id: &str) -> Option<RadioStationPreset> {
    STATIONS.iter().copied().find(|station| station.id == id)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn station_identifiers_are_unique_and_nonempty() {
        let mut identifiers = HashSet::new();

        for station in STATIONS {
            assert!(!station.id.is_empty());
            assert!(
                identifiers.insert(station.id),
                "duplicate station ID: {}",
                station.id
            );
        }
    }

    #[test]
    fn all_homepages_and_streams_are_http_urls() {
        for station in STATIONS {
            let homepage = station
                .homepage_url()
                .expect("built-in homepage should be a valid URL");
            let stream = station
                .stream_url()
                .expect("built-in stream should be a valid URL");

            assert!(matches!(homepage.scheme(), "http" | "https"));
            assert!(homepage.host_str().is_some());
            assert!(matches!(stream.scheme(), "http" | "https"));
            assert!(stream.host_str().is_some());
            if let Some(endpoint) = station.now_playing {
                let endpoint = endpoint
                    .parsed_url()
                    .expect("built-in metadata endpoint should be a valid URL");
                assert!(matches!(endpoint.scheme(), "http" | "https"));
                assert!(endpoint.host_str().is_some());
            }
        }
    }

    #[test]
    fn requested_sector_and_4duk_streams_are_exact() {
        let sector = station_by_id("sector-radio-progressive-flac")
            .expect("Sector Radio preset should exist");
        let four_duk = station_by_id("4duk-radio").expect("4duk preset should exist");

        assert_eq!(sector.homepage, "https://sectorradio.com/");
        assert_eq!(sector.stream, "http://89.223.45.5:8000/progressive-flac");
        assert_eq!(sector.codec, Some(RadioCodec::Flac));
        assert_eq!(sector.bitrate_kbps, None);
        assert_eq!(sector.sample_rate_hz, Some(44_100));
        assert_eq!(sector.channels, Some(2));
        assert_eq!(four_duk.homepage, "https://4duk.ru/");
        assert_eq!(four_duk.stream, "http://radio.4duk.ru/4duk256.mp3");
        assert_eq!(four_duk.bitrate_kbps, Some(256));
        assert_eq!(
            four_duk
                .now_playing
                .expect("4duk metadata endpoint should exist")
                .url,
            "http://www.4duk.ru/4duk/whatsPlaying.action"
        );
    }

    #[test]
    fn researched_public_presets_keep_published_entry_points() {
        let expected = [
            (
                "somafm-groove-salad",
                "https://somafm.com/m3u/groovesalad256.m3u",
            ),
            ("kexp", "https://kexp.streamguys1.com/kexp160.aac"),
            (
                "nts-1",
                "https://stream-relay-geo.ntslive.net/stream?client=direct",
            ),
            (
                "nts-2",
                "https://stream-relay-geo.ntslive.net/stream2?client=direct",
            ),
            ("wfmu-freeform", "http://stream0.wfmu.org/freeform-128k.mp3"),
            (
                "radio-paradise-main-mix",
                "http://stream.radioparadise.com/aac-320",
            ),
            (
                "fip",
                "https://icecast.radiofrance.fr/fip-hifi.aac?id=radiofrance",
            ),
            (
                "deutschlandfunk",
                "https://st01.sslstream.dlf.de/dlf/01/high/aac/stream.aac?aggregator=web",
            ),
            (
                "radio-swiss-classic",
                "https://stream.srg-ssr.ch/srgssr/rsc_de/aac/96",
            ),
            (
                "france-musique",
                "https://icecast.radiofrance.fr/francemusique-hifi.aac?id=radiofrance",
            ),
            (
                "all-classical-radio",
                "https://allclassical.streamguys1.com/ac96k",
            ),
            ("npo-klassiek", "https://icecast.omroep.nl/radio4-bb-aac"),
        ];

        for (id, stream) in expected {
            assert_eq!(
                station_by_id(id)
                    .unwrap_or_else(|| panic!("missing station preset: {id}"))
                    .stream,
                stream
            );
        }
    }

    #[test]
    fn japanese_music_presets_keep_official_entry_points_and_quality() {
        let expected = [
            (
                "r-a-dio",
                "https://r-a-d.io/",
                "https://r-a-d.io/main",
                RadioCodec::Mp3,
                Some(192),
                RadioStreamKind::Direct,
            ),
            (
                "animeradio-de",
                "https://www.animeradio.de/",
                "https://www.animeradio.de/streams/hd.m3u",
                RadioCodec::Mp3,
                Some(192),
                RadioStreamKind::M3u,
            ),
            (
                "anison-fm",
                "https://en.anison.fm/",
                "https://pool.anison.fm/AniSonFM(320)",
                RadioCodec::Mp3,
                Some(320),
                RadioStreamKind::Direct,
            ),
            (
                "listen-moe",
                "https://listen.moe/",
                "https://listen.moe/stream",
                RadioCodec::Opus,
                None,
                RadioStreamKind::Direct,
            ),
        ];

        for (id, homepage, stream, codec, bitrate, stream_kind) in expected {
            let station =
                station_by_id(id).unwrap_or_else(|| panic!("missing Japanese preset: {id}"));
            assert_eq!(station.homepage, homepage);
            assert_eq!(station.stream, stream);
            assert_eq!(station.codec, Some(codec));
            assert_eq!(station.bitrate_kbps, bitrate);
            assert_eq!(station.stream_kind, stream_kind);
            assert_eq!(station.now_playing, None);
        }
        assert_eq!(
            station_by_id("listen-moe")
                .expect("LISTEN.moe preset")
                .summary,
            "Japanese pop music."
        );
        let listen_moe = station_by_id("listen-moe").expect("LISTEN.moe preset");
        assert_eq!(listen_moe.sample_rate_hz, Some(48_000));
        assert_eq!(listen_moe.channels, Some(2));
    }

    #[test]
    fn classical_presets_keep_official_metadata() {
        let expected = [
            (
                "radio-swiss-classic",
                "Radio Swiss Classic",
                "https://www.radioswissclassic.ch/en",
                "https://stream.srg-ssr.ch/srgssr/rsc_de/aac/96",
                96,
            ),
            (
                "france-musique",
                "France Musique",
                "https://www.radiofrance.fr/francemusique",
                "https://icecast.radiofrance.fr/francemusique-hifi.aac?id=radiofrance",
                192,
            ),
            (
                "all-classical-radio",
                "All Classical Radio",
                "https://www.allclassical.org/",
                "https://allclassical.streamguys1.com/ac96k",
                96,
            ),
            (
                "npo-klassiek",
                "NPO Klassiek",
                "https://www.npoklassiek.nl/",
                "https://icecast.omroep.nl/radio4-bb-aac",
                64,
            ),
        ];

        for (id, name, homepage, stream, bitrate_kbps) in expected {
            let station =
                station_by_id(id).unwrap_or_else(|| panic!("missing classical preset: {id}"));
            assert_eq!(station.name, name);
            assert_eq!(station.homepage, homepage);
            assert_eq!(station.stream, stream);
            assert_eq!(station.codec, Some(RadioCodec::Aac));
            assert_eq!(station.bitrate_kbps, Some(bitrate_kbps));
            assert_eq!(station.sample_rate_hz, Some(48_000));
            assert_eq!(station.channels, Some(2));
            assert!(station.summary.to_ascii_lowercase().contains("classical"));
        }
        assert!(
            !station_by_id("deutschlandfunk")
                .expect("Deutschlandfunk preset")
                .summary
                .to_ascii_lowercase()
                .contains("classical")
        );
    }

    #[test]
    fn probed_nts_and_radio_quality_is_retained() {
        for id in ["nts-1", "nts-2"] {
            let station = station_by_id(id).expect("NTS preset");
            assert_eq!(station.codec, Some(RadioCodec::Mp3));
            assert_eq!(station.bitrate_kbps, Some(256));
            assert_eq!(station.sample_rate_hz, Some(44_100));
            assert_eq!(station.channels, Some(2));
        }
        let radio = station_by_id("r-a-dio").expect("R/a/dio preset");
        assert_eq!(radio.codec, Some(RadioCodec::Mp3));
        assert_eq!(radio.bitrate_kbps, Some(192));
        assert_eq!(radio.sample_rate_hz, Some(44_100));
        assert_eq!(radio.channels, Some(2));
    }

    #[test]
    fn france_musique_uses_the_official_typed_metadata_endpoint() {
        let endpoint = station_by_id("france-musique")
            .expect("France Musique preset")
            .now_playing
            .expect("France Musique live metadata endpoint");
        assert_eq!(endpoint.url, "https://api.radiofrance.fr/livemeta/pull/4");
        assert_eq!(endpoint.format, RadioNowPlayingFormat::RadioFranceLiveMeta);
    }

    #[test]
    fn lookup_rejects_unknown_station() {
        assert_eq!(station_by_id("not-a-station"), None);
    }

    #[test]
    fn four_duk_valid_payload_is_typed_and_bounded() {
        let payload = br#"{
            "title":"Warwick Avenue",
            "artist":"Duffy",
            "start":"22:57:27",
            "millisUntilNextRequest":48927,
            "duration":{"minutes":2,"seconds":33},
            "addableToFavorites":false,
            "error":false,
            "endOverridden":true
        }"#;

        assert_eq!(
            parse_four_duk_payload(payload, DEFAULT_MAX_NOW_PLAYING_BYTES)
                .expect("verified 4duk payload should parse"),
            RadioNowPlaying {
                kind: RadioNowPlayingKind::Track,
                title: Some("Warwick Avenue".to_owned()),
                artist: Some("Duffy".to_owned()),
                programme: None,
                station_start_time: Some("22:57:27".to_owned()),
                duration: Some(Duration::from_secs(153)),
                refresh_after: Duration::from_millis(48_927),
            }
        );
    }

    #[test]
    fn four_duk_missing_and_blank_text_is_absent() {
        let missing = br#"{"error":false}"#;
        let blank = br#"{
            "title":"  ",
            "artist":"\t",
            "start":"",
            "error":false
        }"#;

        for payload in [missing.as_slice(), blank.as_slice()] {
            let metadata = parse_four_duk_payload(payload, DEFAULT_MAX_NOW_PLAYING_BYTES)
                .expect("missing or blank optional text should be accepted");
            assert_eq!(metadata.title, None);
            assert_eq!(metadata.artist, None);
            assert_eq!(metadata.station_start_time, None);
            assert_eq!(metadata.refresh_after, DEFAULT_NOW_PLAYING_REFRESH);
        }
    }

    #[test]
    fn four_duk_error_flag_is_rejected() {
        let error = parse_four_duk_payload(
            br#"{"title":"stale","artist":"stale","error":true}"#,
            DEFAULT_MAX_NOW_PLAYING_BYTES,
        )
        .expect_err("remote error flag must not expose stale metadata");

        assert!(matches!(error, ProviderError::InvalidResponse(_)));
        assert!(error.to_string().contains("reported an error"));
    }

    #[test]
    fn four_duk_malformed_and_oversized_payloads_are_rejected() {
        assert!(matches!(
            parse_four_duk_payload(b"{", DEFAULT_MAX_NOW_PLAYING_BYTES),
            Err(ProviderError::InvalidResponse(_))
        ));
        assert!(matches!(
            parse_four_duk_payload(b"123456789", 8),
            Err(ProviderError::ResponseTooLarge { limit: 8 })
        ));

        let oversized_title = format!(
            r#"{{"title":"{}","error":false}}"#,
            "x".repeat(MAX_NOW_PLAYING_TEXT_BYTES + 1)
        );
        assert!(matches!(
            parse_four_duk_payload(oversized_title.as_bytes(), DEFAULT_MAX_NOW_PLAYING_BYTES),
            Err(ProviderError::InvalidResponse(_))
        ));
    }

    #[test]
    fn refresh_advice_is_clamped_for_battery_use() {
        assert_eq!(clamp_refresh_interval(Some(1)), MIN_NOW_PLAYING_REFRESH);
        assert_eq!(
            clamp_refresh_interval(Some(i64::MAX)),
            MAX_NOW_PLAYING_REFRESH
        );
        assert_eq!(clamp_refresh_interval(None), DEFAULT_NOW_PLAYING_REFRESH);
    }

    #[test]
    fn radio_france_fixture_selects_current_programme_and_segment() {
        let payload = br#"{
            "steps": {
                "programme": {
                    "title": "Le Concert du soir",
                    "start": 1000,
                    "end": 1900
                },
                "segment": {
                    "title": "Camilla George en direct",
                    "start": 1200,
                    "end": 1500
                },
                "later": {
                    "title": "Programme suivant",
                    "start": 1900,
                    "end": 2500
                }
            },
            "levels": [
                {"items": ["programme", "later"], "position": 0},
                {"items": ["segment"], "position": 0}
            ],
            "stationId": 4
        }"#;

        assert_eq!(
            parse_radio_france_payload(payload, DEFAULT_MAX_NOW_PLAYING_BYTES, 1300)
                .expect("bounded official fixture should parse"),
            RadioNowPlaying {
                kind: RadioNowPlayingKind::OnAir,
                title: Some("Camilla George en direct".to_owned()),
                artist: None,
                programme: Some("Le Concert du soir".to_owned()),
                station_start_time: Some("1000".to_owned()),
                duration: None,
                refresh_after: Duration::from_secs(200),
            }
        );
    }

    #[test]
    fn radio_france_refresh_uses_end_time_with_battery_bounds() {
        assert_eq!(
            radio_france_refresh_after(Some(1010), 1000),
            MIN_RADIO_FRANCE_REFRESH
        );
        assert_eq!(
            radio_france_refresh_after(Some(5000), 1000),
            MAX_NOW_PLAYING_REFRESH
        );
        assert_eq!(
            radio_france_refresh_after(None, 1000),
            DEFAULT_NOW_PLAYING_REFRESH
        );
    }

    #[test]
    fn radio_france_malformed_empty_and_oversized_payloads_are_rejected() {
        assert!(matches!(
            parse_radio_france_payload(b"{", DEFAULT_MAX_NOW_PLAYING_BYTES, 1000),
            Err(ProviderError::InvalidResponse(_))
        ));
        assert!(matches!(
            parse_radio_france_payload(b"123456789", 8, 1000),
            Err(ProviderError::ResponseTooLarge { limit: 8 })
        ));
        let empty = br#"{
            "steps": {"blank": {"title": " ", "start": 1000, "end": 1100}},
            "levels": [{"items": ["blank"], "position": 0}]
        }"#;
        assert!(matches!(
            parse_radio_france_payload(empty, DEFAULT_MAX_NOW_PLAYING_BYTES, 1000),
            Err(ProviderError::InvalidResponse(_))
        ));
    }

    #[test]
    fn metadata_client_reports_transport_failures() {
        let client = RadioNowPlayingClient::with_options(Duration::from_millis(100), 1024)
            .expect("valid test client");
        let endpoint = RadioNowPlayingEndpoint {
            url: "http://127.0.0.1:9/unreachable",
            format: RadioNowPlayingFormat::FourDukJson,
        };

        assert!(matches!(
            client.fetch(endpoint),
            Err(ProviderError::Transport(_))
        ));
    }
}
