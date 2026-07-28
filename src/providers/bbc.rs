//! BBC Sounds live-radio and podcast helpers.
//!
//! Live playback follows the same public, geo-aware flow as the BBC Sounds web
//! player: Youta fetches a stable station page, reads its short-lived playback
//! token, asks BBC Media Selector for the highest desktop audio profile
//! available to the current connection, and hands one HTTPS HLS or DASH
//! manifest to the playback backend. Tokens and resolved CDN URLs stay in RAM
//! for that playback action and are never persisted.
//!
//! The built-in station list mirrors the machine-readable station modules
//! embedded in BBC's public Sounds directory. Numbered event-only Sports Extra
//! services are intentionally omitted because they are not stable stations.

use std::{sync::Arc, time::Duration};

use serde::Deserialize;
use url::Url;

use super::{DEFAULT_REQUEST_TIMEOUT, ProviderError};

/// BBC's published OPML index of radio and podcast feeds.
pub const PODCAST_OPML_URL: &str = "https://www.bbc.co.uk/radio/opml/bbc_podcast_opml.opml";

/// BBC's public podcast directory.
pub const PODCAST_DIRECTORY_URL: &str = "https://www.bbc.co.uk/sounds/podcasts";

/// BBC's public, machine-readable live-station directory.
pub const STATIONS_DIRECTORY_URL: &str = "https://www.bbc.co.uk/sounds/stations";

const MEDIA_SELECTOR_BASE_URL: &str =
    "https://open.live.bbc.co.uk/mediaselector/6/select/version/3.0/";
const MEDIA_SELECTOR_MEDIASET: &str = "pc";
const DEFAULT_MAX_PAGE_BYTES: usize = 512 * 1024;
const DEFAULT_MAX_SELECTOR_BYTES: usize = 128 * 1024;
const MAX_PLAYBACK_TOKEN_BYTES: usize = 4 * 1024;
const MAX_SERVICE_ID_BYTES: usize = 128;
const MAX_REMOTE_LABEL_BYTES: usize = 128;

/// Stable grouping used by BBC's station directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BbcStationGroup {
    /// UK-wide and international BBC services.
    National,
    /// Services for Scotland, Wales, and Northern Ireland.
    Nations,
    /// English and Channel Islands local-radio services.
    Local,
}

impl BbcStationGroup {
    /// Returns a compact description suitable for the Radio details panel.
    #[must_use]
    pub const fn summary(self) -> &'static str {
        match self {
            Self::National => "BBC national or international live radio.",
            Self::Nations => "BBC nations live radio.",
            Self::Local => "BBC local live radio.",
        }
    }
}

/// One stable BBC Sounds live-station entry point.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BbcStationPreset {
    /// Stable BBC Sounds service identifier.
    pub id: &'static str,
    /// Name displayed in the station picker.
    pub name: &'static str,
    /// Directory group owning the station.
    pub group: BbcStationGroup,
    /// Stable public BBC Sounds landing page.
    pub page: &'static str,
}

impl BbcStationPreset {
    /// Parses the stable public BBC Sounds page.
    ///
    /// # Errors
    ///
    /// Returns an error only if a compile-time preset URL is invalid.
    pub fn sounds_url(self) -> Result<Url, ProviderError> {
        Url::parse(self.page).map_err(|error| ProviderError::InvalidResponse(error.to_string()))
    }
}

macro_rules! station {
    ($id:literal, $name:literal, $group:ident) => {
        BbcStationPreset {
            id: $id,
            name: $name,
            group: BbcStationGroup::$group,
            page: concat!("https://www.bbc.co.uk/sounds/play/live/", $id),
        }
    };
}

