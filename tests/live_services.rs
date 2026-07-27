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

#[cfg(feature = "apple-podcasts")]
const APPLE_SEARCH_TEST_OPT_IN: &str = "YOUTA_RUN_LIVE_APPLE_PODCASTS_SEARCH_TEST";

#[cfg(feature = "network")]
const YOUTUBE_CHANNEL_TEST_OPT_IN: &str = "YOUTA_RUN_LIVE_YOUTUBE_CHANNEL_TEST";
#[cfg(feature = "network")]
const YOUTUBE_CHANNEL_TEST_ID: &str = "YOUTA_LIVE_YOUTUBE_CHANNEL_ID";
#[cfg(feature = "network")]
const DEFAULT_YOUTUBE_CHANNEL_ID: &str = "UC_x5XG1OV2P6uZZ5FSM9Ttw";

#[cfg(feature = "youtube-music")]
const YOUTUBE_MUSIC_TEST_OPT_IN: &str = "YOUTA_RUN_LIVE_YOUTUBE_MUSIC_TEST";
#[cfg(feature = "youtube-music")]
const YOUTUBE_MUSIC_TEST_QUERY: &str = "YOUTA_LIVE_YOUTUBE_MUSIC_QUERY";
#[cfg(feature = "youtube-music")]
const DEFAULT_YOUTUBE_MUSIC_QUERY: &str = "Massive Attack Teardrop";

#[cfg(all(feature = "backend-mpv", feature = "radio"))]
const RADIO_TEST_OPT_IN: &str = "YOUTA_RUN_LIVE_RADIO_TEST";

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

/// Searches Apple's public podcast catalog without credentials.
#[cfg(feature = "apple-podcasts")]
#[test]
#[ignore = "requires the public Apple Search API"]
fn apple_podcasts_public_search_returns_normalized_shows() {
    use youta::providers::apple_podcasts::{
        ApplePodcastsResolver, ApplePodcastsSearchClient, ApplePodcastsSearchRequest,
    };

    assert_eq!(
        std::env::var(APPLE_SEARCH_TEST_OPT_IN).as_deref(),
        Ok("1"),
        "set {APPLE_SEARCH_TEST_OPT_IN}=1 when invoking this live test"
    );

    let mut request = ApplePodcastsSearchRequest::new("Global News Podcast BBC", "us");
    request.limit = 10;
    let results = ApplePodcastsSearchClient::new()
        .search(&request)
        .expect("search Apple's public podcast catalog");

    assert_eq!(results.country, "us");
    assert!(!results.podcasts.is_empty());
    assert!(results.podcasts.len() <= 10);
    assert!(results.podcasts.iter().all(|podcast| {
        podcast.collection_id > 0
            && !podcast.title.trim().is_empty()
            && podcast
                .webpage_url
                .as_ref()
                .is_some_and(|url| url.host_str() == Some("podcasts.apple.com"))
    }));
    let first = results
        .podcasts
        .first()
        .expect("non-empty search has a first result");
    let listed = ApplePodcastsResolver::new()
        .resolve_collection(&results.country, first.collection_id)
        .expect("list the live podcast's documented lookup episode window");
    assert_eq!(listed.podcast.collection_id, first.collection_id);
    assert!(!listed.episodes.is_empty());
    assert!(listed.episodes.len() <= 200);
    assert!(
        listed
            .episodes
            .iter()
            .any(|episode| episode.media_url.is_some()),
        "Apple returned no playable episode URL"
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
        !entity.wikipedia_sitelinks.is_empty(),
        "the live fixture exposed no canonical Wikipedia sitelinks"
    );
    assert!(entity.wikipedia_sitelinks.iter().all(|sitelink| {
        sitelink.url.scheme() == "https"
            && sitelink
                .url
                .host_str()
                .is_some_and(|host| host.ends_with(".wikipedia.org"))
            && sitelink.url.path().starts_with("/wiki/")
    }));
    assert!(
        !entity.hard_bounds_reached,
        "the ordinary live fixture unexpectedly reached a display hard bound"
    );

    // Q13520818 is a non-political P8687 example with dated social-account
    // qualifiers. Assert structure rather than mutable follower totals.
    let follower_entity = provider
        .load_entity_statements("Q13520818")
        .expect("load the live follower-history fixture");
    let follower_values = follower_entity
        .statements
        .iter()
        .find(|statement| statement.property_id == "P8687")
        .map(|statement| statement.values.as_slice())
        .expect("the live fixture retains P8687 observations");
    assert!(!follower_values.is_empty());
    assert!(follower_values.iter().all(|value| {
        value.display.matches(" · ").count() >= 2
            && value.display.contains(" followers")
            && !value.display.contains('–')
    }));
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

/// Searches the public `YouTube Music` songs section without an API key.
#[cfg(feature = "youtube-music")]
#[test]
#[ignore = "requires the public YouTube Music service and yt-dlp"]
fn youtube_music_keyless_search_returns_playable_tracks_before_timeout() {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use youta::providers::youtube_music::{YouTubeMusicSearch, YouTubeMusicSearchConfig};

    assert_eq!(
        std::env::var(YOUTUBE_MUSIC_TEST_OPT_IN).as_deref(),
        Ok("1"),
        "set {YOUTUBE_MUSIC_TEST_OPT_IN}=1 when invoking this live test"
    );
    let executable = std::env::var_os("YOUTA_TEST_YT_DLP")
        .map_or_else(|| PathBuf::from("yt-dlp"), PathBuf::from);
    let query = std::env::var(YOUTUBE_MUSIC_TEST_QUERY)
        .unwrap_or_else(|_| DEFAULT_YOUTUBE_MUSIC_QUERY.to_owned());
    // Keep the provider's process deadline below the historical 20-second
    // failure so a successful test also proves that regression stays fixed.
    let process_timeout = Duration::from_secs(15);
    let search = YouTubeMusicSearch::new(YouTubeMusicSearchConfig {
        executable,
        timeout: process_timeout,
        ..YouTubeMusicSearchConfig::default()
    });

    let started = Instant::now();
    let tracks = search
        .search(&query, 30)
        .expect("complete the real keyless YouTube Music search");
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(20),
        "keyless YouTube Music search exceeded the historical 20-second bound: {elapsed:?}"
    );
    assert!(
        tracks.len() >= 5,
        "the public songs search returned too few playable tracks: {}",
        tracks.len()
    );
    assert!(tracks.len() <= 30);
    assert!(tracks.iter().all(|track| {
        track.video_id.len() == 11
            && !track.title.trim().is_empty()
            && track.webpage_url.scheme() == "https"
            && track.webpage_url.host_str() == Some("music.youtube.com")
            && track
                .webpage_url
                .query_pairs()
                .any(|(key, value)| key == "v" && value.as_ref() == track.video_id.as_str())
    }));
}

