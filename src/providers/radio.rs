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

use super::{
    DEFAULT_REQUEST_TIMEOUT, ProviderError, get_bounded_json, map_ureq_error, provider_agent,
};

#[path = "npr_stations_generated.rs"]
mod npr_stations_generated;

pub use npr_stations_generated::{
    NPR_STATION_QUALITY_LAST_PROBE_ATTEMPT_DATE, NPR_STATION_QUALITY_SERVICE_COUNT,
    NPR_STATION_QUERY_COUNT, NPR_STATION_SERVICE_COUNT, NPR_STATION_SNAPSHOT_DATE, NPR_STATIONS,
};

const DEFAULT_MAX_NOW_PLAYING_BYTES: usize = 16 * 1024;
const MAX_CONFIGURED_NOW_PLAYING_BYTES: usize = 64 * 1024;
const MAX_NOW_PLAYING_TEXT_BYTES: usize = 512;
const MAX_START_TIME_BYTES: usize = 32;
const MAX_TRACK_DURATION_SECONDS: u64 = 7 * 24 * 60 * 60;
const MIN_RADIO_FRANCE_REFRESH: Duration = Duration::from_mins(1);

/// Shortest interval accepted from a station's refresh advice.
pub const MIN_NOW_PLAYING_REFRESH: Duration = Duration::from_secs(15);

/// Longest interval accepted from a station's refresh advice.
pub const MAX_NOW_PLAYING_REFRESH: Duration = Duration::from_mins(10);

/// Refresh interval used when a station omits its recommendation.
pub const DEFAULT_NOW_PLAYING_REFRESH: Duration = Duration::from_mins(1);

/// How a player should interpret a radio playback URL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RadioStreamKind {
    /// The URL points directly to a continuous audio stream.
    Direct,
    /// The URL points to an M3U playlist that resolves to an audio stream.
    M3u,
    /// The URL is a stable BBC Sounds page resolved through Media Selector.
    BbcSounds,
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
    /// Uncompressed pulse-code modulation, including signed-integer PCM.
    Pcm,
    /// Vorbis audio carried in an Ogg container.
    Vorbis,
}