/// Stable BBC national, nations, and local live-radio services.
///
/// This list follows the public Sounds station directory. BBC Radio 5 Sports
/// Extra 2 and 3 are excluded because BBC describes them as additional event
/// streams rather than permanent stations.
pub const STATIONS: &[BbcStationPreset] = &[
    station!("bbc_radio_one", "BBC Radio 1", National),
    station!("bbc_radio_one_anthems", "BBC Radio 1 Anthems", National),
    station!("bbc_radio_one_dance", "BBC Radio 1 Dance", National),
    station!("bbc_1xtra", "BBC Radio 1Xtra", National),
    station!("bbc_radio_two", "BBC Radio 2", National),
    station!("bbc_radio_three", "BBC Radio 3", National),
    station!("bbc_radio_three_unwind", "BBC Radio 3 Unwind", National),
    station!("bbc_radio_fourfm", "BBC Radio 4", National),
    station!("bbc_radio_four_extra", "BBC Radio 4 Extra", National),
    station!("bbc_radio_five_live", "BBC Radio 5 Live", National),
    station!(
        "bbc_radio_five_live_sports_extra",
        "BBC Radio 5 Sports Extra",
        National
    ),
    station!("bbc_6music", "BBC Radio 6 Music", National),
    station!(
        "bbc_radio_six_indie_forever",
        "BBC Radio 6 Indie Forever",
        National
    ),
    station!("bbc_asian_network", "BBC Asian Network", National),
    station!("bbc_world_service", "BBC World Service", National),
    station!("bbc_sounds_news", "BBC Live News", National),
    station!("cbeebies_radio", "BBC CBeebies Radio", National),
    station!("bbc_radio_scotland_fm", "BBC Radio Scotland", Nations),
    station!("bbc_radio_scotland_mw", "BBC Radio Scotland Extra", Nations),
    station!("bbc_radio_orkney", "BBC Radio Orkney", Nations),
    station!("bbc_radio_shetland", "BBC Radio Shetland", Nations),
    station!("bbc_radio_nan_gaidheal", "BBC Radio nan Gàidheal", Nations),
    station!("bbc_radio_ulster", "BBC Radio Ulster", Nations),
    station!("bbc_radio_foyle", "BBC Radio Foyle", Nations),
    station!("bbc_radio_wales_fm", "BBC Radio Wales", Nations),
    station!("bbc_radio_wales_am", "BBC Radio Wales Extra", Nations),
    station!("bbc_radio_cymru", "BBC Radio Cymru", Nations),
    station!("bbc_radio_cymru_2", "BBC Radio Cymru 2", Nations),
    station!("bbc_radio_berkshire", "BBC Radio Berkshire", Local),
    station!("bbc_radio_bristol", "BBC Radio Bristol", Local),
    station!("bbc_radio_cambridge", "BBC Radio Cambridgeshire", Local),
    station!("bbc_radio_cornwall", "BBC Radio Cornwall", Local),
    station!("bbc_radio_coventry_warwickshire", "BBC CWR", Local),
    station!("bbc_radio_cumbria", "BBC Radio Cumbria", Local),
    station!("bbc_radio_derby", "BBC Radio Derby", Local),
    station!("bbc_radio_devon", "BBC Radio Devon", Local),
    station!("bbc_radio_essex", "BBC Essex", Local),
    station!(
        "bbc_radio_gloucestershire",
        "BBC Radio Gloucestershire",
        Local
    ),
    station!("bbc_radio_guernsey", "BBC Radio Guernsey", Local),
    station!(
        "bbc_radio_hereford_worcester",
        "BBC Hereford & Worcester",
        Local
    ),
    station!("bbc_radio_humberside", "BBC Radio Humberside", Local),
    station!("bbc_radio_jersey", "BBC Radio Jersey", Local),
    station!("bbc_radio_kent", "BBC Radio Kent", Local),
    station!("bbc_radio_lancashire", "BBC Radio Lancashire", Local),
    station!("bbc_radio_leeds", "BBC Radio Leeds", Local),
    station!("bbc_radio_leicester", "BBC Radio Leicester", Local),
    station!("bbc_radio_lincolnshire", "BBC Radio Lincolnshire", Local),
    station!("bbc_london", "BBC Radio London", Local),
    station!("bbc_radio_manchester", "BBC Radio Manchester", Local),
    station!("bbc_radio_merseyside", "BBC Radio Merseyside", Local),
    station!("bbc_radio_newcastle", "BBC Radio Newcastle", Local),
    station!("bbc_radio_norfolk", "BBC Radio Norfolk", Local),
    station!("bbc_radio_northampton", "BBC Radio Northampton", Local),
    station!("bbc_radio_nottingham", "BBC Radio Nottingham", Local),
    station!("bbc_radio_oxford", "BBC Radio Oxford", Local),
    station!("bbc_radio_sheffield", "BBC Radio Sheffield", Local),
    station!("bbc_radio_shropshire", "BBC Radio Shropshire", Local),
    station!("bbc_radio_solent", "BBC Radio Solent", Local),
    station!(
        "bbc_radio_solent_west_dorset",
        "BBC Radio Solent Dorset",
        Local
    ),
    station!("bbc_radio_somerset_sound", "BBC Radio Somerset", Local),
    station!("bbc_radio_stoke", "BBC Radio Stoke", Local),
    station!("bbc_radio_suffolk", "BBC Radio Suffolk", Local),
    station!("bbc_radio_surrey", "BBC Radio Surrey", Local),
    station!("bbc_radio_sussex", "BBC Radio Sussex", Local),
    station!("bbc_tees", "BBC Radio Tees", Local),
    station!(
        "bbc_three_counties_radio",
        "BBC Three Counties Radio",
        Local
    ),
    station!("bbc_radio_wiltshire", "BBC Radio Wiltshire", Local),
    station!("bbc_wm", "BBC Radio WM", Local),
    station!("bbc_radio_york", "BBC Radio York", Local),
];

