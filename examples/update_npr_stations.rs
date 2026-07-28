//! Regenerates Youta's checked-in NPR station-service snapshot.
//!
//! The NPR station finder exposes state-filtered searches but no complete
//! enumeration or pagination contract. This tool queries every US state,
//! Washington, D.C., and the inhabited territories, deduplicates inherited
//! station services by NPR stream GUID, and writes a reviewable Rust module.
//! Runtime Youta builds never query the directory.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, VecDeque},
    env,
    error::Error,
    fmt::Write as _,
    fs,
    io::Read as _,
    path::{Path, PathBuf},
    process::{self, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use url::Url;

const API_URL: &str = "https://station.api.npr.org/v3/stations";
const STREAMTHEWORLD_LIVE_SUFFIX: &str = ".live.streamtheworld.com";
const STREAMTHEWORLD_REDIRECT_BASE: &str =
    "https://playerservices.streamtheworld.com/api/livestream-redirect/";
const SNAPSHOT_DATE: &str = "2026-07-28";
const DEFAULT_OUTPUT: &str = "src/providers/npr_stations_generated.rs";
const DEFAULT_QUALITY_CACHE: &str = "src/providers/npr_station_quality_generated.json";
const QUALITY_CACHE_FORMAT_VERSION: u8 = 1;
const QUALITY_PROBE_WORKERS: usize = 4;
const QUALITY_PROBE_TIMEOUT: Duration = Duration::from_secs(12);
const QUALITY_PROBE_POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_QUALITY_PROBE_JSON_BYTES: usize = 64 * 1024;
const MAX_QUALITY_CACHE_JSON_BYTES: usize = 4 * 1024 * 1024;
const MAX_RADIO_BITRATE_KBPS: u16 = 10_000;
const MIN_RADIO_SAMPLE_RATE_HZ: u32 = 8_000;
const MAX_RADIO_SAMPLE_RATE_HZ: u32 = 384_000;
const MAX_RADIO_CHANNELS: u8 = 32;
const COMMON_RADIO_BITRATES_KBPS: &[u16] = &[
    16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 192, 224, 256, 288, 320, 384, 448, 512,
    768,
];
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Codec {
    Aac,
    Flac,
    Mp3,
    Opus,
    Pcm,
    Vorbis,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

/// Command-line options for one deterministic snapshot update.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Options {
    output: PathBuf,
    input_dir: Option<PathBuf>,
    quality_cache: PathBuf,
    probe_quality: bool,
    probe_date: Option<String>,
    ffprobe: PathBuf,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            output: PathBuf::from(DEFAULT_OUTPUT),
            input_dir: None,
            quality_cache: PathBuf::from(DEFAULT_QUALITY_CACHE),
            probe_quality: false,
            probe_date: None,
            ffprobe: PathBuf::from("ffprobe"),
        }
    }
}

/// Result of parsing maintenance-tool arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ParsedOptions {
    Run(Options),
    Help,
}

/// Persistent quality facts for the exact stable URL selected for one service.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct CachedQuality {
    stream_url: String,
    probed_on: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    codec: Option<Codec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bitrate_kbps: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sample_rate_hz: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    channels: Option<u8>,
}

impl CachedQuality {
    /// Returns whether this record contains at least one usable quality fact.
    fn has_quality(&self) -> bool {
        self.codec.is_some()
            || self.bitrate_kbps.is_some()
            || self.sample_rate_hz.is_some()
            || self.channels.is_some()
    }

    /// Creates one whole replacement record from a successful current probe.
    fn from_probe(stream_url: &str, probe_date: &str, probed: ProbedQuality) -> Self {
        Self {
            stream_url: stream_url.to_owned(),
            probed_on: probe_date.to_owned(),
            codec: probed.codec,
            bitrate_kbps: probed.bitrate_kbps,
            sample_rate_hz: probed.sample_rate_hz,
            channels: probed.channels,
        }
    }
}

/// Deterministically sorted sidecar retained across generator runs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct QualityCache {
    format_version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_probe_attempt_on: Option<String>,
    #[serde(default)]
    services: BTreeMap<String, CachedQuality>,
}

impl Default for QualityCache {
    fn default() -> Self {
        Self {
            format_version: QUALITY_CACHE_FORMAT_VERSION,
            last_probe_attempt_on: None,
            services: BTreeMap::new(),
        }
    }
}

/// Best-effort technical facts parsed from one bounded `ffprobe` response.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ProbedQuality {
    codec: Option<Codec>,
    bitrate_kbps: Option<u16>,
    sample_rate_hz: Option<u32>,
    channels: Option<u8>,
}

impl ProbedQuality {
    /// Returns whether `ffprobe` supplied at least one validated quality fact.
    fn has_quality(self) -> bool {
        self.codec.is_some()
            || self.bitrate_kbps.is_some()
            || self.sample_rate_hz.is_some()
            || self.channels.is_some()
    }
}

/// Bounded subset of `ffprobe`'s JSON output used by the generator.
#[derive(Debug, Default, Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    streams: Vec<FfprobeStream>,
    #[serde(default)]
    format: Option<FfprobeFormat>,
}

/// One audio stream reported by `ffprobe`.
#[derive(Debug, Default, Deserialize)]
struct FfprobeStream {
    #[serde(default)]
    codec_name: Option<String>,
    #[serde(default)]
    bit_rate: Option<String>,
    #[serde(default)]
    sample_rate: Option<String>,
    #[serde(default)]
    channels: Option<u32>,
}

/// Container-level bitrate and ICY metadata reported by `ffprobe`.
#[derive(Debug, Default, Deserialize)]
struct FfprobeFormat {
    #[serde(default)]
    bit_rate: Option<String>,
    #[serde(default)]
    tags: BTreeMap<String, String>,
}

fn main() -> Result<(), Box<dyn Error>> {
    let parsed = parse_options(env::args().skip(1))?;
    let ParsedOptions::Run(options) = parsed else {
        print!("{}", usage());
        return Ok(());
    };
    run(options)
}

