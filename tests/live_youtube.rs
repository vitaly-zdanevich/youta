//! Opt-in live playback checks against `YouTube`.
//!
//! Unlike the deterministic process tests in `e2e.rs`, this target exercises
//! Youta's production mpv/yt-dlp integration and therefore needs network
//! access. CI invokes it explicitly; ordinary local `cargo test` runs compile
//! it but leave the ignored test dormant.

#![cfg(all(unix, feature = "backend-mpv", feature = "yt-dlp"))]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use youta::playback::mpv::MpvBackend;
use youta::playback::{
    AudioOutputDriver, AudiophilePlaybackOptions, PlaybackBackend, PlaybackEvent, PlaybackInput,
    PlaybackProfile, ProcessPlaybackConfig,
};

const DEFAULT_FIXTURE_URL: &str = "https://www.youtube.com/watch?v=aqz-KE-bpKQ";
const LIVE_TEST_OPT_IN: &str = "YOUTA_RUN_LIVE_YOUTUBE_TEST";
const LIVE_TEST_URL: &str = "YOUTA_LIVE_YOUTUBE_URL";
const LIVE_TEST_AUDIBLE: &str = "YOUTA_LIVE_YOUTUBE_AUDIBLE";
const RESUME_AT: Duration = Duration::from_secs(30);

/// Resumes and decodes real `YouTube` audio through the production backend.
///
/// The default fixture is the Blender Foundation's Creative Commons-licensed
/// *Big Buck Bunny* upload. Set `YOUTA_LIVE_YOUTUBE_URL` to exercise another
/// public video. CI uses mpv's null output, while a local opt-in can set
/// `YOUTA_LIVE_YOUTUBE_AUDIBLE=1` to use the machine's default audio output.
#[test]
#[ignore = "requires YouTube, yt-dlp, and mpv; CI invokes this target explicitly"]
fn youtube_audio_playback_advances_and_shuts_down() {
    assert_eq!(
        std::env::var(LIVE_TEST_OPT_IN).as_deref(),
        Ok("1"),
        "set {LIVE_TEST_OPT_IN}=1 when invoking this live test"
    );

    let temporary = tempfile::tempdir().expect("temporary live-test directory");
    let url = std::env::var(LIVE_TEST_URL).unwrap_or_else(|_| DEFAULT_FIXTURE_URL.to_owned());
    let audible = std::env::var(LIVE_TEST_AUDIBLE).as_deref() == Ok("1");
    let config = ProcessPlaybackConfig {
        mpv_executable: helper_path("YOUTA_TEST_MPV", "mpv"),
        yt_dlp_executable: helper_path("YOUTA_TEST_YT_DLP", "yt-dlp"),
        runtime_dir: temporary.path().join("runtime"),
        audio_output: if audible {
            AudioOutputDriver::Auto
        } else {
            AudioOutputDriver::Null
        },
        audio_device: None,
        profile: PlaybackProfile::Balanced,
        audiophile: AudiophilePlaybackOptions::default(),
    };

    let mut backend = MpvBackend::spawn(&config).expect("start Youta's mpv backend");
    let mut input = PlaybackInput::new(url);
    input.title = Some("Youta live YouTube smoke test".to_owned());
    input.start_at = RESUME_AT;
    // The application retries a pre-playback YouTube HTTP 403 with yt-dlp's
    // format verification enabled. Cloud CI egress commonly needs that path,
    // while this backend-level test does not instantiate the controller that
    // performs the retry.
    input.verify_remote_format = true;
    backend
        .play(&input)
        .expect("send the real YouTube URL to Youta's backend");

    let deadline = Instant::now() + Duration::from_secs(90);
    let mut playback_started = false;
    let mut observed_position = Duration::ZERO;
    let mut first_started_position = None;
    let mut last_error = None;
    while Instant::now() < deadline {
        loop {
            match backend.poll_event() {
                Ok(Some(PlaybackEvent::PlaybackStarted)) => playback_started = true,
                Ok(Some(PlaybackEvent::Ended(end))) => {
                    last_error = Some(format!("media ended before advancing: {end:?}"));
                    break;
                }
                Ok(Some(PlaybackEvent::ProcessExited { diagnostic })) => {
                    last_error = Some(format!(
                        "mpv exited before advancing: {}",
                        diagnostic.as_deref().unwrap_or("no diagnostics")
                    ));
                    break;
                }
                Ok(Some(PlaybackEvent::MediaLoaded)) => {}
                Ok(None) => break,
                Err(error) => {
                    last_error = Some(error.to_string());
                    break;
                }
            }
        }
        if last_error.is_some() {
            break;
        }
        match backend.status() {
            Ok(status) => {
                observed_position = observed_position.max(status.position);
                if playback_started && !status.position.is_zero() {
                    first_started_position.get_or_insert(status.position);
                }
                if playback_started && observed_position >= RESUME_AT + Duration::from_secs(2) {
                    break;
                }
            }
            Err(error) => {
                last_error = Some(error.to_string());
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    backend
        .shutdown()
        .expect("stop the live playback process cleanly");
    let first_started_position = first_started_position.unwrap_or_else(|| {
        panic!(
            "real playback never exposed a started position; backend error: {}",
            last_error.as_deref().unwrap_or("none")
        )
    });
    assert!(
        first_started_position >= RESUME_AT.saturating_sub(Duration::from_secs(1)),
        "real playback ignored the requested resume position; first started position: \
         {first_started_position:?}; requested: {RESUME_AT:?}"
    );
    assert!(
        playback_started && observed_position >= RESUME_AT + Duration::from_secs(2),
        "real resumed playback did not emit PlaybackStarted and advance within 90 seconds; last \
         position: {observed_position:?}; backend error: {}",
        last_error.as_deref().unwrap_or("none")
    );
}

fn helper_path(variable: &str, fallback: &str) -> PathBuf {
    std::env::var_os(variable).map_or_else(|| PathBuf::from(fallback), PathBuf::from)
}
