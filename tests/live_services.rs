//! Required live checks for remote metadata services.
//!
//! Each test is ignored by an ordinary local `cargo test` invocation and is
//! enabled explicitly by its CI job. This keeps accidental local runs offline
//! while ensuring every pushed commit exercises the production network code.

#[cfg(all(feature = "apple-podcasts", feature = "backend-mpv", feature = "rss"))]
use std::time::{Duration, Instant};

#[cfg(all(feature = "apple-podcasts", feature = "backend-mpv", feature = "rss"))]
use youta::playback::mpv::MpvBackend;
#[cfg(all(feature = "apple-podcasts", feature = "backend-mpv", feature = "rss"))]
use youta::playback::{
    AudioOutputDriver, AudiophilePlaybackOptions, PlaybackBackend, PlaybackInput, PlaybackProfile,
    ProcessPlaybackConfig,
};

#[cfg(all(feature = "apple-podcasts", feature = "backend-mpv", feature = "rss"))]
const APPLE_TEST_OPT_IN: &str = "YOUTA_RUN_LIVE_APPLE_PODCASTS_TEST";
#[cfg(all(feature = "apple-podcasts", feature = "backend-mpv", feature = "rss"))]
const APPLE_TEST_URL: &str = "YOUTA_LIVE_APPLE_PODCASTS_URL";
#[cfg(all(feature = "apple-podcasts", feature = "backend-mpv", feature = "rss"))]
const DEFAULT_APPLE_URL: &str =
    "https://podcasts.apple.com/us/podcast/global-news-podcast/id135067274";

/// Resolves a real Apple Podcasts show, parses its RSS feed, and decodes audio.
#[cfg(all(feature = "apple-podcasts", feature = "backend-mpv", feature = "rss"))]
#[test]
#[ignore = "requires Apple Podcasts, a public RSS feed, and mpv"]
fn apple_podcasts_lookup_rss_and_audio_are_usable() {
    use youta::providers::apple_podcasts::ApplePodcastsResolver;
    use youta::providers::rss::RssPodcastProvider;

    assert_eq!(
        std::env::var(APPLE_TEST_OPT_IN).as_deref(),
        Ok("1"),
        "set {APPLE_TEST_OPT_IN}=1 when invoking this live test"
    );

    let apple_url = std::env::var(APPLE_TEST_URL)
        .unwrap_or_else(|_| DEFAULT_APPLE_URL.to_owned())
        .parse()
        .expect("valid configured Apple Podcasts URL");
    let resolved = ApplePodcastsResolver::new()
        .resolve(&apple_url)
        .expect("resolve the real Apple Podcasts show");
    assert_eq!(resolved.link.collection_id, 135_067_274);
    assert!(!resolved.podcast.title.trim().is_empty());
    let feed_url = resolved
        .podcast
        .feed_url
        .expect("Apple returned a public RSS feed URL");

    let feed = RssPodcastProvider::new()
        .fetch(&feed_url)
        .expect("fetch and normalize the live podcast feed");
    assert!(!feed.episodes.is_empty(), "the live podcast feed is empty");
    let enclosure = feed
        .episodes
        .iter()
        .flat_map(|episode| &episode.enclosures)
        .map(|enclosure| &enclosure.url)
        .next()
        .expect("the live podcast feed contains an audio enclosure");

    let temporary = tempfile::tempdir().expect("temporary Apple playback directory");
    let config = ProcessPlaybackConfig {
        mpv_executable: std::env::var_os("YOUTA_TEST_MPV").map_or_else(|| "mpv".into(), Into::into),
        yt_dlp_executable: std::env::var_os("YOUTA_TEST_YT_DLP")
            .map_or_else(|| "yt-dlp".into(), Into::into),
        runtime_dir: temporary.path().join("runtime"),
        audio_output: AudioOutputDriver::Null,
        audio_device: None,
        profile: PlaybackProfile::Balanced,
        audiophile: AudiophilePlaybackOptions::default(),
    };
    let mut backend = MpvBackend::spawn(&config).expect("start Youta's mpv backend");
    backend
        .play(&PlaybackInput::new(enclosure.as_str()))
        .expect("play the RSS enclosure through Youta");
    let (active, position, backend_error) = wait_for_audio(&mut backend, Duration::from_secs(60));
    backend
        .shutdown()
        .expect("stop Apple Podcasts playback cleanly");
    assert!(
        active && position >= Duration::from_secs(2),
        "Apple Podcasts audio did not advance; last position: {position:?}; \
         backend error: {}",
        backend_error.as_deref().unwrap_or("none")
    );
}

/// Queries the public Wikidata endpoint for the live `YouTube` fixture.
#[cfg(feature = "wikidata")]
#[test]
#[ignore = "requires the public Wikidata Query Service"]
fn wikidata_finds_the_youtube_fixture_item() {
    use youta::providers::wikidata::{WikidataExternalKind, WikidataProvider};

    assert_eq!(
        std::env::var("YOUTA_RUN_LIVE_WIKIDATA_TEST").as_deref(),
        Ok("1"),
        "set YOUTA_RUN_LIVE_WIKIDATA_TEST=1 when invoking this live test"
    );
    let lookup = WikidataProvider::new()
        .lookup_external(WikidataExternalKind::YouTubeVideo, "aqz-KE-bpKQ")
        .expect("query Wikidata for the real YouTube fixture");
    assert_eq!(lookup.kind, WikidataExternalKind::YouTubeVideo);
    assert_eq!(lookup.external_id, "aqz-KE-bpKQ");
    assert!(
        lookup.items.iter().any(|item| item.item_id == "Q282456"),
        "Wikidata no longer links the fixture to Q282456: {:?}",
        lookup.items
    );
}

#[cfg(all(feature = "apple-podcasts", feature = "backend-mpv", feature = "rss"))]
fn wait_for_audio(
    backend: &mut impl PlaybackBackend,
    timeout: Duration,
) -> (bool, Duration, Option<String>) {
    let deadline = Instant::now() + timeout;
    let mut active = false;
    let mut position = Duration::ZERO;
    while Instant::now() < deadline {
        match backend.status() {
            Ok(status) => {
                active |= !status.idle;
                position = position.max(status.position);
                if active && position >= Duration::from_secs(2) {
                    return (active, position, None);
                }
            }
            Err(error) => return (active, position, Some(error.to_string())),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    (active, position, None)
}