/// Parses the maintenance-only command line without reading process-global state.
fn parse_options(arguments: impl IntoIterator<Item = String>) -> Result<ParsedOptions, String> {
    let mut options = Options::default();
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--input-dir" => {
                options.input_dir = Some(PathBuf::from(
                    arguments.next().ok_or("--input-dir requires a path")?,
                ));
            }
            "--output" => {
                options.output = PathBuf::from(arguments.next().ok_or("--output requires a path")?);
            }
            "--quality-cache" => {
                options.quality_cache =
                    PathBuf::from(arguments.next().ok_or("--quality-cache requires a path")?);
            }
            "--probe-quality" => {
                options.probe_quality = true;
            }
            "--probe-date" => {
                options.probe_date =
                    Some(arguments.next().ok_or("--probe-date requires YYYY-MM-DD")?);
            }
            "--ffprobe" => {
                options.ffprobe =
                    PathBuf::from(arguments.next().ok_or("--ffprobe requires a path")?);
            }
            "--help" | "-h" => {
                return Ok(ParsedOptions::Help);
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }

    match (options.probe_quality, options.probe_date.as_deref()) {
        (true, Some(date)) if valid_iso_date(date) => {}
        (true, Some(_)) => return Err("--probe-date must be a valid YYYY-MM-DD date".to_owned()),
        (true, None) => {
            return Err("--probe-quality requires --probe-date YYYY-MM-DD".to_owned());
        }
        (false, Some(_)) => return Err("--probe-date requires --probe-quality".to_owned()),
        (false, None) => {}
    }
    Ok(ParsedOptions::Run(options))
}

/// Validates an explicit Gregorian maintenance-run date without time libraries.
fn valid_iso_date(value: &str) -> bool {
    let Some((year, rest)) = value.split_once('-') else {
        return false;
    };
    let Some((month, day)) = rest.split_once('-') else {
        return false;
    };
    if year.len() != 4 || month.len() != 2 || day.len() != 2 {
        return false;
    }
    let (Ok(year), Ok(month), Ok(day)) =
        (year.parse::<u16>(), month.parse::<u8>(), day.parse::<u8>())
    else {
        return false;
    };
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let maximum_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=maximum_day).contains(&day)
}

/// Returns stable help text for the NPR snapshot maintenance command.
fn usage() -> &'static str {
    "Usage: cargo run --locked --example update_npr_stations --features radio -- \
     [--input-dir DIR] [--output FILE] [--quality-cache FILE]\n\
     \x20      [--probe-quality --probe-date YYYY-MM-DD] [--ffprobe FILE]\n\n\
     --probe-quality  Probe final stream URLs with bounded ffprobe workers and\n\
     \x20                update the generated quality sidecar.\n\
     --probe-date     Record the actual UTC/local maintenance-run date.\n\
     --quality-cache  Read retained quality metadata from FILE in every mode.\n\
     --ffprobe        Use FILE as ffprobe (only launched with --probe-quality).\n"
}

/// Executes one snapshot update after argument parsing.
fn run(options: Options) -> Result<(), Box<dyn Error>> {
    let responses = if let Some(input_dir) = options.input_dir {
        read_responses(&input_dir)?
    } else {
        fetch_responses()?
    };
    let mut services = collect_services(responses);
    resolve_static_playlists(&mut services)?;
    let mut quality_cache = read_quality_cache(&options.quality_cache)?;
    if options.probe_quality {
        let probe_date = options
            .probe_date
            .as_deref()
            .expect("argument validation requires an explicit probe date");
        quality_cache =
            probe_service_quality(&services, &quality_cache, &options.ffprobe, probe_date);
        write_quality_cache(&options.quality_cache, &quality_cache)?;
    }
    let rendered = render_module(&services, &quality_cache);
    atomic_write(&options.output, rendered.as_bytes())?;
    println!(
        "wrote {} distinct NPR services to {}",
        services.len(),
        options.output.display()
    );
    Ok(())
}

/// Reads the deterministic quality sidecar without contacting any stream.
fn read_quality_cache(path: &Path) -> Result<QualityCache, Box<dyn Error>> {
    let payload = match fs::read(path) {
        Ok(payload) => payload,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(QualityCache::default());
        }
        Err(error) => return Err(error.into()),
    };
    if payload.len() > MAX_QUALITY_CACHE_JSON_BYTES {
        return Err(format!(
            "{} exceeds the {}-byte quality-cache limit",
            path.display(),
            MAX_QUALITY_CACHE_JSON_BYTES
        )
        .into());
    }
    let mut cache: QualityCache =
        serde_json::from_slice(&payload).map_err(|error| format!("{}: {error}", path.display()))?;
    if cache.format_version != QUALITY_CACHE_FORMAT_VERSION {
        return Err(format!(
            "{} has unsupported quality-cache format version {}",
            path.display(),
            cache.format_version
        )
        .into());
    }
    for (guid, quality) in &mut cache.services {
        quality.stream_url = normalize_stable_https_url(&quality.stream_url)
            .ok_or_else(|| format!("quality-cache service {guid} has an invalid stream URL"))?;
    }
    for (guid, quality) in &cache.services {
        validate_cached_quality(guid, quality)?;
    }
    Ok(cache)
}

