//! Opt-in real-mpv checks for retaining seekable media at its natural end.
//!
//! These tests create a small PCM WAV locally and use mpv's null audio output;
//! they require neither external network access, yt-dlp, ffmpeg, nor an audio device.
//! Loading-seek checks use a gated loopback HTTP fixture.
//! Run explicitly with:
//!
//! ```text
//! cargo test --no-default-features --features backend-mpv --test mpv_end_pause -- --ignored
//! ```
//!
//! Set `YOUTA_TEST_MPV` to select another mpv executable. Backend ownership
//! ensures the child is stopped and reaped even when an assertion unwinds.

#![cfg(all(any(unix, windows), feature = "backend-mpv"))]

use std::path::Path;
use std::time::{Duration, Instant};

use youta::playback::mpv::MpvBackend;
use youta::playback::threaded::ThreadedBackend;
use youta::playback::{
    AudioOutputDriver, AudiophilePlaybackOptions, PlaybackBackend, PlaybackEndReason,
    PlaybackEvent, PlaybackInput, PlaybackProfile, PlaybackStatus, PlayerCommand,
    ProcessPlaybackConfig,
};

const WAIT_LIMIT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const FIXTURE_DURATION: Duration = Duration::from_secs(2);

/// Counts lifecycle notifications so a seek cannot silently become a reload.
#[derive(Debug, Default)]
struct Events {
    loaded: usize,
    started: usize,
    held: usize,
    eof: usize,
    stopped: usize,
}

impl Events {
    /// Drains a bounded event batch and fails immediately on backend errors.
    fn observe(&mut self, backend: &mut impl PlaybackBackend) -> PlaybackStatus {
        let status = backend.status().expect("read real mpv playback status");
        for _ in 0..32 {
            match backend.poll_event().expect("read real mpv lifecycle event") {
                Some(PlaybackEvent::MediaLoaded) => self.loaded += 1,
                Some(PlaybackEvent::PlaybackStarted) => self.started += 1,
                Some(PlaybackEvent::EndOfFileHeld) => self.held += 1,
                Some(PlaybackEvent::Ended(end)) => match end.reason {
                    PlaybackEndReason::Eof => self.eof += 1,
                    PlaybackEndReason::Stop => self.stopped += 1,
                    _ => panic!("unexpected mpv end event: {end:?}"),
                },
                Some(PlaybackEvent::ProcessExited { diagnostic }) => {
                    panic!("mpv exited unexpectedly: {diagnostic:?}");
                }
                None => return status,
            }
        }
        panic!("mpv emitted an unbounded lifecycle event burst: {self:?}");
    }
}

/// Polls until one observable transition occurs, with a bounded deadline.
fn wait_for(
    backend: &mut impl PlaybackBackend,
    events: &mut Events,
    description: &str,
    condition: impl Fn(&PlaybackStatus, &Events) -> bool,
) -> PlaybackStatus {
    let deadline = Instant::now() + WAIT_LIMIT;
    loop {
        let status = events.observe(backend);
        if condition(&status, events) {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}; status: {status:?}; events: {events:?}"
        );
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Generates two seconds of mono 16-bit PCM without an encoder dependency.
fn write_wav(path: &Path) {
    const SAMPLE_RATE: u32 = 8_000;
    const SAMPLE_COUNT: u32 = SAMPLE_RATE * 2;
    const DATA_BYTES: u32 = SAMPLE_COUNT * 2;
    let mut bytes = Vec::with_capacity(44 + usize::try_from(DATA_BYTES).unwrap());
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + DATA_BYTES).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    bytes.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&DATA_BYTES.to_le_bytes());
    for index in 0..SAMPLE_COUNT {
        let sample: i16 = if index % 32 < 16 { 1_000 } else { -1_000 };
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    std::fs::write(path, bytes).expect("write the small local WAV fixture");
}

