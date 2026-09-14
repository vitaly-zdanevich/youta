//! Bounded validation and lossless publication of explicitly complete mpv Opus caches.

use crate::config::Config;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Mirrors the exporter's conservative player-stall ceiling.
const MAX_CACHE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_STAGING_BYTES: u64 = MAX_CACHE_BYTES * 3;
const MAX_PROCESS_OUTPUT: usize = 64 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 16 * 1024;
const MAX_PACKETS: u64 = 1_000_000;
const MAX_DURATION: f64 = 6.0 * 60.0 * 60.0;
const VALIDATION_DEADLINE: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(10);
// WebM timestamps have millisecond precision. This remains below the smallest
// Opus packet duration (2.5ms), so a missing packet cannot hide in the tolerance.
const TIMESTAMP_TOLERANCE: f64 = 0.0021;
const OPUS_RATE: f64 = 48_000.0;

/// A validated private Opus artifact, not yet visible as a completed download.
#[derive(Debug)]
pub struct PreparedCacheDownload {
    _directory: tempfile::TempDir,
    path: PathBuf,
    destination: PathBuf,
    extension: &'static str,
}

impl PreparedCacheDownload {
    /// Returns the verified Opus file while this guard retains its private directory.
    #[cfg(test)]
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Publishes with one same-filesystem, no-replacement operation after the
    /// controller has rechecked playback identity and cancellation.
    ///
    /// The large copy, remux and durability flush already happened on the worker.
    /// No helper or media copying runs here.
    ///
    /// # Errors
    /// Returns an error without replacing existing files if the destination
    /// changed, the name is unavailable, or atomic hard-link publication fails.
    pub fn publish(self, destination: &Path, title: &str, id: &str) -> Result<PathBuf, String> {
        let destination = crate::fs_path::canonicalize(destination)
            .map_err(|_| "cache download destination is unavailable")?;
        if destination != self.destination {
            return Err("cache download destination changed".to_owned());
        }
        regular_size(&self.path, MAX_CACHE_BYTES)?;
        let title = filename_component(title, 140, "media");
        let id = filename_component(id, 60, "cache");
        for collision in 0..1000 {
            let suffix = if collision == 0 {
                String::new()
            } else {
                format!(" ({collision})")
            };
            let path = destination.join(format!("{title} [{id}]{suffix}.{}", self.extension));
            match std::fs::hard_link(&self.path, &path) {
                Ok(()) => return Ok(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => {
                    return Err(
                        "cache download could not be published without replacement".to_owned()
                    );
                }
            }
        }
        Err("cache download has too many conflicting filenames".to_owned())
    }
}

/// Validates a completed, identity-pinned mpv cache and remuxes Opus without encoding.
///
/// Call only on a worker, retaining the original cache-export guard throughout.
/// Expected duration must be mpv's precise duration, not rounded catalogue data.
/// Opus identifies the codec: Ogg remains .opus and WebM remains .webm so that
/// millisecond container timestamps cannot alter exact end padding during remux.
/// The controller must recheck export identity immediately before publication.
///
/// # Errors
/// Unsupported, incomplete, changed, canceled, oversized or malformed caches are
/// cache misses: the caller may start its ordinary download instead.
pub fn prepare_cached_opus(
    config: &Config,
    source: &Path,
    expected_duration: Duration,
    destination: &Path,
    cancellation: &Arc<AtomicBool>,
) -> Result<PreparedCacheDownload, String> {
    let started = Instant::now();
    check_work(cancellation, started)?;
    let expected = expected_duration.as_secs_f64();
    if expected <= 0.0 || expected > MAX_DURATION {
        return Err("cache duration is unavailable or unsupported".to_owned());
    }
    let destination = crate::fs_path::canonicalize(destination)
        .map_err(|_| "cache download destination is unavailable")?;
    if !destination.is_dir() {
        return Err("cache download destination is not a directory".to_owned());
    }
    regular_size(source, MAX_CACHE_BYTES)?;
    let input_format = cache_input_format(source)?;
    let mut builder = tempfile::Builder::new();
    builder.prefix(".youta-playback-cache-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let directory = builder
        .tempdir_in(&destination)
        .map_err(|_| "cannot create private cache validation directory")?;
    let input = directory.path().join(if input_format == "ogg" {
        "cached.opus"
    } else {
        "cached.webm"
    });
    copy_cache(source, &input, cancellation, started)?;
    let before = probe(
        config,
        &input,
        directory.path(),
        expected,
        cancellation,
        started,
    )?;
    let extension = if input_format == "ogg" {
        "opus"
    } else {
        "webm"
    };
    let output = directory.path().join(format!("verified.{extension}"));
    let mut remux = Command::new(&config.providers.ffmpeg_executable);
    remux
        .args([
            "-nostdin",
            "-hide_banner",
            "-v",
            "error",
            "-xerror",
            "-n",
            "-max_alloc",
            "67108864",
            "-protocol_whitelist",
            "file,pipe",
            "-f",
            input_format,
            "-i",
        ])
        .arg(&input)
        .args([
            "-map", "0:a:0", "-vn", "-sn", "-dn", "-c:a", "copy", "-f", extension,
        ])
        .arg(&output);
    run_helper(remux, false, directory.path(), cancellation, started)?;
    regular_size(&output, MAX_CACHE_BYTES)?;
    let after = probe(
        config,
        &output,
        directory.path(),
        expected,
        cancellation,
        started,
    )?;
    if before != after {
        return Err("cache remux changed encoded packets or decoded samples".to_owned());
    }
    // On Windows FlushFileBuffers requires a writable handle.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&output)
        .and_then(|file| file.sync_all())
        .map_err(|_| "cannot finish verified cache download")?;
    check_work(cancellation, started)?;
    Ok(PreparedCacheDownload {
        _directory: directory,
        path: output,
        destination,
        extension,
    })
}

/// Restricts the only user-controlled components of the final filename.
pub(crate) fn filename_component(value: &str, max_bytes: usize, fallback: &str) -> String {
    let mut result = String::new();
    for character in value.chars() {
        if result.len().saturating_add(character.len_utf8()) > max_bytes {
            break;
        }
        result.push(
            if character.is_control()
                || matches!(
                    character,
                    '/' | '\\' | ':' | '"' | '<' | '>' | '|' | '?' | '*'
                )
            {
                '_'
            } else {
                character
            },
        );
    }
    let result = result.trim().trim_matches('.').trim();
    if result.is_empty() {
        fallback.to_owned()
    } else {
        result.to_owned()
    }
}

fn check_work(cancellation: &AtomicBool, started: Instant) -> Result<(), String> {
    if cancellation.load(Ordering::Relaxed) {
        return Err("cache download canceled".to_owned());
    }
    if started.elapsed() >= VALIDATION_DEADLINE {
        return Err("cache validation timed out".to_owned());
    }
    Ok(())
}

/// Rejects links and special files before any potentially blocking open.
fn regular_size(path: &Path, limit: u64) -> Result<u64, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| "cache file is unavailable")?;
    if !metadata.file_type().is_file() || metadata.len() == 0 || metadata.len() > limit {
        return Err("cache file is nonregular, empty or exceeds its size limit".to_owned());
    }
    Ok(metadata.len())
}

