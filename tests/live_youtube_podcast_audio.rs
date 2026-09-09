//! Opt-in, bounded transport smoke test for real `YouTube` podcast audio.
//!
//! The default smoke test reads only the first 1024 episode bytes. Separately
//! guarded tests stream one complete episode or a nonzero-offset resumed tail,
//! capped at 128 MiB and five minutes without retaining the recording in memory
//! or files. None of the clients retry.
//! Publication dates are synthetic transport fixtures: no catalogue, date
//! lookup, persisted resolution cache, or Youta configuration is loaded.
//! The production resolver disables configuration files and plugins. Its
//! normal internal yt-dlp implementation cache policy is unchanged.

#![cfg(feature = "lan-sharing")]

use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use url::Url;
use youta::lan_share::{LanShareServer, prepare_youtube_podcast_share};
use youta::playback::youtube_prewarm::YouTubePrewarmConfig;
use youta::playback::ytdlp::{CollectionEntry, ExtractedCollection};
use youta::providers::validate_youtube_video_id;

/// Existing public playback fixture: Blender's Big Buck Bunny upload.
const DEFAULT_VIDEO_ID: &str = "aqz-KE-bpKQ";
/// The entire media payload accepted from each cold request.
const SAMPLE_BYTES: usize = 1024;
/// Caps response-header memory even if the server returns malformed output.
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// Includes 30-second extraction and the server's bounded CDN setup retries.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Caps the explicitly selected complete episode before accepting its body.
const MAX_FULL_AUDIO_BYTES: u64 = 128 * 1024 * 1024;
/// Bounds memory independent of an episode's advertised length.
const FULL_AUDIO_BUFFER_BYTES: usize = 64 * 1024;
/// Absolute request-to-EOF deadline, not a resetting idle timeout.
const FULL_AUDIO_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Starts inside the recording so the separately opted-in test exercises resume.
const DEFAULT_RESUME_OFFSET: u64 = 1024 * 1024;

/// Validates a small comma-separated list; duplicate IDs would weaken cold-cache coverage.
fn video_ids(value: Option<&str>) -> Result<Vec<String>, &'static str> {
    let ids = value
        .unwrap_or(DEFAULT_VIDEO_ID)
        .split(',')
        .map(str::trim)
        .collect::<Vec<_>>();
    if ids.is_empty() || ids.len() > 4 {
        return Err("provide between one and four YouTube video IDs");
    }
    let mut unique = HashSet::new();
    for id in &ids {
        if validate_youtube_video_id(id).is_err() {
            return Err("each video ID must be an 11-character YouTube identifier");
        }
        if !unique.insert(*id) {
            return Err("duplicate video IDs would reuse the resolution cache");
        }
    }
    Ok(ids.into_iter().map(str::to_owned).collect())
}

/// Full-transfer opt-in permits exactly one public episode.
fn full_video_id(value: Option<&str>) -> Result<String, &'static str> {
    let mut ids = video_ids(value)?;
    if ids.len() != 1 {
        return Err("the full-audio test requires exactly one YouTube video ID");
    }
    Ok(ids.remove(0))
}

/// Parses only decimal byte counts, never signs, whitespace, or header fragments.
fn resume_byte_count(value: &str) -> Result<u64, &'static str> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("resume byte counts must contain only decimal digits");
    }
    value
        .parse()
        .map_err(|_| "resume byte count is out of range")
}

/// Keeps the selected resume point nonzero and below the full-test size ceiling.
fn resume_offset(value: Option<&str>) -> Result<u64, &'static str> {
    let offset = value.map_or(Ok(DEFAULT_RESUME_OFFSET), resume_byte_count)?;
    if !(1..MAX_FULL_AUDIO_BYTES).contains(&offset) {
        return Err("resume offset must be between 1 byte and 128 MiB minus 1 byte");
    }
    Ok(offset)
}

/// Supplies metadata only to construct enclosure routes, not to test publication dates.
fn transport_collection(ids: &[String]) -> ExtractedCollection {
    ExtractedCollection {
        id: "UC0000000000000000000000".to_owned(),
        title: "Live audio transport fixture".to_owned(),
        extractor: Some("youtube:tab".to_owned()),
        thumbnail_url: None,
        entries: ids
            .iter()
            .map(|id| CollectionEntry {
                id: id.clone(),
                title: format!("Transport fixture {id}"),
                webpage_url: Some(
                    Url::parse(&format!("https://www.youtube.com/watch?v={id}")).unwrap(),
                ),
                duration_seconds: None,
                thumbnail_url: None,
                // This known-valid synthetic date must never trigger network metadata lookup.
                published_at: Some(1_700_000_000),
            })
            .collect(),
    }
}

/// Reads one required response field while rejecting ambiguous duplicate headers.
fn header<'a>(headers: &'a str, name: &str) -> Result<&'a str, &'static str> {
    let mut values = headers.lines().skip(1).filter_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then_some(value.trim())
    });
    let value = values
        .next()
        .ok_or("required range-response header is missing")?;
    if values.next().is_some() {
        return Err("duplicate range-response header");
    }
    Ok(value)
}

