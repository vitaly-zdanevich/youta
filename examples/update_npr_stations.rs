//! Regenerates Youta's checked-in NPR station-service snapshot.
//!
//! The NPR station finder exposes state-filtered searches but no complete
//! enumeration or pagination contract. This tool queries every US state,
//! Washington, D.C., and the inhabited territories, deduplicates inherited
//! station services by NPR stream GUID, and writes a reviewable Rust module.
//! Runtime Youta builds never query the directory.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    env,
    error::Error,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use serde::Deserialize;
use url::Url;

const API_URL: &str = "https://station.api.npr.org/v3/stations";
const SNAPSHOT_DATE: &str = "2026-07-28";
const DEFAULT_OUTPUT: &str = "src/providers/npr_stations_generated.rs";
const STATE_CODES: &[&str] = &[
    "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "FL", "GA", "HI", "ID", "IL", "IN", "IA", "KS",
    "KY", "LA", "ME", "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE", "NV", "NH", "NJ", "NM", "NY",
    "NC", "ND", "OH", "OK", "OR", "PA", "RI", "SC", "SD", "TN", "TX", "UT", "VT", "VA", "WA", "WV",
    "WI", "WY", "DC", "PR", "VI", "GU", "AS", "MP",
];

#[derive(Debug, Deserialize)]
struct StationResponse {
    #[serde(default)]
    items: Vec<StationItem>,
}

#[derive(Debug, Deserialize)]
struct StationItem {
    attributes: StationAttributes,
    #[serde(default)]
    links: StationLinks,
}