/// Pins the exported file and copies only bounded bytes into same-device staging.
fn copy_cache(
    source: &Path,
    destination: &Path,
    cancellation: &AtomicBool,
    started: Instant,
) -> Result<(), String> {
    #[cfg(unix)]
    let mut input = std::fs::File::from(
        rustix::fs::open(
            source,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| "cannot safely open exported cache")?,
    );
    #[cfg(not(unix))]
    let mut input = std::fs::File::open(source).map_err(|_| "cannot open exported cache")?;
    let before = input
        .metadata()
        .map_err(|_| "cannot inspect exported cache")?;
    if !before.is_file() || before.len() == 0 || before.len() > MAX_CACHE_BYTES {
        return Err("cache input is not a bounded regular file".to_owned());
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut output = crate::private_files::open_privately(&mut options)
        .open(destination)
        .map_err(|_| "cannot create private cache copy")?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut copied = 0_u64;
    loop {
        check_work(cancellation, started)?;
        let count = input
            .read(&mut buffer)
            .map_err(|_| "cannot read exported cache")?;
        if count == 0 {
            break;
        }
        copied = copied
            .checked_add(count as u64)
            .ok_or("cache size overflow")?;
        if copied > MAX_CACHE_BYTES {
            return Err("cache size limit exceeded".to_owned());
        }
        output
            .write_all(&buffer[..count])
            .map_err(|_| "cannot copy exported cache")?;
    }
    let after = input
        .metadata()
        .map_err(|_| "cannot recheck exported cache")?;
    if copied != before.len()
        || after.len() != before.len()
        || before.modified().ok() != after.modified().ok()
    {
        return Err("cache file changed during validation".to_owned());
    }
    output
        .flush()
        .map_err(|_| "cannot finish private cache copy")?;
    check_work(cancellation, started)
}

/// Only these invariant values may survive a stream-copy remux.
#[derive(Debug, PartialEq, Eq)]
struct OpusProof {
    packet_digest: [u8; 32],
    codec_header_digest: [u8; 32],
    packets: u64,
    decoded_samples: u64,
    channels: u32,
}

/// Reduces probe lines as they arrive; large inputs never build a JSON tree.
#[derive(Default)]
struct ProbeAccumulator {
    digest: Sha256,
    packets: u64,
    frames: u64,
    samples: u64,
    first_skip: u64,
    codec_header: Option<Vec<u8>>,
    packet_start: Option<f64>,
    packet_end: Option<f64>,
    packet_duration: f64,
    frame_start: Option<f64>,
    frame_end: Option<f64>,
    streams: usize,
    channels: u32,
}

impl ProbeAccumulator {
    fn line(&mut self, line: &str) -> Result<(), String> {
        match line.split('|').next().unwrap_or_default() {
            "packet" => {
                if integer(line, "stream_index")? != 0
                    || field(line, "flags")?.is_some_and(|flags| flags.contains('C'))
                {
                    return Err("cache contains corrupt or unexpected packets".to_owned());
                }
                let start = timestamp(line, "pts_time")?;
                let duration = timestamp(line, "duration_time")?;
                if !(0.0..=0.1201).contains(&duration) || duration == 0.0 {
                    return Err("cache packet duration is unavailable".to_owned());
                }
                if let Some(previous) = self.packet_end {
                    if (start - previous).abs() > TIMESTAMP_TOLERANCE {
                        return Err("cache packet timeline has a gap or overlap".to_owned());
                    }
                } else {
                    self.packet_start = Some(start);
                    self.first_skip = line
                        .split('|')
                        .filter_map(|part| part.split_once('='))
                        .find(|(key, _)| *key == "skip_samples" || key.ends_with(":skip_samples"))
                        .map(|(_, value)| value.parse::<u64>())
                        .transpose()
                        .map_err(|_| "cache has invalid Opus pre-skip")?
                        .unwrap_or(0);
                    if self.first_skip > 65_535 {
                        return Err("cache Opus pre-skip is invalid".to_owned());
                    }
                }
                let hash = field(line, "data_hash")?
                    .ok_or("cache packet fingerprint is missing")?
                    .strip_prefix("SHA256:")
                    .ok_or("cache packet fingerprint is invalid")?;
                if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err("cache packet fingerprint is invalid".to_owned());
                }
                self.digest.update(hash.as_bytes());
                self.packets += 1;
                if self.packets > MAX_PACKETS {
                    return Err("cache packet count limit exceeded".to_owned());
                }
                self.packet_duration += duration;
                self.packet_end = Some(start + duration);
            }
            "frame" => {
                if integer(line, "stream_index")? != 0 {
                    return Err("cache has unexpected decoded streams".to_owned());
                }
                let start = timestamp(line, "best_effort_timestamp_time")?;
                let samples = integer(line, "nb_samples")?;
                if samples == 0 || samples > 5760 {
                    return Err("cache decoded frame is invalid".to_owned());
                }
                if let Some(previous) = self.frame_end {
                    if (start - previous).abs() > TIMESTAMP_TOLERANCE {
                        return Err("cache decoded audio timeline has a gap or overlap".to_owned());
                    }
                } else {
                    self.frame_start = Some(start);
                }
                self.samples = self
                    .samples
                    .checked_add(samples)
                    .ok_or("cache sample count overflow")?;
                self.frames += 1;
                if self.frames > MAX_PACKETS {
                    return Err("cache decoded frame count limit exceeded".to_owned());
                }
                self.frame_end = Some(start + samples as f64 / OPUS_RATE);
            }
            "stream" => {
                self.streams += 1;
                if self.streams != 1
                    || integer(line, "index")? != 0
                    || field(line, "codec_name")? != Some("opus")
                    || field(line, "codec_type")? != Some("audio")
                    || integer(line, "sample_rate")? != 48_000
                {
                    return Err("cache is not one finite Opus audio stream".to_owned());
                }
                let channels = integer(line, "channels")?;
                if !(1..=8).contains(&channels) {
                    return Err("cache Opus channel layout is unsupported".to_owned());
                }
                self.channels = channels as u32;
                let header = opus_header(line)?;
                if u64::from(header[9]) != channels {
                    return Err("cache Opus channel metadata is inconsistent".to_owned());
                }
                let skip = u64::from(u16::from_le_bytes([header[10], header[11]]));
                if self.first_skip != 0 && self.first_skip != skip {
                    return Err("cache Opus pre-skip metadata is inconsistent".to_owned());
                }
                self.first_skip = skip;
                self.codec_header = Some(header);
            }
            "" => {}
            _ => return Err("cache probe returned unexpected records".to_owned()),
        }
        Ok(())
    }

    fn finish(self, expected: f64) -> Result<OpusProof, String> {
        if self.streams != 1 || self.packets == 0 || self.frames == 0 || self.samples == 0 {
            return Err("cache contains no complete decoded Opus stream".to_owned());
        }
        // Ogg/WebM duration can include codec pre-skip, while decoded samples
        // exclude it. Never tolerate an entire missing Opus packet.
        let header = self
            .codec_header
            .ok_or("cache Opus header is unavailable")?;
        let origin_limit = self.first_skip as f64 / OPUS_RATE + TIMESTAMP_TOLERANCE;
        if self
            .packet_start
            .is_none_or(|start| start.abs() > origin_limit)
            || self
                .frame_start
                .is_none_or(|start| start.abs() > origin_limit)
        {
            return Err("cache does not begin at the audio origin".to_owned());
        }
        let packet_span = self
            .packet_end
            .zip(self.packet_start)
            .map(|(end, start)| end - start)
            .ok_or("cache packet timeline is missing")?;
        let frame_span = self
            .frame_end
            .zip(self.frame_start)
            .map(|(end, start)| end - start)
            .ok_or("cache decoded timeline is missing")?;
        if (packet_span - self.packet_duration).abs() > TIMESTAMP_TOLERANCE
            || (frame_span - self.samples as f64 / OPUS_RATE).abs() > TIMESTAMP_TOLERANCE
        {
            return Err("cache timeline contains accumulated gaps or overlaps".to_owned());
        }
        let with_preskip = (self.samples + self.first_skip) as f64 / OPUS_RATE;
        if (expected - with_preskip).abs() > TIMESTAMP_TOLERANCE {
            return Err("cache decoded duration is incomplete".to_owned());
        }
        Ok(OpusProof {
            packet_digest: self.digest.finalize().into(),
            codec_header_digest: Sha256::digest(&header).into(),
            packets: self.packets,
            decoded_samples: self.samples,
            channels: self.channels,
        })
    }
}