/// Decodes a public HTTPS radio stream and parses optional station metadata.
#[cfg(all(feature = "backend-mpv", feature = "radio"))]
#[test]
#[ignore = "requires public radio streams, 4duk metadata, and mpv"]
fn radio_stream_and_passive_metadata_are_usable() {
    use std::time::{Duration, Instant};

    use youta::playback::mpv::MpvBackend;
    use youta::playback::{
        AudioOutputDriver, AudiophilePlaybackOptions, PlaybackBackend, PlaybackInput,
        PlaybackProfile, ProcessPlaybackConfig,
    };
    use youta::providers::radio::{RadioNowPlayingClient, station_by_id};

    assert_eq!(
        std::env::var(RADIO_TEST_OPT_IN).as_deref(),
        Ok("1"),
        "set {RADIO_TEST_OPT_IN}=1 when invoking this live test"
    );

    let station = station_by_id("radio-swiss-classic").expect("HTTPS live-radio fixture");
    let temporary = tempfile::tempdir().expect("temporary Radio playback directory");
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
        .play(&PlaybackInput::new(station.stream))
        .expect("open the public HTTPS radio stream through Youta");
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut active = false;
    let mut position = Duration::ZERO;
    let mut stream_title = None;
    let mut backend_diagnostic = None;
    while Instant::now() < deadline {
        match backend.status() {
            Ok(status) => {
                active |= !status.idle;
                position = position.max(status.position);
                if status
                    .stream_title
                    .as_deref()
                    .is_some_and(|title| !title.trim().is_empty())
                {
                    stream_title = status.stream_title;
                }
            }
            Err(error) => {
                backend_diagnostic = Some(error.to_string());
                break;
            }
        }
        match backend.poll_event() {
            Ok(Some(event @ youta::playback::PlaybackEvent::Ended(_)))
            | Ok(Some(event @ youta::playback::PlaybackEvent::ProcessExited { .. })) => {
                backend_diagnostic = Some(format!("{event:?}"));
            }
            Ok(_) => {}
            Err(error) => backend_diagnostic = Some(error.to_string()),
        }
        if active && position >= Duration::from_secs(2) && stream_title.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    backend.shutdown().expect("stop Radio playback cleanly");
    assert!(
        active && position >= Duration::from_secs(2) && stream_title.is_some(),
        "public Radio audio or ICY metadata did not become active; last position: {position:?}; \
         stream title: {stream_title:?}; backend diagnostic: {}",
        backend_diagnostic.as_deref().unwrap_or("none")
    );

    let four_duk = station_by_id("4duk-radio").expect("4duk metadata fixture");
    let endpoint = four_duk
        .now_playing
        .expect("4duk retains its optional metadata endpoint");
    let metadata = RadioNowPlayingClient::with_options(Duration::from_secs(20), 16 * 1024)
        .expect("bounded Radio metadata client")
        .fetch(endpoint)
        .expect("fetch and parse 4duk's public now-playing JSON");
    assert!(
        metadata.title.is_some() || metadata.artist.is_some(),
        "4duk returned no current title or artist"
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
