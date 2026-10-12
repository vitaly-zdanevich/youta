//! Opt-in audio decoding check for an available YouTube broadcast beginning.
//!
//! Select a public ongoing or recently ended broadcast explicitly:
//!
//! ```text
//! YOUTA_RUN_LIVE_START_TEST=1 YOUTA_LIVE_START_URL=https://www.youtube.com/watch?v=VIDEO_ID \
//! cargo test --locked --test live_youtube_from_start -- --ignored
//! ```
//!
//! No recording is retained. The production transport bounds memory, disables
//! extractor configuration, and removes its private fragment files on exit.

#![cfg(all(unix, feature = "backend-mpv", feature = "yt-dlp"))]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use url::Url;
use youta::playback::live_from_start::{LiveFromStartConfig, LiveFromStartStream};
use youta::playback::mpv::MpvBackend;
use youta::playback::threaded::ThreadedBackend;
use youta::playback::youtube_prewarm::YouTubePrewarmCancellation;
use youta::playback::{
    AudioOutputDriver, AudiophilePlaybackOptions, PlaybackBackend, PlaybackEvent, PlaybackInput,
    PlaybackProfile, ProcessPlaybackConfig,
};

/// Selects installed helpers without passing their arguments through a shell.
fn helper_path(variable: &str, fallback: &str) -> PathBuf {
    std::env::var_os(variable).map_or_else(|| fallback.into(), Into::into)
}

/// The strict transport starts near zero and advances through real mpv audio decoding.
#[test]
#[ignore = "requires an explicitly selected available broadcast, network, yt-dlp, and mpv"]
fn broadcast_beginning_decodes_and_transport_shuts_down() {
    assert_eq!(
        std::env::var("YOUTA_RUN_LIVE_START_TEST").as_deref(),
        Ok("1")
    );
    let source = Url::parse(
        &std::env::var("YOUTA_LIVE_START_URL").expect("select a public broadcast URL explicitly"),
    )
    .expect("valid broadcast URL");
    let temporary = tempfile::tempdir().expect("private mpv test directory");
    let yt_dlp = helper_path("YOUTA_TEST_YT_DLP", "yt-dlp");
    let mut transport = LiveFromStartStream::prepare(
        &LiveFromStartConfig {
            executable: yt_dlp.clone(),
            startup_timeout: Duration::from_secs(60),
            allow_plugins: false,
        },
        &source,
        &YouTubePrewarmCancellation::new(),
    )
    .expect("broadcast beginning is available through the strict transport");
    let config = ProcessPlaybackConfig {
        mpv_executable: helper_path("YOUTA_TEST_MPV", "mpv"),
        yt_dlp_executable: yt_dlp,
        runtime_dir: temporary.path().join("runtime"),
        audio_output: AudioOutputDriver::Null,
        audio_device: None,
        profile: PlaybackProfile::Balanced,
        audiophile: AudiophilePlaybackOptions::default(),
    };
    let mut backend = ThreadedBackend::new(MpvBackend::spawn(&config).expect("start real mpv"));
    let mut input = PlaybackInput::new(transport.url().as_str());
    input.bypass_ytdl = true;
    input.title = Some("Youta broadcast-start transport test".to_owned());
    backend.play(&input).expect("load the original audio bytes");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut started = false;
    let mut first_position = None;
    let mut advanced = false;
    while Instant::now() < deadline {
        assert!(
            transport.take_error().is_none(),
            "strict audio transport failed"
        );
        for _ in 0..32 {
            match backend.poll_event().expect("read real mpv events") {
                Some(PlaybackEvent::PlaybackStarted) => started = true,
                Some(PlaybackEvent::MediaLoaded) => {}
                Some(_) => panic!("audio ended before advancing"),
                None => break,
            }
        }
        let status = backend.status().expect("read decoded audio position");
        if started && !status.position.is_zero() {
            first_position.get_or_insert(status.position);
        }
        if started && status.position >= Duration::from_secs(2) {
            advanced = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    backend.shutdown().expect("stop and reap real mpv");
    let stopping = Instant::now();
    transport.stop();
    assert!(
        stopping.elapsed() < Duration::from_secs(3),
        "transport cleanup is bounded"
    );
    assert!(
        advanced,
        "real audio must advance from the broadcast beginning"
    );
    assert!(
        first_position.is_some_and(|position| position < Duration::from_secs(10)),
        "the stream must begin near zero, not at its live edge"
    );
}
