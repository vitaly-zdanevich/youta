//! Opt-in original-byte MP3 playback through the production cache proxy.
//!
//! Uses only generated audio and owned loopback listeners, never Archive.org.
//! Run with `cargo test --locked --lib --features archive-org,yt-dlp,backend-mpv
//! archive_playback_cache::native_tests::native_archive_mp3_cache_survives_upstream_disconnect
//! -- --ignored --exact --nocapture`. `YOUTA_TEST_MPV`, `YOUTA_TEST_FFMPEG`,
//! and `YOUTA_TEST_LAME` may select installed helper binaries.

#![cfg(all(feature = "backend-mpv", any(unix, windows)))]

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use url::Url;

use super::ArchivePlaybackCache;
use crate::playback::mpv::MpvBackend;
use crate::playback::{
    AudioOutputDriver, AudiophilePlaybackOptions, PlaybackBackend, PlaybackEvent, PlaybackInput,
    PlaybackProfile, PlaybackStatus, PlayerCommand, ProcessPlaybackConfig,
};

const WAIT_LIMIT: Duration = Duration::from_secs(15);
const SOURCE_ETAG: &str = "\"native-mp3-original-v1\"";

/// A bounded range origin that can be disconnected while the cache stays live.
struct LocalOrigin {
    url: Url,
    stopping: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
    worker: Option<thread::JoinHandle<()>>,
}

impl LocalOrigin {
    fn start(bytes: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("private origin listener");
        listener.set_nonblocking(true).expect("stoppable listener");
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let stopping = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let worker_stopping = Arc::clone(&stopping);
        let worker_requests = Arc::clone(&requests);
        let worker = thread::spawn(move || {
            while !worker_stopping.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("owned origin accept: {error}"),
                };
                // Winsock inherits the listener's nonblocking mode on accept.
                stream
                    .set_nonblocking(false)
                    .expect("blocking accepted stream");
                stream
                    .set_read_timeout(Some(Duration::from_millis(200)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(1);
                let mut header = Vec::new();
                let mut byte = [0];
                while header.len() < 16 * 1024 && Instant::now() < deadline {
                    if stream.read_exact(&mut byte).is_err() {
                        break;
                    }
                    header.push(byte[0]);
                    if header.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                if !header.ends_with(b"\r\n\r\n") {
                    continue;
                }
                let header = String::from_utf8(header).expect("ASCII HTTP request");
                let head = header.starts_with("HEAD ");
                assert!(
                    head || header.starts_with("GET "),
                    "read-only origin method"
                );
                worker_requests.fetch_add(1, Ordering::Relaxed);
                let requested = header_value(&header, "range");
                let if_range = header_value(&header, "if-range");
                let range = requested.filter(|_| if_range.is_none_or(|value| value == SOURCE_ETAG));
                let selected = range.map_or(Some((0, bytes.len() - 1)), |range| {
                    parse_byte_range(range, bytes.len())
                });
                let Some((start, end)) = selected else {
                    let response = format!(
                        "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                    continue;
                };
                let status = if range.is_some() {
                    "206 Partial Content"
                } else {
                    "200 OK"
                };
                let content_range = range.map_or_else(String::new, |_| {
                    format!("Content-Range: bytes {start}-{end}/{}\r\n", bytes.len())
                });
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: audio/mpeg\r\nAccept-Ranges: bytes\r\nETag: {SOURCE_ETAG}\r\nContent-Length: {}\r\n{content_range}Connection: close\r\n\r\n",
                    end - start + 1
                );
                if stream.write_all(response.as_bytes()).is_ok() && !head {
                    let _ = stream.write_all(&bytes[start..=end]);
                }
            }
        });
        Self {
            url,
            stopping,
            requests,
            worker: Some(worker),
        }
    }

    fn disconnect(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("owned origin worker");
        }
    }
}