/// Accepts only a numeric HTTP status; arbitrary reason phrases remain private.
fn http_code(value: &str) -> Option<u16> {
    if value.len() != 3 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value
        .parse::<u16>()
        .ok()
        .filter(|code| (100..=599).contains(code))
}

/// Extracts the numeric response status without retaining any other header text.
fn response_status(headers: &str) -> Option<u16> {
    http_code(headers.lines().next()?.split_whitespace().nth(1)?)
}

/// Maps only known Youta error messages to fixed labels and numeric upstream status.
fn safe_proxy_category(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?.trim();
    let label = match text {
        "Remote media request failed" => "remote media request failed",
        "Remote media request failed: upstream connection failure" => "upstream connection failure",
        "Remote media request failed: upstream timeout" => "upstream timeout",
        "Remote media request failed: upstream protocol failure" => "upstream protocol failure",
        _ if text.starts_with("Could not resolve cookie-free YouTube audio") => {
            "cookie-free audio resolution failed"
        }
        _ => {
            let code =
                http_code(text.strip_prefix("Remote media request failed: upstream HTTP ")?)?;
            return Some(format!("upstream HTTP {code}"));
        }
    };
    Some(label.to_owned())
}

/// Permits a tiny error-body read only for explicitly sized, plain-text responses.
fn diagnostic_body_length(headers: &str) -> Option<usize> {
    if !header(headers, "content-type")
        .ok()?
        .split(';')
        .next()?
        .trim()
        .eq_ignore_ascii_case("text/plain")
    {
        return None;
    }
    if headers.lines().skip(1).any(|line| {
        line.split_once(':')
            .is_some_and(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
    }) {
        return None;
    }
    let length = header(headers, "content-length")
        .ok()?
        .parse::<usize>()
        .ok()?;
    (length <= 256).then_some(length)
}

/// Formats only validated numbers and fixed labels, never raw headers or body text.
fn safe_failure_message(headers: &str, body: Option<&[u8]>) -> String {
    let status = response_status(headers).map_or_else(
        || "invalid HTTP status".to_owned(),
        |code| format!("HTTP {code}"),
    );
    let category = body
        .and_then(safe_proxy_category)
        .unwrap_or_else(|| "unrecognized or unavailable proxy error".to_owned());
    format!("{status}: {category}")
}

/// Reads at most 256 error bytes with a short deadline before emitting sanitized detail.
fn read_safe_failure(stream: &mut TcpStream, headers: &str, deadline: Instant) -> String {
    let Some(length) = diagnostic_body_length(headers) else {
        return safe_failure_message(headers, None);
    };
    let deadline = deadline.min(Instant::now() + Duration::from_secs(2));
    let mut body = vec![0_u8; length];
    let mut received = 0;
    while received < length {
        match read_before_deadline(stream, &mut body[received..], deadline) {
            Ok(0) | Err(_) => return safe_failure_message(headers, None),
            Ok(count) => received += count,
        }
    }
    safe_failure_message(headers, Some(&body))
}

/// Rejects failed or unbounded responses before the test reads their media body.
fn validate_range_headers(headers: &str) -> Result<u64, String> {
    if response_status(headers) != Some(206) {
        return Err(safe_failure_message(headers, None));
    }
    let length = header(headers, "content-length")?
        .parse::<usize>()
        .map_err(|_| "invalid Content-Length")?;
    if length != SAMPLE_BYTES {
        return Err("Content-Length must equal the requested 1024 bytes".to_owned());
    }
    if header(headers, "content-type")?
        .split(';')
        .next()
        .map(str::trim)
        != Some("audio/webm")
    {
        return Err("the episode must use the audio/webm content type".to_owned());
    }
    let total = header(headers, "content-range")?
        .strip_prefix("bytes 0-1023/")
        .ok_or("Content-Range must describe bytes 0-1023")?
        .parse::<u64>()
        .map_err(|_| "Content-Range must provide a valid total size")?;
    if total < SAMPLE_BYTES as u64 {
        return Err("Content-Range total is smaller than the requested sample".to_owned());
    }
    Ok(total)
}

/// Requires one finite, uncompressed full-body response before consuming media.
fn validate_full_headers(headers: &str) -> Result<u64, String> {
    if response_status(headers) != Some(200) {
        return Err(safe_failure_message(headers, None));
    }
    if headers.lines().skip(1).any(|line| {
        line.split_once(':').is_some_and(|(name, _)| {
            ["transfer-encoding", "content-range", "content-encoding"]
                .iter()
                .any(|field| name.eq_ignore_ascii_case(field))
        })
    }) {
        return Err("full episode response must not be chunked, ranged, or encoded".to_owned());
    }
    let raw_length = header(headers, "content-length")?;
    if raw_length.is_empty() || !raw_length.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("full episode Content-Length must be an unsigned byte count".to_owned());
    }
    let length = raw_length
        .parse::<u64>()
        .map_err(|_| "invalid full episode Content-Length")?;
    if !(1..=MAX_FULL_AUDIO_BYTES).contains(&length) {
        return Err("full episode Content-Length must be between 1 byte and 128 MiB".to_owned());
    }
    if !header(headers, "content-type")?
        .split(';')
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("audio/webm"))
    {
        return Err("the episode must use the audio/webm content type".to_owned());
    }
    Ok(length)
}