/// The exporter creates only these two fixed container types.
fn cache_input_format(path: &Path) -> Result<&'static str, String> {
    match path.extension().and_then(|value| value.to_str()) {
        Some("opus") => Ok("ogg"),
        Some("webm") => Ok("matroska"),
        _ => Err("cache container is unsupported".to_owned()),
    }
}

/// Reads at most 64 bytes from ffprobe's longstanding xxd codec-extradata form.
/// This header is authoritative when Matroska keeps CodecDelay outside packet
/// side data; no duration-derived guess reconstructs missing discard padding.
/// The header layout is defined by [RFC 7845, section 5.1](https://datatracker.ietf.org/doc/html/rfc7845#section-5.1).
fn opus_header(line: &str) -> Result<Vec<u8>, String> {
    let size = integer(line, "extradata_size")?;
    if !(19..=64).contains(&size) {
        return Err("cache Opus header size is invalid".to_owned());
    }
    let dump = field(line, "extradata")?.ok_or("cache Opus header is missing")?;
    let mut bytes = Vec::with_capacity(size as usize);
    for row in dump.split("\\n").filter(|row| !row.is_empty()) {
        let (offset, body) = row
            .split_once(": ")
            .ok_or("cache Opus header dump is invalid")?;
        if offset.len() != 8 || usize::from_str_radix(offset, 16).ok() != Some(bytes.len()) {
            return Err("cache Opus header offset is invalid".to_owned());
        }
        let hex = body
            .split("  ")
            .next()
            .ok_or("cache Opus header bytes are missing")?;
        for group in hex.split_ascii_whitespace() {
            if !matches!(group.len(), 2 | 4) || !group.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err("cache Opus header bytes are invalid".to_owned());
            }
            for pair in group.as_bytes().chunks_exact(2) {
                let pair = std::str::from_utf8(pair).map_err(|_| "cache Opus header is invalid")?;
                bytes.push(
                    u8::from_str_radix(pair, 16).map_err(|_| "cache Opus header is invalid")?,
                );
                if bytes.len() > 64 {
                    return Err("cache Opus header limit exceeded".to_owned());
                }
            }
        }
    }
    if bytes.len() != size as usize || &bytes[..8] != b"OpusHead" || bytes[8] != 1 {
        return Err("cache Opus header identity is invalid".to_owned());
    }
    Ok(bytes)
}