/// Rejects malformed hand-edited quality facts before they reach generated Rust.
fn validate_cached_quality(guid: &str, quality: &CachedQuality) -> Result<(), Box<dyn Error>> {
    if guid.trim().is_empty() {
        return Err("quality-cache service GUID must not be empty".into());
    }
    if normalize_stable_https_url(&quality.stream_url).as_deref()
        != Some(quality.stream_url.as_str())
    {
        return Err(format!("quality-cache service {guid} has an unstable stream URL").into());
    }
    if quality.probed_on.trim().is_empty() {
        return Err(format!("quality-cache service {guid} has no probe date").into());
    }
    if quality
        .bitrate_kbps
        .is_some_and(|value| value == 0 || value > MAX_RADIO_BITRATE_KBPS)
    {
        return Err(format!("quality-cache service {guid} has an invalid bitrate").into());
    }
    if quality.sample_rate_hz.is_some_and(|value| {
        !(MIN_RADIO_SAMPLE_RATE_HZ..=MAX_RADIO_SAMPLE_RATE_HZ).contains(&value)
    }) {
        return Err(format!("quality-cache service {guid} has an invalid sample rate").into());
    }
    if quality
        .channels
        .is_some_and(|value| value == 0 || value > MAX_RADIO_CHANNELS)
    {
        return Err(format!("quality-cache service {guid} has an invalid channel count").into());
    }
    if !quality.has_quality() {
        return Err(format!("quality-cache service {guid} contains no quality facts").into());
    }
    Ok(())
}

/// Writes sorted quality JSON through a sibling temporary file.
fn write_quality_cache(path: &Path, cache: &QualityCache) -> Result<(), Box<dyn Error>> {
    let mut payload = serde_json::to_vec_pretty(cache)?;
    payload.push(b'\n');
    atomic_write(path, &payload)?;
    Ok(())
}

/// Replaces one generated file only after the complete new payload is durable.
fn atomic_write(path: &Path, payload: &[u8]) -> Result<(), Box<dyn Error>> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("{} has no UTF-8 file name", path.display()))?;
    let temporary = parent.join(format!(".{file_name}.tmp-{}", process::id()));
    fs::write(&temporary, payload)?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

/// Probes every selected stream with fixed parallelism while retaining old facts.
fn probe_service_quality(
    services: &BTreeMap<String, ServiceGroup>,
    existing: &QualityCache,
    ffprobe: &Path,
    probe_date: &str,
) -> QualityCache {
    let retained: BTreeMap<String, CachedQuality> = services
        .iter()
        .filter_map(|(guid, group)| {
            matching_cached_quality(existing, guid, &group.selected.audio.url)
                .cloned()
                .map(|quality| (guid.clone(), quality))
        })
        .collect();
    if services.is_empty() {
        return QualityCache {
            format_version: QUALITY_CACHE_FORMAT_VERSION,
            last_probe_attempt_on: Some(probe_date.to_owned()),
            services: retained,
        };
    }

    let tasks = services
        .iter()
        .map(|(guid, group)| {
            (
                guid.clone(),
                group.selected.audio.url.clone(),
                group.selected.audio.stream_kind,
            )
        })
        .collect::<VecDeque<_>>();
    let task_count = tasks.len();
    let queue = Arc::new(Mutex::new(tasks));
    let (sender, receiver) = mpsc::channel();
    thread::scope(|scope| {
        for _ in 0..QUALITY_PROBE_WORKERS.min(task_count) {
            let queue = Arc::clone(&queue);
            let sender = sender.clone();
            scope.spawn(move || {
                loop {
                    let task = queue.lock().ok().and_then(|mut queue| queue.pop_front());
                    let Some((guid, stream_url, stream_kind)) = task else {
                        break;
                    };
                    let result = probe_stream_quality(ffprobe, &stream_url, stream_kind);
                    if sender.send((guid, stream_url, result)).is_err() {
                        break;
                    }
                }
            });
        }
    });
    drop(sender);

    let mut quality = retained;
    let mut fresh = 0_usize;
    let mut failed = 0_usize;
    for (guid, stream_url, result) in receiver {
        match result {
            Ok(probed) => {
                let replacement = CachedQuality::from_probe(&stream_url, probe_date, probed);
                if replacement.has_quality() {
                    quality.insert(guid, replacement);
                    fresh = fresh.saturating_add(1);
                }
            }
            Err(error) => {
                failed = failed.saturating_add(1);
                eprintln!("quality probe failed for {guid}: {error}");
            }
        }
    }
    eprintln!(
        "quality probe completed: {fresh} fresh/updated, {failed} failed, {} retained total",
        quality.len()
    );
    QualityCache {
        format_version: QUALITY_CACHE_FORMAT_VERSION,
        last_probe_attempt_on: Some(probe_date.to_owned()),
        services: quality,
    }
}

/// Returns a retained record only when the service still uses its exact URL.
fn matching_cached_quality<'a>(
    cache: &'a QualityCache,
    guid: &str,
    stream_url: &str,
) -> Option<&'a CachedQuality> {
    cache
        .services
        .get(guid)
        .filter(|quality| quality.stream_url == stream_url && quality.has_quality())
}

/// Runs one shell-free, time-bounded `ffprobe` stream inspection.
fn probe_stream_quality(
    ffprobe: &Path,
    stream_url: &str,
    stream_kind: StreamKind,
) -> Result<ProbedQuality, String> {
    probe_stream_quality_with_timeout(ffprobe, stream_url, stream_kind, QUALITY_PROBE_TIMEOUT)
}

/// Runs one probe with an injectable wall deadline for timeout regressions.
fn probe_stream_quality_with_timeout(
    ffprobe: &Path,
    stream_url: &str,
    stream_kind: StreamKind,
    timeout: Duration,
) -> Result<ProbedQuality, String> {
    let mut child = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-rw_timeout",
            "8000000",
            "-probesize",
            "262144",
            "-analyzeduration",
            "3000000",
            "-select_streams",
            "a",
            "-show_entries",
            "stream=codec_name,bit_rate,sample_rate,channels:\
             format=bit_rate:format_tags=icy-br",
            "-of",
            "json",
        ])
        .arg(stream_url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not launch {}: {error}", ffprobe.display()))?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("{} stdout was not piped", ffprobe.display()));
    };
    // Drain concurrently so even a faulty helper cannot fill its pipe and
    // deadlock the `try_wait` loop. A channel deadline also prevents a
    // descendant retaining stdout from blocking cleanup after the child exits.
    let (output_sender, output_receiver) = mpsc::sync_channel(1);
    let _output_reader = thread::spawn(move || {
        let mut payload = Vec::new();
        let result = stdout
            .take(u64::try_from(MAX_QUALITY_PROBE_JSON_BYTES + 1).unwrap_or(u64::MAX))
            .read_to_end(&mut payload)
            .map(|_| payload);
        let _ = output_sender.send(result);
    });
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or("quality-probe deadline overflow")?;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{} timed out after {}s",
                    ffprobe.display(),
                    timeout.as_secs_f64()
                ));
            }
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                thread::sleep(remaining.min(QUALITY_PROBE_POLL_INTERVAL));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("could not wait for {}: {error}", ffprobe.display()));
            }
        }
    };
    if !status.success() {
        return Err(format!("{} returned {status}", ffprobe.display()));
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    let payload = output_receiver
        .recv_timeout(remaining)
        .map_err(|error| {
            format!(
                "{} output did not close before deadline: {error}",
                ffprobe.display()
            )
        })?
        .map_err(|error| format!("could not read {} output: {error}", ffprobe.display()))?;
    parse_ffprobe_quality(&payload, stream_kind)
}