/// Wire format exposed by a station's optional now-playing endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RadioNowPlayingFormat {
    /// Icecast status JSON returned by BKK.FM's public relay.
    BkkFmIcecastJson,
    /// JSON returned by `4duk`'s public now-playing endpoint.
    FourDukJson,
    /// Radio.co status JSON carrying one current programme or track title.
    RadioCoStatusJson,
    /// Layered schedule JSON returned by Radio France's live-metadata API.
    RadioFranceLiveMeta,
    /// Current-program JSON returned by NPR's public station-service API.
    NprStationProgramJson,
    /// Plain text returned by Sector Radio's current-track endpoint.
    SectorRadioPlainText,
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
    max_response_bytes: usize,
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
            max_response_bytes: DEFAULT_MAX_NOW_PLAYING_BYTES,
        }
    }

    /// Creates a client with explicit timeout and response cap.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] when the timeout is zero or
    /// the response cap is outside the supported range.
    pub fn with_options(
        timeout: Duration,
        max_response_bytes: usize,
    ) -> Result<Self, ProviderError> {
        if timeout.is_zero() {
            return Err(ProviderError::InvalidRequest(
                "radio metadata timeout must be greater than zero".to_owned(),
            ));
        }
        if !(1..=MAX_CONFIGURED_NOW_PLAYING_BYTES).contains(&max_response_bytes) {
            return Err(ProviderError::InvalidRequest(format!(
                "radio metadata response limit must be between 1 and \
                 {MAX_CONFIGURED_NOW_PLAYING_BYTES} bytes"
            )));
        }
        Ok(Self {
            agent: provider_agent(timeout),
            max_response_bytes,
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
    /// malformed responses, remote error responses, or invalid remote fields.
    pub fn fetch(
        &self,
        endpoint: RadioNowPlayingEndpoint,
    ) -> Result<RadioNowPlaying, ProviderError> {
        let url = endpoint.parsed_url()?;
        self.fetch_url_at(&url, endpoint.format, unix_time_millis())
    }

    fn fetch_url_at(
        &self,
        url: &Url,
        format: RadioNowPlayingFormat,
        now_epoch_millis: u128,
    ) -> Result<RadioNowPlaying, ProviderError> {
        match format {
            RadioNowPlayingFormat::BkkFmIcecastJson => {
                let response: BkkFmIcecastResponse =
                    get_bounded_json(&self.agent, url, self.max_response_bytes)?;
                normalize_bkk_fm_response(response)
            }
            RadioNowPlayingFormat::FourDukJson => {
                let response: FourDukNowPlayingResponse =
                    get_bounded_json(&self.agent, url, self.max_response_bytes)?;
                normalize_four_duk_response(response)
            }
            RadioNowPlayingFormat::RadioCoStatusJson => {
                let response: RadioCoStatusResponse =
                    get_bounded_json(&self.agent, url, self.max_response_bytes)?;
                normalize_radio_co_response(response)
            }
            RadioNowPlayingFormat::RadioFranceLiveMeta => {
                let response: RadioFranceLiveMetaResponse =
                    get_bounded_json(&self.agent, url, self.max_response_bytes)?;
                normalize_radio_france_response(&response, unix_time())
            }
            RadioNowPlayingFormat::NprStationProgramJson => {
                let response: NprStationProgramResponse =
                    get_bounded_json(&self.agent, url, self.max_response_bytes)?;
                normalize_npr_station_program_response(response)
            }
            RadioNowPlayingFormat::SectorRadioPlainText => {
                let request_url = sector_now_playing_request_url(url, now_epoch_millis);
                let payload =
                    get_bounded_radio_text(&self.agent, &request_url, self.max_response_bytes)?;
                parse_sector_radio_payload(payload.as_bytes(), self.max_response_bytes)
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct RadioCoStatusResponse {
    status: Option<String>,
    current_track: Option<RadioCoCurrentTrack>,
}

#[derive(Debug, Deserialize)]
struct RadioCoCurrentTrack {
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NprStationProgramResponse {
    attributes: Option<NprStationProgramAttributes>,
    #[serde(default)]
    items: Vec<NprStationProgramItem>,
}

#[derive(Debug, Deserialize)]
struct NprStationProgramItem {
    attributes: Option<NprStationProgramAttributes>,
}

#[derive(Debug, Deserialize)]
struct NprStationProgramAttributes {
    name: Option<String>,
}

fn normalize_npr_station_program_response(
    response: NprStationProgramResponse,
) -> Result<RadioNowPlaying, ProviderError> {
    let programme = response
        .attributes
        .and_then(|attributes| attributes.name)
        .or_else(|| {
            response
                .items
                .into_iter()
                .filter_map(|item| item.attributes)
                .find_map(|attributes| attributes.name)
        });
    let programme = normalize_remote_text(
        programme,
        "NPR current programme",
        MAX_NOW_PLAYING_TEXT_BYTES,
    )?;
    if programme.is_none() {
        return Err(ProviderError::InvalidResponse(
            "NPR station service has no current programme".to_owned(),
        ));
    }
    Ok(RadioNowPlaying {
        kind: RadioNowPlayingKind::OnAir,
        title: None,
        artist: None,
        programme,
        station_start_time: None,
        duration: None,
        refresh_after: DEFAULT_NOW_PLAYING_REFRESH,
    })
}

fn normalize_radio_co_response(
    response: RadioCoStatusResponse,
) -> Result<RadioNowPlaying, ProviderError> {
    if response.status.as_deref() != Some("online") {
        return Err(ProviderError::InvalidResponse(
            "Radio.co station is not online".to_owned(),
        ));
    }
    let title = normalize_remote_text(
        response.current_track.and_then(|track| track.title),
        "current title",
        MAX_NOW_PLAYING_TEXT_BYTES,
    )?;
    if title.is_none() {
        return Err(ProviderError::InvalidResponse(
            "Radio.co status has no current title".to_owned(),
        ));
    }
    Ok(RadioNowPlaying {
        kind: RadioNowPlayingKind::OnAir,
        title,
        artist: None,
        programme: None,
        station_start_time: None,
        duration: None,
        refresh_after: DEFAULT_NOW_PLAYING_REFRESH,
    })
}

#[derive(Debug, Deserialize)]
struct BkkFmIcecastResponse {
    icestats: BkkFmIcecastStats,
}

#[derive(Debug, Deserialize)]
struct BkkFmIcecastStats {
    source: BkkFmIcecastSources,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum BkkFmIcecastSources {
    One(BkkFmIcecastSource),
    Many(Vec<BkkFmIcecastSource>),
}

#[derive(Debug, Deserialize)]
struct BkkFmIcecastSource {
    mount: Option<String>,
    title: Option<String>,
}

fn normalize_bkk_fm_response(
    response: BkkFmIcecastResponse,
) -> Result<RadioNowPlaying, ProviderError> {
    let sources = match response.icestats.source {
        BkkFmIcecastSources::One(source) => vec![source],
        BkkFmIcecastSources::Many(sources) => sources,
    };
    let raw = sources
        .iter()
        .find(|source| source.mount.as_deref() == Some("/bkkrelay"))
        .or_else(|| sources.iter().find(|source| source.title.is_some()))
        .and_then(|source| source.title.clone())
        .ok_or_else(|| {
            ProviderError::InvalidResponse("BKK.FM Icecast status has no current track".to_owned())
        })?;
    let (artist, title) = raw
        .split_once(" - ")
        .map_or((None, Some(raw.clone())), |(artist, title)| {
            (Some(artist.to_owned()), Some(title.to_owned()))
        });
    let title = normalize_remote_text(title, "title", MAX_NOW_PLAYING_TEXT_BYTES)?;
    let artist = normalize_remote_text(artist, "artist", MAX_NOW_PLAYING_TEXT_BYTES)?;
    if title.is_none() && artist.is_none() {
        return Err(ProviderError::InvalidResponse(
            "BKK.FM Icecast status has no current track".to_owned(),
        ));
    }

    Ok(RadioNowPlaying {
        kind: RadioNowPlayingKind::Track,
        title,
        artist,
        programme: None,
        station_start_time: None,
        duration: None,
        refresh_after: DEFAULT_NOW_PLAYING_REFRESH,
    })
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
            .as_ref()
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
    response: &RadioFranceLiveMetaResponse,
    now_epoch_seconds: u64,
) -> Result<RadioNowPlaying, ProviderError> {
    let programme_step = current_radio_france_step(response, 0);
    let segment_step = current_radio_france_step(response, 1);
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
    let advised = end_epoch_seconds.map_or(DEFAULT_NOW_PLAYING_REFRESH, |end| {
        Duration::from_secs(end.saturating_sub(now_epoch_seconds))
    });
    advised.clamp(MIN_RADIO_FRANCE_REFRESH, MAX_NOW_PLAYING_REFRESH)
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// Returns the current Unix timestamp in milliseconds for cache-busting URLs.
fn unix_time_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

/// Builds Sector Radio's request URL with one fresh `t` query parameter.
fn sector_now_playing_request_url(base_url: &Url, now_epoch_millis: u128) -> Url {
    let retained_query: Vec<(String, String)> = base_url
        .query_pairs()
        .filter(|(key, _)| key != "t")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let mut request_url = base_url.clone();
    request_url.set_query(None);
    {
        let mut query = request_url.query_pairs_mut();
        query.extend_pairs(retained_query);
        query.append_pair("t", &now_epoch_millis.to_string());
    }
    request_url
}

/// Downloads one bounded plain-text radio metadata response.
fn get_bounded_radio_text(
    agent: &ureq::Agent,
    url: &Url,
    limit: usize,
) -> Result<String, ProviderError> {
    let mut response = agent
        .get(url.as_str())
        .header("Accept", "text/plain")
        .call()
        .map_err(map_ureq_error)?;

    if response
        .body()
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(ProviderError::ResponseTooLarge { limit });
    }

    let bytes = response
        .body_mut()
        .with_config()
        .limit(u64::try_from(limit.saturating_add(1)).unwrap_or(u64::MAX))
        .read_to_vec()
        .map_err(|error| match error {
            ureq::Error::BodyExceedsLimit(_) => ProviderError::ResponseTooLarge { limit },
            other => ProviderError::Transport(other.to_string()),
        })?;
    if bytes.len() > limit {
        return Err(ProviderError::ResponseTooLarge { limit });
    }
    String::from_utf8(bytes).map_err(|error| {
        ProviderError::InvalidResponse(format!("radio metadata is not UTF-8: {error}"))
    })
}

/// Parses Sector Radio's optional feature, artist, title, and duration fields.
fn parse_sector_radio_payload(
    payload: &[u8],
    max_response_bytes: usize,
) -> Result<RadioNowPlaying, ProviderError> {
    if payload.len() > max_response_bytes {
        return Err(ProviderError::ResponseTooLarge {
            limit: max_response_bytes,
        });
    }
    let raw = std::str::from_utf8(payload)
        .map_err(|error| {
            ProviderError::InvalidResponse(format!("radio metadata is not UTF-8: {error}"))
        })?
        .trim();
    if raw.is_empty() {
        return Err(ProviderError::InvalidResponse(
            "Sector Radio metadata is empty".to_owned(),
        ));
    }

    let track = raw.split_once(" | ").map_or(raw, |(_, track)| track).trim();
    let (track, duration) = sector_track_and_duration(track)?;
    let (artist, title) = track
        .split_once(" - ")
        .map_or((None, Some(track.to_owned())), |(artist, title)| {
            (Some(artist.to_owned()), Some(title.to_owned()))
        });
    let title = normalize_remote_text(title, "title", MAX_NOW_PLAYING_TEXT_BYTES)?;
    let artist = normalize_remote_text(artist, "artist", MAX_NOW_PLAYING_TEXT_BYTES)?;
    if title.is_none() && artist.is_none() {
        return Err(ProviderError::InvalidResponse(
            "Sector Radio metadata has no track title".to_owned(),
        ));
    }

    Ok(RadioNowPlaying {
        kind: RadioNowPlayingKind::Track,
        title,
        artist,
        programme: None,
        station_start_time: None,
        duration,
        refresh_after: MIN_NOW_PLAYING_REFRESH,
    })
}

/// Removes Sector Radio's trailing `:: seconds` field when it is present.
fn sector_track_and_duration(raw: &str) -> Result<(&str, Option<Duration>), ProviderError> {
    let Some((track, duration)) = raw.rsplit_once("::") else {
        return Ok((raw.trim(), None));
    };
    let duration = duration.trim();
    if duration.is_empty() || !duration.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok((raw.trim(), None));
    }
    let seconds = duration
        .parse::<u64>()
        .ok()
        .filter(|seconds| *seconds <= MAX_TRACK_DURATION_SECONDS)
        .ok_or_else(|| {
            ProviderError::InvalidResponse(
                "Sector Radio duration is outside supported bounds".to_owned(),
            )
        })?;
    Ok((track.trim(), Some(Duration::from_secs(seconds))))
}

fn normalize_four_duk_duration(raw: &FourDukDuration) -> Result<Duration, ProviderError> {
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
fn parse_bkk_fm_payload(
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
    normalize_bkk_fm_response(response)
}

#[cfg(test)]
fn parse_radio_co_payload(
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
    normalize_radio_co_response(response)
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
    normalize_radio_france_response(&response, now_epoch_seconds)
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
    ///
    /// Missing or content-dependent rates remain [`None`]. Uncompressed PCM
    /// throughput is valid only when the stream itself carries PCM; it is not a
    /// substitute for a compressed stream's encoded network bitrate.
    pub bitrate_kbps: Option<u16>,
    /// Advertised or probed sample rate, when trustworthy.
    pub sample_rate_hz: Option<u32>,
    /// Advertised or probed audio channel count, when trustworthy.
    pub channels: Option<u8>,
    /// How the stable playback entry point is resolved.
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
        homepage: "https://www.sectorradio.com/",
        stream: "http://89.223.45.5:8000/progressive-flac",
        summary: "Lossless progressive electronic music.",
        codec: Some(RadioCodec::Flac),
        bitrate_kbps: None,
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: Some(RadioNowPlayingEndpoint {
            url: "https://www.sectorradio.com/nowplaying-progressive.txt",
            format: RadioNowPlayingFormat::SectorRadioPlainText,
        }),
    },
    RadioStationPreset {
        id: "kalx-berkeley-flac",
        name: "KALX 90.7 FM Berkeley",
        homepage: "https://kalx.berkeley.edu/",
        stream: "https://stream.kalx.berkeley.edu:8443/kalx.flac",
        summary: "Free-form college and community radio from UC Berkeley.",
        codec: Some(RadioCodec::Flac),
        bitrate_kbps: None,
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "radio-calico-flac",
        name: "Radio Calico",
        homepage: "https://www.radio-calico.com/",
        stream: "https://stream.radio-calico.com/calico",
        summary: "Ad-free eclectic rock and pop in 24-bit lossless audio.",
        codec: Some(RadioCodec::Flac),
        bitrate_kbps: None,
        sample_rate_hz: Some(48_000),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "intense-radio-flac",
        name: "Intense Radio",
        homepage: "https://www.intenseradio.net/",
        stream: "https://secure.live-streams.nl/flac.ogg",
        summary: "Dance, house, club classics, trance, and melodic techno.",
        codec: Some(RadioCodec::Flac),
        bitrate_kbps: None,
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "openbroadcast-flac",
        name: "open broadcast",
        homepage: "https://www.openbroadcast.ch/",
        stream: "http://stream.openbroadcast.ch/16bit.flac",
        summary: "User-curated Swiss community radio.",
        codec: Some(RadioCodec::Flac),
        bitrate_kbps: None,
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "radio-bergeijk-flac",
        name: "Radio Bergeijk",
        homepage: "https://www.radiobergeijk.nl/",
        stream: "https://stream.radiobergeijk.nl/listen/radio_bergeijk/flac",
        summary: "Format-free Dutch music and community programmes.",
        codec: Some(RadioCodec::Flac),
        bitrate_kbps: None,
        sample_rate_hz: Some(48_000),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "punkrockers-radio-flac",
        name: "Punkrockers Radio",
        homepage: "https://www.punkrockers-radio.de/",
        stream: "https://stream.punkrockers-radio.de:8443/prr.flac",
        summary: "DIY punk, hardcore, Oi!, ska, rockabilly, and live shows.",
        codec: Some(RadioCodec::Flac),
        bitrate_kbps: None,
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "pure-classix-radio-flac",
        name: "Pure Classix Radio",
        homepage: "https://www.pureclassix.com/",
        stream: "https://mscp4.live-streams.nl:8142/flac.ogg",
        summary: "Hits and album tracks from the 1960s, 1970s, and 1980s.",
        codec: Some(RadioCodec::Flac),
        bitrate_kbps: None,
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "radio-campus-grenoble-flac",
        name: "Radio Campus Grenoble 90.8",
        homepage: "https://campusgrenoble.org/",
        stream: "https://live.campusgrenoble.org/dab",
        summary: "French student and community radio from Grenoble.",
        codec: Some(RadioCodec::Flac),
        bitrate_kbps: None,
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "rlocale-radio-flac",
        name: "Rlocale Radio",
        homepage: "https://rlocale.fr/",
        stream: "https://rlocale.org/listen/rlocale/rlocale-flac.ogg",
        summary: "Non-profit experimental French-language community radio.",
        codec: Some(RadioCodec::Flac),
        bitrate_kbps: None,
        sample_rate_hz: Some(48_000),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "1zwolle-flac",
        name: "1Zwolle",
        homepage: "https://1zwolle.nl/",
        stream: "https://stream.and-stuff.nl:8443/live-1zwolle-flac",
        summary: "Local news, culture, and music from Zwolle, Netherlands.",
        codec: Some(RadioCodec::Flac),
        bitrate_kbps: None,
        sample_rate_hz: Some(48_000),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "4duk-radio",
        name: "4duk Radio",
        homepage: "https://4duk.ru/",
        stream: "http://radio.4duk.ru/4duk256.mp3",
        summary: "Russian funk radio with jokes",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(256),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: Some(RadioNowPlayingEndpoint {
            url: "http://www.4duk.ru/4duk/whatsPlaying.action",
            format: RadioNowPlayingFormat::FourDukJson,
        }),
    },
    RadioStationPreset {
        id: "euroradio-belarus",
        name: "Euroradio",
        homepage: "https://euroradio.fm/radio",
        stream: "http://stream.euroradio.fm:8000/euroradio2",
        summary: "Belarusian-language news, talk, and alternative music.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "radio-palitra",
        name: "Radio Palitra",
        homepage: "https://www.radiopalitra.ge/",
        stream: "https://radiostream.palitra.ge/stream.mp3",
        summary: "Georgian-language news, talk, and music.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "al-jazeera-arabic-audio",
        name: "Al Jazeera Arabic — Live Audio",
        homepage: "https://www.aljazeera.net/audio/live",
        stream: "https://live-hls-web-aja-gcp.thehlive.com/VOICE-AJA/index.m3u8",
        summary: "Arabic-language live audio from Al Jazeera Arabic.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(64),
        sample_rate_hz: Some(48_000),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "al-jazeera-english-audio",
        name: "Al Jazeera English — Live Audio",
        homepage: "https://www.aljazeera.com/audio/live",
        stream: "https://live-hls-web-aje-gcp.thehlive.com/VOICE-AJE/index.m3u8",
        summary: "English-language live audio from Al Jazeera English.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(64),
        sample_rate_hz: Some(48_000),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "radio-racyja",
        name: "Radio Racyja",
        homepage: "https://racyja.com/by/",
        stream: "https://air.racyja.com/racja128",
        summary: "Belarusian-language news, culture, regional affairs, and music.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(160),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "radio-maria-belarus",
        name: "Radio Maria Belarus",
        homepage: "https://www.radiomaria.by/",
        stream: "https://server.radiorm.by:8443/live",
        summary: "Belarusian-language Catholic talk, prayer, worship, and devotional music.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "radio-kultura-belarus",
        name: "Канал «Культура»",
        homepage: "https://radiokultura.by/",
        stream: "https://media2.datacenter.by/stream/kultura/stream",
        summary: "Belarusian-language culture, literature, classical music, and folk music.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(256),
        sample_rate_hz: Some(48_000),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "radio-stalica",
        name: "Радыё «Сталіца»",
        homepage: "https://www.tvr.by/live-broadcast/#radiolive",
        stream: "https://media2.datacenter.by/stream/stalica/stream",
        summary: "Belarusian-language rock, folk rock, news, culture, and history.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(256),
        sample_rate_hz: Some(48_000),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "vatican-radio-belarusian",
        name: "Vatican Radio — Belarusian",
        homepage: "https://www.vaticannews.va/be.html",
        stream: "https://radio.vaticannews.va/stream-be",
        summary: "Belarusian-language Catholic news, talk, prayer, and worship.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(48_000),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "kcbs-via-intchoson",
        name: "Korean Central Broadcasting Station — via intchoson",
        homepage: "https://www.intchoson.com/kcbs/",
        stream: "https://stream.intchoson.com/kcbs/index.m3u8",
        summary: "North Korean domestic radio delivered by an independent satellite relay.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(195),
        sample_rate_hz: Some(48_000),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "arirang-radio",
        name: "Arirang Radio",
        homepage: "https://www.arirang.com/radio",
        stream: "https://amdlive-ch03-ctnd-com.akamaized.net/arirang_3ch/smil:arirang_3ch.smil/playlist.m3u8",
        summary: "South Korean English-language K-pop, music, and culture.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "ebs-fm",
        name: "EBS FM",
        homepage: "https://www.ebs.co.kr/radio/home?ch=RADIO",
        stream: "https://ebsonair.ebs.co.kr/fmradiofamilypc/familypc1m/playlist.m3u8",
        summary: "South Korean educational, cultural, and humanities programming.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(64),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "easy-rock-manila",
        name: "96.3 Easy Rock Manila",
        homepage: "https://www.easyrock.com.ph/radio",
        stream: "https://azura.easyrock.com.ph/listen/easy_rock_manila/radio.mp3",
        summary: "Philippine English/Filipino easy-listening, adult-contemporary, and love songs.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "love-radio-manila",
        name: "Love Radio Manila",
        homepage: "https://www.loveradio.com.ph/radio",
        stream: "https://azura.loveradio.com.ph/listen/love_radio_manila/radio.mp3",
        summary: "Philippine Filipino/English pop, OPM, love songs, talk, and DJ programmes.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "radio-maria-philippines",
        name: "Radio Maria Philippines — managed relay",
        homepage: "https://www.radiomaria.ph/",
        stream: "http://dreamsiteradiocp.com:8028/stream",
        summary: "Philippine English/Filipino Catholic talk, prayer, teaching, and music over an external HTTP relay.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(64),
        sample_rate_hz: Some(44_100),
        channels: Some(1),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "mcot-thinking-radio",
        name: "MCOT Thinking Radio 96.5",
        homepage: "https://www.mcot.net/",
        stream: "https://live-org-01-cdn.mcot.net/radiocdn_edge/fm965.stream_aac/chunklist.m3u8",
        summary: "Thai-language knowledge, news, business, culture, and talk.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(48_000),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "thai-lanna-radio",
        name: "Thai Lanna Radio",
        homepage: "https://www.lannaradio.com/",
        stream: "https://inter.lannaradio.com/radio/8000/radio.mp3",
        summary: "Thai and Lanna luk thung, phuea chiwit, regional, indie, and pop music.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(320),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "chili-radio-thailand",
        name: "Chili Radio Thailand",
        homepage: "https://chiliradio.asia/",
        stream: "https://stream.chiliradio.app/chiliclassics",
        summary: "English-language Chiang Mai hits from the 1970s onward, plus Thai news in English.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(256),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "bkk-fm",
        name: "BKK.FM",
        homepage: "https://bkk.fm/",
        stream: "https://rsas.bkk.fm/radio",
        summary: "English-led Bangkok rock, alternative, and electronic music.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(64),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: Some(RadioNowPlayingEndpoint {
            url: "https://rsas.bkk.fm/status-json.xsl",
            format: RadioNowPlayingFormat::BkkFmIcecastJson,
        }),
    },
    RadioStationPreset {
        id: "retro-fm-russia",
        name: "Retro FM Russia",
        homepage: "https://retrofm.ru/",
        stream: "https://retroserver.streamr.ru:8043/retro256.mp3",
        summary: "Russian and international hits from the 1970s, 1980s, and 1990s.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(320),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "somafm-groove-salad",
        name: "SomaFM Groove Salad",
        homepage: "https://somafm.com/groovesalad/",
        stream: "https://somafm.com/m3u/groovesalad130.m3u",
        summary: "Ambient and downtempo grooves.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::M3u,
        now_playing: None,
    },
    RadioStationPreset {
        id: "somafm-drone-zone",
        name: "SomaFM Drone Zone",
        homepage: "https://somafm.com/dronezone/",
        stream: "https://somafm.com/m3u/dronezone130.m3u",
        summary: "Atmospheric space music and ambient textures with minimal beats.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::M3u,
        now_playing: None,
    },
    RadioStationPreset {
        id: "somafm-dark-zone",
        name: "SomaFM The Dark Zone",
        homepage: "https://somafm.com/darkzone/",
        stream: "https://somafm.com/m3u/darkzone130.m3u",
        summary: "Dark, flowing, mostly beatless ambient soundscapes.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::M3u,
        now_playing: None,
    },
    RadioStationPreset {
        id: "somafm-deep-space-one",
        name: "SomaFM Deep Space One",
        homepage: "https://somafm.com/deepspaceone/",
        stream: "https://somafm.com/m3u/deepspaceone130.m3u",
        summary: "Deep ambient electronic, experimental, and space music.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::M3u,
        now_playing: None,
    },
    RadioStationPreset {
        id: "somafm-space-station-soma",
        name: "SomaFM Space Station Soma",
        homepage: "https://somafm.com/spacestation/",
        stream: "https://somafm.com/m3u/spacestation130.m3u",
        summary: "Ambient and mid-tempo electronica for space exploration.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::M3u,
        now_playing: None,
    },
    RadioStationPreset {
        id: "catholic-fm",
        name: "Catholic.fm",
        homepage: "https://catholic.fm/",
        stream: "https://radio.catholic.fm/listen/catholic-fm-radio/radio.mp3",
        summary: "Spiritual music, prayer, and contemplative sound.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(192),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "positively-meditation",
        name: "Positively Meditation",
        homepage: "https://play.you.radio/station/1121",
        stream: "https://streaming.positivity.radio/pr/posimeditation/icecast.audio",
        summary: "Binaural, alpha-wave, and chakra-focused meditation music.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "neuroradio-meditation",
        name: "NeuroRadio — The Meditation Station",
        homepage: "https://neuroradio.uk/the-meditation-channel/",
        stream: "https://visual.shoutca.st:2020/8576/stream",
        summary: "Meditative soundscapes with Vipassana-style guided-awareness prompts.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "hearme-east-asian-meditation",
        name: "HearMe.fm — East Asian Meditation",
        homepage: "https://hearme.fm/radio/east-asian-meditation/",
        stream: "https://radio.hearme.fm:8144/stream",
        summary: "Traditional East Asian instruments and melodies for mindful meditation.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(320),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "hearme-tibetan-singing-bowls",
        name: "HearMe.fm — Tibetan Singing Bowls",
        homepage: "https://hearme.fm/radio/tibetan-singing-bowls/",
        stream: "https://radio.hearme.fm:8204/stream",
        summary: "Singing-bowl harmonics for meditation, yoga, breathwork, and sound baths.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(320),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "hare-krsna-radio",
        name: "Hare Krsna Radio",
        homepage: "https://hkradio.in/",
        stream: "https://cast5.my-control-panel.com/proxy/harekrsn/stream",
        summary: "Gaudiya Vaishnava kirtans, bhajans, sacred chants, and spiritual discourses.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "mantra-radio-eu",
        name: "Mantra Radio",
        homepage: "https://www.mantraradio.eu/",
        stream: "https://whsh4u-panel.com/proxy/gsedemag?mp=/stream",
        summary: "Hare Krishna mantras, kirtans, and Gaudiya Vaishnava devotional music.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(32),
        sample_rate_hz: Some(22_050),
        channels: Some(1),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "sikhnet-simran",
        name: "SikhNet Radio — Simran",
        homepage: "https://play.sikhnet.com/radio/simran",
        stream: "https://radio.sikhnet.com/proxy/channel4/stream_high_autodj",
        summary: "Sikh Naam Simran with meditative repetition of Waheguru by traditional and Western artists.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(96),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "birdsong-radio",
        name: "Birdsong Radio",
        homepage: "https://www.birdsong.fm/",
        stream: "https://a1.radio.co/s5c5da6a36/listen",
        summary: "Continuous woodland birdsong, changing from morning calls through twilight.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: Some(RadioNowPlayingEndpoint {
            url: "https://public.radio.co/stations/s5c5da6a36/status",
            format: RadioNowPlayingFormat::RadioCoStatusJson,
        }),
    },
    RadioStationPreset {
        id: "247-nature-radio",
        name: "24/7 Nature Radio",
        homepage: "https://www.247natureradio.com/",
        stream: "https://ec3.yesstreaming.net:3545/stream",
        summary: "Natural soundscapes including sea waves, waterfalls, forests, and rain.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(64),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "nature-radio-rain",
        name: "Nature Radio Rain",
        homepage: "https://radiosuitenetwork.torontocast.stream/nature-radio-rain/",
        stream: "https://maggie.torontocast.com:2020/stream/natureradiorain",
        summary: "Rain and nature soundscapes blended with sleep and relaxation music.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "somafm-underground-80s",
        name: "SomaFM Underground 80s",
        homepage: "https://somafm.com/u80s/",
        stream: "https://somafm.com/m3u/u80s130.m3u",
        summary: "Early-1980s synth-pop and new wave.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::M3u,
        now_playing: None,
    },
    RadioStationPreset {
        id: "ebm-radio-com",
        name: "EBM-Radio.com",
        homepage: "https://ebm-radio.com/",
        stream: "https://djstream.live/listen/ebmr/256",
        summary: "EBM, industrial, darkwave, synthpop, electro, and futurepop.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(256),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "somafm-doomed",
        name: "SomaFM Doomed",
        homepage: "https://somafm.com/doomed/",
        stream: "https://somafm.com/m3u/doomed130.m3u",
        summary: "Industrial, EBM, neofolk, and dark ambient.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::M3u,
        now_playing: None,
    },
    RadioStationPreset {
        id: "somafm-cliqhop-idm",
        name: "SomaFM Cliqhop IDM",
        homepage: "https://somafm.com/cliqhop/",
        stream: "https://somafm.com/m3u/cliqhop130.m3u",
        summary: "Intelligent dance music and experimental electronic beats.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::M3u,
        now_playing: None,
    },
    RadioStationPreset {
        id: "80s80s-ebm",
        name: "80s80s EBM",
        homepage: "https://www.80s80s.de/ebm",
        stream: "https://streams.80s80s.de/ebm/mp3-192/streams.80s80s.de/",
        summary: "1980s electronic body music and industrial dance-floor beats.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(192),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "80s80s-dark-wave",
        name: "80s80s Dark Wave",
        homepage: "https://www.80s80s.de/80s80s-dark-wave",
        stream: "https://streams.80s80s.de/darkwave/mp3-192/streams.80s80s.de/",
        summary: "Dark wave, gothic rock, and depressive post-punk from the 1980s.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(192),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "radcap-dsbm",
        name: "RADCAP — Depressive Suicidal Black Metal",
        homepage: "https://www.radcap.ru/depressiveblack.html",
        stream: "http://79.111.119.111:8000/dsbm",
        summary: "Depressive suicidal black metal and dark atmospheric metal.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(320),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "dark-star-radio",
        name: "Dark Star Radio",
        homepage: "https://darkstarradio.com/home/",
        stream: "http://s4.radio.co/s21ae5f2ee/listen",
        summary: "Goth, metal, industrial, and dark alternative music.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
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
        sample_rate_hz: Some(44_100),
        channels: Some(2),
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
        stream: "https://stream0.wfmu.org/freeform-high.aac",
        summary: "Listener-supported freeform radio.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
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
        sample_rate_hz: Some(44_100),
        channels: Some(2),
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
        id: "radiosega",
        name: "RadioSEGA",
        homepage: "https://www.radiosega.net/",
        stream: "https://icecast.radiosega.net/live",
        summary: "SEGA video-game music around the clock.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(256),
        sample_rate_hz: Some(48_000),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "cvgm-radio",
        name: "CVGM Radio",
        homepage: "https://radio.cvgm.net/",
        stream: "https://slacker.cvgm.net/cvgm192",
        summary: "Video-game, demoscene, and computer-platform music.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(192),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "kohina",
        name: "Kohina",
        homepage: "https://www.kohina.com/",
        stream: "https://kohina.duckdns.org/playlist_https.m3u",
        summary: "Original old-school game and demo music.",
        codec: Some(RadioCodec::Vorbis),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::M3u,
        now_playing: None,
    },
    RadioStationPreset {
        id: "slay-radio",
        name: "SLAY Radio",
        homepage: "https://www.slayradio.org/",
        stream: "https://www.slayradio.org/tune_in.php/128kbpsaac/slayradio.aac.128.m3u",
        summary: "C64 and Amiga game-music remixes.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::M3u,
        now_playing: None,
    },
    RadioStationPreset {
        id: "scenesat",
        name: "SceneSat",
        homepage: "https://scenesat.com/",
        stream: "https://scenesat.com/listen/normal/max.m3u",
        summary: "Demoscene and video-game music and remixes.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(320),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::M3u,
        now_playing: None,
    },
    RadioStationPreset {
        id: "nectarine-demoscene-radio",
        name: "Nectarine Demoscene Radio",
        homepage: "https://www.scenestream.net/demovibes/",
        stream: "https://nectarine.inversi0n.org/necta192.mp3",
        summary: "Demoscene and tracker music.",
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
        sample_rate_hz: Some(44_100),
        channels: Some(2),
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
        sample_rate_hz: Some(44_100),
        channels: Some(2),
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
        id: "france-musique-la-bo",
        name: "France Musique — La B.O.",
        homepage: "https://www.radiofrance.fr/francemusique",
        stream: "https://icecast.radiofrance.fr/francemusiquelabo-hifi.aac",
        summary: "Film scores and soundtrack compositions from French public radio.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(192),
        sample_rate_hz: Some(48_000),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "cinemix",
        name: "Cinemix",
        homepage: "https://www.cinemix.us/",
        stream: "https://kathy.torontocast.com:1825/stream",
        summary: "Film scores and soundtrack music.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "matts-movie-trax",
        name: "Matt's Movie Trax",
        homepage: "https://www.mattsmovietrax.com/",
        stream: "https://s8.myradiostream.com/11732/;",
        summary: "Movie music and film scores around the clock.",
        codec: Some(RadioCodec::Mp3),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
        stream_kind: RadioStreamKind::Direct,
        now_playing: None,
    },
    RadioStationPreset {
        id: "streaming-soundtracks",
        name: "StreamingSoundtracks.com",
        homepage: "https://streamingsoundtracks.com/",
        stream: "http://hi5.streamingsoundtracks.com/",
        summary: "Instrumental film, television, game, and anime scores.",
        codec: Some(RadioCodec::Aac),
        bitrate_kbps: Some(192),
        sample_rate_hz: Some(44_100),
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
        sample_rate_hz: Some(48_000),
        channels: Some(2),
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
    all_stations().find(|station| station.id == id)
}

/// Iterates over curated presets followed by generated NPR station services.
///
/// The iterator borrows only compile-time data and performs no directory
/// request, allocation, or disk access.
pub fn all_stations() -> impl Iterator<Item = RadioStationPreset> {
    STATIONS.iter().chain(NPR_STATIONS).copied()
}

/// Returns the number of radio presets compiled into this build.
#[must_use]
pub const fn station_count() -> usize {
    STATIONS.len() + NPR_STATIONS.len()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        thread,
    };

    use super::*;

    #[test]
    fn station_identifiers_are_unique_and_nonempty() {
        let mut identifiers = HashSet::new();

        for station in all_stations() {
            assert!(!station.id.is_empty());
            assert!(
                identifiers.insert(station.id),
                "duplicate station ID: {}",
                station.id
            );
        }
    }

    #[test]
    fn curated_presets_keep_complete_static_quality_shape() {
        for station in STATIONS {
            assert!(
                station.codec.is_some(),
                "{} must declare its verified codec",
                station.id
            );
            assert!(
                station.sample_rate_hz.is_some(),
                "{} must declare its verified sample rate",
                station.id
            );
            assert!(
                station.channels.is_some(),
                "{} must declare its verified channel count",
                station.id
            );
        }
    }

    #[test]
    fn curated_presets_limit_missing_bitrates_to_reviewed_allowlist() {
        let mut missing_bitrate_ids = STATIONS
            .iter()
            .filter(|station| station.bitrate_kbps.is_none())
            .map(|station| station.id)
            .collect::<Vec<_>>();
        missing_bitrate_ids.sort_unstable();

        assert_eq!(
            missing_bitrate_ids,
            [
                "1zwolle-flac",
                "intense-radio-flac",
                "kalx-berkeley-flac",
                "listen-moe",
                "openbroadcast-flac",
                "punkrockers-radio-flac",
                "pure-classix-radio-flac",
                "radio-bergeijk-flac",
                "radio-calico-flac",
                "radio-campus-grenoble-flac",
                "rlocale-radio-flac",
                "sector-radio-progressive-flac",
            ],
            "a missing nominal bitrate needs explicit review"
        );
    }

    #[test]
    fn all_homepages_and_streams_are_http_urls() {
        for station in all_stations() {
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
    fn generated_npr_snapshot_has_stable_reviewed_shape() {
        assert_eq!(NPR_STATION_SNAPSHOT_DATE, "2026-07-28");
        assert_eq!(NPR_STATION_QUERY_COUNT, 56);
        assert_eq!(NPR_STATION_SERVICE_COUNT, 504);
        assert_eq!(
            NPR_STATION_QUALITY_LAST_PROBE_ATTEMPT_DATE,
            Some("2026-07-28")
        );
        assert_eq!(NPR_STATION_QUALITY_SERVICE_COUNT, 504);
        assert_eq!(NPR_STATIONS.len(), NPR_STATION_SERVICE_COUNT);
        assert_eq!(station_count(), STATIONS.len() + NPR_STATION_SERVICE_COUNT);

        let mut missing_bitrates = 0_usize;
        let mut missing_sample_rates = 0_usize;
        let mut missing_channels = 0_usize;
        for station in NPR_STATIONS {
            assert!(station.id.starts_with("npr-"));
            assert!(station.stream.starts_with("https://"));
            let stream = station.stream_url().expect("generated NPR stream URL");
            let stream_path = stream.path().to_ascii_lowercase();
            assert!(!stream_path.ends_with(".pls"));
            assert!(!stream_path.ends_with(".m3u"));
            assert!(
                !stream
                    .host_str()
                    .is_some_and(|host| host.ends_with(".live.streamtheworld.com")),
                "rotating StreamTheWorld edge escaped canonicalization: {}",
                station.stream
            );
            for (key, _) in stream.query_pairs() {
                assert!(
                    !matches!(
                        key.to_ascii_lowercase().as_str(),
                        "_ic2"
                            | "auth"
                            | "exp"
                            | "expires"
                            | "hdnea"
                            | "hdnts"
                            | "key"
                            | "playsessionid"
                            | "policy"
                            | "sig"
                            | "signature"
                            | "token"
                            | "zt"
                    ),
                    "generated NPR stream contains transient query key {key}: {}",
                    station.stream
                );
            }
            assert!(station.codec.is_some());
            missing_bitrates += usize::from(station.bitrate_kbps.is_none());
            missing_sample_rates += usize::from(station.sample_rate_hz.is_none());
            missing_channels += usize::from(station.channels.is_none());
            assert!(
                station
                    .bitrate_kbps
                    .is_none_or(|bitrate| (1..=10_000).contains(&bitrate))
            );
            assert!(
                station
                    .sample_rate_hz
                    .is_none_or(|rate| (8_000..=384_000).contains(&rate))
            );
            assert!(
                station
                    .channels
                    .is_none_or(|channels| (1..=32).contains(&channels))
            );
            assert_eq!(
                station.now_playing.map(|endpoint| endpoint.format),
                Some(RadioNowPlayingFormat::NprStationProgramJson)
            );
        }
        assert_eq!(missing_bitrates, 10);
        assert_eq!(missing_sample_rates, 0);
        assert_eq!(missing_channels, 0);
    }

    #[test]
    fn generated_npr_snapshot_excludes_reviewed_unplayable_streams() {
        for id in [
            "npr-14d17493cc9d4529a25dbb7aace6075d",
            "npr-2113007873564808a9ebe6b9b703eb42",
            "npr-4fcf714e1b31499ba249d5ce3faa34fa",
            "npr-4fcf714e22754c91a1441615606ceda0",
            "npr-4fcf714f01814ee78fc17c5596caad37",
            "npr-4fcf714f04b8459ba00637f0c14eed2e",
            "npr-4fcf716517e64b56885f1ca4e61fb8f1",
            "npr-4fcf716611404e448141cf689bd5d18c",
            "npr-5b6ece581b8341108971883b6738740b",
            "npr-afc1acf2aacc4189a12d5f695903939b",
            "npr-b0395e7932b1497d99bc82d82031dcbc",
            "npr-e8e0fbe1d75a4f7bbbb6178d1f053def",
            "npr-f460ff220d5b46b5894720a2cbe6b7e1",
            "npr-st820",
        ] {
            assert!(
                station_by_id(id).is_none(),
                "reviewed unplayable NPR stream escaped exclusion: {id}"
            );
        }
    }

    #[test]
    fn generated_npr_snapshot_uses_a_playable_advertised_alternative() {
        let station = station_by_id("npr-ee0022d106e54ea1a8ece1cb5243c41c")
            .expect("KRCU's verified alternative should remain available");

        assert_eq!(station.name, "KRCU Public Radio — KRCU (128k mp3)");
        assert_eq!(station.stream, "https://krculive.semo.edu:8443/128k");
        assert_eq!(station.codec, Some(RadioCodec::Mp3));
        assert_eq!(station.bitrate_kbps, Some(128));
        assert_eq!(station.sample_rate_hz, Some(44_100));
        assert_eq!(station.channels, Some(2));
    }

    #[test]
    fn generated_npr_pcm_newscasts_preserve_the_probed_codec() {
        for id in [
            "npr-2ebca61667b545b99a791bd8a4f5bade",
            "npr-86ee71fe9cf34a699f47fb5b5ece982c",
        ] {
            let station = station_by_id(id).unwrap_or_else(|| panic!("missing PCM fixture: {id}"));
            assert_eq!(station.codec, Some(RadioCodec::Pcm));
            assert_eq!(station.bitrate_kbps, Some(3_072));
            assert_eq!(station.sample_rate_hz, Some(48_000));
            assert_eq!(station.channels, Some(2));
            assert!(station.stream.ends_with(".wav"));
        }
    }

    #[test]
    fn generated_npr_snapshot_keeps_representative_regions_and_services() {
        let searchable = NPR_STATIONS
            .iter()
            .map(|station| format!("{} {}", station.name, station.summary))
            .collect::<Vec<_>>()
            .join("\n");

        for expected in [
            "KQED",
            "WNYC FM",
            "New Sounds",
            "Anchorage, AK",
            "Honolulu, HI",
            "KPRG",
            "Charlotte Amalie, VI",
        ] {
            assert!(
                searchable.contains(expected),
                "NPR snapshot should contain {expected}"
            );
        }
    }

    #[test]
    fn npr_current_program_accepts_direct_and_proxy_response_shapes() {
        for payload in [
            br#"{"attributes":{"name":"Fresh Air"},"items":[]}"#.as_slice(),
            br#"{"attributes":{},"items":[{"attributes":{"name":"All Things Considered"}}]}"#
                .as_slice(),
        ] {
            let response: NprStationProgramResponse = serde_json::from_slice(payload).unwrap();
            let metadata = normalize_npr_station_program_response(response).unwrap();

            assert_eq!(metadata.kind, RadioNowPlayingKind::OnAir);
            assert!(metadata.programme.is_some());
            assert_eq!(metadata.refresh_after, DEFAULT_NOW_PLAYING_REFRESH);
        }
    }

    #[test]
    fn npr_current_program_rejects_empty_schedule_payload() {
        let response: NprStationProgramResponse =
            serde_json::from_slice(br#"{"attributes":null,"items":[]}"#).unwrap();

        assert!(normalize_npr_station_program_response(response).is_err());
    }

    #[test]
    fn requested_sector_and_4duk_streams_are_exact() {
        let sector = station_by_id("sector-radio-progressive-flac")
            .expect("Sector Radio preset should exist");
        let four_duk = station_by_id("4duk-radio").expect("4duk preset should exist");

        assert_eq!(sector.homepage, "https://www.sectorradio.com/");
        assert_eq!(sector.stream, "http://89.223.45.5:8000/progressive-flac");
        assert_eq!(sector.codec, Some(RadioCodec::Flac));
        assert_eq!(sector.bitrate_kbps, None);
        assert_eq!(sector.sample_rate_hz, Some(44_100));
        assert_eq!(sector.channels, Some(2));
        assert_eq!(
            sector.now_playing,
            Some(RadioNowPlayingEndpoint {
                url: "https://www.sectorradio.com/nowplaying-progressive.txt",
                format: RadioNowPlayingFormat::SectorRadioPlainText,
            })
        );
        assert_eq!(four_duk.homepage, "https://4duk.ru/");
        assert_eq!(four_duk.stream, "http://radio.4duk.ru/4duk256.mp3");
        assert_eq!(four_duk.summary, "Russian funk radio with jokes");
        assert_eq!(four_duk.bitrate_kbps, Some(256));
        assert_eq!(four_duk.sample_rate_hz, Some(44_100));
        assert_eq!(four_duk.channels, Some(2));
        assert_eq!(
            four_duk
                .now_playing
                .expect("4duk metadata endpoint should exist")
                .url,
            "http://www.4duk.ru/4duk/whatsPlaying.action"
        );
    }

    #[test]
    fn requested_lossless_catalogue_has_ten_distinct_verified_flac_stations() {
        let expected = [
            (
                "kalx-berkeley-flac",
                "https://kalx.berkeley.edu/",
                "https://stream.kalx.berkeley.edu:8443/kalx.flac",
                None,
                Some(44_100),
            ),
            (
                "radio-calico-flac",
                "https://www.radio-calico.com/",
                "https://stream.radio-calico.com/calico",
                None,
                Some(48_000),
            ),
            (
                "intense-radio-flac",
                "https://www.intenseradio.net/",
                "https://secure.live-streams.nl/flac.ogg",
                None,
                Some(44_100),
            ),
            (
                "openbroadcast-flac",
                "https://www.openbroadcast.ch/",
                "http://stream.openbroadcast.ch/16bit.flac",
                None,
                Some(44_100),
            ),
            (
                "radio-bergeijk-flac",
                "https://www.radiobergeijk.nl/",
                "https://stream.radiobergeijk.nl/listen/radio_bergeijk/flac",
                None,
                Some(48_000),
            ),
            (
                "punkrockers-radio-flac",
                "https://www.punkrockers-radio.de/",
                "https://stream.punkrockers-radio.de:8443/prr.flac",
                None,
                Some(44_100),
            ),
            (
                "pure-classix-radio-flac",
                "https://www.pureclassix.com/",
                "https://mscp4.live-streams.nl:8142/flac.ogg",
                None,
                Some(44_100),
            ),
            (
                "radio-campus-grenoble-flac",
                "https://campusgrenoble.org/",
                "https://live.campusgrenoble.org/dab",
                None,
                Some(44_100),
            ),
            (
                "rlocale-radio-flac",
                "https://rlocale.fr/",
                "https://rlocale.org/listen/rlocale/rlocale-flac.ogg",
                None,
                Some(48_000),
            ),
            (
                "1zwolle-flac",
                "https://1zwolle.nl/",
                "https://stream.and-stuff.nl:8443/live-1zwolle-flac",
                None,
                Some(48_000),
            ),
        ];
        let mut homepages = HashSet::new();

        assert_eq!(expected.len(), 10);
        for (id, homepage, stream, bitrate_kbps, sample_rate_hz) in expected {
            let station = station_by_id(id).expect("requested FLAC station should exist");

            assert_eq!(station.homepage, homepage);
            assert_eq!(station.stream, stream);
            assert_eq!(station.codec, Some(RadioCodec::Flac));
            assert_eq!(station.bitrate_kbps, bitrate_kbps);
            assert_eq!(station.sample_rate_hz, sample_rate_hz);
            assert_eq!(station.channels, Some(2));
            assert_eq!(station.stream_kind, RadioStreamKind::Direct);
            assert!(
                homepages.insert(station.homepage),
                "FLAC presets should represent distinct broadcasters"
            );
        }
    }

    #[test]
    fn film_soundtrack_presets_keep_live_probed_stream_quality() {
        let expected = [
            (
                "france-musique-la-bo",
                "https://icecast.radiofrance.fr/francemusiquelabo-hifi.aac",
                RadioCodec::Aac,
                192,
                48_000,
                RadioStreamKind::Direct,
            ),
            (
                "cinemix",
                "https://kathy.torontocast.com:1825/stream",
                RadioCodec::Mp3,
                128,
                44_100,
                RadioStreamKind::Direct,
            ),
            (
                "matts-movie-trax",
                "https://s8.myradiostream.com/11732/;",
                RadioCodec::Mp3,
                128,
                44_100,
                RadioStreamKind::Direct,
            ),
            (
                "streaming-soundtracks",
                "http://hi5.streamingsoundtracks.com/",
                RadioCodec::Aac,
                192,
                44_100,
                RadioStreamKind::Direct,
            ),
        ];

        for (id, stream, codec, bitrate_kbps, sample_rate_hz, stream_kind) in expected {
            let station =
                station_by_id(id).unwrap_or_else(|| panic!("missing film-radio preset: {id}"));
            assert_eq!(station.stream, stream);
            assert_eq!(station.codec, Some(codec));
            assert_eq!(station.bitrate_kbps, Some(bitrate_kbps));
            assert_eq!(station.sample_rate_hz, Some(sample_rate_hz));
            assert_eq!(station.channels, Some(2));
            assert_eq!(station.stream_kind, stream_kind);
            assert!(
                station.summary.to_ascii_lowercase().contains("film")
                    || station.summary.to_ascii_lowercase().contains("movie")
            );
        }
    }

    #[test]
    fn korean_presets_disclose_region_and_relay_provenance() {
        let kcbs = station_by_id("kcbs-via-intchoson").expect("KCBS relay preset");
        assert_eq!(kcbs.stream, "https://stream.intchoson.com/kcbs/index.m3u8");
        assert!(kcbs.name.contains("via intchoson"));
        assert!(kcbs.summary.contains("independent satellite relay"));
        assert_eq!(kcbs.codec, Some(RadioCodec::Aac));
        assert_eq!(kcbs.bitrate_kbps, Some(195));
        assert_eq!(kcbs.sample_rate_hz, Some(48_000));
        assert_eq!(kcbs.channels, Some(2));

        for (id, bitrate_kbps) in [("arirang-radio", 128), ("ebs-fm", 64)] {
            let station =
                station_by_id(id).unwrap_or_else(|| panic!("missing South Korean preset: {id}"));
            assert!(station.summary.starts_with("South Korean"));
            assert_eq!(station.codec, Some(RadioCodec::Aac));
            assert_eq!(station.bitrate_kbps, Some(bitrate_kbps));
            assert_eq!(station.sample_rate_hz, Some(44_100));
            assert_eq!(station.channels, Some(2));
        }
    }

    #[test]
    fn philippine_and_thai_presets_keep_validated_stream_quality() {
        let expected = [
            (
                "easy-rock-manila",
                RadioCodec::Mp3,
                128,
                44_100,
                2,
                "Philippine",
            ),
            (
                "love-radio-manila",
                RadioCodec::Mp3,
                128,
                44_100,
                2,
                "Philippine",
            ),
            (
                "radio-maria-philippines",
                RadioCodec::Mp3,
                64,
                44_100,
                1,
                "Philippine",
            ),
            (
                "mcot-thinking-radio",
                RadioCodec::Aac,
                128,
                48_000,
                2,
                "Thai",
            ),
            ("thai-lanna-radio", RadioCodec::Mp3, 320, 44_100, 2, "Thai"),
            (
                "chili-radio-thailand",
                RadioCodec::Mp3,
                256,
                44_100,
                2,
                "English-language Chiang Mai",
            ),
            ("bkk-fm", RadioCodec::Aac, 64, 44_100, 2, "Bangkok"),
        ];

        for (id, codec, bitrate_kbps, sample_rate_hz, channels, region) in expected {
            let station =
                station_by_id(id).unwrap_or_else(|| panic!("missing regional preset: {id}"));
            assert_eq!(station.codec, Some(codec));
            assert_eq!(station.bitrate_kbps, Some(bitrate_kbps));
            assert_eq!(station.sample_rate_hz, Some(sample_rate_hz));
            assert_eq!(station.channels, Some(channels));
            assert_eq!(station.stream_kind, RadioStreamKind::Direct);
            assert!(station.summary.contains(region));
        }

        let radio_maria =
            station_by_id("radio-maria-philippines").expect("Radio Maria Philippines preset");
        assert!(radio_maria.name.contains("managed relay"));
        assert!(radio_maria.summary.contains("external HTTP relay"));
        let bkk = station_by_id("bkk-fm").expect("BKK.FM preset");
        assert_eq!(
            bkk.now_playing,
            Some(RadioNowPlayingEndpoint {
                url: "https://rsas.bkk.fm/status-json.xsl",
                format: RadioNowPlayingFormat::BkkFmIcecastJson,
            })
        );
    }

    #[test]
    fn meditation_presets_are_distinct_and_keep_live_probed_quality() {
        let expected = [
            (
                "positively-meditation",
                "https://streaming.positivity.radio/pr/posimeditation/icecast.audio",
                RadioCodec::Mp3,
                128,
                "Binaural",
            ),
            (
                "neuroradio-meditation",
                "https://visual.shoutca.st:2020/8576/stream",
                RadioCodec::Aac,
                128,
                "Vipassana",
            ),
            (
                "hearme-east-asian-meditation",
                "https://radio.hearme.fm:8144/stream",
                RadioCodec::Mp3,
                320,
                "East Asian",
            ),
            (
                "hearme-tibetan-singing-bowls",
                "https://radio.hearme.fm:8204/stream",
                RadioCodec::Mp3,
                320,
                "Singing-bowl",
            ),
        ];

        for (id, stream, codec, bitrate_kbps, distinguishing_text) in expected {
            let station =
                station_by_id(id).unwrap_or_else(|| panic!("missing meditation preset: {id}"));
            assert_eq!(station.stream, stream);
            assert_eq!(station.codec, Some(codec));
            assert_eq!(station.bitrate_kbps, Some(bitrate_kbps));
            assert_eq!(station.sample_rate_hz, Some(44_100));
            assert_eq!(station.channels, Some(2));
            assert_eq!(station.stream_kind, RadioStreamKind::Direct);
            assert!(station.summary.contains(distinguishing_text));
        }
    }

    #[test]
    fn mantra_and_devotional_presets_keep_live_probed_quality() {
        let expected = [
            (
                "hare-krsna-radio",
                RadioCodec::Mp3,
                128,
                44_100,
                2,
                "kirtans",
            ),
            ("mantra-radio-eu", RadioCodec::Mp3, 32, 22_050, 1, "mantras"),
            ("sikhnet-simran", RadioCodec::Mp3, 96, 44_100, 2, "Simran"),
        ];

        for (id, codec, bitrate_kbps, sample_rate_hz, channels, content) in expected {
            let station =
                station_by_id(id).unwrap_or_else(|| panic!("missing mantra preset: {id}"));
            assert_eq!(station.codec, Some(codec));
            assert_eq!(station.bitrate_kbps, Some(bitrate_kbps));
            assert_eq!(station.sample_rate_hz, Some(sample_rate_hz));
            assert_eq!(station.channels, Some(channels));
            assert_eq!(station.stream_kind, RadioStreamKind::Direct);
            assert!(station.summary.contains(content));
        }
    }

    #[test]
    fn nature_sound_presets_keep_live_probed_quality_and_content_caveats() {
        let expected = [
            (
                "birdsong-radio",
                "https://a1.radio.co/s5c5da6a36/listen",
                128,
                "woodland birdsong",
            ),
            (
                "247-nature-radio",
                "https://ec3.yesstreaming.net:3545/stream",
                64,
                "sea waves",
            ),
            (
                "nature-radio-rain",
                "https://maggie.torontocast.com:2020/stream/natureradiorain",
                128,
                "relaxation music",
            ),
        ];

        for (id, stream, bitrate_kbps, content) in expected {
            let station =
                station_by_id(id).unwrap_or_else(|| panic!("missing nature-sound preset: {id}"));
            assert_eq!(station.stream, stream);
            assert_eq!(station.codec, Some(RadioCodec::Mp3));
            assert_eq!(station.bitrate_kbps, Some(bitrate_kbps));
            assert_eq!(station.sample_rate_hz, Some(44_100));
            assert_eq!(station.channels, Some(2));
            assert_eq!(station.stream_kind, RadioStreamKind::Direct);
            assert!(station.summary.contains(content));
        }

        assert_eq!(
            station_by_id("birdsong-radio")
                .expect("Birdsong Radio preset")
                .now_playing,
            Some(RadioNowPlayingEndpoint {
                url: "https://public.radio.co/stations/s5c5da6a36/status",
                format: RadioNowPlayingFormat::RadioCoStatusJson,
            })
        );
    }

    #[test]
    fn researched_public_presets_keep_published_entry_points() {
        let expected = [
            (
                "somafm-groove-salad",
                "https://somafm.com/m3u/groovesalad130.m3u",
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
            (
                "wfmu-freeform",
                "https://stream0.wfmu.org/freeform-high.aac",
            ),
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
    fn newly_probed_presets_keep_sample_rates_and_channel_counts() {
        let expected = [
            ("radio-bergeijk-flac", 48_000),
            ("4duk-radio", 44_100),
            ("kexp", 44_100),
            ("radio-paradise-main-mix", 44_100),
            ("animeradio-de", 44_100),
            ("anison-fm", 44_100),
            ("fip", 48_000),
        ];

        for (id, sample_rate_hz) in expected {
            let station =
                station_by_id(id).unwrap_or_else(|| panic!("missing probed station: {id}"));
            assert_eq!(station.sample_rate_hz, Some(sample_rate_hz));
            assert_eq!(station.channels, Some(2));
        }
    }

    #[test]
    fn published_aac_128_alternatives_remain_preferred() {
        let expected = [
            (
                "euroradio-belarus",
                "http://stream.euroradio.fm:8000/euroradio2",
                RadioStreamKind::Direct,
            ),
            (
                "somafm-groove-salad",
                "https://somafm.com/m3u/groovesalad130.m3u",
                RadioStreamKind::M3u,
            ),
            (
                "somafm-drone-zone",
                "https://somafm.com/m3u/dronezone130.m3u",
                RadioStreamKind::M3u,
            ),
            (
                "somafm-dark-zone",
                "https://somafm.com/m3u/darkzone130.m3u",
                RadioStreamKind::M3u,
            ),
            (
                "somafm-deep-space-one",
                "https://somafm.com/m3u/deepspaceone130.m3u",
                RadioStreamKind::M3u,
            ),
            (
                "somafm-space-station-soma",
                "https://somafm.com/m3u/spacestation130.m3u",
                RadioStreamKind::M3u,
            ),
            (
                "somafm-underground-80s",
                "https://somafm.com/m3u/u80s130.m3u",
                RadioStreamKind::M3u,
            ),
            (
                "somafm-doomed",
                "https://somafm.com/m3u/doomed130.m3u",
                RadioStreamKind::M3u,
            ),
            (
                "somafm-cliqhop-idm",
                "https://somafm.com/m3u/cliqhop130.m3u",
                RadioStreamKind::M3u,
            ),
            (
                "wfmu-freeform",
                "https://stream0.wfmu.org/freeform-high.aac",
                RadioStreamKind::Direct,
            ),
            (
                "slay-radio",
                "https://www.slayradio.org/tune_in.php/128kbpsaac/slayradio.aac.128.m3u",
                RadioStreamKind::M3u,
            ),
        ];

        for (id, stream, stream_kind) in expected {
            let station = station_by_id(id).unwrap_or_else(|| {
                panic!("missing station with a published AAC alternative: {id}")
            });
            assert_eq!(station.stream, stream);
            assert_eq!(station.codec, Some(RadioCodec::Aac));
            assert_eq!(station.bitrate_kbps, Some(128));
            assert_eq!(station.sample_rate_hz, Some(44_100));
            assert_eq!(station.channels, Some(2));
            assert_eq!(station.stream_kind, stream_kind);
        }
    }

    #[test]
    fn al_jazeera_language_services_keep_separate_official_hls_audio() {
        let expected = [
            (
                "al-jazeera-arabic-audio",
                "Al Jazeera Arabic — Live Audio",
                "https://www.aljazeera.net/audio/live",
                "https://live-hls-web-aja-gcp.thehlive.com/VOICE-AJA/index.m3u8",
                "Arabic-language",
            ),
            (
                "al-jazeera-english-audio",
                "Al Jazeera English — Live Audio",
                "https://www.aljazeera.com/audio/live",
                "https://live-hls-web-aje-gcp.thehlive.com/VOICE-AJE/index.m3u8",
                "English-language",
            ),
        ];

        for (id, name, homepage, stream, language) in expected {
            let station = station_by_id(id)
                .unwrap_or_else(|| panic!("missing official Al Jazeera audio preset: {id}"));
            assert_eq!(station.name, name);
            assert_eq!(station.homepage, homepage);
            assert_eq!(station.stream, stream);
            assert_eq!(station.codec, Some(RadioCodec::Aac));
            assert_eq!(station.bitrate_kbps, Some(64));
            assert_eq!(station.sample_rate_hz, Some(48_000));
            assert_eq!(station.channels, Some(2));
            assert_eq!(station.stream_kind, RadioStreamKind::Direct);
            assert_eq!(station.now_playing, None);
            assert!(station.summary.starts_with(language));
        }
    }

    #[test]
    fn belarusian_language_presets_keep_official_streams_and_probed_quality() {
        let expected = [
            (
                "euroradio-belarus",
                "https://euroradio.fm/radio",
                "http://stream.euroradio.fm:8000/euroradio2",
                RadioCodec::Aac,
                128,
                44_100,
                "news",
            ),
            (
                "radio-racyja",
                "https://racyja.com/by/",
                "https://air.racyja.com/racja128",
                RadioCodec::Aac,
                160,
                44_100,
                "news",
            ),
            (
                "radio-maria-belarus",
                "https://www.radiomaria.by/",
                "https://server.radiorm.by:8443/live",
                RadioCodec::Mp3,
                128,
                44_100,
                "Catholic",
            ),
            (
                "radio-kultura-belarus",
                "https://radiokultura.by/",
                "https://media2.datacenter.by/stream/kultura/stream",
                RadioCodec::Aac,
                256,
                48_000,
                "culture",
            ),
            (
                "radio-stalica",
                "https://www.tvr.by/live-broadcast/#radiolive",
                "https://media2.datacenter.by/stream/stalica/stream",
                RadioCodec::Aac,
                256,
                48_000,
                "rock",
            ),
            (
                "vatican-radio-belarusian",
                "https://www.vaticannews.va/be.html",
                "https://radio.vaticannews.va/stream-be",
                RadioCodec::Mp3,
                128,
                48_000,
                "Catholic",
            ),
        ];

        for (id, homepage, stream, codec, bitrate_kbps, sample_rate_hz, programming) in expected {
            let station =
                station_by_id(id).unwrap_or_else(|| panic!("missing Belarusian preset: {id}"));
            assert_eq!(station.homepage, homepage);
            assert_eq!(station.stream, stream);
            assert_eq!(station.codec, Some(codec));
            assert_eq!(station.bitrate_kbps, Some(bitrate_kbps));
            assert_eq!(station.sample_rate_hz, Some(sample_rate_hz));
            assert_eq!(station.channels, Some(2));
            assert_eq!(station.stream_kind, RadioStreamKind::Direct);
            assert!(station.summary.contains("Belarusian-language"));
            assert!(station.summary.contains(programming));
        }
    }

    #[test]
    fn regional_and_genre_presets_keep_verified_stream_quality() {
        let expected = [
            (
                "euroradio-belarus",
                "https://euroradio.fm/radio",
                "http://stream.euroradio.fm:8000/euroradio2",
                RadioCodec::Aac,
                128,
                Some(44_100),
                Some(2),
                RadioStreamKind::Direct,
                "Belarusian",
            ),
            (
                "radio-palitra",
                "https://www.radiopalitra.ge/",
                "https://radiostream.palitra.ge/stream.mp3",
                RadioCodec::Mp3,
                128,
                Some(44_100),
                Some(2),
                RadioStreamKind::Direct,
                "Georgian",
            ),
            (
                "somafm-drone-zone",
                "https://somafm.com/dronezone/",
                "https://somafm.com/m3u/dronezone130.m3u",
                RadioCodec::Aac,
                128,
                Some(44_100),
                Some(2),
                RadioStreamKind::M3u,
                "ambient",
            ),
            (
                "somafm-dark-zone",
                "https://somafm.com/darkzone/",
                "https://somafm.com/m3u/darkzone130.m3u",
                RadioCodec::Aac,
                128,
                Some(44_100),
                Some(2),
                RadioStreamKind::M3u,
                "ambient",
            ),
            (
                "radiosega",
                "https://www.radiosega.net/",
                "https://icecast.radiosega.net/live",
                RadioCodec::Aac,
                256,
                Some(48_000),
                Some(2),
                RadioStreamKind::Direct,
                "video-game",
            ),
            (
                "catholic-fm",
                "https://catholic.fm/",
                "https://radio.catholic.fm/listen/catholic-fm-radio/radio.mp3",
                RadioCodec::Mp3,
                192,
                Some(44_100),
                Some(2),
                RadioStreamKind::Direct,
                "Spiritual",
            ),
            (
                "somafm-underground-80s",
                "https://somafm.com/u80s/",
                "https://somafm.com/m3u/u80s130.m3u",
                RadioCodec::Aac,
                128,
                Some(44_100),
                Some(2),
                RadioStreamKind::M3u,
                "1980",
            ),
            (
                "retro-fm-russia",
                "https://retrofm.ru/",
                "https://retroserver.streamr.ru:8043/retro256.mp3",
                RadioCodec::Mp3,
                320,
                Some(44_100),
                Some(2),
                RadioStreamKind::Direct,
                "Russian",
            ),
        ];

        for (
            id,
            homepage,
            stream,
            codec,
            bitrate_kbps,
            sample_rate_hz,
            channels,
            stream_kind,
            summary_fragment,
        ) in expected
        {
            let station =
                station_by_id(id).unwrap_or_else(|| panic!("missing requested radio preset: {id}"));
            assert_eq!(station.homepage, homepage);
            assert_eq!(station.stream, stream);
            assert_eq!(station.codec, Some(codec));
            assert_eq!(station.bitrate_kbps, Some(bitrate_kbps));
            assert_eq!(station.sample_rate_hz, sample_rate_hz);
            assert_eq!(station.channels, channels);
            assert_eq!(station.stream_kind, stream_kind);
            assert!(station.summary.contains(summary_fragment));
        }
    }

    #[test]
    fn ambient_industrial_electro_and_dark_presets_keep_verified_stream_quality() {
        let expected = [
            (
                "somafm-deep-space-one",
                "https://somafm.com/deepspaceone/",
                "https://somafm.com/m3u/deepspaceone130.m3u",
                RadioCodec::Aac,
                128,
                RadioStreamKind::M3u,
                "ambient",
            ),
            (
                "somafm-space-station-soma",
                "https://somafm.com/spacestation/",
                "https://somafm.com/m3u/spacestation130.m3u",
                RadioCodec::Aac,
                128,
                RadioStreamKind::M3u,
                "Ambient",
            ),
            (
                "ebm-radio-com",
                "https://ebm-radio.com/",
                "https://djstream.live/listen/ebmr/256",
                RadioCodec::Mp3,
                256,
                RadioStreamKind::Direct,
                "industrial",
            ),
            (
                "somafm-doomed",
                "https://somafm.com/doomed/",
                "https://somafm.com/m3u/doomed130.m3u",
                RadioCodec::Aac,
                128,
                RadioStreamKind::M3u,
                "neofolk",
            ),
            (
                "somafm-cliqhop-idm",
                "https://somafm.com/cliqhop/",
                "https://somafm.com/m3u/cliqhop130.m3u",
                RadioCodec::Aac,
                128,
                RadioStreamKind::M3u,
                "electronic",
            ),
            (
                "80s80s-ebm",
                "https://www.80s80s.de/ebm",
                "https://streams.80s80s.de/ebm/mp3-192/streams.80s80s.de/",
                RadioCodec::Mp3,
                192,
                RadioStreamKind::Direct,
                "electronic body music",
            ),
            (
                "80s80s-dark-wave",
                "https://www.80s80s.de/80s80s-dark-wave",
                "https://streams.80s80s.de/darkwave/mp3-192/streams.80s80s.de/",
                RadioCodec::Mp3,
                192,
                RadioStreamKind::Direct,
                "depressive post-punk",
            ),
            (
                "radcap-dsbm",
                "https://www.radcap.ru/depressiveblack.html",
                "http://79.111.119.111:8000/dsbm",
                RadioCodec::Aac,
                320,
                RadioStreamKind::Direct,
                "Depressive suicidal black metal",
            ),
            (
                "dark-star-radio",
                "https://darkstarradio.com/home/",
                "http://s4.radio.co/s21ae5f2ee/listen",
                RadioCodec::Mp3,
                128,
                RadioStreamKind::Direct,
                "dark alternative",
            ),
        ];

        for (id, homepage, stream, codec, bitrate_kbps, stream_kind, summary_fragment) in expected {
            let station =
                station_by_id(id).unwrap_or_else(|| panic!("missing requested radio preset: {id}"));
            assert_eq!(station.homepage, homepage);
            assert_eq!(station.stream, stream);
            assert_eq!(station.codec, Some(codec));
            assert_eq!(station.bitrate_kbps, Some(bitrate_kbps));
            assert_eq!(station.sample_rate_hz, Some(44_100));
            assert_eq!(station.channels, Some(2));
            assert_eq!(station.stream_kind, stream_kind);
            assert_eq!(station.now_playing, None);
            assert!(station.summary.contains(summary_fragment));
        }

        for id in ["radcap-dsbm", "dark-star-radio"] {
            let station = station_by_id(id)
                .unwrap_or_else(|| panic!("missing intentional HTTP preset: {id}"));
            assert_eq!(
                station
                    .stream_url()
                    .expect("intentional HTTP stream URL should parse")
                    .scheme(),
                "http",
                "{id} must retain the station-published HTTP endpoint"
            );
        }
    }

    #[test]
    fn mod_tracker_chiptune_and_game_presets_keep_verified_stream_quality() {
        let expected = [
            (
                "cvgm-radio",
                "CVGM Radio",
                "https://radio.cvgm.net/",
                "https://slacker.cvgm.net/cvgm192",
                RadioCodec::Mp3,
                192,
                RadioStreamKind::Direct,
                "Video-game, demoscene, and computer-platform music.",
            ),
            (
                "kohina",
                "Kohina",
                "https://www.kohina.com/",
                "https://kohina.duckdns.org/playlist_https.m3u",
                RadioCodec::Vorbis,
                128,
                RadioStreamKind::M3u,
                "Original old-school game and demo music.",
            ),
            (
                "slay-radio",
                "SLAY Radio",
                "https://www.slayradio.org/",
                "https://www.slayradio.org/tune_in.php/128kbpsaac/slayradio.aac.128.m3u",
                RadioCodec::Aac,
                128,
                RadioStreamKind::M3u,
                "C64 and Amiga game-music remixes.",
            ),
            (
                "scenesat",
                "SceneSat",
                "https://scenesat.com/",
                "https://scenesat.com/listen/normal/max.m3u",
                RadioCodec::Mp3,
                320,
                RadioStreamKind::M3u,
                "Demoscene and video-game music and remixes.",
            ),
            (
                "nectarine-demoscene-radio",
                "Nectarine Demoscene Radio",
                "https://www.scenestream.net/demovibes/",
                "https://nectarine.inversi0n.org/necta192.mp3",
                RadioCodec::Mp3,
                192,
                RadioStreamKind::Direct,
                "Demoscene and tracker music.",
            ),
        ];

        for (id, name, homepage, stream, codec, bitrate_kbps, stream_kind, summary) in expected {
            let station =
                station_by_id(id).unwrap_or_else(|| panic!("missing game-radio preset: {id}"));
            assert_eq!(station.name, name);
            assert_eq!(station.homepage, homepage);
            assert_eq!(station.stream, stream);
            assert_eq!(station.summary, summary);
            assert_eq!(station.codec, Some(codec));
            assert_eq!(station.bitrate_kbps, Some(bitrate_kbps));
            assert_eq!(station.sample_rate_hz, Some(44_100));
            assert_eq!(station.channels, Some(2));
            assert_eq!(station.stream_kind, stream_kind);
            assert_eq!(station.now_playing, None);
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
            assert_eq!(
                station.sample_rate_hz,
                Some(if id == "listen-moe" { 48_000 } else { 44_100 })
            );
            assert_eq!(station.channels, Some(2));
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
    fn bkk_fm_status_selects_the_main_mount_and_splits_artist_and_title() {
        let payload = br#"{
            "icestats": {
                "source": [
                    {"mount":"/95txrelay","title":"Other Artist - Other Track"},
                    {"mount":"/bkkrelay","title":"The Script - No Good In Goodbye"}
                ]
            }
        }"#;

        assert_eq!(
            parse_bkk_fm_payload(payload, DEFAULT_MAX_NOW_PLAYING_BYTES)
                .expect("BKK.FM Icecast fixture should parse"),
            RadioNowPlaying {
                kind: RadioNowPlayingKind::Track,
                title: Some("No Good In Goodbye".to_owned()),
                artist: Some("The Script".to_owned()),
                programme: None,
                station_start_time: None,
                duration: None,
                refresh_after: DEFAULT_NOW_PLAYING_REFRESH,
            }
        );
    }

    #[test]
    fn bkk_fm_status_rejects_empty_malformed_and_oversized_payloads() {
        let empty = br#"{"icestats":{"source":[{"mount":"/bkkrelay","title":" "}]}}"#;
        assert!(matches!(
            parse_bkk_fm_payload(empty, DEFAULT_MAX_NOW_PLAYING_BYTES),
            Err(ProviderError::InvalidResponse(_))
        ));
        assert!(matches!(
            parse_bkk_fm_payload(b"{", DEFAULT_MAX_NOW_PLAYING_BYTES),
            Err(ProviderError::InvalidResponse(_))
        ));
        assert!(matches!(
            parse_bkk_fm_payload(b"123456789", 8),
            Err(ProviderError::ResponseTooLarge { limit: 8 })
        ));
    }

    #[test]
    fn radio_co_status_preserves_the_current_birdsong_programme() {
        let payload = br#"{
            "status":"online",
            "current_track":{
                "title":"Drift off to the woodland night - Twilight Songs"
            }
        }"#;

        assert_eq!(
            parse_radio_co_payload(payload, DEFAULT_MAX_NOW_PLAYING_BYTES)
                .expect("Radio.co fixture should parse"),
            RadioNowPlaying {
                kind: RadioNowPlayingKind::OnAir,
                title: Some("Drift off to the woodland night - Twilight Songs".to_owned()),
                artist: None,
                programme: None,
                station_start_time: None,
                duration: None,
                refresh_after: DEFAULT_NOW_PLAYING_REFRESH,
            }
        );
    }

    #[test]
    fn radio_co_status_rejects_offline_empty_and_oversized_payloads() {
        for payload in [
            br#"{"status":"offline","current_track":{"title":"Twilight"}}"#.as_slice(),
            br#"{"status":"online","current_track":{"title":" "}}"#.as_slice(),
        ] {
            assert!(matches!(
                parse_radio_co_payload(payload, DEFAULT_MAX_NOW_PLAYING_BYTES),
                Err(ProviderError::InvalidResponse(_))
            ));
        }
        assert!(matches!(
            parse_radio_co_payload(b"123456789", 8),
            Err(ProviderError::ResponseTooLarge { limit: 8 })
        ));
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
    fn sector_radio_fixture_preserves_track_details_and_duration() {
        let payload = b"Benoit Pioulard - If i could possibly tell the difference, \
            i wouldn't care anyway, 2012 [- \xe2\x80\xa2 -] :: 353";

        assert_eq!(
            parse_sector_radio_payload(payload, DEFAULT_MAX_NOW_PLAYING_BYTES)
                .expect("verified Sector Radio payload should parse"),
            RadioNowPlaying {
                kind: RadioNowPlayingKind::Track,
                title: Some(
                    "If i could possibly tell the difference, i wouldn't care anyway, \
                     2012 [- \u{2022} -]"
                        .to_owned()
                ),
                artist: Some("Benoit Pioulard".to_owned()),
                programme: None,
                station_start_time: None,
                duration: Some(Duration::from_secs(353)),
                refresh_after: MIN_NOW_PLAYING_REFRESH,
            }
        );
    }

    #[test]
    fn sector_radio_optional_feature_prefix_and_missing_duration_are_supported() {
        let with_prefix =
            parse_sector_radio_payload(b"Hi-Res | Artist - Track [Chicago - USA] :: 198", 1024)
                .expect("feature prefix should be ignored");
        assert_eq!(with_prefix.artist.as_deref(), Some("Artist"));
        assert_eq!(with_prefix.title.as_deref(), Some("Track [Chicago - USA]"));
        assert_eq!(with_prefix.duration, Some(Duration::from_secs(198)));

        let without_duration = parse_sector_radio_payload(b"Exalot - Mixtorum, 1996", 1024)
            .expect("duration is optional");
        assert_eq!(without_duration.artist.as_deref(), Some("Exalot"));
        assert_eq!(without_duration.title.as_deref(), Some("Mixtorum, 1996"));
        assert_eq!(without_duration.duration, None);
    }

    #[test]
    fn sector_radio_rejects_empty_invalid_utf8_control_and_oversized_payloads() {
        for payload in [b"".as_slice(), b" \r\n".as_slice(), b"\xff".as_slice()] {
            assert!(matches!(
                parse_sector_radio_payload(payload, 1024),
                Err(ProviderError::InvalidResponse(_))
            ));
        }
        assert!(matches!(
            parse_sector_radio_payload(b"Artist - unsafe\ntrack", 1024),
            Err(ProviderError::InvalidResponse(_))
        ));
        assert!(matches!(
            parse_sector_radio_payload(b"123456789", 8),
            Err(ProviderError::ResponseTooLarge { limit: 8 })
        ));
        let excessive_duration = format!(
            "Artist - Track :: {}",
            MAX_TRACK_DURATION_SECONDS.saturating_add(1)
        );
        assert!(matches!(
            parse_sector_radio_payload(excessive_duration.as_bytes(), 1024),
            Err(ProviderError::InvalidResponse(_))
        ));
    }

    #[test]
    fn sector_radio_request_replaces_cache_buster_and_preserves_other_query_pairs() {
        let base = Url::parse("https://example.test/now.txt?station=progressive&t=old")
            .expect("fixture URL");

        assert_eq!(
            sector_now_playing_request_url(&base, 1_700_000_000_123).as_str(),
            "https://example.test/now.txt?station=progressive&t=1700000000123"
        );
    }

    #[test]
    fn sector_radio_client_sends_dynamic_cache_buster_and_plain_text_accept_header() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock metadata endpoint");
        let address = listener.local_addr().expect("mock endpoint address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept metadata request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone mock stream"));
            let mut lines = Vec::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read request line");
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                lines.push(line.trim_end().to_owned());
            }
            assert_eq!(
                lines.first().map(String::as_str),
                Some("GET /now.txt?station=progressive&t=1700000000123 HTTP/1.1")
            );
            assert!(
                lines
                    .iter()
                    .any(|line| line.eq_ignore_ascii_case("accept: text/plain"))
            );

            let payload = "Artist - Track :: 42";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            )
            .expect("write mock response");
            stream.flush().expect("flush mock response");
        });

        let client = RadioNowPlayingClient::with_options(Duration::from_secs(1), 1024)
            .expect("valid test client");
        let url = Url::parse(&format!(
            "http://{address}/now.txt?station=progressive&t=stale"
        ))
        .expect("mock endpoint URL");
        let metadata = client
            .fetch_url_at(
                &url,
                RadioNowPlayingFormat::SectorRadioPlainText,
                1_700_000_000_123,
            )
            .expect("mock Sector Radio response");
        server.join().expect("mock metadata server");

        assert_eq!(metadata.artist.as_deref(), Some("Artist"));
        assert_eq!(metadata.title.as_deref(), Some("Track"));
        assert_eq!(metadata.duration, Some(Duration::from_secs(42)));
    }

    #[test]
    fn sector_radio_client_bounds_streamed_responses_without_content_length() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock metadata endpoint");
        let address = listener.local_addr().expect("mock endpoint address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept metadata request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone mock stream"));
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read request line");
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                      Connection: close\r\n\r\n123456789",
                )
                .expect("write oversized mock response");
        });

        let client = RadioNowPlayingClient::with_options(Duration::from_secs(1), 8)
            .expect("valid small response cap");
        let url =
            Url::parse(&format!("http://{address}/now.txt")).expect("mock metadata endpoint URL");
        let error = client
            .fetch_url_at(&url, RadioNowPlayingFormat::SectorRadioPlainText, 1)
            .expect_err("streamed response must obey the configured cap");
        server.join().expect("mock metadata server");

        assert!(matches!(
            error,
            ProviderError::ResponseTooLarge { limit: 8 }
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