/// Requires the precise open-ended tail, a finite full size, and identity transfer framing.
fn validate_resume_headers(headers: &str, offset: u64) -> Result<(u64, u64), String> {
    if response_status(headers) != Some(206) {
        return Err(safe_failure_message(headers, None));
    }
    if headers.lines().skip(1).any(|line| {
        line.split_once(':').is_some_and(|(name, _)| {
            name.eq_ignore_ascii_case("transfer-encoding")
                || name.eq_ignore_ascii_case("content-encoding")
        })
    }) {
        return Err("resumed episode must not be chunked or encoded".to_owned());
    }
    let length = resume_byte_count(header(headers, "content-length")?)?;
    let (range, total) = header(headers, "content-range")?
        .strip_prefix("bytes ")
        .and_then(|value| value.split_once('/'))
        .ok_or("resumed Content-Range must include a byte range and total")?;
    let (start, end) = range
        .split_once('-')
        .ok_or("resumed byte range is invalid")?;
    let (start, end, total) = (
        resume_byte_count(start)?,
        resume_byte_count(end)?,
        resume_byte_count(total)?,
    );
    if !(1..MAX_FULL_AUDIO_BYTES).contains(&offset)
        || start != offset
        || total <= start
        || total > MAX_FULL_AUDIO_BYTES
        || end.checked_add(1) != Some(total)
        || length != total - start
    {
        return Err(
            "resumed range and length must match the requested offset through EOF within 128 MiB"
                .to_owned(),
        );
    }
    if !header(headers, "content-type")?
        .split(';')
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("audio/webm"))
    {
        return Err("the episode must use the audio/webm content type".to_owned());
    }
    Ok((length, total))
}

/// Streams and discards one exact body, retaining only a WebM identification prefix.
///
/// The injected read operation enforces the live absolute deadline, while local
/// tests simulate truncated and overlong transfers without a network service.
fn consume_full_audio(
    expected: u64,
    read: impl FnMut(&mut [u8]) -> Result<usize, &'static str>,
) -> Result<u64, String> {
    consume_audio_body(expected, true, read)
}

/// Checks an exact bounded body; resumed tails need not begin with a container header.
fn consume_audio_body(
    expected: u64,
    require_webm_prefix: bool,
    mut read: impl FnMut(&mut [u8]) -> Result<usize, &'static str>,
) -> Result<u64, String> {
    if !(1..=MAX_FULL_AUDIO_BYTES).contains(&expected) {
        return Err("full episode size exceeds the bounded live-test limit".to_owned());
    }
    let mut buffer = [0_u8; FULL_AUDIO_BUFFER_BYTES];
    let mut prefix = [0_u8; SAMPLE_BYTES];
    let mut prefix_bytes = 0;
    let mut received = 0_u64;
    while received < expected {
        let wanted = usize::try_from((expected - received).min(buffer.len() as u64)).unwrap();
        let count = read(&mut buffer[..wanted])?;
        if count == 0 {
            return Err(format!(
                "full episode was truncated: received {received} of {expected} advertised bytes"
            ));
        }
        if count > wanted {
            return Err("episode reader exceeded its requested buffer".to_owned());
        }
        let retain = count.min(prefix.len() - prefix_bytes);
        prefix[prefix_bytes..prefix_bytes + retain].copy_from_slice(&buffer[..retain]);
        prefix_bytes += retain;
        received += count as u64;
    }
    if read(&mut buffer[..1])? != 0 {
        return Err("full episode body is longer than its advertised Content-Length".to_owned());
    }
    let prefix = &prefix[..prefix_bytes];
    if require_webm_prefix
        && (!prefix.starts_with(&[0x1a, 0x45, 0xdf, 0xa3])
            || !prefix.windows(4).any(|bytes| bytes == b"webm"))
    {
        return Err("full episode is not a WebM/EBML audio container".to_owned());
    }
    Ok(received)
}

/// Polls one socket with an absolute deadline rather than an unlimited idle timeout.
fn read_before_deadline(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    deadline: Instant,
) -> Result<usize, &'static str> {
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("episode response exceeded the live-test deadline")?;
        stream
            .set_read_timeout(Some(remaining.min(Duration::from_millis(250))))
            .map_err(|_| "could not bound the episode socket read")?;
        match stream.read(buffer) {
            Ok(count) => return Ok(count),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err("could not read the episode response"),
        }
    }
}