/// Finds a built-in station by its stable BBC Sounds identifier.
#[must_use]
pub fn station_by_id(id: &str) -> Option<BbcStationPreset> {
    STATIONS.iter().copied().find(|station| station.id == id)
}

/// Resolves a public BBC Sounds live page to a built-in station.
///
/// Both the current slash form and BBC's former `live:<service>` form are
/// accepted so persisted links remain useful after a Youta upgrade. Unknown
/// service identifiers and non-BBC hosts are rejected.
#[must_use]
pub fn station_from_url(url: &Url) -> Option<BbcStationPreset> {
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !matches!(
            url.host_str(),
            Some("www.bbc.co.uk" | "bbc.co.uk" | "www.bbc.com" | "bbc.com")
        )
    {
        return None;
    }

    let path = url.path().trim_end_matches('/');
    let service_id = path
        .strip_prefix("/sounds/play/live/")
        .or_else(|| path.strip_prefix("/sounds/play/live:"))?;
    if service_id.contains('/') || service_id.is_empty() {
        return None;
    }
    station_by_id(service_id)
}

/// Audience region selected by BBC's public page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BbcAudience {
    /// The page identified the connection as inside the United Kingdom.
    UnitedKingdom,
    /// The page identified the connection as outside the United Kingdom.
    International,
}

impl BbcAudience {
    /// Returns the human-readable region label used in playback diagnostics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::UnitedKingdom => "UK",
            Self::International => "international",
        }
    }
}

/// Transfer format selected from one BBC Media Selector response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BbcTransferFormat {
    /// HTTP Live Streaming.
    Hls,
    /// MPEG-DASH.
    Dash,
}

impl BbcTransferFormat {
    /// Returns the conventional format label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Hls => "HLS",
            Self::Dash => "DASH",
        }
    }
}

/// Short-lived live-media result returned for one explicit playback action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BbcLiveResolution {
    /// Stable station that was requested.
    pub station: BbcStationPreset,
    /// Highest accessible HTTPS manifest selected for this action.
    pub manifest_url: Url,
    /// Nominal bitrate advertised by BBC, when present.
    pub bitrate_kbps: Option<u32>,
    /// Codec label advertised by BBC.
    pub codec: String,
    /// MIME type advertised by BBC.
    pub mime_type: String,
    /// Selected segmented transfer format.
    pub transfer_format: BbcTransferFormat,
    /// Geo variant chosen by BBC for the current connection.
    pub audience: BbcAudience,
}

/// Blocking resolver for BBC's public Sounds-player flow.
///
/// Construct one resolver in a background provider worker. A resolution is
/// intentionally performed only when playback is requested, because both BBC's
/// token and the returned CDN manifest are action-scoped remote state.
#[derive(Clone)]
pub struct BbcLiveResolver {
    transport: Arc<dyn BbcTransport>,
    max_page_bytes: usize,
    max_selector_bytes: usize,
}

impl Default for BbcLiveResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl BbcLiveResolver {
    /// Creates a resolver with bounded responses and the shared provider timeout.
    #[must_use]
    pub fn new() -> Self {
        Self {
            transport: Arc::new(UreqBbcTransport {
                agent: super::provider_agent(DEFAULT_REQUEST_TIMEOUT),
            }),
            max_page_bytes: DEFAULT_MAX_PAGE_BYTES,
            max_selector_bytes: DEFAULT_MAX_SELECTOR_BYTES,
        }
    }