#[derive(Debug, Deserialize)]
struct StationAttributes {
    #[serde(rename = "orgId")]
    org_id: String,
    brand: Brand,
    #[serde(default)]
    network: Option<Network>,
    #[serde(rename = "streamsV2", default)]
    streams: Vec<Stream>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Brand {
    #[serde(default)]
    band: Option<String>,
    #[serde(default)]
    call: Option<String>,
    #[serde(default)]
    frequency: Option<String>,
    #[serde(default)]
    market_city: Option<String>,
    #[serde(default)]
    market_state: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Network {
    #[serde(default)]
    uses_inheritance: bool,
}

#[derive(Debug, Default, Deserialize)]
struct StationLinks {
    #[serde(default)]
    brand: Vec<Link>,
}

#[derive(Debug, Deserialize)]
struct Link {
    #[serde(default)]
    rel: String,
    #[serde(default)]
    href: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stream {
    #[serde(default)]
    title: String,
    #[serde(default)]
    guid: String,
    #[serde(default)]
    urls: Vec<StreamUrl>,
    #[serde(default)]
    primary: bool,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamUrl {
    #[serde(default)]
    rel: String,
    #[serde(rename = "content-type", default)]
    _content_type: String,
    #[serde(default)]
    href: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Codec {
    Aac,
    Mp3,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum StreamKind {
    Direct,
    M3u,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AudioChoice {
    url: String,
    codec: Option<Codec>,
    stream_kind: StreamKind,
    rank: u8,
}

#[derive(Clone, Debug)]
struct ServiceCandidate {
    guid: String,
    title: String,
    description: Option<String>,
    primary: bool,
    org_id: String,
    brand_name: String,
    call: String,
    brand: String,
    city: String,
    state: String,
    homepage: String,
    inherited: bool,
    audio: AudioChoice,
    audio_alternatives: Vec<AudioChoice>,
}

#[derive(Debug)]
struct ServiceGroup {
    selected: ServiceCandidate,
    aliases: BTreeSet<String>,
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut output = PathBuf::from(DEFAULT_OUTPUT);
    let mut input_dir = None;
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--input-dir" => {
                input_dir = Some(PathBuf::from(
                    arguments.next().ok_or("--input-dir requires a path")?,
                ));
            }
            "--output" => {
                output = PathBuf::from(arguments.next().ok_or("--output requires a path")?);
            }
            "--help" | "-h" => {
                println!(
                    "Usage: cargo run --locked --example update_npr_stations --features radio -- \
                     [--input-dir DIR] [--output FILE]"
                );
                return Ok(());
            }
            _ => return Err(format!("unknown argument: {argument}").into()),
        }
    }

    let responses = if let Some(input_dir) = input_dir {
        read_responses(&input_dir)?
    } else {
        fetch_responses()?
    };
    let mut services = collect_services(responses);
    resolve_static_playlists(&mut services)?;
    let rendered = render_module(&services);
    fs::write(&output, rendered)?;
    println!(
        "wrote {} distinct NPR services to {}",
        services.len(),
        output.display()
    );
    Ok(())
}

fn read_responses(input_dir: &Path) -> Result<Vec<StationResponse>, Box<dyn Error>> {
    STATE_CODES
        .iter()
        .map(|state| {
            let path = input_dir.join(format!("{state}.json"));
            let payload = fs::read(&path)?;
            serde_json::from_slice(&payload)
                .map_err(|error| format!("{}: {error}", path.display()).into())
        })
        .collect()
}

fn fetch_responses() -> Result<Vec<StationResponse>, Box<dyn Error>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .user_agent("Youta NPR station snapshot generator")
        .build()
        .into();
    STATE_CODES
        .iter()
        .map(|state| {
            let mut response = agent.get(API_URL).query("state", state).call()?;
            let parsed = response
                .body_mut()
                .read_json()
                .map_err(|error| error.into());
            thread::sleep(Duration::from_millis(50));
            parsed
        })
        .collect()
}

fn resolve_static_playlists(
    services: &mut BTreeMap<String, ServiceGroup>,
) -> Result<(), Box<dyn Error>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .user_agent("Youta NPR station snapshot generator")
        .build()
        .into();
    let mut excluded = Vec::new();
    for (guid, group) in services.iter_mut() {
        let mut selected = None;
        let mut failures = Vec::new();
        for alternative in &group.selected.audio_alternatives {
            match resolve_audio_choice(&agent, alternative) {
                Ok(resolved) => {
                    selected = Some(resolved);
                    break;
                }
                Err(error) => failures.push(error),
            }
        }
        if let Some(selected) = selected {
            group.selected.audio = selected;
        } else {
            eprintln!(
                "excluding NPR service {guid}; no stable playable HTTPS choice: {}",
                failures.join("; ")
            );
            excluded.push(guid.clone());
        }
    }
    for guid in excluded {
        services.remove(&guid);
    }
    Ok(())
}

fn resolve_audio_choice(agent: &ureq::Agent, choice: &AudioChoice) -> Result<AudioChoice, String> {
    let mut resolved = choice.clone();
    for _ in 0..3 {
        if !is_static_playlist(&resolved.url) {
            resolved.url = normalize_stable_https_url(&resolved.url)
                .ok_or_else(|| format!("unstable or non-HTTPS target {}", resolved.url))?;
            resolved.stream_kind = if is_hls_playlist(&resolved.url) {
                StreamKind::M3u
            } else {
                StreamKind::Direct
            };
            return Ok(resolved);
        }
        let playlist_url = resolved.url.clone();
        let mut response = agent
            .get(&playlist_url)
            .header(
                "Accept",
                "audio/x-scpls,audio/x-mpegurl,application/vnd.apple.mpegurl,text/plain",
            )
            .call()
            .map_err(|error| format!("{playlist_url}: {error}"))?;
        let payload = response
            .body_mut()
            .with_config()
            .limit(64 * 1024)
            .read_to_vec()
            .map_err(|error| format!("{playlist_url}: {error}"))?;
        resolved.url = parse_static_playlist_target(&payload)
            .ok_or_else(|| format!("{playlist_url}: no stable HTTPS audio target"))?;
        thread::sleep(Duration::from_millis(50));
    }
    Err(format!(
        "{}: playlist nesting exceeds three levels",
        choice.url
    ))
}

fn is_static_playlist(url: &str) -> bool {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    path.ends_with(".pls") || (path.ends_with(".m3u") && !path.ends_with(".m3u8"))
}

fn is_hls_playlist(url: &str) -> bool {
    url.split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase()
        .ends_with(".m3u8")
}

fn parse_static_playlist_target(payload: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(payload);
    let mut m3u_target = None;
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if key.trim().to_ascii_lowercase().starts_with("file") {
                let value = value.trim();
                if let Some(value) = normalize_stable_https_url(value) {
                    return Some(value);
                }
            }
            continue;
        }
        if let Some(line) = normalize_stable_https_url(line) {
            m3u_target.get_or_insert(line);
        }
    }
    m3u_target
}