/// Opens one enclosure request and reads bounded headers without accepting its body.
fn open_episode_response(
    port: u16,
    index: usize,
    video_id: &str,
    range: Option<&str>,
    deadline: Instant,
) -> Result<(TcpStream, String), String> {
    let mut stream = TcpStream::connect_timeout(
        &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        Duration::from_secs(5),
    )
    .map_err(|_| "could not connect to the loopback podcast server")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|_| "could not bound the episode socket write")?;
    let encoded =
        percent_encoding::utf8_percent_encode(video_id, percent_encoding::NON_ALPHANUMERIC);
    let range = range.map_or_else(String::new, |value| format!("Range: {value}\r\n"));
    write!(stream, "GET /media/{index}/{encoded}.webm HTTP/1.1\r\nHost: localhost:{port}\r\n{range}Connection: close\r\n\r\n")
        .map_err(|_| "could not send the bounded episode request")?;

    // Single-byte header reads avoid accepting any body from an ignored Range
    // request. The finite header and time limits also bound malformed responses.
    let mut response_headers = Vec::new();
    while !response_headers.ends_with(b"\r\n\r\n") {
        if response_headers.len() == MAX_HEADER_BYTES {
            return Err("episode response headers exceeded 64 KiB".to_owned());
        }
        let mut byte = [0_u8; 1];
        if read_before_deadline(&mut stream, &mut byte, deadline)? == 0 {
            return Err("episode response ended before its headers completed".to_owned());
        }
        response_headers.push(byte[0]);
    }
    let headers = std::str::from_utf8(&response_headers)
        .map_err(|_| "episode response headers were not UTF-8")?;
    Ok((stream, headers.to_owned()))
}

/// Requests exactly one small prefix; there is deliberately no client-side retry.
fn probe_first_range(port: u16, index: usize, video_id: &str) -> Result<(Duration, u64), String> {
    let started = Instant::now();
    let deadline = started + REQUEST_TIMEOUT;
    let (mut stream, headers) =
        open_episode_response(port, index, video_id, Some("bytes=0-1023"), deadline)?;
    let headers = headers.as_str();
    if response_status(headers) != Some(206) {
        return Err(read_safe_failure(&mut stream, headers, deadline));
    }
    let total = validate_range_headers(headers)?;
    let mut sample = [0_u8; SAMPLE_BYTES];
    let mut received = 0;
    while received < sample.len() {
        let count = read_before_deadline(&mut stream, &mut sample[received..], deadline)?;
        if count == 0 {
            return Err("episode sample was truncated before its advertised 1024 bytes".to_owned());
        }
        received += count;
    }
    if !sample.starts_with(&[0x1a, 0x45, 0xdf, 0xa3])
        || !sample.windows(4).any(|bytes| bytes == b"webm")
    {
        return Err("episode sample is not a WebM/EBML audio container".to_owned());
    }
    // Close now: even a misbehaving peer cannot send more than this fixed sample
    // into test memory, and no second request hides an initial CDN failure.
    Ok((started.elapsed(), total))
}

/// Reads one complete episode with no retry and reports only validated byte counts.
fn probe_full_audio(port: u16, video_id: &str) -> Result<(Duration, u64), String> {
    let started = Instant::now();
    let deadline = started + FULL_AUDIO_TIMEOUT;
    let (mut stream, headers) = open_episode_response(port, 0, video_id, None, deadline)?;
    if response_status(&headers) != Some(200) {
        return Err(read_safe_failure(&mut stream, &headers, deadline));
    }
    let expected = validate_full_headers(&headers)?;
    let mut received = 0_u64;
    let mut last_reported = 0_u64;
    let mut last_report = Instant::now();
    let total = consume_full_audio(expected, |buffer| {
        let count = read_before_deadline(&mut stream, buffer, deadline)?;
        received += count as u64;
        if received - last_reported >= 5 * 1024 * 1024
            || last_report.elapsed() >= Duration::from_secs(15)
        {
            println!(
                "Full episode: {received}/{expected} bytes; elapsed: {:?}",
                started.elapsed()
            );
            last_reported = received;
            last_report = Instant::now();
        }
        Ok(count)
    })?;
    Ok((started.elapsed(), total))
}