/// Starts the production process adapter with no external service or device.
fn fixture(root: &Path, keep_open: bool) -> (MpvBackend, PlaybackInput) {
    let media_path = root.join("end-pause.wav");
    write_wav(&media_path);
    let config = ProcessPlaybackConfig {
        mpv_executable: std::env::var_os("YOUTA_TEST_MPV").map_or_else(|| "mpv".into(), Into::into),
        yt_dlp_executable: "unused-yt-dlp".into(),
        runtime_dir: root.join("runtime"),
        audio_output: AudioOutputDriver::Null,
        audio_device: None,
        profile: PlaybackProfile::Balanced,
        audiophile: AudiophilePlaybackOptions::default(),
    };
    let mut input = PlaybackInput::new(media_path.to_str().expect("UTF-8 fixture path"));
    input.keep_open = keep_open;
    input.bypass_ytdl = true;
    (
        MpvBackend::spawn(&config).expect("start the real mpv backend"),
        input,
    )
}

/// Rewinding held EOF resumes it immediately and still honors an explicit Stop.
#[test]
#[ignore = "requires mpv; run the command documented at the top of this file"]
fn held_eof_is_paused_seekable_and_replays_without_reloading() {
    let temporary = tempfile::tempdir().expect("private test directory");
    let (mut backend, input) = fixture(temporary.path(), true);
    let mut events = Events::default();
    backend.play(&input).expect("start the WAV fixture");
    let ended = wait_for(
        &mut backend,
        &mut events,
        "paused and loaded EOF",
        |status, events| {
            // Status properties use separate IPC requests: EOF may occur
            // between the first time-pos read and the later pause read.
            // Wait for the next position snapshot instead of treating the
            // first held notification as an atomic status observation.
            events.held == 1
                && !status.idle
                && status.paused
                && status.position >= FIXTURE_DURATION.saturating_sub(Duration::from_millis(100))
        },
    );
    assert!(ended.seeking_available(), "held EOF must remain seekable");
    assert_eq!(ended.duration, Some(FIXTURE_DURATION));
    assert!(
        ended.position >= FIXTURE_DURATION.saturating_sub(Duration::from_millis(100)),
        "held EOF should show the end position: {ended:?}"
    );
    assert_eq!(events.loaded, 1);
    assert_eq!(events.eof, 0);

    let held_until = Instant::now() + Duration::from_millis(250);
    while Instant::now() < held_until {
        let status = events.observe(&mut backend);
        assert!(!status.idle && status.paused && status.seeking_available());
        assert!(status.position.abs_diff(ended.position) < Duration::from_millis(50));
        assert_eq!(events.loaded, 1);
        assert_eq!(events.held, 1, "held EOF must not repeat on every poll");
        assert_eq!(events.eof, 0);
        std::thread::sleep(POLL_INTERVAL);
    }

    backend
        .command(PlayerCommand::SeekRelative(-1))
        .expect("rewind the retained media without loading it again");
    let rewound = wait_for(
        &mut backend,
        &mut events,
        "backward seek automatically resumes audio",
        |status, _| {
            !status.idle && !status.paused && status.position < Duration::from_millis(1_500)
        },
    );
    assert_eq!(events.loaded, 1, "seeking must not reload the media");
    assert_eq!(events.held, 1);
    assert_eq!(events.eof, 0);
    wait_for(
        &mut backend,
        &mut events,
        "position advancing after resume",
        |status, _| {
            !status.paused && status.position > rewound.position + Duration::from_millis(100)
        },
    );
    wait_for(
        &mut backend,
        &mut events,
        "a second held EOF after resumed playback",
        |status, events| events.held == 2 && !status.idle && status.paused,
    );
    assert_eq!(events.loaded, 1, "resume must reuse the loaded media");
    assert_eq!(events.eof, 0);

    backend
        .command(PlayerCommand::Stop)
        .expect("explicitly stop retained media");
    let stopped = wait_for(
        &mut backend,
        &mut events,
        "an unloaded explicit Stop",
        |status, events| status.idle && events.stopped == 1,
    );
    assert!(!stopped.seeking_available());
    assert_eq!(events.eof, 0, "Stop must not be reported as natural EOF");
}