fn collect_services(responses: Vec<StationResponse>) -> BTreeMap<String, ServiceGroup> {
    let mut services: BTreeMap<String, ServiceGroup> = BTreeMap::new();
    for item in responses.into_iter().flat_map(|response| response.items) {
        let homepage = item
            .links
            .brand
            .iter()
            .find(|link| link.rel == "homepage" && valid_web_url(&link.href))
            .map_or_else(
                || "https://www.npr.org/stations".to_owned(),
                |link| link.href.clone(),
            );
        let brand = station_brand(&item.attributes.brand);
        let call = clean(item.attributes.brand.call.as_deref());
        let brand_name = item
            .attributes
            .brand
            .name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .or(item.attributes.brand.call.as_deref())
            .unwrap_or("NPR member station")
            .trim()
            .to_owned();
        let city = clean(item.attributes.brand.market_city.as_deref());
        let state = clean(item.attributes.brand.market_state.as_deref());
        let inherited = item
            .attributes
            .network
            .as_ref()
            .is_some_and(|network| network.uses_inheritance);

        for stream in item.attributes.streams {
            let audio_alternatives = audio_choices(&stream.urls);
            let Some(audio) = audio_alternatives.first().cloned() else {
                continue;
            };
            if stream.guid.trim().is_empty() {
                continue;
            }
            let title = if stream.title.trim().is_empty() {
                brand_name.clone()
            } else {
                stream.title.trim().to_owned()
            };
            let candidate = ServiceCandidate {
                guid: stream.guid,
                title,
                description: clean_option(stream.description.as_deref()),
                primary: stream.primary,
                org_id: item.attributes.org_id.clone(),
                brand_name: brand_name.clone(),
                call: call.clone(),
                brand: brand.clone(),
                city: city.clone(),
                state: state.clone(),
                homepage: homepage.clone(),
                inherited,
                audio,
                audio_alternatives,
            };
            let alias = candidate_alias(&candidate);
            match services.entry(candidate.guid.clone()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(ServiceGroup {
                        selected: candidate,
                        aliases: BTreeSet::from([alias]),
                    });
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    let group = entry.get_mut();
                    group.aliases.insert(alias);
                    if compare_candidate(&candidate, &group.selected) == Ordering::Less {
                        group.selected = candidate;
                    }
                }
            }
        }
    }
    services
}

fn audio_choices(urls: &[StreamUrl]) -> Vec<AudioChoice> {
    let mut choices: Vec<_> = urls
        .iter()
        .filter_map(|url| {
            let stable_url = normalize_stable_https_url(&url.href)?;
            let (rank, codec) = match url.rel.as_str() {
                // HLS is adaptive when a station publishes variants. Without
                // bitrate metadata, preserve NPR's ordering for direct audio.
                "stream-hls-audio" => (0, Some(Codec::Aac)),
                "stream-mp3-audio" => (1, Some(Codec::Mp3)),
                "stream-aac-audio" => (2, Some(Codec::Aac)),
                _ => return None,
            };
            let lower = url.href.to_ascii_lowercase();
            let stream_kind = if lower.ends_with(".m3u")
                || lower.contains(".m3u?")
                || lower.ends_with(".m3u8")
                || lower.contains(".m3u8?")
            {
                StreamKind::M3u
            } else {
                StreamKind::Direct
            };
            Some(AudioChoice {
                url: stable_url,
                codec,
                stream_kind,
                rank,
            })
        })
        .collect();
    choices.sort_by(|left, right| {
        left.rank
            .cmp(&right.rank)
            .then_with(|| left.url.cmp(&right.url))
    });
    choices.dedup_by(|left, right| left.url == right.url);
    choices
}

fn normalize_stable_https_url(value: &str) -> Option<String> {
    let Ok(mut url) = Url::parse(value) else {
        return None;
    };
    if url.scheme() != "https" || url.host_str().is_none() {
        return None;
    }
    let mut retained = Vec::new();
    for (key, value) in url.query_pairs() {
        let key_lower = key.to_ascii_lowercase();
        if matches!(
            key_lower.as_str(),
            "auth"
                | "exp"
                | "expires"
                | "hdnea"
                | "hdnts"
                | "key"
                | "policy"
                | "sig"
                | "signature"
                | "token"
                | "zt"
        ) {
            return None;
        }
        if matches!(key_lower.as_str(), "_ic2" | "playsessionid") {
            continue;
        }
        retained.push((key.into_owned(), value.into_owned()));
    }
    if retained.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut().clear().extend_pairs(&retained);
    }
    Some(url.into())
}

