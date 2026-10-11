//! Deterministic native radio playback without public stations or audio hardware.
//!
//! The fixtures encode a short generated tone with `FFmpeg`, serve it over loopback
//! HTTP, and exercise Youta's real mpv process adapter with null audio output.
//! The ICY case inserts real metadata blocks into an MPEG audio stream, rather than
//! mocking mpv's IPC responses. No fixture media is committed to the repository.
//!
//! ```text
//! YOUTA_RUN_RADIO_FIXTURE_TEST=1 cargo test --locked --no-default-features \
//!   --features backend-mpv,radio --test radio_playback -- --ignored
//! ```
//!
//! `YOUTA_TEST_MPV` and `YOUTA_TEST_FFMPEG` may select alternative executables.
//! All child processes, sockets and worker threads have bounded waits and are
//! cleaned up when a test fails as well as when it succeeds.

#![cfg(all(any(unix, windows), feature = "backend-mpv", feature = "radio"))]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use youta::playback::mpv::MpvBackend;
use youta::playback::{
    AudioOutputDriver, AudiophilePlaybackOptions, PlaybackBackend, PlaybackEvent, PlaybackInput,
    PlaybackProfile, ProcessPlaybackConfig,
};

const WAIT_LIMIT: Duration = Duration::from_secs(20);
const IO_LIMIT: Duration = Duration::from_secs(2);
const OUTPUT_LIMIT: u64 = 16 * 1024;
const ICY_INTERVAL: usize = 8192;
const ICY_TITLE: &str = "Youta fixture artist - Local radio track";

/// Always terminates and reaps the bounded, non-shell fixture encoder.
struct Encoder(Child);

impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Generates known audio instead of relying on station availability or downloads.
fn encode_fixture(root: &Path, extension: &str, codec: &str) -> Vec<u8> {
    let path = root.join(format!("tone.{extension}"));
    let mut encoder = Encoder(
        Command::new(std::env::var_os("YOUTA_TEST_FFMPEG").unwrap_or_else(|| "ffmpeg".into()))
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=20",
                "-c:a",
                codec,
                "-threads",
                "1",
                "-y",
            ])
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the local fixture encoder"),
    );
    let stderr = encoder.0.stderr.take().expect("encoder error pipe");
    // Draining only the diagnostic allowance keeps memory bounded. If an
    // unexpected encoder floods stderr, the process deadline still kills it.
    let diagnostic = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.take(OUTPUT_LIMIT).read_to_end(&mut bytes).unwrap();
        bytes
    });
    let deadline = Instant::now() + WAIT_LIMIT;
    let status = loop {
        if let Some(status) = encoder.0.try_wait().expect("poll fixture encoder") {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        thread::sleep(Duration::from_millis(20));
    };
    drop(encoder);
    let diagnostic = diagnostic.join().expect("join fixture encoder diagnostics");
    assert!(
        status.is_some_and(|status| status.success()),
        "fixture encoding failed or timed out: {status:?}; {}",
        String::from_utf8_lossy(&diagnostic)
    );
    let bytes = std::fs::read(path).expect("read generated fixture");
    assert!(!bytes.is_empty() && bytes.len() < 2 * 1024 * 1024);
    bytes
}

/// Inserts padded ICY metadata after every complete audio interval.
fn with_icy_metadata(audio: &[u8]) -> Vec<u8> {
    let metadata = format!("StreamTitle='{ICY_TITLE}';");
    let blocks = metadata.len().div_ceil(16);
    let mut body = Vec::new();
    for chunk in audio.chunks(ICY_INTERVAL) {
        body.extend_from_slice(chunk);
        if chunk.len() == ICY_INTERVAL {
            body.push(u8::try_from(blocks).expect("short fixture metadata"));
            body.extend_from_slice(metadata.as_bytes());
            body.resize(body.len() + blocks * 16 - metadata.len(), 0);
        }
    }
    body
}

/// Owns a loopback-only HTTP stream and its bounded, joinable worker.
struct RadioServer {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl RadioServer {
    /// Serves finite audio without Content-Length, like a non-seekable station.
    fn start(audio: Vec<u8>, content_type: &'static str, icy: bool) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind radio fixture");
        let address = listener.local_addr().expect("loopback fixture address");
        listener.set_nonblocking(true).expect("nonblocking accept");
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + WAIT_LIMIT * 2;
            while !worker_stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = serve_request(&mut stream, &audio, content_type, icy, &worker_stop);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("radio fixture accept failed: {error}"),
                }
            }
        });
        Self {
            address,
            stop,
            worker: Some(worker),
        }
    }

    /// Returns the direct stream URL without contacting DNS or public hosts.
    fn url(&self) -> String {
        format!("http://{}/stream", self.address)
    }
}