/// Consumes one exact nonzero-offset tail without retry, disk output, or container-prefix assumptions.
fn probe_resumed_audio(
    port: u16,
    video_id: &str,
    offset: u64,
) -> Result<(Duration, u64, u64), String> {
    let started = Instant::now();
    let deadline = started + FULL_AUDIO_TIMEOUT;
    let range = format!("bytes={offset}-");
    let (mut stream, headers) = open_episode_response(port, 0, video_id, Some(&range), deadline)?;
    if response_status(&headers) != Some(206) {
        return Err(read_safe_failure(&mut stream, &headers, deadline));
    }
    let (expected, total) = validate_resume_headers(&headers, offset)?;
    let mut received = 0_u64;
    let mut last_reported = 0_u64;
    let mut last_report = Instant::now();
    let received = consume_audio_body(expected, false, |buffer| {
        let count = read_before_deadline(&mut stream, buffer, deadline)?;
        received += count as u64;
        if received - last_reported >= 5 * 1024 * 1024
            || last_report.elapsed() >= Duration::from_secs(15)
        {
            println!(
                "Resumed episode: {received}/{expected} bytes from offset {offset}; elapsed: {:?}",
                started.elapsed()
            );
            last_reported = received;
            last_report = Instant::now();
        }
        Ok(count)
    })?;
    Ok((started.elapsed(), received, total))
}

/// Checks cold enclosure transport for up to four public videos, two at a time.
#[test]
#[ignore = "requires YouTube and yt-dlp; explicitly enable YOUTA_RUN_LIVE_YOUTUBE_PODCAST_AUDIO_TEST=1"]
fn youtube_podcast_first_audio_range_is_complete() {
    assert_eq!(
        std::env::var("YOUTA_RUN_LIVE_YOUTUBE_PODCAST_AUDIO_TEST").as_deref(),
        Ok("1"),
        "set YOUTA_RUN_LIVE_YOUTUBE_PODCAST_AUDIO_TEST=1 to opt in"
    );
    let configured_ids = std::env::var("YOUTA_LIVE_PODCAST_VIDEO_IDS").ok();
    let ids = video_ids(configured_ids.as_deref()).expect("invalid live audio fixture selection");
    let server = start_transport_server(&ids);
    probe_sample_episodes(&server, &ids);
}

/// Separately opts into enough real audio traffic to expose mid-transfer truncation.
#[test]
#[ignore = "requires YouTube and yt-dlp; explicitly enable YOUTA_RUN_LIVE_YOUTUBE_PODCAST_FULL_AUDIO_TEST=1"]
fn youtube_podcast_full_audio_matches_advertised_length() {
    assert_eq!(
        std::env::var("YOUTA_RUN_LIVE_YOUTUBE_PODCAST_FULL_AUDIO_TEST").as_deref(),
        Ok("1"),
        "set YOUTA_RUN_LIVE_YOUTUBE_PODCAST_FULL_AUDIO_TEST=1 to opt in"
    );
    let configured = std::env::var("YOUTA_LIVE_PODCAST_VIDEO_IDS").ok();
    let id = full_video_id(configured.as_deref()).expect("select exactly one valid public video");
    let server = start_transport_server(std::slice::from_ref(&id));
    let port = Url::parse(server.url()).unwrap().port().unwrap();
    let (elapsed, received) =
        probe_full_audio(port, &id).expect("complete episode transport failed");
    println!("{id}: HTTP 200; exactly {received} WebM bytes and EOF; elapsed: {elapsed:?}");
}

/// Separately opts into a complete resumed tail, reproducing a podcast app's open-ended Range.
#[test]
#[ignore = "requires YouTube and yt-dlp; explicitly enable YOUTA_RUN_LIVE_YOUTUBE_PODCAST_RESUME_TEST=1"]
fn youtube_podcast_resumed_audio_matches_advertised_length() {
    assert_eq!(
        std::env::var("YOUTA_RUN_LIVE_YOUTUBE_PODCAST_RESUME_TEST").as_deref(),
        Ok("1"),
        "set YOUTA_RUN_LIVE_YOUTUBE_PODCAST_RESUME_TEST=1 to opt in"
    );
    let configured = std::env::var("YOUTA_LIVE_PODCAST_VIDEO_IDS").ok();
    let id = full_video_id(configured.as_deref()).expect("select exactly one valid public video");
    let configured_offset = std::env::var("YOUTA_LIVE_PODCAST_RESUME_OFFSET").ok();
    let offset = resume_offset(configured_offset.as_deref())
        .expect("select a bounded nonzero resume offset");
    let server = start_transport_server(std::slice::from_ref(&id));
    let port = Url::parse(server.url()).unwrap().port().unwrap();
    let (elapsed, received, total) =
        probe_resumed_audio(port, &id, offset).expect("complete resumed episode transport failed");
    println!(
        "{id}: HTTP 206; exactly {received} resumed bytes and EOF; Content-Range: bytes {offset}-{}/{total}; elapsed: {elapsed:?}",
        total - 1
    );
}

/// Starts the production LAN server with synthetic catalogue data and no saved config.
fn start_transport_server(ids: &[String]) -> LanShareServer {
    let resolver = YouTubePrewarmConfig {
        executable: std::env::var_os("YOUTA_TEST_YT_DLP")
            .map_or_else(|| PathBuf::from("yt-dlp"), PathBuf::from),
        timeout: Duration::from_secs(30),
        allow_plugins: false,
        ..YouTubePrewarmConfig::default()
    };
    let prepared = prepare_youtube_podcast_share(transport_collection(ids), resolver, false)
        .expect("prepare synthetic transport-only feed");
    LanShareServer::start(prepared).expect("start fresh podcast transport server")
}