fn compare_candidate(left: &ServiceCandidate, right: &ServiceCandidate) -> Ordering {
    left.inherited
        .cmp(&right.inherited)
        .then_with(|| service_affinity(right).cmp(&service_affinity(left)))
        .then_with(|| (!left.primary).cmp(&(!right.primary)))
        .then_with(|| band_rank(&left.brand).cmp(&band_rank(&right.brand)))
        .then_with(|| left.org_id.cmp(&right.org_id))
}

fn service_affinity(candidate: &ServiceCandidate) -> bool {
    contains_case_insensitive(&candidate.title, &candidate.call)
        || contains_case_insensitive(&candidate.title, &candidate.brand_name)
}

fn band_rank(brand: &str) -> u8 {
    if brand.contains(" FM") {
        0
    } else if brand.contains(" AM") {
        2
    } else {
        1
    }
}

fn station_brand(brand: &Brand) -> String {
    [
        clean(brand.call.as_deref()),
        clean(brand.frequency.as_deref()),
        clean(brand.band.as_deref()),
    ]
    .into_iter()
    .filter(|value| !value.is_empty())
    .collect::<Vec<_>>()
    .join(" ")
}

fn candidate_alias(candidate: &ServiceCandidate) -> String {
    let location = match (candidate.city.is_empty(), candidate.state.is_empty()) {
        (false, false) => format!("{}, {}", candidate.city, candidate.state),
        (false, true) => candidate.city.clone(),
        (true, false) => candidate.state.clone(),
        (true, true) => String::new(),
    };
    match (candidate.brand.is_empty(), location.is_empty()) {
        (false, false) => format!("{} ({location})", candidate.brand),
        (false, true) => candidate.brand.clone(),
        (true, false) => location,
        (true, true) => candidate.brand_name.clone(),
    }
}

fn render_module(services: &BTreeMap<String, ServiceGroup>) -> String {
    let mut output = String::new();
    writeln!(
        output,
        "//! Generated NPR member-station service snapshot.\n//!\n//! Source: \
         <https://station.api.npr.org/v3/stations>, queried by US state and\n//! \
         territory on {SNAPSHOT_DATE}. Do not edit this file by hand; run\n//! \
         `cargo run --locked --example update_npr_stations --features radio`.\n"
    )
    .unwrap();
    writeln!(
        output,
        "use super::{{\n    RadioCodec, RadioNowPlayingEndpoint, RadioNowPlayingFormat, \
         RadioStationPreset, RadioStreamKind,\n}};\n"
    )
    .unwrap();
    writeln!(
        output,
        "/// Date of the official NPR station-finder snapshot.\npub const \
         NPR_STATION_SNAPSHOT_DATE: &str = {SNAPSHOT_DATE:?};"
    )
    .unwrap();
    writeln!(
        output,
        "/// Number of state and territory filters queried by the generator.\npub const \
         NPR_STATION_QUERY_COUNT: usize = {};\n",
        STATE_CODES.len()
    )
    .unwrap();
    writeln!(
        output,
        "/// Distinct NPR stream GUIDs with a stable usable HTTPS audio URL.\npub const \
         NPR_STATION_SERVICE_COUNT: usize = {};\n",
        services.len()
    )
    .unwrap();
    writeln!(
        output,
        "/// Static NPR member-station services available without a startup directory request.\n\
         pub const NPR_STATIONS: &[RadioStationPreset] = &["
    )
    .unwrap();
    for group in services.values() {
        let station = &group.selected;
        let name = display_name(station);
        let summary = summary(group);
        let codec = match station.audio.codec {
            Some(Codec::Aac) => "Some(RadioCodec::Aac)",
            Some(Codec::Mp3) => "Some(RadioCodec::Mp3)",
            None => "None",
        };
        let stream_kind = match station.audio.stream_kind {
            StreamKind::Direct => "RadioStreamKind::Direct",
            StreamKind::M3u => "RadioStreamKind::M3u",
        };
        writeln!(
            output,
            "    RadioStationPreset {{\n        id: {:?},\n        name: {:?},\n        \
             homepage: {:?},\n        stream: {:?},\n        summary: {:?},\n        codec: \
             {codec},\n        bitrate_kbps: None,\n        sample_rate_hz: None,\n        \
             channels: None,\n        stream_kind: {stream_kind},\n        now_playing: \
             Some(RadioNowPlayingEndpoint {{\n            url: {:?},\n            format: \
             RadioNowPlayingFormat::NprStationProgramJson,\n        }}),\n    }},",
            format!("npr-{}", station.guid),
            name,
            station.homepage,
            station.audio.url,
            summary,
            format!(
                "https://organization.api.npr.org/v3/streams/{}/programs/now",
                station.guid
            )
        )
        .unwrap();
    }
    output.push_str("];\n");
    output
}