/// Parses and bounds the audio-stream JSON requested by the probe command.
fn parse_ffprobe_quality(payload: &[u8], stream_kind: StreamKind) -> Result<ProbedQuality, String> {
    if payload.len() > MAX_QUALITY_PROBE_JSON_BYTES {
        return Err(format!(
            "ffprobe JSON exceeds the {MAX_QUALITY_PROBE_JSON_BYTES}-byte limit"
        ));
    }
    let output: FfprobeOutput = serde_json::from_slice(payload)
        .map_err(|error| format!("invalid ffprobe JSON: {error}"))?;
    let stream = match stream_kind {
        StreamKind::Direct => output.streams.first(),
        StreamKind::M3u => output
            .streams
            .iter()
            .max_by_key(|stream| probe_stream_bitrate(stream).unwrap_or_default()),
    }
    .ok_or("ffprobe returned no audio stream")?;
    let format = output.format.as_ref();
    let icy_bitrate = format
        .and_then(|format| case_insensitive_tag(&format.tags, "icy-br"))
        .and_then(parse_icy_bitrate);
    let stream_bitrate = probe_stream_bitrate(stream);
    let bitrate_kbps = match stream_kind {
        StreamKind::Direct => icy_bitrate.or(stream_bitrate).or_else(|| {
            format
                .and_then(|format| format.bit_rate.as_deref())
                .and_then(parse_bits_per_second)
        }),
        // HLS format bitrate can be the sum of every advertised variant, not
        // the bitrate of the highest audio representation selected above.
        StreamKind::M3u => stream_bitrate,
    };
    let probed = ProbedQuality {
        codec: stream.codec_name.as_deref().and_then(parse_probe_codec),
        bitrate_kbps,
        sample_rate_hz: stream
            .sample_rate
            .as_deref()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| (MIN_RADIO_SAMPLE_RATE_HZ..=MAX_RADIO_SAMPLE_RATE_HZ).contains(value)),
        channels: stream
            .channels
            .and_then(|value| u8::try_from(value).ok())
            .filter(|value| *value > 0 && *value <= MAX_RADIO_CHANNELS),
    };
    if !probed.has_quality() {
        return Err("ffprobe returned no supported quality fields".to_owned());
    }
    Ok(probed)
}

/// Returns one stream-level bitrate, excluding container or playlist totals.
fn probe_stream_bitrate(stream: &FfprobeStream) -> Option<u16> {
    stream.bit_rate.as_deref().and_then(parse_bits_per_second)
}

/// Maps codecs supported by [`RadioCodec`] without preserving arbitrary labels.
fn parse_probe_codec(value: &str) -> Option<Codec> {
    let codec = value.trim().to_ascii_lowercase();
    match codec.as_str() {
        "aac" => Some(Codec::Aac),
        "flac" => Some(Codec::Flac),
        "mp3" => Some(Codec::Mp3),
        "opus" => Some(Codec::Opus),
        "vorbis" => Some(Codec::Vorbis),
        _ if codec.starts_with("pcm_") => Some(Codec::Pcm),
        _ => None,
    }
}

/// Converts a bits-per-second value into rounded, bounded kilobits.
fn parse_bits_per_second(value: &str) -> Option<u16> {
    let bits_per_second = value.trim().parse::<u64>().ok()?;
    let kilobits = bits_per_second.saturating_add(500) / 1_000;
    u16::try_from(kilobits)
        .ok()
        .filter(|value| *value > 0 && *value <= MAX_RADIO_BITRATE_KBPS)
        .map(normalize_nominal_bitrate)
}

/// Parses the standard ICY nominal bitrate, which is expressed in kilobits.
fn parse_icy_bitrate(value: &str) -> Option<u16> {
    let digits = value.trim().chars().take_while(char::is_ascii_digit);
    let kilobits = digits.collect::<String>().parse::<u16>().ok()?;
    (kilobits > 0 && kilobits <= MAX_RADIO_BITRATE_KBPS)
        .then(|| normalize_nominal_bitrate(kilobits))
}

/// Snaps small probe-estimation drift to a conventional nominal bitrate.
fn normalize_nominal_bitrate(observed: u16) -> u16 {
    COMMON_RADIO_BITRATES_KBPS
        .iter()
        .copied()
        .min_by_key(|candidate| candidate.abs_diff(observed))
        .filter(|candidate| candidate.abs_diff(observed) <= 2)
        .unwrap_or(observed)
}

/// Finds an ffprobe container tag without depending on server key casing.
fn case_insensitive_tag<'a>(tags: &'a BTreeMap<String, String>, expected: &str) -> Option<&'a str> {
    tags.iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(expected))
        .map(|(_, value)| value.as_str())
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
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
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
    if is_streamtheworld_live_edge(&url) {
        return canonicalize_streamtheworld_live_edge(&url);
    }
    Some(url.into())
}

/// Identifies only one numeric edge label on the exact live-service suffix.
fn is_streamtheworld_live_edge(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let Some(edge) = host.strip_suffix(STREAMTHEWORLD_LIVE_SUFFIX) else {
        return false;
    };
    !edge.is_empty() && edge.bytes().all(|byte| byte.is_ascii_digit())
}