/// Requested scalar keys must appear at most once; metadata cannot override them.
fn field<'a>(line: &'a str, wanted: &str) -> Result<Option<&'a str>, String> {
    let mut values = line
        .split('|')
        .filter_map(|part| part.split_once('='))
        .filter_map(|(key, value)| (key == wanted).then_some(value));
    let value = values.next();
    if values.next().is_some() {
        return Err("cache probe returned duplicate fields".to_owned());
    }
    Ok(value)
}

fn integer(line: &str, key: &str) -> Result<u64, String> {
    field(line, key)?
        .ok_or("cache probe omitted a required integer")?
        .parse()
        .map_err(|_| "cache probe returned an invalid integer".to_owned())
}

fn timestamp(line: &str, key: &str) -> Result<f64, String> {
    let value = field(line, key)?
        .ok_or("cache probe omitted a required timestamp")?
        .parse::<f64>()
        .map_err(|_| "cache probe returned an invalid timestamp")?;
    if !value.is_finite() || value.abs() > MAX_DURATION + 5.0 {
        return Err("cache probe returned a nonfinite or unbounded timestamp".to_owned());
    }
    Ok(value)
}

fn probe(
    config: &Config,
    path: &Path,
    directory: &Path,
    expected: f64,
    cancellation: &AtomicBool,
    started: Instant,
) -> Result<OpusProof, String> {
    run_helper(
        probe_command(config, path)?,
        true,
        directory,
        cancellation,
        started,
    )?
    .finish(expected)
}

/// Selects only scalar packet/frame evidence and the small stream header: even
/// with -show_data, packet payloads are excluded by the explicit field whitelist.
fn probe_command(config: &Config, path: &Path) -> Result<Command, String> {
    let input_format = cache_input_format(path)?;
    let mut command = Command::new(&config.providers.ffprobe_executable);
    command.args([
        "-v", "error", "-max_alloc", "67108864",
        "-protocol_whitelist", "file,pipe", "-f", input_format, "-show_packets", "-show_frames", "-show_streams",
        "-show_entries",
        "packet=pts_time,duration_time,data_hash,stream_index,flags:frame=nb_samples,best_effort_timestamp_time,stream_index:stream=index,codec_name,codec_type,sample_rate,channels,extradata,extradata_size",
        "-show_data", "-show_data_hash", "sha256", "-of", "compact=p=1:nk=0",
    ]).arg(path);
    Ok(command)
}

/// Kills the entire helper group on every success/error/unwind path.
struct HelperGuard(Child);
impl Drop for HelperGuard {
    fn drop(&mut self) {
        crate::child_process::terminate_tree(&mut self.0);
    }
}

#[derive(Default)]
struct ReaderState {
    probe: ProbeAccumulator,
    bytes: usize,
    done: usize,
    error: Option<String>,
}