    /// Creates a resolver with explicit request and memory bounds.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] when a timeout or response
    /// bound is zero.
    pub fn with_options(
        timeout: Duration,
        max_page_bytes: usize,
        max_selector_bytes: usize,
    ) -> Result<Self, ProviderError> {
        if timeout.is_zero() {
            return Err(ProviderError::InvalidRequest(
                "BBC request timeout must be greater than zero".to_owned(),
            ));
        }
        if max_page_bytes == 0 || max_selector_bytes == 0 {
            return Err(ProviderError::InvalidRequest(
                "BBC response limits must be greater than zero".to_owned(),
            ));
        }
        Ok(Self {
            transport: Arc::new(UreqBbcTransport {
                agent: super::provider_agent(timeout),
            }),
            max_page_bytes,
            max_selector_bytes,
        })
    }

    /// Resolves one stable preset to the best HTTPS live manifest BBC exposes.
    ///
    /// BBC remains authoritative for geo and rights restrictions. This method
    /// does not retry with another region, media set, or hidden service.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] for a page/selector transport failure,
    /// oversized response, mismatched page identity, malformed token or
    /// response, or absence of an HTTPS HLS/DASH audio connection.
    pub fn resolve_station(
        &self,
        station: BbcStationPreset,
    ) -> Result<BbcLiveResolution, ProviderError> {
        validate_service_id(station.id)?;
        let page_url = station.sounds_url()?;
        let page = self.transport.get(
            &page_url,
            "text/html,application/xhtml+xml",
            None,
            self.max_page_bytes,
        )?;
        let context = parse_live_page(&page, station.id)?;
        let selector_url = media_selector_url(station.id)?;
        let authorization = format!("Bearer {}", context.token);
        let selector = self.transport.get(
            &selector_url,
            "application/json",
            Some(&authorization),
            self.max_selector_bytes,
        )?;
        let selected = parse_media_selector(&selector)?;
        Ok(BbcLiveResolution {
            station,
            manifest_url: selected.manifest_url,
            bitrate_kbps: selected.bitrate_kbps,
            codec: selected.codec,
            mime_type: selected.mime_type,
            transfer_format: selected.transfer_format,
            audience: context.audience,
        })
    }
}

trait BbcTransport: Send + Sync {
    fn get(
        &self,
        url: &Url,
        accept: &str,
        authorization: Option<&str>,
        limit: usize,
    ) -> Result<Vec<u8>, ProviderError>;
}

struct UreqBbcTransport {
    agent: ureq::Agent,
}

impl BbcTransport for UreqBbcTransport {
    fn get(
        &self,
        url: &Url,
        accept: &str,
        authorization: Option<&str>,
        limit: usize,
    ) -> Result<Vec<u8>, ProviderError> {
        get_bounded_bytes(&self.agent, url, accept, authorization, limit)
    }
}

#[derive(Debug)]
struct LivePageContext {
    token: String,
    audience: BbcAudience,
}

#[derive(Debug, Deserialize)]
struct LiveNextData {
    props: LiveProps,
    query: LiveQuery,
}

#[derive(Debug, Deserialize)]
struct LiveProps {
    #[serde(rename = "pageProps")]
    page_props: LivePageProps,
}

#[derive(Debug, Deserialize)]
struct LivePageProps {
    #[serde(rename = "jwtToken")]
    jwt_token: String,
    #[serde(rename = "isInUK")]
    is_in_uk: bool,
}

#[derive(Debug, Deserialize)]
struct LiveQuery {
    #[serde(rename = "serviceId")]
    service_id: String,
}

fn parse_live_page(
    page: &[u8],
    expected_service_id: &str,
) -> Result<LivePageContext, ProviderError> {
    let document = std::str::from_utf8(page).map_err(|error| {
        ProviderError::InvalidResponse(format!("BBC page is not UTF-8: {error}"))
    })?;
    let next_data = extract_next_data(document)?;
    let state: LiveNextData = serde_json::from_str(next_data).map_err(|error| {
        ProviderError::InvalidResponse(format!("BBC page state is invalid: {error}"))
    })?;
    if state.query.service_id != expected_service_id {
        return Err(ProviderError::InvalidResponse(
            "BBC page returned a different station identifier".to_owned(),
        ));
    }
    validate_playback_token(&state.props.page_props.jwt_token)?;
    Ok(LivePageContext {
        token: state.props.page_props.jwt_token,
        audience: if state.props.page_props.is_in_uk {
            BbcAudience::UnitedKingdom
        } else {
            BbcAudience::International
        },
    })
}