/// Releasing a retained end produces the normal EOF used by autoplay.
#[test]
#[ignore = "requires mpv; run the command documented at the top of this file"]
fn releasing_held_eof_reports_natural_completion() {
    let temporary = tempfile::tempdir().expect("private test directory");
    let (mut backend, input) = fixture(temporary.path(), true);
    let mut events = Events::default();
    backend.play(&input).expect("start the WAV fixture");
    wait_for(
        &mut backend,
        &mut events,
        "held EOF before release",
        |status, events| events.held == 1 && !status.idle && status.paused,
    );
    backend
        .command(PlayerCommand::ReleaseEndOfFile)
        .expect("release retained media for ordinary completion");
    wait_for(
        &mut backend,
        &mut events,
        "natural EOF after release",
        |status, events| status.idle && events.eof == 1,
    );
    assert_eq!(events.loaded, 1);
    assert_eq!(events.held, 1);
    assert_eq!(events.stopped, 0);
}

/// Inputs that do not opt in retain mpv's ordinary unload-at-EOF behavior.
#[test]
#[ignore = "requires mpv; run the command documented at the top of this file"]
fn ordinary_input_unloads_at_eof() {
    let temporary = tempfile::tempdir().expect("private test directory");
    let (mut backend, input) = fixture(temporary.path(), false);
    let mut events = Events::default();
    backend.play(&input).expect("start the WAV fixture");
    wait_for(
        &mut backend,
        &mut events,
        "ordinary natural EOF",
        |status, events| status.idle && events.eof == 1,
    );
    assert_eq!(events.loaded, 1);
    assert_eq!(events.held, 0);
    assert_eq!(events.stopped, 0);
}

/// mpv's native repeat loops before keep-open can hold the end of the file.
#[test]
#[ignore = "requires mpv; run the command documented at the top of this file"]
fn repeat_loops_without_pausing_at_eof() {
    let temporary = tempfile::tempdir().expect("private test directory");
    let (mut backend, input) = fixture(temporary.path(), true);
    let mut events = Events::default();
    backend
        .command(PlayerCommand::SetRepeat(true))
        .expect("enable native repeat");
    backend.play(&input).expect("start the WAV fixture");
    wait_for(
        &mut backend,
        &mut events,
        "position approaching the first loop boundary",
        |status, _| status.position >= Duration::from_millis(1_200),
    );
    wait_for(
        &mut backend,
        &mut events,
        "position wrapping to the next repeat",
        |status, events| {
            assert_eq!(events.held, 0, "repeat must not emit held EOF");
            assert_eq!(events.eof, 0, "repeat must not unload the media");
            assert!(!status.idle && !status.paused);
            status.position < Duration::from_millis(700)
        },
    );
    assert_eq!(events.loaded, 1, "native repeat must not reload the file");

    backend
        .command(PlayerCommand::SetRepeat(false))
        .expect("disable repeat during playback");
    wait_for(
        &mut backend,
        &mut events,
        "held EOF after repeat is disabled",
        |status, events| events.held == 1 && !status.idle && status.paused,
    );
    assert_eq!(events.eof, 0);
}

/// Enabling repeat after completion restarts retained media through release.
#[test]
#[ignore = "requires mpv; run the command documented at the top of this file"]
fn enabling_repeat_at_held_eof_restarts_playback() {
    let temporary = tempfile::tempdir().expect("private test directory");
    let (mut backend, input) = fixture(temporary.path(), true);
    let mut events = Events::default();
    backend.play(&input).expect("start the WAV fixture");
    wait_for(
        &mut backend,
        &mut events,
        "held EOF before enabling repeat",
        |status, events| events.held == 1 && !status.idle && status.paused,
    );
    backend
        .command(PlayerCommand::SetRepeat(true))
        .expect("enable repeat while the ended item is retained");
    backend
        .command(PlayerCommand::ReleaseEndOfFile)
        .expect("release the retained end after enabling repeat");
    wait_for(
        &mut backend,
        &mut events,
        "playing again after enabling repeat at held EOF",
        |status, _| {
            !status.idle
                && !status.paused
                && status.position > Duration::from_millis(100)
                && status.position < Duration::from_millis(1_500)
        },
    );
    assert_eq!(events.loaded, 1, "repeat must reuse the retained media");
    assert_eq!(events.held, 1);
    assert_eq!(events.eof, 0);
    assert_eq!(events.stopped, 0);
}

/// A seek beyond retained EOF leaves the loaded item available for release.
#[test]
#[ignore = "requires mpv; run the command documented at the top of this file"]
fn forward_seek_at_held_eof_keeps_the_item_loaded() {
    for command in [
        PlayerCommand::SeekRelative(5),
        PlayerCommand::SeekPercent(100.0),
        PlayerCommand::SeekAbsolute(FIXTURE_DURATION),
        PlayerCommand::SeekAbsolute(Duration::from_secs(5)),
    ] {
        assert_forward_seek_retains_eof(command);
    }
}