/// Supervision stays off the reducer; readers retain only bounded scalar evidence.
fn run_helper(
    mut command: Command,
    parse_probe: bool,
    directory: &Path,
    cancellation: &AtomicBool,
    started: Instant,
) -> Result<ProbeAccumulator, String> {
    check_work(cancellation, started)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut helper = HelperGuard(
        crate::child_process::supervised(&mut command)
            .spawn()
            .map_err(|error| {
                format!(
                    "cache validation helper is unavailable ({:?})",
                    error.kind()
                )
            })?,
    );
    let stdout = helper
        .0
        .stdout
        .take()
        .ok_or("cache probe output is unavailable")?;
    let stderr = helper
        .0
        .stderr
        .take()
        .ok_or("cache probe diagnostics are unavailable")?;
    let state = Arc::new(Mutex::new(ReaderState::default()));
    let readers = [
        spawn_reader(stdout, parse_probe, false, state.clone())?,
        spawn_reader(stderr, false, true, state.clone())?,
    ];
    let mut exited = None;
    loop {
        check_work(cancellation, started)?;
        check_staging(directory)?;
        let (done, error) = {
            let state = state.lock().map_err(|_| "cache reader failed")?;
            (state.done, state.error.clone())
        };
        if let Some(error) = error {
            return Err(error);
        }
        if exited.is_none()
            && let Some(status) = helper
                .0
                .try_wait()
                .map_err(|_| "cannot inspect cache helper")?
        {
            crate::child_process::terminate_tree(&mut helper.0);
            exited = Some((status, Instant::now()));
        }
        if let Some((status, at)) = exited {
            if done == 2 {
                if !status.success() {
                    return Err("cache validation helper failed".to_owned());
                }
                break;
            }
            if at.elapsed() > Duration::from_secs(1) {
                return Err("cache helper output remained open".to_owned());
            }
        }
        std::thread::sleep(POLL);
    }
    for reader in readers {
        if reader.is_finished() {
            let _ = reader.join();
        }
    }
    let mut state = state.lock().map_err(|_| "cache reader failed")?;
    if let Some(error) = state.error.take() {
        return Err(error);
    }
    Ok(std::mem::take(&mut state.probe))
}

fn spawn_reader(
    mut reader: impl Read + Send + 'static,
    parse_probe: bool,
    diagnostics: bool,
    state: Arc<Mutex<ReaderState>>,
) -> Result<std::thread::JoinHandle<()>, String> {
    std::thread::Builder::new()
        .name("cache-validation-output".to_owned())
        .spawn(move || {
            let result = (|| {
                let mut buffer = [0_u8; 4096];
                let mut line = Vec::new();
                loop {
                    let count = reader
                        .read(&mut buffer)
                        .map_err(|_| "cache helper output read failed")?;
                    if count == 0 {
                        break;
                    }
                    {
                        let mut state = state.lock().map_err(|_| "cache reader failed")?;
                        state.bytes = state.bytes.saturating_add(count);
                        if state.bytes > MAX_PROCESS_OUTPUT {
                            return Err("cache helper output limit exceeded");
                        }
                    }
                    if diagnostics
                        && buffer[..count]
                            .iter()
                            .any(|byte| !byte.is_ascii_whitespace())
                    {
                        return Err("cache decoder reported an error");
                    }
                    if !parse_probe {
                        continue;
                    }
                    for byte in &buffer[..count] {
                        if *byte == b'\n' {
                            parse_probe_line(&line, &state)?;
                            line.clear();
                        } else {
                            if line.len() >= MAX_LINE_BYTES {
                                return Err("cache probe line limit exceeded");
                            }
                            line.push(*byte);
                        }
                    }
                }
                if parse_probe && !line.is_empty() {
                    parse_probe_line(&line, &state)?;
                }
                Ok::<(), &str>(())
            })();
            if let Ok(mut state) = state.lock() {
                if let Err(error) = result
                    && state.error.is_none()
                {
                    state.error = Some(error.to_owned());
                }
                state.done += 1;
            }
        })
        .map_err(|_| "cannot start bounded cache output reader".to_owned())
}

fn parse_probe_line(line: &[u8], state: &Mutex<ReaderState>) -> Result<(), &'static str> {
    let line = std::str::from_utf8(line).map_err(|_| "cache probe output is not UTF-8")?;
    let mut state = state.lock().map_err(|_| "cache reader failed")?;
    if let Err(error) = state.probe.line(line.trim_end_matches('\r')) {
        state.error = Some(error);
        return Err("cache packet or decoded-frame validation failed");
    }
    Ok(())
}