fn extract_next_data(document: &str) -> Result<&str, ProviderError> {
    let mut cursor = 0;
    while let Some(relative_start) = document[cursor..].find("<script") {
        let start = cursor + relative_start;
        let Some(relative_open_end) = document[start..].find('>') else {
            break;
        };
        let open_end = start + relative_open_end;
        let opening = &document[start..=open_end];
        if opening.contains("id=\"__NEXT_DATA__\"") || opening.contains("id='__NEXT_DATA__'") {
            let content_start = open_end + 1;
            let Some(relative_close) = document[content_start..].find("</script>") else {
                return Err(ProviderError::InvalidResponse(
                    "BBC page has an unterminated __NEXT_DATA__ script".to_owned(),
                ));
            };
            let content = &document[content_start..content_start + relative_close];
            if content.trim().is_empty() {
                return Err(ProviderError::InvalidResponse(
                    "BBC page has empty __NEXT_DATA__ state".to_owned(),
                ));
            }
            return Ok(content);
        }
        cursor = open_end + 1;
    }
    Err(ProviderError::InvalidResponse(
        "BBC page does not contain __NEXT_DATA__ state".to_owned(),
    ))
}

fn validate_playback_token(token: &str) -> Result<(), ProviderError> {
    let parts = token.split('.').collect::<Vec<_>>();
    if token.is_empty()
        || token.len() > MAX_PLAYBACK_TOKEN_BYTES
        || parts.len() != 3
        || parts.iter().any(|part| part.is_empty())
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ProviderError::InvalidResponse(
            "BBC page returned a malformed playback token".to_owned(),
        ));
    }
    Ok(())
}

fn validate_service_id(service_id: &str) -> Result<(), ProviderError> {
    if service_id.is_empty()
        || service_id.len() > MAX_SERVICE_ID_BYTES
        || !service_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(ProviderError::InvalidRequest(
            "BBC service ID must contain lowercase ASCII letters, digits, or underscores"
                .to_owned(),
        ));
    }
    Ok(())
}

fn media_selector_url(service_id: &str) -> Result<Url, ProviderError> {
    validate_service_id(service_id)?;
    Url::parse(&format!(
        "{MEDIA_SELECTOR_BASE_URL}mediaset/{MEDIA_SELECTOR_MEDIASET}/cvid/\
         urn:bbc:pips:pid:{service_id}/format/json"
    ))
    .map_err(|error| ProviderError::InvalidResponse(error.to_string()))
}

#[derive(Debug, Deserialize)]
struct MediaSelectorResponse {
    #[serde(default)]
    media: Vec<MediaSelectorMedia>,
}

#[derive(Debug, Deserialize)]
struct MediaSelectorMedia {
    bitrate: Option<u32>,
    kind: Option<String>,
    #[serde(rename = "type")]
    mime_type: Option<String>,
    encoding: Option<String>,
    #[serde(default)]
    connection: Vec<MediaSelectorConnection>,
}

#[derive(Debug, Deserialize)]
struct MediaSelectorConnection {
    protocol: Option<String>,
    #[serde(rename = "transferFormat")]
    transfer_format: Option<String>,
    href: Option<String>,
}

#[derive(Debug)]
struct SelectedMedia {
    manifest_url: Url,
    bitrate_kbps: Option<u32>,
    codec: String,
    mime_type: String,
    transfer_format: BbcTransferFormat,
}

