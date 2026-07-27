//! Required live checks for remote metadata services.
//!
//! Each test is ignored by an ordinary local `cargo test` invocation and is
//! enabled explicitly by a CI job or an opt-in local command. This keeps
//! accidental local runs offline while preserving checks against changing
//! production services.

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

#[cfg(feature = "network")]
const YOUTUBE_CHANNEL_TEST_OPT_IN: &str = "YOUTA_RUN_LIVE_YOUTUBE_CHANNEL_TEST";
#[cfg(feature = "network")]
const YOUTUBE_CHANNEL_TEST_ID: &str = "YOUTA_LIVE_YOUTUBE_CHANNEL_ID";
#[cfg(feature = "network")]
const DEFAULT_YOUTUBE_CHANNEL_ID: &str = "UC_x5XG1OV2P6uZZ5FSM9Ttw";

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
    let provider = WikidataProvider::new();
    let lookup = provider
        .lookup_external(WikidataExternalKind::YouTubeVideo, "aqz-KE-bpKQ")
        .expect("query Wikidata for the real YouTube fixture");
    assert_eq!(lookup.kind, WikidataExternalKind::YouTubeVideo);
    assert_eq!(lookup.external_id, "aqz-KE-bpKQ");
    assert!(
        lookup.items.iter().any(|item| item.item_id == "Q282456"),
        "Wikidata no longer links the fixture to Q282456: {:?}",
        lookup.items
    );

    let channel_lookup = provider
        .lookup_external(
            WikidataExternalKind::YouTubeChannel,
            "UC6R1juCB5ArnJGMmUlEE_fg",
        )
        .expect("query Wikidata for the real YouTube channel fixture");
    assert_eq!(channel_lookup.kind, WikidataExternalKind::YouTubeChannel);
    assert_eq!(channel_lookup.external_id, "UC6R1juCB5ArnJGMmUlEE_fg");
    assert!(
        channel_lookup
            .items
            .iter()
            .any(|item| item.item_id == "Q61113"),
        "Wikidata no longer links the channel fixture to Q61113: {:?}",
        channel_lookup.items
    );

    // Keep the committed fixture non-political while allowing a developer to
    // reproduce another public item through the same live regression path.
    let statement_item =
        std::env::var("YOUTA_LIVE_WIKIDATA_ITEM").unwrap_or_else(|_| "Q282456".to_owned());
    let entity = provider
        .load_entity_statements(&statement_item)
        .expect("load labels, external-ID links, and Commons previews");
    assert!(
        !entity.statements.is_empty(),
        "Wikidata returned no statements for {statement_item}"
    );
    assert!(
        entity
            .statements
            .iter()
            .all(|statement| statement.property_label != statement.property_id),
        "at least one property label remained unresolved for {statement_item}: {:?}",
        entity.statements
    );
    let image = entity
        .statements
        .iter()
        .find(|statement| statement.property_id == "P18")
        .and_then(|statement| statement.values.first())
        .expect("the live fixture retains a P18 image");
    assert!(
        image.external_url.is_some(),
        "P18 must link to its Commons file page"
    );
    assert!(
        image.preview_url.is_some(),
        "P18 must expose a bounded raster preview"
    );
    assert!(
        entity
            .statements
            .iter()
            .filter(|statement| statement.property_id != "P18")
            .flat_map(|statement| &statement.values)
            .any(|value| value.external_url.is_some()),
        "the identifier-rich fixture exposed no formatter-backed external links"
    );
    assert!(
        !entity.hard_bounds_reached,
        "the ordinary live fixture unexpectedly reached a display hard bound"
    );
}

/// Parses current public About-page counts and profile links for one channel.
#[cfg(feature = "network")]
#[test]
#[ignore = "requires a public YouTube channel About page"]
fn youtube_channel_about_profile_is_usable() {
    use youta::providers::youtube_channel_page::YouTubeChannelPageClient;

    assert_eq!(
        std::env::var(YOUTUBE_CHANNEL_TEST_OPT_IN).as_deref(),
        Ok("1"),
        "set {YOUTUBE_CHANNEL_TEST_OPT_IN}=1 when invoking this live test"
    );
    let channel_id = std::env::var(YOUTUBE_CHANNEL_TEST_ID)
        .unwrap_or_else(|_| DEFAULT_YOUTUBE_CHANNEL_ID.to_owned());
    let profile = YouTubeChannelPageClient::new()
        .channel_metadata(&channel_id)
        .expect("load and parse the real public channel About page");

    assert_eq!(profile.channel_id, channel_id);
    assert!(profile.joined_at.is_some(), "joined date is missing");
    assert!(
        profile.video_count.is_some(),
        "public video count is missing"
    );
    assert!(
        profile.total_view_count.is_some(),
        "aggregate public view count is missing"
    );
    assert!(
        !profile.external_links.is_empty(),
        "the live fixture no longer advertises public profile links"
    );
    assert!(
        profile.external_links.iter().all(|link| {
            matches!(link.url.scheme(), "http" | "https")
                && link.url.host_str().is_some()
                && link.url.username().is_empty()
                && link.url.password().is_none()
        }),
        "all parsed profile links must remain credential-free HTTP(S) URLs"
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