/// Exercises one at-or-beyond-end seek against a freshly held real fixture.
fn assert_forward_seek_retains_eof(command: PlayerCommand) {
    let release_description = format!("natural EOF after {command:?} from held EOF");
    let temporary = tempfile::tempdir().expect("private test directory");
    let (mut backend, input) = fixture(temporary.path(), true);
    let mut events = Events::default();
    backend.play(&input).expect("start the WAV fixture");
    wait_for(
        &mut backend,
        &mut events,
        "held EOF before forward seek",
        |status, events| events.held == 1 && !status.idle && status.paused,
    );
    backend
        .command(command)
        .expect("seek forward past retained EOF");
    let deadline = Instant::now() + Duration::from_millis(250);
    while Instant::now() < deadline {
        let status = events.observe(&mut backend);
        assert!(!status.idle && status.paused && status.seeking_available());
        std::thread::sleep(POLL_INTERVAL);
    }
    assert_eq!(events.loaded, 1);
    assert_eq!(events.eof, 0);
    backend
        .command(PlayerCommand::ReleaseEndOfFile)
        .expect("release retained EOF after a forward seek");
    wait_for(
        &mut backend,
        &mut events,
        &release_description,
        |status, events| status.idle && events.eof == 1,
    );
}

/// The production worker preserves held/seek/release ordering around mpv.
#[test]
#[ignore = "requires mpv; run the command documented at the top of this file"]
fn threaded_backend_retains_rewinds_and_releases_eof() {
    let temporary = tempfile::tempdir().expect("private test directory");
    let (backend, input) = fixture(temporary.path(), true);
    let mut backend = ThreadedBackend::new(backend);
    let mut events = Events::default();
    backend.play(&input).expect("start WAV through the worker");
    wait_for(
        &mut backend,
        &mut events,
        "held EOF through the production worker",
        |status, events| events.held == 1 && !status.idle && status.paused,
    );
    backend
        .command(PlayerCommand::SeekRelative(-1))
        .expect("rewind retained media through the worker");
    let rewound = wait_for(
        &mut backend,
        &mut events,
        "backward seek automatically resumes through the worker",
        |status, _| {
            !status.idle && !status.paused && status.position < Duration::from_millis(1_500)
        },
    );
    assert!(rewound.seeking_available());
    assert_eq!(events.loaded, 1);
    assert_eq!(events.eof, 0);
    wait_for(
        &mut backend,
        &mut events,
        "resumed playback through the worker",
        |status, _| {
            !status.idle
                && !status.paused
                && status.position > rewound.position + Duration::from_millis(100)
        },
    );
    wait_for(
        &mut backend,
        &mut events,
        "second held EOF through the worker",
        |status, events| events.held == 2 && !status.idle && status.paused,
    );
    backend
        .command(PlayerCommand::ReleaseEndOfFile)
        .expect("release held EOF through the worker");
    wait_for(
        &mut backend,
        &mut events,
        "natural EOF after release through the worker",
        |status, events| status.idle && events.eof == 1,
    );
    assert_eq!(events.loaded, 1, "the worker must retain the same load");
    assert_eq!(events.held, 2);
    assert_eq!(events.stopped, 0);
}