fn check_staging(directory: &Path) -> Result<(), String> {
    let mut bytes = 0_u64;
    let mut files = 0_usize;
    for entry in std::fs::read_dir(directory).map_err(|_| "cache staging is unavailable")? {
        let entry = entry.map_err(|_| "cache staging entry is unavailable")?;
        let metadata = std::fs::symlink_metadata(entry.path())
            .map_err(|_| "cache staging metadata is unavailable")?;
        files += 1;
        bytes = bytes
            .checked_add(metadata.len())
            .ok_or("cache staging size overflow")?;
        if files > 4 || !metadata.file_type().is_file() || bytes > MAX_STAGING_BYTES {
            return Err("cache staging resource limit exceeded".to_owned());
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    /// Minimal complete 60ms decoded audio with stable, distinct packet hashes.
    fn probe_text() -> String {
        let mut text = String::new();
        for (index, hash) in ['a', 'b', 'c'].into_iter().enumerate() {
            let time = index as f64 * 0.02;
            text.push_str(&format!(
                "packet|stream_index=0|pts_time={time:.6}|duration_time=0.020000|flags=K__|data_hash=SHA256:{}\nframe|stream_index=0|best_effort_timestamp_time={time:.6}|nb_samples=960\n",
                hash.to_string().repeat(64)
            ));
        }
        text.push_str("stream|index=0|codec_name=opus|codec_type=audio|sample_rate=48000|channels=1|extradata_size=19|extradata=\\n00000000: 4f70 7573 4865 6164 0101 0000 80bb 0000  OpusHead........\\n00000010: 0000 00                                  ...\\n|start_time=0.000000|duration=0.060000\n");
        text
    }

    /// Atomically installs a closed helper, then proves executable readiness.
    fn executable(path: &Path, body: &str) {
        const READY: &str = "__youta_cache_fixture_ready__";
        let writing = path.with_extension("writing");
        {
            let mut file = std::fs::File::create(&writing).unwrap();
            file.write_all(
                format!("#!/bin/sh\nif [ \"$1\" = '{READY}' ]; then exit 0; fi\nset -eu\n{body}\n")
                    .as_bytes(),
            )
            .unwrap();
            file.sync_all().unwrap();
        }
        std::fs::set_permissions(&writing, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::rename(&writing, path).unwrap();
        // Parallel instrumentation may briefly retain a writable executable
        // handle. Retry only that fixture race, never a production helper error.
        for attempt in 0..50 {
            match Command::new(path).arg(READY).status() {
                Ok(status) => {
                    assert!(status.success(), "fixture readiness failed: {status}");
                    return;
                }
                Err(error)
                    if attempt < 49 && error.kind() == std::io::ErrorKind::ExecutableFileBusy =>
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("fixture did not become executable: {error}"),
            }
        }
        unreachable!("bounded fixture readiness always returns or fails");
    }

    fn fixture(directory: &Path, probe: &str) -> (Config, PathBuf, PathBuf) {
        let source = directory.join("exported.opus");
        let destination = directory.join("downloads");
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(&source, b"selected cached Opus packets").unwrap();
        let data = directory.join("probe.txt");
        std::fs::write(&data, probe).unwrap();
        let probe_helper = directory.join("ffprobe-fixture");
        executable(&probe_helper, &format!("cat '{}'", data.display()));
        let remux_helper = directory.join("ffmpeg-fixture");
        executable(
            &remux_helper,
            "input=; previous=; copy=0; for argument in \"$@\"; do [ \"$previous\" != -i ] || input=$argument; [ \"$argument\" != copy ] || copy=1; previous=$argument; output=$argument; done; [ \"$copy\" = 1 ]; cp \"$input\" \"$output\"",
        );
        let mut config = Config::for_dir(directory.join("config"));
        config.providers.ffprobe_executable = probe_helper;
        config.providers.ffmpeg_executable = remux_helper;
        (config, source, destination)
    }

    #[test]
    fn complete_opus_stays_private_until_collision_safe_publication() {
        let directory = tempfile::tempdir().unwrap();
        let (config, source, destination) = fixture(directory.path(), &probe_text());
        let prepared = prepare_cached_opus(
            &config,
            &source,
            Duration::from_millis(60),
            &destination,
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(prepared.path()).unwrap(),
            b"selected cached Opus packets"
        );
        let private = prepared.path().parent().unwrap().to_owned();
        let existing = destination.join("Title [video-id].opus");
        std::fs::write(&existing, b"older download").unwrap();
        let published = prepared.publish(&destination, "Title", "video-id").unwrap();
        assert_eq!(published.file_name().unwrap(), "Title [video-id] (1).opus");
        assert_eq!(std::fs::read(existing).unwrap(), b"older download");
        assert_eq!(
            std::fs::read(published).unwrap(),
            b"selected cached Opus packets"
        );
        assert!(!private.exists());
        assert_eq!(
            std::fs::read(source).unwrap(),
            b"selected cached Opus packets"
        );
    }

    #[test]
    fn probe_whitelist_exposes_codec_header_but_never_packet_payloads() {
        let config = Config::default();
        let command = probe_command(&config, Path::new("cached.webm")).unwrap();
        let args = command
            .get_args()
            .map(|argument| argument.to_str().unwrap())
            .collect::<Vec<_>>();
        let entries = args
            .windows(2)
            .find_map(|pair| (pair[0] == "-show_entries").then_some(pair[1]))
            .unwrap();
        assert!(args.contains(&"-show_data"));
        assert!(
            entries
                .split(':')
                .any(|section| section
                    == "packet=pts_time,duration_time,data_hash,stream_index,flags")
        );
        assert!(
            entries.split(':').any(
                |section| section == "frame=nb_samples,best_effort_timestamp_time,stream_index"
            )
        );
        assert!(entries.contains("channels,extradata,extradata_size"));
        assert!(args.windows(2).any(|pair| pair == ["-f", "matroska"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-show_data_hash", "sha256"])
        );
    }

    #[test]
    fn webm_opus_preserves_container_and_collision_safe_extension() {
        let directory = tempfile::tempdir().unwrap();
        let (config, source, destination) = fixture(directory.path(), &probe_text());
        let webm = directory.path().join("exported.webm");
        std::fs::rename(source, &webm).unwrap();
        let arguments = directory.path().join("remux-arguments.txt");
        executable(
            &config.providers.ffmpeg_executable,
            &format!(
                "printf '%s\\n' \"$@\" > '{}'\ninput=; previous=; for argument in \"$@\"; do [ \"$previous\" != -i ] || input=$argument; previous=$argument; output=$argument; done; cp \"$input\" \"$output\"",
                arguments.display(),
            ),
        );
        let prepared = prepare_cached_opus(
            &config,
            &webm,
            Duration::from_millis(60),
            &destination,
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert_eq!(prepared.path().extension().unwrap(), "webm");
        let arguments = std::fs::read_to_string(arguments).unwrap();
        let arguments = arguments.lines().collect::<Vec<_>>();
        assert!(arguments.windows(2).any(|pair| pair == ["-f", "matroska"]));
        assert!(arguments.windows(2).any(|pair| pair == ["-f", "webm"]));
        assert!(arguments.windows(2).any(|pair| pair == ["-c:a", "copy"]));
        let existing = destination.join("Title [video-id].webm");
        std::fs::write(&existing, b"older WebM").unwrap();
        let published = prepared.publish(&destination, "Title", "video-id").unwrap();
        assert_eq!(published.file_name().unwrap(), "Title [video-id] (1).webm");
        assert_eq!(std::fs::read(existing).unwrap(), b"older WebM");
        assert_eq!(
            std::fs::read(published).unwrap(),
            b"selected cached Opus packets"
        );
    }

    #[test]
    fn incomplete_gapped_wrong_codec_and_invalid_timestamps_are_cache_misses() {
        for probe in [
            probe_text().replace("codec_name=opus", "codec_name=aac"),
            probe_text().replace("pts_time=0.020000", "pts_time=0.035000"),
            probe_text().replace("nb_samples=960", "nb_samples=480"),
            probe_text().replace("pts_time=0.020000", "pts_time=NaN"),
            probe_text().replace("sample_rate=48000", "sample_rate=44100"),
            probe_text().replace("flags=K__", "flags=K_C"),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let (config, source, destination) = fixture(directory.path(), &probe);
            let error = prepare_cached_opus(
                &config,
                &source,
                Duration::from_millis(60),
                &destination,
                &Arc::new(AtomicBool::new(false)),
            )
            .unwrap_err();
            assert!(error.contains("cache"), "{error}");
            assert_eq!(std::fs::read_dir(destination).unwrap().count(), 0);
        }
    }

    #[test]
    fn precise_duration_tolerates_opus_preskip_but_not_missing_tail_packets() {
        let directory = tempfile::tempdir().unwrap();
        let probe = probe_text()
            .replacen("data_hash=SHA256:", "skip_samples=312|data_hash=SHA256:", 1)
            .replace("0101 0000", "0101 3801")
            .replacen("nb_samples=960", "nb_samples=648", 1)
            .replacen(
                "best_effort_timestamp_time=0.000000",
                "best_effort_timestamp_time=0.006500",
                1,
            );
        let (config, source, destination) = fixture(directory.path(), &probe);
        let prepared = prepare_cached_opus(
            &config,
            &source,
            Duration::from_millis(60),
            &destination,
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        drop(prepared);
        assert!(
            prepare_cached_opus(
                &config,
                &source,
                Duration::from_millis(80),
                &destination,
                &Arc::new(AtomicBool::new(false)),
            )
            .is_err()
        );
    }

    #[test]
    fn cancellation_stops_silent_probe_without_publishing() {
        let directory = tempfile::tempdir().unwrap();
        let (config, source, destination) = fixture(directory.path(), &probe_text());
        executable(&config.providers.ffprobe_executable, "sleep 30 &\nwait");
        let cancellation = Arc::new(AtomicBool::new(false));
        let cancel = cancellation.clone();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            cancel.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let error = prepare_cached_opus(
            &config,
            &source,
            Duration::from_millis(60),
            &destination,
            &cancellation,
        )
        .unwrap_err();
        worker.join().unwrap();
        assert!(error.contains("canceled"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(std::fs::read_dir(destination).unwrap().count(), 0);
    }

    #[test]
    fn ambiguous_duration_cannot_hide_a_short_missing_tail() {
        let text = probe_text()
            .replacen("data_hash=SHA256:", "skip_samples=312|data_hash=SHA256:", 1)
            .replace("0101 0000", "0101 3801")
            .replacen("nb_samples=960", "nb_samples=648", 1)
            .replacen(
                "best_effort_timestamp_time=0.000000",
                "best_effort_timestamp_time=0.006500",
                1,
            );
        let mut evidence = ProbeAccumulator::default();
        for line in text.lines() {
            evidence.line(line).unwrap();
        }
        // This shortened duration matches decoded samples alone, but not the
        // precise Ogg/WebM duration including the recorded codec pre-skip.
        assert!(evidence.finish(0.0535).is_err());
    }

    #[test]
    fn matroska_opus_uses_codec_header_delay_when_packet_skip_is_absent() {
        let text = probe_text()
            .replace("0101 0000", "0101 3801")
            .replacen("nb_samples=960", "nb_samples=648", 1)
            .replacen(
                "best_effort_timestamp_time=0.000000",
                "best_effort_timestamp_time=0.007000",
                1,
            )
            .replace(
                "best_effort_timestamp_time=0.020000",
                "best_effort_timestamp_time=0.021000",
            )
            .replace(
                "best_effort_timestamp_time=0.040000",
                "best_effort_timestamp_time=0.041000",
            );
        let mut evidence = ProbeAccumulator::default();
        for line in text.lines() {
            evidence.line(line).unwrap();
        }
        assert!(evidence.finish(0.0615).is_ok());
    }

    #[test]
    fn repeated_submillisecond_rounding_cannot_accumulate_into_an_audio_gap() {
        let mut text = probe_text()
            .replace(
                "best_effort_timestamp_time=0.020000",
                "best_effort_timestamp_time=0.021000",
            )
            .replace(
                "best_effort_timestamp_time=0.040000",
                "best_effort_timestamp_time=0.042000",
            );
        let fourth = format!(
            "packet|stream_index=0|pts_time=0.060000|duration_time=0.020000|flags=K__|data_hash=SHA256:{}\nframe|stream_index=0|best_effort_timestamp_time=0.063000|nb_samples=960\n",
            "d".repeat(64)
        );
        text = text.replace("stream|index=0", &format!("{fourth}stream|index=0"));
        let mut evidence = ProbeAccumulator::default();
        for line in text.lines() {
            evidence.line(line).unwrap();
        }
        assert!(evidence.finish(0.080).is_err());
    }

    #[test]
    fn changed_encoded_packets_after_remux_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let (config, source, destination) = fixture(directory.path(), &probe_text());
        let data = directory.path().join("probe.txt");
        let changed = directory.path().join("changed.txt");
        std::fs::write(
            &changed,
            probe_text().replace(&"b".repeat(64), &"d".repeat(64)),
        )
        .unwrap();
        executable(
            &config.providers.ffprobe_executable,
            &format!(
                "for argument in \"$@\"; do input=$argument; done; case \"$input\" in */verified.opus) cat '{}' ;; *) cat '{}' ;; esac",
                changed.display(),
                data.display(),
            ),
        );
        let error = prepare_cached_opus(
            &config,
            &source,
            Duration::from_millis(60),
            &destination,
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap_err();
        assert!(error.contains("changed encoded packets"), "{error}");
        assert_eq!(std::fs::read_dir(destination).unwrap().count(), 0);
    }

    #[test]
    fn unbounded_probe_lines_and_decoder_errors_are_rejected() {
        for script in ["printf '%17000s' x", "printf 'decoder error' >&2"] {
            let directory = tempfile::tempdir().unwrap();
            let (config, source, destination) = fixture(directory.path(), &probe_text());
            executable(&config.providers.ffprobe_executable, script);
            assert!(
                prepare_cached_opus(
                    &config,
                    &source,
                    Duration::from_millis(60),
                    &destination,
                    &Arc::new(AtomicBool::new(false)),
                )
                .is_err()
            );
            assert_eq!(std::fs::read_dir(destination).unwrap().count(), 0);
        }
    }

    #[test]
    fn oversized_symlinked_empty_and_special_inputs_never_reach_helpers() {
        let directory = tempfile::tempdir().unwrap();
        let (mut config, source, destination) = fixture(directory.path(), &probe_text());
        config.providers.ffprobe_executable = PathBuf::from("must-not-run-ffprobe");
        let oversized = directory.path().join("oversized.opus");
        std::fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_CACHE_BYTES + 1)
            .unwrap();
        let empty = directory.path().join("empty.opus");
        std::fs::write(&empty, []).unwrap();
        let link = directory.path().join("linked.opus");
        std::os::unix::fs::symlink(&source, &link).unwrap();
        let fifo = directory.path().join("fifo.opus");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .unwrap();
        for input in [oversized, empty, link, fifo] {
            let error = prepare_cached_opus(
                &config,
                &input,
                Duration::from_millis(60),
                &destination,
                &Arc::new(AtomicBool::new(false)),
            )
            .unwrap_err();
            assert!(error.contains("nonregular"), "{error}");
        }
        assert_eq!(std::fs::read_dir(destination).unwrap().count(), 0);
    }

    #[test]
    fn expired_deadline_never_starts_a_helper() {
        let directory = tempfile::tempdir().unwrap();
        let result = run_helper(
            Command::new("must-not-run-cache-helper"),
            true,
            directory.path(),
            &AtomicBool::new(false),
            Instant::now() - VALIDATION_DEADLINE,
        );
        assert!(result.err().unwrap().contains("timed out"));
    }

    #[test]
    #[ignore = "requires real ffmpeg/ffprobe; generated local audio only"]
    fn real_opus_remux_preserves_packets_and_decoded_samples() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("synthetic.opus");
        let config = Config::for_dir(directory.path().join("config"));
        let mut generate = Command::new(&config.providers.ffmpeg_executable);
        generate
            .args([
                "-v",
                "error",
                "-nostdin",
                "-n",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000",
                "-t",
                "60",
                "-c:a",
                "libopus",
            ])
            .arg(&source);
        run_helper(
            generate,
            false,
            directory.path(),
            &AtomicBool::new(false),
            Instant::now(),
        )
        .unwrap();
        let destination = directory.path().join("downloads");
        std::fs::create_dir(&destination).unwrap();
        let prepared = prepare_cached_opus(
            &config,
            &source,
            Duration::from_micros(60_006_500),
            &destination,
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let evidence = probe(
            &config,
            prepared.path(),
            prepared.path().parent().unwrap(),
            60.0065,
            &AtomicBool::new(false),
            Instant::now(),
        )
        .unwrap();
        assert_eq!(evidence.packets, 3001);
        assert_eq!(evidence.decoded_samples, 2_880_000);
        prepared
            .publish(&destination, "Synthetic tone", "test")
            .unwrap();
    }

    #[test]
    fn prepared_artifact_drop_removes_private_files_without_publishing() {
        let directory = tempfile::tempdir().unwrap();
        let (config, source, destination) = fixture(directory.path(), &probe_text());
        let prepared = prepare_cached_opus(
            &config,
            &source,
            Duration::from_millis(60),
            &destination,
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let path = prepared.path().to_owned();
        drop(prepared);
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(destination).unwrap().count(), 0);
    }
}