fn parse_media_selector(payload: &[u8]) -> Result<SelectedMedia, ProviderError> {
    let response: MediaSelectorResponse = serde_json::from_slice(payload).map_err(|error| {
        ProviderError::InvalidResponse(format!("BBC Media Selector returned invalid JSON: {error}"))
    })?;
    let mut best: Option<((u32, u8), SelectedMedia)> = None;
    for media in response.media {
        if media.kind.as_deref() != Some("audio") {
            continue;
        }
        let bitrate = media
            .bitrate
            .filter(|bitrate| (1..=10_000).contains(bitrate));
        let codec = bounded_remote_label(media.encoding, "codec")?;
        let mime_type = bounded_remote_label(media.mime_type, "MIME type")?;
        for connection in media.connection {
            if connection.protocol.as_deref() != Some("https") {
                continue;
            }
            let transfer_format = match connection.transfer_format.as_deref() {
                Some("hls") => BbcTransferFormat::Hls,
                Some("dash") => BbcTransferFormat::Dash,
                _ => continue,
            };
            let Some(raw_url) = connection.href else {
                continue;
            };
            let manifest_url = validate_manifest_url(&raw_url)?;
            let priority = (
                bitrate.unwrap_or_default(),
                match transfer_format {
                    BbcTransferFormat::Hls => 2,
                    BbcTransferFormat::Dash => 1,
                },
            );
            if best
                .as_ref()
                .is_some_and(|(current_priority, _)| current_priority >= &priority)
            {
                continue;
            }
            best = Some((
                priority,
                SelectedMedia {
                    manifest_url,
                    bitrate_kbps: bitrate,
                    codec: codec.clone(),
                    mime_type: mime_type.clone(),
                    transfer_format,
                },
            ));
        }
    }
    best.map(|(_, selected)| selected).ok_or_else(|| {
        ProviderError::InvalidResponse(
            "BBC Media Selector returned no HTTPS HLS or DASH audio manifest".to_owned(),
        )
    })
}

fn bounded_remote_label(value: Option<String>, field: &str) -> Result<String, ProviderError> {
    let value = value.unwrap_or_default();
    if value.is_empty()
        || value.len() > MAX_REMOTE_LABEL_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(ProviderError::InvalidResponse(format!(
            "BBC Media Selector returned an invalid {field}"
        )));
    }
    Ok(value)
}

fn validate_manifest_url(raw: &str) -> Result<Url, ProviderError> {
    let url = Url::parse(raw).map_err(|error| {
        ProviderError::InvalidResponse(format!("BBC returned an invalid manifest URL: {error}"))
    })?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(ProviderError::InvalidResponse(
            "BBC manifest must be a credential-free HTTPS URL without a fragment".to_owned(),
        ));
    }
    Ok(url)
}

fn get_bounded_bytes(
    agent: &ureq::Agent,
    url: &Url,
    accept: &str,
    authorization: Option<&str>,
    limit: usize,
) -> Result<Vec<u8>, ProviderError> {
    if limit == 0 {
        return Err(ProviderError::InvalidRequest(
            "BBC response limit must be greater than zero".to_owned(),
        ));
    }
    let mut request = agent.get(url.as_str()).header("Accept", accept);
    if let Some(authorization) = authorization {
        request = request.header("Authorization", authorization);
    }
    let mut response = request.call().map_err(map_ureq_error)?;
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
    Ok(bytes)
}

fn map_ureq_error(error: ureq::Error) -> ProviderError {
    match error {
        ureq::Error::StatusCode(code) => ProviderError::HttpStatus(code),
        ureq::Error::BodyExceedsLimit(limit) => ProviderError::ResponseTooLarge {
            limit: usize::try_from(limit).unwrap_or(usize::MAX),
        },
        other => ProviderError::Transport(other.to_string()),
    }
}

/// Builds a BBC podcast RSS URL from an eight-character programme ID.
///
/// BBC podcast pages advertise feeds from `podcasts.files.bbci.co.uk`. The
/// programme ID should be read from that page or imported from the BBC OPML
/// index.
///
/// # Errors
///
/// Returns [`ProviderError::InvalidRequest`] for a malformed programme ID.
pub fn podcast_feed_url(programme_id: &str) -> Result<Url, ProviderError> {
    if programme_id.len() != 8
        || !programme_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Err(ProviderError::InvalidRequest(
            "BBC programme ID must contain eight lowercase ASCII letters or digits".to_owned(),
        ));
    }

    let mut url = Url::parse("https://podcasts.files.bbci.co.uk/")
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    url.path_segments_mut()
        .map_err(|()| ProviderError::InvalidResponse("invalid BBC podcast base URL".to_owned()))?
        .pop_if_empty()
        .push(&format!("{programme_id}.rss"));
    Ok(url)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        sync::{Arc, Mutex},
    };

    use super::*;

    const LIVE_PAGE: &str = r#"<!doctype html><html><body>