impl Drop for LocalOrigin {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// Reads one case-insensitive HTTP header without accepting a prefix match.
fn header_value<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// Handles one closed, open-ended, or suffix byte range for the owned fixture.
fn parse_byte_range(range: &str, length: usize) -> Option<(usize, usize)> {
    let (start, end) = range.strip_prefix("bytes=")?.split_once('-')?;
    if start.is_empty() {
        let suffix = end.parse::<usize>().ok().filter(|size| *size > 0)?;
        return Some((length.saturating_sub(suffix), length.checked_sub(1)?));
    }
    let start = start.parse::<usize>().ok()?;
    let end = if end.is_empty() {
        length.checked_sub(1)?
    } else {
        end.parse::<usize>().ok()?.min(length.checked_sub(1)?)
    };
    (start <= end).then_some((start, end))
}

/// Generates audio plus distinctive tags so a demux/remux cannot pass byte equality.
fn generate_tagged_mp3(path: &Path) {
    let wave_path = path.with_extension("wav");
    let program = std::env::var_os("YOUTA_TEST_FFMPEG").unwrap_or_else(|| "ffmpeg".into());
    let mut generate = Command::new(program);
    generate
        .args([
            "-nostdin",
            "-v",
            "error",
            "-n",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100",
            "-t",
            "12",
            "-c:a",
            "pcm_s16le",
        ])
        .arg(&wave_path);
    run_fixture_helper(&mut generate, "FFmpeg PCM generation");
    // A standalone encoder also works when FFmpeg was built without libmp3lame.
    let program = std::env::var_os("YOUTA_TEST_LAME").unwrap_or_else(|| "lame".into());
    let mut encode = Command::new(program);
    encode
        .args([
            "--silent",
            "-b",
            "128",
            "--add-id3v2",
            "--tt",
            "Youta original-byte MP3 fixture",
            "--ta",
            "Generated locally",
            "--tc",
            "Original ID3 bytes must survive playback-cache saving",
        ])
        .arg(&wave_path)
        .arg(path);
    run_fixture_helper(&mut encode, "LAME tagged MP3 generation");
}

/// Runs a local fixture helper with an explicit deadline and visible diagnostics.
fn run_fixture_helper(command: &mut Command, description: &str) {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|error| panic!("cannot start {description}: {error}"));
    let deadline = Instant::now() + WAIT_LIMIT;
    loop {
        if let Some(status) = child.try_wait().expect("poll fixture generation") {
            assert!(status.success(), "{description} failed: {status}");
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{description} exceeded its bounded deadline");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Counts actual loaded generations rather than assuming a play command succeeded.
fn observe(player: &mut MpvBackend, loaded: &mut usize) -> PlaybackStatus {
    for _ in 0..64 {
        match player.poll_event().expect("native mpv event") {
            Some(PlaybackEvent::MediaLoaded) => *loaded += 1,
            Some(PlaybackEvent::ProcessExited { diagnostic }) => {
                panic!("native mpv exited: {diagnostic:?}")
            }
            Some(_) => {}
            None => return player.status().expect("native mpv status"),
        }
    }
    panic!("native mpv event stream exceeded fixture polling bound");
}

/// Real decoding, disconnected replay, and exact source/tag bytes share one cache.
#[test]
#[ignore = "requires native mpv, FFmpeg and LAME; generated loopback media only"]
fn native_archive_mp3_cache_survives_upstream_disconnect() {
    let directory = tempfile::tempdir().expect("private native MP3 fixture");
    let original_path = directory.path().join("original.mp3");
    generate_tagged_mp3(&original_path);
    assert!(fs::metadata(&original_path).unwrap().len() < 4 * 1024 * 1024);
    let original = fs::read(&original_path).expect("bounded original MP3 bytes");
    assert!(
        original.starts_with(b"ID3"),
        "fixture has original ID3v2 bytes"
    );
    assert_eq!(
        &original[original.len() - 128..original.len() - 125],
        b"TAG",
        "fixture has a trailing ID3v1 tag"
    );
    let mut origin = LocalOrigin::start(original.clone());
    let canonical =
        Url::parse("https://archive.org/download/youta-native-test/fixture.mp3").unwrap();
    let cache = ArchivePlaybackCache::start_with_origin(canonical.clone(), origin.url.clone())
        .expect("production original-byte proxy with an owned test origin");
    let config = ProcessPlaybackConfig {
        mpv_executable: std::env::var_os("YOUTA_TEST_MPV")
            .map_or_else(|| PathBuf::from("mpv"), PathBuf::from),
        yt_dlp_executable: directory.path().join("must-not-start-yt-dlp"),
        runtime_dir: directory.path().join("runtime"),
        audio_output: AudioOutputDriver::Null,
        audio_device: None,
        profile: PlaybackProfile::Balanced,
        audiophile: AudiophilePlaybackOptions::default(),
    };
    let mut player = MpvBackend::spawn(&config).expect("production native mpv backend");
    let mut input = PlaybackInput::new(cache.playback_url());
    input.bypass_ytdl = true;
    input.keep_open = true;
    player
        .play(&input)
        .expect("play through original-byte proxy");
    let mut loaded = 0;
    let deadline = Instant::now() + WAIT_LIMIT;
    let completed = loop {
        let status = observe(&mut player, &mut loaded);
        if loaded >= 1
            && !status.idle
            && let Some(completed) = cache.completed()
        {
            break completed;
        }
        assert!(
            Instant::now() < deadline,
            "playback did not produce complete original-byte coverage: {status:?}"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(completed.source_url(), &canonical);
    assert_eq!(completed.len(), original.len() as u64);
    assert!(
        completed.is_current(),
        "complete original identity is valid"
    );
    origin.disconnect();
    let requests_before = origin.requests.load(Ordering::Relaxed);
    assert!(requests_before > 0, "playback fetched the owned upstream");

    let destination = directory.path().join("downloads");
    fs::create_dir(&destination).expect("private download destination");
    let cancellation = AtomicBool::new(false);
    let prepared = crate::original_cache_download::prepare_cached_original(
        completed.path(),
        completed.len(),
        completed.source_url(),
        &destination,
        &cancellation,
    )
    .expect("stage exact completed original bytes");
    assert!(
        completed.is_current(),
        "publication retains its source lease"
    );
    let saved_path = prepared
        .publish(&destination, "Native original MP3", "fixture")
        .expect("publish through the production no-overwrite API");
    assert_eq!(
        saved_path.extension().and_then(std::ffi::OsStr::to_str),
        Some("mp3")
    );
    assert_eq!(
        fs::read(&saved_path).unwrap(),
        original,
        "all original bytes and both tags survive"
    );

    // A fresh load cannot be satisfied by the previous mpv demuxer packet cache.
    player
        .play(&input)
        .expect("reopen proxy after upstream disconnect");
    let deadline = Instant::now() + WAIT_LIMIT;
    loop {
        let status = observe(&mut player, &mut loaded);
        if loaded >= 2 && !status.idle && status.position > Duration::from_millis(100) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "offline MP3 replay did not start: {status:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    player
        .command(PlayerCommand::SetPaused(true))
        .expect("pause native seek fixture");
    for seconds in [8_u64, 1, 10] {
        player
            .command(PlayerCommand::SeekAbsolute(Duration::from_secs(seconds)))
            .expect("seek cached original MP3");
        let deadline = Instant::now() + WAIT_LIMIT;
        loop {
            let status = observe(&mut player, &mut loaded);
            if (status.position.as_secs_f64() - seconds as f64).abs() < 0.2 {
                assert!(status.paused, "cached access does not change pause state");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "offline MP3 seek did not complete: {status:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    assert_eq!(origin.requests.load(Ordering::Relaxed), requests_before);
    assert_eq!(fs::read(completed.path()).unwrap(), original);
    player.shutdown().expect("stop native MP3 fixture");
}