/// Absolute and percentage rewinds restart audio without an extra resume command.
#[test]
#[ignore = "requires mpv; run the command documented at the top of this file"]
fn backward_absolute_and_percent_seeks_resume_playback() {
    for command in [
        PlayerCommand::SeekAbsolute(Duration::from_secs(1)),
        PlayerCommand::SeekPercent(50.0),
    ] {
        let description = format!("automatic resume and PlaybackStarted after {command:?}");
        let temporary = tempfile::tempdir().expect("private test directory");
        let (mut backend, input) = fixture(temporary.path(), true);
        let mut events = Events::default();
        backend.play(&input).expect("start the WAV fixture");
        wait_for(
            &mut backend,
            &mut events,
            "held EOF before absolute or percentage rewind",
            |status, events| events.held == 1 && !status.idle && status.paused,
        );
        let started_before_seek = events.started;
        backend
            .command(command)
            .expect("rewind retained media to its midpoint");
        let rewound = wait_for(&mut backend, &mut events, &description, |status, events| {
            events.started > started_before_seek
                && !status.idle
                && !status.paused
                && status.position >= Duration::from_millis(750)
                && status.position < Duration::from_millis(1_500)
        });
        assert!(rewound.seeking_available());
        assert_eq!(events.loaded, 1, "rewind must not reload the retained file");
        assert_eq!(events.held, 1);
        assert_eq!(events.eof, 0);
        wait_for(
            &mut backend,
            &mut events,
            "advancing after absolute or percentage rewind",
            |status, _| {
                !status.idle
                    && !status.paused
                    && status.position > rewound.position + Duration::from_millis(100)
            },
        );
        wait_for(
            &mut backend,
            &mut events,
            "second held EOF after absolute or percentage rewind",
            |status, events| events.held == 2 && !status.idle && status.paused,
        );
        assert_eq!(events.loaded, 1, "resume must reuse the loaded file");
        assert_eq!(events.eof, 0);
        assert_eq!(events.stopped, 0);
    }
}

/// Seeking a user-paused item before EOF must not turn playback back on.
#[test]
#[ignore = "requires mpv; run the command documented at the top of this file"]
fn manually_paused_seeks_remain_paused() {
    for command in [
        PlayerCommand::SeekRelative(-1),
        PlayerCommand::SeekAbsolute(Duration::from_millis(250)),
        PlayerCommand::SeekPercent(12.5),
    ] {
        let description = format!("manually paused seek after {command:?}");
        let temporary = tempfile::tempdir().expect("private test directory");
        let (mut backend, input) = fixture(temporary.path(), true);
        let mut events = Events::default();
        backend.play(&input).expect("start the WAV fixture");
        wait_for(
            &mut backend,
            &mut events,
            "active playback before manual pause",
            |status, _| {
                !status.idle && !status.paused && status.position >= Duration::from_millis(500)
            },
        );
        backend
            .command(PlayerCommand::SetPaused(true))
            .expect("pause before the fixture reaches EOF");
        let manually_paused = wait_for(
            &mut backend,
            &mut events,
            "manual pause before EOF",
            |status, _| !status.idle && status.paused,
        );
        assert_eq!(events.held, 0, "this pause must not be a held EOF");
        let started_before_seek = events.started;
        backend
            .command(command)
            .expect("seek while manually paused");
        let rewound = wait_for(&mut backend, &mut events, &description, |status, events| {
            // Paused mpv time-pos includes output-buffer timing and need
            // not equal the requested seek coordinate exactly. A fresh
            // restart plus a lower position proves the seek happened;
            // the invariant under test is that playback stays paused.
            events.started > started_before_seek
                && !status.idle
                && status.paused
                && status.position < manually_paused.position
        });
        assert!(rewound.seeking_available());
        let paused_until = Instant::now() + Duration::from_millis(250);
        while Instant::now() < paused_until {
            let status = events.observe(&mut backend);
            // mpv may settle the reported time-pos after decoder restart,
            // even while paused. Observe its authoritative pause property
            // instead of requiring an unchanged buffered timestamp.
            assert!(!status.idle && status.paused && status.seeking_available());
            assert_eq!(events.loaded, 1);
            assert_eq!(events.held, 0);
            assert_eq!(events.eof, 0);
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

/// Holds a tiny HTTP audio response until the test has submitted its early seek.
struct DelayedAudio {
    url: String,
    requested: std::sync::mpsc::Receiver<()>,
    release: Option<std::sync::mpsc::Sender<()>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl DelayedAudio {
    /// Serves only the generated fixture, with bounded reads and teardown on panic.
    fn new(bytes: Vec<u8>) -> Self {
        use std::io::{BufRead as _, Write as _};
        use std::sync::atomic::{AtomicBool, Ordering};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/delayed.wav", listener.local_addr().unwrap());
        let (request, requested) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let worker_stop = std::sync::Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            let mut released = false;
            while !worker_stop.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => return,
                };
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(WAIT_LIMIT)).unwrap();
                stream.set_write_timeout(Some(WAIT_LIMIT)).unwrap();
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                for _ in 0..64 {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                if !released {
                    let _ = request.send(());
                    if wait.recv_timeout(WAIT_LIMIT).is_err() {
                        return;
                    }
                    released = true;
                }
                if worker_stop.load(Ordering::Acquire) {
                    return;
                }
                if write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                ).is_ok() {
                    let _ = stream.write_all(&bytes);
                }
            }
        });
        Self {
            url,
            requested,
            release: Some(release),
            stop,
            worker: Some(worker),
        }
    }

    /// Releases the response only after the test's commands have been accepted.
    fn release(&mut self) {
        self.release.take().unwrap().send(()).unwrap();
    }
}