<script type="application/json" id="__NEXT_DATA__">{
  "props":{"pageProps":{"jwtToken":"abc.DEF_123.sig-9","isInUK":false}},
  "query":{"serviceId":"bbc_radio_one"}
}</script></body></html>"#;

    const SELECTOR_RESPONSE: &str = r#"{
  "media": [
    {
      "bitrate": 128,
      "presentation_type": "dynamic",
      "kind": "audio",
      "type": "audio/mp4",
      "encoding": "aac",
      "connection": [
        {
          "protocol": "https",
          "href": "https://low.example.test/radio-one.mpd",
          "transferFormat": "dash"
        }
      ]
    },
    {
      "bitrate": 320,
      "presentation_type": "dynamic",
      "kind": "audio",
      "type": "audio/mp4",
      "encoding": "aac",
      "connection": [
        {
          "protocol": "http",
          "href": "http://insecure.example.test/radio-one.m3u8",
          "transferFormat": "hls"
        },
        {
          "protocol": "https",
          "href": "https://audio.example.test/radio-one.mpd",
          "transferFormat": "dash"
        },
        {
          "protocol": "https",
          "href": "https://audio.example.test/radio-one.m3u8",
          "transferFormat": "hls"
        }
      ]
    }
  ]
}"#;

    #[test]
    fn catalogue_contains_all_stable_directory_services_once() {
        assert_eq!(STATIONS.len(), 69);
        let ids = STATIONS
            .iter()
            .map(|station| station.id)
            .collect::<HashSet<_>>();
        assert_eq!(ids.len(), STATIONS.len());
        assert!(!ids.contains("bbc_radio_five_sports_extra_2"));
        assert!(!ids.contains("bbc_radio_five_sports_extra_3"));
        assert_eq!(
            STATIONS
                .iter()
                .filter(|station| station.group == BbcStationGroup::National)
                .count(),
            17
        );
        assert_eq!(
            STATIONS
                .iter()
                .filter(|station| station.group == BbcStationGroup::Nations)
                .count(),
            11
        );
        assert_eq!(
            STATIONS
                .iter()
                .filter(|station| station.group == BbcStationGroup::Local)
                .count(),
            41
        );
    }

    #[test]
    fn presets_use_current_slash_paths_and_correct_1xtra_id() {
        let station = station_by_id("bbc_1xtra").expect("official 1Xtra ID should exist");
        assert_eq!(station.name, "BBC Radio 1Xtra");
        assert_eq!(
            station
                .sounds_url()
                .expect("preset URL should parse")
                .as_str(),
            "https://www.bbc.co.uk/sounds/play/live/bbc_1xtra"
        );
        assert!(station_by_id("bbc_radio_1xtra").is_none());
        for station in STATIONS {
            assert!(
                station
                    .page
                    .starts_with("https://www.bbc.co.uk/sounds/play/live/")
            );
            assert!(!station.page.contains("live:"));
        }
    }

    #[test]
    fn station_urls_accept_current_and_legacy_bbc_forms_only() {
        for raw in [
            "https://www.bbc.co.uk/sounds/play/live/bbc_radio_one",
            "https://bbc.co.uk/sounds/play/live:bbc_radio_one",
            "https://www.bbc.com/sounds/play/live/bbc_radio_one?partner=fixture",
        ] {
            let url = Url::parse(raw).expect("fixture URL");
            assert_eq!(
                station_from_url(&url).map(|station| station.id),
                Some("bbc_radio_one")
            );
        }
        for raw in [
            "http://www.bbc.co.uk/sounds/play/live/bbc_radio_one",
            "https://example.test/sounds/play/live/bbc_radio_one",
            "https://www.bbc.co.uk/sounds/play/live/not_a_station",
            "https://www.bbc.co.uk/sounds/play/live/bbc_radio_one/extra",
            "https://user@www.bbc.co.uk/sounds/play/live/bbc_radio_one",
        ] {
            let url = Url::parse(raw).expect("fixture URL");
            assert!(station_from_url(&url).is_none(), "{raw}");
        }
    }

    #[test]
    fn live_page_parser_keeps_token_private_and_reports_geo_variant() {
        let parsed =
            parse_live_page(LIVE_PAGE.as_bytes(), "bbc_radio_one").expect("valid page state");
        assert_eq!(parsed.token, "abc.DEF_123.sig-9");
        assert_eq!(parsed.audience, BbcAudience::International);
    }

    #[test]
    fn live_page_rejects_mismatched_station_and_malformed_token() {
        assert!(matches!(
            parse_live_page(LIVE_PAGE.as_bytes(), "bbc_radio_two"),
            Err(ProviderError::InvalidResponse(_))
        ));
        let malformed = LIVE_PAGE.replace("abc.DEF_123.sig-9", "not a token");
        assert!(matches!(
            parse_live_page(malformed.as_bytes(), "bbc_radio_one"),
            Err(ProviderError::InvalidResponse(_))
        ));
    }

    #[test]
    fn selector_chooses_highest_https_audio_and_prefers_hls() {
        let selected =
            parse_media_selector(SELECTOR_RESPONSE.as_bytes()).expect("valid selector fixture");
        assert_eq!(
            selected.manifest_url.as_str(),
            "https://audio.example.test/radio-one.m3u8"
        );
        assert_eq!(selected.bitrate_kbps, Some(320));
        assert_eq!(selected.codec, "aac");
        assert_eq!(selected.mime_type, "audio/mp4");
        assert_eq!(selected.transfer_format, BbcTransferFormat::Hls);
    }

    #[test]
    fn selector_rejects_insecure_only_or_non_audio_payloads() {
        let insecure = SELECTOR_RESPONSE
            .replace("\"https\"", "\"http\"")
            .replace("https://", "http://");
        assert!(matches!(
            parse_media_selector(insecure.as_bytes()),
            Err(ProviderError::InvalidResponse(_))
        ));
        let video = SELECTOR_RESPONSE.replace("\"audio\"", "\"video\"");
        assert!(matches!(
            parse_media_selector(video.as_bytes()),
            Err(ProviderError::InvalidResponse(_))
        ));
    }

    #[derive(Default)]
    struct MockTransport {
        requests: Mutex<Vec<(String, Option<String>, usize)>>,
    }

    impl BbcTransport for MockTransport {
        fn get(
            &self,
            url: &Url,
            _accept: &str,
            authorization: Option<&str>,
            limit: usize,
        ) -> Result<Vec<u8>, ProviderError> {
            self.requests.lock().expect("request log lock").push((
                url.as_str().to_owned(),
                authorization.map(str::to_owned),
                limit,
            ));
            if url.host_str() == Some("www.bbc.co.uk") {
                return Ok(LIVE_PAGE.as_bytes().to_vec());
            }
            if url.host_str() == Some("open.live.bbc.co.uk") {
                assert_eq!(authorization, Some("Bearer abc.DEF_123.sig-9"));
                return Ok(SELECTOR_RESPONSE.as_bytes().to_vec());
            }
            Err(ProviderError::InvalidRequest(
                "unexpected mock BBC URL".to_owned(),
            ))
        }
    }

    #[test]
    fn resolver_fetches_a_fresh_manifest_for_each_playback_action() {
        let transport = Arc::new(MockTransport::default());
        let resolver = BbcLiveResolver {
            transport: Arc::clone(&transport) as Arc<dyn BbcTransport>,
            max_page_bytes: DEFAULT_MAX_PAGE_BYTES,
            max_selector_bytes: DEFAULT_MAX_SELECTOR_BYTES,
        };
        let station = station_by_id("bbc_radio_one").expect("fixture station");

        let first = resolver.resolve_station(station).expect("first resolution");
        let second = resolver
            .resolve_station(station)
            .expect("second resolution");

        assert_eq!(first, second);
        let requests = transport.requests.lock().expect("request log lock");
        assert_eq!(requests.len(), 4);
        assert!(requests[0].1.is_none());
        assert_eq!(requests[0].2, DEFAULT_MAX_PAGE_BYTES);
        assert_eq!(requests[1].1.as_deref(), Some("Bearer abc.DEF_123.sig-9"));
        assert_eq!(requests[1].2, DEFAULT_MAX_SELECTOR_BYTES);
        assert!(requests[2].1.is_none());
        assert_eq!(requests[3].1.as_deref(), Some("Bearer abc.DEF_123.sig-9"));
    }

    #[test]
    fn programme_id_builds_documented_rss_url() {
        assert_eq!(
            podcast_feed_url("p02nq0gn")
                .expect("fixture programme ID should be valid")
                .as_str(),
            "https://podcasts.files.bbci.co.uk/p02nq0gn.rss"
        );
    }

    #[test]
    fn programme_id_rejects_paths_and_mixed_case() {
        for invalid in ["../feed!", "P02NQ0GN", "short"] {
            assert!(matches!(
                podcast_feed_url(invalid),
                Err(ProviderError::InvalidRequest(_))
            ));
        }
    }
}