/// Samples cold enclosures two at a time while keeping full-file opt-in separate.
fn probe_sample_episodes(server: &LanShareServer, ids: &[String]) {
    let port = Url::parse(server.url()).unwrap().port().unwrap();
    // Production feed preparation reverses the newest-first input catalogue.
    let episodes = ids.iter().rev().enumerate().collect::<Vec<_>>();
    let mut failures = Vec::new();
    for pair in episodes.chunks(2) {
        let results = std::thread::scope(|scope| {
            let workers = pair
                .iter()
                .map(|&(index, id)| (id, scope.spawn(move || probe_first_range(port, index, id))))
                .collect::<Vec<_>>();
            workers
                .into_iter()
                .map(|(id, worker)| {
                    (
                        id,
                        worker.join().unwrap_or_else(|_| {
                            Err("bounded audio probe worker panicked".to_owned())
                        }),
                    )
                })
                .collect::<Vec<_>>()
        });
        for (id, result) in results {
            match result {
                Ok((elapsed, total)) => println!(
                    "{id}: HTTP 206; exactly {SAMPLE_BYTES} WebM bytes; total audio bytes: {total}; elapsed: {elapsed:?}"
                ),
                Err(error) => {
                    eprintln!("{id}: first audio request failed: {error}");
                    failures.push(format!("{id}: {error}"));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "first audio requests failed: {}",
        failures.join("; ")
    );
}

/// Keeps URL injection, duplicate warm-cache requests, and unbounded lists out of live tests.
#[test]
fn audio_video_id_selection_is_bounded_and_validated() {
    assert_eq!(video_ids(None).unwrap(), [DEFAULT_VIDEO_ID]);
    assert_eq!(
        video_ids(Some(" rnIUhRKWoN0, lWHQHXN7Czg ")).unwrap(),
        ["rnIUhRKWoN0", "lWHQHXN7Czg"]
    );
    for invalid in [
        "",
        ",",
        "rnIUhRKWoN0,",
        "https://www.youtube.com/watch?v=rnIUhRKWoN0",
        "../../audio",
        "rnIUhRKWoN0,rnIUhRKWoN0",
        "rnIUhRKWoN0,lWHQHXN7Czg,aqz-KE-bpKQ,newest00001,oldest00001",
    ] {
        assert!(video_ids(Some(invalid)).is_err());
    }
}

/// Full-file tests must not silently download several large public recordings.
#[test]
fn full_audio_selection_requires_exactly_one_video() {
    assert_eq!(full_video_id(None).unwrap(), DEFAULT_VIDEO_ID);
    assert_eq!(full_video_id(Some(" rnIUhRKWoN0 ")).unwrap(), "rnIUhRKWoN0");
    for invalid in ["", "rnIUhRKWoN0,lWHQHXN7Czg", "../../audio"] {
        assert!(full_video_id(Some(invalid)).is_err());
    }
}

/// Numeric configuration cannot inject headers or select an unbounded resume point.
#[test]
fn resumed_audio_offset_requires_a_bounded_positive_decimal() {
    assert_eq!(resume_offset(None), Ok(DEFAULT_RESUME_OFFSET));
    assert_eq!(resume_offset(Some("2684592")), Ok(2_684_592));
    assert_eq!(resume_offset(Some("1")), Ok(1));
    assert_eq!(
        resume_offset(Some("134217727")),
        Ok(MAX_FULL_AUDIO_BYTES - 1)
    );
    for invalid in [
        "",
        "0",
        "-1",
        "+1",
        " 1",
        "1 ",
        "1\r\n",
        "1-2",
        "134217728",
        "18446744073709551616",
    ] {
        assert!(resume_offset(Some(invalid)).is_err());
    }
}

/// Malformed resumed ranges must fail before any potentially large body is read.
#[test]
fn resumed_audio_headers_require_exact_offset_length_and_eof_range() {
    let valid = "HTTP/1.1 206 Partial Content\r\nContent-Length: 10\r\nContent-Type: audio/webm\r\nContent-Range: bytes 1048576-1048585/1048586\r\n\r\n";
    assert_eq!(
        validate_resume_headers(valid, DEFAULT_RESUME_OFFSET),
        Ok((10, 1_048_586))
    );
    for invalid in [
        valid.replace("206 Partial Content", "200 OK"),
        valid.replace("Content-Length: 10", "Content-Length: 9"),
        valid.replace("Content-Length: 10", "Content-Length: 0"),
        valid.replace("Content-Length: 10", "Content-Length: +10"),
        valid.replace("Content-Length: 10\r\n", ""),
        valid.replace(
            "Content-Length: 10",
            "Content-Length: 10\r\nContent-Length: 10",
        ),
        valid.replace("1048576-", "1048575-"),
        valid.replace("1048576-", "+1048576-"),
        valid.replace("-1048585/", "-1048584/"),
        valid.replace("/1048586", "/1048587"),
        valid.replace("/1048586", "/1048576"),
        valid.replace("/1048586", "/*"),
        valid.replace("/1048586", "/18446744073709551616"),
        valid.replace("1048585/1048586", "134217728/134217729"),
        valid.replace(
            "Content-Range: bytes",
            "Content-Range: bytes 1048576-1048585/1048586\r\nContent-Range: bytes",
        ),
        valid.replace(
            "Content-Type: audio/webm",
            "Content-Type: audio/webm\r\nTransfer-Encoding: chunked",
        ),
        valid.replace(
            "Content-Type: audio/webm",
            "Content-Type: audio/webm\r\nContent-Encoding: gzip",
        ),
        valid.replace("audio/webm", "text/html"),
    ] {
        assert!(validate_resume_headers(&invalid, DEFAULT_RESUME_OFFSET).is_err());
    }
    for offset in [
        0,
        DEFAULT_RESUME_OFFSET - 1,
        1_048_586,
        MAX_FULL_AUDIO_BYTES,
    ] {
        assert!(validate_resume_headers(valid, offset).is_err());
    }
}

/// Unknown, excessive, compressed, or ambiguous lengths cannot start a full read.
#[test]
fn full_audio_headers_reject_unbounded_or_ambiguous_bodies() {
    let valid = "HTTP/1.1 200 OK\r\nContent-Length: 1024\r\nContent-Type: audio/webm\r\n\r\n";
    assert_eq!(validate_full_headers(valid), Ok(1024));
    for invalid in [
        valid.replace("200 OK", "206 Partial Content"),
        valid.replace("1024", "0"),
        valid.replace("1024", "134217729"),
        valid.replace("1024", "+1024"),
        valid.replace("Content-Length: 1024\r\n", ""),
        valid.replace("1024", "1024,1024"),
        valid.replace(
            "Content-Length: 1024",
            "Content-Length: 1024\r\nContent-Length: 1024",
        ),
        valid.replace(
            "Content-Length: 1024",
            "Content-Length: 1024\r\nTransfer-Encoding: chunked",
        ),
        valid.replace(
            "Content-Length: 1024",
            "Content-Length: 1024\r\nContent-Range: bytes 0-1023/2048",
        ),
        valid.replace(
            "Content-Length: 1024",
            "Content-Length: 1024\r\nContent-Encoding: gzip",
        ),
        valid.replace("audio/webm", "text/html"),
        valid.replace(
            "Content-Type: audio/webm",
            "Content-Type: audio/webm\r\nContent-Type: audio/webm",
        ),
    ] {
        assert!(
            validate_full_headers(&invalid).is_err(),
            "accepted invalid fixture"
        );
    }
}

/// Complete transport verification detects the phone's truncated-download failure.
#[test]
fn full_audio_stream_requires_all_advertised_bytes_and_eof() {
    let mut valid = vec![0_u8; 128 * 1024 + 17];
    valid[..8].copy_from_slice(&[0x1a, 0x45, 0xdf, 0xa3, b'w', b'e', b'b', b'm']);
    let expected = valid.len() as u64;
    for (body, result) in [
        (valid.clone(), Ok(expected)),
        (valid[..valid.len() - 1].to_vec(), Err("truncated")),
        ([valid.clone(), vec![1]].concat(), Err("longer")),
        (vec![0; valid.len()], Err("WebM")),
    ] {
        let mut input = io::Cursor::new(body);
        let received = consume_full_audio(expected, |buffer| {
            assert!(buffer.len() <= 64 * 1024);
            input.read(buffer).map_err(|_| "fixture read failed")
        });
        match result {
            Ok(size) => assert_eq!(received.unwrap(), size),
            Err(label) => assert!(received.unwrap_err().contains(label)),
        }
    }
    assert!(
        consume_full_audio(128 * 1024 * 1024 + 1, |_| panic!(
            "oversized body must not be read"
        ))
        .is_err()
    );
}

/// A resumed tail has no EBML prefix but still requires every byte and a clean EOF.
#[test]
fn resumed_audio_stream_requires_all_advertised_bytes_and_eof() {
    let body = vec![0x55; FULL_AUDIO_BUFFER_BYTES * 2 + 17];
    let expected = body.len() as u64;
    for (bytes, failure) in [
        (body.clone(), None),
        (body[..body.len() - 1].to_vec(), Some("truncated")),
        ([body, vec![1]].concat(), Some("longer")),
    ] {
        let mut input = io::Cursor::new(bytes);
        let result = consume_audio_body(expected, false, |buffer| {
            assert!(buffer.len() <= FULL_AUDIO_BUFFER_BYTES);
            input.read(buffer).map_err(|_| "fixture read failed")
        });
        if let Some(message) = failure {
            assert!(result.unwrap_err().contains(message));
        } else {
            assert_eq!(result.unwrap(), expected);
        }
    }
    for invalid in [0, MAX_FULL_AUDIO_BYTES + 1] {
        assert!(
            consume_audio_body(invalid, false, |_| panic!("invalid tail must not be read"))
                .is_err()
        );
    }
    assert!(
        consume_audio_body(1, false, |buffer| Ok(buffer.len() + 1))
            .unwrap_err()
            .contains("exceeded")
    );
    assert!(
        consume_audio_body(1, false, |_| Err("fixture deadline expired"))
            .unwrap_err()
            .contains("deadline expired")
    );
}

/// Keeps the numeric first-request status available without printing its reason phrase.
#[test]
fn audio_range_failure_retains_http_status_without_raw_response() {
    let error = validate_range_headers("HTTP/1.1 502 private-signed-url\r\n\r\n").unwrap_err();
    assert!(error.contains("502"));
    assert!(!error.contains("private-signed-url"));
}

/// Confirms first-request errors and oversized bodies fail before media consumption.
#[test]
fn audio_range_headers_reject_gateway_errors_and_incorrect_lengths() {
    let valid = "HTTP/1.1 206 Partial Content\r\nContent-Length: 1024\r\nContent-Type: audio/webm\r\nContent-Range: bytes 0-1023/123456\r\n\r\n";
    assert_eq!(validate_range_headers(valid), Ok(123_456));
    for invalid in [
        valid.replace("206 Partial Content", "502 Bad Gateway"),
        valid.replace("206 Partial Content", "200 OK"),
        valid.replace("Content-Length: 1024", "Content-Length: 123456"),
        valid.replace("Content-Length: 1024", "Content-Length: 1000"),
        valid.replace("audio/webm", "text/html"),
        valid.replace("bytes 0-1023/123456", "bytes 1-1024/123456"),
        valid.replace("bytes 0-1023/123456", "bytes 0-1023/*"),
        valid.replace("bytes 0-1023/123456", "bytes 0-1023/512"),
        valid.replace(
            "Content-Length: 1024",
            "Content-Length: 1024\r\nContent-Length: 1024",
        ),
    ] {
        assert!(validate_range_headers(&invalid).is_err());
    }
}

/// Keeps only recognized proxy categories and rejects arbitrary response details.
#[test]
fn audio_proxy_diagnostics_allowlist_categories_without_exposing_urls() {
    let headers = "HTTP/1.1 502 private-header-value\r\n\r\n";
    for (body, expected) in [
        (
            "Remote media request failed: upstream HTTP 403",
            "upstream HTTP 403",
        ),
        (
            "Remote media request failed: upstream HTTP 503",
            "upstream HTTP 503",
        ),
        (
            "Remote media request failed: upstream connection failure",
            "upstream connection failure",
        ),
        (
            "Remote media request failed: upstream timeout",
            "upstream timeout",
        ),
        (
            "Remote media request failed: upstream protocol failure",
            "upstream protocol failure",
        ),
        ("Remote media request failed", "remote media request failed"),
        (
            "Could not resolve cookie-free YouTube audio; private-signed-url",
            "cookie-free audio resolution failed",
        ),
    ] {
        assert_eq!(
            safe_failure_message(headers, Some(body.as_bytes())),
            format!("HTTP 502: {expected}")
        );
    }
    for body in [
        "https://media.example.invalid/audio?sig=private-token",
        "Remote media request failed: upstream HTTP 403 private-token",
        "Remote media request failed: upstream HTTP 999",
        "Remote media request failed: private-token",
    ] {
        let message = safe_failure_message(headers, Some(body.as_bytes()));
        assert_eq!(message, "HTTP 502: unrecognized or unavailable proxy error");
        assert!(!message.contains("private"));
    }
    assert_eq!(response_status("HTTP/1.1 invalid-private-status\r\n"), None);
}

/// Restricts optional diagnostic body reads to small, unambiguous plain-text payloads.
#[test]
fn audio_proxy_diagnostic_body_reads_are_size_and_type_bounded() {
    let headers = "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 256\r\n\r\n";
    assert_eq!(diagnostic_body_length(headers), Some(256));
    assert_eq!(
        diagnostic_body_length(&headers.replace("256", "0")),
        Some(0)
    );
    for invalid in [
        headers.replace("256", "257"),
        headers.replace("256", "-1"),
        headers.replace("256", "invalid"),
        headers.replace("text/plain", "audio/webm"),
        headers.replace("text/plain", "text/html"),
        headers.replace(
            "Content-Length: 256",
            "Transfer-Encoding: chunked\r\nContent-Length: 256",
        ),
        headers.replace(
            "Content-Length: 256",
            "Content-Length: 256\r\nContent-Length: 1",
        ),
        headers.replace("Content-Length: 256\r\n", ""),
    ] {
        assert_eq!(diagnostic_body_length(&invalid), None);
    }
}