fn display_name(station: &ServiceCandidate) -> String {
    let title = station.title.trim();
    if contains_case_insensitive(title, &station.brand_name) {
        title.to_owned()
    } else {
        format!("{} — {title}", station.brand_name)
    }
}

fn summary(group: &ServiceGroup) -> String {
    let station = &group.selected;
    let mut parts = vec!["NPR member-station service".to_owned()];
    if let Some(description) = station.description.as_deref() {
        if !contains_case_insensitive(&station.title, description) {
            parts.push(description.trim_end_matches(['.', ';']).trim().to_owned());
        }
    }
    if !group.aliases.is_empty() {
        parts.push(format!(
            "Station: {}",
            group.aliases.iter().cloned().collect::<Vec<_>>().join("; ")
        ));
    }
    parts.join(". ")
}

fn contains_case_insensitive(haystack: &str, needle: &str) -> bool {
    !needle.trim().is_empty()
        && haystack
            .to_ascii_lowercase()
            .contains(&needle.to_ascii_lowercase())
}

fn clean(value: Option<&str>) -> String {
    value.map(str::trim).unwrap_or_default().to_owned()
}

fn clean_option(value: Option<&str>) -> Option<String> {
    let value = clean(value);
    (!value.is_empty()).then_some(value)
}

fn valid_web_url(value: &str) -> bool {
    value.starts_with("https://") || value.starts_with("http://")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOCK_RESPONSE: &str = r#"{
      "items": [
        {
          "attributes": {
            "orgId": "554",
            "network": {"usesInheritance": false},
            "brand": {
              "band": "FM", "call": "WNYC", "frequency": "93.9",
              "marketCity": "New York", "marketState": "NY", "name": "WNYC"
            },
            "streamsV2": [
              {
                "title": "WNYC FM", "guid": "primary-guid", "primary": true,
                "description": "News and talk",
                "urls": [
                  {"rel": "stream-mp3-audio", "content-type": "audio/mp3",
                   "href": "https://example.test/live.mp3"},
                  {"rel": "stream-aac-audio", "content-type": "audio/aac",
                   "href": "https://example.test/live.aac"}
                ]
              },
              {
                "title": "New Sounds", "guid": "music-guid", "primary": false,
                "urls": [
                  {"rel": "stream-hls-audio",
                   "content-type": "application/vnd.apple.mpegurl",
                   "href": "https://example.test/music.m3u8"}
                ]
              }
            ]
          },
          "links": {"brand": [{"rel": "homepage", "href": "https://example.test"}]}
        },
        {
          "attributes": {
            "orgId": "553",
            "network": {"usesInheritance": true},
            "brand": {
              "band": "AM", "call": "WNYC", "frequency": "820",
              "marketCity": "New York", "marketState": "NY", "name": "WNYC"
            },
            "streamsV2": [
              {
                "title": "WNYC FM", "guid": "primary-guid", "primary": true,
                "urls": [
                  {"rel": "stream-mp3-audio", "content-type": "audio/mp3",
                   "href": "https://example.test/live.mp3"}
                ]
              },
              {
                "title": "Insecure", "guid": "insecure-guid", "primary": false,
                "urls": [
                  {"rel": "stream-mp3-audio", "content-type": "audio/mp3",
                   "href": "http://example.test/insecure.mp3"}
                ]
              }
            ]
          },
          "links": {"brand": []}
        }
      ]
    }"#;

    #[test]
    fn deduplicates_inherited_transmitters_but_keeps_distinct_services() {
        let response: StationResponse = serde_json::from_str(MOCK_RESPONSE).unwrap();
        let services = collect_services(vec![response]);

        assert_eq!(services.len(), 2);
        assert!(services.contains_key("primary-guid"));
        assert!(services.contains_key("music-guid"));
        assert_eq!(services["primary-guid"].aliases.len(), 2);
        assert_eq!(services["primary-guid"].selected.org_id, "554");
        assert!(!services.contains_key("insecure-guid"));
    }

    #[test]
    fn prefers_hls_but_does_not_guess_unpublished_quality() {
        let response: StationResponse = serde_json::from_str(MOCK_RESPONSE).unwrap();
        let services = collect_services(vec![response]);
        let music = &services["music-guid"].selected;
        let talk = &services["primary-guid"].selected;

        assert_eq!(music.audio.codec, Some(Codec::Aac));
        assert_eq!(music.audio.url, "https://example.test/music.m3u8");
        assert_eq!(music.audio.stream_kind, StreamKind::M3u);
        assert_eq!(talk.audio.codec, Some(Codec::Mp3));
        assert_eq!(talk.audio.url, "https://example.test/live.mp3");
        let rendered = render_module(&services);
        assert!(rendered.contains("bitrate_kbps: None"));
        assert!(!rendered.contains("bitrate_kbps: Some"));
    }

    #[test]
    fn generated_summary_is_searchable_by_call_location_and_alias() {
        let response: StationResponse = serde_json::from_str(MOCK_RESPONSE).unwrap();
        let services = collect_services(vec![response]);
        let summary = summary(&services["primary-guid"]);

        assert!(summary.contains("WNYC 93.9 FM"));
        assert!(summary.contains("WNYC 820 AM"));
        assert!(summary.contains("New York, NY"));
    }

    #[test]
    fn static_playlist_parser_selects_only_https_audio_targets() {
        assert_eq!(
            parse_static_playlist_target(
                b"[playlist]\nFile1=https://audio.example/live.mp3\nTitle1=Live\n"
            )
            .as_deref(),
            Some("https://audio.example/live.mp3")
        );
        assert_eq!(
            parse_static_playlist_target(
                b"#EXTM3U\nhttp://insecure.example/live\nhttps://audio.example/live.aac\n"
            )
            .as_deref(),
            Some("https://audio.example/live.aac")
        );
        assert_eq!(
            parse_static_playlist_target(b"[playlist]\nFile1=http://insecure.example/live\n"),
            None
        );
    }

    #[test]
    fn hls_is_not_flattened_as_a_static_playlist() {
        assert!(is_static_playlist("https://example.test/live.pls"));
        assert!(is_static_playlist("https://example.test/live.m3u?token=1"));
        assert!(!is_static_playlist("https://example.test/live.m3u8"));
        assert!(is_hls_playlist("https://example.test/live.m3u8?token=1"));
    }

    #[test]
    fn signed_or_expiring_stream_urls_are_never_snapshotted() {
        for url in [
            "https://stream.example/live?token=secret",
            "https://stream.example/live?sig=abc&expires=123",
            "https://stream.example/live?zt=jwt",
        ] {
            assert!(
                normalize_stable_https_url(url).is_none(),
                "transient URL accepted: {url}"
            );
        }
        assert_eq!(
            normalize_stable_https_url(
                "https://stream.example/live?aw_0_1st.playerid=stationconnect"
            )
            .as_deref(),
            Some("https://stream.example/live?aw_0_1st.playerid=stationconnect")
        );
        assert_eq!(
            normalize_stable_https_url(
                "https://stream.example/live?_ic2=1776266297042&playSessionID=session"
            )
            .as_deref(),
            Some("https://stream.example/live")
        );
        assert_eq!(
            parse_static_playlist_target(
                b"File1=https://stream.example/live?token=secret\n\
                  File2=https://stream.example/stable.mp3\n"
            )
            .as_deref(),
            Some("https://stream.example/stable.mp3")
        );
    }
}