impl Drop for DelayedAudio {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        self.release.take();
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !std::thread::panicking() {
                result.expect("delayed audio fixture worker");
            }
        }
    }
}

/// Timestamp clicks during extraction/loading are accepted; the last click wins.
#[test]
#[ignore = "requires mpv; run the command documented at the top of this file"]
fn timestamp_before_audio_load_is_applied_when_ready() {
    let temporary = tempfile::tempdir().expect("private test directory");
    let (mut backend, mut input) = fixture(temporary.path(), true);
    let mut server = DelayedAudio::new(std::fs::read(&input.location).unwrap());
    input.location.clone_from(&server.url);
    backend
        .play(&input)
        .expect("start the delayed HTTP fixture");
    server
        .requested
        .recv_timeout(WAIT_LIMIT)
        .expect("mpv is opening the stream");
    let mut events = Events::default();
    events.observe(&mut backend);
    assert_eq!(events.loaded, 0, "no audio headers have been sent yet");
    for target in [750, 1_250] {
        backend
            .command(PlayerCommand::SeekAbsolute(Duration::from_millis(target)))
            .expect("accept a timestamp before audio is loaded");
    }
    backend
        .command(PlayerCommand::SetPaused(true))
        .expect("retain a stable position after loading");
    server.release();
    wait_for(
        &mut backend,
        &mut events,
        "latest requested timestamp",
        |status, events| {
            events.loaded == 1
                && events.started > 0
                && !status.idle
                && status.paused
                && status.position >= Duration::from_millis(1_000)
                && status.position < Duration::from_millis(1_750)
        },
    );
    assert_eq!(
        events.loaded, 1,
        "the timestamp must not restart extraction"
    );
}

/// An abandoned load's timestamp must never seek a replacement file.
#[test]
#[ignore = "requires mpv; run the command documented at the top of this file"]
fn timestamp_before_audio_load_does_not_leak_after_stop_or_replacement() {
    for stop_first in [false, true] {
        let temporary = tempfile::tempdir().expect("private test directory");
        let (mut backend, input) = fixture(temporary.path(), true);
        let bytes = std::fs::read(&input.location).unwrap();
        let mut server = DelayedAudio::new(bytes.clone());
        let mut replacement_server = DelayedAudio::new(bytes);
        let mut delayed = input.clone();
        delayed.location.clone_from(&server.url);
        backend
            .play(&delayed)
            .expect("start the delayed HTTP fixture");
        server
            .requested
            .recv_timeout(WAIT_LIMIT)
            .expect("mpv is opening the stream");
        backend
            .command(PlayerCommand::SeekAbsolute(Duration::from_millis(1_250)))
            .expect("accept a timestamp before audio is loaded");
        if stop_first {
            backend
                .command(PlayerCommand::Stop)
                .expect("cancel the pending load");
        }
        let mut replacement = input;
        replacement.location.clone_from(&replacement_server.url);
        backend
            .play(&replacement)
            .expect("replace the loading fixture");
        replacement_server
            .requested
            .recv_timeout(WAIT_LIMIT)
            .expect("mpv is opening the replacement");
        backend
            .command(PlayerCommand::SetPaused(true))
            .expect("pause before the replacement receives audio bytes");
        server.release();
        replacement_server.release();
        let mut events = Events::default();
        wait_for(
            &mut backend,
            &mut events,
            "replacement starting at zero",
            |status, events| {
                events.loaded == 1
                    && events.started > 0
                    && !status.idle
                    && status.paused
                    && status.position < Duration::from_millis(500)
            },
        );
        assert_eq!(events.held, 0);
    }
}