/// Replaces a rotating StreamTheWorld edge with its stable mount redirect.
fn canonicalize_streamtheworld_live_edge(url: &Url) -> Option<String> {
    if url.port().is_some() || url.fragment().is_some() {
        return None;
    }
    let mount = url.path().strip_prefix('/')?;
    if mount.is_empty()
        || mount.len() > 128
        || !mount
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        || !mount
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
    {
        return None;
    }
    let mut query = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    if query.iter().any(|(key, value)| {
        !matches!(
            key.to_ascii_lowercase().as_str(),
            "aw_0_1st.playerid" | "dist" | "ttag"
        ) || value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    }) {
        return None;
    }
    query.sort();
    let mut stable = Url::parse(&format!("{STREAMTHEWORLD_REDIRECT_BASE}{mount}")).ok()?;
    if !query.is_empty() {
        stable.query_pairs_mut().extend_pairs(query);
    }
    Some(stable.into())
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

fn render_module(
    services: &BTreeMap<String, ServiceGroup>,
    quality_cache: &QualityCache,
) -> String {
    let mut output = String::new();
    render_module_header(&mut output, services, quality_cache);
    for group in services.values() {
        render_station(&mut output, group, quality_cache);
    }
    output.push_str("];\n");
    output
}

/// Writes generated module documentation, imports, provenance, and constants.
fn render_module_header(
    output: &mut String,
    services: &BTreeMap<String, ServiceGroup>,
    quality_cache: &QualityCache,
) {
    let applied_quality_count = services
        .iter()
        .filter(|(guid, group)| {
            matching_cached_quality(quality_cache, guid, &group.selected.audio.url).is_some()
        })
        .count();
    let quality_probe_attempt_date = quality_cache
        .last_probe_attempt_on
        .as_deref()
        .map_or_else(|| "None".to_owned(), |date| format!("Some({date:?})"));
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
        "/// Date when the latest explicit bounded quality refresh was attempted.\npub \
         const NPR_STATION_QUALITY_LAST_PROBE_ATTEMPT_DATE: Option<&str> = \
         {quality_probe_attempt_date};"
    )
    .unwrap();
    writeln!(
        output,
        "/// Services carrying retained quality facts for their exact current stream URL.\npub \
         const NPR_STATION_QUALITY_SERVICE_COUNT: usize = {applied_quality_count};\n"
    )
    .unwrap();
    writeln!(
        output,
        "/// Static NPR member-station services available without a startup directory request.\n\
         pub const NPR_STATIONS: &[RadioStationPreset] = &["
    )
    .unwrap();
}

