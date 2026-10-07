//! Opt-in, offline regressions for switching back to RAM-cached audio.
//!
//! Run `cargo test --all-features native_ram_cache -- --ignored --nocapture`.
//! The tests generate short Opus fixtures with ffmpeg, use mpv's null audio
//! output, and permit only their owned loopback fixture through a test cache.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::RamPlaybackCache;
use crate::config::Config;
use crate::playback::mpv::MpvBackend;
use crate::playback::{
    AudioOutputDriver, PlaybackBackend, PlaybackEndReason, PlaybackEvent, PlaybackInput,
    PlaybackStatus, PlayerCommand, ProcessPlaybackConfig,
};

const WAIT_LIMIT: Duration = Duration::from_secs(10);
const CACHE_BYTES: u64 = 8 * 1024 * 1024;
const ORIGINAL_SECONDS: u64 = 29;
const OTHER_SECONDS: u64 = 41;

/// Owns a bounded, range-capable audio origin that can be disconnected before replay.
struct LocalSource {
    stop: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
    worker: Option<thread::JoinHandle<()>>,
    url: String,
}

impl LocalSource {
    /// Starts a fixture with strong validators and exact single-range responses.
    fn new(bytes: Vec<u8>, extension: &str) -> Self {
        Self::with_range_failure(bytes, extension, None)
    }

    /// Can refuse later closed ranges while ordinary uncached reads still work.
    fn with_range_failure(bytes: Vec<u8>, extension: &str, fail_from: Option<usize>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("owned fixture listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking fixture listener");
        let url = format!(
            "http://{}/synthetic.{extension}",
            listener.local_addr().expect("fixture address")
        );
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let worker_stop = Arc::clone(&stop);
        let worker_requests = Arc::clone(&requests);
        let content_type = match extension {
            "webm" => "audio/webm",
            "wav" => "audio/wav",
            _ => "audio/ogg",
        };
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        Self::respond(stream, &bytes, content_type, &worker_requests, fail_from);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => return,
                }
            }
        });
        Self {
            stop,
            requests,
            worker: Some(worker),
            url,
        }
    }

    /// Serves an individual fixture request without exposing unrelated local files.
    fn respond(
        stream: TcpStream,
        bytes: &[u8],
        content_type: &str,
        requests: &AtomicUsize,
        fail_from: Option<usize>,
    ) {
        // Winsock inherits nonblocking mode from the listening socket.
        stream
            .set_nonblocking(false)
            .expect("blocking accepted fixture stream");
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("request timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .expect("response timeout");
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        let mut range = None;
        let mut head = false;
        loop {
            let mut line = String::new();
            if reader
                .read_line(&mut line)
                .ok()
                .is_none_or(|count| count == 0)
            {
                return;
            }
            if request.is_empty() {
                head = line.starts_with("HEAD ");
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                range = value.trim().split_once('-').and_then(|(start, end)| {
                    Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()))
                });
            }
            request.push_str(&line);
            if line == "\r\n" {
                break;
            }
            if request.len() > 16 * 1024 {
                return;
            }
        }
        requests.fetch_add(1, Ordering::Relaxed);
        let (start, end) = range.map_or((0, bytes.len() - 1), |(start, end)| {
            (start, end.unwrap_or(bytes.len() - 1).min(bytes.len() - 1))
        });
        let stream = reader.get_mut();
        if fail_from.is_some_and(|limit| start >= limit)
            && range.is_some_and(|(_, end)| end.is_some())
        {
            let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            return;
        }
        if start > end {
            let _ = write!(
                stream,
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            return;
        }
        let status = if range.is_some() {
            "206 Partial Content"
        } else {
            "200 OK"
        };
        let range_header = if range.is_some() {
            format!("Content-Range: bytes {start}-{end}/{}\r\n", bytes.len())
        } else {
            String::new()
        };
        let header = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\nAccept-Ranges: bytes\r\nETag: \"fixture-{}\"\r\n{range_header}Connection: close\r\n\r\n",
            end - start + 1,
            bytes.len(),
        );
        if stream.write_all(header.as_bytes()).is_ok() && !head {
            let _ = stream.write_all(&bytes[start..=end]);
        }
    }

    /// Closes the origin so a successful replay must use already-retained bytes.
    fn disconnect(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("fixture source worker");
        }
    }

    /// Counts requests accepted by this origin, including range probes.
    fn request_count(&self) -> usize {
        self.requests.load(Ordering::Relaxed)
    }
}