impl Drop for RadioServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Reads at most one bounded HTTP header and writes audio with per-I/O deadlines.
fn serve_request(
    stream: &mut TcpStream,
    audio: &[u8],
    content_type: &str,
    icy: bool,
    stop: &AtomicBool,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(IO_LIMIT))?;
    stream.set_write_timeout(Some(IO_LIMIT))?;
    let deadline = Instant::now() + IO_LIMIT;
    let mut request = Vec::new();
    let mut buffer = [0; 1024];
    while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
        if request.len() >= 8192 || Instant::now() >= deadline || stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..read]);
    }
    let metadata_header = if icy {
        format!("icy-name: Youta local fixture\r\nicy-metaint: {ICY_INTERVAL}\r\n")
    } else {
        String::new()
    };
    write!(
        stream,
        "HTTP/1.0 200 OK\r\nContent-Type: {content_type}\r\n{metadata_header}Connection: close\r\n\r\n"
    )?;
    for chunk in audio.chunks(8192) {
        if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
            break;
        }
        stream.write_all(chunk)?;
    }
    Ok(())
}

/// Proves decoded time advances through production IPC, plus ICY when requested.
fn assert_radio_playback(extension: &str, codec: &str, content_type: &'static str, icy: bool) {
    assert_eq!(
        std::env::var("YOUTA_RUN_RADIO_FIXTURE_TEST").as_deref(),
        Ok("1"),
        "set YOUTA_RUN_RADIO_FIXTURE_TEST=1 to run native radio fixtures"
    );
    let temporary = tempfile::tempdir().expect("private radio fixture directory");
    let audio = encode_fixture(temporary.path(), extension, codec);
    let body = if icy {
        with_icy_metadata(&audio)
    } else {
        audio
    };
    let server = RadioServer::start(body, content_type, icy);
    let config = ProcessPlaybackConfig {
        mpv_executable: std::env::var_os("YOUTA_TEST_MPV").map_or_else(|| "mpv".into(), Into::into),
        yt_dlp_executable: "unused-yt-dlp".into(),
        runtime_dir: temporary.path().join("runtime"),
        audio_output: AudioOutputDriver::Null,
        audio_device: None,
        profile: PlaybackProfile::Balanced,
        audiophile: AudiophilePlaybackOptions::default(),
    };
    let mut backend = MpvBackend::spawn(&config).expect("start real radio backend");
    let mut input = PlaybackInput::new(server.url());
    input.bypass_ytdl = true;
    backend.play(&input).expect("play the loopback HTTP stream");
    let deadline = Instant::now() + WAIT_LIMIT;
    let mut baseline = None;
    let mut advance = Duration::ZERO;
    let mut title_seen = false;
    loop {
        let status = backend.status().expect("read real radio playback status");
        for _ in 0..32 {
            match backend.poll_event().expect("read radio lifecycle event") {
                Some(PlaybackEvent::Ended(end)) => panic!("radio ended prematurely: {end:?}"),
                Some(PlaybackEvent::ProcessExited { diagnostic }) => {
                    panic!("radio process exited: {diagnostic:?}");
                }
                None => break,
                Some(_) => {}
            }
        }
        if !status.idle && !status.paused {
            let first = *baseline.get_or_insert(status.position);
            advance = advance.max(status.position.saturating_sub(first));
        }
        title_seen |= status.stream_title.as_deref() == Some(ICY_TITLE);
        if advance >= Duration::from_secs(2) && (!icy || title_seen) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{codec} HTTP radio failed: advance={advance:?}, ICY title seen={title_seen}, status={status:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
    backend.shutdown().expect("stop fixture playback cleanly");
}

/// A deterministic FLAC station must decode through the real production backend.
#[test]
#[ignore = "requires local mpv and ffmpeg; opt in with YOUTA_RUN_RADIO_FIXTURE_TEST=1"]
fn local_flac_radio_advances() {
    assert_radio_playback("flac", "flac", "audio/flac", false);
}

/// A deterministic Ogg Opus station must decode through the production backend.
#[test]
#[ignore = "requires local mpv and ffmpeg; opt in with YOUTA_RUN_RADIO_FIXTURE_TEST=1"]
fn local_ogg_opus_radio_advances() {
    assert_radio_playback("opus", "libopus", "audio/ogg", false);
}

/// Actual interleaved ICY metadata must reach Youta while the audio advances.
#[test]
#[ignore = "requires local mpv and ffmpeg; opt in with YOUTA_RUN_RADIO_FIXTURE_TEST=1"]
fn local_icy_radio_exposes_stream_title() {
    // MPEG Layer II uses FFmpeg's built-in encoder, so this fixture does not
    // require the optional libmp3lame build dependency just to exercise ICY.
    assert_radio_playback("mp2", "mp2", "audio/mpeg", true);
}