/// Writes one station preset with quality matching its exact selected URL.
fn render_station(output: &mut String, group: &ServiceGroup, quality_cache: &QualityCache) {
    let station = &group.selected;
    let name = display_name(station);
    let summary = summary(group);
    let quality = matching_cached_quality(quality_cache, &station.guid, &station.audio.url);
    let codec = match quality
        .and_then(|quality| quality.codec)
        .or(station.audio.codec)
    {
        Some(Codec::Aac) => "Some(RadioCodec::Aac)",
        Some(Codec::Flac) => "Some(RadioCodec::Flac)",
        Some(Codec::Mp3) => "Some(RadioCodec::Mp3)",
        Some(Codec::Opus) => "Some(RadioCodec::Opus)",
        Some(Codec::Pcm) => "Some(RadioCodec::Pcm)",
        Some(Codec::Vorbis) => "Some(RadioCodec::Vorbis)",
        None => "None",
    };
    let bitrate_kbps = format_optional_number(quality.and_then(|quality| quality.bitrate_kbps));
    let sample_rate_hz = format_optional_number(quality.and_then(|quality| quality.sample_rate_hz));
    let channels = format_optional_number(quality.and_then(|quality| quality.channels));
    let stream_kind = match station.audio.stream_kind {
        StreamKind::Direct => "RadioStreamKind::Direct",
        StreamKind::M3u => "RadioStreamKind::M3u",
    };
    writeln!(
        output,
        "    RadioStationPreset {{\n        id: {:?},\n        name: {:?},\n        \
         homepage: {:?},\n        stream: {:?},\n        summary: {:?},\n        codec: \
         {codec},\n        bitrate_kbps: {bitrate_kbps},\n        sample_rate_hz: \
         {sample_rate_hz},\n        channels: {channels},\n        stream_kind: \
         {stream_kind},\n        now_playing: \
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

/// Formats one numeric option as deterministic Rust source.
fn format_optional_number(value: Option<impl std::fmt::Display>) -> String {
    value.map_or_else(|| "None".to_owned(), |value| format!("Some({value})"))
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
        let rendered = render_module(&services, &QualityCache::default());
        assert!(rendered.contains("bitrate_kbps: None"));
        assert!(!rendered.contains("bitrate_kbps: Some"));
        assert!(
            rendered.contains("NPR_STATION_QUALITY_LAST_PROBE_ATTEMPT_DATE: Option<&str> = None")
        );
        assert!(rendered.contains("NPR_STATION_QUALITY_SERVICE_COUNT: usize = 0"));
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

    #[test]
    fn rotating_streamtheworld_edges_share_one_stable_mount_url() {
        let first = normalize_stable_https_url(
            "https://16603.live.streamtheworld.com/WQCSFM_SC?dist=NPR&_ic2=rotating",
        );
        let second =
            normalize_stable_https_url("https://26233.live.streamtheworld.com/WQCSFM_SC?dist=NPR");
        let expected = Some(
            "https://playerservices.streamtheworld.com/api/livestream-redirect/\
             WQCSFM_SC?dist=NPR"
                .replace(char::is_whitespace, ""),
        );

        assert_eq!(first, expected);
        assert_eq!(second, expected);
    }

    #[test]
    fn streamtheworld_canonicalization_rejects_unsafe_authority_and_paths() {
        for url in [
            "https://user@16603.live.streamtheworld.com/WQCSFM_SC",
            "https://16603.live.streamtheworld.com:8443/WQCSFM_SC",
            "https://16603.live.streamtheworld.com/",
            "https://16603.live.streamtheworld.com/one/two",
            "https://16603.live.streamtheworld.com/%2e%2e",
            "https://16603.live.streamtheworld.com/WQCSFM_SC#fragment",
            "https://16603.live.streamtheworld.com/WQCSFM_SC?redirect=evil.example",
        ] {
            assert_eq!(normalize_stable_https_url(url), None, "{url}");
        }
    }

    #[test]
    fn streamtheworld_suffix_attacks_and_unknown_edges_are_not_canonicalized() {
        for url in [
            "https://16603.live.streamtheworld.com.evil.example/WQCSFM_SC",
            "https://evil.16603.live.streamtheworld.com/WQCSFM_SC",
            "https://edge.live.streamtheworld.com/WQCSFM_SC",
        ] {
            let normalized = normalize_stable_https_url(url).unwrap();
            assert_eq!(normalized, url);
            assert!(!normalized.starts_with(STREAMTHEWORLD_REDIRECT_BASE));
        }
    }

    #[test]
    fn ffprobe_quality_corrects_inferred_codec_and_prefers_icy_nominal_bitrate() {
        let quality = parse_ffprobe_quality(
            br#"{
              "streams": [{
                "codec_name": "aac", "bit_rate": "80390",
                "sample_rate": "48000", "channels": 2
              }],
              "format": {
                "bit_rate": "80390",
                "tags": {"ICY-BR": "24"}
              }
            }"#,
            StreamKind::Direct,
        )
        .unwrap();

        assert_eq!(
            quality,
            ProbedQuality {
                codec: Some(Codec::Aac),
                bitrate_kbps: Some(24),
                sample_rate_hz: Some(48_000),
                channels: Some(2),
            }
        );
    }

    #[test]
    fn exact_pcm_probe_overrides_an_inferred_mp3_directory_label() {
        let quality = parse_ffprobe_quality(
            br#"{
              "streams": [{
                "codec_name": "pcm_s16le", "bit_rate": "3072000",
                "sample_rate": "48000", "channels": 2
              }],
              "format": {"bit_rate": "3072000"}
            }"#,
            StreamKind::Direct,
        )
        .unwrap();
        assert_eq!(
            quality,
            ProbedQuality {
                codec: Some(Codec::Pcm),
                bitrate_kbps: Some(3_072),
                sample_rate_hz: Some(48_000),
                channels: Some(2),
            }
        );

        let response: StationResponse = serde_json::from_str(MOCK_RESPONSE).unwrap();
        let services = collect_services(vec![response]);
        let station = &services["primary-guid"];
        assert_eq!(station.selected.audio.codec, Some(Codec::Mp3));
        let cache = QualityCache {
            format_version: QUALITY_CACHE_FORMAT_VERSION,
            last_probe_attempt_on: Some("2026-07-28".to_owned()),
            services: BTreeMap::from([(
                "primary-guid".to_owned(),
                CachedQuality::from_probe(&station.selected.audio.url, "2026-07-28", quality),
            )]),
        };
        let mut rendered = String::new();
        render_station(&mut rendered, station, &cache);

        assert!(rendered.contains("codec: Some(RadioCodec::Pcm)"));
        assert!(!rendered.contains("codec: Some(RadioCodec::Mp3)"));
        assert!(rendered.contains("bitrate_kbps: Some(3072)"));
    }

    #[test]
    fn ffprobe_bitrate_uses_stream_then_format_fallback_and_rounds_kilobits() {
        let stream = parse_ffprobe_quality(
            br#"{
              "streams": [{"codec_name": "aac", "bit_rate": "64040"}],
              "format": {"bit_rate": "127600"}
            }"#,
            StreamKind::Direct,
        )
        .unwrap();
        let format = parse_ffprobe_quality(
            br#"{
              "streams": [{"codec_name": "mp3"}],
              "format": {"bit_rate": "127600"}
            }"#,
            StreamKind::Direct,
        )
        .unwrap();

        assert_eq!(stream.bitrate_kbps, Some(64));
        assert_eq!(format.bitrate_kbps, Some(128));
        assert_eq!(parse_bits_per_second("999"), Some(1));
        assert_eq!(parse_bits_per_second("10000500"), None);
        assert_eq!(parse_icy_bitrate("320 kbps"), Some(320));
        for (observed, nominal) in [
            ("23000", 24),
            ("63000", 64),
            ("127000", 128),
            ("162000", 160),
            ("194000", 192),
            ("319000", 320),
        ] {
            assert_eq!(parse_bits_per_second(observed), Some(nominal));
        }
        assert_eq!(parse_bits_per_second("150000"), Some(150));
    }

    #[test]
    fn adaptive_hls_uses_highest_stream_bitrate_and_never_the_format_total() {
        let quality = parse_ffprobe_quality(
            br#"{
              "streams": [
                {
                  "codec_name": "aac", "bit_rate": "64000",
                  "sample_rate": "44100", "channels": 2
                },
                {
                  "codec_name": "aac", "bit_rate": "128000",
                  "sample_rate": "48000", "channels": 2
                }
              ],
              "format": {"bit_rate": "192000", "tags": {"icy-br": "320"}}
            }"#,
            StreamKind::M3u,
        )
        .unwrap();
        let missing_variant_rate = parse_ffprobe_quality(
            br#"{
              "streams": [{"codec_name": "aac", "sample_rate": "48000", "channels": 2}],
              "format": {"bit_rate": "192000"}
            }"#,
            StreamKind::M3u,
        )
        .unwrap();

        assert_eq!(quality.bitrate_kbps, Some(128));
        assert_eq!(quality.sample_rate_hz, Some(48_000));
        assert_eq!(missing_variant_rate.bitrate_kbps, None);
    }

    #[test]
    fn ffprobe_parser_rejects_malformed_oversized_and_no_audio_payloads() {
        assert!(parse_ffprobe_quality(b"{broken", StreamKind::Direct).is_err());
        assert!(parse_ffprobe_quality(br#"{"streams":[]}"#, StreamKind::Direct).is_err());
        assert!(
            parse_ffprobe_quality(
                br#"{"streams":[{"codec_name":"unmapped","bit_rate":"invalid"}]}"#,
                StreamKind::Direct,
            )
            .is_err()
        );
        assert!(
            parse_ffprobe_quality(
                &vec![b' '; MAX_QUALITY_PROBE_JSON_BYTES + 1],
                StreamKind::Direct
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn oversized_helper_output_is_drained_without_filling_the_child_pipe() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let helper = directory.path().join("mock-ffprobe");
        fs::write(
            &helper,
            "#!/bin/sh\ndd if=/dev/zero bs=65537 count=1 2>/dev/null\n",
        )
        .unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();

        let error = probe_stream_quality(&helper, "https://example.test/live", StreamKind::Direct)
            .expect_err("oversize");

        assert!(error.contains("exceeds"));
    }

    #[cfg(unix)]
    #[test]
    fn probe_timeout_kills_the_direct_helper_without_blocking_cleanup() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let helper = directory.path().join("mock-ffprobe");
        fs::write(&helper, "#!/bin/sh\nwhile :; do :; done\n").unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();

        let started = Instant::now();
        let error = probe_stream_quality_with_timeout(
            &helper,
            "https://example.test/live",
            StreamKind::Direct,
            Duration::from_millis(100),
        )
        .expect_err("timeout");

        assert!(error.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn inherited_stdout_cannot_outlive_the_probe_deadline() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let helper = directory.path().join("mock-ffprobe");
        fs::write(
            &helper,
            "#!/bin/sh\n\
             sleep 1 &\n\
             printf '%s' '{\"streams\":[{\"codec_name\":\"mp3\",\"bit_rate\":\"128000\"}]}'\n",
        )
        .unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();

        let started = Instant::now();
        let error = probe_stream_quality_with_timeout(
            &helper,
            "https://example.test/live",
            StreamKind::Direct,
            Duration::from_millis(100),
        )
        .expect_err("inherited stdout");

        assert!(error.contains("output did not close before deadline"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn retained_quality_is_applied_only_to_the_exact_current_stream_url() {
        let response: StationResponse = serde_json::from_str(MOCK_RESPONSE).unwrap();
        let services = collect_services(vec![response]);
        let cache = QualityCache {
            format_version: QUALITY_CACHE_FORMAT_VERSION,
            last_probe_attempt_on: Some("2026-07-28".to_owned()),
            services: BTreeMap::from([(
                "primary-guid".to_owned(),
                CachedQuality {
                    stream_url: "https://example.test/live.mp3".to_owned(),
                    probed_on: "2026-07-28".to_owned(),
                    codec: Some(Codec::Aac),
                    bitrate_kbps: Some(24),
                    sample_rate_hz: Some(48_000),
                    channels: Some(2),
                },
            )]),
        };

        let rendered = render_module(&services, &cache);
        assert!(rendered.contains(
            "NPR_STATION_QUALITY_LAST_PROBE_ATTEMPT_DATE: Option<&str> = Some(\"2026-07-28\")"
        ));
        assert!(rendered.contains("NPR_STATION_QUALITY_SERVICE_COUNT: usize = 1"));
        assert!(rendered.contains("codec: Some(RadioCodec::Aac)"));
        assert!(rendered.contains("bitrate_kbps: Some(24)"));
        assert!(rendered.contains("sample_rate_hz: Some(48000)"));
        assert!(rendered.contains("channels: Some(2)"));

        let mut changed = cache;
        changed.services.get_mut("primary-guid").unwrap().stream_url =
            "https://example.test/old.mp3".to_owned();
        let rendered = render_module(&services, &changed);
        assert!(rendered.contains("NPR_STATION_QUALITY_SERVICE_COUNT: usize = 0"));
        assert!(!rendered.contains("bitrate_kbps: Some(24)"));
    }

    #[test]
    fn successful_partial_probe_replaces_the_whole_previous_record() {
        let replacement = CachedQuality::from_probe(
            "https://example.test/live",
            "2026-07-29",
            ProbedQuality {
                codec: Some(Codec::Aac),
                sample_rate_hz: Some(48_000),
                ..ProbedQuality::default()
            },
        );

        assert_eq!(replacement.codec, Some(Codec::Aac));
        assert_eq!(replacement.bitrate_kbps, None);
        assert_eq!(replacement.sample_rate_hz, Some(48_000));
        assert_eq!(replacement.channels, None);
        assert_eq!(replacement.probed_on, "2026-07-29");
    }

    #[test]
    fn unavailable_probe_helper_retains_matching_cache_and_marks_the_attempt_date() {
        let response: StationResponse = serde_json::from_str(MOCK_RESPONSE).unwrap();
        let services = collect_services(vec![response]);
        let existing = QualityCache {
            format_version: QUALITY_CACHE_FORMAT_VERSION,
            last_probe_attempt_on: Some("2026-07-27".to_owned()),
            services: BTreeMap::from([(
                "primary-guid".to_owned(),
                CachedQuality {
                    stream_url: "https://example.test/live.mp3".to_owned(),
                    probed_on: "2026-07-27".to_owned(),
                    codec: Some(Codec::Mp3),
                    bitrate_kbps: Some(128),
                    sample_rate_hz: Some(48_000),
                    channels: Some(2),
                },
            )]),
        };
        let missing = Path::new("/definitely/missing/youta-test-ffprobe");

        let retained = probe_service_quality(&services, &existing, missing, "2026-07-29");

        assert_eq!(
            retained.last_probe_attempt_on.as_deref(),
            Some("2026-07-29")
        );
        assert_eq!(retained.services.len(), 1);
        assert_eq!(retained.services["primary-guid"].probed_on, "2026-07-27");
        assert_eq!(retained.services["primary-guid"].bitrate_kbps, Some(128));
    }

    #[cfg(unix)]
    #[test]
    fn individual_probe_failure_retains_matching_verified_quality() {
        use std::os::unix::fs::PermissionsExt as _;

        let response: StationResponse = serde_json::from_str(MOCK_RESPONSE).unwrap();
        let services = collect_services(vec![response]);
        let existing = QualityCache {
            format_version: QUALITY_CACHE_FORMAT_VERSION,
            last_probe_attempt_on: Some("2026-07-27".to_owned()),
            services: BTreeMap::from([(
                "primary-guid".to_owned(),
                CachedQuality {
                    stream_url: "https://example.test/live.mp3".to_owned(),
                    probed_on: "2026-07-27".to_owned(),
                    codec: Some(Codec::Mp3),
                    bitrate_kbps: Some(128),
                    sample_rate_hz: Some(48_000),
                    channels: Some(2),
                },
            )]),
        };
        let directory = tempfile::tempdir().unwrap();
        let helper = directory.path().join("mock-ffprobe");
        fs::write(
            &helper,
            "#!/bin/sh\n[ \"$1\" = '-version' ] && exit 0\nexit 9\n",
        )
        .unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();

        let retained = probe_service_quality(&services, &existing, &helper, "2026-07-29");

        assert_eq!(
            retained.last_probe_attempt_on.as_deref(),
            Some("2026-07-29")
        );
        assert_eq!(retained.services.len(), 1);
        assert_eq!(retained.services["primary-guid"].probed_on, "2026-07-27");
        assert_eq!(retained.services["primary-guid"].bitrate_kbps, Some(128));
    }

    #[test]
    fn quality_sidecar_is_sorted_bounded_and_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quality.json");
        let cache = QualityCache {
            format_version: QUALITY_CACHE_FORMAT_VERSION,
            last_probe_attempt_on: Some("2026-07-28".to_owned()),
            services: BTreeMap::from([
                (
                    "z-guid".to_owned(),
                    CachedQuality {
                        stream_url: "https://example.test/z".to_owned(),
                        probed_on: "2026-07-28".to_owned(),
                        codec: Some(Codec::Mp3),
                        ..CachedQuality::default()
                    },
                ),
                (
                    "a-guid".to_owned(),
                    CachedQuality {
                        stream_url: "https://example.test/a".to_owned(),
                        probed_on: "2026-07-28".to_owned(),
                        codec: Some(Codec::Aac),
                        ..CachedQuality::default()
                    },
                ),
            ]),
        };

        write_quality_cache(&path, &cache).unwrap();
        let payload = fs::read_to_string(&path).unwrap();
        assert!(payload.find("\"a-guid\"").unwrap() < payload.find("\"z-guid\"").unwrap());
        assert_eq!(read_quality_cache(&path).unwrap(), cache);

        fs::write(&path, vec![b' '; MAX_QUALITY_CACHE_JSON_BYTES + 1]).unwrap();
        assert!(read_quality_cache(&path).is_err());
    }

    #[test]
    fn quality_sidecar_normalizes_legacy_rotating_edges_before_exact_matching() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quality.json");
        fs::write(
            &path,
            br#"{
              "format_version": 1,
              "services": {
                "primary-guid": {
                  "stream_url": "https://16603.live.streamtheworld.com/WQCSFM_SC?dist=NPR",
                  "probed_on": "2026-07-28",
                  "codec": "mp3"
                }
              }
            }"#,
        )
        .unwrap();

        let cache = read_quality_cache(&path).unwrap();
        let stable = "https://playerservices.streamtheworld.com/api/livestream-redirect/\
                      WQCSFM_SC?dist=NPR"
            .replace(char::is_whitespace, "");

        assert_eq!(cache.services["primary-guid"].stream_url, stable);
        assert!(matching_cached_quality(&cache, "primary-guid", &stable).is_some());
    }

    #[test]
    fn command_line_requires_explicit_quality_probing_and_documents_options() {
        assert_eq!(
            parse_options(Vec::new()).unwrap(),
            ParsedOptions::Run(Options::default())
        );
        assert_eq!(
            parse_options(["--help".to_owned()]).unwrap(),
            ParsedOptions::Help
        );
        let ParsedOptions::Run(options) = parse_options([
            "--input-dir".to_owned(),
            "fixtures".to_owned(),
            "--output".to_owned(),
            "snapshot.rs".to_owned(),
            "--quality-cache".to_owned(),
            "quality.json".to_owned(),
            "--probe-quality".to_owned(),
            "--probe-date".to_owned(),
            "2026-07-29".to_owned(),
            "--ffprobe".to_owned(),
            "/usr/bin/ffprobe".to_owned(),
        ])
        .unwrap() else {
            panic!("expected runnable options");
        };
        assert!(options.probe_quality);
        assert_eq!(options.probe_date.as_deref(), Some("2026-07-29"));
        assert_eq!(options.quality_cache, PathBuf::from("quality.json"));
        assert_eq!(options.ffprobe, PathBuf::from("/usr/bin/ffprobe"));
        assert!(parse_options(["--probe-quality".to_owned()]).is_err());
        assert!(
            parse_options([
                "--probe-quality".to_owned(),
                "--probe-date".to_owned(),
                "2025-02-29".to_owned(),
            ])
            .is_err()
        );
        assert!(
            parse_options([
                "--probe-quality".to_owned(),
                "--probe-date".to_owned(),
                "2024-02-29".to_owned(),
            ])
            .is_ok()
        );
        assert!(parse_options(["--probe-date".to_owned(), "2026-07-29".to_owned(),]).is_err());
        assert!(parse_options(["--output".to_owned()]).is_err());
        assert!(parse_options(["--unknown".to_owned()]).is_err());
        assert!(usage().contains("--probe-quality"));
        assert!(usage().contains("--probe-date"));
        assert!(usage().contains("--quality-cache"));
        assert!(usage().contains("--ffprobe"));
    }
}