impl Drop for LocalSource {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// Creates distinct synthetic tracks so stale or cross-media bytes change duration.
fn generate_audio(path: &Path, seconds: u64, frequency: u16) {
    let codec = if path.extension().is_some_and(|extension| extension == "wav") {
        "pcm_s16le"
    } else {
        "libopus"
    };
    let result = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-n", "-f", "lavfi", "-i"])
        .arg(format!("sine=frequency={frequency}:sample_rate=48000"))
        .arg("-t")
        .arg(seconds.to_string())
        .args(["-c:a", codec])
        .arg(path)
        .output()
        .expect("ffmpeg is required for this opt-in native regression");
    assert!(
        result.status.success(),
        "ffmpeg fixture: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

/// Waits for a bounded, observable playback condition and surfaces native failures.
fn wait_for(
    player: &mut MpvBackend,
    description: &str,
    condition: impl Fn(&PlaybackStatus) -> bool,
) -> PlaybackStatus {
    let deadline = Instant::now() + WAIT_LIMIT;
    loop {
        for _ in 0..32 {
            match player.poll_event().expect("native playback event") {
                Some(PlaybackEvent::Ended(end)) if end.reason == PlaybackEndReason::Error => {
                    panic!("native playback failed during {description}: {end:?}");
                }
                Some(PlaybackEvent::ProcessExited { diagnostic }) => {
                    panic!("mpv exited during {description}: {diagnostic:?}");
                }
                None => break,
                _ => {}
            }
        }
        let status = player.status().expect("native playback status");
        if condition(&status) {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}: {status:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// Identifies decoded media by duration, independently of its supplied display title.
fn is_track(status: &PlaybackStatus, seconds: u64) -> bool {
    !status.idle
        && status.duration.is_some_and(|duration| {
            duration.abs_diff(Duration::from_secs(seconds)) < Duration::from_millis(100)
        })
}

/// Constructs an exact direct-media selection with an application-owned cache key.
fn direct_input(location: impl Into<String>, identity: &str) -> PlaybackInput {
    let mut input = PlaybackInput::new(location);
    input.bypass_ytdl = true;
    input.keep_open = true;
    input.cache_identity = Some(identity.to_owned());
    input
}

/// Reproduces remote A -> local B -> remote C -> A, including saved-position replay.
fn exercise_switches(
    player: &mut MpvBackend,
    root: &Path,
    extension: &str,
    mut original: LocalSource,
    mut input: PlaybackInput,
    disconnect_origin: bool,
) {
    player.play(&input).expect("start original remote audio");
    wait_for(player, "original remote audio fully buffered", |status| {
        is_track(status, ORIGINAL_SECONDS)
            && status.buffered_ranges.iter().any(|range| {
                range.start < Duration::from_secs(1)
                    && range.end >= Duration::from_secs(ORIGINAL_SECONDS - 1)
            })
    });
    assert!(
        original.request_count() > 0,
        "initial load must fetch its source"
    );
    player
        .command(PlayerCommand::SeekAbsolute(Duration::from_secs(7)))
        .expect("choose saved position");
    player
        .command(PlayerCommand::SetPaused(true))
        .expect("pause before switching away");
    let saved = wait_for(player, "saved original position", |status| {
        status.paused
            && status.position >= Duration::from_secs(6)
            && status.position < Duration::from_secs(9)
    })
    .position;
    if disconnect_origin {
        original.disconnect();
    }
    let original_requests = original.request_count();

    let local_path = root.join(format!("local.{extension}"));
    generate_audio(&local_path, 11, 880);
    let mut local = PlaybackInput::new(local_path.to_string_lossy());
    local.bypass_ytdl = true;
    player.play(&local).expect("switch to local audio");
    wait_for(player, "distinct local audio", |status| {
        is_track(status, 11)
    });

    let other_path = root.join(format!("other.{extension}"));
    generate_audio(&other_path, OTHER_SECONDS, 660);
    let other = LocalSource::new(
        fs::read(other_path).expect("other fixture bytes"),
        extension,
    );
    player
        .play(&direct_input(other.url.clone(), "native:other"))
        .expect("switch to another remote item");
    wait_for(player, "distinct other remote audio", |status| {
        is_track(status, OTHER_SECONDS)
    });
    assert!(
        other.request_count() > 0,
        "a different identity must fetch different bytes"
    );

    input.start_at = saved;
    if input.bypass_ytdl {
        // A refreshed/expired signed URL must not invalidate already-retained
        // bytes belonging to the same exact application-owned media identity.
        input.location.push_str("?expired-signature=fixture");
    }
    let started = Instant::now();
    player.play(&input).expect("return to cached original");
    let resumed = wait_for(player, "cached original at saved position", |status| {
        is_track(status, ORIGINAL_SECONDS)
            && !status.paused
            && status.position >= saved.saturating_sub(Duration::from_millis(100))
            && status.position < saved + Duration::from_secs(2)
    });
    eprintln!(
        "{extension}: cached resume {:?}; position {:?}; no origin requests",
        started.elapsed(),
        resumed.position
    );
    wait_for(player, "cached audio continues decoding", |status| {
        is_track(status, ORIGINAL_SECONDS)
            && status.position > resumed.position + Duration::from_millis(150)
    });
    assert_eq!(
        original.request_count(),
        original_requests,
        "replay must not contact the original origin"
    );
    player.shutdown().expect("stop native playback");
}

/// Cached original bytes survive several selections and decode offline in both containers.
#[test]
#[ignore = "requires native mpv/ffmpeg; uses generated audio and owned loopback only"]
fn native_ram_cache_returns_to_ogg_and_webm_after_other_tracks() {
    for extension in ["opus", "webm"] {
        let directory = tempfile::tempdir().expect("private native fixture");
        let source = directory.path().join(format!("original.{extension}"));
        generate_audio(&source, ORIGINAL_SECONDS, 440);
        let original =
            LocalSource::new(fs::read(source).expect("original fixture bytes"), extension);
        let input = direct_input(original.url.clone(), "native:original");
        let mut process =
            ProcessPlaybackConfig::from_config(&Config::for_dir(directory.path().join("config")));
        process.audio_output = AudioOutputDriver::Null;
        let cache = RamPlaybackCache::for_test(CACHE_BYTES).expect("owned loopback RAM cache");
        let mut player =
            MpvBackend::spawn_with_ram_cache(&process, cache).expect("native null-output mpv");
        // The online case detects even redundant validation requests; the
        // disconnected case proves replay does not require its origin at all.
        exercise_switches(
            &mut player,
            directory.path(),
            extension,
            original,
            input,
            extension == "webm",
        );
    }
}

/// A successful HTTP body cut short by an upstream failure must not become a
/// fake natural end. The ordinary source remains playable for a single retry.
#[test]
#[ignore = "requires native mpv/ffmpeg; generated PCM and owned loopback only"]
fn native_ram_cache_midstream_failure_does_not_hold_false_eof() {
    let directory = tempfile::tempdir().expect("private failure fixture");
    let source = directory.path().join("partial.wav");
    generate_audio(&source, ORIGINAL_SECONDS, 440);
    let source = LocalSource::with_range_failure(
        fs::read(source).expect("PCM fixture bytes"),
        "wav",
        Some(super::BLOCK_BYTES as usize),
    );
    let cache = RamPlaybackCache::for_test(CACHE_BYTES).expect("owned RAM cache");
    let mut process =
        ProcessPlaybackConfig::from_config(&Config::for_dir(directory.path().join("config")));
    process.audio_output = AudioOutputDriver::Null;
    let mut player = MpvBackend::spawn_with_ram_cache(&process, cache.clone()).expect("native mpv");
    let key = "native:partial-source";
    let input = direct_input(source.url.clone(), key);
    player
        .command(PlayerCommand::SetSpeed(3.0))
        .expect("accelerate the bounded regression");
    player
        .play(&input)
        .expect("start partially failing cached stream");
    wait_for(
        &mut player,
        "decoded audio beyond the failed cache block",
        |status| {
            is_track(status, ORIGINAL_SECONDS)
                && !status.paused
                && status.position >= Duration::from_secs(9)
        },
    );
    assert!(
        cache.is_failed(key),
        "fixture must exercise an interrupted successful RAM response"
    );
    player.shutdown().expect("stop failed-cache fixture");
}

/// The canonical YouTube hook route resolves once, then replays without the extractor or origin.
#[cfg(all(unix, feature = "yt-dlp"))]
#[test]
#[ignore = "requires native mpv/ffmpeg; fake local extractor, no YouTube requests"]
fn native_ram_cache_canonical_youtube_replay_skips_extractor() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("private canonical fixture");
    let source = directory.path().join("original.webm");
    generate_audio(&source, ORIGINAL_SECONDS, 440);
    let original = LocalSource::new(fs::read(source).expect("original fixture bytes"), "webm");
    let calls = directory.path().join("extractor-calls");
    let extractor = directory.path().join("fake-yt-dlp");
    let canonical = "https://www.youtube.com/watch?v=ramfixture1";
    let metadata = serde_json::json!({
        "id": "ramfixture1", "title": "Synthetic YouTube fixture", "extractor_key": "Youtube",
        "webpage_url": canonical, "url": original.url, "ext": "webm", "protocol": "http",
        "acodec": "opus", "vcodec": "none", "duration": ORIGINAL_SECONDS,
    });
    let script = format!(
        "#!/bin/sh\nprintf 'called\\n' >> '{}'\nprintf '%s\\n' '{}'\n",
        calls.to_string_lossy().replace('\'', "'\\''"),
        metadata.to_string().replace('\'', "'\\''"),
    );
    fs::write(&extractor, script).expect("private fake extractor");
    fs::set_permissions(&extractor, fs::Permissions::from_mode(0o700)).expect("executable fixture");
    let mut process =
        ProcessPlaybackConfig::from_config(&Config::for_dir(directory.path().join("config")));
    process.audio_output = AudioOutputDriver::Null;
    process.yt_dlp_executable = extractor;
    let cache = RamPlaybackCache::for_test(CACHE_BYTES).expect("owned loopback RAM cache");
    let mut player =
        MpvBackend::spawn_with_ram_cache(&process, cache).expect("native null-output mpv");
    let mut input = PlaybackInput::new(canonical);
    input.cache_identity = Some("youtube:ramfixture1".to_owned());
    input.keep_open = true;
    exercise_switches(&mut player, directory.path(), "webm", original, input, true);
    assert_eq!(
        fs::read_to_string(calls).expect("extractor calls"),
        "called\n",
        "returning to cached YouTube must skip yt-dlp entirely"
    );
}
